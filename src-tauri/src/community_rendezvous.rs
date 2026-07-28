//! Open-community cross-WAN first-contact: enumerated DHT rendezvous slots.
//!
//! A community-keyed rendezvous record lets a link-only joiner (holding the
//! community `epoch_key`) resolve a live serving "beacon" from the DHT. Slot
//! keys reuse `PkarrCase::Community` (its `ikm` contract IS the epoch key) with
//! a distinct, length-disjoint info-prefix so they can never collide with the
//! member-keyed records (`info = identity_pub(64) || epoch_id(8)`, 72 bytes).
//!
//! The escalating-batch resolve *driver* now lives in core
//! (`harmony_pkarr::rendezvous`); this module supplies the community-specific
//! parts: the slot info-layout, the `ReachabilityAnnouncePayload` decoder, the
//! env-knob config builder, and the advertiser-set slot assignment.

use crate::community_relay_announce::COMMUNITY_RELAY_ADVERTISERS_MAX;
use crate::owner_state_types::EpochKey;
use crate::owner_state_types::OwnerAddr;
use crate::reachability_record::ReachabilityAnnouncePayload;
use harmony_pkarr::derive::{derive_ephemeral_key, PkarrCase};
use harmony_pkarr::rendezvous::{
    resolve_rendezvous_with, slot_for_advertiser as core_slot_for_advertiser, PkarrSlotResolver,
    RendezvousResolveConfig, RendezvousResolveOutcome,
};
use harmony_pkarr::PkarrResolver;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use zeroize::Zeroizing;

/// Number of enumerated rendezvous slots == the relay-advertiser cap.
pub const RENDEZVOUS_SLOT_COUNT: usize = COMMUNITY_RELAY_ADVERTISERS_MAX;

/// Domain-separation prefix for rendezvous slot derivation. Length-disjoint
/// from the 72-byte member-keyed `info`, so rendezvous keys can never alias a
/// member's reachability key even though both reuse the `Community` salt.
pub const RENDEZVOUS_INFO_PREFIX: &[u8] = b"harmony.rendezvous.v1";

/// `info = RENDEZVOUS_INFO_PREFIX || slot_index_be(2) || epoch_id_be(8)`.
fn rendezvous_info(slot_index: u16, epoch_id: u64) -> Vec<u8> {
    let mut info = Vec::with_capacity(RENDEZVOUS_INFO_PREFIX.len() + 2 + 8);
    info.extend_from_slice(RENDEZVOUS_INFO_PREFIX);
    info.extend_from_slice(&slot_index.to_be_bytes());
    info.extend_from_slice(&epoch_id.to_be_bytes());
    info
}

pub fn rendezvous_slot_key(
    epoch_key: &EpochKey,
    slot_index: u16,
    epoch_id: u64,
) -> ed25519_dalek::SigningKey {
    let info = rendezvous_info(slot_index, epoch_id);
    derive_ephemeral_key(PkarrCase::Community, epoch_key.as_bytes(), &info)
}

pub fn rendezvous_slot_verifying_key(
    epoch_key: &EpochKey,
    slot_index: u16,
    epoch_id: u64,
) -> ed25519_dalek::VerifyingKey {
    rendezvous_slot_key(epoch_key, slot_index, epoch_id).verifying_key()
}

/// Deterministic slot claim for the community advertiser set (slot cap =
/// `RENDEZVOUS_SLOT_COUNT`). Thin wrapper over the generic core kernel keyed by
/// the 16-byte owner address: sort the advertiser set ascending by address; the
/// position of `me` is its slot index. `None` if `me` is absent or ranks at/
/// beyond the slot cap. Because the advertiser set is CRDT-replicated, every
/// member computes the same ordering, so each slot has exactly one writer.
pub fn slot_for_advertiser(advertisers: &[OwnerAddr], me: &OwnerAddr) -> Option<u16> {
    // `OwnerAddr` is `Ord` over its 16-byte inner address, so it satisfies the
    // core helper's `A: Ord` bound directly — no need to strip to `[u8; 16]`.
    // The derived ordering sorts/dedups by the inner bytes, matching the prior
    // `a.0.cmp(&b.0)` exactly.
    core_slot_for_advertiser(advertisers, me, RENDEZVOUS_SLOT_COUNT)
}

