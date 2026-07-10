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
  /**
   * Local viewed-state, joined from the Rust cache's persisted viewed-set.
   * Present on both the live `vine-received` payload and `list_vine_videos`
   * rows; older frontends ignored it (ZEB-612 S2 gap-fix).
   */
  viewed?: boolean;
  /** See VineVideo.originalRemoved (ZEB-670). */
  originalRemoved?: boolean;
}

/** Wire format for the `vine-removed` event (ZEB-670 tombstone applied). */
export interface VineRemovedEvent {
  vineId: string;
  videoCid: string;
  creatorAddress: string;
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

  /**
   * @param opts.seedMockData Whether to seed the discover feed with mock vines
   *   so the UI is never empty while iterating. Defaults to `import.meta.env.DEV`
   *   — true in `vite dev` / browser dev and under vitest, **false in a
   *   production `vite build`**. ZEB-546: Vines ship in the alpha surface, so a
   *   shipped build must NOT seed fake vines — real testers would otherwise see
   *   them before (or without ever reaching) a live connection. Tests pass an
   *   explicit flag to exercise both modes deterministically.
   */
  constructor(opts?: { seedMockData?: boolean }) {
    const seedMockData = opts?.seedMockData ?? import.meta.env.DEV;
    if (!seedMockData) return;
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

      localUnlisteners.push(await adapter.listen(
        'vine-removed',
        (event) => {
          // ZEB-670: creator tombstone applied by the backend cache
          // (covers both our own delete's loopback echo and remote
          // deletes — one path, mirroring vine-received).
          this.applyRemoval(event.payload as VineRemovedEvent);
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

  /**
   * ZEB-670: delete one of our own vines (creator tombstone).
   *
   * Connected: invokes `delete_vine`; the backend signs + publishes the
   * tombstone and the removal lands via the loopback `vine-removed` echo —
   * same path as remote deletes (mirrors `publish` → `vine-received`).
   * Disconnected/mock mode: applies the removal locally so the UI stays
   * responsive; real errors re-throw for the caller's error strip.
   */
  async deleteVine(vine: VineVideo): Promise<void> {
    if (this.adapter) {
      try {
        await this.adapter.invoke('delete_vine', { vineId: vine.id });
        return; // Backend echoes via subscription → vine-removed event
      } catch (err: unknown) {
        const msg = err instanceof Error ? err.message : String(err);
        // Only fall back locally when genuinely disconnected; re-throw real errors.
        if (!msg.includes('not connected') && !msg.includes('event loop')) {
          throw err;
        }
      }
    }

    // Offline/mock fallback: local removal only.
    this.applyRemoval({
      vineId: vine.id,
      videoCid: vine.videoCid,
      creatorAddress: vine.creatorAddress,
    });
  }

  /**
   * Apply a vine removal: drop the vine (feeds, seen/viewed/reaction
   * state) and mark surviving reshares of the removed content as
   * "Removed by creator" stubs. Stub rule mirrors the Rust cache
   * (`VineFeedCache::original_removed`): direct `reshareOf` id match, or
   * origin-creator + videoCid content match for chain reshares. (The
   * cache's re-publish exception is not replayed here — `list_vine_videos`
   * rows carry the authoritative flag on the next hydrate.)
   */
  private applyRemoval(removed: VineRemovedEvent): void {
    const { vineId, videoCid, creatorAddress } = removed;
    const hadVine = this.seenIds.has(vineId)
      || this.followedVines.some((v) => v.id === vineId)
      || this.discoverVines.some((v) => v.id === vineId);

    let changed = false;
    const markStubs = (list: VineVideo[]): VineVideo[] =>
      list.map((v) => {
        if (!v.reshareOf || v.originalRemoved) return v;
        const origin = v.originalCreatorAddress ?? v.creatorAddress;
        const isStub = v.reshareOf === vineId
          || (origin === creatorAddress && v.videoCid === videoCid);
        if (!isStub) return v;
        changed = true;
        return { ...v, originalRemoved: true };
      });

    this.followedVines = markStubs(this.followedVines.filter((v) => v.id !== vineId));
    this.discoverVines = markStubs(this.discoverVines.filter((v) => v.id !== vineId));
    this.seenIds.delete(vineId);
    this.reactionMap.delete(vineId);
    if (this.viewedIds.has(vineId)) {
      const next = new Set(this.viewedIds);
      next.delete(vineId);
      this.viewedIds = next;
    }
    if (hadVine || changed) this.onChange?.();
  }

  /**
   * ZEB-559: open the backend-owned native picker, ingest the chosen video into
   * CAS, and return its hex Video CID + display filename (or `null` if the user
   * cancels the picker). The backend owns the dialog so the renderer never
   * supplies a filesystem path (no arbitrary-file ingest). Throws if
   * disconnected or if ingest fails — the composer surfaces the message.
   */
  async ingestVideo(): Promise<{ cid: string; fileName: string } | null> {
    if (!this.adapter) throw new Error('not connected');
    const r = (await this.adapter.invoke('ingest_vine_video', {})) as
      | { videoCid: string; fileName: string }
      | null;
    return r ? { cid: r.videoCid, fileName: r.fileName } : null;
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
   * ZEB-612 S2: pull the Rust-persisted feed cache (`vine_feed.json`,
   * ZEB-147) into the frontend. The backend emits `vine-received` only on
   * FIRST insert, so after a restart the reloaded cache never re-emits —
   * without this pull the feed boots empty and the persisted viewed-set is
   * invisible. Call AFTER `loadFollowed()`: the DTO carries no `source`
   * field, so rows are classified by the live follow set (which matches
   * the follow()/unfollow() move semantics this service already applies).
   * Already-seen ids only merge viewed-state (live events may have raced
   * ahead of this call). Throws on IPC failure — the App-level tryConnect
   * wrapper logs it.
   */
  async hydrate(): Promise<void> {
    if (!this.adapter) return;
    const rows = (await this.adapter.invoke('list_vine_videos', {})) as Array<
      VineDescriptorEvent & { viewed: boolean }
    >;
    const viewed = new Set(this.viewedIds);
    const followedAdd: VineVideo[] = [];
    const discoverAdd: VineVideo[] = [];
    for (const wire of rows) {
      if (wire.viewed) viewed.add(wire.id);
      if (this.seenIds.has(wire.id)) continue;
      this.seenIds.add(wire.id);
      const vine = this.wireToVine(wire);
      if (vine.viewed) viewed.add(vine.id);
      if (this.followedAddresses.has(wire.creatorAddress)) {
        followedAdd.push(vine);
      } else {
        discoverAdd.push(vine);
      }
    }
    const viewedGrew = viewed.size !== this.viewedIds.size;
    if (followedAdd.length > 0) this.followedVines = [...this.followedVines, ...followedAdd];
    if (discoverAdd.length > 0) this.discoverVines = [...this.discoverVines, ...discoverAdd];
    if (viewedGrew) this.viewedIds = viewed;

    // ZEB-672: rebuild the reaction map from the Rust-persisted rows —
    // vine-reaction-received only fires on live receipt, so reloaded
    // reactions never re-emit. Same dedupe posture as the live handler
    // (the reactors set); rows are orphan-pruned by the cache, so the
    // known-vine check is belt-and-braces. Own rows resolve likedByMe,
    // which is why App.svelte hydrates only after fetchOwnAddress.
    const reactionRows = (await this.adapter.invoke('list_vine_reactions', {})) as Array<{
      vineId: string;
      reactorAddress: string;
      reactorName: string;
      liked: boolean;
      timestamp: number;
    }>;
    let reactionsChanged = false;
    for (const row of reactionRows) {
      if (!row.liked) continue; // unliked rows carry no boot-time count state
      const known = this.followedVines.some(v => v.id === row.vineId)
        || this.discoverVines.some(v => v.id === row.vineId);
      if (!known) continue;
      const entry = this.reactionMap.get(row.vineId)
        ?? { count: 0, likedByMe: false, reactors: new Set<string>() };
      if (!entry.reactors.has(row.reactorAddress)) {
        entry.reactors.add(row.reactorAddress);
        entry.count += 1;
        reactionsChanged = true;
      }
      if (this.ownAddress != null && row.reactorAddress === this.ownAddress && !entry.likedByMe) {
        entry.likedByMe = true;
        reactionsChanged = true;
      }
      this.reactionMap.set(row.vineId, entry);
    }

    if (viewedGrew || followedAdd.length > 0 || discoverAdd.length > 0 || reactionsChanged) {
      this.onChange?.();
    }
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

  /** ZEB-612 S3: how many vines (across both feeds) reference a video blob.
   *  Drives the Files detail panel's "Used by N vines" row — computed
   *  client-side from real descriptors; no backend involvement. */
  countByVideoCid(videoCid: string): number {
    return (
      this.followedVines.filter(v => v.videoCid === videoCid).length
      + this.discoverVines.filter(v => v.videoCid === videoCid).length
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
    // Count math is guarded on set membership so the toggle is idempotent
    // against state that already tracks us — e.g. a hydrated own-like row
    // (ZEB-672): liking again must not double-count, unliking must find
    // the row to remove. `setMutated` records whether THIS toggle changed
    // the set, so the rollback below undoes exactly what we did — rolling
    // back a set the optimistic step never touched would corrupt hydrated
    // state (Qodo PR #444).
    entry.likedByMe = newLiked;
    let setMutated = false;
    if (newLiked) {
      if (!entry.reactors.has(selfKey)) {
        entry.reactors.add(selfKey);
        entry.count += 1;
        setMutated = true;
      }
    } else if (entry.reactors.has(selfKey)) {
      entry.reactors.delete(selfKey);
      entry.count = Math.max(0, entry.count - 1);
      setMutated = true;
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
        // Rollback on failure — the exact inverse of the optimistic step:
        // only touch the set/count if the optimistic step actually did.
        // A no-op optimistic step over hydrated state (reactors already
        // tracked us) must roll back to a no-op (Qodo PR #444).
        entry.likedByMe = wasLiked;
        if (setMutated) {
          if (newLiked) {
            entry.reactors.delete(selfKey);
            entry.count = Math.max(0, entry.count - 1);
          } else {
            entry.reactors.add(selfKey);
            entry.count += 1;
          }
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
      viewed: wire.viewed === true || isSelf,
      originalRemoved: wire.originalRemoved === true,
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
