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

// ── ZEB-1027: proactive-refresh (zero-sharing DKG) wrappers ──────────────────
//
// The refresh DKG is structurally the regular DKG with a ZERO constant
// term: each participant deals a random polynomial whose secret is 0, so
// summing the resulting shares onto the OLD shares rotates every share
// while preserving the joint secret (and therefore the joint verifying
// key). frost-core's `keys::refresh` module implements it over the same
// `round1`/`round2` package types as `dkg`, so the wire shapes below are
// byte-compatible with the `dkg_part*_local` wrappers above.
//
// CRYPTOGRAPHIC CONSTRAINT (drives ZEB-1027's repair flow): the
// finalization (`refresh_dkg_shares`) computes
// `new_share = old_share + Σ deltas` — it ROTATES a share the member
// still holds; it cannot mint one for a member who lost theirs. Lost
// shares are recovered by the RTS wrappers further down.

/// Run refresh-DKG round 1 locally (zero-constant-term commitment).
/// Same output shape as `dkg_part1_local`: the secret state stays on
/// this node, the CBOR-encoded round-1 package is broadcast publicly.
pub fn refresh_part1_local(
    identifier: Identifier,
    max_signers: u16,
    min_signers: u16,
) -> Result<(round1::SecretPackage, Vec<u8>), String> {
    let (secret, package) = frost_ristretto255::keys::refresh::refresh_dkg_part1(
        identifier,
        max_signers,
        min_signers,
        rand_core::OsRng,
    )
    .map_err(|e| format!("refresh_dkg_part1: {e}"))?;
    let mut buf = Vec::new();
    ciborium::into_writer(&package, &mut buf).map_err(|e| format!("encode refresh r1 pkg: {e}"))?;
    Ok((secret, buf))
}

/// Run refresh-DKG round 2 locally. Mirrors `dkg_part2_local`: takes the
/// round-1 packages from every OTHER member, returns the round-2 secret
/// plus per-recipient round-2 package bytes (sent sealed, pairwise).
pub fn refresh_part2_local(
    round1_secret: round1::SecretPackage,
    round1_packages_received: &BTreeMap<Identifier, Vec<u8>>,
) -> Result<(round2::SecretPackage, BTreeMap<Identifier, Vec<u8>>), String> {
    let mut decoded: BTreeMap<Identifier, round1::Package> = BTreeMap::new();
    for (id, bytes) in round1_packages_received {
        let pkg: round1::Package =
            ciborium::from_reader(&bytes[..]).map_err(|e| format!("decode refresh r1 pkg: {e}"))?;
        decoded.insert(*id, pkg);
    }
    let (secret, round2_packages) =
        frost_ristretto255::keys::refresh::refresh_dkg_part2(round1_secret, &decoded)
            .map_err(|e| format!("refresh_dkg_part2: {e}"))?;
    let mut encoded: BTreeMap<Identifier, Vec<u8>> = BTreeMap::new();
    for (id, pkg) in round2_packages {
        let mut buf = Vec::new();
        ciborium::into_writer(&pkg, &mut buf).map_err(|e| format!("encode refresh r2 pkg: {e}"))?;
        encoded.insert(id, buf);
    }
    Ok((secret, encoded))
}

/// Run refresh-DKG finalization locally. Requires this member's OLD
/// `KeyPackage` (the share being rotated — see the module comment: a
/// member without its old share cannot finalize a refresh) and the OLD
/// `PublicKeyPackage`. Returns the rotated `(KeyPackage,
/// PublicKeyPackage)`; the joint verifying key is preserved by
/// construction, the per-member verifying shares all change.
pub fn refresh_part3_local(
    round2_secret: &round2::SecretPackage,
    round1_packages_received: &BTreeMap<Identifier, Vec<u8>>,
    round2_packages_received: &BTreeMap<Identifier, Vec<u8>>,
    old_pub_key_package: PublicKeyPackage,
    old_key_package: KeyPackage,
) -> Result<(KeyPackage, PublicKeyPackage), String> {
    let mut r1: BTreeMap<Identifier, round1::Package> = BTreeMap::new();
    for (id, bytes) in round1_packages_received {
        let pkg: round1::Package =
            ciborium::from_reader(&bytes[..]).map_err(|e| format!("decode refresh r1 pkg: {e}"))?;
        r1.insert(*id, pkg);
    }
    let mut r2: BTreeMap<Identifier, round2::Package> = BTreeMap::new();
    for (id, bytes) in round2_packages_received {
        let pkg: round2::Package =
            ciborium::from_reader(&bytes[..]).map_err(|e| format!("decode refresh r2 pkg: {e}"))?;
        r2.insert(*id, pkg);
    }
    frost_ristretto255::keys::refresh::refresh_dkg_shares(
        round2_secret,
        &r1,
        &r2,
        old_pub_key_package,
        old_key_package,
    )
    .map_err(|e| format!("refresh_dkg_shares: {e}"))
}

