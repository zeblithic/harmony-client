//! HRSS envelope: passphrase-encrypted owner-state CRDT snapshot.
//!
//! Sidecar format to ZEB-176's HRMR identity backup. Distinct magic
//! and AAD, identical Argon2id + XChaCha20-Poly1305 primitives. See
//! `docs/specs/2026-05-14-zeb-213-identity-backup-owner-state-design.md`.

use std::path::Path;

use ciborium::{from_reader, into_writer};
use harmony_crypto::password_envelope::{self, Argon2idParams};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::owner_state_crdt::OwnerState;
use crate::owner_state_persist::canonicalize;
use crate::owner_state_types::{deserialize_vec_from_bstr, serialize_vec_as_bstr, Hlc, OwnerAddr};

// ── HRSS wire-format constants ─────────────────────────────────────────

/// Envelope magic bytes. Distinct from HRMI (`b"HRMI"`) so a sidecar
/// can never be confused with an identity envelope.
const HRSS_MAGIC: &[u8; 4] = b"HRSS";
const HRSS_FORMAT_VERSION: u8 = 0x01;
const HRSS_KDF_ID_ARGON2ID: u8 = 0x01;

// Argon2id parameters — identical to identity.rs's HRMI envelope so a
// single passphrase can unlock both with the same compute budget.
const KDF_M_KIB: u32 = 65536; // 64 MiB
const KDF_T: u16 = 3;
const KDF_P: u8 = 1;

// Header layout (same shape as HRMI): magic(4) + version(1) + kdf_id(1)
// + m_kib(4 BE) + t(2 BE) + p(1) = 13 bytes.
const HEADER_LEN: usize = 13;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24; // XChaCha20 needs 192-bit nonce
const TAG_LEN: usize = 16; // Poly1305 tag

/// AAD passed to the AEAD. Distinct from any HRMI AAD so a leaked
/// passphrase + magic-flip cannot produce a valid cross-envelope
/// substitution — the AAD binds the AEAD to "this is a state snapshot".
const HRSS_AAD: &[u8] = b"harmony-owner-state-snapshot-v1";

/// Current snapshot payload version. Bump when the inner CBOR shape
/// changes (separate from the envelope version which bumps when the
/// crypto layout changes).
const SNAPSHOT_PAYLOAD_VERSION: u32 = 1;

// ── Errors ─────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("HRSS envelope is too short ({0} bytes; minimum {})", HEADER_LEN + SALT_LEN + NONCE_LEN + TAG_LEN)]
    TooShort(usize),
    #[error("HRSS envelope magic mismatch (expected {:?}, got {:?})", HRSS_MAGIC, .0)]
    BadMagic([u8; 4]),
    #[error("unsupported HRSS envelope version: {0:#04x}")]
    UnsupportedEnvelopeVersion(u8),
    #[error("unsupported KDF id: {0:#04x}")]
    UnsupportedKdfId(u8),
    #[error("HRSS could not be decrypted: wrong passphrase or corrupted file")]
    WrongPassphraseOrCorrupt,
    #[error("HRSS CBOR payload malformed: {0}")]
    CborDecode(String),
    #[error("HRSS CBOR payload could not be encoded: {0}")]
    CborEncode(String),
    #[error("state snapshot format version {0} not supported; please update harmony-app")]
    UnsupportedSnapshotVersion(u32),
    #[error("state sidecar identity mismatch — HRSS addr {} != restored identity {}", hex::encode(.snapshot_addr), hex::encode(.expected_addr))]
    AddrMismatch {
        snapshot_addr: [u8; 16],
        expected_addr: [u8; 16],
    },
    #[error("Argon2 derivation failed: {0}")]
    Argon2Fail(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

// ── Snapshot payload (inner CBOR plaintext) ────────────────────────────

/// Cleartext payload carried inside an HRSS envelope. Canonical CBOR
/// per RFC 8949 §4.2: bytewise-sorted map keys, shortest-form ints,
/// definite-length, no tags. Field names are 2 chars each to satisfy
/// the same-length-keys invariant at this nesting level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerStateSnapshot {
    /// Payload version. Currently `1`. Bumped when the CBOR shape changes.
    #[serde(rename = "vn")]
    pub version: u32,
    /// Owner address (16 bytes); binds the snapshot to one identity.
    #[serde(rename = "oa")]
    pub owner_addr: OwnerAddr,
    /// Export-time HLC; drives the GUI staleness banner.
    #[serde(rename = "at")]
    pub at: Hlc,
    /// Tree bytes — exactly the output of `owner_state_persist::canonicalize(&OwnerState)`,
    /// i.e. `[schema_v2, ...cbor...]`. Stored as bstr.
    #[serde(rename = "tr")]
    #[serde(
        serialize_with = "serialize_vec_as_bstr",
        deserialize_with = "deserialize_vec_from_bstr"
    )]
    pub tree: Vec<u8>,
}

