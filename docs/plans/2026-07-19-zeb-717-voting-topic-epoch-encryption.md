# ZEB-717 Voting Topic Epoch-Encryption — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Epoch-encrypt the voting Zenoh topic so a kicked-then-rotated member can no longer inject voting events.

**Architecture:** Reuse the community state-root plane's `EncryptedEnvelope` + live-`crdt_state` epoch-key machinery for *encrypt*, but apply a channel-log-style **current-epoch-only** receive cut (reject `envelope.epoch != current_epoch`) — the provably-unique transport test that separates a kicked member (held `K(N)`) from a retained member (gets `K(N+1)`). Crypto lives in `community_voting_log_engine`; the Zenoh adapter stays an unmodified byte relay.

**Tech Stack:** Rust, ChaCha20-Poly1305 (`chacha20poly1305` crate), `ciborium` CBOR, tokio, zenoh.

**Spec:** `docs/specs/2026-07-19-zeb-717-voting-topic-epoch-encryption-design.md`

> **Implementation note (placement changed at review):** crypto was moved from the *engine* to the
> *Zenoh adapter* (the wire boundary) after implementation revealed the voting engine has no
> `crdt_state` and 27/28 engine-construction sites are plaintext-mpsc bridge tests. See spec §4 (the
> authoritative architecture). Tasks 2–3 below were superseded by a single **adapter cutover** task
> (`spawn_voting_log_zenoh_adapter` + `VotingLogAdapterRequest` gain `community_id`+`crdt_state`;
> encrypt-on-put, current-epoch-only-decrypt-on-recv). Task 1 (AAD helpers), Task 4 (acceptance test),
> and Task 5 (sweep) landed as written. The engine is unchanged, so the mpsc-bridged voting tests were
> untouched.

## Global Constraints

- Cargo commands run from `src-tauri/`. Always `--locked` and `--features test-fixtures`.
- Iterative gates use `scripts/test-select --context task` (k=4) to avoid the ~50min full-integration relink on every task; the **final** task runs the full CI-parity sweep (`--workspace --all-targets`).
- `VOTING_TOPIC_AAD = b"harmony-voting-v1"` (exact bytes; versioned).
- Flag-day migration: publish AND receive change together; no plaintext-accept path. Wire changes from raw `SignedVotingEvent` CBOR to `EncryptedEnvelope` CBOR.
- State-root plane wire bytes MUST NOT move: the 2-arg `encrypt_for_topic`/`decrypt_for_topic` keep byte-identical output (empty-AAD delegation).
- No new panics; all crypto failures are `Result` drops the receive loop already tolerates.
- `spawn_voting_log_zenoh_adapter` (`event_loop.rs`) is NOT modified.

---

### Task 1: AAD-parameterized epoch crypto helpers

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` (helpers at `:386-446`; add `Payload` import near the `ChaCha20Poly1305` import)
- Test: `src-tauri/src/community_state_sync.rs` (inline `#[cfg(test)]` module — mirror existing epoch tests around `:7813-7896`)

**Interfaces:**
- Produces:
  - `pub fn encrypt_for_topic_with_aad(space: &Space, plaintext: &[u8], aad: &[u8]) -> Result<EncryptedEnvelope, EpochError>`
  - `pub fn decrypt_for_topic_with_aad(space: &Space, envelope: &EncryptedEnvelope, aad: &[u8]) -> Result<Vec<u8>, EpochError>`
  - `encrypt_for_topic` / `decrypt_for_topic` retain their 2-arg signatures, now delegating with `b""`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)]` module. Use the existing test helper that builds a Community `Space` with populated `current_epoch`/`current_epoch_key`/`old_epoch_keys` (search the module for how `community_publish_epoch_key_tracks_rotation` constructs its Space; reuse that constructor).

```rust
#[test]
fn aad_round_trip_matches() {
    let space = test_community_space_with_epoch(); // reuse existing helper
    let pt = b"voting-plaintext";
    let env = encrypt_for_topic_with_aad(&space, pt, b"harmony-voting-v1").unwrap();
    let out = decrypt_for_topic_with_aad(&space, &env, b"harmony-voting-v1").unwrap();
    assert_eq!(out, pt);
}

