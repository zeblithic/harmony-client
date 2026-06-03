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
  /** Cached focus state; re-checked at notify() time. Defaults true (safe: no-op). */
  private focused = true;
  private activeId: string | null = null;
  private unlistenFocus: (() => void) | null = null;

  constructor(deps: AlerterDeps) {
    this.deps = deps;
    // Best-effort focus tracking. Failures (non-Tauri) leave focused=true, so
    // notify() simply no-ops rather than throwing.
    void this.deps.isFocused().then((f) => { this.focused = f; }).catch(() => {});
    void this.deps
      .onFocusChanged((f) => {
        this.focused = f;
        // Focusing the app drops the OS escalation; the in-app toast remains.
        if (f && this.activeId) void this.clear(this.activeId);
      })
      .then((un) => { this.unlistenFocus = un; })
      .catch(() => {});
    if (this.deps.registerActivation) {
      void this.deps
        .registerActivation(() => { void this.deps.raiseWindow().catch(() => {}); })
        .catch(() => {});
    }
  }

  async notify(opts: { id: string; title: string; body: string }): Promise<void> {
    // Re-check focus at call time: if the user is looking at the app, the in-app
    // toast suffices — never double-alert.
    let focused = this.focused;
    try { focused = await this.deps.isFocused(); } catch { /* keep cached */ }
    if (focused) return;
    this.activeId = opts.id;
    // Permission gates only the banner; attention (dock bounce) needs no grant.
    let granted = false;
    try {
      granted = await this.deps.isPermissionGranted();
      if (!granted) granted = (await this.deps.requestPermission()) === 'granted';
    } catch { granted = false; }
    if (granted) {
      try { this.deps.sendNotification({ title: opts.title, body: opts.body }); } catch { /* ignore */ }
    }
    try { await this.deps.requestUserAttention(true); } catch { /* ignore */ }
  }

  async clear(id: string): Promise<void> {
    if (this.activeId !== id) return;
    this.activeId = null;
    // Load-bearing cancellation: stops the persistent bounce/flash. Programmatic
    // dismissal of an already-posted desktop banner is best-effort/unavailable
    // across platforms, so we only cancel attention here.
    try { await this.deps.requestUserAttention(false); } catch { /* ignore */ }
  }

  dispose(): void {
    // Cancel any in-flight OS attention so a dock/taskbar bounce doesn't persist
    // after teardown (e.g. SPA unmount or identity switch while a call is still
    // ringing). Best-effort; fire-and-forget.
    if (this.activeId) void this.deps.requestUserAttention(false).catch(() => {});
    if (this.unlistenFocus) { this.unlistenFocus(); this.unlistenFocus = null; }
    this.activeId = null;
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
