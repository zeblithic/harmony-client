# ZEB-708: GUI quit-path shutdown flush — plan

**Ticket:** ZEB-708 — GUI quit path runs NO Rust-side shutdown flush; owner-state
durability rides on the last debounced `notify_dirty` flush alone.
**Branch:** `zeb-708-gui-quit-shutdown-flush` off `main@67cf89d8` (post-#485).
**Relationship:** completes the shutdown-durability story ZEB-703/#485 started on
the headless side. Same principle: the save point must precede the user-visible
"it's done" signal (here: the window/process disappearing).

## Verified current state (Phase 1, 2026-07-18)

| Quit path | Today | ExitRequested fired? |
|---|---|---|
| FE Quit → `quit_app` (lib.rs:59018) | `app_handle.exit(0)`, no flush | Yes, `code: Some(0)`, preventable |
| Tray "Quit Harmony" 3s fallback (lib.rs:59876) | raw `exit(0)`, no flush | Yes, preventable |
| GUI-mode `/v1/shutdown` (gui_host.rs:157) | #485 barrier runs pre-ack, then quit-requested + 3s fallback `exit(0)` — `stop_inner` never runs | Yes (via fallback/quit_app), preventable |
| Last-window-closed auto-exit (no-tray ZEB-433 case) | process exits, no flush | Yes, `code: None`, preventable |
| **macOS Cmd+Q / menu Quit** (NEW finding) | muda `PredefinedMenuItem::quit` = `sel!(terminate:)` straight to NSApp; tao 0.35.3 has no `applicationShouldTerminate` hook | **NO — full bypass, process-kill blast radius** |

Toolchain facts (tauri 2.11.2 / tauri-runtime-wry 2.11.2 / tao 0.35.3 /
muda 0.19.2, read from vendored registry sources):

- `AppHandle::exit(code)` → `Message::RequestExit` → `RunEvent::ExitRequested
  { code: Some(code) }`; `api.prevent_exit()` IS honored (runtime-wry lib.rs:4363).
- Prevention is a synchronous `try_recv` immediately after the callback returns —
  `prevent_exit()` must be called inside the handler invocation, never deferred.
- Last-window-destroyed emits `ExitRequested { code: None }`, same contract
  (runtime-wry lib.rs:4323).
- Tauri auto-installs `Menu::default` on macOS (`enable_macos_default_menu`
  defaults `true`; harmony sets no `.menu()`), whose app-submenu Quit item owns
  the Cmd+Q accelerator.
- `stop_inner(&Mutex<NodeState>, None)` is the full teardown fence (ZEB-234 send
  drain → #485 R2 outbox gate + Phase-C fence drain → owner-state
  `SyncEngine::shutdown()` persist → ~12 fleet-engine shutdowns → event-loop
  join). It is sync/blocking by design and already runs from non-main threads in
  GUI mode (`stop_node` IPC precedent).

## Design

### A. Exit-flush gate (state machine) + bounded flush runner

New `pub(crate)` seams in lib.rs (near `quit_app`):

- `GUI_QUIT_FLUSH_MAX_MS: u64 = 5_000` — matches #485's pre-ack barrier bound.
- Phase atomic (`AtomicU8`): `0 = NotStarted`, `1 = InFlight`, `2 = Done`.
- `exit_gate(phase: &AtomicU8) -> ExitGateAction` — pure, unit-testable:
  - CAS 0→1 wins → `StartFlush` (prevent exit, spawn flush thread)
  - observes 1 → `AwaitFlush` (prevent exit; flush thread owns the re-exit —
    this also defuses the tray/gui_host 3s fallback racing a slow FE teardown:
    the fallback's exit lands here instead of cutting the flush short)
  - observes 2 → `Allow` (pass through; the flush thread's own `exit` call)
- `run_bounded_flush<R: tauri::Runtime>(handle: AppHandle<R>, bound: Duration)
  -> bool` — spawns an inner thread running
  `stop_inner(&*handle.state::<Mutex<NodeState>>(), None)`, waits on a channel
  with `recv_timeout(bound)`. `true` = completed within bound; `false` = bound
  expired (WARN; the inner thread keeps running and dies with the process —
  identical blast radius to today's behavior, but only after the bound, and
  `stop_inner` reaches the owner-state persist early in its sequence).

### B. `RunEvent::ExitRequested` wiring

`.run(tauri::generate_context!())` becomes
`.build(tauri::generate_context!()).expect(…).run(callback)`:

```rust
tauri::RunEvent::ExitRequested { code, api, .. } => match exit_gate(&PHASE) {
    StartFlush => {
        api.prevent_exit();
        let code = code.unwrap_or(0);           // preserve restart's code too
        spawn(|| {
            let done = run_bounded_flush(handle, GUI_QUIT_FLUSH_MAX);
            if !done { warn!(…) }
            PHASE.store(Done);
            handle.exit(code);                   // second pass → Allow
        });
    }
    AwaitFlush => api.prevent_exit(),
    Allow => {}
}
```

Covers quit_app, both 3s fallbacks, last-window-closed, and (off-main-thread)
`restart()` — code is preserved so the updater's restart-on-exit flag still
works, now flushed first.

### C. macOS Cmd+Q coverage (menu swap)

In `setup`, `#[cfg(target_os = "macos")]`, best-effort (failures WARN and keep
the default menu — degraded = today's behavior, mirroring the ZEB-433 tray
degrade pattern):

1. `Menu::default(&handle)`; take app submenu = `items()[0]`.
2. Verify its last item is the predefined Quit (defensive: kind check); remove
   it; append `MenuItem::with_id(app, "harmony-quit", "Quit Harmony", true,
   Some("CmdOrCtrl+Q"))`.
3. `app.set_menu(menu)`; `app.on_menu_event`: `"harmony-quit"` → identical body
   to the tray "quit" arm (emit `quit-requested` for FE voice/call teardown +
   arm the 3s fallback exit). Cmd+Q now funnels into path B instead of
   `terminate:`.

macOS-only: setting a menu on Windows/Linux would introduce a menubar that
doesn't exist today; those platforms quit via window close → already covered.

### D. Out of scope / residue

- `restart()` invoked ON the main thread skips events entirely (tao direct
  restart) — cannot be intercepted; nothing in harmony calls it today. Noted.
- SIGTERM/SIGKILL/logout-force: OS-level, unchanged (crash-recovery class).
- FE unchanged: `quit_app` stays `app_handle.exit(0)`; interception is
  Rust-side so every caller (FE, fallbacks) is covered uniformly.

## Tests (TDD, red-first)

1. `exit_gate` transitions: NotStarted→StartFlush (once, under concurrent CAS),
   InFlight→AwaitFlush, Done→Allow.
2. **The durability pin:** NodeState fixture (ZEB-703-style: real
   `crdt_state` + `SyncEngine` with huge debounce), LOCAL mutation WITHOUT
   `notify_dirty`, `run_bounded_flush` → `true` AND the mutation is on disk.
   Mutation-verification after green: short-circuit the `stop_inner` call →
   test must go red.
3. Bound respected under wedge: a helper thread holds the NodeState lock
   (blocks `stop_inner`); `run_bounded_flush` with a 100ms bound returns
   `false` in well under 3s (budget ≪ regression threshold per
   wall-clock-budget rule).
4. RunEvent/menu wiring is not unit-testable (needs a live event loop) — the
   behavior rides on the tested seams; wiring is review + manual-smoke
   verified.
