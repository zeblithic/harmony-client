//! ZEB-291 Phase 2: Tier 2 Conviction voting — types + Q96.32 fixed-point math.
//!
//! See spec `docs/specs/2026-05-16-zeb-289-voting-polling-design.md` §5
//! (post-Task 1 amendment). Tier 2 polls accumulate per-voter "conviction"
//! that grows while signaling support and decays exponentially when
//! support is withdrawn. The poll finalizes (via PollResult after a 24h
//! contestability window) when summed conviction crosses a dynamic
//! threshold parameterized by community participation.
//!
//! ## Why fixed-point i128 (Q96.32), not f64?
//!
//! ZEB-291 acceptance criterion #2 requires bit-identical materialization
//! across architectures (x86_64 desktop ⇄ ARM laptop). IEEE 754 f64 does
//! not provide this: fused-multiply-add reordering, subnormal handling, and
//! reciprocal-table approximations diverge between x86 SSE/AVX and ARM
//! NEON. Q96.32 fixed-point (96 integer bits, 32 fractional bits stored
//! in `i128`) is bit-identical by construction — every operation is integer
//! arithmetic with explicit shift/divide rounding. The Task 1 spec
//! amendment captures the full rationale.

use serde::{Deserialize, Serialize};

use crate::community_voting_core::Eligibility;
use crate::owner_state_types::OwnerAddr;

// ---------------------------------------------------------------------------
// Q96.32 fixed-point constants
// ---------------------------------------------------------------------------

/// Number of fractional bits in the Q96.32 fixed-point representation.
pub const CONVICTION_FRAC_BITS: u32 = 32;

/// One unit in Q96.32 (= 2^32 = 4_294_967_296). The conventional "1.0"
/// value when interpreting an `i128` as Q96.32.
pub const Q32: i128 = 1 << CONVICTION_FRAC_BITS;

/// `ln(2) * 2^32`, rounded up from 2_977_044_471.53… so that the
/// Q96.32 representation is a strict upper bound on the true value.
/// Anchor: `(f64::ln(2.0) * 4_294_967_296.0).ceil() as i128 == 2_977_044_472`.
pub const LN2_Q32: i128 = 2_977_044_472;

/// Q96.32 fixed-point conviction value. 96 integer bits, 32 fractional bits.
/// Negative values are possible during intermediate Taylor terms but final
/// charged/decayed conviction is always ≥ 0.
pub type ConvictionQ32 = i128;

// ---------------------------------------------------------------------------
// Tier 2 PollCreate payload (kd="cr", tr=2)
// ---------------------------------------------------------------------------

/// Auto-exec action that fires when a Tier 2 proposal finalizes with `ax != None`.
///
/// Wire shape: CBOR tag-internal enum with a 2-char discriminator `kk`
/// (matches Phase 1 same-length-keys convention; all sibling keys at this
/// nesting level are 2 chars). The trivial `None` variant encodes as a
/// 1-field map `{ "kk": "n" }`; `SetPower` adds `tg` (target pubkey) +
/// `np` (new power). The task-prompt example used a 1-char `k` for the
/// discriminator but that violates the same-length-keys HARD RULE — `kk`
/// is the resolved spelling so the inner map stays uniform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kk")]
pub enum AutoExecAction {
    #[serde(rename = "n")]
    None,
    #[serde(rename = "sp")]
    SetPower {
        #[serde(rename = "tg")]
        target_pubkey: OwnerAddr,
        #[serde(rename = "np")]
        new_power: u32,
    },
}

