import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, waitFor, fireEvent } from '@testing-library/svelte';

// Mock the IPC layer BEFORE importing the component module. vi.mock calls
// are hoisted by vitest, so this runs ahead of the static `import` below.
// Matches the pattern used by sibling component tests
// (PendingJoinsPanel.test.ts, MintLedger.event.test.ts).
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import { invoke } from '@tauri-apps/api/core';
import DiagnosticsPanel from './DiagnosticsPanel.svelte';

const mockInvoke = invoke as unknown as ReturnType<typeof vi.fn>;

// `import.meta.env.DEV` is read once at component module-evaluation
// time into `const isDevMode`. Because the component module is loaded
// statically (one-time evaluation per test-file run), and vitest sets
// `DEV=true` by default for the jsdom environment, the panel's
// isDevMode evaluates to true for every test in this file.
//
// The dev-mode tests below directly exercise the panel's IPC fan-out
// and rendering. The production-mode tree-shaking guarantee is
// asserted separately by the unit test that confirms the `{#if
// isDevMode}` guard exists AND by the architectural fact that Vite's
// `import.meta.env.DEV` is a build-time constant: a `vite build`
// production bundle replaces it with the literal `false`, and the
// dead-code branch (plus its onMount IPC fetches) is tree-shaken out.
// We verify this static guard exists via a source-text check rather
// than runtime stubbing — see `respects the isDevMode guard in source`
// below — because runtime-stubbing `import.meta.env.DEV` AFTER the
// component module has already evaluated has no effect on a `const`
// captured at module-eval time, and stubbing BEFORE module-eval would
// require a dynamic `await import(...)` which (under Svelte 5.53 +
// vitest 4) triggers an `effect_orphan` regression in keyed-each
// rendering of post-mount-populated `$state` arrays.

