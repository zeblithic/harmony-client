# ZEB-356 — Incoming-call notifications (OS notification + window attention + close-to-tray)

**Status:** design approved 2026-06-02
**Epic:** ZEB-348 (Voice comms) · **Predecessors:** ZEB-352 (V4 1:1 DM calls), ZEB-353 (V5 leave-on-close)
**Reused by:** ZEB-360 (group-DM calls) — this builds the incoming-call alert primitive that group calls consume verbatim
**Branch:** `zeb-356-incoming-call-notifications` off `origin/main` `5f94716`

---

## Goal

Make incoming 1:1 DM voice calls reachable when the app window is **not focused, minimized, or dismissed to a system tray**. Today the incoming-call ring is a purely in-app visual toast (`IncomingCallToast.svelte`) driven by live Zenoh signaling, and V5 (ZEB-353) made closing the window quit the app — so there is currently **no path to receive a call unless the app is already running and focused**, and a caller can ring someone who will never see it. This is the single biggest UX gap left in the voice epic.

## Scope decisions (brainstormed + approved 2026-06-02)

- **D1 — Reachability tier: "unfocused + close-to-tray".** v1 delivers OS notification + dock/taskbar attention whenever the window is unfocused/minimized, **and** reverses V5's close-to-quit into **close-to-tray**: closing the window hides it and keeps the process + Zenoh node (and any active call) alive, so the user stays reachable while the app sits in the tray. Waking a fully-quit app (process not running) via OS autostart / push infra is explicitly **out of scope** (a Harmony-wide architectural commitment, not a voice feature).
- **D2 — Notification action: click-to-raise.** Clicking the OS notification restores + focuses the window; the existing in-app ring toast (Accept/Decline) is right there. One reliable code path across macOS/Windows/Linux. Inline Accept/Decline action-buttons in the notification are deferred (uneven cross-platform support).
- **D3 — Sound: OS notification default sound is the v1 audible signal.** There is **no call ring audio today** (the toast is silent; "ring" in code refers only to the signaling states). The OS notification carries its default sound, so an unfocused/tray user hears the call — no double-ring risk. A full in-app ringtone for the focused case (looping audio, per-peer CAS sound CIDs via `NotificationService` overrides) is out of scope for v1.
- **D4 — DND: calls bypass message-DND.** Incoming calls always escalate when unfocused, independent of the message `NotificationService` DND/priority model — a person-initiated, time-bounded call is not a message. A global "incoming-call notifications" toggle and per-peer call-mute are deferred.
- **D5 — Single-instance included.** Re-launching a tray-resident app focuses the existing instance rather than spawning a second (`tauri-plugin-single-instance`); also tightens the existing deep-link invite flow.

## Non-goals (v1)

- Waking a fully-quit (process-not-running) app on an incoming call — needs autostart / background agent / push infra.
- Inline Accept/Decline buttons inside the OS notification.
- An in-app ringtone / custom per-peer ring sounds (focused-window audio).
- Lock-screen caller-name privacy redaction toggle.
- A user-facing settings toggle for call notifications / per-peer call-mute.
- Any change to `call-session.ts` signaling, the media path, or `NotificationService` (message policy engine).

---

## Architecture

Approach A (FE notification module + Rust tray). Two cleanly separated units:

1. **`IncomingCallAlerter`** (frontend, new `src/lib/incoming-call-alert.ts`) — owns window-focus tracking and OS escalation (notification + window attention) for an incoming call. Call-shape-agnostic: `notify({id,title,body})` / `clear(id)`. This is the primitive ZEB-360 reuses.
2. **App lifecycle & system tray** (Rust `lib.rs` setup + a small `App.svelte` change) — close-to-tray, tray icon + menu, Quit-does-teardown, single-instance.

The existing call orchestration in `App.svelte` (the `incoming-call` event listener that sets the `incomingCall` model and renders the toast) gains two lines: `notify(...)` on entering `incoming`, `clear(id)` on the model clearing. No changes to `CallSession`.

### Why this split

