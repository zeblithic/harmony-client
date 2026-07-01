import type { TauriAdapter } from './zenoh-service';

/**
 * Wire shape of a single member's presence, as returned by
 * `get_community_presence` and carried in the `presence-updated` event.
 * The Rust DTO uses `#[serde(rename_all = "camelCase")]`, so keys are
 * camelCase. (ZEB-537 community presence.)
 */
export interface PresenceMemberDto {
  /** Hex-encoded owner_id (32 hex chars, lowercase). */
  ownerIdHex: string;
  /** True iff the owner currently has at least one device beaconing. */
  online: boolean;
  /** Wall-clock ms of the most recent beacon seen for this owner. */
  lastSeenMs: number;
  /** Number of distinct devices currently beaconing for this owner. */
  deviceCount: number;
}

/** Payload of the backend `presence-updated` Tauri event. */
interface PresenceUpdatedPayload {
  communityId: string;
  members: PresenceMemberDto[];
}

/**
 * Per-community online-presence facade over the three community-presence
 * IPCs (`subscribe_community_presence` / `unsubscribe_community_presence` /
 * `get_community_presence`) plus the `presence-updated` push event.
 *
 * Mirrors the sibling service shape (MemberCardService / ChannelMessageService):
 * the {@link TauriAdapter} is injected at construction, `invoke` carries
 * camelCase args across the Tauri boundary, `listen` installs the push
 * listener, and IPC rejections are normalized to `Error` (production
 * rejections are raw strings; tests use `Error`).
 *
 * State is a per-community map of `ownerIdHex(lc) -> PresenceMemberDto`, seeded
 * on {@link subscribe} from `get_community_presence` and kept live by the
 * `presence-updated` event (filtered to the subscribed community). {@link isOnline}
 * answers for the ACTIVE community only (the most recently subscribed one;
 * lowercased owner_id lookup), so a lingering old-community map never leaks.
 */
export class PresenceService {
  /** communityId -> (ownerIdHex(lc) -> presence). */
  private byCommunity = new Map<string, Map<string, PresenceMemberDto>>();
  /** communityId -> unlisten fn for that community's `presence-updated` listener. */
  private unlisteners = new Map<string, () => void>();
  /** communityId -> the caller's onUpdate callback. */
  private callbacks = new Map<string, (members: PresenceMemberDto[]) => void>();
  /**
   * The community whose roster {@link isOnline} consults. Set on a successful
   * {@link subscribe}; cleared when that community is unsubscribed. Scoping
   * isOnline to the active community prevents a lingering old-community map
   * (e.g. mid community-switch) from leaking a stale "online" answer.
   */
  private activeCommunityId: string | null = null;

  /**
   * Adapter is optional so callers can construct the service at boot (before
   * the Tauri runtime is wired) and inject the adapter later via
   * {@link setAdapter}. Network methods no-op (with a `console.warn`) when the
   * adapter is absent — mirrors MemberCardService so a non-connected boot
   * never crashes. (ZEB-537.)
   */
  constructor(private adapter?: TauriAdapter) {}

  /** Wire the Tauri adapter after it becomes available (post-boot). */
  setAdapter(adapter: TauriAdapter): void {
    this.adapter = adapter;
  }

  /**
   * Start beaconing/subscribing for `communityId`, install the
   * `presence-updated` listener, and seed initial state from
   * `get_community_presence`. `onUpdate` fires with the parsed member list on
   * the initial seed and on every subsequent `presence-updated` event for this
   * community. Idempotent per community (a second subscribe replaces the
   * callback and re-seeds; it does not double-install the listener).
   */
  async subscribe(
    communityId: string,
    onUpdate: (members: PresenceMemberDto[]) => void,
  ): Promise<void> {
    if (!this.adapter) {
      console.warn('PresenceService.subscribe: no adapter wired; ignoring');
      return;
    }
    this.callbacks.set(communityId, onUpdate);
    try {
      await this.adapter.invoke('subscribe_community_presence', { communityId });
    } catch (e: unknown) {
      this.callbacks.delete(communityId);
      throw new Error(e instanceof Error ? e.message : String(e));
    }
    // Backend subscribe succeeded → this is now the active community for isOnline.
    this.activeCommunityId = communityId;

    // Install the push listener and seed initial state (firing onUpdate) from
    // the authoritative snapshot. If EITHER the `listen` install OR the seed
    // fetch rejects, roll back the partial subscription (listener if installed +
    // backend subscription + cached state) before rethrowing, so a failed
    // subscribe never leaves an orphaned listener/backend subscription behind.
    try {
      if (!this.unlisteners.has(communityId)) {
        const unlisten = await this.adapter.listen('presence-updated', (event) => {
          const p = event.payload as PresenceUpdatedPayload;
          // Filter to this community; events for others share the channel.
          if (p.communityId !== communityId) return;
          this.applyMembers(communityId, p.members);
        });
        this.unlisteners.set(communityId, unlisten);
      }
      const seed = await this.getPresence(communityId);
      this.applyMembers(communityId, seed);
    } catch (e: unknown) {
      const unlisten = this.unlisteners.get(communityId);
      if (unlisten) {
        unlisten();
        this.unlisteners.delete(communityId);
      }
      this.callbacks.delete(communityId);
      this.byCommunity.delete(communityId);
      if (this.activeCommunityId === communityId) this.activeCommunityId = null;
      await this.adapter
        .invoke('unsubscribe_community_presence', { communityId })
        .catch(() => {});
      throw new Error(e instanceof Error ? e.message : String(e));
    }
  }

