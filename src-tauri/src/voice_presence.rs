//! ZEB-350 Voice V2 presence: ephemeral signed+sealed beacons + the live
//! roster. Beacons ride a dedicated Zenoh topic (never the CRDT); the seal
//! under `ChannelKey` gates non-members, and the device-#2 signature +
//! materialized-membership check prevents intra-member spoofing.

use crate::community_membership::ChannelId;
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use serde::{Deserialize, Serialize};

/// The unsigned presence claim. Canonical CBOR, 2-char keys (same-length
/// invariant for deterministic encoding).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoicePresenceBeacon {
    #[serde(
        rename = "ow",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub owner: [u8; 16],
    #[serde(
        rename = "dv",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub device: [u8; 32],
    #[serde(rename = "mu")]
    pub muted: bool,
    #[serde(rename = "jh")]
    pub joined_hlc: Hlc,
    #[serde(rename = "sq")]
    pub seq: u64,
    #[serde(rename = "lf", default, skip_serializing_if = "is_false")]
    pub left: bool,
    /// ZEB-612 Town Hall: wall-clock ms when this member raised their hand;
    /// absent = lowered. `skip_serializing_if` keeps a lowered-hand beacon
    /// byte-identical to the pre-ZEB-612 wire (the `left` pattern above).
    /// Mixed-fleet note: a pre-ZEB-612 receiver drops a hand-raised beacon at
    /// signature verification (its lossy re-encode omits `hd`), so the raiser
    /// vanishes from stale rosters until the hand lowers — transient (12 s
    /// TTL), contained, and the same posture `left`'s own introduction had.
    #[serde(rename = "hd", default, skip_serializing_if = "Option::is_none")]
    pub hand: Option<u64>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Beacon + detached device-#2 signature over `canonical_cbor_encode(beacon)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedVoicePresenceBeacon {
    #[serde(rename = "bc")]
    pub beacon: VoicePresenceBeacon,
    #[serde(
        rename = "sg",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
}

// Register both beacon types as `CanonicalPayload` so `canonical_cbor_encode`
// (the sealed-trait encoder) can sign/seal them. Mirrors the way
// `owner_state_crdt::OwnerState` registers its impl outside `owner_state_types`.
impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for VoicePresenceBeacon {}
impl crate::owner_state_crypto::CanonicalPayload for VoicePresenceBeacon {}
impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for SignedVoicePresenceBeacon {}
impl crate::owner_state_crypto::CanonicalPayload for SignedVoicePresenceBeacon {}
impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for GroupSignedPresenceBeacon {}
impl crate::owner_state_crypto::CanonicalPayload for GroupSignedPresenceBeacon {}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BeaconError {
    #[error("beacon CBOR encode failed")]
    Encode,
    #[error("beacon signature invalid")]
    BadSig,
    /// `session.put` failed — a transport/runtime fault, distinct from an
    /// encode/seal fault, so callers can diagnose (and one day retry) network
    /// failures without conflating them with CBOR bugs.
    #[error("beacon transport publish failed")]
    Publish,
}

use crate::community_channel_log::ChannelKey;
use crate::owner_state_crypto::canonical_cbor_encode;
use crate::voice_crypto::{decrypt_voice_packet, encrypt_voice_packet, VOICE_PRESENCE_AAD};

/// Sign a beacon with the device-#2 ed25519 key. The signature covers the
/// canonical CBOR of the unsigned beacon (sig field excluded by construction).
pub fn sign_presence_beacon(
    beacon: VoicePresenceBeacon,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<SignedVoicePresenceBeacon, BeaconError> {
    use ed25519_dalek::Signer;
    let bytes = canonical_cbor_encode(&beacon).map_err(|_| BeaconError::Encode)?;
    let sig = signing_key.sign(&bytes).to_bytes();
    Ok(SignedVoicePresenceBeacon { beacon, sig })
}

/// Verify the detached signature against the verifying key embedded in
/// `beacon.device`. This proves the holder of `device`'s private key signed
/// it; the membership check additionally requires `device ∈ owner.enrolled_device_keys`.
pub fn verify_presence_beacon_sig(signed: &SignedVoicePresenceBeacon) -> Result<(), BeaconError> {
    let bytes = canonical_cbor_encode(&signed.beacon).map_err(|_| BeaconError::Encode)?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&signed.beacon.device)
        .map_err(|_| BeaconError::BadSig)?;
    let sig = ed25519_dalek::Signature::from_bytes(&signed.sig);
    vk.verify_strict(&bytes, &sig)
        .map_err(|_| BeaconError::BadSig)
}

/// Seal a signed beacon under the channel key for transport. Output framing
/// matches the voice media packet (`[12B nonce][ct+tag]`), distinct AAD.
pub fn seal_presence_beacon(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    signed: &SignedVoicePresenceBeacon,
) -> Result<Vec<u8>, BeaconError> {
    let plain = canonical_cbor_encode(signed).map_err(|_| BeaconError::Encode)?;
    encrypt_voice_packet(key, community, channel, VOICE_PRESENCE_AAD, &plain)
        .map_err(|_| BeaconError::Encode)
}

/// Open + decode a sealed beacon. Returns `None` on any failure (drop).
pub fn open_presence_beacon(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    packet: &[u8],
) -> Option<SignedVoicePresenceBeacon> {
    let plain = decrypt_voice_packet(key, community, channel, VOICE_PRESENCE_AAD, packet).ok()?;
    ciborium::from_reader(plain.as_slice()).ok()
}

/// Deterministic-nonce seal for wire-format fixtures. NEVER call from
/// production — a fixed nonce with a reused key is catastrophic nonce reuse.
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn seal_presence_beacon_with_nonce(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    signed: &SignedVoicePresenceBeacon,
    nonce: [u8; 12],
) -> Result<Vec<u8>, BeaconError> {
    let plain = canonical_cbor_encode(signed).map_err(|_| BeaconError::Encode)?;
    crate::voice_crypto::encrypt_voice_packet_with_nonce(
        key,
        community,
        channel,
        VOICE_PRESENCE_AAD,
        &plain,
        nonce,
    )
    .map_err(|_| BeaconError::Encode)
}

/// ZEB-360: a signed presence beacon plus the `call_id` it belongs to. The
/// group presence topic is space-scoped (so a banner can discover the active
/// call without holding the `call_id`), therefore the `call_id` must ride
/// INSIDE the sealed payload. Wrapping (rather than adding a field to
/// `VoicePresenceBeacon`) keeps the community-voice beacon + its pinned fixture
/// byte-identical.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupSignedPresenceBeacon {
    #[serde(
        rename = "ci",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub call_id: [u8; 16],
    #[serde(rename = "sb")]
    pub signed: SignedVoicePresenceBeacon,
}

/// Seal a wrapped group-DM presence beacon (ZEB-360). Scopes the AEAD to
/// `space_id` under the group-DM presence key (`derive_groupdm_presence_key`).
pub fn seal_groupdm_presence_beacon(
    key: &ChannelKey,
    space_id: &SpaceId,
    wrapped: &GroupSignedPresenceBeacon,
) -> Result<Vec<u8>, BeaconError> {
    let plain = canonical_cbor_encode(wrapped).map_err(|_| BeaconError::Encode)?;
    crate::voice_crypto::encrypt_groupdm_presence_packet(
        key,
        &space_id.0,
        crate::voice_crypto::VOICE_GROUPDM_PRESENCE_AAD,
        &plain,
    )
    .map_err(|_| BeaconError::Encode)
}

/// Open + decode a sealed wrapped group-DM presence beacon. Returns `None` on
/// any failure (drop), matching [`open_presence_beacon`].
pub fn open_groupdm_presence_beacon(
    key: &ChannelKey,
    space_id: &SpaceId,
    packet: &[u8],
) -> Option<GroupSignedPresenceBeacon> {
    let plain = crate::voice_crypto::decrypt_groupdm_presence_packet(
        key,
        &space_id.0,
        crate::voice_crypto::VOICE_GROUPDM_PRESENCE_AAD,
        packet,
    )
    .ok()?;
    ciborium::from_reader(plain.as_slice()).ok()
}

/// Deterministic-nonce group-DM presence seal for wire-format fixtures. NEVER
/// call from production — a fixed nonce with a reused key is catastrophic nonce
/// reuse.
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn seal_groupdm_presence_beacon_with_nonce(
    key: &ChannelKey,
    space_id: &SpaceId,
    wrapped: &GroupSignedPresenceBeacon,
    nonce: [u8; 12],
) -> Result<Vec<u8>, BeaconError> {
    let plain = canonical_cbor_encode(wrapped).map_err(|_| BeaconError::Encode)?;
    crate::voice_crypto::encrypt_groupdm_presence_packet_with_nonce(
        key,
        &space_id.0,
        crate::voice_crypto::VOICE_GROUPDM_PRESENCE_AAD,
        &plain,
        nonce,
    )
    .map_err(|_| BeaconError::Encode)
}

use std::collections::BTreeMap;

/// Serialize a 16-byte id as a lowercase hex string (JSON-friendly — the
/// roster is emitted to the frontend as a `voice-presence-changed` payload).
fn ser_hex_16<S: serde::Serializer>(b: &[u8; 16], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&hex::encode(b))
}

