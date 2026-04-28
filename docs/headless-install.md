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

`harmony-app` ships two complementary backup formats. Both encode the same
master 32-byte seed; restoring from either produces a byte-identical identity.

### Mnemonic backup (recommended primary)

24 BIP39 English words. Defends against complete data loss — write the
words on paper.

```bash
# Export. The 24 words go to stdout; a warning preamble + identity-hash to stderr.
harmony-app export mnemonic > backup.txt
# Restore (refuses if an identity already exists; pass --force to overwrite).
harmony-app restore mnemonic --mnemonic-file backup.txt
```

The mnemonic file is whitespace-tolerant, case-insensitive, and
ASCII-only. Single line, multi-line, indented, mixed case — all valid.

### Encrypted recovery file (secondary, copy-to-USB friendly)

Argon2id + XChaCha20-Poly1305 envelope of the same seed. Defends against
file leak — without the recovery passphrase the file is useless.

```bash
# Export — requires HARMONY_RECOVERY_PASSPHRASE or HARMONY_RECOVERY_PASSPHRASE_FILE.
HARMONY_RECOVERY_PASSPHRASE_FILE=/run/secrets/harmony-recovery \
    harmony-app export recovery-file --out /mnt/usb/identity.harmony --comment "hostname=$(hostname) date=$(date -I)"

# Restore — same env vars; --force required to overwrite an existing identity.
HARMONY_RECOVERY_PASSPHRASE_FILE=/run/secrets/harmony-recovery \
    harmony-app restore recovery-file --in /mnt/usb/identity.harmony
```

### Recovery passphrase env vars

| Var | Format |
|---|---|
| `HARMONY_RECOVERY_PASSPHRASE` | Direct UTF-8 string |
| `HARMONY_RECOVERY_PASSPHRASE_FILE` | Path to a file containing the passphrase (UTF-8, one trailing newline allowed) |

These are **distinct** from the at-rest `HARMONY_PASSPHRASE` /
`HARMONY_PASSPHRASE_FILE`. Neither falls back to the other — restoring a
recovery file with the wrong env var set fails with a docs pointer.

### Identity hash

Every export and restore prints `identity-hash: <hex32>` to stderr. This
is the operator's eyeball-comparison fingerprint: if the hash on the
backup matches the hash you see when restoring on a new machine, the
seeds are byte-identical and the round-trip worked.

### `--force`

Both restore subcommands check whether `~/.harmony/identity.enc` already
exists. Without `--force`, they refuse and exit 1. With `--force`, the
existing file is overwritten in place via the same atomic
tmp-then-rename pattern used elsewhere. **This is destructive** —
verify the identity-hash before passing `--force` on a machine that has
a real identity you want to keep.

### Why two formats?

The mnemonic is the catastrophic-loss recovery path: words on paper
survive everything except the paper itself. The encrypted recovery file
is the routine-portability path: you can copy it across machines, mail
it to yourself, store it on a USB stick — and without the passphrase
it's useless. If both backups have been made, only losing them simultaneously results in identity loss.

The existing "treat `~/.harmony/identity.enc` and your passphrase as
two halves of a recovery key" guidance still applies for at-rest
storage — the recovery commands give you an additional layer.

## Troubleshooting

| Error | Meaning | Fix |
|---|---|---|
| `no identity store available: keychain unavailable and HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set` | Step 4 in resolution chain | Set `HARMONY_PASSPHRASE` or `HARMONY_PASSPHRASE_FILE`, or install/start a Secret Service provider |
| `identity store could not be decrypted: wrong passphrase or corrupted file` | AEAD tag rejected | Verify the passphrase exactly matches what was used to encrypt; do not regenerate identity unless you accept losing it |
| `identity store is in an unrecognized format` | Old binary, newer file | Upgrade harmony-client |
| `identity store verify-after-write failed` | The store accepted the write but returned different bytes — keychain/disk corruption | File a bug; do not retry blindly |
| `Error: expected 24 BIP39 words, got <N>` | Mnemonic file has wrong word count | Re-check the file; trim partial pastes |
| `Error: unknown word at position <N>: "<word>"` | Mnemonic typo | Re-check the indicated word against the BIP39 wordlist |
| `Error: mnemonic checksum mismatch — likely a typo somewhere in the 24 words` | One or more typos | Visually re-verify each word against the source |
| `Error: wrong passphrase or corrupted recovery file (AEAD tag rejected)` | Bad recovery passphrase OR the file was tampered with | Verify the recovery passphrase matches what was used to export |
| `identity already exists at <path>; pass --force ...` or `identity already exists in OS keychain; pass --force ...` | Restore policy | If you really want to overwrite, re-run with `--force`; otherwise, this is the safety net |
| `neither HARMONY_RECOVERY_PASSPHRASE nor HARMONY_RECOVERY_PASSPHRASE_FILE is set — see docs/headless-install.md` | Recovery passphrase missing | Set one; remember it's distinct from the at-rest passphrase |

## Not yet supported

- **OpenWRT and other embedded Linux** — Argon2id with m=64 MiB exceeds available
  RAM on most embedded targets. The harmony-openwrt repo will provide a tuned
  build with smaller KDF params (and bump the on-disk format_version).
- **Hardware tokens (YubiKey, TPM)** — possible future, out of scope here.
