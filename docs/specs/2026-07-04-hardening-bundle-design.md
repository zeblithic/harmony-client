# Hardening bundle: ZEB-627 + ZEB-628 + ZEB-629 + ZEB-630 + ZEB-633 — design

**Date:** 2026-07-04 · **Branch:** `zeb-627-633-hardening-bundle` (off main `0787a2ab`) · **Approved by:** Jake (in-session, 2026-07-04)

One PR bundling five post-review hardening tickets (per the bundle-small-PRs rule).
All are follow-ups from the PR #392 (ZEB-620), #393 (ZEB-622), #395 (ZEB-623/624)
review waves plus one flake from #396's validation sweeps.

## 0. Scope corrections (ticket items already done on main — no work)

Verified against `0787a2ab` during exploration:

1. **ZEB-627 item 2 (dial semaphore):** landed in PR #392 itself — permits are
   `try_acquire_owned` **before** spawn; at capacity the loop defers due peers to the
   next `DialResult` wake; `SupervisorConfig::dial_timeout` (30s) bounds every dial.
   (Already recorded on the ticket, 2026-07-03 comment.)
2. **ZEB-627 item "ring markers":** already fully wired. `record_reconnected`
   (`reconnect_supervisor.rs:501` inbound drain, `:575` dial-success, gated on the
   `ever_connected` bit), `record_retrying` (`:676`, the single Connected→Retrying
   edge), `record_dormant` (`:740`, once per Retrying→Dormant). All three render in
   `NetworkHealthView.svelte` (`dialHitIcon`, lines 203–213; recent list 342–348).
3. **ZEB-629 item 2 (fixed temp name):** `ConnectivitySettings::save`
   (`connectivity_settings.rs:369-390`) already uses `tempfile::NamedTempFile` +
   `persist` (unique sibling temp, atomic replace, dir fsync). The fixed
   `.json.tmp` race in the ticket text is stale. Covered by
   `save_is_atomic_no_stray_temp_files`.

These get a note at ticket close; the PR body records them.

## 1. ZEB-627a — outbound post-`open_bi` supersession recheck

**Site:** `src-tauri/src/zenoh_iroh_transport.rs`, outbound `new_link` success branch
(~line 937). The inbound accept path rechecks after its stream opens
(`spawn_accept_loop` ~line 658):

```rust
if !mgr.is_active_zenoh_conn(peer_id, conn_id) { /* drop stale link */ return; }
```

The outbound path has that guard only on the `open_bi()` **failure** branch (~931);
on success it admits the link unconditionally. A same-zid reconnect that supersedes
the conn while `open_bi()` is in flight therefore hands zenoh a dead/stale link.

**Change:** mirror the inbound guard on the outbound success branch — after
`open_bi()` returns `Ok`, if `!self.is_active_zenoh_conn(peer_id, conn_id)`, close
the conn (best-effort) and return `Err` (zenoh treats it as a failed link; the
supervisor's normal kick/dial path recovers). Registration order
(`swap_zenoh_conn` → `mark_supervisor_connected` → `spawn_drop_watcher` → stream)
is deliberately unchanged — the #392 review already established that reordering
trades this window for a suppression gap.

**Testing:** the interleave window is a real-QUIC race that cannot be staged
deterministically (same status as the inbound guard, which shipped in #392 without
a race test). The guard predicate (`is_active_zenoh_conn` vs `swap_zenoh_conn`
supersession) is already unit-covered by the registry tests. No new test; the spec
records the rationale so reviewers don't flag a coverage gap silently.

## 2. ZEB-627b — dormant/departed peer eviction via membership events

**Problem:** nothing ever removes entries from `SupervisorInner.states`
(`reconnect_supervisor.rs:228`); the map is unbounded over the life of the process
w.r.t. ever-seen peers (Dormant slots persist by design for kick-revival).

**Chosen design (over an LRU cap):** membership-driven eviction. Departure is an
explicit signal we already observe: the membership event loop in `lib.rs`
(~5811–5938) handles `MembershipEventKind::Leave` (→ `resolver.remove_owner`,
~5906) and `Kick` (~5925). A peer that left or was kicked should never be dialed
again — eviction there is precise, while an LRU cap both under-evicts (departed
peers linger until pressure) and over-evicts (live-but-quiet peers).

**Changes:**

- `SupervisorHandle::evict_peer(node_id: [u8;32])` — removes the slot from
  `states` (any state). No ring marker (eviction is not a liveness edge). Counts
  (`count_peer_states`) drop naturally.
- In the `Leave`/`Kick` arms of the lib.rs membership loop: resolve the departing
  owner's iroh node id from the resolver **before** `remove_owner` (read before the
  destructive write), then call `evict_peer` after `remove_owner` succeeds. If the
  supervisor handle isn't installed (headless paths), skip silently.