/// Serialize a 32-byte id as a lowercase hex string (see [`ser_hex_16`]).
fn ser_hex_32<S: serde::Serializer>(b: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&hex::encode(b))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceEntry {
    pub owner: [u8; 16],
    pub muted: bool,
    /// ZEB-612: wall-clock ms of the member's raised hand (None = lowered).
    /// Peer-supplied and attacker-controlled — display-only; NEVER an ordering
    /// key (see `hand_first_observed_ms`).
    pub hand: Option<u64>,
    /// ZEB-853 D5: LOCAL monotonic-ms timestamp of when THIS node first observed
    /// the current raised hand (None = lowered). The peer `hand` wall stamp is
    /// attacker-controlled — a hostile peer can claim `hand=1` (epoch) to squat
    /// the front of the speaker queue forever — so the derived queue orders on
    /// this local stamp instead. Stamped once on the lowered→raised transition,
    /// stable across heartbeats while raised, cleared on lower, re-stamped on a
    /// fresh raise.
    pub hand_first_observed_ms: Option<u64>,
    pub seq: u64,
    pub joined_hlc: Hlc,
    /// Monotonic-ms timestamp of the last applied beacon (injected by caller).
    pub last_seen_ms: u64,
    /// True once a `left` tombstone has been applied: the row is hidden from the
    /// roster but retained as a freshness *gravestone* (the `joined_hlc` + `seq`
    /// barrier), so a same-session heartbeat the network reordered to arrive
    /// after the tombstone can't resurrect the member before the TTL sweep GCs
    /// it. A strictly-newer `joined_hlc` (a genuine rejoin) revives the row.
    pub left: bool,
}

/// One roster row surfaced to the frontend. `owner`/`device` are kept as raw
/// byte arrays for byte-level Rust assertions, but serialize as hex strings so
/// the JSON `voice-presence-changed` payload is clean (no CBOR bstr-in-JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RosterEntry {
    #[serde(serialize_with = "ser_hex_16")]
    pub owner: [u8; 16],
    #[serde(serialize_with = "ser_hex_32")]
    pub device: [u8; 32],
    pub muted: bool,
    /// ZEB-612: JSON `handRaisedAt` — peer-supplied wall-clock ms of the raised
    /// hand, or null when lowered. Display-only (the ✋ badge); the speaker
    /// queue orders on `hand_first_observed_ms`, not this.
    pub hand_raised_at: Option<u64>,
    /// ZEB-853 D5: JSON `handFirstObservedMs` — LOCAL first-observed monotonic ms
    /// of the current raised hand (null when lowered). The derived speaker queue
    /// orders on THIS (DoS-proof against a squatted `handRaisedAt`), falling back
    /// to `handRaisedAt` only if somehow absent.
    pub hand_first_observed_ms: Option<u64>,
}

/// One row returned by [`VoicePresenceMap::sweep`]: the `(community, channel)`
/// key plus the `(owner, device)` of an evicted entry. The caller (the
/// event-loop sweep arm) re-emits the affected channel's roster.
pub type SweptEntry = ((SpaceId, ChannelId), [u8; 16], [u8; 32]);

#[derive(Debug, Default)]
pub struct VoicePresenceMap {
    // (community, channel) → device → entry
    inner: BTreeMap<(SpaceId, ChannelId), BTreeMap<[u8; 32], PresenceEntry>>,
}

/// ZEB-853 D5: compute the next `hand_first_observed_ms` for an entry from its
/// stored hand state and an incoming beacon's hand, using the injected LOCAL
/// monotonic `now_ms`. Ordering the speaker queue on this local stamp (rather
/// than the peer-controlled `hand` wall value) makes queue position DoS-proof:
/// a hostile `hand=1` can no longer pre-date honest raises. Stamp once on
/// lowered→raised, hold stable while raised, clear on lower.
fn next_hand_first_observed(
    stored_hand: Option<u64>,
    stored_first_observed: Option<u64>,
    incoming_hand: Option<u64>,
    now_ms: u64,
) -> Option<u64> {
    match incoming_hand {
        None => None,                                     // lowered → clear
        Some(_) if stored_hand.is_none() => Some(now_ms), // fresh raise → stamp
        Some(_) => stored_first_observed,                 // still raised → stable
    }
}

