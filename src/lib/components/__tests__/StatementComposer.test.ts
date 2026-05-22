import { render, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import StatementComposer from '../StatementComposer.svelte';
import { VotingAdapter } from '../../voting-adapter';
import type { Tier3PollExport } from '../../types/voting';

const baseDetail: Tier3PollExport = {
  pollId: 'aa'.repeat(32),
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
  deliberationStatements: [],
  myDeliberationStatementCount: 0,
  myDeliberationVotes: [],
  winnerEventHash: null,
  runnerUpEventHash: null,
};

describe('StatementComposer', () => {
  it('shows 5-cap warning when myDeliberationStatementCount === 5', () => {
    const adapter = new VotingAdapter();
    const { getByText, queryByPlaceholderText } = render(StatementComposer, {
      props: {
        detail: { ...baseDetail, myDeliberationStatementCount: 5 },
        adapter,
        onChange: () => {},
      },
    });
    expect(getByText(/used all 5 statement slots/i)).toBeTruthy();
    expect(queryByPlaceholderText(/Up to 280 characters/i)).toBeNull();
  });

  it('submit button opens confirm modal before invoking', async () => {
    const adapter = new VotingAdapter();
    vi.spyOn(adapter, 'submitDeliberationStatement').mockResolvedValue('hash');
    const { getByPlaceholderText, getByText, findByText } = render(StatementComposer, {
      props: { detail: baseDetail, adapter, onChange: () => {} },
    });
    await fireEvent.input(getByPlaceholderText(/Up to 280 characters/i), { target: { value: 'Hello' } });
    await fireEvent.click(getByText(/^Submit$/));
    expect(await findByText(/Confirm statement submission/i)).toBeTruthy();
    expect(adapter.submitDeliberationStatement).not.toHaveBeenCalled();
    await fireEvent.click(await findByText(/^Confirm$/));
    await waitFor(() =>
      expect(adapter.submitDeliberationStatement).toHaveBeenCalledWith('aa'.repeat(32), 'Hello'),
    );
  });

  it('disables submit when stage is not de', async () => {
    const adapter = new VotingAdapter();
    const { getByPlaceholderText, getByText } = render(StatementComposer, {
      props: { detail: { ...baseDetail, stage: 'dr' }, adapter, onChange: () => {} },
    });
    await fireEvent.input(getByPlaceholderText(/Up to 280 characters/i), { target: { value: 'x' } });
    expect((getByText(/^Submit$/).closest('button') as HTMLButtonElement).disabled).toBe(true);
  });

  it('disables submit on whitespace-only text', async () => {
    const adapter = new VotingAdapter();
    const { getByPlaceholderText, getByText } = render(StatementComposer, {
      props: { detail: baseDetail, adapter, onChange: () => {} },
    });
    await fireEvent.input(getByPlaceholderText(/Up to 280 characters/i), { target: { value: '   \t  ' } });
    expect((getByText(/^Submit$/).closest('button') as HTMLButtonElement).disabled).toBe(true);
  });

  it('counts Unicode scalar values (not UTF-16 code units) for charsRemaining', async () => {
    // Cursor Bugbot pass-2 fix: emoji and other supplementary-plane chars
    // must count as 1 to match Rust `chars().count()`. A string of 200
    // 🙂 characters is 400 UTF-16 code units but only 200 Unicode scalars,
    // which is well under the 280 limit; charsRemaining must reflect that.
    const adapter = new VotingAdapter();
    const { getByPlaceholderText, getByText } = render(StatementComposer, {
      props: { detail: baseDetail, adapter, onChange: () => {} },
    });
    const emoji200 = '🙂'.repeat(200);
    await fireEvent.input(getByPlaceholderText(/Up to 280 characters/i), { target: { value: emoji200 } });
    // 280 - 200 = 80 remaining. If the old `text.length` logic were still
    // in play, this would read `-120 chars left` and disable Submit.
    expect(getByText(/80 chars left/)).toBeTruthy();
    expect((getByText(/^Submit$/).closest('button') as HTMLButtonElement).disabled).toBe(false);
  });
});
