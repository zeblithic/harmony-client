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

use harmony_crypto::password_envelope::{self, Argon2idParams};
use harmony_identity::{PqPrivateIdentity, PrivateIdentity};
use serde::{Deserialize, Serialize};
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
const KDF_M_KIB: u32 = 65536; // 64 MiB
const KDF_T: u16 = 3; // iterations
const KDF_P: u8 = 1; // parallelism

// Wire format offsets:
const HEADER_LEN: usize = 13; // magic(4) + version(1) + kdf_id(1) + m(4) + t(2) + p(1)
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24; // XChaCha20 needs 192-bit nonce
const TAG_LEN: usize = 16; // Poly1305 tag
const ENC_FILE_LEN: usize = HEADER_LEN + SALT_LEN + NONCE_LEN + BLOB_LEN + TAG_LEN;

/// Destination chosen by `save_with_fallback`. Returned so force-cleanup
/// callers can know which backend actually received the write rather than
/// inferring from the pre-save probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SaveDestination {
    Keychain,
    EncryptedFile,
}

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

/// Serialize a 32-byte seed into the legacy raw-32 on-disk format.
///
/// ZEB-363: production now persists a CBOR [`SecretVault`], not a bare seed, so
/// this is retained only for tests that construct a legacy raw-32 item/file to
/// exercise the legacy-detection + migration paths.
#[cfg(test)]
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
pub(crate) fn write_atomic_0600(path: &Path, bytes: &[u8]) -> Result<(), String> {
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
        (&f).write_all(bytes)
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
        return Err(
            "verify-after-write returned a different seed than was just written".to_string(),
        );
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

    let params = Argon2idParams::new(KDF_M_KIB, KDF_T as u32, KDF_P as u32)
        .expect("Argon2 params hardcoded valid");
    let ciphertext_with_tag =
        password_envelope::seal(passphrase, &params, salt, nonce, &out[..HEADER_LEN], blob)
            .expect("seal cannot fail with valid inputs");
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
    let params = Argon2idParams::new(m_kib, t, p).map_err(|_| {
        "identity store could not be decrypted: wrong passphrase or corrupted file".to_string()
    })?;
    let plaintext = password_envelope::open(
        passphrase,
        &params,
        salt,
        nonce,
        &bytes[..HEADER_LEN],
        ciphertext_with_tag,
    )
    .map_err(|_| {
        "identity store could not be decrypted: wrong passphrase or corrupted file".to_string()
    })?;

    // Validate length, then copy directly into a Zeroizing-protected buffer
    // (no intermediate unprotected stack array). The borrowed slice points
    // into `plaintext`'s heap buffer, which is itself in Zeroizing<Vec<u8>>.
    let plaintext_slice: &[u8; BLOB_LEN] = plaintext.as_slice().try_into().map_err(|_| {
        format!(
            "decrypted plaintext was {} bytes, expected {}",
            plaintext.len(),
            BLOB_LEN
        )
    })?;
    let mut blob_arr: Zeroizing<[u8; BLOB_LEN]> = Zeroizing::new([0u8; BLOB_LEN]);
    blob_arr.copy_from_slice(plaintext_slice);
    Ok(blob_arr)
}

// ── SecretVault ─────────────────────────────────────────────────────────

/// Version tag for the CBOR `SecretVault` payload. Distinct from the `HRMI`
/// encrypted-file envelope version ([`ENC_FORMAT_VERSION`]): this versions the
/// *plaintext* secret structure; that versions the *file* framing.
const VAULT_VERSION: u8 = 1;

/// All process-local secrets, stored as ONE keychain item (and, in the headless
/// fallback, one `HRMI` encrypted file). Zeroized on drop.
///
/// ZEB-363: collapses the previously-separate `harmony.client`/`iroh.secret_key`
/// and `harmony.owner`/`device_signing_key` keychain items into the seed's
/// `harmony`/`identity` item, so macOS prompts for keychain access once during
/// setup instead of three times. The `seed` is the recovery root (mnemonic /
/// recovery exports encode only it); `iroh_secret_key` and `device_signing_key`
/// are app-local, regenerable, and `None` until first use.
///
/// No `Debug` (would print key material). `PartialEq` is test-only — production
/// comparisons (migration read-back verification) compare fields explicitly.
#[derive(Serialize, Deserialize, zeroize::ZeroizeOnDrop)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(crate) struct SecretVault {
    /// Structure version — see [`VAULT_VERSION`].
    version: u8,
    /// Node/Reticulum identity master seed (recovery root); sub-keys derive from
    /// this. Distinct from `owner_master_seed`.
    seed: [u8; BLOB_LEN],
    /// iroh transport secret key (independent-random; `None` until first node start).
    #[serde(default)]
    iroh_secret_key: Option<[u8; 32]>,
    /// Device #2 signing key (`None` until owner-state init).
    #[serde(default)]
    device_signing_key: Option<[u8; 32]>,
    /// Owner identity master seed (drives backup eligibility; `None` in the
    /// cert-only joiner model). Distinct from `seed` (the node seed).
    #[serde(default)]
    owner_master_seed: Option<[u8; 32]>,
    /// Distributed fleet KeyTree material (CBOR of `FleetKeyMaterial`) for a
    /// cert-only enrolled device (ZEB-492). `None` on the minting device (it
    /// re-derives from `owner_master_seed`) and on devices paired before ZEB-492.
    /// Variable-length, so NOT a 32-byte `VaultSlot`.
    #[serde(default)]
    fleet_keytree: Option<Vec<u8>>,
}

impl SecretVault {
    /// A fresh vault holding only the seed (no app-local keys yet).
    fn from_seed(seed: [u8; BLOB_LEN]) -> Self {
        Self {
            version: VAULT_VERSION,
            seed,
            iroh_secret_key: None,
            device_signing_key: None,
            owner_master_seed: None,
            fleet_keytree: None,
        }
    }

    /// The app-local key for `slot`, if present. Returns a borrow so callers copy
    /// straight into a `Zeroizing` buffer without materializing an intermediate
    /// non-zeroized `[u8; 32]` on the stack (keeps the "zeroized throughout"
    /// discipline).
    fn slot_key(&self, slot: VaultSlot) -> Option<&[u8; 32]> {
        match slot {
            VaultSlot::Iroh => self.iroh_secret_key.as_ref(),
            VaultSlot::Device => self.device_signing_key.as_ref(),
            VaultSlot::OwnerMasterSeed => self.owner_master_seed.as_ref(),
        }
    }

    /// Set (or clear) the app-local key for `slot`.
    fn set_slot_key(&mut self, slot: VaultSlot, key: Option<[u8; 32]>) {
        match slot {
            VaultSlot::Iroh => self.iroh_secret_key = key,
            VaultSlot::Device => self.device_signing_key = key,
            VaultSlot::OwnerMasterSeed => self.owner_master_seed = key,
        }
    }

    /// The distributed fleet KeyTree material (CBOR of `FleetKeyMaterial`),
    /// if present. Variable-length, so it is NOT a 32-byte `VaultSlot` — see
    /// the field docs (ZEB-492).
    fn fleet_keytree(&self) -> Option<&[u8]> {
        self.fleet_keytree.as_deref()
    }

    /// Set (or clear) the distributed fleet KeyTree material.
    fn set_fleet_keytree(&mut self, material: Option<Vec<u8>>) {
        self.fleet_keytree = material;
    }

    /// Serialize to CBOR. The returned buffer holds secret material and is wrapped
    /// in `Zeroizing` so it is wiped on drop.
    fn to_cbor(&self) -> Result<Zeroizing<Vec<u8>>, String> {
        let mut buf = Zeroizing::new(Vec::new());
        ciborium::into_writer(self, &mut *buf).map_err(|e| format!("vault CBOR encode: {e}"))?;
        Ok(buf)
    }

    /// Parse from CBOR. Rejects an unknown `version` so a future on-disk format is
    /// never silently misread as the current one.
    fn from_cbor(bytes: &[u8]) -> Result<Self, String> {
        let vault: SecretVault =
            ciborium::from_reader(bytes).map_err(|e| format!("vault CBOR decode: {e}"))?;
        if vault.version != VAULT_VERSION {
            return Err(format!(
                "unsupported secret-vault version {} (this build supports {VAULT_VERSION})",
                vault.version
            ));
        }
        Ok(vault)
    }
}

#[cfg(test)]
mod vault_tests {
    use super::*;

    #[test]
    fn vault_cbor_round_trips() {
        let v = SecretVault {
            version: VAULT_VERSION,
            seed: [7u8; BLOB_LEN],
            iroh_secret_key: Some([9u8; 32]),
            device_signing_key: None,
            owner_master_seed: Some([8u8; 32]),
            fleet_keytree: None,
        };
        let cbor = v.to_cbor().expect("encode");
        let back = SecretVault::from_cbor(&cbor).expect("decode");
        assert!(v == back, "vault must round-trip through CBOR unchanged");
    }

    #[test]
    fn vault_carries_fleet_keytree() {
        let mut v = SecretVault::from_seed([7u8; BLOB_LEN]);
        assert!(v.fleet_keytree().is_none());
        v.set_fleet_keytree(Some(vec![1, 2, 3, 4, 5]));
        let cbor = v.to_cbor().expect("encode");
        let back = SecretVault::from_cbor(&cbor).expect("decode");
        assert_eq!(back.fleet_keytree(), Some(&[1, 2, 3, 4, 5][..]));
    }

    #[test]
    fn vault_without_fleet_keytree_decodes_to_none() {
        // Back-compat: a vault written by a pre-ZEB-492 build OMITS the
        // `fleet_keytree` key entirely (the field didn't exist), and must decode
        // via `#[serde(default)]` to `fleet_keytree == None`. Serializing a
        // CURRENT `SecretVault` would always emit the key (as `null`), so it would
        // pass even if `#[serde(default)]` were removed — useless as a regression
        // guard. Encode a legacy struct shape mirroring the pre-field layout so
        // the produced CBOR genuinely lacks the `fleet_keytree` key.
        #[derive(Serialize)]
        struct LegacySecretVault {
            version: u8,
            seed: [u8; BLOB_LEN],
            iroh_secret_key: Option<[u8; 32]>,
            device_signing_key: Option<[u8; 32]>,
            owner_master_seed: Option<[u8; 32]>,
            // NOTE: no `fleet_keytree` field — this mirrors the on-disk layout
            // before ZEB-492 added it.
        }

        let legacy = LegacySecretVault {
            version: VAULT_VERSION,
            seed: [1u8; BLOB_LEN],
            iroh_secret_key: Some([2u8; 32]),
            device_signing_key: Some([3u8; 32]),
            owner_master_seed: Some([4u8; 32]),
        };
        let mut cbor = Vec::new();
        ciborium::into_writer(&legacy, &mut cbor).expect("encode legacy vault");

        let back = SecretVault::from_cbor(&cbor).expect("legacy vault decodes via serde default");
        assert!(
            back.fleet_keytree().is_none(),
            "an omitted fleet_keytree key must default to None (serde-default back-compat)"
        );
        // Sanity: the pre-existing fields still decode (the legacy blob is otherwise intact).
        assert_eq!(back.seed, [1u8; BLOB_LEN]);
        assert_eq!(back.owner_master_seed, Some([4u8; 32]));
    }

    /// ZEB-492 (Qodo/CodeAnt round 1, FIX A): `decrypt_vault_bytes` is reachable
    /// directly from the owner-state fleet-keytree file fallback, so a
    /// truncated/garbage `fleet_keytree.enc` routed through it must return `Err`
    /// — NOT panic out-of-bounds (which `decrypt_v2_plaintext`'s unchecked
    /// `bytes[5]`/header indexing would do). 20 bytes is below `MIN_LEN`.
    #[test]
    fn decrypt_vault_bytes_short_input_errors_not_panics() {
        let err = decrypt_vault_bytes(b"some-pass", &[0u8; 20])
            .expect_err("a 20-byte buffer is below MIN_LEN and must error");
        assert!(
            err.contains("below the") && err.contains("minimum"),
            "short input must surface the MIN_LEN guard, got: {err}"
        );
    }

    /// Companion to the short-input guard: a buffer that clears `MIN_LEN` but
    /// carries the wrong magic must also error (not reach the v2 decryptor with
    /// a non-v0x02 layout).
    #[test]
    fn decrypt_vault_bytes_bad_magic_errors() {
        let err =
            decrypt_vault_bytes(b"some-pass", &[0u8; 128]).expect_err("wrong magic must error");
        assert!(
            err.contains("unrecognized format"),
            "bad magic must surface the format guard, got: {err}"
        );
    }

    /// A valid `encrypt_vault_bytes` envelope still round-trips through the
    /// guarded `decrypt_vault_bytes` (the new pre-checks don't reject good
    /// input).
    #[test]
    fn decrypt_vault_bytes_round_trips_valid_envelope() {
        let pass = b"round-trip-pass";
        let plaintext = vec![0xABu8; 161];
        let blob = encrypt_vault_bytes(pass, &plaintext);
        let back = decrypt_vault_bytes(pass, &blob).expect("decrypt valid envelope");
        assert_eq!(back.as_slice(), plaintext.as_slice());
    }

    #[test]
    fn seed_only_vault_exceeds_legacy_32_bytes() {
        // Legacy-item detection relies on: a raw seed is exactly 32 bytes, while
        // any CBOR vault is strictly longer. Guard that invariant.
        let v = SecretVault::from_seed([0u8; BLOB_LEN]);
        let cbor = v.to_cbor().expect("encode");
        assert!(
            cbor.len() > BLOB_LEN,
            "seed-only vault CBOR is {} bytes; must exceed the {BLOB_LEN}-byte legacy seed",
            cbor.len()
        );
    }

    #[test]
    fn from_cbor_rejects_unknown_version() {
        let mut v = SecretVault::from_seed([1u8; BLOB_LEN]);
        v.version = 99;
        let cbor = v.to_cbor().expect("encode");
        assert!(
            SecretVault::from_cbor(&cbor).is_err(),
            "an unknown vault version must be rejected, not silently accepted"
        );
    }

    #[test]
    fn from_seed_has_no_app_keys() {
        let v = SecretVault::from_seed([3u8; BLOB_LEN]);
        assert_eq!(v.version, VAULT_VERSION);
        assert!(v.iroh_secret_key.is_none());
        assert!(v.device_signing_key.is_none());
    }

    #[test]
    fn keychain_legacy_32_item_reads_as_seed_only_vault() {
        // Pre-ZEB-363 installs stored exactly 32 raw seed bytes. The vault loader
        // must read that as a seed-only vault so the seed still loads.
        let kc = KeychainStore::new_mock();
        let seed = [0x11u8; BLOB_LEN];
        kc.entry
            .set_secret(&seed)
            .expect("write legacy raw-32 item");
        let vault = kc.load_vault().expect("load").expect("present");
        assert_eq!(vault.seed, seed, "legacy seed must survive");
        assert!(vault.iroh_secret_key.is_none());
        assert!(vault.device_signing_key.is_none());
    }

    #[test]
    fn keychain_vault_round_trips_with_keys() {
        let kc = KeychainStore::new_mock();
        let vault = SecretVault {
            version: VAULT_VERSION,
            seed: [4u8; BLOB_LEN],
            iroh_secret_key: Some([5u8; 32]),
            device_signing_key: Some([6u8; 32]),
            owner_master_seed: Some([8u8; 32]),
            fleet_keytree: None,
        };
        kc.save_vault(&vault).expect("save");
        let back = kc.load_vault().expect("load").expect("present");
        assert!(
            back == vault,
            "vault must round-trip through the keychain item"
        );
    }

    #[test]
    fn keychain_corrupt_item_is_hard_error() {
        // 40 bytes: not the 32-byte legacy seed, not valid vault CBOR.
        let kc = KeychainStore::new_mock();
        kc.entry.set_secret(&[0xFFu8; 40]).expect("write garbage");
        assert!(
            kc.load_vault().is_err(),
            "a non-seed, non-CBOR item must hard-error, never be silently overwritten"
        );
    }

    #[test]
    fn seed_save_on_corrupt_keychain_item_hard_fails() {
        // The seed-level `save` default must NOT silently overwrite a corrupt
        // (non-seed, non-CBOR) keychain item — doing so would discard app-local
        // keys and break the "hard error, never overwritten" contract.
        let kc = KeychainStore::new_mock();
        let corrupt = [0xFFu8; 40];
        kc.entry.set_secret(&corrupt).expect("write garbage");
        let err = kc
            .save(&[3u8; BLOB_LEN])
            .expect_err("save must hard-fail on a corrupt vault item, not overwrite it");
        assert!(
            err.contains("vault") || err.contains("decode") || err.contains("version"),
            "expected a vault read/decode error, got: {err}"
        );
        let raw = kc.entry.get_secret().expect("item still present");
        assert_eq!(
            raw, corrupt,
            "a corrupt item must not be overwritten by a failed seed save"
        );
    }

    // ── ZEB-429: pre-ZEB-363 relic quarantine ───────────────────────────

    #[test]
    fn is_pre363_relic_matches_integer_tag_only() {
        // The relic: integer-version-tagged CBOR (major type 0), like the
        // 161-byte item observed on Ildwyn (`invalid type: integer 1, expected map`).
        let mut relic = vec![0x01u8];
        relic.resize(161, 0x00);
        assert!(is_pre363_relic(&relic), "integer-tagged payload is a relic");
        // A real SecretVault serializes as a CBOR map (major type 5) → NOT a relic.
        let vault_cbor = SecretVault::from_seed([7u8; BLOB_LEN])
            .to_cbor()
            .expect("encode");
        assert!(
            !is_pre363_relic(&vault_cbor),
            "a real vault (CBOR map) is never a relic"
        );
        // A bare 32-byte legacy seed item is valid → NOT a relic.
        assert!(!is_pre363_relic(&[0x00u8; BLOB_LEN]));
        // Other corrupt shapes (major type 7 here) keep the pre-existing
        // "leave intact / hard-fail" contract → NOT a relic.
        assert!(!is_pre363_relic(&[0xFFu8; 40]));
        // Empty item → nothing to quarantine.
        assert!(!is_pre363_relic(&[]));

        // A well-formed multi-byte uint header (uint8, `0x18` + 1 payload byte)
        // is still a top-level unsigned integer → a relic.
        assert!(
            is_pre363_relic(&[0x18u8, 0xAB]),
            "complete uint8-tagged payload is a relic"
        );
        // Truncated multi-byte header (`0x18` promising a payload byte that is
        // absent) is malformed CBOR → NOT a relic; must hard-fail.
        assert!(
            !is_pre363_relic(&[0x18u8]),
            "truncated uint8 header is corruption, not a relic"
        );
        // Reserved additional-info (`0x1c`) and the illegal indefinite-length
        // form (`0x1f`) are not well-formed integers → NOT relics.
        assert!(!is_pre363_relic(&[0x1cu8, 0xAB, 0xCD]));
        assert!(!is_pre363_relic(&[0x1fu8, 0xAB, 0xCD]));
    }