impl VoicePresenceMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a (verified, opened) beacon. `now_ms` is a monotonic clock the
    /// caller supplies. Returns true if the roster changed.
    pub fn apply(
        &mut self,
        c: &SpaceId,
        ch: &ChannelId,
        beacon: &VoicePresenceBeacon,
        now_ms: u64,
    ) -> bool {
        let chan = self.inner.entry((*c, *ch)).or_default();
        // Freshness is keyed PRIMARILY on `joined_hlc` (the signed join-session
        // identifier), with `seq` only the tiebreak WITHIN a session.
        // `spawn_voice_presence_publisher` restarts `seq` at 0 on every
        // (re)join, so a seq-primary rule rejects the new session's beacons as
        // stale until the 12 s TTL evicts the old higher-seq entry — breaking
        // roster convergence on exactly the rejoin / key-rotation path. Keying
        // on `joined_hlc` lets a newer session supersede regardless of seq.
        if beacon.left {
            // Tombstone. Convert the live row into a freshness *gravestone*
            // rather than deleting it: a same-session heartbeat the network
            // reordered to arrive AFTER this tombstone must not recreate the
            // member (deleting outright would let the `None` arm below resurrect
            // it). The gravestone keeps the `joined_hlc` + (u64::MAX) `seq`
            // barrier, stays hidden from the roster, and the TTL sweep GCs it
            // ~12 s later. A strictly-newer `joined_hlc` (a genuine rejoin)
            // revives it at once.
            return match chan.get_mut(&beacon.device) {
                // Stored row is from a strictly-newer session → ignore a stale
                // old-session tombstone (must not evict a freshly-rejoined row).
                Some(e) if e.joined_hlc.is_strictly_newer_than(&beacon.joined_hlc) => false,
                // Already a gravestone → advance the barrier to this tombstone's
                // session. Older-session tombstones were filtered by the arm
                // above, so this is same-or-newer; updating `joined_hlc` keeps
                // the barrier pinned to the NEWEST departed session. Without it,
                // a reordered heartbeat from that newer session would pass the
                // strictly-newer check below and resurrect a member who already
                // left. Roster unchanged (still hidden).
                Some(e) if e.left => {
                    e.joined_hlc = beacon.joined_hlc.clone();
                    e.seq = beacon.seq;
                    e.last_seen_ms = now_ms;
                    false
                }
                // Live row → bury it. The roster shrinks, so report a change.
                Some(e) => {
                    e.muted = beacon.muted;
                    e.hand_first_observed_ms = next_hand_first_observed(
                        e.hand,
                        e.hand_first_observed_ms,
                        beacon.hand,
                        now_ms,
                    );
                    e.hand = beacon.hand;
                    e.seq = beacon.seq;
                    e.joined_hlc = beacon.joined_hlc.clone();
                    e.last_seen_ms = now_ms;
                    e.left = true;
                    true
                }
                // No row yet → install the gravestone so a later reordered
                // heartbeat is rejected. Nothing was visible → no roster change.
                None => {
                    chan.insert(
                        beacon.device,
                        PresenceEntry {
                            owner: beacon.owner,
                            muted: beacon.muted,
                            hand: beacon.hand,
                            hand_first_observed_ms: next_hand_first_observed(
                                None,
                                None,
                                beacon.hand,
                                now_ms,
                            ),
                            seq: beacon.seq,
                            joined_hlc: beacon.joined_hlc.clone(),
                            last_seen_ms: now_ms,
                            left: true,
                        },
                    );
                    false
                }
            };
        }
        match chan.get_mut(&beacon.device) {
            Some(e) if beacon.joined_hlc.is_strictly_newer_than(&e.joined_hlc) => {
                // A NEW join session supersedes regardless of seq (seq restarts
                // at 0 on rejoin) — and revives a gravestone.
                e.owner = beacon.owner;
                e.muted = beacon.muted;
                // ZEB-853 D5 (deliberate carry): if the superseding session's
                // hand is STILL raised, the LOCAL first-observed stamp is
                // intentionally carried forward. The stamp is local (never
                // peer-controlled), so it cannot be forged back to epoch to
                // steal queue position. A genuine leave→rejoin clears it first
                // via the tombstone's `hand: None`, then re-stamps fresh here.
                e.hand_first_observed_ms =
                    next_hand_first_observed(e.hand, e.hand_first_observed_ms, beacon.hand, now_ms);
                e.hand = beacon.hand;
                e.seq = beacon.seq;
                e.joined_hlc = beacon.joined_hlc.clone();
                e.last_seen_ms = now_ms;
                e.left = false;
                true
            }
            Some(e) if e.joined_hlc.is_strictly_newer_than(&beacon.joined_hlc) => false, // older session → stale
            Some(e) if e.left => false, // same session, buried → reject until TTL GC or a newer session
            Some(e) if beacon.seq <= e.seq => false, // same session, stale or duplicate seq
            Some(e) => {
                // Same session, newer seq: advance liveness, but only report a
                // roster-VISIBLE change when `muted` or `hand` actually flipped
                // (both surface on RosterEntry). A bare seq/last_seen advance is
                // invisible to the frontend, so emitting on it spams
                // `voice-presence-changed` once per heartbeat per peer — including
                // our own beacon echoed back on the shared Zenoh session.
                // `last_seen_ms` is updated regardless so the TTL sweep still
                // sees liveness.
                let visible_changed = e.muted != beacon.muted || e.hand != beacon.hand;
                e.muted = beacon.muted;
                e.hand_first_observed_ms =
                    next_hand_first_observed(e.hand, e.hand_first_observed_ms, beacon.hand, now_ms);
                e.hand = beacon.hand;
                e.seq = beacon.seq;
                e.last_seen_ms = now_ms;
                visible_changed
            }
            None => {
                chan.insert(
                    beacon.device,
                    PresenceEntry {
                        owner: beacon.owner,
                        muted: beacon.muted,
                        hand: beacon.hand,
                        hand_first_observed_ms: next_hand_first_observed(
                            None,
                            None,
                            beacon.hand,
                            now_ms,
                        ),
                        seq: beacon.seq,
                        joined_hlc: beacon.joined_hlc.clone(),
                        last_seen_ms: now_ms,
                        left: false,
                    },
                );
                true
            }
        }
    }

    /// Evict entries whose last beacon is older than `ttl_ms`. Returns the
    /// `(community, channel)` key plus the `(owner, device)` of each evicted
    /// entry, so the caller can re-emit the affected channel's roster.
    pub fn sweep(&mut self, now_ms: u64, ttl_ms: u64) -> Vec<SweptEntry> {
        let mut evicted = Vec::new();
        for (key, chan) in self.inner.iter_mut() {
            chan.retain(|device, e| {
                // ZEB-791: see `community_presence::sweep` — forward bound so a
                // backward local clock step cannot make a row immortal.
                let alive = now_ms >= e.last_seen_ms && now_ms - e.last_seen_ms < ttl_ms;
                if !alive && !e.left {
                    // Only a VISIBLE row's eviction changes the roster; a
                    // gravestone is already hidden, so its GC is silent.
                    evicted.push((*key, e.owner, *device));
                }
                alive
            });
        }
        // Reclaim channel sub-maps emptied by eviction so they don't accumulate
        // as ghost entries that every future sweep re-scans, across a long
        // session that visits many channels (Greptile P2 / Cursor).
        self.inner.retain(|_, chan| !chan.is_empty());
        evicted
    }

    pub fn roster(&self, c: &SpaceId, ch: &ChannelId) -> Vec<RosterEntry> {
        self.inner
            .get(&(*c, *ch))
            .map(|chan| {
                chan.iter()
                    .filter(|(_, e)| !e.left) // gravestones are hidden
                    .map(|(device, e)| RosterEntry {
                        owner: e.owner,
                        device: *device,
                        muted: e.muted,
                        hand_raised_at: e.hand,
                        hand_first_observed_ms: e.hand_first_observed_ms,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Drop a channel's entire roster. Called on `Leave` so the periodic sweep
    /// stops emitting `voice-presence-changed` for a channel the user has left
    /// (otherwise stale entries linger up to the 12 s TTL) and the empty
    /// sub-map is reclaimed rather than accumulating across channel visits.
    pub fn remove_channel(&mut self, c: &SpaceId, ch: &ChannelId) {
        self.inner.remove(&(*c, *ch));
    }

    /// Drop EVERY roster sub-map whose key's `SpaceId` is `space`, regardless of
    /// channel/call id. Called on `UnwatchGroupCall` so stale `(space, call)`
    /// rows for subscriber-discovered calls (those we never published into, so
    /// `remove_channel` can't reach them) are cleared rather than lingering to
    /// the TTL. The inner map is keyed by `(SpaceId, ChannelId)`; retaining only
    /// the rows whose space differs also reclaims the emptied sub-maps.
    pub fn remove_space(&mut self, space: &SpaceId) {
        self.inner.retain(|(s, _), _| s != space);
    }

    /// Resolve the owner for a device currently known in (c, ch), for media-drop
    /// resolution (works even for entries hidden from the visible roster, i.e.
    /// gravestones, since this looks at the raw entry map, not `roster()`).
    pub fn owner_for_device(
        &self,
        c: &SpaceId,
        ch: &ChannelId,
        device: &[u8; 32],
    ) -> Option<[u8; 16]> {
        self.inner
            .get(&(*c, *ch))
            .and_then(|m| m.get(device))
            .map(|e| e.owner)
    }
}

// ── Membership verification + pub/sub spawn helpers ──────────────

use crate::community_state_sync::CommunitySyncRegistry;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::task::JoinHandle;

/// Pure membership predicate over a resolved `MaterializedMembership`:
/// `(owner, device)` must map to a currently-`Joined` member whose
/// `enrolled_device_keys` contains `device`. Factored out of
/// [`beacon_signer_is_member`] so it is unit-testable without standing up a
/// `CommunitySyncRegistry`, and reusable by the two-engine integration test.
pub fn device_is_enrolled(
    materialized: &crate::community_membership::MaterializedMembership,
    owner: &OwnerAddr,
    device: &[u8; 32],
) -> bool {
    materialized.members.get(owner).is_some_and(|m| {
        m.status == crate::community_membership::MemberStatus::Joined
            && m.enrolled_device_keys.contains(device)
    })
}

/// Resolve whether `(owner, device)` is an enrolled, currently-Joined member
/// of `community` per materialized membership (the ZEB-339 norm). Cheap:
/// `materialized()` returns an owned clone keyed off the engine's cached admin
/// addr. Returns false on any missing piece (no engine, owner absent, device
/// not enrolled, status != Joined) — the caller drops the beacon.
pub async fn beacon_signer_is_member(
    registry: &Arc<CommunitySyncRegistry>,
    community: &SpaceId,
    owner: &OwnerAddr,
    device: &[u8; 32],
) -> bool {
    let Some(engine) = registry.engine_arc(community).await else {
        return false;
    };
    let admin = engine.admin_addr();
    // `materialized(&self) -> MaterializedMembership` returns an owned clone,
    // so the owned value outlives the dropped guard. Bind the `Arc<Mutex<_>>`
    // first — `engine.state()` returns it by value, so locking it inline would
    // drop the temporary while the guard still borrows it.
    let state = engine.state();
    let materialized = {
        let guard = state.lock().await;
        guard.materialized(admin)
    };
    device_is_enrolled(&materialized, owner, device)
}

/// Spawn a presence subscriber on `topic`: open the seal → verify the device-#2
/// signature → verify materialized membership → apply to the shared map →
/// emit `voice-presence-changed` on change. Drops on any failure. Mirrors the
/// channel-log subscriber idiom (`recv_async` → `payload().to_bytes()`).
///
/// `session` is an owned, cheaply-cloned `zenoh::Session` (call sites pass
/// `session.clone()`); `now_ms` is the loop's monotonic clock.
#[allow(clippy::too_many_arguments)]
pub fn spawn_voice_presence_subscriber(
    session: zenoh::Session,
    topic: String,
    channel_key: Arc<ChannelKey>,
    community: SpaceId,
    channel: ChannelId,
    registry: Arc<CommunitySyncRegistry>,
    map: Arc<tokio::sync::Mutex<VoicePresenceMap>>,
    app: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    closing: Arc<AtomicBool>,
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let sub = match session.declare_subscriber(&topic).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(%topic, err = %e, "presence subscribe failed");
                return;
            }
        };
        while let Ok(sample) = sub.recv_async().await {
            if sample.payload().len() > crate::voice_crypto::MAX_VOICE_PACKET_BYTES {
                tracing::warn!(
                    len = sample.payload().len(),
                    max = crate::voice_crypto::MAX_VOICE_PACKET_BYTES,
                    "oversized voice packet dropped"
                );
                continue;
            }
            let bytes = sample.payload().to_bytes().to_vec();
            let Some(signed) = open_presence_beacon(&channel_key, &community, &channel, &bytes)
            else {
                continue; // non-member key / wrong scope / tamper → drop
            };
            if verify_presence_beacon_sig(&signed).is_err() {
                continue; // bad device-#2 signature → drop
            }
            let owner = OwnerAddr(signed.beacon.owner);
            if !beacon_signer_is_member(&registry, &community, &owner, &signed.beacon.device).await
            {
                continue; // signer not an enrolled, Joined member → drop
            }
            let changed = {
                let mut g = map.lock().await;
                g.apply(&community, &channel, &signed.beacon, (now_ms)())
            };
            if changed {
                let roster = {
                    let g = map.lock().await;
                    g.roster(&community, &channel)
                };
                crate::node_event_sink::emit_ser(
                    app.as_ref(),
                    "voice-presence-changed",
                    &serde_json::json!({
                        "community": hex::encode(community.0),
                        "channel": hex::encode(channel.0),
                        "roster": roster,
                    }),
                );
            }
        }
        if !closing.load(Ordering::SeqCst) {
            tracing::warn!(%topic, "presence subscriber closed unexpectedly");
        }
    })
}

