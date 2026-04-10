# Identity & Keychain Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Store harmony-client's private identity keys in the OS-native keychain with file-based fallback and auto-migration.

**Architecture:** A `KeyStore` trait with two implementations: `KeychainStore` (via `keyring` crate) and `FileStore` (extracted from current code). A resolution chain in `load_or_generate()` tries keychain first, migrates existing files, and falls back to file storage when no keychain is available.

**Tech Stack:** Rust, `keyring` 3.x (apple-native, windows-native, linux-native features), `zeroize`, harmony-identity crate

**Spec:** `docs/specs/2026-04-10-identity-keychain-design.md`

---

## File Structure

### Modified Files

| File | Changes |
|------|---------|
| `src-tauri/src/identity.rs` | Extract `FileStore`, add `KeychainStore`, `KeyStore` trait, new resolution chain in `load_or_generate()` |
| `src-tauri/Cargo.toml` | Add `keyring` dependency |

### Unchanged Files

| File | Reason |
|------|--------|
| `src-tauri/src/lib.rs` | Still calls `identity::load_or_generate()` — same public API |
| `src-tauri/src/event_loop.rs` | No identity changes |
| All frontend code | `get_node_addr()` works identically |

---

## Task 1: Add keyring dependency

**Files:**
- Modify: `src-tauri/Cargo.toml`

- [ ] **Step 1: Add keyring to Cargo.toml**

Add after the `tracing` dependency (line 25):

```toml
keyring = { version = "3", features = ["apple-native", "windows-native", "linux-native"] }
```

- [ ] **Step 2: Verify it compiles**

Run: `cd src-tauri && cargo check`
Expected: compiles with no new errors (pre-existing warnings OK)

- [ ] **Step 3: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "deps: add keyring crate for OS-native keychain access"
```

---

## Task 2: Extract FileStore and define KeyStore trait

Extract the current file-based I/O into a `FileStore` struct and define the `KeyStore` trait that both backends will implement. No behavior change — this is a pure refactor.

**Files:**
- Modify: `src-tauri/src/identity.rs`

- [ ] **Step 1: Write failing tests for FileStore**

Add at the bottom of `identity.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        let store = FileStore::new(path.clone());

        let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
        let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
        let identity = NodeIdentity {
            pq: pq,
            ed25519: ed25519,
        };

        store.save(&identity).unwrap();
        let loaded = store.load().unwrap().expect("should find saved identity");
        assert_eq!(
            loaded.ed25519.public_identity().address_hash,
            identity.ed25519.public_identity().address_hash,
        );
        assert_eq!(
            loaded.pq.public_identity().address_hash,
            identity.pq.public_identity().address_hash,
        );
    }

    #[test]
    fn file_store_load_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.key");
        let store = FileStore::new(path);
        assert!(store.load().unwrap().is_none());
    }
}
```

- [ ] **Step 2: Add tempfile dev-dependency**

In `src-tauri/Cargo.toml`, add at the bottom:

```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cd src-tauri && cargo test -p harmony-app -- tests::file_store`
Expected: FAIL — `FileStore` not defined yet

- [ ] **Step 4: Define the KeyStore trait and FileStore struct**

Replace the contents of `identity.rs` with the refactored version. The existing `load`, `save`, `load_or_generate`, `resolve_path`, and `warn_permissions` functions are reorganized — file I/O moves into `FileStore`, the public API stays the same.

```rust
//! Node identity management — Ed25519 + post-quantum key generation and persistence.
//!
//! Two storage backends behind a common `KeyStore` trait:
//! - `KeychainStore` — OS-native keychain via the `keyring` crate
//! - `FileStore`     — binary file at `~/.harmony/identity.key`
//!
//! `load_or_generate()` tries keychain first, migrates existing files,
//! and falls back to file storage when no keychain is available.

use std::path::{Path, PathBuf};

use harmony_identity::{PqPrivateIdentity, PrivateIdentity};
use zeroize::Zeroizing;

const VERSION: u8 = 0x01;
const PQ_KEY_LEN: usize = 96;
const ED25519_KEY_LEN: usize = 64;
const BLOB_LEN: usize = 1 + PQ_KEY_LEN + ED25519_KEY_LEN; // 161

pub struct NodeIdentity {
    pub pq: PqPrivateIdentity,
    pub ed25519: PrivateIdentity,
}

