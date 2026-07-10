<script lang="ts">
  /**
   * ZEB-290 Phase 1 — chat-native Tier-1 (Approval) poll card.
   *
   * Renders one poll inline in a chat feed: title-area + option list
   * with live approval counts and bar-graph fills. Clicking an option
   * casts/toggles that option's bit on the caller's ballot
   * (last-write-wins by HLC on the backend).
   *
   * Disabled when the poll's lifecycle is anything other than `Open`
   * (Closed / Finalized / Archived all read-only; Draft is impl-only
   * and never reaches the wire).
   *
   * Tally updates: subscribes to the adapter's `onBallotCast` and
   * re-fetches `getPoll(pollId)` when the matching pollId fires.
   * Phase 1 only sees self-casts (backend doesn't fan-out remote
   * events until Phase 1.5 ships the Zenoh subscriber), but the
   * refresh path is identical.
   *
   * Per ZEB-287 R4 critical-bug memory: $props() destructures EVERY
   * prop being used in the template / effects. Svelte 5 silently
   * no-ops missing destructures, so adding a prop here without
   * destructuring it would leak the literal `undefined` into the
   * markup.
   *
   * Per Tauri error-extraction memory: catches use
   * `e instanceof Error ? e.message : String(e)`.
   */

  import {
    pollIdEqual,
    pollIdToHex,
    type PollIdHex,
    type PollStateExport,
    type PollMeta,
  } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';
  import StatusPill from './governance/StatusPill.svelte';

  let {
    pollId,
    meta,
    adapter,
  }: {
    /** 32-byte poll id (arrives over Tauri JSON IPC as `number[]` —
     *  see PollIdHex JSDoc). */
    pollId: PollIdHex;
    /** Initial PollMeta (e.g. from listActivePolls cache). Used for
     *  first-paint title/tier/lifecycle before getPoll() resolves. */
    meta: PollMeta;
    /** Voting IPC adapter. Must be connected (connectAdapter) so the
     *  ballot-cast listener fires the refresh path. */
    adapter: VotingAdapter;
  } = $props();

  let state = $state<PollStateExport | null>(null);
  let loadError = $state<string | null>(null);
  let casting = $state(false);
  let castError = $state<string | null>(null);

  /** Voter's own current ballot (set of approved indices) for fast
   *  highlight + toggle. Derived from `state.your_ballot`. */
  let myApproved = $state<Set<number>>(new Set());

  /** True when the poll is currently accepting ballots.
   *  Once getPoll resolves we trust the fetched lifecycle as the source
   *  of truth (the initial `meta` snapshot can be stale — caught by
   *  CodeRabbit on PR #130). */
  let isOpen = $derived(
    state ? state.meta.lifecycle === 'Open' : meta.lifecycle === 'Open',
  );

  /** Total approvals across all options — divisor for percentage bars
   *  (using ballot_count would make bars sum to >100% in multi-approve
   *  polls; using max(counts) gives a cleaner "leading option = full
   *  bar" visual without misrepresenting absolute counts). */
  let maxCount = $derived(state ? Math.max(1, ...state.tally.counts) : 1);

  // Initial fetch + listener wiring. Resets per-poll UI state at the
  // top of the effect so a parent that swaps `pollId` on the same
  // component instance (or {#key pollId} reuse without remount) sees
  // a clean slate rather than the previous poll's bars / approval
  // highlight bleeding through. CodeRabbit #130 round-5 caught this.
  $effect(() => {
    let cancelled = false;
    const id = pollId;
    // The IPC boundary takes hex strings while the prop is the IPC-returned
    // byte array — convert once here. Passing the raw array made every
    // getPoll/castTier1Ballot reject at arg deserialization (Greptile #443).
    const idHex = pollIdToHex(id);
    state = null;
    loadError = null;
    castError = null;
    myApproved = new Set();

    void (async () => {
      try {
        const next = await adapter.getPoll(idHex);
        if (cancelled) return;
        state = next;
        loadError = null;
        myApproved = new Set(next.your_ballot ?? []);
      } catch (e) {
        if (cancelled) return;
        loadError = e instanceof Error ? e.message : String(e);
      }
    })();

    // Use the adapter's subscriber-list API so multiple poll cards can
    // each receive ballot-cast events without the chain-pattern leak
    // the monkey-patched onBallotCast had (Cursor #130 round-6 caught
    // dead handlers accumulating after non-LIFO unmount). The returned
    // unsubscribe closure splices our handler out of the list in O(n)
    // regardless of mount/unmount order.
    const unsubBallotCast = adapter.subscribeBallotCast((payload) => {
      // The EVENT payload carries the poll id as a hex string (the Rust
      // payload structs emit String) — compare against our hex form, not
      // the byte array (Greptile #443: the old pollIdEqual(payload, id)
      // compared string-vs-array and never matched, so the live-refresh
      // path was dead).
      if (cancelled || payload.pollId !== idHex) return;
      void (async () => {
        try {
          const next = await adapter.getPoll(idHex);
          if (cancelled) return;
          state = next;
          myApproved = new Set(next.your_ballot ?? []);
        } catch {
          // Stale poll (e.g. log evicted): keep last-known state.
        }
      })();
    });

    return () => {
      cancelled = true;
      unsubBallotCast();
    };
  });

  async function toggleOption(idx: number) {
    if (!isOpen || casting || !state) return;
    // Snapshot pollId at function-entry so a post-await prop swap
    // doesn't write this poll's optimistic ballot into another poll's
    // UI state. CodeRabbit #130 round-5 catch.
    const id = pollId;
    castError = null;
    const next = new Set(myApproved);
    if (next.has(idx)) {
      next.delete(idx);
    } else {
      next.add(idx);
    }
    // Mirror the backend's validate_ballot rules (EmptyBallot /
    // AbstentionBallot) on the client so a deselect-to-zero or
    // select-all interaction is a silent no-op rather than a
    // confusing "Vote failed" round-trip. Withdrawing a vote is a
    // future affordance (Phase 2 conviction-withdraw) and would
    // need a different verb than "toggle the last option off".
    if (next.size === 0) return;
    if (next.size === state.options.length) return;
    const indices = Array.from(next).sort((a, b) => a - b);
    casting = true;
    try {
      // Hex at the IPC boundary (see the fetch effect above).
      await adapter.castTier1Ballot(pollIdToHex(id), indices);
      // pollId is a byte-array (JSON IPC), so `!==` would reference-compare
      // and always be true. Value-equal via pollIdEqual.
      if (!pollIdEqual(pollId, id)) return;
      // Optimistic local update; the ballot-cast event will trigger a
      // server-confirmed refresh that supersedes this.
      myApproved = next;
    } catch (e) {
      if (!pollIdEqual(pollId, id)) return;
      castError = e instanceof Error ? e.message : String(e);
    } finally {
      if (pollIdEqual(pollId, id)) {
        casting = false;
      }
    }
  }

  function optionLabel(idx: number): string {
    if (state && idx < state.options.length) {
      return state.options[idx];
    }
    return `Option ${idx + 1}`;
  }

  function tierLabel(t: number): string {
    if (t === 1) return 'Approval';
    if (t === 2) return 'Conviction';
    if (t === 3) return 'Sortition';
    return `Tier ${t}`;
  }
