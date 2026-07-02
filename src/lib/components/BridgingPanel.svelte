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

  function authorShort(addr: string): string {
    return addr.length > 8 ? `${addr.slice(0, 8)}…` : addr;
  }
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
              <span>by {authorShort(s.author)}</span>
              <span class="chip agree">👍 {s.agreeCount}</span>
              <span class="chip diversity">diversity {diversityPct(s)}%</span>
            </div>
          </div>
        </li>
      {/each}
    </ol>
  {/if}
</aside>

<style>
  .bridging-panel { background: var(--panel-bg-deep); padding: 0.75rem; border-radius: 6px; }
  .subtitle { color: var(--text-faint); font-size: 0.8rem; margin: 0 0 0.5rem 0; }
  ol { list-style: none; padding: 0; margin: 0; display: flex; flex-direction: column; gap: 0.4rem; }
  .card { position: relative; padding: 0.5rem; background: var(--panel-bg); border-radius: 4px; overflow: hidden; }
  .heat-bar { position: absolute; left: 0; top: 0; bottom: 0; background: linear-gradient(to right, color-mix(in srgb, var(--success-gov) 18%, transparent), color-mix(in srgb, var(--success-gov) 0%, transparent)); z-index: 0; }
  .content { position: relative; z-index: 1; }
  .text { margin: 0; font-weight: 500; }
  .meta { margin-top: 0.3rem; display: flex; gap: 0.5rem; font-size: 0.75rem; color: var(--text-faint); align-items: center; }
  .chip { padding: 0.05rem 0.35rem; background: var(--chip-bg); border-radius: 2px; }
  .chip.agree { color: var(--success-gov); }
  .empty { color: var(--text-faint); font-style: italic; }
  .error { color: var(--danger-alt); }
</style>
