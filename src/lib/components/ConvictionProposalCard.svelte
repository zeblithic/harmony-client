<script lang="ts">
  /**
   * ZEB-291 Phase 2 Task 26 — Tier 2 (Conviction) proposal card.
   *
   * Renders one Tier 2 proposal: lifecycle badge, proposal text, a
   * conviction bar (filled% relative to the dynamic threshold at fetch
   * time), and a signal toggle. Signal toggle optimistically flips
   * `your_signal` and rolls back on error.
   *
   * Per ZEB-287 R4 critical-bug discipline: every $props() field
   * referenced in the template/effects is destructured below — Svelte 5
   * silently no-ops un-destructured props, so a missed name leaks
   * `undefined` into the markup.
   *
   * Per Tauri error-extraction memory: catches use
   * `e instanceof Error ? e.message : String(e)`.
   *
   * Sibling to `PollMessage.svelte` (Tier 1); intentionally NOT sharing
   * a base component — the two cards have different lifecycle states
   * (Open/Closed vs Open/ThresholdReached/Finalized/Archived), different
   * action affordances (option-toggle list vs single signal toggle),
   * and different progress visuals (per-option bars vs a single
   * conviction-vs-threshold bar). A wrapping component would have to
   * thread both shapes through and pick which to render — the
   * branching factor exceeds the share value.
   */

  import { convictionPercent, type Tier2ProposalExport } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';

  let {
    communityId: _communityId,
    proposal,
    adapter,
    myDelegate = null,
    delegateName = null,
  }: {
    /** Hex community id this proposal lives in. Currently unused inside
     *  the card (the proposal already carries it) but accepted for
     *  symmetry with the panel that mounts these cards — keeps the
     *  surface stable when we later wire community-scoped affordances
     *  like delegate-to-this-voter. Prefixed `_` so the unused-prop
     *  lint stays quiet. */
    communityId: string;
    proposal: Tier2ProposalExport;
    /** Voting IPC adapter. Must be connected (connectAdapter) so the
     *  signal IPC has a route. */
    adapter: VotingAdapter;
    /** ZEB-292 Phase 3: caller's current delegate (32-char hex
     *  OwnerAddr) for this community, or null if voting directly. When
     *  set AND the caller has not signaled directly on this proposal
     *  (`proposal.your_signal === undefined`), a per-proposal "Vote
     *  directly" override affordance appears. */
    myDelegate?: string | null;
    /** ZEB-292 Phase 3: display name of the delegate (already resolved
     *  by the parent panel from the community member roster) — used
     *  in the override pill copy. Null when myDelegate is null. */
    delegateName?: string | null;
  } = $props();

  let signaling = $state(false);
  let signalError = $state<string | null>(null);
  /** Optimistic copy of `your_signal`. Mirrors `proposal.your_signal`
   *  on mount and after every refetch the parent does; the toggle path
   *  flips this immediately for instant UI feedback, then rolls back
   *  if the IPC errors.
   *
   *  Initialised to `undefined`; the $effect below seeds it from the
   *  current `proposal.your_signal` on first run (and re-syncs whenever
   *  the parent swaps in a fresh DTO). Initialising directly from
   *  `proposal.your_signal` would trigger Svelte's `state_referenced_locally`
   *  warning — direct prop reads in $state initialisers only capture
   *  the first-render value, masking later prop swaps. */
  let optimisticSignal = $state<boolean | undefined>(undefined);

  // Re-sync the optimistic flag when the parent swaps in a fresh
  // proposal snapshot (e.g. after a refetch triggered by an event).
  // Without this $effect, an optimistic update would persist forever
  // because the parent's refetched DTO sets a new `proposal.your_signal`
  // but the local optimisticSignal would never reset.
  $effect(() => {
    optimisticSignal = proposal.your_signal;
  });

  /** "Open" / "ThresholdReached" accept new signals. Finalized /
   *  Archived are terminal — toggle is hidden. */
  let canSignal = $derived(
    proposal.lifecycle === 'Open' || proposal.lifecycle === 'ThresholdReached',
  );

  /** ZEB-292 Phase 3 override affordance gate: show the "delegate votes
   *  for you — [Vote directly]" pill when the caller has a delegate
   *  AND has never signaled directly on this proposal yet. Once the
   *  caller has any direct signal state (true OR false), the regular
   *  signal toggle takes over — the backend has no "un-override"
   *  primitive that removes a per_voter entry. */
  let showOverridePill = $derived(
    canSignal && myDelegate !== null && optimisticSignal === undefined,
  );

  let pctFilled = $derived(
    convictionPercent(proposal.total_conviction_ms, proposal.threshold_conviction_ms),
  );

  /** Human label for the lifecycle badge. Spec §5: ThresholdReached
   *  enters a 24h contestability window before finalize — the badge
   *  copy reflects that. */
  let lifecycleLabel = $derived.by(() => {
    switch (proposal.lifecycle) {
      case 'Open':
        return 'Open';
      case 'ThresholdReached':
        return 'Threshold reached — 24h window';
      case 'Finalized':
        return 'Finalized';
      case 'Archived':
        return 'Archived';
      default:
        return proposal.lifecycle;
    }
  });

  async function toggleSignal() {
    if (signaling || !canSignal) return;
    // Compute the "intended next direction": if currently supporting,
    // withdraw; otherwise add support. (We don't toggle to false-when-
    // already-false — that's a no-op the backend would accept but the
    // UI would silently produce no observable state change for, which
    // misleads the user.)
    const currentlySupporting = optimisticSignal === true;
    const nextSupport = !currentlySupporting;

    const prevSignal = optimisticSignal;
    optimisticSignal = nextSupport; // optimistic flip
    signaling = true;
    signalError = null;
    try {
      await adapter.signalTier2(proposal.proposal_id, nextSupport);
      // Success: leave the optimistic state in place. The
      // signal-cast event will fire a refetch through the parent,
      // which will then reset optimisticSignal via the $effect above.
    } catch (e) {
      // Roll back optimistic state.
      optimisticSignal = prevSignal;
      signalError = e instanceof Error ? e.message : String(e);
    } finally {
      signaling = false;
    }
  }
