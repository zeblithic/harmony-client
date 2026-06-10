//! ZEB-418 SP2 Phase 1: butler deposit wire module — frame + ack + sealed
//! payload types for the `harmony/butler-deposit/v1` ALPN, plus u32-LE
//! length-prefixed framing helpers.
//!
//! A sender that cannot reach a recipient's device directly deposits the
//! sealed DM with an online sibling device of the recipient (the "butler"),
//! which persists it into the `dm-inbox-v1` fleet dataset and acks. See
//! `docs/specs/2026-06-09-zeb-418-sp2-butler-design.md` §4.
//!
//! Wire conventions (mirrors `dm_envelope`):
//! * Two-character serde renames so each struct's keys are the same encoded
//!   length at a single nesting level — the same-length-keys precondition
//!   documented on `crate::owner_state_crypto::canonical_cbor_encode`.
//! * Canonical CBOR via `canonical_cbor_encode` / `canonical_cbor_decode`
//!   (trailing bytes rejected on decode).
//! * The sealed envelope is `dm_signing`'s shipped sealed-ECDH construction
//!   byte-for-byte (`32-byte ephemeral X25519 ‖ 12-byte nonce ‖ ct+tag`),
//!   domain-separated with the new HKDF info string
//!   [`BUTLER_DEPOSIT_SEAL_INFO`] via `seal_to_owner_with_info` /
//!   `open_from_owner_with_info`.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::owner_state_crypto::{
    canonical_cbor_decode, canonical_cbor_encode, sealed::CanonicalPayloadSealed, CanonicalPayload,
};

/// Domain-separation prefix for the deposit frame signature. The sender's
/// cert-bound device ed25519 signs `domain ‖ recipient_owner ‖ sealed_blob`
/// (see [`deposit_sig_payload`] — the single shared construction for signer
/// and verifier).
pub const BUTLER_DEPOSIT_SIG_DOMAIN: &[u8] = b"harmony-zeb-418-deposit-sig-v1";

/// HKDF info string for the deposit sealed envelope. Distinct from the
/// ZEB-249 epoch-key-seal info so a ciphertext sealed for one context can
/// never be opened in the other (key-derivation domain separation).
pub const BUTLER_DEPOSIT_SEAL_INFO: &[u8] = b"harmony-zeb-418-butler-deposit-v1";

/// Cap on a single length-prefixed frame body (either direction). Any u32
/// length prefix exceeding this is rejected BEFORE the body is read or
/// allocated.
pub const DEPOSIT_MAX_FRAME_BYTES: usize = 256 * 1024;

/// The sender → butler deposit request body (canonical CBOR, sent inside a
/// length-prefixed frame). Spec §4: the butler verifies the enrollment cert
/// chain, the `sig`, and admission (friend graph + quota) BEFORE any
/// decryption of `sealed_blob`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DepositFrame {
    /// Recipient OwnerAddr bytes — whose inbox this deposit is for.
    #[serde(rename = "ro")]
    pub recipient_owner: [u8; 16],
    /// Sender OwnerAddr bytes — admission-checked against the butler's
    /// friend graph.
    #[serde(rename = "so")]
    pub sender_owner: [u8; 16],
    /// Canonical CBOR of the sender device's `EnrollmentCert`, verified
    /// against the sender owner's master key.
    #[serde(rename = "ec")]
    pub sender_enrollment_cert: Vec<u8>,
    /// 64-byte device ed25519 signature over
    /// `BUTLER_DEPOSIT_SIG_DOMAIN ‖ recipient_owner ‖ sealed_blob`
    /// ([`deposit_sig_payload`]).
    #[serde(rename = "sg")]
    pub sig: Vec<u8>,
    /// [`DepositPayload`] canonical CBOR, sealed to the butler device's
    /// birational X25519 key with [`BUTLER_DEPOSIT_SEAL_INFO`].
    #[serde(rename = "sb")]
    pub sealed_blob: Vec<u8>,
}

/// The butler → sender acknowledgement, returned only AFTER the deposit is
/// persisted into `dm-inbox-v1` (spec §4 step 7: persist-then-ack).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DepositAck {
    #[serde(rename = "sp")]
    pub space_id: [u8; 16],
    #[serde(rename = "mc")]
    pub message_cid: Vec<u8>,
}

