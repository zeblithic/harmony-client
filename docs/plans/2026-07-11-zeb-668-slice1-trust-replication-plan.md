# ZEB-668 S1 — Trust-State Replication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replicate the harmony-owner trust CRDT (enrollments / vouching / revocations / liveness) between an owner's devices via a new `FleetSyncEngine` instance, with a revoked-self detection hook.

**Architecture:** The trust CRDT (`harmony_owner::state::OwnerState`, persisted `owner_state.cbor`) is device-local today. We add the eighth `FleetSyncEngine<S>` instance (donors: `owner_state_sync.rs` thin-wrapper, `fleet_net` boot block at `lib.rs:5101-5230`, `spawn_dataset_sync_zenoh_adapter` at `event_loop.rs:1891-1918`): a resident `Arc<Mutex<OwnerState>>` doc in AppState, a merger that folds remote records through the crate's own validating `add_*` mutators, persistence through the existing `save_owner_state_cbor_only`, and zenoh transport on dataset `owner-trust-v1` (`harmony/owner/{addr_hex}/ds/owner-trust-v1`). An `on_applied` nudge task emits `owner-devices-updated`, detects `is_revoked(self)` → emits `device-revoked-self`, halts fleet engines, and start_node refuses fleet wiring for a revoked self.

**Tech Stack:** Rust (tokio, serde canonical CBOR), existing `fleet_sync::FleetSyncEngine`, Tauri events. No frontend changes in S1 (panel work is S2).

## Global Constraints

