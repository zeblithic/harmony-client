# ZEB-687 — Boot-integration regression guard for the RevokedDeviceProjection feed

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a test that fails if the live `start_node_inner` → `RevokedDeviceProjection` feed wiring is ever removed, so the shared-community DM revocation cutoff (ZEB-684) cannot be silently disabled by a refactor.

**Architecture:** The cutoff is fed by two calls in `start_node_inner` (`lib.rs`): the live on-epoch delta hook (~7115) and the boot-replay seed (~7817), both `revoked_device_projection.union_from_members(mat.members…revoked_device_keys)`. Today no test drives either *through a running node* — the projection is a `start_node_inner` local, unobservable, so deleting a feed call fails silently (plain statements, not compiler-enforced). This plan (a) adds a `#[cfg(test)]` observability seam exposing the live projection on `NodeState`, (b) extracts the feed into one shared helper called at both sites, and (c) adds an in-crate boot test that drives one community delta through the **real live on-epoch hook** and asserts the node's projection reports the revoked key.

**Scope decision (settled with Jake, 2026-07-14): Strategy B — live on-epoch hook only.** The boot-replay seed (`lib.rs:7817`) call site is a **documented residual**: exercising it needs a signed `DeviceRetire` + persist + restart cycle (`materialized()` only reads real signed events — `bootstrap_hint` is `#[serde(skip)]`), roughly 2× the cost. Not built here. The shared helper is still *called* at 7817, so its logic is covered; only that call site's deletion stays uncaught.

**Tech Stack:** Rust, tokio, cargo-nextest. In-crate `#[cfg(test)]` test (NOT `tests/`) so it can reach private `NodeState` fields (`community_registry`, `community_delta_tx`) and the test getter without any new public API.

## Global Constraints

- **CI-parity gates** (run from `src-tauri/`): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Frontend gates are untouched by this plan (no TS/Svelte changes).
- **No new PUBLIC production API.** The observability field is a private `NodeState` field; the getter is `#[cfg(test)]` (or `pub(crate)` gated). Do not expose `RevokedDeviceProjection` on `StartNodeResponse` or any pub surface.
- **Keychain safety (ZEB-428):** the boot test MUST set `HARMONY_PASSPHRASE` + a temp `HOME`. A `#[cfg(test)]` build already refuses `KeychainStore::new()` (falls back to the encrypted-file store) — do NOT set `HARMONY_ALLOW_REAL_KEYCHAIN`. Also set `HARMONY_DISABLE_KEYCHAIN=1` belt-and-suspenders.
- **Determinism (no quiet-window barriers):** the test waits for the projection via a **condition poll** (`wait until is_revoked` with a generous timeout), NEVER a wall-clock "stable for N polls" heuristic — that is exactly the ZEB-686 flake anti-pattern. Wrap the whole test body in a defense-in-depth outer `tokio::time::timeout` (kill switch), and keep every inner budget ≫ expected (per `feedback_wall_clock_regression_budget`).
- **Helper extraction is a pure refactor:** feed behavior stays byte-identical; all existing tests remain green. No `#2`/`#3` logic changes.
- **nextest runs each test in its own process**, so `std::env::set_var` in the boot test is process-isolated (no cross-test contamination). Env mutation may be `unsafe` depending on edition — match how env is set elsewhere in the crate.

## File Structure

- **Modify `src-tauri/src/lib.rs` only:**
  - `NodeState` struct (def ~748): add one field.
  - `impl Default for NodeState` (~1713): add the `None` default.
  - `start_node_inner` populate site (where `guard.community_registry` / `community_delta_tx` are assigned into the boot `NodeState`): assign the projection clone.
  - New free fn `feed_revoked_from_materialized` + its two call sites (~7115, ~7817).
  - New `#[cfg(test)]` getter on `NodeState`.
  - New `#[cfg(test)]` boot test module.
- No new files. No changes outside `lib.rs`.

---

### Task 1: Observability seam + shared-helper extraction (no behavior change)

**Files:**
- Modify: `src-tauri/src/lib.rs`

**Interfaces:**
- Produces: `NodeState.revoked_device_projection: Option<crate::revoked_device_projection::RevokedDeviceProjection>` (private field); `NodeState::revoked_device_projection_for_test(&self) -> Option<crate::revoked_device_projection::RevokedDeviceProjection>` (`#[cfg(test)]`); free fn `feed_revoked_from_materialized(proj, mat)`.
- Consumes: existing `revoked_device_projection` local in `start_node_inner` (constructed ~4276); `MaterializedMembership` (`community_membership.rs:1571`, has `Default`); `MemberState.revoked_device_keys: BTreeSet<[u8;32]>`.

