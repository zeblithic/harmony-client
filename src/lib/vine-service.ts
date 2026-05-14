import type { TauriAdapter } from './zenoh-service';
import type { VineVideo } from './types';
import { vineVideos as mockVines } from './mock-data';

/** Wire format for vine descriptors from the Rust backend. */
export interface VineDescriptorEvent {
  id: string;
  creatorAddress: string;
  creatorName: string;
  createdAt: number;
  videoCid: string;
  title?: string;
  reshareOf?: string;
  /** See VineVideo.originalCreatorAddress. */
  originalCreatorAddress?: string;
  /** See VineVideo.originalCreatorName. */
  originalCreatorName?: string;
  source?: 'followed' | 'discover';
}

/**
 * Manages real-time vine feed over Zenoh pub/sub.
 *
 * When connected, vine descriptors flow via Tauri IPC events (`vine-received`).
 * When disconnected (or in browser dev mode), seeds with mock data so the
 * UI is never empty. Call `connectAdapter()` to upgrade from offline to live.
 */
export class VineService {
  followedVines: VineVideo[] = [];
  discoverVines: VineVideo[] = [];
  /** Backwards-compat getter — combines both feeds. */
  get vines(): VineVideo[] {
    return [...this.followedVines, ...this.discoverVines];
  }
  /** Called whenever the vine list or viewed state changes so the UI can re-render. */
  onChange?: () => void;
  /** Hex-encoded node address — set after Zenoh connects so we can
   *  identify self-published vines in the echo. */
  ownAddress: string | null = null;
  /** Display name to include in outgoing vines. */
  ownDisplayName = 'You';
  /** Locally tracked viewed vine IDs. */
  viewedIds = new Set<string>();
  followedAddresses = new Set<string>();
  /** In-memory reaction state per vine. */
  private reactionMap = new Map<string, { count: number; likedByMe: boolean; reactors: Set<string> }>();
  /** Vine IDs with an in-flight toggleLike call — prevents concurrent mutations. */
  private likePending = new Set<string>();

  private adapter: TauriAdapter | null = null;
  private connecting = false;
  private unlisteners: Array<() => void> = [];
  private seenIds = new Set<string>();
  /** IDs of constructor-seeded mock vines — used to selectively clear
   *  mocks on `connectAdapter()` while preserving any locally-created
   *  entries from offline-fallback `publish()` calls during the pre-connect
   *  window (ZEB-209 bot R1). */
  private mockSeededIds = new Set<string>();

  constructor() {
    // Seed with mock data for browser/dev mode — `connectAdapter()` clears
    // these (selectively, preserving any locally-created entries from the
    // pre-connect window) before subscribing to real events (ZEB-209).
    this.discoverVines = [...mockVines];
    this.mockSeededIds = new Set(mockVines.map((v) => v.id));
    for (const v of this.discoverVines) {
      this.seenIds.add(v.id);
      if (v.viewed) this.viewedIds.add(v.id);
    }
  }