// ── Public API ─────────────────────────────────────────────────────────

/// Encode an owner-state snapshot into an HRSS envelope.
///
/// Generates fresh random salt + nonce per call (CSPRNG). Production
/// callers should use this. Tests using `test-fixtures` may bypass via
/// `encode_snapshot_with_params`.
pub fn encode_snapshot(
    passphrase: &[u8],
    addr: OwnerAddr,
    at: Hlc,
    state: &OwnerState,
) -> Result<Vec<u8>, SnapshotError> {
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut salt);
    rand::thread_rng().fill_bytes(&mut nonce);
    encode_snapshot_inner(passphrase, addr, at, state, &salt, &nonce)
}

/// Deterministic variant — same crypto, caller-supplied salt + nonce.
/// Used by `test-fixtures` for byte-pinning. Production code must call
/// `encode_snapshot` to ensure fresh entropy.
///
/// **Security**: gated behind `#[cfg(any(test, feature = "test-fixtures"))]`
/// so production builds cannot link this symbol. Catastrophic
/// nonce-reuse on XChaCha20-Poly1305 leaks the keystream and
/// authentication key — production code MUST NOT have access to this
/// helper. Round-1 bot finding C4 (Qodo High Severity).
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn encode_snapshot_with_params(
    passphrase: &[u8],
    addr: OwnerAddr,
    at: Hlc,
    state: &OwnerState,
    salt: &[u8; SALT_LEN],
    nonce: &[u8; NONCE_LEN],
) -> Result<Vec<u8>, SnapshotError> {
    encode_snapshot_inner(passphrase, addr, at, state, salt, nonce)
}

/// Internal shared implementation. Crate-private — production callers
/// reach it only through [`encode_snapshot`] (random salt/nonce); test
/// fixtures reach it through [`encode_snapshot_with_params`].
fn encode_snapshot_inner(
    passphrase: &[u8],
    addr: OwnerAddr,
    at: Hlc,
    state: &OwnerState,
    salt: &[u8; SALT_LEN],
    nonce: &[u8; NONCE_LEN],
) -> Result<Vec<u8>, SnapshotError> {
    let tree =
        canonicalize(state).map_err(|e| SnapshotError::CborEncode(format!("canonicalize: {e}")))?;
    let snapshot = OwnerStateSnapshot {
        version: SNAPSHOT_PAYLOAD_VERSION,
        owner_addr: addr,
        at,
        tree,
    };
    let mut cbor: Vec<u8> = Vec::new();
    into_writer(&snapshot, &mut cbor).map_err(|e| SnapshotError::CborEncode(e.to_string()))?;
    let cbor = Zeroizing::new(cbor);

    // Header: 13 bytes (same shape as HRMI). Also serves as AAD's
    // structural part — but the actual AEAD AAD is HRSS_AAD constant
    // for cross-envelope domain separation.
    let mut out = Vec::with_capacity(HEADER_LEN + SALT_LEN + NONCE_LEN + cbor.len() + TAG_LEN);
    out.extend_from_slice(HRSS_MAGIC);
    out.push(HRSS_FORMAT_VERSION);
    out.push(HRSS_KDF_ID_ARGON2ID);
    out.extend_from_slice(&KDF_M_KIB.to_be_bytes());
    out.extend_from_slice(&KDF_T.to_be_bytes());
    out.push(KDF_P);
    debug_assert_eq!(out.len(), HEADER_LEN);
    out.extend_from_slice(salt);
    out.extend_from_slice(nonce);

    let params = Argon2idParams::new(KDF_M_KIB, KDF_T as u32, KDF_P as u32)
        .map_err(|e| SnapshotError::Argon2Fail(e.to_string()))?;
    let ciphertext_with_tag =
        password_envelope::seal(passphrase, &params, salt, nonce, HRSS_AAD, cbor.as_slice())
            .map_err(|_| SnapshotError::WrongPassphraseOrCorrupt)?;
    out.extend_from_slice(&ciphertext_with_tag);
    Ok(out)
}

