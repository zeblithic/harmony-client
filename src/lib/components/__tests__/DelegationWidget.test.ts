/**
 * ZEB-292 Phase 3 — DelegationWidget vitest coverage.
 *
 * Patches VotingAdapter methods + the subscribeDelegationChanged
 * handler so we can drive the widget through DOM interactions and
 * synthesize delegation-changed events without standing up a real
 * Tauri adapter.
 */
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import DelegationWidget from '../DelegationWidget.svelte';
import { VotingAdapter } from '../../voting-adapter';
import type { CommunityMember } from '../../types';
import type {
  Tier2ProposalExport,
  VotingDelegationChangedPayload,
} from '../../types/voting';

const COMMUNITY_ID = 'bb'.repeat(16);
const ME = 'aa'.repeat(16);
const BOB = 'cc'.repeat(16);
const CAROL = 'dd'.repeat(16);

function makeMember(addr: string, displayName: string, power = 1): CommunityMember {
  return { address: addr, displayName, power, status: 'joined' };
}

function makeProposal(overrides: Partial<Tier2ProposalExport> = {}): Tier2ProposalExport {
  return {
    proposal_id: 'ff'.repeat(32),
    community_id: COMMUNITY_ID,
    proposal_text: 'sample',
    lifecycle: 'Open',
    total_conviction_ms: '100',
    threshold_conviction_ms: '1000',
    half_life_seconds: 7 * 86400,
    auto_exec: { kk: 'n' },
    total_supply: 10,
    voter_count: 1,
    your_signal: undefined,
    threshold_reached_at_ms: undefined,
    ...overrides,
  };
}