/// Plaintext inside `sealed_blob` (sealed to birational(vk) of the butler
/// device): the full signed CidNotify packet plus the CAS storage blob —
/// exactly what the recipient's normal DM receive path consumes on ingestion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DepositPayload {
    /// Full signed CidNotify packet bytes (discriminant+body+sig).
    #[serde(rename = "cn")]
    pub cidnotify_packet: Vec<u8>,
    /// The CAS storage blob ([ver][nonce][ct][tag]).
    #[serde(rename = "pl")]
    pub storage_blob: Vec<u8>,
}

// Manual CanonicalPayload registration (the `impl_canonical!` macro in
// owner_state_types.rs is module-private; mirror `dm_inbox_crdt`).
impl CanonicalPayloadSealed for DepositFrame {}
impl CanonicalPayload for DepositFrame {}
impl CanonicalPayloadSealed for DepositAck {}
impl CanonicalPayload for DepositAck {}
impl CanonicalPayloadSealed for DepositPayload {}
impl CanonicalPayload for DepositPayload {}

/// Errors from the deposit wire layer (codec + framing).
#[derive(Debug, thiserror::Error)]
pub enum DepositWireError {
    /// Length prefix of zero or exceeding [`DEPOSIT_MAX_FRAME_BYTES`].
    /// Raised BEFORE any body byte is read or allocated.
    #[error("deposit frame length {len} out of bounds (min 1, max {max})")]
    FrameOutOfBounds { len: usize, max: usize },
    #[error("deposit frame I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("deposit CBOR encode: {0}")]
    Encode(String),
    #[error("deposit CBOR decode: {0}")]
    Decode(String),
}

/// The single shared signature construction for deposit frames:
/// `BUTLER_DEPOSIT_SIG_DOMAIN ‖ recipient_owner ‖ sealed_blob`. Both the
/// sender (signer) and the butler (verifier) MUST build the signed bytes
/// through this function — never inline the concatenation.
pub fn deposit_sig_payload(recipient_owner: &[u8; 16], sealed_blob: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        BUTLER_DEPOSIT_SIG_DOMAIN.len() + recipient_owner.len() + sealed_blob.len(),
    );
    out.extend_from_slice(BUTLER_DEPOSIT_SIG_DOMAIN);
    out.extend_from_slice(recipient_owner);
    out.extend_from_slice(sealed_blob);
    out
}

/// Encode a [`DepositFrame`] to canonical CBOR (no length prefix — the
/// caller frames it with [`write_length_prefixed`]).
pub fn encode_deposit_frame(frame: &DepositFrame) -> Result<Vec<u8>, DepositWireError> {
    canonical_cbor_encode(frame).map_err(|e| DepositWireError::Encode(format!("{e}")))
}

/// Decode a [`DepositFrame`] from canonical CBOR (trailing bytes rejected).
pub fn decode_deposit_frame(bytes: &[u8]) -> Result<DepositFrame, DepositWireError> {
    canonical_cbor_decode(bytes).map_err(|e| DepositWireError::Decode(format!("{e}")))
}

/// Encode a [`DepositAck`] to canonical CBOR (no length prefix).
pub fn encode_deposit_ack(ack: &DepositAck) -> Result<Vec<u8>, DepositWireError> {
    canonical_cbor_encode(ack).map_err(|e| DepositWireError::Encode(format!("{e}")))
}

/// Decode a [`DepositAck`] from canonical CBOR (trailing bytes rejected).
pub fn decode_deposit_ack(bytes: &[u8]) -> Result<DepositAck, DepositWireError> {
    canonical_cbor_decode(bytes).map_err(|e| DepositWireError::Decode(format!("{e}")))
}

/// Encode a [`DepositPayload`] to canonical CBOR (the plaintext that gets
/// sealed into `DepositFrame::sealed_blob`).
pub fn encode_deposit_payload(payload: &DepositPayload) -> Result<Vec<u8>, DepositWireError> {
    canonical_cbor_encode(payload).map_err(|e| DepositWireError::Encode(format!("{e}")))
}

