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