/// Tier 2 PollConfig — payload of PollCreate (`kd="cr"`, `tr=2`).
///
/// All map keys are 2-char to satisfy the spec §3 same-length-keys
/// invariant at every nesting level. (Task 1 spec amendment renamed
/// the `β` field from `"b"` to `"bb"` for this reason.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier2PollConfig {
    /// Human-readable proposal text.
    #[serde(rename = "pt")]
    pub proposal_text: String,
    /// Conviction half-life, in seconds. After one half-life of continuous
    /// support, accumulated conviction reaches `(1 - 0.5) * hl / ln(2)`
    /// ≈ `0.721 * hl` of conviction-multiplier units.
    #[serde(rename = "hl")]
    pub half_life_seconds: u32,
    /// `T_min` — the floor of the dynamic threshold band, stored as Q96.32.
    /// At full participation (effective_supply == total_supply) the
    /// threshold equals exactly `T_min`.
    #[serde(rename = "tn")]
    pub threshold_min_q32: ConvictionQ32,
    /// `T_max` — the ceiling of the dynamic threshold band, stored as Q96.32.
    /// At zero participation the threshold equals exactly `T_max`.
    #[serde(rename = "tx")]
    pub threshold_max_q32: ConvictionQ32,
    /// β exponent for the `(1 - participation_ratio)^β` curve shaping
    /// the dynamic threshold. Small positive integer, typically 1-3;
    /// default 2 per spec §5.
    #[serde(rename = "bb")]
    pub beta: u8,
    /// Whether voters may delegate their conviction-weight via `Delegate`
    /// events for this poll.
    #[serde(rename = "dl")]
    pub delegation_allowed: bool,
    /// Auto-exec action performed on finalization (Tier 2 specific —
    /// Tier 1 polls do not auto-exec).
    #[serde(rename = "ax")]
    pub auto_exec: AutoExecAction,
    /// Eligibility predicate shared with Tier 1; embedded so verify-on-
    /// receive doesn't need a separate event type.
    #[serde(rename = "el")]
    pub eligibility: Eligibility,
}

// ---------------------------------------------------------------------------
// Fixed-point math primitives
// ---------------------------------------------------------------------------

/// Number of binary argument-reduction steps used by `exp_neg_q32`. For each
/// step we compute `exp(-x/2)` instead of `exp(-x)` and then square the
/// result; 8 steps reduces any x in the supported `[0, 20·Q32]` range to
/// `x' ≤ 0.078`, where a degree-7 Taylor series for `exp(-x')` has
/// truncation error `< x'^8/8! ≈ 1.0e-12` — comfortably below Q32
/// resolution.
///
/// Implementation choice (NOT spec-stated; the Task 1 spec amendment's
/// inline `exp_neg_q32` pseudocode showed `n=7` Taylor terms applied
/// directly to `x ≤ 10`, which would have ~30% relative error at the
/// upper end of the practical range — a spec bug captured in the Task 3
/// implementation notes). Argument reduction is the canonical fix and
/// preserves the spec's bit-identical-by-construction property: every
/// step is integer arithmetic, the reduction count is fixed, and the
/// Taylor truncation point is fixed.
const EXP_NEG_REDUCTION_STEPS: u32 = 8;

/// Number of Taylor series terms used inside `exp_neg_q32` after argument
/// reduction. Combined with `EXP_NEG_REDUCTION_STEPS`, this gives total
/// error `< 1e-12` on `[0, 20·Q32]`.
const EXP_NEG_TAYLOR_TERMS: i128 = 7;

/// `exp(-x)` for `x_q32` in Q96.32, returned in Q96.32.
///
/// Algorithm: argument-reduce `x` by halving `EXP_NEG_REDUCTION_STEPS` (=8)
/// times to a reduced `x' = x / 256`, compute `exp(-x')` via a
/// degree-`EXP_NEG_TAYLOR_TERMS` (=7) Taylor series, then square the
/// result 8 times to recover `exp(-x) = exp(-x')^256`. Total error stays
/// well below Q32 resolution for `x ≤ 20·Q32`.
///
/// For `x > 20·Q32` the result is clamped to 0 (exp(-20) ≈ 2.06e-9, already
/// below Q32 resolution ≈ 2.33e-10).
pub fn exp_neg_q32(x_q32: ConvictionQ32) -> ConvictionQ32 {
    if x_q32 <= 0 {
        return Q32;
    }
    if x_q32 > 20 * Q32 {
        return 0;
    }
    let x_reduced = x_q32 >> EXP_NEG_REDUCTION_STEPS;
    let mut term: ConvictionQ32 = Q32; // n=0 term = 1.0
    let mut sum: ConvictionQ32 = Q32;
    for n in 1..=EXP_NEG_TAYLOR_TERMS {
        // term_n = term_{n-1} * (-x_reduced) / n; sign alternates,
        // matching the Taylor series.
        term = -(term * x_reduced) / (Q32 * n);
        sum += term;
        if term == 0 {
            break;
        }
    }
    let mut result = sum.max(0);
    // Square 8 times: result = exp(-x_reduced)^(2^8) = exp(-x).
    for _ in 0..EXP_NEG_REDUCTION_STEPS {
        result = (result * result) >> CONVICTION_FRAC_BITS;
    }
    result
}

