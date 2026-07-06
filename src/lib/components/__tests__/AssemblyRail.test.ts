import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import AssemblyRail from '../AssemblyRail.svelte';
import type { VotingAdapter } from '../../voting-adapter';
import type { Tier2ProposalExport } from '../../types/voting';

function makeProposal(overrides: Partial<Tier2ProposalExport> = {}): Tier2ProposalExport {
  return {
    proposal_id: 'aa'.repeat(32),
    community_id: 'c1',
    proposal_text: 'Fix the fountain',
    lifecycle: 'Open',
    total_conviction_ms: '100',
    threshold_conviction_ms: '1000',
    half_life_seconds: 3600,
    auto_exec: { kk: 'n' },
    total_supply: 10,
    voter_count: 1,
    ...overrides,
  };
}

/** Multi-handler voting mock (VotingAdapter is multi-subscriber). */
function makeVotingMock(initial: Tier2ProposalExport[]) {
  let listResult = initial;
  type H<T> = Array<(p: T) => void>;
  const created: H<{ proposalId: string; communityId: string }> = [];
  const reached: H<{ communityId: string; proposalId: string; thresholdReachedAtMs: number }> = [];
  const reverted: H<{ communityId: string; proposalId: string; revertedAtMs: number }> = [];
  const finalized: H<{ communityId: string; proposalId: string }> = [];
  const listTier2Proposals = vi.fn(async (_cid: string) => listResult);
  const adapter = {
    listTier2Proposals,
    subscribeProposalCreated: (h: (typeof created)[number]) => { created.push(h); return () => created.splice(created.indexOf(h), 1); },
    subscribeThresholdReached: (h: (typeof reached)[number]) => { reached.push(h); return () => reached.splice(reached.indexOf(h), 1); },
    subscribeThresholdReverted: (h: (typeof reverted)[number]) => { reverted.push(h); return () => reverted.splice(reverted.indexOf(h), 1); },
    subscribeProposalFinalized: (h: (typeof finalized)[number]) => { finalized.push(h); return () => finalized.splice(finalized.indexOf(h), 1); },
  } as unknown as VotingAdapter;
  return {
    adapter,
    listTier2Proposals,
    setList: (l: Tier2ProposalExport[]) => { listResult = l; },
    emitCreated: (p: { proposalId: string; communityId: string }) => [...created].forEach((h) => h(p)),
    handlerCount: () => created.length + reached.length + reverted.length + finalized.length,
  };
}

describe('AssemblyRail (ZEB-606)', () => {
  it('renders active proposals, ThresholdReached first then conviction desc', async () => {
    const { adapter } = makeVotingMock([
      makeProposal({ proposal_id: 'p-low', proposal_text: 'Low conviction', lifecycle: 'Open', total_conviction_ms: '10' }),
      makeProposal({ proposal_id: 'p-arch', proposal_text: 'Archived one', lifecycle: 'Archived' }),
      makeProposal({ proposal_id: 'p-thresh', proposal_text: 'Crossed threshold', lifecycle: 'ThresholdReached', total_conviction_ms: '5' }),
      makeProposal({ proposal_id: 'p-high', proposal_text: 'High conviction', lifecycle: 'Open', total_conviction_ms: '900' }),
    ]);
    const { container } = render(AssemblyRail, { props: { communityId: 'c1', adapter } });
    await waitFor(() => expect(screen.getByText('Crossed threshold')).toBeTruthy());
    expect(screen.queryByText('Archived one')).toBeNull();
    const text = container.textContent ?? '';
    expect(text.indexOf('Crossed threshold')).toBeLessThan(text.indexOf('High conviction'));
    expect(text.indexOf('High conviction')).toBeLessThan(text.indexOf('Low conviction'));
  });

  it('shows the empty state when no proposals are active', async () => {
    const { adapter } = makeVotingMock([makeProposal({ lifecycle: 'Finalized' })]);
    render(AssemblyRail, { props: { communityId: 'c1', adapter } });
    await waitFor(() => expect(screen.getByText('No open proposals')).toBeTruthy());
  });

  it('fires onViewAllProposals from the footer link', async () => {
    const { adapter } = makeVotingMock([]);
    const onViewAllProposals = vi.fn();
    render(AssemblyRail, { props: { communityId: 'c1', adapter, onViewAllProposals } });
    await waitFor(() => expect(screen.getByText('View all proposals →')).toBeTruthy());
    await fireEvent.click(screen.getByText('View all proposals →'));
    expect(onViewAllProposals).toHaveBeenCalledTimes(1);
  });

  it('refetches on a matching lifecycle event and ignores other communities', async () => {
    const mock = makeVotingMock([]);
    render(AssemblyRail, { props: { communityId: 'c1', adapter: mock.adapter } });
    await waitFor(() => expect(mock.listTier2Proposals).toHaveBeenCalledTimes(1));
    mock.setList([makeProposal({ proposal_text: 'Fresh proposal' })]);
    mock.emitCreated({ proposalId: 'px', communityId: 'other' });
    await new Promise((r) => setTimeout(r, 0));
    expect(mock.listTier2Proposals).toHaveBeenCalledTimes(1);
    mock.emitCreated({ proposalId: 'px', communityId: 'c1' });
    await waitFor(() => expect(screen.getByText('Fresh proposal')).toBeTruthy());
  });

  it('unsubscribes all handlers on destroy', async () => {
    const mock = makeVotingMock([]);
    const { unmount } = render(AssemblyRail, { props: { communityId: 'c1', adapter: mock.adapter } });
    await waitFor(() => expect(mock.handlerCount()).toBe(4));
    unmount();
    expect(mock.handlerCount()).toBe(0);
  });
});
