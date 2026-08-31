import { render, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import Tier3ProposalPanel from '../Tier3ProposalPanel.svelte';
import { VotingAdapter } from '../../voting-adapter';
import type { Tier3PollSummary, Tier3PollExport } from '../../types/voting';

const TEST_COMMUNITY_ID = '11'.repeat(16);
const TEST_POLL_ID = 'aa'.repeat(32);
const TEST_MY_ADDR = '22'.repeat(32);

function makeExportFixture(): Tier3PollExport {
  return {
    pollId: TEST_POLL_ID,
    communityId: TEST_COMMUNITY_ID,
    proposalText: 'Existing proposal',
    proposer: TEST_MY_ADDR,
    stage: 'dr',
    pollCreateHlcMs: 1_700_000_000_000,
    sortitionSize: 100,
    deliberationWindowSeconds: 86400,
    draftingWindowSeconds: 86400,
    ratificationWindowSeconds: 86400,
    incentiveMode: 'a',
    miniPublic: [],
    backupPool: [],
    declined: [],
    draftCandidates: [],
    ratificationCandidates: [],
    myRole: 'observer',
    myDraftingApprovals: [],
    myRatificationScores: null,
    deliberationStatements: [],
    myDeliberationStatementCount: 0,
    myDeliberationVotes: [],
    winnerEventHash: null,
    runnerUpEventHash: null,
    privacyMode: 'pu',
    encryptedTallyShareCount: 0,
    encryptedTallyThreshold: 0,
    encryptedTallyCommitteeSize: 0,
  };
}

function makeSummaryFixture(): Tier3PollSummary {
  return {
    pollId: TEST_POLL_ID,
    communityId: TEST_COMMUNITY_ID,
    proposalText: 'Existing proposal',
    proposer: TEST_MY_ADDR,
    stage: 'dr',
    pollCreateHlcMs: 1_700_000_000_000,
    sortitionSize: 100,
    winnerText: null,
    privacyMode: 'pu',
  };
}

function createAdapterMock(summaries: Tier3PollSummary[] = []) {
  const adapter = new VotingAdapter();
  vi.spyOn(adapter, 'listTier3Polls').mockResolvedValue(summaries);
  vi.spyOn(adapter, 'createTier3Proposal').mockResolvedValue('pollid'.padEnd(64, '0'));
  vi.spyOn(adapter, 'getTier3Poll').mockResolvedValue(makeExportFixture());
  vi.spyOn(adapter, 'subscribeTier3PollCreated').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3SortitionComplete').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3DraftingOpen').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3RatificationOpen').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3Finalized').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3TallyShareApplied').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3MiniPublicDecline').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3DraftCandidate').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3DraftApproval').mockReturnValue(() => {});
  // ZEB-1031 §7/§9 — voided-poll event + relaunch IPC.
  vi.spyOn(adapter, 'subscribeTier3Voided').mockReturnValue(() => {});
  vi.spyOn(adapter, 'relaunchVoidedPoll').mockResolvedValue('ff'.repeat(32));
  // ZEB-1018 — D-FROST ceremony event subscribers.
  vi.spyOn(adapter, 'subscribeDfrostDkgProgress').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeDfrostBeaconReady').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeDfrostRefreshProgress').mockReturnValue(() => {});
  return adapter;
}

