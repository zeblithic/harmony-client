//! ZEB-216 Sub-B Phase 3b: per-device Ed25519 signing primitives for
//! Reticulum DM packet bodies (Path B per spec — see
//! docs/specs/2026-05-02-zeb-216-sub-b-dm-transport-design.md
//! §"Application-signature binding rule").
//!
//! Pure functions over (body_bytes, key, signature). No state, no I/O.
//!
//! Device-hash-from-pubkey scheme: delegates to
//! `harmony_identity::Identity::from_public_bytes(identity_pub).address_hash`,
//! which is `SHA256(X25519_pub(32) || Ed25519_pub(32))[:16]` per
//! `~/work/zeblithic/harmony/crates/harmony-identity/src/identity.rs:58-76`
//! (commit c53e525). The 64-byte combined-pubs input is what makes the
//! computed hash match `DeviceIdentityHash` values stored in
//! OwnerDeviceCache.devices — an Ed25519-only hash would diverge.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier};
use sha2::{Digest, Sha256};

use crate::dm_outbox::DmReceiveError;
use crate::owner_state_types::DeviceIdentityHash;

// The sealed-box PKE and the ed25519↔x25519 conversions now live in core
// `harmony_crypto` (ZEB-738 / harmony#290). This module keeps the client
// domain layer: the `DmSignError` taxonomy, the frozen `info` domain constant,
// and thin delegating wrappers so the ~dozens of `crate::dm_signing::…` call
// sites are unchanged. The core construction is byte-identical (it composes the
// same HKDF-SHA256 + ChaCha20-Poly1305 primitives with empty AAD).

/// Errors from epoch-key sealing operations (`seal_to_owner` / `open_from_owner`).
/// Distinct from `DmReceiveError` because these helpers are used in
/// EpochRotation/EpochCatchup key-delivery paths (Tasks 3+), not the
/// DM-packet receive pipeline.
#[derive(Debug, thiserror::Error)]
pub enum DmSignError {
    #[error("AEAD encryption failed")]
    EncryptionFailed,
    #[error("AEAD decryption failed (tag mismatch or wrong key)")]
    DecryptionFailed,
    #[error("malformed sealed envelope (too short or bad framing)")]
    MalformedSealedEnvelope,
    #[error("invalid Ed25519 pubkey (cannot decompress)")]
    InvalidEd25519Pubkey,
    /// C1/C2: low-order or small-order public key rejected. Either the
    /// X25519 ephemeral produced an all-zero shared secret (low-order
    /// point attack on seal/open), or the Ed25519 point is small-order
    /// (torsion component attack on the Twisted Edwards → Montgomery
    /// conversion). In both cases the resulting ECDH output is predictable
    /// and MUST be rejected before it reaches the AEAD layer.
    #[error("low-order or small-order public key rejected")]
    InvalidPublicKey,
}

/// Seal a payload to a recipient's X25519 public key using
/// X25519-ECDH-derived ChaCha20-Poly1305 (hybrid public-key encryption).
///
/// Output layout (92 bytes total for a 32-byte payload):
///   - 32 bytes: ephemeral X25519 public key (fresh per call)
///   - 12 bytes: AEAD random nonce
///   - 32 bytes: ciphertext
///   - 16 bytes: Poly1305 authentication tag
///
/// The shared secret is HKDF-derived from the ECDH output with empty
/// salt + a domain-separation `info` string. The ephemeral pubkey is
/// fresh per call — no nonce-reuse risk across multiple seals to the
/// same recipient.
///
/// Used by ZEB-249's EpochRotation/EpochCatchup events to deliver
/// fresh EpochKeys to specific recipients.
pub fn seal_to_owner(
    recipient_x25519_pub: &[u8; 32],
    plaintext: &[u8],
) -> Result<Vec<u8>, DmSignError> {
    seal_to_owner_with_info(recipient_x25519_pub, plaintext, ZEB_249_EPOCH_KEY_SEAL_INFO)
}

/// [`seal_to_owner`] with a caller-supplied HKDF domain-separation `info`
/// string. Identical envelope layout (`32-byte ephemeral X25519 ‖ 12-byte
/// nonce ‖ ct+tag`) and identical low-order-point checks — only the AEAD
/// key derivation is domain-separated, so a ciphertext sealed for one
/// context can never be opened in another (ZEB-418 butler deposits use
/// `butler_deposit::BUTLER_DEPOSIT_SEAL_INFO`).
pub fn seal_to_owner_with_info(
    recipient_x25519_pub: &[u8; 32],
    plaintext: &[u8],
    info: &[u8],
) -> Result<Vec<u8>, DmSignError> {
    harmony_crypto::sealed_box::seal(
        recipient_x25519_pub,
        plaintext,
        info,
        &mut rand::rngs::OsRng,
    )
    .map_err(|e| map_sealed_box_err(e, DmSignError::EncryptionFailed))
}

/// Open a sealed envelope using the recipient's X25519 private key.
/// Inverse of `seal_to_owner`. Returns `DmSignError::DecryptionFailed`
/// on AEAD tag mismatch (wrong recipient OR tampered ciphertext).
pub fn open_from_owner(
    recipient_x25519_priv: &[u8; 32],
    sealed: &[u8],
) -> Result<Vec<u8>, DmSignError> {
    open_from_owner_with_info(recipient_x25519_priv, sealed, ZEB_249_EPOCH_KEY_SEAL_INFO)
}

