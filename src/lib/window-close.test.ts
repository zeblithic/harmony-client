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
    notifyTrayResident: vi.fn(),
    storage: memStorage(),
    ...overrides,
  };
}

function event() {
  return { preventDefault: vi.fn() };
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

  it('tray active → prevents close, hides, notifies once, persists the guard', async () => {
    const storage = memStorage();
    const d = deps({ storage });
    const handler = makeCloseRequestedHandler(true, d);
    const e = event();
    await handler(e);
    expect(e.preventDefault).toHaveBeenCalledOnce();
    expect(d.hide).toHaveBeenCalledOnce();
    expect(d.notifyTrayResident).toHaveBeenCalledOnce();
    expect(storage.getItem(TRAY_NOTICE_KEY)).toBe('1');
  });

  it('second close in the same session hides again but does not re-notify', async () => {
    const d = deps();
    const handler = makeCloseRequestedHandler(true, d);
    await handler(event());
    await handler(event());
    expect(d.hide).toHaveBeenCalledTimes(2);
    expect(d.notifyTrayResident).toHaveBeenCalledOnce();
  });

  it('guard already persisted from a prior session → hides without notifying', async () => {
    const d = deps({ storage: memStorage({ [TRAY_NOTICE_KEY]: '1' }) });
    const handler = makeCloseRequestedHandler(true, d);
    await handler(event());
    expect(d.hide).toHaveBeenCalledOnce();
    expect(d.notifyTrayResident).not.toHaveBeenCalled();
  });

  it('storage throwing still hides, notifies at most once per session', async () => {
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
    expect(d.hide).toHaveBeenCalledTimes(2);
    // getItem threw before `alreadyShown` could be confirmed → stays false →
    // notice fires, but the session flag bounds it to once.
    expect(d.notifyTrayResident).toHaveBeenCalledOnce();
  });

  it('a throwing notifier never breaks the hide path', async () => {
    const d = deps({
      notifyTrayResident: vi.fn(() => {
        throw new Error('no notifications');
      }),
    });
    const handler = makeCloseRequestedHandler(true, d);
    await expect(handler(event())).resolves.toBeUndefined();
    expect(d.hide).toHaveBeenCalledOnce();
  });
});

describe('makeTrayResidentNotifier', () => {
  function flush() {
    // The notifier is fire-and-forget; let its internal async chain settle.
    return new Promise((r) => setTimeout(r, 0));
  }

  it('sends when permission already granted', async () => {
    const send = vi.fn();
    const notify = makeTrayResidentNotifier({
      isPermissionGranted: async () => true,
      requestPermission: async () => 'denied',
      sendNotification: send,
    });
    notify();
    await flush();
    expect(send).toHaveBeenCalledOnce();
    expect(send.mock.calls[0][0].title).toContain('still running');
  });

  it('requests permission when not granted; sends on grant', async () => {
    const send = vi.fn();
    const request = vi.fn().mockResolvedValue('granted');
    const notify = makeTrayResidentNotifier({
      isPermissionGranted: async () => false,
      requestPermission: request,
      sendNotification: send,
    });
    notify();
    await flush();
    expect(request).toHaveBeenCalledOnce();
    expect(send).toHaveBeenCalledOnce();
  });

  it('denied permission → no send', async () => {
    const send = vi.fn();
    const notify = makeTrayResidentNotifier({
      isPermissionGranted: async () => false,
      requestPermission: async () => 'denied',
      sendNotification: send,
    });
    notify();
    await flush();
    expect(send).not.toHaveBeenCalled();
  });

  it('permission API throwing → no send, no unhandled rejection', async () => {
    const send = vi.fn();
    const notify = makeTrayResidentNotifier({
      isPermissionGranted: async () => {
        throw new Error('no plugin');
      },
      requestPermission: async () => 'denied',
      sendNotification: send,
    });
    notify();
    await flush();
    expect(send).not.toHaveBeenCalled();
  });
});
