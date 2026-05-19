//! ZEB-301 Phase 4a-foundation: thin wrappers over `frost-ristretto255` for
//! the D-FROST committee. Keeps the rest of the codebase from depending
//! directly on FROST's `BTreeMap<Identifier, ...>` API by exposing
//! `(Identifier, Vec<u8>)`-shaped helpers that match the wire envelope.
//!
//! Threshold-sign + aggregate wrappers stubbed for Task 7 — the DKG path
//! is exercised end-to-end in Tasks 4-5.

use frost_ristretto255::{
    keys::{
        dkg::{self, round1, round2},
        KeyPackage, PublicKeyPackage, VerifyingShare,
    },
    rand_core, Identifier, VerifyingKey,
};
use std::collections::BTreeMap;

/// 1-indexed FROST `Identifier` from a zero-based array index. Sorted-
/// `OwnerAddr` enumeration → `index 0 → id=1`, `index 1 → id=2`, etc.
///
/// FROST's `Identifier::try_from(u16)` rejects `0` (the additive identity)
/// so callers MUST pass `index < u16::MAX - 1`. v1 committees cap at
/// max_signers ≤ 7 so this is well within range.
pub fn identifier_for_index(index: usize) -> Identifier {
    // R1 fix (CodeRabbit Major, Cursor Low): use u16::try_from to reject
    // indices >= 65536 loudly rather than letting `as u16` silently wrap
    // them into the low-end and collide with legitimate identifiers.
    // Caller is responsible for staying within `max_signers <= u16::MAX`
    // (v1 committees cap at 7, so this is defence-in-depth).
    let idx_u16: u16 =
        u16::try_from(index).expect("committee index overflowed u16 — committee too large");
    let n: u16 = idx_u16
        .checked_add(1)
        .expect("committee index +1 overflowed u16 — committee too large");
    Identifier::try_from(n).expect("FROST rejects identifier 0; index+1 guarantees non-zero")
}

/// Run FROST DKG round-1 locally. Returns the secret state (kept on this
/// node) and the CBOR-encoded round-1 broadcast package (sent to every
/// other committee member). `max_signers` and `min_signers` are committee-
/// wide constants known before the ceremony starts.
pub fn dkg_part1_local(
    identifier: Identifier,
    max_signers: u16,
    min_signers: u16,
) -> Result<(round1::SecretPackage, Vec<u8>), String> {
    let (secret, package) = dkg::part1(identifier, max_signers, min_signers, rand_core::OsRng)
        .map_err(|e| format!("dkg::part1: {e}"))?;
    let mut buf = Vec::new();
    ciborium::into_writer(&package, &mut buf).map_err(|e| format!("encode r1 pkg: {e}"))?;
    Ok((secret, buf))
}

/// Run FROST DKG round-2 locally. Takes this member's round-1 secret and
/// the round-1 packages received from every OTHER member (keyed by their
/// `Identifier`). Returns the round-2 secret and a map of recipient-
/// identifier → CBOR-encoded round-2 package (each must be sent privately
/// to that recipient — round-2 packages are pairwise, not broadcast).
pub fn dkg_part2_local(
    round1_secret: round1::SecretPackage,
    round1_packages_received: &BTreeMap<Identifier, Vec<u8>>,
) -> Result<(round2::SecretPackage, BTreeMap<Identifier, Vec<u8>>), String> {
    let mut decoded: BTreeMap<Identifier, round1::Package> = BTreeMap::new();
    for (id, bytes) in round1_packages_received {
        let pkg: round1::Package =
            ciborium::from_reader(&bytes[..]).map_err(|e| format!("decode r1 pkg: {e}"))?;
        decoded.insert(*id, pkg);
    }
    let (secret, round2_packages) =
        dkg::part2(round1_secret, &decoded).map_err(|e| format!("dkg::part2: {e}"))?;
    let mut encoded: BTreeMap<Identifier, Vec<u8>> = BTreeMap::new();
    for (id, pkg) in round2_packages {
        let mut buf = Vec::new();
        ciborium::into_writer(&pkg, &mut buf).map_err(|e| format!("encode r2 pkg: {e}"))?;
        encoded.insert(id, buf);
    }
    Ok((secret, encoded))
}

/// Run FROST DKG round-3 (finalization) locally. Returns this member's
/// `KeyPackage` (private signing share) and the committee's
/// `PublicKeyPackage` (joint verifying key + every member's verifying
/// share). After part3, the ceremony is complete on this node.
pub fn dkg_part3_local(
    round2_secret: &round2::SecretPackage,
    round1_packages_received: &BTreeMap<Identifier, Vec<u8>>,
    round2_packages_received: &BTreeMap<Identifier, Vec<u8>>,
) -> Result<(KeyPackage, PublicKeyPackage), String> {
    let mut r1: BTreeMap<Identifier, round1::Package> = BTreeMap::new();
    for (id, bytes) in round1_packages_received {
        let pkg: round1::Package =
            ciborium::from_reader(&bytes[..]).map_err(|e| format!("decode r1 pkg: {e}"))?;
        r1.insert(*id, pkg);
    }
    let mut r2: BTreeMap<Identifier, round2::Package> = BTreeMap::new();
    for (id, bytes) in round2_packages_received {
        let pkg: round2::Package =
            ciborium::from_reader(&bytes[..]).map_err(|e| format!("decode r2 pkg: {e}"))?;
        r2.insert(*id, pkg);
    }
    dkg::part3(round2_secret, &r1, &r2).map_err(|e| format!("dkg::part3: {e}"))
}

