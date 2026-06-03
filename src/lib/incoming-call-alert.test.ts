// src/lib/incoming-call-alert.test.ts
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { createIncomingCallAlerter, type AlerterDeps } from './incoming-call-alert';
import { createDefaultIncomingCallAlerter } from './incoming-call-alert';

/** Flush all pending microtasks (escalate() chains several awaits). */
const tick = () => new Promise((r) => setTimeout(r, 0));

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

  it('dispose cancels in-flight attention when a call is still ringing', async () => {
    const a = createIncomingCallAlerter(d);
    await a.notify({ id: 'c1', title: 'Incoming call', body: 'Alice is calling' });
    (d.requestUserAttention as ReturnType<typeof vi.fn>).mockClear();
    a.dispose();
    expect(d.requestUserAttention).toHaveBeenCalledWith(false);
  });

  it('REGRESSION — re-escalates when the window loses focus while a call still rings (Cursor High)', async () => {
    let focused = true;
    d.isFocused = vi.fn(async () => focused);
    const a = createIncomingCallAlerter(d);
    await a.notify({ id: 'c1', title: 'Incoming call', body: 'Alice is calling' });
    // Focused at arrival → the in-app toast suffices, no OS escalation yet.
    expect(d.sendNotification).not.toHaveBeenCalled();
    expect(d.requestUserAttention).not.toHaveBeenCalledWith(true);
    // Now the user switches away while the call is still ringing.
    focused = false;
    d._fireFocus(false);
    await tick();
    expect(d.sendNotification).toHaveBeenCalledWith({ title: 'Incoming call', body: 'Alice is calling' });
    expect(d.requestUserAttention).toHaveBeenCalledWith(true);
  });

  it('REGRESSION — does not arm attention if focus is regained during the permission await (Cursor Medium)', async () => {
    let focused = false;
    d.isFocused = vi.fn(async () => focused);
    // Simulate the user focusing the app *during* the permission await.
    d.isPermissionGranted = vi.fn(async () => { focused = true; return true; });
    const a = createIncomingCallAlerter(d);
    await a.notify({ id: 'c1', title: 'Incoming call', body: 'Alice is calling' });
    // First focus gate saw unfocused → requested permission; that flipped focus;
    // the post-permission gate sees focused → bail before arming.
    expect(d.sendNotification).not.toHaveBeenCalled();
    expect(d.requestUserAttention).not.toHaveBeenCalledWith(true);
  });

  it('REGRESSION — dispose during a pending notify never re-arms OS attention (CodeRabbit TOCTOU)', async () => {
    const a = createIncomingCallAlerter(d);
    // Tear down mid-flight, during the permission await of escalate().
    d.isPermissionGranted = vi.fn(async () => { a.dispose(); return true; });
    await a.notify({ id: 'c1', title: 'Incoming call', body: 'Alice is calling' });
    await tick();
    // The resumed escalate() must bail at its re-validation, not arm attention.
    expect(d.requestUserAttention).not.toHaveBeenCalledWith(true);
    expect(d.sendNotification).not.toHaveBeenCalled();
  });

  it('REGRESSION — clear during the attention-arm await ends with attention cancelled (Cursor R3)', async () => {
    // Hold the attention(true) arm open so we can clear() while it is in flight.
    let releaseArm!: () => void;
    d.requestUserAttention = vi.fn((critical: boolean) =>
      critical ? new Promise<void>((r) => { releaseArm = r; }) : Promise.resolve(),
    );
    const a = createIncomingCallAlerter(d);
    const p = a.notify({ id: 'c1', title: 'Incoming call', body: 'Alice is calling' });
    await tick(); // let escalate() reach the (now-pending) arm await
    await a.clear('c1'); // call accepted/declined/timed-out while the arm is in flight
    releaseArm(); // let the arm resolve
    await p;
    await tick();
    // The final OS attention state must be cancelled, not left bouncing.
    const calls = (d.requestUserAttention as ReturnType<typeof vi.fn>).mock.calls.map((c) => c[0]);
    expect(calls.at(-1)).toBe(false);
  });
});

describe('createDefaultIncomingCallAlerter (non-Tauri)', () => {
  it('returns a no-op alerter outside Tauri — methods never throw', async () => {
    // jsdom has window but no Tauri internals → isTauri() === false.
    const a = await createDefaultIncomingCallAlerter();
    await expect(a.notify({ id: 'c1', title: 't', body: 'b' })).resolves.toBeUndefined();
    await expect(a.clear('c1')).resolves.toBeUndefined();
    expect(() => a.dispose()).not.toThrow();
  });
});
