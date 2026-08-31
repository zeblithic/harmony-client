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
use crate::community_dfrost_types::{
    DfrostEventKind, DkgCompletePayload, ResetMarkerPayload, SignedCommitteeEvent,
};
use crate::owner_state_types::OwnerAddr;

/// Wire version for the catch-up request/frame codec. Bumped on any
/// breaking change to [`CatchupRequest`] or [`CatchupFrame`].
pub const CATCHUP_VERSION: u8 = 1;

/// Inbound size cap for one encoded [`CatchupRequest`] or [`CatchupFrame`].
/// A `Status`/`DkEvidence`/`Beacon` frame carries at most one
/// `SignedCommitteeEvent` — small CBOR, well under this bound for any
/// realistic committee (mirrors `MAX_DFROST_PAYLOAD_BYTES` in
/// `event_loop.rs`). A `ResetChain` frame (ZEB-1031 §6.3, reshaped by
/// ZEB-1038) carries ONE reset-chain link — a marker plus its successor
/// `dk` quorum, trimmed to O(t·N) bytes by ZEB-1045 — served
/// one-per-frame up to [`MAX_RESET_CHAIN_LINKS_PER_RESPONSE`] frames per
/// response; see that constant's doc for the sizing math. Checked
/// before decode to prevent peer-controlled allocation.
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

/// ZEB-1031 §6.3 review I3: cap on the number of reset-chain links
/// (`ResetChainLink`, each a marker plus its successor `dk` quorum)
/// served in ONE `CatchupBody::ResetChain` frame. `select_reset_chain`
/// serves the OLDEST needed links first (its natural iteration order
/// over `vk_history`, ascending `old_epoch`), so a chain longer than
/// this cap heals over multiple catch-up rounds — the requester's own
/// epoch/`pending_reset` state advances past whatever was applied last
/// round, resuming the walk with no separate cursor needed (the house
/// pattern behind the 300s catch-up cadence generally).
///
/// Enforced on BOTH sides: the responder truncates the selection before
/// serving (`select_reset_chain`), and the requester independently caps
/// the links it will ATTEMPT to verify from one responder GROUP — across
/// all of the group's `ResetChain` frames combined, charged BEFORE
/// verification (`catchup_decode_and_verify`; group-total since
/// ZEB-1038, because per-link frames made multi-frame chains the
/// legitimate serving shape and a per-frame cap would multiply the
/// verify-work bound by the frame count; attempt-counted rather than
/// accepted-counted, because invalid links never enter the accepted set
/// and would otherwise refresh the budget frame after frame) — the same
/// defence-in-depth posture as `MAX_CATCHUP_RESPONDER_GROUPS`/
/// `MAX_CATCHUP_BEACONS_PER_ROUND`, whose docs reason about exactly this
/// per-link `Ed25519::verify_strict` cost; a `ResetChain` frame is the
/// one fan-out point in this module that was unbounded before this cap.
///
/// Sizing against [`MAX_DFROST_CATCHUP_FRAME_BYTES`] (64 KiB): one link
/// is a marker plus `dk` events whose payloads EACH carry the full
/// N-entry verifying-share list (~50 bytes/entry) plus the N-entry
/// member list — ~(80N + 300) bytes per event. Served untrimmed (one
/// event per confirming member) a link weighed O(N²): past the frame
/// cap around committee N in the low 40s, and at N≈16 a 3-link chain
/// already exceeded the cap. The pre-ZEB-1038 shape packed the whole
/// selection into one frame and dropped it whole on overflow,
/// permanently wedging that requester/responder pair
/// (`select_reset_chain` rebuilt the same oversized set every round).
/// Two fixes bound this:
///
/// * ZEB-1038 — `catchup_respond` serves ONE link per frame,
///   oldest-first, fit-testing each frame with [`encode_frame`] and
///   STOPPING at the first link that cannot fit alone (markers apply in
///   epoch order, so links past a gap are wasted verify work).
/// * ZEB-1045 — `trim_dk_events_to_quorum` flattens each link to its
///   payload threshold t (O(t·N) bytes, ~t × (80N + 300)), since
///   `adopt_initial_quorum` needs only t distinct actors. The
///   single-link bound moves from N in the low 40s to roughly N≈400 at
///   t=2, N≈270 at t=3, N≈155 at t=5; the ZEB-1038
///   stop-at-first-misfit remains the backstop past that.
pub const MAX_RESET_CHAIN_LINKS_PER_RESPONSE: usize = 8;

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

