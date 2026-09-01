<script lang="ts">
  import { formatBytes } from '../file-utils';

  // ZEB-612 S3: no overall storage quota exists in the system, so the bar
  // shows real used bytes with no invented denominator. The meter below it
  // is the real (enforced) pinned-content budget from get_storage_budget;
  // it hides when the budget is unknown (demo mode / IPC failure).
  let {
    usedBytes,
    pinnedUsedBytes,
    pinnedBudgetBytes,
  }: {
    usedBytes: number;
    pinnedUsedBytes: number;
    pinnedBudgetBytes: number | null;
  } = $props();

  // A known zero budget with non-zero usage is fully over budget — it must
  // not render as a healthy 0% (distinct from null = budget unknown).
  let pinnedPercent = $derived(
    pinnedBudgetBytes == null
      ? 0
      : pinnedBudgetBytes > 0
        ? Math.min(100, Math.round((pinnedUsedBytes / pinnedBudgetBytes) * 100))
        : pinnedUsedBytes > 0
          ? 100
          : 0,
  );
  let warning = $derived(pinnedPercent >= 85);
</script>

<div
  class="quota-bar"
  aria-label="Storage: {formatBytes(usedBytes)} stored locally{pinnedBudgetBytes != null
    ? `, pinned ${formatBytes(pinnedUsedBytes)} of ${formatBytes(pinnedBudgetBytes)} budget`
    : ''}"
>
  <span class="quota-text used">{formatBytes(usedBytes)} stored locally</span>
  {#if pinnedBudgetBytes != null}
    <div class="quota-track">
      <div class="quota-fill" class:warning style:width="{pinnedPercent}%"></div>
    </div>
    <span class="quota-text">
      Pinned {formatBytes(pinnedUsedBytes)} of {formatBytes(pinnedBudgetBytes)}
    </span>
  {/if}
</div>

<style>
  .quota-bar {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 8px 12px;
    background: var(--bg-secondary);
    border-top: 1px solid var(--border);
    width: 100%;
    text-align: left;
    font: inherit;
    color: inherit;
  }

  .quota-track {
    height: 6px;
    background: var(--tally-track);
    border-radius: 999px;
    overflow: hidden;
  }

  .quota-fill {
    height: 100%;
    background: var(--accent);
    border-radius: 999px;
    transition: width 0.3s;
  }

  .quota-fill.warning {
    background: var(--gov-clay);
  }

  .quota-text {
    font-size: 0.75rem;
    color: var(--text-muted);
  }

  .quota-text.used {
    font-family: var(--font-mono);
  }
</style>
