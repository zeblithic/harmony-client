import { render } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import Tier3LifecycleStatus from '../Tier3LifecycleStatus.svelte';
import type { Tier3PollSummary } from '../../types/voting';

const baseSummary: Tier3PollSummary = {
  pollId: 'aa'.repeat(32),
  communityId: '11'.repeat(16),
  proposalText: 'Amend §3',
  proposer: '22'.repeat(32),
  stage: 'so',
  pollCreateHlcMs: 1_700_000_000_000,
  sortitionSize: 100,
  winnerText: null,
  privacyMode: 'pu',
};

describe('Tier3LifecycleStatus', () => {
  it('renders all four stage chips with current stage highlighted', () => {
    const { container, getByText } = render(Tier3LifecycleStatus, {
      props: { summary: { ...baseSummary, stage: 'dr' } },
    });
    expect(getByText('Sortition')).toBeTruthy();
    expect(getByText('Deliberation')).toBeTruthy();
    expect(getByText('Drafting')).toBeTruthy();
    expect(getByText('Ratification')).toBeTruthy();
    const current = container.querySelector('.stage-chip.current');
    expect(current?.textContent).toContain('Drafting');
  });

  it('renders a failed badge when stage is fa', () => {
    const { getByText } = render(Tier3LifecycleStatus, {
      props: { summary: { ...baseSummary, stage: 'fa' } },
    });
    expect(getByText(/sortition failed/i)).toBeTruthy();
  });

  it('renders the winner text when stage is fi', () => {
    const { getByText } = render(Tier3LifecycleStatus, {
      props: { summary: { ...baseSummary, stage: 'fi', winnerText: 'Charter amended' } },
    });
    expect(getByText('Charter amended')).toBeTruthy();
  });

  // ── ZEB-1031 §7/§9: voided-poll badge ───────────────────────────────
  it('renders a Voided badge — taking priority over the stage chips — when voided is set', () => {
    const { getByText, queryByText } = render(Tier3LifecycleStatus, {
      props: {
        summary: {
          ...baseSummary,
          stage: 'dr',
          voided: { resetId: 'aa'.repeat(16), oldEpoch: 3 },
        },
      },
    });
    expect(getByText(/Voided/)).toBeTruthy();
    expect(queryByText('Drafting')).toBeNull();
  });

  it('does not render the Voided badge when voided is absent', () => {
    const { queryByText } = render(Tier3LifecycleStatus, {
      props: { summary: { ...baseSummary, stage: 'dr' } },
    });
    expect(queryByText(/Voided/)).toBeNull();
  });
});
