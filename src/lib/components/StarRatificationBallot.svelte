<script lang="ts">
  /**
   * ZEB-311 — STAR ratification ballot. One 0-5 slider per candidate
   * (paired with a number input per accessibility memory). Cast goes
   * through click-confirm severity tier. Re-cast allowed.
   *
   * Per ZEB-287 R4: every $props field destructured below.
   * Per Tauri error-extraction memory.
   */
  import type { Tier3PollExport } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';

  let {
    detail,
    adapter,
    onCast,
  }: {
    detail: Tier3PollExport;
    adapter: VotingAdapter;
    onCast: () => void;
  } = $props();

  // Initial scores: seeded by the $effect below to avoid Svelte's
  // `state_referenced_locally` warning (direct prop reads in $state()
  // initialisers only capture the first-render value, masking later prop
  // swaps). The $effect runs on first render and whenever detail changes.
  let scores = $state<number[]>([]);
  let confirming = $state(false);
  let casting = $state(false);
  let castError = $state<string | null>(null);
  let castSuccess = $state(false);

  // Seed/sync scores from the server snapshot whenever detail changes
  // (including first render). Prefills from myRatificationScores when
  // present; otherwise initialises all-zero sized to the candidate count.
  $effect(() => {
    if (detail.myRatificationScores) {
      scores = [...detail.myRatificationScores];
    } else {
      scores = new Array(detail.ratificationCandidates.length).fill(0);
    }
  });

  function setScore(index: number, value: number) {
    const clamped = Math.max(0, Math.min(5, Math.round(value)));
    scores[index] = clamped;
  }

  async function confirmCast() {
    confirming = false;
    casting = true;
    castError = null;
    try {
      await adapter.castRatificationBallot(detail.pollId, scores);
      castSuccess = true;
      onCast();
    } catch (e) {
      castError = e instanceof Error ? e.message : String(e);
    } finally {
      casting = false;
    }
  }
</script>

<section class="ratification-ballot">
  <h5>STAR ratification ballot</h5>
  <p class="instructions">Score each candidate 0-5. Top two advance to runoff; the candidate scored higher on more ballots wins.</p>

  <ol class="candidate-list">
    {#each detail.ratificationCandidates as c, i (c.eventHash)}
      <li class="candidate">
        <div class="candidate-text">{c.text}</div>
        <div class="score-input">
          <input
            type="range"
            min="0"
            max="5"
            step="1"
            value={scores[i]}
            oninput={(e) => setScore(i, Number((e.target as HTMLInputElement).value))}
            aria-label={`Score for ${c.text}`}
          />
          <input
            type="number"
            min="0"
            max="5"
            value={scores[i]}
            oninput={(e) => setScore(i, Number((e.target as HTMLInputElement).value))}
            aria-label={`Score number for ${c.text}`}
          />
        </div>
      </li>
    {/each}
  </ol>

  <div class="ballot-footer">
    {#if castSuccess}
      <span class="success">✓ Ballot submitted</span>
    {/if}
    <button type="button" onclick={() => (confirming = true)} disabled={casting || detail.stage !== 'ra'}>
      {casting ? 'Casting…' : detail.myRatificationScores ? 'Re-cast ballot' : 'Cast ballot'}
    </button>
  </div>
  {#if castError}<p class="error">{castError}</p>{/if}
</section>

{#if confirming}
  <div class="confirm-modal" role="dialog" aria-modal="true" aria-label="Confirm ratification ballot">
    <p>Confirm ratification ballot</p>
    <ul class="ballot-summary">
      {#each detail.ratificationCandidates as c, i}
        <li><strong>{scores[i]}</strong> — {c.text}</li>
      {/each}
    </ul>
    <p class="caveat">You can re-cast later if the ratification window is still open.</p>
    <div class="confirm-actions">
      <button type="button" onclick={() => (confirming = false)}>Cancel</button>
      <button type="button" onclick={confirmCast}>Confirm</button>
    </div>
  </div>
{/if}

<style>
  .ratification-ballot { margin-top: 1rem; }
  .instructions { color: #8a8c95; font-size: 0.85rem; }
  .candidate-list { list-style: decimal; padding-left: 1.25rem; }
  .candidate {
    display: grid;
    grid-template-columns: 1fr auto;
    gap: 0.75rem;
    align-items: center;
    margin-bottom: 0.4rem;
  }
  .candidate-text { font-weight: 500; }
  .score-input { display: flex; gap: 0.4rem; align-items: center; }
  .score-input input[type="range"] { width: 140px; }
  .score-input input[type="number"] {
    width: 50px;
    background: var(--input-bg, #0e0f15);
    color: inherit;
    border: 1px solid #2a2c34;
    border-radius: 3px;
    padding: 0.2rem 0.3rem;
  }
  .ballot-footer { margin-top: 0.75rem; display: flex; justify-content: flex-end; gap: 0.75rem; align-items: center; }
  .ballot-footer button {
    background: var(--accent, #4a9eff);
    color: #fff;
    border: 0;
    padding: 0.35rem 0.9rem;
    border-radius: 3px;
    cursor: pointer;
  }
  .ballot-footer button:disabled { opacity: 0.5; cursor: not-allowed; }
  .success { color: var(--success, #4ad97a); font-size: 0.85rem; }
  .error { color: #d93838; }
  .confirm-modal {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: grid;
    place-items: center;
    z-index: 100;
  }
  .confirm-modal > * {
    background: var(--panel-bg, #1a1c24);
    padding: 1rem 1.5rem;
    border-radius: 8px;
    margin: 0.25rem;
  }
  .ballot-summary { list-style: none; padding: 0; }
  .ballot-summary li { padding: 0.1rem 0; }
  .caveat { color: #8a8c95; font-size: 0.8rem; }
  .confirm-actions { display: flex; gap: 0.5rem; }
  .confirm-actions button:last-child {
    background: var(--accent, #4a9eff);
    color: #fff;
  }
</style>
