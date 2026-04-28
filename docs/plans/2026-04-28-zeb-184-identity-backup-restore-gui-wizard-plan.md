# ZEB-184 — Identity Backup/Restore GUI Wizard — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a Settings → Identity panel with two-button (Backup… / Restore…) wizard wrapping the existing `recovery_cli::*_with_keychain` functions in new Tauri commands and a new `IdentityPanel.svelte` component.

**Architecture:** Thin Svelte shell over already-tested Rust logic. New `recovery_cli` word-array variants (so we don't leak the mnemonic to a temp file), then 7 new Tauri commands, then a single `IdentityPanel.svelte` component hosting both wizard flows via internal state machines.

**Tech Stack:** Svelte 5 runes (`$state`, `$derived`, `$effect`), Tauri 2 IPC (`@tauri-apps/api/core` `invoke`), `tauri-plugin-dialog` (file save/open), `vitest` + `@testing-library/svelte` for component tests, `serial_test::serial` for env-var test isolation in Rust.

**Spec:** `docs/specs/2026-04-28-zeb-184-identity-backup-restore-gui-wizard-design.md` — all design decisions and rationale.

---

## Build sequence

The 10 tasks below are ordered for dependency and review velocity:

1. **Rust word-array variants** — new functions in `recovery_cli.rs`, refactor existing functions to delegate
2. **Read-only Tauri commands** — `current_identity_hash`, `export_mnemonic_words`, `preview_mnemonic_identity`, `preview_recovery_file`
3. **Mutating Tauri commands** — `export_recovery_file_to_path`, `restore_mnemonic_from_words`, `restore_recovery_file_from_path`, plus CLI↔GUI round-trip integration test
4. **`IdentityPanel` foundation** — Svelte component skeleton, header, two buttons, identity_hash display, wire into `App.svelte`
5. **Backup-mnemonic flow** — wizard steps 1 → 2a → done
6. **Backup-recovery-file flow** — wizard steps 1 → 2b → 3b → done
7. **Restore wizard skeleton** — source picker + shared confirmation step (3) + done step (4)
8. **Restore-mnemonic flow** — step 2a (textarea, live validation, identity_hash preview) plugs into shared steps 3+4
9. **Restore-recovery-file flow** — step 2b (file picker, passphrase, decrypt, metadata display) plugs into shared steps 3+4
10. **Documentation** — update `docs/headless-install.md` Backup-and-recovery section with GUI walkthrough

---

### Task 1: Rust word-array variants in `recovery_cli.rs`

**Goal:** Add `export_mnemonic_words_with_keychain` and `restore_mnemonic_from_words_with_keychain`. Refactor existing `_cli` and `_to_writers` mnemonic functions to delegate to the new variants. Avoids leaking the mnemonic to a temp file in the GUI path.

**Files:**
- Modify: `src-tauri/src/recovery_cli.rs`
- Modify: `src-tauri/tests/recovery_cli_integration.rs`

- [ ] **Step 1: Write a failing test for `export_mnemonic_words_with_keychain`**

Add to `src-tauri/tests/recovery_cli_integration.rs`:

```rust
#[test]
#[serial]
fn export_mnemonic_words_returns_24_words() {
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");

    std::env::set_var("HARMONY_PASSPHRASE", "words-test");

    let original_seed = [0xD4u8; 32];
    plant_seed(&plaintext_path, &original_seed);

    let words = recovery_cli::export_mnemonic_words_with_keychain(&plaintext_path, None)
        .expect("export words");
    assert_eq!(words.len(), 24, "BIP39-24 produces exactly 24 words");
    for w in &words {
        assert!(!w.is_empty() && w.chars().all(|c| c.is_ascii_lowercase()),
            "each word is non-empty lowercase ASCII");
    }

    std::env::remove_var("HARMONY_PASSPHRASE");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo test --test recovery_cli_integration export_mnemonic_words_returns_24_words`

Expected: FAIL with `no function or associated item named export_mnemonic_words_with_keychain`.

- [ ] **Step 3: Implement `export_mnemonic_words_with_keychain`**

In `src-tauri/src/recovery_cli.rs`, add:

```rust
/// Read the seed from disk and convert to 24 BIP39 words. Used by the
/// GUI wizard so the words never touch a temp file. The CLI's
/// `export_mnemonic_to_writers` refactors to delegate here.
pub fn export_mnemonic_words_with_keychain(
    plaintext_path: &Path,
    keychain: Option<KeychainStore>,
) -> Result<Vec<String>, String> {
    let seed = identity::read_seed_from_disk_with_keychain(plaintext_path, keychain)?;
    let artifact = RecoveryArtifact::from_seed(*seed);
    let mnemonic = artifact.to_mnemonic();
    Ok(mnemonic.as_str().split_whitespace().map(String::from).collect())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd src-tauri && cargo test --test recovery_cli_integration export_mnemonic_words_returns_24_words`

Expected: PASS.

- [ ] **Step 5: Write a failing test for `restore_mnemonic_from_words_with_keychain`**

Add to `src-tauri/tests/recovery_cli_integration.rs`:

```rust
#[test]
#[serial]
fn restore_mnemonic_from_words_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");

    std::env::set_var("HARMONY_PASSPHRASE", "words-rt");

    let original_seed = [0xD5u8; 32];
    plant_seed(&plaintext_path, &original_seed);
    let original_id = RecoveryArtifact::from_seed(original_seed)
        .master_pubkey_bundle()
        .identity_hash();

    let words = recovery_cli::export_mnemonic_words_with_keychain(&plaintext_path, None)
        .expect("export");
    wipe_identity_store(&plaintext_path);

    recovery_cli::restore_mnemonic_from_words_with_keychain(
        &plaintext_path,
        &words,
        /*force=*/ false,
        None,
    )
    .expect("restore from words");

    let reloaded = identity::read_seed_from_disk_with_keychain(&plaintext_path, None).unwrap();
    let reloaded_id = RecoveryArtifact::from_seed(*reloaded)
        .master_pubkey_bundle()
        .identity_hash();
    assert_eq!(reloaded_id, original_id);

    std::env::remove_var("HARMONY_PASSPHRASE");
}
```

- [ ] **Step 6: Run test to verify it fails**

Run: `cd src-tauri && cargo test --test recovery_cli_integration restore_mnemonic_from_words_round_trip`

Expected: FAIL with `no function or associated item named restore_mnemonic_from_words_with_keychain`.

- [ ] **Step 7: Implement `restore_mnemonic_from_words_with_keychain`**

In `src-tauri/src/recovery_cli.rs`, add:

```rust
/// Restore the on-disk identity from a 24-word array. Refuses to
/// overwrite an existing identity unless `force` is true. The CLI's
/// `restore_mnemonic_with_keychain` (which reads from a file path)
/// refactors to read the file and delegate here.
pub fn restore_mnemonic_from_words_with_keychain(
    plaintext_path: &Path,
    words: &[String],
    force: bool,
    keychain: Option<KeychainStore>,
) -> Result<(), String> {
    if words.len() != 24 {
        return Err(format!(
            "expected 24 BIP39 words, got {}",
            words.len()
        ));
    }
    let phrase = words.join(" ");
    let mnemonic = bip39::Mnemonic::parse(&phrase)
        .map_err(|e| format!("invalid recovery phrase: {e}"))?;
    let entropy = mnemonic.to_entropy();
    let seed: [u8; 32] = entropy.try_into()
        .map_err(|_| "BIP39-24 entropy is not 32 bytes — wordlist drift?".to_string())?;
    identity::write_seed_to_disk_with_keychain(plaintext_path, &seed, force, keychain)
        .map_err(|e| e.to_string())
}
```

If the existing `restore_mnemonic_with_keychain` has a different `Mnemonic::parse` shape (e.g. uses `harmony_owner::lifecycle::RecoveryArtifact::from_mnemonic`), match that — the goal is "same parse path, different input source." Mirror whatever the existing function does and replace the file-read step.

- [ ] **Step 8: Run test to verify it passes**

Run: `cd src-tauri && cargo test --test recovery_cli_integration restore_mnemonic_from_words_round_trip`

Expected: PASS.

- [ ] **Step 9: Refactor existing `_cli` and `_to_writers` functions to delegate**

In `src-tauri/src/recovery_cli.rs`:

1. `export_mnemonic_to_writers` — replace its inline `RecoveryArtifact::from_seed(...).to_mnemonic()` derivation with a call to `export_mnemonic_words_with_keychain` followed by `words.join(" ")` for stdout output.
2. `restore_mnemonic_with_keychain` — keep its file-read step, then call `restore_mnemonic_from_words_with_keychain` instead of inlining the mnemonic→seed parse.

Goal: zero behavior change on the public CLI surface; the new variants become the canonical implementations.

- [ ] **Step 10: Run the full suite to verify no regression**

Run: `cd src-tauri && cargo test --workspace --all-targets`

Expected: all tests pass, including the 3 pre-existing `recovery_cli_integration` round-trip tests (`mnemonic_round_trip_preserves_identity_hash`, `recovery_file_round_trip_preserves_identity_hash`, `cross_encoding_equivalence_via_cli`) plus the 2 new ones.

- [ ] **Step 11: Commit**

```bash
git add src-tauri/src/recovery_cli.rs src-tauri/tests/recovery_cli_integration.rs
git commit -m "feat(recovery): word-array variants for mnemonic export/restore (ZEB-184)

Adds export_mnemonic_words_with_keychain and
restore_mnemonic_from_words_with_keychain so the GUI wizard can pass
the 24 words directly without a temp file. Existing _cli and
_to_writers functions refactor to delegate; CLI behavior unchanged."
```

---

### Task 2: Read-only Tauri commands

**Goal:** Add the four read-only Tauri commands the GUI needs: `current_identity_hash`, `export_mnemonic_words`, `preview_mnemonic_identity`, `preview_recovery_file`. Each is a thin wrapper around `recovery_cli::*_with_keychain` and friends.

**Files:**
- Modify: `src-tauri/src/lib.rs` (or create `src-tauri/src/identity_commands.rs` if `lib.rs` is already large)
- Test: Rust unit tests inside the new module + add to `tests/recovery_cli_integration.rs` for cross-module integration

- [ ] **Step 1: Locate the existing `#[tauri::command]` registration site**

Open `src-tauri/src/lib.rs` and find the `tauri::Builder::default().invoke_handler(tauri::generate_handler![...])` call. Note the surrounding code structure to match the pattern.

- [ ] **Step 2: Define the `RestoreInfo` shared response type**

In `src-tauri/src/lib.rs` (or a new `identity_commands.rs`), add:

```rust
#[derive(serde::Serialize, Clone, Debug)]
pub struct RestoreInfo {
    /// 32-char hex (16 bytes truncated BLAKE3 — `identity_hash()` returns `[u8; 16]`).
    pub identity_hash: String,
    /// Unix epoch seconds; `None` for older backups without a timestamp.
    pub minted_at: Option<u64>,
    pub comment: Option<String>,
}
```

- [ ] **Step 3: Implement `current_identity_hash`**

```rust
#[tauri::command]
fn current_identity_hash() -> Result<String, String> {
    let path = identity::resolve_path(None)?;
    let seed = identity::read_seed_from_disk_with_keychain(&path, None)?;
    let artifact = RecoveryArtifact::from_seed(*seed);
    Ok(hex::encode(artifact.master_pubkey_bundle().identity_hash()))
}
```

The `None` keychain argument means "use the default keychain backend resolution chain" — same as the CLI does at startup.

- [ ] **Step 4: Implement `export_mnemonic_words`**

```rust
#[tauri::command]
fn export_mnemonic_words() -> Result<Vec<String>, String> {
    let path = identity::resolve_path(None)?;
    recovery_cli::export_mnemonic_words_with_keychain(&path, None)
}
```

- [ ] **Step 5: Implement `preview_mnemonic_identity`**

```rust
#[tauri::command]
fn preview_mnemonic_identity(words: Vec<String>) -> Result<String, String> {
    if words.len() != 24 {
        return Err(format!("expected 24 words, got {}", words.len()));
    }
    let phrase = words.join(" ");
    let mnemonic = bip39::Mnemonic::parse(&phrase)
        .map_err(|e| format!("invalid recovery phrase: {e}"))?;
    let entropy: [u8; 32] = mnemonic.to_entropy().try_into()
        .map_err(|_| "BIP39-24 entropy is not 32 bytes".to_string())?;
    let artifact = RecoveryArtifact::from_seed(entropy);
    Ok(hex::encode(artifact.master_pubkey_bundle().identity_hash()))
}
```

If `harmony_owner::lifecycle::RecoveryArtifact::from_mnemonic` exists and is what the CLI uses, prefer that — match the CLI's parse path exactly.

- [ ] **Step 6: Implement `preview_recovery_file`**

```rust
#[tauri::command]
fn preview_recovery_file(in_path: PathBuf, passphrase: String) -> Result<RestoreInfo, String> {
    let bytes = std::fs::read(&in_path)
        .map_err(|e| format!("could not read {}: {e}", in_path.display()))?;
    let artifact = RecoveryArtifact::from_recovery_file_bytes(
        &bytes,
        secrecy::SecretString::from(passphrase),
    )
    .map_err(|e| format!("could not decrypt — passphrase incorrect or file corrupted: {e}"))?;

    Ok(RestoreInfo {
        identity_hash: hex::encode(artifact.master_pubkey_bundle().identity_hash()),
        minted_at: artifact.mint_at().to_rfc3339(),
        comment: artifact.comment().map(String::from),
    })
}
```

If the actual `harmony_owner` API for parsing recovery files is named differently (e.g. `RecoveryArtifact::decrypt` or `parse_recovery_file`), match it. The error message stays deliberately ambiguous (passphrase vs corruption) per the spec's error-handling principle.

- [ ] **Step 7: Register the four commands in the invoke handler**

```rust
tauri::Builder::default()
    .invoke_handler(tauri::generate_handler![
        // ...existing commands...
        current_identity_hash,
        export_mnemonic_words,
        preview_mnemonic_identity,
        preview_recovery_file,
    ])
```

- [ ] **Step 8: Add Rust unit tests for each command**

Add to `src-tauri/tests/recovery_cli_integration.rs` (or a new `tests/gui_commands_integration.rs`):

```rust
#[test]
#[serial]
fn current_identity_hash_returns_hex() {
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");

    // resolve_path() reads HARMONY_IDENTITY_PATH env var; point it at our tempdir.
    std::env::set_var("HARMONY_IDENTITY_PATH", &plaintext_path);
    std::env::set_var("HARMONY_PASSPHRASE", "current-id-test");

    let seed = [0xD6u8; 32];
    plant_seed(&plaintext_path, &seed);

    let hash = harmony_app::current_identity_hash().expect("hash");
    assert_eq!(hash.len(), 64, "32-byte hash → 64 hex chars");
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));

    std::env::remove_var("HARMONY_PASSPHRASE");
    std::env::remove_var("HARMONY_IDENTITY_PATH");
}
```

If `current_identity_hash` is not `pub` (because Tauri commands often aren't), expose a `pub` non-`#[tauri::command]` helper that the command delegates to — same shape `harmony_app::recovery_cli::*_with_keychain` already follows. Test the helper.

Repeat for `export_mnemonic_words`, `preview_mnemonic_identity`, `preview_recovery_file`.

- [ ] **Step 9: Run the new tests**

Run: `cd src-tauri && cargo test --test gui_commands_integration` (or wherever you placed them)

Expected: all four tests pass.

- [ ] **Step 10: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/
git commit -m "feat(commands): read-only Tauri commands for identity backup/restore (ZEB-184)

Adds current_identity_hash, export_mnemonic_words,
preview_mnemonic_identity, preview_recovery_file. All thin wrappers
around recovery_cli::*_with_keychain functions. Two-phase restore
(preview, then commit) so the GUI shows accurate hash diff before
the confirmation step."
```

---

### Task 3: Mutating Tauri commands + CLI round-trip integration

**Goal:** Add the three mutating Tauri commands (`export_recovery_file_to_path`, `restore_mnemonic_from_words`, `restore_recovery_file_from_path`). Add an integration test proving GUI-Tauri-command path round-trips with the CLI path.

**Files:**
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/tests/recovery_cli_integration.rs` (or extend `gui_commands_integration.rs`)

- [ ] **Step 1: Implement `export_recovery_file_to_path`**

```rust
#[tauri::command]
fn export_recovery_file_to_path(
    out_path: PathBuf,
    passphrase: String,
    comment: Option<String>,
) -> Result<(), String> {
    let identity_path = identity::resolve_path(None)?;
    // Set HARMONY_RECOVERY_PASSPHRASE for the duration of this call.
    // The existing recovery_cli function reads from env; the GUI passes
    // the passphrase explicitly, so we wrap.
    let prev = std::env::var("HARMONY_RECOVERY_PASSPHRASE").ok();
    std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", &passphrase);
    let result = recovery_cli::export_recovery_file_with_keychain(
        &identity_path,
        &out_path,
        comment.as_deref(),
        None,
    );
    match prev {
        Some(p) => std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", p),
        None => std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE"),
    }
    result
}
```

If `recovery_cli::export_recovery_file_with_keychain` already takes the passphrase as an argument (rather than env), drop the env-var wrapping and pass directly. The env-var wrapping is the fallback for current API shape; check `recovery_cli.rs` first.

- [ ] **Step 2: Implement `restore_mnemonic_from_words`**

```rust
#[tauri::command]
fn restore_mnemonic_from_words(words: Vec<String>) -> Result<String, String> {
    let path = identity::resolve_path(None)?;
    recovery_cli::restore_mnemonic_from_words_with_keychain(
        &path,
        &words,
        /*force=*/ true,  // GUI pre-confirms via TypeToConfirmDialog
        None,
    )?;
    let seed = identity::read_seed_from_disk_with_keychain(&path, None)?;
    let artifact = RecoveryArtifact::from_seed(*seed);
    Ok(hex::encode(artifact.master_pubkey_bundle().identity_hash()))
}
```

- [ ] **Step 3: Implement `restore_recovery_file_from_path`**

```rust
#[tauri::command]
fn restore_recovery_file_from_path(
    in_path: PathBuf,
    passphrase: String,
) -> Result<RestoreInfo, String> {
    let identity_path = identity::resolve_path(None)?;
    let prev = std::env::var("HARMONY_RECOVERY_PASSPHRASE").ok();
    std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", &passphrase);
    let result = recovery_cli::restore_recovery_file_with_keychain(
        &identity_path,
        &in_path,
        /*force=*/ true,
        None,
    );
    match prev {
        Some(p) => std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", p),
        None => std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE"),
    }
    result?;

    // Re-read the just-restored identity to populate RestoreInfo.
    // The recovery file's metadata (mint_at, comment) is also accessible
    // via preview_recovery_file; we re-decrypt to extract them since
    // restore_recovery_file_with_keychain doesn't currently return them.
    let bytes = std::fs::read(&in_path)
        .map_err(|e| format!("could not re-read recovery file for metadata: {e}"))?;
    let artifact = RecoveryArtifact::from_recovery_file_bytes(
        &bytes,
        secrecy::SecretString::from(passphrase),
    )
    .map_err(|e| format!("metadata extract failed: {e}"))?;

    Ok(RestoreInfo {
        identity_hash: hex::encode(artifact.master_pubkey_bundle().identity_hash()),
        minted_at: artifact.mint_at().to_rfc3339(),
        comment: artifact.comment().map(String::from),
    })
}
```

The double-decrypt is acceptable (~101 bytes, microseconds). If `restore_recovery_file_with_keychain` is straightforward to extend to return `(IdentityHash, RestoreInfo)`, prefer that — single decrypt.

- [ ] **Step 4: Register the three commands in the invoke handler**

```rust
.invoke_handler(tauri::generate_handler![
    // ...
    export_recovery_file_to_path,
    restore_mnemonic_from_words,
    restore_recovery_file_from_path,
])
```

- [ ] **Step 5: Write integration test for CLI ↔ GUI round-trip**

Add to `src-tauri/tests/recovery_cli_integration.rs`:

```rust
#[test]
#[serial]
fn gui_export_mnemonic_restored_via_cli_preserves_identity_hash() {
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");
    let mnemonic_path = dir.path().join("mnemonic.txt");

    std::env::set_var("HARMONY_PASSPHRASE", "gui-cli-rt-1");
    std::env::set_var("HARMONY_IDENTITY_PATH", &plaintext_path);

    let seed = [0xD7u8; 32];
    plant_seed(&plaintext_path, &seed);
    let original_id = RecoveryArtifact::from_seed(seed)
        .master_pubkey_bundle()
        .identity_hash();

    // Export via the GUI helper (returns words array)
    let words = harmony_app::export_mnemonic_words_helper().expect("export");
    std::fs::write(&mnemonic_path, words.join(" ")).unwrap();

    // Wipe + restore via CLI
    wipe_identity_store(&plaintext_path);
    recovery_cli::restore_mnemonic_with_keychain(&plaintext_path, &mnemonic_path, false, None)
        .expect("CLI restore");

    let reloaded = identity::read_seed_from_disk_with_keychain(&plaintext_path, None).unwrap();
    let reloaded_id = RecoveryArtifact::from_seed(*reloaded)
        .master_pubkey_bundle()
        .identity_hash();
    assert_eq!(reloaded_id, original_id);

    std::env::remove_var("HARMONY_PASSPHRASE");
    std::env::remove_var("HARMONY_IDENTITY_PATH");
}

#[test]
#[serial]
fn cli_export_mnemonic_restored_via_gui_preserves_identity_hash() {
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");

    std::env::set_var("HARMONY_PASSPHRASE", "cli-gui-rt-1");
    std::env::set_var("HARMONY_IDENTITY_PATH", &plaintext_path);

    let seed = [0xD8u8; 32];
    plant_seed(&plaintext_path, &seed);
    let original_id = RecoveryArtifact::from_seed(seed)
        .master_pubkey_bundle()
        .identity_hash();

    // Export via the existing CLI helper (writes to a file we then read)
    let words = recovery_cli::export_mnemonic_words_with_keychain(&plaintext_path, None)
        .expect("export");

    wipe_identity_store(&plaintext_path);

    // Restore via the GUI helper (takes words array directly)
    let restored_hash = harmony_app::restore_mnemonic_from_words_helper(words)
        .expect("GUI restore");
    assert_eq!(restored_hash, hex::encode(original_id));

    std::env::remove_var("HARMONY_PASSPHRASE");
    std::env::remove_var("HARMONY_IDENTITY_PATH");
}
```

The `_helper` functions are `pub` non-`#[tauri::command]` shims that the Tauri commands delegate to. Define them next to the commands; tests call the helpers directly. Same shape ZEB-176 used.

- [ ] **Step 6: Run the integration tests**

Run: `cd src-tauri && cargo test --test recovery_cli_integration gui_`

Expected: both new tests pass.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/recovery_cli_integration.rs
git commit -m "feat(commands): mutating Tauri commands + CLI↔GUI round-trip (ZEB-184)

Adds export_recovery_file_to_path, restore_mnemonic_from_words,
restore_recovery_file_from_path. force=true on the restore commands —
GUI pre-confirms via TypeToConfirmDialog, so the Rust-side gate is
explicit-permission acknowledgement. Integration tests prove
parity: GUI-export → CLI-restore and CLI-export → GUI-restore both
preserve identity_hash."
```

---

### Task 4: `IdentityPanel.svelte` foundation

**Goal:** Create the new component skeleton: heading, identity_hash display (8-char prefix + click-to-copy-full), two buttons ("Backup…" and "Restore…"). Wire into `App.svelte` next to `NotificationSettingsPanel` and `ProfileEditor`. No wizard flows yet — those are Tasks 5-9.

**Files:**
- Create: `src/lib/components/IdentityPanel.svelte`
- Create: `src/lib/components/__tests__/IdentityPanel.test.ts`
- Modify: `src/App.svelte` (one new import + one new render site)

- [ ] **Step 1: Write the failing test for default-state rendering**

Create `src/lib/components/__tests__/IdentityPanel.test.ts`:

```ts
import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { invoke } from '@tauri-apps/api/core';
import IdentityPanel from '../IdentityPanel.svelte';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));