#[test]
fn aad_mismatch_rejects() {
    let space = test_community_space_with_epoch();
    let env = encrypt_for_topic_with_aad(&space, b"x", b"harmony-voting-v1").unwrap();
    // wrong AAD (empty, i.e. the state-root domain) must fail the tag:
    let err = decrypt_for_topic_with_aad(&space, &env, b"").unwrap_err();
    assert!(matches!(err, EpochError::DecryptionFailed(_)));
    // and the 2-arg state-root decrypt (empty AAD) must also fail on a voting envelope:
    assert!(decrypt_for_topic(&space, &env).is_err());
}

#[test]
fn empty_aad_is_byte_compatible_state_root() {
    // The 2-arg path must be indistinguishable from with_aad(.., b"").
    let space = test_community_space_with_epoch();
    let env = encrypt_for_topic(&space, b"state-root").unwrap();
    // decrypts under empty AAD via both entry points:
    assert_eq!(decrypt_for_topic(&space, &env).unwrap(), b"state-root");
    assert_eq!(decrypt_for_topic_with_aad(&space, &env, b"").unwrap(), b"state-root");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(aad_round_trip_matches) + test(aad_mismatch_rejects) + test(empty_aad_is_byte_compatible_state_root)'`
Expected: FAIL — `encrypt_for_topic_with_aad` not found.

- [ ] **Step 3: Add the `Payload` import**

At the top of `community_state_sync.rs`, alongside the existing `use chacha20poly1305::{...}`:

```rust
use chacha20poly1305::aead::Payload;
```
(If `ChaCha20Poly1305`, `Nonce`, `Aead`, `KeyInit` are imported from `chacha20poly1305`, add `aead::Payload` to that group.)

- [ ] **Step 4: Refactor the four helpers**

Replace the bodies at `:395-446` with:

```rust
pub fn encrypt_for_topic(space: &Space, plaintext: &[u8]) -> Result<EncryptedEnvelope, EpochError> {
    encrypt_for_topic_with_aad(space, plaintext, b"")
}

/// AAD-parameterized variant. Voting binds `VOTING_TOPIC_AAD` for cryptographic
/// domain separation from the state-root plane (which passes `b""`). Empty AAD is
/// byte-identical to the previous no-AAD call, so state-root wire bytes are unchanged.
pub fn encrypt_for_topic_with_aad(
    space: &Space,
    plaintext: &[u8],
    aad: &[u8],
) -> Result<EncryptedEnvelope, EpochError> {
    let epoch = space.current_epoch.ok_or(EpochError::MissingEpochState)?;
    let key = space
        .current_epoch_key
        .as_ref()
        .ok_or(EpochError::MissingEpochState)?;
    let cipher = ChaCha20Poly1305::new(key.as_chacha_key());

    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);

    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), Payload { msg: plaintext, aad })
        .map_err(|_| EpochError::EncryptionFailed(epoch))?;

    Ok(EncryptedEnvelope {
        epoch,
        nonce: nonce_bytes,
        ciphertext,
        ratchet_generation: None,
    })
}

pub fn decrypt_for_topic(space: &Space, envelope: &EncryptedEnvelope) -> Result<Vec<u8>, EpochError> {
    decrypt_for_topic_with_aad(space, envelope, b"")
}

pub fn decrypt_for_topic_with_aad(
    space: &Space,
    envelope: &EncryptedEnvelope,
    aad: &[u8],
) -> Result<Vec<u8>, EpochError> {
    let current_epoch = space
        .current_epoch
        .ok_or(EpochError::KeyNotAvailable(envelope.epoch))?;
    let key = if envelope.epoch == current_epoch {
        space.current_epoch_key.as_ref()
    } else {
        space.old_epoch_keys.get(&envelope.epoch)
    }
    .ok_or(EpochError::KeyNotAvailable(envelope.epoch))?;

    let cipher = ChaCha20Poly1305::new(key.as_chacha_key());
    cipher
        .decrypt(
            Nonce::from_slice(&envelope.nonce),
            Payload { msg: envelope.ciphertext.as_slice(), aad },
        )
        .map_err(|_| EpochError::DecryptionFailed(envelope.epoch))
}
```

- [ ] **Step 5: Run the new tests + the existing epoch suite**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_state_sync) + test(aad_)'`
Expected: PASS (new AAD tests + all pre-existing `community_publish_epoch_*` / `decrypt_for_topic` tests still green — proves state-root byte-compat).