/// Externally-tagged enum — encodes as a 1-entry map
/// {"st"|"dk"|"vb"|"rc": ...}.
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
    /// ZEB-1031 §6.3: ciborium-encoded `Vec<ResetChainLink>` — the
    /// responder's reset-chain healing payload for a straggler stuck
    /// pre-reset (spec §6.2). A NEW variant, so a legacy (pre-ZEB-1031)
    /// decoder that never sees this tag is unaffected — every existing
    /// `Status`/`DkEvidence`/`Beacon` frame encodes byte-identically to
    /// before; an old peer that DOES receive a `ResetChain` frame drops
    /// just that one frame (the established per-frame decode-failure
    /// tolerance — see `decode_frame`'s callers), never the whole round.
    #[serde(rename = "rc")]
    ResetChain(#[serde(with = "serde_bytes")] Vec<u8>),
}

/// ZEB-1031 §6.3: one link of a reset chain — the `rs` marker that
/// retired a committee plus the successor epoch's retained `dk` quorum
/// events (possibly empty, if the successor DKG hasn't completed on
/// this responder yet). Interleaved `marker₁, quorum₁, marker₂,
/// quorum₂, …` across links for a multi-reset chain (spec §6.2).
/// 2-char keys per the module's same-length-keys invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetChainLink {
    #[serde(rename = "mk")]
    pub marker: SignedCommitteeEvent,
    #[serde(rename = "dk")]
    pub dk_events: Vec<SignedCommitteeEvent>,
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
    /// ZEB-1031 §6.3: reset-chain healing links, oldest reset first.
    /// Empty unless this responder's `vk_history` holds a reset the
    /// requester's implied epoch predates.
    pub reset_chain: Vec<ResetChainLink>,
}

/// `dk` (DkgComplete) events at exactly `epoch`, one per distinct actor
/// (newest per actor in synthesized-id/HLC order — `log.events()` is
/// already HLC-ordered, so a later `insert` for the same actor key
/// naturally overwrites the earlier one). Shared by `select_catchup`'s
/// current-epoch retrieval and the reset-chain's per-link successor-
/// epoch retrieval (ZEB-1031 §6.3 — "reuse that retrieval").
fn dk_events_for_epoch(log: &DfrostLog, epoch: u64) -> Vec<SignedCommitteeEvent> {
    let mut newest_per_actor: BTreeMap<OwnerAddr, SignedCommitteeEvent> = BTreeMap::new();
    for ev in log.events() {
        if ev.kind != DfrostEventKind::DkgComplete {
            continue;
        }
        let payload: DkgCompletePayload = match ciborium::de::from_reader(&ev.payload[..]) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if payload.epoch != epoch {
            continue;
        }
        newest_per_actor.insert(ev.actor, ev.clone());
    }
    newest_per_actor.into_values().collect()
}

