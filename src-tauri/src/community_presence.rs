//! ZEB-537 community presence: ephemeral signed+sealed liveness beacons,
//! generalizing the per-call `voice_presence` (ZEB-350) pattern to a
//! per-community scope. Beacons ride a dedicated Zenoh topic (never the CRDT);
//! the seal under the per-community presence key (`derive_presence_key`) gates
//! non-members, and the device-#2 signature + materialized-membership check
//! prevents intra-member spoofing.
//!
//! Presence here is community-scoped (not per-channel): the seal binds only the
//! community, via a sentinel all-zero `ChannelId` passed to the audited
//! `encrypt_voice_packet` / `decrypt_voice_packet` AEAD seam, under the distinct
//! `COMMUNITY_PRESENCE_AAD` domain.

use crate::community_channel_log::ChannelKey;
use crate::community_membership::ChannelId;
use crate::community_state_sync::CommunitySyncRegistry;
use crate::owner_state_crypto::canonical_cbor_encode;
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use crate::reconnect_supervisor::SupervisorHandle;
use crate::voice_crypto::{decrypt_voice_packet, encrypt_voice_packet, COMMUNITY_PRESENCE_AAD};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::task::JoinHandle;

/// Presence has no channel, so the AEAD seam (which is `(community, channel)`
/// scoped for voice) is bound with this all-zero sentinel channel. The
/// `COMMUNITY_PRESENCE_AAD` domain already separates community presence from
/// every voice/channel-log artifact, so the sentinel only needs to be stable.
const PRESENCE_SENTINEL_CHANNEL: ChannelId = ChannelId([0u8; 16]);

/// The unsigned community-presence claim. Canonical CBOR, 2-char keys
/// (same-length invariant for deterministic encoding).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceBeacon {
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
    #[serde(rename = "sh")]
    pub started_hlc: Hlc,
    #[serde(rename = "sq")]
    pub seq: u64,
}

/// Beacon + detached device-#2 signature over `canonical_cbor_encode(beacon)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedPresenceBeacon {
    #[serde(rename = "bc")]
    pub beacon: PresenceBeacon,
    #[serde(
        rename = "sg",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
}

// Register both beacon types as `CanonicalPayload` so `canonical_cbor_encode`
// (the sealed-trait encoder) can sign/seal them. Mirrors `voice_presence`.
impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for PresenceBeacon {}
impl crate::owner_state_crypto::CanonicalPayload for PresenceBeacon {}
impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for SignedPresenceBeacon {}
impl crate::owner_state_crypto::CanonicalPayload for SignedPresenceBeacon {}

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

/// Sign a beacon with the device-#2 ed25519 key. The signature covers the
/// canonical CBOR of the unsigned beacon (sig field excluded by construction).
pub fn sign_presence_beacon(
    beacon: PresenceBeacon,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<SignedPresenceBeacon, BeaconError> {
    use ed25519_dalek::Signer;
    let bytes = canonical_cbor_encode(&beacon).map_err(|_| BeaconError::Encode)?;
    let sig = signing_key.sign(&bytes).to_bytes();
    Ok(SignedPresenceBeacon { beacon, sig })
}

/// Verify the detached signature against the verifying key embedded in
/// `beacon.device`. This proves the holder of `device`'s private key signed it;
/// the membership check additionally requires `device ∈ owner.enrolled_device_keys`.
pub fn verify_presence_beacon_sig(signed: &SignedPresenceBeacon) -> Result<(), BeaconError> {
    let bytes = canonical_cbor_encode(&signed.beacon).map_err(|_| BeaconError::Encode)?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&signed.beacon.device)
        .map_err(|_| BeaconError::BadSig)?;
    let sig = ed25519_dalek::Signature::from_bytes(&signed.sig);
    vk.verify_strict(&bytes, &sig)
        .map_err(|_| BeaconError::BadSig)
}

/// Seal a signed beacon under the per-community presence key for transport.
/// Output framing matches the voice media packet (`[12B nonce][ct+tag]`) but
/// binds the sentinel channel + `COMMUNITY_PRESENCE_AAD` so it can only ever be
/// opened as a community-presence beacon for `community`.
pub fn seal_presence_beacon(
    key: &ChannelKey,
    community: &SpaceId,
    signed: &SignedPresenceBeacon,
) -> Result<Vec<u8>, BeaconError> {
    let plain = canonical_cbor_encode(signed).map_err(|_| BeaconError::Encode)?;
    encrypt_voice_packet(
        key,
        community,
        &PRESENCE_SENTINEL_CHANNEL,
        COMMUNITY_PRESENCE_AAD,
        &plain,
    )
    .map_err(|_| BeaconError::Encode)
}

/// ZEB-919: open a sealed beacon under the first membership-epoch key
/// candidate that succeeds (candidates come from
/// [`crate::community_state_sync::epoch_key_candidates`]: the live current
/// key first, then the immediately-previous epoch's archived key, or the
/// spawn-time key in degraded mode). The previous-key rung heals rotation
/// skew: an un-rotated member's beacons stay visible to rotated members
/// exactly while the rotation event is still propagating to it — the same
/// load-bearing argument as the ZEB-918 rendezvous resolver rung.
pub(crate) fn open_presence_with_any(
    mks: &[crate::owner_state_types::EpochKey],
    community: &SpaceId,
    packet: &[u8],
) -> Option<SignedPresenceBeacon> {
    mks.iter().find_map(|mk| {
        let key = crate::community_channel_log::derive_presence_key(mk, community);
        open_presence_beacon(&key, community, packet)
    })
}

/// Open + decode a sealed beacon. Returns `None` on any failure (drop).
pub fn open_presence_beacon(
    key: &ChannelKey,
    community: &SpaceId,
    packet: &[u8],
) -> Option<SignedPresenceBeacon> {
    let plain = decrypt_voice_packet(
        key,
        community,
        &PRESENCE_SENTINEL_CHANNEL,
        COMMUNITY_PRESENCE_AAD,
        packet,
    )
    .ok()?;
    ciborium::from_reader(plain.as_slice()).ok()
}