/// ZEB-351 Voice V3: build the heartbeat beacon for the current mute state.
/// Pure (no I/O) so the publisher loop and the one-shot immediate-beacon path
/// share exactly one construction site, and the mute-tracking is unit-testable
/// (and integration-testable) without standing up Zenoh.
pub fn build_heartbeat_beacon(
    self_owner: OwnerAddr,
    self_device: [u8; 32],
    joined_hlc: &Hlc,
    seq: u64,
    muted: bool,
    hand: Option<u64>,
) -> VoicePresenceBeacon {
    VoicePresenceBeacon {
        owner: self_owner.0,
        device: self_device,
        muted,
        joined_hlc: joined_hlc.clone(),
        seq,
        left: false,
        hand,
    }
}

/// ZEB-351 Voice V3: publish a single heartbeat beacon immediately (build →
/// sign → seal → `session.put`). The event loop calls this on a `SetMuted`
/// toggle so the roster reflects the new mute state without waiting out the
/// next ≤4 s heartbeat. Best-effort: returns `Err` on sign/seal/put failure so
/// the caller can log without blocking the flag flip itself.
#[allow(clippy::too_many_arguments)]
pub async fn publish_presence_once(
    session: &zenoh::Session,
    topic: &str,
    channel_key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    signing_key: &ed25519_dalek::SigningKey,
    self_owner: OwnerAddr,
    self_device: [u8; 32],
    joined_hlc: &Hlc,
    seq: u64,
    muted: bool,
    hand: Option<u64>,
) -> Result<(), BeaconError> {
    let beacon = build_heartbeat_beacon(self_owner, self_device, joined_hlc, seq, muted, hand);
    let signed = sign_presence_beacon(beacon, signing_key)?;
    let sealed = seal_presence_beacon(channel_key, community, channel, &signed)?;
    session
        .put(topic, sealed)
        .await
        .map_err(|_| BeaconError::Publish)
}

/// Spawn a heartbeat publisher: emit an immediate beacon, then one every
/// `interval` (4 s in V2). ZEB-351 Voice V3: each tick reads the shared
/// `muted` flag (flipped by `set_voice_muted` → `VoiceChannelRequest::SetMuted`)
/// instead of the old hardcoded `true`. Honors `closing` for shutdown.
///
/// `seq_counter` is a per-(community, channel) monotone `Arc<AtomicU64>` SHARED
/// with the event loop's immediate-beacon path (`publish_presence_once` on a
/// `SetMuted` toggle). Each beacon — heartbeat OR immediate — draws a strictly
/// increasing `seq` via `fetch_add`, so the immediate mute beacon can never
/// outrank later heartbeats (the bug a private `0..` counter + a `u64::MAX-1`
/// sentinel produced: the sentinel permanently poisoned same-session freshness,
/// stalling the roster). `u64::MAX` stays reserved for the leave tombstone.
///
/// `self_kicked` is a per-(community, channel) `Arc<AtomicBool>` SHARED with the
/// control sub / sweep tick. While set, the publisher skips publishing so peers
/// presence-evict this kicked client (ZEB-358).
///
/// `session` is an owned, cheaply-cloned `zenoh::Session`.
#[allow(clippy::too_many_arguments)]
pub fn spawn_voice_presence_publisher(
    session: zenoh::Session,
    topic: String,
    channel_key: Arc<ChannelKey>,
    community: SpaceId,
    channel: ChannelId,
    signing_key: Arc<ed25519_dalek::SigningKey>,
    self_owner: OwnerAddr,
    self_device: [u8; 32],
    joined_hlc: Hlc,
    muted: Arc<AtomicBool>,
    hand_raised_at: Arc<AtomicU64>,
    self_kicked: Arc<AtomicBool>,
    seq_counter: Arc<AtomicU64>,
    interval: std::time::Duration,
    closing: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // `interval` fires immediately on the first `tick()`, giving us the
        // "immediate first beacon then every 4 s" cadence for free.
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            if closing.load(Ordering::SeqCst) {
                break;
            }
            // ZEB-358 (Cursor HIGH): a kicked owner must stop advertising
            // presence so peers presence-evict us (the 12 s TTL backstops; the FE
            // overlay hides us immediately). Skip publishing while the per-channel
            // `self_kicked` flag is set — set by the control sub / sweep tick from
            // the moderation map. We keep ticking (no break) so a later
            // un-kick within the same session resumes beacons.
            if self_kicked.load(Ordering::Relaxed) {
                continue;
            }
            let seq = seq_counter.fetch_add(1, Ordering::SeqCst);
            // ZEB-612: the shared hand cell uses 0 as the "lowered" sentinel
            // (wall-clock ms is never 0); each heartbeat republishes it.
            let hr = hand_raised_at.load(Ordering::SeqCst);
            let beacon = build_heartbeat_beacon(
                self_owner,
                self_device,
                &joined_hlc,
                seq,
                muted.load(Ordering::SeqCst),
                (hr != 0).then_some(hr),
            );
            let Ok(signed) = sign_presence_beacon(beacon, &signing_key) else {
                continue;
            };
            let Ok(sealed) = seal_presence_beacon(&channel_key, &community, &channel, &signed)
            else {
                continue;
            };
            if let Err(e) = session.put(&topic, sealed).await {
                tracing::warn!(%topic, err = %e, "presence publish failed");
            }
        }
    })
}

/// ZEB-612: apply a raise/lower to the shared per-channel hand cell and return
/// the value the immediate beacon should carry. Raising keeps the ORIGINAL
/// timestamp if already raised (queue position must be stable across repeat
/// raises); lowering always resets. 0 is the "lowered" sentinel — wall-clock
/// ms is never 0. Pure over the cell so the stamp-once rule is unit-testable
/// without the event loop.
pub fn update_hand_cell(cell: &AtomicU64, raised_at: Option<u64>) -> Option<u64> {
    match raised_at {
        Some(ts) => {
            let prev = cell.load(Ordering::SeqCst);
            if prev == 0 {
                cell.store(ts, Ordering::SeqCst);
                Some(ts)
            } else {
                Some(prev)
            }
        }
        None => {
            cell.store(0, Ordering::SeqCst);
            None
        }
    }
}

/// Build + sign + seal a single `left=true` tombstone for instant roster
/// removal on leave. `seq = u64::MAX` so it always wins freshness, and `left`
/// short-circuits ordering in `apply` anyway. Returns `None` on encode failure.
pub fn build_presence_tombstone(
    channel_key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    signing_key: &ed25519_dalek::SigningKey,
    self_owner: OwnerAddr,
    self_device: [u8; 32],
    joined_hlc: Hlc,
) -> Option<Vec<u8>> {
    let beacon = VoicePresenceBeacon {
        owner: self_owner.0,
        device: self_device,
        muted: true,
        joined_hlc,
        seq: u64::MAX,
        left: true,
        hand: None,
    };
    let signed = sign_presence_beacon(beacon, signing_key).ok()?;
    seal_presence_beacon(channel_key, community, channel, &signed).ok()
}

// ── ZEB-360: group-DM presence pub/sub + membership ─────────────

/// ZEB-360: a group-DM presence beacon is admissible iff its signer
/// (`owner` + 32-byte Ed25519 `device` key) is an enrolled device of a current
/// member of `space_id`. The CRDT `OwnerState` is the authority here (mirrors
/// the voice-signal invite handler in `event_loop.rs`, which reads exactly
/// these field paths): `spaces[space_id].members` gates owner-membership, and
/// `owner_device_cache.devices[owner].device_identity_pubs` carries the
/// enrolled devices. Each cached identity_pub is `[X25519(32) || Ed25519(32)]`,
/// so the beacon `device` (the Ed25519 half) is compared against bytes
/// `[32..64]`. Returns false on any missing piece (space absent, owner not a
/// member, no cached devices, device not enrolled) — the caller drops the
/// beacon.
pub async fn groupdm_beacon_signer_is_member(
    crdt_state: &tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>,
    space_id: &SpaceId,
    owner: &OwnerAddr,
    device: &[u8; 32],
) -> bool {
    let os = crdt_state.lock().await;
    let Some(space) = os.spaces.get(space_id) else {
        return false;
    };
    if !space.members.contains(owner) {
        return false;
    }
    let Some(entry) = os.owner_device_cache.devices.get(owner) else {
        return false;
    };
    // `device_identity_pubs` is `Vec<Option<[u8; 64]>>`; `.flatten()` yields
    // `&[u8; 64]` only over the populated (`Some`) slots — a known-by-hash
    // device whose pub hasn't propagated yet contributes nothing (matches the
    // invite handler's `.filter_map(|p| *p)`).
    entry
        .device_identity_pubs
        .iter()
        .flatten()
        .any(|ip| &ip[32..64] == device.as_slice())
}

