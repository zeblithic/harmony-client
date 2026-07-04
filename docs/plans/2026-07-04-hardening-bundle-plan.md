# Hardening Bundle (ZEB-627/628/629/630/633) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the five approved post-review hardening fixes as one PR: outbound link supersession recheck, membership-driven supervisor eviction, generation-keyed zid→node cache, Degraded rollup tier, unified settings write lock, iroh relay RMW tests, and the redeem-timeout flake fix.

**Architecture:** Six independent surgical changes in existing modules (no new files); each lands with its own tests and commit. Spec: `docs/specs/2026-07-04-hardening-bundle-design.md` (commit `9c7ac481`).

**Tech Stack:** Rust (tokio, zenoh, iroh), nextest, existing test harnesses (`start_paused` supervisor tests, tempdir settings tests).

## Global Constraints

- Branch: `zeb-627-633-hardening-bundle`; commit per task.
- ONE cargo invocation at a time, from `src-tauri/`.
- Clippy gate (CI form): `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`.
- fmt gate: `cargo fmt --all -- --check`.
- Per-task test runs use `-p harmony-app --lib` where the tests are unit tests (relink cost); integration-test targets named explicitly. One full `--workspace --all-targets --features test-fixtures --no-fail-fast` sweep at the end (background + caffeinate if >10min).
- Frontend gates from repo root: `npx tsc --noEmit`, `npx vitest run` (no frontend changes expected; run anyway).
- Do not touch `.config/nextest.toml`, `vendor/`, or any file outside the sites below.
- Last-relay rejection message must stay byte-identical: `"cannot remove the last iroh relay; use reset to follow the built-in defaults instead"`.

---

### Task 1: ZEB-628 — `ConnectionMode::Degraded` joins the Relay rollup tier

**Files:**
- Modify: `src-tauri/src/network_health.rs:496-523` (fn + doc), `:2419-2443` (test template neighbor), `:3487-3498` (e2e assert)

**Interfaces:**
- Consumes: `ConnectionMode`, `ReachabilityStatus` (both in this file).
- Produces: nothing used by later tasks.

- [ ] **Step 1: Write the failing unit test** — insert after `derive_reachability_status_degraded_when_only_relay` (ends line 2443):

```rust
    #[test]
    fn derive_reachability_status_degraded_when_only_peer_signal_degraded() {
        // ZEB-628: a peer whose only signal is liveness `Degraded` (live link,
        // no selected path yet) is degraded-reachable, NOT unreachable.
        let my = MyNetworkSummary {
            iroh_node_id: "deadbeef".into(),
            reachability: ReachabilityStatus::Unreachable, // ignored
            nat_classification: NatClass::Unknown,
            home_relay_url: None,
            relay_rtt_ms: None,
            direct_addresses: vec![],
        };
        let peers = vec![PeerHealth {
            owner_addr: "abcd".into(),
            display_name: None,
            shared_communities: vec![],
            connection_mode: ConnectionMode::Degraded,
            rtt_ms: None,
            last_seen_ms: None,
            reachability_record_age_ms: None,
            protocol_incompat_reason: None,
        }];
        assert_eq!(
            derive_reachability_status(&my, &peers),
            ReachabilityStatus::Degraded
        );
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(derive_reachability_status_degraded_when_only_peer_signal_degraded)'`
Expected: FAIL — left `Unreachable`, right `Degraded`.

- [ ] **Step 3: Implement** — replace the second arm (lines 510-514) and update the doc comment (496-500):

```rust
/// Spec §4.1 + ZEB-628: derive top-level reachability from my own state +
/// peer set. Reachable: at least one peer is Direct-connected (or no peers
/// yet — our own endpoint works; others' reachability is unknown, not
/// failing). Degraded: best peer signal is Relay OR liveness-Degraded (a
/// live link without a selected path is degraded-reachable, not down).
/// Unreachable: peers exist and every one is NoConnection. (`_my` presence
/// is enforced by the caller — this only runs inside `my_network.map(..)`.)
pub fn derive_reachability_status(
    _my: &MyNetworkSummary,
    peers: &[PeerHealth],
) -> ReachabilityStatus {
    if peers
        .iter()
        .any(|p| p.connection_mode == ConnectionMode::Direct)
    {
        ReachabilityStatus::Reachable
    } else if peers.iter().any(|p| {
        matches!(
            p.connection_mode,
            ConnectionMode::Relay | ConnectionMode::Degraded
        )
    }) {
        ReachabilityStatus::Degraded
    } else if peers.is_empty() {
        // No peers yet ≠ unreachable. Report Reachable because *we* have
        // working endpoint state; reachability of others is unknown,
        // not failing.
        ReachabilityStatus::Reachable
    } else {
        ReachabilityStatus::Unreachable
    }
}
```

- [ ] **Step 4: Strengthen the e2e snapshot test** — in `liveness_degraded_clears_stale_self_test_rtt` (line ~3487, after `let snap = svc.snapshot().await;` and its existing asserts), append:

