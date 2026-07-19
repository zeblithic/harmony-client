/**
 * ZEB-666 — per-thread unread counts for DM / group-DM nav rows.
 *
 * Sibling of ChannelUnreadService (ZEB-665), NOT a generalization: it
 * consumes DM shapes (the raw `dm-received` payload and `read_dm_thread`
 * page entries), which carry bare wall-clock ms instead of full HLCs.
 * The cursor is a `receivedAt` watermark (strict >) stored as a pseudo-HLC
 * `{ wallMs, logical: 0, deviceId: '' }` in the shared UnreadCursorStore
 * under the 'dm' namespace (key `dm:<spaceId>`), so the ZEB-665 storage
 * machinery works unchanged. Local-arrival ordering is the right unread
 * semantic (immune to sender clock skew) and matches `read_dm_thread`'s own
 * pagination key.
 *
 * Known v1 caveat (spec §4.1): equal-millisecond ties at a restart boundary
 * are swallowed from the count (strict > on bare ms). Rare, self-heals on
 * open; the root fix is full HLC on the DM path (spec §6.1 follow-up).
 *
 * All side effects are injected → unit-testable. NavService renders the
 * counts (`setUnread`); this service owns the model.
 */
import type { Hlc } from './types';
import type { UnreadCursorStore } from './unread-cursor-store';
import { UNREAD_TRACK_CAP } from './channel-unread-service';
import { isMissedCallHex } from './call-log';

/** Store namespace: cursor keys are `dm:<spaceId>`. Channel keys are
 *  `<communityId>:<channelId>` with 32-hex community ids — no collision. */
const NS = 'dm';

/** The subset of a `read_dm_thread` DmThreadMessage this service reads.
 *  The page is newest-first (backend-hardcoded), so an over-cap seed
 *  naturally keeps the newest cap-worth — no ZEB-602-style ordering work. */
export interface DmThreadPageEntry {
  messageCid: string;
  from: string;
  receivedAt: number;
  isSelfOutbound: boolean;
  /** ZEB-357: missed-call classification (call-event mime + hex body). */
  mimeType: string;
  body: string;
}

/** The subset of a `dm-received` payload this service reads (the full
 *  MessageService payload is structurally assignable). */
export interface DmArrival {
  spaceId: string;
  messageCid: string;
  from: string;
  receivedAt: number;
  /** ZEB-357: missed-call classification (call-event mime + hex body). */
  mimeType: string;
  body: string;
}

export interface DmUnreadDeps {
  /** Raw `read_dm_thread` invoke (newest-first page, `beforeHlc: null`).
   *  NOT MessageService.loadDmThread — that ingests into the feed cache and
   *  advances its own pagination cursor. */
  listThreadPage(spaceId: string, limit: number): Promise<DmThreadPageEntry[]>;
  /** ZEB-357: `missedCalls` is the unseen missed-class call-event subset of
   *  `count` — rendered as a distinct 📞 badge on the DM nav row. */
  setUnread(spaceId: string, count: number, missedCalls: number): void;
  /** True iff the viewer is looking right at this DM thread's feed. */
  isActiveThread(spaceId: string): boolean;
  isFocused(): boolean;
  selfOwnerId(): string | null;
  storage: UnreadCursorStore;
  /** Wall clock (injectable for tests) — start-clean / clear stamps only. */
  now(): number;
}

const pseudoHlc = (wallMs: number): Hlc => ({ wallMs, logical: 0, deviceId: '' });

export class DmUnreadService {
  /** Unread message CIDs per space, capped at UNREAD_TRACK_CAP. */
  private sets = new Map<string, Set<string>>();
  /** ZEB-357: the missed-call subset of `sets` — CIDs of unseen missed-class
   *  call events. Same lifecycle as the unread sets (cursor-gated add,
   *  open-clears-all, cleared on owner reconnect / space removal); membership
   *  in the capped unread set is what gates an add, so this can never exceed
   *  the tracked unread population. */
  private missedSets = new Map<string, Set<string>>();
  /** Newest receivedAt ever seen per space (seeds AND arrivals), for clear stamps. */
  private maxSeen = new Map<string, number>();
  /** Spaces seeded this session (cleared by connectOwner / removal). */
  private seeded = new Set<string>();
  /** Spaces materialized in the nav, for owner-connect re-seed. */
  private materialized = new Set<string>();

  constructor(private deps: DmUnreadDeps) {}

  /** (Re)connect the cursor store when the owner identity lands. Spaces
   *  materialized pre-identity were start-clean no-ops (the store refuses
   *  pre-owner writes), so clear session state and re-seed everything. */
  connectOwner(ownerId: string): void {
    this.deps.storage.connectOwner(ownerId);
    this.sets.clear();
    this.missedSets.clear();
    this.maxSeen.clear();
    this.seeded.clear();
    for (const spaceId of this.materialized) {
      void this.onDmSpaceMaterialized(spaceId);
    }
  }

  /** Called whenever a DM / group-DM nav row is (re)pushed into the nav —
   *  boot rehydration, runtime nav-updated, or handleDmCreate. Seeds once
   *  per session; always re-pushes the count (the nav node may have just
   *  been rebuilt with unreadCount preserved-or-zero). */
  async onDmSpaceMaterialized(spaceId: string): Promise<void> {
    this.materialized.add(spaceId);
    if (!this.seeded.has(spaceId)) {
      this.seeded.add(spaceId); // pre-mark so a concurrent re-materialize can't double-seed
      try {
        await this.seedSpace(spaceId);
      } catch (e) {
        this.seeded.delete(spaceId); // retried on the next materialize
        const msg = e instanceof Error ? e.message : String(e);
        console.warn(`[dm-unread] seed failed for ${spaceId}:`, msg);
      }
    }
    this.push(spaceId);
  }

