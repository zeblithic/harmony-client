/**
 * ZEB-290 Phase 1 — TypeScript types mirroring the Rust voting wire
 * format. All Rust fields are emitted by serde with the default
 * snake_case → camelCase IPC convention used elsewhere in this app
 * (Tauri's IPC layer auto-converts at the boundary), except for the
 * `PollMeta` / `Eligibility` payloads which are serialized verbatim
 * by `serde(rename = "…")` shortcodes for canonical CBOR. Those
 * shortcode fields use the same letter names on the TS side so the
 * decoded shape lines up with what the Rust backend hands over.
 *
 * See `src-tauri/src/community_voting_core.rs` for the source of truth.
 */

/** 32-byte SHA-256 poll id.
 *
 * **IPC reality:** Tauri's JSON serializer has no byte-string type, so
 * `[u8; N]` Rust fields arrive over the IPC boundary as integer arrays
 * (`[171, 171, ...]`), not as the hex strings the Rust wire format
 * (CBOR bstr via `serialize_bytes_as_bstr`) uses. Treat these as opaque
 * `number[]` round-trip identifiers, NOT as strings — interpolating
 * them into a URL or comparing with `===` would silently misbehave
 * (`===` reference-compares arrays). Use `pollIdEqual()` for value
 * equality.
 *
 * A format-aware (`is_human_readable`) serializer that hex-encodes for
 * IPC while keeping bstr for CBOR is tracked as a Phase 1.5 cleanup
 * shared with `SpaceId`/`OwnerAddr`/`ChannelId`. Once shipped, these
 * types narrow to `string` without code-side changes. */
export type PollIdHex = number[];

/** 16-byte SpaceId. See PollIdHex JSDoc — same JSON integer-array reality. */
export type CommunityIdHex = number[];

/** 16-byte ChannelId. See PollIdHex JSDoc. */
export type ChannelIdHex = number[];

/** 16-byte OwnerAddr. See PollIdHex JSDoc. */
export type OwnerAddrHex = number[];

/** Compare two byte-array identifiers by value (===-comparing arrays
 *  is reference-equality and would always be false for distinct
 *  Tauri-IPC-decoded instances). */
export function pollIdEqual(a: number[], b: number[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) return false;
  }
  return true;
}

/** Lowercase-hex-encode a byte-array identifier for the IPC boundary.
 *  The Rust IPC commands (`voting_get_poll`, `voting_cast_tier1_ballot`,
 *  …) take `poll_id: String` (hex), while IPC RETURN values carry ids as
 *  `number[]` (see PollIdHex JSDoc) — every call that feeds a returned id
 *  back into a command must convert through this (Greptile PR #443). */
export function pollIdToHex(id: number[]): string {
  return id.map((b) => b.toString(16).padStart(2, '0')).join('');
}

/** Voting tiers. Mirrors Rust `Tier` (u8 repr — Approval=1, Conviction=2,
 *  Sortition=3); the Rust enum uses `serde_repr` so the wire form (and
 *  thus the Tauri IPC payload) is the integer discriminant. Compare
 *  against the numeric literals, not against string variant names. */
export type Tier = 1 | 2 | 3;
export const TIER_APPROVAL = 1 as const;
export const TIER_CONVICTION = 2 as const;
export const TIER_SORTITION = 3 as const;

/** Lifecycle phases. Draft is implementation-only — never on the wire. */
export type Lifecycle = 'Draft' | 'Open' | 'Closed' | 'Finalized' | 'Archived';

/** Per-poll eligibility predicate. Mirrors `Eligibility` — serde
 *  rename uses `mp`/`mv`/`sz` shortcodes for canonical CBOR. */
export interface Eligibility {
  /** Required member power level. */
  mp: number;
  /** Optional vouching depth (Sybil filter). */
  mv?: number;
  /** Tier-3 sortition pool size; ignored for Tier 1/2. */
  sz?: number;
}

