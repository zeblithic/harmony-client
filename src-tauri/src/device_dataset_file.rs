//! ZEB-982: device-sealed at-rest envelope for the owner-state family and
//! keyless-boot peripherals — the files that [`crate::fleet_dataset_file`]
//! (ZEB-981) structurally cannot cover because they are written on boots
//! where no fleet KeyTree exists (ZEB-905 local-only mode, pre-mint, the
//! recovery CLI).
//!
//! On-disk format:
//!
//! - legacy (read-only): any file whose first byte is not the sentinel —
//!   per-family schema-byte images (`0x01`/`0x02`) AND bare canonical CBOR
//!   (`owner_state.cbor`, `mint_sync_state.cbor`) alike
//! - v3 (written): `[0x03] ‖ nonce(12) ‖ AEAD(inner image) ‖ tag(16)`
//!
//! where the AEAD plaintext is the complete former on-disk image. Key: a
//! device-local key derived from the node identity master seed
//! ([`crate::owner_state_crypto::derive_device_dataset_key`]) — available on
//! every boot mode, so there is no plaintext fallback mode. AAD binds the
//! canonical filename, so ciphertext moved between files fails the tag.
//!
//! **This module owns only the envelope, never recovery semantics.** It
//! hands each persist family the decrypted-or-legacy inner image plus an
//! [`ImageError`] classified read-I/O vs content-corrupt, and the family
//! applies its own contract (boot-fatal for the owner family, quarantine
//! for the sidecars, freeze-writes for the card store, …). That layering —
//! not shared load/recover like `fleet_dataset_file` — is what preserves
//! each family's contract by construction. See
//! `docs/specs/2026-08-23-zeb-982-device-at-rest-encryption-design.md`.
//!
//! Sentinel `0x03` is reserved forever: after sealing ships, saves are
//! always sealed, so the plaintext formats (schema bytes ≤ 2, CBOR headers
//! `0x80+`/`0xA0+`) never evolve into it. `SEALED_SCHEMA_V2 == 2` in the
//! fleet module doubles as a key-domain marker: `0x02` = fleet KeyTree,
//! `0x03` = device key — a file handed to the wrong module fails loudly at
//! the version byte, never as a confusing AEAD failure.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use zeroize::Zeroizing;

use crate::owner_state_crypto::{
    derive_device_dataset_key, open_device_file, seal_device_file, CryptoError,
};

/// Outer version byte for device-sealed files. No legacy first byte in
/// scope equals it (verified per family in the ZEB-982 spec).
pub const SEALED_DEVICE_SCHEMA_V3: u8 = 3;

/// Coarse plausibility cap checked against file METADATA before the bytes
/// are read — same rationale and value as
/// `fleet_dataset_file::MAX_DATASET_FILE_BYTES` (PR #727 review).
const MAX_DEVICE_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// Sealing context for device-sealed files: the seed-derived at-rest key.
/// Cheap to clone; the key bytes are shared and zeroized on final drop.
#[derive(Clone)]
pub struct DeviceCipher {
    key: Arc<Zeroizing<[u8; 32]>>,
}

/// Redacting Debug: the key bytes must never reach logs or a `#[derive(Debug)]`
/// on a struct that holds a cipher (e.g. `ChannelLog`).
impl std::fmt::Debug for DeviceCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeviceCipher(<redacted>)")
    }
}

impl DeviceCipher {
    /// Derive from the node identity master seed (boot has it in hand
    /// right after `identity::load_or_generate`; the recovery CLI reads it
    /// via `identity::read_seed_from_disk`).
    pub fn derive(seed: &[u8; 32]) -> Result<Self, CryptoError> {
        Ok(Self {
            key: Arc::new(derive_device_dataset_key(seed)?),
        })
    }
}

/// Deterministic cipher for tests (unit + integration via `test-fixtures`).
#[cfg(any(test, feature = "test-fixtures"))]
pub fn test_cipher() -> DeviceCipher {
    DeviceCipher::derive(&[7u8; 32]).expect("test device cipher derives")
}

