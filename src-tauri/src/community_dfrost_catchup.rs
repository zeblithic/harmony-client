//! ZEB-1030: D-FROST committee catch-up — wire types + pure responder
//! selection.
//!
//! A late-joining or long-offline peer cannot reconstruct committee
//! state from live `dk`/`vb` gossip alone (Zenoh pub/sub carries no
//! anti-entropy — see the module doc on `community_dfrost_log_engine`).
//! This module defines the request/response wire shapes for a one-shot
//! catch-up exchange plus the pure (no I/O, no crypto verification)
//! logic a responder uses to decide what to serve. Transport wiring
//! (Zenoh queryable/GET) and requester-side adoption land in later
//! ZEB-1030 tasks; this task only ships the types, the codec, and
//! `select_catchup`/`group_frames`.
//!
//! Same-length-keys invariant: every top-level CBOR map in this module
//! uses 2-character keys, matching `SignedCommitteeEvent`'s envelope
//! convention (`community_dfrost_types.rs`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::community_dfrost_log::DfrostLog;
use crate::community_dfrost_types::{DfrostEventKind, DkgCompletePayload, SignedCommitteeEvent};
use crate::owner_state_types::OwnerAddr;

/// Wire version for the catch-up request/frame codec. Bumped on any
/// breaking change to [`CatchupRequest`] or [`CatchupFrame`].
pub const CATCHUP_VERSION: u8 = 1;

/// Inbound size cap for one encoded [`CatchupRequest`] or [`CatchupFrame`].
/// A single frame carries at most one `SignedCommitteeEvent` — small CBOR,
/// well under this bound for any realistic committee (mirrors
/// `MAX_DFROST_PAYLOAD_BYTES` in `event_loop.rs`). Checked before decode
/// to prevent peer-controlled allocation.
pub const MAX_DFROST_CATCHUP_FRAME_BYTES: usize = 64 * 1024;

/// Per-round buffer ceiling for a requester draining a responder's reply
/// stream (mirrors `MAX_RBSR_ROUND_BYTES` in `event_loop.rs`). Not
/// enforced by this module directly — the transport layer (later tasks)
/// charges each received frame against it.
pub const MAX_DFROST_CATCHUP_ROUND_BYTES: usize = 16 * 1024 * 1024;

/// Default cap on the number of `vb` (VRF beacon) events served in a
/// single catch-up round. Bounds responder work and reply size even
/// against a requester with a very stale (or absent) watermark.
pub const MAX_CATCHUP_BEACONS_PER_ROUND: usize = 64;

/// ZEB-1030 final-review I4: cap on the number of responder groups
/// (`group_frames` output) a requester will process in one
/// `catchup_ingest` round. A responder reply is otherwise bounded only
/// by `MAX_DFROST_CATCHUP_ROUND_BYTES` (16 MiB), which admits on the
/// order of 10^5 tiny single-status-single-`dk` groups — each of which
/// pays an `Ed25519::verify_strict` per non-status frame, and on the
/// joiner path a membership-resolver `snapshot_at` per `dk` event. 16
/// is generous: a legitimate round only ever needs a handful of
/// independent responders to make its case.
pub const MAX_CATCHUP_RESPONDER_GROUPS: usize = 16;

/// ZEB-1030 PR#778 round-1: margin subtracted from
/// [`MAX_DFROST_CATCHUP_FRAME_BYTES`] when capping PLAINTEXT frame/request
/// bytes at encode time. `MAX_DFROST_CATCHUP_FRAME_BYTES` is the cap the
/// RECEIVER enforces against the raw bytes it reads off the wire —
/// `dfrost_catchup_open_plaintext` in `event_loop.rs`, checked BEFORE
/// decrypt — and those raw bytes are the SEALED envelope: this module's
/// plaintext plus an AEAD nonce, an authentication tag, and the
/// `EncryptedEnvelope`'s own CBOR framing. A plaintext frame or request
/// encoded right up to the raw cap would pass `encode_frame`/
/// `encode_request` here but exceed the raw cap once sealed, so it would
/// be silently dropped by the receiver — never even reaching decrypt. 256
/// bytes is generous headroom over the actual sealing overhead (a 12-byte
/// nonce, a 16-byte AEAD tag, and a handful of CBOR map/length bytes).
pub const DFROST_CATCHUP_SEAL_MARGIN_BYTES: usize = 256;

