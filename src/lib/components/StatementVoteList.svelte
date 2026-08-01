<script lang="ts">
  /**
   * ZEB-294 — statement vote list. Renders detail.deliberationStatements
   * chronologically ASC. Mini-public members see agree/disagree/pass
   * tri-button; observers see read-only count chips. "Unvoted by me"
   * filter defaults ON for mini-public.
   */
  import type {
    Tier3PollExport,
    DeliberationVoteCode,
    DeliberationStatementExport,
  } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';
  import { shortId } from '../short-addr';
  import { compareHlc } from '../hlc';
  import TallyBar from './governance/TallyBar.svelte';

  let {
    detail,
    adapter,
    myAddr,
    onChange,
  }: {
    detail: Tier3PollExport;
    adapter: VotingAdapter;
    myAddr: string;
    onChange: () => void;
  } = $props();

  // Default filter ON for mini-public, OFF for observers.
  let filterUnvoted = $state(detail.myRole === 'mini_public');

  let myVoteMap = $derived(
    new Map(detail.myDeliberationVotes.map((v) => [v.statementEventHash, v.vote])),
  );

  let isMiniPublic = $derived(detail.myRole === 'mini_public');
  let isWritable = $derived(isMiniPublic && detail.stage === 'de');

  let sortedStatements = $derived(
    [...detail.deliberationStatements].sort((a, b) =>
      compareHlc(
        { wallMs: a.createdAtHlcMs, logical: a.createdAtHlcLogical ?? 0, deviceId: a.createdAtHlcDeviceId ?? '' },
        { wallMs: b.createdAtHlcMs, logical: b.createdAtHlcLogical ?? 0, deviceId: b.createdAtHlcDeviceId ?? '' },
      ),
    ),
  );

  let visibleStatements = $derived(
    filterUnvoted
      ? sortedStatements.filter((s) => !myVoteMap.has(s.statementEventHash))
      : sortedStatements,
  );

  let castError = $state<string | null>(null);
  let castingHash = $state<string | null>(null);

  async function castVote(statementEventHash: string, vote: DeliberationVoteCode) {
    castingHash = statementEventHash;
    castError = null;
    try {
      await adapter.castDeliberationVote(detail.pollId, statementEventHash, vote);
      onChange();
    } catch (e) {
      castError = e instanceof Error ? e.message : String(e);
    } finally {
      castingHash = null;
    }
  }

  const authorShort = shortId;

  function myVote(s: DeliberationStatementExport): DeliberationVoteCode | undefined {
    return myVoteMap.get(s.statementEventHash);
  }

  /** True when `s` was authored by the current viewer; the apply layer
   * silently drops self-votes (spec §2.3), so we hide the tri-button to
   * avoid presenting controls that are silently no-ops on click. */
  function isOwnStatement(s: DeliberationStatementExport): boolean {
    return s.author === myAddr;
  }
</script>

<section class="vote-list">
  <header>
    <h5>Statements ({sortedStatements.length})</h5>
    {#if isMiniPublic}
      <label class="filter-toggle">
        <input type="checkbox" bind:checked={filterUnvoted} />
        Unvoted by me only
      </label>
    {/if}
  </header>

  {#if visibleStatements.length === 0}
    <p class="empty">
      {sortedStatements.length === 0
        ? 'No statements yet. Statements will appear here as mini-public members submit them.'
        : "You've voted on every statement currently visible. Toggle the filter off to revisit."}
    </p>
  {/if}

  <ol>
    {#each visibleStatements as s (s.statementEventHash)}
      <li class="row">
        <div class="text">{s.text}</div>
        <div class="meta">by {authorShort(s.author)}</div>
        {#if isWritable && !isOwnStatement(s)}
          <div class="tri-button">
            <button
              type="button"
              class:active={myVote(s) === 'agree'}
              disabled={castingHash === s.statementEventHash}
              onclick={() => castVote(s.statementEventHash, 'agree')}
            >👍 Agree</button>
            <button
              type="button"
              class:active={myVote(s) === 'disagree'}
              disabled={castingHash === s.statementEventHash}
              onclick={() => castVote(s.statementEventHash, 'disagree')}
            >👎 Disagree</button>
            <button
              type="button"
              class:active={myVote(s) === 'pass'}
              disabled={castingHash === s.statementEventHash}
              onclick={() => castVote(s.statementEventHash, 'pass')}
            >⊘ Pass</button>
          </div>
        {:else}
          {@const total = s.agreeCount + s.disagreeCount + s.passCount}
          {#if total > 0}
            <div class="chips-tally">
              <TallyBar
                height={5}
                label={`Statement votes: ${s.agreeCount} agree, ${s.disagreeCount} disagree, ${s.passCount} pass`}
                segments={[
                  { pct: (s.agreeCount / total) * 100, token: '--vote-for' },
                  { pct: (s.disagreeCount / total) * 100, token: '--vote-against' },
                  { pct: (s.passCount / total) * 100, token: '--vote-abstain' },
                ]}
              />
            </div>
          {/if}
          <div class="chips">
            <span class="chip agree">👍 {s.agreeCount}</span>
            <span class="chip disagree">👎 {s.disagreeCount}</span>
            <span class="chip pass">⊘ {s.passCount}</span>
            {#if isOwnStatement(s)}
              <span class="chip own" title="You authored this statement">yours</span>
            {/if}
          </div>
        {/if}
      </li>
    {/each}
  </ol>
  {#if castError}<p class="error">{castError}</p>{/if}
</section>

<style>
  .vote-list { background: var(--panel-bg-deep); padding: 0.75rem; border-radius: 6px; }
  header { display: flex; justify-content: space-between; align-items: baseline; margin-bottom: 0.5rem; }
  .filter-toggle { font-size: 0.85rem; color: var(--text-faint); cursor: pointer; }
  ol { list-style: none; padding: 0; margin: 0; display: flex; flex-direction: column; gap: 0.4rem; }
  .row { padding: 0.5rem; background: var(--panel-bg); border-radius: 4px; }
  .text { font-weight: 500; }
  .meta { font-size: 0.75rem; color: var(--text-faint); margin-top: 0.2rem; }
  .tri-button { margin-top: 0.4rem; display: flex; gap: 0.3rem; }
  .tri-button button {
    background: var(--chip-bg); color: var(--text-chip); border: 1px solid transparent;
    padding: 0.2rem 0.5rem; border-radius: 3px; font-size: 0.8rem; cursor: pointer;
  }
  .tri-button button.active { border-color: var(--vote-for); }
  .tri-button button:disabled { opacity: 0.5; cursor: not-allowed; }
  .chips-tally { margin-top: 0.4rem; }
  .chips { margin-top: 0.4rem; display: flex; gap: 0.4rem; font-size: 0.8rem; }
  .chip { padding: 0.1rem 0.4rem; background: var(--chip-bg); border-radius: 2px; color: var(--text-faint); }
  .chip.agree { color: var(--vote-for); }
  .chip.disagree { color: var(--vote-against); }
  .chip.own { color: var(--text-chip); background: var(--chip-bg-active); }
  .empty { color: var(--text-faint); font-style: italic; }
  .error { color: var(--danger); }
</style>
