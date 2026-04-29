# ZEB-194 Export-Path Capability-Token Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace renderer-supplied write paths in `export_owner_recovery_file_to_path` and `export_recovery_file_to_path` with capability-tokens issued by a backend-owned save-dialog IPC.

**Architecture:** Add a sibling `PATH_TOKEN_CACHE` to the existing `TOKEN_CACHE` in `owner_state.rs` (same TTL, same LRU, same panic-recovering lock). Introduce `request_export_save_path` in a new `save_dialog.rs` module that opens the OS dialog server-side via `tauri_plugin_dialog::DialogExt` and caches the user-confirmed path under a UUID. Migrate both `*_to_path` IPCs to take `path_token: String` instead of a raw path. Update the two svelte panels (DevicesPanel, IdentityPanel) to call the new dialog IPC.

**Tech Stack:** Rust 2021, Tokio, Tauri 2 (`tauri-plugin-dialog`), Svelte 5 / TypeScript, Vitest.

**Spec:** `docs/specs/2026-04-29-zeb-194-export-path-capability-token-design.md`

---

## File Structure

**New:**
- `src-tauri/src/save_dialog.rs` — Tauri command `request_export_save_path` that opens the OS save dialog and caches the chosen path under a UUID. Self-contained module; no other code lives here.

**Modified (Rust):**
- `src-tauri/src/owner_state.rs` — add `PATH_TOKEN_CACHE` and its insert/take/clear API alongside the existing `TOKEN_CACHE`.
- `src-tauri/src/owner_commands.rs` — `export_owner_recovery_file_to_path` takes `path_token` instead of `path`; `ExportInfo` gains `path: String`.
- `src-tauri/src/identity_commands.rs` — `export_recovery_file_to_path` takes `path_token` instead of `out_path`.
- `src-tauri/src/lib.rs` — `mod save_dialog;` and register `save_dialog::request_export_save_path` in `invoke_handler!`.

**Modified (Frontend):**
- `src/lib/owner-service.ts` — add `requestExportSavePath` helper and `ExportSavePathRequest` type; update `exportRecoveryFile` signature; add `path` to `ExportInfo`.
- `src/lib/components/DevicesPanel.svelte` — replace `save({...})` with `svc.requestExportSavePath({...})`; pass `pathToken` into `exportRecoveryFile`; remove the `@tauri-apps/plugin-dialog` import.
- `src/lib/components/IdentityPanel.svelte` — same refactor; preserve the `epoch` guard and silent-error semantics.
- `src/lib/components/__tests__/DevicesPanel.test.ts` — remove dialog plugin mock; mock `request_export_save_path` invoke; update existing assertions about `save()` calls.
- `src/lib/components/__tests__/IdentityPanel.test.ts` — same.
- `src/lib/owner-service.test.ts` — argument-order test for the new `exportRecoveryFile` shape; new test for `requestExportSavePath`.

---

### Task 1: Add PATH_TOKEN_CACHE in owner_state.rs

**Files:**
- Modify: `src-tauri/src/owner_state.rs:67-200` (extend the token-cache section).

- [ ] **Step 1: Write failing tests for the new cache**

Add to the end of the existing `token_cache_tests` mod, OR create a sibling `path_token_cache_tests` mod (sibling is cleaner — prevents test cross-contamination on the global cache):

```rust
#[cfg(test)]
mod path_token_cache_tests {
    use super::*;
    use serial_test::serial;
    use std::path::PathBuf;

    #[test]
    #[serial]
    fn insert_then_take_returns_path_once() {
        clear_path_token_cache();
        let path = PathBuf::from("/tmp/example-recovery.bin");
        let token = insert_path_token(path.clone());
        let taken = take_path_token(&token).expect("first take must succeed");
        assert_eq!(taken, path);
        assert!(
            take_path_token(&token).is_none(),
            "second take must return None (single-use)"
        );
    }

    #[test]
    #[serial]
    fn nonexistent_token_returns_none() {
        clear_path_token_cache();
        let bogus = Uuid::new_v4();
        assert!(take_path_token(&bogus).is_none());
    }

    #[test]
    #[serial]
    fn lru_evicts_when_max_live_path_tokens_exceeded() {
        // Mirrors the seed-token test: newest-preserving + cap invariant.
        // Same Instant::now() non-determinism caveat — we don't assert which
        // tokens were evicted, only that the cap holds and the newest survives.
        clear_path_token_cache();
        let mut tokens = Vec::new();
        for i in 0..(MAX_LIVE_PATH_TOKENS + 2) {
            tokens.push(insert_path_token(PathBuf::from(format!("/tmp/{i}.bin"))));
        }
        let last_token = tokens[MAX_LIVE_PATH_TOKENS + 1];
        assert!(
            take_path_token(&last_token).is_some(),
            "newest-inserted token must remain after cap-exceed insert"
        );
        let remaining: usize = tokens
            .iter()
            .filter(|t| **t != last_token)
            .filter(|t| take_path_token(t).is_some())
            .count();
        assert_eq!(
            remaining,
            MAX_LIVE_PATH_TOKENS - 1,
            "after MAX_LIVE_PATH_TOKENS+2 inserts, exactly MAX_LIVE_PATH_TOKENS must survive"
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo test --lib owner_state::path_token_cache_tests`
Expected: FAIL with "cannot find function `insert_path_token`" (and similar for `take_path_token`, `clear_path_token_cache`, `MAX_LIVE_PATH_TOKENS`).

