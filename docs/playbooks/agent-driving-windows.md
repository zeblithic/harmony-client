# Agent-driving a Windows harmony-client node (CDP + Playwright MCP)

> Goal: let a Claude Code (or other agent) session **cold-start the desktop app,
> drive it headlessly over Chrome DevTools Protocol, exchange messages with
> another node, and clean up its own state — no human in the loop.**
>
> This is the per-node bring-up recipe for the agent-driven Windows fleet
> (Ildwyn, AVALON, KRILE, …). It is the agent-facing counterpart to the
> human/in-app [`cross-wan-validation.md`](../cross-wan-validation.md). For the
> one-time machine install (toolchain, signing, first run) see
> [`install-windows.md`](../install-windows.md); for test/lint gates see
> [`../../CLAUDE.md`](../../CLAUDE.md).

This playbook is meant to be a **copy-paste exercise** for standing up the next
Windows node. Where a value is machine-specific (paths, identity IDs), it is
called out; everything else is identical across nodes.

---

## 0. Mental model: why driving the *real* app is non-trivial on Windows

harmony-client is a Tauri 2 app: a Rust backend (`src-tauri/`) hosting a WebView2
(Edge/Chromium) frontend. An agent drives it the same way it would drive a
browser — over the **Chrome DevTools Protocol (CDP)** — but three things make
Windows different from a normal Playwright target:

1. **Two secrets, two backends.** The Reticulum identity seed and the
   consolidated key vault are stored separately, and the vault is keychain-only.
   Get the launch env wrong and the app boots *half-dead* (no transport, owner
   never mints). See §2.
