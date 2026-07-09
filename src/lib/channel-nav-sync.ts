import type { ChannelInfo } from './community-service';

/**
 * ZEB-663: the bridge from the channel data-pipe (CommunityService +
 * the `channel-config-updated` event) into NavService's tree. All side
 * effects are injected → deterministic + unit-testable (mirrors the
 * dep-injection pattern of mention-alert.ts / incoming-call-alert.ts).
 */
export interface ChannelNavSyncDeps {
  /** communityService.listChannels — cached; may reject pre-connect. */
  listChannels(communityId: string): Promise<ChannelInfo[]>;
  /** navService.setChannels — reconciles the community's channel children. */
  setChannels(communityId: string, channels: ChannelInfo[]): void;
  /** Current nav community node ids (navService.nodes, community-kind). */
  listCommunityIds(): string[];
}

export class ChannelNavSyncService {
  constructor(private deps: ChannelNavSyncDeps) {}

  /** Per-community issue counter for last-write-wins ordering. App triggers
   *  resync from boot start(), community switches, and channel-config-updated,
   *  and CommunityService.listChannels is cached but not single-flight — so
   *  overlapping resyncs for the same community can resolve out of order. Each
   *  resync captures its issue number and only applies its snapshot if no newer
   *  resync superseded it, so a slow stale fetch can't clobber a fresh one. */
  private seq = new Map<string, number>();

  /** Eager boot: populate channels for every joined community. Per-community
   *  failures are isolated (a stalled/erroring community renders childless and
   *  self-heals on its next resync); start() never rejects. */
  async start(): Promise<void> {
    await Promise.allSettled(
      this.deps.listCommunityIds().map((id) => this.resync(id)),
    );
  }

  /** Re-fetch a community's channels and reconcile them into the nav tree.
   *  The `channel-config-updated` event already invalidated CommunityService's
   *  cache, so listChannels re-fetches. Swallows failures (never throws into
   *  boot / event handlers). */
  async resync(communityId: string): Promise<void> {
    const issue = (this.seq.get(communityId) ?? 0) + 1;
    this.seq.set(communityId, issue);
    try {
      const channels = await this.deps.listChannels(communityId);
      // A newer resync superseded us while we awaited — its snapshot is fresher,
      // so drop ours rather than regress the nav tree with stale data.
      if (this.seq.get(communityId) !== issue) return;
      const live = channels.filter((c) => c.deletedAt === undefined);
      this.deps.setChannels(communityId, live);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      console.warn(`[channel-nav-sync] resync failed for ${communityId}:`, msg);
    }
  }
}
