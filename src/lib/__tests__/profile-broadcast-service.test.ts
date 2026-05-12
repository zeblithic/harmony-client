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
});