2. **Closing the window does not quit the app** ([ZEB-433](https://linear.app/zeblith/issue/ZEB-433)).
   A real quit means killing the PID. See §6.
3. **`Browser.close` over CDP kills the app.** Never send it. Drop the socket
   instead. See §4.

Internalize §2 and §6 before your first launch — they are the two ways to waste
an hour.

---

## 1. Prerequisites

- Machine installed per [`install-windows.md`](../install-windows.md): Rust
  stable (msvc), **Node ≥22**, the app builds (`npm ci` at repo root, then a
  `tauri dev` build succeeds). Node 22 is required because the raw-CDP driver in
  §4 uses the **built-in `WebSocket`** (stable since Node 22); on Node 20 the
  driver fails before any IPC runs.
- `cargo-nextest` installed (`cargo install cargo-nextest --locked`) if you'll
  run gates.
- PowerShell 5.1 is the default shell. Know its hazards: it mangles non-ASCII in
  some pipelines (commit via `git commit -F <file>`, never inline `-m` with
  emoji), and `"$env:VAR:suffix"` mis-parses — use the subexpression form
  `"$($env:VAR):suffix"`.
- Set `RUST_MIN_STACK=8388608` in any shell that builds/launches the app —
  without it you can hit a `0xc00000fd` stack-overflow crash on Windows.

---

## 2. Secret storage and launch env (read before first launch)

harmony-client keeps its secrets across the OS keychain (the ZEB-363
consolidated `harmony`/`identity` vault item) and encrypted files. **The
fallback story differs per secret** — and the difference is exactly what bites
agents:

| Secret | Backend | File fallback? |
|---|---|---|
| Reticulum identity seed (`identity.enc`) | keychain preferred → encrypted file | **Yes** — `HARMONY_PASSPHRASE` / `HARMONY_PASSPHRASE_FILE` (`load_or_generate_with_stores`) |
| Owner master seed + device secret | vault slot → encrypted file | **Yes** — same passphrase env (`owner_state.rs::load_secret`/`save_secret` → `EncryptedFileStore::from_env`) |
| **iroh secret key** (transport) + other app-local vault keys | vault slot → *legacy keychain item* | **No** — `vault_app_key_or_create` does `KeychainStore::new()?`; on failure the error propagates and there is **no encrypted-file path** (see [ZEB-449](https://linear.app/zeblith/issue/ZEB-449)) |

The asymmetry in the last row is the whole game: the owner/identity material can
live in a file, but the **iroh transport key cannot** — it needs a working
keychain.

Consequences that bite agents:

- **`HARMONY_DISABLE_KEYCHAIN=1` must NEVER be set on a real launch.** It is a
  *test-only* kill-switch (ZEB-428) for test-spawned production children. On a
  real launch `KeychainStore::new()` returns `Err`, so the iroh key (no file
  fallback) can't load → **transport disabled this session**. The owner *seed*
  itself can still persist to its encrypted-file fallback if a passphrase is set,
  but with transport down the node is non-functional, so don't set it
  ([ZEB-450](https://linear.app/zeblith/issue/ZEB-450)).
- **An agent-spawned process may not reach Credential Manager** in some
  contexts. If it can't, the **iroh/app-key path** (no file fallback) fails and
  takes transport down with it, even though the seed-and-owner secrets would
  survive on a passphrase. Whether CredMan is reachable from an agent-launched
  process is **per-machine** — verify it with a `cmdkey /list` probe from an
  agent-spawned shell before relying on it. (On AVALON it is reachable; on
  Ildwyn it is not.)

### Passphrase file (one-time per machine)

The identity seed needs a passphrase to use its file backend. Generate one and
lock it down — **never print its contents to the agent transcript:**

```powershell
$dir = 'C:\zeblith\secrets'
New-Item -ItemType Directory -Force $dir | Out-Null
# 32 cryptographically-secure random bytes, base64 — written, never displayed.
$bytes = New-Object byte[] 32
[System.Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
[Convert]::ToBase64String($bytes) |
  Out-File "$dir\harmony-passphrase.txt" -NoNewline -Encoding ascii
# Lock to the current user, read-only, no inheritance
icacls "$dir\harmony-passphrase.txt" /inheritance:r /grant:r "$($env:USERNAME):(R)"
```

> Use the .NET CSPRNG (`RandomNumberGenerator`) shown above — **not**
> `Get-Random`, which is not cryptographically strong and would weaken the key
> protecting `identity.enc`. This matches the crypto-grade `openssl rand -base64`
> guidance in [`headless-install.md`](../headless-install.md). Where `openssl` is
> on PATH, `openssl rand -base64 48 > <file>` is an equivalent one-liner.

### Two launch flavors

Pick based on whether you want the machine's real identity or a disposable one.

**A. Throwaway identity** (ephemeral testing — never touches the real identity):

```powershell
$env:HOME = (New-Item -ItemType Directory -Force "$env:TEMP\harmony-throwaway-$(Get-Random)").FullName
$env:HARMONY_PASSPHRASE_FILE = 'C:\zeblith\secrets\harmony-passphrase.txt'
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = '--remote-debugging-port=9222'
$env:RUST_MIN_STACK = '8388608'
# from the repo root (Vite watches cwd):
npm run tauri dev
```

`identity::resolve_path` reads `HOME` before `USERPROFILE`, so redirecting `HOME`
isolates a throwaway identity store; cargo/npm keep using `USERPROFILE`, so the
build is unaffected.

**B. Real machine identity** (the node's actual peer identity):

```powershell
# NO $env:HOME redirect.
$env:HARMONY_PASSPHRASE_FILE = 'C:\zeblith\secrets\harmony-passphrase.txt'
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = '--remote-debugging-port=9222'
$env:RUST_MIN_STACK = '8388608'
# NEVER set HARMONY_DISABLE_KEYCHAIN here.
npm run tauri dev
```

On a healthy real launch the owner identity **auto-mints at first boot** — no UI
action needed. If you see `Owner identity not loaded` plus
`transport disabled this session`, you almost certainly set
`HARMONY_DISABLE_KEYCHAIN` or the vault couldn't reach the keychain (§2).

> **Identity isolation rule:** mint a fresh identity on each machine. **Never
> copy an `identity.enc` or a keychain vault item between machines** — two nodes
> sharing one identity will fight over discovery and device trust.

---

## 3. Attach after boot completes

Build at least once in the normal env first, so compile errors surface in the
terminal rather than as a silent failure to attach.

Boot is complete when a CDP **page** target exists whose title is not
`about:blank`. Poll for it:

```powershell
# Quick check that the debug port is live and the page has booted:
(Invoke-RestMethod http://localhost:9222/json/list) |
  Where-Object { $_.type -eq 'page' } |
  Select-Object title, id
```

WebView2's CDP endpoint is at `http://localhost:9222`. pkarr relays take ~1–2
minutes to warm up (and flap after the machine sleeps), so gate readiness on an
actual resolve (§5), not on relay health.

---

## 4. The raw-CDP driver (`cdp.mjs`)

Playwright's `connectOverCDP` can crash on WebView2's `shared_worker` blob
target. A **raw-CDP driver** over the Node ≥22 built-in `WebSocket` is more
robust. Drop this into `.playwright-scratch/cdp.mjs` (that dir is **untracked**
— never commit it):

```javascript
// Raw-CDP driver for harmony-client. Usage: node cdp.mjs '<js expression>' [bootWaitSeconds]
//   The expression is evaluated in the app page with awaitPromise+returnByValue.
//   Tauri IPC example: node cdp.mjs "window.__TAURI_INTERNALS__.invoke('list_owner_communities', {})"
// IMPORTANT: never sends Browser.close — drops the socket when done (closing would kill the app).
const PORT = 9222;
const expr = process.argv[2];
const waitSec = Number(process.argv[3] ?? 120);
if (!expr) {
  console.error('usage: node cdp.mjs "<js expression>" [bootWaitSeconds]');
  process.exit(2);
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function findPageTarget() {
  const deadline = Date.now() + waitSec * 1000;
  while (Date.now() < deadline) {
    try {
      const res = await fetch(`http://localhost:${PORT}/json/list`);
      if (!res.ok) throw new Error(`/json/list HTTP ${res.status}`);
      const targets = await res.json();
      // Boot is complete when a real page target exists with a non-blank title.
      const page = targets.find(
        (t) => t.type === 'page' && t.title && t.title !== 'about:blank'
      );
      if (page) return page;
    } catch {
      // app not up yet
    }
    await sleep(2000);
  }
  throw new Error(`no booted page target on :${PORT} after ${waitSec}s`);
}

const page = await findPageTarget();
console.error(`[cdp] target: "${page.title}" ${page.id}`);

const ws = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
  ws.onopen = resolve;
  ws.onerror = (e) => reject(new Error(`ws error: ${e.message ?? e}`));
});

let nextId = 1;
const pending = new Map();
// Reject every in-flight request if the socket drops or errors — otherwise a
// closed connection leaves `await send(...)` hanging forever.
function rejectAllPending(reason) {
  for (const { reject, timer } of pending.values()) {
    clearTimeout(timer);
    reject(new Error(reason));
  }
  pending.clear();
}
ws.onclose = () => rejectAllPending('ws closed before reply');
ws.onerror = (e) => rejectAllPending(`ws error: ${e.message ?? e}`);
ws.onmessage = (ev) => {
  const msg = JSON.parse(ev.data);
  if (msg.id && pending.has(msg.id)) {
    const { resolve, timer } = pending.get(msg.id);
    clearTimeout(timer);
    pending.delete(msg.id);
    resolve(msg);
  }
};
function send(method, params, timeoutMs = 30000) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`CDP ${method} timed out after ${timeoutMs}ms`));
    }, timeoutMs);
    pending.set(id, { resolve, reject, timer });
    ws.send(JSON.stringify({ id, method, params }));
  });
}

