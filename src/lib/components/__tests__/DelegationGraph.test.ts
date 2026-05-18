/**
 * ZEB-292 Phase 3 — DelegationGraph vitest coverage.
 *
 * Asserts rendering shape (node + edge counts), event-driven refetch,
 * and that the local user's node is visually distinguished. We don't
 * pin specific d3-force positions — those are layout-dependent and
 * would just flake when d3 updates.
 */
import { render, screen, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import DelegationGraph from '../DelegationGraph.svelte';
import { VotingAdapter } from '../../voting-adapter';
import type { CommunityMember } from '../../types';
import type {
  DelegationEdgeExport,
  VotingDelegationChangedPayload,
} from '../../types/voting';

const COMMUNITY_ID = 'bb'.repeat(16);
const ME = 'aa'.repeat(16);
const BOB = 'cc'.repeat(16);
const CAROL = 'dd'.repeat(16);

function makeMember(addr: string, displayName: string): CommunityMember {
  return { address: addr, displayName, power: 1, status: 'joined' };
}

function makeEdge(from: string, to: string, hlcMs = 1): DelegationEdgeExport {
  return { from, to, lastHlcMs: hlcMs, lastHlcLogical: 0 };
}

describe('DelegationGraph', () => {
  let adapter: VotingAdapter;
  let listMock: ReturnType<typeof vi.fn<(cid: string) => Promise<DelegationEdgeExport[]>>>;
  let changedHandlers: Array<(p: VotingDelegationChangedPayload) => void>;

  const members: CommunityMember[] = [
    makeMember(ME, 'me'),
    makeMember(BOB, 'bob'),
    makeMember(CAROL, 'carol'),
  ];

  beforeEach(() => {
    adapter = new VotingAdapter();
    changedHandlers = [];
    listMock = vi
      .fn<(cid: string) => Promise<DelegationEdgeExport[]>>()
      .mockResolvedValue([]);
    adapter.listDelegations = listMock;
    adapter.subscribeDelegationChanged = (h) => {
      changedHandlers.push(h);
      return () => {
        const i = changedHandlers.indexOf(h);
        if (i >= 0) changedHandlers.splice(i, 1);
      };
    };
  });

  it('shows empty-state message when no delegations exist', async () => {
    listMock.mockResolvedValueOnce([]);
    render(DelegationGraph, {
      props: { communityId: COMMUNITY_ID, adapter, myAddr: ME, communityMembers: members },
    });
    expect(
      await screen.findByText(/no active delegations/i),
    ).toBeInTheDocument();
  });

  it('renders one node per address appearing in any edge', async () => {
    listMock.mockResolvedValueOnce([makeEdge(ME, BOB), makeEdge(CAROL, BOB)]);
    const { container } = render(DelegationGraph, {
      props: { communityId: COMMUNITY_ID, adapter, myAddr: ME, communityMembers: members },
    });
    await waitFor(() => {
      expect(container.querySelectorAll('circle.dg-node')).toHaveLength(3);
    });
    expect(container.querySelectorAll('line.dg-edge')).toHaveLength(2);
  });

  it('highlights the local user node with dg-node-local class', async () => {
    listMock.mockResolvedValueOnce([makeEdge(ME, BOB)]);
    const { container } = render(DelegationGraph, {
      props: { communityId: COMMUNITY_ID, adapter, myAddr: ME, communityMembers: members },
    });
    await waitFor(() => {
      expect(container.querySelector('circle.dg-node-local')).toBeTruthy();
    });
    expect(container.querySelectorAll('circle.dg-node-local')).toHaveLength(1);
  });

  it('refetches on voting-delegation-changed for the same community', async () => {
    listMock.mockResolvedValueOnce([]);
    render(DelegationGraph, {
      props: { communityId: COMMUNITY_ID, adapter, myAddr: ME, communityMembers: members },
    });
    await waitFor(() => {
      expect(listMock).toHaveBeenCalledTimes(1);
    });
    listMock.mockResolvedValueOnce([makeEdge(BOB, CAROL)]);
    for (const h of changedHandlers) {
      h({ communityId: COMMUNITY_ID, delegator: BOB, delegate: CAROL });
    }
    await waitFor(() => {
      expect(listMock).toHaveBeenCalledTimes(2);
    });
  });

  it('does not refetch on event for a different community', async () => {
    listMock.mockResolvedValueOnce([]);
    render(DelegationGraph, {
      props: { communityId: COMMUNITY_ID, adapter, myAddr: ME, communityMembers: members },
    });
    await waitFor(() => {
      expect(listMock).toHaveBeenCalledTimes(1);
    });
    for (const h of changedHandlers) {
      h({ communityId: 'ee'.repeat(16), delegator: BOB, delegate: CAROL });
    }
    await new Promise((r) => setTimeout(r, 5));
    expect(listMock).toHaveBeenCalledTimes(1);
  });

  it('falls back to short hex when a graph node has no roster entry', async () => {
    // Edge references an address NOT in the member list (e.g. delegate
    // who has since left). Display should still render — falling back
    // to the short-hex truncation.
    const STRANGER = 'ee'.repeat(16);
    listMock.mockResolvedValueOnce([makeEdge(ME, STRANGER)]);
    const { container } = render(DelegationGraph, {
      props: {
        communityId: COMMUNITY_ID,
        adapter,
        myAddr: ME,
        communityMembers: [makeMember(ME, 'me')],
      },
    });
    await waitFor(() => {
      expect(container.querySelectorAll('circle.dg-node')).toHaveLength(2);
    });
    // The stranger's short hex (first 8 chars + ellipsis) must appear.
    expect(container.textContent).toContain(STRANGER.slice(0, 8));
  });
});
