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

/// Encode a [`CatchupRequest`]. No size cap on encode — a request has no
/// event payload, so it cannot grow unbounded.
pub fn encode_request(req: &CatchupRequest) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(req, &mut out)
        .map_err(|e| format!("dfrost catchup request encode: {e}"))?;
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
/// be evaluated on the encoded bytes.
pub fn encode_frame(frame: &CatchupFrame) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(frame, &mut out)
        .map_err(|e| format!("dfrost catchup frame encode: {e}"))?;
    if out.len() > MAX_DFROST_CATCHUP_FRAME_BYTES {
        return Err(format!(
            "dfrost catchup frame exceeds {MAX_DFROST_CATCHUP_FRAME_BYTES}-byte cap \
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
/// (ZEB-753), so the LAST `vb` event encountered while scanning in
/// order carries the maximum `(wall_ms, logical, device_id)` among all
/// `vb` events — no separate max-tracking needed.
pub fn beacon_watermark_of(log: &DfrostLog) -> Option<BeaconWatermark> {
    log.events()
        .filter(|e| e.kind == DfrostEventKind::VrfBeacon)
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
    /// `vb` events with envelope HLC strictly above the watermark,
    /// OLDEST-first, capped at `max_beacons`.
    pub beacons: Vec<SignedCommitteeEvent>,
}

/// Pure responder selection. `None` ⇒ nothing to serve (inactive
/// responder, or requester already fully current) — transport answers
/// with silence.
pub fn select_catchup(
    log: &DfrostLog,
    req: &CatchupRequest,
    max_beacons: usize,
) -> Option<CatchupSelection> {
    // Rule 1: inactive responder has nothing to serve.
    if !log.committee_state.active {
        return None;
    }
    let current_epoch = log.committee_state.current_epoch;

    // Beacons: envelope HLC strictly above the watermark (no watermark
    // ⇒ all), oldest-first, capped at `max_beacons`. `log.events()` is
    // already HLC-ordered, so a simple forward scan with an early break
    // yields the oldest matches first.
    let watermark = req
        .beacon_watermark
        .as_ref()
        .map(|w| (w.wall_ms, w.logical, w.device_id.clone()));
    let mut beacons = Vec::new();
    for ev in log.events() {
        if beacons.len() >= max_beacons {
            break;
        }
        if ev.kind != DfrostEventKind::VrfBeacon {
            continue;
        }
        let hlc_key = (ev.hlc.wall_ms, ev.hlc.logical, ev.hlc.device_id.clone());
        let above_watermark = match &watermark {
            Some(wm) => &hlc_key > wm,
            None => true,
        };
        if above_watermark {
            beacons.push(ev.clone());
        }
    }

    // Rule 2: requester fully current (active at the current epoch, and
    // no beacon above its watermark) ⇒ nothing to serve.
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
pub fn group_frames(frames: Vec<CatchupFrame>) -> Vec<(CatchupStatus, Vec<CatchupFrame>)> {
    let mut grouped: Vec<([u8; 8], Vec<CatchupFrame>)> = Vec::new();
    for frame in frames {
        match grouped
            .iter_mut()
            .find(|(rid, _)| *rid == frame.responder_id)
        {
            Some((_, group)) => group.push(frame),
            None => grouped.push((frame.responder_id, vec![frame])),
        }
    }
    grouped
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
        .collect()
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
        let payload = VrfBeaconPayload {
            ceremony_id: [0u8; 32],
            message_hash: [0u8; 32],
            signature: vec![0u8; 64],
            vrf_output: [0u8; 32],
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
        let sel = select_catchup(&log, &req, MAX_CATCHUP_BEACONS_PER_ROUND).expect("selection");
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
            select_catchup(&log, &req_last_wm, MAX_CATCHUP_BEACONS_PER_ROUND).is_none(),
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
        let sel2 =
            select_catchup(&log, &req_first_wm, MAX_CATCHUP_BEACONS_PER_ROUND).expect("selection");
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
        assert!(select_catchup(&log, &req, MAX_CATCHUP_BEACONS_PER_ROUND).is_none());
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
        let sel = select_catchup(&log, &req, 2).expect("selection");
        assert_eq!(sel.beacons, vec![beacons[0].clone(), beacons[1].clone()]);

        // max_beacons = 0 boundary: the cap must be checked BEFORE the
        // push, not after, or a beacon slips through on the first match.
        let sel_zero = select_catchup(&log, &req, 0).expect("selection");
        assert!(
            sel_zero.beacons.is_empty(),
            "max_beacons=0 yields zero beacons"
        );
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
        let groups = group_frames(frames);
        assert_eq!(groups.len(), 1, "only rid X's group survives");
        assert_eq!(groups[0].0, status);
        assert_eq!(groups[0].1, vec![frame_status_x, frame_dk_x]);
    }
}
