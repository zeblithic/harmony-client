<script lang="ts">
  /**
   * ZEB-291 Phase 2 Task 27 — Tier 2 (Conviction) community governance
   * panel. Lists every Tier 2 proposal in a community and gates a
   * new-proposal form behind `myPower >= 1` (any current member can
   * propose; the backend eligibility check further enforces per-config
   * minimums at create time).
   *
   * Event refetch: subscribes to `proposalCreated`,
   * `thresholdReached`, and `proposalFinalized` and reloads the full
   * list on each. The signal-cast event does NOT trigger a refetch
   * here because individual `ConvictionProposalCard` children handle
   * their own optimistic updates and the tick-driven recomputation will
   * flow through the next periodic refresh — we could subscribe and
   * refetch here too, but it would race the card's optimistic state
   * and visually flicker. (Tradeoff: a remote peer's signal won't
   * appear until the next refetch; acceptable for Phase 2 single-node-
   * correct behavior — Phase 3 polish.)
   *
   * Per ZEB-287 R4: destructure every $props() field referenced below.
   * Per Tauri error-extraction memory: `e instanceof Error ? e.message
   * : String(e)`.
   */

  import { onDestroy } from 'svelte';
  import type { CommunityMember } from '../types';
  import type { Tier2ProposalExport } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';
  import ConvictionProposalCard from './ConvictionProposalCard.svelte';
  import DelegationGraph from './DelegationGraph.svelte';
  import DelegationWidget from './DelegationWidget.svelte';

  let {
    communityId,
    adapter,
    myPower,
    myAddr,
    communityMembers,
  }: {
    /** Hex SpaceId of the community. */
    communityId: string;
    /** Voting IPC adapter — must be connected. */
    adapter: VotingAdapter;
    /** Caller's power level in this community. Form is gated on `>= 1`
     *  (current-member check). Backend separately enforces per-proposal
     *  eligibility (`min_power`) at create time. */
    myPower: number;
    /** ZEB-292 Phase 3: caller's 32-char hex OwnerAddr. Forwarded to
     *  DelegationWidget + ConvictionProposalCard's per-proposal override
     *  affordance. */
    myAddr: string;
    /** ZEB-292 Phase 3: community member roster, forwarded to the
     *  DelegationWidget picker + the override affordance's delegate-
     *  name resolution. */
    communityMembers: CommunityMember[];
  } = $props();

  let proposals = $state<Tier2ProposalExport[] | null>(null);
  let loadError = $state<string | null>(null);
  let loading = $state(false);
  /** Whether the delegation-graph collapsible is expanded. Bound to
   *  the <details> element below; the graph component is conditionally
   *  rendered so its d3-force simulation only runs after the user
   *  expands the section (Cursor R1: HTML <details> renders children
   *  regardless of open state — visibility-only via CSS — so the
   *  simulation would otherwise tick continuously while collapsed). */
  let graphOpen = $state(false);
  /** ZEB-292 Phase 3: caller's current delegate hex (32 chars) or
   *  null. Loaded on mount and refreshed on every
   *  voting-delegation-changed event for the local user. Passed to
   *  each ConvictionProposalCard so the per-proposal override
   *  affordance can render. */
  let myDelegate = $state<string | null>(null);
  let myDelegateName = $derived(
    myDelegate
      ? (communityMembers.find((m) => m.address === myDelegate)?.displayName ??
        `${myDelegate.slice(0, 8)}…`)
      : null,
  );
  /** Monotonic counter; bumped per proposals-list load. Resolution
   *  callbacks of superseded loads drop their results. Guards against
   *  a community switch landing while a previous `refetch` is still
   *  in flight (CR R1 outside-diff). */
  let latestProposalsLoadToken = 0;
  /** Separate token space for the delegate read so a proposals
   *  refetch firing while a delegate read is in flight doesn't
   *  silently drop the delegate result (Cursor R2 #2). */
  let latestDelegateLoadToken = 0;

  // New-proposal form state.
  let proposalText = $state('');
  let creating = $state(false);
  let createError = $state<string | null>(null);

  const PROPOSAL_TEXT_MAX = 2000;

  /** Anyone with at least power 1 (i.e. an actual member, not a
   *  guest/observer) can attempt to create a proposal. The Rust IPC
   *  enforces per-proposal eligibility on top. */
  let canPropose = $derived(myPower >= 1);

  async function refetch() {
    const token = ++latestProposalsLoadToken;
    loading = true;
    try {
      const list = await adapter.listTier2Proposals(communityId);
      if (token !== latestProposalsLoadToken) return;
      proposals = list;
      loadError = null;
    } catch (e) {
      if (token !== latestProposalsLoadToken) return;
      loadError = e instanceof Error ? e.message : String(e);
    } finally {
      if (token === latestProposalsLoadToken) loading = false;
    }
  }

  // Initial fetch + event-driven refetch. The $effect re-runs whenever
  // `communityId` changes so swapping the parent's selected community
  // re-loads from scratch (without this, a parent that re-uses this
  // component instance across community switches would show stale data).
  async function refetchDelegate() {
    const token = ++latestDelegateLoadToken;
    try {
      const next = await adapter.getMyDelegate(communityId);
      if (token !== latestDelegateLoadToken) return;
      myDelegate = next;
    } catch {
      if (token !== latestDelegateLoadToken) return;
      // Defensive: a failure to read the delegate just means the
      // override affordance won't render. The proposals list itself
      // is independent.
      myDelegate = null;
    }
  }

  $effect(() => {
    const cid = communityId;
    let cancelled = false;
    proposals = null;
    loadError = null;
    myDelegate = null;

    void (async () => {
      if (cancelled) return;
      await Promise.all([refetch(), refetchDelegate()]);
    })();

    // The three event subscribers. Each refetches on fire — cheap
    // (one IPC) and avoids needing per-proposal state diffing.
    const unsubCreated = adapter.subscribeProposalCreated((p) => {
      if (cancelled || p.communityId !== cid) return;
      void refetch();
    });
    const unsubThreshold = adapter.subscribeThresholdReached((p) => {
      if (cancelled || p.communityId !== cid) return;
      void refetch();
    });
    const unsubFinalized = adapter.subscribeProposalFinalized((p) => {
      if (cancelled || p.communityId !== cid) return;
      void refetch();
    });
    // ZEB-292 Phase 3: refresh `myDelegate` when the local user's
    // delegation changes (so the override pill appears/disappears in
    // sync with DelegationWidget).
    const unsubDelegation = adapter.subscribeDelegationChanged((p) => {
      if (cancelled || p.communityId !== cid) return;
      if (p.delegator !== myAddr) return;
      void refetchDelegate();
    });

    return () => {
      cancelled = true;
      unsubCreated();
      unsubThreshold();
      unsubFinalized();
      unsubDelegation();
    };
  });

  // Belt-and-suspenders: onDestroy is a no-op when the $effect
  // cleanup ran (the effect's cancelled flag will be set and
  // unsubscribes will have fired). Kept for paranoia in case the
  // effect was never wired (e.g. a parent destroyed the component
  // before the effect was scheduled — unlikely but cheap to guard).
  onDestroy(() => {});

  async function submitProposal() {
    const text = proposalText.trim();
    if (creating || !text || !canPropose) return;
    if (text.length > PROPOSAL_TEXT_MAX) {
      createError = `Proposal text too long (${text.length}/${PROPOSAL_TEXT_MAX})`;
      return;
    }
    creating = true;
    createError = null;
    try {
      // channelId is required by the Rust IPC for permission verification;
      // for the community-level governance area we pass communityId as
      // a placeholder. The Rust handler only validates this is well-
      // formed 16-byte hex — same shape as a channel id — and does not
      // currently enforce that it's an actual channel in the community.
      // TODO ZEB-291 follow-up: thread a real "governance channel" id
      // through if/when communities pin a canonical channel for
      // proposal-creation eligibility.
      await adapter.createTier2Proposal({
        communityId,
        channelId: communityId,
        proposalText: text,
      });
      proposalText = '';
      // The proposalCreated event will fire and refetch — no manual
      // refetch needed here.
    } catch (e) {
      createError = e instanceof Error ? e.message : String(e);
    } finally {
      creating = false;
    }
  }
