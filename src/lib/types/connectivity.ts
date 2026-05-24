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
 *  - `'pkarr_resolved_no_handshake'` — inviter found on the DHT; full join
 *    (iroh connect + counter-sig) is Phase 2c (ZEB-323 §7.2). NOT "joined".
 *  - `'inviter_unreachable'` — pkarr lookup returned nothing.
 *  - `'missing_admin_identity_pub'` — invite has no admin identity key for
 *    verification; cannot safely complete discovery.
 *  - `'fallback_reticulum'` — use the LAN Reticulum path instead.
 *  - An opaque backend string for future variants.
 *
 * NOTE: `'joined'` is intentionally NOT a valid status from this IPC.
 * This command resolves pkarr records only; community join state is only
 * mutated by the `redeem_invite` IPC (Reticulum path).
 */
export interface RedemptionOutcome {
  status: 'pkarr_resolved_no_handshake' | 'inviter_unreachable' | 'missing_admin_identity_pub' | 'fallback_reticulum' | string;
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