- [ ] **Step 6: fmt + clippy (scoped) + commit**

Run: `cd src-tauri && cargo fmt --all && cargo clippy --locked --lib --features test-fixtures --no-deps -- -D warnings`
Expected: clean.

```bash
git add src-tauri/src/community_state_sync.rs
git commit -m "ZEB-717: AAD-parameterized encrypt/decrypt_for_topic helpers (state-root byte-compatible)"
```

---

### Task 2: Thread `crdt_state` into the voting engine

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (`VotingLogEngine<R>` struct `:246-308`; `VotingLogEngineParams` `:198-234`; `start`/constructor)
- Modify: `src-tauri/src/lib.rs` (`ensure_voting_engine_for` `:47847-47943` — pass `crdt_state.clone()`)
- Modify: every other `VotingLogEngineParams { .. }` construction site (grep — includes test harnesses)

**Interfaces:**
- Produces: `VotingLogEngine` and `VotingLogEngineParams` each carry `crdt_state: Arc<Mutex<OwnerState>>` (same type already used by `OwnerDeviceCacheResolver` at `lib.rs:47913`). No behavior change yet.

- [ ] **Step 1: Add the field to `VotingLogEngineParams` and `VotingLogEngine`**

In `VotingLogEngineParams`:
```rust
/// Live community/owner CRDT state — read for the current epoch key when
/// encrypting/decrypting voting packets (ZEB-717). Same handle the
/// identity resolver already borrows.
pub crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_types::OwnerState>>,
```
Add the matching `crdt_state` field to `VotingLogEngine<R>` and set it from `params.crdt_state` in `start`/the constructor (follow how `publisher_tx` is carried from params to struct).

- [ ] **Step 2: Set it at the production construction site**

In `ensure_voting_engine_for` (`lib.rs`), in the `VotingLogEngineParams { .. }` literal, add:
```rust
crdt_state: crdt_state.clone(),
```
(`crdt_state` is already the parameter name in scope — it is cloned into `OwnerDeviceCacheResolver::new` just above.)

- [ ] **Step 3: Fix all remaining construction sites**

Run: `cd src-tauri && rg -n 'VotingLogEngineParams\s*\{' --type rust`
For each hit (test harnesses, other engines), add `crdt_state: <handle>` — tests can build an empty state via the existing test constructor (search a neighboring voting test for how it makes an `OwnerState`/`Arc<Mutex<OwnerState>>`; reuse it).

- [ ] **Step 4: Compile-check (lib + tests, no run)**

Run: `cd src-tauri && cargo nextest list --locked --features test-fixtures -E 'test(community_voting)' >/dev/null`
Expected: compiles (all construction sites satisfied). Fix any missing-field errors it reports.

- [ ] **Step 5: Run the voting engine unit tests (unchanged behavior)**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_voting_log_engine)'`
Expected: PASS (plumbing only).

- [ ] **Step 6: fmt + commit**

Run: `cd src-tauri && cargo fmt --all`
```bash
git add src-tauri/src/community_voting_log_engine.rs src-tauri/src/lib.rs
git commit -m "ZEB-717: thread crdt_state into VotingLogEngine (no behavior change)"
```

---

### Task 3: Flag-day cutover — encrypt on publish, current-epoch-only decrypt on receive

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (add `VOTING_TOPIC_AAD` const; publish seam `:1543-1694`; `process_inbound_dispatch` `:2506`)
- Test: `src-tauri/src/community_voting_log_engine.rs` inline tests

