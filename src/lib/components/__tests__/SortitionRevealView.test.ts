import { render } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import SortitionRevealView from '../SortitionRevealView.svelte';
import type { Tier3PollExport } from '../../types/voting';

const baseDetail: Tier3PollExport = {
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
  miniPublic: ['aa'.repeat(32), 'bb'.repeat(32)],
  backupPool: ['cc'.repeat(32)],
  declined: [],
  draftCandidates: [],
  ratificationCandidates: [],
  myRole: 'observer',
  myDraftingApprovals: [],
  myRatificationScores: null,
  winnerEventHash: null,
  runnerUpEventHash: null,
};

describe('SortitionRevealView', () => {
  it('renders mini-public + backup with counts', () => {
    const { getByText } = render(SortitionRevealView, {
      props: { detail: baseDetail, myAddr: 'zz'.repeat(32) },
    });
    expect(getByText(/Mini-public \(2\)/)).toBeTruthy();
    expect(getByText(/Backup pool \(1\)/)).toBeTruthy();
  });

  it('highlights "You were selected!" when self in primary', () => {
    const { getByText } = render(SortitionRevealView, {
      props: { detail: { ...baseDetail, myRole: 'mini_public' }, myAddr: 'aa'.repeat(32) },
    });
    expect(getByText(/You were selected/i)).toBeTruthy();
  });

  it('shows declined members when present', () => {
    const { getByText } = render(SortitionRevealView, {
      props: {
        detail: { ...baseDetail, declined: [['bb'.repeat(32), 1_700_000_500_000]] },
        myAddr: 'zz'.repeat(32),
      },
    });
    expect(getByText(/Declined \(1\)/)).toBeTruthy();
  });
});