    #[test]
    fn quarantine_relic_between_preserves_then_evicts() {
        let primary = mock_entry();
        let backup = mock_entry();
        let relic = vec![0x01u8, 2, 3, 4, 5];
        primary.set_secret(&relic).expect("seed relic");
        quarantine_relic_between(&primary, &backup, &relic).expect("quarantine");
        assert!(
            matches!(primary.get_secret(), Err(keyring::Error::NoEntry)),
            "primary address must be freed"
        );
        assert_eq!(
            backup.get_secret().expect("backup present"),
            relic,
            "relic bytes must be preserved verbatim in the backup"
        );
    }

    #[test]
    fn quarantine_relic_between_is_idempotent_on_identical_backup() {
        let primary = mock_entry();
        let backup = mock_entry();
        let relic = vec![0x01u8, 9, 9, 9];
        primary.set_secret(&relic).expect("seed relic");
        backup
            .set_secret(&relic)
            .expect("pre-existing identical backup");
        quarantine_relic_between(&primary, &backup, &relic).expect("idempotent quarantine");
        assert!(
            matches!(primary.get_secret(), Err(keyring::Error::NoEntry)),
            "primary still evicted when backup already matches"
        );
        assert_eq!(backup.get_secret().expect("backup present"), relic);
    }

    #[test]
    fn quarantine_relic_between_refuses_to_clobber_different_backup() {
        let primary = mock_entry();
        let backup = mock_entry();
        let relic = vec![0x01u8, 1, 1, 1];
        let other = vec![0x01u8, 2, 2, 2];
        primary.set_secret(&relic).expect("seed relic");
        backup
            .set_secret(&other)
            .expect("pre-existing DIFFERENT backup");
        let err = quarantine_relic_between(&primary, &backup, &relic)
            .expect_err("must refuse to clobber a different backup");
        assert!(err.contains("clobber"), "got: {err}");
        // Nothing is lost: both the relic and the pre-existing backup stay intact.
        assert_eq!(primary.get_secret().expect("relic intact"), relic);
        assert_eq!(backup.get_secret().expect("backup intact"), other);
    }

    #[test]
    fn keychain_pre363_relic_is_quarantined_and_unblocks_vault() {
        let kc = KeychainStore::new_mock();
        // A pre-ZEB-363 relic (integer-tagged, 161 bytes) sitting at harmony/identity.
        let mut relic = vec![0x01u8];
        relic.resize(161, 0xAB);
        kc.entry.set_secret(&relic).expect("seed relic");

        // load_vault must quarantine (not propagate the decode error) → reports absent.
        assert!(
            kc.load_vault()
                .expect("relic must be quarantined, not error")
                .is_none(),
            "a quarantined relic reads as an empty vault address"
        );
        // Primary freed; relic preserved verbatim in the backup account.
        assert!(
            matches!(kc.entry.get_secret(), Err(keyring::Error::NoEntry)),
            "the relic must be removed from the primary vault address"
        );
        assert_eq!(
            kc.backup.get_secret().expect("relic preserved in backup"),
            relic,
            "the relic bytes must be preserved verbatim (never lost)"
        );
        // Migration unblocked: a subsequent seed save now creates a clean vault.
        kc.save(&[9u8; BLOB_LEN])
            .expect("save must now create a fresh vault at the freed address");
        assert_eq!(
            kc.load_vault().expect("load").expect("present").seed,
            [9u8; BLOB_LEN],
            "a fresh vault materializes at the freed address"
        );
    }

    #[test]
    fn reconcile_after_enc_restore_preserves_app_local_keys() {
        // An encrypted-file force-restore reconciles a stale keychain vault to
        // the restored seed WITHOUT dropping app-local keys (pre-ZEB-363 those
        // lived in separate items untouched by a seed write).
        let kc = KeychainStore::new_mock();
        let mut vault = SecretVault::from_seed([1u8; BLOB_LEN]);
        vault.set_slot_key(VaultSlot::Iroh, Some([9u8; 32]));
        vault.set_slot_key(VaultSlot::Device, Some([8u8; 32]));
        kc.save_vault(&vault).expect("seed initial vault");

        let restored_seed = [2u8; BLOB_LEN];
        reconcile_keychain_after_enc_restore(&kc, &restored_seed);

        let back = kc.load_vault().expect("load").expect("present");
        assert_eq!(
            back.seed, restored_seed,
            "seed must be reconciled to the restored value"
        );
        assert_eq!(
            back.slot_key(VaultSlot::Iroh),
            Some(&[9u8; 32]),
            "iroh key must be preserved across reconcile"
        );
        assert_eq!(
            back.slot_key(VaultSlot::Device),
            Some(&[8u8; 32]),
            "device key must be preserved across reconcile"
        );
    }

    #[test]
    fn enc_file_v1_seed_reads_as_seed_only_vault() {
        // A legacy v0x01 (fixed 32-byte seed) envelope decodes to a seed-only vault.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.enc");
        let salt = [1u8; SALT_LEN];
        let nonce = [2u8; NONCE_LEN];
        let seed = [9u8; BLOB_LEN];
        let v1 = encrypt_with_params(b"vault-v1-test", &salt, &nonce, &seed);
        std::fs::write(&path, &v1).unwrap();
        let store = EncryptedFileStore::new(
            path,
            secrecy::SecretString::from("vault-v1-test".to_string()),
        );
        let vault = store.load_vault().expect("load").expect("present");
        assert_eq!(
            vault.seed, seed,
            "v1 seed must decode into a seed-only vault"
        );
        assert!(vault.iroh_secret_key.is_none());
        assert!(vault.device_signing_key.is_none());
    }

    #[test]
    fn enc_file_v2_round_trips_vault_with_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.enc");
        let store = EncryptedFileStore::new(
            path,
            secrecy::SecretString::from("vault-v2-test".to_string()),
        );
        let vault = SecretVault {
            version: VAULT_VERSION,
            seed: [1u8; BLOB_LEN],
            iroh_secret_key: Some([2u8; 32]),
            device_signing_key: Some([3u8; 32]),
            owner_master_seed: Some([4u8; 32]),
            fleet_keytree: None,
        };
        store.save_vault(&vault).expect("save");
        let back = store.load_vault().expect("load").expect("present");
        assert!(
            back == vault,
            "v2 vault must round-trip through the encrypted file"
        );
    }

    fn mock_entry() -> keyring::Entry {
        keyring::Entry::new_with_credential(Box::new(keyring::mock::MockCredential::default()))
    }

    #[test]
    fn accessor_returns_existing_vault_key_without_touching_legacy() {
        let store = KeychainStore::new_mock();
        let mut vault = SecretVault::from_seed([1u8; BLOB_LEN]);
        vault.iroh_secret_key = Some([7u8; 32]);
        store.save_vault(&vault).unwrap();
        let legacy = mock_entry();

        let (key, fresh) =
            vault_app_key_or_create_with_store(&store, VaultSlot::Iroh, &legacy).unwrap();
        assert_eq!(*key, [7u8; 32]);
        assert!(!fresh, "an existing vault key is not freshly created");
        assert!(
            matches!(legacy.get_secret(), Err(keyring::Error::NoEntry)),
            "legacy item must not be created when the vault already has the key"
        );
    }

    #[test]
    fn corrupt_vault_degrades_app_key_to_legacy_item() {
        // A corrupt/unreadable harmony/identity vault must NOT take iroh transport
        // down: the accessor degrades to the legacy per-item key and leaves the
        // corrupt vault intact (never overwritten). (Cursor High.)
        let store = KeychainStore::new_mock();
        let corrupt = [0xFFu8; 40];
        store
            .entry
            .set_secret(&corrupt)
            .expect("write corrupt vault");
        let legacy = mock_entry();
        legacy.set_secret(&[7u8; 32]).expect("seed legacy iroh key");

        let (key, fresh) = vault_app_key_or_create_with_store(&store, VaultSlot::Iroh, &legacy)
            .expect("must degrade to the legacy key, not hard-fail");
        assert_eq!(
            *key, [7u8; 32],
            "legacy app-local key returned on corrupt vault"
        );
        assert!(!fresh, "an existing legacy key is not freshly created");

        let raw = store
            .entry
            .get_secret()
            .expect("corrupt vault still present");
        assert_eq!(
            raw, corrupt,
            "the corrupt vault must not be overwritten by the degrade path"
        );
        assert_eq!(
            legacy.get_secret().expect("legacy retained"),
            vec![7u8; 32],
            "the legacy item is retained when the vault is unreadable"
        );
    }

    #[test]
    fn corrupt_vault_degrades_load_slot_to_legacy_item() {
        // The owner slot read also degrades to the legacy item on a corrupt vault
        // rather than skipping the secret. (Cursor High.)
        let store = KeychainStore::new_mock();
        store
            .entry
            .set_secret(&[0xFFu8; 40])
            .expect("write corrupt vault");
        let legacy = mock_entry();
        legacy
            .set_secret(&[5u8; 32])
            .expect("seed legacy device key");

        let got = vault_load_slot_with_store(&store, VaultSlot::Device, &legacy)
            .expect("must degrade to the legacy slot, not hard-fail");
        assert_eq!(
            got.map(|z| *z),
            Some([5u8; 32]),
            "legacy slot value read on corrupt vault"
        );
    }

    #[test]
    fn accessor_migrates_legacy_item_then_deletes_it() {
        let store = KeychainStore::new_mock();
        store
            .save_vault(&SecretVault::from_seed([1u8; BLOB_LEN]))
            .unwrap();
        let legacy = mock_entry();
        legacy.set_secret(&[9u8; 32]).unwrap();

        let (key, fresh) =
            vault_app_key_or_create_with_store(&store, VaultSlot::Device, &legacy).unwrap();
        assert_eq!(*key, [9u8; 32], "migrated key value preserved");
        assert!(
            !fresh,
            "a migrated key is the same identity, not freshly created"
        );

        let v = store.load_vault().unwrap().unwrap();
        assert_eq!(v.device_signing_key, Some([9u8; 32]), "folded into vault");
        assert_eq!(v.seed, [1u8; BLOB_LEN], "seed preserved through the fold");
        assert!(
            matches!(legacy.get_secret(), Err(keyring::Error::NoEntry)),
            "legacy item deleted after verified read-back"
        );
    }

    #[test]
    fn accessor_generates_when_neither_present_and_is_idempotent() {
        let store = KeychainStore::new_mock();
        store
            .save_vault(&SecretVault::from_seed([1u8; BLOB_LEN]))
            .unwrap();
        let legacy = mock_entry();

        let (key, fresh) =
            vault_app_key_or_create_with_store(&store, VaultSlot::Iroh, &legacy).unwrap();
        assert!(fresh, "no vault key + no legacy item => freshly created");
        let v = store.load_vault().unwrap().unwrap();
        assert_eq!(
            v.iroh_secret_key,
            Some(*key),
            "generated key folded into vault"
        );

        // Second call returns the same key and is no longer "fresh".
        let (key2, fresh2) =
            vault_app_key_or_create_with_store(&store, VaultSlot::Iroh, &legacy).unwrap();
        assert_eq!(*key2, *key);
        assert!(!fresh2);
    }

    // ── ZEB-449: encrypted-file fallback for app-local (iroh) keys ──────────
    //
    // The fallback is supplied as a lazy factory closure: it must run ONLY when
    // the keychain is unavailable/unusable, so these tests exercise the closure
    // directly (no env mutation, no real keychain — per the ZEB-428 rules).

    #[test]
    fn app_key_file_fallback_generates_persists_and_reloads_without_keychain() {
        // ZEB-449: with no keychain available, the app-local (iroh) key must
        // persist to an encrypted file and reload identically. Previously this
        // path was keychain-only, so a keychain-less / kill-switched node could
        // not obtain an iroh key and booted with no transport.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("iroh_sk.enc");
        let legacy = mock_entry();

        let p1 = path.clone();
        let (k1, fresh1) = app_key_or_create_with_stores(None, VaultSlot::Iroh, &legacy, || {
            Ok(Some(EncryptedFileStore::new(
                p1,
                secrecy::SecretString::from("zeb449-pp".to_string()),
            )))
        })
        .unwrap();
        assert!(
            fresh1,
            "first call with an empty file generates a fresh key"
        );
        assert!(path.exists(), "fresh key persisted to the encrypted file");

        // A second call over the same file must read the SAME key, not regenerate.
        let p2 = path.clone();
        let (k2, fresh2) = app_key_or_create_with_stores(None, VaultSlot::Iroh, &legacy, || {
            Ok(Some(EncryptedFileStore::new(
                p2,
                secrecy::SecretString::from("zeb449-pp".to_string()),
            )))
        })
        .unwrap();
        assert!(!fresh2, "an existing file key is not freshly created");
        assert_eq!(*k1, *k2, "reloaded key matches the persisted one");
    }

    #[test]
    fn app_key_no_keychain_no_passphrase_fails_loudly() {
        // No keychain AND no encrypted-file fallback configured must produce a
        // clear, actionable error rather than a silent transport-disable.
        // (ZEB-449 / ZEB-450.)
        let legacy = mock_entry();
        let err =
            app_key_or_create_with_stores(None, VaultSlot::Iroh, &legacy, || Ok(None)).unwrap_err();
        assert!(
            err.contains("HARMONY_PASSPHRASE"),
            "error names the remediation env var: {err}"
        );
        assert!(
            err.contains("headless-install"),
            "error points to the headless docs: {err}"
        );
    }

    #[test]
    fn app_key_prefers_keychain_and_skips_fallback_factory() {
        // When the keychain is healthy the key comes from the vault and the
        // fallback factory is NEVER invoked — so a malformed passphrase or a
        // missing HOME cannot break a working keychain. The panicking factory
        // proves the laziness.
        let store = KeychainStore::new_mock();
        store
            .save_vault(&SecretVault::from_seed([1u8; BLOB_LEN]))
            .unwrap();
        let legacy = mock_entry();

        let (key, fresh) =
            app_key_or_create_with_stores(Some(&store), VaultSlot::Iroh, &legacy, || {
                panic!("fallback factory must not run when the keychain is healthy")
            })
            .unwrap();
        assert!(fresh, "fresh generate folded into the vault");
        assert_eq!(
            store.load_vault().unwrap().unwrap().iroh_secret_key,
            Some(*key),
            "key folded into the keychain vault"
        );
    }

    #[test]
    fn app_key_runtime_failing_keychain_falls_through_to_file() {
        // A keychain backend present but unusable at runtime (locked / no Secret
        // Service) must fall THROUGH to the encrypted file rather than
        // hard-failing — this is what gives keychain-installed-but-broken hosts
        // (and headless Linux) a working transport key. Mirrors
        // owner_state::load_secret. (CodeRabbit.)
        let store = KeychainStore::new_load_failing_mock();
        // A legacy entry that also errors on every op, so the vault accessor's
        // legacy-degrade path fails too and the whole keychain attempt errors.
        let failing_legacy = keyring::Entry::new_with_credential(Box::new(AlwaysFailOnLoad));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("iroh_sk.enc");

        let p = path.clone();
        let (key, fresh) =
            app_key_or_create_with_stores(Some(&store), VaultSlot::Iroh, &failing_legacy, || {
                Ok(Some(EncryptedFileStore::new(
                    p,
                    secrecy::SecretString::from("pp".to_string()),
                )))
            })
            .unwrap();
        assert!(
            fresh,
            "unusable keychain → fell through and generated in the file"
        );
        assert!(
            path.exists(),
            "key persisted to the encrypted-file fallback"
        );

        let reload = EncryptedFileStore::new(path, secrecy::SecretString::from("pp".to_string()))
            .load()
            .unwrap()
            .unwrap();
        assert_eq!(
            *key, *reload,
            "the generated key is what landed in the file"
        );
    }

    #[test]
    fn keychain_consistency_errors_are_terminal_not_fallthrough() {
        // Post-write/consistency failures must abort (not flap to the file),
        // while backend-availability failures are safe to fall through. Pins the
        // classifier the fallthrough branch keys off. (CodeRabbit.)
        assert!(is_keychain_consistency_error(
            "secret-vault read-back mismatch after fold; legacy item retained"
        ));
        assert!(is_keychain_consistency_error(
            "secret vault disappeared immediately after write"
        ));
        assert!(!is_keychain_consistency_error(
            "legacy keychain item read failed: platform error"
        ));
        assert!(!is_keychain_consistency_error(
            "OS keychain disabled via HARMONY_DISABLE_KEYCHAIN (ZEB-428)"
        ));
    }

    #[test]
    fn accessor_without_vault_item_falls_back_to_legacy() {
        // Empty mock store: load_vault() -> None (headless / no-keychain seed).
        let store = KeychainStore::new_mock();
        let legacy = mock_entry();

        let (key, fresh) =
            vault_app_key_or_create_with_store(&store, VaultSlot::Iroh, &legacy).unwrap();
        assert!(fresh, "fresh generate persisted to the legacy item");
        assert_eq!(
            legacy.get_secret().unwrap(),
            (*key).to_vec(),
            "stored in legacy"
        );
        assert!(
            store.load_vault().unwrap().is_none(),
            "no vault item is created in the fallback path"
        );

        // Second call reads the existing legacy key (not fresh).
        let (key2, fresh2) =
            vault_app_key_or_create_with_store(&store, VaultSlot::Iroh, &legacy).unwrap();
        assert!(!fresh2);
        assert_eq!(*key2, *key);
    }

    #[test]
    fn load_slot_returns_vault_value_and_migrates_legacy() {
        // (a) value already in the vault.
        let store = KeychainStore::new_mock();
        let mut v = SecretVault::from_seed([1u8; BLOB_LEN]);
        v.owner_master_seed = Some([2u8; 32]);
        store.save_vault(&v).unwrap();
        let legacy = mock_entry();
        let got = vault_load_slot_with_store(&store, VaultSlot::OwnerMasterSeed, &legacy).unwrap();
        assert_eq!(*got.unwrap(), [2u8; 32]);

        // (b) value only in the legacy item -> migrated + legacy deleted.
        let store2 = KeychainStore::new_mock();
        store2
            .save_vault(&SecretVault::from_seed([1u8; BLOB_LEN]))
            .unwrap();
        let legacy2 = mock_entry();
        legacy2.set_secret(&[3u8; 32]).unwrap();
        let got2 = vault_load_slot_with_store(&store2, VaultSlot::Device, &legacy2).unwrap();
        assert_eq!(*got2.unwrap(), [3u8; 32]);
        assert_eq!(
            store2.load_vault().unwrap().unwrap().device_signing_key,
            Some([3u8; 32])
        );
        assert!(matches!(legacy2.get_secret(), Err(keyring::Error::NoEntry)));
    }

    #[test]
    fn load_slot_is_none_when_absent_everywhere_and_never_generates() {
        let store = KeychainStore::new_mock();
        store
            .save_vault(&SecretVault::from_seed([1u8; BLOB_LEN]))
            .unwrap();
        let legacy = mock_entry();
        let got = vault_load_slot_with_store(&store, VaultSlot::Device, &legacy).unwrap();
        assert!(got.is_none(), "pure read must not generate a key");
    }

    #[test]
    fn save_slot_writes_to_vault_or_reports_no_item() {
        let store = KeychainStore::new_mock();
        store
            .save_vault(&SecretVault::from_seed([1u8; BLOB_LEN]))
            .unwrap();
        assert!(
            vault_save_slot_with_store(&store, VaultSlot::OwnerMasterSeed, &[7u8; 32]).unwrap()
        );
        assert_eq!(
            store.load_vault().unwrap().unwrap().owner_master_seed,
            Some([7u8; 32])
        );

        // No vault item -> Ok(false) so the caller falls back to its own store.
        let empty = KeychainStore::new_mock();
        assert!(
            !vault_save_slot_with_store(&empty, VaultSlot::OwnerMasterSeed, &[7u8; 32]).unwrap()
        );
    }

    #[test]
    fn fleet_keytree_vault_round_trips_via_store() {
        // Variable-length sibling of the 32-byte slot accessors: save -> Ok(true),
        // load -> Some(material), clear -> load returns None.
        let store = KeychainStore::new_mock();
        store
            .save_vault(&SecretVault::from_seed([1u8; BLOB_LEN]))
            .unwrap();

        // ~161-byte material (CBOR of FleetKeyMaterial is variable-length).
        let material = vec![0xCDu8; 161];
        assert!(vault_save_fleet_keytree_with_store(&store, &material).unwrap());
        // Other fields are preserved (read-modify-write).
        assert_eq!(store.load_vault().unwrap().unwrap().seed, [1u8; BLOB_LEN]);

        let loaded = vault_load_fleet_keytree_with_store(&store).unwrap();
        assert_eq!(loaded.as_deref().map(Vec::as_slice), Some(&material[..]));

        vault_clear_fleet_keytree_with_store(&store).unwrap();
        assert!(vault_load_fleet_keytree_with_store(&store)
            .unwrap()
            .is_none());
        // Idempotent second clear.
        vault_clear_fleet_keytree_with_store(&store).unwrap();

        // No vault item -> Ok(false) so the caller falls back to its own store.
        let empty = KeychainStore::new_mock();
        assert!(!vault_save_fleet_keytree_with_store(&empty, &material).unwrap());
        assert!(vault_load_fleet_keytree_with_store(&empty)
            .unwrap()
            .is_none());
    }

    #[test]
    fn fleet_keytree_clear_propagates_unreadable_vault_err() {
        // An unreadable/locked vault must surface an Err from the clear, NOT a
        // silent Ok(()) — otherwise `install_joiner_state_inner`'s None branch
        // (which calls `clear_fleet_keytree(...)?` BEFORE committing owner_state)
        // could commit owner_state while stale fleet material survives in a
        // temporarily-locked keychain and later shadows fresh state. Mirrors the
        // corrupt-vault seam used by `corrupt_vault_degrades_load_slot_to_legacy_item`.
        let store = KeychainStore::new_mock();
        let corrupt = [0xFFu8; 40];
        store
            .entry
            .set_secret(&corrupt)
            .expect("write corrupt vault");

        vault_clear_fleet_keytree_with_store(&store)
            .expect_err("clear must propagate the unreadable-vault error, not swallow it");

        // The corrupt item is left intact (never overwritten by the clear path).
        assert_eq!(
            store
                .entry
                .get_secret()
                .expect("corrupt vault still present"),
            corrupt,
            "the corrupt vault must not be overwritten by the clear path"
        );
    }

    #[test]
    fn clear_slot_clears_vault_and_legacy_idempotently() {
        let store = KeychainStore::new_mock();
        let mut v = SecretVault::from_seed([1u8; BLOB_LEN]);
        v.owner_master_seed = Some([9u8; 32]);
        store.save_vault(&v).unwrap();
        let legacy = mock_entry();
        legacy.set_secret(&[9u8; 32]).unwrap();

        vault_clear_slot_with_store(&store, VaultSlot::OwnerMasterSeed, &legacy).unwrap();
        assert_eq!(
            store.load_vault().unwrap().unwrap().owner_master_seed,
            None,
            "vault slot cleared"
        );
        assert!(matches!(legacy.get_secret(), Err(keyring::Error::NoEntry)));
        // Idempotent second call.
        vault_clear_slot_with_store(&store, VaultSlot::OwnerMasterSeed, &legacy).unwrap();
    }

    #[test]
    fn seed_save_preserves_existing_app_local_keys() {
        // A node-seed write (restore / re-generate) must keep the device's
        // iroh / device / owner-master keys — same as the pre-consolidation
        // behaviour where they lived in separate, untouched items.
        let store = KeychainStore::new_mock();
        let mut v = SecretVault::from_seed([1u8; BLOB_LEN]);
        v.iroh_secret_key = Some([2u8; 32]);
        v.device_signing_key = Some([3u8; 32]);
        v.owner_master_seed = Some([4u8; 32]);
        store.save_vault(&v).unwrap();

        store.save(&[9u8; BLOB_LEN]).unwrap();

        let back = store.load_vault().unwrap().unwrap();
        assert_eq!(back.seed, [9u8; BLOB_LEN], "seed updated");
        assert_eq!(back.iroh_secret_key, Some([2u8; 32]), "iroh preserved");
        assert_eq!(back.device_signing_key, Some([3u8; 32]), "device preserved");
        assert_eq!(back.owner_master_seed, Some([4u8; 32]), "owner preserved");
    }

    #[test]
    fn seed_save_on_empty_creates_seed_only_vault() {
        let store = KeychainStore::new_mock();
        store.save(&[5u8; BLOB_LEN]).unwrap();
        let back = store.load_vault().unwrap().unwrap();
        assert_eq!(back.seed, [5u8; BLOB_LEN]);
        assert!(back.iroh_secret_key.is_none());
        assert!(back.device_signing_key.is_none());
        assert!(back.owner_master_seed.is_none());
    }
}

