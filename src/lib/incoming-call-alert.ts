// src/lib/incoming-call-alert.ts
//
// IncomingCallAlerter — escalates an incoming call to the OS (notification +
// window attention) when the app window is NOT focused. The in-app ring toast
// already covers the focused case, so escalation is a strict no-op while
// focused. Injectable deps + default factory (later task) + no-op outside Tauri,
// mirroring CallSession / VoiceSession. Reused by ZEB-360 group-DM calls via
// the call-shape-agnostic notify({id,title,body}) / clear(id) surface.

export interface AlerterDeps {
  isPermissionGranted: () => Promise<boolean>;
  requestPermission: () => Promise<'granted' | 'denied' | 'default'>;
  sendNotification: (opts: { title: string; body: string }) => void;
  isFocused: () => Promise<boolean>;
  /** Subscribe to focus changes; resolves to an unlisten fn. */
  onFocusChanged: (cb: (focused: boolean) => void) => Promise<() => void>;
  /** true → request Critical attention (persistent bounce/flash); false → cancel. */
  requestUserAttention: (critical: boolean) => Promise<void>;
  /** unminimize + show + setFocus the main window. */
  raiseWindow: () => Promise<void>;
  /** Register a notification-activation (click) callback. Optional. */
  registerActivation?: (cb: () => void) => Promise<void>;
}

export interface IncomingCallAlerter {
  /** Escalate an incoming call to the OS — no-op if the window is focused. */
  notify(opts: { id: string; title: string; body: string }): Promise<void>;
  /** Cancel attention + dismiss the escalation for this id. */
  clear(id: string): Promise<void>;
  /** Tear down the focus listener (call on app teardown). */
  dispose(): void;
}

class Alerter implements IncomingCallAlerter {
  private deps: AlerterDeps;
  /** Cached focus state; only a fallback for isFocusedSafe(). Defaults true (safe: no-op). */
  private focused = true;
  /**
   * The call currently ringing, or null. Escalation is focus-driven off this:
   * while a call is armed, losing focus (re-)escalates and regaining it cancels.
   */
  private ringing: { id: string; title: string; body: string } | null = null;
  /** Whether OS attention is currently armed for `ringing` (or claimed in-flight). */
  private escalated = false;
  private unlistenFocus: (() => void) | null = null;
  private disposed = false;

  constructor(deps: AlerterDeps) {
    this.deps = deps;
    // Best-effort focus tracking. Failures (non-Tauri) leave focused=true.
    void this.deps.isFocused().then((f) => { this.focused = f; }).catch(() => {});
    void this.deps
      .onFocusChanged((f) => this.onFocusChange(f))
      // If teardown beat the async registration, unlisten immediately.
      .then((un) => { if (this.disposed) un(); else this.unlistenFocus = un; })
      .catch(() => {});
    if (this.deps.registerActivation) {
      void this.deps
        .registerActivation(() => { void this.deps.raiseWindow().catch(() => {}); })
        .catch(() => {});
    }
  }

  private onFocusChange(focused: boolean): void {
    this.focused = focused;
    if (focused) {
      // Looking at the app → the in-app toast suffices; drop OS attention but
      // keep the call armed so re-unfocusing while it still rings re-escalates.
      if (this.escalated) {
        this.escalated = false;
        void this.deps.requestUserAttention(false).catch(() => {});
      }
    } else if (this.ringing && !this.escalated) {
      // Unfocused while a call is still ringing → (re-)escalate.
      void this.escalate();
    }
  }

  async notify(opts: { id: string; title: string; body: string }): Promise<void> {
    if (this.disposed) return;
    this.ringing = { ...opts };
    this.escalated = false;
    await this.escalate();
  }