// ── ZEB-1027: Repairable Threshold Scheme (RTS) wrappers ─────────────────────
//
// RTS (<https://eprint.iacr.org/2017/1155>) restores ONE participant's
// LOST signing share using ≥ `min_signers` helpers who still hold
// theirs: the helpers jointly interpolate the current secret polynomial
// at the participant's identifier without any helper learning another's
// share. Deltas travel helper→helper, sigmas helper→participant — both
// sealed. Works at whatever epoch the helpers' shares are at, so it
// composes with proactive refresh (repair-then-refresh or
// refresh-then-repair both land on the current polynomial).

/// RTS part 1 (helper): produce one delta per declared helper
/// (including self). `helpers` is the declared helper identifier set —
/// the Lagrange coefficients are computed over exactly this set, so
/// every listed helper must eventually contribute.
pub fn repair_part1_local(
    helpers: &[Identifier],
    key_package: &KeyPackage,
    participant: Identifier,
) -> Result<BTreeMap<Identifier, Vec<u8>>, String> {
    let deltas = frost_ristretto255::keys::repairable::repair_share_part1::<
        frost_ristretto255::Ristretto255Sha512,
        _,
    >(helpers, key_package, &mut rand_core::OsRng, participant)
    .map_err(|e| format!("repair_share_part1: {e}"))?;
    Ok(deltas
        .into_iter()
        .map(|(id, delta)| (id, delta.serialize()))
        .collect())
}

/// RTS part 2 (helper): sum the deltas received from every declared
/// helper (own included) into the sigma sent sealed to the participant.
pub fn repair_part2_local(delta_bytes: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    let mut deltas = Vec::with_capacity(delta_bytes.len());
    for bytes in delta_bytes {
        deltas.push(
            frost_ristretto255::keys::repairable::Delta::deserialize(bytes)
                .map_err(|e| format!("Delta::deserialize: {e}"))?,
        );
    }
    let sigma = frost_ristretto255::keys::repairable::repair_share_part2(&deltas);
    Ok(sigma.serialize())
}

/// RTS part 3 (participant): sum the helpers' sigmas into the
/// reconstructed `KeyPackage`.
///
/// SECURITY: `repair_share_part3` performs NO verification — it derives
/// the verifying share from whatever the sigmas sum to. The caller MUST
/// check the returned package's verifying share against the committee's
/// consensus `verifying_shares[self]` before installing it (a single
/// malicious or epoch-skewed sigma otherwise installs a garbage share
/// that poisons every future threshold signature this node emits).
pub fn repair_part3_local(
    sigma_bytes: &[Vec<u8>],
    participant: Identifier,
    pub_key_package: &PublicKeyPackage,
) -> Result<KeyPackage, String> {
    let mut sigmas = Vec::with_capacity(sigma_bytes.len());
    for bytes in sigma_bytes {
        sigmas.push(
            frost_ristretto255::keys::repairable::Sigma::deserialize(bytes)
                .map_err(|e| format!("Sigma::deserialize: {e}"))?,
        );
    }
    frost_ristretto255::keys::repairable::repair_share_part3(&sigmas, participant, pub_key_package)
        .map_err(|e| format!("repair_share_part3: {e}"))
}

/// ZEB-1027: rebuild the committee's `PublicKeyPackage` from the
/// persisted consensus bytes (`CommitteeState.verifying_shares` mapped
/// to identifiers + `joint_verifying_key` + `threshold`). This is what
/// lets a RESTARTED node — whose in-memory `local_pub_key_package` died
/// with the process — run RTS part 3 or serve as the old-package input
/// elsewhere, entirely from the sealed `dfrost.cbor` snapshot's public
/// state.
pub fn pub_key_package_from_bytes(
    verifying_shares: &BTreeMap<Identifier, [u8; 32]>,
    joint_vk_bytes: &[u8; 32],
    threshold: u16,
) -> Result<PublicKeyPackage, String> {
    let vk = VerifyingKey::deserialize(joint_vk_bytes)
        .map_err(|e| format!("VerifyingKey::deserialize: {e}"))?;
    let mut shares: BTreeMap<Identifier, VerifyingShare> = BTreeMap::new();
    for (id, bytes) in verifying_shares {
        shares.insert(
            *id,
            VerifyingShare::deserialize(bytes)
                .map_err(|e| format!("VerifyingShare::deserialize: {e}"))?,
        );
    }
    Ok(PublicKeyPackage::new(shares, vk, Some(threshold)))
}