/** HLC. Mirrors `Hlc` — serde rename uses `w`/`l`/`d` shortcodes for
 *  canonical CBOR same-length-keys invariant. */
export interface Hlc {
  w: number;
  l: number;
  d: string;
}

/**
 * Materialized poll metadata. Mirrors `PollMeta`. Returned by
 * `voting_list_active_polls` (per poll) and embedded in
 * `PollStateExport.meta` from `voting_get_poll`.
 *
 * Field names use snake_case here because `PollMeta` does NOT carry a
 * `#[serde(rename_all = ...)]` attribute on the Rust side — it round-
 * trips canonical CBOR with the field names as written. The
 * camelCase-rename convention only applies to IPC event payload structs
 * (which explicitly opt in via `#[serde(rename_all = "camelCase")]`).
 */
export interface PollMeta {
  poll_id: PollIdHex;
  community_id: CommunityIdHex;
  creator: OwnerAddrHex;
  tier: Tier;
  eligibility: Eligibility;
  lifecycle: Lifecycle;
  created_at: Hlc;
  opens_at: Hlc;
  closes_at: Hlc;
  extends_at?: Hlc;
  /** Tier-1 chat-native polls carry their host channel id here. */
  channel_id?: ChannelIdHex;
}

/** Tally projection. Mirrors `TallyExport`. */
export interface TallyExport {
  /** Per-option approval counts, length = `options.length` for Tier 1.
   *  Empty array for peer-received polls without a cached snapshot. */
  counts: number[];
  /** Total ballots tallied (after last-write-wins dedupe by voter). */
  ballot_count: number;
}

/**
 * Frontend-friendly subset of `PollState` for IPC return values.
 * Mirrors `PollStateExport`. Returned by `voting_get_poll`.
 *
 * `options` (added Task 16) carries the Tier-1 option labels so the UI
 * can render labels alongside the tally without a second IPC. Empty
 * for non-Tier-1 polls or peer-received polls without a cached
 * `Tier1PollConfig`.
 */
export interface PollStateExport {
  meta: PollMeta;
  tally: TallyExport;
  /** Caller's own latest ballot's approved indices (last-write-wins
   *  by HLC). Undefined when caller has not yet voted. */
  your_ballot?: number[];
  /** Tier-1 option labels in option-index order. */
  options: string[];
}

// ── Tauri event payloads ──────────────────────────────────────────────
// These are camelCased because the Rust payload structs explicitly use
// `#[serde(rename_all = "camelCase")]`.

/** Payload for `voting-poll-created` Tauri event. */
export interface VotingPollCreatedPayload {
  /** Lowercase hex — the Rust payload structs emit `String` ids (unlike
   *  IPC RETURN values, which carry ids as `number[]`; see PollIdHex).
   *  These were previously mistyped as byte arrays, which made
   *  `pollIdEqual(payload.pollId, …)` comparisons dead code
   *  (Greptile PR #443). */
  pollId: string;
  channelId: string;
  communityId: string;
}

/** Payload for `voting-ballot-cast` Tauri event. */
export interface VotingBallotCastPayload {
  /** Lowercase hex (Rust emits `String` — see VotingPollCreatedPayload). */
  pollId: string;
  /** Lowercase hex. */
  voter: string;
  approvedCount: number;
}

/**
 * Payload for `voting-poll-closed` Tauri event. Phase 1: the backend
 * does NOT actually emit this event (lifecycle transitions to Closed
 * land in Phase 1.5 with the Zenoh auto-close tick). We wire the
 * listener now for forward compatibility.
 */
export interface VotingPollClosedPayload {
  /** Lowercase hex (Rust emits `String` — see VotingPollCreatedPayload). */
  pollId: string;
  communityId: string;
}