/// ZEB-360: spawn a group-DM presence heartbeat publisher. Build → sign → wrap
/// (with `call_id`) → seal under the group-DM presence key (scoped to
/// `space_id`) → `session.put` every `interval`, drawing a strictly-increasing
/// `seq` from the SHARED `seq_counter` (so an immediate mute beacon can never
/// outrank later heartbeats; `u64::MAX` is reserved for the leave tombstone).
/// Honors `closing` for shutdown. Mirrors [`spawn_voice_presence_publisher`].
#[allow(clippy::too_many_arguments)]
pub fn spawn_groupdm_presence_publisher(
    session: zenoh::Session,
    topic: String,
    presence_key: Arc<ChannelKey>,
    space_id: SpaceId,
    call_id: [u8; 16],
    signing_key: Arc<ed25519_dalek::SigningKey>,
    self_owner: OwnerAddr,
    self_device: [u8; 32],
    joined_hlc: Hlc,
    muted: Arc<AtomicBool>,
    seq_counter: Arc<AtomicU64>,
    interval: std::time::Duration,
    closing: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // `interval` fires immediately on the first `tick()` for the
        // "immediate first beacon then every `interval`" cadence.
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            if closing.load(Ordering::SeqCst) {
                break;
            }
            let seq = seq_counter.fetch_add(1, Ordering::SeqCst);
            // Group-DM calls have no raise-hand semantics (ZEB-612 is
            // community town halls only) → hand is always None here.
            let beacon = build_heartbeat_beacon(
                self_owner,
                self_device,
                &joined_hlc,
                seq,
                muted.load(Ordering::SeqCst),
                None,
            );
            let Ok(signed) = sign_presence_beacon(beacon, &signing_key) else {
                continue;
            };
            let wrapped = GroupSignedPresenceBeacon { call_id, signed };
            let Ok(sealed) = seal_groupdm_presence_beacon(&presence_key, &space_id, &wrapped)
            else {
                continue;
            };
            if let Err(e) = session.put(&topic, sealed).await {
                tracing::warn!(%topic, err = %e, "group presence publish failed");
            }
        }
    })
}

/// ZEB-360: publish a single `left` tombstone so peers evict us immediately
/// rather than waiting out the TTL. `seq = u64::MAX` so it always wins freshness
/// within the session, and `left` short-circuits ordering in
/// [`VoicePresenceMap::apply`] anyway. Best-effort (drops on sign/seal/put
/// failure). Mirrors [`build_presence_tombstone`] for the group-DM wire shape.
#[allow(clippy::too_many_arguments)]
pub async fn publish_groupdm_leave_tombstone(
    session: &zenoh::Session,
    topic: &str,
    presence_key: &ChannelKey,
    space_id: &SpaceId,
    call_id: [u8; 16],
    signing_key: &ed25519_dalek::SigningKey,
    self_owner: OwnerAddr,
    self_device: [u8; 32],
    joined_hlc: &Hlc,
) {
    let mut beacon =
        build_heartbeat_beacon(self_owner, self_device, joined_hlc, u64::MAX, true, None);
    beacon.left = true;
    let Ok(signed) = sign_presence_beacon(beacon, signing_key) else {
        return;
    };
    let wrapped = GroupSignedPresenceBeacon { call_id, signed };
    if let Ok(sealed) = seal_groupdm_presence_beacon(presence_key, space_id, &wrapped) {
        let _ = session.put(topic, sealed).await;
    }
}

/// ZEB-360: spawn a group-DM presence subscriber on `topic`: open the seal
/// (scoped to `space_id` under the group-DM presence key) → verify the device-#2
/// signature → verify CRDT-backed group membership → apply to the shared map →
/// emit `group-call-presence-changed` on change. Drops on any failure. Reuses
/// [`VoicePresenceMap`] keyed by `(space_id, ChannelId(call_id))` so
/// `apply`/`roster` are shared verbatim with community voice. Mirrors
/// [`spawn_voice_presence_subscriber`].
#[allow(clippy::too_many_arguments)]
pub fn spawn_groupdm_presence_subscriber(
    session: zenoh::Session,
    topic: String,
    presence_key: Arc<ChannelKey>,
    space_id: SpaceId,
    crdt_state: Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    map: Arc<tokio::sync::Mutex<VoicePresenceMap>>,
    app: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    closing: Arc<AtomicBool>,
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let sub = match session.declare_subscriber(&topic).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(%topic, err = %e, "group presence subscribe failed");
                return;
            }
        };
        while let Ok(sample) = sub.recv_async().await {
            if sample.payload().len() > crate::voice_crypto::MAX_VOICE_PACKET_BYTES {
                tracing::warn!(
                    len = sample.payload().len(),
                    max = crate::voice_crypto::MAX_VOICE_PACKET_BYTES,
                    "oversized group voice packet dropped"
                );
                continue;
            }
            let bytes = sample.payload().to_bytes().to_vec();
            let Some(wrapped) = open_groupdm_presence_beacon(&presence_key, &space_id, &bytes)
            else {
                continue; // wrong key / wrong scope / tamper → drop
            };
            if verify_presence_beacon_sig(&wrapped.signed).is_err() {
                continue; // bad device-#2 signature → drop
            }
            let owner = OwnerAddr(wrapped.signed.beacon.owner);
            if !groupdm_beacon_signer_is_member(
                &crdt_state,
                &space_id,
                &owner,
                &wrapped.signed.beacon.device,
            )
            .await
            {
                continue; // signer not an enrolled device of a current member → drop
            }
            let call_chan = ChannelId(wrapped.call_id);
            let changed = {
                let mut g = map.lock().await;
                g.apply(&space_id, &call_chan, &wrapped.signed.beacon, (now_ms)())
            };
            if changed {
                let roster = {
                    let g = map.lock().await;
                    g.roster(&space_id, &call_chan)
                };
                crate::node_event_sink::emit_ser(
                    app.as_ref(),
                    "group-call-presence-changed",
                    &serde_json::json!({
                        "spaceId": hex::encode(space_id.0),
                        "callId": hex::encode(wrapped.call_id),
                        "roster": roster,
                    }),
                );
            }
        }
        if !closing.load(Ordering::SeqCst) {
            tracing::warn!(%topic, "group presence subscriber closed unexpectedly");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn beacon(seq: u64) -> VoicePresenceBeacon {
        VoicePresenceBeacon {
            owner: [0xa1; 16],
            device: [0u8; 32], // overwritten by sign helper's caller in real use
            muted: true,
            joined_hlc: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "aa".repeat(32),
            },
            seq,
            left: false,
            hand: None,
        }
    }

    #[test]
    fn heartbeat_beacon_tracks_mute_flag() {
        let hlc = Hlc {
            wall_ms: 1,
            logical: 0,
            device_id: "aa".repeat(32),
        };
        let owner = OwnerAddr([7u8; 16]);
        assert!(build_heartbeat_beacon(owner, [1u8; 32], &hlc, 0, true, None).muted);
        assert!(!build_heartbeat_beacon(owner, [1u8; 32], &hlc, 1, false, None).muted);
    }

    #[test]
    fn heartbeat_beacon_carries_hand() {
        let hlc = Hlc {
            wall_ms: 1,
            logical: 0,
            device_id: "aa".repeat(32),
        };
        let owner = OwnerAddr([7u8; 16]);
        assert_eq!(
            build_heartbeat_beacon(owner, [1u8; 32], &hlc, 0, true, Some(42)).hand,
            Some(42)
        );
        assert_eq!(
            build_heartbeat_beacon(owner, [1u8; 32], &hlc, 1, true, None).hand,
            None
        );
    }

    #[test]
    fn update_hand_cell_stamps_once_and_lower_resets() {
        use std::sync::atomic::AtomicU64;
        let cell = AtomicU64::new(0);
        // First raise stamps.
        assert_eq!(update_hand_cell(&cell, Some(500)), Some(500));
        // Repeat raise keeps the ORIGINAL stamp (stable queue position).
        assert_eq!(update_hand_cell(&cell, Some(900)), Some(500));
        // Lower resets.
        assert_eq!(update_hand_cell(&cell, None), None);
        assert_eq!(cell.load(Ordering::SeqCst), 0);
        // Raise after lower restamps.
        assert_eq!(update_hand_cell(&cell, Some(900)), Some(900));
    }

    #[test]
    fn next_hand_first_observed_covers_all_arms() {
        // ZEB-853 D5: direct coverage of the pure queue-stamp helper (the
        // supersede-arm apply path only ever routes through these branches).
        // Local `now_ms` is fixed so the fresh stamp is deterministic.
        let now = 5_000u64;

        // (1) Incoming lowered → clear, regardless of stored hand/stamp.
        assert_eq!(
            next_hand_first_observed(Some(100), Some(200), None, now),
            None
        );
        assert_eq!(next_hand_first_observed(None, None, None, now), None);

        // (2) Incoming raised & stored hand lowered → fresh stamp at `now`.
        assert_eq!(
            next_hand_first_observed(None, None, Some(1), now),
            Some(now)
        );

        // (3) Incoming raised & stored hand raised WITH an existing stamp →
        // carry the existing stamp unchanged (stable queue position).
        assert_eq!(
            next_hand_first_observed(Some(1), Some(200), Some(1), now),
            Some(200)
        );

        // (4) Incoming raised & stored hand raised but stamp MISSING — a
        // defensive/should-be-unreachable state (every path that sets
        // `hand = Some` also stamps). The current helper CARRIES the stored
        // (None) stamp; it does NOT re-stamp to `now`. Asserting the real
        // return keeps this a no-behavior-change coverage backfill. (The
        // fixwave brief anticipated Some(now) here — a defensive fresh stamp
        // the helper does not implement; flagged as a concern, not changed.)
        assert_eq!(next_hand_first_observed(Some(1), None, Some(1), now), None);
    }

    #[test]
    fn sign_then_verify_round_trips() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(1);
        b.device = sk.verifying_key().to_bytes();
        let signed = sign_presence_beacon(b.clone(), &sk).unwrap();
        assert_eq!(signed.beacon, b);
        verify_presence_beacon_sig(&signed).expect("valid sig");
    }

    #[test]
    fn tampered_beacon_fails_verify() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(1);
        b.device = sk.verifying_key().to_bytes();
        let mut signed = sign_presence_beacon(b, &sk).unwrap();
        signed.beacon.muted = false; // tamper after signing
        assert_eq!(
            verify_presence_beacon_sig(&signed),
            Err(BeaconError::BadSig)
        );
    }

    #[test]
    fn signature_must_match_embedded_device_key() {
        let signer = SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(1);
        b.device = SigningKey::from_bytes(&[9u8; 32])
            .verifying_key()
            .to_bytes(); // different device
        let signed = sign_presence_beacon(b, &signer).unwrap();
        assert_eq!(
            verify_presence_beacon_sig(&signed),
            Err(BeaconError::BadSig)
        );
    }

    #[test]
    fn seal_open_round_trips_and_wrong_key_drops() {
        use crate::community_channel_log::derive_channel_key;
        use crate::owner_state_types::EpochKey;
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(3);
        b.device = sk.verifying_key().to_bytes();
        let signed = sign_presence_beacon(b, &sk).unwrap();
        let (c, ch) = (SpaceId([0xc0; 16]), ChannelId([0xc1; 16]));
        let key = derive_channel_key(&EpochKey::new([0x11; 32]), &c, &ch);
        let sealed = seal_presence_beacon(&key, &c, &ch, &signed).unwrap();
        assert_eq!(open_presence_beacon(&key, &c, &ch, &sealed), Some(signed));
        let other = derive_channel_key(&EpochKey::new([0x22; 32]), &c, &ch);
        assert_eq!(open_presence_beacon(&other, &c, &ch, &sealed), None);
    }
}