/// [`open_from_owner`] with a caller-supplied HKDF domain-separation `info`
/// string. Inverse of [`seal_to_owner_with_info`] — `info` must match the
/// sealing side or decryption fails with `DecryptionFailed` (tag mismatch).
pub fn open_from_owner_with_info(
    recipient_x25519_priv: &[u8; 32],
    sealed: &[u8],
    info: &[u8],
) -> Result<Vec<u8>, DmSignError> {
    harmony_crypto::sealed_box::open(recipient_x25519_priv, sealed, info)
        .map_err(|e| map_sealed_box_err(e, DmSignError::DecryptionFailed))
}

/// Convert an Ed25519 public key to an X25519 public key via the
/// standard birational map (RFC 7748 §5). Used for sealing material
/// to recipients identified by their Ed25519 identity.
///
/// The Ed25519 curve point is the Twisted Edwards curve point y → u
/// Montgomery form: u = (1 + y) / (1 - y) mod p.
pub fn ed25519_pub_to_x25519(ed25519_pub: &[u8; 32]) -> Result<[u8; 32], DmSignError> {
    harmony_crypto::x25519::ed25519_pub_to_x25519(ed25519_pub)
        .ok_or(DmSignError::InvalidEd25519Pubkey)
}

/// Convert an Ed25519 signing key to an X25519 private key via the
/// standard derivation (RFC 7748 §5 with SHA-512 + clamping).
///
/// The Ed25519 secret scalar is derived via SHA-512 of the 32-byte
/// private seed; the first 32 bytes are the X25519 scalar candidate,
/// clamped per RFC 7748 §5 before use.
pub fn ed25519_priv_to_x25519(
    signing_key: &ed25519_dalek::SigningKey,
) -> zeroize::Zeroizing<[u8; 32]> {
    harmony_crypto::x25519::ed25519_priv_to_x25519(signing_key)
}

/// HKDF info string for ZEB-249 epoch-key sealed envelopes — the original
/// (and default) domain for `seal_to_owner`/`open_from_owner`. The byte
/// value is FROZEN: existing ZEB-249 ciphertexts in the wild derive their
/// AEAD key from it.
const ZEB_249_EPOCH_KEY_SEAL_INFO: &[u8] = b"harmony-zeb-249-epoch-key-seal";

/// Map a `harmony_crypto` sealed-box error onto this module's `DmSignError`
/// taxonomy. The explicit arms are operation-agnostic (`sealed_box::seal`/`open`
/// only ever produce these four today); `fallback` is the operation-appropriate
/// default for any future `CryptoError` variant, so a seal error never surfaces
/// as a decryption failure (or vice versa). The fixed 32-byte HKDF output length
/// can never trip `HkdfLengthExceeded`, so the fallback is currently unreachable.
fn map_sealed_box_err(e: harmony_crypto::CryptoError, fallback: DmSignError) -> DmSignError {
    use harmony_crypto::CryptoError;
    match e {
        CryptoError::AeadEncryptFailed => DmSignError::EncryptionFailed,
        CryptoError::AeadDecryptFailed => DmSignError::DecryptionFailed,
        CryptoError::CiphertextTooShort => DmSignError::MalformedSealedEnvelope,
        CryptoError::InvalidPublicKey => DmSignError::InvalidPublicKey,
        _ => fallback,
    }
}

/// Reticulum app+aspect for DM-protocol packets. The full destination
/// name is `"harmony.dm"` (app `"harmony"`, single aspect `"dm"`); see
/// `harmony_reticulum::destination::DestinationName::from_name` for the
/// canonical naming scheme. Pinned here as a constant so every consumer
/// (the drain-side `resolve_destinations` helper in `dm_outbox` and any
/// future receive-side fan-out) computes the same destination hash for any
/// given device-identity hash.
const DM_DESTINATION_FULL_NAME: &[u8] = b"harmony.dm";

/// Compute the Reticulum 16-byte destination hash for a DM packet
/// addressed to `identity_address_hash` (a 16-byte `DeviceIdentityHash`).
///
/// Per `harmony_reticulum::destination::DestinationName::destination_hash`:
///   `name_hash       = SHA256("harmony.dm")[:10]`
///   `destination_hash = SHA256(name_hash || identity_address_hash)[:16]`
///
/// We replicate the formula inline (rather than depending on the
/// `harmony-reticulum` crate, which is currently a transitive-only dep)
/// because the only call sites in harmony-client are this module and the
/// future Task 11 resolver. Pin the bytes against `harmony-reticulum`'s
/// canonical implementation via the `compute_dm_destination_hash_matches_*`
/// equivalence test in this module — if harmony-reticulum's formula ever
/// drifts, that test breaks loudly.
pub fn compute_dm_destination_hash(identity_address_hash: [u8; 16]) -> [u8; 16] {
    // name_hash = SHA256("harmony.dm")[:10]
    let mut name_hasher = Sha256::new();
    name_hasher.update(DM_DESTINATION_FULL_NAME);
    let name_full: [u8; 32] = name_hasher.finalize().into();
    let mut name_hash = [0u8; 10];
    name_hash.copy_from_slice(&name_full[..10]);

    // destination_hash = SHA256(name_hash || identity_address_hash)[:16]
    let mut dest_hasher = Sha256::new();
    dest_hasher.update(name_hash);
    dest_hasher.update(identity_address_hash);
    let dest_full: [u8; 32] = dest_hasher.finalize().into();
    let mut dest_hash = [0u8; 16];
    dest_hash.copy_from_slice(&dest_full[..16]);
    dest_hash
}

