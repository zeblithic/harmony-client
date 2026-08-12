# ZEB-920: Channel-Log Live Epoch Keys — Design

**Ticket:** [ZEB-920](https://linear.app/zeblith/issue/ZEB-920) — deferred from ZEB-919 (spec: `2026-08-12-zeb919-epoch-pin-audit-design.md`, §5).
**Date:** 2026-08-12 · **Branch:** `zeblith/zeb-920-channel-log-keys-derive-from-the-spawn-pinned-membership-key`

## 1. Problem

Channel-log engines derive their `ChannelKey` once, at spawn, from the community
engine's `membership_key()` — which is itself bound at `spawn_engine` and never
changes for the engine's lifetime (`community_state_sync.rs:1654-1663`). Epoch
rotation only updates `Space.current_epoch_key` in owner-state. Consequences
(verified in the ZEB-919 audit; wire-only — at-rest segments store plaintext
`SignedChannelEvent` CBOR, so history is never stranded):

1. **Backward-secrecy window.** Post-rotation, a revoked member holding the old
   membership key keeps deriving valid channel keys and can decrypt all channel
   traffic until every member restarts. (Their own posts are already rejected by
   the membership gate in `verify_channel_event` — this is read-only exposure,
   but the read is exactly what rotation exists to cut.)
2. **Restart split.** A restarted member re-pins to the NEW key, derives
   different channel keys, and silently drops un-restarted members' packets
   (and vice versa) — the membership fragments by accident of restart timing.

### Pinned consumers (line refs at `40ab307c`)

The engine holds `channel_key: Arc<ChannelKey>` (`community_channel_log_engine.rs:473`),
fed by three spawn paths, all deriving from the spawn-pinned membership key:

- `reconcile_from_state` (boot + in-session walk) — derives per channel at `:3075`.
- `Registry::spawn` — receives a pre-derived `ChannelKey`; `DeferredSpawn` (`:113`)
  captures it across ZEB-271 transactions.
- `lib.rs::register_channel_log_engine` (`:32472`) — derives from
  `community_engine.membership_key()` for both the eager `create_channel_impl`
  spawn and the delta-consumer `Created` hook.

Production key uses inside the engine (all in async contexts):

| Site | Op | Direction |
|---|---|---|
| `publish_event` `:1098` | `encrypt_channel_packet` | encrypt |
| `publish_react` `:1300` | `encrypt_channel_packet` | encrypt |
| backfill catch-up request `:1227` | `seal_watermark_vector` | encrypt |
| `rbsr_build_initial` `:1954` | `seal_rbsr_message` | encrypt |
| `rbsr_respond` `:1916` | `open_rbsr_message` + `seal_rbsr_message` | both |
| `rbsr_ingest_and_next` `:1987` | `open_rbsr_message` (frame classify) + seal next | both |
| `process_inbound_packet` `:1622` | `decrypt_channel_packet` | decrypt |
| registry `read_for_query` serve task `:2597-2635` | `open_watermark_vector` + encrypt reply events | both |

One consumer outside the engine (found by this audit; not in the ticket):

- **Voice join** — `lib.rs:28516` `engine.channel_key_arc()` → `VoiceJoinCaps.channel_key`
  (ZEB-350). The voice relay holds the key for the lifetime of a join; per-call
  keys derive from it downstream (`event_loop.rs` caps clones).

Out of scope (not membership-key-derived): DM voice keys (`derive_dm_voice_key`,
DM-content-key based), group-DM presence, community presence/addrbook (done in
ZEB-919).

## 2. Approach decision

**Chosen: per-op live re-derive** — no re-key machinery. The ticket's suggested
shape §1 asked to measure HKDF cost first because a cheap re-derive "collapses
the whole re-key story into two call-site changes." It does:

- `derive_channel_key` is one HKDF-SHA256 expand (~2 compression calls, sub-µs)
  against a ChaCha20-Poly1305 AEAD + Ed25519 signature verify already paid on
  every packet, at human chat rates.
- The only structural cost is one `OwnerState` tokio-mutex lock per operation
  (`live_epoch_key` / `epoch_key_candidates`). Presence already pays this per
  tick; channel ops are human-initiated or batched. The backfill/RBSR serve
  paths hoist one key fetch per request, not per event.

Rejected:

- **Provider seam + re-key on rotation delta** (engine holds
  `RwLock<Arc<ChannelKey>>`, delta consumer pushes re-keys): more state, couples
  the delta consumer to the channel-log registry, and still needs decrypt
  candidates to handle rotation skew — per-op re-derive gets identical semantics
  with none of the machinery.
- **Epoch-stamped key cache** (re-derive only when `Space.current_epoch`
  changes): premature optimization; adds a coherence invariant with no measured
  need. Revisit only if the owner-state lock ever shows up in a profile.

Key facts making per-op selection safe: nothing in engine *state* depends on
which key sealed a packet — replay tracking, verification, and persistence all
operate on the decrypted `SignedChannelEvent`; segments store plaintext. Key
selection can move from spawn-time to op-time with no migration, no re-key
event, and no wire-format change.

## 3. Design

### 3.1 `ChannelKeyLiveSource` (new, `community_channel_log_engine.rs`)

```rust
pub(crate) struct ChannelKeyLiveSource {
    /// Spawn-time membership key — the fallback the live read degrades to
    /// (publisher-degrades, ZEB-597 mirror).
    pub membership_key: EpochKey,
    /// Live owner-state — `Space.current_epoch_key` / `old_epoch_keys`.
    pub crdt_state: Arc<tokio::sync::Mutex<OwnerState>>,
}
```

Threaded as `Option<ChannelKeyLiveSource>` through `ChannelLogEngineParams`,
`DeferredSpawn`, `Registry::spawn`, `reconcile_from_state`, and the engine
struct. `None` is the documented degraded/test mode: every existing test
constructs it, and behavior is byte-identical to today (pinned spawn key both
directions). Both fields travel together — no half-wired state where a live
read exists without its fallback.

### 3.2 Engine key selection (new methods)

```rust
/// Encrypt/seal key: live current epoch (degrades to the pinned spawn key).
async fn encrypt_channel_key(&self) -> Arc<ChannelKey>;
/// Decrypt/open candidates: [current, previous] epochs (degrades to [pinned]).
async fn decrypt_channel_keys(&self) -> Vec<Arc<ChannelKey>>;
```

- `encrypt_channel_key`: `None` source → `Arc::clone(&self.channel_key)`;
  `Some` → `community_publish_epoch_key_typed(community, Some(crdt), &membership_key)`
  then `derive_channel_key`. Every degrade lands on the spawn key — never worse
  than today.
- `decrypt_channel_keys`: `None` → `vec![pinned]`; `Some` →
  `epoch_key_candidates(...)` (ZEB-918: `[current, previous]`, never more than
  one epoch back) mapped through `derive_channel_key`. When the live read
  degrades inside `epoch_key_candidates` it returns `[fallback]`, which derives
  to exactly the pinned key.

### 3.3 Candidate-open helpers (`community_channel_log.rs`)

Pure, unit-testable without zenoh, next to their single-key siblings
(mirroring ZEB-919's `open_presence_with_any` / `open_records_with_any`):

- `decrypt_channel_packet_with_any(keys, packet) -> Result<SignedChannelEvent, ChannelEventError>`
  — first key that opens wins; all-fail returns the LAST error so
  `process_inbound_packet`'s garbage-drop warn keeps a real cause.
- `open_watermark_vector_with_any(keys, bytes) -> Option<...>` (matches the
  existing `open_watermark_vector` option-shape at the serve site).
- `open_rbsr_message_with_any(keys, frame) -> Result<RbsrMessage, ...>` (matches
  `open_rbsr_message`'s result-shape; `rbsr_ingest_and_next`'s frame
  classification treats all-keys-fail as "inline Have packet", same as today's
  single-key fail).

### 3.4 Consumer conversion

Every encrypt site calls `encrypt_channel_key().await` at the top of the
operation; every decrypt site calls `decrypt_channel_keys().await` and the
`_with_any` helper. Specifics:

- `rbsr_respond`: open the request under candidates, seal the reply under the
  live key (fetched once at fn entry). A requester one epoch behind gets a
  reply it cannot open — acceptable: the RBSR driver already falls back to the
  vector path on open-failure, and the vector/backfill path serves events as
  channel packets which the requester CAN open via its own candidate rungs.
- `rbsr_ingest_and_next`: classify frames under candidates; seal the next
  round's request under live. `Have` packets route through
  `process_inbound_packet`, which decrypts under candidates anyway.
- Registry `read_for_query` serve task: one `decrypt_channel_keys()` +
  `encrypt_channel_key()` fetch per request, hoisted outside the reply-event
  loop.
- Voice join (`lib.rs:28516`): `engine.channel_key_arc()` →
  `engine.encrypt_channel_key().await`. Join-time live read only; a call in
  progress across a rotation keeps its key (calls are minutes, epochs are
  long-lived; mid-call re-key is a session-protocol change, out of scope).
- `channel_key_ref` / `channel_key_arc`: after conversion, retire to
  test-gated or delete if unused — no production site may read the pinned key
  directly (that is the grep-clean invariant ZEB-919 established for the other
  families).

### 3.5 Spawn-site threading (`lib.rs`)

`register_channel_log_engine` and `reconcile_community_channel_logs` gain a
`crdt_state: Option<Arc<Mutex<OwnerState>>>` param; the boot reconcile
(`lib.rs:9044`) and delta-consumer `Created` hook (`lib.rs:7947`) pass clones of
the owner-block `crdt_state` (the same Arc ZEB-919 threaded to presence /
addrbook); IPC-scope callers (`create_channel_impl`, open-join reconcile at
`:42202`, `:37039`) reach it via node state (field at `lib.rs:968`) or their
enclosing scope. Each site builds
`ChannelKeyLiveSource { membership_key: community_engine.membership_key(), crdt_state }`.
`reconcile_from_state` keeps its `membership_key: &EpochKey` param (it derives
per channel) and gains the optional `crdt_state`, constructing the source per
spawned engine.

## 4. Rotation semantics (mirror of ZEB-918/919)

| Scenario | Behavior |
|---|---|
| Rotated member ← un-rotated member's OLD-sealed packet | Opens via previous-epoch rung — the healing direction; membership sync keeps flowing so rotation propagates. |
| Un-rotated member ← rotated member's NEW-sealed packet | Cannot open until the rotation event reaches it via membership sync (bounded by propagation, not process lifetime). Cryptographically unavoidable; identical to ZEB-918/919. |
| Revoked member reading NEW traffic | Window collapses immediately: live members seal under the new key on their next op, no restarts required. (Today: window lasts until every member restarts.) |
| Revoked member reading residual OLD-sealed traffic | Possible until propagation completes (un-rotated members still seal OLD) — inherent to any propagation-based rotation. |
| Degraded (`None` source, or live read misses) | Pinned spawn key both directions — byte-identical to today's behavior. Never worse. |
| >1 epoch behind | Hard cut (candidates never reach further back) — ZEB-918 precedent; backfill after re-admission covers the gap since segments are plaintext at rest. |

## 5. Testing

- **Unit (candidate-open helpers ×3):** OLD-sealed artifact opens under
  `[new, old]` candidates; unrelated key rejected; empty/garbage rejected.
- **Engine rotation pins (mirror ZEB-919's):** engine constructed with a
  `ChannelKeyLiveSource` over a rotated `OwnerState`
  (`test_community_space(c, 1, live_key)` fixture from ZEB-919):
  - publish seals under the NEW key while the engine's pinned key is OLD
    (decrypt with new-derived key succeeds; spawn-derived fails);
  - `process_inbound_packet` accepts an OLD-sealed packet via the previous
    rung post-rotation (replay/append proceeds normally);
  - watermark + RBSR seal-live/open-candidates pins.
- **Degraded regression:** `live_key_source: None` behaves exactly as today —
  the entire existing engine suite (which constructs `None`) is the regression
  net; no existing assertions change.
- **Voice join:** join-time key selection follows the rotated live state
  (pin at the `lib.rs` seam or via engine helper test).
- Full workspace sweep + clippy `-D warnings` + fmt.

## 6. Rollout

No wire-format, CRDT-schema, or IPC changes — packets, AAD, and shapes are
unchanged; only which key seals/opens moves. Mixed-version fleets: an old
binary behaves like today's un-restarted member and stays covered by new
binaries' previous-epoch decrypt rungs until rotation propagates; degraded
paths land on the same spawn key as today.
