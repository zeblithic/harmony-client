# ZEB-831 — Bounded-time policy module + three prior-fix regressions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce the one auditable home for the project's bounded-time trust policy (the `clock_trust` module: the house forward-skew constants + `reject_future`/`clamp_future` helpers), and close the three incomplete-prior-fix regressions the ZEB-831 audit routed into this ticket — ZEB-818 (vine-feed forward gap), ZEB-711 (OpenJoinRateLimiter still wall-clocked), ZEB-621 (network-health reads the unclamped announce).

**Architecture:** A new leaf module `src/clock_trust.rs` owns two forward-skew tiers already present ad-hoc in the tree — `MAX_FORWARD_SKEW_MS` (5 min, control/security) and `DISPLAY_SKEW_TOLERANCE_MS` (30 min, display/discovery) — plus unit-agnostic `reject_future`/`clamp_future` helpers and a compile-visible pin test. The three regression sites then consume it (or, for the two behavior-preserving consolidations, re-derive their existing local constant from it), each with a positive-discrimination test (a poisoned stamp higher than a legit one → visible reject/clamp). ZEB-711 additionally migrates the open-join limiter to a monotonic epoch, mirroring the shipped `IntroRateLimiter` (ZEB-711 phase 1).

**Tech Stack:** Rust (Tauri `src-tauri/` workspace, crate `harmony-app`), `cargo nextest`, `tokio::time::Instant` for the monotonic limiter epoch.

## Global Constraints

- **Spec:** `docs/superpowers/specs/2026-08-01-zeb-831-wall-clock-threat-model.md` (§3 trust model, §5 incomplete prior fixes, §6.5 policy module, §10 testing posture). Every requirement here traces to that spec.
- **Scope boundary:** This PR is ONLY the `clock_trust` module + ZEB-818 + ZEB-711 + ZEB-621. **ZEB-792 (the governance apply-path bound) is OUT** — routed into T-GOV (ZEB-846), which is a superset. The CRITICAL findings (A1/SS/FR/GR/C3/C4/E1/SP) are separate spawned tickets (ZEB-846..854), NOT this PR. Do not add governance/owner-state/mint/card bounds here.
- **House constants (exact values):** `MAX_FORWARD_SKEW_MS = 5 * 60 * 1000` (300_000); `DISPLAY_SKEW_TOLERANCE_MS = 30 * 60 * 1000` (1_800_000); `DISPLAY_SKEW_TOLERANCE_SECS = DISPLAY_SKEW_TOLERANCE_MS / 1000` (1800). These match the existing 5-min control tier (`harmony_pkarr::record::FUTURE_TOLERANCE_MS`, `reachability_resolver::FUTURE_SKEW_TOLERANCE_MS`) and the 30-min display/discovery tier (`VINE_PULL_INVALID_FORWARD_SKEW_SECS`, `INTRODUCTION_MAX_FORWARD_SKEW_MS`). `ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS` is *also* 30 min today, but it is a **governance control** budget (an existing, deferred value T-GOV will migrate onto the 5-min control tier) — not a display-tier policy match.
- **Behavior-preserving consolidations only:** where an existing constant is re-derived from the module, the numeric value MUST stay identical (5 min → 5 min, 30 min → 30 min). No control's window changes in this PR.
- **Every new bound ships a positive-discrimination test** (poisoned-higher-than-legit → visible reject/clamp), mirroring the ZEB-790 T5–T7 pattern.
- **CI gates (run from `src-tauri/`):**
  - `cargo fmt --all -- --check`
  - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
  - Iterative dev: `scripts/test-select --context task` (per-task) is fine; the **final pre-PR sweep must be the full `--workspace --all-targets` commands**.
- **`--all-targets` and `--locked` are load-bearing** (CLAUDE.md): clippy on `--lib` misses inline `#[cfg(test)]` lints; the tests here are all inline module tests.

---

## File Structure

- **Create `src/clock_trust.rs`** — the policy module: 3 constants, 2 helpers, unit tests + the pin test. One responsibility: bounded-time trust policy. (Task 1)
- **Modify `src/lib.rs`** — register `pub mod clock_trust;` alongside the other module declarations (near `pub mod friend_intro;` ~line 196). (Task 1)
- **Modify `src/reachability_resolver.rs`** — re-derive `FUTURE_SKEW_TOLERANCE_MS` from `clock_trust::MAX_FORWARD_SKEW_MS` (consolidation, same value); add `list_active_peers_effective()` accessor carrying the clamped `effective_announced_at_ms`. (Task 2)
- **Modify `src/network_health.rs`** — `ProdReachabilitySnapshot::list_records` reads `list_active_peers_effective()` and sets `last_seen_ms` from the clamped value, not the raw `announced_at_ms`. (Task 2)
- **Modify `src/vine_feed_cache.rs`** — add a forward-skew gate in `on_descriptor_sample` right after the age gate; add a discrimination test; realign the existing tests' synthetic clocks. (Task 3)
- **Modify `src/vine_pull_driver.rs`** — re-derive `VINE_PULL_INVALID_FORWARD_SKEW_SECS` from `clock_trust::DISPLAY_SKEW_TOLERANCE_SECS` (consolidation, same value). (Task 3)
- **Modify `src/open_join_admit.rs`** — `OpenJoinRateLimiter` gains a `tokio::time::Instant` epoch + `new()` + `monotonic_now_ms()`; `verify_and_admit_open_join` splits its `now_ms` into `wall_now_ms` + `limiter_now_ms`; update the 8 existing tests; add the B1 regression test. (Task 4)
- **Modify `src/iroh_invite_acceptor.rs`** — construct the limiter with `::new()`; derive `limiter_now_ms` from `monotonic_now_ms()` at the call site; keep `now_ms` (wall) for the freshness arg. (Task 4)

**Dependencies:** Task 1 is the foundation (Tasks 2 & 3 import `clock_trust`). Task 4 is independent of `clock_trust` (a monotonic-clock migration, no skew constant) but is sequenced last. Implement in order 1 → 2 → 3 → 4.

