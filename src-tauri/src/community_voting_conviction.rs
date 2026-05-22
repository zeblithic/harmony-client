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

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::community_voting_core::{Eligibility, PollId};
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
    /// `T_min` — the floor of the dynamic threshold band, in the same
    /// units `charge_q32` / `total_conviction_at` return (raw
    /// milliseconds at the conviction-multiplier scale; see `charge_q32`
    /// doc). At full participation (effective_supply == total_supply)
    /// the threshold equals exactly `T_min`. The `_q32` field-name
    /// suffix is vestigial — spec §5's first draft documented these as
    /// Q96.32 fractions of a notional ceiling, but the actual conviction
    /// arithmetic (`(1 - 0.5^(d/h)) * h / ln(2)`) yields ms-scale
    /// values, so thresholds must be ms-scale for `just_crossed_threshold`
    /// to be meaningful. A sensible value at hl=7d is on the order of
    /// `charge(1d, 7d) ≈ 8.2e6 ms` per voter-day of desired effort.
    #[serde(rename = "tn")]
    pub threshold_min_q32: ConvictionQ32,
    /// `T_max` — the ceiling of the dynamic threshold band, same ms
    /// units as `threshold_min_q32` (see that field's doc). At zero
    /// participation the threshold equals exactly `T_max`.
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

/// ZEB-298: community-scoped voting policy. Settings that apply across
/// all polls in a community (Tier 1 + Tier 3 currently have no
/// community-scoped settings; this struct grows organically as more
/// policy fields are needed). Mutated via IPC (Task 6), not via signed
/// event — policy is local UX preference, not consensus-relevant.
///
/// All fields default to `false` so existing communities that don't
/// have a policy stored (or have an older serialized policy missing
/// new fields) preserve their pre-policy behavior.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommunityVotingPolicy {
    /// ZEB-298: when `true`, the engine emits
    /// `voting-delegate-signaled-on-your-behalf` on inbound kd=signal
    /// (Tier 2 Signal) when the signaler is the local user's current
    /// delegate in this community. Opt-in so existing communities
    /// don't suddenly notify.
    ///
    /// `skip_serializing_if = "<not>"` makes a `false` value omit
    /// the key on the wire so a default-valued policy encodes as the
    /// empty CBOR map (`0xA0`). Combined with `#[serde(default)]` on
    /// deserialize, this preserves the upgrade-in-place property:
    /// communities storing an older shape (or no shape yet) decode to
    /// today's `Default` and would re-encode to the same empty map.
    /// `wire_format_community_voting_policy_fixtures.rs` byte-pins
    /// this invariant.
    #[serde(default, rename = "nd", skip_serializing_if = "::std::ops::Not::not")]
    pub notify_on_delegate_signal: bool,

    /// ZEB-295 Phase 6: community-wide default for Tier 3 PollCreate
    /// `privacy_mode` when the proposer does not explicitly set one in the
    /// IPC payload. Accepts `"pu"` (public scores) or `"se"` (ballot-secret
    /// threshold-ElGamal); `"rf"` is reserved for Phase 7. The per-poll
    /// `Tier3PollConfigPayload.privacy_mode` always wins — this field is
    /// only consulted when the proposer omits the override.
    ///
    /// Wire-format stability: `skip_serializing_if = "is_pu_default"` ensures
    /// a policy whose only divergence from default is this `"pu"` value still
    /// encodes as the empty CBOR map (`0xA0`), preserving the upgrade-in-
    /// place property pinned by `wire_format_community_voting_policy_fixtures.rs`.
    #[serde(
        default = "default_tier3_privacy_mode",
        rename = "t3",
        skip_serializing_if = "is_pu_default"
    )]
    pub tier3_privacy_mode_default: String,
}

fn default_tier3_privacy_mode() -> String {
    "pu".into()
}

#[allow(clippy::ptr_arg)] // serde's skip_serializing_if requires &String, not &str
fn is_pu_default(s: &String) -> bool {
    s == "pu"
}

impl Default for CommunityVotingPolicy {
    fn default() -> Self {
        Self {
            notify_on_delegate_signal: false,
            tier3_privacy_mode_default: default_tier3_privacy_mode(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tier 2 event payloads (Signal / Delegate / Undelegate)
// ---------------------------------------------------------------------------

/// Signal payload — wire form for `kd="sg"`, `tr=2`.
///
/// Same-length-keys (§3): two 2-char keys. The `"sp"` key is a Phase-2
/// rename of the spec's original `"s"` (1-char) — the §3 invariant pins
/// every-key-at-this-nesting-level to identical lengths.
/// `tests/wire_format_zeb291_fixtures.rs` byte-pins this shape so any
/// drift in serde renames or field ordering fails before it can land.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalPayload {
    /// Proposal (poll) id this Signal targets.
    #[serde(rename = "pr")]
    pub proposal_id: PollId,
    /// `true` = start (or continue) signaling support; `false` = withdraw
    /// support. Per spec §5 this is a toggle — repeated same-direction
    /// Signal events are idempotent at the `VoterConvictionState` layer.
    #[serde(rename = "sp")]
    pub support: bool,
}

/// Delegate payload — wire form for `kd="dg"`, `tr=2`.
///
/// `to` is the 16-byte `OwnerAddr` of the delegate. Spec §5's original
/// "32-byte ed25519 pubkey" choice was incompatible with the rest of the
/// CRDT, which keys on `OwnerAddr = SHA256(X25519_pub || Ed25519_pub)[..16]`
/// — that hash cannot be derived from just the Ed25519 pubkey, so any
/// delegate-target field that carried the raw pubkey would never match
/// the membership-graph keys. Carrying the OwnerAddr directly mirrors
/// every other actor identifier in the system. The `OwnerAddr` newtype's
/// serde impl emits a CBOR `bstr(16)` and rejects any other length at
/// decode time (CR R5 Major — the prior `Vec<u8>` + `serde_bytes` shape
/// accepted any length on the wire and relied on a downstream length
/// check, leaving a window where the wire boundary and the apply layer
/// could disagree).
///
/// `sc` ("scope") is reserved for future per-poll or per-tag delegation
/// scoping; v1 callers always pass `"all"` (community-wide delegation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegatePayload {
    #[serde(rename = "to")]
    pub to: OwnerAddr,
    #[serde(rename = "sc")]
    pub scope: String,
}

/// Undelegate payload — wire form for `kd="ud"`, `tr=2`. Empty map per
/// spec §5; the delegator (`event.actor`) is the only data needed and
/// already lives in the envelope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndelegatePayload {}

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
// Per-voter conviction state
// ---------------------------------------------------------------------------

/// Per-voter conviction state for a single Tier 2 proposal.
///
/// Persisted-across-events; aggregated by `Tier2ProposalState` (Task 6).
/// All time values are i128 milliseconds (signed for arithmetic safety).
/// All conviction values are Q96.32 fixed-point.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoterConvictionState {
    /// Currently signaling support? (Toggled by Signal events.)
    pub is_supporting: bool,
    /// HLC of the most recent Signal{true} that started the current support session.
    /// 0 if has never signaled support.
    pub support_started_at_ms: i128,
    /// Conviction accumulated from PRIOR support sessions (i.e., sessions
    /// that have been "closed" by a Signal{false}). Q96.32.
    pub accumulated_conviction_q32: ConvictionQ32,
    /// HLC of the last event applied (Signal toggle in either direction).
    /// Used as the reference point for decaying `accumulated_conviction_q32`.
    pub last_event_at_ms: i128,
    /// Full `(wall_ms, logical)` HLC ordinal of the last event applied,
    /// or `None` if no event has been applied yet. Used by `apply_signal`
    /// to enforce LWW across (a) same-ms different-logical events on one
    /// device and (b) inter-device re-orderings on the same actor.
    /// Without this, a stale event arriving after a newer one would
    /// overwrite state with rewound `support_started_at_ms`. `None` is
    /// the sentinel because `(0, 0)` is a valid (test-only) ordinal.
    pub last_event_ordinal: Option<(u64, u32)>,
}

impl VoterConvictionState {
    /// Apply a Signal event at the given HLC. Idempotent: a repeated
    /// Signal in the same direction is a no-op. Events whose HLC
    /// ordinal is `<=` the last applied ordinal are dropped (LWW), so
    /// out-of-order delivery converges to the same state on every replica.
    pub fn apply_signal(
        &mut self,
        support: bool,
        event_hlc_ms: i128,
        event_hlc_logical: u32,
        half_life_ms: i128,
    ) {
        // Reject negative event_hlc_ms. The production caller always
        // passes `event.hlc.wall_ms as i128` where `wall_ms: u64`, so
        // production traffic never lands here negative. But the i128
        // signature permits negative values from tests or malformed
        // peer events, and a naive `as u64` cast would wrap a negative
        // ms into `u64::MAX`-adjacent — making the event appear newer
        // than any prior recorded ordinal and slipping past the LWW
        // guard. Cursor R6 Medium.
        if event_hlc_ms < 0 {
            return;
        }
        let event_ord = (event_hlc_ms as u64, event_hlc_logical);
        if matches!(self.last_event_ordinal, Some(last) if event_ord <= last) {
            return;
        }
        // Always bump the high-water mark on a freshness-passing event —
        // including idempotent same-direction Signals where no other
        // state changes. Skipping the bump leaves a hole: a later-
        // arriving Signal with HLC between this event and the prior
        // bumped event could pass the LWW guard and incorrectly mutate
        // state (Cursor High Sev, round-2). The visible-state invariant
        // "repeated same-direction Signals are no-ops" still holds —
        // `conviction_at` queries are unchanged because the support
        // session boundaries don't move.
        self.last_event_ordinal = Some(event_ord);
        if support == self.is_supporting {
            return;
        }
        if support && !self.is_supporting {
            // Decay any prior accumulation up to the moment the new session
            // begins, so subsequent `conviction_at` computations can treat
            // `last_event_at_ms` as the reference point for both the prior
            // pool and the active-session start.
            let dt = event_hlc_ms - self.last_event_at_ms;
            self.accumulated_conviction_q32 =
                decay_q32(self.accumulated_conviction_q32, dt, half_life_ms);
            self.is_supporting = true;
            self.support_started_at_ms = event_hlc_ms;
            self.last_event_at_ms = event_hlc_ms;
        } else if !support && self.is_supporting {
            let session_dur = event_hlc_ms - self.support_started_at_ms;
            // Mirror `conviction_at(is_supporting=true)`: the prior pool
            // has been decaying across the support session while the
            // active charge grew. When the session closes we collapse
            // both into `accumulated_conviction_q32`: decay the prior
            // pool by `session_dur` first, then add the session charge.
            // Without the leading decay, `accumulated += charge` would
            // freeze the prior pool at its session-start value and
            // overstate post-close conviction.
            self.accumulated_conviction_q32 =
                decay_q32(self.accumulated_conviction_q32, session_dur, half_life_ms)
                    + charge_q32(session_dur, half_life_ms);
            self.is_supporting = false;
            self.last_event_at_ms = event_hlc_ms;
        }
        // No-op cases: support==is_supporting (idempotent toggle).
    }

