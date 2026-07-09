/**
 * ZEB-665 — per-channel unread counts for community channels.
 *
 * Maintains a capped per-channel Set of unread message IDs over a persisted
 * "newest-seen" HLC cursor (UnreadCursorStore, owner-scoped). The Set is the
 * load-bearing piece: live and backfilled messages arrive on the SAME
 * `channel-message-received` event with no distinguishing flag, so counting
 * events overcounts — an ID set gated by `hlc > cursor` dedupes re-delivery
 * for free, and the cap bounds memory exactly at the "99+" display ceiling.
 *
 * All side effects are injected (mention-alert.ts pattern) → unit-testable.
 * NavService renders the counts (`setUnread`); this service owns the model.
 *
 * Known v1 caveats (spec §6): start-clean and overflow-clear stamp from the
 * local wall clock, inheriting bounded clock-skew risk; the cross-device
 * OwnerState-CRDT follow-up (spec §8.1) replaces stamps with true head HLCs.
 */
import type { ChannelMessageDto } from './channel-message-service';
import type { ChannelInfo } from './community-service';
import type { Hlc } from './types';
import type { UnreadCursorStore } from './unread-cursor-store';
import { compareHlc, hlcNewer } from './hlc';

/** Internal tracking cap == the "99+" display ceiling (100 means "≥100"). */
export const UNREAD_TRACK_CAP = 100;

export interface ChannelUnreadDeps {
  /** Raw `list_channel_messages` IPC (exclusive strictly-newer `since`,
   *  server-capped; wired newest-first via `order: 'desc'` — ZEB-602 — so an
   *  over-cap seed keeps the newest messages; the service itself is
   *  order-agnostic). NOT ChannelMessageService.listMessages — that ingests
   *  into the feed cache and re-fires onMessage. */
  listMessagesSince(
    communityId: string,
    channelId: string,
    since: Hlc,
    limit: number,
  ): Promise<ChannelMessageDto[]>;
  setUnread(channelId: string, count: number): void;
  /** True iff the viewer is looking right at this community channel's feed
   *  (same resolver contract as MentionAlertDeps.isActiveChannel). */
  isActiveChannel(communityId: string, channelId: string): boolean;
  isFocused(): boolean;
  selfOwnerId(): string | null;
  storage: UnreadCursorStore;
  /** Wall clock (injectable for tests) — start-clean / clear stamps only. */
  now(): number;
}

export class ChannelUnreadService {
  /** Unread message IDs per channel, capped at UNREAD_TRACK_CAP. */
  private sets = new Map<string, Set<string>>();
  /** Newest HLC ever seen per channel (seeds AND events), for clear stamps. */
  private maxSeen = new Map<string, Hlc>();
  /** Channels seeded this session (cleared by connectOwner / removal). */
  private seeded = new Set<string>();
  /** Last materialized channel list per community, for owner-connect re-seed. */
  private channelsByCommunity = new Map<string, ChannelInfo[]>();

  constructor(private deps: ChannelUnreadDeps) {}

  /** (Re)connect the cursor store when the owner identity lands. Channels
   *  materialized pre-identity were start-clean no-ops (the store refuses
   *  pre-owner writes), so clear session state and re-seed everything. */
  connectOwner(ownerId: string): void {
    this.deps.storage.connectOwner(ownerId);
    this.sets.clear();
    this.maxSeen.clear();
    this.seeded.clear();
    for (const [communityId, channels] of this.channelsByCommunity) {
      void this.onChannelsMaterialized(communityId, channels);
    }
  }

  /** Called whenever a community's channels are (re)pushed into the nav.
   *  Seeds each channel once per session; always re-pushes counts (the nav
   *  nodes may have just been rebuilt with unreadCount preserved-or-zero). */
  async onChannelsMaterialized(communityId: string, channels: ChannelInfo[]): Promise<void> {
    this.channelsByCommunity.set(communityId, channels);
    await Promise.all(
      channels.map(async (info) => {
        const k = this.key(communityId, info.channelId);
        if (!this.seeded.has(k)) {
          this.seeded.add(k); // pre-mark so a concurrent re-materialize can't double-seed
          try {
            await this.seedChannel(communityId, info.channelId);
          } catch (e) {
            this.seeded.delete(k); // retried on the next materialize
            const msg = e instanceof Error ? e.message : String(e);
            console.warn(`[channel-unread] seed failed for ${communityId}:${info.channelId}:`, msg);
          }
        }
        this.push(communityId, info.channelId);
      }),
    );
  }

