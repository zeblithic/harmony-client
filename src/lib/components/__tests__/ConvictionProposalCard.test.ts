/**
 * ZEB-291 Phase 2 Task 26 — ConvictionProposalCard vitest coverage.
 *
 * Mirrors the PollMessage.test.ts pattern: patch the adapter's IPC
 * methods directly (no real Tauri adapter needed) and drive the
 * component through DOM events.
 */
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import ConvictionProposalCard from '../ConvictionProposalCard.svelte';
import { VotingAdapter } from '../../voting-adapter';
import type { Tier2ProposalExport } from '../../types/voting';

const COMMUNITY_ID = 'bb'.repeat(16); // 32 hex chars = 16 bytes
const PROPOSAL_ID = 'aa'.repeat(32); // 64 hex chars = 32 bytes

function makeProposal(overrides: Partial<Tier2ProposalExport> = {}): Tier2ProposalExport {
  return {
    proposal_id: PROPOSAL_ID,
    community_id: COMMUNITY_ID,
    proposal_text: 'Adopt the new logo design',
    lifecycle: 'Open',
    total_conviction_ms: 250,
    threshold_conviction_ms: 1000,
    half_life_seconds: 7 * 86400,
    auto_exec: { kk: 'n' },
    total_supply: 10,
    voter_count: 3,
    your_signal: undefined,
    threshold_reached_at_ms: undefined,
    ...overrides,
  };
}

describe('ConvictionProposalCard', () => {
  let adapter: VotingAdapter;
  let signalMock: ReturnType<typeof vi.fn<(pid: string, support: boolean) => Promise<void>>>;

  beforeEach(() => {
    adapter = new VotingAdapter();
    signalMock = vi
      .fn<(pid: string, support: boolean) => Promise<void>>()
      .mockResolvedValue(undefined);
    // Patch directly — the card never calls listTier2Proposals /
    // getTier2Proposal, only signalTier2.
    adapter.signalTier2 = signalMock;
  });

  it('renders the proposal text and lifecycle badge', () => {
    render(ConvictionProposalCard, {
      props: { communityId: COMMUNITY_ID, proposal: makeProposal(), adapter },
    });
    expect(screen.getByText('Adopt the new logo design')).toBeTruthy();
    // Lifecycle badge label for Open is just "Open".
    expect(screen.getByLabelText('Lifecycle').textContent).toBe('Open');
  });

  it('shows "Signal support" when the caller has not yet signaled', () => {
    render(ConvictionProposalCard, {
      props: {
        communityId: COMMUNITY_ID,
        proposal: makeProposal({ your_signal: undefined }),
        adapter,
      },
    });
    const btn = screen.getByRole('button');
    expect(btn.textContent).toContain('Signal support');
    expect((btn as HTMLButtonElement).getAttribute('aria-pressed')).toBe('false');
  });

  it('shows "Withdraw signal" when the caller is currently supporting', () => {
    render(ConvictionProposalCard, {
      props: {
        communityId: COMMUNITY_ID,
        proposal: makeProposal({ your_signal: true }),
        adapter,
      },
    });
    const btn = screen.getByRole('button');
    expect(btn.textContent).toContain('Withdraw signal');
    expect((btn as HTMLButtonElement).getAttribute('aria-pressed')).toBe('true');
  });

  it('hides the signal button when the proposal is Finalized', () => {
    render(ConvictionProposalCard, {
      props: {
        communityId: COMMUNITY_ID,
        proposal: makeProposal({ lifecycle: 'Finalized' }),
        adapter,
      },
    });
    // No signal button — terminal lifecycle.
    expect(screen.queryByRole('button')).toBeNull();
    expect(screen.getByLabelText('Lifecycle').textContent).toBe('Finalized');
  });

  it('calls signalTier2(proposal_id, true) when clicking Signal support', async () => {
    render(ConvictionProposalCard, {
      props: {
        communityId: COMMUNITY_ID,
        proposal: makeProposal({ your_signal: undefined }),
        adapter,
      },
    });
    const btn = screen.getByRole('button');
    await fireEvent.click(btn);
    await waitFor(() => {
      expect(signalMock).toHaveBeenCalled();
    });
    expect(signalMock).toHaveBeenCalledWith(PROPOSAL_ID, true);
  });

  it('calls signalTier2(proposal_id, false) when clicking Withdraw signal', async () => {
    render(ConvictionProposalCard, {
      props: {
        communityId: COMMUNITY_ID,
        proposal: makeProposal({ your_signal: true }),
        adapter,
      },
    });
    const btn = screen.getByRole('button');
    await fireEvent.click(btn);
    await waitFor(() => {
      expect(signalMock).toHaveBeenCalled();
    });
    expect(signalMock).toHaveBeenCalledWith(PROPOSAL_ID, false);
  });

  it('shows error and rolls back optimistic state on signal failure', async () => {
    signalMock.mockRejectedValueOnce(new Error('voting_signal_tier2: not a member'));
    render(ConvictionProposalCard, {
      props: {
        communityId: COMMUNITY_ID,
        proposal: makeProposal({ your_signal: undefined }),
        adapter,
      },
    });
    const btn = screen.getByRole('button');
    await fireEvent.click(btn);
    await waitFor(() => {
      expect(screen.getByRole('alert').textContent).toContain('voting_signal_tier2: not a member');
    });
    // Roll-back: button should now be back to "Signal support" (the
    // optimistic flip to "Withdraw signal" reverted).
    expect(screen.getByRole('button').textContent).toContain('Signal support');
  });
});