```rust
        assert_eq!(
            snap.my_network.expect("my_network present").reachability,
            ReachabilityStatus::Degraded,
            "ZEB-628: peer-signal-only Degraded rolls up Degraded, not Unreachable"
        );
```

(If `snap.my_network` is not `Option` here, adapt to the actual field type — the assert's substance is the top-level `reachability`.)

- [ ] **Step 5: Run the file's tests**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(derive_reachability_status) | test(liveness_degraded_clears_stale_self_test_rtt)'`
Expected: all PASS (4 old + 1 new derive tests, 1 e2e).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/network_health.rs
git commit -m "ZEB-628: fold liveness-Degraded into the Degraded reachability rollup tier"
```

---

### Task 2: ZEB-627a — outbound post-`open_bi` supersession recheck

**Files:**
- Modify: `src-tauri/src/zenoh_iroh_transport.rs:936-940` (outbound `new_link` success branch)

**Interfaces:** none produced; mirrors inbound guard at `:658`.

- [ ] **Step 1: Implement** — in `new_link`, between the `open_bi()` match (ends line 936) and the `let src = ...` line, insert:

```rust
        // ZEB-627: a same-zid reconnect may have superseded this connection
        // while `open_bi()` was awaiting (its swap closed us and installed a
        // newer conn). Do NOT hand zenoh a stale link — mirrors the inbound
        // accept path's post-`accept_bi` recheck (ZEB-616). The supersessor's
        // swap already closed this conn; the supervisor's normal kick/dial
        // path owns recovery, so failing the link here is safe.
        if !self.is_active_zenoh_conn(peer_id, conn_id) {
            tracing::debug!(
                peer = %peer_id,
                "ZEB-627: connection superseded during open_bi; not admitting stale link"
            );
            return Err(zerror!("iroh connection superseded during open_bi").into());
        }
```

No new test: the interleave is a real-QUIC race that cannot be deterministically staged (same status as the inbound guard from #392); the guard predicate is already unit-covered by the registry tests (spec §1 records the rationale).

- [ ] **Step 2: Run the transport tests (regression)**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(zenoh_iroh_transport)'`
Expected: all PASS (existing outbound/inbound registry + supervisor-integration tests unaffected).

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/zenoh_iroh_transport.rs
git commit -m "ZEB-627: recheck supersession after outbound open_bi (mirror inbound guard)"
```

---

### Task 3: ZEB-627b — `evict_peer` + membership Leave/Kick wiring

**Files:**
- Modify: `src-tauri/src/reconnect_supervisor.rs` (after `mark_connected`, ~line 318; tests after ~line 1660)
- Modify: `src-tauri/src/lib.rs:5896-5937` (membership `Leave`/`Kick` arms)

**Interfaces:**
- Produces: `SupervisorHandle::evict_peer(&self, peer: [u8; 32])` (non-async, sync-context-safe like the other handle methods).
- Consumes: `ReachabilityResolver::resolve(&OwnerAddr) -> Vec<ReachabilityAnnouncePayload>` (`.iroh_node_id` per device), `ReachabilityResolver::supervisor() -> Option<SupervisorHandle>`.

- [ ] **Step 1: Write the failing tests** — in the `reconnect_supervisor.rs` test module (append near the marker tests, after `inbound_reconnect_emits_marker_via_mark_connected`):

```rust
    #[tokio::test(start_paused = true)]
    async fn evict_peer_removes_slot_in_any_state() {
        let handle = SupervisorHandle::new();
        let peer = [7u8; 32];
        handle.mark_connected(peer);
        assert_eq!(handle.states_snapshot().len(), 1, "slot exists pre-evict");
        handle.evict_peer(peer);
        assert!(
            handle.states_snapshot().is_empty(),
            "ZEB-627: eviction removes the slot outright"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn evict_peer_unknown_is_noop() {
        let handle = SupervisorHandle::new();
        handle.evict_peer([9u8; 32]);
        assert!(handle.states_snapshot().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn evict_peer_clears_pending_kick_but_not_future_ones() {
        // ZEB-627: eviction drops the peer's PENDING dirty entry (so the loop
        // doesn't immediately resurrect the slot from a pre-eviction kick),
        // but a LATER kick (e.g. the departing conn's final drop-watcher
        // `Dropped`) still lands — pinning the documented bounded residual:
        // post-eviction kicks recreate a slot that ladders to Dormant.
        let handle = SupervisorHandle::new();
        let peer = [3u8; 32];
        handle.kick(peer, ReconnectTrigger::Dropped);
        assert!(handle.pending_trigger(peer).is_some(), "kick pending");
        handle.evict_peer(peer);
        assert!(
            handle.pending_trigger(peer).is_none(),
            "eviction clears the pending kick"
        );
        handle.kick(peer, ReconnectTrigger::Dropped);
        assert_eq!(
            handle.pending_trigger(peer),
            Some(ReconnectTrigger::Dropped),
            "a post-eviction kick still lands (documented residual)"
        );
    }
```

- [ ] **Step 2: Run to verify they fail to compile** (no `evict_peer` yet)

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(evict_peer)'`
Expected: compile error `no method named evict_peer`.

- [ ] **Step 3: Implement `evict_peer`** — on `impl SupervisorHandle`, after `mark_connected` (line ~318):

```rust
    /// ZEB-627: forget a departed peer (membership Leave/Kick). Removes the
    /// slot in ANY state — a departed peer must never be dialed again, and
    /// Dormant slots otherwise persist for process life (unbounded over
    /// long-session peer churn). Also drops the peer's pending dirty entry so
    /// a pre-eviction kick can't immediately resurrect the slot. A LATER kick
    /// (e.g. the departing conn's final drop-watcher `Dropped`) recreates a
    /// fresh slot that resolve-misses ladder to Dormant — a bounded, transient
    /// tail, accepted by design (spec §2). Non-async and sync-context-safe,
    /// like every other handle method.
    pub fn evict_peer(&self, peer: [u8; 32]) {
        {
            let mut dirty = self.inner.dirty.lock().expect("dirty lock");
            dirty.remove(&peer);
        }
        let removed = {
            let mut states = self.inner.states.lock().expect("states lock");
            states.remove(&peer).is_some()
        };
        if removed {
            self.inner.notify.notify_one();
        }
    }
```

- [ ] **Step 4: Run the new tests**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(evict_peer)'`
Expected: 3 PASS.

- [ ] **Step 5: Wire the membership arms** — in `src-tauri/src/lib.rs`, `Leave` arm (5896-5917), replace the body:

```rust
                                            crate::community_membership::MembershipEventKind::Leave => {
                                                // ZEB-458 P4B: retract the
                                                // leaver's relay ads promptly
                                                // (read-time freshness would
                                                // eventually drop them, but a
                                                // Leave is an explicit signal —
                                                // mirrors the reachability
                                                // remove_owner below).
                                                community_relay_resolver
                                                    .remove_advertiser(&community_id, &event.actor);
                                                // ZEB-627: capture the leaver's
                                                // device node-ids BEFORE the
                                                // resolver forgets them (read
                                                // precedes the destructive
                                                // write), to evict their
                                                // reconnect-supervisor slots.
                                                let departed_nodes: Vec<[u8; 32]> = resolver
                                                    .resolve(&event.actor)
                                                    .into_iter()
                                                    .map(|p| p.iroh_node_id)
                                                    .collect();
                                                let n = resolver.remove_owner(&event.actor);
                                                if n > 0 {
                                                    // ZEB-627: a departed peer
                                                    // must not stay scheduled
                                                    // (or parked Dormant) for
                                                    // process life.
                                                    if let Some(sup) = resolver.supervisor() {
                                                        for node in &departed_nodes {
                                                            sup.evict_peer(*node);
                                                        }
                                                    }
                                                    // ZEB-329: see comment above.
                                                    if let Some(nh) = network_health_cell
                                                        .read()
                                                        .ok()
                                                        .and_then(|g| g.as_ref().cloned())
                                                    {
                                                        nh.notify();
                                                    }
                                                    emit_changed = Some(event.actor);
                                                }
                                            }
```

And the `Kick` arm (5919-5937), same shape with `target`:

```rust
                                            crate::community_membership::MembershipEventKind::Kick { target, .. } => {
                                                // ZEB-458 P4B: retract the
                                                // kicked member's relay ads
                                                // (mirrors remove_owner below).
                                                community_relay_resolver
                                                    .remove_advertiser(&community_id, target);
                                                // ZEB-627: capture-then-evict,
                                                // as in the Leave arm above.
                                                let departed_nodes: Vec<[u8; 32]> = resolver
                                                    .resolve(target)
                                                    .into_iter()
                                                    .map(|p| p.iroh_node_id)
                                                    .collect();
                                                let n = resolver.remove_owner(target);
                                                if n > 0 {
                                                    if let Some(sup) = resolver.supervisor() {
                                                        for node in &departed_nodes {
                                                            sup.evict_peer(*node);
                                                        }
                                                    }
                                                    // ZEB-329: see comment above.
                                                    if let Some(nh) = network_health_cell
                                                        .read()
                                                        .ok()
                                                        .and_then(|g| g.as_ref().cloned())
                                                    {
                                                        nh.notify();
                                                    }
                                                    emit_changed = Some(*target);
                                                }
                                            }
```

- [ ] **Step 6: Run supervisor + a compile-check of lib**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(reconnect_supervisor)'`
Expected: all PASS (module suite green; lib compiles → wiring type-checks).

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/reconnect_supervisor.rs src-tauri/src/lib.rs
git commit -m "ZEB-627: evict supervisor slots on membership Leave/Kick"
```

---

### Task 4: ZEB-627c+d — resolver generation counter + `ZidNodeCache`

**Files:**
- Modify: `src-tauri/src/reachability_resolver.rs` (struct 168-206, Default 217-229, Clone 231-246, `update_with_source` 320-398, `remove_owner` 575-594; tests in its test module)
- Modify: `src-tauri/src/event_loop.rs:1205-1233` (listener map → cache; new struct + test module)

**Interfaces:**
- Produces: `ReachabilityResolver::generation(&self) -> u64`; `ZidNodeCache` (private to event_loop.rs) with `fn lookup(&mut self, zid: &str, current_gen: u64, rebuild: impl FnOnce() -> HashMap<String, [u8; 32]>) -> Option<[u8; 32]>`.

- [ ] **Step 1: Resolver — add the field.** In the struct (after `refresh_permits`, line ~205):

```rust
    // ZEB-627: monotonic change counter for the peer-record map — bumped on
    // every MATERIALIZED update (LWW-accepted slot write) and on any
    // non-empty `remove_owner`. Generation-keyed caches over derived views
    // (event_loop's zid→node map) compare it to decide when to rebuild; a
    // bump covers both stale directions (evicted/reassigned AND newly added).
    // Shared across clones (`Arc`) like every other field.
    generation: Arc<std::sync::atomic::AtomicU64>,
```

In `Default` (line ~219 block) add: `generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),`
In `Clone` (line ~233 block) add: `generation: Arc::clone(&self.generation),`

- [ ] **Step 2: Bump sites + accessor.** In `update_with_source`, extend the `do_replace` write (line ~373):

```rust
        if do_replace {
            *target = Some(next);
            // ZEB-627: the derived zid→node view may have changed.
            self.generation
                .fetch_add(1, std::sync::atomic::Ordering::Release);
        }
```

In `remove_owner`, after the removal loop (before `drop(map)`, line ~584):

```rust
        if n > 0 {
            // ZEB-627: departed devices left the derived views.
            self.generation
                .fetch_add(1, std::sync::atomic::Ordering::Release);
        }
```

Accessor on `impl ReachabilityResolver` (near `supervisor()`, line ~283):

```rust
    /// ZEB-627: current map generation (see the field doc). `Acquire` pairs
    /// with the mutators' `Release` bumps.
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Acquire)
    }
```

- [ ] **Step 3: Resolver generation test** — in `reachability_resolver.rs`'s test module (model the seed on `reconnect_supervisor.rs`'s `seed` fn):

```rust
    #[test]
    fn generation_bumps_only_on_materialized_change() {
        let r = ReachabilityResolver::new();
        let g0 = r.generation();
        let payload = |node: [u8; 32], at: u64| ReachabilityAnnouncePayload {
            iroh_node_id: node,
            home_relay_url: String::new(),
            direct_addresses: vec![],
            announced_at_ms: at,
            identity_signature: [0u8; 64],
            butler_set: vec![],
            bs_at: 0,
        };
        let hlc = |wall: u64| Hlc {
            wall_ms: wall,
            logical: 0,
            device_id: String::new(),
        };
        // First learn: materialized → bump.
        r.update(OwnerAddr([0xAA; 16]), payload([1u8; 32], 10), hlc(10));
        let g1 = r.generation();
        assert!(g1 > g0, "accepted write bumps");
        // LWW-rejected (older HLC on the same slot): no bump.
        r.update(OwnerAddr([0xAA; 16]), payload([1u8; 32], 5), hlc(5));
        assert_eq!(r.generation(), g1, "rejected write does not bump");
        // Eviction: bump.
        let n = r.remove_owner(&OwnerAddr([0xAA; 16]));
        assert_eq!(n, 1);
        assert!(r.generation() > g1, "remove_owner bumps");
        // Removing an absent owner: no bump.
        let g2 = r.generation();
        assert_eq!(r.remove_owner(&OwnerAddr([0xBB; 16])), 0);
        assert_eq!(r.generation(), g2, "empty removal does not bump");
    }
```

(Adapt constructor names to the test module's existing imports; the file's other tests show the exact paths.)

- [ ] **Step 4: Run resolver tests**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(reachability_resolver)'`
Expected: all PASS including the new one.

- [ ] **Step 5: `ZidNodeCache`** — in `event_loop.rs`, above the function containing the listener spawn:

```rust
/// ZEB-627: generation-keyed zid→node cache for the zenoh transport-events
/// listener. Values are `Option<[u8; 32]>` — `None` tombstones a zid unknown
/// at this generation, so repeated events from a non-peer session don't pay an
/// O(active_peers) rebuild each (the pre-ZEB-627 behavior). Any resolver
/// change (generation bump) clears the cache wholesale, covering BOTH stale
/// directions: a hit for an evicted/reassigned peer (stale-positive kicks for
/// departed nodes) and a tombstone hiding a newly learned peer.
struct ZidNodeCache {
    map: std::collections::HashMap<String, Option<[u8; 32]>>,
    seen_gen: Option<u64>,
}

impl ZidNodeCache {
    fn new() -> Self {
        Self {
            map: std::collections::HashMap::new(),
            seen_gen: None,
        }
    }

    /// Resolve `zid`. `current_gen` must be read from the resolver BEFORE the
    /// rebuild closure would run — a concurrent mutation mid-rebuild then
    /// forces another clear on the next event (conservative, never stale).
    fn lookup(
        &mut self,
        zid: &str,
        current_gen: u64,
        rebuild: impl FnOnce() -> std::collections::HashMap<String, [u8; 32]>,
    ) -> Option<[u8; 32]> {
        if self.seen_gen != Some(current_gen) {
            self.map.clear();
            self.seen_gen = Some(current_gen);
        }
        if let Some(cached) = self.map.get(zid) {
            return *cached;
        }
        self.map = rebuild().into_iter().map(|(k, v)| (k, Some(v))).collect();
        let hit = self.map.get(zid).copied().flatten();
        if hit.is_none() {
            self.map.insert(zid.to_string(), None); // tombstone
        }
        hit
    }
}
```

- [ ] **Step 6: Replace the listener's inline map** — swap lines 1205-1233 for:

```rust
                // ZEB-627: generation-keyed zid→node cache (see ZidNodeCache).
                // Replaces the former miss-only rebuild map, whose hits were
                // never revalidated (stale-positive kicks for departed peers)
                // and whose unknown-zid misses rebuilt O(active_peers) on
                // every event (no negative cache).
                let mut zid_cache = ZidNodeCache::new();
                while let Ok(event) = listener.recv_async().await {
                    let zid = event.transport().zid().to_string();
                    let current_gen = listener_resolver.generation();
                    let node_id = zid_cache.lookup(&zid, current_gen, || {
                        listener_resolver
                            .list_active_peers()
                            .into_iter()
                            .map(|(_owner, p)| {
                                (
                                    crate::iroh_dial_driver::deterministic_zid_hex(
                                        &p.iroh_node_id,
                                    ),
                                    p.iroh_node_id,
                                )
                            })
                            .collect()
                    });
```

(The `match event.kind()` body below stays untouched.)

- [ ] **Step 7: Cache unit tests** — new `#[cfg(test)]` module at the bottom of `event_loop.rs` (or inside its existing test module if one exists — check first):

```rust
#[cfg(test)]
mod zid_node_cache_tests {
    use super::ZidNodeCache;
    use std::cell::Cell;
    use std::collections::HashMap;

    fn view(entries: &[(&str, [u8; 32])]) -> HashMap<String, [u8; 32]> {
        entries
            .iter()
            .map(|(z, n)| (z.to_string(), *n))
            .collect()
    }

    #[test]
    fn hit_does_not_rebuild() {
        let mut c = ZidNodeCache::new();
        let rebuilds = Cell::new(0);
        let a = [1u8; 32];
        assert_eq!(
            c.lookup("z1", 0, || {
                rebuilds.set(rebuilds.get() + 1);
                view(&[("z1", a)])
            }),
            Some(a)
        );
        assert_eq!(
            c.lookup("z1", 0, || {
                rebuilds.set(rebuilds.get() + 1);
                view(&[("z1", a)])
            }),
            Some(a)
        );
        assert_eq!(rebuilds.get(), 1, "same-generation hit skips the rebuild");
    }

    #[test]
    fn tombstone_prevents_rebuild_per_event() {
        let mut c = ZidNodeCache::new();
        let rebuilds = Cell::new(0);
        for _ in 0..3 {
            assert_eq!(
                c.lookup("ghost", 7, || {
                    rebuilds.set(rebuilds.get() + 1);
                    view(&[])
                }),
                None
            );
        }
        assert_eq!(
            rebuilds.get(),
            1,
            "unknown zid rebuilds once per generation, then tombstones"
        );
    }

    #[test]
    fn generation_bump_clears_stale_positive() {
        let mut c = ZidNodeCache::new();
        let a = [1u8; 32];
        assert_eq!(c.lookup("z1", 0, || view(&[("z1", a)])), Some(a));
        // Resolver evicted z1's peer → generation bumped, view empty.
        assert_eq!(
            c.lookup("z1", 1, || view(&[])),
            None,
            "stale-positive entry does not survive a generation bump"
        );
    }

    #[test]
    fn generation_bump_reveals_new_peer_behind_tombstone() {
        let mut c = ZidNodeCache::new();
        let b = [2u8; 32];
        assert_eq!(c.lookup("z2", 0, || view(&[])), None); // tombstoned
        assert_eq!(
            c.lookup("z2", 1, || view(&[("z2", b)])),
            Some(b),
            "stale-negative tombstone does not survive a generation bump"
        );
    }
}
```

- [ ] **Step 8: Run the new tests**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(zid_node_cache)'`
Expected: 4 PASS.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/reachability_resolver.rs src-tauri/src/event_loop.rs
git commit -m "ZEB-627: generation-keyed zid→node cache (stale-positive + negative-cache fix)"
```

---

### Task 5: ZEB-630 — extract iroh relay RMW helpers + unit tests

**Files:**
- Modify: `src-tauri/src/lib.rs` — new helpers after `iroh_target_relay_urls` (~47305); `add_iroh_relay` body (~47444-47449); `remove_iroh_relay` body (~47471-47485); new test near `add_remove_pkarr_relay_read_modify_write` (~50429)

**Interfaces:**
- Produces: `fn add_iroh_relay_target(settings: &connectivity_settings::ConnectivitySettings, url: &str) -> Result<Vec<String>, String>`; `fn remove_iroh_relay_target(...) -> Result<Vec<String>, String>`.

- [ ] **Step 1: Extract the helpers** — insert after `iroh_target_relay_urls` (line 47305):

```rust
/// ZEB-630: pure RMW core of `add_iroh_relay` — materialize the EFFECTIVE
/// list (preset defaults on a defaults-following node), append, validate
/// (dedups, caps at `MAX_IROH_RELAYS`). Extracted from the IPC so the
/// materialization behavior is unit-testable without a Tauri runtime.
fn add_iroh_relay_target(
    settings: &connectivity_settings::ConnectivitySettings,
    url: &str,
) -> Result<Vec<String>, String> {
    let mut relays = iroh_target_relay_urls(settings);
    relays.push(url.to_string());
    connectivity_settings::validate_iroh_relay_urls(relays)
}

/// ZEB-630: pure RMW core of `remove_iroh_relay` — filter the EFFECTIVE list
/// (trailing-slash-normalized match), reject removing the last relay (use
/// reset to follow defaults), validate. Extracted so the last-relay guard is
/// covered by a unit test rather than a simulation.
fn remove_iroh_relay_target(
    settings: &connectivity_settings::ConnectivitySettings,
    url: &str,
) -> Result<Vec<String>, String> {
    let target = url.trim().trim_end_matches('/');
    let remaining: Vec<String> = iroh_target_relay_urls(settings)
        .into_iter()
        .filter(|r| r.trim_end_matches('/') != target)
        .collect();
    if remaining.is_empty() {
        return Err(
            "cannot remove the last iroh relay; use reset to follow the built-in defaults instead"
                .to_string(),
        );
    }
    connectivity_settings::validate_iroh_relay_urls(remaining)
}
```

- [ ] **Step 2: Delegate the commands.** `add_iroh_relay`: replace lines 47445-47448 (`let settings = ...` stays; `let mut relays`/`push`/`validate` go) with:

```rust
    let settings = connectivity_settings::ConnectivitySettings::load_or_default(&path);
    let validated = add_iroh_relay_target(&settings, &url)?;
```

`remove_iroh_relay`: replace lines 47472-47484 (from `let settings = ...` through the `validate_iroh_relay_urls(remaining)?` line) with:

```rust
    let settings = connectivity_settings::ConnectivitySettings::load_or_default(&path);
    let validated = remove_iroh_relay_target(&settings, &url)?;
```

(Both then fall through to the existing `apply_iroh_relays(&app, &state, validated).await`.) This is a MOVE, not a rewrite — diff the extracted bodies against the deleted lines to confirm byte-equivalent logic.

- [ ] **Step 3: Write the RMW test** — after `add_remove_pkarr_relay_read_modify_write` (ends line 50429):

```rust
    #[test]
    fn add_remove_iroh_relay_read_modify_write() {
        // ZEB-630: iroh mirror of add_remove_pkarr_relay_read_modify_write,
        // driving the REAL RMW cores (add/remove_iroh_relay_target) that the
        // IPCs delegate to — including the defaults-materialization on a
        // defaults-following node and the inline last-relay rejection (which,
        // unlike pkarr, is NOT in the validator).
        let td = tempfile::TempDir::new().expect("tempdir");
        let path = td.path().join("connectivity-settings.json");

        // Defaults-following node: no custom iroh relays persisted.
        let settings =
            crate::connectivity_settings::ConnectivitySettings::load_or_default(&path);
        assert!(
            crate::connectivity_settings::effective_iroh_relays(&settings).is_none(),
            "fresh settings follow defaults"
        );
        let defaults = crate::iroh_default_relay_urls();
        assert!(!defaults.is_empty(), "preset defaults are non-empty");

        // --- Add on a defaults-following node materializes defaults + new ---
        let added = crate::add_iroh_relay_target(&settings, "https://relay.zeblithic.example")
            .expect("valid add");
        assert_eq!(added.len(), defaults.len() + 1, "defaults materialized + 1");
        assert!(added.contains(&"https://relay.zeblithic.example".to_string()));
        let mut s = crate::connectivity_settings::ConnectivitySettings::load_or_default(&path);
        s.iroh_relays = added.clone();
        s.save(&path).expect("persist add");

        // --- Re-add dedups (validator) ---
        let s2 = crate::connectivity_settings::ConnectivitySettings::load_or_default(&path);
        let readded = crate::add_iroh_relay_target(&s2, "https://relay.zeblithic.example")
            .expect("dedup add");
        assert_eq!(readded.len(), added.len(), "re-add is a no-op");

        // --- Remove one (trailing-slash-normalized) ---
        let removed =
            crate::remove_iroh_relay_target(&s2, "https://relay.zeblithic.example/")
                .expect("valid remove");
        assert_eq!(removed.len(), added.len() - 1);
        assert!(!removed.contains(&"https://relay.zeblithic.example".to_string()));

        // --- Remove down to one, then the LAST one → inline guard fires ---
        let mut one = crate::connectivity_settings::ConnectivitySettings::load_or_default(&path);
        one.iroh_relays = vec![removed[0].clone()];
        one.save(&path).expect("persist single");
        let s3 = crate::connectivity_settings::ConnectivitySettings::load_or_default(&path);
        let err = crate::remove_iroh_relay_target(&s3, &removed[0])
            .expect_err("last-relay removal must be rejected");
        assert_eq!(
            err,
            "cannot remove the last iroh relay; use reset to follow the built-in defaults instead"
        );
    }
```

(If the test module path prefixes differ — e.g. helpers referenced without `crate::` — match the sibling pkarr test's conventions.)

- [ ] **Step 4: Run**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(add_remove_iroh_relay_read_modify_write) | test(iroh_relay)'`
Expected: new test PASS + existing iroh-relay tests still PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "ZEB-630: extract + unit-test the iroh relay RMW cores (materialization, last-relay guard)"
```

---

### Task 6: ZEB-629 — one unified connectivity-settings write lock

**Files:**
- Modify: `src-tauri/src/lib.rs` — lock statics (47105-47115 pkarr, 47307-47316 iroh), all 11 `*_relay_write_lock()` call sites (47125, 47138ish, 47159ish, 47189ish, 10257 | 47380, 47410, 47422, 47437, 47464, 10311), three lock-free writers (32019-32040, 46777-46787, 48797-48801)

**Interfaces:**
- Produces: `fn connectivity_settings_write_lock() -> &'static tokio::sync::Mutex<()>`.

- [ ] **Step 1: Add the unified lock, delete the two old ones.** Replace the `PKARR_RELAY_WRITE_LOCK` block (47105-47115) with:

```rust
/// ZEB-629: serializes EVERY read-modify-write of `connectivity-settings.json`
/// — pkarr relays, iroh relays, presence visibility, identity
/// discoverability, friend auto-accept, and the two boot reconciles. The file
/// is a single per-process whole-file save target, so writers under different
/// locks (the former PKARR/IROH pair) — or under none (the three toggles) —
/// could interleave load/load/save/save and silently drop each other's field
/// (ZEB-623/624 final-review findings (j)/(m)). One process-global lock
/// closes every pairing. LOCK ORDER: always acquired BEFORE the NodeState
/// mutex, never while holding it (matches the pre-unification relay-lock
/// convention — see `get_iroh_relays`' round-2 note). Readers other than
/// `get_iroh_relays` (which pairs the custom flag with the live list) stay
/// lock-free: `save` renames atomically, so they always see a complete file.
static CONNECTIVITY_SETTINGS_WRITE_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> =
    std::sync::OnceLock::new();

fn connectivity_settings_write_lock() -> &'static tokio::sync::Mutex<()> {
    CONNECTIVITY_SETTINGS_WRITE_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}
```

Delete the `IROH_RELAY_WRITE_LOCK` block (47307-47316) entirely.

- [ ] **Step 2: Retarget all 11 call sites.** Replace every `pkarr_relay_write_lock()` and `iroh_relay_write_lock()` with `connectivity_settings_write_lock()`:

```bash
cd src-tauri && grep -n "pkarr_relay_write_lock()\|iroh_relay_write_lock()" src/lib.rs
```

Expected 11 hits (5 pkarr: set/reset/add/remove + boot ~10257; 6 iroh: get/set/reset/add/remove + boot ~10311). Edit each. Also update stale comment references: the two boot-guard comments ("Hold the same relay write lock…" ~10249 and ~10305), `apply_iroh_relays`' doc ("Callers hold `iroh_relay_write_lock()`" → `connectivity_settings_write_lock()`), and any `apply_pkarr_relays` doc mention — find them:

```bash
grep -n "relay_write_lock\|RELAY_WRITE_LOCK" src/lib.rs
```

Expected: zero hits after this step (all renamed or deleted).

- [ ] **Step 3: Cover the three lock-free writers.**

`set_presence_visibility` (~32037): wrap the spawn_blocking (NodeState guard was already dropped at the block end above; presence-map flip stays outside the lock — it's not file state):

```rust
    let path = connectivity_settings_path(settings_path)?;
    {
        // ZEB-629: file RMW under the process-global settings write lock.
        let _settings_guard = connectivity_settings_write_lock().lock().await;
        tokio::task::spawn_blocking(move || persist_presence_visibility(&path, visible))
            .await
            .map_err(|e| format!("persist presence visibility task: {e}"))??;
    }
```

`connectivity_set_identity_discoverable_impl` (~46779): same wrap around its `spawn_blocking(...)await??` statement:

```rust
    {
        // ZEB-629: file RMW under the process-global settings write lock.
        let _settings_guard = connectivity_settings_write_lock().lock().await;
        tokio::task::spawn_blocking(move || {
            let mut settings =
                connectivity_settings::ConnectivitySettings::load_or_default(&path);
            settings.identity_discoverable = enabled;
            settings
                .save(&path)
                .map_err(|e| format!("save connectivity-settings: {e}"))
        })
        .await
        .map_err(|e| format!("save connectivity-settings task: {e}"))??;
    }
```

`set_friend_auto_accept` (~48797): wrap the inline RMW:

```rust
    {
        // ZEB-629: file RMW under the process-global settings write lock.
        let _settings_guard = connectivity_settings_write_lock().lock().await;
        let mut settings = connectivity_settings::ConnectivitySettings::load_or_default(&path);
        settings.friend_auto_accept_known = enabled;
        settings
            .save(&path)
            .map_err(|e| format!("save connectivity-settings: {e}"))?;
    }
```

- [ ] **Step 4: Lock-order audit.** For each of the 14 sites, confirm the NodeState mutex is only ever taken AFTER the settings lock or in an already-closed scope before it. The three toggle writers read NodeState in a scoped block that drops the guard before the settings lock — confirm no site `.lock()`s NodeState while a `_settings_guard`/`_relay_write_guard`/`_relay_boot_guard` binding is live EXCEPT the established lock→NodeState direction (relay verbs, boot guards, `apply_*_relays`). Record the audit result in the commit message.

- [ ] **Step 5: Run the settings/relay test surface**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(relay) | test(presence_visibility) | test(discoverable) | test(friend_auto_accept) | test(connectivity_settings)'`
Expected: all PASS.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "ZEB-629: one process-global connectivity-settings write lock for all writers"
```

---

### Task 7: ZEB-633 — redeem fixture timeouts 50ms → 2s

**Files:**
- Modify: `src-tauri/tests/community_sync/community_sync_integration.rs:3001, :3080` (the only two `redeem_timeout` overrides in the tree) + doc comments `:2690-2695` and any other `50ms` mention near the fixture

- [ ] **Step 1: Change both overrides.** At L3001 and L3080:

```rust
                redeem_timeout: Some(std::time::Duration::from_secs(2)),
```

- [ ] **Step 2: Update the fixture doc** (2690-2695) — replace the `(50ms)` sentence:

```rust
    // ZEB-501: the redeem oneshot now fires ONLY on a real JoinCountersign, so an
    // unreachable inviter (no countersign) genuinely reaches the step-7d timeout.
    // Both tests drive that timeout with a short `redeem_timeout` passed via
    // `RedeemInviteOverrides` — NOT the process-global
    // HARMONY_REDEEM_INVITE_TIMEOUT_MS env var, so there is no cross-test env
    // race (the one Qodo/CodeAnt flagged on #293). ZEB-633: 2s, not 50ms — the
    // 50ms budget flaked once under a full-parallelism sweep (budget≈starvation
    // window); the duration is semantically irrelevant (the inviter is
    // unreachable, nothing can arrive), so a load-proof budget costs only
    // wall-clock. If it EVER flakes at 2s, capture full output (no tail pipes).
```

Then `grep -n "50ms\|from_millis(50)" src-tauri/tests/community_sync/community_sync_integration.rs` — update any remaining mention tied to these two tests (leave unrelated hits alone).

- [ ] **Step 3: Run both tests**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(redeem_invite_only_)'`
Expected: 2 PASS (each now takes ~2s — the timeout must genuinely expire).

- [ ] **Step 4: Commit**

```bash
git add src-tauri/tests/community_sync/community_sync_integration.rs
git commit -m "ZEB-633: redeem fixture timeout 50ms → 2s (load-proof; duration is semantically irrelevant)"
```

---

### Task 8: Full gates, sweep, PR

- [ ] **Step 1: fmt**

Run: `cd src-tauri && cargo fmt --all` then `cargo fmt --all -- --check`
Expected: clean.

- [ ] **Step 2: clippy (CI form)**

Run: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: clean. (Commit any fmt/clippy fixups: `git add -u && git commit -m "chore: fmt/clippy fixups for the hardening bundle"` — skip if empty.)

- [ ] **Step 3: Full sweep** (background + caffeinate; ~10min post-ZEB-626)

Run: `cd src-tauri && caffeinate -dims cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast` (run_in_background; capture FULL output, no tail pipes)
Expected: all tests pass (~4060).

- [ ] **Step 4: Frontend gates** (repo root)

Run: `npx tsc --noEmit && npx vitest run`
Expected: clean / all pass.

- [ ] **Step 5: Whole-branch review, then PR**

Adversarial whole-branch review (spec-vs-diff) before opening. PR body: summary per ticket, §0 already-done notes, `Closes ZEB-627`, `Closes ZEB-628`, `Closes ZEB-629`, `Closes ZEB-630`, `Closes ZEB-633`. Push branch, open PR against main, trigger CodeRabbit once, converge per standing protocol. NEVER auto-merge.