  /** Live per-message hook (also fires for list/backfill deliveries).
   *  Synchronous by design — cursor reads are localStorage-backed. */
  onMessage(communityId: string, channelId: string, message: ChannelMessageDto): void {
    this.bumpMaxSeen(communityId, channelId, message.at);
    if (message.author === this.deps.selfOwnerId()) return;
    const cursor = this.deps.storage.get(communityId, channelId);
    // No cursor yet → first-sight start-clean at materialize covers this
    // channel; counting here would violate the start-clean decision.
    if (cursor === null) return;
    if (this.deps.isFocused() && this.deps.isActiveChannel(communityId, channelId)) {
      // Looking right at it → THIS message is read the moment it lands. Do NOT
      // wipe the whole set: an unfocused-but-open backlog stays badged until
      // markChannelRead (spec §6; Qodo PR #430). Remove only this message's ID
      // (it can be present via re-emission of a previously-counted message).
      if (hlcNewer(message.at, cursor)) {
        this.deps.storage.set(communityId, channelId, message.at);
      }
      const set = this.sets.get(this.key(communityId, channelId));
      if (set?.delete(message.messageId)) this.push(communityId, channelId);
      return;
    }
    if (!hlcNewer(message.at, cursor)) return; // history replay / backfill of read messages
    const set = this.setFor(communityId, channelId);
    const before = set.size;
    if (set.size < UNREAD_TRACK_CAP || set.has(message.messageId)) {
      set.add(message.messageId);
    }
    if (set.size !== before) this.push(communityId, channelId);
  }

  /** Open-clears-all: stamp the cursor past everything we know about and wipe
   *  the set. With the newest-first seed (ZEB-602) maxSeen reflects the true
   *  newest even under overflow; the wall-clock stamp stays as belt-and-braces
   *  (e.g. a failed seed) so opening always clears. */
  markChannelRead(communityId: string, channelId: string): void {
    const candidates: Hlc[] = [{ wallMs: this.deps.now(), logical: 0, deviceId: '' }];
    const cursor = this.deps.storage.get(communityId, channelId);
    if (cursor) candidates.push(cursor);
    const seen = this.maxSeen.get(this.key(communityId, channelId));
    if (seen) candidates.push(seen);
    const newest = candidates.reduce((a, b) => (compareHlc(b, a) > 0 ? b : a));
    this.deps.storage.set(communityId, channelId, newest);
    this.sets.get(this.key(communityId, channelId))?.clear();
    this.push(communityId, channelId);
  }

  /** Drop session state for a removed community (cursors are kept — rejoin
   *  preserving read state is the better behavior, and they're tiny). */
  onCommunityRemoved(communityId: string): void {
    const channels = this.channelsByCommunity.get(communityId) ?? [];
    for (const info of channels) {
      const k = this.key(communityId, info.channelId);
      this.sets.delete(k);
      this.maxSeen.delete(k);
      this.seeded.delete(k);
    }
    this.channelsByCommunity.delete(communityId);
  }

  private async seedChannel(communityId: string, channelId: string): Promise<void> {
    const cursor = this.deps.storage.get(communityId, channelId);
    if (cursor === null) {
      // First sight: start clean (decision #4) — stamp "now", count from here.
      this.deps.storage.set(communityId, channelId, {
        wallMs: this.deps.now(),
        logical: 0,
        deviceId: '',
      });
      return;
    }
    const msgs = await this.deps.listMessagesSince(
      communityId,
      channelId,
      cursor,
      UNREAD_TRACK_CAP,
    );
    const self = this.deps.selfOwnerId();
    const set = this.setFor(communityId, channelId); // union with event-race arrivals
    for (const m of msgs) {
      this.bumpMaxSeen(communityId, channelId, m.at);
      if (m.author === self) continue;
      if (set.size < UNREAD_TRACK_CAP || set.has(m.messageId)) {
        set.add(m.messageId);
      }
    }
  }

  private push(communityId: string, channelId: string): void {
    const set = this.sets.get(this.key(communityId, channelId));
    this.deps.setUnread(channelId, set?.size ?? 0);
  }

  private setFor(communityId: string, channelId: string): Set<string> {
    const k = this.key(communityId, channelId);
    let set = this.sets.get(k);
    if (!set) {
      set = new Set();
      this.sets.set(k, set);
    }
    return set;
  }

  private bumpMaxSeen(communityId: string, channelId: string, at: Hlc): void {
    const k = this.key(communityId, channelId);
    const cur = this.maxSeen.get(k);
    if (!cur || compareHlc(at, cur) > 0) this.maxSeen.set(k, at);
  }

  /** Composite key matching the cursor store's blob keys. */
  private key(communityId: string, channelId: string): string {
    return `${communityId}:${channelId}`;
  }
}
