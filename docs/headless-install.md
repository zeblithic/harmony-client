# Headless install (servers, CI, containers)

harmony-client encrypts identity at rest in one of two ways:

1. **Desktop**: OS keychain (macOS Keychain, Windows Credential Manager, Linux Secret Service via gnome-keyring/KWallet)
2. **Headless**: AEAD-encrypted file at `~/.harmony/identity.enc`, key derived from a passphrase you supply

If you're running on a server, in a container, in CI, or anywhere without a
running OS keychain, you must supply a passphrase via environment variable.
**Without a passphrase, harmony-client refuses to start** — it will not write
plaintext identity material to disk.

## Quickstart (Linux server / Docker)

Generate a passphrase (≥32 chars, store in your secret manager):

```sh
openssl rand -base64 48 > /etc/harmony/passphrase
chmod 600 /etc/harmony/passphrase
chown harmony:harmony /etc/harmony/passphrase
```

Then either:

```sh
# Direct (less common — exposes passphrase in process listings)
HARMONY_PASSPHRASE='...' harmony-app

# File-based (recommended — file is loaded once at startup)
HARMONY_PASSPHRASE_FILE=/etc/harmony/passphrase harmony-app
```

## systemd unit

```ini
[Service]
ExecStart=/usr/local/bin/harmony-app
Environment=HARMONY_PASSPHRASE_FILE=/etc/harmony/passphrase
User=harmony
NoNewPrivileges=true
ProtectSystem=strict
# harmony-app stores the encrypted identity at $HOME/.harmony/identity.enc;
# whitelist that path under ProtectSystem=strict so the daemon can create and
# rotate it. (If you'd rather store identity outside $HOME, set HOME explicitly
# and adjust this path to match.)
ReadWritePaths=/home/harmony/.harmony
```

## Docker

```sh
docker run --rm \
  -v /etc/harmony/passphrase:/run/secrets/passphrase:ro \
  -v harmony-data:/home/harmony/.harmony \
  -e HARMONY_PASSPHRASE_FILE=/run/secrets/passphrase \
  harmony-client
```

## Env var precedence

If both are set, `HARMONY_PASSPHRASE` (direct) wins over `HARMONY_PASSPHRASE_FILE`.
A warning is logged.

## Passphrase format rules

- UTF-8 bytes, used as-is — no Unicode normalization
- Must be byte-stable across boots — same UTF-8 bytes in, same key out
- Empty passphrases (after stripping one trailing `\n` or `\r\n`) are rejected
- File mode should be `0600`; a warning is logged if more permissive

## Migration from prior versions

If you upgrade from an earlier harmony-client that wrote plaintext to
`~/.harmony/identity.key`:

- **With keychain available**: harmony migrates plaintext → keychain on first
  launch, verifies, then deletes the plaintext file. No `.bak` is left.
- **Headless with `HARMONY_PASSPHRASE` or `HARMONY_PASSPHRASE_FILE` set**: same
  migration, destination is `~/.harmony/identity.enc`.
- **Headless without `HARMONY_PASSPHRASE` or `HARMONY_PASSPHRASE_FILE`**: harmony
  refuses to start with a hard error pointing here. Set one of the env vars and
  re-launch.

A `.bak` file from earlier code that did keep backups is auto-cleaned on first
launch after harmony verifies the live store has the same identity.

## Rotating the passphrase

To re-encrypt `identity.enc` with a new passphrase:

```sh
# Old passphrase still needed to decrypt; new one supplied via flag.
HARMONY_PASSPHRASE_FILE=/etc/harmony/old.txt \
  harmony-app rotate-passphrase \
    --new-passphrase-file=/etc/harmony/new.txt
```

The command verifies the new file decrypts correctly before exiting. Once it
returns 0, swap your systemd unit / Docker secret to point at the new file
and discard the old one.

Rotation is only meaningful for the encrypted-file backend. If you're running
on a desktop install (keychain backend), the OS handles re-encryption of
keychain entries when you change your login password — you don't need to do
anything here.

## Backup and recovery

Backup of identity material — including how to recover from device loss,
how to mint a fresh identity that claims continuity with a lost one, and
how to export/import a recovery artifact — is the scope of **ZEB-175**
(Identity backup/restore UX). This document covers encryption-at-rest only.

In the meantime: treat `~/.harmony/identity.enc` and your passphrase as
two halves of a recovery key. Lose either and the other is useless. Back
both up to separate storage if you can't tolerate identity loss.

## Troubleshooting

| Error | Meaning | Fix |
|---|---|---|
| `no identity store available: keychain unavailable and HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set` | Step 4 in resolution chain | Set `HARMONY_PASSPHRASE` or `HARMONY_PASSPHRASE_FILE`, or install/start a Secret Service provider |
| `identity store could not be decrypted: wrong passphrase or corrupted file` | AEAD tag rejected | Verify the passphrase exactly matches what was used to encrypt; do not regenerate identity unless you accept losing it |
| `identity store is in an unrecognized format` | Old binary, newer file | Upgrade harmony-client |
| `identity store verify-after-write failed` | The store accepted the write but returned different bytes — keychain/disk corruption | File a bug; do not retry blindly |
| `plaintext identity at <path> needs a destination but no keychain available and HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set` | Existing plaintext file but no destination to migrate it to | Set `HARMONY_PASSPHRASE` or `HARMONY_PASSPHRASE_FILE`, or run on a system with a keychain — harmony will migrate the plaintext on next launch |

## Not yet supported

- **OpenWRT and other embedded Linux** — Argon2id with m=64 MiB exceeds available
  RAM on most embedded targets. The harmony-openwrt repo will provide a tuned
  build with smaller KDF params (and bump the on-disk format_version).
- **Hardware tokens (YubiKey, TPM)** — possible future, out of scope here.