/// Decode an HRSS envelope and return its inner `OwnerStateSnapshot`.
///
/// Does NOT verify `owner_addr` against a restored identity — that is
/// the caller's responsibility (see `verify_snapshot_addr`). Failures
/// here are all envelope-level: malformed bytes, wrong passphrase,
/// unknown version.
pub fn decode_snapshot(
    passphrase: &[u8],
    bytes: &[u8],
) -> Result<OwnerStateSnapshot, SnapshotError> {
    if bytes.len() < HEADER_LEN + SALT_LEN + NONCE_LEN + TAG_LEN {
        return Err(SnapshotError::TooShort(bytes.len()));
    }
    if &bytes[0..4] != HRSS_MAGIC {
        let mut got = [0u8; 4];
        got.copy_from_slice(&bytes[0..4]);
        return Err(SnapshotError::BadMagic(got));
    }
    if bytes[4] != HRSS_FORMAT_VERSION {
        return Err(SnapshotError::UnsupportedEnvelopeVersion(bytes[4]));
    }
    if bytes[5] != HRSS_KDF_ID_ARGON2ID {
        return Err(SnapshotError::UnsupportedKdfId(bytes[5]));
    }

    const M_KIB_OFF: usize = 6;
    const T_OFF: usize = M_KIB_OFF + 4; // 10
    const P_OFF: usize = T_OFF + 2; // 12
    const SALT_OFF: usize = HEADER_LEN; // 13
    const NONCE_OFF: usize = SALT_OFF + SALT_LEN; // 29
    const CIPHER_OFF: usize = NONCE_OFF + NONCE_LEN; // 53

    let m_kib = u32::from_be_bytes(bytes[M_KIB_OFF..M_KIB_OFF + 4].try_into().unwrap());
    let t = u16::from_be_bytes(bytes[T_OFF..T_OFF + 2].try_into().unwrap()) as u32;
    let p = bytes[P_OFF] as u32;
    if m_kib != KDF_M_KIB || t != KDF_T as u32 || p != KDF_P as u32 {
        return Err(SnapshotError::WrongPassphraseOrCorrupt);
    }
    let salt: &[u8; SALT_LEN] = bytes[SALT_OFF..NONCE_OFF].try_into().unwrap();
    let nonce: &[u8; NONCE_LEN] = bytes[NONCE_OFF..CIPHER_OFF].try_into().unwrap();
    let ciphertext_with_tag = &bytes[CIPHER_OFF..];

    let params =
        Argon2idParams::new(m_kib, t, p).map_err(|e| SnapshotError::Argon2Fail(e.to_string()))?;
    let cleartext = password_envelope::open(
        passphrase,
        &params,
        salt,
        nonce,
        HRSS_AAD,
        ciphertext_with_tag,
    )
    .map_err(|_| SnapshotError::WrongPassphraseOrCorrupt)?;

    let snapshot: OwnerStateSnapshot =
        from_reader(cleartext.as_slice()).map_err(|e| SnapshotError::CborDecode(e.to_string()))?;
    if snapshot.version != SNAPSHOT_PAYLOAD_VERSION {
        return Err(SnapshotError::UnsupportedSnapshotVersion(snapshot.version));
    }
    Ok(snapshot)
}

/// Verify the snapshot's `owner_addr` matches a restored identity.
/// Hard-fail on mismatch — prevents pairing an HRMR with someone
/// else's HRSS sidecar.
pub fn verify_snapshot_addr(
    snapshot: &OwnerStateSnapshot,
    expected: &[u8; 16],
) -> Result<(), SnapshotError> {
    if &snapshot.owner_addr.0 != expected {
        return Err(SnapshotError::AddrMismatch {
            snapshot_addr: snapshot.owner_addr.0,
            expected_addr: *expected,
        });
    }
    Ok(())
}