---

### Task 1: `clock_trust` policy module

**Files:**
- Create: `src-tauri/src/clock_trust.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod clock_trust;` near line 196)
- Test: inline `#[cfg(test)] mod tests` in `src-tauri/src/clock_trust.rs`

**Interfaces:**
- Produces:
  - `pub const MAX_FORWARD_SKEW_MS: u64` (= 300_000)
  - `pub const DISPLAY_SKEW_TOLERANCE_MS: u64` (= 1_800_000)
  - `pub const DISPLAY_SKEW_TOLERANCE_SECS: u64` (= 1_800)
  - `pub fn reject_future(stamp: u64, now: u64, tolerance: u64) -> bool` — true iff `stamp` is more than `tolerance` ahead of `now`
  - `pub fn clamp_future(stamp: u64, now: u64, tolerance: u64) -> u64` — `stamp.min(now + tolerance)`
- Consumes (in the pin test only): `crate::hlc_adopt_floor::HLC_ADOPT_FORWARD_CAP_MS`, `crate::community_membership::ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS`.

- [ ] **Step 1: Create the module file with constants + helpers + tests**

Create `src-tauri/src/clock_trust.rs`:

```rust
//! ZEB-831: the one auditable home for the project's bounded-time trust policy.
//!
//! A wall-clock timestamp is untrusted input the moment it crosses a device
//! boundary (spec
//! `docs/superpowers/specs/2026-08-01-zeb-831-wall-clock-threat-model.md`). Any
//! peer-supplied — or adoption-nudged local — stamp that gates a control or
//! enters a shared LWW / freshest-wins register is accepted only within a
//! bounded forward window of the *receiver's own* clock; beyond it the stamp is
//! rejected or clamped, never silently trusted (spec §3).
//!
//! Two tiers, matching the two forward-skew budgets already present ad-hoc in
//! the tree:
//!
//! * [`MAX_FORWARD_SKEW_MS`] (5 min) — control / security / governance
//!   decisions (expiry, admission, revocation, governance ordering). Matches
//!   `harmony_pkarr::record::FUTURE_TOLERANCE_MS` and
//!   `crate::reachability_resolver::FUTURE_SKEW_TOLERANCE_MS`.
//! * [`DISPLAY_SKEW_TOLERANCE_MS`] (30 min) — pure display / discovery ordering
//!   where no control is gated (vine feed, discovery lists). A future-dated
//!   stamp can only mis-sort a list, not bypass a control. Matches the
//!   governance/discovery 30-min house default
//!   (`crate::community_membership::ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS`,
//!   `crate::vine_pull_driver::VINE_PULL_INVALID_FORWARD_SKEW_SECS`).
//!
//! The helpers are unit-agnostic: [`reject_future`] / [`clamp_future`] operate
//! on raw `u64`, and the caller supplies `stamp`, `now`, and `tolerance` in one
//! shared unit — all milliseconds, or all seconds (see
//! [`DISPLAY_SKEW_TOLERANCE_SECS`] for the seconds-domain tier).

/// House forward-skew ceiling for every control / security / governance
/// decision. A stamp more than this far ahead of the receiver's own clock is
/// rejected or clamped. 5 min matches `harmony_pkarr::record::FUTURE_TOLERANCE_MS`.
pub const MAX_FORWARD_SKEW_MS: u64 = 5 * 60 * 1000;

/// Looser forward-skew tolerance for pure display / discovery ordering, where a
/// future-dated stamp can only mis-sort a list and never bypasses a control.
/// 30 min matches the governance/discovery house default.
pub const DISPLAY_SKEW_TOLERANCE_MS: u64 = 30 * 60 * 1000;

/// [`DISPLAY_SKEW_TOLERANCE_MS`] in whole seconds, for stamps whose native unit
/// is seconds (e.g. a vine descriptor's `created_at`).
pub const DISPLAY_SKEW_TOLERANCE_SECS: u64 = DISPLAY_SKEW_TOLERANCE_MS / 1000;

/// Returns `true` if `stamp` is more than `tolerance` ahead of `now` — i.e.
/// implausibly future-dated and to be rejected. A past/present stamp
/// (`stamp <= now`) is never rejected here; staleness is a separate, opposite
/// bound owned by the caller. `stamp`, `now`, and `tolerance` MUST share one
/// unit (all ms, or all secs).
///
/// The boundary is inclusive: `stamp == now + tolerance` is accepted, matching
/// the existing `<= ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS` convention
/// (`community_membership.rs:5932`).
#[inline]
pub fn reject_future(stamp: u64, now: u64, tolerance: u64) -> bool {
    stamp.saturating_sub(now) > tolerance
}

/// Clamps a future-dated `stamp` down to at most `now + tolerance`; a
/// past/present stamp is returned unchanged. Mirrors the reachability
/// resolver's `announced_at_ms.min(now + skew)` clamp
/// (`reachability_resolver.rs:422`). Same-unit args.
#[inline]
pub fn clamp_future(stamp: u64, now: u64, tolerance: u64) -> u64 {
    stamp.min(now.saturating_add(tolerance))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_future_boundary_is_inclusive() {
        let now = 1_000_000;
        assert!(!reject_future(now, now, MAX_FORWARD_SKEW_MS), "present accepted");
        assert!(!reject_future(now - 999, now, MAX_FORWARD_SKEW_MS), "past accepted");
        assert!(
            !reject_future(now + MAX_FORWARD_SKEW_MS, now, MAX_FORWARD_SKEW_MS),
            "exactly at the ceiling is accepted"
        );
        assert!(
            reject_future(now + MAX_FORWARD_SKEW_MS + 1, now, MAX_FORWARD_SKEW_MS),
            "one past the ceiling is rejected"
        );
    }

    #[test]
    fn clamp_future_caps_only_the_future() {
        let now = 1_000_000;
        assert_eq!(clamp_future(now - 5, now, MAX_FORWARD_SKEW_MS), now - 5, "past unchanged");
        assert_eq!(clamp_future(now, now, MAX_FORWARD_SKEW_MS), now, "present unchanged");
        assert_eq!(
            clamp_future(now + MAX_FORWARD_SKEW_MS + 10_000, now, MAX_FORWARD_SKEW_MS),
            now + MAX_FORWARD_SKEW_MS,
            "future capped to the ceiling"
        );
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn skew_tiers_stay_within_consumer_budgets() {
        // Compile-visible pins (spec §3.3, §10): widening a tier must consciously
        // re-derive these relations. Mirrors
        // `hlc_adopt_floor::adopt_cap_stays_far_below_consumer_budgets`.

        // The control tier is the 5-min pkarr/reachability sibling.
        assert_eq!(MAX_FORWARD_SKEW_MS, 5 * 60 * 1000);
        // The display tier is the 30-min governance/discovery sibling, and is
        // never tighter than the control tier.
        assert_eq!(DISPLAY_SKEW_TOLERANCE_MS, 30 * 60 * 1000);
        assert!(DISPLAY_SKEW_TOLERANCE_MS >= MAX_FORWARD_SKEW_MS);
        // The seconds convenience is exactly the ms tier / 1000.
        assert_eq!(DISPLAY_SKEW_TOLERANCE_SECS * 1000, DISPLAY_SKEW_TOLERANCE_MS);
        // The adoption floor's local nudge (5 s) is far below the control window
        // it must never widen past.
        assert!(crate::hlc_adopt_floor::HLC_ADOPT_FORWARD_CAP_MS < MAX_FORWARD_SKEW_MS);
        // The control tier is at or below governance's current ingest budget
        // (T-GOV may later tighten governance ordering TO this constant).
        assert!(
            MAX_FORWARD_SKEW_MS
                <= crate::community_membership::ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS
        );
    }
}
```

