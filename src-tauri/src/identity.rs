//! Node identity management — Ed25519 + post-quantum key generation and persistence.
//!
//! Identity at-rest is a 32-byte master seed; sub-keys are derived deterministically
//! via `NodeIdentity::from_seed` on every load. Storage backends (production):
//!
//! - `KeychainStore` — OS-native keychain via the `keyring` crate (primary on
//!   macOS / Windows / Linux-with-Secret-Service)
//! - `EncryptedFileStore` — Argon2id + XChaCha20-Poly1305 envelope at
//!   `~/.harmony/identity.enc`, keyed from the
//!   `HARMONY_PASSPHRASE` / `HARMONY_PASSPHRASE_FILE` env vars (headless installs)
//!
//! `FileStore` is retained behind `#[cfg(test)]` solely to write seed fixtures in
//! tests — production code never writes plaintext.
//!
//! `load_or_generate()` runs the resolution chain: keychain → encrypted file →
//! fresh-generate. Hard-fails with a pointer to `docs/headless-install.md` when no
//! destination is available. Wrong passphrase on the encrypted file is a hard fail
//! too — never silently regenerates identity.

use std::path::{Path, PathBuf};

use harmony_identity::{PqPrivateIdentity, PrivateIdentity};
use zeroize::Zeroizing;

/// Plaintext payload protected by the `HRMI` envelope: the master 32-byte
/// seed. Sub-key derivation is deterministic via `NodeIdentity::from_seed`
/// — the seed is the only secret on disk.
const BLOB_LEN: usize = 32;

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
const ENC_FILE_LEN: usize = HEADER_LEN + SALT_LEN + NONCE_LEN + BLOB_LEN + TAG_LEN;

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

impl NodeIdentity {
    /// Derive a `NodeIdentity` from a 32-byte master seed.
    ///
    /// Thin shim over `PrivateIdentity::from_seed` and
    /// `PqPrivateIdentity::from_seed` (ZEB-177). Deterministic: the same
    /// seed produces byte-identical sub-keys on every call. This is the
    /// load-bearing invariant for the seed-on-disk storage model — every
    /// launch reads the seed and re-derives the keypairs from scratch.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            pq: PqPrivateIdentity::from_seed(seed),
            ed25519: PrivateIdentity::from_seed(seed),
        }
    }
}

// ── Serialization helpers (shared by both backends) ─────────────────────

/// Serialize a 32-byte seed into the on-disk binary format. Identity at this
/// layer is *just* the seed — the sub-keys are derived deterministically via
/// `NodeIdentity::from_seed` on every load.
fn seed_to_blob(seed: &[u8; BLOB_LEN]) -> Zeroizing<Vec<u8>> {
    let mut buf = Zeroizing::new(Vec::with_capacity(BLOB_LEN));
    buf.extend_from_slice(seed);
    debug_assert_eq!(buf.len(), BLOB_LEN, "seed blob length mismatch");
    buf
}

/// Deserialize a 32-byte seed from a binary blob.
fn blob_to_seed(buf: &[u8]) -> Result<Zeroizing<[u8; BLOB_LEN]>, String> {
    if buf.len() != BLOB_LEN {
        return Err(format!(
            "identity store payload length is unexpected: expected {BLOB_LEN} bytes, got {}",
            buf.len()
        ));
    }
    let mut out: Zeroizing<[u8; BLOB_LEN]> = Zeroizing::new([0u8; BLOB_LEN]);
    out.copy_from_slice(buf);
    Ok(out)
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

    // Per-write unique temp file: a high-entropy suffix plus `create_new` makes
    // staging exclusive, so two processes (or two threads) writing the same
    // identity never share a `.tmp` and never publish a partial file. The
    // `create_new` flag also turns any pre-existing collision into an error
    // we surface rather than silently truncating someone else's in-flight save.
    let tmp_path = {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{:016x}.tmp", rand::random::<u64>()));
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
                .create_new(true)
                .mode(0o600)
                .open(&tmp_path)
                .map_err(|e| format!("Failed to create {}: {e}", tmp_path.display()))?
        };
        #[cfg(not(unix))]
        let f = {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
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
    // fsync the parent directory so the new directory entry survives a crash
    // immediately after rename. Without this, the temp file's contents are
    // durable but the rename is not. Unix-only (Windows doesn't expose
    // directory fsync via std).
    #[cfg(unix)]
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let dir = std::fs::File::open(parent)
            .map_err(|e| format!("Failed to open {} for fsync: {e}", parent.display()))?;
        dir.sync_all()
            .map_err(|e| format!("Failed to fsync {}: {e}", parent.display()))?;
    }
    std::mem::forget(guard);
    Ok(())
}

// ── Verify-after-write helper ──────────────────────────────────────────

/// After a `KeyStore::save`, immediately re-read and byte-compare against the
/// expected seed. Constant-time comparison via the `subtle` crate.
///
/// Returns Err if the store doesn't return what was written.
fn verify_round_trip(store: &dyn KeyStore, expected: &[u8; BLOB_LEN]) -> Result<(), String> {
    let loaded = store
        .load()
        .map_err(|e| format!("verify-after-write read-back failed: {e}"))?
        .ok_or_else(|| "verify-after-write read-back returned None".to_string())?;
    use subtle::ConstantTimeEq;
    if !bool::from(loaded.as_slice().ct_eq(expected.as_slice())) {
        return Err("verify-after-write returned a different seed than was just written".to_string());
    }
    Ok(())
}