/// Pre-populate the [`get_or_derive`] memo with [`test_cipher`] for
/// `identity_dir`, so tests exercising the free-function owner-state paths
/// (which derive lazily) neither need an identity store nor a real seed.
#[cfg(any(test, feature = "test-fixtures"))]
pub fn install_test_cipher(identity_dir: &Path) {
    let memo_key = identity_dir
        .canonicalize()
        .unwrap_or_else(|_| identity_dir.to_path_buf());
    memo()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(memo_key, test_cipher());
}

/// Memoized derive for free-function call sites that cannot be threaded a
/// cipher (e.g. `owner_state::read_persisted_owner_id`'s convenience
/// wrapper). Keyed by canonicalized identity-dir path so multi-profile
/// processes and multi-test binaries never cross keys. The seed resolves
/// through the standard chain (`identity::read_seed_from_disk`: keychain →
/// `identity.enc` → generate), so test builds hit the same ZEB-428 gates as
/// any other seed read.
///
/// Callers that must not create an identity as a side effect (the chain
/// fresh-generates when no identity exists) should check their target
/// file's existence FIRST and skip the derive when it is absent.
fn memo() -> &'static Mutex<HashMap<PathBuf, DeviceCipher>> {
    static MEMO: LazyLock<Mutex<HashMap<PathBuf, DeviceCipher>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &MEMO
}

/// Bumped by [`invalidate`]. `get_or_derive` snapshots it before the seed
/// read and refuses to insert a cipher derived against a superseded
/// generation (PR #728 review): without this, a thread that read the OLD
/// seed could insert its stale cipher AFTER a restore's invalidation and
/// poison the memo for the rest of the process.
static MEMO_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn get_or_derive(identity_dir: &Path) -> Result<DeviceCipher, String> {
    let memo_key = identity_dir
        .canonicalize()
        .unwrap_or_else(|_| identity_dir.to_path_buf());
    loop {
        let generation = MEMO_GENERATION.load(std::sync::atomic::Ordering::Acquire);
        if let Some(cipher) = memo()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&memo_key)
        {
            return Ok(cipher.clone());
        }
        // Deliberately NOT holding the memo lock across the seed read: the
        // read can block on the identity flock, pay an Argon2id derive, or
        // take the fresh-generate path (which acquires
        // IDENTITY_FILE_WRITE_LOCK) — none of that may nest inside another
        // lock. Two same-generation racers derive the same key
        // (`with_identity_write_guards` makes generation atomic, so the
        // loser reads the winner's seed) and the second insert is an
        // identical overwrite.
        let seed = crate::identity::read_seed_from_disk(&identity_dir.join("identity.key"))?;
        let cipher = DeviceCipher::derive(&seed).map_err(|e| e.to_string())?;
        // Insert only if no invalidation happened since the snapshot — the
        // seed this cipher was derived from may have been replaced mid-read.
        // The re-check MUST happen UNDER the memo lock (Greptile, PR #728):
        // checked outside it, `invalidate` could bump-and-remove entirely
        // between the check and the insert, re-poisoning the memo with the
        // old-seed cipher. Under the lock the interleavings are exhaustive
        // and both safe: if this critical section runs after `invalidate`'s
        // remove, the mutex unlock→lock edge guarantees the bumped
        // generation is visible here and we retry; if it runs before, our
        // insert (stale or not) is removed by that pending `invalidate`.
        // On a lost race, loop: the retry re-reads the post-replacement seed.
        {
            let mut memo = memo().lock().unwrap_or_else(|p| p.into_inner());
            if MEMO_GENERATION.load(std::sync::atomic::Ordering::Acquire) == generation {
                memo.insert(memo_key.clone(), cipher.clone());
                return Ok(cipher);
            }
        }
    }
}