// ── Vault item / envelope codecs (ZEB-363) ─────────────────────────────

/// HRMI envelope version carrying a CBOR [`SecretVault`] plaintext (vs. the
/// `v0x01` fixed 32-byte seed plaintext).
const ENC_FORMAT_VERSION_V2: u8 = 0x02;

/// Interpret a keychain item's raw bytes as a [`SecretVault`].
///
/// A **legacy** item is exactly the 32 raw seed bytes (pre-ZEB-363); anything
/// else is the CBOR vault. A legacy item becomes a seed-only vault — its
/// iroh/device keys (if the install had them) still live in the old
/// `harmony.client` / `harmony.owner` items and are folded in by the per-key
/// accessors. A non-32-byte, non-CBOR value is a hard error (never overwritten).
fn item_bytes_to_vault(bytes: &[u8]) -> Result<SecretVault, String> {
    if bytes.len() == BLOB_LEN {
        let seed = blob_to_seed(bytes)?;
        return Ok(SecretVault::from_seed(*seed));
    }
    SecretVault::from_cbor(bytes)
}

/// Encrypt a CBOR `SecretVault` plaintext into the HRMI `v0x02` envelope.
///
/// Production-only: generates a fresh random salt and nonce per call (no
/// caller-supplied nonce, so there is no deterministic-nonce surface to misuse
/// in production). Framing matches [`encrypt_with_params`] (Argon2id
/// m=64MiB/t=3/p=1 → XChaCha20-Poly1305, 13-byte header bound as AAD) except the
/// version byte is `0x02` and the protected plaintext is variable-length.
fn encrypt_vault(passphrase: &[u8], plaintext: &[u8]) -> Vec<u8> {
    use rand::RngCore;
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    encrypt_vault_inner(passphrase, plaintext, &salt, &nonce)
}

/// Deterministic variant for byte-pinning fixtures. Gated so production
/// cannot link a caller-supplied-nonce path (nonce reuse on XChaCha20 is
/// catastrophic — mirrors `encode_snapshot_with_params`, Qodo C4).
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn encrypt_vault_with_params(
    passphrase: &[u8],
    plaintext: &[u8],
    salt: &[u8; SALT_LEN],
    nonce: &[u8; NONCE_LEN],
) -> Vec<u8> {
    encrypt_vault_inner(passphrase, plaintext, salt, nonce)
}

fn encrypt_vault_inner(
    passphrase: &[u8],
    plaintext: &[u8],
    salt: &[u8; SALT_LEN],
    nonce: &[u8; NONCE_LEN],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + SALT_LEN + NONCE_LEN + plaintext.len() + TAG_LEN);
    out.extend_from_slice(ENC_MAGIC);
    out.push(ENC_FORMAT_VERSION_V2);
    out.push(ENC_KDF_ID_ARGON2ID);
    out.extend_from_slice(&KDF_M_KIB.to_be_bytes());
    out.extend_from_slice(&KDF_T.to_be_bytes());
    out.push(KDF_P);
    debug_assert_eq!(out.len(), HEADER_LEN);
    out.extend_from_slice(salt);
    out.extend_from_slice(nonce);
    let params = Argon2idParams::new(KDF_M_KIB, KDF_T as u32, KDF_P as u32)
        .expect("Argon2 params hardcoded valid");
    let ciphertext_with_tag = password_envelope::seal(
        passphrase,
        &params,
        salt,
        nonce,
        &out[..HEADER_LEN],
        plaintext,
    )
    .expect("seal cannot fail with valid inputs");
    out.extend_from_slice(&ciphertext_with_tag);
    out
}

/// Decrypt an HRMI envelope (`v0x01` *or* `v0x02`) into a [`SecretVault`].
///
/// `v0x01` decrypts to a 32-byte seed (legacy) → seed-only vault; `v0x02`
/// decrypts to the CBOR vault. Wrong-passphrase vs. corrupted-file remain
/// indistinguishable (same as [`decrypt`]).
fn decrypt_vault(passphrase: &[u8], bytes: &[u8]) -> Result<SecretVault, String> {
    const MIN_LEN: usize = HEADER_LEN + SALT_LEN + NONCE_LEN + TAG_LEN;
    if bytes.len() < MIN_LEN {
        return Err(format!(
            "identity store is corrupt: {} bytes is below the {MIN_LEN}-byte minimum",
            bytes.len()
        ));
    }
    if &bytes[0..4] != ENC_MAGIC {
        return Err(format!(
            "identity store is in an unrecognized format (magic={:?}) — this build may be too old",
            &bytes[0..4]
        ));
    }
    match bytes[4] {
        // v1: delegate to the original fixed-length decrypt.
        ENC_FORMAT_VERSION => {
            let seed = decrypt(passphrase, bytes)?;
            Ok(SecretVault::from_seed(*seed))
        }
        ENC_FORMAT_VERSION_V2 => {
            let plaintext = decrypt_v2_plaintext(passphrase, bytes)?;
            SecretVault::from_cbor(&plaintext)
        }
        other => Err(format!(
            "identity store is in an unrecognized format (version={other:#04x}) — this build may be too old"
        )),
    }
}

/// AEAD-decrypt a `v0x02` envelope to its (variable-length) CBOR plaintext.
///
/// Mirrors [`decrypt`] but without the fixed-length assumption: the ciphertext
/// length is derived from the file length. Same KDF DoS guard (reject non-v1 KDF
/// params before the Argon2 allocation) and indistinguishable error.
fn decrypt_v2_plaintext(passphrase: &[u8], bytes: &[u8]) -> Result<Zeroizing<Vec<u8>>, String> {
    const M_KIB_OFF: usize = 6;
    const T_OFF: usize = M_KIB_OFF + 4;
    const P_OFF: usize = T_OFF + 2;
    const SALT_OFF: usize = HEADER_LEN;
    const NONCE_OFF: usize = SALT_OFF + SALT_LEN;
    const CIPHER_OFF: usize = NONCE_OFF + NONCE_LEN;

    if bytes[5] != ENC_KDF_ID_ARGON2ID {
        return Err(format!(
            "identity store is in an unrecognized format (kdf_id={:#04x}) — this build may be too old",
            bytes[5]
        ));
    }
    let m_kib = u32::from_be_bytes(bytes[M_KIB_OFF..M_KIB_OFF + 4].try_into().unwrap());
    let t = u16::from_be_bytes(bytes[T_OFF..T_OFF + 2].try_into().unwrap()) as u32;
    let p = bytes[P_OFF] as u32;
    let salt: &[u8; SALT_LEN] = bytes[SALT_OFF..NONCE_OFF].try_into().unwrap();
    let nonce: &[u8; NONCE_LEN] = bytes[NONCE_OFF..CIPHER_OFF].try_into().unwrap();
    let ciphertext_with_tag = &bytes[CIPHER_OFF..];

    // Strict v1 KDF param check before allocating Argon2 memory (DoS guard).
    if m_kib != KDF_M_KIB || t != KDF_T as u32 || p != KDF_P as u32 {
        return Err(
            "identity store could not be decrypted: wrong passphrase or corrupted file".to_string(),
        );
    }
    let params = Argon2idParams::new(m_kib, t, p).map_err(|_| {
        "identity store could not be decrypted: wrong passphrase or corrupted file".to_string()
    })?;
    let plaintext = password_envelope::open(
        passphrase,
        &params,
        salt,
        nonce,
        &bytes[..HEADER_LEN],
        ciphertext_with_tag,
    )
    .map_err(|_| {
        "identity store could not be decrypted: wrong passphrase or corrupted file".to_string()
    })?;
    Ok(plaintext)
}

/// Encrypt arbitrary key-material bytes into the HRMI `v0x02` variable-length
/// envelope, for the owner-state fleet-KeyTree encrypted-file fallback
/// (ZEB-492). Thin `pub(crate)` wrapper over [`encrypt_vault`] so the
/// owner-state layer reuses the exact production envelope rather than
/// reimplementing crypto.
///
/// Live as of ZEB-492 Task 4 via the `owner_state::save_fleet_keytree`
/// encrypted-file fallback, which the pairing install path now calls.
pub(crate) fn encrypt_vault_bytes(passphrase: &[u8], plaintext: &[u8]) -> Vec<u8> {
    encrypt_vault(passphrase, plaintext)
}

/// Decrypt an HRMI `v0x02` variable-length envelope produced by
/// [`encrypt_vault_bytes`] back to its plaintext. The returned buffer holds key
/// material and is `Zeroizing`.
///
/// `decrypt_v2_plaintext` indexes `bytes[5]` and fixed header offsets WITHOUT a
/// length check — it relies on its in-module caller [`decrypt_vault`] having
/// first done the `bytes.len() < MIN_LEN` guard + `ENC_MAGIC` + version checks.
/// This wrapper is reachable directly from the owner-state fleet-keytree file
/// fallback, so it MUST replay those same pre-checks itself; otherwise a
/// truncated/garbage `fleet_keytree.enc` (e.g. 20 bytes) would PANIC
/// (out-of-bounds) instead of returning an Err, crashing boot. Mirrors
/// [`decrypt_vault`]'s guards exactly (same `MIN_LEN`/`ENC_MAGIC` constants and
/// indistinguishable error messages).
pub(crate) fn decrypt_vault_bytes(
    passphrase: &[u8],
    bytes: &[u8],
) -> Result<Zeroizing<Vec<u8>>, String> {
    const MIN_LEN: usize = HEADER_LEN + SALT_LEN + NONCE_LEN + TAG_LEN;
    if bytes.len() < MIN_LEN {
        return Err(format!(
            "identity store is corrupt: {} bytes is below the {MIN_LEN}-byte minimum",
            bytes.len()
        ));
    }
    if &bytes[0..4] != ENC_MAGIC {
        return Err(format!(
            "identity store is in an unrecognized format (magic={:?}) — this build may be too old",
            &bytes[0..4]
        ));
    }
    // `encrypt_vault_bytes` only ever produces the v0x02 variable-length
    // envelope, so reject anything else before delegating to the v2 decryptor
    // (which assumes a v0x02 layout).
    if bytes[4] != ENC_FORMAT_VERSION_V2 {
        return Err(format!(
            "identity store is in an unrecognized format (version={:#04x}) — this build may be too old",
            bytes[4]
        ));
    }
    decrypt_v2_plaintext(passphrase, bytes)
}

// ── App-local key accessors (ZEB-363 consolidation) ─────────────────────

/// Which app-local key slot in the [`SecretVault`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultSlot {
    /// iroh transport secret key (`harmony.client`/`iroh.secret_key` legacy item).
    Iroh,
    /// Device #2 signing key (`harmony.owner`/`device_signing_key` legacy item).
    Device,
    /// Owner identity master seed (`harmony.owner`/`master_seed` legacy item).
    OwnerMasterSeed,
}

/// Load-or-create a 32-byte app-local key (e.g. the iroh transport secret key),
/// preferring the OS keychain vault but **falling back to an encrypted file**
/// (`HARMONY_PASSPHRASE` / `HARMONY_PASSPHRASE_FILE`) when no keychain is
/// available or usable.
///
/// ZEB-449: previously these keys were keychain-only — on a keychain-less box
/// (RPi5 headless, no Secret Service) or under the `HARMONY_DISABLE_KEYCHAIN`
/// kill-switch the iroh key could not be obtained and the node booted with no
/// transport. This mirrors the identity-seed (`load_or_generate_with_stores`)
/// and owner-secret (`owner_state::load_secret`) fallbacks.
///
/// `make_fallback` resolves the encrypted-file store **lazily** — it is invoked
/// only when the keychain is unavailable or unusable, so a keychain-healthy node
/// never parses the passphrase env or resolves the fallback path. A malformed
/// `HARMONY_PASSPHRASE` (or a missing `HOME`/`USERPROFILE` during path
/// resolution) must NOT break a working keychain.
///
/// Returns `(key, freshly_created)`. `freshly_created` is `true` ONLY when a
/// brand-new key was generated — never when an existing key was loaded (those
/// are the *same* identity, so the iroh `EndpointId` is preserved).
pub(crate) fn app_key_or_create_with_fallback<F>(
    slot: VaultSlot,
    legacy: &keyring::Entry,
    make_fallback: F,
) -> Result<(Zeroizing<[u8; 32]>, bool), String>
where
    F: FnOnce() -> Result<Option<EncryptedFileStore>, String>,
{
    // Mirrors the blessed `mint_owner_identity` production wrapper: the real
    // keychain is acquired HERE and injected into the testable inner, which in
    // test/test-fixtures builds receives `None` (KeychainStore::new() refuses,
    // ZEB-428) so tests never touch the developer's real credential store.
    app_key_or_create_with_stores(
        KeychainStore::new().ok().as_ref(),
        slot,
        legacy,
        make_fallback,
    )
}

