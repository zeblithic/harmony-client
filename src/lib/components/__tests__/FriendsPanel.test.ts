import { render, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import FriendsPanel from '../FriendsPanel.svelte';
import type { FriendService } from '../../friend-service';
import type { DmInviteService, PendingDmInviteDto } from '../../dm-invite-service';

// The panel reads/subscribes to the connectivity adapter (Tauri) at mount; stub
// its three exports so the panel mounts in jsdom without a backend. importOriginal
// preserves any other exports the module may add.
vi.mock('../../connectivity-adapter', async (importOriginal) => ({
  ...(await importOriginal<typeof import('../../connectivity-adapter')>()),
  getIdentityDiscoverable: vi.fn().mockResolvedValue(null),
  setIdentityDiscoverable: vi.fn().mockResolvedValue(undefined),
  onIdentityDiscoverableChanged: vi.fn().mockReturnValue(() => {}),
}));

const INVITER = 'ee'.repeat(16); // 32-char owner_id hex

const invite: PendingDmInviteDto = {
  spaceIdHex: 'a1',
  inviterOwnerIdHex: INVITER,
  kind: 'd',
  memberOwnerIdsHex: [],
  createdAtMs: 1,
  receivedAtMs: 2,
};

// A FriendService stub covering only the methods FriendsPanel calls on mount —
// every list is empty so the DM-invite row is the sole `.friend-name` rendered.
function mockService(): FriendService {
  return {
    onFriendsChanged: () => () => {},
    onPendingRequestsChanged: () => () => {},
    listFriends: () => Promise.resolve([]),
    listPendingRequests: () => Promise.resolve([]),
    listOutboundRequests: () => Promise.resolve([]),
    getAutoAccept: () => Promise.resolve(false),
    getPeerIntroPolicy: () => Promise.resolve('accept_all'),
    getMyIdentityPubHex: () => Promise.resolve(null),
  } as unknown as FriendService;
}

function mockDmSvc(pending: PendingDmInviteDto[]): DmInviteService {
  return {
    onPendingChanged: () => () => {},
    listPending: () => Promise.resolve(pending),
  } as unknown as DmInviteService;
}

describe('FriendsPanel DM-invite inviter name resolution (ZEB-961)', () => {
  it('resolves the inviter broadcast card name over hex', async () => {
    const { getByTestId } = render(FriendsPanel, {
      props: {
        service: mockService(),
        dmInviteService: mockDmSvc([invite]),
        resolveCard: (id: string) =>
          id === INVITER ? { displayName: 'Zeb', statusText: '' } : undefined,
      },
    });
    await waitFor(() =>
      expect(getByTestId('dm-invite-inviter-a1').textContent).toBe('Zeb'),
    );
  });

  it('falls back to short hex when no card resolves', async () => {
    const { getByTestId } = render(FriendsPanel, {
      props: {
        service: mockService(),
        dmInviteService: mockDmSvc([invite]),
        resolveCard: () => undefined,
      },
    });
    // FriendsPanel's in-file shortId truncates to 12 (matching its sibling
    // friend/request/outbound rows), so the DM-invite row now shares that shape.
    await waitFor(() =>
      expect(getByTestId('dm-invite-inviter-a1').textContent).toBe(`${INVITER.slice(0, 12)}…`),
    );
  });

  // ZEB-961 (CodeRabbit #708): the resolver only resolves a card the service has
  // subscribed, so the panel must include its pending DM-invite inviters in the
  // card bucket it drives — otherwise the row above can never resolve a name.
  it('subscribes DM-invite inviters into the friends card bucket', async () => {
    const setFriendsBucket = vi.fn();
    render(FriendsPanel, {
      props: {
        service: mockService(),
        dmInviteService: mockDmSvc([invite]),
        setFriendsBucket,
      },
    });
    await waitFor(() =>
      expect(setFriendsBucket).toHaveBeenCalledWith(expect.arrayContaining([INVITER])),
    );
  });
});