let reply;
try {
  reply = await send('Runtime.evaluate', {
    expression: expr,
    awaitPromise: true,
    returnByValue: true,
  });
} catch (e) {
  // Timeout, or socket dropped before a reply arrived.
  console.error('[cdp] SEND FAILED:', e.message);
  console.log(JSON.stringify({ ok: false, error: e.message }));
  try { ws.close(); } catch {}
  process.exit(1);
}

// Protocol-level failure (bad params, target gone): `error`, no `result`.
if (reply.error) {
  console.error('[cdp] PROTOCOL ERROR:', reply.error.message ?? reply.error);
  console.log(JSON.stringify({ ok: false, error: reply.error.message ?? reply.error }));
  ws.close(); // closes OUR socket only — not Browser.close
  process.exit(1);
}

if (reply.result?.exceptionDetails) {
  const ex = reply.result.exceptionDetails;
  console.error('[cdp] EXCEPTION:', ex.exception?.description ?? ex.text);
  console.log(JSON.stringify({ ok: false, exception: ex.exception?.description ?? ex.text }));
  ws.close(); // closes OUR socket only — not Browser.close
  process.exit(1);
}

console.log(JSON.stringify({ ok: true, value: reply.result?.result?.value }, null, 2));
ws.close(); // closes OUR socket only — not Browser.close
process.exit(0);
```

What this driver bakes in:

- **Never `Browser.close` / `browser.close()`.** It kills the app. The driver
  only ever closes its own WebSocket.
- **It fails loudly, never silently.** Three distinct failure shapes each print
  `{ ok: false, ... }` and exit 1: a CDP *protocol* error (`reply.error` — bad
  params, target gone), a JS *exception* in the page (`exceptionDetails`), and a
  *transport* failure (per-request 30 s timeout, or the socket dropping with
  requests in flight — every pending `send` is rejected on `onclose`/`onerror`).
  Without these, a failed call can otherwise print `{ ok: true, value: undefined }`
  or hang forever — an agent would read that as success.
- **Wrap invokes that can reject** so the reason survives `returnByValue`. A
  rejected promise's *string* reason does not survive `Runtime.evaluate` with
  `returnByValue: true` — you get a bare `Uncaught (in promise)`. Pattern:

  ```javascript
  node .playwright-scratch\cdp.mjs "window.__TAURI_INTERNALS__.invoke('add_friend_by_key', {identityPubHex: KEY}).then(v => ({ok:true, v}), e => ({ok:false, err: e instanceof Error ? e.message : String(e)}))"
  ```

### Playwright MCP (alternative)

If the Playwright MCP server is attached to the session, it can connect to the
same `:9222` endpoint for snapshot/click/screenshot-style interaction. The raw
driver above is preferred for pure IPC calls (it's deterministic and won't trip
on the shared-worker target); reach for Playwright MCP when you need DOM
inspection or screenshots.

---

## 5. IPC cheat sheet & self-resolve diagnostic

Tauri commands take **camelCase** params from JS (the snake_case rename was
purged in ZEB-414). `start_node` auto-fires at app boot.

| Call | Notes |
|---|---|
| `connectivity_get_my_identity_pub_hex {}` | returns your 128-hex transport key (`null` if node not started) |
| `get_owner_state {}` | returns `ownerId` + devices |
| `connectivity_set_identity_discoverable {enabled: true}` | Settings → Network → "Allow discovery by identity address" |
| `add_friend_by_key {identityPubHex}` | friend-by-key (see choreography below) |
| `connectivity_redeem_invite_iroh {inviteUrl}` | redeem an invite (NOT legacy `redeem_invite`) |
| `list_owner_communities {}` | rows key on `spaceId` |
| `post_channel_message {communityId, channelId, body}` | `body` is a **byte array**: `[...new TextEncoder().encode(text)]` |
| `publish_profile {profile:{...}}` | publish profile |

**Self-resolve diagnostic** — the fastest proof the transport stack works.
Adding your *own* key should fail with `Connecting to ourself is not supported`.
That error is the **success signal**: it proves pkarr publish + resolve + iroh
dial all completed.

```powershell
$me = (node .playwright-scratch\cdp.mjs "window.__TAURI_INTERNALS__.invoke('connectivity_get_my_identity_pub_hex', {})" | ConvertFrom-Json).value
node .playwright-scratch\cdp.mjs "window.__TAURI_INTERNALS__.invoke('add_friend_by_key', {identityPubHex: '$me'}).then(v => ({ok:true, v}), e => ({ok:false, err: e instanceof Error ? e.message : String(e)}))"
# expect: { ok:false, err: "...Connecting to ourself is not supported" }
```

If `add_friend_by_key` throws `no relays available`, the relays haven't warmed
up. Recovery: `stop_node {}` then `start_node {endpoint: null}`, then wait and
re-check the self-resolve.

**Friend-by-key is a 3-beat choreography by design:** B adds A → pending;
A accepts (stores a pre-approval only); **B must re-add** to complete the link.
Both sides need discoverability ON.

---

## 6. Windows hazards (the things that waste an hour)

- **Closing the window does NOT quit the app** ([ZEB-433](https://linear.app/zeblith/issue/ZEB-433)).
  It keeps running headless on the same PID, holding ports 9222/5173. A "restart"
  reattaches to the same PID. To **really** quit, kill the process and verify it
  and the ports are gone:

  ```powershell
  Stop-Process -Name harmony-app -Force -ErrorAction SilentlyContinue
  Get-Process harmony-app -ErrorAction SilentlyContinue   # expect: nothing
  Get-NetTCPConnection -LocalPort 9222,5173 -ErrorAction SilentlyContinue  # expect: nothing
  ```

  Always confirm the process is dead before relaunching, or the new instance's
  single-instance guard hands off to the old PID and your env changes are
  ignored.
- **CDP page-target `id` changes on window reattach**, while the **PID does
  not**. Don't cache the target id across a window close/reopen — re-discover it
  via `/json/list` each attach (the driver in §4 does this).
- **Fresh-mint trust badge shows "refused"** ([ZEB-342](https://linear.app/zeblith/issue/ZEB-342))
  on a freshly-minted first-only device. Known cosmetic issue; not a transport
  failure.
- **Vite binds `::1` (IPv6 localhost)** for the dev server. If a tool resolves
  `localhost` to `127.0.0.1` only, prefer the explicit `http://[::1]:5173` or
  `http://localhost` forms that match what's listening.
