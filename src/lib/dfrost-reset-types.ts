/**
 * ZEB-1031 §9 — DTO shapes for the D-FROST committee-reset admin IPC
 * surface (`get_dfrost_reset_state` / `propose_dfrost_reset` /
 * `cosign_dfrost_reset` / `respond_dfrost_reset` / `relaunch_voided_poll`).
 * Mirrors the Rust `ResetProposalDto` family in `src-tauri/src/lib.rs`
 * (serde camelCase).
 *
 * Vocabulary note (spec §1/§9): this is a D-FROST *committee* reset — the
 * threshold-signing group backing Tier-3 secret ballots + VRF sortition —
 * NOT community admin identity recovery (ZEB-714, `recovery-types.ts`) and
 * not fleet/device recovery. Keep user-facing copy verbally distinct.
 */

export type ResetPhase =
  | 'collecting'
  | 'window'
  | 'authorized'
  | 'consumed'
  | 'vetoed'
  | 'expired'
  | 'lapsed';

export interface ResetProposalDto {
  proposalEventId: string;
  proposerAddr: string;
  targetVk: string;
  targetEpoch: number;
  newMemberAddrs: string[];
  newThreshold: number;
  vetoWindowMs: number;
  signerAddrs: string[];
  proposedAtWallMs: number;
  deadlineMs: number | null;
  authorizedAtMs: number | null;
  /** True iff Authorized was reached via an effective endorse from the
   *  committee (cooperative path) rather than the 48h finality wait
   *  (disaster path) — spec §4.1. */
  endorsed: boolean;
  phase: ResetPhase;
  /** Set iff phase === 'consumed'. */
  consumedNewVk: string | null;
  /** True iff this Consumed verdict was later overridden by proof (a
   *  later veto threshold-signed under `targetVk`) that the old
   *  committee is still alive — spec §6.1 supersession. */
  consumptionSuperseded: boolean;
  selfHasCosigned: boolean;
  /** Admin-quorum value in effect when this proposal was authorized —
   *  NOT the community's live quorum (a later `ChangeQuorum` must not
   *  relabel an already-authorized proposal's signature count). `null`
   *  while `phase === 'collecting'`; render the live quorum then. */
  effectiveQuorum: number | null;
}

export interface ProposeDfrostResetResult {
  proposalEventId: string;
}

export interface CosignDfrostResetResult {
  signersAfter: number;
  phase: ResetPhase;
  reachedThreshold: boolean;
}

/** Phases where the proposal is still open — signature collection or the
 *  post-quorum veto window — and a cosign/endorse/veto response is still
 *  meaningful. */
export function isActiveResetPhase(phase: ResetPhase): boolean {
  return phase === 'collecting' || phase === 'window';
}

/** Phases that are terminal — no further state transition happens. */
export function isTerminalResetPhase(phase: ResetPhase): boolean {
  return phase === 'consumed' || phase === 'vetoed' || phase === 'expired' || phase === 'lapsed';
}

// Spec §8 constants.
export const RESET_VETO_WINDOW_FLOOR_MS = 24 * 60 * 60 * 1000; // 24h
export const RESET_VETO_WINDOW_CEILING_MS = 30 * 24 * 60 * 60 * 1000; // 30d
export const RESET_VETO_WINDOW_DEFAULT_MS = 72 * 60 * 60 * 1000; // 72h
