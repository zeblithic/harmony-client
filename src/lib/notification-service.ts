import type {
  MessagePriority,
  NotificationAction,
  NotificationPolicy,
  NotificationSettings,
  Profile,
  SoundOverrides,
} from './types';

const DEFAULT_POLICY: NotificationPolicy = {
  quiet: 'dot_only',
  standard: 'sound',
  loud: 'break_dnd',
};

export class NotificationService {
  settings: NotificationSettings;

  /** ZEB-662: fired by every mutating setter so a persistence layer can save.
   *  Not fired by `load()` (boot-time hydrate, not a user edit). */
  onChange?: () => void;

  constructor() {
    this.settings = {
      global: { ...DEFAULT_POLICY },
      perCommunity: new Map(),
      perPeer: new Map(),
      perPeerSounds: new Map(),
      perCommunitySounds: new Map(),
    };
  }

  resolve(
    priority: MessagePriority,
    peerAddress: string,
    communityId?: string,
  ): NotificationAction {
    const peerPolicy = this.settings.perPeer.get(peerAddress);
    if (peerPolicy && peerPolicy[priority] !== undefined) {
      return peerPolicy[priority]!;
    }

    if (communityId) {
      const commPolicy = this.settings.perCommunity.get(communityId);
      if (commPolicy && commPolicy[priority] !== undefined) {
        return commPolicy[priority]!;
      }
    }

    return this.settings.global[priority];
  }

  setGlobalPolicy(policy: NotificationPolicy): void {
    this.settings.global = { ...policy };
    this.onChange?.();
  }

  setCommunityPolicy(communityId: string, policy: Partial<NotificationPolicy>): void {
    this.settings.perCommunity.set(communityId, { ...policy });
    this.onChange?.();
  }

  setPeerPolicy(peerAddress: string, policy: Partial<NotificationPolicy>): void {
    this.settings.perPeer.set(peerAddress, { ...policy });
    this.onChange?.();
  }

  clearCommunityPolicy(communityId: string): void {
    this.settings.perCommunity.delete(communityId);
    this.onChange?.();
  }

  clearPeerPolicy(peerAddress: string): void {
    this.settings.perPeer.delete(peerAddress);
    this.onChange?.();
  }

  shouldPlaySound(action: NotificationAction): boolean {
    return action === 'sound' || action === 'break_dnd';
  }

  resolveSoundCid(
    priority: MessagePriority,
    peerAddress: string,
    senderProfile: Profile,
    communityId?: string,
  ): string | undefined {
    const peerSounds = this.settings.perPeerSounds.get(peerAddress);
    if (peerSounds?.[priority] !== undefined) return peerSounds[priority];

    if (communityId) {
      const commSounds = this.settings.perCommunitySounds.get(communityId);
      if (commSounds?.[priority] !== undefined) return commSounds[priority];
    }

    if (senderProfile.notificationSounds?.[priority] !== undefined) {
      return senderProfile.notificationSounds[priority];
    }

    return undefined;
  }

  setPeerSoundOverrides(peerAddress: string, sounds: SoundOverrides): void {
    this.settings.perPeerSounds.set(peerAddress, { ...sounds });
    this.onChange?.();
  }

  clearPeerSoundOverrides(peerAddress: string): void {
    this.settings.perPeerSounds.delete(peerAddress);
    this.onChange?.();
  }

  setCommunitySoundOverrides(communityId: string, sounds: SoundOverrides): void {
    this.settings.perCommunitySounds.set(communityId, { ...sounds });
    this.onChange?.();
  }

  clearCommunitySoundOverrides(communityId: string): void {
    this.settings.perCommunitySounds.delete(communityId);
    this.onChange?.();
  }

  /** ZEB-662: serialize settings for persistence (Maps → plain objects). */
  serialize(): string {
    const mapObj = <V>(m: Map<string, V>) => Object.fromEntries(m.entries());
    return JSON.stringify({
      global: this.settings.global,
      perCommunity: mapObj(this.settings.perCommunity),
      perPeer: mapObj(this.settings.perPeer),
      perPeerSounds: mapObj(this.settings.perPeerSounds),
      perCommunitySounds: mapObj(this.settings.perCommunitySounds),
    });
  }

  /** ZEB-662: load persisted settings. Defensive — a parse/shape failure
   *  leaves the current (default) settings intact and never throws. */
  load(raw: string): void {
    let parsed: unknown;
    try {
      parsed = JSON.parse(raw);
    } catch {
      return;
    }
    if (!parsed || typeof parsed !== 'object') return;
    const p = parsed as Record<string, unknown>;
    const isObj = (v: unknown): v is Record<string, unknown> =>
      !!v && typeof v === 'object' && !Array.isArray(v);
    const toMap = <V>(v: unknown): Map<string, V> =>
      isObj(v) ? new Map(Object.entries(v) as [string, V][]) : new Map();
    if (isObj(p.global)) {
      this.settings.global = { ...DEFAULT_POLICY, ...(p.global as unknown as Partial<NotificationPolicy>) };
    }
    this.settings.perCommunity = toMap(p.perCommunity);
    this.settings.perPeer = toMap(p.perPeer);
    this.settings.perPeerSounds = toMap(p.perPeerSounds);
    this.settings.perCommunitySounds = toMap(p.perCommunitySounds);
  }
}
