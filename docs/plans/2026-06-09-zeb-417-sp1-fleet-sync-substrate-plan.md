# ZEB-417 SP1 — Fleet Sync Substrate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract the copy-pasted owner/mint sync engine into one reusable generic `FleetSyncEngine<S>`, migrate `owner_state_sync` onto it with byte-identical wire/disk behavior, and add **Notes** as a second owner-private synced dataset — plus a best-effort "synced to N devices" indicator and the narrow SP1↔SP2 seam.

**Architecture:** A new `fleet_sync` module owns the engine: the `notify_dirty`→debounce→`publish_root_now` loop, the `decrypt→replay→CAS-fetch→merge→advance→persist` receive path, `next_hlc`, and the `FleetRootPublish` envelope. Per-dataset behavior is injected via a `FleetSyncConfig<S>` (snapshot type `S`, a `merger` closure, a `FleetPersist<S>` trait impl, a `lookup_key_tag`, a `publish_seen` flag, an optional `on_applied` UI callback). `owner_state_sync::SyncEngine` becomes a thin wrapper preserving its exact public signature so `lib.rs` is untouched. Notes is a parallel instance on its own Zenoh topic with its own replay tracker, persistence file, and IPC surface.

**Tech Stack:** Rust (tokio, ciborium/CBOR, ChaCha20-Poly1305 via existing `owner_state_crypto`, `harmony_content` CIDs, Zenoh transport, Tauri IPC), TypeScript/Svelte frontend (Vitest), `cargo-nextest`.

---

## Key design decisions (resolved from the spec's deferred open items)

These were the spec's four "Open items deferred to the implementation plan." Resolved here against the real donor code:

