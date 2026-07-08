<script lang="ts">
  /**
   * ZEB-311 — Drafting stage: mini-public proposes & approves draft
   * candidates. Observers see a read-only list.
   *
   * Per ZEB-287 R4: every $props field destructured below.
   * Per Tauri error-extraction memory.
   */
  import type { Tier3PollExport } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';
  import { shortAddr as shortHex } from '../short-addr';

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

  let candidateText = $state('');
  let submitting = $state(false);
  let submitError = $state<string | null>(null);
  let approvingHash = $state<string | null>(null);
  let approveError = $state<string | null>(null);

  let canPropose = $derived(detail.myRole === 'mini_public');
  let approvalSet = $derived(new Set(detail.myDraftingApprovals));

  function shortAddr(hex: string | null): string {
    if (!hex) return 'system';
    return shortHex(hex);
  }

  async function submitCandidate() {
    if (!candidateText.trim() || submitting) return;
    submitting = true;
    submitError = null;
    try {
      await adapter.proposeDraftCandidate(detail.pollId, candidateText.trim());
      candidateText = '';
      onChange();
    } catch (e) {
      submitError = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }

  async function approveCandidate(eventHash: string) {
    if (approvingHash) return;
    approvingHash = eventHash;
    approveError = null;
    try {
      await adapter.approveDraftCandidate(detail.pollId, eventHash);
      onChange();
    } catch (e) {
      approveError = e instanceof Error ? e.message : String(e);
    } finally {
      approvingHash = null;
    }
  }
</script>

<section class="drafting-panel">
  <h5>Draft candidates ({detail.draftCandidates.length})</h5>

  {#if detail.draftCandidates.length === 0}
    <p class="empty">No candidates proposed yet.</p>
  {:else}
    <ul class="candidate-list">
      {#each detail.draftCandidates as c (c.eventHash)}
        <li class="candidate">
          <div class="candidate-body">
            <p class="text">{c.text}</p>
            <p class="proposer">— {shortAddr(c.proposer)}</p>
          </div>
          <div class="candidate-actions">
            <span class="approval-count">{c.approvalCount} approval{c.approvalCount === 1 ? '' : 's'}</span>
            {#if canPropose}
              {#if approvalSet.has(c.eventHash)}
                <button type="button" disabled>Approved</button>
              {:else}
                <button
                  type="button"
                  onclick={() => approveCandidate(c.eventHash)}
                  disabled={approvingHash !== null}
                >
                  {approvingHash === c.eventHash ? 'Approving…' : 'Approve'}
                </button>
              {/if}
            {/if}
          </div>
        </li>
      {/each}
    </ul>
    {#if approveError}<p class="error">{approveError}</p>{/if}
  {/if}

  {#if canPropose}
    <form
      class="propose-form"
      onsubmit={(e) => {
        e.preventDefault();
        submitCandidate();
      }}
    >
      <label>
        <span>Propose candidate</span>
        <textarea
          bind:value={candidateText}
          rows="2"
          maxlength="512"
          placeholder="Charter §3.1: Moderator dismissal requires 67% supermajority of voting members…"
        ></textarea>
      </label>
      <div class="form-footer">
        <span class="char-count">{candidateText.length}/512</span>
        <button type="submit" disabled={!candidateText.trim() || submitting}>
          {submitting ? 'Submitting…' : 'Submit candidate'}
        </button>
      </div>
      {#if submitError}<p class="error">{submitError}</p>{/if}
    </form>
  {/if}
</section>

<style>
  .drafting-panel { margin-top: 1rem; }
  h5 {
    font-size: 0.68rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-muted);
  }
  .candidate-list { list-style: none; padding: 0; }
  .candidate {
    display: flex;
    justify-content: space-between;
    gap: 0.75rem;
    padding: 0.5rem 0.75rem;
    background: var(--input-bg);
    border-radius: 4px;
    margin-bottom: 0.4rem;
  }
  .candidate-body { flex: 1; }
  .candidate-body .text { margin: 0; }
  .candidate-body .proposer { margin: 0.15rem 0 0; color: var(--text-faint); font-size: 0.8rem; }
  .candidate-actions { display: flex; flex-direction: column; align-items: flex-end; gap: 0.25rem; }
  .approval-count { color: var(--text-faint); font-size: 0.8rem; }
  .candidate-actions button {
    background: var(--accent);
    color: var(--on-accent);
    border: 0;
    padding: 0.25rem 0.6rem;
    border-radius: 3px;
    cursor: pointer;
  }
  .candidate-actions button:disabled { opacity: 0.6; cursor: not-allowed; }
  .propose-form { margin-top: 0.75rem; display: flex; flex-direction: column; gap: 0.4rem; }
  .propose-form textarea {
    background: var(--input-bg);
    color: inherit;
    border: 1px solid var(--chip-bg);
    border-radius: 4px;
    padding: 0.4rem 0.5rem;
    width: 100%;
  }
  .form-footer { display: flex; justify-content: space-between; align-items: center; }
  .char-count { color: var(--text-faint); font-size: 0.75rem; }
  .form-footer button {
    background: var(--accent);
    color: var(--on-accent);
    border: 0;
    padding: 0.3rem 0.8rem;
    border-radius: 3px;
    cursor: pointer;
  }
  .form-footer button:disabled { opacity: 0.5; cursor: not-allowed; }
  .empty { color: var(--text-faint); }
  .error { color: var(--danger); }
</style>
