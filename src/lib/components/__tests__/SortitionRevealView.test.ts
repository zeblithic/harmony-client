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

describe('SortitionRevealView', () => {
  it('renders mini-public + backup with counts', () => {
    const { getByText } = render(SortitionRevealView, {
      props: { detail: baseDetail, myAddr: 'zz'.repeat(32) },
    });
    expect(getByText(/Mini-public \(2\)/)).toBeTruthy();
    expect(getByText(/Backup pool \(1\)/)).toBeTruthy();
  });

  it('shows "You were selected" banner when myRole=mini_public', () => {
    const { getByText } = render(SortitionRevealView, {
      props: { detail: { ...baseDetail, myRole: 'mini_public' }, myAddr: 'aa'.repeat(32) },
    });
    expect(getByText(/You were selected/i)).toBeTruthy();
  });

  it('shows "backup pool" banner when myRole=backup', () => {
    const { getByText } = render(SortitionRevealView, {
      props: { detail: { ...baseDetail, myRole: 'backup' }, myAddr: 'cc'.repeat(32) },
    });
    // Match banner copy specifically, not the "Backup pool (N)" roster heading.
    expect(getByText(/in the backup pool/i)).toBeTruthy();
  });

  it('shows no banner when myRole=observer (declined primary case)', () => {
    // Self is in static miniPublic but declined → backend projects myRole=observer.
    // Banner must NOT use the static roster as source of truth.
    const { queryByText } = render(SortitionRevealView, {
      props: {
        detail: {
          ...baseDetail,
          myRole: 'observer',
          declined: [['aa'.repeat(32), 1_700_000_500_000]],
        },
        myAddr: 'aa'.repeat(32),
      },
    });
    expect(queryByText(/You were selected/i)).toBeNull();
    expect(queryByText(/in the backup pool/i)).toBeNull();
  });

  it('shows "You were selected" banner for promoted backup (in static backupPool, myRole=mini_public)', () => {
    // Self is in static backupPool but a primary declined and self was promoted →
    // backend projects myRole=mini_public. Banner must trust myRole, not static roster.
    const { getByText, queryByText } = render(SortitionRevealView, {
      props: {
        detail: {
          ...baseDetail,
          myRole: 'mini_public',
          declined: [['bb'.repeat(32), 1_700_000_500_000]],
        },
        myAddr: 'cc'.repeat(32),
      },
    });
    expect(getByText(/You were selected/i)).toBeTruthy();
    expect(queryByText(/in the backup pool/i)).toBeNull();
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
