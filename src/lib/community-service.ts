import type { TauriAdapter } from './zenoh-service';
import { POWER_THRESHOLDS, type CommunityMember, type ModerationEvent } from './types';
import type { ChannelMessageDto } from './channel-message-service';

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

/** Mirrors `ChannelInfoDto` in src-tauri/src/lib.rs (ZEB-266 Phase 1).
 *  HLC fields wire as `HlcDto` ({wallMs, logical, deviceId}). */
export interface ChannelInfo {
  channelId: string;
  name: string;
  writePower: number;
  createdAt: { wallMs: number; logical: number; deviceId: string };
  deletedAt?: { wallMs: number; logical: number; deviceId: string };
}

/**
 * ZEB-285 Phase 1 Task 11: mirrors `PreForkSnapshotDto` from src-tauri/src/lib.rs.
 * channelLog keys are channel-ID hex strings (32 chars); values are
 * HLC-ascending sorted ChannelMessageDto arrays (same shape as live messages).
 */
export interface PreForkSnapshotDto {
  originalCommunityName: string;
  forkedAtMs: number;
  channelLog: Record<string, ChannelMessageDto[]>;
}

/** Action discriminator on the `channel-config-updated` Tauri event.
 *  Backend serializes via serde rename_all = "camelCase" so the wire
 *  shape is the literal strings 'created' | 'modified' | 'deleted'. */
export type ChannelConfigAction = 'created' | 'modified' | 'deleted';

interface ChannelConfigChangedPayload {
  communityId: string;
  channelId: string;
  action: ChannelConfigAction;
  name?: string;
  writePower?: number;
  atWallMs: number;
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

  /** Called when a channel-config CRDT mutation materializes through
   *  the per-community state-CRDT. Receivers should refresh
   *  listChannels(communityId) to pull the post-mutation snapshot. */
  onChannelConfigChanged?: (
    communityId: string,
    action: ChannelConfigAction,
    channelId: string,
    name?: string,
    writePower?: number,
  ) => void;

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
  private channelCache = new Map<string, ChannelInfo[]>();
  private selectedChannelByCommunity = new Map<string, string>();
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

    const unlistenChannelConfig = await adapter.listen(
      'channel-config-updated',
      (event) => {
        const p = event.payload as ChannelConfigChangedPayload;
        // Invalidate the per-community channel cache so the next
        // listChannels(communityId) re-fetches.
        this.channelCache.delete(p.communityId);
        this.onChannelConfigChanged?.(
          p.communityId,
          p.action,
          p.channelId,
          p.name,
          p.writePower,
        );
      },
    );
    this.unlisteners.push(unlistenChannelConfig);
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

  /**
   * ZEB-252 Sub-D Phase 6: typed direct-join entry point for
   * library-directory click flows. The backend re-resolves the matching
   * `LibraryDirectoryEntry` by community_id and delegates to the same
   * `redeem_invite_inner` codepath `redeemInvite` uses, so the resulting
   * DTO and side-effects (engine spawn, owner-state Space row, self-Join
   * event log) are identical. `redeemInvite(url)` stays for hand-pasted
   * URLs.
   */
  async joinOpenCommunity(communityId: string): Promise<RedeemInviteResultDto> {
    const dto = await this.invoke<RedeemInviteResultDto>('join_open_community', { communityId });
    // Backend hands back the kind; populate getKind() the same way redeemInvite does.
    // Phase 6 only joins OPEN communities (invite-only entries are rejected
    // by the backend's defensive re-check), so isInviteOnly will always be false
    // for successful returns — but we mirror redeemInvite's logic for symmetry
    // rather than assuming.
    this.knownKinds.set(dto.communityId, dto.isInviteOnly ? 'invite-only' : 'open');
    return dto;
  }

  async leaveCommunity(communityId: string): Promise<void> {
    await this.invoke<void>('leave_community', { communityId });
  }

  async forkCommunity(
    communityId: string,
    opts: { name: string; silent?: boolean; alsoLeave?: boolean },
  ): Promise<{ forkSpaceId: string; visible: boolean; snapshotMessageCount: number }> {
    try {
      return await this.invoke<{
        forkSpaceId: string;
        visible: boolean;
        snapshotMessageCount: number;
      }>('fork_community', { communityId, opts });
    } catch (e) {
      throw new Error(e instanceof Error ? e.message : String(e));
    }
  }

