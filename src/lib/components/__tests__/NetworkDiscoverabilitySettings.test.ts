import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

// Mock Tauri IPC and event layers before any module evaluation.
// The connectivity-adapter calls invoke() and listen() from these packages.
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import NetworkDiscoverabilitySettings from '../NetworkDiscoverabilitySettings.svelte';
import type { RelayHealth } from '../../types/network-health';

const mockInvoke = invoke as unknown as ReturnType<typeof vi.fn>;

// Default relay list matching Rust default_relays() — must stay in sync.
const DEFAULT_RELAYS = ['https://relay.pkarr.org', 'https://pkarr.pubky.app'];

function makeRelay(url: string, healthy = true): RelayHealth {
  return {
    url,
    state: healthy ? { kind: 'healthy' } : { kind: 'coolingDown', untilMs: Date.now() + 30_000 },
    lastOutcome: null,
    lastSuccessMs: null,
  };
}

// Convenience: set up mockInvoke to handle all commands used by the component.
function setupDefaultMocks(
  relayList: RelayHealth[] = DEFAULT_RELAYS.map((u) => makeRelay(u)),
): void {
  mockInvoke.mockImplementation((cmd: string) => {
    if (cmd === 'connectivity_get_identity_discoverable') return Promise.resolve(false);
    if (cmd === 'get_pkarr_relays') return Promise.resolve(relayList);
    if (cmd === 'set_pkarr_relays') return Promise.resolve(undefined);
    if (cmd === 'reset_pkarr_relays') return Promise.resolve(undefined);
    if (cmd === 'add_pkarr_relay') return Promise.resolve(undefined);
    if (cmd === 'remove_pkarr_relay') return Promise.resolve(undefined);
    return Promise.resolve(null);
  });
}

