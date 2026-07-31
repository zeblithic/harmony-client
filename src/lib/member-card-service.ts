import type { TauriAdapter } from './zenoh-service';

export interface ResolvedCard {
  displayName: string;
  statusText: string;
  avatarUrl?: string;
  /** ZEB-345: CID (hex) of the member's long-form profile-page doc, if any.
   *  Pass-through (unlike avatarCid, no URL resolution): the ProfilePanel feeds
   *  it to the lazy ProfilePageResolver only when the panel opens. */
  profilePageRoot?: string;
}

/**
 * Wire shape of the `get_cached_member_card` IPC return value. The Rust DTO
 * uses `#[serde(rename_all = "camelCase")]`, so keys are camelCase.
 */
export interface DiscoveredCardInfo {
  /** Hex-encoded owner_id (32 hex chars, lowercase). */
  ownerIdHex: string;
  displayName: string;
  statusText: string;
  avatarCid?: string;
  /** ZEB-345: CID (hex) of the member's long-form profile-page doc, if any.
   *  Matches the backend DiscoveredCardInfo serde `profilePageRoot`. */
  profilePageRoot?: string;
}

/** Poll cadence for the cache-drain loop. 3s balances name-fill latency
 *  against IPC chatter for a panel that may show dozens of members. */
const POLL_INTERVAL_MS = 3000;

/**
 * Resolves a community member's owner_id -> {displayName, statusText}.
 *
 * Self (seeded via {@link seedSelf}) is authoritative and never subscribed
 * to nor overwritten by the network poll. Peers are resolved by subscribing
 * to each wanted owner_id's card broadcast (via named buckets, {@link
 * setBucket}) and draining the server-side cache on a poll loop.
 *
 * Reactivity: this stays a plain class with a plain `Map` (no runes). After
 * any poll mutates the map, {@link onUpdate} fires so the owner (App.svelte)
 * can bump a `$state` version counter and re-run the `$derived` consumers
 * that call {@link resolve}.
 *
 * owner_id keys are lowercase 32-char hex (matches MemberInfoDto.addr /
 * msg.author).
 */
export class MemberCardService {
  private cards: Map<string, ResolvedCard> = new Map();
  /** ownerIdHex(lc) -> subscriptionId for each active peer subscription. The
   *  set of keys here is always the UNION of every bucket (minus self). */
  private subs = new Map<string, number>();
  /**
   * Named subscription buckets: bucketName -> the owner hexes (lowercase) that
   * source wants subscribed. Each driver (community roster, friends, voice,
   * groupCall, dm) owns exactly one bucket and sets it independently; the
   * backend subscription set is reconciled to the union of all buckets, so no
   * driver can clobber another's subscriptions (ZEB-840). Replaces the old
   * single-set `subscribeVisible` reconcile, whose "the argument is the whole
   * desired set" contract forced a call-site merge funnel and a second service
   * instance.
   */
  private buckets = new Map<string, Set<string>>();
  /** Set in seedSelf; excluded from subscriptions and never cached over. */
  private selfKey?: string;
  private pollHandle: ReturnType<typeof setInterval> | null = null;
  /** Guards against overlapping ticks when an IPC round-trip is slow. */
  private polling = false;
  /**
   * Serializes the async mutators of `subs` ({@link subscribeVisible} /
   * {@link unsubscribeAll}) so they never interleave across their `await`
   * points. Without this, a panel unmount (unsubscribeAll) racing a remount
   * (subscribeVisible) on a community switch could let `unsubscribeAll`'s final
   * `subs.clear()` wipe an entry the concurrent `subscribeVisible` just added,
   * orphaning a backend subscription with no frontend tracking.
   */
  private opChain: Promise<void> = Promise.resolve();

  /** Run `fn` after any in-flight subs-mutating op completes. */
  private runExclusive(fn: () => Promise<void>): Promise<void> {
    const next = this.opChain.then(fn, fn);
    // Keep the chain alive regardless of fn's outcome (a rejection must not
    // poison every subsequent op).
    this.opChain = next.catch(() => {});
    return next;
  }

