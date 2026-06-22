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
 * answers across all subscribed communities (lowercased owner_id lookup).
 */
export class PresenceService {
  /** communityId -> (ownerIdHex(lc) -> presence). */
  private byCommunity = new Map<string, Map<string, PresenceMemberDto>>();
  /** communityId -> unlisten fn for that community's `presence-updated` listener. */
  private unlisteners = new Map<string, () => void>();
  /** communityId -> the caller's onUpdate callback. */
  private callbacks = new Map<string, (members: PresenceMemberDto[]) => void>();

  constructor(private adapter: TauriAdapter) {}

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
    this.callbacks.set(communityId, onUpdate);
    try {
      await this.adapter.invoke('subscribe_community_presence', { communityId });
    } catch (e: unknown) {
      this.callbacks.delete(communityId);
      throw new Error(e instanceof Error ? e.message : String(e));
    }

    if (!this.unlisteners.has(communityId)) {
      const unlisten = await this.adapter.listen('presence-updated', (event) => {
        const p = event.payload as PresenceUpdatedPayload;
        // Filter to this community; events for others share the channel.
        if (p.communityId !== communityId) return;
        this.applyMembers(communityId, p.members);
      });
      this.unlisteners.set(communityId, unlisten);
    }

    // Seed initial state (and fire onUpdate) from the authoritative snapshot.
    const seed = await this.getPresence(communityId);
    this.applyMembers(communityId, seed);
  }

  /**
   * Stop beaconing/subscribing for `communityId`, remove its `presence-updated`
   * listener, and drop its cached state. Idempotent.
   */
  async unsubscribe(communityId: string): Promise<void> {
    const unlisten = this.unlisteners.get(communityId);
    if (unlisten) {
      unlisten();
      this.unlisteners.delete(communityId);
    }
    this.callbacks.delete(communityId);
    this.byCommunity.delete(communityId);
    try {
      await this.adapter.invoke('unsubscribe_community_presence', { communityId });
    } catch (e: unknown) {
      throw new Error(e instanceof Error ? e.message : String(e));
    }
  }

  /** Fetch the current online members for `communityId` (one-shot snapshot). */
  async getPresence(communityId: string): Promise<PresenceMemberDto[]> {
    try {
      return (await this.adapter.invoke('get_community_presence', {
        communityId,
      })) as PresenceMemberDto[];
    } catch (e: unknown) {
      throw new Error(e instanceof Error ? e.message : String(e));
    }
  }

  /**
   * True iff `ownerIdHex` is currently online in any subscribed community.
   * owner_id lookup is case-insensitive (keys are stored lowercased).
   */
  isOnline(ownerIdHex: string): boolean {
    const key = ownerIdHex.toLowerCase();
    for (const members of this.byCommunity.values()) {
      if (members.get(key)?.online) return true;
    }
    return false;
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
