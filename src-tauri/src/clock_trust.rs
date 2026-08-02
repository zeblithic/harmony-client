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
//!   [`crate::reachability_resolver::FUTURE_SKEW_TOLERANCE_MS`].
//! * [`DISPLAY_SKEW_TOLERANCE_MS`] (30 min) — pure display / discovery ordering
//!   where no control is gated (vine feed, discovery lists). A future-dated
//!   stamp can only mis-sort a list, not bypass a control. Its 30-min magnitude
//!   matches the vine discovery default
//!   ([`crate::vine_pull_driver::VINE_PULL_INVALID_FORWARD_SKEW_SECS`]).
//!
//! `community_membership::ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS` was *also* 30 min,
//! but it is a **governance control** budget, not a display-tier consumer — the
//! shared magnitude was coincidental. ZEB-846 (T-GOV) completed the migration:
//! it is now a 5-min alias of [`MAX_FORWARD_SKEW_MS`]. Do **not** point a new
//! *control* consumer at the display tier — controls take [`MAX_FORWARD_SKEW_MS`].
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
/// the existing `<= MAX_FORWARD_SKEW_MS` convention in the admin-proposal
/// planner filter (`community_membership::plan_admin_proposal_auto_exec`).
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

/// This node's own trusted wall clock as milliseconds since the Unix epoch,
/// or `None` when the system clock is pre-epoch / unreadable.
///
/// The single source for the receiver-`now` that every ZEB-846 forward-skew
/// bound is measured against. It is deliberately derived only from
/// [`std::time::SystemTime::now`] — NEVER a peer-supplied or HLC-adopt value,
/// which are exactly the clocks the bound exists to distrust (an attacker who
/// can nudge those forward must not be able to widen their own window).
///
/// The `None`-on-failure contract is load-bearing and callers MUST honour it as
/// "disable the forward bound (apply-all)", never substitute `0`: at `now = 0`
/// every honest present-day wall (~1.7e12 ms) exceeds [`MAX_FORWARD_SKEW_MS`],
/// so an `unwrap_or(0)` fallback would reject *every* real event and freeze
/// governance ingestion — the exact inversion of the §2 invariant that a bad
/// LOCAL clock must never drop honest governance.
#[inline]
pub fn receiver_now_ms() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_future_boundary_is_inclusive() {
        let now = 1_000_000;
        assert!(
            !reject_future(now, now, MAX_FORWARD_SKEW_MS),
            "present accepted"
        );
        assert!(
            !reject_future(now - 999, now, MAX_FORWARD_SKEW_MS),
            "past accepted"
        );
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
    fn receiver_now_ms_is_a_plausible_present_wall() {
        // Pins the load-bearing `.ok()` semantics: on any real (post-epoch)
        // host clock the helper yields `Some` — never the `unwrap_or(0)`
        // degenerate that would freeze governance ingestion. The lower bound
        // (2024-01-01T00:00:00Z in ms) is comfortably below any real run and
        // far above `MAX_FORWARD_SKEW_MS`, so it also documents that an honest
        // present-day wall is orders of magnitude past the ceiling — which is
        // exactly why a `now = 0` fallback would reject everything.
        let now = receiver_now_ms().expect("host clock is post-epoch");
        assert!(
            now > 1_704_067_200_000,
            "receiver_now_ms() must be a real present-day wall, got {now}"
        );
    }

    #[test]
    fn clamp_future_caps_only_the_future() {
        let now = 1_000_000;
        assert_eq!(
            clamp_future(now - 5, now, MAX_FORWARD_SKEW_MS),
            now - 5,
            "past unchanged"
        );
        assert_eq!(
            clamp_future(now, now, MAX_FORWARD_SKEW_MS),
            now,
            "present unchanged"
        );
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
        assert_eq!(
            DISPLAY_SKEW_TOLERANCE_SECS * 1000,
            DISPLAY_SKEW_TOLERANCE_MS
        );
        // The adoption floor's local nudge (5 s) is far below the control window
        // it must never widen past.
        assert!(crate::hlc_adopt_floor::HLC_ADOPT_FORWARD_CAP_MS < MAX_FORWARD_SKEW_MS);
        // The control tier is at or below governance's current ingest budget
        // (T-GOV may later tighten governance ordering TO this constant).
        assert!(
            MAX_FORWARD_SKEW_MS <= crate::community_membership::ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS
        );
    }
}
