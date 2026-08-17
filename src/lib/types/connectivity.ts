/**
 * ZEB-321 Phase 1: connectivity-types frontend.
 *
 * Mirrors the camelCase DTOs emitted by the Rust IPCs in
 * `src-tauri/src/lib.rs` (`ReachabilityRecordDto` and `PeerReachabilityDto`).
 *
 * Field-shape rationale and provenance:
 *   * `irohNodeId`   — hex(64) lowercase, 32 raw bytes of the iroh `EndpointId`.
 *   * `homeRelayUrl` — stringified DERP URL; empty string when no relay is
 *                     negotiated yet (or relay mode is disabled).
 *   * `directAddresses` — `ip:port` string forms from iroh's local-addr probe.
 *   * `announcedAtMs`   — wall-clock millis. For peers, copied verbatim from
 *                         their on-wire `ReachabilityAnnouncePayload`; for
 *                         this device's own snapshot it is always 0 in Phase 1
 *                         (the publisher does not track "when did I last
 *                         announce" yet — peer freshness is what the UI cares
 *                         about, and the resolver already exposes it).
 */
export interface ReachabilityRecord {
  irohNodeId: string;
  homeRelayUrl: string;
  directAddresses: string[];
  announcedAtMs: number;
}

/**
 * Peer-keyed wrapper returned by `connectivity_list_peer_reachability`.
 *
 * `ownerAddress` is the 32-char lowercase-hex `OwnerAddr.0` (16 raw bytes).
 * Note: spec docs sometimes refer to this as "hex(16)" — the 16 is the BYTE
 * count, which becomes 32 hex characters on the wire.
 */
export interface PeerReachability {
  ownerAddress: string;
  record: ReachabilityRecord;
  /**
   * Provenance of this entry: `'durableCrdt'` (replicated community-membership
   * CRDT — window-exempt seal-targets) or `'pkarrLive'` (live pkarr cache-back,
   * 15-min windowed). An opaque string for forward-compat. (ZEB-488)
   */
  source: 'durableCrdt' | 'pkarrLive' | string;
}

/**
 * Payload emitted on the `connectivity-reachability-changed` Tauri event by
 * the event_loop delta-consumer hook whenever a `ReachabilityAnnounce`
 * membership event is applied to the LWW resolver.
 *
 * `actor` is hex(OwnerAddr.0) — 32-char lowercase hex.
 */
export interface ConnectivityReachabilityChangedPayload {
  actor: string;
}

// ---------------------------------------------------------------------------
// ZEB-323 Phase 2b: pkarr-backed discovery types
// ---------------------------------------------------------------------------

/**
 * Routing record decoded from a pkarr DHT lookup via
 * `connectivity_discover_identity`. Mirrors the Rust `DiscoveredRecord` DTO
 * (`#[serde(rename_all = "camelCase")]`).
 */
export interface DiscoveredRecord {
  irohNodeId: string;
  relayUrl?: string;
  directAddrs: string[];
  announcedAtMs: number;
}

/**
 * Snapshot of the pkarr publisher's active publication handles, returned by
 * `connectivity_pkarr_publication_status`. Mirrors `PublicationStatus` DTO.
 */
export interface PkarrPublicationStatus {
  inviteCount: number;
  identityActive: boolean;
  communityCount: number;
}

/**
 * Result of `connectivity_redeem_invite_iroh`. Mirrors `RedemptionOutcome` DTO.
 *
 * `status` values:
 *  - `'joined'` — ZEB-325 Phase 2c: full handshake completed (pkarr resolve
 *    → iroh connect → PendingJoin → counter-signed Join applied). The
 *    `communityId` is set to the joined community.
 *  - `'pkarr_resolved_no_handshake'` — pkarr resolved but the inner redeem
 *    handshake failed; defensive fallback (Phase 2c is meant to eliminate
 *    this; surfaced if `redeem_invite_inner` errs after a successful seed).
 *  - `'inviter_unreachable'` — pkarr lookup returned nothing OR the post-
 *    seed redeem path failed (backend collapses both into one user-facing
 *    message).
 *  - `'no_member_reachable'` — ZEB-911: the admin dial failed AND the
 *    witness ladder dialed at least one rendezvous-resolved community
 *    member, all unreachable. Same retry semantics as
 *    `'inviter_unreachable'`; distinct so the copy names the community,
 *    not the inviter.
 *  - `'join_failed'` — ZEB-325 PR #159 F1: the inviter WAS reached and a
 *    valid JoinCountersign was delivered, but the subsequent local
 *    `redeem_invite_inner_with_overrides` errored (engine insert, fence,
 *    commit rollback, etc.). `communityId` is set so the frontend can
 *    surface "we found Alice but the local insert failed".
 *  - `'missing_inviter_identity_pub'` — invite has no inviter identity key for
 *    verification; cannot safely complete discovery.
 *  - `'fallback_reticulum'` — use the LAN Reticulum path instead.
 *  - An opaque backend string for future variants.
 */
export interface RedemptionOutcome {
  status:
    | 'joined'
    | 'pkarr_resolved_no_handshake'
    | 'inviter_unreachable'
    | 'no_member_reachable'
    | 'join_failed'
    | 'missing_inviter_identity_pub'
    | 'fallback_reticulum'
    | string;
  communityId?: string;
  /**
   * ZEB-902: for a `'joined'` outcome, whether the join landed as a *pending*
   * member (a deposited PendingJoin still awaiting the admin's countersign)
   * rather than a fully-ratified member. Mirrors the `dto.pending` flag the
   * LAN redeem path already surfaces (ZEB-254). Always `false`/absent for the
   * non-`'joined'` statuses. Optional for defensive parsing (older payloads /
   * mocks) — treat missing as `false`. Lets the dialog show an honest
   * terminal state ("Join request sent — unlocks once the admin approves")
   * instead of a flat "Joined ✓".
   */
  pending?: boolean;
}

/**
 * The stage a cross-WAN invite redemption is currently at, emitted on the
 * `connectivity-invite-resolution-progress` event.
 */
export type RedemptionStage =
  | 'resolving'
  | 'connecting'
  | 'sending'
  | 'awaiting_countersig'
  | 'joined';

/**
 * Payload of the `connectivity-invite-resolution-progress` event.
 */
export interface ResolutionProgressEvent {
  inviteId: string;
  stage: RedemptionStage;
  attemptN: number;
}

/**
 * ZEB-650 slice 3: pure-local invite preview (mirror of the Rust
 * `InvitePreviewDto`). `inviterVerified` is scoped to the inviter
 * authorization chain — it does NOT authenticate the payload's name, which
 * is why the preview carries no member/channel counts. `inviterDisplayName`
 * is always null today (local name resolution is engine-async); consumers
 * fall back to `inviterFingerprint`.
 */
export interface InvitePreviewDto {
  communityName: string;
  isInviteOnly: boolean;
  inviterVerified: boolean;
  inviterFingerprint: string | null;
  inviterDisplayName: string | null;
  expired: boolean;
}
