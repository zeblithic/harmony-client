import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import PairingInviter from '../PairingInviter.svelte';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
const mockedInvoke = invoke as unknown as ReturnType<typeof vi.fn>;
const mockedListen = listen as unknown as ReturnType<typeof vi.fn>;

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

  // ZEB-668 S6: completion seam. onComplete must fire AFTER a fold
  // re-fetch of get_pairing_state — the backend's Complete branch is what
  // folds the freshly-persisted enrollment into the resident trust doc,
  // and the Devices panel (which consumes onComplete) renders from that
  // doc. Without the fold the successor row is invisible until restart.
  it('fires onComplete once with the device id only AFTER the fold re-fetch settles', async () => {
    const complete = { kind: 'complete', deviceIdHex: 'cc'.repeat(16) };
    // Deferred second fetch (CodeRabbit, PR #456 round 1): with an
    // immediately-resolved mock, a regression that fires onComplete
    // without awaiting refreshSnapshot() would still pass.
    let stateFetches = 0;
    let resolveRefresh!: (value: typeof complete) => void;
    const refresh = new Promise<typeof complete>((resolve) => {
      resolveRefresh = resolve;
    });
    mockedInvoke.mockImplementation((cmd: unknown) => {
      if (cmd !== 'get_pairing_state') return Promise.resolve(undefined);
      stateFetches += 1;
      return stateFetches === 1 ? Promise.resolve(complete) : refresh;
    });
    const onComplete = vi.fn();
    render(PairingInviter, { props: { hostname: 'KRILE', onComplete } });
    await waitFor(() => expect(stateFetches).toBe(2));
    expect(onComplete).not.toHaveBeenCalled(); // fold still pending
    resolveRefresh(complete);
    await waitFor(() => expect(onComplete).toHaveBeenCalledTimes(1));
    expect(onComplete).toHaveBeenCalledWith('cc'.repeat(16));
  });

  it('does not re-fire onComplete on a duplicate complete flush', async () => {
    let listener: ((e: { payload: unknown }) => void) | undefined;
    mockedListen.mockImplementation(
      (_evt: string, cb: (e: { payload: unknown }) => void) => {
        listener = cb;
        return Promise.resolve(() => {});
      },
    );
    const complete = { kind: 'complete', deviceIdHex: 'cc'.repeat(16) };
    let stateFetches = 0;
    let resolveRefresh!: (value: typeof complete) => void;
    const refresh = new Promise<typeof complete>((resolve) => {
      resolveRefresh = resolve;
    });
    mockedInvoke.mockImplementation((cmd: unknown) => {
      if (cmd !== 'get_pairing_state') return Promise.resolve(undefined);
      stateFetches += 1;
      return stateFetches === 1 ? Promise.resolve(complete) : refresh;
    });
    const onComplete = vi.fn();
    render(PairingInviter, { props: { hostname: 'KRILE', onComplete } });
    await waitFor(() => expect(stateFetches).toBe(2));
    // Duplicate flush lands WHILE the fold is still pending — the
    // notified-once guard must hold across the await, not just after it.
    listener!({ payload: complete });
    resolveRefresh(complete);
    await waitFor(() => expect(onComplete).toHaveBeenCalledTimes(1));
    listener!({ payload: complete }); // and again after settling
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(onComplete).toHaveBeenCalledTimes(1);
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
