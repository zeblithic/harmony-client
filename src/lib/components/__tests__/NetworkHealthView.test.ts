import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

// Hoisted IPC mocks. vi.mock() is hoisted ahead of the static imports
// below, mirroring DiagnosticsPanel.test.ts and the other component tests
// in this folder.
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import { invoke } from '@tauri-apps/api/core';
import NetworkHealthView from '../NetworkHealthView.svelte';
import type { NetworkHealthSnapshot } from '../../types/network-health';

const mockInvoke = invoke as unknown as ReturnType<typeof vi.fn>;

function emptySnap(): NetworkHealthSnapshot {
  return {
    schemaVersion: 3,
    capturedAtMs: 0,
    appVersion: 'test',
    platform: 'test',
    myNetwork: null,
    peers: [],
    pkarrStatus: {
      identityPublished: false,
      identityLastPublishMs: null,
      communityPublishCount: 0,
      recentFallbackEvents: [],
      relays: [],
    },
  };
}

function readySnap(): NetworkHealthSnapshot {
  return {
    ...emptySnap(),
    myNetwork: {
      irohNodeId: 'a3f9e1c2'.repeat(8),
      reachability: 'reachable',
      natClassification: 'fullCone',
      homeRelayUrl: 'https://derp.example/',
      relayRttMs: 24,
      directAddresses: [],
    },
  };
}

// ZEB-377: snapshot carrying ZEB-373 dial telemetry. The `recent` ring is
// intentionally NOT newest-first so the view's sort (capturedAtMs descending)
// is actually exercised.
function readySnapWithDials(): NetworkHealthSnapshot {
  const now = Date.now();
  return {
    ...readySnap(),
    dialStatus: {
      attempts: 3,
      succeeded: 2,
      failed: 1,
      skippedDuplicate: 0,
      // ZEB-620/622: live per-peer-state counts (distinct values so the
      // summary-row assertions below can pin each field).
      connected: 5,
      retrying: 2,
      dormant: 1,
      recent: [
        {
          nodeIdShort: 'dead5678',
          ownerShort: 'cc22dd33',
          outcome: 'failed',
          capturedAtMs: now - 41000,
        },
        // ZEB-622: a reconnect-supervisor transition marker in the ring.
        {
          nodeIdShort: 'beef9012',
          ownerShort: 'aa11bb22',
          outcome: 'reconnected',
          capturedAtMs: now - 20000,
        },
        {
          nodeIdShort: 'a3f9e1c2',
          ownerShort: '4b2c0011',
          outcome: 'succeeded',
          capturedAtMs: now - 8000,
        },
      ],
    },
  };
}

// ZEB-377: dial telemetry present but no dials have happened (steady idle
// state — the surface must still render with an explicit empty message).
function readySnapIdleDials(): NetworkHealthSnapshot {
  return {
    ...readySnap(),
    dialStatus: {
      attempts: 0,
      succeeded: 0,
      failed: 0,
      skippedDuplicate: 0,
      connected: 0,
      retrying: 0,
      dormant: 0,
      recent: [],
    },
  };
}

// ZEB-622: a snapshot with a `degraded` peer (link up, no selected path yet)
// so the panel's shared-⚠ / distinct-title handling is exercised.
function readySnapWithDegradedPeer(): NetworkHealthSnapshot {
  return {
    ...readySnap(),
    peers: [
      {
        ownerAddr: 'f00dbabe'.repeat(8),
        displayName: null,
        sharedCommunities: ['c0ffee'],
        connectionMode: 'degraded',
        rttMs: null,
        lastSeenMs: null,
        reachabilityRecordAgeMs: null,
      },
    ],
  };
}

