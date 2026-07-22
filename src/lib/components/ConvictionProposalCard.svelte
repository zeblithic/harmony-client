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
  import { showSignalCastToast } from '../voting-toast-wiring';
  import StatusPill from './governance/StatusPill.svelte';
  import TallyBar from './governance/TallyBar.svelte';
  import CountChip from './governance/CountChip.svelte';
  import IdPill from './governance/IdPill.svelte';
  import { tier2LifecyclePill, formatHalfLife } from './governance/proposal-format';

  let {
    communityId: _communityId,
    proposal,
    adapter,
    myDelegate = null,
    delegateName = null,
    hideText = false,
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
    /** ZEB-607: detail vote-column mounts the card next to the doc column, which already shows the text. */
    hideText?: boolean;
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

  /** ZEB-648: single {variant, label} shared with the panel breadcrumb so
   *  one proposal never shows two different lifecycle labels on one screen.
   *  (Spec §5: ThresholdReached's label spells out the 24h contestability
   *  window.) */
  let lifecyclePill = $derived(tier2LifecyclePill(proposal.lifecycle));
  let halfLifeText = $derived(formatHalfLife(proposal.half_life_seconds));

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
      showSignalCastToast(nextSupport); // ZEB-607 D6: signed-vote feedback
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
    <IdPill id={proposal.proposal_id} ariaLabel="Proposal id" />
    <StatusPill variant={lifecyclePill.variant} label={lifecyclePill.label} ariaLabel="Lifecycle" />
    <span class="cp-half-life" aria-label="Half-life">half-life {halfLifeText}</span>
  </header>

  {#if !hideText}
    <p class="cp-text">{proposal.proposal_text}</p>
  {/if}

  <div class="cp-bar-wrap" aria-label="Conviction progress">
    <TallyBar
      segments={[{ pct: pctFilled, token: pctFilled >= 100 ? '--gov-clay' : '--vote-for' }]}
      label={`Conviction ${pctFilled.toFixed(0)}% of threshold`}
    />
    <span class="cp-bar-pct" aria-label="Percent of threshold">
      {pctFilled.toFixed(0)}%
    </span>
  </div>

  <div class="cp-chips">
    <CountChip tone="sage" label="Threshold" value={`${pctFilled.toFixed(0)}% reached`} />
    <CountChip
      tone="clay"
      label="Supporters"
      value={`${proposal.voter_count} / ${proposal.total_supply}`}
    />
  </div>

  {#if showOverridePill}
    <!-- ZEB-292 Phase 3 override affordance, restyled as the Commons
         proxied footer (spec D5 + amendment 2: the action is "Vote
         directly" — the real per-proposal override verb; community-
         scoped Recall lives in DelegationWidget only). -->
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
        {optimisticSignal === true ? 'Withdraw support' : '▲ Support'}
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
    border-left: 3px solid var(--gov-clay);
    border-radius: 8px;
    background: var(--surface-raised);
    box-shadow: var(--shadow-e1);
    max-width: 520px;
  }
  .cp-header {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .cp-half-life {
    margin-left: auto;
    font-family: var(--font-mono);
    font-size: 0.72rem;
    color: var(--faint);
  }
  .cp-text {
    margin: 0;
    color: var(--text-primary);
    font-size: 0.95rem;
    line-height: 1.5;
    white-space: pre-wrap;
  }
  .cp-bar-wrap {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .cp-bar-wrap > :global(.tally-track) {
    flex: 1;
  }
  .cp-bar-pct {
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    font-size: 0.8rem;
    color: var(--text-muted);
    min-width: 48px;
    text-align: right;
  }
  .cp-chips {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }
  .cp-signal-row {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
  }
  .cp-signal-btn {
    padding: 8px 16px;
    border: 1px solid var(--vote-for);
    border-radius: 7px;
    background: var(--vote-for);
    color: var(--status-passed-fg);
    font: inherit;
    font-weight: 600;
    font-size: 0.85rem;
    cursor: pointer;
  }
  .cp-signal-btn:disabled {
    cursor: not-allowed;
    opacity: 0.6;
  }
  .cp-signal-btn.supporting {
    background: var(--surface-raised);
    color: var(--vote-for);
    border-color: var(--primary-border);
    font-weight: 600;
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
    border-top: 1px solid var(--line-soft);
    background: var(--paper);
    border-radius: 0 0 6px 6px;
    margin: 2px -6px -4px;
  }
  .cp-override-text {
    flex: 1 1 auto;
    color: var(--text-muted);
    font-size: 0.8rem;
  }
  .cp-override-text strong {
    color: var(--vote-for);
  }
  .cp-override-btn {
    padding: 4px 12px;
    border: 1px solid var(--primary-border);
    background: transparent;
    color: var(--vote-for);
    border-radius: 7px;
    font: inherit;
    font-size: 0.8rem;
    font-weight: 600;
    cursor: pointer;
  }
  .cp-override-btn:disabled {
    cursor: not-allowed;
    opacity: 0.6;
  }
</style>
