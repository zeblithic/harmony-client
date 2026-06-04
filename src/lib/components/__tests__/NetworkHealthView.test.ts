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
    schemaVersion: 1,
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
      recent: [
        {
          nodeIdShort: 'dead5678',
          ownerShort: 'cc22dd33',
          outcome: 'failed',
          capturedAtMs: now - 41000,
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
      recent: [],
    },
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
    const hits = screen.getAllByTestId('nh-dial-hit');
    expect(hits.length).toBe(2);
    // Newest-first: the 8s-ago succeeded hit must sort above the 41s-ago one.
    expect(hits[0].textContent).toContain('a3f9e1c2');
  });

  it('renders idle dial state when there are no recent hits', async () => {
    mockInvoke.mockResolvedValue(readySnapIdleDials());
    render(NetworkHealthView);
    await waitFor(() => screen.getByTestId('nh-dial-empty'));
    expect(screen.getByTestId('nh-dial-empty')).toBeTruthy();
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