/// Envelope HLC of the newest `vb` event the requester holds. 2-char keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeaconWatermark {
    #[serde(rename = "wm")]
    pub wall_ms: u64,
    #[serde(rename = "lg")]
    pub logical: u32,
    #[serde(rename = "dv")]
    pub device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatchupRequest {
    #[serde(rename = "vr")]
    pub version: u8,
    /// Requester's committee epoch (0 + active=false ⇒ no state).
    #[serde(rename = "ep")]
    pub epoch: u64,
    #[serde(rename = "ac")]
    pub active: bool,
    #[serde(rename = "bw", skip_serializing_if = "Option::is_none", default)]
    pub beacon_watermark: Option<BeaconWatermark>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatchupStatus {
    #[serde(rename = "ep")]
    pub epoch: u64,
    #[serde(rename = "ac")]
    pub active: bool,
}

/// Externally-tagged enum — encodes as a 1-entry map {"st"|"dk"|"vb": ...}.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CatchupBody {
    #[serde(rename = "st")]
    Status(CatchupStatus),
    /// Verbatim ciborium-encoded `SignedCommitteeEvent` (kind `dk`).
    #[serde(rename = "dk")]
    DkEvidence(#[serde(with = "serde_bytes")] Vec<u8>),
    /// Verbatim ciborium-encoded `SignedCommitteeEvent` (kind `vb`).
    #[serde(rename = "vb")]
    Beacon(#[serde(with = "serde_bytes")] Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatchupFrame {
    #[serde(rename = "vr")]
    pub version: u8,
    /// Per-round random responder id — frames group by this on the
    /// requester (Zenoh reply order/attribution is not load-bearing).
    #[serde(rename = "ri", with = "serde_bytes")]
    pub responder_id: [u8; 8],
    #[serde(rename = "bd")]
    pub body: CatchupBody,
}

/// Encode a [`CatchupRequest`]. Size cap is checked AFTER encoding, at the
/// [`DFROST_CATCHUP_SEAL_MARGIN_BYTES`]-adjusted bound (mirrors
/// `encode_frame`) — `BeaconWatermark.device_id` is operator-controlled
/// `Hlc` string data, not a fixed-size field, so a request's encoded size
/// is not actually bounded by construction the way this comment used to
/// claim.
pub fn encode_request(req: &CatchupRequest) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(req, &mut out)
        .map_err(|e| format!("dfrost catchup request encode: {e}"))?;
    let cap = MAX_DFROST_CATCHUP_FRAME_BYTES - DFROST_CATCHUP_SEAL_MARGIN_BYTES;
    if out.len() > cap {
        return Err(format!(
            "dfrost catchup request exceeds {cap}-byte cap \
             ({} bytes)",
            out.len()
        ));
    }
    Ok(out)
}

/// Decode + validate a [`CatchupRequest`]. Length gate first (before any
/// decode work), then version gate.
pub fn decode_request(bytes: &[u8]) -> Result<CatchupRequest, String> {
    if bytes.len() > MAX_DFROST_CATCHUP_FRAME_BYTES {
        return Err(format!(
            "dfrost catchup request exceeds {MAX_DFROST_CATCHUP_FRAME_BYTES}-byte cap \
             ({} bytes)",
            bytes.len()
        ));
    }
    let req: CatchupRequest = ciborium::de::from_reader(bytes)
        .map_err(|e| format!("dfrost catchup request decode: {e}"))?;
    if req.version != CATCHUP_VERSION {
        return Err(format!(
            "dfrost catchup request version {} unsupported (want {CATCHUP_VERSION})",
            req.version
        ));
    }
    Ok(req)
}

/// Encode a [`CatchupFrame`]. Size cap is checked AFTER encoding — the
/// frame's `dk`/`vb` body carries a verbatim event, so the cap can only
/// be evaluated on the encoded bytes. Enforced at the
/// [`DFROST_CATCHUP_SEAL_MARGIN_BYTES`]-adjusted bound, not the raw
/// [`MAX_DFROST_CATCHUP_FRAME_BYTES`] — see that constant's doc: sealing
/// this plaintext adds bytes the receiver's raw-bytes cap also has to
/// absorb.
pub fn encode_frame(frame: &CatchupFrame) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(frame, &mut out)
        .map_err(|e| format!("dfrost catchup frame encode: {e}"))?;
    let cap = MAX_DFROST_CATCHUP_FRAME_BYTES - DFROST_CATCHUP_SEAL_MARGIN_BYTES;
    if out.len() > cap {
        return Err(format!(
            "dfrost catchup frame exceeds {cap}-byte cap \
             ({} bytes)",
            out.len()
        ));
    }
    Ok(out)
}

/// Decode + validate a [`CatchupFrame`]. Length gate first, then version gate.
pub fn decode_frame(bytes: &[u8]) -> Result<CatchupFrame, String> {
    if bytes.len() > MAX_DFROST_CATCHUP_FRAME_BYTES {
        return Err(format!(
            "dfrost catchup frame exceeds {MAX_DFROST_CATCHUP_FRAME_BYTES}-byte cap \
             ({} bytes)",
            bytes.len()
        ));
    }
    let frame: CatchupFrame = ciborium::de::from_reader(bytes)
        .map_err(|e| format!("dfrost catchup frame decode: {e}"))?;
    if frame.version != CATCHUP_VERSION {
        return Err(format!(
            "dfrost catchup frame version {} unsupported (want {CATCHUP_VERSION})",
            frame.version
        ));
    }
    Ok(frame)
}

/// Max envelope HLC among retained `vb` events (for the request).
///
/// `log.events()` is already synthesized-id (HLC-major) ordered
/// (ZEB-753), so the LAST matching `vb` event encountered while
/// scanning in order carries the maximum `(wall_ms, logical, device_id)`
/// among the events considered — no separate max-tracking needed.
///
/// ZEB-1030 final-review C1: an event's envelope HLC is chosen by
/// whichever peer re-wraps and re-signs it, and `adopt_beacons` verifies
/// only the payload (Schnorr sig + VRF binding) — never the envelope HLC
/// or the signing actor's membership. A `vb` retained at an implausibly
/// future `wall_ms` (attacker-chosen, or an honest peer's badly-set
/// clock) must never become the watermark: `select_catchup`'s
/// `&hlc_key > watermark` test would then never be satisfied by any real
/// future beacon again, permanently starving this node's beacon
/// catch-up. Per house rule the skew gate lives at the VIEW, not the
/// store — the event stays retained (`log.events()`/`log` are
/// untouched); it is only skipped when computing this scalar high-water.
/// `now_wall_ms == 0` (receiver clock unreadable) disables the gate —
/// mirrors `community_voting_log_engine.rs`'s `receiver_now_ms`
/// convention; a bad LOCAL clock must never suppress a real watermark.
pub fn beacon_watermark_of(log: &DfrostLog, now_wall_ms: u64) -> Option<BeaconWatermark> {
    log.events()
        .filter(|e| e.kind == DfrostEventKind::VrfBeacon)
        .filter(|e| {
            now_wall_ms == 0
                || !crate::clock_trust::reject_future_logged(
                    e.hlc.wall_ms,
                    now_wall_ms,
                    crate::clock_trust::MAX_FORWARD_SKEW_MS,
                    "dfrost_catchup.beacon_watermark.envelope_hlc",
                )
        })
        .last()
        .map(|e| BeaconWatermark {
            wall_ms: e.hlc.wall_ms,
            logical: e.hlc.logical,
            device_id: e.hlc.device_id.clone(),
        })
}

pub struct CatchupSelection {
    pub status: CatchupStatus,
    /// Current epoch's `dk` events, one per distinct actor (newest per
    /// actor in synthesized-id order). May be sub-threshold — the
    /// requester decides adoptability.
    pub dk_events: Vec<SignedCommitteeEvent>,
    /// `vb` events with envelope HLC strictly above the watermark, plus
    /// (ZEB-1036) every member of a TIED beacon group regardless of
    /// watermark. OLDEST-first, capped at `max_beacons`.
    pub beacons: Vec<SignedCommitteeEvent>,
}