/// Inner resolution with an injected keychain + a lazy fallback factory, for
/// testability (mirrors `load_or_generate_with_stores` / `owner_state::load_secret`).
///
/// 1. Keychain present and usable → the consolidated-vault behaviour of
///    [`vault_app_key_or_create_with_store`] (vault has the key → return it;
///    vault lacks it but the `legacy` single-key item exists → fold + verify +
///    delete; neither → generate and fold). `make_fallback` is never called.
/// 2. Keychain present but a **runtime** read/write fails (locked / no backend)
///    → warn and fall through to the encrypted file, exactly like
///    `owner_state::load_secret`. This is what makes a machine that *has* a
///    keychain backend installed but unusable still get a transport key.
/// 3. Keychain absent (kill-switch / headless) → use the encrypted file.
///
/// With no usable keychain AND no file fallback configured, surfaces the
/// keychain error if there was one, else loud guidance — never a silent
/// transport-disable.
fn app_key_or_create_with_stores<F>(
    keychain: Option<&KeychainStore>,
    slot: VaultSlot,
    legacy: &keyring::Entry,
    make_fallback: F,
) -> Result<(Zeroizing<[u8; 32]>, bool), String>
where
    F: FnOnce() -> Result<Option<EncryptedFileStore>, String>,
{
    // Prefer the keychain; a runtime failure is non-fatal when a file fallback
    // is configured (the seed/owner-state probe pattern).
    let mut keychain_err = None;
    if let Some(store) = keychain {
        match vault_app_key_or_create_with_store(store, slot, legacy) {
            Ok(result) => return Ok(result),
            // A post-write/consistency failure means the keychain was already
            // mutated (the key may now live there), so falling through to the
            // file would let THIS boot use a file key while the NEXT boot
            // re-prefers the keychain key — flapping the EndpointId. Terminal:
            // surface it rather than silently diverging the identity. (CodeRabbit.)
            Err(e) if is_keychain_consistency_error(&e) => return Err(e),
            Err(e) => {
                tracing::warn!(
                    "keychain unusable for the app-local key ({e}); trying the encrypted-file fallback"
                );
                keychain_err = Some(e);
            }
        }
    }
    // Only now resolve the passphrase env / fallback path (lazy: a healthy
    // keychain never reaches here).
    let enc = match make_fallback()? {
        Some(store) => store,
        None => {
            return Err(keychain_err.unwrap_or_else(|| {
                "no keychain available and HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set \
                 — cannot persist the app-local (iroh) key; see docs/headless-install.md"
                    .to_string()
            }));
        }
    };
    match enc.load()? {
        Some(key) => Ok((key, false)),
        None => {
            let mut key: Zeroizing<[u8; 32]> = Zeroizing::new([0u8; 32]);
            use rand::RngCore;
            rand::rngs::OsRng.fill_bytes(key.as_mut());
            enc.save(&key)?;
            // The key now lives durably in the file. Best-effort drop any stale
            // legacy keychain item so a later boot with a recovered keychain
            // can't shadow this file key. No-ops on a broken/absent backend;
            // mirrors owner_state::save_secret. (Cursor.)
            if let Err(e) = legacy.delete_credential() {
                if !matches!(e, keyring::Error::NoEntry) {
                    tracing::warn!(
                        "could not delete stale legacy app-local keychain item after file save: {e}"
                    );
                }
            }
            Ok((key, true))
        }
    }
}

/// Post-write/consistency failures from [`vault_app_key_or_create_with_store`]:
/// the keychain was already mutated but the verify/read-back failed, so the key
/// may now be in the keychain. These must NOT fall through to the encrypted-file
/// fallback — doing so would flap the iroh `EndpointId` across boots (file key
/// this boot, keychain key the next). Only backend-availability errors are safe
/// to fall through. Matched on the messages the accessor emits after a write
/// (`secret-vault read-back mismatch after fold`, `secret vault disappeared
/// immediately after write`).
fn is_keychain_consistency_error(err: &str) -> bool {
    err.contains("read-back mismatch") || err.contains("disappeared immediately after write")
}

/// Serializes read-modify-write sequences against the single consolidated
/// keychain vault item.
///
/// Every vault-mutating helper does `load_vault` → modify → `save_vault`. Without
/// a guard, two concurrent writers updating different slots (e.g. iroh and owner
/// folding at first startup) could each save a stale snapshot, so the last commit
/// wins and the other slot is silently dropped. Startup is sequential today, but
/// this enforces the invariant by construction rather than relying on call
/// ordering. (CodeAnt / Greptile.)
///
/// **Process-local only.** Cross-process races (two app instances sharing one
/// keychain) remain — `keyring` exposes no compare-and-swap / add-if-absent
/// primitive, the same accepted limitation documented for the
/// `write_seed_to_disk` TOCTOU. The five helpers below never call one another, so
/// this non-reentrant lock cannot deadlock.
fn vault_rmw_guard() -> std::sync::MutexGuard<'static, ()> {
    static VAULT_RMW_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    VAULT_RMW_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn vault_app_key_or_create_with_store(
    store: &KeychainStore,
    slot: VaultSlot,
    legacy: &keyring::Entry,
) -> Result<(Zeroizing<[u8; 32]>, bool), String> {
    let _vault_guard = vault_rmw_guard();
    let mut vault = match store.load_vault() {
        Ok(Some(v)) => v,
        // No keychain vault item — keep app-local keys in their own legacy item.
        Ok(None) => return read_legacy_key_or_create_persisting(legacy),
        // Vault item present but UNREADABLE (corrupt / unknown-version). Don't take
        // iroh transport down with the seed: degrade to the pre-ZEB-363 per-item
        // legacy key (read-or-create), leaving the corrupt vault intact — never
        // overwritten, mirroring the seed path which falls back to identity.enc on
        // the same error. (Cursor High; consolidation must not widen the blast
        // radius of a corrupt item.)
        Err(e) => {
            tracing::warn!(
                "keychain vault unreadable ({e}); using the legacy per-item app-local \
                 key and leaving the vault intact"
            );
            return read_legacy_key_or_create_persisting(legacy);
        }
    };

    if let Some(k) = vault.slot_key(slot) {
        return Ok((Zeroizing::new(*k), false));
    }

    let (key, fresh) = read_legacy_key_or_generate(legacy)?;
    vault.set_slot_key(slot, Some(*key));
    store.save_vault(&vault)?;

    // Verify the key is durably in the vault BEFORE removing the legacy item, so
    // a failed write never loses the key.
    let back = store
        .load_vault()?
        .ok_or_else(|| "secret vault disappeared immediately after write".to_string())?;
    if back.slot_key(slot) != Some(&*key) {
        return Err("secret-vault read-back mismatch after fold; legacy item retained".to_string());
    }

    // The key now lives in the vault. Best-effort remove the legacy item so only
    // the one consolidated item remains. (NoEntry on the generate path is fine.)
    if let Err(e) = legacy.delete_credential() {
        if !matches!(e, keyring::Error::NoEntry) {
            tracing::warn!("could not delete migrated legacy keychain item: {e}");
        }
    }

    Ok((key, fresh))
}

/// Read a 32-byte key from a legacy single-key item, or generate a fresh one
/// (NOT persisted here — the caller folds it into the vault).
fn read_legacy_key_or_generate(
    legacy: &keyring::Entry,
) -> Result<(Zeroizing<[u8; 32]>, bool), String> {
    match legacy.get_secret() {
        Ok(bytes) => {
            let bytes = Zeroizing::new(bytes);
            Ok((blob_to_seed(&bytes)?, false))
        }
        Err(keyring::Error::NoEntry) => {
            let mut key: Zeroizing<[u8; 32]> = Zeroizing::new([0u8; 32]);
            use rand::RngCore;
            rand::rngs::OsRng.fill_bytes(key.as_mut());
            Ok((key, true))
        }
        Err(e) => Err(format!("legacy keychain item read failed: {e}")),
    }
}

/// Fallback when there is no keychain vault item: read-or-create the key in the
/// `legacy` single-key item itself (pre-ZEB-363 behaviour).
fn read_legacy_key_or_create_persisting(
    legacy: &keyring::Entry,
) -> Result<(Zeroizing<[u8; 32]>, bool), String> {
    let (key, fresh) = read_legacy_key_or_generate(legacy)?;
    if fresh {
        legacy
            .set_secret(key.as_ref())
            .map_err(|e| format!("legacy keychain item write failed: {e}"))?;
    }
    Ok((key, fresh))
}

/// Read a 32-byte key from a legacy single-key item (no generation). `Ok(None)`
/// if absent.
fn read_legacy_slot(legacy: &keyring::Entry) -> Result<Option<Zeroizing<[u8; 32]>>, String> {
    match legacy.get_secret() {
        Ok(bytes) => {
            let bytes = Zeroizing::new(bytes);
            Ok(Some(blob_to_seed(&bytes)?))
        }
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("legacy keychain item read failed: {e}")),
    }
}

/// Read an app-local key **slot** from the keychain vault, folding in (and
/// deleting, after a verified read-back) a `legacy` single-key item if the vault
/// lacks it. Returns `Ok(None)` when the key is in neither place.
///
/// Unlike [`vault_app_key_or_create`], this NEVER generates a key — it is a pure
/// read for callers (owner-state) that manage their own creation. With no
/// keychain vault item, reads the `legacy` item directly.
pub fn vault_load_slot(
    slot: VaultSlot,
    legacy: &keyring::Entry,
) -> Result<Option<Zeroizing<[u8; 32]>>, String> {
    vault_load_slot_with_store(&KeychainStore::new()?, slot, legacy)
}

fn vault_load_slot_with_store(
    store: &KeychainStore,
    slot: VaultSlot,
    legacy: &keyring::Entry,
) -> Result<Option<Zeroizing<[u8; 32]>>, String> {
    let _vault_guard = vault_rmw_guard();
    let mut vault = match store.load_vault() {
        Ok(Some(v)) => v,
        Ok(None) => return read_legacy_slot(legacy),
        // Unreadable vault: read the slot from the legacy item rather than
        // skipping the owner secret entirely, leaving the corrupt vault intact.
        // (Cursor High; consistent with the iroh accessor + the seed→enc path.)
        Err(e) => {
            tracing::warn!(
                "keychain vault unreadable ({e}); reading the app-local slot from the \
                 legacy item and leaving the vault intact"
            );
            return read_legacy_slot(legacy);
        }
    };
    if let Some(k) = vault.slot_key(slot) {
        return Ok(Some(Zeroizing::new(*k)));
    }
    let Some(key) = read_legacy_slot(legacy)? else {
        return Ok(None);
    };
    vault.set_slot_key(slot, Some(*key));
    store.save_vault(&vault)?;
    let back = store
        .load_vault()?
        .ok_or_else(|| "secret vault disappeared immediately after write".to_string())?;
    if back.slot_key(slot) != Some(&*key) {
        return Err("secret-vault read-back mismatch after fold; legacy item retained".to_string());
    }
    if let Err(e) = legacy.delete_credential() {
        if !matches!(e, keyring::Error::NoEntry) {
            tracing::warn!("could not delete migrated legacy keychain item: {e}");
        }
    }
    Ok(Some(key))
}

/// Write an app-local key into the keychain vault (read-modify-write, preserving
/// other slots). Returns `Ok(false)` when there is no keychain vault item to
/// write into, so the caller can use its own fallback store.
pub fn vault_save_slot(slot: VaultSlot, key: &[u8; 32]) -> Result<bool, String> {
    vault_save_slot_with_store(&KeychainStore::new()?, slot, key)
}

fn vault_save_slot_with_store(
    store: &KeychainStore,
    slot: VaultSlot,
    key: &[u8; 32],
) -> Result<bool, String> {
    let _vault_guard = vault_rmw_guard();
    let Some(mut vault) = store.load_vault()? else {
        return Ok(false);
    };
    vault.set_slot_key(slot, Some(*key));
    store.save_vault(&vault)?;
    // Unlike the fold paths in `vault_app_key_or_create_with_store` /
    // `vault_load_slot_with_store`, this site intentionally omits a read-back
    // assertion: it performs no subsequent destructive action (no legacy item is
    // deleted on the strength of the write), so there is nothing to confirm
    // before. The read-back in the other two guards a legacy delete, not the
    // write itself. Do NOT copy this without re-adding the read-back if a new
    // call site deletes another copy after this returns.
    Ok(true)
}

/// Clear an app-local key slot in the keychain vault (if a vault item exists) and
/// best-effort delete any `legacy` single-key item. Idempotent.
pub fn vault_clear_slot(slot: VaultSlot, legacy: &keyring::Entry) -> Result<(), String> {
    vault_clear_slot_with_store(&KeychainStore::new()?, slot, legacy)
}

fn vault_clear_slot_with_store(
    store: &KeychainStore,
    slot: VaultSlot,
    legacy: &keyring::Entry,
) -> Result<(), String> {
    let _vault_guard = vault_rmw_guard();
    match store.load_vault() {
        Ok(Some(mut vault)) => {
            if vault.slot_key(slot).is_some() {
                vault.set_slot_key(slot, None);
                store.save_vault(&vault)?;
            }
        }
        Ok(None) => {}
        // Unreadable vault: we can't selectively clear one slot without risking an
        // overwrite of the whole (corrupt) item, so leave it intact and still clear
        // the legacy item below. An unreadable vault can't resurrect the secret on
        // load anyway (the read fails the same way). (Cursor High.)
        Err(e) => tracing::warn!(
            "keychain vault unreadable ({e}); leaving it intact and clearing only the \
             legacy item"
        ),
    }
    if let Err(e) = legacy.delete_credential() {
        if !matches!(e, keyring::Error::NoEntry) {
            return Err(format!("legacy keychain item delete failed: {e}"));
        }
    }
    Ok(())
}

// ── Distributed fleet KeyTree (variable-length vault item, ZEB-492) ──────
//
// Siblings of the 32-byte `vault_*_slot` accessors above, operating on the
// variable-length `SecretVault::fleet_keytree` field (CBOR of
// `FleetKeyMaterial`, ~161 bytes) instead of a fixed slot. They mirror the
// same store-resolution shape but take NO `legacy: &keyring::Entry`: the
// fleet KeyTree slot is brand-new (ZEB-492), so there is no pre-consolidation
// per-item entry to fold in or delete.

/// Read the distributed fleet KeyTree material from the keychain vault.
/// `Ok(None)` ONLY for genuine absence — no vault item, or a vault item that
/// carries no fleet KeyTree. An unreadable/locked vault propagates `Err` so the
/// caller can distinguish "no material" from "couldn't read the keychain" and
/// surface an actionable error rather than silently booting a cert-only device
/// with no fleet engines (ZEB-492 Qodo/CodeAnt round 1, FIX B). Mirrors the
/// error-surfacing contract of `load_secret`/`vault_load_slot` (the slot
/// accessors degrade to a legacy item; this brand-new slot has none, so the
/// only faithful "couldn't read" signal is to propagate the `Err`).
/// The returned buffer holds key material and is wrapped in `Zeroizing`.
pub fn vault_load_fleet_keytree() -> Result<Option<Zeroizing<Vec<u8>>>, String> {
    vault_load_fleet_keytree_with_store(&KeychainStore::new()?)
}

fn vault_load_fleet_keytree_with_store(
    store: &KeychainStore,
) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
    let _vault_guard = vault_rmw_guard();
    let vault = match store.load_vault() {
        Ok(Some(v)) => v,
        // No vault item — genuine absence. (No legacy fallback: this slot is new.)
        Ok(None) => return Ok(None),
        // Unreadable vault (corrupt / locked / unknown-version): leave it intact
        // and PROPAGATE the error. Reporting `Ok(None)` here would be
        // indistinguishable from genuine absence, so a locked keychain would let
        // a cert-only device boot with no fleet engines and no signal. The caller
        // (`owner_state::load_fleet_keytree`) captures this and only swallows it
        // if the encrypted-file fallback also has nothing usable.
        Err(e) => {
            tracing::warn!("keychain vault unreadable ({e}); leaving the vault intact");
            return Err(e);
        }
    };
    // Vault read fine but carries no fleet KeyTree → genuine absence.
    Ok(vault.fleet_keytree().map(|b| Zeroizing::new(b.to_vec())))
}

/// Write the distributed fleet KeyTree material into the keychain vault
/// (read-modify-write, preserving every other field). Returns `Ok(false)`
/// when there is no keychain vault item to write into, so the caller can fall
/// back to its own (encrypted-file) store — identical contract to
/// [`vault_save_slot`].
pub fn vault_save_fleet_keytree(material: &[u8]) -> Result<bool, String> {
    vault_save_fleet_keytree_with_store(&KeychainStore::new()?, material)
}

fn vault_save_fleet_keytree_with_store(
    store: &KeychainStore,
    material: &[u8],
) -> Result<bool, String> {
    let _vault_guard = vault_rmw_guard();
    let Some(mut vault) = store.load_vault()? else {
        return Ok(false);
    };
    vault.set_fleet_keytree(Some(material.to_vec()));
    store.save_vault(&vault)?;
    // No read-back assertion: like `vault_save_slot_with_store`, this site takes
    // no subsequent destructive action on the strength of the write, so there is
    // nothing to confirm before. Do NOT copy without re-adding the read-back if a
    // new call site deletes another copy after this returns.
    Ok(true)
}

/// Clear the distributed fleet KeyTree material in the keychain vault (if a
/// vault item exists). Idempotent.
pub fn vault_clear_fleet_keytree() -> Result<(), String> {
    vault_clear_fleet_keytree_with_store(&KeychainStore::new()?)
}

fn vault_clear_fleet_keytree_with_store(store: &KeychainStore) -> Result<(), String> {
    let _vault_guard = vault_rmw_guard();
    match store.load_vault() {
        Ok(Some(mut vault)) => {
            if vault.fleet_keytree().is_some() {
                vault.set_fleet_keytree(None);
                store.save_vault(&vault)?;
            }
        }
        Ok(None) => {}
        // Unreadable vault: leave it intact (can't selectively clear one field
        // without risking an overwrite of the whole corrupt item), but PROPAGATE
        // the read error rather than reporting a successful clear. Unlike
        // `vault_clear_slot_with_store` — which still clears the *legacy* per-item
        // entry on this branch, so the slot's overall clear can succeed — the
        // fleet KeyTree slot is brand-new (ZEB-492) and has NO legacy fallback, so
        // an unreadable vault means the clear genuinely did NOT happen. The
        // load-side (`vault_load_fleet_keytree_with_store`) likewise propagates
        // this Err. Swallowing it here would let `install_joiner_state_inner`'s
        // None branch (FIX D: `clear_fleet_keytree(...)?` runs BEFORE the
        // owner_state commit) commit owner_state while stale fleet material
        // survives in a temporarily-locked keychain — that material would then
        // shadow fresh state once the keychain becomes readable. Returning Err
        // aborts the install before any owner_state write.
        Err(e) => {
            tracing::warn!(
                "keychain vault unreadable ({e}); leaving it intact and propagating the \
                 fleet-keytree clear failure"
            );
            return Err(e);
        }
    }
    Ok(())
}

// ── KeyStore trait ──────────────────────────────────────────────────────

/// Common interface for identity storage backends.
///
/// ZEB-363: a backend stores one [`SecretVault`] per item/file. `load_vault` /
/// `save_vault` are the primary surface; the seed-level `load` / `save` are
/// convenience wrappers over them (read/write only the `seed` field) used by the
/// existing seed-resolution machinery, which is otherwise unchanged.
pub(crate) trait KeyStore {
    /// Load the full secret vault. Returns `Ok(None)` if no entry exists.
    fn load_vault(&self) -> Result<Option<SecretVault>, String>;
    /// Save the full secret vault (overwriting any existing item).
    fn save_vault(&self, vault: &SecretVault) -> Result<(), String>;

    /// Load just the master seed. `Ok(None)` if no entry exists.
    fn load(&self) -> Result<Option<Zeroizing<[u8; BLOB_LEN]>>, String> {
        Ok(self.load_vault()?.map(|v| Zeroizing::new(v.seed)))
    }

