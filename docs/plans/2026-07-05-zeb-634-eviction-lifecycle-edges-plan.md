# ZEB-634 Eviction Lifecycle Edges Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the three ZEB-627 eviction residuals: record-less `Dropped` slot-creation/re-arm gate in the reconnect supervisor (items 1+3), and a MembershipProjection consult so a Leave/Kick from one community doesn't evict a peer who is still a co-member elsewhere (item 2).

**Architecture:** One gate at the top of `apply_trigger` (a `Dropped` trigger for a resolver-record-less peer removes/declines the slot instead of arming it) + one new synchronous read method on `MembershipProjection` + a consult in the lib.rs membership-consumer Leave/Kick arms. Spec: `docs/specs/2026-07-05-zeb-634-eviction-lifecycle-edges-design.md` (commit 2fbf0800) — the contract; read it first.

**Tech Stack:** Rust (tokio, paused-clock supervisor tests). No frontend, no CI, no DTO changes.

## Global Constraints

- Cargo commands run from `src-tauri/`; ONE cargo invocation at a time; always `--locked --features test-fixtures`.
- Iterative gate per task: `scripts/test-select --context task` (run from repo root; paste its `round=… bucket=…` summary line into the task report). Scoped clippy per task: `cargo clippy --locked -p harmony-app --lib --features test-fixtures -- -D warnings`. `cargo fmt --all` before each commit.
- Final sweep (Task 3 only): `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- Commit per task on branch `zeb-634-supervisor-eviction-edges`; no worktrees.
- FOREGROUND commands only (no `run_in_background`); commit before any long gate.

---

### Task 1: record-less `Dropped` gate in `apply_trigger` (items 1+3)

**Files:**
- Modify: `src-tauri/src/reconnect_supervisor.rs`

**Interfaces:**
- Consumes: `resolver.resolve_by_node_id(&peer)` (already a parameter of `apply_trigger`, used for ring markers).
- Produces: no signature changes. Behavior contract for Task 3's doc references: a `Dropped` trigger for a peer with no live resolver record never creates a slot and removes an existing one.

- [ ] **Step 1: Add the gate.** In `apply_trigger` (currently ~line 667), insert immediately after `let role = dial_role(self_node_id, &peer);` — actually BEFORE it (role is unused on the early-return path); place the gate as the first statement of the function body:

```rust
    // ZEB-634 (items 1+3): a `Dropped` kick for a peer with NO live resolver
    // record is a departure echo, not a reconnect signal — the canonical
    // source is the departing conn's drop-watcher firing after membership
    // eviction already removed both the records and the slot. Arming it
    // would recreate a slot that resolve-misses ladder to a Dormant that
    // parks for process life (the ZEB-627 residual). Removing instead of
    // re-arming also cleans inbound-conn-only peers (slot via
    // `mark_connected`, zero records — the membership-eviction path can
    // never name their node-id) at conn-drop, the only causally-available
    // moment. Record-backed kicks are exempt by construction: `NewPeer`/
    // `RecordChanged` fire only AFTER the resolver write, so a record-add
    // always re-creates the slot; a live inbound accept re-enters via
    // `mark_connected`. `remove` on an absent key is the decline-to-create
    // no-op.
    if matches!(trigger, ReconnectTrigger::Dropped)
        && resolver.resolve_by_node_id(&peer).is_none()
    {
        states.remove(&peer);
        return;
    }
```

The rest of `apply_trigger` is unchanged.

- [ ] **Step 2: Update the two stale-residual comments.**
  - `evict_peer`'s doc comment (~line 320): replace the entire `KNOWN RESIDUAL (final review 2026-07-04): …` paragraph (through `…tracked as ZEB-634.`) with:

```rust
    /// ZEB-634 closed the former residual here: a LATER kick — in
    /// particular the departing conn's drop-watcher `Dropped` — used to
    /// recreate a fresh slot that resolve-misses laddered to a
    /// process-lifetime Dormant. `apply_trigger` now declines to arm (and
    /// removes any existing slot for) a `Dropped` whose peer has no live
    /// resolver record, so the eviction is final unless the peer
    /// legitimately re-announces (record-add ⇒ `NewPeer`/`RecordChanged`)
    /// or connects inbound (`mark_connected`).
