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
  pollId: PollIdHex;
  channelId: ChannelIdHex;
  communityId: CommunityIdHex;
}

/** Payload for `voting-ballot-cast` Tauri event. */
export interface VotingBallotCastPayload {
  pollId: PollIdHex;
  voter: OwnerAddrHex;
  approvedCount: number;
}

/**
 * Payload for `voting-poll-closed` Tauri event. Phase 1: the backend
 * does NOT actually emit this event (lifecycle transitions to Closed
 * land in Phase 1.5 with the Zenoh auto-close tick). We wire the
 * listener now for forward compatibility.
 */
export interface VotingPollClosedPayload {
  pollId: PollIdHex;
  communityId: CommunityIdHex;
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
  return Math.min(100, tenths / 10);
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

/** Payload for `voting-proposal-finalized` event (tick-emitted). */
export interface VotingProposalFinalizedPayload {
  communityId: string;
  proposalId: string;
}
