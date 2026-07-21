# Installing Harmony on macOS

Harmony is in public alpha — anyone with the released client can create or join communities. These instructions cover the full install flow.

---

## 1. Download

Go to **<https://github.com/zeblithic/harmony-client/releases/latest>** and download the `.dmg` that matches your Mac:

**Which chip do I have?**
Click the Apple menu (top-left corner) → **About This Mac**.
- If you see **Chip: Apple M1 / M2 / M3 / M4** (or any M-series) → download `Harmony_X.Y.Z_aarch64.dmg`
- If you see **Processor: Intel Core …** → download `Harmony_X.Y.Z_x64.dmg`

If you pick the wrong one, Harmony won't launch (you'll see an error about the app not being compatible with your Mac).

---

## 2. Install

1. Double-click the downloaded `.dmg` file.
2. A window opens showing the Harmony icon and your Applications folder.
3. Drag **Harmony** into **Applications**.
4. Eject the disk image (drag it to Trash or press Cmd+E).

---

## 3. First launch — Gatekeeper workaround

Because Harmony ships unsigned (no Apple Developer certificate — a deliberate choice, see below), macOS Gatekeeper will block the first launch. You'll see:

> **"Harmony" can't be opened because Apple cannot check it for malicious software.**

This is expected. Here's how to get past it:

1. Open **Applications** in Finder.
2. **Right-click** (or Control-click) the Harmony icon.
3. Choose **Open** from the context menu.
4. A new dialog appears — click **Open** again.

After you do this once, macOS trusts the app permanently. Future launches work normally with a double-click.

> **Why does this happen?** Apple charges developers a recurring fee for the notarization certificate macOS uses to verify an app's origin. Harmony is a small, self-funded project and **deliberately ships unsigned** rather than carry that ongoing cost — the app is fully functional without it. The one-time approval above is all you'll ever need.

---

## 4. xattr fallback (if right-click → Open doesn't work)

On some macOS versions the right-click → Open flow doesn't show the Open button. If that happens:

1. Open **Terminal** (Applications → Utilities → Terminal).
2. Run:

   ```bash
   xattr -dr com.apple.quarantine /Applications/Harmony.app
   ```

3. Launch Harmony normally from Applications.

---

## 5. First-launch permission prompts

When Harmony starts for the first time you may see one or two system permission dialogs:

- **Keychain access** — Harmony stores your identity key in the macOS Keychain. This is required for identity persistence across restarts. If you deny, Harmony won't be able to remember who you are between sessions.
- **Network access** — Harmony opens an outbound connection to join the Harmony network. Required for cross-WAN connectivity. If you deny, Harmony can't reach other users.

Click **Allow** for both. If you accidentally denied either, go to **System Settings → Privacy & Security** and grant access there.

---

## 6. Updating

Harmony checks for updates automatically. When a new build is available you'll see a toast notification inside the app with an **Update** button. The update downloads in the background and applies on next restart.

If the toast doesn't appear after a few days and you know a new release is out, re-download the latest `.dmg` from the releases page and reinstall over your existing copy.

---

## 7. Uninstalling

To remove Harmony:

1. Drag **Harmony** from Applications to Trash.

To also wipe your Harmony identity, community memberships, and local message history:

```bash
rm -rf ~/Library/Application\ Support/net.zeblith.harmony
```

> **Warning:** this permanently destroys your Harmony identity. There is no recovery. Only run this if you intentionally want to start over or fully remove Harmony.