#[cfg(test)]
mod map_tests {
    use super::*;

    fn b(owner: u8, device: u8, seq: u64, muted: bool, left: bool) -> VoicePresenceBeacon {
        VoicePresenceBeacon {
            owner: [owner; 16],
            device: [device; 32],
            muted,
            joined_hlc: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "x".into(),
            },
            seq,
            left,
            hand: None,
        }
    }
    const C: SpaceId = SpaceId([0xc0; 16]);
    const CH: ChannelId = ChannelId([0xc1; 16]);
    const TTL_MS: u64 = 12_000;

    // Two HLCs where H2 is strictly newer than H1 (later wall clock). Used to
    // model distinct join sessions in the rejoin-convergence tests.
    fn h1() -> Hlc {
        Hlc {
            wall_ms: 1000,
            logical: 0,
            device_id: "d".into(),
        }
    }
    fn h2() -> Hlc {
        Hlc {
            wall_ms: 2000,
            logical: 0,
            device_id: "d".into(),
        }
    }

    // Beacon with an explicit `joined_hlc` (the `b` helper above pins one fixed
    // session HLC; these tests need to vary it to model rejoins).
    fn bh(
        owner: u8,
        device: u8,
        seq: u64,
        muted: bool,
        left: bool,
        joined_hlc: Hlc,
    ) -> VoicePresenceBeacon {
        VoicePresenceBeacon {
            owner: [owner; 16],
            device: [device; 32],
            muted,
            joined_hlc,
            seq,
            left,
            hand: None,
        }
    }

    #[test]
    fn apply_then_roster_lists_member() {
        let mut m = VoicePresenceMap::new();
        assert!(m.apply(&C, &CH, &b(1, 1, 0, true, false), 0));
        let r = m.roster(&C, &CH);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].owner, [1; 16]);
        assert!(r[0].muted);
        assert_eq!(r[0].hand_raised_at, None);
    }

    /// ZEB-612: a same-session heartbeat whose ONLY delta is the hand field is
    /// a roster-visible change (the ✋ badge / queue reorder), while a bare
    /// heartbeat with unchanged hand stays invisible (no emit spam).
    #[test]
    fn hand_flip_is_roster_visible_and_bare_heartbeat_is_not() {
        let mut m = VoicePresenceMap::new();
        assert!(m.apply(&C, &CH, &b(1, 1, 0, true, false), 0));
        // Raise: same session, newer seq, hand Some → visible change.
        let mut raised = b(1, 1, 1, true, false);
        raised.hand = Some(777);
        assert!(m.apply(&C, &CH, &raised, 100), "hand raise must be visible");
        assert_eq!(m.roster(&C, &CH)[0].hand_raised_at, Some(777));
        // Bare heartbeat with the SAME hand → invisible (liveness only).
        let mut same = b(1, 1, 2, true, false);
        same.hand = Some(777);
        assert!(
            !m.apply(&C, &CH, &same, 200),
            "unchanged hand heartbeat must not emit"
        );
        // Lower: hand back to None → visible change.
        assert!(
            m.apply(&C, &CH, &b(1, 1, 3, true, false), 300),
            "hand lower must be visible"
        );
        assert_eq!(m.roster(&C, &CH)[0].hand_raised_at, None);
    }

    /// ZEB-853 D5: the raise is stamped by a LOCAL monotonic clock on first
    /// observation and ordering keys on THAT — not the peer-supplied `hand` wall
    /// value — so a hostile `hand=1` (epoch) can't squat the front of the queue.
    /// The stamp is set once on lowered→raised, stable across heartbeats while
    /// raised, cleared on lower, and re-stamped on a fresh raise.
    #[test]
    fn hand_first_observed_is_local_and_stable() {
        let mut m = VoicePresenceMap::new();
        // Attacker raises with hand=Some(1) (epoch); we stamp our own now_ms.
        let mut raise = b(1, 1, 1, true, false);
        raise.hand = Some(1);
        assert!(m.apply(&C, &CH, &raise, 5_000));
        assert_eq!(
            m.roster(&C, &CH)[0].hand_first_observed_ms,
            Some(5_000),
            "stamped with local now_ms, not the peer's epoch"
        );
        // Second heartbeat, still raised (same hand), at a later now_ms → stable.
        let mut still = b(1, 1, 2, true, false);
        still.hand = Some(1);
        m.apply(&C, &CH, &still, 6_000);
        assert_eq!(
            m.roster(&C, &CH)[0].hand_first_observed_ms,
            Some(5_000),
            "stable across heartbeats"
        );
        // Lower (hand None) → cleared.
        assert!(m.apply(&C, &CH, &b(1, 1, 3, true, false), 7_000));
        assert_eq!(m.roster(&C, &CH)[0].hand_first_observed_ms, None);
        // Re-raise → re-stamped at the new now_ms.
        let mut reraise = b(1, 1, 4, true, false);
        reraise.hand = Some(1);
        m.apply(&C, &CH, &reraise, 8_000);
        assert_eq!(m.roster(&C, &CH)[0].hand_first_observed_ms, Some(8_000));
    }

    /// ZEB-612: `RosterEntry` serializes the hand as camelCase `handRaisedAt`
    /// (number when raised, null when lowered) for the JSON event payload.
    #[test]
    fn roster_entry_serializes_hand_raised_at() {
        let raised = RosterEntry {
            owner: [1; 16],
            device: [2; 32],
            muted: false,
            hand_raised_at: Some(777),
            hand_first_observed_ms: Some(5_000),
        };
        let lowered = RosterEntry {
            hand_raised_at: None,
            hand_first_observed_ms: None,
            ..raised.clone()
        };
        let jr = serde_json::to_value(&raised).expect("serialize");
        let jl = serde_json::to_value(&lowered).expect("serialize");
        assert_eq!(jr.get("handRaisedAt").and_then(|v| v.as_u64()), Some(777));
        assert!(jl.get("handRaisedAt").expect("key present").is_null());
        // ZEB-853 D5: the local first-observed stamp rides the same JSON payload.
        assert_eq!(
            jr.get("handFirstObservedMs").and_then(|v| v.as_u64()),
            Some(5_000)
        );
        assert!(jl
            .get("handFirstObservedMs")
            .expect("key present")
            .is_null());
    }

    #[test]
    fn stale_seq_ignored_newer_applied() {
        let mut m = VoicePresenceMap::new();
        m.apply(&C, &CH, &b(1, 1, 5, true, false), 0);
        assert!(
            !m.apply(&C, &CH, &b(1, 1, 3, false, false), 0),
            "older seq → no change"
        );
        assert!(m.roster(&C, &CH)[0].muted, "still muted=true from seq 5");
        assert!(
            m.apply(&C, &CH, &b(1, 1, 6, false, false), 0),
            "newer seq → change"
        );
        assert!(!m.roster(&C, &CH)[0].muted);
    }

    #[test]
    fn shared_seq_counter_keeps_heartbeats_fresh_after_immediate_beacon() {
        // Regression (Cursor/CodeAnt/CodeRabbit): the immediate mute beacon used
        // to jump to `u64::MAX - 1`, which then permanently outranked the
        // publisher's `0..` heartbeats — every later heartbeat was rejected as
        // stale, `last_seen_ms` froze, and the device TTL-evicted from its own
        // channel while a second toggle (reusing the sentinel) stopped
        // propagating. The fix shares ONE monotone counter so the immediate
        // beacon sits strictly between surrounding heartbeats. Model that order:
        let mut m = VoicePresenceMap::new();
        // Heartbeats seq 0,1 (muted=true from the start-muted join).
        assert!(m.apply(&C, &CH, &b(1, 1, 0, true, false), 1_000));
        m.apply(&C, &CH, &b(1, 1, 1, true, false), 2_000);
        // Immediate unmute beacon seq 2 (drawn from the SHARED counter).
        assert!(
            m.apply(&C, &CH, &b(1, 1, 2, false, false), 3_000),
            "immediate beacon (newer seq) flips muted"
        );
        assert!(!m.roster(&C, &CH)[0].muted);
        // A subsequent heartbeat seq 3 MUST still advance liveness…
        m.apply(&C, &CH, &b(1, 1, 3, false, false), 9_000);
        assert!(
            m.sweep(15_000, TTL_MS).is_empty(),
            "heartbeat seq 3 advanced last_seen → NOT evicted",
        );
        // …and a second toggle (seq 4) MUST still propagate.
        assert!(
            m.apply(&C, &CH, &b(1, 1, 4, true, false), 10_000),
            "second toggle (seq 4) still propagates — not a duplicate sentinel",
        );
        assert!(m.roster(&C, &CH)[0].muted);
    }

    #[test]
    fn immediate_beacon_sentinel_seq_would_poison_heartbeats() {
        // Documents the OLD bug the shared counter removes: an immediate beacon
        // at a near-`u64::MAX` sentinel makes every later `0..` heartbeat look
        // stale, freezing liveness until the TTL wrongly evicts a live member.
        let mut m = VoicePresenceMap::new();
        m.apply(&C, &CH, &b(1, 1, 0, true, false), 1_000);
        m.apply(&C, &CH, &b(1, 1, u64::MAX - 1, false, false), 2_000); // old sentinel
        assert!(
            !m.apply(&C, &CH, &b(1, 1, 1, false, false), 9_000),
            "low-seq heartbeat rejected after sentinel — the poisoning we removed",
        );
        assert!(
            !m.sweep(15_000, TTL_MS).is_empty(),
            "last_seen frozen at the toggle → live member wrongly evicted",
        );
    }

    #[test]
    fn heartbeat_keeps_alive_silence_evicts() {
        let mut m = VoicePresenceMap::new();
        m.apply(&C, &CH, &b(1, 1, 0, true, false), 0);
        m.apply(&C, &CH, &b(1, 1, 1, true, false), 8_000); // heartbeat at 8s
        assert!(
            m.sweep(11_000, TTL_MS).is_empty(),
            "within TTL of last beacon"
        );
        assert_eq!(
            m.sweep(21_000, TTL_MS),
            vec![((C, CH), [1u8; 16], [1u8; 32])],
            "12s after last → evict"
        );
        assert!(m.roster(&C, &CH).is_empty());
    }

    /// ZEB-791: mirrors `community_presence::sweep_evicts_future_stamped_entry`
    /// — a row whose locally-written `last_seen_ms` sits ahead of `now_ms`
    /// (backward clock step) must still age out rather than stay in the roster.
    #[test]
    fn sweep_evicts_future_stamped_entry() {
        let mut m = VoicePresenceMap::new();
        m.apply(&C, &CH, &b(1, 1, 0, true, false), 100_000);
        assert_eq!(
            m.sweep(10_000, TTL_MS),
            vec![((C, CH), [1u8; 16], [1u8; 32])],
            "a future-stamped entry must not be immortal"
        );
        assert!(m.roster(&C, &CH).is_empty());
    }

    #[test]
    fn tombstone_removes_instantly() {
        let mut m = VoicePresenceMap::new();
        m.apply(&C, &CH, &b(1, 1, 0, true, false), 0);
        assert!(
            m.apply(&C, &CH, &b(1, 1, 1, true, true), 100),
            "left=true → change (removal)"
        );
        assert!(m.roster(&C, &CH).is_empty());
    }

    #[test]
    fn hlc_ordering_sanity() {
        assert!(h2().is_strictly_newer_than(&h1()));
        assert!(!h1().is_strictly_newer_than(&h2()));
    }

    #[test]
    fn rejoin_newer_session_supersedes_lower_seq() {
        // Device D present at session H1 with a high seq.
        let mut m = VoicePresenceMap::new();
        assert!(m.apply(&C, &CH, &bh(1, 1, 5, true, false, h1()), 0));
        assert_eq!(m.roster(&C, &CH).len(), 1);
        // Rejoin: NEW session H2, seq restarted at 0. Even though seq dropped,
        // the newer joined_hlc must win — seq 0 is accepted, NOT rejected.
        assert!(
            m.apply(&C, &CH, &bh(1, 1, 0, false, false, h2()), 100),
            "newer session supersedes regardless of lower seq"
        );
        let r = m.roster(&C, &CH);
        assert_eq!(r.len(), 1);
        assert!(!r[0].muted, "roster reflects the new session (muted=false)");
    }

    #[test]
    fn older_session_heartbeat_is_stale() {
        // Device D present at session H2.
        let mut m = VoicePresenceMap::new();
        assert!(m.apply(&C, &CH, &bh(1, 1, 0, false, false, h2()), 0));
        // A delayed heartbeat from the OLDER session H1, even with a huge seq,
        // must be rejected as stale and leave the entry untouched.
        assert!(
            !m.apply(&C, &CH, &bh(1, 1, 99, true, false, h1()), 100),
            "older session → stale even with higher seq"
        );
        let r = m.roster(&C, &CH);
        assert_eq!(r.len(), 1);
        assert!(!r[0].muted, "entry unchanged (still muted=false from H2)");
    }

    #[test]
    fn stale_old_session_tombstone_is_ignored() {
        // Device D freshly rejoined at session H2.
        let mut m = VoicePresenceMap::new();
        assert!(m.apply(&C, &CH, &bh(1, 1, 0, false, false, h2()), 0));
        // A delayed tombstone from the OLD session H1 must NOT evict the
        // freshly-rejoined entry.
        assert!(
            !m.apply(&C, &CH, &bh(1, 1, 1, false, true, h1()), 100),
            "old-session tombstone is ignored"
        );
        assert_eq!(m.roster(&C, &CH).len(), 1, "freshly-rejoined entry remains");
    }

    #[test]
    fn owner_for_device_resolves_known_and_gravestone_entries() {
        let mut m = VoicePresenceMap::new();
        m.apply(&C, &CH, &b(1, 1, 0, true, false), 0);
        assert_eq!(m.owner_for_device(&C, &CH, &[1u8; 32]), Some([1u8; 16]));
        // Unknown device → None.
        assert_eq!(m.owner_for_device(&C, &CH, &[9u8; 32]), None);
        // A buried (gravestone) entry is hidden from the roster but still
        // resolvable here, so a muted-then-departing sender's last frames drop.
        m.apply(&C, &CH, &b(1, 1, 1, true, true), 100);
        assert!(m.roster(&C, &CH).is_empty());
        assert_eq!(m.owner_for_device(&C, &CH, &[1u8; 32]), Some([1u8; 16]));
    }

    #[test]
    fn remove_channel_clears_roster() {
        let mut m = VoicePresenceMap::new();
        m.apply(&C, &CH, &b(1, 1, 0, true, false), 0);
        assert!(!m.roster(&C, &CH).is_empty());
        m.remove_channel(&C, &CH);
        assert!(
            m.roster(&C, &CH).is_empty(),
            "remove_channel drops the channel's roster so the sweep stops emitting for it"
        );
        // A subsequent sweep must not resurrect or report the cleared entry.
        assert!(m.sweep(99_999, 12_000).is_empty());
    }

    #[test]
    fn tombstone_blocks_reordered_same_session_heartbeat() {
        // Member present at session H1 (production tombstones carry seq u64::MAX).
        let mut m = VoicePresenceMap::new();
        assert!(m.apply(&C, &CH, &bh(1, 1, 3, true, false, h1()), 0));
        // Leave: tombstone buries the live member (roster shrinks → change).
        assert!(
            m.apply(&C, &CH, &bh(1, 1, u64::MAX, true, true, h1()), 100),
            "tombstone buries the live member"
        );
        assert!(m.roster(&C, &CH).is_empty(), "buried → hidden from roster");
        // A heartbeat from the SAME session, reordered to arrive AFTER the
        // tombstone, must NOT resurrect the member.
        assert!(
            !m.apply(&C, &CH, &bh(1, 1, 4, false, false, h1()), 150),
            "reordered same-session heartbeat is rejected by the gravestone"
        );
        assert!(m.roster(&C, &CH).is_empty(), "no resurrection");
        // But a genuine NEW session (strictly-newer joined_hlc) revives at once.
        assert!(
            m.apply(&C, &CH, &bh(1, 1, 0, false, false, h2()), 200),
            "newer session revives the gravestone"
        );
        let r = m.roster(&C, &CH);
        assert_eq!(r.len(), 1, "fresh session is live");
        assert!(!r[0].muted, "roster reflects the new session");
    }

    #[test]
    fn sweep_prunes_empty_channel_submaps() {
        let mut m = VoicePresenceMap::new();
        m.apply(&C, &CH, &b(1, 1, 0, true, false), 0);
        // TTL-evict the only member (not an explicit leave / remove_channel).
        assert_eq!(
            m.sweep(TTL_MS, TTL_MS).len(),
            1,
            "the live member is evicted"
        );
        // The now-empty (community, channel) sub-map must be reclaimed, not left
        // as a ghost that every future sweep re-scans.
        assert!(
            m.inner.is_empty(),
            "empty channel sub-map pruned after eviction"
        );
    }

    #[test]
    fn gravestone_is_gc_d_silently_by_ttl_sweep() {
        let mut m = VoicePresenceMap::new();
        m.apply(&C, &CH, &b(1, 1, 0, true, false), 0);
        m.apply(&C, &CH, &b(1, 1, 1, true, true), 100); // tombstone → gravestone
                                                        // The gravestone is hidden but retained as a barrier until TTL.
        assert!(m.roster(&C, &CH).is_empty());
        // TTL sweep GCs the gravestone — and reports NO eviction (it was already
        // hidden, so its removal is not a roster-visible change).
        assert!(
            m.sweep(100 + TTL_MS, TTL_MS).is_empty(),
            "gravestone GC is silent"
        );
        assert!(m.inner.is_empty(), "gravestone sub-map reclaimed");
    }

    #[test]
    fn newer_session_tombstone_advances_gravestone_barrier() {
        let mut m = VoicePresenceMap::new();
        // Session H1 joins, then leaves → gravestone pinned at H1.
        assert!(m.apply(&C, &CH, &bh(1, 1, 0, true, false, h1()), 0));
        assert!(m.apply(&C, &CH, &bh(1, 1, u64::MAX, true, true, h1()), 10));
        assert!(m.roster(&C, &CH).is_empty());
        // A NEWER session H2 also departs while we still hold the H1 gravestone
        // (H2's live beacons reordered after its tombstone, or never seen). The
        // gravestone barrier must ADVANCE to H2 — not stay pinned at H1.
        assert!(
            !m.apply(&C, &CH, &bh(1, 1, u64::MAX, true, true, h2()), 20),
            "newer-session tombstone on a gravestone is not a roster change"
        );
        // A reordered heartbeat from H2 (the now-departed newer session) must be
        // rejected; without the barrier advance it would resurrect H2.
        assert!(
            !m.apply(&C, &CH, &bh(1, 1, 5, false, false, h2()), 30),
            "H2 heartbeat must not revive: barrier advanced to H2"
        );
        assert!(m.roster(&C, &CH).is_empty(), "no resurrection");
        // Only a still-newer session (H3) legitimately revives the gravestone.
        let h3 = Hlc {
            wall_ms: 3000,
            logical: 0,
            device_id: "d".into(),
        };
        assert!(
            m.apply(&C, &CH, &bh(1, 1, 0, false, false, h3), 40),
            "a genuinely newer session (H3) revives the gravestone"
        );
        assert_eq!(m.roster(&C, &CH).len(), 1);
    }
}