- [ ] **Step 2: Register the module in `lib.rs`**

In `src-tauri/src/lib.rs`, add the declaration next to the other `pub mod` lines (alphabetically it sits just before `pub mod friend_intro;` at ~line 196):

```rust
pub mod clock_trust;
```

- [ ] **Step 3: Run the module tests — expect PASS**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(clock_trust)'`
Expected: 3 tests pass (`reject_future_boundary_is_inclusive`, `clamp_future_caps_only_the_future`, `skew_tiers_stay_within_consumer_budgets`).

If `skew_tiers_stay_within_consumer_budgets` fails to compile on the cross-module constant references, confirm `HLC_ADOPT_FORWARD_CAP_MS` (`hlc_adopt_floor.rs:28`, `pub`) and `ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS` (`community_membership.rs:5513`, `pub`) are the exact paths — both are `pub` today.

- [ ] **Step 4: Gate — fmt + clippy**

Run: `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/clock_trust.rs src-tauri/src/lib.rs
git commit -m "ZEB-831: clock_trust policy module (bounded-time trust: MAX_FORWARD_SKEW_MS + DISPLAY tier + reject_future/clamp_future)"
```

---

### Task 2: ZEB-621 — network-health reads the clamped announce

**Problem (spec §5, finding D1):** `reachability_resolver` already computes and stores the future-skew-clamped `effective_announced_at_ms` on each `ResolverEntry` (`reachability_resolver.rs:422`, 5-min clamp). But `list_active_peers()` (`:501`) returns only `(OwnerAddr, ReachabilityAnnouncePayload)` — the raw payload — so `ProdReachabilitySnapshot::list_records` (`network_health.rs:3604`) reports `last_seen_ms: Some(payload.announced_at_ms)`, the *unclamped* value. A future-dated record then shows "seen 0 s ago." Fix = a sibling accessor that carries the clamped field, consumed by network-health. Also re-derive the resolver's 5-min constant from `clock_trust` (consolidation, same value).

**Files:**
- Modify: `src-tauri/src/reachability_resolver.rs` (`:46` constant; add accessor after `list_active_peers` at `:508`)
- Modify: `src-tauri/src/network_health.rs` (`ProdReachabilitySnapshot::list_records`, `:3589-3615`)
- Test: inline tests in both files

**Interfaces:**
- Consumes: `crate::clock_trust::MAX_FORWARD_SKEW_MS` (Task 1); existing `ResolverEntry::effective_announced_at_ms` (`:131`), `ResolverSlots::durable_preferred`.
- Produces: `ReachabilityResolver::list_active_peers_effective(&self) -> Vec<(OwnerAddr, ReachabilityAnnouncePayload, u64)>` (the `u64` is `effective_announced_at_ms`).

- [ ] **Step 1: Write the failing resolver-accessor test**

In `reachability_resolver.rs`'s `#[cfg(test)] mod tests`, add (mirror the injected-clock idiom of `future_record_clamped_and_healed_by_refresh` at `:2098`, and the `make_payload` helper at `:939`):

```rust
#[test]
fn list_active_peers_effective_reports_the_future_skew_clamp() {
    const T: u64 = 1_000_000_000_000; // fixed "now" (ms)
    let r = ReachabilityResolver::new();
    r.set_clock(std::sync::Arc::new(|| T));

    // A future-dated durable record: announced_at 1 h ahead of now.
    let owner = OwnerAddr([7u8; 32]);
    let payload = make_payload(1, T + 3_600_000);
    r.update(owner, payload, Hlc { wall_ms: T, logical: 0, device_id: String::new() });

    let got = r.list_active_peers_effective();
    assert_eq!(got.len(), 1);
    // Raw announce is still the future value on the payload...
    assert_eq!(got[0].1.announced_at_ms, T + 3_600_000);
    // ...but the effective (clamped) value is capped at now + 5 min.
    assert_eq!(got[0].2, T + crate::clock_trust::MAX_FORWARD_SKEW_MS);
}
```

