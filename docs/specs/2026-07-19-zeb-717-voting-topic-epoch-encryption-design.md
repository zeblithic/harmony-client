# ZEB-717 — Epoch-encrypt the voting Zenoh topic

**Status:** Design (approved decisions inline)
**Author:** Koya (fleet)
**Date:** 2026-07-19
**Ticket:** [ZEB-717](https://linear.app/zeblith/issue/ZEB-717)
**Related:** ZEB-249 community backward-secrecy (`docs/specs/2026-05-11-zeb-249-community-backward-secrecy-design.md`), ZEB-315 at-event-HLC membership resolution (PR #502), ZEB-718 voting backfill (follow-up)

## 1. Problem

`spawn_voting_log_zenoh_adapter` (`src-tauri/src/event_loop.rs:9352`) publishes the engine's
`SignedVotingEvent` CBOR **as plaintext** on `harmony/community/{id}/voting`, and subscribes the
same way. Any peer that can reach the community's Zenoh mesh can therefore deliver voting packets
forever; the only gate on an inbound event is signature + membership verification.

PR #502 (ZEB-315) moved inbound voting-event membership verification to the event's **own HLC**
(spec §10 rolling eligibility — the only convergent rule; at-HEAD resolution made replicas that
received an event before vs. after a kick diverge permanently). That change is correct and
convergence-required. Its flip side: a kicked member who backdates an event's HLC to before their
kick now passes verification **uniformly**, where the old at-HEAD resolution rejected them
order-dependently at kick-aware replicas (accidental friction, never a real control).

A backdated mint is byte-indistinguishable from a legitimately-delayed event, so a verify-layer
guard cannot close this without breaking the CRDT (clock-skew windows reject late-but-legitimate
events and the backfill direction; per-actor monotonic wall-time rejects legitimate multi-device
concurrency). **Containment is the transport's job** — deny a kicked-then-rotated member the ability
to produce ciphertext the mesh will accept.

## 2. The containment mechanism (derived, not assumed)

The ticket said to "mirror channel-log." Investigation showed that phrase hides the real mechanism,
and getting it exactly right is the whole ticket. Two facts frame it:

- **The voting topic is the sole delivery path.** `VotingLog` is not a field of `OwnerState`/`Space`,
  is not reconciled by `community_state_sync`, and is not persisted to disk. It is an in-memory
  registry (`lib.rs:1132`) fed only by the live Zenoh topic. Module doc `community_voting_log.rs:14`:
  *"No backfill."* A voting event dropped on the wire is permanently lost to any peer that missed it.
- **Channel-log does not rekey live.** `ChannelLogEngine` decrypts with a single spawn-time
  `channel_key` and has no old-key fallback (`community_channel_log_engine.rs:1613`); an
  `EpochRotation` apply does **not** re-derive it (`lib.rs:7374` fires only a pkarr re-register). Its
  key only changes at `start_node`/`redeem_invite`. So channel-log's "cut" is not actually live — it
  lands at restart.

**What actually separates a kicked member from a retained member.** Encryption under epoch N proves
only that the encryptor held `K(N)`. A member kicked at the N→N+1 rotation *had* `K(N)` — so
epoch-N ciphertext does **not** distinguish a kicked member from a still-member. The single thing that
does distinguish them: after the rotation, retained members receive `K(N+1)`; the kicked member does
not. Therefore the **only** transport-level test that cuts the kicked member is *"can you produce
**current-epoch** ciphertext?"* This forces the design:

> **Voting encrypts under the community's live current epoch key and, on receive, accepts only
> envelopes tagged with the current epoch** (`envelope.epoch == space.current_epoch`). Any other
> epoch — including a retained old key the receiver still holds for other purposes — is rejected.

This is **not** the state-root plane's `decrypt_for_topic`, which *falls back to* `old_epoch_keys`
(correct there — it must read historical encrypted state). Reusing that fallback would decrypt the
kicked member's epoch-N envelope and leave the hole exactly as it is today. Voting instead uses the
state-root plane's **encrypt / `EncryptedEnvelope` / live-`crdt_state`-read** machinery with a
**channel-log-style current-epoch-only receive cut** — a deliberate hybrid.

### 2.1 Inherent cost: cross-rotation legitimate votes (accepted)

Current-epoch-only is the *unique* transport cut (proof above), and it has one inherent cost: a
legitimate vote published under epoch N, arriving at a peer that has already applied the N→N+1
rotation, is dropped — and because voting has **no backfill** (Q1), it is not recovered. This is
narrow (sparse voting volume × rare admin-driven rotation × exact in-flight timing) and sits **within
voting's existing availability envelope**: today an offline peer already permanently misses voting
events. The security gain (closing the injection hole) is the ticket's explicit purpose. When the
deferred voting backfill / pull-on-rejoin (`community_voting_log.rs:14`) lands, cross-rotation drops
become recoverable under the new epoch; that is out of scope here and noted in §7.

## 3. Decisions

### D1. Cryptographic domain separation via AAD — **YES** (approved)

The voting plane and the state-root plane share the same community `current_epoch_key`, and
`encrypt_for_topic` binds **no AAD** today (`community_state_sync.rs:407`). Binding a voting-specific
AAD makes a cross-plane ciphertext fail the Poly1305 tag (`DecryptionFailed`) rather than merely
failing a downstream deserialize — cryptographic plane isolation, matching channel-log's static-AAD
discipline (`CHANNEL_PACKET_AAD = b"harmony-channel-msg-v1"`).

Mechanism (additive, byte-compatible for state-root):

```
// new, AAD-parameterized core in community_state_sync.rs:
pub fn encrypt_for_topic_with_aad(space, plaintext, aad: &[u8]) -> Result<EncryptedEnvelope, EpochError>
pub fn decrypt_for_topic_with_aad(space, envelope, aad: &[u8]) -> Result<Vec<u8>, EpochError>

// existing signatures preserved — delegate with empty AAD.
// ChaCha20Poly1305::encrypt(nonce, msg) is byte-identical to Payload{msg, aad: b""},
// so state-root wire bytes and fixtures do not move:
pub fn encrypt_for_topic(space, plaintext) = encrypt_for_topic_with_aad(space, plaintext, b"")
pub fn decrypt_for_topic(space, envelope)  = decrypt_for_topic_with_aad(space, envelope, b"")

// voting passes a versioned domain string:
const VOTING_TOPIC_AAD: &[u8] = b"harmony-voting-v1";
```

`decrypt_for_topic_with_aad` keeps the general current-then-old key selection (so state-root is
unchanged). **Voting does not use that fallback** — see D3.

### D2. Migration — **flag-day cutover** (approved)

The voting topic wire changes from raw `SignedVotingEvent` CBOR to `EncryptedEnvelope` CBOR. Voting
is live-only with no persisted/backfilled data. New code publishes and expects encrypted envelopes
only and rejects plaintext; there is no transition window in which a plaintext injection is accepted
(a staged accept-both window would re-open exactly the hole this ticket closes). Mixed-version nodes
drop each other's voting packets cleanly (old plaintext fails `EncryptedEnvelope` decode; new envelope
fails `SignedVotingEvent` decode) — voting partitions by version during a rollout, then heals once all
nodes upgrade. Acceptable at current scale (no persisted data, ~fleet-only users, routine restarts).
The `EncryptedEnvelope.ratchet_generation` reserved field is the additive seam for any future staged
change.

### D3. Receive policy — **current-epoch-only** (design conclusion, §2)

Derived above as the unique transport cut. On receive, reject `envelope.epoch != space.current_epoch`
before attempting decryption; decrypt the current-epoch envelope with `current_epoch_key` +
`VOTING_TOPIC_AAD`. Voting never consults `old_epoch_keys`.

## 4. Architecture

**Crypto lives in the Zenoh adapter (the wire boundary); the engine is unchanged.**
Placement chosen at implementation review (2026-07-19): the voting engine has no `crdt_state` today
(unlike the state-root plane's `handle_incoming_publish`, which already holds it), and 27 of 28
engine-construction sites are tests that bridge engine↔engine over plaintext mpsc. Encrypting in the
engine would force a heavyweight `Arc<Mutex<OwnerState>>` coupling onto the engine and make every
bridge test carry ciphertext (shared-epoch-key seeding across ~6 files). Encrypting at the adapter —
the layer that actually touches Zenoh, spawned from the event loop where `crdt_state` already lives —
gives a cleaner invariant (**everything on the Zenoh wire is ciphertext; everything in-process is
plaintext**), keeps voting-logic tests decoupled from crypto, and leaves all in-process/mpsc-bridged
tests untouched. Trade-off accepted: voting's adapter is no longer a pure byte relay (channel-log and
state-root adapters still are).

### 4.1 Adapter gains the live key source

`spawn_voting_log_zenoh_adapter` (`event_loop.rs:9352`) and its `VotingLogAdapterRequest`
(`event_loop.rs:170`) gain `community_id: SpaceId` + `crdt_state: Arc<Mutex<OwnerState>>`. Both are on
hand where the request is built (`ensure_voting_engine_for`, `lib.rs:47965`, which already clones
`crdt_state` into `OwnerDeviceCacheResolver`). The adapter reads the community `Space`
(`crdt_state.lock().await.spaces.get(&community_id)`) for the live epoch key at put/recv time. The
`VotingLogEngine` and its `Params` are **unchanged**.

### 4.2 Publish seam — adapter outbound (`event_loop.rs:9399-9410`)

The engine still sends plaintext `SignedVotingEvent` CBOR on `publisher_rx`. Before `session_pub.put`,
the adapter encrypts:

```
maybe = publisher_rx.recv() => {
    let Some(plaintext) = maybe else { break; };
    let wire = {                                   // brief crdt_state lock, crypto only
        let st = crdt_state.lock().await;
        match st.spaces.get(&community_id) {
            Some(space) => match encrypt_for_topic_with_aad(space, &plaintext, VOTING_TOPIC_AAD) {
                Ok(env) => { let mut w = Vec::new(); ciborium::into_writer(&env, &mut w).ok(); Some(w) }
                Err(e) => { tracing::warn!(%topic_pub, err=%e, "voting encrypt failed; dropping outbound"); None }
            },
            None => { tracing::warn!(%topic_pub, "no community space; dropping outbound voting packet"); None }
        }
    };
    if let Some(wire) = wire {
        if let Err(e) = session_pub.put(&key_pub, wire).await { /* existing !closing warn */ }
    }
}
```

Missing epoch state → the outbound packet is dropped (the engine already applied locally; a node
without the epoch key is not a broadcasting member — intended containment). No panic.

### 4.3 Receive seam — adapter inbound (`event_loop.rs:9489-9492`)

The bytes off Zenoh are now an `EncryptedEnvelope` CBOR. Before forwarding plaintext to the engine on
`subscriber_tx`, the adapter applies the current-epoch-only cut and decrypts:

```
let raw: Vec<u8> = sample.payload().to_bytes().to_vec();
let plaintext = {                                  // single lock: epoch gate + decrypt, no TOCTOU
    let envelope: EncryptedEnvelope = match ciborium::from_reader(raw.as_slice()) {
        Ok(env) => env,
        Err(e) => { tracing::warn!(%topic_sub, err=%e, "drop voting packet (envelope decode)"); continue; }
    };
    let st = crdt_state.lock().await;
    let Some(space) = st.spaces.get(&community_id) else { continue; };
    // D3 current-epoch-only cut — the containment gate:
    match space.current_epoch {
        Some(cur) if cur == envelope.epoch => {}
        _ => { tracing::warn!(%topic_sub, epoch=envelope.epoch, "drop voting packet (stale/unknown epoch)"); continue; }
    }
    match decrypt_for_topic_with_aad(space, &envelope, VOTING_TOPIC_AAD) {
        Ok(pt) => pt,
        Err(e) => { tracing::warn!(%topic_sub, err=%e, "drop voting packet (decrypt)"); continue; }
    }
};
if subscriber_tx.send(plaintext).await.is_err() { break; }
```

The engine's `process_inbound_dispatch` / `process_inbound` receive plaintext `SignedVotingEvent` CBOR
exactly as today — **zero engine changes**. The 64 KiB size cap already applied to the raw payload
still runs first.

### 4.4 Data flow summary

```
publish:  engine: SignedVotingEvent --cbor--> plaintext --> publisher_rx
          adapter: encrypt@current+AAD --> EncryptedEnvelope --cbor--> Zenoh put
receive:  adapter: Zenoh sample --cbor--> EncryptedEnvelope --[epoch==current?]--> decrypt@current+AAD --> plaintext
          engine:  subscriber_rx --> SignedVotingEvent --> verify@hlc --> apply
```

## 5. Error handling

All handled in the adapter; every failure is a drop the adapter loops already tolerate (no panic, no
change to engine error surfaces):

- **Publish, missing epoch state:** adapter drops the outbound packet (`warn`). The engine already
  applied locally; a node without the epoch key is not a broadcasting member (intended containment).
- **Receive, stale/unknown epoch:** `envelope.epoch != current_epoch` → `continue` (drop, `warn`).
  Containment path for a kicked-then-rotated member.
- **Receive, tag mismatch (tamper or cross-plane replay):** `DecryptionFailed(epoch)` → drop.
- **Receive, malformed envelope CBOR:** decode error → drop, same posture as today's malformed packet.

## 6. Testing

**Unit (mirror `community_channel_log.rs:3478` AEAD tests):**
- voting round-trip (`encrypt_for_topic_with_aad` → `decrypt_for_topic_with_aad` under matching AAD +
  matching epoch).
- wrong-AAD rejects (state-root empty-AAD envelope fails under `VOTING_TOPIC_AAD`, and vice-versa) —
  pins D1.
- stale-epoch rejects: an envelope tagged with a prior epoch is refused by the D3 gate even though the
  receiver still holds that old key in `old_epoch_keys` — pins the containment property directly.
- tampered-ciphertext rejects (`DecryptionFailed`).
- state-root byte-compat: `encrypt_for_topic` (2-arg) output unchanged vs. pre-refactor (empty-AAD
  equivalence).

**Integration — the acceptance criterion.** Extend `voting_event_flows_through_two_zenoh_sessions`
(`community_voting_zenoh_integration.rs:75`), which stands up two real `zenoh::Session`s with
`spawn_voting_log_zenoh_adapter` on each:
- pre-rotation: an event published by a still-member flows and applies on B (regression: encryption is
  transparent to the happy path).
- post-kick + rotation: B's `Space` reflects the rotated epoch (`current_epoch = N+1`, old key archived
  into `old_epoch_keys[N]`); an event encrypted under the kicked identity's stale epoch-N key is
  **dropped** (not applied) on B — even though B still holds `K(N)`. This is the exact acceptance test
  and the reason current-epoch-only (not old-key fallback) is required.