- **Single-instance hijack during CLI subcommands:** the app is single-instance.
  Running a CLI subcommand (e.g. `export mnemonic`) while the GUI is alive can be
  hijacked by the running instance. Kill the app first (above), then run the CLI.

---

## 7. Identity backup (one-time, human-run)

Back up the identity seed early. The seed mnemonic is **24 BIP39 words** —
those words must **never** enter an agent transcript. A human runs this in a
separate terminal, with the app fully closed:

```powershell
$env:HARMONY_PASSPHRASE_FILE = 'C:\zeblith\secrets\harmony-passphrase.txt'
& 'C:\zeblith\work\zeblithic\harmony-client\src-tauri\target\debug\harmony-app.exe' export mnemonic
```

The **owner master seed** is a separate secret with its own export
(`export owner-mnemonic`, lands with the [ZEB-430](https://linear.app/zeblith/issue/ZEB-430)
work / PR #231). Back that up too once it's available. See
[`../headless-install.md`](../headless-install.md) for the authoritative
backup/recovery reference.

---

## 8. Prove it: two-node message exchange

With the node minted, discoverable, and self-resolving, complete bring-up by
exchanging messages with another node (e.g. AVALON ↔ Ildwyn). Drive your side
over CDP; the other side can be agent-driven or human. The end-to-end steps
(invite, redeem, post, verify) are in
[`../cross-wan-validation.md`](../cross-wan-validation.md) — run them via the
IPC calls in §5 instead of the in-app UI:

1. One side creates/holds the standing test community and issues an invite URL.
2. The other redeems it: `connectivity_redeem_invite_iroh {inviteUrl}`.
3. Post from each side: `post_channel_message {communityId, channelId, body:[...new TextEncoder().encode('hello from <node>')]}`.
4. Verify both messages land on both sides.

That round-trip is the Definition of Done for a new node's bring-up.

---

## 9. Cleanup

- Drop your CDP socket (the driver does this on exit). **Never** `Browser.close`.
- To stop the node, kill the PID per §6 and confirm ports 9222/5173 are free.
- Throwaway identities: delete the redirected `$env:HOME` tempdir.
- Don't commit `.playwright-scratch/` or `src-tauri/gen/schemas/*.json` — both
  are local-only artifacts.

---

## Appendix: per-node values

Keep machine-specific identity IDs (owner_id, device_id, Reticulum address,
iroh EndpointId) in that machine's agent memory, **not** in this repo. This
playbook stays generic so the next node is a copy-paste. Record per-node:

- Passphrase file path (default `C:\zeblith\secrets\harmony-passphrase.txt`).
- Whether Credential Manager is reachable from an agent-spawned process
  (verify with a `cmdkey /list` probe — it's per-machine).
- The minted owner_id / device_id, recorded out-of-band after first boot.
