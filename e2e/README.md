# harmony-client E2E smoke suite

Playwright tests that exercise the full stack by attaching to a live `tauri dev`
WebView2 instance over CDP. This is the only way to cover the Svelte UI +
Tauri IPC + NodeRuntime + Zenoh path in one shot — `vitest` can only reach the
frontend in jsdom, and `cargo test` can't reach the webview at all.

**Windows-only, local-only, human-triggered.** See [ZEB-150][zeb150].

[zeb150]: https://linear.app/zeblith/issue/ZEB-150

## Requirements

- Windows (WebView2 CDP is the only platform where Playwright can attach to
  a Tauri v2 webview)
- `npm install` has been run (installs `@playwright/test`)

## How to run

Two terminals. **Do not combine them** — `tauri dev` doesn't background
cleanly across bash/pwsh/cmd, and trying to chain `npm run tauri dev & test`
in a single command silently loses the CDP port half the time.

### Terminal 1 — the app under test

```pwsh
# PowerShell
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = '--remote-debugging-port=9222'
npm run tauri dev
```

```bash
# bash / git-bash
export WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9222'
npm run tauri dev
```

Wait for the WebView2 window to open and Svelte to mount (the identity
proof-of-work on first launch adds ~5–10s). The app's console should show the
normal startup logs; no special E2E-mode flag is needed.

Quick sanity check that CDP is live:

```bash
curl http://localhost:9222/json/list
```

You should see at least one target whose `url` starts with the Vite origin
(`http://localhost:5173/` by default, or whatever you've set `VITE_ORIGIN` to).

### Terminal 2 — the suite

```bash
npm run test:e2e
```

This runs all specs once against the live webview. Both endpoints are
overridable via environment variables — the fixture keys page discovery off
the Vite origin, so if your dev server isn't on the default port you must
override *both*:

| Variable | Default | What it controls |
|----------|---------|------------------|
| `CDP_ENDPOINT` | `http://localhost:9222` | Chromium-DevTools-Protocol port WebView2 was launched with |
| `VITE_ORIGIN` | `http://localhost:5173` | Origin used to filter CDP targets for the main app page |

## What's covered

| Spec | Guards against |
|------|----------------|
| `boot.spec.ts` | Tauri bridge missing, node never auto-starts ([ZEB-143][]), mail manager uninitialized |
| `navigation.spec.ts` | Network button silently fails ([ZEB-144][]), network-viz privilege escalation |
| `zenoh-connect.spec.ts` | Connect-click crash ([ZEB-149][]), status wiring, post-connect IPC responsiveness |
| `profile-sync.spec.ts` | ProfileEditor save doesn't propagate to parent state ([ZEB-148][]) |

[zeb-143]: https://linear.app/zeblith/issue/ZEB-143
[zeb-144]: https://linear.app/zeblith/issue/ZEB-144
[zeb-148]: https://linear.app/zeblith/issue/ZEB-148
[zeb-149]: https://linear.app/zeblith/issue/ZEB-149

## Gotchas (codified from painful experience)

- **Never call `browser.close()` on a CDP-attached WebView2.** Playwright
  forwards `Browser.close`, which terminates the Tauri process (observed exit
  `0xcfffffff`). The fixture in `fixtures/tauri-bridge.ts` deliberately skips
  teardown close; the node worker exits and the WebSocket dies with it.

- **Don't grab `browser.contexts()[0].pages()[0]` blindly.** The webview briefly
  hosts `about:blank` before Svelte mounts. The fixture polls for a page whose
  main frame is on `http://localhost:5173` — callers never need to handle that
  race themselves.

- **If the Tauri process dies mid-test, restart both terminals.** The CDP
  handle goes stale; a dangling Playwright connection will error with "Target
  page, context or browser has been closed" on every subsequent call.

- **State bleeds between runs.** The suite shares a single live webview across
  specs. `profile-sync` restores its changes, but zenoh-connect may leave the
  session connected. If ordering matters for a future spec, run it first or
  add explicit cleanup.

## Not covered (deferred follow-ups)

- **Vite-stderr `state_referenced_locally` warnings** (the other half of ZEB-148):
  would need the suite to spawn `tauri dev` itself and tee its stderr.
  Currently tracked as a future improvement.
- **CI integration:** requires a Windows runner with headless WebView2. Cost
  and flake risk currently not worth it for a single-developer repo.
- **Visual regression:** out of scope for a smoke suite — needs baseline
  screenshots and flake mitigation.

## Troubleshooting

- *"Timed out waiting for a page on http://localhost:5173"* — Terminal 1 isn't
  running, or `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` wasn't set before
  `tauri dev` launched. Verify `curl http://localhost:9222/json/list` returns
  a target.
- *"Node did not report a hex identity within 20000ms"* — the backend is slow
  or wedged. Give the first boot a full minute on cold clone, then rerun.
  Subsequent runs are faster (identity is persisted).
- *`test:e2e` hangs indefinitely* — the CDP handle is stale from a prior crash.
  Close the Tauri window, kill `tauri dev`, restart both terminals.