impl std::fmt::Debug for NodeIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeIdentity")
            .field("pq_address", &self.pq.public_identity().address_hash)
            .field(
                "ed25519_address",
                &self.ed25519.public_identity().address_hash,
            )
            .finish()
    }
}

// ── Serialization helpers (shared by both backends) ─────────────────────

/// Serialize a `NodeIdentity` into the 161-byte binary format.
fn identity_to_blob(identity: &NodeIdentity) -> Zeroizing<Vec<u8>> {
    let pq_bytes = Zeroizing::new(identity.pq.to_private_bytes());
    let ed_bytes = Zeroizing::new(identity.ed25519.to_private_bytes());
    let mut buf = Zeroizing::new(Vec::with_capacity(BLOB_LEN));
    buf.push(VERSION);
    buf.extend_from_slice(&pq_bytes);
    buf.extend_from_slice(ed_bytes.as_slice());
    buf
}

/// Deserialize a `NodeIdentity` from a 161-byte binary blob.
fn blob_to_identity(buf: &[u8]) -> Result<NodeIdentity, String> {
    if buf.len() != BLOB_LEN {
        return Err(format!(
            "Corrupt identity blob: expected {BLOB_LEN} bytes, got {}",
            buf.len()
        ));
    }
    if buf[0] != VERSION {
        return Err(format!(
            "Unsupported identity blob version: {:#04x}",
            buf[0]
        ));
    }
    let pq = PqPrivateIdentity::from_private_bytes(&buf[1..1 + PQ_KEY_LEN])
        .map_err(|e| format!("Corrupt PQ identity: {e}"))?;
    let ed25519 = PrivateIdentity::from_private_bytes(&buf[1 + PQ_KEY_LEN..])
        .map_err(|e| format!("Corrupt Ed25519 identity: {e}"))?;
    Ok(NodeIdentity { pq, ed25519 })
}

// ── KeyStore trait ──────────────────────────────────────────────────────

/// Common interface for identity storage backends.
pub trait KeyStore {
    /// Load identity from this store. Returns `Ok(None)` if no entry exists.
    fn load(&self) -> Result<Option<NodeIdentity>, String>;
    /// Save identity to this store.
    fn save(&self, identity: &NodeIdentity) -> Result<(), String>;
}

// ── FileStore ───────────────────────────────────────────────────────────

/// File-based identity storage at a given path.
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Rename the identity file to `.bak` (used during migration).
    pub fn rename_to_backup(&self) -> Result<(), String> {
        let bak = self.path.with_extension("key.bak");
        std::fs::rename(&self.path, &bak).map_err(|e| {
            format!(
                "Failed to rename {} → {}: {e}",
                self.path.display(),
                bak.display()
            )
        })
    }
}

impl KeyStore for FileStore {
    fn load(&self) -> Result<Option<NodeIdentity>, String> {
        if !self.path.exists() {
            return Ok(None);
        }
        let buf = Zeroizing::new(
            std::fs::read(&self.path)
                .map_err(|e| format!("Failed to read {}: {e}", self.path.display()))?,
        );
        let identity = blob_to_identity(&buf)?;
        #[cfg(unix)]
        warn_permissions(&self.path);
        Ok(Some(identity))
    }

    fn save(&self, identity: &NodeIdentity) -> Result<(), String> {
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        let blob = identity_to_blob(identity);

        // Atomic write: tmp file with restricted permissions → fsync → rename.
        let tmp_path = {
            let mut name = self.path.file_name().unwrap_or_default().to_os_string();
            name.push(".tmp");
            self.path.with_file_name(name)
        };

        struct TmpGuard<'a>(&'a Path);
        impl Drop for TmpGuard<'_> {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(self.0);
            }
        }
        let guard = TmpGuard(&tmp_path);

        {
            #[cfg(unix)]
            let f = {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&tmp_path)
                    .map_err(|e| format!("Failed to create {}: {e}", tmp_path.display()))?
            };
            #[cfg(not(unix))]
            let f = {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&tmp_path)
                    .map_err(|e| format!("Failed to create {}: {e}", tmp_path.display()))?
            };
            use std::io::Write;
            (&f)
                .write_all(&blob)
                .map_err(|e| format!("Failed to write {}: {e}", tmp_path.display()))?;
            f.sync_all()
                .map_err(|e| format!("Failed to fsync {}: {e}", tmp_path.display()))?;
        }
        std::fs::rename(&tmp_path, &self.path).map_err(|e| {
            format!(
                "Failed to rename {} → {}: {e}",
                tmp_path.display(),
                self.path.display()
            )
        })?;
        std::mem::forget(guard);
        Ok(())
    }
}

