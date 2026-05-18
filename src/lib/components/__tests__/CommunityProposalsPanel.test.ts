/**
 * ZEB-291 Phase 2 Task 27 — CommunityProposalsPanel vitest coverage.
 *
 * Patches VotingAdapter methods + subscribe handlers directly. Drives
 * the panel through DOM interactions and asserts on the resulting
 * IPC calls.
 */
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import CommunityProposalsPanel from '../CommunityProposalsPanel.svelte';
import { VotingAdapter } from '../../voting-adapter';
import type { CommunityMember } from '../../types';
import type {
  Tier2ProposalExport,
  VotingTier2ProposalCreatedPayload,
} from '../../types/voting';

const COMMUNITY_ID = 'bb'.repeat(16);
const OTHER_COMMUNITY = 'cc'.repeat(16);
const MY_ADDR = 'aa'.repeat(16);
const BOB_ADDR = 'cc'.repeat(16);
const COMMUNITY_MEMBERS: CommunityMember[] = [
  { address: MY_ADDR, displayName: 'me', power: 50, status: 'joined' },
  { address: BOB_ADDR, displayName: 'bob', power: 1, status: 'joined' },
];

function makeProposal(id: string, overrides: Partial<Tier2ProposalExport> = {}): Tier2ProposalExport {
  return {
    proposal_id: id.padEnd(64, '0').slice(0, 64),
    community_id: COMMUNITY_ID,
    proposal_text: `Proposal ${id}`,
    lifecycle: 'Open',
    total_conviction_ms: '100',
    threshold_conviction_ms: '1000',
    half_life_seconds: 7 * 86400,
    auto_exec: { kk: 'n' },
    total_supply: 5,
    voter_count: 1,
    your_signal: undefined,
    threshold_reached_at_ms: undefined,
    ...overrides,
  };
}

