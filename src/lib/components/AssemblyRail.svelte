<script lang="ts">
  /**
   * ZEB-606: the Assembly rail — a compact, live list of ACTIVE Tier-2
   * proposals for one community, mounted in the messages-mode right rail.
   *
   * Lifecycle copies the proven CommunityProposalsPanel pattern: an $effect
   * keyed on communityId resets state, fetches, subscribes the Tier-2
   * lifecycle events (filtered by communityId), and cleans up with a
   * cancelled flag + unsubscribes. A monotonic load token drops superseded
   * fetch results (community-switch race).
   *
   * Signal-cast handling (PR #408 Greptile P1): the OWN user's casts never
   * refetch — ConvictionProposalCard handles its own optimistic state and a
   * refetch would race it and flicker (ZEB-291 tradeoff, same as the full
   * panel) — but REMOTE voters' casts on a listed proposal do refetch, so
   * supporter counts / conviction bars don't sit stale until the next
   * lifecycle event.
   *
   * Delegate context (PR #408 Greptile P1): the caller's delegate is loaded
   * and threaded into each card so a delegated voter sees the same
   * "conviction follows your delegate / Vote directly" override the full
   * Proposals view shows, instead of a plain Signal control that
   * misrepresents how their conviction is routed.
   */
  import type { CommunityMember } from '../types';
  import type { Tier2ProposalExport } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';
  import ConvictionProposalCard from './ConvictionProposalCard.svelte';

  let {
    communityId,
    adapter,
    myAddr = '',
    communityMembers = [],
    onViewAllProposals,
  }: {
    /** Hex SpaceId of the community whose assembly this shows. */
    communityId: string;
    /** Voting IPC adapter (connected or connecting — a pre-connect fetch
     *  rejects and surfaces as the error state; the next lifecycle event
     *  refetches). */
    adapter: VotingAdapter;
    /** Caller's 32-char hex OwnerAddr — delegate lookups + own-cast filter.
     *  Empty string disables the delegate affordance (cards fall back to the
     *  plain signal control). */
    myAddr?: string;
    /** Community roster, for resolving the delegate's display name. */
    communityMembers?: CommunityMember[];
    /** "View all proposals →" — App routes this to the Proposals view. */
    onViewAllProposals?: () => void;
  } = $props();

  let proposals = $state<Tier2ProposalExport[] | null>(null);
  let loadError = $state<string | null>(null);
  /** Caller's current delegate hex, or null when voting directly. */
  let myDelegate = $state<string | null>(null);
  let delegateName = $derived(
    myDelegate
      ? (communityMembers.find((m) => m.address === myDelegate)?.displayName ??
        `${myDelegate.slice(0, 8)}…`)
      : null,
  );
  /** Monotonic; superseded loads drop their results (community switch). */
  let latestLoadToken = 0;
  /** Separate token space so a proposals refetch can't drop a delegate read
   *  (mirrors CommunityProposalsPanel). */
  let latestDelegateLoadToken = 0;

  async function refetch(cid: string) {
    const token = ++latestLoadToken;
    try {
      const list = await adapter.listTier2Proposals(cid);
      if (token !== latestLoadToken) return;
      proposals = list;
      loadError = null;
    } catch (e) {
      if (token !== latestLoadToken) return;
      loadError = e instanceof Error ? e.message : String(e);
    }
  }

  async function refetchDelegate(cid: string) {
    const token = ++latestDelegateLoadToken;
    try {
      const next = await adapter.getMyDelegate(cid);
      if (token !== latestDelegateLoadToken) return;
      myDelegate = next;
    } catch {
      if (token !== latestDelegateLoadToken) return;
      // Failure just means the override affordance won't render; the
      // proposals list itself is independent (CommunityProposalsPanel idiom).
      myDelegate = null;
    }
  }

  $effect(() => {
    const cid = communityId;
    const addr = myAddr;
    let cancelled = false;
    proposals = null;
    loadError = null;
    myDelegate = null;
    void refetch(cid);
    if (addr !== '') void refetchDelegate(cid);
    const unsubs = [
      adapter.subscribeProposalCreated((p) => {
        if (!cancelled && p.communityId === cid) void refetch(cid);
      }),
      adapter.subscribeThresholdReached((p) => {
        if (!cancelled && p.communityId === cid) void refetch(cid);
      }),
      adapter.subscribeThresholdReverted((p) => {
        if (!cancelled && p.communityId === cid) void refetch(cid);
      }),
      adapter.subscribeProposalFinalized((p) => {
        if (!cancelled && p.communityId === cid) void refetch(cid);
      }),
      // Remote voters' casts refresh totals; own casts are skipped (the
      // card's optimistic state would race the refetch and flicker). The
      // payload carries no communityId, so membership in the current list
      // scopes it to this community.
      adapter.subscribeSignalCast((p) => {
        if (cancelled || p.voter === addr) return;
        if (!proposals?.some((pr) => pr.proposal_id === p.proposalId)) return;
        void refetch(cid);
      }),
      // Keep the delegate affordance in sync with DelegationWidget changes.
      adapter.subscribeDelegationChanged((p) => {
        if (cancelled || p.communityId !== cid || p.delegator !== addr) return;
        void refetchDelegate(cid);
      }),
    ];
    return () => {
      cancelled = true;
      for (const u of unsubs) u();
    };
  });

  /** Active proposals only: ThresholdReached first (closest to execution),
   *  then by total conviction descending (BigInt — Q96.32 decimal strings
   *  routinely exceed Number.MAX_SAFE_INTEGER). */
  let activeProposals = $derived.by(() => {
    if (proposals === null) return null;
    return proposals
      .filter((p) => p.lifecycle === 'Open' || p.lifecycle === 'ThresholdReached')
      .slice()
      .sort((a, b) => {
        if (a.lifecycle !== b.lifecycle) {
          return a.lifecycle === 'ThresholdReached' ? -1 : 1;
        }
        const d = BigInt(b.total_conviction_ms) - BigInt(a.total_conviction_ms);
        return d > 0n ? 1 : d < 0n ? -1 : 0;
      });
  });
</script>

<div class="assembly-rail" aria-label="Assembly">
  <h3 class="assembly-title">Assembly</h3>
  {#if loadError}
    <p class="assembly-error">{loadError}</p>
  {:else if activeProposals === null}
    <p class="assembly-empty">Loading proposals…</p>
  {:else if activeProposals.length === 0}
    <p class="assembly-empty">No open proposals</p>
  {:else}
    <div class="assembly-cards">
      {#each activeProposals as proposal (proposal.proposal_id)}
        <ConvictionProposalCard {communityId} {proposal} {adapter} {myDelegate} {delegateName} />
      {/each}
    </div>
  {/if}
  <button type="button" class="view-all" onclick={() => onViewAllProposals?.()}>
    View all proposals →
  </button>
</div>

<style>
  .assembly-rail {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .assembly-title {
    margin: 0;
    font-family: var(--font-display);
    font-size: 16px;
    font-weight: 600;
    color: var(--text-primary);
  }
  .assembly-cards {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .assembly-empty {
    margin: 0;
    color: var(--text-muted);
    font-size: 13px;
    text-align: center;
    padding: 24px 8px;
  }
  .assembly-error {
    margin: 0;
    color: var(--danger);
    font-size: 12px;
  }
  .view-all {
    border: none;
    background: none;
    color: var(--gov-clay);
    font-size: 13px;
    cursor: pointer;
    text-align: left;
    padding: 4px 0;
  }
  .view-all:hover {
    text-decoration: underline;
  }
</style>
