import type {
  MessagePriority,
  NotificationAction,
  NotificationPolicy,
  NotificationSettings,
} from './types';

const DEFAULT_POLICY: NotificationPolicy = {
  quiet: 'dot_only',
  standard: 'sound',
  loud: 'break_dnd',
};

export class NotificationService {
  settings: NotificationSettings;

  constructor() {
    this.settings = {
      global: { ...DEFAULT_POLICY },
      perCommunity: new Map(),
      perPeer: new Map(),
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
  }

  setCommunityPolicy(communityId: string, policy: Partial<NotificationPolicy>): void {
    this.settings.perCommunity.set(communityId, { ...policy });
  }

  setPeerPolicy(peerAddress: string, policy: Partial<NotificationPolicy>): void {
    this.settings.perPeer.set(peerAddress, { ...policy });
  }

  clearCommunityPolicy(communityId: string): void {
    this.settings.perCommunity.delete(communityId);
  }

  clearPeerPolicy(peerAddress: string): void {
    this.settings.perPeer.delete(peerAddress);
  }

  shouldPlaySound(action: NotificationAction): boolean {
    return action === 'sound' || action === 'break_dnd';
  }
}
