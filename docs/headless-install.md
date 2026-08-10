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

### Paired export (identity + owner-state)

```bash
export HARMONY_PASSPHRASE="$(cat at-rest.passphrase)"
export HARMONY_RECOVERY_PASSPHRASE="$(cat recovery.passphrase)"
harmony-app export recovery-file --out /mnt/usb/recovery.bin --comment "2026-05-14 paired"

# Output (stderr):
# wrote /mnt/usb/recovery.bin (101 bytes)
# wrote /mnt/usb/recovery.bin.state (12345678 bytes)
# identity-hash: 1a2b3c4d...
```

The `recovery.bin.state` sidecar carries the encrypted owner-state CRDT
(your nav tree + DM history metadata + read markers). Store both files
together.

### Identity-only export

```bash
harmony-app export recovery-file --out /mnt/usb/identity-only.bin --no-state
```

Emits only `identity-only.bin`. No sidecar. Equivalent to the pre-ZEB-213
behavior. Useful when sharing an identity backup with a trusted operator
who shouldn't see your nav tree.

### Paired restore

```bash
export HARMONY_PASSPHRASE="$(cat at-rest.passphrase)"
export HARMONY_RECOVERY_PASSPHRASE="$(cat recovery.passphrase)"
harmony-app restore recovery-file --in /mnt/usb/recovery.bin --force

# Output (stderr):
# recovery-file minted-at: 2026-05-14T18:32:11Z
# recovery-file comment: 2026-05-14 paired
# restored identity-hash: 1a2b3c4d...
# owner-state snapshot: 47 spaces, exported 1715600000000 ms wall-clock
```

