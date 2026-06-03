// src/lib/incoming-call-alert.test.ts
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { createIncomingCallAlerter, type AlerterDeps } from './incoming-call-alert';

function makeDeps() {
  let focusCb: ((f: boolean) => void) | undefined;
  let activationCb: (() => void) | undefined;
  const deps: AlerterDeps & {
    _fireFocus: (f: boolean) => void;
    _fireActivation: () => void;
  } = {
    isPermissionGranted: vi.fn(async () => true),
    requestPermission: vi.fn(async () => 'granted' as const),
    sendNotification: vi.fn(),
    isFocused: vi.fn(async () => false), // default: unfocused
    onFocusChanged: vi.fn(async (cb: (f: boolean) => void) => {
      focusCb = cb;
      return () => {};
    }),
    requestUserAttention: vi.fn(async () => {}),
    raiseWindow: vi.fn(async () => {}),
    registerActivation: vi.fn(async (cb: () => void) => {
      activationCb = cb;
    }),
    _fireFocus: (f: boolean) => focusCb?.(f),
    _fireActivation: () => activationCb?.(),
  };
  return deps;
}

describe('IncomingCallAlerter', () => {
  let d: ReturnType<typeof makeDeps>;
  beforeEach(() => { d = makeDeps(); });

  it('unfocused: notify sends an OS notification AND requests Critical attention', async () => {
    const a = createIncomingCallAlerter(d);
    await a.notify({ id: 'c1', title: 'Incoming call', body: 'Alice is calling' });
    expect(d.sendNotification).toHaveBeenCalledWith({ title: 'Incoming call', body: 'Alice is calling' });
    expect(d.requestUserAttention).toHaveBeenCalledWith(true);
  });

  it('REGRESSION GUARD — focused: notify is a strict no-op', async () => {
    d.isFocused = vi.fn(async () => true);
    const a = createIncomingCallAlerter(d);
    await a.notify({ id: 'c1', title: 'Incoming call', body: 'Alice is calling' });
    expect(d.sendNotification).not.toHaveBeenCalled();
    expect(d.requestUserAttention).not.toHaveBeenCalled();
  });

  it('clear cancels attention for the active id', async () => {
    const a = createIncomingCallAlerter(d);
    await a.notify({ id: 'c1', title: 'Incoming call', body: 'Alice is calling' });
    (d.requestUserAttention as ReturnType<typeof vi.fn>).mockClear();
    await a.clear('c1');
    expect(d.requestUserAttention).toHaveBeenCalledWith(false);
  });

  it('permission denied: skips notification but STILL requests attention (dock bounce)', async () => {
    d.isPermissionGranted = vi.fn(async () => false);
    d.requestPermission = vi.fn(async () => 'denied' as const);
    const a = createIncomingCallAlerter(d);
    await a.notify({ id: 'c1', title: 'Incoming call', body: 'Alice is calling' });
    expect(d.sendNotification).not.toHaveBeenCalled();
    expect(d.requestUserAttention).toHaveBeenCalledWith(true);
  });

  it('focus regained while ringing auto-clears the escalation', async () => {
    const a = createIncomingCallAlerter(d);
    await a.notify({ id: 'c1', title: 'Incoming call', body: 'Alice is calling' });
    (d.requestUserAttention as ReturnType<typeof vi.fn>).mockClear();
    d._fireFocus(true);
    await Promise.resolve(); // let the async clear settle
    expect(d.requestUserAttention).toHaveBeenCalledWith(false);
  });

  it('notification activation raises the window', async () => {
    const a = createIncomingCallAlerter(d);
    void a; // construction registers the activation callback
    d._fireActivation();
    await Promise.resolve();
    expect(d.raiseWindow).toHaveBeenCalled();
  });
});
