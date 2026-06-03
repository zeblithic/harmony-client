# ZEB-356 — Incoming-call notifications Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make incoming 1:1 DM voice calls reachable when the Harmony window is unfocused, minimized, or dismissed to a system tray — via an OS notification + window attention, plus a close-to-tray lifecycle that keeps the process (and its Zenoh node) alive.

**Architecture:** Approach A — a small injectable `IncomingCallAlerter` frontend primitive (OS notification + `requestUserAttention`, escalating only when the window is unfocused) wired into the existing `incoming-call` flow in `App.svelte`; plus a Rust system-tray + single-instance + `quit_app` lifecycle that reverses V5's close-to-quit into close-to-hide. The alerter is call-shape-agnostic so ZEB-360 group calls reuse it verbatim.

**Tech Stack:** TypeScript + Svelte 5 (vitest), Tauri v2 (`tauri-plugin-notification`, `tauri-plugin-single-instance`, built-in `tray-icon`), `@tauri-apps/api/webviewWindow`.

**Spec:** `docs/specs/2026-06-02-zeb-356-incoming-call-notifications-design.md` (commit `f5abf77`). Decisions D1–D5 and non-goals are settled — do not reopen them.

**Branch:** `zeb-356-incoming-call-notifications` (already created off `origin/main` `5f94716`).

---

## Conventions for every task

- **Commit BEFORE running the gate.** Stage + commit the task's work first, then run the gate; if the gate finds something, fix + amend. This guarantees no work is lost to a long/hung cargo command.
- **10-minute wall-clock kill switch** on any cargo command. If a single `cargo` invocation exceeds ~10 min, stop it, report `DONE_WITH_CONCERNS` with what ran, and hand back — do not silently wait.
- **DONE_WITH_CONCERNS escape:** if blocked or uncertain after a genuine attempt, report status + the specific concern rather than guessing.
- **Per-task Rust gating (relink cost):** for Rust tasks use `cd src-tauri && cargo clippy --locked -p harmony-app --lib --features test-fixtures -- -D warnings` and `cargo nextest run --locked -p harmony-app --lib --features test-fixtures`. Do **not** use `--all-targets` per task — it relinks ~97 integration binaries (~25 min). `--all-targets` is reserved for the final-sweep task (T8).
- **Frontend gating:** `npx tsc --noEmit` (from repo root) + `npx vitest run` (scope to the file under test during dev, e.g. `npx vitest run src/lib/incoming-call-alert.test.ts`).
- **Format:** Rust tasks end with `cd src-tauri && cargo fmt --all`. Never skip fmt — CI runs `cargo fmt --all -- --check`.
- **Tauri IPC param naming:** Rust declares `snake_case`; JS callers pass `camelCase` (auto-converted). Error extraction: `e instanceof Error ? e.message : String(e)`.

---

## File structure

| File | Responsibility | Task |
|---|---|---|
| `src/lib/incoming-call-alert.ts` (new) | `IncomingCallAlerter` class + `AlerterDeps` + `createIncomingCallAlerter` + `createDefaultIncomingCallAlerter` factory (no-op outside Tauri) | T1, T2 |
| `src/lib/incoming-call-alert.test.ts` (new) | 7 vitest cases | T1, T2 |
| `package.json` | add `@tauri-apps/plugin-notification` | T2 |
| `src/App.svelte` | construct alerter + permission + notify/clear wiring (T3); close-to-tray reversal + quit-requested (T7) | T3, T7 |
| `src-tauri/Cargo.toml` | add notification + single-instance plugins; `tray-icon` feature | T4 |
| `src-tauri/capabilities/default.json` | notification + window permissions | T4 |
| `src-tauri/src/lib.rs` | register plugins; `quit_app` command (T5); tray + menu + single-instance callback (T6) | T5, T6 |
| `docs/plans/...` (this file) | manual smoke checklist | T8 |

---

## Task 1: `IncomingCallAlerter` core class (injectable deps)

