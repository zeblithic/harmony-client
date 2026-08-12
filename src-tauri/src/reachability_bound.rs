//! ZEB-880: bound a reachability payload so its published pkarr record stays
//! within pkarr's build-time DNS-packet size gate.
//!
//! The reachability payload is built once by a shared `blob_builder`
//! (`lib.rs`) and published under five record types (identity/case-B,
//! community, rendezvous, friend/case-D, invite). Each wraps the payload in a
//! `PkarrRoutingRecord`, which is base64url-encoded (~×4/3) and DNS-framed
//! before the cap check in `harmony_pkarr::wire::build_relay_payload`. On a
//! dual-stack host advertising several direct addresses — especially with a
//! populated 2-entry `butler_set` (offline-DM seal-targets, ZEB-418) — the
//! record overflows and `RecordTooLarge` fires every publish cycle, so the
//! record NEVER publishes (community/identity silently undiscoverable).
//!
//! Round 2: the round-1 budget (729 B) was derived from
//! `SignedPacket::MAX_BYTES = 1104` — but that outer bound includes a 104-byte
//! crypto envelope and is only checked when *deserializing* a packet. The gate
//! that actually fails a publish is pkarr's builder: the encoded DNS packet
//! must be ≤ 1000 bytes, and inside it the TXT record's name is
//! origin-normalized to `_r.<52-char z32 pubkey>.` (57 bytes of name, not a
//! bare `_r`). The true ceiling is [`MAX_RECORD_CBOR_BYTES`] = 687 B — records
//! landing in 688..=729 passed the round-1 bound yet failed the real build,
//! which is exactly how AVALON kept hitting `RecordTooLarge` with the round-1
//! fix deployed. The `budget_matches_pkarr_build_gate_exactly` test pins the
//! constant to the real builder so it can never silently drift again.
//!
//! Levers, applied via [`bound_record_payload`]:
//!   * `direct_addresses` is the only unbounded payload field.
//!     [`bound_direct_addresses`] trims it (largest-encoding first) so the
//!     encoded record fits — keeping the relay + as many addresses as budget
//!     allows. Dialing is relay-assisted (`endpoint_addr_from_routing`), so
//!     trimming degrades gracefully.
//!   * a full 2-entry `butler_set` (~290 B) may not fit at any address count —
//!     always true for the vouch-carrying rendezvous blob, and possible for
//!     identity/friend records whose butler entries carry long relay URLs.
//!     [`cap_butler_set`] caps it to one entry (rather than stripping) so a
//!     discovered peer keeps a seal target for the offline-DM deposit path,
//!     while the record still fits. [`bound_record_payload`] applies the cap
//!     only when the fixed fields alone could never fit — a butler pair that
//!     fits is kept.

use crate::reachability_record::ReachabilityAnnouncePayload;
use std::net::SocketAddr;

/// pkarr's build-time gate (`pkarr` `signed_packet.rs`): the **encoded DNS
/// packet** — not the full relay payload — must be ≤ 1000 bytes (the Mainline
/// DHT mutable-value limit). `SignedPacket::MAX_BYTES = 1104` is this plus the
/// 104-byte crypto envelope (pubkey 32 + signature 64 + timestamp 8) and is
/// only enforced at deserialization; budgeting against it (round 1) admitted
/// records the builder then rejected.
const PKARR_MAX_ENCODED_DNS_PACKET_BYTES: usize = 1000;

/// Deterministic DNS framing around the base64 payload inside that packet, for
/// the single TXT record `harmony_pkarr::wire::build_relay_payload` emits:
/// header (12) + the origin-normalized name `_r.<z32 pubkey>.` (1+2 for `_r`,
/// 1+52 for the z-base-32 ed25519 pubkey label, 1 root = 57) + type/class/TTL/
/// RDLENGTH (10) + one character-string length byte per 255-byte base64 chunk
/// (4 at this budget) = 83. Every term is fixed — pkarr normalizes record
/// names to the signing key's origin, so the 53-byte key label is always
/// present (missing it is what round 1's "measured ~110 B, reserve 130"
/// estimate got wrong).
const DNS_TXT_FRAMING_BYTES: usize = 83;

