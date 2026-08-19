# Installing Harmony on macOS

Harmony is in public alpha — anyone with the released client can create or join communities. These instructions cover the full install flow.

---

## 1. Download

Go to **<https://github.com/zeblithic/harmony-client/releases/latest>** and download the `.dmg` that matches your Mac:

**Which chip do I have?**
Click the Apple menu (top-left corner) → **About This Mac**.
- If you see **Chip: Apple M1 / M2 / M3 / M4 / M5** (or any M-series) → download `Harmony_X.Y.Z_aarch64.dmg`
- If you see **Processor: Intel Core …** → download `Harmony_X.Y.Z_x64.dmg`

Pick the build that matches your chip for the best speed. (The Intel `x64` build *will* also run on Apple Silicon through Rosetta 2 — just slower — but the Apple Silicon `aarch64` build will **not** run on an Intel Mac.)

> Download the **`.dmg`** — not the `.app.tar.gz`. That tarball is the auto-updater's internal payload, not a manual installer.

---

## 2. Install

1. Double-click the downloaded `.dmg` file.
2. A window opens showing the Harmony icon and your Applications folder.
3. Drag **Harmony** into **Applications**.
4. Eject the disk image (drag it to Trash or press Cmd+E).

---

## 3. First launch — getting past Gatekeeper

Harmony isn't signed with an Apple Developer certificate (a deliberate, permanent choice — see the end of this section), so macOS can't confirm who published it. Gatekeeper blocks the **first** launch with a message about the app being damaged, unverified, or from an unidentified developer. The exact wording varies by macOS version — but the fix is the same **one Terminal command** in every case:

1. Open **Terminal** (Applications → Utilities → Terminal).
2. Copy-paste this whole line and press Return:

   ```bash
   xattr -dr com.apple.quarantine /Applications/Harmony.app
   ```

3. Launch Harmony normally from Applications (double-click). It opens from now on.

> If you left Harmony somewhere other than Applications, point the command at wherever it is, e.g. `~/Downloads/Harmony.app`.

**Don't bother with the traditional workarounds — for Harmony they don't work.** On current macOS (verified on Sequoia and later with the shipping builds):

- **Right-click → Open** does not offer a working "Open" path.
- **System Settings → Privacy & Security** does not show an "Open Anyway" entry for the block.
- If the message says the app is **"damaged" — do _not_ move it to the Trash.** The download is fine; that alarming wording is just how macOS labels an unsigned app still carrying the invisible "downloaded from the internet" quarantine flag.

The Terminal command clears that quarantine flag directly, which is why it works when the buttons don't.

---

After you get past Gatekeeper once, macOS trusts the app — future launches work normally with a double-click.

> **Why does this happen?** Signing and notarizing an app so macOS can verify its origin requires an **Apple Developer Program** membership — a recurring annual fee that bundles the Developer ID certificate and notarization. Harmony is a small, self-funded project and **deliberately ships without one** rather than carry that ongoing cost. The app is fully functional without it, and the one-time approval above is all you'll ever need.

---

## 4. First-launch permission prompts

When Harmony starts for the first time you may see one or two system permission dialogs:

- **Keychain access** — Harmony stores your identity key in the macOS Keychain. This is required for identity persistence across restarts. If you deny, Harmony won't be able to remember who you are between sessions.
- **Network access** — Harmony opens an outbound connection to join the Harmony network. Required for cross-WAN connectivity. If you deny, Harmony can't reach other users.

Click **Allow** for both. If you accidentally denied either, go to **System Settings → Privacy & Security** and grant access there.

---

## 5. Updating

Harmony checks for updates automatically. When a new build is available you'll see a toast notification inside the app with an **Update** button. The update downloads in the background and applies on next restart.

If the toast doesn't appear after a few days and you know a new release is out, re-download the latest `.dmg` from the releases page and reinstall over your existing copy.

---

## 6. Uninstalling

To remove Harmony:

1. Drag **Harmony** from Applications to Trash.

To also wipe your Harmony identity, community memberships, and local message history:

```bash
rm -rf ~/Library/Application\ Support/net.zeblith.harmony
```

> **Warning:** this permanently destroys your Harmony identity. There is no recovery. Only run this if you intentionally want to start over or fully remove Harmony.