// ─── ZEB-291 Phase 2 — Tier 2 (Conviction) frontend types ──────────────
// These mirror the Rust shapes from src-tauri/src/lib.rs
// (`Tier2ProposalExport`, `VotingTier2ProposalCreatedPayload`,
// `VotingTier2SignalCastPayload`) and from
// `community_voting_conviction::{AutoExecAction, Tier2PollConfig}`.
//
// The IPC return values use `#[serde(rename_all = "camelCase")]` so
// fields arrive at the JS boundary in snake_case → camelCase form — but
// we deliberately keep the snake_case spelling on the TS side (same
// convention PollStateExport uses) because the field names ARE the
// snake_case spelling on the wire here (Rust struct fields are
// snake_case; the `rename_all` was a Tier 1 mistake we don't repeat
// here — see PollMeta JSDoc note about which structs do/don't use the
// rename). Actual wire shape verified against `build_tier2_export`.

/** Auto-exec action attached to a Tier 2 proposal.
 *
 * Wire form: tagged union with a 2-char `kk` discriminator. Mirrors
 * Rust `community_voting_conviction::AutoExecAction`:
 *   - `None` → `{ kk: 'n' }`
 *   - `SetPower { target_pubkey, new_power }` → `{ kk: 'sp', tg, np }`
 *
 * The 2-char discriminator is load-bearing for the spec §3 same-length-
 * keys invariant — every key at this nesting level is 2 chars. */
export type AutoExecAction =
  | { kk: 'n' }
  /** `tg` is the 16-byte target OwnerAddr as a JSON `number[]`; `np`
   *  is the new power level (0..=100). */
  | { kk: 'sp'; tg: number[]; np: number };

/** Tier 2 (Conviction) poll config. Mirrors Rust
 *  `community_voting_conviction::Tier2PollConfig` (same 2-char-key
 *  invariant). Frontend-emitted when creating a proposal via the
 *  voting-adapter; the adapter expands these into the camelCase IPC
 *  arg names the Rust `voting_create_tier2_proposal` command expects. */
export interface Tier2PollConfig {
  /** Proposal text (max 4096 bytes per Rust IPC validation). */
  pt: string;
  /** Conviction half-life, in seconds. */
  hl: number;
  /** `T_min` — floor of the dynamic threshold band (Q96.32 raw — see
   *  spec §5 amendment). */
  tn: number;
  /** `T_max` — ceiling of the dynamic threshold band (Q96.32 raw). */
  tx: number;
  /** β exponent for the `(1 - participation_ratio)^β` curve. */
  bb: number;
  /** Whether voters may delegate their conviction-weight. */
  dl: boolean;
  /** Auto-exec action that fires on finalization. */
  ax: AutoExecAction;
  /** Eligibility predicate — mirrors Phase 1 `Eligibility` shape. */
  el: { mp: number; mv?: number; sz?: number };
}

/** Frontend DTO for a Tier 2 proposal. Mirrors Rust
 *  `Tier2ProposalExport` (lib.rs). The `lifecycle` field is a string
 *  because the Rust side serializes via `format!("{:?}", lifecycle)`
 *  rather than a typed enum — UI compares against the string spellings. */
export interface Tier2ProposalExport {
  /** Hex-encoded 32-byte PollId. */
  proposal_id: string;
  /** Hex-encoded 16-byte SpaceId. */
  community_id: string;
  proposal_text: string;
  /** "Open" | "ThresholdReached" | "Finalized" | "Archived". (The
   *  Rust enum also emits "Draft" and "Closed" variants but neither
   *  applies to a Tier 2 lifecycle in practice — Draft is impl-only,
   *  Closed is Tier 1's terminal pre-Finalized state.) */
  lifecycle: 'Open' | 'ThresholdReached' | 'Finalized' | 'Archived';
  /** Q96.32 sum of per-voter conviction (post-delegation). Serialized
   *  as a decimal string by the Rust IPC layer because raw Q96.32 values
   *  routinely exceed `Number.MAX_SAFE_INTEGER` — reconstruct via
   *  `BigInt(value)`. For progress bars use `convictionPercent()` below. */
  total_conviction_ms: string;
  /** Q96.32 dynamic threshold at fetch-time (recomputed each call).
   *  Decimal string, same rationale as `total_conviction_ms`. */
  threshold_conviction_ms: string;
  half_life_seconds: number;
  auto_exec: AutoExecAction;
  /** Total members eligible at PollCreate.hlc (frozen). */
  total_supply: number;
  /** Active-Signal-true voter count right now. */
  voter_count: number;
  /** Caller's own latest Signal direction: true = supporting, false =
   *  withdrawn, undefined = never signaled (or self-identity unavailable). */
  your_signal?: boolean;
  /** Wall-clock (ms since UNIX_EPOCH) when total_conviction first
   *  crossed threshold_conviction. Undefined if never crossed (or
   *  reverted by an unsignal). */
  threshold_reached_at_ms?: number;
}

