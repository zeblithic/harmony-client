import { describe, expect, it, vi } from 'vitest';
import { DmInviteService } from './dm-invite-service';
import { createMockAdapter } from './test-utils';

describe('DmInviteService', () => {
  it('fans out onPendingChanged for both invite events', async () => {
    const { adapter, emit } = createMockAdapter();
    const svc = new DmInviteService();
    await svc.connectAdapter(adapter);
    const cb = vi.fn();
    svc.onPendingChanged(cb);
    emit('dm-invite-received', {});
    emit('dm-invite-list-changed', {});
    expect(cb).toHaveBeenCalledTimes(2);
  });

  it('listPending invokes the verb and returns rows', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as any) = vi.fn().mockResolvedValue([
      { spaceIdHex: 'aa', inviterOwnerIdHex: 'bb', kind: 'd',
        memberOwnerIdsHex: ['bb', 'cc'], createdAtMs: 1, receivedAtMs: 2 },
    ]);
    const svc = new DmInviteService();
    await svc.connectAdapter(adapter);
    const rows = await svc.listPending();
    expect(adapter.invoke).toHaveBeenCalledWith('list_pending_dm_invites', {});
    expect(rows[0].inviterOwnerIdHex).toBe('bb');
  });

  it('accept/decline pass camelCase spaceId and normalize errors', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as any) = vi.fn().mockRejectedValue(new Error('no pending DM invite for space'));
    const svc = new DmInviteService();
    await svc.connectAdapter(adapter);
    await expect(svc.accept('aa')).rejects.toThrow('no pending DM invite for space');
    expect(adapter.invoke).toHaveBeenCalledWith('accept_dm_invite', { spaceId: 'aa' });
  });

  it('decline invokes the verb with camelCase spaceId and normalizes string rejections', async () => {
    const { adapter } = createMockAdapter();
    // Production Tauri rejections are plain strings (CLAUDE.md "Tauri IPC
    // error extraction") — decline must normalize them into Error like accept.
    (adapter.invoke as any) = vi.fn().mockRejectedValue('no pending DM invite for space');
    const svc = new DmInviteService();
    await svc.connectAdapter(adapter);
    await expect(svc.decline('bb')).rejects.toThrow('no pending DM invite for space');
    expect(adapter.invoke).toHaveBeenCalledWith('decline_dm_invite', { spaceId: 'bb' });
  });

  it('destroy unlistens, clears listeners, and allows reconnect', async () => {
    const { adapter, emit, unlisten } = createMockAdapter();
    const svc = new DmInviteService();
    await svc.connectAdapter(adapter);
    const cb = vi.fn();
    svc.onPendingChanged(cb);

    svc.destroy();
    expect(unlisten).toHaveBeenCalledTimes(2); // both event registrations torn down
    emit('dm-invite-received', {});
    expect(cb).not.toHaveBeenCalled(); // listener set cleared

    // Adapter nulled → the duplicate-init guard doesn't block reconnect.
    await svc.connectAdapter(adapter);
    const cb2 = vi.fn();
    svc.onPendingChanged(cb2);
    emit('dm-invite-list-changed', {});
    expect(cb2).toHaveBeenCalledTimes(1);
  });

  it('rolls back on partial listener registration failure so reconnect can retry', async () => {
    const { adapter, unlisten } = createMockAdapter();
    // First listen (dm-invite-received) succeeds, second rejects → the service
    // must undo the half-wired state instead of latching adapter forever.
    (adapter.listen as any) = vi
      .fn()
      .mockImplementationOnce((adapter.listen as any).getMockImplementation())
      .mockRejectedValueOnce(new Error('event bridge down'));
    const svc = new DmInviteService();
    await expect(svc.connectAdapter(adapter)).rejects.toThrow('event bridge down');
    expect(unlisten).toHaveBeenCalledTimes(1); // the successful registration was rolled back

    // Retry succeeds: guard was cleared, both events wire up.
    (adapter.listen as any) = vi.fn().mockResolvedValue(() => {});
    await svc.connectAdapter(adapter);
    expect(adapter.listen).toHaveBeenCalledTimes(2);
  });
});