`IncomingCallAlerter` is a focused, injectable unit testable in isolation with the established vitest mock-injection pattern. Lifecycle/tray is necessarily Rust (window + event loop) and is the one piece that can't be unit-tested headlessly; it gets a manual smoke checklist instead. Keeping escalation logic out of the already-large `App.svelte` and out of `call-session.ts` preserves clear boundaries and gives ZEB-360 a clean handoff.

---

## Component 1 — `IncomingCallAlerter` (frontend)

**File:** `src/lib/incoming-call-alert.ts` · **Test:** `src/lib/incoming-call-alert.test.ts`

### Interface

```ts
export interface IncomingCallAlerter {
  /** Escalate an incoming call to the OS if the window is not focused. No-op if focused. */
  notify(opts: { id: string; title: string; body: string }): Promise<void>;
  /** Cancel attention + dismiss the OS notification for this id. */
  clear(id: string): Promise<void>;
  /** Tear down focus listener (called on app teardown). */
  dispose(): void;
}

export interface AlerterDeps {
  isPermissionGranted: () => Promise<boolean>;
  requestPermission: () => Promise<'granted' | 'denied' | 'default'>;
  sendNotification: (opts: { title: string; body: string }) => void;
  isFocused: () => Promise<boolean>;
  onFocusChanged: (cb: (focused: boolean) => void) => Promise<() => void>;
  requestUserAttention: (critical: boolean) => Promise<void>; // false => cancel
  raiseWindow: () => Promise<void>; // unminimize + show + setFocus
  registerActivation?: (cb: () => void) => Promise<void>; // notification click
}
```

A default factory `createIncomingCallAlerter()` wires the real deps from `@tauri-apps/plugin-notification` and `@tauri-apps/api/webviewWindow`. Outside Tauri (web preview / tests without injection), the factory returns a **no-op alerter** (guards on `window.__TAURI__` / failed plugin import) so nothing throws.

### Behavior

- **Construction:** read `isFocused()` into an internal `focused` flag and subscribe via `onFocusChanged`. When focus is regained, automatically `clear()` the active alert (focusing the app drops the OS escalation; the in-app toast remains).
- **`notify({id,title,body})`:** re-check `focused`; if focused → **no-op**. Else: ensure permission (`isPermissionGranted` → else `requestPermission`); if granted → `sendNotification({title,body})`; always → `requestUserAttention(true)` (Critical). Record `activeId = id`.
  - Permission denied → skip `sendNotification` but still `requestUserAttention(true)` (dock bounce needs no notification permission). Log once.
- **`clear(id)`:** if `id === activeId` → `requestUserAttention(false)` (cancel) + best-effort dismiss the notification; reset `activeId = null`. The load-bearing cancellation is `requestUserAttention(false)` (stops the persistent bounce/flash). Programmatic dismissal of an already-posted desktop notification banner is not reliably available across platforms in the Tauri v2 notification plugin; treat it as best-effort (the banner auto-hides on its own; a lingering notification-center entry is acceptable).
- **Activation:** `registerActivation(() => raiseWindow())` so clicking the notification raises the window. If a platform doesn't deliver activation, the tray click still restores it.

### Permission timing

