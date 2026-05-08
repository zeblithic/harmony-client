import type { TauriAdapter } from './zenoh-service';
import { POWER_THRESHOLDS, type CommunityMember } from './types';

interface MembersChangedPayload { communityId: string; }
interface DegradedPayload { communityId: string; degraded: boolean; }

interface HlcDto { wallMs: number; logical: number; deviceId: string; }
interface MemberInfoDto {
  addr: string;
  displayName?: string | null;
  status: 'joined' | 'left' | 'invited' | 'banned';
  power: number;
  joinedAt: HlcDto;
}

function dtoToMember(d: MemberInfoDto): CommunityMember {
  return {
    address: d.addr,
    displayName: d.displayName ?? undefined,
    status: d.status,
    power: d.power,
    joinedAt: d.joinedAt?.wallMs,
  };
}

export class CommunityService {
  /** Called whenever member rosters or degraded state changes.
   *  Receives the community whose data changed so callers can filter. */
  onChange?: (communityId?: string) => void;

  private adapter: TauriAdapter | null = null;
  private memberCache: Map<string, CommunityMember[]> = new Map();
  private degraded: Map<string, boolean> = new Map();
  // Per-community kind, recorded locally for communities the user
  // creates this session. Backend doesn't yet expose kind on the wire
  // (open follow-up), so for redeemed/foreign communities we return
  // 'unknown' rather than fabricating a value.
  private knownKinds: Map<string, 'open' | 'invite-only'> = new Map();
  private unlisteners: Array<() => void> = [];

  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return;
    this.adapter = adapter;

    const unlistenMembers = await adapter.listen(
      'community-members-changed',
      (event) => {
        const p = event.payload as MembersChangedPayload;
        this.memberCache.delete(p.communityId);
        this.onChange?.(p.communityId);
      },
    );
    this.unlisteners.push(unlistenMembers);

    const unlistenDegraded = await adapter.listen(
      'community-state-sync-degraded',
      (event) => {
        const p = event.payload as DegradedPayload;
        this.degraded.set(p.communityId, p.degraded);
        this.onChange?.(p.communityId);
      },
    );
    this.unlisteners.push(unlistenDegraded);
  }

  async createCommunity(name: string, kind: 'open' | 'invite-only'): Promise<string> {
    const id = await this.invoke<string>('create_community', {
      name,
      isInviteOnly: kind === 'invite-only',
    });
    this.knownKinds.set(id, kind);
    return id;
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
    // Clamp to POWER_THRESHOLDS.max (100) — matches the UI slider's
    // declared range and the backend's accepted range. Earlier
    // revisions clamped to 255 (u8 max) which made the intent
    // unambiguous to readers and would have allowed out-of-range
    // values if this method ever got called outside SetPowerDialog.
    const level = Math.max(0, Math.min(POWER_THRESHOLDS.max, Math.trunc(newPower)));
    await this.invoke<void>('set_power_level', { communityId, targetAddr, level });
  }

  async generateInvite(communityId: string): Promise<string> {
    return this.invoke<string>('generate_invite', {
      communityId,
      inviteeHint: null,
      expiresAt: null,
    });
  }

  async listCommunityMembers(communityId: string): Promise<CommunityMember[]> {
    const cached = this.memberCache.get(communityId);
    if (cached) return cached;
    const dtos = await this.invoke<MemberInfoDto[]>('list_community_members', { communityId });
    const fresh = dtos.map(dtoToMember);
    this.memberCache.set(communityId, fresh);
    return fresh;
  }

  isDegraded(communityId: string): boolean {
    return this.degraded.get(communityId) ?? false;
  }

  /** Returns the locally-known kind for a community, or 'unknown' for
   *  redeemed/foreign communities (until backend exposes kind on the
   *  wire). Callers should render kind-specific UI conditionally on
   *  this not being 'unknown' rather than assuming a default. */
  getKind(communityId: string): 'open' | 'invite-only' | 'unknown' {
    return this.knownKinds.get(communityId) ?? 'unknown';
  }

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
    this.memberCache.clear();
    this.degraded.clear();
    this.knownKinds.clear();
  }

  private async invoke<T>(cmd: string, args: Record<string, unknown>): Promise<T> {
    if (!this.adapter) throw new Error(`CommunityService.${cmd}: adapter not connected`);
    return this.adapter.invoke(cmd, args) as Promise<T>;
  }
}
