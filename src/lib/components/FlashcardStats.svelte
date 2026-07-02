<script lang="ts">
  import type { SessionStats } from '../flashcard-types';

  let { stats }: { stats: SessionStats } = $props();

  function formatTime(ms: number | null): string {
    if (ms === null) return '—';
    return `${(ms / 1000).toFixed(2)}s`;
  }

  let averageTimeMs = $derived(
    stats.cardsCompleted > 0 ? stats.totalTimeMs / stats.cardsCompleted : null
  );

  let effectiveBitrate = $derived(
    stats.totalTimeMs > 0
      ? `${(stats.totalCreditedBits / (stats.totalTimeMs / 1000)).toFixed(1)} bps`
      : '—'
  );

  const statRows = $derived([
    { label: 'Cards completed', value: String(stats.cardsCompleted) },
    { label: 'Perfect cards', value: String(stats.perfectCards) },
    { label: 'Express cards', value: String(stats.expressCards) },
    { label: 'Best time', value: formatTime(stats.bestTimeMs) },
    { label: 'Average time', value: formatTime(averageTimeMs) },
    { label: 'Previous time', value: formatTime(stats.previousTimeMs) },
    { label: 'Combo', value: String(stats.combo) },
    { label: 'Effective bitrate', value: effectiveBitrate },
  ]);
</script>

<div class="flashcard-stats">
  <h3 class="stats-title">Session Stats</h3>
  <dl class="stats-list">
    {#each statRows as row}
      <div class="stat-row">
        <dt class="stat-label">{row.label}</dt>
        <dd class="stat-value" data-testid="stat-value">{row.value}</dd>
      </div>
    {/each}
  </dl>
</div>

<style>
  .flashcard-stats {
    padding: 16px;
  }

  .stats-title {
    font-size: 0.875rem;
    font-weight: 600;
    color: var(--text-primary);
    margin: 0 0 12px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }

  .stats-list {
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .stat-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }

  .stat-label {
    font-size: 0.8125rem;
    color: var(--text-secondary);
  }

  .stat-value {
    font-size: 0.875rem;
    font-weight: 600;
    color: var(--text-primary);
    font-family: 'Courier New', Courier, monospace;
    margin: 0;
  }
</style>
