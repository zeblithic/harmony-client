import { render, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import MiniPublicParticipationToggle from '../MiniPublicParticipationToggle.svelte';
import { VotingAdapter } from '../../voting-adapter';
import type { Tier3PollExport } from '../../types/voting';

const detail: Tier3PollExport = {
  pollId: 'aa'.repeat(32),
  communityId: '11'.repeat(16),
  proposalText: 'Amend §3',
  proposer: 'pp'.repeat(32),
  stage: 'de',
  pollCreateHlcMs: 1_700_000_000_000,
  sortitionSize: 100,
  deliberationWindowSeconds: 1_209_600,
  draftingWindowSeconds: 604_800,
  ratificationWindowSeconds: 1_209_600,
  incentiveMode: 'd',
  miniPublic: ['mm'.repeat(32)],
  backupPool: [],
  declined: [],
  draftCandidates: [],
  ratificationCandidates: [],
  myRole: 'mini_public',
  myDraftingApprovals: [],
  myRatificationScores: null,
  deliberationStatements: [],
  myDeliberationStatementCount: 0,
  myDeliberationVotes: [],
  winnerEventHash: null,
  runnerUpEventHash: null,
};

describe('MiniPublicParticipationToggle', () => {
  it('renders Decline button when not yet declined', () => {
    const adapter = new VotingAdapter();
    const { getByText } = render(MiniPublicParticipationToggle, {
      props: { detail, adapter, myAddr: 'mm'.repeat(32), onDecline: () => {} },
    });
    expect(getByText(/Decline mini-public role/i)).toBeTruthy();
  });

  it('invokes declineSortition with the pollId on click', async () => {
    const adapter = new VotingAdapter();
    vi.spyOn(adapter, 'declineSortition').mockResolvedValue();
    const onDecline = vi.fn();
    const { getByText } = render(MiniPublicParticipationToggle, {
      props: { detail, adapter, myAddr: 'mm'.repeat(32), onDecline },
    });
    await fireEvent.click(getByText(/Decline mini-public role/i));
    await waitFor(() => expect(adapter.declineSortition).toHaveBeenCalledWith(detail.pollId, undefined));
    expect(onDecline).toHaveBeenCalled();
  });

  it('shows already-declined message when self is in declined set', () => {
    const declinedDetail = {
      ...detail,
      declined: [['mm'.repeat(32), 1_700_000_500_000]] as [string, number][],
    };
    const adapter = new VotingAdapter();
    const { getByText } = render(MiniPublicParticipationToggle, {
      props: { detail: declinedDetail, adapter, myAddr: 'mm'.repeat(32), onDecline: () => {} },
    });
    expect(getByText(/You declined this role/i)).toBeTruthy();
  });
});