**Files:**
- Create: `src/lib/incoming-call-alert.ts`
- Test: `src/lib/incoming-call-alert.test.ts`

Mirrors the `CallSession` pattern (`src/lib/call-session.ts`): an injectable-deps class. All Tauri calls are behind `AlerterDeps`, so the class is testable in pure jsdom with `vi.fn()`s — no Tauri mocking needed.

- [ ] **Step 1: Write the failing tests** (`src/lib/incoming-call-alert.test.ts`)

```ts
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/incoming-call-alert.test.ts`
Expected: FAIL — `Cannot find module './incoming-call-alert'`.

- [ ] **Step 3: Implement `src/lib/incoming-call-alert.ts`** (class + `createIncomingCallAlerter`; the default factory comes in T2)

```ts
// src/lib/incoming-call-alert.ts
//
// IncomingCallAlerter — escalates an incoming call to the OS (notification +
// window attention) when the app window is NOT focused. The in-app ring toast
// already covers the focused case, so escalation is a strict no-op while
// focused. Injectable deps + default factory (T2) + no-op outside Tauri,
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
    if (this.unlistenFocus) { this.unlistenFocus(); this.unlistenFocus = null; }
    this.activeId = null;
  }
}

export function createIncomingCallAlerter(deps: AlerterDeps): IncomingCallAlerter {
  return new Alerter(deps);
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/incoming-call-alert.test.ts`
Expected: PASS (6 tests).

- [ ] **Step 5: Type-check**

Run: `npx tsc --noEmit`
Expected: no errors.

- [ ] **Step 6: Commit**

```bash
git add src/lib/incoming-call-alert.ts src/lib/incoming-call-alert.test.ts
git commit -m "feat(zeb-356): IncomingCallAlerter primitive (notify/clear, unfocused-only escalation)"
```

---

## Task 2: Default factory (`createDefaultIncomingCallAlerter`) + plugin dependency

**Files:**
- Modify: `src/lib/incoming-call-alert.ts` (append factory)
- Modify: `package.json` (add `@tauri-apps/plugin-notification`)
- Test: `src/lib/incoming-call-alert.test.ts` (append non-Tauri no-op case)

The factory wires the real Tauri APIs and degrades to a no-op outside Tauri (web preview / tests), mirroring how `CallSession` is built behind `isTauri()` in `App.svelte`.

- [ ] **Step 1: Add the dependency**

Run:
```bash
npm install @tauri-apps/plugin-notification@^2
```
Expected: `package.json` `dependencies` now includes `@tauri-apps/plugin-notification` (a `^2.x` line); `package-lock.json` updated. (If the environment is offline and `npm install` fails, manually add `"@tauri-apps/plugin-notification": "^2.3.1"` to `dependencies` in `package.json` and note that `npm ci` must run in CI — but prefer the real install so the lockfile is correct.)

- [ ] **Step 2: Write the failing test** (append to `incoming-call-alert.test.ts`)

```ts
import { createDefaultIncomingCallAlerter } from './incoming-call-alert';

describe('createDefaultIncomingCallAlerter (non-Tauri)', () => {
  it('returns a no-op alerter outside Tauri — methods never throw', async () => {
    // jsdom has window but no Tauri internals → isTauri() === false.
    const a = await createDefaultIncomingCallAlerter();
    await expect(a.notify({ id: 'c1', title: 't', body: 'b' })).resolves.toBeUndefined();
    await expect(a.clear('c1')).resolves.toBeUndefined();
    expect(() => a.dispose()).not.toThrow();
  });
});
```

- [ ] **Step 3: Run test to verify it fails**

Run: `npx vitest run src/lib/incoming-call-alert.test.ts`
Expected: FAIL — `createDefaultIncomingCallAlerter` is not exported.

- [ ] **Step 4: Implement the factory** (append to `src/lib/incoming-call-alert.ts`)

