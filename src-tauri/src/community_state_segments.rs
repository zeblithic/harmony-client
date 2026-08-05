//! ZEB-814: segmented community-state root — pure core.
//!
//! Replaces the monolithic ≤1 MiB community-state root blob with a small
//! **manifest** that references immutable, content-addressed **segments**.
//! This module is the pure, sync, engine-free core:
//!
//! - the wire/persist types (`SegmentRef`, `ManifestCleartext`,
//!   `SegmentCleartext`, `SegmentIndex`),
//! - `plan_segments` — the deterministic, cascade-free partition of a
//!   sorted membership log into sealed segments + a live tail,
//! - `seal_segment`/`seal_manifest` and `open_segment`/`open_manifest`
//!   crypto helpers (reusing `community_state_sync::{encrypt_blob,
//!   decrypt_blob}`).
//!
//! ## Invariants
//!
//! - **Segmentation is a transport partition only.** The receive path always
//!   re-sorts every event by the full `event_sort_key`
//!   (`community_membership::event_sort_key`), so a mis-assigned event is
//!   harmless to correctness — segments only decide *which blob carries which
//!   events*, never ordering or validity.
//! - **Boundaries are pinned `(wall_ms, logical, device_id, EventId)` cut
//!   points** (the `sig` tail of `event_sort_key` is dropped). Because upper
//!   boundaries are pinned, a backdated event whose key lands inside an
//!   already-sealed interval re-seals exactly that one segment — later
//!   segments never shift (no cascade).
//! - **`K_s` is epoch-independent.** Each segment is encrypted under its own
//!   random 32-byte key `K_s`; the manifest (encrypted under the *current*
//!   epoch key) carries every `K_s` in plaintext. So a segment's ContentId is
//!   stable across epoch rotations, and a rotation re-encrypts only the small
//!   manifest — never the segments (backward secrecy: a kicked member can't
//!   decrypt post-rotation manifests, so can't recover any `K_s`).
//! - **Per-publisher O(delta).** A publisher persists its sealed segments'
//!   `(K_s, cid)` in the `SegmentIndex` sidecar and reuses them, so its own
//!   republishes re-`put` only the changed tail segment.

use serde::{Deserialize, Serialize};

use crate::community_membership::{EventId, SignedMembershipEvent};
use crate::owner_state_crypto::{
    canonical_cbor_decode, canonical_cbor_encode, sealed::CanonicalPayloadSealed, CanonicalPayload,
};
use crate::owner_state_types::{
    deserialize_bytes_from_bstr, serialize_bytes_as_bstr, EpochKey, SpaceId,
};

/// Seal a tail segment once it reaches this many events. (Approved threshold.)
pub const SEGMENT_SEAL_EVENTS: usize = 512;
/// …or once its cleartext reaches this many bytes, whichever comes first.
/// Well under `MAX_PAYLOAD_SIZE` so a single segment can never approach the cap.
pub const SEGMENT_SEAL_BYTES: usize = 256 * 1024;

/// The `"mf"` publish-payload discriminator value for a segmented (manifest)
/// root. `None` on the wire ⇒ legacy monolithic root.
pub const MANIFEST_FORMAT_V1: u8 = 1;

const SEGMENT_CLEARTEXT_V1: u16 = 1;
/// Manifest cleartext format version. `pub` so the publish path (Task 3)
/// stamps new manifests with it.
pub const MANIFEST_CLEARTEXT_V1: u16 = 1;
const SEGMENT_INDEX_V1: u16 = 1;

/// A pinned cut point in `event_sort_key` order, minus the `sig` tail.
///
/// Total order is `(wall_ms, logical, device_id, id)` — see [`boundary_key`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventBoundary {
    #[serde(rename = "wm")]
    pub wall_ms: u64,
    #[serde(rename = "lg")]
    pub logical: u32,
    #[serde(rename = "dv")]
    pub device_id: String,
    #[serde(
        rename = "id",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub id: EventId,
}

impl EventBoundary {
    /// The boundary of an event (its `event_sort_key` minus `sig`).
    pub fn of(e: &SignedMembershipEvent) -> Self {
        EventBoundary {
            wall_ms: e.at.wall_ms,
            logical: e.at.logical,
            device_id: e.at.device_id.clone(),
            id: e.id,
        }
    }
}

