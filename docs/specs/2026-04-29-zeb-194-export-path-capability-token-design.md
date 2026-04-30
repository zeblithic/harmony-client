# ZEB-194 — Capability-Token for Renderer-Supplied Export Paths

**Status:** Design accepted 2026-04-29

**Goal:** Close the IPC trust gap where `export_owner_recovery_file_to_path` and `export_recovery_file_to_path` accept a renderer-supplied write path, by inverting the dialog flow — the backend opens the OS save dialog, caches the user-confirmed path under an opaque UUID, and the export commit consumes that token in lieu of a raw path.

**Background:** CodeRabbit round 4 finding on PR #62 (ZEB-170). The two `*_to_path` Tauri commands accept a path string from the renderer and call `crate::identity::write_atomic_0600` against it. The normal UI flow goes through `@tauri-apps/plugin-dialog::save` (a JS dialog) which returns a user-confirmed path, but a compromised renderer can invoke either IPC directly with any path the Tauri process can write to. The same pattern applies to other Tauri save-to-arbitrary-path commands across the codebase; this issue is the first deployment of the fix.

**Scope:** Two existing IPCs:

1. `owner_commands::export_owner_recovery_file_to_path` — master-seed recovery file (the ZEB-194 nominal target).
2. `identity_commands::export_recovery_file_to_path` — per-device transport-identity recovery file (sibling primitive, same vulnerability).

Both are migrated together in this PR. Bundling avoids a second round of IPC plumbing and a security fix that leaves a parallel hole open.

## Threat model

This is the standard Tauri trust boundary: a fully-compromised renderer can broadly compromise the user regardless. The export-to-path commands are particularly clean primitives for *"write arbitrary content to an arbitrary file under the user's account"*, so they are worth hardening within the model — making the IPC surface refuse arbitrary paths means a renderer that gets RCE on the JS side cannot freely point the writer at `~/.ssh/authorized_keys`, the user's shell rc, or other sensitive targets.

After the change, the renderer never names a write path. It receives an opaque UUID from the dialog command, then ferries that UUID into the export commit. A renderer that captures or replays a path token can only write to the path the user *already confirmed* in the OS dialog — i.e., the legitimate target. That is not a privilege gain over the un-attacked flow.

**Out of scope:**
- Defeating a fully-compromised renderer that drives the OS dialog programmatically. Not in Tauri's threat model.
- Renderer-supplied dialog hints (default filename / file filter): the user explicitly confirms the final path in the OS dialog and can rename / redirect freely. Default filename is a UX hint, not a security control.
- *Other* arbitrary-path write commands across the codebase. The ZEB-194 issue notes this is the right pattern for a Tauri-wide convention; this PR establishes the convention. Future commands can adopt the same pattern.

## Architecture

A second token cache (`PATH_TOKEN_CACHE`) sibling to the existing `TOKEN_CACHE` in `owner_state.rs`, plus one IPC that owns the OS save dialog server-side. Two cache types because they hold semantically different things:

- `TOKEN_CACHE`: `Zeroizing<[u8; 32]>` — master-seed bytes; needs zeroize-on-drop; lifetime tied to the mint→export window.
- `PATH_TOKEN_CACHE`: `PathBuf` — non-secret data; lifetime tied to the dialog→commit window.

Conflating them invites runtime "wrong token type" errors that should be type-level. The two caches are independent, both 5-minute TTL, both 8-entry LRU-capped, both panic-recovering.

The new `request_export_save_path` IPC is generic over export type. The renderer supplies dialog hints (`default_filename`, `filter_name`, `filter_extensions`), which are UX-only. The IPC opens the OS dialog server-side, caches the user-confirmed path on `Some(_)`, and returns the token. On user-cancel returns `Ok(None)`; on dialog-plugin error returns `Err(String)`.

## Files & responsibilities

### `src-tauri/src/owner_state.rs` (modified)

Adds, alongside the existing `TOKEN_CACHE`:

```rust
const PATH_TOKEN_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_LIVE_PATH_TOKENS: usize = 8;

struct PathTokenEntry {
    path: PathBuf,
    inserted_at: Instant,
}

static PATH_TOKEN_CACHE: LazyLock<Mutex<HashMap<Uuid, PathTokenEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn path_token_cache_lock() -> std::sync::MutexGuard<'static, HashMap<Uuid, PathTokenEntry>> { … }
pub fn insert_path_token(path: PathBuf) -> Uuid { … }
pub fn take_path_token(token: &Uuid) -> Option<PathBuf> { … }
fn evict_expired_paths(cache: &mut HashMap<Uuid, PathTokenEntry>) { … }
#[doc(hidden)] #[cfg(test)] pub(crate) fn clear_path_token_cache() { … }
```

Mirrors the existing seed-token API exactly. Same panic-recovery, same TTL, same LRU shape.

### `src-tauri/src/save_dialog.rs` (new)

Single new IPC plus its dependencies:

```rust
use tauri_plugin_dialog::DialogExt;

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportPathRequest {
    pub title: Option<String>,
    pub default_filename: String,
    pub filter_name: String,
    pub filter_extensions: Vec<String>,
}

#[tauri::command]
pub async fn request_export_save_path(
    app: tauri::AppHandle,
    request: ExportPathRequest,
) -> Result<Option<String>, String> {
    // Wraps dialog plugin's blocking save-file API; on Some(path) inserts
    // into PATH_TOKEN_CACHE and returns Ok(Some(uuid_string)). On user
    // cancel returns Ok(None). On plugin error returns Err.
    //
    // Uses run_blocking — blocking_save_file blocks a thread.
}
```

`title` is optional because the underlying `save({...})` call sites differ: DevicesPanel currently omits a title; IdentityPanel sets `'Save recovery file'`. Preserving that asymmetry keeps the visual diff to platform conventions only.

Lives in its own module because it's a *pattern* the rest of the codebase will eventually adopt for other arbitrary-path writers; centralizing it makes that adoption mechanical (one import, one IPC).

### `src-tauri/src/owner_commands.rs::export_owner_recovery_file_to_path` (modified)

Signature change: `path: String` → `path_token: String`. Inside `run_blocking`, take the path token *first* (before consuming the recovery_token), then proceed with current logic.

```rust
#[tauri::command]
pub async fn export_owner_recovery_file_to_path(
    recovery_token: String,
    path_token: String,
    passphrase: String,
    comment: Option<String>,
) -> Result<ExportInfo, String> {
    // … existing passphrase + comment validation …
    let recovery_uuid: Uuid = recovery_token.parse().map_err(…)?;
    let path_uuid: Uuid = path_token.parse().map_err(…)?;
    run_blocking(move || {
        // Consume path_token first so a downstream seed-token failure
        // doesn't leave a path token live in the cache pointing at the
        // user's chosen file.
        let out = take_path_token(&path_uuid)
            .ok_or_else(|| "Save path token expired or invalid. Please re-trigger backup.".to_string())?;
        let seed = take_token(&recovery_uuid).ok_or_else(|| "Recovery token expired or invalid. …".to_string())?;
        // … existing encrypt + write_atomic_0600 …
        Ok(ExportInfo { identity_hash: …, byte_len: …, path: out.display().to_string() })
    }).await
}
```

`ExportInfo` gains a `path: String` field so the renderer can display the actual save location without re-stating what it requested.

### `src-tauri/src/identity_commands.rs::export_recovery_file_to_path` (modified)

Signature change: `out_path: PathBuf` → `path_token: String`. The per-device transport identity is resolved from disk (no seed-token), so only the path token is consumed. Returns `String` (the path written to) so the renderer — which no longer knows the path — can keep its existing "saved to X" feedback.

```rust
#[tauri::command]
pub async fn export_recovery_file_to_path(
    path_token: String,
    passphrase: String,
    comment: Option<String>,
) -> Result<String, String> {
    let plaintext_path = identity::resolve_path(None)?;
    let path_uuid: Uuid = path_token.parse().map_err(…)?;
    run_blocking(move || {
        let out_path = take_path_token(&path_uuid)
            .ok_or_else(|| "Save path token expired or invalid. Please re-trigger backup.".to_string())?;
        export_recovery_file_to_path_helper(&plaintext_path, &out_path, &passphrase, comment, KeychainStore::new().ok())?;
        Ok(out_path.display().to_string())
    }).await
}
```

### `src-tauri/src/lib.rs` (modified)

Register the new command in `invoke_handler!`:

```rust
save_dialog::request_export_save_path,
```

Add `mod save_dialog;` near the other module declarations.

### `src/lib/owner-service.ts` (modified)

The `requestExportSavePath` helper lives here even though it is also used by IdentityPanel (which historically invokes identity commands directly). The dialog wrapper is generic UX scaffolding, not an owner-vs-identity dispatch concern, so a single shared helper keeps both call sites in sync.

```ts
async exportRecoveryFile(
  recoveryToken: string,
  pathToken: string,        // was: path: string
  passphrase: string,
  comment: string | null,
): Promise<ExportInfo> { … }

export interface ExportSavePathRequest {
  title?: string;
  defaultFilename: string;
  filterName: string;
  filterExtensions: string[];
}

async requestExportSavePath(req: ExportSavePathRequest): Promise<string | null> {
  return invoke('request_export_save_path', { request: req });
}
```

`ExportInfo` TS type gains `path: string`.

### `src/lib/components/DevicesPanel.svelte::commitBackup` (modified)

Replace the `await save({...})` block with:

```ts
let pathToken: string | null;
try {
  pathToken = await svc.requestExportSavePath({
    defaultFilename: 'owner-recovery.bin',
    filterName: 'Recovery file',
    filterExtensions: ['bin'],
  });
} catch (e) {
  backupError = extractError(e);
  return;
}
if (pathToken === null) return;  // user cancelled

backupInFlight = true;
try {
  const info = await svc.exportRecoveryFile(
    recoveryToken,
    pathToken,
    backupPassphrase,
    trimmedComment ? trimmedComment : null,
  );
  backupSavedPath = info.path;
} catch (e) { … }
finally { … }
```

The `@tauri-apps/plugin-dialog::save` import is removed from this component.

### `src/lib/components/IdentityPanel.svelte` (modified)

Same refactor pattern in the file-backup wizard, with two preservation requirements:

```ts
let pathToken: string | null;
try {
  pathToken = await svc.requestExportSavePath({
    title: 'Save recovery file',
    defaultFilename: 'identity.recovery',
    filterName: 'Recovery file',
    filterExtensions: ['recovery'],
  });
} catch {
  return;  // current behavior: silent return to fileEntry on dialog error
}

if (wizardState !== epoch) return;  // wizard cancelled while dialog was open
if (pathToken === null) return;     // user cancelled dialog

// … rest of the wizard's existing flow, but invoke with pathToken:
await invoke('export_recovery_file_to_path', { pathToken, passphrase, comment: comment || null });
```

Two existing behaviors that must survive the refactor:
1. **The `epoch` guard** — IdentityPanel's wizard tracks an `epoch` to detect "user cancelled the wizard while the dialog was open." That guard runs after the dialog returns (today: after `save()`; after the change: after `requestExportSavePath`). Same semantics, same placement.
2. **Silent-return on dialog error** — current behavior `catch { return; }` (no error surfaced) is preserved.

The `@tauri-apps/plugin-dialog::save` import is removed from this component.

## Data flow

**Master-seed backup (after change):**

```
Frontend                          Backend
────────                          ───────
openBackup()
  → issue_owner_recovery_token  → insert_token(seed) → recovery_token
  ← recovery_token

[user fills passphrase/comment, clicks Save]
commitBackup()
  → request_export_save_path     → dialog.blocking_save_file()
                                   user picks /Users/.../owner-recovery.bin
                                 → insert_path_token(path) → path_token
  ← Some(path_token)

  → export_owner_recovery_file_to_path(recovery_token, path_token, pass, comment)
                                 → take_path_token(path_token) → path
                                 → take_token(recovery_token) → seed
                                 → encrypt + write_atomic_0600(path, bytes)
  ← ExportInfo { identityHash, byteLen, path }
```

Both tokens consumed *inside* `run_blocking`. Path token is consumed first so a downstream seed-token failure doesn't leave the path live in the cache. Path tokens are cheap to discard.

**Cancel:** `request_export_save_path` returns `Ok(None)` when the user cancels the dialog. Renderer treats this exactly like today's `if (!out) return` — no token issued, no further IPC, recovery_token survives in cache for the next attempt.

## Error handling

