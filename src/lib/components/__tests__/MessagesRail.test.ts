import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import MessagesRail from '../MessagesRail.svelte';
import type { VotingAdapter } from '../../voting-adapter';

/** Minimal voting stub — the rail needs list/delegate reads + the 6 subscribes. */
function makeVotingStub(): VotingAdapter {
  return {
    listTier2Proposals: vi.fn(async () => []),
    getMyDelegate: vi.fn(async () => null),
    subscribeProposalCreated: () => () => {},
    subscribeThresholdReached: () => () => {},
    subscribeThresholdReverted: () => () => {},
    subscribeProposalFinalized: () => () => {},
    subscribeSignalCast: () => () => {},
    subscribeDelegationChanged: () => () => {},
  } as unknown as VotingAdapter;
}

function baseProps(overrides: Record<string, unknown> = {}) {
  return {
    communityId: 'c1',
    votingAdapter: makeVotingStub(),
    onViewAllProposals: vi.fn(),
    ...overrides,
  };
}

describe('MessagesRail (ZEB-606)', () => {
  it('renders AssemblyRail when a community and voting adapter are active', async () => {
    render(MessagesRail, { props: baseProps() });
    await waitFor(() => expect(screen.getByText('No open proposals')).toBeTruthy());
  });

  it('renders nothing without a community', () => {
    const { container } = render(MessagesRail, { props: baseProps({ communityId: null }) });
    expect(container.textContent?.trim()).toBe('');
  });

  it('renders nothing without a votingAdapter', () => {
    const { container } = render(MessagesRail, { props: baseProps({ votingAdapter: undefined }) });
    expect(container.textContent?.trim()).toBe('');
  });

  it('routes View-all through onViewAllProposals with the community id', async () => {
    const onViewAllProposals = vi.fn();
    render(MessagesRail, { props: baseProps({ onViewAllProposals }) });
    await waitFor(() => expect(screen.getByText('View all proposals →')).toBeTruthy());
    await fireEvent.click(screen.getByText('View all proposals →'));
    expect(onViewAllProposals).toHaveBeenCalledWith('c1');
  });
});