    /// Save the master seed, **preserving any existing app-local keys**.
    ///
    /// Read-modify-write: only the `seed` field is replaced. A node-seed write
    /// (fresh-generate / restore) must NOT wipe the device's iroh / device /
    /// owner-master keys — matching the pre-consolidation behaviour where those
    /// lived in separate keychain items untouched by a seed write (so a restore
    /// preserves the EndpointId and owner backup eligibility). On a fresh install
    /// (no existing vault) this creates a seed-only vault.
    fn save(&self, seed: &[u8; BLOB_LEN]) -> Result<(), String> {
        // RMW-preserve app-local keys when an existing vault is readable. A
        // genuine read error (corrupt / unknown-version item) is a HARD FAIL:
        // silently overwriting it would discard app-local keys and break the
        // corruption-handling contract (`item_bytes_to_vault` documents
        // non-legacy/non-CBOR as "hard error, never overwritten"). Only
        // `Ok(None)` (no entry) creates a fresh seed-only vault.
        //
        // The encrypted-file backend overrides this: for it a read error means a
        // file it cannot DECRYPT (wrong passphrase during a deliberate restore /
        // regen, or AEAD-corrupt), which it intentionally overwrites — see
        // `impl KeyStore for EncryptedFileStore`. Passphrase rotation never
        // relies on this path (it re-encrypts the whole vault via
        // `rotate_passphrase`).
        let mut vault = match self.load_vault()? {
            Some(v) => v,
            None => SecretVault::from_seed(*seed),
        };
        vault.version = VAULT_VERSION;
        vault.seed = *seed;
        self.save_vault(&vault)
    }
}

// ── FileStore ───────────────────────────────────────────────────────────

/// File-based identity storage at a given path.
///
/// Test-only fixture helper for writing seed blobs to a plaintext file at a
/// given path. Production code never writes plaintext at all — the only
/// backends are `KeychainStore` and `EncryptedFileStore`.
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
    fn load_vault(&self) -> Result<Option<SecretVault>, String> {
        let raw = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("Failed to read {}: {e}", self.path.display())),
        };
        let buf = Zeroizing::new(raw);
        #[cfg(unix)]
        warn_permissions(&self.path);
        Ok(Some(item_bytes_to_vault(&buf)?))
    }

    fn save_vault(&self, vault: &SecretVault) -> Result<(), String> {
        let cbor = vault.to_cbor()?;
        write_atomic_0600(&self.path, &cbor)
    }
}

// ── KeychainStore ───────────────────────────────────────────────────────

const KEYCHAIN_SERVICE: &str = "harmony";
const KEYCHAIN_ACCOUNT: &str = "identity";
/// ZEB-429: quarantine address for a pre-ZEB-363 relic occupying
/// `KEYCHAIN_ACCOUNT`. A distinct account under the same service, so the primary
/// vault address is freed for a clean vault while the relic's bytes (which may be
/// an old identity's only seed copy) are preserved — never deleted.
const KEYCHAIN_BACKUP_ACCOUNT: &str = "identity.pre363-backup";

/// OS-native keychain storage via the `keyring` crate.
pub struct KeychainStore {
    entry: keyring::Entry,
    /// ZEB-429: backup slot a pre-ZEB-363 relic is copied to before the primary
    /// is evicted (see `quarantine_pre363_relic`). Eagerly bound so tests can
    /// inject a mock backend rather than reaching the real keychain.
    backup: keyring::Entry,
}

impl KeychainStore {
    /// Create a store backed by the real OS keychain.
    ///
    /// ## ZEB-428 — the real keychain is a process-global resource that
    /// tempdir-based test isolation cannot scope
    ///
    /// A `--workspace --all-targets --features test-fixtures` sweep once
    /// silently overwrote a developer's real owner identity: a test
    /// redirected `HOME` to a tempdir, but this constructor addresses the
    /// OS keychain by fixed service/account names, so the test's mint
    /// persisted into the developer's real credential store. The tempdir
    /// evaporated; the foreign keychain entry stayed; the next boot failed
    /// the enrollment gate and the identity was unrecoverable.
    ///
    /// Three gates close that class:
    /// 1. `HARMONY_DISABLE_KEYCHAIN` (non-empty, not `"0"`) → `Err` in every
    ///    build. An explicit operator kill-switch; beats all overrides.
    /// 2. In test builds (`cfg(test)` or the `test-fixtures` feature — every
    ///    integration-test compilation requires the latter) → `Err` unless
    ///    `HARMONY_ALLOW_REAL_KEYCHAIN=1`. Production builds don't compile
    ///    this branch, so the app's keychain behavior is unchanged.
    /// 3. A named profile (ZEB-446) → `Err` in every build: keychain names are
    ///    machine-global, so named profiles are file-vault-only.
    ///
    /// Every caller already tolerates `Err` (`.ok()` → encrypted-file
    /// fallback, or propagation) — a gated test run behaves exactly like
    /// Linux CI, where no keychain backend exists and the suite is green.
    pub fn new() -> Result<Self, String> {
        if std::env::var("HARMONY_DISABLE_KEYCHAIN").is_ok_and(|v| !v.is_empty() && v != "0") {
            return Err("OS keychain disabled via HARMONY_DISABLE_KEYCHAIN (ZEB-428)".to_string());
        }
        // ZEB-446: named profiles never touch the OS keychain — the
        // service/account names below are machine-global, so two profiles
        // on one machine would read/clobber EACH OTHER'S vault (the
        // ZEB-428 class, in production). Named profiles use the
        // encrypted-file vault under their own identity dir instead.
        if let Some(p) = crate::profile::active_profile() {
            return Err(format!(
                "OS keychain refused for named profile {p:?} (ZEB-446): keychain names are \
                 machine-global; this profile uses the encrypted-file vault — set \
                 HARMONY_PASSPHRASE or HARMONY_PASSPHRASE_FILE"
            ));
        }
        #[cfg(any(test, feature = "test-fixtures"))]
        {
            if std::env::var("HARMONY_ALLOW_REAL_KEYCHAIN").as_deref() != Ok("1") {
                return Err(
                    "real OS keychain refused in test builds (ZEB-428): tempdir isolation \
                     cannot scope the OS keychain, so tests must inject None or a mock; \
                     set HARMONY_ALLOW_REAL_KEYCHAIN=1 only for a test that deliberately \
                     exercises the real credential store"
                        .to_string(),
                );
            }
        }
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .map_err(|e| format!("keychain entry creation failed: {e}"))?;
        let backup = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_BACKUP_ACCOUNT)
            .map_err(|e| format!("keychain backup entry creation failed: {e}"))?;
        Ok(Self { entry, backup })
    }

    /// Create a store backed by the keyring mock credential store (for tests).
    /// The `backup` slot is an independent mock — modelling two separate keychain
    /// items, exactly as the real store has two distinct accounts.
    #[cfg(test)]
    pub fn new_mock() -> Self {
        let entry =
            keyring::Entry::new_with_credential(Box::new(keyring::mock::MockCredential::default()));
        let backup =
            keyring::Entry::new_with_credential(Box::new(keyring::mock::MockCredential::default()));
        Self { entry, backup }
    }

    /// Create a store that always fails on save (for testing fallback).
    ///
    /// Load returns `NoEntry`; save always returns an error.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn new_failing_mock() -> Self {
        let entry = keyring::Entry::new_with_credential(Box::new(AlwaysFailOnSave));
        let backup =
            keyring::Entry::new_with_credential(Box::new(keyring::mock::MockCredential::default()));
        Self { entry, backup }
    }

    /// Create a store where ALL operations fail (simulates inaccessible keychain).
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn new_load_failing_mock() -> Self {
        let entry = keyring::Entry::new_with_credential(Box::new(AlwaysFailOnLoad));
        let backup =
            keyring::Entry::new_with_credential(Box::new(keyring::mock::MockCredential::default()));
        Self { entry, backup }
    }

    /// Delete the keychain entry. Returns `Ok(())` if deleted or not present.
    ///
    /// Used by `write_seed_to_disk_with_keychain` to best-effort unlink a stale
    /// keychain entry after a successful force-write to the encrypted-file backend.
    /// Also used by integration tests (different crate boundary) to clean up
    /// stale keychain state before exercising `*_cli` functions.
    pub fn delete(&self) -> Result<(), keyring::Error> {
        self.entry.delete_credential()
    }
}

impl KeyStore for KeychainStore {
    fn load_vault(&self) -> Result<Option<SecretVault>, String> {
        match self.entry.get_secret() {
            Ok(bytes) => {
                let buf = Zeroizing::new(bytes);
                match item_bytes_to_vault(&buf) {
                    Ok(v) => Ok(Some(v)),
                    // ZEB-429: a pre-ZEB-363 build wrote a DIFFERENT payload to this
                    // same address (harmony/identity) — integer-version-tagged CBOR
                    // that cannot be a SecretVault (always a CBOR map or a bare
                    // 32-byte seed). Left intact it permanently blocked vault
                    // migration: every read Err'd → legacy/file fallback, 3 warns per
                    // boot. Quarantine the relic (copy to a backup account, verify,
                    // evict the primary) and report the address as now-empty, so a
                    // fresh vault is created on the next write. A decode failure that
                    // is NOT this relic signature (a future map-shaped vault version,
                    // or other corruption) is left intact and propagated, per the
                    // pre-existing "never overwrite an unreadable item" contract.
                    Err(e) => {
                        if is_pre363_relic(&buf) {
                            match self.quarantine_pre363_relic(&buf) {
                                Ok(()) => Ok(None),
                                Err(qe) => {
                                    tracing::warn!(
                                        "ZEB-429: could not quarantine pre-ZEB-363 keychain relic \
                                         ({qe}); leaving it intact and falling back"
                                    );
                                    Err(e)
                                }
                            }
                        } else {
                            Err(e)
                        }
                    }
                }
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("keychain load failed: {e}")),
        }
    }

    fn save_vault(&self, vault: &SecretVault) -> Result<(), String> {
        let cbor = vault.to_cbor()?;
        self.entry
            .set_secret(&cbor)
            .map_err(|e| format!("keychain save failed: {e}"))
    }
}

impl KeychainStore {
    /// ZEB-429: preserve, then evict, a pre-ZEB-363 relic occupying the vault
    /// address. Delegates to [`quarantine_relic_between`] with this store's
    /// primary + backup entries.
    fn quarantine_pre363_relic(&self, raw: &[u8]) -> Result<(), String> {
        quarantine_relic_between(&self.entry, &self.backup, raw)
    }
}

/// ZEB-429: true iff `bytes` carries the pre-ZEB-363 relic signature — a
/// **well-formed** top-level CBOR unsigned integer (major type 0, the "version
/// tag" that decodes as `invalid type: integer 1, expected map`). The current
/// vault is always a CBOR map (major type 5) or a bare `BLOB_LEN`-byte seed, so
/// neither is ever misclassified. Only consulted after `item_bytes_to_vault`
/// has already failed to decode `bytes`.
///
/// The header must be a *complete* uint encoding: an inline value (`0x00..=0x17`)
/// or a `0x18/0x19/0x1a/0x1b` header with its full 1/2/4/8-byte payload present.
/// Reserved additional-info (`0x1c..=0x1e`), the illegal indefinite form
/// (`0x1f`), and truncated multi-byte headers are NOT relics — matching mere
/// major-type bits would let arbitrary corruption be quarantined instead of
/// hard-failing.
///
/// Deliberately narrow: other undecodable shapes (a future map-shaped vault
/// version, or arbitrary corruption) are NOT relics and stay under the
/// pre-existing "leave intact / hard-fail" contract — so an older build never
/// clobbers a newer vault, and genuine corruption is never silently discarded.
fn is_pre363_relic(bytes: &[u8]) -> bool {
    // A bare legacy seed is a valid item, never a relic.
    if bytes.len() == BLOB_LEN {
        return false;
    }
    match bytes.first().copied() {
        // Inline unsigned integer (value 0..=23): complete in one byte.
        Some(0x00..=0x17) => true,
        // uint8/16/32/64 headers: relic only if the promised payload is present.
        Some(0x18) => bytes.len() >= 2,
        Some(0x19) => bytes.len() >= 3,
        Some(0x1a) => bytes.len() >= 5,
        Some(0x1b) => bytes.len() >= 9,
        // Reserved (0x1c..=0x1e), indefinite (0x1f), any other major type, or
        // an empty buffer → not a relic; keep the hard-fail contract.
        _ => false,
    }
}

/// ZEB-429: the store-agnostic quarantine step, split out so tests can inject
/// mock `primary`/`backup` entries (a real backup entry can't share state with a
/// `MockCredential`). Copy → verify → delete; never deletes without a verified,
/// byte-identical backup, so a crash mid-quarantine can never leave zero copies.
fn quarantine_relic_between(
    primary: &keyring::Entry,
    backup: &keyring::Entry,
    raw: &[u8],
) -> Result<(), String> {
    match backup.get_secret() {
        // Backup already holds these exact bytes → a prior quarantine; idempotent.
        Ok(existing) => {
            let existing = Zeroizing::new(existing);
            if existing.as_slice() != raw {
                return Err(
                    "backup account already holds different bytes; refusing to clobber it"
                        .to_string(),
                );
            }
        }
        Err(keyring::Error::NoEntry) => {
            backup
                .set_secret(raw)
                .map_err(|e| format!("relic backup write failed: {e}"))?;
            // Confirm the copy is durable + byte-exact BEFORE evicting the primary.
            let back = Zeroizing::new(
                backup
                    .get_secret()
                    .map_err(|e| format!("relic backup read-back failed: {e}"))?,
            );
            if back.as_slice() != raw {
                return Err("relic backup read-back mismatch; leaving the relic intact".to_string());
            }
        }
        Err(e) => return Err(format!("relic backup probe failed: {e}")),
    }
    // Backup verified present + identical → safe to free the primary address.
    primary
        .delete_credential()
        .map_err(|e| format!("relic eviction failed: {e}"))?;
    tracing::info!(
        "ZEB-429: quarantined a pre-ZEB-363 keychain relic to account \
         '{KEYCHAIN_BACKUP_ACCOUNT}'; a fresh secret vault will be created on next write"
    );
    Ok(())
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
        return Err(
            "contains an empty passphrase (after trimming one trailing newline)".to_string(),
        );
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
    /// environment variables — see [`resolve_passphrase_env`] for the
    /// resolution rules.
    pub(crate) fn from_env(path: PathBuf) -> Result<Option<Self>, String> {
        Ok(resolve_passphrase_env()?.map(|passphrase| Self::new(path, passphrase)))
    }
}

/// Resolve the vault passphrase from `HARMONY_PASSPHRASE` /
/// `HARMONY_PASSPHRASE_FILE`.
///
/// The single source of truth shared by [`EncryptedFileStore::from_env`]
/// (the consumer) and [`passphrase_env_configured`] (the ZEB-446 fail-fast
/// gate), so the gate can never pass a configuration the vault will later
/// reject — or refuse one it would accept (PR #245 round 4, Cursor Bugbot
/// and Greptile: the two used to diverge on whitespace-only direct values
/// and on trimming of the file path).
///
/// Returns:
///   - `Ok(None)` if neither env var is set
///   - `Ok(Some(passphrase))` if a non-empty passphrase resolves
///   - `Err(...)` if either var is set but malformed (empty, file unreadable,
///     resolves to empty)
///
/// Precedence: `HARMONY_PASSPHRASE` (direct) wins over `HARMONY_PASSPHRASE_FILE`
/// when both are set; a warning is logged.
///
/// `pub(crate)` so the owner-state variable-length fleet-KeyTree fallback
/// (ZEB-492) resolves the passphrase through the SAME logic the
/// `EncryptedFileStore` 32-byte fallback uses — the two must never diverge.
pub(crate) fn resolve_passphrase_env() -> Result<Option<SecretString>, String> {
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

    Ok(Some(SecretString::from(passphrase_str)))
}

impl KeyStore for EncryptedFileStore {
    fn load_vault(&self) -> Result<Option<SecretVault>, String> {
        let raw = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("Failed to read {}: {e}", self.path.display())),
        };
        // `decrypt_vault` dispatches on the HRMI version byte: v0x01 → seed-only
        // vault (legacy), v0x02 → full CBOR vault. Both are Zeroizing-backed.
        let vault = decrypt_vault(self.passphrase.expose_secret().as_bytes(), &raw)?;
        Ok(Some(vault))
    }

    fn save_vault(&self, vault: &SecretVault) -> Result<(), String> {
        let plaintext = vault.to_cbor()?;
        let bytes = encrypt_vault(self.passphrase.expose_secret().as_bytes(), &plaintext);
        write_atomic_0600(&self.path, &bytes)
    }

    /// Overrides the default seed-level `save` for the encrypted-file backend:
    /// an existing file we cannot DECRYPT (wrong passphrase during a deliberate
    /// force-restore / regen, or an AEAD-corrupt file) is replaced with a fresh
    /// seed-only vault. A deliberate restore write means "replace this identity,"
    /// and AEAD makes wrong-passphrase indistinguishable from corruption, so the
    /// distinction the keychain backend draws (hard-fail on corrupt) can't be
    /// made here. This preserves the pre-ZEB-363 blind-overwrite restore
    /// semantics for the headless / encrypted-file path. A *readable* file is
    /// RMW-preserved exactly like the default.
    fn save(&self, seed: &[u8; BLOB_LEN]) -> Result<(), String> {
        let mut vault = match self.load_vault() {
            Ok(Some(v)) => v,
            Ok(None) | Err(_) => SecretVault::from_seed(*seed),
        };
        vault.version = VAULT_VERSION;
        vault.seed = *seed;
        self.save_vault(&vault)
    }
}

// ── Public API (unchanged shape) ────────────────────────────────────────

/// Resolve the identity file path. `~/.harmony/identity.key` on the
/// default profile; `~/.harmony/profiles/<p>/identity.key` on a named
/// profile (ZEB-446 — named profiles get their own identity tree, which
/// also scopes the ZEB-449 encrypted-file vault and `iroh_sk.enc`).
pub fn resolve_path(override_path: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(p) = override_path {
        return Ok(p.to_path_buf());
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| {
            "Cannot determine identity file path: neither $HOME nor $USERPROFILE is set".to_string()
        })?;
    Ok(identity_path_in(
        Path::new(&home),
        crate::profile::active_profile(),
    ))
}

/// Pure path join for [`resolve_path`] — unit-testable without env state.
fn identity_path_in(home: &Path, profile: Option<&str>) -> PathBuf {
    let root = home.join(".harmony");
    let root = match profile {
        Some(p) => root.join("profiles").join(p),
        None => root,
    };
    root.join("identity.key")
}

/// ZEB-446: true when the encrypted-file vault has a passphrase source.
/// Named profiles are file-vault-only, so entrypoints fail fast on this
/// instead of letting the first vault access fail later (the ZEB-450
/// silent-degradation class).
///
/// Defined as "[`EncryptedFileStore::from_env`] would yield a store":
/// the gate and the consumer share [`resolve_passphrase_env`], so they
/// cannot drift apart (PR #245 round 4, Cursor Bugbot + Greptile). A
/// set-but-malformed value (empty string, unreadable or blank file)
/// counts as NOT configured — boot fails here, at the gate, rather than
/// at the first vault access.
pub fn passphrase_env_configured() -> bool {
    matches!(resolve_passphrase_env(), Ok(Some(_)))
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
#[allow(dead_code)] // pre-existing; tracked for cleanup
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
    // `None` lock target: this variant is test-only (hermetic, single-process,
    // isolated stores) — the cross-process generate guard is exercised through
    // `read_seed_from_disk_with_keychain`, which passes a real path.
    load_or_generate_with_stores_post_probe(None, keychain, keychain_healthy, encrypted)
}

