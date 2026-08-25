//! ZEB-790: bounded causal adoption floor for HLC minting.
//!
//! A session-only high-water of verified remote `wall_ms` values. Feeding
//! happens ONLY after an accept path's commit/record succeeded (the same
//! censorship-defence discipline as the replay trackers — a rejected frame
//! must never move this). Reading happens inside the mint seams:
//! `effective_wall = max(now, min(floor, now + HLC_ADOPT_FORWARD_CAP_MS))`.
//!
//! The stored value is `max observed remote wall + 1`: we adopt only the
//! wall (not `logical`), and a remote stamp `(W, l>0)` would out-sort a
//! naive adoption minted at `(W, 0)` — storing `W+1` makes the adopted
//! mint strictly exceed the observed stamp on the FIRST tuple component,
//! so `logical` and `device_id` never matter. Cost: ≤1ms inflation per
//! causal hop, all inside the cap.
//!
//! Not persisted: re-learned from live traffic within seconds, and the
//! clamp is applied against current `now` at every read anyway.
//! See docs/superpowers/specs/2026-07-31-zeb-790-hlc-bounded-adoption-design.md §3.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// How far ahead of this device's own wall clock the mint may be pulled by
/// adopting a verified remote stamp. 5s = 5x the observed ZEB-788 failure
/// class (~1s skew), 12x under the tightest wall-time-coupled consumer
/// budget (the 60s invite/open-join forward windows). Task 8's
/// budget-relation test pins those margins.
pub const HLC_ADOPT_FORWARD_CAP_MS: u64 = 5_000;

/// 0 = nothing observed yet (wall_ms 0 is the epoch; no real stamp is 0,
/// and `merged_now` degenerates to the identity on 0 regardless).
#[derive(Clone, Debug, Default)]
pub struct HlcAdoptFloor(Arc<AtomicU64>);

impl HlcAdoptFloor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed: record a VERIFIED remote stamp's wall. Callers must sit
    /// strictly after the accept path's commit/record success.
    ///
    /// Ordering: this is a standalone monotonic max-register guarding no
    /// other memory, so per-location *coherence* alone already forbids a
    /// reader from ever seeing the floor regress — `Relaxed` would suffice
    /// for correctness. We use `AcqRel`/`Acquire` (here and in `merged_now`)
    /// to make the release→acquire synchronization explicit: a mint that
    /// reads a value written by `observe` has a happens-before edge to it.
    /// What no ordering can provide — and what this floor deliberately does
    /// NOT claim — is *real-time* cross-task visibility: a mint on another
    /// task that races an in-flight `observe` may read the pre-`observe`
    /// value. That is by design; the floor is a best-effort session hint
    /// (see the visibility note in the module docs and spec §4), and the
    /// clamp against current `now` keeps even a stale read bounded.
    pub fn observe(&self, remote_wall_ms: u64) {
        self.0
            .fetch_max(remote_wall_ms.saturating_add(1), Ordering::AcqRel);
    }

    /// Read: the wall the mint should use instead of `wall_now_ms`.
    /// max(now, min(floor, now + CAP)) — see the case table in the spec §3.
    pub fn merged_now(&self, wall_now_ms: u64) -> u64 {
        let floor = self.0.load(Ordering::Acquire);
        wall_now_ms.max(floor.min(wall_now_ms.saturating_add(HLC_ADOPT_FORWARD_CAP_MS)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_floor_is_identity() {
        let f = HlcAdoptFloor::new();
        assert_eq!(f.merged_now(1_000_000), 1_000_000);
        assert_eq!(f.merged_now(0), 0);
    }

    #[test]
    fn remote_behind_is_identity() {
        let f = HlcAdoptFloor::new();
        f.observe(999);
        assert_eq!(f.merged_now(5_000), 5_000, "floor 1000 <= now: identity");
    }

    #[test]
    fn adopts_within_cap_strictly_past_observed_wall() {
        let f = HlcAdoptFloor::new();
        let now = 1_000_000u64;
        f.observe(now + 600); // the ZEB-788 class: remote 600ms ahead
        assert_eq!(f.merged_now(now), now + 601, "floor = W+1, adopted");
    }

    #[test]
    fn clamps_beyond_cap() {
        let f = HlcAdoptFloor::new();
        let now = 1_000_000u64;
        f.observe(now + HLC_ADOPT_FORWARD_CAP_MS + 60_000); // hostile far-future
        assert_eq!(
            f.merged_now(now),
            now + HLC_ADOPT_FORWARD_CAP_MS,
            "damage bounded at CAP"
        );
    }

    #[test]
    fn boundary_w_equals_now_plus_cap_clamps_to_w() {
        // The contract is strict (W < now+CAP): at exactly now+CAP the +1
        // floor clamps TO W, not past it. Spec §2.
        let f = HlcAdoptFloor::new();
        let now = 1_000_000u64;
        let w = now + HLC_ADOPT_FORWARD_CAP_MS;
        f.observe(w);
        assert_eq!(f.merged_now(now), w, "not w+1: clamped");
    }

    #[test]
    fn observe_is_monotone_max() {
        let f = HlcAdoptFloor::new();
        f.observe(500);
        f.observe(300); // lower: no regression
        assert_eq!(f.merged_now(0), 501);
    }

    #[test]
    fn observe_saturates_at_u64_max() {
        let f = HlcAdoptFloor::new();
        f.observe(u64::MAX); // +1 must not wrap to 0
        let now = 1_000u64;
        assert_eq!(f.merged_now(now), now + HLC_ADOPT_FORWARD_CAP_MS);
    }

    #[test]
    fn clones_share_state() {
        let f = HlcAdoptFloor::new();
        let g = f.clone();
        g.observe(9_999);
        // Read with now = the observed wall: merged_now clamps against
        // `now + CAP`, so a tiny `now` would cap the answer — this is also
        // why the feed-site tests (Tasks 5-7) read merged_now(at.wall_ms).
        assert_eq!(
            f.merged_now(9_999),
            10_000,
            "Arc-shared: feed via clone visible"
        );
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn adopt_cap_stays_far_below_consumer_budgets() {
        // The whole point IS asserting on constants: a compile-visible pin that
        // widening HLC_ADOPT_FORWARD_CAP_MS must consciously re-derive (spec §6.2).
        // ZEB-790 spec §6.2. Widening CAP past these relations invalidates
        // the blast-radius analysis — re-run it before touching this test.
        // 60_000 = the invite/open-join forward windows
        // (open_join_admit.rs `now + 60_000`, community_invite.rs same).
        //
        // ZEB-846 unified ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS onto
        // clock_trust::MAX_FORWARD_SKEW_MS (30min → 5min = 300_000ms);
        // headroom re-derived 360x → 60x. The 5s adopt cap stays far below the
        // tightened budget (60x), so ZEB-790 §6.2's blast-radius conclusion
        // holds — a 5s adopt advance is negligible against a 5min governance
        // skew gate. Widening CAP still trips this pin.
        assert!(HLC_ADOPT_FORWARD_CAP_MS * 12 <= 60_000);
        // NOTE (ZEB-548 Stage 1): the second pin
        //   HLC_ADOPT_FORWARD_CAP_MS * 60 <= community_membership::ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS
        // now lives in harmony-app (community_membership tests) — this crate is a
        // pure leaf and cannot see the community cluster.
    }
}