// ── Public API (unchanged shape) ────────────────────────────────────────

/// Resolve the identity file path. Uses `~/.harmony/identity.key` by default.
pub fn resolve_path(override_path: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(p) = override_path {
        return Ok(p.to_path_buf());
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| {
            "Cannot determine identity file path: neither $HOME nor $USERPROFILE is set"
                .to_string()
        })?;
    Ok(PathBuf::from(home).join(".harmony").join("identity.key"))
}

/// Load identity from file, or generate and save a new one if it doesn't exist.
pub fn load_or_generate(path: &Path) -> Result<NodeIdentity, String> {
    let file_store = FileStore::new(path.to_path_buf());
    if let Some(identity) = file_store.load()? {
        return Ok(identity);
    }
    let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
    let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
    let identity = NodeIdentity { pq, ed25519 };
    file_store.save(&identity)?;
    Ok(identity)
}

#[cfg(unix)]
fn warn_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            tracing::warn!(
                path = %path.display(),
                mode = format!("{mode:#05o}"),
                "identity file has open permissions, should be 0600"
            );
        }
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd src-tauri && cargo test -p harmony-app -- tests::file_store`
Expected: both `file_store_round_trip` and `file_store_load_returns_none_when_missing` PASS

- [ ] **Step 6: Run full Rust test suite**

Run: `cd src-tauri && cargo test -p harmony-app`
Expected: all existing tests still pass (no regressions from the refactor)

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/identity.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "refactor(identity): extract FileStore and KeyStore trait"
```

---

## Task 3: Implement KeychainStore

Add the `KeychainStore` that wraps `keyring::Entry` for OS-native keychain access.

**Files:**
- Modify: `src-tauri/src/identity.rs`

- [ ] **Step 1: Write failing tests for KeychainStore**

Add to the `tests` module in `identity.rs`:

```rust
    #[test]
    fn keychain_store_round_trip() {
        let store = KeychainStore::new_mock();

        let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
        let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
        let identity = NodeIdentity { pq, ed25519 };

        store.save(&identity).unwrap();
        let loaded = store.load().unwrap().expect("should find saved identity");
        assert_eq!(
            loaded.ed25519.public_identity().address_hash,
            identity.ed25519.public_identity().address_hash,
        );
        assert_eq!(
            loaded.pq.public_identity().address_hash,
            identity.pq.public_identity().address_hash,
        );
    }

    #[test]
    fn keychain_store_load_returns_none_when_empty() {
        let store = KeychainStore::new_mock();
        assert!(store.load().unwrap().is_none());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo test -p harmony-app -- tests::keychain_store`
Expected: FAIL — `KeychainStore` not defined yet

- [ ] **Step 3: Implement KeychainStore**

Add this block to `identity.rs`, after the `FileStore` implementation and before the "Public API" section:

```rust
// ── KeychainStore ───────────────────────────────────────────────────────

const KEYCHAIN_SERVICE: &str = "harmony";
const KEYCHAIN_ACCOUNT: &str = "identity";

/// OS-native keychain storage via the `keyring` crate.
pub struct KeychainStore {
    entry: keyring::Entry,
}

impl KeychainStore {
    /// Create a store backed by the real OS keychain.
    pub fn new() -> Result<Self, String> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .map_err(|e| format!("keychain entry creation failed: {e}"))?;
        Ok(Self { entry })
    }

    /// Create a store backed by the keyring mock credential store (for tests).
    #[cfg(test)]
    pub fn new_mock() -> Self {
        use keyring::credential::CredentialPersistence;
        let entry = keyring::Entry::new_with_credential(
            Box::new(keyring::mock::MockCredential::new_with_persistence(
                KEYCHAIN_SERVICE,
                KEYCHAIN_ACCOUNT,
                CredentialPersistence::UntilDelete,
            )),
        );
        Self { entry }
    }
}

impl KeyStore for KeychainStore {
    fn load(&self) -> Result<Option<NodeIdentity>, String> {
        match self.entry.get_secret() {
            Ok(bytes) => {
                let buf = Zeroizing::new(bytes);
                let identity = blob_to_identity(&buf)?;
                Ok(Some(identity))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("keychain load failed: {e}")),
        }
    }

    fn save(&self, identity: &NodeIdentity) -> Result<(), String> {
        let blob = identity_to_blob(identity);
        self.entry
            .set_secret(&blob)
            .map_err(|e| format!("keychain save failed: {e}"))
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo test -p harmony-app -- tests::keychain_store`
Expected: both PASS

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/identity.rs
git commit -m "feat(identity): add KeychainStore backed by keyring crate"
```

---

## Task 4: Implement resolution chain with migration

Replace the `load_or_generate()` function with the full resolution chain:
keychain → migrate file → generate new. This is the core behavior change.

**Files:**
- Modify: `src-tauri/src/identity.rs`

- [ ] **Step 1: Write failing tests for the resolution chain**

Add to the `tests` module:

```rust
    #[test]
    fn load_or_generate_migrates_file_to_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        let bak_path = dir.path().join("identity.key.bak");

        // Pre-create an identity file
        let file_store = FileStore::new(path.clone());
        let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
        let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
        let original = NodeIdentity { pq, ed25519 };
        let original_addr = original.ed25519.public_identity().address_hash;
        file_store.save(&original).unwrap();

        // Load via resolution chain with mock keychain
        let keychain = KeychainStore::new_mock();
        let loaded = load_or_generate_with_stores(&keychain, &file_store).unwrap();

        // Same identity
        assert_eq!(loaded.ed25519.public_identity().address_hash, original_addr);
        // File renamed to .bak
        assert!(!path.exists(), "original file should be renamed");
        assert!(bak_path.exists(), ".bak file should exist");
        // Keychain now has the identity
        let from_keychain = keychain.load().unwrap().expect("should be in keychain");
        assert_eq!(
            from_keychain.ed25519.public_identity().address_hash,
            original_addr,
        );
    }

    #[test]
    fn load_or_generate_uses_keychain_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");

        // Pre-populate keychain
        let keychain = KeychainStore::new_mock();
        let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
        let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
        let original = NodeIdentity { pq, ed25519 };
        let original_addr = original.ed25519.public_identity().address_hash;
        keychain.save(&original).unwrap();

        let file_store = FileStore::new(path.clone());
        let loaded = load_or_generate_with_stores(&keychain, &file_store).unwrap();

        assert_eq!(loaded.ed25519.public_identity().address_hash, original_addr);
        // No file should have been created
        assert!(!path.exists());
    }

    #[test]
    fn load_or_generate_creates_new_in_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");

        let keychain = KeychainStore::new_mock();
        let file_store = FileStore::new(path.clone());

        // Nothing exists — should generate fresh keys
        let identity = load_or_generate_with_stores(&keychain, &file_store).unwrap();

        // Stored in keychain
        let from_keychain = keychain.load().unwrap().expect("should be in keychain");
        assert_eq!(
            from_keychain.ed25519.public_identity().address_hash,
            identity.ed25519.public_identity().address_hash,
        );
        // NOT stored on disk
        assert!(!path.exists());
    }

    #[test]
    fn load_or_generate_falls_back_to_file_on_keychain_write_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");

        // Use a keychain store that always errors on save
        let keychain = KeychainStore::new_failing_mock();
        let file_store = FileStore::new(path.clone());

        let identity = load_or_generate_with_stores(&keychain, &file_store).unwrap();

        // Should have fallen back to file
        assert!(path.exists());
        let from_file = file_store.load().unwrap().expect("should be on disk");
        assert_eq!(
            from_file.ed25519.public_identity().address_hash,
            identity.ed25519.public_identity().address_hash,
        );
    }

    #[test]
    fn migration_aborted_when_keychain_write_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        let bak_path = dir.path().join("identity.key.bak");

        // Pre-create an identity file
        let file_store = FileStore::new(path.clone());
        let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
        let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
        let original = NodeIdentity { pq, ed25519 };
        let original_addr = original.ed25519.public_identity().address_hash;
        file_store.save(&original).unwrap();

        // Keychain that fails on save
        let keychain = KeychainStore::new_failing_mock();
        let loaded = load_or_generate_with_stores(&keychain, &file_store).unwrap();

        // Same identity loaded
        assert_eq!(loaded.ed25519.public_identity().address_hash, original_addr);
        // File NOT renamed (migration aborted)
        assert!(path.exists(), "file should still exist");
        assert!(!bak_path.exists(), ".bak should NOT exist");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo test -p harmony-app -- tests::load_or_generate`
Expected: FAIL — `load_or_generate_with_stores` and `new_failing_mock` not defined

- [ ] **Step 3: Add the failing mock constructor**

Add to `KeychainStore`:

```rust
    /// Create a store that always fails on save (for testing fallback).
    #[cfg(test)]
    pub fn new_failing_mock() -> Self {
        use keyring::credential::CredentialPersistence;
        let mut mock = keyring::mock::MockCredential::new_with_persistence(
            KEYCHAIN_SERVICE,
            KEYCHAIN_ACCOUNT,
            CredentialPersistence::UntilDelete,
        );
        mock.set_error(keyring::mock::MockCredentialError::Invalid(
            "simulated keychain failure".to_string(),
        ));
        Self {
            entry: keyring::Entry::new_with_credential(Box::new(mock)),
        }
    }