/// ZEB-1031 §6.3: build the reset-chain healing links for a requester
/// whose implied epoch (`req.epoch`; `0` for an inactive/fresh
/// requester) predates one or more of this responder's recorded
/// resets. For each `vk_history` entry with `req.epoch <= old_epoch`
/// (monotonically increasing across resets, so this naturally selects
/// "that entry forward"), attach the retained `rs` marker event for
/// that reset plus the successor epoch's `dk` quorum (which may still
/// be empty if this responder hasn't itself completed that successor
/// DKG yet — the requester picks the chain back up on a later round),
/// trimmed to a threshold-quorum subset by
/// [`trim_dk_events_to_quorum`] (ZEB-1045 — see its doc).
///
/// ZEB-1031 review I3: capped at [`MAX_RESET_CHAIN_LINKS_PER_RESPONSE`]
/// links, OLDEST first (the natural ascending-`old_epoch` iteration
/// order over `vk_history`) — a chain longer than the cap is served
/// across multiple rounds; see that constant's doc for the reasoning
/// and the frame-size math.
fn select_reset_chain(log: &DfrostLog, req: &CatchupRequest) -> Vec<ResetChainLink> {
    if log.committee_state.vk_history.is_empty() {
        return Vec::new();
    }
    let mut links = Vec::new();
    for entry in &log.committee_state.vk_history {
        if links.len() >= MAX_RESET_CHAIN_LINKS_PER_RESPONSE {
            break;
        }
        if req.epoch > entry.old_epoch {
            continue;
        }
        let marker = log.events().find(|ev| {
            if ev.kind != DfrostEventKind::ResetMarker {
                return false;
            }
            let payload: ResetMarkerPayload = match ciborium::de::from_reader(&ev.payload[..]) {
                Ok(p) => p,
                Err(_) => return false,
            };
            payload.reset_proposal_id == entry.reset_id
        });
        let Some(marker) = marker else {
            // Defensive: a vk_history entry with no retained marker
            // event would be an apply-layer bug (`apply_reset_marker`
            // always inserts the event it just applied) — skip rather
            // than serve a link with no marker to verify against.
            tracing::warn!(
                reset_id = ?entry.reset_id,
                "dfrost catchup: vk_history entry has no retained rs marker event — skipped",
            );
            continue;
        };
        let successor_epoch = entry.old_epoch.saturating_add(1);
        links.push(ResetChainLink {
            marker: marker.clone(),
            dk_events: trim_dk_events_to_quorum(dk_events_for_epoch(log, successor_epoch)),
        });
    }
    links
}

