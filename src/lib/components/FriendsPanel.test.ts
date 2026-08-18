import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import FriendsPanel from './FriendsPanel.svelte';
import type { FriendService } from '../friend-service';
import type { DmInviteService, PendingDmInviteDto } from '../dm-invite-service';
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
    // ZEB-783: loaded unconditionally on mount, like listPendingRequests.
    listOutboundRequests: vi.fn().mockResolvedValue([]),
    cancelOutboundRequest: vi.fn().mockResolvedValue(undefined),
    getAutoAccept: vi.fn().mockResolvedValue(false),
    // ZEB-376 Phase 2b Task 14: mirrors getAutoAccept's default — loaded
    // unconditionally on mount, so every test needs a resolved value even
    // when it doesn't exercise the policy select. 'fof' mirrors the Rust
    // `PeerIntroPolicy` derive default.
    getPeerIntroPolicy: vi.fn().mockResolvedValue('fof'),
    onFriendsChanged: vi.fn().mockReturnValue(() => {}),
    onPendingRequestsChanged: vi.fn().mockReturnValue(() => {}),
    getMyIdentityPubHex: vi.fn().mockResolvedValue(null),
    ...overrides,
  } as unknown as FriendService;
}

// ZEB-840: stand-ins for the two card props the panel now consumes (the shared
// MemberCardService is injected as closures by App, not a whole instance):
//   resolveCard(id)     -> the card map (name/avatar/status)
//   setFriendsBucket(ids) -> the `friends` subscription bucket driver
function mockCards(
  cards: Record<string, { displayName: string; avatarUrl?: string; statusText?: string }> = {},
) {
  return {
    resolveCard: vi.fn((id: string) => cards[id.toLowerCase()]),
    setFriendsBucket: vi.fn(),
  };
}

// ZEB-236 T7: a stand-in for the optional DmInviteService the panel consumes.
// Only the surface the panel touches (listPending / accept / decline /
// onPendingChanged) is implemented.
function mockDmInviteService(overrides: Partial<DmInviteService> = {}): DmInviteService {
  return {
    listPending: vi.fn().mockResolvedValue([]),
    accept: vi.fn().mockResolvedValue(undefined),
    decline: vi.fn().mockResolvedValue(undefined),
    onPendingChanged: vi.fn().mockReturnValue(() => {}),
    ...overrides,
  } as unknown as DmInviteService;
}

const INVITE: PendingDmInviteDto = {
  spaceIdHex: 'deadbeef'.repeat(8),
  inviterOwnerIdHex: 'aabbccdd11223344aabbccdd11223344',
  kind: 'd',
  memberOwnerIdsHex: ['aabbccdd11223344aabbccdd11223344'],
  createdAtMs: 1_700_000_000_000,
  receivedAtMs: 1_700_000_000_000,
};

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

