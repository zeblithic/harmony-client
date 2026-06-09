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

// ZEB-419: a stand-in for the dedicated MemberCardService the panel runs. Only
// the surface the panel touches (resolve / subscribeVisible / unsubscribeAll /
// onUpdate) is implemented.
function mockCardService(
  cards: Record<string, { displayName: string; avatarUrl?: string; statusText?: string }> = {},
) {
  return {
    onUpdate: undefined as (() => void) | undefined,
    resolve: vi.fn((id: string) => cards[id.toLowerCase()]),
    subscribeVisible: vi.fn().mockResolvedValue(undefined),
    unsubscribeAll: vi.fn().mockResolvedValue(undefined),
  };
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

  it('surfaces a retry error instead of masking it as normal waiting', async () => {
    vi.useFakeTimers();
    const addByKey = vi
      .fn()
      .mockResolvedValueOnce({ kind: 'pending' }) // initial add
      .mockRejectedValueOnce(new Error('iroh connect failed')); // retry throws
    const service = mockService({ addByKey });
    const { getByTestId } = render(FriendsPanel, { props: { service } });
    await vi.advanceTimersByTimeAsync(0);

    await fireEvent.input(getByTestId('add-by-key-input'), { target: { value: PEER_KEY } });
    await fireEvent.click(getByTestId('add-by-key-btn'));
    await vi.advanceTimersByTimeAsync(0);
    expect(getByTestId('add-by-key-status').textContent).toContain('Request sent');

    await vi.advanceTimersByTimeAsync(10_000); // retry → throws a hard error
    const status = getByTestId('add-by-key-status').textContent;
    // The failure is surfaced immediately, not hidden behind the rosy
    // "we'll connect automatically" copy until the window elapses.
    expect(status).toContain('iroh connect failed');
    expect(status).not.toContain('Request sent');
  });

  it('does not start a retry chain if unmounted during the initial add', async () => {
    vi.useFakeTimers();
    let resolveAdd: (v: { kind: string }) => void = () => {};
    const addByKey = vi.fn().mockReturnValue(
      new Promise((r) => {
        resolveAdd = r as (v: { kind: string }) => void;
      }),
    );
    const service = mockService({ addByKey });
    const { getByTestId, unmount } = render(FriendsPanel, { props: { service } });
    await vi.advanceTimersByTimeAsync(0);

    await fireEvent.input(getByTestId('add-by-key-input'), { target: { value: PEER_KEY } });
    await fireEvent.click(getByTestId('add-by-key-btn'));
    // addByKey is in-flight; tear the panel down, THEN let it resolve pending.
    unmount();
    resolveAdd({ kind: 'pending' });
    await vi.advanceTimersByTimeAsync(60_000);

    // No retry chain was created after teardown (only the initial in-flight call).
    expect(addByKey).toHaveBeenCalledTimes(1);
  });

  it('terminal message reflects the last error, not a generic wait', async () => {
    vi.useFakeTimers();
    const addByKey = vi
      .fn()
      .mockResolvedValueOnce({ kind: 'pending' })
      .mockRejectedValue(new Error('pkarr resolve: boom'));
    const service = mockService({ addByKey });
    const { getByTestId } = render(FriendsPanel, { props: { service } });
    await vi.advanceTimersByTimeAsync(0);

    await fireEvent.input(getByTestId('add-by-key-input'), { target: { value: PEER_KEY } });
    await fireEvent.click(getByTestId('add-by-key-btn'));
    await vi.advanceTimersByTimeAsync(0);

    // Exhaust the whole retry window so the terminal message is shown.
    await vi.advanceTimersByTimeAsync(10_000 * 31);
    const status = getByTestId('add-by-key-status').textContent ?? '';
    expect(status).toContain('pkarr resolve: boom'); // the real failure, not hidden
    expect(status).not.toContain('have not accepted');
  });

  it('terminal message reflects repeated unreachable, not "waiting to accept"', async () => {
    vi.useFakeTimers();
    const addByKey = vi
      .fn()
      .mockResolvedValueOnce({ kind: 'pending' })
      .mockResolvedValue({ kind: 'unreachable' });
    const service = mockService({ addByKey });
    const { getByTestId } = render(FriendsPanel, { props: { service } });
    await vi.advanceTimersByTimeAsync(0);

    await fireEvent.input(getByTestId('add-by-key-input'), { target: { value: PEER_KEY } });
    await fireEvent.click(getByTestId('add-by-key-btn'));
    await vi.advanceTimersByTimeAsync(0);

    await vi.advanceTimersByTimeAsync(10_000 * 31);
    const status = (getByTestId('add-by-key-status').textContent ?? '').toLowerCase();
    expect(status).toContain('reach'); // "could not reach them on the last try"
    expect(status).not.toContain('have not accepted');
  });

  it('does not start a retry chain if unmounted during refreshPending', async () => {
    vi.useFakeTimers();
    let resolvePending: (v: unknown[]) => void = () => {};
    const addByKey = vi.fn().mockResolvedValue({ kind: 'pending' });
    const listPendingRequests = vi
      .fn()
      .mockResolvedValueOnce([]) // initial mount refresh
      .mockReturnValue(
        new Promise((r) => {
          resolvePending = r as (v: unknown[]) => void;
        }),
      ); // the post-add refresh hangs until we resolve it
    const service = mockService({ addByKey, listPendingRequests });
    const { getByTestId, unmount } = render(FriendsPanel, { props: { service } });
    await vi.advanceTimersByTimeAsync(0);

    await fireEvent.input(getByTestId('add-by-key-input'), { target: { value: PEER_KEY } });
    await fireEvent.click(getByTestId('add-by-key-btn'));
    await vi.advanceTimersByTimeAsync(0); // addByKey resolves pending; now awaiting refreshPending

    // Tear down while refreshPending is in flight, THEN let it resolve.
    unmount();
    resolvePending([]);
    await vi.advanceTimersByTimeAsync(60_000);

    // The post-refresh `startAddRetry` must not run after teardown.
    expect(addByKey).toHaveBeenCalledTimes(1);
  });
});