**Wire fixtures.** Refresh voting fixtures for the new envelope wire. Encryption is nondeterministic
(random nonce); pin via a deterministic-nonce test-fixtures variant if byte-pinning is needed (mirror
`encrypt_channel_packet_with_nonce`, gated `#[cfg(any(test, feature = "test-fixtures"))]`), otherwise
assert round-trip. Existing plaintext `SignedVotingEvent` fixtures remain valid as *inner* plaintext
pins.

## 7. Known limitation & out of scope

- **Cross-rotation vote drop (inherent, §2.1):** a legitimate epoch-N vote in flight across an N→N+1
  rotation is dropped at already-rotated peers with no recovery, because voting has no backfill. This
  is inherent to any transport cut that defeats the kicked member (proof in §2) and is within voting's
  existing no-backfill availability posture. Recovery is unlocked by the deferred voting backfill /
  pull-on-rejoin (`community_voting_log.rs:14`), tracked in [ZEB-718](https://linear.app/zeblith/issue/ZEB-718).
- **Device-identity binding** of `event.hlc.device_id` to enrolled devices — owned by the ZEB-668
  device-management family (ticket §scope item 4).
- **Channel-log data-plane rotation survival** (a separate, pre-existing limitation; channel-log
  rekeys only at restart).
- **State-root plane wire/callers** — guaranteed unchanged by the empty-AAD delegation.

## 8. Files touched

- `src-tauri/src/community_state_sync.rs` — add `encrypt_for_topic_with_aad` / `decrypt_for_topic_with_aad` (existing 2-arg helpers delegate with `b""`); `pub const VOTING_TOPIC_AAD = b"harmony-voting-v1"`. **[done — Task 1]**
- `src-tauri/src/event_loop.rs` — `VotingLogAdapterRequest` gains `community_id: SpaceId` + `crdt_state`; `spawn_voting_log_zenoh_adapter` gains the same params and does encrypt-on-put + current-epoch-only-decrypt-on-recv.
- `src-tauri/src/lib.rs` — populate the two new `VotingLogAdapterRequest` fields at `ensure_voting_engine_for` (`crdt_state` already in scope).
- `src-tauri/tests/community_voting/community_voting_zenoh_integration.rs` — seed epoch state on both nodes; post-kick+rotation rejection test.
- `src-tauri/tests/community_voting/community_voting_tier2_delegate_on_behalf_integration.rs` — seed epoch state (uses the real adapter).
- `src-tauri/tests/wire_format/…` voting fixtures — envelope wire refresh (if any pin topic bytes).
- `src-tauri/src/community_voting_log_engine.rs` — **no change** (engine stays wire-format-agnostic; all mpsc-bridged voting tests untouched).
