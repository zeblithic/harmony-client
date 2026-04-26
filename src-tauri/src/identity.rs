//! Node identity management — Ed25519 + post-quantum key generation and persistence.
//!
//! Storage backends:
//! - `KeychainStore`         — OS-native keychain via the `keyring` crate (KeyStore impl)
//! - `FileStore`             — binary file at `~/.harmony/identity.key` (KeyStore impl;
//!                              transitional, will be retired in favour of the encrypted
//!                              backend)
//! - `LegacyPlaintextReader` — read-only migration helper; reads the legacy plaintext
//!                              file but never writes one (intentionally NOT a KeyStore)
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

// ── Encrypted file wire format constants ───────────────────────────────

const ENC_MAGIC: &[u8; 4] = b"HRMI";
const ENC_FORMAT_VERSION: u8 = 0x01;
const ENC_KDF_ID_ARGON2ID: u8 = 0x01;

// Argon2id parameters (v1):
const KDF_M_KIB: u32 = 65536;  // 64 MiB
const KDF_T: u16 = 3;          // iterations
const KDF_P: u8 = 1;           // parallelism
const KDF_OUT_LEN: usize = 32; // XChaCha20-Poly1305 key length

// Wire format offsets:
const HEADER_LEN: usize = 13;   // magic(4) + version(1) + kdf_id(1) + m(4) + t(2) + p(1)
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;    // XChaCha20 needs 192-bit nonce
const TAG_LEN: usize = 16;      // Poly1305 tag
const ENC_FILE_LEN: usize = HEADER_LEN + SALT_LEN + NONCE_LEN + BLOB_LEN + TAG_LEN; // 230

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

// ── Encrypted file wire format helpers ─────────────────────────────────

/// Encode a 161-byte identity blob into the 230-byte encrypted-file format.
///
/// Caller supplies salt and nonce explicitly so the function is deterministic
/// for testing. Production code generates fresh random values per save.
fn encrypt_with_params(
    passphrase: &[u8],
    salt: &[u8; SALT_LEN],
    nonce: &[u8; NONCE_LEN],
    blob: &[u8; BLOB_LEN],
) -> Vec<u8> {
    use argon2::{Algorithm, Argon2, Params, Version};
    use chacha20poly1305::{
        aead::{Aead, KeyInit, Payload},
        XChaCha20Poly1305, XNonce,
    };

    // Build header (13 bytes — also serves as AAD).
    let mut out = Vec::with_capacity(ENC_FILE_LEN);
    out.extend_from_slice(ENC_MAGIC);
    out.push(ENC_FORMAT_VERSION);
    out.push(ENC_KDF_ID_ARGON2ID);
    out.extend_from_slice(&KDF_M_KIB.to_be_bytes());
    out.extend_from_slice(&KDF_T.to_be_bytes());
    out.push(KDF_P);
    debug_assert_eq!(out.len(), HEADER_LEN);

    // Append salt, nonce.
    out.extend_from_slice(salt);
    out.extend_from_slice(nonce);
    debug_assert_eq!(out.len(), HEADER_LEN + SALT_LEN + NONCE_LEN);

    // KDF.
    let params = Params::new(KDF_M_KIB, KDF_T as u32, KDF_P as u32, Some(KDF_OUT_LEN))
        .expect("Argon2 params hardcoded valid");
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; KDF_OUT_LEN]);
    argon
        .hash_password_into(passphrase, salt, key.as_mut_slice())
        .expect("Argon2 derivation cannot fail with hardcoded params");

    // AEAD encrypt with header (first 13 bytes) as AAD.
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_slice())
        .expect("32-byte key always valid");
    let payload = Payload {
        msg: blob,
        aad: &out[..HEADER_LEN],
    };
    let ciphertext_with_tag = cipher
        .encrypt(XNonce::from_slice(nonce), payload)
        .expect("AEAD encrypt cannot fail with valid inputs");
    debug_assert_eq!(ciphertext_with_tag.len(), BLOB_LEN + TAG_LEN);

    out.extend_from_slice(&ciphertext_with_tag);
    debug_assert_eq!(out.len(), ENC_FILE_LEN);
    out
}

