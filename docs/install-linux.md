# Installing Harmony on Linux

Harmony ships as an AppImage for Linux — a self-contained executable that runs on most x86_64 desktop distributions without installation.

---

## 1. Download

Go to **<https://github.com/zeblithic/harmony-client/releases/latest>** and download:

```
harmony_X.Y.Z_amd64.AppImage
```

(Replace `X.Y.Z` with the version number shown on the releases page.)

Harmony currently ships for x86_64 (amd64) only. ARM Linux is not yet supported.

---

## 2. Make it executable

Open a terminal in the directory where you downloaded the file and run:

```bash
chmod +x harmony_*.AppImage
```

---

## 3. Launch

Run it from the terminal:

```bash
./harmony_*.AppImage
```

Or double-click it in your file manager (Nautilus, Dolphin, Thunar, etc.). Some file managers ask "Do you want to run this executable?" — click **Run**.

---

## 4. Desktop integration (optional)

By default the AppImage doesn't add itself to your application launcher. If you want menu integration, install **AppImageLauncher**:

<https://github.com/TheAssassin/AppImageLauncher>

AppImageLauncher intercepts AppImage launches and offers to integrate them into your menu automatically. Harmony doesn't ship native `.deb` or `.rpm` packages yet; AppImageLauncher is the cleanest workaround until we do.

---

## 5. libsecret dependency (required)

Harmony requires `libsecret` to store your identity key securely. On most desktop installs this is already present via GNOME Keyring or KWallet, but if Harmony fails to start or shows a keychain error, install it:

**Debian / Ubuntu / Linux Mint:**

```bash
sudo apt install libsecret-1-0 gnome-keyring
```

**Fedora / RHEL / CentOS Stream:**

```bash
sudo dnf install libsecret gnome-keyring
```

**Arch Linux:**

```bash
sudo pacman -S libsecret gnome-keyring
```

`libsecret` is a hard requirement for identity persistence. Without it, Harmony can't remember your identity between sessions.

> **Server / headless use:** if you're running Harmony on a machine without a graphical session or keyring daemon, see [docs/headless-install.md](headless-install.md) for the server-mode setup.

---

## 6. glibc requirement

The AppImage is built on Ubuntu 22.04 and requires **glibc 2.35 or newer**. It will not run on:

- Ubuntu 20.04 LTS (glibc 2.31)
- RHEL 7 / CentOS 7 (glibc 2.17)
- Debian 10 Buster (glibc 2.28)

If you see an error like `version 'GLIBC_2.35' not found`, your distribution is too old. Upgrade to a newer release or use a newer distro.

---

## 7. Updating

Harmony checks for updates automatically. When a new build is available you'll see a toast notification inside the app with an **Update** button. The update downloads in the background and applies on next restart.

To update manually: download the new `.AppImage` from the releases page, make it executable (`chmod +x`), and replace your existing one.

---

## 8. Uninstalling

To remove Harmony, delete the AppImage:

```bash
rm harmony_*.AppImage
```

To also wipe your Harmony identity, community memberships, and local message history:

```bash
rm -rf ~/.config/net.zeblith.harmony
```

If you've set `$XDG_CONFIG_HOME` to a custom location, substitute that path instead of `~/.config`.

> **Warning:** deleting the config directory permanently destroys your Harmony identity. There is no recovery. Only do this if you intentionally want to start over or fully remove Harmony.
