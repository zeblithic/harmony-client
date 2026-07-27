//! Case E: vine relay discovery — pkarr record codec + resolve (ZEB-811 Task 2).
//!
//! A creator who shares vines publicly publishes a small "relay set" record
//! under a pkarr slot derived from their public hex address
//! (`harmony_pkarr::PkarrCase::Vines`, see `harmony-pkarr/src/derive.rs`).
//! Anyone who follows the creator holds that address and can deterministically
//! find the slot — unlike Case B (identity-keyed), a follower does NOT hold
//! the creator's 64-byte identity pub (addresses are one-way hashes of it), so
//! the record is keyed off the address instead.
//!
//! **Design note (spec deviation, deliberate):** the spec's §1 payload table
//! lists an inner `sg identity_signature`. The pkarr envelope
//! (`PkarrRoutingRecord`) already carries the `#3` identity signature over the
//! blob plus the embedded 64-byte identity pub (`harmony-pkarr/src/record.rs`),
//! and the reachability flavor zero-fills its inner signature on the pkarr
//! path for exactly this reason (`lib.rs`, `ReachabilityAnnouncePayload`
//! construction). The vines payload therefore carries only `rs`/`ts`;
//! authenticity = `verify_inner_sig()` + freshness + the identity-pub→address
//! binding. Task 11 records this in the spec's as-implemented notes.

use serde::{Deserialize, Serialize};

use crate::owner_state_types::{deserialize_bytes_from_bstr, serialize_bytes_as_bstr};

/// Max relay-set entries a vines record may carry (bounds fan-out, mirrors
/// `COMMUNITY_RELAY_ADVERTISERS_MAX`).
pub const VINE_RELAY_SET_MAX: usize = 4;

/// Headroom under pkarr's 1104-byte `SignedPacket` budget. A record that
/// exceeds the budget doesn't fail loudly — it fails as an eternal silent
/// 60s-retry loop (`harmony-pkarr/src/publisher.rs:204-208` logs a warning
/// and retries forever). Rejecting oversize payloads at build time turns
/// that into an immediate, attributable error.
const VINES_RECORD_BLOB_MAX_BYTES: usize = 700;

/// One relay device's dialing coordinates, as advertised by a vine creator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VineRelayEntry {
    #[serde(
        rename = "ep",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub iroh_endpoint_id: [u8; 32],
    #[serde(rename = "hr")]
    pub home_relay: String,
}

/// The vines pkarr record's routing-blob payload (Task 2 §Design note: no
/// inner signature field — authenticity comes from the pkarr envelope).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VineRelayRecordPayload {
    #[serde(rename = "rs")]
    pub relay_set: Vec<VineRelayEntry>,
    #[serde(rename = "ts")]
    pub issued_at_ms: u64,
}

/// The Case-E HKDF `ikm`: the creator's public hex address, hex-decoded to
/// raw bytes (see `PkarrCase::Vines` doc comment for the security rationale —
/// this derives from public input by design).
pub fn vines_ikm(creator_addr_hex: &str) -> Result<Vec<u8>, String> {
    hex::decode(creator_addr_hex).map_err(|e| format!("creator address is not hex: {e}"))
}

/// Derive the ephemeral Ed25519 signing key for a given creator address and
/// epoch. Publisher and resolver both call this with identical inputs and
/// obtain identical keys (publisher signs under it; resolver derives the
/// verifying key and queries the DHT under it).
pub fn vines_key_for_epoch(
    creator_addr_hex: &str,
    epoch_id: u64,
) -> Result<ed25519_dalek::SigningKey, String> {
    let ikm = vines_ikm(creator_addr_hex)?;
    Ok(harmony_pkarr::derive_ephemeral_key(
        harmony_pkarr::PkarrCase::Vines,
        &ikm,
        &epoch_id.to_be_bytes(),
    ))
}

/// Encode a vines payload to its canonical CBOR routing-blob bytes. Rejects
/// an oversize relay set or an encoded blob that would blow pkarr's packet
/// budget (see [`VINES_RECORD_BLOB_MAX_BYTES`]).
pub fn build_vines_record_blob(payload: &VineRelayRecordPayload) -> Result<Vec<u8>, String> {
    if payload.relay_set.len() > VINE_RELAY_SET_MAX {
        return Err(format!(
            "relay_set has {} entries, exceeds max of {VINE_RELAY_SET_MAX}",
            payload.relay_set.len()
        ));
    }
    let mut out = Vec::new();
    ciborium::into_writer(payload, &mut out).map_err(|e| format!("cbor encode: {e}"))?;
    if out.len() > VINES_RECORD_BLOB_MAX_BYTES {
        return Err(format!(
            "vines record blob is {} bytes, exceeds max of {VINES_RECORD_BLOB_MAX_BYTES}",
            out.len()
        ));
    }
    Ok(out)
}

/// Verify a resolved vines pkarr record: inner signature → freshness →
/// identity-pub→address binding → decode + bound the relay set.
pub fn verify_vines_record(
    rec: &harmony_pkarr::PkarrRoutingRecord,
    creator_addr_hex: &str,
    now_ms: u64,
) -> Result<VineRelayRecordPayload, String> {
    rec.verify_inner_sig()
        .map_err(|e| format!("inner signature invalid: {e}"))?;
    rec.verify_freshness(now_ms)
        .map_err(|e| format!("record stale or skewed: {e}"))?;
    let identity_pub_hex = hex::encode(rec.harmony_identity_pub);
    let derived_addr = crate::vine_signing::address_for_identity_pub_hex(&identity_pub_hex)?;
    if derived_addr != creator_addr_hex {
        return Err("record identity pub does not match claimed creator address".to_string());
    }
    let payload: VineRelayRecordPayload = ciborium::from_reader(rec.routing_blob.as_slice())
        .map_err(|e| format!("decode routing_blob: {e}"))?;
    if payload.relay_set.len() > VINE_RELAY_SET_MAX {
        return Err(format!(
            "relay_set has {} entries, exceeds max of {VINE_RELAY_SET_MAX}",
            payload.relay_set.len()
        ));
    }
    Ok(payload)
}