/// Total-order key for a boundary. Matches `event_sort_key` (`community_
/// membership.rs`) with the `sig` field dropped — sufficient because
/// segmentation is a transport partition (see module invariants).
fn boundary_key(b: &EventBoundary) -> (u64, u32, &str, &[u8; 16]) {
    (b.wall_ms, b.logical, b.device_id.as_str(), &b.id)
}

/// Total-order key for an event, matching [`boundary_key`].
fn event_key(e: &SignedMembershipEvent) -> (u64, u32, &str, &[u8; 16]) {
    (e.at.wall_ms, e.at.logical, e.at.device_id.as_str(), &e.id)
}

/// A manifest entry describing one sealed segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentRef {
    #[serde(rename = "sc")]
    pub segment_cid: harmony_content::cid::ContentId,
    #[serde(rename = "lo")]
    pub lo: EventBoundary,
    #[serde(rename = "hi")]
    pub hi: EventBoundary,
    #[serde(rename = "nn")]
    pub count: u32,
    #[serde(
        rename = "ks",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub k_s: [u8; 32],
}

/// The new root object (encrypted under the current epoch key). Carries the
/// sealed-segment list + `K_s` keys + the unsealed live tail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestCleartext {
    #[serde(rename = "vn")]
    pub version: u16,
    #[serde(rename = "ci")]
    pub community_id: SpaceId,
    #[serde(rename = "sg")]
    pub segments: Vec<SegmentRef>,
    #[serde(rename = "tl")]
    pub tail: Vec<SignedMembershipEvent>,
}

/// A single segment's cleartext (encrypted under its `K_s`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentCleartext {
    #[serde(rename = "vn")]
    pub version: u16,
    #[serde(rename = "ci")]
    pub community_id: SpaceId,
    #[serde(rename = "ev")]
    pub events: Vec<SignedMembershipEvent>,
}

// Both cleartexts are canonical-CBOR encoded/decoded (deterministic byte order
// → reproducible ContentIds across replicas). The `CanonicalPayload` seal is
// `pub(crate)`, so same-crate impls are permitted.
impl CanonicalPayloadSealed for SegmentCleartext {}
impl CanonicalPayload for SegmentCleartext {}
impl CanonicalPayloadSealed for ManifestCleartext {}
impl CanonicalPayload for ManifestCleartext {}
// The sidecar index is persisted via `canonical_cbor_encode` (Task 2).
impl CanonicalPayloadSealed for SegmentIndex {}
impl CanonicalPayload for SegmentIndex {}

/// One entry in the local publisher sidecar (see `community_state_persist`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedEntry {
    #[serde(rename = "lo")]
    pub lo: EventBoundary,
    #[serde(rename = "hi")]
    pub hi: EventBoundary,
    #[serde(rename = "nn")]
    pub count: u32,
    #[serde(
        rename = "ks",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub k_s: [u8; 32],
    #[serde(rename = "sc")]
    pub segment_cid: harmony_content::cid::ContentId,
}

/// The publisher's local segment index (sidecar `segments.cbor`). Ascending
/// by `lo`. Gives per-publisher segment-CID stability across republishes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentIndex {
    #[serde(rename = "vn")]
    pub version: u16,
    #[serde(rename = "sg")]
    pub sealed: Vec<SealedEntry>,
}

/// Output of a publish-time re-derivation (`plan_segments`).
#[derive(Clone, Debug)]
pub struct SegmentPlan {
    /// Sealed-segment refs for the manifest, ascending by `lo`.
    pub refs: Vec<SegmentRef>,
    /// Unsealed live-tail events (event_sort_key order).
    pub tail: Vec<SignedMembershipEvent>,
    /// The index to persist to the sidecar.
    pub index: SegmentIndex,
    /// Segments sealed or re-sealed this call — `(ref, ciphertext)` to
    /// `put_serveable`. Reused (unchanged) segments are NOT included.
    pub newly_sealed: Vec<(SegmentRef, Vec<u8>)>,
}