  async kickFromCommunity(
    communityId: string,
    targetAddr: string,
    reason?: string,
  ): Promise<void> {
    await this.invoke<void>('kick_from_community', {
      communityId,
      targetAddr,
      reason: reason ?? null,
    });
  }

  async unbanFromCommunity(
    communityId: string,
    targetAddr: string,
    reason?: string,
  ): Promise<void> {
    await this.invoke<void>('unban_from_community', {
      communityId,
      targetAddr,
      reason: reason ?? null,
    });
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

  async createChannel(
    communityId: string,
    name: string,
    writePower: number,
  ): Promise<string> {
    return this.invoke<string>('create_channel', {
      communityId,
      name,
      writePower,
    });
  }

  async modifyChannel(
    communityId: string,
    channelId: string,
    name?: string,
    writePower?: number,
  ): Promise<void> {
    await this.invoke<void>('modify_channel', {
      communityId,
      channelId,
      name,
      writePower,
    });
  }

  async deleteChannel(communityId: string, channelId: string): Promise<void> {
    await this.invoke<void>('delete_channel', { communityId, channelId });
  }

  async listChannels(communityId: string): Promise<ChannelInfo[]> {
    const cached = this.channelCache.get(communityId);
    if (cached) return cached;
    const fresh = await this.invoke<ChannelInfo[]>('list_channels', { communityId });
    this.channelCache.set(communityId, fresh);
    return fresh;
  }

  /** Per spec §6.5: session-scoped selected-channel map. Returns
   *  undefined for first-visit to a community (caller falls back to
   *  #general or first channel). Cleared by destroy(). */
  getSelectedChannel(communityId: string): string | undefined {
    return this.selectedChannelByCommunity.get(communityId);
  }

  setSelectedChannel(communityId: string, channelId: string): void {
    this.selectedChannelByCommunity.set(communityId, channelId);
  }

  async listCommunityMembers(communityId: string): Promise<CommunityMember[]> {
    const cached = this.memberCache.get(communityId);
    if (cached) return cached;
    const dtos = await this.invoke<MemberInfoDto[]>('list_community_members', { communityId });
    const fresh = dtos.map(dtoToMember);
    this.memberCache.set(communityId, fresh);
    return fresh;
  }

  async listRecentModerationEvents(
    communityId: string,
    limit: number = 10,
  ): Promise<ModerationEvent[]> {
    return await this.invoke<ModerationEvent[]>('list_recent_moderation_events', {
      communityId,
      limit,
    });
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

  /**
   * ZEB-285 Phase 1 Task 11: fetch the full channel-log snapshot for a forked
   * community so the unified timeline can merge pre-fork + live messages.
   * Returns null when the community is not a fork (no pre_fork_snapshot.bin).
   * Mirrors `PreForkSnapshotDto` from src-tauri/src/lib.rs.
   */
  async getPreForkSnapshot(communityId: string): Promise<PreForkSnapshotDto | null> {
    try {
      const dto = await this.invoke<PreForkSnapshotDto | null>('get_pre_fork_snapshot', {
        communityId,
      });
      return dto ?? null;
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      console.warn(`getPreForkSnapshot failed for ${communityId}: ${msg}`);
      return null;
    }
  }

  /**
   * ZEB-285 Phase 1 Task 10: fetch fork lineage metadata for the Settings panel.
   * Returns null when the community is not a fork (no pre_fork_snapshot.bin).
   * Mirrors the `CommunityLineageDto` returned by the `get_community_lineage` IPC.
   */
  async getCommunityLineage(communityId: string): Promise<{
    originalCommunityName: string;
    forkedAtMs: number;
    snapshotMessageCount: number;
  } | null> {
    const dto = await this.invoke<{
      originalCommunityName: string;
      forkedAtMs: number;
      snapshotMessageCount: number;
    } | null>('get_community_lineage', { communityId });
    return dto ?? null;
  }

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
    this.memberCache.clear();
    this.degraded.clear();
    this.knownKinds.clear();
    this.channelCache.clear();
    this.selectedChannelByCommunity.clear();
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