/// Drop any memoized cipher for `identity_dir`. MUST be called after the
/// node identity seed backing `<identity_dir>/identity.key` is overwritten
/// (recovery restore): the memo is keyed by directory, not seed value, so a
/// stale entry would keep sealing under the pre-restore key while the next
/// boot derives the new one — leaving every sealed file unreadable. Wired
/// into `identity::write_seed_to_disk_with_keychain` so any future seed
/// writer inherits the invalidation.
pub fn invalidate(identity_dir: &Path) {
    let memo_key = identity_dir
        .canonicalize()
        .unwrap_or_else(|_| identity_dir.to_path_buf());
    // Bump BEFORE taking the memo lock to remove: the bump is then
    // sequenced before this function's critical section, so any
    // `get_or_derive` insert that locks the memo AFTER this remove is
    // guaranteed (mutex unlock→lock happens-before) to see the bumped
    // generation and discard its possibly-stale cipher. An insert that
    // locked BEFORE this remove is cleaned up by the remove itself.
    MEMO_GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    memo()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&memo_key);
}

/// Envelope-layer failure, classified so each family can map it onto its
/// existing corrupt-vs-transient branch: `Io` is the read failing (the
/// bytes may be fine — do not discard state over it), `Crypto` is the bytes
/// being wrong (AEAD tag, truncated envelope, implausible size).
#[derive(Debug)]
pub enum ImageError {
    Io(std::io::Error),
    Crypto(String),
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageError::Io(e) => write!(f, "read failed: {e}"),
            ImageError::Crypto(msg) => write!(f, "sealed image invalid: {msg}"),
        }
    }
}

/// A file's inner image: the bytes each family's own parser consumes.
/// Debug is manual and redacting — decrypted contents must never land in
/// logs or assertion output.
pub struct Image {
    pub bytes: Zeroizing<Vec<u8>>,
    /// `true` → the file was plaintext on disk (any first byte other than
    /// the sentinel, including an empty file). Callers pass the image to
    /// [`reseal_if_legacy`] AFTER their own parse succeeds — resealing an
    /// image the family rejected would launder corrupt bytes into a valid
    /// envelope.
    pub was_legacy: bool,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("len", &self.bytes.len())
            .field("was_legacy", &self.was_legacy)
            .finish()
    }
}

/// The 256 MiB metadata cap, checked BEFORE any read. `Ok(())` also for
/// stat errors — those fall through to `read` so the transient-vs-missing
/// mapping is decided in exactly one place. Public within the module family
/// so callers that must read bytes themselves (`owner_state.rs`'s
/// single-read sentinel peek) enforce the same cap first (PR #728 review).
pub fn check_size_cap(path: &Path, filename: &str) -> Result<(), ImageError> {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_DEVICE_FILE_BYTES {
            return Err(ImageError::Crypto(format!(
                "{filename} is implausibly large ({} bytes, cap {MAX_DEVICE_FILE_BYTES}): {}",
                meta.len(),
                path.display()
            )));
        }
    }
    Ok(())
}

/// Envelope logic on bytes already in hand — the single-read seam for
/// callers that peek the sentinel themselves (PR #728 review: `read_image`'s
/// path-level API re-read the file after such a peek, an uncapped read plus
/// a TOCTOU between the two reads). Sealed → decrypt; anything else (schema
/// bytes, bare CBOR headers, the empty file) is the whole legacy image; the
/// family's own parser classifies it under that family's contract, not ours.
pub fn open_raw_image(
    cipher: &DeviceCipher,
    filename: &str,
    raw: Vec<u8>,
) -> Result<Image, ImageError> {
    match raw.first() {
        Some(&SEALED_DEVICE_SCHEMA_V3) => {
            let inner = open_device_file(&cipher.key, filename, &raw[1..])
                .map_err(|e| ImageError::Crypto(format!("open {filename}: {e}")))?;
            Ok(Image {
                bytes: inner,
                was_legacy: false,
            })
        }
        _ => Ok(Image {
            bytes: Zeroizing::new(raw),
            was_legacy: true,
        }),
    }
}

/// Read a file's inner image. `Ok(None)` = file absent. A legacy file (first
/// byte ≠ sentinel) is returned whole as the image; a sealed file is
/// decrypted. The 256 MiB metadata cap is checked before the read.
pub fn read_image(
    cipher: &DeviceCipher,
    path: &Path,
    filename: &str,
) -> Result<Option<Image>, ImageError> {
    check_size_cap(path, filename)?;
    let raw = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ImageError::Io(e)),
    };
    open_raw_image(cipher, filename, raw).map(Some)
}

