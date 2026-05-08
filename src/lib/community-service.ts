import type { TauriAdapter } from './zenoh-service';
import type { CommunityMember } from './types';

interface MembersChangedPayload { communityId: string; }
interface DegradedPayload { communityId: string; degraded: boolean; }

export class CommunityService {
  /** Called whenever member rosters or degraded state changes. */
  onChange?: () => void;

  private adapter: TauriAdapter | null = null;
  private memberCache: Map<string, CommunityMember[]> = new Map();
  private degraded: Map<string, boolean> = new Map();
  private unlisteners: Array<() => void> = [];

  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return;
    this.adapter = adapter;

    const unlistenMembers = await adapter.listen(
      'community-members-changed',
      (event) => {
        const p = event.payload as MembersChangedPayload;
        this.memberCache.delete(p.communityId);
        this.onChange?.();
      },
    );
    this.unlisteners.push(unlistenMembers);

    const unlistenDegraded = await adapter.listen(
      'community-state-sync-degraded',
      (event) => {
        const p = event.payload as DegradedPayload;
        this.degraded.set(p.communityId, p.degraded);
        this.onChange?.();
      },
    );
    this.unlisteners.push(unlistenDegraded);
  }

  async createCommunity(name: string, kind: 'open' | 'invite-only'): Promise<string> {
    return this.invoke<string>('create_community', { name, kind });
  }

  async redeemInvite(url: string): Promise<string> {
    return this.invoke<string>('redeem_invite', { url });
  }

  async leaveCommunity(communityId: string): Promise<void> {
    await this.invoke<void>('leave_community', { communityId });
  }

  async kickMember(communityId: string, targetAddr: string): Promise<void> {
    await this.invoke<void>('kick_from_community', { communityId, targetAddr });
  }

  async setPowerLevel(communityId: string, targetAddr: string, newPower: number): Promise<void> {
    await this.invoke<void>('set_power_level', { communityId, targetAddr, newPower });
  }

  async generateInvite(communityId: string): Promise<string> {
    return this.invoke<string>('generate_invite', {
      communityId,
      inviteeHint: null,
      expiresAt: null,
    });
  }

  async listMembers(communityId: string): Promise<CommunityMember[]> {
    const cached = this.memberCache.get(communityId);
    if (cached) return cached;
    const fresh = await this.invoke<CommunityMember[]>('list_community_members', { communityId });
    this.memberCache.set(communityId, fresh);
    return fresh;
  }

  isDegraded(communityId: string): boolean {
    return this.degraded.get(communityId) ?? false;
  }

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
    this.memberCache.clear();
    this.degraded.clear();
  }

  private async invoke<T>(cmd: string, args: Record<string, unknown>): Promise<T> {
    if (!this.adapter) throw new Error(`CommunityService.${cmd}: adapter not connected`);
    return this.adapter.invoke(cmd, args) as Promise<T>;
  }
}
