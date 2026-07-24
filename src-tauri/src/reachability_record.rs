//! ZEB-321 Phase 1: ReachabilityAnnounce CRDT event payload.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §5.

use serde::Serialize;
use std::net::SocketAddr;

use crate::owner_state_crypto::{
    canonical_cbor_encode, sealed::CanonicalPayloadSealed, CanonicalPayload, CryptoError,
};
use crate::owner_state_types::{serialize_bytes_as_bstr, Hlc, OwnerAddr};

/// Signed validity window applied to every reachability pkarr record
/// (identity / community / friend / invite). One epoch (7 days): covers the
/// ~3.5-day republish gap with margin and never outlives the per-epoch
/// ephemeral key. The resolver (`harmony_pkarr::verify_freshness`) honors a
/// record while `now <= announced_at + this`.
pub(crate) const REACHABILITY_RECORD_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

// The record shape + delegate type + canonical CBOR now live in the core crate
// (harmony-reachability, ZEB-744 PR 1). This module keeps only the client-side
// inner-signature scheme, butler-set accessors, and freshness policy, operating
// on the core-owned record through its public fields.
pub use harmony_reachability::{DelegateEndpoint as ButlerSetEntry, ReachabilityAnnouncePayload};

/// Reader-side accessor for the butler set (ZEB-418 SP2 P1): returns the
/// advertised entries iff the `bs_at` stamp is present and within
/// [`crate::butler_deposit::BUTLER_SET_FRESHNESS_MS`] of `now_ms`, truncated
/// to [`crate::butler_deposit::BUTLER_SET_MAX_ENTRIES`]. A missing (zero) or
/// stale stamp yields an empty vec — the caller falls through to the
/// existing retry chain (spec §3: a stale ad can never make delivery worse
/// than today). Sender/receiver clock skew is tolerated up to ONE freshness
/// window FORWARD (`bs_at` ≤ `now_ms + BUTLER_SET_FRESHNESS_MS`); beyond
/// that the stamp is treated as stale — a maliciously future-stamped record
/// must not stay "fresh" until the wall clock catches up, pinning the
/// deposit rung onto dead butlers (PR #221 round 1).
pub fn fresh_butler_set(blob: &ReachabilityAnnouncePayload, now_ms: u64) -> Vec<ButlerSetEntry> {
    if blob.bs_at == 0
        || blob.bs_at > now_ms.saturating_add(crate::butler_deposit::BUTLER_SET_FRESHNESS_MS)
        || now_ms.saturating_sub(blob.bs_at) > crate::butler_deposit::BUTLER_SET_FRESHNESS_MS
    {
        return Vec::new();
    }
    blob.butler_set
        .iter()
        .take(crate::butler_deposit::BUTLER_SET_MAX_ENTRIES)
        .cloned()
        .collect()
}

/// Like [`fresh_butler_set`] but WITHOUT the freshness window — for butler-sets
/// sourced from the durable community CRDT (Decision 3). The seal-target vk is
/// durable even when the advertised endpoint has drifted (a stale endpoint just
/// fails the dial and falls through; the vk stays a valid seal target). A
/// zero/missing `bs_at` still means "no butler-set".
pub fn durable_butler_set(blob: &ReachabilityAnnouncePayload) -> Vec<ButlerSetEntry> {
    if blob.bs_at == 0 {
        return Vec::new();
    }
    blob.butler_set
        .iter()
        .take(crate::butler_deposit::BUTLER_SET_MAX_ENTRIES)
        .cloned()
        .collect()
}

impl CanonicalPayloadSealed for ReachabilityAnnouncePayload {}
impl CanonicalPayload for ReachabilityAnnouncePayload {}

/// Convenience: canonical-encode for hashing / signing.
pub fn canonical_payload_bytes(p: &ReachabilityAnnouncePayload) -> Result<Vec<u8>, CryptoError> {
    canonical_cbor_encode(p)
}