  /**
   * Stop beaconing/subscribing for `communityId`, remove its `presence-updated`
   * listener, and drop its cached state. Idempotent.
   */
  async unsubscribe(communityId: string): Promise<void> {
    if (!this.adapter) {
      console.warn('PresenceService.unsubscribe: no adapter wired; ignoring');
      return;
    }
    const unlisten = this.unlisteners.get(communityId);
    if (unlisten) {
      unlisten();
      this.unlisteners.delete(communityId);
    }
    this.callbacks.delete(communityId);
    this.byCommunity.delete(communityId);
    if (this.activeCommunityId === communityId) this.activeCommunityId = null;
    try {
      await this.adapter.invoke('unsubscribe_community_presence', { communityId });
    } catch (e: unknown) {
      throw new Error(e instanceof Error ? e.message : String(e));
    }
  }

  /** Fetch the current online members for `communityId` (one-shot snapshot). */
  async getPresence(communityId: string): Promise<PresenceMemberDto[]> {
    if (!this.adapter) {
      console.warn('PresenceService.getPresence: no adapter wired; returning empty');
      return [];
    }
    try {
      return (await this.adapter.invoke('get_community_presence', {
        communityId,
      })) as PresenceMemberDto[];
    } catch (e: unknown) {
      throw new Error(e instanceof Error ? e.message : String(e));
    }
  }

  /**
   * True iff `ownerIdHex` is currently online in the ACTIVE community (the most
   * recently subscribed one). Scoped to the active community so a lingering
   * old-community map (e.g. during a community switch) can't leak a stale
   * answer. owner_id lookup is case-insensitive (keys are stored lowercased).
   */
  isOnline(ownerIdHex: string): boolean {
    if (!this.activeCommunityId) return false;
    return (
      this.byCommunity.get(this.activeCommunityId)?.get(ownerIdHex.toLowerCase())?.online ?? false
    );
  }

  /**
   * ZEB-600: count of online members in `communityId` (0 if unsubscribed or
   * unknown). Feeds the member-panel "N online" header. Self is never in the
   * roster (zenoh doesn't loop our own beacon), so callers that want to include
   * themselves add 1 separately.
   */
  onlineCount(communityId: string): number {
    const map = this.byCommunity.get(communityId);
    if (!map) return 0;
    let n = 0;
    for (const m of map.values()) if (m.online) n++;
    return n;
  }

  /**
   * ZEB-600: true iff at least one member OTHER THAN `selfOwnerIdHex` is online
   * in `communityId`. Drives the sidebar dot ("someone besides you is around"),
   * so a community where only you are present does not light up. Self is never
   * in the roster, but we exclude defensively (case-insensitive).
   */
  hasOthersOnline(communityId: string, selfOwnerIdHex: string): boolean {
    const map = this.byCommunity.get(communityId);
    if (!map) return false;
    const self = selfOwnerIdHex.toLowerCase();
    for (const m of map.values()) {
      if (m.online && m.ownerIdHex.toLowerCase() !== self) return true;
    }
    return false;
  }

  /**
   * ZEB-600: true iff `ownerIdHex` is online in ANY subscribed community. Drives
   * the DM-list dot. Caveat: a DM contact who shares no joined community with
   * you always reads offline — inherent to community-scoped presence.
   */
  isOnlineAnywhere(ownerIdHex: string): boolean {
    const key = ownerIdHex.toLowerCase();
    for (const map of this.byCommunity.values()) {
      if (map.get(key)?.online) return true;
    }
    return false;
  }

  /** ZEB-600: true iff `communityId` currently has a live subscription. */
  isSubscribed(communityId: string): boolean {
    return this.callbacks.has(communityId);
  }

  /**
   * ZEB-600: point {@link isOnline} at `communityId` without re-subscribing —
   * the subscribe-all model keeps every joined community live, so a
   * community-switch only needs to re-target the active roster. No-op if
   * `communityId` isn't subscribed (the switch path subscribes it instead).
   */
  setActive(communityId: string): void {
    if (this.callbacks.has(communityId)) this.activeCommunityId = communityId;
  }

  /** ZEB-600: tear down every live subscription (app unmount). Idempotent. */
  async unsubscribeAll(): Promise<void> {
    for (const id of [...this.callbacks.keys()]) {
      await this.unsubscribe(id).catch(() => {});
    }
  }

  /** Replace a community's cached presence map and notify its subscriber. */
  private applyMembers(communityId: string, members: PresenceMemberDto[]): void {
    const map = new Map<string, PresenceMemberDto>();
    for (const m of members) {
      map.set(m.ownerIdHex.toLowerCase(), m);
    }
    this.byCommunity.set(communityId, map);
    try {
      this.callbacks.get(communityId)?.(members);
    } catch (e) {
      console.error(`PresenceService onUpdate failed for ${communityId}:`, e);
    }
  }
}
