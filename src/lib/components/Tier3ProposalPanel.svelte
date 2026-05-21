<script lang="ts">
  /**
   * ZEB-311 — Tier 3 governance panel: create form + poll list +
   * stage-specific detail view.
   *
   * Sections:
   *   1. Create form (proposal text + sortition_size + 3 paired
   *      slider/number-input window controls). Submit goes through
   *      a click-confirm per `feedback_severe_action_confirmation`.
   *   2. List of existing Tier 3 polls (via adapter.listTier3Polls).
   *      Each row renders Tier3LifecycleStatus + click-to-expand.
   *   3. Expanded detail pane: dispatches on poll.stage + poll.myRole
   *      to mount SortitionRevealView / MiniPublicParticipationToggle /
   *      DraftingPanel / StarRatificationBallot.
   *
   * Refetches list/detail when ANY of the 5 Tier 3 Tauri events fire.
   *
   * Retry: a Failed poll where myRole = 'proposer' shows a "Retry"
   * button that pre-fills the create form with the failed poll's
   * fields. No retry_of linkage — fresh proposal per user direction.
   *
   * Per ZEB-287 R4: every $props field destructured below.
   * Per Tauri error-extraction memory: e instanceof Error ? e.message : String(e).
   */
  import { onDestroy, onMount } from 'svelte';
  import type {
    Tier3PollExport,
    Tier3PollSummary,
  } from '../types/voting';
  import { tier3StageLabel } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';
  import Tier3LifecycleStatus from './Tier3LifecycleStatus.svelte';
  import SortitionRevealView from './SortitionRevealView.svelte';
  import MiniPublicParticipationToggle from './MiniPublicParticipationToggle.svelte';
  import DraftingPanel from './DraftingPanel.svelte';
  import StarRatificationBallot from './StarRatificationBallot.svelte';

  let {
    communityId,
    adapter,
    myAddr,
  }: {
    communityId: string;
    adapter: VotingAdapter;
    myAddr: string;
  } = $props();

  // Create-form state
  let proposalText = $state('');
  let sortitionSize = $state(100);
  let deliberationWindowSeconds = $state(1_209_600); // 14d
  let draftingWindowSeconds = $state(604_800);       // 7d
  let ratificationWindowSeconds = $state(1_209_600); // 14d
  // The backend `validate_tier3_poll_config` accepts only the 1-char tags
  // 'a' | 'b' | 'c' | 'd' (see community_voting_tier3.rs). Labels are
  // descriptive UI strings; the wire value MUST be the single char.
  let incentiveMode = $state<'a' | 'b' | 'c' | 'd'>('d');
  let confirmingCreate = $state(false);
  let creating = $state(false);
  let createError = $state<string | null>(null);

  // List + selection state
  let summaries = $state<Tier3PollSummary[]>([]);
  let listError = $state<string | null>(null);
  let selectedPollId = $state<string | null>(null);
  let selectedDetail = $state<Tier3PollExport | null>(null);
  let detailError = $state<string | null>(null);
  let detailRequestSeq = $state(0);

  let unsubscribers: Array<() => void> = [];

  async function loadSummaries() {
    try {
      summaries = await adapter.listTier3Polls(communityId);
      listError = null;
    } catch (e) {
      listError = e instanceof Error ? e.message : String(e);
    }
  }

  async function loadDetail(pollId: string) {
    // Rapid clicks or event-driven refetches can race: an older
    // response can overwrite a fresher selection. Track a sequence
    // number and a pollId snapshot; commit only when both still match.
    const req = ++detailRequestSeq;
    try {
      const next = await adapter.getTier3Poll(pollId);
      if (req !== detailRequestSeq || selectedPollId !== pollId) return;
      selectedDetail = next;
      detailError = null;
    } catch (e) {
      if (req !== detailRequestSeq || selectedPollId !== pollId) return;
      detailError = e instanceof Error ? e.message : String(e);
    }
  }

  function select(pollId: string) {
    selectedPollId = pollId;
    // Drop the prior poll's detail immediately so the pane doesn't
    // render stale text/stage/child-panels while the new fetch
    // resolves. (Background event-driven refetches via refetchSelected
    // call loadDetail without going through select(), so they don't
    // null out the detail.)
    selectedDetail = null;
    detailError = null;
    loadDetail(pollId);
  }

  function refetchSelected() {
    if (selectedPollId) loadDetail(selectedPollId);
  }

  async function submitCreate() {
    if (creating) return;
    creating = true;
    try {
      await adapter.createTier3Proposal({
        communityId,
        channelId: communityId,
        proposalText,
        sortitionSize,
        deliberationWindowSeconds,
        draftingWindowSeconds,
        ratificationWindowSeconds,
        incentiveMode,
        // Eligibility floor for proposing. Default 0 so 0-power members
        // can author constitutional proposals — Tier 3 is the
        // egalitarian deliberation tier; gating proposal authorship on
        // power is a configuration choice the proposer makes
        // explicitly, not the platform's default.
        minPower: 0,
      });
      proposalText = '';
      confirmingCreate = false;
      createError = null;
      await loadSummaries();
    } catch (e) {
      createError = e instanceof Error ? e.message : String(e);
      confirmingCreate = false;
    } finally {
      creating = false;
    }
  }

  function retryFailed(failed: Tier3PollSummary) {
    proposalText = failed.proposalText;
    sortitionSize = failed.sortitionSize;
    // Keep current window/incentive — the proposer can tweak before resubmitting.
    confirmingCreate = false;
    // Scroll to top so the create form is visible.
    window.scrollTo({ top: 0, behavior: 'smooth' });
  }

  onMount(() => {
    loadSummaries();
    unsubscribers.push(adapter.subscribeTier3PollCreated(() => loadSummaries()));
    unsubscribers.push(
      adapter.subscribeTier3SortitionComplete(() => {
        loadSummaries();
        refetchSelected();
      }),
    );
    unsubscribers.push(
      adapter.subscribeTier3DraftingOpen(() => {
        loadSummaries();
        refetchSelected();
      }),
    );
    unsubscribers.push(
      adapter.subscribeTier3RatificationOpen(() => {
        loadSummaries();
        refetchSelected();
      }),
    );
    unsubscribers.push(
      adapter.subscribeTier3Finalized(() => {
        loadSummaries();
        refetchSelected();
      }),
    );
  });

  onDestroy(() => {
    for (const u of unsubscribers) u();
    unsubscribers = [];
  });