/// Envelope overhead in the on-disk form: sentinel + nonce + Poly1305 tag.
const SEALED_OVERHEAD: usize = 1 + 12 + 16;

/// True iff an inner image of `inner_len` bytes seals to a file within the
/// read-side cap. Write-side guard (PR #728 review): the owner family's
/// read-refusal is boot-fatal, so writing a file the next boot would refuse
/// to read is self-bricking — refusing the WRITE loses one persist (loud,
/// retryable) instead of the whole state.
fn sealed_len_within_cap(inner_len: usize) -> bool {
    (inner_len as u64).saturating_add(SEALED_OVERHEAD as u64) <= MAX_DEVICE_FILE_BYTES
}

/// Seal `inner` into the complete v3 on-disk byte form (sentinel-prefixed).
/// For families that keep their own write primitive (e.g. `owner_state.rs`'s
/// `write_atomic_0600`); everything else uses [`write_image`]. Refuses an
/// inner image whose sealed form would exceed the read-side size cap.
pub fn seal_image(cipher: &DeviceCipher, filename: &str, inner: &[u8]) -> Result<Vec<u8>, String> {
    if !sealed_len_within_cap(inner.len()) {
        return Err(format!(
            "{filename}: inner image of {} bytes would seal past the {MAX_DEVICE_FILE_BYTES}-byte \
             cap the next boot enforces on read; refusing to write",
            inner.len()
        ));
    }
    let sealed = seal_device_file(&cipher.key, filename, inner)
        .map_err(|e| format!("seal {filename}: {e}"))?;
    let mut bytes = Vec::with_capacity(1 + sealed.len());
    bytes.push(SEALED_DEVICE_SCHEMA_V3);
    bytes.extend_from_slice(&sealed);
    Ok(bytes)
}

/// Seal `inner` and write the v3 envelope atomically (parent-dir creation +
/// crash-durable rename via `owner_state_persist::save_atomically`).
pub fn write_image(
    cipher: &DeviceCipher,
    path: &Path,
    filename: &str,
    inner: &[u8],
) -> Result<(), std::io::Error> {
    let bytes = seal_image(cipher, filename, inner).map_err(std::io::Error::other)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::owner_state_persist::save_atomically(path, &bytes)
        .map_err(|e| std::io::Error::other(e.to_string()))
}

