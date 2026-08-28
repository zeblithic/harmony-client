import { describe, it, expect, vi } from 'vitest';
import { VotingAdapter } from '../voting-adapter';
import type { TauriAdapter } from '../zenoh-service';
import type {
  VotingTier3PollCreatedPayload,
  VotingTier3SortitionCompletePayload,
  VotingTier3DraftingOpenPayload,
  VotingTier3RatificationOpenPayload,
  VotingTier3FinalizedPayload,
} from '../types/voting';

function makeMockAdapter(): {
  adapter: TauriAdapter;
  invoke: ReturnType<typeof vi.fn>;
  emit: (event: string, payload: unknown) => void;
} {
  const listeners = new Map<string, Array<(e: { payload: unknown }) => void>>();
  const invoke = vi.fn();
  const adapter: TauriAdapter = {
    invoke,
    listen: async (event, handler) => {
      const list = listeners.get(event) ?? [];
      list.push(handler as (e: { payload: unknown }) => void);
      listeners.set(event, list);
      return () => {
        const cur = listeners.get(event);
        if (cur) {
          const i = cur.indexOf(handler as (e: { payload: unknown }) => void);
          if (i >= 0) cur.splice(i, 1);
        }
      };
    },
  };
  return {
    adapter,
    invoke,
    emit: (event, payload) => {
      const list = listeners.get(event) ?? [];
      for (const h of list) h({ payload });
    },
  };
}

describe('VotingAdapter Tier 3 IPC wrappers', () => {
  it('createTier3Proposal invokes with camelCase params', async () => {
    const { adapter, invoke } = makeMockAdapter();
    invoke.mockResolvedValue('aa'.repeat(32));
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const pid = await va.createTier3Proposal({
      communityId: 'bb'.repeat(16),
      channelId: 'cc'.repeat(16),
      proposalText: 'test',
      sortitionSize: 20,
      deliberationWindowSeconds: 600,
      draftingWindowSeconds: 600,
      ratificationWindowSeconds: 600,
      incentiveMode: 'd',
      minPower: 0,
    });
    expect(pid).toBe('aa'.repeat(32));
    expect(invoke).toHaveBeenCalledWith(
      'voting_create_tier3_proposal',
      expect.objectContaining({
        communityId: 'bb'.repeat(16),
        sortitionSize: 20,
        incentiveMode: 'd',
      }),
    );
  });

  it('submitDeliberationStatement returns event_hash', async () => {
    const { adapter, invoke } = makeMockAdapter();
    invoke.mockResolvedValue('dd'.repeat(32));
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const eh = await va.submitDeliberationStatement('aa'.repeat(32), 'my view');
    expect(eh).toBe('dd'.repeat(32));
  });

  it('proposeDraftCandidate returns candidate_event_hash', async () => {
    const { adapter, invoke } = makeMockAdapter();
    invoke.mockResolvedValue('ee'.repeat(32));
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const eh = await va.proposeDraftCandidate('aa'.repeat(32), 'option A');
    expect(eh).toBe('ee'.repeat(32));
  });

  it('approveDraftCandidate returns void', async () => {
    const { adapter, invoke } = makeMockAdapter();
    invoke.mockResolvedValue(null);
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const r = await va.approveDraftCandidate('aa'.repeat(32), 'bb'.repeat(32));
    expect(r).toBeUndefined();
  });

  it('declineSortition passes optional reason', async () => {
    const { adapter, invoke } = makeMockAdapter();
    invoke.mockResolvedValue(null);
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    await va.declineSortition('aa'.repeat(32), 'u1');
    expect(invoke).toHaveBeenCalledWith('voting_decline_sortition', {
      pollId: 'aa'.repeat(32),
      reason: 'u1',
    });
  });

  it('castRatificationBallot sends scores array', async () => {
    const { adapter, invoke } = makeMockAdapter();
    invoke.mockResolvedValue(null);
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    await va.castRatificationBallot('aa'.repeat(32), [5, 4, 0, 3]);
    expect(invoke).toHaveBeenCalledWith('voting_cast_ratification_ballot', {
      pollId: 'aa'.repeat(32),
      scores: [5, 4, 0, 3],
    });
  });

  it('extracts string rejection error with command prefix', async () => {
    const { adapter, invoke } = makeMockAdapter();
    invoke.mockRejectedValue('eligibility failed');
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    await expect(va.declineSortition('aa'.repeat(32))).rejects.toThrow(
      /voting_decline_sortition failed: eligibility failed/,
    );
  });

  it('extracts Error.message with command prefix', async () => {
    const { adapter, invoke } = makeMockAdapter();
    invoke.mockRejectedValue(new Error('Error: snapshot missing'));
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    await expect(va.proposeDraftCandidate('aa'.repeat(32), 'x')).rejects.toThrow(
      /voting_propose_draft_candidate failed: Error: snapshot missing/,
    );
  });
});