describe('DiagnosticsPanel', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  afterEach(() => {
    vi.unstubAllEnvs();
  });

  it('respects the isDevMode guard in source (production no-op)', async () => {
    // Source-text assertion of the build-time tree-shaking guarantee.
    // In a production `vite build`, `import.meta.env.DEV` is replaced
    // with the literal `false`, the `{#if isDevMode}` block is dead
    // code, and the `onMount` body's `if (!isDevMode) return` early-
    // outs before any IPC call. This test pins those source-level
    // invariants so a future refactor that drops either guard will
    // surface immediately.
    const fs = await import('node:fs');
    const path = await import('node:path');
    const src = fs.readFileSync(
      path.resolve(__dirname, 'DiagnosticsPanel.svelte'),
      'utf8',
    );
    expect(src).toMatch(/import\.meta\.env\.DEV/);
    expect(src).toMatch(/\{#if isDevMode\}/);
    expect(src).toMatch(/if \(!isDevMode\) return/);
  });

  it('renders my-node section when backend returns a record', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_my_reachability_record') {
        return Promise.resolve({
          irohNodeId: 'ab'.repeat(32),
          homeRelayUrl: 'https://derp.example/',
          directAddresses: ['10.0.0.1:4242'],
          announcedAtMs: 0,
        });
      }
      if (cmd === 'connectivity_list_peer_reachability') {
        return Promise.resolve([]);
      }
      return Promise.resolve(null);
    });

    const { findByTestId } = render(DiagnosticsPanel);
    const nodeId = await findByTestId('diag-my-node-id');
    expect(nodeId.textContent).toMatch(/^abab/);

    const relay = await findByTestId('diag-my-relay');
    expect(relay.textContent).toBe('https://derp.example/');

    const direct = await findByTestId('diag-my-direct');
    expect(direct.textContent).toBe('10.0.0.1:4242');
  });

  it('renders empty-state when iroh endpoint is not ready', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_my_reachability_record') return Promise.resolve(null);
      if (cmd === 'connectivity_list_peer_reachability') return Promise.resolve([]);
      return Promise.resolve(null);
    });

    const { findByTestId } = render(DiagnosticsPanel);
    const empty = await findByTestId('diag-my-empty');
    expect(empty.textContent).toMatch(/not ready/i);
  });

  it('renders known-peers list with truncated identifiers', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_my_reachability_record') return Promise.resolve(null);
      if (cmd === 'connectivity_list_peer_reachability') {
        return Promise.resolve([
          {
            ownerAddress: 'aa'.repeat(16),
            record: {
              irohNodeId: 'bb'.repeat(32),
              homeRelayUrl: '',
              directAddresses: [],
              announcedAtMs: 1_700_000_000_000,
            },
          },
          {
            ownerAddress: 'cc'.repeat(16),
            record: {
              irohNodeId: 'dd'.repeat(32),
              homeRelayUrl: '',
              directAddresses: [],
              announcedAtMs: 1_700_000_001_000,
            },
          },
        ]);
      }
      return Promise.resolve(null);
    });

    const { container } = render(DiagnosticsPanel);
    await waitFor(() => {
      const peers = container.querySelectorAll('[data-testid="diag-peer"]');
      expect(peers.length).toBe(2);
    });
    const peers = container.querySelectorAll('[data-testid="diag-peer"]');
    // 32-char ownerAddress (hex of 16 bytes) → truncated to 12 chars in UI.
    expect(peers[0].textContent).toContain('aaaaaaaaaaaa');
    expect(peers[1].textContent).toContain('cccccccccccc');
  });

  it('renders error surface when IPC rejects', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_my_reachability_record') {
        return Promise.reject(new Error('iroh poisoned'));
      }
      return Promise.resolve([]);
    });

    const { findByTestId } = render(DiagnosticsPanel);
    const err = await findByTestId('diag-error');
    // Adapter wraps the message with the IPC-name prefix; the panel
    // just stringifies whatever the adapter throws.
    expect(err.textContent).toContain('connectivity_get_my_reachability_record');
    expect(err.textContent).toContain('iroh poisoned');
  });

  it('calls invoke for both my-record and peer-list on mount', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_my_reachability_record') return Promise.resolve(null);
      if (cmd === 'connectivity_list_peer_reachability') return Promise.resolve([]);
      return Promise.resolve(null);
    });

    render(DiagnosticsPanel);
    await waitFor(() => {
      expect(mockInvoke).toHaveBeenCalledWith('connectivity_get_my_reachability_record');
      expect(mockInvoke).toHaveBeenCalledWith('connectivity_list_peer_reachability');
    });
  });

  // ── ZEB-323 Phase 2b: pkarr section tests ─────────────────────────────────

  it('renders pkarr section toggle', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_my_reachability_record') return Promise.resolve(null);
      if (cmd === 'connectivity_list_peer_reachability') return Promise.resolve([]);
      if (cmd === 'connectivity_pkarr_publication_status') {
        return Promise.resolve({ inviteCount: 0, identityActive: false, communityCount: 0 });
      }
      return Promise.resolve(null);
    });

    const { findByTestId } = render(DiagnosticsPanel);
    const toggle = await findByTestId('pkarr-section-toggle');
    expect(toggle).toBeTruthy();
  });

  it('pkarr content is hidden by default (collapsed)', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_my_reachability_record') return Promise.resolve(null);
      if (cmd === 'connectivity_list_peer_reachability') return Promise.resolve([]);
      if (cmd === 'connectivity_pkarr_publication_status') {
        return Promise.resolve({ inviteCount: 0, identityActive: false, communityCount: 0 });
      }
      return Promise.resolve(null);
    });

    const { container } = render(DiagnosticsPanel);
    await waitFor(() => {
      // Wait for mount to finish.
      expect(mockInvoke).toHaveBeenCalledWith('connectivity_pkarr_publication_status');
    });
    // Content div should not be present while collapsed.
    expect(container.querySelector('[data-testid="pkarr-content"]')).toBeNull();
  });

  it('expands pkarr section on toggle click and displays status counts', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_my_reachability_record') return Promise.resolve(null);
      if (cmd === 'connectivity_list_peer_reachability') return Promise.resolve([]);
      if (cmd === 'connectivity_pkarr_publication_status') {
        return Promise.resolve({ inviteCount: 2, identityActive: true, communityCount: 3 });
      }
      return Promise.resolve(null);
    });

    const { findByTestId } = render(DiagnosticsPanel);

    const toggle = await findByTestId('pkarr-section-toggle');
    await fireEvent.click(toggle);

    const inviteCount = await findByTestId('pkarr-invite-count');
    expect(inviteCount.textContent?.trim()).toBe('2');

    const identityActive = await findByTestId('pkarr-identity-active');
    expect(identityActive.textContent?.trim()).toBe('Active');

    const communityCount = await findByTestId('pkarr-community-count');
    expect(communityCount.textContent?.trim()).toBe('3');
  });

  it('shows "not initialized" when pkarr_publication_status rejects', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_my_reachability_record') return Promise.resolve(null);
      if (cmd === 'connectivity_list_peer_reachability') return Promise.resolve([]);
      if (cmd === 'connectivity_pkarr_publication_status') {
        return Promise.reject(new Error('pkarr publisher not initialized'));
      }
      return Promise.resolve(null);
    });

    const { findByTestId } = render(DiagnosticsPanel);

    const toggle = await findByTestId('pkarr-section-toggle');
    await fireEvent.click(toggle);

    const notReady = await findByTestId('pkarr-not-ready');
    expect(notReady.textContent).toContain('not initialized');
  });

  it('shows empty fallback log by default', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_my_reachability_record') return Promise.resolve(null);
      if (cmd === 'connectivity_list_peer_reachability') return Promise.resolve([]);
      if (cmd === 'connectivity_pkarr_publication_status') {
        return Promise.resolve({ inviteCount: 0, identityActive: false, communityCount: 0 });
      }
      return Promise.resolve(null);
    });

    const { findByTestId } = render(DiagnosticsPanel);

    const toggle = await findByTestId('pkarr-section-toggle');
    await fireEvent.click(toggle);

    const empty = await findByTestId('pkarr-fallback-empty');
    expect(empty.textContent).toContain('No fallback lookups yet');
  });

  it('calls connectivity_pkarr_publication_status on mount', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_my_reachability_record') return Promise.resolve(null);
      if (cmd === 'connectivity_list_peer_reachability') return Promise.resolve([]);
      if (cmd === 'connectivity_pkarr_publication_status') {
        return Promise.resolve({ inviteCount: 0, identityActive: false, communityCount: 0 });
      }
      return Promise.resolve(null);
    });

    render(DiagnosticsPanel);

    await waitFor(() => {
      expect(mockInvoke).toHaveBeenCalledWith('connectivity_pkarr_publication_status');
    });
  });
});