describe('NetworkDiscoverabilitySettings', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders the toggle section', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_identity_discoverable') return Promise.resolve(false);
      return Promise.resolve(null);
    });

    const { findByTestId } = render(NetworkDiscoverabilitySettings);

    await findByTestId('network-discoverability-settings');
    expect(true).toBe(true); // rendered successfully
  });

  it('shows "Off" when initial value is false', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_identity_discoverable') return Promise.resolve(false);
      return Promise.resolve(null);
    });

    const { findByTestId } = render(NetworkDiscoverabilitySettings);

    await waitFor(async () => {
      const val = await findByTestId('discoverability-value');
      expect(val.textContent?.trim()).toBe('Off');
    });
  });

  it('shows "On" when initial value is true', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_identity_discoverable') return Promise.resolve(true);
      return Promise.resolve(null);
    });

    const { findByTestId } = render(NetworkDiscoverabilitySettings);

    await waitFor(async () => {
      const val = await findByTestId('discoverability-value');
      expect(val.textContent?.trim()).toBe('On');
    });
  });

  it('calls connectivity_set_identity_discoverable with true when toggled on', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_identity_discoverable') return Promise.resolve(false);
      if (cmd === 'connectivity_set_identity_discoverable') return Promise.resolve(undefined);
      return Promise.resolve(null);
    });

    const { findByTestId } = render(NetworkDiscoverabilitySettings);

    await waitFor(async () => {
      const val = await findByTestId('discoverability-value');
      expect(val.textContent?.trim()).toBe('Off');
    });

    const toggle = await findByTestId('discoverability-toggle');
    await fireEvent.change(toggle, { target: { checked: true } });

    await waitFor(() => {
      expect(mockInvoke).toHaveBeenCalledWith(
        'connectivity_set_identity_discoverable',
        { enabled: true },
      );
    });
  });

  it('calls connectivity_set_identity_discoverable with false when toggled off', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_identity_discoverable') return Promise.resolve(true);
      if (cmd === 'connectivity_set_identity_discoverable') return Promise.resolve(undefined);
      return Promise.resolve(null);
    });

    const { findByTestId } = render(NetworkDiscoverabilitySettings);

    await waitFor(async () => {
      const val = await findByTestId('discoverability-value');
      expect(val.textContent?.trim()).toBe('On');
    });

    const toggle = await findByTestId('discoverability-toggle');
    await fireEvent.change(toggle, { target: { checked: false } });

    await waitFor(() => {
      expect(mockInvoke).toHaveBeenCalledWith(
        'connectivity_set_identity_discoverable',
        { enabled: false },
      );
    });
  });

  it('rolls back optimistic update on IPC failure and shows error', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_identity_discoverable') return Promise.resolve(false);
      if (cmd === 'connectivity_set_identity_discoverable') {
        return Promise.reject(new Error('disk full'));
      }
      return Promise.resolve(null);
    });

    const { findByTestId } = render(NetworkDiscoverabilitySettings);

    await waitFor(async () => {
      const val = await findByTestId('discoverability-value');
      expect(val.textContent?.trim()).toBe('Off');
    });

    const toggle = await findByTestId('discoverability-toggle');
    await fireEvent.change(toggle, { target: { checked: true } });

    await waitFor(async () => {
      // After rollback, value should revert to Off.
      const val = await findByTestId('discoverability-value');
      expect(val.textContent?.trim()).toBe('Off');
    });

    const err = await findByTestId('discoverability-error');
    expect(err.textContent).toContain('disk full');
  });

  it('calls connectivity_get_identity_discoverable on mount', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_identity_discoverable') return Promise.resolve(false);
      return Promise.resolve(null);
    });

    render(NetworkDiscoverabilitySettings);

    await waitFor(() => {
      expect(mockInvoke).toHaveBeenCalledWith('connectivity_get_identity_discoverable');
    });
  });

  it('shows loading state "…" before IPC resolves', () => {
    // Never-resolving promise — component stays in loading state.
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_identity_discoverable') {
        return new Promise(() => {});
      }
      return Promise.resolve(null);
    });

    const { getByTestId } = render(NetworkDiscoverabilitySettings);

    const val = getByTestId('discoverability-value');
    expect(val.textContent?.trim()).toBe('…');
  });

  it('renders helper text describing the toggle behaviour', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'connectivity_get_identity_discoverable') return Promise.resolve(false);
      return Promise.resolve(null);
    });

    const { container } = render(NetworkDiscoverabilitySettings);

    await waitFor(() => {
      expect(container.textContent).toContain('anyone who has your identity address');
    });
  });

  // ZEB-380: relay manager tests.
  describe('relay manager', () => {
    it('renders configured relays with badges', async () => {
      setupDefaultMocks();
      render(NetworkDiscoverabilitySettings);

      await waitFor(() => {
        const rows = screen.getAllByTestId('relay-row');
        expect(rows.length).toBe(2);
      });

      const badges = screen.getAllByTestId('relay-badge');
      expect(badges.length).toBe(2);
      badges.forEach((b) => expect(b.textContent).toContain('Healthy'));
    });

    it('renders a cooling-down badge for a coolingDown relay', async () => {
      const relayList = [
        makeRelay('https://relay.pkarr.org', false), // coolingDown
        makeRelay('https://pkarr.pubky.app', true),
      ];
      setupDefaultMocks(relayList);
      render(NetworkDiscoverabilitySettings);

      await waitFor(() => {
        const badges = screen.getAllByTestId('relay-badge');
        const cooling = badges.find((b) => b.textContent?.includes('Cooling down'));
        expect(cooling).toBeTruthy();
        // ZEB-384: countdown must render a real number, never `(NaNs)`.
        expect(cooling?.textContent).toMatch(/Cooling down \(\d+s\)/);
        expect(cooling?.textContent).not.toContain('NaN');
      });
    });

    it('Add submits add_pkarr_relay with only the new URL (server-authoritative RMW)', async () => {
      setupDefaultMocks();
      render(NetworkDiscoverabilitySettings);

      await waitFor(() => screen.getByTestId('relay-url-input'));

      const input = screen.getByTestId('relay-url-input') as HTMLInputElement;
      const addBtn = screen.getByTestId('relay-add-button') as HTMLButtonElement;

      await fireEvent.input(input, { target: { value: 'https://new.relay.example' } });
      await fireEvent.click(addBtn);

      await waitFor(() => {
        // ZEB-380: convergence refactor — client sends only the single URL;
        // the backend appends to the CURRENT persisted list (kills stale-full-list race).
        expect(mockInvoke).toHaveBeenCalledWith('add_pkarr_relay', {
          url: 'https://new.relay.example',
        });
      });

      // Ensure set_pkarr_relays was NOT called with a client-built full list.
      const setPkarrCalls = (mockInvoke as ReturnType<typeof vi.fn>).mock.calls.filter(
        (call) => call[0] === 'set_pkarr_relays',
      );
      expect(setPkarrCalls.length).toBe(0);
    });

    it('shows inline error and leaves list unchanged on invalid-URL rejection', async () => {
      mockInvoke.mockImplementation((cmd: string) => {
        if (cmd === 'connectivity_get_identity_discoverable') return Promise.resolve(false);
        if (cmd === 'get_pkarr_relays') return Promise.resolve(DEFAULT_RELAYS.map((u) => makeRelay(u)));
        // ZEB-380: Add now calls add_pkarr_relay, not set_pkarr_relays.
        if (cmd === 'add_pkarr_relay') return Promise.reject(new Error('invalid URL: missing scheme'));
        return Promise.resolve(null);
      });
      render(NetworkDiscoverabilitySettings);

      await waitFor(() => screen.getByTestId('relay-url-input'));

      const input = screen.getByTestId('relay-url-input') as HTMLInputElement;
      const addBtn = screen.getByTestId('relay-add-button') as HTMLButtonElement;

      await fireEvent.input(input, { target: { value: 'not-a-url' } });
      await fireEvent.click(addBtn);

      await waitFor(() => screen.getByTestId('relay-error'));
      const errEl = screen.getByTestId('relay-error');
      expect(errEl.textContent).toContain('invalid URL: missing scheme');

      // List must remain unchanged (2 default relays).
      const rows = screen.getAllByTestId('relay-row');
      expect(rows.length).toBe(2);
    });

    it('Remove submits remove_pkarr_relay with only the target URL (server-authoritative RMW)', async () => {
      setupDefaultMocks();
      render(NetworkDiscoverabilitySettings);

      await waitFor(() => screen.getAllByTestId('relay-remove'));

      // Click the first Remove button (relay.pkarr.org).
      const removeBtns = screen.getAllByTestId('relay-remove') as HTMLButtonElement[];
      await fireEvent.click(removeBtns[0]);

      await waitFor(() => {
        // ZEB-380: convergence refactor — client sends only the target URL;
        // the backend filters the CURRENT persisted list (kills stale-full-list race).
        expect(mockInvoke).toHaveBeenCalledWith('remove_pkarr_relay', {
          url: DEFAULT_RELAYS[0],
        });
      });

      // Ensure set_pkarr_relays was NOT called with a client-built list.
      const setPkarrCalls = (mockInvoke as ReturnType<typeof vi.fn>).mock.calls.filter(
        (call) => call[0] === 'set_pkarr_relays',
      );
      expect(setPkarrCalls.length).toBe(0);
    });

    it('Remove is disabled when only one relay remains', async () => {
      setupDefaultMocks([makeRelay('https://relay.pkarr.org')]);
      render(NetworkDiscoverabilitySettings);

      await waitFor(() => screen.getByTestId('relay-remove'));
      const removeBtn = screen.getByTestId('relay-remove') as HTMLButtonElement;
      expect(removeBtn.disabled).toBe(true);
    });

    it('Restore recommended invokes reset_pkarr_relays IPC (server-authoritative defaults)', async () => {
      setupDefaultMocks([makeRelay('https://custom.relay.example')]);
      render(NetworkDiscoverabilitySettings);

      await waitFor(() => screen.getByTestId('relay-restore-button'));
      const restoreBtn = screen.getByTestId('relay-restore-button') as HTMLButtonElement;
      await fireEvent.click(restoreBtn);

      await waitFor(() => {
        expect(mockInvoke).toHaveBeenCalledWith('reset_pkarr_relays');
      });
      // Ensure set_pkarr_relays was NOT called with a hardcoded list.
      const setPkarrCalls = (mockInvoke as ReturnType<typeof vi.fn>).mock.calls.filter(
        (call) => call[0] === 'set_pkarr_relays',
      );
      expect(setPkarrCalls.length).toBe(0);
    });

    it('Add button is disabled and does not call set_pkarr_relays before get_pkarr_relays resolves', async () => {
      // get_pkarr_relays never resolves → relayLoaded stays false → Add must be
      // disabled and the handler must be a no-op (Cursor round-4 HIGH fix).
      mockInvoke.mockImplementation((cmd: string) => {
        if (cmd === 'connectivity_get_identity_discoverable') return Promise.resolve(false);
        if (cmd === 'get_pkarr_relays') return new Promise(() => {}); // never resolves
        if (cmd === 'set_pkarr_relays') return Promise.resolve(undefined);
        return Promise.resolve(null);
      });

      render(NetworkDiscoverabilitySettings);

      // Wait for the component to stabilise (discoverable IPC resolves, relay stays pending).
      await waitFor(() => {
        const val = screen.getByTestId('discoverability-value');
        expect(val.textContent?.trim()).toBe('Off');
      });

      const input = screen.getByTestId('relay-url-input') as HTMLInputElement;
      const addBtn = screen.getByTestId('relay-add-button') as HTMLButtonElement;

      // Input and Add button must be disabled while the pool is unknown.
      expect(input.disabled).toBe(true);
      expect(addBtn.disabled).toBe(true);

      // Even if we try to fire the click directly, neither add_pkarr_relay nor
      // set_pkarr_relays must be called while the pool is unknown.
      await fireEvent.click(addBtn);

      const mutationCalls = (mockInvoke as ReturnType<typeof vi.fn>).mock.calls.filter(
        (call) => call[0] === 'add_pkarr_relay' || call[0] === 'set_pkarr_relays',
      );
      expect(mutationCalls.length).toBe(0);
    });

    it('clears relay error banner after a successful reload (Fix E)', async () => {
      // First call to get_pkarr_relays fails, then succeeds on retry.
      let callCount = 0;
      mockInvoke.mockImplementation((cmd: string) => {
        if (cmd === 'connectivity_get_identity_discoverable') return Promise.resolve(false);
        if (cmd === 'get_pkarr_relays') {
          callCount++;
          if (callCount === 1) return Promise.reject(new Error('network error'));
          return Promise.resolve(DEFAULT_RELAYS.map((u) => makeRelay(u)));
        }
        if (cmd === 'set_pkarr_relays') return Promise.resolve(undefined);
        if (cmd === 'reset_pkarr_relays') return Promise.resolve(undefined);
        return Promise.resolve(null);
      });

      render(NetworkDiscoverabilitySettings);

      // Wait for the initial error to appear.
      await waitFor(() => screen.getByTestId('relay-error'));
      expect(screen.getByTestId('relay-error').textContent).toContain('network error');

      // Trigger a reload via "Restore recommended" (which calls fetchRelays on success).
      await waitFor(() => screen.getByTestId('relay-restore-button'));
      await fireEvent.click(screen.getByTestId('relay-restore-button'));

      // After the successful fetchRelays, the error banner must clear.
      await waitFor(() => {
        expect(screen.queryByTestId('relay-error')).toBeNull();
      });
    });

    it('a failed event-driven refresh after load keeps the list and shows no error', async () => {
      // ZEB-380 (Cursor round-11): apply_pkarr_relays still emits
      // connectivity-relays-changed after a mutation, which triggers a refresh
      // refetch. If that refetch fails, it must NOT surface an error over a
      // list that is already current — only an INITIAL load failure is fatal.
      // Capture the relays-changed listener so we can fire it manually.
      let relayChangedCb: (() => void) | undefined;
      (listen as unknown as ReturnType<typeof vi.fn>).mockImplementation(
        (event: string, cb: () => void) => {
          if (event === 'connectivity-relays-changed') relayChangedCb = cb;
          return Promise.resolve(() => {});
        },
      );
      let failRefresh = false;
      mockInvoke.mockImplementation((cmd: string) => {
        if (cmd === 'connectivity_get_identity_discoverable') return Promise.resolve(false);
        if (cmd === 'get_pkarr_relays') {
          return failRefresh
            ? Promise.reject(new Error('transient refresh failure'))
            : Promise.resolve(DEFAULT_RELAYS.map((u) => makeRelay(u)));
        }
        return Promise.resolve(null);
      });

      render(NetworkDiscoverabilitySettings);

      // Initial load succeeds: both relays render, no error.
      await waitFor(() => expect(screen.getAllByTestId('relay-row')).toHaveLength(2));
      expect(screen.queryByTestId('relay-error')).toBeNull();

      // A subsequent event-driven refresh fails.
      failRefresh = true;
      expect(relayChangedCb).toBeDefined();
      relayChangedCb?.();

      // Wait for the failing refetch to have been attempted + settled, then
      // assert the error stayed absent and the list is intact.
      await waitFor(() => {
        const getCalls = mockInvoke.mock.calls.filter((c) => c[0] === 'get_pkarr_relays');
        expect(getCalls.length).toBeGreaterThanOrEqual(2);
      });
      expect(screen.queryByTestId('relay-error')).toBeNull();
      expect(screen.getAllByTestId('relay-row')).toHaveLength(2);
    });
  });
});