  private avatarResolver?: { resolve: (cid: string) => string | undefined };
  /** ownerIdHex(lc) -> the card's avatarCid hex, tracked so onAvatarsRefreshed
   *  can re-resolve to a blob URL once the resolver fetches it. */
  private cardAvatarCids = new Map<string, string>();

  /**
   * Adapter is optional so the synchronous seedSelf/resolve unit tests can
   * construct the service with no Tauri runtime. Network methods no-op (with
   * a console.warn) when the adapter is absent, so a non-connected boot never
   * crashes.
   */
  constructor(private adapter?: TauriAdapter) {}

  /** Called once after any poll mutates the card map (or on seedSelf). */
  public onUpdate?: () => void;

  /** Wire the Tauri adapter after it becomes available (post-boot). */
  setAdapter(adapter: TauriAdapter): void {
    this.adapter = adapter;
    // The adapter can arrive AFTER buckets were set — App wires Tauri
    // asynchronously while drivers may set the community/dm buckets during boot.
    // reconcileToUnion no-ops without an adapter, so replay the stored union now
    // that the backend is reachable; otherwise those subscriptions would never
    // start until the next bucket mutation happened to re-run reconciliation.
    if (this.buckets.size > 0) {
      void this.runExclusive(() => this.reconcileToUnion());
    }
  }

  /** Attach the shared AvatarResolver. Does NOT touch resolver.onChange — the
   *  owner (App.svelte) owns a combined onChange that calls both nav-service
   *  and this service's onAvatarsRefreshed(). */
  setAvatarResolver(resolver: { resolve: (cid: string) => string | undefined }): void {
    this.avatarResolver = resolver;
  }

  private resolveAvatarUrl(avatarCid?: string): string | undefined {
    if (!avatarCid || !this.avatarResolver) return undefined;
    return this.avatarResolver.resolve(avatarCid);
  }

  /** Re-resolve avatarUrls for cards whose avatarCid newly resolved (called by
   *  App after the AvatarResolver fetches a blob URL). Fires onUpdate if any
   *  card changed. */
  onAvatarsRefreshed(): void {
    let changed = false;
    for (const [owner, card] of this.cards) {
      const cid = this.cardAvatarCids.get(owner);
      if (!cid) continue;
      const url = this.resolveAvatarUrl(cid);
      if (url && card.avatarUrl !== url) {
        this.cards.set(owner, { ...card, avatarUrl: url });
        changed = true;
      }
    }
    if (changed) this.onUpdate?.();
  }

  /** Seed (or overwrite) the self owner_id from the local profile. Synchronous.
   *  When the profile carries an `avatarCid` but no live blob `avatarUrl` (e.g.
   *  after a reload, where the session-local blob URL was stripped before
   *  persisting), resolve the avatar from its CID via the AvatarResolver —
   *  otherwise the self row/messages fall back to an identicon. The resolver
   *  kicks off a fetch on a miss; `onAvatarsRefreshed()` (which does NOT skip
   *  self) fills in the URL when it lands. */
  seedSelf(ownerIdHex: string, profile: ResolvedCard & { avatarCid?: string }): void {
    this.selfKey = ownerIdHex.toLowerCase();
    // Track the self avatarCid for later resolution ONLY when there is no live
    // blob preview. While a session-local `blob:` avatarUrl is active (just
    // uploaded), tracking the cid would let onAvatarsRefreshed() overwrite that
    // live preview with the resolver URL once the fetch completes. After a
    // reload (no blob) the cid IS tracked so the avatar resolves from CAS.
    if (profile.avatarCid && !profile.avatarUrl) {
      this.cardAvatarCids.set(this.selfKey, profile.avatarCid);
    } else {
      this.cardAvatarCids.delete(this.selfKey);
    }
    const avatarUrl = profile.avatarUrl ?? this.resolveAvatarUrl(profile.avatarCid);
    this.cards.set(this.selfKey, {
      displayName: profile.displayName,
      statusText: profile.statusText,
      avatarUrl,
      profilePageRoot: profile.profilePageRoot,
    });
    this.onUpdate?.();
    // If a bucket already subscribed to this owner before we knew it was self
    // (e.g. the community roster includes self, set before boot resolved the
    // local identity), reconcile so the now-self owner is unsubscribed. No-op
    // when no adapter/buckets exist yet (the common seedSelf-first path).
    if (this.adapter && this.buckets.size > 0) {
      void this.runExclusive(() => this.reconcileToUnion());
    }
  }