/// Decode a 230-byte encrypted-file blob back into the 161-byte identity blob.
///
/// Indistinguishable error for wrong-passphrase vs corrupted-ciphertext to
/// avoid leaking which case occurred (an attacker who can probe with arbitrary
/// passphrases gains no signal from the error message).
///
/// Returns `Zeroizing<[u8; BLOB_LEN]>` so the caller's stack-resident copy of
/// the plaintext key bytes is wiped on drop. The intermediate `Vec<u8>` from
/// `cipher.decrypt(...)` is also wrapped in `Zeroizing` before any further use.
fn decrypt(passphrase: &[u8], bytes: &[u8]) -> Result<Zeroizing<[u8; BLOB_LEN]>, String> {
    use argon2::{Algorithm, Argon2, Params, Version};
    use chacha20poly1305::{
        aead::{Aead, KeyInit, Payload},
        XChaCha20Poly1305, XNonce,
    };

    if bytes.len() != ENC_FILE_LEN {
        return Err(format!(
            "identity store is corrupt: expected {ENC_FILE_LEN} bytes, got {}",
            bytes.len()
        ));
    }
    if &bytes[0..4] != ENC_MAGIC {
        return Err(format!(
            "identity store is in an unrecognized format (magic={:?}) — this build may be too old",
            &bytes[0..4]
        ));
    }
    if bytes[4] != ENC_FORMAT_VERSION {
        return Err(format!(
            "identity store is in an unrecognized format (version={:#04x}) — this build may be too old",
            bytes[4]
        ));
    }
    if bytes[5] != ENC_KDF_ID_ARGON2ID {
        return Err(format!(
            "identity store is in an unrecognized format (kdf_id={:#04x}) — this build may be too old",
            bytes[5]
        ));
    }

    // Pull KDF params from the file (self-describing). Offsets are derived from
    // the named layout constants so a future format change automatically shifts
    // every range — adding a new field would only require updating the constants.
    const M_KIB_OFF: usize = 6;
    const T_OFF: usize = M_KIB_OFF + 4; // = 10
    const P_OFF: usize = T_OFF + 2; // = 12
    const SALT_OFF: usize = HEADER_LEN; // = 13
    const NONCE_OFF: usize = SALT_OFF + SALT_LEN; // = 29
    const CIPHER_OFF: usize = NONCE_OFF + NONCE_LEN; // = 53

    let m_kib = u32::from_be_bytes(bytes[M_KIB_OFF..M_KIB_OFF + 4].try_into().unwrap());
    let t = u16::from_be_bytes(bytes[T_OFF..T_OFF + 2].try_into().unwrap()) as u32;
    let p = bytes[P_OFF] as u32;
    let salt: &[u8; SALT_LEN] = bytes[SALT_OFF..NONCE_OFF].try_into().unwrap();
    let nonce: &[u8; NONCE_LEN] = bytes[NONCE_OFF..CIPHER_OFF].try_into().unwrap();
    let ciphertext_with_tag = &bytes[CIPHER_OFF..ENC_FILE_LEN];

    // Invalid params most likely indicate tampering (a flipped bit in the
    // header can produce values below Argon2's minimum). Return the
    // indistinguishable error so an attacker learns nothing from probing.
    let params = Params::new(m_kib, t, p, Some(KDF_OUT_LEN))
        .map_err(|_| "identity store could not be decrypted: wrong passphrase or corrupted file".to_string())?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; KDF_OUT_LEN]);
    // hash_password_into can return SaltTooShort/SaltTooLong (ruled out: salt is
    // a fixed 16-byte slice from the file) or PwdTooLong (requires a >4 GiB
    // passphrase — practically unreachable). Surfacing the specific error here
    // is safe: it cannot be triggered by an adversary tampering with the file.
    argon
        .hash_password_into(passphrase, salt, key.as_mut_slice())
        .map_err(|e| format!("Argon2 derivation failed: {e}"))?;

    let cipher = XChaCha20Poly1305::new_from_slice(key.as_slice())
        .expect("32-byte key always valid");
    let payload = Payload {
        msg: ciphertext_with_tag,
        aad: &bytes[..HEADER_LEN],
    };
    // Wrap the AEAD output Vec in Zeroizing immediately so it is wiped on drop.
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(XNonce::from_slice(nonce), payload)
            .map_err(|_| "identity store could not be decrypted: wrong passphrase or corrupted file".to_string())?,
    );

    let blob_arr: [u8; BLOB_LEN] = plaintext
        .as_slice()
        .try_into()
        .map_err(|_| format!("decrypted plaintext was {} bytes, expected {}", plaintext.len(), BLOB_LEN))?;
    Ok(Zeroizing::new(blob_arr))
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

