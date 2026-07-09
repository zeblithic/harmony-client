import { describe, it, expect, vi } from 'vitest';
import { MentionAlertService, type MentionAlertDeps } from './mention-alert';
import type { NotificationAction } from './types';

const ME = 'aa'.repeat(16);
const SENDER = 'bb'.repeat(16);
const msg = (over: Partial<{ mentions: string[]; author: string }> = {}) => ({
  messageId: 'm',
  communityId: 'c1',
  channelId: 'ch1',
  author: over.author ?? SENDER,
  at: { wallMs: 0, logical: 0, deviceId: 'd' },
  body: [] as number[],
  mentions: over.mentions ?? [ME],
});

function harness(over: Partial<MentionAlertDeps> = {}) {
  const calls = { inc: [] as string[], toast: [] as string[], os: 0 };
  const deps: MentionAlertDeps = {
    getSelfOwnerId: () => ME,
    getActiveChannelId: () => null,
    isFocused: () => true,
    resolve: () => 'notify' as NotificationAction,
    incMention: (id) => calls.inc.push(id),
    showToast: (m) => calls.toast.push(m),
    sendOsNotification: () => {
      calls.os++;
    },
    ...over,
  };
  return { svc: new MentionAlertService(deps), calls };
}

describe('MentionAlertService (ZEB-662)', () => {
  it('ignores a message that does not mention me', async () => {
    const { svc, calls } = harness();
    await svc.onMessage('c1', 'ch1', msg({ mentions: [SENDER] }));
    expect(calls.inc).toEqual([]);
    expect(calls.toast).toEqual([]);
    expect(calls.os).toBe(0);
  });

  it('suppresses when the mentioned channel is active and focused', async () => {
    const { svc, calls } = harness({ getActiveChannelId: () => 'ch1', isFocused: () => true });
    await svc.onMessage('c1', 'ch1', msg());
    expect(calls.inc).toEqual([]);
    expect(calls.toast).toEqual([]);
    expect(calls.os).toBe(0);
  });

  it('still notifies for a mention in a non-active channel even when focused', async () => {
    const { svc, calls } = harness({ getActiveChannelId: () => 'other', isFocused: () => true });
    await svc.onMessage('c1', 'ch1', msg());
    expect(calls.inc).toEqual(['ch1']);
    expect(calls.toast.length).toBe(1);
    expect(calls.os).toBe(0);
  });

  it('silent action: no dot, no toast, no OS', async () => {
    const { svc, calls } = harness({ resolve: () => 'silent' });
    await svc.onMessage('c1', 'ch1', msg());
    expect(calls.inc).toEqual([]);
    expect(calls.toast).toEqual([]);
    expect(calls.os).toBe(0);
  });

  it('dot_only: nav dot, no toast, no OS', async () => {
    const { svc, calls } = harness({ resolve: () => 'dot_only', getActiveChannelId: () => 'other' });
    await svc.onMessage('c1', 'ch1', msg());
    expect(calls.inc).toEqual(['ch1']);
    expect(calls.toast).toEqual([]);
    expect(calls.os).toBe(0);
  });

  it('notify + unfocused: nav dot + OS notification, no toast', async () => {
    const { svc, calls } = harness({ isFocused: () => false });
    await svc.onMessage('c1', 'ch1', msg());
    expect(calls.inc).toEqual(['ch1']);
    expect(calls.toast).toEqual([]);
    expect(calls.os).toBe(1);
  });

  it('sound/break_dnd behave like notify (toast when focused)', async () => {
    for (const action of ['sound', 'break_dnd'] as NotificationAction[]) {
      const { svc, calls } = harness({ resolve: () => action, getActiveChannelId: () => 'other' });
      await svc.onMessage('c1', 'ch1', msg());
      expect(calls.inc).toEqual(['ch1']);
      expect(calls.toast.length).toBe(1);
      expect(calls.os).toBe(0);
    }
  });

  it('resolve is called with loud + sender + community', async () => {
    const resolve = vi.fn(() => 'notify' as NotificationAction);
    const { svc } = harness({ resolve, getActiveChannelId: () => 'other' });
    await svc.onMessage('c1', 'ch1', msg());
    expect(resolve).toHaveBeenCalledWith('loud', SENDER, 'c1');
  });

  it('swallows an OS-notification throw', async () => {
    const { svc, calls } = harness({
      isFocused: () => false,
      sendOsNotification: () => {
        throw new Error('x');
      },
    });
    await expect(svc.onMessage('c1', 'ch1', msg())).resolves.toBeUndefined();
    expect(calls.inc).toEqual(['ch1']); // dot still recorded
  });

  it('no self owner id → ignored', async () => {
    const { svc, calls } = harness({ getSelfOwnerId: () => undefined });
    await svc.onMessage('c1', 'ch1', msg());
    expect(calls.inc).toEqual([]);
  });

  it('does not notify for my own message that lists me', async () => {
    const { svc, calls } = harness({ getActiveChannelId: () => 'other' });
    await svc.onMessage('c1', 'ch1', msg({ author: ME, mentions: [ME] }));
    expect(calls.inc).toEqual([]);
    expect(calls.toast).toEqual([]);
    expect(calls.os).toBe(0);
  });
});
