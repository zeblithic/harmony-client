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

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BeaconError {
    #[error("beacon CBOR encode failed")]
    Encode,
    #[error("beacon signature invalid")]
    BadSig,
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
    pub seq: u64,
    pub joined_hlc: Hlc,
    /// Monotonic-ms timestamp of the last applied beacon (injected by caller).
    pub last_seen_ms: u64,
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
            // Only honour a tombstone if it is NOT from a strictly-older
            // session than what we hold: a delayed old-session tombstone must
            // not evict a freshly-rejoined entry.
            if let Some(e) = chan.get(&beacon.device) {
                if e.joined_hlc.is_strictly_newer_than(&beacon.joined_hlc) {
                    return false;
                }
            }
            return chan.remove(&beacon.device).is_some();
        }
        match chan.get_mut(&beacon.device) {
            Some(e) if beacon.joined_hlc.is_strictly_newer_than(&e.joined_hlc) => {
                // A NEW join session supersedes regardless of seq (seq restarts
                // at 0 on rejoin).
                e.owner = beacon.owner;
                e.muted = beacon.muted;
                e.seq = beacon.seq;
                e.joined_hlc = beacon.joined_hlc.clone();
                e.last_seen_ms = now_ms;
                true
            }
            Some(e) if e.joined_hlc.is_strictly_newer_than(&beacon.joined_hlc) => false, // older session → stale
            Some(e) if beacon.seq <= e.seq => false, // same session, stale or duplicate seq
            Some(e) => {
                // Same session, newer seq. `joined_hlc` is unchanged.
                e.muted = beacon.muted;
                e.seq = beacon.seq;
                e.last_seen_ms = now_ms;
                // A newer beacon always advances liveness (last_seen), which is
                // itself a roster-relevant change for the heartbeat/emit cadence
                // — so any seq advance reports `true`. The call site may
                // additionally gate the frontend emit on mute/membership change.
                true
            }
            None => {
                chan.insert(
                    beacon.device,
                    PresenceEntry {
                        owner: beacon.owner,
                        muted: beacon.muted,
                        seq: beacon.seq,
                        joined_hlc: beacon.joined_hlc.clone(),
                        last_seen_ms: now_ms,
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
                let alive = now_ms.saturating_sub(e.last_seen_ms) < ttl_ms;
                if !alive {
                    evicted.push((*key, e.owner, *device));
                }
                alive
            });
        }
        evicted
    }

    pub fn roster(&self, c: &SpaceId, ch: &ChannelId) -> Vec<RosterEntry> {
        self.inner
            .get(&(*c, *ch))
            .map(|chan| {
                chan.iter()
                    .map(|(device, e)| RosterEntry {
                        owner: e.owner,
                        device: *device,
                        muted: e.muted,
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
}

// ── Membership verification + pub/sub spawn helpers ──────────────

use crate::community_state_sync::CommunitySyncRegistry;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::Emitter;
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
pub fn spawn_voice_presence_subscriber<R: tauri::Runtime>(
    session: zenoh::Session,
    topic: String,
    channel_key: Arc<ChannelKey>,
    community: SpaceId,
    channel: ChannelId,
    registry: Arc<CommunitySyncRegistry>,
    map: Arc<tokio::sync::Mutex<VoicePresenceMap>>,
    app: tauri::AppHandle<R>,
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
                let _ = app.emit(
                    "voice-presence-changed",
                    serde_json::json!({
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

/// Spawn a heartbeat publisher: emit an immediate beacon, then one every
/// `interval` (4 s in V2). `muted` is hardcoded `true` — V2 has no mic capture,
/// so every session starts (and stays) muted. A per-publisher monotone `seq`
/// drives roster freshness. Honors `closing` for shutdown.
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
    interval: std::time::Duration,
    closing: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut seq: u64 = 0;
        // `interval` fires immediately on the first `tick()`, giving us the
        // "immediate first beacon then every 4 s" cadence for free.
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            if closing.load(Ordering::SeqCst) {
                break;
            }
            let beacon = VoicePresenceBeacon {
                owner: self_owner.0,
                device: self_device,
                muted: true,
                joined_hlc: joined_hlc.clone(),
                seq,
                left: false,
            };
            seq = seq.wrapping_add(1);
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
    };
    let signed = sign_presence_beacon(beacon, signing_key).ok()?;
    seal_presence_beacon(channel_key, community, channel, &signed).ok()
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
        }
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
