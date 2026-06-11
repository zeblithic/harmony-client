// ZEB-433: conditional close-to-tray handler semantics.

import { describe, it, expect, vi } from 'vitest';
import {
  makeCloseRequestedHandler,
  makeTrayResidentNotifier,
  TRAY_NOTICE_KEY,
  type CloseDeps,
} from './window-close';

function memStorage(initial: Record<string, string> = {}) {
  const map = new Map(Object.entries(initial));
  return {
    getItem: (k: string) => (map.has(k) ? map.get(k)! : null),
    setItem: (k: string, v: string) => {
      map.set(k, v);
    },
    map,
  };
}

function deps(overrides: Partial<CloseDeps> = {}) {
  return {
    hide: vi.fn().mockResolvedValue(undefined),
    notifyTrayResident: vi.fn().mockResolvedValue(true),
    storage: memStorage(),
    ...overrides,
  };
}

function event() {
  return { preventDefault: vi.fn() };
}

/** Let the fire-and-forget notify chain settle. */
function flush() {
  return new Promise((r) => setTimeout(r, 0));
}

describe('makeCloseRequestedHandler', () => {
  it('no tray → close proceeds: no preventDefault, no hide, no notice', async () => {
    const d = deps();
    const handler = makeCloseRequestedHandler(false, d);
    const e = event();
    await handler(e);
    expect(e.preventDefault).not.toHaveBeenCalled();
    expect(d.hide).not.toHaveBeenCalled();
    expect(d.notifyTrayResident).not.toHaveBeenCalled();
  });

  it('tray active → hides, then prevents close, notifies once, persists the guard', async () => {
    const storage = memStorage();
    const hide = vi.fn().mockResolvedValue(undefined);
    const d = deps({ storage, hide });
    const handler = makeCloseRequestedHandler(true, d);
    const e = event();
    await handler(e);
    await flush();
    expect(hide).toHaveBeenCalledOnce();
    expect(e.preventDefault).toHaveBeenCalledOnce();
    // preventDefault must come AFTER a successful hide — a rejected hide
    // falls through to a real close, which requires not preventing first.
    expect(hide.mock.invocationCallOrder[0]).toBeLessThan(
      e.preventDefault.mock.invocationCallOrder[0],
    );
    expect(d.notifyTrayResident).toHaveBeenCalledOnce();
    expect(storage.getItem(TRAY_NOTICE_KEY)).toBe('1');
  });

  it('hide() rejecting → close proceeds: no preventDefault, no notice, no throw', async () => {
    const d = deps({ hide: vi.fn().mockRejectedValue(new Error('hide failed')) });
    const handler = makeCloseRequestedHandler(true, d);
    const e = event();
    await expect(handler(e)).resolves.toBeUndefined();
    expect(e.preventDefault).not.toHaveBeenCalled();
    expect(d.notifyTrayResident).not.toHaveBeenCalled();
  });

  it('second close in the same session hides again but does not re-attempt the notice', async () => {
    const d = deps();
    const handler = makeCloseRequestedHandler(true, d);
    await handler(event());
    await handler(event());
    await flush();
    expect(d.hide).toHaveBeenCalledTimes(2);
    expect(d.notifyTrayResident).toHaveBeenCalledOnce();
  });

  it('guard already persisted from a prior session → hides without notifying', async () => {
    const d = deps({ storage: memStorage({ [TRAY_NOTICE_KEY]: '1' }) });
    const handler = makeCloseRequestedHandler(true, d);
    await handler(event());
    await flush();
    expect(d.hide).toHaveBeenCalledOnce();
    expect(d.notifyTrayResident).not.toHaveBeenCalled();
  });

  it('notice not dispatched (e.g. permission denied) → guard NOT persisted, retry next session', async () => {
    const storage = memStorage();
    const d = deps({
      storage,
      notifyTrayResident: vi.fn().mockResolvedValue(false),
    });
    const handler = makeCloseRequestedHandler(true, d);
    await handler(event());
    await flush();
    expect(d.notifyTrayResident).toHaveBeenCalledOnce();
    // Guard stays unset so a later session (where permission may have been
    // granted) attempts the notice again.
    expect(storage.getItem(TRAY_NOTICE_KEY)).toBeNull();
    // ...but the session flag still bounds attempts within this run.
    await handler(event());
    await flush();
    expect(d.notifyTrayResident).toHaveBeenCalledOnce();
  });

  it('storage throwing still hides; notice attempted at most once per session', async () => {
    const broken = {
      getItem: () => {
        throw new Error('denied');
      },
      setItem: () => {
        throw new Error('denied');
      },
    };
    const d = deps({ storage: broken });
    const handler = makeCloseRequestedHandler(true, d);
    await handler(event());
    await handler(event());
    await flush();
    expect(d.hide).toHaveBeenCalledTimes(2);
    expect(d.notifyTrayResident).toHaveBeenCalledOnce();
  });

  it('a rejecting or throwing notifier never breaks the hide path', async () => {
    const rejecting = deps({
      notifyTrayResident: vi.fn().mockRejectedValue(new Error('no notifications')),
    });
    await expect(makeCloseRequestedHandler(true, rejecting)(event())).resolves.toBeUndefined();
    await flush();
    expect(rejecting.hide).toHaveBeenCalledOnce();

    const throwing = deps({
      notifyTrayResident: vi.fn(() => {
        throw new Error('sync throw');
      }) as unknown as CloseDeps['notifyTrayResident'],
    });
    await expect(makeCloseRequestedHandler(true, throwing)(event())).resolves.toBeUndefined();
    expect(throwing.hide).toHaveBeenCalledOnce();
  });
});

describe('makeTrayResidentNotifier', () => {
  it('sends and resolves true when permission already granted', async () => {
    const send = vi.fn();
    const notify = makeTrayResidentNotifier({
      isPermissionGranted: async () => true,
      requestPermission: async () => 'denied',
      sendNotification: send,
    });
    await expect(notify()).resolves.toBe(true);
    expect(send).toHaveBeenCalledOnce();
    expect(send.mock.calls[0][0].title).toContain('still running');
  });

  it('requests permission when not granted; sends and resolves true on grant', async () => {
    const send = vi.fn();
    const request = vi.fn().mockResolvedValue('granted');
    const notify = makeTrayResidentNotifier({
      isPermissionGranted: async () => false,
      requestPermission: request,
      sendNotification: send,
    });
    await expect(notify()).resolves.toBe(true);
    expect(request).toHaveBeenCalledOnce();
    expect(send).toHaveBeenCalledOnce();
  });

  it('denied permission → no send, resolves false', async () => {
    const send = vi.fn();
    const notify = makeTrayResidentNotifier({
      isPermissionGranted: async () => false,
      requestPermission: async () => 'denied',
      sendNotification: send,
    });
    await expect(notify()).resolves.toBe(false);
    expect(send).not.toHaveBeenCalled();
  });

  it('permission API throwing → resolves false, no send', async () => {
    const send = vi.fn();
    const notify = makeTrayResidentNotifier({
      isPermissionGranted: async () => {
        throw new Error('no plugin');
      },
      requestPermission: async () => 'denied',
      sendNotification: send,
    });
    await expect(notify()).resolves.toBe(false);
    expect(send).not.toHaveBeenCalled();
  });

  it('sendNotification throwing → resolves false (guard not persisted upstream)', async () => {
    const notify = makeTrayResidentNotifier({
      isPermissionGranted: async () => true,
      requestPermission: async () => 'granted',
      sendNotification: () => {
        throw new Error('send failed');
      },
    });
    await expect(notify()).resolves.toBe(false);
  });
});