> Read `make_payload` (`:939`) and the `Hlc`/`OwnerAddr` imports already in scope in the test module; match the exact field/import names that file uses (e.g. `Hlc { wall_ms, logical, device_id }`). If `update`'s signature differs, use the same call shape as the sibling tests at `:1044`/`:1081`.

- [ ] **Step 2: Run it — expect FAIL (method missing)**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(list_active_peers_effective_reports_the_future_skew_clamp)'`
Expected: FAIL to compile — `no method named list_active_peers_effective`.

- [ ] **Step 3: Add the accessor + consolidate the constant**

In `reachability_resolver.rs`, change the constant at `:46` (value identical, now sourced from the house module):

```rust
pub(crate) const FUTURE_SKEW_TOLERANCE_MS: u64 = crate::clock_trust::MAX_FORWARD_SKEW_MS;
```

Add the accessor immediately after `list_active_peers` (after `:508`):

```rust
/// ZEB-621 / D1: like [`list_active_peers`](Self::list_active_peers), but
/// pairs each peer with its future-skew-clamped `effective_announced_at_ms`
/// (`announced_at_ms` capped at `now + FUTURE_SKEW_TOLERANCE_MS`). Diagnostics
/// (network-health "last seen") read THIS so a future-dated record cannot
/// report a peer as "seen 0 s ago". Backed by `durable_preferred`, matching
/// `list_active_peers`.
pub fn list_active_peers_effective(
    &self,
) -> Vec<(OwnerAddr, ReachabilityAnnouncePayload, u64)> {
    let map = self.inner.read().expect("resolver read lock");
    map.iter()
        .filter_map(|((owner, _node_id), v)| {
            v.durable_preferred()
                .map(|e| (*owner, e.payload.clone(), e.effective_announced_at_ms))
        })
        .collect()
}
```

- [ ] **Step 4: Run the resolver test — expect PASS**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(list_active_peers_effective_reports_the_future_skew_clamp)'`
Expected: PASS.

- [ ] **Step 5: Write the failing network-health wiring test**

In `network_health.rs`'s `#[cfg(test)] mod tests`, add a test that a future-dated durable record surfaces a *clamped* `last_seen_ms` through `ProdReachabilitySnapshot`:

```rust
#[test]
fn prod_reachability_snapshot_last_seen_is_future_skew_clamped() {
    use crate::owner_state_types::{Hlc, OwnerAddr};
    use crate::reachability_resolver::{ReachabilityResolver, FUTURE_SKEW_TOLERANCE_MS};
    // Payload type: import via the same path `reachability_resolver` uses (its
    // `use` line resolves `ReachabilityAnnouncePayload` from the
    // `harmony-reachability` crate; match that exact path here).
    use harmony_reachability::record::ReachabilityAnnouncePayload;

    const T: u64 = 1_000_000_000_000;
    let resolver = ReachabilityResolver::new();
    resolver.set_clock(std::sync::Arc::new(|| T)); // #[cfg(test)] pub(crate), same crate

    // A future-dated durable record: announced 1 h ahead of now. Fields mirror
    // `make_payload` (reachability_resolver.rs:939).
    let payload = ReachabilityAnnouncePayload {
        iroh_node_id: [2u8; 32],
        home_relay_url: "https://derp.example/".into(),
        direct_addresses: vec![],
        announced_at_ms: T + 3_600_000,
        identity_signature: [0; 64],
        butler_set: Vec::new(),
        bs_at: 0,
    };
    resolver.update(
        OwnerAddr([9u8; 32]),
        payload,
        Hlc { wall_ms: T, logical: 0, device_id: String::new() },
    );

    let records = ProdReachabilitySnapshot(resolver).list_records();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].last_seen_ms,
        Some(T + FUTURE_SKEW_TOLERANCE_MS),
        "network-health last-seen must read the clamped effective announce, not the raw future value"
    );
}
```

> `FUTURE_SKEW_TOLERANCE_MS` is `pub(crate)` (same crate — importable). If `ReachabilityAnnouncePayload`'s field set differs from the above at implementation time, copy it verbatim from `make_payload` (`reachability_resolver.rs:939`) — it is the authority. Do NOT reach into `reachability_resolver`'s private `#[cfg(test)] mod tests` for helpers; build the payload inline as shown.

- [ ] **Step 6: Run it — expect FAIL (reads raw future value)**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(prod_reachability_snapshot_last_seen_is_future_skew_clamped)'`
Expected: FAIL — `last_seen_ms` is `Some(T + 3_600_000)` (raw), not the clamp.

- [ ] **Step 7: Wire network-health to the clamped accessor**

In `network_health.rs`, `ProdReachabilitySnapshot::list_records` (`:3589`), change the source iterator and the `last_seen_ms` line:

```rust
    fn list_records(&self) -> Vec<ResolverPeerRecord> {
        self.0
            .list_active_peers_effective()
            .into_iter()
            .map(|(owner, payload, effective_announced_at_ms)| ResolverPeerRecord {
                owner_addr: owner.0,
                iroh_node_id: payload.iroh_node_id,
                display_name: None,
                connection_mode: ConnectionMode::NoConnection,
                rtt_ms: None,
                // ZEB-621 / D1: the future-skew-clamped announce, never the raw
                // `payload.announced_at_ms`, so a future-dated record cannot
                // report "seen 0 s ago".
                last_seen_ms: Some(effective_announced_at_ms),
                protocol_incompat_reason: None,
                last_traffic_ms: None,
                last_relay_pull_served_ms: None,
                connected_since_ms: None,
            })
            .collect()
    }
```

(Keep every other field exactly as it was — only the iterator source, the closure binding, and the `last_seen_ms` line change.)

- [ ] **Step 8: Run both tests — expect PASS**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(prod_reachability_snapshot_last_seen_is_future_skew_clamped) + test(list_active_peers_effective_reports_the_future_skew_clamp)'`
Expected: both PASS.

