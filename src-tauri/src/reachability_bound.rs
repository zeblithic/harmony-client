//! ZEB-880: bound a reachability payload so its published pkarr record stays
//! within the frozen core's `SignedPacket` size cap (`MAX_BYTES = 1104`).
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
//! Two levers, applied by callers:
//!   * `direct_addresses` is the only unbounded payload field. [`bound_direct_addresses`]
//!     trims it (least-useful first) so the encoded record fits — keeping the
//!     relay + as many addresses as budget allows. Dialing is relay-assisted
//!     (`endpoint_addr_from_routing`), so trimming degrades gracefully.
//!   * the rendezvous *dial beacon* never reads `butler_set` (a resolver uses
//!     `iroh_node_id` + relay + addrs to dial), so it is pure dead weight there
//!     and is the single largest reclaimable chunk (~290 B for 2 entries).
//!     [`strip_offline_delivery_fields`] drops it from the rendezvous blob.

use crate::reachability_record::ReachabilityAnnouncePayload;
use std::net::{IpAddr, SocketAddr};

/// The frozen core's `pkarr::SignedPacket::MAX_BYTES` (see `harmony-pkarr`
/// `error.rs`: "MAX_BYTES = 1104"). The relay PUT payload must not exceed it.
const PKARR_SIGNED_PACKET_MAX_BYTES: usize = 1104;

/// Conservative reserve for the transform between the record's canonical CBOR
/// and the relay payload the cap applies to: base64url is accounted for by the
/// ×4/3 in [`MAX_RECORD_CBOR_BYTES`]; this covers the DNS TXT framing
/// (header + `_r` name + type/class/ttl/rdlen + one length byte per 255-char
/// chunk) plus the BEP44 envelope (64-byte signature + 8-byte seq). Measured
/// framing is ~110 B; 130 leaves headroom against the cap.
const PKARR_FRAMING_RESERVE_BYTES: usize = 130;

/// Max canonical-CBOR length of a `PkarrRoutingRecord` that still fits the pkarr
/// cap after base64url expansion + framing. Derived, not guessed:
/// `base64url_len(n) = 4·⌈n/3⌉`, so `4·⌈n/3⌉ + FRAMING ≤ MAX_BYTES` ⇒
/// `n ≤ (MAX_BYTES − FRAMING)·3/4`. With the constants above: **730 B**
/// (→ base64 ~974 B → relay ~1084 B < 1104). Field-anchored: AVALON's actual
/// overflowing rendezvous record measured 902 B CBOR / 1204 B base64.
pub const MAX_RECORD_CBOR_BYTES: usize =
    (PKARR_SIGNED_PACKET_MAX_BYTES - PKARR_FRAMING_RESERVE_BYTES) * 3 / 4;

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

/// True for addresses that are useless for cross-WAN first contact — loopback,
/// unspecified, RFC1918/link-local IPv4, or link-local/ULA IPv6. These are
/// dropped before any globally-scoped address, so a trim keeps the reachable
/// legs. (The routability filter already removes loopback/link-local, but this
/// stays defensive and also demotes RFC1918/ULA that the filter keeps for
/// same-LAN peers.)
fn is_locally_scoped(addr: &SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            let seg0 = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (seg0 & 0xffc0) == 0xfe80 // fe80::/10 link-local
                || (seg0 & 0xfe00) == 0xfc00 // fc00::/7 unique-local
        }
    }
}

/// Trim `payload.direct_addresses` until the encoded record — the payload plus
/// `reserved` bytes for its envelope (and any vouch/seal the caller adds) —
/// fits [`MAX_RECORD_CBOR_BYTES`]. Drops the least-useful address each round
/// (locally-scoped first, then largest-encoding to reclaim the most), so global
/// legs and the relay survive. Order of surviving addresses is preserved. A
/// no-op (bytes unchanged) when already within budget. Returns the number of
/// addresses dropped.
pub fn bound_direct_addresses(payload: &mut ReachabilityAnnouncePayload, reserved: usize) -> usize {
    let mut dropped = 0usize;
    while encoded_len(payload) + reserved > MAX_RECORD_CBOR_BYTES {
        // Pick the lowest-priority survivor to drop: maximize (locally_scoped,
        // cbor_len). `position_max`-style scan without an extra crate.
        let Some((victim, _)) = payload
            .direct_addresses
            .iter()
            .enumerate()
            .map(|(i, a)| (i, (is_locally_scoped(a), addr_cbor_len(a))))
            .max_by(|(_, ka), (_, kb)| ka.cmp(kb))
        else {
            break; // no addresses left to trim — payload's fixed fields alone exceed budget
        };
        payload.direct_addresses.remove(victim);
        dropped += 1;
    }
    dropped
}