describe('Tier3ProposalPanel', () => {
  it('lists existing Tier 3 polls on mount', async () => {
    const adapter = createAdapterMock([
      {
        pollId: 'aa'.repeat(32),
        communityId: '11'.repeat(16),
        proposalText: 'Existing proposal',
        proposer: '22'.repeat(32),
        stage: 'dr',
        pollCreateHlcMs: 1_700_000_000_000,
        sortitionSize: 100,
        winnerText: null,
        privacyMode: 'pu',
      },
    ]);
    const { findByText } = render(Tier3ProposalPanel, {
      props: { communityId: '11'.repeat(16), adapter, myAddr: '22'.repeat(32) },
    });
    expect(await findByText('Existing proposal')).toBeTruthy();
  });

  it('opens click-confirm before invoking createTier3Proposal', async () => {
    const adapter = createAdapterMock();
    const { getByLabelText, findByText } = render(Tier3ProposalPanel, {
      props: { communityId: '11'.repeat(16), adapter, myAddr: '22'.repeat(32) },
    });
    await fireEvent.input(getByLabelText(/Proposal text/i), { target: { value: 'New' } });
    await fireEvent.click(await findByText(/Create proposal/i));
    // Confirm modal appears
    expect(await findByText(/Confirm new Tier 3 proposal/i)).toBeTruthy();
    // Not yet invoked
    expect(adapter.createTier3Proposal).not.toHaveBeenCalled();
    // Click the confirm button
    await fireEvent.click(await findByText(/^Confirm$/i));
    await waitFor(() => expect(adapter.createTier3Proposal).toHaveBeenCalledTimes(1));
  });

  it('refetches the list when subscribeTier3Finalized fires', async () => {
    let finalizedHandler: (() => void) | null = null;
    const adapter = createAdapterMock();
    vi.spyOn(adapter, 'subscribeTier3Finalized').mockImplementation((h) => {
      finalizedHandler = h as () => void;
      return () => {};
    });
    render(Tier3ProposalPanel, {
      props: { communityId: '11'.repeat(16), adapter, myAddr: '22'.repeat(32) },
    });
    await waitFor(() => expect(adapter.listTier3Polls).toHaveBeenCalledTimes(1));
    finalizedHandler!();
    await waitFor(() => expect(adapter.listTier3Polls).toHaveBeenCalledTimes(2));
  });

  it('reloads list + resets selection when communityId prop changes', async () => {
    const adapter = createAdapterMock();
    const { rerender } = render(Tier3ProposalPanel, {
      props: { communityId: '11'.repeat(16), adapter, myAddr: '22'.repeat(32) },
    });
    await waitFor(() => expect(adapter.listTier3Polls).toHaveBeenCalledTimes(1));
    expect(adapter.listTier3Polls).toHaveBeenLastCalledWith('11'.repeat(16));

    // Switch communities via prop rebinding.
    await rerender({ communityId: '99'.repeat(16), adapter, myAddr: '22'.repeat(32) });
    await waitFor(() => expect(adapter.listTier3Polls).toHaveBeenCalledTimes(2));
    expect(adapter.listTier3Polls).toHaveBeenLastCalledWith('99'.repeat(16));
  });

  it('re-arms event subscriptions on communityId change (no leaked subscribers)', async () => {
    const adapter = createAdapterMock();
    const unsubscribe = vi.fn();
    vi.spyOn(adapter, 'subscribeTier3Finalized').mockImplementation(() => unsubscribe);
    const { rerender } = render(Tier3ProposalPanel, {
      props: { communityId: '11'.repeat(16), adapter, myAddr: '22'.repeat(32) },
    });
    await waitFor(() => expect(adapter.subscribeTier3Finalized).toHaveBeenCalledTimes(1));

    await rerender({ communityId: '99'.repeat(16), adapter, myAddr: '22'.repeat(32) });
    await waitFor(() => expect(adapter.subscribeTier3Finalized).toHaveBeenCalledTimes(2));
    // Prior subscription's teardown was invoked exactly once on switch.
    expect(unsubscribe).toHaveBeenCalledTimes(1);
  });

  it('drops stale listTier3Polls response when communityId switches mid-flight', async () => {
    // Race: communityId switches while the first listTier3Polls is still
    // in flight. The second IPC for the new community must "win" even if
    // the older one resolves last. Without the seq guard, the stale
    // resolution would overwrite `summaries` with the previous
    // community's polls.
    const adapter = new VotingAdapter();
    let resolveA!: (v: Tier3PollSummary[]) => void;
    let resolveB!: (v: Tier3PollSummary[]) => void;
    const aPromise = new Promise<Tier3PollSummary[]>((r) => {
      resolveA = r;
    });
    const bPromise = new Promise<Tier3PollSummary[]>((r) => {
      resolveB = r;
    });
    let callIdx = 0;
    vi.spyOn(adapter, 'listTier3Polls').mockImplementation(async () => {
      callIdx += 1;
      return callIdx === 1 ? aPromise : bPromise;
    });
    vi.spyOn(adapter, 'getTier3Poll').mockResolvedValue(makeExportFixture());
    vi.spyOn(adapter, 'subscribeTier3PollCreated').mockReturnValue(() => {});
    vi.spyOn(adapter, 'subscribeTier3SortitionComplete').mockReturnValue(() => {});
    vi.spyOn(adapter, 'subscribeTier3DraftingOpen').mockReturnValue(() => {});
    vi.spyOn(adapter, 'subscribeTier3RatificationOpen').mockReturnValue(() => {});
    vi.spyOn(adapter, 'subscribeTier3Finalized').mockReturnValue(() => {});
    vi.spyOn(adapter, 'subscribeTier3TallyShareApplied').mockReturnValue(() => {});
    vi.spyOn(adapter, 'subscribeTier3MiniPublicDecline').mockReturnValue(() => {});
    vi.spyOn(adapter, 'subscribeTier3DraftCandidate').mockReturnValue(() => {});
    vi.spyOn(adapter, 'subscribeTier3DraftApproval').mockReturnValue(() => {});
    vi.spyOn(adapter, 'subscribeTier3Voided').mockReturnValue(() => {});
    vi.spyOn(adapter, 'subscribeDfrostDkgProgress').mockReturnValue(() => {});
    vi.spyOn(adapter, 'subscribeDfrostBeaconReady').mockReturnValue(() => {});
    vi.spyOn(adapter, 'subscribeDfrostRefreshProgress').mockReturnValue(() => {});

    const aSummary: Tier3PollSummary = {
      pollId: 'aa'.repeat(32),
      communityId: 'aa'.repeat(16),
      proposalText: 'Community A proposal',
      proposer: '22'.repeat(32),
      stage: 'dr',
      pollCreateHlcMs: 1_700_000_000_000,
      sortitionSize: 100,
      winnerText: null,
      privacyMode: 'pu',
    };
    const bSummary: Tier3PollSummary = {
      pollId: 'bb'.repeat(32),
      communityId: 'bb'.repeat(16),
      proposalText: 'Community B proposal',
      proposer: '22'.repeat(32),
      stage: 'dr',
      pollCreateHlcMs: 1_700_000_000_000,
      sortitionSize: 100,
      winnerText: null,
      privacyMode: 'pu',
    };

    const { rerender, queryByText, findByText } = render(Tier3ProposalPanel, {
      props: { communityId: 'aa'.repeat(16), adapter, myAddr: '22'.repeat(32) },
    });
    // Switch to community B while A's listTier3Polls is still pending.
    await rerender({ communityId: 'bb'.repeat(16), adapter, myAddr: '22'.repeat(32) });
    await waitFor(() => expect(adapter.listTier3Polls).toHaveBeenCalledTimes(2));

    // Resolve B's IPC first (the newer call), then A's (the stale call).
    resolveB([bSummary]);
    await findByText('Community B proposal');
    resolveA([aSummary]);
    // Flush microtasks so any stale write would have landed by now.
    await new Promise((r) => setTimeout(r, 0));
    expect(queryByText('Community A proposal')).toBeNull();
    expect(queryByText('Community B proposal')).toBeTruthy();
  });

  // ── ZEB-319: event-driven refetch tests ──────────────────────────────────

  it('refetches detail only (not summaries) on voting-tier3-mini-public-decline matching selected community + poll', async () => {
    let declineHandler: ((p: { pollId: string; communityId: string; decliner: string; declineHlcMs: number }) => void) | null = null;
    const adapter = createAdapterMock([makeSummaryFixture()]);
    vi.spyOn(adapter, 'subscribeTier3MiniPublicDecline').mockImplementation((h) => {
      declineHandler = h as typeof declineHandler;
      return () => {};
    });

    const { findByText } = render(Tier3ProposalPanel, {
      props: { communityId: TEST_COMMUNITY_ID, adapter, myAddr: TEST_MY_ADDR },
    });

    // Select the poll so selectedPollId is set.
    await fireEvent.click(await findByText('Existing proposal'));
    await waitFor(() => expect(adapter.getTier3Poll).toHaveBeenCalledTimes(1));
    expect(declineHandler).not.toBeNull();

    vi.mocked(adapter.getTier3Poll).mockClear();
    vi.mocked(adapter.listTier3Polls).mockClear();

    declineHandler!({
      communityId: TEST_COMMUNITY_ID,
      pollId: TEST_POLL_ID,
      decliner: 'cc'.repeat(32),
      declineHlcMs: 1_234_567_890,
    });

    await waitFor(() => expect(adapter.getTier3Poll).toHaveBeenCalledTimes(1));
    expect(adapter.getTier3Poll).toHaveBeenCalledWith(TEST_POLL_ID);
    // Summary DTO does not include decline / roster fields, so loadSummaries
    // must NOT be triggered by a decline event.
    expect(adapter.listTier3Polls).not.toHaveBeenCalled();
  });

  it('ignores voting-tier3-mini-public-decline with mismatched communityId', async () => {
    let declineHandler: ((p: { pollId: string; communityId: string; decliner: string; declineHlcMs: number }) => void) | null = null;
    const adapter = createAdapterMock([makeSummaryFixture()]);
    vi.spyOn(adapter, 'subscribeTier3MiniPublicDecline').mockImplementation((h) => {
      declineHandler = h as typeof declineHandler;
      return () => {};
    });

    const { findByText } = render(Tier3ProposalPanel, {
      props: { communityId: TEST_COMMUNITY_ID, adapter, myAddr: TEST_MY_ADDR },
    });

    await fireEvent.click(await findByText('Existing proposal'));
    await waitFor(() => expect(adapter.getTier3Poll).toHaveBeenCalledTimes(1));
    expect(declineHandler).not.toBeNull();

    vi.mocked(adapter.getTier3Poll).mockClear();
    vi.mocked(adapter.listTier3Polls).mockClear();

    // Fire with a different communityId — panel should ignore it entirely.
    declineHandler!({
      communityId: 'ff'.repeat(16),
      pollId: TEST_POLL_ID,
      decliner: 'cc'.repeat(32),
      declineHlcMs: 1_234_567_890,
    });

    // Allow a tick for any async effects to settle.
    await new Promise((r) => setTimeout(r, 0));

    expect(adapter.getTier3Poll).not.toHaveBeenCalled();
    expect(adapter.listTier3Polls).not.toHaveBeenCalled();
  });

  it('refetches detail on voting-tier3-draft-candidate matching selected community + poll', async () => {
    let candidateHandler: ((p: { pollId: string; communityId: string; proposer: string; eventHash: string; candidateText: string }) => void) | null = null;
    const adapter = createAdapterMock([makeSummaryFixture()]);
    vi.spyOn(adapter, 'subscribeTier3DraftCandidate').mockImplementation((h) => {
      candidateHandler = h as typeof candidateHandler;
      return () => {};
    });

    const { findByText } = render(Tier3ProposalPanel, {
      props: { communityId: TEST_COMMUNITY_ID, adapter, myAddr: TEST_MY_ADDR },
    });

    await fireEvent.click(await findByText('Existing proposal'));
    await waitFor(() => expect(adapter.getTier3Poll).toHaveBeenCalledTimes(1));
    expect(candidateHandler).not.toBeNull();

    vi.mocked(adapter.getTier3Poll).mockClear();
    vi.mocked(adapter.listTier3Polls).mockClear();

    candidateHandler!({
      communityId: TEST_COMMUNITY_ID,
      pollId: TEST_POLL_ID,
      proposer: TEST_MY_ADDR,
      eventHash: 'dd'.repeat(32),
      candidateText: 'Draft text',
    });

    await waitFor(() => expect(adapter.getTier3Poll).toHaveBeenCalledTimes(1));
    expect(adapter.getTier3Poll).toHaveBeenCalledWith(TEST_POLL_ID);
    // Draft-candidate does NOT trigger loadSummaries — only refetchSelected.
    expect(adapter.listTier3Polls).not.toHaveBeenCalled();
  });

  it('refetches detail on voting-tier3-draft-approval matching selected community + poll', async () => {
    let approvalHandler: ((p: { pollId: string; communityId: string; approver: string; targetEventHash: string }) => void) | null = null;
    const adapter = createAdapterMock([makeSummaryFixture()]);
    vi.spyOn(adapter, 'subscribeTier3DraftApproval').mockImplementation((h) => {
      approvalHandler = h as typeof approvalHandler;
      return () => {};
    });

    const { findByText } = render(Tier3ProposalPanel, {
      props: { communityId: TEST_COMMUNITY_ID, adapter, myAddr: TEST_MY_ADDR },
    });

    await fireEvent.click(await findByText('Existing proposal'));
    await waitFor(() => expect(adapter.getTier3Poll).toHaveBeenCalledTimes(1));
    expect(approvalHandler).not.toBeNull();

    vi.mocked(adapter.getTier3Poll).mockClear();
    vi.mocked(adapter.listTier3Polls).mockClear();

    approvalHandler!({
      communityId: TEST_COMMUNITY_ID,
      pollId: TEST_POLL_ID,
      approver: TEST_MY_ADDR,
      targetEventHash: 'ee'.repeat(32),
    });

    await waitFor(() => expect(adapter.getTier3Poll).toHaveBeenCalledTimes(1));
    expect(adapter.getTier3Poll).toHaveBeenCalledWith(TEST_POLL_ID);
    // Draft-approval does NOT trigger loadSummaries — only refetchSelected.
    expect(adapter.listTier3Polls).not.toHaveBeenCalled();
  });

  // ── ZEB-1018: D-FROST ceremony event tests ───────────────────────────────

  it('refetches list + detail and clears ceremony status on dfrost-beacon-ready for this community', async () => {
    let dkgHandler: ((p: { communityId: string; ceremonyId: string; roundNum: number; participantsSoFar: number }) => void) | null = null;
    let beaconHandler: ((p: { communityId: string; ceremonyId: string; vrfOutput: string }) => void) | null = null;
    const adapter = createAdapterMock([makeSummaryFixture()]);
    vi.spyOn(adapter, 'subscribeDfrostDkgProgress').mockImplementation((h) => {
      dkgHandler = h as typeof dkgHandler;
      return () => {};
    });
    vi.spyOn(adapter, 'subscribeDfrostBeaconReady').mockImplementation((h) => {
      beaconHandler = h as typeof beaconHandler;
      return () => {};
    });

    const { findByText, queryByText } = render(Tier3ProposalPanel, {
      props: { communityId: TEST_COMMUNITY_ID, adapter, myAddr: TEST_MY_ADDR },
    });

    await fireEvent.click(await findByText('Existing proposal'));
    await waitFor(() => expect(adapter.getTier3Poll).toHaveBeenCalledTimes(1));
    expect(dkgHandler).not.toBeNull();
    expect(beaconHandler).not.toBeNull();

    // A DKG progress event surfaces the ceremony status line.
    dkgHandler!({
      communityId: TEST_COMMUNITY_ID,
      ceremonyId: 'ab'.repeat(32),
      roundNum: 2,
      participantsSoFar: 3,
    });
    expect(
      await findByText(/Committee key ceremony — round 2 \(3 contributions\)/),
    ).toBeTruthy();

    vi.mocked(adapter.getTier3Poll).mockClear();
    vi.mocked(adapter.listTier3Polls).mockClear();

    // A delayed beacon from an OLDER ceremony still refetches (any
    // beacon can drive stage transitions) but must NOT blank the
    // in-flight ceremony's status (CodeAnt PR #768).
    beaconHandler!({
      communityId: TEST_COMMUNITY_ID,
      ceremonyId: 'ee'.repeat(32),
      vrfOutput: 'cd'.repeat(32),
    });
    await waitFor(() => expect(adapter.listTier3Polls).toHaveBeenCalledTimes(1));
    expect(
      await findByText(/Committee key ceremony — round 2 \(3 contributions\)/),
    ).toBeTruthy();

    // The matching ceremony's beacon clears the status and refetches.
    beaconHandler!({
      communityId: TEST_COMMUNITY_ID,
      ceremonyId: 'ab'.repeat(32),
      vrfOutput: 'cd'.repeat(32),
    });

    await waitFor(() => expect(adapter.listTier3Polls).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(adapter.getTier3Poll).toHaveBeenCalledTimes(2));
    expect(queryByText(/Committee key ceremony/)).toBeNull();
  });

  it('ignores dfrost events for a different community', async () => {
    let dkgHandler: ((p: { communityId: string; ceremonyId: string; roundNum: number; participantsSoFar: number }) => void) | null = null;
    let beaconHandler: ((p: { communityId: string; ceremonyId: string; vrfOutput: string }) => void) | null = null;
    const adapter = createAdapterMock([makeSummaryFixture()]);
    vi.spyOn(adapter, 'subscribeDfrostDkgProgress').mockImplementation((h) => {
      dkgHandler = h as typeof dkgHandler;
      return () => {};
    });
    vi.spyOn(adapter, 'subscribeDfrostBeaconReady').mockImplementation((h) => {
      beaconHandler = h as typeof beaconHandler;
      return () => {};
    });

    const { queryByText, findByText } = render(Tier3ProposalPanel, {
      props: { communityId: TEST_COMMUNITY_ID, adapter, myAddr: TEST_MY_ADDR },
    });
    await findByText('Existing proposal');
    await waitFor(() => expect(dkgHandler).not.toBeNull());

    vi.mocked(adapter.listTier3Polls).mockClear();

    dkgHandler!({
      communityId: 'ff'.repeat(16),
      ceremonyId: 'ab'.repeat(32),
      roundNum: 1,
      participantsSoFar: 1,
    });
    beaconHandler!({
      communityId: 'ff'.repeat(16),
      ceremonyId: 'ab'.repeat(32),
      vrfOutput: 'cd'.repeat(32),
    });
    await new Promise((r) => setTimeout(r, 0));

    expect(queryByText(/Committee key ceremony/)).toBeNull();
    expect(adapter.listTier3Polls).not.toHaveBeenCalled();
  });

  it('does not schedule a polling interval after mount', async () => {
    const adapter = createAdapterMock([makeSummaryFixture()]);

    // Mount with real timers so the async Svelte $effect / mount lifecycle
    // settles cleanly before we take over the clock.
    render(Tier3ProposalPanel, {
      props: { communityId: TEST_COMMUNITY_ID, adapter, myAddr: TEST_MY_ADDR },
    });

    // Let mount complete: listTier3Polls is called once on mount.
    await waitFor(() => expect(adapter.listTier3Polls).toHaveBeenCalledTimes(1));

    // Switch to fake timers AFTER mount so Svelte internals aren't disrupted.
    vi.useFakeTimers();
    try {
      // Clear call counts to focus on post-mount activity only.
      vi.mocked(adapter.getTier3Poll).mockClear();
      vi.mocked(adapter.listTier3Polls).mockClear();

      // Advance 10 seconds — without polling, no extra refetches should fire.
      vi.advanceTimersByTime(10_000);
      // Allow microtasks to flush after the timer advance.
      await Promise.resolve();

      expect(adapter.getTier3Poll).not.toHaveBeenCalled();
      expect(adapter.listTier3Polls).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  // ── ZEB-1031 §7/§9: voided-poll banner + relaunch ────────────────────────

  it('renders the voided banner with a short hex reset id and suppresses the stage controls', async () => {
    const adapter = createAdapterMock([makeSummaryFixture()]);
    vi.spyOn(adapter, 'getTier3Poll').mockResolvedValue({
      ...makeExportFixture(),
      stage: 'so',
      voided: { resetId: 'ab'.repeat(16), oldEpoch: 3 },
    });

    const { findByText, queryByText, getByTestId } = render(Tier3ProposalPanel, {
      props: { communityId: TEST_COMMUNITY_ID, adapter, myAddr: TEST_MY_ADDR },
    });
    await fireEvent.click(await findByText('Existing proposal'));

    const banner = await waitFor(() => getByTestId('voided-banner'));
    expect(banner.textContent).toContain('ab'.repeat(16).slice(0, 8));
    expect(banner.textContent).toMatch(/unrecoverable/);
    expect(banner.textContent).toMatch(/re-voting is honest/);
    // Voided is orthogonal to stage — the poll stayed at 'so', but its
    // interactive stage-dispatch controls must not render.
    expect(queryByText(/Awaiting sortition draw/)).toBeNull();
  });

  it('hides Relaunch for a viewer who is neither the creator nor an admin', async () => {
    const adapter = createAdapterMock([makeSummaryFixture()]);
    vi.spyOn(adapter, 'getTier3Poll').mockResolvedValue({
      ...makeExportFixture(),
      proposer: 'ee'.repeat(32), // not TEST_MY_ADDR
      voided: { resetId: 'ab'.repeat(16), oldEpoch: 3 },
    });

    const { findByText, queryByText, getByTestId } = render(Tier3ProposalPanel, {
      props: { communityId: TEST_COMMUNITY_ID, adapter, myAddr: TEST_MY_ADDR, myPower: 0 },
    });
    await fireEvent.click(await findByText('Existing proposal'));
    await waitFor(() => getByTestId('voided-banner'));
    expect(queryByText('Relaunch')).toBeNull();
  });

  it('shows Relaunch for the poll creator, calls relaunchVoidedPoll with the pollId, and navigates to the new poll', async () => {
    const adapter = createAdapterMock([makeSummaryFixture()]);
    vi.spyOn(adapter, 'getTier3Poll').mockResolvedValue({
      ...makeExportFixture(),
      proposer: TEST_MY_ADDR,
      voided: { resetId: 'ab'.repeat(16), oldEpoch: 3 },
    });

    const { findByText, getByTestId } = render(Tier3ProposalPanel, {
      props: { communityId: TEST_COMMUNITY_ID, adapter, myAddr: TEST_MY_ADDR, myPower: 0 },
    });
    await fireEvent.click(await findByText('Existing proposal'));
    await waitFor(() => getByTestId('voided-banner'));

    vi.mocked(adapter.getTier3Poll).mockClear();
    await fireEvent.click(await findByText('Relaunch'));

    await waitFor(() => expect(adapter.relaunchVoidedPoll).toHaveBeenCalledWith(TEST_POLL_ID));
    // Navigates the detail pane to the newly relaunched poll.
    await waitFor(() => expect(adapter.getTier3Poll).toHaveBeenCalledWith('ff'.repeat(32)));
  });

  it('shows Relaunch for an admin (myPower >= 100) even when not the creator', async () => {
    const adapter = createAdapterMock([makeSummaryFixture()]);
    vi.spyOn(adapter, 'getTier3Poll').mockResolvedValue({
      ...makeExportFixture(),
      proposer: 'ee'.repeat(32), // not TEST_MY_ADDR
      voided: { resetId: 'ab'.repeat(16), oldEpoch: 3 },
    });

    const { findByText, getByTestId } = render(Tier3ProposalPanel, {
      props: { communityId: TEST_COMMUNITY_ID, adapter, myAddr: TEST_MY_ADDR, myPower: 100 },
    });
    await fireEvent.click(await findByText('Existing proposal'));
    await waitFor(() => getByTestId('voided-banner'));
    expect(await findByText('Relaunch')).toBeTruthy();
  });

  it('surfaces a relaunchVoidedPoll rejection via the standard error-extraction convention', async () => {
    const adapter = createAdapterMock([makeSummaryFixture()]);
    vi.spyOn(adapter, 'getTier3Poll').mockResolvedValue({
      ...makeExportFixture(),
      proposer: TEST_MY_ADDR,
      voided: { resetId: 'ab'.repeat(16), oldEpoch: 3 },
    });
    vi.spyOn(adapter, 'relaunchVoidedPoll').mockRejectedValue(new Error('not authorized'));

    const { findByText, getByTestId } = render(Tier3ProposalPanel, {
      props: { communityId: TEST_COMMUNITY_ID, adapter, myAddr: TEST_MY_ADDR },
    });
    await fireEvent.click(await findByText('Existing proposal'));
    await waitFor(() => getByTestId('voided-banner'));
    await fireEvent.click(await findByText('Relaunch'));
    expect(await findByText('not authorized')).toBeTruthy();
  });

  it('refetches list + detail on voting-tier3-voided matching selected community + poll', async () => {
    let voidedHandler:
      | ((p: { pollId: string; communityId: string; resetId: string; oldEpoch: number }) => void)
      | null = null;
    const adapter = createAdapterMock([makeSummaryFixture()]);
    vi.spyOn(adapter, 'subscribeTier3Voided').mockImplementation((h) => {
      voidedHandler = h as typeof voidedHandler;
      return () => {};
    });

    const { findByText } = render(Tier3ProposalPanel, {
      props: { communityId: TEST_COMMUNITY_ID, adapter, myAddr: TEST_MY_ADDR },
    });

    await fireEvent.click(await findByText('Existing proposal'));
    await waitFor(() => expect(adapter.getTier3Poll).toHaveBeenCalledTimes(1));
    expect(voidedHandler).not.toBeNull();

    vi.mocked(adapter.getTier3Poll).mockClear();
    vi.mocked(adapter.listTier3Polls).mockClear();

    voidedHandler!({
      communityId: TEST_COMMUNITY_ID,
      pollId: TEST_POLL_ID,
      resetId: 'ab'.repeat(16),
      oldEpoch: 3,
    });

    // Unlike the mini-public-decline / draft-candidate / draft-approval
    // events, voiding is out-of-band engine mutation that also changes the
    // list-row badge — so it refetches BOTH the summaries and the detail
    // (mirrors subscribeTier3Finalized's dual refetch).
    await waitFor(() => expect(adapter.getTier3Poll).toHaveBeenCalledTimes(1));
    expect(adapter.getTier3Poll).toHaveBeenCalledWith(TEST_POLL_ID);
    await waitFor(() => expect(adapter.listTier3Polls).toHaveBeenCalledTimes(1));
  });

  it('ignores voting-tier3-voided with mismatched communityId', async () => {
    let voidedHandler:
      | ((p: { pollId: string; communityId: string; resetId: string; oldEpoch: number }) => void)
      | null = null;
    const adapter = createAdapterMock([makeSummaryFixture()]);
    vi.spyOn(adapter, 'subscribeTier3Voided').mockImplementation((h) => {
      voidedHandler = h as typeof voidedHandler;
      return () => {};
    });

    const { findByText } = render(Tier3ProposalPanel, {
      props: { communityId: TEST_COMMUNITY_ID, adapter, myAddr: TEST_MY_ADDR },
    });
    await findByText('Existing proposal');
    await waitFor(() => expect(voidedHandler).not.toBeNull());

    vi.mocked(adapter.getTier3Poll).mockClear();
    vi.mocked(adapter.listTier3Polls).mockClear();

    voidedHandler!({
      communityId: 'ff'.repeat(16),
      pollId: TEST_POLL_ID,
      resetId: 'ab'.repeat(16),
      oldEpoch: 3,
    });
    await new Promise((r) => setTimeout(r, 0));

    expect(adapter.getTier3Poll).not.toHaveBeenCalled();
    expect(adapter.listTier3Polls).not.toHaveBeenCalled();
  });
});

