import type { TrustLevel, TrustSettings } from './types';
import type { TrustGraphService } from './trust-graph-service';

export class TrustService {
  readonly settings: TrustSettings;
  private trustGraph: TrustGraphService | null;

  constructor(trustGraph?: TrustGraphService) {
    this.settings = {
      global: 'untrusted',
      perPeer: new Map(),
      perCommunity: new Map(),
    };
    this.trustGraph = trustGraph ?? null;
  }

  resolve(peerAddress: string, communityId?: string): TrustLevel {
    const peerLevel = this.settings.perPeer.get(peerAddress);
    if (peerLevel !== undefined) return peerLevel;

    if (communityId) {
      const commLevel = this.settings.perCommunity.get(communityId);
      if (commLevel !== undefined) return commLevel;
    }

    if (this.trustGraph) {
      const graphLevel = this.trustGraph.resolveMediaTrust(peerAddress);
      if (graphLevel !== null) return graphLevel;
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
}