- Spec: `docs/specs/2026-07-11-zeb-668-device-management-design.md` §2. Deviation approved during planning: transport uses the established dataset pattern `harmony/owner/{addr_hex}/ds/owner-trust-v1` (via `spawn_dataset_sync_zenoh_adapter`), not a bespoke `trust-root-v1` topic; lookup tag is `b"owner-trust-v1"`.
- Event names exactly: `owner-devices-updated`, `device-revoked-self`.
- Merge is **never trust-degrading**: records that fail crate validation are dropped with `tracing::warn!`, never applied.
- Fold order in the merger is load-bearing: enrollments → revocations → vouching → liveness (signer-enrollment checks require enrollments first; remove-wins revocations must land before vouching/liveness so a revoked signer's records are rejected).
- Keychain writes only via `*_inner` seams (ZEB-428); this slice touches ONLY `save_owner_state_cbor_only` (disk, no keychain).
- All cargo commands from `src-tauri/`, always `--locked`; iterative gates via `scripts/test-select --context task` (paste the `round=…/bucket=…` line into reports); clippy `--all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo fmt --all -- --check`.
- Commit trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`.
- Implementer gate budget: commit before gates; if a gate exceeds 10 min wall-clock, kill it and report DONE_WITH_CONCERNS.

## File Structure

- Create: `src-tauri/src/owner_trust_sync.rs` — everything trust-sync-specific: CanonicalPayload registration, replay persist helpers, `TrustPersist`, merger, dual-mode mutation helper, revoked-self detector task. One module, one responsibility (mirrors `owner_state_sync.rs` scope).
- Modify: `src-tauri/src/lib.rs` — AppState fields, start_node boot block, stop_node/shutdown teardown, module decl.
- Modify: `src-tauri/src/event_loop.rs` — one more `spawn_dataset_sync_zenoh_adapter` call + handles field.
- Modify: `src-tauri/src/owner_commands.rs` — `get_owner_state` reads the resident doc when present; liveness refresh goes through the mutation helper.
- Modify: `src-tauri/src/pairing/persist.rs` — pairing installs update the resident doc + notify_dirty when the engine is live.

---

### Task 1: `owner_trust_sync` module core — registration, persistence, merger

**Files:**
- Create: `src-tauri/src/owner_trust_sync.rs`
- Modify: `src-tauri/src/lib.rs` (module decl only, alongside `mod owner_state_sync;`)

**Interfaces:**
- Consumes: `harmony_owner::state::OwnerState` (derives `Debug, Clone, Default, Serialize, Deserialize`, validating deserialize — `state.rs:18`); `crate::fleet_sync::{FleetPersist, MergeOutcome, Merger, SyncError}`; `crate::owner_state::save_owner_state_cbor_only(identity_dir: &Path, state: &OwnerState) -> Result<(), String>` (`owner_state.rs:617`); `crate::owner_state_types::Hlc`.
- Produces (later tasks rely on these exact names):
  - `pub const OWNER_TRUST_DATASET: &str = "owner-trust-v1";`
  - `pub const OWNER_TRUST_LOOKUP_TAG: &[u8] = b"owner-trust-v1";`
  - `pub const OWNER_TRUST_DATASET_MAX_BYTES: usize = 256 * 1024;`
  - `pub const OWNER_TRUST_REPLAY_FILENAME: &str = "owner_trust_replay.cbor";`
  - `pub fn merge_trust_remote_into_local(local: &mut OwnerState, remote: OwnerState) -> MergeOutcome`
  - `pub fn trust_merger() -> Merger<OwnerState>`
  - `pub struct TrustPersist { pub identity_dir: PathBuf, pub replay_path: PathBuf }` implementing `FleetPersist<OwnerState>`
  - `pub fn load_trust_replay_or_recover(path: &Path) -> BTreeMap<String, Hlc>`

- [ ] **Step 1: Write the failing tests** (inside `#[cfg(test)] mod tests` in the new file)

```rust
// Test fixtures: harmony_owner's lifecycle mint gives us real signed records.
// mint_owner(now) -> (OwnerState with device #1 enrolled, RecoveryArtifact, device sk).
// Follow the fixture recipe used by src-tauri/src/owner_state.rs tests
// (mint via harmony_owner::lifecycle::mint::mint_owner, enroll a second
// device via lifecycle::enroll_master::enroll_with_master).

#[test]
fn merge_folds_new_enrollment_from_remote() {
    let now = 1_700_000_000u64;
    let (mut local, artifact, _sk1) = test_mint(now);
    let mut remote = local.clone();
    let (_sk2, cert2) = test_enroll_second_device(&artifact, &remote, now + 10);
    remote
        .add_enrollment(cert2, now + 10, harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS)
        .unwrap();
    assert_eq!(local.enrollments.len(), 1);
    let outcome = merge_trust_remote_into_local(&mut local, remote);
    assert!(outcome.changed);
    assert_eq!(local.enrollments.len(), 2);
}

#[test]
fn merge_is_idempotent_and_reports_unchanged() {
    let now = 1_700_000_000u64;
    let (mut local, _artifact, _sk) = test_mint(now);
    let remote = local.clone();
    let outcome = merge_trust_remote_into_local(&mut local, remote);
    assert!(!outcome.changed);
}

#[test]
fn merge_revocation_wins_over_concurrent_liveness() {
    // Remote revokes device 2; a liveness cert for device 2 in the same
    // snapshot must not resurrect it (fold order: revocations before liveness).
    let now = 1_700_000_000u64;
    let (mut local, artifact, _sk1) = test_mint(now);
    let (sk2, cert2) = test_enroll_second_device(&artifact, &local, now + 10);
    let d2 = cert2.device_id;
    local
        .add_enrollment(cert2, now + 10, harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS)
        .unwrap();
    let mut remote = local.clone();
    let rev = test_master_revocation(&artifact, &remote, d2, now + 20);
    remote.add_revocation(rev, now + 20).unwrap();
    // Liveness signed by device 2 AFTER the revocation exists in the doc —
    // add on a pre-revocation copy, then merge both into local.
    let mut remote_liveness_branch = local.clone();
    let live2 = test_liveness(&sk2, &local, d2, now + 25);
    remote_liveness_branch.add_liveness(live2).unwrap();
    merge_trust_remote_into_local(&mut local, remote);
    let outcome2 = merge_trust_remote_into_local(&mut local, remote_liveness_branch);
    assert!(local.is_revoked(&d2));
    // The revoked device's liveness record must have been dropped.
    assert!(!local.liveness.contains_key(&d2));
    let _ = outcome2;
}

#[test]
fn merge_drops_record_for_foreign_owner_without_degrading() {
    let now = 1_700_000_000u64;
    let (mut local, _a1, _s1) = test_mint(now);
    let (foreign, _a2, _s2) = test_mint(now + 5); // different owner_id
    let before = local.clone();
    let outcome = merge_trust_remote_into_local(&mut local, foreign);
    assert!(!outcome.changed);
    assert_eq!(
        harmony_owner::cbor::to_canonical(&local).unwrap(),
        harmony_owner::cbor::to_canonical(&before).unwrap()
    );
}

#[test]
fn trust_persist_round_trips_doc_and_replay() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000u64;
    let (state, _a, _s) = test_mint(now);
    let replay_path = dir.path().join(OWNER_TRUST_REPLAY_FILENAME);
    let persist = TrustPersist {
        identity_dir: dir.path().to_path_buf(),
        replay_path: replay_path.clone(),
    };
    let mut tracker = std::collections::BTreeMap::new();
    tracker.insert("device-a".to_string(), crate::owner_state_types::Hlc::new(now, 0));
    fleet_persist_call(&persist, &state, &tracker); // thin helper: persist.persist(...)
    let reloaded = crate::owner_state::load_owner_state_cbor(dir.path()).unwrap();
    assert_eq!(reloaded.enrollments.len(), state.enrollments.len());
    let replay = load_trust_replay_or_recover(&replay_path);
    assert_eq!(replay.get("device-a"), tracker.get("device-a"));
}

#[test]
fn replay_recover_returns_empty_on_missing_or_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.cbor");
    assert!(load_trust_replay_or_recover(&missing).is_empty());
    let corrupt = dir.path().join("bad.cbor");
    std::fs::write(&corrupt, b"not cbor").unwrap();
    assert!(load_trust_replay_or_recover(&corrupt).is_empty());
}
```

Note for the implementer: `test_mint` / `test_enroll_second_device` / `test_master_revocation` / `test_liveness` are local fixture fns you write in the test module using `harmony_owner::lifecycle::{mint, enroll_master}` and `RevocationCert::sign_master` / `LivenessCert` signing — copy the exact recipe from the existing fixtures in `src-tauri/src/owner_state.rs`'s test module (search `mint_owner` there). If `load_owner_state_cbor` (doc-only loader) does not already exist in `owner_state.rs`, add a thin `pub fn load_owner_state_cbor(identity_dir: &Path) -> Result<OwnerState, String>` next to `save_owner_state_cbor_only` reading the same path — check first; a private reader likely exists to extract.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(owner_trust)'`
Expected: compile failure — module `owner_trust_sync` does not exist.

- [ ] **Step 3: Implement the module core**

```rust
//! Trust-state replication (ZEB-668 S1). Replicates the harmony-owner
//! trust CRDT (enrollments / vouching / revocations / liveness) between
//! the owner's devices as the next `FleetSyncEngine` dataset. Donor
//! pattern: `owner_state_sync.rs` (wrapper shape) + the fleet-net boot
//! block (engine construction). Spec:
//! docs/specs/2026-07-11-zeb-668-device-management-design.md §2.

use crate::fleet_sync::{FleetPersist, MergeOutcome, Merger, SyncError};
use crate::owner_state::save_owner_state_cbor_only;
use crate::owner_state_types::Hlc;
use harmony_owner::state::OwnerState;
use harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const OWNER_TRUST_DATASET: &str = "owner-trust-v1";
pub const OWNER_TRUST_LOOKUP_TAG: &[u8] = b"owner-trust-v1";
/// Trust docs are tiny (≤ MAX_DEVICES_PER_OWNER certs per map); 256 KiB is
/// generous headroom while still bounding a hostile publish.
pub const OWNER_TRUST_DATASET_MAX_BYTES: usize = 256 * 1024;
pub const OWNER_TRUST_REPLAY_FILENAME: &str = "owner_trust_replay.cbor";

// ZEB-220 sealed CanonicalPayload registration for the FOREIGN type
// harmony_owner::state::OwnerState — same two empty impls the
// impl_canonical! macro expands to (see fleet_sync.rs:65-71 which does
// this for FleetRootPublish).
impl crate::owner_state_types::CanonicalPayloadSealed for OwnerState {}
impl crate::owner_state_types::CanonicalPayload for OwnerState {}

/// Fold a remote trust snapshot into local via the crate's validating
/// mutators. NEVER trust-degrading: records that fail validation are
/// dropped with a warn log. Fold order is load-bearing (global
/// constraints): enrollments → revocations → vouching → liveness.
/// Changed-detection is canonical-encode compare (docs are tiny).
pub fn merge_trust_remote_into_local(local: &mut OwnerState, remote: OwnerState) -> MergeOutcome {
    let before = harmony_owner::cbor::to_canonical(local).ok();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    for (_id, cert) in remote.enrollments {
        if let Err(e) = local.add_enrollment(cert, now, DEFAULT_ACTIVE_WINDOW_SECS) {
            tracing::debug!(error = %e, "trust merge: enrollment dropped");
        }
    }
    for cert in remote.revocations.iter_certs() {
        if let Err(e) = local.add_revocation(cert.clone(), now) {
            tracing::debug!(error = %e, "trust merge: revocation dropped");
        }
    }
    for cert in remote.vouching.iter_certs() {
        if let Err(e) = local.add_vouching(cert.clone()) {
            tracing::debug!(error = %e, "trust merge: vouching dropped");
        }
    }
    for (_id, cert) in remote.liveness {
        if let Err(e) = local.add_liveness(cert) {
            tracing::debug!(error = %e, "trust merge: liveness dropped");
        }
    }
    let after = harmony_owner::cbor::to_canonical(local).ok();
    MergeOutcome { changed: before != after }
}

pub fn trust_merger() -> Merger<OwnerState> {
    Arc::new(merge_trust_remote_into_local)
}

/// Durability sink: doc through the existing owner_state.cbor writer
/// (disk-only — no keychain; ZEB-428 stays untouched), replay tracker to
/// its own file (canonical CBOR, atomic-tmp-rename like
/// fleet_net_persist::save_replay — copy that recipe).
pub struct TrustPersist {
    pub identity_dir: PathBuf,
    pub replay_path: PathBuf,
}

impl FleetPersist<OwnerState> for TrustPersist {
    fn persist(&self, state: &OwnerState, tracker: &BTreeMap<String, Hlc>) -> Result<(), SyncError> {
        save_owner_state_cbor_only(&self.identity_dir, state)
            .map_err(SyncError::Persist)?;
        save_trust_replay(&self.replay_path, tracker)
            .map_err(SyncError::Persist)?;
        Ok(())
    }
}
```

Adaptation notes (verify while implementing, adjust mechanically):
- `remote.enrollments` / `remote.liveness` are `HashMap<[u8;16], _>` — iterate by value as shown (remote is owned).
- If `RevocationSet` / `VouchingSet` expose no `iter_certs()`, check their APIs in `~/.cargo/git/checkouts/harmony-6e325dd2bc445c08/8b870ae/crates/harmony-owner/src/crdt/` for the actual iteration method (`certs()`, `iter()`, or public field) and use that; do NOT add crate patches.
- `add_enrollment(cert, now, window)` / `add_revocation(cert, now)` signatures: confirm arg lists at `state.rs:188` / `state.rs:365` and match exactly.
- `save_trust_replay` / `load_trust_replay_or_recover`: copy `fleet_net_persist.rs`'s `save_replay` / `load_replay_or_recover` bodies (atomic tmp+rename write, recover-to-empty on read failure), retargeted at the new filename.
- If `harmony_owner::cbor::to_canonical` is not public, use the client's `crate::owner_state_crypto::canonical_cbor_encode` for the changed-compare instead.
- Merger closure type is `Arc<dyn Fn(&mut S, S) -> MergeOutcome>`; `Arc::new(merge_trust_remote_into_local)` coerces a fn item — if inference balks, wrap in a closure.

- [ ] **Step 4: Add `mod owner_trust_sync;` to lib.rs** next to `mod owner_state_sync;`, run tests

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(owner_trust)'`
Expected: PASS (all 6).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/owner_trust_sync.rs src-tauri/src/lib.rs src-tauri/src/owner_state.rs
git commit -m "ZEB-668 S1 T1: owner_trust_sync core — registration, merger, persist"
```

---

### Task 2: Resident trust doc + dual-mode mutation helper

**Files:**
- Modify: `src-tauri/src/owner_trust_sync.rs` (add helper + AppState-facing types)
- Modify: `src-tauri/src/lib.rs` (AppState fields ~line 1329 block, init ~line 1780 block, reset in stop_node ~line 2352 block)
- Modify: `src-tauri/src/owner_commands.rs` (`get_owner_state` resident-doc path)

**Interfaces:**
- Consumes: Task 1's module; `LoadedOwnerState` (`owner_state.rs`, has `.state: OwnerState`); `refresh_self_liveness(&mut OwnerState, &SigningKey, [u8;16], u64)`-shaped helper (`owner_state.rs:769`, confirm exact signature).
- Produces:
  - AppState fields (exact names, mirroring the `fleet_net_doc` block at `lib.rs:1329-1341`): `pub owner_trust_doc: Option<std::sync::Arc<tokio::sync::Mutex<harmony_owner::state::OwnerState>>>`, `pub owner_trust_sync: Option<std::sync::Arc<crate::fleet_sync::FleetSyncEngine<harmony_owner::state::OwnerState>>>`, `pub owner_trust_revoked_self: std::sync::Arc<std::sync::atomic::AtomicBool>`
  - `pub enum TrustStateAccess { Resident { doc, engine }, FileOnly { identity_dir } }` — constructed by callers from AppState
  - `pub async fn mutate_trust_state<R>(access: TrustStateAccess, f: impl FnOnce(&mut OwnerState) -> R) -> Result<R, String>` — Resident: lock doc, apply `f`, `engine.notify_dirty()`; FileOnly: load `owner_state.cbor`, apply `f`, `save_owner_state_cbor_only`.

- [ ] **Step 1: Write the failing tests** (same test module)

```rust
#[tokio::test]
async fn mutate_file_only_loads_applies_saves() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000u64;
    let (state, _a, _s) = test_mint(now);
    save_owner_state_cbor_only(dir.path(), &state).unwrap();
    let n = mutate_trust_state(
        TrustStateAccess::FileOnly { identity_dir: dir.path().to_path_buf() },
        |s| s.enrollments.len(),
    )
    .await
    .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn mutate_resident_applies_and_notifies() {
    // Engine harness: construct a FleetSyncEngine over in-memory channels
    // exactly as owner_state_sync's tests do (see its test module around
    // line 1953 — reuse that harness recipe with trust_merger()).
    let (engine, doc, mut out_rx) = test_trust_engine().await;
    mutate_trust_state(
        TrustStateAccess::Resident { doc: doc.clone(), engine: engine.clone() },
        |s| { let _ = s; },
    )
    .await
    .unwrap();
    // notify_dirty → debounced publish lands on the outbound channel.
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), out_rx.recv())
        .await
        .expect("publish within 5s")
        .expect("channel open");
    assert!(!frame.is_empty());
}
```

- [ ] **Step 2: Run to verify failure** — `cargo nextest run --locked --features test-fixtures -E 'test(mutate_trust)'` → compile failure (types missing).

- [ ] **Step 3: Implement** `TrustStateAccess` + `mutate_trust_state` in `owner_trust_sync.rs`:

```rust
pub enum TrustStateAccess {
    Resident {
        doc: Arc<tokio::sync::Mutex<OwnerState>>,
        engine: Arc<crate::fleet_sync::FleetSyncEngine<OwnerState>>,
    },
    FileOnly {
        identity_dir: PathBuf,
    },
}