- [ ] **Step 9: Scoped regression run + gates**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(reachability) + test(network_health)'`
Then: `cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: all pass, clippy clean. (Confirms the `FUTURE_SKEW_TOLERANCE_MS` re-derivation didn't disturb existing resolver clamp tests — value is unchanged.)

- [ ] **Step 10: Commit**

```bash
git add src-tauri/src/reachability_resolver.rs src-tauri/src/network_health.rs
git commit -m "ZEB-831/ZEB-621: network-health reads the future-skew-clamped announce (D1); source reachability skew tolerance from clock_trust"
```

---

### Task 3: ZEB-818 — vine-feed forward-skew gate

**Problem (spec §5, finding VF):** `on_descriptor_sample` (`vine_feed_cache.rs:621`) has a *backward* age gate (`:701`, rejects too-old `created_at`) but **no forward bound**. A future-dated `created_at` (1) survives ingest, (2) is immune to the capacity-trim (`:735` drops the *oldest* `created_at` — a future stamp is never oldest, so honest entries are evicted instead), and (3) pins the top of the feed forever (`list_descriptors` sorts `created_at` DESC, `:787`). Fix = a forward gate right after the age gate, seconds-domain, at the display tier (matching the sibling pull-cursor bound `VINE_PULL_INVALID_FORWARD_SKEW_SECS`). Also re-derive that pull-cursor constant from `clock_trust` (consolidation, same value), tying both vine bounds to one source.

**Files:**
- Modify: `src-tauri/src/vine_feed_cache.rs` (gate after `:706`; new test; realign existing test clocks)
- Modify: `src-tauri/src/vine_pull_driver.rs` (`:78` constant)
- Test: inline tests in `vine_feed_cache.rs`

**Interfaces:**
- Consumes: `crate::clock_trust::{reject_future, DISPLAY_SKEW_TOLERANCE_SECS}` (Task 1). `descriptor.created_at` and `now_secs` are both **seconds** (`now_secs = now_ms / 1000`, `:700`).

- [ ] **Step 1: Consolidate the pull-cursor constant**

In `vine_pull_driver.rs`, change `:78` (value identical — `30 * 60` → `DISPLAY_SKEW_TOLERANCE_SECS` = 1800):

```rust
pub const VINE_PULL_INVALID_FORWARD_SKEW_SECS: u64 = crate::clock_trust::DISPLAY_SKEW_TOLERANCE_SECS;
```

Update the doc comment's last sentence (`:76-77`) to read: `// 30 min = the display-tier house default (`clock_trust::DISPLAY_SKEW_TOLERANCE_SECS`).`

- [ ] **Step 2: Write the failing discrimination test**

In `vine_feed_cache.rs`'s `#[cfg(test)] mod tests`, add (mirror `on_descriptor_sample_followed_creator_inserts_with_followed_source` at `:2018` for the `canonical_descriptor_bytes` / `topic` / `followed_set_with` fixture idiom; the 7th positional arg to `canonical_descriptor_bytes` is `created_at` in **seconds**):

```rust
#[test]
fn future_dated_descriptor_beyond_display_skew_is_rejected() {
    // ZEB-831 / VF (ZEB-818): a descriptor whose created_at is further ahead
    // than the display-tier tolerance must be rejected at ingest — otherwise it
    // pins the top of the feed forever and is immune to capacity-trim.
    let mut cache = VineFeedCache::new();
    let now_secs: u64 = 1_700_000_000;
    let now_ms = now_secs * 1000;
    let followed = followed_set_with(&["alice-addr"]);

    // A legit, recent descriptor inserts.
    let legit = canonical_descriptor_bytes(
        "vine-legit", "alice-addr", "Alice", "cid-legit", None, None,
        now_secs, None, None,
    );
    assert!(matches!(
        cache.on_descriptor_sample(&topic("alice-addr"), &legit, &followed, now_ms),
        Some(DescriptorOutcome::Inserted { .. })
    ));

    // A poisoned descriptor dated one second past the display tolerance is rejected.
    let poisoned = canonical_descriptor_bytes(
        "vine-poison", "alice-addr", "Alice", "cid-poison", None, None,
        now_secs + crate::clock_trust::DISPLAY_SKEW_TOLERANCE_SECS + 1, None, None,
    );
    match cache.on_descriptor_sample(&topic("alice-addr"), &poisoned, &followed, now_ms) {
        Some(DescriptorOutcome::Rejected(_)) => {}
        other => panic!("expected Rejected for future-dated descriptor, got {other:?}"),
    }

    // The poisoned descriptor never entered the cache; the legit one still leads.
    let listed = cache.list_descriptors();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "vine-legit");

    // Boundary: exactly at the tolerance ceiling is accepted.
    let edge = canonical_descriptor_bytes(
        "vine-edge", "alice-addr", "Alice", "cid-edge", None, None,
        now_secs + crate::clock_trust::DISPLAY_SKEW_TOLERANCE_SECS, None, None,
    );
    assert!(matches!(
        cache.on_descriptor_sample(&topic("alice-addr"), &edge, &followed, now_ms),
        Some(DescriptorOutcome::Inserted { .. })
    ));
}
```

- [ ] **Step 3: Run it — expect FAIL (poisoned descriptor currently inserts)**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(future_dated_descriptor_beyond_display_skew_is_rejected)'`
Expected: FAIL — the poisoned descriptor returns `Inserted`, so `list_descriptors().len()` is 2 and/or `listed[0].id` is `"vine-poison"`.

- [ ] **Step 4: Add the forward gate**

In `on_descriptor_sample`, immediately AFTER the age-gate block that ends at `:706` (the `}` closing `if descriptor.created_at < now_secs.saturating_sub(MAX_AGE_SECS) { ... }`), insert:

