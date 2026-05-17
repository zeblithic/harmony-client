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
