import { render, screen, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('../../network-health-adapter', () => ({
  snapshot: vi.fn(),
  onNetworkHealthChanged: vi.fn(async () => () => {}),
}));

import ConnectionStatusChip from '../ConnectionStatusChip.svelte';
import { snapshot } from '../../network-health-adapter';
import type { NetworkHealthSnapshot, PeerHealth } from '../../types/network-health';

function makePeer(connectionMode: PeerHealth['connectionMode']): PeerHealth {
  return {
    ownerAddr: 'aa'.repeat(16),
    displayName: null,
    sharedCommunities: [],
    connectionMode,
    rttMs: null,
    lastSeenMs: null,
    reachabilityRecordAgeMs: null,
    protocolIncompatReason: null,
  };
}

function makeSnap(overrides: Partial<NetworkHealthSnapshot>): NetworkHealthSnapshot {
  return {
    schemaVersion: 4,
    capturedAtMs: 0,
    appVersion: 'test',
    platform: 'test',
    myNetwork: {
      irohNodeId: 'node',
      reachability: 'reachable',
      natClassification: 'unknown',
      homeRelayUrl: null,
      relayRttMs: null,
      directAddresses: [],
    },
    peers: [],
    pkarrStatus: {} as NetworkHealthSnapshot['pkarrStatus'],
    ...overrides,
  };
}

beforeEach(() => {
  vi.mocked(snapshot).mockReset();
});

describe('ConnectionStatusChip (ZEB-606)', () => {
  it('shows connected with the count of connected peers only', async () => {
    vi.mocked(snapshot).mockResolvedValue(
      makeSnap({ peers: [makePeer('direct'), makePeer('relay'), makePeer('noConnection')] }),
    );
    render(ConnectionStatusChip);
    await waitFor(() => expect(screen.getByText('● connected · 2 peers')).toBeTruthy());
  });

  it('singularizes one peer', async () => {
    vi.mocked(snapshot).mockResolvedValue(makeSnap({ peers: [makePeer('direct')] }));
    render(ConnectionStatusChip);
    await waitFor(() => expect(screen.getByText('● connected · 1 peer')).toBeTruthy());
  });

  it('shows degraded when reachability is degraded', async () => {
    vi.mocked(snapshot).mockResolvedValue(
      makeSnap({
        myNetwork: {
          irohNodeId: 'node',
          reachability: 'degraded',
          natClassification: 'unknown',
          homeRelayUrl: null,
          relayRttMs: null,
          directAddresses: [],
        },
        peers: [makePeer('degraded')],
      }),
    );
    render(ConnectionStatusChip);
    await waitFor(() => expect(screen.getByText('● degraded · 1 peer')).toBeTruthy());
  });

  it('shows offline with a tooltip when the transport is disabled', async () => {
    vi.mocked(snapshot).mockResolvedValue(
      makeSnap({ transportDisabledReason: 'keychain unavailable' }),
    );
    render(ConnectionStatusChip);
    await waitFor(() => expect(screen.getByText('● offline')).toBeTruthy());
    expect(screen.getByText('● offline').getAttribute('title')).toBe('keychain unavailable');
  });

  it('renders nothing while initializing (myNetwork null, transport up)', async () => {
    vi.mocked(snapshot).mockResolvedValue(makeSnap({ myNetwork: null }));
    const { container } = render(ConnectionStatusChip);
    await new Promise((r) => setTimeout(r, 0));
    expect(container.querySelector('.status-chip')).toBeNull();
  });

  it('renders nothing when the snapshot IPC rejects (boot window)', async () => {
    vi.mocked(snapshot).mockRejectedValue('ipc not ready');
    const { container } = render(ConnectionStatusChip);
    await new Promise((r) => setTimeout(r, 0));
    expect(container.querySelector('.status-chip')).toBeNull();
  });
});