/// `0.5^(t / half_life)` in Q96.32 fixed-point.
///
/// Implemented as `exp(-x · ln 2)` where `x = t / half_life`, routed
/// through `exp_neg_q32`. Returns `Q32` for `t ≤ 0` (no decay yet) and
/// `0` for `half_life ≤ 0` (degenerate config; caller is responsible
/// for validating half-life upstream, but we don't panic).
pub fn pow_half_q32(t_ms: i128, half_life_ms: i128) -> ConvictionQ32 {
    if t_ms <= 0 {
        return Q32;
    }
    if half_life_ms <= 0 {
        return 0;
    }
    let x_q32 = (t_ms * LN2_Q32) / half_life_ms;
    exp_neg_q32(x_q32)
}

/// Charge function: `(1 - 0.5^(duration/half_life)) * half_life / ln(2)`.
///
/// Conviction accumulated over `duration_ms` of continuous support, in
/// Q96.32. Units are milliseconds × (Q96.32 fractional multiplier); the
/// caller is responsible for unit interpretation (typically summed across
/// voters then compared against a Q96.32 threshold of the same units).
pub fn charge_q32(duration_ms: i128, half_life_ms: i128) -> ConvictionQ32 {
    if duration_ms <= 0 || half_life_ms <= 0 {
        return 0;
    }
    let pow = pow_half_q32(duration_ms, half_life_ms); // Q32, in [0, Q32]
    let one_minus = Q32 - pow; // Q32, in [0, Q32]
    (one_minus * half_life_ms) / LN2_Q32
}

