import { describe, it, expect, vi } from 'vitest';
import { ProfileBroadcastService } from '../profile-broadcast-service';
import type { TauriAdapter } from '../zenoh-service';

describe('ProfileBroadcastService', () => {
  it('service_subscribe_returns_handle', async () => {
    const invoke = vi.fn(async (cmd: string, args?: unknown) => {
      expect(cmd).toBe('subscribe_peer_profile');
      expect(args).toEqual({ peerAddr: 'abcd1234' });
      return 42;
    });
    const adapter = {
      invoke,
      listen: vi.fn(async () => () => {}),
    } as unknown as TauriAdapter;
    const svc = new ProfileBroadcastService(adapter);
    const id = await svc.subscribe('abcd1234');
    expect(id).toBe(42);
    expect(invoke).toHaveBeenCalledOnce();
  });

  it('service_list_shared_set_returns_ids', async () => {
    const invoke = vi.fn(async (cmd: string, args?: unknown) => {
      expect(cmd).toBe('list_shared_in_profile_communities');
      expect(args).toEqual({});
      return ['aa'.repeat(16), 'bb'.repeat(16)];
    });
    const adapter = {
      invoke,
      listen: vi.fn(async () => () => {}),
    } as unknown as TauriAdapter;
    const svc = new ProfileBroadcastService(adapter);
    const ids = await svc.listSharedSet();
    expect(ids).toHaveLength(2);
    expect(ids[0]).toBe('aa'.repeat(16));
    expect(ids[1]).toBe('bb'.repeat(16));
    expect(invoke).toHaveBeenCalledOnce();
  });
});
