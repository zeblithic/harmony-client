import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import FriendsPanel from './FriendsPanel.svelte';
import type { FriendService } from '../friend-service';
import * as connectivity from '../connectivity-adapter';

// FriendsPanel reads + toggles "Allow discovery by identity address" directly
// through the connectivity-adapter module functions (ZEB-415 #1). Mock the
// module so the component never reaches the real Tauri `invoke` under jsdom.
vi.mock('../connectivity-adapter', () => ({
  getIdentityDiscoverable: vi.fn(),
  setIdentityDiscoverable: vi.fn(),
  onIdentityDiscoverableChanged: vi.fn(),
}));

const FULL_KEY = 'ab'.repeat(64); // 128 hex chars

function mockService(overrides: Partial<FriendService> = {}): FriendService {
  return {
    listFriends: vi.fn().mockResolvedValue([]),
    listPendingRequests: vi.fn().mockResolvedValue([]),
    getAutoAccept: vi.fn().mockResolvedValue(false),
    onFriendsChanged: vi.fn().mockReturnValue(() => {}),
    onPendingRequestsChanged: vi.fn().mockReturnValue(() => {}),
    getMyIdentityPubHex: vi.fn().mockResolvedValue(null),
    ...overrides,
  } as unknown as FriendService;
}

const writeText = vi.fn().mockResolvedValue(undefined);