/** Convert raw Q96.32 conviction values (decimal-string i128s from the
 *  Rust IPC layer) to a 0-100 progress percentage for UI bars. Returns
 *  0 when `threshold` is non-positive (defensive — the Rust apply path
 *  enforces `T_max > T_min ≥ 0`, but a peer with a malformed
 *  Tier2PollConfig could conceivably ship 0 across the wire). Caps at
 *  100 so the bar never overflows the track.
 *
 *  Uses BigInt throughout because raw Q96.32 conviction values
 *  routinely exceed `Number.MAX_SAFE_INTEGER`; `*1000n / threshold`
 *  preserves one decimal place of precision in integer math, then we
 *  cast to Number for the final `/10` (the ratio fits in a double). */
export function convictionPercent(total: string, threshold: string): number {
  const totalBI = BigInt(total);
  const thresholdBI = BigInt(threshold);
  if (thresholdBI <= 0n) return 0;
  const tenthsBI = (totalBI * 1000n) / thresholdBI;
  const tenths = Number(tenthsBI);
  // Lower-bound clamp keeps the bar non-negative if a malformed or
  // hostile payload ships a negative total (the Q96.32 fields are i128
  // so negative values are encodable). CR R3 Minor.
  return Math.max(0, Math.min(100, tenths / 10));
}

// ─── Tier 2 Tauri event payloads ───────────────────────────────────────
// These are camelCased — the Rust payload structs use
// `#[serde(rename_all = "camelCase")]` (see
// `VotingTier2ProposalCreatedPayload`, `VotingTier2SignalCastPayload`
// in lib.rs). The tick-emitted events (`voting-threshold-reached`,
// `voting-proposal-finalized`) are built ad-hoc via `serde_json::json!`
// in community_voting_tick.rs and also use camelCase keys.

/** Payload for `voting-tier2-proposal-created` event. */
export interface VotingTier2ProposalCreatedPayload {
  proposalId: string;
  communityId: string;
}

/** Payload for `voting-tier2-signal-cast` event. */
export interface VotingTier2SignalCastPayload {
  proposalId: string;
  voter: string;
  support: boolean;
}

/** Payload for `voting-threshold-reached` event (tick-emitted). */
export interface VotingThresholdReachedPayload {
  communityId: string;
  proposalId: string;
  thresholdReachedAtMs: number;
}

/** Payload for `voting-threshold-reverted` event (tick-emitted when a
 *  Tier 2 proposal drops back below threshold from `ThresholdReached`). */
export interface VotingThresholdRevertedPayload {
  communityId: string;
  proposalId: string;
  revertedAtMs: number;
}

/** Payload for `voting-proposal-finalized` event (tick-emitted). */
export interface VotingProposalFinalizedPayload {
  communityId: string;
  proposalId: string;
}

/** ZEB-292 Phase 3: one Delegate edge in the community delegation graph.
 *  Mirrors the Rust `DelegationEdgeExport` shape exactly. */
