import type { TauriAdapter } from './zenoh-service';

export interface ResolvedCard {
  displayName: string;
  statusText: string;
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
}

/** Poll cadence for the cache-drain loop. 3s balances name-fill latency
 *  against IPC chatter for a panel that may show dozens of members. */
const POLL_INTERVAL_MS = 3000;

/**
 * Resolves a community member's owner_id -> {displayName, statusText}.
 *
 * Self (seeded via {@link seedSelf}) is authoritative and never subscribed
 * to nor overwritten by the network poll. Peers are resolved by subscribing
 * to each visible owner_id's card broadcast ({@link subscribeVisible}) and
 * draining the server-side cache on a poll loop.
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
  /** ownerIdHex(lc) -> subscriptionId for each active peer subscription. */
  private subs = new Map<string, number>();
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
  }

  /** Seed (or overwrite) the self owner_id from the local profile. Synchronous. */
  seedSelf(ownerIdHex: string, profile: ResolvedCard): void {
    this.selfKey = ownerIdHex.toLowerCase();
    this.cards.set(this.selfKey, { ...profile });
    this.onUpdate?.();
  }

  /** Resolve an owner_id to its card, or undefined if unresolved. */
  resolve(ownerIdHex: string): ResolvedCard | undefined {
    return this.cards.get(ownerIdHex.toLowerCase());
  }

  /** Apply a card delivered via the `member-card-received` push event (instant
   *  path; the poll loop is the fallback). Idempotent with the poll. Never
   *  overwrites the locally-seeded self entry. */
  applyCard(ownerIdHex: string, card: ResolvedCard): void {
    const key = ownerIdHex.toLowerCase();
    if (key === this.selfKey) return; // self stays authoritative (seedSelf)
    const prev = this.cards.get(key);
    if (prev && prev.displayName === card.displayName && prev.statusText === card.statusText) {
      return; // unchanged — no churn
    }
    this.cards.set(key, { displayName: card.displayName, statusText: card.statusText });
    this.onUpdate?.();
  }

  /**
   * Reconcile the set of visible members against active subscriptions.
   * Subscribes to newly-visible owners, unsubscribes owners that left the
   * view, and excludes self (kept authoritative via seedSelf). Idempotent.
   */
  subscribeVisible(ownerIdHexes: string[]): Promise<void> {
    return this.runExclusive(() => this.subscribeVisibleImpl(ownerIdHexes));
  }

  private async subscribeVisibleImpl(ownerIdHexes: string[]): Promise<void> {
    if (!this.adapter) {
      console.warn('MemberCardService.subscribeVisible: no adapter; skipping');
      return;
    }
    const wanted = new Set(
      ownerIdHexes
        .map((h) => h.toLowerCase())
        .filter((h) => h !== this.selfKey),
    );

    // Unsubscribe owners no longer visible.
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

    // Subscribe newly-visible owners.
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
    // it when reconciliation has drained the set to empty (all visible members
    // departed or were filtered as self). Without the else-branch the 3s
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
          const prev = this.cards.get(owner);
          if (
            !prev ||
            prev.displayName !== info.displayName ||
            prev.statusText !== info.statusText
          ) {
            this.cards.set(owner, {
              displayName: info.displayName,
              statusText: info.statusText,
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
