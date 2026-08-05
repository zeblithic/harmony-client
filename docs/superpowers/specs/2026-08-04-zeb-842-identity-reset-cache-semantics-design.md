# ZEB-842 — Identity-reset cache semantics: two-tier reset + erase-all — Design

**Status:** approved (brainstorm 2026-08-04) — ready for implementation plan.
**Ticket:** ZEB-842 (Medium, `harmony-client`). Follow-up to ZEB-835/836 (the reset escape hatch), ZEB-841 §5 (retention documented as intentional-for-now), ZEB-586 (per-profile isolation), ZEB-796 (re-minted identity inherits prior state).

## Problem

`reset_local_identity` ("Reset this device & start fresh", behind the boot-failure modal) operates on the **identity dir** only. It snapshots-and-moves the `OWNER_RESET_FILES` manifest out of `~/.harmony` and best-effort clears the keychain. It touches **nothing** under the separate per-profile **app-data dir**, where every content cache lives — including the private, delivered-once `mail/` DM store. So after a "start fresh," private message content (and every other cache) remains on disk, and the copy ("start fresh") can be read as a clean slate it does not deliver.

Verified against source 2026-08-04 (see Appendix A for the full inventory). Two facts sharpen the fix:

1. **`mail/` is the sharp edge** — private, *not* re-fetchable, stored at a **shared (non-owner-keyed) path** (`app_data_dir/mail`). A same-profile reset + re-mint therefore lets a *new* identity inherit the *old* identity's private DM cache (the ZEB-586 owner-agnostic-cache class; adjacent to ZEB-796). Documenting retention does not remove that residue.
2. **The app-data dir is not uniformly "content caches."** It mixes re-fetchable caches (`avatars/`, `content-index.json`, `follows.json`, card store, `vine_pull.cbor`), private content (`mail/`), device config (`connectivity-settings.json`), and owner-scoped economic state (`mint/`, `storage_*.json`, `last_backup.json`). A holistic fix must be deliberate about the whole subtree, not a per-cache carve-out.

## Decision

**Two-tier model** (approved):

- **Recovery reset** (`reset_local_identity`, existing) stays a *minimal un-brick tool*: snapshot-then-move the identity, keep the app-data caches, and — the new part — **fix its copy** so it no longer implies a data wipe and points to the erase-all action for a full wipe.
- **Erase-all** (`erase_all_local_data`, **new**) is the real clean-slate: a **typed-confirm**, irreversible, hard-delete of the active profile's identity *and* app-data subtrees. Reachable from **both** Settings → Account (deliberate case) and the boot-failure modal (bricked case); both surfaces call the one command.

This matches our severe-action-confirmation tiering: the reversible-ish recovery reset keeps its checkbox; the irreversible erase-all requires typed confirmation. The identity's safety net under erase-all is the **recovery phrase** (reminded in the dialog), not an on-disk backup.

Rejected alternatives: *recovery-only + document* (leaves private `mail/` residue and the cross-identity inheritance leak, merely warning about them); *single reset that also scrubs* (conflates un-bricking with wiping, destroying non-re-fetchable `mail/` as a side effect of recovery — the footgun the two-tier model exists to avoid).

---

## Architecture

### Component 1 — `erase_all_local_data` backend command

A **second, distinct** Tauri command, not a flag on `reset_local_identity`. The two have opposite postures and must not share the snapshot path:

| | `reset_local_identity` (recovery, exists) | `erase_all_local_data` (new) |
|---|---|---|
| Purpose | un-brick a boot failure | wipe this machine's data for this profile |
| Identity | **snapshot**-then-move (dev-recoverable) | **hard-delete**, no snapshot |
| App-data caches | untouched | **hard-delete** |
| Confirm tier | checkbox | **typed-confirm** |
| Safety net | the `_reset-backup-*` folder | the **recovery phrase** |

Erase-all **hard-deletes** (never snapshots): snapshotting `mail/` into a backup dir would leave the private content on disk, defeating the point. The identity is hard-deleted too — the recovery phrase is the backup, consistent with our self-sovereign recovery model.