export interface DelegationEdgeExport {
  /** 32-char hex `OwnerAddr` of the delegator. */
  from: string;
  /** 32-char hex `OwnerAddr` of the delegate. */
  to: string;
  /** Wall-clock millisecond component of the most recent HLC ordinal
   *  for this delegator's terminal event (Delegate or Undelegate). */
  lastHlcMs: number;
  /** Logical-counter component of the HLC ordinal. */
  lastHlcLogical: number;
}

/** ZEB-292 Phase 3: payload for `voting-delegation-changed` event.
 *  Fired by both local IPCs (`voting_delegate_tier2` /
 *  `voting_undelegate_tier2`) and the engine's inbound apply path so
 *  the UI refreshes regardless of origin. `delegate` is null for
 *  undelegate events. */
export interface VotingDelegationChangedPayload {
  communityId: string;
  /** 32-char hex `OwnerAddr` of the delegator. */
  delegator: string;
  /** 32-char hex `OwnerAddr` of the new delegate, or null on undelegate. */
  delegate: string | null;
}

/** ZEB-292 Phase 3: payload for `voting-delegate-signaled-on-your-behalf`
 *  event. Fired only when (a) the local user has a Delegate edge active
 *  for `communityId`, (b) the signaler IS the delegate, and (c) the
 *  community policy `notifyOnDelegateSignal` is true. */
export interface VotingDelegateSignaledOnYourBehalfPayload {
  communityId: string;
  proposalId: string;
  /** 32-char hex `OwnerAddr` of the delegate who signaled. */
  delegate: string;
  support: boolean;
}

// ─── ZEB-310 Phase 4a-main — Tier 3 (Sortition + STAR) frontend types ─
// Args + event payloads + DTOs for the 6 Tier 3 IPC commands declared in
// src-tauri/src/lib.rs and the 5 Tier 3 events emitted from the engine
// post-apply hook (see ZEB-310 Task 12). All event payload structs carry
// `#[serde(rename_all = "camelCase")]` on the Rust side, so the wire
// keys are camelCase as written here. Hex string IDs are used in this
// surface (not `number[]`) because Tier 3 payloads are built ad-hoc via
// serde_json with the Rust hex helpers — same convention as Tier 2.

/** Args for `createTier3Proposal`. Mirrors the Rust IPC signature 1:1
 *  (`voting_create_tier3_proposal` in src-tauri/src/lib.rs). */
export interface CreateTier3ProposalArgs {
  communityId: string;
  channelId: string;
  proposalText: string;
  sortitionSize: number;
  deliberationWindowSeconds: number;
  draftingWindowSeconds: number;
  ratificationWindowSeconds: number;
  /** 1-char `incentive_mode` tag accepted by the backend
   *  `validate_tier3_poll_config`: 'a' | 'b' | 'c' | 'd'. */
  incentiveMode: 'a' | 'b' | 'c' | 'd';
  minPower: number;
  minVouchingDepth?: number;
  /** Optional prior PollId hex when this proposal retries a failed Tier 3. */
  retryOf?: string;
  /** ZEB-295 Phase 6 Task 9: optional privacy_mode tag. The Rust IPC
   *  accepts `Option<String>` and defaults to "pu" when omitted. Domain
   *  here is restricted to the two end-user choices; 'rf' is reserved. */
  privacyMode?: 'pu' | 'se';
}

/** Payload for `voting-tier3-poll-created` event. */
export interface VotingTier3PollCreatedPayload {
  pollId: string;
  channelId: string;
  communityId: string;
  /** 32-char hex `OwnerAddr` of the proposer. */
  proposer: string;
  sortitionSize: number;
  deliberationWindowSeconds: number;
  draftingWindowSeconds: number;
  ratificationWindowSeconds: number;
}

/** Payload for `voting-tier3-sortition-complete` event. Lists the
 *  selected mini-public (primary) and the backup pool (secondary). */
export interface VotingTier3SortitionCompletePayload {
  pollId: string;
  communityId: string;
  /** 32-char hex `OwnerAddr` strings — selected mini-public members. */
  primary: string[];
  /** 32-char hex `OwnerAddr` strings — backup pool members. */
  backup: string[];
}

