import type { MediaAttachment, TrustLevel, TrustSettings } from './types';

export class TrustService {
  readonly settings: TrustSettings;
  private loadedAttachments = new Set<string>();

  constructor() {
    this.settings = {
      global: 'untrusted',
      perPeer: new Map(),
      perCommunity: new Map(),
    };
  }

  resolve(peerAddress: string, communityId?: string): TrustLevel {
    const peerLevel = this.settings.perPeer.get(peerAddress);
    if (peerLevel !== undefined) return peerLevel;

    if (communityId) {
      const commLevel = this.settings.perCommunity.get(communityId);
      if (commLevel !== undefined) return commLevel;
    }

    return this.settings.global;
  }

  setGlobalTrust(level: TrustLevel): void {
    this.settings.global = level;
  }

  setPeerTrust(address: string, level: TrustLevel): void {
    this.settings.perPeer.set(address, level);
  }

  clearPeerTrust(address: string): void {
    this.settings.perPeer.delete(address);
  }

  setCommunityTrust(id: string, level: TrustLevel): void {
    this.settings.perCommunity.set(id, level);
  }

  clearCommunityTrust(id: string): void {
    this.settings.perCommunity.delete(id);
  }

  isLoaded(attachmentId: string): boolean {
    return this.loadedAttachments.has(attachmentId);
  }

  markLoaded(attachmentId: string): void {
    this.loadedAttachments.add(attachmentId);
  }

  clearLoaded(): void {
    this.loadedAttachments.clear();
  }

  static isGated(attachment: MediaAttachment): boolean {
    return attachment.type === 'image' || attachment.type === 'link';
  }
}