```ts
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
      // Activation (notification click) → raise. Best-effort: if the installed
      // plugin build lacks an action callback, omit this — the tray click also
      // raises the window (graceful degradation, per spec D2).
      registerActivation: notif.onAction
        ? async (cb) => { await notif.onAction(() => cb()); }
        : undefined,
    };
    return createIncomingCallAlerter(deps);
  } catch {
    return noopAlerter();
  }
}
```

> NOTE for the implementer: verify `notif.onAction` exists in the installed `@tauri-apps/plugin-notification` build (the v2 plugin exposes notification action/click callbacks; the exact symbol may be `onAction`). If the installed version exports a differently-named activation hook, wire it; if it exposes none, set `registerActivation: undefined` and leave a one-line comment — the tray click still raises the window. Do NOT block on this; activation is explicitly graceful-degradation.

- [ ] **Step 5: Run tests + type-check**

Run: `npx vitest run src/lib/incoming-call-alert.test.ts && npx tsc --noEmit`
Expected: PASS (7 tests); no type errors.

- [ ] **Step 6: Commit**

```bash
git add src/lib/incoming-call-alert.ts src/lib/incoming-call-alert.test.ts package.json package-lock.json
git commit -m "feat(zeb-356): default Tauri-wired alerter factory + plugin-notification dep"
```

---

## Task 3: Wire the alerter into the incoming-call flow (`App.svelte`)

**Files:**
- Modify: `src/App.svelte` — construct alerter in the Tauri-init IIFE; request permission once; `notify` on incoming; `clear` on leave-incoming; `dispose` on unmount.

No `CallSession` changes. This task has no new unit test (App.svelte is not unit-tested wholesale; the alerter logic is covered by T1/T2 and the wiring by the T8 smoke checklist). The gate is `tsc` + existing `vitest`.

- [ ] **Step 1: Add a module-scoped alerter handle** near the call-session state (after `src/App.svelte:133`, the `callStateUnsub` declaration)

```svelte
  // ── ZEB-356: incoming-call OS notification + window attention ──────
  // Built in the Tauri-init IIFE below (real Tauri deps); null in web/dev.
  let incomingCallAlerter: import('./lib/incoming-call-alert').IncomingCallAlerter | null = null;
```

- [ ] **Step 2: Construct the alerter + request permission, inside the Tauri-init IIFE**

In the Tauri-init block, immediately after the DM-call signaling listeners are wired (after `src/App.svelte:1424`, the `unlistenCallEnded` registration) and before the `callStateUnsub` teardown registration at line 1428, insert:

```svelte
      // ── ZEB-356: build the incoming-call alerter (OS notification + attention).
      // Request notification permission once up front so the first incoming call
      // doesn't lose its banner to a permission-prompt race. Dynamic import keeps
      // the web bundle free of the plugin (matches the dynamic-import pattern used
      // for the close handler below).
      try {
        const { createDefaultIncomingCallAlerter } = await import('./lib/incoming-call-alert');
        incomingCallAlerter = await createDefaultIncomingCallAlerter();
        const { isPermissionGranted, requestPermission } = await import('@tauri-apps/plugin-notification');
        if (!(await isPermissionGranted())) { await requestPermission(); }
        fileManagerService.addUnlisten(() => { incomingCallAlerter?.dispose(); incomingCallAlerter = null; });
      } catch (e) {
        console.warn('[harmony-client] incoming-call alerter init failed:', e);
      }
```

- [ ] **Step 3: `notify` on incoming** — extend the `incoming-call` listener (inside the `if (callSession && ... phase === 'incoming')` block at `src/App.svelte:1378-1386`), after `incomingCall = {...}` is set:

```svelte
          // ZEB-356: escalate to the OS if the window is unfocused (no-op if
          // focused — the in-app toast above suffices).
          void incomingCallAlerter?.notify({
            id: p.callId,
            title: 'Incoming call',
            body: `${incomingCall.callerName} is calling`,
          });
```