describe('FriendsPanel — owner names + nicknames (ZEB-419)', () => {
  const ID = (b: string) => b.repeat(32); // 16-byte owner_id = 32 hex chars

  it('label ladder: nickname > card name > display hint > short-hex', async () => {
    const friends = [
      { ownerIdHex: ID('a'), display: null, nickname: 'Nick', status: 'active', establishedVia: 'mutual_key', referrable: false },
      { ownerIdHex: ID('b'), display: 'Hint', nickname: null, status: 'active', establishedVia: 'token', referrable: false },
      { ownerIdHex: ID('c'), display: 'Hint', nickname: null, status: 'active', establishedVia: 'token', referrable: false },
      { ownerIdHex: ID('d'), display: null, nickname: null, status: 'active', establishedVia: 'mutual_key', referrable: false },
    ];
    const service = mockService({ listFriends: vi.fn().mockResolvedValue(friends) });
    const cardService = mockCardService({ [ID('c')]: { displayName: 'CardName' } });
    const { findByTestId, getByTestId } = render(FriendsPanel, {
      props: { service, cardService },
    });
    await findByTestId('friend-list');

    expect(getByTestId(`friend-name-${ID('a')}`).textContent).toBe('Nick'); // nickname wins
    expect(getByTestId(`friend-name-${ID('b')}`).textContent).toBe('Hint'); // display hint (no card)
    expect(getByTestId(`friend-name-${ID('c')}`).textContent).toBe('CardName'); // card beats hint
    expect(getByTestId(`friend-name-${ID('d')}`).textContent).toContain('dddddddddddd'); // short-hex
  });

  it('subscribes to friend + pending owner_ids and unsubscribes on unmount', async () => {
    const friends = [
      { ownerIdHex: ID('a'), display: null, nickname: null, status: 'active', establishedVia: 'mutual_key', referrable: false },
    ];
    const pending = [{ ownerIdHex: ID('b'), display: null, receivedAtMs: 0 }];
    const service = mockService({
      listFriends: vi.fn().mockResolvedValue(friends),
      listPendingRequests: vi.fn().mockResolvedValue(pending),
    });
    const cardService = mockCardService();
    const { findByTestId, unmount } = render(FriendsPanel, {
      props: { service, cardService },
    });
    await findByTestId('friend-list');

    await vi.waitFor(() =>
      expect(cardService.subscribeVisible).toHaveBeenCalledWith(
        expect.arrayContaining([ID('a'), ID('b')]),
      ),
    );

    unmount();
    expect(cardService.unsubscribeAll).toHaveBeenCalled();
  });

  it('sets a nickname via the inline editor', async () => {
    const id = 'a'.repeat(32);
    const friends = [
      { ownerIdHex: id, display: null, nickname: null, status: 'active', establishedVia: 'mutual_key', referrable: false },
    ];
    const setNickname = vi.fn().mockResolvedValue(undefined);
    const service = mockService({ listFriends: vi.fn().mockResolvedValue(friends), setNickname });
    const { findByTestId, getByTestId } = render(FriendsPanel, {
      props: { service, cardService: mockCardService() },
    });
    await findByTestId('friend-list');

    await fireEvent.click(getByTestId(`set-nickname-btn-${id}`));
    await fireEvent.input(getByTestId(`nickname-input-${id}`), { target: { value: 'Koya' } });
    await fireEvent.click(getByTestId(`nickname-save-${id}`));
    expect(setNickname).toHaveBeenCalledWith(id, 'Koya');
  });

  it('clears a nickname when saved blank (trim → null)', async () => {
    const id = 'b'.repeat(32);
    const friends = [
      { ownerIdHex: id, display: 'Hint', nickname: 'Old', status: 'active', establishedVia: 'token', referrable: false },
    ];
    const setNickname = vi.fn().mockResolvedValue(undefined);
    const service = mockService({ listFriends: vi.fn().mockResolvedValue(friends), setNickname });
    const { findByTestId, getByTestId } = render(FriendsPanel, {
      props: { service, cardService: mockCardService() },
    });
    await findByTestId('friend-list');

    await fireEvent.click(getByTestId(`set-nickname-btn-${id}`));
    await fireEvent.input(getByTestId(`nickname-input-${id}`), { target: { value: '   ' } });
    await fireEvent.click(getByTestId(`nickname-save-${id}`));
    expect(setNickname).toHaveBeenCalledWith(id, null);
  });

  it('identity drill-down opens the owner card with full hex + real card name (not the nickname)', async () => {
    const id = 'a'.repeat(32);
    const friends = [
      { ownerIdHex: id, display: null, nickname: 'Nick', status: 'active', establishedVia: 'mutual_key', referrable: false },
    ];
    const service = mockService({ listFriends: vi.fn().mockResolvedValue(friends) });
    const cardService = mockCardService({ [id]: { displayName: 'RealCardName', statusText: 'hi' } });
    const onOpenCard = vi.fn();
    const { findByTestId, getByTestId } = render(FriendsPanel, {
      props: { service, cardService, onOpenCard },
    });
    await findByTestId('friend-list');

    await fireEvent.click(getByTestId(`friend-identity-${id}`));
    expect(onOpenCard).toHaveBeenCalledTimes(1);
    const payload = onOpenCard.mock.calls[0][0];
    expect(payload.ownerIdHex).toBe(id); // FULL hex, not short
    expect(payload.displayName).toBe('RealCardName'); // the signed card name, NOT 'Nick'
  });
});