#[cfg(test)]
mod membership_tests {
    use super::*;
    use crate::community_membership::{MaterializedMembership, MemberState, MemberStatus};
    use std::collections::{BTreeMap, BTreeSet};

    fn member(status: MemberStatus, device: [u8; 32]) -> MemberState {
        let mut keys = BTreeSet::new();
        keys.insert(device);
        MemberState {
            status,
            joined_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "x".into(),
            },
            left_at: None,
            enrolled_device_keys: keys,
            revoked_device_keys: BTreeSet::new(),
        }
    }

    fn materialized_with(owner: OwnerAddr, member: MemberState) -> MaterializedMembership {
        let mut members = BTreeMap::new();
        members.insert(owner, member);
        MaterializedMembership {
            members,
            ..Default::default()
        }
    }

    #[test]
    fn enrolled_joined_member_passes() {
        let owner = OwnerAddr([0xa1; 16]);
        let device = [0x22; 32];
        let m = materialized_with(owner, member(MemberStatus::Joined, device));
        assert!(device_is_enrolled(&m, &owner, &device));
    }

    #[test]
    fn unknown_owner_fails() {
        let owner = OwnerAddr([0xa1; 16]);
        let device = [0x22; 32];
        let m = materialized_with(owner, member(MemberStatus::Joined, device));
        assert!(!device_is_enrolled(&m, &OwnerAddr([0xff; 16]), &device));
    }

    #[test]
    fn non_joined_status_fails() {
        let owner = OwnerAddr([0xa1; 16]);
        let device = [0x22; 32];
        let m = materialized_with(owner, member(MemberStatus::Invited, device));
        assert!(!device_is_enrolled(&m, &owner, &device));
    }

    #[test]
    fn unenrolled_device_fails() {
        let owner = OwnerAddr([0xa1; 16]);
        let device = [0x22; 32];
        let m = materialized_with(owner, member(MemberStatus::Joined, device));
        // same Joined owner, but a different device key → rejected
        assert!(!device_is_enrolled(&m, &owner, &[0x33; 32]));
    }
}

