import { describe, expect, it, vi } from 'vitest';
import { ProposalCountService } from './proposal-count-service';
import type { VotingAdapter } from './voting-adapter';
import type { Tier2ProposalExport } from './types/voting';

/** Snake_case Tier-2 fixture (wire-realistic — see types/voting.ts:224). */
function makeProposal(overrides: Partial<Tier2ProposalExport> = {}): Tier2ProposalExport {
  return {
    proposal_id: 'aa'.repeat(32),
    community_id: 'c1',
    proposal_text: 'Fix the fountain',
    lifecycle: 'Open',
    total_conviction_ms: '0',
    threshold_conviction_ms: '1000',
    half_life_seconds: 3600,
    auto_exec: { kk: 'n' },
    total_supply: 10,
    voter_count: 0,
    ...overrides,
  };
}

/** Multi-handler mock — VotingAdapter is multi-subscriber, so the shared
 *  createMockAdapter (single handler slot per event) is insufficient here.
 *  Mirrors the inline makeMockAdapter idiom in voting-adapter-tier3.test.ts. */
function makeVotingMock() {
  const created: Array<(p: { proposalId: string; communityId: string }) => void> = [];
  const reached: Array<(p: { communityId: string; proposalId: string; thresholdReachedAtMs: number }) => void> = [];
  const reverted: Array<(p: { communityId: string; proposalId: string; revertedAtMs: number }) => void> = [];
  const finalized: Array<(p: { communityId: string; proposalId: string }) => void> = [];
  const listTier2Proposals = vi.fn(async (_cid: string): Promise<Tier2ProposalExport[]> => []);
  const adapter = {
    listTier2Proposals,
    subscribeProposalCreated: (h: (typeof created)[number]) => {
      created.push(h);
      return () => created.splice(created.indexOf(h), 1);
    },
    subscribeThresholdReached: (h: (typeof reached)[number]) => {
      reached.push(h);
      return () => reached.splice(reached.indexOf(h), 1);
    },
    subscribeThresholdReverted: (h: (typeof reverted)[number]) => {
      reverted.push(h);
      return () => reverted.splice(reverted.indexOf(h), 1);
    },
    subscribeProposalFinalized: (h: (typeof finalized)[number]) => {
      finalized.push(h);
      return () => finalized.splice(finalized.indexOf(h), 1);
    },
  } as unknown as VotingAdapter;
  return {
    adapter,
    listTier2Proposals,
    emitCreated: (p: { proposalId: string; communityId: string }) => [...created].forEach((h) => h(p)),
    emitFinalized: (p: { communityId: string; proposalId: string }) => [...finalized].forEach((h) => h(p)),
    counts: { created, reached, reverted, finalized },
  };
}

/** Flush pending microtasks (service refetches are fire-and-forget). */
const flush = () => new Promise<void>((r) => setTimeout(r, 0));

describe('ProposalCountService', () => {
  it('ensure() lazily fetches and counts only Open + ThresholdReached', async () => {
    const { adapter, listTier2Proposals } = makeVotingMock();
    listTier2Proposals.mockResolvedValue([
      makeProposal({ proposal_id: 'p1', lifecycle: 'Open' }),
      makeProposal({ proposal_id: 'p2', lifecycle: 'ThresholdReached' }),
      makeProposal({ proposal_id: 'p3', lifecycle: 'Finalized' }),
      makeProposal({ proposal_id: 'p4', lifecycle: 'Archived' }),
    ]);
    const svc = new ProposalCountService();
    svc.connectAdapter(adapter);
    expect(svc.countFor('c1')).toBeUndefined();
    svc.ensure('c1');
    await flush();
    expect(svc.countFor('c1')).toBe(2);
    expect(listTier2Proposals).toHaveBeenCalledTimes(1);
  });

  it('ensure() is idempotent (no duplicate IPC while loaded or loading)', async () => {
    const { adapter, listTier2Proposals } = makeVotingMock();
    const svc = new ProposalCountService();
    svc.connectAdapter(adapter);
    svc.ensure('c1');
    svc.ensure('c1'); // in-flight
    await flush();
    svc.ensure('c1'); // loaded
    await flush();
    expect(listTier2Proposals).toHaveBeenCalledTimes(1);
  });

  it('lifecycle events refetch the affected community and fire onChange', async () => {
    const { adapter, listTier2Proposals, emitCreated } = makeVotingMock();
    listTier2Proposals.mockResolvedValue([makeProposal({ lifecycle: 'Open' })]);
    const svc = new ProposalCountService();
    svc.connectAdapter(adapter);
    const onChange = vi.fn();
    svc.onChange = onChange;
    svc.ensure('c1');
    await flush();
    expect(svc.countFor('c1')).toBe(1);
    const v0 = svc.version;
    listTier2Proposals.mockResolvedValue([
      makeProposal({ proposal_id: 'p1', lifecycle: 'Open' }),
      makeProposal({ proposal_id: 'p2', lifecycle: 'Open' }),
    ]);
    emitCreated({ proposalId: 'p2', communityId: 'c1' });
    await flush();
    expect(svc.countFor('c1')).toBe(2);
    expect(svc.version).toBeGreaterThan(v0);
    expect(onChange).toHaveBeenCalled();
  });

  it('a stale slow fetch cannot clobber a newer event-driven refetch', async () => {
    const { adapter, listTier2Proposals, emitFinalized } = makeVotingMock();
    let releaseFirst!: (v: Tier2ProposalExport[]) => void;
    const first = new Promise<Tier2ProposalExport[]>((r) => (releaseFirst = r));
    listTier2Proposals.mockReturnValueOnce(first); // slow initial load
    const svc = new ProposalCountService();
    svc.connectAdapter(adapter);
    svc.ensure('c1');
    // Event fires while the first fetch hangs; its refetch resolves first.
    listTier2Proposals.mockResolvedValue([makeProposal({ lifecycle: 'Open' })]);
    emitFinalized({ communityId: 'c1', proposalId: 'p9' });
    await flush();
    expect(svc.countFor('c1')).toBe(1);
    // Now the stale first fetch lands with 3 actives — must be dropped.
    releaseFirst([
      makeProposal({ proposal_id: 'p1' }),
      makeProposal({ proposal_id: 'p2' }),
      makeProposal({ proposal_id: 'p3' }),
    ]);
    await flush();
    expect(svc.countFor('c1')).toBe(1);
  });

  it('fetch errors leave the count undefined and allow a later ensure() retry', async () => {
    const { adapter, listTier2Proposals } = makeVotingMock();
    listTier2Proposals.mockRejectedValueOnce('boom');
    const svc = new ProposalCountService();
    svc.connectAdapter(adapter);
    svc.ensure('c1');
    await flush();
    expect(svc.countFor('c1')).toBeUndefined();
    listTier2Proposals.mockResolvedValue([makeProposal()]);
    svc.ensure('c1'); // retry allowed after a failed first load
    await flush();
    expect(svc.countFor('c1')).toBe(1);
  });

  it('disconnect() unsubscribes all four event handlers', () => {
    const mock = makeVotingMock();
    const svc = new ProposalCountService();
    svc.connectAdapter(mock.adapter);
    expect(mock.counts.created.length + mock.counts.reached.length + mock.counts.reverted.length + mock.counts.finalized.length).toBe(4);
    svc.disconnect();
    expect(mock.counts.created.length + mock.counts.reached.length + mock.counts.reverted.length + mock.counts.finalized.length).toBe(0);
  });
});
