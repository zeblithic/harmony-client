import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import PairingInviter from '../PairingInviter.svelte';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import { invoke } from '@tauri-apps/api/core';
const mockedInvoke = invoke as unknown as ReturnType<typeof vi.fn>;

beforeEach(() => {
  vi.resetAllMocks();
});

describe('PairingInviter', () => {
  it('starts inviter mode automatically on mount', async () => {
    mockedInvoke.mockResolvedValueOnce({ kind: 'idle' }); // get_pairing_state
    mockedInvoke.mockResolvedValueOnce(undefined); // start_inviter_pairing
    render(PairingInviter, { props: { hostname: 'KRILE' } });
    // Wait a tick for onMount.
    await new Promise(resolve => setTimeout(resolve, 0));
    expect(invoke).toHaveBeenCalledWith('start_inviter_pairing', { displayName: 'KRILE' });
  });

  it('renders SAS digits when state transitions to handshaking', async () => {
    mockedInvoke.mockResolvedValueOnce({
      kind: 'handshaking',
      peerSessionId: '00000000-0000-0000-0000-000000000002',
      sasDigits: '987654',
    });
    mockedInvoke.mockResolvedValueOnce(undefined);
    render(PairingInviter, { props: { hostname: 'KRILE' } });
    expect(await screen.findByText(/987\s*654|987654/)).toBeInTheDocument();
  });

  it('renders peer rows in discovered state', async () => {
    mockedInvoke.mockResolvedValueOnce({
      kind: 'discovered',
      peers: [{
        sessionId: '00000000-0000-0000-0000-000000000003',
        role: 'joiner',
        displayName: 'AVALON',
        ownerIdIfInviter: null,
        ephemeralPubkeyHex: '00'.repeat(32),
        joinerEd25519VerifyHex: '11'.repeat(32),
        seenAtUnix: 1_700_000_000,
      }],
    });
    mockedInvoke.mockResolvedValueOnce(undefined);
    render(PairingInviter, { props: { hostname: 'KRILE' } });
    expect(await screen.findByText('AVALON')).toBeInTheDocument();
  });

  it('Cancel invokes cancel_pairing', async () => {
    mockedInvoke.mockResolvedValueOnce({ kind: 'discovering', role: 'inviter', ephemeralPubkeyHex: '', sessionId: '00000000-0000-0000-0000-000000000004' });
    mockedInvoke.mockResolvedValueOnce(undefined);
    render(PairingInviter, { props: { hostname: 'KRILE' } });
    const cancelBtn = await screen.findByRole('button', { name: /cancel/i });
    await fireEvent.click(cancelBtn);
    expect(invoke).toHaveBeenCalledWith('cancel_pairing');
  });

  it('renders modal with correct a11y attributes', async () => {
    mockedInvoke.mockResolvedValueOnce({ kind: 'idle' });
    mockedInvoke.mockResolvedValueOnce(undefined);
    render(PairingInviter, { props: { hostname: 'KRILE' } });
    const dialog = await screen.findByRole('dialog');
    expect(dialog).toHaveAttribute('aria-modal', 'true');
    expect(dialog).toHaveAttribute('aria-labelledby', 'invite-heading');
  });

  // ZEB-610 Commons chrome regression guard: the SAS stays in a mono display
  // block (`.sas-display`) with the digits rendered as text (whitespace-only
  // between triplets — no injected separator). Pins the restyle so a later
  // change can't drop the class the visual card depends on.
  it('renders the SAS in a mono display block (Commons chrome)', async () => {
    mockedInvoke.mockResolvedValueOnce({
      kind: 'handshaking',
      peerSessionId: '00000000-0000-0000-0000-000000000002',
      sasDigits: '987654',
    });
    mockedInvoke.mockResolvedValueOnce(undefined);
    const { container } = render(PairingInviter, { props: { hostname: 'KRILE' } });
    expect(await screen.findByText(/987\s*654|987654/)).toBeInTheDocument();
    expect(container.querySelector('.sas-display')).toBeTruthy();
  });
});