The `minted-at` line (RFC 3339 UTC, stamped at export time) and the
`comment` line (the export's `--comment` text) echo the recovery file's
metadata as a **post-restore confirmation** — the identity has already
been written by the time these print, so they confirm *which* backup was
applied (and let you notice after the fact that a stale one was used), not
as a pre-commit preview. Either line is omitted when the file carries no
such metadata — e.g. `--comment` was not passed, or the file predates
metadata stamping.

If a `PATH.state` sidecar (derived from the recovery file passed as `--in PATH`) exists, it is auto-detected and restored alongside the main recovery file.

### Identity-only restore (ignore sidecar)

```bash
harmony-app restore recovery-file --in /mnt/usb/recovery.bin --ignore-state --force
```

Skips the sidecar even if present. The restored device starts with an
empty owner-state; Flow A (Zenoh state-root sync) will populate it from
any surviving bound device of the same owner.

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

### Concurrent identity writes

Every writer of the at-rest identity material — `restore mnemonic`,
`restore recovery-file`, `rotate-passphrase`, and first-boot identity
**generation** (when no identity exists yet) — takes the same OS advisory
lock on `~/.harmony/identity.enc.lock` spanning its whole check-and-write.
So any two of them racing the same `~/.harmony` cannot silently clobber
each other: the loser fails fast with `another harmony-app process is
writing the identity store; retry once it finishes` rather than overwriting
the winner. A first-boot generate that loses the race re-checks under the
lock and adopts the identity the winner just wrote, instead of generating a
throwaway one. The lock is released automatically if the holder process
dies, so a crash never strands it — no manual lockfile cleanup is needed.

Concurrent `serve` / GUI boots of the *same profile* are additionally
excluded earlier, by the per-profile `serve.lock`.

### Why two formats?

The mnemonic is the catastrophic-loss recovery path: words on paper
survive everything except the paper itself. The encrypted recovery file
is the routine-portability path: you can copy it across machines, mail
it to yourself, store it on a USB stick — and without the passphrase
it's useless. If both backups have been made, only losing them simultaneously results in identity loss.

The existing "treat `~/.harmony/identity.enc` and your passphrase as
two halves of a recovery key" guidance still applies for at-rest
storage — the recovery commands give you an additional layer.

## Backup and recovery (GUI)

Most desktop users will use the GUI wizard. The CLI walkthrough above is for
headless installs (server, Docker, CI). The two are interchangeable — a backup
exported via the GUI restores fine via the CLI and vice versa.

### Open the Identity panel

Navigate to **Settings → Identity**. The panel shows your current identity hash
(8-char prefix displayed; click to copy the full 32-char hex).

### Back up to a 24-word recovery phrase

1. Click **Backup…** → choose **24-word recovery phrase** → **Continue**.
2. Click **Reveal** to show all 24 words. Write every word down on paper. Anyone
   who has this phrase can recover your identity.
3. Tick **I've stored this safely**, then click **Done**.

The mnemonic phrase produced by the GUI is identical in format to the one
produced by `harmony-app export mnemonic`. Either can be restored via the other
path.

### Back up to an encrypted recovery file

1. Click **Backup…** → choose **Encrypted recovery file** → **Continue**.
2. Type a passphrase (entered twice). Optionally fill in the **Comment** field
   (e.g. `laptop-2026-04-15`) — it is stored in plaintext in the file envelope
   and shown during restore.
3. Click **Save** to open the system save dialog. Choose a name and location for
   the `.recovery` file (a USB stick or off-device storage is recommended).
4. The wizard confirms the saved path. Click **Done**.

The `.recovery` file is the same Argon2id + XChaCha20-Poly1305 envelope that
`harmony-app export recovery-file` produces. You can restore it with either the
GUI or the CLI.

### Restore from a 24-word recovery phrase

1. Click **Restore…** → choose **24-word recovery phrase** → **Continue**.
2. Paste or type your 24 words into the text area. The wizard validates the
   BIP39 checksum and shows the resulting identity hash.
3. Click **Continue** to reach the confirmation step. The wizard shows your
   **current** identity hash and the **to-be-restored** identity hash side by
   side. Type the first 8 characters of your **current** identity hash into the
   confirmation field to proceed.
4. Click **Replace identity**. This is destructive — your previous identity is
   gone after this step.
5. The done screen shows the new identity hash. Verify it matches what you
   expected (compare to the hash shown during export).

### Restore from an encrypted recovery file

1. Click **Restore…** → choose **Recovery file** → **Continue**.
2. Click **Pick recovery file…** to open the system file picker. Select your
   `.recovery` file.
3. Type the passphrase you set at backup time. Click **Decrypt**.
4. The wizard shows the restored identity hash along with the backup's **Minted**
   timestamp and **Comment** (if you set one at export). Click **Continue**.
5. Confirmation step — same as the mnemonic flow: type the first 8 chars of your
   current identity hash, then click **Replace identity**.
6. Verify the new identity hash on the done screen.

### Cross-format compatibility

A mnemonic exported via the GUI is restorable via the CLI:

```bash
# Paste your 24 words into a file, one per line or space-separated.
harmony-app restore mnemonic --mnemonic-file /tmp/mnemonic.txt
```

A recovery file exported via the CLI is restorable via the GUI: open
**Restore… → Recovery file** and pick the file from the system file picker.

Cross-format round-trips are tested by the CLI↔GUI integration tests; both
export formats are byte-for-byte compatible.

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

## API control surface (serve mode) — ZEB-445

`harmony-app serve` runs a windowless node — no Tauri, no webview — and
exposes the same control surface the GUI uses over a localhost-only HTTP+WS
API. Agents and scripts drive it with `curl`/`websocat` or any HTTP client.
The RPC surface is **the same command names, same camelCase JSON args, same
DTOs, and same error strings** as the GUI's Tauri IPC — one mental model
across both.

Three moving parts, all under `<data-dir>/api/` (the platform app-data dir
for `net.zeblith.harmony`):

| File | Contents |
|---|---|
| `token` | Bearer token, mode `0600`. Required on every request. |
| `port` | The actually-bound port (default 7420; `--api-port` / `HARMONY_API_PORT`; `0` = OS-assigned ephemeral). |
| `serve.lock` | PID lockfile — single profile per machine (see caveats). |

### Quickstart (macOS)

```bash
harmony-app serve &   # default port 7420; --api-port 0 for ephemeral
API="$HOME/Library/Application Support/net.zeblith.harmony/api"
TOKEN=$(cat "$API/token"); PORT=$(cat "$API/port")
H="Authorization: Bearer $TOKEN"
curl -s -H "$H" "http://127.0.0.1:$PORT/v1/status"
# fresh profile: mint first
curl -s -H "$H" -X POST -H 'Content-Type: application/json' -d '{}' \
  "http://127.0.0.1:$PORT/v1/rpc/mint_owner_identity"
curl -s -H "$H" -X POST -H 'Content-Type: application/json' \
  -d '{"name":"test","isInviteOnly":false}' \
  "http://127.0.0.1:$PORT/v1/rpc/create_community"
# event stream (websocat):
websocat -H="$H" "ws://127.0.0.1:$PORT/v1/events"
# quit:
curl -s -H "$H" -X POST "http://127.0.0.1:$PORT/v1/shutdown"
```

**Windows:** same flow; the api dir is `%APPDATA%\net.zeblith.harmony\api\`
(Linux: `~/.local/share/net.zeblith.harmony/api/`).

First boot is **pre-mint** — `serve` starts the node but does not create an
identity. `GET /v1/status` reports `"ownerId": null` until you `POST
/v1/rpc/mint_owner_identity` (once per profile; subsequent boots load it).

### The v1 RPC surface (common commands)

All commands are `POST /v1/rpc/{command}` with a JSON object body
(camelCase keys, exactly the args the GUI sends). No-arg commands accept
`{}` or an empty body.

The table below is a curated subset of the most commonly used commands —
the registry has grown well past it. The authoritative, always-current
list is the `registry_has_exactly_the_curated_v1_surface` pin test in
`src-tauri/src/api/rpc.rs`.

| Capability | Commands |
|---|---|
| Node lifecycle (2) | `start_node`, `stop_node` (serve auto-starts the node at boot) |
| Identity (2) | `get_owner_state`, `mint_owner_identity` |
| Communities (9) | `list_owner_communities`, `create_community`, `list_community_members`, `generate_invite`, **`connectivity_redeem_invite_iroh`**, **`connectivity_open_join_iroh`**, `leave_community`, `list_left_communities`, `remove_space` |
| Channels (5) | `create_channel`, `list_channels`, `list_channel_messages`, **`list_mentions`**, `post_channel_message` |
| Files / sharing (7) | `ingest_content_encrypted`, `grant_read`, `revoke_read`, `list_grants`, `list_received_grants`, `dismiss_received_grant`, `burn_content` |
| Friends (9) | `list_friends`, `generate_friend_token`, `redeem_friend_token`, `add_friend_by_key`, `list_pending_friend_requests`, `accept_friend_request`, `decline_friend_request`, `list_outbound_friend_requests`, `cancel_outbound_friend_request` |
| Spaces / DMs (3) | `add_space`, `send_dm`, `read_dm_thread` |
| Diagnostics (5) | `connectivity_get_my_reachability_record`, `connectivity_list_peer_reachability`, **`connectivity_pkarr_publication_status`**, `network_health_snapshot`, `network_health_run_self_test` |
| Pairing (6) | `start_inviter_pairing` (`{"displayName": string}`), `start_joiner_pairing` (`{"displayName": string}`), `select_pairing_peer` (`{"peerSessionId": uuid}`), `confirm_pairing_sas`, `cancel_pairing`, `get_pairing_state` → `PairingState` JSON |

Beyond the RPC surface: `GET /v1/status` (liveness + identity + uptime),
`POST /v1/shutdown` (graceful quit), `GET /v1/events` (WS firehose).

> **Joining a community: use the `connectivity_*` verbs.** A `redeem_invite`
> command is also registered, but it is the pre-iroh Reticulum path and
> **cold-fails between two never-met nodes** — which is the normal case for
> redeeming an invite. Since invite-only invites are single-use, calling it
> can cost you the invite on a guaranteed failure. Reach for
> `connectivity_redeem_invite_iroh` (invite-only) or
> `connectivity_open_join_iroh` (open), which do the real first contact:
> pkarr-resolve + iroh handshake.
>
> Both take **`url`**, not `inviteUrl` — the Tauri IPC uses the camelCase
> name and the RPC surface does not. Passing the wrong key fails with
> `400 missing field 'url'`, which is at least a loud failure.
>
> Two more things that bite scripted joins. Channel materialisation can lag
> the join, so **poll `list_channels` rather than asserting once** — it may
> briefly return `[]` or `no engine for <community>/<channel>`. And
> `list_channel_messages` requires an explicit `limit`; the GUI's adapter
> defaults it, this surface does not.

> **Watching a channel: `limit` means the LATEST N.** `order` defaults to
> `"desc"`, and it decides which end `limit` cuts from as well as the
> sequence you get back. Pass `"asc"` and `{limit: 50}` becomes *the fifty
> oldest messages in the channel* — a window that stops changing the moment
> the channel outgrows it, while every poll keeps succeeding and returning
> fifty real messages. Two fleet watchers were blind for ~18 minutes each
> that way (ZEB-789), because a frozen window is indistinguishable from a
> quiet channel.
>
> `"desc"` was **not** the default before this release. If you are driving a
> node built before it, passing `order` does nothing: this surface used to
> ignore unknown keys, so an older node accepts `order` and silently returns
> the oldest window (ZEB-797). Unknown keys are now a `400` naming the field,
> so "my node is too old for this argument" is a one-line diagnosis — but
> that only helps against nodes new enough to reject.

> **`list_mentions`: check `complete` before believing an empty reply.** The
> verb answers "which messages address me", so an agent can wake on being
> addressed instead of on every message:
>
> ```bash
> harmony-app api --profile <name> list_mentions '{"limit": 50}'
> #   → { "mentions": [ { "communityId", "channelId", "channelName",
> #                       "messageId", "author", "at" }, ... ],
> #       "complete": true,
> #       "truncatedChannels": [], "unavailableCommunities": [],
> #       "unavailableChannels": [] }
> ```
>
> `communityId` is optional (omit to scan every joined community). `since` is
> **your** cursor, not server-held read state — pass the `at` of the newest
> mention you have handled and the query is resumable and idempotent. `limit`
> follows the same contract as `list_channel_messages`, `0` included.
>
> **`complete: false` means never advance your cursor past this reply.** An
> empty `mentions` can mean "nobody addressed you" or "I could not look", and
> those are not the same answer. Gate on `complete` rather than on the detail
> lists: it is true only when every scope was read and no window filled, so it
> stays correct if another incompleteness reason is ever added. The lists tell
> you what to do about it:
>
> - `truncatedChannels` — the channel was read but its window filled. Each
>   channel is scanned newest-first for up to `limit` **messages** and the
>   mention filter runs after that read, so a channel with `limit` recent
>   non-mention messages can hide older mentions behind the window. Remedy:
>   widen `limit`, or advance `since` and re-poll.
> - `unavailableCommunities` — no engine is running for that community, so
>   *none* of its channels were read: a community still starting, one joined
>   while the node was already running, or a Space carrying no `admin_addr`
>   (which never gets an engine, so it stays listed). Remedy: retry.
>
>   On the node this was measured against the API did not begin answering
>   until after every engine had spawned, so a scan right after boot came
>   back complete rather than listing everything. Do not rely on that
>   ordering — it was one node with a handful of communities, and nothing
>   guarantees it.
> - `unavailableChannels` — the community knows the channel but no log engine
>   is registered for it. Remedy: retry.
>
> A cold scope is reported rather than raised as an error, because one
> community still starting must not fail a scan that spans thirty. Single-
> channel verbs like `list_channel_messages` do error (`no engine for …`) —
> there the missing engine is the whole request.

> **`add_friend_by_key` needs the TARGET to be discoverable — which is now ON
> by default (ZEB-881), but can be turned off.** Case-B — the public identity
> slot this verb resolves against — publishes automatically for a fresh
> identity, so first contact works out of the box. A node whose operator
> explicitly disabled discoverability (or one still in the fail-closed
> untrusted-state posture) does not publish case-B; dialling it returns a bare
> `unreachable`, the same answer you get for offline, wrong key, or a broken
> DHT.
>
> To confirm a target is reachable (or diagnose an `unreachable`):
>
> ```bash
> harmony-app api --profile <name> connectivity_pkarr_publication_status '{}'
> #   → { "identityActive": true, ... }   ← the direct answer
> # only if it was turned off:
> harmony-app api --profile <name> connectivity_set_identity_discoverable '{"enabled": true}'
> ```
>
> `identityActive: false` means no dialler can find you, whatever they do.
> The node also states its discoverability at boot — grep the log for `ZEB-794`.
>
> **The friend-token path needs no discoverability at all.** `generate_friend_token`
> publishes case-A as part of minting the token, so
> `generate_friend_token` → `redeem_friend_token` works even against a node
> whose discoverability was turned off. If you only need the two nodes to
> become friends, that path has the fewest preconditions.

> **A freshly-minted identity starts from the product privacy defaults
> (ZEB-796 + ZEB-881).** `mint_owner_identity` normalizes the four connectivity
> privacy/trust toggles — `identity_discoverable` (→ **ON**),
> `friend_auto_accept_known`, `presence_invisible`, `peer_intro_policy` — to
> their product first-run defaults, so a re-minted identity neither inherits the
> previous identity's posture nor drops into the fail-closed (untrusted-state)
> posture: a deliberate mint is treated as a fresh install, and discoverable-ON
> is what makes first cross-WAN contact work (ZEB-881). The machine's relay pool
> (`relays` / `iroh_relays`) is operational infra and **persists** across a mint.
>
> This matters when you use "mint a fresh profile" as a test control:
> `connectivity-settings.json` is keyed to the app-data dir, not the identity,
> so it outlives the identity. Pre-ZEB-796 a new identity inherited whatever
> flag the file already held (this produced a false conclusion during the
> ZEB-770 exercise: `enrolledAt` dates the *identity*, but the flag lived in a
> longer-lived file). Now a fresh mint deterministically reads
> `identity_discoverable: true` — a valid clean-slate control, but the clean
> state is **discoverable ON**, so to exercise the opted-out path you must
> disable it explicitly after minting. Relays are deliberately carried over
> either way.

> **Sharing files: `grant_read` requires an Active friend.** Grants deliver
> over the friend transport, so sharing with someone you merely share a
> community with is rejected as `ineligible: non_friend`. Friend them first
> (`generate_friend_token` / `redeem_friend_token`). `revoke_read` has no such
> gate — a grantee can always be revoked.
>
> `ingest_content_encrypted` takes a `sourcePath` and **has no size cap** — it
> streams whatever you point it at, frame-by-frame. Non-regular files (FIFOs,
> device nodes) are rejected, but a very large regular file will simply be
> ingested in full. Bound it on the caller's side if that matters to you.

### Error contract

Every error response is JSON: `{"error": "<message>"}`.

| Status | Meaning |
|---|---|
| `401` | Missing or wrong bearer token (body: `{"error": "unauthorized"}`). |
| `404` | Unknown command — not in the curated v1 registry. |
| `400` | Args failed to deserialize (the message names the offending field). |
| `500` | The command itself failed — the message is **the same string the GUI IPC rejects with** (e.g. "owner identity not loaded"). |

Parity note: a script that handles the GUI's error strings handles serve
mode's `500` bodies unchanged, and vice versa.

### Caveats

- **Single profile per machine.** `serve` takes `<data-dir>/api/serve.lock`
  (PID lockfile; stale locks from crashed processes are reclaimed). A second
  `serve` — or a GUI launch with the API host enabled — refuses with
  "profile already in use". This is deliberate: two nodes on one profile
  would race identity/state files.
- **The plain GUI does not check the lock.** Don't launch the desktop GUI
  while `serve` holds the profile — the lock only protects against other
  lock-aware processes.
- **Keychain-backed vault.** serve-mode v1 stores identity in the OS
  keychain — the same desktop backend described at the top of this file. On
  a box with no keychain, the `HARMONY_PASSPHRASE` encrypted-file fallback
  (see [Quickstart](#quickstart-linux-server--docker) above) applies as
  usual; the encrypted-file fallback now also covers the iroh transport key
  (ZEB-449), so a keychain-less node networks normally as long as a
  passphrase is configured.
- **`HARMONY_DISABLE_KEYCHAIN=1` is not a "leave my keychain alone" switch.**
  On a real launch it forces every vault read to the encrypted file, so
  **without** `HARMONY_PASSPHRASE` / `HARMONY_PASSPHRASE_FILE` it disables
  iroh transport for the session — the node boots but can't network, and the
  Network Health panel shows a persistent "this node can't network" banner
  (ZEB-450). It does **not** block owner mint (the owner seed has its own
  encrypted-file fallback). **Never set it on a real launch unless a
  passphrase is configured** — it exists only for test-spawned children that
  don't need transport and must not touch the developer's real keychain
  (ZEB-428).
- **The event stream is a firehose.** `GET /v1/events` delivers live events
  only — no replay, no resume. A slow consumer that falls behind receives an
  explicit `{"event": "_lagged"}` marker frame and continues from the live
  edge; re-fetch state via RPC after a lag marker.

### GUI-mode opt-in (`HARMONY_API_PORT`)

A windowed (GUI) instance hosts the identical API when launched with
`HARMONY_API_PORT` set (env var only in v1 — no flag):

```bash
HARMONY_API_PORT=7420 harmony-app        # fixed port
HARMONY_API_PORT=0 harmony-app           # OS-assigned; read <data-dir>/api/port
```

Behavior notes:

- Auth, discovery files, and the profile lock are identical to `serve`.
- Every node event reaches the webview **and** the WS firehose.
- If the profile lock is already held (e.g. a `serve` process on the same
  profile), the GUI launches normally but the API stays **disabled** — look
  for `ZEB-452: profile lock unavailable` in the log. The lock exists
  because two nodes on one profile race state files; don't mix `serve` and
  a GUI on the same profile.
- `POST /v1/shutdown` quits the whole app gracefully (the API analogue of
  tray → Quit). A GUI quit by any other path may leave stale
  `<data-dir>/api/{port,token}` files behind; they are rewritten on every
  server start and are safe to ignore or delete.

### CLI client (`harmony-app api`)

No new binary — two subcommand forms on the existing app, reading
`<data-dir>/api/{port,token}`:

```bash
# One-shot RPC: result JSON on stdout, exit 0.
harmony-app api get_owner_state
harmony-app api create_community '{"name":"testbed","isInviteOnly":true}'

# Event firehose: one {seq,event,payload} JSON frame per line until ^C.
harmony-app api --events
```

Exit codes: `0` success · `1` server-reported error (the error JSON goes
to stderr — same strings the GUI sees) · `2` local failure (server not
running / discovery files missing / args not valid JSON / bad usage).

Stdout carries ONLY the result JSON or event frames (PR #231 stdout-purity
discipline); logs and errors go to stderr, so `harmony-app api ... | jq .`
is always safe. `curl`/`websocat` remain first-class alternatives.

## Side-by-side coordination instance (named profiles)

Run a pinned release build as an always-on headless "coordination" device
beside your dev instance — same machine, separate enrolled device, zero
shared state.

1. **Pin a binary** (copy it OUT of the build tree so dev rebuilds can't
   touch it):

   ```bash
   cp target/release/harmony-app ~/harmony-pinned/harmony-app
   ```

2. **Create a vault passphrase file** (named profiles never touch the OS
   keychain — they use the encrypted-file vault):

   ```bash
   umask 077
   echo 'a-long-random-passphrase' > ~/.harmony-coord-pass
   ```

3. **Start the coordination instance** on its own profile and API port:

   ```bash
   HARMONY_PASSPHRASE_FILE=~/.harmony-coord-pass \
     ~/harmony-pinned/harmony-app serve --profile coord --api-port 7421
   ```

   Storage lands under `~/.harmony/profiles/coord/` and
   `<data-dir>/net.zeblith.harmony/profiles/coord/`. If the dev instance
   holds UDP 4242, the coordination instance logs a warning and runs
   without Reticulum LAN discovery (zenoh/iroh/pkarr are unaffected); set
   `HARMONY_RETICULUM_PORT=0` to silence the bind attempt entirely.

4. **Verify it's up** (pre-enrollment, the owner state is `null`):

   ```bash
   ~/harmony-pinned/harmony-app api --profile coord get_owner_state
   ```

5. **Enroll it as a device in your fleet** (the inviter side runs on your
   default-profile GUI — or its API when launched with `HARMONY_API_PORT`):

   ```bash
   # coordination instance (joiner):
   ~/harmony-pinned/harmony-app api --profile coord start_joiner_pairing \
     '{"displayName":"coord"}'
   # dev GUI: start inviter pairing from the device-pairing UI (or
   # `api start_inviter_pairing '{"displayName":"main"}'`), then on both
   # sides select the peer and compare the short auth string:
   ~/harmony-pinned/harmony-app api --profile coord get_pairing_state
   ~/harmony-pinned/harmony-app api --profile coord confirm_pairing_sas
   ```

6. **Verify** both devices appear in your fleet, then kill / rebuild /
   relaunch the dev instance freely — the coordination instance is
   unaffected.

A single GUI can also run on a named profile (onboarding tests against a
scratch profile): launch with `HARMONY_PROFILE=<name>` plus a passphrase
env. Two *simultaneous* GUIs remain unsupported (the single-instance
plugin is identifier-global), and `harmony://` deep links always reach the
default-profile GUI.
