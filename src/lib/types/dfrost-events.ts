// ZEB-305: TypeScript contracts for D-FROST Tauri events.
//
// Payload field naming follows Tauri's snake_case → camelCase boundary
// auto-conversion: the Rust struct `pub ceremony_id: String` with
// `#[serde(rename_all = "camelCase")]` becomes `ceremonyId` in the
// emitted JSON payload.
//
// Pairs with the Rust IPC handlers in src-tauri/src/lib.rs:
// - dfrost_initiate_dkg / dfrost_contribute_dkg_round emit DfrostDkgProgressPayload
// - dfrost_contribute_threshold_sign emits DfrostBeaconReadyPayload on threshold reached
// - dfrost_propose_refresh / dfrost_contribute_refresh_round emit
//   DfrostRefreshProgressPayload (ZEB-1027: rounds 1-3)
// - dfrost_request_share_repair / dfrost_contribute_repair_round emit
//   DfrostRepairProgressPayload (ZEB-1027)

/** Fires on every `dr` (DkgRound) event apply. `roundNum` is 1, 2, or 3. */
export interface DfrostDkgProgressPayload {
  communityId: string;      // hex(SpaceId), 32 chars — ZEB-1018 filter key
  ceremonyId: string;       // hex(ceremony_id), 64 chars
  roundNum: number;
  participantsSoFar: number;
}

/** Fires when a `vb` (VrfBeacon) event is applied — threshold sig aggregated. */
export interface DfrostBeaconReadyPayload {
  communityId: string;      // hex(SpaceId), 32 chars — ZEB-1018 filter key
  ceremonyId: string;       // hex(ceremony_id), 64 chars
  vrfOutput: string;        // hex(vrf_output), 64 chars
}

/** Fires on every `rf` (ProactiveRefresh) event apply. `roundNum` is 1-3 (3 = the finalizing `dk`). */
export interface DfrostRefreshProgressPayload {
  communityId: string;      // hex(SpaceId), 32 chars — ZEB-1018 filter key
  ceremonyId: string;       // hex(ceremony_id), 64 chars
  roundNum: number;
}

/** ZEB-1027: fires on every `rp` (RepairShare) event apply. */
export interface DfrostRepairProgressPayload {
  communityId: string;      // hex(SpaceId), 32 chars — ZEB-1018 filter key
  ceremonyId: string;       // hex(ceremony_id), 64 chars
  roundNum: number;         // 1 = request, 2 = helper deltas, 3 = helper sigma
  participant: string;      // hex(OwnerAddr) of the member being repaired
}

/**
 * Tauri event names. Use these as the first argument to `listen()`:
 *
 *   import { listen } from '@tauri-apps/api/event';
 *   import { DFROST_DKG_PROGRESS, type DfrostDkgProgressPayload } from '$lib/types/dfrost-events';
 *
 *   const unlisten = await listen<DfrostDkgProgressPayload>(
 *     DFROST_DKG_PROGRESS,
 *     (event) => console.log('DKG progress', event.payload.ceremonyId)
 *   );
 */
export const DFROST_DKG_PROGRESS = 'dfrost-dkg-progress' as const;
export const DFROST_BEACON_READY = 'dfrost-beacon-ready' as const;
export const DFROST_REFRESH_PROGRESS = 'dfrost-refresh-progress' as const;
export const DFROST_REPAIR_PROGRESS = 'dfrost-repair-progress' as const;
