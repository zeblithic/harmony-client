<script lang="ts">
  import type { ContributionSummaryDto } from '../storage-buddy-service';
  import { formatBytes } from '../file-utils';

  let {
    summary,
    onManage,
  }: {
    /** `null` until `get_contribution_summary` succeeds — the meter renders
     *  nothing rather than fabricated numbers (ZEB-610 §0). */
    summary: ContributionSummaryDto | null;
    onManage: () => void;
  } = $props();

  // Percent-fill arithmetic mirrors QuotaBar: a 0 budget with usage reads
  // as 100% (over), not divide-by-zero.
  let pct = $derived(
    summary == null
      ? 0
      : summary.budgetBytes > 0
        ? Math.min(100, Math.round((summary.hostedBytes / summary.budgetBytes) * 100))
        : summary.hostedBytes > 0
          ? 100
          : 0,
  );

  // Spec §3 three-state rule, rendered verbatim — no invented scoring.
  const HEALTH_COPY = {
    healthy: 'Healthy.',
    catchingUp: 'Catching up.',
    overBudget: 'Over budget.',
  } as const;
</script>

{#if summary}
  <section class="contribution-meter" aria-label="Storage buddies" data-testid="contribution-meter">
    <div class="meter-track">
      <div
        class="meter-fill"
        class:warning={summary.health !== 'healthy'}
        style:width="{pct}%"
      ></div>
    </div>
    <span class="meter-text">
      {formatBytes(summary.hostedBytes)} of {formatBytes(summary.budgetBytes)} shared
    </span>
    {#if summary.buddyCount > 0}
      <p class="meter-status">
        You host pieces for {summary.buddyCount} storage
        {summary.buddyCount === 1 ? 'buddy' : 'buddies'}.
        {HEALTH_COPY[summary.health]}
      </p>
    {:else}
      <p class="meter-status">No storage buddies yet.</p>
    {/if}
    <button type="button" class="manage-btn" data-testid="buddy-manage-btn" onclick={onManage}>
      {summary.buddyCount === 0 ? 'Invite a friend' : 'Manage'}
    </button>
  </section>
{/if}

<style>
  .contribution-meter {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 10px 12px;
    border-top: 1px solid var(--border);
  }

  .meter-track {
    height: 6px;
    background: var(--tally-track);
    border-radius: 999px;
    overflow: hidden;
  }

  .meter-fill {
    height: 100%;
    background: var(--accent);
    border-radius: 999px;
    transition: width 0.3s;
  }

  .meter-fill.warning {
    background: var(--gov-clay);
  }

  .meter-text {
    font-size: 0.75rem;
    color: var(--text-secondary);
    font-variant-numeric: tabular-nums;
  }

  .meter-status {
    margin: 0;
    font-size: 0.7rem;
    color: var(--text-muted);
  }

  .manage-btn {
    align-self: flex-start;
    margin-top: 2px;
    padding: 2px 10px;
    font-size: 0.75rem;
    color: var(--text-secondary);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 6px;
    cursor: pointer;
  }

  .manage-btn:hover {
    background: var(--bg-secondary);
    color: var(--text-primary);
  }
</style>