/** Payload for `voting-tier3-drafting-open` event. */
export interface VotingTier3DraftingOpenPayload {
  pollId: string;
  communityId: string;
}

/** One draft candidate in the ratification ordering. Embedded in
 *  `VotingTier3RatificationOpenPayload.candidateOrdering`. */
export interface CandidateRef {
  /** 64-char hex of the 32-byte event hash of the kd=dc candidate event. */
  eventHash: string;
  text: string;
  approvalCount: number;
}

/** Payload for `voting-tier3-ratification-open` event. Carries the
 *  canonical candidate ordering (ratifiers score by index into this
 *  list, so order MUST be deterministic across peers). */
export interface VotingTier3RatificationOpenPayload {
  pollId: string;
  communityId: string;
  candidateOrdering: CandidateRef[];
}

/** Per-candidate score summary in `VotingTier3FinalizedPayload`. */
export interface CandidateScore {
  /** 64-char hex of the 32-byte event hash of the kd=dc candidate event. */
  eventHash: string;
  /** Sum of all STAR scores (0..=5) cast for this candidate across all
   *  ratification ballots. */
  totalScore: number;
  /** Number of runoff ballots this candidate received (STAR runoff
   *  phase between the two top scorers). */
  runoffVotes: number;
}

/** Payload for `voting-tier3-finalized` event. */
export interface VotingTier3FinalizedPayload {
  pollId: string;
  communityId: string;
  /** 64-char hex of the winning candidate's kd=dc event hash. */
  winnerEventHash: string;
  winnerText: string;
  /** Runner-up event hash when present (STAR always identifies a
   *  runoff opponent unless there's only one candidate). */
  runnerUpEventHash?: string;
  scoresSummary: CandidateScore[];
}

// ─── ZEB-294 — Tier 3 deliberation types ────────────────────────────────
// Mirror the Rust `DeliberationStatementExport`, `MyDeliberationVoteExport`,
// and `BridgingScoreExport` DTOs. All fields arrive camelCased (Rust serde
// `rename_all = "camelCase"`).

/** Wire vote code for kd=dv DeliberationVote events. */
export type DeliberationVoteCode = 'agree' | 'disagree' | 'pass';

/** ZEB-294: one deliberation statement with aggregate vote counts.
 *  Mirrors Rust `DeliberationStatementExport`. */
export interface DeliberationStatementExport {
  /** 64-char hex SHA-256 of the signing bytes of the kd=ds event. */
  statementEventHash: string;
  /** 32-char hex (OwnerAddr is 16 bytes). */
  author: string;
  text: string;
  createdAtHlcMs: number;
  /** ZEB-790: full-tuple tiebreak (optional — absent in older fixtures). */
  createdAtHlcLogical?: number;
  createdAtHlcDeviceId?: string;
  agreeCount: number;
  disagreeCount: number;
  passCount: number;
}

/** ZEB-294: caller's vote on a single deliberation statement.
 *  Mirrors Rust `MyDeliberationVoteExport`. */
export interface MyDeliberationVoteExport {
  statementEventHash: string;
  vote: DeliberationVoteCode;
}

/** ZEB-294: bridging-score row for a deliberation statement.
 *  Mirrors Rust `BridgingScoreExport`. Returned by
 *  `adapter.listBridgingStatements(pollId, topN)`. */
export interface BridgingScoreExport {
  /** 64-char hex SHA-256 of the signing bytes of the kd=ds event. */
  statementEventHash: string;
  statementText: string;
  /** 32-char hex (OwnerAddr is 16 bytes). */
  author: string;
  agreeCount: number;
  disagreeCount: number;
  passCount: number;
  /** Q32 fixed-point u64 as decimal string. Frontend renders as 0..1 float
   *  for the heat bar — never used for sort. */
  diversityQ32: string;
  /** Q64 fixed-point u64 as decimal string. Sort key. */
  bridgingScoreQ64: string;
}