/// Compute the DeviceIdentityHash for a given 64-byte combined identity
/// public-bytes value (`X25519_pub(32) || Ed25519_pub(32)`, the canonical
/// `harmony_identity::Identity::to_public_bytes()` layout).
///
/// ZEB-548 Stage 0: relocated into `harmony_core_types::owner_state_types`
/// (where `DeviceIdentityHash` lives, so the OwnerDeviceCache deserialize
/// check can call it without depending on harmony-app). Re-exported here so
/// the existing `crate::dm_signing::derive_device_hash_from_identity_pub` call
/// sites are unchanged.
///
/// Single source of truth: delegates to
/// `harmony_identity::Identity::from_public_bytes(identity_pub).address_hash`.
/// Returns `None` if the bytes are malformed. Distinct notion from
/// `harmony_owner::PubKeyBundle::identity_hash()` (signing-only material);
/// the two must never be converged. Infallible untrusted-bytes twin:
/// `crate::community_invite::device_hash_from_identity_pub`.
pub use harmony_core_types::owner_state_types::derive_device_hash_from_identity_pub;

/// ZEB-580 S1: build the 64-byte DM combined pub (`X25519_pub(32) ‖
/// Ed25519_pub(32)`) for an enrolled device (#2) from its EnrollmentCert's
/// classical pubkeys. This is the same layout as
/// `harmony_identity::Identity::to_public_bytes()`, so its DM device hash is
/// `derive_device_hash_from_identity_pub(&combined)` and
/// `verify_dm_packet_signature` accepts it unchanged.
pub fn device2_combined_pub(cert: &harmony_owner::certs::EnrollmentCert) -> [u8; 64] {
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(&cert.device_pubkeys.classical.x25519_pub);
    out[32..].copy_from_slice(&cert.device_pubkeys.classical.ed25519_verify);
    out
}

/// ZEB-376: the device-#2 Ed25519 VERIFYING key from an EnrollmentCert (the
/// `ed25519_verify` half of the classical device pubkeys). Parallels
/// [`device2_combined_pub`], but returns just the signature-verifying half —
/// what a relayed reachability's inner-signature check needs to authenticate the
/// subject who signed it. `None` when the bytes are not a valid Ed25519 point (a
/// degenerate / pre-ZEB-372 stub cert); callers treat `None` as "no usable #2
/// key" and reject.
pub fn device2_verifying_key(
    cert: &harmony_owner::certs::EnrollmentCert,
) -> Option<ed25519_dalek::VerifyingKey> {
    ed25519_dalek::VerifyingKey::from_bytes(&cert.device_pubkeys.classical.ed25519_verify).ok()
}

/// ZEB-580 S1: the DM device hash for a device's #2 identity, or `None` when
/// the cert lacks a usable X25519 pub (all-zero pre-ZEB-372 stub or a
/// degenerate synthetic cert) or the combined pub is not a valid Identity
/// point. Callers treat `None` as "no #2 identity available" and degrade to
/// the legacy #3 path.
pub fn device2_signing_hash(
    cert: &harmony_owner::certs::EnrollmentCert,
) -> Option<DeviceIdentityHash> {
    let combined = device2_combined_pub(cert);
    if combined[..32] == [0u8; 32] {
        return None;
    }
    derive_device_hash_from_identity_pub(&combined)
}

/// Sign a Reticulum DM packet body. The signature is applied to the
/// canonical CBOR encoding of the body (which includes
/// `signing_device_hash` to prevent key-substitution attacks).
///
/// Caller computes `body_bytes` once, passes here for signing. The
/// resulting 64-byte Ed25519 signature is appended after `body_bytes`
/// in the wire packet by encode_packet (Task 5).
pub fn sign_dm_packet(body_bytes: &[u8], signing_key: &SigningKey) -> [u8; 64] {
    let sig: Signature = signing_key.sign(body_bytes);
    sig.to_bytes()
}