    /// Compute conviction at wall-clock t_ms (Q96.32). Handles both
    /// "currently supporting" (active charge + prior decay) and "not
    /// supporting" (decay of accumulated only) branches per spec §5.
    pub fn conviction_at(&self, t_ms: i128, half_life_ms: i128) -> ConvictionQ32 {
        if self.is_supporting {
            // active charge from the current session
            let session_dur = t_ms - self.support_started_at_ms;
            let active = charge_q32(session_dur, half_life_ms);
            // plus decay of any prior accumulation (which has been
            // decaying since the moment the current session started,
            // i.e., since `last_event_at_ms` which == support_started_at_ms
            // when is_supporting was just toggled on)
            let decayed = decay_q32(
                self.accumulated_conviction_q32,
                t_ms - self.last_event_at_ms,
                half_life_ms,
            );
            decayed + active
        } else {
            decay_q32(
                self.accumulated_conviction_q32,
                t_ms - self.last_event_at_ms,
                half_life_ms,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Tier 2 per-proposal aggregated state
// ---------------------------------------------------------------------------

/// Per-proposal aggregated state for a Tier 2 conviction poll.
///
/// Owns each voter's `VoterConvictionState` keyed by `OwnerAddr`, plus the
/// poll-level `Tier2PollConfig`, snapshotted `total_supply`, and two
/// wall-clock tracking fields used by the 24h-contestability tick (Task 16):
/// `threshold_reached_at_ms` and `last_unsignal_after_threshold_ms`.
///
/// `total_supply` is snapshotted at `PollCreate.hlc` and never mutates — Tier
/// 2's `effective_supply` ratio (used by `threshold_conviction_at`) is the
/// ratio of currently-supporting voters to that snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier2ProposalState {
    pub config: Tier2PollConfig,
    /// Total eligible voters at PollCreate.hlc — snapshotted; never changes
    /// after poll creation (Tier 2's effective_supply derives from this).
    pub total_supply: u32,
    /// Per-voter state, keyed by voter's OwnerAddr.
    pub per_voter: HashMap<OwnerAddr, VoterConvictionState>,
    /// Wall-clock when total_conviction first crossed threshold_conviction.
    /// None if never crossed; reset to None on conviction-drop reversion.
    pub threshold_reached_at_ms: Option<i128>,
    /// Wall-clock of the most recent Unsignal (Signal{false}) event that
    /// arrived AFTER threshold was reached. Used by the tick to compute
    /// "24h uncontested since".
    pub last_unsignal_after_threshold_ms: Option<i128>,
}

impl Tier2ProposalState {
    pub fn new(config: Tier2PollConfig, total_supply: u32) -> Self {
        Self {
            config,
            total_supply,
            per_voter: HashMap::new(),
            threshold_reached_at_ms: None,
            last_unsignal_after_threshold_ms: None,
        }
    }

    /// Sum of all per-voter conviction values at t_ms.
    pub fn total_conviction_at(&self, t_ms: i128) -> ConvictionQ32 {
        let hl_ms = (self.config.half_life_seconds as i128) * 1000;
        self.per_voter
            .values()
            .map(|v| v.conviction_at(t_ms, hl_ms))
            .sum()
    }

    /// Effective supply: count of voters with an active Signal at t_ms.
    ///
    /// Note: the per-voter `is_supporting` flag is the source of truth — it
    /// reflects the most-recent Signal applied. Querying at a `t_ms` earlier
    /// than the last applied event will still report the post-event state
    /// (matching `VoterConvictionState`'s own time-travel limitations).
    pub fn effective_supply_at(&self, _t_ms: i128) -> u32 {
        self.per_voter.values().filter(|v| v.is_supporting).count() as u32
    }

    /// Dynamic threshold conviction at t_ms per spec §5:
    /// `T_min + (T_max - T_min) * (1 - ratio)^β` in Q96.32.
    /// At zero participation: returns `T_max`. At full participation: returns `T_min`.
    /// If `total_supply == 0` (degenerate; should be guarded at PollCreate
    /// time), returns `T_max` so the threshold is never inadvertently low.
    pub fn threshold_conviction_at(&self, t_ms: i128) -> ConvictionQ32 {
        let cfg = &self.config;
        if self.total_supply == 0 {
            return cfg.threshold_max_q32;
        }
        // β = 0 case: mathematically `(1-ratio)^0 = 1` for any ratio, so
        // the threshold is participation-insensitive and equals `T_max`.
        // Without this branch the loop `for _ in 1..0` would be empty,
        // leaving `pow = one_minus = (1-ratio)^1` — silently behaving as
        // β = 1. The IPC config-validator rejects β = 0 at creation time,
        // but a malformed config arriving via the wire would otherwise
        // bypass that; handling it here makes the math defensively correct.
        if cfg.beta == 0 {
            return cfg.threshold_max_q32;
        }
        // `effective_supply_at` can legitimately exceed `total_supply`
        // under spec §5 rolling eligibility (members joining mid-
        // proposal can signal). If we let `ratio_q32 > Q32` flow
        // through, `one_minus` goes negative, and for odd β that drops
        // the threshold below `T_min` — even into negative territory —
        // making the proposal easier to finalize than the config
        // allows. Clamp to `[0, Q32]` so over-100% participation is
        // treated as 100% (already-saturated minimum threshold = T_min).
        // CR R4 Major.
        let ratio_q32 = ((self.effective_supply_at(t_ms) as i128 * Q32)
            / self.total_supply as i128)
            .clamp(0, Q32);
        let one_minus = Q32 - ratio_q32;
        // (1 - ratio)^β via repeated multiplication; β=1 → no iter (pow=one_minus).
        let mut pow = one_minus;
        for _ in 1..cfg.beta {
            pow = (pow * one_minus) >> CONVICTION_FRAC_BITS;
        }
        let span = cfg.threshold_max_q32 - cfg.threshold_min_q32;
        cfg.threshold_min_q32 + ((span * pow) >> CONVICTION_FRAC_BITS)
    }

    /// True if the threshold was JUST crossed at t_ms (and isn't already
    /// in the "reached" state). Takes an explicit `&DelegationGraph` so
    /// the comparison stays delegation-weighted; the production tick
    /// holds the graph by ref alongside the proposal state and passes it
    /// here. (An earlier draft of this helper used the unweighted
    /// `total_conviction_at` and was flagged by Cursor R3: silently
    /// produced wrong answers in the presence of any Delegate edges.)
    pub fn just_crossed_threshold(&self, t_ms: i128, delegation_graph: &DelegationGraph) -> bool {
        self.threshold_reached_at_ms.is_none()
            && self.total_conviction_at_with_delegation(t_ms, delegation_graph)
                >= self.threshold_conviction_at(t_ms)
    }

    /// True if conviction dropped back below threshold at t_ms (from a
    /// previously-reached state). Same `&DelegationGraph` discipline as
    /// `just_crossed_threshold`.
    pub fn just_dropped_below_threshold(
        &self,
        t_ms: i128,
        delegation_graph: &DelegationGraph,
    ) -> bool {
        self.threshold_reached_at_ms.is_some()
            && self.total_conviction_at_with_delegation(t_ms, delegation_graph)
                < self.threshold_conviction_at(t_ms)
    }

    /// Total conviction at t_ms with delegation weights applied.
    ///
    /// Each voter `V` in `per_voter` contributes
    /// `(1 + N) * conviction_at(t_ms)`, where `N` is the count of accounts
    /// delegating to `V` via `delegation_graph` who have NOT themselves
    /// directly signaled on this proposal (override semantics, spec §5).
    ///
    /// **Non-transitive in v1** per spec §5: "voter A delegates to voter B
    /// → B's signaling carries B's own weight + A's weight." Only direct
    /// delegators count; chains do not propagate. If spec §5 is later
    /// amended to make delegation transitive, this is the single function
    /// to change.
    pub fn total_conviction_at_with_delegation(
        &self,
        t_ms: i128,
        delegation_graph: &DelegationGraph,
    ) -> ConvictionQ32 {
        // Honor the per-proposal `delegation_allowed` toggle: when a
        // proposal was created with delegation disabled, ignore the
        // graph entirely and fall back to the unweighted sum. Without
        // this gate, a community-wide DelegationGraph edge would
        // silently fold into the conviction even for proposals that
        // explicitly opted out — Tier 2 proposals would converge on
        // different decisions than the config dictated. (CR R3 Major.)
        if !self.config.delegation_allowed {
            return self.total_conviction_at(t_ms);
        }
        let hl_ms = (self.config.half_life_seconds as i128) * 1000;
        let mut weighted: ConvictionQ32 = 0;
        for (voter, state) in self.per_voter.iter() {
            // Count direct delegators who haven't themselves directly
            // signaled on THIS proposal (override semantics).
            let direct_delegators = delegation_graph
                .delegators_of(*voter)
                .filter(|delegator| !self.per_voter.contains_key(delegator))
                .count() as i128;
            let weight = 1 + direct_delegators;
            let conv = state.conviction_at(t_ms, hl_ms);
            weighted += conv * weight;
        }
        weighted
    }
}

// ---------------------------------------------------------------------------
// Delegation graph CRDT
// ---------------------------------------------------------------------------

/// Failure modes for `DelegationGraph::apply_delegate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationError {
    /// D2 violation: applying this Delegate would create a cycle in the graph.
    Cycle,
    /// HLC-LWW: an existing Delegate event for this delegator has a
    /// newer-or-equal HLC, so the incoming event is stale.
    StaleHlc,
}

/// Full HLC ordinal `(wall_ms, logical)` used for LWW comparisons across
/// the voting subsystem. Matches the shape used by `VotingReplayTracker`
/// and `VoterConvictionState`; lexicographic tuple ordering is the
/// canonical HLC ordering. CR R3 Major: prior `i128 ms-only` shape
/// misordered same-ms different-logical Delegate/Undelegate events.
pub type HlcOrdinal = (u64, u32);

/// Per-community delegation graph CRDT.
///
/// Each delegator has at most one active delegate. Conflicts between
/// concurrent Delegate events for the same delegator resolve by HLC-LWW
/// (latest wins). Undelegate clears the delegator's edge with HLC-LWW too.
///
/// D2 invariant: the graph is acyclic at all times. `apply_delegate`
/// rejects any event that would create a cycle (including the degenerate
/// self-delegation A→A).
///
/// LWW tombstone (`last_hlc`): tracks the HLC of the most recent terminal
/// event for each delegator — Delegate OR Undelegate. Without this an
/// out-of-order Delegate(A→B, hlc=t1) delivered *after* an
/// Undelegate(A, hlc=t2 > t1) would find no live edge to compare against
/// and silently re-install the stale delegation. Consulting `last_hlc`
/// during `apply_delegate` rejects such resurrection by HLC-LWW even when
/// no edge currently exists.
///
/// Both `edges` and `last_hlc` key the HLC component as a full
/// `(wall_ms, logical)` ordinal — same-ms different-logical events are
/// distinguishable, matching `VotingReplayTracker` /
/// `VoterConvictionState` semantics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DelegationGraph {
    /// delegator → (delegate, hlc_ordinal_of_latest_event)
    edges: HashMap<OwnerAddr, (OwnerAddr, HlcOrdinal)>,
    /// delegator → hlc_ordinal of last terminal event (Delegate OR
    /// Undelegate). Survives edge removal so Undelegates leave a tombstone.
    last_hlc: HashMap<OwnerAddr, HlcOrdinal>,
}

impl DelegationGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a `Delegate` event. Returns:
    /// - `Err(Cycle)` if `delegator == delegate` or if the new edge would
    ///   close a cycle through the existing graph.
    /// - `Err(StaleHlc)` if there's already a terminal event for
    ///   `delegator` with an HLC ordinal `>= hlc` (HLC-LWW; ties also
    ///   rejected — first applied wins, which is fine because real HLC
    ///   ordinals are unique-per-actor by the per-device tracker's
    ///   logical-bump rule).
    /// - `Ok(())` on success; the edge is installed.
    pub fn apply_delegate(
        &mut self,
        delegator: OwnerAddr,
        delegate: OwnerAddr,
        hlc: HlcOrdinal,
    ) -> Result<(), DelegationError> {
        // Self-delegation is also a degenerate cycle.
        if delegator == delegate {
            return Err(DelegationError::Cycle);
        }
        // D2: walk delegate's outgoing chain looking for delegator. We must
        // do this BEFORE the HLC-LWW check so a stale-and-cyclic event is
        // diagnosed precisely; the order doesn't change the eventual graph
        // state (both branches reject), but it surfaces the structural
        // problem to callers/tests.
        if self.would_create_cycle(delegator, delegate) {
            return Err(DelegationError::Cycle);
        }
        // HLC-LWW: reject if last terminal event (edge OR tombstone) for
        // this delegator has newer-or-equal HLC. Checking the tombstone
        // (not just the live edge) is the load-bearing part: it blocks
        // resurrection by a late-arriving Delegate whose HLC predates a
        // prior Undelegate.
        if let Some(&last) = self.last_hlc.get(&delegator) {
            if last >= hlc {
                return Err(DelegationError::StaleHlc);
            }
        }
        self.edges.insert(delegator, (delegate, hlc));
        self.last_hlc.insert(delegator, hlc);
        Ok(())
    }