  /** Resolve an owner_id to its card, or undefined if unresolved. */
  resolve(ownerIdHex: string): ResolvedCard | undefined {
    return this.cards.get(ownerIdHex.toLowerCase());
  }

  /** Apply a card delivered via the `member-card-received` push event (instant
   *  path; the poll loop is the fallback). Idempotent with the poll. Never
   *  overwrites the locally-seeded self entry. */
  applyCard(ownerIdHex: string, card: ResolvedCard & { avatarCid?: string }): void {
    const key = ownerIdHex.toLowerCase();
    if (key === this.selfKey) return; // self stays authoritative (seedSelf)
    // Track the CURRENT avatarCid. A card that drops its avatar must clear the
    // tracked cid AND the avatarUrl so a stale blob URL doesn't linger.
    if (card.avatarCid) {
      this.cardAvatarCids.set(key, card.avatarCid);
    } else {
      this.cardAvatarCids.delete(key);
    }
    const avatarUrl = card.avatarCid ? this.resolveAvatarUrl(card.avatarCid) : undefined;
    const next: ResolvedCard = {
      displayName: card.displayName,
      statusText: card.statusText,
      avatarUrl,
      profilePageRoot: card.profilePageRoot,
    };
    const prev = this.cards.get(key);
    if (
      prev &&
      prev.displayName === next.displayName &&
      prev.statusText === next.statusText &&
      prev.avatarUrl === next.avatarUrl &&
      prev.profilePageRoot === next.profilePageRoot
    ) {
      return; // unchanged — no churn
    }
    this.cards.set(key, next);
    this.onUpdate?.();
  }

  /**
   * Set (or replace) the owner-id set for a named subscription bucket, then
   * reconcile the backend subscription set to the UNION of all buckets.
   * Independent buckets never clobber each other — each driver owns exactly one
   * bucket (community / friends / voice / groupCall / dm). Passing an empty
   * array clears this bucket's contribution to the union. Excludes self (kept
   * authoritative via seedSelf). Idempotent; serialized via {@link opChain}.
   */
  setBucket(name: string, ownerIdHexes: string[]): Promise<void> {
    return this.runExclusive(() => this.setBucketImpl(name, ownerIdHexes));
  }

  /** Clear a named bucket's contribution to the union. Alias for
   *  `setBucket(name, [])`. */
  clearBucket(name: string): Promise<void> {
    return this.setBucket(name, []);
  }

  private async setBucketImpl(name: string, ownerIdHexes: string[]): Promise<void> {
    const set = new Set(ownerIdHexes.map((h) => h.toLowerCase()));
    if (set.size === 0) {
      this.buckets.delete(name);
    } else {
      this.buckets.set(name, set);
    }
    await this.reconcileToUnion();
  }

  /**
   * Reconcile `this.subs` to the union of every bucket (minus self): subscribe
   * newly-wanted owners, unsubscribe owners no bucket wants anymore. Self is
   * filtered here (not at bucket-store time) so a later {@link seedSelf} still
   * excludes it. Caller MUST hold the opChain (via {@link runExclusive}).
   */
  private async reconcileToUnion(): Promise<void> {
    if (!this.adapter) {
      console.warn('MemberCardService.setBucket: no adapter; skipping');
      return;
    }
    const wanted = new Set<string>();
    for (const bucket of this.buckets.values()) {
      for (const owner of bucket) {
        if (owner !== this.selfKey) wanted.add(owner);
      }
    }

    // Unsubscribe owners no bucket wants anymore.
    for (const [owner, subscriptionId] of [...this.subs]) {
      if (!wanted.has(owner)) {
        try {
          await this.adapter.invoke('unsubscribe_member_card', { subscriptionId });
        } catch (e) {
          console.warn(
            'unsubscribe_member_card failed',
            e instanceof Error ? e.message : String(e),
          );
        }
        this.subs.delete(owner);
      }
    }

    // Subscribe newly-wanted owners.
    for (const ownerIdHex of wanted) {
      if (this.subs.has(ownerIdHex)) continue;
      try {
        const id = (await this.adapter.invoke('subscribe_member_card', {
          ownerIdHex,
        })) as number;
        this.subs.set(ownerIdHex, id);
      } catch (e) {
        console.warn(
          'subscribe_member_card failed',
          e instanceof Error ? e.message : String(e),
        );
      }
    }

    // Start the poll loop only while at least one subscription is active; stop
    // it when reconciliation has drained the union to empty (every bucket
    // cleared, or all owners filtered as self). Without the else-branch the 3s
    // interval would keep firing over an empty `subs` map until `unsubscribeAll`
    // is explicitly called — wasted IPC-free ticks for the life of the view.
    if (this.subs.size > 0) {
      this.ensurePolling();
    } else {
      this.stopPolling();
    }
  }

