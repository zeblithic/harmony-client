/**
 * ZEB-714 — DTO shapes for the admin-recovery IPC surface
 * (`get_recovery_state` / `set_recovery_designates` /
 * `initiate_admin_recovery` / `cosign_admin_recovery` /
 * `veto_admin_recovery`). Mirrors the Rust `RecoveryStateDto` family in
 * `src-tauri/src/lib.rs` (serde camelCase).
 *
 * Vocabulary note (spec §8): this is COMMUNITY admin recovery — replacing
 * a lost community admin identity. Keep all user-facing copy verbally
 * distinct from fleet/device recovery ("your devices").
 */

export type RecoveryPhase =
  | 'collecting'
  | 'timeLocked'
  | 'executed'
  | 'vetoed'
  | 'expired'
  | 'configChanged'
  | 'superseded'
  | 'stalled';

export interface RecoveryConfigDto {
  designateAddrs: string[];
  threshold: number;
  vetoWindowMs: number;
}

export interface RecoveryProposalDto {
  proposalEventId: string;
  proposerAddr: string;
  lostAdminAddr: string;
  newAdminAddr: string;
  signerAddrs: string[];
  signersSoFar: number;
  threshold: number;
  proposedAtWallMs: number;
  deadlineMs: number | null;
  phase: RecoveryPhase;
  /** Set iff phase === 'vetoed'. */
  vetoedByAddr: string | null;
  /** For an executed proposal: deadline + F (48 h finality margin) —
   *  before this wall clock the UI shows "rotation pending finality". */
  rotationEligibleAtMs: number | null;
  selfHasCosigned: boolean;
}

export interface RecoveryStateDto {
  config: RecoveryConfigDto | null;
  proposals: RecoveryProposalDto[];
  selfIsDesignate: boolean;
  selfPower: number;
}

export interface InitiateRecoveryResult {
  proposalEventId: string;
  signersSoFar: number;
  threshold: number;
}

export interface RecoveryCosignResult {
  signersAfter: number;
  threshold: number;
  reachedThreshold: boolean;
}

/** Phases that render as an active (non-terminal) recovery banner. */
export function isActiveRecoveryPhase(phase: RecoveryPhase): boolean {
  return phase === 'collecting' || phase === 'timeLocked';
}