- [ ] **Step 1: Write the failing unit test for the helper.** In a `#[cfg(test)]` module in `lib.rs`, add:

```rust
#[test]
fn feed_revoked_from_materialized_unions_member_keys() {
    use crate::community_membership::{MaterializedMembership, MemberState, MemberStatus};
    use crate::owner_state_types::OwnerAddr;
    use crate::revoked_device_projection::RevokedDeviceProjection;
    use std::collections::BTreeSet;

    let proj = RevokedDeviceProjection::new();
    let owner = OwnerAddr([0x11; 16]);
    let revoked = [0xaa; 32];
    let mut mat = MaterializedMembership::default();
    mat.members.insert(
        owner,
        MemberState {
            status: MemberStatus::Joined,
            joined_at: crate::owner_state_types::Hlc { wall_ms: 1, logical: 0, device_id: "zeb687".into() },
            left_at: None,
            enrolled_device_keys: BTreeSet::new(),
            revoked_device_keys: BTreeSet::from([revoked]),
        },
    );
    assert!(!proj.is_revoked(&owner, &revoked));
    feed_revoked_from_materialized(&proj, &mat);
    assert!(proj.is_revoked(&owner, &revoked), "helper must union member revoked keys");
}
```

- [ ] **Step 2: Run it — expect FAIL** (`feed_revoked_from_materialized` not defined). `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(feed_revoked_from_materialized)'`.

- [ ] **Step 3: Add the helper** at module level in `lib.rs` (near other free fns / above `start_node_inner`):

```rust
/// ZEB-687: the single feed point for the revoked-device projection from a
/// community's materialized view. Both `start_node_inner` choke points (the
/// live on-epoch hook and the boot-replay seed) call this, so the feed logic
/// lives in one tested place and a helper-body regression breaks both sites.
fn feed_revoked_from_materialized(
    proj: &crate::revoked_device_projection::RevokedDeviceProjection,
    mat: &crate::community_membership::MaterializedMembership,
) {
    proj.union_from_members(mat.members.iter().map(|(o, m)| (*o, &m.revoked_device_keys)));
}
```

- [ ] **Step 4: Replace both inline feed calls with the helper.**
  - At ~`lib.rs:7115` (live hook; the local is `mat`): replace the `revoked_device_projection.union_from_members(mat.members.iter().map(|(o, m)| (*o, &m.revoked_device_keys)))` statement with `feed_revoked_from_materialized(&revoked_device_projection, &mat);`. **Keep the surrounding comment** (ZEB-580 S2 note) and the unconditional placement (before the joined/remove branch) exactly.
  - At ~`lib.rs:7817` (boot-replay seed; the local is `current`): replace the analogous `union_from_members(current.members…)` statement with `feed_revoked_from_materialized(&revoked_device_projection, &current);`. Keep the comment and unconditional placement.

- [ ] **Step 5: Add the field + Default + test getter.**
  - In `struct NodeState` (~748), after `community_delta_tx` (~871), add:
    ```rust
    /// ZEB-687: a clone of the live `RevokedDeviceProjection` fed by the
    /// on-epoch hook / boot-replay seed. Stored only so `#[cfg(test)]` can
    /// observe that the feed wiring actually runs; production reads go
    /// through the receive-path handles, not this field.
    revoked_device_projection: Option<crate::revoked_device_projection::RevokedDeviceProjection>,
    ```
  - In `impl Default for NodeState` (~1713, beside `community_delta_tx: None`), add `revoked_device_projection: None,`.
  - Run `cargo check --locked --all-targets --features test-fixtures`. Any full `NodeState { … }` struct literals the compiler now flags (i.e. those not using `..Default::default()`) get `revoked_device_projection: None,` added. Note them in the report.
  - Add a getter on `NodeState` (in an existing `impl NodeState` block or a new `#[cfg(test)] impl`):
    ```rust
    #[cfg(test)]
    pub(crate) fn revoked_device_projection_for_test(
        &self,
    ) -> Option<crate::revoked_device_projection::RevokedDeviceProjection> {
        self.revoked_device_projection.clone()
    }
    ```

- [ ] **Step 6: Assign the clone at the boot populate site.** Find where `start_node_inner` populates the boot `NodeState` guard with `community_registry` / `community_delta_tx` (grep for `guard.community_delta_tx =` or the populate block near the end of `start_node_inner`, ~line 10765–10770 — NOT the stop/teardown `.take()` sites ~2279). Add, alongside them:
    ```rust
    guard.revoked_device_projection = Some(revoked_device_projection.clone());
    ```
  `RevokedDeviceProjection` is `Clone` over an inner `Arc<RwLock<…>>`, so the stored clone observes the live hook's writes.