describe('FriendsPanel — outbound requests (ZEB-784 / ZEB-783)', () => {
  const PEER_KEY = 'cd'.repeat(64);
  const OUTBOUND = {
    identityPubHex: PEER_KEY,
    requestedAtMs: 1_700_000_000_000,
    expiresAtMs: 1_700_000_000_000 + 7 * 24 * 60 * 60 * 1000,
  };

  // ZEB-415 #2 previously ran a 10s/30-attempt retry chain inside this
  // component. It is gone: the NODE now owns the retry, durably and headlessly.
  // These tests pin the replacement contract — the panel SHOWS the node's state
  // and offers manual overrides, and initiates no recurring dialling of its own.

  it('renders a sent-request row so a pending add leaves visible evidence', async () => {
    const service = mockService({
      listOutboundRequests: vi.fn().mockResolvedValue([OUTBOUND]),
    });
    const { getByTestId, findByTestId } = render(FriendsPanel, { props: { service } });

    await findByTestId('outbound-requests-section');
    expect(getByTestId(`outbound-status-${PEER_KEY}`).textContent).toContain(
      'Waiting for them to accept',
    );
    // The row shows the KEY the user typed. It deliberately shows no owner id
    // and no display name: a peer who hasn't accepted has disclosed neither.
    expect(getByTestId(`outbound-key-${PEER_KEY}`).textContent).toContain('cdcdcdcd');
  });

  it('hides the section entirely when nothing is waiting', async () => {
    const service = mockService();
    const { queryByTestId } = render(FriendsPanel, { props: { service } });
    await vi.waitFor(() => expect(queryByTestId('pending-empty')).not.toBeNull());
    expect(queryByTestId('outbound-requests-section')).toBeNull();
  });

  it('runs NO recurring dial of its own — the node owns the retry', async () => {
    vi.useFakeTimers();
    const addByKey = vi.fn().mockResolvedValue({ kind: 'pending' });
    const service = mockService({
      addByKey,
      listOutboundRequests: vi.fn().mockResolvedValue([OUTBOUND]),
    });
    const { getByTestId } = render(FriendsPanel, { props: { service } });
    await vi.advanceTimersByTimeAsync(0);

    await fireEvent.input(getByTestId('add-by-key-input'), { target: { value: PEER_KEY } });
    await fireEvent.click(getByTestId('add-by-key-btn'));
    await vi.advanceTimersByTimeAsync(0);
    expect(addByKey).toHaveBeenCalledTimes(1);

    // The old chain would have fired ~30 more times across this window. A
    // second dial now can only come from the user pressing "Retry now".
    await vi.advanceTimersByTimeAsync(10 * 60_000);
    expect(addByKey).toHaveBeenCalledTimes(1);
  });

  it('shows the sent request immediately after a pending add', async () => {
    const listOutboundRequests = vi
      .fn()
      .mockResolvedValueOnce([]) // mount: nothing waiting yet
      .mockResolvedValue([OUTBOUND]); // after the add: the node recorded it
    const service = mockService({
      addByKey: vi.fn().mockResolvedValue({ kind: 'pending' }),
      listOutboundRequests,
    });
    const { getByTestId, findByTestId } = render(FriendsPanel, { props: { service } });
    await vi.waitFor(() => expect(getByTestId('add-by-key-btn')).not.toBeNull());

    await fireEvent.input(getByTestId('add-by-key-input'), { target: { value: PEER_KEY } });
    await fireEvent.click(getByTestId('add-by-key-btn'));

    // ZEB-783's actual complaint: the add reported `pending` and then left no
    // trace in any surface. The row appearing IS the fix.
    await findByTestId('outbound-requests-section');
    expect(getByTestId('add-by-key-status').textContent).toContain('Request sent');
  });

  it('Retry now dials once and reports the link when the peer has accepted', async () => {
    const addByKey = vi
      .fn()
      .mockResolvedValue({ kind: 'linked', ownerIdHex: 'ab'.repeat(8), display: 'Koya' });
    const service = mockService({
      addByKey,
      listOutboundRequests: vi
        .fn()
        .mockResolvedValueOnce([OUTBOUND])
        .mockResolvedValue([]), // the node forgets the record once linked
    });
    const { getByTestId, findByTestId } = render(FriendsPanel, { props: { service } });
    await findByTestId('outbound-requests-section');

    await fireEvent.click(getByTestId(`outbound-retry-${PEER_KEY}`));

    await vi.waitFor(() =>
      expect(getByTestId('add-by-key-status').textContent).toContain('Now connected'),
    );
    expect(addByKey).toHaveBeenCalledTimes(1);
    expect(addByKey).toHaveBeenCalledWith(PEER_KEY);
  });

  it('surfaces a Retry now failure instead of masking it as normal waiting', async () => {
    const service = mockService({
      addByKey: vi.fn().mockRejectedValue(new Error('iroh connect failed')),
      listOutboundRequests: vi.fn().mockResolvedValue([OUTBOUND]),
    });
    const { getByTestId, findByTestId } = render(FriendsPanel, { props: { service } });
    await findByTestId('outbound-requests-section');

    await fireEvent.click(getByTestId(`outbound-retry-${PEER_KEY}`));

    await vi.waitFor(() => {
      const status = getByTestId('add-by-key-status').textContent ?? '';
      expect(status).toContain('iroh connect failed');
    });
    // The row must SURVIVE a failed manual retry — the node keeps trying
    // regardless of what one manual attempt did.
    expect(getByTestId('outbound-requests-section')).not.toBeNull();
  });

  it('Cancel stops the retry and drops the row', async () => {
    const cancelOutboundRequest = vi.fn().mockResolvedValue(undefined);
    const service = mockService({
      cancelOutboundRequest,
      listOutboundRequests: vi
        .fn()
        .mockResolvedValueOnce([OUTBOUND])
        .mockResolvedValue([]),
    });
    const { getByTestId, queryByTestId, findByTestId } = render(FriendsPanel, {
      props: { service },
    });
    await findByTestId('outbound-requests-section');

    await fireEvent.click(getByTestId(`outbound-cancel-${PEER_KEY}`));

    expect(cancelOutboundRequest).toHaveBeenCalledWith(PEER_KEY);
    await vi.waitFor(() => expect(queryByTestId('outbound-requests-section')).toBeNull());
  });

  it('keys per-row action buttons so multiple rows stay addressable', async () => {
    // Regression guard for the duplicate-test-id bug: with static ids, two rows
    // render two `outbound-retry-btn`s and getByTestId throws on the ambiguity.
    // A single-row fixture cannot catch it, which is why this one has two.
    const OTHER = 'ef'.repeat(64);
    const service = mockService({
      listOutboundRequests: vi.fn().mockResolvedValue([
        OUTBOUND,
        { identityPubHex: OTHER, requestedAtMs: 1_700_000_100_000, expiresAtMs: 1_700_000_100_000 },
      ]),
    });
    const { getByTestId, findByTestId } = render(FriendsPanel, { props: { service } });
    await findByTestId('outbound-requests-section');

    // Each row's controls resolve unambiguously to that row's key.
    expect(getByTestId(`outbound-retry-${PEER_KEY}`)).not.toBeNull();
    expect(getByTestId(`outbound-cancel-${PEER_KEY}`)).not.toBeNull();
    expect(getByTestId(`outbound-retry-${OTHER}`)).not.toBeNull();
    expect(getByTestId(`outbound-cancel-${OTHER}`)).not.toBeNull();
  });

  it('clears the waiting copy after a cancel so it cannot outlive the row', async () => {
    // The status line is shared with handleAddByKey, which writes "we'll keep
    // trying until they do". After a cancel that message describes a request
    // that no longer exists and retries that will not happen.
    const service = mockService({
      addByKey: vi.fn().mockResolvedValue({ kind: 'pending' }),
      listOutboundRequests: vi
        .fn()
        .mockResolvedValueOnce([])
        .mockResolvedValueOnce([OUTBOUND])
        .mockResolvedValue([]),
    });
    const { getByTestId, queryByTestId, findByTestId } = render(FriendsPanel, {
      props: { service },
    });
    await vi.waitFor(() => expect(getByTestId('add-by-key-btn')).not.toBeNull());

    await fireEvent.input(getByTestId('add-by-key-input'), { target: { value: PEER_KEY } });
    await fireEvent.click(getByTestId('add-by-key-btn'));
    await findByTestId('outbound-requests-section');
    expect(getByTestId('add-by-key-status').textContent).toContain('keep trying');

    await fireEvent.click(getByTestId(`outbound-cancel-${PEER_KEY}`));

    await vi.waitFor(() => expect(queryByTestId('outbound-requests-section')).toBeNull());
    expect(queryByTestId('add-by-key-status')).toBeNull();
  });

  it('does not write state after unmount when an add is still in flight', async () => {
    vi.useFakeTimers();
    let resolveAdd: (v: unknown) => void = () => {};
    const addByKey = vi.fn().mockReturnValue(
      new Promise((r) => {
        resolveAdd = r as (v: unknown) => void;
      }),
    );
    const listOutboundRequests = vi.fn().mockResolvedValue([]);
    const service = mockService({ addByKey, listOutboundRequests });
    const { getByTestId, unmount } = render(FriendsPanel, { props: { service } });
    await vi.advanceTimersByTimeAsync(0);
    const callsAtMount = listOutboundRequests.mock.calls.length;

    await fireEvent.input(getByTestId('add-by-key-input'), { target: { value: PEER_KEY } });
    await fireEvent.click(getByTestId('add-by-key-btn'));

    // Tear down while the dial is in flight, THEN let it resolve.
    unmount();
    resolveAdd({ kind: 'pending' });
    await vi.advanceTimersByTimeAsync(60_000);

    // The post-add refresh must not run after teardown — it would assign
    // $state on a destroyed component.
    expect(listOutboundRequests.mock.calls.length).toBe(callsAtMount);
  });
});