// ── ZEB-295 Phase 6: FROST→ElGamal primitive bridges ─────────────────────────
//
// The threshold-ElGamal scheme used for ballot-secret ratification (spec §1)
// reuses the FROST committee's key material directly:
//   - joint verifying key Y = G·x       → ElGamal encryption key
//   - per-member verifying share Y_i    → DLEQ verifier basis at T2 apply
//   - per-member signing share x_i      → ElGamal decryption secret share
//
// FROST exposes these as wrapper types (`SigningShare`, `VerifyingShare`,
// `VerifyingKey`); the threshold-ElGamal helpers operate on raw
// `curve25519_dalek` types. These three thin converters bridge the gap
// without re-deriving any secret material — the FROST `SigningShare` IS the
// per-member decryption share (no separate ceremony needed).

/// Expose this committee member's signing share `x_i` as a curve25519-dalek
/// Scalar — the same Scalar that the FROST library internally holds.
/// Used as the threshold-ElGamal decryption secret share. Spec §1
/// "FROST `signing_share` IS the per-member ElGamal decryption secret share x_i".
pub fn signing_share_as_scalar(kp: &KeyPackage) -> curve25519_dalek::scalar::Scalar {
    // `SigningShare::serialize` is infallible for Ristretto255 and returns
    // a 32-byte canonical Scalar encoding. Copy into a fixed array and
    // round-trip through `Scalar::from_canonical_bytes` — the unwrap is
    // safe because FROST never produces a non-canonical share.
    let bytes = kp.signing_share().serialize();
    debug_assert_eq!(bytes.len(), 32, "Ristretto SigningShare is 32 bytes");
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Option::from(curve25519_dalek::scalar::Scalar::from_canonical_bytes(arr))
        .expect("FROST SigningShare must be a canonical Ristretto scalar")
}

/// Expose a single committee member's verifying share `Y_i = G·x_i` as a
/// curve25519-dalek RistrettoPoint. Used by T2 (DLEQ verify) at apply time.
pub fn verifying_share_to_point(
    vs: &VerifyingShare,
) -> curve25519_dalek::ristretto::RistrettoPoint {
    use curve25519_dalek::ristretto::CompressedRistretto;
    let bytes = verifying_share_to_bytes(vs);
    CompressedRistretto::from_slice(&bytes)
        .expect("32 bytes")
        .decompress()
        .expect("FROST VerifyingShare must be a valid Ristretto point")
}

