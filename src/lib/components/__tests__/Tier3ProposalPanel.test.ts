import { render, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import Tier3ProposalPanel from '../Tier3ProposalPanel.svelte';
import { VotingAdapter } from '../../voting-adapter';
import type { Tier3PollSummary } from '../../types/voting';

function createAdapterMock(summaries: Tier3PollSummary[] = []) {
  const adapter = new VotingAdapter();
  vi.spyOn(adapter, 'listTier3Polls').mockResolvedValue(summaries);
  vi.spyOn(adapter, 'createTier3Proposal').mockResolvedValue('pollid'.padEnd(64, '0'));
  vi.spyOn(adapter, 'subscribeTier3PollCreated').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3SortitionComplete').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3DraftingOpen').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3RatificationOpen').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3Finalized').mockReturnValue(() => {});
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

  it('polls selectedDetail every 5s while a poll is expanded', async () => {
    vi.useFakeTimers();
    try {
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
        },
      ]);
      vi.spyOn(adapter, 'getTier3Poll').mockResolvedValue({
        pollId: 'aa'.repeat(32),
        communityId: '11'.repeat(16),
        proposalText: 'Existing proposal',
        proposer: '22'.repeat(32),
        stage: 'dr',
        pollCreateHlcMs: 1_700_000_000_000,
        sortitionSize: 100,
        deliberationWindowSeconds: 1_209_600,
        draftingWindowSeconds: 604_800,
        ratificationWindowSeconds: 1_209_600,
        incentiveMode: 'd',
        miniPublic: [],
        backupPool: [],
        declined: [],
        draftCandidates: [],
        ratificationCandidates: [],
        myRole: 'observer',
        myDraftingApprovals: [],
        myRatificationScores: null,
        winnerEventHash: null,
        runnerUpEventHash: null,
      });

      const { findByText } = render(Tier3ProposalPanel, {
        props: { communityId: '11'.repeat(16), adapter, myAddr: '22'.repeat(32) },
      });
      // Wait for the list to render and click into the row to expand it.
      const row = await findByText('Existing proposal');
      await fireEvent.click(row);
      await vi.waitFor(() => expect(adapter.getTier3Poll).toHaveBeenCalledTimes(1));

      // Advance 5s — polling interval should fire one more getTier3Poll.
      await vi.advanceTimersByTimeAsync(5_000);
      expect(adapter.getTier3Poll).toHaveBeenCalledTimes(2);

      // Advance another 5s — third poll.
      await vi.advanceTimersByTimeAsync(5_000);
      expect(adapter.getTier3Poll).toHaveBeenCalledTimes(3);
    } finally {
      vi.useRealTimers();
    }
  });
});