const mockInvoke = vi.mocked(invoke);

describe('IdentityPanel — default state', () => {
  beforeEach(() => {
    mockInvoke.mockReset();
  });

  it('renders the truncated identity hash and two action buttons', async () => {
    const fullHash = 'a1b2c3d4'.repeat(8); // 64 hex chars
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return fullHash;
      throw new Error(`unexpected invoke: ${cmd}`);
    });

    render(IdentityPanel);

    // Wait for the async load
    await screen.findByText(/0xa1b2c3d4/);

    expect(screen.getByRole('button', { name: /backup/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /restore/i })).toBeInTheDocument();
  });

  it('copies the full identity hash to clipboard on click', async () => {
    const fullHash = 'a1b2c3d4'.repeat(8);
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return fullHash;
      throw new Error(`unexpected invoke: ${cmd}`);
    });

    const writeText = vi.fn();
    Object.defineProperty(navigator, 'clipboard', {
      value: { writeText },
      writable: true,
    });

    render(IdentityPanel);
    const hashElement = await screen.findByText(/0xa1b2c3d4/);
    await fireEvent.click(hashElement);

    expect(writeText).toHaveBeenCalledWith(fullHash);
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/IdentityPanel.test.ts`

Expected: FAIL with `Failed to resolve import "../IdentityPanel.svelte"`.

- [ ] **Step 3: Create the `IdentityPanel.svelte` skeleton**

Create `src/lib/components/IdentityPanel.svelte`:

```svelte
<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onMount } from 'svelte';

  let fullHash = $state('');
  let displayHash = $derived(fullHash ? `0x${fullHash.slice(0, 8)}…` : '…');
  let loadError = $state<string | null>(null);

  type WizardMode = 'idle' | 'backup' | 'restore';
  let mode = $state<WizardMode>('idle');

  onMount(async () => {
    try {
      fullHash = await invoke<string>('current_identity_hash');
    } catch (e) {
      loadError = `Could not read identity store: ${e}. The wizard cannot continue.`;
    }
  });

  async function copyHash() {
    if (fullHash) {
      await navigator.clipboard.writeText(fullHash);
    }
  }
