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
import IrohRelaySettings from '../IrohRelaySettings.svelte';
import type { IrohRelayInfo } from '../../types/network-health';

const mockInvoke = invoke as unknown as ReturnType<typeof vi.fn>;
const mockListen = listen as unknown as ReturnType<typeof vi.fn>;

// Two placeholder "recommended defaults" — the component is agnostic to the
// exact URLs (the backend is authoritative); it only renders what it is given.
const DEFAULT_RELAYS = ['https://use1.relay.example', 'https://euw1.relay.example'];

function info(relays: string[], custom: boolean): IrohRelayInfo {
  return { relays, custom };
}

// Set up mockInvoke to answer every iroh relay command with the given info.
function setupDefaultMocks(
  current: IrohRelayInfo = info(DEFAULT_RELAYS, false),
  mutated: IrohRelayInfo = current,
): void {
  mockInvoke.mockImplementation((cmd: string) => {
    if (cmd === 'get_iroh_relays') return Promise.resolve(current);
    if (cmd === 'set_iroh_relays') return Promise.resolve(mutated);
    if (cmd === 'add_iroh_relay') return Promise.resolve(mutated);
    if (cmd === 'remove_iroh_relay') return Promise.resolve(mutated);
    if (cmd === 'reset_iroh_relays') return Promise.resolve(mutated);
    return Promise.resolve(null);
  });
}