/// Deterministic-nonce seal for wire-format fixtures. NEVER call from
/// production — a fixed nonce with a reused key is catastrophic nonce reuse.
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn seal_presence_beacon_with_nonce(
    key: &ChannelKey,
    community: &SpaceId,
    signed: &SignedPresenceBeacon,
    nonce: [u8; 12],
) -> Result<Vec<u8>, BeaconError> {
    let plain = canonical_cbor_encode(signed).map_err(|_| BeaconError::Encode)?;
    crate::voice_crypto::encrypt_voice_packet_with_nonce(
        key,
        community,
        &PRESENCE_SENTINEL_CHANNEL,
        COMMUNITY_PRESENCE_AAD,
        &plain,
        nonce,
    )
    .map_err(|_| BeaconError::Encode)
}

// ── Beacon cadence + builder (ZEB-537 Task 4) ───────────────────────

/// Heartbeat interval: a live device republishes its presence beacon every
/// 10 s. Mirrors the voice publisher cadence (scaled up — community presence is
/// pure liveness, not call-state, so it tolerates a slower beat).
pub const BEACON_INTERVAL_MS: u64 = 10_000;

/// TTL: a device is swept from the roster once `STALE_MS` have elapsed since its
/// last beacon (3× the interval, so two dropped beacons don't evict a live peer).
pub const STALE_MS: u64 = 30_000;

/// Build an unsigned community-presence beacon. Pure (no I/O) so the publisher
/// loop has exactly one construction site, unit-testable without standing up
/// Zenoh. Community presence carries no `muted`/`left` (pure liveness).
pub(crate) fn build_presence_beacon(
    owner: [u8; 16],
    device: [u8; 32],
    started_hlc: &Hlc,
    seq: u64,
) -> PresenceBeacon {
    PresenceBeacon {
        owner,
        device,
        started_hlc: started_hlc.clone(),
        seq,
    }
}

// ── In-memory roster map (ZEB-537 Task 2) ───────────────────────────
//
// Community-scoped, pure-liveness presence roster. Mirrors `VoicePresenceMap`
// (ZEB-350) but SIMPLIFIED: community presence carries no `muted` and no
// `left`/gravestone tombstone logic — it is pure liveness, so TTL eviction
// is the only way a device leaves the roster. Keyed by `SpaceId` (community)
// → device → entry (voice keys by `(community, channel)`).

/// One live device's last-known presence state within a community.
#[derive(Debug)]
struct PresenceEntry {
    owner: [u8; 16],
    started_hlc: Hlc,
    seq: u64,
    last_seen_ms: u64,
}

/// One row in a community's online roster: an owner aggregated across all of
/// their currently-live devices. This (owner-level, not device-level) is what
/// the frontend renders, so `apply` reports change only at owner granularity.
pub struct OwnerPresence {
    pub owner: [u8; 16],
    pub device_count: u32,
    pub last_seen_ms: u64,
}

/// ZEB-537: one online member as serialized to the frontend in a
/// `presence-updated` event (or returned by the `get_community_presence` IPC).
/// `online` is always `true` — the roster only ever contains live owners; an
/// owner dropping off is communicated by their absence from a fresh `members`
/// list. The camelCase wire shape is contract-pinned (frontend depends on it).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresenceMemberDto {
    pub owner_id_hex: String,
    pub online: bool,
    /// Unix-epoch ms of the most recent beacon receipt (ZEB-972: rebased from
    /// the map's monotonic clock via [`CommunityPresenceMap::online_owners_wall`]
    /// — the frontend compares this against `Date.now()`, so a raw map stamp
    /// must never land here).
    pub last_seen_ms: u64,
    pub device_count: u32,
}

/// ZEB-537: the full `presence-updated` event payload for one community.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresenceUpdatedPayload {
    pub community_id: String,
    pub members: Vec<PresenceMemberDto>,
}

impl PresenceMemberDto {
    pub fn from_owner_presence(o: &OwnerPresence) -> Self {
        Self {
            owner_id_hex: hex::encode(o.owner),
            online: true,
            last_seen_ms: o.last_seen_ms,
            device_count: o.device_count,
        }
    }
}

impl PresenceUpdatedPayload {
    pub fn new(community_id: [u8; 16], members: &[OwnerPresence]) -> Self {
        Self {
            community_id: hex::encode(community_id),
            members: members
                .iter()
                .map(PresenceMemberDto::from_owner_presence)
                .collect(),
        }
    }
}

pub struct CommunityPresenceMap {
    // community → device → entry
    inner: BTreeMap<SpaceId, BTreeMap<[u8; 32], PresenceEntry>>,
    /// ZEB-972 (Greptile PR #722): the map's own monotonic presence clock —
    /// ms since map creation by default. Every `apply`/`sweep` caller MUST
    /// source `now_ms` from THIS clock (`now_ms()`), and every wire emission
    /// MUST go through [`online_owners_wall`], which rebases receipt stamps
    /// from this domain onto the Unix epoch. Owning the clock here (rather
    /// than borrowing the event loop's `voice_now_ms`) is what makes writer
    /// and reader share one base regardless of construction/boot ordering —
    /// the seed IPC reads this map from outside the event loop and has no
    /// access to the loop's `Instant`.
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// ZEB-600: node-global presence-visibility gate. `true` = broadcast beacons
    /// (visible); `false` = appear offline. Shared as an `Arc` handle with every
    /// presence publisher so `set_presence_visibility` takes effect live. Default
    /// is TRUE — note a *derived* `Default` would be `false`
    /// (`Arc<AtomicBool>::default()`), which would silently ship every node
    /// invisible; hence the explicit `new()`/`Default` below.
    presence_visible: Arc<AtomicBool>,
}

impl Default for CommunityPresenceMap {
    fn default() -> Self {
        Self::new()
    }
}

// Manual impl: the `clock` closure isn't `Debug`.
impl std::fmt::Debug for CommunityPresenceMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommunityPresenceMap")
            .field("inner", &self.inner)
            .field("presence_visible", &self.presence_visible)
            .finish_non_exhaustive()
    }
}