// ── LegacyPlaintextReader ───────────────────────────────────────────────

/// Read-only reader for legacy plaintext identity files at `~/.harmony/identity.key`.
///
/// Deliberately does not implement `KeyStore` — there is no `save` and there
/// will never be one. This type exists solely to migrate identities written by
/// the pre-encryption code path into the modern keychain or encrypted-file
/// backends.
pub(crate) struct LegacyPlaintextReader {
    path: PathBuf,
}

impl LegacyPlaintextReader {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Read the plaintext identity at `self.path`, or `Ok(None)` if missing.
    pub(crate) fn read(&self) -> Result<Option<NodeIdentity>, String> {
        Self::read_from(&self.path)
    }

    /// Free function variant — read plaintext identity from `path`, or
    /// `Ok(None)` if missing.
    pub(crate) fn read_from(path: &Path) -> Result<Option<NodeIdentity>, String> {
        let raw = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("Failed to read {}: {e}", path.display())),
        };
        let buf = Zeroizing::new(raw);
        let identity = blob_to_identity(&buf)?;
        #[cfg(unix)]
        warn_permissions(path);
        Ok(Some(identity))
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

// ── EncryptedFileStore ─────────────────────────────────────────────────

use secrecy::{ExposeSecret, SecretString};

/// Passphrase-encrypted identity file at a given path.
///
/// On-disk format is the 230-byte layout produced by `encrypt_with_params`:
/// Argon2id (m=64MiB, t=3, p=1) derives a 32-byte key for XChaCha20-Poly1305
/// AEAD over the 161-byte identity blob. The 13-byte header (magic, version,
/// kdf_id, KDF params) is bound as AAD.
///
/// Used as the headless fallback when no OS keychain is reachable. Keyed from
/// the `HARMONY_PASSPHRASE` / `HARMONY_PASSPHRASE_FILE` environment variables
/// — see `Self::from_env` (added in Task 6).
//
// Debug is safe to derive: SecretString (secrecy 0.10) formats as
// `SecretBox<str>([REDACTED])`. If new secret-bearing fields are added they
// must use a `secrecy` wrapper, or this derive must become a manual impl.
#[derive(Debug)]
pub(crate) struct EncryptedFileStore {
    path: PathBuf,
    passphrase: SecretString,
}

impl EncryptedFileStore {
    /// Build a store backed by `path`, encrypted with `passphrase`.
    pub(crate) fn new(path: PathBuf, passphrase: SecretString) -> Self {
        Self { path, passphrase }
    }

