# Small-Fixes Bundle (ZEB-643/642/637/625) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One bundled PR closing four independent Low residuals: the
NewPeer/eviction interleave (ZEB-643), DM-invite purge symmetrization
(ZEB-642), the self row in network-health peers (ZEB-637), and the
kick-vs-floor invariant test pins (ZEB-625).

**Architecture:** Four self-contained tasks with no cross-task interfaces.
Spec: `docs/specs/2026-07-05-small-fixes-bundle-design.md` (committed
8df87513) — it is the contract; this plan is the mechanical realization.

**Tech Stack:** Rust (src-tauri), tokio paused-time tests, one shell-comment
edit. No frontend changes.

## Global Constraints

- Branch: `zeb-625-637-642-643-small-fixes`. One commit per task,
  commit-before-gate.
- Cargo commands run from `src-tauri/`; ONE cargo invocation at a time.
- Per-task iterative gate: `scripts/test-select --context task` from the
  repo root (git-add new/changed test files FIRST — untracked files are
  invisible to the always-run set). Paste the printed `round=… bucket=…`
  summary into the task report.
- Per-task lint gate: `cargo clippy --locked -p harmony-app --all-targets
  --features test-fixtures --no-deps -- -D warnings` then `cargo fmt --all`
  (both from `src-tauri/`). (`--all-targets` is load-bearing for inline
  `#[cfg(test)]` code — Qodo PR #406 R1; matches what the tasks ran.)
- Final sweep (after Task 4, controller-run): `cargo fmt --all -- --check`;
  `cargo clippy --locked --all-targets --features test-fixtures --no-deps
  -- -D warnings`; `cargo nextest run --locked --workspace --all-targets
  --features test-fixtures`.
- Timing tests: tokio paused time ONLY (`#[tokio::test(start_paused =
  true)]`, `tokio::time::advance`) — no real sleeps, budgets are absolute
  virtual deadlines.
- Keychain rules apply if anything touches identity persistence (nothing
  here does).

---

### Task 1: ZEB-643 — `remove_owner` returns the removed node-ids

**Files:**
- Modify: `src-tauri/src/reachability_resolver.rs:593-617` (method) + count
  asserts in its tests (`:919-925`, `:1199-1200`, `:1214-1219`, one more
  near `:1475` — the compiler will flag it) + one new test after `:1220`
- Modify: `src-tauri/src/lib.rs` Leave arm `~:5953-5964`, Kick arm
  `~:6005-6012`; test call sites `:59233`, `:59280`, `:59320`, `:59389`
- Modify: `src-tauri/src/reconnect_supervisor.rs` — one new test after
  `dropped_kick_recordless_unknown_creates_no_slot` (`:1808`)

**Interfaces:**
- Produces: `ReachabilityResolver::remove_owner(&self, actor: &OwnerAddr)
  -> Vec<[u8; 32]>` (was `-> usize`). No other task consumes it.

- [ ] **Step 1: Change `remove_owner`** (`reachability_resolver.rs:593`).
  Replace the whole method (keep/extend its existing doc comment with the
  new contract paragraph below):

```rust
    /// Remove ALL device records for `actor`, returning the iroh node-ids
    /// of the removed records. ZEB-643: the decide-and-remove pair executes
    /// under a single write-lock hold, so the returned set is the
    /// authoritative deleted set — a concurrent `update` for the same owner
    /// cannot slip a record between a caller's separate capture read and
    /// this removal (the old resolve→remove_owner two-step raced exactly
    /// that way, leaking a record-less supervisor slot via the pending
    /// NewPeer kick of the uncaptured device). Callers evict supervisor
    /// slots from the returned set.
    pub fn remove_owner(&self, actor: &OwnerAddr) -> Vec<[u8; 32]> {
        let mut map = self.inner.write().expect("resolver write lock");
        let to_remove: Vec<ResolverKey> = map
            .range((*actor, [0u8; 32])..=(*actor, [0xFFu8; 32]))
            .map(|(k, _)| *k)
            .collect();
        for k in &to_remove {
            map.remove(k);
        }
        if !to_remove.is_empty() {
            // ZEB-627: departed devices left the derived views.
            self.generation
                .fetch_add(1, std::sync::atomic::Ordering::Release);
        }
        drop(map);
        // Full per-owner eviction (ZEB-621): drop the stale-refresh cooldown
        // too, else the map grows monotonically with every stale-dispatched
        // owner over process lifetime.
        self.refresh_cooldowns
            .lock()
            .expect("refresh_cooldowns lock")
            .remove(actor);
        to_remove.into_iter().map(|(_, node_id)| node_id).collect()
    }
```

- [ ] **Step 2: Update the Leave arm** (`lib.rs`, currently `:5953-5964`).
  DELETE the `departed_nodes` pre-capture (the ZEB-627 capture comment + the
  `resolver.resolve(&event.actor)…collect()` statement) and the
  `let n = resolver.remove_owner(&event.actor);` + `if n > 0 {` pair;
  replace with:

```rust
                                                    // ZEB-643: `remove_owner` returns the node-ids it
                                                    // deleted, captured atomically under the same
                                                    // write-lock hold as the removal — a concurrent
                                                    // device announce can no longer slip a record
                                                    // between a separate capture read and the
                                                    // destructive write (the old resolve→remove_owner
                                                    // two-step). Evicting exactly the returned set
                                                    // also clears each node's pending supervisor kick.
                                                    let departed_nodes = resolver.remove_owner(&event.actor);
                                                    if !departed_nodes.is_empty() {
```

  Everything inside the block (the ZEB-627 evict loop over
  `&departed_nodes`, the ZEB-329 notify, `emit_changed = Some(event.actor);`)
  stays byte-identical.

- [ ] **Step 3: Update the Kick arm** (`lib.rs`, currently `:6005-6012`).
  Same transformation with `target`: delete the capture (the "ZEB-627:
  capture-then-evict, as in the Leave arm above." comment + the
  `resolver.resolve(target)…collect()` statement), replace
  `let n = resolver.remove_owner(target); if n > 0 {` with:

```rust
                                                    // ZEB-643: as in the Leave arm above.
                                                    let departed_nodes = resolver.remove_owner(target);
                                                    if !departed_nodes.is_empty() {
```

- [ ] **Step 4: Mechanically update count-assert call sites.** The return
  type change breaks these; convert each count use to `.len()`:
  - `reachability_resolver.rs:919-920`: `let n = r.remove_owner(&actor);
    assert_eq!(n, 1);` → `assert_eq!(r.remove_owner(&actor).len(), 1);`
  - `reachability_resolver.rs:924`: `assert_eq!(r.remove_owner(&OwnerAddr([0xBB; 16])), 0);`
    → `assert_eq!(r.remove_owner(&OwnerAddr([0xBB; 16])).len(), 0);`
  - `reachability_resolver.rs:1199-1200`: `let removed = r.remove_owner(&actor);
    assert_eq!(removed, 3, …);` → `assert_eq!(removed.len(), 3, …);`
  - `reachability_resolver.rs:1214/1217/1219` (`remove_owner_is_idempotent`):
    wrap each `assert_eq!(r.remove_owner(…), N)` as `.len(), N`.
  - `lib.rs:59233` (asserts n==2), `lib.rs:59320` (n==1), `lib.rs:59389`
    (n==1): change the assert to `n.len()`, keeping messages.
  - `lib.rs:59280` and `reconnect_supervisor.rs:1797` discard the return —
    no change needed.
  - Run `cargo check --locked -p harmony-app --features test-fixtures`
    (from `src-tauri/`) and fix ANY remaining count-assert the compiler
    flags the same way (there is at least one more near
    `reachability_resolver.rs:1475`).

- [ ] **Step 5: New resolver unit test** — insert after
  `remove_owner_is_idempotent` (`reachability_resolver.rs:~1220`), same
  module (uses that module's `make_payload`/`make_hlc`):

```rust
    /// ZEB-643: `remove_owner` returns the node-ids of the records it
    /// deleted — the authoritative set, captured under the same write-lock
    /// hold as the removal (callers evict supervisor slots from it).
    #[test]
    fn remove_owner_returns_removed_node_ids() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        let other = OwnerAddr([0x22; 16]);
        r.update(actor, make_payload(0x01, 1000), make_hlc(1000, 0, "a"));
        r.update(actor, make_payload(0x02, 1000), make_hlc(1000, 0, "b"));
        r.update(other, make_payload(0xCC, 1000), make_hlc(1000, 0, "c"));

        let mut removed = r.remove_owner(&actor);
        removed.sort();
        assert_eq!(
            removed,
            vec![[0x01; 32], [0x02; 32]],
            "returns exactly the deleted node-ids"
        );
        assert_eq!(r.resolve(&other).len(), 1, "other owner untouched");
        assert!(
            r.remove_owner(&actor).is_empty(),
            "second remove returns the empty set"
        );
    }
```

- [ ] **Step 6: New supervisor test** — insert after
  `dropped_kick_recordless_unknown_creates_no_slot`
  (`reconnect_supervisor.rs:~1808`). Note `seed(&resolver, p)` seeds node
  `p` under the fixed owner `OwnerAddr([0xAA; 16])`, so calling it twice
  with different node-ids models one owner with two devices. Current-thread
  paused-time runtime: the spawned loop only runs at the test's await
  points, so the kick queued between the two non-await statements is
  deterministically still pending at evict time.

```rust
    /// ZEB-643: the eviction pair evicts EXACTLY the node-ids
    /// `remove_owner` deleted. Interleave under pin: a second device (n2)
    /// announces after the arm's would-be capture point but before
    /// `remove_owner`; evicting the RETURNED set clears n2's pending
    /// NewPeer kick (pre-fix, evicting only a stale pre-captured set left
    /// that kick to drain record-less into a process-lifetime Dormant).
    #[tokio::test(start_paused = true)]
    async fn eviction_from_removed_set_clears_interleaved_newpeer_kick() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let n1 = peer(1);
        let n2 = peer(2);
        seed(&resolver, n1);
        let handle = SupervisorHandle::new();
        let config = cfg(1_000, 64_000, 3_600_000, 30_000, 4, 3_000);
        tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer.clone(),
            resolver.clone(),
            telemetry.clone(),
            peer(0),
            config,
        ));

        // n1 is a live, connected peer with a slot.
        handle.kick(n1, ReconnectTrigger::NewPeer);
        handle.mark_connected(n1);
        tokio::time::sleep(ms(500)).await;
        assert_eq!(handle.states_snapshot().len(), 1, "connected slot exists");

        // The interleave: a NEW device (n2) lands its record + NewPeer kick
        // after the would-be capture point. No awaits until the eviction
        // pair below, so the kick is still pending when we evict.
        seed(&resolver, n2);
        handle.kick(n2, ReconnectTrigger::NewPeer);
        assert!(handle.pending_trigger(n2).is_some(), "n2 kick queued");

        // The lib.rs arm shape (ZEB-643): evict the RETURNED set.
        let removed = resolver.remove_owner(&OwnerAddr([0xAA; 16]));
        assert_eq!(removed.len(), 2, "both devices deleted");
        for node in &removed {
            handle.evict_peer(*node);
        }
        assert!(
            handle.pending_trigger(n2).is_none(),
            "ZEB-643: the interleaved NewPeer kick is cleared by eviction"
        );

        tokio::time::sleep(ms(500)).await;
        assert!(
            handle.states_snapshot().is_empty(),
            "no slot survives or resurrects after the eviction pair"
        );
    }
```

- [ ] **Step 7: Gate.** From `src-tauri/`: `cargo check --locked -p
  harmony-app --features test-fixtures` (clean), then targeted tests:
  `cargo nextest run --locked -p harmony-app --features test-fixtures -E
  'test(remove_owner) or test(eviction_from_removed_set) or test(dropped_kick) or test(zeb_321_event_loop)'`
  — all pass, incl. the updated count asserts.

- [ ] **Step 8: Commit.** `git add -A && git commit` — message:
  `ZEB-643: remove_owner returns removed node-ids — atomic eviction set (interleave fix)`.

- [ ] **Step 9: Iterative gate.** Repo root: `scripts/test-select --context
  task`; then from `src-tauri/`: the per-task clippy + `cargo fmt --all`.
  If fmt changed files, amend the commit.

---

### Task 2: ZEB-642 — DM-invite purge symmetrization (items 1–3)

**Files:**
- Modify: `src-tauri/src/dm_inbox_ingest.rs:532-537` and `:1008-1014`
  (arms), `:826` area (flag doc comment)
- Modify: `src-tauri/src/community_relay_prod.rs:471-476` (arm), `:519`
  area (flag doc comment)
- Modify: `src-tauri/src/dm_outbox.rs` — one new test after
  `non_friend_invite_for_existing_space_is_ignored_not_staged` (`:~6025`)

**Interfaces:**
- Consumes: `crate::pending_dm_invites::purge_stale_staged_on_accept(
  Option<&Arc<PendingDmInvites>>, &dyn NodeEventSink, &SpaceId)` — existing
  helper; call it exactly as the adjacent `Accepted` arm in each match does.

- [ ] **Step 1: Tunnel-ingest arm** (`dm_inbox_ingest.rs:532-537`). Replace
  the arm with (comment replaces the old `— no-op.` line):

```rust
                // ZEB-639: non-friend invite for a space we already hold.
                // ZEB-642 (1): a staged row for this space is stale by
                // definition once the space exists (the same argument that
                // blessed the co-deposit Ok(None) conflation) — purge it.
                // The helper emits dm-invite-list-changed only on actual
                // removal, so redeliveries stay event-quiet.
                crate::dm_outbox::ApplyInviteOutcome::IgnoredExistingSpace => {
                    tracing::debug!(
                        space_id = ?invite_space_id,
                        "tunnel invite ignored: space already exists locally (non-friend inviter)"
                    );
                    crate::pending_dm_invites::purge_stale_staged_on_accept(
                        pending_invites.as_ref(),
                        sink.as_ref(),
                        &invite_space_id,
                    );
                }
```

- [ ] **Step 2: Deposit-recover arm** (`dm_inbox_ingest.rs:1008-1014`).
  Same transformation with this site's handles and its `Ok(())` preserved:

```rust
            // ZEB-639: non-friend invite for a space we already hold.
            // ZEB-642 (1): purge the stale staged row (see tunnel arm).
            crate::dm_outbox::ApplyInviteOutcome::IgnoredExistingSpace => {
                tracing::debug!(
                    space_id = ?invite_space_id,
                    "deposited invite ignored: space already exists locally (non-friend inviter)"
                );
                crate::pending_dm_invites::purge_stale_staged_on_accept(
                    self.pending_dm_invites.as_ref(),
                    self.sink.as_ref(),
                    &invite_space_id,
                );
                Ok(())
            }
```

- [ ] **Step 3: Relay-recover arm** (`community_relay_prod.rs:471-476`):

```rust
                // ZEB-639: non-friend invite for a space we already hold.
                // ZEB-642 (1): purge the stale staged row (see the
                // dm_inbox_ingest tunnel arm).
                crate::dm_outbox::ApplyInviteOutcome::IgnoredExistingSpace => {
                    tracing::debug!(
                        space_id = ?invite_space_id,
                        "relay invite ignored: space already exists locally (non-friend inviter)"
                    );
                    crate::pending_dm_invites::purge_stale_staged_on_accept(
                        self.pending_dm_invites.as_ref(),
                        self.sink.as_ref(),
                        &invite_space_id,
                    );
                }
```

- [ ] **Step 4: Skip-window doc comments (item 2).** Append ONE line to each
  existing `purge_stale_staged` flag comment block:
  - `dm_inbox_ingest.rs` (block ending just above
    `let mut purge_stale_staged = false;` at `:826`), append:
    `// ZEB-642 (2): dm_inbox's skip-window CLOSES BEFORE blob↔packet binding — the purge at lock-drop precedes step 4, so a binding Err cannot skip it.`
  - `community_relay_prod.rs` (block above `:519`), append:
    `// ZEB-642 (2): relay's skip-window EXTENDS THROUGH apply_inbox — the lock scope encloses blob-binding/decrypt/apply_inbox, any of whose Err returns skips the purge until the next delivery.`

- [ ] **Step 5: Tombstone-staging test pin (item 3)** — insert after
  `non_friend_invite_for_existing_space_is_ignored_not_staged`
  (`dm_outbox.rs:~6025`, same module — `build_valid_dm_invite`'s fixture
  space is `SpaceId([7; 16])`):

```rust
    /// ZEB-642 (3): a TOMBSTONED space is NOT in `state.spaces`, so a
    /// non-friend invite for it still STAGES (consent re-asked; accept
    /// later surfaces the permanent tombstone rejection). Pins the
    /// `spaces.contains_key` gate comment in `apply_invite`.
    #[test]
    fn non_friend_invite_for_tombstoned_space_still_stages() {
        let self_owner = OwnerAddr([0xaa; 16]);
        let mut state = OwnerState::default(); // friend_graph EMPTY → non-friend tier

        // Arrange: the invite's target space is tombstoned (removed from
        // `spaces`, held only in `tombstones`).
        state.tombstone_space(crate::owner_state_types::SpaceId([7; 16]));
        let before = crate::owner_state_persist::canonicalize(&state).unwrap();

        let (signed, signature, body_bytes) = build_valid_dm_invite(self_owner);
        let outcome = apply_invite(
            &mut state,
            self_owner,
            "dev",
            signed,
            signature,
            &body_bytes,
            4242,
            None,
            true,
        )
        .unwrap();

        assert!(
            matches!(outcome, ApplyInviteOutcome::Staged(_)),
            "tombstoned space must still stage, got {outcome:?}"
        );
        let after = crate::owner_state_persist::canonicalize(&state).unwrap();
        assert_eq!(before, after, "staging must write NOTHING");
    }
```

  (If `SpaceId` is already imported unprefixed in the test module, drop the
  path prefix to match local style.)

- [ ] **Step 6: Gate.** From `src-tauri/`: `cargo nextest run --locked -p
  harmony-app --features test-fixtures -E
  'test(tombstoned) or test(non_friend_invite) or test(purge_stale) or test(ignored)'`
  — all pass.

- [ ] **Step 7: Commit.** Message: `ZEB-642: symmetrize staged-invite purge
  into direct IgnoredExistingSpace arms + skip-window docs + tombstone pin`.

- [ ] **Step 8: Iterative gate.** Repo root `scripts/test-select --context
  task`; per-task clippy + `cargo fmt --all`; amend if fmt changed files.

---

### Task 3: ZEB-637 — filter the self row out of snapshot peers[]

**Files:**
- Modify: `src-tauri/src/network_health.rs` — filter fn `:553`, service
  struct `~:824-859`, a new setter after `set_protocol_compat_source`
  (`:913-918`), snapshot call `:1087`, test call sites (`:2386, :2593,
  :2607, :2621, :~2641, :~2656` — every existing
  `filter_peers_by_shared_membership(` call adds a `None` third arg), one
  new filter test, one new/extended snapshot test near `:3016`
- Modify: `src-tauri/src/lib.rs:~9906` (construction site wiring)
- Modify: `scripts/gce-xwan/run-tests.sh:286-295` (comment only)

**Interfaces:**
- Produces: `filter_peers_by_shared_membership(records, memb,
  self_owner: Option<&[u8; 16]>, now_ms)` (3rd param inserted before
  `now_ms`); `NetworkHealthService::set_self_owner(&mut self, owner:
  [u8; 16])`. No other task consumes them.

- [ ] **Step 1: Extend the filter fn** (`network_health.rs:553`). New
  signature + skip at the top of the record loop:

```rust
pub fn filter_peers_by_shared_membership(
    resolver_records: Vec<ResolverPeerRecord>,
    my_memberships: &dyn MyMembershipSet,
    self_owner: Option<&[u8; 16]>,
    now_ms: u64,
) -> Vec<PeerHealth> {
    let mut out: Vec<PeerHealth> = Vec::new();
    for r in resolver_records {
        // ZEB-637: the node's own announce lands in its own resolver (the
        // membership consumer is self-blind by design) and the projection
        // "shares" every community with self — so without this skip the
        // snapshot grows a permanent self row at noConnection (no
        // connection source ever keys on self). Filter it here so every
        // peers[] consumer (panel, e2e asserts, GCE suite) sees peers only.
        if self_owner.is_some_and(|s| r.owner_addr == *s) {
            continue;
        }
        let shared = my_memberships.communities_shared_with(&r.owner_addr);
        // …rest of the loop and the sort UNCHANGED…
```

- [ ] **Step 2: Service field + setter.** Add `self_owner: Option<[u8; 16]>`
  to the `NetworkHealthService` struct, initialize `self_owner: None` in
  `new()`, and add after `set_protocol_compat_source` (`:918`):

```rust
    /// ZEB-637: install the local node's own OwnerAddr so `snapshot` can
    /// filter the self row out of `peers[]`. Called once at boot when an
    /// identity is loaded; when unset (no identity yet) no filtering
    /// happens — additive like the other `set_*` sources.
    pub fn set_self_owner(&mut self, owner: [u8; 16]) {
        self.self_owner = Some(owner);
    }
```

  Update the snapshot call at `:1087` to
  `filter_peers_by_shared_membership(records, &*self.membership, self.self_owner.as_ref(), now)`.

- [ ] **Step 3: Wire at the construction site** (`lib.rs`, immediately
  after the `NetworkHealthService::new(…)` call ending `:9906`):

```rust
                            // ZEB-637: the panel's peers[] must not list the
                            // node's own owner (a permanent noConnection row
                            // — bit the GCE suite and both flag-day agents).
                            if let Some(self_owner) = guard.dm_self_owner {
                                nh.set_self_owner(self_owner.0);
                            }
```

- [ ] **Step 4: Update existing filter-test call sites.** Every existing
  test call `filter_peers_by_shared_membership(records, &memb, NOW)` gains
  `None` as the new third arg (sites listed in **Files**; `cargo check`
  confirms none is missed).

- [ ] **Step 5: New filter test** — insert after
  `filter_peers_excludes_peers_with_no_shared_community` (`:~2610`):

```rust
    /// ZEB-637: the self owner's record is dropped from peers[]; `None`
    /// (no identity loaded) keeps the unfiltered behavior.
    #[test]
    fn filter_peers_drops_self_owner_row() {
        let records = vec![
            make_record(0x11, ConnectionMode::NoConnection, Some(1000)),
            make_record(0x22, ConnectionMode::Direct, Some(2000)),
        ];
        let mut table = std::collections::HashMap::new();
        table.insert([0x11u8; 16], vec!["comm-a".to_string()]);
        table.insert([0x22u8; 16], vec!["comm-a".to_string()]);
        let memb = FakeMembership { table };

        let self_owner = [0x11u8; 16];
        let out = filter_peers_by_shared_membership(
            records.clone(),
            &memb,
            Some(&self_owner),
            5000,
        );
        assert_eq!(out.len(), 1, "self row dropped");
        assert_eq!(out[0].owner_addr, hex::encode([0x22u8; 16]));

        let out = filter_peers_by_shared_membership(records, &memb, None, 5000);
        assert_eq!(out.len(), 2, "no identity → no filtering");
    }
```

- [ ] **Step 6: Snapshot-level pin.** Copy
  `snapshot_with_three_peers_sorted_by_last_seen_desc` (`:3016`) as
  `snapshot_filters_self_owner_from_peers`: identical fixture, plus call
  `nh.set_self_owner(<one of the fixture's three owner byte-arrays>)`
  before `snapshot()`, and assert `snap.peers.len()` is one fewer and no
  remaining row's `owner_addr` equals that owner's hex. (Existing snapshot
  tests set no self owner and stay untouched — Option-gated behavior.)

- [ ] **Step 7: GCE suite comment refresh**
  (`scripts/gce-xwan/run-tests.sh:286-295`, comment only — script logic
  unchanged). Replace the sentence
  `snapshots include other rows (e.g. a self ownerAddr entry at noConnection), so an any-peer check could false-PASS/FAIL.`
  with
  `the self ownerAddr row is filtered as of ZEB-637; peer-scoping retained as belt-and-braces against future extra rows.`

- [ ] **Step 8: Gate.** From `src-tauri/`: `cargo nextest run --locked -p
  harmony-app --features test-fixtures -E
  'test(filter_peers) or test(snapshot) or test(membership_projection)'` —
  all pass.

- [ ] **Step 9: Commit.** Message: `ZEB-637: filter self owner out of
  network_health_snapshot peers[]`.

- [ ] **Step 10: Iterative gate.** Repo root `scripts/test-select --context
  task`; per-task clippy + `cargo fmt --all`; amend if fmt changed files.

---

### Task 4: ZEB-625 — kick-vs-floor invariant paused-time tests (test-only)

**Files:**
- Modify: `src-tauri/src/channel_backfill.rs` — three new tests in
  `mod tests` (`:1183`), placed with the ZEB-618 cluster (after
  `root_driver_persisted_floor_first_fire_at_deadline`, `:~2931`)

**Interfaces:**
- Consumes (all existing, test-only): `run_backfill_driver` /
  `run_root_fetch_driver` (arg orders below are pinned by the neighboring
  tests), `ResyncPersist`, `EPOCH_REARM_COOLDOWN_MS`, `PageFetch`,
  `RootFetch`, `BackfillLatch`, `RootFetchLatch`.

- [ ] **Step 1: Backfill invariant test.** Insert with the ZEB-618 cluster:

```rust
    /// ZEB-625 (1): a presence kick re-arms a FULL reconcile but must NOT
    /// advance the persisted resync floor — `on_full_reconcile` stays
    /// uncalled and the floor still fires at the ORIGINAL absolute
    /// deadline. (Kick arms do `latch.reset` only; only the resync_tick
    /// arm persists — the invariant was code-verified but never pinned.)
    #[tokio::test(start_paused = true)]
    async fn backfill_presence_kick_does_not_advance_persisted_floor() {
        const DEADLINE_MS: u64 = 200_000;
        const INTERVAL_MS: u64 = 500_000;
        let sinces: Arc<StdMutex<Vec<Option<Hlc>>>> = Arc::new(StdMutex::new(Vec::new()));
        let since_log = Arc::clone(&sinces);
        let request_page = move |since: Option<Hlc>| {
            let since_log = Arc::clone(&since_log);
            async move {
                since_log.lock().unwrap().push(since);
                PageFetch::Completed(0, 256) // short page → satisfied → park
            }
        };
        let fired: Arc<StdMutex<Vec<u64>>> = Arc::new(StdMutex::new(Vec::new()));
        let fired_cb = Arc::clone(&fired);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (presence_tx, presence_rx) = tokio::sync::watch::channel(0u64);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_backfill_driver(
            BackfillLatch::new(None),
            request_page,
            || async { None::<Hlc> },
            shutdown_rx,
            None,              // no transport-epoch watch — isolate presence vs floor
            Some(presence_rx), // presence kick wired
            Some(INTERVAL_MS),
            move || start.elapsed().as_millis() as u64,
            Some(ResyncPersist {
                first_deadline_ms: DEADLINE_MS,
                on_full_reconcile: Arc::new(move |ts| fired_cb.lock().unwrap().push(ts)),
            }),
        ));
        // Req #1 at spawn satisfies → parks on Idle.
        while sinces.lock().unwrap().len() < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Presence kick past the re-arm cooldown → req #2 (full reconcile).
        tokio::time::advance(Duration::from_millis(EPOCH_REARM_COOLDOWN_MS + 1)).await;
        presence_tx.send(1).expect("presence bump");
        for _ in 0..128 {
            if sinces.lock().unwrap().len() >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(sinces.lock().unwrap().len(), 2, "kick re-armed a request");
        assert!(
            fired.lock().unwrap().is_empty(),
            "ZEB-625: a presence kick must NOT invoke on_full_reconcile"
        );
        // The floor still fires at the ORIGINAL absolute deadline: just shy
        // → nothing; cross it → exactly one persisted fire.
        let now = start.elapsed().as_millis() as u64;
        tokio::time::advance(Duration::from_millis(DEADLINE_MS - 1 - now)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            fired.lock().unwrap().is_empty(),
            "floor must not fire early — the kick moved nothing"
        );
        tokio::time::advance(Duration::from_millis(2)).await;
        for _ in 0..128 {
            if !fired.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let stamps = fired.lock().unwrap();
        assert_eq!(stamps.len(), 1, "exactly one floor fire");
        assert!(
            stamps[0] >= DEADLINE_MS,
            "fire at the original absolute deadline (kick did not advance it)"
        );
        driver.abort();
    }
```

- [ ] **Step 2: Root invariant test** (same shape on
  `run_root_fetch_driver`; template harness =
  `root_driver_persisted_floor_first_fire_at_deadline` `:2868` + the kick
  pattern of `root_driver_presence_kick_rearms` `:2818`):

```rust
    /// ZEB-625 (1): root-driver twin of
    /// `backfill_presence_kick_does_not_advance_persisted_floor`.
    #[tokio::test(start_paused = true)]
    async fn root_presence_kick_does_not_advance_persisted_floor() {
        const DEADLINE_MS: u64 = 200_000;
        const INTERVAL_MS: u64 = 500_000;
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_root = move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                RootFetch::Answered // satisfied → parks
            }
        };
        let fired: Arc<StdMutex<Vec<u64>>> = Arc::new(StdMutex::new(Vec::new()));
        let fired_cb = Arc::clone(&fired);
        let (kick_tx, kick_rx) = tokio::sync::watch::channel(0u64);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            None,          // no epoch watch
            Some(kick_rx), // presence kick wired
            Some(INTERVAL_MS),
            move || start.elapsed().as_millis() as u64,
            Some(ResyncPersist {
                first_deadline_ms: DEADLINE_MS,
                on_full_reconcile: Arc::new(move |ts| fired_cb.lock().unwrap().push(ts)),
            }),
        ));
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Presence kick past the cooldown → req #2; no persist.
        tokio::time::advance(Duration::from_millis(EPOCH_REARM_COOLDOWN_MS + 1)).await;
        kick_tx.send_modify(|e| *e = e.wrapping_add(1));
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        assert!(
            fired.lock().unwrap().is_empty(),
            "ZEB-625: a presence kick must NOT invoke on_full_reconcile"
        );
        // Floor fires at the ORIGINAL absolute deadline.
        let now = start.elapsed().as_millis() as u64;
        tokio::time::advance(Duration::from_millis(DEADLINE_MS - 1 - now)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            fired.lock().unwrap().is_empty(),
            "floor must not fire early — the kick moved nothing"
        );
        tokio::time::advance(Duration::from_millis(2)).await;
        for _ in 0..128 {
            if !fired.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let stamps = fired.lock().unwrap();
        assert_eq!(stamps.len(), 1, "exactly one floor fire");
        assert!(
            stamps[0] >= DEADLINE_MS,
            "fire at the original absolute deadline (kick did not advance it)"
        );
        driver.abort();
    }
```

- [ ] **Step 3: Root WaitUntil (mid-backoff) presence re-arm test:**

```rust
    /// ZEB-625 (2): a presence kick arriving MID-BACKOFF (the root
    /// driver's WaitUntil arm) re-arms the latch after the cooldown —
    /// direct coverage for the arm previously validated only by
    /// mirror-faithfulness with the backfill driver.
    #[tokio::test(start_paused = true)]
    async fn root_presence_kick_mid_backoff_rearms() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_root = move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                RootFetch::NoReply // unanswered → the latch backs off (WaitUntil)
            }
        };
        let (kick_tx, kick_rx) = tokio::sync::watch::channel(0u64);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            None,          // no epoch watch
            Some(kick_rx), // presence kick wired
            None,          // no floor — isolate the WaitUntil presence arm
            move || start.elapsed().as_millis() as u64,
            None,
        ));
        // Req #1 fires and goes unanswered → the driver parks in WaitUntil.
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Mid-backoff presence kick; the cooldown defers the re-query
        // until we advance past it.
        kick_tx.send_modify(|e| *e = e.wrapping_add(1));
        tokio::time::advance(Duration::from_millis(EPOCH_REARM_COOLDOWN_MS + 1)).await;
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        assert!(
            requests.load(Ordering::SeqCst) >= 2,
            "a mid-backoff presence kick must re-arm the root fetch"
        );
        driver.abort();
    }
```

- [ ] **Step 4: Gate.** From `src-tauri/`: `cargo nextest run --locked -p
  harmony-app --features test-fixtures -E
  'test(presence_kick) or test(persisted_floor) or test(mid_backoff)'` —
  all pass (new + neighboring existing).

- [ ] **Step 5: Commit.** Message: `ZEB-625: pin kick-vs-floor invariant
  with paused-time tests (both drivers + WaitUntil re-arm)`.
  Note in the commit body: the ticket's optional mark_unchanged polish was
  dropped — premise refuted (no such precedent in-tree; see spec §4).

- [ ] **Step 6: Iterative gate.** Repo root `scripts/test-select --context
  task`; per-task clippy + `cargo fmt --all`; amend if fmt changed files.