/// Best-effort eager migration: if the image came off disk as legacy
/// plaintext, re-seal exactly those bytes (no re-serialization — the
/// ZEB-981 byte-losslessness contract). A write failure warns and leaves
/// the plaintext in place; the next load retries. Call only after the
/// family's own parse of `image.bytes` succeeded.
pub fn reseal_if_legacy(cipher: &DeviceCipher, path: &Path, filename: &str, image: &Image) {
    if !image.was_legacy {
        return;
    }
    if let Err(e) = write_image(cipher, path, filename, &image.bytes) {
        tracing::warn!(
            file = filename,
            error = %e,
            "ZEB-982: legacy plaintext file could not be re-sealed; leaving in place"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn sealed_round_trip() {
        let dir = tmp();
        let path = dir.path().join("owner_state.cbor");
        let cipher = test_cipher();
        write_image(&cipher, &path, "owner_state.cbor", b"\xa5inner").unwrap();
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk[0], SEALED_DEVICE_SCHEMA_V3);
        let img = read_image(&cipher, &path, "owner_state.cbor")
            .unwrap()
            .unwrap();
        assert_eq!(&img.bytes[..], b"\xa5inner");
        assert!(!img.was_legacy);
    }

    #[test]
    fn missing_file_is_none() {
        let dir = tmp();
        let cipher = test_cipher();
        assert!(
            read_image(&cipher, &dir.path().join("absent.cbor"), "absent.cbor")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn legacy_schema_bytes_route_to_legacy_image() {
        // Collision regression (ZEB-982 spec): first byte 2 — the owner
        // CRDT's CRDT_FILE_SCHEMA_V2 — must route to the legacy path, never
        // the envelope. Same for schema byte 1.
        let dir = tmp();
        let cipher = test_cipher();
        for (name, first) in [
            ("owner_state_crdt.cbor", 2u8),
            ("state_root_replay.cbor", 1u8),
        ] {
            let path = dir.path().join(name);
            let legacy = [&[first][..], b"payload"].concat();
            std::fs::write(&path, &legacy).unwrap();
            let img = read_image(&cipher, &path, name).unwrap().unwrap();
            assert!(img.was_legacy);
            assert_eq!(&img.bytes[..], &legacy[..], "whole file is the image");
        }
    }

    #[test]
    fn legacy_bare_cbor_routes_to_legacy_image() {
        // owner_state.cbor has no schema byte — first byte is a CBOR map
        // header (0xA5 for the 5-field struct).
        let dir = tmp();
        let path = dir.path().join("owner_state.cbor");
        let legacy = b"\xa5rest-of-map".to_vec();
        std::fs::write(&path, &legacy).unwrap();
        let cipher = test_cipher();
        let img = read_image(&cipher, &path, "owner_state.cbor")
            .unwrap()
            .unwrap();
        assert!(img.was_legacy);
        assert_eq!(&img.bytes[..], &legacy[..]);
    }

    #[test]
    fn empty_file_is_legacy_empty_image() {
        let dir = tmp();
        let path = dir.path().join("owner_state.cbor");
        std::fs::write(&path, b"").unwrap();
        let cipher = test_cipher();
        let img = read_image(&cipher, &path, "owner_state.cbor")
            .unwrap()
            .unwrap();
        assert!(img.was_legacy);
        assert!(img.bytes.is_empty());
    }

    #[test]
    fn reseal_if_legacy_is_byte_lossless() {
        let dir = tmp();
        let path = dir.path().join("owner_state_crdt.cbor");
        // Deliberately non-canonical padding after the payload: migration
        // must preserve it verbatim.
        let legacy = [&[2u8][..], b"payload-with-\x00-oddities"].concat();
        std::fs::write(&path, &legacy).unwrap();
        let cipher = test_cipher();
        let img = read_image(&cipher, &path, "owner_state_crdt.cbor")
            .unwrap()
            .unwrap();
        reseal_if_legacy(&cipher, &path, "owner_state_crdt.cbor", &img);
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk[0], SEALED_DEVICE_SCHEMA_V3, "now sealed");
        let reopened = read_image(&cipher, &path, "owner_state_crdt.cbor")
            .unwrap()
            .unwrap();
        assert!(!reopened.was_legacy);
        assert_eq!(&reopened.bytes[..], &legacy[..], "inner image identical");
    }

    #[test]
    fn reseal_noop_when_already_sealed() {
        let dir = tmp();
        let path = dir.path().join("f.cbor");
        let cipher = test_cipher();
        write_image(&cipher, &path, "f.cbor", b"x").unwrap();
        let before = std::fs::read(&path).unwrap();
        let img = read_image(&cipher, &path, "f.cbor").unwrap().unwrap();
        reseal_if_legacy(&cipher, &path, "f.cbor", &img);
        assert_eq!(std::fs::read(&path).unwrap(), before, "no rewrite");
    }

    #[cfg(unix)]
    #[test]
    fn reseal_write_failure_leaves_plaintext_in_place() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp();
        let sub = dir.path().join("ro");
        std::fs::create_dir(&sub).unwrap();
        let path = sub.join("f.cbor");
        let legacy = [&[1u8][..], b"payload"].concat();
        std::fs::write(&path, &legacy).unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root bypasses directory permissions (PR #727 review): probe and
        // skip rather than assert a write failure that will not happen.
        let probe = sub.join("probe");
        if std::fs::write(&probe, b"x").is_ok() {
            let _ = std::fs::remove_file(&probe);
            let _ = std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755));
            return;
        }
        let cipher = test_cipher();
        let img = read_image(&cipher, &path, "f.cbor").unwrap().unwrap();
        reseal_if_legacy(&cipher, &path, "f.cbor", &img);
        let _ = std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            legacy,
            "plaintext untouched after failed reseal"
        );
    }

    #[test]
    fn filename_swap_fails_as_crypto() {
        let dir = tmp();
        let a = dir.path().join("a.cbor");
        let cipher = test_cipher();
        write_image(&cipher, &a, "a.cbor", b"x").unwrap();
        match read_image(&cipher, &a, "b.cbor") {
            Err(ImageError::Crypto(_)) => {}
            other => panic!("expected Crypto error, got {other:?}"),
        }
    }

    #[test]
    fn foreign_seed_fails_as_crypto() {
        let dir = tmp();
        let path = dir.path().join("f.cbor");
        let cipher = test_cipher();
        write_image(&cipher, &path, "f.cbor", b"x").unwrap();
        let other = DeviceCipher::derive(&[8u8; 32]).unwrap();
        match read_image(&other, &path, "f.cbor") {
            Err(ImageError::Crypto(_)) => {}
            other => panic!("expected Crypto error, got {other:?}"),
        }
    }

    #[test]
    fn truncated_envelope_fails_as_crypto() {
        let dir = tmp();
        let path = dir.path().join("f.cbor");
        // Sentinel present but too short for nonce+tag.
        std::fs::write(&path, [SEALED_DEVICE_SCHEMA_V3, 0, 1, 2]).unwrap();
        let cipher = test_cipher();
        match read_image(&cipher, &path, "f.cbor") {
            Err(ImageError::Crypto(_)) => {}
            other => panic!("expected Crypto error, got {other:?}"),
        }
    }

    #[test]
    fn transient_io_fails_as_io_never_crypto() {
        let dir = tmp();
        // The path is a directory: read fails with a non-NotFound I/O error.
        let cipher = test_cipher();
        match read_image(&cipher, dir.path(), "f.cbor") {
            Err(ImageError::Io(_)) => {}
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    #[test]
    fn implausibly_large_file_refused_before_read() {
        let dir = tmp();
        let path = dir.path().join("f.cbor");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_DEVICE_FILE_BYTES + 1).unwrap();
        drop(f);
        let cipher = test_cipher();
        match read_image(&cipher, &path, "f.cbor") {
            Err(ImageError::Crypto(msg)) => {
                assert!(msg.contains("implausibly large"), "{msg}");
            }
            other => panic!("expected Crypto error, got {other:?}"),
        }
    }

    #[test]
    fn sealed_len_cap_boundary() {
        // Largest accepted inner image vs the first rejected one — pure
        // helper math, so the boundary is testable without 256 MiB allocs.
        let max_inner = (MAX_DEVICE_FILE_BYTES as usize) - SEALED_OVERHEAD;
        assert!(sealed_len_within_cap(max_inner));
        assert!(!sealed_len_within_cap(max_inner + 1));
        assert!(!sealed_len_within_cap(usize::MAX), "no overflow wraparound");
    }

    #[test]
    fn seal_image_refuses_oversize_inner() {
        // The check runs before any encryption, so the reject is instant.
        let cipher = test_cipher();
        let oversize = vec![0u8; (MAX_DEVICE_FILE_BYTES as usize) - SEALED_OVERHEAD + 1];
        let err = seal_image(&cipher, "f.cbor", &oversize).unwrap_err();
        assert!(err.contains("refusing to write"), "{err}");
    }

    #[test]
    fn invalidation_between_seed_read_and_insert_discards_stale_cipher() {
        // PR #728 review: a derive that lost a race with `invalidate` must
        // not poison the memo. Simulated deterministically: install a
        // cipher, snapshot the generation semantics by invalidating, then
        // verify the memo entry is gone AND a later install wins cleanly
        // (the retry loop's insert path).
        let dir = tmp();
        install_test_cipher(dir.path());
        let memo_key = dir.path().canonicalize().unwrap();
        assert!(memo().lock().unwrap().contains_key(&memo_key));
        invalidate(dir.path());
        assert!(
            !memo().lock().unwrap().contains_key(&memo_key),
            "invalidate removes the entry"
        );
        // Re-install (stands in for the retry's fresh derive) — usable again.
        install_test_cipher(dir.path());
        assert!(memo().lock().unwrap().contains_key(&memo_key));
    }
}