    /// Path to the on-disk file (used by callers like `rotate_passphrase`).
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Construct from the `HARMONY_PASSPHRASE` / `HARMONY_PASSPHRASE_FILE`
    /// environment variables.
    ///
    /// Returns:
    ///   - `Ok(None)` if neither env var is set
    ///   - `Ok(Some(store))` if a non-empty passphrase resolves
    ///   - `Err(...)` if either var is set but malformed (empty, file unreadable,
    ///     resolves to empty)
    ///
    /// Precedence: `HARMONY_PASSPHRASE` (direct) wins over `HARMONY_PASSPHRASE_FILE`
    /// when both are set; a warning is logged.
    pub(crate) fn from_env(path: PathBuf) -> Result<Option<Self>, String> {
        let direct = std::env::var("HARMONY_PASSPHRASE").ok();
        let file_path = std::env::var("HARMONY_PASSPHRASE_FILE").ok();

        if direct.is_some() && file_path.is_some() {
            tracing::warn!(
                "both HARMONY_PASSPHRASE and HARMONY_PASSPHRASE_FILE are set; HARMONY_PASSPHRASE takes precedence"
            );
        }

        let passphrase_str = if let Some(s) = direct {
            if s.is_empty() {
                return Err("HARMONY_PASSPHRASE is set to an empty string".to_string());
            }
            s
        } else if let Some(file_path) = file_path {
            let raw = std::fs::read(&file_path)
                .map_err(|e| format!("HARMONY_PASSPHRASE_FILE={file_path} could not be read: {e}"))?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&file_path) {
                    let mode = meta.permissions().mode() & 0o777;
                    if mode & 0o077 != 0 {
                        tracing::warn!(
                            path = %file_path,
                            mode = format!("{mode:#05o}"),
                            "HARMONY_PASSPHRASE_FILE has open permissions, should be 0600"
                        );
                    }
                }
            }

            // Strip exactly one trailing \n or \r\n.
            let mut s = String::from_utf8(raw)
                .map_err(|_| format!("HARMONY_PASSPHRASE_FILE={file_path} is not valid UTF-8"))?;
            if s.ends_with("\r\n") {
                s.truncate(s.len() - 2);
            } else if s.ends_with('\n') {
                s.truncate(s.len() - 1);
            }
            if s.is_empty() {
                return Err(format!(
                    "HARMONY_PASSPHRASE_FILE={file_path} contains an empty passphrase (after trimming one trailing newline)"
                ));
            }
            s
        } else {
            return Ok(None);
        };

        Ok(Some(Self::new(path, SecretString::from(passphrase_str))))
    }
}

