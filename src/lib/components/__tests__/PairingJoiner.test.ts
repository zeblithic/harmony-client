import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import PairingJoiner from '../PairingJoiner.svelte';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import { invoke } from '@tauri-apps/api/core';
const mockedInvoke = invoke as unknown as ReturnType<typeof vi.fn>;

beforeEach(() => {
  vi.resetAllMocks();
});

describe('PairingJoiner', () => {
  it('renders display-name input as the first step', async () => {
    mockedInvoke.mockResolvedValueOnce({ kind: 'idle' });
    render(PairingJoiner);
    expect(await screen.findByLabelText(/give this device a name/i)).toBeInTheDocument();
  });

  it('starts the joiner flow when name is submitted', async () => {
    mockedInvoke.mockResolvedValueOnce({ kind: 'idle' }); // get_pairing_state
    mockedInvoke.mockResolvedValueOnce(undefined); // start_joiner_pairing
    render(PairingJoiner);
    const input = await screen.findByLabelText(/give this device a name/i);
    await fireEvent.input(input, { target: { value: 'AVALON' } });
    const startBtn = screen.getByRole('button', { name: /start pairing/i });
    await fireEvent.click(startBtn);
    expect(invoke).toHaveBeenCalledWith('start_joiner_pairing', { displayName: 'AVALON' });
  });

  it('renders SAS digits when state transitions to handshaking', async () => {
    // The component renders state from PairingService; we patch its store directly.
    mockedInvoke.mockResolvedValueOnce({
      kind: 'handshaking',
      peerSessionId: '00000000-0000-0000-0000-000000000001',
      sasDigits: '012845',
    });
    render(PairingJoiner);
    expect(await screen.findByText(/012\s*845|012845/)).toBeInTheDocument();
  });

  it('renders Cancel button that invokes cancel_pairing', async () => {
    mockedInvoke.mockResolvedValueOnce({ kind: 'idle' });
    mockedInvoke.mockResolvedValueOnce(undefined);
    render(PairingJoiner);
    const cancelBtn = await screen.findByRole('button', { name: /cancel/i });
    await fireEvent.click(cancelBtn);
    expect(invoke).toHaveBeenCalledWith('cancel_pairing');
  });

  it('renders modal with correct a11y attributes', async () => {
    mockedInvoke.mockResolvedValueOnce({ kind: 'idle' });
    mockedInvoke.mockResolvedValueOnce(undefined);
    render(PairingJoiner);
    const dialog = await screen.findByRole('dialog');
    expect(dialog).toHaveAttribute('aria-modal', 'true');
    expect(dialog).toHaveAttribute('aria-labelledby', 'join-heading');
  });
});