/// Compressed Ristretto encoding of the joint verifying key (32 bytes).
/// This is the public-key bytes that go into the `DkgComplete` event so
/// every non-committee replica can verify VRF beacons.
pub fn verifying_key_to_bytes(vk: &VerifyingKey) -> [u8; 32] {
    let mut out = [0u8; 32];
    let ser = vk
        .serialize()
        .expect("VerifyingKey::serialize is infallible for Ristretto255");
    debug_assert_eq!(ser.len(), 32, "Ristretto compressed point is 32 bytes");
    out.copy_from_slice(&ser);
    out
}

/// Compressed Ristretto encoding of a single member's verifying share.
/// Persisted per-member on `DkgComplete` so partial-signature verification
/// can run without re-deriving from the secret share.
pub fn verifying_share_to_bytes(vs: &VerifyingShare) -> [u8; 32] {
    let mut out = [0u8; 32];
    let ser = vs
        .serialize()
        .expect("VerifyingShare::serialize is infallible for Ristretto255");
    debug_assert_eq!(ser.len(), 32, "Ristretto compressed point is 32 bytes");
    out.copy_from_slice(&ser);
    out
}

/// Verify a 64-byte FROST-Schnorr signature against a 32-byte compressed
/// joint verifying key and a message. Returns `Ok(())` iff the signature
/// is a valid Schnorr signature on `msg` under `joint_vk_bytes`.
///
/// R2 (CodeRabbit Critical): the VRF beacon apply path was previously
/// only validating `derive_vrf_output(R_compressed) == payload.vrf_output`,
/// which any 64-byte blob with a matching SHA-256(R) prefix would
/// trivially pass. The actual security guarantee — that the committee
/// produced a valid threshold signature on the agreed message — requires
/// the full Schnorr verify against the joint verifying key. Without
/// this, an attacker can forge VRF beacons by feeding garbage signature
/// bytes whose first 32 bytes hash to a chosen vrf_output.
pub fn verify_schnorr_signature(
    joint_vk_bytes: &[u8; 32],
    msg: &[u8],
    signature_bytes: &[u8],
) -> Result<(), String> {
    use frost_ristretto255::Signature;
    if signature_bytes.len() != 64 {
        return Err(format!(
            "schnorr signature must be 64 bytes, got {}",
            signature_bytes.len()
        ));
    }
    let vk = VerifyingKey::deserialize(joint_vk_bytes)
        .map_err(|e| format!("VerifyingKey::deserialize: {e}"))?;
    let sig = Signature::deserialize(signature_bytes)
        .map_err(|e| format!("Signature::deserialize: {e}"))?;
    vk.verify(msg, &sig).map_err(|e| format!("verify: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use frost_ristretto255::keys::dkg;

    #[test]
    fn dkg_part1_round1_package_cbor_round_trips() {
        // Verifies: frost serde feature works + ciborium handles the package type.
        let id = identifier_for_index(0);
        let (_secret, r1_bytes) = dkg_part1_local(id, 2, 2).expect("part1");
        assert!(!r1_bytes.is_empty(), "round1 package must produce bytes");
        // Deserialize and re-serialize: must be byte-identical.
        let pkg: dkg::round1::Package = ciborium::from_reader(&r1_bytes[..]).expect("decode");
        let mut re_encoded = Vec::new();
        ciborium::into_writer(&pkg, &mut re_encoded).expect("re-encode");
        assert_eq!(r1_bytes, re_encoded, "round1::Package CBOR must round-trip");
    }

    #[test]
    fn identifier_for_index_is_1_indexed_and_deterministic() {
        let id0 = identifier_for_index(0);
        let id1 = identifier_for_index(1);
        assert_ne!(id0, id1);
        assert_eq!(identifier_for_index(0), id0); // deterministic
                                                  // id0 == Identifier::try_from(1u16)
        assert_eq!(id0, frost_ristretto255::Identifier::try_from(1u16).unwrap());
    }

    #[test]
    fn dkg_part2_produces_one_package_per_other_participant() {
        let id1 = identifier_for_index(0);
        let id2 = identifier_for_index(1);
        let id3 = identifier_for_index(2);

        let (sec1, _r1_1) = dkg_part1_local(id1, 3, 2).unwrap();
        let (_sec2, r1_2) = dkg_part1_local(id2, 3, 2).unwrap();
        let (_sec3, r1_3) = dkg_part1_local(id3, 3, 2).unwrap();

        // id1 runs part2 with packages from id2 and id3
        let received: BTreeMap<Identifier, Vec<u8>> =
            [(id2, r1_2), (id3, r1_3)].into_iter().collect();

        let (_sec2_pkg, r2_map) = dkg_part2_local(sec1, &received).expect("part2");
        // part2 produces one package per other participant (2 here)
        assert_eq!(r2_map.len(), 2);
        assert!(r2_map.contains_key(&id2));
        assert!(r2_map.contains_key(&id3));
    }
}