// ── Encrypted file wire format helpers ─────────────────────────────────

/// Encode a 32-byte seed into the 101-byte encrypted-file format.
///
/// Caller supplies salt and nonce explicitly so the function is deterministic
/// for testing. Production code generates fresh random values per save.
#[doc(hidden)]
pub fn encrypt_with_params(
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

/// Decode a 101-byte encrypted-file blob back into the 32-byte seed.
///
/// Indistinguishable error for wrong-passphrase vs corrupted-ciphertext to
/// avoid leaking which case occurred (an attacker who can probe with arbitrary
/// passphrases gains no signal from the error message).
///
/// Returns `Zeroizing<[u8; BLOB_LEN]>` so the caller's stack-resident copy of
/// the plaintext seed bytes is wiped on drop. The intermediate `Vec<u8>` from
/// `cipher.decrypt(...)` is also wrapped in `Zeroizing` before any further use.
#[doc(hidden)]
pub fn decrypt(passphrase: &[u8], bytes: &[u8]) -> Result<Zeroizing<[u8; BLOB_LEN]>, String> {
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

    // Strict v1 KDF param check: refuse to allocate Argon2 memory on
    // attacker-controlled values. The AAD binding via the Poly1305 tag would
    // reject mismatched params eventually, but only AFTER hash_password_into
    // attempts the m_kib allocation — which is a DoS vector if m_kib is
    // 16 GiB. v1 hardcodes all three params, so any other value is either a
    // future format (handled by the format_version check above) or tampering.
    // Return the indistinguishable error to leak nothing about which it is.
    if m_kib != KDF_M_KIB || t != KDF_T as u32 || p != KDF_P as u32 {
        return Err(
            "identity store could not be decrypted: wrong passphrase or corrupted file".to_string(),
        );
    }
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

    // Validate length, then copy directly into a Zeroizing-protected buffer
    // (no intermediate unprotected stack array). The borrowed slice points
    // into `plaintext`'s heap buffer, which is itself in Zeroizing<Vec<u8>>.
    let plaintext_slice: &[u8; BLOB_LEN] = plaintext
        .as_slice()
        .try_into()
        .map_err(|_| format!("decrypted plaintext was {} bytes, expected {}", plaintext.len(), BLOB_LEN))?;
    let mut blob_arr: Zeroizing<[u8; BLOB_LEN]> = Zeroizing::new([0u8; BLOB_LEN]);
    blob_arr.copy_from_slice(plaintext_slice);
    Ok(blob_arr)
}

// ── KeyStore trait ──────────────────────────────────────────────────────

/// Common interface for identity storage backends.
pub trait KeyStore {
    /// Load the master seed from this store. Returns `Ok(None)` if no entry exists.
    fn load(&self) -> Result<Option<Zeroizing<[u8; BLOB_LEN]>>, String>;
    /// Save the master seed to this store.
    fn save(&self, seed: &[u8; BLOB_LEN]) -> Result<(), String>;
}

// ── FileStore ───────────────────────────────────────────────────────────

/// File-based identity storage at a given path.
///
/// Test-only fixture helper for writing legacy plaintext blobs. Production
/// code reads legacy plaintext through `LegacyPlaintextReader` and writes
/// only through `KeychainStore` or `EncryptedFileStore`.
#[cfg(test)]
pub struct FileStore {
    path: PathBuf,
}

#[cfg(test)]
impl FileStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

}

// FileStore is retained as a test-only helper for setting up seed fixtures.
// Production code never writes plaintext — all writes go through
// KeychainStore or EncryptedFileStore.
#[cfg(test)]
impl KeyStore for FileStore {
    fn load(&self) -> Result<Option<Zeroizing<[u8; BLOB_LEN]>>, String> {
        let raw = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("Failed to read {}: {e}", self.path.display())),
        };
        let buf = Zeroizing::new(raw);
        let seed = blob_to_seed(&buf)?;
        #[cfg(unix)]
        warn_permissions(&self.path);
        Ok(Some(seed))
    }

    fn save(&self, seed: &[u8; BLOB_LEN]) -> Result<(), String> {
        let blob = seed_to_blob(seed);
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
    #[allow(dead_code)]
    pub fn new_failing_mock() -> Self {
        let credential = AlwaysFailOnSave;
        let entry = keyring::Entry::new_with_credential(Box::new(credential));
        Self { entry }
    }

    /// Create a store where ALL operations fail (simulates inaccessible keychain).
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn new_load_failing_mock() -> Self {
        let credential = AlwaysFailOnLoad;
        let entry = keyring::Entry::new_with_credential(Box::new(credential));
        Self { entry }
    }
}

impl KeyStore for KeychainStore {
    fn load(&self) -> Result<Option<Zeroizing<[u8; BLOB_LEN]>>, String> {
        match self.entry.get_secret() {
            Ok(bytes) => {
                let buf = Zeroizing::new(bytes);
                let seed = blob_to_seed(&buf)?;
                Ok(Some(seed))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("keychain load failed: {e}")),
        }
    }

    fn save(&self, seed: &[u8; BLOB_LEN]) -> Result<(), String> {
        let blob = seed_to_blob(seed);
        self.entry
            .set_secret(&blob)
            .map_err(|e| format!("keychain save failed: {e}"))
    }
}

// ── Passphrase-file parser ─────────────────────────────────────────────

