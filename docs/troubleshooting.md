# Troubleshooting

Common issues testers may hit on `harmony-client`. If your problem isn't covered here, submit feedback via the **(?) → Submit Feedback** menu in the app — the form pre-fills environment info and (optionally) a redacted network diagnostic snapshot for you.

## Install + first-launch

### Gatekeeper blocks Harmony on first launch (macOS)

The binary isn't signed with an Apple Developer certificate (a deliberate, permanent choice — see [`install-macos.md`](install-macos.md)). On first launch macOS Gatekeeper shows **one of two messages**:

**"Harmony is damaged and can't be opened. You should move it to the Trash."**
The app is **not** damaged — this is just how recent macOS (especially Apple Silicon) labels an unsigned, freshly-downloaded app. **Don't trash it.** Clear the quarantine flag in Terminal, then launch normally:

```bash
xattr -dr com.apple.quarantine /Applications/Harmony.app
```

Right-click → **Open** does *not* clear this one — only the command above does.

**"Harmony can't be opened because Apple cannot check it for malicious software."**
Open **System Settings → Privacy & Security**, scroll to the "Harmony was blocked…" message, and click **Open Anyway** (or right-click the app in Applications → **Open**).

Subsequent launches don't re-prompt. See [`install-macos.md`](install-macos.md) for the full Gatekeeper walkthrough.

### SmartScreen warns "Windows protected your PC" (Windows)

Same root cause — unsigned binary:

1. Click **More info** in the SmartScreen dialog.
2. Click **Run anyway**.

See [`install-windows.md`](install-windows.md) for the SmartScreen walkthrough.

### AppImage won't launch (Linux)

If `./Harmony.AppImage` fails with "Permission denied":

```bash
chmod +x ./Harmony.AppImage
./Harmony.AppImage
```

If FUSE isn't installed, extract + run:

```bash
./Harmony.AppImage --appimage-extract
./squashfs-root/AppRun
```

See [`install-linux.md`](install-linux.md) for the AppImage walkthrough.

## First-run welcome behaviour

### "I don't see the welcome modal on launch"

The welcome modal fires only on **fresh identity** — when no `harmony.client` keychain entry exists. If you've launched Harmony before on this machine, the modal is suppressed by design (we don't pester returning users).

To force the modal again (e.g., for testing the fresh-install path):

**macOS:**

```bash
security delete-generic-password -s "harmony.client"
```

**Windows / Linux:** delete the equivalent secret-service entry via your OS keychain manager.

Then relaunch Harmony.

### "Welcome modal didn't appear after I pasted my invite"

A welcome modal is automatically suppressed when a `harmony://` URL is delivered to the app at launch — clicking an invite is an explicit action, and stacking a welcome screen in front of it would be jarring. You'll see the invite-redeem dialog directly. The information from welcome (alpha-tester orientation, where the (?) icon lives) is reachable any time via that icon.

## Network connectivity

### Network Health says "Unreachable" or shows red

Open **Sidebar → Network**. The panel diagnoses the four common breakage modes:

1. **No relay home_relay_url** — your iroh endpoint hasn't picked up a default relay. Wait 10-15 seconds after first launch; if still missing, file a feedback report.
2. **Pkarr publish failing** — your reachability record isn't being published to Mainline DHT. Often means the network is blocking outbound UDP. Try a different network (e.g., tethering off your phone) to confirm.
3. **No peers in shared communities** — expected if you haven't joined any community yet. Paste a `harmony://invite/...` URL via Sidebar → Communities → Redeem invite.
4. **Self-test fails on `relay_rtt`** — your local network blocks connections to n0's relay infrastructure. Check firewall settings; corporate networks often need allowlisting `*.n0.network`.

The **Run Self-Test** button at the bottom of the Network panel reports which of the four steps (endpoint init / relay RTT / pkarr publish / pkarr resolve) fails first. Each Fail line includes a short reason.

### "I can't see anyone in my community"

Network Health green but the community looks empty? Check:

1. **Are you actually joined?** Sidebar → Communities → click the community name → Members panel. If you see only yourself, the join handshake didn't complete. Re-paste the invite.
2. **Are your peers online?** Communities are peer-to-peer; if no other member is reachable right now, the roster appears with last-seen timestamps but no live presence.
3. **Have you been offline for a long time?** When you come back online, give it a few minutes for pkarr re-resolution + reachability re-publish. Watch the Network Health "Peers" section for last-seen freshness.

### Cross-WAN doesn't work

See [`cross-wan-validation.md`](cross-wan-validation.md) for the two-host playbook (Step 1: single-machine baseline → Step 2: first contact → Step 3: bidirectional exchange → Step 4: diagnostic export). If both hosts pass Step 1 but fail Step 2, the issue is almost always either NAT topology (double-NAT on one side) or pkarr DHT reachability (some ISPs block).

## Identity + backup

### "How do I back up my identity?"

Use the in-app backup flow: **Identity → Backup**, set a passphrase (12+ characters), and save the encrypted recovery file somewhere safe. Restore later via **Identity → Restore**. Make a backup early — otherwise the identity lives only in your OS credential store.