/// Pure responder selection. `None` ⇒ nothing to serve (inactive
/// responder, or requester already fully current) — transport answers
/// with silence.
///
/// ZEB-1035: `now_wall_ms` (responder's trusted wall clock; `0` = clock
/// unreadable ⇒ gate disabled) applies the same forward-skew rejection
/// the requester's `beacon_watermark_of` applies at its view: a
/// retained `vb` whose envelope HLC is implausibly future is never
/// SERVED. Without this, one future-stamped beacon in a responder's
/// log sorts above every requester's (correctly capped) watermark
/// forever — re-served and re-verified every round, and permanently
/// defeating the fully-current → `None` short-circuit below. The event
/// stays retained (view-not-store); it is only excluded from serving.
///
/// ZEB-1036 (tied beacons): concurrent threshold-sign quorums can each
/// produce a VALID beacon for one `message_hash` (partition-separated
/// signer subsets → different aggregate commitments R → different
/// `vrf_output`s; the ceremony id is already deterministic and does not
/// prevent this). The requester's watermark is a scalar high-water, so
/// a replica that retained only the LARGER-output sibling of such a tie
/// can never be served the smaller one it needs for min-wins
/// convergence — the hole sits below its watermark by construction. So:
/// every member of a tied group (≥ 2 distinct `vrf_output`s among this
/// responder's skew-admitted `vb` events for one `message_hash`) is
/// served REGARDLESS of watermark; `adopt_beacons` is idempotent and
/// min-wins, so the requester heals. Cost: while a responder retains a
/// tie, its tied events ride every response (bounded: ties require the
/// concurrent-quorum race, and re-adoption is a no-op) — with no ties
/// retained, selection and the fully-current → `None` short-circuit
/// behave exactly as before.
pub fn select_catchup(
    log: &DfrostLog,
    req: &CatchupRequest,
    max_beacons: usize,
    now_wall_ms: u64,
) -> Option<CatchupSelection> {
    use crate::community_dfrost_types::VrfBeaconPayload;

    // Rule 1: inactive responder has nothing to serve.
    if !log.committee_state.active {
        return None;
    }
    let current_epoch = log.committee_state.current_epoch;

    // Beacons: one scan skew-admits each `vb` event and decodes its
    // payload once (an undecodable payload keeps pre-ZEB-1036 handling:
    // servable above the watermark, never tie-eligible). `log.events()`
    // is already HLC-ordered, so selection below stays oldest-first.
    struct AdmittedBeacon<'a> {
        event: &'a SignedCommitteeEvent,
        /// Decoded `(message_hash, vrf_output)`; `None` ⇒ undecodable.
        hash_output: Option<([u8; 32], [u8; 32])>,
    }
    let mut admitted: Vec<AdmittedBeacon<'_>> = Vec::new();
    for ev in log.events() {
        if ev.kind != DfrostEventKind::VrfBeacon {
            continue;
        }
        // ZEB-1035: never serve a forward-skewed beacon — see the fn doc.
        if now_wall_ms != 0
            && crate::clock_trust::reject_future_logged(
                ev.hlc.wall_ms,
                now_wall_ms,
                crate::clock_trust::MAX_FORWARD_SKEW_MS,
                "dfrost_catchup.select.envelope_hlc",
            )
        {
            continue;
        }
        let hash_output = ciborium::de::from_reader::<VrfBeaconPayload, _>(&ev.payload[..])
            .ok()
            .map(|p| (p.message_hash, p.vrf_output));
        admitted.push(AdmittedBeacon {
            event: ev,
            hash_output,
        });
    }

    // ZEB-1036: message hashes with ≥ 2 distinct outputs — see fn doc.
    let mut outputs_by_hash: BTreeMap<[u8; 32], std::collections::BTreeSet<[u8; 32]>> =
        BTreeMap::new();
    for beacon in &admitted {
        if let Some((hash, output)) = beacon.hash_output {
            outputs_by_hash.entry(hash).or_default().insert(output);
        }
    }
    let tied_hashes: std::collections::BTreeSet<[u8; 32]> = outputs_by_hash
        .into_iter()
        .filter(|(_, outputs)| outputs.len() >= 2)
        .map(|(hash, _)| hash)
        .collect();

    let watermark = req
        .beacon_watermark
        .as_ref()
        .map(|w| (w.wall_ms, w.logical, w.device_id.clone()));
    // PR#780 round-1 (CodeAnt): tied members are the healing payload, so
    // they claim cap budget FIRST — a single interleaved pass could spend
    // the whole cap on above-watermark backlog and truncate a tie group
    // mid-pair, serving a requester only the sibling it already has. Two
    // index passes over the HLC-ordered `admitted` list, emitted in index
    // (= HLC) order, keep the served ordering identical to before. A tie
    // population larger than the cap itself still truncates — that would
    // take `max_beacons` distinct tied events at once.
    let mut selected: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    for (i, beacon) in admitted.iter().enumerate() {
        if selected.len() >= max_beacons {
            break;
        }
        if beacon
            .hash_output
            .is_some_and(|(hash, _)| tied_hashes.contains(&hash))
        {
            selected.insert(i);
        }
    }
    for (i, beacon) in admitted.iter().enumerate() {
        if selected.len() >= max_beacons {
            break;
        }
        if selected.contains(&i) {
            continue;
        }
        let ev = beacon.event;
        let hlc_key = (ev.hlc.wall_ms, ev.hlc.logical, ev.hlc.device_id.clone());
        let above_watermark = match &watermark {
            Some(wm) => &hlc_key > wm,
            None => true,
        };
        if above_watermark {
            selected.insert(i);
        }
    }
    let beacons: Vec<SignedCommitteeEvent> = selected
        .iter()
        .map(|&i| admitted[i].event.clone())
        .collect();

    // Rule 2: requester fully current (active at the current epoch, and
    // no beacon above its watermark — nor any tied group to heal) ⇒
    // nothing to serve.
    let requester_current = req.active && req.epoch == current_epoch;
    if requester_current && beacons.is_empty() {
        return None;
    }

    // dk_events: only when the requester is behind the current epoch
    // (inactive, or active at an older epoch). Collapse to newest per
    // actor — iterating in HLC order means a later `insert` for the same
    // actor key naturally overwrites the earlier one.
    let dk_events = if !req.active || req.epoch < current_epoch {
        let mut newest_per_actor: BTreeMap<OwnerAddr, SignedCommitteeEvent> = BTreeMap::new();
        for ev in log.events() {
            if ev.kind != DfrostEventKind::DkgComplete {
                continue;
            }
            let payload: DkgCompletePayload = match ciborium::de::from_reader(&ev.payload[..]) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if payload.epoch != current_epoch {
                continue;
            }
            newest_per_actor.insert(ev.actor, ev.clone());
        }
        newest_per_actor.into_values().collect()
    } else {
        Vec::new()
    };

    Some(CatchupSelection {
        status: CatchupStatus {
            epoch: current_epoch,
            active: true,
        },
        dk_events,
        beacons,
    })
}