impl CommunityPresenceMap {
    pub fn new() -> Self {
        let start = std::time::Instant::now();
        Self::new_with_clock(Arc::new(move || start.elapsed().as_millis() as u64))
    }

    /// ZEB-972: test seam — inject a deterministic monotonic clock.
    pub fn new_with_clock(clock: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Self {
            inner: BTreeMap::new(),
            presence_visible: Arc::new(AtomicBool::new(true)),
            clock,
        }
    }

    /// The map's monotonic clock. `apply`/`sweep` callers source `now_ms`
    /// here so receipt stamps and eviction math share one clock base.
    pub fn now_ms(&self) -> u64 {
        (self.clock)()
    }

    /// ZEB-600: clone the node-global visibility gate handle for a presence
    /// publisher, so a live [`set_visible`](Self::set_visible) flip is observed
    /// on the publisher's next tick without re-spawning it.
    pub fn visible_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.presence_visible)
    }

    /// ZEB-600: set node-global presence visibility (`true` = broadcast beacons).
    pub fn set_visible(&self, visible: bool) {
        self.presence_visible.store(visible, Ordering::SeqCst);
    }

    /// ZEB-600: read node-global presence visibility.
    pub fn is_visible(&self) -> bool {
        self.presence_visible.load(Ordering::SeqCst)
    }

    /// Apply a (verified, opened) beacon. `now_ms` is a monotonic clock the
    /// caller supplies. Returns true when the device set changes — i.e. a new
    /// device is inserted (a fresh owner OR an additional device for an
    /// already-online owner, since `deviceCount` is part of the emitted DTO and
    /// so is roster-visible). A bare liveness refresh of an existing device
    /// (same/newer-session or newer-seq for a device we already track) returns
    /// false to avoid frontend event spam.
    ///
    /// Freshness mirrors `VoicePresenceMap::apply` (minus gravestones): a
    /// strictly-newer `started_hlc` supersedes regardless of `seq` (seq
    /// restarts at 0 on every (re)start of the presence publisher), an older
    /// session is rejected, and within the same session only a strictly-newer
    /// `seq` advances liveness.
    pub fn apply(&mut self, c: &SpaceId, beacon: &PresenceBeacon, now_ms: u64) -> bool {
        let community = self.inner.entry(*c).or_default();
        match community.get_mut(&beacon.device) {
            Some(e) if beacon.started_hlc.is_strictly_newer_than(&e.started_hlc) => {
                // A NEW session supersedes regardless of seq. The owner already
                // had this device online, so the owner-level roster is unchanged.
                e.owner = beacon.owner;
                e.started_hlc = beacon.started_hlc.clone();
                e.seq = beacon.seq;
                e.last_seen_ms = now_ms;
                false
            }
            Some(e) if e.started_hlc.is_strictly_newer_than(&beacon.started_hlc) => false, // older session → stale
            Some(e) if beacon.seq <= e.seq => false, // same session, stale or duplicate seq
            Some(e) => {
                // Same session, newer seq: advance liveness so the TTL sweep
                // still sees this device alive, but the owner is already online
                // → not an owner-visible change.
                e.seq = beacon.seq;
                e.last_seen_ms = now_ms;
                false
            }
            None => {
                // Brand-new device for this community. A new device always
                // changes the roster's device set (the owner's `deviceCount` /
                // `last_seen_ms` is part of the emitted DTO), so this is always
                // a roster-visible change — whether or not the owner already had
                // another live device here.
                community.insert(
                    beacon.device,
                    PresenceEntry {
                        owner: beacon.owner,
                        started_hlc: beacon.started_hlc.clone(),
                        seq: beacon.seq,
                        last_seen_ms: now_ms,
                    },
                );
                true
            }
        }
    }

    /// Evict every entry whose last beacon is older than `ttl_ms`. Returns the
    /// `(community, owner, device)` of each evicted entry so the caller can
    /// re-emit the affected community's roster. With no gravestones every
    /// eviction is roster-affecting.
    pub fn sweep(&mut self, now_ms: u64, ttl_ms: u64) -> Vec<(SpaceId, [u8; 16], [u8; 32])> {
        let mut evicted = Vec::new();
        for (community, devices) in self.inner.iter_mut() {
            devices.retain(|device, e| {
                // ZEB-791: bound the forward direction too. `last_seen_ms` is
                // stamped locally at receipt, so only a backward local clock
                // step can put it ahead of `now_ms` — but while it is,
                // `saturating_sub` reads age 0 and the entry is immortal.
                // Fail closed: evict. A device that is genuinely still present
                // is re-added by its next beacon.
                let alive = now_ms >= e.last_seen_ms && now_ms - e.last_seen_ms < ttl_ms;
                if !alive {
                    evicted.push((*community, e.owner, *device));
                }
                alive
            });
        }
        // Reclaim community sub-maps emptied by eviction so they don't linger as
        // ghost entries every future sweep re-scans (Greptile lesson, ZEB-350).
        self.inner.retain(|_, devices| !devices.is_empty());
        evicted
    }

    /// The owners currently online in `c`, aggregated across their devices.
    /// `device_count` is that owner's live device count; `last_seen_ms` is the
    /// max over those devices. Deterministic order (sorted by owner bytes).
    pub fn online_owners(&self, c: &SpaceId) -> Vec<OwnerPresence> {
        let Some(devices) = self.inner.get(c) else {
            return Vec::new();
        };
        let mut by_owner: BTreeMap<[u8; 16], (u32, u64)> = BTreeMap::new();
        for e in devices.values() {
            let agg = by_owner.entry(e.owner).or_insert((0, 0));
            agg.0 += 1;
            agg.1 = agg.1.max(e.last_seen_ms);
        }
        by_owner
            .into_iter()
            .map(|(owner, (device_count, last_seen_ms))| OwnerPresence {
                owner,
                device_count,
                last_seen_ms,
            })
            .collect()
    }

    /// ZEB-972 (Greptile PR #722): [`online_owners`](Self::online_owners) with
    /// `last_seen_ms` rebased from the map's monotonic clock onto the Unix
    /// epoch (`epoch_now_ms − age`), for emission to the frontend — which
    /// compares these stamps against `Date.now()`. Raw map stamps are
    /// process-relative (ms since map creation) and MUST NOT cross the wire:
    /// `Date.now() − raw_stamp` reads as decades, classifying every live peer
    /// stale. Rebasing at read time (rather than storing wall stamps at
    /// receipt) keeps monotonic as the domain of record — ages stay correct
    /// across wall-clock steps, and the ZEB-791 sweep math is untouched.
    /// `epoch_now_ms` is injected for test determinism; production callers
    /// pass [`crate::file_sharing::now_epoch_ms`]. Saturating both ways: a
    /// stamp can't exceed the same clock's `now` (no wrap on age), and an age
    /// beyond `epoch_now_ms` clamps to 0 rather than wrapping far-future.
    pub fn online_owners_wall(&self, c: &SpaceId, epoch_now_ms: u64) -> Vec<OwnerPresence> {
        let mono_now = self.now_ms();
        self.online_owners(c)
            .into_iter()
            .map(|mut o| {
                let age = mono_now.saturating_sub(o.last_seen_ms);
                o.last_seen_ms = epoch_now_ms.saturating_sub(age);
                o
            })
            .collect()
    }

    /// Drop a community's entire roster (e.g. on leave), reclaiming the sub-map.
    pub fn remove_community(&mut self, c: &SpaceId) {
        self.inner.remove(c);
    }
}

