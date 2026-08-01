import { render, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import StatementVoteList from '../StatementVoteList.svelte';
import { VotingAdapter } from '../../voting-adapter';
import type { Tier3PollExport, DeliberationStatementExport } from '../../types/voting';

const stmt: DeliberationStatementExport = {
  statementEventHash: 'aa'.repeat(32),
  author: '33'.repeat(32),
  text: 'A bridging idea',
  createdAtHlcMs: 1_700_000_010_000,
  agreeCount: 0,
  disagreeCount: 0,
  passCount: 0,
};

const baseDetail: Tier3PollExport = {
  pollId: 'bb'.repeat(32),
  communityId: '11'.repeat(16),
  proposalText: 'Test',
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
  deliberationStatements: [stmt],
  myDeliberationStatementCount: 0,
  myDeliberationVotes: [],
  winnerEventHash: null,
  runnerUpEventHash: null,
  privacyMode: 'pu',
  encryptedTallyShareCount: 0,
  encryptedTallyThreshold: 0,
  encryptedTallyCommitteeSize: 0,
};

// myAddr is intentionally different from `stmt.author` so the tri-button
// renders by default. Self-vote suppression is covered by a dedicated test
// below.
const otherAddr = '44'.repeat(32);

describe('StatementVoteList', () => {
  it('renders tri-button for mini-public (statement from another member)', () => {
    const adapter = new VotingAdapter();
    const { getByText } = render(StatementVoteList, {
      props: { detail: baseDetail, adapter, myAddr: otherAddr, onChange: () => {} },
    });
    expect(getByText(/👍 Agree/)).toBeTruthy();
    expect(getByText(/👎 Disagree/)).toBeTruthy();
    expect(getByText(/⊘ Pass/)).toBeTruthy();
  });

  it('renders read-only chips for observer', () => {
    const adapter = new VotingAdapter();
    const { queryByText, getByText } = render(StatementVoteList, {
      props: {
        detail: { ...baseDetail, myRole: 'observer' },
        adapter, myAddr: 'zz'.repeat(32), onChange: () => {},
      },
    });
    expect(queryByText(/👍 Agree/)).toBeNull();
    expect(getByText(/👍 0/)).toBeTruthy();
  });

  it('casts vote via adapter when tri-button clicked', async () => {
    const adapter = new VotingAdapter();
    vi.spyOn(adapter, 'castDeliberationVote').mockResolvedValue();
    const { getByText } = render(StatementVoteList, {
      props: { detail: baseDetail, adapter, myAddr: otherAddr, onChange: () => {} },
    });
    await fireEvent.click(getByText(/👍 Agree/));
    await waitFor(() =>
      expect(adapter.castDeliberationVote).toHaveBeenCalledWith(
        'bb'.repeat(32), 'aa'.repeat(32), 'agree',
      ),
    );
  });

  it('filter "Unvoted by me" hides statements I have voted on', async () => {
    const adapter = new VotingAdapter();
    const { queryByText } = render(StatementVoteList, {
      props: {
        detail: {
          ...baseDetail,
          myDeliberationVotes: [{ statementEventHash: stmt.statementEventHash, vote: 'agree' }],
        },
        adapter,
        myAddr: otherAddr,
        onChange: () => {},
      },
    });
    // Statement is voted-on, filter is default-on for mini-public → hidden.
    expect(queryByText('A bridging idea')).toBeNull();
  });

  it('renders the 3-bucket tally bar with per-bucket widths when counts are non-zero', () => {
    // ZEB-648 item 2: the observer read-only branch renders TallyBar only
    // when agree+disagree+pass > 0. Every other fixture is 0-count, so this
    // net-new ZEB-607 render path was previously uncovered.
    const adapter = new VotingAdapter();
    const tallied: DeliberationStatementExport = {
      ...stmt,
      agreeCount: 3,
      disagreeCount: 1,
      passCount: 0,
    };
    const { container, getByLabelText } = render(StatementVoteList, {
      props: {
        detail: { ...baseDetail, myRole: 'observer', deliberationStatements: [tallied] },
        adapter,
        myAddr: 'zz'.repeat(32),
        onChange: () => {},
      },
    });
    const fills = container.querySelectorAll('.tally-fill');
    expect(fills.length).toBe(3);
    // total = 4 → 75% agree / 25% disagree / 0% pass.
    expect((fills[0] as HTMLElement).style.width).toBe('75%');
    expect((fills[1] as HTMLElement).style.width).toBe('25%');
    expect((fills[2] as HTMLElement).style.width).toBe('0%');
    // aria-label carries the counts, not a bare "Statement votes".
    expect(getByLabelText('Statement votes: 3 agree, 1 disagree, 0 pass')).toBeTruthy();
  });

  it('orders same-ms statements by logical then deviceId (ZEB-790)', () => {
    const a: DeliberationStatementExport = {
      ...stmt, statementEventHash: 'cc'.repeat(32),
      text: 'second-by-logical', createdAtHlcMs: 1_700_000_020_000,
      createdAtHlcLogical: 2, createdAtHlcDeviceId: 'dev-a',
    };
    const b: DeliberationStatementExport = {
      ...stmt, statementEventHash: 'dd'.repeat(32),
      text: 'first-by-logical', createdAtHlcMs: 1_700_000_020_000,
      createdAtHlcLogical: 1, createdAtHlcDeviceId: 'dev-z',
    };
    const adapter = new VotingAdapter();
    const { container } = render(StatementVoteList, {
      props: {
        detail: { ...baseDetail, deliberationStatements: [a, b] },
        adapter, myAddr: otherAddr, onChange: () => {},
      },
    });
    const text = container.textContent ?? '';
    expect(text.indexOf('first-by-logical')).toBeGreaterThan(-1);
    expect(text.indexOf('first-by-logical')).toBeLessThan(text.indexOf('second-by-logical'));
  });

  it('breaks same-ms same-logical statements by deviceId lexically (ZEB-790)', () => {
    // Identical wall AND logical: only deviceId can order these — the final
    // compareHlc tuple component, which wall-only sorting would leave to
    // nondeterministic hash order.
    const a: DeliberationStatementExport = {
      ...stmt, statementEventHash: 'ee'.repeat(32),
      text: 'device-z-second', createdAtHlcMs: 1_700_000_030_000,
      createdAtHlcLogical: 5, createdAtHlcDeviceId: 'dev-z',
    };
    const b: DeliberationStatementExport = {
      ...stmt, statementEventHash: 'ff'.repeat(32),
      text: 'device-a-first', createdAtHlcMs: 1_700_000_030_000,
      createdAtHlcLogical: 5, createdAtHlcDeviceId: 'dev-a',
    };
    const adapter = new VotingAdapter();
    const { container } = render(StatementVoteList, {
      props: {
        detail: { ...baseDetail, deliberationStatements: [a, b] },
        adapter, myAddr: otherAddr, onChange: () => {},
      },
    });
    const text = container.textContent ?? '';
    expect(text.indexOf('device-a-first')).toBeGreaterThan(-1);
    expect(text.indexOf('device-a-first')).toBeLessThan(text.indexOf('device-z-second'));
  });

  it('hides tri-button on own statement and shows "yours" chip', () => {
    const adapter = new VotingAdapter();
    // myAddr === stmt.author → self-vote case (Greptile bot-pass 2 P2).
    const { queryByText, getByText } = render(StatementVoteList, {
      props: { detail: baseDetail, adapter, myAddr: stmt.author, onChange: () => {} },
    });
    // Tri-button must be suppressed (backend silently drops self-votes).
    expect(queryByText(/👍 Agree/)).toBeNull();
    // Read-only chips and a "yours" indicator must still render.
    expect(getByText(/👍 0/)).toBeTruthy();
    expect(getByText(/yours/i)).toBeTruthy();
  });
});