#[cfg(test)]
mod groupdm_membership_tests {
    use super::*;
    use crate::owner_state_crdt::OwnerState;
    use crate::owner_state_types::{
        DeviceIdentityHash, DmContentKey, OwnerDeviceEntry, Space, SpaceKind,
    };

    fn hlc() -> Hlc {
        Hlc {
            wall_ms: 1,
            logical: 0,
            device_id: "t".into(),
        }
    }

    /// A 64-byte identity_pub `[X25519(32) || Ed25519(32)]` whose Ed25519 half
    /// (bytes `[32..64]`) is `device`. The X25519 half is filler — the
    /// membership check only inspects the upper half.
    fn identity_pub_for(device: &[u8; 32]) -> [u8; 64] {
        let mut ip = [0u8; 64];
        ip[32..64].copy_from_slice(device);
        ip
    }

    fn group_dm_space(id: SpaceId, members: Vec<OwnerAddr>) -> Space {
        Space {
            id,
            kind: SpaceKind::GroupDm,
            parent: None,
            community_id: None,
            name: "Group".into(),
            transport: None,
            members,
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(),
            updated_at: hlc(),
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        }
    }

    /// Build an `OwnerState` with a 3-member `GroupDm` space and one cached,
    /// enrolled device per the given `(owner, device)` pairs.
    fn state_with(space_id: SpaceId, enrolled: &[(OwnerAddr, [u8; 32])]) -> OwnerState {
        let members: Vec<OwnerAddr> = enrolled.iter().map(|(o, _)| *o).collect();
        let mut os = OwnerState::default();
        os.spaces
            .insert(space_id, group_dm_space(space_id, members));
        for (owner, device) in enrolled {
            os.owner_device_cache.devices.insert(
                *owner,
                OwnerDeviceEntry {
                    devices: vec![DeviceIdentityHash([0u8; 16])],
                    device_identity_pubs: vec![Some(identity_pub_for(device))],
                    learned_at: hlc(),
                    device_tunnel_contacts: vec![None],
                },
            );
        }
        os
    }

    #[tokio::test]
    async fn groupdm_member_with_enrolled_device_passes() {
        let space_id = SpaceId([0x5d; 16]);
        let m1 = (OwnerAddr([0x01; 16]), [0x11; 32]);
        let m2 = (OwnerAddr([0x02; 16]), [0x22; 32]);
        let m3 = (OwnerAddr([0x03; 16]), [0x33; 32]);
        let state = tokio::sync::Mutex::new(state_with(space_id, &[m1, m2, m3]));
        // (a) a member whose enrolled device matches → true
        assert!(groupdm_beacon_signer_is_member(&state, &space_id, &m2.0, &m2.1).await);
    }

    #[tokio::test]
    async fn groupdm_non_member_fails() {
        let space_id = SpaceId([0x5d; 16]);
        let m1 = (OwnerAddr([0x01; 16]), [0x11; 32]);
        let m2 = (OwnerAddr([0x02; 16]), [0x22; 32]);
        let m3 = (OwnerAddr([0x03; 16]), [0x33; 32]);
        let state = tokio::sync::Mutex::new(state_with(space_id, &[m1, m2, m3]));
        // (b) a non-member owner (not in `members`) → false
        let outsider = OwnerAddr([0xff; 16]);
        assert!(!groupdm_beacon_signer_is_member(&state, &space_id, &outsider, &[0x11; 32]).await);
    }

    #[tokio::test]
    async fn groupdm_member_with_unenrolled_device_fails() {
        let space_id = SpaceId([0x5d; 16]);
        let m1 = (OwnerAddr([0x01; 16]), [0x11; 32]);
        let m2 = (OwnerAddr([0x02; 16]), [0x22; 32]);
        let m3 = (OwnerAddr([0x03; 16]), [0x33; 32]);
        let state = tokio::sync::Mutex::new(state_with(space_id, &[m1, m2, m3]));
        // (c) a current member, but a device NOT in their enrolled set → false
        assert!(!groupdm_beacon_signer_is_member(&state, &space_id, &m2.0, &[0x99; 32]).await);
    }
}