impl KeyStore for EncryptedFileStore {
    fn load(&self) -> Result<Option<NodeIdentity>, String> {
        let raw = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("Failed to read {}: {e}", self.path.display())),
        };
        // `decrypt` returns Zeroizing<[u8; BLOB_LEN]> — the stack array's bytes
        // are wiped on drop. blob_to_identity reads the slice without copying.
        let blob = decrypt(self.passphrase.expose_secret().as_bytes(), &raw)?;
        let identity = blob_to_identity(blob.as_slice())?;
        Ok(Some(identity))
    }

    fn save(&self, identity: &NodeIdentity) -> Result<(), String> {
        let blob = identity_to_blob(identity);
        let blob_arr: [u8; BLOB_LEN] = blob
            .as_slice()
            .try_into()
            .expect("identity_to_blob always returns BLOB_LEN bytes");

        let mut salt = [0u8; SALT_LEN];
        let mut nonce = [0u8; NONCE_LEN];
        use rand::RngCore;
        rand::rngs::OsRng.fill_bytes(&mut salt);
        rand::rngs::OsRng.fill_bytes(&mut nonce);

        let bytes = encrypt_with_params(
            self.passphrase.expose_secret().as_bytes(),
            &salt,
            &nonce,
            &blob_arr,
        );
        write_atomic_0600(&self.path, &bytes)
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

    mod wire_format {
        use super::*;

        const TEST_PASSPHRASE: &[u8] = b"correct horse battery staple";
        const TEST_SALT: [u8; 16] = [0xAB; 16];
        const TEST_NONCE: [u8; 24] = [0xCD; 24];
        const TEST_BLOB: [u8; 161] = [0x42; 161];

        #[test]
        fn round_trip_correct_passphrase() {
            let bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
            let decrypted = decrypt(TEST_PASSPHRASE, &bytes).unwrap();
            assert_eq!(&decrypted[..], &TEST_BLOB[..]);
        }

        #[test]
        fn wrong_passphrase_fails_aead() {
            let bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
            let err = decrypt(b"wrong passphrase", &bytes).unwrap_err();
            assert!(
                err.contains("wrong passphrase or corrupted file"),
                "expected indistinguishable error, got: {err}"
            );
        }

        #[test]
        fn tampered_ciphertext_fails() {
            let mut bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
            bytes[100] ^= 0x01;  // flip one bit in the ciphertext range (53..214)
            let err = decrypt(TEST_PASSPHRASE, &bytes).unwrap_err();
            assert!(err.contains("wrong passphrase or corrupted file"));
        }

        #[test]
        fn tampered_kdf_params_fails_aad() {
            let mut bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
            // Flip a byte in kdf_m_kib (offset 6..10) — part of AAD
            bytes[7] ^= 0x01;
            let err = decrypt(TEST_PASSPHRASE, &bytes).unwrap_err();
            assert!(
                err.contains("wrong passphrase or corrupted file"),
                "AAD binding should reject param tampering, got: {err}"
            );
        }

        #[test]
        fn tampered_magic_fails() {
            let mut bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
            bytes[0] = b'X';  // trash magic
            let err = decrypt(TEST_PASSPHRASE, &bytes).unwrap_err();
            assert!(err.contains("unrecognized format"), "got: {err}");
        }

        #[test]
        fn tampered_version_fails() {
            let mut bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
            bytes[4] = 0xFF;  // unknown version
            let err = decrypt(TEST_PASSPHRASE, &bytes).unwrap_err();
            assert!(err.contains("unrecognized format"), "got: {err}");
        }

        #[test]
        fn truncated_file_fails() {
            let bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
            let err = decrypt(TEST_PASSPHRASE, &bytes[..200]).unwrap_err();
            assert!(err.contains("expected 230 bytes"), "got: {err}");
        }

        #[test]
        fn output_is_exactly_230_bytes() {
            let bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
            assert_eq!(bytes.len(), 230);
        }

        #[test]
        fn header_layout_is_exact() {
            let bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
            // 230-byte file: 13-byte header, 16-byte salt, 24-byte nonce, 161-byte ciphertext, 16-byte tag.
            // NOTE: kdf_t is u16 BE (not u32) so the header fits in 13 bytes total.
            assert_eq!(&bytes[0..4], b"HRMI", "magic mismatch");
            assert_eq!(bytes[4], 0x01, "format_version mismatch");
            assert_eq!(bytes[5], 0x01, "kdf_id mismatch");
            assert_eq!(&bytes[6..10], &65536u32.to_be_bytes(), "kdf_m_kib (u32 BE) mismatch");
            assert_eq!(&bytes[10..12], &3u16.to_be_bytes(), "kdf_t (u16 BE) mismatch");
            assert_eq!(bytes[12], 1, "kdf_p (u8) mismatch");
            assert_eq!(&bytes[13..29], &TEST_SALT[..], "salt mismatch");
            assert_eq!(&bytes[29..53], &TEST_NONCE[..], "nonce mismatch");
            assert_eq!(bytes.len(), 230);
        }
    }

    mod encrypted_file_store {
        use super::*;
        use secrecy::SecretString;

        fn fresh_identity() -> NodeIdentity {
            let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
            let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
            NodeIdentity { pq, ed25519 }
        }

        fn fresh_passphrase() -> SecretString {
            SecretString::from("correct horse battery staple".to_string())
        }

        #[test]
        fn round_trip_correct_passphrase() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");
            let store = EncryptedFileStore::new(path.clone(), fresh_passphrase());

            let original = fresh_identity();
            let original_addr = original.ed25519.public_identity().address_hash;

            store.save(&original).unwrap();
            let loaded = store.load().unwrap().expect("should find saved identity");
            assert_eq!(loaded.ed25519.public_identity().address_hash, original_addr);
        }

        #[test]
        fn load_returns_none_when_missing() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("nonexistent.enc");
            let store = EncryptedFileStore::new(path, fresh_passphrase());
            assert!(store.load().unwrap().is_none());
        }

        #[test]
        fn wrong_passphrase_fails() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");

            EncryptedFileStore::new(path.clone(), fresh_passphrase())
                .save(&fresh_identity())
                .unwrap();

            let wrong = EncryptedFileStore::new(path, SecretString::from("wrong".to_string()));
            let err = wrong.load().unwrap_err();
            assert!(err.contains("wrong passphrase or corrupted file"), "got: {err}");
        }

        #[test]
        fn salt_rotates_per_save() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");
            let store = EncryptedFileStore::new(path.clone(), fresh_passphrase());
            let id = fresh_identity();

            store.save(&id).unwrap();
            let bytes_a = std::fs::read(&path).unwrap();
            store.save(&id).unwrap();
            let bytes_b = std::fs::read(&path).unwrap();

            assert_ne!(bytes_a, bytes_b, "salt+nonce must rotate per save");
            // Both must still load back to the same identity:
            let loaded = store.load().unwrap().unwrap();
            assert_eq!(
                loaded.ed25519.public_identity().address_hash,
                id.ed25519.public_identity().address_hash,
            );
        }

        #[cfg(unix)]
        #[test]
        fn file_mode_0o600_unix() {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");
            let store = EncryptedFileStore::new(path.clone(), fresh_passphrase());

            store.save(&fresh_identity()).unwrap();

            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "expected 0o600, got {mode:#o}");
        }

        #[test]
        fn file_is_exactly_230_bytes() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");
            let store = EncryptedFileStore::new(path.clone(), fresh_passphrase());
            store.save(&fresh_identity()).unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 230);
        }

        #[test]
        fn truncated_file_load_fails() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");
            let store = EncryptedFileStore::new(path.clone(), fresh_passphrase());
            store.save(&fresh_identity()).unwrap();

            // Truncate to 200 bytes.
            let bytes = std::fs::read(&path).unwrap();
            std::fs::write(&path, &bytes[..200]).unwrap();

            let err = store.load().unwrap_err();
            assert!(err.contains("expected 230 bytes"), "got: {err}");
        }
    }

    mod env {
        use super::*;
        use serial_test::serial;
        use secrecy::ExposeSecret;

        const HARMONY_PASSPHRASE: &str = "HARMONY_PASSPHRASE";
        const HARMONY_PASSPHRASE_FILE: &str = "HARMONY_PASSPHRASE_FILE";

        /// Clear both env vars before each test to avoid cross-test leakage.
        fn clear_env() {
            std::env::remove_var(HARMONY_PASSPHRASE);
            std::env::remove_var(HARMONY_PASSPHRASE_FILE);
        }

        #[test]
        #[serial]
        fn returns_none_when_no_env_var() {
            clear_env();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");
            assert!(EncryptedFileStore::from_env(path).unwrap().is_none());
        }

        #[test]
        #[serial]
        fn direct_env_var_set() {
            clear_env();
            std::env::set_var(HARMONY_PASSPHRASE, "foo");
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");
            let store = EncryptedFileStore::from_env(path).unwrap().expect("should be Some");
            assert_eq!(store.passphrase.expose_secret(), "foo");
            clear_env();
        }

        #[test]
        #[serial]
        fn file_var_set_strips_trailing_lf() {
            clear_env();
            let dir = tempfile::tempdir().unwrap();
            let pass_file = dir.path().join("pass.txt");
            std::fs::write(&pass_file, b"bar\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&pass_file, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            std::env::set_var(HARMONY_PASSPHRASE_FILE, &pass_file);

            let path = dir.path().join("identity.enc");
            let store = EncryptedFileStore::from_env(path).unwrap().expect("should be Some");
            assert_eq!(store.passphrase.expose_secret(), "bar");
            clear_env();
        }

        #[test]
        #[serial]
        fn file_var_set_strips_trailing_crlf() {
            clear_env();
            let dir = tempfile::tempdir().unwrap();
            let pass_file = dir.path().join("pass.txt");
            std::fs::write(&pass_file, b"bar\r\n").unwrap();
            std::env::set_var(HARMONY_PASSPHRASE_FILE, &pass_file);

            let path = dir.path().join("identity.enc");
            let store = EncryptedFileStore::from_env(path).unwrap().expect("should be Some");
            assert_eq!(store.passphrase.expose_secret(), "bar");
            clear_env();
        }

        #[test]
        #[serial]
        fn direct_wins_over_file() {
            clear_env();
            let dir = tempfile::tempdir().unwrap();
            let pass_file = dir.path().join("pass.txt");
            std::fs::write(&pass_file, b"from_file").unwrap();
            std::env::set_var(HARMONY_PASSPHRASE, "from_env");
            std::env::set_var(HARMONY_PASSPHRASE_FILE, &pass_file);

            let path = dir.path().join("identity.enc");
            let store = EncryptedFileStore::from_env(path).unwrap().expect("should be Some");
            assert_eq!(store.passphrase.expose_secret(), "from_env");
            clear_env();
        }

        #[test]
        #[serial]
        fn empty_direct_hard_fails() {
            clear_env();
            std::env::set_var(HARMONY_PASSPHRASE, "");
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");
            let err = EncryptedFileStore::from_env(path).unwrap_err();
            assert!(err.contains("empty"), "got: {err}");
            clear_env();
        }

        #[test]
        #[serial]
        fn empty_file_hard_fails() {
            clear_env();
            let dir = tempfile::tempdir().unwrap();
            let pass_file = dir.path().join("pass.txt");
            std::fs::write(&pass_file, b"\n").unwrap();  // strips to empty
            std::env::set_var(HARMONY_PASSPHRASE_FILE, &pass_file);

            let path = dir.path().join("identity.enc");
            let err = EncryptedFileStore::from_env(path).unwrap_err();
            assert!(err.contains("empty"), "got: {err}");
            clear_env();
        }

        #[test]
        #[serial]
        fn missing_file_hard_fails() {
            clear_env();
            std::env::set_var(HARMONY_PASSPHRASE_FILE, "/nonexistent/passphrase/file");
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");
            let err = EncryptedFileStore::from_env(path).unwrap_err();
            assert!(err.contains("could not be read"), "got: {err}");
            clear_env();
        }
    }

    mod legacy_plaintext_reader {
        use super::*;

        #[test]
        fn read_existing_plaintext() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.key");

            // Pre-populate via FileStore (which writes the same 161-byte format)
            let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
            let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
            let original = NodeIdentity { pq, ed25519 };
            let original_addr = original.ed25519.public_identity().address_hash;
            FileStore::new(path.clone()).save(&original).unwrap();

            // Read back via LegacyPlaintextReader
            let reader = LegacyPlaintextReader::new(path);
            let loaded = reader.read().unwrap().expect("should read plaintext");
            assert_eq!(
                loaded.ed25519.public_identity().address_hash,
                original_addr,
            );
        }

        #[test]
        fn read_returns_none_when_missing() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("nonexistent.key");
            let reader = LegacyPlaintextReader::new(path);
            assert!(reader.read().unwrap().is_none());
        }

        #[test]
        fn read_from_static_method_works() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.key");
            let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
            let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
            let original = NodeIdentity { pq, ed25519 };
            let original_addr = original.ed25519.public_identity().address_hash;
            FileStore::new(path.clone()).save(&original).unwrap();

            let loaded = LegacyPlaintextReader::read_from(&path)
                .unwrap()
                .expect("should read plaintext via static method");
            assert_eq!(loaded.ed25519.public_identity().address_hash, original_addr);
        }
    }
}