**Interfaces:**
- Consumes: `encrypt_for_topic_with_aad` / `decrypt_for_topic_with_aad` (Task 1); `self.crdt_state` (Task 2).
- Produces: the voting wire packet is now `ciborium(EncryptedEnvelope)`; receive rejects `envelope.epoch != current_epoch`.

- [ ] **Step 1: Write the failing unit tests**

```rust
const AAD: &[u8] = b"harmony-voting-v1";

#[tokio::test]
async fn publish_emits_current_epoch_envelope() {
    // Build an engine whose crdt_state has a Community Space at current_epoch = E.
    // Publish one event; capture the bytes the engine hands to publisher_tx.
    let (engine, mut publisher_rx, space_snapshot) = test_engine_with_epoch().await;
    engine.publish_event(sample_event()).await.unwrap();
    let wire = publisher_rx.recv().await.unwrap();
    let env: crate::community_state_sync::EncryptedEnvelope =
        ciborium::from_reader(wire.as_slice()).unwrap();
    assert_eq!(env.epoch, space_snapshot.current_epoch.unwrap());
    // decrypts under voting AAD back to the original event:
    let pt = crate::community_state_sync::decrypt_for_topic_with_aad(&space_snapshot, &env, AAD).unwrap();
    let ev: SignedVotingEvent = ciborium::from_reader(pt.as_slice()).unwrap();
    assert_eq!(ev, sample_event());
}

#[tokio::test]
async fn receive_rejects_stale_epoch_even_with_retained_key() {
    // Space at current_epoch = E+1, with old_epoch_keys[E] STILL PRESENT.
    // An envelope encrypted under epoch E must be dropped (not applied),
    // proving current-epoch-only, not old-key fallback.
    let (engine, space_prev_epoch) = test_engine_rotated_retaining_old().await;
    let stale_wire = encrypt_event_under(&space_prev_epoch, &sample_event(), AAD); // epoch E
    engine.process_inbound_dispatch_for_test(&stale_wire).await.unwrap(); // Ok, but dropped
    assert!(engine.voting_log_is_empty().await, "stale-epoch event must not apply");
}
```
(Add tiny `#[cfg(test)]` accessors — `process_inbound_dispatch_for_test`, `voting_log_is_empty` — if not already present; follow existing test-accessor patterns in the file.)

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(publish_emits_current_epoch_envelope) + test(receive_rejects_stale_epoch_even_with_retained_key)'`
Expected: FAIL (publish still emits plaintext; receive still decodes plaintext).

- [ ] **Step 3: Add the AAD const**

Near the top of `community_voting_log_engine.rs`:
```rust
/// Domain-separation AAD for the voting topic (ZEB-717 D1). Distinct from the
/// state-root plane (which uses empty AAD) so a cross-plane ciphertext fails the
/// AEAD tag rather than merely a downstream deserialize.
const VOTING_TOPIC_AAD: &[u8] = b"harmony-voting-v1";
```

- [ ] **Step 4: Encrypt on publish**

At the publish seam (`:1543` encode, `:1691` send), after the existing `ciborium::into_writer(&event, &mut packet)` that produces the plaintext CBOR, wrap before send:
```rust
// packet = plaintext SignedVotingEvent CBOR (existing)
let envelope = {
    let st = self.crdt_state.lock().await;
    let space = st
        .spaces
        .get(&self.community_id)
        .ok_or_else(|| format!("voting publish: no community space {:?}", self.community_id))?;
    crate::community_state_sync::encrypt_for_topic_with_aad(space, &packet, VOTING_TOPIC_AAD)
        .map_err(|e| format!("voting encrypt: {e}"))?
};
let mut wire = Vec::new();
ciborium::into_writer(&envelope, &mut wire).map_err(|e| format!("envelope encode: {e}"))?;
self.publisher_tx.send(wire).await …   // was: send(packet)
```
Keep the lock scope minimal (crypto only); release before any `voting_log` lock. (Confirm exact field name for the spaces map by reading `OwnerState`; adjust `st.spaces.get(...)` accordingly.)

- [ ] **Step 5: Current-epoch-only decrypt on receive**

