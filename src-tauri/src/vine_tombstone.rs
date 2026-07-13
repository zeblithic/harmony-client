//! ZEB-670: creator-signed vine tombstone (delete verb).
//!
//! The vine wire path is otherwise unsigned (ZEB-673 tracks that gap); the
//! tombstone is signed anyway because it is the first REMOTELY DESTRUCTIVE
//! vine verb — unsigned, it would let any peer censor any creator's vines,
//! whereas unsigned descriptors are merely additive (first-writer-wins by
//! id, so existing content cannot be remotely destroyed or overwritten).
//!
//! The scheme mirrors `community_membership::verify_signature`: the record
//! carries the signer's 64-byte identity pub (X25519(32) || Ed25519(32));
//! receivers require `Identity::from_public_bytes(pub).address_hash` to
//! equal the claimed `creator_address` (hex), then `verify_strict` over the
//! canonical bytes. This binds the signature to the address the vine
//! subsystem already uses as creator identity (`node_addr` is
//! `hex::encode(address_hash)` — see `start_node`'s identity-load block).

use serde::{Deserialize, Serialize};
// ZEB-678 S2: `sk.sign(..)` on the enrolled `#2` key comes from the
// `ed25519_dalek::Signer` trait (the `#3` path signs via `PrivateIdentity`).
use ed25519_dalek::Signer as _;

/// Domain-separation prefix + version for the signed byte string.
const CANONICAL_PREFIX: &str = "harmony-vine-tombstone-v1";
/// ZEB-678 S2: `-v2` domain for the enrolled `#2` device signature.
const CANONICAL_PREFIX_V2: &str = "harmony-vine-tombstone-v2";

/// Wire record published on `harmony/vines/{creator}/tombstones/{vine_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VineTombstonePayload {
    pub vine_id: String,
    pub video_cid: String,
    pub creator_address: String,
    /// Unix seconds at signing time.
    pub deleted_at: u64,
    /// Hex-encoded 64-byte identity pub (X25519(32) || Ed25519(32)),
    /// per `harmony_identity::Identity::to_public_bytes`.
    pub creator_identity_pub: String,
    /// Hex-encoded 64-byte Ed25519 signature over `canonical_bytes`.
    pub sig: String,
    /// ZEB-678 S2: hex 64-byte enrolled `#2` device signature over
    /// `canonical_bytes_v2`. A tombstone dual-signs (the `#3` `sig`/
    /// `creator_identity_pub` stay required for backward verify); presence
    /// of `device_sig` marks the tombstone as migrated. Wire-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_sig: Option<String>,
}

/// Deterministic byte string the signature covers. `|` cannot appear in
/// any field (vine ids are `vine-{hex}-{secs}-{hex}`, CIDs and addresses
/// are hex, `deleted_at` is decimal), so the encoding is unambiguous.
pub fn canonical_bytes(
    vine_id: &str,
    video_cid: &str,
    creator_address: &str,
    deleted_at: u64,
) -> Vec<u8> {
    format!("{CANONICAL_PREFIX}|{vine_id}|{video_cid}|{creator_address}|{deleted_at}").into_bytes()
}

/// Key expression a tombstone is published on. Its own key space (sibling
/// to `/reactions/`) because the descriptor subscription `harmony/vines/*`
/// is single-segment; the event loop subscribes to
/// `harmony/vines/*/tombstones/*` and routes by `/tombstones/`.
pub fn tombstone_key_expr(creator_address: &str, vine_id: &str) -> String {
    format!("harmony/vines/{creator_address}/tombstones/{vine_id}")
}

/// Build and sign a tombstone with the local owner identity. The caller is
/// responsible for passing the `creator_address` that matches `private`'s
/// identity (`hex::encode(private.public_identity().address_hash)`) —
/// receivers reject mismatches in `verify_tombstone`.
pub fn sign_tombstone(
    private: &harmony_identity::PrivateIdentity,
    vine_id: String,
    video_cid: String,
    creator_address: String,
    deleted_at: u64,
) -> VineTombstonePayload {
    let bytes = canonical_bytes(&vine_id, &video_cid, &creator_address, deleted_at);
    let sig = private.sign(&bytes);
    VineTombstonePayload {
        vine_id,
        video_cid,
        creator_address,
        deleted_at,
        creator_identity_pub: hex::encode(private.public_identity().to_public_bytes()),
        sig: hex::encode(sig),
        device_sig: None,
    }
}

