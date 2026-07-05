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
});