/// Apply a mutation to the trust doc in whichever mode the app is in.
/// Resident (node running): mutate the shared doc and let the engine's
/// debounced publish+persist carry it. FileOnly (node stopped / CLI):
/// classic load-mutate-save on owner_state.cbor.
pub async fn mutate_trust_state<R>(
    access: TrustStateAccess,
    f: impl FnOnce(&mut OwnerState) -> R,
) -> Result<R, String> {
    match access {
        TrustStateAccess::Resident { doc, engine } => {
            let r = {
                let mut guard = doc.lock().await;
                f(&mut guard)
            };
            engine.notify_dirty();
            Ok(r)
        }
        TrustStateAccess::FileOnly { identity_dir } => {
            let mut state = crate::owner_state::load_owner_state_cbor(&identity_dir)?;
            let r = f(&mut state);
            save_owner_state_cbor_only(&identity_dir, &state)?;
            Ok(r)
        }
    }
}
```

Add the three AppState fields in the `lib.rs:1329` block with doc comments matching the `fleet_net_doc` style ("`None` until start_node wires the FleetSyncEngine"); init them `None` / fresh `AtomicBool(false)` in the constructor block (~1780) and clear the two `Option`s in the stop_node reset block (~2352, next to `fleet_net_device_id = None`). The AtomicBool is NOT cleared on stop_node (revoked stays revoked for the process lifetime; boot re-derives it from disk).

In `owner_commands.rs` `get_owner_state`: where it currently loads from disk and calls `refresh_self_liveness` then `save_owner_state_cbor_only` (`owner_commands.rs:203-208`), first check the resident doc: if `state.owner_trust_doc` is `Some`, clone the doc snapshot under its lock for the view build, and run the liveness refresh through `mutate_trust_state(Resident…)` instead of the file path. The existing file path remains as the `else` branch (node stopped). Preserve the existing view-building code untouched — only the state source changes.

- [ ] **Step 4: Run** the two new tests + existing owner_commands tests: `cargo nextest run --locked --features test-fixtures -E 'test(mutate_trust) or test(get_owner_state)'` → PASS.

- [ ] **Step 5: Commit** — `git commit -m "ZEB-668 S1 T2: resident trust doc + dual-mode mutation helper"`

---

### Task 3: Engine boot, zenoh adapter, shutdown, pairing writers

**Files:**
- Modify: `src-tauri/src/lib.rs` (start_node owner-loaded block — insert after the fleet-net block ending ~5230; shutdown block ~2820)
- Modify: `src-tauri/src/event_loop.rs` (handles struct + one adapter call after line 1918)
- Modify: `src-tauri/src/pairing/persist.rs` (post-install notify)

**Interfaces:**
- Consumes: Tasks 1–2; `DatasetSyncHandles { addr_hex, outbound_rx, inbound_tx }` (`event_loop.rs:80-88`); `spawn_dataset_sync_zenoh_adapter(&session, &app, &closing, handles, dataset, degraded_event, max_bytes)`; the boot-block locals `kt`, `device_id`, `content_store`, `identity_dir`, `loaded` (the `LoadedOwnerState`), `owner_addr_hex`.
- Produces: a running trust engine end-to-end; `trust_sync_handles: Option<DatasetSyncHandles>` threaded to event_loop exactly like `p2_sync_handles`.

- [ ] **Step 1: Write the failing convergence test** (in `owner_trust_sync.rs` tests — pure channel harness, no zenoh):

```rust
#[tokio::test]
async fn two_engines_converge_on_revocation() {
    // Device A and device B each run a trust engine over crossed in-memory
    // channels (A.out → B.in, B.out → A.in) — the owner_state_sync test
    // harness recipe. A revokes device B; B's doc must converge to
    // is_revoked(B) and B's replay tracker must record A's publish.
    let now = 1_700_000_000u64;
    let (state_a, artifact, _sk1) = test_mint(now);
    let (sk_b, cert_b) = test_enroll_second_device(&artifact, &state_a, now + 10);
    let _ = sk_b;
    let mut seeded = state_a.clone();
    seeded.add_enrollment(cert_b.clone(), now + 10, harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS).unwrap();
    let (eng_a, doc_a, eng_b, doc_b) = test_trust_engine_pair(seeded.clone()).await;
    let d_b = cert_b.device_id;
    let rev = test_master_revocation(&artifact, &seeded, d_b, now + 20);
    mutate_trust_state(
        TrustStateAccess::Resident { doc: doc_a, engine: eng_a },
        move |s| s.add_revocation(rev, now + 20).unwrap(),
    ).await.unwrap();
    // Poll B up to 5s for convergence.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if doc_b.lock().await.is_revoked(&d_b) { break; }
        assert!(std::time::Instant::now() < deadline, "B never converged");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let _ = eng_b;
}
```

- [ ] **Step 2: Run to verify failure** — harness fn missing → compile failure.

- [ ] **Step 3: Implement boot + wiring.** In the start_node owner-loaded block, directly after the fleet-net block (post ~5230), mirroring it exactly:

```rust
// ── ZEB-668 S1: owner-trust replication engine ─────────────────────
let trust_replay_path = identity_dir.join(crate::owner_trust_sync::OWNER_TRUST_REPLAY_FILENAME);
let owner_trust_doc = std::sync::Arc::new(tokio::sync::Mutex::new(loaded.state.clone()));
let owner_trust_tracker = std::sync::Arc::new(tokio::sync::Mutex::new(
    crate::owner_trust_sync::load_trust_replay_or_recover(&trust_replay_path),
));
let (trust_out_tx, trust_out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
let (trust_in_tx, trust_in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
let owner_trust_sync = std::sync::Arc::new(crate::fleet_sync::FleetSyncEngine::new(
    crate::fleet_sync::FleetSyncConfig {
        kt: std::sync::Arc::clone(&kt),
        device_id: device_id.clone(),
        state: std::sync::Arc::clone(&owner_trust_doc),
        merger: crate::owner_trust_sync::trust_merger(),
        replay_tracker: std::sync::Arc::clone(&owner_trust_tracker),
        content_store: std::sync::Arc::clone(&content_store),
        publisher_tx: trust_out_tx,
        subscriber_rx: trust_in_rx,
        persist: std::sync::Arc::new(crate::owner_trust_sync::TrustPersist {
            identity_dir: identity_dir.clone(),
            replay_path: trust_replay_path,
        }),
        lookup_key_tag: crate::owner_trust_sync::OWNER_TRUST_LOOKUP_TAG,
        debounce_ms: crate::fleet_sync::DEFAULT_DEBOUNCE_MS,
        publish_seen: true,
        on_applied: None, // Task 4 replaces this with the nudge helper
        sibling_acks: std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::BTreeMap::new())),
    },
));
```

Then:
- Store `owner_trust_doc` / `owner_trust_sync` into the AppState fields (same place fleet_net stores its).
- Build `trust_sync_handles = Some(DatasetSyncHandles { addr_hex: owner_addr_hex.clone(), outbound_rx: trust_out_rx, inbound_tx: trust_in_tx })` and thread it into the event_loop spawn exactly as `p2_sync_handles` travels (add a field/param alongside it; grep `p2_sync_handles` for the 3-4 plumbing points).
- In `event_loop.rs`, after the fleet-net adapter call (line 1918):

```rust
if let Some(trust) = trust_sync_handles.take() {
    spawn_dataset_sync_zenoh_adapter(
        &session, &app, &closing, trust,
        crate::owner_trust_sync::OWNER_TRUST_DATASET,
        "owner-trust-sync-degraded",
        crate::owner_trust_sync::OWNER_TRUST_DATASET_MAX_BYTES,
    ).await;
}
```

- Shutdown: in the stop_node/shutdown teardown (~2820, next to `fleet_net_engine.shutdown()`), take the trust engine and `rt.block_on(engine.shutdown())`, logging errors — copy the fleet_net shutdown stanza verbatim with names swapped.
- Pairing writers (`pairing/persist.rs`): after the existing successful install/persist of trust state on inviter (`persist.rs:166-173`) and joiner (`persist.rs:29-97`), if AppState carries a live resident doc+engine, also fold the freshly-installed state into it via `mutate_trust_state(Resident…, |s| merge_trust_remote_into_local(s, installed.clone()))`. Pairing runs while the node is up, so the resident doc exists; guard with `if let Some(...)` anyway (defense in depth — and tests run persist without AppState).
- Also implement the `test_trust_engine` / `test_trust_engine_pair` fixtures used by Tasks 2–3 (crossed channels, `publish_seen: true`, tempdir persist).

- [ ] **Step 4: Run** — `cargo nextest run --locked --features test-fixtures -E 'test(owner_trust) or test(two_engines_converge)'` → PASS. Then `scripts/test-select --context task` (from repo root) → paste `round=…/bucket=…` line. Expected: green.

- [ ] **Step 5: Commit** — `git commit -m "ZEB-668 S1 T3: trust engine boot + zenoh adapter + shutdown + pairing writers"`

---

### Task 4: `on_applied` nudge task — events + revoked-self halt + boot refusal

**Files:**
- Modify: `src-tauri/src/owner_trust_sync.rs` (detector task fn)
- Modify: `src-tauri/src/lib.rs` (wire `on_applied`, spawn detector, boot refusal check)

**Interfaces:**
- Consumes: `crate::dm_inbox_ingest::ingest_nudge_on_applied(tx)` (the generic try_send-`()` nudge helper, see `lib.rs:5166`); Tauri event emission — grep `"storage-buddies-updated"` in lib.rs for the exact `app_handle.emit` idiom and copy it.
- Produces: `pub fn spawn_trust_applied_task(...) -> tokio::task::JoinHandle<()>` with signature:

```rust
pub fn spawn_trust_applied_task(
    mut nudge_rx: tokio::sync::mpsc::Receiver<()>,
    doc: Arc<tokio::sync::Mutex<OwnerState>>,
    self_device_id: [u8; 16],
    revoked_flag: Arc<std::sync::atomic::AtomicBool>,
    emit: Arc<dyn Fn(&str) + Send + Sync>, // emit(event_name) — lib.rs passes a closure over app_handle
    engines_to_halt: Vec<Arc<dyn TrustHaltable>>, // see below
) -> tokio::task::JoinHandle<()>
```

with `pub trait TrustHaltable: Send + Sync { fn halt(&self) -> futures::future::BoxFuture<'static, ()>; }` — lib.rs wraps each engine's `shutdown()` (owner-state SyncEngine, fleet-net engine, trust engine) in a small adapter struct so the task needs no generic knowledge. If `futures` is not already a dependency, use `std::pin::Pin<Box<dyn Future<Output=()> + Send>>` directly.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn applied_task_emits_devices_updated_on_nudge() {
    let now = 1_700_000_000u64;
    let (state, _a, _s) = test_mint(now);
    let self_id = *state.enrollments.keys().next().unwrap();
    let doc = Arc::new(tokio::sync::Mutex::new(state));
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let ev2 = Arc::clone(&events);
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _h = spawn_trust_applied_task(rx, doc, self_id, Arc::clone(&flag),
        Arc::new(move |name: &str| ev2.lock().unwrap().push(name.to_string())), vec![]);
    tx.send(()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(events.lock().unwrap().as_slice(), ["owner-devices-updated"]);
    assert!(!flag.load(std::sync::atomic::Ordering::Acquire));
}

#[tokio::test]
async fn applied_task_detects_self_revocation_and_halts() {
    let now = 1_700_000_000u64;
    let (mut state, artifact, _sk1) = test_mint(now);
    let self_id = *state.enrollments.keys().next().unwrap();
    // Second device so revoking self doesn't leave an empty fleet in the fixture.
    let (_skb, cert_b) = test_enroll_second_device(&artifact, &state, now + 5);
    state.add_enrollment(cert_b, now + 5, harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS).unwrap();
    let rev = test_master_revocation(&artifact, &state, self_id, now + 10);
    state.add_revocation(rev, now + 10).unwrap();
    let doc = Arc::new(tokio::sync::Mutex::new(state));
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let ev2 = Arc::clone(&events);
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let halted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _h = spawn_trust_applied_task(rx, doc, self_id, Arc::clone(&flag),
        Arc::new(move |n: &str| ev2.lock().unwrap().push(n.to_string())),
        vec![test_haltable(Arc::clone(&halted))]);
    tx.send(()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let evs = events.lock().unwrap().clone();
    assert!(evs.contains(&"owner-devices-updated".to_string()));
    assert!(evs.contains(&"device-revoked-self".to_string()));
    assert!(flag.load(std::sync::atomic::Ordering::Acquire));
    assert!(halted.load(std::sync::atomic::Ordering::Acquire));
}

#[tokio::test]
async fn applied_task_fires_revoked_self_exactly_once() {
    // Second nudge after detection must emit owner-devices-updated only.
    // (Build on the previous test's setup; send two nudges; count
    // device-revoked-self occurrences == 1.)
}
```

(The third test's body follows the second's pattern — write it fully in the implementation, asserting the count.)

- [ ] **Step 2: Run to verify failure** — compile failure (fn missing).

- [ ] **Step 3: Implement** the task: loop on `nudge_rx.recv()`; per nudge → emit `owner-devices-updated`; then `doc.lock().await`, check `is_revoked(&self_device_id)`; if true and `revoked_flag.swap(true, AcqRel) == false` → emit `device-revoked-self`, then `for e in &engines_to_halt { e.halt().await; }` (halting the trust engine from here is safe: `on_applied` only try_sends the nudge and returns, so the engine task is free to process its shutdown message). In lib.rs: create the nudge channel, pass `on_applied: Some(crate::dm_inbox_ingest::ingest_nudge_on_applied(trust_nudge_tx))` (replacing Task 3's `None`), spawn the task with adapters over the owner-state `SyncEngine`, fleet-net engine, and trust engine, and an `emit` closure over the app handle (copy the `storage-buddies-updated` emit idiom). Boot refusal: at the top of the owner-loaded block, after `loaded` is available — `if loaded.state.is_revoked(&self_device_id_bytes) { tracing::warn!(...); skip wiring ALL fleet engines (owner-state, fleet-net, trust, notes, dm-inbox, outhold, relay) but do not fail start_node; set the flag; emit device-revoked-self; }`. Implement as an early bool `let self_revoked = …` gating the engine-wiring blocks (`if !self_revoked { … }`), NOT an early return (the rest of start_node must still run).

- [ ] **Step 4: Run** — `cargo nextest run --locked --features test-fixtures -E 'test(applied_task)'` → PASS, then `scripts/test-select --context task` (paste round/bucket line) → green.

- [ ] **Step 5: Commit** — `git commit -m "ZEB-668 S1 T4: trust on_applied task — events, revoked-self halt, boot refusal"`

---

### Task 5: Full gates + PR

**Files:** none new (fixes only).

- [ ] **Step 1:** `cd src-tauri && cargo fmt --all` then `cargo fmt --all -- --check` → clean.
- [ ] **Step 2:** `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` → clean (10-min budget; commit first).
- [ ] **Step 3:** Full sweep: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` → all pass. Frontend untouched, but run `npx tsc --noEmit && npx vitest run` from repo root anyway (cheap; guards accidental drift).
- [ ] **Step 4:** Commit any gate fixes; push branch `zeb-668-device-mgmt`; open PR titled `ZEB-668 S1: trust-state replication — enrollments/vouching/revocations/liveness sync between owner devices` with spec link, the ds-topic deviation note, and the standard attribution footer; fire `@coderabbitai review` ONCE at open.

---

## Verification checklist (self-review done at write time)

- Spec §2 coverage: topic/tag ✅ (T3, with approved ds-pattern deviation), merge-via-mutators ✅ (T1), never-trust-degrading ✅ (T1), persistence via cbor_only ✅ (T1), publish triggers ✅ (T2 helper + T3 pairing writers), wipe-on-receipt hook + events ✅ (T4), liveness propagation side effect ✅ (T2 get_owner_state refresh through helper), boot refusal ✅ (T4).
- Not in S1 (later slices per spec): revoke IPC (S2), panel UI (S2), retire-announce (S3), self-revoke publish-before-stop ordering (S2 — no revoke path exists in S1 to order).
- Type consistency: `TrustStateAccess`/`mutate_trust_state` (T2) consumed by T3/T4 with matching signatures; constants from T1 used in T3.