- [ ] **Step 4: `clear` on leave-incoming** — extend the `callStateUnsub` subscription in `buildVoiceSession` (`src/App.svelte:232-234`):

```svelte
      callStateUnsub = callSession.state.subscribe((s) => {
        if (s.phase !== 'incoming') {
          // ZEB-356: drop the OS escalation when the call leaves 'incoming'
          // (accepted / declined / canceled / timeout). Capture the id before
          // clearing the banner model.
          const id = incomingCall?.callId;
          if (id) void incomingCallAlerter?.clear(id);
          incomingCall = null;
        }
      });
```

- [ ] **Step 5: Type-check + frontend tests**

Run: `npx tsc --noEmit && npx vitest run`
Expected: no type errors; all existing vitest suites still pass.

- [ ] **Step 6: Commit**

```bash
git add src/App.svelte
git commit -m "feat(zeb-356): wire incoming-call alerter (notify on ring, clear on resolve)"
```

---

## Task 4: Rust dependencies + capabilities + config

**Files:**
- Modify: `src-tauri/Cargo.toml` — add `tauri-plugin-notification`, `tauri-plugin-single-instance`; enable `tauri` `tray-icon` feature.
- Modify: `src-tauri/capabilities/default.json` — notification + window permissions.

Config-only task (no behavior yet) — produces a still-compiling tree so T5/T6 build on green.

- [ ] **Step 1: Add the plugin deps + tray-icon feature** (`src-tauri/Cargo.toml`)

Change line 25 from:
```toml
tauri = { version = "2", features = [] }
```
to:
```toml
tauri = { version = "2", features = ["tray-icon"] }
```

Add to the `[dependencies]` block (next to the other `tauri-plugin-*` lines, ~line 56-60):
```toml
tauri-plugin-notification = "2"
tauri-plugin-single-instance = "2"
```

- [ ] **Step 2: Add capabilities** (`src-tauri/capabilities/default.json`) — add these to the `permissions` array (after `"shell:allow-open"`):

```json
    "notification:default",
    "core:window:allow-request-user-attention",
    "core:window:allow-is-focused",
    "core:window:allow-show",
    "core:window:allow-hide",
    "core:window:allow-set-focus",
    "core:window:allow-unminimize"
```

(Keep JSON valid: add a comma after `"shell:allow-open"`.)

- [ ] **Step 3: Verify it still compiles**

Run: `cd src-tauri && cargo check --locked -p harmony-app --lib --features test-fixtures`
Expected: compiles (new deps downloaded). If a plugin version doesn't resolve, pin to the latest `2.x` that matches the `@tauri-apps/plugin-notification` JS version installed in T2.

- [ ] **Step 4: Format + commit**

```bash
cd src-tauri && cargo fmt --all && cd ..
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/capabilities/default.json
git commit -m "build(zeb-356): notification + single-instance plugins, tray-icon feature, capabilities"
```

---

## Task 5: Register plugins + `quit_app` command (`lib.rs`)

**Files:**
- Modify: `src-tauri/src/lib.rs` — register `tauri-plugin-notification` + `tauri-plugin-single-instance`; add `quit_app` command + register it.

- [ ] **Step 1: Add the `quit_app` command** near the other top-level `#[tauri::command]` fns (e.g. just above `pub fn run()` at `src-tauri/src/lib.rs:32782`):

```rust
/// ZEB-356: real application exit. A tray-resident app does NOT quit when its
/// last window is hidden/destroyed (the tray keeps the process alive), so the
/// FE Quit path runs its voice/call teardown and then invokes this to terminate
/// the process. Distinct from the window-close path, which now only hides.
#[tauri::command]
fn quit_app(app_handle: tauri::AppHandle) {
    app_handle.exit(0);
}
```

- [ ] **Step 2: Register single-instance FIRST + notification in the builder** (`src-tauri/src/lib.rs:32783-32789`)