/// Read a passphrase from `path`, with the same parsing rules used for
/// `HARMONY_PASSPHRASE_FILE` and the `--new-passphrase-file` CLI flag:
///
///   1. Read raw bytes
///   2. Warn (Unix only) if the file mode is more permissive than 0600
///   3. UTF-8 decode (zeroizing the bytes if decode fails)
///   4. Strip exactly one trailing `\r\n` or `\n`
///   5. Reject empty result
///
/// Returned errors are *unprefixed* — callers add their own context (e.g.,
/// "HARMONY_PASSPHRASE_FILE=<path> ..." or "--new-passphrase-file=<path> ...")
/// because the same parser is invoked from both env-var and CLI paths.
pub(crate) fn parse_passphrase_file(path: &Path) -> Result<String, String> {
    let raw = std::fs::read(path).map_err(|e| format!("could not be read: {e}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                tracing::warn!(
                    path = %path.display(),
                    mode = format!("{mode:#05o}"),
                    "passphrase file has open permissions, should be 0600"
                );
            }
        }
    }

    // On UTF-8 failure, the FromUtf8Error owns the original Vec<u8> — zeroize
    // those bytes before dropping the error so the (sensitive) raw passphrase
    // bytes don't linger on the heap.
    let mut s = String::from_utf8(raw).map_err(|e| {
        use zeroize::Zeroize;
        let mut bytes = e.into_bytes();
        bytes.zeroize();
        "is not valid UTF-8".to_string()
    })?;
    if s.ends_with("\r\n") {
        s.truncate(s.len() - 2);
    } else if s.ends_with('\n') {
        s.truncate(s.len() - 1);
    }
    if s.is_empty() {
        return Err("contains an empty passphrase (after trimming one trailing newline)".to_string());
    }
    Ok(s)
}

// ── EncryptedFileStore ─────────────────────────────────────────────────

use secrecy::{ExposeSecret, SecretString};

/// Passphrase-encrypted identity file at a given path.
///
/// On-disk format is the 101-byte layout produced by `encrypt_with_params`:
/// Argon2id (m=64MiB, t=3, p=1) derives a 32-byte key for XChaCha20-Poly1305
/// AEAD over the 32-byte master seed. The 13-byte header (magic, version,
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

    /// Constant-time check whether `candidate` matches the stored passphrase.
    ///
    /// Used by the CLI rotate handler to detect a no-op rotation (old == new) so
    /// it can emit a warning without aborting.
    pub(crate) fn passphrase_eq(&self, candidate: &SecretString) -> bool {
        use secrecy::ExposeSecret;
        bool::from(subtle::ConstantTimeEq::ct_eq(
            self.passphrase.expose_secret().as_bytes(),
            candidate.expose_secret().as_bytes(),
        ))
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
            parse_passphrase_file(Path::new(&file_path))
                .map_err(|e| format!("HARMONY_PASSPHRASE_FILE={file_path} {e}"))?
        } else {
            return Ok(None);
        };

        Ok(Some(Self::new(path, SecretString::from(passphrase_str))))
    }
}