/// ZEB-1045: trim a reset-chain link's successor-epoch `dk` set to a
/// threshold-quorum subset. `adopt_initial_quorum` needs only
/// `threshold` distinct actors carrying byte-identical payloads — the
/// remaining N−t events are redundant attestations — so serving all N
/// made a link O(N²) bytes (each of N events carries the full N-entry
/// verifying-share list) for zero adoptability gain. Trimming flattens
/// a link to O(t·N), moving the single-link 64KiB bound from committee
/// N in the low 40s into the hundreds for small t (the ZEB-1038
/// residual this closes for realistic sizes).
///
/// Selection is deterministic so repeated rounds serve the same subset:
/// group by identical signed payload bytes (honest confirmers of one
/// ceremony encode identical payloads; `adopt_initial_quorum` rejects
/// any set whose payloads disagree, so a mixed set is unadoptable
/// anyway — which means the pre-trim shape let ONE divergent-payload
/// event from a misbehaving member poison the whole served set every
/// round), take the group with the most distinct actors (ties break to
/// the smaller payload key via `BTreeMap` order + strict `>`), and
/// serve its first `min(threshold, len)` events in ascending actor
/// order. Sub-threshold serving ("serve what you have, the requester
/// retries next round") is preserved by the `min`. Bare `t` with no
/// margin is deliberate: the requester drops a link WHOLE on any dk
/// verify failure, so extra events can never rescue a link — they only
/// add bytes and one more event that could fail.
fn trim_dk_events_to_quorum(dk_events: Vec<SignedCommitteeEvent>) -> Vec<SignedCommitteeEvent> {
    if dk_events.is_empty() {
        return dk_events;
    }
    // One event per actor in ascending actor order (dk_events_for_epoch's
    // BTreeMap order), so group size = distinct-actor count and
    // within-group index order stays ascending-actor.
    let selected: Vec<usize> = {
        let mut groups: BTreeMap<&[u8], Vec<usize>> = BTreeMap::new();
        for (i, ev) in dk_events.iter().enumerate() {
            groups.entry(ev.payload.as_slice()).or_default().push(i);
        }
        let (payload, idxs) = groups
            .into_iter()
            .reduce(|best, cand| {
                if cand.1.len() > best.1.len() {
                    cand
                } else {
                    best
                }
            })
            .expect("non-empty dk_events yields at least one group");
        let threshold = match ciborium::de::from_reader::<DkgCompletePayload, _>(payload) {
            Ok(p) => p.threshold as usize,
            // Defensive only: `dk_events_for_epoch` already drops events
            // with undecodable payloads, so this arm is unreachable from
            // the one call site — serve untrimmed rather than guess.
            Err(_) => return dk_events,
        };
        idxs.into_iter().take(threshold).collect()
    };
    let mut kept = 0usize;
    dk_events
        .into_iter()
        .enumerate()
        .filter_map(|(i, ev)| {
            if selected.get(kept) == Some(&i) {
                kept += 1;
                Some(ev)
            } else {
                None
            }
        })
        .collect()
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

    // ZEB-1031 §6.3: reset-chain healing links. Computed only in this
    // (Rule 1-passed, responder active) branch — a responder that is
    // ITSELF mid-reset (deactivated, not yet promoted) serves nothing
    // at all here (Rule 1 above).
    //
    // ZEB-1031 review I2: `select_reset_chain` serves exactly what
    // THIS responder's own `vk_history` holds — a partial (or empty)
    // lineage, never mis-sliced or panicked on (it filters/iterates
    // the vec as-is; an empty `vk_history` short-circuits to `Vec::
    // new()`). "Every active responder's vk_history is the complete
    // lineage" is FALSE: `adopt_initial_quorum`/`adopt_refresh_quorum`
    // never touch `vk_history`, so a node that bootstrapped as a fresh
    // joiner (or healed via an ordinary refresh quorum) after a reset
    // is active with an empty or truncated `vk_history` and serves no
    // chain, or only a suffix of one, even though it genuinely IS
    // current. If the only reachable responders for a straggler are
    // such nodes, the straggler gets no healing path from them — this
    // joins the disclosed denial residuals of spec §10 (a lone/limited
    // responder set can withhold service) rather than being narrower
    // or more self-healing than that family; it is NOT bounded to
    // "wait for this one responder's own DKG to finish" the way a
    // solitary mid-reset responder's gap is.
    let reset_chain = select_reset_chain(log, req);

    // Rule 2: requester fully current (active at the current epoch, no
    // beacon above its watermark — nor any tied group to heal — and no
    // reset chain to serve) ⇒ nothing to serve.
    let requester_current = req.active && req.epoch == current_epoch;
    if requester_current && beacons.is_empty() && reset_chain.is_empty() {
        return None;
    }

    // dk_events: only when the requester is behind the current epoch
    // (inactive, or active at an older epoch).
    let dk_events = if !req.active || req.epoch < current_epoch {
        dk_events_for_epoch(log, current_epoch)
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
        reset_chain,
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

    /// ZEB-1031: `rs` (ResetMarker) event builder for `select_reset_chain`
    /// coverage. `reset_proposal_id` is the join key `select_reset_chain`
    /// uses to find the retained marker for a `vk_history` entry.
    fn test_rs_event(
        reset_proposal_id: [u8; 16],
        old_vk: [u8; 32],
        old_epoch: u64,
        hlc: Hlc,
    ) -> SignedCommitteeEvent {
        let payload = ResetMarkerPayload {
            reset_proposal_id,
            reset_digest: [0u8; 32],
            old_vk,
            old_epoch,
            space_id: crate::owner_state_types::SpaceId([0u8; 16]),
        };
        let mut payload_bytes = Vec::new();
        ciborium::ser::into_writer(&payload, &mut payload_bytes).unwrap();
        SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ResetMarker,
            hlc,
            actor: test_owner(0xAD),
            payload: payload_bytes,
            sig: vec![0u8; 64],
        }
    }

    /// Seed a `vk_history` entry PLUS its retained `rs` marker event in
    /// one call — the shape `select_reset_chain` expects (a lineage
    /// entry with no retained marker is the defensive-skip case,
    /// covered separately by NOT calling this).
    fn seed_reset(log: &mut DfrostLog, reset_id: [u8; 16], old_vk: [u8; 32], old_epoch: u64) {
        let marker_hlc = test_hlc(old_epoch * 1000, 0, "admin");
        log.insert_event_for_test(test_rs_event(
            reset_id,
            old_vk,
            old_epoch,
            marker_hlc.clone(),
        ));
        log.committee_state
            .vk_history
            .push(crate::community_dfrost_log::VkLineageEntry {
                old_vk,
                old_epoch,
                reset_id,
                digest: [0u8; 32],
                at: marker_hlc,
            });
    }

    #[test]
    fn select_reset_chain_empty_vk_history_returns_empty_zeb1031() {
        let log = test_active_log(1);
        assert!(log.committee_state.vk_history.is_empty());
        let req = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 0,
            active: false,
            beacon_watermark: None,
        };
        assert!(select_reset_chain(&log, &req).is_empty());
    }

    #[test]
    fn select_reset_chain_epoch_equals_old_epoch_boundary_is_served_zeb1031() {
        let mut log = test_active_log(2);
        seed_reset(&mut log, [0x01; 16], [0xAA; 32], 1);
        // req.epoch == entry.old_epoch: the straggler's OWN pre-reset
        // epoch — must be served (`<=`, not `<`).
        let req = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 1,
            active: true,
            beacon_watermark: None,
        };
        let links = select_reset_chain(&log, &req);
        assert_eq!(links.len(), 1, "req.epoch == old_epoch must be served");
        assert_eq!(links[0].marker.kind, DfrostEventKind::ResetMarker);
    }

    #[test]
    fn select_reset_chain_epoch_past_old_epoch_is_not_served_zeb1031() {
        let mut log = test_active_log(2);
        seed_reset(&mut log, [0x01; 16], [0xAA; 32], 1);
        let req = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 2, // strictly past old_epoch=1: this reset is already known
            active: true,
            beacon_watermark: None,
        };
        assert!(select_reset_chain(&log, &req).is_empty());
    }

    /// Defensive skip (community_dfrost_catchup.rs's own doc on the
    /// `let Some(marker) = marker else { .. continue; }` branch):
    /// a `vk_history` entry with no retained `rs` event is skipped,
    /// not a panic or a malformed link.
    #[test]
    fn select_reset_chain_skips_entry_with_no_retained_marker_zeb1031() {
        let mut log = test_active_log(1);
        // vk_history entry WITHOUT calling seed_reset's insert_event_for_test.
        log.committee_state
            .vk_history
            .push(crate::community_dfrost_log::VkLineageEntry {
                old_vk: [0xAA; 32],
                old_epoch: 0,
                reset_id: [0x02; 16],
                digest: [0u8; 32],
                at: test_hlc(500, 0, "admin"),
            });
        let req = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 0,
            active: false,
            beacon_watermark: None,
        };
        assert!(
            select_reset_chain(&log, &req).is_empty(),
            "entry with no retained marker must be skipped, not panic"
        );
    }

    /// Two-reset chain: both links present, oldest first, each with
    /// its own successor `dk` quorum attached.
    #[test]
    fn select_reset_chain_two_link_chain_zeb1031() {
        let mut log = test_active_log(3);
        seed_reset(&mut log, [0x01; 16], [0xAA; 32], 1);
        seed_reset(&mut log, [0x02; 16], [0xBB; 32], 2);
        // Successor dk quorum for the SECOND reset (epoch 3).
        log.insert_event_for_test(test_dk_event(3, test_owner(1), test_hlc(3100, 0, "dev1")));
        let req = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 0,
            active: false,
            beacon_watermark: None,
        };
        let links = select_reset_chain(&log, &req);
        assert_eq!(links.len(), 2, "both resets served");
        assert_eq!(
            links[0].marker.hlc.wall_ms, 1000,
            "oldest reset (old_epoch=1) first"
        );
        assert_eq!(links[1].marker.hlc.wall_ms, 2000, "newest reset second");
        assert!(
            links[0].dk_events.is_empty(),
            "no epoch-2 dk quorum was seeded — link 1 has none yet"
        );
        assert_eq!(
            links[1].dk_events.len(),
            1,
            "epoch-3 dk quorum attached to link 2"
        );
    }

    /// ZEB-1045: full-payload `dk` builder — real member/verifying-share
    /// lists so trimmed links carry realistic O(N) payloads and can be
    /// driven through `adopt_initial_quorum` end-to-end (which never
    /// verifies signatures, so arbitrary sig bytes are fine here just as
    /// they are for `select_catchup`).
    fn zeb1045_dk_event(
        epoch: u64,
        actor: OwnerAddr,
        members: &[OwnerAddr],
        threshold: u16,
        joint_vk: [u8; 32],
        hlc: Hlc,
    ) -> SignedCommitteeEvent {
        use crate::community_dfrost_types::MemberVerifyingShare;
        let payload = DkgCompletePayload {
            ceremony_id: [0x45; 32],
            joint_verifying_key: joint_vk,
            verifying_shares: members
                .iter()
                .map(|m| MemberVerifyingShare {
                    member: *m,
                    verifying_share: [0x55; 32],
                })
                .collect(),
            epoch,
            members: members.to_vec(),
            threshold,
            max_signers: members.len() as u16,
            space_id: Some(crate::owner_state_types::SpaceId([0x45; 16])),
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

    /// ZEB-1045: a link's `dk` set is trimmed to the payload's own
    /// `threshold` — the first t events in ascending actor order — and
    /// the trimmed set still clears `adopt_initial_quorum` (which needs
    /// only `threshold` distinct actors, not all N).
    #[test]
    fn reset_chain_links_trimmed_to_threshold_quorum_zeb1045() {
        let mut log = test_active_log(2);
        seed_reset(&mut log, [0x01; 16], [0xAA; 32], 1);
        let members: Vec<OwnerAddr> = (1..=7).map(test_owner).collect();
        for (i, actor) in members.iter().enumerate() {
            log.insert_event_for_test(zeb1045_dk_event(
                2,
                *actor,
                &members,
                3,
                [0xAB; 32],
                test_hlc(1100 + i as u64, 0, "dev"),
            ));
        }
        let req = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 1,
            active: true,
            beacon_watermark: None,
        };
        let links = select_reset_chain(&log, &req);
        assert_eq!(links.len(), 1);
        let dk = &links[0].dk_events;
        assert_eq!(
            dk.len(),
            3,
            "dk set trimmed to the payload threshold (got {} events)",
            dk.len()
        );
        let served: Vec<OwnerAddr> = dk.iter().map(|ev| ev.actor).collect();
        assert_eq!(
            served,
            vec![test_owner(1), test_owner(2), test_owner(3)],
            "deterministic first-t subset in ascending actor order"
        );
        let mut adopter = DfrostLog::new();
        let epoch = adopter
            .adopt_initial_quorum(
                dk,
                &crate::owner_state_types::SpaceId([0x45; 16]),
                &std::collections::BTreeSet::new(),
            )
            .expect("trimmed quorum must still be adoptable");
        assert_eq!(epoch, 2);
    }

    /// ZEB-1045: trimming picks from the largest byte-identical-payload
    /// group, not blindly the first t actors — one divergent-payload dk
    /// event (which would make the whole served set unadoptable:
    /// `adopt_initial_quorum` requires exact payload agreement) is
    /// excluded even when its actor sorts FIRST.
    #[test]
    fn reset_chain_trim_excludes_divergent_payload_minority_zeb1045() {
        let mut log = test_active_log(2);
        seed_reset(&mut log, [0x01; 16], [0xAA; 32], 1);
        let members: Vec<OwnerAddr> = (1..=6).map(test_owner).collect();
        // Actor 1 (first in sort order) diverges on the claimed vk.
        log.insert_event_for_test(zeb1045_dk_event(
            2,
            test_owner(1),
            &members,
            2,
            [0xBB; 32],
            test_hlc(1100, 0, "dev"),
        ));
        for (i, actor) in members.iter().enumerate().skip(1) {
            log.insert_event_for_test(zeb1045_dk_event(
                2,
                *actor,
                &members,
                2,
                [0xAA; 32],
                test_hlc(1100 + i as u64, 0, "dev"),
            ));
        }
        let req = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 1,
            active: true,
            beacon_watermark: None,
        };
        let links = select_reset_chain(&log, &req);
        assert_eq!(links.len(), 1);
        let dk = &links[0].dk_events;
        assert_eq!(dk.len(), 2, "trimmed to the majority group's threshold");
        for ev in dk {
            let p: DkgCompletePayload = ciborium::de::from_reader(&ev.payload[..]).unwrap();
            assert_eq!(
                p.joint_verifying_key, [0xAA; 32],
                "served events all come from the agreeing majority group"
            );
        }
        let served: Vec<OwnerAddr> = dk.iter().map(|ev| ev.actor).collect();
        assert_eq!(
            served,
            vec![test_owner(2), test_owner(3)],
            "first t of the MAJORITY group — the divergent actor 1 is passed over"
        );
    }

    /// ZEB-1045 headline: a committee whose untrimmed link exceeds the
    /// 64KiB frame cap (the documented ZEB-1038 residual) serves a
    /// fitting link after the quorum trim. 120 payload members × 8 dk
    /// actors ≈ 70KiB untrimmed; trimmed to threshold 2 ≈ 18KiB.
    #[test]
    fn reset_chain_large_committee_link_fits_after_trim_zeb1045() {
        let mut log = test_active_log(2);
        seed_reset(&mut log, [0x01; 16], [0xAA; 32], 1);
        let members: Vec<OwnerAddr> = (1..=120).map(|i| test_owner(i as u8)).collect();
        for (i, actor) in members.iter().take(8).enumerate() {
            log.insert_event_for_test(zeb1045_dk_event(
                2,
                *actor,
                &members,
                2,
                [0xAB; 32],
                test_hlc(1100 + i as u64, 0, "dev"),
            ));
        }
        // Regression precondition: the UNTRIMMED link (all 8 events)
        // cannot be framed — this is the fixture ZEB-1038 could only
        // stop on.
        let untrimmed = ResetChainLink {
            marker: log
                .events()
                .find(|ev| ev.kind == DfrostEventKind::ResetMarker)
                .unwrap()
                .clone(),
            dk_events: log
                .events()
                .filter(|ev| ev.kind == DfrostEventKind::DkgComplete)
                .cloned()
                .collect(),
        };
        assert_eq!(untrimmed.dk_events.len(), 8);
        let mut body = Vec::new();
        ciborium::ser::into_writer(std::slice::from_ref(&untrimmed), &mut body).unwrap();
        let oversize = CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: [0x45; 8],
            body: CatchupBody::ResetChain(body),
        };
        assert!(
            encode_frame(&oversize).is_err(),
            "fixture must exceed the frame cap untrimmed"
        );

        let req = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 1,
            active: true,
            beacon_watermark: None,
        };
        let links = select_reset_chain(&log, &req);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].dk_events.len(), 2, "trimmed to threshold");
        let mut body = Vec::new();
        ciborium::ser::into_writer(std::slice::from_ref(&links[0]), &mut body).unwrap();
        let trimmed = CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: [0x45; 8],
            body: CatchupBody::ResetChain(body),
        };
        assert!(
            encode_frame(&trimmed).is_ok(),
            "trimmed link must fit the frame cap"
        );
    }

    /// ZEB-1031 review I3: a chain longer than the per-response cap
    /// truncates to the cap, oldest-first — the remainder heals on a
    /// later round (the requester's own advancing epoch resumes past
    /// whatever was served here).
    #[test]
    fn select_reset_chain_caps_at_max_links_oldest_first_zeb1031() {
        let mut log = test_active_log(MAX_RESET_CHAIN_LINKS_PER_RESPONSE as u64 + 5);
        for i in 0..(MAX_RESET_CHAIN_LINKS_PER_RESPONSE + 3) {
            seed_reset(&mut log, [i as u8; 16], [0xAA; 32], i as u64);
        }
        let req = CatchupRequest {
            version: CATCHUP_VERSION,
            epoch: 0,
            active: false,
            beacon_watermark: None,
        };
        let links = select_reset_chain(&log, &req);
        assert_eq!(
            links.len(),
            MAX_RESET_CHAIN_LINKS_PER_RESPONSE,
            "truncated to the cap"
        );
        for (i, link) in links.iter().enumerate() {
            assert_eq!(
                link.marker.hlc.wall_ms,
                i as u64 * 1000,
                "oldest-first order preserved under the cap"
            );
        }
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

    /// ZEB-1031 review I4: the catch-up wire evolution (`CatchupBody::
    /// ResetChain` + `ResetChainLink`) had zero tests — round-trip AND
    /// the module's same-length-keys invariant, both at the frame level
    /// (`bd` → `{"rc": <bytes>}`) and inside the `ResetChainLink`
    /// struct itself (`mk`/`dk`) once its bytes are decoded.
    #[test]
    fn catchup_body_reset_chain_round_trip_and_key_shape_zeb1031() {
        let marker = test_rs_event([0x01; 16], [0xAA; 32], 1, test_hlc(1000, 0, "admin"));
        let dk = test_dk_event(2, test_owner(1), test_hlc(2000, 0, "dev1"));
        let links = vec![ResetChainLink {
            marker: marker.clone(),
            dk_events: vec![dk.clone()],
        }];
        let mut link_bytes = Vec::new();
        ciborium::ser::into_writer(&links, &mut link_bytes).unwrap();
        let frame = CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: [7u8; 8],
            body: CatchupBody::ResetChain(link_bytes),
        };
        let fbytes = encode_frame(&frame).unwrap();
        assert_eq!(decode_frame(&fbytes).unwrap(), frame, "frame round-trips");
        assert_frame_two_char_keys(&fbytes);

        // Decode the inner ResetChainLink bytes as a bare CBOR value and
        // check ITS keys too — `assert_frame_two_char_keys` only walks
        // one level into `bd`'s externally-tagged map, not the `rc`
        // variant's OWN nested bytes.
        let CatchupBody::ResetChain(inner) = &decode_frame(&fbytes).unwrap().body else {
            panic!("decoded body is not ResetChain");
        };
        let decoded_links: Vec<ResetChainLink> = ciborium::de::from_reader(&inner[..]).unwrap();
        assert_eq!(decoded_links, links, "ResetChainLink round-trips");
        let link_val: ciborium::Value = ciborium::de::from_reader(&inner[..]).unwrap();
        let link_arr = link_val.as_array().expect("links is a CBOR array");
        for link in link_arr {
            let link_map = link.as_map().expect("ResetChainLink is a map");
            let keys: Vec<&str> = link_map.iter().filter_map(|(k, _)| k.as_text()).collect();
            assert_eq!(keys.len(), 2, "ResetChainLink has exactly mk/dk");
            for k in &keys {
                assert_eq!(
                    k.len(),
                    2,
                    "ResetChainLink key {k:?} violates 2-char invariant"
                );
            }
            assert!(keys.contains(&"mk"), "marker key present");
            assert!(keys.contains(&"dk"), "dk_events key present");
        }
    }

    /// ZEB-1031 review I4 / spec §11 ("legacy no-reset communities
    /// unaffected"): a responder with an EMPTY `vk_history` (no reset
    /// has ever happened) never emits a `ResetChain` frame, for any
    /// requester shape — `catchup_respond`'s existing frame set is
    /// byte-identical to pre-ZEB-1031 in this case.
    #[test]
    fn select_catchup_no_reset_history_never_emits_reset_chain_zeb1031() {
        let mut log = test_active_log(1);
        log.insert_event_for_test(test_dk_event(1, test_owner(1), test_hlc(1000, 0, "dev1")));
        assert!(log.committee_state.vk_history.is_empty());

        for req in [
            CatchupRequest {
                version: CATCHUP_VERSION,
                epoch: 0,
                active: false,
                beacon_watermark: None,
            },
            CatchupRequest {
                version: CATCHUP_VERSION,
                epoch: 1,
                active: true,
                beacon_watermark: None,
            },
        ] {
            let sel = select_catchup(&log, &req, MAX_CATCHUP_BEACONS_PER_ROUND, 0);
            if let Some(sel) = sel {
                assert!(
                    sel.reset_chain.is_empty(),
                    "no vk_history ⇒ reset_chain always empty"
                );
            }
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