- [ ] **Step 3: Implement the cache**

Add to `src-tauri/src/owner_state.rs` immediately after the existing `clear_token_cache` definition (around line 132):

```rust
// ── Path token cache for export-save-dialog confirmations ────────────────
//
// Mirror of TOKEN_CACHE but for user-confirmed save paths. The
// `request_export_save_path` IPC opens the OS save dialog server-side and
// inserts the chosen PathBuf here; `export_*_to_path` consumes the token
// at commit time. The renderer never names a write path directly.
//
// Two separate caches (rather than one polymorphic enum) because the value
// types are semantically different: master-seed tokens hold
// `Zeroizing<[u8; 32]>` and need zeroize-on-drop; path tokens hold
// `PathBuf` and don't. Type-level separation prevents "wrong token type"
// runtime bugs.

const PATH_TOKEN_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_LIVE_PATH_TOKENS: usize = 8;

struct PathTokenEntry {
    path: std::path::PathBuf,
    inserted_at: Instant,
}

static PATH_TOKEN_CACHE: LazyLock<Mutex<HashMap<Uuid, PathTokenEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn path_token_cache_lock() -> std::sync::MutexGuard<'static, HashMap<Uuid, PathTokenEntry>> {
    PATH_TOKEN_CACHE.lock().unwrap_or_else(|p| p.into_inner())
}

/// Insert a save path into the path-token cache, returning a fresh
/// single-use token. Caller hands the token to the GUI; GUI presents it
/// back via `take_path_token` on commit.
pub fn insert_path_token(path: std::path::PathBuf) -> Uuid {
    let token = Uuid::new_v4();
    let mut cache = path_token_cache_lock();
    evict_expired_paths(&mut cache);
    if cache.len() >= MAX_LIVE_PATH_TOKENS {
        let oldest = cache
            .iter()
            .min_by_key(|(_, e)| e.inserted_at)
            .map(|(k, _)| *k);
        if let Some(k) = oldest {
            cache.remove(&k);
        }
    }
    cache.insert(
        token,
        PathTokenEntry {
            path,
            inserted_at: Instant::now(),
        },
    );
    token
}

/// Consume a path token: returns the user-confirmed save path exactly once.
pub fn take_path_token(token: &Uuid) -> Option<std::path::PathBuf> {
    let mut cache = path_token_cache_lock();
    evict_expired_paths(&mut cache);
    cache.remove(token).map(|e| e.path)
}

fn evict_expired_paths(cache: &mut HashMap<Uuid, PathTokenEntry>) {
    cache.retain(|_, e| e.inserted_at.elapsed() < PATH_TOKEN_TTL);
}

#[doc(hidden)]
#[cfg(test)]
pub(crate) fn clear_path_token_cache() {
    path_token_cache_lock().clear();
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo test --lib owner_state::path_token_cache_tests`
Expected: PASS (3 tests).

- [ ] **Step 5: Run lints + format**

Run: `cd src-tauri && cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/owner_state.rs
git commit -m "feat(owner_state): add PATH_TOKEN_CACHE for export-save-dialog confirmations (ZEB-194)

Sibling to TOKEN_CACHE: same TTL, same LRU, same panic-recovering lock,
but holds PathBuf instead of Zeroizing<[u8;32]>. Two separate caches
prevent runtime 'wrong token type' bugs that a single polymorphic cache
would invite.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: New save_dialog.rs module + register IPC in lib.rs

**Files:**
- Create: `src-tauri/src/save_dialog.rs`
- Modify: `src-tauri/src/lib.rs` — add `mod save_dialog;` near other module declarations and register `save_dialog::request_export_save_path` in `invoke_handler!`.

- [ ] **Step 1: Create save_dialog.rs**

Write the new module:

```rust
//! Backend-owned save-dialog IPC. The renderer never names a write path
//! directly: it asks this command to open the OS save dialog, and gets back
//! an opaque UUID that resolves to the user-confirmed PathBuf on commit.
//!
//! Pair this with `crate::owner_state::take_path_token` in any command that
//! used to take a renderer-supplied path. See ZEB-194 design doc:
//! `docs/specs/2026-04-29-zeb-194-export-path-capability-token-design.md`.

use crate::identity_commands::run_blocking;
use crate::owner_state::insert_path_token;
use tauri_plugin_dialog::DialogExt;

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportPathRequest {
    pub title: Option<String>,
    pub default_filename: String,
    pub filter_name: String,
    pub filter_extensions: Vec<String>,
}

