/**
 * ZEB-714 — durable dismissal flags for the admin-recovery UI, scoped by
 * `(owner, community)` (localStorage is bundle-scoped, not per-profile —
 * the ZEB-587 lesson). Three flag families:
 *
 * 1. Sole-admin nudge dismissal (spec §5.1) — the "configure recovery
 *    designates" settings prompt is persistent but dismissible.
 * 2. Resolved-banner dismissal — a terminal recovery proposal
 *    (executed / vetoed / expired / stalled) stays visible until the
 *    member dismisses it, keyed per proposal so a NEW proposal always
 *    shows.
 * 3. New-admin reconfigure nudge dismissal (spec §6 T4) — keyed per
 *    executed proposal.
 *
 * All reads/writes are try/caught (storage may be unavailable) and every
 * write fires RECOVERY_FLAGS_CHANGED_EVENT so mounted readers re-derive
 * (the onboarding-backup-flags pattern).
 */

const SOLE_ADMIN_NUDGE = 'harmony.recovery.soleAdminNudgeDismissed';
const RESOLVED_BANNER = 'harmony.recovery.resolvedProposalDismissed';
const RECONFIGURE_NUDGE = 'harmony.recovery.reconfigureNudgeDismissed';

export const RECOVERY_FLAGS_CHANGED_EVENT = 'harmony:recovery-flags-changed';

function scopedKey(base: string, ownerId: string, communityId: string, suffix?: string): string {
  const tail = suffix ? `:${suffix}` : '';
  return `${base}:owner-${ownerId}:community-${communityId}${tail}`;
}

function readFlag(key: string): boolean {
  try {
    return localStorage.getItem(key) === 'true';
  } catch {
    return false;
  }
}

function writeFlag(key: string): void {
  try {
    localStorage.setItem(key, 'true');
  } catch (e) {
    console.debug('[zeb-714] recovery flag write failed:', e instanceof Error ? e.message : String(e));
  }
  try {
    window.dispatchEvent(new Event(RECOVERY_FLAGS_CHANGED_EVENT));
  } catch {
    // Non-DOM environment — persistence still happened.
  }
}

/** Sole-admin "configure recovery designates" settings nudge (spec §5.1). */
export function isSoleAdminNudgeDismissed(ownerId: string, communityId: string): boolean {
  return readFlag(scopedKey(SOLE_ADMIN_NUDGE, ownerId, communityId));
}

export function dismissSoleAdminNudge(ownerId: string, communityId: string): void {
  writeFlag(scopedKey(SOLE_ADMIN_NUDGE, ownerId, communityId));
}

/** Per-proposal dismissal of a resolved (terminal) recovery banner row. */
export function isResolvedProposalDismissed(
  ownerId: string,
  communityId: string,
  proposalEventId: string,
): boolean {
  return readFlag(scopedKey(RESOLVED_BANNER, ownerId, communityId, proposalEventId));
}

export function dismissResolvedProposal(
  ownerId: string,
  communityId: string,
  proposalEventId: string,
): void {
  writeFlag(scopedKey(RESOLVED_BANNER, ownerId, communityId, proposalEventId));
}

/** Per-proposal dismissal of the new-admin "reconfigure designates" nudge. */
export function isReconfigureNudgeDismissed(
  ownerId: string,
  communityId: string,
  proposalEventId: string,
): boolean {
  return readFlag(scopedKey(RECONFIGURE_NUDGE, ownerId, communityId, proposalEventId));
}

export function dismissReconfigureNudge(
  ownerId: string,
  communityId: string,
  proposalEventId: string,
): void {
  writeFlag(scopedKey(RECONFIGURE_NUDGE, ownerId, communityId, proposalEventId));
}