describe('NetworkHealthView', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders "starting up…" when my_network is null', async () => {
    mockInvoke.mockResolvedValue(emptySnap());
    render(NetworkHealthView);
    await waitFor(() => screen.getByTestId('nh-starting-up'));
    expect(screen.getByTestId('nh-starting-up')).toBeTruthy();
  });

  it('renders pkarr-relays section during startup (myNetwork null) — Cursor round-4 Medium fix', async () => {
    // Snapshot with myNetwork=null but a populated relay list — the relay
    // health section must be visible while the network is still starting up.
    const snap: NetworkHealthSnapshot = {
      ...emptySnap(),
      myNetwork: null,
      pkarrStatus: {
        identityPublished: false,
        identityLastPublishMs: null,
        communityPublishCount: 0,
        recentFallbackEvents: [],
        relays: [
          {
            url: 'https://relay.pkarr.org',
            state: { kind: 'healthy' },
            lastOutcome: null,
            lastSuccessMs: null,
          },
        ],
      },
    };
    mockInvoke.mockResolvedValue(snap);
    render(NetworkHealthView);

    // Starting-up banner must still appear (myNetwork is null).
    await waitFor(() => screen.getByTestId('nh-starting-up'));

    // AND the pkarr-relays section must also be visible with its relay row.
    expect(screen.getByTestId('nh-pkarr-relays')).toBeTruthy();
    expect(screen.getByTestId('nh-relay-row')).toBeTruthy();
  });

  it('renders summary card when my_network is populated', async () => {
    mockInvoke.mockResolvedValue(readySnap());
    render(NetworkHealthView);
    await waitFor(() => screen.getByTestId('nh-my-network'));
    expect(screen.getByTestId('nh-headline').textContent).toContain(
      'Direct connections work',
    );
  });

  it('renders empty-peer state when peers list is empty', async () => {
    mockInvoke.mockResolvedValue(readySnap());
    render(NetworkHealthView);
    await waitFor(() => screen.getByTestId('nh-peers-empty'));
    expect(screen.getByTestId('nh-peers-empty')).toBeTruthy();
  });

  it('renders dial counters and recent hits when dialStatus is present', async () => {
    mockInvoke.mockResolvedValue(readySnapWithDials());
    render(NetworkHealthView);
    await waitFor(() => screen.getByTestId('nh-dynamic-dials'));
    expect(screen.getByTestId('nh-dial-attempts').textContent).toContain('3');
    expect(screen.getByTestId('nh-dial-succeeded').textContent).toContain('2');
    expect(screen.getByTestId('nh-dial-failed').textContent).toContain('1');
    // ZEB-620/622: the per-peer-state summary row surfaces the supervisor tally.
    const states = screen.getByTestId('nh-dial-peer-states');
    expect(states.textContent).toContain('5');
    expect(states.textContent).toContain('connected');
    expect(states.textContent).toContain('2');
    expect(states.textContent).toContain('retrying');
    expect(states.textContent).toContain('1');
    expect(states.textContent).toContain('dormant');
    const hits = screen.getAllByTestId('nh-dial-hit');
    expect(hits.length).toBe(3);
    // Newest-first: the 8s-ago succeeded hit sorts above the 20s reconnected
    // and 41s failed hits.
    expect(hits[0].textContent).toContain('a3f9e1c2');
    // ZEB-622: the reconnected marker renders its ↻ icon (not the ✓/✗ pair).
    const reconnected = hits.find((h) => h.textContent?.includes('beef9012'));
    expect(reconnected?.textContent).toContain('↻');
  });

  it('renders a degraded peer with the ⚠ icon and its own title (ZEB-622)', async () => {
    mockInvoke.mockResolvedValue(readySnapWithDegradedPeer());
    render(NetworkHealthView);
    await waitFor(() => screen.getByTestId('nh-peer'));
    const icon = screen.getByTestId('nh-peer-icon');
    // Shares the warn glyph with relay, but its title disambiguates the state.
    expect(icon.textContent).toContain('⚠');
    expect(icon.getAttribute('title')).toContain('degraded');
    // The mode label still reads "degraded" in the row body.
    expect(screen.getByTestId('nh-peer').textContent).toContain('degraded');
  });

  it('renders idle dial state when there are no recent hits', async () => {
    mockInvoke.mockResolvedValue(readySnapIdleDials());
    render(NetworkHealthView);
    await waitFor(() => screen.getByTestId('nh-dial-empty'));
    expect(screen.getByTestId('nh-dial-empty')).toBeTruthy();
  });

  it('renders a Cooling down badge for a coolingDown relay', async () => {
    const snap: NetworkHealthSnapshot = {
      ...readySnap(),
      pkarrStatus: {
        identityPublished: false,
        identityLastPublishMs: null,
        communityPublishCount: 0,
        recentFallbackEvents: [],
        relays: [
          {
            url: 'https://relay.pkarr.org',
            // untilMs far in the future so seconds count is well above 0
            state: { kind: 'coolingDown', untilMs: Date.now() + 90_000 },
            lastOutcome: null,
            lastSuccessMs: null,
          },
        ],
      },
    };
    mockInvoke.mockResolvedValue(snap);
    render(NetworkHealthView);
    await waitFor(() => screen.getByTestId('nh-relay-badge'));
    const badge = screen.getByTestId('nh-relay-badge');
    expect(badge.textContent).toContain('Cooling down');
    // ZEB-384: the countdown must be a real number, not NaN (the wire field is
    // `untilMs`; a snake_case `until_ms` regression renders `(NaNs)`).
    expect(badge.textContent).toMatch(/Cooling down \(\d+s\)/);
    expect(badge.textContent).not.toContain('NaN');
  });

  it('renders Last error: http 503 for a relay with an http outcome', async () => {
    const snap: NetworkHealthSnapshot = {
      ...readySnap(),
      pkarrStatus: {
        identityPublished: false,
        identityLastPublishMs: null,
        communityPublishCount: 0,
        recentFallbackEvents: [],
        relays: [
          {
            url: 'https://relay.pkarr.org',
            state: { kind: 'healthy' },
            lastOutcome: { kind: 'http', status: 503 },
            lastSuccessMs: null,
          },
        ],
      },
    };
    mockInvoke.mockResolvedValue(snap);
    render(NetworkHealthView);
    await waitFor(() => screen.getByTestId('nh-relay-last-error'));
    const errEl = screen.getByTestId('nh-relay-last-error');
    expect(errEl.textContent).toContain('Last error: http 503');
  });

  it('self-test button disables while running', async () => {
    mockInvoke
      .mockResolvedValueOnce(readySnap()) // snapshot
      .mockImplementationOnce(
        () =>
          new Promise(() => {
            /* never resolves — keeps self-test in-flight */
          }),
      ); // runSelfTest hangs
    render(NetworkHealthView);
    await waitFor(() => screen.getByTestId('nh-self-test-button'));
    const btn = screen.getByTestId('nh-self-test-button') as HTMLButtonElement;
    await fireEvent.click(btn);
    await waitFor(() => expect(btn.disabled).toBe(true));
  });
});