/// Max canonical-CBOR length of a `PkarrRoutingRecord` that still fits pkarr's
/// 1000-byte encoded-DNS-packet gate after base64url expansion + framing. The
/// wire codec is `URL_SAFE_NO_PAD` (unpadded, `len = ⌈4n/3⌉`), but we derive
/// the budget under the *padded* formula `4·⌈n/3⌉` — a strict upper bound on
/// the unpadded length — so the budget is safe regardless of padding. The
/// largest `n` with `4·⌈n/3⌉ ≤ (PACKET − FRAMING)` is
/// `((PACKET − FRAMING) / 4) · 3` (integer floor of the base64-block count,
/// times 3). With the constants above: **687 B** (→ base64 ≤ 916 B → encoded
/// packet ≤ 999 B ≤ 1000). This is exact, not merely safe: a 688-byte record
/// base64-encodes to 918 B → a 1001-byte packet → `PacketTooLarge` (pinned by
/// `budget_matches_pkarr_build_gate_exactly`).
///
/// Computing `((P−F)/4)·3` rather than `(P−F)·3/4` matters: the latter's
/// multiply-then-floor can round *up* past the true inverse and let a record one
/// byte over the cap slip through (Qodo/CodeRabbit convergence finding).
pub const MAX_RECORD_CBOR_BYTES: usize =
    ((PKARR_MAX_ENCODED_DNS_PACKET_BYTES - DNS_TXT_FRAMING_BYTES) / 4) * 3;

/// CBOR the `PkarrRoutingRecord` envelope adds around the routing blob:
/// `harmony_identity_pub` (64) + two `u64` stamps + `inner_sig` (64) + CBOR map
/// framing. Measured ~169 B; rounded up. Callers pass this (plus any per-record
/// overhead below) as `reserved` so the trim targets the *record*, not the bare
/// payload.
pub const RECORD_ENVELOPE_BYTES: usize = 176;

/// Extra bytes the friend/case-D path adds by sealing the blob
/// (`seal_case_d_payload`: 12-byte nonce + 16-byte Poly1305 tag). Reserved at
/// the shared builder so the largest bare-blob consumer (friend) also fits.
pub const CASE_D_SEAL_BYTES: usize = 32;

/// Extra CBOR the rendezvous blob adds by merging a `MembershipVouch` under the
/// `"mv"` key (`encode_rendezvous_blob`). Measured ~138 B; rounded up.
pub const RENDEZVOUS_VOUCH_BYTES: usize = 144;

/// Canonical-CBOR length of a reachability payload.
fn encoded_len(payload: &ReachabilityAnnouncePayload) -> usize {
    let mut buf = Vec::new();
    // Infallible for this fixed, serializable struct against a Vec writer.
    let _ = ciborium::into_writer(payload, &mut buf);
    buf.len()
}

/// CBOR length of a single `SocketAddr` as `direct_addresses` encodes it — used
/// only to break drop-priority ties toward reclaiming the most bytes.
fn addr_cbor_len(addr: &SocketAddr) -> usize {
    let mut buf = Vec::new();
    let _ = ciborium::into_writer(addr, &mut buf);
    buf.len()
}

/// Trim `payload.direct_addresses` until the encoded record — the payload plus
/// `reserved` bytes for its envelope (and any vouch/seal the caller adds) —
/// fits [`MAX_RECORD_CBOR_BYTES`]. Drops the **largest-encoding** address each
/// round (reclaims the most bytes per drop, and keeps the greatest *number* of
/// candidates). This deliberately does NOT rank by scope: IPv6 legs (16-byte
/// octets) are the largest, so a trim tends to keep the compact IPv4 legs —
/// which naturally retains both a public and a same-LAN (RFC1918) IPv4 route
/// when present, rather than unconditionally demoting local addresses that may
/// be a peer's only usable path (CodeAnt convergence finding). The relay
/// (`home_relay_url`, always kept) plus the surviving legs keep the node
/// dialable; dialing is relay-assisted, so trimming degrades gracefully. Order
/// of surviving addresses is preserved; a no-op (bytes unchanged) when already
/// within budget. Returns an [`AddressBound`] — the number of addresses dropped
/// AND whether the record is still over budget afterward.
pub fn bound_direct_addresses(
    payload: &mut ReachabilityAnnouncePayload,
    reserved: usize,
) -> AddressBound {
    let mut dropped = 0usize;
    while encoded_len(payload) + reserved > MAX_RECORD_CBOR_BYTES {
        // Drop the largest-encoding survivor. `position_max`-style scan (first
        // index wins ties, so drops are deterministic).
        let Some((victim, _)) = payload
            .direct_addresses
            .iter()
            .enumerate()
            .map(|(i, a)| (i, addr_cbor_len(a)))
            .max_by(|(_, ka), (_, kb)| ka.cmp(kb))
        else {
            // No addresses left to trim and the loop condition is still true:
            // the payload's FIXED fields (relay URL(s) + butler set, and any
            // vouch/seal in `reserved`) alone exceed the cap, so no address trim
            // can make the record fit. Signal it (ZEB-891) — the caller must
            // `warn!`, because publishing this over-cap record fails with
            // `RecordTooLarge` every cycle, leaving the node silently
            // undiscoverable with only a `debug!` that reads like success.
            return AddressBound {
                dropped,
                over_budget: true,
            };
        };
        payload.direct_addresses.remove(victim);
        dropped += 1;
    }
    AddressBound {
        dropped,
        over_budget: false,
    }
}

