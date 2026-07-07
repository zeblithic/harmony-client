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

  // ZEB-494 / PR #283 (Qodo + CodeAnt): Escape on the terminal `complete`
  // screen must route through onComplete (not onClose), so the onboarding gate
  // reloads into the joined identity rather than bouncing back to the gate.
  it('Escape on the completed screen fires onComplete, not onClose', async () => {
    mockedInvoke.mockResolvedValueOnce({ kind: 'complete' }); // get_pairing_state
    const onClose = vi.fn();
    const onComplete = vi.fn();
    render(PairingJoiner, { props: { onClose, onComplete } });
    // Confirm we're in the terminal `complete` branch before dismissing.
    await screen.findByText(/this device is now part of the owner identity/i);
    const dialog = screen.getByRole('dialog');
    await fireEvent.keyDown(dialog, { key: 'Escape' });
    expect(onComplete).toHaveBeenCalledTimes(1);
    expect(onClose).not.toHaveBeenCalled();
    // Terminal state → no wasted backend cancel IPC.
    expect(invoke).not.toHaveBeenCalledWith('cancel_pairing');
  });

  // ZEB-610 Commons chrome regression guard: the SAS stays in a mono display
  // block (`.sas-display`), digits rendered as text with whitespace-only
  // separation between triplets. Pins the restyle.
  it('renders the SAS in a mono display block (Commons chrome)', async () => {
    mockedInvoke.mockResolvedValueOnce({
      kind: 'handshaking',
      peerSessionId: '00000000-0000-0000-0000-000000000001',
      sasDigits: '012845',
    });
    const { container } = render(PairingJoiner);
    expect(await screen.findByText(/012\s*845|012845/)).toBeInTheDocument();
    expect(container.querySelector('.sas-display')).toBeTruthy();
  });

  // Guard the inverse: Escape in a non-terminal state keeps cancel semantics
  // (backend cancel IPC + onClose), and never fires onComplete.
  it('Escape in a non-terminal state still cancels via onClose', async () => {
    mockedInvoke.mockResolvedValueOnce({ kind: 'idle' }); // get_pairing_state
    mockedInvoke.mockResolvedValue(undefined); // cancel_pairing
    const onClose = vi.fn();
    const onComplete = vi.fn();
    render(PairingJoiner, { props: { onClose, onComplete } });
    const dialog = await screen.findByRole('dialog');
    await fireEvent.keyDown(dialog, { key: 'Escape' });
    await vi.waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
    expect(invoke).toHaveBeenCalledWith('cancel_pairing');
    expect(onComplete).not.toHaveBeenCalled();
  });
});