```rust
        // ZEB-831 / VF (ZEB-818): forward-skew gate. The age gate above is a
        // BACKWARD bound only; without a forward bound a future-dated
        // `created_at` survives ingest, is immune to the capacity-trim below
        // (which drops the *oldest* `created_at`, so honest entries are evicted
        // instead), and pins the top of the feed forever (`list_descriptors`
        // sorts `created_at` DESC). Reject anything dated further ahead than the
        // display-tier tolerance, mirroring the sibling pull-cursor bound
        // (`vine_pull_driver::VINE_PULL_INVALID_FORWARD_SKEW_SECS`). Seconds domain.
        if crate::clock_trust::reject_future(
            descriptor.created_at,
            now_secs,
            crate::clock_trust::DISPLAY_SKEW_TOLERANCE_SECS,
        ) {
            return Some(DescriptorOutcome::Rejected(format!(
                "descriptor {} is dated further ahead than the plausible clock window",
                descriptor.id
            )));
        }
```

(`descriptor` and `now_secs` are both in scope here; `descriptor` is not moved until `:716`.)

- [ ] **Step 5: Run the new test — expect PASS**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(future_dated_descriptor_beyond_display_skew_is_rejected)'`
Expected: PASS.

- [ ] **Step 6: Realign the existing vine tests' synthetic clocks (TDD discovery loop)**

The new gate requires `descriptor.created_at <= now_secs + 1800`. Several existing tests pass a **tiny synthetic `now_ms`** (`0`, `1_000`, `5_000`, …) while their descriptor's `created_at` is a **real-epoch value** (`1_700_000_000` literal, or `SystemTime::now().as_secs() - N`). Those calls now return `Rejected`. This is expected and must be fixed by realigning the caller's clock — NOT by weakening the gate.

Run the full vine suite to enumerate the exact failing set:

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(vine_feed_cache)'`

For **each** failure whose cause is the new "further ahead than the plausible clock window" rejection, apply this deterministic rule:

- **Pass a `now_ms` consistent with the descriptor's `created_at`.** If the test already computes a realistic `now_secs` (via `SystemTime::now()` or a fixed real-epoch value), pass `now_secs * 1000` as the `on_descriptor_sample` `now_ms` argument (replacing the `1_000`/`0`). If the test uses a literal `created_at` like `1_700_000_000`, pass `1_700_000_000_000` (that value × 1000).
- **Preserve relative ordering** where a test uses several increasing `now_ms` values across calls with the same/related descriptors (e.g. the idempotency test at `:2082/:2087/:2098` uses `3_000/4_000/5_000`): keep the deltas by rebasing onto the realistic base (`base + 0/1_000/2_000`), not by collapsing them to one value.
- **Do NOT change `created_at` literals** — several tests encode ordering/tuple semantics in `created_at` (e.g. `:1853` `[("b",10),("a",10),("c",11)]`, reshare/degree tests). Tests whose `created_at` is already tiny (≤ `now_secs + 1800`) are unaffected — leave them alone.
- **Do NOT touch assertions** unless a realigned `now_ms` changes an asserted `received_at_ms`; if it does, update that expected value to the new `now_ms` (received_at is set to `now_ms` at `:720`).

Known-affected sites to check first (from the audit grep; the suite run is authoritative): the `1_700_000_000`/`1700000000`/`1700000100` literal payloads (`:1675/:1708/:1736/:2027/:2055`) and the `SystemTime::now()`/`now_secs`-based tests (`:1757/:1806/:2566/:2661/:2735/:2763/:2819/:2862/:2960/:3171/:3242/:3354/:3756`). Re-run after each batch until the whole vine suite is green.

Run (repeat until green): `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(vine_feed_cache)'`
Expected: all vine tests pass, including `future_dated_descriptor_beyond_display_skew_is_rejected`.

- [ ] **Step 7: Gate — fmt + clippy**

Run: `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/vine_feed_cache.rs src-tauri/src/vine_pull_driver.rs
git commit -m "ZEB-831/ZEB-818: vine-feed forward-skew gate (VF); source vine pull-cursor skew from clock_trust; realign synthetic test clocks"
```

---

### Task 4: ZEB-711 — OpenJoinRateLimiter → monotonic epoch

**Problem (spec §5/§6.4, finding B1):** `verify_and_admit_open_join` (`open_join_admit.rs:146`) takes a single `now_ms` — the *beacon wall clock* — and uses it for BOTH the freshness bound against the joiner's `created_at.wall_ms` (step 2, `:165`) AND the `OpenJoinRateLimiter` window/nonce (step 7, `:214-220`). A wall-clock step (forward: a flood gets a fresh budget; backward: the window/nonce horizon is corrupted) distorts rate-limit enforcement. Fix = give the limiter its own monotonic `tokio::time::Instant` epoch (mirroring the shipped `IntroRateLimiter`, `friend_intro.rs:737-800`) and split the function's `now_ms` into `wall_now_ms` (freshness) + `limiter_now_ms` (window/nonce). No `clock_trust` dependency — the freshness arm keeps its existing 60 s bound (pinned by `hlc_adopt_floor.rs:157`).

**Files:**
- Modify: `src-tauri/src/open_join_admit.rs` (limiter struct/methods; function signature; 8 tests; new regression test)
- Modify: `src-tauri/src/iroh_invite_acceptor.rs` (`:267` ctor; `:605-618` call site)
- Test: inline tests in `open_join_admit.rs`

**Interfaces:**
- Produces:
  - `OpenJoinRateLimiter::new() -> Self` (replaces `#[derive(Default)]`)
  - `OpenJoinRateLimiter::monotonic_now_ms(&self) -> u64`
  - `verify_and_admit_open_join(..., wall_now_ms: u64, freshness_window_ms: u64, limiter_now_ms: u64, limiter: &mut OpenJoinRateLimiter)` — the single `now_ms` becomes `wall_now_ms`; a new `limiter_now_ms` is inserted immediately before `limiter`.
- The limiter's `allow`/`is_replay`/`record_nonce` internal `now_ms` params are renamed `limiter_now_ms` (semantic only).

- [ ] **Step 1: Write the failing B1 regression test**