  /**
   * Escalate the ringing call to the OS iff the window is unfocused. Idempotent
   * and re-entrant-safe: claims the escalation synchronously so a concurrent
   * trigger can't double-fire, then re-validates (disposed / cleared / newer
   * call / focus regained) after every await before arming OS attention. This
   * closes the TOCTOU windows where dispose()/clear()/focus could otherwise be
   * defeated by an in-flight notify().
   */
  private async escalate(): Promise<void> {
    const target = this.ringing;
    if (!target || this.disposed || this.escalated) return;
    this.escalated = true; // claim synchronously

    // If the user is already looking at the app, don't even request permission.
    if (await this.isFocusedSafe()) { this.escalated = false; return; }

    // Permission gates only the banner; the dock/taskbar attention needs no grant.
    let granted = false;
    try {
      granted = await this.deps.isPermissionGranted();
      if (!granted) granted = (await this.deps.requestPermission()) === 'granted';
    } catch { granted = false; }

    // Re-validate before the side effect: teardown, a clear()/newer call, or
    // focus regained during the permission await each cancel this escalation.
    if (this.disposed || this.ringing !== target || (await this.isFocusedSafe())) {
      this.escalated = false;
      return;
    }
    if (granted) {
      try { this.deps.sendNotification({ title: target.title, body: target.body }); } catch { /* ignore */ }
    }
    try { await this.deps.requestUserAttention(true); } catch { /* ignore */ }
    // Final re-validation — the arm above is itself an await. clear() or dispose()
    // can land while it's in flight (call accepted/declined/timed-out, or quit),
    // and their cancel(false) may have raced ahead of our arm(true), leaving a
    // stale dock/taskbar bounce. If the call is no longer ringing (cleared) or
    // we're torn down, cancel what we just armed. Gated on `ringing === null`
    // (not `!== target`) so a newer call that replaced this one isn't disarmed.
    if (this.disposed || this.ringing === null) {
      this.escalated = false;
      try { await this.deps.requestUserAttention(false); } catch { /* ignore */ }
    }
  }

  private async isFocusedSafe(): Promise<boolean> {
    try { return await this.deps.isFocused(); } catch { return this.focused; }
  }

  async clear(id: string): Promise<void> {
    if (this.ringing?.id !== id) return;
    this.ringing = null;
    const wasEscalated = this.escalated;
    this.escalated = false;
    // Load-bearing cancellation: stops the persistent bounce/flash. Programmatic
    // dismissal of an already-posted desktop banner is best-effort/unavailable
    // across platforms, so we only cancel attention here.
    if (wasEscalated) {
      try { await this.deps.requestUserAttention(false); } catch { /* ignore */ }
    }
  }

  dispose(): void {
    // Mark disposed FIRST so any in-flight escalate() bails at its next
    // re-validation instead of re-arming a dock/taskbar bounce after teardown
    // (e.g. SPA unmount or identity switch while a call is still ringing).
    this.disposed = true;
    const wasEscalated = this.escalated;
    this.ringing = null;
    this.escalated = false;
    if (wasEscalated) void this.deps.requestUserAttention(false).catch(() => {});
    if (this.unlistenFocus) { this.unlistenFocus(); this.unlistenFocus = null; }
  }
}

export function createIncomingCallAlerter(deps: AlerterDeps): IncomingCallAlerter {
  return new Alerter(deps);
}

import { isTauri } from '@tauri-apps/api/core';

function noopAlerter(): IncomingCallAlerter {
  return { notify: async () => {}, clear: async () => {}, dispose: () => {} };
}

/**
 * Build the alerter wired to the real Tauri plugin/window APIs. Outside Tauri
 * (web preview / unit tests) returns a no-op so callers need no guard. Dynamic
 * imports match App.svelte's pattern (no static @tauri-apps plugin import that
 * would break the web bundle).
 */
export async function createDefaultIncomingCallAlerter(): Promise<IncomingCallAlerter> {
  if (!isTauri()) return noopAlerter();
  try {
    const notif = await import('@tauri-apps/plugin-notification');
    const { getCurrentWebviewWindow } = await import('@tauri-apps/api/webviewWindow');
    const { UserAttentionType } = await import('@tauri-apps/api/window');
    const appWin = getCurrentWebviewWindow();
    const deps: AlerterDeps = {
      isPermissionGranted: () => notif.isPermissionGranted(),
      requestPermission: () => notif.requestPermission(),
      sendNotification: (o) => notif.sendNotification(o),
      isFocused: () => appWin.isFocused(),
      onFocusChanged: async (cb) => {
        const un = await appWin.onFocusChanged(({ payload }) => cb(payload));
        return un;
      },
      requestUserAttention: (critical) =>
        appWin.requestUserAttention(critical ? UserAttentionType.Critical : null),
      raiseWindow: async () => {
        await appWin.unminimize().catch(() => {});
        await appWin.show().catch(() => {});
        await appWin.setFocus().catch(() => {});
      },
      // onAction exists in @tauri-apps/plugin-notification@^2.3.x: wired here.
      // Activation (notification click) → raise. Best-effort: graceful degradation
      // per spec D2 (tray click also raises the window).
      registerActivation: notif.onAction
        ? async (cb) => { await notif.onAction(() => cb()); }
        : undefined,
    };
    return createIncomingCallAlerter(deps);
  } catch {
    return noopAlerter();
  }
}
