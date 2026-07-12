import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { PairingService } from './pairing-service';

const mockedInvoke = invoke as unknown as ReturnType<typeof vi.fn>;
const mockedListen = listen as unknown as ReturnType<typeof vi.fn>;

describe('PairingService', () => {
  beforeEach(() => {
    vi.resetAllMocks();
    mockedListen.mockResolvedValue(() => {});
  });

  it('startInviter invokes start_inviter_pairing with display name', async () => {
    mockedInvoke.mockResolvedValueOnce(undefined);
    const svc = new PairingService();
    await svc.startInviter('KRILE');
    expect(invoke).toHaveBeenCalledWith('start_inviter_pairing', { displayName: 'KRILE' });
  });

  it('startJoiner invokes start_joiner_pairing with display name', async () => {
    mockedInvoke.mockResolvedValueOnce(undefined);
    const svc = new PairingService();
    await svc.startJoiner('AVALON');
    expect(invoke).toHaveBeenCalledWith('start_joiner_pairing', { displayName: 'AVALON' });
  });

  it('selectPeer invokes select_pairing_peer with session id', async () => {
    mockedInvoke.mockResolvedValueOnce(undefined);
    const svc = new PairingService();
    await svc.selectPeer('00000000-0000-0000-0000-000000000001');
    expect(invoke).toHaveBeenCalledWith('select_pairing_peer', {
      peerSessionId: '00000000-0000-0000-0000-000000000001',
    });
  });

  it('confirmSas invokes confirm_pairing_sas', async () => {
    mockedInvoke.mockResolvedValueOnce(undefined);
    const svc = new PairingService();
    await svc.confirmSas();
    expect(invoke).toHaveBeenCalledWith('confirm_pairing_sas');
  });

  it('cancel invokes cancel_pairing', async () => {
    mockedInvoke.mockResolvedValueOnce(undefined);
    const svc = new PairingService();
    await svc.cancel();
    expect(invoke).toHaveBeenCalledWith('cancel_pairing');
  });

  // ZEB-668 S6: the re-fetch matters beyond the returned value — on
  // Complete, get_pairing_state_inner folds the freshly-persisted
  // enrollment into the resident trust doc (which the Devices panel
  // renders from). This pins the frontend half of that contract.
  it('refreshSnapshot re-fetches get_pairing_state, applies it, and fires onChange', async () => {
    const complete = { kind: 'complete', deviceIdHex: 'cc'.repeat(16) };
    mockedInvoke.mockResolvedValueOnce(complete);
    const svc = new PairingService();
    const changed = vi.fn();
    svc.onChange = changed;
    await svc.refreshSnapshot();
    expect(invoke).toHaveBeenCalledWith('get_pairing_state');
    expect(svc.state).toEqual(complete);
    expect(changed).toHaveBeenCalledTimes(1);
  });

  it('subscribes to pairing-state-changed and updates state', async () => {
    let listener: ((event: { payload: unknown }) => void) | undefined;
    mockedListen.mockImplementation((_event: string, cb: (e: { payload: unknown }) => void) => {
      listener = cb;
      return Promise.resolve(() => {});
    });
    mockedInvoke.mockResolvedValueOnce({ kind: 'idle' });
    const svc = new PairingService();
    await svc.init();
    expect(listener).toBeDefined();
    listener!({ payload: { kind: 'enrolling' } });
    expect(svc.state).toEqual({ kind: 'enrolling' });
  });
});