</script>

<article
  class="conviction-proposal-card"
  data-proposal-id={proposal.proposal_id}
  aria-label="Conviction proposal"
>
  <header class="cp-header">
    <span
      class="cp-lifecycle"
      class:open={proposal.lifecycle === 'Open'}
      class:threshold={proposal.lifecycle === 'ThresholdReached'}
      class:finalized={proposal.lifecycle === 'Finalized'}
      aria-label="Lifecycle"
    >
      {lifecycleLabel}
    </span>
    <span class="cp-voter-count" aria-label="Supporters">
      {proposal.voter_count} / {proposal.total_supply} supporting
    </span>
  </header>

  <p class="cp-text">{proposal.proposal_text}</p>

  <div class="cp-bar-wrap" aria-label="Conviction progress">
    <div class="cp-bar-track">
      <div
        class="cp-bar-fill"
        class:past-threshold={pctFilled >= 100}
        style="width: {pctFilled}%"
      ></div>
      <!-- Threshold line marker — visually anchors the 100% point so
        the user reads bar fill as "% of threshold reached". For a
        Q96.32 bar capped at 100% the line sits at the right edge; if
        we later let the bar overflow to 110%/120% the line can stay
        at the inner 100% mark and the overflow visually pokes past. -->
      <div class="cp-bar-threshold" aria-hidden="true"></div>
    </div>
    <span class="cp-bar-pct" aria-label="Percent of threshold">
      {pctFilled.toFixed(1)}%
    </span>
  </div>

  {#if showOverridePill}
    <!-- ZEB-292 Phase 3: override affordance. Single click → cast a
         direct Signal(true) on this proposal, which moves the caller's
         weight out of the delegate's effective conviction (per spec §5
         override rule enforced by community_voting_conviction.rs:583).
         Copy describes the routing relationship (always true while a
         delegate edge exists) rather than asserting the delegate has
         signaled — the proposal DTO doesn't surface per-voter state,
         so claiming "X voted" would be unverifiable (Cursor R4). -->
    <div class="cp-override-pill" role="status" aria-label="Delegate signaling on your behalf">
      <span class="cp-override-text">
        Your conviction follows <strong>{delegateName ?? 'your delegate'}</strong> on this proposal.
      </span>
      <button
        type="button"
        class="cp-override-btn"
        disabled={signaling}
        onclick={toggleSignal}
      >
        Vote directly
      </button>
      {#if signalError}
        <span class="cp-error" role="alert">Override failed: {signalError}</span>
      {/if}
    </div>
  {:else if canSignal}
    <div class="cp-signal-row">
      <button
        type="button"
        class="cp-signal-btn"
        class:supporting={optimisticSignal === true}
        disabled={signaling}
        aria-pressed={optimisticSignal === true}
        onclick={toggleSignal}
      >
        {optimisticSignal === true ? 'Withdraw signal' : 'Signal support'}
      </button>
      {#if signalError}
        <span class="cp-error" role="alert">Signal failed: {signalError}</span>
      {/if}
    </div>
  {/if}
</article>

<style>
  .conviction-proposal-card {
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 14px 16px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-secondary);
    max-width: 520px;
  }
  .cp-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: 0.78rem;
    color: var(--text-secondary);
  }
  .cp-lifecycle {
    text-transform: uppercase;
    letter-spacing: 0.05em;
    font-weight: 600;
  }
  .cp-lifecycle.open { color: var(--accent); }
  .cp-lifecycle.threshold { color: #fbbf24; }
  .cp-lifecycle.finalized { color: var(--text-secondary); }
  .cp-text {
    margin: 0;
    color: var(--text-primary);
    font-size: 0.95rem;
    line-height: 1.4;
    white-space: pre-wrap;
  }
  .cp-bar-wrap {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .cp-bar-track {
    position: relative;
    flex: 1;
    height: 8px;
    background: var(--border);
    border-radius: 4px;
    overflow: hidden;
  }
  .cp-bar-fill {
    height: 100%;
    background: var(--accent);
    transition: width 250ms ease;
  }
  .cp-bar-fill.past-threshold {
    background: #fbbf24;
  }
  .cp-bar-threshold {
    position: absolute;
    top: 0;
    right: 0;
    width: 2px;
    height: 100%;
    background: var(--text-secondary);
    opacity: 0.5;
  }
  .cp-bar-pct {
    font-variant-numeric: tabular-nums;
    font-size: 0.8rem;
    color: var(--text-secondary);
    min-width: 48px;
    text-align: right;
  }
  .cp-signal-row {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
  }
  .cp-signal-btn {
    padding: 6px 14px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg-primary);
    color: var(--text-primary);
    font: inherit;
    cursor: pointer;
  }
  .cp-signal-btn:hover:not(:disabled) {
    border-color: var(--accent);
  }
  .cp-signal-btn:disabled {
    cursor: not-allowed;
    opacity: 0.6;
  }
  .cp-signal-btn.supporting {
    border-color: var(--accent);
    background: color-mix(in srgb, var(--accent) 12%, var(--bg-primary));
  }
  .cp-error {
    color: var(--danger);
    font-size: 0.85rem;
  }
  .cp-override-pill {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
    padding: 8px 12px;
    border: 1px solid #fbbf24;
    border-radius: 4px;
    background: color-mix(in srgb, #fbbf24 8%, var(--bg-primary));
  }
  .cp-override-text {
    flex: 1 1 auto;
    color: var(--text-primary);
    font-size: 0.85rem;
  }
  .cp-override-btn {
    padding: 4px 12px;
    border: 1px solid #fbbf24;
    background: #fbbf24;
    color: var(--bg-primary);
    border-radius: 4px;
    font: inherit;
    cursor: pointer;
  }
  .cp-override-btn:disabled {
    cursor: not-allowed;
    opacity: 0.6;
  }
</style>