1. **Signatures.** `FleetSyncEngine::new(FleetSyncConfig<S>)` (a config struct, not 12 positional args — avoids the donor's `#[allow(clippy::too_many_arguments)]`). `MergeOutcome { changed: bool }` (drives only the optional UI-refresh callback; **persist fires on every non-duplicate inbound** because the tracker advance is itself a mutation that must be durably recorded). `FleetPersist<S>` is a one-method trait run inside `spawn_blocking`.
2. **`seen` placement = engine-local envelope, not the shared `RootPublishPayload`.** A new `FleetRootPublish { rc, at, sn }` is introduced in `fleet_sync`. `sn` is `#[serde(rename = "sn", default, skip_serializing_if = "BTreeMap::is_empty")]`, so when empty it encodes **byte-identically** to the legacy `RootPublishPayload { rc, at }`. The shared `RootPublishPayload` (used by `mint_sync` + `community_state_sync` sig-verify) is **left untouched**. Owner-state migrates with `publish_seen = false` (keeps the wire-pin test exact); Notes uses `publish_seen = true` to exercise the indicator.
3. **`list_online_devices()` = replay-tracker keys** in SP1 (devices seen publishing), documented as the SP1 baseline; presence-based liveness refinement is SP2 work.
4. **Event-loop wiring = copy the owner-state adapter** (`event_loop.rs:757-874`) with topic `harmony/owner/{addr}/ds/notes-v1` and lookup tag `b"notes-v1"`.

**Tracker-advance ordering — important reconciliation.** The spec mandates centralizing *mint's* CRITICAL ordering: **apply → advance-tracker → persist** (apply-before-advance). The owner donor currently does the opposite (advance-before-apply, with `IncomingOutcome::ErrPre/PostMutation` classification). The generic engine adopts **mint's apply-before-advance model** (the spec's stated invariant). This is safe for owner-state: its merge is infallible and convergence is unchanged; only the transport-internal retry semantics on an *injected* CAS-fetch failure differ (advance-before would persist the advanced tracker and rely on the next publish; apply-before retries the same HLC) — and no wire or disk byte is sensitive to this. Consequence: owner's old `IncomingOutcome`-asserting unit tests are **superseded** by the engine's ordering tests; owner's **two-engine convergence tests are preserved** as the engine's regression harness.

---

## File structure

**Create:**
- `src-tauri/src/fleet_sync.rs` — the generic engine: `SyncError`, `MergeOutcome`, `FleetRootPublish`, `FleetPersist<S>`, `FleetSyncConfig<S>`, `FleetSyncEngine<S>`, `mint_next_hlc`, internal task + publish/receive paths.
- `src-tauri/src/notes_crdt.rs` — `Note`, `NotesDoc`, the LWW+tombstone merge, `CanonicalPayload` registration.
- `src-tauri/src/notes_persist.rs` — atomic CBOR load/save for `NotesDoc` + its replay tracker (mirrors `mint_sync_persist.rs`).
- `src-tauri/src/notes_commands.rs` — `notes_list` / `notes_upsert` / `notes_delete` Tauri commands.

**Modify:**
- `src-tauri/src/owner_state_sync.rs` — gut the duplicated engine; `SyncEngine` becomes a wrapper over `FleetSyncEngine<OwnerState>` (public API unchanged).
- `src-tauri/src/owner_state_types.rs` — (only if `FleetRootPublish` lives here instead of `fleet_sync`; this plan keeps it in `fleet_sync`). No change expected beyond possibly `pub use`.
- `src-tauri/src/lib.rs` — register `fleet_sync`, `notes_crdt`, `notes_persist`, `notes_commands` modules; build the Notes engine at startup; add `NodeState` notes fields; register the 3 IPC handlers; shutdown the notes engine in `stop_inner`.
- `src-tauri/src/event_loop.rs` — add `NotesAdapterHandles` + the notes Zenoh adapter task.
- `src/lib/notes-service.ts` — swap localStorage for Tauri IPC (async, contract preserved).
- `src/lib/components/NotesView.svelte` — await the now-async service methods.
- `src/App.svelte` — one-time localStorage→backend import on sync-capable launch.

**Test files:**
- Rust unit tests live in-module (`#[cfg(test)] mod tests`) in each new file, matching the donor convention.
- `src/lib/notes-service.test.ts` — rewrite for the IPC-backed service (mock `invoke`).

---

## Conventions for every task

- **Gates run from `src-tauri/`** unless noted. Per the relink-cost memory, per-task gates are `--lib`-scoped:

  ```bash
  cargo fmt --all -- --check
  cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
  cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(fleet_sync)+test(owner_state)+test(notes_)'
  ```

  Frontend gates run from repo root: `npx tsc --noEmit && npx vitest run`.
- **Commit before running the long clippy/nextest gate** (memory: implementer time-budget discipline).
- Reuse existing crypto/types verbatim — never rewrite `KeyTree`, `encrypt_entry`, `encrypt_root_publish`, `Hlc`, `ContentId`, `space_lookup_key`.

---

### Task 1: `FleetRootPublish` envelope + `MergeOutcome` + wire-compat pin

**Files:**
- Create: `src-tauri/src/fleet_sync.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod fleet_sync;` near the other `mod` declarations)
- Test: in-module `#[cfg(test)] mod tests` in `fleet_sync.rs`

Establishes the engine-local envelope and proves it is byte-identical to the legacy `RootPublishPayload` when `seen` is empty — the single most load-bearing compatibility guarantee for the owner-state migration.

- [ ] **Step 1: Write the failing wire-compat test**

In `fleet_sync.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_crypto::canonical_cbor_encode;
    use crate::owner_state_types::{Hlc, RootPublishPayload};
    use harmony_content::cid::{ContentFlags, ContentId};

    fn fixed_cid() -> ContentId {
        // 28-byte hash region is deterministic for a fixed input.
        ContentId::for_book(b"fleet-sync-pin-fixture", ContentFlags { encrypted: true, ..Default::default() })
            .expect("cid")
    }

    fn fixed_hlc() -> Hlc {
        Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: "dev-A".into() }
    }

    #[test]
    fn fleet_root_publish_with_empty_seen_is_byte_identical_to_legacy() {
        let cid = fixed_cid();
        let at = fixed_hlc();

        let legacy = RootPublishPayload { root_cid: cid, at: at.clone() };
        let legacy_bytes = canonical_cbor_encode(&legacy).expect("legacy encode");

        let fleet = FleetRootPublish { root_cid: cid, at, seen: std::collections::BTreeMap::new() };
        let fleet_bytes = canonical_cbor_encode(&fleet).expect("fleet encode");

        assert_eq!(
            fleet_bytes, legacy_bytes,
            "FleetRootPublish with empty seen MUST encode identically to legacy RootPublishPayload \
             — owner-state migration depends on this"
        );
    }

    #[test]
    fn fleet_root_publish_with_seen_round_trips() {
        use crate::owner_state_crypto::canonical_cbor_decode;
        let mut seen = std::collections::BTreeMap::new();
        seen.insert("dev-B".to_string(), fixed_hlc());
        let env = FleetRootPublish { root_cid: fixed_cid(), at: fixed_hlc(), seen };
        let bytes = canonical_cbor_encode(&env).expect("encode");
        let back: FleetRootPublish = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(back, env);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails to compile**

Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(fleet_sync)'`
Expected: FAIL — `FleetRootPublish` / `MergeOutcome` not defined.

- [ ] **Step 3: Define the envelope + outcome + register canonical**

At the top of `fleet_sync.rs`:

```rust
//! Generic per-owner replicated-dataset sync engine (ZEB-417 SP1).
//!
//! Extracted from the near-identical `owner_state_sync` / `mint_sync` engines:
//! state-root CID publish → full encrypted-blob CAS fetch → CRDT/LWW merge,
//! over a Zenoh-notified channel, gated by a device-keyed HLC replay tracker.
//! One instance per named dataset. Behavior differs only via `FleetSyncConfig`.

use std::collections::BTreeMap;

use crate::owner_state_crypto::{CanonicalPayload, sealed::CanonicalPayloadSealed};
use crate::owner_state_types::Hlc;
use harmony_content::cid::ContentId;
use serde::{Deserialize, Serialize};

/// Engine-local root-publish envelope.
///
/// Structurally a superset of the legacy `RootPublishPayload { rc, at }`: the
/// added `seen` durability vector is `skip_serializing_if` empty, so an
/// empty-`seen` envelope encodes BYTE-IDENTICALLY to the legacy payload.
/// This lets `owner_state_sync` migrate onto this engine with zero wire change
/// (it publishes with `publish_seen = false`, so `seen` is always empty).
///
/// All three keys (`rc`, `at`, `sn`) are 2 chars to satisfy
/// `canonical_cbor_encode`'s same-length-keys precondition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetRootPublish {
    #[serde(rename = "rc")]
    pub root_cid: ContentId,
    #[serde(rename = "at")]
    pub at: Hlc,
    /// Highest HLC this device has merged from each peer (incl. itself),
    /// bounded by `MAX_DEVICES_PER_OWNER`. Empty unless `publish_seen`.
    #[serde(rename = "sn", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub seen: BTreeMap<String, Hlc>,
}

impl CanonicalPayloadSealed for FleetRootPublish {}
impl CanonicalPayload for FleetRootPublish {}

/// Maximum number of sibling devices tracked in `seen` / the durability vector.
pub const MAX_DEVICES_PER_OWNER: usize = 32;

/// Result of a dataset merge. `changed` drives the optional `on_applied`
/// UI-refresh callback ONLY — persistence is independent (the engine persists
/// on every accepted inbound because advancing the replay tracker is itself a
/// durable mutation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MergeOutcome {
    pub changed: bool,
}
```

> Implementer note: confirm the exact path to the sealed canonical-payload trait by reading `owner_state_crypto.rs` (~lines 28-36 and the `impl_canonical!` macro ~1139-1175). If the codebase registers via the `impl_canonical!` macro rather than a manual `impl`, use the macro instead of the two hand-written `impl` lines above. The `RootPublishPayload` is registered there already — mirror exactly how it is done.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(fleet_sync)'`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add src-tauri/src/fleet_sync.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-417): FleetRootPublish envelope + MergeOutcome (wire-compat with legacy RootPublishPayload)"
```

---

### Task 2: Generic `FleetSyncEngine<S>` core (publish/receive/debounce/persist)

**Files:**
- Modify: `src-tauri/src/fleet_sync.rs`
- Test: in-module `#[cfg(test)] mod tests`

This is the heart. Lift the donor machinery (`owner_state_sync.rs:54-491` + `680-758`) generic over `S`, using **mint's apply-before-advance** ordering. Reuse the in-memory `ContentStore` stub for tests.

- [ ] **Step 1: Write the failing convergence + ordering tests**

Add to `fleet_sync.rs` tests. Use a trivial dataset `S = ToyDoc` (a `BTreeMap<String, (u64, String)>` value-LWW-by-counter) so the engine test is independent of any real consumer:

```rust
#[cfg(test)]
mod engine_tests {
    use super::*;
    use crate::content_store::{ContentStore, InMemoryStub};
    use crate::owner_state_crypto::KeyTree;
    use std::sync::Arc;
    use tokio::sync::{mpsc, Mutex};

    #[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct ToyDoc {
        // key -> (lww_counter, value)
        #[serde(rename = "en")]
        entries: BTreeMap<String, ToyEntry>,
    }
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct ToyEntry {
        #[serde(rename = "ct")]
        ctr: u64,
        #[serde(rename = "vl")]
        val: String,
    }
    impl CanonicalPayloadSealed for ToyDoc {}
    impl CanonicalPayload for ToyDoc {}
    impl CanonicalPayloadSealed for ToyEntry {}
    impl CanonicalPayload for ToyEntry {}

    fn toy_merge(local: &mut ToyDoc, remote: ToyDoc) -> MergeOutcome {
        let mut changed = false;
        for (k, r) in remote.entries {
            match local.entries.get(&k) {
                Some(l) if l.ctr >= r.ctr => {}
                _ => { local.entries.insert(k, r); changed = true; }
            }
        }
        MergeOutcome { changed }
    }

    struct NoopPersist;
    impl<S: Send + Sync> FleetPersist<S> for NoopPersist {
        fn persist(&self, _s: &S, _t: &BTreeMap<String, Hlc>) -> Result<(), SyncError> { Ok(()) }
    }

    fn test_kt() -> Arc<KeyTree> { Arc::new(KeyTree::derive(&[7u8; 32]).expect("kt")) }

    // Build one engine wired to a shared in-memory CAS, returning its handle +
    // the (out_tx-as-other-side, in_rx-as-other-side) for the test harness to
    // shuttle bytes between two engines.
    // ... helper omitted here; implement inline ...

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_engines_converge() {
        // A shared CAS so B can fetch the blob A put.
        let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
        // wire A.out -> B.in and B.out -> A.in
        // write on A, flush_now, shuttle bytes to B, assert B converges.
        // (full harness in implementation)
        todo!("implement two-engine harness");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blob_miss_is_dropped_and_recovered_on_next_publish() {
        todo!("B receives a publish whose blob is absent from CAS -> dropped, \
               tracker NOT advanced; A republishes newer with blob present -> converges");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_before_advance_failure_does_not_advance_tracker() {
        // Inject a CAS whose get() errors. Feed B a valid envelope.
        // Assert B's replay tracker has NO entry for A afterward.
        todo!("implement");
    }
}
```

> Implementer: replace each `todo!` with a real harness. The harness builds two engines, drives `out_rx` of each into the other's `in_tx` with a `tokio::spawn` forwarder, calls `engine.flush_now().await` after a local mutation, and polls the peer's `Arc<Mutex<ToyDoc>>` until convergence (use a bounded `tokio::time::timeout`, not a fixed sleep — memory: condition-based waiting). For the failure test, supply a `ContentStore` wrapper whose `get` returns `Err`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(fleet_sync)'`
Expected: FAIL — `FleetSyncEngine`, `FleetSyncConfig`, `FleetPersist`, `SyncError` undefined.

- [ ] **Step 3: Implement the engine**

Add to `fleet_sync.rs`. This lifts `owner_state_sync.rs` generic over `S`. Key adaptations vs the donor: (a) **apply-before-advance** ordering (mint model); (b) `merger`/`persist`/`on_applied`/`publish_seen` come from config; (c) `FleetRootPublish` replaces `RootPublishPayload`; (d) explicit echo-suppression; (e) `seen` populated from the tracker when `publish_seen`.

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;

use crate::content_store::{ContentStore, ContentStoreError};
use crate::owner_state_crypto::{
    canonical_cbor_decode, canonical_cbor_encode, decrypt_entry, decrypt_root_publish,
    encrypt_entry, encrypt_root_publish, space_lookup_key, KeyTree,
};

pub const DEFAULT_DEBOUNCE_MS: u64 = 250; // keep equal to owner_state_sync::DEFAULT_DEBOUNCE_MS

#[derive(thiserror::Error, Debug)]
pub enum SyncError {
    #[error("cbor encode: {0}")]
    CborEncode(String),
    #[error("cbor decode: {0}")]
    CborDecode(String),
    #[error("crypto: {0}")]
    Crypto(String),
    #[error("content store: {0}")]
    ContentStore(#[from] ContentStoreError),
    #[error("transport channel closed")]
    TransportClosed,
    #[error("persist: {0}")]
    Persist(String),
}

/// Per-dataset disk persistence. Called inside `spawn_blocking` with cloned
/// snapshots; the engine serializes all calls (no concurrent persist).
pub trait FleetPersist<S>: Send + Sync {
    fn persist(&self, state: &S, replay_tracker: &BTreeMap<String, Hlc>) -> Result<(), SyncError>;
}

/// All per-dataset configuration for one engine instance.
pub struct FleetSyncConfig<S> {
    pub kt: Arc<KeyTree>,
    pub device_id: String,
    pub state: Arc<Mutex<S>>,
    pub merger: Arc<dyn Fn(&mut S, S) -> MergeOutcome + Send + Sync>,
    pub replay_tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
    pub content_store: Arc<dyn ContentStore>,
    pub publisher_tx: mpsc::Sender<Vec<u8>>,
    pub subscriber_rx: mpsc::Receiver<Vec<u8>>,
    pub persist: Arc<dyn FleetPersist<S>>,
    /// Domain-separation tag for `space_lookup_key` (the CAS-blob AEAD key).
    pub lookup_key_tag: &'static [u8],
    pub debounce_ms: u64,
    /// When true, outbound publishes carry the `seen` durability vector and
    /// inbound `seen` is recorded for the "synced to N devices" indicator.
    /// owner-state sets this false to keep its wire byte-identical.
    pub publish_seen: bool,
    /// Fired after an inbound merge that actually changed local state.
    pub on_applied: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Shared sibling-ack map for the durability indicator (peer -> highest
    /// HLC-of-mine that peer reports having seen). Only written when
    /// `publish_seen`. Exposed so a consumer can read `synced_device_count`.
    pub sibling_acks: Arc<Mutex<BTreeMap<String, Hlc>>>,
}

pub struct FleetSyncEngine<S: Send + 'static> {
    notify_dirty: Arc<Notify>,
    has_pending_dirty: Arc<AtomicBool>,
    flush_now_tx: mpsc::Sender<tokio::sync::oneshot::Sender<Result<(), SyncError>>>,
    shutdown_tx: mpsc::Sender<tokio::sync::oneshot::Sender<Result<(), SyncError>>>,
    replay_tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
    sibling_acks: Arc<Mutex<BTreeMap<String, Hlc>>>,
    device_id: String,
    task: Mutex<Option<JoinHandle<()>>>,
    _s: std::marker::PhantomData<fn() -> S>,
}
```

The engine spawns an internal task carrying a `Ctx<S>` (mirrors the donor's `InternalCtx`, lines 194-209). Lift `internal_task` from donor lines **211-349 verbatim**, with these substitutions:
- `OwnerState` → `S`; `merge_remote_into_local(&mut local, remote)` → `(ctx.merger)(&mut local, remote)`;
- `persist_both(&ctx.state, &ctx.tracker, &ctx.paths)` → `persist_now(&ctx).await` (calls `ctx.persist.persist(...)` inside `spawn_blocking` with cloned snapshots — adapt donor `persist_both` lines 362-388);
- keep the **pinned `Notified`** idiom, the **`has_pending_dirty` swap/restore** on publish failure, the **`inbound_closed` latch**, and all the `tracing` logs **unchanged**.

`publish_root_now` — adapt donor lines 390-444:

```rust
async fn publish_root_now<S>(ctx: &Ctx<S>) -> Result<(), SyncError>
where S: CanonicalPayload + serde::de::DeserializeOwned + Clone + Send + 'static
{
    let snapshot = { ctx.state.lock().await.clone() };
    let blob_cleartext = canonical_cbor_encode(&snapshot)
        .map_err(|e| SyncError::CborEncode(e.to_string()))?;
    let lookup = space_lookup_key(&ctx.kt, ctx.lookup_key_tag);
    let blob_ciphertext = encrypt_entry(&ctx.kt, &lookup, &blob_cleartext)
        .map_err(|e| SyncError::Crypto(e.to_string()))?;
    let root_cid = harmony_content::cid::ContentId::for_book(
        &blob_ciphertext,
        harmony_content::cid::ContentFlags { encrypted: true, ..Default::default() },
    ).map_err(|e| SyncError::Crypto(format!("ContentId::for_book: {e}")))?;
    ctx.content_store.put(root_cid, blob_ciphertext).await?;

    let now = next_hlc(&ctx.replay_tracker, &ctx.device_id).await;
    let seen = if ctx.publish_seen {
        let t = ctx.replay_tracker.lock().await;
        // bound to MAX_DEVICES_PER_OWNER newest entries
        t.iter().take(MAX_DEVICES_PER_OWNER).map(|(k, v)| (k.clone(), v.clone())).collect()
    } else {
        BTreeMap::new()
    };
    let payload = FleetRootPublish { root_cid, at: now, seen };
    let payload_bytes = canonical_cbor_encode(&payload)
        .map_err(|e| SyncError::CborEncode(e.to_string()))?;
    let wire = encrypt_root_publish(&ctx.kt, &payload_bytes)
        .map_err(|e| SyncError::Crypto(e.to_string()))?;
    ctx.publisher_tx.send(wire).await.map_err(|_| SyncError::TransportClosed)?;
    Ok(())
}
```

`handle_incoming_publish` — **mint ordering** (read-only replay check → fetch → decrypt → decode → merge → advance tracker → record `seen`). Adapt donor lines 680-758 + mint lines 957-1070:

```rust
enum Inbound { Duplicate, Echo, Dropped, Applied(MergeOutcome) }

async fn handle_incoming_publish<S>(ctx: &Ctx<S>, wire: Vec<u8>) -> Inbound
where S: CanonicalPayload + serde::de::DeserializeOwned + Clone + Send + 'static
{
    let payload_bytes = match decrypt_root_publish(&ctx.kt, &wire) {
        Ok(b) => b, Err(e) => { tracing::warn!(error=%e, "decrypt_root_publish"); return Inbound::Dropped; }
    };
    let payload: FleetRootPublish = match canonical_cbor_decode(&payload_bytes) {
        Ok(p) => p, Err(e) => { tracing::warn!(error=%e, "envelope decode"); return Inbound::Dropped; }
    };
    // Echo-suppress our own publishes looped back through Zenoh.
    if payload.at.device_id == ctx.device_id { return Inbound::Echo; }
    // Replay check (READ-ONLY — advance only after a successful apply).
    {
        let t = ctx.replay_tracker.lock().await;
        let accept = match t.get(&payload.at.device_id) {
            None => true, Some(existing) => payload.at.is_strictly_newer_than(existing),
        };
        if !accept { return Inbound::Duplicate; }
    }
    let blob = match ctx.content_store.get(&payload.root_cid).await {
        Ok(Some(b)) => b,
        Ok(None) => { tracing::warn!(?payload.root_cid, "missing root blob; drop (eventual-consistency retry)"); return Inbound::Dropped; }
        Err(e) => { tracing::warn!(error=%e, "content_store.get"); return Inbound::Dropped; }
    };
    let lookup = space_lookup_key(&ctx.kt, ctx.lookup_key_tag);
    let cleartext = match decrypt_entry(&ctx.kt, &lookup, &blob) {
        Ok(b) => b, Err(e) => { tracing::warn!(error=%e, "decrypt_entry"); return Inbound::Dropped; }
    };
    let remote: S = match canonical_cbor_decode(&cleartext) {
        Ok(s) => s, Err(e) => { tracing::warn!(error=%e, "blob decode"); return Inbound::Dropped; }
    };
    let outcome = { let mut local = ctx.state.lock().await; (ctx.merger)(&mut local, remote) };
    // Advance tracker AFTER successful apply (mint CRITICAL 3).
    { ctx.replay_tracker.lock().await.insert(payload.at.device_id.clone(), payload.at.clone()); }
    // Durability: record what this peer reports having seen of US.
    if ctx.publish_seen {
        if let Some(seen_of_me) = payload.seen.get(&ctx.device_id) {
            let mut acks = ctx.sibling_acks.lock().await;
            let newer = acks.get(&payload.at.device_id).is_none_or(|e| seen_of_me.is_strictly_newer_than(e));
            if newer { acks.insert(payload.at.device_id.clone(), seen_of_me.clone()); }
        }
    }
    Inbound::Applied(outcome)
}
```

The internal task persists on `Inbound::Applied(_)` (and fires `on_applied` when `outcome.changed`). `next_hlc` — lift donor lines 452-491 verbatim into a free fn `next_hlc(tracker, device_id)` (this is also reused by Notes IPC, so make it `pub async fn mint_next_hlc`). `flush_now`/`shutdown`/`notify_dirty` — lift donor lines 118-191 onto `FleetSyncEngine`. Add the public accessors:

```rust
impl<S: Send + 'static> FleetSyncEngine<S> {
    pub fn notify_dirty(&self) { self.has_pending_dirty.store(true, Ordering::Release); self.notify_dirty.notify_one(); }
    pub async fn flush_now(&self) -> Result<(), SyncError> { /* donor 130-144 */ }
    pub async fn shutdown(&self) -> Result<(), SyncError> { /* donor 146-191 */ }

    /// SP1↔SP2 seam: best-effort fleet presence (devices seen publishing).
    /// SP2 refines "online" with liveness/presence.
    pub async fn list_online_devices(&self) -> Vec<String> {
        self.replay_tracker.lock().await.keys()
            .filter(|d| *d != &self.device_id).cloned().collect()
    }

    /// Best-effort "synced to N devices": peers whose reported view of us has
    /// caught up to our latest publish.
    pub async fn synced_device_count(&self) -> usize {
        let my_latest = { self.replay_tracker.lock().await.get(&self.device_id).cloned() };
        let Some(my_latest) = my_latest else { return 0 };
        let acks = self.sibling_acks.lock().await;
        acks.values().filter(|seen_of_me| {
            seen_of_me.wall_ms > my_latest.wall_ms
                || (**seen_of_me == my_latest)
                || seen_of_me.is_strictly_newer_than(&my_latest)
        }).count()
    }
}
```

> Implementer: the `synced_device_count` predicate is "peer's seen-of-me ≥ my latest". Express `≥` cleanly as `!my_latest.is_strictly_newer_than(seen_of_me)` and unit-test both boundaries. Replace the rough disjunction above with that single clean comparison.

`pub async fn mint_next_hlc(tracker: &Arc<Mutex<BTreeMap<String, Hlc>>>, device_id: &str) -> Hlc` — exact body of donor `next_hlc` (lines 452-491).

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(fleet_sync)'`
Expected: PASS (envelope tests + convergence + blob-miss + ordering).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add src-tauri/src/fleet_sync.rs
git commit -m "feat(zeb-417): generic FleetSyncEngine<S> core (apply-before-advance, FleetPersist, durability seam)"
```

---

### Task 3: Durability indicator end-to-end test

**Files:**
- Modify: `src-tauri/src/fleet_sync.rs` (tests only — mechanism landed in Task 2)
- Test: in-module

- [ ] **Step 1: Write the failing N-instance durability test**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn synced_device_count_reflects_fan_out() {
    // 3 engines (publish_seen = true) sharing a CAS, fully meshed.
    // A writes + flushes; shuttle until quiescent.
    // After B and C have merged A and republished their seen vectors back,
    // assert A.synced_device_count() == 2.
    // A single isolated engine reports 0 (not yet backed up).
    todo!("implement 3-engine mesh harness reusing Task 2 helpers");
}
```

- [ ] **Step 2: Run to verify failure.** Expected FAIL (assertion / harness).
- [ ] **Step 3: Implement the mesh harness** (reuse the Task 2 forwarder; wire 3 engines pairwise; pump bytes with bounded `timeout` until `synced_device_count` stabilizes).
- [ ] **Step 4: Run to verify pass.**
- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/fleet_sync.rs
git commit -m "test(zeb-417): durability indicator reports correct synced-device count across fan-out"
```

---

### Task 4: Migrate `owner_state_sync` onto `FleetSyncEngine<OwnerState>`

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs`
- Test: in-module (keep the two-engine convergence tests; add a wire/disk pin test)

`SyncEngine` becomes a thin wrapper; **its public `new`/`flush_now`/`shutdown`/`notify_dirty` signatures stay identical** so `lib.rs:2990-3003` is untouched.

- [ ] **Step 1: Write the failing wire/disk pin test**

Add to `owner_state_sync.rs` tests — a fixed `OwnerState` must produce a `FleetRootPublish` whose encoded bytes equal a pinned fixture AND equal the legacy `RootPublishPayload`-based encoding (proving the migration changed no on-wire byte):

```rust
#[tokio::test]
async fn owner_state_publish_wire_is_byte_identical_post_migration() {
    // Build a deterministic OwnerState fixture, run it through the engine's
    // publish path with a deterministic HLC + a fixed CAS, capture the wire
    // bytes sent on publisher_tx, decrypt_root_publish them, and assert the
    // decoded FleetRootPublish has empty `seen` and re-encodes to the same
    // bytes as the legacy RootPublishPayload { root_cid, at }.
    todo!("implement using a captured publisher_tx channel");
}
```

- [ ] **Step 2: Run to verify failure.** Expected FAIL.

- [ ] **Step 3: Rewrite `SyncEngine` as a wrapper + delete duplicated machinery**

- Add `use crate::fleet_sync::{FleetSyncEngine, FleetSyncConfig, FleetPersist, MergeOutcome, SyncError, DEFAULT_DEBOUNCE_MS};` and `pub use crate::fleet_sync::SyncError;` (re-export so existing `owner_state_sync::SyncError` references compile).
- **Delete** from `owner_state_sync.rs`: `internal_task`, `InternalCtx`, `publish_root_now`, `handle_incoming_publish`, `next_hlc`, `persist_both`, `IncomingOutcome` and its helpers, `OWNER_STATE_ROOT_BLOB_TAG` const stays. **Keep**: `PersistPaths`, `merge_remote_into_local` (now the merger), `DEFAULT_DEBOUNCE_MS` (re-export from fleet_sync or keep equal).
- Implement the `FleetPersist<OwnerState>` impl:

```rust
struct OwnerStatePersist { paths: PersistPaths }
impl FleetPersist<OwnerState> for OwnerStatePersist {
    fn persist(&self, state: &OwnerState, tracker: &BTreeMap<String, Hlc>) -> Result<(), SyncError> {
        crate::owner_state_persist::save_crdt(&self.paths.crdt, state)
            .map_err(|e| SyncError::Persist(e.to_string()))?;
        crate::owner_state_persist::save_replay(&self.paths.replay, tracker)
            .map_err(|e| SyncError::Persist(e.to_string()))?;
        Ok(())
    }
}
```

- Rewrite `SyncEngine`:

```rust
pub struct SyncEngine { inner: FleetSyncEngine<OwnerState> }

impl SyncEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kt: Arc<KeyTree>, device_id: String,
        state: Arc<Mutex<OwnerState>>,
        tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
        content_store: Arc<dyn ContentStore>,
        publisher_tx: mpsc::Sender<Vec<u8>>,
        subscriber_rx: mpsc::Receiver<Vec<u8>>,
        paths: PersistPaths,
        debounce_ms: u64,
    ) -> Self {
        let merger: Arc<dyn Fn(&mut OwnerState, OwnerState) -> MergeOutcome + Send + Sync> =
            Arc::new(|local, remote| { merge_remote_into_local(local, remote); MergeOutcome { changed: true } });
        let inner = FleetSyncEngine::new(FleetSyncConfig {
            kt, device_id, state, merger,
            replay_tracker: tracker,
            content_store, publisher_tx, subscriber_rx,
            persist: Arc::new(OwnerStatePersist { paths }),
            lookup_key_tag: OWNER_STATE_ROOT_BLOB_TAG,
            debounce_ms,
            publish_seen: false,          // wire-identical: owner-state carries no `seen`
            on_applied: None,             // owner-state emits no inbound UI event (unchanged)
            sibling_acks: Arc::new(Mutex::new(BTreeMap::new())),
        });
        SyncEngine { inner }
    }
    pub fn notify_dirty(&self) { self.inner.notify_dirty(); }
    pub async fn flush_now(&self) -> Result<(), SyncError> { self.inner.flush_now().await }
    pub async fn shutdown(&self) -> Result<(), SyncError> { self.inner.shutdown().await }
}
```

- Remove the donor unit tests that asserted `IncomingOutcome::ErrPreMutation/ErrPostMutation/Duplicate` internals (superseded by `fleet_sync`'s ordering tests — see the ordering reconciliation note). **Keep** every two-engine convergence test (the regression harness) — adapt only the construction call if its signature drifted (it shouldn't).

- [ ] **Step 4: Run the owner-state suite to verify green**

Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(owner_state)+test(fleet_sync)'`
Expected: PASS — every retained owner-state convergence test + the new wire-pin test.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add src-tauri/src/owner_state_sync.rs
git commit -m "refactor(zeb-417): migrate owner_state_sync onto FleetSyncEngine (wire/disk byte-identical)"
```

---

### Task 5: `NotesDoc` CRDT model + merge

**Files:**
- Create: `src-tauri/src/notes_crdt.rs`
- Modify: `src-tauri/src/lib.rs` (`mod notes_crdt;`)
- Test: in-module

- [ ] **Step 1: Write failing merge tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::Hlc;
    fn hlc(w: u64, d: &str) -> Hlc { Hlc { wall_ms: w, logical: 0, device_id: d.into() } }

    #[test]
    fn lww_newer_update_wins() {
        let mut a = NotesDoc::default();
        let id = "n1".to_string();
        a.upsert(id.clone(), "old".into(), hlc(1, "A"));
        let mut b = a.clone();
        b.upsert(id.clone(), "new".into(), hlc(2, "B"));
        let out = a.merge_from(b);
        assert!(out.changed);
        assert_eq!(a.get(&id).unwrap().text, "new");
    }

    #[test]
    fn delete_tombstone_propagates_and_hides() {
        let mut a = NotesDoc::default();
        a.upsert("n1".into(), "hi".into(), hlc(1, "A"));
        let mut b = a.clone();
        b.delete("n1", hlc(2, "B"));
        a.merge_from(b);
        assert!(a.get("n1").is_none(), "deleted note hidden from list()");
        assert!(a.notes.get("n1").unwrap().deleted_at.is_some(), "tombstone retained for convergence");
    }

    #[test]
    fn concurrent_edit_converges_deterministically() {
        // same id edited on A@hlc(2,"A") and B@hlc(2,"B"); HLC tiebreak by device_id.
        let mut a = NotesDoc::default(); a.upsert("n1".into(), "fromA".into(), hlc(2, "A"));
        let mut b = NotesDoc::default(); b.upsert("n1".into(), "fromB".into(), hlc(2, "B"));
        let mut a2 = a.clone(); a2.merge_from(b.clone());
        let mut b2 = b.clone(); b2.merge_from(a.clone());
        assert_eq!(a2.get("n1").unwrap().text, b2.get("n1").unwrap().text, "convergent");
    }

    #[test]
    fn stale_update_is_ignored() {
        let mut a = NotesDoc::default();
        a.upsert("n1".into(), "new".into(), hlc(5, "A"));
        let mut b = NotesDoc::default(); b.upsert("n1".into(), "old".into(), hlc(1, "B"));
        let out = a.merge_from(b);
        assert!(!out.changed);
        assert_eq!(a.get("n1").unwrap().text, "new");
    }
}
```

- [ ] **Step 2: Run to verify failure.** Expected FAIL — `NotesDoc` undefined.

- [ ] **Step 3: Implement `NotesDoc`**

```rust
//! Owner-private Notes CRDT (ZEB-361 / ZEB-417). LWW-element-set of notes,
//! per-id LWW on `updated_at`, delete = tombstone via `deleted_at`. Mirrors
//! mint's proven shape. Plain text only in v1.

use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};
use crate::owner_state_crypto::{CanonicalPayload, sealed::CanonicalPayloadSealed};
use crate::owner_state_types::Hlc;
use crate::fleet_sync::MergeOutcome;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    #[serde(rename = "id")]
    pub id: String,                     // ULID (time-sortable, device-unique)
    #[serde(rename = "tx")]
    pub text: String,
    #[serde(rename = "ca")]
    pub created_at: Hlc,
    #[serde(rename = "ua")]
    pub updated_at: Hlc,
    #[serde(rename = "da", default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<Hlc>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotesDoc {
    #[serde(rename = "no")]
    pub notes: BTreeMap<String, Note>,
}

impl CanonicalPayloadSealed for Note {}
impl CanonicalPayload for Note {}
impl CanonicalPayloadSealed for NotesDoc {}
impl CanonicalPayload for NotesDoc {}

impl NotesDoc {
    /// Live (non-tombstoned) note by id.
    pub fn get(&self, id: &str) -> Option<&Note> {
        self.notes.get(id).filter(|n| n.deleted_at.is_none())
    }

    /// Live notes, oldest-first by id (ULID == creation order).
    pub fn list(&self) -> Vec<&Note> {
        self.notes.values().filter(|n| n.deleted_at.is_none()).collect()
    }

    /// Insert or update. Caller supplies a freshly minted HLC (monotone via the
    /// shared replay tracker). For a brand-new id, `created_at == updated_at`.
    pub fn upsert(&mut self, id: String, text: String, at: Hlc) {
        match self.notes.get_mut(&id) {
            Some(n) if at.is_strictly_newer_than(&n.updated_at) => {
                n.text = text; n.updated_at = at; n.deleted_at = None;
            }
            Some(_) => {} // stale, ignore
            None => {
                self.notes.insert(id.clone(), Note {
                    id, text, created_at: at.clone(), updated_at: at, deleted_at: None,
                });
            }
        }
    }

    /// Tombstone a note (LWW on the delete HLC).
    pub fn delete(&mut self, id: &str, at: Hlc) {
        if let Some(n) = self.notes.get_mut(id) {
            if at.is_strictly_newer_than(&n.updated_at) {
                n.updated_at = at.clone(); n.deleted_at = Some(at);
            }
        }
    }

    /// Merge a remote doc, per-id LWW on `updated_at`. Returns whether anything
    /// observable changed (drives the UI-refresh callback).
    pub fn merge_from(&mut self, remote: NotesDoc) -> MergeOutcome {
        let mut changed = false;
        for (id, r) in remote.notes {
            match self.notes.get(&id) {
                Some(l) if !r.updated_at.is_strictly_newer_than(&l.updated_at) => {}
                _ => {
                    let was_live = self.notes.get(&id).map(|n| n.deleted_at.is_none());
                    let now_live = r.deleted_at.is_none();
                    if was_live != Some(now_live) || was_live.is_none()
                        || self.notes.get(&id).map(|n| n.text != r.text).unwrap_or(true) {
                        changed = true;
                    }
                    self.notes.insert(id, r);
                }
            }
        }
        MergeOutcome { changed }
    }
}
```

> Implementer: register `Note` + `NotesDoc` for `CanonicalPayload` exactly as `RootPublishPayload`/`Space` are registered in `owner_state_crypto.rs` (macro vs manual `impl` — match the file). All field renames are 2 chars (`id`,`tx`,`ca`,`ua`,`da`,`no`) to satisfy the same-length-keys precondition; add a CBOR-shape pin test for `Note` like `hlc_cbor_uses_single_char_field_names...` if the file pins other types.

- [ ] **Step 4: Run to verify pass.** `-E 'test(notes_crdt)'`
- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add src-tauri/src/notes_crdt.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-417): NotesDoc LWW-element-set CRDT (per-id LWW + tombstone)"
```

---

### Task 6: `notes_persist` (atomic CBOR load/save)

**Files:**
- Create: `src-tauri/src/notes_persist.rs`
- Modify: `src-tauri/src/lib.rs` (`mod notes_persist;`)
- Test: in-module (tempfile round-trip)

Mirror `mint_sync_persist.rs` (atomic-rename + fsync) for both the `NotesDoc` and its replay tracker. **Match `owner_state_persist`'s at-rest treatment** (encrypted-or-plaintext — read it and follow it; do not invent a new at-rest scheme).

- [ ] **Step 1: Write failing round-trip test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::Hlc;

    #[test]
    fn doc_round_trips_and_missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.cbor");
        assert_eq!(load(&path).unwrap(), crate::notes_crdt::NotesDoc::default());
        let mut doc = crate::notes_crdt::NotesDoc::default();
        doc.upsert("n1".into(), "hi".into(), Hlc { wall_ms: 1, logical: 0, device_id: "A".into() });
        save(&path, &doc).unwrap();
        assert_eq!(load(&path).unwrap(), doc);
    }

    #[test]
    fn replay_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes_replay.cbor");
        let mut t = std::collections::BTreeMap::new();
        t.insert("A".to_string(), Hlc { wall_ms: 9, logical: 1, device_id: "A".into() });
        save_replay(&path, &t).unwrap();
        assert_eq!(load_replay(&path).unwrap(), t);
    }
}
```

- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement** `load`/`save`/`load_replay`/`save_replay` + `NOTES_FILENAME`/`NOTES_REPLAY_FILENAME` consts, copying `mint_sync_persist.rs:1-57` structure (tempfile `persist`, `sync_all`, NotFound→default). Error type: reuse `fleet_sync::SyncError` (`Persist(String)`) or a small local `thiserror` enum mapped into it — match how `owner_state_persist` exposes its error.
- [ ] **Step 4: Run to verify pass.** `-E 'test(notes_persist)'`
- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/notes_persist.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-417): notes_persist atomic CBOR load/save (mirrors mint_sync_persist)"
```

---

### Task 7: Notes IPC commands + `NodeState` wiring + handler registration

**Files:**
- Create: `src-tauri/src/notes_commands.rs`
- Modify: `src-tauri/src/lib.rs` (`NodeState` fields; `mod notes_commands;`; `tauri::generate_handler![...]` registration)
- Test: in-module (lock/mutate/notify round-trip with a stub engine)

- [ ] **Step 1: Write failing command tests**

Tests construct a `NotesDoc` + tracker, call the command bodies (factored so the core logic is testable without a live Tauri `State`), assert insert/update/delete + HLC monotonicity. Mirror the test approach used by `mint::*` command tests.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    // upsert with no id mints a ULID + monotone HLC; list reflects it; delete tombstones.
    #[tokio::test]
    async fn upsert_then_list_then_delete() {
        todo!("call notes_upsert_core / notes_list_core / notes_delete_core against in-memory state");
    }
}
```

