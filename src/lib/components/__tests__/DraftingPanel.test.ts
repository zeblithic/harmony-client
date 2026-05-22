import { render, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import DraftingPanel from '../DraftingPanel.svelte';
import { VotingAdapter } from '../../voting-adapter';
import type { Tier3PollExport, DraftCandidateExport } from '../../types/voting';

const candidates: DraftCandidateExport[] = [
  { eventHash: 'aa'.repeat(32), text: 'Candidate A', proposer: 'pp'.repeat(32), approvalCount: 2 },
  { eventHash: 'bb'.repeat(32), text: 'Candidate B', proposer: 'qq'.repeat(32), approvalCount: 1 },
];

const baseDetail: Tier3PollExport = {
  pollId: 'aa'.repeat(32),
  communityId: '11'.repeat(16),
  proposalText: 'Amend §3',
  proposer: 'pp'.repeat(32),
  stage: 'dr',
  pollCreateHlcMs: 1_700_000_000_000,
  sortitionSize: 100,
  deliberationWindowSeconds: 1_209_600,
  draftingWindowSeconds: 604_800,
  ratificationWindowSeconds: 1_209_600,
  incentiveMode: 'd',
  miniPublic: ['mm'.repeat(32)],
  backupPool: [],
  declined: [],
  draftCandidates: candidates,
  ratificationCandidates: [],
  myRole: 'mini_public',
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

describe('DraftingPanel', () => {
  it('lists candidates with approval counts', () => {
    const adapter = new VotingAdapter();
    const { getByText } = render(DraftingPanel, {
      props: { detail: baseDetail, adapter, myAddr: 'mm'.repeat(32), onChange: () => {} },
    });
    expect(getByText('Candidate A')).toBeTruthy();
    expect(getByText('Candidate B')).toBeTruthy();
    expect(getByText(/2 approval/)).toBeTruthy();
  });

  it('mini-public members can propose new candidate via textarea', async () => {
    const adapter = new VotingAdapter();
    vi.spyOn(adapter, 'proposeDraftCandidate').mockResolvedValue('cc'.repeat(32));
    const onChange = vi.fn();
    const { getByLabelText, getByText } = render(DraftingPanel, {
      props: { detail: baseDetail, adapter, myAddr: 'mm'.repeat(32), onChange },
    });
    await fireEvent.input(getByLabelText(/Propose candidate/i), { target: { value: 'New candidate' } });
    await fireEvent.click(getByText(/Submit candidate/i));
    await waitFor(() => expect(adapter.proposeDraftCandidate).toHaveBeenCalledWith(baseDetail.pollId, 'New candidate'));
    expect(onChange).toHaveBeenCalled();
  });

  it('renders read-only when myRole === observer', () => {
    const adapter = new VotingAdapter();
    const { queryByLabelText } = render(DraftingPanel, {
      props: {
        detail: { ...baseDetail, myRole: 'observer' },
        adapter,
        myAddr: 'zz'.repeat(32),
        onChange: () => {},
      },
    });
    expect(queryByLabelText(/Propose candidate/i)).toBeNull();
  });

  it('approve button is disabled when already approved', () => {
    const adapter = new VotingAdapter();
    const { getAllByText } = render(DraftingPanel, {
      props: {
        detail: { ...baseDetail, myDraftingApprovals: ['aa'.repeat(32)] },
        adapter,
        myAddr: 'mm'.repeat(32),
        onChange: () => {},
      },
    });
    const buttons = getAllByText(/Approved|Approve/);
    expect((buttons[0] as HTMLButtonElement).disabled).toBe(true);
  });
});