  /** Live per-message hook (fires once per CID — `apply_inbox` is idempotent
   *  and MessageService dedups by messageCid before calling this; the capped
   *  CID-set still dedupes defensively). Synchronous by design. */
  onDmReceived(p: DmArrival): void {
    this.bumpMaxSeen(p.spaceId, p.receivedAt);
    if (p.from === this.deps.selfOwnerId()) return;
    const cursor = this.deps.storage.get(NS, p.spaceId);
    // No cursor yet → first-sight start-clean at materialize covers this
    // space; counting here would violate the start-clean decision.
    if (cursor === null) return;
    if (this.deps.isFocused() && this.deps.isActiveThread(p.spaceId)) {
      // Looking right at it → THIS message is read the moment it lands. Do
      // NOT wipe the whole set: an unfocused-but-open backlog stays badged
      // until markThreadRead. Remove only this message's CID (it can be
      // present via re-delivery of a previously-counted message).
      if (p.receivedAt > cursor.wallMs) {
        this.deps.storage.set(NS, p.spaceId, pseudoHlc(p.receivedAt));
      }
      const set = this.sets.get(p.spaceId);
      const removedMissed = this.missedSets.get(p.spaceId)?.delete(p.messageCid) ?? false;
      if ((set?.delete(p.messageCid) ?? false) || removedMissed) this.push(p.spaceId);
      return;
    }
    if (p.receivedAt <= cursor.wallMs) return; // history replay of read messages
    const set = this.setFor(p.spaceId);
    const before = set.size;
    if (set.size < UNREAD_TRACK_CAP || set.has(p.messageCid)) {
      set.add(p.messageCid);
      // ZEB-357: tracked missed-class call events feed the 📞 badge too.
      if (isMissedCallHex(p.mimeType, p.body)) {
        this.missedSetFor(p.spaceId).add(p.messageCid);
      }
    }
    if (set.size !== before) this.push(p.spaceId);
  }

  /** Open-clears-all: stamp the cursor past everything we know about and
   *  wipe the set. maxSeen reflects the true newest even under overflow
   *  (the newest-first page bumps it); the wall-clock stamp stays as
   *  belt-and-braces (e.g. a failed seed) so opening always clears. */
  markThreadRead(spaceId: string): void {
    const candidates = [this.deps.now()];
    const cursor = this.deps.storage.get(NS, spaceId);
    if (cursor) candidates.push(cursor.wallMs);
    const seen = this.maxSeen.get(spaceId);
    if (seen !== undefined) candidates.push(seen);
    this.deps.storage.set(NS, spaceId, pseudoHlc(Math.max(...candidates)));
    this.sets.get(spaceId)?.clear();
    this.missedSets.get(spaceId)?.clear();
    this.push(spaceId);
  }

  /** Drop session state for a removed DM row (cursor is kept — a re-added
   *  thread preserving read state is the better behavior, and it's tiny). */
  onDmSpaceRemoved(spaceId: string): void {
    this.sets.delete(spaceId);
    this.missedSets.delete(spaceId);
    this.maxSeen.delete(spaceId);
    this.seeded.delete(spaceId);
    this.materialized.delete(spaceId);
  }

  private async seedSpace(spaceId: string): Promise<void> {
    const cursor = this.deps.storage.get(NS, spaceId);
    if (cursor === null) {
      // First sight: start clean — stamp "now", count from here.
      this.deps.storage.set(NS, spaceId, pseudoHlc(this.deps.now()));
      return;
    }
    const page = await this.deps.listThreadPage(spaceId, UNREAD_TRACK_CAP);
    // Re-read the cursor: it may have advanced during the await —
    // markThreadRead from a user click, or a focused+active live arrival —
    // and filtering against the pre-await snapshot would resurrect
    // just-read messages (CodeRabbit PR #432). Cursors only move forward,
    // so the fresh read is always the stricter filter.
    const fresh = this.deps.storage.get(NS, spaceId) ?? cursor;
    const set = this.setFor(spaceId); // union with event-race arrivals
    for (const m of page) {
      this.bumpMaxSeen(spaceId, m.receivedAt);
      if (m.isSelfOutbound) continue;
      if (m.receivedAt <= fresh.wallMs) continue; // page is uncursored; filter client-side
      if (set.size < UNREAD_TRACK_CAP || set.has(m.messageCid)) {
        set.add(m.messageCid);
        // ZEB-357: tracked missed-class call events feed the 📞 badge too.
        if (isMissedCallHex(m.mimeType, m.body)) {
          this.missedSetFor(spaceId).add(m.messageCid);
        }
      }
    }
  }

  private push(spaceId: string): void {
    const set = this.sets.get(spaceId);
    this.deps.setUnread(spaceId, set?.size ?? 0, this.missedSets.get(spaceId)?.size ?? 0);
  }

  private setFor(spaceId: string): Set<string> {
    let set = this.sets.get(spaceId);
    if (!set) {
      set = new Set();
      this.sets.set(spaceId, set);
    }
    return set;
  }

  private missedSetFor(spaceId: string): Set<string> {
    let set = this.missedSets.get(spaceId);
    if (!set) {
      set = new Set();
      this.missedSets.set(spaceId, set);
    }
    return set;
  }

  private bumpMaxSeen(spaceId: string, receivedAt: number): void {
    const cur = this.maxSeen.get(spaceId);
    if (cur === undefined || receivedAt > cur) this.maxSeen.set(spaceId, receivedAt);
  }
}
