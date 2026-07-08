<script lang="ts">
  /**
   * ZEB-294 — bridging-statement surface. Renders BridgingScoreExport list
   * sorted DESC by bridging_score_q64 (already sorted by the IPC). Heat-bar
   * width = score / max_score * 100% (per-viewer-local f64; NEVER used for
   * sort).
   *
   * Empty state copy designed for the live state (not the empty state) per
   * feedback_design_for_eventual_state.
   */
  import type { BridgingScoreExport } from '../types/voting';
  import { shortId } from '../short-addr';
  import CountChip from './governance/CountChip.svelte';

  let {
    scores,
    error,
  }: {
    scores: BridgingScoreExport[];
    error: string | null;
  } = $props();

  let maxScore = $derived(
    scores.length === 0 ? 1 : Math.max(...scores.map((s) => Number(s.bridgingScoreQ64))),
  );

  function heatPct(s: BridgingScoreExport): number {
    if (maxScore === 0) return 0;
    return Math.round((Number(s.bridgingScoreQ64) / maxScore) * 100);
  }

  function diversityPct(s: BridgingScoreExport): number {
    const q32 = Number(s.diversityQ32);
    return Math.round((q32 / 2 ** 32) * 100);
  }

  const authorShort = shortId;
</script>

<aside class="bridging-panel">
  <h5>★ Bridging statements</h5>
  <p class="subtitle">Statements with broad support across people who otherwise disagree.</p>

  {#if error}
    <p class="error">Couldn't load bridging: {error}</p>
  {:else if scores.length === 0}
    <p class="empty">
      Bridging scores will appear once mini-public members vote on statements.
    </p>
  {:else}
    <ol>
      {#each scores as s (s.statementEventHash)}
        <li class="card">
          <div class="heat-bar" style:width={`${heatPct(s)}%`}></div>
          <div class="content">
            <p class="text">{s.statementText}</p>
            <div class="meta">
              <span class="author">by {authorShort(s.author)}</span>
              <CountChip label="Agree" value={String(s.agreeCount)} tone="sage" />
              <CountChip label="Diversity" value={`${diversityPct(s)}%`} tone="neutral" />
            </div>
          </div>
        </li>
      {/each}
    </ol>
  {/if}
</aside>

<style>
  /* ZEB-655: the panel is a self-titled aside in DeliberationView's unframed
     right column, so it owns the Commons card chrome; rows are recessed inset
     (var(--surface) below the raised panel) rather than nested cards — no
     shadow-stacking / card-in-card. */
  .bridging-panel {
    background: var(--surface-raised);
    border: 1px solid var(--border);
    box-shadow: var(--shadow-e1);
    border-radius: 8px;
    padding: 12px;
  }
  h5 { margin: 0 0 4px; font-family: var(--font-display); font-size: 0.95rem; }
  .subtitle { color: var(--text-faint); font-size: 0.8rem; margin: 0 0 8px; }
  ol { list-style: none; padding: 0; margin: 0; display: flex; flex-direction: column; gap: 8px; }
  .card { position: relative; padding: 10px; background: var(--surface); border-radius: 8px; overflow: hidden; }
  .heat-bar { position: absolute; left: 0; top: 0; bottom: 0; background: linear-gradient(to right, color-mix(in srgb, var(--gov-clay) 18%, transparent), color-mix(in srgb, var(--gov-clay) 0%, transparent)); z-index: 0; }
  .content { position: relative; z-index: 1; }
  .text { margin: 0; font-weight: 500; }
  .meta { margin-top: 6px; display: flex; gap: 8px; font-size: 0.75rem; color: var(--text-faint); align-items: center; }
  /* Truncated author address reads as machine data → mono. */
  .author { font-family: var(--font-mono); }
  .empty { color: var(--text-faint); font-style: italic; }
  .error { color: var(--danger); }
</style>
