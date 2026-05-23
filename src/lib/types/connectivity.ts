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