</script>

{#if loadError}
  <div class="identity-panel">
    <h2>Identity</h2>
    <p class="error">{loadError}</p>
  </div>
{:else if mode === 'idle'}
  <div class="identity-panel">
    <h2>Identity</h2>
    <div class="hash-row">
      <span class="label">Identity hash</span>
      <button
        class="hash-display"
        title="Click to copy full {fullHash.length}-char hex"
        onclick={copyHash}
      >
        {displayHash}
      </button>
    </div>
    <div class="actions">
      <button onclick={() => (mode = 'backup')}>Backup…</button>
      <button onclick={() => (mode = 'restore')}>Restore…</button>
    </div>
    <p class="explainer">
      Back up your identity to a 24-word phrase or an encrypted file.
      Restore replaces your current identity — the current one becomes unrecoverable.
    </p>
  </div>
{:else if mode === 'backup'}
  <!-- TODO Task 5/6: backup wizard flows -->
  <div class="identity-panel">
    <button onclick={() => (mode = 'idle')}>← Back</button>
    <p>Backup wizard placeholder.</p>
  </div>
{:else}
  <!-- TODO Task 7/8/9: restore wizard flows -->
  <div class="identity-panel">
    <button onclick={() => (mode = 'idle')}>← Back</button>
    <p>Restore wizard placeholder.</p>
  </div>
{/if}

<style>
  .identity-panel { padding: 16px; }
  .hash-row { display: flex; align-items: center; gap: 8px; margin: 8px 0; }
  .hash-display {
    font-family: ui-monospace, monospace;
    background: #2a2c33;
    padding: 6px 10px;
    border-radius: 4px;
    border: none;
    color: inherit;
    cursor: pointer;
  }
  .hash-display:hover { background: #36393f; }
  .actions { display: flex; gap: 8px; margin: 16px 0; }
  .explainer { color: #b9bbbe; font-size: 0.85em; margin-top: 14px; }
  .error { color: #ed4245; }
</style>
```

The `<!-- TODO Task N -->` placeholders are intentional plan-time markers — they get replaced as Tasks 5-9 land. They don't violate the "no placeholders in plans" rule because the plan itself documents when each placeholder is filled in.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/IdentityPanel.test.ts`

Expected: both tests pass.

- [ ] **Step 5: Wire into `App.svelte`**

Modify `src/App.svelte`:

1. Add the import near the other component imports:
   ```ts
   import IdentityPanel from './lib/components/IdentityPanel.svelte';
   ```

2. In the settings render block (where `<NotificationSettingsPanel>` and `<ProfileEditor>` are rendered when `showSettings` is true), add:
   ```svelte
   <IdentityPanel />
   ```
   Place it between `<ProfileEditor>` and `<NotificationSettingsPanel>` — order is: Profile → Identity → Notifications. (Identity sits between profile-style and notification-style settings.)

- [ ] **Step 6: Smoke-test the dev build**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npm run dev` (in one terminal) + `cd src-tauri && cargo tauri dev` (in another).

Open the app, click the Settings icon, scroll to the Identity section. Expected: see the truncated hash and the two buttons. Click "Backup…" → see the placeholder. Click ← Back → return to default state. Click the hash → it copies (paste-test in another field).

If the dev build doesn't load the panel, check `App.svelte` for `<IdentityPanel />` placement and the `showSettings` gate.

- [ ] **Step 7: Run full vitest suite**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run`

Expected: all tests pass (including new IdentityPanel tests).

- [ ] **Step 8: Run `tsc --noEmit`**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit`

Expected: clean (CI gate from ZEB-182 will enforce this).

- [ ] **Step 9: Commit**

```bash
git add src/lib/components/IdentityPanel.svelte src/lib/components/__tests__/IdentityPanel.test.ts src/App.svelte
git commit -m "feat(identity): IdentityPanel.svelte foundation (ZEB-184)

New Settings → Identity section. Renders truncated identity_hash
(8-char prefix) with click-to-copy-full, plus Backup… / Restore…
buttons that toggle internal wizard state. Wizard flows themselves
land in subsequent tasks. Vitest covers the default-state render
and the click-to-copy."
```

---

### Task 5: Backup-mnemonic flow

**Goal:** Implement steps 1 → 2a → done of the Backup wizard. User clicks "Backup…", picks "24-word recovery phrase", reveals the words, ticks "I've stored this safely", clicks Done, returns to idle.

**Files:**
- Modify: `src/lib/components/IdentityPanel.svelte`
- Modify: `src/lib/components/__tests__/IdentityPanel.test.ts`

- [ ] **Step 1: Write a failing test for the backup type picker (step 1)**

Add to `IdentityPanel.test.ts`:

```ts
describe('Backup wizard — step 1 (type picker)', () => {
  it('shows two options when Backup… is clicked', async () => {
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      throw new Error(`unexpected: ${cmd}`);
    });

    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);

    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));

    expect(screen.getByText(/24-word recovery phrase/i)).toBeInTheDocument();
    expect(screen.getByText(/encrypted recovery file/i)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /continue/i })).toBeInTheDocument();
  });

  it('Continue button is disabled until a type is selected', async () => {
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      throw new Error(`unexpected: ${cmd}`);
    });

    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));

    const continueBtn = screen.getByRole('button', { name: /continue/i });
    expect(continueBtn).toBeDisabled();

    await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
    expect(continueBtn).not.toBeDisabled();
  });
});
```

- [ ] **Step 2: Run tests, verify failure**

Run: `npx vitest run src/lib/components/__tests__/IdentityPanel.test.ts`

Expected: new tests fail (placeholder shows instead of the picker).

- [ ] **Step 3: Implement the backup type picker (step 1)**

Replace the `{:else if mode === 'backup'}` block in `IdentityPanel.svelte`:

```svelte
{:else if mode === 'backup'}
  {#if backupStep === 1}
    <div class="identity-panel wizard">
      <button class="back" onclick={resetWizard}>← Back to settings</button>
      <h3>Choose backup type</h3>
      <fieldset>
        <legend class="visually-hidden">Backup type</legend>
        <label>
          <input type="radio" bind:group={backupType} value="mnemonic" />
          24-word recovery phrase
        </label>
        <label>
          <input type="radio" bind:group={backupType} value="file" />
          Encrypted recovery file
        </label>
      </fieldset>
      <div class="actions">
        <button onclick={resetWizard}>Cancel</button>
        <button disabled={!backupType} onclick={advanceBackup}>Continue</button>
      </div>
    </div>
  {:else if backupStep === '2a'}
    <!-- TODO Step 5: mnemonic reveal screen -->
  {/if}
{/if}
```

In `<script>`:

```ts
type BackupType = 'mnemonic' | 'file' | null;
type BackupStep = 1 | '2a' | '2b' | 'done';
let backupStep = $state<BackupStep>(1);
let backupType = $state<BackupType>(null);

function advanceBackup() {
  if (backupType === 'mnemonic') backupStep = '2a';
  else if (backupType === 'file') backupStep = '2b';
}

function resetWizard() {
  mode = 'idle';
  backupStep = 1;
  backupType = null;
  // (more state reset as flows land)
}
```

- [ ] **Step 4: Run tests, verify pass**

Run: `npx vitest run src/lib/components/__tests__/IdentityPanel.test.ts`

Expected: type-picker tests pass.

- [ ] **Step 5: Write a failing test for the mnemonic reveal screen (step 2a)**

```ts
describe('Backup wizard — step 2a (mnemonic reveal)', () => {
  it('fetches words and shows them blurred initially', async () => {
    const words = Array.from({ length: 24 }, (_, i) => `word${i + 1}`);
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      if (cmd === 'export_mnemonic_words') return words;
      throw new Error(`unexpected: ${cmd}`);
    });

    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));
    await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    // Words load and render blurred.
    await screen.findByText('word1');
    const grid = screen.getByTestId('mnemonic-grid');
    expect(grid).toHaveClass('blurred');

    // Reveal button shows.
    expect(screen.getByRole('button', { name: /reveal/i })).toBeInTheDocument();
  });

  it('Done is disabled until checkbox ticked AND grid revealed', async () => {
    const words = Array.from({ length: 24 }, (_, i) => `word${i + 1}`);
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      if (cmd === 'export_mnemonic_words') return words;
      throw new Error(`unexpected: ${cmd}`);
    });

    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));
    await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));
    await screen.findByText('word1');

    const doneBtn = screen.getByRole('button', { name: /done/i });
    expect(doneBtn).toBeDisabled();

    // Reveal first.
    await fireEvent.click(screen.getByRole('button', { name: /reveal/i }));
    expect(doneBtn).toBeDisabled();  // still disabled, checkbox not ticked

    // Tick checkbox.
    await fireEvent.click(screen.getByLabelText(/i've stored this safely/i));
    expect(doneBtn).not.toBeDisabled();
  });
});
```

- [ ] **Step 6: Run tests, verify failure**

Expected: reveal-screen tests fail.

- [ ] **Step 7: Implement the mnemonic reveal screen (step 2a)**

Replace the `{:else if backupStep === '2a'}` block:

```svelte
  {:else if backupStep === '2a'}
    {#if mnemonicWords.length === 0 && !mnemonicError}
      <p>Loading…</p>
    {:else if mnemonicError}
      <div class="error">{mnemonicError}</div>
      <button onclick={resetWizard}>Back to settings</button>
    {:else}
      <div class="identity-panel wizard">
        <button class="back" onclick={resetWizard}>← Back to settings</button>
        <p class="hash-anchor">Backing up identity {displayHash}</p>
        <p class="explainer">
          Write these 24 words down. Anyone with them can recover your
          identity. There is no way to retrieve them later.
        </p>
        <div data-testid="mnemonic-grid" class="mnemonic-grid" class:blurred={!revealed}>
          {#each mnemonicWords as w, i}
            <div class="word"><span class="num">{i + 1}.</span> {w}</div>
          {/each}
        </div>
        {#if !revealed}
          <button onclick={() => (revealed = true)}>Reveal</button>
        {:else}
          <label class="confirm-label">
            <input type="checkbox" bind:checked={storedSafely} />
            I've stored this safely
          </label>
        {/if}
        <div class="actions">
          <button onclick={resetWizard}>Cancel</button>
          <button disabled={!revealed || !storedSafely} onclick={resetWizard}>Done</button>
        </div>
      </div>
    {/if}
  {/if}
```

In `<script>`:

```ts
let mnemonicWords = $state<string[]>([]);
let mnemonicError = $state<string | null>(null);
let revealed = $state(false);
let storedSafely = $state(false);

$effect(() => {
  if (mode === 'backup' && backupStep === '2a' && mnemonicWords.length === 0 && !mnemonicError) {
    invoke<string[]>('export_mnemonic_words')
      .then(w => { mnemonicWords = w; })
      .catch(e => { mnemonicError = `Could not load recovery phrase: ${e}`; });
  }
});

// resetWizard() also clears these:
function resetWizard() {
  mode = 'idle';
  backupStep = 1;
  backupType = null;
  mnemonicWords = [];
  mnemonicError = null;
  revealed = false;
  storedSafely = false;
}
```

CSS additions:

```css
.mnemonic-grid {
  display: grid; grid-template-columns: repeat(4, 1fr); gap: 6px;
  background: #2a2c33; border-radius: 6px; padding: 12px;
  font-family: ui-monospace, monospace; font-size: 0.85em;
}
.mnemonic-grid.blurred { filter: blur(6px); user-select: none; }
.word .num { color: #72767d; margin-right: 4px; }
.confirm-label { display: flex; align-items: center; gap: 8px; margin: 12px 0; }
```

- [ ] **Step 8: Run tests, verify pass**

Expected: reveal-screen tests pass.

- [ ] **Step 9: Smoke-test the flow in `tauri dev`**

Open the app → Settings → Identity → Backup… → 24-word recovery phrase → Continue → see blurred grid → Reveal → tick checkbox → Done returns to idle.

If anything renders wrong, fix and re-test.

- [ ] **Step 10: Commit**

```bash
git add src/lib/components/IdentityPanel.svelte src/lib/components/__tests__/IdentityPanel.test.ts
git commit -m "feat(identity): backup-mnemonic wizard flow (ZEB-184)

Steps 1 → 2a → done. Type picker, blurred reveal screen, 'I've
stored this safely' checkbox gate, Done returns to idle. Mnemonic
fetched via export_mnemonic_words Tauri command. No clipboard
support, no auto-hide timer, per spec."
```

---

### Task 6: Backup-recovery-file flow

**Goal:** Implement steps 1 → 2b → 3b → done of the Backup wizard. User picks "Encrypted recovery file", types passphrase + confirm + optional comment, picks save location via Tauri save dialog, file written, success screen, returns to idle.

**Files:**
- Modify: `src/lib/components/IdentityPanel.svelte`
- Modify: `src/lib/components/__tests__/IdentityPanel.test.ts`

- [ ] **Step 1: Write a failing test for the passphrase entry screen (step 2b)**

```ts
describe('Backup wizard — step 2b (recovery file passphrase)', () => {
  it('Continue is disabled until passphrase + confirm match and are non-empty', async () => {
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      throw new Error(`unexpected: ${cmd}`);
    });

    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));
    await fireEvent.click(screen.getByLabelText(/encrypted recovery file/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    const continueBtn = screen.getByRole('button', { name: /continue/i });
    expect(continueBtn).toBeDisabled();

    await fireEvent.input(screen.getByLabelText(/^passphrase/i), { target: { value: 'hunter2' } });
    expect(continueBtn).toBeDisabled();  // confirm field empty

    await fireEvent.input(screen.getByLabelText(/confirm passphrase/i), { target: { value: 'hunter2' } });
    expect(continueBtn).not.toBeDisabled();

    // Mismatch re-disables.
    await fireEvent.input(screen.getByLabelText(/confirm passphrase/i), { target: { value: 'wrong' } });
    expect(continueBtn).toBeDisabled();
  });

  it('show/hide toggle reveals the passphrase', async () => {
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      throw new Error(`unexpected: ${cmd}`);
    });

    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));
    await fireEvent.click(screen.getByLabelText(/encrypted recovery file/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    const passField = screen.getByLabelText(/^passphrase/i) as HTMLInputElement;
    expect(passField.type).toBe('password');

    await fireEvent.click(screen.getByRole('button', { name: /show passphrase/i }));
    expect(passField.type).toBe('text');
  });
});
```

- [ ] **Step 2: Run tests, verify failure**

- [ ] **Step 3: Implement the passphrase entry screen (step 2b)**

Add the `{:else if backupStep === '2b'}` block in `IdentityPanel.svelte`:

```svelte
  {:else if backupStep === '2b'}
    <div class="identity-panel wizard">
      <button class="back" onclick={resetWizard}>← Back to settings</button>
      <p class="hash-anchor">Backing up identity {displayHash}</p>
      <h3>Recovery file passphrase</h3>
      <p class="explainer">
        This passphrase encrypts your recovery file. You'll need it to
        restore later. Don't reuse your account password — pick something
        you can remember or store in a password manager.
      </p>

      <label>
        Passphrase
        <div class="passphrase-row">
          <input
            type={showPass ? 'text' : 'password'}
            bind:value={recoveryPass}
          />
          <button
            type="button"
            aria-label={showPass ? 'Hide passphrase' : 'Show passphrase'}
            onclick={() => (showPass = !showPass)}
          >{showPass ? '🙈' : '👁'}</button>
        </div>
      </label>

      <label>
        Confirm passphrase
        <input
          type={showPass ? 'text' : 'password'}
          bind:value={recoveryPassConfirm}
        />
      </label>

      <label>
        Comment (optional)
        <input type="text" bind:value={comment} placeholder="laptop-2026-04-15" />
      </label>

      <div class="actions">
        <button onclick={resetWizard}>Cancel</button>
        <button
          disabled={!recoveryPass || recoveryPass !== recoveryPassConfirm}
          onclick={pickSaveLocation}
        >Continue</button>
      </div>
    </div>
  {:else if backupStep === '3b'}
    <!-- TODO Step 4 — save dialog + write + success -->
  {/if}