/// Strip the offline-delivery fields (`butler_set` + its `bs_at` stamp) from a
/// payload destined for a rendezvous *dial beacon*. A joiner resolving a
/// rendezvous slot dials via `iroh_node_id` + relay + `direct_addresses` and
/// never reads `butler_set` (offline-DM seal-targets are a member-record
/// concept), so carrying it there is dead weight — and its ~290 B for two
/// entries is what pushes the rendezvous record over the cap (ZEB-880).
pub fn strip_offline_delivery_fields(payload: &mut ReachabilityAnnouncePayload) {
    payload.butler_set = Vec::new();
    payload.bs_at = 0;
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

    /// The derived budget must leave the worst satisfiable case (friend: sealed
    /// butler payload with zero addresses) room to fit — otherwise the trim can
    /// never converge.
    #[test]
    fn budget_is_satisfiable_for_worst_butler_record() {
        let mut p = avalon_payload();
        p.direct_addresses.clear();
        // friend reserve = envelope + case-d seal.
        assert!(
            encoded_len(&p) + RECORD_ENVELOPE_BYTES + CASE_D_SEAL_BYTES <= MAX_RECORD_CBOR_BYTES,
            "butler-carrying record with zero addresses ({} B) must fit the budget \
             ({} B) with envelope+seal reserve — else the trim can't converge",
            encoded_len(&p),
            MAX_RECORD_CBOR_BYTES,
        );
    }

    /// The reported bug: AVALON's rendezvous record (butler + vouch + 5 addrs)
    /// overflowed. After stripping butler + bounding, the signed record's CBOR
    /// must be within budget — with all global addresses retained (butler, not
    /// addresses, was the driver, so no address trim is needed once it's gone).
    #[test]
    fn rendezvous_record_fits_after_strip_and_bound() {
        let mut p = avalon_payload();
        // Pre-condition: the un-fixed rendezvous record overflows.
        let before = sign_record(encode_rendezvous_blob(&p, Some(&vouch())));
        assert!(
            before > MAX_RECORD_CBOR_BYTES,
            "expected the un-fixed AVALON rendezvous record to overflow, got {before} B"
        );

        strip_offline_delivery_fields(&mut p);
        let dropped =
            bound_direct_addresses(&mut p, RECORD_ENVELOPE_BYTES + RENDEZVOUS_VOUCH_BYTES);
        let after = sign_record(encode_rendezvous_blob(&p, Some(&vouch())));
        assert!(
            after <= MAX_RECORD_CBOR_BYTES,
            "rendezvous record still over budget after fix: {after} B > {MAX_RECORD_CBOR_BYTES} B"
        );
        assert_eq!(
            dropped, 0,
            "stripping butler alone should fit all 5 addresses; dropped {dropped}"
        );
        assert_eq!(p.direct_addresses.len(), 5, "all addresses retained");
    }

    /// The case-B identity record (keeps butler_set for offline DM) also
    /// overflows on the same host; bounding addresses must bring it under budget
    /// while keeping the butler set.
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
        let dropped = bound_direct_addresses(&mut p, RECORD_ENVELOPE_BYTES + CASE_D_SEAL_BYTES);
        assert!(dropped > 0, "expected some addresses trimmed");
        assert!(
            !p.butler_set.is_empty(),
            "butler set must be preserved (offline-DM seal-targets) — trim addresses, not butler"
        );
        let mut bare = Vec::new();
        ciborium::into_writer(&p, &mut bare).unwrap();
        let after = sign_record(bare);
        assert!(
            after <= MAX_RECORD_CBOR_BYTES,
            "identity record still over budget after bounding: {after} B"
        );
    }

    /// Drop-priority keeps globally-scoped addresses: the RFC1918 `192.168.*`
    /// leg is dropped before any public/global one.
    #[test]
    fn trim_drops_locally_scoped_before_global() {
        let mut p = avalon_payload();
        // Force exactly one drop by reserving almost the whole budget.
        let target = encoded_len(&p) - 1;
        let reserved = MAX_RECORD_CBOR_BYTES.saturating_sub(target);
        let dropped = bound_direct_addresses(&mut p, reserved);
        assert_eq!(dropped, 1, "reserve chosen to force exactly one drop");
        assert!(
            !p.direct_addresses
                .iter()
                .any(|a| a.ip().to_string() == "192.168.1.59"),
            "the RFC1918 address should be the first dropped"
        );
        assert!(
            p.direct_addresses
                .iter()
                .any(|a| a.ip().to_string() == "165.162.82.51"),
            "the public IPv4 must survive"
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
        let dropped = bound_direct_addresses(&mut p, RECORD_ENVELOPE_BYTES + CASE_D_SEAL_BYTES);
        assert_eq!(dropped, 0);
        assert_eq!(p, before, "under-budget payload must be untouched");
    }
}
