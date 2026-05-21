import { render, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import DeliberationView from '../DeliberationView.svelte';
import { VotingAdapter } from '../../voting-adapter';
import type { Tier3PollExport } from '../../types/voting';

function createDetail(overrides: Partial<Tier3PollExport> = {}): Tier3PollExport {
  return {
    pollId: 'aa'.repeat(32),
    communityId: '11'.repeat(16),
    proposalText: 'Test proposal',
    proposer: '22'.repeat(32),
    stage: 'de',
    pollCreateHlcMs: 1_700_000_000_000,
    sortitionSize: 100,
    deliberationWindowSeconds: 1_209_600,
    draftingWindowSeconds: 604_800,
    ratificationWindowSeconds: 1_209_600,
    incentiveMode: 'd',
    miniPublic: ['33'.repeat(32)],
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
    ...overrides,
  };
}

function createAdapterMock() {
  const adapter = new VotingAdapter();
  vi.spyOn(adapter, 'listBridgingStatements').mockResolvedValue([]);
  vi.spyOn(adapter, 'subscribeTier3DeliberationStatementCreated').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3DeliberationVoteCast').mockReturnValue(() => {});
  return adapter;
}

describe('DeliberationView', () => {
  it('renders composer for mini-public', () => {
    const adapter = createAdapterMock();
    const { getByText } = render(DeliberationView, {
      props: { detail: createDetail(), adapter, myAddr: '33'.repeat(32), onChange: () => {} },
    });
    expect(getByText(/Compose statement/i)).toBeTruthy();
  });

  it('hides composer for observer', () => {
    const adapter = createAdapterMock();
    const { queryByText } = render(DeliberationView, {
      props: { detail: createDetail({ myRole: 'observer' }), adapter, myAddr: 'zz'.repeat(32), onChange: () => {} },
    });
    expect(queryByText(/Compose statement/i)).toBeNull();
  });

  it('loads bridging scores on mount', async () => {
    const adapter = createAdapterMock();
    render(DeliberationView, {
      props: { detail: createDetail(), adapter, myAddr: '33'.repeat(32), onChange: () => {} },
    });
    await waitFor(() => expect(adapter.listBridgingStatements).toHaveBeenCalledTimes(1));
    expect(adapter.listBridgingStatements).toHaveBeenLastCalledWith('aa'.repeat(32), 10);
  });

  it('refreshes bridging when subscribeTier3DeliberationVoteCast fires', async () => {
    let voteHandler: (() => void) | null = null;
    const adapter = createAdapterMock();
    vi.spyOn(adapter, 'subscribeTier3DeliberationVoteCast').mockImplementation((h) => {
      voteHandler = h as () => void;
      return () => {};
    });
    render(DeliberationView, {
      props: { detail: createDetail(), adapter, myAddr: '33'.repeat(32), onChange: () => {} },
    });
    await waitFor(() => expect(adapter.listBridgingStatements).toHaveBeenCalledTimes(1));
    voteHandler!();
    await waitFor(() => expect(adapter.listBridgingStatements).toHaveBeenCalledTimes(2));
  });
});