```

In `<script>`:

```ts
let recoveryPass = $state('');
let recoveryPassConfirm = $state('');
let comment = $state('');
let showPass = $state(false);
let savedPath = $state<string | null>(null);
let saveError = $state<string | null>(null);

async function pickSaveLocation() {
  // Implementation lands in step 4 of this task.
  backupStep = '3b';
}

// Extend resetWizard():
function resetWizard() {
  mode = 'idle'; backupStep = 1; backupType = null;
  mnemonicWords = []; mnemonicError = null; revealed = false; storedSafely = false;
  recoveryPass = ''; recoveryPassConfirm = ''; comment = ''; showPass = false;
  savedPath = null; saveError = null;
}
```

- [ ] **Step 4: Run tests, verify pass**

- [ ] **Step 5: Write a failing test for the save flow (step 3b)**

```ts
describe('Backup wizard — step 3b (save recovery file)', () => {
  it('writes the file and shows the saved path on success', async () => {
    const savePath = '/tmp/identity.recovery';
    mockInvoke.mockImplementation(async (cmd: string, args?: unknown) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      if (cmd === 'export_recovery_file_to_path') {
        const a = args as { outPath: string; passphrase: string; comment: string | null };
        expect(a.outPath).toBe(savePath);
        expect(a.passphrase).toBe('hunter2');
        expect(a.comment).toBe('laptop-2026-04-15');
        return undefined;
      }
      throw new Error(`unexpected: ${cmd}`);
    });

    // Mock tauri-plugin-dialog `save`
    vi.doMock('@tauri-apps/plugin-dialog', () => ({
      save: vi.fn().mockResolvedValue(savePath),
    }));

    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /backup/i }));
    await fireEvent.click(screen.getByLabelText(/encrypted recovery file/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));
    await fireEvent.input(screen.getByLabelText(/^passphrase/i), { target: { value: 'hunter2' } });
    await fireEvent.input(screen.getByLabelText(/confirm passphrase/i), { target: { value: 'hunter2' } });
    await fireEvent.input(screen.getByLabelText(/^comment/i), { target: { value: 'laptop-2026-04-15' } });
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    await screen.findByText(/wrote .* to \/tmp\/identity\.recovery/i);
    expect(screen.getByRole('button', { name: /done/i })).not.toBeDisabled();
  });
});
```

- [ ] **Step 6: Implement the save flow (step 3b)**

Update `pickSaveLocation` and add the `'3b'` block:

```ts
import { save } from '@tauri-apps/plugin-dialog';