/// Build the core rendezvous resolve config from the open-join env knobs.
/// `HARMONY_OPEN_JOIN_RESOLVE_CURVE` (comma-separated batch widths, each clamped
/// to `1..=RENDEZVOUS_SLOT_COUNT`) and `HARMONY_OPEN_JOIN_RESOLVE_DEADLINE_MS`
/// (clamped `>= 1`). Defaults to the `[1, 2, N]` widening curve at 2500ms.
pub fn rendezvous_config_from_env() -> RendezvousResolveConfig {
    let mut cfg = RendezvousResolveConfig {
        batch_curve: vec![1, 2, RENDEZVOUS_SLOT_COUNT],
        per_batch_deadline: Duration::from_millis(2_500),
    };
    if let Ok(curve) = std::env::var("HARMONY_OPEN_JOIN_RESOLVE_CURVE") {
        // CLAMP each parsed width to 1..=RENDEZVOUS_SLOT_COUNT rather than
        // dropping out-of-range entries: a user passing `8` with N=4 wants a
        // full-width batch, not a silently-dropped curve step.
        let parsed: Vec<usize> = curve
            .split(',')
            .filter_map(|s| s.trim().parse::<usize>().ok())
            .map(|w| w.clamp(1, RENDEZVOUS_SLOT_COUNT))
            .collect();
        if !parsed.is_empty() {
            cfg.batch_curve = parsed;
        }
    }
    if let Ok(ms) = std::env::var("HARMONY_OPEN_JOIN_RESOLVE_DEADLINE_MS") {
        if let Ok(ms) = ms.parse::<u64>() {
            cfg.per_batch_deadline = Duration::from_millis(ms.max(1));
        }
    }
    cfg
}

/// Production entry point: resolve a live community beacon from the DHT via the
/// generic kernel, keyed by `PkarrCase::Community` + the community rendezvous
/// info-layout, decoding the routing blob as a `ReachabilityAnnouncePayload`.
/// The BEP44 envelope already proves the writer held `epoch_key`, so the inner
/// identity signature is intentionally NOT verified here — the joiner does not
/// know the beacon's identity; trust is established at the handshake/admission
/// layer.
pub async fn resolve_rendezvous(
    pkarr: &Arc<PkarrResolver>,
    epoch_key: &EpochKey,
    now_ms: u64,
    cfg: &RendezvousResolveConfig,
) -> RendezvousResolveOutcome<ReachabilityAnnouncePayload> {
    // `PkarrSlotResolver` fields are private (the `ikm` secret never lives in a
    // public field); construct via `new`, which wraps `ikm` in `Zeroizing`.
    let resolver = PkarrSlotResolver::new(
        Arc::clone(pkarr),
        PkarrCase::Community,
        epoch_key.as_bytes().to_vec(),
        Arc::new(rendezvous_info),
        |blob: &[u8]| ciborium::from_reader::<ReachabilityAnnouncePayload, _>(blob).ok(),
    );
    resolve_rendezvous_with(&resolver, now_ms, cfg).await
}

/// ZEB-824: a resolved beacon with the outer record's identity preserved, so a
/// member-side caller can derive the beacon's `OwnerAddr` — the composite
/// device-address hash it uses as the **seed key** for the dial view. It is not
/// an admission input: admission is epoch-envelope trust (spec §5c), so there is
/// no membership gate on the far side of this type. The plain
/// [`resolve_rendezvous`] decode discards the outer
/// [`harmony_pkarr::PkarrRoutingRecord`]; open-join keeps using it (a joiner
/// defers identity trust to admission — module doc above).
#[derive(Debug, Clone)]
pub struct IdentifiedBeacon {
    pub payload: ReachabilityAnnouncePayload,
    /// The outer record's `harmony_identity_pub` (inner-sig-verified by
    /// `PkarrResolver::resolve`): X25519(32) ‖ Ed25519(32).
    pub beacon_identity_pub: [u8; 64],
}