```

  Keep the trailing `Non-async and sync-context-safe, like every other handle method.` sentence.
  - Test `evict_peer_clears_pending_kick_but_not_future_ones` (~line 1713): the assertions still hold (kicks land in the dirty set unconditionally; the gate acts at drain time), but the comment describes the retired residual. Replace the test's leading comment block with:

```rust
        // ZEB-627: eviction drops the peer's PENDING dirty entry (so the loop
        // doesn't immediately resurrect the slot from a pre-eviction kick).
        // A LATER kick still lands in the dirty set — the handle can't know
        // the peer is gone — but since ZEB-634 the loop's `apply_trigger`
        // declines to arm a record-less `Dropped` at drain time (see
        // `dropped_kick_recordless_unknown_creates_no_slot`), so a landed
        // post-eviction kick no longer recreates a slot.
```

  And change the final assertion message from `"a post-eviction kick still lands (documented residual)"` to `"a post-eviction kick still lands in the dirty set (drain-time gate declines it)"`.

- [ ] **Step 3: Add three tests** to the `tests` module in `reconnect_supervisor.rs`, after `evict_peer_clears_pending_kick_but_not_future_ones` (mirror the harness idioms: `RecordingDialer`, `seed()`, `cfg()`, paused clock, `sleep(ms(500))` drain waits):

```rust
    /// ZEB-634 item 1 (the headline leak): after a membership eviction
    /// (records removed + slot evicted), the departing conn's drop-watcher
    /// `Dropped` must NOT recreate a slot — pre-fix it armed a Retrying slot
    /// that resolve-misses laddered to a process-lifetime Dormant.
    #[tokio::test(start_paused = true)]
    async fn dropped_kick_recordless_unknown_creates_no_slot() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        seed(&resolver, p);
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

        // Live peer: inbound connect, then the membership eviction pair
        // (resolver first, slot second — the lib.rs arm order).
        handle.kick(p, ReconnectTrigger::NewPeer);
        handle.mark_connected(p);
        tokio::time::sleep(ms(500)).await;
        assert_eq!(handle.states_snapshot().len(), 1, "connected slot exists");

        resolver.remove_owner(&OwnerAddr([0xAA; 16]));
        handle.evict_peer(p);
        assert!(handle.states_snapshot().is_empty(), "evicted");

        // The departing conn's drop-watcher fires AFTER eviction.
        handle.kick(p, ReconnectTrigger::Dropped);
        tokio::time::sleep(ms(500)).await;
        assert!(
            handle.states_snapshot().is_empty(),
            "ZEB-634: a record-less Dropped must not recreate a slot"
        );
    }

    /// Non-regression pin for the gate's scope: a `Dropped` for an unknown
    /// peer that DOES have a live record arms a Retrying slot as before.
    #[tokio::test(start_paused = true)]
    async fn dropped_kick_with_record_still_creates_slot() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        seed(&resolver, p);
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

        handle.kick(p, ReconnectTrigger::Dropped);
        tokio::time::sleep(ms(500)).await;
        let snap = handle.states_snapshot();
        assert_eq!(snap.len(), 1, "record-backed Dropped still arms the peer");
        assert!(
            matches!(snap[0].1, PeerStateWire::Retrying { .. }),
            "armed at the ladder, got {:?}",
            snap[0].1
        );
    }

    /// ZEB-634 item 3: an inbound-conn-only peer (slot via `mark_connected`,
    /// zero resolver records — the membership-eviction path can never name
    /// its node-id) is cleaned at conn-drop instead of laddering to a parked
    /// Dormant; a later record-add (⇒ `NewPeer` kick) re-creates the slot.
    #[tokio::test(start_paused = true)]
    async fn dropped_kick_recordless_removes_existing_slot() {
        let dialer = RecordingDialer::failing();
        let resolver = Arc::new(ReachabilityResolver::new());
        let telemetry = Arc::new(DialTelemetry::new());
        let p = peer(1);
        // NO seed: inbound-conn-only peer.
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

        handle.mark_connected(p);
        tokio::time::sleep(ms(500)).await;
        assert_eq!(handle.states_snapshot().len(), 1, "inbound slot exists");

        handle.kick(p, ReconnectTrigger::Dropped);
        tokio::time::sleep(ms(500)).await;
        assert!(
            handle.states_snapshot().is_empty(),
            "ZEB-634: record-less Dropped removes the slot (not Dormant-park)"
        );

        // Revival path: a record-add kicks NewPeer (in production via the
        // resolver's installed supervisor handle; manual here) and re-creates.
        seed(&resolver, p);
        handle.kick(p, ReconnectTrigger::NewPeer);
        tokio::time::sleep(ms(500)).await;
        assert_eq!(
            handle.states_snapshot().len(),
            1,
            "record-add revives the peer at the base rung"
        );
    }
