/**
 * ZEB-662 — mention notifications. Subscribes (via App wiring) to the per-message
 * hook, detects self-mentions (DTO `mentions` field), classifies them `loud`,
 * resolves an action through the existing NotificationService policy engine, and
 * delivers across the nav mention indicator + toast/OS-notification rails,
 * focus-aware. All side effects are injected → deterministic + unit-testable.
 * Mirrors the incoming-call-alert.ts dep-injection + default-factory pattern.
 */
import type { NotificationAction } from './types';
import type { ChannelMessageDto } from './channel-message-service';
import { messageMentionsOwner } from './mention-detect';

export interface MentionAlertDeps {
  getSelfOwnerId(): string | undefined;
  /** True iff the viewer is looking right at this exact community channel's
   *  feed (community channels live in CommunityView state, not NavService, so
   *  this can't be a bare channel-id compare). */
  isActiveChannel(communityId: string, channelId: string): boolean;
  isFocused(): boolean | Promise<boolean>;
  resolve(
    priority: 'quiet' | 'standard' | 'loud',
    peerAddress: string,
    communityId?: string,
  ): NotificationAction;
  incMention(communityId: string, channelId: string): void;
  /** Human-readable channel name for the notification body (fallback allowed). */
  getChannelName(communityId: string, channelId: string): string;
  showToast(message: string): void;
  sendOsNotification(o: { title: string; body: string }): void;
}

const NOTIFY_ACTIONS: ReadonlySet<NotificationAction> = new Set([
  'notify',
  'sound',
  'break_dnd',
]);

export class MentionAlertService {
  constructor(private deps: MentionAlertDeps) {}

  async onMessage(
    communityId: string,
    channelId: string,
    message: ChannelMessageDto,
  ): Promise<void> {
    const self = this.deps.getSelfOwnerId();
    if (!self || !messageMentionsOwner(message, self)) return;
    // Never self-notify for your own message that happens to @-mention you.
    if (message.author === self) return;

    // Resolve focus once per mention (reused by the seen-check and delivery).
    const focused = await this.focusedSafe();
    // Looking right at it → treat as seen.
    if (focused && this.deps.isActiveChannel(communityId, channelId)) return;

    const action = this.deps.resolve('loud', message.author, communityId);
    if (action === 'silent') return;

    // dot_only and above: always record the nav indicator.
    this.deps.incMention(communityId, channelId);
    if (!NOTIFY_ACTIONS.has(action)) return;

    const title = 'New mention';
    const body = `You were mentioned in ${this.deps.getChannelName(communityId, channelId)}`;
    if (focused) {
      this.deps.showToast(body);
    } else {
      try {
        this.deps.sendOsNotification({ title, body });
      } catch {
        /* OS unavailable — nav dot already set */
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

/** App-supplied deps (everything except the Tauri focus/notify capabilities). */
export type MentionAlertAppDeps = Omit<MentionAlertDeps, 'isFocused' | 'sendOsNotification'>;

/**
 * Build a MentionAlertService wired to the real Tauri window/notification APIs.
 * Outside Tauri (web preview / tests) isFocused defaults to true and OS notify
 * is a no-op, so callers need no guard. Mirrors createDefaultIncomingCallAlerter.
 */
export async function createDefaultMentionAlerter(
  appDeps: MentionAlertAppDeps,
): Promise<MentionAlertService> {
  const { isTauri } = await import('@tauri-apps/api/core');
  if (!isTauri()) {
    return new MentionAlertService({
      ...appDeps,
      isFocused: () => true,
      sendOsNotification: () => {},
    });
  }
  const notif = await import('@tauri-apps/plugin-notification');
  const { getCurrentWebviewWindow } = await import('@tauri-apps/api/webviewWindow');
  const appWin = getCurrentWebviewWindow();
  return new MentionAlertService({
    ...appDeps,
    isFocused: () => appWin.isFocused(),
    sendOsNotification: (o) => notif.sendNotification(o),
  });
}