/// Verify a Reticulum DM packet signature.
///
/// Two-step check:
///   1. The provided `identity_pub` MUST hash to `expected_signing_device_hash`
///      (defeats key-substitution attacks where attacker presents pubkey K
///      but claims a different device's hash).
///   2. The Ed25519 signature MUST verify against bytes `[32..64]` of
///      `identity_pub` (the Ed25519 verifying key) and `body_bytes`.
///
/// `body_bytes`: canonical CBOR encoding of the signed body (NOT
/// including the discriminant byte or the appended signature).
/// `signature`: 64-byte Ed25519 signature appended after body_bytes.
/// `identity_pub`: 64-byte combined identity pubs (X25519_pub(32) ||
/// Ed25519_pub(32)) looked up by the caller (from OwnerDeviceCache's
/// device_identity_pubs parallel-vec for CidNotify/Ack post-bootstrap,
/// or from the inline `inviter_identity_pub` for DmInvite).
/// `expected_signing_device_hash`: the body's `signing_device_hash` field.
///
/// Returns Ok on success; Err on either failure mode.
pub fn verify_dm_packet_signature(
    body_bytes: &[u8],
    signature: &[u8; 64],
    identity_pub: &[u8; 64],
    expected_signing_device_hash: DeviceIdentityHash,
) -> Result<(), DmReceiveError> {
    // Step 1: derive device hash + check it matches the body's claim.
    let computed_hash = derive_device_hash_from_identity_pub(identity_pub)
        .ok_or(DmReceiveError::SignatureVerificationFailed)?;
    if computed_hash != expected_signing_device_hash {
        return Err(DmReceiveError::SigningKeyDoesNotMatchDeviceHash);
    }
    // Step 2: extract Ed25519 verifying key from second half + verify signature.
    let ed25519_pub_bytes: [u8; 32] = identity_pub[32..64].try_into().expect("64 - 32 == 32");
    let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&ed25519_pub_bytes)
        .map_err(|_| DmReceiveError::SignatureVerificationFailed)?;
    let sig = Signature::from_bytes(signature);
    verifying_key
        .verify(body_bytes, &sig)
        .map_err(|_| DmReceiveError::SignatureVerificationFailed)?;
    Ok(())
}

#[cfg(test)]
mod ed25519_x25519_conversion_tests {
    use super::*;

    #[test]
    fn ed25519_to_x25519_round_trip_via_seal() {
        let signing = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let verifying = signing.verifying_key();
        let x_pub = ed25519_pub_to_x25519(&verifying.to_bytes()).expect("conversion");
        let x_priv = ed25519_priv_to_x25519(&signing);

        let payload = b"some 32-byte payload xxxxxxxxxxx";
        let sealed = seal_to_owner(&x_pub, payload).expect("seal");
        let opened = open_from_owner(&x_priv, &sealed).expect("open");
        assert_eq!(opened, payload);
    }

    #[test]
    fn ed25519_pub_to_x25519_deterministic() {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32]);
        let verifying = signing.verifying_key();
        let x1 = ed25519_pub_to_x25519(&verifying.to_bytes()).expect("first");
        let x2 = ed25519_pub_to_x25519(&verifying.to_bytes()).expect("second");
        assert_eq!(x1, x2);
    }

    #[test]
    fn ed25519_priv_to_x25519_deterministic() {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32]);
        let x1 = ed25519_priv_to_x25519(&signing);
        let x2 = ed25519_priv_to_x25519(&signing);
        assert_eq!(x1, x2);
    }

    /// C2: ed25519_pub_to_x25519 must reject small-order (torsion) Edwards
    /// points. The identity point on the Twisted Edwards curve is
    /// `(0, 1)` which in compressed form is the byte `0x01` followed by
    /// 31 zero bytes. This is a small-order point (order 1 in the
    /// cofactor-8 group) and MUST NOT be converted to a Montgomery form
    /// for use in ECDH.
    #[test]
    fn ed25519_pub_to_x25519_rejects_small_order_identity_point() {
        // Compressed Edwards Y for the identity point: y=1, sign bit clear.
        // In little-endian: 0x01 || [0x00; 31].
        let mut identity_point = [0u8; 32];
        identity_point[0] = 0x01;
        let err = ed25519_pub_to_x25519(&identity_point)
            .expect_err("ed25519_pub_to_x25519 must reject the small-order identity point");
        assert!(
            matches!(err, DmSignError::InvalidEd25519Pubkey),
            "expected InvalidEd25519Pubkey for identity point, got {err:?}"
        );
    }
}

#[cfg(test)]
mod epoch_seal_tests {
    use super::*;

    /// Build a test X25519 keypair from an Ed25519 seed for use in seal
    /// round-trip tests. Returns (x25519_private_bytes, x25519_public_bytes).
    fn make_x25519_keypair(seed_byte: u8) -> ([u8; 32], [u8; 32]) {
        // Derive an Ed25519 signing key from seed, then convert the scalar
        // to an X25519 static secret. x25519-dalek StaticSecret accepts
        // the raw 32 bytes directly.
        use hkdf::Hkdf;
        use sha2::Sha256;
        use x25519_dalek::{PublicKey, StaticSecret};

        let seed = [seed_byte; 32];
        let hk = Hkdf::<Sha256>::new(None, &seed);
        let mut scalar = [0u8; 32];
        hk.expand(b"harmony-zeb-249-test-x25519-scalar", &mut scalar)
            .expect("HKDF 32 bytes always works");

        let secret = StaticSecret::from(scalar);
        let public = PublicKey::from(&secret);
        (scalar, *public.as_bytes())
    }

    #[test]
    fn seal_and_open_round_trip() {
        let (priv_bytes, pub_bytes) = make_x25519_keypair(0x01);
        let plaintext = [0xde_u8; 32];
        let sealed = seal_to_owner(&pub_bytes, &plaintext).expect("seal must succeed");
        // Expected layout: 32 ephemeral_pub + 12 nonce + (32 + 16 tag) = 92 bytes.
        assert_eq!(
            sealed.len(),
            92,
            "sealed length must be 92 for 32-byte plaintext"
        );
        let recovered = open_from_owner(&priv_bytes, &sealed).expect("open must succeed");
        assert_eq!(
            recovered, plaintext,
            "recovered plaintext must match original"
        );
    }