describe('DelegationWidget', () => {
  let adapter: VotingAdapter;
  let getMyDelegateMock: ReturnType<typeof vi.fn<(cid: string) => Promise<string | null>>>;
  let delegateMock: ReturnType<typeof vi.fn<(cid: string, d: string) => Promise<void>>>;
  let undelegateMock: ReturnType<typeof vi.fn<(cid: string) => Promise<void>>>;
  let listProposalsMock: ReturnType<typeof vi.fn<(cid: string) => Promise<Tier2ProposalExport[]>>>;
  let delegationChangedHandlers: Array<(p: VotingDelegationChangedPayload) => void>;

  const members: CommunityMember[] = [
    makeMember(ME, 'me'),
    makeMember(BOB, 'bob'),
    makeMember(CAROL, 'carol'),
  ];

  beforeEach(() => {
    adapter = new VotingAdapter();
    delegationChangedHandlers = [];
    getMyDelegateMock = vi
      .fn<(cid: string) => Promise<string | null>>()
      .mockResolvedValue(null);
    delegateMock = vi
      .fn<(cid: string, d: string) => Promise<void>>()
      .mockResolvedValue(undefined);
    undelegateMock = vi
      .fn<(cid: string) => Promise<void>>()
      .mockResolvedValue(undefined);
    listProposalsMock = vi
      .fn<(cid: string) => Promise<Tier2ProposalExport[]>>()
      .mockResolvedValue([]);
    adapter.getMyDelegate = getMyDelegateMock;
    adapter.delegateTier2 = delegateMock;
    adapter.undelegateTier2 = undelegateMock;
    adapter.listTier2Proposals = listProposalsMock;
    adapter.subscribeDelegationChanged = (h) => {
      delegationChangedHandlers.push(h);
      return () => {
        const i = delegationChangedHandlers.indexOf(h);
        if (i >= 0) delegationChangedHandlers.splice(i, 1);
      };
    };
  });

  it('renders "Voting directly" when caller has no delegate', async () => {
    getMyDelegateMock.mockResolvedValueOnce(null);
    render(DelegationWidget, {
      props: { communityId: COMMUNITY_ID, adapter, myAddr: ME, communityMembers: members },
    });
    expect(await screen.findByText(/voting directly/i)).toBeInTheDocument();
  });

  it('renders the delegate display name when caller has a delegate', async () => {
    getMyDelegateMock.mockResolvedValueOnce(BOB);
    render(DelegationWidget, {
      props: { communityId: COMMUNITY_ID, adapter, myAddr: ME, communityMembers: members },
    });
    await waitFor(() => {
      expect(screen.getByText('bob')).toBeInTheDocument();
    });
  });

  it('delegate-on-pick fires delegateTier2 IPC with the picked address', async () => {
    getMyDelegateMock.mockResolvedValueOnce(null);
    render(DelegationWidget, {
      props: { communityId: COMMUNITY_ID, adapter, myAddr: ME, communityMembers: members },
    });
    const select = (await screen.findByLabelText(/delegate to/i)) as HTMLSelectElement;
    await fireEvent.change(select, { target: { value: BOB } });
    await fireEvent.click(screen.getByRole('button', { name: /^delegate$/i }));
    await waitFor(() => {
      expect(delegateMock).toHaveBeenCalledWith(COMMUNITY_ID, BOB);
    });
  });

  it('revoke with no active proposals shows the click-confirm bar', async () => {
    getMyDelegateMock.mockResolvedValueOnce(BOB);
    listProposalsMock.mockResolvedValueOnce([]);
    render(DelegationWidget, {
      props: { communityId: COMMUNITY_ID, adapter, myAddr: ME, communityMembers: members },
    });
    await fireEvent.click(await screen.findByText(/revoke delegation/i));
    expect(
      await screen.findByRole('alertdialog', { name: /confirm revoke/i }),
    ).toBeInTheDocument();
    await fireEvent.click(screen.getByRole('button', { name: /confirm revoke/i }));
    await waitFor(() => {
      expect(undelegateMock).toHaveBeenCalledWith(COMMUNITY_ID);
    });
  });

  it('revoke with high-participation active proposal shows typed-confirm', async () => {
    getMyDelegateMock.mockResolvedValueOnce(BOB);
    // 3 voters of 10 total supply = 30% participation, > 25% threshold.
    listProposalsMock.mockResolvedValueOnce([
      makeProposal({ lifecycle: 'Open', total_supply: 10, voter_count: 3 }),
    ]);
    render(DelegationWidget, {
      props: { communityId: COMMUNITY_ID, adapter, myAddr: ME, communityMembers: members },
    });
    await fireEvent.click(await screen.findByText(/revoke delegation/i));
    const dialog = await screen.findByRole('alertdialog', { name: /type-to-confirm revoke/i });
    expect(dialog).toBeInTheDocument();
    // Typing wrong word should NOT enable the confirm button.
    const input = screen.getByPlaceholderText('revoke') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'no' } });
    expect(screen.getByRole('button', { name: /confirm revoke/i })).toBeDisabled();
    // Typing "revoke" enables and fires.
    await fireEvent.input(input, { target: { value: 'revoke' } });
    await fireEvent.click(screen.getByRole('button', { name: /confirm revoke/i }));
    await waitFor(() => {
      expect(undelegateMock).toHaveBeenCalledWith(COMMUNITY_ID);
    });
  });

  it('voting-delegation-changed event for the local user triggers a refetch', async () => {
    getMyDelegateMock.mockResolvedValueOnce(null);
    render(DelegationWidget, {
      props: { communityId: COMMUNITY_ID, adapter, myAddr: ME, communityMembers: members },
    });
    // Wait for the initial mount load to flush.
    await waitFor(() => {
      expect(getMyDelegateMock).toHaveBeenCalledTimes(1);
    });
    getMyDelegateMock.mockResolvedValueOnce(BOB);
    // Synthesize a delegation-changed event affecting the local user.
    for (const h of delegationChangedHandlers) {
      h({ communityId: COMMUNITY_ID, delegator: ME, delegate: BOB });
    }
    await waitFor(() => {
      expect(getMyDelegateMock).toHaveBeenCalledTimes(2);
    });
  });

  it('voting-delegation-changed for a different community is ignored', async () => {
    getMyDelegateMock.mockResolvedValueOnce(null);
    render(DelegationWidget, {
      props: { communityId: COMMUNITY_ID, adapter, myAddr: ME, communityMembers: members },
    });
    await waitFor(() => {
      expect(getMyDelegateMock).toHaveBeenCalledTimes(1);
    });
    for (const h of delegationChangedHandlers) {
      h({ communityId: 'ee'.repeat(16), delegator: ME, delegate: BOB });
    }
    // Allow a few microtasks to flush.
    await new Promise((r) => setTimeout(r, 5));
    expect(getMyDelegateMock).toHaveBeenCalledTimes(1);
  });

  it('voting-delegation-changed for an unrelated user is ignored', async () => {
    getMyDelegateMock.mockResolvedValueOnce(null);
    render(DelegationWidget, {
      props: { communityId: COMMUNITY_ID, adapter, myAddr: ME, communityMembers: members },
    });
    await waitFor(() => {
      expect(getMyDelegateMock).toHaveBeenCalledTimes(1);
    });
    for (const h of delegationChangedHandlers) {
      h({ communityId: COMMUNITY_ID, delegator: CAROL, delegate: BOB });
    }
    await new Promise((r) => setTimeout(r, 5));
    expect(getMyDelegateMock).toHaveBeenCalledTimes(1);
  });

  it('member picker excludes the caller and non-joined members', async () => {
    getMyDelegateMock.mockResolvedValueOnce(null);
    render(DelegationWidget, {
      props: {
        communityId: COMMUNITY_ID,
        adapter,
        myAddr: ME,
        communityMembers: [
          makeMember(ME, 'me'),
          makeMember(BOB, 'bob'),
          { ...makeMember(CAROL, 'carol'), status: 'banned' },
        ],
      },
    });
    await screen.findByText(/voting directly/i);
    const select = screen.getByLabelText(/delegate to/i) as HTMLSelectElement;
    const opts = Array.from(select.options).map((o) => o.value);
    expect(opts).toContain(BOB);
    expect(opts).not.toContain(ME);
    expect(opts).not.toContain(CAROL);
  });
});