If you need to reach the underlying secret directly:

- **macOS:** your iroh secret key lives in Keychain Access → "harmony.client". You can export the keychain item via Keychain Access → File → Export. Treat the export as you would a password — it grants full control of your identity.
- **Windows:** Credential Manager → Generic Credentials → "harmony.client". Credential Manager has no export UI — use the in-app Backup flow instead.
- **Linux:** secret-service entry under the "harmony.client" name. Use `secret-tool` or `seahorse` for inspection.

If you never make a backup and lose the credential-store entry, your identity is gone forever — so back up early, and keep the recovery file (and its passphrase) safe.

### "I see a 'Identity not backed up' warning"

The `BackupStalenessWarning` reminds you to back up your identity via **Identity → Backup**. Until you do, treat it as a real prompt rather than dismissing it — an un-backed-up identity is unrecoverable if the credential store is lost.

## Window + app lifecycle

### "I closed the window but Harmony is still running"

Intended: closing the window hides Harmony to the **system tray** and keeps your node online (presence, message sync, and any active call continue). The first time this happens, a notification reminds you. To actually quit, right-click the tray icon → **Quit Harmony**.

- **Windows 11 note:** new tray icons land in the hidden overflow flyout — click the `^` chevron next to the clock if you don't see the Harmony icon.
- If the tray could not be created (the log shows `tray creation failed; window close will exit`), closing the window **does** quit the app — Harmony never runs hidden without a tray icon to bring it back.

### "I relaunched Harmony but it didn't restart"

Also intended: Harmony is single-instance. Launching it while it's already running (including hidden in the tray) re-opens the existing window in the same process — the node is not restarted. To truly restart: tray → Quit Harmony (or, for test drivers, invoke the `quit_app` IPC / kill the process), then launch again.

Test-driver note: a hidden-to-tray instance keeps its PID and process start time; only the webview page target changes on reattach. Scripted "offline" phases must verify the process actually exited rather than trusting a window close.

### serve mode (headless) lifecycle

`harmony-app serve` has no window and no tray — quit it via `POST /v1/shutdown` (authenticated, like every API call) or SIGTERM/Ctrl-C. A "profile already in use" error on start means another harmony-app — the GUI with the API host enabled, or another `serve` — currently holds the profile lock; quit that instance first. See the [API control surface section in `headless-install.md`](headless-install.md#api-control-surface-serve-mode--zeb-445) for the full quickstart.

- **`harmony-app api` exits 2 with "is `harmony-app serve` ... running?"** —
  no live server has written `<data-dir>/api/{port,token}`. Start `serve`
  (or a GUI with `HARMONY_API_PORT`), then retry.
- **GUI launched with `HARMONY_API_PORT` but no API answers** — check the
  log for `ZEB-452: profile lock unavailable`: another process (usually a
  `serve`) holds the profile. One node per profile in v1.
- **`named profile requires HARMONY_PASSPHRASE`** at startup — named
  profiles are file-vault-only; set `HARMONY_PASSPHRASE` or
  `HARMONY_PASSPHRASE_FILE`. See
  [Side-by-side coordination instance](headless-install.md#side-by-side-coordination-instance-named-profiles).
- **`invalid profile name`** — profile names must match
  `[a-z0-9][a-z0-9_-]{0,31}`; `default` is reserved (omit `--profile` /
  `HARMONY_PROFILE` to use the default profile).
- **`UDP bind failed … Reticulum LAN discovery disabled`** is a warning,
  not an error — another local instance holds the port; the node still
  networks via zenoh/iroh/pkarr. Set `HARMONY_RETICULUM_PORT` to rebind to
  a different port, or `HARMONY_RETICULUM_PORT=0` to disable the bind
  attempt entirely.

## Help + feedback

### "I want to send feedback"

Click the **(?)** icon in the top-right of the app. Choose **Submit Feedback**:

- Type a description (≥10 characters).
- Optional: toggle "Attach network diagnostics" — this includes a redacted snapshot (no full identifiers) of your Network Health panel in the GitHub issue body. Review the preview before submitting.
- Click **Submit**. Your default browser opens a pre-filled GitHub new-issue page. Review the body and click **Submit new issue** on GitHub.

See [`feedback.md`](feedback.md) for full details on what gets included.

### "The browser didn't open when I clicked Submit"

The app falls back to copying the GitHub URL to your clipboard with a toast notification ("Couldn't open browser. URL copied to clipboard."). Paste it manually in your browser of choice.

## Where to get more help

- **GitHub issues:** [zeblithic/harmony-client/issues](https://github.com/zeblithic/harmony-client/issues) — file a new issue or search existing ones.
- **In-app diagnostic export:** Sidebar → Network → "Export diagnostics" — produces a redacted markdown report you can attach to bug reports manually.
- **Cross-WAN validation playbook:** [`cross-wan-validation.md`](cross-wan-validation.md) — for testing two-host scenarios end-to-end.