/// Deterministic byte string the inner identity signature covers:
/// CBOR of (nd, rl, da, ts, ac, hl, bs, ba). The actor + hlc are pulled
/// from the surrounding membership envelope; they're NOT part of the
/// payload struct itself but are bound into the signature so a replay
/// attacker can't lift a `ReachabilityAnnouncePayload` from one envelope
/// and re-attach it under a different actor or HLC. The butler set (`bs`)
/// and its freshness stamp (`ba`) ARE payload fields, bound here so they
/// can't be forged or stripped (see the butler-set binding note below).
///
/// All field keys are 2 chars to satisfy the same-length-keys
/// invariant at this nesting level. This sig-input map MUST be kept
/// distinct from `ReachabilityAnnouncePayload`'s wire shape (which has
/// the self-contained fields nd/rl/da/ts/sg/bs/ba but NOT ac/hl);
/// confusing them would let a peer replay the inner-sig bytes verbatim.
///
/// **Butler-set binding (ZEB-418 durable seal-targets):** the recipient's
/// `butler_set` (`bs`) + freshness stamp `bs_at` (`ba`) are folded into the
/// preimage so a co-member or relay can neither forge nor strip the device-key
/// seal-targets used for offline DM deposit — they're authenticated by the
/// recipient's own harmony identity. NOTE: this is a flag-day preimage change
/// — records signed before this field was added will NOT verify against the
/// extended preimage (and vice-versa). The pkarr routing blob is unaffected:
/// it zero-fills `identity_signature` and never calls this function, so only
/// durable CRDT membership records carry the extended signature.
///
/// **Not a `CanonicalPayload`**: this function encodes via raw
/// `ciborium::into_writer` rather than the repo's `canonical_cbor_encode`
/// helper because the input struct holds references and the
/// `CanonicalPayload` sealed-trait API requires owned types. The
/// underlying serde impls for all eight fields (`[u8;32]`, `&str`,
/// `&[SocketAddr]`, `u64`, `&OwnerAddr`, `&Hlc`, `&[ButlerSetEntry]`,
/// `u64`) are deterministic,
/// so the byte sequence is stable across runs and machines — sufficient
/// for sign/verify symmetry. If a future requirement needs strict
/// canonical (length-prefix-sorted) encoding here, switch to an owned
/// struct that impls `CanonicalPayload`. Per CodeRabbit on PR #157.
// Each argument is a distinct semantic field of the signed preimage; bundling
// them into a struct would obscure the sign/verify contract.
#[allow(clippy::too_many_arguments)]
pub fn inner_signed_bytes(
    iroh_node_id: &[u8; 32],
    home_relay_url: &str,
    direct_addresses: &[SocketAddr],
    announced_at_ms: u64,
    actor: &OwnerAddr,
    hlc: &Hlc,
    butler_set: &[ButlerSetEntry],
    bs_at: u64,
) -> Result<Vec<u8>, CryptoError> {
    #[derive(Serialize)]
    struct InnerSigInput<'a> {
        #[serde(rename = "nd", serialize_with = "serialize_bytes_as_bstr")]
        nd: &'a [u8; 32],
        #[serde(rename = "rl")]
        rl: &'a str,
        #[serde(rename = "da")]
        da: &'a [SocketAddr],
        #[serde(rename = "ts")]
        ts: u64,
        #[serde(rename = "ac")]
        ac: &'a OwnerAddr,
        #[serde(rename = "hl")]
        hl: &'a Hlc,
        #[serde(rename = "bs")]
        bs: &'a [ButlerSetEntry],
        #[serde(rename = "ba")]
        ba: u64,
    }
    let input = InnerSigInput {
        nd: iroh_node_id,
        rl: home_relay_url,
        da: direct_addresses,
        ts: announced_at_ms,
        ac: actor,
        hl: hlc,
        bs: butler_set,
        ba: bs_at,
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&input, &mut buf).map_err(|e| CryptoError::CborEncode(format!("{e}")))?;
    Ok(buf)
}

/// Sign a fresh `ReachabilityAnnouncePayload` using the device's harmony
/// identity signing key. Caller is responsible for ensuring `actor`
/// matches the identity (`identity.identity.address_hash`).
#[allow(clippy::too_many_arguments)]
pub fn build_signed_payload(
    iroh_node_id: [u8; 32],
    home_relay_url: String,
    direct_addresses: Vec<SocketAddr>,
    announced_at_ms: u64,
    actor: &OwnerAddr,
    hlc: &Hlc,
    butler_set: Vec<ButlerSetEntry>,
    bs_at: u64,
    identity: &harmony_identity::PrivateIdentity,
) -> Result<ReachabilityAnnouncePayload, CryptoError> {
    let inner = inner_signed_bytes(
        &iroh_node_id,
        &home_relay_url,
        &direct_addresses,
        announced_at_ms,
        actor,
        hlc,
        &butler_set,
        bs_at,
    )?;
    let sig = identity.sign(&inner);
    Ok(ReachabilityAnnouncePayload {
        iroh_node_id,
        home_relay_url,
        direct_addresses,
        announced_at_ms,
        identity_signature: sig,
        butler_set,
        bs_at,
    })
}

