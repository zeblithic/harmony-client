//! ZEB-350 Voice V2 presence: ephemeral signed+sealed beacons + the live
//! roster. Beacons ride a dedicated Zenoh topic (never the CRDT); the seal
//! under `ChannelKey` gates non-members, and the device-#2 signature +
//! materialized-membership check (Task 7) prevents intra-member spoofing.

use crate::community_membership::ChannelId;
use crate::owner_state_types::{Hlc, SpaceId};
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
/// it; Task 7 additionally checks `device ∈ owner.enrolled_device_keys`.
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
/// key plus the `(owner, device)` of an evicted entry. The caller (Task 7's
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
        if beacon.left {
            return chan.remove(&beacon.device).is_some();
        }
        match chan.get_mut(&beacon.device) {
            Some(e) if beacon.seq <= e.seq => false, // stale or duplicate
            Some(e) => {
                e.muted = beacon.muted;
                e.seq = beacon.seq;
                e.last_seen_ms = now_ms;
                e.joined_hlc = beacon.joined_hlc.clone();
                // A newer beacon always advances liveness (last_seen), which is
                // itself a roster-relevant change for the heartbeat/emit cadence
                // — so any seq advance reports `true`. The call site (Task 7) may
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
}