/// Open the OS save dialog and cache the user-confirmed path under a UUID.
/// Returns `Ok(Some(uuid))` on confirm, `Ok(None)` on cancel,
/// `Err(_)` on dialog plugin error.
///
/// The dialog hints (title/filename/filter) are renderer-supplied UX-only
/// values — the user explicitly confirms the final path in the OS dialog
/// and can rename or redirect freely. Default filename is a hint, not a
/// security control.
#[tauri::command]
pub async fn request_export_save_path(
    app: tauri::AppHandle,
    request: ExportPathRequest,
) -> Result<Option<String>, String> {
    run_blocking(move || {
        // tauri_plugin_dialog's blocking save_file API: synchronous on a
        // worker thread, returns Option<FilePath>. None == user cancelled.
        let mut builder = app
            .dialog()
            .file()
            .set_file_name(&request.default_filename)
            .add_filter(&request.filter_name, &request.filter_extensions);
        if let Some(t) = request.title.as_deref() {
            builder = builder.set_title(t);
        }
        let chosen = builder.blocking_save_file();
        match chosen {
            Some(file_path) => {
                // Tauri 2 wraps the path in `FilePath`; `into_path()` extracts
                // the PathBuf. On platforms where the dialog can return URI-
                // style paths (mobile), this errors — we surface that as an
                // IPC error rather than silently dropping the user's choice.
                let path = file_path
                    .into_path()
                    .map_err(|e| format!("dialog returned non-filesystem path: {e}"))?;
                let token = insert_path_token(path);
                Ok(Some(token.to_string()))
            }
            None => Ok(None),
        }
    })
    .await
}
```

- [ ] **Step 2: Wire the module into lib.rs**

Edit `src-tauri/src/lib.rs`:
1. Find the block of `mod` declarations near the top of the file, add: `mod save_dialog;`
2. Find the `tauri::generate_handler![...]` block where `owner_commands::export_owner_recovery_file_to_path` and `identity_commands::export_recovery_file_to_path` are registered. Add a new line: `save_dialog::request_export_save_path,`

- [ ] **Step 3: Build to verify wiring**

Run: `cd src-tauri && cargo build`
Expected: clean build. If `tauri_plugin_dialog::DialogExt` isn't in scope, the import in `save_dialog.rs` already brings it in — no change needed elsewhere.

- [ ] **Step 4: Run lints + format**

Run: `cd src-tauri && cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/save_dialog.rs src-tauri/src/lib.rs
git commit -m "feat(save_dialog): add request_export_save_path IPC (ZEB-194)

Backend-owned OS save dialog. Renderer supplies UX-only hints
(title/default filename/filter); user confirms final path in the OS
dialog; backend caches the chosen path under a UUID and returns the
token. Pairs with owner_state::take_path_token in export commit IPCs.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Migrate export_owner_recovery_file_to_path to path_token

**Files:**
- Modify: `src-tauri/src/owner_commands.rs` — signature change at line 220, ExportInfo at line 58, three existing tests at lines 326, 353, 374.

- [ ] **Step 1: Update existing tests to construct path tokens**

In the `tests` mod near the bottom of `owner_commands.rs`:

For each of the three existing tests (`export_with_too_short_passphrase_errors_without_consuming_token`, `export_with_invalid_token_errors`, `comment_over_cap_rejected`):
- Replace the literal path string `"/tmp/should-not-write".into()` with a path token created via `insert_path_token(PathBuf::from("/tmp/should-not-write"))`.
- For `export_with_invalid_token_errors`: the test currently uses a bogus recovery token and a real path string. Convert to: bogus recovery token, valid path token. The error must still mention "expired" or "invalid" (now from the recovery_token consumption since path_token is consumed first and succeeds).
- Add `clear_path_token_cache()` next to each `clear_token_cache()` call.
- Add `use crate::owner_state::{clear_path_token_cache, insert_path_token};` to the imports.

For `export_with_too_short_passphrase_errors_without_consuming_token` and `comment_over_cap_rejected`:
- Capture the path token Uuid before calling `export_owner_recovery_file_to_path`.
- After the assert that the recovery token survived, ALSO assert `take_path_token(&path_uuid).is_some()` — the validation runs before any cache consumption, so neither token must be consumed on these failure paths.

Updated invocation pattern:
```rust
let recovery_uuid = insert_token(Zeroizing::new([0xCDu8; 32]));
let path_uuid = insert_path_token(PathBuf::from("/tmp/should-not-write"));
let result = rt.block_on(export_owner_recovery_file_to_path(
    recovery_uuid.to_string(),
    path_uuid.to_string(),
    "short".into(),
    None,
));
```

- [ ] **Step 2: Add new tests for the path-token contract**

Add three new tests to the same `tests` mod:

```rust
#[test]
#[serial]
fn export_with_invalid_path_token_errors() {
    clear_token_cache();
    clear_path_token_cache();
    let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
    let recovery_uuid = insert_token(Zeroizing::new([0xAAu8; 32]));
    let bogus_path_uuid = Uuid::new_v4();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(export_owner_recovery_file_to_path(
        recovery_uuid.to_string(),
        bogus_path_uuid.to_string(),
        "passphrase-12+".into(),
        None,
    ));
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.to_lowercase().contains("path token") && (err.contains("expired") || err.contains("invalid")),
        "error must mention path token expired/invalid; got: {err}"
    );
    // Recovery token MUST survive: path-token consumption happens first
    // and fails, so seed-token consumption never runs.
    assert!(
        take_token(&recovery_uuid).is_some(),
        "invalid path-token must not consume recovery token"
    );
}

#[test]
#[serial]
fn export_consumes_path_token_even_when_seed_token_invalid() {
    // Pins the consumption ORDER: path_token taken first; if that succeeds
    // and seed-token consumption fails, the path token is still gone (so a
    // later replay of either token is impossible). This documents the
    // invariant against future refactors that might reorder consumption.
    clear_token_cache();
    clear_path_token_cache();
    let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
    let bogus_recovery_uuid = Uuid::new_v4();
    let path_uuid = insert_path_token(PathBuf::from("/tmp/zeb194-ordering-test.bin"));
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(export_owner_recovery_file_to_path(
        bogus_recovery_uuid.to_string(),
        path_uuid.to_string(),
        "passphrase-12+".into(),
        None,
    ));
    assert!(result.is_err());
    // Path token MUST have been consumed (taken first) even though the
    // overall command failed.
    assert!(
        take_path_token(&path_uuid).is_none(),
        "path token must be consumed even when subsequent seed-token consumption fails"
    );
}

#[test]
#[serial]
fn export_consumes_both_tokens_on_success() {
    // Note: this test cannot exercise the actual file write because the
    // command resolves identity_dir from real OS paths and writes via
    // write_atomic_0600 which would touch the real filesystem. We assert
    // the cache invariant — both tokens consumed — under the failure that
    // happens AFTER both takes (the actual recovery-artifact construction
    // succeeds; the disk write to /tmp/.../<unique> may also succeed).
    //
    // To keep the test pure-cache, point the path at a tempdir.
    clear_token_cache();
    clear_path_token_cache();
    let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("recovery.bin");
    let recovery_uuid = insert_token(Zeroizing::new([0xBBu8; 32]));
    let path_uuid = insert_path_token(out.clone());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(export_owner_recovery_file_to_path(
        recovery_uuid.to_string(),
        path_uuid.to_string(),
        "passphrase-12+".into(),
        None,
    ));
    assert!(result.is_ok(), "export must succeed; got: {result:?}");
    // Both caches must no longer hold the consumed UUIDs.
    assert!(take_token(&recovery_uuid).is_none(), "recovery token must be consumed");
    assert!(take_path_token(&path_uuid).is_none(), "path token must be consumed");
    // ExportInfo.path must echo the chosen path.
    let info = result.unwrap();
    assert_eq!(info.path, out.display().to_string());
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cd src-tauri && cargo test --lib owner_commands::tests`
Expected: FAIL — signature mismatch on `export_owner_recovery_file_to_path` (existing tests now pass `path_uuid.to_string()` where the impl expects a path string), and `info.path` field doesn't exist on `ExportInfo`.

- [ ] **Step 4: Update the IPC signature and ExportInfo**

In `src-tauri/src/owner_commands.rs`:

1. Add `path: String` to `ExportInfo` (currently lines 56-61):

```rust
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportInfo {
    pub identity_hash: String,
    pub byte_len: u64,
    pub path: String,
}
```

2. Replace `export_owner_recovery_file_to_path` (lines 219-272) with the new path-token shape. The new body:

```rust
#[tauri::command]
pub async fn export_owner_recovery_file_to_path(
    recovery_token: String,
    path_token: String,
    passphrase: String,
    comment: Option<String>,
) -> Result<ExportInfo, String> {
    // Validate passphrase length BEFORE consuming any token (existing).
    if passphrase.chars().count() < MIN_RECOVERY_PASSPHRASE_LEN {
        return Err(format!(
            "Recovery passphrase must be at least {MIN_RECOVERY_PASSPHRASE_LEN} characters."
        ));
    }
    // Validate comment length BEFORE consuming any token (existing).
    let comment_validated = match comment {
        Some(c) if c.len() > 256 => {
            return Err("Recovery comment must be at most 256 bytes.".to_string());
        }
        c => c,
    };
    let recovery_uuid: Uuid = recovery_token
        .parse()
        .map_err(|e| format!("invalid recovery token: {e}"))?;
    let path_uuid: Uuid = path_token
        .parse()
        .map_err(|e| format!("invalid path token: {e}"))?;
    run_blocking(move || {
        // Consume path_token FIRST so a downstream seed-token consumption
        // failure does not leave a path token live in the cache pointing
        // at the user's chosen file (ZEB-194 ordering invariant — see test
        // `export_consumes_path_token_even_when_seed_token_invalid`).
        let out = crate::owner_state::take_path_token(&path_uuid).ok_or_else(|| {
            "Save path token expired or invalid. Please re-trigger backup.".to_string()
        })?;
        let seed = take_token(&recovery_uuid).ok_or_else(|| {
            "Recovery token expired or invalid. Please re-trigger backup from the Devices panel."
                .to_string()
        })?;
        let secret = SecretString::from(passphrase);
        let artifact = RecoveryArtifact::from_seed(*seed);
        let id_hash = artifact.master_pubkey_bundle().identity_hash();
        let metadata = RecoveryMetadata {
            mint_at: None,
            comment: comment_validated,
        };
        let bytes = artifact
            .to_encrypted_file(&secret, &metadata)
            .map_err(|e| format!("encrypt recovery file: {e}"))?;
        crate::identity::write_atomic_0600(&out, &bytes)
            .map_err(|e| format!("write {}: {e}", out.display()))?;
        Ok(ExportInfo {
            identity_hash: hex::encode(id_hash),
            byte_len: bytes.len() as u64,
            path: out.display().to_string(),
        })
    })
    .await
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd src-tauri && cargo test --lib owner_commands::tests`
Expected: PASS (3 existing + 3 new = 6 tests in this mod, plus the unrelated tests).