/// ZEB-339: sign a fresh `ReachabilityAnnouncePayload`'s inner
/// `identity_signature` with the harmony-owner ENROLLED device key (#2).
///
/// T4 rewired RCH2 verification to check the inner `identity_signature`
/// against the resolved enrolled device key (not the Reticulum identity),
/// so the production reachability mint MUST sign the inner sig with device
/// #2. Caller is responsible for ensuring `actor` matches the enrolled
/// device key's owner.
#[allow(clippy::too_many_arguments)]
pub fn build_signed_payload_with_key(
    iroh_node_id: [u8; 32],
    home_relay_url: String,
    direct_addresses: Vec<SocketAddr>,
    announced_at_ms: u64,
    actor: &OwnerAddr,
    hlc: &Hlc,
    butler_set: Vec<ButlerSetEntry>,
    bs_at: u64,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<ReachabilityAnnouncePayload, CryptoError> {
    use ed25519_dalek::Signer;
    let inner = inner_signed_bytes(
        &iroh_node_id,
        &home_relay_url,
        &direct_addresses,
        announced_at_ms,
        actor,
        hlc,
        &butler_set,
        bs_at,
    )?;
    let sig = signing_key.sign(&inner).to_bytes();
    Ok(ReachabilityAnnouncePayload {
        iroh_node_id,
        home_relay_url,
        direct_addresses,
        announced_at_ms,
        identity_signature: sig,
        butler_set,
        bs_at,
    })
}

/// Verify the inner identity signature against the given Ed25519
/// verifying key — the 32-byte Ed25519 half of the 64-byte
/// `harmony_identity::Identity::to_public_bytes()`.
pub fn verify_inner_signature(
    p: &ReachabilityAnnouncePayload,
    actor: &OwnerAddr,
    hlc: &Hlc,
    actor_ed25519_pub: &ed25519_dalek::VerifyingKey,
) -> Result<(), InnerSigError> {
    let bytes = inner_signed_bytes(
        &p.iroh_node_id,
        &p.home_relay_url,
        &p.direct_addresses,
        p.announced_at_ms,
        actor,
        hlc,
        &p.butler_set,
        p.bs_at,
    )
    .map_err(|_| InnerSigError::Encode)?;
    let sig = ed25519_dalek::Signature::from_bytes(&p.identity_signature);
    actor_ed25519_pub
        .verify_strict(&bytes, &sig)
        .map_err(|_| InnerSigError::Invalid)
}

/// ZEB-376: freshness gate for a reachability payload relayed OUT-OF-BAND (an
/// `Introduction`), where there is NO pkarr [`harmony_pkarr::PkarrRoutingRecord`]
/// envelope to run `verify_freshness` against — only the payload's self-stamped
/// `announced_at_ms`. Bounds it the same way the Case-B/Case-D initiator's
/// `PkarrRoutingRecord::verify_freshness` bounds a resolved record:
///
/// * **Forged-future guard** — reject a stamp more than
///   [`crate::community_membership::REACHABILITY_TIMESTAMP_SKEW_MAX_MS`] ahead of
///   the reader's clock (a maliciously future-stamped record must not stay fresh
///   forever, the same PR #221-class hazard `fresh_butler_set` guards).
/// * **Age bound** — reject a stamp older than one signed reachability TTL
///   ([`REACHABILITY_RECORD_TTL_MS`]); this mirrors the pkarr record's signed
///   `valid_until_ms` upper bound synthesized from the epoch TTL, since the
///   relayed payload carries no separate `valid_until_ms`.
///
/// Returns a human-readable `Err(String)` (the acceptor logs it and drops the
/// introduction) rather than a typed error — the caller has nothing to branch on.
pub(crate) fn reachability_freshness_check(
    reachability: &ReachabilityAnnouncePayload,
    now_ms: u64,
) -> Result<(), String> {
    let announced = reachability.announced_at_ms;
    let future_tolerance = crate::community_membership::REACHABILITY_TIMESTAMP_SKEW_MAX_MS;
    if announced > now_ms.saturating_add(future_tolerance) {
        return Err(format!(
            "relayed reachability announced_at_ms {announced} is implausibly in the future \
             (now {now_ms}, tolerance {future_tolerance}ms)"
        ));
    }
    if now_ms > announced.saturating_add(REACHABILITY_RECORD_TTL_MS) {
        return Err(format!(
            "relayed reachability is stale: announced_at_ms {announced} + TTL \
             {REACHABILITY_RECORD_TTL_MS}ms < now {now_ms}"
        ));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InnerSigError {
    /// Unreachable in practice — `ciborium::into_writer` against a
    /// `Vec<u8>` writer is infallible (no IO failure mode). Kept as
    /// defensive-coding hygiene for future refactors that might use a
    /// real IO sink.
    #[error("inner reachability signature failed to encode")]
    Encode,
    #[error("inner reachability signature invalid")]
    Invalid,
}

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_identity::PrivateIdentity;

    fn fixture_hlc() -> Hlc {
        Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "fix".into(),
        }
    }

    fn fixture_payload() -> ReachabilityAnnouncePayload {
        ReachabilityAnnouncePayload {
            iroh_node_id: [0xAB; 32],
            home_relay_url: "https://derp.example/".into(),
            direct_addresses: vec![],
            announced_at_ms: 1_700_000_000_000,
            identity_signature: [0xCD; 64],
            butler_set: Vec::new(),
            bs_at: 0,
        }
    }

    /// Byte-lock for the inner-signature preimage (`inner_signed_bytes`), which now
    /// spans the repo boundary: record fields come from the core crate, `actor`/`hlc`
    /// from the client. DO NOT REGENERATE — regenerating would mask a preimage drift.
    #[test]
    fn inner_signed_bytes_preimage_is_wire_pinned() {
        let hlc = fixture_hlc();
        let actor = OwnerAddr([0x11; 16]);
        let p = fixture_payload();
        let bytes = inner_signed_bytes(
            &p.iroh_node_id,
            &p.home_relay_url,
            &p.direct_addresses,
            p.announced_at_ms,
            &actor,
            &hlc,
            &p.butler_set,
            p.bs_at,
        )
        .expect("preimage");
        const EXPECTED_PREIMAGE_HEX: &str = "a8626e645820abababababababababababababababababababababababababababababababab62726c7568747470733a2f2f646572702e6578616d706c652f626461806274731b0000018bcfe56800626163501111111111111111111111111111111162686ca361771b0000018bcfe56800616c006164636669786262738062626100";
        assert_eq!(
            hex::encode(&bytes),
            EXPECTED_PREIMAGE_HEX,
            "inner-sig preimage drifted"
        );
    }

    /// THE wire-compat pin (ZEB-418 P1 Task 4): a routing blob WITHOUT a
    /// butler set must encode byte-identically to the pre-butler-set
    /// (legacy) encoding, so deployed peers that decode the published pkarr
    /// routing_blob keep working unchanged.
    ///
    /// The pinned hex below was captured from the struct AS IT EXISTED ON
    /// MAIN before the butler-set fields were added (first verified run on
    /// the unmodified struct, 2026-06-09). DO NOT REGENERATE: a failure
    /// here means the legacy wire shape changed — that is a compat break
    /// with already-published pkarr records, not a fixture to refresh.
    #[test]
    fn routing_blob_without_butler_set_is_wire_identical_to_legacy() {
        const EXPECTED_LEGACY_HEX: &str = "a5626e645820abababababababababababababababababababababababababababababababab62726c7568747470733a2f2f646572702e6578616d706c652f626461806274731b0000018bcfe568006273675840cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";
        let p = fixture_payload();
        let bytes = canonical_payload_bytes(&p).expect("encode");
        let actual = hex::encode(&bytes);
        assert_eq!(
            actual, EXPECTED_LEGACY_HEX,
            "routing blob without butler set drifted from the legacy wire \
             encoding.\nactual hex: {actual}"
        );
    }

    /// Deterministic butler entry for the ZEB-418 tests. `seed` fans out to
    /// every byte field so two entries are distinguishable after round-trip.
    fn fixture_butler_entry(seed: u8) -> ButlerSetEntry {
        ButlerSetEntry {
            device_id: [seed; 16],
            iroh_endpoint_id: [seed.wrapping_add(1); 32],
            device_ed25519_verify: [seed.wrapping_add(2); 32],
            home_relay: "https://use1-1.relay.iroh.network./".into(),
            pinned: false,
        }
    }

    /// Forward-compat direction of the pin: a LEGACY blob (encoded before
    /// the butler-set fields existed) must decode into the extended struct
    /// with the defaults (empty set, zero stamp).
    #[test]
    fn legacy_routing_blob_decodes_with_empty_butler_set() {
        let legacy_bytes = canonical_payload_bytes(&fixture_payload()).expect("encode");
        let decoded: ReachabilityAnnouncePayload =
            ciborium::de::from_reader(&legacy_bytes[..]).expect("decode legacy");
        assert!(decoded.butler_set.is_empty());
        assert_eq!(decoded.bs_at, 0);
    }

    #[test]
    fn routing_blob_with_butler_set_round_trips() {
        let mut p = fixture_payload();
        p.butler_set = vec![fixture_butler_entry(0x10), fixture_butler_entry(0x20)];
        p.bs_at = 1_700_000_000_000;

        let bytes = canonical_payload_bytes(&p).expect("encode");
        let decoded: ReachabilityAnnouncePayload =
            ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded, p);
        assert_eq!(decoded.butler_set.len(), 2);
        assert_eq!(decoded.bs_at, 1_700_000_000_000);
    }

    /// The reader truncates an oversized (adversarial) advertisement to
    /// `BUTLER_SET_MAX_ENTRIES`, preserving priority order.
    #[test]
    fn butler_set_capped_at_two() {
        let mut p = fixture_payload();
        p.butler_set = vec![
            fixture_butler_entry(0x10),
            fixture_butler_entry(0x20),
            fixture_butler_entry(0x30),
        ];
        p.bs_at = 1_700_000_000_000;

        let fresh = fresh_butler_set(&p, p.bs_at);
        assert_eq!(fresh.len(), crate::butler_deposit::BUTLER_SET_MAX_ENTRIES);
        assert_eq!(
            fresh,
            vec![fixture_butler_entry(0x10), fixture_butler_entry(0x20)],
            "cap must keep the first (highest-priority) entries"
        );
    }

    #[test]
    fn stale_bs_at_is_filtered_by_reader() {
        let bs_at: u64 = 1_700_000_000_000;
        let mut p = fixture_payload();
        p.butler_set = vec![fixture_butler_entry(0x10)];
        p.bs_at = bs_at;

        let window = crate::butler_deposit::BUTLER_SET_FRESHNESS_MS;
        // Fresh: at the stamp, at the window edge, and slightly in the
        // future of the reader's clock (bounded forward-skew tolerance).
        assert_eq!(fresh_butler_set(&p, bs_at).len(), 1);
        assert_eq!(fresh_butler_set(&p, bs_at + window).len(), 1);
        assert_eq!(fresh_butler_set(&p, bs_at - 5_000).len(), 1);
        // Forward-skew edge: reader BEHIND the stamp by a full window
        // (bs_at == now + window) is still fresh.
        assert_eq!(fresh_butler_set(&p, bs_at - window).len(), 1);
        // Stale: one past the window → treated as no butler set.
        assert!(fresh_butler_set(&p, bs_at + window + 1).is_empty());
        // Far-future stamp (window + 1 in the reader's future) → rejected:
        // a maliciously future-stamped record must not be fresh-forever
        // (PR #221 round 1).
        assert!(fresh_butler_set(&p, bs_at - window - 1).is_empty());
        // Missing stamp (legacy / elided) → no butler set, regardless of
        // entries being present.
        p.bs_at = 0;
        assert!(fresh_butler_set(&p, 1).is_empty());
    }

    /// Plan-time size gate (spec §3): the full routing blob with TWO butler
    /// entries and realistic field values must stay under 900 bytes,
    /// leaving headroom inside BEP44's ~1000-byte record-value cap for the
    /// outer PkarrRoutingRecord envelope.
    #[test]
    fn encoded_size_with_two_entries_under_bep44_budget() {
        let p = ReachabilityAnnouncePayload {
            iroh_node_id: [0xAB; 32],
            home_relay_url: "https://use1-1.relay.iroh.network./".into(),
            direct_addresses: vec![
                "203.0.113.7:62103".parse().expect("v4 addr"),
                "[2001:db8::1234:5678]:62103".parse().expect("v6 addr"),
            ],
            announced_at_ms: 1_700_000_000_000,
            identity_signature: [0xCD; 64],
            butler_set: vec![fixture_butler_entry(0x10), fixture_butler_entry(0x20)],
            bs_at: 1_700_000_000_000,
        };
        let bytes = canonical_payload_bytes(&p).expect("encode");
        eprintln!(
            "routing blob with 2 butler entries: {} bytes (budget < 900)",
            bytes.len()
        );
        assert!(
            bytes.len() < 900,
            "routing blob with 2 butler entries is {} bytes — busts the \
             BEP44 budget (spec §3 fallback: separate butler record at a \
             derived pkarr key)",
            bytes.len()
        );
    }

    #[test]
    fn roundtrip_cbor() {
        let p = fixture_payload();
        let bytes = canonical_payload_bytes(&p).expect("encode");
        let decoded: ReachabilityAnnouncePayload =
            ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded, p);
    }

    #[test]
    fn payload_keys_are_2_chars() {
        // Same-length-keys CBOR invariant — see EventPayload doc in community_membership.rs.
        let p = fixture_payload();
        let bytes = canonical_payload_bytes(&p).expect("encode");
        let val: ciborium::Value = ciborium::de::from_reader(&bytes[..]).expect("decode");
        let map = val.as_map().expect("payload is map");
        for (k, _) in map {
            let s = k.as_text().expect("key is text");
            assert_eq!(
                s.chars().count(),
                2,
                "ReachabilityAnnouncePayload key {s:?} violates 2-char invariant"
            );
        }
    }

    #[test]
    fn inner_sig_roundtrip_with_real_identity() {
        // Deterministic seed matches the pattern used by
        // zeb_321_reachability_verify_tests::make_identity — keeps tests
        // reproducible across runs (no rand::thread_rng dependency).
        let identity = PrivateIdentity::from_seed(&[0xAA; 32]);
        let public = identity.public_identity();
        let actor = OwnerAddr(public.address_hash);
        let hlc = fixture_hlc();
        let p = build_signed_payload(
            [0xAB; 32],
            "https://derp.example/".into(),
            vec![],
            1_700_000_000_000,
            &actor,
            &hlc,
            Vec::new(),
            0,
            &identity,
        )
        .expect("sign");

        verify_inner_signature(&p, &actor, &hlc, &public.verifying_key).expect("verify");
    }

    /// ZEB-339: verify `build_signed_payload_with_key` (the enrolled-device
    /// variant) produces a payload whose inner signature verifies under the
    /// matching `VerifyingKey`, and FAILS if either the actor (OwnerAddr) or
    /// the HLC used to compute the signed bytes is mutated.
    #[test]
    fn build_signed_payload_with_key_verifies_and_rejects_mutation() {
        // Fixed, deterministic ed25519 keypair from a known seed.
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32]);
        let verifying_key = signing_key.verifying_key();

        let actor = OwnerAddr([0xAA; 16]);
        let hlc = Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "test-dev".into(),
        };

        let p = build_signed_payload_with_key(
            [0xAB; 32],
            "https://derp.example/".into(),
            vec![],
            1_700_000_000_000,
            &actor,
            &hlc,
            Vec::new(),
            0,
            &signing_key,
        )
        .expect("build_signed_payload_with_key");

        // Correct actor + hlc: signature must verify.
        verify_inner_signature(&p, &actor, &hlc, &verifying_key)
            .expect("inner sig must verify with correct actor + hlc");

        // Mutated actor: signature must FAIL.
        let wrong_actor = OwnerAddr([0xBB; 16]);
        assert_eq!(
            verify_inner_signature(&p, &wrong_actor, &hlc, &verifying_key),
            Err(InnerSigError::Invalid),
            "inner sig must fail with wrong actor"
        );

        // Mutated HLC: signature must FAIL.
        let wrong_hlc = Hlc {
            wall_ms: 1_700_000_000_001, // one ms later
            logical: 0,
            device_id: "test-dev".into(),
        };
        assert_eq!(
            verify_inner_signature(&p, &actor, &wrong_hlc, &verifying_key),
            Err(InnerSigError::Invalid),
            "inner sig must fail with wrong hlc"
        );
    }

    /// ZEB-376: `reachability_freshness_check` bounds a relayed record's
    /// `announced_at_ms` on both edges — fresh within the TTL, stale past it, and
    /// forged-future beyond the skew tolerance is rejected.
    #[test]
    fn reachability_freshness_check_bounds_announced_at() {
        let skew = crate::community_membership::REACHABILITY_TIMESTAMP_SKEW_MAX_MS;
        let mut p = fixture_payload(); // announced_at_ms = 1_700_000_000_000
        let now = p.announced_at_ms;

        // Fresh exactly at the stamp and anywhere up to the TTL edge.
        assert!(reachability_freshness_check(&p, now).is_ok());
        assert!(reachability_freshness_check(&p, now + REACHABILITY_RECORD_TTL_MS).is_ok());
        // Stale one ms past the TTL → rejected.
        assert!(reachability_freshness_check(&p, now + REACHABILITY_RECORD_TTL_MS + 1).is_err());

        // Small forward skew (reader behind the stamp, within tolerance) is fine.
        p.announced_at_ms = now + skew;
        assert!(reachability_freshness_check(&p, now).is_ok());
        // Forged-future: one ms beyond the tolerance → rejected (can't stay
        // fresh-forever until the wall clock catches up).
        p.announced_at_ms = now + skew + 1;
        assert!(reachability_freshness_check(&p, now).is_err());
    }

    #[test]
    fn inner_sig_rejects_tampered_node_id() {
        let identity = PrivateIdentity::from_seed(&[0xAA; 32]);
        let public = identity.public_identity();
        let actor = OwnerAddr(public.address_hash);
        let hlc = fixture_hlc();
        let mut p = build_signed_payload(
            [0xAB; 32],
            "https://derp.example/".into(),
            vec![],
            1_700_000_000_000,
            &actor,
            &hlc,
            Vec::new(),
            0,
            &identity,
        )
        .expect("sign");
        p.iroh_node_id[0] ^= 0xFF;

        assert_eq!(
            verify_inner_signature(&p, &actor, &hlc, &public.verifying_key),
            Err(InnerSigError::Invalid)
        );
    }

    /// The inner identity signature must COVER the butler set + its freshness
    /// stamp, so a co-member or relay can neither forge nor strip the
    /// recipient's seal-targets (ZEB-418 durable seal-targets). A signed
    /// payload built WITH a non-empty butler set verifies; tampering with a
    /// butler entry, or clearing the set/stamp, must both invalidate the sig.
    #[test]
    fn inner_sig_covers_butler_set_and_rejects_tamper() {
        let identity = PrivateIdentity::from_seed(&[0xAA; 32]);
        let public = identity.public_identity();
        let actor = OwnerAddr(public.address_hash);
        let hlc = fixture_hlc();
        let p = build_signed_payload(
            [0xAB; 32],
            "https://derp.example/".into(),
            vec![],
            1_700_000_000_000,
            &actor,
            &hlc,
            vec![fixture_butler_entry(0x10)],
            1_700_000_000_000,
            &identity,
        )
        .expect("sign");

        // Sanity: the butler set + stamp are carried in the payload.
        assert_eq!(p.butler_set, vec![fixture_butler_entry(0x10)]);
        assert_eq!(p.bs_at, 1_700_000_000_000);

        // Honest record verifies.
        verify_inner_signature(&p, &actor, &hlc, &public.verifying_key).expect("verify");

        // (a) Tamper a butler entry → sig must reject.
        let mut tampered = p.clone();
        tampered.butler_set[0].device_ed25519_verify[0] ^= 0xFF;
        assert_eq!(
            verify_inner_signature(&tampered, &actor, &hlc, &public.verifying_key),
            Err(InnerSigError::Invalid)
        );

        // (b) Strip the butler set + zero the stamp → sig must reject.
        let mut stripped = p.clone();
        stripped.butler_set.clear();
        stripped.bs_at = 0;
        assert_eq!(
            verify_inner_signature(&stripped, &actor, &hlc, &public.verifying_key),
            Err(InnerSigError::Invalid)
        );
    }
}
