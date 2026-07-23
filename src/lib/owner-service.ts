import { invoke } from '@tauri-apps/api/core';

export interface OwnerStateView {
  ownerId: string;
  ownerDisplayName: string;
  devices: DeviceView[];
  canBackUp: boolean;
  /** ZEB-668 S5: the fleet's current KeyTree epoch (0 = never rotated). */
  fleetEpoch: number;
  /**
   * ZEB-668 S5: true when any device removal postdates the last key
   * rotation — the removed device still holds decryptable fleet material.
   * Seed-holders (`canBackUp`) get the rotate action; others a passive note.
   */
  fleetEpochStale: boolean;
  /**
   * ZEB-677 S3: whether THIS device holds the master seed. Same value as
   * `canBackUp` today; kept distinct because their semantics diverge —
   * this one gates the quorum-ceremony surfaces. Optional: a stale backend
   * predating S3 omits every quorum field (read as `=== true`).
   */
  selfIsMaster?: boolean;
  /**
   * ZEB-677 S3: pending quorum co-sign requests (unexpired). Optional: a
   * stale backend omits it — read through `?? []`.
   */
  quorumRequests?: QuorumRequestView[];
  /**
   * ZEB-677 S3 (S4 arm flow surface): wall-ms the arm window closes.
   * Optional: a stale backend omits it.
   */
  quorumArmedUntilMs?: number | null;
  /**
   * ZEB-677 S4: whether this device may arm quorum enrollment — true only
   * when the fleet has ≥2 active Master-certed devices (server-computed).
   * Gates the arm affordance entirely: a fleet that can't co-sign an
   * enrollment must show no false promise. Optional: a stale backend
   * predating S4 omits it (read as `=== true`).
   */
  canArmEnrollment?: boolean;
  /**
   * ZEB-721: seconds THIS device's own liveness cert is stamped in the future
   * vs the host clock — the clock regressed behind an already-signed cert,
   * pausing liveness renewal until it recovers. Absent/`undefined` when healthy.
   */
  selfClockRegressedSkewSecs?: number;
}

/**
 * ZEB-677 S3: one pending quorum co-sign request, pre-joined server-side —
 * `canCosign` already encodes the whole eligibility rule; device ids join
 * against `DeviceView.deviceId` for petname display.
 */
export interface QuorumRequestView {
  requestId: string;
  /** "revocation" (S4 adds "enrollment"). */
  kind: string;
  targetDeviceId: string;
  initiatorDeviceId: string;
  reason: string;
  expiresAtMs: number;
  initiatedByMe: boolean;
  signedByMe: boolean;
  declinedByMe: boolean;
  /** ANY device declined — the request is dead (initiator sees "declined"). */
  declined: boolean;
  /** A co-signature has arrived (initiator-side: completion imminent). */
  cosignerSigned: boolean;
  canCosign: boolean;
}

export interface DeviceView {
  deviceId: string;
  displayName: string;
  isThisDevice: boolean;
  trustDecision: TrustDecisionView;
  enrolledAt: number;
  fingerprint: string;
  /** ZEB-418 P2 D17: true iff this device is the owner's pinned butler. */
  butlerPinned: boolean;
  /**
   * 64-hex ed25519 verify key — the SP1 device-id form `setButlerPin`
   * expects. `deviceId` (identity-hash form) is NOT accepted there.
   */
  deviceVkHex: string;
  /**
   * ZEB-668 S2: revocation surface. A revoked device keeps its enrollment
   * row; the panel splits active vs the Removed-devices section on this.
   */
  revoked: boolean;
  /** Unix seconds the revocation was issued; null when not revoked. */
  revokedAt: number | null;
  /** "decommissioned" | "lost" | "compromised" (free text for legacy). */
  revokedReason: string | null;
  /**
   * ZEB-668 S4: fleet-synced petname (LWW; null = never named/cleared).
   * Wins the label ladder over the local device label.
   */
  petName: string | null;
  /**
   * ZEB-668 S4: wall-ms of the last fleet-net heartbeat (~7.5 min cadence).
   * null = never fleet-synced → render NOTHING (honesty rule).
   */
  lastSeenMs: number | null;
  /** ZEB-668 S4: live Connected transport slot for this device right now. */
  connectedNow: boolean;
  /**
   * ZEB-677 S3: this sibling row may be removed via the quorum co-sign
   * ceremony (master seed absent here; this device + one other active
   * sibling hold master-issued certs). Always false on seed-holders.
   * Optional: a stale backend omits it — read as `=== true`.
   */
  quorumRemovable?: boolean;
}

