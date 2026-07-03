# ZEB-621: Unified Self-Address-Change Pipeline + Freshest-Wins Record Precedence — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stale routes stop pinning dials (freshest record wins at dial time, stale records trigger async pkarr refresh), and every self-address change — interface flap, home-relay flap, sleep/wake — reaches the CRDT record, the pkarr identity/community slots, and the reconnect supervisor within one 2-second debounce window instead of the 60-minute backstop.

**Architecture:** Two independent halves. (1) The resolver's single LWW slot per `(owner, node_id)` becomes a **dual slot** (durable + pkarr): butler-set/diagnostic consumers keep today's durable-preferred view (preserving ZEB-488's freshness-window exemption), while the dial path (`resolve_by_node_id` → supervisor gate + `new_link` addressing) reads a **freshest-by-`announced_at_ms`** view; a stale freshest view (>24h) triggers a cooldown-limited async pkarr re-resolve from the supervisor's dispatch loop. (2) The existing `ReachabilityPublisher` (if-watch + 2s debounce + 60min idle + force-Notify) gains an iroh `watch_addr()` stream merged into the same debounced trigger, and the publish fan-out gains an address-delta gate that re-registers the pkarr identity/community slots (mirroring Case-D) and sweeps the reconnect supervisor; a clock-jump resume detector feeds the same force handle plus `Endpoint::network_change()`.

**Tech Stack:** Rust (tokio, futures::StreamExt), iroh 1.0.1 (`Endpoint::watch_addr`, `Endpoint::network_change`), if-watch 3.x, existing harmony-pkarr publisher (re-register = force-immediate-republish).

## Global Constraints

- Branch: `zeb-621-addr-change-pipeline` (already created off main `aae102d6`). Never commit to main.
- Spec: ZEB-621 ticket (Area C, approved 2026-07-01) + `docs/specs/2026-07-02-zeb-321-phase3-decision-record.md` Area C approval log entry (lines 311-318).
- **ZEB-488 regression guard (binding):** a pkarr record must NEVER evict a durable record's butler-set semantics. Butler/diag consumers (`resolve_with_source`, `resolve_async_with_source`, `list_active_peers_with_source`) must return the durable entry (tagged `DurableCrdt`) whenever one exists, regardless of pkarr freshness. Pin with a test.
- **Cross-source freshness comparisons use `payload.announced_at_ms`, never HLC** (pkarr entries carry a synthesized HLC `{wall_ms: announced_at_ms, logical: 0, device_id: ""}` — cross-source HLC comparison is meaningless). Same-source LWW keeps the existing `(HLC, announced_at_ms, node_id)` tuple rule.
- Staleness constants: `STALE_RECORD_REFRESH_MS = 24 * 60 * 60 * 1000` (24h), refresh cooldown `PKARR_REFRESH_COOLDOWN: Duration = Duration::from_secs(15 * 60)` (15min — matches the pkarr positive-cache window).
- Debounce stays `NETWORK_CHANGE_DEBOUNCE = 2s`; idle backstop stays `IDLE_REFRESH_INTERVAL = 60min` (demoted to backstop, not removed).
- Do NOT change Case-D friend-slot cadence (`sync_case_d_handles` stays unconditional on every publish tick).
- CodeRabbit/Qodo converge later; do not pre-optimize for them. `cargo fmt --all` + `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` + scoped `cargo nextest run --locked -p harmony-app --features test-fixtures -E '<scope>'` per task; ONE cargo invocation at a time from `src-tauri/`; full workspace sweep only in the final task.
- Timing tests: wall-clock budgets must be ≫ the asserted latency (e.g. assert publish-within-10s for a 2s debounce), or use paused tokio time with injected clocks. Never assert a budget equal to the threshold.
- New tests that bind a hermetic iroh endpoint must be covered by the nextest `iroh-endpoint` group filter in `src-tauri/.config/nextest.toml` (check the filter expression; extend it if the new test names don't match).
- Commit after every task with a descriptive message ending in the standard co-author trailer.

## Research facts (source-verified 2026-07-03 — do not re-derive)

- `iroh 1.0.1` (`~/.cargo/registry/src/index.crates.io-*/iroh-1.0.1/src/endpoint.rs:1270`): `pub fn watch_addr(&self) -> impl n0_watcher::Watcher<Value = EndpointAddr> + use<>` — the returned watcher is `'static`/owned (movable into a spawned task). `iroh::Watcher` is re-exported n0-watcher. Watcher API: `.get()`, `.updated()`, `.stream()` (consumes; first item = current value), `.stream_updates_only()` (skips current value). The stream ends only when the last Endpoint clone drops (`close()` alone does not end it).
- `EndpointAddr` (`iroh-base-1.0.1/src/endpoint_addr.rs:41`): `{ id: EndpointId, addrs: BTreeSet<TransportAddr> }`, derives `Clone, PartialEq, Eq, Hash, Ord`. `TransportAddr` is `#[non_exhaustive] enum { Relay(RelayUrl), Ip(SocketAddr), Custom(CustomAddr) }`. Helpers: `relay_urls()`, `ip_addrs()`. There is NO public home-relay-only watcher in 1.0.1 — relay changes arrive through `watch_addr`.
- `pub async fn network_change(&self)` exists (`endpoint.rs:1641`) — idempotent re-probe hint, safe to call spuriously, logs+ignores if closed.
- Consume iroh watcher streams with the crate-local `futures::StreamExt` idiom (see `peer_liveness.rs:391-400` comment) — NOT `n0_future`.
- `PkarrPublisher::register(handle, key_builder, builder)` sets `next_publish_at = now` + wakes the drive loop — **re-registering a slot forces an immediate republish**. Client wrappers: `PkarrIdentityPublisher::enable()` (`pkarr_identity_publisher.rs:40`), `PkarrCommunityPublisher::on_community_joined(space_id, epoch_key)` (`pkarr_community_publisher.rs:37`), Case-D `sync_case_d_handles` (`pkarr_friend_publisher.rs:95`) — the Case-D `RecordBuilder` re-reads current reachability per publish; identity/community builders must be confirmed equivalent by the implementer at wiring time.
- Resolver supervisor kick seam already exists (`reachability_resolver.rs:220-226`): `NewPeer` on first-learn, `RecordChanged` on LWW-replace where `addressing_differs`. `SupervisorHandle::kick_sweep()` re-arms all known NON-connected peers (`reconnect_supervisor.rs:279`, `do_sweep` at `:688`); connected peers are only re-armed by `Dropped`.
- The supervisor re-reads `resolver.resolve_by_node_id(peer)` on EVERY dial dispatch (`reconnect_supervisor.rs:515`) — freshest-wins changes propagate with no snapshot invalidation. `new_link` re-reads it per outbound link (`zenoh_iroh_transport.rs:842-858`) to synthesize the `EndpointAddr`.
- The frozen boot snapshot: `self_reachability_for_friend` built at `lib.rs:7848-7858`, wired via `.with_self_reachability(...)` at `lib.rs:7900`. The acceptor already prefers a fresh relay read via `HomeRelayRefresh` (`iroh_friend_acceptor.rs:549`, `effective_home_relay` at `:569`). Dialer-path bundles are already built fresh per call (`build_self_handshake_reachability`, `lib.rs:45902`; call sites `:47479`, `:49009`).
- No sleep/wake detection exists anywhere in the tree.

---

### Task 1: Resolver dual-slot storage + freshest-wins dial view

**Files:**
- Modify: `src-tauri/src/reachability_resolver.rs` (storage shape, `update_with_source`, all read paths, `should_replace` → `lww_newer`, tests)

**Interfaces:**
- Produces: `ResolverSlots { durable: Option<ResolverEntry>, pkarr: Option<ResolverEntry> }` (private); `ResolverSlots::freshest(&self) -> Option<&ResolverEntry>`; `ReachabilityResolver::resolve_by_node_id` KEEPS its signature `(&self, &[u8; 32]) -> Option<(OwnerAddr, ReachabilityAnnouncePayload)>` but now returns the freshest view; `resolve_entry_by_node_id(&self, &[u8; 32]) -> Option<(OwnerAddr, ResolverEntry)>` (new, freshest view with source + timestamps, used by Task 2).
- Consumes: nothing from other tasks.

**Design (binding):**
- Map value type changes from `ResolverEntry` to `ResolverSlots`. Each source writes ONLY its own slot; same-source replacement uses the existing HLC/announced_at/node_id LWW (rename the tail of `should_replace` to `lww_newer(prev, next) -> bool`, deleting the two cross-source match arms and the ZEB-488 source-guard comment — replace it with a comment explaining the dual-slot split: durable slot = butler authority + window exemption; freshest view = dial authority).
- `ResolverSlots::freshest()`: the entry with the greater `payload.announced_at_ms`; tie → durable.
- `ResolverSlots::durable_preferred()`: `durable.as_ref().or(pkarr.as_ref())`.
- Read-path semantics: `resolve`, `resolve_with_source`, `list_active_peers`, `list_active_peers_with_source`, `resolve_async`, `resolve_async_with_source` → durable-preferred (today's post-guard behavior, byte-for-byte compatible for butler/diag/e2e consumers). `resolve_by_node_id` + new `resolve_entry_by_node_id` → freshest (the dial path).
- Kick gate in `update_with_source`: compute the freshest-view payload BEFORE and AFTER the slot write. `was_present` = any slot existed before. Kick `NewPeer` if `!was_present` and a view now exists; kick `RecordChanged` if `was_present` and `addressing_differs(&before_view, &after_view)`. (This naturally fires when a fresher pkarr record changes the effective dial addressing — the payoff of the whole slice.)
- `remove_owner` clears whole `ResolverSlots` entries (both slots) for the owner range.

- [ ] **Step 1: Write the failing tests** (in the existing `mod tests`; reuse `make_payload`/`make_hlc` helpers)

```rust
/// ZEB-621: dial view (resolve_by_node_id) prefers the FRESHER record across
/// sources — a fresher pkarr record beats a stale durable one at dial time.
#[test]
fn dial_view_prefers_fresher_pkarr_over_stale_durable() {
    let r = ReachabilityResolver::new();
    let owner = [1u8; 32];
    let durable = make_payload(7, 1_000); // announced_at_ms = 1_000 (stale)
    r.update(owner, durable, make_hlc(1_000, 0, "dev-a"));
    let mut pkarr = make_payload(7, 50_000); // same node, fresher
    pkarr.home_relay_url = "https://fresh.relay/".to_string();
    let hlc = Hlc { wall_ms: 50_000, logical: 0, device_id: String::new() };
    r.update_with_source(owner, pkarr, hlc, ReachabilitySource::PkarrLive);

    let (o, p) = r.resolve_by_node_id(&node_id_bytes(7)).expect("record");
    assert_eq!(o, owner);
    assert_eq!(p.home_relay_url, "https://fresh.relay/");
    assert_eq!(p.announced_at_ms, 50_000);
}

/// ZEB-621 + ZEB-488 pin: the durable slot is NEVER evicted by pkarr — the
/// butler/diag view (resolve_with_source) still returns the durable entry,
/// tagged DurableCrdt, even when a fresher pkarr record wins the dial view.
#[test]
fn butler_view_stays_durable_despite_fresher_pkarr() {
    let r = ReachabilityResolver::new();
    let owner = [1u8; 32];
    r.update(owner, make_payload(7, 1_000), make_hlc(1_000, 0, "dev-a"));
    let hlc = Hlc { wall_ms: 50_000, logical: 0, device_id: String::new() };
    r.update_with_source(owner, make_payload(7, 50_000), hlc, ReachabilitySource::PkarrLive);

    let v = r.resolve_with_source(&owner);
    assert_eq!(v.len(), 1, "one view per (owner,node), durable-preferred");
    assert_eq!(v[0].1, ReachabilitySource::DurableCrdt);
    assert_eq!(v[0].0.announced_at_ms, 1_000);
}

/// ZEB-621: an OLDER pkarr record loses the dial view to a fresher durable.
#[test]
fn dial_view_keeps_fresher_durable_over_stale_pkarr() {
    let r = ReachabilityResolver::new();
    let owner = [1u8; 32];
    let hlc = Hlc { wall_ms: 1_000, logical: 0, device_id: String::new() };
    r.update_with_source(owner, make_payload(7, 1_000), hlc, ReachabilitySource::PkarrLive);
    r.update(owner, make_payload(7, 50_000), make_hlc(50_000, 0, "dev-a"));

    let (_, p) = r.resolve_by_node_id(&node_id_bytes(7)).expect("record");
    assert_eq!(p.announced_at_ms, 50_000);
}

/// ZEB-621: a fresher pkarr record that changes effective addressing kicks
/// RecordChanged (the stale-route-unpin payoff).
#[test]
fn fresher_pkarr_with_new_addressing_kicks_record_changed() {
    let r = ReachabilityResolver::new();
    let sup = SupervisorHandle::new();
    r.set_supervisor(sup.clone());
    let owner = [1u8; 32];
    r.update(owner, make_payload(7, 1_000), make_hlc(1_000, 0, "dev-a"));
    let _ = sup.pending_trigger(&node_id_bytes(7)); // drain the NewPeer kick
    let mut pkarr = make_payload(7, 50_000);
    pkarr.home_relay_url = "https://fresh.relay/".to_string();
    let hlc = Hlc { wall_ms: 50_000, logical: 0, device_id: String::new() };
    r.update_with_source(owner, pkarr, hlc, ReachabilitySource::PkarrLive);
    assert_eq!(
        sup.pending_trigger(&node_id_bytes(7)),
        Some(ReconnectTrigger::RecordChanged)
    );
}

/// ZEB-621: a fresher pkarr record with IDENTICAL addressing does NOT kick
/// (no ladder thrash on a byte-identical refresh).
#[test]
fn fresher_pkarr_same_addressing_does_not_kick() {
    let r = ReachabilityResolver::new();
    let sup = SupervisorHandle::new();
    r.set_supervisor(sup.clone());
    let owner = [1u8; 32];
    r.update(owner, make_payload(7, 1_000), make_hlc(1_000, 0, "dev-a"));
    let _ = sup.pending_trigger(&node_id_bytes(7));
    let hlc = Hlc { wall_ms: 50_000, logical: 0, device_id: String::new() };
    r.update_with_source(owner, make_payload(7, 50_000), hlc, ReachabilitySource::PkarrLive);
    assert_eq!(sup.pending_trigger(&node_id_bytes(7)), None);
}

/// remove_owner clears BOTH slots.
#[test]
fn remove_owner_clears_both_slots() {
    let r = ReachabilityResolver::new();
    let owner = [1u8; 32];
    r.update(owner, make_payload(7, 1_000), make_hlc(1_000, 0, "dev-a"));
    let hlc = Hlc { wall_ms: 50_000, logical: 0, device_id: String::new() };
    r.update_with_source(owner, make_payload(7, 50_000), hlc, ReachabilitySource::PkarrLive);
    assert_eq!(r.remove_owner(&owner), 1); // one composite entry (both slots)
    assert!(r.resolve_by_node_id(&node_id_bytes(7)).is_none());
    assert!(r.resolve(&owner).is_empty());
}
```

Add a tiny helper next to `make_payload` if absent: `fn node_id_bytes(b: u8) -> [u8; 32] { [b; 32] }` — match however `make_payload` derives node ids (READ it first; reuse its convention so ids agree). Likewise, the `pending_trigger` drain/assert calls above are sketches — match the exact call pattern of the existing kick tests at `reachability_resolver.rs:901-1003` (they demonstrate the test-only `SupervisorHandle` accessors' real signatures).

**Existing tests that MUST be rewritten (spec-mandated behavior change — flag any others to the orchestrator instead of silently changing):** `durable_not_evicted_by_fresher_pkarr` (its butler-view half is superseded by `butler_view_stays_durable_despite_fresher_pkarr`; delete or rewrite), `pkarr_upgraded_by_older_durable` (durable now installs into its own slot; assert the butler view flips to durable while the dial view stays with the fresher pkarr record). All other same-source LWW tests must pass UNCHANGED.

- [ ] **Step 2: Run the new tests, verify they fail** — `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(reachability_resolver)'` — expect the new tests to fail (compile errors for `resolve_entry_by_node_id` count as failing).

- [ ] **Step 3: Implement** the dual-slot storage per the Design block above. Keep `ResolverEntry` unchanged. The `resolve_async*` cache-miss check (`cached.is_empty()`) and the CA3 TOCTOU re-read keep working through `durable_preferred` semantics unchanged.

- [ ] **Step 4: Run the full resolver + supervisor test scope** — `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(reachability_resolver) or test(reconnect_supervisor)'` — all green.

- [ ] **Step 5: Commit** — `git add -A && git commit -m "feat(zeb-621): resolver dual-slot storage — freshest-wins dial view, durable-preferred butler view"`

### Task 2: Stale-record async pkarr refresh (resolver seam + supervisor call site)

**Files:**
- Modify: `src-tauri/src/reachability_resolver.rs` (constants, cooldown map, `maybe_refresh_stale`)
- Modify: `src-tauri/src/reconnect_supervisor.rs` (dispatch-loop call after the `:515` resolve)

**Interfaces:**
- Consumes: `resolve_entry_by_node_id` (Task 1).
- Produces: `ReachabilityResolver::maybe_refresh_stale(&self, owner: OwnerAddr, node_id: [u8; 32], now_ms: u64)` — sync, non-blocking; internally checks staleness + cooldown, then `tokio::spawn`s the fallback resolve.

**Design (binding):**
- Constants (top of `reachability_resolver.rs`): `const STALE_RECORD_REFRESH_MS: u64 = 24 * 60 * 60 * 1000;` and `const PKARR_REFRESH_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(15 * 60);`
- New field on `ReachabilityResolver`: `refresh_cooldowns: Arc<std::sync::Mutex<std::collections::HashMap<OwnerAddr, tokio::time::Instant>>>` (tokio Instant → paused-time testable). Shared across clones (Arc), like the other fields.
- `maybe_refresh_stale` logic: (1) freshest view for `node_id` — if none, or `now_ms.saturating_sub(freshest.payload.announced_at_ms) <= STALE_RECORD_REFRESH_MS`, return. (2) cooldown check-and-set under the mutex (`Instant::now()` vs stored + cooldown; insert BEFORE spawning so concurrent callers can't double-fire). (3) clone the fallback Arc (as `resolve_async` does); if none, return. (4) `tokio::spawn`: `let payloads = fb.resolve(&owner).await;` then for each payload feed `update_with_source(owner, payload, synthesized_hlc, PkarrLive)` — reuse the exact HLC-synthesis pattern from `resolve_async` (`wall_ms = announced_at_ms, logical: 0, device_id: ""`). The RecordChanged kick then fires automatically via Task 1's gate iff the refresh actually changed the dial view.
- Supervisor call site: in `run_reconnect_supervisor`'s dispatch pass, immediately after the `resolver.resolve_by_node_id(peer)` read returns `Some((owner, _payload))` (`reconnect_supervisor.rs:515` region), add `resolver.maybe_refresh_stale(owner, *peer, now_ms());` (use the module's existing `now_ms()` helper). It must NOT gate or delay the dial being dispatched — refresh is fire-and-forget alongside the attempt.

- [ ] **Step 1: Write the failing tests** (resolver `mod fallback_tests` — reuse `StubFallback` or add a counting stub):

```rust
/// ZEB-621: a stale freshest view (>24h) triggers exactly one async pkarr
/// refresh; a second call inside the cooldown window is suppressed.
#[tokio::test(start_paused = true)]
async fn stale_record_triggers_refresh_once_per_cooldown() {
    let r = ReachabilityResolver::new();
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    r.set_fallback_source(Arc::new(CountingFallback {
        calls: Arc::clone(&counter),
        payloads: vec![make_payload(7, 90_000_000_000)],
    }));
    let owner = [1u8; 32];
    r.update(owner, make_payload(7, 1_000), make_hlc(1_000, 0, "dev-a"));

    let now = 1_000 + STALE_RECORD_REFRESH_MS + 1;
    r.maybe_refresh_stale(owner, node_id_bytes(7), now);
    tokio::task::yield_now().await; // let the spawned refresh run
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    // refresh result installed into the pkarr slot → dial view updated
    let (_, p) = r.resolve_by_node_id(&node_id_bytes(7)).expect("record");
    assert_eq!(p.announced_at_ms, 90_000_000_000);

    // second call within the cooldown: suppressed even though still "stale"
    r.maybe_refresh_stale(owner, node_id_bytes(7), now + 1);
    tokio::task::yield_now().await;
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);

    // advance past the cooldown: fires again
    tokio::time::advance(PKARR_REFRESH_COOLDOWN + std::time::Duration::from_secs(1)).await;
    r.maybe_refresh_stale(owner, node_id_bytes(7), now + 2);
    tokio::task::yield_now().await;
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// A fresh record (<24h) never fires the fallback.
#[tokio::test(start_paused = true)]
async fn fresh_record_never_triggers_refresh() {
    let r = ReachabilityResolver::new();
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    r.set_fallback_source(Arc::new(CountingFallback {
        calls: Arc::clone(&counter),
        payloads: vec![],
    }));
    let owner = [1u8; 32];
    r.update(owner, make_payload(7, 1_000), make_hlc(1_000, 0, "dev-a"));
    r.maybe_refresh_stale(owner, node_id_bytes(7), 1_000 + STALE_RECORD_REFRESH_MS);
    tokio::task::yield_now().await;
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);
}
```

`CountingFallback`: a `ReachabilityFallback` impl storing `calls: Arc<AtomicUsize>` + `payloads: Vec<ReachabilityAnnouncePayload>`; `resolve` increments and returns the payloads. Model it on the existing `StubFallback` (`reachability_resolver.rs:1024-1033`). Note the tests may need more than one `yield_now()` to drive the spawned task — if flaky under `--test-threads` contention, use `tokio::time::timeout(Duration::from_secs(5), async { while counter.load(..) < expected { tokio::task::yield_now().await } })` as the wait idiom (paused-time timeouts don't consume wall clock).

- [ ] **Step 2: Run, verify failure** — `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(reachability_resolver)'`
- [ ] **Step 3: Implement** per the Design block. Make the constants `pub(crate)` so tests reference them.
- [ ] **Step 4: Add the supervisor call site + one supervisor-side test**: in `reconnect_supervisor.rs` tests, seed a peer with a STALE record (`seed` helper writes `announced_at_ms` — read the helper; give it a stale timestamp variant), attach a `CountingFallback`-style stub via `resolver.set_fallback_source`, run the paused-time supervisor loop long enough for one dispatch, assert the stub fired ≥1. Follow the existing paused-time supervisor test harness (`cfg(...)` + `RecordingDialer` + `jitter_seed`).
- [ ] **Step 5: Run both scopes green** — `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(reachability_resolver) or test(reconnect_supervisor)'`
- [ ] **Step 6: Commit** — `git commit -m "feat(zeb-621): stale-record async pkarr refresh — 24h staleness gate, 15min cooldown, supervisor dispatch hook"`

### Task 3: iroh watch_addr stream → publisher network-change arm

**Files:**
- Modify: `src-tauri/src/iroh_endpoint.rs` (two wrapper methods)
- Modify: `src-tauri/src/reachability_publisher.rs` (merged change-stream, module doc rewrite, tests)
- Modify: `src-tauri/src/lib.rs` (the single `ReachabilityPublisher::new(...)` call site at ~`lib.rs:7004` passes the addr stream)

**Interfaces:**
- Produces: `IrohEndpoint::watch_addr_stream(&self) -> futures::stream::BoxStream<'static, iroh::EndpointAddr>` (via `self.inner.watch_addr().stream_updates_only()`, boxed — skips the initial value so boot doesn't double-publish); `IrohEndpoint::network_change(&self)` async wrapper (used by Task 6); `ReachabilityPublisher::new(endpoint, publish, addr_stream: Option<futures::stream::BoxStream<'static, iroh::EndpointAddr>>)` — third parameter added.
- Consumes: nothing from other tasks.

**Design (binding):**
- In `spawn()`, build ONE merged change stream: map the if-watch stream to `()` (with a `tracing::debug!` naming the source inside the map), map the addr stream to `()` likewise, then `futures::stream::select` them. Cases: both present → select; if-watch init failed but addr stream present → addr-only (log the degrade); both absent → existing `idle_loop()`. The existing single `item = iface_stream.next()` arm becomes `item = change_stream.next()` with the SAME debounce-drain body (the drain now naturally coalesces events from BOTH sources — this closes the "if-watch fires, then watch_addr fires 500ms later → two publishes" hole by construction). `None` from the merged stream (both sources ended) → existing idle-only fallback.
- Rewrite the module-doc paragraph at `reachability_publisher.rs:19-24`: home-relay-change IS now handled via `Endpoint::watch_addr` (ZEB-621); the 60-min idle tick is demoted to backstop. Update the trigger list doc.
- lib.rs call site: `ReachabilityPublisher::new(ep_arc_for_publisher.clone(), publish_fn, Some(ep_arc_for_publisher.watch_addr_stream()))` — adapt to the actual local names at the site.

- [ ] **Step 1: Write the failing tests** (in `reachability_publisher.rs::tests`; wall-clock with generous budgets per Global Constraints; fake addr stream via `tokio::sync::mpsc` + `tokio_stream::wrappers::ReceiverStream` if `tokio_stream` is a dep — otherwise a small manual `futures::stream::unfold` over the receiver):

```rust
/// ZEB-621 acceptance: a home-relay change (addr-stream event) triggers a
/// publish within the debounce window — NOT the 60-minute backstop.
#[tokio::test]
async fn addr_change_triggers_publish_within_debounce() {
    crate::iroh_endpoint::warm_up_iroh_global_init().await;
    tokio::time::timeout(Duration::from_secs(40), async {
        let published = Arc::new(Notify::new());
        let p2 = Arc::clone(&published);
        let publish: PublishFn = Arc::new(move || {
            let n = Arc::clone(&p2);
            Box::pin(async move { n.notify_one(); }) as futures::future::BoxFuture<'static, ()>
        });
        let ep = build_hermetic_iroh_endpoint().await;
        let (tx, rx) = tokio::sync::mpsc::channel::<iroh::EndpointAddr>(8);
        let addr_stream = Box::pin(futures::stream::unfold(rx, |mut rx| async {
            rx.recv().await.map(|a| (a, rx))
        })) as futures::stream::BoxStream<'static, iroh::EndpointAddr>;
        let publisher = Arc::new(ReachabilityPublisher::new(ep.clone(), publish, Some(addr_stream)));
        let _handle = publisher.spawn();
        // startup publish
        tokio::time::timeout(Duration::from_secs(5), published.notified())
            .await.expect("startup publish");
        // inject an addr change → publish within debounce(2s) + slack
        tx.send(fake_endpoint_addr()).await.expect("send addr event");
        tokio::time::timeout(Duration::from_secs(10), published.notified())
            .await.expect("addr-change publish within 10s (2s debounce + slack)");
    }).await.expect("test must complete inside outer budget");
}
```

`fake_endpoint_addr()`: build an `iroh::EndpointAddr` from a random `EndpointId` (e.g. `iroh::SecretKey::generate().public()` → `EndpointAddr::new(id)` — verify the exact constructor from iroh-base; `from_parts` also exists). Add a second test `addr_flap_coalesces_to_one_publish`: send 3 events 50ms apart, count publishes via `AtomicUsize`, sleep past the window (e.g. 6s), assert exactly 1 post-startup publish (budget per Global Constraints; keep counts strictly, the debounce drain guarantees coalescing).

- [ ] **Step 2: Run, verify failure** — `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(reachability_publisher)'` (new-signature compile errors count).
- [ ] **Step 3: Implement** wrapper methods + merged stream + lib.rs call site + module-doc rewrite. Existing tests: update `force_notify_triggers_publish` construction to pass `None` for the addr stream.
- [ ] **Step 4: Verify the nextest `iroh-endpoint` group filter** in `src-tauri/.config/nextest.toml` covers `test(reachability_publisher)` (these tests bind hermetic endpoints); extend the filter if not.
- [ ] **Step 5: Run scope green** — `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(reachability_publisher)'`
- [ ] **Step 6: Commit** — `git commit -m "feat(zeb-621): iroh watch_addr feeds the reachability publisher — relay flap republishes within the 2s debounce"`

### Task 4: Address-delta fan-out — pkarr slot re-register + supervisor sweep

**Files:**
- Create: `src-tauri/src/addr_change_fanout.rs`
- Modify: `src-tauri/src/lib.rs` (declare module; capture + call inside `publish_fn` at ~`lib.rs:6756-7001`; install hooks where the pkarr publishers + supervisor handle are wired)

**Interfaces:**
- Consumes: nothing from other tasks (composes with Task 3 at runtime only).
- Produces: `AddrChangeFanout` with: `pub fn new() -> Arc<Self>`; `pub fn set_pkarr_republish(&self, f: Box<dyn Fn() + Send + Sync>)` (OnceLock install-once); `pub fn set_supervisor_sweep(&self, f: Box<dyn Fn() + Send + Sync>)` (OnceLock); `pub fn observe(&self, home_relay: Option<String>, direct_addrs: std::collections::BTreeSet<std::net::SocketAddr>) -> bool` — compares against the last-observed snapshot (Mutex), updates it, and on a CHANGE (excluding the first observation) fires both hooks and returns true.

**Design (binding):**
- First observation (boot publish) records the snapshot and does NOT fire (boot paths already register every pkarr slot and seed the supervisor).
- `observe` is sync and non-blocking; hooks are plain `Fn()` closures (the pkarr re-register wrappers and `SupervisorHandle::kick_sweep` are all sync).
- lib.rs wiring: (1) construct `let addr_fanout = AddrChangeFanout::new();` before `publish_fn`; clone into the closure. (2) Inside `publish_fn`, right after the existing step-1 iroh snapshot (node_id / home_relay / gathered direct addrs — locate the locals around `lib.rs:6781-6793`), call `addr_fanout.observe(home_relay_string_opt.clone(), direct_addrs_btreeset.clone())` — adapt to the actual local names/types at the site; the direct-addr list is a Vec there, collect to `BTreeSet`. (3) Install `set_pkarr_republish` at the point where BOTH `PkarrIdentityPublisher` and `PkarrCommunityPublisher` are enabled at boot (near `lib.rs:7362`/`7387`): the closure re-calls `identity.enable()` and, for EACH currently-joined community, `community_pub.on_community_joined(space_id, epoch_key)` — reuse however the boot path enumerates joined communities + their epoch keys; if the enumeration requires async or locks unavailable in a sync closure, make the closure spawn a task (document which). Confirm at wiring time that re-calling `enable()`/`on_community_joined` rebuilds builders that read CURRENT addresses (mirror Case-D's `blob_builder` pattern) — if a builder captures a frozen snapshot, rebuild it inside the closure. (4) Install `set_supervisor_sweep` where the supervisor handle exists (it's installed into the resolver at `event_loop.rs:1139`; lib.rs holds it nearby — closure body: `handle.kick_sweep()`).
- Case-D `sync_case_d_handles` at `lib.rs:6993` stays UNCONDITIONAL (Global Constraints).

- [ ] **Step 1: Write the failing tests** (new module's `#[cfg(test)]`):

```rust
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::Arc;

fn addrs(list: &[&str]) -> BTreeSet<SocketAddr> {
    list.iter().map(|s| s.parse().unwrap()).collect()
}

/// Wire counters into both hooks; return (fanout, pkarr_count, sweep_count).
fn counted() -> (Arc<AddrChangeFanout>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let f = AddrChangeFanout::new();
    let pk = Arc::new(AtomicUsize::new(0));
    let sw = Arc::new(AtomicUsize::new(0));
    let pk2 = Arc::clone(&pk);
    let sw2 = Arc::clone(&sw);
    f.set_pkarr_republish(Box::new(move || {
        pk2.fetch_add(1, SeqCst);
    }));
    f.set_supervisor_sweep(Box::new(move || {
        sw2.fetch_add(1, SeqCst);
    }));
    (f, pk, sw)
}

#[test]
fn first_observation_records_but_does_not_fire() {
    let (f, pk, sw) = counted();
    let fired = f.observe(Some("https://relay.a/".into()), addrs(&["10.0.0.1:4433"]));
    assert!(!fired);
    assert_eq!(pk.load(SeqCst), 0);
    assert_eq!(sw.load(SeqCst), 0);
}

#[test]
fn relay_change_fires_both_hooks_once() {
    let (f, pk, sw) = counted();
    f.observe(Some("https://relay.a/".into()), addrs(&["10.0.0.1:4433"]));
    let fired = f.observe(Some("https://relay.b/".into()), addrs(&["10.0.0.1:4433"]));
    assert!(fired);
    assert_eq!(pk.load(SeqCst), 1);
    assert_eq!(sw.load(SeqCst), 1);
    // Same snapshot again: no re-fire.
    let fired = f.observe(Some("https://relay.b/".into()), addrs(&["10.0.0.1:4433"]));
    assert!(!fired);
    assert_eq!(pk.load(SeqCst), 1);
    assert_eq!(sw.load(SeqCst), 1);
}

#[test]
fn direct_addr_set_change_fires() {
    let (f, pk, _sw) = counted();
    f.observe(Some("https://relay.a/".into()), addrs(&["10.0.0.1:4433"]));
    let fired = f.observe(
        Some("https://relay.a/".into()),
        addrs(&["10.0.0.1:4433", "192.168.1.5:4433"]),
    );
    assert!(fired);
    assert_eq!(pk.load(SeqCst), 1);
}

#[test]
fn addr_set_equality_is_order_insensitive() {
    let (f, pk, _sw) = counted();
    f.observe(None, addrs(&["10.0.0.1:4433", "192.168.1.5:4433"]));
    // Same members, built in the opposite order → BTreeSet equality → no fire.
    let fired = f.observe(None, addrs(&["192.168.1.5:4433", "10.0.0.1:4433"]));
    assert!(!fired);
    assert_eq!(pk.load(SeqCst), 0);
}

#[test]
fn change_with_no_hooks_installed_returns_true_without_panic() {
    let f = AddrChangeFanout::new();
    f.observe(Some("https://relay.a/".into()), addrs(&[]));
    assert!(f.observe(Some("https://relay.b/".into()), addrs(&[])));
}
```

- [ ] **Step 2: Run, verify failure** — `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(addr_change_fanout)'`
- [ ] **Step 3: Implement the module**, then the lib.rs wiring per the Design block. The wiring compiles against real locals — READ the `publish_fn` body and the pkarr-publisher boot block before editing; report BLOCKED with specifics if the community-enumeration closure can't be built sync-safely rather than inventing a new async pipeline.
- [ ] **Step 4: Run scope + neighbors green** — `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(addr_change_fanout) or test(reachability_publisher)'`
- [ ] **Step 5: Run `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`** (lib-only is acceptable mid-plan; the final task runs `--all-targets`).
- [ ] **Step 6: Commit** — `git commit -m "feat(zeb-621): addr-delta fan-out — pkarr identity/community re-register + supervisor sweep on real address change"`

### Task 5: Retire the frozen SelfHandshakeReachability boot snapshot

**Files:**
- Modify: `src-tauri/src/iroh_friend_acceptor.rs` (acceptor stops holding a snapshot; statics + mandatory fresh relay read)
- Modify: `src-tauri/src/lib.rs` (delete the boot construction at ~`:7848-7858` + `.with_self_reachability(...)` at ~`:7900`; keep `build_self_handshake_reachability` for the per-call dialer bundles)
- Modify: `src-tauri/src/dm_tunnel_contact.rs` (only if signatures force it — prefer no change; the dialer path already builds fresh bundles per call)

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces: acceptor builder gains `with_self_statics(SelfHandshakeStatics)` where `pub struct SelfHandshakeStatics { pub identity_pub_64: [u8; 64], pub iroh_node_id: [u8; 32], pub pq_dsa_pubkey: Vec<u8>, pub pq_kem_pubkey: Vec<u8> }` (i.e. `SelfHandshakeReachability` minus `home_relay_url`); the acceptor's relay value comes EXCLUSIVELY from the existing `HomeRelayRefresh` closure.

**Design (binding):**
- `SelfHandshakeReachability` (with `home_relay_url`) SURVIVES as the per-call dialer bundle type (`build_self_handshake_reachability` at `lib.rs:45902` constructs it fresh per dial — already ZEB-521-correct). What dies is the acceptor-held FROZEN instance.
- In `iroh_friend_acceptor.rs`: replace the `self_reachability: Option<SelfHandshakeReachability>` field (`:1242`) with `self_statics: Option<SelfHandshakeStatics>`; `build_accept`'s `self_reachability: Option<&SelfHandshakeReachability>` parameter becomes `self_statics: Option<&SelfHandshakeStatics>` PLUS an explicit `home_relay_url: Option<String>` parameter; the two accept-sign dispatch sites (`:1623-1637`, `:1658-1672`) pass `self.current_fresh_home_relay()` directly. `effective_home_relay(fresh, snapshot_fallback)` (`:569`) loses its snapshot-fallback arm — fresh read only (delete the function if it degenerates to identity; keep the empty-string filter). Update its rationale doc + tests (`:2802-2896`) accordingly.
- In lib.rs: the boot block `self_reachability_for_friend` (`:7848`) is REPLACED by a `SelfHandshakeStatics` construction (same immutable fields, NO relay read), wired via the renamed builder. `.with_self_home_relay_refresh(...)` wiring at `:7909-7920` stays (it is now the only relay source).
- Grep-clean gates (run all three): `rg 'self_reachability_for_friend' src-tauri/src/` → empty; `rg 'with_self_reachability\(' src-tauri/src/` → empty; `rg 'home_relay_url' src-tauri/src/iroh_friend_acceptor.rs` → only sites reading the fresh closure / statics-free paths (no stored snapshot field).

- [ ] **Step 1: Write/adjust the failing tests first**: in `iroh_friend_acceptor.rs` tests, the existing fresh-vs-snapshot tests (`:2802-2896`) become fresh-only: an acceptor built with statics + a `HomeRelayRefresh` returning `Some("https://fresh/")` must sign accepts carrying the fresh relay; one built with a refresh returning `None` must carry `None` (no stale fallback). Adapt the existing test bodies — do not delete coverage of the accept-bundle fields (identity/node/pq statics must still round-trip).
- [ ] **Step 2: Run, verify failures** — `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(iroh_friend_acceptor) or test(dm_tunnel_contact)'`
- [ ] **Step 3: Implement** per the Design block (acceptor first, then lib.rs wiring).
- [ ] **Step 4: Run the grep-clean gates** (all three above) + the test scope green.
- [ ] **Step 5: Commit** — `git commit -m "feat(zeb-621): retire the frozen SelfHandshakeReachability boot snapshot — acceptor relay is fresh-read only (ZEB-521 completion)"`

### Task 6: Sleep/wake resume detector

**Files:**
- Create: `src-tauri/src/resume_detector.rs`
- Modify: `src-tauri/src/iroh_endpoint.rs` (add `network_change` wrapper if Task 3 didn't) 
- Modify: `src-tauri/src/lib.rs` (spawn the detector near the publisher spawn at ~`:7022`; `on_resume` closure = `endpoint.network_change().await` then `force_handle.notify_one()`)

**Interfaces:**
- Consumes: `IrohEndpoint::network_change` (Task 3), `ReachabilityPublisher::force_handle` (existing).
- Produces: `pub fn resume_gap_detected(expected_tick: Duration, observed_wall_delta_ms: u64) -> bool`; `pub async fn run_resume_detector(now_ms: Arc<dyn Fn() -> u64 + Send + Sync>, tick: Duration, on_resume: Arc<dyn Fn() + Send + Sync>)` (loop; lib.rs spawns it with `SystemTime`-backed `now_ms` and a closure that spawns the async network_change + notify).

**Design (binding):**
- Detection rule: `observed_wall_delta_ms > (expected_tick.as_millis() as u64) * 2 + 5_000` — a wall-clock jump of more than one missed tick + 5s margin means the process was suspended (or the clock stepped — either way, re-probing is safe/idempotent). Tick = 30s in production (`const RESUME_DETECTOR_TICK: Duration = Duration::from_secs(30);`).
- The loop: `let mut prev = now_ms(); loop { tokio::time::sleep(tick).await; let cur = now_ms(); if resume_gap_detected(tick, cur.saturating_sub(prev)) { tracing::info!(...); on_resume(); } prev = cur; }`
- lib.rs `on_resume` closure: clone `Arc<IrohEndpoint>` + the force `Arc<Notify>`; body spawns `async move { ep.network_change().await; force.notify_one(); }`. The `network_change` re-probe runs FIRST so the publish that follows the (2s-debounced, addr-delta-gated) pipeline sees post-probe addresses; the immediate force covers the case where addresses did NOT change but the 7-day pkarr record aged past a suspend.

- [ ] **Step 1: Write the failing tests**:

```rust
#[test]
fn no_gap_below_threshold() {
    assert!(!resume_gap_detected(Duration::from_secs(30), 30_000));
    assert!(!resume_gap_detected(Duration::from_secs(30), 64_999)); // 2*30s+5s boundary
}

#[test]
fn gap_above_threshold_detected() {
    assert!(resume_gap_detected(Duration::from_secs(30), 65_001));
    assert!(resume_gap_detected(Duration::from_secs(30), 3 * 60 * 60 * 1000)); // 3h suspend
}

/// Paused-time loop test with an injected, jumpable clock.
#[tokio::test(start_paused = true)]
async fn loop_fires_on_resume_and_not_on_normal_ticks() {
    let wall = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let w2 = Arc::clone(&wall);
    let now_ms: Arc<dyn Fn() -> u64 + Send + Sync> =
        Arc::new(move || w2.load(std::sync::atomic::Ordering::SeqCst));
    let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let f2 = Arc::clone(&fired);
    let on_resume: Arc<dyn Fn() + Send + Sync> =
        Arc::new(move || { f2.fetch_add(1, std::sync::atomic::Ordering::SeqCst); });
    let tick = Duration::from_secs(30);
    tokio::spawn(run_resume_detector(now_ms, tick, on_resume));

    // Normal tick: wall advances in lockstep with virtual time → no fire.
    wall.fetch_add(30_000, std::sync::atomic::Ordering::SeqCst);
    tokio::time::advance(tick).await;
    tokio::task::yield_now().await;
    assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 0);

    // Suspend: virtual sleep elapses once, but the wall clock jumped 2h.
    wall.fetch_add(2 * 60 * 60 * 1000, std::sync::atomic::Ordering::SeqCst);
    tokio::time::advance(tick).await;
    tokio::task::yield_now().await;
    assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
}
```

- [ ] **Step 2: Run, verify failure** — `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(resume_detector)'`
- [ ] **Step 3: Implement** module + lib.rs spawn wiring (module declaration in lib.rs's mod list, alphabetical placement matching neighbors).
- [ ] **Step 4: Run scope green** — same filter.
- [ ] **Step 5: Commit** — `git commit -m "feat(zeb-621): sleep/wake resume detector — wall-clock jump fires network_change + immediate republish"`

### Task 7: Full-workspace sweep + doc/spec alignment

**Files:**
- Modify: `docs/specs/2026-07-02-zeb-321-phase3-decision-record.md` (Area C entry: no edits unless a deviation occurred — if any task deviated from the approved decision, document the deviation inline in THIS plan file instead and surface it in the final report)
- Verify only: everything else.

- [ ] **Step 1:** `cd src-tauri && cargo fmt --all` then `git diff --stat` (fmt-only deltas fold into this task's commit).
- [ ] **Step 2:** `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` — clean.
- [ ] **Step 3:** `set -o pipefail; cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -15` — ALL green (expect ~60min under the iroh-endpoint throttle group; this is the ONE full sweep).
- [ ] **Step 4:** From repo root: `npx tsc --noEmit && npx vitest run` — clean (no frontend changes expected; this pins that).
- [ ] **Step 5:** Re-run the Task 5 grep-clean gates; also `rg 'ZEB-321 Phase 3' src-tauri/src/iroh_dial_driver.rs | head -3` (context only — the dial-driver deferrals are ZEB-620/622 territory, must NOT have been touched by this slice).
- [ ] **Step 6: Commit** — `git commit -m "chore(zeb-621): full-sweep gate + fmt"` (or amend-free empty-diff skip if nothing changed).

## Deliberate scope notes (carry into the PR body)

- **Dual-slot resolver instead of a rewritten `should_replace` guard**: the ticket text says "fix the should_replace source guard", but `source` does two jobs (dial addressing + butler-set window exemption); a single-slot freshest-wins rule would let a fresher pkarr record evict a durable one and re-open ZEB-488's seal-target outage for offline recipients. Dual slots deliver the blessed dialer rule ("newest announced_at_ms wins") while keeping the butler view durable-preferred. This is an implementation-shape deviation, not a semantics deviation.
- **Changed-record re-dial hints (Area C item)**: already shipped in ZEB-620 (`NewPeer`/`RecordChanged` kicks, `addressing_differs` gate) — no new work; Task 1 extends the gate to the fused dial view.
- **iroh auto-republish**: iroh 1.0.1 does auto-republish its OWN address-lookup record on addr change (source-verified) — but harmony's identity/community pkarr slots are harmony-owned records, so Task 4's re-register remains necessary. The decision record's "not a documented guarantee" clause stands for semver purposes.
- **Sleep/wake via clock-jump detection** (not OS power hooks): portable, no new deps, testable; `network_change()` is idempotent so false positives (manual clock steps) are harmless.
- **Supervisor notification = `kick_sweep`** (re-arm all non-connected peers): connected peers are left to iroh path migration + the ZEB-622 liveness drop edges; forcibly re-dialing live links on self-rebind would churn.