/// Generate a fresh random 32-byte seed and persist it via
/// [`save_with_fallback`] (keychain preferred, encrypted-file fallback).
///
/// Callers that need cross-process safety (the production boot path) MUST run
/// this while holding [`with_identity_write_guards`] and MUST have re-probed
/// both stores under the guards first — this function unconditionally overwrites
/// via `save_with_fallback`, so a concurrent writer's identity would be clobbered
/// without that double-check.
fn generate_and_save_seed(
    keychain: Option<&KeychainStore>,
    keychain_healthy: bool,
    encrypted: Option<&EncryptedFileStore>,
) -> Result<Zeroizing<[u8; BLOB_LEN]>, String> {
    let mut seed_buf: Zeroizing<[u8; BLOB_LEN]> = Zeroizing::new([0u8; BLOB_LEN]);
    use rand::RngCore;
    rand::rngs::OsRng.fill_bytes(seed_buf.as_mut());
    let _ = save_with_fallback(
        keychain_healthy,
        keychain,
        encrypted,
        &seed_buf,
        || {
            "no identity store available: keychain unavailable and HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set — see docs/headless-install.md".to_string()
        },
        |e| {
            format!(
            "keychain save failed and no encrypted fallback configured: {e} — see docs/headless-install.md"
        )
        },
    )?;
    Ok(seed_buf)
}

/// Resolve the seed from an already-keychain-probed set of stores: return an
/// existing encrypted-file identity, or generate + persist a fresh one.
///
/// `lock_target` is the identity path used to derive the `identity.enc.lock`
/// sibling. Production callers pass `Some(path)`: when generation is needed this
/// closes the cross-process generate-vs-restore race (ZEB-735) by acquiring the
/// shared [`with_identity_write_guards`] and **double-checking** both stores
/// under them — a concurrent `restore` that wrote between our lock-free probe
/// and the guard is observed and returned instead of being clobbered. The
/// initial `enc.load()` probe stays *outside* the guard so the common
/// already-exists boot path (every boot after the first) never touches the lock.
/// Hermetic unit tests pass `None` (single-process, isolated tempdirs) to skip
/// the guard and avoid serializing parallel tests on the process-global mutex.
fn load_or_generate_with_stores_post_probe(
    lock_target: Option<&Path>,
    keychain: Option<&KeychainStore>,
    keychain_healthy: bool,
    encrypted: Option<&EncryptedFileStore>,
) -> Result<Zeroizing<[u8; BLOB_LEN]>, String> {
    // Lock-free fast path: an existing encrypted identity is returned without
    // ever taking the write guards (this is the hot every-boot read path).
    if let Some(enc) = encrypted {
        match enc.load() {
            Ok(Some(seed)) => return Ok(seed),
            Ok(None) => { /* fall through to guarded generate */ }
            Err(e) => return Err(e),
        }
    }

    let generate = || generate_and_save_seed(keychain, keychain_healthy, encrypted);

    match lock_target {
        // Production, with a writable destination: serialize the generate
        // against every other identity writer and re-probe under the guard
        // before writing.
        Some(identity_path) if keychain_healthy || encrypted.is_some() => {
            with_identity_write_guards(identity_path, || {
                // Double-check under the lock: a concurrent restore/generate may
                // have written between our lock-free probe above and acquiring
                // the guard. Mirror each store's first-probe error policy exactly
                // — keychain errors warn-and-continue (as in the caller's probe),
                // but an encrypted-file load ERROR must hard-fail, never silently
                // regenerate over a present-but-unreadable file (the resolution
                // chain's "wrong passphrase / corruption ⇒ never regenerate"
                // invariant, same as the lock-free fast path above).
                if let Some(kc) = keychain {
                    match kc.load() {
                        Ok(Some(seed)) => return Ok(seed),
                        Ok(None) => {}
                        Err(e) => tracing::warn!(
                            "keychain re-probe under identity write-lock failed ({e}); \
                             proceeding to generate"
                        ),
                    }
                }
                if let Some(enc) = encrypted {
                    match enc.load() {
                        Ok(Some(seed)) => return Ok(seed),
                        Ok(None) => {}
                        Err(e) => return Err(e),
                    }
                }
                generate()
            })
        }
        // No writable destination (a doomed boot: no keychain AND no encrypted
        // store) — or a hermetic test (`None` lock target). There is no write to
        // race, so skip the guard: `generate()` fails fast with the no-store
        // error WITHOUT creating a lockfile / identity dir on a boot that was
        // going to fail anyway (tests write to their isolated store directly).
        _ => generate(),
    }
}

/// After an encrypted-file force-restore, reconcile a stale keychain vault so it
/// can't shadow the restore on next boot (the resolution chain prefers the
/// keychain), without gratuitously discarding app-local keys.
///
/// The consolidated vault also holds the iroh / device / owner-master keys,
/// which pre-ZEB-363 lived in separate keychain items untouched by a seed write.
/// Blindly deleting the whole vault here would regress that (new iroh
/// `EndpointId`; owner-state vs `owner_state.cbor` inconsistency). So we rewrite
/// only the seed (RMW-preserve app-local) when the keychain is readable, and
/// fall back to deleting the stale entry only when it isn't — at which point the
/// app-local keys regenerate on next boot, per the seed-only recovery model.
fn reconcile_keychain_after_enc_restore(kc: &KeychainStore, seed: &[u8; BLOB_LEN]) {
    let _vault_guard = vault_rmw_guard();
    match kc.load_vault() {
        Ok(Some(mut vault)) => {
            vault.version = VAULT_VERSION;
            vault.seed = *seed;
            match kc.save_vault(&vault) {
                Ok(()) => tracing::info!(
                    "reconciled keychain vault seed after encrypted-file force-restore (app-local keys preserved)"
                ),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "could not reconcile keychain seed after encrypted-file force-restore; deleting stale entry so it cannot shadow the restore (app-local keys will regenerate)"
                    );
                    delete_stale_keychain_after_restore(kc);
                }
            }
        }
        Ok(None) => { /* no stale keychain entry to reconcile */ }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "could not read keychain while reconciling after encrypted-file force-restore; deleting stale entry defensively so it cannot shadow the restore"
            );
            delete_stale_keychain_after_restore(kc);
        }
    }
}

/// Best-effort delete of a stale keychain entry (last resort when it can't be
/// reconciled). NoEntry is silent; other errors warn but do not fail the restore.
fn delete_stale_keychain_after_restore(kc: &KeychainStore) {
    match kc.delete() {
        Ok(()) => {
            tracing::info!("removed stale keychain entry after encrypted-file force-restore")
        }
        Err(keyring::Error::NoEntry) => { /* nothing to clean */ }
        Err(e) => tracing::warn!(
            error = %e,
            "could not remove stale keychain entry after encrypted-file force-restore — manual cleanup may be needed"
        ),
    }
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
) -> Result<SaveDestination, String> {
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
                return Ok(SaveDestination::Keychain);
            }
            Err(e) => {
                tracing::warn!("keychain save failed: {e}; trying encrypted fallback if available");
                keychain_err = Some(e);
            }
        }
    }

    if let Some(enc) = encrypted {
        enc.save(seed)?;
        verify_round_trip(enc, seed)?;
        tracing::info!(path = %enc.path().display(), "identity stored in encrypted file");
        Ok(SaveDestination::EncryptedFile)
    } else if let Some(e) = keychain_err {
        Err(keychain_failed_no_enc_err(&e))
    } else {
        Err(no_dest_err())
    }
}

/// Public entry point — resolves env-derived encrypted store, attempts the
/// keychain, and runs the resolution chain. Returns the derived `NodeIdentity`.
pub fn load_or_generate(identity_path: &Path) -> Result<NodeIdentity, String> {
    let seed = read_seed_from_disk_with_keychain(identity_path, KeychainStore::new().ok())?;
    Ok(NodeIdentity::from_seed(&seed))
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn load_or_generate_with_keychain(
    identity_path: &Path,
    keychain: Option<KeychainStore>,
) -> Result<NodeIdentity, String> {
    let seed = read_seed_from_disk_with_keychain(identity_path, keychain)?;
    Ok(NodeIdentity::from_seed(&seed))
}

/// Read the master seed from disk via the standard resolution chain
/// (keychain → encrypted file → fresh-generate). Returns the seed bytes
/// directly so the recovery CLI can encode them without first deriving a
/// `NodeIdentity`.
pub fn read_seed_from_disk(identity_path: &Path) -> Result<Zeroizing<[u8; BLOB_LEN]>, String> {
    read_seed_from_disk_with_keychain(identity_path, KeychainStore::new().ok())
}

/// Inner entry point. Integration tests (across the crate boundary) inject a
/// deterministic keychain. `pub` rather than `pub(crate)` so
/// `tests/recovery_cli_integration.rs` can reach it.
pub fn read_seed_from_disk_with_keychain(
    identity_path: &Path,
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

    let enc_path = identity_path.with_file_name("identity.enc");
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

    // Use _post_probe variant: we already probed the keychain above (single
    // round-trip). The non-post-probe variant probes again, which is a real
    // perf regression on macOS Keychain / Linux Secret Service.
    //
    // `Some(identity_path)`: the production boot path. When no identity exists
    // yet, the fresh-generate is serialized (and double-checked) against a
    // concurrent cross-process `restore` / `rotate` on the same `~/.harmony`
    // via `identity.enc.lock` (ZEB-735). An existing identity returns on the
    // lock-free fast path inside `_post_probe`, so the common boot never locks.
    load_or_generate_with_stores_post_probe(
        Some(identity_path),
        keychain.as_ref(),
        keychain_probe_ok,
        encrypted.as_ref(),
    )
}

/// Write the master seed to disk via the standard resolution chain
/// (keychain preferred, encrypted-file fallback). Refuses if a destination
/// already exists unless `force` is true; with `force = true`, overwrites
/// in place via the existing atomic `create_new` tmp-then-rename pattern in
/// `save_with_fallback`.
pub fn write_seed_to_disk(
    identity_path: &Path,
    seed: &[u8; BLOB_LEN],
    force: bool,
) -> Result<(), String> {
    write_seed_to_disk_with_keychain(identity_path, seed, force, KeychainStore::new().ok())
}

/// Process-wide mutex serializing **all writers of the at-rest encrypted-file
/// identity material** — [`write_seed_to_disk_with_keychain`],
/// [`rotate_passphrase`], first-boot generation (via
/// [`read_seed_from_disk_with_keychain`]), and the secret-writing body of
/// `owner_state::save_owner_state_atomic`.
///
/// Sibling of `owner_commands::OWNER_STATE_WRITE_LOCK` (ZEB-199), which guards
/// `owner_state.cbor`. The two protect distinct invariants — that lock: "one
/// writer of OwnerState at a time"; this: "one writer of the encrypted-file
/// fallback at a time". Rotate/restore write the encrypted key file but NOT
/// OwnerState, so folding them into the former would over-serialize unrelated
/// work (ZEB-201).
///
/// Why it's needed (ZEB-201): in `HARMONY_PASSPHRASE`-only mode (no OS
/// keychain), [`rotate_passphrase`] does a load→re-encrypt→atomic-write
/// read-modify-write on `identity.enc`, and a concurrent `write_seed_*`
/// (recovery-file / mnemonic restore) writes the same file. `EncryptedFileStore`
/// uses atomic rename so the file never tears, but without serialization one
/// writer's update is silently lost — the "loser" overwrites the "winner"
/// mid-RMW. Holding this across each writer's whole critical section closes
/// that window. This is the intra-process half; the cross-process file+keychain
/// TOCTOU on the `!force` probe is closed by the `fd_lock` on `identity.enc.lock`
/// that [`with_identity_write_guards`] layers *inside* this mutex for the
/// restore, rotate, and first-boot-generate writers (ZEB-179 / ZEB-735).
///
/// **Lock ordering (deadlock-free by construction):** whenever both locks are
/// held, `OWNER_STATE_WRITE_LOCK` is the OUTER lock and this is the INNER.
/// `save_owner_state_atomic` (called under `OWNER_STATE_WRITE_LOCK` by mint /
/// pairing-persist / mnemonic-restore) acquires this internally; whereas
/// [`with_identity_write_guards`] (the restore / rotate / first-boot-generate
/// writers) acquires this and then the `fd_lock` — never
/// `OWNER_STATE_WRITE_LOCK` — so the OwnerState↔identity order can never invert.
/// None of these writers calls another while holding the lock, so this
/// non-reentrant `Mutex` is never re-acquired on one thread. Recover from
/// poisoning so a panic in one writer doesn't brick future ones (mirrors
/// `OWNER_STATE_WRITE_LOCK`).
pub(crate) static IDENTITY_FILE_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire both identity-write guards and run `critical_section` while they are
/// held, releasing them on return. This is the single acquisition point shared
/// by every writer of the at-rest identity material —
/// [`write_seed_to_disk_with_keychain`] (restore / mint), [`rotate_passphrase`],
/// and first-boot generation (via [`read_seed_from_disk_with_keychain`]) — so
/// all three flock the *same* `identity.enc.lock` in the *same* order and
/// therefore mutually exclude across processes without any risk of deadlock
/// (ZEB-179 restore-path fix, extended to generate + rotate by ZEB-735).
///
/// - **OUTER — in-process [`IDENTITY_FILE_WRITE_LOCK`]:** serializes against
///   every other encrypted-file writer within this process (ZEB-201). Acquired
///   first, it guarantees exactly one thread per process ever reaches the
///   `fd_lock` — `flock` / `LockFileEx` are per-open-file-description and would
///   self-conflict between two handles in one process otherwise, so this
///   ordering is what makes the cross-process lock intra-process-safe.
/// - **INNER — cross-process `fd_lock` on `identity.enc.lock`:** closes the
///   check-then-act TOCTOU between the caller's existence probe and its write.
///   Non-blocking (`try_write`): a loser fails fast with a clear "another
///   process is writing" error instead of hanging — essential on the
///   boot-generate path, where blocking could stall node startup. `fd_lock`
///   releases the lock on process death, so a crashed peer never strands it (no
///   PID-liveness / stale-lock reclaim needed).
///
/// The lock path is derived as the `identity.enc.lock` sibling of
/// `identity_path`, matching `enc_path`; callers on the rotate path pass
/// `old.path()` (the `identity.enc` file), whose sibling is the same lockfile.
fn with_identity_write_guards<T>(
    identity_path: &Path,
    critical_section: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    // OUTER: in-process serialization — see fn docs. Recover from poisoning so a
    // panic in one writer doesn't brick future ones (mirrors OWNER_STATE lock).
    let _identity_file_guard = IDENTITY_FILE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());

    // INNER: cross-process advisory lock spanning the caller's critical section.
    let lock_path = identity_path.with_file_name("identity.enc.lock");
    // Filter empty parents (a bare relative `identity_path` yields `Some("")`,
    // and `create_dir_all("")` errors) — same guard as `write_atomic_0600`.
    // Harden the identity directory to 0o700 the same way `write_atomic_0600`
    // does: on first-boot generation this guard can be the FIRST code path to
    // create it, so it must not leave it umask-derived.
    if let Some(parent) = lock_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Best-effort: ignore failures (dir may pre-exist / be other-owned).
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    // Create the lockfile 0o600 on Unix (mirrors `write_atomic_0600`). It holds
    // no secrets, but a world-readable lockfile in the identity dir is needless
    // metadata exposure and undercuts the dir's 0o700 intent. `mode()` applies
    // only when the file is created; a pre-existing lockfile is left as-is.
    #[cfg(unix)]
    let lock_file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|e| format!("open identity write-lock {}: {e}", lock_path.display()))?
    };
    #[cfg(not(unix))]
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("open identity write-lock {}: {e}", lock_path.display()))?;
    let mut cross_process_lock = fd_lock::RwLock::new(lock_file);
    // Only genuine contention (`WouldBlock`) means "another writer holds it";
    // other I/O errors (e.g. `Interrupted` from a signal) must surface as real
    // failures, not be masked as a retryable contention message.
    let _cross_process_guard = cross_process_lock.try_write().map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            "another harmony-app process is writing the identity store; retry once it finishes"
                .to_string()
        } else {
            format!(
                "could not acquire identity write-lock {}: {e}",
                lock_path.display()
            )
        }
    })?;

    critical_section()
}

/// **Cross-process + in-process write serialization.** Runs the entire
/// probe-and-write below inside [`with_identity_write_guards`], so the
/// check-then-act between the `!force` existence probes (keychain `load()`,
/// `enc_path.exists()`) and `save_with_fallback` is atomic against every other
/// identity writer — within this process (the in-process mutex) and across
/// processes (the `fd_lock` on `identity.enc.lock`). The realistic race this
/// closes is an operator accidentally running two `harmony-app restore` / `mint`
/// invocations against the same `~/.harmony`: the loser fails fast with "another
/// process is writing" and, on retry, simply observes the now-existing file (the
/// ordinary "already exists" refusal). See [`with_identity_write_guards`] for
/// the lock-ordering and non-blocking-acquire rationale.
///
/// Residual, documented and accepted: the keychain side has no atomic
/// add-if-absent (`keyring::set_password` is an unconditional overwrite), so
/// two *force* writes to the keychain remain last-writer-wins. The guards still
/// serialize them (both are taken before touching the keychain), so the outcome
/// is a clean last-writer-wins of an operator-supplied identity — never an
/// interleaved half-state, and AEAD authentication of the file is unaffected.
/// Closing the keychain race outright needs an upstream `keyring` change; out of
/// scope per ZEB-179.
pub fn write_seed_to_disk_with_keychain(
    identity_path: &Path,
    seed: &[u8; BLOB_LEN],
    force: bool,
    keychain: Option<KeychainStore>,
) -> Result<(), String> {
    // Hold both the in-process and cross-process identity-write guards across
    // the whole probe-and-write. Shared with `rotate_passphrase` and the
    // first-boot generate path so all three mutually exclude (ZEB-179 / ZEB-735).
    with_identity_write_guards(identity_path, move || {
        write_seed_probe_and_write(identity_path, seed, force, keychain)
    })
}