  /** Connect a Tauri adapter and start listening for vine descriptors. */
  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter || this.connecting) return; // already wired or in-progress; prevent duplicate listeners
    this.connecting = true;

    // ── R3: Selective clear FIRST so handlers see clean state ──
    // Preserves locally-created entries from offline-fallback publish() calls
    // during the pre-connect window. By the time any listener handler can
    // possibly fire, mock ids are already removed from seenIds and the feed
    // arrays, and mockSeededIds is empty — no mock-vs-real dedup collision
    // is possible. On listen failure the service is in a clean-but-unconnected
    // state; this.adapter is still null so a retry is allowed, and the
    // selective clear is idempotent (mockSeededIds is now empty).
    this.discoverVines = this.discoverVines.filter((v) => !this.mockSeededIds.has(v.id));
    this.followedVines = this.followedVines.filter((v) => !this.mockSeededIds.has(v.id));
    for (const id of this.mockSeededIds) {
      this.seenIds.delete(id);
      this.viewedIds.delete(id);
      this.reactionMap.delete(id);
    }
    this.mockSeededIds = new Set();
    // likePending is short-lived (set by user click), never mock-seeded — leave it alone.
    this.onChange?.();

    // ZEB-209 bot-feedback round 1: register listeners FIRST so a partial-init
    // failure doesn't wedge the service. Adapter and unlisteners are committed
    // only after all listens succeed; on failure we tear down partial work and
    // rethrow so the caller (App.svelte tryConnect) can retry.
    // ZEB-209 bot-feedback round 2: `connecting` sentinel (above) prevents a
    // concurrent caller from passing the guard while awaits are in flight.
    // ZEB-209 bot-feedback round 3: selective clear moved above listener
    // registration so handlers see clean state between successive awaits.
    const localUnlisteners: Array<() => void> = [];
    try { // outer try — releases connecting sentinel in finally on any exit path
    try {
      localUnlisteners.push(await adapter.listen(
        'vine-received',
        (event) => {
          const wire = event.payload as VineDescriptorEvent;
          if (this.seenIds.has(wire.id)) return;
          this.seenIds.add(wire.id);
          const vine = this.wireToVine(wire);
          if (vine.viewed) this.viewedIds = new Set([...this.viewedIds, vine.id]);
          if (wire.source === 'followed') {
            this.followedVines = [...this.followedVines, vine];
          } else {
            this.discoverVines = [...this.discoverVines, vine];
          }
          this.onChange?.();
        },
      ));

      localUnlisteners.push(await adapter.listen(
        'vine-reaction-received',
        (event) => {
          const wire = event.payload as {
            vineId: string;
            reactorAddress: string;
            reactorName: string;
            liked: boolean;
            timestamp: number;
          };

          // Skip self-echo — already applied optimistically
          if (wire.reactorAddress === 'self' || (this.ownAddress && wire.reactorAddress === this.ownAddress)) {
            return;
          }

          // Ignore reactions for vines not in our feed
          const known = this.followedVines.some(v => v.id === wire.vineId)
            || this.discoverVines.some(v => v.id === wire.vineId);
          if (!known) return;

          const entry = this.reactionMap.get(wire.vineId)
            ?? { count: 0, likedByMe: false, reactors: new Set<string>() };

          const alreadyTracked = entry.reactors.has(wire.reactorAddress);

          if (wire.liked) {
            if (alreadyTracked) return; // Already counted
            entry.reactors.add(wire.reactorAddress);
            entry.count += 1;
          } else {
            if (!alreadyTracked) return; // Nothing to remove
            entry.reactors.delete(wire.reactorAddress);
            entry.count = Math.max(0, entry.count - 1);
          }

          this.reactionMap.set(wire.vineId, entry);
          this.onChange?.();
        },
      ));
    } catch (err) {
      for (const fn of localUnlisteners) fn();
      // Note: mocks are already cleared and user-created entries preserved.
      // On listen failure the service is in a clean but unconnected state —
      // the App.svelte tryConnect wrapper logs. A retry is allowed
      // (this.adapter is still null), and the selective clear is idempotent
      // (mockSeededIds is now empty, so the filters are no-ops).
      throw err;
    }

    // Commit: assign adapter and register unlisteners.
    // (Selective clear was moved before listener registration per R3.)
    this.adapter = adapter;
    this.unlisteners.push(...localUnlisteners);
    } finally {
      this.connecting = false;
    }
  }

  /**
   * Publish a vine via Tauri command.
   *
   * `originalCreatorAddress` / `originalCreatorName` carry attribution for
   * reshares. See `VineVideo.originalCreatorAddress` for the "always traces
   * to true origin" semantics — callers must resolve the chain origin
   * before passing (typically `vine.originalCreator* ?? vine.creator*`,
   * done in `App.svelte::handleVineReshare`).
   *
   * Self-reshare prevention lives at the caller layer
   * (`App.svelte::handleVineReshare`) because the load-bearing signal is
   * the SOURCE vine's identity (`creatorAddress === 'self'/ownAddress`
   * AND `!reshareOf`), not the resolved `originalCreatorAddress`. A
   * reshare of someone else's reshare of our content correctly has
   * `originalCreatorAddress === 'self'` and MUST be allowed through (spec
   * §Edge Cases → Self-reshare prevention).
   */
  async publish(
    videoCid: string,
    title?: string,
    reshareOf?: string,
    originalCreatorAddress?: string,
    originalCreatorName?: string,
  ): Promise<void> {
    if (this.adapter) {
      try {
        await this.adapter.invoke('publish_vine', {
          vine: {
            videoCid,
            title,
            reshareOf,
            creatorName: this.ownDisplayName,
            originalCreatorAddress,
            originalCreatorName,
          },
        });
        return; // Backend will echo via subscription → vine-received event
      } catch (err: unknown) {
        const msg = err instanceof Error ? err.message : String(err);
        // Only fall back locally when genuinely disconnected; re-throw real errors.
        if (!msg.includes('not connected') && !msg.includes('event loop')) {
          throw err;
        }
      }
    }

    // Offline fallback: append locally so the UI stays responsive.
    const id = `vine-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
    this.seenIds.add(id);
    this.viewedIds = new Set([...this.viewedIds, id]);
    const vine: VineVideo = {
      id,
      creatorAddress: 'self',
      creatorName: this.ownDisplayName,
      createdAt: Math.floor(Date.now() / 1000),
      videoCid,
      title,
      reshareOf,
      originalCreatorAddress,
      originalCreatorName,
      viewed: true,
    };
    this.discoverVines = [...this.discoverVines, vine];
    this.onChange?.();
  }

  /** Mark a vine as viewed locally (and notify backend). */
  markViewed(id: string): void {
    if (this.viewedIds.has(id)) return;
    this.viewedIds = new Set([...this.viewedIds, id]);
    this.onChange?.();
    if (this.adapter) {
      this.adapter.invoke('mark_vine_viewed', { vineId: id }).catch(() => {});
    }
  }

  async follow(address: string, name?: string): Promise<void> {
    // Guard against following yourself — wireToVine remaps own address to
    // 'self', so the UI may pass either the real hex or 'self'.
    if (address === 'self' || (this.ownAddress && address === this.ownAddress)) {
      return;
    }
    if (this.adapter) {
      await this.adapter.invoke('follow_vine_creator', { address, name: name ?? null });
    }
    this.followedAddresses.add(address);
    const toMove = this.discoverVines.filter(v => v.creatorAddress === address);
    if (toMove.length > 0) {
      this.discoverVines = this.discoverVines.filter(v => v.creatorAddress !== address);
      this.followedVines = [...this.followedVines, ...toMove];
    }
    this.onChange?.();
  }

  async unfollow(address: string): Promise<void> {
    if (this.adapter) {
      await this.adapter.invoke('unfollow_vine_creator', { address });
    }
    this.followedAddresses.delete(address);
    const toMove = this.followedVines.filter(v => v.creatorAddress === address);
    this.followedVines = this.followedVines.filter(v => v.creatorAddress !== address);
    if (toMove.length > 0) {
      this.discoverVines = [...toMove, ...this.discoverVines];
    }
    this.onChange?.();
  }

  async loadFollowed(): Promise<void> {
    if (!this.adapter) return;
    try {
      const entries = await this.adapter.invoke('list_followed', {}) as Array<{
        address: string;
        name: string | null;
        followedAt: number;
      }>;
      for (const entry of entries) {
        this.followedAddresses.add(entry.address);
      }
      // Reconcile: move any vines from discover to followed that
      // arrived before the follow list was loaded.
      const toMove = this.discoverVines.filter(v => this.followedAddresses.has(v.creatorAddress));
      if (toMove.length > 0) {
        this.discoverVines = this.discoverVines.filter(v => !this.followedAddresses.has(v.creatorAddress));
        this.followedVines = [...this.followedVines, ...toMove];
      }
      // Always fire onChange so UI picks up followedAddresses changes
      // (e.g., follow buttons render correctly even when no vines to reconcile)
      this.onChange?.();
    } catch {
      // Not connected yet
    }
  }

  isFollowed(address: string): boolean {
    return this.followedAddresses.has(address);
  }

  /**
   * Find a vine by id, searching followedVines then discoverVines.
   * Returns the first match or undefined.
   *
   * Used by the UI when the user clicks an attribution link to navigate
   * to the original vine. If the original isn't in the local feed (e.g.,
   * creator isn't followed and the original wasn't surfaced in discover),
   * the click is silently ignored.
   */
  findVine(vineId: string): VineVideo | undefined {
    return (
      this.followedVines.find(v => v.id === vineId)
      ?? this.discoverVines.find(v => v.id === vineId)
    );
  }

  /**
   * Count how many vines in the local feed reshare the given vine id.
   *
   * Only meaningful for original vines (where the caller's vine has no
   * `reshareOf` itself). Counts across both followed and discover feeds.
   * Computed on demand — no separate state map kept.
   */
  getReshareCount(vineId: string): number {
    return (
      this.followedVines.filter(v => v.reshareOf === vineId).length
      + this.discoverVines.filter(v => v.reshareOf === vineId).length
    );
  }

  /** Get reaction state for a vine. Returns zero state if no reactions tracked. */
  getReaction(vineId: string): { count: number; likedByMe: boolean } {
    const entry = this.reactionMap.get(vineId);
    return entry
      ? { count: entry.count, likedByMe: entry.likedByMe }
      : { count: 0, likedByMe: false };
  }

  /** Toggle like on a vine with optimistic update. */
  async toggleLike(vine: VineVideo): Promise<void> {
    if (this.likePending.has(vine.id)) return; // Prevent concurrent toggleLike calls
    this.likePending.add(vine.id);
    try {
      await this._toggleLikeInner(vine);
    } finally {
      this.likePending.delete(vine.id);
    }
  }

  private async _toggleLikeInner(vine: VineVideo): Promise<void> {
    const entry = this.reactionMap.get(vine.id) ?? { count: 0, likedByMe: false, reactors: new Set<string>() };
    const wasLiked = entry.likedByMe;
    const newLiked = !wasLiked;

    // Optimistic update — use ownAddress when available so the reactor key
    // matches the real hex address in self-echo dedup (avoids double-count
    // if a self-echo arrives before ownAddress is set).
    const selfKey = this.ownAddress ?? 'self';
    // Migrate stale 'self' reactor entry if ownAddress was set after initial like
    if (selfKey !== 'self' && entry.reactors.has('self')) {
      entry.reactors.delete('self');
      entry.reactors.add(selfKey);
    }
    entry.likedByMe = newLiked;
    entry.count = Math.max(0, entry.count + (newLiked ? 1 : -1));
    if (newLiked) {
      entry.reactors.add(selfKey);
    } else {
      entry.reactors.delete(selfKey);
    }
    this.reactionMap.set(vine.id, entry);
    this.onChange?.();

    if (this.adapter) {
      // Can't safely publish without our own address — self-echo suppression
      // and reactor dedup both depend on knowing the real hex address.
      if (!this.ownAddress) return;
      const creatorAddr =
        vine.creatorAddress === 'self'
          ? this.ownAddress
          : vine.creatorAddress;
      try {
        await this.adapter.invoke('publish_vine_reaction', {
          reaction: {
            vineId: vine.id,
            vineCreatorAddress: creatorAddr,
            liked: newLiked,
            reactorName: this.ownDisplayName,
          },
        });
      } catch {
        // Rollback on failure
        entry.likedByMe = wasLiked;
        entry.count = Math.max(0, entry.count + (wasLiked ? 1 : -1));
        if (wasLiked) {
          entry.reactors.add(selfKey);
        } else {
          entry.reactors.delete(selfKey);
        }
        this.reactionMap.set(vine.id, entry);
        this.onChange?.();
      }
    }
  }

  /** Convert wire format to frontend VineVideo type. */
  private wireToVine(wire: VineDescriptorEvent): VineVideo {
    // Self-published vines echo back via Zenoh — map to 'self'/ownDisplayName.
    const isSelf = this.ownAddress != null && wire.creatorAddress === this.ownAddress;

    return {
      id: wire.id,
      creatorAddress: isSelf ? 'self' : wire.creatorAddress,
      creatorName: isSelf
        ? this.ownDisplayName
        : wire.creatorName || wire.creatorAddress.slice(0, 8),
      createdAt: wire.createdAt,
      videoCid: wire.videoCid,
      title: wire.title,
      reshareOf: wire.reshareOf,
      originalCreatorAddress: wire.originalCreatorAddress,
      originalCreatorName: wire.originalCreatorName,
      viewed: isSelf,
    };
  }

  /** Register an external unlisten handle (e.g. zenoh-status listener)
   *  so it gets cleaned up alongside the service. */
  addUnlisten(fn: () => void): void {
    this.unlisteners.push(fn);
  }

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
  }
}
