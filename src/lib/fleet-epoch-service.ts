/**
 * ZEB-668 S5 — fleet KeyTree epoch rotation.
 *
 * `bump_fleet_epoch` rotates the fleet's shared KeyTree to the next epoch:
 * new material is sealed per surviving device and distributed through the
 * fleet-keys carrier, so a removed device's retained keys go stale. Only
 * the seed-holding device can rotate (the backend answers "notMaster:"
 * elsewhere); the DevicesPanel gates the affordance on `canBackUp`.
 */
import { invoke } from '@tauri-apps/api/core';

/** Rotate the fleet KeyTree to the next epoch. Resolves to the new epoch. */
export async function bumpFleetEpoch(): Promise<number> {
  return invoke<number>('bump_fleet_epoch');
}

/**
 * ZEB-677 S5 — open a K=2 quorum fleet-epoch rotation on a master-less fleet
 * (the `fleetEpochStale` retry surface when there is no master seed to sign
 * a direct `bump_fleet_epoch`). Resolves to the pending request id; another
 * of your devices must co-sign for the rotation to land.
 */
export async function requestQuorumEpochBump(): Promise<string> {
  return invoke<string>('request_quorum_epoch_bump');
}
