<script lang="ts">
  /**
   * ZEB-294 — Tier 3b deliberation surface. Mounts inside Tier3ProposalPanel
   * when stage === 'de'. Two-column: composer + vote list on left, live
   * bridging panel on right. Refreshes bridging on every relevant Tauri event.
   *
   * Per PR #152 R9: seq+key in-flight guard via plain `let` (NOT `$state`).
   */
  import { untrack } from 'svelte';
  import StatementComposer from './StatementComposer.svelte';
  import StatementVoteList from './StatementVoteList.svelte';
  import BridgingPanel from './BridgingPanel.svelte';
  import type { Tier3PollExport, BridgingScoreExport } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';

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

  let bridgingScores = $state<BridgingScoreExport[]>([]);
  let bridgingError = $state<string | null>(null);

  // Plain `let` — increments inside $effect would trip effect_update_depth_exceeded if tracked.
  let bridgingRequestSeq = 0;

  // Bridging surface size: a 150-member mini-public with the 5-statement
  // spam cap admits up to 750 statements per poll. Top-10 was too narrow
  // for that scale (Greptile bot-pass 2 nit). Top-20 keeps the panel
  // scannable while surfacing a meaningfully broader slice; revisit if
  // larger mini-publics or longer deliberations become common.
  const BRIDGING_TOP_N = 20;

  async function loadBridging() {
    const seq = ++bridgingRequestSeq;
    const pollIdSnapshot = detail.pollId;
    try {
      const next = await adapter.listBridgingStatements(pollIdSnapshot, BRIDGING_TOP_N);
      if (seq !== bridgingRequestSeq || pollIdSnapshot !== detail.pollId) return;
      bridgingScores = next;
      bridgingError = null;
    } catch (e) {
      if (seq !== bridgingRequestSeq || pollIdSnapshot !== detail.pollId) return;
      bridgingError = e instanceof Error ? e.message : String(e);
    }
  }

  let unsubscribers: Array<() => void> = [];

  $effect(() => {
    void detail.pollId; // reactive dep
    for (const u of unsubscribers) u();
    unsubscribers = [];
    bridgingScores = [];
    bridgingError = null;
    untrack(() => loadBridging());
    // Capture the active pollId snapshot for guarding event handlers.
    // The subscription is re-armed in $effect whenever detail.pollId changes,
    // but events for unrelated polls would otherwise fire onChange/loadBridging
    // and churn the IPC layer.
    const activePollId = detail.pollId;
    unsubscribers.push(
      adapter.subscribeTier3DeliberationStatementCreated((p) => {
        if (p.pollId !== activePollId) return;
        onChange();
        void loadBridging();
      }),
    );
    unsubscribers.push(
      adapter.subscribeTier3DeliberationVoteCast((p) => {
        if (p.pollId !== activePollId) return;
        onChange();
        void loadBridging();
      }),
    );
    return () => {
      for (const u of unsubscribers) u();
      unsubscribers = [];
    };
  });
</script>

<section class="deliberation-view">
  <h4>Deliberation</h4>
  <div class="two-column">
    <div class="left-col">
      {#if detail.myRole === 'mini_public'}
        <StatementComposer {detail} {adapter} {onChange} />
      {/if}
      <StatementVoteList {detail} {adapter} {myAddr} {onChange} />
    </div>
    <div class="right-col">
      <BridgingPanel scores={bridgingScores} error={bridgingError} />
    </div>
  </div>
</section>

<style>
  .deliberation-view { margin: 1rem 0; }
  .two-column {
    display: grid;
    grid-template-columns: 1.4fr 1fr;
    gap: 1rem;
  }
  @media (max-width: 720px) {
    .two-column { grid-template-columns: 1fr; }
  }
  .left-col, .right-col { display: flex; flex-direction: column; gap: 0.75rem; }
</style>