describe('FriendsPanel — teardown-guarded failure paths (ZEB-793)', () => {
  it('surfaces a pending-requests refresh failure in the section error', async () => {
    // Locks the MOUNTED failure path the ZEB-793 guard touches: the `catch`
    // must still surface the error while the panel is alive (the guard only
    // suppresses writes AFTER teardown). Regression guard against over-guarding.
    const listPendingRequests = vi.fn().mockRejectedValue(new Error('pending boom'));
    const service = mockService({ listPendingRequests });
    const { findByTestId } = render(FriendsPanel, { props: { service } });
    const err = await findByTestId('pending-error');
    expect(err.textContent).toContain('pending boom');
  });

  it('does not warn or throw when a pending fetch rejects after unmount', async () => {
    // ZEB-793 teardown smoke test. NOTE: the guard's actual effect — skipping
    // the `pendingError`/`pendingLoading` writes after teardown — has no
    // external observable (Svelte 5 neither throws nor warns on a post-unmount
    // $state write; verified by removing the guard and watching this stay
    // green). That property is covered structurally by parity with the
    // already-merged `refreshOutbound` guard (ZEB-784) and its family teardown
    // test above. This test pins the weaker-but-real property: a late rejection
    // settling after unmount escapes as neither a throw nor an unhandled
    // rejection warning.
    vi.useFakeTimers();
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const error = vi.spyOn(console, 'error').mockImplementation(() => {});
    let rejectPending: (e: unknown) => void = () => {};
    const listPendingRequests = vi.fn().mockReturnValue(
      new Promise((_r, rej) => {
        rejectPending = rej as (e: unknown) => void;
      }),
    );
    const service = mockService({ listPendingRequests });
    const { unmount } = render(FriendsPanel, { props: { service } });
    await vi.advanceTimersByTimeAsync(0);

    // Prove the in-flight fetch actually started before teardown, so the
    // late-rejection path below is genuinely exercised and this can't pass
    // vacuously if a refactor stops calling listPendingRequests on mount.
    expect(listPendingRequests).toHaveBeenCalled();

    // Tear down while the pending-list fetch is in flight, THEN reject it.
    unmount();
    rejectPending(new Error('late boom'));
    await vi.advanceTimersByTimeAsync(0);

    expect(warn).not.toHaveBeenCalled();
    expect(error).not.toHaveBeenCalled();
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
    const cards = mockCards({ [ID('c')]: { displayName: 'CardName' } });
    const { findByTestId, getByTestId } = render(FriendsPanel, {
      props: { service, resolveCard: cards.resolveCard },
    });
    await findByTestId('friend-list');

    expect(getByTestId(`friend-name-${ID('a')}`).textContent).toBe('Nick'); // nickname wins
    expect(getByTestId(`friend-name-${ID('b')}`).textContent).toBe('Hint'); // display hint (no card)
    expect(getByTestId(`friend-name-${ID('c')}`).textContent).toBe('CardName'); // card beats hint
    expect(getByTestId(`friend-name-${ID('d')}`).textContent).toContain('dddddddddddd'); // short-hex
  });

  it('drives the friends bucket with friend + pending owner_ids and clears it on unmount', async () => {
    const friends = [
      { ownerIdHex: ID('a'), display: null, nickname: null, status: 'active', establishedVia: 'mutual_key', referrable: false },
    ];
    const pending = [{ ownerIdHex: ID('b'), display: null, receivedAtMs: 0 }];
    const service = mockService({
      listFriends: vi.fn().mockResolvedValue(friends),
      listPendingRequests: vi.fn().mockResolvedValue(pending),
    });
    const cards = mockCards();
    const { findByTestId, unmount } = render(FriendsPanel, {
      props: { service, resolveCard: cards.resolveCard, setFriendsBucket: cards.setFriendsBucket },
    });
    await findByTestId('friend-list');

    await vi.waitFor(() =>
      expect(cards.setFriendsBucket).toHaveBeenCalledWith(
        expect.arrayContaining([ID('a'), ID('b')]),
      ),
    );

    // ZEB-840: unmount clears ONLY the friends bucket (not unsubscribeAll).
    unmount();
    expect(cards.setFriendsBucket).toHaveBeenLastCalledWith([]);
  });

  it('sets a nickname via the inline editor', async () => {
    const id = 'a'.repeat(32);
    const friends = [
      { ownerIdHex: id, display: null, nickname: null, status: 'active', establishedVia: 'mutual_key', referrable: false },
    ];
    const setNickname = vi.fn().mockResolvedValue(undefined);
    const service = mockService({ listFriends: vi.fn().mockResolvedValue(friends), setNickname });
    const { findByTestId, getByTestId } = render(FriendsPanel, {
      props: { service },
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
      props: { service },
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
    const cards = mockCards({ [id]: { displayName: 'RealCardName', statusText: 'hi' } });
    const onOpenCard = vi.fn();
    const { findByTestId, getByTestId } = render(FriendsPanel, {
      props: { service, resolveCard: cards.resolveCard, onOpenCard },
    });
    await findByTestId('friend-list');

    await fireEvent.click(getByTestId(`friend-identity-${id}`));
    expect(onOpenCard).toHaveBeenCalledTimes(1);
    const payload = onOpenCard.mock.calls[0][0];
    expect(payload.ownerIdHex).toBe(id); // FULL hex, not short
    expect(payload.displayName).toBe('RealCardName'); // the signed card name, NOT 'Nick'
  });
});

describe('FriendsPanel — DM invites pending section (ZEB-236 T7)', () => {
  it('renders a DM-invite row (short inviter hex + kind) and accepts it', async () => {
    const listPending = vi.fn().mockResolvedValue([INVITE]);
    const accept = vi.fn().mockResolvedValue(undefined);
    const dmInviteService = mockDmInviteService({ listPending, accept });
    const { findByTestId, getByTestId } = render(FriendsPanel, {
      props: { service: mockService(), dmInviteService },
    });

    const list = await findByTestId('dm-invite-list');
    // Non-friends have no nickname — the short inviter hex (first 8 chars) shows.
    expect(getByTestId(`dm-invite-inviter-${INVITE.spaceIdHex}`).textContent).toContain('aabbccdd');
    // …alongside the invite kind, rendered as the human label (the 'd' wire
    // tag maps to "DM"), never the raw serde tag.
    expect(list.textContent).toContain('DM');

    await fireEvent.click(getByTestId('dm-invite-accept-btn'));
    expect(accept).toHaveBeenCalledWith(INVITE.spaceIdHex);
    // Accepting refreshes the pending list (initial mount + post-accept).
    await vi.waitFor(() => expect(listPending).toHaveBeenCalledTimes(2));
  });

  it('declines a DM invite and refreshes', async () => {
    // Second listPending resolves [] so the test proves the UI actually
    // re-renders without the declined row, not just that a refresh ran.
    const listPending = vi.fn().mockResolvedValueOnce([INVITE]).mockResolvedValueOnce([]);
    const decline = vi.fn().mockResolvedValue(undefined);
    const dmInviteService = mockDmInviteService({ listPending, decline });
    const { findByTestId, getByTestId, queryByTestId } = render(FriendsPanel, {
      props: { service: mockService(), dmInviteService },
    });

    await findByTestId('dm-invite-list');
    await fireEvent.click(getByTestId('dm-invite-decline-btn'));
    expect(decline).toHaveBeenCalledWith(INVITE.spaceIdHex);
    await vi.waitFor(() => expect(listPending).toHaveBeenCalledTimes(2));
    await vi.waitFor(() => expect(queryByTestId('dm-invites-section')).toBeNull());
  });

  it('renders no DM-invites section when the service prop is absent', async () => {
    const { findByTestId, queryByTestId } = render(FriendsPanel, {
      props: { service: mockService() },
    });
    await findByTestId('friends-panel');
    expect(queryByTestId('dm-invites-section')).toBeNull();
  });

  it('renders no DM-invites section when there are no pending invites', async () => {
    const dmInviteService = mockDmInviteService({
      listPending: vi.fn().mockResolvedValue([]),
    });
    const { findByTestId, queryByTestId } = render(FriendsPanel, {
      props: { service: mockService(), dmInviteService },
    });
    await findByTestId('friends-panel');
    expect(queryByTestId('dm-invites-section')).toBeNull();
  });

  it('surfaces an accept failure in the section error', async () => {
    const listPending = vi.fn().mockResolvedValue([INVITE]);
    const accept = vi.fn().mockRejectedValue(new Error('no pending DM invite for space'));
    const dmInviteService = mockDmInviteService({ listPending, accept });
    const { findByTestId, getByTestId } = render(FriendsPanel, {
      props: { service: mockService(), dmInviteService },
    });

    await findByTestId('dm-invite-list');
    await fireEvent.click(getByTestId('dm-invite-accept-btn'));
    const err = await findByTestId('dm-invite-error');
    expect(err.textContent).toContain('no pending DM invite for space');
  });

  it('guards against double-invoke while accept is in flight', async () => {
    let resolveAccept!: () => void;
    const accept = vi.fn().mockImplementation(
      () => new Promise<void>((r) => { resolveAccept = r; }),
    );
    const listPending = vi.fn().mockResolvedValue([INVITE]);
    const dmInviteService = mockDmInviteService({ listPending, accept });
    const { findByTestId, getByTestId } = render(FriendsPanel, {
      props: { service: mockService(), dmInviteService },
    });

    await findByTestId('dm-invite-list');
    await fireEvent.click(getByTestId('dm-invite-accept-btn'));
    // The in-flight state disables the button (belt)…
    expect((getByTestId('dm-invite-accept-btn') as HTMLButtonElement).disabled).toBe(true);
    // …and the handler's re-entry check drops the call regardless (suspenders).
    await fireEvent.click(getByTestId('dm-invite-accept-btn')); // in-flight
    expect(accept).toHaveBeenCalledTimes(1);
    resolveAccept();
  });
});

describe('FriendsPanel — introduction broker (ZEB-376 Phase 2b)', () => {
  const ID = (b: string) => b.repeat(32); // 16-byte owner_id = 32 hex chars

  it('loads the peer-intro policy and saves a change via the select', async () => {
    const setPeerIntroPolicy = vi.fn().mockResolvedValue(undefined);
    const service = mockService({
      getPeerIntroPolicy: vi.fn().mockResolvedValue('ask'),
      setPeerIntroPolicy,
    });
    const { findByTestId } = render(FriendsPanel, { props: { service } });

    const select = (await findByTestId('peer-intro-policy-select')) as HTMLSelectElement;
    await vi.waitFor(() => expect(select.value).toBe('ask'));

    await fireEvent.change(select, { target: { value: 'closed' } });
    expect(setPeerIntroPolicy).toHaveBeenCalledWith('closed');
    await vi.waitFor(() => expect(select.value).toBe('closed'));
  });

  it('surfaces a peer-intro policy load failure', async () => {
    const service = mockService({
      getPeerIntroPolicy: vi.fn().mockRejectedValue(new Error('boom')),
    });
    const { findByTestId } = render(FriendsPanel, { props: { service } });
    const err = await findByTestId('peer-intro-policy-error');
    expect(err.textContent).toContain('boom');
  });

  it('shows a "Request introduction" button (not the already-friend badge) for a browsable referral and requests it', async () => {
    const via = ID('a');
    const target = ID('b');
    const friends = [
      { ownerIdHex: via, display: 'Alice', status: 'active', establishedVia: 'mutual_key', referrable: true },
    ];
    const browseReferrals = vi.fn().mockResolvedValue([
      { ownerIdHex: target, display: 'Carol', alreadyFriend: false },
    ]);
    const requestIntroduction = vi.fn().mockResolvedValue(undefined);
    const service = mockService({
      listFriends: vi.fn().mockResolvedValue(friends),
      browseReferrals,
      requestIntroduction,
    });
    const { findByTestId, getByTestId, queryByTestId } = render(FriendsPanel, {
      props: { service },
    });
    await findByTestId('friend-list');

    await fireEvent.click(getByTestId('browse-referrals-btn'));
    const btn = await findByTestId('request-intro-btn');
    expect(queryByTestId('already-friend-badge')).toBeNull();

    await fireEvent.click(btn);
    expect(requestIntroduction).toHaveBeenCalledWith(via, target);
    const status = await findByTestId('request-intro-status');
    expect(status.textContent).toContain('Introduction requested.');
  });

  it('shows only the already-friend badge (no button) when the referral is already a friend', async () => {
    const via = ID('a');
    const target = ID('c');
    const friends = [
      { ownerIdHex: via, display: 'Alice', status: 'active', establishedVia: 'mutual_key', referrable: true },
    ];
    const browseReferrals = vi.fn().mockResolvedValue([
      { ownerIdHex: target, display: 'Dave', alreadyFriend: true },
    ]);
    const service = mockService({
      listFriends: vi.fn().mockResolvedValue(friends),
      browseReferrals,
    });
    const { findByTestId, getByTestId, queryByTestId } = render(FriendsPanel, {
      props: { service },
    });
    await findByTestId('friend-list');

    await fireEvent.click(getByTestId('browse-referrals-btn'));
    await findByTestId('already-friend-badge');
    expect(queryByTestId('request-intro-btn')).toBeNull();
  });

  it('badges a pending request with its introducer when introducedBy is set', async () => {
    const requesterId = ID('d');
    const voucherId = ID('e');
    const pending = [
      { ownerIdHex: requesterId, display: 'Eve', receivedAtMs: 0, introducedBy: voucherId },
    ];
    const service = mockService({ listPendingRequests: vi.fn().mockResolvedValue(pending) });
    const { findByTestId } = render(FriendsPanel, { props: { service } });

    const badge = await findByTestId(`introduced-by-badge-${requesterId}`);
    expect(badge.textContent).toContain('introduced by');
    expect(badge.textContent).toContain(voucherId.slice(0, 12));
  });

  it('renders no introducer badge for a plain (non-introduction) pending request', async () => {
    const requesterId = ID('f');
    const pending = [
      { ownerIdHex: requesterId, display: 'Frank', receivedAtMs: 0, introducedBy: null },
    ];
    const service = mockService({ listPendingRequests: vi.fn().mockResolvedValue(pending) });
    const { findByTestId, queryByTestId } = render(FriendsPanel, { props: { service } });

    await findByTestId('pending-list');
    expect(queryByTestId(`introduced-by-badge-${requesterId}`)).toBeNull();
  });
});

describe('FriendsPanel — friend request accept (ZEB-694 Task B5)', () => {
  const ownerIdHex = 'a1'.repeat(16); // 16-byte owner_id = 32 hex chars

  const REQ = { ownerIdHex, display: 'Alice', receivedAtMs: 0, introducedBy: null };

  it('keeps the pending row and surfaces the backend message when accept is rejected (not linked)', async () => {
    const listPendingRequests = vi.fn().mockResolvedValue([REQ]);
    const acceptRequest = vi
      .fn()
      .mockRejectedValue(
        new Error(
          "Couldn't reach them right now — the introduction is saved, try Accept again later.",
        ),
      );
    const service = mockService({ listPendingRequests, acceptRequest });
    const { findByTestId, getByTestId } = render(FriendsPanel, { props: { service } });

    await findByTestId('pending-list');
    await fireEvent.click(getByTestId('accept-btn'));

    // The backend rejection message surfaces near the requests inbox…
    const err = await findByTestId('pending-error');
    expect(err.textContent).toContain('introduction is saved');

    // …and the row is NOT optimistically removed — a non-linked accept leaves
    // the staged offer in place backend-side, so the row must only disappear
    // once a fresh `list_pending_friend_requests` (via `friend-list-changed`)
    // actually drops it. `listPendingRequests` should still have been called
    // only once (the initial mount fetch) — no refetch was triggered by the
    // failure, so nothing could have removed the row.
    expect(listPendingRequests).toHaveBeenCalledTimes(1);
    expect(getByTestId('pending-list')).toBeTruthy();
    expect(getByTestId(`friend-name-${ownerIdHex}`).textContent).toContain('Alice');
  });

  it('accepts successfully, clears any prior error, and refreshes the pending list', async () => {
    // ZEB-694 CR#10: the first accept REJECTS (so pendingError is genuinely set),
    // then a retry RESOLVES — proving handleAccept's success path clears the
    // stale error rather than the test trivially passing with no error ever set.
    const listPendingRequests = vi
      .fn()
      .mockResolvedValueOnce([REQ])
      .mockResolvedValueOnce([]);
    const acceptRequest = vi
      .fn()
      .mockRejectedValueOnce(new Error('Introduction not delivered — try again.'))
      .mockResolvedValueOnce(undefined);
    const service = mockService({ listPendingRequests, acceptRequest });
    const { findByTestId, getByTestId, queryByTestId } = render(FriendsPanel, {
      props: { service },
    });

    await findByTestId('pending-list');
    await fireEvent.click(getByTestId('accept-btn'));

    const err = await findByTestId('pending-error');
    expect(err.textContent).toContain('Introduction not delivered');
    expect(listPendingRequests).toHaveBeenCalledTimes(1);

    await fireEvent.click(getByTestId('accept-btn'));
    expect(acceptRequest).toHaveBeenCalledWith(ownerIdHex);
    expect(acceptRequest).toHaveBeenCalledTimes(2);

    await vi.waitFor(() => expect(listPendingRequests).toHaveBeenCalledTimes(2));
    await vi.waitFor(() => expect(queryByTestId('pending-error')).toBeNull());
    expect(queryByTestId(`friend-name-${ownerIdHex}`)).toBeNull();
  });
});

// ── ZEB-960: the label ladders here use `||` (which already drops "") but a
// whitespace-only name is truthy and survives — masking a valid card name and
// rendering a blank label. nonEmpty() closes that hole on every rung. ──
describe('FriendsPanel — ZEB-960 whitespace name ladder', () => {
  const ID = (b: string) => b.repeat(32); // 16-byte owner_id = 32 hex chars

  it('a whitespace nickname no longer masks the card name', async () => {
    const id = ID('a');
    const friends = [
      { ownerIdHex: id, display: null, nickname: '   ', status: 'active', establishedVia: 'mutual_key', referrable: false },
    ];
    const service = mockService({ listFriends: vi.fn().mockResolvedValue(friends) });
    const cards = mockCards({ [id]: { displayName: 'CardName' } });
    const { findByTestId, getByTestId } = render(FriendsPanel, {
      props: { service, resolveCard: cards.resolveCard },
    });
    await findByTestId('friend-list');
    expect(getByTestId(`friend-name-${id}`).textContent).toBe('CardName');
  });

  it('a whitespace display hint with no card falls to the short hex, not blank', async () => {
    const id = ID('b');
    const friends = [
      { ownerIdHex: id, display: '   ', nickname: null, status: 'active', establishedVia: 'token', referrable: false },
    ];
    const service = mockService({ listFriends: vi.fn().mockResolvedValue(friends) });
    const { findByTestId, getByTestId } = render(FriendsPanel, { props: { service } });
    await findByTestId('friend-list');
    expect(getByTestId(`friend-name-${id}`).textContent).toContain('bbbbbbbbbbbb');
  });

  it('a pending request with a whitespace display hint falls to the short hex', async () => {
    const id = ID('c');
    const pending = [{ ownerIdHex: id, display: '   ', receivedAtMs: 0, introducedBy: null }];
    const service = mockService({ listPendingRequests: vi.fn().mockResolvedValue(pending) });
    const { findByTestId, getByTestId } = render(FriendsPanel, { props: { service } });
    await findByTestId('pending-list');
    expect(getByTestId(`friend-name-${id}`).textContent).toContain('cccccccccccc');
  });

  it('a referral with a whitespace display shows the short hex, not blank', async () => {
    const via = ID('a');
    const target = ID('d');
    const friends = [
      { ownerIdHex: via, display: 'Alice', status: 'active', establishedVia: 'mutual_key', referrable: true },
    ];
    const browseReferrals = vi.fn().mockResolvedValue([
      { ownerIdHex: target, display: '   ', alreadyFriend: false },
    ]);
    const service = mockService({
      listFriends: vi.fn().mockResolvedValue(friends),
      browseReferrals,
    });
    const { findByTestId, getByTestId } = render(FriendsPanel, { props: { service } });
    await findByTestId('friend-list');
    await fireEvent.click(getByTestId('browse-referrals-btn'));
    const list = await findByTestId('referrals-list');
    expect(list.textContent).toContain('dddddddddddd');
  });

  it('the add-by-key connected toast falls to the short hex for a whitespace display', async () => {
    const PEER = 'ab'.repeat(64); // 128 hex chars
    const linkedId = ID('e');
    const addByKey = vi
      .fn()
      .mockResolvedValue({ kind: 'linked', ownerIdHex: linkedId, display: '   ' });
    const service = mockService({ addByKey });
    const { getByTestId, findByTestId } = render(FriendsPanel, { props: { service } });
    await findByTestId('friends-panel');

    await fireEvent.input(getByTestId('add-by-key-input'), { target: { value: PEER } });
    await fireEvent.click(getByTestId('add-by-key-btn'));
    await vi.waitFor(() =>
      expect(getByTestId('add-by-key-status').textContent).toContain('eeeeeeeeeeee'),
    );
  });
});