/** ZEB-668 S2: the three UI-selectable revocation reasons (spec §3). */
export type RevokeReason = 'decommissioned' | 'lost' | 'compromised';

export interface TrustDecisionView {
  kind: 'full' | 'provisional' | 'refused';
  reason: string | null;
}

export interface MintIpcResult {
  state: OwnerStateView;
  recoveryToken: string;
}

export interface ExportInfo {
  identityHash: string;
  byteLen: number;
  path: string;
}

export interface ExportSavePathRequest {
  title?: string;
  defaultFilename: string;
  filterName: string;
  filterExtensions: string[];
}

/**
 * Service-class wrapper around the owner-binding Tauri commands.
 *
 * Mirrors `notification-service.ts` pattern: methods + onChange callback
 * for reactive state updates. Error extraction follows the project's
 * memory rule (production rejections are strings; tests emit Errors).
 */
export class OwnerService {
  state: OwnerStateView | null = null;
  onChange?: () => void;

  async refresh(): Promise<void> {
    const view = await invoke<OwnerStateView | null>('get_owner_state');
    this.state = view;
    this.onChange?.();
  }

  /**
   * ZEB-668 S2: revoke a device (self or, from the seed-holder, a sibling).
   * Rejections surface backend prefixes (`notMaster:`, `lastDevice:`) —
   * callers map them to friendly copy. Callers refresh() on success.
   */
  async revoke(deviceVkHex: string, reason: RevokeReason): Promise<void> {
    await invoke('revoke_device', { deviceVkHex, reason });
  }

  /**
   * ZEB-677 S3: master-less removal — writes a co-sign request that a
   * sibling approves; the revocation lands when the co-signature returns.
   * Rejections surface backend prefixes (`noQuorum:`, `duplicateRequest:`,
   * `nodeNotRunning:`); callers map them to friendly copy.
   */
  async requestQuorumRevocation(deviceVkHex: string, reason: RevokeReason): Promise<string> {
    return invoke<string>('request_quorum_revocation', { deviceVkHex, reason });
  }

  /** ZEB-677 S3: approve a sibling's pending co-sign request. */
  async cosignQuorumRequest(requestId: string): Promise<void> {
    await invoke('cosign_quorum_request', { requestId });
  }

  /** ZEB-677 S3: decline a sibling's pending co-sign request (tombstones it). */
  async declineQuorumRequest(requestId: string): Promise<void> {
    await invoke('decline_quorum_request', { requestId });
  }

  /**
   * ZEB-677 S4: arm this device to co-sign one new-device enrollment started
   * from a sibling. Returns the wall-ms the 15-minute window closes
   * (`armedUntilMs`). Callers refresh on success (the owner-quorum-updated
   * event also fires).
   */
  async armEnrollment(): Promise<number> {
    return invoke<number>('arm_quorum_enrollment');
  }

  /** ZEB-677 S4: cancel an armed enrollment window before it expires. */
  async disarmEnrollment(): Promise<void> {
    await invoke('disarm_quorum_enrollment');
  }

  async mint(): Promise<MintIpcResult> {
    const result = await invoke<MintIpcResult>('mint_owner_identity');
    this.state = result.state;
    this.onChange?.();
    return result;
  }

  async issueRecoveryToken(): Promise<string> {
    const result = await invoke<{ recoveryToken: string }>('issue_owner_recovery_token');
    return result.recoveryToken;
  }

  /**
   * ZEB-196: proactively drop an issued-but-unconsumed recovery token from the
   * server-side cache (e.g. when the user cancels the backup modal), so it
   * can't linger for its 5-minute TTL and LRU-evict a legitimate live token.
   * Idempotent server-side; callers fire it best-effort and needn't await.
   */
  async revokeRecoveryToken(recoveryToken: string): Promise<void> {
    await invoke('revoke_owner_recovery_token', { recoveryToken });
  }

  async exportRecoveryFile(
    recoveryToken: string,
    pathToken: string,
    passphrase: string,
    comment: string | null,
  ): Promise<ExportInfo> {
    return invoke<ExportInfo>('export_owner_recovery_file_to_path', {
      recoveryToken,
      pathToken,
      passphrase,
      comment,
    });
  }

  async requestExportSavePath(req: ExportSavePathRequest): Promise<string | null> {
    return invoke<string | null>('request_export_save_path', { request: req });
  }
}

/** Memory-rule-compliant error extraction for invoke rejections. */
export function extractError(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
