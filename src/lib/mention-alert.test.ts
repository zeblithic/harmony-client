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
  const calls = {
    inc: [] as string[],
    incArgs: [] as Array<[string, string]>,
    toast: [] as string[],
    os: 0,
    osBody: '',
  };
  const deps: MentionAlertDeps = {
    getSelfOwnerId: () => ME,
    isActiveChannel: () => false,
    isFocused: () => true,
    resolve: () => 'notify' as NotificationAction,
    incMention: (communityId, channelId) => {
      calls.inc.push(channelId);
      calls.incArgs.push([communityId, channelId]);
    },
    getChannelName: (_communityId, channelId) => `#${channelId}`,
    showToast: (m) => calls.toast.push(m),
    sendOsNotification: (o) => {
      calls.os++;
      calls.osBody = o.body;
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
    const { svc, calls } = harness({ isActiveChannel: () => true, isFocused: () => true });
    await svc.onMessage('c1', 'ch1', msg());
    expect(calls.inc).toEqual([]);
    expect(calls.toast).toEqual([]);
    expect(calls.os).toBe(0);
  });

  it('still notifies for a mention in the active channel when unfocused', async () => {
    // Active-channel suppression only applies when the window is focused; a
    // background mention in the same channel must still notify (OS).
    const { svc, calls } = harness({ isActiveChannel: () => true, isFocused: () => false });
    await svc.onMessage('c1', 'ch1', msg());
    expect(calls.inc).toEqual(['ch1']);
    expect(calls.os).toBe(1);
  });

  it('still notifies for a mention in a non-active channel even when focused', async () => {
    const { svc, calls } = harness({ isActiveChannel: () => false, isFocused: () => true });
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
    const { svc, calls } = harness({ resolve: () => 'dot_only', isActiveChannel: () => false });
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
      const { svc, calls } = harness({ resolve: () => action, isActiveChannel: () => false });
      await svc.onMessage('c1', 'ch1', msg());
      expect(calls.inc).toEqual(['ch1']);
      expect(calls.toast.length).toBe(1);
      expect(calls.os).toBe(0);
    }
  });

  it('resolve is called with loud + sender + community', async () => {
    const resolve = vi.fn(() => 'notify' as NotificationAction);
    const { svc } = harness({ resolve, isActiveChannel: () => false });
    await svc.onMessage('c1', 'ch1', msg());
    expect(resolve).toHaveBeenCalledWith('loud', SENDER, 'c1');
  });

  it('threads communityId + channelId through the seen-check, nav, and name', async () => {
    const isActiveChannel = vi.fn(() => false);
    const getChannelName = vi.fn(() => 'general');
    const { svc, calls } = harness({ isActiveChannel, getChannelName });
    await svc.onMessage('c1', 'ch1', msg());
    // Community channels aren't nav nodes, so every dep must receive the
    // (communityId, channelId) pair — not a bare channel id.
    expect(isActiveChannel).toHaveBeenCalledWith('c1', 'ch1');
    expect(getChannelName).toHaveBeenCalledWith('c1', 'ch1');
    expect(calls.incArgs).toEqual([['c1', 'ch1']]);
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

  it('uses the resolved channel name in the toast body', async () => {
    const { svc, calls } = harness({
      isActiveChannel: () => false,
      getChannelName: () => 'general',
    });
    await svc.onMessage('c1', 'ch1', msg());
    expect(calls.toast[0]).toBe('You were mentioned in general');
  });

  it('uses the resolved channel name in the OS-notification body (unfocused)', async () => {
    const { svc, calls } = harness({
      isFocused: () => false,
      getChannelName: () => 'general',
    });
    await svc.onMessage('c1', 'ch1', msg());
    expect(calls.osBody).toBe('You were mentioned in general');
  });

  it('does not notify for my own message that lists me', async () => {
    const { svc, calls } = harness({ isActiveChannel: () => false });
    await svc.onMessage('c1', 'ch1', msg({ author: ME, mentions: [ME] }));
    expect(calls.inc).toEqual([]);
    expect(calls.toast).toEqual([]);
    expect(calls.os).toBe(0);
  });
});
