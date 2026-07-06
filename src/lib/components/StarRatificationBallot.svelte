<script lang="ts">
  /**
   * ZEB-311 — STAR ratification ballot. One 0-5 slider per candidate
   * (paired with a number input per accessibility memory). Cast goes
   * through click-confirm severity tier. Re-cast allowed.
   *
   * Per ZEB-287 R4: every $props field destructured below.
   * Per Tauri error-extraction memory.
   */
  import { untrack } from 'svelte';
  import type { Tier3PollExport } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';
  import GovConfirmModal from './governance/GovConfirmModal.svelte';

  let {
    detail,
    adapter,
    onCast,
  }: {
    detail: Tier3PollExport;
    adapter: VotingAdapter;
    onCast: () => void;
  } = $props();

  // Track which (poll, server-cast-state) we last seeded from. Reseed when:
  //   - the active poll changes (different pollId), OR
  //   - the same poll's myRatificationScores transitions null→array, OR
  //   - the same poll's myRatificationScores array contents change.
  // We MUST NOT reseed merely because `detail` changes — Tier3ProposalPanel
  // refetches the selected poll on each Tier 3 event, and reseeding from
  // a server snapshot that still has `myRatificationScores: null` (because
  // the user hasn't cast yet) would clobber in-progress slider edits.
  // Stringifying the scores array gives a stable identity for "same vs
  // different cast state" — typed arrays are short (≤5 candidates per
  // ZEB-309 cap), so the cost is negligible.
  function seedKey(d: Tier3PollExport): string {
    return `${d.pollId}|${d.myRatificationScores ? JSON.stringify(d.myRatificationScores) : 'null'}`;
  }
  function initialScores(d: Tier3PollExport): number[] {
    return d.myRatificationScores
      ? [...d.myRatificationScores]
      : new Array(d.ratificationCandidates.length).fill(0);
  }

  // Seed synchronously at component construction so the first render paints
  // the correct slider/number values. Initializing `scores = []` and then
  // populating from a $effect would let the first paint show browser-default
  // slider midpoints + empty number inputs before snapping on the effect's
  // first run. `untrack` documents that the snapshot is intentional — the
  // $effect below handles all subsequent updates to `detail`.
  let scores = $state<number[]>(untrack(() => initialScores(detail)));
  let confirming = $state(false);
  let casting = $state(false);
  let castError = $state<string | null>(null);
  let castSuccess = $state(false);
  let lastSeededKey: string | null = untrack(() => seedKey(detail));

  $effect(() => {
    const key = seedKey(detail);
    if (key === lastSeededKey) return;
    const isPollSwap = !lastSeededKey.startsWith(`${detail.pollId}|`);
    lastSeededKey = key;
    castSuccess = false;
    castError = null;
    if (detail.myRatificationScores) {
      scores = [...detail.myRatificationScores];
    } else if (isPollSwap) {
      // Fresh poll, no prior cast — start at zeros.
      scores = new Array(detail.ratificationCandidates.length).fill(0);
    }
    // Else: same poll, no server-side cast yet — keep in-progress edits.
  });

  function setScore(index: number, value: number) {
    const clamped = Math.max(0, Math.min(5, Math.round(value)));
    // Reassign the array (not in-place index write) so the paired
    // slider + number input both react. Svelte 5 $state arrays are
    // proxied and per-index writes generally do reactively fire, but
    // doing a full reassignment is the load-bearing pattern that
    // guarantees both `value={scores[i]}` bindings re-render across
    // any Svelte 5 minor version. n ≤ 5 (ZEB-309 candidate cap), so
    // the O(n) cost is negligible.
    scores = scores.map((s, i) => (i === index ? clamped : s));
  }

  async function confirmCast() {
    confirming = false;
    casting = true;
    castSuccess = false;
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

  {#if detail.privacyMode === 'se'}
    <p class="encryption-banner">
      🔒 Your ballot will be encrypted to the community committee. The tally
      is revealed only after the ratification window closes.
    </p>
  {/if}

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
      {#if casting}
        {detail.privacyMode === 'se' ? 'Encrypting…' : 'Casting…'}
      {:else}
        {detail.myRatificationScores ? 'Re-cast ballot' : 'Cast ballot'}
      {/if}
    </button>
  </div>
  {#if castError}<p class="error">{castError}</p>{/if}
</section>

{#if confirming}
  <GovConfirmModal
    title="Confirm ratification ballot"
    onConfirm={confirmCast}
    onCancel={() => (confirming = false)}
  >
    <ul class="ballot-summary">
      {#each detail.ratificationCandidates as c, i}
        <li><strong>{scores[i]}</strong> — {c.text}</li>
      {/each}
    </ul>
    <p class="caveat">You can re-cast later if the ratification window is still open.</p>
  </GovConfirmModal>
{/if}

<style>
  .ratification-ballot { margin-top: 1rem; }
  .instructions { color: var(--text-faint); font-size: 0.85rem; }
  /* ZEB-295 Phase 6 Task 11: ballot-secret affordance — lock-icon banner
     above the per-candidate sliders so the voter knows the ballot will
     be encrypted before submitting. */
  .encryption-banner {
    margin: 0.5rem 0 0.75rem;
    padding: 0.6rem 0.8rem;
    background: var(--sortition-bg);
    border-left: 3px solid var(--gov-purple);
    border-radius: 3px;
    color: var(--text-doc);
    font-size: 0.9rem;
    line-height: 1.4;
  }
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
    background: var(--input-bg);
    color: inherit;
    border: 1px solid var(--chip-bg);
    border-radius: 3px;
    padding: 0.2rem 0.3rem;
  }
  .ballot-footer { margin-top: 0.75rem; display: flex; justify-content: flex-end; gap: 0.75rem; align-items: center; }
  .ballot-footer button {
    background: var(--accent);
    color: var(--text-bright);
    border: 0;
    padding: 0.35rem 0.9rem;
    border-radius: 3px;
    cursor: pointer;
  }
  .ballot-footer button:disabled { opacity: 0.5; cursor: not-allowed; }
  .success { color: var(--vote-for); font-size: 0.85rem; }
  .error { color: var(--danger); }
  .ballot-summary { list-style: none; padding: 0; margin: 0; }
  .ballot-summary li { padding: 0.1rem 0; }
  .caveat { color: var(--text-faint); font-size: 0.8rem; }
</style>