// ── pub/sub spawn helpers (ZEB-537 Task 4) ──────────────────────────
//
// Mirror `voice_presence::spawn_voice_presence_{publisher,subscriber}` but
// community-scoped (no channel) and pure-liveness (no mute/kick/left).
//
// ZEB-919 key model: the engine's `membership_key()` is bound at spawn and
// NEVER changes for the engine's lifetime, so re-fetching the engine per
// tick does not follow epoch rotation (the pre-919 comments claimed it
// did — that was false). The presence key is now derived from the LIVE
// `Space.current_epoch_key` per operation: the publisher seals under the
// live key (degrading to the spawn key only when the live read is
// unavailable — publisher-degrades, ZEB-597 mirror), and the subscriber
// opens under `[current, previous]` candidates so rotation skew heals in
// both directions while the rotation event propagates. `crdt_state = None`
// (test/legacy wiring) is the documented degraded mode: both sides fall
// back to the spawn key coherently. A missing engine still means skip the
// tick (publisher) / drop the packet (subscriber).

/// Spawn a community-presence heartbeat publisher: emit an immediate beacon,
/// then one every `interval` (10 s in V1). Each tick re-derives the presence key
/// from the community's current epoch key (so rotation is followed), builds →
/// signs → seals → `session.put`s a beacon drawing a strictly-increasing `seq`
/// from `seq_counter`. Honors `closing` for shutdown. Mirrors
/// [`crate::voice_presence::spawn_voice_presence_publisher`], minus mute/kick.
///
/// `session` is an owned, cheaply-cloned `zenoh::Session` (call sites pass
/// `session.clone()`).
#[allow(clippy::too_many_arguments)]
pub fn spawn_community_presence_publisher(
    session: zenoh::Session,
    topic: String,
    registry: Arc<CommunitySyncRegistry>,
    community: SpaceId,
    signing_key: Arc<ed25519_dalek::SigningKey>,
    self_owner: OwnerAddr,
    self_device: [u8; 32],
    started_hlc: Hlc,
    seq_counter: Arc<AtomicU64>,
    interval: std::time::Duration,
    presence_visible: Arc<AtomicBool>,
    closing: Arc<AtomicBool>,
    // ZEB-919: owner-state handle for the live epoch-key read; `None` is the
    // documented spawn-key degraded mode (test/legacy wiring).
    crdt_state: Option<Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // `interval` fires immediately on the first `tick()`, giving the
        // "immediate first beacon then every `interval`" cadence for free.
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            if closing.load(Ordering::SeqCst) {
                break;
            }
            // ZEB-600: invisible mode — skip publishing our beacon (the
            // subscriber keeps running, so we still see others). Peers evict us
            // within STALE_MS once we stop publishing. Mirrors the `closing`
            // gate; a live `set_presence_visibility(false)` takes effect here.
            if !presence_visible.load(Ordering::SeqCst) {
                continue;
            }
            let seq = seq_counter.fetch_add(1, Ordering::SeqCst);
            let beacon = build_presence_beacon(self_owner.0, self_device, &started_hlc, seq);
            let Ok(signed) = sign_presence_beacon(beacon, &signing_key) else {
                continue;
            };
            // ZEB-919: seal under the LIVE epoch key per tick (the engine's
            // membership_key is spawn-pinned and never follows rotation);
            // degrade to the spawn key only when the live read is unavailable.
            // If the engine is gone we have nothing to seal under → skip.
            let Some(engine) = registry.engine_arc(&community).await else {
                continue;
            };
            let mk = crate::community_state_sync::community_publish_epoch_key_typed(
                community,
                crdt_state.as_ref(),
                &engine.membership_key(),
            )
            .await;
            let key = crate::community_channel_log::derive_presence_key(&mk, &community);
            let Ok(sealed) = seal_presence_beacon(&key, &community, &signed) else {
                continue;
            };
            if let Err(e) = session.put(&topic, sealed).await {
                tracing::warn!(%topic, err = %e, "community presence publish failed");
            }
        }
    })
}

/// ZEB-620 Task 5: the side effects fired when a presence apply reports a roster
/// device-set change — extracted from the subscriber's `if changed` arm so the
/// edge is unit-testable without standing up a live zenoh session. Bumps the
/// ZEB-599 channel-log backfill resync watch, kicks the reconnect supervisor
/// into a presence sweep (re-arm all non-connected peers), and (ZEB-815) wakes
/// this community's address-book snapshot requester. All three are lossless,
/// non-blocking, and safe with no downstream consumers (`send_modify` with no
/// receivers is a no-op; `supervisor` is `None` on iroh-disabled runs;
/// `notify_one` with no waiter just leaves a permit).
fn on_presence_roster_change(
    resync_tx: &tokio::sync::watch::Sender<u64>,
    supervisor: Option<&SupervisorHandle>,
    addrbook_resync: Option<&Arc<tokio::sync::Notify>>,
) {
    resync_tx.send_modify(|e| *e = e.wrapping_add(1));
    if let Some(sup) = supervisor {
        sup.kick_sweep();
    }
    if let Some(n) = addrbook_resync {
        n.notify_one();
    }
}

