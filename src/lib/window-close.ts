// src/lib/window-close.ts
//
// ZEB-433: conditional close-to-tray. Closing the window hides it to the tray
// ONLY when the tray actually exists (backend `tray_active` IPC); with no tray
// a hidden window is unreachable — no restore or quit affordance — and the
// node lingers headless and invisible (the Ildwyn trap that motivated
// ZEB-433). In that degraded case the handler lets the close proceed, and
// Tauri's last-window-closed default exits the process.
//
// Degraded-close caveat: allowing the close skips the `quit-requested`
// voice/call teardown (same blast radius as a process kill). Acceptable for a
// path that only occurs when tray creation already failed; peers recover via
// presence TTL.
//
// First hide also fires a one-time "Harmony is still running" OS notification
// (per install via localStorage, with a per-handler session backstop when
// storage is unavailable) — on Win11 new tray icons land in the hidden
// overflow flyout, so without the notice a close looks like a quit.

export interface CloseDeps {
  /** Hide the window (close-to-tray). */
  hide: () => Promise<void>;
  /** Fire the "still running in the tray" OS notification (fire-and-forget). */
  notifyTrayResident: () => void;
  /** Persisted once-per-install guard for the notice. */
  storage: Pick<Storage, 'getItem' | 'setItem'>;
}

export const TRAY_NOTICE_KEY = 'harmony:tray-close-notice-shown';

/**
 * Build the `onCloseRequested` handler. `trayActive` is sampled once at init
 * (the tray is created during setup and never changes for the process
 * lifetime), so the handler itself stays synchronous on the decision.
 */
export function makeCloseRequestedHandler(
  trayActive: boolean,
  deps: CloseDeps,
): (event: { preventDefault(): void }) => Promise<void> {
  let notifiedThisSession = false;
  return async (event) => {
    if (!trayActive) {
      // Degraded path: no tray to come back from — let the close proceed so
      // the process exits instead of lingering headless.
      return;
    }
    event.preventDefault();
    await deps.hide();

    if (notifiedThisSession) return;
    notifiedThisSession = true;
    let alreadyShown = false;
    try {
      alreadyShown = deps.storage.getItem(TRAY_NOTICE_KEY) !== null;
      if (!alreadyShown) deps.storage.setItem(TRAY_NOTICE_KEY, '1');
    } catch {
      // Storage unavailable → the session flag above still bounds the notice
      // to once per app run.
    }
    if (!alreadyShown) {
      try {
        deps.notifyTrayResident();
      } catch {
        // Best-effort: a notification failure must never break window close.
      }
    }
  };
}

/**
 * Default `notifyTrayResident` over the notification plugin, with the same
 * permission flow as incoming-call-alert. Fire-and-forget: the close path
 * must never await a permission prompt.
 */
export function makeTrayResidentNotifier(notif: {
  isPermissionGranted: () => Promise<boolean>;
  requestPermission: () => Promise<'granted' | 'denied' | 'default'>;
  sendNotification: (opts: { title: string; body: string }) => void;
}): () => void {
  return () => {
    void (async () => {
      let granted = false;
      try {
        granted = await notif.isPermissionGranted();
        if (!granted) granted = (await notif.requestPermission()) === 'granted';
      } catch {
        granted = false;
      }
      if (!granted) return;
      try {
        notif.sendNotification({
          title: 'Harmony is still running',
          body: 'Closing the window keeps Harmony in the system tray. Use the tray icon to reopen or quit.',
        });
      } catch {
        // Best-effort.
      }
    })();
  };
}
