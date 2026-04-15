import { describe, it, expect, vi, beforeEach } from 'vitest';
import { createMockAdapter } from './test-utils';
import { MailService } from './mail-service';
import type { MailMessageDetail } from './types';

describe('MailService — Phase 2 sync and lazy body fetch', () => {
  let svc: MailService;

  beforeEach(() => {
    svc = new MailService();
  });

  it('mirrors mail-sync-status events into syncState/syncError', async () => {
    const { adapter, emit } = createMockAdapter();
    // list_mail returns empty so connectAdapter completes cleanly.
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue([]);
    await svc.connectAdapter(adapter);

    expect(svc.syncState).toBe('idle');
    expect(svc.syncError).toBeNull();

    emit('mail-sync-status', { state: 'syncing' });
    expect(svc.syncState).toBe('syncing');
    expect(svc.syncError).toBeNull();

    emit('mail-sync-status', { state: 'error', error: 'boom' });
    expect(svc.syncState).toBe('error');
    expect(svc.syncError).toBe('boom');

    emit('mail-sync-status', { state: 'idle' });
    expect(svc.syncState).toBe('idle');
    expect(svc.syncError).toBeNull();
  });

  it('triggers onChange when syncState actually changes', async () => {
    const { adapter, emit } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue([]);
    await svc.connectAdapter(adapter);

    const onChange = vi.fn();
    svc.onChange = onChange;
    onChange.mockClear();

    // First change: idle → syncing. Should fire.
    emit('mail-sync-status', { state: 'syncing' });
    expect(onChange).toHaveBeenCalledTimes(1);

    // Redundant event with same state+error — should NOT refire.
    emit('mail-sync-status', { state: 'syncing' });
    expect(onChange).toHaveBeenCalledTimes(1);

    // State change from syncing → idle.
    emit('mail-sync-status', { state: 'idle' });
    expect(onChange).toHaveBeenCalledTimes(2);
  });

  it('refresh() calls invoke("refresh_mail")', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue([]);
    await svc.connectAdapter(adapter);

    (adapter.invoke as ReturnType<typeof vi.fn>).mockClear();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue(undefined);

    await svc.refresh();
    expect(adapter.invoke).toHaveBeenCalledWith('refresh_mail', {});
  });

  it('refresh() is a no-op before connectAdapter (no throw)', async () => {
    // No adapter connected yet — must silently no-op.
    await expect(svc.refresh()).resolves.toBeUndefined();
  });

  it('getMessage short-circuits Local entries to a single get_mail call', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue([]);
    await svc.connectAdapter(adapter);

    const local: MailMessageDetail = {
      messageCid: 'cid-local',
      messageId: 'mid-1',
      subject: 'hi',
      body: 'cached body',
      senderAddress: 'sender',
      recipients: [],
      timestamp: 0,
      attachments: [],
      isReply: false,
      isForward: false,
      bodyState: 'local',
    };
    (adapter.invoke as ReturnType<typeof vi.fn>).mockClear();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValueOnce(local);

    const result = await svc.getMessage('cid-local');
    expect(result).toEqual(local);
    expect(adapter.invoke).toHaveBeenCalledTimes(1);
    expect(adapter.invoke).toHaveBeenCalledWith('get_mail', { messageCid: 'cid-local' });
  });

  it('getMessage routes Pending entries through fetch_mail_body', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue([]);
    await svc.connectAdapter(adapter);

    const pending: MailMessageDetail = {
      messageCid: 'cid-pending',
      messageId: 'mid-2',
      subject: 's',
      body: '',
      senderAddress: 'sender',
      recipients: [],
      timestamp: 0,
      attachments: [],
      isReply: false,
      isForward: false,
      bodyState: 'pending',
    };
    const fetched: MailMessageDetail = { ...pending, body: 'fetched body', bodyState: 'local' };

    (adapter.invoke as ReturnType<typeof vi.fn>).mockClear();
    (adapter.invoke as ReturnType<typeof vi.fn>)
      .mockResolvedValueOnce(pending)   // first call: get_mail returns Pending stub
      .mockResolvedValueOnce(fetched);  // second call: fetch_mail_body returns Local

    const result = await svc.getMessage('cid-pending');
    expect(result).toEqual(fetched);
    expect(result?.bodyState).toBe('local');
    expect(adapter.invoke).toHaveBeenCalledTimes(2);
    expect(adapter.invoke).toHaveBeenNthCalledWith(1, 'get_mail', { messageCid: 'cid-pending' });
    expect(adapter.invoke).toHaveBeenNthCalledWith(2, 'fetch_mail_body', { messageCid: 'cid-pending' });
  });

  it('getMessage returns null on error', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue([]);
    await svc.connectAdapter(adapter);

    (adapter.invoke as ReturnType<typeof vi.fn>).mockClear();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockRejectedValueOnce(new Error('backend down'));

    const result = await svc.getMessage('cid-missing');
    expect(result).toBeNull();
  });
});