/// Spawn a community-presence subscriber on `topic`: open the seal (under the
/// community's current presence key) → verify the device-#2 signature → verify
/// materialized membership → apply to the shared map → emit `presence-updated`
/// on change. Drops on any failure. Mirrors
/// [`crate::voice_presence::spawn_voice_presence_subscriber`], minus mute/kick.
///
/// `session` is an owned, cheaply-cloned `zenoh::Session`. Beacon receipt
/// stamps come from the shared map's own clock (ZEB-972), not a caller-passed
/// one — see [`CommunityPresenceMap::now_ms`].
#[allow(clippy::too_many_arguments)]
pub fn spawn_community_presence_subscriber(
    session: zenoh::Session,
    topic: String,
    registry: Arc<CommunitySyncRegistry>,
    community: SpaceId,
    map: Arc<tokio::sync::Mutex<CommunityPresenceMap>>,
    app: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    closing: Arc<AtomicBool>,
    // ZEB-599 Direction 1: bumped on a roster device-set change so channel-log
    // backfill drivers re-arm with a FULL reconcile — the fast, relay-mediated
    // analogue of the ~1h anti-entropy floor. A bump with no receivers is a
    // harmless no-op (same as `transport_epoch`).
    resync_tx: tokio::sync::watch::Sender<u64>,
    // ZEB-620 Task 5: reconnect-supervisor handle. On the same roster-change edge
    // as `resync_tx`, kicked into a presence sweep (re-arm all non-connected
    // peers) — the identity-free roster edge that recovers a peer whose transport
    // dropped without a registry/zenoh Delete reaching us. `None` for iroh-
    // disabled runs and test callers that bypass `start_node`.
    supervisor: Option<SupervisorHandle>,
    // ZEB-815: this community's address-book resync handle. Fired on the same
    // roster-change edge — a newly-visible device is exactly when a peer with
    // a fuller address book has just become queryable, so it is the cheapest
    // reliable trigger for a snapshot catch-up (spec §2). `None` when the
    // address-book pool is unwired (test callers that bypass `start_node`).
    addrbook_resync: Option<Arc<tokio::sync::Notify>>,
    // ZEB-919: owner-state handle for the live epoch-key candidates; `None`
    // is the documented spawn-key degraded mode (test/legacy wiring).
    crdt_state: Option<Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let sub = match session.declare_subscriber(&topic).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(%topic, err = %e, "community presence subscribe failed");
                return;
            }
        };
        while let Ok(sample) = sub.recv_async().await {
            if sample.payload().len() > crate::voice_crypto::MAX_VOICE_PACKET_BYTES {
                tracing::warn!(
                    len = sample.payload().len(),
                    max = crate::voice_crypto::MAX_VOICE_PACKET_BYTES,
                    "oversized community presence packet dropped"
                );
                continue;
            }
            let bytes = sample.payload().to_bytes().to_vec();
            // ZEB-919: open under the live [current, previous] epoch-key
            // candidates per packet (the engine's membership_key is
            // spawn-pinned and never follows rotation; the previous-key rung
            // keeps un-rotated members visible while the rotation event
            // propagates to them). If the engine is gone we cannot open →
            // drop. Cost note: the candidates read takes the owner-state
            // mutex for a ≤2-key clone at presence cadence (~0.1 Hz per
            // member) — contention noise.
            let Some(engine) = registry.engine_arc(&community).await else {
                continue;
            };
            let mks = crate::community_state_sync::epoch_key_candidates(
                community,
                crdt_state.as_ref(),
                &engine.membership_key(),
            )
            .await;
            let Some(signed) = open_presence_with_any(&mks, &community, &bytes) else {
                continue; // non-member key / wrong scope / tamper → drop
            };
            if verify_presence_beacon_sig(&signed).is_err() {
                continue; // bad device-#2 signature → drop
            }
            let owner = OwnerAddr(signed.beacon.owner);
            // Reuse the voice membership gate verbatim (same crate, same
            // materialized-membership authority) — community presence shares it.
            if !crate::voice_presence::beacon_signer_is_member(
                &registry,
                &community,
                &owner,
                &signed.beacon.device,
            )
            .await
            {
                continue; // signer not an enrolled, Joined member → drop
            }
            let changed = {
                let mut g = map.lock().await;
                // ZEB-972: receipt stamps come from the MAP's clock (not the
                // loop's voice clock) so `online_owners_wall`'s rebase math
                // shares the writer's base.
                let now = g.now_ms();
                g.apply(&community, &signed.beacon, now)
            };
            if changed {
                // ZEB-599 Direction 1: a roster device-set change means a new
                // potential holder just became reachable cross-WAN → kick every
                // channel-log backfill driver into a FULL reconcile (the driver
                // cooldown-gates it, and the ~1h floor stays the backstop).
                // ZEB-620: the same edge also kicks the reconnect supervisor into
                // a presence sweep. Both side effects live in one helper so the
                // edge is unit-testable without a live zenoh session.
                tracing::debug!(
                    target: "harmony_channel",
                    community = %hex::encode(&community.0[..4]),
                    device = %hex::encode(&signed.beacon.device[..4]),
                    supervisor_kicked = supervisor.is_some(),
                    "presence roster change → full-reconcile kick"
                );
                on_presence_roster_change(
                    &resync_tx,
                    supervisor.as_ref(),
                    addrbook_resync.as_ref(),
                );
                let members = {
                    let g = map.lock().await;
                    // ZEB-972: epoch-rebased stamps — the frontend compares
                    // against Date.now().
                    g.online_owners_wall(&community, crate::file_sharing::now_epoch_ms())
                };
                crate::node_event_sink::emit_ser(
                    app.as_ref(),
                    "presence-updated",
                    &PresenceUpdatedPayload::new(community.0, &members),
                );
            }
        }
        if !closing.load(Ordering::SeqCst) {
            tracing::warn!(%topic, "community presence subscriber closed unexpectedly");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_channel_log::derive_presence_key;
    use crate::owner_state_types::EpochKey;
    use ed25519_dalek::SigningKey;

    fn beacon(seq: u64) -> PresenceBeacon {
        PresenceBeacon {
            owner: [0xa1; 16],
            device: [0u8; 32], // overwritten by the signer in real use
            started_hlc: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "aa".repeat(32),
            },
            seq,
        }
    }

    #[test]
    fn sign_then_verify_ok() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(1);
        b.device = sk.verifying_key().to_bytes();
        let signed = sign_presence_beacon(b.clone(), &sk).unwrap();
        assert_eq!(signed.beacon, b);
        verify_presence_beacon_sig(&signed).expect("valid sig");
    }

    #[test]
    fn tampered_sig_rejected() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(1);
        b.device = sk.verifying_key().to_bytes();
        let mut signed = sign_presence_beacon(b, &sk).unwrap();
        signed.beacon.seq = 99; // tamper after signing
        assert_eq!(
            verify_presence_beacon_sig(&signed),
            Err(BeaconError::BadSig)
        );
    }

    #[test]
    fn seal_open_roundtrip_and_wrong_key_drops() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(3);
        b.device = sk.verifying_key().to_bytes();
        let signed = sign_presence_beacon(b, &sk).unwrap();
        let c = SpaceId([0xc0; 16]);
        let key = derive_presence_key(&EpochKey::new([0x11; 32]), &c);
        let sealed = seal_presence_beacon(&key, &c, &signed).unwrap();
        assert_eq!(open_presence_beacon(&key, &c, &sealed), Some(signed));
        let other = derive_presence_key(&EpochKey::new([0x22; 32]), &c);
        assert_eq!(open_presence_beacon(&other, &c, &sealed), None);
    }

    /// ZEB-919: a beacon sealed under the PREVIOUS epoch key (an un-rotated
    /// member) opens via the second candidate; sealed under the CURRENT key
    /// it opens via the first. This is the presence analogue of the ZEB-918
    /// rendezvous previous-epoch rung.
    #[test]
    fn open_presence_with_any_previous_candidate_opens_old_sealed() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(4);
        b.device = sk.verifying_key().to_bytes();
        let signed = sign_presence_beacon(b, &sk).unwrap();
        let c = SpaceId([0xc0; 16]);
        let old = EpochKey::new([0x11; 32]);
        let new = EpochKey::new([0x22; 32]);
        let candidates = vec![new.clone(), old.clone()];

        let sealed_old = seal_presence_beacon(&derive_presence_key(&old, &c), &c, &signed).unwrap();
        assert_eq!(
            open_presence_with_any(&candidates, &c, &sealed_old),
            Some(signed.clone()),
            "previous-epoch candidate must open an un-rotated member's beacon"
        );

        let sealed_new = seal_presence_beacon(&derive_presence_key(&new, &c), &c, &signed).unwrap();
        assert_eq!(
            open_presence_with_any(&candidates, &c, &sealed_new),
            Some(signed),
            "current-epoch candidate must open a rotated member's beacon"
        );
    }

    /// ZEB-919: a beacon sealed under a key NOT in the candidate list (two
    /// epochs back, or a non-member key) drops — the rung is one epoch deep,
    /// never a skeleton key.
    #[test]
    fn open_presence_with_any_rejects_unrelated_key() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(5);
        b.device = sk.verifying_key().to_bytes();
        let signed = sign_presence_beacon(b, &sk).unwrap();
        let c = SpaceId([0xc0; 16]);
        let ancient = EpochKey::new([0x0a; 32]);
        let sealed = seal_presence_beacon(&derive_presence_key(&ancient, &c), &c, &signed).unwrap();
        let candidates = vec![EpochKey::new([0x22; 32]), EpochKey::new([0x11; 32])];
        assert_eq!(open_presence_with_any(&candidates, &c, &sealed), None);
    }

    /// ZEB-919: the publisher's per-tick key selection reads the LIVE
    /// `Space.current_epoch_key`, not the spawn-pinned fallback — a beacon
    /// sealed with the selected key opens under the NEW epoch key after a
    /// rotation lands in owner-state, even though the engine still pins OLD.
    #[tokio::test]
    async fn publisher_seals_under_live_key_when_rotated() {
        use std::sync::Arc;
        let c = SpaceId([0xc0; 16]);
        let spawn_pinned = EpochKey::new([0x11; 32]); // engine's stale copy
        let live = EpochKey::new([0x22; 32]); // post-rotation key

        let mut os = crate::owner_state_crdt::OwnerState::default();
        os.spaces.insert(
            c,
            crate::community_state_sync::test_community_space(c, 1, live.clone()),
        );
        let crdt = Arc::new(tokio::sync::Mutex::new(os));

        let mk = crate::community_state_sync::community_publish_epoch_key_typed(
            c,
            Some(&crdt),
            &spawn_pinned,
        )
        .await;

        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(6);
        b.device = sk.verifying_key().to_bytes();
        let signed = sign_presence_beacon(b, &sk).unwrap();
        let sealed = seal_presence_beacon(&derive_presence_key(&mk, &c), &c, &signed).unwrap();

        assert_eq!(
            open_presence_beacon(&derive_presence_key(&live, &c), &c, &sealed),
            Some(signed),
            "publisher must seal under the live key after rotation"
        );
        assert_eq!(
            open_presence_beacon(&derive_presence_key(&spawn_pinned, &c), &c, &sealed),
            None,
            "the spawn-pinned key must NOT open a post-rotation beacon"
        );
    }

    #[test]
    fn build_presence_beacon_sets_fields() {
        let h = Hlc {
            wall_ms: 5,
            logical: 0,
            device_id: "aa".repeat(32),
        };
        let b = build_presence_beacon([1; 16], [2; 32], &h, 7);
        assert_eq!(b.owner, [1; 16]);
        assert_eq!(b.device, [2; 32]);
        assert_eq!(b.seq, 7);
        assert_eq!(b.started_hlc, h);
    }

    // ── CommunityPresenceMap (Task 2) ───────────────────────────────

    fn b(owner: u8, dev: u8, wall: u64, seq: u64) -> PresenceBeacon {
        PresenceBeacon {
            owner: [owner; 16],
            device: [dev; 32],
            started_hlc: Hlc {
                wall_ms: wall,
                logical: 0,
                device_id: "aa".repeat(32),
            },
            seq,
        }
    }

    #[test]
    fn new_device_marks_online_and_reports_change() {
        let mut m = CommunityPresenceMap::new();
        let c = SpaceId([1; 16]);
        assert!(m.apply(&c, &b(1, 1, 100, 0), 1_000));
        let r = m.online_owners(&c);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].owner, [1; 16]);
        assert_eq!(r[0].device_count, 1);
    }

    #[test]
    fn visibility_defaults_to_visible() {
        // ZEB-600: a fresh map (and its Default) must be VISIBLE. A *derived*
        // Default would be false (Arc<AtomicBool>::default) — invisible — which
        // would silently hide every node. Pin the correct direction.
        assert!(CommunityPresenceMap::new().is_visible());
        assert!(CommunityPresenceMap::default().is_visible());
    }

    #[test]
    fn set_visible_toggles_and_handle_shares_state() {
        let m = CommunityPresenceMap::new();
        let handle = m.visible_handle();
        assert!(handle.load(Ordering::SeqCst));
        m.set_visible(false);
        // A publisher holds this Arc handle; a set_visible flip is observed
        // through it (same atomic) — this is what makes the toggle live.
        assert!(!handle.load(Ordering::SeqCst));
        assert!(!m.is_visible());
        m.set_visible(true);
        assert!(handle.load(Ordering::SeqCst));
        assert!(m.is_visible());
    }

    #[test]
    fn bare_refresh_does_not_report_change() {
        let mut m = CommunityPresenceMap::new();
        let c = SpaceId([1; 16]);
        assert!(m.apply(&c, &b(1, 1, 100, 0), 1_000));
        assert!(!m.apply(&c, &b(1, 1, 100, 1), 2_000)); // same session, newer seq, already online
    }

    #[test]
    fn reordered_old_beacon_rejected() {
        let mut m = CommunityPresenceMap::new();
        let c = SpaceId([1; 16]);
        assert!(m.apply(&c, &b(1, 1, 100, 5), 1_000));
        assert!(!m.apply(&c, &b(1, 1, 100, 3), 2_000)); // stale seq, no change
    }

    #[test]
    fn restart_new_session_supersedes() {
        let mut m = CommunityPresenceMap::new();
        let c = SpaceId([1; 16]);
        assert!(m.apply(&c, &b(1, 1, 100, 9), 1_000));
        // newer started_hlc, seq reset to 0, accepted; owner already online so NOT a visible change:
        assert!(!m.apply(&c, &b(1, 1, 200, 0), 2_000));
        // but last_seen advanced — sweep at 2_000+ttl-1 keeps it:
        assert_eq!(m.online_owners(&c).len(), 1);
    }

    #[test]
    fn sweep_evicts_stale_and_reports() {
        let mut m = CommunityPresenceMap::new();
        let c = SpaceId([1; 16]);
        m.apply(&c, &b(1, 1, 100, 0), 1_000);
        let ev = m.sweep(1_000 + 30_001, 30_000);
        assert_eq!(ev.len(), 1);
        assert!(m.online_owners(&c).is_empty());
    }

    /// ZEB-791: a row stamped in the FUTURE must still be evictable.
    ///
    /// `last_seen_ms` is written locally at receipt, so the only way it exceeds
    /// `now_ms` is a backward local clock step — which is not hypothetical: a
    /// ~975ms backward step was performed on a fleet node as the ZEB-788
    /// remediation. Before the forward bound, `saturating_sub` reported age 0
    /// for the whole skew window and a departed device read as online.
    #[test]
    fn sweep_evicts_future_stamped_entry() {
        let mut m = CommunityPresenceMap::new();
        let c = SpaceId([1; 16]);
        // Beacon recorded at 100_000, then the clock steps back to 10_000.
        m.apply(&c, &b(1, 1, 100, 0), 100_000);
        let ev = m.sweep(10_000, 30_000);
        assert_eq!(ev.len(), 1, "a future-stamped entry must not be immortal");
        assert!(m.online_owners(&c).is_empty());
    }

    #[test]
    fn multi_device_aggregates_to_one_owner() {
        let mut m = CommunityPresenceMap::new();
        let c = SpaceId([1; 16]);
        m.apply(&c, &b(1, 1, 100, 0), 1_000);
        m.apply(&c, &b(1, 2, 100, 0), 1_000);
        let r = m.online_owners(&c);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].device_count, 2);
    }

    #[test]
    fn second_device_for_online_owner_reports_change() {
        // A new device for an ALREADY-online owner changes the owner's
        // device_count (part of the emitted DTO) → must report a change so the
        // subscriber re-emits `presence-updated` (ZEB-537 apply device-count fix).
        let mut m = CommunityPresenceMap::new();
        let c = SpaceId([1; 16]);
        assert!(m.apply(&c, &b(1, 1, 100, 0), 1_000)); // first device → change
        assert!(m.apply(&c, &b(1, 2, 100, 0), 2_000)); // second device, same owner → change
        let r = m.online_owners(&c);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].owner, [1; 16]);
        assert_eq!(r[0].device_count, 2);
    }

    #[test]
    fn remove_community_clears_roster() {
        let mut m = CommunityPresenceMap::new();
        let c = SpaceId([1; 16]);
        m.apply(&c, &b(1, 1, 100, 0), 1_000);
        m.remove_community(&c);
        assert!(m.online_owners(&c).is_empty());
    }

    /// ZEB-620 Task 5: the subscriber's `if changed` edge (a roster device-set
    /// change) fires [`on_presence_roster_change`], which bumps the ZEB-599
    /// resync watch, kicks the reconnect supervisor into a presence sweep
    /// (re-arm all non-connected peers), and — ZEB-815 — wakes the community's
    /// address-book snapshot requester. Drives the same `apply → changed` path
    /// the subscriber runs, then the extracted edge helper, and observes the
    /// sweep on a real `SupervisorHandle`.
    #[tokio::test]
    async fn presence_edge_triggers_sweep() {
        use crate::reconnect_supervisor::SupervisorHandle;

        let mut m = CommunityPresenceMap::new();
        let c = SpaceId([7u8; 16]);
        let (resync_tx, resync_rx) = tokio::sync::watch::channel(0u64);
        let sup = SupervisorHandle::new();
        let addrbook_resync = Arc::new(tokio::sync::Notify::new());

        // A first beacon is a roster device-set change (apply → true): the exact
        // edge the subscriber fires its side effects on.
        let changed = m.apply(&c, &b(1, 1, 100, 0), 1_000);
        assert!(changed, "first-device apply is a roster change");
        assert!(!sup.sweep_pending(), "no sweep before the edge fires");
        on_presence_roster_change(&resync_tx, Some(&sup), Some(&addrbook_resync));
        assert!(
            sup.sweep_pending(),
            "roster change must kick a presence sweep"
        );
        assert_eq!(
            *resync_rx.borrow(),
            1,
            "resync watch bumped alongside the sweep (ZEB-599 parity preserved)"
        );
        // ZEB-815: the addrbook requester's wake is a stored permit, so a
        // `notified()` that starts AFTER the fire still completes immediately.
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            addrbook_resync.notified(),
        )
        .await
        .expect("roster change must wake the addrbook snapshot requester");

        // A stale re-apply of the SAME beacon is not a roster change (apply →
        // false), so the subscriber would not reach the edge → no sweep.
        let sup2 = SupervisorHandle::new();
        let changed2 = m.apply(&c, &b(1, 1, 100, 0), 2_000);
        assert!(!changed2, "duplicate-seq re-apply is not a roster change");
        assert!(
            !sup2.sweep_pending(),
            "no sweep without a roster change edge"
        );

        // Neither side effect installed (iroh-disabled / addrbook-unwired test
        // callers) → no panic.
        on_presence_roster_change(&resync_tx, None, None);
    }

    /// A first beacon for an owner is a roster change (`apply` → true) and
    /// populates the online-owner set.
    #[test]
    fn apply_first_beacon_updates_roster() {
        let mut m = CommunityPresenceMap::new();
        let c = SpaceId([7u8; 16]);
        assert!(m.apply(&c, &b(0x0a, 1, 100, 0), 1_000));
        assert_eq!(m.online_owners(&c).len(), 1);
    }

    // ── online_owners_wall (ZEB-972 Greptile clock-domain fix) ──────

    /// The wire stamp must be Unix-epoch comparable: `online_owners_wall`
    /// rebases the map-clock (monotonic) receipt stamp onto the caller's
    /// epoch clock as `epoch_now − age`.
    #[test]
    fn online_owners_wall_rebases_monotonic_stamps_to_epoch() {
        use std::sync::atomic::AtomicU64;
        let t = Arc::new(AtomicU64::new(5_000));
        let tc = Arc::clone(&t);
        let mut m =
            CommunityPresenceMap::new_with_clock(Arc::new(move || tc.load(Ordering::SeqCst)));
        let c = SpaceId([7u8; 16]);
        let now = m.now_ms();
        assert!(m.apply(&c, &b(0x0a, 1, 100, 0), now));
        // 7 s pass on the map's monotonic clock.
        t.store(12_000, Ordering::SeqCst);
        let epoch_now: u64 = 1_700_000_000_000;
        let r = m.online_owners_wall(&c, epoch_now);
        assert_eq!(r.len(), 1);
        assert_eq!(
            r[0].last_seen_ms,
            epoch_now - 7_000,
            "wall stamp = epoch_now − monotonic age"
        );
    }

    /// Contract pin for the exact defect Greptile caught on PR #722: a stamp
    /// emitted to the frontend must be comparable against `Date.now()`. With
    /// the PRODUCTION clock (`new()`, ms since map creation), a raw
    /// `last_seen_ms` is process-relative (≈0) and `Date.now() − stamp` reads
    /// as decades — `online_owners_wall` must instead land within seconds of
    /// the current epoch time for a just-applied beacon.
    #[test]
    fn online_owners_wall_stamps_are_epoch_comparable() {
        let mut m = CommunityPresenceMap::new();
        let c = SpaceId([7u8; 16]);
        let now = m.now_ms();
        assert!(m.apply(&c, &b(0x0a, 1, 100, 0), now));
        let epoch_now = crate::file_sharing::now_epoch_ms();
        let r = m.online_owners_wall(&c, epoch_now);
        assert_eq!(r.len(), 1);
        let drift = epoch_now.abs_diff(r[0].last_seen_ms);
        assert!(
            drift < 5_000,
            "just-applied beacon must rebase to ~epoch-now (drift {drift} ms)"
        );
    }

    /// Degenerate guard: an age larger than `epoch_now` saturates to 0 rather
    /// than wrapping (u64 underflow would fabricate a far-future stamp).
    #[test]
    fn online_owners_wall_saturates_instead_of_wrapping() {
        use std::sync::atomic::AtomicU64;
        let t = Arc::new(AtomicU64::new(0));
        let tc = Arc::clone(&t);
        let mut m =
            CommunityPresenceMap::new_with_clock(Arc::new(move || tc.load(Ordering::SeqCst)));
        let c = SpaceId([7u8; 16]);
        let now = m.now_ms();
        assert!(m.apply(&c, &b(0x0a, 1, 100, 0), now));
        t.store(10_000, Ordering::SeqCst);
        // epoch_now smaller than the 10 s age → clamp to 0, never wrap.
        let r = m.online_owners_wall(&c, 1_000);
        assert_eq!(r[0].last_seen_ms, 0);
    }
}