- [ ] **Step 2: Run to verify failure.**

- [ ] **Step 3: Implement the commands**

Define DTOs + a testable core, then thin `#[tauri::command]` wrappers. HLC minted via `fleet_sync::mint_next_hlc` against the **notes** tracker (separate from owner-state's). ULID via the `ulid` crate (add to `Cargo.toml` if absent; else generate a lexicographically-sortable id from `wall_ms` + random suffix).

```rust
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::owner_state_types::Hlc;
use crate::notes_crdt::NotesDoc;

#[derive(Serialize)]
pub struct NoteView { pub id: String, pub text: String, pub timestamp: u64 }

fn to_view(n: &crate::notes_crdt::Note) -> NoteView {
    NoteView { id: n.id.clone(), text: n.text.clone(), timestamp: n.updated_at.wall_ms }
}

// Testable cores (no Tauri State) ---------------------------------------------
pub(crate) async fn notes_list_core(doc: &Arc<Mutex<NotesDoc>>) -> Vec<NoteView> {
    doc.lock().await.list().into_iter().map(to_view).collect()
}

pub(crate) async fn notes_upsert_core(
    doc: &Arc<Mutex<NotesDoc>>,
    tracker: &Arc<Mutex<BTreeMap<String, Hlc>>>,
    device_id: &str,
    id: Option<String>,
    text: String,
) -> Result<NoteView, String> {
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() { return Err("note text is empty".into()); }
    let at = crate::fleet_sync::mint_next_hlc(tracker, device_id).await;
    let id = id.unwrap_or_else(|| new_ulid(at.wall_ms));
    let mut d = doc.lock().await;
    d.upsert(id.clone(), trimmed, at);
    Ok(to_view(d.get(&id).expect("just upserted")))
}

pub(crate) async fn notes_delete_core(
    doc: &Arc<Mutex<NotesDoc>>,
    tracker: &Arc<Mutex<BTreeMap<String, Hlc>>>,
    device_id: &str,
    id: String,
) -> Result<(), String> {
    let at = crate::fleet_sync::mint_next_hlc(tracker, device_id).await;
    doc.lock().await.delete(&id, at);
    Ok(())
}

fn new_ulid(wall_ms: u64) -> String { /* ulid::Ulid::from_parts(wall_ms, rand) OR crate ulid */ todo!() }

// Tauri wrappers --------------------------------------------------------------
// Each: snapshot (notes_doc, notes_tracker, notes_sync, device_id) from NodeState
// under the std::sync::Mutex guard, DROP the guard before await, call the core,
// then `notes_sync.notify_dirty()` on a successful mutation. Mirror send_dm
// (lib.rs:6589-6701) for the snapshot-then-drop pattern.
#[tauri::command]
pub async fn notes_list(state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>) -> Result<Vec<NoteView>, String> { /* ... */ todo!() }
#[tauri::command]
pub async fn notes_upsert(state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>, id: Option<String>, text: String) -> Result<NoteView, String> { /* ... */ todo!() }
#[tauri::command]
pub async fn notes_delete(state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>, id: String) -> Result<(), String> { /* ... */ todo!() }
```

Add to `NodeState` (lib.rs:493-866 region), mirroring the mint fields:

```rust
pub notes_doc: Option<std::sync::Arc<tokio::sync::Mutex<crate::notes_crdt::NotesDoc>>>,
pub notes_tracker: Option<std::sync::Arc<tokio::sync::Mutex<std::collections::BTreeMap<String, crate::owner_state_types::Hlc>>>>,
pub notes_sync: Option<std::sync::Arc<crate::fleet_sync::FleetSyncEngine<crate::notes_crdt::NotesDoc>>>,
pub notes_device_id: Option<String>,
```

Register in `tauri::generate_handler![...]` (lib.rs:37689+): `notes_commands::notes_list, notes_commands::notes_upsert, notes_commands::notes_delete`.

> Implementer: replace the `todo!`s. For `new_ulid`, prefer the `ulid` crate if already a dependency (`grep ulid src-tauri/Cargo.toml`); otherwise emit `format!("{wall_ms:013}-{:08x}", rand)` is NOT acceptable (not collision-safe across devices) — add the `ulid` crate. ULID's 80-bit randomness makes cross-device offline-create collisions negligible.

- [ ] **Step 4: Run to verify pass.** `-E 'test(notes_commands)+test(notes_)'`
- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add src-tauri/src/notes_commands.rs src-tauri/src/lib.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat(zeb-417): notes IPC (list/upsert/delete) + NodeState fields + ULID ids"
```

---

### Task 8: Notes engine startup wiring + event-loop Zenoh adapter

**Files:**
- Modify: `src-tauri/src/lib.rs` (build the engine at startup near owner/mint setup ~2916-3166; store handles; shutdown in `stop_inner`)
- Modify: `src-tauri/src/event_loop.rs` (add `NotesAdapterHandles` + adapter task, mirroring owner-state `event_loop.rs:757-874`)
- Test: in-module wiring smoke (construct the engine via a small factory + assert it publishes on a captured channel)

- [ ] **Step 1: Write a failing engine-construction test**

Factor the engine build into a testable helper `build_notes_engine(kt, device_id, doc, tracker, cas, out_tx, in_rx) -> FleetSyncEngine<NotesDoc>` and test that a local upsert + `flush_now` emits one wire frame on `out_tx`.

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notes_engine_publishes_on_local_write() {
    todo!("build_notes_engine, upsert via notes_upsert_core, flush_now, assert out_rx got 1 frame");
}
```

- [ ] **Step 2: Run to verify failure.**

- [ ] **Step 3: Implement wiring**

In `lib.rs`, after the mint setup, add (mirroring lib.rs:3054-3166):

```rust
let notes_path = identity_dir.join(crate::notes_persist::NOTES_FILENAME);
let notes_replay_path = identity_dir.join(crate::notes_persist::NOTES_REPLAY_FILENAME);
let notes_doc = std::sync::Arc::new(tokio::sync::Mutex::new(
    crate::notes_persist::load(&notes_path).unwrap_or_default()));
let notes_tracker = std::sync::Arc::new(tokio::sync::Mutex::new(
    crate::notes_persist::load_replay(&notes_replay_path).unwrap_or_default()));
let (notes_out_tx, notes_out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
let (notes_in_tx, notes_in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

let notes_merger: std::sync::Arc<dyn Fn(&mut crate::notes_crdt::NotesDoc, crate::notes_crdt::NotesDoc) -> crate::fleet_sync::MergeOutcome + Send + Sync> =
    std::sync::Arc::new(|local, remote| local.merge_from(remote));
let notes_app = app.clone();
let notes_sync = std::sync::Arc::new(crate::fleet_sync::FleetSyncEngine::new(crate::fleet_sync::FleetSyncConfig {
    kt: std::sync::Arc::clone(&kt),
    device_id: device_id.clone(),
    state: std::sync::Arc::clone(&notes_doc),
    merger: notes_merger,
    replay_tracker: std::sync::Arc::clone(&notes_tracker),
    content_store: std::sync::Arc::clone(&content_store),
    publisher_tx: notes_out_tx,
    subscriber_rx: notes_in_rx,
    persist: std::sync::Arc::new(crate::notes_persist::NotesPersist {
        doc_path: notes_path, replay_path: notes_replay_path }),
    lookup_key_tag: b"notes-v1",
    debounce_ms: crate::fleet_sync::DEFAULT_DEBOUNCE_MS,
    publish_seen: true,
    on_applied: Some(std::sync::Arc::new(move || { let _ = notes_app.emit("notes-changed", ()); })),
    sibling_acks: std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::BTreeMap::new())),
}));
notes_sync_handles_opt = Some(crate::event_loop::NotesAdapterHandles {
    addr_hex: owner_addr_hex.clone(),
    outbound_rx: notes_out_rx,
    inbound_tx: notes_in_tx,
});
// stored into NodeState alongside mint (lib.rs:5448 region):
//   guard.notes_doc = Some(notes_doc); guard.notes_tracker = Some(notes_tracker);
//   guard.notes_sync = Some(notes_sync); guard.notes_device_id = Some(device_id);
```

Add `NotesPersist { doc_path, replay_path }` implementing `FleetPersist<NotesDoc>` in `notes_persist.rs`.

In `stop_inner`, call `notes_sync.shutdown().await` before joining the event-loop thread (mirror the owner/mint shutdown).

In `event_loop.rs`, add (mirror lines 757-874):

```rust
pub struct NotesAdapterHandles { pub addr_hex: String, pub outbound_rx: mpsc::Receiver<Vec<u8>>, pub inbound_tx: mpsc::Sender<Vec<u8>> }

// inside run(), after the mint adapter:
if let Some(handles) = notes_handles.take() {
    let topic = format!("harmony/owner/{}/ds/notes-v1", handles.addr_hex);
    let key_expr = /* declare key_expr from topic, as owner-state does */;
    // outbound drain -> session.put(topic, bytes)   [copy 784-797]
    // inbound declare_subscriber -> inbound_tx.send  [copy 799-848, with the mint backoff-resubscribe at 909-980 preferred]
}
```

Thread `notes_sync_handles_opt` into the event-loop `run(...)` call exactly as `mint_sync_handles_opt` is threaded.

> Implementer: this task touches the large `lib.rs` + `event_loop.rs`. Keep changes mechanical and pattern-matched to the mint adapter (which has the superior backoff-resubscribe). Prefer the mint inbound loop (event_loop.rs:909-980) over the owner-state one for resilience.

- [ ] **Step 4: Run to verify pass + the broader lib gate**

Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(notes_)+test(fleet_sync)+test(owner_state)'`
Expected: PASS. (Event-loop Zenoh paths are exercised by manual smoke + the unit convergence tests; no Zenoh in unit tests.)

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add src-tauri/src/lib.rs src-tauri/src/event_loop.rs src-tauri/src/notes_persist.rs
git commit -m "feat(zeb-417): wire Notes FleetSyncEngine at startup + notes Zenoh adapter"
```

---

### Task 9: Frontend — IPC-backed `NotesService`

**Files:**
- Modify: `src/lib/notes-service.ts`
- Modify: `src/lib/components/NotesView.svelte` (await async methods)
- Test: rewrite `src/lib/notes-service.test.ts` (mock `invoke`)

Preserve the consumer contract: `getEntries(ownerId)`, `append(ownerId, text)`, `load(ownerId)`, `entries`, `onChange` — now async.

- [ ] **Step 1: Write failing vitest against mocked `invoke`**

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
import { invoke } from '@tauri-apps/api/core';
import { NotesService } from './notes-service';

describe('NotesService — IPC-backed (ZEB-417)', () => {
  beforeEach(() => vi.mocked(invoke).mockReset());

  it('append calls notes_upsert and returns the created entry', async () => {
    vi.mocked(invoke).mockResolvedValueOnce({ id: 'n1', text: 'hi', timestamp: 5 });
    const svc = new NotesService();
    const entry = await svc.append('owner-abc', '  hi  ');
    expect(invoke).toHaveBeenCalledWith('notes_upsert', { text: '  hi  ', id: undefined });
    expect(entry).toEqual({ id: 'n1', text: 'hi', timestamp: 5 });
  });

  it('getEntries calls notes_list', async () => {
    vi.mocked(invoke).mockResolvedValueOnce([{ id: 'n1', text: 'hi', timestamp: 5 }]);
    const svc = new NotesService();
    expect(await svc.getEntries('owner-abc')).toHaveLength(1);
    expect(invoke).toHaveBeenCalledWith('notes_list', {});
  });

  it('append rejects blank text without calling invoke', async () => {
    const svc = new NotesService();
    expect(await svc.append('owner-abc', '   ')).toBeNull();
    expect(invoke).not.toHaveBeenCalled();
  });
});
```

- [ ] **Step 2: Run to verify failure.** `npx vitest run src/lib/notes-service.test.ts`

- [ ] **Step 3: Rewrite `notes-service.ts`**

```typescript
import { invoke } from '@tauri-apps/api/core';

export interface NoteEntry { id: string; text: string; timestamp: number; }

/** IPC-backed self-notes store (ZEB-417). Notes live in the Rust `NotesDoc`
 *  dataset, synced across the owner's devices. The `ownerId` arg is retained
 *  for call-site compatibility but the backend keys notes by the active owner. */
export class NotesService {
  entries: NoteEntry[] = [];
  onChange?: () => void;

  async getEntries(_ownerId: string): Promise<NoteEntry[]> {
    try { return await invoke<NoteEntry[]>('notes_list', {}); }
    catch { return []; }
  }

  async load(ownerId: string): Promise<void> {
    this.entries = await this.getEntries(ownerId);
    this.onChange?.();
  }

  async append(_ownerId: string, text: string): Promise<NoteEntry | null> {
    if (!text.trim()) return null;
    try {
      const entry = await invoke<NoteEntry>('notes_upsert', { text, id: undefined });
      this.entries = [...this.entries, entry];
      this.onChange?.();
      return entry;
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      console.error('notes_upsert failed:', msg);
      return null;
    }
  }
}
```

Update `NotesView.svelte` (lines ~30, 41): `await notesService.getEntries(ownerId)` on mount/refresh and `await notesService.append(...)`. The component should subscribe to the `notes-changed` Tauri event to re-`load` (so inbound syncs refresh the list) — add a `listen('notes-changed', () => notesService.load(ownerId))` in `onMount` (import `listen` from `@tauri-apps/api/event`), mirroring how other views listen for backend events.

- [ ] **Step 4: Run to verify pass.** `npx vitest run src/lib/notes-service.test.ts && npx tsc --noEmit`
- [ ] **Step 5: Commit**

```bash
git add src/lib/notes-service.ts src/lib/notes-service.test.ts src/lib/components/NotesView.svelte
git commit -m "feat(zeb-417): IPC-backed NotesService + notes-changed live refresh"
```

---

### Task 10: One-time `localStorage` → backend import

**Files:**
- Modify: `src/App.svelte` (import on sync-capable launch, keyed by `selfOwnerId`)
- Test: vitest for the importer (extract the import into a small pure-ish function `migrateLocalNotes(ownerId, invokeFn)` so it is unit-testable)

- [ ] **Step 1: Write failing vitest for `migrateLocalNotes`**

```typescript
it('imports legacy localStorage notes once, idempotently', async () => {
  localStorage.setItem('harmony-notes:owner-abc', JSON.stringify([{ id: 'a', text: 'one', timestamp: 1 }]));
  const inv = vi.fn().mockResolvedValue({ id: 'a', text: 'one', timestamp: 1 });
  await migrateLocalNotes('owner-abc', inv);
  expect(inv).toHaveBeenCalledWith('notes_upsert', { text: 'one', id: undefined });
  expect(localStorage.getItem('harmony-notes-migrated:owner-abc')).toBe('1');
  inv.mockClear();
  await migrateLocalNotes('owner-abc', inv); // second run is a no-op
  expect(inv).not.toHaveBeenCalled();
});
```

- [ ] **Step 2: Run to verify failure.**

- [ ] **Step 3: Implement `migrateLocalNotes`** (in `src/lib/notes-migrate.ts`):

```typescript
export async function migrateLocalNotes(
  ownerId: string,
  invokeFn: (cmd: string, args: Record<string, unknown>) => Promise<unknown>,
): Promise<void> {
  if (!ownerId) return;
  const doneKey = `harmony-notes-migrated:${ownerId}`;
  if (localStorage.getItem(doneKey) === '1') return;
  let legacy: Array<{ text: string }> = [];
  try {
    const raw = localStorage.getItem(`harmony-notes:${ownerId}`);
    legacy = raw ? (JSON.parse(raw) as Array<{ text: string }>).filter(e => e && typeof e.text === 'string') : [];
  } catch { legacy = []; }
  for (const e of legacy) {
    if (e.text.trim()) await invokeFn('notes_upsert', { text: e.text, id: undefined });
  }
  localStorage.setItem(doneKey, '1'); // keep the original key as a safety copy
}
```

Call it from `App.svelte` once `selfOwnerId` is set (after `get_owner_state` / `mint_owner_identity`, lines ~786 / ~1399), passing the real `invoke`. Then `notesService.load(selfOwnerId)`.

- [ ] **Step 4: Run to verify pass.** `npx vitest run src/lib/notes-migrate.test.ts && npx tsc --noEmit`
- [ ] **Step 5: Commit**

```bash
git add src/lib/notes-migrate.ts src/lib/notes-migrate.test.ts src/App.svelte
git commit -m "feat(zeb-417): one-time idempotent localStorage->backend notes import"
```

---

### Task 11: SP1↔SP2 seam documentation + `list_online_devices` test

**Files:**
- Modify: `src-tauri/src/fleet_sync.rs` (doc-comment the seam; the method landed in Task 2)
- Test: in-module

- [ ] **Step 1: Write failing test**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_online_devices_returns_seen_peers_excluding_self() {
    // After B's publish is merged by A, A.list_online_devices() == ["B"].
    todo!("reuse Task 2 harness");
}
```

- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3:** implement the test; add a module-level doc block describing the seam:

```rust
//! ## SP1↔SP2 seam (the Butler, ZEB-418)
//! SP2 deposits via exactly two operations, never reaching into replication:
//!   * `write(dataset, op)` — a consumer mutates the dataset's `S` under its
//!     lock, then calls `engine.notify_dirty()` to schedule the debounced
//!     publish/fan-out. (Notes' IPC commands are the reference `write` site.)
//!   * `list_online_devices()` — the butler-set source. SP1 derives it from the
//!     replay tracker (devices seen publishing); SP2 refines liveness/presence.
```

- [ ] **Step 4: Run to verify pass.**
- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/fleet_sync.rs
git commit -m "docs(zeb-417): document SP1<->SP2 seam; test list_online_devices"
```

---

### Task 12: Final full-sweep gates

**Files:** none (verification + any fixups surfaced)

- [ ] **Step 1: Rust full sweep (`--all-targets`)**

From `src-tauri/`:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: clean. Fix any integration-target breakage (e.g. a test file constructing the old `owner_state_sync` internals). Per memory: `--all-targets` is load-bearing — lib-only green can hide integration breakage.

- [ ] **Step 2: MSRV check**

```bash
cargo check --locked --all-targets --features test-fixtures   # with the declared MSRV toolchain
```

- [ ] **Step 3: Frontend sweep (repo root)**

```bash
npx tsc --noEmit
npx vitest run
```

- [ ] **Step 4: Commit any fixups**

```bash
cargo fmt --all
git add -A
git commit -m "chore(zeb-417): final-sweep fixups (all-targets clippy/nextest/MSRV/frontend green)"
```

---

## Self-review checklist (run after drafting; fix inline)

- **Spec coverage:** ✅ generic engine (T1-T3); ✅ owner migration wire/disk-identical (T4); ✅ Notes dataset + localStorage migration (T5-T10); ✅ durability indicator (T2-T3); ✅ SP1↔SP2 seam (T11); declined RBSR/mint/community-migration correctly out of scope.
- **Type consistency:** `MergeOutcome`, `FleetRootPublish`, `FleetPersist<S>`, `FleetSyncConfig<S>`, `FleetSyncEngine<S>`, `mint_next_hlc`, `NotesDoc`/`Note`, `NoteView`/`NoteEntry`, `NotesAdapterHandles` are used consistently across tasks.
- **Remaining `todo!`s are intentional harness stubs** with explicit implementer instructions (two-engine forwarder, ULID, Tauri command wrappers, mesh harness) — each names exactly what to build and which donor lines to copy. They are NOT silent placeholders; they are test-harness scaffolds the implementer fills following the cited patterns.
- **CRITICAL ordering** is centralized once in the engine (apply→advance→persist) and tested (T2 ordering test, T5 convergence).
- **Wire/disk identity** for owner-state is pinned (T1 + T4 pin tests) and `publish_seen=false` keeps `seen` off owner's wire.
