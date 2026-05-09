import type { TauriAdapter } from './zenoh-service';
import { POWER_THRESHOLDS, type CommunityMember } from './types';

interface MembersChangedPayload { communityId: string; }
interface DegradedPayload { communityId: string; degraded: boolean; }

/**
 * Mirrors `RedeemInviteResultDto` in src-tauri/src/lib.rs (ZEB-265).
 * Returned from `redeem_invite` so the caller can render a real
 * community name + record the kind without re-decoding the invite URL.
 */
export interface RedeemInviteResultDto {
  communityId: string;
  communityName: string;
  isInviteOnly: boolean;
}

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
  /** Called when a community's member roster changes. The receiver
   *  should refresh listCommunityMembers for the given community.
   *  Separated from onDegradedChanged so degraded-only events don't
   *  trigger an unnecessary roster fetch + reactive cascade. */
  onMembersChanged?: (communityId: string) => void;

  /** Called when a community's degraded sync flag flips. Cheap to
   *  handle — the receiver should re-read isDegraded(communityId)
   *  but does not need to invalidate any local roster state. */
  onDegradedChanged?: (communityId: string) => void;

  private adapter: TauriAdapter | null = null;
  private memberCache: Map<string, CommunityMember[]> = new Map();
  private degraded: Map<string, boolean> = new Map();
  // Per-community kind. Populated by createCommunity (from the
  // user-supplied argument) and redeemInvite (from
  // RedeemInviteResultDto.isInviteOnly — ZEB-265). Communities the
  // current session has neither created nor redeemed (e.g. foreign
  // communities surfaced via cross-device sync, before any IPC
  // round-trip) will not have an entry here, and getKind() returns
  // 'unknown' for those.
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
        this.onMembersChanged?.(p.communityId);
      },
    );
    this.unlisteners.push(unlistenMembers);

    const unlistenDegraded = await adapter.listen(
      'community-state-sync-degraded',
      (event) => {
        const p = event.payload as DegradedPayload;
        this.degraded.set(p.communityId, p.degraded);
        this.onDegradedChanged?.(p.communityId);
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

  async redeemInvite(url: string): Promise<RedeemInviteResultDto> {
    const dto = await this.invoke<RedeemInviteResultDto>('redeem_invite', { url });
    // Now that the backend hands back the kind, redeemed/foreign
    // communities can populate `getKind()` correctly instead of
    // returning 'unknown'. ZEB-265.
    this.knownKinds.set(dto.communityId, dto.isInviteOnly ? 'invite-only' : 'open');
    return dto;
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

  /** Returns the locally-known kind for a community, or 'unknown'
   *  when neither createCommunity nor redeemInvite has run for it in
   *  this session (e.g. foreign communities surfaced via cross-device
   *  sync). Callers should render kind-specific UI conditionally on
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
    // Null the adapter too — without this, connectAdapter's
    // duplicate-init guard (`if (this.adapter) return;`) would
    // silently no-op on reconnect after destroy(), leaving the
    // service alive-but-listenerless.
    this.adapter = null;
  }

  private async invoke<T>(cmd: string, args: Record<string, unknown>): Promise<T> {
    if (!this.adapter) throw new Error(`CommunityService.${cmd}: adapter not connected`);
    return this.adapter.invoke(cmd, args) as Promise<T>;
  }
}
