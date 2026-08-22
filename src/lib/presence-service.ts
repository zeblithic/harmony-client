import type { TauriAdapter } from './zenoh-service';
import { formatLastSeen, type TimeFormatPrefs } from './time-format';

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
 * ZEB-972: client-side staleness threshold. The backend evicts a silent owner
 * from the roster after 30 s (3× the 10 s beacon interval,
 * `community_presence::STALE_MS`), so under normal operation a cached row's
 * beacon stamp is never older than ~30 s plus event latency. 2× that TTL means
 * "the backend's eviction is overdue" — the sweep lives in the event loop and
 * is the SOLE eviction path, so an event-loop stall/death (the ZEB-970 wedge
 * incident) freezes rows in place and, without this guard, dots would stay
 * green forever. Generous enough to never flap during normal operation.
 */
export const PRESENCE_STALE_AFTER_MS = 60_000;

/**
 * ZEB-972: three-state presence for a dot renderer.
 *  - `online`  — roster row present with a fresh beacon (≤ {@link PRESENCE_STALE_AFTER_MS}).
 *  - `stale`   — roster row present but the beacon is overdue for backend
 *                eviction: the push pipeline itself is suspect, so the dot must
 *                stop claiming live presence.
 *  - `offline` — no roster row (normal absence, including backend eviction).
 */
export type PresenceState = 'online' | 'stale' | 'offline';

export interface PresenceDisplay {
  state: PresenceState;
  /**
   * Wall-clock ms of the most recent beacon observed for this owner in ANY
   * community this session (survives roster eviction — see `lastKnown`).
   * Absent when no beacon has ever been observed this session.
   */
  lastSeenMs?: number;
}

/**
 * ZEB-972: shared title/aria copy for a peer's presence dot, so MemberRow and
 * ChannelMembersPanel can't drift apart. The 1-minute "just now" floor matches
 * the 10 s beacon cadence (a stale row is ≥60 s old by definition, so its
 * label is always a real "~Nm ago"). Self/invisible copy stays component-local
 * — this covers peers only.
 */
export function presenceDotLabel(p: PresenceDisplay, now: number, prefs: TimeFormatPrefs): string {
  const seen =
    p.lastSeenMs === undefined
      ? undefined
      : formatLastSeen(p.lastSeenMs, now, prefs, { justNowUnderMin: 1 });
  if (p.state === 'online') return 'Online';
  if (p.state === 'stale') {
    return seen === undefined ? 'Connection may be stale' : `Last seen ${seen} — connection may be stale`;
  }
  return seen === undefined ? 'Offline' : `Offline · last seen ${seen}`;
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
   * ZEB-972: ownerIdHex(lc) -> newest beacon stamp ever observed this session,
   * across ALL communities. Max-merged in {@link applyMembers} and never
   * cleared (session-scoped memory), so an owner evicted from every roster
   * still yields a "last seen ~Xm ago" tooltip instead of a bare "Offline".
   */
  private lastKnown = new Map<string, number>();
  /** ZEB-972: injectable clock for deterministic staleness tests. */
  private nowFn: () => number;

  /**
   * Adapter is optional so callers can construct the service at boot (before
   * the Tauri runtime is wired) and inject the adapter later via
   * {@link setAdapter}. Network methods no-op (with a `console.warn`) when the
   * adapter is absent — mirrors MemberCardService so a non-connected boot
   * never crashes. (ZEB-537.)
   */
  constructor(
    private adapter?: TauriAdapter,
    opts?: { now?: () => number },
  ) {
    this.nowFn = opts?.now ?? Date.now;
  }

  /**
   * ZEB-972: true iff this roster row may claim LIVE presence — the backend's
   * `online` flag AND a beacon fresh enough that the backend's 30 s eviction
   * sweep is not overdue. Clock basis is safe: the stamp is the local
   * backend's `now_ms` at beacon receipt, compared against the local frontend
   * clock (same machine).
   */
  private isFresh(m: PresenceMemberDto): boolean {
    return m.online && this.nowFn() - m.lastSeenMs <= PRESENCE_STALE_AFTER_MS;
  }

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
    opts?: { setActive?: boolean },
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
    // Backend subscribe succeeded → point isOnline at this community UNLESS the
    // caller opts out. Boot subscribe-all subscribes every joined community but
    // passes setActive:false so a slow async completion can't clobber the user's
    // selected roster (CodeRabbit #381); the selection path uses the default.
    if (opts?.setActive ?? true) {
      this.activeCommunityId = communityId;
    }

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
    return this.presenceFor(ownerIdHex).state === 'online';
  }

  /**
   * ZEB-972: three-state presence for `ownerIdHex` in the ACTIVE community
   * (same scoping and case-insensitivity as {@link isOnline}). This is the
   * dot renderer's resolver: `online` needs a present row AND a fresh beacon;
   * a present row with an eviction-overdue beacon is `stale` (the push
   * pipeline is suspect — dot must stop claiming live presence); an absent
   * row is `offline`, carrying the session's last-known beacon stamp when one
   * exists so the tooltip can say "last seen ~Xm ago".
   */
  presenceFor(ownerIdHex: string): PresenceDisplay {
    const key = ownerIdHex.toLowerCase();
    const row = this.activeCommunityId
      ? this.byCommunity.get(this.activeCommunityId)?.get(key)
      : undefined;
    if (row?.online) {
      return this.isFresh(row)
        ? { state: 'online', lastSeenMs: row.lastSeenMs }
        : { state: 'stale', lastSeenMs: row.lastSeenMs };
    }
    const known = this.lastKnown.get(key);
    return known === undefined ? { state: 'offline' } : { state: 'offline', lastSeenMs: known };
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
    // ZEB-972: freshness-gated — a stale row must not inflate the header count.
    for (const m of map.values()) if (this.isFresh(m)) n++;
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
    // ZEB-972: freshness-gated — a stale row must not light the sidebar dot.
    for (const m of map.values()) {
      if (this.isFresh(m) && m.ownerIdHex.toLowerCase() !== self) return true;
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
    // ZEB-972: freshness-gated — a stale row must not light the DM-list dot.
    for (const map of this.byCommunity.values()) {
      const m = map.get(key);
      if (m && this.isFresh(m)) return true;
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
      const key = m.ownerIdHex.toLowerCase();
      map.set(key, m);
      // ZEB-972: max-merge the session-wide last-known beacon stamp BEFORE the
      // wholesale replace below discards evicted rows — max (not overwrite) so
      // an out-of-order older delivery can't regress it.
      const prev = this.lastKnown.get(key);
      if (prev === undefined || m.lastSeenMs > prev) this.lastKnown.set(key, m.lastSeenMs);
    }
    this.byCommunity.set(communityId, map);
    try {
      this.callbacks.get(communityId)?.(members);
    } catch (e) {
      console.error(`PresenceService onUpdate failed for ${communityId}:`, e);
    }
  }
}
