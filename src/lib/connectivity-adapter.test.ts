import { describe, it, expect, vi, beforeEach } from 'vitest';

// Use vi.mock of the IPC module — matches the established pattern in
// `owner-service.test.ts` and `PendingJoinsPanel.test.ts`. The repo has
// no existing `mockIPC` callers, so we stick with the convention rather
// than introducing a second mocking style.
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import {
  getMyReachabilityRecord,
  listPeerReachability,
  forceRepublish,
  onReachabilityChanged,
  redeemInviteIroh,
  discoverIdentity,
  getPkarrRelays,
  setPkarrRelays,
  addPkarrRelay,
  removePkarrRelay,
  getIrohRelays,
  setIrohRelays,
  addIrohRelay,
  removeIrohRelay,
  resetIrohRelays,
} from './connectivity-adapter';
import type {
  ReachabilityRecord,
  PeerReachability,
  ConnectivityReachabilityChangedPayload,
} from './types/connectivity';
import type { RelayHealth, IrohRelayInfo } from './types/network-health';

const mockInvoke = invoke as unknown as ReturnType<typeof vi.fn>;
const mockListen = listen as unknown as ReturnType<typeof vi.fn>;

describe('connectivity-adapter', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  describe('getMyReachabilityRecord', () => {
    it('returns null when iroh transport is not running', async () => {
      mockInvoke.mockResolvedValueOnce(null);
      const result = await getMyReachabilityRecord();
      expect(result).toBeNull();
      expect(mockInvoke).toHaveBeenCalledWith('connectivity_get_my_reachability_record');
    });

    it('returns the populated record when iroh is bound', async () => {
      const record: ReachabilityRecord = {
        irohNodeId: 'ab'.repeat(32),
        homeRelayUrl: 'https://derp.example/',
        directAddresses: ['10.0.0.1:4242', '[fe80::1]:4242'],
        announcedAtMs: 0,
      };
      mockInvoke.mockResolvedValueOnce(record);
      const result = await getMyReachabilityRecord();
      expect(result).toEqual(record);
    });

    it('wraps Error rejections with the IPC-name prefix', async () => {
      mockInvoke.mockRejectedValueOnce(new Error('iroh poisoned'));
      await expect(getMyReachabilityRecord()).rejects.toThrow(
        'connectivity_get_my_reachability_record: iroh poisoned',
      );
    });

    it('wraps plain-string rejections (production Tauri rejection shape)', async () => {
      // Memory rule `feedback_tauri_error_extraction`: production rejections
      // are plain strings, not Error objects. The adapter must handle both.
      mockInvoke.mockRejectedValueOnce('NodeState poisoned: PoisonError');
      await expect(getMyReachabilityRecord()).rejects.toThrow(
        'connectivity_get_my_reachability_record: NodeState poisoned: PoisonError',
      );
    });
  });

  describe('listPeerReachability', () => {
    it('returns an empty array when no peers are known', async () => {
      mockInvoke.mockResolvedValueOnce([]);
      const result = await listPeerReachability();
      expect(result).toEqual([]);
      expect(mockInvoke).toHaveBeenCalledWith('connectivity_list_peer_reachability');
    });

    it('returns the PeerReachability objects emitted by the backend DTO', async () => {
      // Backend DTO shape: PeerReachabilityDto { owner_address, record } with
      // serde rename_all=camelCase, so the wire-form is `ownerAddress` +
      // `record`. NOT a [string, record] tuple — the plan said tuples but
      // Task 9 actually returns objects, confirmed by reading lib.rs.
      const sample: PeerReachability[] = [
        {
          ownerAddress: 'aa'.repeat(16),
          record: {
            irohNodeId: 'bb'.repeat(32),
            homeRelayUrl: 'https://derp.example/',
            directAddresses: [],
            announcedAtMs: 1_700_000_000_000,
          },
          source: 'durableCrdt',
        },
      ];
      mockInvoke.mockResolvedValueOnce(sample);
      const result = await listPeerReachability();
      expect(result).toEqual(sample);
      expect(result[0].ownerAddress).toBe('aa'.repeat(16));
      expect(result[0].record.irohNodeId).toBe('bb'.repeat(32));
    });

    it('wraps rejections with the IPC-name prefix', async () => {
      mockInvoke.mockRejectedValueOnce(new Error('resolver missing'));
      await expect(listPeerReachability()).rejects.toThrow(
        'connectivity_list_peer_reachability: resolver missing',
      );
    });
  });

  describe('forceRepublish', () => {
    it('returns true when the publisher was notified', async () => {
      // Plan said Promise<void>, but Task 9's actual signature is
      // Result<bool, String>. We return the bool so callers can verify
      // the publisher path was reached.
      mockInvoke.mockResolvedValueOnce(true);
      const result = await forceRepublish();
      expect(result).toBe(true);
      expect(mockInvoke).toHaveBeenCalledWith('connectivity_force_republish');
    });

    it('returns false when no publisher is running', async () => {
      mockInvoke.mockResolvedValueOnce(false);
      const result = await forceRepublish();
      expect(result).toBe(false);
    });

    it('wraps rejections with the IPC-name prefix', async () => {
      mockInvoke.mockRejectedValueOnce(new Error('lock poisoned'));
      await expect(forceRepublish()).rejects.toThrow(
        'connectivity_force_republish: lock poisoned',
      );
    });
  });

  describe('onReachabilityChanged', () => {
    it('subscribes to the connectivity-reachability-changed event', async () => {
      const unlistenStub = vi.fn();
      mockListen.mockResolvedValueOnce(unlistenStub);
      const callback = vi.fn();
      const unlisten = await onReachabilityChanged(callback);
      expect(mockListen).toHaveBeenCalledWith(
        'connectivity-reachability-changed',
        expect.any(Function),
      );
      expect(unlisten).toBe(unlistenStub);
    });

    it('delivers the deserialized payload to the callback', async () => {
      // Capture the listen() handler so we can simulate an event delivery.
      let capturedHandler:
        | ((event: { payload: ConnectivityReachabilityChangedPayload }) => void)
        | null = null;
      mockListen.mockImplementationOnce((_name, handler) => {
        capturedHandler = handler;
        return Promise.resolve(() => {});
      });
      const callback = vi.fn();
      await onReachabilityChanged(callback);
      expect(capturedHandler).not.toBeNull();

      // Simulate an emit from the Rust side. The adapter strips the event
      // envelope and forwards just the payload.
      const payload: ConnectivityReachabilityChangedPayload = {
        actor: 'aa'.repeat(16),
      };
      capturedHandler!({ payload });
      expect(callback).toHaveBeenCalledWith(payload);
      expect(callback).toHaveBeenCalledTimes(1);
    });
  });

  // ZEB-380: relay pool IPCs.
  describe('getPkarrRelays', () => {
    it('returns the mocked relay list', async () => {
      const fixture: RelayHealth[] = [
        {
          url: 'https://relay.pkarr.org',
          state: { kind: 'healthy' },
          lastOutcome: { kind: 'success' },
          lastSuccessMs: 1_700_000_000_000,
        },
      ];
      mockInvoke.mockResolvedValueOnce(fixture);
      const result = await getPkarrRelays();
      expect(result).toEqual(fixture);
      expect(mockInvoke).toHaveBeenCalledWith('get_pkarr_relays');
    });

    it('wraps rejections with the IPC-name prefix', async () => {
      mockInvoke.mockRejectedValueOnce(new Error('pool not running'));
      await expect(getPkarrRelays()).rejects.toThrow('get_pkarr_relays: pool not running');
    });

    it('wraps plain-string rejections (production Tauri shape)', async () => {
      mockInvoke.mockRejectedValueOnce('pool not running');
      await expect(getPkarrRelays()).rejects.toThrow('get_pkarr_relays: pool not running');
    });
  });

  describe('setPkarrRelays', () => {
    it('passes the relays array as { relays } to invoke', async () => {
      mockInvoke.mockResolvedValueOnce(undefined);
      await setPkarrRelays(['https://relay.pkarr.org', 'https://pkarr.pubky.app']);
      expect(mockInvoke).toHaveBeenCalledWith('set_pkarr_relays', {
        relays: ['https://relay.pkarr.org', 'https://pkarr.pubky.app'],
      });
    });

    it('resolves without a return value on success', async () => {
      mockInvoke.mockResolvedValueOnce(undefined);
      await expect(setPkarrRelays(['https://relay.pkarr.org'])).resolves.toBeUndefined();
    });

    it('wraps a rejected set_pkarr_relays and surfaces the error string', async () => {
      mockInvoke.mockRejectedValueOnce('invalid URL: missing scheme');
      await expect(setPkarrRelays(['not-a-url'])).rejects.toThrow(
        'set_pkarr_relays: invalid URL: missing scheme',
      );
    });

    it('wraps Error-object rejections as well', async () => {
      mockInvoke.mockRejectedValueOnce(new Error('cap exceeded'));
      await expect(setPkarrRelays(Array(9).fill('https://relay.pkarr.org'))).rejects.toThrow(
        'set_pkarr_relays: cap exceeded',
      );
    });
  });

  // ZEB-380: server-authoritative read-modify-write relay IPCs.
  describe('addPkarrRelay', () => {
    it('passes the url as { url } to invoke', async () => {
      mockInvoke.mockResolvedValueOnce(undefined);
      await addPkarrRelay('https://new.relay.example');
      expect(mockInvoke).toHaveBeenCalledWith('add_pkarr_relay', {
        url: 'https://new.relay.example',
      });
    });

    it('resolves without a return value on success', async () => {
      mockInvoke.mockResolvedValueOnce(undefined);
      await expect(addPkarrRelay('https://relay.pkarr.org')).resolves.toBeUndefined();
    });

    it('wraps a rejected add_pkarr_relay and surfaces the error string', async () => {
      mockInvoke.mockRejectedValueOnce('invalid URL: missing scheme');
      await expect(addPkarrRelay('not-a-url')).rejects.toThrow(
        'add_pkarr_relay: invalid URL: missing scheme',
      );
    });

    it('wraps Error-object rejections as well', async () => {
      mockInvoke.mockRejectedValueOnce(new Error('cap exceeded'));
      await expect(addPkarrRelay('https://relay.pkarr.org')).rejects.toThrow(
        'add_pkarr_relay: cap exceeded',
      );
    });
  });

  describe('removePkarrRelay', () => {
    it('passes the url as { url } to invoke', async () => {
      mockInvoke.mockResolvedValueOnce(undefined);
      await removePkarrRelay('https://relay.pkarr.org');
      expect(mockInvoke).toHaveBeenCalledWith('remove_pkarr_relay', {
        url: 'https://relay.pkarr.org',
      });
    });

    it('resolves without a return value on success', async () => {
      mockInvoke.mockResolvedValueOnce(undefined);
      await expect(removePkarrRelay('https://relay.pkarr.org')).resolves.toBeUndefined();
    });

    it('wraps a rejected remove_pkarr_relay and surfaces the error string', async () => {
      mockInvoke.mockRejectedValueOnce('at least one relay is required');
      await expect(removePkarrRelay('https://relay.pkarr.org')).rejects.toThrow(
        'remove_pkarr_relay: at least one relay is required',
      );
    });

    it('wraps Error-object rejections as well', async () => {
      mockInvoke.mockRejectedValueOnce(new Error('NodeState poisoned'));
      await expect(removePkarrRelay('https://relay.pkarr.org')).rejects.toThrow(
        'remove_pkarr_relay: NodeState poisoned',
      );
    });
  });

  // ZEB-624: iroh transport relay config IPCs. Unlike the pkarr relay pool
  // (per-relay RelayHealth), these return an IrohRelayInfo `{ relays, custom }`:
  // `custom === false` means the node is following iroh's recommended defaults
  // (the returned `relays` shows them), `true` means a materialized custom list.
  describe('getIrohRelays', () => {
    it('returns the mocked relay info and invokes with no args', async () => {
      const fixture: IrohRelayInfo = {
        relays: ['https://use1.relay.example', 'https://euw1.relay.example'],
        custom: false,
      };
      mockInvoke.mockResolvedValueOnce(fixture);
      const result = await getIrohRelays();
      expect(result).toEqual(fixture);
      expect(mockInvoke).toHaveBeenCalledWith('get_iroh_relays');
    });

    it('wraps Error rejections with the IPC-name prefix', async () => {
      mockInvoke.mockRejectedValueOnce(new Error('config poisoned'));
      await expect(getIrohRelays()).rejects.toThrow('get_iroh_relays: config poisoned');
    });

    it('wraps plain-string rejections (production Tauri shape)', async () => {
      mockInvoke.mockRejectedValueOnce('config poisoned');
      await expect(getIrohRelays()).rejects.toThrow('get_iroh_relays: config poisoned');
    });
  });

  describe('setIrohRelays', () => {
    it('passes the relays array as { relays } and returns the authoritative info', async () => {
      const fixture: IrohRelayInfo = { relays: ['https://a.example'], custom: true };
      mockInvoke.mockResolvedValueOnce(fixture);
      const result = await setIrohRelays(['https://a.example']);
      expect(mockInvoke).toHaveBeenCalledWith('set_iroh_relays', {
        relays: ['https://a.example'],
      });
      expect(result).toEqual(fixture);
    });

    it('wraps a rejected set_iroh_relays and surfaces the error string', async () => {
      mockInvoke.mockRejectedValueOnce('invalid URL: missing scheme');
      await expect(setIrohRelays(['not-a-url'])).rejects.toThrow(
        'set_iroh_relays: invalid URL: missing scheme',
      );
    });

    it('wraps Error-object rejections as well', async () => {
      mockInvoke.mockRejectedValueOnce(new Error('cap exceeded'));
      await expect(setIrohRelays(Array(9).fill('https://a.example'))).rejects.toThrow(
        'set_iroh_relays: cap exceeded',
      );
    });
  });

  describe('addIrohRelay', () => {
    it('passes the url as { url } and returns the materialized custom list', async () => {
      const fixture: IrohRelayInfo = {
        relays: ['https://a.example', 'https://new.relay.example'],
        custom: true,
      };
      mockInvoke.mockResolvedValueOnce(fixture);
      const result = await addIrohRelay('https://new.relay.example');
      expect(mockInvoke).toHaveBeenCalledWith('add_iroh_relay', {
        url: 'https://new.relay.example',
      });
      expect(result).toEqual(fixture);
    });

    it('wraps a rejected add_iroh_relay and surfaces the error string', async () => {
      mockInvoke.mockRejectedValueOnce('invalid URL: missing scheme');
      await expect(addIrohRelay('not-a-url')).rejects.toThrow(
        'add_iroh_relay: invalid URL: missing scheme',
      );
    });

    it('wraps Error-object rejections as well', async () => {
      mockInvoke.mockRejectedValueOnce(new Error('cap exceeded'));
      await expect(addIrohRelay('https://a.example')).rejects.toThrow(
        'add_iroh_relay: cap exceeded',
      );
    });
  });

  describe('removeIrohRelay', () => {
    it('passes the url as { url } and returns the authoritative info', async () => {
      const fixture: IrohRelayInfo = { relays: ['https://a.example'], custom: true };
      mockInvoke.mockResolvedValueOnce(fixture);
      const result = await removeIrohRelay('https://b.example');
      expect(mockInvoke).toHaveBeenCalledWith('remove_iroh_relay', {
        url: 'https://b.example',
      });
      expect(result).toEqual(fixture);
    });

    it('wraps a rejected remove_iroh_relay (last-relay reject) and surfaces the error', async () => {
      mockInvoke.mockRejectedValueOnce('cannot remove the last relay; reset to recommended instead');
      await expect(removeIrohRelay('https://a.example')).rejects.toThrow(
        'remove_iroh_relay: cannot remove the last relay; reset to recommended instead',
      );
    });

    it('wraps Error-object rejections as well', async () => {
      mockInvoke.mockRejectedValueOnce(new Error('config poisoned'));
      await expect(removeIrohRelay('https://a.example')).rejects.toThrow(
        'remove_iroh_relay: config poisoned',
      );
    });
  });

  describe('resetIrohRelays', () => {
    it('invokes reset_iroh_relays and returns the recommended defaults info', async () => {
      const fixture: IrohRelayInfo = {
        relays: ['https://use1.relay.example', 'https://euw1.relay.example'],
        custom: false,
      };
      mockInvoke.mockResolvedValueOnce(fixture);
      const result = await resetIrohRelays();
      expect(mockInvoke).toHaveBeenCalledWith('reset_iroh_relays');
      expect(result).toEqual(fixture);
    });

    it('wraps rejections with the IPC-name prefix', async () => {
      mockInvoke.mockRejectedValueOnce(new Error('config poisoned'));
      await expect(resetIrohRelays()).rejects.toThrow('reset_iroh_relays: config poisoned');
    });
  });

  // ZEB-365: these two commands take multi-word args, so the JS->Rust key
  // casing must match. The backend commands are bare `#[tauri::command]`
  // (camelCase, the app convention), so the adapter MUST send camelCase. These
  // tests pin the keys against a regression to snake_case, which silently
  // breaks the Tauri arg boundary ("missing required key invite_url").
  describe('camelCase arg contract (ZEB-365)', () => {
    it('redeemInviteIroh invokes with a camelCase inviteUrl key', async () => {
      mockInvoke.mockResolvedValueOnce({ status: 'joined', communityId: 'abc' });
      await redeemInviteIroh('harmony://invite/xyz');
      expect(mockInvoke).toHaveBeenCalledWith('connectivity_redeem_invite_iroh', {
        inviteUrl: 'harmony://invite/xyz',
      });
    });

    it('discoverIdentity invokes with a camelCase identityPubHex key', async () => {
      mockInvoke.mockResolvedValueOnce(null);
      const key = 'ab'.repeat(64);
      await discoverIdentity(key);
      expect(mockInvoke).toHaveBeenCalledWith('connectivity_discover_identity', {
        identityPubHex: key,
      });
    });
  });
});