- [ ] **Step 7: Run the helper test — expect PASS.** Then gates: `cargo fmt --all`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, and `scripts/test-select --context task` (behavior unchanged → existing tests green). Paste the `round=… bucket=…` line into the report.

- [ ] **Step 8: Commit.** `git add -A && git commit -m "ZEB-687: observability seam + shared feed helper for RevokedDeviceProjection"`

---

### Task 2: In-crate boot test — the live on-epoch hook feeds the projection

**Files:**
- Modify: `src-tauri/src/lib.rs` (new `#[cfg(test)]` test module)

**Interfaces:**
- Consumes: Task 1's `revoked_device_projection_for_test()`; private `NodeState.community_registry` + `community_delta_tx` (reachable because the test is in-crate); `start_node_inner` (`lib.rs:3203`, `pub`); the on-epoch hook wiring (`lib.rs:7104–7117`).

**Copy-templates (read these first — they carry the exact call shapes):**
- Node boot + env scaffolding: `tests/profile/profile_isolation.rs:17-63` (temp HOME / passphrase / `warm_up_iroh_global_init` / `start_node_inner`).
- In-crate iroh-warmup + 60s kill-switch pattern: `lib.rs:65123-65134` (`force_republish_wakes_publisher`).
- Spawn a bare engine + seed bootstrap hint: `tests/misc/community_presence_two_engine_integration.rs:121` (`spawn_engine_inner_now`) and `:166` (`engine.state().lock().await.seed_bootstrap_hint(hint)`).
- `spawn_engine_inner_now` signature (`community_state_sync.rs:4720`): `(community_id: SpaceId, membership_key: EpochKey, admin_addr: OwnerAddr, is_invite_only: bool, publisher_tx: mpsc::Sender<Vec<u8>>, subscriber_rx: mpsc::Receiver<Vec<u8>>, root_serve_rx: None, fetch_request_tx: None, transport_epoch_rx: None) -> Result<bool, _>`. Test callers pass `None, None, None` for the last three.
- `CommunityMembershipDelta { community_id: SpaceId, event: SignedMembershipEvent }` (`community_state_sync.rs:814`). **The on-epoch hook reads ONLY `delta.community_id`** (`lib.rs:6752`/`7104`) — `event` contents are ignored.
- `SignedMembershipEvent` fields (all pub, `community_membership.rs:470`): `id: EventId (= [u8;16])`, `community_id: SpaceId`, `kind: MembershipEventKind`, `actor: OwnerAddr`, `at: Hlc`, `sig: [u8;64]`, `countersig: Option<_>`, `enrollment: Option<_>`, `signer_certs: Vec<_>`. Cheapest `kind` is field-less **`MembershipEventKind::Join`**.
- `EpochKey::new([u8;32])` (e.g. `EpochKey::new([0x87; 32])`). `OwnerAddr([u8;16])`, `SpaceId(…)` tuple structs (`owner_state_types.rs:354/365`).

- [ ] **Step 1: Write the test** in a new `#[cfg(test)] mod zeb_687_revoked_feed_boot_tests { use super::*; … }`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn live_on_epoch_hook_feeds_revoked_projection() {
    // Defense-in-depth kill switch: boot + iroh init is heavy; the real
    // assertions below use their own tight-ish budgets. (feedback_wall_clock_*)
    tokio::time::timeout(std::time::Duration::from_secs(90), live_on_epoch_hook_feeds_revoked_projection_inner())
        .await
        .expect("test must complete within 90s");
}