/// The probe-and-write body of [`write_seed_to_disk_with_keychain`], run inside
/// [`with_identity_write_guards`]. Split out so the guard acquisition is shared
/// verbatim with the other identity writers; never call this without holding the
/// guards (it performs the check-then-act the guards make atomic).
fn write_seed_probe_and_write(
    identity_path: &Path,
    seed: &[u8; BLOB_LEN],
    force: bool,
    keychain: Option<KeychainStore>,
) -> Result<(), String> {
    let enc_path = identity_path.with_file_name("identity.enc");
    let mut keychain_healthy = false;

    if !force {
        // Refuse if either destination has an existing identity.
        // Check the keychain first (cheap probe), then the encrypted file path.
        // The probe also doubles as a connectivity check — Ok(None) means the
        // backend is responsive AND there's no existing entry, so we can mark
        // it healthy and skip the second probe below.
        if let Some(kc) = &keychain {
            match kc.load() {
                Ok(Some(_)) => {
                    return Err(
                        "identity already exists in OS keychain; pass --force to overwrite (this is destructive)"
                            .to_string(),
                    );
                }
                Ok(None) => keychain_healthy = true,
                Err(e) => {
                    return Err(format!(
                        "could not determine whether an identity already exists in OS keychain — refusing to overwrite: {e}; pass --force to override"
                    ));
                }
            }
        }
        if enc_path.exists() {
            return Err(format!(
                "identity already exists at {}; pass --force to overwrite (this is destructive)",
                enc_path.display()
            ));
        }
    } else if let Some(kc) = &keychain {
        // Force path: still need a connectivity probe to compute keychain_healthy.
        if kc.load().is_ok() {
            keychain_healthy = true;
        }
    }

    let encrypted = match EncryptedFileStore::from_env(enc_path.clone()) {
        Ok(opt) => opt,
        Err(e) if keychain_healthy => {
            tracing::warn!(
                "HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE configured but invalid \
                 ({e}); ignoring — keychain is available as fallback"
            );
            None
        }
        Err(e) => return Err(e),
    };

    let destination = save_with_fallback(
        keychain_healthy,
        keychain.as_ref(),
        encrypted.as_ref(),
        seed,
        || {
            "no identity store available: keychain unavailable and HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set — see docs/headless-install.md".to_string()
        },
        |e| {
            format!(
            "keychain save failed and no encrypted fallback configured: {e} — see docs/headless-install.md"
        )
        },
    )?;

    // After save_with_fallback succeeds, with force=true: best-effort unlink
    // the non-destination so the user can't be stranded on a stale backup
    // after a future keychain clear or env-var change. Drive cleanup from the
    // actual save destination, not from the pre-save probe, to avoid the
    // data-loss bug where probe-ok + save-fail → fallback writes encrypted file
    // → cleanup wrongly deletes the freshly-written .enc.
    if force {
        match destination {
            SaveDestination::Keychain => {
                // Wrote to keychain → unlink the encrypted file if it exists.
                // A leftover .enc with the pre-restore seed is the silent-failure
                // scenario this guards against.
                match std::fs::remove_file(&enc_path) {
                    Ok(()) => tracing::info!(
                        path = %enc_path.display(),
                        "removed stale encrypted-file backend after keychain force-restore"
                    ),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // Nothing to clean up.
                    }
                    Err(e) => tracing::warn!(
                        path = %enc_path.display(),
                        error = %e,
                        "could not remove stale encrypted-file backend after keychain force-restore — manual cleanup may be needed"
                    ),
                }
            }
            SaveDestination::EncryptedFile => {
                // Wrote the restored seed to the encrypted file → reconcile any
                // stale keychain vault so it can't shadow the restore on next
                // boot, WITHOUT gratuitously dropping app-local keys.
                if let Some(kc) = &keychain {
                    reconcile_keychain_after_enc_restore(kc, seed);
                }
            }
        }
    }

    Ok(())
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
    // Hold both the in-process and cross-process identity-write guards across
    // the whole load→re-encrypt→atomic-write RMW, so no concurrent `write_seed_*`
    // / `save_owner_state_atomic` / boot-generate writer overwrites this one
    // mid-RMW — within this process AND across processes (ZEB-201 / ZEB-179 /
    // ZEB-735). The lockfile is the `identity.enc.lock` sibling of `old.path()`,
    // the same one every other identity writer takes.
    with_identity_write_guards(old.path(), move || {
        // ZEB-363: rotate the WHOLE vault (preserving app-local keys), not just
        // the seed. The new store has a different passphrase, so a seed-level RMW
        // save would try to read the old-passphrase file with the new passphrase
        // and fail — load the full vault with the old passphrase and re-encrypt
        // it under the new one.
        let vault = old.load_vault()?.ok_or_else(|| {
            format!(
                "no encrypted identity to rotate at {}",
                old.path().display()
            )
        })?;
        let seed = Zeroizing::new(vault.seed);

        let new_store = EncryptedFileStore::new(old.path().to_path_buf(), new_passphrase);
        new_store.save_vault(&vault)?;
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
    })
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

    #[cfg(any(test, feature = "test-fixtures"))]
    pub use super::encrypt_vault_with_params as encrypt_vault_with_params_for_test;

    // `decrypt_vault_bytes` is `pub(crate)`, so integration tests can't reach it
    // directly and a `pub use` of it fails E0364 (a re-export can't be more
    // public than the item). Expose it through a gated wrapper for the v0x02
    // round-trip assertion — this leaves `decrypt_vault_bytes`'s own
    // `pub(crate)` visibility unchanged in production builds.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn decrypt_vault_bytes_for_test(
        passphrase: &[u8],
        bytes: &[u8],
    ) -> Result<super::Zeroizing<Vec<u8>>, String> {
        super::decrypt_vault_bytes(passphrase, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn identity_path_in_maps_default_and_named_profiles() {
        use std::path::Path;
        let home = Path::new("/home/u");
        assert_eq!(
            identity_path_in(home, None),
            Path::new("/home/u/.harmony/identity.key")
        );
        assert_eq!(
            identity_path_in(home, Some("coord")),
            Path::new("/home/u/.harmony/profiles/coord/identity.key")
        );
    }

    /// Pins the ZEB-446 constructor-gate condition itself, so it must call
    /// the real constructor — the same pattern as tests/keychain_isolation.rs
    /// (the canonical ZEB-428 gate-pinning test). No host-keychain access is
    /// possible: the named-profile refusal returns before keyring::Entry is
    /// ever constructed. nextest (the supported runner, CLAUDE.md) is
    /// process-per-test, so the OnceLock set here cannot leak.
    #[test]
    fn keychain_constructor_refuses_on_named_profile() {
        crate::profile::set_active_profile(Some("gatetest")).expect("activate");
        let err = match KeychainStore::new() {
            Err(e) => e,
            Ok(_) => panic!("named profile must refuse the OS keychain"),
        };
        assert!(
            err.contains("ZEB-446"),
            "named-profile refusal must cite ZEB-446 (got the test-build gate instead?): {err}"
        );
    }

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

    // ── ZEB-428: test builds must never open the real OS keychain ──────
    // A full `--all-targets --features test-fixtures` sweep on Ildwyn's
    // Windows machine silently overwrote the developer's real owner
    // identity in CredMan (tests/mint_owner_lifecycle.rs persisted through
    // `KeychainStore::new()` — the HOME-to-tempdir redirect does not scope
    // the OS keychain). These tests pin the constructor-level gate that
    // makes that class of clobber impossible by construction.

    #[test]
    #[serial]
    fn keychain_new_is_disabled_in_test_builds() {
        std::env::remove_var("HARMONY_ALLOW_REAL_KEYCHAIN");
        std::env::remove_var("HARMONY_DISABLE_KEYCHAIN");
        let err = match KeychainStore::new() {
            Ok(_) => panic!(
                "test builds (cfg(test) / test-fixtures) must refuse to open the real OS keychain"
            ),
            Err(e) => e,
        };
        assert!(
            err.contains("ZEB-428"),
            "gate error should reference the incident ticket for discoverability: {err}"
        );
    }

    /// True when `err` came from one of the ZEB-428 gates (as opposed to a
    /// platform backend failure — e.g. no Secret Service on headless Linux).
    /// The allow-override tests pin GATE behavior only: a backend error on a
    /// keychain-less system is legitimate and must not fail the suite (Qodo
    /// PR #227 R1).
    fn is_zeb428_gate_error(err: &str) -> bool {
        err.contains("ZEB-428") || err.contains("HARMONY_DISABLE_KEYCHAIN")
    }

    #[test]
    #[serial]
    fn keychain_new_escape_hatch_overrides_test_gate() {
        std::env::set_var("HARMONY_ALLOW_REAL_KEYCHAIN", "1");
        std::env::remove_var("HARMONY_DISABLE_KEYCHAIN");
        // With the explicit override the constructor must get PAST the gates
        // to the real keyring entry. On a keychain-less system (headless
        // Linux/CI) entry creation may then fail for backend reasons — that
        // is acceptable; what must NOT appear is a gate refusal. Entry
        // creation never touches the credential itself (no read/write
        // happens until load/save), so this is safe on a dev machine.
        let result = KeychainStore::new();
        std::env::remove_var("HARMONY_ALLOW_REAL_KEYCHAIN");
        if let Err(e) = result {
            assert!(
                !is_zeb428_gate_error(&e),
                "HARMONY_ALLOW_REAL_KEYCHAIN=1 must bypass the ZEB-428 gates, got gate error: {e}"
            );
        }
    }

    #[test]
    #[serial]
    fn keychain_new_disable_env_wins_over_escape_hatch() {
        std::env::set_var("HARMONY_DISABLE_KEYCHAIN", "1");
        std::env::set_var("HARMONY_ALLOW_REAL_KEYCHAIN", "1");
        let result = KeychainStore::new();
        std::env::remove_var("HARMONY_DISABLE_KEYCHAIN");
        std::env::remove_var("HARMONY_ALLOW_REAL_KEYCHAIN");
        let err = match result {
            Ok(_) => panic!("explicit disable must beat the allow override"),
            Err(e) => e,
        };
        assert!(err.contains("HARMONY_DISABLE_KEYCHAIN"), "{err}");
    }

    #[test]
    #[serial]
    fn keychain_new_disable_env_zero_or_empty_means_off() {
        std::env::set_var("HARMONY_ALLOW_REAL_KEYCHAIN", "1");
        for benign in ["0", ""] {
            std::env::set_var("HARMONY_DISABLE_KEYCHAIN", benign);
            // Same backend-agnostic posture as the escape-hatch test: a
            // benign disable value must not trigger either gate; a backend
            // error on a keychain-less system is acceptable.
            if let Err(e) = KeychainStore::new() {
                assert!(
                    !is_zeb428_gate_error(&e),
                    "HARMONY_DISABLE_KEYCHAIN={benign:?} must not trigger a gate, got: {e}"
                );
            }
        }
        std::env::remove_var("HARMONY_DISABLE_KEYCHAIN");
        std::env::remove_var("HARMONY_ALLOW_REAL_KEYCHAIN");
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

    #[test]
    #[serial]
    fn read_seed_round_trips_via_encrypted_file() {
        use secrecy::SecretString;
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let enc_path = dir.path().join("identity.enc");

        // Set up an encrypted store with a known passphrase, write a known seed.
        std::env::set_var("HARMONY_PASSPHRASE", "round-trip-test");
        let store = EncryptedFileStore::new(
            enc_path.clone(),
            SecretString::from("round-trip-test".to_string()),
        );
        let written = [0xCDu8; 32];
        store.save(&written).expect("save");

        // Read it back through the public seed-shaped helper.
        let loaded = read_seed_from_disk_with_keychain(&identity_path, None).expect("read");
        assert_eq!(
            *loaded, written,
            "seed must round-trip through the encrypted store"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn write_seed_refuses_when_identity_exists_without_force() {
        use secrecy::SecretString;
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let enc_path = dir.path().join("identity.enc");

        std::env::set_var("HARMONY_PASSPHRASE", "refuse-test");
        let existing_seed = [0x11u8; 32];
        let store = EncryptedFileStore::new(
            enc_path.clone(),
            SecretString::from("refuse-test".to_string()),
        );
        store.save(&existing_seed).unwrap();

        let new_seed = [0x22u8; 32];
        // Pass `None` for the keychain to keep this test hermetic — without it,
        // write_seed_to_disk would resolve `KeychainStore::new().ok()`, which on
        // a developer machine reads/writes the real `harmony/identity` keychain
        // entry and prompts for keychain access.
        let err = write_seed_to_disk_with_keychain(
            &identity_path,
            &new_seed,
            /*force=*/ false,
            None,
        )
        .expect_err("must refuse when destination exists");
        assert!(err.contains("identity already exists"), "actual: {err}");
        assert!(err.contains("--force"), "actual: {err}");

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn write_seed_with_force_overwrites_existing() {
        use secrecy::SecretString;
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let enc_path = dir.path().join("identity.enc");

        std::env::set_var("HARMONY_PASSPHRASE", "force-test");
        let existing_seed = [0x33u8; 32];
        let store = EncryptedFileStore::new(
            enc_path.clone(),
            SecretString::from("force-test".to_string()),
        );
        store.save(&existing_seed).unwrap();

        let new_seed = [0x44u8; 32];
        // Pass `None` so the test stays file-only and never touches the real OS
        // keychain (see hermeticity comment on the sibling test above).
        write_seed_to_disk_with_keychain(&identity_path, &new_seed, /*force=*/ true, None)
            .expect("force must succeed");

        let reloaded = read_seed_from_disk_with_keychain(&identity_path, None).expect("reload");
        assert_eq!(
            *reloaded, new_seed,
            "after force-overwrite, the new seed must be present"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn identity_file_write_lock_serializes_writers() {
        // ZEB-201: holding IDENTITY_FILE_WRITE_LOCK must block a concurrent
        // encrypted-file writer until released. Deterministic: while we hold the
        // lock the spawned writer blocks on lock-acquire and can NEVER complete,
        // so "not finished within 200 ms" holds regardless of machine speed; a
        // broken lock would let the tiny write finish in ~ms and fail the
        // negative assertion. (nextest runs each test in its own process, so
        // this process-global lock is uncontended by other tests.)
        use std::sync::mpsc;
        use std::time::Duration;

        std::env::set_var("HARMONY_PASSPHRASE", "zeb201-mutex-test");
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");

        let guard = IDENTITY_FILE_WRITE_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let (tx_started, rx_started) = mpsc::channel::<()>();
        let (tx_done, rx_done) = mpsc::channel::<Result<(), String>>();
        let ipath = identity_path.clone();
        let handle = std::thread::spawn(move || {
            // Signal we're about to attempt the write (and thus the lock).
            tx_started.send(()).unwrap();
            let r =
                write_seed_to_disk_with_keychain(&ipath, &[0x55u8; 32], /*force=*/ true, None);
            let _ = tx_done.send(r);
        });

        // The writer thread is alive and (about to be) blocked on lock-acquire.
        rx_started
            .recv_timeout(Duration::from_secs(30))
            .expect("writer thread must start");
        // It MUST NOT complete while we hold the lock. The window has to clear
        // the real work-time of an *unblocked* writer so a silently-dropped lock
        // is actually caught: this write runs a real Argon2id KDF (m=64MiB, t=3)
        // that is ~100-300ms, so a 200ms window could let a broken lock's write
        // still miss the deadline and pass by coincidence (CodeRabbit). 1s sits
        // well above the KDF cost yet far under the completion budget below; a
        // correctly-held lock keeps the writer blocked the whole window
        // regardless of machine load, so this stays robust in both directions.
        assert!(
            rx_done.recv_timeout(Duration::from_millis(1000)).is_err(),
            "a concurrent encrypted-file writer must block while IDENTITY_FILE_WRITE_LOCK is held"
        );

        // Release the lock; the writer now proceeds and completes. Generous
        // budget: this only converts a true (infinite) hang into a clean
        // failure, and the write does a real Argon2id KDF that is seconds-slow
        // and highly variable under loaded CI — so the bound must be >> that
        // legit work-time, not a tight perf assertion.
        drop(guard);
        let result = rx_done
            .recv_timeout(Duration::from_secs(60))
            .expect("writer must finish once the lock is released");
        assert!(
            result.is_ok(),
            "unblocked writer must succeed; got {result:?}"
        );
        handle.join().unwrap();

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn write_seed_refuses_while_another_process_holds_the_write_lock() {
        // ZEB-179: the cross-process fd-lock must make a second writer fail fast
        // rather than race the probe-and-write. We simulate a "concurrent
        // process" by holding an fd-lock write guard on the same
        // `identity.enc.lock`: flock is per-open-file-description, so a second
        // handle in THIS process conflicts exactly as another process would
        // (this is precisely what `api::lock`'s own test relies on). Fully
        // deterministic — no spawned binaries.
        std::env::set_var("HARMONY_PASSPHRASE", "zeb179-fdlock-test");
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let enc_path = identity_path.with_file_name("identity.enc");
        let lock_path = identity_path.with_file_name("identity.enc.lock");

        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        let mut external = fd_lock::RwLock::new(lock_file);
        let held = external
            .try_write()
            .expect("external holder acquires the write lock");

        // While the peer holds the lock, write_seed must refuse fast (non-blocking).
        let err = write_seed_to_disk_with_keychain(
            &identity_path,
            &[0x99u8; 32],
            /*force=*/ false,
            None,
        )
        .expect_err("must refuse while another writer holds the cross-process lock");
        assert!(
            err.contains("another harmony-app process is writing"),
            "actual: {err}"
        );
        // The probe-and-write never ran, so nothing was written.
        assert!(
            !enc_path.exists(),
            "no identity.enc may be created while the write is blocked"
        );

        // Release the simulated peer; the write now succeeds and round-trips.
        drop(held);
        write_seed_to_disk_with_keychain(
            &identity_path,
            &[0x99u8; 32],
            /*force=*/ false,
            None,
        )
        .expect("write must succeed once the cross-process lock is free");
        let reloaded = read_seed_from_disk_with_keychain(&identity_path, None).expect("reload");
        assert_eq!(
            *reloaded, [0x99u8; 32],
            "the seed must round-trip after the lock frees"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn generate_refuses_while_another_process_holds_the_write_lock() {
        // ZEB-735: first-boot identity generation now takes the same
        // cross-process fd-lock as restore. With a peer holding
        // `identity.enc.lock`, the boot generate must fail fast rather than
        // write a fresh (throwaway) identity that could clobber the peer's
        // concurrent restore. Deterministic — no spawned binaries (see the
        // write_seed variant above for why a second in-process fd-lock handle
        // conflicts exactly as another process would).
        std::env::set_var("HARMONY_PASSPHRASE", "zeb735-generate-fdlock");
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let enc_path = identity_path.with_file_name("identity.enc");
        let lock_path = identity_path.with_file_name("identity.enc.lock");

        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        let mut external = fd_lock::RwLock::new(lock_file);
        let held = external
            .try_write()
            .expect("external holder acquires the write lock");

        // No identity exists yet -> read_seed hits the generate path -> refuse.
        let err = read_seed_from_disk_with_keychain(&identity_path, None)
            .expect_err("boot generate must refuse while another writer holds the lock");
        assert!(
            err.contains("another harmony-app process is writing"),
            "actual: {err}"
        );
        assert!(
            !enc_path.exists(),
            "no identity.enc may be generated while the write is blocked"
        );

        // Release the peer; generation now succeeds and persists a stable
        // identity that reloads identically.
        drop(held);
        let first = read_seed_from_disk_with_keychain(&identity_path, None)
            .expect("generate must succeed once the lock frees");
        assert!(enc_path.exists(), "generate must persist identity.enc");
        let second = read_seed_from_disk_with_keychain(&identity_path, None)
            .expect("reload existing identity");
        assert_eq!(
            *first, *second,
            "the generated seed must persist and reload identically"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn boot_generate_completes_and_reread_is_lock_free() {
        // ZEB-735 boot-deadlock regression: an UNCONTENDED first-boot generate
        // must complete (not hang) under the new guard, and every subsequent
        // boot must take the lock-free read fast path. If the guard could
        // deadlock the boot path this test would HANG (nextest timeout) rather
        // than fail — a completing run is itself the no-deadlock assertion.
        std::env::set_var("HARMONY_PASSPHRASE", "zeb735-boot-nodeadlock");
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let lock_path = identity_path.with_file_name("identity.enc.lock");

        // First boot: generates + persists. Completing at all proves no deadlock.
        let generated = read_seed_from_disk_with_keychain(&identity_path, None)
            .expect("first-boot generate must complete");
        assert!(
            lock_path.exists(),
            "the generate path must have taken identity.enc.lock"
        );

        // Delete the lockfile, then re-read: an existing-identity boot returns
        // on the lock-free fast path and must NOT re-create the lockfile.
        std::fs::remove_file(&lock_path).unwrap();
        let reread = read_seed_from_disk_with_keychain(&identity_path, None)
            .expect("second-boot read must complete");
        assert_eq!(
            *generated, *reread,
            "an existing identity must reload identically"
        );
        assert!(
            !lock_path.exists(),
            "an existing-identity read must not take the write lock"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn boot_generate_with_no_store_fails_without_creating_lockfile() {
        // ZEB-735: a doomed boot (no keychain AND no HARMONY_PASSPHRASE) has no
        // writable destination, so no write can race — the guard is skipped and
        // the boot fails fast with the no-store error WITHOUT creating the
        // identity dir or an empty lockfile (restores the pre-change behavior).
        std::env::remove_var("HARMONY_PASSPHRASE");
        std::env::remove_var("HARMONY_PASSPHRASE_FILE");
        let dir = tempfile::tempdir().unwrap();
        // Nested path so the parent dir does not pre-exist: if the guard were
        // taken it would `create_dir_all` this and drop a lockfile in it.
        let identity_path = dir.path().join("sub").join("identity.key");
        let lock_path = identity_path.with_file_name("identity.enc.lock");

        let err = read_seed_from_disk_with_keychain(&identity_path, None)
            .expect_err("a boot with no identity store must fail");
        assert!(err.contains("no identity store available"), "actual: {err}");
        assert!(
            !lock_path.exists(),
            "a doomed no-store boot must not create identity.enc.lock"
        );
        assert!(
            !identity_path.parent().unwrap().exists(),
            "a doomed no-store boot must not create the identity dir"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn boot_generate_hardens_lockfile_and_dir_permissions() {
        // ZEB-735: on first-boot generation the write guard can be the first
        // code path to create the identity dir + lockfile, so it must harden
        // them like `write_atomic_0600` (dir 0o700, file 0o600) rather than
        // leave them umask-derived. The lockfile assertion isolates the guard's
        // own hardening (write_atomic_0600 never touches the lockfile).
        use std::os::unix::fs::PermissionsExt;
        std::env::set_var("HARMONY_PASSPHRASE", "zeb735-perms");
        let dir = tempfile::tempdir().unwrap();
        // Nested parent so the guard is what creates (and must harden) it.
        let identity_path = dir.path().join("nested").join("identity.key");
        let lock_path = identity_path.with_file_name("identity.enc.lock");
        let parent = identity_path.parent().unwrap().to_path_buf();

        read_seed_from_disk_with_keychain(&identity_path, None).expect("first-boot generate");

        let dir_mode = std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            dir_mode, 0o700,
            "identity dir must be hardened to 0o700, got {dir_mode:#o}"
        );
        let lock_mode = std::fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            lock_mode, 0o600,
            "identity.enc.lock must be created 0o600, got {lock_mode:#o}"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn force_unlinks_stale_encrypted_file_after_keychain_write() {
        use secrecy::SecretString;

        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let enc_path = identity_path.with_file_name("identity.enc");

        // Seed both backends with different seeds. We use a healthy mock keychain
        // for the keychain path; the encrypted file is real on disk.
        std::env::set_var("HARMONY_PASSPHRASE", "force-cleanup-test");
        let kc = KeychainStore::new_mock();
        kc.save(&[0x11u8; 32]).unwrap();

        let store = EncryptedFileStore::new(
            enc_path.clone(),
            SecretString::from("force-cleanup-test".to_string()),
        );
        store.save(&[0x22u8; 32]).unwrap();
        assert!(enc_path.exists(), "test setup: enc file present");

        // Force-write a third seed. With keychain healthy, save_with_fallback
        // writes to keychain; the cleanup logic should unlink the stale .enc.
        write_seed_to_disk_with_keychain(&identity_path, &[0x33u8; 32], true, Some(kc))
            .expect("force write must succeed");
        assert!(
            !enc_path.exists(),
            "stale enc file must be unlinked after keychain force-restore"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
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
            let mut bytes =
                encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
            bytes[60] ^= 0x01; // flip one bit in the ciphertext range (53..85)
            let err = decrypt(TEST_PASSPHRASE, &bytes).unwrap_err();
            assert!(err.contains("wrong passphrase or corrupted file"));
        }

        #[test]
        fn tampered_kdf_params_fails() {
            let mut bytes =
                encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
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
            let mut bytes =
                encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
            bytes[0] = b'X'; // trash magic
            let err = decrypt(TEST_PASSPHRASE, &bytes).unwrap_err();
            assert!(err.contains("unrecognized format"), "got: {err}");
        }

        #[test]
        fn tampered_version_fails() {
            let mut bytes =
                encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
            bytes[4] = 0xFF; // unknown version
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
            assert_eq!(
                &bytes[6..10],
                &65536u32.to_be_bytes(),
                "kdf_m_kib (u32 BE) mismatch"
            );
            assert_eq!(
                &bytes[10..12],
                &3u16.to_be_bytes(),
                "kdf_t (u16 BE) mismatch"
            );
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
            assert!(
                err.contains("wrong passphrase or corrupted file"),
                "got: {err}"
            );
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
        fn file_is_v2_vault_envelope() {
            // ZEB-363: the encrypted file now carries a variable-length CBOR
            // SecretVault (HRMI v0x02), not the fixed 101-byte v1 seed envelope.
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");
            let store = EncryptedFileStore::new(path.clone(), fresh_passphrase());
            store.save(&fresh_seed()).unwrap();
            let raw = std::fs::read(&path).unwrap();
            assert_eq!(&raw[0..4], b"HRMI", "magic");
            assert_eq!(raw[4], 0x02, "must be the v0x02 vault envelope");
            // header(13) + salt(16) + nonce(24) + tag(16) = 69; a real vault
            // plaintext pushes it strictly larger.
            assert!(
                raw.len() > 69,
                "envelope must carry a vault plaintext, got {}",
                raw.len()
            );
        }

        #[test]
        fn truncated_file_load_fails() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");
            let store = EncryptedFileStore::new(path.clone(), fresh_passphrase());
            store.save(&fresh_seed()).unwrap();

            // Truncate to 70 bytes: above the 69-byte envelope floor but with a
            // mangled ciphertext, so the AEAD tag check fails (indistinguishable
            // wrong-passphrase/corruption error).
            let bytes = std::fs::read(&path).unwrap();
            std::fs::write(&path, &bytes[..70]).unwrap();

            let err = store.load().unwrap_err();
            assert!(
                err.contains("corrupt") || err.contains("wrong passphrase or corrupted"),
                "got: {err}"
            );
        }
    }

    mod env {
        use super::*;
        use secrecy::ExposeSecret;
        use serial_test::serial;

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
            let store = EncryptedFileStore::from_env(path)
                .unwrap()
                .expect("should be Some");
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
                std::fs::set_permissions(&pass_file, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
            }
            std::env::set_var(HARMONY_PASSPHRASE_FILE, &pass_file);

            let path = dir.path().join("identity.enc");
            let store = EncryptedFileStore::from_env(path)
                .unwrap()
                .expect("should be Some");
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
            let store = EncryptedFileStore::from_env(path)
                .unwrap()
                .expect("should be Some");
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
            let store = EncryptedFileStore::from_env(path)
                .unwrap()
                .expect("should be Some");
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
            std::fs::write(&pass_file, b"\n").unwrap(); // strips to empty
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

        /// Regression for PR #245 round 4 (Cursor Bugbot + Greptile): the
        /// ZEB-446 fail-fast gate and the vault consumer must agree on
        /// every env configuration. The old hand-rolled gate diverged on a
        /// whitespace-only HARMONY_PASSPHRASE (gate fell through to the
        /// FILE check; from_env used the whitespace as the passphrase) and
        /// on a file path with stray whitespace (gate trimmed it; from_env
        /// did not). Both now share resolve_passphrase_env, and this matrix
        /// pins the equivalence.
        #[test]
        #[serial]
        fn gate_agrees_with_from_env_on_every_configuration() {
            let dir = tempfile::tempdir().unwrap();
            let pass_file = dir.path().join("pass.txt");
            std::fs::write(&pass_file, b"from_file\n").unwrap();
            let good_path = pass_file.to_str().unwrap().to_string();
            let padded_path = format!("{good_path} "); // Greptile: trailing space

            let cases: &[(&str, Option<&str>, Option<&str>)] = &[
                ("nothing set", None, None),
                ("direct set", Some("foo"), None),
                ("direct empty", Some(""), None),
                ("direct empty, file good", Some(""), Some(&good_path)),
                // Cursor Bugbot round 4: direct whitespace must mean the
                // SAME thing to the gate as to the vault (it is a valid,
                // if odd, passphrase — from_env uses it verbatim).
                ("direct whitespace, file good", Some(" "), Some(&good_path)),
                ("file good", None, Some(&good_path)),
                ("file path padded", None, Some(&padded_path)),
                ("file missing", None, Some("/nonexistent/passphrase/file")),
            ];

            for (label, direct, file) in cases {
                clear_env();
                if let Some(v) = direct {
                    std::env::set_var(HARMONY_PASSPHRASE, v);
                }
                if let Some(v) = file {
                    std::env::set_var(HARMONY_PASSPHRASE_FILE, v);
                }
                let store_path = dir.path().join("identity.enc");
                let consumer_has_store =
                    matches!(EncryptedFileStore::from_env(store_path), Ok(Some(_)));
                assert_eq!(
                    passphrase_env_configured(),
                    consumer_has_store,
                    "gate/consumer divergence on case: {label}"
                );
            }
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

            let from_keychain = keychain
                .load()
                .unwrap()
                .expect("seed should be in keychain");
            assert_eq!(*from_keychain, *result);
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
            assert!(
                err.contains("docs/headless-install.md"),
                "should point at docs: {err}"
            );
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
            let wrong =
                EncryptedFileStore::new(enc_path.clone(), SecretString::from("WRONG".to_string()));
            let err = load_or_generate_with_stores(None, Some(&wrong)).unwrap_err();
            assert!(
                err.contains("wrong passphrase or corrupted file"),
                "got: {err}"
            );

            // Critically: original .enc must still be intact (not regenerated).
            let recovered = EncryptedFileStore::new(enc_path.clone(), fresh_passphrase())
                .load()
                .unwrap()
                .expect("original .enc must still be loadable with correct passphrase");
            assert_eq!(
                *recovered, original,
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

            let result = load_or_generate_with_stores(Some(&keychain), Some(&encrypted))
                .expect("keychain Err must fall through, not hard-fail");

            // Seed ended up in the encrypted store, not the keychain.
            assert!(
                enc_path.exists(),
                "encrypted file should be the destination"
            );
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

            let result = load_or_generate_with_stores(Some(&keychain), Some(&encrypted))
                .expect("must fall back to encrypted, not hard-fail");

            assert!(
                enc_path.exists(),
                "encrypted file should be the destination"
            );
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
            struct CorruptingStore {
                inner: KeychainStore,
            }
            impl KeyStore for CorruptingStore {
                fn load_vault(&self) -> Result<Option<SecretVault>, String> {
                    self.inner.load_vault()
                }
                fn save_vault(&self, vault: &SecretVault) -> Result<(), String> {
                    self.inner.save_vault(vault)
                }
                // Override the seed-level load to always return a different seed,
                // forcing verify_round_trip's mismatch path.
                fn load(&self) -> Result<Option<Zeroizing<[u8; BLOB_LEN]>>, String> {
                    let mut buf: Zeroizing<[u8; BLOB_LEN]> = Zeroizing::new([0u8; BLOB_LEN]);
                    use rand::RngCore;
                    rand::rngs::OsRng.fill_bytes(buf.as_mut());
                    Ok(Some(buf))
                }
            }

            let original = fresh_seed();
            let store = CorruptingStore {
                inner: KeychainStore::new_mock(),
            };
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
            let identity_path = dir.path().join("identity.key");

            let kc = KeychainStore::new_load_failing_mock();
            let err = load_or_generate_with_keychain(&identity_path, Some(kc)).unwrap_err();

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
            let identity_path = dir.path().join("identity.key");

            // Healthy mock keychain: load returns Ok(None), save succeeds.
            let kc = KeychainStore::new_mock();
            let result = load_or_generate_with_keychain(&identity_path, Some(kc));

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
        assert_eq!(
            seed, *recovered,
            "seed must round-trip byte-for-byte through blob serialization"
        );
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
            assert!(
                err.contains("wrong passphrase or corrupted file"),
                "got: {err}"
            );
        }

        #[test]
        fn rotate_wrong_old_passphrase_fails() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");

            EncryptedFileStore::new(path.clone(), SecretString::from("real".to_string()))
                .save(&fresh_seed())
                .unwrap();

            let bytes_before = std::fs::read(&path).unwrap();

            let wrong =
                EncryptedFileStore::new(path.clone(), SecretString::from("wrong".to_string()));
            let err = rotate_passphrase(&wrong, SecretString::from("new".to_string())).unwrap_err();
            assert!(
                err.contains("wrong passphrase or corrupted file"),
                "got: {err}"
            );

            // File untouched.
            let bytes_after = std::fs::read(&path).unwrap();
            assert_eq!(
                bytes_before, bytes_after,
                "file must not be modified on auth failure"
            );
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
            assert_ne!(
                bytes_before, bytes_after,
                "salt+nonce must rotate even when passphrase is same"
            );
        }

        #[test]
        fn rotate_no_file_fails() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");
            let store = EncryptedFileStore::new(path, SecretString::from("any".to_string()));

            let err = rotate_passphrase(&store, SecretString::from("new".to_string())).unwrap_err();
            assert!(
                err.contains("no encrypted identity to rotate"),
                "got: {err}"
            );
        }

        #[test]
        fn rotate_refuses_while_another_process_holds_the_write_lock() {
            // ZEB-735: rotate_passphrase now takes the same cross-process
            // fd-lock as restore / generate (on the `identity.enc.lock` sibling
            // of the store path). A peer holding the lock must make rotate fail
            // fast, leaving the file untouched; once released it succeeds.
            // Deterministic — explicit passphrases, no env vars, no spawned
            // binaries (a second in-process fd-lock handle conflicts exactly as
            // another process would).
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("identity.enc");
            let lock_path = path.with_file_name("identity.enc.lock");
            let pass_a = SecretString::from("pass_a".to_string());
            let pass_b = SecretString::from("pass_b".to_string());
            let seed = fresh_seed();

            EncryptedFileStore::new(path.clone(), pass_a.clone())
                .save(&seed)
                .unwrap();

            let lock_file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&lock_path)
                .unwrap();
            let mut external = fd_lock::RwLock::new(lock_file);
            let held = external
                .try_write()
                .expect("external holder acquires the write lock");

            let store_a = EncryptedFileStore::new(path.clone(), pass_a.clone());
            let err = rotate_passphrase(&store_a, pass_b.clone())
                .expect_err("rotate must refuse while another writer holds the lock");
            assert!(
                err.contains("another harmony-app process is writing"),
                "actual: {err}"
            );
            // The RMW never ran: still decryptable with the OLD passphrase.
            let still_a = EncryptedFileStore::new(path.clone(), pass_a.clone())
                .load()
                .unwrap()
                .unwrap();
            assert_eq!(
                *still_a, seed,
                "rotate must not have modified the file while blocked"
            );

            // Release the peer; rotate now succeeds and the new passphrase
            // decrypts the same seed.
            drop(held);
            rotate_passphrase(&store_a, pass_b.clone())
                .expect("rotate must succeed once the lock frees");
            let loaded = EncryptedFileStore::new(path, pass_b)
                .load()
                .unwrap()
                .unwrap();
            assert_eq!(*loaded, seed, "rotated file must decrypt to the same seed");
        }
    }

    // ── Bug 1 regression: force-cleanup uses actual save destination ──────

    /// Keychain probe ok, keychain save fails → fallback writes encrypted file.
    /// The force-cleanup must NOT delete the freshly-written .enc.
    #[test]
    #[serial]
    fn force_does_not_delete_enc_when_keychain_save_fails_and_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let enc_path = identity_path.with_file_name("identity.enc");

        std::env::set_var("HARMONY_PASSPHRASE", "fallback-test");

        // new_failing_mock: load returns NoEntry (probe succeeds → keychain_healthy = true),
        // but save always returns Err → save_with_fallback falls back to encrypted file.
        let kc = KeychainStore::new_failing_mock();

        write_seed_to_disk_with_keychain(
            &identity_path,
            &[0xBEu8; 32],
            /*force=*/ true,
            Some(kc),
        )
        .expect("force write must succeed via encrypted-file fallback");

        assert!(
            enc_path.exists(),
            "the freshly-written encrypted file must NOT be deleted"
        );
        let raw = std::fs::read(&enc_path).unwrap();
        assert_eq!(
            &raw[0..4],
            b"HRMI",
            "the encrypted file must hold an HRMI envelope"
        );
        assert_eq!(
            raw[4], 0x02,
            "the encrypted file must hold the new v0x02 vault envelope"
        );
        assert!(
            raw.len() > 69,
            "envelope must carry a vault plaintext, got {}",
            raw.len()
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    // ── Bug 2 regression: write path tolerates bad env var when keychain is healthy ──

    /// Malformed HARMONY_PASSPHRASE must not break the write path when the keychain is healthy.
    #[test]
    #[serial]
    fn force_write_succeeds_via_keychain_when_at_rest_passphrase_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");

        // Empty HARMONY_PASSPHRASE → EncryptedFileStore::from_env returns Err.
        // With a healthy keychain the write should still succeed.
        std::env::set_var("HARMONY_PASSPHRASE", "");
        let kc = KeychainStore::new_mock();
        write_seed_to_disk_with_keychain(
            &identity_path,
            &[0xCEu8; 32],
            /*force=*/ true,
            Some(kc),
        )
        .expect("force write must succeed via keychain even with bad HARMONY_PASSPHRASE");

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    // ── Bug 3 regression: !force probe fails closed on keychain Err ───────

    /// Keychain probe Err (unreachable keychain) must refuse without --force.
    #[test]
    #[serial]
    fn write_seed_refuses_when_keychain_probe_fails_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");

        std::env::set_var("HARMONY_PASSPHRASE", "probe-fail-test");
        // new_load_failing_mock returns Err on load(), simulating an unreachable keychain.
        let kc = KeychainStore::new_load_failing_mock();

        let err = write_seed_to_disk_with_keychain(
            &identity_path,
            &[0x55u8; 32],
            /*force=*/ false,
            Some(kc),
        )
        .expect_err("must refuse when keychain probe fails (fail-closed)");
        assert!(
            err.contains("could not determine") || err.contains("refusing"),
            "actual: {err}"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }
}