**Documented residual (accepted):** the departed peer's connection teardown may
fire one final `Dropped` kick after eviction, recreating a Retrying slot that
resolve-misses ladder to Dormant. That is today's steady-state behavior for
*every* departed peer; post-change it is a transient tail for at most one
teardown, not a permanent leak. Not worth suppressing (would require kick-time
resolver consultation on a hot path).

**Tests** (unit style: `#[tokio::test(start_paused = true)]` + `RecordingDialer` +
`seed`, per the existing module): evict removes the slot (counts before/after);
evicting an unknown peer is a no-op; a post-eviction `Dropped` kick recreates a
slot (pins the documented residual so a future change is deliberate).

## 3. ZEB-627c+d — zid→node cache: invalidation (both directions) + negative cache

**Problem:** the zenoh transport-events listener (`event_loop.rs` ~1189–1268) keeps
a task-local `zid_to_node: HashMap<String,[u8;32]>` rebuilt from
`resolver.list_active_peers()` **only on lookup miss**. Two defects:

- *Stale-positive:* a cache **hit is never revalidated** — a zid mapped to node A
  keeps resolving to A after the resolver evicts/reassigns A, so a `Delete` kicks
  a departed/wrong node (Greptile finding on #392).
- *Stale-negative / no negative cache:* an unknown zid triggers a full
  O(active_peers) rebuild on **every** event, and a wrong mapping cached as a hit
  shadows later resolver additions for that zid.

**Chosen design (over TTL or revalidate-every-hit):** resolver **generation
counter** + tombstoned cache.

- `ReachabilityResolver` gains `generation: AtomicU64`, incremented in
  `update_with_source` (`reachability_resolver.rs:320`; `update` at `:311` funnels
  into it — verify at implementation, bump both if not) and `remove_owner`
  (`:575`), with a `pub fn generation(&self) -> u64` accessor. Ordering: `Release`
  on bump / `Acquire` on read (cheap, conservative).
- New small unit `ZidNodeCache` in `event_loop.rs`:
  `map: HashMap<String, Option<[u8;32]>>` (a `None` value = tombstone for a zid
  unknown at this generation) + `seen_gen: u64`. Single method:
  `lookup(&mut self, zid: &str, current_gen: u64, rebuild: impl FnOnce() -> HashMap<String,[u8;32]>) -> Option<[u8;32]>`
  — if `current_gen != seen_gen`: clear + record gen. On hit (incl. tombstone):
  return it. On miss: run `rebuild`, adopt entries, insert `None` tombstone if the
  zid is still absent, return the result.
- The listener replaces its inline map with `ZidNodeCache`, passing
  `listener_resolver.generation()` per event and the existing
  `list_active_peers()`-based rebuild closure. Net effect: one atomic load per
  event; a full rebuild only when the resolver actually changed or a genuinely
  new zid appears; both staleness directions closed.

**Tests** (pure unit tests on `ZidNodeCache`): hit without rebuild; tombstone
prevents repeated rebuilds at the same generation; generation bump + removal →
stale-positive entry gone after clear; generation bump + addition → formerly
tombstoned zid now resolves. Plus a resolver test: `update`/`remove_owner` each
bump `generation()`.

## 4. ZEB-628 — `ConnectionMode::Degraded` joins the Relay rollup tier

**Site:** `derive_reachability_status`, `network_health.rs:501-523`. The chain
matches `Direct` → `Reachable`, `Relay` → `Degraded`, empty → `Reachable`, else →
`Unreachable`. `ConnectionMode::Degraded` is matched nowhere, so a
peer-signal-only Degraded (live link, no selected path — the ZEB-622 up-edge
window, or an external zenoh `Put` with no registry follow-up) rolls up as
top-level `Unreachable`.

**Change:** fold `Degraded` into the Relay arm:

```rust
} else if peers.iter().any(|p| {
    matches!(p.connection_mode, ConnectionMode::Relay | ConnectionMode::Degraded)
}) {
    ReachabilityStatus::Degraded
}
```

(A live-but-degraded link is at least as reachable as relay-tier; both mean "you
have connectivity, not ideal".) Update the doc comment. The unused `_my` parameter
oddity is noted but out of scope.

**Tests:** new `derive_reachability_status_degraded_when_only_peer_signal_degraded`
cloned from the Relay-tier test (`network_health.rs:2419-2443`); strengthen the
existing end-to-end snapshot test (~3466–3491, drives `LivenessStateWire::Degraded`
through to `PeerHealth`) to also assert the top-level `my_network.reachability` —
exactly where this gap hid.

**Explicitly not doing:** reconciling the supervisor counts row vs the per-peer
`connectionMode` column (ticket phrased it as only-if-confusing-in-fleet-use; two
honest domains; defer until fleet use complains).

## 5. ZEB-629 — one unified connectivity-settings write lock

**Problem:** `connectivity-settings.json` is a whole-file RMW target written under
**two independent locks** (`PKARR_RELAY_WRITE_LOCK` lib.rs ~47110,
`IROH_RELAY_WRITE_LOCK` ~47311) plus **three lock-free writers**
(`persist_presence_visibility` ~31999, `connectivity_set_identity_discoverable_impl`
~46753, `set_friend_auto_accept` ~48781). Any cross-group interleave can
last-writer-wins a field. (Atomic save protects readers from torn files, not
writers from lost updates.)

**Change:** one process-global `CONNECTIVITY_SETTINGS_WRITE_LOCK`
(`OnceLock<tokio::sync::Mutex<()>>`, same pattern as `NICKNAME_WRITE_LOCK`
~48836), replacing both relay locks and covering all writers:

- All current `pkarr_relay_write_lock()` users: `set_pkarr_relays`,
  `reset_pkarr_relays`, `add_pkarr_relay`, `remove_pkarr_relay`, boot reconcile
  (~10257).
- All current `iroh_relay_write_lock()` users: `get_iroh_relays` (keeps its
  read-pairing acquisition), `set_iroh_relays`, `reset_iroh_relays`,
  `add_iroh_relay`, `remove_iroh_relay`, boot reconcile (~10311).
- The three lock-free writers. These run their file IO inside `spawn_blocking`:
  acquire the guard in the async wrapper and hold it across the `spawn_blocking`
  await (tokio `MutexGuard` is held by the async fn, not the closure).
- Delete the two old lock statics + helper fns; single new helper with a doc
  comment explaining the whole-file-RMW rationale.

**Ordering constraint (verify at implementation):** existing convention is
relay-lock → NodeState-mutex (e.g. `add_iroh_relay` → `apply_iroh_relays`). Grep
every new/changed site to confirm no path acquires the NodeState mutex *before*
the unified lock; `get_pkarr_relays` stays lock-free (read path, atomic-rename
protected) — unchanged and documented.

**Tests:** no deterministic race test is possible for lock *absence*; rely on the
compile-visible structure (single lock helper, all writers routed through it) +
existing save/RMW tests. The ZEB-630 tests below exercise the RMW logic itself.

## 6. ZEB-630 — iroh relay IPC RMW unit tests (via extracted pure helpers)

**Sites:** `add_iroh_relay` (lib.rs ~47432), `remove_iroh_relay` (~47459),
`iroh_target_relay_urls` (~47303, already pure). The commands are
`#[tauri::command]`s coupled to `State<Mutex<NodeState>>` + `app.emit`, so the
ticket's ask (mirror `add_remove_pkarr_relay_read_modify_write`, now at lib.rs
~50337) would only *simulate* the RMW — and the last-relay rejection is an inline
guard in `remove_iroh_relay`, which a simulation would copy rather than cover.

**Change (one step past the ticket, approved):** extract the RMW cores as pure
free fns next to `iroh_target_relay_urls`:

- `fn add_iroh_relay_target(settings: &ConnectivitySettings, url: &str) -> Result<Vec<String>, String>`
  — materialize via `iroh_target_relay_urls`, append, `validate_iroh_relay_urls`
  (which dedups/caps).
- `fn remove_iroh_relay_target(settings: &ConnectivitySettings, url: &str) -> Result<Vec<String>, String>`
  — materialize, filter (preserve the current normalization — trailing-slash
  trim, exactly as the command does today), **inline last-relay rejection moves
  here verbatim**: `"cannot remove the last iroh relay; use reset to follow the
  built-in defaults instead"`, then validate.

The two commands delegate to these (behavior byte-identical; the plan verifies by
reading the current command bodies and moving, not rewriting, the logic).

**Tests:** `add_remove_iroh_relay_read_modify_write` mirroring the pkarr template
(tempdir + `load_or_default`/`save` round-trips; add → assert materialized
defaults + new URL persisted; dedup on re-add; remove → assert gone; remove-last →
assert the exact `Err` from `remove_iroh_relay_target`). Covers the
materialize-defaults-into-custom behavior and the real guard.

## 7. ZEB-633 — redeem fence tests: 2s timeout, not 50ms

**Sites:** `tests/community_sync/community_sync_integration.rs` — the only two
`redeem_timeout` overrides in the tree:
`redeem_invite_only_commits_pending_join_when_inviter_unreachable` (L3001) and
`redeem_invite_only_rolls_back_owner_state_on_fence_failure` (L3080), both
`Some(Duration::from_millis(50))` on the unreachable-inviter fixture.

**Chosen design (over paused time):** raise both to
`Duration::from_secs(2)` and update the fixture/test doc comments that cite 50ms
(~2621–2632, ~2690–2695). Rationale: the timeout is `tokio::time::timeout`
(pausable in principle) and the tests are already current-thread, but the fixture
wires live engine/registry handles whose background tasks risk paused-time
auto-advance stalls (the channel_backfill.rs L2168 gotcha), and the assertion
never depends on the duration — the inviter is unreachable, so nothing can arrive
regardless of budget. 2s satisfies the wall-clock rule (budget ≫ any plausible
scheduler starvation; the #396 flake fired at 0.095s). Cost: +4s suite wall-clock.

If the flake ever recurs at 2s, capture full output (no `tail` pipes) — noted on
the ticket.

**Addendum (implementation-time diagnosis correction, 2026-07-04):** with full
output finally captured (25-iteration hunt, reproduced on iteration 6), the
flake is NOT the redeem timeout at all — both redeem asserts pass; the panic is
the test's final `registry.shutdown_all().expect("shutdown")` failing with
`Persist("io: Invalid argument (os error 22)")`. Root cause: the **ZEB-463
race** (fence-failure rollback's detached `shutdown_engine_and_cleanup_persistence`
removing the community persistence dir while `shutdown_all` flushes the same
engine) surfacing with an errno other than ENOENT — macOS/APFS returns EINVAL
when `write_atomic`'s rename runs against the dying directory, and ZEB-463's
causal downgrade keys on `NotFound` only. Fix shipped in this bundle:
`shutdown_flush_lost_race_to_dir_removal` gains a second layer — a plain
`Persist` failure downgrades iff the community dir is GONE at check time (the
only removers are intentional discards, so the flush is moot whatever the
errno); a persist fault with the dir present still propagates (ZEB-460), as
does everything else. Unit-tested. Validated with a 25-iteration paired-test
hunt post-fix. The 50ms→2s timeout raise is kept as hygiene (it makes the
budget≈starvation anti-pattern moot) but was never the mechanism.

## 8. Verification & delivery

- Gates: `cargo fmt --all -- --check`; clippy CI form
  (`cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`);
  targeted nextest per area during implementation; one full
  `--workspace --all-targets --features test-fixtures` sweep (`--no-fail-fast`)
  at the end; `npx tsc --noEmit` + `npx vitest run` (no frontend changes expected,
  gates run regardless).
- One cargo invocation at a time from `src-tauri/`; caffeinate + background/wakeup
  net for anything >10min.
- One PR titled for the bundle; body carries `Closes ZEB-627 / ZEB-628 / ZEB-629 /
  ZEB-630 / ZEB-633` plus the §0 already-done notes. Converge with bots per
  standing protocol; never auto-merge.

## Out of scope

- Counts-row vs per-peer-column reconciliation (ZEB-628 conditional item).
- Suppressing the one-shot post-eviction `Dropped` recreation (documented residual).
- `derive_reachability_status`'s unused `_my` parameter.
- `get_pkarr_relays` read-path locking (atomic rename suffices for readers).