/// Group frames by responder_id, DISCARDING any group without exactly
/// one Status frame. Order: groups in first-seen order.
///
/// ZEB-1030 PR#778 round-1: `cap` is enforced DURING insertion, not by
/// truncating the grouped result afterward — a flood of frames carrying
/// distinct, never-before-seen `responder_id`s would otherwise make the
/// per-frame `grouped.iter_mut().find(...)` linear scan itself
/// O(frames × distinct_rids) before any cap ever applied, since a new
/// entry was pushed onto `grouped` (and so lengthened the scan) for
/// every unseen id. Once `cap` distinct responder ids have been admitted,
/// a frame for a still-unseen id is dropped on sight (counted, not
/// pushed) — bounding both the scan and the final group count — while a
/// frame for an already-admitted id still appends normally, without
/// limit. Returns `(groups, dropped_frame_count)` for the caller's
/// truncation warning.
pub fn group_frames(
    frames: Vec<CatchupFrame>,
    cap: usize,
) -> (Vec<(CatchupStatus, Vec<CatchupFrame>)>, usize) {
    let mut grouped: Vec<([u8; 8], Vec<CatchupFrame>)> = Vec::new();
    let mut dropped = 0usize;
    for frame in frames {
        match grouped
            .iter()
            .position(|(rid, _)| *rid == frame.responder_id)
        {
            Some(idx) => grouped[idx].1.push(frame),
            None if grouped.len() < cap => grouped.push((frame.responder_id, vec![frame])),
            None => dropped += 1,
        }
    }
    let groups = grouped
        .into_iter()
        .filter_map(|(_, group)| {
            let mut status = None;
            let mut status_count = 0usize;
            for frame in &group {
                if let CatchupBody::Status(s) = &frame.body {
                    status = Some(*s);
                    status_count += 1;
                }
            }
            if status_count == 1 {
                status.map(|s| (s, group))
            } else {
                None
            }
        })
        .collect();
    (groups, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_dfrost_types::VrfBeaconPayload;
    use crate::owner_state_types::Hlc;

    fn test_hlc(wall_ms: u64, logical: u32, device_id: &str) -> Hlc {
        Hlc {
            wall_ms,
            logical,
            device_id: device_id.to_string(),
        }
    }

    fn test_owner(byte: u8) -> OwnerAddr {
        OwnerAddr([byte; 16])
    }

    fn test_dk_event(epoch: u64, actor: OwnerAddr, hlc: Hlc) -> SignedCommitteeEvent {
        let payload = DkgCompletePayload {
            ceremony_id: [0u8; 32],
            joint_verifying_key: [0u8; 32],
            verifying_shares: vec![],
            epoch,
            members: vec![],
            threshold: 2,
            max_signers: 3,
            space_id: None,
        };
        let mut payload_bytes = Vec::new();
        ciborium::ser::into_writer(&payload, &mut payload_bytes).unwrap();
        SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::DkgComplete,
            hlc,
            actor,
            payload: payload_bytes,
            sig: vec![0u8; 64],
        }
    }

    fn test_vb_event(hlc: Hlc) -> SignedCommitteeEvent {
        test_vb_event_with(hlc, [0u8; 32], [0u8; 32])
    }

    /// ZEB-1036: `vb` builder with explicit `message_hash`/`vrf_output`
    /// so tests can construct tied beacon groups.
    fn test_vb_event_with(
        hlc: Hlc,
        message_hash: [u8; 32],
        vrf_output: [u8; 32],
    ) -> SignedCommitteeEvent {
        let payload = VrfBeaconPayload {
            ceremony_id: [0u8; 32],
            message_hash,
            signature: vec![0u8; 64],
            vrf_output,
        };
        let mut payload_bytes = Vec::new();
        ciborium::ser::into_writer(&payload, &mut payload_bytes).unwrap();
        SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::VrfBeacon,
            hlc,
            actor: test_owner(0xAA),
            payload: payload_bytes,
            sig: vec![0u8; 64],
        }
    }

    /// Active committee log at `current_epoch`, no events seeded — the
    /// caller adds events via `insert_event_for_test`. Mirrors the
    /// restored-log public-state shape used by `committee_log_from_material`
    /// in `community_dfrost_log.rs`, but with arbitrary crypto material —
    /// `select_catchup` never verifies signatures or key material.
    fn test_active_log(current_epoch: u64) -> DfrostLog {
        let mut log = DfrostLog::new();
        log.committee_state.active = true;
        log.committee_state.current_epoch = current_epoch;
        log.committee_state.members = vec![test_owner(1), test_owner(2), test_owner(3)];
        log.committee_state.threshold = 2;
        log.committee_state.max_signers = 3;
        log.committee_state.joint_verifying_key = Some([0u8; 32]);
        log
    }

    fn assert_top_level_two_char_keys(bytes: &[u8]) {
        let val: ciborium::Value = ciborium::de::from_reader(bytes).unwrap();
        let map = val.as_map().expect("top-level value is a map");
        for (k, _) in map {
            let s = k.as_text().expect("key is text");
            assert_eq!(s.len(), 2, "key {s:?} violates 2-char invariant");
        }
    }

    /// Like `assert_top_level_two_char_keys`, but additionally checks the
    /// `CatchupBody`'s own (externally-tagged, 1-entry) map nested under
    /// the frame's `bd` key.
    fn assert_frame_two_char_keys(bytes: &[u8]) {
        let val: ciborium::Value = ciborium::de::from_reader(bytes).unwrap();
        let map = val.as_map().expect("frame top-level value is a map");
        for (k, v) in map {
            let key_str = k.as_text().expect("key is text");
            assert_eq!(
                key_str.len(),
                2,
                "key {key_str:?} violates 2-char invariant"
            );
            if key_str == "bd" {
                let body_map = v.as_map().expect("body value is a map");
                assert_eq!(
                    body_map.len(),
                    1,
                    "externally-tagged CatchupBody encodes to a 1-entry map"
                );
                for (bk, _) in body_map {
                    let bks = bk.as_text().expect("body key is text");
                    assert_eq!(bks.len(), 2, "body key {bks:?} violates 2-char invariant");
                }
            }
        }
    }

    #[test]
    fn catchup_request_and_frame_round_trip_zeb1030() {
        let req_no_bw = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 5,
            active: true,
            beacon_watermark: None,
        };
        let bytes = encode_request(&req_no_bw).unwrap();
        assert_eq!(decode_request(&bytes).unwrap(), req_no_bw);
        assert_top_level_two_char_keys(&bytes);

        let req_with_bw = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 5,
            active: true,
            beacon_watermark: Some(BeaconWatermark {
                wall_ms: 111,
                logical: 2,
                device_id: "dev".into(),
            }),
        };
        let bytes = encode_request(&req_with_bw).unwrap();
        assert_eq!(decode_request(&bytes).unwrap(), req_with_bw);
        assert_top_level_two_char_keys(&bytes);

        let status_frame = CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: [7u8; 8],
            body: CatchupBody::Status(CatchupStatus {
                epoch: 3,
                active: true,
            }),
        };
        let dk_frame = CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: [7u8; 8],
            body: CatchupBody::DkEvidence(vec![0xAA, 0xBB]),
        };
        let vb_frame = CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: [7u8; 8],
            body: CatchupBody::Beacon(vec![0xCC, 0xDD]),
        };
        for frame in [status_frame, dk_frame, vb_frame] {
            let fbytes = encode_frame(&frame).unwrap();
            assert_eq!(decode_frame(&fbytes).unwrap(), frame);
            assert_frame_two_char_keys(&fbytes);
        }
    }

    #[test]
    fn decode_rejects_bad_version_and_oversize_zeb1030() {
        let mut req = CatchupRequest {
            version: 0,
            epoch: 1,
            active: true,
            beacon_watermark: None,
        };
        let bytes = encode_request(&req).unwrap();
        assert!(decode_request(&bytes).is_err(), "version 0 rejected");

        req.version = 2;
        let bytes = encode_request(&req).unwrap();
        assert!(decode_request(&bytes).is_err(), "version 2 rejected");

        let mut frame = CatchupFrame {
            version: 0,
            responder_id: [1u8; 8],
            body: CatchupBody::Status(CatchupStatus {
                epoch: 1,
                active: true,
            }),
        };
        let fbytes = encode_frame(&frame).unwrap();
        assert!(decode_frame(&fbytes).is_err(), "frame version 0 rejected");

        frame.version = 2;
        let fbytes = encode_frame(&frame).unwrap();
        assert!(decode_frame(&fbytes).is_err(), "frame version 2 rejected");

        let oversize = vec![0u8; MAX_DFROST_CATCHUP_FRAME_BYTES + 1];
        assert!(
            decode_request(&oversize).is_err(),
            "oversize input rejected before decode (request)"
        );
        assert!(
            decode_frame(&oversize).is_err(),
            "oversize input rejected before decode (frame)"
        );

        let garbage = vec![0xFFu8; 16];
        assert!(
            decode_request(&garbage).is_err(),
            "garbage bytes rejected (request)"
        );
        assert!(
            decode_frame(&garbage).is_err(),
            "garbage bytes rejected (frame)"
        );
    }

    #[test]
    fn select_catchup_serves_dk_quorum_and_beacons_zeb1030() {
        let mut log = test_active_log(1);
        let actor_a = test_owner(0xA1);
        let actor_b = test_owner(0xA2);

        // 1 stale `dk` at epoch 0 — must be excluded from selection.
        log.insert_event_for_test(test_dk_event(0, actor_a, test_hlc(900, 0, "dev1")));
        // 2 `dk` events at epoch 1 from actors A/B.
        log.insert_event_for_test(test_dk_event(1, actor_a, test_hlc(1000, 0, "dev1")));
        log.insert_event_for_test(test_dk_event(1, actor_b, test_hlc(1001, 0, "dev1")));
        // Re-minted duplicate `dk` from A with a higher HLC — must win
        // over A's earlier `dk` for the newest-per-actor collapse.
        let dk_a_remint = test_dk_event(1, actor_a, test_hlc(1002, 0, "dev1"));
        log.insert_event_for_test(dk_a_remint.clone());

        // 3 `vb` events at ascending HLCs.
        let vb1 = test_vb_event(test_hlc(2000, 0, "dev1"));
        let vb2 = test_vb_event(test_hlc(2001, 0, "dev1"));
        let vb3 = test_vb_event(test_hlc(2002, 0, "dev1"));
        log.insert_event_for_test(vb1.clone());
        log.insert_event_for_test(vb2.clone());
        log.insert_event_for_test(vb3.clone());

        // Fresh requester: no state at all → full catch-up.
        let req = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 0,
            active: false,
            beacon_watermark: None,
        };
        let sel = select_catchup(&log, &req, MAX_CATCHUP_BEACONS_PER_ROUND, 0).expect("selection");
        assert_eq!(
            sel.status,
            CatchupStatus {
                epoch: 1,
                active: true
            }
        );
        assert_eq!(
            sel.dk_events.len(),
            2,
            "exactly 2 dk events (newest per actor)"
        );
        let a_selected = sel
            .dk_events
            .iter()
            .find(|e| e.actor == actor_a)
            .expect("actor A represented");
        assert_eq!(a_selected, &dk_a_remint, "re-mint wins for actor A");
        assert!(
            sel.dk_events.iter().any(|e| e.actor == actor_b),
            "actor B represented"
        );
        assert_eq!(
            sel.beacons,
            vec![vb1.clone(), vb2.clone(), vb3.clone()],
            "all 3 beacons oldest-first"
        );

        // Requester active at the current epoch, watermark = LAST beacon
        // → nothing new → None.
        let req_last_wm = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 1,
            active: true,
            beacon_watermark: Some(BeaconWatermark {
                wall_ms: 2002,
                logical: 0,
                device_id: "dev1".into(),
            }),
        };
        assert!(
            select_catchup(&log, &req_last_wm, MAX_CATCHUP_BEACONS_PER_ROUND, 0).is_none(),
            "fully current requester gets None"
        );

        // Requester active at the current epoch, watermark = FIRST beacon
        // → dk empty (requester is already at current epoch), beacons =
        // [2nd, 3rd].
        let req_first_wm = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 1,
            active: true,
            beacon_watermark: Some(BeaconWatermark {
                wall_ms: 2000,
                logical: 0,
                device_id: "dev1".into(),
            }),
        };
        let sel2 = select_catchup(&log, &req_first_wm, MAX_CATCHUP_BEACONS_PER_ROUND, 0)
            .expect("selection");
        assert!(
            sel2.dk_events.is_empty(),
            "requester already at current epoch"
        );
        assert_eq!(sel2.beacons, vec![vb2, vb3]);
    }

    #[test]
    fn select_catchup_inactive_responder_serves_nothing_zeb1030() {
        let log = DfrostLog::new();
        let req = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 0,
            active: false,
            beacon_watermark: None,
        };
        assert!(select_catchup(&log, &req, MAX_CATCHUP_BEACONS_PER_ROUND, 0).is_none());
    }

    #[test]
    fn select_catchup_caps_beacons_oldest_first_zeb1030() {
        let mut log = test_active_log(1);
        let beacons: Vec<SignedCommitteeEvent> = (0..5)
            .map(|i| test_vb_event(test_hlc(1000 + i, 0, "dev1")))
            .collect();
        for b in &beacons {
            log.insert_event_for_test(b.clone());
        }
        let req = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 0,
            active: false,
            beacon_watermark: None,
        };
        let sel = select_catchup(&log, &req, 2, 0).expect("selection");
        assert_eq!(sel.beacons, vec![beacons[0].clone(), beacons[1].clone()]);

        // max_beacons = 0 boundary: the cap must be checked BEFORE the
        // push, not after, or a beacon slips through on the first match.
        let sel_zero = select_catchup(&log, &req, 0, 0).expect("selection");
        assert!(
            sel_zero.beacons.is_empty(),
            "max_beacons=0 yields zero beacons"
        );
    }

    /// ZEB-1030 final-review C1 regression: a genuine-payload beacon
    /// re-wrapped in an envelope at `wall_ms = u64::MAX` (the
    /// permanent-watermark-pin vector) must be ignored when computing
    /// the watermark — the event is still retained in the log (skew
    /// gate is at the view, not the store), and a subsequent normal
    /// beacon still advances the watermark past it.
    #[test]
    fn beacon_watermark_of_ignores_forward_skewed_envelope_zeb1030() {
        let mut log = test_active_log(1);
        let now = 1_700_000_000_000u64; // fixed plausible "now" for determinism

        assert!(beacon_watermark_of(&log, now).is_none(), "no vb events yet");

        let sane = test_vb_event(test_hlc(now - 1000, 0, "dev1"));
        log.insert_event_for_test(sane);
        let wm = beacon_watermark_of(&log, now).expect("watermark present");
        assert_eq!(wm.wall_ms, now - 1000);

        // Adversarial/misconfigured-clock vector: same kind of genuine
        // payload, envelope re-wrapped at wall_ms = u64::MAX.
        let skewed = test_vb_event(test_hlc(u64::MAX, 0, "dev1"));
        log.insert_event_for_test(skewed.clone());
        assert!(
            log.events().any(|e| e.hlc.wall_ms == u64::MAX),
            "skewed event is still retained in the log"
        );
        let wm_after_skewed = beacon_watermark_of(&log, now).expect("watermark still present");
        assert_eq!(
            wm_after_skewed.wall_ms,
            now - 1000,
            "skewed envelope must not advance (or clear) the watermark"
        );

        // A subsequent normal beacon still advances the watermark.
        let newer_sane = test_vb_event(test_hlc(now - 500, 0, "dev1"));
        log.insert_event_for_test(newer_sane);
        let wm_final = beacon_watermark_of(&log, now).expect("watermark present");
        assert_eq!(
            wm_final.wall_ms,
            now - 500,
            "a subsequent normal beacon still advances the watermark"
        );

        // now_wall_ms == 0 (unreadable local clock) disables the gate —
        // the skewed event becomes the max again.
        let wm_no_clock = beacon_watermark_of(&log, 0).expect("watermark present");
        assert_eq!(
            wm_no_clock.wall_ms,
            u64::MAX,
            "now_wall_ms=0 disables the forward-skew gate (apply-all)"
        );
    }

    /// ZEB-1035: a retained `vb` whose envelope HLC is implausibly
    /// future must never be SERVED. Requesters cap their watermark at
    /// the view (`beacon_watermark_of`, ZEB-1030 final-review C1), so a
    /// future-stamped retained beacon sorts above every watermark
    /// forever — without this gate it is re-served and re-verified
    /// every round, and the fully-current → `None` short-circuit never
    /// fires for any affected requester.
    #[test]
    fn select_catchup_skips_forward_skewed_beacons_zeb1035() {
        let now: u64 = 1_000_000_000;
        let max = crate::clock_trust::MAX_FORWARD_SKEW_MS;

        let mut log = test_active_log(1);
        let vb_ok = test_vb_event(test_hlc(2000, 0, "dev1"));
        // Exactly at the tolerance bound: still plausible, still served.
        let vb_edge = test_vb_event(test_hlc(now + max, 0, "dev1"));
        let vb_skewed = test_vb_event(test_hlc(now + max + 1, 0, "dev1"));
        log.insert_event_for_test(vb_ok.clone());
        log.insert_event_for_test(vb_edge.clone());
        log.insert_event_for_test(vb_skewed.clone());

        // Fully-current requester whose watermark covers every PLAUSIBLE
        // beacon: only the skewed event sorts above it, the gate withholds
        // it, and the short-circuit fires again (pre-fix, this requester
        // was served the skewed beacon every round, forever).
        let req_current = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 1,
            active: true,
            beacon_watermark: Some(beacon_watermark_of(&log, now).expect("watermark")),
        };
        assert!(
            select_catchup(&log, &req_current, MAX_CATCHUP_BEACONS_PER_ROUND, now).is_none(),
            "forward-skewed retained beacon must not defeat the fully-current short-circuit"
        );

        // A behind requester (no watermark) receives the plausible
        // beacons — never the skewed one.
        let req_fresh = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 0,
            active: false,
            beacon_watermark: None,
        };
        let sel = select_catchup(&log, &req_fresh, MAX_CATCHUP_BEACONS_PER_ROUND, now)
            .expect("selection");
        assert_eq!(
            sel.beacons,
            vec![vb_ok.clone(), vb_edge.clone()],
            "at-bound beacon served; beyond-bound beacon withheld"
        );

        // now == 0 (responder clock unreadable) disables the gate.
        let sel_disabled =
            select_catchup(&log, &req_fresh, MAX_CATCHUP_BEACONS_PER_ROUND, 0).expect("selection");
        assert_eq!(sel_disabled.beacons.len(), 3, "gate disabled at now == 0");
    }

    /// ZEB-1036: a replica that retained only the LARGER-output sibling
    /// of a tied beacon group has that group's HLC range at-or-below its
    /// watermark, so the strictly-above rule alone can never serve it
    /// the smaller sibling it needs for min-wins convergence. Tied
    /// groups are therefore served regardless of watermark — and a
    /// no-tie log keeps the fully-current → `None` short-circuit.
    #[test]
    fn select_catchup_serves_tied_beacons_below_watermark_zeb1036() {
        let now: u64 = 1_000_000_000;
        let hash_tied = [0x11; 32];
        let hash_solo = [0x22; 32];

        let mut log = test_active_log(1);
        let vb_tied_lo = test_vb_event_with(test_hlc(1000, 0, "dev1"), hash_tied, [0x01; 32]);
        let vb_tied_hi = test_vb_event_with(test_hlc(2000, 0, "dev1"), hash_tied, [0x02; 32]);
        let vb_solo = test_vb_event_with(test_hlc(3000, 0, "dev1"), hash_solo, [0x03; 32]);
        log.insert_event_for_test(vb_tied_lo.clone());
        log.insert_event_for_test(vb_tied_hi.clone());
        log.insert_event_for_test(vb_solo.clone());

        // Fully-current requester (watermark = newest retained vb, 3000):
        // nothing sorts strictly above it, but the tied pair is served
        // anyway — this requester may be the one holding only the larger
        // sibling. The untied vb_solo is NOT re-served.
        let req_current = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 1,
            active: true,
            beacon_watermark: Some(beacon_watermark_of(&log, now).expect("watermark")),
        };
        let sel = select_catchup(&log, &req_current, MAX_CATCHUP_BEACONS_PER_ROUND, now)
            .expect("tied group must be served despite a covering watermark");
        assert_eq!(
            sel.beacons,
            vec![vb_tied_lo.clone(), vb_tied_hi.clone()],
            "exactly the tied group, oldest-first, watermark ignored for it"
        );

        // Same shape without the tie: short-circuit intact.
        let mut no_tie_log = test_active_log(1);
        no_tie_log.insert_event_for_test(vb_tied_lo.clone());
        no_tie_log.insert_event_for_test(vb_solo.clone());
        let req_current_no_tie = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 1,
            active: true,
            beacon_watermark: Some(beacon_watermark_of(&no_tie_log, now).expect("watermark")),
        };
        assert!(
            select_catchup(
                &no_tie_log,
                &req_current_no_tie,
                MAX_CATCHUP_BEACONS_PER_ROUND,
                now
            )
            .is_none(),
            "distinct-hash beacons are not a tie — fully-current stays None"
        );

        // A behind requester (no watermark) still gets everything once,
        // with no duplicates from the tie rule.
        let req_fresh = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 0,
            active: false,
            beacon_watermark: None,
        };
        let sel_fresh = select_catchup(&log, &req_fresh, MAX_CATCHUP_BEACONS_PER_ROUND, now)
            .expect("selection");
        assert_eq!(sel_fresh.beacons, vec![vb_tied_lo, vb_tied_hi, vb_solo]);
    }

    /// PR#780 round-1 (CodeAnt): the beacon cap must never truncate a
    /// tie group mid-pair — tied members claim cap budget first, and the
    /// served order stays HLC (oldest-first).
    #[test]
    fn select_catchup_cap_never_splits_tied_group_zeb1036() {
        let now: u64 = 1_000_000_000;
        let hash_tied = [0x44; 32];

        let mut log = test_active_log(1);
        let vb_old_a = test_vb_event_with(test_hlc(1000, 0, "dev1"), [0x55; 32], [0x0A; 32]);
        let vb_old_b = test_vb_event_with(test_hlc(2000, 0, "dev1"), [0x66; 32], [0x0B; 32]);
        let vb_tied_lo = test_vb_event_with(test_hlc(3000, 0, "dev1"), hash_tied, [0x01; 32]);
        let vb_tied_hi = test_vb_event_with(test_hlc(4000, 0, "dev1"), hash_tied, [0x02; 32]);
        log.insert_event_for_test(vb_old_a.clone());
        log.insert_event_for_test(vb_old_b.clone());
        log.insert_event_for_test(vb_tied_lo.clone());
        log.insert_event_for_test(vb_tied_hi.clone());

        // Cold requester, cap 3: an interleaved oldest-first selection
        // would spend the cap on [old_a, old_b, tied_lo] and cut the tie
        // in half. Tied-first budgeting keeps the pair whole and fills
        // the remainder with the oldest backlog, emitted in HLC order.
        let req_fresh = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 0,
            active: false,
            beacon_watermark: None,
        };
        let sel = select_catchup(&log, &req_fresh, 3, now).expect("selection");
        assert_eq!(
            sel.beacons,
            vec![vb_old_a, vb_tied_lo, vb_tied_hi],
            "tie pair whole under the cap, remainder oldest-first, HLC order"
        );
    }

    /// ZEB-1036 × ZEB-1035: the tie rule never overrides the skew gate.
    /// A forward-skewed sibling is excluded from serving AND from tie
    /// detection — one plausible output plus one skewed output is not a
    /// servable tie, so the fully-current short-circuit still fires.
    #[test]
    fn select_catchup_tied_beacons_still_skew_gated_zeb1036() {
        let now: u64 = 1_000_000_000;
        let max = crate::clock_trust::MAX_FORWARD_SKEW_MS;
        let hash_tied = [0x33; 32];

        let mut log = test_active_log(1);
        let vb_plausible = test_vb_event_with(test_hlc(1000, 0, "dev1"), hash_tied, [0x01; 32]);
        let vb_skewed_sibling =
            test_vb_event_with(test_hlc(now + max + 1, 0, "dev1"), hash_tied, [0x02; 32]);
        log.insert_event_for_test(vb_plausible.clone());
        log.insert_event_for_test(vb_skewed_sibling);

        let req_current = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 1,
            active: true,
            beacon_watermark: Some(beacon_watermark_of(&log, now).expect("watermark")),
        };
        assert!(
            select_catchup(&log, &req_current, MAX_CATCHUP_BEACONS_PER_ROUND, now).is_none(),
            "a tie completed only by a forward-skewed event must not defeat the short-circuit"
        );

        // With the gate disabled (now == 0) the pair IS a tie again and
        // both members are served past the watermark.
        let req_current_disabled = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 1,
            active: true,
            beacon_watermark: Some(beacon_watermark_of(&log, 0).expect("watermark")),
        };
        let sel = select_catchup(
            &log,
            &req_current_disabled,
            MAX_CATCHUP_BEACONS_PER_ROUND,
            0,
        )
        .expect("gate disabled: tie visible again");
        assert_eq!(sel.beacons.len(), 2);
    }

    /// ZEB-1030 final-review I4 / PR#778 round-1 regression: capping is
    /// enforced DURING insertion, not by truncating the grouped result
    /// afterward. 16 responder ids fill the cap; a 17th arriving after
    /// that is dropped on sight (and counted); a LATE follow-up frame for
    /// an already-admitted id (rid 0) still lands in its existing group,
    /// proving the cap gates new groups, not frames for an existing one.
    #[test]
    fn group_frames_caps_at_insertion_time_zeb1030() {
        let status = CatchupStatus {
            epoch: 1,
            active: true,
        };
        fn rid_for(i: u16) -> [u8; 8] {
            let mut rid = [0u8; 8];
            rid[..2].copy_from_slice(&i.to_be_bytes());
            rid
        }

        // 16 distinct responder ids, each with its Status frame — fills
        // the cap exactly.
        let mut frames: Vec<CatchupFrame> = (0..16u16)
            .map(|i| CatchupFrame {
                version: CATCHUP_VERSION,
                responder_id: rid_for(i),
                body: CatchupBody::Status(status),
            })
            .collect();

        // The 17th distinct responder id arrives AFTER the cap is
        // already full — its frame must be dropped at insertion time.
        frames.push(CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: rid_for(16),
            body: CatchupBody::Status(status),
        });

        // A follow-up frame for an ALREADY-admitted id (rid 0), arriving
        // after the dropped 17th-group frame, must still land.
        let followup_dk_rid0 = CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: rid_for(0),
            body: CatchupBody::DkEvidence(vec![9, 9, 9]),
        };
        frames.push(followup_dk_rid0.clone());

        let (groups, dropped) = group_frames(frames, MAX_CATCHUP_RESPONDER_GROUPS);
        assert_eq!(
            groups.len(),
            MAX_CATCHUP_RESPONDER_GROUPS,
            "at most MAX_CATCHUP_RESPONDER_GROUPS groups survive"
        );
        assert_eq!(
            dropped, 1,
            "the 17th group's single Status frame was dropped"
        );
        assert!(
            groups.iter().all(|(_, g)| g[0].responder_id != rid_for(16)),
            "no group exists for the capped-out 17th responder id"
        );
        let rid0_group = &groups
            .iter()
            .find(|(_, g)| g[0].responder_id == rid_for(0))
            .expect("rid 0's group survived")
            .1;
        assert_eq!(
            rid0_group.len(),
            2,
            "rid 0's late follow-up frame still landed in its existing group"
        );
        assert_eq!(rid0_group[1], followup_dk_rid0);
    }

    #[test]
    fn group_frames_discards_statusless_groups_zeb1030() {
        let rid_x = [1u8; 8];
        let rid_y = [2u8; 8];
        let rid_z = [3u8; 8];
        let status = CatchupStatus {
            epoch: 1,
            active: true,
        };

        let frame_status_x = CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: rid_x,
            body: CatchupBody::Status(status),
        };
        let frame_dk_x = CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: rid_x,
            body: CatchupBody::DkEvidence(vec![1, 2, 3]),
        };
        // rid Y has a `dk` frame but no status frame → discarded.
        let frame_dk_y = CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: rid_y,
            body: CatchupBody::DkEvidence(vec![4, 5, 6]),
        };
        // rid Z has TWO status frames → also discarded.
        let frame_status_z1 = CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: rid_z,
            body: CatchupBody::Status(status),
        };
        let frame_status_z2 = CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: rid_z,
            body: CatchupBody::Status(status),
        };

        let frames = vec![
            frame_status_x.clone(),
            frame_dk_x.clone(),
            frame_dk_y,
            frame_status_z1,
            frame_status_z2,
        ];
        let (groups, dropped) = group_frames(frames, MAX_CATCHUP_RESPONDER_GROUPS);
        assert_eq!(dropped, 0, "well under the cap — nothing dropped");
        assert_eq!(groups.len(), 1, "only rid X's group survives");
        assert_eq!(groups[0].0, status);
        assert_eq!(groups[0].1, vec![frame_status_x, frame_dk_x]);
    }
}
