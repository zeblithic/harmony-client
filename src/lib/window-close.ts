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
// — on Win11 new tray icons land in the hidden overflow flyout, so without
// the notice a close looks like a quit. The persisted once-per-install guard
// is only set when a notification was actually dispatched (permission granted
// and send succeeded), so a user who denies notifications today still gets
// the notice if they grant them later; a per-handler session flag bounds
// re-attempts to one per app run.

export interface CloseDeps {
  /** Hide the window (close-to-tray). */
  hide: () => Promise<void>;
  /**
   * Attempt the "still running in the tray" OS notification. Resolves true
   * iff a notification was actually dispatched (drives the persisted guard).
   */
  notifyTrayResident: () => Promise<boolean>;
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
  let attemptedThisSession = false;
  return async (event) => {
    if (!trayActive) {
      // Degraded path: no tray to come back from — let the close proceed so
      // the process exits instead of lingering headless.
      return;
    }
    // Hide FIRST, prevent the close only on success. Tauri reads the prevent
    // flag after this async handler resolves, so the late preventDefault is
    // safe — and a rejected hide() then falls through to a real close instead
    // of trapping the user in a window that neither hides nor closes.
    try {
      await deps.hide();
    } catch {
      return;
    }
    event.preventDefault();

    if (attemptedThisSession) return;
    attemptedThisSession = true;
    let alreadyShown = false;
    try {
      alreadyShown = deps.storage.getItem(TRAY_NOTICE_KEY) !== null;
    } catch {
      // Storage unavailable → the session flag above still bounds attempts
      // to once per app run.
    }
    if (alreadyShown) return;
    // Fire-and-forget: the close path must never await a permission prompt.
    // Persist the guard only when the notice was actually dispatched, so a
    // permission-denied user isn't permanently marked as "shown".
    try {
      void deps
        .notifyTrayResident()
        .then((dispatched) => {
          if (!dispatched) return;
          try {
            deps.storage.setItem(TRAY_NOTICE_KEY, '1');
          } catch {
            // Best-effort; worst case the notice repeats next session.
          }
        })
        .catch(() => {});
    } catch {
      // A synchronously-throwing notifier must never break window close.
    }
  };
}

/**
 * Default `notifyTrayResident` over the notification plugin, with the same
 * permission flow as incoming-call-alert. Resolves true iff the notification
 * was dispatched.
 */
export function makeTrayResidentNotifier(notif: {
  isPermissionGranted: () => Promise<boolean>;
  requestPermission: () => Promise<'granted' | 'denied' | 'default'>;
  sendNotification: (opts: { title: string; body: string }) => void;
}): () => Promise<boolean> {
  return async () => {
    let granted = false;
    try {
      granted = await notif.isPermissionGranted();
      if (!granted) granted = (await notif.requestPermission()) === 'granted';
    } catch {
      return false;
    }
    if (!granted) return false;
    try {
      notif.sendNotification({
        title: 'Harmony is still running',
        body: 'Closing the window keeps Harmony in the system tray. Use the tray icon to reopen or quit.',
      });
      return true;
    } catch {
      return false;
    }
  };
}