</script>

<article class="poll-message" data-poll-id={pollId} aria-label="Poll">
  <header class="poll-header">
    <span class="poll-tier" aria-label="Tier">{tierLabel(meta.tier)}</span>
    <StatusPill
      variant={isOpen ? 'open' : 'archived'}
      label={state?.meta.lifecycle ?? meta.lifecycle}
      ariaLabel="Lifecycle"
    />
  </header>

  {#if loadError}
    <p class="poll-error" role="alert">Couldn't load poll: {loadError}</p>
  {:else if !state}
    <p class="poll-loading">Loading poll…</p>
  {:else}
    <ul class="poll-options">
      {#each state.tally.counts as count, idx (idx)}
        {@const pct = Math.round((count / maxCount) * 100)}
        {@const selected = myApproved.has(idx)}
        <li class="poll-option">
          <button
            type="button"
            class="poll-option-btn"
            class:selected
            disabled={!isOpen || casting}
            aria-pressed={selected}
            onclick={() => toggleOption(idx)}
          >
            <span class="poll-option-label">{optionLabel(idx)}</span>
            <span class="poll-option-count" aria-label="Approvals">{count}</span>
            <span class="poll-option-bar" aria-hidden="true">
              <span class="poll-option-bar-fill" style="width: {pct}%"></span>
            </span>
          </button>
        </li>
      {/each}
    </ul>

    <footer class="poll-footer">
      <span class="poll-ballot-count">{state.tally.ballot_count} ballots</span>
      {#if !isOpen}
        <span class="poll-closed-badge">Voting closed</span>
      {/if}
    </footer>

    {#if castError}
      <p class="poll-error" role="alert">Vote failed: {castError}</p>
    {/if}
  {/if}
</article>

<style>
  .poll-message {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 12px 14px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-secondary);
    max-width: 480px;
  }
  .poll-header {
    display: flex;
    gap: 8px;
    font-size: 0.78rem;
    color: var(--text-secondary);
  }
  .poll-tier {
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  .poll-options {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .poll-option-btn {
    width: 100%;
    display: grid;
    grid-template-columns: 1fr auto;
    grid-template-rows: auto auto;
    column-gap: 8px;
    row-gap: 4px;
    padding: 8px 10px;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--bg-primary);
    color: var(--text-primary);
    cursor: pointer;
    text-align: left;
    font: inherit;
  }
  .poll-option-btn:hover:not(:disabled) {
    border-color: var(--vote-for);
  }
  .poll-option-btn:disabled {
    cursor: not-allowed;
    opacity: 0.7;
  }
  .poll-option-btn.selected {
    border-color: var(--vote-for);
    background: color-mix(in srgb, var(--vote-for) 10%, var(--bg-primary));
  }
  .poll-option-label {
    grid-column: 1;
    grid-row: 1;
  }
  .poll-option-count {
    grid-column: 2;
    grid-row: 1;
    font-variant-numeric: tabular-nums;
    color: var(--text-secondary);
  }
  .poll-option-bar {
    grid-column: 1 / -1;
    grid-row: 2;
    height: 6px;
    background: var(--tally-track);
    border-radius: 3px;
    overflow: hidden;
  }
  .poll-option-bar-fill {
    display: block;
    height: 100%;
    background: var(--vote-for);
    transition: width 0.35s ease;
  }
  .poll-footer {
    display: flex;
    justify-content: space-between;
    font-size: 0.78rem;
    color: var(--text-secondary);
  }
  .poll-closed-badge {
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  .poll-error {
    color: var(--danger);
    font-size: 0.85rem;
    margin: 4px 0 0;
  }
  .poll-loading {
    color: var(--text-secondary);
    font-size: 0.85rem;
    margin: 0;
  }
</style>