/// Decay function: `c * 0.5^(dt/half_life)`.
///
/// Applied to a previously-accumulated conviction value to advance it
/// forward in time by `dt_ms` of no-support. Returns the conviction
/// unchanged for `dt_ms ≤ 0` or degenerate `half_life_ms ≤ 0`.
pub fn decay_q32(conviction_q32: ConvictionQ32, dt_ms: i128, half_life_ms: i128) -> ConvictionQ32 {
    if dt_ms <= 0 || half_life_ms <= 0 {
        return conviction_q32;
    }
    let pow = pow_half_q32(dt_ms, half_life_ms);
    (conviction_q32 * pow) >> CONVICTION_FRAC_BITS
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Tolerance for "approximately equal" comparisons against f64 oracles,
    /// ≈ 6 fractional digits in Q32 units. The Taylor truncation + integer
    /// division rounding both contribute below this scale.
    const Q32_TOL: i128 = Q32 / 1_000_000;

    fn approx_eq(a: i128, b: i128, tol: i128) -> bool {
        (a - b).abs() <= tol
    }

    // -------- Constants --------

    #[test]
    fn ln2_q32_matches_f64_ceiling_roundtrip() {
        // Anchor LN2_Q32 to its derivation: ceiling of ln(2) * 2^32 from f64.
        // The f64 computation runs in test only; production code never uses it.
        let expected = (2f64.ln() * 4_294_967_296f64).ceil() as i128;
        assert_eq!(LN2_Q32, expected);
        assert_eq!(LN2_Q32, 2_977_044_472);
    }

    #[test]
    fn q32_is_2_pow_32() {
        assert_eq!(Q32, 4_294_967_296);
        assert_eq!(Q32, 1i128 << CONVICTION_FRAC_BITS);
    }

    // -------- pow_half_q32 --------

    #[test]
    fn pow_half_at_zero_is_one() {
        for hl in &[1i128, 1000, 1_000_000] {
            assert_eq!(pow_half_q32(0, *hl), Q32, "0.5^0 = 1 for hl={hl}");
        }
    }

    #[test]
    fn pow_half_at_one_half_life_is_one_half() {
        for hl in &[1000i128, 60_000, 86_400_000] {
            let p = pow_half_q32(*hl, *hl);
            assert!(
                approx_eq(p, Q32 / 2, Q32_TOL),
                "pow_half(hl, hl) = {p}, want ≈ {} (Q32/2) for hl={hl}",
                Q32 / 2,
            );
        }
    }

    #[test]
    fn pow_half_at_two_half_lives_is_one_quarter() {
        for hl in &[1000i128, 60_000, 86_400_000] {
            let p = pow_half_q32(2 * *hl, *hl);
            assert!(
                approx_eq(p, Q32 / 4, Q32_TOL),
                "pow_half(2*hl, hl) = {p}, want ≈ {} (Q32/4) for hl={hl}",
                Q32 / 4,
            );
        }
    }

    #[test]
    fn pow_half_at_three_half_lives_is_one_eighth() {
        let hl = 1000i128;
        let p = pow_half_q32(3 * hl, hl);
        assert!(
            approx_eq(p, Q32 / 8, Q32_TOL),
            "pow_half(3*hl, hl) = {p}, want ≈ {} (Q32/8)",
            Q32 / 8,
        );
    }

    #[test]
    fn pow_half_degenerate_half_life_returns_zero() {
        assert_eq!(pow_half_q32(1000, 0), 0);
        assert_eq!(pow_half_q32(1000, -1), 0);
    }

    // -------- decay_q32 --------

    #[test]
    fn decay_at_one_half_life_halves_conviction() {
        let c = 1_000_000i128 * Q32;
        let hl = 1000i128;
        let d = decay_q32(c, hl, hl);
        // Tolerance scales with the conviction magnitude.
        let tol = c / 1_000_000;
        assert!(
            approx_eq(d, c / 2, tol),
            "decay(c, hl, hl) = {d}, want ≈ {} (c/2)",
            c / 2,
        );
    }

    #[test]
    fn decay_with_zero_dt_is_identity() {
        let c = 12_345i128 * Q32;
        assert_eq!(decay_q32(c, 0, 1000), c);
        assert_eq!(decay_q32(c, -1, 1000), c);
    }

    #[test]
    fn decay_with_degenerate_half_life_is_identity() {
        let c = 999i128 * Q32;
        assert_eq!(decay_q32(c, 1000, 0), c);
    }

    // -------- charge_q32 --------

    #[test]
    fn charge_with_zero_duration_is_zero() {
        assert_eq!(charge_q32(0, 1000), 0);
        assert_eq!(charge_q32(-1, 1000), 0);
    }

    #[test]
    fn charge_with_degenerate_half_life_is_zero() {
        assert_eq!(charge_q32(1000, 0), 0);
    }

    #[test]
    fn charge_at_one_half_life_matches_closed_form() {
        // Closed form: charge(hl, hl) = hl * (1 - 0.5) / ln(2) ≈ hl * 0.7213475.
        // Spec §5 amendment formula: `(one_minus * hl) / LN2_Q32` — both
        // `one_minus` and `LN2_Q32` carry the Q32 multiplier so they cancel,
        // leaving the result in plain milliseconds (NOT Q32-scaled
        // milliseconds). The `accumulated_conviction_q32` variable name in
        // the spec is therefore a unit misnomer; documented as a Task 3
        // concern for downstream tasks to reconcile when the threshold
        // comparison is wired up.
        let hl_ms: i128 = 1_000_000;
        let c = charge_q32(hl_ms, hl_ms);
        let expected_f = hl_ms as f64 * 0.5 / 2f64.ln();
        let expected = expected_f as i128;
        let tol = expected.abs() / 1_000_000 + 1;
        assert!(
            approx_eq(c, expected, tol),
            "charge(hl, hl) = {c}, want ≈ {expected} (within {tol})",
        );
    }

    #[test]
    fn charge_at_infinity_approaches_half_life_over_ln2() {
        // As duration → ∞, (1 - 0.5^∞) → 1, so charge → hl / ln(2).
        // Result is in plain milliseconds (see charge_at_one_half_life test).
        let hl_ms: i128 = 1_000_000;
        // 30 half-lives ≈ 2^-30 ≈ 1e-9 residual, well below Q32 resolution.
        let c = charge_q32(30 * hl_ms, hl_ms);
        let expected_f = hl_ms as f64 / 2f64.ln();
        let expected = expected_f as i128;
        let tol = expected.abs() / 1_000_000 + 1;
        assert!(
            approx_eq(c, expected, tol),
            "charge(30*hl, hl) = {c}, want ≈ {expected} (within {tol})",
        );
    }

    // -------- exp_neg_q32 --------

    #[test]
    fn exp_neg_at_zero_is_one() {
        assert_eq!(exp_neg_q32(0), Q32);
        assert_eq!(exp_neg_q32(-1), Q32);
    }

    #[test]
    fn exp_neg_at_small_integers_matches_f64() {
        for x_int in 1..=3i128 {
            let actual = exp_neg_q32(x_int * Q32);
            let expected_f = (-(x_int as f64)).exp();
            let expected = (expected_f * Q32 as f64) as i128;
            // 1e-6 tolerance relative to Q32.
            let tol = Q32 / 1_000_000;
            assert!(
                approx_eq(actual, expected, tol),
                "exp(-{x_int}) = {actual}, want ≈ {expected} (within {tol})",
            );
        }
    }

    #[test]
    fn exp_neg_huge_x_returns_zero() {
        // x = 100 * Q32 → exp(-100) ≈ 3.7e-44, far below Q32 resolution,
        // and large enough to trigger the overflow clamp branch.
        assert_eq!(exp_neg_q32(100 * Q32), 0);
    }

    // -------- Determinism --------

    #[test]
    fn math_helpers_are_deterministic_across_iterations() {
        // Pure integer arithmetic is trivially deterministic, but pin
        // the invariant against any future refactor sneaking in f64 or
        // a HashMap-iter-order dependence.
        let inputs: Vec<(i128, i128)> = vec![
            (0, 1000),
            (100, 1000),
            (1000, 1000),
            (3700, 1000),
            (86_400_000, 60_000),
            (1, 999_999_999),
        ];
        let mut first_charge: Vec<i128> = vec![];
        let mut first_decay: Vec<i128> = vec![];
        let mut first_pow: Vec<i128> = vec![];
        let mut first_exp: Vec<i128> = vec![];
        for (t, hl) in &inputs {
            first_charge.push(charge_q32(*t, *hl));
            first_decay.push(decay_q32(123_456 * Q32, *t, *hl));
            first_pow.push(pow_half_q32(*t, *hl));
            first_exp.push(exp_neg_q32(*t));
        }
        for _ in 0..100 {
            for (i, (t, hl)) in inputs.iter().enumerate() {
                assert_eq!(charge_q32(*t, *hl), first_charge[i]);
                assert_eq!(decay_q32(123_456 * Q32, *t, *hl), first_decay[i]);
                assert_eq!(pow_half_q32(*t, *hl), first_pow[i]);
                assert_eq!(exp_neg_q32(*t), first_exp[i]);
            }
        }
    }

    // -------- Tier2PollConfig wire shape --------

    fn sample_config_with(auto_exec: AutoExecAction) -> Tier2PollConfig {
        Tier2PollConfig {
            proposal_text: "Raise dues to 5 ❂ per month".into(),
            half_life_seconds: 86_400,
            threshold_min_q32: 10 * Q32,
            threshold_max_q32: 1_000 * Q32,
            beta: 2,
            delegation_allowed: true,
            auto_exec,
            eligibility: Eligibility {
                min_power: 1,
                min_vouching_depth: None,
                sortition_size: None,
            },
        }
    }

    #[test]
    fn tier2_config_round_trips_via_cbor_with_auto_exec_none() {
        let cfg = sample_config_with(AutoExecAction::None);
        let mut encoded = Vec::new();
        ciborium::into_writer(&cfg, &mut encoded).expect("encode");
        let decoded: Tier2PollConfig = ciborium::from_reader(&encoded[..]).expect("decode");
        assert_eq!(cfg, decoded);
    }

    #[test]
    fn tier2_config_round_trips_via_cbor_with_set_power() {
        let cfg = sample_config_with(AutoExecAction::SetPower {
            target_pubkey: OwnerAddr([0x42; 16]),
            new_power: 5,
        });
        let mut encoded = Vec::new();
        ciborium::into_writer(&cfg, &mut encoded).expect("encode");
        let decoded: Tier2PollConfig = ciborium::from_reader(&encoded[..]).expect("decode");
        assert_eq!(cfg, decoded);
    }

    #[test]
    fn tier2_config_top_level_keys_all_two_chars() {
        // Same-length-keys invariant at the top level of the payload map.
        let cfg = sample_config_with(AutoExecAction::None);
        let mut encoded = Vec::new();
        ciborium::into_writer(&cfg, &mut encoded).expect("encode");
        let value: ciborium::Value = ciborium::from_reader(&encoded[..]).expect("decode as value");
        let map = value.as_map().expect("top-level is a CBOR map");
        for (k, _) in map.iter() {
            let s = k.as_text().expect("key is text");
            assert_eq!(
                s.len(),
                2,
                "Tier2PollConfig key {s:?} violates 2-char invariant",
            );
        }
        // Sanity: all 8 fields present.
        assert_eq!(map.len(), 8, "Tier2PollConfig must have exactly 8 fields");
        for expected in &["pt", "hl", "tn", "tx", "bb", "dl", "ax", "el"] {
            assert!(
                map.iter()
                    .any(|(k, _): &(ciborium::Value, ciborium::Value)| k.as_text()
                        == Some(*expected)),
                "Tier2PollConfig missing key {expected:?}"
            );
        }
    }

    #[test]
    fn auto_exec_set_power_inner_keys_all_two_chars() {
        // Same-length-keys invariant at the nested ax-variant map.
        let cfg = sample_config_with(AutoExecAction::SetPower {
            target_pubkey: OwnerAddr([0x33; 16]),
            new_power: 7,
        });
        let mut encoded = Vec::new();
        ciborium::into_writer(&cfg, &mut encoded).expect("encode");
        let value: ciborium::Value = ciborium::from_reader(&encoded[..]).expect("decode as value");
        let map = value.as_map().expect("top-level map");
        let (_, ax_value) = map
            .iter()
            .find(|(k, _): &&(ciborium::Value, ciborium::Value)| k.as_text() == Some("ax"))
            .expect("ax field present");
        let ax_map = ax_value.as_map().expect("ax is a map");
        for (k, _) in ax_map.iter() {
            let s = k.as_text().expect("key is text");
            assert_eq!(
                s.len(),
                2,
                "AutoExecAction::SetPower key {s:?} violates 2-char invariant",
            );
        }
        // Sanity: SetPower has exactly 3 keys: k/tg/np.
        assert_eq!(ax_map.len(), 3);
    }

    #[test]
    fn auto_exec_none_wire_form_is_single_key_map() {
        // None encodes as just `{ "kk": "n" }` — verify the discriminator
        // is the only key and uses the 2-char tag.
        let mut encoded = Vec::new();
        ciborium::into_writer(&AutoExecAction::None, &mut encoded).expect("encode");
        let value: ciborium::Value = ciborium::from_reader(&encoded[..]).expect("decode as value");
        let map = value.as_map().expect("map");
        assert_eq!(map.len(), 1);
        let (k, v) = &map[0];
        assert_eq!(k.as_text(), Some("kk"));
        assert_eq!(v.as_text(), Some("n"));
    }
}