/** Tauri event payload for `voting-tier3-deliberation-statement-created`.
 *  Fired post-apply for every accepted kd=ds event. */
export interface Tier3DeliberationStatementCreatedPayload {
  pollId: string;
  statementEventHash: string;
  author: string;
  text: string;
  createdAtHlcMs: number;
}

/** Tauri event payload for `voting-tier3-deliberation-vote-cast`.
 *  Fired post-apply for every accepted kd=dv event. */
export interface Tier3DeliberationVoteCastPayload {
  pollId: string;
  statementEventHash: string;
  voter: string;
  vote: DeliberationVoteCode;
}

// ─── ZEB-311 Phase 4a-main — Tier 3 pull-IPC types ─────────────────────
// Mirror the Rust Tier3PollExport + Tier3PollSummary DTOs produced by
// `voting_get_tier3_poll` and `voting_list_tier3_polls`. All fields
// arrive camelCased (Rust serde `rename_all = "camelCase"`).

/** ZEB-311 — Tier 3 stage tag. Wire-encoded as 2-char strings. */
export type Tier3Stage = 'so' | 'de' | 'dr' | 'ra' | 'fi' | 'fa';

/** ZEB-311 — caller's role for a Tier 3 poll. snake_case on the wire. */
export type Tier3MyRole = 'proposer' | 'mini_public' | 'backup' | 'observer';

/** ZEB-311 — one draft candidate visible to the frontend. */
export interface DraftCandidateExport {
  /** 64-char hex of the SHA-256 of the kd=dc event's signing bytes. */
  eventHash: string;
  text: string;
  /** 64-char hex OwnerAddr; null for the synthetic status_quo. */
  proposer: string | null;
  approvalCount: number;
}

/** ZEB-311 — one ratification candidate. */
export interface RatificationCandidateExport {
  eventHash: string;
  text: string;
}

/** ZEB-311 — full Tier 3 poll state for the UI. Returned by
 *  `adapter.getTier3Poll(pollId)`. */
export interface Tier3PollExport {
  pollId: string;
  communityId: string;
  proposalText: string;
  proposer: string;
  stage: Tier3Stage;
  /** HLC wall_ms projection. */
  pollCreateHlcMs: number;
  /** ZEB-790: full-tuple tiebreak (optional — absent in older fixtures). */
  pollCreateHlcLogical?: number;
  pollCreateHlcDeviceId?: string;
  sortitionSize: number;
  deliberationWindowSeconds: number;
  draftingWindowSeconds: number;
  ratificationWindowSeconds: number;
  /** 1-char `incentive_mode` tag from `validate_tier3_poll_config`. */
  incentiveMode: 'a' | 'b' | 'c' | 'd';
  miniPublic: string[];
  backupPool: string[];
  /** Tuples of (ownerHex, hlcMs). */
  declined: [string, number][];
  draftCandidates: DraftCandidateExport[];
  ratificationCandidates: RatificationCandidateExport[];
  myRole: Tier3MyRole;
  myDraftingApprovals: string[];
  myRatificationScores: number[] | null;
  /** ZEB-294: deliberation statements with aggregate vote counts.
   *  Ordered by statementEventHash ascending (BTreeMap iteration order). */
  deliberationStatements: DeliberationStatementExport[];
  /** ZEB-294: caller's accepted-statement count (for composer 5-cap UX). */
  myDeliberationStatementCount: number;
  /** ZEB-294: caller's per-statement votes; entry exists only for
   *  statements the caller has voted on. */
  myDeliberationVotes: MyDeliberationVoteExport[];
  winnerEventHash: string | null;
  runnerUpEventHash: string | null;
  /** ZEB-295: privacy mode for ratification ballots.
   *   - 'pu' = Public (default): ballots are visible per-voter.
   *   - 'se' = Ballot-secret: ballots are encrypted, tally revealed by
   *     threshold committee decryption after the ratification window.
   *   - 'rf' = Reserved future tag (Rust side accepts the string but no
   *     production code path yet uses it). */
  privacyMode: 'pu' | 'se' | 'rf';
  /** ZEB-295 (se-mode only): committee members who have published shares
   *  at the latest epoch. Always 0 in pu-mode polls. */
  encryptedTallyShareCount: number;
  /** ZEB-295 (se-mode only): threshold t at the latest committee epoch.
   *  Always 0 in pu-mode polls or when no committee is yet active. */
  encryptedTallyThreshold: number;
  /** ZEB-295 (se-mode only): committee size n at the latest committee
   *  epoch. Always 0 in pu-mode polls or when no committee is yet active. */
  encryptedTallyCommitteeSize: number;
}