| Scenario | Behavior |
|---|---|
| Renderer replays consumed path_token | `take_path_token` returns `None` → `Err("Save path token expired or invalid. Please re-trigger backup.")` |
| Renderer fabricates a UUID | Same as above (uniform error message). |
| Renderer captures path_token mid-flight, beats user to commit | Renderer commits to user's chosen path. Same outcome the user authorized. Not a privilege gain. |
| Path token TTL expires (5 min) | `take_path_token` returns `None`; same error path as above. Recoverable: user re-clicks Save. |
| Two parallel export attempts | Each gets its own path_token. Cache caps at 8 with LRU. |
| Renderer-supplied dialog hints (default filename / filter) deceive user | Out of scope — final path is user-confirmed. |
| Tauri dialog plugin returns error | `request_export_save_path` returns `Err(e.to_string())`; renderer surfaces as backupError. |

## Testing strategy

**Rust unit tests:**

In `owner_state.rs`, alongside `token_cache_tests`:

- `path_token_cache_tests::insert_then_take_returns_path_once` — single-use; second take returns `None`.
- `path_token_cache_tests::nonexistent_token_returns_none`.
- `path_token_cache_tests::lru_evicts_when_max_live_path_tokens_exceeded` — newest-preserving + cap invariant. Same `Instant::now()` non-determinism caveat as the seed-token version.

In `owner_commands.rs`:

- `export_with_invalid_path_token_errors` — recovery_token valid, path_token bogus → "Save path token expired or invalid".
- `export_consumes_both_tokens_on_success` — happy path; assert both caches no longer contain the consumed UUIDs.
- `export_consumes_path_token_even_when_seed_token_invalid` — fixes consumption ordering: path_token taken first, seed second; if seed fails the path token is still gone (so a later replay is impossible). Documents the invariant against future refactors.

In `identity_commands.rs`:

- `export_recovery_with_invalid_path_token_errors` — sibling test for the per-device export.

The new `request_export_save_path` IPC is *not* unit-tested directly — `tauri_plugin_dialog`'s blocking save-file requires an AppHandle and can't be driven headless without a full Tauri runtime. The wrapper is a thin "call dialog → cache path → return UUID" composition; both ends of that composition (`insert_path_token` and the dialog plugin) are independently verified.

**Frontend (vitest):**

- `owner-service.test.ts` — argument-order test for the new `exportRecoveryFile(recoveryToken, pathToken, ...)` shape.
- `DevicesPanel.test.ts` — mock `request_export_save_path` instead of `@tauri-apps/plugin-dialog::save`. Update the existing 5+ tests for: happy path, dialog cancel, invalid path token, save errors. Verify `extractError` still surfaces correctly.
- `IdentityPanel.test.ts` — same refactor for the per-device export wizard. Update the existing 7+ tests.

**Manual smoke test (added to PR Test Plan):**

1. `npm run tauri dev`.
2. Mint identity → click "Back up owner identity" → choose a path → enter passphrase → confirm.
3. Verify file lands at chosen location with mode `0600`.
4. Repeat with cancel-the-dialog → verify token survives, retry works.
5. Repeat with bad passphrase → verify error displayed and modal recoverable.
6. Same flow for IdentityPanel's per-device backup wizard.

## Acceptance criteria

- `export_owner_recovery_file_to_path` takes `path_token: String`, never accepts a renderer-supplied path. Direct IPC invocation with a fabricated UUID produces a clear error and writes nothing.
- `export_recovery_file_to_path` likewise.
- New `request_export_save_path` IPC opens OS dialog server-side, caches user-confirmed path under a UUID with 5-min TTL + 8-entry LRU.
- DevicesPanel and IdentityPanel no longer import `@tauri-apps/plugin-dialog::save`. UX is unchanged from the user's perspective.
- All existing tests pass after refactor; new tests added per the testing strategy above.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `npx tsc --noEmit`, and `npx vitest run` all clean.

## References

- ZEB-194 (this issue).
- ZEB-170 PR #62 — original CodeRabbit finding.
- `src-tauri/src/owner_state.rs::TOKEN_CACHE` — the pattern this design mirrors.
- Tauri 2 dialog plugin: `tauri_plugin_dialog::DialogExt::dialog()` for Rust-side dialog access.
- Existing dialog usage in `src-tauri/src/lib.rs` (already imports `tauri_plugin_dialog::DialogExt`).