/// Outcome of [`bound_direct_addresses`]: how many direct addresses were dropped
/// to fit the cap, and whether the record is STILL over budget after trimming.
///
/// `over_budget == true` means the fixed fields alone exceed
/// [`MAX_RECORD_CBOR_BYTES`] — trimming drained every address and still can't
/// fit. It is the ZEB-891 signal callers MUST surface at `warn!` (an over-cap
/// record never publishes → the node is silently undiscoverable); no address
/// trim can recover it, so the operator must shorten the configured relay URL(s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressBound {
    /// Number of `direct_addresses` removed to reach (or attempt) the budget.
    pub dropped: usize,
    /// `true` iff the record still exceeds the cap after trimming — the fixed
    /// fields alone are too large for any address count to fit.
    pub over_budget: bool,
}

/// Outcome of [`bound_record_payload`]: the address-trim outcome plus whether
/// the size-aware butler fallback fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordBound {
    /// Number of `direct_addresses` removed to reach (or attempt) the budget.
    pub dropped: usize,
    /// `true` iff the butler set was capped to one entry because the fixed
    /// fields with the full set could never fit at any address count.
    pub butler_capped: bool,
    /// `true` iff the record still exceeds the cap after BOTH levers — the
    /// ZEB-891 signal callers MUST surface at `warn!` (an over-cap record
    /// never publishes → the node is silently undiscoverable); the operator
    /// must shorten the configured relay URL(s).
    pub over_budget: bool,
}

/// Bound `payload` so the encoded record — the payload plus `reserved` bytes
/// for its envelope (and any vouch/seal the caller adds) — fits
/// [`MAX_RECORD_CBOR_BYTES`], spending expendable fields in graceful-
/// degradation order:
///
/// 1. **Butler fallback (round 2):** if the FIXED fields alone (zero
///    addresses) could never fit and the butler set holds more than one
///    entry, cap it to one seal target up front — so the address trim that
///    follows can still keep real dial candidates instead of draining every
///    address before discovering the record can never fit. The production
///    iroh-relay butler pair fits the corrected budget (~452 B fixed + 208 B
///    reserve ≤ 687 B), so this is a DEFENSIVE lever: it fires when longer
///    butler relay URLs (e.g. self-hosted community relays) push the fixed
///    fields over, converting a permanent ZEB-891 publish outage into a
///    record with one seal target (the offline-DM deposit path stays alive,
///    mirroring the rendezvous blob's long-standing cap). A butler pair that
///    fits is left intact.
/// 2. **Address trim:** [`bound_direct_addresses`], largest-encoding first.
///
/// `over_budget` is `true` iff the record still cannot fit after both levers
/// (fixed fields alone exceed the budget even with a single-entry butler set —
/// e.g. a pathologically long relay URL).
pub fn bound_record_payload(
    payload: &mut ReachabilityAnnouncePayload,
    reserved: usize,
) -> RecordBound {
    let butler_capped = if payload.butler_set.len() > 1 {
        let mut probe = payload.clone();
        probe.direct_addresses.clear();
        if encoded_len(&probe) + reserved > MAX_RECORD_CBOR_BYTES {
            cap_butler_set(payload, 1);
            true
        } else {
            false
        }
    } else {
        false
    };
    let bound = bound_direct_addresses(payload, reserved);
    RecordBound {
        dropped: bound.dropped,
        butler_capped,
        over_budget: bound.over_budget,
    }
}