async fn live_on_epoch_hook_feeds_revoked_projection_inner() {
    use crate::community_membership::{MaterializedMembership, MemberState, MemberStatus, MembershipEventKind, SignedMembershipEvent};
    use crate::community_state_sync::CommunityMembershipDelta;
    use crate::owner_state_types::{OwnerAddr, SpaceId};
    use std::collections::BTreeSet;
    use std::time::{Duration, Instant};

    // (a) Env scaffolding — temp HOME + passphrase; keychain auto-refuses in
    //     cfg(test). Mirror tests/profile/profile_isolation.rs:19-30. Use the
    //     same env-set mechanism the crate uses elsewhere (env set may be
    //     `unsafe`); nextest's process-per-test makes raw set safe for isolation.
    let home = tempfile::tempdir().expect("tempdir");
    let home_str = home.path().to_str().unwrap().to_string();
    // set HOME, USERPROFILE, HARMONY_PASSPHRASE, XDG_DATA_HOME, APPDATA,
    // HARMONY_DISABLE_KEYCHAIN=1  (see template)

    // (b) Boot the node exactly as serve_cli does.
    crate::iroh_endpoint::warm_up_iroh_global_init().await;
    let state = std::sync::Arc::new(Mutex::new(NodeState::default()));
    let events = crate::api::events::ApiEventSink::new();
    let sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink> = std::sync::Arc::new(events.clone());
    start_node_inner(None, sink, None, &state).await.expect("node boots");

    // (c) Pull the live handles (in-crate → private fields readable).
    let (registry, delta_tx, node_proj) = {
        let g = state.lock().await;
        (
            g.community_registry.clone().expect("registry present after owner-load boot"),
            g.community_delta_tx.clone().expect("delta_tx present"),
            g.revoked_device_projection_for_test().expect("projection stashed"),
        )
    };

    // (d) Spawn a bare engine for a fresh community. NB: SpaceId is [u8;16].
    let cid = SpaceId([0x87; 16]);
    let admin = OwnerAddr([0x87; 16]);
    let mk = crate::owner_state_types::EpochKey::new([0x87; 32]);
    let (pub_tx, _pub_rx) = tokio::sync::mpsc::channel(16);
    let (_sub_tx, sub_rx) = tokio::sync::mpsc::channel(16);
    registry.spawn_engine_inner_now(cid, mk, admin, false, pub_tx, sub_rx, None, None, None)
        .await.expect("spawn engine");
    let engine = registry.engine_arc(&cid).await.expect("engine present");

    // (e) Seed a revoked member with NO signing (bootstrap hint → materialized()).
    let owner = OwnerAddr([0x42; 16]);
    let revoked = [0x99; 32];
    let mut hint = MaterializedMembership::default();
    hint.members.insert(owner, MemberState {
        status: MemberStatus::Joined,
        joined_at: crate::owner_state_types::Hlc { wall_ms: 1, logical: 0, device_id: "zeb687".into() },
        left_at: None,
        enrolled_device_keys: BTreeSet::new(),
        revoked_device_keys: BTreeSet::from([revoked]),
    });
    engine.state().lock().await.seed_bootstrap_hint(hint);

    // (f) Discrimination: the hook has NOT run yet.
    assert!(!node_proj.is_revoked(&owner, &revoked), "projection empty before the delta");

    // (g) Drive ONE delta through the real consumer → on-epoch hook.
    //     Contents ignored by the hook; only community_id is read.
    let ev = SignedMembershipEvent {
        id: [0u8; 16], community_id: cid, kind: MembershipEventKind::Join,
        actor: owner,
        at: crate::owner_state_types::Hlc { wall_ms: 1, logical: 0, device_id: "zeb687".into() },
        sig: [0u8; 64],
        countersig: None, enrollment: None, signer_certs: vec![],
    };
    delta_tx.send(CommunityMembershipDelta { community_id: cid, event: ev }).await.expect("send delta");

    // (h) Condition poll (NOT a quiet-window). Succeeds as soon as the hook feeds.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !node_proj.is_revoked(&owner, &revoked) {
        assert!(Instant::now() < deadline, "on-epoch hook must feed the revoked projection within 10s");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(node_proj.is_revoked(&owner, &revoked), "live on-epoch hook fed the revoked key");
}
```

- [ ] **Step 2: Run it — expect PASS.** `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(live_on_epoch_hook_feeds_revoked_projection)'`. Resolve any construction gaps (Hlc ctor, SpaceId inner size, `EpochKey` import path) from the copy-templates. Do NOT loosen keychain guards to make it boot — set `HARMONY_PASSPHRASE`.

- [ ] **Step 3: RED-check — prove the guard actually guards** (do NOT commit this change). Temporarily neutralize the live feed: comment out the `feed_revoked_from_materialized(&revoked_device_projection, &mat);` call at `lib.rs:~7115`. Re-run ONLY this test → it MUST FAIL (the step-(h) poll times out). Restore the call, re-run → PASS. Paste both outcomes (fail + restored-pass) into the task report — this is the evidence the test detects the regression it exists to catch.

- [ ] **Step 4: Gates.** `cargo fmt --all`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `scripts/test-select --context task` (paste the `round=… bucket=…` line).

- [ ] **Step 5: Commit.** `git add -A && git commit -m "ZEB-687: boot test proving the live on-epoch hook feeds the revoked-device projection"`

---

## Post-tasks (controller)

- Whole-branch review (opus, most-capable) over `main..HEAD`.
- Final full CI-parity sweep (`--workspace --all-targets`, `--no-fail-fast`) before PR.
- PR body: note Strategy B scope + the boot-replay-seed documented residual (7817 call-site deletion uncaught) + that the sibling `MembershipProjection` shares the same untested boot-feed (out of scope; could reuse this seam later).
