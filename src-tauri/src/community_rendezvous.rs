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

use crate::membership_vouch::MembershipVouch;
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

/// Number of enumerated rendezvous slots.
///
/// ZEB-910: deliberately DECOUPLED from (and larger than)
/// [`COMMUNITY_RELAY_ADVERTISERS_MAX`]. Slots bound *discovery* — how many
/// advertisers publish a bridgeable beacon — while the relay cap bounds
/// *service reads* (`relays_for_community` truncation feeding the pull
/// drivers). Raising only the slot count shrinks the chance that every beacon
/// publisher sits in one island after a community split, without doubling
/// relay-pull fan-out. Compatibility: the slot index is only a key-derivation
/// input — old resolvers probe slots 0..4 and still find those publishers;
/// old publishers fill only 0..4, which new resolvers still probe.
pub const RENDEZVOUS_SLOT_COUNT: usize = 8;

/// ZEB-827: encode a rendezvous slot's routing blob — a superset of
/// `ReachabilityAnnouncePayload` carrying an optional membership vouch. A `None`
/// vouch encodes byte-identically to the bare payload (so already-deployed peers
/// keep decoding published blobs unchanged); a `Some` vouch is merged into the
/// payload's CBOR map under the `"mv"` key, which legacy bare-payload decoders
/// ignore. Uses CBOR-`Value` map-merge rather than `#[serde(flatten)]` — the
/// latter emits an indefinite-length map and so is NOT byte-compatible with the
/// definite-length legacy encoding.
pub fn encode_rendezvous_blob(
    reachability: &ReachabilityAnnouncePayload,
    vouch: Option<&MembershipVouch>,
) -> Vec<u8> {
    let mut out = Vec::new();
    // Fixed-size/serializable payload — encode cannot fail in practice.
    let _ = ciborium::into_writer(reachability, &mut out);
    let Some(v) = vouch else {
        return out;
    };
    // Merge "mv" into the payload's CBOR map without disturbing existing keys.
    let mut val: ciborium::value::Value = match ciborium::from_reader(&out[..]) {
        Ok(val) => val,
        Err(_) => return out,
    };
    if let ciborium::value::Value::Map(entries) = &mut val {
        let mut vbytes = Vec::new();
        if ciborium::into_writer(v, &mut vbytes).is_ok() {
            if let Ok(vval) = ciborium::from_reader::<ciborium::value::Value, _>(&vbytes[..]) {
                entries.push((ciborium::value::Value::Text("mv".to_string()), vval));
            }
        }
    }
    let mut merged = Vec::new();
    let _ = ciborium::into_writer(&val, &mut merged);
    merged
}