describe('NetworkHealthView — transport-disabled banner (ZEB-450)', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  const REASON =
    'iroh transport unavailable this session: no keychain available and ' +
    'HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set';

  it('renders the loud banner with the boot reason when transport is disabled this session', async () => {
    mockInvoke.mockResolvedValue({ ...emptySnap(), transportDisabledReason: REASON });
    render(NetworkHealthView);

    const banner = await screen.findByTestId('nh-transport-disabled');
    expect(banner.getAttribute('role')).toBe('alert');
    expect(screen.getByTestId('nh-transport-disabled-reason').textContent).toContain(REASON);
    // The banner REPLACES the bland "starting up…" placeholder — transport
    // won't recover without a restart, so the auto-retry spinner must not show.
    expect(screen.queryByTestId('nh-starting-up')).toBeNull();
  });

  it('shows "starting up…" (not the banner) when no transport reason is set', async () => {
    mockInvoke.mockResolvedValue(emptySnap()); // transportDisabledReason absent
    render(NetworkHealthView);

    await waitFor(() => screen.getByTestId('nh-starting-up'));
    expect(screen.queryByTestId('nh-transport-disabled')).toBeNull();
  });

  it('offers a "Check again" button that re-fetches and clears the banner on recovery (Qodo)', async () => {
    // Auto-retry is suppressed while disabled, so the banner must still give the
    // user a way to pick up a recovery — otherwise the view can stale on the
    // "can't network" state after a restart fixes transport.
    mockInvoke
      .mockResolvedValueOnce({ ...emptySnap(), transportDisabledReason: REASON }) // onMount: disabled
      .mockResolvedValueOnce(readySnap()); // recheck: transport recovered
    render(NetworkHealthView);

    const btn = await screen.findByTestId('nh-transport-recheck');
    await fireEvent.click(btn);

    await waitFor(() => expect(screen.queryByTestId('nh-transport-disabled')).toBeNull());
    expect(screen.getByTestId('nh-my-network')).toBeTruthy();
  });
});