**Scrub mechanism — wholesale subtree delete, minus exclusions (chosen over an allowlist manifest).**
Delete every child entry under the active profile's `identity_dir` **and** `app_data_dir`, excluding exactly two subdirectory names in each:

- **`profiles/`** — holds sibling identities/profiles. The per-profile isolation invariant (ZEB-586): erase-all from profile X must never touch profile Y.
- **`logs/`** — the live tracing sink (`app_data_dir/logs`). The GUI's post-action `reload()` reloads the **webview only**; the Rust process and its tracing subscriber keep running and holding the log file open, so deleting `logs/` merely races the appender and it reappears. Logs are diagnostic, not user content. (A complete log wipe would need a full process restart — out of scope.)

Rationale vs. an explicit allowlist (like the existing `OWNER_RESET_FILES`): a manifest **drifts** — every cache added later silently escapes the wipe until a human extends the list. That drift is exactly what created this ticket (`avatars/` was added; reset never learned of it). "Erase all" should mean *all*; a manifest is a standing liability for a privacy action. The two exclusions are structural (sibling isolation, live-sink), not per-cache carve-outs.

**The `profiles/` trap (why this is "delete children except X," not `remove_dir_all(dir)`):**

- *Named* profile: `identity_dir` = `~/.harmony/profiles/<name>/`, `app_data_dir` = `…/net.zeblith.harmony/profiles/<name>/`. Self-contained subtrees; wiping the whole dir is safe. (`profiles/` won't appear inside them, so the exclusion is a no-op here; `logs/` still applies.)
- *Default* profile: `identity_dir` = `~/.harmony/`, `app_data_dir` = `…/net.zeblith.harmony/`. Both **contain** `profiles/` (sibling named identities/profiles). A `remove_dir_all` of the dir itself would destroy siblings. So the operation iterates the dir's entries and deletes each **except** `profiles/` and `logs/`.

This is uniform across both cases: "for each child entry of `dir`, if its file name is not `profiles/` or `logs/`, remove it (recursively for dirs)." A shared helper `remove_dir_children_except(dir, &excluded)` implements it.

**Reused machinery (mirrors `reset_local_identity`):**
- Stop the node first (`crate::stop_inner(state, None)`) so no engine (liveness refresher, mail persist, fleet sync) rewrites a cache into the gap.
- Run the blocking filesystem work in `run_blocking`.
- Hold `OWNER_STATE_WRITE_LOCK` and wrap the identity-dir portion in `crate::identity::with_identity_dir_write_guard` (cross-process exclusion), same as the reset.
- Best-effort clear the OS keychain owner secrets (`prod_keychain().delete_all()`); a keychain-clear failure logs a warning but does **not** fail the wipe (the on-disk removal is authoritative — mirrors reset).
- **Best-effort per-entry deletion:** a single un-removable entry (e.g. a Windows-locked file) is logged and skipped; the wipe continues rather than aborting half-done. The command returns `Ok(())` on completion (or a lightweight summary — see Interfaces); it fails hard only on the cross-process write-guard contention (another harmony process is mid-write), matching reset.

**Testable seam (ZEB-428):** `pub(crate) fn erase_all_local_data_inner(identity_dir: &Path, app_data_dir: &Path, keychain: Option<KeychainStore>) -> Result<(), String>`. Production passes `prod_keychain()`; tests inject `None` or a mock and never construct `KeychainStore::new()` in test-reachable code. The Tauri command resolves `resolve_identity_dir()` + `resolve_app_data_dir()`, stops the node, and calls `_inner` inside `run_blocking`.

### Component 2 — frontend surfaces (two, one command)

The typed-confirm word is the fixed literal **`ERASE`** (uppercase), identical on **both** surfaces — a fixed word, not the owner-id prefix IdentityPanel's restore uses, so the erase-all gate reads the same everywhere and needs no per-identity data to render. The submit stays disabled until the input exactly equals `ERASE`.

**(a) `StartupRecoveryOptions.svelte`** (boot-failure modal). Add a third remedy below the existing recovery reset: **"Erase all local data."** Because it is irreversible, it uses the **typed-confirm** (`ERASE`) — not the recovery reset's checkbox. On confirmed submit → `invoke('erase_all_local_data')` → `reload()`. `invoke`/`reload` stay injectable (existing pattern) so the flow is unit-testable without Tauri.

**(b) `IdentityPanel.svelte`** (Settings → Account, beside Backup/Restore). Add the same **"Erase all local data"** action in a danger zone, gated by the same typed-confirm (`ERASE`) — reusing the component's existing typed-confirm *machinery* while pinning the required text to the fixed word rather than the restore flow's owner-id prefix. Same command, and on success it reloads into first-run onboarding like the boot-modal path.

**(c) Recovery-reset copy fix** (`StartupRecoveryOptions.svelte`). The current checkbox copy ("Start fresh on this device. Your current identity is backed up to a folder on this device first. You'll lose access to communities you joined here unless you have your recovery phrase. This can't be undone from the app.") is silent on cached content. Tighten it to state explicitly that **cached content (messages, avatars, etc.) stays on this device** after a recovery reset, and point to "Erase all local data" for a full wipe. This closes the "misleading copy" half of the ticket.

Both components use design tokens only (`var(--…)`) per the ZEB-605 style-token-guard test — no raw color literals.

### Data flow

```
User (bricked OR Settings)
  → clicks "Erase all local data"
  → typed-confirm gate (types ERASE)
  → invoke('erase_all_local_data')
        → stop_inner(node)                       // engines quiesced
        → run_blocking:
            with_identity_dir_write_guard(identity_dir):
              remove_dir_children_except(identity_dir, {profiles/, logs/})
            remove_dir_children_except(app_data_dir, {profiles/, logs/})
            keychain.delete_all()                // best-effort
  → reload()  → next boot: owner_state.cbor gone → classifies `missing` → first-run onboarding
```

### Error handling

- **Write-guard contention** (another harmony process mid-write on the same identity dir): fail fast with the existing guard error; nothing deleted. Same as reset.
- **Per-entry removal failure:** logged (`tracing::warn!` with the path), skipped; wipe continues. Not surfaced as a command failure — a partial wipe still succeeds at its goal for every removable entry, and the alternative (abort on first locked file) leaves *more* residue.
- **Keychain-clear failure:** `tracing::warn!` per item, non-fatal (the on-disk removal is the authoritative onboarding gate).
- **Frontend:** an `invoke` rejection surfaces in the confirm block (mirrors reset's `resetError`) without navigating away; the user can retry.

---

## Testing

### Backend (`erase_all_local_data_inner`, tempdir `identity_dir` + `app_data_dir`, injected keychain)

1. **Full wipe.** Populate identity files (`owner_state.cbor`, `master_seed.enc`, …) and app-data caches (`mail/` with a blob, `avatars/`, `follows.json`, `content-index.json`, `profile_cards.<id>.cbor`, `mint/`, `storage_records.json`, `storage_ledger.json`, `connectivity-settings.json`, `vine_pull.cbor`). Erase → assert **all** removed; mock keychain's `delete_all` was called.
2. **Per-profile isolation (ZEB-586 regression guard).** Under the same platform root, plant a sibling `profiles/<other>/…` subtree (both under identity root and app-data root). Erase the **default** profile → assert `profiles/` and its contents are **untouched**; erase a **named** profile → assert a *different* named sibling under `profiles/` is untouched.
3. **`logs/` exclusion.** Plant `app_data_dir/logs/app.log` → erase → assert `logs/` survives (diagnostic sink), while its siblings are gone.
4. **No-op safety.** Erase on empty identity + app-data dirs → clean `Ok(())`; no panic, nothing created.
5. **Best-effort tolerance** (where portable): a residual entry that cannot be removed does not abort the others. (If a locked-file simulation isn't portable, assert instead that a pre-existing unrelated *excluded* dir plus removable siblings yields all-siblings-gone.)

### Frontend

6. **`StartupRecoveryOptions`:** typed-confirm gates erase-all — wrong text leaves the submit disabled; correct text → `invoke('erase_all_local_data')` then `reload()`; an `invoke` rejection shows an error and does not reload. Recovery reset's existing behavior is unchanged.
7. **`IdentityPanel`:** the danger-zone erase-all action's typed-prefix confirm → `invoke('erase_all_local_data')`; wrong prefix disabled; error surfaces.
8. **Recovery-reset copy:** assert the new retention-honest wording renders in the reset-confirm block.

### Gates

Rust: `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo fmt --all -- --check` (from `src-tauri/`). Frontend: `npx tsc --noEmit`, `npx vitest run` (from repo root).

---

## Scope boundaries (YAGNI)

- **Headless/`api` exposure of erase-all is out of scope.** Erase-all is a user-facing destructive GUI action; the fleet/e2e tooling has no need to wipe a live node's data via IPC. If a future need arises, the `_inner` seam is already headless-callable.
- **No change to `reset_local_identity`'s behavior** beyond its copy — it keeps snapshotting the identity and retaining caches. Only the wording changes.
- **No new cache is added, moved, or made owner-keyed here.** Re-keying `mail/` to be owner-scoped (which would also fix the cross-identity inheritance leak for the *recovery* path) is a larger, separate change; erase-all addresses the residue holistically without it. Note the residual for a possible follow-up.
- **Logs are retained by design** (live sink); not a security regression — they are diagnostic, and a full log wipe needs a process restart the webview `reload()` doesn't provide.

---

## Appendix A — verified app-data inventory (per-profile `…/net.zeblith.harmony[/profiles/<name>]`)

| Item | Path | Category | Re-fetchable | Owner-keyed |
|---|---|---|---|---|
| `mail/` | `mail/` | private DM content | ❌ delivered-once | ❌ shared path |
| `avatars/` | `avatars/` | public image bytes | ✅ content-addressed | ❌ shared |
| `content-index.json` | file | peer CIDs / names | ✅ self-healing | ❌ shared |
| `follows.json` | file | follow graph | ✅ from network | ❌ shared |
| card store | `profile_cards.<ownerId>.cbor` | peer display names | ✅ self-healing | ✅ owner-keyed |
| `vine_pull.cbor` | file | vine pull progress | ✅ regenerable | ❌ shared |
| `mint/` | `mint/` | minted-content state | — | ❌ shared |
| `storage_records.json` / `storage_ledger.json` | files | storage economics | — | ❌ shared |
| `connectivity-settings.json` | file | device network prefs (identity-agnostic) | — | ❌ shared |
| `last_backup.json` | file | backup timestamp | — | ❌ shared |
| `logs/` | `logs/` | diagnostic logs | — | ❌ shared — **excluded from wipe** |
| `profiles/<other>/` | subtree | sibling profiles/identities | — | — **excluded from wipe** |

## Appendix B — source references (verified 2026-08-04)

- `owner_commands.rs:1833` `OWNER_RESET_FILES`; `:1895` `reset_local_identity`; `:1916` `reset_local_identity_inner` (snapshot posture, `OWNER_STATE_WRITE_LOCK`, `with_identity_dir_write_guard`, keychain clear); `:625` `resolve_identity_dir` (→ `identity::resolve_path(None).parent()`).
- `identity.rs:2719` `resolve_path` → `identity_path_in` (profile-aware: named → `~/.harmony/profiles/<name>`).
- `lib.rs:380` `resolve_app_data_dir` → `:414` `app_data_dir_in` (profile-aware: named → `…/net.zeblith.harmony/profiles/<name>`); cache joins at `:3736`/`:3739` (storage), `:4217`/`12118` (`mail/`), `:4227` (`avatars/`), `:5040`/`9392` (connectivity), `:5571`/`20866` (`mint/`), `:11319` (`vine_pull.cbor`), `:51838` (`last_backup.json`); `follows.rs:19` `FOLLOWS_FILE`, `content_index.rs:68` `INDEX_FILE`, `persistent_card_store.rs:137` `path_for_owner`.
- `app_tracing.rs:21` `log_dir_in` → `app_data_dir_in(base, profile).join("logs")`.
- `StartupRecoveryOptions.svelte` (boot-modal recovery reset: checkbox confirm → `invoke('reset_local_identity')` → `reload()`; `invoke`/`reload` injectable). `IdentityPanel.svelte` (Settings → Account; existing typed-prefix restore-confirm pattern).
