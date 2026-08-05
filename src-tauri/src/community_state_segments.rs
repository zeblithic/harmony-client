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

/// Segment cleartext format version. `pub` so the receive path can reject an
/// unsupported version and the wire-format fixtures can pin the right constant.
pub const SEGMENT_CLEARTEXT_V1: u16 = 1;
/// Manifest cleartext format version. `pub` so the publish path (Task 3)
/// stamps new manifests with it.
pub const MANIFEST_CLEARTEXT_V1: u16 = 1;
/// Sidecar segment-index format version. `pub` so the wire-format fixtures can
/// pin the right constant.
pub const SEGMENT_INDEX_V1: u16 = 1;

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
    /// A `segment`/`manifest` cleartext decoded cleanly but declared a
    /// `version` this build does not understand. Distinct from `CborDecode`
    /// (the bytes parsed fine — it is a forward-compat rejection, not a
    /// malformed blob) so the receive path can report it as an unsupported
    /// format rather than a decode fault.
    #[error("unsupported {kind} version {found}")]
    UnsupportedVersion { kind: &'static str, found: u16 },
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
    if cleartext.version != SEGMENT_CLEARTEXT_V1 {
        return Err(SegmentError::UnsupportedVersion {
            kind: "segment",
            found: cleartext.version,
        });
    }
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
    if manifest.version != MANIFEST_CLEARTEXT_V1 {
        return Err(SegmentError::UnsupportedVersion {
            kind: "manifest",
            found: manifest.version,
        });
    }
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

/// Greedily seal threshold-bounded segments from the front of `run` (whose
/// per-event byte sizes are `sizes`, in the same order). Each sealed segment's
/// `SegmentRef` is pushed to `refs`, its `SealedEntry` to `sealed`, and its
/// `(ref, ciphertext)` to `newly_sealed`.
///
/// `key_for(chunk_index)` supplies the `K_s` for the Nth chunk cut in THIS
/// call: the dirty-reseal path reuses the prior interval's key for chunk 0 and
/// fresh keys for any split-off remainder; the tail path uses a fresh key for
/// every chunk.
///
/// Returns the index into `run` where the unsealed remainder begins:
/// - `seal_remainder == false` (tail): stops once the remaining run is below
///   BOTH thresholds, leaving that sub-threshold remainder as the live tail.
/// - `seal_remainder == true` (dirty re-seal): seals every event — the final,
///   possibly sub-threshold, chunk is sealed too — so the return is `run.len()`.
///
/// Either way every emitted segment respects `SEGMENT_SEAL_EVENTS` /
/// `SEGMENT_SEAL_BYTES`, so no single segment can approach `MAX_PAYLOAD_SIZE`.
/// The bound holds uniformly for first-seal AND backdated re-seal: an interval
/// that grew past a threshold via backdated events is split into bounded
/// replacement segments rather than re-sealed as one oversized blob (ZEB-814 —
/// otherwise the payload-cap cliff this module removes could re-emerge).
///
/// `remaining_bytes` is maintained incrementally (subtracting each sealed
/// chunk), so the whole partition is O(run) — not the O(run²) a per-iteration
/// suffix sum would cost on a large first-seal / sidecar-loss republish.
#[allow(clippy::too_many_arguments)]
fn seal_chunks(
    community_id: SpaceId,
    run: &[SignedMembershipEvent],
    sizes: &[usize],
    seal_remainder: bool,
    mut key_for: impl FnMut(usize) -> [u8; 32],
    refs: &mut Vec<SegmentRef>,
    sealed: &mut Vec<SealedEntry>,
    newly_sealed: &mut Vec<(SegmentRef, Vec<u8>)>,
) -> Result<usize, SegmentError> {
    let n = run.len();
    let mut remaining_bytes: usize = sizes.iter().sum();
    let mut j = 0usize;
    let mut chunk_idx = 0usize;
    loop {
        let remaining_len = n - j;
        if remaining_len == 0 {
            break;
        }
        let over = remaining_len >= SEGMENT_SEAL_EVENTS || remaining_bytes >= SEGMENT_SEAL_BYTES;
        if !over && !seal_remainder {
            break;
        }
        // Cut a leading chunk: at most SEGMENT_SEAL_EVENTS events, stopping
        // early once cumulative bytes reach SEGMENT_SEAL_BYTES (always >=1
        // event). When !over (a seal_remainder final chunk) this takes the
        // whole sub-threshold remainder.
        let mut cut = 0usize;
        let mut acc = 0usize;
        while cut < remaining_len && cut < SEGMENT_SEAL_EVENTS {
            acc += sizes[j + cut];
            cut += 1;
            if acc >= SEGMENT_SEAL_BYTES {
                break;
            }
        }
        let chunk = &run[j..j + cut];
        let k_s = key_for(chunk_idx);
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
        remaining_bytes -= acc;
        chunk_idx += 1;
    }
    Ok(j)
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
            // Re-seal this interval (a backdated/gap event landed inside). It
            // MUST still respect the seal thresholds: an interval that grew
            // past SEGMENT_SEAL_* is split into bounded replacement segments so
            // no single segment can approach the payload cap. The first
            // replacement reuses the prior `K_s` (per-publisher determinism for
            // the common single-segment case); any split-off remainder gets
            // fresh keys. Only this interval's region changes — later sealed
            // intervals keep their CIDs (no cascade), because `i` has already
            // advanced past exactly this interval's events. (`lo`/`hi` were
            // already consumed by the `unchanged` check; `seal_chunks`
            // recomputes boundaries per replacement chunk.)
            let sizes: Vec<usize> = run.iter().map(event_bytes).collect();
            let entry_k_s = entry.k_s;
            seal_chunks(
                community_id,
                run,
                &sizes,
                true, // sealed history — the whole run stays sealed, never tail
                |idx| if idx == 0 { entry_k_s } else { new_k_s() },
                &mut refs,
                &mut sealed,
                &mut newly_sealed,
            )?;
        }
    }

    // 2. The suffix (key > last sealed hi) is the tail candidate. Seal leading
    //    threshold-bounded chunks (each a fresh `K_s`); the sub-threshold
    //    remainder is the live tail.
    let tail_candidate = &sorted_events[i..];
    let sizes: Vec<usize> = tail_candidate.iter().map(event_bytes).collect();
    let j = seal_chunks(
        community_id,
        tail_candidate,
        &sizes,
        false, // leave the sub-threshold remainder as the live tail
        |_| new_k_s(),
        &mut refs,
        &mut sealed,
        &mut newly_sealed,
    )?;
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
    fn open_segment_rejects_unsupported_version() {
        // A future-versioned segment cleartext must be REJECTED as an
        // unsupported version, not misparsed or surfaced as a decode error.
        let k_s = [5u8; 32];
        let cleartext = SegmentCleartext {
            version: SEGMENT_CLEARTEXT_V1 + 1,
            community_id: ci(),
            events: vec![ev(1, 0, "d", 1)],
        };
        let bytes = canonical_cbor_encode(&cleartext).unwrap();
        let ct = crate::community_state_sync::encrypt_blob(&EpochKey::new(k_s), &bytes).unwrap();
        let err = open_segment(&k_s, ci(), &ct).unwrap_err();
        assert!(matches!(
            err,
            SegmentError::UnsupportedVersion { kind: "segment", found }
                if found == SEGMENT_CLEARTEXT_V1 + 1
        ));
    }

    #[test]
    fn open_manifest_rejects_unsupported_version() {
        let key = EpochKey::new([6u8; 32]);
        let manifest = ManifestCleartext {
            version: MANIFEST_CLEARTEXT_V1 + 1,
            community_id: ci(),
            segments: Vec::new(),
            tail: Vec::new(),
        };
        let bytes = canonical_cbor_encode(&manifest).unwrap();
        let ct = crate::community_state_sync::encrypt_blob(&key, &bytes).unwrap();
        let err = open_manifest(&key, ci(), &ct).unwrap_err();
        assert!(matches!(
            err,
            SegmentError::UnsupportedVersion { kind: "manifest", found }
                if found == MANIFEST_CLEARTEXT_V1 + 1
        ));
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
    fn plan_backdated_event_reseals_affected_interval_no_cascade() {
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

        // Segment 0's region re-seals (its old CID is gone from the manifest).
        assert!(
            plan2.refs.iter().all(|r| r.segment_cid != seg0_cid),
            "segment 0 must re-seal"
        );
        // Segment 1 (a LATER interval) is untouched — present, same CID, and NOT
        // re-`put` to CAS. This is the cascade-free property: a backdated event
        // re-seals only its own interval's region, never the segments after it.
        assert!(
            plan2.refs.iter().any(|r| r.segment_cid == seg1_cid),
            "segment 1 must still be referenced"
        );
        assert!(
            plan2
                .newly_sealed
                .iter()
                .all(|(r, _)| r.segment_cid != seg1_cid),
            "segment 1 must NOT be re-put (no cascade)"
        );
        // Every segment — including the re-sealed region — respects the seal
        // threshold, so none can approach the payload cap.
        assert!(
            plan2
                .refs
                .iter()
                .all(|r| r.count as usize <= SEGMENT_SEAL_EVENTS),
            "no segment may exceed the event seal threshold"
        );
    }

    #[test]
    fn plan_dirty_reseal_stays_under_seal_threshold() {
        // One sealed segment, then a BURST of events backdated into its
        // [lo,hi] — enough that a single re-sealed segment would blow past the
        // threshold. The dirty path must split it into bounded segments (CR:
        // otherwise the payload-cap cliff re-emerges), and reconstruction must
        // still recover every event.
        let mut v = Vec::new();
        for k in 0..(SEGMENT_SEAL_EVENTS as u64 + 2) {
            v.push(ev((k + 1) * 1000, 0, "d", (k % 251) as u8));
        }
        let evs = sorted(v.clone());
        let plan1 = plan_segments(ci(), &evs, &SegmentIndex::default(), &mut counter()).unwrap();
        assert_eq!(plan1.refs.len(), 1, "one sealed segment to start");

        // Backdate ~2 segments' worth of events INTO segment 0's [lo,hi] using
        // distinct logical clocks so keys stay unique and land inside.
        let lo_wall = plan1.refs[0].lo.wall_ms;
        let hi_wall = plan1.refs[0].hi.wall_ms;
        let mut v2 = v.clone();
        for k in 0..(2 * SEGMENT_SEAL_EVENTS as u64) {
            let wall = lo_wall + 1 + (k % (hi_wall - lo_wall - 1).max(1));
            v2.push(ev(wall, 1000 + k as u32, "d", (k % 251) as u8));
        }
        let evs2 = sorted(v2);
        let plan2 = plan_segments(ci(), &evs2, &plan1.index, &mut counter()).unwrap();

        // The oversized interval was split — no single segment exceeds the cap.
        assert!(
            plan2
                .refs
                .iter()
                .all(|r| r.count as usize <= SEGMENT_SEAL_EVENTS),
            "dirty re-seal must split oversized intervals to stay under the threshold"
        );
        assert!(
            plan2.refs.len() >= 3,
            "the grown interval must span multiple bounded segments"
        );

        // Reconstruction parity: opening every segment + the tail recovers the
        // exact backdated event set.
        let mut got: Vec<SignedMembershipEvent> = Vec::new();
        for (r, ct) in &plan2.newly_sealed {
            got.extend(open_segment(&r.k_s, ci(), ct).unwrap());
        }
        // Reused (not re-put) segments contribute nothing new here because the
        // whole grown interval re-sealed; add the tail and compare to input.
        got.extend(plan2.tail.clone());
        got.sort_by(|a, b| {
            crate::community_membership::event_sort_key(a)
                .cmp(&crate::community_membership::event_sort_key(b))
        });
        assert_eq!(got, evs2, "split segments + tail reconstruct every event");
    }

    #[test]
    fn plan_caller_side_resort_is_deterministic() {
        // `plan_segments` requires ascending, deduped input (see its doc
        // comment) — callers sort with `event_sort_key`. This pins the
        // CALLER-SIDE contract: sorting a differently-ordered copy of the same
        // event set back into `event_sort_key` order reproduces byte-identical
        // segmentation. (It does not assert the planner sorts internally — it
        // does not; feeding it unsorted input is a caller bug.)
        let mut v = Vec::new();
        for k in 0..(SEGMENT_SEAL_EVENTS as u64 + 7) {
            v.push(ev(k + 1, 0, "d", (k % 251) as u8));
        }
        let sorted_in = sorted(v.clone());
        let plan_sorted =
            plan_segments(ci(), &sorted_in, &SegmentIndex::default(), &mut counter()).unwrap();
        let mut reversed = v;
        reversed.reverse();
        let resorted = sorted(reversed);
        let plan_resorted =
            plan_segments(ci(), &resorted, &SegmentIndex::default(), &mut counter()).unwrap();
        assert_eq!(plan_sorted.refs, plan_resorted.refs);
        assert_eq!(plan_sorted.tail, plan_resorted.tail);
    }

    #[test]
    fn rotation_reseals_manifest_not_segments() {
        // Enough events to seal at least one segment.
        let mut v = Vec::new();
        for k in 0..(SEGMENT_SEAL_EVENTS as u64 + 3) {
            v.push(ev(k + 1, 0, "d", (k % 251) as u8));
        }
        let evs = sorted(v);
        let plan = plan_segments(ci(), &evs, &SegmentIndex::default(), &mut counter()).unwrap();
        assert!(!plan.refs.is_empty());
        let seg_cids: Vec<_> = plan.refs.iter().map(|r| r.segment_cid).collect();

        let manifest = ManifestCleartext {
            version: MANIFEST_CLEARTEXT_V1,
            community_id: ci(),
            segments: plan.refs,
            tail: plan.tail,
        };
        // Seal the SAME manifest under two different epoch keys (a rotation).
        let key_n = EpochKey::new([10u8; 32]);
        let key_n1 = EpochKey::new([20u8; 32]);
        let (root_n, ct_n) = seal_manifest(&key_n, &manifest).unwrap();
        let (root_n1, ct_n1) = seal_manifest(&key_n1, &manifest).unwrap();

        // Rotation re-encrypts the manifest → different root CID + bytes …
        assert_ne!(root_n, root_n1, "rotation must re-key the manifest");
        assert_ne!(ct_n, ct_n1);
        // … but the segments it references are epoch-independent: a member with
        // EITHER epoch key unwraps the SAME segment set (identical CIDs). This
        // is what makes rotation O(manifest), not O(history).
        let m_n = open_manifest(&key_n, ci(), &ct_n).unwrap();
        let m_n1 = open_manifest(&key_n1, ci(), &ct_n1).unwrap();
        let cids_n: Vec<_> = m_n.segments.iter().map(|r| r.segment_cid).collect();
        let cids_n1: Vec<_> = m_n1.segments.iter().map(|r| r.segment_cid).collect();
        assert_eq!(cids_n, seg_cids);
        assert_eq!(cids_n1, seg_cids);
    }

    #[test]
    fn manifest_receive_parity_reconstructs_all_events() {
        // >1 sealed segment + a tail.
        let mut v = Vec::new();
        for k in 0..(2 * SEGMENT_SEAL_EVENTS as u64 + 9) {
            v.push(ev(k + 1, 0, "d", (k % 251) as u8));
        }
        let evs = sorted(v);
        let plan = plan_segments(ci(), &evs, &SegmentIndex::default(), &mut counter()).unwrap();
        // Stand-in CAS: (segment_cid, ciphertext) for every sealed segment.
        // `ContentId` isn't `Ord`, so use a Vec + linear lookup (PartialEq).
        let cas: Vec<(harmony_content::cid::ContentId, Vec<u8>)> = plan
            .newly_sealed
            .iter()
            .map(|(r, ct)| (r.segment_cid, ct.clone()))
            .collect();
        let manifest = ManifestCleartext {
            version: MANIFEST_CLEARTEXT_V1,
            community_id: ci(),
            segments: plan.refs,
            tail: plan.tail,
        };

        // Simulate the receive path: open each segment (by its K_s) from CAS,
        // append the tail, sort by event_sort_key — must equal the input log.
        let mut got: Vec<SignedMembershipEvent> = Vec::new();
        for r in &manifest.segments {
            let ct = &cas
                .iter()
                .find(|(cid, _)| *cid == r.segment_cid)
                .expect("segment present in CAS")
                .1;
            got.extend(open_segment(&r.k_s, ci(), ct).unwrap());
        }
        got.extend(manifest.tail);
        got.sort_by(|a, b| {
            crate::community_membership::event_sort_key(a)
                .cmp(&crate::community_membership::event_sort_key(b))
        });
        assert_eq!(
            got, evs,
            "manifest+segments reconstruct the exact event set"
        );
    }
}