</script>

<section class="community-proposals" aria-label="Community proposals">
  <DelegationWidget {communityId} {adapter} {myAddr} {communityMembers} />

  <details class="dg-section" bind:open={graphOpen}>
    <summary>Delegation graph</summary>
    {#if graphOpen}
      <!-- {#if} guard: <details> renders children regardless of `open`
           (CSS-only visibility), so without this the d3-force
           simulation would tick while collapsed. Conditional render
           defers mount until the user expands. -->
      <DelegationGraph {communityId} {adapter} {myAddr} {communityMembers} />
    {/if}
  </details>

  {#if canPropose}
    <form
      class="new-proposal-form"
      onsubmit={(e) => {
        e.preventDefault();
        void submitProposal();
      }}
    >
      <label class="np-label" for="np-text">New proposal</label>
      <textarea
        id="np-text"
        class="np-text"
        bind:value={proposalText}
        maxlength={PROPOSAL_TEXT_MAX}
        rows="3"
        placeholder="Describe the proposal…"
        disabled={creating}
      ></textarea>
      <div class="np-actions">
        <span class="np-charcount" aria-label="Character count">
          {proposalText.length}/{PROPOSAL_TEXT_MAX}
        </span>
        <button
          type="submit"
          class="np-submit"
          disabled={creating || proposalText.trim().length === 0}
        >
          {creating ? 'Creating…' : 'Submit'}
        </button>
      </div>
      {#if createError}
        <p class="np-error" role="alert">Couldn't create proposal: {createError}</p>
      {/if}
    </form>
  {/if}

  <div class="proposals-list" aria-label="Proposals">
    {#if loading && proposals === null}
      <p class="cp-loading">Loading proposals…</p>
    {:else if loadError}
      <p class="cp-error" role="alert">Couldn't load proposals: {loadError}</p>
    {:else if proposals && proposals.length === 0}
      <p class="cp-empty">No active proposals. Create one to start.</p>
    {:else if proposals}
      {#each proposals as proposal (proposal.proposal_id)}
        <ConvictionProposalCard
          {communityId}
          {proposal}
          {adapter}
          {myDelegate}
          delegateName={myDelegateName}
        />
      {/each}
    {/if}
  </div>
</section>

<style>
  .community-proposals {
    display: flex;
    flex-direction: column;
    gap: 16px;
    padding: 16px;
    overflow-y: auto;
    flex: 1;
    min-height: 0;
  }
  .new-proposal-form {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 12px 14px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-secondary);
    max-width: 520px;
  }
  .np-label {
    font-size: 0.78rem;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  .np-text {
    width: 100%;
    box-sizing: border-box;
    padding: 8px 10px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg-primary);
    color: var(--text-primary);
    font: inherit;
    resize: vertical;
  }
  .np-text:focus {
    outline: 1px solid var(--accent, #4ade80);
    outline-offset: 1px;
  }
  .np-actions {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }
  .np-charcount {
    font-size: 0.78rem;
    color: var(--text-secondary);
    font-variant-numeric: tabular-nums;
  }
  .np-submit {
    padding: 6px 16px;
    border: 1px solid var(--accent, #4ade80);
    border-radius: 4px;
    background: var(--accent, #4ade80);
    color: var(--bg-primary);
    font: inherit;
    cursor: pointer;
  }
  .np-submit:disabled {
    cursor: not-allowed;
    opacity: 0.5;
  }
  .np-error {
    color: var(--danger, #f87171);
    font-size: 0.85rem;
    margin: 4px 0 0;
  }
  .proposals-list {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .cp-loading,
  .cp-empty {
    color: var(--text-secondary);
    font-size: 0.9rem;
    margin: 0;
  }
  .cp-error {
    color: var(--danger, #f87171);
    font-size: 0.9rem;
    margin: 0;
  }
  .dg-section {
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-secondary);
  }
  .dg-section > summary {
    cursor: pointer;
    padding: 10px 14px;
    font-size: 0.92rem;
    color: var(--text-primary);
  }
  .dg-section[open] > summary {
    border-bottom: 1px solid var(--border);
  }
</style>