The single-instance plugin must be the **first** `.plugin(...)` call (Tauri requirement). Change the builder head from:
```rust
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
```
to:
```rust
    tauri::Builder::default()
        // ZEB-356: single-instance MUST be registered first. On a second launch
        // its callback shows + focuses the existing window instead of spawning a
        // duplicate (important now that closing only hides to the tray).
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            use tauri::Manager;
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
```

- [ ] **Step 3: Register `quit_app`** in the `invoke_handler` list (`src-tauri/src/lib.rs`, in the `generate_handler![...]` block — add near `start_node`/`stop_node` ~line 32838):

```rust
            quit_app,
```

> **Deep-link interaction (note, not a blocker):** macOS delivers `harmony://` deep links via `on_open_url` (already wired in `setup`), so single-instance does not affect the macOS invite flow. On Windows/Linux a deep link arrives as a *second launch* with the URL in argv, which single-instance now intercepts; our callback ignores `_args` and only raises the window, so second-launch URL **routing** on Win/Linux is a known v1 gap (not a regression — previously it spawned a duplicate). Routing the argv URL into the existing deep-link handler is a follow-up; do not expand scope here. Leave a one-line comment in the callback noting this.

- [ ] **Step 4: Build + lint (lib-scoped)**

```bash
cd src-tauri && cargo clippy --locked -p harmony-app --lib --features test-fixtures -- -D warnings
```
Expected: no warnings/errors. (If `tauri::Manager` is already imported at module scope, the inner `use` is harmless but remove it if clippy flags an unused/duplicate import.)

- [ ] **Step 5: Format + commit**

```bash
cd src-tauri && cargo fmt --all && cd ..
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-356): register notification + single-instance plugins, add quit_app command"
```

---

## Task 6: System tray + menu + Quit emit (`lib.rs` setup hook)

**Files:**
- Modify: `src-tauri/src/lib.rs` — build the tray icon + menu inside the `setup` hook; tray click + Show → raise; Quit → emit `quit-requested`.

- [ ] **Step 1: Build the tray in the setup hook** — inside `.setup(|app| { ... })` (`src-tauri/src/lib.rs:32790-32802`), after the deep-link handler block and before `Ok(())`:

```rust
            // ── ZEB-356: system tray (close-to-tray reachability). ──────────
            // Tray click / "Show Harmony" → raise the window. "Quit Harmony"
            // emits `quit-requested`; the FE runs voice/call teardown then
            // invokes `quit_app`. The window itself never destroys on close
            // (the FE's onCloseRequested only hides), so the last-window-closed
            // auto-exit never fires — quit_app is the sole exit path.
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
            use tauri::{Emitter, Manager};

            fn raise_main(app: &tauri::AppHandle) {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.unminimize();
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }

            let show_i = MenuItem::with_id(app, "show", "Show Harmony", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit Harmony", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&show_i, &quit_i])?;
            let _tray = TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().expect("app has a default icon").clone())
                .tooltip("Harmony")
                .menu(&tray_menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => raise_main(app),
                    "quit" => { let _ = app.emit("quit-requested", ()); }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        raise_main(tray.app_handle());
                    }
                })
                .build(app)?;
```

> NOTE: the `use` statements are inside the `setup` closure to keep the change local. If any of `Emitter` / `Manager` are already imported at module scope, clippy will flag the duplicate — remove the duplicate from the inner `use` in that case. Tauri v2 tray/menu symbol paths: `tauri::tray::TrayIconBuilder`, `tauri::menu::{Menu, MenuItem}`. If `show_menu_on_left_click` is named differently in the pinned Tauri version (older builds: `menu_on_left_click`), use the available setter — the goal is "left-click does NOT open the menu; it raises the window."

- [ ] **Step 2: Build + lint (lib-scoped)**

```bash
cd src-tauri && cargo clippy --locked -p harmony-app --lib --features test-fixtures -- -D warnings
```
Expected: no warnings/errors. Resolve any symbol-path differences against the pinned Tauri 2.x (tray/menu APIs are stable in 2.x; only the left-click-menu setter name has drifted historically).

