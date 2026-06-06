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
        expect(badges.some((b) => b.textContent?.includes('Cooling down'))).toBe(true);
      });
    });

    it('Add submits setPkarrRelays with the new list', async () => {
      setupDefaultMocks();
      render(NetworkDiscoverabilitySettings);

      await waitFor(() => screen.getByTestId('relay-url-input'));

      const input = screen.getByTestId('relay-url-input') as HTMLInputElement;
      const addBtn = screen.getByTestId('relay-add-button') as HTMLButtonElement;

      await fireEvent.input(input, { target: { value: 'https://new.relay.example' } });
      await fireEvent.click(addBtn);

      await waitFor(() => {
        expect(mockInvoke).toHaveBeenCalledWith('set_pkarr_relays', {
          relays: [...DEFAULT_RELAYS, 'https://new.relay.example'],
        });
      });
    });

    it('shows inline error and leaves list unchanged on invalid-URL rejection', async () => {
      mockInvoke.mockImplementation((cmd: string) => {
        if (cmd === 'connectivity_get_identity_discoverable') return Promise.resolve(false);
        if (cmd === 'get_pkarr_relays') return Promise.resolve(DEFAULT_RELAYS.map((u) => makeRelay(u)));
        if (cmd === 'set_pkarr_relays') return Promise.reject(new Error('invalid URL: missing scheme'));
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

    it('Remove is disabled when only one relay remains', async () => {
      setupDefaultMocks([makeRelay('https://relay.pkarr.org')]);
      render(NetworkDiscoverabilitySettings);

      await waitFor(() => screen.getByTestId('relay-remove'));
      const removeBtn = screen.getByTestId('relay-remove') as HTMLButtonElement;
      expect(removeBtn.disabled).toBe(true);
    });

    it('Restore recommended submits the default set', async () => {
      setupDefaultMocks([makeRelay('https://custom.relay.example')]);
      render(NetworkDiscoverabilitySettings);

      await waitFor(() => screen.getByTestId('relay-restore-button'));
      const restoreBtn = screen.getByTestId('relay-restore-button') as HTMLButtonElement;
      await fireEvent.click(restoreBtn);

      await waitFor(() => {
        expect(mockInvoke).toHaveBeenCalledWith('set_pkarr_relays', {
          relays: DEFAULT_RELAYS,
        });
      });
    });
  });
});