#[derive(thiserror::Error, Debug)]
pub enum SegmentError {
    #[error("CBOR encode: {0}")]
    CborEncode(String),
    #[error("CBOR decode: {0}")]
    CborDecode(String),
    #[error("crypto: {0}")]
    Crypto(#[from] crate::community_state_sync::CommunityCryptoError),
    #[error("ContentId: {0}")]
    ContentId(String),
    #[error("misrouted segment/manifest: expected {expected:?} found {found:?}")]
    Misrouted { expected: SpaceId, found: SpaceId },
}

/// Encrypt+CID a run of events as a segment under `k_s`. Returns
/// `(segment_cid, ciphertext)`; the ciphertext is what goes to CAS.
pub fn seal_segment(
    community_id: SpaceId,
    events: &[SignedMembershipEvent],
    k_s: &[u8; 32],
) -> Result<(harmony_content::cid::ContentId, Vec<u8>), SegmentError> {
    let cleartext = SegmentCleartext {
        version: SEGMENT_CLEARTEXT_V1,
        community_id,
        events: events.to_vec(),
    };
    let bytes =
        canonical_cbor_encode(&cleartext).map_err(|e| SegmentError::CborEncode(e.to_string()))?;
    let key = EpochKey::new(*k_s);
    let ciphertext = crate::community_state_sync::encrypt_blob(&key, &bytes)?;
    let cid = harmony_content::cid::ContentId::for_book(
        &ciphertext,
        harmony_content::cid::ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .map_err(|e| SegmentError::ContentId(e.to_string()))?;
    Ok((cid, ciphertext))
}

/// Decrypt a segment blob under its `K_s` and return its events. Verifies the
/// embedded `community_id` matches `expected_community`.
pub fn open_segment(
    k_s: &[u8; 32],
    expected_community: SpaceId,
    ciphertext: &[u8],
) -> Result<Vec<SignedMembershipEvent>, SegmentError> {
    let key = EpochKey::new(*k_s);
    let bytes = crate::community_state_sync::decrypt_blob(&key, ciphertext)?;
    let cleartext: SegmentCleartext =
        canonical_cbor_decode(&bytes).map_err(|e| SegmentError::CborDecode(e.to_string()))?;
    if cleartext.community_id != expected_community {
        return Err(SegmentError::Misrouted {
            expected: expected_community,
            found: cleartext.community_id,
        });
    }
    Ok(cleartext.events)
}

/// Encrypt+CID a manifest under the current epoch key.
pub fn seal_manifest(
    epoch_key: &EpochKey,
    manifest: &ManifestCleartext,
) -> Result<(harmony_content::cid::ContentId, Vec<u8>), SegmentError> {
    let bytes =
        canonical_cbor_encode(manifest).map_err(|e| SegmentError::CborEncode(e.to_string()))?;
    let ciphertext = crate::community_state_sync::encrypt_blob(epoch_key, &bytes)?;
    let cid = harmony_content::cid::ContentId::for_book(
        &ciphertext,
        harmony_content::cid::ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .map_err(|e| SegmentError::ContentId(e.to_string()))?;
    Ok((cid, ciphertext))
}

/// Decrypt a manifest blob under the current epoch key. Verifies `community_id`.
pub fn open_manifest(
    epoch_key: &EpochKey,
    expected_community: SpaceId,
    ciphertext: &[u8],
) -> Result<ManifestCleartext, SegmentError> {
    let bytes = crate::community_state_sync::decrypt_blob(epoch_key, ciphertext)?;
    let manifest: ManifestCleartext =
        canonical_cbor_decode(&bytes).map_err(|e| SegmentError::CborDecode(e.to_string()))?;
    if manifest.community_id != expected_community {
        return Err(SegmentError::Misrouted {
            expected: expected_community,
            found: manifest.community_id,
        });
    }
    Ok(manifest)
}

/// Per-event serialized size (for the byte-threshold seal cut). Uses the
/// event's own canonical CBOR length — a close proxy for its contribution to
/// the segment cleartext (segment framing overhead is a few dozen bytes,
/// negligible against `SEGMENT_SEAL_BYTES`).
fn event_bytes(e: &SignedMembershipEvent) -> usize {
    canonical_cbor_encode(e).map(|b| b.len()).unwrap_or(0)
}

/// Deterministically (re)partition `sorted_events` against a prior index,
/// sealing new and dirtied segments. Pure + sync.
///
/// `sorted_events` MUST be ascending by `event_sort_key` and deduped.
/// `new_k_s` supplies fresh 32-byte keys for segments sealed this call
/// (production: OS entropy; tests: a deterministic counter).
pub fn plan_segments(
    community_id: SpaceId,
    sorted_events: &[SignedMembershipEvent],
    prior: &SegmentIndex,
    new_k_s: &mut dyn FnMut() -> [u8; 32],
) -> Result<SegmentPlan, SegmentError> {
    let mut refs: Vec<SegmentRef> = Vec::new();
    let mut sealed: Vec<SealedEntry> = Vec::new();
    let mut newly_sealed: Vec<(SegmentRef, Vec<u8>)> = Vec::new();

    // 1. Walk prior sealed intervals in order. For each, greedily collect the
    //    run of `sorted_events` with key <= this interval's `hi`. Because
    //    processing is ascending and disjoint, an event with key <= hi_k joins
    //    the earliest interval whose hi it does not exceed — a backdated event
    //    lands in exactly one interval and re-seals only it.
    let mut i = 0usize;
    for entry in &prior.sealed {
        let hi_key = boundary_key(&entry.hi);
        let run_start = i;
        while i < sorted_events.len() && event_key(&sorted_events[i]) <= hi_key {
            i += 1;
        }
        let run = &sorted_events[run_start..i];
        // Append-only log ⇒ a previously-sealed interval never loses events;
        // an empty run would mean the log shrank, which cannot happen. Guard
        // anyway: an empty run is dropped (its events vanished from input).
        if run.is_empty() {
            continue;
        }
        let lo = EventBoundary::of(&run[0]);
        let hi = EventBoundary::of(&run[run.len() - 1]);
        let unchanged = run.len() as u32 == entry.count && lo == entry.lo && hi == entry.hi;
        if unchanged {
            // Reuse: identical bytes already in CAS; not re-`put`.
            let r = SegmentRef {
                segment_cid: entry.segment_cid,
                lo: entry.lo.clone(),
                hi: entry.hi.clone(),
                count: entry.count,
                k_s: entry.k_s,
            };
            refs.push(r);
            sealed.push(entry.clone());
        } else {
            // Re-seal this one interval (a backdated/gap event landed inside).
            // Keep the same K_s (per-publisher determinism); recompute cid.
            let (cid, ct) = seal_segment(community_id, run, &entry.k_s)?;
            let r = SegmentRef {
                segment_cid: cid,
                lo: lo.clone(),
                hi: hi.clone(),
                count: run.len() as u32,
                k_s: entry.k_s,
            };
            sealed.push(SealedEntry {
                lo,
                hi,
                count: run.len() as u32,
                k_s: entry.k_s,
                segment_cid: cid,
            });
            newly_sealed.push((r.clone(), ct));
            refs.push(r);
        }
    }

    // 2. The suffix (key > last sealed hi) is the tail candidate. Seal leading
    //    chunks while it exceeds a threshold; the remainder is the live tail.
    let tail_candidate = &sorted_events[i..];
    let sizes: Vec<usize> = tail_candidate.iter().map(event_bytes).collect();
    let mut j = 0usize;
    loop {
        let remaining_len = tail_candidate.len() - j;
        let remaining_bytes: usize = sizes[j..].iter().sum();
        let over_events = remaining_len >= SEGMENT_SEAL_EVENTS;
        let over_bytes = remaining_bytes >= SEGMENT_SEAL_BYTES;
        if !over_events && !over_bytes {
            break;
        }
        // Cut a leading chunk: at most SEGMENT_SEAL_EVENTS, and stop early once
        // cumulative bytes reach SEGMENT_SEAL_BYTES (at least one event).
        let mut cut = 0usize;
        let mut acc = 0usize;
        while cut < remaining_len && cut < SEGMENT_SEAL_EVENTS {
            acc += sizes[j + cut];
            cut += 1;
            if acc >= SEGMENT_SEAL_BYTES {
                break;
            }
        }
        let chunk = &tail_candidate[j..j + cut];
        let k_s = new_k_s();
        let (cid, ct) = seal_segment(community_id, chunk, &k_s)?;
        let lo = EventBoundary::of(&chunk[0]);
        let hi = EventBoundary::of(&chunk[chunk.len() - 1]);
        let r = SegmentRef {
            segment_cid: cid,
            lo: lo.clone(),
            hi: hi.clone(),
            count: cut as u32,
            k_s,
        };
        sealed.push(SealedEntry {
            lo,
            hi,
            count: cut as u32,
            k_s,
            segment_cid: cid,
        });
        newly_sealed.push((r.clone(), ct));
        refs.push(r);
        j += cut;
    }

    let tail = tail_candidate[j..].to_vec();
    Ok(SegmentPlan {
        refs,
        tail,
        index: SegmentIndex {
            version: SEGMENT_INDEX_V1,
            sealed,
        },
        newly_sealed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_membership::MembershipEventKind;
    use crate::owner_state_types::{Hlc, OwnerAddr};

    fn ci() -> SpaceId {
        SpaceId([9u8; 16])
    }

    /// Minimal well-formed (not cryptographically valid) Leave event. The
    /// segment layer never verifies signatures, so a dummy sig is fine.
    fn ev(wall_ms: u64, logical: u32, dev: &str, id_byte: u8) -> SignedMembershipEvent {
        let mut id = [0u8; 16];
        id[0] = id_byte;
        SignedMembershipEvent {
            id,
            community_id: ci(),
            kind: MembershipEventKind::Leave,
            actor: OwnerAddr([id_byte; 16]),
            at: Hlc {
                wall_ms,
                logical,
                device_id: dev.to_string(),
            },
            sig: [0u8; 64],
            countersig: None,
            enrollment: None,
            signer_certs: Vec::new(),
        }
    }

    fn sorted(mut v: Vec<SignedMembershipEvent>) -> Vec<SignedMembershipEvent> {
        v.sort_by(|a, b| {
            crate::community_membership::event_sort_key(a)
                .cmp(&crate::community_membership::event_sort_key(b))
        });
        v
    }

    fn counter() -> impl FnMut() -> [u8; 32] {
        let mut n = 0u8;
        move || {
            n += 1;
            [n; 32]
        }
    }

    #[test]
    fn roundtrip_segment() {
        let evs = vec![ev(1, 0, "d", 1), ev(2, 0, "d", 2)];
        let (_cid, ct) = seal_segment(ci(), &evs, &[7u8; 32]).unwrap();
        let back = open_segment(&[7u8; 32], ci(), &ct).unwrap();
        assert_eq!(back, evs);
        // Wrong community_id → Misrouted.
        let err = open_segment(&[7u8; 32], SpaceId([1u8; 16]), &ct).unwrap_err();
        assert!(matches!(err, SegmentError::Misrouted { .. }));
    }

    #[test]
    fn roundtrip_manifest() {
        let key = EpochKey::new([3u8; 32]);
        let manifest = ManifestCleartext {
            version: MANIFEST_CLEARTEXT_V1,
            community_id: ci(),
            segments: Vec::new(),
            tail: vec![ev(1, 0, "d", 1)],
        };
        let (_cid, ct) = seal_manifest(&key, &manifest).unwrap();
        let back = open_manifest(&key, ci(), &ct).unwrap();
        assert_eq!(back, manifest);
        let err = open_manifest(&key, SpaceId([1u8; 16]), &ct).unwrap_err();
        assert!(matches!(err, SegmentError::Misrouted { .. }));
    }

    #[test]
    fn plan_empty_log_has_no_segments_only_tail() {
        let evs = sorted(vec![ev(1, 0, "d", 1), ev(2, 0, "d", 2), ev(3, 0, "d", 3)]);
        let plan = plan_segments(ci(), &evs, &SegmentIndex::default(), &mut counter()).unwrap();
        assert!(plan.refs.is_empty());
        assert_eq!(plan.tail, evs);
        assert!(plan.newly_sealed.is_empty());
    }

    #[test]
    fn plan_seals_at_event_threshold() {
        let mut v = Vec::new();
        for k in 0..(SEGMENT_SEAL_EVENTS as u64 + 5) {
            // distinct wall_ms keeps them ordered and distinct
            v.push(ev(k + 1, 0, "d", (k % 251) as u8));
        }
        let evs = sorted(v);
        let plan = plan_segments(ci(), &evs, &SegmentIndex::default(), &mut counter()).unwrap();
        assert_eq!(plan.refs.len(), 1);
        assert_eq!(plan.refs[0].count as usize, SEGMENT_SEAL_EVENTS);
        assert_eq!(plan.tail.len(), 5);
        assert_eq!(plan.newly_sealed.len(), 1);
    }

    #[test]
    fn plan_reuses_prior_segments_cid_stable() {
        let mut v = Vec::new();
        for k in 0..(SEGMENT_SEAL_EVENTS as u64 + 3) {
            v.push(ev(k + 1, 0, "d", (k % 251) as u8));
        }
        let evs = sorted(v.clone());
        let plan1 = plan_segments(ci(), &evs, &SegmentIndex::default(), &mut counter()).unwrap();
        let seg0_cid = plan1.refs[0].segment_cid;

        // Append 3 new (newer) tail events; republish against plan1's index.
        let mut v2 = v;
        for k in 0..3u64 {
            v2.push(ev(10_000 + k, 0, "d", (200 + k) as u8));
        }
        let evs2 = sorted(v2);
        let plan2 = plan_segments(ci(), &evs2, &plan1.index, &mut counter()).unwrap();
        assert_eq!(
            plan2.refs[0].segment_cid, seg0_cid,
            "prior segment cid must be stable"
        );
        assert!(
            plan2
                .newly_sealed
                .iter()
                .all(|(r, _)| r.segment_cid != seg0_cid),
            "reused segment must not be re-put"
        );
    }

    #[test]
    fn plan_backdated_event_reseals_one_segment_no_cascade() {
        // Two full sealed segments + a little tail.
        let mut v = Vec::new();
        for k in 0..(2 * SEGMENT_SEAL_EVENTS as u64 + 4) {
            v.push(ev((k + 1) * 10, 0, "d", (k % 251) as u8));
        }
        let evs = sorted(v.clone());
        let plan1 = plan_segments(ci(), &evs, &SegmentIndex::default(), &mut counter()).unwrap();
        assert!(plan1.refs.len() >= 2, "expected >=2 sealed segments");
        let seg0_cid = plan1.refs[0].segment_cid;
        let seg1_cid = plan1.refs[1].segment_cid;

        // Inject a backdated event whose key falls INSIDE segment 0's [lo,hi].
        let lo_wall = plan1.refs[0].lo.wall_ms;
        let hi_wall = plan1.refs[0].hi.wall_ms;
        let mid = lo_wall + (hi_wall - lo_wall) / 2 + 1; // strictly inside (odd offsets)
        let mut v2 = v;
        v2.push(ev(mid, 7, "d", 249)); // logical=7 to avoid colliding an existing key
        let evs2 = sorted(v2);
        let plan2 = plan_segments(ci(), &evs2, &plan1.index, &mut counter()).unwrap();

        assert_ne!(
            plan2.refs[0].segment_cid, seg0_cid,
            "segment 0 must re-seal"
        );
        assert_eq!(
            plan2.refs[1].segment_cid, seg1_cid,
            "segment 1 must NOT change (no cascade)"
        );
        assert_eq!(plan2.newly_sealed.len(), 1, "only one segment re-put");
        assert_eq!(
            plan2.newly_sealed[0].0.segment_cid,
            plan2.refs[0].segment_cid
        );
    }

    #[test]
    fn plan_total_order_is_transport_only() {
        let mut v = Vec::new();
        for k in 0..(SEGMENT_SEAL_EVENTS as u64 + 7) {
            v.push(ev(k + 1, 0, "d", (k % 251) as u8));
        }
        let sorted_in = sorted(v.clone());
        let plan_sorted =
            plan_segments(ci(), &sorted_in, &SegmentIndex::default(), &mut counter()).unwrap();
        // Shuffle deterministically (reverse) — plan must sort internally? No:
        // plan_segments requires sorted input, so callers sort. Re-sorting a
        // reversed vec must reproduce identical output.
        let mut reversed = v;
        reversed.reverse();
        let resorted = sorted(reversed);
        let plan_resorted =
            plan_segments(ci(), &resorted, &SegmentIndex::default(), &mut counter()).unwrap();
        assert_eq!(plan_sorted.refs, plan_resorted.refs);
        assert_eq!(plan_sorted.tail, plan_resorted.tail);
    }
}
