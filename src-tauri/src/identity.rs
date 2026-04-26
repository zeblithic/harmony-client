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
    debug_assert_eq!(buf.len(), BLOB_LEN, "identity blob length mismatch");
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

// ── Atomic file write ──────────────────────────────────────────────────

/// Write `bytes` to `path` atomically with mode 0o600 on Unix.
///
/// Steps:
///   1. Ensure parent directory exists with mode 0o700 (Unix only).
///   2. Open `<path>.tmp` with mode 0o600 (Unix only).
///   3. Write + fsync.
///   4. Atomic rename `<path>.tmp` → `<path>`.
///
/// `TmpGuard` removes the `.tmp` file if anything panics or returns Err
/// before the rename completes. After successful rename, the guard is
/// `mem::forget`ed so the renamed file isn't unlinked.
fn write_atomic_0600(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Best-effort: ignore failures (directory may already exist with a
            // different owner — common in containers / multi-user setups).
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }

    let tmp_path = {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".tmp");
        path.with_file_name(name)
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
            .write_all(bytes)
            .map_err(|e| format!("Failed to write {}: {e}", tmp_path.display()))?;
        f.sync_all()
            .map_err(|e| format!("Failed to fsync {}: {e}", tmp_path.display()))?;
    }
    std::fs::rename(&tmp_path, path).map_err(|e| {
        format!(
            "Failed to rename {} → {}: {e}",
            tmp_path.display(),
            path.display()
        )
    })?;
    std::mem::forget(guard);
    Ok(())
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
        let raw = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("Failed to read {}: {e}", self.path.display())),
        };
        let buf = Zeroizing::new(raw);
        let identity = blob_to_identity(&buf)?;
        #[cfg(unix)]
        warn_permissions(&self.path);
        Ok(Some(identity))
    }

    fn save(&self, identity: &NodeIdentity) -> Result<(), String> {
        let blob = identity_to_blob(identity);
        write_atomic_0600(&self.path, &blob)
    }
}

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
        let credential = keyring::mock::MockCredential::default();
        let entry = keyring::Entry::new_with_credential(Box::new(credential));
        Self { entry }
    }

    /// Create a store that always fails on save (for testing fallback).
    ///
    /// Load returns `NoEntry`; save always returns an error.
    #[cfg(test)]
    pub fn new_failing_mock() -> Self {
        let credential = AlwaysFailOnSave;
        let entry = keyring::Entry::new_with_credential(Box::new(credential));
        Self { entry }
    }

    /// Create a store where ALL operations fail (simulates inaccessible keychain).
    #[cfg(test)]
    pub fn new_load_failing_mock() -> Self {
        let credential = AlwaysFailOnLoad;
        let entry = keyring::Entry::new_with_credential(Box::new(credential));
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

/// Internal resolution chain — accepts injected stores for testability.
fn load_or_generate_with_stores(
    keychain: &KeychainStore,
    file_store: &FileStore,
) -> Result<NodeIdentity, String> {
    // 1. Try keychain first
    let keychain_load_failed = match keychain.load() {
        Ok(Some(identity)) => return Ok(identity),
        Ok(None) => false,
        Err(e) => {
            tracing::warn!("keychain load failed, trying file: {e}");
            true
        }
    };

    // 2. Check for existing file — migrate to keychain if found
    if let Some(identity) = file_store.load()? {
        // Only attempt migration if keychain is healthy (load returned Ok).
        // If keychain errored, the entry may still exist but be inaccessible —
        // writing could overwrite it.
        if !keychain_load_failed {
            match keychain.save(&identity) {
                Ok(()) => {
                    if let Err(e) = file_store.rename_to_backup() {
                        tracing::warn!("identity saved to keychain but file rename to .bak failed: {e}");
                    } else {
                        tracing::info!("migrated identity from file to OS keychain");
                    }
                }
                Err(e) => {
                    tracing::warn!("keychain write failed during migration, keeping file: {e}");
                }
            }
        }
        return Ok(identity);
    }

    // 3. Generate fresh identity.
    let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
    let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
    let identity = NodeIdentity { pq, ed25519 };

    // If keychain errored, don't attempt writes — an inaccessible entry may
    // exist and we'd overwrite it. Save to file only.
    if keychain_load_failed {
        tracing::warn!("keychain unhealthy, saving new identity to file only");
        file_store.save(&identity)?;
        return Ok(identity);
    }

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

/// A `CredentialApi` implementation that always returns `NoEntry` on reads
/// and `Error::Invalid` on writes. Used only in tests for failure-path coverage.
#[cfg(test)]
#[derive(Debug)]
struct AlwaysFailOnSave;

/// A `CredentialApi` implementation that returns a platform error on ALL operations.
/// Simulates a keychain that is constructed successfully but fails at I/O time
/// (e.g., locked macOS keychain, missing D-Bus session).
#[cfg(test)]
#[derive(Debug)]
struct AlwaysFailOnLoad;

#[cfg(test)]
impl keyring::credential::CredentialApi for AlwaysFailOnLoad {
    fn set_secret(&self, _secret: &[u8]) -> keyring::Result<()> {
        Err(keyring::Error::Invalid(
            "simulated platform failure".to_string(),
            "always fails".to_string(),
        ))
    }

    fn get_secret(&self) -> keyring::Result<Vec<u8>> {
        Err(keyring::Error::Invalid(
            "simulated platform failure".to_string(),
            "always fails".to_string(),
        ))
    }

    fn delete_credential(&self) -> keyring::Result<()> {
        Err(keyring::Error::Invalid(
            "simulated platform failure".to_string(),
            "always fails".to_string(),
        ))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
impl keyring::credential::CredentialApi for AlwaysFailOnSave {
    fn set_secret(&self, _secret: &[u8]) -> keyring::Result<()> {
        Err(keyring::Error::Invalid(
            "simulated keychain failure".to_string(),
            "always fails".to_string(),
        ))
    }

    fn get_secret(&self) -> keyring::Result<Vec<u8>> {
        Err(keyring::Error::NoEntry)
    }

    fn delete_credential(&self) -> keyring::Result<()> {
        Err(keyring::Error::NoEntry)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
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
    fn file_store_load_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.key");
        let store = FileStore::new(path);
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn keychain_store_round_trip() {
        // PQ keygen (ML-DSA scalar NTT) requires ~2 MB stack — spawn a larger thread.
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
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
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn keychain_store_load_returns_none_when_empty() {
        let store = KeychainStore::new_mock();
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn load_or_generate_migrates_file_to_keychain() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("identity.key");

                // Pre-create a file via FileStore
                let file_store = FileStore::new(path.clone());
                let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
                let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
                let original = NodeIdentity { pq, ed25519 };
                let original_ed_hash = original.ed25519.public_identity().address_hash;
                let original_pq_hash = original.pq.public_identity().address_hash;
                file_store.save(&original).unwrap();

                let keychain = KeychainStore::new_mock();
                let result = load_or_generate_with_stores(&keychain, &file_store).unwrap();

                // Same identity returned
                assert_eq!(
                    result.ed25519.public_identity().address_hash,
                    original_ed_hash
                );
                assert_eq!(
                    result.pq.public_identity().address_hash,
                    original_pq_hash
                );

                // File renamed to .bak
                assert!(!path.exists(), "original file should be renamed");
                let bak = path.with_extension("key.bak");
                assert!(bak.exists(), "backup file should exist");

                // Identity in keychain
                let from_keychain = keychain.load().unwrap().expect("identity should be in keychain");
                assert_eq!(
                    from_keychain.ed25519.public_identity().address_hash,
                    original_ed_hash
                );
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn load_or_generate_uses_keychain_when_present() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("identity.key");

                // Pre-populate mock keychain
                let keychain = KeychainStore::new_mock();
                let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
                let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
                let original = NodeIdentity { pq, ed25519 };
                let original_ed_hash = original.ed25519.public_identity().address_hash;
                keychain.save(&original).unwrap();

                let file_store = FileStore::new(path.clone());
                let result = load_or_generate_with_stores(&keychain, &file_store).unwrap();

                // Same identity returned
                assert_eq!(
                    result.ed25519.public_identity().address_hash,
                    original_ed_hash
                );

                // No file created on disk
                assert!(!path.exists(), "no file should be created when keychain is used");
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn load_or_generate_creates_new_in_keychain() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("identity.key");

                // Nothing exists
                let keychain = KeychainStore::new_mock();
                let file_store = FileStore::new(path.clone());

                let result = load_or_generate_with_stores(&keychain, &file_store).unwrap();

                // Identity returned (non-trivial check: must have an address)
                let _ = result.ed25519.public_identity().address_hash;

                // Stored in keychain
                let from_keychain = keychain.load().unwrap().expect("identity should be in keychain");
                assert_eq!(
                    from_keychain.ed25519.public_identity().address_hash,
                    result.ed25519.public_identity().address_hash,
                );

                // NOT on disk
                assert!(!path.exists(), "identity should be in keychain, not on disk");
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn load_or_generate_falls_back_to_file_on_keychain_write_failure() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("identity.key");

                // Use a failing mock keychain
                let keychain = KeychainStore::new_failing_mock();
                let file_store = FileStore::new(path.clone());

                let result = load_or_generate_with_stores(&keychain, &file_store).unwrap();

                // Identity returned
                let ed_hash = result.ed25519.public_identity().address_hash;

                // Stored on disk (fallback)
                assert!(path.exists(), "identity should be on disk as fallback");
                let from_file = file_store.load().unwrap().expect("should load from file");
                assert_eq!(
                    from_file.ed25519.public_identity().address_hash,
                    ed_hash,
                );
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn migration_aborted_when_keychain_write_fails() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("identity.key");

                // Pre-create a file
                let file_store = FileStore::new(path.clone());
                let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
                let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
                let original = NodeIdentity { pq, ed25519 };
                let original_ed_hash = original.ed25519.public_identity().address_hash;
                file_store.save(&original).unwrap();

                // Use failing mock keychain
                let keychain = KeychainStore::new_failing_mock();
                let result = load_or_generate_with_stores(&keychain, &file_store).unwrap();

                // Identity loaded from file (same address)
                assert_eq!(
                    result.ed25519.public_identity().address_hash,
                    original_ed_hash,
                );

                // File NOT renamed to .bak (migration was aborted)
                assert!(path.exists(), "original file should still exist");
                let bak = path.with_extension("key.bak");
                assert!(!bak.exists(), "backup file should NOT exist when migration fails");
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn keychain_load_error_no_file_generates_to_file() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("identity.key");

                // Keychain that errors on load (not NoEntry — a real error)
                let keychain = KeychainStore::new_load_failing_mock();
                let file_store = FileStore::new(path.clone());

                // Should succeed — falls back to file generation
                let identity = load_or_generate_with_stores(&keychain, &file_store).unwrap();

                // Saved to file (keychain was skipped)
                assert!(path.exists(), "identity should be saved to file");
                let from_file = file_store.load().unwrap().expect("should be on disk");
                assert_eq!(
                    from_file.ed25519.public_identity().address_hash,
                    identity.ed25519.public_identity().address_hash,
                );
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn keychain_load_error_with_file_uses_file_without_migration() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
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

                // Keychain that errors on load
                let keychain = KeychainStore::new_load_failing_mock();
                let loaded = load_or_generate_with_stores(&keychain, &file_store).unwrap();

                // Same identity from file
                assert_eq!(loaded.ed25519.public_identity().address_hash, original_addr);
                // File NOT renamed — migration was skipped (keychain unhealthy)
                assert!(path.exists(), "file should still exist (no migration)");
                assert!(!bak_path.exists(), "no .bak (migration was skipped)");
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