beforeEach(() => {
  writeText.mockClear();
  Object.defineProperty(navigator, 'clipboard', {
    value: { writeText },
    configurable: true,
  });
  // Default: discovery ON (no warning) unless a test overrides it.
  vi.mocked(connectivity.getIdentityDiscoverable).mockResolvedValue(true);
  vi.mocked(connectivity.setIdentityDiscoverable).mockResolvedValue(undefined);
  vi.mocked(connectivity.onIdentityDiscoverableChanged).mockReturnValue(() => {});
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe('FriendsPanel — My key (ZEB-388)', () => {
  it('renders the key and copies the full hex to the clipboard', async () => {
    const service = mockService({
      getMyIdentityPubHex: vi.fn().mockResolvedValue(FULL_KEY),
    });
    const { findByTestId } = render(FriendsPanel, { props: { service } });

    const input = (await findByTestId('my-key-input')) as HTMLInputElement;
    expect(input.value).toBe(FULL_KEY);

    const btn = await findByTestId('my-key-copy-btn');
    await fireEvent.click(btn);
    expect(writeText).toHaveBeenCalledWith(FULL_KEY);
  });

  it('shows a neutral "start your node" message when no key is available', async () => {
    const service = mockService({
      getMyIdentityPubHex: vi.fn().mockResolvedValue(null),
    });
    const { findByTestId, queryByTestId } = render(FriendsPanel, { props: { service } });

    expect(await findByTestId('my-key-empty')).toBeTruthy();
    expect(queryByTestId('my-key-copy-btn')).toBeNull();
  });
});

describe('FriendsPanel — discovery-off footgun (ZEB-415 #1)', () => {
  it('warns and offers Enable when a key is shown but discovery is off', async () => {
    vi.mocked(connectivity.getIdentityDiscoverable).mockResolvedValue(false);
    const service = mockService({
      getMyIdentityPubHex: vi.fn().mockResolvedValue(FULL_KEY),
    });
    const { findByTestId } = render(FriendsPanel, { props: { service } });

    // The warning surfaces directly under the shared key…
    expect(await findByTestId('my-key-discovery-warning')).toBeTruthy();

    // …and its inline action turns discovery on.
    const enableBtn = await findByTestId('my-key-enable-discovery-btn');
    await fireEvent.click(enableBtn);
    expect(connectivity.setIdentityDiscoverable).toHaveBeenCalledWith(true);
  });

  it('does not warn when discovery is already on', async () => {
    vi.mocked(connectivity.getIdentityDiscoverable).mockResolvedValue(true);
    const service = mockService({
      getMyIdentityPubHex: vi.fn().mockResolvedValue(FULL_KEY),
    });
    const { findByTestId, queryByTestId } = render(FriendsPanel, { props: { service } });

    // Key present, discovery on → no nag.
    await findByTestId('my-key-input');
    expect(queryByTestId('my-key-discovery-warning')).toBeNull();
  });

  it('clears the warning when discovery is toggled on elsewhere', async () => {
    let fireChange: (enabled: boolean) => void = () => {};
    vi.mocked(connectivity.getIdentityDiscoverable).mockResolvedValue(false);
    vi.mocked(connectivity.onIdentityDiscoverableChanged).mockImplementation((cb) => {
      fireChange = cb;
      return () => {};
    });
    const service = mockService({
      getMyIdentityPubHex: vi.fn().mockResolvedValue(FULL_KEY),
    });
    const { findByTestId, queryByTestId } = render(FriendsPanel, { props: { service } });

    expect(await findByTestId('my-key-discovery-warning')).toBeTruthy();

    // A discovery-on event from anywhere (e.g. the Network settings panel) clears it.
    fireChange(true);
    await vi.waitFor(() => expect(queryByTestId('my-key-discovery-warning')).toBeNull());
  });

  it('does not let a slow initial read clobber a fresher discovery event', async () => {
    // Defer the mount-time read so a change event can land before it resolves.
    let resolveRead: (v: boolean) => void = () => {};
    vi.mocked(connectivity.getIdentityDiscoverable).mockReturnValue(
      new Promise<boolean>((res) => {
        resolveRead = res;
      }),
    );
    let fireChange: (enabled: boolean) => void = () => {};
    vi.mocked(connectivity.onIdentityDiscoverableChanged).mockImplementation((cb) => {
      fireChange = cb;
      return () => {};
    });
    const service = mockService({
      getMyIdentityPubHex: vi.fn().mockResolvedValue(FULL_KEY),
    });
    const { findByTestId, queryByTestId } = render(FriendsPanel, { props: { service } });

    await findByTestId('my-key-input'); // mounted; subscription registered

    // A fresher event reports discovery ON, THEN the slow mount-time read
    // finally resolves with the now-stale OFF snapshot. The stale value must
    // NOT win and resurrect the warning.
    fireChange(true);
    resolveRead(false);
    // Settle on a macrotask boundary so the stale-read continuation AND the
    // Svelte DOM flush both complete — we assert the final stable state, not a
    // mid-flush snapshot.
    await new Promise((r) => setTimeout(r, 0));
    expect(queryByTestId('my-key-discovery-warning')).toBeNull();
  });
});

describe('FriendsPanel — add-by-key auto-retry (ZEB-415 #2)', () => {
  const PEER_KEY = 'cd'.repeat(64);

  it('auto-retries a pending add and reports connection once the peer accepts', async () => {
    vi.useFakeTimers();
    // 1st (initial click) + 2nd (retry) stay pending; 3rd retry links.
    const addByKey = vi
      .fn()
      .mockResolvedValueOnce({ kind: 'pending' })
      .mockResolvedValueOnce({ kind: 'pending' })
      .mockResolvedValue({ kind: 'linked', ownerIdHex: 'ab'.repeat(8), display: 'Koya' });
    const service = mockService({ addByKey });
    const { getByTestId } = render(FriendsPanel, { props: { service } });
    await vi.advanceTimersByTimeAsync(0); // flush mount

    await fireEvent.input(getByTestId('add-by-key-input'), { target: { value: PEER_KEY } });
    await fireEvent.click(getByTestId('add-by-key-btn'));
    await vi.advanceTimersByTimeAsync(0); // resolve the initial 'pending'

    expect(getByTestId('add-by-key-status').textContent).toContain('Request sent');
    expect(addByKey).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(10_000); // retry 1 → still pending
    expect(addByKey).toHaveBeenCalledTimes(2);

    await vi.advanceTimersByTimeAsync(10_000); // retry 2 → linked
    expect(addByKey).toHaveBeenCalledTimes(3);
    expect(getByTestId('add-by-key-status').textContent).toContain('Now connected');

    // No further retries fire once linked.
    await vi.advanceTimersByTimeAsync(30_000);
    expect(addByKey).toHaveBeenCalledTimes(3);
  });

  it('stops auto-retrying when the panel is destroyed', async () => {
    vi.useFakeTimers();
    const addByKey = vi.fn().mockResolvedValue({ kind: 'pending' });
    const service = mockService({ addByKey });
    const { getByTestId, unmount } = render(FriendsPanel, { props: { service } });
    await vi.advanceTimersByTimeAsync(0);

    await fireEvent.input(getByTestId('add-by-key-input'), { target: { value: PEER_KEY } });
    await fireEvent.click(getByTestId('add-by-key-btn'));
    await vi.advanceTimersByTimeAsync(0);
    expect(addByKey).toHaveBeenCalledTimes(1);

    unmount();
    await vi.advanceTimersByTimeAsync(60_000);
    // No retry timer survives unmount (no $state mutation after teardown).
    expect(addByKey).toHaveBeenCalledTimes(1);
  });

  it('gives an actionable message (discovery / try again) when unreachable', async () => {
    const addByKey = vi.fn().mockResolvedValue({ kind: 'unreachable' });
    const service = mockService({ addByKey });
    const { getByTestId, findByTestId } = render(FriendsPanel, { props: { service } });

    await fireEvent.input(getByTestId('add-by-key-input'), { target: { value: PEER_KEY } });
    await fireEvent.click(getByTestId('add-by-key-btn'));

    const status = await findByTestId('add-by-key-status');
    expect(status.textContent).toMatch(/discovery|try again/i);
    // An unreachable peer is NOT auto-retried (only a 'pending' outcome is).
    expect(addByKey).toHaveBeenCalledTimes(1);
  });
});