    /// Apply an `Undelegate` event. HLC-LWW: silently skips if the last
    /// terminal event (edge OR prior Undelegate tombstone) has a
    /// newer-or-equal HLC ordinal. The tombstone is recorded
    /// unconditionally on a successful undelegate so subsequent
    /// out-of-order Delegate events with earlier ordinals can still be
    /// rejected.
    pub fn apply_undelegate(&mut self, delegator: OwnerAddr, hlc: HlcOrdinal) {
        if let Some(&last) = self.last_hlc.get(&delegator) {
            if last >= hlc {
                return;
            }
        }
        self.edges.remove(&delegator);
        self.last_hlc.insert(delegator, hlc);
    }

    /// Number of accounts directly delegating to `delegate` (not transitive).
    pub fn delegator_count(&self, delegate: OwnerAddr) -> u32 {
        self.edges.values().filter(|(d, _)| *d == delegate).count() as u32
    }

    /// Current delegate of `who`, or `None` if `who` is not delegating.
    pub fn delegate_of(&self, who: OwnerAddr) -> Option<OwnerAddr> {
        self.edges.get(&who).map(|&(d, _)| d)
    }

    /// Iterate the accounts directly delegating to `delegate`. Helper used
    /// by `Tier2ProposalState::total_conviction_at_with_delegation` so the
    /// `edges` field can stay private.
    pub fn delegators_of(&self, delegate: OwnerAddr) -> impl Iterator<Item = OwnerAddr> + '_ {
        self.edges
            .iter()
            .filter(move |(_, (d, _))| *d == delegate)
            .map(|(delegator, _)| *delegator)
    }