    #[test]
    fn open_with_wrong_key_fails() {
        let (_priv1, pub1) = make_x25519_keypair(0x01);
        let (priv2, _pub2) = make_x25519_keypair(0x02);
        let plaintext = b"wrong key test payload";
        let sealed = seal_to_owner(&pub1, plaintext).expect("seal must succeed");
        let err = open_from_owner(&priv2, &sealed).expect_err("opening with wrong key must fail");
        assert!(
            matches!(err, DmSignError::DecryptionFailed),
            "expected DecryptionFailed, got {err:?}"
        );
    }

    /// C1: seal_to_owner must reject a low-order (all-zero) recipient X25519
    /// pubkey. The all-zero X25519 point is a small-order (torsion) point;
    /// ECDH with it yields an all-zero shared secret, making the derived AEAD
    /// key predictable regardless of the ephemeral scalar.
    #[test]
    fn seal_to_owner_rejects_low_order_x25519_pubkey() {
        // The all-zero X25519 pubkey is a well-known small-order point.
        let low_order_pub = [0u8; 32];
        let plaintext = b"low-order attack test";
        let err = seal_to_owner(&low_order_pub, plaintext)
            .expect_err("seal_to_owner must reject a low-order (all-zero) X25519 pubkey");
        assert!(
            matches!(err, DmSignError::InvalidPublicKey),
            "expected InvalidPublicKey, got {err:?}"
        );
    }

    /// C1: open_from_owner must reject a sealed blob whose embedded ephemeral
    /// pubkey is the all-zero (low-order) X25519 point. Craft a blob
    /// with [0; 32] as the ephemeral pub and fill the rest with zeros;
    /// the shared secret would be all-zero, which we must reject before
    /// reaching the AEAD layer.
    #[test]
    fn open_from_owner_rejects_low_order_ephemeral_pubkey() {
        let (priv_bytes, _pub_bytes) = make_x25519_keypair(0x01);
        // Craft a fake sealed blob: 32-byte all-zero ephemeral pub + 12-byte
        // nonce + at least 16-byte ciphertext+tag (all zeros).
        let mut fake_sealed = vec![0u8; 32 + 12 + 16];
        // ephemeral pub bytes [0..32] are already zero — the low-order point.
        // nonce [32..44] and ciphertext+tag [44..] are zero too.
        fake_sealed[0..32].fill(0);
        let err = open_from_owner(&priv_bytes, &fake_sealed)
            .expect_err("open_from_owner must reject a low-order ephemeral X25519 pubkey");
        assert!(
            matches!(err, DmSignError::InvalidPublicKey),
            "expected InvalidPublicKey, got {err:?}"
        );
    }