/// Decode a rendezvous routing blob. Tolerates a legacy bare payload (no
/// vouch → `None`) and a vouch-carrying superset alike.
pub fn decode_rendezvous_blob(
    bytes: &[u8],
) -> Option<(ReachabilityAnnouncePayload, Option<MembershipVouch>)> {
    let reachability: ReachabilityAnnouncePayload = ciborium::from_reader(bytes).ok()?;
    let mut vouch = None;
    if let Ok(ciborium::value::Value::Map(entries)) =
        ciborium::from_reader::<ciborium::value::Value, _>(bytes)
    {
        for (k, v) in entries {
            if let ciborium::value::Value::Text(t) = &k {
                if t.as_str() == "mv" {
                    let mut vb = Vec::new();
                    if ciborium::into_writer(&v, &mut vb).is_ok() {
                        vouch = ciborium::from_reader::<MembershipVouch, _>(&vb[..]).ok();
                    }
                }
            }
        }
    }
    Some((reachability, vouch))
}

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
    /// ZEB-827: the enrolled device key whose vouch this beacon passed (against
    /// the resolve-time enrolled snapshot). Carried so the driver can
    /// re-validate membership after the async resolve, before seeding — closing
    /// the revoke-during-resolve TOCTOU window (CR-2).
    pub membership_device_vk: [u8; 32],
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
    /// ZEB-827: this community's id (the vouch binds it) and the union of
    /// Joined (non-self) members' effective enrolled device keys — the set a
    /// beacon's vouch key must belong to.
    community_id: crate::owner_state_types::SpaceId,
    enrolled_keys: Arc<std::collections::HashSet<[u8; 32]>>,
    /// Beacons that verified transport+epoch but failed the membership vouch
    /// (missing, malformed, stale, bad sig, or device not enrolled). Read as an
    /// empty slot so the batch driver widens — mirrors `resolve_errors`.
    membership_rejects: Arc<AtomicUsize>,
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
        // ZEB-827 strict: a beacon must carry a vouch that (a) verifies under a
        // member's enrolled device key and (b) binds THIS record's transport
        // identity + community. Any failure reads as an EMPTY slot so the
        // escalating batch driver widens to the other slots (same shape as the
        // self-filter below), while `membership_rejects` records that a beacon
        // WAS present — the caller maps that to `rejectedNonMember`. A DEBUG log
        // carries the specific sub-reason so a strict-rollout spike is
        // diagnosable without an env-var (spec §7).
        //
        // CA-1: a fresh, inner-sig-valid record whose blob won't decode is a
        // malformed beacon (present but unusable), NOT proof-shaped absence — so
        // it too is a membership rejection, not a miss.
        let Some((payload, vouch)) = decode_rendezvous_blob(rec.routing_blob.as_slice()) else {
            self.membership_rejects.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                slot = slot_index,
                reason = "malformed_blob",
                "ZEB-827: rendezvous beacon rejected — routing blob did not decode"
            );
            return None;
        };
        if payload.iroh_node_id == self.self_endpoint_id {
            return None;
        }
        let device_vk = match &vouch {
            None => {
                self.membership_rejects.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    slot = slot_index,
                    node_id = %hex::encode(&payload.iroh_node_id[..8]),
                    reason = "missing_vouch",
                    "ZEB-827: rendezvous beacon rejected — no membership vouch"
                );
                return None;
            }
            Some(v) => match crate::membership_vouch::verify_membership_vouch(
                v,
                self.community_id,
                &rec.harmony_identity_pub,
                &self.enrolled_keys,
                now_ms,
            ) {
                Ok(()) => v.device_vk,
                Err(reason) => {
                    self.membership_rejects.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(
                        slot = slot_index,
                        node_id = %hex::encode(&payload.iroh_node_id[..8]),
                        reason = reason.as_str(),
                        "ZEB-827: rendezvous beacon rejected — invalid membership vouch"
                    );
                    return None;
                }
            },
        };
        Some(IdentifiedBeacon {
            payload,
            beacon_identity_pub: rec.harmony_identity_pub,
            membership_device_vk: device_vk,
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
    /// ZEB-827: beacons present but rejected for lacking a valid membership
    /// vouch. Nonzero with an empty outcome means "a beacon was here but is not
    /// a member" (→ `rejectedNonMember`), distinct from proof-shaped absence.
    pub membership_rejects: usize,
}

/// ZEB-824 production entry point: like [`resolve_rendezvous`], but yields an
/// [`IdentifiedBeacon`] and treats our own record as an empty slot. The
/// pkarr-layer probe-error count rides along in [`IdentifiedResolve`].
///
/// ZEB-827: `community_id` + `enrolled_keys` drive the strict membership-vouch
/// check inside each slot probe (a beacon lacking a valid vouch reads as an
/// empty slot so the escalating batch driver widens past it).
pub async fn resolve_rendezvous_identified(
    pkarr: &Arc<PkarrResolver>,
    epoch_key: &EpochKey,
    self_endpoint_id: [u8; 32],
    community_id: crate::owner_state_types::SpaceId,
    enrolled_keys: Arc<std::collections::HashSet<[u8; 32]>>,
    now_ms: u64,
    cfg: &RendezvousResolveConfig,
) -> IdentifiedResolve {
    let resolve_errors = Arc::new(AtomicUsize::new(0));
    let membership_rejects = Arc::new(AtomicUsize::new(0));
    let resolver = IdentifiedSlotResolver {
        pkarr: Arc::clone(pkarr),
        epoch_key_bytes: Zeroizing::new(epoch_key.as_bytes().to_vec()),
        self_endpoint_id,
        resolve_errors: Arc::clone(&resolve_errors),
        community_id,
        enrolled_keys,
        membership_rejects: Arc::clone(&membership_rejects),
    };
    let outcome = resolve_rendezvous_with(&resolver, now_ms, cfg).await;
    IdentifiedResolve {
        outcome,
        resolve_errors: resolve_errors.load(Ordering::Relaxed),
        membership_rejects: membership_rejects.load(Ordering::Relaxed),
    }
}

/// ZEB-910: result of [`resolve_rendezvous_all_slots`] — every distinct
/// verified beacon across the full slot space, plus the same
/// error/rejection counts [`IdentifiedResolve`] carries.
pub struct AllSlotsResolve {
    /// Deduped by `iroh_node_id`; a node republishing across the weekly
    /// boundary keeps its CURRENT-epoch beacon.
    pub hits: Vec<IdentifiedBeacon>,
    pub resolve_errors: usize,
    pub membership_rejects: usize,
}

/// ZEB-910: probe every slot (`0..RENDEZVOUS_SLOT_COUNT`) across the
/// current + previous time epoch concurrently and collect ALL distinct hits.
/// Returns `(hits, timed_out_probe_count)` — a timed-out probe carries no
/// information (infrastructure trouble, not proof of absence), so the caller
/// folds the count into `resolve_errors`.
///
/// This deliberately skips the escalating-batch economy of
/// [`resolve_rendezvous_with`]: repair passes (the gateway dial driver's
/// Starved/Degraded ladder) are already ladder-paced and need every reachable
/// island's beacon, not the first one — on a community split the single-hit
/// resolve is coin-flip likely to return an already-reachable member and
/// never bridge (spec §3.3.1).
pub(crate) async fn all_slots_scan<R>(
    resolver: &R,
    now_ms: u64,
    per_probe_deadline: Duration,
) -> (Vec<IdentifiedBeacon>, usize)
where
    R: harmony_pkarr::rendezvous::SlotResolver<IdentifiedBeacon> + Sync,
{
    // PR #659 review: bound in-flight probes — a repair pass must never hold
    // more concurrent pkarr GETs than the resolver's stale-refresh path may
    // (`PKARR_REFRESH_MAX_CONCURRENT` = 4; kept as a local const to avoid a
    // cross-module dependency on a pub(crate) tuning knob).
    const ALL_SLOTS_PROBE_CONCURRENCY: usize = 4;
    use futures::StreamExt;
    let current = harmony_pkarr::current_epoch_id(now_ms);
    let mut epochs = vec![current];
    if current > 0 {
        epochs.push(current - 1);
    }
    // Current-epoch probes FIRST: `buffered` (the ORDERED variant — never
    // `buffer_unordered` here) preserves input order, so the dedup below
    // prefers a node's current-epoch beacon over its previous one.
    let probes: Vec<(u16, u64)> = epochs
        .iter()
        .flat_map(|e| (0..RENDEZVOUS_SLOT_COUNT as u16).map(move |s| (s, *e)))
        .collect();
    let results: Vec<(Option<IdentifiedBeacon>, bool)> =
        futures::stream::iter(probes.into_iter().map(|(slot, epoch)| async move {
            match tokio::time::timeout(per_probe_deadline, resolver.resolve_slot(slot, epoch)).await
            {
                Ok(hit) => (hit, false),
                Err(_) => (None, true),
            }
        }))
        .buffered(ALL_SLOTS_PROBE_CONCURRENCY)
        .collect()
        .await;
    let mut timeouts = 0usize;
    let mut seen = std::collections::HashSet::new();
    let mut hits = Vec::new();
    for (hit, timed_out) in results {
        if timed_out {
            timeouts += 1;
        }
        if let Some(beacon) = hit {
            if seen.insert(beacon.payload.iroh_node_id) {
                hits.push(beacon);
            }
        }
    }
    (hits, timeouts)
}

/// ZEB-910 production entry point: like [`resolve_rendezvous_identified`] but
/// scanning EVERY slot and returning every verified hit (see
/// [`all_slots_scan`] for why). Per-probe deadline = the config's
/// `per_batch_deadline`; membership-vouch verification, freshness re-checks,
/// and the self-endpoint filter are identical (same [`IdentifiedSlotResolver`]).
pub async fn resolve_rendezvous_all_slots(
    pkarr: &Arc<PkarrResolver>,
    epoch_key: &EpochKey,
    self_endpoint_id: [u8; 32],
    community_id: crate::owner_state_types::SpaceId,
    enrolled_keys: Arc<std::collections::HashSet<[u8; 32]>>,
    now_ms: u64,
    cfg: &RendezvousResolveConfig,
) -> AllSlotsResolve {
    let resolve_errors = Arc::new(AtomicUsize::new(0));
    let membership_rejects = Arc::new(AtomicUsize::new(0));
    let resolver = IdentifiedSlotResolver {
        pkarr: Arc::clone(pkarr),
        epoch_key_bytes: Zeroizing::new(epoch_key.as_bytes().to_vec()),
        self_endpoint_id,
        resolve_errors: Arc::clone(&resolve_errors),
        community_id,
        enrolled_keys,
        membership_rejects: Arc::clone(&membership_rejects),
    };
    let (hits, timeouts) = all_slots_scan(&resolver, now_ms, cfg.per_batch_deadline).await;
    AllSlotsResolve {
        hits,
        resolve_errors: resolve_errors.load(Ordering::Relaxed) + timeouts,
        membership_rejects: membership_rejects.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_relay_announce::COMMUNITY_RELAY_ADVERTISERS_MAX;

    fn ek() -> EpochKey {
        EpochKey::new([7u8; 32])
    }

    fn test_beacon(node: u8) -> IdentifiedBeacon {
        IdentifiedBeacon {
            payload: ReachabilityAnnouncePayload {
                iroh_node_id: [node; 32],
                home_relay_url: String::new(),
                direct_addresses: vec![],
                announced_at_ms: 0,
                identity_signature: [0u8; 64],
                butler_set: vec![],
                bs_at: 0,
            },
            beacon_identity_pub: [0u8; 64],
            membership_device_vk: [node; 32],
        }
    }

    /// Map-backed [`SlotResolver`] for the ZEB-910 all-slots scan tests: a hit
    /// per explicitly-seeded `(slot, epoch)` coordinate, empty everywhere else.
    struct MapSlotResolver {
        hits: std::collections::HashMap<(u16, u64), IdentifiedBeacon>,
    }

    #[async_trait::async_trait]
    impl harmony_pkarr::rendezvous::SlotResolver<IdentifiedBeacon> for MapSlotResolver {
        async fn resolve_slot(&self, slot_index: u16, epoch_id: u64) -> Option<IdentifiedBeacon> {
            self.hits.get(&(slot_index, epoch_id)).cloned()
        }
    }

    /// ZEB-910: the scan visits every slot in BOTH tolerance-window epochs and
    /// returns each distinct beacon — the property the escalating single-hit
    /// driver deliberately lacks.
    #[tokio::test]
    async fn all_slots_scan_collects_every_distinct_hit() {
        let now = 1_700_000_000_000u64;
        let cur = harmony_pkarr::current_epoch_id(now);
        let prev = cur.saturating_sub(1);
        let mut hits = std::collections::HashMap::new();
        hits.insert((0u16, cur), test_beacon(1));
        hits.insert((3u16, cur), test_beacon(2));
        hits.insert((1u16, prev), test_beacon(3));
        let r = MapSlotResolver { hits };
        let (found, timeouts) = all_slots_scan(&r, now, Duration::from_millis(500)).await;
        assert_eq!(timeouts, 0);
        let ids: std::collections::HashSet<[u8; 32]> =
            found.iter().map(|b| b.payload.iroh_node_id).collect();
        let want: std::collections::HashSet<[u8; 32]> =
            [[1u8; 32], [2u8; 32], [3u8; 32]].into_iter().collect();
        assert_eq!(ids, want);
    }

    /// ZEB-910: a node republishing across the weekly boundary appears once,
    /// and the CURRENT epoch's beacon wins the dedup.
    #[tokio::test]
    async fn all_slots_scan_dedups_same_node_across_epochs_preferring_current() {
        let now = 1_700_000_000_000u64;
        let cur = harmony_pkarr::current_epoch_id(now);
        let prev = cur.saturating_sub(1);
        let mut current_beacon = test_beacon(1);
        current_beacon.membership_device_vk = [0xAA; 32];
        let mut previous_beacon = test_beacon(1);
        previous_beacon.membership_device_vk = [0xBB; 32];
        let mut hits = std::collections::HashMap::new();
        hits.insert((0u16, cur), current_beacon);
        hits.insert((0u16, prev), previous_beacon);
        let r = MapSlotResolver { hits };
        let (found, _) = all_slots_scan(&r, now, Duration::from_millis(500)).await;
        assert_eq!(found.len(), 1, "same node across epochs must dedup");
        assert_eq!(
            found[0].membership_device_vk, [0xAA; 32],
            "the current-epoch beacon wins"
        );
    }

    /// ZEB-910: an all-empty slot space yields no hits and no phantom timeouts.
    #[tokio::test]
    async fn all_slots_scan_empty_when_no_slot_has_a_beacon() {
        let r = MapSlotResolver {
            hits: std::collections::HashMap::new(),
        };
        let (found, timeouts) =
            all_slots_scan(&r, 1_700_000_000_000, Duration::from_millis(500)).await;
        assert!(found.is_empty());
        assert_eq!(timeouts, 0);
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

    /// ZEB-910: slots bound DISCOVERY (who publishes a bridgeable beacon), the
    /// relay cap bounds SERVICE reads (`relays_for_community` fan-out).
    /// Decoupled deliberately — advertisers ranked 4..8 publish beacons
    /// without joining the pull set, so one island can't hold every slot
    /// as easily while relay-pull load stays unchanged.
    #[test]
    fn slot_count_exceeds_relay_read_cap_by_design() {
        assert_eq!(RENDEZVOUS_SLOT_COUNT, 8);
        const _: () = assert!(RENDEZVOUS_SLOT_COUNT >= COMMUNITY_RELAY_ADVERTISERS_MAX);
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

#[cfg(test)]
mod rendezvous_blob_tests {
    use super::*;
    use crate::membership_vouch::mint_membership_vouch;
    use crate::owner_state_types::SpaceId;
    use crate::reachability_record::ReachabilityAnnouncePayload;
    use ed25519_dalek::SigningKey;

    fn payload() -> ReachabilityAnnouncePayload {
        ReachabilityAnnouncePayload {
            iroh_node_id: [0xAB; 32],
            home_relay_url: "https://derp.example/".to_string(),
            direct_addresses: vec![],
            announced_at_ms: 1_700_000_000_000,
            identity_signature: [0xCD; 64],
            butler_set: vec![],
            bs_at: 0,
        }
    }

    #[test]
    fn roundtrips_with_vouch() {
        let p = payload();
        let v = mint_membership_vouch(
            &SigningKey::from_bytes(&[3; 32]),
            SpaceId([1; 16]),
            &[9; 64],
            1,
            2,
        );
        let bytes = encode_rendezvous_blob(&p, Some(&v));
        let (dp, dv) = decode_rendezvous_blob(&bytes).expect("decode");
        assert_eq!(dp, p);
        assert_eq!(dv, Some(v));
    }

    #[test]
    fn vouchless_blob_is_byte_identical_to_bare_payload() {
        let p = payload();
        let wrapped = encode_rendezvous_blob(&p, None);
        let mut bare = Vec::new();
        ciborium::into_writer(&p, &mut bare).unwrap();
        assert_eq!(
            wrapped, bare,
            "vouchless wrapper must equal legacy bare payload bytes"
        );
    }

    #[test]
    fn legacy_bare_decode_ignores_vouch() {
        // Back-compat: an OLD resolver decoding a NEW (vouch-carrying) blob as a
        // bare payload still recovers reachability.
        let p = payload();
        let v = mint_membership_vouch(
            &SigningKey::from_bytes(&[3; 32]),
            SpaceId([1; 16]),
            &[9; 64],
            1,
            2,
        );
        let bytes = encode_rendezvous_blob(&p, Some(&v));
        let bare: ReachabilityAnnouncePayload =
            ciborium::from_reader(&bytes[..]).expect("bare decode of wrapped");
        assert_eq!(bare, p);
    }

    #[test]
    fn decode_of_legacy_bare_yields_no_vouch() {
        // Forward-compat: a NEW resolver decoding an OLD (bare) blob sees no vouch.
        let p = payload();
        let mut bare = Vec::new();
        ciborium::into_writer(&p, &mut bare).unwrap();
        let (dp, dv) = decode_rendezvous_blob(&bare).expect("decode bare");
        assert_eq!(dp, p);
        assert_eq!(dv, None);
    }
}