/// Verify a received tombstone: pubkey→address binding, then strict
/// Ed25519 over the canonical bytes. Mirrors
/// `community_membership::verify_signature` (`verify_strict` rejects
/// non-canonical S values / small-order R points — RFC 8032 strict subset).
pub fn verify_tombstone(t: &VineTombstonePayload) -> Result<(), String> {
    // The canonical encoding joins fields with `|`; a field containing the
    // separator could make two distinct payloads share signed bytes. Legit
    // records never contain it (ids are `vine-{hex}-{secs}-{hex}`, CIDs and
    // addresses are hex) — reject rather than trust. (CodeRabbit PR #445)
    if t.vine_id.contains('|') || t.video_cid.contains('|') || t.creator_address.contains('|') {
        return Err("tombstone field contains the canonical separator '|'".into());
    }
    let pub_vec = hex::decode(&t.creator_identity_pub)
        .map_err(|e| format!("tombstone identity pub is not hex: {e}"))?;
    let identity = harmony_identity::Identity::from_public_bytes(&pub_vec)
        .map_err(|_| "tombstone identity pub invalid".to_string())?;
    if hex::encode(identity.address_hash) != t.creator_address {
        return Err("tombstone pubkey does not match claimed creator address".into());
    }
    let sig_bytes: [u8; 64] = hex::decode(&t.sig)
        .map_err(|e| format!("tombstone sig is not hex: {e}"))?
        .try_into()
        .map_err(|_| "tombstone sig must be 64 bytes".to_string())?;
    let bytes = canonical_bytes(&t.vine_id, &t.video_cid, &t.creator_address, t.deleted_at);
    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
    identity
        .verifying_key
        .verify_strict(&bytes, &sig)
        .map_err(|_| "tombstone signature invalid".to_string())
}

/// ZEB-678 S2: same field set as [`canonical_bytes`] under the `-v2` domain.
pub fn canonical_bytes_v2(
    vine_id: &str,
    video_cid: &str,
    creator_address: &str,
    deleted_at: u64,
) -> Vec<u8> {
    format!("{CANONICAL_PREFIX_V2}|{vine_id}|{video_cid}|{creator_address}|{deleted_at}")
        .into_bytes()
}

/// ZEB-678 S2: add the enrolled `#2` `device_sig` to an already-`#3`-signed
/// tombstone (dual-sign — the required `#3` `sig`/`creator_identity_pub` stay
/// for backward verify; `device_sig` marks it migrated).
pub fn sign_tombstone_v2(sk: &ed25519_dalek::SigningKey, t: &mut VineTombstonePayload) {
    let bytes = canonical_bytes_v2(&t.vine_id, &t.video_cid, &t.creator_address, t.deleted_at);
    t.device_sig = Some(hex::encode(sk.sign(&bytes).to_bytes()));
}