    /// ZEB-738 cross-repo byte-preservation anchor. This is the SAME frozen
    /// envelope asserted by harmony-crypto's `sealed_box::tests::frozen_open_kat`
    /// (recipient derived from Ed25519 seed `[0x24; 32]`, info = the zeb-249
    /// default). Decrypting it here proves the client's delegating `open` path
    /// recovers byte-identical plaintext to core's — pinning framing offsets,
    /// the HKDF schedule, and the ChaCha20-Poly1305 layer identical across both
    /// crates. DO NOT regenerate: it anchors the sealed-blob wire format.
    #[test]
    fn zeb738_frozen_sealed_envelope_opens_cross_repo() {
        let sk = SigningKey::from_bytes(&[0x24u8; 32]);
        let recipient_priv = *ed25519_priv_to_x25519(&sk);
        const FROZEN_ENVELOPE: &str = "0021bf9fce0c9b89eb3cf5f4c77cefa61c97cde1a8000902a9f86f03dc53bc158188f93da1cff420a0dda47f0b533087cc2812a74aaefe84df65cfe51315577cf0cecb77a5bc86d85ee14bdabfd0278e014adc2126a821557947423eaae99e177c97cf069c0fc6";
        const EXPECTED_PLAINTEXT: &[u8] = b"harmony sealed-box known-answer test vector";
        let sealed = hex::decode(FROZEN_ENVELOPE).expect("valid hex fixture");
        let opened = open_from_owner(&recipient_priv, &sealed)
            .expect("client open must recover the core-sealed envelope");
        assert_eq!(
            opened, EXPECTED_PLAINTEXT,
            "cross-repo sealed-box decrypt path must be byte-identical"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test fixture API note: `harmony_identity::PrivateIdentity` does NOT
    /// expose a public `signing_key()` accessor (the field is private and
    /// the struct is `ZeroizeOnDrop`). It DOES expose `sign(&[u8]) -> [u8; 64]`
    /// (identity.rs:268), which internally calls `self.signing_key.sign(msg).to_bytes()`
    /// — bit-identical to what `sign_dm_packet` produces against the same
    /// SigningKey. Most tests therefore use `private.sign(body)` to obtain
    /// signatures, which still exercises the verification path end-to-end.
    ///
    /// The dedicated `sign_dm_packet_matches_private_identity_sign` test
    /// (below) covers `sign_dm_packet` directly by constructing a SigningKey
    /// from a known seed and asserting bit-equality with PrivateIdentity::sign
    /// derived from the same Ed25519 seed bytes — pinning the equivalence so
    /// any future drift in either path is caught.
    fn make_test_identity(
        seed_byte: u8,
    ) -> (
        harmony_identity::PrivateIdentity,
        [u8; 64],
        DeviceIdentityHash,
    ) {
        let seed = [seed_byte; 32];
        let private = harmony_identity::PrivateIdentity::from_seed(&seed);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);
        (private, identity_pub, device_hash)
    }

    #[test]
    fn sign_then_verify_round_trip() {
        let (private, identity_pub, device_hash) = make_test_identity(0x42);
        let body = b"hello world body bytes";
        let sig = private.sign(body);
        assert!(verify_dm_packet_signature(body, &sig, &identity_pub, device_hash).is_ok());
    }

    #[test]
    fn verify_tampered_body_rejects() {
        let (private, identity_pub, device_hash) = make_test_identity(0x42);
        let body = b"hello world body bytes";
        let sig = private.sign(body);
        let mut tampered = body.to_vec();
        tampered[0] ^= 0xff;
        let err =
            verify_dm_packet_signature(&tampered, &sig, &identity_pub, device_hash).unwrap_err();
        assert!(matches!(err, DmReceiveError::SignatureVerificationFailed));
    }

    #[test]
    fn verify_wrong_signing_key_rejects() {
        let (private1, _, _) = make_test_identity(0x42);
        let (_, identity_pub_2, device_hash_2) = make_test_identity(0x99);
        let body = b"hello world body bytes";
        // Sign with identity 1's key.
        let sig = private1.sign(body);
        // Verify with identity_pub_2 + claim its device hash → first check
        // passes (identity_pub_2 hashes to device_hash_2), then signature
        // verification fails (sk1's signature doesn't verify under
        // identity_pub_2's Ed25519 half).
        let err =
            verify_dm_packet_signature(body, &sig, &identity_pub_2, device_hash_2).unwrap_err();
        assert!(matches!(err, DmReceiveError::SignatureVerificationFailed));
    }

    #[test]
    fn verify_pubkey_does_not_match_device_hash_rejects() {
        let (private1, identity_pub_1, _) = make_test_identity(0x42);
        let (_, _, device_hash_2) = make_test_identity(0x99);
        let body = b"hello world body bytes";
        let sig = private1.sign(body);
        // Present identity_pub_1 but claim device_hash_2 (which is for
        // a different identity). Key-substitution attack defense: this
        // MUST reject before even attempting signature verification.
        let err =
            verify_dm_packet_signature(body, &sig, &identity_pub_1, device_hash_2).unwrap_err();
        assert!(matches!(
            err,
            DmReceiveError::SigningKeyDoesNotMatchDeviceHash
        ));
    }

    #[test]
    fn derive_device_hash_is_deterministic() {
        let (_, identity_pub, _) = make_test_identity(0x42);
        let h1 = derive_device_hash_from_identity_pub(&identity_pub).unwrap();
        let h2 = derive_device_hash_from_identity_pub(&identity_pub).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn derive_device_hash_differs_per_identity() {
        let (_, ip1, _) = make_test_identity(0x11);
        let (_, ip2, _) = make_test_identity(0x22);
        assert_ne!(
            derive_device_hash_from_identity_pub(&ip1).unwrap(),
            derive_device_hash_from_identity_pub(&ip2).unwrap()
        );
    }

    // NOTE: a "malformed_rejects" test was originally planned here
    // (asserting `derive_device_hash_from_identity_pub(&[0u8; 64]).is_none()`).
    // It was dropped because empirically
    // `harmony_identity::Identity::from_public_bytes(&[0u8; 64])` actually
    // SUCCEEDS — `ed25519_dalek::VerifyingKey::from_bytes` in the version
    // pinned by harmony does not reject the all-zero (low-order / identity)
    // point at construction time (it only validates point membership lazily
    // during verify). Constructing a 64-byte input that ed25519-dalek
    // rejects at decode time is non-trivial without reaching into
    // curve-encoding internals, and the verify-step rejection is what
    // matters in practice anyway: any signature presented under a
    // pathologically chosen key still has to verify against `body_bytes`,
    // and the Step-1 device-hash check in `verify_dm_packet_signature`
    // independently binds the pubkey to the claimed device hash.
    //
    // The Step-1 (key-substitution) and Step-2 (signature) defenses are
    // covered by `verify_pubkey_does_not_match_device_hash_rejects` and
    // `verify_wrong_signing_key_rejects` respectively, so the security
    // surface is still pinned even without an explicit malformed-bytes
    // test on `derive_device_hash_from_identity_pub`.

    /// CRITICAL equivalence test: derive_device_hash_from_identity_pub
    /// MUST agree with harmony_identity::PrivateIdentity::public_identity().address_hash
    /// for the same identity. If this ever fails, signature verification on
    /// inbound DM packets will silently break (the device_hash claimed by a
    /// peer's signing_device_hash field won't match the hash derived from
    /// their cached identity_pub, so SigningKeyDoesNotMatchDeviceHash fires
    /// even for legitimate packets).
    ///
    /// Direct delegation to Identity::from_public_bytes makes this near-
    /// trivially true, but the test pins it explicitly so a future refactor
    /// (e.g., re-implementing the hash formula here for "performance") can't
    /// regress the equivalence silently.
    #[test]
    fn derive_device_hash_equals_harmony_identity_address_hash() {
        let seed = [0xabu8; 32];
        let private = harmony_identity::PrivateIdentity::from_seed(&seed);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let our_hash = derive_device_hash_from_identity_pub(&identity_pub).unwrap();
        assert_eq!(
            our_hash.0,
            public.address_hash,
            "derive_device_hash_from_identity_pub MUST match harmony_identity::Identity::address_hash. \
             If this fails, signature verification on inbound DM packets will silently break."
        );
    }

    /// Pin `compute_dm_destination_hash` against a hand-computed reference.
    /// Formula per `harmony_reticulum::destination::DestinationName`:
    ///   name_hash        = SHA256("harmony.dm")[:10]
    ///   destination_hash = SHA256(name_hash || identity_address_hash)[:16]
    /// Verifying the inline replica matches a re-derivation against the same
    /// input pins the helper bytes — the formula is transport-agnostic (ZEB-474:
    /// renamed from compute_dm_destination_hash_matches_reticulum_formula).
    #[test]
    fn compute_dm_destination_hash_matches_pinned_formula() {
        use sha2::{Digest, Sha256};
        let identity_hash = [0xabu8; 16];

        // Reproduce the formula directly here.
        let mut name_hasher = Sha256::new();
        name_hasher.update(b"harmony.dm");
        let name_full: [u8; 32] = name_hasher.finalize().into();
        let mut name_hash = [0u8; 10];
        name_hash.copy_from_slice(&name_full[..10]);

        let mut dest_hasher = Sha256::new();
        dest_hasher.update(name_hash);
        dest_hasher.update(identity_hash);
        let dest_full: [u8; 32] = dest_hasher.finalize().into();
        let mut expected = [0u8; 16];
        expected.copy_from_slice(&dest_full[..16]);

        let actual = compute_dm_destination_hash(identity_hash);
        assert_eq!(
            actual, expected,
            "compute_dm_destination_hash must match SHA256(SHA256(\"harmony.dm\")[:10] || identity_hash)[:16]"
        );
    }

    #[test]
    fn compute_dm_destination_hash_is_deterministic_per_identity() {
        let h1 = compute_dm_destination_hash([0x42; 16]);
        let h2 = compute_dm_destination_hash([0x42; 16]);
        assert_eq!(h1, h2);
    }

    #[test]
    fn compute_dm_destination_hash_differs_per_identity() {
        let h1 = compute_dm_destination_hash([0x11; 16]);
        let h2 = compute_dm_destination_hash([0x22; 16]);
        assert_ne!(h1, h2);
    }

    /// Pin that `sign_dm_packet(body, &sk)` is bit-identical to
    /// `PrivateIdentity::sign(body)` when `sk` is the same Ed25519
    /// signing key the PrivateIdentity holds internally.
    ///
    /// `PrivateIdentity::from_seed` HKDF-expands the master seed with
    /// info=`harmony-identity-ed25519-v1` to derive the Ed25519 sub-key
    /// (identity.rs:197-217). This test mirrors that derivation locally
    /// to obtain the same SigningKey, then asserts both signing paths
    /// produce identical 64-byte outputs over the same body. Ed25519 is
    /// deterministic per RFC 8032, so the equality must hold byte-for-byte.
    ///
    /// Why this matters: the rest of the test suite signs via
    /// `PrivateIdentity::sign` for ergonomics (no signing_key() accessor
    /// exposed). This test is the single direct invocation of
    /// `sign_dm_packet`, ensuring it's actually exercised and that the
    /// PrivateIdentity-based tests aren't silently bypassing it.
    #[test]
    fn sign_dm_packet_matches_private_identity_sign() {
        // Mirror PrivateIdentity::from_seed's Ed25519 sub-key derivation via
        // the SAME core primitive it uses (harmony-identity identity.rs:197 →
        // harmony_crypto::hkdf::DerivedKey), so the mirror stays byte-exact
        // even if core's HKDF invocation details evolve. (harmony-crypto is a
        // direct dep since ZEB-716.)
        let seed = [0x42u8; 32];
        let dk =
            harmony_crypto::hkdf::DerivedKey::new(&seed, None, b"harmony-identity-ed25519-v1", 32)
                .expect("HKDF length 32 within SHA-256 limit");
        let mut ed_arr = [0u8; 32];
        ed_arr.copy_from_slice(dk.as_bytes());
        let signing_key = SigningKey::from_bytes(&ed_arr);

        let private = harmony_identity::PrivateIdentity::from_seed(&seed);

        let body = b"sign_dm_packet equivalence body";
        let sig_via_module = sign_dm_packet(body, &signing_key);
        let sig_via_identity = private.sign(body);
        assert_eq!(
            sig_via_module, sig_via_identity,
            "sign_dm_packet must produce identical bytes to PrivateIdentity::sign for the same key + body \
             (Ed25519 is deterministic per RFC 8032)"
        );

        // And the produced signature must verify.
        let identity_pub = private.public_identity().to_public_bytes();
        let device_hash = DeviceIdentityHash(private.public_identity().address_hash);
        assert!(
            verify_dm_packet_signature(body, &sig_via_module, &identity_pub, device_hash).is_ok()
        );
    }

    /// ZEB-372 Phase 2 — THE cross-repo proof the ticket exists for: the
    /// pinned harmony rev populates `EnrollmentCert.device_pubkeys.classical
    /// .x25519_pub` with the real birational X25519, this client seals to
    /// that exact cert field, and the device opens with
    /// `ed25519_priv_to_x25519(device_signing_key)`. If this fails after a
    /// pin bump, the two repos' derivations have drifted — that is a
    /// sealed-blob-orphaning compat break, not a fixture to refresh.
    /// Select the enrollment cert belonging to `minted.device_signing_key`
    /// (NOT `.values().next()` — if `mint_owner` ever enrolls more than one
    /// device, an arbitrary pick would validate the wrong cert and the
    /// round-trip below would fail confusingly, or worse, pass against a
    /// cert the device key can't actually open for). Qodo, PR #220.
    fn minted_device_cert(
        minted: &harmony_owner::lifecycle::MintResult,
    ) -> &harmony_owner::certs::EnrollmentCert {
        let device_vk = minted.device_signing_key.verifying_key().to_bytes();
        minted
            .state
            .enrollments
            .values()
            .find(|c| c.device_pubkeys.classical.ed25519_verify == device_vk)
            .expect("an enrollment cert for the minted device signing key")
    }

    #[test]
    fn zeb372_cert_x25519_seals_and_device_key_opens() {
        let minted = harmony_owner::lifecycle::mint_owner(1_700_000_000).expect("mint fresh owner");
        let cert = minted_device_cert(&minted);
        let cert_x = cert.device_pubkeys.classical.x25519_pub;
        assert_ne!(
            cert_x, [0u8; 32],
            "pinned harmony rev still ships the pre-ZEB-372 zeroed X25519 stub"
        );

        let msg = b"ZEB-372 phase 2: seal to the cert key, open with the device key";
        let sealed = seal_to_owner(&cert_x, msg).expect("seal to cert-carried X25519");
        let device_x_priv = ed25519_priv_to_x25519(&minted.device_signing_key);
        let opened = open_from_owner(&device_x_priv, &sealed).expect("device key opens");
        assert_eq!(opened, msg);
    }

    /// ZEB-372 Phase 2 parity pin: harmony-owner's `ed25519_pub_to_x25519`
    /// and this module's implementation must agree forever, for both the
    /// device bundle and the master bundle (read from the device cert's
    /// `Master` issuer). Guards the same drift as the round-trip test but
    /// localizes a failure to the derivation rather than the seal path.
    #[test]
    fn zeb372_cert_x25519_matches_client_birational_derivation() {
        let minted = harmony_owner::lifecycle::mint_owner(1_700_000_000).expect("mint fresh owner");
        let cert = minted_device_cert(&minted);

        let bundles = [
            ("device", &cert.device_pubkeys),
            (
                "master",
                match &cert.issuer {
                    harmony_owner::certs::EnrollmentIssuer::Master { master_pubkey } => {
                        master_pubkey
                    }
                    other => panic!("device #1 cert must be Master-issued, got {other:?}"),
                },
            ),
        ];
        for (which, bundle) in bundles {
            let expected = ed25519_pub_to_x25519(&bundle.classical.ed25519_verify)
                .expect("freshly minted key converts");
            assert_eq!(
                bundle.classical.x25519_pub, expected,
                "{which} bundle: harmony-owner and harmony-client birational \
                 implementations disagree — cross-repo derivation drift"
            );
        }
    }

    /// ZEB-580 S1: the #2 combined pub is x25519_pub ‖ ed25519_verify from
    /// the cert, and its DM hash differs from the same device's #3 hash.
    #[test]
    fn device2_combined_pub_and_hash_from_mint() {
        let minted = harmony_owner::lifecycle::mint_owner(1_700_000_000).expect("mint");
        let cert = minted
            .state
            .enrollments
            .values()
            .find(|c| {
                c.device_pubkeys.classical.ed25519_verify
                    == minted.device_signing_key.verifying_key().to_bytes()
            })
            .expect("device cert");

        let combined = device2_combined_pub(cert);
        assert_eq!(&combined[..32], &cert.device_pubkeys.classical.x25519_pub);
        assert_eq!(
            &combined[32..],
            &cert.device_pubkeys.classical.ed25519_verify
        );

        let h2 = device2_signing_hash(cert).expect("real cert yields a #2 hash");
        // Deterministic + equals the direct derivation.
        assert_eq!(h2, derive_device_hash_from_identity_pub(&combined).unwrap());
    }

    /// A cert with an all-zero X25519 half (the pre-ZEB-372 stub / a
    /// degenerate synthetic cert) yields no usable #2 identity — callers
    /// must degrade rather than cache a degenerate combined pub.
    #[test]
    fn device2_signing_hash_rejects_zeroed_x25519() {
        let minted = harmony_owner::lifecycle::mint_owner(1_700_000_000).expect("mint");
        let mut cert = minted.state.enrollments.values().next().unwrap().clone();
        cert.device_pubkeys.classical.x25519_pub = [0u8; 32];
        assert!(device2_signing_hash(&cert).is_none());
    }
}