- [ ] **Step 6: Run lints + format**

Run: `cd src-tauri && cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/owner_commands.rs
git commit -m "feat(owner_commands): export_owner_recovery_file_to_path consumes path_token (ZEB-194)

Replace renderer-supplied path string with a UUID resolved server-side
from PATH_TOKEN_CACHE. Path token is consumed FIRST inside run_blocking
so a downstream seed-token failure does not leave a replayable path
token. ExportInfo gains a `path` field so the renderer can display the
saved location without re-stating its request.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Migrate export_recovery_file_to_path to path_token

**Files:**
- Modify: `src-tauri/src/identity_commands.rs:467-484` (the IPC) and the relevant test cases.

- [ ] **Step 1: Find existing tests for the IPC**

Search: `grep -n "export_recovery_file_to_path_helper\|export_recovery_file_to_path(" src-tauri/src/identity_commands.rs`

The unit tests in `identity_commands.rs` invoke `export_recovery_file_to_path_helper` directly (not the `#[tauri::command]` wrapper), so they don't need updating — the helper still takes a `&Path`. Only the wrapper signature changes.

- [ ] **Step 2: Write a failing IPC-level test**

Add to the `tests` mod in `identity_commands.rs`:

```rust
#[test]
#[serial]
fn export_recovery_with_invalid_path_token_errors() {
    use crate::owner_state::{clear_path_token_cache, insert_path_token};
    clear_path_token_cache();
    let bogus = Uuid::new_v4();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(export_recovery_file_to_path(
        bogus.to_string(),
        "passphrase-12+".into(),
        None,
    ));
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.to_lowercase().contains("path token") && (err.contains("expired") || err.contains("invalid")),
        "error must mention path token expired/invalid; got: {err}"
    );
    // Sanity: a fresh path token is still acceptable (cache is functional).
    let _ = insert_path_token(PathBuf::from("/tmp/zeb194-sanity"));
}
```

