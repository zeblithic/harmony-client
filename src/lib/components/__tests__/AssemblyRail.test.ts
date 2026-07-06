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
  const signalCast: H<{ proposalId: string; voter: string; support: boolean }> = [];
  const delegation: H<{ communityId: string; delegator: string; delegate: string | null }> = [];
  const listTier2Proposals = vi.fn(async (_cid: string) => listResult);
  const getMyDelegate = vi.fn(async (_cid: string): Promise<string | null> => null);
  const adapter = {
    listTier2Proposals,
    getMyDelegate,
    subscribeProposalCreated: (h: (typeof created)[number]) => { created.push(h); return () => created.splice(created.indexOf(h), 1); },
    subscribeThresholdReached: (h: (typeof reached)[number]) => { reached.push(h); return () => reached.splice(reached.indexOf(h), 1); },
    subscribeThresholdReverted: (h: (typeof reverted)[number]) => { reverted.push(h); return () => reverted.splice(reverted.indexOf(h), 1); },
    subscribeProposalFinalized: (h: (typeof finalized)[number]) => { finalized.push(h); return () => finalized.splice(finalized.indexOf(h), 1); },
    subscribeSignalCast: (h: (typeof signalCast)[number]) => { signalCast.push(h); return () => signalCast.splice(signalCast.indexOf(h), 1); },
    subscribeDelegationChanged: (h: (typeof delegation)[number]) => { delegation.push(h); return () => delegation.splice(delegation.indexOf(h), 1); },
  } as unknown as VotingAdapter;
  return {
    adapter,
    listTier2Proposals,
    getMyDelegate,
    setList: (l: Tier2ProposalExport[]) => { listResult = l; },
    emitCreated: (p: { proposalId: string; communityId: string }) => [...created].forEach((h) => h(p)),
    emitSignalCast: (p: { proposalId: string; voter: string; support: boolean }) => [...signalCast].forEach((h) => h(p)),
    emitDelegationChanged: (p: { communityId: string; delegator: string; delegate: string | null }) => [...delegation].forEach((h) => h(p)),
    handlerCount: () =>
      created.length + reached.length + reverted.length + finalized.length + signalCast.length + delegation.length,
  };
}

const MY_ADDR = 'me'.repeat(16);
const OTHER_ADDR = 'ee'.repeat(16);

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
    await waitFor(() => expect(mock.handlerCount()).toBe(6));
    unmount();
    expect(mock.handlerCount()).toBe(0);
  });

  // PR #408 Greptile P1: remote casts refresh totals; own casts stay
  // optimistic-only; casts on unlisted proposals are out of scope.
  it('refetches on a remote signal-cast for a listed proposal only', async () => {
    const mock = makeVotingMock([makeProposal({ proposal_id: 'p1', proposal_text: 'Listed one' })]);
    render(AssemblyRail, { props: { communityId: 'c1', adapter: mock.adapter, myAddr: MY_ADDR } });
    await waitFor(() => expect(screen.getByText('Listed one')).toBeTruthy());
    expect(mock.listTier2Proposals).toHaveBeenCalledTimes(1);
    // Own cast → no refetch (card handles its own optimistic state).
    mock.emitSignalCast({ proposalId: 'p1', voter: MY_ADDR, support: true });
    await new Promise((r) => setTimeout(r, 0));
    expect(mock.listTier2Proposals).toHaveBeenCalledTimes(1);
    // Cast on a proposal we don't list → no refetch.
    mock.emitSignalCast({ proposalId: 'p-unknown', voter: OTHER_ADDR, support: true });
    await new Promise((r) => setTimeout(r, 0));
    expect(mock.listTier2Proposals).toHaveBeenCalledTimes(1);
    // Remote cast on a listed proposal → refetch.
    mock.emitSignalCast({ proposalId: 'p1', voter: OTHER_ADDR, support: true });
    await waitFor(() => expect(mock.listTier2Proposals).toHaveBeenCalledTimes(2));
  });

  // PR #408 Greptile P1: delegate context reaches the rail cards so a
  // delegated voter sees the same override affordance as the full view.
  it('threads the delegate into cards and re-reads it on delegation-changed', async () => {
    const mock = makeVotingMock([makeProposal({ proposal_id: 'p1', proposal_text: 'Delegated one' })]);
    mock.getMyDelegate.mockResolvedValue(OTHER_ADDR);
    render(AssemblyRail, {
      props: {
        communityId: 'c1',
        adapter: mock.adapter,
        myAddr: MY_ADDR,
        communityMembers: [
          { address: OTHER_ADDR, displayName: 'Devin Ross', power: 1, status: 'joined' },
        ],
      },
    });
    await waitFor(() => expect(screen.getByText('Devin Ross')).toBeTruthy());
    expect(mock.getMyDelegate).toHaveBeenCalledTimes(1);
    // Someone ELSE's delegation change is ignored…
    mock.emitDelegationChanged({ communityId: 'c1', delegator: OTHER_ADDR, delegate: null });
    await new Promise((r) => setTimeout(r, 0));
    expect(mock.getMyDelegate).toHaveBeenCalledTimes(1);
    // …but my own re-reads the delegate.
    mock.emitDelegationChanged({ communityId: 'c1', delegator: MY_ADDR, delegate: null });
    await waitFor(() => expect(mock.getMyDelegate).toHaveBeenCalledTimes(2));
  });
});
