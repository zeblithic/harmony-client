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

/// Parse an RFC-3339 timestamp to non-negative epoch milliseconds.
///
/// Accepts both honest mint encodings — `+00:00` with variable fractional
/// precision (`chrono::Utc::now().to_rfc3339()`) and the fixed `...Z` epoch
/// literal. Pre-epoch instants floor to `0` (ancient — never future).
/// Returns `None` when `stamp` is not valid RFC-3339.
pub fn parse_rfc3339_ms(stamp: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(stamp)
        .ok()
        .map(|dt| dt.timestamp_millis().max(0) as u64)
}

/// Reject a peer-supplied RFC-3339 wall stamp used as a stored, replicated LWW
/// ordering key. Returns `true` when the remote row MUST be dropped at ingest:
///
/// * the stamp does not parse as RFC-3339 (never a valid ordering key — e.g.
///   `"zzzz"`, which also sorts above every real stamp under a raw string
///   compare), or
/// * it parses to more than `tol_ms` beyond the receiver clock `now_ms`.
///
/// `now_ms == None` (receiver clock unreadable) ⇒ the forward-skew half is
/// skipped (fail-open on skew, matching `receiver_now_ms`'s contract), but an
/// unparseable stamp is still rejected — that check needs no clock.
pub fn reject_rfc3339_future(stamp: &str, now_ms: Option<u64>, tol_ms: u64) -> bool {
    match parse_rfc3339_ms(stamp) {
        None => true,
        Some(ms) => now_ms.is_some_and(|now| reject_future(ms, now, tol_ms)),
    }
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

/// `true` iff `wall_ms` is implausibly far in the receiver's *future* under the
/// control-tier ceiling ([`MAX_FORWARD_SKEW_MS`]). `receiver_now_ms == None`
/// (unreadable / pre-epoch clock) ⇒ `false` (apply-all): a bad LOCAL clock must
/// never drop honest state. The forward-skew half of the T-OWNER (ZEB-847) and
/// T-GOV (ZEB-846) owner/governance merge bounds; boundary is inclusive.
#[inline]
pub fn wall_exceeds_forward_skew(wall_ms: u64, receiver_now_ms: Option<u64>) -> bool {
    receiver_now_ms.is_some_and(|rn| reject_future(wall_ms, rn, MAX_FORWARD_SKEW_MS))
}

/// Seconds-domain sibling of [`wall_exceeds_forward_skew`] for callers whose
/// receiver clock is epoch-**seconds** (e.g.
/// `crate::iroh_friend_acceptor::wall_now_secs` = `wall_now_ms()/1000` with
/// `.unwrap_or(0)`), and who pick the tier explicitly: [`MAX_FORWARD_SKEW_MS`]
/// for a control/identity concern, [`DISPLAY_SKEW_TOLERANCE_MS`] for pure
/// display / discovery ordering.
///
/// `now_secs == 0` is the unreadable-local-clock sentinel (the `.unwrap_or(0)`
/// above) ⇒ `false` (apply-all): a bad LOCAL clock must never drop honest state.
///
/// The `+999` upper-bounds the floored seconds→ms conversion: `now_secs` is
/// `floor(true_now_ms / 1000)`, so `now_secs * 1000` can trail true wall time by
/// up to 999 ms. Comparing against `now_secs*1000 + 999` keeps the compensated
/// `now` at or above true wall time, so sub-second truncation never *tightens*
/// the ceiling — it errs toward accepting an honest near-ceiling stamp rather
/// than dropping it (the fail-open priority), at a cost of at most 999 ms of
/// extra tolerance on a 5-min-plus window.
#[inline]
pub fn wall_exceeds_forward_skew_secs(wall_ms: u64, now_secs: u64, tolerance_ms: u64) -> bool {
    now_secs != 0
        && reject_future(
            wall_ms,
            now_secs.saturating_mul(1000).saturating_add(999),
            tolerance_ms,
        )
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
    fn wall_exceeds_forward_skew_none_now_is_apply_all() {
        // Unreadable local clock ⇒ never reject (a bad LOCAL clock must not drop honest state).
        assert!(!wall_exceeds_forward_skew(u64::MAX, None));
        assert!(!wall_exceeds_forward_skew(0, None));
    }

    #[test]
    fn wall_exceeds_forward_skew_honors_the_inclusive_ceiling() {
        let now = 1_700_000_000_000;
        assert!(
            !wall_exceeds_forward_skew(now, Some(now)),
            "present accepted"
        );
        assert!(
            !wall_exceeds_forward_skew(now - 10_000, Some(now)),
            "past accepted"
        );
        assert!(
            !wall_exceeds_forward_skew(now + MAX_FORWARD_SKEW_MS, Some(now)),
            "exactly at the ceiling is accepted (inclusive)"
        );
        assert!(
            wall_exceeds_forward_skew(now + MAX_FORWARD_SKEW_MS + 1, Some(now)),
            "one past the ceiling is rejected"
        );
    }

    #[test]
    fn wall_exceeds_forward_skew_secs_zero_now_is_apply_all() {
        assert!(!wall_exceeds_forward_skew_secs(
            u64::MAX,
            0,
            MAX_FORWARD_SKEW_MS
        ));
        assert!(!wall_exceeds_forward_skew_secs(
            u64::MAX,
            0,
            DISPLAY_SKEW_TOLERANCE_MS
        ));
    }

    #[test]
    fn wall_exceeds_forward_skew_secs_compensates_floored_conversion_and_honors_tier() {
        let now_secs = 1_700_000_000u64;
        let now_ms = now_secs * 1000;
        // Compensated inclusive ceiling is now_ms + 999 + tolerance.
        assert!(
            !wall_exceeds_forward_skew_secs(now_ms, now_secs, MAX_FORWARD_SKEW_MS),
            "present accepted"
        );
        assert!(
            !wall_exceeds_forward_skew_secs(
                now_ms + 999 + MAX_FORWARD_SKEW_MS,
                now_secs,
                MAX_FORWARD_SKEW_MS
            ),
            "at the compensated ceiling accepted (no sub-second tightening)"
        );
        assert!(
            wall_exceeds_forward_skew_secs(
                now_ms + 1000 + MAX_FORWARD_SKEW_MS,
                now_secs,
                MAX_FORWARD_SKEW_MS
            ),
            "one ms past the compensated ceiling rejected"
        );
        // Tier is a parameter: 10 min ahead exceeds the control tier but is within
        // the display tier.
        let ten_min = 10 * 60 * 1000;
        assert!(
            wall_exceeds_forward_skew_secs(now_ms + ten_min, now_secs, MAX_FORWARD_SKEW_MS),
            "10 min > 5-min control tier"
        );
        assert!(
            !wall_exceeds_forward_skew_secs(now_ms + ten_min, now_secs, DISPLAY_SKEW_TOLERANCE_MS),
            "10 min < 30-min display tier"
        );
    }

    #[test]
    fn parse_rfc3339_ms_accepts_both_honest_encodings() {
        // `Z` and `+00:00` at the same instant parse equal.
        let z = parse_rfc3339_ms("2026-08-02T15:00:00Z").unwrap();
        let off = parse_rfc3339_ms("2026-08-02T15:00:00+00:00").unwrap();
        assert_eq!(z, off);
        // Variable fractional precision (production `to_rfc3339()` shape).
        assert!(parse_rfc3339_ms("2026-08-02T15:00:00.789+00:00").is_some());
        // Epoch literal → 0; pre-epoch floors to 0 (ancient, never future).
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_ms("1960-01-01T00:00:00Z"), Some(0));
        // Garbage / empty → None.
        assert_eq!(parse_rfc3339_ms("zzzz"), None);
        assert_eq!(parse_rfc3339_ms(""), None);
    }

    #[test]
    fn reject_rfc3339_future_drops_unparseable_and_far_future() {
        let now: u64 = 1_760_000_000_000;
        let tol = MAX_FORWARD_SKEW_MS;
        let ms_to_str = |ms: u64| {
            chrono::DateTime::from_timestamp_millis(ms as i64)
                .unwrap()
                .to_rfc3339()
        };

        // Unparseable → reject regardless of clock (poison; and it sorts above real stamps).
        assert!(reject_rfc3339_future("zzzz", Some(now), tol));
        assert!(reject_rfc3339_future("zzzz", None, tol));

        // Parseable far-future → reject when the clock is readable.
        assert!(reject_rfc3339_future(
            "9999-12-31T23:59:59Z",
            Some(now),
            tol
        ));
        // …but fail-open on skew when the local clock is unreadable.
        assert!(!reject_rfc3339_future("9999-12-31T23:59:59Z", None, tol));

        // Inclusive boundary: exactly now+tol is accepted; just beyond is rejected.
        assert!(!reject_rfc3339_future(
            &ms_to_str(now + tol),
            Some(now),
            tol
        ));
        assert!(reject_rfc3339_future(
            &ms_to_str(now + tol + 1_000),
            Some(now),
            tol
        ));
        // Within tolerance and in the past are accepted.
        assert!(!reject_rfc3339_future(
            &ms_to_str(now + tol - 1_000),
            Some(now),
            tol
        ));
        assert!(!reject_rfc3339_future(
            &ms_to_str(now - 60_000),
            Some(now),
            tol
        ));
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
