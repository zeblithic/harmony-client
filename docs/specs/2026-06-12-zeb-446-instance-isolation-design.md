# ZEB-446: Side-by-side instance isolation — design

**Status:** approved by Jake 2026-06-12 (design session in Koya Claude Code transcript).
**Base:** main `95e54850` (includes the headless API PRs #238/#242, the encrypted-file
vault fallback #236, and the zenoh-zid leading-zero fix #243).

## Goal

Run two harmony-app instances on one machine — a pinned release build agents use as a
stable **coordination** instance, plus the **dev** build under test — without the two
clobbering each other. The coordination instance is a **separate enrolled device in the
owner's fleet** (settled 2026-06-11): honestly modeled as another always-on device,
which exercises the enrollment + butler/fleet paths for free.

## Settled decisions (2026-06-12 design session)

1. **Coordination instance form: headless `harmony-app serve`** on a named profile,
   driven via the headless API + `harmony-app api` CLI. No second GUI. This dissolves
   four collision points outright: the single-instance plugin (no Tauri runtime in
   serve mode), the OS-global `harmony://` deep-link registration, the shared webview
   profile/localStorage, and CDP port juggling.
2. **Profile selection: `--profile <name>`** (+ `HARMONY_PROFILE` env; flag wins).
   A named profile suffixes BOTH existing storage roots; the default profile is
   today's exact layout — zero migration.
3. **Vault: named profiles never touch the OS keychain.** They use the encrypted-file
   vault (Argon2id + XChaCha20-Poly1305, HRMI envelope — shipped for the consolidated
   vault in PR #236) under their own identity dir, unlocked via
   `HARMONY_PASSPHRASE`/`HARMONY_PASSPHRASE_FILE`.
4. **Reticulum UDP 4242: non-fatal bind + `HARMONY_RETICULUM_PORT` override**
   (`0` = disabled). Today a failed bind kills transport init; that also causes the
   ZEB-420/ZEB-165 integration-test port race.
5. **pkarr: the coordination instance publishes normally.** The owner identity record
   is owner-keyed and last-write-wins across devices; always-on first-contact
   reachability is load-bearing for the butler story. Fleet-net rows carry per-device
   routing after first contact. Follow-up only if LWW churn bites in practice
   (ZEB-392 vicinity).

## Ground truth: collision inventory (surveyed 2026-06-12)

Two storage roots exist, **both behind single chokepoints**:

| Root | Resolver | Default location | Holds |
|---|---|---|---|
| identity dir | `identity::resolve_path()` (identity.rs:2051) | `$HOME/.harmony/` | `identity.key` (vault file), `owner_state.cbor`, `communities/<cid>/channels/<ch>/`, replay trackers, CRDT logs |
| app data dir | `resolve_app_data_dir()` (lib.rs:274) | `dirs::data_dir()/net.zeblith.harmony/` | follows, vine feed, mail blobs, dm_inbox/outhold, fleet_net, notes, mint, connectivity settings, content index, `api/{port,token,serve.lock}` |

A third small site computes the log dir independently (`app_tracing.rs:26`,
`dirs::data_dir()/net.zeblith.harmony/logs/`).

Collision points and their dispositions:

| # | Collision | Disposition |
|---|---|---|
| 1 | Single-instance plugin keys on the hardcoded app identifier `net.zeblith.harmony` (lib.rs:41704); not scopeable | **Dissolved** — serve mode runs no Tauri plugins. Concurrent dual-GUI remains impossible and is a documented non-goal. |
| 2 | Both storage roots hardcoded; no env/flag override anywhere | **Fixed** — profile resolution (§Design 1). |
| 3 | Keychain vault names fixed machine-wide (`harmony`/`identity`, identity.rs:1758) → two instances clobber each other's vaults (ZEB-428 hazard class) | **Fixed** — named profiles are file-vault-only (§Design 2). |
| 4 | Reticulum UDP 4242 bind is fatal on failure (event_loop.rs:844-863, `ready_tx.send(Err)`) | **Fixed** — non-fatal degrade + env override (§Design 3). |
| 5 | zenoh: deterministic zid per device identity, ephemeral listeners, default multicast scouting | **No change needed** — distinct device keys → distinct zids; the two local instances scout and interconnect, which is desirable. |
| 6 | iroh: ephemeral ports, per-profile secret keys | **No change needed.** |
| 7 | pkarr owner identity record: owner-keyed, LWW across devices (pkarr_identity_publisher.rs:40-63) | **Documented semantics, no change** (settled decision 5). |
| 8 | Voice/DM presence: beacons carry the 32-byte enrolled device key; rosters disambiguate `(owner, device)` | **No change needed.** |
| 9 | API discovery + lock files live under the app data dir (api/lock.rs:18, api/mod.rs:214-226) | **Free** — per-profile isolation falls out of profile resolution. |
| 10 | `harmony://` deep links route to the default GUI only | **Documented caveat** — coordination instance redeems invites via API/CLI, no deep links needed. |
| 11 | CDP `:9222` | Not in this codebase (WebView2 launch env on Windows); recipe note only. |

Enrollment ground truth: device #2 needs **no quorum** — an inviter holding the master
seed signs the `EnrollmentCert` directly (`pairing_commands.rs`; quorum enforcement in
`enroll_quorum.rs` applies only when no master key is available). The joiner side
generates a fresh `SigningKey` in-memory (`pairing_commands.rs:42`) and needs no
keychain at pairing start; post-pairing persistence flows through the vault seams.
Pairing requires a started node (`pairing_handle` lives in `NodeState`), which serve
mode satisfies by auto-starting. The 6 pairing IPCs are **not** currently in the
headless RPC registry (api/rpc.rs).

## Design

### 1. Profile resolution

New `src-tauri/src/profile.rs`:

- `set_active_profile(name: &str) -> Result<(), String>` — called by CLI subcommand
  handlers in `main.rs` **before** tracing init or any path resolution. Errors if a
  different profile was already activated.
- `active_profile() -> Option<&'static str>` — `OnceLock`-backed; lazily initialized
  from `HARMONY_PROFILE` on first read if the setter was never called (this is how GUI
  launches pick up a profile). `None` = default profile.
- Validation: names match `^[a-z0-9][a-z0-9_-]{0,31}$`; the literal name `default` is
  rejected with guidance (the default profile is selected by omitting the flag/env).
  Invalid `HARMONY_PROFILE` values are a **hard error** at activation, not a silent
  fallback — a wrong profile silently landing in the default profile's data is the
  worst outcome. GUI launches validate eagerly: `run()` calls `set_active_profile`
  from the env at entry (before any path use) and aborts the launch with the
  validation error rather than deferring to a lazy first read.

Path mapping (named profile `<p>`):

| Root | Default | Named |
|---|---|---|
| identity dir | `$HOME/.harmony/` | `$HOME/.harmony/profiles/<p>/` |
| app data dir | `…/net.zeblith.harmony/` | `…/net.zeblith.harmony/profiles/<p>/` |
| logs | `…/net.zeblith.harmony/logs/` | `…/net.zeblith.harmony/profiles/<p>/logs/` |

Implementation: `identity::resolve_path()` and `resolve_app_data_dir()` consult
`active_profile()`; `app_tracing` switches from its own path computation to the
profile-aware app-data resolver. A sweep task in the plan audits for any other
independent path computations (none are known beyond these three).

Surfaces gaining `--profile`: `serve`, `api`, `rotate-passphrase`, `export *`,
`restore *`. The GUI honors `HARMONY_PROFILE` (storage only; concurrent dual-GUI
stays impossible regardless — see non-goals).

### 2. Vault routing for named profiles

`KeychainStore::new()`'s constructor gate (ZEB-428) gains a third refusal condition:
`active_profile().is_some()`. All ambient construction sites — including
`start_inviter_pairing`'s `KeychainStore::new().ok()` — then fall back to the
encrypted-file store exactly as test builds and Linux CI do today. The vault file
lands at `<profile identity dir>/identity.key` automatically because
`identity::resolve_path()` is profile-aware.

Fail-fast guard: on a named profile, if neither `HARMONY_PASSPHRASE` nor
`HARMONY_PASSPHRASE_FILE` is set, refuse at startup with explicit guidance — `serve`
exits non-zero; a GUI launch surfaces the error and refuses to start the node. No
ZEB-450-style silent transport loss.

Default-profile behavior is byte-for-byte unchanged.

### 3. Reticulum UDP (4242)

- `HARMONY_RETICULUM_PORT`: `u16`; `0` = skip the bind entirely (no Reticulum LAN
  discovery this session); unset = 4242. An unparseable value **warns loudly and uses
  the default 4242** — Reticulum is default-on, so a bad override must not silently
  change behavior (contrast `parse_api_port`, where the feature is opt-in and
  disabling loudly is correct).
- Bind failure (any port) degrades to `tracing::warn!` + continue: transport init
  returns Ok, the Reticulum broadcast/receive tasks are simply not spawned, and
  zenoh/iroh/pkarr proceed unaffected. The two local instances still interconnect via
  zenoh scouting.
- Integration tests set `HARMONY_RETICULUM_PORT=0`, retiring the ZEB-420/ZEB-165
  fixed-port race class.

### 4. Pairing over the headless API

Add the 6 pairing commands to the RPC registry (same `rpc!` adaptation pattern as the
existing 29):

| Command | Args | Returns |
|---|---|---|
| `start_inviter_pairing` | `{"displayName": string}` | null |
| `start_joiner_pairing` | `{"displayName": string}` | null |
| `select_pairing_peer` | `{"peerSessionId": uuid-string}` | null |
| `confirm_pairing_sas` | none | null |
| `cancel_pairing` | none | null |
| `get_pairing_state` | none | `PairingState` JSON |

SAS verification flows through `get_pairing_state` polling on both sides (the human or
agent compares the short auth strings out-of-band, as the GUI flow does). If the
pairing state machine emits events through the `NodeEventSink` seam they appear on the
WS firehose for free; polling is the v1 contract either way.

### 5. Enrollment + operation recipe (docs deliverable)

Documented in `docs/headless-install.md` (new "Side-by-side coordination instance"
section) + a pointer from `docs/troubleshooting.md`:

1. Pin a release binary (copy it out of the build tree so dev rebuilds can't touch it).
2. Create a passphrase file: `umask 077; echo '<passphrase>' > ~/.harmony-coord-pass`.
3. Start: `harmony-app-pinned serve --profile coord --api-port 7421` with
   `HARMONY_PASSPHRASE_FILE=~/.harmony-coord-pass` (and optionally
   `HARMONY_RETICULUM_PORT=0`; without it the second instance just logs a warn when
   4242 is taken and continues degraded).
4. Verify: `harmony-app-pinned api --profile coord get_owner_state` → `null` pre-enroll.
5. Enroll: joiner on the coordination instance
   (`api --profile coord start_joiner_pairing '{"displayName":"koya-coord"}'`),
   inviter on the dev GUI (UI flow, or its own API when launched with
   `HARMONY_API_PORT`), `select_pairing_peer` on both, compare SAS via
   `get_pairing_state`, `confirm_pairing_sas` on both.
6. Verify both devices appear in the fleet; kill/rebuild/relaunch the dev instance
   freely — the coordination instance is unaffected.

### Error handling summary

| Condition | Behavior |
|---|---|
| Invalid profile name (flag or env) | Hard error at startup with the validation rule spelled out |
| Named profile without passphrase env | Refuse to start (serve: exit non-zero; GUI: surfaced error, node not started) |
| Reticulum port taken / unbindable | Warn + continue without Reticulum |
| `HARMONY_RETICULUM_PORT` unparseable | Warn + default 4242 |
| Second serve on the SAME profile | Existing `serve.lock` refusal, now naturally per-profile |
| Pairing RPC before node start | Existing "pairing not initialized — start node first" error string, verbatim over the API |

## Non-goals / documented caveats (v1)

- **Concurrent dual-GUI** on one machine: the single-instance plugin is
  identifier-global and stays untouched. A *single* GUI on a named profile (e.g.
  onboarding testing against a scratch profile) works via `HARMONY_PROFILE`.
- **Deep links** (`harmony://`) reach the default-profile GUI only.
- **Webview profile redirection, CDP code changes**: none (CDP stays a WebView2
  launch-env concern on Windows; recipe note only).
- **pkarr per-device records / LWW churn mitigation**: deferred until observed to
  matter (ZEB-392 vicinity).
- **Default-profile layout changes**: none, including the `~/.harmony/identity.key`
  location.

## Testing

**Unit** (in-module):
- Profile name validation matrix (valid, invalid chars, length, `default` reserved,
  flag-vs-env precedence, double-activation error).
- Path mapping matrix for both roots, default vs named.
- `HARMONY_RETICULUM_PORT` parse (unset → 4242, `0` → disabled, garbage → warn+4242).
- Keychain constructor gate refuses when a named profile is active.

**Integration** (new `--test profile_isolation`, temp-HOME + keychain-hermetic per
ZEB-428 rules, `HARMONY_PASSPHRASE` set):
- Serve-core boot on a named profile: both roots created under `profiles/<name>/`,
  API discovery files under the profile's app-data dir, default-profile dirs NOT
  created.
- Occupied Reticulum port: pre-bind a socket, boot pointing at it → boot succeeds
  (transport ready Ok), warn logged.
- `HARMONY_RETICULUM_PORT=0` → boot succeeds without the socket.
- Pairing RPC registry: the 6 commands dispatch (non-404), `get_pairing_state`
  pre-node returns the documented error; full two-sided pairing state-machine
  exercise reuses the `tests/pairing_integration.rs` in-memory-broker patterns where
  feasible.

**Live two-instance E2E** (machine errand, not CI): run the §5 recipe on Koya —
pinned serve on `coord` + dev GUI, real pairing, both devices in fleet, dev-instance
kill/rebuild/relaunch. Ildwyn/AVALON repeat on Windows. This is the ticket DoD's
"both instances online simultaneously as distinct enrolled devices".

## Definition of done mapping

| Ticket DoD | Delivered by |
|---|---|
| Documented flags + recipe on Ildwyn/AVALON/Koya | §1 flags + §5 recipe docs |
| Both instances online as distinct enrolled devices | §4 pairing-over-API + live errand |
| Dev instance killable/rebuildable without disturbing coordination instance | Profile isolation (§1-§3) + live errand |

One PR in harmony-client. PR body references only this ticket.