</script>

<section class="tier3-panel">
  <h2>Constitutional Decisions (Tier 3)</h2>

  <form
    class="create-form"
    onsubmit={(e) => {
      e.preventDefault();
      if (proposalText.trim()) confirmingCreate = true;
    }}
  >
    <label>
      <span>Proposal text</span>
      <textarea
        bind:value={proposalText}
        rows="3"
        maxlength="2000"
        placeholder="Amend charter §3: require 2/3 supermajority for moderator dismissals"
        required
      ></textarea>
    </label>

    <div class="paired-input">
      <label for="sortition-size">Sortition size</label>
      <input
        id="sortition-size"
        type="range"
        min="20"
        max="300"
        step="1"
        bind:value={sortitionSize}
      />
      <input type="number" min="20" max="300" bind:value={sortitionSize} />
    </div>

    <div class="paired-input">
      <label for="deliberation-window">Deliberation window (days)</label>
      <input
        id="deliberation-window"
        type="range"
        min="1"
        max="30"
        step="1"
        value={Math.round(deliberationWindowSeconds / 86_400)}
        oninput={(e) => {
          deliberationWindowSeconds =
            Number((e.target as HTMLInputElement).value) * 86_400;
        }}
      />
      <input
        type="number"
        min="1"
        max="30"
        value={Math.round(deliberationWindowSeconds / 86_400)}
        oninput={(e) => {
          deliberationWindowSeconds =
            Number((e.target as HTMLInputElement).value) * 86_400;
        }}
      />
    </div>

    <div class="paired-input">
      <label for="drafting-window">Drafting window (days)</label>
      <input
        id="drafting-window"
        type="range"
        min="1"
        max="30"
        step="1"
        value={Math.round(draftingWindowSeconds / 86_400)}
        oninput={(e) => {
          draftingWindowSeconds =
            Number((e.target as HTMLInputElement).value) * 86_400;
        }}
      />
      <input
        type="number"
        min="1"
        max="30"
        value={Math.round(draftingWindowSeconds / 86_400)}
        oninput={(e) => {
          draftingWindowSeconds =
            Number((e.target as HTMLInputElement).value) * 86_400;
        }}
      />
    </div>

    <div class="paired-input">
      <label for="ratification-window">Ratification window (days)</label>
      <input
        id="ratification-window"
        type="range"
        min="1"
        max="30"
        step="1"
        value={Math.round(ratificationWindowSeconds / 86_400)}
        oninput={(e) => {
          ratificationWindowSeconds =
            Number((e.target as HTMLInputElement).value) * 86_400;
        }}
      />
      <input
        type="number"
        min="1"
        max="30"
        value={Math.round(ratificationWindowSeconds / 86_400)}
        oninput={(e) => {
          ratificationWindowSeconds =
            Number((e.target as HTMLInputElement).value) * 86_400;
        }}
      />
    </div>

    <label>
      <span>Incentive mode</span>
      <select bind:value={incentiveMode}>
        <option value="a">a — SortitionEqual</option>
        <option value="b">b — ApprovalBonus</option>
        <option value="c">c — Community</option>
        <option value="d">d — DecisionPower (default)</option>
      </select>
    </label>

    <button type="submit" disabled={!proposalText.trim()}>Create proposal</button>
    {#if createError}
      <p class="error">{createError}</p>
    {/if}
  </form>

  {#if confirmingCreate}
    <div class="confirm-modal" role="dialog" aria-modal="true" aria-label="Confirm new Tier 3 proposal">
      <div class="confirm-card">
        <p>Confirm new Tier 3 proposal</p>
        <p class="confirm-summary">
          "{proposalText.slice(0, 120)}{proposalText.length > 120 ? '…' : ''}"
        </p>
        <div class="confirm-actions">
          <button type="button" onclick={() => (confirmingCreate = false)} disabled={creating}>Cancel</button>
          <button type="button" onclick={submitCreate} disabled={creating}>
            {creating ? 'Creating…' : 'Confirm'}
          </button>
        </div>
      </div>
    </div>
  {/if}

  <h3 class="list-heading">Existing proposals</h3>
  {#if listError}
    <p class="error">{listError}</p>
  {/if}
  {#if summaries.length === 0}
    <p class="empty">No constitutional decisions in this community yet.</p>
  {:else}
    <ul class="poll-list">
      {#each summaries as s (s.pollId)}
        <li class="poll-row">
          <button
            type="button"
            class="poll-row-button"
            onclick={() => select(s.pollId)}
            class:selected={selectedPollId === s.pollId}
          >
            <span class="proposal-text">{s.proposalText}</span>
            <Tier3LifecycleStatus summary={s} />
          </button>
          {#if s.stage === 'fa' && s.proposer === myAddr}
            <button type="button" class="retry-btn" onclick={() => retryFailed(s)}>
              Retry
            </button>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}

  {#if selectedPollId}
    <section class="detail-pane">
      {#if detailError}
        <p class="error">{detailError}</p>
      {:else if !selectedDetail}
        <p class="empty">Loading proposal details…</p>
      {:else}
        <h4>{selectedDetail.proposalText}</h4>
        <p class="stage-label">{tier3StageLabel(selectedDetail.stage)}</p>

        {#if selectedDetail.stage === 'so'}
          <p>Awaiting sortition draw. The D-FROST committee must produce the VRF beacon before the mini-public is selected.</p>
        {:else if selectedDetail.stage === 'de' || selectedDetail.stage === 'dr' || selectedDetail.stage === 'ra' || selectedDetail.stage === 'fi'}
          <SortitionRevealView detail={selectedDetail} {myAddr} />
          {#if selectedDetail.myRole === 'mini_public' && (selectedDetail.stage === 'de' || selectedDetail.stage === 'dr')}
            <MiniPublicParticipationToggle detail={selectedDetail} {adapter} {myAddr} onDecline={refetchSelected} />
          {/if}
          {#if selectedDetail.stage === 'dr'}
            <DraftingPanel detail={selectedDetail} {adapter} {myAddr} onChange={refetchSelected} />
          {/if}
          {#if selectedDetail.stage === 'ra'}
            <StarRatificationBallot detail={selectedDetail} {adapter} onCast={refetchSelected} />
          {:else if selectedDetail.stage === 'fi'}
            <!-- Finalized view: ratificationCandidates pivots from the
                 drafting-derived ordering to result.finalists, so the
                 caller's `myRatificationScores` (indexed against the OLD
                 ordering at cast time) cannot be safely re-paired here.
                 Show the read-only outcome instead of mounting the ballot. -->
            {@const winner = selectedDetail.ratificationCandidates.find(
              (c) => c.eventHash === selectedDetail.winnerEventHash,
            )}
            {@const runnerUp = selectedDetail.runnerUpEventHash
              ? selectedDetail.ratificationCandidates.find(
                  (c) => c.eventHash === selectedDetail.runnerUpEventHash,
                )
              : null}
            <section class="finalized-result">
              <h5>Outcome</h5>
              {#if winner}
                <p class="winner-line"><span class="badge winner">Winner</span> {winner.text}</p>
              {/if}
              {#if runnerUp}
                <p class="runner-up-line"><span class="badge runner-up">Runner-up</span> {runnerUp.text}</p>
              {/if}
              <details class="finalists">
                <summary>All finalists ({selectedDetail.ratificationCandidates.length})</summary>
                <ol>
                  {#each selectedDetail.ratificationCandidates as c (c.eventHash)}
                    <li>{c.text}</li>
                  {/each}
                </ol>
              </details>
            </section>
          {/if}
        {:else if selectedDetail.stage === 'fa'}
          <p class="failed-detail">
            Sortition failed — the backup pool was exhausted before the mini-public could be assembled.
          </p>
        {/if}
      {/if}
    </section>
  {/if}
</section>

<style>
  .tier3-panel { padding: 1rem; max-width: 880px; margin: 0 auto; }
  .create-form {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
    padding: 1rem;
    background: var(--panel-bg, #1a1c24);
    border-radius: 8px;
    margin-bottom: 1.5rem;
  }
  .paired-input {
    display: grid;
    grid-template-columns: 1fr 3fr 80px;
    gap: 0.5rem;
    align-items: center;
  }
  textarea, select, input[type="number"] {
    background: var(--input-bg, #0e0f15);
    color: inherit;
    border: 1px solid #2a2c34;
    border-radius: 4px;
    padding: 0.4rem 0.5rem;
    font: inherit;
  }
  button[type="submit"] {
    align-self: flex-start;
    background: var(--accent, #4a9eff);
    color: #fff;
    border: 0;
    padding: 0.5rem 1rem;
    border-radius: 4px;
    cursor: pointer;
  }
  button[type="submit"]:disabled { opacity: 0.5; cursor: not-allowed; }
  .confirm-modal {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: grid;
    place-items: center;
    z-index: 100;
  }
  .confirm-card {
    background: var(--panel-bg, #1a1c24);
    padding: 1.25rem 1.5rem;
    border-radius: 8px;
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
    max-width: 480px;
  }
  .confirm-actions { display: flex; gap: 0.5rem; justify-content: flex-end; }
  .confirm-actions button:last-child {
    background: var(--accent, #4a9eff);
    color: #fff;
  }
  .list-heading { margin-top: 1.5rem; font-size: 1rem; }
  .poll-list { list-style: none; padding: 0; }
  .poll-row { display: flex; gap: 0.5rem; align-items: center; padding: 0.5rem 0; border-bottom: 1px solid #2a2c34; }
  .poll-row-button {
    flex: 1;
    background: transparent;
    border: 0;
    color: inherit;
    text-align: left;
    cursor: pointer;
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 0.75rem;
    padding: 0.25rem 0.5rem;
    border-radius: 4px;
  }
  .poll-row-button.selected { background: rgba(74, 158, 255, 0.1); }
  .proposal-text { font-weight: 500; }
  .retry-btn {
    background: transparent;
    color: var(--accent, #4a9eff);
    border: 1px solid var(--accent, #4a9eff);
    padding: 0.25rem 0.6rem;
    border-radius: 4px;
    cursor: pointer;
  }
  .detail-pane {
    margin-top: 1.5rem;
    padding: 1rem;
    background: var(--panel-bg, #1a1c24);
    border-radius: 8px;
  }
  .stage-label { color: #8a8c95; font-size: 0.85rem; margin-top: -0.25rem; }
  .error { color: #d93838; }
  .empty { color: #8a8c95; }
  .failed-detail { color: #d93838; }
  .finalized-result { margin-top: 1rem; }
  .winner-line, .runner-up-line { margin: 0.4rem 0; }
  .badge {
    display: inline-block;
    font-size: 0.7rem;
    padding: 0.1rem 0.4rem;
    border-radius: 3px;
    margin-right: 0.4rem;
    font-weight: 500;
  }
  .badge.winner { background: var(--success-bg, rgba(74, 217, 122, 0.15)); color: var(--success, #4ad97a); }
  .badge.runner-up { background: rgba(74, 158, 255, 0.15); color: var(--accent, #4a9eff); }
  .finalists { margin-top: 0.5rem; color: #8a8c95; font-size: 0.85rem; }
  .finalists ol { margin: 0.25rem 0; padding-left: 1.25rem; }
</style>