/// ZEB-678 S2: verify a tombstone's `#2` `device_sig` against the feed
/// authority `publisher_key`.
pub fn verify_tombstone_v2(
    t: &VineTombstonePayload,
    publisher_key: &[u8; 32],
) -> Result<(), String> {
    if t.vine_id.contains('|') || t.video_cid.contains('|') || t.creator_address.contains('|') {
        return Err("tombstone field contains the canonical separator '|'".into());
    }
    let bytes = canonical_bytes_v2(&t.vine_id, &t.video_cid, &t.creator_address, t.deleted_at);
    crate::vine_signing::verify_device_sig(
        t.device_sig.as_deref(),
        publisher_key,
        &bytes,
        "tombstone",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_identity() -> harmony_identity::PrivateIdentity {
        harmony_identity::PrivateIdentity::generate(&mut rand::rngs::OsRng)
    }

    fn addr_of(private: &harmony_identity::PrivateIdentity) -> String {
        hex::encode(private.public_identity().address_hash)
    }

    #[test]
    fn sign_verify_roundtrip() {
        let id = test_identity();
        let t = sign_tombstone(
            &id,
            "vine-abc-1".into(),
            "cafe01".into(),
            addr_of(&id),
            1_700_000_000,
        );
        assert!(verify_tombstone(&t).is_ok());
    }

    #[test]
    fn verify_rejects_address_not_matching_pubkey() {
        let id = test_identity();
        let other = test_identity();
        // Signed with `id` but claiming `other`'s address: the canonical
        // bytes cover the claimed address, so this fails the
        // pubkey→address binding check (first gate).
        let t = sign_tombstone(
            &id,
            "vine-abc-1".into(),
            "cafe01".into(),
            addr_of(&other),
            1_700_000_000,
        );
        let err = verify_tombstone(&t).unwrap_err();
        assert!(err.contains("does not match"), "got {err:?}");
    }

    #[test]
    fn verify_rejects_tampered_vine_id() {
        let id = test_identity();
        let mut t = sign_tombstone(
            &id,
            "vine-abc-1".into(),
            "cafe01".into(),
            addr_of(&id),
            1_700_000_000,
        );
        t.vine_id = "vine-abc-2".into();
        let err = verify_tombstone(&t).unwrap_err();
        assert!(err.contains("signature invalid"), "got {err:?}");
    }

    #[test]
    fn verify_rejects_signature_from_different_identity() {
        let id = test_identity();
        let attacker = test_identity();
        // Attacker forges a tombstone claiming the victim's address AND
        // shipping the victim's real pubkey (so the address binding
        // passes), but can only sign with their own key.
        let victim_addr = addr_of(&id);
        let bytes = canonical_bytes("vine-abc-1", "cafe01", &victim_addr, 1_700_000_000);
        let forged = VineTombstonePayload {
            vine_id: "vine-abc-1".into(),
            video_cid: "cafe01".into(),
            creator_address: victim_addr,
            deleted_at: 1_700_000_000,
            creator_identity_pub: hex::encode(id.public_identity().to_public_bytes()),
            sig: hex::encode(attacker.sign(&bytes)),
            device_sig: None,
        };
        let err = verify_tombstone(&forged).unwrap_err();
        assert!(err.contains("signature invalid"), "got {err:?}");
    }

    #[test]
    fn verify_rejects_separator_injection() {
        // vine_id="a|cid" / video_cid="b" and vine_id="a" / video_cid="cid|b"
        // would share canonical bytes — the verifier refuses any field
        // carrying the separator instead of disambiguating.
        let id = test_identity();
        let addr = addr_of(&id);
        let bytes = canonical_bytes("vine-a|x", "cafe01", &addr, 1_700_000_000);
        let t = VineTombstonePayload {
            vine_id: "vine-a|x".into(),
            video_cid: "cafe01".into(),
            creator_address: addr,
            deleted_at: 1_700_000_000,
            creator_identity_pub: hex::encode(id.public_identity().to_public_bytes()),
            sig: hex::encode(id.sign(&bytes)),
            device_sig: None,
        };
        let err = verify_tombstone(&t).unwrap_err();
        assert!(err.contains("separator"), "got {err:?}");
    }

    #[test]
    fn payload_serializes_camel_case() {
        let id = test_identity();
        let t = sign_tombstone(
            &id,
            "vine-abc-1".into(),
            "cafe01".into(),
            addr_of(&id),
            1_700_000_000,
        );
        let json = serde_json::to_value(&t).unwrap();
        for key in [
            "vineId",
            "videoCid",
            "creatorAddress",
            "deletedAt",
            "creatorIdentityPub",
            "sig",
        ] {
            assert!(json.get(key).is_some(), "missing camelCase key {key}");
        }
    }

    #[test]
    fn key_expr_shape() {
        assert_eq!(
            tombstone_key_expr("aabb", "vine-1"),
            "harmony/vines/aabb/tombstones/vine-1"
        );
    }

    #[test]
    fn tombstone_v2_dual_signs_and_verifies() {
        let id = test_identity();
        let sk = ed25519_dalek::SigningKey::from_bytes(&[0x66; 32]);
        let pk = sk.verifying_key().to_bytes();
        let mut t = sign_tombstone(
            &id,
            "vine-abc-1".into(),
            "cafe01".into(),
            addr_of(&id),
            1_700_000_000,
        );
        assert!(t.device_sig.is_none());
        assert!(verify_tombstone(&t).is_ok(), "legacy #3 valid before v2");
        sign_tombstone_v2(&sk, &mut t);
        assert!(t.device_sig.is_some());
        verify_tombstone_v2(&t, &pk).expect("valid #2 tombstone signature");
        // Dual-sign: the legacy `#3` signature STILL verifies after adding v2.
        assert!(
            verify_tombstone(&t).is_ok(),
            "legacy #3 still valid on a dual-signed tombstone"
        );
        let wrong = ed25519_dalek::SigningKey::from_bytes(&[0x77; 32])
            .verifying_key()
            .to_bytes();
        assert!(
            verify_tombstone_v2(&t, &wrong).is_err(),
            "wrong key rejected"
        );
        // A tombstone with no device_sig fails the v2 path.
        let plain = sign_tombstone(
            &id,
            "vine-abc-2".into(),
            "cafe02".into(),
            addr_of(&id),
            1_700_000_001,
        );
        assert!(verify_tombstone_v2(&plain, &pk)
            .unwrap_err()
            .contains("no device signature"));
    }
}
