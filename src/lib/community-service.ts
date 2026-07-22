import type { TauriAdapter } from './zenoh-service';
import {
  POWER_THRESHOLDS,
  type AdminActionResult,
  type CommunityGovernance,
  type CommunityLineageDto,
  type CommunityMember,
  type ForkDescendantDto,
  type ModerationEvent,
} from './types';
import type { ChannelMessageDto } from './channel-message-service';
import type { NavUpdatedPayload } from './nav-service';

interface MembersChangedPayload { communityId: string; }
interface DegradedPayload { communityId: string; degraded: boolean; }

/**
 * Open-community cross-WAN first-contact status (camelCase wire key `status`).
 * The backend (`connectivity_open_join_iroh`) emits exactly one of these, or
 * omits the field entirely on the legacy/local redeem path:
 *   - `"joined"`    → the join completed.
 *   - `"searching"` → RETRYABLE cold start: no beacon reachable yet; the node
 *                     keeps retrying on its transport-epoch re-arm.
 *   - `"rejected"`  → the beacon explicitly rejected the join (banned / bad
 *                     capability), or an internal dial invariant failed — a
 *                     distinct, NON-retryable blocked state.
 */
export type OpenJoinStatus = 'joined' | 'searching' | 'rejected';

/**
 * Mirrors `RedeemInviteResultDto` in src-tauri/src/lib.rs (ZEB-265).
 * Returned from `redeem_invite` so the caller can render a real
 * community name + record the kind without re-decoding the invite URL.
 */
export interface RedeemInviteResultDto {
  communityId: string;
  communityName: string;
  isInviteOnly: boolean;
  /**
   * ZEB-254: true if the redemption returned before a JoinCountersign
   * landed locally (admin was offline; the 5s fast-path timeout fired).
   * The community appears in nav greyed; ungreys when JoinCountersign
   * arrives via state-root sync. false for open communities or when
   * counter-sign came back within 5s.
   */
  pending: boolean;
  /**
   * Open-community cross-WAN first-contact status (camelCase wire key
   * `status`, omitted by the backend when null — so this is `undefined`
   * for the legacy/local redeem path). Only emitted by
   * `connectivity_open_join_iroh`. Field absent → legacy/local redeem;
   * render exactly as today. See {@link OpenJoinStatus}.
   */
  status?: OpenJoinStatus;
}

/**
 * ZEB-393 Bug B: a persisted community shaped for boot rehydration of the
 * nav sidebar. Mirrors `CommunityNavDto` in src-tauri/src/lib.rs
 * (`#[serde(rename_all = "camelCase")]`). Returned from
 * `list_owner_communities`; `pending` carries the ZEB-254 greyed state.
 */
export interface CommunityNavDto {
  spaceId: string;
  name: string;
  isInviteOnly: boolean;
  pending: boolean;
}

/**
 * ZEB-435: a left (but not yet deleted-forever) community for the
 * left-communities management surface. Mirrors `LeftCommunityNavDto` in
 * src-tauri/src/lib.rs (`#[serde(rename_all = "camelCase")]`). `leftAtMs` is
 * the leave's HLC wall-clock for display. Returned from
 * `list_left_communities`; rows disappear once `removeSpace` tombstones them.
 */
export interface LeftCommunityNavDto {
  spaceId: string;
  name: string;
  leftAtMs: number;
}

/**
 * ZEB-393 Bug B: CommunityNavDto → an 'added' community nav payload for
 * `navService.addOrUpdateNavSpace`. `pending: undefined` when not pending so
 * the nav tree leaves the greyed state off (addOrUpdateNavSpace reads
 * `pending ?? undefined`); `pending: true` keeps an invite-only join whose
 * countersign hasn't landed greyed at boot.
 */
export function toNavPayload(c: CommunityNavDto): NavUpdatedPayload {
  return {
    action: 'added',
    spaceId: c.spaceId,
    kind: 'community',
    name: c.name,
    pending: c.pending || undefined,
  };
}

/** Mirrors `ChannelInfoDto` in src-tauri/src/lib.rs (ZEB-266 Phase 1).
 *  HLC fields wire as `HlcDto` ({wallMs, logical, deviceId}). */
export interface ChannelInfo {
  channelId: string;
  name: string;
  writePower: number;
  /** ZEB-349: channel kind. The Rust `ChannelInfoDto` always emits this
   *  as the camelCase string "text" | "voice" | "townhall" (ZEB-612), so it
   *  is required. */
  kind: 'text' | 'voice' | 'townhall';
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
  /** ZEB-649: the forker's stated why; null for pre-ZEB-649 snapshots. */
  forkReason: string | null;
  channelLog: Record<string, ChannelMessageDto[]>;
}

/** Action discriminator on the `channel-config-updated` Tauri event.
 *  Backend serializes via serde rename_all = "camelCase" so the wire
 *  shape is the literal strings 'created' | 'modified' | 'deleted'. */
