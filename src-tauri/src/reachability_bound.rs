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
//!   * `direct_addresses` is the only unbounded payload field.
//!     [`bound_direct_addresses`] trims it (largest-encoding first) so the
//!     encoded record fits — keeping the relay + as many addresses as budget
//!     allows. Dialing is relay-assisted (`endpoint_addr_from_routing`), so
//!     trimming degrades gracefully.
//!   * the rendezvous blob uniquely adds a `MembershipVouch`, so a full 2-entry
//!     `butler_set` (~290 B) cannot fit at any address count. [`cap_butler_set`]
//!     caps it (rather than stripping) so a rendezvous-seeded peer keeps a
//!     seal target for the offline-DM deposit path, while the record still fits.

use crate::reachability_record::ReachabilityAnnouncePayload;
use std::net::SocketAddr;

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
/// cap after base64url expansion + framing. The wire codec is `URL_SAFE_NO_PAD`
/// (unpadded, `len = ⌈4n/3⌉`), but we derive the budget under the *padded*
/// formula `4·⌈n/3⌉` — a strict upper bound on the unpadded length — so the
/// budget is safe regardless of padding. The largest `n` with
/// `4·⌈n/3⌉ ≤ (MAX_BYTES − FRAMING)` is `((MAX_BYTES − FRAMING) / 4) · 3`
/// (integer floor of the base64-block count, times 3). With the constants
/// above: **729 B** (→ base64 ≤ 972 B → relay ≤ ~1082 B < 1104). Field-anchored:
/// AVALON's actual overflowing rendezvous record measured 902 B CBOR / 1204 B
/// base64.
///
/// Computing `((M−F)/4)·3` rather than `(M−F)·3/4` matters: the latter's
/// multiply-then-floor can round *up* past the true inverse and let a record one
/// byte over the cap slip through (Qodo/CodeRabbit convergence finding).
pub const MAX_RECORD_CBOR_BYTES: usize =
    ((PKARR_SIGNED_PACKET_MAX_BYTES - PKARR_FRAMING_RESERVE_BYTES) / 4) * 3;

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
/// within budget. Returns the number of addresses dropped.
pub fn bound_direct_addresses(payload: &mut ReachabilityAnnouncePayload, reserved: usize) -> usize {
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
            break; // no addresses left to trim — payload's fixed fields alone exceed budget
        };
        payload.direct_addresses.remove(victim);
        dropped += 1;
    }
    dropped
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
            bound_direct_addresses(&mut p, RECORD_ENVELOPE_BYTES + RENDEZVOUS_VOUCH_BYTES);
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

    /// Largest-first drop keeps the compact IPv4 legs (both public and the
    /// same-LAN RFC1918 one) and sheds the larger IPv6 octets first — so a trim
    /// never strands a peer's only local route (CodeAnt convergence finding).
    #[test]
    fn trim_drops_largest_ipv6_before_compact_ipv4() {
        let mut p = avalon_payload();
        // Force exactly one drop by reserving almost the whole budget.
        let target = encoded_len(&p) - 1;
        let reserved = MAX_RECORD_CBOR_BYTES.saturating_sub(target);
        let dropped = bound_direct_addresses(&mut p, reserved);
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
        let dropped = bound_direct_addresses(&mut p, RECORD_ENVELOPE_BYTES + CASE_D_SEAL_BYTES);
        assert_eq!(dropped, 0);
        assert_eq!(p, before, "under-budget payload must be untouched");
    }
}