/// Resolve a creator's vine relay set: query the 3-epoch tolerance window in
/// parallel (same pattern as the identity-keyed resolve, `lib.rs`
/// `add_friend_by_key`), take the freshest **verified** record across relays,
/// and return the bound relay set. Verification runs per candidate inside the
/// resolver (ZEB-817) — see the call site below for why post-hoc verification
/// of a single freshest-by-seq winner is not sufficient for this case.
pub async fn resolve_vine_relays(
    resolver: &harmony_pkarr::PkarrResolver,
    creator_addr_hex: &str,
    now_ms: u64,
) -> Result<Vec<VineRelayEntry>, String> {
    let epoch_window = harmony_pkarr::epoch_tolerance_window(now_ms);
    let mut verifying_keys = Vec::with_capacity(epoch_window.len());
    for epoch_id in epoch_window {
        verifying_keys.push(vines_key_for_epoch(creator_addr_hex, epoch_id)?.verifying_key());
    }
    // ZEB-817: verify INSIDE the resolver, not after it. The vines slot key
    // derives from the creator's public address, so anyone can publish a
    // self-consistent record there (outer sig, inner sig and freshness all
    // pass — the inner sig verifies against the record's OWN embedded
    // identity pub). Verifying only the resolver's freshest-by-seq winner
    // would let a squat with a higher seq shadow the genuine record AND pin
    // the resolver's seq-highwater + positive cache with itself, hiding the
    // genuine record for the process lifetime. `_with` ranks candidates
    // freshest-first and takes the first that passes this predicate;
    // candidates that fail it touch neither surface.
    let verify = |rec: &harmony_pkarr::PkarrRoutingRecord| {
        verify_vines_record(rec, creator_addr_hex, now_ms).is_ok()
    };
    let rec = resolver
        .resolve_window_freshest_with(&verifying_keys, &verify)
        .await
        .map_err(|e| format!("pkarr resolve: {e}"))?
        // A record that exists but verifies for nobody now lands here too —
        // correct: the pull driver treats this Err as "retry next pass" and
        // deliberately preserves its cached relay hint meanwhile.
        .ok_or_else(|| "no vines record found for creator".to_string())?;
    // Re-run the (pure, cheap) chain on the winner to get the decoded payload.
    let payload = verify_vines_record(&rec, creator_addr_hex, now_ms)?;
    Ok(payload.relay_set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vine_signing::{
        identity_pub_64, identity_signing_key, signer_address, test_identity,
    };

    #[test]
    fn payload_round_trips_via_cbor() {
        let p = VineRelayRecordPayload {
            relay_set: vec![VineRelayEntry {
                iroh_endpoint_id: [7u8; 32],
                home_relay: "https://relay.example".into(),
            }],
            issued_at_ms: 1_000,
        };
        let blob = build_vines_record_blob(&p).unwrap();
        let back: VineRelayRecordPayload = ciborium::from_reader(blob.as_slice()).unwrap();
        assert_eq!(back.relay_set.len(), 1);
        assert_eq!(back.relay_set[0].iroh_endpoint_id, [7u8; 32]);
        assert_eq!(back.issued_at_ms, 1_000);
    }

    #[test]
    fn oversize_relay_set_is_rejected_at_build() {
        let entry = VineRelayEntry {
            iroh_endpoint_id: [1u8; 32],
            home_relay: "https://r".into(),
        };
        let p = VineRelayRecordPayload {
            relay_set: vec![entry; VINE_RELAY_SET_MAX + 1],
            issued_at_ms: 0,
        };
        assert!(build_vines_record_blob(&p).is_err());
    }

    #[test]
    fn slot_derivation_is_stable_and_address_scoped() {
        let k1 = vines_key_for_epoch("aabbccdd00112233aabbccdd00112233", 42).unwrap();
        let k2 = vines_key_for_epoch("aabbccdd00112233aabbccdd00112233", 42).unwrap();
        let k3 = vines_key_for_epoch("ffeeddcc00112233aabbccdd00112233", 42).unwrap();
        assert_eq!(k1.verifying_key(), k2.verifying_key());
        assert_ne!(k1.verifying_key(), k3.verifying_key());
        assert!(vines_key_for_epoch("not-hex", 42).is_err());
    }

    #[test]
    fn record_verification_binds_identity_to_address() {
        // Build a real record signed by a real identity, then verify against
        // the right and wrong addresses.
        let identity = test_identity();
        let addr = signer_address(&identity);
        let payload = VineRelayRecordPayload {
            relay_set: vec![],
            issued_at_ms: 5_000,
        };
        let blob = build_vines_record_blob(&payload).unwrap();
        let rec = harmony_pkarr::PkarrRoutingRecord::sign_new(
            blob,
            identity_pub_64(&identity),
            5_000,
            5_000 + crate::reachability_record::REACHABILITY_RECORD_TTL_MS,
            &identity_signing_key(&identity),
        )
        .unwrap();
        assert!(verify_vines_record(&rec, &addr, 6_000).is_ok());
        assert!(
            verify_vines_record(&rec, "00112233445566770011223344556677", 6_000).is_err(),
            "wrong address must fail the binding"
        );
    }
}
