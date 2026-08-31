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
  import { onDestroy } from 'svelte';
  import type {
    Tier3PollExport,
    Tier3PollSummary,
  } from '../types/voting';
  import { tier3StageLabel } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';
  import { POWER_THRESHOLDS } from '../types';
  import Tier3LifecycleStatus from './Tier3LifecycleStatus.svelte';
  import SortitionRevealView from './SortitionRevealView.svelte';
  import MiniPublicParticipationToggle from './MiniPublicParticipationToggle.svelte';
  import DraftingPanel from './DraftingPanel.svelte';
  import StarRatificationBallot from './StarRatificationBallot.svelte';
  import DeliberationView from './DeliberationView.svelte';
  import GovConfirmModal from './governance/GovConfirmModal.svelte';

  let {
    communityId,
    adapter,
    myAddr,
    myPower = 0,
  }: {
    communityId: string;
    adapter: VotingAdapter;
    myAddr: string;
    /** ZEB-1031 §7/§9: gates the Relaunch button on a voided poll
     *  (creator-or-admin per spec) — defaults to 0 so callers that
     *  don't yet thread admin power through still render, just without
     *  the admin branch of that gate. */
    myPower?: number;
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
  // ZEB-295 Phase 6 Task 11: privacy mode for ratification ballots.
  //   'pu' (default) = public — per-voter ballots are visible.
  //   'se' = ballot-secret — ballots encrypted, tally revealed only after
  //          D-FROST committee threshold decryption.
  // 'rf' is reserved on the wire but never user-selectable here.
  let privacyMode = $state<'pu' | 'se'>('pu');
  let confirmingCreate = $state(false);
  let creating = $state(false);
  let createError = $state<string | null>(null);

  // List + selection state
  let summaries = $state<Tier3PollSummary[]>([]);
  let listError = $state<string | null>(null);
  let selectedPollId = $state<string | null>(null);
  let selectedDetail = $state<Tier3PollExport | null>(null);
  let detailError = $state<string | null>(null);
  // Internal sequence numbers for in-flight-response race protection.
  // NOT $state — these are written inside $effect bodies (the
  // community-reactivity effect and the polling effect), and a $state
  // here would make ++seq read+write the same tracked dep, triggering
  // an effect_update_depth_exceeded loop. They aren't rendered in
  // templates so reactivity isn't required.
  let detailRequestSeq = 0;
  let summariesRequestSeq = 0;

  // ZEB-1018: transient D-FROST committee-ceremony status line. Set by
  // dkg/refresh progress events (which now arrive from PEERS too — the
  // transport adapter feeds the same Tauri events the local IPC layer
  // emits), cleared when the SAME ceremony's beacon lands or on
  // community switch. The ceremonyId is tracked so a delayed beacon
  // from an older ceremony can't blank the status of the one currently
  // in flight (CodeAnt PR #768).
  let ceremonyStatus = $state<{ ceremonyId: string; text: string } | null>(null);

  let unsubscribers: Array<() => void> = [];

  async function loadSummaries() {
    // Race protection mirroring loadDetail: a community switch or 5s
    // polling fire can leave two listTier3Polls calls in-flight; an
    // older response finishing last would overwrite `summaries` with
    // the previous community's polls. `req` guards against same-cid
    // overwrites; `cid !== communityId` guards against the user
    // switching communities between request dispatch and response
    // arrival.
    const req = ++summariesRequestSeq;
    const cid = communityId;
    try {
      const next = await adapter.listTier3Polls(cid);
      if (req !== summariesRequestSeq || cid !== communityId) return;
      summaries = next;
      listError = null;
    } catch (e) {
      if (req !== summariesRequestSeq || cid !== communityId) return;
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
    // CodeAnt nitpick 3 (review round 1): a failed relaunch's error
    // must not linger into a different poll's voided banner.
    relaunchError = null;
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
        // ZEB-295 Phase 6 Task 11: privacy_mode passthrough. Omitted (pu)
        // lets the Rust IPC substitute the default.
        privacyMode,
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

  // ZEB-295 Phase 6 Task 11: derive whether to render the awaiting-tally
  // committee-progress banner instead of the active ratification ballot.
  //
  // CodeRabbit PR #155: we previously gated on `Date.now() >= endMs` using
  // a client-clock-derived window end. A device with a fast clock would
  // hide the ballot UI before the backend considers ratification closed.
  // Switch to a backend-derived signal: once any committee member has
  // published a kd=ts share, the engine considers ratification closed from
  // its perspective (shares are only emitted post-window). If no shares
  // have arrived yet, keep the ballot UI up — worst case the backend
  // rejects a too-late cast with a clear error, which is strictly better
  // than hiding controls early on clock skew. Re-render the banner once
  // a winner has been finalized is also out (caller's render branch
  // guards on `!d.winnerEventHash`).
  function shouldShowAwaitingTally(d: Tier3PollExport): boolean {
    return d.privacyMode === 'se'
        && !d.winnerEventHash
        && d.encryptedTallyShareCount > 0;
  }

  // ZEB-1031 §7/§9: relaunch a poll voided by a D-FROST committee reset.
  // Authors a fresh PollCreate copying the voided poll's parameters at the
  // current epoch and navigates the detail pane to it on success — the old
  // poll's ballots are cryptographically unrecoverable (ElGamal-encrypted
  // to the retired committee), so this is a fresh poll, not a resume.
  let relaunching = $state(false);
  let relaunchError = $state<string | null>(null);
  let relaunchRequestSeq = 0;

  async function relaunchVoided(pollId: string) {
    // CR review round 1: guard against a community switch (or a second
    // relaunch click) racing this call, mirroring loadSummaries'/
    // loadDetail's `req`/`cid` pattern — an older relaunch's success
    // must not `select()` a new poll into the WRONG (now-current)
    // community's panel, and its error must not land in the wrong
    // community either.
    if (relaunching) return;
    const request = ++relaunchRequestSeq;
    const cid = communityId;
    relaunching = true;
    relaunchError = null;
    try {
      const newPollId = await adapter.relaunchVoidedPoll(pollId);
      if (request !== relaunchRequestSeq || cid !== communityId) return;
      await loadSummaries();
      if (request !== relaunchRequestSeq || cid !== communityId) return;
      select(newPollId);
    } catch (e) {
      if (request === relaunchRequestSeq && cid === communityId) {
        relaunchError = e instanceof Error ? e.message : String(e);
      }
    } finally {
      if (request === relaunchRequestSeq) relaunching = false;
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

  // Reload list + tear-down/re-arm subscriptions when communityId changes.
  // Using $effect (not onMount) so the panel reacts to parent re-binding
  // the prop — e.g. user switching between communities in the same view.
  // Subscriber handlers themselves are not community-scoped (they fire on
  // any tier3 event); they call loadSummaries() which always reads the
  // current `communityId` via the IPC. Resetting selection on switch
  // avoids rendering the previous community's detail while the new
  // community's list is in-flight.
  $effect(() => {
    // Track communityId as a reactive dep. Reads must be synchronous;
    // the dep wouldn't register if we only read it inside the async
    // loadSummaries() body.
    void communityId;

    for (const u of unsubscribers) u();
    unsubscribers = [];

    summaries = [];
    selectedPollId = null;
    selectedDetail = null;
    detailError = null;
    listError = null;
    ceremonyStatus = null;
    relaunchError = null;

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
    // ZEB-295 Phase 6 Task 11: refetch the selected poll when a kd=ts
    // share is applied so the awaiting-tally banner's k-of-n counter
    // updates incrementally. The list view doesn't depend on share
    // counts, so we only refetch the detail — not the summary list.
    //
    // CodeRabbit PR #155: the event fires for every accepted kd=ts across
    // every community + poll the engine sees. Filter to (this community,
    // currently-selected poll) so a tally share in a different community
    // doesn't trigger a needless detail refetch on this panel.
    unsubscribers.push(
      adapter.subscribeTier3TallyShareApplied((p) => {
        if (p.communityId !== communityId) return;
        if (selectedPollId && p.pollId === selectedPollId) refetchSelected();
      }),
    );
    // ZEB-319: refetch on mid-stage mutations (replaces the 5s
    // polling fallback). Filter by (communityId, pollId) to avoid
    // needless refetches in multi-community / multi-poll panels.
    unsubscribers.push(
      adapter.subscribeTier3MiniPublicDecline((p) => {
        if (p.communityId !== communityId) return;
        if (selectedPollId && p.pollId === selectedPollId) refetchSelected();
        // Summary DTO does not include decline / roster fields
        // (see voting_list_tier3_polls_raw in lib.rs), so no summaries
        // reload is needed — declines only affect detail.
      }),
    );
    unsubscribers.push(
      adapter.subscribeTier3DraftCandidate((p) => {
        if (p.communityId !== communityId) return;
        if (selectedPollId && p.pollId === selectedPollId) refetchSelected();
      }),
    );
    unsubscribers.push(
      adapter.subscribeTier3DraftApproval((p) => {
        if (p.communityId !== communityId) return;
        if (selectedPollId && p.pollId === selectedPollId) refetchSelected();
      }),
    );
    // ZEB-1031 §7/§9: voiding is out-of-band engine mutation (a D-FROST
    // reset-marker apply, not a kd=* poll event), so — unlike the events
    // above — it touches BOTH the list (the voided chip) and the detail
    // pane (the banner), mirroring subscribeTier3Finalized's dual refetch.
    unsubscribers.push(
      adapter.subscribeTier3Voided((p) => {
        if (p.communityId !== communityId) return;
        loadSummaries();
        if (selectedPollId && p.pollId === selectedPollId) refetchSelected();
      }),
    );
    // ZEB-1018 — D-FROST committee ceremony events. A completed beacon
    // drives stage transitions (sortition reveal, se-mode tally reveal),
    // so it refetches list + detail like the stage events above; the
    // round-progress events only update the transient status line.
    unsubscribers.push(
      adapter.subscribeDfrostDkgProgress((p) => {
        if (p.communityId !== communityId) return;
        ceremonyStatus = {
          ceremonyId: p.ceremonyId,
          text:
            `Committee key ceremony — round ${p.roundNum}` +
            ` (${p.participantsSoFar} contribution${p.participantsSoFar === 1 ? '' : 's'})`,
        };
      }),
    );
    unsubscribers.push(
      adapter.subscribeDfrostRefreshProgress((p) => {
        if (p.communityId !== communityId) return;
        ceremonyStatus = {
          ceremonyId: p.ceremonyId,
          text: `Committee key refresh — round ${p.roundNum}`,
        };
      }),
    );
    unsubscribers.push(
      adapter.subscribeDfrostBeaconReady((p) => {
        if (p.communityId !== communityId) return;
        // Clear only the ceremony this beacon concluded — a delayed
        // beacon from an older ceremony must not blank the in-flight
        // one's status. The refetch stays unconditional: any beacon in
        // this community can drive sortition/tally stage transitions.
        if (ceremonyStatus?.ceremonyId === p.ceremonyId) ceremonyStatus = null;
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

  {#if ceremonyStatus}
    <p class="ceremony-status" role="status">{ceremonyStatus.text}</p>
  {/if}

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

    <label>
      <span>Privacy mode</span>
      <select id="privacy-mode" bind:value={privacyMode}>
        <option value="pu">Public — all ballots visible</option>
        <option value="se">Ballot-secret — only the aggregate tally is revealed</option>
      </select>
    </label>
    {#if privacyMode === 'se'}
      <p class="help-text">
        🔒 Encrypted ballots; only the aggregate tally is decrypted after the
        ratification window closes. Requires the community's D-FROST committee
        to perform threshold decryption.
      </p>
    {/if}

    <button type="submit" disabled={!proposalText.trim()}>Create proposal</button>
    {#if createError}
      <p class="error">{createError}</p>
    {/if}
  </form>

  {#if confirmingCreate}
    <GovConfirmModal
      title="Confirm new Tier 3 proposal"
      confirmLabel={creating ? 'Creating…' : 'Confirm'}
      busy={creating}
      onConfirm={submitCreate}
      onCancel={() => (confirmingCreate = false)}
    >
      <p class="confirm-summary">
        "{proposalText.slice(0, 120)}{proposalText.length > 120 ? '…' : ''}"
      </p>
    </GovConfirmModal>
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
            {#if s.privacyMode === 'se'}
              <span class="privacy-chip" aria-label="ballot-secret poll" title="Ballot-secret">🔒</span>
            {/if}
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

        {#if selectedDetail.voided}
          <!-- ZEB-1031 §7: the poll stays at whatever stage it was voided
               at (orthogonal to `stage`) — ballots are ElGamal-encrypted
               to the retired committee and unrecoverable, so every
               interactive control below is suppressed in favor of this
               banner + a one-click relaunch (fresh poll, not a resume). -->
          <div class="voided-banner" role="alert" data-testid="voided-banner">
            <p class="voided-text">
              This poll was voided by committee reset {selectedDetail.voided.resetId.slice(0, 8)}… —
              ballots encrypted to the retired committee are unrecoverable; re-voting is honest.
            </p>
            {#if selectedDetail.proposer === myAddr || myPower >= POWER_THRESHOLDS.max}
              <button
                type="button"
                class="relaunch-btn"
                disabled={relaunching}
                onclick={() => relaunchVoided(selectedDetail!.pollId)}
              >
                {relaunching ? 'Relaunching…' : 'Relaunch'}
              </button>
            {/if}
            {#if relaunchError}
              <p class="error">{relaunchError}</p>
            {/if}
          </div>
        {:else if selectedDetail.stage === 'so'}
          <p>Awaiting sortition draw. The D-FROST committee must produce the VRF beacon before the mini-public is selected.</p>
        {:else if selectedDetail.stage === 'de' || selectedDetail.stage === 'dr' || selectedDetail.stage === 'ra' || selectedDetail.stage === 'fi'}
          <SortitionRevealView detail={selectedDetail} {myAddr} />
          {#if selectedDetail.stage === 'de'}
            <DeliberationView detail={selectedDetail} {adapter} {myAddr} onChange={refetchSelected} />
          {/if}
          {#if selectedDetail.myRole === 'mini_public' && (selectedDetail.stage === 'de' || selectedDetail.stage === 'dr')}
            <MiniPublicParticipationToggle detail={selectedDetail} {adapter} {myAddr} onDecline={refetchSelected} />
          {/if}
          {#if selectedDetail.stage === 'dr'}
            <DraftingPanel detail={selectedDetail} {adapter} {myAddr} onChange={refetchSelected} />
          {/if}
          {#if selectedDetail.stage === 'ra'}
            {#if shouldShowAwaitingTally(selectedDetail)}
              <p class="awaiting-tally">
                🔒 Ballots closed. Awaiting committee tally —
                {selectedDetail.encryptedTallyShareCount} / {selectedDetail.encryptedTallyThreshold}
                of {selectedDetail.encryptedTallyCommitteeSize} committee members have published shares.
              </p>
            {:else}
              <StarRatificationBallot detail={selectedDetail} {adapter} onCast={refetchSelected} />
            {/if}
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
  h2 { font-family: var(--font-display); font-weight: 500; }
  .ceremony-status {
    font-size: 0.85rem;
    opacity: 0.75;
    margin: 0 0 0.75rem;
  }
  .create-form {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
    padding: 1rem;
    background: var(--panel-bg);
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
    background: var(--input-bg);
    color: inherit;
    border: 1px solid var(--chip-bg);
    border-radius: 4px;
    padding: 0.4rem 0.5rem;
    font: inherit;
  }
  button[type="submit"] {
    align-self: flex-start;
    background: var(--accent);
    color: var(--on-accent);
    border: 0;
    padding: 0.5rem 1rem;
    border-radius: 4px;
    cursor: pointer;
  }
  button[type="submit"]:disabled { opacity: 0.5; cursor: not-allowed; }
  .confirm-summary { margin: 0; color: var(--text-secondary); }
  .list-heading { margin-top: 1.5rem; font-size: 1rem; }
  .poll-list { list-style: none; padding: 0; }
  .poll-row { display: flex; gap: 0.5rem; align-items: center; padding: 0.5rem 0; border-bottom: 1px solid var(--chip-bg); }
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
  .poll-row-button.selected { background: var(--primary-soft); }
  .proposal-text { font-weight: 500; }
  .retry-btn {
    background: transparent;
    color: var(--accent);
    border: 1px solid var(--accent);
    padding: 0.25rem 0.6rem;
    border-radius: 4px;
    cursor: pointer;
  }
  .detail-pane {
    margin-top: 1.5rem;
    padding: 1rem;
    background: var(--panel-bg);
    border-radius: 8px;
  }
  .stage-label { color: var(--text-faint); font-size: 0.85rem; margin-top: -0.25rem; }
  .error { color: var(--danger); }
  .empty { color: var(--text-faint); }
  .failed-detail { color: var(--vote-against); }
  /* ZEB-295 Phase 6 Task 11: ballot-secret affordances. Lock-icon chip
     on the list row, help text under the create-form privacy toggle,
     and the awaiting-tally banner on the ratification detail pane. */
  .privacy-chip {
    font-size: 0.85rem;
    padding: 0.05rem 0.35rem;
    border-radius: 3px;
    background: var(--sortition-bg);
    color: var(--gov-purple);
  }
  .help-text {
    color: var(--text-faint);
    font-size: 0.8rem;
    margin: -0.4rem 0 0;
    line-height: 1.4;
  }
  .awaiting-tally {
    margin: 0.75rem 0;
    padding: 0.6rem 0.8rem;
    background: var(--sortition-bg);
    border-left: 3px solid var(--gov-purple);
    border-radius: 3px;
    color: var(--text-doc);
    font-size: 0.9rem;
  }
  /* ZEB-1031 §7/§9: a poll voided by a D-FROST committee reset — same
     shape as .awaiting-tally but flagged, not informational. */
  .voided-banner {
    margin: 0.75rem 0;
    padding: 0.6rem 0.8rem;
    background: color-mix(in srgb, var(--vote-against) 12%, var(--panel-bg));
    border-left: 3px solid var(--vote-against);
    border-radius: 3px;
  }
  .voided-text {
    margin: 0 0 0.5rem;
    color: var(--text-doc);
    font-size: 0.9rem;
  }
  .relaunch-btn {
    background: var(--accent);
    color: var(--on-accent);
    border: 0;
    padding: 0.4rem 0.9rem;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.85rem;
  }
  .relaunch-btn:disabled { opacity: 0.5; cursor: not-allowed; }
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
  .badge.winner { background: var(--status-passed-bg); color: var(--status-passed-fg); }
  .badge.runner-up { background: var(--status-drafting-bg); color: var(--status-drafting-fg); }
  .finalists { margin-top: 0.5rem; color: var(--text-faint); font-size: 0.85rem; }
  .finalists ol { margin: 0.25rem 0; padding-left: 1.25rem; }
</style>