    /// Iterate every live delegation edge as `(delegator, delegate, hlc)`.
    /// Used by `voting_list_delegations` to surface the full graph to the
    /// frontend visualization. Iteration order is the underlying HashMap
    /// order — not deterministic across runs; consumers must not rely on
    /// it for cross-engine convergence (the CRDT itself converges).
    pub fn iter_edges(&self) -> impl Iterator<Item = (OwnerAddr, OwnerAddr, HlcOrdinal)> + '_ {
        self.edges
            .iter()
            .map(|(delegator, &(delegate, hlc))| (*delegator, delegate, hlc))
    }

    /// Walk from `start` through the delegate chain; return true if any
    /// node visited equals `target`. Used by cycle detection in
    /// `apply_delegate`.
    fn would_create_cycle(&self, target: OwnerAddr, start: OwnerAddr) -> bool {
        let mut visited = HashSet::new();
        let mut current = start;
        loop {
            if current == target {
                return true;
            }
            if !visited.insert(current) {
                // Defensive: pre-existing cycle. apply_delegate prevents
                // these from ever being installed, but bail out rather than
                // looping forever if invariants ever get violated.
                return true;
            }
            match self.edges.get(&current) {
                Some(&(next, _)) => current = next,
                None => return false,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── ZEB-295 Phase 6: policy default for Tier 3 privacy_mode ──────────

    /// `CommunityVotingPolicy::default()` must yield `"pu"` so existing
    /// communities (which never set the field) stay on public-ballot
    /// scores until the proposer opts in to ballot-secret per poll.
    #[test]
    fn community_voting_policy_default_tier3_privacy_mode_is_pu() {
        let p = CommunityVotingPolicy::default();
        assert_eq!(p.tier3_privacy_mode_default, "pu");
        assert!(!p.notify_on_delegate_signal);
    }

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
        // Sanity: SetPower has exactly 3 keys: kk/tg/np.
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

    // -------- VoterConvictionState --------

    /// Half-life used across the voter-state tests. 1 second in ms gives
    /// charge(hl, hl) ≈ 721 (plain ms units; see `charge_at_one_half_life`
    /// test for the unit discussion).
    const TEST_HL: i128 = 1_000;

    #[test]
    fn voter_state_default_has_zero_conviction() {
        let s = VoterConvictionState::default();
        // At any t (zero, mid, far future), default state has nothing.
        assert_eq!(s.conviction_at(0, TEST_HL), 0);
        assert_eq!(s.conviction_at(TEST_HL, TEST_HL), 0);
        assert_eq!(s.conviction_at(100 * TEST_HL, TEST_HL), 0);
        // Also: default field values are the spec defaults.
        assert!(!s.is_supporting);
        assert_eq!(s.support_started_at_ms, 0);
        assert_eq!(s.accumulated_conviction_q32, 0);
        assert_eq!(s.last_event_at_ms, 0);
    }

    #[test]
    fn single_support_session_builds_conviction() {
        let mut s = VoterConvictionState::default();
        s.apply_signal(true, 0, 0, TEST_HL);
        // After one half-life of continuous support, conviction equals
        // the closed-form charge(hl, hl).
        let actual = s.conviction_at(TEST_HL, TEST_HL);
        let expected = charge_q32(TEST_HL, TEST_HL);
        assert_eq!(actual, expected);
        // And it is positive.
        assert!(actual > 0);
    }

    #[test]
    fn toggle_off_freezes_accumulated_conviction() {
        let mut s = VoterConvictionState::default();
        s.apply_signal(true, 0, 0, TEST_HL);
        s.apply_signal(false, TEST_HL, 0, TEST_HL);
        // At the moment of toggle-off, conviction == charge(hl, hl).
        let at_off = s.conviction_at(TEST_HL, TEST_HL);
        assert_eq!(at_off, charge_q32(TEST_HL, TEST_HL));
        // One more half-life later, it should have decayed by 0.5.
        let at_decayed = s.conviction_at(2 * TEST_HL, TEST_HL);
        let expected = decay_q32(charge_q32(TEST_HL, TEST_HL), TEST_HL, TEST_HL);
        assert_eq!(at_decayed, expected);
        assert!(at_decayed < at_off);
    }

    #[test]
    fn toggle_off_then_idle_decays() {
        let mut s = VoterConvictionState::default();
        s.apply_signal(true, 0, 0, TEST_HL);
        s.apply_signal(false, TEST_HL, 0, TEST_HL);
        let base = s.conviction_at(TEST_HL, TEST_HL);
        // After 1 idle half-life: ≈ base/2. After 2 idle half-lives: ≈ base/4.
        // After 3 idle half-lives: ≈ base/8. Tolerances scale with the
        // base magnitude (Q32 noise from the integer division in decay).
        let tol = base / 1_000_000 + 1;
        let h1 = s.conviction_at(2 * TEST_HL, TEST_HL);
        let h2 = s.conviction_at(3 * TEST_HL, TEST_HL);
        let h3 = s.conviction_at(4 * TEST_HL, TEST_HL);
        assert!(
            approx_eq(h1, base / 2, tol),
            "h1 = {h1}, want ≈ {}",
            base / 2
        );
        assert!(
            approx_eq(h2, base / 4, tol),
            "h2 = {h2}, want ≈ {}",
            base / 4
        );
        assert!(
            approx_eq(h3, base / 8, tol),
            "h3 = {h3}, want ≈ {}",
            base / 8
        );
    }

    #[test]
    fn toggle_on_after_decay_resumes_from_decayed_base() {
        // support → 1hl → not-support → 1hl → support; query at 2hl.
        let mut s = VoterConvictionState::default();
        s.apply_signal(true, 0, 0, TEST_HL);
        s.apply_signal(false, TEST_HL, 0, TEST_HL);
        s.apply_signal(true, 2 * TEST_HL, 0, TEST_HL);
        // At 2*hl, active session just started (dur=0 → active charge=0);
        // accumulated has been decaying for one full hl from charge(hl,hl).
        let actual = s.conviction_at(2 * TEST_HL, TEST_HL);
        let expected = decay_q32(charge_q32(TEST_HL, TEST_HL), TEST_HL, TEST_HL);
        assert_eq!(actual, expected);
    }

    #[test]
    fn re_toggle_off_accumulates_more() {
        // support → 1hl → off → support → 1hl → off.
        let mut s = VoterConvictionState::default();
        s.apply_signal(true, 0, 0, TEST_HL);
        s.apply_signal(false, TEST_HL, 0, TEST_HL);
        // Immediate restart at same wall_ms; logical+1 keeps the ordinal
        // strictly greater so the LWW guard accepts it.
        s.apply_signal(true, TEST_HL, 1, TEST_HL);
        s.apply_signal(false, 2 * TEST_HL, 0, TEST_HL);
        // Per the decay-before-add fix in `apply_signal`'s unsignal branch,
        // closing session 2 first decays the prior accumulated pool (one
        // charge(hl,hl) earned in session 1) by `session_dur == TEST_HL`
        // — i.e., by one half-life, halving it — then adds session 2's
        // charge(hl,hl). So accumulated ≈ 0.5*charge + charge = 1.5*charge.
        let one_charge = charge_q32(TEST_HL, TEST_HL);
        let expected = decay_q32(one_charge, TEST_HL, TEST_HL) + one_charge;
        assert_eq!(s.accumulated_conviction_q32, expected);
        // And the conviction at the moment of second-off equals that
        // accumulated total (no further time elapsed).
        let at_off = s.conviction_at(2 * TEST_HL, TEST_HL);
        assert_eq!(at_off, expected);
    }

    #[test]
    fn idempotent_repeat_signal_true() {
        // "Idempotent" here means the externally-observable state
        // (is_supporting, support_started_at_ms, accumulated_conviction)
        // is unchanged by a same-direction repeat. The LWW high-water
        // mark `last_event_ordinal` DOES advance — see the Cursor
        // round-2 fix: skipping the bump for "idempotent" events
        // re-opened a hole where a later-arriving earlier-HLC opposite
        // signal could pass the guard and rewind state.
        let mut a = VoterConvictionState::default();
        a.apply_signal(true, 0, 0, TEST_HL);
        let mut b = a.clone();
        b.apply_signal(true, TEST_HL, 0, TEST_HL); // repeat true; observably no-op
        assert_eq!(a.is_supporting, b.is_supporting);
        assert_eq!(a.support_started_at_ms, b.support_started_at_ms);
        assert_eq!(a.accumulated_conviction_q32, b.accumulated_conviction_q32);
        assert_eq!(a.last_event_at_ms, b.last_event_at_ms);
        // The ordinal advances on the repeat — that's the LWW guard
        // doing its job, not a bug.
        assert!(b.last_event_ordinal > a.last_event_ordinal);
        // And conviction queries agree (the observable behavior).
        assert_eq!(
            a.conviction_at(2 * TEST_HL, TEST_HL),
            b.conviction_at(2 * TEST_HL, TEST_HL),
        );
    }

    #[test]
    fn idempotent_repeat_signal_false() {
        // apply_signal(false, ...) when not supporting is observably a
        // no-op for support state; the ordinal still advances (Cursor
        // round-2 fix — see `idempotent_repeat_signal_true`).
        let mut a = VoterConvictionState::default();
        let snapshot = a.clone();
        a.apply_signal(false, TEST_HL, 0, TEST_HL);
        assert_eq!(a.is_supporting, snapshot.is_supporting);
        assert_eq!(
            a.accumulated_conviction_q32,
            snapshot.accumulated_conviction_q32
        );
        assert!(a.last_event_ordinal > snapshot.last_event_ordinal);
        // Also after a complete session, repeated false is still observably no-op.
        let mut s = VoterConvictionState::default();
        s.apply_signal(true, 0, 0, TEST_HL);
        s.apply_signal(false, TEST_HL, 0, TEST_HL);
        let snap2 = s.clone();
        s.apply_signal(false, 2 * TEST_HL, 0, TEST_HL);
        // Observable state unchanged; ordinal advances.
        assert_eq!(s.is_supporting, snap2.is_supporting);
        assert_eq!(
            s.accumulated_conviction_q32,
            snap2.accumulated_conviction_q32
        );
        assert!(s.last_event_ordinal > snap2.last_event_ordinal);
    }

    #[test]
    fn determinism_two_states_identical_event_seq() {
        // Build a deterministic 10-event mixed signal sequence.
        // (true/false toggles with varied gaps; exercises both branches +
        // idempotent no-ops.) The two same-ms events at t=5_000 must have
        // distinct logical ordinals — in production HLCs are monotonic
        // per device, so two state-changing events cannot share an
        // ordinal; the LWW guard in `apply_signal` would otherwise drop
        // the second.
        let seq: [(bool, i128, u32); 10] = [
            (true, 0, 0),
            (true, 50, 0),     // idempotent no-op
            (false, 1_000, 0), // close session 1
            (false, 1_500, 0), // idempotent no-op
            (true, 2_000, 0),  // start session 2 after 1*hl idle
            (false, 2_500, 0), // close session 2
            (true, 4_000, 0),  // start session 3 after 1.5*hl idle
            (false, 5_000, 0), // close session 3
            (true, 5_000, 1),  // start session 4 immediately (logical+1)
            (false, 7_000, 0), // close session 4
        ];
        let mut a = VoterConvictionState::default();
        let mut b = VoterConvictionState::default();
        for (sup, t, l) in &seq {
            a.apply_signal(*sup, *t, *l, TEST_HL);
            b.apply_signal(*sup, *t, *l, TEST_HL);
        }
        // Bit-identical state.
        assert_eq!(a, b);
        // And conviction queries agree.
        for t in &[0i128, 100, 1_000, 2_500, 7_000, 10_000] {
            assert_eq!(a.conviction_at(*t, TEST_HL), b.conviction_at(*t, TEST_HL),);
        }
    }

    #[test]
    fn determinism_conviction_at_repeatable() {
        // Identical inputs to conviction_at produce identical outputs
        // across N=100 iterations. Pure-integer arithmetic makes this
        // trivially true, but pin the invariant.
        let mut s = VoterConvictionState::default();
        s.apply_signal(true, 0, 0, TEST_HL);
        s.apply_signal(false, TEST_HL, 0, TEST_HL);
        s.apply_signal(true, 3 * TEST_HL, 0, TEST_HL);
        let baseline: Vec<i128> = (0..10)
            .map(|i| s.conviction_at(i as i128 * TEST_HL, TEST_HL))
            .collect();
        for _ in 0..100 {
            for (i, expected) in baseline.iter().enumerate() {
                assert_eq!(s.conviction_at(i as i128 * TEST_HL, TEST_HL), *expected);
            }
        }
    }

    #[test]
    fn negative_dt_handled_gracefully() {
        // conviction_at(t) where t < last_event_at_ms must not panic.
        // Not-supporting branch: dt < 0 → decay_q32 returns identity, so
        // result equals accumulated.
        let mut s = VoterConvictionState::default();
        s.apply_signal(true, 1_000, 0, TEST_HL);
        s.apply_signal(false, 2_000, 0, TEST_HL);
        let accumulated = s.accumulated_conviction_q32;
        let result = s.conviction_at(500, TEST_HL); // t < last_event_at_ms
        assert_eq!(result, accumulated);

        // Supporting branch: t < support_started_at_ms gives session_dur < 0
        // (charge_q32 returns 0) and dt < 0 (decay_q32 is identity); so
        // result equals accumulated unchanged.
        let mut s2 = VoterConvictionState::default();
        s2.apply_signal(true, 0, 0, TEST_HL);
        s2.apply_signal(false, TEST_HL, 0, TEST_HL);
        s2.apply_signal(true, 5_000, 0, TEST_HL);
        let result2 = s2.conviction_at(100, TEST_HL); // before everything
                                                      // accumulated at this point is the prior charge after pre-resume
                                                      // decay, plus zero active charge.
        let expected2 = s2.accumulated_conviction_q32;
        assert_eq!(result2, expected2);
    }

    #[test]
    fn negative_event_hlc_dropped_does_not_wrap_lww_guard() {
        // Cursor R6 Medium: a negative i128 `event_hlc_ms` cast through
        // `as u64` would wrap to ~u64::MAX, making a stale event appear
        // newer than any prior ordinal and bypassing the LWW guard.
        // apply_signal must reject negative wall-times before that cast.
        let mut s = VoterConvictionState::default();
        // First, a legitimate Signal at wall_ms=100.
        s.apply_signal(true, 100, 0, TEST_HL);
        let baseline = s.clone();

        // A negative-wall-ms Signal must be a no-op — neither bump the
        // LWW high-water mark nor toggle is_supporting.
        s.apply_signal(false, -1, 0, TEST_HL);
        assert_eq!(s, baseline, "negative event_hlc_ms must not mutate state");

        // And without the guard, the wrapped ordinal (u64::MAX, 0)
        // would have passed the freshness check; a follow-up legitimate
        // event at wall_ms=200 must still be applied (proves the
        // guard didn't poison the ordinal).
        s.apply_signal(false, 200, 0, TEST_HL);
        assert!(!s.is_supporting);
        assert_eq!(s.last_event_ordinal, Some((200, 0)));
    }

    // -------- Tier2ProposalState --------

    fn voter(byte: u8) -> OwnerAddr {
        OwnerAddr([byte; 16])
    }

    /// Sample Tier 2 config with a 1-second half-life (matches `TEST_HL` ms)
    /// and a wide threshold band so per-voter conviction can drive crossings
    /// in a few half-lives.
    fn t2_config(beta: u8, t_min_q32: ConvictionQ32, t_max_q32: ConvictionQ32) -> Tier2PollConfig {
        Tier2PollConfig {
            proposal_text: "Test proposal".into(),
            // 1-second half-life → hl_ms = 1000 = TEST_HL.
            half_life_seconds: 1,
            threshold_min_q32: t_min_q32,
            threshold_max_q32: t_max_q32,
            beta,
            delegation_allowed: true,
            auto_exec: AutoExecAction::None,
            eligibility: Eligibility {
                min_power: 1,
                min_vouching_depth: None,
                sortition_size: None,
            },
        }
    }

    #[test]
    fn tier2_state_new_starts_empty() {
        let cfg = t2_config(2, 0, 100 * Q32);
        let s = Tier2ProposalState::new(cfg.clone(), 10);
        assert_eq!(s.total_supply, 10);
        assert_eq!(s.per_voter.len(), 0);
        assert_eq!(s.threshold_reached_at_ms, None);
        assert_eq!(s.last_unsignal_after_threshold_ms, None);
        assert_eq!(s.config, cfg);
    }

    #[test]
    fn total_conviction_zero_with_no_signals() {
        let s = Tier2ProposalState::new(t2_config(2, 0, 100 * Q32), 10);
        assert_eq!(s.total_conviction_at(0), 0);
        assert_eq!(s.total_conviction_at(1_000_000), 0);
    }

    #[test]
    fn threshold_full_participation_equals_t_min() {
        // 3 voters, total_supply 3, all supporting → ratio = 1 → (1-1)^β = 0
        // → threshold = T_min.
        let mut s = Tier2ProposalState::new(t2_config(2, 7 * Q32, 100 * Q32), 3);
        for i in 0..3u8 {
            let mut v = VoterConvictionState::default();
            v.apply_signal(true, 0, 0, TEST_HL);
            s.per_voter.insert(voter(i + 1), v);
        }
        assert_eq!(s.effective_supply_at(0), 3);
        assert_eq!(s.threshold_conviction_at(0), 7 * Q32);
    }

    #[test]
    fn threshold_zero_participation_equals_t_max() {
        // No active signals → ratio = 0 → (1-0)^β = 1 → threshold = T_max.
        let s = Tier2ProposalState::new(t2_config(2, 7 * Q32, 100 * Q32), 5);
        assert_eq!(s.effective_supply_at(0), 0);
        assert_eq!(s.threshold_conviction_at(0), 100 * Q32);
    }

    #[test]
    fn threshold_over_full_participation_clamps_to_t_min() {
        // Rolling eligibility (spec §5): members joining post-snapshot
        // can signal. effective_supply can therefore exceed
        // total_supply, which would push ratio_q32 above Q32 and make
        // `(1 - ratio)` negative. For odd β that drops the threshold
        // below `T_min` (potentially negative); for even β it stays
        // positive but still doesn't match the spec intent of
        // "saturated minimum threshold = T_min". Clamp ratio to
        // `[0, Q32]` so we treat overfill as 100% — CR R4 Major.
        //
        // total_supply = 2 (snapshot at PollCreate), but 4 voters
        // signal: ratio computes as 2.0 (200%), clamps to 1.0 → T_min.
        let mut s = Tier2ProposalState::new(t2_config(1, 5 * Q32, 100 * Q32), 2);
        for i in 0..4u8 {
            let mut v = VoterConvictionState::default();
            v.apply_signal(true, 0, 0, TEST_HL);
            s.per_voter.insert(voter(i + 1), v);
        }
        assert_eq!(s.effective_supply_at(0), 4);
        // β=1 (odd) would have produced a negative threshold without
        // the clamp; with the clamp it equals T_min.
        assert_eq!(s.threshold_conviction_at(0), 5 * Q32);

        // Same scenario with β=3 (also odd, would also have driven
        // negative): clamps to T_min.
        let mut s3 = Tier2ProposalState::new(t2_config(3, 5 * Q32, 100 * Q32), 2);
        for i in 0..4u8 {
            let mut v = VoterConvictionState::default();
            v.apply_signal(true, 0, 0, TEST_HL);
            s3.per_voter.insert(voter(i + 1), v);
        }
        assert_eq!(s3.threshold_conviction_at(0), 5 * Q32);
    }

    #[test]
    fn threshold_beta_zero_always_returns_t_max() {
        // β = 0 mathematically gives `(1-ratio)^0 = 1` regardless of
        // ratio, so the threshold must equal `T_max` at every
        // participation level. Without the `cfg.beta == 0` special-case
        // the `for _ in 1..0` loop is empty and `pow` stays at
        // `one_minus`, silently behaving as β = 1 — Cursor round-2 Low
        // Severity. IPC-layer validation rejects β = 0 at creation, but
        // a malformed config arriving via the wire would still hit this
        // codepath, so the defensive branch lives in the math.
        let mut s = Tier2ProposalState::new(t2_config(0, 7 * Q32, 100 * Q32), 4);
        // No participation → T_max.
        assert_eq!(s.threshold_conviction_at(0), 100 * Q32);
        // Half participation → still T_max (not the β=1-equivalent value).
        for i in 0..2u8 {
            let mut v = VoterConvictionState::default();
            v.apply_signal(true, 0, 0, TEST_HL);
            s.per_voter.insert(voter(i + 1), v);
        }
        assert_eq!(s.effective_supply_at(0), 2);
        assert_eq!(s.threshold_conviction_at(0), 100 * Q32);
        // Full participation → still T_max.
        for i in 2..4u8 {
            let mut v = VoterConvictionState::default();
            v.apply_signal(true, 0, 0, TEST_HL);
            s.per_voter.insert(voter(i + 1), v);
        }
        assert_eq!(s.effective_supply_at(0), 4);
        assert_eq!(s.threshold_conviction_at(0), 100 * Q32);
    }

    #[test]
    fn threshold_curve_beta_2_mid_participation() {
        // ratio = 0.5, β = 2: pow = 0.25, threshold = T_min + 0.25*(T_max-T_min).
        let mut s = Tier2ProposalState::new(t2_config(2, 0, 100 * Q32), 4);
        for i in 0..2u8 {
            let mut v = VoterConvictionState::default();
            v.apply_signal(true, 0, 0, TEST_HL);
            s.per_voter.insert(voter(i + 1), v);
        }
        // Two non-supporting voters fill out the total_supply count.
        for i in 2..4u8 {
            s.per_voter
                .insert(voter(i + 1), VoterConvictionState::default());
        }
        assert_eq!(s.effective_supply_at(0), 2);
        assert_eq!(s.total_supply, 4);
        // Expected: 0 + 0.25 * (100 - 0) * Q32 = 25 * Q32.
        assert_eq!(s.threshold_conviction_at(0), 25 * Q32);
    }

    #[test]
    fn threshold_curve_beta_1_mid_participation() {
        // β=1: pow = (1 - ratio); ratio = 0.5 → pow = 0.5 → threshold = T_min + 0.5*(T_max - T_min) = 50.
        let mut s = Tier2ProposalState::new(t2_config(1, 0, 100 * Q32), 4);
        for i in 0..2u8 {
            let mut v = VoterConvictionState::default();
            v.apply_signal(true, 0, 0, TEST_HL);
            s.per_voter.insert(voter(i + 1), v);
        }
        for i in 2..4u8 {
            s.per_voter
                .insert(voter(i + 1), VoterConvictionState::default());
        }
        assert_eq!(s.threshold_conviction_at(0), 50 * Q32);
    }

    #[test]
    fn threshold_zero_total_supply_returns_t_max() {
        // Degenerate guard: avoid div-by-zero, return the safe (high) ceiling.
        let s = Tier2ProposalState::new(t2_config(2, 5 * Q32, 42 * Q32), 0);
        assert_eq!(s.threshold_conviction_at(0), 42 * Q32);
    }

    #[test]
    fn just_crossed_threshold_true_when_total_meets_threshold_first_time() {
        // Construct a state where total_conviction crosses threshold at a
        // specific t. With T_min=0, T_max=very high, and a single voter
        // signaling for an hour, total grows monotonically; eventually it
        // crosses the (full-participation) T_min = 0 — but T_min=0 makes
        // the test trivial. Use a non-zero T_min so the crossing is real.
        // total_supply=1, single voter → ratio=1 → threshold = T_min.
        // Pick T_min such that crossing happens after exactly 1 half-life.
        let target = charge_q32(TEST_HL, TEST_HL); // conviction after 1 hl
        let mut s = Tier2ProposalState::new(t2_config(2, target, 100 * Q32), 1);
        let mut v = VoterConvictionState::default();
        v.apply_signal(true, 0, 0, TEST_HL);
        s.per_voter.insert(voter(1), v);

        let empty_graph = DelegationGraph::new();
        // Just before 1 half-life: not crossed.
        assert!(s.total_conviction_at(TEST_HL - 1) < s.threshold_conviction_at(TEST_HL - 1));
        assert!(!s.just_crossed_threshold(TEST_HL - 1, &empty_graph));

        // At 1 half-life: total == threshold → crossed.
        assert!(s.just_crossed_threshold(TEST_HL, &empty_graph));

        // Once "reached" is recorded, subsequent calls return false.
        s.threshold_reached_at_ms = Some(TEST_HL);
        assert!(!s.just_crossed_threshold(2 * TEST_HL, &empty_graph));
    }

    #[test]
    fn just_dropped_below_threshold_only_after_reached() {
        let mut s = Tier2ProposalState::new(t2_config(2, 100 * Q32, 100 * Q32), 1);
        let empty_graph = DelegationGraph::new();
        // Never reached: just_dropped is always false.
        assert!(!s.just_dropped_below_threshold(0, &empty_graph));
        assert!(!s.just_dropped_below_threshold(10_000, &empty_graph));

        // Mark reached, ensure total_conviction is below threshold (no
        // signals → 0; threshold T_min=T_max=100 * Q32). just_dropped true.
        s.threshold_reached_at_ms = Some(0);
        assert!(s.just_dropped_below_threshold(1_000, &empty_graph));
    }

    // -------- DelegationGraph --------

    #[test]
    fn delegation_simple_a_to_b_accepted() {
        let mut g = DelegationGraph::new();
        let a = voter(1);
        let b = voter(2);
        assert_eq!(g.apply_delegate(a, b, (1_000, 0)), Ok(()));
        assert_eq!(g.delegate_of(a), Some(b));
        assert_eq!(g.delegate_of(b), None);
        assert_eq!(g.delegator_count(b), 1);
        assert_eq!(g.delegator_count(a), 0);
    }

    #[test]
    fn delegation_cycle_a_to_b_to_a_rejected() {
        let mut g = DelegationGraph::new();
        let a = voter(1);
        let b = voter(2);
        g.apply_delegate(a, b, (1_000, 0)).unwrap();
        // Now trying B→A would close a 2-cycle.
        assert_eq!(
            g.apply_delegate(b, a, (2_000, 0)),
            Err(DelegationError::Cycle)
        );
        // Graph state unchanged.
        assert_eq!(g.delegate_of(b), None);
        assert_eq!(g.delegate_of(a), Some(b));
    }

    #[test]
    fn delegation_transitive_cycle_a_to_b_to_c_to_a_rejected() {
        let mut g = DelegationGraph::new();
        let a = voter(1);
        let b = voter(2);
        let c = voter(3);
        g.apply_delegate(a, b, (1_000, 0)).unwrap();
        g.apply_delegate(b, c, (2_000, 0)).unwrap();
        // Now C→A would close the 3-cycle A→B→C→A.
        assert_eq!(
            g.apply_delegate(c, a, (3_000, 0)),
            Err(DelegationError::Cycle)
        );
        assert_eq!(g.delegate_of(c), None);
    }

    #[test]
    fn delegation_self_delegation_rejected() {
        let mut g = DelegationGraph::new();
        let a = voter(1);
        assert_eq!(
            g.apply_delegate(a, a, (1_000, 0)),
            Err(DelegationError::Cycle)
        );
        assert_eq!(g.delegate_of(a), None);
    }

    #[test]
    fn delegation_hlc_lww_newer_replaces_older() {
        let mut g = DelegationGraph::new();
        let a = voter(1);
        let b = voter(2);
        let c = voter(3);
        g.apply_delegate(a, b, (1_000, 0)).unwrap();
        // Newer Delegate from A→C wins.
        g.apply_delegate(a, c, (2_000, 0)).unwrap();
        assert_eq!(g.delegate_of(a), Some(c));
        assert_eq!(g.delegator_count(b), 0);
        assert_eq!(g.delegator_count(c), 1);
    }

    #[test]
    fn delegation_hlc_lww_older_rejected_as_stale() {
        let mut g = DelegationGraph::new();
        let a = voter(1);
        let b = voter(2);
        let c = voter(3);
        g.apply_delegate(a, b, (2_000, 0)).unwrap();
        // Older Delegate from A→C is stale.
        assert_eq!(
            g.apply_delegate(a, c, (1_000, 0)),
            Err(DelegationError::StaleHlc),
        );
        // Equal HLC is also rejected (ties → first wins).
        assert_eq!(
            g.apply_delegate(a, c, (2_000, 0)),
            Err(DelegationError::StaleHlc),
        );
        assert_eq!(g.delegate_of(a), Some(b));
    }

    #[test]
    fn undelegate_clears_edge_when_newer() {
        let mut g = DelegationGraph::new();
        let a = voter(1);
        let b = voter(2);
        g.apply_delegate(a, b, (1_000, 0)).unwrap();
        g.apply_undelegate(a, (2_000, 0));
        assert_eq!(g.delegate_of(a), None);
        assert_eq!(g.delegator_count(b), 0);
    }

    #[test]
    fn undelegate_no_op_when_older() {
        let mut g = DelegationGraph::new();
        let a = voter(1);
        let b = voter(2);
        g.apply_delegate(a, b, (2_000, 0)).unwrap();
        // Stale Undelegate: should not remove the edge.
        g.apply_undelegate(a, (1_000, 0));
        assert_eq!(g.delegate_of(a), Some(b));
        // Equal HLC also no-op (LWW: ≥ existing, so skip).
        g.apply_undelegate(a, (2_000, 0));
        assert_eq!(g.delegate_of(a), Some(b));
    }

    #[test]
    fn undelegate_idempotent_when_no_edge() {
        let mut g = DelegationGraph::new();
        let a = voter(1);
        g.apply_undelegate(a, (1_000, 0)); // no edge to clear; just no-op
        assert_eq!(g.delegate_of(a), None);
    }

    #[test]
    fn delegator_count_basic() {
        let mut g = DelegationGraph::new();
        let a = voter(1);
        let b = voter(2);
        let c = voter(3);
        let d = voter(4);
        g.apply_delegate(a, d, (1_000, 0)).unwrap();
        g.apply_delegate(b, d, (1_001, 0)).unwrap();
        g.apply_delegate(c, d, (1_002, 0)).unwrap();
        assert_eq!(g.delegator_count(d), 3);
        assert_eq!(g.delegator_count(a), 0);
        // Re-delegate one away.
        let e = voter(5);
        g.apply_delegate(b, e, (2_000, 0)).unwrap();
        assert_eq!(g.delegator_count(d), 2);
        assert_eq!(g.delegator_count(e), 1);
    }

    // -------- Delegation-weighted conviction --------

    #[test]
    fn weighted_conviction_no_delegation_equals_simple_total() {
        // Sanity: with an empty graph, weighted == simple total.
        let mut s = Tier2ProposalState::new(t2_config(2, 0, 100 * Q32), 2);
        let mut v1 = VoterConvictionState::default();
        v1.apply_signal(true, 0, 0, TEST_HL);
        s.per_voter.insert(voter(1), v1);
        let g = DelegationGraph::new();
        assert_eq!(
            s.total_conviction_at_with_delegation(TEST_HL, &g),
            s.total_conviction_at(TEST_HL),
        );
    }

    #[test]
    fn weighted_conviction_counts_delegator_weight() {
        // A→B; only B signals. B's conviction should be doubled (weight = 1 + 1).
        let mut s = Tier2ProposalState::new(t2_config(2, 0, 100 * Q32), 2);
        let a = voter(1);
        let b = voter(2);
        let mut vb = VoterConvictionState::default();
        vb.apply_signal(true, 0, 0, TEST_HL);
        s.per_voter.insert(b, vb);
        let mut g = DelegationGraph::new();
        g.apply_delegate(a, b, (1, 0)).unwrap();

        let simple = s.total_conviction_at(TEST_HL);
        let weighted = s.total_conviction_at_with_delegation(TEST_HL, &g);
        assert_eq!(weighted, 2 * simple);
    }

    #[test]
    fn delegation_disabled_falls_back_to_unweighted() {
        // Same A→B; only B signals; but the proposal config has
        // delegation_allowed=false. The weighted variant must ignore the
        // graph and return the unweighted sum (CR R3 Major).
        let mut cfg = t2_config(2, 0, 100 * Q32);
        cfg.delegation_allowed = false;
        let mut s = Tier2ProposalState::new(cfg, 2);
        let a = voter(1);
        let b = voter(2);
        let mut vb = VoterConvictionState::default();
        vb.apply_signal(true, 0, 0, TEST_HL);
        s.per_voter.insert(b, vb);
        let mut g = DelegationGraph::new();
        g.apply_delegate(a, b, (1, 0)).unwrap();

        let simple = s.total_conviction_at(TEST_HL);
        let weighted = s.total_conviction_at_with_delegation(TEST_HL, &g);
        assert_eq!(weighted, simple);
    }

    #[test]
    fn direct_signal_overrides_delegation_for_that_proposal() {
        // A→B; BOTH A and B signal directly on this proposal. Override
        // semantics: A's direct signal beats their delegation; B does NOT
        // get A's weight. So total = conviction(A) + conviction(B).
        let mut s = Tier2ProposalState::new(t2_config(2, 0, 100 * Q32), 2);
        let a = voter(1);
        let b = voter(2);
        let mut va = VoterConvictionState::default();
        va.apply_signal(true, 0, 0, TEST_HL);
        s.per_voter.insert(a, va);
        let mut vb = VoterConvictionState::default();
        vb.apply_signal(true, 0, 0, TEST_HL);
        s.per_voter.insert(b, vb);
        let mut g = DelegationGraph::new();
        g.apply_delegate(a, b, (1, 0)).unwrap();

        let weighted = s.total_conviction_at_with_delegation(TEST_HL, &g);
        // Both at weight 1 (A overrides; B has no eligible delegators).
        let single = charge_q32(TEST_HL, TEST_HL);
        assert_eq!(weighted, 2 * single);
    }

    #[test]
    fn multi_level_delegation_does_not_double_count() {
        // A→B and C→A. Neither A nor C has a direct signal; B signals.
        //
        // SPEC §5 (v1, "universal scope only"): delegation is
        // NON-TRANSITIVE — "voter A delegates to voter B → B's signaling
        // carries B's own weight + A's weight." Only A's vote moves to B;
        // C's delegation to A does NOT cascade to B in v1.
        //
        // Therefore: B's weight = 1 (self) + 1 (A) = 2; C's delegation to
        // A contributes nothing because A doesn't signal. Total weight = 2.
        let mut s = Tier2ProposalState::new(t2_config(2, 0, 100 * Q32), 3);
        let a = voter(1);
        let b = voter(2);
        let c = voter(3);
        let mut vb = VoterConvictionState::default();
        vb.apply_signal(true, 0, 0, TEST_HL);
        s.per_voter.insert(b, vb);
        let mut g = DelegationGraph::new();
        g.apply_delegate(a, b, (1, 0)).unwrap();
        g.apply_delegate(c, a, (2, 0)).unwrap();

        let weighted = s.total_conviction_at_with_delegation(TEST_HL, &g);
        let single = charge_q32(TEST_HL, TEST_HL);
        assert_eq!(weighted, 2 * single);
    }

    #[test]
    fn total_conviction_with_delegation_override_for_one_proposal_only() {
        // Phase 3 UI scenario: voter A delegates to B (community-wide).
        // On proposal P1, A signals directly — A's weight is counted via
        // A's direct state, NOT folded into B's effective conviction
        // (override semantics). On proposal P2, A does not signal — A's
        // weight folds into B (delegate's normal behavior). Regression
        // pin: prevents future refactors from silently breaking
        // `.filter(|delegator| !self.per_voter.contains_key(delegator))`
        // in `total_conviction_at_with_delegation`.
        let delegator = OwnerAddr([0xa1; 16]);
        let delegate = OwnerAddr([0xb2; 16]);
        let mut graph = DelegationGraph::new();
        graph
            .apply_delegate(delegator, delegate, (1, 0))
            .expect("delegate edge applies");

        let cfg = t2_config(2, 0, 100 * Q32);
        let per_voter_charge = charge_q32(TEST_HL, TEST_HL);

        // P1: B signals, A also signals directly (override).
        let mut p1 = Tier2ProposalState::new(cfg.clone(), 10);
        let mut a_state = VoterConvictionState::default();
        a_state.apply_signal(true, 0, 0, TEST_HL);
        let mut b_state = VoterConvictionState::default();
        b_state.apply_signal(true, 0, 0, TEST_HL);
        p1.per_voter.insert(delegator, a_state);
        p1.per_voter.insert(delegate, b_state.clone());
        let p1_total = p1.total_conviction_at_with_delegation(TEST_HL, &graph);
        assert_eq!(
            p1_total,
            per_voter_charge * 2,
            "P1: A direct + B direct, A's delegation override prevents double-count"
        );

        // P2: only B signals; A's weight folds into B via delegation.
        let mut p2 = Tier2ProposalState::new(cfg, 10);
        p2.per_voter.insert(delegate, b_state);
        let p2_total = p2.total_conviction_at_with_delegation(TEST_HL, &graph);
        assert_eq!(
            p2_total,
            per_voter_charge * 2,
            "P2: B's conviction weighted by (1 + 1 delegator)"
        );
    }
}