/// Client-side [`SlotResolver`] that keeps the outer record's identity and
/// filters out our own endpoint. Mirrors the core `PkarrSlotResolver` probe
/// (derive slot vk → resolve → post-await freshness re-check → decode); it
/// exists because the core decode closure only sees the routing blob, so the
/// identity cannot be recovered from inside a closure.
struct IdentifiedSlotResolver {
    pkarr: Arc<PkarrResolver>,
    epoch_key_bytes: Zeroizing<Vec<u8>>,
    /// Our own iroh endpoint id: a record pointing at ourselves reads as an
    /// EMPTY slot, so the escalating-batch driver widens to the other slots
    /// (spec §5 self-dial hazard; ZEB-806 lesson — compare 32-byte endpoint
    /// ids, never 16-byte device ids).
    self_endpoint_id: [u8; 32],
    /// Slot probes that failed at the pkarr/transport layer during this
    /// resolve. To the escalating driver an errored probe still reads as a
    /// miss (widening must continue), but the caller needs the count to tell
    /// "no beacon published" (proof-shaped absence) apart from "resolve
    /// infrastructure failing" (no information) — review r1 finding 2.
    resolve_errors: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl harmony_pkarr::rendezvous::SlotResolver<IdentifiedBeacon> for IdentifiedSlotResolver {
    async fn resolve_slot(&self, slot_index: u16, epoch_id: u64) -> Option<IdentifiedBeacon> {
        let info = rendezvous_info(slot_index, epoch_id);
        let vk = derive_ephemeral_key(PkarrCase::Community, &self.epoch_key_bytes, &info)
            .verifying_key();
        let rec = match self.pkarr.resolve(&vk).await {
            Ok(Some(rec)) => rec,
            Ok(None) => return None,
            Err(e) => {
                self.resolve_errors.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(slot = slot_index, error = ?e,
                    "identified rendezvous probe errored — treating as a miss");
                return None;
            }
        };
        // Post-await freshness re-check, same as the core resolver (PR#306
        // stale-clock lesson).
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        rec.verify_freshness(now_ms).ok()?;
        let payload: ReachabilityAnnouncePayload =
            ciborium::from_reader(rec.routing_blob.as_slice()).ok()?;
        if payload.iroh_node_id == self.self_endpoint_id {
            return None;
        }
        Some(IdentifiedBeacon {
            payload,
            beacon_identity_pub: rec.harmony_identity_pub,
        })
    }
}

/// Result of an identified rendezvous resolve. `resolve_errors` counts slot
/// probes that failed at the pkarr/transport layer during this resolve —
/// the caller uses it to distinguish "no beacon published" (proof-shaped
/// absence) from "resolve infrastructure failing" (no information).
pub struct IdentifiedResolve {
    pub outcome: RendezvousResolveOutcome<IdentifiedBeacon>,
    pub resolve_errors: usize,
}

/// ZEB-824 production entry point: like [`resolve_rendezvous`], but yields an
/// [`IdentifiedBeacon`] and treats our own record as an empty slot. The
/// pkarr-layer probe-error count rides along in [`IdentifiedResolve`].
pub async fn resolve_rendezvous_identified(
    pkarr: &Arc<PkarrResolver>,
    epoch_key: &EpochKey,
    self_endpoint_id: [u8; 32],
    now_ms: u64,
    cfg: &RendezvousResolveConfig,
) -> IdentifiedResolve {
    let resolve_errors = Arc::new(AtomicUsize::new(0));
    let resolver = IdentifiedSlotResolver {
        pkarr: Arc::clone(pkarr),
        epoch_key_bytes: Zeroizing::new(epoch_key.as_bytes().to_vec()),
        self_endpoint_id,
        resolve_errors: Arc::clone(&resolve_errors),
    };
    let outcome = resolve_rendezvous_with(&resolver, now_ms, cfg).await;
    IdentifiedResolve {
        outcome,
        resolve_errors: resolve_errors.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ek() -> EpochKey {
        EpochKey::new([7u8; 32])
    }

    #[test]
    fn slot_key_is_deterministic() {
        let a = rendezvous_slot_verifying_key(&ek(), 0, 42);
        let b = rendezvous_slot_verifying_key(&ek(), 0, 42);
        assert_eq!(a.to_bytes(), b.to_bytes());
    }

    #[test]
    fn distinct_slots_and_epochs_give_distinct_keys() {
        let s0 = rendezvous_slot_verifying_key(&ek(), 0, 42).to_bytes();
        let s1 = rendezvous_slot_verifying_key(&ek(), 1, 42).to_bytes();
        let s0_next = rendezvous_slot_verifying_key(&ek(), 0, 43).to_bytes();
        assert_ne!(s0, s1, "different slot index must yield a different key");
        assert_ne!(s0, s0_next, "different epoch must yield a different key");
    }

    #[test]
    fn rendezvous_key_is_disjoint_from_member_keyed_record() {
        // Member-keyed info is identity_pub(64) || epoch_id(8) = 72 bytes under
        // the SAME Community salt. Reconstruct it and confirm no rendezvous slot
        // collides with it.
        let epoch_id = 42u64;
        let identity_pub = [9u8; 64];
        let mut member_info = Vec::with_capacity(72);
        member_info.extend_from_slice(&identity_pub);
        member_info.extend_from_slice(&epoch_id.to_be_bytes());
        let member_key = derive_ephemeral_key(PkarrCase::Community, ek().as_bytes(), &member_info)
            .verifying_key()
            .to_bytes();
        for slot in 0..RENDEZVOUS_SLOT_COUNT as u16 {
            let rk = rendezvous_slot_verifying_key(&ek(), slot, epoch_id).to_bytes();
            assert_ne!(rk, member_key, "slot {slot} aliased a member key");
        }
    }

    #[test]
    fn slot_count_tracks_advertiser_cap() {
        assert_eq!(RENDEZVOUS_SLOT_COUNT, COMMUNITY_RELAY_ADVERTISERS_MAX);
    }

    fn addr(b: u8) -> OwnerAddr {
        OwnerAddr([b; 16])
    }

    #[test]
    fn slot_assignment_is_deterministic_across_members() {
        // Two members compute the SAME ordering from the same (unordered) set.
        let set_a = vec![addr(3), addr(1), addr(2)];
        let set_b = vec![addr(2), addr(3), addr(1)];
        for who in [addr(1), addr(2), addr(3)] {
            assert_eq!(
                slot_for_advertiser(&set_a, &who),
                slot_for_advertiser(&set_b, &who),
                "ordering disagreed for {who:?}"
            );
        }
        // Sorted ascending: addr(1)->0, addr(2)->1, addr(3)->2.
        assert_eq!(slot_for_advertiser(&set_a, &addr(1)), Some(0));
        assert_eq!(slot_for_advertiser(&set_a, &addr(2)), Some(1));
        assert_eq!(slot_for_advertiser(&set_a, &addr(3)), Some(2));
    }

    #[test]
    fn not_in_set_returns_none() {
        let set = vec![addr(1), addr(2)];
        assert_eq!(slot_for_advertiser(&set, &addr(9)), None);
    }

    #[test]
    fn rank_beyond_cap_returns_none() {
        // RENDEZVOUS_SLOT_COUNT (=4) advertisers fill slots 0..3; a 5th (highest
        // address) ranks 4 >= cap and claims no slot.
        let set: Vec<OwnerAddr> = (1..=(RENDEZVOUS_SLOT_COUNT as u8 + 1)).map(addr).collect();
        let highest = addr(RENDEZVOUS_SLOT_COUNT as u8 + 1);
        assert_eq!(slot_for_advertiser(&set, &highest), None);
        // The one just under the cap still gets the last valid slot.
        let last_valid = addr(RENDEZVOUS_SLOT_COUNT as u8);
        assert_eq!(
            slot_for_advertiser(&set, &last_valid),
            Some((RENDEZVOUS_SLOT_COUNT - 1) as u16)
        );
    }

    #[test]
    fn duplicate_addresses_do_not_shift_ranks() {
        // Defensive: a duplicated advertiser must not change anyone's slot.
        let set = vec![addr(1), addr(2), addr(2), addr(3)];
        assert_eq!(slot_for_advertiser(&set, &addr(1)), Some(0));
        assert_eq!(slot_for_advertiser(&set, &addr(2)), Some(1));
        assert_eq!(slot_for_advertiser(&set, &addr(3)), Some(2));
    }
}
