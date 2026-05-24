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
 *  - `'join_failed'` — ZEB-325 PR #159 F1: the inviter WAS reached and a
 *    valid JoinCountersign was delivered, but the subsequent local
 *    `redeem_invite_inner_with_overrides` errored (engine insert, fence,
 *    commit rollback, etc.). `communityId` is set so the frontend can
 *    surface "we found Alice but the local insert failed".
 *  - `'missing_admin_identity_pub'` — invite has no admin identity key for
 *    verification; cannot safely complete discovery.
 *  - `'fallback_reticulum'` — use the LAN Reticulum path instead.
 *  - An opaque backend string for future variants.
 */
export interface RedemptionOutcome {
  status:
    | 'joined'
    | 'pkarr_resolved_no_handshake'
    | 'inviter_unreachable'
    | 'join_failed'
    | 'missing_admin_identity_pub'
    | 'fallback_reticulum'
    | string;
  communityId?: string;
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
