<script lang="ts">
  /**
   * ZEB-311 — Tier 3 poll stage indicator.
   *
   * Renders a 4-chip progression Sortition → Deliberation → Drafting →
   * Ratification with the current stage highlighted, plus a countdown
   * to the next stage transition based on PollCreate.hlc + cumulative
   * window durations. SortitionFailed shows a single red badge
   * (proposer-initiated retry button is mounted by the parent panel,
   * not here — this component is presentation-only).
   *
   * Per ZEB-287 R4: every $props field is destructured below.
   */
  import { tier3StageLabel, type Tier3PollSummary } from '../types/voting';

  let { summary }: { summary: Tier3PollSummary } = $props();

  const stages = ['so', 'de', 'dr', 'ra'] as const;

  // Cumulative ms-since-PollCreate at the END of each stage (= START of next).
  // Stage 'so' ends when kd=ss applies — that's not deadline-driven, so we
  // show no countdown for Sortition. Subsequent stages all have wall-clock
  // deadlines via the kd=cl auto-mint at PollCreate.hlc + sum(windows so far).
  // (Phase 4a-main does not surface kd=ss arrival ETA — only stage chips.)
</script>

{#if summary.voided}
  <!-- ZEB-1031 §7/§9: a reset-voided poll takes priority over the
       ordinary stage/finalized/failed badges — it stays at whatever
       stage it was voided at, but that stage is no longer actionable. -->
  <div class="voided-badge">🚫 Voided</div>
{:else if summary.stage === 'fa'}
  <div class="failed-badge">⚠ Sortition failed (backup pool exhausted)</div>
{:else if summary.stage === 'fi'}
  <div class="finalized-badge">
    <span class="checkmark">✓</span>
    <span class="winner">{summary.winnerText ?? 'Finalized'}</span>
  </div>
{:else}
  <ol class="stage-chips" aria-label="Tier 3 poll stage progression">
    {#each stages as s}
      <li
        class="stage-chip"
        class:current={summary.stage === s}
        class:past={stages.indexOf(s) < stages.indexOf(summary.stage as typeof stages[number])}
      >
        {tier3StageLabel(s)}
      </li>
    {/each}
  </ol>
{/if}

<style>
  .stage-chips {
    display: flex;
    list-style: none;
    gap: 0.25rem;
    padding: 0;
    margin: 0;
    font-size: 11px;
  }
  .stage-chip {
    padding: 4px 11px;
    border-radius: 20px;
    font-weight: 600;
    background: var(--status-drafting-bg);
    color: var(--status-drafting-fg);
  }
  .stage-chip.past {
    opacity: 0.6;
  }
  .stage-chip.current {
    background: var(--status-open-bg);
    color: var(--status-open-fg);
  }
  .failed-badge {
    color: var(--vote-against);
    font-weight: 600;
  }
  .voided-badge {
    color: var(--vote-against);
    font-weight: 600;
  }
  .finalized-badge {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    color: var(--vote-for);
  }
</style>