/** ZEB-311 — list-row shape. Lightweight; no candidate details. */
export interface Tier3PollSummary {
  pollId: string;
  communityId: string;
  proposalText: string;
  proposer: string;
  stage: Tier3Stage;
  pollCreateHlcMs: number;
  /** ZEB-790: full-tuple tiebreak (optional — absent in older fixtures). */
  pollCreateHlcLogical?: number;
  pollCreateHlcDeviceId?: string;
  sortitionSize: number;
  winnerText: string | null;
  /** ZEB-295: privacy mode tag — lets the list view render the 🔒 chip
   *  without a full poll-detail fetch. Same three-tag domain as
   *  `Tier3PollExport.privacyMode`. */
  privacyMode: 'pu' | 'se' | 'rf';
}

/** ZEB-295: emitted on every accepted kd=ts so the frontend can update
 *  incremental committee-share-count progress on the awaiting-tally
 *  ratification view.
 *
 *  `epoch` is widened to `u64` on the Rust side (CodeAnt PR #155 critical
 *  fix for the u32/u64 epoch mismatch). JavaScript can lossless-represent
 *  integers up to 2^53−1; CHURP rotations are not expected to cross that
 *  for any deployed community, so `number` remains the safe shape. If we
 *  ever need exact-u64, Tauri's serde JSON encoder handles `number ↔ u64`
 *  for values ≤ Number.MAX_SAFE_INTEGER; values above would need a string
 *  encoding migration. */
export interface Tier3TallyShareAppliedPayload {
  communityId: string;
  pollId: string;
  epoch: number;
  shareCount: number;
  threshold: number;
}

/**
 * ZEB-319: payload of `voting-tier3-mini-public-decline` Tauri event.
 * Emitted when a kd=md MiniPublicDecline event is successfully applied
 * to a Tier 3 poll's projection (backup promotion may follow).
 */
export interface Tier3MiniPublicDeclinePayload {
  pollId: string;
  communityId: string;
  decliner: string;
  declineHlcMs: number;
}

/**
 * ZEB-319: payload of `voting-tier3-draft-candidate` Tauri event.
 * Emitted when a kd=dc DraftCandidate event is successfully applied
 * to a Tier 3 poll's projection.
 */
export interface Tier3DraftCandidatePayload {
  pollId: string;
  communityId: string;
  proposer: string;
  eventHash: string;
  candidateText: string;
}

/**
 * ZEB-319: payload of `voting-tier3-draft-approval` Tauri event.
 * Emitted when a kd=da DraftApproval event is successfully applied
 * to a Tier 3 poll's projection.
 */
export interface Tier3DraftApprovalPayload {
  pollId: string;
  communityId: string;
  approver: string;
  targetEventHash: string;
}

/** Stage label for UI display. */
export function tier3StageLabel(s: Tier3Stage): string {
  switch (s) {
    case 'so': return 'Sortition';
    case 'de': return 'Deliberation';
    case 'dr': return 'Drafting';
    case 'ra': return 'Ratification';
    case 'fi': return 'Finalized';
    case 'fa': return 'Failed';
  }
}
