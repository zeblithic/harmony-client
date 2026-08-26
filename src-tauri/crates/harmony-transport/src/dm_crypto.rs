//! ZEB-216 Sub-B Phase 1: DM encrypt/decrypt + AAD + sender-binding helpers.
//!
//! See `docs/specs/2026-05-02-zeb-216-sub-b-dm-transport-design.md`
//! §"Encryption helpers (Phase 1)" and §"Sender-binding check".

use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
    ChaCha20Poly1305,
};

use crate::dm_envelope::MessagePayload;
use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use crate::owner_state_types::{DmContentKey, OwnerAddr, Space};

/// Storage-blob layout per ZEB-219 §"Wire format":
///   version_byte(1) || nonce_12(12) || ciphertext(N) || poly1305_tag(16)
/// = N + 29 bytes minimum.
const STORAGE_BLOB_V1: u8 = 0x01;
const NONCE_LEN_V1: usize = 12;
const TAG_LEN: usize = 16;
const MIN_BLOB_LEN_V1: usize = 1 + NONCE_LEN_V1 + TAG_LEN; // 29

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DmEncryptError {
    #[error("payload CBOR encode failed: {0}")]
    PayloadEncode(String),
    #[error("AEAD encryption failed")]
    AeadFailure,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DmDecryptError {
    #[error("storage_blob shorter than minimum 29 bytes")]
    TruncatedBlob,
    #[error("unknown storage_blob version byte 0x{0:02x}")]
    UnknownVersion(u8),
    #[error("AEAD decryption failed under all candidate keys (current + prior)")]
    AeadFailureAllKeys,
    #[error("plaintext CBOR decode failed: {0}")]
    PayloadDecode(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DmReceiveError {
    #[error("payload sender does not match link-origin OwnerAddr (impersonation)")]
    SenderImpersonation,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AadComputeError {
    #[error("dedupe_key CBOR encode failed: {0}")]
    Encode(String),
}

/// Encrypt a MessagePayload into a v1 storage_blob bound by AAD.
/// The plaintext is canonical-CBOR-encoded MessagePayload bytes.
pub fn encrypt_dm_message(
    content_key: &DmContentKey,
    aad: &[u8],
    payload: &MessagePayload,
) -> Result<Vec<u8>, DmEncryptError> {
    let plaintext =
        canonical_cbor_encode(payload).map_err(|e| DmEncryptError::PayloadEncode(e.to_string()))?;
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let cipher = ChaCha20Poly1305::new(content_key.as_bytes().into());
    let ciphertext_with_tag = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: &plaintext,
                aad,
            },
        )
        .map_err(|_| DmEncryptError::AeadFailure)?;
    let mut blob = Vec::with_capacity(1 + NONCE_LEN_V1 + ciphertext_with_tag.len());
    blob.push(STORAGE_BLOB_V1);
    blob.extend_from_slice(nonce.as_slice());
    blob.extend_from_slice(&ciphertext_with_tag);
    Ok(blob)
}

/// Decrypt a v1 storage_blob, trying current key first then each prior
/// content_key in stored order. Length-gate enforced before slicing.
pub fn decrypt_dm_message(
    content_key: &DmContentKey,
    prior_content_keys: &[DmContentKey],
    aad: &[u8],
    storage_blob: &[u8],
) -> Result<MessagePayload, DmDecryptError> {
    if storage_blob.len() < MIN_BLOB_LEN_V1 {
        return Err(DmDecryptError::TruncatedBlob);
    }
    let version = storage_blob[0];
    let (nonce_slice, ciphertext_slice) = match version {
        STORAGE_BLOB_V1 => (
            &storage_blob[1..1 + NONCE_LEN_V1],
            &storage_blob[1 + NONCE_LEN_V1..],
        ),
        // 0x02 reserved (XChaCha20-Poly1305 with 24-byte nonce)
        other => return Err(DmDecryptError::UnknownVersion(other)),
    };
    let nonce: [u8; NONCE_LEN_V1] = nonce_slice.try_into().expect("length-gated above");

    for key in std::iter::once(content_key).chain(prior_content_keys.iter()) {
        let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
        if let Ok(plaintext) = cipher.decrypt(
            &nonce.into(),
            Payload {
                msg: ciphertext_slice,
                aad,
            },
        ) {
            return canonical_cbor_decode::<MessagePayload>(&plaintext)
                .map_err(|e| DmDecryptError::PayloadDecode(e.to_string()));
        }
    }
    Err(DmDecryptError::AeadFailureAllKeys)
}

/// Receive-time check: the encrypted-payload `sender` field MUST match
/// the OwnerAddr resolved from the inbound Reticulum link's identity_hash.
/// Phase 3b wires `link_origin` from `OwnerDeviceCache` resolution.
pub fn verify_sender_binding(
    payload: &MessagePayload,
    link_origin: OwnerAddr,
) -> Result<(), DmReceiveError> {
    if payload.sender != link_origin {
        return Err(DmReceiveError::SenderImpersonation);
    }
    Ok(())
}

/// Compute the AAD for a DM Space's encrypted messages: canonical CBOR
/// encoding of the Space's `dedupe_key()`. Stable across cross-SpaceId
/// dedupe collapses (per ZEB-219 §"Why dedupe_key not space_id").
pub fn compute_aad(space: &Space) -> Result<Vec<u8>, AadComputeError> {
    canonical_cbor_encode(&space.dedupe_key()).map_err(|e| AadComputeError::Encode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dm_envelope::MessagePayload;
    use crate::owner_state_types::{DedupeKey, DmContentKey, Hlc, OwnerAddr, SpaceId, SpaceKind};

    fn payload(sender: OwnerAddr) -> MessagePayload {
        MessagePayload {
            body: b"hello".to_vec(),
            mime_type: "text/plain".into(),
            sender,
            sent_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        }
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let key = DmContentKey::new([0x55; 32]);
        let aad = b"some aad";
        let p = payload(OwnerAddr([1; 16]));
        let blob = encrypt_dm_message(&key, aad, &p).unwrap();
        // version + nonce + ciphertext + tag = at least 29 bytes
        assert!(blob.len() >= 29);
        assert_eq!(blob[0], 0x01);
        let recovered = decrypt_dm_message(&key, &[], aad, &blob).unwrap();
        assert_eq!(p, recovered);
    }

    #[test]
    fn aad_mismatch_rejects() {
        let key = DmContentKey::new([0x55; 32]);
        let p = payload(OwnerAddr([1; 16]));
        let blob = encrypt_dm_message(&key, b"aad-1", &p).unwrap();
        let err = decrypt_dm_message(&key, &[], b"aad-2", &blob).unwrap_err();
        assert!(matches!(err, DmDecryptError::AeadFailureAllKeys));
    }

    #[test]
    fn version_byte_unknown_rejects() {
        let key = DmContentKey::new([0x55; 32]);
        let p = payload(OwnerAddr([1; 16]));
        let mut blob = encrypt_dm_message(&key, b"aad", &p).unwrap();
        blob[0] = 0xff; // unknown version
        let err = decrypt_dm_message(&key, &[], b"aad", &blob).unwrap_err();
        assert!(matches!(err, DmDecryptError::UnknownVersion(0xff)));
    }

    #[test]
    fn length_gate_short_blob_rejects() {
        let key = DmContentKey::new([0x55; 32]);
        let short = vec![0x01; 28]; // one byte short of 29
        let err = decrypt_dm_message(&key, &[], b"aad", &short).unwrap_err();
        assert!(matches!(err, DmDecryptError::TruncatedBlob));
    }

    #[test]
    fn tampered_ciphertext_rejects() {
        let key = DmContentKey::new([0x55; 32]);
        let p = payload(OwnerAddr([1; 16]));
        let mut blob = encrypt_dm_message(&key, b"aad", &p).unwrap();
        let last_idx = blob.len() - 1;
        blob[last_idx] ^= 0xff; // flip last byte (the auth tag)
        let err = decrypt_dm_message(&key, &[], b"aad", &blob).unwrap_err();
        assert!(matches!(err, DmDecryptError::AeadFailureAllKeys));
    }

    #[test]
    fn prior_content_keys_fallback_succeeds() {
        let k1 = DmContentKey::new([0x11; 32]);
        let k2 = DmContentKey::new([0x22; 32]);
        let p = payload(OwnerAddr([1; 16]));
        // Encrypt under k1; decrypt with current=k2, prior=[k1] — fallback.
        let blob = encrypt_dm_message(&k1, b"aad", &p).unwrap();
        let recovered = decrypt_dm_message(&k2, &[k1], b"aad", &blob).unwrap();
        assert_eq!(p, recovered);
    }

    #[test]
    fn sender_binding_match_ok() {
        let p = payload(OwnerAddr([1; 16]));
        assert!(verify_sender_binding(&p, OwnerAddr([1; 16])).is_ok());
    }

    #[test]
    fn sender_binding_mismatch_rejects() {
        let p = payload(OwnerAddr([1; 16]));
        let err = verify_sender_binding(&p, OwnerAddr([2; 16])).unwrap_err();
        assert!(matches!(err, DmReceiveError::SenderImpersonation));
    }

    #[test]
    fn compute_aad_dm_uses_dedupe_key() {
        // Two DM Spaces with the same sorted members must yield the same AAD.
        // ZEB-474: transport=None (Reticulum variant removed).
        let s1 = crate::owner_state_types::Space {
            id: SpaceId([1; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "x".into(),
            transport: None,
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            updated_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            content_key: Some(DmContentKey::new([0xaa; 32])),
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
        let mut s2 = s1.clone();
        s2.id = SpaceId([99; 16]); // different SpaceId
        s2.content_key = Some(DmContentKey::new([0xbb; 32])); // different key
                                                              // Same members → same dedupe_key → same AAD.
        assert_eq!(compute_aad(&s1).unwrap(), compute_aad(&s2).unwrap());
    }

    // ── DedupeKey CBOR golden-byte tests ──────────────────────────────────────
    //
    // DedupeKey is the AAD seed for every encrypted DM message: compute_aad()
    // calls `canonical_cbor_encode(&space.dedupe_key())`. If a serde rename or
    // variant-code change alters the encoded bytes, the AEAD tag will no longer
    // verify for any existing ciphertext — every live DM becomes undecryptable.
    //
    // Adjacent-tagging layout (`#[serde(tag = "tg", content = "vl")]`):
    //   map(2) { "tg": <variant-code>, "vl": <content> }
    //
    // CBOR integer reference used below:
    //   0xa2 = map(2)    0x62 = text(2)    0x61 = text(1)
    //   0x82 = array(2)  0x50 = bstr(16)   0xf6 = null

    #[test]
    fn dedupe_key_sorted_members_cbor_golden_bytes() {
        // DedupeKey::SortedMembers is the load-bearing variant — it is what
        // compute_aad produces for every DM Space. Lock in its wire bytes so
        // any future serde rename is caught immediately.
        let key = DedupeKey::SortedMembers(vec![OwnerAddr([0x01; 16]), OwnerAddr([0x02; 16])]);
        let encoded = canonical_cbor_encode(&key).unwrap();

        // Wire layout:
        //   map(2)             0xa2
        //   text(2) "tg"       0x62 't' 'g'
        //   text(1) "s"        0x61 's'         ← variant rename = "s"
        //   text(2) "vl"       0x62 'v' 'l'
        //   array(2)           0x82
        //   bstr(16) [0x01;16] 0x50 || 16 × 0x01
        //   bstr(16) [0x02;16] 0x50 || 16 × 0x02
        //
        // Total: 1 + 3 + 2 + 3 + 1 + 17 + 17 = 44 bytes
        let mut expected: Vec<u8> = vec![
            0xa2, // map(2)
            0x62, b't', b'g', // "tg"
            0x61, b's', // "s"  — SortedMembers variant code
            0x62, b'v', b'l', // "vl"
            0x82, // array(2)
            0x50, // bstr(16)
        ];
        expected.extend([0x01u8; 16]);
        expected.push(0x50); // bstr(16)
        expected.extend([0x02u8; 16]);

        assert_eq!(
            encoded, expected,
            "DedupeKey::SortedMembers CBOR wire format MUST NOT change — it is the AAD \
             on every encrypted DM message. If this assertion fails, you have broken AAD \
             stability; every existing live DM will become undecryptable under the new AAD.",
        );
    }

    #[test]
    fn dedupe_key_none_cbor_golden_bytes() {
        // DedupeKey::None is assigned to Folder Spaces (which never dedupe).
        // Lock in the unit-variant encoding so adjacent-tagging `vl: null`
        // stays pinned.
        let key = DedupeKey::None;
        let encoded = canonical_cbor_encode(&key).unwrap();

        // Serde adjacent tagging for a unit variant omits the "vl" (content)
        // key entirely — it only emits the tag field.
        //
        // Wire layout:
        //   map(1)       0xa1
        //   text(2) "tg" 0x62 't' 'g'
        //   text(1) "n"  0x61 'n'   ← variant rename = "n"
        //
        // Total: 1 + 3 + 2 = 6 bytes
        let expected: Vec<u8> = vec![
            0xa1, // map(1)  — no "vl" key for unit variant
            0x62, b't', b'g', // "tg"
            0x61, b'n', // "n"  — None variant code
        ];

        assert_eq!(
            encoded, expected,
            "DedupeKey::None CBOR wire format pinned — variant code must stay \"n\"",
        );
    }

    #[test]
    fn dedupe_key_id_cbor_golden_bytes() {
        // DedupeKey::Id is assigned to Community/Channel/Group-DM Spaces.
        let key = DedupeKey::Id(SpaceId([0xab; 16]));
        let encoded = canonical_cbor_encode(&key).unwrap();

        // Wire layout:
        //   map(2)             0xa2
        //   text(2) "tg"       0x62 't' 'g'
        //   text(1) "i"        0x61 'i'   ← variant rename = "i"
        //   text(2) "vl"       0x62 'v' 'l'
        //   bstr(16) [0xab;16] 0x50 || 16 × 0xab
        //
        // Total: 1 + 3 + 2 + 3 + 17 = 26 bytes
        let mut expected: Vec<u8> = vec![
            0xa2, // map(2)
            0x62, b't', b'g', // "tg"
            0x61, b'i', // "i"  — Id variant code
            0x62, b'v', b'l', // "vl"
            0x50, // bstr(16)
        ];
        expected.extend([0xabu8; 16]);

        assert_eq!(
            encoded, expected,
            "DedupeKey::Id CBOR wire format pinned — variant code must stay \"i\"",
        );
    }

    #[test]
    fn dedupe_key_topic_cbor_golden_bytes() {
        // DedupeKey::Topic is assigned to public-channel Spaces (Zenoh topic).
        let key = DedupeKey::Topic("dm:test".to_string());
        let encoded = canonical_cbor_encode(&key).unwrap();

        // Wire layout:
        //   map(2)             0xa2
        //   text(2) "tg"       0x62 't' 'g'
        //   text(1) "t"        0x61 't'   ← variant rename = "t"
        //   text(2) "vl"       0x62 'v' 'l'
        //   text(7) "dm:test"  0x67 'd' 'm' ':' 't' 'e' 's' 't'
        //
        // Total: 1 + 3 + 2 + 3 + 8 = 17 bytes
        let expected: Vec<u8> = vec![
            0xa2, // map(2)
            0x62, b't', b'g', // "tg"
            0x61, b't', // "t"  — Topic variant code
            0x62, b'v', b'l', // "vl"
            0x67, b'd', b'm', b':', b't', b'e', b's', b't', // text(7) "dm:test"
        ];

        assert_eq!(
            encoded, expected,
            "DedupeKey::Topic CBOR wire format pinned — variant code must stay \"t\"",
        );
    }
}