/// Cap `payload.butler_set` to at most `max_entries` for a rendezvous *dial
/// beacon*. The rendezvous blob uniquely also carries a `MembershipVouch`, so a
/// full 2-entry butler set (~290 B) cannot fit the record under the cap at any
/// address count. Rather than strip the set entirely — which would strand the
/// offline-DM seal targets the gateway dial driver seeds into the
/// `ReachabilityResolver` (`seed_from_pkarr` → `PkarrLive`, read by
/// `butler_deposit::freshest_butler_set_by_source`; CodeAnt convergence finding)
/// — keep the first `max_entries` (highest-priority: pinned/self first, per
/// `fleet_net::build_butler_set`) so a rendezvous-discovered peer still has a
/// seal target until durable/member reachability arrives. `bs_at` is cleared
/// only when the set is emptied, so a retained entry stays fresh-gated. No-op
/// if already at/under `max_entries`.
pub fn cap_butler_set(payload: &mut ReachabilityAnnouncePayload, max_entries: usize) {
    if payload.butler_set.len() > max_entries {
        payload.butler_set.truncate(max_entries);
    }
    if payload.butler_set.is_empty() {
        payload.bs_at = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_rendezvous::encode_rendezvous_blob;
    use crate::membership_vouch::mint_membership_vouch;
    use crate::owner_state_types::SpaceId;
    use crate::reachability_record::{ButlerSetEntry, ReachabilityAnnouncePayload};
    use ed25519_dalek::SigningKey;

    fn butler(s: u8) -> ButlerSetEntry {
        ButlerSetEntry {
            device_id: [s; 16],
            iroh_endpoint_id: [s.wrapping_add(1); 32],
            device_ed25519_verify: [s.wrapping_add(2); 32],
            home_relay: "https://usw1-1.relay.n0.iroh.link/".to_string(),
            pinned: false,
        }
    }

    /// AVALON's exact advertised reachability: 2 IPv4 (1 public, 1 RFC1918) +
    /// 3 global IPv6, a real relay URL, and a full 2-entry butler set.
    fn avalon_payload() -> ReachabilityAnnouncePayload {
        ReachabilityAnnouncePayload {
            iroh_node_id: [0x03; 32],
            home_relay_url: "https://usw1-1.relay.n0.iroh.link/".to_string(),
            direct_addresses: vec![
                "165.162.82.51:35102".parse().unwrap(),
                "192.168.1.59:63933".parse().unwrap(),
                "[2603:8002:ddf0:3380::1787]:63934".parse().unwrap(),
                "[2603:8002:ddf0:3380:6b34:be5b:30f8:5f6e]:63934"
                    .parse()
                    .unwrap(),
                "[2603:8002:ddf0:3380:bc5e:7e59:7bfb:19a6]:63934"
                    .parse()
                    .unwrap(),
            ],
            announced_at_ms: 1_700_000_000_000,
            identity_signature: [0u8; 64],
            butler_set: vec![butler(0x10), butler(0x20)],
            bs_at: 1_700_000_000_000,
        }
    }

    fn sign_record(blob: Vec<u8>) -> usize {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut id_pub = [0u8; 64];
        id_pub[32..].copy_from_slice(&sk.verifying_key().to_bytes());
        harmony_pkarr::PkarrRoutingRecord::sign_new(
            blob,
            id_pub,
            1_700_000_000_000,
            1_700_600_000_000,
            &sk,
        )
        .unwrap()
        .to_canonical_cbor()
        .unwrap()
        .len()
    }

    fn vouch() -> crate::membership_vouch::MembershipVouch {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut id_pub = [0u8; 64];
        id_pub[32..].copy_from_slice(&sk.verifying_key().to_bytes());
        mint_membership_vouch(
            &SigningKey::from_bytes(&[3; 32]),
            SpaceId([1; 16]),
            &id_pub,
            1_700_000_000_000,
            1_700_600_000_000,
        )
    }

    /// ZEB-880 round 2: pin `MAX_RECORD_CBOR_BYTES` to pkarr's REAL build-time
    /// gate by replicating `harmony_pkarr::wire::build_relay_payload`'s packet
    /// assembly byte-for-byte (base64url'd record in one `_r` TXT, 255-byte
    /// character-strings, TTL 300) against the actual `pkarr` builder — which
    /// origin-normalizes the name to `_r.<z32 pubkey>.` and enforces the
    /// 1000-byte encoded-DNS-packet limit. A record of exactly the budget must
    /// build; one byte more must fail with `PacketTooLarge`. Round 1 derived
    /// the budget from `SignedPacket::MAX_BYTES = 1104` (the outer
    /// deserialization bound), admitting 688..=729-byte records the builder
    /// rejects — the reopened AVALON `RecordTooLarge`. This pin makes any
    /// future drift between the constant and pkarr's gate a test failure, not
    /// a silent production publish outage.
    #[test]
    fn budget_matches_pkarr_build_gate_exactly() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        use pkarr::dns::rdata::TXT;
        use pkarr::dns::{CharacterString, Name};
        use pkarr::errors::SignedPacketBuildError;
        use pkarr::{Keypair, SignedPacket};

        let build = |record_cbor_len: usize| {
            let keypair = Keypair::from_secret_key(&[7u8; 32]);
            let b64 = URL_SAFE_NO_PAD.encode(vec![0xA5u8; record_cbor_len]);
            let mut txt = TXT::new();
            for chunk in b64.as_bytes().chunks(255) {
                txt.add_char_string(CharacterString::new(chunk).expect("char-string"));
            }
            SignedPacket::builder()
                .txt(Name::new("_r").expect("name"), txt, 300)
                .sign(&keypair)
        };
        assert!(
            build(MAX_RECORD_CBOR_BYTES).is_ok(),
            "a record of exactly the budget ({MAX_RECORD_CBOR_BYTES} B) must pass \
             pkarr's packet build"
        );
        assert!(
            matches!(
                build(MAX_RECORD_CBOR_BYTES + 1),
                Err(SignedPacketBuildError::PacketTooLarge(_))
            ),
            "one byte over the budget must fail the packet build — if this fires, \
             the budget is UNDER the true gate (safe but wasteful); if the assert \
             above fires, records over the gate are being admitted (the round-1 bug)"
        );
    }

    /// When butler entries carry LONG relay URLs (e.g. a self-hosted community
    /// relay rather than the ~34-char production iroh relays), the fixed
    /// fields with a full butler pair can exceed the budget at any address
    /// count. The size-aware butler fallback must cap the set to one seal
    /// target so the record converges instead of going ZEB-891 over-budget
    /// (a permanent publish outage).
    #[test]
    fn long_relay_butler_record_converges_via_butler_fallback() {
        let long_relay = "https://relay.community-infra.zeblithic-example.org/".to_string();
        let mut p = avalon_payload();
        p.home_relay_url = long_relay.clone();
        for b in &mut p.butler_set {
            b.home_relay = long_relay.clone();
        }
        p.direct_addresses.clear();
        // Premise: with these URLs the full butler pair no longer fits even
        // with zero addresses — the scenario the fallback exists for.
        assert!(
            encoded_len(&p) + RECORD_ENVELOPE_BYTES + CASE_D_SEAL_BYTES > MAX_RECORD_CBOR_BYTES,
            "premise: butler(2) fixed fields ({} B) must exceed the budget ({} B) \
             — if they now fit, shorten the budget's margin story or this fixture",
            encoded_len(&p),
            MAX_RECORD_CBOR_BYTES,
        );
        let bound = bound_record_payload(&mut p, RECORD_ENVELOPE_BYTES + CASE_D_SEAL_BYTES);
        assert!(bound.butler_capped, "fallback must fire");
        assert!(!bound.over_budget, "capped record must converge");
        assert_eq!(p.butler_set.len(), 1, "one seal target retained");
        assert!(
            encoded_len(&p) + RECORD_ENVELOPE_BYTES + CASE_D_SEAL_BYTES <= MAX_RECORD_CBOR_BYTES,
            "record must genuinely fit after the fallback"
        );
    }

    /// A butler pair that FITS is kept — the fallback caps only when the fixed
    /// fields could never fit, not as a blanket policy (offline-DM seal-target
    /// redundancy is worth keeping when the budget allows it).
    #[test]
    fn butler_pair_kept_when_it_fits() {
        let short_relay = "https://r.example/".to_string();
        let mut p = avalon_payload();
        p.home_relay_url = short_relay.clone();
        for b in &mut p.butler_set {
            b.home_relay = short_relay.clone();
        }
        p.direct_addresses.truncate(2);
        let bound = bound_record_payload(&mut p, RECORD_ENVELOPE_BYTES + CASE_D_SEAL_BYTES);
        assert!(
            !bound.butler_capped,
            "a fitting butler pair must not be capped"
        );
        assert!(!bound.over_budget);
        assert_eq!(p.butler_set.len(), 2, "both seal targets kept");
    }

    /// The reported bug: AVALON's rendezvous record (butler + vouch + 5 addrs)
    /// overflowed. Capping butler to 1 (keeping a seal target for the seed path)
    /// then bounding addresses must bring the signed record within budget, and
    /// keep at least one butler entry (offline-DM seed) and at least one direct
    /// address (dial candidate).
    #[test]
    fn rendezvous_record_fits_after_cap_and_bound() {
        let mut p = avalon_payload();
        // Pre-condition: the un-fixed rendezvous record overflows, and does so
        // even with zero addresses — proving butler(2)+vouch cannot fit, so the
        // cap (not just an address trim) is required.
        let before = sign_record(encode_rendezvous_blob(&p, Some(&vouch())));
        assert!(
            before > MAX_RECORD_CBOR_BYTES,
            "expected the un-fixed AVALON rendezvous record to overflow, got {before} B"
        );
        let mut no_addrs = p.clone();
        no_addrs.direct_addresses.clear();
        let butler2_floor = sign_record(encode_rendezvous_blob(&no_addrs, Some(&vouch())));
        assert!(
            butler2_floor > MAX_RECORD_CBOR_BYTES,
            "butler(2)+vouch must overflow even with 0 addresses ({butler2_floor} B) — \
             address-bounding alone cannot fit it, so the butler cap is required"
        );

        cap_butler_set(&mut p, 1);
        let dropped =
            bound_record_payload(&mut p, RECORD_ENVELOPE_BYTES + RENDEZVOUS_VOUCH_BYTES).dropped;
        let after = sign_record(encode_rendezvous_blob(&p, Some(&vouch())));
        assert!(
            after <= MAX_RECORD_CBOR_BYTES,
            "rendezvous record still over budget after fix: {after} B > {MAX_RECORD_CBOR_BYTES} B"
        );
        assert_eq!(
            p.butler_set.len(),
            1,
            "one seal target retained for the seed path"
        );
        assert!(
            !p.direct_addresses.is_empty(),
            "at least one dial address retained (dropped {dropped} of 5)"
        );
    }

    /// The case-B identity record (keeps butler_set for offline DM) also
    /// overflows on the same host; bounding must bring it under the corrected
    /// budget by trimming addresses alone — the production-relay butler pair
    /// (~452 B of fixed fields + 208 B reserve = ~660 B) still fits under
    /// 687 B, so the butler fallback must NOT fire here (seal-target
    /// redundancy is preserved; only long-relay-URL butler sets trigger it).
    #[test]
    fn identity_record_fits_after_bounding_addresses() {
        let mut p = avalon_payload();
        let mut bare = Vec::new();
        ciborium::into_writer(&p, &mut bare).unwrap();
        let before = sign_record(bare);
        assert!(
            before > MAX_RECORD_CBOR_BYTES,
            "expected the un-fixed AVALON identity record to overflow, got {before} B"
        );

        // shared-builder reserve = envelope + case-d seal (covers friend too).
        let bound = bound_record_payload(&mut p, RECORD_ENVELOPE_BYTES + CASE_D_SEAL_BYTES);
        assert!(bound.dropped > 0, "expected some addresses trimmed");
        assert!(
            !bound.butler_capped,
            "the production-relay butler pair fits the corrected budget — \
             the fallback must not fire for it"
        );
        assert!(
            !bound.over_budget,
            "trimming brought it under budget, so over_budget must be false"
        );
        assert_eq!(
            p.butler_set.len(),
            2,
            "butler set must be preserved (offline-DM seal-targets) — trim addresses, not butler"
        );
        assert!(
            !p.direct_addresses.is_empty(),
            "at least one dial address must survive"
        );
        let mut bare = Vec::new();
        ciborium::into_writer(&p, &mut bare).unwrap();
        let after = sign_record(bare);
        assert!(
            after <= MAX_RECORD_CBOR_BYTES,
            "identity record still over budget after bounding: {after} B"
        );
    }

    /// Largest-first drop keeps the compact IPv4 legs (both public and the
    /// same-LAN RFC1918 one) and sheds the larger IPv6 octets first — so a trim
    /// never strands a peer's only local route (CodeAnt convergence finding).
    #[test]
    fn trim_drops_largest_ipv6_before_compact_ipv4() {
        let mut p = avalon_payload();
        // Force exactly one drop by reserving almost the whole budget.
        let target = encoded_len(&p) - 1;
        let reserved = MAX_RECORD_CBOR_BYTES.saturating_sub(target);
        let dropped = bound_direct_addresses(&mut p, reserved).dropped;
        assert_eq!(dropped, 1, "reserve chosen to force exactly one drop");
        // Both IPv4 legs (public + RFC1918) survive; an IPv6 leg is the one dropped.
        assert!(
            p.direct_addresses
                .iter()
                .any(|a| a.ip().to_string() == "192.168.1.59"),
            "the same-LAN RFC1918 route must survive (not dropped first)"
        );
        assert!(
            p.direct_addresses
                .iter()
                .any(|a| a.ip().to_string() == "165.162.82.51"),
            "the public IPv4 must survive"
        );
        assert_eq!(
            p.direct_addresses.iter().filter(|a| a.is_ipv6()).count(),
            2,
            "exactly one of the three IPv6 legs is dropped"
        );
    }

    /// A modest payload (few addresses, no butler set) is left byte-for-byte
    /// unchanged — the common solo-node case pays nothing.
    #[test]
    fn small_payload_is_untouched() {
        let mut p = ReachabilityAnnouncePayload {
            iroh_node_id: [0xAB; 32],
            home_relay_url: "https://derp.example/".to_string(),
            direct_addresses: vec![
                "203.0.113.7:62103".parse().unwrap(),
                "[2001:db8::1]:62103".parse().unwrap(),
            ],
            announced_at_ms: 1_700_000_000_000,
            identity_signature: [0u8; 64],
            butler_set: vec![],
            bs_at: 0,
        };
        let before = p.clone();
        let bound = bound_record_payload(&mut p, RECORD_ENVELOPE_BYTES + CASE_D_SEAL_BYTES);
        assert_eq!(bound.dropped, 0);
        assert!(!bound.butler_capped);
        assert!(
            !bound.over_budget,
            "an already-fitting payload is not over budget"
        );
        assert_eq!(p, before, "under-budget payload must be untouched");
    }

    /// ZEB-891: when the FIXED fields alone (a long relay URL) exceed the cap
    /// even after the butler fallback, trimming drains every address and STILL
    /// can't fit — the function must report `over_budget = true` so the caller
    /// can `warn!` instead of silently publishing an over-cap record
    /// (RecordTooLarge every cycle = silently undiscoverable). Before ZEB-891
    /// this returned normally with no over-budget signal.
    #[test]
    fn over_budget_signaled_when_fixed_fields_exceed_cap() {
        let mut p = avalon_payload();
        // A pathologically long self-hosted relay URL pushes the fixed fields
        // over the cap on their own — the ZEB-891 operator-misconfig scenario
        // (a >~66-char relay URL with a full butler set).
        p.home_relay_url = format!("https://{}.internal.zeblithic.dev/", "r".repeat(200));
        let bound = bound_record_payload(&mut p, RECORD_ENVELOPE_BYTES + CASE_D_SEAL_BYTES);
        assert!(
            bound.butler_capped,
            "the fallback fires first (fixed fields over budget with butler > 1)"
        );
        assert!(
            p.direct_addresses.is_empty(),
            "all addresses must be drained trying to fit"
        );
        assert!(
            bound.over_budget,
            "must signal over_budget when the fixed fields alone exceed the cap"
        );
        // The signal is truthful: the payload really is still over budget.
        assert!(
            encoded_len(&p) + RECORD_ENVELOPE_BYTES + CASE_D_SEAL_BYTES > MAX_RECORD_CBOR_BYTES,
            "sanity: payload genuinely still over budget after draining addresses"
        );
    }
}