```

- [ ] **Step 4: Implement the resolution chain**

Replace the existing `load_or_generate` function and add the internal helper:

```rust
/// Internal resolution chain — accepts injected stores for testability.
fn load_or_generate_with_stores(
    keychain: &KeychainStore,
    file_store: &FileStore,
) -> Result<NodeIdentity, String> {
    // 1. Try keychain first
    match keychain.load() {
        Ok(Some(identity)) => return Ok(identity),
        Ok(None) => {}
        Err(e) => tracing::warn!("keychain load failed, trying file: {e}"),
    }

    // 2. Check for existing file — migrate to keychain if found
    if let Some(identity) = file_store.load()? {
        match keychain.save(&identity) {
            Ok(()) => {
                // Migration succeeded — rename file to .bak
                if let Err(e) = file_store.rename_to_backup() {
                    tracing::warn!("failed to rename identity file to .bak: {e}");
                }
                tracing::info!("migrated identity from file to OS keychain");
            }
            Err(e) => {
                // Migration failed — keep using file
                tracing::warn!("keychain write failed during migration, keeping file: {e}");
            }
        }
        return Ok(identity);
    }

    // 3. Generate fresh identity
    let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
    let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
    let identity = NodeIdentity { pq, ed25519 };

    // Try keychain first, fall back to file
    match keychain.save(&identity) {
        Ok(()) => tracing::info!("new identity stored in OS keychain"),
        Err(e) => {
            tracing::warn!("keychain write failed, falling back to file: {e}");
            file_store.save(&identity)?;
        }
    }

    Ok(identity)
}

/// Load identity from keychain or file, or generate and save a new one.
///
/// Resolution order:
/// 1. OS keychain (via `keyring`)
/// 2. File at `path` (migrated to keychain if found)
/// 3. Generate fresh keys (stored in keychain, file fallback)
pub fn load_or_generate(path: &Path) -> Result<NodeIdentity, String> {
    let file_store = FileStore::new(path.to_path_buf());

    match KeychainStore::new() {
        Ok(keychain) => load_or_generate_with_stores(&keychain, &file_store),
        Err(e) => {
            tracing::warn!("keychain unavailable, using file storage: {e}");
            // Fall back to file-only behavior
            if let Some(identity) = file_store.load()? {
                return Ok(identity);
            }
            let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
            let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
            let identity = NodeIdentity { pq, ed25519 };
            file_store.save(&identity)?;
            Ok(identity)
        }
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd src-tauri && cargo test -p harmony-app -- tests::load_or_generate`
Expected: all 5 new tests PASS

- [ ] **Step 6: Run full Rust test suite**

Run: `cd src-tauri && cargo test -p harmony-app`
Expected: all tests pass, no regressions

- [ ] **Step 7: Run cargo clippy**

Run: `cd src-tauri && cargo clippy -p harmony-app`
Expected: no new warnings from our code

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/identity.rs
git commit -m "feat(identity): resolution chain with keychain, migration, and fallback"
```

---

## Self-Review

**Spec coverage:**
- Keychain storage via `keyring` → Task 3 (KeychainStore)
- File-based fallback → Task 2 (FileStore, preserved)
- Auto-migration with `.bak` → Task 4 (resolution chain + `rename_to_backup`)
- Error handling / fallback table → Task 4 (all paths tested)
- Testing strategy → Tasks 2–4 (unit tests with mock keychain)

**Placeholder scan:** No TBD, TODO, or incomplete steps. All code blocks are complete.

**Type consistency:** `NodeIdentity`, `KeyStore`, `FileStore`, `KeychainStore`, `identity_to_blob`, `blob_to_identity`, `load_or_generate_with_stores` — all used consistently across tasks.