```

- [ ] **Step 4: Gate.** From repo root: `scripts/test-select --context task` (record the summary line). Expect the always-run pass to include `test(reconnect_supervisor)` and all its tests to pass. Then from `src-tauri/`: `cargo clippy --locked -p harmony-app --lib --features test-fixtures -- -D warnings` and `cargo fmt --all`.
- [ ] **Step 5: Commit** `git add -A && git commit -m "ZEB-634: decline/remove supervisor slot on record-less Dropped (items 1+3)"`.

### Task 2: `MembershipProjection::is_joined_elsewhere` (item 2 read seam)

**Files:**
- Modify: `src-tauri/src/network_health.rs`

**Interfaces:**
- Consumes: existing `MembershipProjection` internals (`inner: Arc<RwLock<BTreeMap<SpaceId, BTreeSet<OwnerAddr>>>>`).
- Produces: `pub fn is_joined_elsewhere(&self, peer: &[u8; 16], excluding: &crate::owner_state_types::SpaceId) -> bool` — Task 3 wires it into the lib.rs Leave/Kick arms.

- [ ] **Step 1: Add the method** to `impl MembershipProjection` (after `communities_shared_with`, ~line 2213):

```rust
    /// True if `peer` is a Joined member of any community OTHER than
    /// `excluding` that the local node is Joined in. The lib.rs Leave/Kick
    /// eviction arms consult this (ZEB-634 item 2) so a departure from ONE
    /// shared community doesn't evict the reachability records and
    /// reconnect slot of a peer who is still a co-member elsewhere.
    /// `excluding` must be passed explicitly: for the SAME delta the
    /// consumer updates this projection AFTER the eviction arm runs, so the
    /// departing community's stored set is still pre-Leave (it would
    /// otherwise always match). Synchronous, poisoned lock recovered (see
    /// `set_community_members`).
    pub fn is_joined_elsewhere(
        &self,
        peer: &[u8; 16],
        excluding: &crate::owner_state_types::SpaceId,
    ) -> bool {
        let needle = crate::owner_state_types::OwnerAddr(*peer);
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        g.iter()
            .any(|(cid, members)| cid != excluding && members.contains(&needle))
    }