describe('VotingAdapter Tier 3 event subscribers', () => {
  it('subscribeTier3PollCreated receives payload', async () => {
    const { adapter, emit } = makeMockAdapter();
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const seen: VotingTier3PollCreatedPayload[] = [];
    va.subscribeTier3PollCreated((p) => seen.push(p));
    emit('voting-tier3-poll-created', {
      pollId: 'aa'.repeat(32),
      channelId: 'bb'.repeat(16),
      communityId: 'cc'.repeat(16),
      proposer: 'dd'.repeat(16),
      sortitionSize: 20,
      deliberationWindowSeconds: 600,
      draftingWindowSeconds: 600,
      ratificationWindowSeconds: 600,
    });
    expect(seen).toHaveLength(1);
    expect(seen[0]?.sortitionSize).toBe(20);
  });

  it('subscribeTier3SortitionComplete fires once and unsubscribe removes', async () => {
    const { adapter, emit } = makeMockAdapter();
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const seen: VotingTier3SortitionCompletePayload[] = [];
    const unsub = va.subscribeTier3SortitionComplete((p) => seen.push(p));
    emit('voting-tier3-sortition-complete', {
      pollId: 'aa'.repeat(32),
      communityId: 'bb'.repeat(16),
      primary: [],
      backup: [],
    });
    unsub();
    emit('voting-tier3-sortition-complete', {
      pollId: 'aa'.repeat(32),
      communityId: 'bb'.repeat(16),
      primary: ['x'],
      backup: ['y'],
    });
    expect(seen).toHaveLength(1);
  });

  it('subscribeTier3DraftingOpen receives transition', async () => {
    const { adapter, emit } = makeMockAdapter();
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const seen: VotingTier3DraftingOpenPayload[] = [];
    va.subscribeTier3DraftingOpen((p) => seen.push(p));
    emit('voting-tier3-drafting-open', {
      pollId: 'aa'.repeat(32),
      communityId: 'bb'.repeat(16),
    });
    expect(seen).toHaveLength(1);
  });

  it('subscribeTier3RatificationOpen exposes candidate ordering', async () => {
    const { adapter, emit } = makeMockAdapter();
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const seen: VotingTier3RatificationOpenPayload[] = [];
    va.subscribeTier3RatificationOpen((p) => seen.push(p));
    emit('voting-tier3-ratification-open', {
      pollId: 'aa'.repeat(32),
      communityId: 'bb'.repeat(16),
      candidateOrdering: [
        { eventHash: 'cc'.repeat(32), text: 'A', approvalCount: 3 },
        { eventHash: 'dd'.repeat(32), text: 'B', approvalCount: 1 },
      ],
    });
    expect(seen[0]?.candidateOrdering).toHaveLength(2);
  });

  it('subscribeTier3Finalized exposes scoresSummary', async () => {
    const { adapter, emit } = makeMockAdapter();
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const seen: VotingTier3FinalizedPayload[] = [];
    va.subscribeTier3Finalized((p) => seen.push(p));
    emit('voting-tier3-finalized', {
      pollId: 'aa'.repeat(32),
      communityId: 'bb'.repeat(16),
      winnerEventHash: 'cc'.repeat(32),
      winnerText: 'Winner!',
      scoresSummary: [{ eventHash: 'cc'.repeat(32), totalScore: 10, runoffVotes: 5 }],
    });
    expect(seen[0]?.winnerText).toBe('Winner!');
  });
});

describe('VotingAdapter.getTier3Poll', () => {
  it('invokes voting_get_tier3_poll with the camelCased pollId', async () => {
    const { adapter, invoke } = makeMockAdapter();
    const a = new VotingAdapter();
    await a.connectAdapter(adapter);
    invoke.mockResolvedValueOnce({ pollId: 'aa', stage: 'so' });
    await a.getTier3Poll('aa');
    expect(invoke).toHaveBeenCalledWith('voting_get_tier3_poll', { pollId: 'aa' });
  });
});

describe('VotingAdapter.listTier3Polls', () => {
  it('invokes voting_list_tier3_polls with the camelCased communityId', async () => {
    const { adapter, invoke } = makeMockAdapter();
    const a = new VotingAdapter();
    await a.connectAdapter(adapter);
    invoke.mockResolvedValueOnce([]);
    await a.listTier3Polls('11');
    expect(invoke).toHaveBeenCalledWith('voting_list_tier3_polls', { communityId: '11' });
  });
});

// ── ZEB-1018: D-FROST ceremony event subscribers ────────────────────────────

describe('VotingAdapter D-FROST event subscribers', () => {
  it('delivers dfrost-dkg-progress payloads to subscribers', async () => {
    const { adapter, emit } = makeMockAdapter();
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const seen: Array<{ communityId: string; roundNum: number }> = [];
    va.subscribeDfrostDkgProgress((p) => seen.push(p));
    emit('dfrost-dkg-progress', {
      communityId: 'bb'.repeat(16),
      ceremonyId: 'aa'.repeat(32),
      roundNum: 2,
      participantsSoFar: 3,
    });
    expect(seen).toHaveLength(1);
    expect(seen[0]?.communityId).toBe('bb'.repeat(16));
    expect(seen[0]?.roundNum).toBe(2);
  });

  it('delivers dfrost-beacon-ready payloads to subscribers', async () => {
    const { adapter, emit } = makeMockAdapter();
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const seen: Array<{ vrfOutput: string }> = [];
    va.subscribeDfrostBeaconReady((p) => seen.push(p));
    emit('dfrost-beacon-ready', {
      communityId: 'bb'.repeat(16),
      ceremonyId: 'aa'.repeat(32),
      vrfOutput: 'cd'.repeat(32),
    });
    expect(seen[0]?.vrfOutput).toBe('cd'.repeat(32));
  });

  it('delivers dfrost-refresh-progress payloads and honors unsubscribe', async () => {
    const { adapter, emit } = makeMockAdapter();
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const seen: Array<{ roundNum: number }> = [];
    const unsub = va.subscribeDfrostRefreshProgress((p) => seen.push(p));
    emit('dfrost-refresh-progress', {
      communityId: 'bb'.repeat(16),
      ceremonyId: 'aa'.repeat(32),
      roundNum: 1,
    });
    expect(seen).toHaveLength(1);
    unsub();
    emit('dfrost-refresh-progress', {
      communityId: 'bb'.repeat(16),
      ceremonyId: 'aa'.repeat(32),
      roundNum: 2,
    });
    expect(seen).toHaveLength(1);
  });
});
