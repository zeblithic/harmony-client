# ZEB-289 — Voting / Polling Umbrella Design

**Linear:** [ZEB-289](https://linear.app/zeblith/issue/ZEB-289) (parent epic — this spec produces the sub-ticket tree)
**Branch:** `zeb-289-voting-umbrella-design`
**Date:** 2026-05-16
**Status:** Umbrella spec — phased implementation via sub-tickets (see §15).

## Summary

A decentralized voting / polling primitive for Harmony communities — distinct from the *vertical* admin governance surface ([ZEB-217](https://linear.app/zeblith/issue/ZEB-217) power levels, [ZEB-250](https://linear.app/zeblith/issue/ZEB-250) admin quorum) — built around three stakes-tiered mechanisms (Approval / Conviction / Sortition+Pol.is+STAR) that share a common signing / eligibility / audit core. Tier 3 supports two privacy modes (ballot-secret via D-FROST, receipt-free via TRIP at civic infrastructure). Each community is sovereign over its tier defaults; per-poll override is allowed.

The architecture follows the existing harmony-client domain-factoring pattern (`community_membership.rs`, `community_channel_log.rs`, `community_fork.rs`): a shared `voting_core.rs` module owns infrastructure; one module per tier owns mechanism-specific logic; voting events live in a new per-community signed-event log (`community_voting_log.rs`) parallel to channel-config.

## Research foundation

This spec builds on two Gemini research reports synthesized prior to brainstorming:

- **Report 1 (cryptographic protocols)** — established that MACI-class receipt-freeness is fundamentally unachievable in pure CRDT (revote conflict-resolution leaks observable metadata to coercers). Identified three viable substitute paths for receipt-freeness: (a) TRIP/Votegral kiosks + D-FROST tallying; (b) VDF time-locks + Traceable Ring Signatures; (c) Selections-style panic passwords + TEE-FHE. We adopt path (a) because the Harmony civic-infrastructure pattern (libraries as federated trust anchors) makes physical kiosks genuinely viable.
- **Report 2 (mechanism design)** — surveyed voting mechanisms across 1-person-1-vote contexts. Recommended tier-by-stakes architecture: Approval (low) / Conviction (medium continuous) / Sortition+Pol.is+STAR (high constitutional). Established that ballot entropy directly trades off with coercion resistance (high-entropy ballots like ranked or QV are inherently vote-buying-vulnerable).

## 1. Goals & Non-goals

### Goals (priority order)

1. **Verifiable.** Voters confirm their ballot was counted; observers reproduce the tally from the signed-event log; results are bit-identical across nodes given the same input set (deterministic materialize).
2. **Decentralized.** No central tally server; ballots aggregate via CRDT convergence under HLC ordering. Compatible with Iroh transport + CBOR event logs.
3. **Tier-by-stakes.** Each tier optimized for its threat model + UX context.
4. **Configurable privacy for Tier 3.** Each Tier 3 poll picks ballot-secret (D-FROST threshold decryption committee) OR receipt-free (TRIP via civic infrastructure) at poll-create time.
5. **Fixed eligibility schema.** Per poll: `min_power: u8` + optional `min_vouching_depth: u8` (+ `sortition_size: u16` for Tier 3).
6. **Polycentric.** Each community is sovereign over tier defaults + per-poll overrides. No platform-level admin intervention.

### Non-goals (v1)

- **Token-weighted voting** — no token, no treasury.
- **Smart-contract dependency** — Harmony has no EVM.
- **MACI-class receipt-freeness for Tier 1/2** — fundamentally impossible in pure CRDT (Report 1).
- **Cross-community voting** — federation-level coordination deferred to a future umbrella.
- **Financial / monetary decisions** — Harmony has no economic primitive.
- **Auto-execution of results** beyond a tightly-scoped action set (see §5 + §7). Results are *signals*; downstream systems (admin actions, community bots, IFTTT layers, manual moderator workflows) consume them however the community decides.

### Use cases

| # | Use case | Primary tier | Notes |
|---|----------|--------------|-------|
| 1 | Channel proposals (create / rename / delete) | Tier 1 Approval | Member-vote alternative to admin fiat |
| 2 | Resource allocation (pin / feature content) | Tier 1 Approval | Ad-hoc, usually no `min_power` |
| 3 | Moderation policy / CoC amendments | Tier 2 Conviction | Continuous norm evolution |
| 4 | Trust delegations (vouching, reputation signals) | Tier 2 Conviction | Pairs with continuous-signal model |
| 5 | Periodic moderator elections | Tier 2 (small) / Tier 3 (large) | Community-policy choice |
| 6 | Fork referendums | Tier 3 Sortition+Pol.is+STAR | Interfaces with [ZEB-285](https://linear.app/zeblith/issue/ZEB-285) |

## 2. Architecture overview

### Module structure

```
src-tauri/src/
├── community_membership.rs          [existing — admin governance]
├── community_state_crdt.rs          [existing — per-community state]
├── community_channel_log.rs         [existing — channel-config CRDT]
├── community_fork.rs                [existing — fork primitive]
│
├── community_voting_core.rs         [NEW — shared types + lifecycle + IPC dispatcher]
├── community_voting_log.rs          [NEW — per-community signed-event log + sync]
├── community_voting_approval.rs     [NEW — Tier 1]
├── community_voting_conviction.rs   [NEW — Tier 2]
└── community_voting_sortition.rs    [NEW — Tier 3 (sortition + Pol.is + STAR)]
```

### `community_voting_core.rs` responsibilities

- **Types:**
  - `PollId([u8; 32])` — `H(community_id || poll_create_event_hash)`.
  - `Tier` enum — `Approval = 1`, `Conviction = 2`, `Sortition = 3`.
  - `Eligibility { min_power: u8, min_vouching_depth: Option<u8>, sortition_size: Option<u16> }`.
  - `PollMeta { poll_id, tier, eligibility, lifecycle_state, created_at, opens_at, closes_at, extends_at }`.
- **Signed-event envelope** (see §3) and per-tier payload dispatch.
- **Eligibility verifier** — single shared implementation; given a `SignedEvent` + community membership/vouching snapshot, returns `Ok` iff signer meets predicate.
- **Lifecycle state machine** — `Draft → Open → Closed → Finalized → Archived`. Archived after 90 days (per-ballot events dropped; `PollMeta` + `PollResult` retained).
- **IPC dispatcher** — routes `voting_*` IPC commands to per-tier handlers; generic commands (`list_polls`, `get_poll`, `cancel_draft`) live here.
- **Audit log** — `explain_tally(poll_id) -> AuditTrail` returns the deterministic event sequence + materialize trace, so any node reproduces the tally bit-identically.

### Per-tier module trait

```rust
pub trait VotingTier {
    const TIER: Tier;
    fn validate_poll_config(bytes: &[u8]) -> Result<(), VotingError>;
    fn validate_ballot(bytes: &[u8], poll: &PollMeta) -> Result<(), VotingError>;
    fn materialize(events: &[SignedPollEvent]) -> TallyState;
    fn finalize(state: &TallyState, poll: &PollMeta) -> PollResult;
    fn ipc_handlers() -> Vec<IpcHandler>;
}
```

Each tier module registers with `voting_core` at startup. Unimplemented tiers stay unregistered; incoming events for unknown tiers fail-soft with `UnknownTier` (see §8 V3).

### CRDT placement

New per-community voting event log (`community_voting_log.rs`) parallel to `community_channel_log`. Zenoh topic: `harmony/community/{id}/voting`. HLC-ordered events; per-community materializer maintains `HashMap<PollId, PollState>`. Stops materializing on `Archived`.

### Poll lifecycle

```
[no event]
    │ create_poll IPC
    ▼
  Draft  ──── publish ────▶  Open
                                │ window expires OR
                                │ explicit close event
                                ▼
                             Closed  ─── tally finalizes ───▶  Finalized
                                                                    │
                                                                    │ +90 days
                                                                    ▼
                                                                Archived
```

## 3. Wire format

All voting events use a common CBOR envelope. Same-length-keys invariant: at any nesting level, all map keys share a length (2-char throughout for voting).

### Envelope (top-level CBOR map)

```text
{
  "tg": "p",                          # event family tag — "p" for poll/voting
  "vr": 1,                            # schema version
  "tr": 1 | 2 | 3,                    # tier
  "kd": "cr"|"op"|"xt"|"cl"|"bl"|"rs" # event kind
        |"sg"|"dg"|"ud"               #   Tier 2 specifics
        |"ss"|"ds"|"dv"|"dc"|"rb"|"ts" # Tier 3 specifics
  "hc": <hlc-bytes>,
  "ac": <ed25519-pubkey>,
  "pd": <tier+kind-specific CBOR bytes>,
  "sg": <ed25519-sig>                 # signs canonical CBOR of all fields except "sg"
}
```

### Event kinds (`kd`)

| Tier | Kind | Meaning |
|---|---|---|
| any | `cr` | PollCreate |
| any | `op` | PollOpen |
| any | `xt` | PollExtend |
| any | `cl` | PollClose |
| any | `bl` | BallotCast (Tier 1) |
| any | `rs` | PollResult |
| 2 | `sg` | Signal |
| 2 | `dg` | Delegate |
| 2 | `ud` | Undelegate |
| 3 | `ss` | SortitionSelection |
| 3 | `ds` | DeliberationStatement |
| 3 | `dv` | DeliberationVote |
| 3 | `dc` | DraftCandidate |
| 3 | `rb` | RatificationBallot |
| 3 | `ts` | TallyShare (D-FROST partial decrypt) |

Inner `pd` payloads use their own 2-char keys (per-tier subsections below).

## 4. Tier 1 — Approval Voting

Chat-native, low-stakes mechanism. UX target: feels like a Discord/Slack reaction poll. Approval (rather than Plurality) eliminates the spoiler effect at near-zero cognitive cost.

### Mechanism

Each ballot is a subset of the poll's options. Tally is the per-option sum of approvals. Winner is the option with maximum count. Optional quorum, optional supermajority threshold, optional multi-winner (top-N).

### PollConfig payload (`pd` for `kd="cr"`, Tier 1)

```text
{
  "o":  ["Pizza", "Burgers", "Sushi"],  # options (2-20; label length ≤ 80 chars)
  "w":  3600,                            # window seconds (60 ≤ w ≤ 2_592_000 = 30d)
  "q":  5,                               # optional: min quorum
  "th": 50,                              # optional: min threshold percent (0-100)
  "mw": 1                                # optional: multi-winner top-N (default 1)
}
```

### Ballot payload (`pd` for `kd="bl"`, Tier 1)

```text
{
  "ap": [0, 2]   # approved option indices (deduped, sorted ascending)
}
```

Constraints in `validate_ballot`: indices in range, list non-empty, list not equal to "approve everything" (rejected as abstention).

### Lifecycle

Chat-native, no draft phase. `PollCreate` opens the poll immediately. Auto-closes at `PollCreate.hlc + window_seconds` — any node signs `PollClose` when its HLC passes the window. `PollResult` published after materialize finalizes (reproducible, signable by anyone).

### Tally algorithm

```text
1. Collect all BallotCast events for poll_id, ordered by HLC.
2. Per (voter, poll_id): keep only the latest ballot (HLC last-write-wins).
3. Reject ballots whose voter fails eligibility at PollCreate.hlc snapshot.
4. For each remaining ballot, increment counts[ap[i]] for each i in ap.
5. If `q` set and len(ballots) < q: result = NoQuorum.
6. Sort options by count descending; tie-break by option index ascending.
7. winners = top mw.unwrap_or(1) options.
8. If `th` set and `winners[mw-1].count * 100 / len(ballots) < th`: result = NoMajority. (Use the count-side multiplication so integer truncation rounds against the winner, never against the threshold — `50 * 3 / 100 = 1`, which would let a 1-of-3 win declare "≥50%" under the naive form.)
9. Else: result = Winners(winners).
```

### Conflict resolution

HLC last-write-wins per `(voter, poll_id)`. Voters can re-vote during the open window; only the latest ballot counts. Earlier ballots remain in the log but are silently superseded by materialize.

**Note:** re-vote is *visible* in the log. For Tier 1 public polls this is fine (low-stakes social decisions). It becomes a critical vulnerability for Tier 3 receipt-free mode — and is exactly what TRIP-style fake credentials solve in that tier. The CRDT-revote-leak issue is deliberately confined to tiers where it doesn't matter.

### Eligibility

Snapshot at `PollCreate.hlc`. Members eligible at that instant can vote for the duration of the window; late-joiners can't vote on this poll.

### UI

Chat-native. Poll appears as an embedded message in any channel where the author has post permission. Live tally bars; voter's selections highlighted; window countdown; auto-result on close. The embedded poll card persists in the channel's message history.

### IPC commands

```text
voting_create_tier1_poll(
    channel_id: ChannelId,
    options: Vec<String>,
    window_seconds: u32,
    eligibility: Eligibility,
    quorum: Option<u32>,
    threshold_percent: Option<u8>,
    multi_winner: Option<u8>,
) -> Result<PollId, VotingError>

voting_cast_tier1_ballot(poll_id: PollId, approved_indices: Vec<u8>) -> Result<()>
voting_list_active_polls(community_id: SpaceId) -> Vec<PollMeta>
voting_get_poll(poll_id: PollId) -> PollState   # includes live tally
```

### Tauri events

- `voting-poll-created` — poll_id, channel_id, tier, question.
- `voting-ballot-cast` — poll_id, voter, approved_count.
- `voting-poll-closed` — poll_id, result.

### Auto-exec actions for Tier 1

**Always `none`.** Tier 1 polls produce signals only; admin enacts manually (e.g., proposer creates the winning channel after the poll succeeds).

## 5. Tier 2 — Conviction Voting

> **Amendment note (ZEB-291, 2026-05-17).**
> The `Conviction compute (deterministic materialize)` and `Dynamic threshold` subsections below have been rewritten from the original f64 floating-point pseudocode to **fixed-point i128 Q96.32** equivalents. The change is required by ZEB-291 acceptance criterion #2 (two engines on different architectures must materialize bit-identical conviction state from the same event log). IEEE 754 f64 cannot guarantee this — fused-multiply-add reordering, subnormal handling, and reciprocal-table approximations diverge between x86 SSE/AVX and ARM NEON. See the **Why fixed-point i128, not f64?** rationale at the end of this section.
>
> Constants introduced by this amendment:
> - `LN2_Q32 = 2_977_044_472` — `ln(2) * 2^32`, rounded up from the exact value `2_977_044_471.5340…`.
> - `CONVICTION_FRAC_BITS = 32`.
> - `Q32 = 1 << 32 = 4_294_967_296`.
> - Conviction values are stored as `i128` in Q96.32 fixed-point (96 integer bits, 32 fractional bits) with an implicit `/ 2^32` interpretation factor.
>
> The Tier 2 PollConfig key `b` (β exponent) has also been renamed to `bb` to satisfy the §3 same-length-keys invariant (2-char throughout). All other Tier 2 wire-format keys and the Phase 1 / Tier 1 wire format are unchanged.

Continuous-signaling, medium-stakes mechanism. Aragon / 1Hive lineage adapted for 1p1v non-monetary context. Replaces dramatic "election day" events with an always-on signaling layer.

### Mechanism

Each voter has unit weight (1p1v); signal is binary (supporting / not supporting a proposal); conviction grows continuously while supporting, decays continuously while not. Proposal passes when total conviction exceeds dynamic threshold for one HLC tick.

### PollConfig payload (`pd` for `kd="cr"`, Tier 2)

```text
{
  "pt": "Promote @alice to moderator (power 50)",   # proposal text
  "hl": 604800,                # half-life seconds (u32; default 7 days; community-policy default)
  "tn": "214748365",           # T_min as i128 Q32 (= 0.05 * 2^32); CBOR encodes as integer,
                               #   JSON IPC encodes as decimal string to avoid f64 round-trip
  "tx": "2147483648",          # T_max as i128 Q32 (= 0.50 * 2^32)
  "bb": 2,                     # β exponent (curvature; small u32; renamed from "b" for
                               #   §3 same-length-keys invariant)
  "dl": true,                  # delegation_allowed (default true)
  "ax": "sp"                   # auto-exec action kind (only "none" or "sp"=set_power in v1)
}
```

### Signaling event payloads

```text
# Signal (kd="sg")
{ "pr": <proposal_id>, "s": true | false }

# Delegate (kd="dg")
{ "to": <16-byte OwnerAddr>, "sc": "all" }  # scope: "all" only in v1
# Spec amendment (post-implementation review): `to` was originally
# documented as the 32-byte ed25519 pubkey, but the rest of the CRDT
# keys on `OwnerAddr = SHA256(X25519_pub || Ed25519_pub)[..16]` — a hash
# that cannot be derived from just the Ed25519 pubkey. Carrying the
# OwnerAddr directly matches every other actor identifier in the system.
# Decoders MUST reject any `to` whose length is not exactly 16 bytes.

# Undelegate (kd="ud")
{ }
```

### Conviction compute (deterministic materialize)

Walk events in HLC order; per-(voter, proposal) maintain Q96.32 fixed-point state:

```text
state = {
  is_supporting: bool,
  support_started_at_ms: i128,
  accumulated_conviction_q32: i128,  // Q96.32 fixed-point
  last_event_at_ms: i128,
}

On Signal{support: true} at event.hlc_ms:
  if !is_supporting:
    # Decay prior accumulated up to "now" first, so subsequent
    # conviction_at queries treat last_event_at_ms as the joint
    # reference for both pools.
    dt = event.hlc_ms - last_event_at_ms
    accumulated_conviction_q32 = decay_q32(accumulated_conviction_q32, dt, half_life_ms)
    is_supporting = true
    support_started_at_ms = event.hlc_ms
    last_event_at_ms = event.hlc_ms

On Signal{support: false} at event.hlc_ms:
  if is_supporting:
    duration_ms = event.hlc_ms - support_started_at_ms
    # Decay prior accumulated across the just-ended support session
    # BEFORE adding the session's charge — mirrors conviction_at's
    # supporting branch (decayed prior + active charge). Without the
    # leading decay, += would freeze the prior pool at its session-start
    # value and overstate post-close conviction.
    accumulated_conviction_q32 = decay_q32(accumulated_conviction_q32, duration_ms, half_life_ms)
                                 + charge_q32(duration_ms, half_life_ms)
    is_supporting = false
    last_event_at_ms = event.hlc_ms

# Fixed-point charge function (Q96.32):
# charge(d, hl) = (1 - 0.5^(d/hl)) * hl / ln(2)
charge_q32(duration_ms: i128, half_life_ms: i128) -> i128:
  pow = pow_half_q32(duration_ms, half_life_ms)        # Q32; ≤ Q32
  one_minus = Q32 - pow
  return (one_minus * half_life_ms) / LN2_Q32          # ms

# Fixed-point decay function (Q96.32):
# decay(c, dt, hl) = c * 0.5^(dt/hl)
decay_q32(c_q32: i128, dt_ms: i128, half_life_ms: i128) -> i128:
  pow = pow_half_q32(dt_ms, half_life_ms)              # Q32
  return (c_q32 * pow) >> CONVICTION_FRAC_BITS

# Fixed-point 0.5^(t/hl) via Taylor series for exp(-x ln 2):
pow_half_q32(t_ms: i128, hl_ms: i128) -> i128:
  x_q32 = (t_ms * LN2_Q32) / hl_ms
  return exp_neg_q32(x_q32)

exp_neg_q32(x_q32: i128) -> i128:
  # Argument reduction + Taylor + squaring. Direct 7-term Taylor of exp(-x)
  # diverges at x ≥ ~2 (the |x^n/n!| terms overshoot the limit). Instead:
  #
  #   1. Clamp: if x > 20 * Q32, return 0 (exp(-20) ≈ 2e-9 underflows Q32 anyway).
  #   2. Halve k=8 times: x_reduced = x >> 8 (so x_reduced ≤ 20 * Q32 / 256 ≈ 0.08).
  #   3. Taylor 7 terms on x_reduced (error < 1e-12 in this small-x range).
  #   4. Square k=8 times: result = (result^2 >> Q32)^... (mathematically exact:
  #      exp(-x/256)^256 = exp(-x)).
  #
  # All steps are pure integer arithmetic with fixed iteration counts; bit-
  # identical across architectures.
  if x_q32 > 20 * Q32: return 0
  x_reduced = x_q32 >> 8
  term = Q32   # n=0
  sum = Q32
  for n in 1..=7:
    term = -(term * x_reduced) / (Q32 * n)
    sum += term
  for _ in 0..8:
    sum = (sum * sum) >> CONVICTION_FRAC_BITS
  return max(sum, 0)

conviction_at(t_ms, half_life_ms):
  if is_supporting:
    active_charge = charge_q32(t_ms - support_started_at_ms, half_life_ms)
    decayed_prior = decay_q32(accumulated_conviction_q32, t_ms - last_event_at_ms, half_life_ms)
    return decayed_prior + active_charge
  else:
    return decay_q32(accumulated_conviction_q32, t_ms - last_event_at_ms, half_life_ms)
```

Per-proposal total conviction at time t = Σ over voters of `conviction_at(t)`, weighted by delegation if applicable. Sums are performed on i128 Q32 values directly (associative integer addition is bit-identical across architectures).

### Dynamic threshold

```text
effective_supply(t_ms) = | { voter : voter has ≥ 1 active Signal at hlc_ms ≤ t_ms } |
total_supply           = | eligible_voters at PollCreate.hlc_ms |   # snapshotted

# `ratio` and `(1-ratio)^β` are dimensionless Q32 fractions:
ratio_q32 = (effective_supply * Q32) / total_supply              # ≤ Q32
one_minus_ratio_q32 = Q32 - ratio_q32

# (1 - ratio)^β via repeated multiplication (β is small integer, default 2):
pow_q32 = one_minus_ratio_q32
for _ in 1..β:
  pow_q32 = (pow_q32 * one_minus_ratio_q32) >> CONVICTION_FRAC_BITS

# T_min and T_max are in conviction-multiplier ms units — the SAME
# units `charge_q32` returns. This is a spec amendment from the first
# draft (which described them as Q96.32 fractions of a notional ceiling).
# The amendment was forced by `charge_q32`'s actual return units (the
# Q32 factors in `one_minus` and `LN2_Q32` cancel, leaving plain ms).
# Compare directly against `total_conviction_at` for threshold crossing.
span = T_max - T_min
threshold = T_min + ((span * pow_q32) >> CONVICTION_FRAC_BITS)
```

High participation → low threshold; low participation → high threshold. Solves the low-turnout paralysis problem: a small dedicated subgroup can pass routine items provided broader community doesn't actively oppose.

### Why fixed-point i128, not f64?

ZEB-291 acceptance criterion #2 requires that two engines running on different architectures (e.g., x86_64 desktop vs ARM laptop) materialize bit-identical conviction state from the same event log. IEEE 754 f64 does not provide this guarantee — fused-multiply-add hint reordering, subnormal handling, and `1/x` table approximations differ between x86 SSE/AVX and ARM NEON. Empirically, the same f64 computation on two x86 chips can differ in the least 1-2 bits, and on x86 vs ARM the divergence is reproducible. Q96.32 fixed-point is bit-identical by construction: every operation is integer arithmetic with explicit shift/divide rounding. The cost is a small implementation complexity and the loss of subnormal-range precision, neither of which matter for conviction values bounded by community membership × half-life.

### Lifecycle

Continuous — no `closes_at`. State machine:

```
Open  ──── conviction crosses threshold ────▶  ThresholdReached
                                                      │
                                          24h uncontested
                                                      │
                                                      ▼
                                                 Finalized
                                                 (PollResult signed;
                                                  auto-exec fires if "ax" set)
```

24h contestability window resolves CRDT ordering races: late-arriving Unsignal events from a partition can drop conviction back below threshold, returning state to `Open`. After 24h uncontested, finalizes at `ThresholdReachedProposal.hlc`.

### Delegation (liquid democracy)

Universal scope only in v1: voter A delegates to voter B → B's signaling carries B's own weight + A's weight on every Tier 2 proposal. A retains override (A can directly Signal on any proposal, superseding B's effective signal for A on that proposal). Delegation is **Tier 2 only** — does not affect Tier 1 ballots or Tier 3 sortition selection. Per-topic delegation is future work (requires topic taxonomy).

**Privacy trade-off documented:** Both delegate's signaling and delegator's `Delegate` event are public. Tier 2 has no privacy mode; communities concerned about coercion of delegators escalate those decisions to Tier 3.

### Eligibility — **rolling** (refinement)

Each `Signal` / `Delegate` event verified at its own HLC against community membership. Differs from Tier 1's snapshot-at-create. **Rationale:** continuous proposals admit continuous participation across the community's full lifetime (members join and leave, are born and die); some proposals may remain open for the community's entire existence.

### Kicked-member's accumulated conviction

**Decays normally at the half-life rate** (no implicit Unsignal at kick time). Rationale: (a) deincentivizes kicking-to-shift-polls; (b) gives time for improperly-kicked members to respond / be reinvited; (c) natural decay preserves community stability. Their already-cast Signal stops accumulating (they can no longer emit new Signal events), and their delegate (if any) loses the delegated weight on kick.

### UI

Distinct from Tier 1 — Conviction needs a persistent "active proposals" view, not chat-embedded. New `CommunityProposalsPanel.svelte` accessible from community settings / governance area. Per proposal: title, current conviction bar (vs. threshold line), ETA-to-threshold estimate, signaling toggle. Delegation widget shipped in Phase 3, not Phase 2.

### IPC commands

```text
voting_create_tier2_proposal(
    community_id: SpaceId,
    proposal_text: String,
    half_life_seconds: Option<u32>,
    threshold_min: Option<f32>,
    threshold_max: Option<f32>,
    beta: Option<f32>,
    delegation_allowed: bool,
    auto_exec_action: Option<AutoExecAction>,
    eligibility: Eligibility,
) -> Result<PollId>

voting_signal_tier2(proposal_id: PollId, support: bool) -> Result<()>
voting_delegate_tier2(community_id: SpaceId, delegate: Option<OwnerAddr>) -> Result<()>
voting_list_tier2_proposals(community_id: SpaceId) -> Vec<ProposalState>
voting_get_tier2_proposal(proposal_id: PollId) -> ProposalState
voting_contest_tier2_finalization(proposal_id: PollId) -> Result<()>
```

### Auto-exec actions

V1 scope: `none` (signals only) and `set_power { target_pubkey, new_power }`. Other downstream actions (kick, set_admin_quorum, update_channel_config, set_community_policy) are out of scope for v1 — communities build IFTTT-style automation on top of the signal events through their own admin workflows or community-bot layers. Voting just emits the signal.

**Admin-quorum interaction (ZEB-300).** When the community has `admin_quorum > 1` and the `set_power` outcome is *admin-affecting* (ZEB-250 §4.3: `new_power == 100`, or the target currently holds power 100), a *direct* `SetPower` is rejected by `verify_event` (`SetPowerRequiresQuorum`), so auto-exec instead routes through `AdminProposal`. Each admin replica's tick runs a deterministic planner: mint an `AdminProposal::SetPower` if no live one exists for that `(target, level)`, otherwise countersign the **canonical** (smallest-`EventId`) pending proposal it has not yet signed. This converges across ticks without a coordinator and tolerates absent admins; under a simultaneous-tick race the non-canonical proposal is left inert and expires per `ADMIN_PROPOSAL_EXPIRY_MS`. See ZEB-300 and `docs/specs/2026-07-19-zeb-300-tier2-adminproposal-setpower-design.md`.

## 6. Tier 3 — Sortition + Pol.is + STAR

Constitutional decisions. Multi-stage: selection → deliberation → drafting → ratification. Supports two privacy modes (ballot-secret D-FROST, receipt-free TRIP).

### Lifecycle (four stages)

```
Stage 1: Proposal + Sortition Selection
  ↓ (deliberation_window — default 14d)
Stage 2: Pol.is-style Deliberation (mini-public only)
  ↓ (drafting_window — default 7d)
Stage 3: Drafting (mini-public synthesizes candidates)
  ↓ (ratification_window — default 14d)
Stage 4: STAR Ratification (full electorate)
  ↓
Finalized
```

### PollConfig payload (`pd` for `kd="cr"`, Tier 3)

```text
{
  "pt": "Amend charter §3: require 2/3 supermajority for moderator dismissals",
  "ss": 100,                # sortition_size (50-300; default 100)
  "dw": 1209600,            # deliberation_window seconds (default 14d)
  "fw": 604800,             # drafting_window seconds (default 7d)
  "rw": 1209600,            # ratification_window seconds (default 14d)
  "pm": "se" | "rf",        # privacy_mode: "se"=ballot-secret D-FROST, "rf"=receipt-free TRIP
  "im": "d"                 # incentive_mode (a/b/c/d per §6.1.2; default "d")
}
```

Auto-exec always `none` for Tier 3 — constitutional decisions are too consequential to auto-execute; admin enacts manually.

### 6.1 Stage 1 — Sortition Selection

Deterministic random selection via D-FROST-derived VRF beacon. The community's D-FROST committee (n-of-m members holding refreshable secret shares per CHURP) produces a VRF output seeded by `H(PollCreate.event_hash || community_epoch)`. Output is publicly verifiable, unbiasable (committee cannot grind), unpredictable until the committee produces it.

Anyone can derive the sortition selection from VRF output + eligible-electorate snapshot using deterministic Fisher-Yates sampling seeded by the VRF. Selection signed as `SortitionSelection` event; mismatches between independently-computed selections indicate bug or beacon misbehavior.

#### 6.1.1 Sortition size

Default 100. Dynamic heuristic (future enhancement): `max(20, min(300, sqrt(eligible_electorate_size)))`. For 10k electorate, √10000 = 100; for 100, floor at 20; for 1M, ceiling at 300.

#### 6.1.2 Mini-public participation incentive modes

Community-configurable; v1 ships all four:

- **(a) SoftExpectation** — pure civic-duty framing in UI; no consequences for declining.
- **(b) AutoPowerBoost** — temporary power increment during service.
- **(c) CompulsoryWithOptOut** — selection stands; you can decline but a "did not participate" record is created.
- **(d) DeclineWithBackupPool** — DEFAULT. Declining is no-stigma; backup pool fills the slot.

**Failure mode flagged:** mass-decline could exhaust the backup pool. If sortition fails to assemble a full mini-public, the proposal returns to its proposer with a `SortitionFailed` signal — they can reschedule or reduce `sortition_size`.

### 6.2 Stage 2 — Pol.is-style Deliberation

Mini-public members submit short statements (≤280 chars) via `DeliberationStatement` events. All mini-public members agree/disagree on every other member's statements via `DeliberationVote` events. An opinion-clustering pass identifies opinion groups, then surfaces "bridging statements" (statements with strong consensus across opinion clusters).

**Algorithm choice (full PCA vs. simpler heuristic) deferred to phase brainstorm.** Pol.is itself is centralized; decentralized Pol.is is an open research question. Phase 5 begins with a simpler bridging-statement heuristic and upgrades to PCA if feasibility is established.

### 6.3 Stage 3 — Drafting

Mini-public synthesizes 1-5 candidate resolutions from bridging statements. Selection mechanism: internal Tier 1 Approval among mini-public members (recursive use of Tier 1 — communities use voting to vote on what to vote on). Status quo is automatically included as a candidate.

### 6.4 Stage 4 — STAR Ratification

Every eligible community member (full electorate, not just mini-public) scores each candidate 0-5 via `RatificationBallot`.

```text
1. Total each candidate's scores; top 2 = finalists.
2. For each ballot, allocate one full vote to whichever finalist the voter scored higher.
3. Winner = runoff winner. Ties broken by total-score in finalist round.
```

### 6.5 Privacy modes

#### Mode A — Ballot-secret (D-FROST)

Each ratification ballot encrypted to committee's joint public key. After ratification window closes, committee members publish decryption shares (`TallyShare` events — D-FROST partial decryptions). Anyone combines threshold shares to recover aggregate tally without recovering individual ballots.

Voter retains randomness; can prove their vote to a coercer if they want (no receipt-freeness). Defeats passive observers and post-vote doxxing.

#### Mode B — Receipt-free (TRIP at civic infrastructure)

Voter visits a physical kiosk (library / makerspace / post office / civic space) hosting the community's TRIP component. Kiosk in a privacy booth generates either a real or fake credential as voter chooses; printed-paper interactive zero-knowledge proof distinguishes them only to the voter (who observes the print order), not to anyone outside the booth.

Voter casts ratification ballots signed by credential. Tally absorbs fake-credential ballots silently; real-credential ballots count. Voter cannot prove how they voted (fake credential is indistinguishable from real).

Communities without nearby kiosks cannot use Mode B (must use Mode A). Per Harmony's civic-infrastructure pattern, libraries are the natural federated trust anchors for kiosk hosting.

### 6.6 Eligibility

- Sortition: snapshot at `PollCreate.hlc`.
- Mini-public: drawn from snapshot; no additional rights, only additional *duties* during deliberation/drafting.
- Ratification: opens to the *full* snapshot (everyone eligible at `PollCreate.hlc`, not just mini-public).

### 6.7 IPC commands (Tier 3, additional to core)

```text
voting_create_tier3_proposal(
    community_id: SpaceId,
    proposal_text: String,
    sortition_size: Option<u16>,
    deliberation_window_seconds: Option<u32>,
    drafting_window_seconds: Option<u32>,
    ratification_window_seconds: Option<u32>,
    privacy_mode: PrivacyMode,    # Secret | ReceiptFree
    incentive_mode: IncentiveMode,
    eligibility: Eligibility,
) -> Result<PollId>

voting_submit_deliberation_statement(poll_id, text: String) -> Result<()>
voting_vote_deliberation_statement(poll_id, statement_id, agree: bool) -> Result<()>
voting_propose_draft_candidate(poll_id, candidate_text: String) -> Result<()>

voting_cast_ratification_ballot(
    poll_id: PollId,
    scores: Vec<u8>,            # 0-5 per candidate, indexed by candidate order
    credential: Credential,     # plaintext (Mode A) or TRIP-derived (Mode B)
) -> Result<()>

voting_publish_tally_share(poll_id) -> Result<()>   # committee members invoke after window closes
```

## 7. Tier selection & community policy

### Tier selection model

Per-community policy with per-poll override. Matches polycentric governance principle. Community admins configure defaults in `CommunityVotingPolicy`; poll authors override per-poll unless community policy disables override.

### CommunityVotingPolicy schema

Extends existing per-community state:

```text
CommunityVotingPolicy {
  tier_defaults_per_use_case: HashMap<UseCase, Tier>,
  override_allowed: bool,
  conviction_half_life_default_seconds: u32,    # default 604800 = 7d
  conviction_threshold_min: f32,                # default 0.05
  conviction_threshold_max: f32,                # default 0.50
  conviction_beta: f32,                         # default 2.0
  sortition_size_default: u16,                  # default 100
  sortition_size_formula: SortitionSizeFormula, # Fixed | SqrtN { min, max }
  tier3_privacy_mode_default: PrivacyMode,
  tier3_incentive_mode_default: IncentiveMode,  # default DeclineWithBackupPool
}

UseCase {
  ChannelProposal,
  ResourceAllocation,
  ModerationPolicy,
  TrustDelegation,
  ModeratorElection,
  ForkReferendum,
  AdHoc,
}
```

### Default tier-to-use-case mapping (out-of-box)

| Use case | Default tier | Notes |
|----------|--------------|-------|
| `ChannelProposal` | Tier 1 | low-stakes, chat-native fits |
| `ResourceAllocation` | Tier 1 | pin / feature decisions |
| `ModerationPolicy` | Tier 2 | continuous norm evolution |
| `TrustDelegation` | Tier 2 | pairs with continuous signaling |
| `ModeratorElection` | Tier 2 | small communities; large communities override to Tier 3 |
| `ForkReferendum` | Tier 3 | constitutional gravity |
| `AdHoc` | Tier 1 | safest default |

### Cross-tier composition

Tiers are non-chaining — Tier 3 result does not auto-trigger Tier 2 vote. Composition is manual:

- Tier 1 result → admin enacts manually (auto-exec always `none`).
- Tier 2 result → either auto-exec (`none` or `set_power`) or admin enacts manually.
- Tier 3 result → admin enacts manually only (auto-exec always `none` for safety).

Communities wanting "Tier 3 decision X triggers Tier 2 vote on enactment Y" run that orchestration through their own admin workflow or community-bot layer.

## 8. Verify rules (receive-time)

Applied to every incoming voting event.

### Generic

```text
V1  Signature valid against `ac` over canonical CBOR-encoded envelope (excluding "sg").
V2  Schema version "vr" recognized; unknown version → drop with UnknownVersion.
V3  Tier "tr" known to this node; unknown → drop with UnknownTier (fail-soft).
V4  Kind "kd" valid for tier "tr".
V5  HLC "hc" strictly monotonic w.r.t. actor's prior events in same voting log.
V6  Actor "ac" is community member at event.hlc.
```

### PollCreate (`kd="cr"`)

```text
C1  Tier-specific validate_poll_config(pd) succeeds.
C2  Actor meets community policy's "who can create polls" gate.
```

### BallotCast (`kd="bl"`, Tier 1) / RatificationBallot (`kd="rb"`, Tier 3)

```text
B1  Referenced poll_id exists as known PollCreate event.
B2  Poll lifecycle state is Open at event.hlc.
B3  Actor meets poll's eligibility predicate at PollCreate.hlc snapshot
      (Tier 1; Tier 3 ratification — same snapshot).
B4  Tier-specific validate_ballot(pd, poll_meta) succeeds.
B5  (Tier 3 rb only) Ballot encoding matches privacy_mode.
```

### PollClose (`kd="cl"`)

```text
L1  Poll lifecycle state is Open at event.hlc.
L2  Window expired: event.hlc ≥ PollCreate.hlc + window_seconds (Tier 1 only).
```

### PollResult (`kd="rs"`)

```text
R1  Poll lifecycle state is Closed at event.hlc.
R2  Result in pd is bit-identical to result deterministically computed
      from event log at PollResult.hlc.
```

R2 makes `PollResult` signable by *anyone* — tally is a pure function of the log; no designated tallyer.

### Tier 2 Signal (`kd="sg"`)

```text
S1  Referenced proposal_id exists and is Open at event.hlc.
S2  Actor eligible per poll's eligibility predicate AT EVENT.HLC (rolling — §5).
```

### Tier 2 Delegate (`kd="dg"`)

```text
D1  Delegate OwnerAddr is a community member at event.hlc.
D2  No delegation cycle (A→B→...→A); detected via transitive-closure walk over active Delegate events.
```

### Tier 3 SortitionSelection (`kd="ss"`)

```text
SS1 Selection deterministically derived from D-FROST VRF output + eligible-electorate snapshot.
    Any node computes the same; mismatches rejected.
```

### Tier 3 Deliberation / Drafting (`kd="ds"`/`"dv"`/`"dc"`)

```text
SD1 Actor is in the mini-public for this poll (per SortitionSelection).
```

### Tier 3 TallyShare (`kd="ts"`)

```text
T1  Actor is in D-FROST committee for community's current epoch.
T2  Share decrypts validly against published committee public key.
```

## 9. Materialize rules

```text
Inputs: stream of verified events for community C, ordered by (hlc, event_hash) lexicographically.

State: HashMap<PollId, PollState> where
  PollState {
    meta: PollMeta,
    events: Vec<SignedPollEvent>,    # for audit
    tier_state: Box<dyn Any>,        # tier-specific tally state
    lifecycle: Lifecycle,            # Draft|Open|Closed|Finalized|Archived
  }

For each event e in HLC order:
  Look up tier impl from voting_core registry.
  Dispatch:
    PollCreate  → create PollState{meta=PollMeta::from(pd),
                                   tier_state=tier.materialize_initial(),
                                   lifecycle=Open}
    PollClose   → state.lifecycle = Closed; trigger tier.finalize() on next materialize tick
    PollResult  → state.lifecycle = Finalized; canonical result stored
    Other       → tier.apply_event(&mut state.tier_state, e)
  Append e to state.events (for audit).

# Archive sweep (runs daily):
For each PollState in Finalized lifecycle where (now - PollResult.hlc) > 90 days:
  Drop state.events (keep only meta + result).
  Set state.lifecycle = Archived.
```

## 10. Failure modes & edge cases

### Eligibility timing (per-tier rules)

| Tier | Eligibility rule | Rationale |
|---|---|---|
| 1 | Snapshot at `PollCreate.hlc` | One-shot tally; deterministic electorate |
| 2 | **Rolling — each event verified at its own HLC** | Continuous proposals admit continuous participation |
| 3 Sortition | Snapshot at `PollCreate.hlc` | Mini-public must be statistically representative |
| 3 Ratification | Snapshot at `PollCreate.hlc` | Same electorate that authorized sortition ratifies outcome |

### Network partition handling

- **Tier 1:** Last-write-wins per `(voter, poll)`. Partition-A's ballot superseded by Partition-B's later ballot from same voter on merge.
- **Tier 2:** Conviction recomputed deterministically from full merged log. May briefly cross/uncross threshold during merge — 24h contestability window absorbs this.
- **Tier 3 sortition:** VRF beacon is deterministic; partitions independently compute same `SortitionSelection`. Partition without VRF beacon yet may produce no selection; rejected at SS1 on merge.
- **Tier 3 ratification:** All ballots converge; D-FROST tally proceeds once threshold of TallyShare events accumulate post-merge.

### Membership changes mid-poll

- **Kicked member:** ballots they already cast remain valid (verified at their own HLC, when they were a member). Future ballots rejected at V6. For Tier 2: their existing accumulated conviction **decays at normal half-life rate** — no implicit Unsignal — but they cannot emit new Signal events. Their delegate (if any) loses delegated weight on kick.
- **Power changed mid-poll:** doesn't affect already-cast ballots; affects future ballots only if poll has `min_power` gate.
- **Vouching depth changed mid-poll:** same — past ballots valid, future ballots gated.
- **Join mid-Tier-1-poll:** cannot vote (snapshot-at-create excludes them).
- **Join mid-Tier-2-proposal:** CAN signal (rolling eligibility); conviction accumulates from first Signal.

### Tier 3 D-FROST committee churn during long polls

Tier 3 polls span ~5 weeks. Committee rotates via CHURP — public key stays stable; private shares redistribute. Poll's encryption key is bound to committee public key, not individual members. Decryption works after rotation. Edge case: churn exceeds threshold simultaneously → tally stalls until membership recovers. Surfaced as "tally pending — awaiting committee availability"; community can trigger manual refresh.

### TRIP kiosk unavailability (Mode B receipt-free)

Voters need credentials *before* ratification window opens. PollCreate-time verification: community confirms at least one operational kiosk in federation; otherwise C1 fails. During ~3 weeks pre-ratification, voters visit kiosks. Kiosk offline mid-window: voters without credentials at that point cannot vote on this poll; tally proceeds with available credentials.

### Community fork during a poll

Per [ZEB-285](https://linear.app/zeblith/issue/ZEB-285), forking copies forker's locally-held history. Poll events signed before fork-point appear in fork's voting log; fork inherits historical poll state. **Behavior:** original community's poll continues unchanged. In the fork, poll appears as inherited record but **cannot be extended** — new BallotCast / Signal events rejected at V6 because eligibility predicate uses *original community's* membership snapshot. Fork wanting to continue the spirit of the poll creates a new poll in their own community.

### Auto-exec target invalidation

If Tier 2 `set_power` targets a member kicked between proposal-create and finalization, auto-exec fails at apply-time (target no longer exists). Surfaced as `PollExecutionFailed` event; admins enact manually. No retry, no panic.

## 11. Backwards compatibility

- **Communities created before voting ships:** `CommunityVotingPolicy` absent. Default policy synthesized at materialize time from §7 defaults.
- **Voting events at nodes without voting support** (`UnknownEventFamily` on `"tg":"p"`): events stay in community's signed-event log (not dropped — node still verifies signature and HLC), just not materialized for voting state. Once node upgrades, materialize re-runs and tallies catch up.
- **New tiers in future versions:** `UnknownTier` fail-soft (V3) lets older nodes skip new tier events without breaking. Older nodes show "this poll uses a newer voting tier your client doesn't support yet" in UI.
- **New event kinds within existing tier:** same fail-soft. `UnknownKind` rejected at V4; node logs and continues.
- **Schema version bumps:** V2 rejects unknown versions. Major schema changes ship as `"vr": 2`; older nodes skip; once adoption sufficient, tier modules drop `"vr": 1` support.
- **No interaction with [ZEB-217](https://linear.app/zeblith/issue/ZEB-217) (membership) / [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) (channels) / [ZEB-250](https://linear.app/zeblith/issue/ZEB-250) (admin quorum) / [ZEB-285](https://linear.app/zeblith/issue/ZEB-285) (fork)** beyond reading their state as inputs (membership snapshot, channel existence, member power levels, fork provenance). Voting does not write into their CRDTs.

## 12. Testing strategy (umbrella-level)

Each phase ships its own detailed test plan; this section pins what gates each phase must clear at minimum.

### Per-phase gates (all phases)

- Five required CI gates (per project convention):
  - `cargo fmt --all -- --check`
  - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
  - `npx tsc --noEmit`
  - `npx vitest run`
- Wire-format fixture pinning (CBOR byte-pinned fixtures per event kind; regenerate-on-first-run pattern).
- Multi-engine integration tests (2+ in-process engines converge on tally).
- Backwards compatibility test: old-version events decode + materialize on new schema.

### Tier-specific test priorities

- **Tier 1:** convergence under HLC reordering; revote LWW; quorum / threshold edge cases; multi-winner.
- **Tier 2:** conviction compute determinism; threshold-crossing detection; 24h contestability; rolling eligibility under churn; delegation cycle detection.
- **Tier 3:** VRF determinism; sortition reproducibility; mini-public restriction enforcement; STAR runoff math; both privacy modes; CHURP rotation tolerance.

## 13. Open research questions

Tracked for follow-up before relevant phases begin:

1. **Decentralized Pol.is** (blocks Phase 5 polish — Phase 5 ships with simpler bridging heuristic; full PCA is enhancement). Genuine open research question: PCA + opinion clustering in CRDT-land.
2. **Decentralized randomness beacon implementation details** (blocks Phase 4). D-FROST-derived VRF is locked in conceptually; concrete construction (e.g., threshold BLS, Schnorr-based VRF) chosen during Phase 4 brainstorm.
3. **CHURP-style proactive secret sharing on consumer mobile hardware** (relevant for Phase 4 + Phase 6). Performance characteristics on residential bandwidth + consumer CPUs need empirical validation.
4. **TRIP kiosk software architecture** (Phase 7). Likely a separate small repo; partnership model with civic institutions (libraries) needs operational design.

## 14. Out of scope (v1)

Future / parking-lot items the spec explicitly does not design:

- **Per-topic delegation** (Tier 2 enhancement). Requires topic taxonomy. File when a real community asks.
- **Decentralized Pol.is full PCA** (per Phase 5 above).
- **Federation-level voting** (cross-community). Needs own umbrella spec.
- **Mobile-optimized TRIP kiosk** (Phase 7 variant). v1 designs around desktop-class kiosks at civic spaces.
- **Auto-exec actions beyond `set_power`** (Tier 2). Communities build IFTTT layers on signal events.
- **Mini-public incentive modes a/b/c implementations** (Tier 3). v1 ships mode `d` (DeclineWithBackupPool) as default; a/b/c are wire-format-reserved but UI/UX deferred.

## 15. Phased decomposition into sub-tickets

Seven phases, ascending complexity / descending user-frequency. ZEB-289 stays as parent epic; each phase becomes its own sub-ticket with own spec + plan + implementation cycle.

| Phase | Linear | Scope | Dependencies | Effort |
|---|---|---|---|---|
| **1** | [ZEB-290](https://linear.app/zeblith/issue/ZEB-290) | `voting_core.rs` (shared types, eligibility verifier, IPC dispatcher, lifecycle state machine) + `voting_approval.rs` (Approval mechanism, ballot, materialize, tally) + chat-embedded poll UI + IPC commands + Tauri events. | none | M (~2-3w) |
| **2** | [ZEB-291](https://linear.app/zeblith/issue/ZEB-291) | `voting_conviction.rs` (conviction compute, dynamic threshold, lifecycle, 24h contestability, **delegation event types + verify rules** but no delegation UI) + CommunityProposalsPanel.svelte (basic signal toggle, no delegation widget) + `set_power` auto-exec wiring. | Phase 1 | M (~2-3w) |
| **3** | [ZEB-292](https://linear.app/zeblith/issue/ZEB-292) | Delegation widget in CommunityProposalsPanel, delegate-graph visualization, revocation flow, delegation-state notifications. No new backend (already shipped in Phase 2). | Phase 2 | S (~1w) |
| **4** | [ZEB-293](https://linear.app/zeblith/issue/ZEB-293) | `voting_sortition.rs` (sortition selection, STAR ratification, drafting), D-FROST-derived VRF beacon, mini-public/full-electorate eligibility split, public ratification ballots only (no Pol.is, no privacy). Includes foundational D-FROST committee primitives. | Phase 1 (Tier 1 reused within drafting), D-FROST committee primitives | L (~3-4w) |
| **5** | [ZEB-294](https://linear.app/zeblith/issue/ZEB-294) | DeliberationStatement + DeliberationVote CRDT, opinion clustering / bridging-statement detection. Algorithm choice deferred to phase brainstorm. Mini-public deliberation UI. | Phase 4 | M (~2-3w) |
| **6** | [ZEB-295](https://linear.app/zeblith/issue/ZEB-295) | Extend D-FROST committee to handle threshold-ElGamal decryption. Encrypted RatificationBallot, TallyShare events, multi-party decryption protocol. CHURP-style proactive secret share rotation. | Phase 4, Phase 5 if in path | L (~3-4w) |
| **7** | [ZEB-296](https://linear.app/zeblith/issue/ZEB-296) | TRIP kiosk software (likely separate small repo), credential generation + paper IZKP printing, voter UX for credential storage + ballot signing, library partnership onboarding documentation. Builds on Phase 6 D-FROST tally. | Phase 6, civic infrastructure partnerships | XL (~6-8w + civic rollout) |

**Parallelization opportunities:** Phase 4 doesn't strictly depend on Phase 3 (delegation UI), so Phase 4 could start while Phase 3 is in flight. Phase 5 (Pol.is) doesn't block Phase 6 (D-FROST tally) — they touch different parts of Tier 3 lifecycle. If shipping Tier 3 privacy is higher priority than deliberation polish, order can become 4 → 6 → 5 → 7. Linear order matches user-value-per-phase (most-useful first).

## 16. References

- [ZEB-289](https://linear.app/zeblith/issue/ZEB-289) — this umbrella epic
- [ZEB-217](https://linear.app/zeblith/issue/ZEB-217) — Sub-C v1 community membership CRDT (admin governance foundation)
- [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) — Sub-C v2 channel-config CRDT (pattern source for parallel per-community log)
- [ZEB-250](https://linear.app/zeblith/issue/ZEB-250) — M-of-N admin quorum (CBOR same-length-keys pattern source)
- [ZEB-285](https://linear.app/zeblith/issue/ZEB-285) — community forking primitive (poll-fork interaction)
- Memory: `project_harmony_polycentric_governance` — communities-only governance, no platform admin
- Memory: `project_harmony_civic_infrastructure_pattern` — libraries as federated trust anchors (TRIP kiosk hosts)
- Memory: `feedback_design_for_eventual_state` — design for eventual UX, not transitional empty state
- Memory: `feedback_engineer_for_real_scale` — design for billions
- Gemini research report on decentralized receipt-free voting protocols (Report 1)
- Gemini research report on voting mechanism design space (Report 2)