/// Decode a [`DepositPayload`] from canonical CBOR (trailing bytes rejected).
pub fn decode_deposit_payload(bytes: &[u8]) -> Result<DepositPayload, DepositWireError> {
    canonical_cbor_decode(bytes).map_err(|e| DepositWireError::Decode(format!("{e}")))
}

/// Write `body` as a `[u32 LE length-prefix][body]` frame. Rejects bodies
/// exceeding [`DEPOSIT_MAX_FRAME_BYTES`] (or empty) before writing anything.
pub async fn write_length_prefixed<W: AsyncWrite + Unpin>(
    writer: &mut W,
    body: &[u8],
) -> Result<(), DepositWireError> {
    if body.is_empty() || body.len() > DEPOSIT_MAX_FRAME_BYTES {
        return Err(DepositWireError::FrameOutOfBounds {
            len: body.len(),
            max: DEPOSIT_MAX_FRAME_BYTES,
        });
    }
    // Cast is lossless: body.len() <= DEPOSIT_MAX_FRAME_BYTES << u32::MAX.
    writer.write_all(&(body.len() as u32).to_le_bytes()).await?;
    writer.write_all(body).await?;
    Ok(())
}

/// Read a `[u32 LE length-prefix][body]` frame. A prefix of zero or
/// exceeding [`DEPOSIT_MAX_FRAME_BYTES`] is rejected BEFORE the body is
/// read or allocated — an attacker-supplied prefix never drives an
/// allocation past the cap.
pub async fn read_length_prefixed<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Vec<u8>, DepositWireError> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > DEPOSIT_MAX_FRAME_BYTES {
        return Err(DepositWireError::FrameOutOfBounds {
            len,
            max: DEPOSIT_MAX_FRAME_BYTES,
        });
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dm_signing::{open_from_owner, open_from_owner_with_info, seal_to_owner_with_info};

    /// Deterministic test X25519 keypair (mirrors `dm_signing`'s
    /// `epoch_seal_tests::make_x25519_keypair`). Returns (priv, pub).
    fn make_x25519_keypair(seed_byte: u8) -> ([u8; 32], [u8; 32]) {
        use hkdf::Hkdf;
        use sha2::Sha256;
        use x25519_dalek::{PublicKey, StaticSecret};

        let seed = [seed_byte; 32];
        let hk = Hkdf::<Sha256>::new(None, &seed);
        let mut scalar = [0u8; 32];
        hk.expand(b"harmony-zeb-418-test-x25519-scalar", &mut scalar)
            .expect("HKDF 32 bytes always works");

        let secret = StaticSecret::from(scalar);
        let public = PublicKey::from(&secret);
        (scalar, *public.as_bytes())
    }

    /// A fully deterministic DepositFrame for the byte-pin fixture: fixed
    /// repeated-byte addresses, a fixed fake cert blob, and a fixed
    /// `sig: [0x09; 64]` (pins BYTE LAYOUT only, not a real signature —
    /// mirrors the wire_format_zeb375 fixture discipline).
    fn fixture_frame() -> DepositFrame {
        DepositFrame {
            recipient_owner: [0x11; 16],
            sender_owner: [0x22; 16],
            sender_enrollment_cert: vec![0xE1, 0xE2, 0xE3, 0xE4],
            sig: vec![0x09; 64],
            sealed_blob: vec![0x5B, 0x5C, 0x5D],
        }
    }

    #[test]
    fn deposit_frame_cbor_round_trips() {
        let frame = fixture_frame();
        let bytes = encode_deposit_frame(&frame).expect("encode");
        let back = decode_deposit_frame(&bytes).expect("decode");
        assert_eq!(back, frame);

        // Canonical-decode rejects trailing bytes (fingerprint safety).
        let mut padded = bytes.clone();
        padded.push(0x00);
        assert!(matches!(
            decode_deposit_frame(&padded),
            Err(DepositWireError::Decode(_))
        ));
    }

    /// Byte-pin the DepositFrame wire encoding. A failure here is a
    /// wire-protocol break (field rename, key-length change, reorder,
    /// encoding-shape change) — review carefully before updating the pinned
    /// hex (cross-version compat: deployed senders/butlers must agree).
    ///
    /// DO NOT REGENERATE casually: the hex below was captured from the
    /// FIRST VERIFIED RUN of this test (run once with "FILL_AFTER", paste
    /// the actual hex from the panic message). Any change to it after that
    /// first pin means the wire format changed and old peers break.
    #[test]
    fn deposit_frame_wire_fixture_pinned() {
        const EXPECTED_DEPOSIT_FRAME_HEX: &str = "a562726f901111111111111111111111111111111162736f9018221822182218221822182218221822182218221822182218221822182218226265638418e118e218e318e462736798400909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090962736283185b185c185d";
        let bytes = encode_deposit_frame(&fixture_frame()).expect("encode");
        let actual = hex::encode(&bytes);
        assert_eq!(
            actual, EXPECTED_DEPOSIT_FRAME_HEX,
            "DepositFrame wire encoding drifted from the pinned fixture.\nactual hex: {actual}"
        );
    }

    #[test]
    fn ack_round_trips() {
        let ack = DepositAck {
            space_id: [0x33; 16],
            message_cid: vec![0xCD; 32],
        };
        let bytes = encode_deposit_ack(&ack).expect("encode");
        let back = decode_deposit_ack(&bytes).expect("decode");
        assert_eq!(back, ack);
    }

    #[test]
    fn deposit_payload_round_trips() {
        let payload = DepositPayload {
            cidnotify_packet: vec![0x01, 0x02, 0x03],
            storage_blob: vec![0x04, 0x05, 0x06, 0x07],
        };
        let bytes = encode_deposit_payload(&payload).expect("encode");
        let back = decode_deposit_payload(&bytes).expect("decode");
        assert_eq!(back, payload);
    }

    /// The sealed envelope round-trips under the butler info string, and is
    /// domain-separated from ZEB-249: the same ciphertext MUST NOT open
    /// under the epoch-key-seal info (different HKDF info → different AEAD
    /// key → tag mismatch).
    #[test]
    fn sealed_envelope_round_trips_with_butler_info_string() {
        let (priv_bytes, pub_bytes) = make_x25519_keypair(0x01);
        let plaintext = b"butler deposit payload plaintext";

        let sealed = seal_to_owner_with_info(&pub_bytes, plaintext, BUTLER_DEPOSIT_SEAL_INFO)
            .expect("seal must succeed");
        // Same 32 ‖ 12 ‖ ct+tag envelope layout as ZEB-249.
        assert_eq!(sealed.len(), 32 + 12 + plaintext.len() + 16);

        let opened = open_from_owner_with_info(&priv_bytes, &sealed, BUTLER_DEPOSIT_SEAL_INFO)
            .expect("open with butler info must succeed");
        assert_eq!(opened, plaintext);

        // Cross-domain open MUST fail: ZEB-249's open_from_owner uses the
        // epoch-key-seal info string.
        let err = open_from_owner(&priv_bytes, &sealed)
            .expect_err("butler-sealed envelope must not open under the ZEB-249 info");
        assert!(
            matches!(err, crate::dm_signing::DmSignError::DecryptionFailed),
            "expected DecryptionFailed, got {err:?}"
        );
    }

    /// The signature payload is domain-separated: a different domain prefix
    /// produces different signed bytes, and a signature over the real
    /// payload does not verify against the alternate-domain payload.
    #[test]
    fn sig_payload_is_domain_separated() {
        use ed25519_dalek::{Signer, SigningKey, Verifier};

        let ro = [0x11u8; 16];
        let sb = vec![0xAB; 24];
        let payload = deposit_sig_payload(&ro, &sb);

        // Pin the construction: domain ‖ ro ‖ sealed_blob, in that order.
        assert!(payload.starts_with(BUTLER_DEPOSIT_SIG_DOMAIN));
        assert_eq!(
            &payload[BUTLER_DEPOSIT_SIG_DOMAIN.len()..BUTLER_DEPOSIT_SIG_DOMAIN.len() + 16],
            &ro
        );
        assert_eq!(&payload[BUTLER_DEPOSIT_SIG_DOMAIN.len() + 16..], &sb[..]);

        // Same-length alternate domain → different payload bytes.
        let alt_domain = b"harmony-zeb-999-deposit-sig-v1";
        assert_eq!(alt_domain.len(), BUTLER_DEPOSIT_SIG_DOMAIN.len());
        let mut alt_payload = Vec::new();
        alt_payload.extend_from_slice(alt_domain);
        alt_payload.extend_from_slice(&ro);
        alt_payload.extend_from_slice(&sb);
        assert_ne!(payload, alt_payload);

        // A signature over the domain-separated payload does not verify
        // against the alternate-domain bytes.
        let signing = SigningKey::from_bytes(&[0x42; 32]);
        let sig = signing.sign(&payload);
        assert!(signing.verifying_key().verify(&payload, &sig).is_ok());
        assert!(signing.verifying_key().verify(&alt_payload, &sig).is_err());
    }

    #[tokio::test]
    async fn frame_write_read_round_trips() {
        let body = encode_deposit_frame(&fixture_frame()).expect("encode");
        let mut wire: Vec<u8> = Vec::new();
        write_length_prefixed(&mut wire, &body)
            .await
            .expect("write frame");
        assert_eq!(wire.len(), 4 + body.len());
        assert_eq!(&wire[..4], &(body.len() as u32).to_le_bytes());

        let mut reader = wire.as_slice();
        let read_back = read_length_prefixed(&mut reader).await.expect("read frame");
        assert_eq!(read_back, body);
        assert!(
            reader.is_empty(),
            "frame read must consume exactly the frame"
        );
    }

    /// An oversize length prefix is rejected BEFORE the body is read or
    /// allocated. The reader is given ONLY the 4 prefix bytes — if the
    /// implementation attempted to read the body first it would hit
    /// UnexpectedEof (`Io`), not `FrameOutOfBounds`.
    #[tokio::test]
    async fn frame_read_rejects_oversize_before_body() {
        let oversize = (DEPOSIT_MAX_FRAME_BYTES as u32) + 1;
        let prefix_only = oversize.to_le_bytes().to_vec();
        let mut reader = prefix_only.as_slice();
        let err = read_length_prefixed(&mut reader)
            .await
            .expect_err("oversize prefix must be rejected");
        assert!(
            matches!(
                err,
                DepositWireError::FrameOutOfBounds { len, max }
                    if len == oversize as usize && max == DEPOSIT_MAX_FRAME_BYTES
            ),
            "expected FrameOutOfBounds (rejected before body read), got {err:?}"
        );

        // Zero-length prefix is likewise rejected before any body handling.
        let zero_prefix = 0u32.to_le_bytes().to_vec();
        let mut reader = zero_prefix.as_slice();
        let err = read_length_prefixed(&mut reader)
            .await
            .expect_err("zero-length prefix must be rejected");
        assert!(matches!(
            err,
            DepositWireError::FrameOutOfBounds { len: 0, .. }
        ));

        // Exactly-at-cap is accepted (boundary pin) — body present.
        let mut at_cap_wire = (DEPOSIT_MAX_FRAME_BYTES as u32).to_le_bytes().to_vec();
        at_cap_wire.extend(std::iter::repeat(0xAA).take(DEPOSIT_MAX_FRAME_BYTES));
        let mut reader = at_cap_wire.as_slice();
        let body = read_length_prefixed(&mut reader)
            .await
            .expect("at-cap frame must be accepted");
        assert_eq!(body.len(), DEPOSIT_MAX_FRAME_BYTES);
    }

    #[tokio::test]
    async fn frame_write_rejects_oversize() {
        let body = vec![0u8; DEPOSIT_MAX_FRAME_BYTES + 1];
        let mut wire: Vec<u8> = Vec::new();
        let err = write_length_prefixed(&mut wire, &body)
            .await
            .expect_err("oversize body must be rejected");
        assert!(matches!(err, DepositWireError::FrameOutOfBounds { .. }));
        assert!(
            wire.is_empty(),
            "nothing may be written for a rejected frame"
        );
    }
}