```

- [ ] **Step 2: Add the matrix test** to the `tests` module in `network_health.rs` (use the module's existing import style; `SpaceId`/`OwnerAddr` come from `crate::owner_state_types`):

```rust
    /// ZEB-634 item 2: the Leave/Kick consult. Peer in A+B excluding A →
    /// true (skip eviction); only-A excluding A → false (last shared
    /// community: evict); unknown peer / empty projection → false.
    #[test]
    fn is_joined_elsewhere_matrix() {
        use crate::owner_state_types::{OwnerAddr, SpaceId};
        let proj = MembershipProjection::new();
        let a = SpaceId([0xA1; 16]);
        let b = SpaceId([0xB2; 16]);
        let peer = [0x77u8; 16];
        let loner = [0x88u8; 16];

        // Empty projection: nobody is joined anywhere.
        assert!(!proj.is_joined_elsewhere(&peer, &a), "empty projection");

        let mut a_members = std::collections::BTreeSet::new();
        a_members.insert(OwnerAddr(peer));
        a_members.insert(OwnerAddr(loner));
        proj.set_community_members(a, a_members);
        let mut b_members = std::collections::BTreeSet::new();
        b_members.insert(OwnerAddr(peer));
        proj.set_community_members(b, b_members);

        assert!(
            proj.is_joined_elsewhere(&peer, &a),
            "peer shares B: leaving A must not evict"
        );
        assert!(
            proj.is_joined_elsewhere(&peer, &b),
            "symmetric: leaving B, still in A"
        );
        assert!(
            !proj.is_joined_elsewhere(&loner, &a),
            "A is loner's LAST shared community: evict"
        );
        assert!(
            !proj.is_joined_elsewhere(&[0x99u8; 16], &a),
            "unknown peer matches nothing"
        );
    }
```

- [ ] **Step 3: Gate.** `scripts/test-select --context task` from repo root (summary line into the report; always-run should include `test(network_health)` and `test(reconnect_supervisor)` from the branch diff). Then scoped clippy + `cargo fmt --all` as in Task 1.
- [ ] **Step 4: Commit** `git commit -am "ZEB-634: MembershipProjection::is_joined_elsewhere — co-membership consult seam"`.

### Task 3: Leave/Kick consult wiring + hook-mirror tests + final sweep

**Files:**
- Modify: `src-tauri/src/lib.rs` (membership-consumer Leave arm ~:5916 and Kick arm ~:5959; hook-mirror tests near `leave_delta_evicts_resolver_entries` ~:59101)

**Interfaces:**
- Consumes: `membership_projection.is_joined_elsewhere(&addr.0, &community_id)` from Task 2 (`membership_projection` is already a per-invocation clone in scope inside the consumer's `async move` block — the projection-maintenance code below the arms uses it).

- [ ] **Step 1: Wire the Leave arm.** In the `MembershipEventKind::Leave` arm, keep `community_relay_resolver.remove_advertiser(&community_id, &event.actor);` unconditional (it is community-scoped), then wrap the ENTIRE remaining block (the `departed_nodes` capture through the `emit_changed = Some(event.actor);` line) in the consult:

```rust
                                                // ZEB-634 item 2: a Leave from THIS
                                                // community must not evict a peer who
                                                // is still a Joined co-member of
                                                // another shared community —
                                                // `remove_owner` is owner-global, so
                                                // pre-consult the projection. The
                                                // departing community is excluded
                                                // explicitly (its projected set is
                                                // still pre-Leave: the consumer
                                                // refreshes the projection AFTER this
                                                // arm). Skipping leaves records, slot,
                                                // and the live conn untouched — there
                                                // is no reachability change to notify
                                                // or emit.
                                                if !membership_projection
                                                    .is_joined_elsewhere(&event.actor.0, &community_id)
                                                {
                                                    // …existing capture/remove/evict/
                                                    // notify block, unchanged…
                                                }
```

  (Indent the existing block one level; change nothing inside it.)

- [ ] **Step 2: Wire the Kick arm** identically, with `target`: `if !membership_projection.is_joined_elsewhere(&target.0, &community_id) { …existing block… }` and a one-line comment pointing at the Leave arm's rationale: `// ZEB-634 item 2: as in the Leave arm above, for the kicked target.`