describe('IrohRelaySettings', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    // Restore the default no-op listen resolution after clearAllMocks wipes it.
    mockListen.mockResolvedValue(() => {});
  });

  it('renders the section heading and helper copy', async () => {
    setupDefaultMocks();
    const { container } = render(IrohRelaySettings);

    await screen.findByTestId('iroh-relay-manager');
    expect(container.textContent).toContain('Transport relays (iroh)');
    expect(container.textContent).toContain(
      "Relays carry traffic when a direct connection isn't possible",
    );
  });

  it('renders the relay list returned by get_iroh_relays', async () => {
    setupDefaultMocks();
    render(IrohRelaySettings);

    await waitFor(() => {
      const rows = screen.getAllByTestId('iroh-relay-row');
      expect(rows.length).toBe(DEFAULT_RELAYS.length);
    });
    const urls = screen.getAllByTestId('iroh-relay-url').map((el) => el.textContent);
    expect(urls).toEqual(DEFAULT_RELAYS);
  });

  it('shows "Using recommended relays" when custom is false', async () => {
    setupDefaultMocks(info(DEFAULT_RELAYS, false));
    render(IrohRelaySettings);

    await waitFor(() => {
      const note = screen.getByTestId('iroh-relay-state-note');
      expect(note.textContent).toContain('Using recommended relays');
    });
  });

  it('shows "Custom relay set" when custom is true', async () => {
    setupDefaultMocks(info(['https://mine.relay.example'], true));
    render(IrohRelaySettings);

    await waitFor(() => {
      const note = screen.getByTestId('iroh-relay-state-note');
      expect(note.textContent).toContain('Custom relay set');
    });
  });

  it('Add calls add_iroh_relay with only the new URL and renders the returned list', async () => {
    const added = info([...DEFAULT_RELAYS, 'https://new.relay.example'], true);
    setupDefaultMocks(info(DEFAULT_RELAYS, false), added);
    render(IrohRelaySettings);

    await waitFor(() => screen.getByTestId('iroh-relay-url-input'));
    const input = screen.getByTestId('iroh-relay-url-input') as HTMLInputElement;
    const addBtn = screen.getByTestId('iroh-relay-add-button') as HTMLButtonElement;

    await fireEvent.input(input, { target: { value: 'https://new.relay.example' } });
    await fireEvent.click(addBtn);

    await waitFor(() => {
      expect(mockInvoke).toHaveBeenCalledWith('add_iroh_relay', {
        url: 'https://new.relay.example',
      });
    });

    // The returned authoritative list is applied directly (custom now true).
    await waitFor(() => {
      const rows = screen.getAllByTestId('iroh-relay-row');
      expect(rows.length).toBe(added.relays.length);
      expect(screen.getByTestId('iroh-relay-state-note').textContent).toContain(
        'Custom relay set',
      );
    });

    // A mutation must NOT trigger a client-built full-list set_iroh_relays.
    const setCalls = mockInvoke.mock.calls.filter((c) => c[0] === 'set_iroh_relays');
    expect(setCalls.length).toBe(0);
    // And it applies the returned list WITHOUT a refetch: get called once (mount).
    const getCalls = mockInvoke.mock.calls.filter((c) => c[0] === 'get_iroh_relays');
    expect(getCalls.length).toBe(1);
  });

  it('Remove calls remove_iroh_relay with only the target URL', async () => {
    const remaining = info([DEFAULT_RELAYS[1]], true);
    setupDefaultMocks(info(DEFAULT_RELAYS, false), remaining);
    render(IrohRelaySettings);

    await waitFor(() => screen.getAllByTestId('iroh-relay-remove'));
    const removeBtns = screen.getAllByTestId('iroh-relay-remove') as HTMLButtonElement[];
    await fireEvent.click(removeBtns[0]);

    await waitFor(() => {
      expect(mockInvoke).toHaveBeenCalledWith('remove_iroh_relay', {
        url: DEFAULT_RELAYS[0],
      });
    });
    // Applies the returned list directly — no refetch.
    const getCalls = mockInvoke.mock.calls.filter((c) => c[0] === 'get_iroh_relays');
    expect(getCalls.length).toBe(1);
  });

  it('Restore recommended calls reset_iroh_relays', async () => {
    setupDefaultMocks(info(['https://mine.relay.example'], true), info(DEFAULT_RELAYS, false));
    render(IrohRelaySettings);

    await waitFor(() => screen.getByTestId('iroh-relay-restore-button'));
    await fireEvent.click(screen.getByTestId('iroh-relay-restore-button'));

    await waitFor(() => {
      expect(mockInvoke).toHaveBeenCalledWith('reset_iroh_relays');
    });
    // After reset, the note flips back to recommended.
    await waitFor(() => {
      expect(screen.getByTestId('iroh-relay-state-note').textContent).toContain(
        'Using recommended relays',
      );
    });
  });

  it('surfaces a mutation rejection in a role="alert" error region', async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === 'get_iroh_relays') return Promise.resolve(info(DEFAULT_RELAYS, false));
      if (cmd === 'add_iroh_relay') {
        return Promise.reject(new Error('invalid URL: missing scheme'));
      }
      return Promise.resolve(null);
    });
    render(IrohRelaySettings);

    await waitFor(() => screen.getByTestId('iroh-relay-url-input'));
    const input = screen.getByTestId('iroh-relay-url-input') as HTMLInputElement;
    const addBtn = screen.getByTestId('iroh-relay-add-button') as HTMLButtonElement;

    await fireEvent.input(input, { target: { value: 'not-a-url' } });
    await fireEvent.click(addBtn);

    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('invalid URL: missing scheme');

    // The list must be unchanged (still the defaults).
    expect(screen.getAllByTestId('iroh-relay-row').length).toBe(DEFAULT_RELAYS.length);
  });

  it('subscribes to iroh-relays-changed on mount and refetches on the event', async () => {
    let relayChangedCb: (() => void) | undefined;
    mockListen.mockImplementation((event: string, cb: () => void) => {
      if (event === 'iroh-relays-changed') relayChangedCb = cb;
      return Promise.resolve(() => {});
    });
    setupDefaultMocks(info(DEFAULT_RELAYS, false));
    render(IrohRelaySettings);

    await waitFor(() =>
      expect(screen.getAllByTestId('iroh-relay-row')).toHaveLength(DEFAULT_RELAYS.length),
    );
    expect(mockListen).toHaveBeenCalledWith('iroh-relays-changed', expect.any(Function));
    expect(relayChangedCb).toBeDefined();

    // Fire the event; the component must refetch via get_iroh_relays.
    relayChangedCb?.();

    await waitFor(() => {
      const getCalls = mockInvoke.mock.calls.filter((c) => c[0] === 'get_iroh_relays');
      expect(getCalls.length).toBeGreaterThanOrEqual(2);
    });
  });
});
