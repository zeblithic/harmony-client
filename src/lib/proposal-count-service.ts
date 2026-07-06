import type { VotingAdapter } from './voting-adapter';

/**
 * ZEB-606: per-community count of ACTIVE Tier-2 conviction proposals
 * (`lifecycle ∈ {Open, ThresholdReached}`) for the nav proposals-row badge.
 *
 * Mirrors the PresenceService shape: a plain class the App owns, with a
 * `version` counter + `onChange` callback for Svelte invalidation (App bumps
 * a $state mirror in onChange; the nav resolver reads that mirror). Counts
 * load lazily via {@link ensure} — one IPC per community, communities are
 * few — and stay fresh via the four Tier-2 lifecycle events, each of which
 * refetches only the affected community (payloads carry `communityId`).
 *
 * NOT used by the Assembly rail: the rail holds full proposal lists itself;
 * the two stay consistent because both refetch on the same events.
 */
export class ProposalCountService {
  private counts = new Map<string, number>();
  /** communityId → monotonically increasing load token (stale-fetch guard). */
  private tokens = new Map<string, number>();
  private adapter: VotingAdapter | null = null;
  private unsubs: Array<() => void> = [];
  /** Bumped on every count change (presence-service version idiom). */
  version = 0;
  /** App-installed notifier — bumps a $state counter for reactivity. */
  onChange?: () => void;

  /** Wire the (possibly still-connecting) VotingAdapter. Idempotent. */
  connectAdapter(adapter: VotingAdapter): void {
    if (this.adapter) return;
    this.adapter = adapter;
    const refresh = (p: { communityId: string }) => {
      void this.refetch(p.communityId);
    };
    this.unsubs.push(
      adapter.subscribeProposalCreated(refresh),
      adapter.subscribeThresholdReached(refresh),
      adapter.subscribeThresholdReverted(refresh),
      adapter.subscribeProposalFinalized(refresh),
    );
  }

  /** Lazily fetch the count for `communityId`. No-op while a load is in
   *  flight or after one has succeeded (events keep it fresh from there). */
  ensure(communityId: string): void {
    if (!this.adapter) return;
    if (this.counts.has(communityId) || this.tokens.has(communityId)) return;
    void this.refetch(communityId);
  }

  /** Current active-proposal count, or undefined before the first
   *  successful fetch (callers render no badge for undefined). */
  countFor(communityId: string): number | undefined {
    return this.counts.get(communityId);
  }

  /** Tear down all event subscriptions (App unmount). */
  disconnect(): void {
    for (const u of this.unsubs) u();
    this.unsubs = [];
    this.adapter = null;
  }

  private async refetch(communityId: string): Promise<void> {
    if (!this.adapter) return;
    const token = (this.tokens.get(communityId) ?? 0) + 1;
    this.tokens.set(communityId, token);
    try {
      const list = await this.adapter.listTier2Proposals(communityId);
      if (this.tokens.get(communityId) !== token) return; // superseded
      const count = list.filter(
        (p) => p.lifecycle === 'Open' || p.lifecycle === 'ThresholdReached',
      ).length;
      if (this.counts.get(communityId) !== count) {
        this.counts.set(communityId, count);
        this.version += 1;
        this.onChange?.();
      }
    } catch (e) {
      if (this.tokens.get(communityId) !== token) return;
      // Badge is best-effort; log and leave the count unset/stale. If this
      // was the FIRST load (no count yet), clear the token so a later
      // ensure() may retry — otherwise a boot-window failure would pin the
      // badge to "unknown" forever.
      if (!this.counts.has(communityId)) this.tokens.delete(communityId);
      console.warn(
        `[zeb-606] proposal-count fetch failed for ${communityId}:`,
        e instanceof Error ? e.message : String(e),
      );
    }
  }
}