/// Convenience: read an HRSS sidecar file from disk and decode.
pub fn decode_snapshot_file(
    passphrase: &[u8],
    path: &Path,
) -> Result<OwnerStateSnapshot, SnapshotError> {
    let bytes = std::fs::read(path).map_err(SnapshotError::Io)?;
    decode_snapshot(passphrase, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_crdt::OwnerState;
    use crate::owner_state_types::{OwnerAddr, Space, SpaceId, SpaceKind};

    fn hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "test-device".into(),
        }
    }

    fn sample_owner_addr() -> OwnerAddr {
        OwnerAddr([0xAA; 16])
    }

    fn sample_state() -> OwnerState {
        let mut s = OwnerState::default();
        let folder = Space {
            id: SpaceId([0x11; 16]),
            kind: SpaceKind::Folder,
            parent: None,
            community_id: None,
            name: "Sample".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(100),
            updated_at: hlc(200),
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            read_receipt_pref: None,
            pending_join_at: None,
        };
        s.spaces.insert(folder.id, folder);
        s
    }

    #[test]
    fn hrss_envelope_round_trip() {
        let state = sample_state();
        let addr = sample_owner_addr();
        let at = hlc(1_700_000_000);
        let bytes = encode_snapshot(b"correct-pass", addr, at.clone(), &state).expect("encode");
        let snapshot = decode_snapshot(b"correct-pass", &bytes).expect("decode");
        assert_eq!(snapshot.version, 1);
        assert_eq!(snapshot.owner_addr, addr);
        assert_eq!(snapshot.at, at);
        // The tree bytes must be exactly canonicalize(&state).
        let expected_tree = canonicalize(&state).unwrap();
        assert_eq!(snapshot.tree, expected_tree);
    }

    #[test]
    fn hrss_addr_binding_rejects_cross_identity() {
        let state = sample_state();
        let addr_a = OwnerAddr([0xAA; 16]);
        let addr_b = OwnerAddr([0xBB; 16]);
        let bytes = encode_snapshot(b"pp", addr_a, hlc(1), &state).unwrap();
        let snapshot = decode_snapshot(b"pp", &bytes).unwrap();
        let err = verify_snapshot_addr(&snapshot, &addr_b.0).expect_err("must reject");
        match err {
            SnapshotError::AddrMismatch {
                snapshot_addr,
                expected_addr,
            } => {
                assert_eq!(snapshot_addr, addr_a.0, "snapshot_addr must hold A");
                assert_eq!(expected_addr, addr_b.0, "expected_addr must hold B");
            }
            other => panic!("expected AddrMismatch, got {other:?}"),
        }
    }

    #[test]
    fn hrss_wrong_passphrase() {
        let state = sample_state();
        let bytes = encode_snapshot(b"right", sample_owner_addr(), hlc(1), &state).unwrap();
        let err = decode_snapshot(b"wrong", &bytes).expect_err("must reject");
        assert!(
            matches!(err, SnapshotError::WrongPassphraseOrCorrupt),
            "got: {err:?}"
        );
    }

    #[test]
    fn hrss_unknown_version_rejected() {
        let state = sample_state();
        // Synthesize a snapshot with version 99, canonical-encode it,
        // then HRSS-wrap.
        let snapshot = OwnerStateSnapshot {
            version: 99,
            owner_addr: sample_owner_addr(),
            at: hlc(1),
            tree: canonicalize(&state).unwrap(),
        };
        let mut cbor = Vec::new();
        into_writer(&snapshot, &mut cbor).unwrap();

        // Build a fresh envelope where the inner CBOR carries v=99. We do this by
        // re-running the AEAD over the v=99 cbor via the shared primitive.
        let salt = [0u8; SALT_LEN];
        let nonce = [0u8; NONCE_LEN];
        let mut out = Vec::new();
        out.extend_from_slice(HRSS_MAGIC);
        out.push(HRSS_FORMAT_VERSION);
        out.push(HRSS_KDF_ID_ARGON2ID);
        out.extend_from_slice(&KDF_M_KIB.to_be_bytes());
        out.extend_from_slice(&KDF_T.to_be_bytes());
        out.push(KDF_P);
        out.extend_from_slice(&salt);
        out.extend_from_slice(&nonce);
        let params = Argon2idParams::new(KDF_M_KIB, KDF_T as u32, KDF_P as u32).unwrap();
        let ciphertext_with_tag =
            password_envelope::seal(b"pp", &params, &salt, &nonce, HRSS_AAD, cbor.as_slice())
                .unwrap();
        out.extend_from_slice(&ciphertext_with_tag);

        let err = decode_snapshot(b"pp", &out).expect_err("must reject future version");
        assert!(
            matches!(err, SnapshotError::UnsupportedSnapshotVersion(99)),
            "got: {err:?}"
        );
    }

    #[test]
    fn hrss_canonical_cbor_stability() {
        // Same state + same params + same passphrase → byte-identical
        // envelope. Load-bearing for the wire-format fixture in Task 3.
        let state = sample_state();
        let salt = [0x11u8; SALT_LEN];
        let nonce = [0x22u8; NONCE_LEN];
        let a =
            encode_snapshot_with_params(b"pp", sample_owner_addr(), hlc(1), &state, &salt, &nonce)
                .unwrap();
        let b =
            encode_snapshot_with_params(b"pp", sample_owner_addr(), hlc(1), &state, &salt, &nonce)
                .unwrap();
        assert_eq!(a, b, "deterministic params must yield byte-identical HRSS");
    }
}
