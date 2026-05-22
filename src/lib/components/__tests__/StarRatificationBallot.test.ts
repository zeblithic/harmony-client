import { render, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import StarRatificationBallot from '../StarRatificationBallot.svelte';
import { VotingAdapter } from '../../voting-adapter';
import type { Tier3PollExport } from '../../types/voting';

const detail: Tier3PollExport = {
  pollId: 'aa'.repeat(32),
  communityId: '11'.repeat(16),
  proposalText: 'Amend §3',
  proposer: 'pp'.repeat(32),
  stage: 'ra',
  pollCreateHlcMs: 1_700_000_000_000,
  sortitionSize: 100,
  deliberationWindowSeconds: 1_209_600,
  draftingWindowSeconds: 604_800,
  ratificationWindowSeconds: 1_209_600,
  incentiveMode: 'd',
  miniPublic: ['mm'.repeat(32)],
  backupPool: [],
  declined: [],
  draftCandidates: [],
  ratificationCandidates: [
    { eventHash: 'aa'.repeat(32), text: 'Candidate A' },
    { eventHash: 'bb'.repeat(32), text: 'Candidate B' },
  ],
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

describe('StarRatificationBallot', () => {
  it('renders one slider per ratification candidate', () => {
    const adapter = new VotingAdapter();
    const { getAllByRole } = render(StarRatificationBallot, {
      props: { detail, adapter, onCast: () => {} },
    });
    const sliders = getAllByRole('slider');
    expect(sliders).toHaveLength(2);
  });

  it('seeds slider + number inputs to 0 synchronously on first render', () => {
    // Regression: scores were previously initialized as `[]` and populated by
    // $effect, leaving scores[i] === undefined during the first paint. Now
    // seeded synchronously via $state initializer.
    const adapter = new VotingAdapter();
    const { getAllByRole, container } = render(StarRatificationBallot, {
      props: { detail, adapter, onCast: () => {} },
    });
    const sliders = getAllByRole('slider') as HTMLInputElement[];
    expect(sliders[0].value).toBe('0');
    expect(sliders[1].value).toBe('0');
    const numberInputs = container.querySelectorAll('input[type="number"]') as NodeListOf<HTMLInputElement>;
    expect(numberInputs).toHaveLength(2);
    expect(numberInputs[0].value).toBe('0');
    expect(numberInputs[1].value).toBe('0');
  });

  it('cast button opens confirm modal before invoking', async () => {
    const adapter = new VotingAdapter();
    vi.spyOn(adapter, 'castRatificationBallot').mockResolvedValue();
    const { getByText, findByText } = render(StarRatificationBallot, {
      props: { detail, adapter, onCast: () => {} },
    });
    await fireEvent.click(getByText(/Cast ballot/i));
    expect(await findByText(/Confirm ratification ballot/i)).toBeTruthy();
    expect(adapter.castRatificationBallot).not.toHaveBeenCalled();
    await fireEvent.click(await findByText(/^Confirm$/i));
    await waitFor(() => expect(adapter.castRatificationBallot).toHaveBeenCalledWith(detail.pollId, [0, 0]));
  });

  it('prefills sliders with myRatificationScores when present', () => {
    const adapter = new VotingAdapter();
    const { getAllByRole } = render(StarRatificationBallot, {
      props: { detail: { ...detail, myRatificationScores: [3, 5] }, adapter, onCast: () => {} },
    });
    const sliders = getAllByRole('slider') as HTMLInputElement[];
    expect(sliders[0].value).toBe('3');
    expect(sliders[1].value).toBe('5');
  });

  it('keeps slider + number input in sync when one is changed via setScore', async () => {
    // Regression: setScore previously wrote `scores[index] = clamped` in
    // place. In Svelte 5 $state arrays this should reactively update both
    // paired inputs, but reassigning the array is the load-bearing pattern
    // that guarantees re-render across Svelte 5 minor versions.
    const adapter = new VotingAdapter();
    const { getAllByRole, container } = render(StarRatificationBallot, {
      props: { detail, adapter, onCast: () => {} },
    });
    const sliders = getAllByRole('slider') as HTMLInputElement[];
    const numberInputs = container.querySelectorAll('input[type="number"]') as NodeListOf<HTMLInputElement>;

    // Change the FIRST slider to 4 → number input for index 0 must follow.
    await fireEvent.input(sliders[0], { target: { value: '4' } });
    expect(sliders[0].value).toBe('4');
    expect(numberInputs[0].value).toBe('4');
    // The second pair must remain at 0.
    expect(sliders[1].value).toBe('0');
    expect(numberInputs[1].value).toBe('0');

    // Now change the SECOND number input to 2 → slider for index 1 must follow.
    await fireEvent.input(numberInputs[1], { target: { value: '2' } });
    expect(numberInputs[1].value).toBe('2');
    expect(sliders[1].value).toBe('2');
    // First pair unchanged.
    expect(sliders[0].value).toBe('4');
    expect(numberInputs[0].value).toBe('4');
  });
});
