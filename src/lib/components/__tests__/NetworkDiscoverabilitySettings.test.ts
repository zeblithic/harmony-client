import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';

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

const mockInvoke = invoke as unknown as ReturnType<typeof vi.fn>;

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
});