export type ChannelConfigAction = 'created' | 'modified' | 'deleted';

/** Mirrors `ChannelConfigChangedPayload` in src-tauri/src/lib.rs.
 *  Note (ZEB-349): `kind` is intentionally NOT on this event — consumers
 *  re-fetch `list_channels` (whose `ChannelInfo.kind` is always present) on
 *  a config change, so the event itself doesn't carry it. */
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

/**
 * True iff `authorHex` is a currently-JOINED member of `members`.
 *
 * Used (ZEB-404) to detect a message from someone not yet in our roster — a
 * signal that we missed a live `community-members-changed` delta and the
 * roster should be re-fetched. Address comparison is case-insensitive because
 * roster addresses and message authors are both lowercase hex by convention,
 * but we normalize defensively.
 */
export function rosterHasJoinedAuthor(
  members: Pick<CommunityMember, 'address' | 'status'>[],
  authorHex: string,
): boolean {
  const a = authorHex.toLowerCase();
  return members.some((x) => x.status === 'joined' && x.address.toLowerCase() === a);
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
  // ZEB-404: in-flight member fetches per community, for single-flight
  // coalescing (see listCommunityMembers).
  private inFlightMemberFetches: Map<string, Promise<CommunityMember[]>> = new Map();
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

  /**
   * ZEB-393 Bug B: enumerate the viewer's persisted communities so the nav
   * sidebar can be rehydrated at boot. The nav tree is otherwise push-only
   * (filled by runtime `nav-updated` events) and boots empty on every
   * restart. Also records each community's kind so `getKind()` resolves for
   * rehydrated nodes instead of returning 'unknown'. App.svelte seeds the
   * tree via `navService.addOrUpdateNavSpace(toNavPayload(row))`.
   */
  async listOwnerCommunities(): Promise<CommunityNavDto[]> {
    const rows = await this.invoke<CommunityNavDto[]>('list_owner_communities', {});
    for (const r of rows) {
      this.knownKinds.set(r.spaceId, r.isInviteOnly ? 'invite-only' : 'open');
    }
    return rows;
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

  /**
   * ZEB-435: enumerate left-but-not-deleted communities for the management
   * surface (they're hidden from nav, so this list is the only way to reach
   * them). Pull-on-open; no push events — re-fetch after each removeSpace.
   */
  async listLeftCommunities(): Promise<LeftCommunityNavDto[]> {
    return this.invoke<LeftCommunityNavDto[]>('list_left_communities', {});
  }

  /**
   * ZEB-435: explicit "delete forever" — permanently tombstone a space and (for
   * a community) delete its on-disk data. IRREVERSIBLE: the tombstone blocks any
   * re-invite from resurrecting it. For a community the caller MUST have
   * `leaveCommunity`'d first (the backend refuses otherwise, to avoid leaving a
   * ghost member). Distinct from `leaveCommunity`, which is a reversible leave.
   */
  async removeSpace(spaceId: string): Promise<void> {
    await this.invoke<void>('remove_space', { spaceId });
  }

  async forkCommunity(
    communityId: string,
    // ZEB-649: reason is mandatory — the backend rejects empty/oversize
    // before any state change.
    opts: { name: string; reason: string; silent?: boolean; alsoLeave?: boolean },
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
  ): Promise<AdminActionResult> {
    return this.invoke<AdminActionResult>('kick_from_community', {
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

  async setPowerLevel(
    communityId: string,
    targetAddr: string,
    newPower: number,
  ): Promise<AdminActionResult> {
    // Clamp to POWER_THRESHOLDS.max (100) — matches the UI slider's
    // declared range and the backend's accepted range. Earlier
    // revisions clamped to 255 (u8 max) which made the intent
    // unambiguous to readers and would have allowed out-of-range
    // values if this method ever got called outside SetPowerDialog.
    const level = Math.max(0, Math.min(POWER_THRESHOLDS.max, Math.trunc(newPower)));
    return this.invoke<AdminActionResult>('set_power_level', { communityId, targetAddr, level });
  }

  async generateInvite(communityId: string): Promise<string> {
    return this.invoke<string>('generate_invite', {
      communityId,
      inviteeHint: null,
      // ZEB-564: TTL duration in ms (null → backend's 7-day default). The GUI
      // doesn't expose an expiry yet; the backend derives `now + ttlMs`.
      ttlMs: null,
    });
  }

  async createChannel(
    communityId: string,
    name: string,
    writePower: number,
    kind: 'text' | 'voice' | 'townhall' = 'text',
  ): Promise<string> {
    return this.invoke<string>('create_channel', {
      communityId,
      name,
      writePower,
      kind,
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

  /** ZEB-662: synchronous best-effort channel-name lookup from the session
   *  cache that `listChannels` populates (community channels aren't nav nodes
   *  in production, so the mention alerter can't resolve names via NavService).
   *  Returns undefined if the community's channels haven't been fetched this
   *  session — the caller falls back to a generic label. */
  getCachedChannelName(communityId: string, channelId: string): string | undefined {
    return this.channelCache
      .get(communityId)
      ?.find((c) => c.channelId === channelId)?.name;
  }

  /**
   * Fetch a community's members. Returns the per-community in-memory cache
   * when present and `forceRefresh` is false.
   *
   * ZEB-404: explicit live-refresh triggers (a peer joined / reconnect) pass
   * `forceRefresh: true` so they bypass a roster the cache may be holding
   * stale — the cache is otherwise invalidated only by the
   * `community-members-changed` event, the very event those triggers exist to
   * compensate for when it isn't delivered.
   */
  async listCommunityMembers(
    communityId: string,
    forceRefresh = false,
  ): Promise<CommunityMember[]> {
    // ZEB-404: single-flight. Multiple triggers (the message-throttle,
    // reconnect, and community-open refreshes) can request a fetch for the same
    // community concurrently. Share any in-flight fetch FIRST — before the
    // cache — so even a non-forced read joins an imminent fresh result instead
    // of serving a cache entry that fetch is about to replace. Coalescing also
    // makes the cache write single and ordered, so two overlapping IPCs can't
    // complete out of order and poison `memberCache`.
    const inFlight = this.inFlightMemberFetches.get(communityId);
    if (inFlight) return inFlight;
    if (!forceRefresh) {
      const cached = this.memberCache.get(communityId);
      if (cached) return cached;
    }
    const fetchPromise = (async () => {
      try {
        const dtos = await this.invoke<MemberInfoDto[]>('list_community_members', { communityId });
        const fresh = dtos.map(dtoToMember);
        this.memberCache.set(communityId, fresh);
        return fresh;
      } finally {
        this.inFlightMemberFetches.delete(communityId);
      }
    })();
    this.inFlightMemberFetches.set(communityId, fetchPromise);
    return fetchPromise;
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
   * ZEB-285 Phase 1 Task 10: fetch fork snapshot metadata for the Settings panel.
   * ZEB-287 Phase 2 renamed (was `getCommunityLineage`) — Phase 2 introduces a
   * distinct `getCommunityLineage` that returns the multi-hop ancestor chain.
   * Returns null when the community is not a fork (no pre_fork_snapshot.bin).
   * Mirrors the `ForkSnapshotMetadataDto` returned by the
   * `get_fork_snapshot_metadata` IPC.
   */
  async getForkSnapshotMetadata(communityId: string): Promise<{
    originalCommunityName: string;
    forkedAtMs: number;
    snapshotMessageCount: number;
    forkReason: string | null;
  } | null> {
    const dto = await this.invoke<{
      originalCommunityName: string;
      forkedAtMs: number;
      snapshotMessageCount: number;
      forkReason: string | null;
    } | null>('get_fork_snapshot_metadata', { communityId });
    return dto ?? null;
  }

  /**
   * ZEB-287 Phase 2 spec §4.2: fetch multi-hop fork lineage metadata for
   * a community, sourced from CommunityState (not disk). Returns the
   * immediate-parent + ancestor chain + self info needed by
   * ForkLineageTree.svelte to render ancestors / "you are here" /
   * descendants in one component.
   *
   * Throws on backend error; caller renders the empty-state fallback if
   * this rejects (e.g. caller not yet Joined, community not yet in
   * registry).
   */
  async getCommunityLineage(communityId: string): Promise<CommunityLineageDto> {
    try {
      return await this.invoke<CommunityLineageDto>('get_community_lineage', { communityId });
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      throw new Error(`getCommunityLineage: ${msg}`);
    }
  }

  /**
   * ZEB-287 Phase 2 spec §4.1: list visible Fork descendants for a
   * community by walking its membership log for MembershipEventKind::Fork
   * events. Silent forks are absent by design. Caller must be Joined.
   *
   * Returns rows sorted ascending by forkedAtWallMs with stable
   * forkerAddr tie-break.
   */
  async listCommunityForks(communityId: string): Promise<ForkDescendantDto[]> {
    try {
      return await this.invoke<ForkDescendantDto[]>('list_community_forks', { communityId });
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      throw new Error(`listCommunityForks: ${msg}`);
    }
  }

  /**
   * ZEB-608 D1: read-only governance values for a community. Readable by
   * ANY Joined member (no power gate) — powers the CharterView admin-quorum
   * card and the settings panel's real quorum display (fixes the
   * always-shows-1 default, spec §0.2).
   */
  async getCommunityGovernance(communityId: string): Promise<CommunityGovernance> {
    try {
      return await this.invoke<CommunityGovernance>('get_community_governance', { communityId });
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      throw new Error(`getCommunityGovernance: ${msg}`);
    }
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
