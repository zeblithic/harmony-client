import { render, screen, fireEvent, cleanup, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, afterEach } from 'vitest';
import ProfilePopover from '../ProfilePopover.svelte';
import type { Profile } from '../../types';
import type {
  ProfileBroadcastService,
  ProfileMembershipBroadcastInfo,
} from '../../profile-broadcast-service';

// Stub the Tauri event API: production loads it via dynamic import to
// listen for `profile-broadcast-received`, but in tests there's no
// Tauri runtime so the call rejects with `transformCallback` undefined.
// Without this stub, vitest reports unhandled rejections and exits 1
// even when every test assertion passes.
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(async () => () => {}),
}));

const SELF_ADDR = '00'.repeat(16);
const PEER_ADDR = 'aa'.repeat(16);

const mockProfile: Profile = {
  address: 'a1b2c3d4',
  displayName: 'Alice',
  statusText: 'Working on transport layer',
};

function makeService(opts?: {
  initialCached?: ProfileMembershipBroadcastInfo | null;
}) {
  const subscribe = vi.fn(async () => 1);
  const unsubscribe = vi.fn(async () => {});
  const getCached = vi.fn(async () => opts?.initialCached ?? null);
  const setShared = vi.fn(async () => {});
  return {
    service: { subscribe, unsubscribe, getCached, setShared } as unknown as ProfileBroadcastService,
    subscribe,
    unsubscribe,
    getCached,
  };
}

function baseProps(overrides: Record<string, unknown> = {}) {
  const { service } = makeService();
  return {
    profile: mockProfile,
    x: 100,
    y: 100,
    onClose: vi.fn(),
    ownAddress: SELF_ADDR,
    profileBroadcastService: service,
    resolveCommunityName: (() => null) as (cid: string) => string | null,
    ...overrides,
  };
}

describe('ProfilePopover', () => {
  afterEach(() => cleanup());

  it('renders display name and status text', () => {
    render(ProfilePopover, {
      props: baseProps(),
    });
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('Working on transport layer')).toBeTruthy();
  });

  it('renders truncated peer address', () => {
    render(ProfilePopover, {
      props: baseProps(),
    });
    expect(screen.getByText('a1b2c3d4')).toBeTruthy();
  });

  it('renders sound slot labels', () => {
    render(ProfilePopover, {
      props: baseProps(),
    });
    expect(screen.getByText('Quiet')).toBeTruthy();
    expect(screen.getByText('Standard')).toBeTruthy();
    expect(screen.getByText('Loud')).toBeTruthy();
  });

  it('shows "System default" when no custom sounds set', () => {
    render(ProfilePopover, {
      props: baseProps(),
    });
    const defaults = screen.getAllByText('System default');
    expect(defaults.length).toBe(3);
  });

  it('calls onClose when Escape is pressed', async () => {
    const onClose = vi.fn();
    render(ProfilePopover, {
      props: baseProps({ onClose }),
    });
    await fireEvent.keyDown(window, { key: 'Escape' });
    expect(onClose).toHaveBeenCalled();
  });

  it('handles profile without status text', () => {
    const noStatus: Profile = { address: 'xyz789', displayName: 'Bob' };
    render(ProfilePopover, {
      props: baseProps({ profile: noStatus }),
    });
    expect(screen.getByText('Bob')).toBeTruthy();
    expect(screen.queryByText('Working on transport layer')).toBeNull();
  });

  it('popover_subscribes_on_mount', async () => {
    const { service, subscribe } = makeService();
    const peerProfile: Profile = { address: PEER_ADDR, displayName: 'Peer' };
    render(ProfilePopover, {
      props: baseProps({
        profile: peerProfile,
        profileBroadcastService: service,
      }),
    });
    await waitFor(() => expect(subscribe).toHaveBeenCalledWith(PEER_ADDR));
  });

  it('popover_unsubscribes_on_close', async () => {
    const { service, subscribe, unsubscribe } = makeService();
    const peerProfile: Profile = { address: PEER_ADDR, displayName: 'Peer' };
    const { unmount } = render(ProfilePopover, {
      props: baseProps({
        profile: peerProfile,
        profileBroadcastService: service,
      }),
    });
    // Let the async subscribe() resolve before unmounting; without
    // this, the cleanup fires before subscriptionId is captured.
    await waitFor(() => expect(subscribe).toHaveBeenCalled());
    unmount();
    await waitFor(() => expect(unsubscribe).toHaveBeenCalled());
  });

  it('popover_shows_loading_then_loaded', async () => {
    const { service } = makeService({
      initialCached: {
        ownerAddr: PEER_ADDR,
        communityIds: ['bb'.repeat(16)],
        sharedAt: '5000',
      },
    });
    const peerProfile: Profile = { address: PEER_ADDR, displayName: 'Peer' };
    const { getByText } = render(ProfilePopover, {
      props: baseProps({
        profile: peerProfile,
        profileBroadcastService: service,
        resolveCommunityName: () => 'Test Community',
      }),
    });
    // First render shows the loading state.
    expect(getByText('Looking up public memberships…')).toBeTruthy();
    // After cache hydration, the community name appears.
    await waitFor(() => expect(getByText('Test Community')).toBeTruthy());
  });

  it('popover_shows_no_memberships_after_timeout', async () => {
    vi.useFakeTimers();
    try {
      const { service } = makeService(); // returns null
      const peerProfile: Profile = { address: PEER_ADDR, displayName: 'Peer' };
      const { getByText } = render(ProfilePopover, {
        props: baseProps({
          profile: peerProfile,
          profileBroadcastService: service,
        }),
      });
      expect(getByText('Looking up public memberships…')).toBeTruthy();
      // Advance 3s; loading flips off and empty state renders.
      await vi.advanceTimersByTimeAsync(3100);
      expect(getByText('No public memberships shared.')).toBeTruthy();
    } finally {
      // Always restore real timers — if any assertion above throws, the
      // bare `vi.useRealTimers()` call wouldn't run and subsequent tests
      // would inherit fake-timer state.
      vi.useRealTimers();
    }
  });
});