- [ ] **Step 3: Run lib tests (sanity)**

```bash
cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```
Expected: PASS (no new tests; confirms the lib still builds + links clean).

- [ ] **Step 4: Format + commit**

```bash
cd src-tauri && cargo fmt --all && cd ..
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-356): system tray icon + Show/Quit menu (close-to-tray)"
```

---

## Task 7: Close-to-tray reversal + quit-requested handler (`App.svelte`)

**Files:**
- Modify: `src/App.svelte` — `onCloseRequested` → hide (not destroy); add a `quit-requested` listener that runs V5 teardown then `invoke('quit_app')`.

This reverses V5 (ZEB-353): closing the window no longer tears down + quits — it hides to the tray and keeps the call alive. The V5 teardown moves to the Quit path.

- [ ] **Step 1: Reverse `onCloseRequested`** — replace the close handler body (`src/App.svelte:1443-1470`). The new handler just prevents the default and hides:

```svelte
      // ── ZEB-356: close-to-tray (reverses ZEB-353 close-to-quit). ─────
      // Closing the window hides it to the tray and keeps the process, the
      // Zenoh node, and any ACTIVE CALL alive (hide-during-call = "minimize to
      // keep talking"). The real exit is the tray "Quit Harmony" item, handled
      // by the quit-requested listener below. No teardown here.
      const unlistenClose = await appWin.onCloseRequested(async (event) => {
        event.preventDefault();
        await appWin.hide();
      });
      fileManagerService.addUnlisten(unlistenClose);

      // ── ZEB-356: real quit path. The tray "Quit Harmony" item emits
      // `quit-requested`; run the (bounded, best-effort) V5 voice/call teardown
      // so we don't linger in peers' rosters or hold the mic, then invoke
      // quit_app to terminate the tray-resident process.
      const unlistenQuit = await listen('quit-requested', async () => {
        const teardown = Promise.allSettled([
          voiceSession?.leave() ?? Promise.resolve(),
          callSession?.end() ?? Promise.resolve(),
        ]);
        const timedOut = Symbol('timeout');
        const raced = await Promise.race([
          teardown,
          new Promise((r) => setTimeout(() => r(timedOut), 1500)),
        ]);
        if (raced === timedOut) {
          console.warn('[harmony-client] voice teardown on quit exceeded 1.5s; quitting anyway');
        }
        await invoke('quit_app');
      });
      fileManagerService.addUnlisten(unlistenQuit);
```

> NOTE: `listen` and `invoke` are already in scope in this Tauri-init IIFE (the existing call listeners use `listen`, and `invoke` is the app-wide adapter). Verify the `appWin` binding from `getCurrentWebviewWindow()` (line 1441) is still above this block — it is; only the handler body changes. Keep the `closing` re-entry guard removed (no longer needed: `hide()` is idempotent and cheap).

- [ ] **Step 2: Type-check + frontend tests**

Run: `npx tsc --noEmit && npx vitest run`
Expected: no type errors; existing suites pass.

- [ ] **Step 3: Commit**

```bash
git add src/App.svelte
git commit -m "feat(zeb-356): close-to-tray window close + quit-requested teardown path"
```

---

## Task 8: Final sweep + smoke checklist + push + PR

**Files:**
- Modify: this plan (check the smoke-checklist boxes are present).

- [ ] **Step 1: Full Rust gate (`--all-targets`, the one task that pays the relink)**

