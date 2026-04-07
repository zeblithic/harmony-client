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
  private unlisteners: Array<() => void> = [];
  private seenIds = new Set<string>();

  constructor() {
    // Seed with mock data — real vines append on top.
    this.discoverVines = [...mockVines];
    for (const v of this.discoverVines) {
      this.seenIds.add(v.id);
      if (v.viewed) this.viewedIds.add(v.id);
    }
  }

  /** Connect a Tauri adapter and start listening for vine descriptors. */
  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return; // already wired; prevent duplicate listeners
    this.adapter = adapter;
    const unlisten = await adapter.listen(
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
    );
    this.unlisteners.push(unlisten);

    const unlistenReaction = await adapter.listen(
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
    );
    this.unlisteners.push(unlistenReaction);
  }

  /** Publish a vine via Tauri command. */
  async publish(
    videoCid: string,
    title?: string,
    reshareOf?: string,
  ): Promise<void> {
    if (this.adapter) {
      try {
        await this.adapter.invoke('publish_vine', {
          vine: { videoCid, title, reshareOf, creatorName: this.ownDisplayName },
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
      const creatorAddr =
        vine.creatorAddress === 'self'
          ? this.ownAddress
          : vine.creatorAddress;
      if (!creatorAddr) return; // ownAddress not yet fetched; skip publish
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