describe('CommunityProposalsPanel', () => {
  type CreateArgs = Parameters<VotingAdapter['createTier2Proposal']>[0];

  let adapter: VotingAdapter;
  let listMock: ReturnType<typeof vi.fn<(cid: string) => Promise<Tier2ProposalExport[]>>>;
  let createMock: ReturnType<typeof vi.fn<(args: CreateArgs) => Promise<string>>>;
  // Capture handlers passed to subscribeProposalCreated so tests can
  // fire events synthetically without standing up a real Tauri adapter.
  let createdHandlers: Array<(p: VotingTier2ProposalCreatedPayload) => void>;

  beforeEach(() => {
    adapter = new VotingAdapter();
    createdHandlers = [];
    listMock = vi
      .fn<(cid: string) => Promise<Tier2ProposalExport[]>>()
      .mockResolvedValue([]);
    createMock = vi
      .fn<(args: CreateArgs) => Promise<string>>()
      .mockResolvedValue('aa'.repeat(32));
    adapter.listTier2Proposals = listMock;
    adapter.createTier2Proposal = createMock;
    // Patch the subscribe methods so tests can capture the handlers.
    adapter.subscribeProposalCreated = (h) => {
      createdHandlers.push(h);
      return () => {
        const i = createdHandlers.indexOf(h);
        if (i >= 0) createdHandlers.splice(i, 1);
      };
    };
    // The other two event subscriptions are not under test here —
    // patch to no-op unsubscribes so the $effect cleanup is well-formed.
    adapter.subscribeThresholdReached = () => () => {};
    adapter.subscribeProposalFinalized = () => () => {};
    // ZEB-292 Phase 3: the embedded DelegationWidget and DelegationGraph
    // call these on mount. Patch to safe no-ops so the parent's tests
    // stay focused on proposal flow.
    adapter.getMyDelegate = vi.fn().mockResolvedValue(null);
    adapter.listDelegations = vi.fn().mockResolvedValue([]);
    adapter.subscribeDelegationChanged = () => () => {};
  });

  it('loads proposals on mount via listTier2Proposals', async () => {
    listMock.mockResolvedValueOnce([makeProposal('1'), makeProposal('2')]);
    render(CommunityProposalsPanel, {
      props: { communityId: COMMUNITY_ID, adapter, myPower: 50, myAddr: MY_ADDR, communityMembers: COMMUNITY_MEMBERS },
    });
    await waitFor(() => {
      expect(listMock).toHaveBeenCalledWith(COMMUNITY_ID);
    });
    await waitFor(() => {
      expect(screen.getByText('Proposal 1')).toBeTruthy();
    });
    expect(screen.getByText('Proposal 2')).toBeTruthy();
  });

  it('shows the new-proposal form when myPower >= 1', async () => {
    render(CommunityProposalsPanel, {
      props: { communityId: COMMUNITY_ID, adapter, myPower: 1, myAddr: MY_ADDR, communityMembers: COMMUNITY_MEMBERS },
    });
    await waitFor(() => {
      expect(listMock).toHaveBeenCalled();
    });
    expect(screen.getByLabelText('New proposal')).toBeTruthy();
    expect(screen.getByRole('button', { name: /Submit/ })).toBeTruthy();
  });

  it('hides the new-proposal form when myPower < 1', async () => {
    render(CommunityProposalsPanel, {
      props: { communityId: COMMUNITY_ID, adapter, myPower: 0, myAddr: MY_ADDR, communityMembers: COMMUNITY_MEMBERS },
    });
    await waitFor(() => {
      expect(listMock).toHaveBeenCalled();
    });
    // No form means no New proposal label.
    expect(screen.queryByLabelText('New proposal')).toBeNull();
  });

  it('submits proposal form via createTier2Proposal', async () => {
    render(CommunityProposalsPanel, {
      props: { communityId: COMMUNITY_ID, adapter, myPower: 50, myAddr: MY_ADDR, communityMembers: COMMUNITY_MEMBERS },
    });
    await waitFor(() => {
      expect(listMock).toHaveBeenCalled();
    });
    const textarea = screen.getByLabelText('New proposal') as HTMLTextAreaElement;
    await fireEvent.input(textarea, { target: { value: 'Add a new channel for art' } });
    const submitBtn = screen.getByRole('button', { name: /Submit/ });
    await fireEvent.click(submitBtn);
    await waitFor(() => {
      expect(createMock).toHaveBeenCalled();
    });
    const args = createMock.mock.calls[0][0];
    expect(args.communityId).toBe(COMMUNITY_ID);
    expect(args.proposalText).toBe('Add a new channel for art');
  });

  it('refetches on proposalCreated event for the same community', async () => {
    listMock.mockResolvedValueOnce([]);
    render(CommunityProposalsPanel, {
      props: { communityId: COMMUNITY_ID, adapter, myPower: 1, myAddr: MY_ADDR, communityMembers: COMMUNITY_MEMBERS },
    });
    await waitFor(() => {
      expect(listMock).toHaveBeenCalledTimes(1);
    });
    // Fire a proposalCreated event for the same community → refetch.
    listMock.mockResolvedValueOnce([makeProposal('new')]);
    for (const h of [...createdHandlers]) {
      h({ proposalId: 'aa'.repeat(32), communityId: COMMUNITY_ID });
    }
    await waitFor(() => {
      expect(listMock).toHaveBeenCalledTimes(2);
    });
    await waitFor(() => {
      expect(screen.getByText('Proposal new')).toBeTruthy();
    });
    // A proposalCreated for a DIFFERENT community must NOT refetch.
    listMock.mockClear();
    for (const h of [...createdHandlers]) {
      h({ proposalId: 'bb'.repeat(32), communityId: OTHER_COMMUNITY });
    }
    // Give microtasks a chance.
    await new Promise((r) => setTimeout(r, 0));
    expect(listMock).not.toHaveBeenCalled();
  });
});