async function pickSaveLocation() {
  const path = await save({
    title: 'Save recovery file',
    defaultPath: 'identity.recovery',
    filters: [{ name: 'Recovery file', extensions: ['recovery'] }],
  });
  if (!path) return;  // User cancelled — silent return per spec
  try {
    await invoke('export_recovery_file_to_path', {
      outPath: path,
      passphrase: recoveryPass,
      comment: comment || null,
    });
    savedPath = path;
    backupStep = '3b';
  } catch (e) {
    saveError = `Could not save to ${path}: ${e}. Try a different location.`;
    backupStep = '3b';
  }
}
```

```svelte
  {:else if backupStep === '3b'}
    <div class="identity-panel wizard">
      {#if saveError}
        <div class="error">{saveError}</div>
        <div class="actions">
          <button onclick={() => (backupStep = '2b')}>Back</button>
          <button onclick={resetWizard}>Cancel</button>
        </div>
      {:else if savedPath}
        <p>✓ Wrote recovery file to <code>{savedPath}</code></p>
        <p class="hash-anchor">Backed up identity {displayHash}</p>
        <div class="actions">
          <button onclick={resetWizard}>Done</button>
        </div>
      {/if}
    </div>
  {/if}
```

- [ ] **Step 7: Run tests, verify pass**

- [ ] **Step 8: Smoke-test the flow in `tauri dev`**

Real save dialog. Real file written. Verify file size matches expected (~101 bytes encrypted envelope).

- [ ] **Step 9: Commit**

```bash
git add src/lib/components/IdentityPanel.svelte src/lib/components/__tests__/IdentityPanel.test.ts
git commit -m "feat(identity): backup-recovery-file wizard flow (ZEB-184)

Steps 1 → 2b → 3b → done. Passphrase + confirm + optional comment;
Tauri save dialog; export_recovery_file_to_path; success screen
shows the saved path. Mismatched-confirm and empty-passphrase
gates keep Continue disabled."
```

---

### Task 7: Restore wizard skeleton (steps 1, 3, 4)

**Goal:** Implement the restore wizard's source picker (step 1), shared confirmation step (step 3 — type-to-confirm overwrite), and shared done screen (step 4). Steps 2a and 2b land in Tasks 8 and 9 — they plug into the shared 3+4 logic from this task.

**Files:**
- Modify: `src/lib/components/IdentityPanel.svelte`
- Modify: `src/lib/components/__tests__/IdentityPanel.test.ts`

- [ ] **Step 1: Write a failing test for the restore source picker (step 1)**

```ts
describe('Restore wizard — step 1 (source picker)', () => {
  it('shows two source options when Restore… is clicked', async () => {
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a'.repeat(64);
      throw new Error(`unexpected: ${cmd}`);
    });

    render(IdentityPanel);
    await screen.findByText(/0xaaaaaaaa/);
    await fireEvent.click(screen.getByRole('button', { name: /restore/i }));

    expect(screen.getByText(/24-word recovery phrase/i)).toBeInTheDocument();
    expect(screen.getByText(/recovery file/i)).toBeInTheDocument();
  });
});
```

- [ ] **Step 2: Implement the restore source picker (step 1)**

```svelte
{:else if mode === 'restore'}
  {#if restoreStep === 1}
    <div class="identity-panel wizard">
      <button class="back" onclick={resetWizard}>← Back to settings</button>
      <h3>What do you have?</h3>
      <fieldset>
        <label>
          <input type="radio" bind:group={restoreSource} value="mnemonic" />
          24-word recovery phrase
        </label>
        <label>
          <input type="radio" bind:group={restoreSource} value="file" />
          Recovery file
        </label>
      </fieldset>
      <div class="actions">
        <button onclick={resetWizard}>Cancel</button>
        <button disabled={!restoreSource} onclick={advanceRestore}>Continue</button>
      </div>
    </div>
  {:else if restoreStep === '2a'}
    <!-- TODO Task 8: mnemonic textarea -->
  {:else if restoreStep === '2b'}
    <!-- TODO Task 9: file picker + decrypt -->
  {:else if restoreStep === 3}
    <!-- shared confirmation step — implemented this task -->
  {:else if restoreStep === 4}
    <!-- shared done screen — implemented this task -->
  {/if}
{/if}
```

```ts
type RestoreSource = 'mnemonic' | 'file' | null;
type RestoreStep = 1 | '2a' | '2b' | 3 | 4;
let restoreStep = $state<RestoreStep>(1);
let restoreSource = $state<RestoreSource>(null);
let restoreCandidate = $state<{ identity_hash: string; minted_at?: string; comment?: string | null } | null>(null);
let typedPrefix = $state('');
let restoreError = $state<string | null>(null);
let postRestoreHash = $state<string | null>(null);

function advanceRestore() {
  if (restoreSource === 'mnemonic') restoreStep = '2a';
  else if (restoreSource === 'file') restoreStep = '2b';
}
```

Extend `resetWizard()`:

```ts
function resetWizard() {
  // ...existing backup state reset...
  mode = 'idle';
  restoreStep = 1; restoreSource = null;
  restoreCandidate = null; typedPrefix = ''; restoreError = null; postRestoreHash = null;
}
```

- [ ] **Step 3: Write a failing test for step 3 (confirmation)**

```ts
describe('Restore wizard — step 3 (confirmation)', () => {
  // Helper to drive the flow up to step 3 with a fake candidate.
  async function arrangeAtStep3(candidateHash: string) {
    // Tasks 8/9 will call setRestoreCandidateAndAdvance; for this test,
    // we use a test-only props seam if needed, OR we simulate the mnemonic
    // textarea path which sets restoreCandidate then advances to step 3.
    // For the bare step-3 test, render with the component pre-populated:
    const { component } = render(IdentityPanel, {
      props: {
        // Test seam — controllable via $bindable or initial-state prop.
      },
    });
    // ...drive to step 3...
    // (Implementation details depend on the test seam chosen.)
  }

  it('Replace identity disabled until typed prefix matches current hash', async () => {
    const currentHash = 'a1b2c3d4'.repeat(8);
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return currentHash;
      throw new Error(`unexpected: ${cmd}`);
    });

    // Same setup as above — once we have a 2a or 2b path, drive through it.
    // This test will land integrated with Task 8 (the first concrete 2a path).
    // For now, verify the inline component logic via a pure-function test if needed.
  });
});
```

The Step-3 test fully integrates only after Task 8 lands (2a is the first concrete path that populates `restoreCandidate`). Mark this test as `it.skip` for now and revisit at the end of Task 8. The IMPLEMENTATION of step 3 still happens in this task.

- [ ] **Step 4: Implement step 3 (confirmation)**

```svelte
  {:else if restoreStep === 3}
    <div class="identity-panel wizard">
      <button class="back" onclick={() => (restoreStep = restoreSource === 'mnemonic' ? '2a' : '2b')}>← Back</button>
      <h3>Confirm overwrite</h3>
      <p class="warning">
        This will <strong>replace your current identity</strong>. Your current
        identity will be unrecoverable after this step.
      </p>
      <div class="hash-diff">
        <div>
          <span class="label">Current</span>
          <code>0x{fullHash.slice(0, 8)}…</code>
        </div>
        <span class="arrow">→</span>
        <div>
          <span class="label">Restored</span>
          <code>0x{restoreCandidate?.identity_hash.slice(0, 8)}…</code>
        </div>
      </div>
      <label class="confirm-prompt">
        Type the first 8 chars of your <strong>current</strong> identity hash:
        <small>({fullHash.slice(0, 8)})</small>
        <input
          type="text"
          bind:value={typedPrefix}
          autocomplete="off"
          spellcheck="false"
        />
      </label>
      {#if typedPrefix && typedPrefix !== fullHash.slice(0, 8)}
        <p class="error">That doesn't match your current identity hash.</p>
      {/if}
      <div class="actions">
        <button onclick={resetWizard}>Cancel</button>
        <button
          disabled={typedPrefix !== fullHash.slice(0, 8)}
          onclick={commitRestore}
        >Replace identity</button>
      </div>
    </div>
```

```ts
async function commitRestore() {
  try {
    if (restoreSource === 'mnemonic') {
      const newHash = await invoke<string>('restore_mnemonic_from_words', { words: pendingWords });
      postRestoreHash = newHash;
    } else {
      const info = await invoke<{ identity_hash: string }>('restore_recovery_file_from_path', {
        inPath: pendingFilePath,
        passphrase: recoveryPass,
      });
      postRestoreHash = info.identity_hash;
    }
    restoreStep = 4;
  } catch (e) {
    restoreError = `Restore failed: ${e}. Your current identity is unchanged.`;
  }
}
```

`pendingWords` and `pendingFilePath` are state set by Tasks 8/9. Declare them as `$state<string[]>([])` and `$state<string>('')` respectively in this task — just empty placeholders the next tasks fill in.

- [ ] **Step 5: Implement step 4 (done)**

```svelte
  {:else if restoreStep === 4}
    <div class="identity-panel wizard">
      <p>✓ Identity restored.</p>
      <div class="hash-anchor">
        <span class="label">New identity hash</span>
        <button class="hash-display" onclick={() => navigator.clipboard.writeText(postRestoreHash || '')}>
          0x{postRestoreHash?.slice(0, 8)}…
        </button>
      </div>
      <p class="explainer">
        Verify this matches what you expected. If it doesn't match your
        backup's expected hash, restore again from the correct backup
        before performing any other action.
      </p>
      <div class="actions">
        <button onclick={async () => {
          // Refresh fullHash from backend so the panel returns to a coherent state.
          fullHash = await invoke<string>('current_identity_hash');
          resetWizard();
        }}>Done</button>
      </div>
    </div>
  {/if}
```

- [ ] **Step 6: Run tests, verify the source-picker test passes; step-3 test stays skipped until Task 8**

- [ ] **Step 7: Commit**

```bash
git add src/lib/components/IdentityPanel.svelte src/lib/components/__tests__/IdentityPanel.test.ts
git commit -m "feat(identity): restore wizard skeleton — steps 1, 3, 4 (ZEB-184)

Source picker (mnemonic vs file) and the SHARED confirmation +
done steps. Step 2a/2b land in subsequent tasks and feed into the
shared 3+4 via restoreCandidate / pendingWords / pendingFilePath
state. Type-to-confirm input requires first 8 chars of CURRENT
identity_hash (not the restored one)."
```

---

### Task 8: Restore-mnemonic flow (step 2a)

**Goal:** Implement the mnemonic textarea step. User pastes 24 words, sees live validation (count, wordlist membership, checksum), sees a preview of the restored identity_hash, clicks Continue → advances to shared step 3.

**Files:**
- Modify: `src/lib/components/IdentityPanel.svelte`
- Modify: `src/lib/components/__tests__/IdentityPanel.test.ts`

- [ ] **Step 1: Write a failing test for the textarea + validation**

```ts
describe('Restore wizard — step 2a (mnemonic textarea)', () => {
  it('shows word count and validates on Continue click', async () => {
    const newHash = 'b2c3d4e5'.repeat(8);
    mockInvoke.mockImplementation(async (cmd: string, args?: unknown) => {
      if (cmd === 'current_identity_hash') return 'a1b2c3d4'.repeat(8);
      if (cmd === 'preview_mnemonic_identity') {
        const a = args as { words: string[] };
        if (a.words.length !== 24) throw new Error(`expected 24 words, got ${a.words.length}`);
        return newHash;
      }
      throw new Error(`unexpected: ${cmd}`);
    });

    render(IdentityPanel);
    await screen.findByText(/0xa1b2c3d4/);
    await fireEvent.click(screen.getByRole('button', { name: /restore/i }));
    await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    const textarea = screen.getByRole('textbox', { name: /recovery phrase/i });

    await fireEvent.input(textarea, { target: { value: 'only three words' } });
    expect(screen.getByText(/3 \/ 24 words/i)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /continue/i })).toBeDisabled();

    await fireEvent.input(textarea, {
      target: { value: Array(24).fill('witness').join(' ') },
    });
    expect(screen.getByText(/24 \/ 24 words/i)).toBeInTheDocument();
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    // Step 2a calls preview_mnemonic_identity, populates restoreCandidate, advances to step 3.
    await screen.findByText(/0xb2c3d4e5/);  // restored hash diff in step 3
    expect(screen.getByText(/replace your current identity/i)).toBeInTheDocument();
  });

  it('renders inline error on bad checksum', async () => {
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a1b2c3d4'.repeat(8);
      if (cmd === 'preview_mnemonic_identity') {
        throw new Error("invalid recovery phrase: failed checksum");
      }
      throw new Error(`unexpected: ${cmd}`);
    });

    render(IdentityPanel);
    await screen.findByText(/0xa1b2c3d4/);
    await fireEvent.click(screen.getByRole('button', { name: /restore/i }));
    await fireEvent.click(screen.getByLabelText(/24-word recovery phrase/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    const textarea = screen.getByRole('textbox', { name: /recovery phrase/i });
    await fireEvent.input(textarea, { target: { value: Array(24).fill('witness').join(' ') } });
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    await screen.findByText(/don't form a valid recovery phrase/i);
  });
});
```

- [ ] **Step 2: Run tests, verify failure**

- [ ] **Step 3: Implement step 2a**

```svelte
  {:else if restoreStep === '2a'}
    <div class="identity-panel wizard">
      <button class="back" onclick={resetWizard}>← Back to settings</button>
      <h3>Restore from recovery phrase</h3>
      <label>
        Recovery phrase
        <textarea
          bind:value={mnemonicInput}
          placeholder="Paste your 24 words here. Spaces or newlines are fine."
          rows="6"
        ></textarea>
      </label>
      <p class="word-count">{wordCount} / 24 words</p>
      {#if mnemonicValidationError}
        <p class="error">{mnemonicValidationError}</p>
      {/if}
      <div class="actions">
        <button onclick={resetWizard}>Cancel</button>
        <button disabled={wordCount !== 24} onclick={previewMnemonic}>Continue</button>
      </div>
    </div>
```

```ts
let mnemonicInput = $state('');
let mnemonicValidationError = $state<string | null>(null);
let pendingWords = $state<string[]>([]);

let wordCount = $derived(
  mnemonicInput.split(/\s+/).filter(w => w.length > 0).length
);

async function previewMnemonic() {
  mnemonicValidationError = null;
  const words = mnemonicInput.split(/\s+/).filter(w => w.length > 0);
  if (words.length !== 24) {
    mnemonicValidationError = `Need exactly 24 words; you entered ${words.length}.`;
    return;
  }
  try {
    const candidateHash = await invoke<string>('preview_mnemonic_identity', { words });
    restoreCandidate = { identity_hash: candidateHash };
    pendingWords = words;
    restoreStep = 3;
  } catch (e) {
    const msg = String(e);
    if (/checksum/i.test(msg)) {
      mnemonicValidationError = "These 24 words don't form a valid recovery phrase. Double-check your transcription.";
    } else if (/wordlist|not.*word/i.test(msg)) {
      mnemonicValidationError = "One or more words isn't a recognized recovery word.";
    } else {
      mnemonicValidationError = `Could not parse recovery phrase: ${msg}`;
    }
  }
}
```

Extend `resetWizard()` to clear `mnemonicInput`, `mnemonicValidationError`, `pendingWords`.

- [ ] **Step 4: Run tests, verify pass**

- [ ] **Step 5: Un-skip the step-3 test from Task 7 (now reachable)**

The previous `it.skip` from Task 7 step 3 — re-enable it, since the mnemonic path now drives through to step 3 with a populated `restoreCandidate`. Adjust the test to use the new path.

- [ ] **Step 6: Smoke-test the flow in `tauri dev`**

Restore mnemonic flow end-to-end. Try a known-good 24-word phrase from a previous backup; try a deliberately-broken 24 words; try fewer than 24 words.

- [ ] **Step 7: Commit**

```bash
git add src/lib/components/IdentityPanel.svelte src/lib/components/__tests__/IdentityPanel.test.ts
git commit -m "feat(identity): restore-mnemonic flow step 2a (ZEB-184)

Textarea with live word count, preview_mnemonic_identity for
validation + identity_hash preview, advances to shared step 3 with
restoreCandidate populated. Inline errors map BIP39 checksum and
wordlist failures to friendly messages."
```

---

### Task 9: Restore-recovery-file flow (step 2b)

**Goal:** Implement the file-picker + passphrase + decrypt step. User picks a `.recovery` file via Tauri open dialog, types passphrase, clicks Decrypt → previews `RestoreInfo` (identity_hash + minted_at + comment), clicks Continue → advances to shared step 3.

**Files:**
- Modify: `src/lib/components/IdentityPanel.svelte`
- Modify: `src/lib/components/__tests__/IdentityPanel.test.ts`

- [ ] **Step 1: Write a failing test for the file picker + decrypt**

```ts
describe('Restore wizard — step 2b (recovery file)', () => {
  it('decrypts and shows metadata; advances to step 3 on Continue', async () => {
    const filePath = '/tmp/identity.recovery';
    const newHash = 'b2c3d4e5'.repeat(8);
    mockInvoke.mockImplementation(async (cmd: string, args?: unknown) => {
      if (cmd === 'current_identity_hash') return 'a1b2c3d4'.repeat(8);
      if (cmd === 'preview_recovery_file') {
        const a = args as { inPath: string; passphrase: string };
        expect(a.inPath).toBe(filePath);
        expect(a.passphrase).toBe('hunter2');
        return {
          identity_hash: newHash,
          minted_at: '2026-04-15T18:32:11Z',
          comment: 'laptop-2026-04-15',
        };
      }
      throw new Error(`unexpected: ${cmd}`);
    });

    vi.doMock('@tauri-apps/plugin-dialog', () => ({
      open: vi.fn().mockResolvedValue(filePath),
    }));

    render(IdentityPanel);
    await screen.findByText(/0xa1b2c3d4/);
    await fireEvent.click(screen.getByRole('button', { name: /restore/i }));
    await fireEvent.click(screen.getByLabelText(/recovery file/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    await fireEvent.click(screen.getByRole('button', { name: /pick recovery file/i }));
    await fireEvent.input(screen.getByLabelText(/passphrase/i), { target: { value: 'hunter2' } });
    await fireEvent.click(screen.getByRole('button', { name: /decrypt/i }));

    await screen.findByText(/2026-04-15T18:32:11Z/);
    await screen.findByText(/laptop-2026-04-15/);
    expect(screen.getByText(/0xb2c3d4e5/i)).toBeInTheDocument();

    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));
    await screen.findByText(/replace your current identity/i);
  });

  it('shows ambiguous error on AEAD failure (passphrase or corruption)', async () => {
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') return 'a1b2c3d4'.repeat(8);
      if (cmd === 'preview_recovery_file') {
        throw new Error("could not decrypt — passphrase incorrect or file corrupted");
      }
      throw new Error(`unexpected: ${cmd}`);
    });

    vi.doMock('@tauri-apps/plugin-dialog', () => ({
      open: vi.fn().mockResolvedValue('/tmp/bad.recovery'),
    }));

    render(IdentityPanel);
    await screen.findByText(/0xa1b2c3d4/);
    await fireEvent.click(screen.getByRole('button', { name: /restore/i }));
    await fireEvent.click(screen.getByLabelText(/recovery file/i));
    await fireEvent.click(screen.getByRole('button', { name: /continue/i }));

    await fireEvent.click(screen.getByRole('button', { name: /pick recovery file/i }));
    await fireEvent.input(screen.getByLabelText(/passphrase/i), { target: { value: 'wrong' } });
    await fireEvent.click(screen.getByRole('button', { name: /decrypt/i }));

    await screen.findByText(/could not decrypt — passphrase incorrect or file corrupted/i);
  });
});
```

- [ ] **Step 2: Implement step 2b**

```svelte
  {:else if restoreStep === '2b'}
    <div class="identity-panel wizard">
      <button class="back" onclick={resetWizard}>← Back to settings</button>
      <h3>Restore from recovery file</h3>
      <button onclick={pickRecoveryFile}>Pick recovery file…</button>
      {#if pendingFilePath}
        <p>File: <code>{pendingFilePath}</code></p>
        <label>
          Passphrase
          <input type="password" bind:value={recoveryPass} />
        </label>
        <button onclick={decryptRecoveryFile}>Decrypt</button>
      {/if}
      {#if restoreCandidate}
        <p>✓ Decrypted <code>{pendingFilePath}</code></p>
        <p class="hash-anchor">Restored identity hash: <code>0x{restoreCandidate.identity_hash.slice(0,8)}…</code></p>
        <div class="metadata">
          <p>Backup metadata</p>
          {#if restoreCandidate.minted_at}<p>Minted: {restoreCandidate.minted_at}</p>{/if}
          {#if restoreCandidate.comment}<p>Comment: {restoreCandidate.comment}</p>{/if}
        </div>
        <div class="actions">
          <button onclick={resetWizard}>Cancel</button>
          <button onclick={() => (restoreStep = 3)}>Continue</button>
        </div>
      {/if}
      {#if restoreError}
        <p class="error">{restoreError}</p>
      {/if}
    </div>
```

```ts
import { open } from '@tauri-apps/plugin-dialog';

let pendingFilePath = $state('');

async function pickRecoveryFile() {
  const path = await open({
    multiple: false,
    filters: [{ name: 'Recovery file', extensions: ['recovery'] }],
  });
  if (typeof path === 'string') {
    pendingFilePath = path;
    restoreCandidate = null;  // reset previous decrypt attempt
    restoreError = null;
  }
}

async function decryptRecoveryFile() {
  restoreError = null;
  try {
    const info = await invoke<{ identity_hash: string; minted_at: string; comment: string | null }>(
      'preview_recovery_file',
      { inPath: pendingFilePath, passphrase: recoveryPass }
    );
    restoreCandidate = info;
  } catch (e) {
    restoreError = String(e);
  }
}
```

Extend `resetWizard()` to clear `pendingFilePath`.

- [ ] **Step 3: Run tests, verify pass**

- [ ] **Step 4: Smoke-test the flow in `tauri dev`**

Use a real recovery file written by the backup flow from Task 6. Verify the metadata shows up correctly. Try an intentionally-wrong passphrase. Try a non-recovery file.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/IdentityPanel.svelte src/lib/components/__tests__/IdentityPanel.test.ts
git commit -m "feat(identity): restore-recovery-file flow step 2b (ZEB-184)

Tauri open dialog → passphrase → preview_recovery_file decrypt →
metadata display (minted_at + comment from folded ZEB-180) →
advance to shared step 3. Inline AEAD failure error is deliberately
ambiguous per spec (don't oracle bad-passphrase vs corruption)."
```

---

### Task 10: Documentation

**Goal:** Update `docs/headless-install.md` Backup-and-recovery section with a GUI walkthrough alongside the existing CLI commands. Optionally rename or split into `docs/identity-backup.md` if the headless-install doc gets too long.

**Files:**
- Modify: `docs/headless-install.md`
- (Possibly create: `docs/identity-backup.md`)

- [ ] **Step 1: Read the current state of `docs/headless-install.md`**

Run: `cat docs/headless-install.md` — find the existing Backup-and-recovery section that ZEB-176 added. Note its structure and tone.

- [ ] **Step 2: Decide structure**

If the existing section is short (~30 lines or less), append a "GUI walkthrough" subsection after the existing "CLI walkthrough" subsection. If it's already long, split into a new `docs/identity-backup.md` and link from `headless-install.md`.

- [ ] **Step 3: Write the GUI walkthrough**

Cover, for each of the four flows (backup-mnemonic, backup-file, restore-mnemonic, restore-file):
- Where to find it (Settings → Identity → Backup… or Restore…)
- Each step the user goes through, in sentences (no screenshots required for the v1 doc; a follow-up can add them once real screenshots are available)
- Where the artifact ends up (mnemonic = your responsibility; file = your chosen save location)
- The verification step (compare identity_hash post-restore)

Cross-reference the CLI walkthrough so users know either path works. Add a note that GUI-exported and CLI-exported artifacts are interchangeable (proven by the integration tests in Task 3).

- [ ] **Step 4: Run a markdown lint pass**

If the repo uses any markdown linter (check `package.json` scripts), run it. If not, manually proofread for broken links, code-block fence consistency, and heading hierarchy.

- [ ] **Step 5: Commit**

```bash
git add docs/
git commit -m "docs(identity): GUI walkthrough for backup/restore wizard (ZEB-184)

Adds GUI walkthrough alongside the existing CLI walkthrough. Covers
all four flows; cross-references the CLI for interchangeability.
Closes the documentation half of ZEB-184's DoD."
```

---

## After all tasks

Run the full local verification (the same gates CI enforces):

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --no-deps
cargo test --locked --workspace --all-targets

cd ..
npx tsc --noEmit
npx vitest run
```

All five must exit 0.

Then run the manual smoke-test checklist from the spec's "Manual smoke" section:
1. macOS: real Keychain, real Tauri dialogs, all four flows, mnemonic + file round-trip
2. Linux (libsecret available): same checklist
3. Windows (Credential Manager): same checklist
4. Cross-platform: export on macOS, restore on Linux. Same identity_hash.

Open the PR with the manual-smoke checklist filled in. Reference the spec at `docs/specs/2026-04-28-zeb-184-identity-backup-restore-gui-wizard-design.md`.
