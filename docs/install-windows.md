# Installing Harmony on Windows

Harmony is in public alpha — anyone with the released client can create or join communities. These instructions cover the full install flow on Windows 10 or 11 (x64).

---

## 1. Download

Go to **<https://github.com/zeblithic/harmony-client/releases/latest>** and download:

```
Harmony_X.Y.Z_x64-setup.exe
```

(Replace `X.Y.Z` with the version number shown on the releases page.)

Harmony currently ships for 64-bit Windows only. ARM Windows is not yet supported.

---

## 2. Run the installer — SmartScreen workaround

Double-click the `.exe` file. Because Harmony ships without an extended-validation code-signing certificate (a deliberate choice, see below), Windows SmartScreen will block it:

> **Windows protected your PC**
> Microsoft Defender SmartScreen prevented an unrecognized app from starting.

This is expected. Here's how to get past it:

1. Click **More info** (below the warning text).
2. A **Run anyway** button appears — click it.
3. If a User Account Control (UAC) prompt appears, click **Yes**.
4. Follow the installer steps (Next → Install → Finish).

After you install once, Windows trusts the app and won't warn you again for the same version.

> **Why does this happen?** SmartScreen warns about apps it doesn't recognize — an unsigned build has no publisher reputation, so each new release is flagged until it earns trust. A code-signing certificate (which accrues that reputation over time) carries a recurring cost that Harmony — a small, self-funded project — **deliberately skips**; the app is fully functional unsigned, and the one-time approval above clears it for that version.

---

## 3. Install location

The installer places Harmony in your user profile by default — no admin rights required:

```
%LOCALAPPDATA%\Programs\Harmony\
```

A shortcut is added to your Start menu and optionally your Desktop.

---

## 4. First-launch firewall prompt

The first time Harmony runs, Windows Defender Firewall may ask whether to allow Harmony to communicate on the network:

> **Allow Harmony to communicate on these networks?**

Check at least **Private networks**. If you use Harmony on public Wi-Fi (coffee shops, airports), check **Public networks** too.

Click **Allow access**. If you click **Cancel**, Harmony can't reach other users on the Harmony network. You can fix this later in **Control Panel → Windows Defender Firewall → Allow an app through Firewall**.

---

## 5. Updating

Harmony checks for updates automatically. When a new build is available you'll see a toast notification inside the app with an **Update** button. The update downloads in the background and applies on next restart.

If the toast doesn't appear after a few days and you know a new release is out, re-download the latest `.exe` from the releases page and run it — the installer will upgrade your existing installation in place.

---

## 6. Uninstalling

To remove Harmony:

1. Open **Settings → Apps** (or **Settings → Apps & features** on Windows 10).
2. Search for **Harmony**.
3. Click **Uninstall** and follow the prompts.

To also wipe your Harmony identity, community memberships, and local message history, delete this folder after uninstalling:

```
%APPDATA%\net.zeblith.harmony
```

You can paste that path directly into the Windows Explorer address bar.

> **Warning:** deleting this folder permanently destroys your Harmony identity. There is no recovery. Only do this if you intentionally want to start over or fully remove Harmony.
