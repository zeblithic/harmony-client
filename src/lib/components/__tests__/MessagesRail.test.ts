import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import MessagesRail from '../MessagesRail.svelte';
import { TrustService } from '../../trust-service';
import type { VotingAdapter } from '../../voting-adapter';
import type { Message } from '../../types';

beforeEach(() => {
  localStorage.clear();
});

const noMessages: Message[] = [];

/** Minimal voting stub — the rail only needs list + the 4 subscribes. */
function makeVotingStub(): VotingAdapter {
  return {
    listTier2Proposals: vi.fn(async () => []),
    subscribeProposalCreated: () => () => {},
    subscribeThresholdReached: () => () => {},
    subscribeThresholdReverted: () => () => {},
    subscribeProposalFinalized: () => () => {},
  } as unknown as VotingAdapter;
}

function baseProps(overrides: Record<string, unknown> = {}) {
  return {
    communityId: 'c1',
    votingAdapter: makeVotingStub(),
    onViewAllProposals: vi.fn(),
    messages: noMessages,
    trustService: new TrustService(),
    ...overrides,
  };
}

describe('MessagesRail (ZEB-606)', () => {
  it('defaults to the Assembly tab when a community is active', async () => {
    render(MessagesRail, { props: baseProps() });
    expect(screen.getByRole('tab', { name: '⚖ Assembly' })).toBeTruthy();
    await waitFor(() => expect(screen.getByText('No open proposals')).toBeTruthy());
    expect(screen.queryByText('No media yet')).toBeNull();
  });

  it('switches to Media and persists the choice', async () => {
    render(MessagesRail, { props: baseProps() });
    await fireEvent.click(screen.getByRole('tab', { name: 'Media' }));
    expect(screen.getByText('No media yet')).toBeTruthy();
    expect(localStorage.getItem('harmony-rail-tab')).toBe('media');
  });

  it('honors a persisted media preference on mount', () => {
    localStorage.setItem('harmony-rail-tab', 'media');
    render(MessagesRail, { props: baseProps() });
    expect(screen.getByText('No media yet')).toBeTruthy();
  });

  it('renders media-only (no tabs) without a community', () => {
    render(MessagesRail, { props: baseProps({ communityId: null }) });
    expect(screen.queryByRole('tab', { name: '⚖ Assembly' })).toBeNull();
    expect(screen.getByText('No media yet')).toBeTruthy();
  });

  it('renders media-only (no tabs) without a votingAdapter', () => {
    render(MessagesRail, { props: baseProps({ votingAdapter: undefined }) });
    expect(screen.queryByRole('tab', { name: '⚖ Assembly' })).toBeNull();
    expect(screen.getByText('No media yet')).toBeTruthy();
  });

  it('routes View-all through onViewAllProposals with the community id', async () => {
    const onViewAllProposals = vi.fn();
    render(MessagesRail, { props: baseProps({ onViewAllProposals }) });
    await waitFor(() => expect(screen.getByText('View all proposals →')).toBeTruthy());
    await fireEvent.click(screen.getByText('View all proposals →'));
    expect(onViewAllProposals).toHaveBeenCalledWith('c1');
  });
});