At the top of `process_inbound_dispatch` (`:2506`), before the existing `:2515` peek, transform the raw `packet` into plaintext and thread the plaintext through the rest of the function:
```rust
let envelope: crate::community_state_sync::EncryptedEnvelope =
    match ciborium::from_reader(packet) {
        Ok(env) => env,
        Err(e) => {
            tracing::warn!(community_id = ?self.community_id, err = %e, "drop voting packet (envelope decode)");
            return Ok(());
        }
    };
let plaintext: Vec<u8> = {
    let st = self.crdt_state.lock().await;
    let space = match st.spaces.get(&self.community_id) {
        Some(s) => s,
        None => return Ok(()),
    };
    // D3: current-epoch-only cut — the kicked-then-rotated containment gate.
    match space.current_epoch {
        Some(cur) if cur == envelope.epoch => {}
        _ => {
            tracing::warn!(community_id = ?self.community_id, epoch = envelope.epoch, "drop voting packet (stale/unknown epoch)");
            return Ok(());
        }
    }
    match crate::community_state_sync::decrypt_for_topic_with_aad(space, &envelope, VOTING_TOPIC_AAD) {
        Ok(pt) => pt,
        Err(e) => {
            tracing::warn!(community_id = ?self.community_id, err = %e, "drop voting packet (decrypt)");
            return Ok(());
        }
    }
};
// From here, use `&plaintext` where the function previously used `packet`:
//  - the :2515 lifecycle peek: ciborium::from_reader::<SignedVotingEvent, _>(plaintext.as_slice())
//  - the Self::process_inbound(.., &plaintext) call
```
Update BOTH downstream consumers (peek + `process_inbound`) to read `&plaintext`. `process_inbound`'s `&[u8]` signature is unchanged.

- [ ] **Step 6: Update the existing happy-path integration test expectation (still green)**

The existing `voting_event_flows_through_two_zenoh_sessions` (`tests/community_voting/community_voting_zenoh_integration.rs:75`) uses `spawn_voting_log_zenoh_adapter` end-to-end; with both sides now speaking envelope it should still pass **provided both engines' `crdt_state` carry a Space at the same `current_epoch`**. If the test builds engines without epoch state, extend its `FixedResolvers`/setup to seed a Community Space with `current_epoch = 0` + a `current_epoch_key` on both nodes. (This is the minimal setup the encryption now requires.)

- [ ] **Step 7: Run unit + the happy-path integration test**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_voting_log_engine) + test(voting_event_flows_through_two_zenoh_sessions)'`
Expected: PASS.

- [ ] **Step 8: fmt + clippy + commit**

Run: `cd src-tauri && cargo fmt --all && cargo clippy --locked --lib --features test-fixtures --no-deps -- -D warnings`
```bash
git add src-tauri/src/community_voting_log_engine.rs src-tauri/tests/community_voting/community_voting_zenoh_integration.rs
git commit -m "ZEB-717: epoch-encrypt voting topic (encrypt@current on publish, current-epoch-only decrypt on receive)"
```

---

### Task 4: Acceptance integration test — post-kick+rotation injection rejected

**Files:**
- Modify: `src-tauri/tests/community_voting/community_voting_zenoh_integration.rs`

**Interfaces:**
- Consumes: the two-session harness already in the file; `FixedResolvers`.

- [ ] **Step 1: Write the acceptance test**

```rust
/// ZEB-717: a member kicked at the N->N+1 rotation, holding only the stale
/// epoch-N key, cannot inject a voting event — even though the receiver still
/// retains K(N) in old_epoch_keys. This is the containment criterion.
#[tokio::test(flavor = "multi_thread")]
async fn kicked_then_rotated_member_injection_is_dropped() {
    // Node B: Community Space rotated to current_epoch = 1, old_epoch_keys[0] retained.
    // "Kicked member" session: encrypts a (backdated-HLC) event under epoch 0.
    // Publish it on harmony/community/{id}/voting via a raw zenoh put.
    // Assert: B's voting_log does NOT contain the event after a bounded settle.
    //
    // Control: an event encrypted under epoch 1 (current) by a still-member DOES apply,
    // proving the drop is epoch-specific, not a broken pipe.
    // (Build on the existing two-session setup; reuse its poll-until helpers.)
}
```
Fill the body using the file's existing session/adapter/poll patterns. Seed B's Space with `current_epoch = 1` and both `current_epoch_key`(epoch 1) and `old_epoch_keys[0]`. Encrypt the injection with `encrypt_for_topic_with_aad` against a Space snapshot at `current_epoch = 0`.

