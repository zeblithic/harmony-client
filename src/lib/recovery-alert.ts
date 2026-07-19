/**
 * ZEB-714 — power-100 escalation for community admin-recovery proposals
 * (spec §5.4: admins get the OS-notification treatment in BOTH the
 * collecting and time-locked phases; the banner alone is the surface for
 * everyone else). Deduped per (community, proposal, phase) for the
 * session so each phase entry notifies once. All side effects injected —
 * mirrors the mention-alert / incoming-call-alert DI + default-factory
 * pattern.
 *
 * Copy rule (spec §8): community-admin recovery, never device/fleet
 * recovery — no "your devices" vocabulary here.
 */
import type { RecoveryStateDto } from './recovery-types';
import { isActiveRecoveryPhase } from './recovery-types';

export interface RecoveryAlertDeps {
  isFocused(): boolean | Promise<boolean>;
  sendOsNotification(o: { title: string; body: string }): void;
  showToast(message: string): void;
}

export class RecoveryAlertService {
  /** `${communityId}:${proposalId}:${phase}` seen this session. */
  private seen = new Set<string>();

  constructor(private deps: RecoveryAlertDeps) {}

  /**
   * Feed a freshly-fetched recovery state (App fetches on every
   * `community-recovery-changed` event). No-op below power 100 — the
   * DTO's `selfPower` comes from the same materialized view as the
   * proposals, so the check needs no roster access.
   */
  async onRecoveryState(
    communityId: string,
    communityName: string,
    state: RecoveryStateDto,
  ): Promise<void> {
    if (state.selfPower < 100) return;
    for (const p of state.proposals) {
      if (!isActiveRecoveryPhase(p.phase)) continue;
      const key = `${communityId}:${p.proposalEventId}:${p.phase}`;
      if (this.seen.has(key)) continue;
      this.seen.add(key);

      const body =
        p.phase === 'collecting'
          ? (() => {
              const remaining = Math.max(0, p.threshold - p.signersSoFar);
              return `Admin recovery proposed in ${communityName} — ${remaining} more signature${remaining === 1 ? '' : 's'} needed. As an admin, you can veto.`;
            })()
          : p.deadlineMs !== null
            ? `Admin recovery in ${communityName} executes on ${new Date(p.deadlineMs).toLocaleDateString()} unless an admin vetoes.`
            : `Admin recovery in ${communityName} executes once its veto window closes unless an admin vetoes.`;

      if (await this.focusedSafe()) {
        this.deps.showToast(body);
      } else {
        try {
          this.deps.sendOsNotification({ title: 'Community admin recovery', body });
        } catch {
          /* OS notification unavailable — the banner still shows */
        }
      }
    }
  }

  /** Focus query, defaulting to focused on failure (prefer in-app toast). */
  private async focusedSafe(): Promise<boolean> {
    try {
      return await this.deps.isFocused();
    } catch {
      return true;
    }
  }
}

export type RecoveryAlertAppDeps = Omit<RecoveryAlertDeps, 'isFocused' | 'sendOsNotification'>;

/**
 * Build a RecoveryAlertService wired to the real Tauri window/notification
 * APIs. Outside Tauri (web preview / tests) isFocused defaults to true and
 * OS notify is a no-op. Mirrors createDefaultMentionAlerter.
 */
export async function createDefaultRecoveryAlerter(
  appDeps: RecoveryAlertAppDeps,
): Promise<RecoveryAlertService> {
  const { isTauri } = await import('@tauri-apps/api/core');
  if (!isTauri()) {
    return new RecoveryAlertService({
      ...appDeps,
      isFocused: () => true,
      sendOsNotification: () => {},
    });
  }
  const notif = await import('@tauri-apps/plugin-notification');
  const { getCurrentWebviewWindow } = await import('@tauri-apps/api/webviewWindow');
  const appWin = getCurrentWebviewWindow();
  return new RecoveryAlertService({
    ...appDeps,
    isFocused: () => appWin.isFocused(),
    sendOsNotification: (o) => notif.sendNotification(o),
  });
}