impl KeyStore for EncryptedFileStore {
    fn load(&self) -> Result<Option<Zeroizing<[u8; BLOB_LEN]>>, String> {
        let raw = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("Failed to read {}: {e}", self.path.display())),
        };
        // `decrypt` returns Zeroizing<[u8; BLOB_LEN]> — the underlying [u8; 32]
        // is wiped on drop. The seed array can be returned directly without
        // a second deserialization step.
        let seed = decrypt(self.passphrase.expose_secret().as_bytes(), &raw)?;
        Ok(Some(seed))
    }

    fn save(&self, seed: &[u8; BLOB_LEN]) -> Result<(), String> {
        let mut salt = [0u8; SALT_LEN];
        let mut nonce = [0u8; NONCE_LEN];
        use rand::RngCore;
        rand::rngs::OsRng.fill_bytes(&mut salt);
        rand::rngs::OsRng.fill_bytes(&mut nonce);

        let bytes = encrypt_with_params(
            self.passphrase.expose_secret().as_bytes(),
            &salt,
            &nonce,
            seed,
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
///
/// Resolution order:
///   1. keychain.load() — return on success; fall through on None or Err
///   2. encrypted.load() — return on success; HARD FAIL on Err (wrong
///      passphrase / corruption — never silently regenerate)
///   3. fresh generate → save 32B seed to keychain (preferred) or encrypted
///
/// Hard-fails when no destination is available (no keychain, no encrypted store).
/// Pre-ZEB-176 plaintext `~/.harmony/identity.key` files are no longer
/// auto-migrated — users with a placeholder pre-ZEB-176 identity hard-fail
/// and re-mint (acceptable per spec scope).
fn load_or_generate_with_stores(
    keychain: Option<&KeychainStore>,
    encrypted: Option<&EncryptedFileStore>,
) -> Result<Zeroizing<[u8; BLOB_LEN]>, String> {
    let mut keychain_healthy = false;
    if let Some(kc) = keychain {
        match kc.load() {
            Ok(Some(seed)) => return Ok(seed),
            Ok(None) => keychain_healthy = true,
            Err(e) => {
                tracing::warn!("keychain load failed, trying next store: {e}");
            }
        }
    }
    load_or_generate_with_stores_post_probe(keychain, keychain_healthy, encrypted)
}

fn load_or_generate_with_stores_post_probe(
    keychain: Option<&KeychainStore>,
    keychain_healthy: bool,
    encrypted: Option<&EncryptedFileStore>,
) -> Result<Zeroizing<[u8; BLOB_LEN]>, String> {
    if let Some(enc) = encrypted {
        match enc.load() {
            Ok(Some(seed)) => return Ok(seed),
            Ok(None) => { /* fall through to fresh-generate */ }
            Err(e) => return Err(e),
        }
    }

    let mut seed_buf: Zeroizing<[u8; BLOB_LEN]> = Zeroizing::new([0u8; BLOB_LEN]);
    use rand::RngCore;
    rand::rngs::OsRng.fill_bytes(seed_buf.as_mut());
    save_with_fallback(
        keychain_healthy,
        keychain,
        encrypted,
        &seed_buf,
        || "no identity store available: keychain unavailable and HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set — see docs/headless-install.md".to_string(),
        |e| format!(
            "keychain save failed and no encrypted fallback configured: {e} — see docs/headless-install.md"
        ),
    )?;
    Ok(seed_buf)
}

/// Save `seed` to the preferred destination (keychain > encrypted), with
/// fallback when the keychain save/verify fails. Used by the fresh-generate
/// path so the destination-selection-with-fallback logic isn't duplicated.
///
/// `no_dest_err` produces the error returned when no destination is available
/// at all (no keychain, no encrypted store). `keychain_failed_no_enc_err`
/// produces the error returned when the keychain was tried, the save failed,
/// AND there's no encrypted store to fall back to — the keychain failure
/// message is passed to it for context.
fn save_with_fallback(
    keychain_healthy: bool,
    keychain: Option<&KeychainStore>,
    encrypted: Option<&EncryptedFileStore>,
    seed: &[u8; BLOB_LEN],
    no_dest_err: impl FnOnce() -> String,
    keychain_failed_no_enc_err: impl FnOnce(&str) -> String,
) -> Result<(), String> {
    let mut keychain_err: Option<String> = None;

    if keychain_healthy {
        let kc = keychain.expect("keychain_healthy implies Some(keychain)");
        // Save and verify are split deliberately: a save Err means nothing
        // landed in the keychain, so we can safely fall back to encrypted.
        // A verify Err is different — the save APPEARED to succeed (the
        // keychain may now contain a corrupt entry that load_or_generate's
        // step 1 would prefer on next boot), so we MUST NOT fall back; doing
        // so would mask the corruption and the user's real identity would
        // diverge between the two backends. Verify Err is terminal here.
        match kc.save(seed) {
            Ok(()) => {
                if let Err(verify_err) = verify_round_trip(kc, seed) {
                    tracing::error!(
                        "keychain save succeeded but verify-after-write failed: {verify_err}. \
                         The keychain entry may be corrupt and would be preferred over any \
                         encrypted-file fallback on next boot. Refusing to proceed. Manual \
                         cleanup likely needed: delete the 'harmony/identity' keychain entry \
                         before retrying."
                    );
                    return Err(verify_err);
                }
                tracing::info!("identity stored in OS keychain");
                return Ok(());
            }
            Err(e) => {
                tracing::warn!(
                    "keychain save failed: {e}; trying encrypted fallback if available"
                );
                keychain_err = Some(e);
            }
        }
    }

    if let Some(enc) = encrypted {
        enc.save(seed)?;
        verify_round_trip(enc, seed)?;
        tracing::info!(path = %enc.path().display(), "identity stored in encrypted file");
        Ok(())
    } else if let Some(e) = keychain_err {
        Err(keychain_failed_no_enc_err(&e))
    } else {
        Err(no_dest_err())
    }
}

/// Public entry point — resolves env-derived encrypted store, attempts the
/// keychain, and runs the resolution chain. Returns the derived `NodeIdentity`.
pub fn load_or_generate(plaintext_path: &Path) -> Result<NodeIdentity, String> {
    let seed = read_seed_from_disk_with_keychain(plaintext_path, KeychainStore::new().ok())?;
    Ok(NodeIdentity::from_seed(&seed))
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn load_or_generate_with_keychain(
    plaintext_path: &Path,
    keychain: Option<KeychainStore>,
) -> Result<NodeIdentity, String> {
    let seed = read_seed_from_disk_with_keychain(plaintext_path, keychain)?;
    Ok(NodeIdentity::from_seed(&seed))
}

/// Read the master seed from disk via the standard resolution chain
/// (keychain → encrypted file → fresh-generate). Returns the seed bytes
/// directly so the recovery CLI can encode them without first deriving a
/// `NodeIdentity`.
pub fn read_seed_from_disk(plaintext_path: &Path) -> Result<Zeroizing<[u8; BLOB_LEN]>, String> {
    read_seed_from_disk_with_keychain(plaintext_path, KeychainStore::new().ok())
}

/// Inner entry point. Integration tests (across the crate boundary) inject a
/// deterministic keychain. `pub` rather than `pub(crate)` so
/// `tests/recovery_cli_integration.rs` can reach it.
pub fn read_seed_from_disk_with_keychain(
    plaintext_path: &Path,
    keychain: Option<KeychainStore>,
) -> Result<Zeroizing<[u8; BLOB_LEN]>, String> {
    let mut keychain_probe_ok = false;
    if let Some(kc) = &keychain {
        match kc.load() {
            Ok(Some(seed)) => return Ok(seed),
            Ok(None) => keychain_probe_ok = true,
            Err(e) => {
                tracing::warn!(
                    "keychain probe failed in read_seed_from_disk ({e}); env-var \
                     configuration errors will stay fatal"
                );
            }
        }
    }

    let enc_path = plaintext_path.with_file_name("identity.enc");
    let encrypted = match EncryptedFileStore::from_env(enc_path) {
        Ok(opt) => opt,
        Err(e) if keychain_probe_ok => {
            tracing::warn!(
                "HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE configured but invalid \
                 ({e}); ignoring — keychain is available as fallback"
            );
            None
        }
        Err(e) => return Err(e),
    };

    load_or_generate_with_stores(keychain.as_ref(), encrypted.as_ref())
}

/// Re-encrypt the identity at `old.path()` with `new_passphrase`.
///
/// Loads the identity using the old store's passphrase, writes back to the same
/// path with the new passphrase (fresh salt + nonce, atomic rename), and
/// verifies the round-trip before returning.
///
/// Caller-side concerns (keychain check, env var resolution, CLI wiring) live
/// in `main.rs` — this function is the pure key-rotation primitive.
pub(crate) fn rotate_passphrase(
    old: &EncryptedFileStore,
    new_passphrase: SecretString,
) -> Result<(), String> {
    let seed = old
        .load()?
        .ok_or_else(|| {
            format!(
                "no encrypted identity to rotate at {}",
                old.path().display()
            )
        })?;

    let new_store = EncryptedFileStore::new(old.path().to_path_buf(), new_passphrase);
    new_store.save(&seed)?;
    // After save() returns Ok, the file at `old.path()` has been atomically
    // replaced and is now decryptable ONLY by the new passphrase. A
    // verify-after-write failure here is a transient I/O / corruption signal,
    // not a "rotation didn't happen" signal — the operator MUST keep the new
    // passphrase or they lose access to their identity. Rewrite the error so
    // a panicked operator doesn't discard the new passphrase file.
    verify_round_trip(&new_store, &seed).map_err(|e| {
        format!(
            "{} was rewritten with the new passphrase, but the verify-after-write \
             read-back failed: {e}. The new passphrase is now REQUIRED to decrypt \
             this file — do NOT discard the new passphrase file. Investigate the \
             read-back failure (disk error? concurrent writer?) and re-run \
             rotate-passphrase if needed once the underlying issue is resolved.",
            old.path().display()
        )
    })?;
    Ok(())
}

/// A `CredentialApi` implementation that always returns `NoEntry` on reads
/// and `Error::Invalid` on writes. Used only in tests for failure-path coverage.
#[cfg(test)]
#[allow(dead_code)]
#[derive(Debug)]
struct AlwaysFailOnSave;

/// A `CredentialApi` implementation that returns a platform error on ALL operations.
/// Simulates a keychain that is constructed successfully but fails at I/O time
/// (e.g., locked macOS keychain, missing D-Bus session).
#[cfg(test)]
#[allow(dead_code)]
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

#[cfg(all(unix, test))]
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

/// Test-only re-exports. Used by integration tests in `tests/` to pin the
/// wire format. Production code MUST NOT use these.
///
/// `decrypt_for_test` isn't called by any current test but is exported
/// symmetrically with `encrypt_with_params_for_test` so future round-trip
/// fixture tests can use either side without re-touching this module.
#[doc(hidden)]
pub mod test_only {
    pub use super::decrypt as decrypt_for_test;
    pub use super::encrypt_with_params as encrypt_with_params_for_test;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        let store = FileStore::new(path.clone());

        let mut original = [0u8; BLOB_LEN];
        use rand::RngCore;
        rand::rngs::OsRng.fill_bytes(&mut original);

        store.save(&original).unwrap();
        let loaded = store.load().unwrap().expect("should find saved seed");
        assert_eq!(*loaded, original);
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
        let store = KeychainStore::new_mock();

        let mut original = [0u8; BLOB_LEN];
        use rand::RngCore;
        rand::rngs::OsRng.fill_bytes(&mut original);

        store.save(&original).unwrap();
        let loaded = store.load().unwrap().expect("should find saved seed");
        assert_eq!(*loaded, original);
    }

    #[test]
    fn keychain_store_load_returns_none_when_empty() {
        let store = KeychainStore::new_mock();
        assert!(store.load().unwrap().is_none());
    }

    mod wire_format {
        use super::*;

        const TEST_PASSPHRASE: &[u8] = b"correct horse battery staple";
        const TEST_SALT: [u8; 16] = [0xAB; 16];
        const TEST_NONCE: [u8; 24] = [0xCD; 24];
        const TEST_BLOB: [u8; 32] = [0x42; 32];

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
            bytes[60] ^= 0x01;  // flip one bit in the ciphertext range (53..85)
            let err = decrypt(TEST_PASSPHRASE, &bytes).unwrap_err();
            assert!(err.contains("wrong passphrase or corrupted file"));
        }

        #[test]
        fn tampered_kdf_params_fails() {
            let mut bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
            // Flip a byte in kdf_m_kib (offset 6..10). For v1 this fires the
            // strict-equality KDF param check (which also avoids allocating
            // attacker-controlled Argon2 memory). The 13-byte header is also
            // bound as AAD, so even if the strict check were ever removed, the
            // Poly1305 tag would reject the same tamper — defense in depth.
            bytes[7] ^= 0x01;
            let err = decrypt(TEST_PASSPHRASE, &bytes).unwrap_err();
            assert!(
                err.contains("wrong passphrase or corrupted file"),
                "tampered KDF params must be rejected, got: {err}"
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
            let err = decrypt(TEST_PASSPHRASE, &bytes[..70]).unwrap_err();
            assert!(err.contains("expected 101 bytes"), "got: {err}");
        }

        #[test]
        fn output_is_exactly_101_bytes() {
            let bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
            assert_eq!(bytes.len(), 101);
        }

        #[test]
        fn header_layout_is_exact() {
            let bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
            // 101-byte file: 13-byte header, 16-byte salt, 24-byte nonce, 32-byte ciphertext, 16-byte tag.
            // NOTE: kdf_t is u16 BE (not u32) so the header fits in 13 bytes total.
            assert_eq!(&bytes[0..4], b"HRMI", "magic mismatch");
            assert_eq!(bytes[4], 0x01, "format_version mismatch");
            assert_eq!(bytes[5], 0x01, "kdf_id mismatch");
            assert_eq!(&bytes[6..10], &65536u32.to_be_bytes(), "kdf_m_kib (u32 BE) mismatch");
            assert_eq!(&bytes[10..12], &3u16.to_be_bytes(), "kdf_t (u16 BE) mismatch");
            assert_eq!(bytes[12], 1, "kdf_p (u8) mismatch");
            assert_eq!(&bytes[13..29], &TEST_SALT[..], "salt mismatch");
            assert_eq!(&bytes[29..53], &TEST_NONCE[..], "nonce mismatch");
            assert_eq!(bytes.len(), 101);
        }
    }

    mod encrypted_file_store {
        use super::*;
        use secrecy::SecretString;

        fn fresh_seed() -> [u8; BLOB_LEN] {
            let mut buf = [0u8; BLOB_LEN];
            use rand::RngCore;
            rand::rngs::OsRng.fill_bytes(&mut buf);
            buf
        }

        fn fresh_passphrase() -> SecretString {
            SecretString::from("correct horse battery staple".to_string())
        }

        #[test]
        fn round_trip_correct_passphrase() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");
            let store = EncryptedFileStore::new(path.clone(), fresh_passphrase());

            let original = fresh_seed();
            store.save(&original).unwrap();
            let loaded = store.load().unwrap().expect("should find saved seed");
            assert_eq!(*loaded, original);
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
                .save(&fresh_seed())
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
            let seed = fresh_seed();

            store.save(&seed).unwrap();
            let bytes_a = std::fs::read(&path).unwrap();
            store.save(&seed).unwrap();
            let bytes_b = std::fs::read(&path).unwrap();

            assert_ne!(bytes_a, bytes_b, "salt+nonce must rotate per save");
            // Both must still load back to the same seed:
            let loaded = store.load().unwrap().unwrap();
            assert_eq!(*loaded, seed);
        }

        #[cfg(unix)]
        #[test]
        fn file_mode_0o600_unix() {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");
            let store = EncryptedFileStore::new(path.clone(), fresh_passphrase());

            store.save(&fresh_seed()).unwrap();

            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "expected 0o600, got {mode:#o}");
        }

        #[test]
        fn file_is_exactly_101_bytes() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");
            let store = EncryptedFileStore::new(path.clone(), fresh_passphrase());
            store.save(&fresh_seed()).unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 101);
        }

        #[test]
        fn truncated_file_load_fails() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");
            let store = EncryptedFileStore::new(path.clone(), fresh_passphrase());
            store.save(&fresh_seed()).unwrap();

            // Truncate to 70 bytes.
            let bytes = std::fs::read(&path).unwrap();
            std::fs::write(&path, &bytes[..70]).unwrap();

            let err = store.load().unwrap_err();
            assert!(err.contains("expected 101 bytes"), "got: {err}");
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

    mod resolution_chain {
        use super::*;
        use secrecy::SecretString;

        fn fresh_seed() -> [u8; BLOB_LEN] {
            let mut buf = [0u8; BLOB_LEN];
            use rand::RngCore;
            rand::rngs::OsRng.fill_bytes(&mut buf);
            buf
        }

        fn fresh_passphrase() -> SecretString {
            SecretString::from("correct horse battery staple".to_string())
        }

        #[test]
        fn keychain_present_returns_keychain() {
            let original = fresh_seed();
            let keychain = KeychainStore::new_mock();
            keychain.save(&original).unwrap();

            let result = load_or_generate_with_stores(Some(&keychain), None).unwrap();
            assert_eq!(*result, original);
        }

        #[test]
        fn fresh_install_writes_to_keychain() {
            let keychain = KeychainStore::new_mock();
            let result = load_or_generate_with_stores(Some(&keychain), None).unwrap();

            let from_keychain = keychain.load().unwrap().expect("seed should be in keychain");
            assert_eq!(*from_keychain, *result);
        }

        #[test]
        fn migrate_plaintext_to_keychain_and_unlink() {
            // Post-ZEB-176: no plaintext migration. Seed saved directly to keychain.
            let original = fresh_seed();
            let keychain = KeychainStore::new_mock();
            keychain.save(&original).unwrap();
            let result = load_or_generate_with_stores(Some(&keychain), None).unwrap();
            assert_eq!(*result, original);
        }

        #[test]
        fn fresh_install_writes_to_encrypted_when_no_keychain() {
            let dir = tempfile::tempdir().unwrap();
            let enc_path = dir.path().join("identity.enc");

            let encrypted = EncryptedFileStore::new(enc_path.clone(), fresh_passphrase());
            let result = load_or_generate_with_stores(None, Some(&encrypted)).unwrap();

            assert!(enc_path.exists());
            let from_enc = encrypted.load().unwrap().expect("should be in .enc");
            assert_eq!(*from_enc, *result);
        }

        #[test]
        fn headless_no_keychain_no_env_hard_fails_on_fresh() {
            let err = load_or_generate_with_stores(None, None).unwrap_err();
            assert!(err.contains("no identity store available"), "got: {err}");
            assert!(err.contains("docs/headless-install.md"), "should point at docs: {err}");
        }

        #[test]
        fn wrong_passphrase_does_not_regenerate() {
            let dir = tempfile::tempdir().unwrap();
            let enc_path = dir.path().join("identity.enc");

            // Write an .enc with passphrase A.
            let original = fresh_seed();
            EncryptedFileStore::new(enc_path.clone(), fresh_passphrase())
                .save(&original)
                .unwrap();

            // Try to load with wrong passphrase B.
            let wrong = EncryptedFileStore::new(enc_path.clone(), SecretString::from("WRONG".to_string()));
            let err = load_or_generate_with_stores(None, Some(&wrong)).unwrap_err();
            assert!(err.contains("wrong passphrase or corrupted file"), "got: {err}");

            // Critically: original .enc must still be intact (not regenerated).
            let recovered = EncryptedFileStore::new(enc_path.clone(), fresh_passphrase())
                .load()
                .unwrap()
                .expect("original .enc must still be loadable with correct passphrase");
            assert_eq!(
                *recovered,
                original,
                "wrong-passphrase must NOT trigger fresh generate",
            );
        }

        /// Keychain Err (transient OS-keychain failure) is recoverable: the
        /// chain falls through to the encrypted backend rather than hard-failing.
        /// This is the asymmetry between step 1 (recoverable) and step 2
        /// (hard-fail) — exercised here with `new_load_failing_mock` which
        /// errors on every load attempt.
        #[test]
        fn keychain_err_falls_through_to_encrypted() {
            let dir = tempfile::tempdir().unwrap();
            let enc_path = dir.path().join("identity.enc");

            let keychain = KeychainStore::new_load_failing_mock();
            let encrypted = EncryptedFileStore::new(enc_path.clone(), fresh_passphrase());

            let result = load_or_generate_with_stores(
                Some(&keychain),
                Some(&encrypted),
            )
            .expect("keychain Err must fall through, not hard-fail");

            // Seed ended up in the encrypted store, not the keychain.
            assert!(enc_path.exists(), "encrypted file should be the destination");
            let from_enc = encrypted
                .load()
                .unwrap()
                .expect("encrypted store should hold the new seed");
            assert_eq!(*from_enc, *result);
        }

        /// Fresh generate: keychain accepted load (Ok(None)) but rejected save.
        /// Should fall back to the encrypted backend rather than hard-failing.
        #[test]
        fn keychain_save_failure_falls_back_to_encrypted_on_fresh() {
            let dir = tempfile::tempdir().unwrap();
            let enc_path = dir.path().join("identity.enc");

            // new_failing_mock: load returns NoEntry (so step 1's Ok(None)
            // sets keychain_healthy = true), but save returns Err.
            let keychain = KeychainStore::new_failing_mock();
            let encrypted = EncryptedFileStore::new(enc_path.clone(), fresh_passphrase());

            let result = load_or_generate_with_stores(
                Some(&keychain),
                Some(&encrypted),
            )
            .expect("must fall back to encrypted, not hard-fail");

            assert!(enc_path.exists(), "encrypted file should be the destination");
            let from_enc = encrypted
                .load()
                .unwrap()
                .expect("encrypted store should hold the new seed");
            assert_eq!(*from_enc, *result);
            // Sanity: the keychain entry was NOT persisted (mock fails save).
            assert!(
                keychain.load().unwrap().is_none(),
                "failing keychain mock must not have persisted anything"
            );
        }

        /// verify_round_trip: custom store that returns a different seed on load.
        #[test]
        fn verify_round_trip_detects_mismatch() {
            struct CorruptingStore { inner: KeychainStore }
            impl KeyStore for CorruptingStore {
                fn save(&self, seed: &[u8; BLOB_LEN]) -> Result<(), String> { self.inner.save(seed) }
                fn load(&self) -> Result<Option<Zeroizing<[u8; BLOB_LEN]>>, String> {
                    // Always return a different seed to force mismatch.
                    let mut buf: Zeroizing<[u8; BLOB_LEN]> = Zeroizing::new([0u8; BLOB_LEN]);
                    use rand::RngCore;
                    rand::rngs::OsRng.fill_bytes(buf.as_mut());
                    Ok(Some(buf))
                }
            }

            let original = fresh_seed();
            let store = CorruptingStore { inner: KeychainStore::new_mock() };
            let err = verify_round_trip(&store, &original).unwrap_err();
            assert!(err.contains("verify-after-write"), "got: {err}");
        }

        // ── from_env error policy: scoped to a successful keychain probe ──
        //
        // Regression coverage for the bug where `keychain.is_some()` was used as
        // the swallowing predicate, causing an unreadable HARMONY_PASSPHRASE_FILE
        // to be silently demoted to a warning even when the keychain probe had
        // errored — masking the env-var failure and routing the caller to a
        // misleading "no identity store available" path. The fix tracks
        // `keychain_probe_ok` separately so the swallow only fires when the
        // keychain actually responded.

        /// Probe-failed keychain + invalid env var must surface the env error,
        /// not silently fall through to a "no destination" message.
        #[test]
        #[serial_test::serial]
        fn keychain_probe_err_propagates_env_error() {
            std::env::remove_var("HARMONY_PASSPHRASE");
            std::env::set_var("HARMONY_PASSPHRASE_FILE", "/nonexistent/passphrase/file");

            let dir = tempfile::tempdir().unwrap();
            let plaintext_path = dir.path().join("identity.key");

            let kc = KeychainStore::new_load_failing_mock();
            let err = load_or_generate_with_keychain(&plaintext_path, Some(kc)).unwrap_err();

            assert!(
                err.contains("could not be read"),
                "expected the env-var read error to surface, got: {err}",
            );
            assert!(
                !err.contains("no identity store available"),
                "must NOT mask env error with the no-destination message: {err}",
            );

            std::env::remove_var("HARMONY_PASSPHRASE_FILE");
        }

        /// Healthy-empty keychain + invalid env var: swallow the env error
        /// (warning only) and let the chain mint into the keychain. Positive
        /// control matching the policy from the docstring on `load_or_generate`.
        #[test]
        #[serial_test::serial]
        fn keychain_probe_ok_swallows_env_error() {
            std::env::remove_var("HARMONY_PASSPHRASE");
            std::env::set_var("HARMONY_PASSPHRASE_FILE", "/nonexistent/passphrase/file");

            let dir = tempfile::tempdir().unwrap();
            let plaintext_path = dir.path().join("identity.key");

            // Healthy mock keychain: load returns Ok(None), save succeeds.
            let kc = KeychainStore::new_mock();
            let result = load_or_generate_with_keychain(&plaintext_path, Some(kc));

            assert!(
                result.is_ok(),
                "healthy keychain should swallow env error and mint fresh: {:?}",
                result.err(),
            );

            std::env::remove_var("HARMONY_PASSPHRASE_FILE");
        }
    }

    #[test]
    fn seed_round_trip_via_blob() {
        let seed = [0xABu8; 32];
        let blob = seed_to_blob(&seed);
        let recovered = blob_to_seed(blob.as_slice()).unwrap();
        assert_eq!(seed, *recovered, "seed must round-trip byte-for-byte through blob serialization");
    }

    #[test]
    fn from_seed_yields_same_node_identity_across_launches() {
        let seed = [0x42u8; 32];
        let id_a = NodeIdentity::from_seed(&seed);
        let id_b = NodeIdentity::from_seed(&seed);
        assert_eq!(
            id_a.ed25519.to_private_bytes().as_slice(),
            id_b.ed25519.to_private_bytes().as_slice(),
            "Ed25519 sub-key must be deterministic across calls"
        );
        assert_eq!(
            id_a.pq.to_private_bytes().as_slice(),
            id_b.pq.to_private_bytes().as_slice(),
            "PQ sub-key must be deterministic across calls"
        );
    }

    mod rotation {
        use super::*;
        use secrecy::SecretString;

        fn fresh_seed() -> [u8; BLOB_LEN] {
            let mut buf = [0u8; BLOB_LEN];
            use rand::RngCore;
            rand::rngs::OsRng.fill_bytes(&mut buf);
            buf
        }

        #[test]
        fn rotate_happy_path() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");

            let pass_a = SecretString::from("pass_a".to_string());
            let pass_b = SecretString::from("pass_b".to_string());

            let seed = fresh_seed();

            // Write with A.
            EncryptedFileStore::new(path.clone(), pass_a.clone())
                .save(&seed)
                .unwrap();

            // Rotate to B.
            let store_a = EncryptedFileStore::new(path.clone(), pass_a.clone());
            rotate_passphrase(&store_a, pass_b.clone()).unwrap();

            // B can decrypt and returns the same seed.
            let loaded = EncryptedFileStore::new(path.clone(), pass_b)
                .load()
                .unwrap()
                .unwrap();
            assert_eq!(*loaded, seed);

            // A can no longer decrypt.
            let err = EncryptedFileStore::new(path, pass_a).load().unwrap_err();
            assert!(err.contains("wrong passphrase or corrupted file"), "got: {err}");
        }

        #[test]
        fn rotate_wrong_old_passphrase_fails() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");

            EncryptedFileStore::new(path.clone(), SecretString::from("real".to_string()))
                .save(&fresh_seed())
                .unwrap();

            let bytes_before = std::fs::read(&path).unwrap();

            let wrong = EncryptedFileStore::new(path.clone(), SecretString::from("wrong".to_string()));
            let err = rotate_passphrase(&wrong, SecretString::from("new".to_string())).unwrap_err();
            assert!(err.contains("wrong passphrase or corrupted file"), "got: {err}");

            // File untouched.
            let bytes_after = std::fs::read(&path).unwrap();
            assert_eq!(bytes_before, bytes_after, "file must not be modified on auth failure");
        }

        #[test]
        fn rotate_to_same_passphrase_succeeds_with_new_salt_nonce() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");
            let pass = SecretString::from("same".to_string());

            EncryptedFileStore::new(path.clone(), pass.clone())
                .save(&fresh_seed())
                .unwrap();
            let bytes_before = std::fs::read(&path).unwrap();

            let store = EncryptedFileStore::new(path.clone(), pass.clone());
            rotate_passphrase(&store, pass.clone()).unwrap();

            let bytes_after = std::fs::read(&path).unwrap();
            assert_ne!(bytes_before, bytes_after, "salt+nonce must rotate even when passphrase is same");
        }

        #[test]
        fn rotate_no_file_fails() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");
            let store = EncryptedFileStore::new(path, SecretString::from("any".to_string()));

            let err = rotate_passphrase(&store, SecretString::from("new".to_string())).unwrap_err();
            assert!(err.contains("no encrypted identity to rotate"), "got: {err}");
        }
    }
}