Uses the existing `Fixture` (`open_join_admit.rs:280`): `Fixture::new()`, `f.now_ms` (= 5_000, in-freshness of the request's hardcoded `created_at.wall_ms` = 1000), `f.fresh_request()` (no args → `(OpenJoinRequest, [u8;64], Vec<u8>)` with a unique nonce), and the consts `FRESHNESS` (`:274`), `OPEN_JOIN_RATE_LIMIT_PER_WINDOW`, `OPEN_JOIN_RATE_LIMIT_WINDOW_MS`.

> Constraint that shapes this test: the request's `created_at.wall_ms` is fixed at 1000 and `FRESHNESS` is 60_000, so `wall_now_ms` MUST stay within ~60 s of 1000 (a large backward wall jump trips the `Stale` gate before the limiter). The rate window is also 60_000. So B1 is proven by holding `wall_now_ms` FIXED and advancing only `limiter_now_ms` past the window: a monotonic-keyed window rolls; a wall-keyed one would not (wall never moved).

In `open_join_admit.rs`'s `#[cfg(test)] mod tests`, add (mirror `rate_limit_sheds_excess` at `:637` for the loop/call shape — but written against the POST-split signature this plan introduces in Step 3):

```rust
#[test]
fn limiter_window_keys_on_monotonic_clock_not_wall() {
    // ZEB-711 / B1: the rate-limit window rolls on the limiter's OWN monotonic
    // clock (`limiter_now_ms`), never the beacon wall clock (`wall_now_ms`).
    // Hold wall FIXED (freshness constant) and advance only the limiter clock:
    // the window must roll on the limiter clock alone. If the limiter (wrongly)
    // keyed on wall, the post-roll request would still be shed (wall never moved).
    let f = Fixture::new();
    let mut lim = OpenJoinRateLimiter::new();
    let wall = f.now_ms; // fixed, in-freshness of created_at (= 1000)

    // Fill the window at limiter t = 0.
    for _ in 0..OPEN_JOIN_RATE_LIMIT_PER_WINDOW {
        let (req, sig, sb) = f.fresh_request();
        verify_and_admit_open_join(
            &req, &sig, &sb, &f.epoch_key, f.community_id, f.admin_addr,
            &f.current_events, wall, FRESHNESS, 0, &mut lim,
        )
        .expect("in-window requests admit");
    }

    // Same limiter time, wall unchanged → window is full → shed.
    let (shed_req, shed_sig, shed_sb) = f.fresh_request();
    assert_eq!(
        verify_and_admit_open_join(
            &shed_req, &shed_sig, &shed_sb, &f.epoch_key, f.community_id,
            f.admin_addr, &f.current_events, wall, FRESHNESS, 0, &mut lim,
        )
        .unwrap_err(),
        OpenJoinReject::RateLimited,
        "window is full at the same limiter time"
    );

    // Advance ONLY the limiter clock past the window; wall stays fixed. The
    // window rolls on the monotonic limiter clock → admits. A wall-keyed window
    // would still be full here (wall never moved), so this is the discriminator.
    let (next_req, next_sig, next_sb) = f.fresh_request();
    verify_and_admit_open_join(
        &next_req, &next_sig, &next_sb, &f.epoch_key, f.community_id,
        f.admin_addr, &f.current_events, wall, FRESHNESS,
        OPEN_JOIN_RATE_LIMIT_WINDOW_MS + 1, &mut lim,
    )
    .expect("window rolled on the monotonic limiter clock, so this admits");
}
```

- [ ] **Step 2: Run it — expect FAIL (signature mismatch / compile error)**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(limiter_window_keys_on_monotonic_clock_not_wall)'`
Expected: FAIL to compile — `OpenJoinRateLimiter::new` missing and/or arity mismatch on `verify_and_admit_open_join` (the `limiter_now_ms` arg does not exist yet).

- [ ] **Step 3: Migrate the limiter to a monotonic epoch + split the function clock**

In `open_join_admit.rs`:

**(a)** Replace the limiter struct's `#[derive(Default)]` (`:81`) and add the epoch + constructor + accessor (mirror `friend_intro.rs:737-800`):

```rust
/// Per-window admission limiter + nonce-replay cache. The window is coarse
/// (a count per rolling window); the nonce cache rejects exact-replay within
/// the retention horizon and is bounded by eviction.
pub struct OpenJoinRateLimiter {
    window_start_ms: u64,
    count_in_window: usize,
    seen_nonces: HashSet<[u8; 16]>,
    nonce_seen_at: HashMap<[u8; 16], u64>,
    /// ZEB-711: monotonic epoch for the production limiter timeline. `allow` /
    /// `is_replay` / `record_nonce` keep taking an explicit `limiter_now_ms`
    /// (the unit-test seam), but the acceptor derives it from
    /// [`Self::monotonic_now_ms`] instead of the beacon wall clock — a wall
    /// step would otherwise distort enforcement (forward: a flood gets a fresh
    /// budget; backward: an honest shed peer stays shed longer, and the nonce
    /// horizon is corrupted). `tokio::time::Instant` also honors the paused test
    /// clock. Wall time stays for the freshness arm only (`created_at.wall_ms`).
    epoch: tokio::time::Instant,
}

impl OpenJoinRateLimiter {
    /// Fresh limiter with its monotonic epoch anchored now.
    pub fn new() -> Self {
        Self {
            window_start_ms: 0,
            count_in_window: 0,
            seen_nonces: HashSet::new(),
            nonce_seen_at: HashMap::new(),
            epoch: tokio::time::Instant::now(),
        }
    }

    /// ZEB-711: the production timeline for admits on this limiter —
    /// milliseconds since it was constructed, from the monotonic (and
    /// test-pausable) tokio clock. Window state and epoch live and die with the
    /// limiter instance, so the timeline is internally consistent by construction.
    pub fn monotonic_now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }
```

Rename the internal `now_ms` params of `allow`, `is_replay`, `record_nonce` to `limiter_now_ms` (bodies unchanged otherwise — pure rename for clarity).

**(b)** In `verify_and_admit_open_join` (`:146`), rename `now_ms: u64` → `wall_now_ms: u64` and insert `limiter_now_ms: u64` immediately before `limiter: &mut OpenJoinRateLimiter`:

```rust
    current_events: &[SignedMembershipEvent],
    wall_now_ms: u64,
    freshness_window_ms: u64,
    limiter_now_ms: u64,
    limiter: &mut OpenJoinRateLimiter,
) -> Result<OpenJoinAdmitOk, OpenJoinReject> {
```

Then in the body: step 2 (freshness, `:164-169`) uses `wall_now_ms` (replace both `now_ms` occurrences); step 7 (`:214-220`) uses `limiter_now_ms` for `is_replay` / `allow` / `record_nonce`. Update the doc comment at `:141` to describe the two clocks.

- [ ] **Step 4: Update the 8 existing tests + acceptor construction**

In `open_join_admit.rs` tests: replace each `OpenJoinRateLimiter::default()` (`:481/:507/:534/:557/:580/:604/:639/:666`) with `OpenJoinRateLimiter::new()`. At **every** `verify_and_admit_open_join(...)` call in the test module (`:482/:509/:536/:559/:583/:605/:619/:643/:670/:687/:705`) except the new B1 test, insert `limiter_now_ms` immediately before `&mut lim`, passing **the same value/expression that call already passes as the wall arg** — the wall and limiter clocks coincide in these tests, so behavior is preserved exactly. Concretely: where the call passes `f.now_ms`, pass `f.now_ms` again; where it passes a rolled value like `later = f.now_ms + OPEN_JOIN_RATE_LIMIT_WINDOW_MS + 1` (the second call in `rate_limited_request_nonce_is_retryable_after_window`, `:705`), pass that same `later` expression again. Do NOT change any asserted outcome — same-value split is behavior-identical to today.

In `iroh_invite_acceptor.rs`:
- `:267` ctor: `OpenJoinRateLimiter::default()` → `OpenJoinRateLimiter::new()`.
- Call site (`:605-619`): derive the limiter clock from the limiter, keep `now_ms` (wall) for freshness:

```rust
        let admit = {
            let mut limiter = self.open_join_limiter.lock().await;
            let limiter_now_ms = limiter.monotonic_now_ms();
            crate::open_join_admit::verify_and_admit_open_join(
                &req,
                &signature,
                &signed_bytes,
                &epoch_key,
                community_id,
                admin_addr,
                &current_events,
                now_ms,
                OPEN_JOIN_FRESHNESS_WINDOW_MS,
                limiter_now_ms,
                &mut limiter,
            )
        };
```

(`monotonic_now_ms()` borrows `&limiter`; bind `limiter_now_ms` first, then pass `&mut limiter` — the immutable borrow has already ended.)

- [ ] **Step 5: Run the open-join + acceptor tests — expect PASS**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(open_join) + test(iroh_invite_acceptor)'`
Expected: all pass, including `limiter_window_keys_on_monotonic_clock_not_wall`.

> Note: `OpenJoinRateLimiter::new()` calls `tokio::time::Instant::now()`, which is safe in plain `#[test]` functions without a reactor (it falls back to the std monotonic clock when unpaused — the shipped `IntroRateLimiter::new()` does the same in plain `#[test]`s, e.g. `friend_intro.rs:1677`). No test needs converting to `#[tokio::test]`.

- [ ] **Step 6: Gate — fmt + clippy**

Run: `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: clean. (Watch for a `clippy::new_without_default` lint on `OpenJoinRateLimiter::new` — if it fires, `tokio::time::Instant` has no `Default`, so the correct fix is `#[allow(clippy::new_without_default)]` on the impl with a one-line reason, NOT re-adding `Default`.)

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/open_join_admit.rs src-tauri/src/iroh_invite_acceptor.rs
git commit -m "ZEB-831/ZEB-711: OpenJoinRateLimiter monotonic epoch; split open-join wall vs limiter clock (B1)"
```

---

## Final verification (before opening the PR)

- [ ] **Full CI-parity sweep** (not `test-select`):
  - `cd src-tauri && cargo fmt --all -- --check`
  - `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
  - Expected: all green. Budget ~12 min warm; a cold sccache miss can run longer (slow, not hung — see the long-running-supervision discipline).
- [ ] **Frontend gates unaffected** (no TS touched) — but run once to be safe: from repo root `npx tsc --noEmit` (vitest not needed; no frontend change).
- [ ] Confirm the diff's **implementation** changes touch ONLY the files in the File Structure section (the spec/plan docs and their required test-clock realignments in already-existing vine test files are expected too) and add ONLY forward/monotonic bounds — no governance/owner-state/mint/card change leaked in (those are separate tickets).

## Self-review notes (author)

- **Spec coverage:** clock_trust module → Task 1 (spec §6.5, §10 pin test). ZEB-818/VF → Task 3 (§5, §6.2). ZEB-711/B1 → Task 4 (§5, §6.4). ZEB-621/D1 → Task 2 (§5). ZEB-792 explicitly excluded (routed to T-GOV/ZEB-846) per the scope decision — Global Constraints call this out.
- **Type consistency:** `list_active_peers_effective` returns `(OwnerAddr, ReachabilityAnnouncePayload, u64)` and network-health destructures the same triple. `verify_and_admit_open_join` gains `limiter_now_ms` before `limiter` consistently in the signature (Step 3), the acceptor call (Step 4), the 11 test calls (Step 4), and the new regression test (Step 1).
- **No placeholders:** every code step carries exact code. The one discovery loop (Task 3 Step 6) is a deterministic realignment RULE over a test set the suite run enumerates — the correct TDD shape for a new gate that trips synthetic-clock tests (memory: `wall_clock_gate_retroactively_breaks_realclock_tests`), not a vague "fix appropriately."
- **Values pinned:** both consolidations (reachability 5 min, vine-pull 30 min) keep identical numeric values; the pin test (Task 1) guards the tier relationships.