- [ ] **Step 3: Add two hook-mirror tests** next to `leave_delta_evicts_resolver_entries` (same style: construct the pieces, apply the arm's decision logic verbatim, assert on resolver contents):

```rust
    /// ZEB-634 item 2: the Leave-arm consult. A peer still Joined in another
    /// shared community must survive a Leave-driven eviction — mirrors the
    /// consumer-closure hook logic (consult BEFORE remove_owner).
    #[test]
    fn leave_from_one_shared_community_skips_eviction() {
        let actor = OwnerAddr([0x77; 16]);
        let community_a = SpaceId([0xA1; 16]);
        let community_b = SpaceId([0xB2; 16]);
        let resolver = ReachabilityResolver::new();
        resolver.update(
            actor,
            crate::reachability_record::ReachabilityAnnouncePayload {
                iroh_node_id: [0x01; 32],
                home_relay_url: "https://derp.example/".into(),
                direct_addresses: vec![],
                announced_at_ms: 1000,
                identity_signature: [0; 64],
                butler_set: Vec::new(),
                bs_at: 0,
            },
            Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "a".into(),
            },
        );

        // Projection: actor Joined in BOTH A and B (pre-Leave state, as the
        // consumer sees it — the projection refreshes after the arm).
        let projection = crate::network_health::MembershipProjection::new();
        let mut members = std::collections::BTreeSet::new();
        members.insert(actor);
        projection.set_community_members(community_a, members.clone());
        projection.set_community_members(community_b, members);

        // Hook logic for Leave from A: consult, then (conditionally) evict.
        if !projection.is_joined_elsewhere(&actor.0, &community_a) {
            resolver.remove_owner(&actor);
        }
        assert_eq!(
            resolver.resolve(&actor).len(),
            1,
            "co-member elsewhere: records survive the Leave"
        );
    }

    /// ZEB-634 item 2 counterpart: the consult must not over-protect — a
    /// Leave from the peer's LAST shared community still evicts.
    #[test]
    fn leave_from_last_shared_community_still_evicts() {
        let actor = OwnerAddr([0x77; 16]);
        let community_a = SpaceId([0xA1; 16]);
        let resolver = ReachabilityResolver::new();
        resolver.update(
            actor,
            crate::reachability_record::ReachabilityAnnouncePayload {
                iroh_node_id: [0x01; 32],
                home_relay_url: "https://derp.example/".into(),
                direct_addresses: vec![],
                announced_at_ms: 1000,
                identity_signature: [0; 64],
                butler_set: Vec::new(),
                bs_at: 0,
            },
            Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "a".into(),
            },
        );

        let projection = crate::network_health::MembershipProjection::new();
        let mut members = std::collections::BTreeSet::new();
        members.insert(actor);
        projection.set_community_members(community_a, members);

        if !projection.is_joined_elsewhere(&actor.0, &community_a) {
            let n = resolver.remove_owner(&actor);
            assert_eq!(n, 1, "eviction ran");
        }
        assert!(
            resolver.resolve(&actor).is_empty(),
            "last shared community: eviction proceeds as before"
        );
    }
```

  Place them inside the same `#[cfg(test)]` module as `leave_delta_evicts_resolver_entries`, reusing its imports (`OwnerAddr`, `SpaceId`, `Hlc` are already in scope there; add `use` lines only if the compiler asks).

- [ ] **Step 4: Iterative gate.** `scripts/test-select --context task` from repo root (summary line into the report). Scoped clippy + fmt as before.
- [ ] **Step 5: Final sweep** (this branch's last task — CI-parity, from `src-tauri/`, ONE at a time):
  - `cargo fmt --all -- --check`
  - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
  Expected: all green (frontend untouched — tsc/vitest run in CI regardless).
- [ ] **Step 6: Commit** `git commit -am "ZEB-634: consult co-membership before Leave/Kick eviction (item 2)"`.
