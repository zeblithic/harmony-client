# ZEB-215 Sub-A Phase 3b: Real harmony-content CAS — Design

**Status:** Draft (2026-05-01)
**Implements:** ZEB-215 Sub-A Phase 3b — closes the deliberate stub-shaped hole left by Phase 3a so cross-device state-root sync works end-to-end.
**Depends on:** Phase 3a ([state-root sync + persistence](2026-05-01-zeb-215-sub-a-phase3a-sync-design.md)) merged in PR #74.
**Companion:** A small harmony-content PR (see [Cross-repo touch](#cross-repo-touch-harmony-content)).
**Unblocks:** Phase 4 (Tauri IPC), Phase 5 (frontend NavService rewrite). Reboot durability of the CAS layer is deferred to a follow-up phase (see [Non-goals](#non-goals)).

## Goals

Phase 3b replaces Phase 3a's `InMemoryStub` `ContentStore` with the real harmony-content CAS, threaded through the existing `NodeRuntime` + `StorageTier` instance that already runs on the `harmony-runtime` thread (`lib.rs:830`). After this phase, two paired devices on the same LAN converge their `OwnerState` over Zenoh through harmony-content's transport — the `state-root-sync-degraded` banner Phase 3a left behind is retired.

In scope:

1. **Async `ContentStore` trait.** `put` and `get` become `async fn` so the real adapter can await network fetches.
2. **`RuntimeContentStore` adapter.** Implements `ContentStore` by sending operations on a new mpsc channel into the harmony-runtime event loop and awaiting oneshot replies.
3. **`CasOp` channel handler in `event_loop.rs`.** Single new select arm processes both `PutLocal` (admit our own ciphertext to local cache) and `GetOrFetch` (cache-or-network-with-timeout, plus fire-and-forget admit-on-success).
4. **Wire-format reinterpretation of `RootPublishPayload.root_cid`.** Same 32-byte bstr, new meaning: harmony-content's structured `ContentId` (4-byte header + 28-byte SHA-256-MSB-truncated hash). Phase 3a's BLAKE3 interpretation never escaped a single process; treating v1 as never-deployed is honest.
5. **Companion PR in harmony-content.** `Serialize for ContentId` emits CBOR `bstr(32)` instead of an array-of-u8. Required for harmony-client's bstr-based wire format; bytewise-identical in postcard, the workspace's primary codec.
6. **Retire `state-root-sync-degraded` event.** Phase 3a's permanent "we know sync doesn't work" banner deletes. If a future phase needs a degraded indicator, it'll be a different signal with its own reason payload.

## Non-goals

These are deferred and intentionally NOT in Phase 3b's scope:

- **Reboot durability of CAS state.** `MemoryBookStore` is in-memory; a device restart drops its cached blobs. The Phase 3a persistence files (`owner_state_crdt.cbor`, `state_root_replay.cbor`) survive restart and re-broadcast our state-root, which (assuming any peer is online) heals cross-device state. A device that restarts in isolation can publish but cannot serve historical CIDs from cache. Disk-backed `BookStore` is a separate phase.
- **Per-entry blob refactor.** The "root blob" stays the full `OwnerState` canonical-CBOR-encoded as a single ciphertext, just like Phase 3a. Restructuring into per-entry blobs (matching ZEB-206 Flow A's Prolly Tree shape) is its own design exercise and irrelevant to "cross-device sync works."
- **Bounded retry queue for failed fetches.** A fetch that misses the 500ms deadline is logged and dropped; CRDT eventual consistency carries the recovery via the next publisher's state-root. Pathological miss-rate handling is a future phase if observability shows we need it.
- **Tauri IPC commands wrapping SyncEngine** — Phase 4.
- **Frontend integration / Svelte stores listening to `nav-updated` events** — Phase 5. Phase 3b emits no new Tauri events; the `state-root-sync-degraded` deletion is a removal, not an addition.
- **DM content encryption** (per-Space content keys) — Sub-B / ZEB-219.
- **Cross-internet sync** (non-LAN, NAT traversal). The current Zenoh session is LAN/mDNS; cross-NAT is a transport-layer concern.

## Scope split: Phase 3b vs follow-ups

The original Phase 3 framing bundled "Zenoh state-root sync + harmony-content CAS persistence" and Phase 3a separated the protocol from the CAS integration. Phase 3b sits in the middle: real CAS over the existing in-memory `BookStore`, with reboot durability and per-entry refactoring as separate later phases. Each phase produces working, observable behavior:

- **Phase 3a** (merged): state-root pub/sub + on-disk owner-state + ContentStore trait + InMemoryStub.
- **Phase 3b** (this spec): real harmony-content CAS via NodeRuntime; `state-root-sync-degraded` retires.
- **Phase 3c** (future): on-disk `BookStore` so cached blobs survive reboot.
- **Phase 3d** (future): per-entry blobs replacing the single-ciphertext root.

The split keeps each PR's review surface tractable and each merge user-visible.

## Cross-repo touch: harmony-content

harmony-content's current `Serialize for ContentId` (cid.rs:400-404) is:

```rust
impl Serialize for ContentId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_bytes().serialize(serializer)
    }
}
```

`to_bytes()` returns `[u8; 32]`, which serde encodes as a 32-element tuple of u8. In CBOR codecs (ciborium, serde_cbor) this becomes a major-type-4 array of 32 unsigned-int values — significantly larger than the `bstr(32)` shape harmony-client's wire format uses elsewhere (e.g., `OwnerAddr`, the existing `RootPublishPayload.root_cid`).

The fix is a five-line change:

```rust
impl Serialize for ContentId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.to_bytes())
    }
}
```

Plus a unit test asserting ciborium encodes a `ContentId` as a 33-byte CBOR `bstr(32)` (1-byte tag + 32 payload).

This change is bytewise-identical in postcard (the workspace's primary codec, which encodes both `[u8; 32]` and `&[u8]` of known length as 32 raw bytes). harmony-content's existing consumers — harmony-mail, harmony-runtime, harmony-mailbox — all use postcard for wire shapes, so the change is non-breaking on their hot paths. Audit step before merging the harmony-content PR: grep for any consumer that does CBOR-style serde of a `ContentId`. Expectation: none today.

The harmony-content PR ships first; harmony-client's PR pulls a `[patch]` override or pinned commit until harmony-content's main absorbs it.

## Module boundaries

Three files modified in `src-tauri/src/`. No new files.

- **`content_store.rs`** (~150 → ~230 lines) — trait becomes async; `InMemoryStub` keeps in-process semantics for unit tests; new `RuntimeContentStore` adapter sends `CasOp` over an mpsc and awaits a oneshot reply; `CasOp` enum lives here as the single import site for both producer (`RuntimeContentStore`) and consumer (`event_loop::run`).
- **`event_loop.rs`** (~100 lines added) — single new select arm `Some(op) = cas_op_rx.recv() => { ... }`. Reuses the existing `fetch_via_zenoh` helper and the existing `RuntimeEvent::SubscriptionMessage` ingest pattern; no new Zenoh paths.
- **`lib.rs`** (~30 lines) — new `cas_op` channel constructed near the existing `publish_rx`/`fetch_rx` pair; `cas_op_rx` threaded into `event_loop::run`'s arguments; `Arc::new(RuntimeContentStore { cas_op_tx })` replaces the existing `Arc::new(InMemoryStub::default())` at SyncEngine construction; the `state-root-sync-degraded` emit at the end of `start_node` deletes.
- **`owner_state_sync.rs`** (~20 lines) — `publish_root_now`'s `let root_cid = ContentId(blake3::hash(...).into())` → `let root_cid = ContentId::for_book(&ciphertext, ContentFlags { encrypted: true, ..Default::default() })?`; import path adjusts from `crate::owner_state_types::ContentId` to `harmony_content::cid::{ContentId, ContentFlags}`; `.put().await` and `.get().await` call sites pick up the new async signatures naturally.
- **`owner_state_types.rs`** (small touch) — local `pub struct ContentId([u8; 32])` removes; `RootPublishPayload` re-imports from harmony-content; the `impl_canonical!` macro list still references `ContentId` (now harmony-content's type), so its `CanonicalPayload` impl needs to live in `owner_state_types.rs` as a foreign-impl line (sealed-trait pattern allows this — `CanonicalPayloadSealed` is in scope here).

Phase 1's `owner_state_crypto.rs`, Phase 2's `owner_state_crdt.rs`, and Phase 3a's `owner_state_persist.rs` stay untouched.

## Architecture

### High-level flow

```
Publisher (local mutation):
  apply_outbox(entry)                            ← Phase 2 CRDT mutation, sync
    │
    ▼ engine.notify_dirty()                      ← Phase 3a, unchanged
    │
    └── (250ms debounce window)
    │
    ▼ publish_root_now():
        let snapshot = owner_state.lock().clone()
        let now = next_hlc()                     ← strictly newer than last
        let cleartext = canonical_cbor_encode(&snapshot)
        let ciphertext = encrypt_entry(&kt, &owner_state_root_lookup_key, &cleartext)
        let cid = ContentId::for_book(            ← NEW: harmony-content structured CID
            &ciphertext,
            ContentFlags { encrypted: true, ..Default::default() }
        )?
        content_store.put(cid, ciphertext).await  ← NEW: routes through CasOp::PutLocal
        let payload = canonical_cbor_encode(&RootPublishPayload { root_cid: cid, at: now })
        let wire = encrypt_root_publish(&kt, &payload)
        zenoh_publisher.put(wire).await           ← unchanged

Subscriber (incoming Zenoh delivery on harmony/owner/{addr}/state-root-v1):
  let payload = decrypt_root_publish(&kt, &wire)
  let RootPublishPayload { root_cid, at } = canonical_cbor_decode(&payload)
  if !tracker.accept(&at):
    return                                       ← replay rejected
  let blob_opt = content_store.get(&root_cid).await  ← NEW: routes through CasOp::GetOrFetch
  let blob = match blob_opt {
    Some(b) => b,
    None => {
      log_warn("blob fetch timed out for cid={cid}, hlc={at}")
      return                                     ← drop; CRDT carries recovery
    }
  }
  let cleartext = decrypt_entry(&kt, &lookup_key, &blob)
  let remote: OwnerState = canonical_cbor_decode(&cleartext)
  // ... apply_* iteration (Phase 2 CRDT semantics, unchanged)
  // ... persist_crdt_debounced + tracker_dirty (Phase 3a, unchanged)
```

### `CasOp` channel protocol

```rust
// content_store.rs
pub enum CasOp {
    PutLocal {
        cid: ContentId,
        blob: Vec<u8>,
        reply: tokio::sync::oneshot::Sender<Result<(), ContentStoreError>>,
    },
    GetOrFetch {
        cid: ContentId,
        timeout: std::time::Duration,
        reply: tokio::sync::oneshot::Sender<Result<Option<Vec<u8>>, ContentStoreError>>,
    },
}

#[async_trait::async_trait]
pub trait ContentStore: Send + Sync {
    async fn put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError>;
    async fn get(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError>;
}

pub struct RuntimeContentStore {
    cas_op_tx: tokio::sync::mpsc::Sender<CasOp>,
    fetch_timeout: std::time::Duration,  // DEFAULT_FETCH_TIMEOUT_MS = 500
}
```

`RuntimeContentStore::put` constructs `PutLocal`, sends it on `cas_op_tx`, awaits the reply, and propagates errors. `RuntimeContentStore::get` does the same with `GetOrFetch` carrying `self.fetch_timeout`. Both translate channel-closed (`SendError`) to `ContentStoreError::Io("event loop unavailable")`.

### Event loop handler

In `event_loop.rs`, a new select arm:

```rust
Some(op) = cas_op_rx.recv() => {
    match op {
        CasOp::PutLocal { cid, blob, reply } => {
            // Admit to local cache via existing StorageTier ingest path.
            // Mirrors the existing ingest_rx arm but with the CID already
            // pre-computed (no hex-decode).
            let cid_hex = hex::encode(cid.to_bytes());
            let key_expr = format!("harmony/content/publish/{cid_hex}");
            runtime.push_event(RuntimeEvent::SubscriptionMessage {
                key_expr,
                payload: blob,
            });
            for action in runtime.tick() {
                dispatch_action(action, &session, &zenoh_tx, &udp,
                                &broadcast_addr, &app, &closing, &own_zid).await;
            }
            let _ = reply.send(Ok(()));
        }
        CasOp::GetOrFetch { cid, timeout, reply } => {
            // 1. Cache check first.
            if let Some(bytes) = runtime.storage_tier().cache().get(&cid).map(|b| b.to_vec()) {
                let _ = reply.send(Ok(Some(bytes)));
                continue;
            }
            // 2. Cache miss — Zenoh GET with timeout.
            let cid_hex = hex::encode(cid.to_bytes());
            let prefix = cid_hex.get(1..2).unwrap_or("");
            let key = format!("harmony/content/{prefix}/{cid_hex}");
            let session = session.clone();
            let cas_op_tx_for_admit = cas_op_tx.clone();  // for re-entry hop, see below
            tokio::spawn(async move {
                let fetch = fetch_via_zenoh(&session, &key);
                match tokio::time::timeout(timeout, fetch).await {
                    Ok(Ok(bytes)) => {
                        // 3. Best-effort admit via try_send. We have the
                        //    bytes for the caller regardless of whether
                        //    caching succeeds — admit is fire-and-forget
                        //    so network-fetch latency isn't blocked on
                        //    local cache contention or event-loop progress.
                        //    If the cas_op channel is full or closed,
                        //    caching is skipped; subsequent GetOrFetch on
                        //    this CID will re-fetch over the network.
                        let (admit_tx, _admit_rx) = tokio::sync::oneshot::channel();
                        let _ = cas_op_tx_for_admit.try_send(CasOp::PutLocal {
                            cid,
                            blob: bytes.clone(),
                            reply: admit_tx,
                        });
                        let _ = reply.send(Ok(Some(bytes)));
                    }
                    Ok(Err(e)) => { let _ = reply.send(Err(ContentStoreError::Io(format!("fetch: {e}")))); }
                    Err(_)     => { let _ = reply.send(Ok(None)); }  // timeout → None
                }
            });
        }
    }
}
```

### Re-entry: spawned fetch task admitting into the runtime

The fetch task runs outside the event loop's select (it's spawned to avoid blocking the select arm during the network GET). Once bytes are in hand it admits them via a fire-and-forget second-mpsc-hop: the spawned task uses `cas_op_tx_for_admit.try_send(CasOp::PutLocal { ... })` to enqueue the admit, then immediately replies `Ok(Some(bytes))` to the original `GetOrFetch` caller WITHOUT waiting for the admit oneshot reply. The admit eventually runs through the select arm if there's room in the channel; if the channel is full or closed, the admit is dropped silently and the caller still receives the bytes.

This design preserves two important properties:
- **Bounded latency.** `RuntimeContentStore::get` cannot block longer than the configured `fetch_timeout` plus a constant for `try_send` (microseconds). Earlier designs awaited the admit reply, which under cas_op channel backpressure could extend `get` latency unboundedly.
- **Always preserve valid bytes.** When a fetch succeeds, the bytes are always returned to the caller — caching is opportunistic. Earlier designs returned `Ok(None)` on admit failure, which would discard valid bytes in hand and force the subscriber to drop the publish for purely-local-cache reasons.

The full `GetOrFetch` happy path is:
```
SyncEngine.get(cid)
  → cas_op_tx::send(GetOrFetch{cid, t, reply_outer})
  → event loop select arm
    → cache miss
    → spawn fetch task
  → spawn task: fetch_via_zenoh(session, key) within tokio::time::timeout(500ms)
    → bytes returned
    → cas_op_tx::try_send(PutLocal{cid, bytes, _admit_rx_dropped})  ← fire-and-forget
    → reply_outer ← Ok(Some(bytes))                                  ← immediate
SyncEngine.get returns Ok(Some(bytes))
(Admit eventually drains through the select arm, or is dropped if channel full/closed.)
```

The corrupted-bytes case (peer served bytes that fail StorageTier's hash check) manifests as: the admit eventually runs and is silently dropped by StorageTier, the cache stays empty for this CID, and the next `GetOrFetch` on the same CID re-fetches over Zenoh. The caller of THIS `GetOrFetch` still receives the (corrupt) bytes — but they would fail downstream decryption (`decrypt_entry`) which surfaces as `SyncError::Crypto` and the publish is dropped. CRDT eventual consistency carries the recovery via the next state-root from any peer.

### Lifecycle

Boot sequence (extends Phase 3a's, in `lib.rs::start_node`):

```
1. Tauri setup hook fires (unchanged).
2. Legacy load_owner_state() → master_seed (unchanged).
3. KeyTree::derive(...) → AEAD keys (unchanged).
4. load_crdt_from_disk() / load_replay_from_disk() (unchanged).
5. NEW: cas_op channel (cas_op_tx, cas_op_rx) constructed.
6. SyncEngine::new(..., Arc::new(RuntimeContentStore { cas_op_tx, fetch_timeout }), ...)
7. NodeRuntime spawned on harmony-runtime thread (existing thread::Builder), event_loop::run now receives cas_op_rx alongside its existing channel arguments.
8. App ready.
```

Step 5 must happen before step 6 so SyncEngine takes the `Sender` end at construction. Step 7 takes the `Receiver` end. Both ends are alive before `tick()` runs, so the first `notify_dirty` after boot correctly queues a `PutLocal`. The 250ms debounce window comfortably covers any startup latency between the channel creation and event-loop entry.

The `state-root-sync-degraded` emit at the end of `start_node` deletes — Phase 3a's documented stub-shaped hole closes here.

Shutdown (extends Phase 3a's `stop_inner` flow). SyncEngine's existing shutdown path (force-publish + persist) sends one final `PutLocal` through `cas_op_tx` if dirty before signaling complete. The event loop's existing shutdown path drains `cas_op_rx` before exiting — mirroring how `publish_rx` and `fetch_rx` already drain. No new ordering invariant.

## Wire format

`RootPublishPayload` keeps its existing field layout:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootPublishPayload {
    #[serde(rename = "rc")]
    pub root_cid: ContentId,        // 32-byte bstr — see below
    #[serde(rename = "at")]
    pub at: Hlc,
}
impl_canonical!(RootPublishPayload);
```

What changes is the *meaning* of `root_cid`'s 32 bytes:

- **Phase 3a:** `ContentId([u8; 32])` = raw BLAKE3 hash of the encrypted root blob.
- **Phase 3b:** `harmony_content::cid::ContentId` = 4-byte structured header (mode/depth/size/checksum) + 28-byte hash. With `ContentFlags { encrypted: true, ..Default::default() }` the hash is SHA-256 truncated to its 224 most-significant bits.

Wire shape stays a 32-byte `bstr` (after the harmony-content companion PR ships). Phase 3a's BLAKE3 interpretation never escaped a single process — the stub exchanged blobs only within one harmony-client instance — so treating v1 as never-deployed is honest. The Zenoh topic name `harmony/owner/{addr_hex}/state-root-v1` stays.

The encryption-key derivation is unchanged: `space_lookup_key(&kt, b"owner-state-root-blob-v1")` keeps the same byte literal. The byte literal is the schema discriminator for the *blob* (versioned for future per-entry refactor); the topic name is the schema discriminator for the *publish wire shape* (versioned for `at` / `root_cid` field changes). They're independent.

## ContentStore trait migration

Phase 3a:
```rust
pub trait ContentStore: Send + Sync {
    fn put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError>;
    fn get(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError>;
}
```

Phase 3b:
```rust
#[async_trait::async_trait]
pub trait ContentStore: Send + Sync {
    async fn put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError>;
    async fn get(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError>;
}
```

`async-trait = "0.1"` is already in `Cargo.toml`. `InMemoryStub`'s impl just adds `async` to the method bodies — the in-process semantics don't need to actually `.await` anything but the trait demands the `async` keyword.

`SyncEngine::new` continues to take `Arc<dyn ContentStore>`; production wires `RuntimeContentStore`, tests can wire either `InMemoryStub` (single-process integration) or a custom mock.

All Phase 3a `.put()` / `.get()` call sites in `owner_state_sync.rs` pick up `.await` — about 8 sites, mechanical change.

## Error handling

Three new failure modes fold into existing `ContentStoreError` / `SyncError` variants:

1. **Cache admit rejected (StorageTier policy).** `runtime.tick()` after a `PublishContent` event can yield a `RuntimeAction` indicating the content was rejected (e.g., budget exhausted). The `PutLocal` arm inspects actions for rejection and surfaces as `ContentStoreError::Io("admit rejected: ...")`. With Phase 3a's StorageBudget (`cache_capacity: 512`, `max_pinned_bytes: 50_000_000`), single-digit-KB owner-state blobs will not realistically hit this — but failing loudly beats silent corruption. Bubbles through `SyncError::ContentStore` into the existing degraded-path logging.
2. **Network fetch timeout (GetOrFetch).** Returned as `Ok(None)` — semantically "blob not present in network within deadline." SyncEngine logs at `WARN` with the CID hex and HLC, drops the publish. CRDT eventual consistency carries the recovery via the next state-root from any peer.
3. **Hash verify failure on receipt.** `runtime.push_event(SubscriptionMessage{...}) + tick()` validates `cid.verify_hash(data)` inside StorageTier before admitting. With the fire-and-forget admit design, a hash-verify failure causes StorageTier to silently drop the admit; the cache stays empty for this CID. The caller of `GetOrFetch` already received the (corrupt) bytes, but they fail downstream decryption (`decrypt_entry`) which surfaces as `SyncError::Crypto` and the publish is dropped. CRDT eventual consistency carries the recovery. Admit observability (previously `Ok(None)` returned to caller on reject) is intentionally removed — bounded latency is the higher priority.

Existing `SyncError::ContentStore` (for CAS misses and timeouts) and `SyncError::Crypto`/`SyncError::CborDecode` (for decrypt and decode failures) cover all three failure modes. No new variants.

## Testing strategy

Three test layers, each catching different bugs:

### Unit tests in `content_store.rs`

- **Existing `InMemoryStub` round-trip** — verifies the trait surface still works for in-process testing. Phase 3a's three tests stay; only the test functions become `#[tokio::test]` and the calls add `.await`.
- **New: `RuntimeContentStore` happy path** — construct with a stub mpsc receiver loop that returns canned `Ok(...)` replies. Verify the channel send + reply-await dance for both `put` and `get`.
- **New: `RuntimeContentStore` error paths** — receiver replies with `Err(...)` (admit rejection); receiver dropped (channel closed); oneshot dropped before reply (caller-side cancellation).
- **New: `RuntimeContentStore` timeout simulation** — receiver delays past `fetch_timeout`; verify `get` returns `Ok(None)`. (The actual `tokio::time::timeout` is in the event loop; this test verifies the `RuntimeContentStore` correctly propagates whatever the event loop replied — including `Ok(None)`.)

### Integration tests in `owner_state_sync.rs`

- **Existing two-engine + shared `Arc<InMemoryStub>` tests** keep working unchanged. `InMemoryStub` is still in the codebase, still exercises trait conformance, and remains the primary unit-level integration harness.
- **New: end-to-end channel protocol test** — two SyncEngines + two real `RuntimeContentStore`s + one shared `cas_op` mpsc + a stub event-loop task that simulates StorageTier behavior (HashMap-backed, no real Zenoh). Verifies the full `CasOp` protocol: publisher's `PutLocal`, subscriber's `GetOrFetch` with cache hit, subscriber's `GetOrFetch` with cache miss + simulated network fetch.
- **New: subscriber-side timeout test** — stub event-loop replies with `Ok(None)` for one specific CID; verify SyncEngine logs at WARN, drops the publish, state stays consistent, next valid state-root from the same peer applies normally.
- **New: hash-verify-failure test** — stub event-loop replies with `Ok(None)` simulating admit rejection; verify same behavior as timeout (dropped, no error escalation).

### End-to-end manual test (one-shot validation, not in CI)

Two physical devices on the same LAN, both running harmony-client release builds:

1. Pair them via the existing pairing flow (Phase 3a foundation).
2. Mutate `OwnerState` on device A — e.g., create a Space via the Tauri command harness or test driver.
3. Observe within ~750ms (250ms debounce + up to 500ms fetch budget) that the same Space appears on device B's `OwnerState` snapshot.
4. Mutate on device B; verify it propagates back to A.
5. Pause one device for ~5 seconds; mutate on the other; resume the paused device; verify CRDT convergence.

This is the operational definition of "cross-device sync works."

### What's explicitly not tested in this PR

- **Reboot durability** — `MemoryBookStore` loses state on restart; that's Phase 3c's territory.
- **Three-device convergence** — Phase 3a's CRDT properties cover three devices; three-device just exercises the same code paths.
- **Network partition / split-brain** — needs a Zenoh fault-injection harness; out of scope.
- **Rapid-fire mutation under fetch back-pressure** — would require a load-test harness; deferred until production telemetry shows we need it.

## Risks and open questions

- **`runtime.tick()` admit-rejection signal.** Need to confirm StorageTier's actions vector exposes a "content rejected" action distinct from successful ingest, and that we can match on it cleanly in the `PutLocal` arm. If StorageTier silently drops rejected content, the `PutLocal` arm cannot distinguish success from rejection without a deeper API addition. **Mitigation:** read `harmony_runtime::runtime::tick`'s implementation early; if needed, the harmony-content companion PR adds the missing action variant.
- **Zenoh topic prefix collisions.** State-root payloads land on `harmony/owner/{addr}/state-root-v1`; CAS blobs land on `harmony/content/{prefix}/{cid_hex}`. The two namespaces are independent. The harmony-content companion PR doesn't change topic taxonomy.
- **`cas_op_tx` capacity tuning.** mpsc bounded channel; what capacity? Phase 3a's `flush_now`/`shutdown` channels use bounded(1) for back-pressure semantics. CasOp can be similarly bounded — at most one in-flight `PutLocal` (publisher's debounce serializes them) and at most one in-flight `GetOrFetch` per subscriber-arrival. **Decision:** start with `bounded(8)` to absorb the spawned-task re-entry hop without back-pressure surprises. Revisit if telemetry shows congestion.
- **`async-trait` overhead.** Every trait method call boxes a future. For `get` (called once per subscriber arrival) and `put` (called once per publish), the allocation cost is in the noise — milliseconds of network I/O dominate. No optimization needed.
- **Test infrastructure: stub event loop.** The integration tests need a small in-process simulator of the event-loop CasOp arm. Roughly 30 lines: spawn a tokio task that recvs from the shared mpsc, maintains a `HashMap<ContentId, Vec<u8>>`, handles both variants. Lives in `owner_state_sync.rs` under `#[cfg(test)]`. Reusable by future tests.

## Acceptance criteria

- All existing tests pass (`cargo test --workspace`).
- New unit + integration tests pass.
- `cargo fmt --all -- --check` clean.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `state-root-sync-degraded` emit and its frontend listener (if any) deleted.
- Two-device manual test (5 steps above) passes.
- harmony-content companion PR merged before harmony-client PR opens.

## Implementation order (preview for the plan phase)

1. harmony-content companion PR: change `Serialize for ContentId` + ciborium round-trip test. Get this merged first.
2. harmony-client: bump harmony-content dep to the merged commit.
3. harmony-client: `ContentStore` trait → async; update `InMemoryStub`; update all Phase 3a call sites to `.await`. (Tests still green at this checkpoint.)
4. harmony-client: introduce `CasOp` enum + `RuntimeContentStore` adapter (with stub-receiver unit tests).
5. harmony-client: wire `cas_op` channel through `lib.rs` + `event_loop.rs`; implement the new select arm including spawned-fetch + admit re-entry.
6. harmony-client: switch `RootPublishPayload.root_cid` from local `ContentId([u8; 32])` to `harmony_content::cid::ContentId`; update CID derivation in `publish_root_now`.
7. harmony-client: replace production `Arc::new(InMemoryStub::default())` with `Arc::new(RuntimeContentStore { ... })`; delete `state-root-sync-degraded` emit.
8. harmony-client: end-to-end channel protocol integration test.
9. harmony-client: manual two-device LAN validation; document outcome in PR description.

Each numbered step is a single commit with its own tests passing.