```bash
cd src-tauri && cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: fmt clean; clippy 0 warnings; nextest green. **Known non-blocking flakes** (iroh/zenoh loopback): `reachability_publisher::force_notify_triggers_publish`, `zeb_321_connectivity_ipc_tests::force_republish_wakes_publisher`, `zenoh_iroh_link::paired_stream_roundtrip_via_loopback`, two `zenoh_iroh_transport` tests, `community_reachability_two_engine_integration` — if only these fail, re-run them; they pass on CI. A real notification/lifecycle failure must be fixed.
> Respect the 10-min-per-command kill switch: `--all-targets` clippy/nextest can run long on a cold build. If a single command exceeds ~10 min, background it with a wall-clock safety net rather than blocking.

- [ ] **Step 2: Full frontend gate**

```bash
npx tsc --noEmit
npx vitest run
```
Expected: no type errors; all suites pass including `incoming-call-alert.test.ts` (7 cases).

- [ ] **Step 3: MSRV check**

```bash
cd src-tauri && cargo check --locked --all-targets --features test-fixtures
```
Expected: compiles under the declared MSRV (`rust-version = "1.88"`).

- [ ] **Step 4: Manual smoke checklist** (lifecycle/tray is not unit-testable headlessly; record results in the PR body). On a `npm run tauri dev` build:

  1. Close the window (X) → window hides; the app keeps running (tray icon present), not quit.
  2. Tray **Show Harmony** (or tray left-click) → window restores + focuses.
  3. Tray **Quit Harmony** → leaves any voice channel / ends any call, then the process exits.
  4. Re-launch the app while tray-resident → the existing window is focused; no duplicate instance.
  5. With the window unfocused/hidden, have a peer place a DM call → OS notification (with sound) appears + the dock/taskbar attention escalates.
  6. Click the notification → the window raises and the in-app ring toast is visible.
  7. Accept / Decline from the raised toast → connects / declines as before.
  8. Deny notification permission, then receive a call → still get dock/taskbar attention (no banner).
  9. While in an active call, close the window → the call stays connected (audio continues); reopen via tray shows the in-call bar.

- [ ] **Step 5: Push + open the PR**

```bash
git push -u origin zeb-356-incoming-call-notifications
gh pr create --title "ZEB-356: incoming-call notifications (OS notification + window attention + close-to-tray)" --body "<see below>"
```

PR body should reference: spec commit `f5abf77` + path; this plan + path; parent ZEB-348; predecessors ZEB-352 (V4) / ZEB-353 (V5, whose close-to-quit this reverses); that it builds the `IncomingCallAlerter` primitive ZEB-360 will reuse; a summary of changes (new FE alerter + 7 vitest cases; App.svelte notify/clear wiring + close-to-tray reversal; Rust tray + single-instance + `quit_app`; capabilities + plugin deps); and the 9-step smoke-checklist results (which are manual, since tray/lifecycle isn't unit-tested).

---

## Self-review notes (author)

- **Spec coverage:** D1 close-to-tray → T6/T7; D1 unfocused escalation → T1/T3; D2 click-to-raise → T1 (activation) + T2 (raiseWindow) + T6 (tray); D3 OS-default sound → inherent in `sendNotification` (no in-app audio added); D4 calls bypass DND → T1/T3 (alerter never consults `NotificationService`); D5 single-instance → T5. Permission flow → T3. Capabilities/config → T4. Error/edge cases → T1 (focused no-op, permission denied, focus-race auto-clear) + T7 (quit teardown). Testing → T1/T2 (vitest) + T8 (smoke). ZEB-360 reuse → the call-agnostic `notify/clear` surface (T1).
- **Type consistency:** `IncomingCallAlerter` / `AlerterDeps` / `createIncomingCallAlerter` / `createDefaultIncomingCallAlerter` names are identical across T1–T3. `requestUserAttention(critical: boolean)` (true=Critical, false=cancel) is consistent T1↔T2. `notify({id,title,body})` / `clear(id)` identical T1↔T3.
- **No placeholders:** every code step shows full code; commands have expected output; the only deliberately-open items (`notif.onAction` symbol, `show_menu_on_left_click` setter name, plugin version pin) are flagged with explicit fallback instructions because they depend on the installed Tauri 2.x point version — not vague "handle it" placeholders.
