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
    total_conviction_ms: '250',
    threshold_conviction_ms: '1000',
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

  it('shows a sub-day half-life as hours (not "0d") and a whole-percent readout', () => {
    // ZEB-648 item 3: half-life <~12h used to render "0d"; percent readout
    // now whole-number (was toFixed(1)) so it matches the chip.
    render(ConvictionProposalCard, {
      props: {
        communityId: COMMUNITY_ID,
        proposal: makeProposal({ half_life_seconds: 6 * 3600 }),
        adapter,
      },
    });
    expect(screen.getByLabelText('Half-life').textContent).toContain('half-life 6h');
    // 250 / 1000 = 25% of threshold, no decimals.
    expect(screen.getByLabelText('Percent of threshold').textContent?.trim()).toBe('25%');
  });

  it('shows "▲ Support" when the caller has not yet signaled', () => {
    render(ConvictionProposalCard, {
      props: {
        communityId: COMMUNITY_ID,
        proposal: makeProposal({ your_signal: undefined }),
        adapter,
      },
    });
    const btn = screen.getByRole('button');
    expect(btn.textContent).toContain('▲ Support');
    expect((btn as HTMLButtonElement).getAttribute('aria-pressed')).toBe('false');
  });

  it('shows "Withdraw support" when the caller is currently supporting', () => {
    render(ConvictionProposalCard, {
      props: {
        communityId: COMMUNITY_ID,
        proposal: makeProposal({ your_signal: true }),
        adapter,
      },
    });
    const btn = screen.getByRole('button');
    expect(btn.textContent).toContain('Withdraw support');
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

  it('calls signalTier2(proposal_id, true) when clicking ▲ Support', async () => {
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

  it('calls signalTier2(proposal_id, false) when clicking Withdraw support', async () => {
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
    // Roll-back: button should now be back to "▲ Support" (the
    // optimistic flip to "Withdraw support" reverted).
    expect(screen.getByRole('button').textContent).toContain('▲ Support');
  });

  // ─── ZEB-292 Phase 3: per-proposal override affordance ────────────
  describe('per-proposal override affordance', () => {
    const DELEGATE = 'cc'.repeat(16);

    it('shows the override pill when caller has a delegate and no direct signal', () => {
      render(ConvictionProposalCard, {
        props: {
          communityId: COMMUNITY_ID,
          proposal: makeProposal({ your_signal: undefined }),
          adapter,
          myDelegate: DELEGATE,
          delegateName: 'bob',
        },
      });
      expect(screen.getByRole('status', { name: /delegate signaling on your behalf/i })).toBeTruthy();
      expect(screen.getByText(/bob/i)).toBeTruthy();
      expect(screen.getByRole('button', { name: /vote directly/i })).toBeTruthy();
    });

    it('hides the override pill when caller has signaled directly', () => {
      render(ConvictionProposalCard, {
        props: {
          communityId: COMMUNITY_ID,
          proposal: makeProposal({ your_signal: true }),
          adapter,
          myDelegate: DELEGATE,
          delegateName: 'bob',
        },
      });
      expect(screen.queryByRole('status', { name: /delegate signaling on your behalf/i })).toBeNull();
      // The standard signal toggle is visible instead.
      expect(screen.getByRole('button', { name: /withdraw support/i })).toBeTruthy();
    });

    it('hides the override pill when caller has no delegate', () => {
      render(ConvictionProposalCard, {
        props: {
          communityId: COMMUNITY_ID,
          proposal: makeProposal({ your_signal: undefined }),
          adapter,
          myDelegate: null,
          delegateName: null,
        },
      });
      expect(screen.queryByRole('status', { name: /delegate signaling on your behalf/i })).toBeNull();
      expect(screen.getByRole('button', { name: /▲ support/i })).toBeTruthy();
    });

    it('clicking "Vote directly" fires signalTier2(proposalId, true)', async () => {
      render(ConvictionProposalCard, {
        props: {
          communityId: COMMUNITY_ID,
          proposal: makeProposal({ your_signal: undefined }),
          adapter,
          myDelegate: DELEGATE,
          delegateName: 'bob',
        },
      });
      await fireEvent.click(screen.getByRole('button', { name: /vote directly/i }));
      await waitFor(() => {
        expect(signalMock).toHaveBeenCalledWith(PROPOSAL_ID, true);
      });
    });

    it('falls back to generic "Your delegate" label when delegateName is null', () => {
      render(ConvictionProposalCard, {
        props: {
          communityId: COMMUNITY_ID,
          proposal: makeProposal({ your_signal: undefined }),
          adapter,
          myDelegate: DELEGATE,
          delegateName: null,
        },
      });
      expect(screen.getByText(/your delegate/i)).toBeTruthy();
    });

    it('hides the override pill on Finalized lifecycle', () => {
      render(ConvictionProposalCard, {
        props: {
          communityId: COMMUNITY_ID,
          proposal: makeProposal({ your_signal: undefined, lifecycle: 'Finalized' }),
          adapter,
          myDelegate: DELEGATE,
          delegateName: 'bob',
        },
      });
      expect(screen.queryByRole('status', { name: /delegate signaling on your behalf/i })).toBeNull();
    });

    it('shows error on Vote directly failure and keeps the pill visible for retry', async () => {
      signalMock.mockRejectedValueOnce(new Error('voting_signal_tier2: not a member'));
      render(ConvictionProposalCard, {
        props: {
          communityId: COMMUNITY_ID,
          proposal: makeProposal({ your_signal: undefined }),
          adapter,
          myDelegate: DELEGATE,
          delegateName: 'bob',
        },
      });
      await fireEvent.click(screen.getByRole('button', { name: /vote directly/i }));
      await waitFor(() => {
        expect(screen.getByRole('alert').textContent).toContain('voting_signal_tier2: not a member');
      });
      // The override pill should still be visible since the optimistic
      // flip rolls back on error — the caller can retry without losing
      // the override affordance.
      expect(
        screen.getByRole('status', { name: /delegate signaling on your behalf/i }),
      ).toBeTruthy();
      expect(screen.getByRole('button', { name: /vote directly/i })).toBeTruthy();
    });
  });
});