- [ ] **Step 2: Run it (targeted)**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(kicked_then_rotated_member_injection_is_dropped)'`
Expected: PASS (stale-epoch injection dropped; current-epoch control applies).

- [ ] **Step 3: Commit**

```bash
git add src-tauri/tests/community_voting/community_voting_zenoh_integration.rs
git commit -m "ZEB-717: acceptance test — kicked+rotated member voting injection dropped"
```

---

### Task 5: Wire fixtures + full CI-parity sweep

**Files:**
- Modify: `src-tauri/tests/wire_format/…` voting fixtures (`zeb290_fixtures.rs` / `zeb291_fixtures.rs` and companions)

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: Assess fixture impact**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(wire_format) + test(voting)'`
The existing plaintext `SignedVotingEvent`/envelope fixtures remain valid as *inner-plaintext* pins (the encoding of the event itself is unchanged). If any fixture pins the **published topic bytes** (full packet), it now needs the encrypted-envelope form.

- [ ] **Step 2: Add an envelope wire pin (deterministic nonce) if topic-byte pinning is required**

Only if a fixture pins topic bytes: add a `#[cfg(any(test, feature = "test-fixtures"))]` deterministic-nonce voting encrypt variant (mirror `encrypt_channel_packet_with_nonce` in `community_channel_log.rs:707`) and pin the envelope hex. Otherwise assert round-trip only. Do NOT expose a deterministic-nonce helper to production paths.

- [ ] **Step 3: Full CI-parity sweep**

Run: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
Then: `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` and `cargo fmt --all -- --check`
Then (frontend, from repo root): `npx tsc --noEmit && npx vitest run` (expected unaffected — no frontend change).
Expected: all green.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "ZEB-717: refresh voting wire fixtures for encrypted envelope; full gate green"
```

---

## Self-Review

**Spec coverage:**
- §3 D1 (voting AAD) → Task 1 + `VOTING_TOPIC_AAD` in Task 3. ✓
- §3 D2 (flag-day) → Task 3 (publish+receive cut over together; no plaintext-accept). ✓
- §3 D3 / §2 (current-epoch-only) → Task 3 Step 5 epoch gate + Task 4 acceptance test. ✓
- §4.1 (crdt_state on engine) → Task 2. ✓
- §4.2 publish seam → Task 3 Step 4. ✓ §4.3 receive seam → Task 3 Step 5. ✓
- §5 error handling (drops, no panic) → Task 3 Steps 4-5 (all `Result`/warn+return). ✓
- §6 tests → Task 1 (AAD unit), Task 3 (publish/receive unit + happy-path integration), Task 4 (acceptance), Task 5 (fixtures). ✓
- §6 state-root byte-compat → Task 1 `empty_aad_is_byte_compatible_state_root`. ✓
- Adapter unchanged (§4) → no task touches `event_loop.rs`. ✓

**Placeholder scan:** test bodies in Tasks 3/4 are sketched with exact assertions but rely on file-local harness helpers (`test_engine_with_epoch`, `FixedResolvers` seeding) that must be read from the file at execution — flagged inline as "reuse existing pattern," not left as blind TODOs. Acceptable for inline execution with full context.

**Type consistency:** `encrypt_for_topic_with_aad`/`decrypt_for_topic_with_aad` signatures identical across Tasks 1/3; `crdt_state: Arc<Mutex<OwnerState>>` identical across Tasks 2/3; `VOTING_TOPIC_AAD = b"harmony-voting-v1"` identical in const (Task 3) and tests (Tasks 3/4). ✓