  /** Start the poll loop on first subscribe; no-op if already running. */
  private ensurePolling(): void {
    if (this.pollHandle !== null) return;
    this.pollHandle = setInterval(() => {
      void this.pollOnce();
    }, POLL_INTERVAL_MS);
  }

  /** Cancel the poll loop if running. Idempotent. */
  private stopPolling(): void {
    if (this.pollHandle !== null) {
      clearInterval(this.pollHandle);
      this.pollHandle = null;
    }
  }

  /**
   * Drain the server-side cache once for every active subscription. Exposed
   * for testability (the interval calls it). Per-sub IPC errors are swallowed
   * (warn + continue) so one failing sub doesn't kill the loop. Fires
   * onUpdate at most once per tick, only if a card actually changed.
   */
  async pollOnce(): Promise<void> {
    if (!this.adapter) return;
    if (this.polling) return;
    this.polling = true;
    try {
      let changed = false;
      for (const [owner, subscriptionId] of [...this.subs]) {
        // Self stays authoritative — never cache a network card over it.
        if (owner === this.selfKey) continue;
        try {
          const info = (await this.adapter.invoke('get_cached_member_card', {
            subscriptionId,
          })) as DiscoveredCardInfo | null;
          if (info === null) continue;
          // Track the CURRENT avatarCid. A card that drops its avatar must clear
          // the tracked cid AND the avatarUrl so a stale blob URL doesn't linger.
          if (info.avatarCid) {
            this.cardAvatarCids.set(owner, info.avatarCid);
          } else {
            this.cardAvatarCids.delete(owner);
          }
          const avatarUrl = info.avatarCid ? this.resolveAvatarUrl(info.avatarCid) : undefined;
          const prev = this.cards.get(owner);
          if (
            !prev ||
            prev.displayName !== info.displayName ||
            prev.statusText !== info.statusText ||
            prev.avatarUrl !== avatarUrl ||
            prev.profilePageRoot !== info.profilePageRoot
          ) {
            this.cards.set(owner, {
              displayName: info.displayName,
              statusText: info.statusText,
              avatarUrl,
              profilePageRoot: info.profilePageRoot,
            });
            changed = true;
          }
        } catch (e) {
          console.warn(
            'get_cached_member_card failed',
            e instanceof Error ? e.message : String(e),
          );
        }
      }
      if (changed) this.onUpdate?.();
    } finally {
      this.polling = false;
    }
  }

  /** Cancel the poll loop and unsubscribe every active subscription (best-effort). */
  unsubscribeAll(): Promise<void> {
    return this.runExclusive(() => this.unsubscribeAllImpl());
  }

  private async unsubscribeAllImpl(): Promise<void> {
    this.stopPolling();
    this.buckets.clear();
    if (this.adapter) {
      for (const subscriptionId of this.subs.values()) {
        try {
          await this.adapter.invoke('unsubscribe_member_card', { subscriptionId });
        } catch (e) {
          console.warn(
            'unsubscribe_member_card failed',
            e instanceof Error ? e.message : String(e),
          );
        }
      }
    }
    this.subs.clear();
  }
}