Request notification permission **once proactively at app startup**, after owner-identity is loaded / first-run completes (so the first incoming call doesn't lose its notification to a permission-prompt race). Lazy re-check inside `notify` is the fallback.

---

## Component 2 — App lifecycle & system tray

### Close → hide-to-tray (reverses V5)

`App.svelte`'s `onCloseRequested` currently runs V5 teardown (`voiceSession.leave()` + `callSession.end()`) then `destroy()`. New: `onCloseRequested` → `preventDefault()` → `appWindow.hide()`. **No teardown on close** — the process, Zenoh node, and any active call stay alive (hiding during a call = "minimize to keep talking").

### Tray icon + menu (Rust, `lib.rs` setup via `TrayIconBuilder`)

- Reuses the existing app icon (Tauri v2 built-in `tray-icon` feature — no extra plugin).
- Tray click → show + unminimize + focus the window.
- Menu: **"Show Harmony"** (show+focus) and **"Quit Harmony"**.

### Quit does V5's teardown

"Quit Harmony" is the real exit and must not leave a ghost participant. The tray Quit handler emits a `quit-requested` event → the FE runs the **existing V5 teardown** (`voiceSession.leave()` + `callSession.end()`, same ≤1.5s-bounded logic) → then invokes a new `quit_app` command that calls `app_handle.exit(0)`. A dedicated exit command is required because a tray-resident app does **not** quit when its last window is destroyed (the tray keeps the process alive), so `destroy()` alone — V5's old exit — no longer terminates the process. V5's teardown logic is **moved** from `onCloseRequested` to the quit path, not deleted.

### Single-instance (`tauri-plugin-single-instance`)

Rust-only. On a second launch, its callback shows + focuses the existing window instead of spawning a duplicate; routes any launch-time deep-link URL to the running instance.

---

## Data flow

```
Caller places call
  → Zenoh Invite → event_loop.rs emits `incoming-call` { callId, callerOwner, spaceId }
  → App.svelte `incoming-call` listener:
       callSession.onIncoming(...)            (phase → 'incoming', arms 30s ring timeout)
       incomingCall = { callId, spaceId, callerName, callerAvatarUrl }
       IncomingCallToast renders               (visual, as today)
       alerter.notify({ id: callId, title: 'Incoming call',
                        body: `${callerName} is calling` })
            ├─ window focused?  → no-op (toast suffices)
            └─ window unfocused/minimized/tray-hidden?
                 → sendNotification (default sound)  + requestUserAttention(Critical)

User reacts:
  click notification → raiseWindow() → toast visible → Accept/Decline (existing wiring)
  Accept/Decline/caller-cancel/30s-timeout → phase leaves 'incoming'
       → incomingCall = null → alerter.clear(callId)  (cancel attention + dismiss)
  window regains focus while ringing → alerter auto-clears OS escalation (toast stays)
```

Caller display name comes from the same `resolveCard(callerOwnerHex)` the toast already uses; fall back to `Someone is calling` if unresolved.

---

## Notification content, sound, attention, DND

- **Content:** title `Incoming call`, body `<Display Name> is calling` (fallback `Someone is calling`). No message content. v1 shows the caller name.
- **Sound:** OS notification default sound (the v1 audible signal); nothing else plays, so no double-ring.
- **Attention:** `UserAttentionType.Critical` — persistent dock-bounce (macOS) / taskbar-flash (Windows) / WM-dependent (Linux) until focused; canceled on resolve/focus.
- **DND:** calls always escalate when unfocused, bypassing the message `NotificationService` model.

---

## Config & capabilities

**`package.json`:** add `@tauri-apps/plugin-notification`.
**`src-tauri/Cargo.toml`:** add `tauri-plugin-notification`, `tauri-plugin-single-instance`; enable the `tauri` crate `tray-icon` feature.
**`src-tauri/src/lib.rs`:** register both plugins; build the tray + menu in the `setup` hook; the tray Quit handler emits `quit-requested`; add a `quit_app` command (calls `app_handle.exit(0)`) that the FE invokes after teardown.
**`src-tauri/capabilities/default.json`:** add `notification:default` and window perms `core:window:allow-request-user-attention`, `allow-is-focused`, `allow-show`, `allow-hide`, `allow-set-focus`, `allow-unminimize`.
**`src-tauri/tauri.conf.json`:** window config unchanged (label `main`); tray is built in Rust. Ensure the tray icon asset is bundled.

---

## Cross-platform notes (target all three; degrade gracefully — never a hard failure)

- **macOS:** tray = menu-bar extra; dock-bounce via Critical; default notification sound. *Smoke caveat:* notification + sound delivery is most reliable in a signed/bundled build; dev runs may attribute them to the Tauri identity.
- **Windows:** tray icon + taskbar flash; toast notifications use the bundle's AppUserModelID (packaged build provides it).
- **Linux:** tray depends on the DE (StatusNotifier/libappindicator) — if absent, the app still runs (no visible tray icon); notifications via libnotify; `requestUserAttention` is WM-dependent. Single-instance still re-focuses.

---

## Error handling & edge cases

1. **Permission denied** → skip `sendNotification`, still `requestUserAttention` (visual escalation survives). Log once.
2. **Non-Tauri / test / web preview** → default factory no-ops if `__TAURI__` / plugin import is absent; tests inject mocks.
3. **Focus race** → `notify` re-checks `focused` at call time (no-op if focused); focusing mid-ring auto-clears.
4. **Call resolves before user reacts** (caller-cancel / 30s timeout) → `clear(id)` dismisses the stale notification + cancels attention.
5. **Quit during a ring** → Quit teardown declines/ends the call and clears the alert.
6. **Rapid/again invites** → `CallSession` already enforces one session (second invite auto-declines while busy) → at most one alert.
7. **Activation when already visible** → `raiseWindow` (show/setFocus/unminimize) is idempotent.
8. **Hidden-webview reliability** → the `incoming-call` listener + `notify` run in the webview, which keeps executing event handlers while hidden-to-tray (confirmed by the tray-hidden smoke test).

---

## Testing

### Frontend unit (vitest) — `src/lib/incoming-call-alert.test.ts`

- unfocused → `sendNotification` + `requestUserAttention(true)` with correct title/body.
- focused → strict **no-op** (no notification, no attention). **Regression guard against double-alerting.**
- `clear` → `requestUserAttention(false)` + notification dismissed.
- permission denied → `sendNotification` skipped, `requestUserAttention(true)` still fired.
- focus regained mid-ring → auto-`clear`.
- non-Tauri / missing plugin → every method no-ops without throwing.
- activation → `raiseWindow` invoked.

`IncomingCallToast.test.ts` unchanged (toast behavior unchanged).

### Rust / lifecycle — manual smoke checklist (in plan)

Tray/single-instance/close-to-tray are window+event-loop behaviors not unit-testable headlessly; no brittle UI tests. Smoke checklist:

1. Close window → hides to tray, app still running (not quit).
2. Tray **Show** → restores + focuses.
3. Tray **Quit** → runs V5 teardown (leaves voice / ends call) then exits.
4. Re-launch while tray-resident → focuses existing instance (no duplicate).
5. Incoming call while tray-hidden → OS notification (with sound) + dock/taskbar attention.
6. Click notification → window raises, toast visible.
7. Accept / Decline from the raised toast → works (existing wiring).
8. Deny notification permission → still get dock/taskbar attention on incoming call.
9. Active call + close window → call stays alive (audio continues).

### Gates (CI)

`cargo fmt --check` + `cargo clippy --all-targets --features test-fixtures -D warnings` + `cargo nextest --all-targets --features test-fixtures` + `npx tsc --noEmit` + `npx vitest run` + MSRV `cargo check`. Frontend-heavy change; minimal Rust relink.

---

## ZEB-360 reuse surface

`IncomingCallAlerter` (`notify({id,title,body})` / `clear(id)`) is call-shape-agnostic. Group-DM calls (ZEB-360) construct a group-framed title/body and call the same two methods — **zero changes** to the primitive, capabilities, tray, or lifecycle. Focus-predicate, permission flow, attention, and activation all transfer unchanged.

---

## Open risks

- **Hidden-webview event delivery.** The design assumes a hidden Tauri window keeps executing `listen()` callbacks. True for desktop Tauri (a hidden window's webview is not suspended), but timer throttling can occur — `notify` is event-driven (not timer-driven), so this is fine; smoke test #5 confirms.
- **Production notification delivery** needs a properly signed/bundled build on macOS/Windows; dev-mode delivery may differ. Documented as a smoke caveat, not a code risk.
- **Linux tray variance** across desktop environments; mitigated by graceful degradation (app still functions without a visible tray icon) and single-instance re-focus.