(Note: the existing `export_recovery_file_to_path_helper` tests at lines 825 and 853 stay as-is — they test the helper which keeps its `&Path` signature. Only the IPC wrapper's tests need the new shape.)

- [ ] **Step 3: Run tests to verify they fail**

Run: `cd src-tauri && cargo test --lib identity_commands::tests::export_recovery_with_invalid_path_token_errors`
Expected: FAIL — signature mismatch (current `export_recovery_file_to_path` takes `out_path: PathBuf`, not `path_token: String`).

- [ ] **Step 4: Update the IPC signature**

Replace `export_recovery_file_to_path` in `src-tauri/src/identity_commands.rs:467-484` with:

```rust
/// Export the master seed as a passphrase-encrypted recovery file at the
/// path the user confirmed via `request_export_save_path`. The renderer
/// passes back the opaque token from that dialog command — it never names
/// the path directly.
///
/// Returns the path that was actually written to, so the renderer (which
/// no longer knows the path) can display "saved to X" feedback.
#[tauri::command]
pub async fn export_recovery_file_to_path(
    path_token: String,
    passphrase: String,
    comment: Option<String>,
) -> Result<String, String> {
    let plaintext_path = identity::resolve_path(None)?;
    let path_uuid: Uuid = path_token
        .parse()
        .map_err(|e| format!("invalid path token: {e}"))?;
    run_blocking(move || {
        let out_path = crate::owner_state::take_path_token(&path_uuid).ok_or_else(|| {
            "Save path token expired or invalid. Please re-trigger backup.".to_string()
        })?;
        export_recovery_file_to_path_helper(
            &plaintext_path,
            &out_path,
            &passphrase,
            comment,
            KeychainStore::new().ok(),
        )?;
        Ok(out_path.display().to_string())
    })
    .await
}
```

(`Uuid` is already imported at the top of `identity_commands.rs` from existing preview-token logic; verify the import covers the bare `Uuid` symbol — if not, add `use uuid::Uuid;`.)

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd src-tauri && cargo test --lib identity_commands::tests`
Expected: PASS (existing helper tests still green, new IPC test green).

- [ ] **Step 6: Run lints + format**

Run: `cd src-tauri && cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/identity_commands.rs
git commit -m "feat(identity_commands): export_recovery_file_to_path consumes path_token (ZEB-194)

Sibling fix to owner-export migration. Per-device transport-identity
recovery export now resolves its write path from PATH_TOKEN_CACHE
instead of accepting a renderer-supplied path string.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Update owner-service.ts

**Files:**
- Modify: `src/lib/owner-service.ts` — add helper + update signature.
- Modify: `src/lib/owner-service.test.ts` — update call-site test, add new helper test.

- [ ] **Step 1: Update the test file with the new shapes**

Find the `exportRecoveryFile` test at `src/lib/owner-service.test.ts:53` and update its call:

```ts
it('exportRecoveryFile passes args verbatim', async () => {
  const invoke = vi.fn().mockResolvedValue({
    identityHash: 'h', byteLen: 1024, path: '/tmp/r',
  });
  const svc = new OwnerService(invoke);
  await svc.exportRecoveryFile('tok', 'path-tok', 'a-strong-passphrase', 'comment');
  expect(invoke).toHaveBeenCalledWith('export_owner_recovery_file_to_path', {
    recoveryToken: 'tok',
    pathToken: 'path-tok',
    passphrase: 'a-strong-passphrase',
    comment: 'comment',
  });
});
```

Add a new test for the helper:

```ts
it('requestExportSavePath forwards the dialog request shape', async () => {
  const invoke = vi.fn().mockResolvedValue('path-token-uuid');
  const svc = new OwnerService(invoke);
  const got = await svc.requestExportSavePath({
    title: 'Save backup',
    defaultFilename: 'owner-recovery.bin',
    filterName: 'Recovery file',
    filterExtensions: ['bin'],
  });
  expect(got).toBe('path-token-uuid');
  expect(invoke).toHaveBeenCalledWith('request_export_save_path', {
    request: {
      title: 'Save backup',
      defaultFilename: 'owner-recovery.bin',
      filterName: 'Recovery file',
      filterExtensions: ['bin'],
    },
  });
});

it('requestExportSavePath returns null when user cancels', async () => {
  const invoke = vi.fn().mockResolvedValue(null);
  const svc = new OwnerService(invoke);
  const got = await svc.requestExportSavePath({
    defaultFilename: 'x',
    filterName: 'y',
    filterExtensions: ['z'],
  });
  expect(got).toBeNull();
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/owner-service.test.ts`
Expected: FAIL on signature mismatch (`pathToken` arg) and missing `requestExportSavePath` method.

- [ ] **Step 3: Update owner-service.ts**

In `src/lib/owner-service.ts`:

1. Update the `ExportInfo` type to include `path: string`.
2. Update `exportRecoveryFile` signature:

```ts
async exportRecoveryFile(
  recoveryToken: string,
  pathToken: string,
  passphrase: string,
  comment: string | null,
): Promise<ExportInfo> {
  return invoke<ExportInfo>('export_owner_recovery_file_to_path', {
    recoveryToken,
    pathToken,
    passphrase,
    comment,
  });
}
```

3. Add the helper + type:

```ts
export interface ExportSavePathRequest {
  title?: string;
  defaultFilename: string;
  filterName: string;
  filterExtensions: string[];
}

async requestExportSavePath(req: ExportSavePathRequest): Promise<string | null> {
  return invoke<string | null>('request_export_save_path', { request: req });
}
```

(Place `requestExportSavePath` next to `exportRecoveryFile`. The `ExportSavePathRequest` interface goes near the top of the file with the other exported types.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/owner-service.test.ts`
Expected: PASS.

- [ ] **Step 5: Run typecheck**

Run: `npx tsc --noEmit`
Expected: clean (frontend-wide; this catches downstream call sites that still use the old `exportRecoveryFile(_, path, ...)` signature — those are fixed in Tasks 6 and 7, so transient TS errors here are expected if running in isolation; if so, keep a note and run again at the end of Task 7).

- [ ] **Step 6: Commit**

```bash
git add src/lib/owner-service.ts src/lib/owner-service.test.ts
git commit -m "feat(owner-service): add requestExportSavePath helper, swap exportRecoveryFile path→pathToken (ZEB-194)

Frontend wrapper for the new request_export_save_path IPC. Lives in
owner-service even though IdentityPanel will also use it — the dialog
wrapper is generic UX scaffolding, not an owner-vs-identity dispatch
concern. ExportInfo TS type gains \`path\` to mirror the backend.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Refactor DevicesPanel.svelte

**Files:**
- Modify: `src/lib/components/DevicesPanel.svelte::commitBackup` — replace `save({...})` with `svc.requestExportSavePath({...})`.
- Modify: `src/lib/components/__tests__/DevicesPanel.test.ts` — drop dialog plugin mock, mock the new IPC.

- [ ] **Step 1: Update DevicesPanel.test.ts to mock the new IPC**

In `src/lib/components/__tests__/DevicesPanel.test.ts`:

1. Remove the `vi.mock('@tauri-apps/plugin-dialog', …)` block at lines 9-12.
2. Remove `import { save } from '@tauri-apps/plugin-dialog';` at line 15.
3. For each existing test that exercises `commitBackup` (search for tests that call the "Save backup" button after entering a passphrase), the previous mock chain was:
   - `mockResolvedValueOnce(initialState)` for `get_owner_state`
   - `mockResolvedValueOnce({ recoveryToken: 'tok' })` for `issue_owner_recovery_token`
   - implicit `save()` returns `'/tmp/owner-recovery.bin'`
   - `mockResolvedValueOnce(...)` for `export_owner_recovery_file_to_path`

   The new chain:
   - `mockResolvedValueOnce(initialState)` for `get_owner_state`
   - `mockResolvedValueOnce({ recoveryToken: 'tok' })` for `issue_owner_recovery_token`
   - `mockResolvedValueOnce('path-token-uuid')` for `request_export_save_path` (NEW)
   - `mockResolvedValueOnce({ identityHash: 'h', byteLen: 1024, path: '/tmp/owner-recovery.bin' })` for `export_owner_recovery_file_to_path`

4. For tests that assert the save() call shape (e.g., `expect(save).toHaveBeenCalledWith(...)`), replace with `expect(invoke).toHaveBeenCalledWith('request_export_save_path', { request: { defaultFilename: 'owner-recovery.bin', filterName: 'Recovery file', filterExtensions: ['bin'] } })`.

5. For tests that assert the `export_owner_recovery_file_to_path` invoke shape (search for `'export_owner_recovery_file_to_path'`), update the expected args to use `pathToken: 'path-token-uuid'` instead of the literal `'/tmp/owner-recovery.bin'` path.

6. For the "user cancels dialog" test (if any — search for `mockResolvedValueOnce(null)` near `save`), update it to mock `request_export_save_path` returning `null` and assert the export IPC is NOT called.

7. Add a new test: "user receives saved path from ExportInfo" — assert `backupSavedPath` ends up at the path returned in `ExportInfo.path`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts`
Expected: FAIL — DevicesPanel still calls `save()` from `@tauri-apps/plugin-dialog`, but the import was removed by the test refactor (or, if you ran tests before refactoring DevicesPanel.svelte: tests fail because the panel actually calls `save` which is no longer mocked, so it tries to invoke the real plugin in jsdom).

- [ ] **Step 3: Refactor DevicesPanel.svelte::commitBackup**

In `src/lib/components/DevicesPanel.svelte`:

1. Remove `import { save } from '@tauri-apps/plugin-dialog';` (top of `<script>` block).
2. Replace the dialog block in `commitBackup` (currently lines 179-189):

   **Old:**
   ```ts
   let out: string | null;
   try {
     out = await save({
       defaultPath: 'owner-recovery.bin',
       filters: [{ name: 'Recovery file', extensions: ['bin'] }],
     });
   } catch (e) {
     backupError = extractError(e);
     return;
   }
   if (!out) return;
   ```

   **New:**
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
   ```

3. Update the export call (currently lines 192-197):

   **Old:**
   ```ts
   await svc.exportRecoveryFile(
     recoveryToken,
     out,
     backupPassphrase,
     trimmedComment ? trimmedComment : null,
   );
   backupSavedPath = out;
   ```

   **New:**
   ```ts
   const info = await svc.exportRecoveryFile(
     recoveryToken,
     pathToken,
     backupPassphrase,
     trimmedComment ? trimmedComment : null,
   );
   backupSavedPath = info.path;
   ```

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts`
Expected: PASS.

- [ ] **Step 5: Run typecheck**

Run: `npx tsc --noEmit`
Expected: clean (modulo the IdentityPanel-side errors, which are still on the old shape until Task 7).

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/DevicesPanel.svelte src/lib/components/__tests__/DevicesPanel.test.ts
git commit -m "feat(DevicesPanel): use backend save dialog (request_export_save_path) (ZEB-194)

Replace @tauri-apps/plugin-dialog::save with svc.requestExportSavePath.
The renderer no longer names the export path; it receives an opaque
path token from the backend dialog and ferries it into the export
commit. backupSavedPath now sources from ExportInfo.path so the UI
reflects exactly what the backend wrote.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: Refactor IdentityPanel.svelte

**Files:**
- Modify: `src/lib/components/IdentityPanel.svelte::advanceFromFileEntry` (around line 166).
- Modify: `src/lib/components/__tests__/IdentityPanel.test.ts` — remove dialog plugin mock, mock the new IPC.

- [ ] **Step 1: Update IdentityPanel.test.ts to mock the new IPC**

In `src/lib/components/__tests__/IdentityPanel.test.ts`:

1. Search for `'@tauri-apps/plugin-dialog'` and `from 'svelte'` mocks; remove the dialog `save` mock (similar to Task 6 step 1 sub-step 1).
2. For each test that exercises `advanceFromFileEntry` (search for `'export_recovery_file_to_path'` — there are ~7 cases at lines 568, 597, 620, 641, 663, 683, 708, 736, 766), update the mock chain to include a `request_export_save_path` mock returning a path token (e.g., `'path-tok'`) BEFORE the `export_recovery_file_to_path` mock.
3. Update assertions about the `export_recovery_file_to_path` invoke shape: `outPath` parameter is removed; `pathToken: 'path-tok'` is added.
4. For tests that check dialog cancel behavior (if any — search for `null` returns in dialog mocks), retarget to `request_export_save_path` returning `null` and assert downstream invokes do not run.

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/components/__tests__/IdentityPanel.test.ts`
Expected: FAIL on the same dynamic as Task 6 step 2.

- [ ] **Step 3: Refactor IdentityPanel.svelte::advanceFromFileEntry**

In `src/lib/components/IdentityPanel.svelte`:

1. Remove `import { save } from '@tauri-apps/plugin-dialog';`.
2. Add the owner-service import if not already present (the file may not currently import it):
   ```ts
   import { OwnerService } from '../owner-service';
   const svc = new OwnerService(invoke);
   ```
   (Check the existing import block — if `OwnerService` is already imported elsewhere in the file or via a context, reuse that. The instance can be a `const` at top of `<script>`.)
3. Replace the dialog block (lines 174-184):

   **Old:**
   ```ts
   let outPath: string | null;
   try {
     outPath = await save({
       title: 'Save recovery file',
       defaultPath: 'identity.recovery',
       filters: [{ name: 'Recovery file', extensions: ['recovery'] }],
     });
   } catch {
     return;
   }
   ```

   **New:**
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
     return;  // preserve current silent-return-on-error behavior
   }
   ```

4. Update the cancel + epoch checks immediately after (lines 186-190):

   **Old:**
   ```ts
   if (wizardState !== epoch) return;
   if (!outPath) return;
   ```

   **New:**
   ```ts
   if (wizardState !== epoch) return;
   if (pathToken === null) return;
   ```

5. Update the invoke call (lines 195-200):

   **Old:**
   ```ts
   await invoke('export_recovery_file_to_path', {
     outPath,
     passphrase,
     comment: comment || null,
   });

   if (wizardState !== epoch2) return;
   wizardState = { kind: 'backup', step: { phase: 'fileSaved', savedPath: outPath } };
   ```

   **New:**
   ```ts
   const savedPath = await invoke<string>('export_recovery_file_to_path', {
     pathToken,
     passphrase,
     comment: comment || null,
   });

   if (wizardState !== epoch2) return;
   wizardState = { kind: 'backup', step: { phase: 'fileSaved', savedPath } };
   ```

   The backend now returns the path that was actually written to (Task 4 step 4 changed the IPC return type from `()` to `String`), so the wizard's `fileSaved` phase keeps its existing UI shape unchanged.

6. Update the error-path branch (lines 204-217) — the `Could not save to ${outPath}` error string previously used the path that the user picked. Since the renderer no longer knows that path on the failure path (we never receive a return value when the call throws), replace `outPath` with `'the chosen location'`:

   ```ts
   error: `Could not save to the chosen location: ${e}. Try a different location.`,
   ```

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/components/__tests__/IdentityPanel.test.ts`
Expected: PASS.

- [ ] **Step 5: Run full typecheck and full vitest**

Run: `npx tsc --noEmit && npx vitest run`
Expected: clean (all frontend tests pass, no TS errors anywhere).

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/IdentityPanel.svelte src/lib/components/__tests__/IdentityPanel.test.ts
git commit -m "feat(IdentityPanel): use backend save dialog for per-device backup wizard (ZEB-194)

Mirrors DevicesPanel refactor for the per-device transport-identity
backup. The wizard's epoch guard and silent-error semantics are
preserved. The backend now returns the saved path so the existing
'savedPath' UI feedback works unchanged.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Final Verification Gates

Run all the following from the repo root after Task 7 commits:

```bash
cd src-tauri && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test --lib
cd .. && npx tsc --noEmit && npx vitest run
```

All four must be clean. Any failure means a regression in a task's commit — fix forward (don't squash silently).

## Manual Verification (User)

These can only be exercised in a real Tauri runtime. Add to the PR Test Plan:

1. `npm run tauri dev`. Start fresh (or wipe identity).
2. **Owner backup happy path**: Mint identity → click "Back up owner identity" → choose a path in the OS dialog → enter passphrase + confirm + comment → verify file lands at chosen location with mode `0600`. Verify the saved-path UI feedback shows the actual chosen path (not the default filename).
3. **Owner backup cancel**: Click "Back up", fill passphrase, click Save, **cancel the OS dialog**. Verify modal stays open without error and Retry works.
4. **Owner backup bad passphrase**: Type a too-short passphrase. Verify validation error appears BEFORE the OS dialog opens (validation runs first by design).
5. **Identity backup happy path** (if exposed in the GUI): mirror flow on IdentityPanel's wizard.
6. **Identity backup cancel**: silent return to `fileEntry` phase per existing spec.
7. **Direct IPC fuzzing (optional, in dev console)**: invoke `export_owner_recovery_file_to_path` with a fabricated `pathToken` UUID. Confirm "Save path token expired or invalid" error and no file write.

If steps 2-7 all pass, the PR is ready to mark ready-for-review.