/// Expose the joint committee verifying key `Y = G·x` as a RistrettoPoint —
/// the ElGamal encryption key voters target. Spec §1.
pub fn joint_verifying_key_to_point(
    vk: &VerifyingKey,
) -> curve25519_dalek::ristretto::RistrettoPoint {
    use curve25519_dalek::ristretto::CompressedRistretto;
    let bytes = verifying_key_to_bytes(vk);
    CompressedRistretto::from_slice(&bytes)
        .expect("32 bytes")
        .decompress()
        .expect("FROST VerifyingKey must be a valid Ristretto point")
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

    // ── ZEB-295 Phase 6: FROST→ElGamal bridge round-trips ────────────────

    /// Helper: run a complete 2-of-3 FROST DKG and return each participant's
    /// `(KeyPackage, PublicKeyPackage)`. Mirrors the boilerplate from
    /// `dkg_part2_produces_one_package_per_other_participant` but carries
    /// the ceremony all the way through part3.
    fn run_3_party_dkg_2_of_3() -> Vec<(Identifier, KeyPackage, PublicKeyPackage)> {
        let ids: Vec<Identifier> = (0..3).map(identifier_for_index).collect();

        // ── Round 1: every participant runs part1 locally and broadcasts the
        //    round-1 package to every other participant.
        let mut r1_secrets: BTreeMap<Identifier, dkg::round1::SecretPackage> = BTreeMap::new();
        let mut r1_pkgs: BTreeMap<Identifier, Vec<u8>> = BTreeMap::new();
        for id in &ids {
            let (sec, pkg) = dkg_part1_local(*id, 3, 2).expect("part1");
            r1_secrets.insert(*id, sec);
            r1_pkgs.insert(*id, pkg);
        }

        // ── Round 2: every participant runs part2 against the round-1 packages
        //    received from the OTHERS, producing pairwise round-2 packages.
        //    `r2_outbound[sender][recipient]` is the encoded round-2 package
        //    that `sender` sends privately to `recipient`.
        let mut r2_secrets: BTreeMap<Identifier, dkg::round2::SecretPackage> = BTreeMap::new();
        let mut r2_outbound: BTreeMap<Identifier, BTreeMap<Identifier, Vec<u8>>> = BTreeMap::new();
        for id in &ids {
            // Move the round-1 secret out (consumed by part2).
            let sec = r1_secrets
                .remove(id)
                .expect("each participant has a round-1 secret");
            // Round-1 packages received from the OTHERS.
            let received_r1: BTreeMap<Identifier, Vec<u8>> = r1_pkgs
                .iter()
                .filter(|(other, _)| *other != id)
                .map(|(other, bytes)| (*other, bytes.clone()))
                .collect();
            let (r2_sec, r2_out) = dkg_part2_local(sec, &received_r1).expect("part2");
            r2_secrets.insert(*id, r2_sec);
            r2_outbound.insert(*id, r2_out);
        }

        // ── Round 3: each participant gathers the round-2 packages addressed
        //    to it (one from each of the other 2 senders) and finalizes.
        let mut out: Vec<(Identifier, KeyPackage, PublicKeyPackage)> = Vec::new();
        for id in &ids {
            // Round-1 packages received from the OTHERS (same as in round 2).
            let received_r1: BTreeMap<Identifier, Vec<u8>> = r1_pkgs
                .iter()
                .filter(|(other, _)| *other != id)
                .map(|(other, bytes)| (*other, bytes.clone()))
                .collect();
            // Round-2 packages addressed TO this participant by each OTHER sender.
            let mut received_r2: BTreeMap<Identifier, Vec<u8>> = BTreeMap::new();
            for (sender, out_map) in &r2_outbound {
                if sender == id {
                    continue;
                }
                let bytes = out_map
                    .get(id)
                    .cloned()
                    .expect("every other sender produced a round-2 package for me");
                received_r2.insert(*sender, bytes);
            }
            let r2_sec = r2_secrets
                .get(id)
                .expect("each participant has a round-2 secret");
            let (kp, pkp) = dkg_part3_local(r2_sec, &received_r1, &received_r2).expect("part3");
            out.push((*id, kp, pkp));
        }
        out
    }

    // ── ZEB-1027: refresh + repair wrapper round-trips ───────────────────

    /// Full 3-party refresh over the wrappers: joint VK preserved,
    /// every signing share rotated, and the refreshed shares still
    /// produce a valid FROST signature under the ORIGINAL joint VK.
    #[test]
    fn refresh_round_trip_preserves_vk_and_rotates_shares_zeb1027() {
        let parties = run_3_party_dkg_2_of_3();
        let ids: Vec<Identifier> = parties.iter().map(|(id, _, _)| *id).collect();

        // Round 1: zero-sharing commitments, broadcast.
        let mut r1_secrets: BTreeMap<Identifier, dkg::round1::SecretPackage> = BTreeMap::new();
        let mut r1_pkgs: BTreeMap<Identifier, Vec<u8>> = BTreeMap::new();
        for id in &ids {
            let (sec, pkg) = refresh_part1_local(*id, 3, 2).expect("refresh part1");
            r1_secrets.insert(*id, sec);
            r1_pkgs.insert(*id, pkg);
        }

        // Round 2: pairwise refresh shares.
        let mut r2_secrets: BTreeMap<Identifier, dkg::round2::SecretPackage> = BTreeMap::new();
        let mut r2_outbound: BTreeMap<Identifier, BTreeMap<Identifier, Vec<u8>>> = BTreeMap::new();
        for id in &ids {
            let sec = r1_secrets.remove(id).unwrap();
            let received: BTreeMap<Identifier, Vec<u8>> = r1_pkgs
                .iter()
                .filter(|(o, _)| *o != id)
                .map(|(o, b)| (*o, b.clone()))
                .collect();
            let (r2_sec, r2_out) = refresh_part2_local(sec, &received).expect("refresh part2");
            r2_secrets.insert(*id, r2_sec);
            r2_outbound.insert(*id, r2_out);
        }

        // Finalization: every party rotates its share.
        let mut refreshed: BTreeMap<Identifier, (KeyPackage, PublicKeyPackage)> = BTreeMap::new();
        for (id, old_kp, old_pkp) in &parties {
            let received_r1: BTreeMap<Identifier, Vec<u8>> = r1_pkgs
                .iter()
                .filter(|(o, _)| *o != id)
                .map(|(o, b)| (*o, b.clone()))
                .collect();
            let mut received_r2: BTreeMap<Identifier, Vec<u8>> = BTreeMap::new();
            for (sender, out) in &r2_outbound {
                if sender != id {
                    received_r2.insert(*sender, out.get(id).cloned().expect("pairwise pkg"));
                }
            }
            let (new_kp, new_pkp) = refresh_part3_local(
                r2_secrets.get(id).unwrap(),
                &received_r1,
                &received_r2,
                old_pkp.clone(),
                old_kp.clone(),
            )
            .expect("refresh part3");
            // VK preserved; signing share rotated.
            assert_eq!(
                verifying_key_to_bytes(new_pkp.verifying_key()),
                verifying_key_to_bytes(old_pkp.verifying_key()),
                "refresh must preserve the joint verifying key"
            );
            assert_ne!(
                old_kp.signing_share().serialize(),
                new_kp.signing_share().serialize(),
                "refresh must rotate the signing share"
            );
            refreshed.insert(*id, (new_kp, new_pkp));
        }

        // The refreshed shares sign under the ORIGINAL joint VK.
        let signers: Vec<Identifier> = ids.iter().take(2).copied().collect();
        let mut nonces_map = BTreeMap::new();
        let mut commitments_map = BTreeMap::new();
        for id in &signers {
            let (kp, _) = refreshed.get(id).unwrap();
            let (nonces, commitments) =
                frost_ristretto255::round1::commit(kp.signing_share(), &mut rand_core::OsRng);
            nonces_map.insert(*id, nonces);
            commitments_map.insert(*id, commitments);
        }
        let msg = b"zeb1027 refresh signing check";
        let signing_package = frost_ristretto255::SigningPackage::new(commitments_map, msg);
        let mut shares = BTreeMap::new();
        for id in &signers {
            let (kp, _) = refreshed.get(id).unwrap();
            let share =
                frost_ristretto255::round2::sign(&signing_package, nonces_map.get(id).unwrap(), kp)
                    .expect("round2 sign");
            shares.insert(*id, share);
        }
        let (_, _, old_pkp) = &parties[0];
        let (_, new_pkp) = refreshed.get(&ids[0]).unwrap();
        let sig = frost_ristretto255::aggregate(&signing_package, &shares, new_pkp)
            .expect("aggregate under refreshed package");
        old_pkp
            .verifying_key()
            .verify(msg, &sig)
            .expect("signature must verify under the ORIGINAL joint verifying key");
    }

    /// RTS round-trip: two helpers reconstruct the third party's share
    /// exactly, and `pub_key_package_from_bytes` rebuilds a package that
    /// part 3 accepts (the restart path uses exactly that input).
    #[test]
    fn repair_round_trip_reconstructs_lost_share_zeb1027() {
        let parties = run_3_party_dkg_2_of_3();
        let (lost_id, lost_kp, _) = &parties[2];
        let helpers: Vec<Identifier> = vec![parties[0].0, parties[1].0];

        // Each helper produces deltas for every declared helper.
        let mut deltas_by_helper: BTreeMap<Identifier, BTreeMap<Identifier, Vec<u8>>> =
            BTreeMap::new();
        for (id, kp, _) in parties.iter().take(2) {
            let deltas = repair_part1_local(&helpers, kp, *lost_id).expect("repair part1");
            assert_eq!(deltas.len(), helpers.len(), "one delta per declared helper");
            deltas_by_helper.insert(*id, deltas);
        }

        // Each helper sums the deltas addressed to it into a sigma.
        let mut sigmas: Vec<Vec<u8>> = Vec::new();
        for helper in &helpers {
            let received: Vec<Vec<u8>> = deltas_by_helper
                .values()
                .map(|m| m.get(helper).cloned().expect("delta for helper"))
                .collect();
            sigmas.push(repair_part2_local(&received).expect("repair part2"));
        }

        // Participant rebuilds the public package from persisted-shaped
        // bytes (the restart path) and reconstructs its share.
        let (_, _, pkp) = &parties[0];
        let shares_bytes: BTreeMap<Identifier, [u8; 32]> = pkp
            .verifying_shares()
            .iter()
            .map(|(id, vs)| (*id, verifying_share_to_bytes(vs)))
            .collect();
        let rebuilt = pub_key_package_from_bytes(
            &shares_bytes,
            &verifying_key_to_bytes(pkp.verifying_key()),
            2,
        )
        .expect("pub_key_package_from_bytes");
        let repaired = repair_part3_local(&sigmas, *lost_id, &rebuilt).expect("repair part3");

        assert_eq!(
            repaired.signing_share().serialize(),
            lost_kp.signing_share().serialize(),
            "RTS must reconstruct the exact lost signing share"
        );
        assert_eq!(
            verifying_share_to_bytes(repaired.verifying_share()),
            verifying_share_to_bytes(lost_kp.verifying_share()),
            "reconstructed verifying share must match the committee's consensus entry"
        );
    }

    /// A corrupted sigma is NOT caught by part 3 itself — the derived
    /// verifying share simply diverges from the consensus entry. Pins
    /// the exact check the log's finalize path must perform before
    /// installing a repaired share.
    #[test]
    fn repair_with_corrupt_sigma_yields_mismatched_verifying_share_zeb1027() {
        let parties = run_3_party_dkg_2_of_3();
        let (lost_id, lost_kp, _) = &parties[2];
        let helpers: Vec<Identifier> = vec![parties[0].0, parties[1].0];

        let mut deltas_by_helper: BTreeMap<Identifier, BTreeMap<Identifier, Vec<u8>>> =
            BTreeMap::new();
        for (id, kp, _) in parties.iter().take(2) {
            deltas_by_helper.insert(
                *id,
                repair_part1_local(&helpers, kp, *lost_id).expect("repair part1"),
            );
        }
        let mut sigmas: Vec<Vec<u8>> = Vec::new();
        for helper in &helpers {
            let received: Vec<Vec<u8>> = deltas_by_helper
                .values()
                .map(|m| m.get(helper).cloned().unwrap())
                .collect();
            sigmas.push(repair_part2_local(&received).expect("repair part2"));
        }
        // Corrupt one sigma: replace with a canonical-but-wrong scalar
        // (1). Deserialization succeeds; only the verifying-share check
        // can catch it.
        sigmas[1] = {
            let mut one = [0u8; 32];
            one[0] = 1;
            one.to_vec()
        };

        let (_, _, pkp) = &parties[0];
        let repaired = repair_part3_local(&sigmas, *lost_id, pkp).expect("part3 does not verify");
        assert_ne!(
            verifying_share_to_bytes(repaired.verifying_share()),
            verifying_share_to_bytes(lost_kp.verifying_share()),
            "corrupt sigma must surface as a verifying-share mismatch"
        );
    }

    #[test]
    fn joint_verifying_key_round_trip_with_elgamal_point() {
        // Run a 2-of-3 DKG, extract the joint VK from the PublicKeyPackage,
        // convert to a RistrettoPoint, recompress, and compare against the
        // canonical `verifying_key_to_bytes` encoding. The conversion must
        // be byte-exact — any drift would silently desync the ElGamal voter
        // encryption key from the FROST joint VK.
        let parties = run_3_party_dkg_2_of_3();
        let (_id, _kp, pkp) = &parties[0];
        let vk = pkp.verifying_key();
        let point = joint_verifying_key_to_point(vk);
        let canonical_bytes = verifying_key_to_bytes(vk);
        let point_bytes = point.compress().to_bytes();
        assert_eq!(
            point_bytes, canonical_bytes,
            "joint_verifying_key_to_point must round-trip with verifying_key_to_bytes"
        );
    }

    #[test]
    fn signing_share_as_scalar_round_trips_through_verifying_share() {
        // After DKG: G * signing_share_as_scalar(kp) == verifying_share_to_point(kp.verifying_share()).
        // i.e. our exposed Scalar matches the FROST library's exposed VerifyingShare.
        use curve25519_dalek::constants::RISTRETTO_BASEPOINT_TABLE;
        let parties = run_3_party_dkg_2_of_3();
        for (id, kp, _pkp) in &parties {
            let x_i = signing_share_as_scalar(kp);
            let y_i_via_basepoint = RISTRETTO_BASEPOINT_TABLE * &x_i;
            let y_i_from_frost = verifying_share_to_point(kp.verifying_share());
            assert_eq!(
                y_i_via_basepoint.compress().to_bytes(),
                y_i_from_frost.compress().to_bytes(),
                "G * x_i must equal Y_i for participant {id:?}",
            );
        }
    }
}
