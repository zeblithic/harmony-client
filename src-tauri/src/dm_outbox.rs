//! DM/group-DM outbox orchestrator (ZEB-216 Sub-B Phase 2).
//!
//! Implements the spec at
//! `docs/specs/2026-05-02-zeb-216-sub-b-dm-transport-design.md`
//! §"Module structure / dm_outbox.rs".
//!
//! Phase 2 ships:
//!   - `DmTransport` trait with an in-process `StubTransport` for tests.
//!   - `DmOutbox` orchestrator: `send_dm`, `drain`, `handle_ack`.
//!   - Wall-clock-driven 30-day expiration + per-recipient exponential backoff.
//!
//! Phase 3b will:
//!   - Replace `StubTransport` with a real harmony-runtime adapter that
//!     emits `RuntimeAction::SendUnicastToDevice` per resolved device hash.
//!
//! Inbound demux note (ZEB-710): the direct `handle_unicast` /
//! `handle_cidnotify_lifted` receive handlers were deleted — they had no
//! production callers after the Reticulum teardown. Every live CidNotify
//! producer feeds `dm_inbox_ingest::ingest_dm_packet`, which shares this
//! module's admission/decrypt helpers (`verify_cidnotify_admission`,
//! `decrypt_and_bind_dm_blob`); `handle_invite` / `handle_ack` remain as
//! the invite-accept and ack-application primitives.

use crate::content_store::{ContentStore, ContentStoreError};
use crate::dm_crypto::{compute_aad, encrypt_dm_message, DmEncryptError};
use crate::dm_envelope::MessagePayload;
use crate::owner_state_crdt::{ApplyOutcome, OwnerState, RejectionReason};
use crate::owner_state_types::{
    ContentId, DeliveryStatus, DeviceIdentityHash, Hlc, OutboxEntry, OutboxEntryId, OwnerAddr,
    OwnerDeviceCache, SpaceId, SpaceKind,
};
use async_trait::async_trait;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

pub type MessageId = OutboxEntryId;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("transport temporarily unavailable: {0}")]
    Transient(String),
    /// ZEB-525: transient failure where NO live delivery attempt was even
    /// launched (no reachable tunnel targets, tunnel-send capacity shed, or a
    /// deposit-only interim transport). Backoff treatment is identical to
    /// [`Transient`](Self::Transient); the difference is deposit candidacy —
    /// drain's Err-arm skips the one-backoff-window grace and deposits on the
    /// FIRST failure, because the grace window only exists to give an
    /// in-flight live attempt a chance to win before burning a deposit.
    #[error("transport temporarily unavailable (no live attempt): {0}")]
    TransientNoLiveAttempt(String),
    #[error("transport permanently failed: {0}")]
    Permanent(String),
}

#[async_trait]
pub trait DmTransport: Send + Sync {
    /// Send `entry` to `recipient`'s pre-resolved `destinations`. The
    /// caller (drain) resolves OwnerAddr → device-hash list before
    /// invoking — see `resolve_destinations` below. Empty `destinations`
    /// must be filtered out by the caller (drain treats empty as a
    /// transient resolver miss and bumps backoff without calling send).
    ///
    /// Resolution is split out of the transport (was inside
    /// `RuntimeUnicastTransport::send` via an injected `DestinationResolver`
    /// in the original Phase 3b shape) because production drain holds
    /// `OwnerState`'s mutex via `&mut OwnerState`, and the production
    /// resolver also needed to read `OwnerState` — which deadlocked with
    /// `try_lock` on the same Tokio mutex. Resolving inside drain reads
    /// directly from the held `&OwnerState` reference, no locking
    /// required.
    async fn send(
        &self,
        entry: &OutboxEntry,
        recipient: OwnerAddr,
        destinations: Vec<[u8; 16]>,
    ) -> Result<(), TransportError>;
}

/// Resolve `recipient` → list of 16-byte Reticulum destination hashes
/// from `OwnerDeviceCache`. Each cached `DeviceIdentityHash` maps to its
/// destination via `compute_dm_destination_hash` (Task 10). Empty Vec
/// when no entry is known — drain treats that as a transient miss and
/// bumps backoff so a future tick (after Flow A propagates the missing
/// entry) retries.
///
/// Pure function: no locking, no `&mut`. Drain calls this with the
/// `&OwnerState` it already has from its mutex guard, sidestepping the
/// recursive-lock deadlock that lived in the original Phase 3b shape.
pub fn resolve_destinations(cache: &OwnerDeviceCache, recipient: OwnerAddr) -> Vec<[u8; 16]> {
    cache
        .devices
        .get(&recipient)
        .map(|entry| {
            entry
                .devices
                .iter()
                .map(|d| crate::dm_signing::compute_dm_destination_hash(d.0))
                .collect()
        })
        .unwrap_or_default()
}

/// In-process transport for Phase 2 tests + the in-process Tauri integration
/// test harness. Records every send call so tests can assert on them, and lets
/// the test pre-seed an outcome (Ok or Transient/Permanent error) per
/// (entry_id, recipient) pair.
#[derive(Default)]
pub struct StubTransport {
    inner: Mutex<StubInner>,
}

#[derive(Default)]
struct StubInner {
    /// Bounded ring buffer of recorded sends. `StubTransport` is wired into
    /// `start_node` as the production Phase 2 transport, so a long-lived node
    /// would otherwise accumulate one entry per send call forever. Capped at
    /// `STUB_MAX_RECORDED_SENDS` (~32KB worst case at 32B/entry × 1024); on
    /// overflow the oldest entry is `pop_front`ed before `push_back`. No test
    /// asserts a `sends` count above ~10, so the cap is non-disruptive.
    sends: VecDeque<(OutboxEntryId, OwnerAddr)>,
    /// Pre-seeded outcomes; if absent, default = Ok(()).
    outcomes: HashMap<(OutboxEntryId, OwnerAddr), Result<(), TransportError>>,
}

impl StubTransport {
    /// FIFO cap on `StubInner::sends` to keep the production stub bounded.
    const STUB_MAX_RECORDED_SENDS: usize = 1024;

    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-seed the outcome for the next `send(entry_id, recipient)` call.
    pub fn set_outcome(
        &self,
        entry_id: OutboxEntryId,
        recipient: OwnerAddr,
        outcome: Result<(), TransportError>,
    ) {
        self.inner
            .lock()
            .expect("StubTransport poisoned")
            .outcomes
            .insert((entry_id, recipient), outcome);
    }

    /// Snapshot all recorded sends (in call order, oldest first).
    pub fn sends(&self) -> Vec<(OutboxEntryId, OwnerAddr)> {
        self.inner
            .lock()
            .expect("StubTransport poisoned")
            .sends
            .iter()
            .copied()
            .collect()
    }
}

// `TransportError` is not Clone (thiserror + io-style errors rarely are).
// `remove` instead of `get/clone` so each pre-seeded outcome fires once;
// repeat calls without re-seeding fall through to the default Ok(()).
//
// Stub ignores `destinations` — its purpose is to record (entry, recipient)
// for unit-test assertions; per-device fan-out is exercised by the
// integration test against `RuntimeUnicastTransport`.
#[async_trait]
impl DmTransport for StubTransport {
    async fn send(
        &self,
        entry: &OutboxEntry,
        recipient: OwnerAddr,
        _destinations: Vec<[u8; 16]>,
    ) -> Result<(), TransportError> {
        let mut inner = self.inner.lock().expect("StubTransport poisoned");
        if inner.sends.len() >= Self::STUB_MAX_RECORDED_SENDS {
            inner.sends.pop_front();
        }
        inner.sends.push_back((entry.id, recipient));
        inner
            .outcomes
            .remove(&(entry.id, recipient))
            .unwrap_or(Ok(()))
    }
}

/// Payload pushed by `RuntimeUnicastTransport` into the event-loop's
/// outbound channel. Task 7 wires the receiver into `event_loop` which
/// translates each request into `RuntimeEvent::SendUnicastToDevice` for
/// `NodeRuntime`. Per-destination FIFO + cross-destination best-effort
/// ordering inherits from ZEB-226's runtime.
#[derive(Debug, Clone)]
pub struct UnicastSendRequest {
    pub destination_hash: [u8; 16],
    pub packet: Vec<u8>,
}

/// Production `DmTransport` adapter (ZEB-227 Phase 3b). Per `send`:
///
/// 1. Build a `DmCidNotifySigned` whose `signing_device_hash` is our
///    device's identity hash (single-device `sender_devices` for Phase
///    3b — cross-device piggyback grows automatically as Flow A
///    propagates more entries; see spec §"Public-key storage on
///    OwnerDeviceCache").
/// 2. Sign + canonical-CBOR-encode via
///    `dm_envelope::build_signed_cidnotify` + `encode_packet`.
/// 3. Push one `UnicastSendRequest` per destination hash into `tx`,
///    which the event-loop drains and forwards to `NodeRuntime`.
///
/// Resolution of `recipient: OwnerAddr` → `destinations: Vec<[u8; 16]>`
/// happens UPSTREAM in `DmOutbox::drain` (which has `&OwnerState` in
/// scope from its mutex guard). Original Phase 3b shape had the
/// transport own a `DestinationResolver` that also wanted to lock
/// `OwnerState` — recursive `try_lock` on the same Tokio mutex always
/// failed → empty Vec → no DMs ever delivered. Splitting resolution out
/// of the transport sidesteps that deadlock; see `resolve_destinations`
/// above.
///
/// `DmInvite` outbound is Phase 4's `add_space` IPC for DM kinds
/// (spec Flow 1). `DmAck` outbound was built directly by the receive-side
/// direct handler (ack fan-out removed by ZEB-473; the handler itself
/// deleted in ZEB-710) — it bypassed `DmTransport::send` because acks are
/// not tied to an `OutboxEntry` retry loop.
pub struct RuntimeUnicastTransport {
    tx: tokio::sync::mpsc::Sender<UnicastSendRequest>,
    self_owner: OwnerAddr,
    our_signing_device_hash: DeviceIdentityHash,
    signing_key: Arc<ed25519_dalek::SigningKey>,
}

impl RuntimeUnicastTransport {
    pub fn new(
        tx: tokio::sync::mpsc::Sender<UnicastSendRequest>,
        self_owner: OwnerAddr,
        our_signing_device_hash: DeviceIdentityHash,
        signing_key: Arc<ed25519_dalek::SigningKey>,
    ) -> Self {
        Self {
            tx,
            self_owner,
            our_signing_device_hash,
            signing_key,
        }
    }
}

#[async_trait]
impl DmTransport for RuntimeUnicastTransport {
    async fn send(
        &self,
        entry: &OutboxEntry,
        recipient: OwnerAddr,
        destinations: Vec<[u8; 16]>,
    ) -> Result<(), TransportError> {
        // Empty destinations → no known devices for this recipient.
        // Surface as Transient so the outbox backoff drives a future
        // retry once Flow A propagates the missing OwnerDeviceCache
        // entry. (StubTransport ignores `destinations` and returns
        // pre-seeded outcomes — this branch only fires for the real
        // production transport path.)
        if destinations.is_empty() {
            return Err(TransportError::Transient(format!(
                "no known devices for recipient {recipient:?}"
            )));
        }
        // ZEB-505: this test-only transport builds a CidNotify, so it handles
        // message entries only (invite-only entries have no `message_cid`).
        let Some(message_cid) = entry.message_cid else {
            return Err(TransportError::Transient(
                "RuntimeUnicastTransport (test-only) does not handle invite-only entries".into(),
            ));
        };
        let signed = crate::dm_envelope::DmCidNotifySigned {
            space_id: entry.space_id,
            message_cid,
            sender_owner_addr: self.self_owner,
            // `RuntimeUnicastTransport` is test-only (no production `::new`
            // call site — the Reticulum direct path it served was torn out in
            // ZEB-474). The production CidNotify builders — the deposit path
            // (`build_cidnotify_packet_bytes`) and the live tunnel
            // (`IrohTunnelDmTransport::send`) — carry the sender's FULL cached
            // device set via `resolve_sender_devices` (ZEB-506) so the
            // recipient's LWW-replace cache refresh never shrinks a multi-device
            // sender. This synthetic transport keeps the bare singleton: it has
            // no `OwnerState` to resolve against, and tests that need a
            // multi-device set seed the receiver cache directly.
            sender_devices: vec![self.our_signing_device_hash],
            signing_device_hash: self.our_signing_device_hash,
        };
        let wire = build_dm_packet(signed, &self.signing_key).map_err(TransportError::Permanent)?;

        for destination_hash in destinations {
            // Use try_send (not send().await) because this transport runs
            // inside the event-loop task that ALSO drains
            // `unicast_send_rx`. .await on a full channel would deadlock
            // the event loop on itself. Transient errors flow back into
            // DmOutbox::drain's per-recipient backoff, which retries on
            // the next tick once the channel has drained.
            self.tx
                .try_send(UnicastSendRequest {
                    destination_hash,
                    packet: wire.clone(),
                })
                .map_err(|e| match e {
                    tokio::sync::mpsc::error::TrySendError::Full(_) => {
                        TransportError::Transient("unicast channel full".to_string())
                    }
                    // Closed channel = event-loop receiver dropped (runtime
                    // shutdown / panic). Permanent because retry will never
                    // succeed; the OutboxEntry surfaces failure once instead
                    // of spinning every drain tick.
                    tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                        TransportError::Permanent(format!("event-loop channel closed: {e}"))
                    }
                })?;
        }
        Ok(())
    }
}

/// ZEB-474 (coalescence Move 2): the Reticulum unicast carrier is gone.
/// In the interim before Move 1a (ZEB-473) brings up a live iroh-tunnel DM
/// carrier, DM delivery is store-and-forward only — the outbox's deposit
/// rung (butler → community-relay) carries the signed cidnotify to the
/// recipient over iroh and marks the entry delivered on butler-ack.
///
/// This transport is therefore a no-op "direct send" that always signals
/// `TransientNoLiveAttempt`. Returning an error (not `Ok`) is deliberate: an
/// `Ok` would steer the pair into the "sent, awaiting ack" arm, which only
/// deposits from `DEPOSIT_NOACK_WINDOWS` (= 2) windows onward — strictly
/// later, weakening durability. The `NoLiveAttempt` flavor (ZEB-525) is
/// equally deliberate: this transport by definition launches no live attempt,
/// so the deposit fires on the FIRST drain pass instead of waiting out the
/// one-backoff-window grace that only exists to let a live attempt win.
///
/// Move 1a replaces this with `IrohTunnelDmTransport` on the same
/// `DmTransport` seam — no other outbox code changes.
pub struct DepositOnlyDmTransport;

#[async_trait]
impl DmTransport for DepositOnlyDmTransport {
    async fn send(
        &self,
        _entry: &OutboxEntry,
        _recipient: OwnerAddr,
        _destinations: Vec<[u8; 16]>,
    ) -> Result<(), TransportError> {
        Err(TransportError::TransientNoLiveAttempt(
            "deposit-only interim (ZEB-474): no direct DM carrier; \
             routing via butler/community-relay deposit"
                .to_string(),
        ))
    }
}

/// Build the sealed+signed DM wire bytes for a `DmCidNotifySigned` —
/// `build_signed_cidnotify` (sign with the Reticulum device signing key)
/// → `encode_packet` (canonical-CBOR framing). ZEB-473 Task 8 factored this
/// out of the deposit path (`DmOutbox::build_cidnotify_packet_bytes`) so the
/// live tunnel carrier (`IrohTunnelDmTransport`) produces byte-identical
/// packets — the recipient's verify/decrypt/ingest pipeline sees exactly
/// what a deposit arrival would carry, and CRDT-inbox dedup collapses the
/// deposit copy with the tunnel copy.
///
/// Encode is effectively infallible (fixed-shape struct over canonical CBOR),
/// but both inner calls are fallible, so this returns `Result<_, String>`
/// (matching the deposit path's existing error propagation) rather than
/// panicking.
pub(crate) fn build_dm_packet(
    signed: crate::dm_envelope::DmCidNotifySigned,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<Vec<u8>, String> {
    let packet = crate::dm_envelope::build_signed_cidnotify(signed, signing_key)
        .map_err(|e| format!("build_signed_cidnotify: {e}"))?;
    crate::dm_envelope::encode_packet(&packet).map_err(|e| format!("encode_packet: {e}"))
}

/// ZEB-484: build a `CidNotifyWithBlob` wire packet — the signed CidNotify plus
/// the encrypted `storage_blob` inline. Parallel to `build_dm_packet`.
pub(crate) fn build_dm_packet_with_blob(
    signed: crate::dm_envelope::DmCidNotifySigned,
    signing_key: &ed25519_dalek::SigningKey,
    storage_blob: Vec<u8>,
) -> Result<Vec<u8>, String> {
    let packet =
        crate::dm_envelope::build_signed_cidnotify_with_blob(signed, signing_key, storage_blob)
            .map_err(|e| format!("build_signed_cidnotify_with_blob: {e}"))?;
    crate::dm_envelope::encode_packet(&packet).map_err(|e| format!("encode_packet: {e}"))
}

/// ZEB-504: resolve the inviter's own `sender_devices` list for a
/// `DmInviteSigned`, sourced from the live `OwnerDeviceCache` (authoritative)
/// with a pre-bootstrap singleton fallback, plus the Phase-3b defense-in-depth
/// invariant that the signing device is always present.
///
/// Shared by `add_space_dm_inner`'s original-invite path and
/// [`build_invite_packet_from_space`]'s rebuild path so the two can never
/// diverge. They DID diverge — the rebuild path hard-coded a bare singleton —
/// and that was a device-list-regression bug: a rebuilt invite re-driven over
/// the live tunnel (ZEB-504) is applied receiver-side with
/// `refresh_owner_device_cache = true` at a fresh *local* `learned_at` HLC, and
/// [`crate::owner_state_crdt::OwnerState::apply_owner_device_update`] is
/// LWW-by-`learned_at` and REPLACES (not unions) the device list. A singleton
/// resend would therefore shrink the receiver's cached inviter device set down
/// to one device, dropping later messages signed by the inviter's other devices
/// as `UnknownSigningKey`. (The deposit-recover path applies with
/// `refresh = false`, so it never mutates the cache — but it shares this helper
/// for a consistent invite shape, which is harmless there.)
///
/// Behavior-preserving extraction of `add_space_dm_inner`'s prior inline logic:
/// the cache stores `devices` already sorted+deduped, so the common branch
/// returns it as-is; only the (rare) fallback that must append the signing hash
/// re-sorts and re-caps to `MAX_DEVICES_PER_OWNER` (ZEB-506: the decoder rejects
/// an over-cap `sender_devices`, so appending the signer must never overflow).
pub(crate) fn resolve_sender_devices(
    state: &OwnerState,
    self_owner: OwnerAddr,
    our_signing_device_hash: DeviceIdentityHash,
) -> Vec<DeviceIdentityHash> {
    let our_devices: Vec<DeviceIdentityHash> = state
        .owner_device_cache
        .devices
        .get(&self_owner)
        .map(|e| e.devices.clone())
        .unwrap_or_else(|| vec![our_signing_device_hash]);
    if our_devices.contains(&our_signing_device_hash) {
        our_devices
    } else {
        let mut combined = our_devices;
        combined.push(our_signing_device_hash);
        combined.sort();
        combined.dedup();
        // ZEB-506 (Qodo): the cached set is already capped at
        // MAX_DEVICES_PER_OWNER, so appending a missing signer can push this to
        // MAX + 1. The CidNotify / Invite decoder rejects any packet whose
        // `sender_devices.len() > MAX_DEVICES_PER_OWNER` AND requires the signer
        // to be present — so re-cap by evicting the largest NON-signer
        // device(s), never the signer. (Removing from a sorted Vec preserves
        // the sort, so no re-sort is needed.)
        while combined.len() > crate::owner_state_types::MAX_DEVICES_PER_OWNER {
            match combined.iter().rposition(|d| *d != our_signing_device_hash) {
                Some(pos) => {
                    combined.remove(pos);
                }
                // Unreachable after dedup (the signer appears at most once, so a
                // list longer than 1 always has a non-signer to evict); break
                // rather than loop forever if that invariant ever changes.
                None => break,
            }
        }
        combined
    }
}

/// ZEB-504: reconstruct a signed `DmInvite` wire packet for `space_id` from the
/// durable Space record. Shared by the deposit rung
/// ([`DmOutbox::build_invite_packet_bytes`]) and the live PQ-tunnel transport
/// (`iroh_tunnel_dm_transport::IrohTunnelDmTransport::send`) so the cold
/// first-contact invite is re-driven over the *warming* tunnel byte-for-byte the
/// same way the deposit path rebuilds it — closing the gap where the live tunnel
/// carried only the CidNotify and the recipient bounced it with `SpaceNotFound`.
///
/// `Ok(None)` for a genuinely non-DM Space OR a missing Space record (the caller
/// then carries only the CidNotify — a vanished record can't be classified as a
/// DM, and is unreachable for a real outbox entry whose DM Space is
/// fleet-replicated alongside it). `Err` for a DM/GroupDm Space that EXISTS but
/// has no `content_key`, or a sign/encode failure — the invite is load-bearing
/// there, so the caller must NOT silently drop it.
///
/// ZEB-580 S1: `inviter_enrollment` attaches the inviter's own #2
/// `EnrollmentCert` (boxed to keep the `DmPacket::Invite` variant small) so an
/// updated receiver verifies the invite via the master-attested cert (Task 3
/// Check B). The caller pairs this with a #2 `signing_key` / `our_signing_device_hash`
/// / `inviter_identity_pub` (the cert's #2 combined pub) so the invite is
/// self-consistent. On the #3 fallback path the caller passes `None` here and
/// the legacy #3 pub, preserving pre-migration wire bytes exactly.
pub(crate) fn build_invite_packet_from_space(
    state: &OwnerState,
    space_id: &SpaceId,
    signing_key: &ed25519_dalek::SigningKey,
    self_owner: OwnerAddr,
    our_signing_device_hash: DeviceIdentityHash,
    inviter_identity_pub: [u8; 64],
    inviter_enrollment: Option<Box<harmony_owner::certs::EnrollmentCert>>,
) -> Result<Option<Vec<u8>>, String> {
    let Some(space) = state.spaces.get(space_id) else {
        return Ok(None);
    };
    if !matches!(space.kind, SpaceKind::Dm | SpaceKind::GroupDm) {
        return Ok(None);
    }
    let content_key = space
        .content_key
        .clone()
        .ok_or_else(|| format!("DM space {space_id:?} has no content_key"))?;
    let signed = crate::dm_envelope::DmInviteSigned {
        space_id: space.id,
        kind: space.kind,
        members: space.members.clone(),
        inviter: self_owner,
        content_key,
        // ZEB-504: carry the inviter's FULL cached device set, sourced exactly
        // like `add_space_dm_inner`'s original invite (NOT a bare singleton) —
        // see `resolve_sender_devices` for why a singleton here would
        // LWW-shrink the receiver's OwnerDeviceCache on the live-tunnel resend.
        sender_devices: resolve_sender_devices(state, self_owner, our_signing_device_hash),
        created_at: space.created_at.clone(),
        signing_device_hash: our_signing_device_hash,
        inviter_identity_pub,
        inviter_enrollment,
    };
    crate::dm_envelope::build_signed_invite(signed, signing_key)
        .and_then(|p| crate::dm_envelope::encode_packet(&p))
        .map(Some)
        .map_err(|e| format!("invite rebuild failed: {e}"))
}

#[derive(Debug, Clone, Copy)]
struct AttemptState {
    last_attempt_wall_ms: u64,
    failure_count: u32,
}

const BACKOFF_BASE_MS: u64 = 5_000; // 5s
const BACKOFF_MULTIPLIER: u64 = 2;
const BACKOFF_CAP_MS: u64 = 5 * 60 * 1_000; // 5 min
const BACKOFF_MAX_EXPONENT: u32 = 8; // 5s * 2^8 = 1280s -> capped at 5min
pub const EXPIRATION_MS: u64 = 30 * 24 * 60 * 60 * 1_000; // 30 days
/// ZEB-246: minimum age before an in-flight (`Pending`/`Partial`) OutboxEntry
/// may be manually deleted. Below this, `delete_dm_outbox_entry` rejects with
/// `NotYetStuck` so a direct IPC call can't turn manual-cleanup into an
/// unsend/cancel-delivery primitive (outside Phase 4's documented contract).
/// Mirrors the UI's `TextMessage.svelte:canDelete` 60s "stuck" threshold — the
/// duplication is deliberate defense-in-depth: the backend must not trust the
/// frontend to be the only gate (a devtools/extension/future code path could
/// call the IPC directly). `Expired`/terminal entries bypass this (they are
/// stuck by definition); `Complete` continues to error with `AlreadyDelivered`.
pub const STUCK_THRESHOLD_MS: u64 = 60_000; // 60s
/// ZEB-703 (PR #485 Greptile P1): cap on concurrent detached Phase C tasks
/// (one permit each, held for the task's lifetime incl. deposit rungs).
/// Drain ticks fire every ~250ms and Phase C is usually sub-second, so 64
/// outstanding means pathological wedging — the tick then skips its Phase C
/// with a WARN rather than spawning unfenced. The shutdown barrier
/// `acquire_many`s this many to await drain-path quiescence.
pub(crate) const DRAIN_PHASE_C_FENCE_CAPACITY: usize = 64;

/// ZEB-710: process-lived counters for the ZEB-703 fence's two documented
/// degraded modes, which were previously WARN-log-only:
///
/// 1. Phase-C fence exhaustion — `DRAIN_PHASE_C_FENCE_CAPACITY` detached
///    Phase C tasks wedged means a drain tick skips Phase C entirely
///    (safe direction, but pathological wedging deserves a metric).
/// 2. `stop_inner` finding the outbox lock contended and degrading to
///    no-fence (a Phase C mutation may race the final persist).
///
/// Process-global (not outbox-lived) deliberately: the stop arm fires while
/// the node is tearing down, so an outbox-lived counter would die before
/// any snapshot could read it — a restart within the same process surfaces
/// the previous stop's skip in the next boot's network-health snapshot.
/// Registered with `NetworkHealthService` at boot via
/// `set_dm_fence_source`.
pub(crate) struct DmFenceStats {
    phase_c_saturated_skips: std::sync::atomic::AtomicU64,
    stop_fence_skipped_contended: std::sync::atomic::AtomicU64,
}

impl DmFenceStats {
    /// Test-only: a private instance so snapshot-mapping tests can assert
    /// exact values instead of deltas against the process-global.
    #[cfg(test)]
    pub(crate) fn new_for_source() -> Self {
        Self {
            phase_c_saturated_skips: std::sync::atomic::AtomicU64::new(0),
            stop_fence_skipped_contended: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub(crate) fn record_phase_c_saturated_skip(&self) {
        self.phase_c_saturated_skips
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    pub(crate) fn record_stop_fence_skipped_contended(&self) {
        self.stop_fence_skipped_contended
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    pub(crate) fn phase_c_saturated_skips(&self) -> u64 {
        self.phase_c_saturated_skips
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    pub(crate) fn stop_fence_skipped_contended(&self) -> u64 {
        self.stop_fence_skipped_contended
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// ZEB-710: the process-global instance (see [`DmFenceStats`] for why it is
/// not outbox-lived). `Arc` so the network-health registry can hold the
/// same allocation via its additive `set_*_source` pattern.
pub(crate) static DM_FENCE_STATS: std::sync::LazyLock<Arc<DmFenceStats>> =
    std::sync::LazyLock::new(|| {
        Arc::new(DmFenceStats {
            phase_c_saturated_skips: std::sync::atomic::AtomicU64::new(0),
            stop_fence_skipped_contended: std::sync::atomic::AtomicU64::new(0),
        })
    });

#[derive(Debug, Default, PartialEq, Eq)]
pub struct DrainOutcome {
    /// `(space_id, message_cid, recipient_owner_addr)` triples whose
    /// `delivered_to` was just set this tick. Caller emits
    /// `dm-delivered` IPC events with this exact field set (ZEB-231:
    /// spec-compliant identifier is `(space_id, message_cid)`, not
    /// the internal `OutboxEntryId`).
    pub newly_delivered: Vec<(SpaceId, ContentId, OwnerAddr)>,
    /// `(space_id, message_cid)` pairs for entries that transitioned
    /// to Expired this tick. Caller emits `dm-expired` IPC events.
    pub newly_expired: Vec<(SpaceId, ContentId)>,
}

/// ZEB-233: A single `(entry, recipient)` work unit produced by drain's
/// Phase A. Carries everything Phase B needs to perform an unlocked
/// `transport.send().await` — the OutboxEntry clone (so Phase B doesn't
/// need to re-read `state.outbox`) plus pre-resolved destinations.
#[derive(Debug, Clone)]
pub struct DrainWorkUnit {
    pub entry_id: OutboxEntryId,
    pub entry_clone: OutboxEntry,
    pub recipient: OwnerAddr,
    pub destinations: Vec<[u8; 16]>,
}

/// ZEB-233: The outcome of one `transport.send().await` call, paired
/// with the (entry, recipient) it targeted. Phase C consumes these to
/// update backoff + clear in_flight markers under the re-acquired locks.
#[derive(Debug)]
pub struct DrainSendResult {
    pub entry_id: OutboxEntryId,
    pub recipient: OwnerAddr,
    pub result: Result<(), TransportError>,
}

/// Phase 4 — outcome of `DmOutbox::delete_dm_outbox_entry`.
///
/// The IPC layer reads this to decide which `dm-deleted` IPC event to
/// emit. All fields are `Option` so a no-op delete (idempotent missing-
/// id call) returns `Default::default()` and the caller knows to emit
/// nothing.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DeleteDmOutboxOutcome {
    pub deleted_outbox_id: Option<OutboxEntryId>,
    pub deleted_inbox_key: Option<crate::owner_state_types::InboxKey>,
    pub space_id: Option<SpaceId>,
    pub message_cid: Option<crate::owner_state_types::ContentId>,
}

/// Phase 4 — error type for `DmOutbox::delete_dm_outbox_entry`.
///
/// `AlreadyDelivered`: the targeted OutboxEntry is `DeliveryStatus::Complete`,
/// meaning every recipient already acknowledged. Manual-delete must NOT remove
/// delivered self-history — the user's UX expectation for "delete" of a
/// stuck/expired message does not extend to wiping messages that successfully
/// shipped (and that may already be replicated to a paired device). The IPC
/// layer surfaces this as an Err so the caller can distinguish "we erased a
/// stuck/expired thing" from "this message already made it; refusing to erase
/// delivered history."
///
/// Missing entries remain the idempotent success case (returns
/// `Ok(DeleteDmOutboxOutcome::default())`) — a stale UI/IPC retry must not
/// be elevated to an error.
#[derive(Debug, thiserror::Error)]
pub enum DeleteDmError {
    #[error("message {0:?} is already complete (all recipients acked); refusing to erase delivered self-history")]
    AlreadyDelivered(OutboxEntryId),
    /// ZEB-246: an in-flight (`Pending`/`Partial`) entry younger than
    /// `STUCK_THRESHOLD_MS`. Manual delete targets stuck/expired entries;
    /// deleting a fresh in-flight entry would be an unsend/cancel-delivery
    /// primitive outside Phase 4's contract. `age_ms`/`threshold_ms` are
    /// surfaced so the IPC/UI can explain the wait.
    #[error("message is still in flight ({age_ms}ms old, must be {threshold_ms}ms to delete); wait for it to expire or complete")]
    NotYetStuck { age_ms: u64, threshold_ms: u64 },
}

/// Per-process DM-outbox state. One instance per running node, shared between
/// the IPC handler (writes via `send_dm`) and the event-loop drain tick.
///
/// `OwnerState` is held in a separate `Arc<tokio::sync::Mutex<OwnerState>>`
/// (constructed in `start_node`) and passed in by callers that have just
/// acquired its lock. This `DmOutbox` owns only ephemeral per-process state
/// (in-flight set, backoff timestamps); CRDT state lives in `OwnerState`.
pub struct DmOutbox {
    pub(crate) device_id: String,
    pub(crate) self_owner: OwnerAddr,
    /// Phase 3b: our device-Identity hash, used as the
    /// `signing_device_hash` on outbound DM packets (drain's CidNotify
    /// builds; legacy #3 DM signing per `dm_signing_material`). Mirrors
    /// `RuntimeUnicastTransport`'s field of the same name; both are
    /// populated from the same identity-management site in production
    /// (Task 11 wires `lib.rs::start_node`).
    pub(crate) our_signing_device_hash: DeviceIdentityHash,
    /// Phase 3b: our device's Ed25519 signing key (#3), used to sign
    /// outbound DM packets built by the drain path. Held via `Arc` so the
    /// outbox can outlive any single owning context —
    /// `RuntimeUnicastTransport` holds it the same way.
    pub(crate) signing_key: Arc<ed25519_dalek::SigningKey>,
    /// Phase 4 (ZEB-262): full `PrivateIdentity` snapshot, parallel to
    /// `signing_key`. The receive-side counter-sign path
    /// (`community_invite::handle_unicast` →
    /// `community_membership::attach_countersig_with_identity`) needs a
    /// `&harmony_identity::PrivateIdentity`, not just an
    /// `Arc<SigningKey>`. Held alongside `signing_key` so the inbound
    /// CommunityInvite handler can grab a reference under the dm_outbox
    /// lock without re-loading the on-disk identity. Both fields MUST
    /// be derived from the same identity bytes — the
    /// `dm_outbox_holds_private_identity_for_countersign` test asserts
    /// they produce identical signatures for the same message, and
    /// `redeem_invite_inner` snapshots both fields under the outbox
    /// lock to feed `build_signed_invite_packet`.
    pub(crate) private_identity: Arc<harmony_identity::PrivateIdentity>,
    /// ZEB-339: the harmony-owner ENROLLED device signing key (#2). Distinct
    /// from `signing_key` (the Reticulum/transport key, #3). Community
    /// membership events sign with this; DM/transport keep `signing_key`.
    pub(crate) community_signing_key: Arc<ed25519_dalek::SigningKey>,
    /// ZEB-339: this device's own Master EnrollmentCert (owner_id -> device #2),
    /// attached to outbound identity-introducing events (bootstrap/redeem Join,
    /// PendingJoin).
    pub(crate) enrollment_cert: harmony_owner::certs::EnrollmentCert,
    /// ZEB-580 S1: this device's #2 DM hash, computed from `enrollment_cert`.
    /// `None` when the cert has no usable X25519 (synthetic/test certs) — then
    /// DM body signing degrades to the legacy #3 (`signing_key` /
    /// `our_signing_device_hash`) via [`DmOutbox::dm_signing_material`].
    /// Populated identically in both `new` and `new_synthetic`.
    pub(crate) our_device2_signing_hash: Option<DeviceIdentityHash>,
    in_flight: HashSet<(OutboxEntryId, OwnerAddr)>,
    backoff: HashMap<(OutboxEntryId, OwnerAddr), AttemptState>,
    /// ZEB-703 (PR #485 Greptile P1): shutdown gate for the drain path.
    /// Once set, `drain_lifted` skips the whole tick (no sends, no Phase C
    /// spawn) — mirroring the ZEB-234 `dm_send_stopping` flag's role for
    /// the IPC paths. Set by `/v1/shutdown`'s pre-ack barrier and by
    /// `stop_inner`; never cleared (the outbox dies with the node).
    shutdown_gate: Arc<std::sync::atomic::AtomicBool>,
    /// ZEB-703 (PR #485 Greptile P1): Phase C in-flight fence. Every
    /// detached Phase C task (drain outcomes + deposit rungs — all the
    /// drain-path CRDT mutation sites) holds one permit for its lifetime;
    /// the shutdown barrier `acquire_many(DRAIN_PHASE_C_FENCE_CAPACITY)`s
    /// to wait for them BEFORE the pre-ack owner-state snapshot, so a
    /// mid-flight delivery/expiry/ack transition can't land after the
    /// persist and be lost to a kill-on-200 supervisor. Same pattern as
    /// the ZEB-234 send fence, scoped to the drain path.
    phase_c_inflight: Arc<tokio::sync::Semaphore>,
    /// ZEB-418 SP2 P1 Task 8: sender-side butler deposit client. `None`
    /// (default) disables the deposit rung entirely — drain behaves exactly
    /// as before. Production injects `IrohButlerDepositClient` via
    /// `set_butler_deposit_client` at start_node (only when the iroh
    /// endpoint bound); outbox tests inject a mock.
    butler_deposit_client: Option<Arc<dyn crate::butler_deposit::ButlerDepositClient>>,
    /// ZEB-458 P4 Phase B: last-resort community-relay deposit client. `None`
    /// (default) disables the relay rung entirely. Fires ONLY when the butler
    /// rung did not ack for a given candidate. Production injects the real
    /// client via `set_community_relay_deposit_client` at start_node; tests
    /// inject a mock.
    community_relay_deposit_client:
        Option<Arc<dyn crate::community_relay::CommunityRelayDepositClient>>,
    /// ZEB-418 SP2 P2 Task 3: outbound-hold side-table. `None` (default)
    /// disables the hold write entirely — send_dm behaves exactly as before.
    /// Production installs both via `set_outhold` at start_node alongside the
    /// dm-outhold FleetSyncEngine; tests inject a bare doc + flag closure.
    outhold_doc: Option<std::sync::Arc<tokio::sync::Mutex<crate::dm_outhold::DmOutholdDoc>>>,
    /// The engine's `notify_dirty` — called after a successful hold write so
    /// the debounced publisher picks it up.
    outhold_notify: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

impl DmOutbox {
    pub fn new(
        device_id: String,
        self_owner: OwnerAddr,
        our_signing_device_hash: DeviceIdentityHash,
        signing_key: Arc<ed25519_dalek::SigningKey>,
        private_identity: Arc<harmony_identity::PrivateIdentity>,
        community_signing_key: Arc<ed25519_dalek::SigningKey>,
        enrollment_cert: harmony_owner::certs::EnrollmentCert,
    ) -> Self {
        // ZEB-339: defense-in-depth — validate enrolled materials in ALL
        // builds (not just debug). These three invariants must hold for any
        // correctly-plumbed DmOutbox; a violation is a wiring bug (cert from
        // owner A paired with self_owner from owner B, or a mismatched
        // community_signing_key) that would otherwise surface much later as
        // an unverifiable community event. Enforced unconditionally because
        // the cert↔key binding is security-relevant — fail fast at
        // construction. `new` is called ~once per node start, so the
        // cert.verify() cost is negligible. Tests that intentionally use
        // mismatched synthetic material call `new_synthetic`, which bypasses
        // these checks by design.
        assert!(
            enrollment_cert.verify(0).is_ok(),
            "DmOutbox: enrollment_cert must verify (structural; expiry-agnostic by design — ZEB-378)"
        );
        assert_eq!(
            enrollment_cert.owner_id, self_owner.0,
            "DmOutbox: cert.owner_id must match self_owner"
        );
        assert_eq!(
            enrollment_cert.device_pubkeys.classical.ed25519_verify,
            community_signing_key.verifying_key().to_bytes(),
            "DmOutbox: cert device key must match community_signing_key"
        );
        // ZEB-580 S1: compute the #2 DM hash BEFORE `enrollment_cert` is moved
        // into the struct literal below. `None` degrades DM signing to #3.
        let our_device2_signing_hash = crate::dm_signing::device2_signing_hash(&enrollment_cert);
        Self {
            device_id,
            self_owner,
            our_signing_device_hash,
            signing_key,
            private_identity,
            community_signing_key,
            enrollment_cert,
            our_device2_signing_hash,
            in_flight: HashSet::new(),
            backoff: HashMap::new(),
            shutdown_gate: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            phase_c_inflight: Arc::new(tokio::sync::Semaphore::new(DRAIN_PHASE_C_FENCE_CAPACITY)),
            butler_deposit_client: None,
            community_relay_deposit_client: None,
            outhold_doc: None,
            outhold_notify: None,
        }
    }

    /// ZEB-703 (PR #485 Greptile P1): the drain-path shutdown fence
    /// handles, cloned out for the `/v1/shutdown` pre-ack barrier and
    /// `stop_inner`. Callers set the gate (stops new drain ticks), then
    /// `acquire_many(DRAIN_PHASE_C_FENCE_CAPACITY)` on the semaphore to
    /// await every in-flight detached Phase C task before snapshotting
    /// owner-state.
    pub(crate) fn shutdown_fence_handles(
        &self,
    ) -> (
        Arc<std::sync::atomic::AtomicBool>,
        Arc<tokio::sync::Semaphore>,
    ) {
        (
            Arc::clone(&self.shutdown_gate),
            Arc::clone(&self.phase_c_inflight),
        )
    }

    /// ZEB-703/710: snapshot the drain-path fence handles at stop. `try_lock`
    /// because `stop_inner` is fully synchronous (`blocking_lock` would panic
    /// from async contexts); contention means a drain tick holds the lock for
    /// its brief Phase A window — degrade to no-fence with a WARN rather than
    /// spinning. ZEB-710: the degraded mode also increments
    /// [`DM_FENCE_STATS`] so wedge visibility is not log-only.
    pub(crate) fn snapshot_shutdown_fence_at_stop(
        outbox: &Arc<tokio::sync::Mutex<DmOutbox>>,
    ) -> Option<(
        Arc<std::sync::atomic::AtomicBool>,
        Arc<tokio::sync::Semaphore>,
    )> {
        match outbox.try_lock() {
            Ok(g) => Some(g.shutdown_fence_handles()),
            Err(_) => {
                // ZEB-710: count the degrade — wedge visibility must not be
                // log-only.
                DM_FENCE_STATS.record_stop_fence_skipped_contended();
                tracing::warn!(
                    "ZEB-703: dm_outbox contended at stop; skipping drain-path fence \
                     (a Phase C mutation may race the final persist)"
                );
                None
            }
        }
    }

    /// Test-only constructor that bypasses the ZEB-339 `assert!`
    /// invariant checks in `DmOutbox::new`. Use this in integration tests
    /// that are still using synthetic/mismatched enrolled-device material
    /// (e.g. a cert minted with a different seed than the community owner
    /// addr). Tests that build fully consistent material MUST use `new`.
    ///
    /// The `test-fixtures` gate ensures this constructor is never available
    /// in production builds.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn new_synthetic(
        device_id: String,
        self_owner: OwnerAddr,
        our_signing_device_hash: DeviceIdentityHash,
        signing_key: Arc<ed25519_dalek::SigningKey>,
        private_identity: Arc<harmony_identity::PrivateIdentity>,
        community_signing_key: Arc<ed25519_dalek::SigningKey>,
        enrollment_cert: harmony_owner::certs::EnrollmentCert,
    ) -> Self {
        // ZEB-580 S1: same #2 DM hash computation as `new` (kept in sync).
        let our_device2_signing_hash = crate::dm_signing::device2_signing_hash(&enrollment_cert);
        Self {
            device_id,
            self_owner,
            our_signing_device_hash,
            signing_key,
            private_identity,
            community_signing_key,
            enrollment_cert,
            our_device2_signing_hash,
            in_flight: HashSet::new(),
            backoff: HashMap::new(),
            shutdown_gate: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            phase_c_inflight: Arc::new(tokio::sync::Semaphore::new(DRAIN_PHASE_C_FENCE_CAPACITY)),
            butler_deposit_client: None,
            community_relay_deposit_client: None,
            outhold_doc: None,
            outhold_notify: None,
        }
    }

    /// ZEB-418 SP2 P1 Task 8: install the sender-side butler deposit
    /// client. Until this is called the deposit rung is disabled and drain
    /// behaves exactly as before (spec §6: the butler is a new rung, never
    /// a replacement).
    pub fn set_butler_deposit_client(
        &mut self,
        client: Arc<dyn crate::butler_deposit::ButlerDepositClient>,
    ) {
        self.butler_deposit_client = Some(client);
    }

    /// ZEB-458 P4 Phase B: install the last-resort community-relay deposit
    /// client. Until this is called the relay rung is disabled. The relay
    /// rung fires ONLY when the butler rung did not ack for a given
    /// candidate — it is strictly additive, never a replacement for the
    /// butler or direct paths.
    pub fn set_community_relay_deposit_client(
        &mut self,
        client: Arc<dyn crate::community_relay::CommunityRelayDepositClient>,
    ) {
        self.community_relay_deposit_client = Some(client);
    }

    /// ZEB-418 SP2 P2 Task 3: install the outbound-hold doc + dirty-notify.
    /// Until this is called the hold write is disabled and send_dm behaves
    /// exactly as before (spec D12: the hold is additive, never load-bearing
    /// for the legacy path).
    pub fn set_outhold(
        &mut self,
        doc: std::sync::Arc<tokio::sync::Mutex<crate::dm_outhold::DmOutholdDoc>>,
        notify: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) {
        self.outhold_doc = Some(doc);
        self.outhold_notify = Some(notify);
    }

    /// Encrypt `content` under `Space.content_key`, write the storage blob to
    /// CAS, mint a fresh OutboxEntry, install it. Returns the new
    /// `(MessageId, ContentId)` — MessageId is the OutboxEntryId for
    /// lifecycle correlation; ContentId is the message_cid the IPC layer
    /// surfaces to the frontend so it can stably key by content identifier
    /// across optimistic / dm-received / read_dm_thread paths (a
    /// MessageId-only return would force the caller to re-look-up the
    /// entry to recover the cid). Drain (next tick) attempts delivery;
    /// this call returns immediately.
    ///
    /// `wall_now_ms` and `prev_hlc` are passed in (not derived) so tests can
    /// drive deterministic HLCs and so the IPC handler can keep the per-device
    /// HLC monotone via the existing SyncEngine HLC tracker.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_dm(
        &mut self,
        state: &mut OwnerState,
        cas: &dyn ContentStore,
        space_id: SpaceId,
        content: Vec<u8>,
        mime_type: String,
        wall_now_ms: u64,
        prev_hlc: Option<&Hlc>,
    ) -> Result<(MessageId, crate::owner_state_types::ContentId), SendDmError> {
        // 1. Look up Space, check kind + content_key.
        let space = state
            .spaces
            .get(&space_id)
            .ok_or(SendDmError::UnknownSpace(space_id))?;
        match space.kind {
            SpaceKind::Dm | SpaceKind::GroupDm => {}
            SpaceKind::Folder => return Err(SendDmError::InvalidSpaceKind(space_id, "Folder")),
            SpaceKind::Community => {
                return Err(SendDmError::InvalidSpaceKind(space_id, "Community"))
            }
            SpaceKind::Channel => return Err(SendDmError::InvalidSpaceKind(space_id, "Channel")),
            SpaceKind::PublicChannel => {
                return Err(SendDmError::InvalidSpaceKind(space_id, "PublicChannel"))
            }
        }

        let content_key = space
            .content_key
            .as_ref()
            .ok_or(SendDmError::MissingContentKey(space_id))?;

        // 2. Derive recipient_owners — exclude self, dedup, sort.
        let recipients = derive_recipients(&space.members, &self.self_owner);
        // Reject self-only DMs up front. Without this we'd mint an
        // OutboxEntry with `recipient_owners: vec![]`, which drain() never
        // sends to anyone AND which the expiration sweep would mark
        // Complete (vacuous all-acked truth) instead of Expired — so the
        // entry sits forever doing nothing.
        if recipients.is_empty() {
            return Err(SendDmError::NoRecipients(space_id));
        }

        // 3. Build MessagePayload + HLC stamp.
        let sent_at = next_hlc(prev_hlc, wall_now_ms, &self.device_id);
        let payload = MessagePayload {
            body: content,
            mime_type,
            sender: self.self_owner,
            sent_at: sent_at.clone(),
        };

        // 4. Encrypt under (content_key, AAD = canonical_cbor(dedupe_key)).
        let aad =
            compute_aad(space).map_err(|e| SendDmError::Encode(format!("compute_aad: {e}")))?;
        let storage_blob = encrypt_dm_message(content_key, &aad, &payload)?;

        // 5. Compute message_cid + write to CAS. Mirror publish_root_now's
        //    EncryptedDurable flag pair: encrypted=true, ephemeral=false
        //    (default). DM bodies should never auto-burn from the
        //    StorageTier — they're chat history.
        let message_cid = harmony_content::cid::ContentId::for_book(
            &storage_blob,
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .map_err(|e| SendDmError::Encode(format!("ContentId::for_book: {e}")))?;
        // ZEB-418 P2: clone the blob once before the CAS put so sibling
        // devices can complete delivery via the outbound hold. Cloned only
        // when outhold_doc is installed; the CAS put consumes the original.
        let held_blob = if self.outhold_doc.is_some() {
            Some(storage_blob.clone())
        } else {
            None
        };
        cas.put(message_cid, storage_blob).await?;

        // 6. Mint OutboxEntry, install via apply_outbox.
        let entry_id = OutboxEntryId(ulid::Ulid::new().to_bytes());
        let entry = OutboxEntry {
            id: entry_id,
            space_id,
            recipient_owners: recipients,
            // ZEB-505: a real message entry always carries a `message_cid`.
            message_cid: Some(message_cid),
            created_at: sent_at.clone(),
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        };
        match state.apply_outbox(entry) {
            ApplyOutcome::Inserted | ApplyOutcome::Merged { .. } => {
                // Phase 4: Self-InboxEntry write for self-history persistence.
                //
                // InboxEntry semantics widen here from "received from someone
                // else" to "exists in this Space's history (sender OR
                // recipient)". A paired device receiving the same DmCidNotify
                // writes its own InboxEntry on receipt; this self-write on
                // the sending device matches what the paired device will
                // write, so the InboxEntry table converges naturally without
                // special-casing.
                let self_inbox_entry = crate::owner_state_types::InboxEntry {
                    space_id,
                    message_cid,
                    from: self.self_owner,
                    received_at: sent_at.clone(),
                };
                let _ = state.apply_inbox(self_inbox_entry);
                // Outcome ignored: Inserted is the happy path; Merged{old_id:
                // None} fires if a paired device's CidNotify already wrote
                // this CID first (cross-device race), which is fine — same
                // payload, idempotent.

                // ZEB-418 P2: record the blob in the outbound hold so siblings
                // can complete delivery (spec D12). After state mutations, never
                // held across the CAS await; lock scope is the insert only.
                if let Some(outhold) = self.outhold_doc.clone() {
                    let key =
                        crate::dm_outhold::DmOutholdDoc::key(&space_id.0, &message_cid.to_bytes());
                    let mut doc = outhold.lock().await;
                    doc.entries
                        .entry(key)
                        .or_insert(crate::dm_outhold::DmOutholdEntry {
                            storage_blob: held_blob.expect("cloned when outhold_doc is Some"),
                            space_id: space_id.0,
                            created_at: sent_at.clone(),
                        });
                    drop(doc);
                    if let Some(notify) = self.outhold_notify.as_ref() {
                        notify();
                    }
                }

                Ok((entry_id, message_cid))
                // Note: ApplyOutcome::Merged would also reach here. It "should
                // not happen" because a fresh ULID can't collide with any
                // existing entry, but we treat it the same as Inserted for
                // safety.
            }
            ApplyOutcome::Rejected(r) => Err(SendDmError::CrdtRejected(r)),
        }
    }

    /// Mark `recipient` as delivered for `entry_id`. Idempotent.
    /// Returns true iff this call mutated `delivered_to` (i.e., recipient
    /// was not already present). Caller emits `dm-delivered` IPC event
    /// only on `true`.
    ///
    /// Drops with telemetry on:
    ///   - unknown entry_id (likely stale ack from before app restart)
    ///   - recipient not in entry.recipient_owners (forged ack)
    ///
    /// Both mismatches log at warn level; neither mutates state.
    ///
    /// Phase 3b note: this is the post-verification delivery-marking
    /// primitive that Task 11's `handle_ack` (the inbound DM packet
    /// dispatcher) calls AFTER signature verification + signed-origin
    /// resolution. Phase 2 callers (drain integration tests) drive it
    /// directly because Phase 2 had no signature layer to verify against.
    pub fn mark_ack_delivered(
        &mut self,
        state: &mut OwnerState,
        entry_id: OutboxEntryId,
        recipient: OwnerAddr,
    ) -> bool {
        let Some(entry) = state.outbox.get_mut(&entry_id) else {
            tracing::warn!(?entry_id, ?recipient, "DmAck dropped: unknown entry");
            return false;
        };
        if !entry.recipient_owners.contains(&recipient) {
            tracing::warn!(
                ?entry_id,
                ?recipient,
                "DmAck dropped: recipient not in entry.recipient_owners (forged ack)"
            );
            return false;
        }
        let inserted = entry.delivered_to.insert(recipient);
        if inserted {
            // Re-derive status. is_expired=false because this is the
            // happy-path mutation; expiration is owned by drain's wall-clock
            // sweep. If drain has already marked Expired, compute_status
            // will preserve Expired only when (a) is_expired is passed true
            // — so we must check the current state to keep Expired sticky.
            let was_expired = matches!(entry.delivery_status, DeliveryStatus::Expired);
            entry.delivery_status = entry.compute_status(was_expired);
            // Clear in-flight + backoff for this (entry, recipient) so a
            // subsequent drain doesn't re-attempt a now-completed delivery.
            self.in_flight.remove(&(entry_id, recipient));
            self.backoff.remove(&(entry_id, recipient));
        }
        inserted
    }

    /// Phase 4 — Manual delete of a stuck or expired self-OutboxEntry.
    ///
    /// Removes BOTH the OutboxEntry and the corresponding self-InboxEntry
    /// keyed by `(space_id, message_cid)`. User intent on manual delete
    /// is "make this message go away," so removing both is the expected
    /// UX. The IPC layer reads the returned `DeleteDmOutboxOutcome` to
    /// decide which `dm-deleted` event to emit.
    ///
    /// Also clears any in-flight + backoff cache entries for the deleted
    /// message so a stale entry can't resurface from a future drain tick.
    ///
    /// Idempotent: returns `Default::default()` (all None) if the
    /// OutboxEntry doesn't exist (e.g., already deleted, or the caller
    /// raced a Complete → GC).
    pub fn delete_dm_outbox_entry(
        &mut self,
        state: &mut OwnerState,
        message_id: OutboxEntryId,
        wall_now_ms: u64,
    ) -> Result<DeleteDmOutboxOutcome, DeleteDmError> {
        // Peek before removing so a Complete entry doesn't get unconditionally
        // wiped. Idempotent miss → Ok(default()); Complete hit → Err so the
        // caller can distinguish "refusing to erase delivered history" from
        // "the entry was already deleted/never existed."
        //
        // ZEB-246: an in-flight (Pending/Partial) entry younger than
        // STUCK_THRESHOLD_MS is also refused — otherwise a direct IPC call
        // (bypassing the UI's 60s canDelete gate) could delete a message
        // mid-delivery, turning manual-cleanup into an unsend primitive.
        // `saturating_sub` keeps a future-stamped created_at (clock skew)
        // conservatively "fresh" (age 0 → rejected) rather than underflowing.
        // Expired entries are stuck by definition and stay deletable.
        match state.outbox.get(&message_id) {
            None => return Ok(DeleteDmOutboxOutcome::default()),
            Some(e) if matches!(e.delivery_status, DeliveryStatus::Complete) => {
                return Err(DeleteDmError::AlreadyDelivered(message_id));
            }
            Some(e)
                if matches!(
                    e.delivery_status,
                    DeliveryStatus::Pending | DeliveryStatus::Partial
                ) =>
            {
                let age_ms = wall_now_ms.saturating_sub(e.created_at.wall_ms);
                if age_ms < STUCK_THRESHOLD_MS {
                    return Err(DeleteDmError::NotYetStuck {
                        age_ms,
                        threshold_ms: STUCK_THRESHOLD_MS,
                    });
                }
            }
            Some(_) => {}
        }
        // SAFETY: the get() above proved the entry exists with non-Complete
        // status. No await between, so no concurrent mutator can race.
        let outbox_entry = state
            .outbox
            .remove(&message_id)
            .expect("entry existed in get() above; no await between");
        // ZEB-505: an invite-only entry (`None` message_cid) has no InboxEntry.
        let inbox_key =
            outbox_entry
                .message_cid
                .map(|message_cid| crate::owner_state_types::InboxKey {
                    space_id: outbox_entry.space_id,
                    message_cid,
                });
        // Self-InboxEntry may legitimately be absent (e.g., a paired
        // device's CidNotify could have raced ahead and the InboxEntry
        // could have been GC'd). Either way, idempotent removal.
        if let Some(key) = inbox_key {
            let _removed_inbox = state.delete_inbox_entry(key);
        }

        // ZEB-243: write a tombstone so paired-device sync cannot
        // resurrect this entry via apply_outbox. HLC is minted with the
        // same device_id and the caller-supplied wall_now_ms. There is no
        // prior tombstone for this id (the entry existed, so no tombstone
        // was present — outbox_tombstones and outbox are mutually
        // exclusive for any given id under the intended invariant).
        //
        // Pass Some(&outbox_entry.created_at) as the previous HLC so the
        // monotone advance is threaded through next_hlc even when
        // wall_now_ms equals created_at.wall_ms (same-millisecond delete)
        // or rolls backward (clock skew). next_hlc bumps the logical
        // component in both cases, guaranteeing tombstone_hlc is
        // strictly newer than created_at. Passing None would discard
        // prior HLC info and could produce tombstone_hlc == created_at on
        // a same-millisecond delete, causing apply_outbox's strict-newer-
        // than gate to fail to block a peer resurrection.
        let tombstone_hlc = next_hlc(Some(&outbox_entry.created_at), wall_now_ms, &self.device_id);
        state.outbox_tombstones.insert(message_id, tombstone_hlc);

        // Clear in-flight + backoff caches across all recipients of this
        // message so a stale entry can't resurface on a future drain.
        self.in_flight.retain(|(eid, _)| *eid != message_id);
        self.backoff.retain(|(eid, _), _| *eid != message_id);

        Ok(DeleteDmOutboxOutcome {
            deleted_outbox_id: Some(message_id),
            deleted_inbox_key: inbox_key,
            space_id: Some(outbox_entry.space_id),
            message_cid: outbox_entry.message_cid,
        })
    }

    /// Single drain pass. Walks every Pending/Partial entry; per outstanding
    /// recipient (in `recipient_owners` ∖ `delivered_to`):
    ///   - skip if in `in_flight` set already
    ///   - skip if backoff says next attempt is in the future
    ///   - else mark in-flight, call transport.send().
    ///     - Ok(()): clear in-flight, bump the pair's AttemptState
    ///       (failure_count ACCUMULATES per unacked window — ZEB-422) so
    ///       the next attempt waits the exponential backoff (5s base) for
    ///       an ack before re-sending; from `DEPOSIT_NOACK_WINDOWS` unacked
    ///       windows onward the butler-deposit rung is also attempted.
    ///       handle_ack clears the entry on real ack; drain's epilogue
    ///       clears it on Complete-via-CRDT-merge.
    ///     - Err(_): clear in-flight, bump backoff failure_count + record
    ///       last_attempt_wall_ms (exponential escalation up to 5min cap).
    ///
    /// Then sweep for expiration: any Pending/Partial entry where
    /// `wall_now_ms - created_at.wall_ms >= EXPIRATION_MS` and not all
    /// recipients in delivered_to → mark Expired, record in newly_expired.
    ///
    /// Epilogue: drop backoff/in_flight entries for any OutboxEntry that's
    /// no longer Pending/Partial — covers Complete via local handle_ack,
    /// Complete via CRDT-merge replication of a peer's ack, and Expired.
    ///
    /// This is the legacy lock-held entrypoint: callers acquire `&mut self`
    /// (i.e. hold the `tokio::sync::Mutex<DmOutbox>` guard) for the full
    /// duration including the transport sends. Tests use this directly.
    /// Production code calls `drain_lifted` instead (ZEB-233) which
    /// releases the outbox + state locks around the transport sends so
    /// concurrent `send_dm` IPCs don't block on the slowest in-flight send.
    pub async fn drain(
        &mut self,
        state: &mut OwnerState,
        transport: &dyn DmTransport,
        wall_now_ms: u64,
    ) -> DrainOutcome {
        let work = self.drain_phase_a(state, wall_now_ms);
        let mut results = Vec::with_capacity(work.len());
        for unit in work {
            let result = transport
                .send(&unit.entry_clone, unit.recipient, unit.destinations)
                .await;
            results.push(DrainSendResult {
                entry_id: unit.entry_id,
                recipient: unit.recipient,
                result,
            });
        }
        // Test wrapper holds locks throughout, so no TOCTOU-skip
        // semantics — pass an empty `skipped` Vec to drain_phase_c.
        // Same `wall_now_ms` for both backoff and expiration clocks
        // since tests don't simulate Phase B/C latency.
        let (mut outcome, deposit_candidates) =
            self.drain_phase_c(state, results, Vec::new(), wall_now_ms, wall_now_ms);

        // ZEB-418 P1 Task 8: butler deposit rung (lock-held variant —
        // production runs it unlocked in `drain_lifted`). An ack routes
        // through the existing idempotent `mark_ack_delivered`; every
        // other outcome leaves the entry exactly as the transient direct
        // failure left it (spec §6).
        //
        // ZEB-458 P4 Phase B: clone both clients into locals before the
        // loop so the borrow of `self` for `mark_ack_delivered` (which
        // takes `&mut self`) inside the loop doesn't conflict with a live
        // borrow of `self.community_relay_deposit_client`.
        if !deposit_candidates.is_empty() {
            let butler_client = self.butler_deposit_client.clone();
            let relay_client = self.community_relay_deposit_client.clone();
            if butler_client.is_some() || relay_client.is_some() {
                for c in deposit_candidates {
                    let butler_acked = if let Some(ref client) = butler_client {
                        let butler_outcome = client.deposit(&c).await;
                        let acked = matches!(
                            butler_outcome,
                            crate::butler_deposit::DepositRungOutcome::Acked
                        );
                        match butler_outcome {
                            crate::butler_deposit::DepositRungOutcome::Acked => {
                                if self.mark_ack_delivered(state, c.entry_id, c.recipient_owner) {
                                    // ZEB-505: invite-only entries (no message_cid) ack
                                    // but emit no `dm-delivered` (no message to surface).
                                    if let Some(message_cid) = c.message_cid {
                                        outcome.newly_delivered.push((
                                            c.space_id,
                                            message_cid,
                                            c.recipient_owner,
                                        ));
                                    }
                                }
                            }
                            crate::butler_deposit::DepositRungOutcome::SkippedNoFreshButlerSet => {}
                            crate::butler_deposit::DepositRungOutcome::Failed(e) => {
                                tracing::debug!(
                                    entry_id = ?c.entry_id,
                                    recipient = ?c.recipient_owner,
                                    error = %e,
                                    "ZEB-418: butler deposit rung failed; existing retry chain continues"
                                );
                            }
                        }
                        acked
                    } else {
                        false
                    };
                    // ZEB-458 P4: last-resort community relay rung — only if
                    // the butler did not ack.
                    if !butler_acked {
                        if let Some(ref relay) = relay_client {
                            if relay.deposit(&c).await
                                && self.mark_ack_delivered(state, c.entry_id, c.recipient_owner)
                            {
                                // ZEB-505: invite-only entries emit no `dm-delivered`.
                                if let Some(message_cid) = c.message_cid {
                                    outcome.newly_delivered.push((
                                        c.space_id,
                                        message_cid,
                                        c.recipient_owner,
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
        outcome
    }

    /// Phase A of drain: under the outbox + state locks, collect every
    /// (entry, recipient) pair that needs a transport.send this tick.
    /// Marks each in `self.in_flight` so a concurrent drain tick can't
    /// double-send the same pair. Synchronous (no await).
    ///
    /// Entries already past `EXPIRATION_MS` are skipped here — they get
    /// marked `Expired` in Phase C's sweep without a final wasted
    /// transport.send attempt.
    ///
    /// Returns the work units ready for Phase B (unlocked sends). Each
    /// unit carries a clone of the OutboxEntry so Phase B doesn't need
    /// to re-read `state.outbox`.
    fn drain_phase_a(&mut self, state: &OwnerState, wall_now_ms: u64) -> Vec<DrainWorkUnit> {
        // 1. Collect outstanding (entry, recipient) pairs from Pending/
        //    Partial entries within EXPIRATION_MS.
        let outstanding: Vec<(OutboxEntryId, OutboxEntry, Vec<OwnerAddr>)> = state
            .outbox
            .iter()
            .filter(|(_, e)| {
                matches!(
                    e.delivery_status,
                    DeliveryStatus::Pending | DeliveryStatus::Partial
                )
            })
            // ZEB-791 swept this site and deliberately LEFT IT SATURATING.
            // `created_at` is our own minted HLC (`next_hlc(prev, wall_now,
            // device)`), which is monotonic per-device, while `wall_now_ms` is a
            // raw clock read — so after a backward step the HLC high-water mark
            // can sit ahead of the clock. Here `saturating_sub` then yields age
            // 0, which KEEPS the entry in the retry set. That is the safe
            // direction: bounding it forward would make us abandon delivery of
            // our own freshly-created DM. Do not "harden" this to match the
            // presence sites — their fail-open direction is immortality, this
            // one's is persistence.
            .filter(|(_, e)| wall_now_ms.saturating_sub(e.created_at.wall_ms) < EXPIRATION_MS)
            .map(|(id, e)| {
                let outstanding: Vec<OwnerAddr> = e
                    .recipient_owners
                    .iter()
                    .copied()
                    .filter(|r| !e.delivered_to.contains(r))
                    .collect();
                (*id, e.clone(), outstanding)
            })
            .collect();

        // 2. Per-(entry, recipient): apply in_flight + is_due filters,
        //    resolve destinations, mark in_flight, build a work unit.
        let mut work = Vec::new();
        for (entry_id, entry_clone, outstanding) in outstanding {
            for recipient in outstanding {
                if self.in_flight.contains(&(entry_id, recipient)) {
                    continue;
                }
                if !self.is_due(entry_id, recipient, wall_now_ms) {
                    continue;
                }
                // Resolve destinations from `&OwnerState` (no mutex
                // acquisition — caller holds the state guard). Empty
                // destinations: production transport returns Transient;
                // test stubs ignore the field.
                let destinations = resolve_destinations(&state.owner_device_cache, recipient);
                self.in_flight.insert((entry_id, recipient));
                work.push(DrainWorkUnit {
                    entry_id,
                    entry_clone: entry_clone.clone(),
                    recipient,
                    destinations,
                });
            }
        }
        work
    }

    /// Phase C of drain: under the outbox + state locks, apply each
    /// transport.send result (update backoff, clear in_flight), clear
    /// in_flight for pairs Phase B skipped (liveness check failed or
    /// lock contended), run the 30-day expiration sweep, and clean up
    /// backoff/in_flight entries for OutboxEntries that are no longer
    /// Pending/Partial. Synchronous.
    ///
    /// `skipped` carries the (entry_id, recipient) pairs that Phase B
    /// chose NOT to send via the liveness re-check (ZEB-233 round 2).
    /// Their in_flight markers MUST be cleared here so a future drain
    /// tick can re-attempt; backoff is NOT updated (skipping isn't an
    /// attempt).
    ///
    /// Two distinct timestamps (ZEB-233 round 3, CodeRabbit Major):
    ///
    /// * `backoff_now_ms`: fresh wall-clock captured AFTER Phase C
    ///   acquired its locks. Reflects when the send outcome was
    ///   actually recorded — accurate for `last_attempt_wall_ms`
    ///   bookkeeping so the next is_due check uses a real "we tried
    ///   at time T" anchor.
    ///
    /// * `expiration_now_ms`: original tick-time wall-clock (Phase A's
    ///   `wall_now_ms`). Reflects when Phase A admitted this drain
    ///   tick as in-flight. Used for the 30-day expiration sweep so
    ///   that an entry Phase A admitted as live can NEVER be expired
    ///   in the same tick due to Phase B/C latency. A slow
    ///   `transport.send()` (or contended Phase C lock acquisition)
    ///   that takes the wall-clock past `EXPIRATION_MS` must NOT mark
    ///   the just-sent entry Expired before its ack arrives.
    ///
    /// ZEB-418 P1 Task 8 + P2 ZEB-422: additionally returns the
    /// butler-deposit candidates this tick produced — one per TRANSIENT
    /// direct-send failure whose `(entry, recipient)` pair already carried
    /// an `AttemptState` (i.e. the entry has been pending ≥ one backoff
    /// cycle), and one per Ok-send whose pair has sat sent-but-never-acked
    /// for ≥ `DEPOSIT_NOACK_WINDOWS` full backoff windows (ZEB-422: the
    /// cached-but-offline recipient — the butler's PRIMARY scenario).
    /// Candidates only arise from send events, and each backoff window
    /// contains exactly one direct attempt, so the rung fires at most once
    /// per backoff window by construction (no hot loop). The caller runs
    /// the deposits AFTER this synchronous phase (unlocked in
    /// `drain_lifted`) and routes acks through `mark_ack_delivered`.
    fn drain_phase_c(
        &mut self,
        state: &mut OwnerState,
        results: Vec<DrainSendResult>,
        skipped: Vec<(OutboxEntryId, OwnerAddr)>,
        backoff_now_ms: u64,
        expiration_now_ms: u64,
    ) -> (
        DrainOutcome,
        Vec<crate::butler_deposit::ButlerDepositRequest>,
    ) {
        let mut outcome = DrainOutcome::default();
        let mut deposit_candidates: Vec<crate::butler_deposit::ButlerDepositRequest> = Vec::new();

        // 1. Apply each send result.
        for r in results {
            self.in_flight.remove(&(r.entry_id, r.recipient));
            // ZEB-233 round 4 (CodeRabbit Minor): skip the backoff
            // write if a concurrent `handle_ack` already marked this
            // recipient delivered between Phase B's send and Phase C's
            // (delayed, spawned) lock acquisition. Without this check,
            // we'd resurrect stale per-recipient retry state for an
            // already-acked recipient, which then sticks until the
            // whole message completes or expires (Step 4's
            // backoff.retain only sees the entry-level Pending/Partial
            // status, not per-recipient delivered_to).
            let recipient_still_pending = state.outbox.get(&r.entry_id).is_some_and(|entry| {
                matches!(
                    entry.delivery_status,
                    DeliveryStatus::Pending | DeliveryStatus::Partial
                ) && !entry.delivered_to.contains(&r.recipient)
            });
            if !recipient_still_pending {
                continue;
            }
            match r.result {
                Ok(()) => {
                    // Throttle post-Ok retries until the ack arrives.
                    // Without this, `is_due` returns true on the very next
                    // 250ms tick (no backoff entry → first attempt),
                    // producing tick-rate retry until handle_ack fires —
                    // ~4 sends/sec/recipient against the production
                    // StubTransport (which always returns Ok and has an
                    // unbounded sends Vec). Treat "sent but ack pending"
                    // as an accumulating failure_count (ZEB-422 — was
                    // pinned to 1 every window) so the existing
                    // exponential backoff applies (5s base × 2^(n-1),
                    // 5min cap). First post-Ok retry still waits 5s; if
                    // still no ack the next waits 10s, then 20s, etc.
                    // The 30-day expiration sweep is the eventual
                    // terminator.
                    let st =
                        self.backoff
                            .entry((r.entry_id, r.recipient))
                            .or_insert(AttemptState {
                                last_attempt_wall_ms: 0,
                                failure_count: 0,
                            });
                    let pre_failure_count = st.failure_count;
                    st.last_attempt_wall_ms = backoff_now_ms;
                    st.failure_count = st.failure_count.saturating_add(1);
                    // ZEB-422: sent-but-never-acked candidacy. The pair has
                    // completed pre_failure_count full backoff windows
                    // without an ack; from DEPOSIT_NOACK_WINDOWS onward each
                    // further window also tries the butler rung. Side
                    // effect, intentional: direct-send backoff now grows
                    // toward the 5-min cap for unresponsive recipients (was
                    // pinned at window 1), matching the Err-path behavior.
                    // Rung outcomes never touch the AttemptState written
                    // above (spec §6 / P2 §4 never-worse).
                    if pre_failure_count >= crate::butler_deposit::DEPOSIT_NOACK_WINDOWS
                        && (self.butler_deposit_client.is_some()
                            || self.community_relay_deposit_client.is_some())
                    {
                        self.push_deposit_candidate(
                            state,
                            r.entry_id,
                            r.recipient,
                            backoff_now_ms,
                            &mut deposit_candidates,
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        entry_id = ?r.entry_id,
                        recipient = ?r.recipient,
                        error = %e,
                        "transport.send failed; bumping backoff"
                    );
                    let st =
                        self.backoff
                            .entry((r.entry_id, r.recipient))
                            .or_insert(AttemptState {
                                last_attempt_wall_ms: 0,
                                failure_count: 0,
                            });
                    let pre_failure_count = st.failure_count;
                    st.last_attempt_wall_ms = backoff_now_ms;
                    st.failure_count = st.failure_count.saturating_add(1);

                    // ZEB-418 P1 Task 8: deposit-rung candidacy. Fires only
                    // for a TRANSIENT failure on a pair that already had an
                    // AttemptState (pending ≥ one backoff cycle: a prior
                    // failed attempt OR a sent-but-never-acked attempt).
                    // Permanent failures are excluded — a butler can't fix
                    // a broken local pipeline. Failure-event-driven means
                    // at most one deposit per backoff window (each window
                    // contains exactly one direct attempt). Rung outcomes
                    // never touch the AttemptState written above (spec §6:
                    // delivery is never worse than today).
                    //
                    // ZEB-525: the `pre_failure_count >= 1` grace window
                    // exists to let an in-flight live attempt win (recipient
                    // acks via the tunnel before the next tick → deposit
                    // never fires → butler storage/bandwidth saved). When
                    // the transport reports it launched NO live attempt
                    // (`TransientNoLiveAttempt`), the grace buys nothing —
                    // it is a pure one-window delay on first durability —
                    // so candidacy fires on the FIRST failure instead.
                    let no_live_attempt = matches!(e, TransportError::TransientNoLiveAttempt(_));
                    if (no_live_attempt
                        || (matches!(e, TransportError::Transient(_)) && pre_failure_count >= 1))
                        && (self.butler_deposit_client.is_some()
                            || self.community_relay_deposit_client.is_some())
                    {
                        self.push_deposit_candidate(
                            state,
                            r.entry_id,
                            r.recipient,
                            backoff_now_ms,
                            &mut deposit_candidates,
                        );
                    }
                }
            }
        }

        // 2. Clear in_flight markers for skipped pairs (ZEB-233 round 2).
        // Phase B chose not to send these — either the liveness check
        // saw the entry deleted/acked, or outbox/state was contended
        // and we conservatively skipped rather than send a possibly-
        // stale entry. DO NOT update backoff: skipping isn't a send
        // attempt, and bumping `failure_count` would unfairly throttle
        // a healthy entry on the next tick. The next drain tick's
        // Phase A will re-evaluate is_due + include if still due.
        for (entry_id, recipient) in skipped {
            self.in_flight.remove(&(entry_id, recipient));
        }

        // 3. Expiration sweep.
        let mut expired: Vec<(SpaceId, ContentId)> = Vec::new();
        for (_id, entry) in state.outbox.iter_mut() {
            if !matches!(
                entry.delivery_status,
                DeliveryStatus::Pending | DeliveryStatus::Partial
            ) {
                continue;
            }
            // ZEB-791: saturating on purpose, same reasoning as the retry-set
            // filter above — a future `created_at` yields age 0, so the entry is
            // NOT marked Expired and delivery keeps being attempted. Failing
            // closed here would emit `dm-expired` for a message we had only just
            // created.
            let age = expiration_now_ms.saturating_sub(entry.created_at.wall_ms);
            if age >= EXPIRATION_MS {
                let recipient_set: BTreeSet<&OwnerAddr> = entry.recipient_owners.iter().collect();
                let all_acked = recipient_set
                    .iter()
                    .all(|r| entry.delivered_to.contains(*r));
                if !all_acked {
                    entry.delivery_status = DeliveryStatus::Expired;
                    // ZEB-505: invite-only entries emit no `dm-expired` (no message).
                    if let Some(message_cid) = entry.message_cid {
                        expired.push((entry.space_id, message_cid));
                    }
                }
            }
        }

        // 4. Cleanup backoff/in_flight for entries no longer Pending/Partial.
        // Covers expired (just marked above), Complete via local handle_ack
        // (already cleaned in handle_ack but defensive double-cleanup is
        // cheap), AND Complete via CRDT merge (a peer device's ack
        // replicated through owner-state sync — handle_ack never fires for
        // that path so the previous narrow expired-only sweep leaked
        // forever). Entries whose underlying OutboxEntry is gone
        // (shouldn't happen; defensive) are also cleaned.
        self.backoff.retain(|(entry_id, _), _| {
            state
                .outbox
                .get(entry_id)
                .map(|e| {
                    matches!(
                        e.delivery_status,
                        DeliveryStatus::Pending | DeliveryStatus::Partial
                    )
                })
                .unwrap_or(false)
        });
        self.in_flight.retain(|(entry_id, _)| {
            state
                .outbox
                .get(entry_id)
                .map(|e| {
                    matches!(
                        e.delivery_status,
                        DeliveryStatus::Pending | DeliveryStatus::Partial
                    )
                })
                .unwrap_or(false)
        });
        outcome.newly_expired = expired;
        (outcome, deposit_candidates)
    }

    /// ZEB-580 S1: the (key, device-hash) pair for DM body signing — the
    /// enrolled #2 identity (`community_signing_key` + the cert's #2 DM hash)
    /// when a usable enrolled identity exists (`our_device2_signing_hash` is
    /// `Some`), else the legacy #3 Reticulum transport key (`signing_key` +
    /// `our_signing_device_hash`). Every outbound DM *body* sign site
    /// (CidNotify + Invite) routes through this so the send side never diverges;
    /// the transport-digest / countersign paths keep the #3 key by design (N4).
    fn dm_signing_material(&self) -> (&Arc<ed25519_dalek::SigningKey>, DeviceIdentityHash) {
        match self.our_device2_signing_hash {
            Some(h) => (&self.community_signing_key, h),
            None => (&self.signing_key, self.our_signing_device_hash),
        }
    }

    /// ZEB-580 S1: the full material an outbound *bootstrap invite* signs with —
    /// the [`dm_signing_material`](Self::dm_signing_material) `(key, device-hash)`
    /// pair PLUS the self-consistent `inviter_identity_pub` (the cert's #2 combined
    /// pub on the enrolled path, so
    /// `derive_device_hash_from_identity_pub(inviter_identity_pub) == device-hash`,
    /// which Task 3's receiver Check B asserts) and the attached #2
    /// `EnrollmentCert` (boxed to keep the `DmPacket::Invite` variant small).
    ///
    /// The single source of truth for the invite #2/#3 selection:
    /// `build_invite_packet_bytes` (the deposit rung) AND `add_space`'s live
    /// invite send (lib.rs Task 6) both route through this, so a #2-only receiver
    /// can never see one copy of an invite signed #2 and its sibling signed #3.
    /// Degrades to the legacy #3 transport identity (inline #3 pub, no cert) when
    /// no enrolled identity is usable — preserving pre-migration wire bytes
    /// exactly (the #3 pub is `private_identity`'s combined pub, bit-identical to
    /// `start_node`'s captured `identity_pub_64`).
    pub(crate) fn dm_invite_material(
        &self,
    ) -> (
        &Arc<ed25519_dalek::SigningKey>,
        DeviceIdentityHash,
        [u8; 64],
        Option<Box<harmony_owner::certs::EnrollmentCert>>,
    ) {
        let (key, dh) = self.dm_signing_material();
        let (inviter_identity_pub, inviter_enrollment) = match self.our_device2_signing_hash {
            Some(_) => (
                crate::dm_signing::device2_combined_pub(&self.enrollment_cert),
                Some(Box::new(self.enrollment_cert.clone())),
            ),
            None => (
                self.private_identity.public_identity().to_public_bytes(),
                None,
            ),
        };
        (key, dh, inviter_identity_pub, inviter_enrollment)
    }

    /// ZEB-418 P1 Task 8 / ZEB-506: build the signed CidNotify wire bytes for a
    /// deposit. Carries the sender's FULL cached device set via
    /// [`resolve_sender_devices`] (NOT a bare singleton): the recipient's
    /// ingestion refreshes its `OwnerDeviceCache` for this sender from
    /// `sender_devices` through the LWW-REPLACE `apply_owner_device_update`, so a
    /// singleton here would shrink a multi-device sender's cached set down to the
    /// signing device and drop later messages signed by the others
    /// (`UnknownSigningKey`). This is the same fix the invite builders received
    /// in PR #302 — see [`resolve_sender_devices`].
    fn build_cidnotify_packet_bytes(
        &self,
        state: &OwnerState,
        entry: &OutboxEntry,
    ) -> Result<Vec<u8>, String> {
        // ZEB-505: a CidNotify exists only for a message entry. An invite-only
        // entry (`None` message_cid) is deposited as an invite alone — the
        // caller (`push_deposit_candidate`) branches before reaching here.
        let Some(message_cid) = entry.message_cid else {
            return Err("build_cidnotify_packet_bytes called on an invite-only entry".into());
        };
        // ZEB-580 S1: sign the DM body with #2 (enrolled) when available, else #3.
        let (key, dh) = self.dm_signing_material();
        let signed = crate::dm_envelope::DmCidNotifySigned {
            space_id: entry.space_id,
            message_cid,
            sender_owner_addr: self.self_owner,
            sender_devices: resolve_sender_devices(state, self.self_owner, dh),
            signing_device_hash: dh,
        };
        build_dm_packet(signed, key)
    }

    /// ZEB-483 / ZEB-504: rebuild + sign the DmInvite wire bytes for a DM-Space
    /// deposit by delegating to the shared [`build_invite_packet_from_space`] —
    /// the SAME reconstruction the live PQ-tunnel transport uses — so the deposit
    /// rung and the live tunnel rebuild the bootstrap invite identically (both
    /// carry the sender's full `OwnerDeviceCache` device list via
    /// `resolve_sender_devices`, not a singleton). A deposited copy therefore
    /// bootstraps exactly the Space `add_space_dm_inner` would.
    ///
    /// The deposit-recover path bootstraps ONLY the Space — it never refreshes
    /// the OwnerDeviceCache from a deposited invite (see `apply_invite`'s
    /// `refresh_owner_device_cache = false`); the recipient learns the sender's
    /// devices from the friend handshake / the verified CidNotify path.
    ///
    /// Return semantics are the free fn's: `Ok(Some)` for a healthy DM/GroupDm
    /// Space; `Ok(None)` for a non-DM or missing Space record (deposit the
    /// CidNotify alone — a vanished record is unreachable for a real DM entry
    /// and can't be classified as a DM); `Err` for a DM/GroupDm Space that
    /// EXISTS but has no `content_key`, or a sign/encode failure — load-bearing
    /// for offline recovery, so the caller SKIPS the deposit candidate and
    /// leaves the entry pending for retry rather than depositing a CidNotify the
    /// recipient would recover into `SpaceNotFound`.
    fn build_invite_packet_bytes(
        &self,
        state: &OwnerState,
        space_id: &SpaceId,
    ) -> Result<Option<Vec<u8>>, String> {
        // ZEB-580 S1: sign the invite body with #2 (enrolled) when available and
        // attach our own #2 EnrollmentCert + the self-consistent #2 combined pub
        // (so `derive_device_hash_from_identity_pub(inviter_identity_pub) ==
        // signing_device_hash`, which Task 3's receiver Check B asserts). Degrade
        // to the legacy #3 transport identity with NO attached cert otherwise —
        // preserving pre-migration wire bytes exactly. `dm_invite_material` is the
        // single source of truth shared with `add_space`'s live send (Task 6).
        let (key, dh, inviter_identity_pub, inviter_enrollment) = self.dm_invite_material();
        // ZEB-504: delegate to the shared free fn so the deposit rung and the
        // live-tunnel transport rebuild the bootstrap invite identically.
        build_invite_packet_from_space(
            state,
            space_id,
            key,
            self.self_owner,
            dh,
            inviter_identity_pub,
            inviter_enrollment,
        )
    }

    /// Build + push one butler-deposit candidate for `(entry_id,
    /// recipient)` — shared by `drain_phase_c`'s Err-arm transient-failure
    /// rung (P1 Task 8) and its Ok-arm sent-but-never-acked rung (P2
    /// ZEB-422). `now_ms` feeds the request's freshness clock; both call
    /// sites pass Phase C's `backoff_now_ms` (the clock the surrounding
    /// AttemptState bump anchors to). A CidNotify build failure skips the
    /// candidate with a warn — it never fails the drain.
    fn push_deposit_candidate(
        &self,
        state: &OwnerState,
        entry_id: OutboxEntryId,
        recipient: OwnerAddr,
        now_ms: u64,
        out: &mut Vec<crate::butler_deposit::ButlerDepositRequest>,
    ) {
        let Some(entry) = state.outbox.get(&entry_id) else {
            return;
        };
        // ZEB-483 (CodeRabbit): build the bootstrap invite FIRST and treat a DM
        // failure as fail-closed — skip the whole candidate so the entry stays
        // pending for retry. Depositing the CidNotify without the invite would
        // let a butler/relay ack mark the message delivered while an offline
        // recipient recovers it straight into `SpaceNotFound`.
        let invite_packet = match self.build_invite_packet_bytes(state, &entry.space_id) {
            Ok(invite_packet) => invite_packet,
            Err(err) => {
                tracing::warn!(
                    entry_id = ?entry_id,
                    recipient = ?recipient,
                    error = %err,
                    "ZEB-483: DM invite rebuild failed; skipping deposit candidate (stays pending for retry)"
                );
                return;
            }
        };
        // ZEB-505: branch on message vs invite-only entry.
        let cidnotify_packet = match entry.message_cid {
            // Message entry: deposit the CidNotify (alongside the invite). A
            // CidNotify build failure skips the candidate (leave pending).
            Some(_) => match self.build_cidnotify_packet_bytes(state, entry) {
                Ok(packet) => Some(packet),
                Err(err) => {
                    tracing::warn!(
                        entry_id = ?entry_id,
                        recipient = ?recipient,
                        error = %err,
                        "ZEB-418: CidNotify build failed; skipping deposit candidate"
                    );
                    return;
                }
            },
            // Invite-only entry: the invite IS the payload, so a missing
            // rebuildable invite is fail-closed — there is nothing to deposit.
            None => {
                if invite_packet.is_none() {
                    tracing::warn!(
                        entry_id = ?entry_id,
                        recipient = ?recipient,
                        "ZEB-505: invite-only entry has no rebuildable invite; skipping deposit candidate (stays pending for retry)"
                    );
                    return;
                }
                None
            }
        };
        out.push(crate::butler_deposit::ButlerDepositRequest {
            entry_id,
            recipient_owner: recipient,
            space_id: entry.space_id,
            message_cid: entry.message_cid,
            cidnotify_packet,
            invite_packet,
            // ZEB-691: this candidate builder only ever produces message /
            // invite-only deposits; revocation deposits are a separate
            // production path (Task B4). ZEB-674: grant deposits likewise ride a
            // separate direct path (`grant_read`), never the outbox retry loop.
            revocation_push: None,
            grant_push: None,
            grant_revoke: None,
            now_ms,
        });
    }

    fn is_due(&self, entry_id: OutboxEntryId, recipient: OwnerAddr, wall_now_ms: u64) -> bool {
        match self.backoff.get(&(entry_id, recipient)) {
            None => true, // first attempt
            Some(st) => {
                let exponent = st.failure_count.saturating_sub(1).min(BACKOFF_MAX_EXPONENT);
                let raw =
                    BACKOFF_BASE_MS.saturating_mul(BACKOFF_MULTIPLIER.saturating_pow(exponent));
                let delay = raw.min(BACKOFF_CAP_MS);
                wall_now_ms >= st.last_attempt_wall_ms.saturating_add(delay)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn backoff_len(&self) -> usize {
        self.backoff.len()
    }

    #[cfg(test)]
    pub(crate) fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }

    /// Inbound `DmInvite` handler — Phase 3b auto-accept.
    ///
    /// Per ZEB-216 spec §"Application-signature binding rule":
    ///   1. Three sanity gates (cheap, run before signature verification):
    ///      - inviter ∈ members
    ///      - signing_device_hash ∈ sender_devices (defense-in-depth;
    ///        decode_packet also enforces this — the gate here catches
    ///        future regressions if decode's invariant is ever loosened)
    ///      - self_owner ∈ members
    ///   2. Verify signature using inline `inviter_identity_pub` (the
    ///      64-byte combined identity pubs — DmInvite is the bootstrap
    ///      exception, the receiver does not yet have an OwnerDeviceCache
    ///      entry for the inviter so the signing pub ships inline).
    ///   3. Auto-accept (Phase 3b ships no UI; user-driven decline UX is
    ///      deferred to Phase 4 with a follow-up Linear ticket filed at
    ///      PR-creation time per the Phase 3b spec):
    ///      - `apply_owner_device_update` with a parallel pubs vec that
    ///        has `Some(inviter_identity_pub)` at the signer's index and
    ///        `None` everywhere else. The receiver knows the inviter's
    ///        identity pub for the device that signed THIS invite, but
    ///        has no pubs for the inviter's other devices yet — they
    ///        remain pre-bootstrap until the next invite-equivalent flow.
    ///        The LWW HLC for this update is built from OUR local
    ///        `wall_now_ms` + `self.device_id`, NOT `signed.created_at`
    ///        (the inviter's claim) — using the remote HLC would let
    ///        an attacker forge a far-future timestamp on a single
    ///        malicious invite and pin the cache, rejecting all future
    ///        legitimate updates from the same owner as `StaleHlc`.
    ///      - `apply_space_with_canonicalization` for the new DM Space,
    ///        mirroring what `dm_outbox::send_dm` builds on the outbound
    ///        side (Reticulum transport binding, Phase 1 invariants for
    ///        `content_key` etc. — `validate_invariants` runs inside
    ///        `apply_space_with_canonicalization`, so the Space MUST
    ///        satisfy the DM-kind invariants).
    ///   4. Return `DrainOutcome::default()` — no IPC events from the
    ///      bare invite (`dm-received` events are tied to incoming
    ///      messages, not invites).
    pub async fn handle_invite(
        &mut self,
        state: &mut OwnerState,
        signed: crate::dm_envelope::DmInviteSigned,
        signature: [u8; 64],
        signed_bytes: &[u8],
        wall_now_ms: u64,
    ) -> Result<DrainOutcome, DmReceiveError> {
        // ZEB-482: the auto-accept body now lives in the shared free function
        // `apply_invite` so the tunnel ingest path (`ingest_dm_packet`, which
        // holds the owner-state lock but has no outbox handle) applies the
        // identical trust gates. This method stays the (dormant) outbox entry
        // point, delegating to the single source of truth.
        let invite_space_id = signed.space_id; // for the ZEB-639 ignore log
        match apply_invite(
            state,
            self.self_owner,
            &self.device_id,
            signed,
            signature,
            signed_bytes,
            wall_now_ms,
            // CodeRabbit F1: the dormant outbox method has no authenticated
            // transport-peer context, so it cannot bind the inviter. It is never
            // reached over an untrusted carrier; pass `None` to preserve its
            // existing (uncalled) behavior. The live tunnel ingest path
            // (`ingest_dm_packet`) passes `Some(owner)`.
            None,
            // ZEB-483: dormant authenticated path — refresh the cache as before.
            true,
            // ZEB-580 S2: this dormant path has no `RevokedDeviceProjection` handle
            // wired (no live caller); an empty projection is a safe no-op (it is
            // never reached over an untrusted carrier, mirroring `expected_inviter:
            // None` above).
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
        )? {
            // ZEB-236: this dormant path has no pending-invite store wired, so a
            // non-friend invite is staged-and-dropped here (the live ingest
            // paths own real staging). Warn so it is not silently swallowed.
            ApplyInviteOutcome::Staged(staged) => {
                tracing::warn!(
                    space_id = ?staged.signed.space_id,
                    "dormant handle_invite: non-friend invite staged-and-dropped (no store on this path)"
                );
                Ok(DrainOutcome::default())
            }
            ApplyInviteOutcome::Accepted => Ok(DrainOutcome::default()),
            // ZEB-639: non-friend invite for a space we already hold — no-op.
            ApplyInviteOutcome::IgnoredExistingSpace => {
                tracing::debug!(
                    space_id = ?invite_space_id,
                    "dormant handle_invite: non-friend invite ignored (space already exists locally)"
                );
                Ok(DrainOutcome::default())
            }
        }
    }

    /// Inbound `DmAck` handler — Phase 3b receive path for the sender side.
    ///
    /// Per ZEB-216 spec §"Application-signature binding rule" + Flow 3:
    ///   1. Look up signing pubkey via `lookup_pubkey_for_device`. None →
    ///      `UnknownSigningKey` (pre-bootstrap state — drop, telemetry).
    ///      Verify signature via `dm_signing::verify_dm_packet_signature`
    ///      (key-substitution defense + Ed25519 verify).
    ///   2. `resolve_signed_origin_owner(cache, signing_device_hash)` →
    ///      `resolved_owner` (UnknownSigningDevice / AmbiguousSigningDevice
    ///      drop on degenerate cache states).
    ///   3. `signed.ack_from_owner_addr ?= resolved_owner` — drop
    ///      `OwnerFieldMismatch` on cache-poisoning.
    ///   4. Look up the OutboxEntry by `(space_id, message_cid)`. Missing →
    ///      `OutboxEntryNotFound` (stale ack from before app restart, or
    ///      ack for an entry already swept).
    ///   5. Verify `resolved_owner ∈ entry.recipient_owners` —
    ///      `AckFromNonRecipient` (forged-ack regression).
    ///   6. Refresh OwnerDeviceCache with `signed.ack_from_devices` and
    ///      our newly-verified pubkey for the signer at the matching index.
    ///      Rejected outcome ignored — our cache may be fresher.
    ///   7. Call `mark_ack_delivered` to mutate `delivered_to`, recompute
    ///      `delivery_status`, and clear in-flight/backoff. Push into
    ///      `DrainOutcome.newly_delivered` if newly delivered (caller
    ///      emits `dm-delivered` IPC). `mark_ack_delivered` already calls
    ///      the CRDT `apply_outbox` path indirectly via direct mutation
    ///      with status recomputation — no separate apply needed.
    pub async fn handle_ack(
        &mut self,
        state: &mut OwnerState,
        signed: crate::dm_envelope::DmAckSigned,
        signature: [u8; 64],
        signed_bytes: &[u8],
        wall_now_ms: u64,
        revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
    ) -> Result<DrainOutcome, DmReceiveError> {
        // Step 1: look up signing pubkey + verify signature.
        let identity_pub =
            lookup_pubkey_for_device(&state.owner_device_cache, signed.signing_device_hash)
                .ok_or(DmReceiveError::UnknownSigningKey)?;
        crate::dm_signing::verify_dm_packet_signature(
            signed_bytes,
            &signature,
            &identity_pub,
            signed.signing_device_hash,
        )?;

        // Step 2: resolve signing_device_hash → OwnerAddr.
        let resolved_owner =
            resolve_signed_origin_owner(&state.owner_device_cache, signed.signing_device_hash)?;

        // ZEB-580 S2: defense-in-depth revocation cutoff. `handle_ack` is
        // dormant in production (Ack is rejected on the live tunnel at
        // dm_inbox_ingest.rs:556) — this guard exists so a future
        // re-activation of this path can't reintroduce a bypass for a
        // signer whose device #2 (Ed25519) has since been revoked.
        // Uniform/unconditional: no #2-vs-#3 branch (unlike the
        // membership-recipient checks below, which are outbox-specific).
        let ack_ed25519: [u8; 32] = identity_pub[32..64].try_into().expect("64 - 32 == 32");
        if revoked.is_revoked(&resolved_owner, &ack_ed25519) {
            return Err(DmReceiveError::SignerDeviceRevoked);
        }

        // Step 3: verify ack_from_owner_addr matches the resolved owner.
        // Drops cache-poisoning attempts where a peer claims an
        // ack_from_owner_addr that doesn't agree with the cryptographically-
        // authenticated source.
        if signed.ack_from_owner_addr != resolved_owner {
            return Err(DmReceiveError::OwnerFieldMismatch);
        }

        // Step 4: find the OutboxEntry by (space_id, message_cid). The
        // outbox is keyed by OutboxEntryId (a fresh ULID minted at send
        // time), so we iterate to locate the match. Missing entry =
        // stale ack from before app restart, or ack for an entry already
        // swept — drop with telemetry.
        let entry_id = state
            .outbox
            .iter()
            .find(|(_, e)| {
                e.space_id == signed.space_id && e.message_cid == Some(signed.message_cid)
            })
            .map(|(id, _)| *id)
            .ok_or(DmReceiveError::OutboxEntryNotFound)?;

        // Step 5: forged-ack defense — resolved_owner MUST be in the
        // entry's recipient_owners. A peer NOT on the recipient list cannot
        // legitimately ack the message; their ack must not advance
        // delivered_to.
        //
        // No `space.members` gate parallel to the receive path's membership
        // check is needed here. The receive path (`verify_cidnotify_admission`,
        // driven by `dm_inbox_ingest::ingest_dm_packet`) gates against the LIVE
        // space.members snapshot to block ex-members from injecting fresh
        // inbox writes. handle_ack instead gates against the OutboxEntry's
        // OWN `recipient_owners` snapshot, which was frozen at send time.
        // That's strictly stronger for the ack flow: a peer who was a member
        // at send time but was removed before acking is still a legitimate
        // recipient of the in-flight message — denying their ack would leak
        // delivery state. AckFromNonRecipient already covers the
        // ex-member-with-cached-key case (they were never in this entry's
        // recipient_owners), so a separate space.members lookup would be
        // redundant.
        let entry_ref = state
            .outbox
            .get(&entry_id)
            .expect("entry_id was just looked up from state.outbox");
        if !entry_ref.recipient_owners.contains(&resolved_owner) {
            return Err(DmReceiveError::AckFromNonRecipient);
        }

        // Step 6: refresh OwnerDeviceCache with ack.ack_from_devices.
        // Pubs vec: Some(identity_pub) at the signer's index, None at
        // every other index (Path B post-bootstrap — non-signer devices
        // remain pubs-less until the next signed packet from each
        // surfaces them). HLC uses our local wall clock + device_id.
        let mut updated_pubs: Vec<Option<[u8; 64]>> = vec![None; signed.ack_from_devices.len()];
        if let Some(idx) = signed
            .ack_from_devices
            .iter()
            .position(|d| *d == signed.signing_device_hash)
        {
            updated_pubs[idx] = Some(identity_pub);
        }
        // Ignore the apply outcome — Rejected (StaleHlc) is acceptable
        // here, our cache may already be fresher than what just arrived.
        let _ = state.apply_owner_device_update(
            resolved_owner,
            signed.ack_from_devices.clone(),
            updated_pubs,
            // ZEB-473: no tunnel contacts on this DM-ack cache refresh
            // (populated only on the friend handshake, Task 5).
            Vec::new(),
            Hlc {
                wall_ms: wall_now_ms,
                logical: 0,
                device_id: self.device_id.clone(),
            },
        );

        // Step 7: mutate delivered_to + recompute delivery_status via
        // mark_ack_delivered (Phase 2's primitive — handles the
        // delivered_to insert, status recompute (Expired-sticky), and
        // in_flight/backoff cleanup). Returns true iff this was newly
        // delivered (not a duplicate). Caller emits dm-delivered IPC for
        // the entries in newly_delivered.
        let mut drain_outcome = DrainOutcome::default();
        if self.mark_ack_delivered(state, entry_id, resolved_owner) {
            // ZEB-231: emit (space_id, message_cid, recipient) per
            // spec — internal OutboxEntryId is not part of the IPC
            // contract.
            drain_outcome.newly_delivered.push((
                signed.space_id,
                signed.message_cid,
                resolved_owner,
            ));
        }
        Ok(drain_outcome)
    }
}

/// ZEB-236: outcome of `apply_invite`. An invite from an ACTIVE friend is
/// auto-accepted (the friendship approval was the consent gate) → `Accepted`.
/// Anything else is `Staged` for an explicit user decision — carrying the
/// verified invite (plus its ingest-route entitlements) so the deferred
/// accept path can run the identical accept tail without re-verifying.
// `ApplyInviteOutcome` is a transient, immediately-matched return value (never
// stored in a collection or sent over a channel), so the `Accepted`-vs-`Staged`
// size asymmetry the lint flags has no memory cost worth boxing for — and the
// deferred-accept caller (`accept_dm_invite_impl`, Task 4) reads `s.signed`
// directly, which an unboxed payload keeps ergonomic.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(crate) enum ApplyInviteOutcome {
    /// The inviter was an active friend; the DM Space (and, when entitled, the
    /// OwnerDeviceCache) was written via `run_invite_accept_tail`.
    Accepted,
    /// The inviter was NOT an active friend; NOTHING was written. The verified
    /// invite is returned for the caller to stage (spec: staging is
    /// process-local only, so decline is indistinguishable from offline).
    Staged(crate::pending_dm_invites::StagedDmInvite),
    /// ZEB-639: a structurally-valid NON-FRIEND invite for a space that already
    /// exists locally. Never staged: we are already a member, so there is no
    /// consent to ask for — and a consent prompt here is exactly the kicked
    /// GroupDm co-member re-admit vector (forged fresh invite for the existing
    /// space_id). Legit roster changes arrive via Space CRDT sync, not invites.
    /// Matches the co-deposit path's semantics (it only stages on SpaceNotFound).
    /// Friend-tier invites are NOT gated — idempotent redelivery merge contract.
    IgnoredExistingSpace,
}

/// ZEB-691: the cert-verification + trust-bind core of `handle_revocation_push`,
/// factored out so the butler acceptor can PRE-VALIDATE a deposited revocation
/// (D7: never persist+ack a forgery) with the SAME authority the recipient uses
/// on recover. Returns the bridged revoked #2 ed25519 verify key.
pub(crate) fn verify_revocation_push(
    expected_owner: OwnerAddr,
    revocation: &harmony_owner::certs::RevocationCert,
    enrollment: &harmony_owner::certs::EnrollmentCert,
) -> Result<[u8; 32], DmReceiveError> {
    // 1. Master-signed revocation — `verify(None)` self-verifies the embedded
    //    master pub and binds `master.identity_hash() == revocation.owner_id`.
    revocation
        .verify(None)
        .map_err(|_| DmReceiveError::SignatureVerificationFailed)?;
    // 2. Trust-bind: the revocation AND the paired enrollment must belong to the
    //    pushing friend. A friend may only revoke THEIR OWN devices — this is
    //    what stops A relaying a (valid) third-party revocation into our
    //    projection. Both owner_ids are master-identity-hashes, so equality to
    //    `expected_owner` proves the same master signed both.
    if OwnerAddr(revocation.owner_id) != expected_owner
        || OwnerAddr(enrollment.owner_id) != expected_owner
    {
        return Err(DmReceiveError::OwnerFieldMismatch);
    }
    // 3. Verify the enrollment's master→#2 chain + its `device_id ↔ pubkeys`
    //    binding, EXPIRY-AGNOSTIC (pass `0` so the `now > expires_at` gate never
    //    fires — spec §8.5: a revoked device may hold an EXPIRED cert; the
    //    signature + id-binding are what secure the target→ed25519 bridge, not
    //    current validity), then bind the enrollment to the cert's target.
    enrollment
        .verify(0)
        .map_err(|_| DmReceiveError::SignatureVerificationFailed)?;
    if enrollment.device_id != revocation.target {
        return Err(DmReceiveError::OwnerFieldMismatch);
    }
    Ok(enrollment.device_pubkeys.classical.ed25519_verify)
}

/// ZEB-685 (S3): apply a friend-pushed device revocation. `expected_owner` is
/// the tunnel-peer's resolved owner (a friend). Verifies the master-signed
/// revocation + the paired enrollment, trust-binds BOTH to `expected_owner` (a
/// friend may only revoke THEIR OWN devices — never relay a third party's
/// revocation into our projection), bridges the cert target (`device_id[16]`)
/// to the revoked `ed25519[32]` via the enrollment, stores it union-merged in
/// the owner-state CRDT, and feeds the live `RevokedDeviceProjection` so the
/// §5.2 DM cutoff rejects that device's DMs for this DM-only contact.
///
/// `pub` (not `pub(crate)`) so the end-to-end cutoff integration test can drive
/// the real handler alongside the live `dm_inbox_ingest::ingest_dm_packet`
/// receive entrypoint it pairs with.
///
/// Returns `Ok(true)` iff a NEW revoked key was inserted into the owner-state
/// CRDT store (`Ok(false)` on an idempotent re-apply). The caller uses this to
/// mark the owner-state engine dirty ONLY on a genuine change — the store lives
/// in the owner-state CRDT, which persists + replicates to sibling devices only
/// via a `notify_dirty` flush, and RevocationPush has no deposit-rung backstop,
/// so without this the revocation is lost on restart (boot-replay re-seeds
/// nothing) and never reaches the owner's other devices.
pub fn handle_revocation_push(
    state: &mut OwnerState,
    expected_owner: OwnerAddr,
    revocation: &harmony_owner::certs::RevocationCert,
    enrollment: &harmony_owner::certs::EnrollmentCert,
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
) -> Result<bool, DmReceiveError> {
    let ed25519 = verify_revocation_push(expected_owner, revocation, enrollment)?;
    // Bridge target `device_id` → the revoked `ed25519`, store union-merged
    // (survives across the owner's devices), and feed the live projection.
    let inserted = state.apply_revoked_dm_device(expected_owner, ed25519);
    // ZEB-699: the projection feed is DELIBERATELY unconditional — even when
    // the capped store evicted the key (`inserted == false` at the ZEB-692
    // 256-cap edge), the revocation is cryptographically proven above, and the
    // projection is the live ENFORCEMENT surface while the store is the
    // durability record. The resulting projection ⊇ store divergence is
    // fail-closed (over-reject only, never under-reject) and self-heals on
    // restart (the projection boot-seeds from the capped store). Gating the
    // feed on store survival would not restore projection == store anyway:
    // a later smaller-key insert evicts an already-fed max and the union-only
    // projection has no removal, so the divergence is inherent — enforcing
    // the proven revocation is the better half of the trade.
    let mut one = std::collections::BTreeSet::new();
    one.insert(ed25519);
    revoked.union_from_members(std::iter::once((expected_owner, &one)));
    Ok(inserted)
}

/// ZEB-482: auto-accept a received DmInvite — write the DM Space + cache the
/// inviter's devices/identity-pub. Idempotent on `space_id`. Shared by the
/// (dormant) outbox `handle_invite` method and the tunnel ingest path so both
/// apply identical trust gates. No IPC emit (invites carry no `dm-received`).
///
/// Parameterized on `self_owner` / `device_id` (the receiver's identity)
/// instead of `&self.*` so the ingest path — which holds the owner-state lock
/// but has no `DmOutbox` handle — can call it directly. Behavior is identical
/// to the prior `handle_invite` body.
// The arg list is the receiver identity + the verified-invite triple + the
// learned-at clock + the F1 inviter-bind hint; threading them through a struct
// would not improve clarity at this single shared call boundary.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_invite(
    state: &mut OwnerState,
    self_owner: OwnerAddr,
    device_id: &str,
    signed: crate::dm_envelope::DmInviteSigned,
    signature: [u8; 64],
    signed_bytes: &[u8],
    wall_now_ms: u64,
    // ZEB-482 (CodeRabbit F1): the owner the AUTHENTICATED tunnel peer resolves
    // to. `verify_dm_packet_signature` only authenticates the signing DEVICE; it
    // does NOT prove the payload-controlled `signed.inviter` (OwnerAddr) is the
    // peer that actually sent this frame. Without this bind, a valid signer (a
    // malicious friend) could claim `inviter = <victim owner>` and poison the
    // receiver's OwnerDeviceCache (mapping the attacker's device under the
    // victim's owner) + create a spoofed DM Space. The tunnel ingest path passes
    // `Some(owner)` resolved from the peer's DeviceTunnelContact and we reject any
    // `signed.inviter` mismatch BEFORE any state mutation. The dormant outbox
    // `handle_invite` method has no transport-peer context and passes `None`,
    // preserving its existing (uncalled) behavior.
    expected_inviter: Option<OwnerAddr>,
    // ZEB-483 (CodeRabbit): whether to refresh the OwnerDeviceCache from the
    // invite. `true` for the authenticated tunnel / dormant path (it legitimately
    // learns the inviter's signing device). `false` for the deposit-recover path,
    // which has already verified the sender against the pristine cache and must
    // NOT let a sender-claimed device list regress that verified state — it
    // bootstraps ONLY the Space.
    refresh_owner_device_cache: bool,
    // ZEB-580 S2: the shared-community revocation projection — forwarded to the
    // post-signature-verify cutoff below. Mirrors `verify_cidnotify_sender_binding`'s
    // `revoked` param on the CidNotify path.
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
) -> Result<ApplyInviteOutcome, DmReceiveError> {
    // SECURITY (CodeRabbit F1): bind the payload-controlled `signed.inviter` to
    // the authenticated tunnel peer BEFORE touching any trust state (cache or
    // Space). When the caller cannot supply an expected owner (`None`), behavior
    // is unchanged — that path (the dormant outbox method) is never reached over
    // an untrusted transport.
    if let Some(expected) = expected_inviter {
        if signed.inviter != expected {
            return Err(DmReceiveError::InviterMismatch);
        }
    }
    // Sanity gate 1: inviter ∈ members.
    if !signed.members.contains(&signed.inviter) {
        return Err(DmReceiveError::InviterNotInMembers);
    }
    // Sanity gate 2: signing_device_hash ∈ sender_devices.
    // (decode_packet already enforces this — defense-in-depth here.)
    if !signed.sender_devices.contains(&signed.signing_device_hash) {
        return Err(DmReceiveError::SigningDeviceNotInSenderDevices);
    }
    // Sanity gate 3: self_owner ∈ members.
    if !signed.members.contains(&self_owner) {
        return Err(DmReceiveError::ReceiverNotInMembers);
    }
    // ZEB-580 S1: resolve the signer's combined pub. If the invite carries an
    // inviter_enrollment (#2 cert), verify master→#2 + owner binding + hash
    // agreement and use the #2 combined pub; otherwise fall back to the legacy
    // inline #3 pub. Either way the resolved pub is fed to
    // verify_dm_packet_signature (which splits it: Ed25519 half checks the
    // actual signature, X25519 half participates only in the device-hash
    // recomputation that defeats key-substitution).
    let signer_identity_pub: [u8; 64] = if let Some(cert) = &signed.inviter_enrollment {
        // Master-issued only on this path (N3): no signer bundle carried, so a
        // Quorum cert reaching here (empty signer_certs) errors → mapped to
        // SignatureVerificationFailed → the invite is dropped (the sender must
        // re-bootstrap via the friend handshake). Expiry-agnostic (now_secs =
        // 0), matching DmOutbox::new's ZEB-378 invariant.
        crate::enrollment_verify::verify_enrollment_any_issuer(
            cert,
            &[],
            Some(&signed.inviter.0),
            0,
        )
        .map_err(|_| DmReceiveError::SignatureVerificationFailed)?;
        // The cert's #2 DM hash must equal the body's claimed signing device
        // hash — rejects an invite that carries a valid cert but is signed by a
        // DIFFERENT device (forged-sender / cert-hash desync).
        let expected = crate::dm_signing::device2_signing_hash(cert)
            .ok_or(DmReceiveError::SignatureVerificationFailed)?;
        if expected != signed.signing_device_hash {
            return Err(DmReceiveError::SigningKeyDoesNotMatchDeviceHash);
        }
        let d2_pub = crate::dm_signing::device2_combined_pub(cert);
        // Defense in depth: the cert and the self-consistent inline pub (Task 5
        // sets inviter_identity_pub = device2_combined_pub on the #2 path) must
        // agree, so a mismatched invite (cert for one #2, inline pub for
        // another) is rejected rather than silently trusting the cert over the
        // signed pub.
        if d2_pub != signed.inviter_identity_pub {
            return Err(DmReceiveError::SigningKeyDoesNotMatchDeviceHash);
        }
        d2_pub
    } else {
        signed.inviter_identity_pub
    };
    crate::dm_signing::verify_dm_packet_signature(
        signed_bytes,
        &signature,
        &signer_identity_pub,
        signed.signing_device_hash,
    )?;

    // ZEB-580 S2: revocation cutoff — drop a Space invite signed by a revoked #2
    // device before any cache/Space write. `signed.inviter` is bound to the
    // authenticated peer above (expected_inviter). No-op for legacy #3 (its
    // inline pub's ed25519 is never an enrolled key).
    let inviter_ed25519: [u8; 32] = signer_identity_pub[32..64]
        .try_into()
        .expect("64 - 32 == 32");
    if revoked.is_revoked(&signed.inviter, &inviter_ed25519) {
        return Err(DmReceiveError::SignerDeviceRevoked);
    }

    // ZEB-236 tier fork: invites from ACTIVE friends keep Phase 3b's
    // auto-accept (the friendship approval was the consent gate). Anything
    // else is STAGED for an explicit user decision — no Space, no cache
    // write, nothing persistent (spec: decline must be indistinguishable
    // from offline, so staging itself is process-local only).
    let inviter_is_active_friend = state
        .friend_graph
        .friends
        .get(&signed.inviter)
        .is_some_and(|e| e.status == crate::friend_graph::FriendStatus::Active);
    if !inviter_is_active_friend {
        // ZEB-639 (1): never stage a non-friend invite for a space we already
        // hold — there is no consent to ask for, and prompting is the kicked
        // GroupDm co-member re-admit vector. Friend-tier invites bypass this
        // (idempotent redelivery merge). Tombstoned spaces are NOT in
        // `state.spaces`, so they still stage (accept later surfaces the
        // permanent rejection).
        if state.spaces.contains_key(&signed.space_id) {
            return Ok(ApplyInviteOutcome::IgnoredExistingSpace);
        }
        return Ok(ApplyInviteOutcome::Staged(
            crate::pending_dm_invites::StagedDmInvite {
                signed,
                received_at_ms: wall_now_ms,
                refresh_owner_device_cache,
                // ZEB-236 (final review): the CO-DEPOSIT ingest sites tag this
                // with the notifying message's `message_cid` AFTER this returns
                // (they hold the verified CidNotify); the tunnel / dormant path
                // leaves it None — it never sweep-redelivers, so a genuine
                // re-send is a new sender action that SHOULD re-prompt.
                source_cid: None,
            },
        ));
    }
    run_invite_accept_tail(
        state,
        device_id,
        signed,
        wall_now_ms,
        refresh_owner_device_cache,
        signer_identity_pub,
    )?;
    Ok(ApplyInviteOutcome::Accepted)
}

/// ZEB-236: the invite ACCEPT tail — exactly the Phase 3b auto-accept body,
/// extracted so the deferred user-accept path (`accept_dm_invite_impl`) and
/// the friend-tier auto-accept run the same code. Callers guarantee `signed`
/// already passed `apply_invite`'s gates + signature verification.
pub(crate) fn run_invite_accept_tail(
    state: &mut OwnerState,
    device_id: &str,
    signed: crate::dm_envelope::DmInviteSigned,
    wall_now_ms: u64,
    refresh_owner_device_cache: bool,
    // ZEB-580 S1: the inviter's resolved combined identity pub to cache for the
    // signing device — the cert's #2 combined pub when `apply_invite` took the
    // cert path, else the legacy inline #3 pub. Callers resolve this; the tail
    // must NOT re-read `signed.inviter_identity_pub` for the cache write (that
    // would cache the #3 pub even on a #2-verified invite).
    signer_identity_pub: [u8; 64],
) -> Result<(), DmReceiveError> {
    // Phase 3b auto-accept: write the Space, and — on the authenticated
    // tunnel/dormant path only — refresh the OwnerDeviceCache.
    // (Phase 4 will replace this with a stage-pending-invite + UI prompt
    // path; follow-up ticket filed at PR-creation time per spec.)

    // Build the Space from the invite. Mirror what add_space's DM/group-DM
    // handling will produce (Phase 4 will produce these on the SEND side
    // as outbound invites; here we mirror the same shape on the RECEIVE
    // side as inbound invite acceptance).
    // ZEB-474: DM/GroupDm Spaces carry transport=None (deposit-only;
    // the Reticulum carrier was removed). Delivery uses OwnerDeviceCache,
    // not Space.transport.
    // SECURITY (ZEB-639): clamp the Space's LWW driver to a local-clock
    // ceiling. `lww_merge_space` is LWW-by-`updated_at` and GroupDm dedupe_key
    // is id-derived (members ARE mutable on the same SpaceId), so echoing the
    // invite-controlled `created_at` would let one forged far-future HLC pin
    // this Space against every future legitimate update — the same
    // denial-of-updates attack the cache `learned_at` rule below already
    // defeats. Legit invites have past created_at → clamp is a no-op (golden
    // parity tests pin this). `created_at` keeps the claimed value: it is
    // provenance/display and does not drive LWW.
    let updated_at = if signed.created_at.wall_ms > wall_now_ms {
        Hlc {
            wall_ms: wall_now_ms,
            logical: 0,
            device_id: device_id.to_string(),
        }
    } else {
        signed.created_at.clone()
    };
    let space = crate::owner_state_types::Space {
        id: signed.space_id,
        kind: signed.kind,
        parent: None,
        community_id: None,
        name: format!("DM with {}", hex::encode(signed.inviter.0)),
        transport: None,
        members: signed.members,
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: signed.created_at.clone(),
        updated_at,
        content_key: Some(signed.content_key),
        prior_content_keys: vec![],
        current_epoch: None,
        current_epoch_key: None,
        old_epoch_keys: std::collections::BTreeMap::new(),
        admin_addr: None,
        is_invite_only: None,
        shared_in_profile: false,
        read_receipt_pref: None,
        pending_join_at: None,
    };
    let space_outcome = state.apply_space_with_canonicalization(space);
    if let crate::owner_state_crdt::ApplyOutcome::Rejected(reason) = space_outcome {
        return Err(DmReceiveError::CrdtRejected(format!("{:?}", reason)));
    }

    // ZEB-483 (CodeRabbit): the OwnerDeviceCache refresh is gated. The
    // authenticated tunnel / dormant path (`refresh_owner_device_cache = true`)
    // legitimately learns the inviter's signing device from the invite. The
    // deposit-recover path passes `false`: it has ALREADY verified the sender
    // against the PRISTINE cache (which is authoritative — see
    // `verify_cidnotify_sender_binding`), and applying the invite's singleton,
    // sender-claimed `sender_devices` at a fresh local HLC would let a stale or
    // replayed deposited invite SHRINK / regress that verified device set
    // (`apply_owner_device_update` is LWW-by-`learned_at` and REPLACES, not
    // unions, the device list). Deposit recovery therefore bootstraps ONLY the
    // Space; the verified CidNotify path owns all cache mutation.
    if refresh_owner_device_cache {
        // Build a parallel pubs vec: Some(inviter_identity_pub) at the signer's
        // index, None everywhere else. The receiver knows the inviter's
        // identity pub for the device that signed THIS invite, but has no pubs
        // for the inviter's other devices yet — they remain pre-bootstrap
        // until the next invite-equivalent flow.
        let mut device_identity_pubs: Vec<Option<[u8; 64]>> =
            vec![None; signed.sender_devices.len()];
        let signer_idx = signed
            .sender_devices
            .iter()
            .position(|d| *d == signed.signing_device_hash)
            .expect("sanity gate 2 already verified signing_device_hash ∈ sender_devices");
        device_identity_pubs[signer_idx] = Some(signer_identity_pub);

        // SECURITY: the OwnerDeviceCache LWW HLC must record when WE
        // learned about these devices, NOT the timestamp the inviter
        // claims they sent the invite. Using `signed.created_at` here
        // would let an attacker forge a far-future HLC (e.g.,
        // wall_ms = u64::MAX / 2) on a single malicious invite,
        // pinning the local cache and rejecting every legitimate
        // future update from the same owner as `StaleHlc` — a
        // denial-of-updates attack. Mirror the pattern the receive path's
        // Phase A admission (`verify_cidnotify_admission`) already uses
        // (local wall clock + our device_id).
        let learned_at = Hlc {
            wall_ms: wall_now_ms,
            logical: 0,
            device_id: device_id.to_string(),
        };

        // SECURITY (CodeRabbit F2): only write the OwnerDeviceCache AFTER the
        // Space apply succeeds (above). A malformed/rejected invite must not
        // alter trust state, so the cache mutation is sequenced behind the
        // Space's invariant check.
        let cache_outcome = state.apply_owner_device_update(
            signed.inviter,
            signed.sender_devices,
            device_identity_pubs,
            // ZEB-473: no tunnel contacts on this DM-receive cache refresh
            // (populated only on the friend handshake, Task 5).
            Vec::new(),
            learned_at,
        );
        if let crate::owner_state_crdt::ApplyOutcome::Rejected(reason) = cache_outcome {
            return Err(DmReceiveError::CrdtRejected(format!("{:?}", reason)));
        }
    }

    Ok(())
}

/// ZEB-483: apply a deposited DmInvite (if present) to bootstrap ONLY the DM
/// Space, on the deposit-recover path (no authenticated tunnel peer).
///
/// SECURITY (CodeRabbit, Critical): the caller MUST already have verified the
/// co-deposited CidNotify's signer against the PRISTINE `OwnerDeviceCache` (via
/// [`verify_cidnotify_sender_binding`]) and pass the VERIFIED results here —
/// `expected_inviter` / `expected_space_id` / `expected_signing_device_hash` /
/// `expected_identity_pub`. Every trust-bearing field of the decoded invite is
/// pinned to those verified values BEFORE any state mutation, so the invite can
/// only ever bootstrap the exact Space the verified sender is notifying about —
/// it can neither introduce a new `device → owner → pub` binding nor target a
/// different Space. It bootstraps ONLY the Space: `apply_invite` is called with
/// `refresh_owner_device_cache = false`, so the deposit-recover path never
/// mutates the `OwnerDeviceCache` (the verified CidNotify path owns that). A
/// forged, non-DM, or mismatched invite is rejected before it touches any state.
/// Size-bounded; fail-closed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_deposited_invite(
    state: &mut OwnerState,
    self_owner: OwnerAddr,
    device_id: &str,
    invite_packet: &[u8],
    expected_inviter: OwnerAddr,
    expected_space_id: SpaceId,
    expected_signing_device_hash: DeviceIdentityHash,
    expected_identity_pub: [u8; 64],
    wall_now_ms: u64,
    // ZEB-580 S2: forwarded to `apply_invite`'s revocation cutoff.
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
) -> Result<Option<crate::pending_dm_invites::StagedDmInvite>, String> {
    if invite_packet.len() > crate::butler_deposit::MAX_DEPOSIT_INVITE_BYTES {
        return Err(format!(
            "deposited invite too large: {} bytes",
            invite_packet.len()
        ));
    }
    let packet = crate::dm_envelope::decode_packet(invite_packet)
        .map_err(|e| format!("decode invite: {e}"))?;
    let crate::dm_envelope::DmPacket::Invite {
        signed,
        signature,
        signed_bytes,
    } = packet
    else {
        return Err("deposited invite_packet is not an Invite".into());
    };
    // SpaceKind is enforced at the EARLIEST point: `decode_packet` above rejects
    // any DmInvite whose `kind` is not Dm/GroupDm (a payload invariant), so a
    // non-DM invite never decodes and can never reach `apply_invite` to build a
    // Space — no separate downstream SpaceKind gate is needed here (CodeRabbit
    // round 3; the `verify_cidnotify_space` kind check is the redundant backstop).
    // Pin every trust-bearing invite field to the independently-verified
    // CidNotify sender BEFORE any mutation (CodeRabbit Critical). `inviter` is
    // additionally enforced inside `apply_invite` via `Some(expected_inviter)`.
    if signed.space_id != expected_space_id {
        return Err("deposited invite space_id does not match verified CidNotify".into());
    }
    if signed.signing_device_hash != expected_signing_device_hash {
        return Err("deposited invite signer does not match verified CidNotify signer".into());
    }
    if signed.inviter_identity_pub != expected_identity_pub {
        return Err(
            "deposited invite identity_pub does not match verified CidNotify signer".into(),
        );
    }
    match apply_invite(
        state,
        self_owner,
        device_id,
        signed,
        signature,
        &signed_bytes,
        wall_now_ms,
        Some(expected_inviter),
        // ZEB-483 (CodeRabbit): deposit-recover bootstraps ONLY the Space. The
        // sender was already verified against the pristine cache, so this invite
        // must NOT mutate authenticated device-cache state.
        false,
        revoked,
    )
    .map_err(|e| format!("apply_invite: {e:?}"))?
    {
        // ZEB-236 (T3): a non-friend deposited invite bootstraps NO Space; the
        // verified invite is handed back to the caller to stage + emit AFTER it
        // releases the owner-state lock (this fn holds only `&mut state` and has
        // no store/sink handle — the ingest ctx owns those). `Accepted` (active
        // friend) already wrote the Space, so there is nothing to stage.
        ApplyInviteOutcome::Staged(staged) => Ok(Some(staged)),
        ApplyInviteOutcome::Accepted => Ok(None),
        // ZEB-639: non-friend invite for a space we already hold — nothing to
        // stage, nothing written (same caller contract as `Accepted`).
        ApplyInviteOutcome::IgnoredExistingSpace => Ok(None),
    }
}

/// ZEB-233: lock-lifted drain entrypoint for production.
///
/// Three-phase locked/unlocked/re-locked structure (ZEB-241 — the same
/// shape the live receive path `dm_inbox_ingest::ingest_dm_packet` uses):
///
/// * **Phase A (locked, try_lock):** acquire `outbox` + `state` via
///   `try_lock`. On contention, skip this tick. Calls
///   `DmOutbox::drain_phase_a` to collect work units + mark in_flight.
///   Locks drop at the end of this block.
///
/// * **Phase B (unlocked):** iterate work units, awaiting each
///   `transport.send().await`. No locks held — concurrent `send_dm`
///   IPC calls progress against the released outbox lock.
///
/// * **Phase C (spawned, locked):** spawn a `tokio::spawn` task that
///   re-acquires `outbox` + `state` via `.lock().await` (not
///   `try_lock` — the spawn detaches from the event_loop's `select!`,
///   so `.lock().await` here doesn't risk the cas_op_rx deadlock that
///   forced try_lock at Phase A). Calls `DmOutbox::drain_phase_c` to
///   apply results + run the expiration sweep + cleanup. Emits
///   `dm-delivered` / `dm-expired` IPC events from the spawned task.
///
/// ## Why spawn Phase C instead of awaiting inline
///
/// The event_loop runs in a single `select!` and pumps multiple arms
/// (UDP recv, timer tick, cas_op_rx, etc.). Drain runs on the timer
/// tick arm. If Phase C used `.lock().await` inline, the event_loop
/// would stall at the timer arm waiting for the outbox lock — and a
/// concurrent `send_dm` IPC (which holds outbox while awaiting
/// `cas.put()` → `cas_op_tx`) could deadlock because cas_op_rx never
/// gets pumped while the event_loop is parked at the timer arm.
///
/// Spawning Phase C makes the timer arm return immediately after
/// Phase B completes. The event_loop resumes its `select!` and can
/// pump cas_op_rx, unblocking the holder of outbox, which lets
/// Phase C's `.lock().await` eventually succeed.
///
/// ## Why try_lock at Phase A
///
/// Phase A is on the timer arm hot path. Same deadlock risk as the
/// original (pre-ZEB-233) drain caller — preserved by keeping
/// try_lock + skip-this-tick at this boundary.
///
/// ## Effect on concurrent send_dm IPCs
///
/// Before ZEB-233: `send_dm` blocks at `outbox.lock().await` while
/// drain awaits `transport.send()` (~500ms-5s on real Reticulum).
/// After ZEB-233: `send_dm` acquires outbox immediately during
/// Phase B because the lock is released. Latency improvement is
/// proportional to the slowest in-flight transport.send.
pub async fn drain_lifted(
    outbox: std::sync::Arc<tokio::sync::Mutex<DmOutbox>>,
    state: std::sync::Arc<tokio::sync::Mutex<OwnerState>>,
    transport: &dyn DmTransport,
    wall_now_ms: u64,
    app: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    // ZEB-703: owner-state SyncEngine handle. Phase C's CRDT mutations
    // (delivered_to/status transitions, the 30-day expiry sweep) and the
    // deposit rungs' `mark_ack_delivered` writes must `notify_dirty()` or
    // they are never persisted at runtime NOR replicated to paired devices
    // (the owner-state CRDT persists + replicates ONLY via a notify_dirty
    // flush — same discipline as the ZEB-685 revocation drain). `None` in
    // tests that don't exercise durability; production always passes the
    // live engine.
    owner_sync: Option<std::sync::Arc<crate::owner_state_sync::SyncEngine>>,
) {
    // Phase A: try_lock + collect work. If either lock is contended,
    // skip this tick. (Same deadlock-avoidance rationale as the
    // original event_loop drain caller — see event_loop.rs timer arm
    // comment.)
    let (work, phase_c_sem) = match (outbox.try_lock(), state.try_lock()) {
        (Ok(mut o_g), Ok(s_g)) => {
            // ZEB-703 (PR #485 Greptile P1): once the shutdown barrier has
            // gated the drain path, skip the whole tick — no sends, no
            // Phase C spawn — so no drain-path CRDT mutation can land
            // after the pre-ack owner-state snapshot.
            if o_g.shutdown_gate.load(std::sync::atomic::Ordering::Acquire) {
                tracing::debug!("drain_lifted: shutdown gate set; skipping tick");
                return;
            }
            (
                o_g.drain_phase_a(&s_g, wall_now_ms),
                Arc::clone(&o_g.phase_c_inflight),
            )
        }
        _ => {
            tracing::debug!("drain_lifted Phase A: outbox/state lock contended; skipping tick");
            return;
        }
    };

    // Phase B: unlocked transport sends. Concurrent send_dm IPCs hold
    // outbox/state during this stretch, so they don't block on us.
    //
    // ## TOCTOU liveness re-check (ZEB-233 round 2, closes ZEB-277)
    //
    // Between Phase A's lock-drop and a given `transport.send` in this
    // loop, the (entry, recipient) pair could have its OutboxEntry
    // deleted by `delete_dm_outbox_entry`, marked Complete for that
    // recipient by `handle_ack`, or had its in_flight marker cleared
    // by a previous Phase C cleanup pass. Before each send we re-check
    // liveness using `try_lock` (NOT `.lock().await` — that would
    // reintroduce the deadlock Phase A's try_lock was designed to
    // avoid). On contention OR liveness-check-fail, we push to
    // `skipped` and let Phase C clear the in_flight marker; the next
    // drain tick re-evaluates.
    //
    // Cost: one try_lock pair (outbox + state) per work unit. The
    // guards drop at the end of the `match` arm — BEFORE
    // `transport.send.await`. No locks held across the await.
    //
    // Failure modes after this fix:
    //   * Locks contended at the moment of try_lock: skip + clear
    //     in_flight in Phase C → next tick re-attempts.
    //   * Entry deleted between Phase A and Phase B: skip + clear
    //     in_flight in Phase C → no stale send.
    //   * Recipient acked between Phase A and Phase B: skip + clear
    //     in_flight in Phase C → no duplicate send.
    let mut results = Vec::with_capacity(work.len());
    let mut skipped: Vec<(OutboxEntryId, OwnerAddr)> = Vec::new();
    for unit in work {
        let entry_id = unit.entry_id;
        let recipient = unit.recipient;
        // ZEB-233 round 4 (CodeRabbit Major): the liveness re-check
        // ALSO re-resolves destinations from the CURRENT
        // `owner_device_cache`. Phase A's snapshot can be stale if a
        // device was rotated/revoked between Phase A and Phase B —
        // without the re-resolve, the send would target stale device
        // hashes (misdelivery to a revoked device). Returns
        // `Some(destinations)` only when both still-live AND
        // lock-acquisition succeed.
        let destinations = match (outbox.try_lock(), state.try_lock()) {
            (Ok(o_g), Ok(s_g)) => {
                let live = s_g.outbox.get(&entry_id).is_some_and(|entry| {
                    matches!(
                        entry.delivery_status,
                        DeliveryStatus::Pending | DeliveryStatus::Partial
                    ) && !entry.delivered_to.contains(&recipient)
                        && o_g.in_flight.contains(&(entry_id, recipient))
                });
                // Compute the re-resolved destinations under the same
                // try_lock guard, so the cache read is consistent with
                // the liveness check above. o_g + s_g drop at end of
                // match arm — BEFORE the `.await` below. No locks
                // held across the await.
                if live {
                    Some(resolve_destinations(&s_g.owner_device_cache, recipient))
                } else {
                    None
                }
            }
            _ => {
                tracing::debug!(
                    ?entry_id,
                    ?recipient,
                    "drain_lifted Phase B: lock contended on liveness re-check; skipping send (next tick retries)"
                );
                None
            }
        };
        let Some(destinations) = destinations else {
            skipped.push((entry_id, recipient));
            continue;
        };
        let result = transport
            .send(&unit.entry_clone, recipient, destinations)
            .await;
        results.push(DrainSendResult {
            entry_id,
            recipient,
            result,
        });
    }

    // ZEB-703 (PR #485 Greptile P1): fence the detached Phase C task. One
    // permit, held for the task's whole lifetime (Phase C lock-block AND
    // the deposit rungs — every drain-path CRDT mutation site), so the
    // shutdown barrier's acquire_many awaits it before snapshotting
    // owner-state. Exhaustion (64 outstanding tasks) means pathological
    // wedging: skip this tick's Phase C with a WARN rather than spawning
    // unfenced — the next tick re-derives everything from state.
    let phase_c_permit = match phase_c_sem.try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            // ZEB-710: count the degrade — wedge visibility must not be
            // log-only.
            DM_FENCE_STATS.record_phase_c_saturated_skip();
            tracing::warn!(
                "drain_lifted: Phase C fence exhausted ({} in flight); skipping this \
                 tick's Phase C (ZEB-703)",
                DRAIN_PHASE_C_FENCE_CAPACITY
            );
            return;
        }
    };

    // Phase C: spawn so the event_loop's timer arm returns to select!
    // immediately. The spawned task runs detached and uses .lock().await
    // (not try_lock) — by the time it awakens after .lock().await
    // returns, the event_loop is free to pump cas_op_rx, so any holder
    // of outbox/state can release.
    tokio::spawn(async move {
        // ZEB-703: hold the fence permit until every mutation in this task
        // (Phase C outcomes + deposit-rung acks) has completed.
        let _phase_c_permit = phase_c_permit;
        let (outcome, deposit_candidates, deposit_client, relay_client) = {
            let mut o_g = outbox.lock().await;
            let mut s_g = state.lock().await;
            // ZEB-233 round 1 (Qodo Correctness #1) + round 3 (CodeRabbit
            // Major): Phase C needs two distinct timestamps. `backoff_now_ms`
            // is recomputed AFTER lock acquisition — it reflects when the
            // send outcome was actually recorded, so `last_attempt_wall_ms`
            // bookkeeping is accurate. `expiration_now_ms` REUSES the
            // outer `wall_now_ms` from the captured `let` move — it
            // reflects when Phase A admitted this drain tick as in-flight,
            // so the 30-day expiration sweep can't expire an entry in the
            // same tick it was just sent due to Phase B/C latency.
            let backoff_now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let (outcome, candidates) =
                o_g.drain_phase_c(&mut s_g, results, skipped, backoff_now_ms, wall_now_ms);
            let client = o_g.butler_deposit_client.clone();
            // ZEB-458 P4 Phase B: also capture the relay client so the
            // last-resort rung can fire after the butler rung per candidate.
            let relay = o_g.community_relay_deposit_client.clone();
            // Locks drop at the end of this block — before any IPC emit
            // and before the deposit rungs' network I/O below.
            (outcome, candidates, client, relay)
        };
        // ZEB-703: Phase C mutated the owner-state CRDT iff it recorded a
        // delivery or expired an entry — mark the engine dirty so the
        // debounced flush persists + replicates the transition. Never
        // notify on an idle tick (an unconditional notify would republish
        // a byte-identical root every debounce window, forever).
        if !outcome.newly_delivered.is_empty() || !outcome.newly_expired.is_empty() {
            if let Some(engine) = owner_sync.as_ref() {
                engine.notify_dirty();
            }
        }
        for (space_id, message_cid, recipient) in outcome.newly_delivered {
            let payload = serde_json::json!({
                "spaceId": hex::encode(space_id.0),
                "messageCid": hex::encode(message_cid.to_bytes()),
                "recipientOwnerAddr": hex::encode(recipient.0),
            });
            crate::node_event_sink::emit_ser(app.as_ref(), "dm-delivered", &payload);
        }
        for (space_id, message_cid) in outcome.newly_expired {
            let payload = serde_json::json!({
                "spaceId": hex::encode(space_id.0),
                "messageCid": hex::encode(message_cid.to_bytes()),
            });
            crate::node_event_sink::emit_ser(app.as_ref(), "dm-expired", &payload);
        }

        // ZEB-418 P1 Task 8 — butler deposit rung. Runs UNLOCKED (pkarr
        // resolve + up to two iroh dials, each deadline-bounded), after the
        // direct results' events have already been emitted so a slow
        // deposit can't delay them. An ack re-acquires the locks briefly
        // and routes through the existing idempotent `mark_ack_delivered`
        // (a raced direct ack makes it a no-op); skip/failure outcomes
        // touch NOTHING — the entry keeps the exact transient-failure
        // backoff Phase C just recorded (spec §6: never worse than today).
        //
        // ZEB-458 P4 Phase B: after the butler match, if the butler did
        // NOT ack, attempt the last-resort community-relay rung (if
        // installed). A relay ack mirrors the butler Acked arm exactly:
        // re-acquire locks, `mark_ack_delivered`, emit `dm-delivered`.
        if deposit_client.is_none() && relay_client.is_none() {
            return;
        }
        for c in deposit_candidates {
            let butler_acked = if let Some(ref client) = deposit_client {
                let butler_outcome = client.deposit(&c).await;
                let acked = matches!(
                    butler_outcome,
                    crate::butler_deposit::DepositRungOutcome::Acked
                );
                match butler_outcome {
                    crate::butler_deposit::DepositRungOutcome::Acked => {
                        let newly = {
                            let mut o_g = outbox.lock().await;
                            let mut s_g = state.lock().await;
                            o_g.mark_ack_delivered(&mut s_g, c.entry_id, c.recipient_owner)
                        };
                        // ZEB-703: a fresh ack mutated delivered_to/status —
                        // persist + replicate it (idempotent re-ack: no-op,
                        // no notify).
                        if newly {
                            if let Some(engine) = owner_sync.as_ref() {
                                engine.notify_dirty();
                            }
                        }
                        // ZEB-505: invite-only entries ack but emit no
                        // `dm-delivered` (no message to surface).
                        if newly {
                            if let Some(message_cid) = c.message_cid {
                                let payload = serde_json::json!({
                                    "spaceId": hex::encode(c.space_id.0),
                                    "messageCid": hex::encode(message_cid.to_bytes()),
                                    "recipientOwnerAddr": hex::encode(c.recipient_owner.0),
                                });
                                crate::node_event_sink::emit_ser(
                                    app.as_ref(),
                                    "dm-delivered",
                                    &payload,
                                );
                            }
                        }
                    }
                    crate::butler_deposit::DepositRungOutcome::SkippedNoFreshButlerSet => {}
                    crate::butler_deposit::DepositRungOutcome::Failed(e) => {
                        tracing::debug!(
                            entry_id = ?c.entry_id,
                            recipient = ?c.recipient_owner,
                            error = %e,
                            "ZEB-418: butler deposit rung failed; existing retry chain continues"
                        );
                    }
                }
                acked
            } else {
                false
            };
            // ZEB-458 P4: last-resort community relay rung — only if the
            // butler did not ack.
            if !butler_acked {
                if let Some(ref relay) = relay_client {
                    if relay.deposit(&c).await {
                        let newly = {
                            let mut o_g = outbox.lock().await;
                            let mut s_g = state.lock().await;
                            o_g.mark_ack_delivered(&mut s_g, c.entry_id, c.recipient_owner)
                        };
                        // ZEB-703: same as the butler Acked arm — a fresh
                        // relay ack mutated the CRDT; persist + replicate.
                        if newly {
                            if let Some(engine) = owner_sync.as_ref() {
                                engine.notify_dirty();
                            }
                        }
                        // ZEB-505: invite-only entries ack but emit no
                        // `dm-delivered` (no message to surface).
                        if newly {
                            if let Some(message_cid) = c.message_cid {
                                let payload = serde_json::json!({
                                    "spaceId": hex::encode(c.space_id.0),
                                    "messageCid": hex::encode(message_cid.to_bytes()),
                                    "recipientOwnerAddr": hex::encode(c.recipient_owner.0),
                                });
                                crate::node_event_sink::emit_ser(
                                    app.as_ref(),
                                    "dm-delivered",
                                    &payload,
                                );
                            }
                        }
                    }
                }
            }
        }
    });
}

#[derive(Debug, thiserror::Error)]
pub enum SendDmError {
    #[error("space {0:?} not found")]
    UnknownSpace(SpaceId),
    #[error("space {0:?} kind {1:?} is not Dm or GroupDm")]
    InvalidSpaceKind(SpaceId, &'static str),
    #[error("space {0:?} has no content_key (DM/group-dm invariant violated)")]
    MissingContentKey(SpaceId),
    #[error("space {0:?} has no remote recipients (members contains only self)")]
    NoRecipients(SpaceId),
    #[error("encryption failed: {0}")]
    Encrypt(#[from] DmEncryptError),
    #[error("CAS write failed: {0}")]
    Cas(#[from] ContentStoreError),
    #[error("CRDT rejected outbox entry: {0:?}")]
    CrdtRejected(RejectionReason),
    #[error("encoding failed: {0}")]
    Encode(String),
}

/// Inbound-DM packet handling errors. Each variant maps to a "drop +
/// telemetry" decision in the receive path per ZEB-216 §"Application-
/// signature binding rule". Distinct from dm_crypto::DmReceiveError
/// which only carries the SenderImpersonation case for the encrypted-
/// payload-layer check.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DmReceiveError {
    #[error("signing_device_hash not present in any OwnerDeviceCache entry")]
    UnknownSigningDevice,
    #[error("signing_device_hash claimed by multiple OwnerDeviceCache entries (corrupted state or cache-poisoning attempt)")]
    AmbiguousSigningDevice,
    #[error("no public key cached for signing_device_hash (pre-bootstrap)")]
    UnknownSigningKey,
    #[error("signature does not verify against the provided public key")]
    SignatureVerificationFailed,
    #[error("public key does not match claimed signing_device_hash (key-substitution attempt)")]
    SigningKeyDoesNotMatchDeviceHash,
    #[error("payload owner field does not match signed-origin-resolved owner")]
    OwnerFieldMismatch,
    #[error("DmInvite.inviter must be in DmInvite.members")]
    InviterNotInMembers,
    #[error("signing_device_hash must be in DmInvite.sender_devices")]
    SigningDeviceNotInSenderDevices,
    #[error("self_owner_addr must be in DmInvite.members")]
    ReceiverNotInMembers,
    /// CodeRabbit F1: the payload-controlled `DmInvite.inviter` does not match
    /// the owner the authenticated tunnel peer resolves to. A valid signer
    /// claiming another owner's `OwnerAddr` is rejected BEFORE any cache/Space
    /// mutation — this defeats cache-poisoning + spoofed-DM-Space via an
    /// inviter-field forgery on the authenticated tunnel ingest path.
    #[error("DmInvite.inviter does not match the authenticated tunnel peer's owner")]
    InviterMismatch,
    #[error("ack from owner not in OutboxEntry.recipient_owners")]
    AckFromNonRecipient,
    #[error("OutboxEntry not found for (space_id, message_cid)")]
    OutboxEntryNotFound,
    #[error("Space not found for incoming DmCidNotify (we are not a member?)")]
    SpaceNotFound,
    /// Sender's resolved owner is not in space.members. Defends against
    /// ex-members whose signing key is still cached in OwnerDeviceCache.
    #[error("sender's resolved owner is not in space.members (ex-member with cached key?)")]
    SenderNotInSpaceMembers,
    /// Space.kind is not `Dm` or `GroupDm`. `validate_invariants` guarantees
    /// `content_key.is_some()` only for Dm/GroupDm — without this gate,
    /// a forged CidNotify targeting a Channel/PublicChannel Space (which
    /// can also have members for read-access controls) would proceed
    /// through Phase B's 500ms CAS fetch and reach Phase C, which then
    /// log-drops with a generic "invariant violation" warning. The Phase A
    /// gate is the early-drop optimization (avoid CAS bandwidth) and the
    /// precise telemetry channel for this attack vector. ZEB-275.
    #[error("Space.kind is not Dm or GroupDm (forged CidNotify targeting non-DM space?)")]
    SpaceKindMismatch,
    #[error("CAS fetch failed or timed out: {0}")]
    CasFetchFailed(String),
    #[error("DM blob decryption failed under all candidate keys")]
    DecryptFailed,
    /// `space.content_key` is None for a Dm/GroupDm Space — invariant
    /// violation (`validate_invariants` forbids writing such a Space),
    /// possibly corrupted state. Previously logged inline by the direct
    /// receive handler's Phase C with this same wording; promoted to a
    /// variant when the decrypt block was extracted into
    /// `decrypt_and_bind_dm_blob` (ZEB-418 P1 Task 6).
    #[error("DM Space lacks content_key — invariant violation, possibly corrupted state")]
    MissingContentKey,
    #[error("payload sender does not match resolved owner (impersonation)")]
    SenderImpersonation,
    #[error("packet decode failed: {0}")]
    Decode(String),
    #[error("AAD compute failed: {0}")]
    AadCompute(String),
    #[error("CRDT rejected the apply (invariant violation): {0}")]
    CrdtRejected(String),
    /// ZEB-580 S2: the CidNotify signer's #2 ed25519 device key is revoked
    /// for the resolved sender-owner (shared-community revocation cutoff).
    /// Uniform check — no #2-vs-#3 branch; a legacy #3 signer's identity key
    /// is never an enrolled #2 device key, so this is a safe no-op for it.
    #[error("signer device is revoked")]
    SignerDeviceRevoked,
}

fn derive_recipients(members: &[OwnerAddr], self_addr: &OwnerAddr) -> Vec<OwnerAddr> {
    let mut set: BTreeSet<OwnerAddr> = members.iter().copied().collect();
    set.remove(self_addr);
    set.into_iter().collect() // BTreeSet → ascending lex order, deduped
}

// Mirrors `owner_state_sync.rs:452`'s `next_hlc` helper but is duplicated
// rather than re-exported because the SyncEngine's version reaches into
// its private `tracker: BTreeMap<String, Hlc>` and we don't want
// `dm_outbox` coupling to that internal. Phase 2 acceptable; Task 6
// (IPC wiring) will pass the SyncEngine's tracker entry as `prev` to
// keep production HLCs monotone with state-root publishes. (A future
// cleanup could promote this to a shared module — out of Phase 2 scope.)
pub(crate) fn next_hlc(prev: Option<&Hlc>, wall_now_ms: u64, device_id: &str) -> Hlc {
    // Same tick rule as `fleet_sync::compute_next_hlc` — this was a verbatim
    // second copy of it. Both now delegate to the core kernel (ZEB-759), so
    // the saturating-monotonicity rule has exactly one implementation.
    let tick =
        harmony_crdt_sync::HlcTick::next(prev.map(harmony_crdt_sync::HlcTick::from), wall_now_ms);
    Hlc {
        wall_ms: tick.wall_ms,
        logical: tick.logical,
        device_id: device_id.to_string(),
    }
}

/// The two tracker shapes a device can reserve an HLC against.
///
/// The channel-log family keeps a plain `BTreeMap<String, Hlc>`; the fleet
/// family keeps a `ReplayTracker`, whose peer watermarks are reachable only
/// through the apply-before-advance admit/commit pair, leaving
/// `observe_local` as the minting write (ZEB-759). Both answer the same two
/// questions, so `reserve_next_hlc_for_device` is generic over this rather
/// than duplicated per shape — and every existing call site infers.
pub trait DeviceHlcStore {
    /// The last HLC recorded for `device_id`, if any.
    fn last_for(&self, device_id: &str) -> Option<&Hlc>;
    /// Record an HLC this device just minted for itself.
    fn record_local(&mut self, device_id: &str, hlc: Hlc);
}

impl DeviceHlcStore for std::collections::BTreeMap<String, Hlc> {
    fn last_for(&self, device_id: &str) -> Option<&Hlc> {
        self.get(device_id)
    }
    fn record_local(&mut self, device_id: &str, hlc: Hlc) {
        self.insert(device_id.to_string(), hlc);
    }
}

impl DeviceHlcStore for harmony_crdt_sync::ReplayTracker<String, Hlc> {
    fn last_for(&self, device_id: &str) -> Option<&Hlc> {
        // Through `accepted()` rather than `accepted_from`: the latter takes
        // `&K` (= `&String`) and would allocate on every lookup, under the
        // tracker lock, on a path ~77 call sites reach. `BTreeMap<String, _>`
        // borrows to `str`, so this lookup is allocation-free.
        self.accepted().get(device_id)
    }
    fn record_local(&mut self, device_id: &str, hlc: Hlc) {
        debug_assert_eq!(
            device_id,
            self.local().as_str(),
            "reserve_next_hlc_for_device mints only for the local device; a peer's \
             watermark must go through admit/commit so apply-before-advance holds"
        );
        // Monotone rather than unconditional, unlike the map impl. Equivalent
        // here: `next_hlc` never returns something older than `prev`, and in
        // the one case where it returns something EQUAL (logical saturated at
        // u32::MAX) both an overwrite and a rejected observe leave the same
        // stored value.
        self.observe_local(hlc);
    }
}

/// Atomically reserve the next HLC for a device.
///
/// Acquires `tracker`, reads the device's last-known HLC, computes
/// the successor via `next_hlc`, writes it back, and returns it —
/// all under a single lock acquisition. Replaces the
/// snapshot-then-release pattern at all power-gated community-event
/// IPCs (kick / leave / set_power / channel_* / redeem /
/// create_community).
///
/// Tracker is bumped at reservation time, regardless of whether the
/// caller's downstream `engine.insert_local_event` succeeds. A
/// rejected insert "burns" the reserved HLC — fine, since HLCs are
/// 64-bit logical and burning is already implicit on signature- or
/// verify-failure paths today.
///
/// ZEB-267 — replaces the snapshot-then-release pattern that had a
/// race window between the `prev_hlc` read and the post-`Inserted`
/// advance. See `docs/specs/2026-05-09-zeb-267-atomic-hlc-reservation-design.md`.
///
/// ZEB-790: `floor` bounds the mint to at most `HLC_ADOPT_FORWARD_CAP_MS`
/// ahead of `wall_now_ms`, adopting the highest verified remote wall this
/// session has observed (via `HlcAdoptFloor::observe`). An empty floor is
/// the identity — `floor.merged_now(wall_now_ms) == wall_now_ms` — so this
/// is a strict superset of pre-ZEB-790 behavior.
pub async fn reserve_next_hlc_for_device<T: DeviceHlcStore>(
    tracker: &std::sync::Arc<tokio::sync::Mutex<T>>,
    floor: &crate::hlc_adopt_floor::HlcAdoptFloor,
    device_id: &str,
    wall_now_ms: u64,
) -> Hlc {
    // ZEB-790: bounded causal adoption — the floor read is a lock-free
    // atomic, so the ZEB-267 single-lock atomicity is unchanged.
    let wall_now_ms = floor.merged_now(wall_now_ms);
    let mut t = tracker.lock().await;
    let prev = t.last_for(device_id).cloned();
    let next = next_hlc(prev.as_ref(), wall_now_ms, device_id);
    t.record_local(device_id, next.clone());
    next
}

/// Resolve a verified signing device → owner. MUST match exactly one OwnerAddr.
///
/// Pre-condition: the caller has already verified the signature against
/// the public key for `signing_device_hash`. This function only does the
/// device-hash → OwnerAddr lookup, not signature verification.
///
/// Returns Err on zero matches (UnknownSigningDevice) or multiple matches
/// (AmbiguousSigningDevice). Multi-match is reachable via corrupted state
/// or a malicious cache-poisoning DmInvite that claimed an existing device
/// hash for a different owner; either way the resolution is not trustworthy
/// — drop + telemetry.
///
/// Uses `binary_search` on `OwnerDeviceEntry::devices`, which is sorted-
/// ascending-lex per its existing invariant (re-established by the
/// struct-level `Deserialize` impl on `OwnerDeviceEntry` on every load —
/// jointly with the parallel `device_identity_pubs` vec — see
/// `owner_state_types.rs:286-307`).
// Task 9 (handle_invite) does NOT call this — invites carry the inviter
// inline so resolution is unnecessary. The receive-path admission checks
// (`verify_cidnotify_admission`, driven by `dm_inbox_ingest`) and
// Task 11 (handle_ack) are the consumers.
pub(crate) fn resolve_signed_origin_owner(
    cache: &OwnerDeviceCache,
    signing_device_hash: DeviceIdentityHash,
) -> Result<OwnerAddr, DmReceiveError> {
    let matches: Vec<OwnerAddr> = cache
        .devices
        .iter()
        .filter(|(_, entry)| entry.devices.binary_search(&signing_device_hash).is_ok())
        .map(|(addr, _)| *addr)
        .collect();
    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(DmReceiveError::UnknownSigningDevice),
        _ => Err(DmReceiveError::AmbiguousSigningDevice),
    }
}

/// Look up the cached 64-byte combined identity pubs for a known device.
/// Reads from `OwnerDeviceCache` via the parallel-vec correspondence
/// between `devices[i]` and `device_identity_pubs[i]` (Task 4).
///
/// Returns `Some(identity_pub_bytes)` only if the device hash is in the
/// cache AND the cache has a `Some(pub)` at the corresponding index.
/// Returns `None` for any of: device unknown, or device known but
/// `device_identity_pubs[i] == None` (pre-bootstrap state — handler
/// treats as `UnknownSigningKey`).
///
/// Returns the full 64-byte combined pub (X25519 || Ed25519); the caller
/// passes this to `dm_signing::verify_dm_packet_signature`, which splits
/// out the Ed25519 half internally. We must return the full 64 bytes
/// (not just Ed25519) so `verify_dm_packet_signature` can re-derive the
/// `signing_device_hash` and confirm the cached pub actually maps to the
/// hash the body claims (key-substitution defense).
pub(crate) fn lookup_pubkey_for_device(
    cache: &OwnerDeviceCache,
    signing_device_hash: DeviceIdentityHash,
) -> Option<[u8; 64]> {
    for entry in cache.devices.values() {
        if let Ok(idx) = entry.devices.binary_search(&signing_device_hash) {
            if idx < entry.device_identity_pubs.len() {
                // device_identity_pubs[idx] is Option<[u8; 64]>;
                // Some → return; None → fall through, no pub cached.
                return entry.device_identity_pubs[idx];
            }
            return None; // device present but pubs vec shorter than expected
        }
    }
    None
}

/// Phase A admission checks for an inbound DmCidNotify (ZEB-418 P1 Task 6;
/// originally extracted from the since-deleted direct receive handler,
/// ZEB-710). Every live CidNotify consumer — `dm_inbox_ingest` and the
/// community-relay recover path — runs deposited packets through EXACTLY
/// this one verification instead of a parallel re-implementation.
///
/// Checks, in the original inline order:
///   1. signing-device pubkey lookup (`UnknownSigningKey` when absent);
///   2. packet signature verification (includes the key-substitution
///      defense inside `verify_dm_packet_signature`);
///   3. signing device → owner resolution (`UnknownSigningDevice` /
///      `AmbiguousSigningDevice`);
///   4. `signed.sender_owner_addr` must equal the resolved owner
///      (`OwnerFieldMismatch`, cache-poisoning defense);
///   5. Space lookup by `signed.space_id` (`SpaceNotFound`);
///   6. SpaceKind gate: `Dm | GroupDm` only (`SpaceKindMismatch`) —
///      ZEB-275: gate BEFORE any decrypt-path work. Defends against
///      forged CidNotifys targeting Channel / PublicChannel spaces (which
///      can also have members for read-access controls).
///      `validate_invariants` guarantees `content_key.is_some()` only for
///      Dm/GroupDm — without this gate, the handler proceeds into Phase
///      B's 500ms CAS fetch and Phase C, which then log-drops on
///      `content_key=None` with a generic "invariant violation" warning.
///      The gate fires earlier (cheap path), avoids wasted CAS bandwidth,
///      and emits the precise `SpaceKindMismatch` telemetry. Also
///      defense-in-depth against a future code change that might
///      reintroduce a panic in the decrypt path;
///   7. sender membership in `space.members` (`SenderNotInSpaceMembers`).
///
/// Returns the cloned Space, the resolved owner, and the signer's cached
/// 64-byte identity pub (the normal path feeds it to
/// `apply_owner_device_update`).
pub(crate) fn verify_cidnotify_admission(
    state: &OwnerState,
    signed: &crate::dm_envelope::DmCidNotifySigned,
    signature: &[u8; 64],
    signed_bytes: &[u8],
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
) -> Result<(crate::owner_state_types::Space, OwnerAddr, [u8; 64]), DmReceiveError> {
    let (resolved_owner, identity_pub) =
        verify_cidnotify_sender_binding(state, signed, signature, signed_bytes, revoked)?;
    let space = verify_cidnotify_space(state, signed, resolved_owner)?;
    Ok((space, resolved_owner, identity_pub))
}

/// Sender-binding prefix of [`verify_cidnotify_admission`] (steps 1–4): resolve
/// and cryptographically authenticate the CidNotify's signer against the
/// `OwnerDeviceCache` **as it currently exists**, returning the tuple
/// `(resolved_owner, identity_pub)`.
///
/// SECURITY (ZEB-483, CodeRabbit): the deposit-recover path MUST call this
/// against the PRISTINE cache — BEFORE any deposited `DmInvite` mutates trust
/// state. `apply_invite` seeds the `device → owner → identity_pub` rows that
/// `lookup_pubkey_for_device` / `resolve_signed_origin_owner` read here; if the
/// invite ran first, a forged CidNotify would "verify" against cache the
/// untrusted invite just wrote (circular trust — an attacker seeds their own
/// device under a victim owner, then the notify resolves to the victim). On the
/// legitimate offline-DM path the sender is an existing friend whose devices are
/// already cached, so this resolves without help from the invite; the invite is
/// then only permitted to bootstrap the missing Space, never device trust.
pub(crate) fn verify_cidnotify_sender_binding(
    state: &OwnerState,
    signed: &crate::dm_envelope::DmCidNotifySigned,
    signature: &[u8; 64],
    signed_bytes: &[u8],
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
) -> Result<(OwnerAddr, [u8; 64]), DmReceiveError> {
    let identity_pub =
        lookup_pubkey_for_device(&state.owner_device_cache, signed.signing_device_hash)
            .ok_or(DmReceiveError::UnknownSigningKey)?;
    crate::dm_signing::verify_dm_packet_signature(
        signed_bytes,
        signature,
        &identity_pub,
        signed.signing_device_hash,
    )?;
    let resolved_owner =
        resolve_signed_origin_owner(&state.owner_device_cache, signed.signing_device_hash)?;
    if signed.sender_owner_addr != resolved_owner {
        return Err(DmReceiveError::OwnerFieldMismatch);
    }
    // ZEB-580 S2: shared-community revocation cutoff. Drop if the signer's #2
    // ed25519 (combined_pub[32..64]) is revoked for the resolved owner. No-op
    // for legacy #3 signers (a #3 key is never an enrolled device key).
    let ed25519: [u8; 32] = identity_pub[32..64].try_into().expect("64 - 32 == 32");
    if revoked.is_revoked(&resolved_owner, &ed25519) {
        return Err(DmReceiveError::SignerDeviceRevoked);
    }
    Ok((resolved_owner, identity_pub))
}

/// Space-binding suffix of [`verify_cidnotify_admission`] (steps 5–7): look up
/// the target Space, gate its kind (`Dm | GroupDm`), and confirm the
/// already-resolved sender is a member. Split out so the deposit-recover path
/// can bootstrap the Space from a deposited invite BETWEEN sender-binding and
/// this check (ZEB-483) without re-reading the cache.
pub(crate) fn verify_cidnotify_space(
    state: &OwnerState,
    signed: &crate::dm_envelope::DmCidNotifySigned,
    resolved_owner: OwnerAddr,
) -> Result<crate::owner_state_types::Space, DmReceiveError> {
    let space = state
        .spaces
        .get(&signed.space_id)
        .cloned()
        .ok_or(DmReceiveError::SpaceNotFound)?;
    if !matches!(space.kind, SpaceKind::Dm | SpaceKind::GroupDm) {
        return Err(DmReceiveError::SpaceKindMismatch);
    }
    if !space.members.contains(&resolved_owner) {
        return Err(DmReceiveError::SenderNotInSpaceMembers);
    }
    Ok(space)
}

/// ZEB-214: verify an inbound read-receipt frame against the CURRENT
/// OwnerDeviceCache — mirrors `verify_cidnotify_admission` (sender-binding +
/// space-binding) for `DmReadReceiptSigned`. Returns the resolved sender owner.
/// No CAS/blob steps: a receipt carries no message body.
pub(crate) fn verify_read_receipt_admission(
    state: &OwnerState,
    signed: &crate::dm_envelope::DmReadReceiptSigned,
    signature: &[u8; 64],
    signed_bytes: &[u8],
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
) -> Result<OwnerAddr, DmReceiveError> {
    let identity_pub =
        lookup_pubkey_for_device(&state.owner_device_cache, signed.signing_device_hash)
            .ok_or(DmReceiveError::UnknownSigningKey)?;
    crate::dm_signing::verify_dm_packet_signature(
        signed_bytes,
        signature,
        &identity_pub,
        signed.signing_device_hash,
    )?;
    let resolved_owner =
        resolve_signed_origin_owner(&state.owner_device_cache, signed.signing_device_hash)?;
    if signed.sender_owner_addr != resolved_owner {
        return Err(DmReceiveError::OwnerFieldMismatch);
    }
    let ed25519: [u8; 32] = identity_pub[32..64].try_into().expect("64 - 32 == 32");
    if revoked.is_revoked(&resolved_owner, &ed25519) {
        return Err(DmReceiveError::SignerDeviceRevoked);
    }
    let space = state
        .spaces
        .get(&signed.space_id)
        .ok_or(DmReceiveError::SpaceNotFound)?;
    if !matches!(space.kind, SpaceKind::Dm | SpaceKind::GroupDm) {
        return Err(DmReceiveError::SpaceKindMismatch);
    }
    if !space.members.contains(&resolved_owner) {
        return Err(DmReceiveError::SenderNotInSpaceMembers);
    }
    Ok(resolved_owner)
}

/// Phase C decrypt + sender binding for an inbound DM storage blob
/// (ZEB-418 P1 Task 6; originally extracted from the since-deleted direct
/// receive handler, ZEB-710). Shared by every live receive path via
/// `dm_inbox_ingest`.
///
/// * AAD is bound to the Space's `dedupe_key` (stable across cross-SpaceId
///   dedupe collapses).
/// * `space.content_key` is non-None for any DM/group-DM Space that passed
///   `validate_invariants` — the invariant check in
///   `apply_space_with_canonicalization` (which wrote the Space into
///   state) rejects DM/group-DM Spaces with `content_key=None`. Reachable
///   only via corrupted state, a migration bug, or direct test insertion
///   that bypasses `validate_invariants`; the failure mode is a graceful
///   `MissingContentKey` reject (never a panic — in the spawned-task path
///   a panic would die silently).
/// * Decryption tries the current `content_key` then each
///   `prior_content_keys` entry in stored order, so a rotation landing
///   between Phase A and Phase C (or between deposit and ingestion) still
///   decrypts.
/// * The decrypted payload's `sender` must equal `resolved_owner`
///   (`SenderImpersonation` defense).
pub(crate) fn decrypt_and_bind_dm_blob(
    space: &crate::owner_state_types::Space,
    blob: &[u8],
    resolved_owner: OwnerAddr,
) -> Result<MessagePayload, DmReceiveError> {
    let aad = compute_aad(space).map_err(|e| DmReceiveError::AadCompute(e.to_string()))?;
    let content_key = space
        .content_key
        .as_ref()
        .ok_or(DmReceiveError::MissingContentKey)?;
    let payload =
        crate::dm_crypto::decrypt_dm_message(content_key, &space.prior_content_keys, &aad, blob)
            .map_err(|_| DmReceiveError::DecryptFailed)?;
    crate::dm_crypto::verify_sender_binding(&payload, resolved_owner)
        .map_err(|_| DmReceiveError::SenderImpersonation)?;
    Ok(payload)
}

/// The `dm-received` UI event name, shared by the normal receive path and
/// butler dm-inbox ingestion (ZEB-418 P1 Task 6) so both deliver through
/// one event the frontend already listens for.
pub(crate) const DM_RECEIVED_EVENT: &str = "dm-received";

/// Builds the `dm-received` event payload — the single source of truth for
/// its shape, shared by every receive path (`dm_inbox_ingest`, relay
/// recover) so the frontend cannot observe which path delivered a
/// message.
pub(crate) fn dm_received_event_payload(
    rm: &crate::owner_state_types::ReceivedMessage,
) -> serde_json::Value {
    serde_json::json!({
        "spaceId": hex::encode(rm.inbox_entry.space_id.0),
        "messageCid": hex::encode(rm.inbox_entry.message_cid.to_bytes()),
        "from": hex::encode(rm.inbox_entry.from.0),
        "receivedAt": rm.inbox_entry.received_at.wall_ms,
        "sentAt": rm.sent_at.wall_ms,
        "body": hex::encode(&rm.body),
        "mimeType": rm.mime_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_store::InMemoryStub;
    use crate::owner_state_types::{ContentId, DmContentKey, InboxEntry, Space};

    // ── ZEB-685 (S3): handle_revocation_push ─────────────────────────────────

    struct RevCase {
        owner: OwnerAddr,
        master_sk: ed25519_dalek::SigningKey,
        master_bundle: harmony_owner::pubkey_bundle::PubKeyBundle,
        revocation: harmony_owner::certs::RevocationCert,
        enrollment: harmony_owner::certs::EnrollmentCert,
        revoked_ed: [u8; 32],
    }

    /// A self-contained master-signed revocation scenario (mirrors
    /// `mint_owner`'s internals): a master, a device it enrolled, and a
    /// master-signed `RevocationCert` for that device.
    fn sample_revocation_case() -> RevCase {
        use ed25519_dalek::SigningKey;
        use harmony_owner::certs::{EnrollmentCert, RevocationCert, RevocationReason};
        use harmony_owner::pubkey_bundle::PubKeyBundle;

        let master_sk = SigningKey::from_bytes(&[0x11; 32]);
        let master_bundle = PubKeyBundle::classical_only(master_sk.verifying_key().to_bytes());
        let owner = OwnerAddr(master_bundle.identity_hash());

        let device_sk = SigningKey::from_bytes(&[0x22; 32]);
        let device_bundle = PubKeyBundle::classical_only(device_sk.verifying_key().to_bytes());
        let device_id = device_bundle.identity_hash();
        let revoked_ed = device_bundle.classical.ed25519_verify;

        let now = 1_700_000_000u64;
        let enrollment = EnrollmentCert::sign_master(
            &master_sk,
            master_bundle.clone(),
            device_id,
            device_bundle,
            now,
            None,
        )
        .expect("enrollment sign");
        let revocation = RevocationCert::sign_master(
            &master_sk,
            master_bundle.clone(),
            device_id,
            now,
            RevocationReason::Compromised,
        )
        .expect("revocation sign");
        RevCase {
            owner,
            master_sk,
            master_bundle,
            revocation,
            enrollment,
            revoked_ed,
        }
    }

    #[test]
    fn verify_revocation_push_accepts_valid_and_rejects_tampered() {
        let case = sample_revocation_case(); // existing test helper (RevCase)
        let ed = verify_revocation_push(case.owner, &case.revocation, &case.enrollment)
            .expect("valid pair verifies");
        assert_eq!(ed, case.revoked_ed);
        // Third-party owner (expected != revocation.owner) → OwnerFieldMismatch.
        let other = crate::owner_state_types::OwnerAddr([0xEE; 16]);
        assert!(matches!(
            verify_revocation_push(other, &case.revocation, &case.enrollment),
            Err(DmReceiveError::OwnerFieldMismatch)
        ));
    }

    #[test]
    fn handle_revocation_push_accepts_and_feeds_projection() {
        let c = sample_revocation_case();
        let mut state = OwnerState::default();
        let proj = crate::revoked_device_projection::RevokedDeviceProjection::new();
        assert!(!proj.is_revoked(&c.owner, &c.revoked_ed));
        let inserted =
            handle_revocation_push(&mut state, c.owner, &c.revocation, &c.enrollment, &proj)
                .expect("valid master-signed push accepted");
        assert!(
            inserted,
            "fresh revocation reports a new insert (drives notify_dirty)"
        );
        assert!(proj.is_revoked(&c.owner, &c.revoked_ed), "projection fed");
        assert!(
            state
                .revoked_dm_devices
                .get(&c.owner)
                .unwrap()
                .contains(&c.revoked_ed),
            "CRDT stored"
        );
        // Idempotent re-apply — still exactly one entry, and reports no new insert
        // (so the caller does NOT spuriously mark the engine dirty).
        let reinserted =
            handle_revocation_push(&mut state, c.owner, &c.revocation, &c.enrollment, &proj)
                .expect("idempotent re-apply");
        assert!(!reinserted, "idempotent re-apply reports no new insert");
        assert_eq!(state.revoked_dm_devices.get(&c.owner).unwrap().len(), 1);
    }

    /// ZEB-699: pins the 256-cap eviction edge as INTENTIONAL fail-closed
    /// defense. When the pushed key is the byte-order max of an at-cap store,
    /// `apply_revoked_dm_device` evicts it back out (ZEB-692 keeps the
    /// smallest-256) — but the revocation was cryptographically verified, so
    /// the live projection must still learn it (enforcement over durability;
    /// projection ⊇ store, transient, self-heals on restart). The `false`
    /// return pins that the caller does NOT `notify_dirty`: the store is
    /// unchanged, so there is nothing to persist.
    #[test]
    fn handle_revocation_push_feeds_projection_even_when_store_evicts_at_cap() {
        let c = sample_revocation_case();
        let mut state = OwnerState::default();
        let proj = crate::revoked_device_projection::RevokedDeviceProjection::new();

        // Pre-fill the owner's store to exactly the cap with keys that are all
        // byte-order-smaller than the real revoked ed25519 key (30 leading zero
        // bytes; a real ed25519 point encoding never starts with 30 zero
        // bytes — asserted below against this test's FIXED seed, so the
        // precondition is deterministic, not probabilistic).
        let filler_keys: Vec<[u8; 32]> = (0
            ..crate::owner_state_crdt::MAX_REVOKED_DM_DEVICES_PER_OWNER)
            .map(|i| {
                let mut k = [0u8; 32];
                k[30] = (i / 256) as u8;
                k[31] = (i % 256) as u8;
                k
            })
            .collect();
        assert!(
            filler_keys.iter().all(|k| *k < c.revoked_ed),
            "precondition: every filler key sorts below the pushed key"
        );
        for k in &filler_keys {
            assert!(state.apply_revoked_dm_device(c.owner, *k));
        }
        assert_eq!(
            state.revoked_dm_devices.get(&c.owner).unwrap().len(),
            crate::owner_state_crdt::MAX_REVOKED_DM_DEVICES_PER_OWNER,
            "store at cap before the push"
        );

        let inserted =
            handle_revocation_push(&mut state, c.owner, &c.revocation, &c.enrollment, &proj)
                .expect("valid master-signed push accepted at the cap edge");

        // Store: unchanged — the pushed key was the max, evicted by the cap.
        assert!(!inserted, "evicted-at-cap insert reports no net change");
        let stored = state.revoked_dm_devices.get(&c.owner).unwrap();
        assert_eq!(
            stored.len(),
            crate::owner_state_crdt::MAX_REVOKED_DM_DEVICES_PER_OWNER
        );
        assert!(
            !stored.contains(&c.revoked_ed),
            "the capped store evicted the byte-order-max pushed key"
        );
        // Projection: fed anyway — the verified revocation is enforced live.
        assert!(
            proj.is_revoked(&c.owner, &c.revoked_ed),
            "projection must learn the verified key even when the store evicts it"
        );
    }

    #[test]
    fn handle_revocation_push_rejects_third_party_owner() {
        // The trust-bind: a friend may only revoke THEIR OWN devices. A valid
        // master-signed revocation whose owner != the pushing peer is rejected
        // (no relaying a third party's revocation into our projection).
        let c = sample_revocation_case();
        let wrong = OwnerAddr([0xEE; 16]);
        let mut state = OwnerState::default();
        let proj = crate::revoked_device_projection::RevokedDeviceProjection::new();
        let err = handle_revocation_push(&mut state, wrong, &c.revocation, &c.enrollment, &proj);
        assert!(
            matches!(err, Err(DmReceiveError::OwnerFieldMismatch)),
            "third-party owner must be rejected, got {err:?}"
        );
        assert!(
            state.revoked_dm_devices.is_empty(),
            "nothing stored on reject"
        );
    }

    #[test]
    fn handle_revocation_push_rejects_target_enrollment_mismatch() {
        use harmony_owner::certs::EnrollmentCert;
        use harmony_owner::pubkey_bundle::PubKeyBundle;
        // A valid enrollment for a DIFFERENT device under the SAME master: its
        // device_id != revocation.target, so the bridge binding must reject
        // (else a friend could cut off the wrong device via a mismatched pair).
        let c = sample_revocation_case();
        let other = PubKeyBundle::classical_only(
            ed25519_dalek::SigningKey::from_bytes(&[0x33; 32])
                .verifying_key()
                .to_bytes(),
        );
        let other_enrollment = EnrollmentCert::sign_master(
            &c.master_sk,
            c.master_bundle.clone(),
            other.identity_hash(),
            other,
            1_700_000_000,
            None,
        )
        .expect("sign other enrollment");
        let mut state = OwnerState::default();
        let proj = crate::revoked_device_projection::RevokedDeviceProjection::new();
        let err =
            handle_revocation_push(&mut state, c.owner, &c.revocation, &other_enrollment, &proj);
        assert!(
            matches!(err, Err(DmReceiveError::OwnerFieldMismatch)),
            "target/enrollment mismatch must be rejected, got {err:?}"
        );
    }

    #[test]
    fn handle_revocation_push_rejects_self_issued() {
        use harmony_owner::certs::{RevocationCert, RevocationReason};
        // A SelfDevice-issued revocation (a device revoking itself) is not a
        // master attestation — §3.3 accepts only Master-issued (design line 60).
        // `verify(None)` hits the `(SelfDevice, None) => InvalidSignature` arm.
        let c = sample_revocation_case();
        let device_sk = ed25519_dalek::SigningKey::from_bytes(&[0x22; 32]);
        let self_rev = RevocationCert::sign_self(
            &device_sk,
            c.owner.0,
            c.enrollment.device_id,
            1_700_000_000,
            RevocationReason::Decommissioned,
        )
        .expect("sign_self");
        let mut state = OwnerState::default();
        let proj = crate::revoked_device_projection::RevokedDeviceProjection::new();
        let err = handle_revocation_push(&mut state, c.owner, &self_rev, &c.enrollment, &proj);
        assert!(
            matches!(err, Err(DmReceiveError::SignatureVerificationFailed)),
            "SelfDevice-issued push must be rejected, got {err:?}"
        );
        assert!(
            state.revoked_dm_devices.is_empty(),
            "nothing stored on reject"
        );
    }

    /// Test-only helper: build a `DmOutbox` with synthetic materials for
    /// tests that don't exercise community-signing paths. Routes through
    /// `DmOutbox::new_synthetic` (which bypasses `DmOutbox::new`'s `assert!`
    /// checks) so that tests which need a specific `self_owner` for DM space
    /// membership or ack-address assertions can supply an arbitrary address
    /// without needing a matching `EnrollmentCert`.
    ///
    /// Tests that DO exercise community-signing invariants (cert verify,
    /// cert↔key binding, cert↔owner binding) should call `DmOutbox::new`
    /// directly with consistent `mint_test_owner` material — the
    /// `dm_outbox_community_signing_key_and_enrollment_cert` test does this.
    fn make_outbox_synthetic(device_id: &str, self_owner: OwnerAddr) -> DmOutbox {
        // Derive all three identity-bound fields (signing_key,
        // private_identity, DeviceIdentityHash) from a single
        // PrivateIdentity seed so the synthetic outbox can never
        // silently violate the same-identity invariant
        // `dm_outbox_holds_private_identity_for_countersign` enforces
        // for production callers. Even though current tests using this
        // helper don't exercise the cross-binding paths, future tests
        // that do would otherwise see Ed25519 sig/verify mismatches
        // with no clear symptom.
        let private_identity = harmony_identity::PrivateIdentity::from_seed(&[0x55; 32]);
        let priv_bytes = private_identity.to_private_bytes();
        let mut ed_seed = [0u8; 32];
        ed_seed.copy_from_slice(&priv_bytes[32..64]);
        let signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&ed_seed));
        let device_hash = DeviceIdentityHash(private_identity.identity.address_hash);
        let private_identity = std::sync::Arc::new(private_identity);
        // ZEB-339: synthetic community_signing_key + enrollment_cert for tests
        // that don't exercise community-signing paths. Uses a fixed seed so
        // the helper stays deterministic. The cert's owner_id does NOT match
        // self_owner (the assert in `DmOutbox::new` is bypassed via
        // `DmOutbox::new_synthetic`; production callers must use `new()`).
        let test_owner = crate::community_membership::mint_test_owner(0xAB);
        let community_signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(
            &test_owner.device_key.to_bytes(),
        ));
        let enrollment_cert = test_owner.cert;
        // Use the test-only `new_synthetic` constructor (which bypasses
        // DmOutbox::new's enrollment asserts by design) rather than
        // open-coding the struct — keeps the synthetic setup in one place.
        DmOutbox::new_synthetic(
            device_id.into(),
            self_owner,
            device_hash,
            signing_key,
            private_identity,
            community_signing_key,
            enrollment_cert,
        )
    }

    /// ZEB-236 test helper: mark `inviter` an ACTIVE friend so `apply_invite`'s
    /// tier fork takes the auto-accept branch. Uses a DIRECT map insert (not the
    /// validated `apply_friend_update` route): that route re-derives `owner_id`
    /// from `master_ed25519` and rejects any entry whose map key ≠ that
    /// derivation, and these synthetic fixture inviters are arbitrary
    /// `OwnerAddr`s not derived from a real master key. The tier fork reads only
    /// `friend_graph.friends[inviter].status`, which a direct insert satisfies.
    fn insert_active_friend(state: &mut OwnerState, inviter: OwnerAddr) {
        state.friend_graph.friends.insert(
            inviter,
            crate::friend_graph::FriendEntry {
                master_ed25519: [0u8; 32],
                display: None,
                status: crate::friend_graph::FriendStatus::Active,
                established_via: crate::friend_graph::FriendOrigin::Token,
                referrable: false,
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "test-friend".into(),
                },
                sealed_secret: None,
            },
        );
    }

    /// ZEB-236 test helper: a signature-valid 2-member `Dm` invite whose inviter
    /// is `OwnerAddr([1; 16])` and whose other member is `self_owner`. Mirrors
    /// the inline builder in `handle_invite_writes_space_and_cache_with_signing_pub`
    /// (same fixed `[0x42; 32]` identity seed), so the invite verifies and two
    /// calls produce byte-identical invites — a precondition for the golden
    /// parity test. Returns `(signed, signature, signed_bytes)`.
    fn build_valid_dm_invite(
        self_owner: OwnerAddr,
    ) -> (crate::dm_envelope::DmInviteSigned, [u8; 64], Vec<u8>) {
        let private = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);

        let signed = crate::dm_envelope::DmInviteSigned {
            space_id: SpaceId([7; 16]),
            kind: SpaceKind::Dm,
            members: vec![OwnerAddr([1; 16]), self_owner],
            inviter: OwnerAddr([1; 16]),
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![device_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice".into(),
            },
            signing_device_hash: device_hash,
            inviter_identity_pub: identity_pub,
            inviter_enrollment: None,
        };
        let body_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&body_bytes);
        (signed, signature, body_bytes)
    }

    /// ZEB-639 test helper: pre-populate `state` with the Space `signed`
    /// targets — the exact shape `run_invite_accept_tail` builds, so the
    /// "space already exists locally" arrangement matches what a genuine
    /// prior accept would have written.
    fn insert_space_from_invite(
        state: &mut OwnerState,
        signed: &crate::dm_envelope::DmInviteSigned,
    ) {
        let space = crate::owner_state_types::Space {
            id: signed.space_id,
            kind: signed.kind,
            parent: None,
            community_id: None,
            name: format!("DM with {}", hex::encode(signed.inviter.0)),
            transport: None,
            members: signed.members.clone(),
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: signed.created_at.clone(),
            updated_at: signed.created_at.clone(),
            content_key: Some(signed.content_key.clone()),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            read_receipt_pref: None,
            pending_join_at: None,
        };
        assert!(
            matches!(
                state.apply_space_with_canonicalization(space),
                crate::owner_state_crdt::ApplyOutcome::Inserted
            ),
            "fixture Space must insert cleanly"
        );
    }

    /// ZEB-639 (2) test helper: `build_valid_dm_invite` with the `created_at`
    /// HLC forged to a far-future wall clock (`u64::MAX / 2`), re-signed with
    /// the same deterministic `[0x42; 32]` identity so the invite still passes
    /// `apply_invite`'s signature gate — `created_at` is inside the signed
    /// body, so the attack is a fully valid-looking invite, not a corrupt one.
    fn build_far_future_dm_invite(
        self_owner: OwnerAddr,
    ) -> (crate::dm_envelope::DmInviteSigned, [u8; 64], Vec<u8>) {
        let (mut signed, _sig, _bytes) = build_valid_dm_invite(self_owner);
        signed.created_at = Hlc {
            wall_ms: u64::MAX / 2,
            logical: 0,
            device_id: "alice".into(),
        };
        let private = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let body_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&body_bytes);
        (signed, signature, body_bytes)
    }

    /// ZEB-580 S1 (Task 3) test helper: the canonical CBOR encoding of a signed
    /// invite body — the exact bytes `apply_invite` verifies the signature over.
    fn canonical(signed: &crate::dm_envelope::DmInviteSigned) -> Vec<u8> {
        crate::owner_state_crypto::canonical_cbor_encode(signed).unwrap()
    }

    /// ZEB-580 S1 (Task 3) test helper: a #2-cert-carrying `DmInviteSigned`
    /// (UNSIGNED) whose signing device is the one enrolled by `cert` — i.e.
    /// `signing_device_hash` is the cert's #2 DM hash, `sender_devices` is that
    /// singleton, and `inviter_identity_pub` is the cert's #2 combined pub (the
    /// shape Task 5 emits on the send side). Marks `inviter` an ACTIVE friend so
    /// a *valid* invite reaches the accept tail. Callers sign the returned body
    /// with the enrolled device's #2 key (and may mutate exactly one field first
    /// to drive a reject path). `inviter` is passed separately from the cert so
    /// the owner-mismatch case can wire a cert whose `owner_id != inviter`.
    fn build_dm_invite_signed_with_cert(
        state: &mut OwnerState,
        self_owner: OwnerAddr,
        inviter: OwnerAddr,
        cert: harmony_owner::certs::EnrollmentCert,
    ) -> crate::dm_envelope::DmInviteSigned {
        insert_active_friend(state, inviter);
        let device2_pub = crate::dm_signing::device2_combined_pub(&cert);
        let device2_hash =
            crate::dm_signing::device2_signing_hash(&cert).expect("minted cert yields a #2 hash");
        // mint_owner draws a random owner_id, so `inviter` may sort either side
        // of `self_owner` — the DM Space canonical-CBOR invariant requires
        // members sorted ascending, so sort here for determinism (order-agnostic
        // for the `.contains()` sanity gates; the body is signed AFTER this).
        let mut members = vec![inviter, self_owner];
        members.sort_by(|a, b| a.0.cmp(&b.0));
        crate::dm_envelope::DmInviteSigned {
            space_id: SpaceId([7; 16]),
            kind: SpaceKind::Dm,
            members,
            inviter,
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![device2_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "inviter".into(),
            },
            signing_device_hash: device2_hash,
            inviter_identity_pub: device2_pub,
            inviter_enrollment: Some(Box::new(cert)),
        }
    }

    /// ZEB-262 Phase 4 Task 2: assert that `DmOutbox.signing_key` and
    /// `DmOutbox.private_identity` are derived from the same identity
    /// material — they MUST produce identical Ed25519 signatures over the
    /// same bytes. A misplumbed field (e.g. signing_key from identity A
    /// paired with private_identity from identity B) would silently break
    /// receive-side counter-signing in `handle_invite` without surfacing
    /// any obvious symptom; this test fails loud at construction time.
    ///
    /// Uses `PrivateIdentity::from_private_bytes` round-trip rather than
    /// `clone()` because `PrivateIdentity` deliberately does NOT implement
    /// `Clone` (it carries `ZeroizeOnDrop` to discourage accidental copies
    /// of secret material). Both the `Arc<PrivateIdentity>` stored in the
    /// outbox AND the local `signing_key` derived for comparison are built
    /// from the same 64-byte private-bytes blob, so they MUST sign
    /// identically.
    #[test]
    fn dm_outbox_holds_private_identity_for_countersign() {
        use ed25519_dalek::Signer;
        use harmony_identity::PrivateIdentity;

        let identity = PrivateIdentity::from_seed(&[0xc7; 32]);
        let priv_bytes = identity.to_private_bytes();
        // Round-trip a second instance from the same private bytes — gives
        // us an Arc<PrivateIdentity> while leaving `identity` available for
        // seed extraction below.
        let private_identity = std::sync::Arc::new(
            PrivateIdentity::from_private_bytes(&priv_bytes).expect("private bytes round-trip"),
        );
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&priv_bytes[32..64]);
        let signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&seed));
        let self_owner = OwnerAddr(identity.identity.address_hash);
        // Derive the DeviceIdentityHash from the SAME identity that
        // `signing_key` and `private_identity` came from, then assert
        // it matches what the outbox surfaces. This pins the full
        // 3-way invariant — without binding the device hash to the
        // identity, a misplumbed call site could pair a mismatched
        // hash with a correct sig pair and the receive-side
        // signing_device_hash check would fail at decode time on
        // production packets.
        let device_hash = DeviceIdentityHash(identity.identity.address_hash);

        // ZEB-339: supply synthetic community_signing_key + enrollment_cert;
        // this test only exercises the Reticulum signing_key / private_identity
        // binding, not the community cert binding. Use new_synthetic to bypass
        // the cert.owner_id assert (self_owner here is Reticulum-derived,
        // not from a harmony-owner master key).
        let test_owner_for_countersign = crate::community_membership::mint_test_owner(0xCC);
        let community_signing_key_for_countersign =
            std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(
                &test_owner_for_countersign.device_key.to_bytes(),
            ));
        let enrollment_cert_for_countersign = test_owner_for_countersign.cert;

        let outbox = DmOutbox::new_synthetic(
            "dev".into(),
            self_owner,
            device_hash,
            std::sync::Arc::clone(&signing_key),
            std::sync::Arc::clone(&private_identity),
            community_signing_key_for_countersign,
            enrollment_cert_for_countersign,
        );

        let msg = b"countersig harness";
        let sig_via_outbox_signing_key = outbox.signing_key.sign(msg).to_bytes();
        let sig_via_private_identity = outbox.private_identity.sign(msg);
        assert_eq!(
            sig_via_outbox_signing_key, sig_via_private_identity,
            "DmOutbox.signing_key and DmOutbox.private_identity must produce identical signatures"
        );
        assert_eq!(
            outbox.our_signing_device_hash.0, outbox.private_identity.identity.address_hash,
            "DmOutbox.our_signing_device_hash must match private_identity.identity.address_hash \
             (3-way invariant: signing_key, private_identity, device_hash all from one identity)"
        );
    }

    fn entry(id: u8) -> OutboxEntry {
        OutboxEntry {
            id: OutboxEntryId([id; 16]),
            space_id: SpaceId([1u8; 16]),
            recipient_owners: vec![OwnerAddr([2u8; 16])],
            message_cid: Some(ContentId::from_bytes([3u8; 32])),
            created_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "test".into(),
            },
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        }
    }

    /// Build a minimal-but-valid DM Space. Members must be sorted ascending
    /// (DM invariant), transport must be Reticulum (DM invariant), content_key
    /// must be Some (DM invariant). Tests that want a different kind must reset
    /// these fields after calling.
    fn make_dm_space(id_byte: u8, members: Vec<OwnerAddr>) -> Space {
        Space {
            id: SpaceId([id_byte; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "Bob".into(),
            transport: None,
            members,
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "dev".into(),
            },
            updated_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "dev".into(),
            },
            content_key: Some(DmContentKey::new([0x42u8; 32])),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            read_receipt_pref: None,
            pending_join_at: None,
        }
    }

    fn install_space(state: &mut OwnerState, sp: Space) {
        let outcome = state.apply_space_with_canonicalization(sp);
        assert!(
            matches!(outcome, ApplyOutcome::Inserted),
            "fixture install must succeed, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn stub_records_sends_and_returns_default_ok() {
        let t = StubTransport::new();
        let e = entry(1);
        let r = OwnerAddr([2u8; 16]);
        let res = t.send(&e, r, Vec::new()).await;
        assert!(res.is_ok(), "default outcome is Ok: {res:?}");
        assert_eq!(t.sends(), vec![(e.id, r)]);
    }

    #[tokio::test]
    async fn stub_transport_caps_recorded_sends_at_max() {
        // StubTransport is wired into start_node as the production Phase 2
        // transport. Without the FIFO cap on `sends`, a long-lived node would
        // accumulate one entry per send call forever (~32 bytes each). Verify:
        //   - count is bounded at STUB_MAX_RECORDED_SENDS
        //   - eviction is FIFO (oldest evicted, not newest) — guards against
        //     a future refactor accidentally using pop_back
        let t = StubTransport::new();
        let r = OwnerAddr([2u8; 16]);
        // Each call uses a unique entry_id (1, 2, ...) so we can verify which
        // entries survived eviction by their byte-pattern.
        let total = 2000u32;
        for i in 1..=total {
            let id = OutboxEntryId([i as u8; 16]);
            let mut e = entry(0);
            e.id = id;
            let _ = t.send(&e, r, Vec::new()).await;
        }
        let recorded = t.sends();
        assert_eq!(
            recorded.len(),
            StubTransport::STUB_MAX_RECORDED_SENDS,
            "ring buffer must cap at STUB_MAX_RECORDED_SENDS"
        );
        // FIFO: the oldest survivor is push #(total - cap + 1).
        // total=2000, cap=1024 → first survivor is #977.
        // entry_id is [u8; 16] of (i as u8), which wraps mod 256.
        let first_survivor_index = total - StubTransport::STUB_MAX_RECORDED_SENDS as u32 + 1;
        let expected_first_byte = first_survivor_index as u8;
        assert_eq!(
            recorded[0].0 .0[0], expected_first_byte,
            "FIFO eviction: oldest survivor should be push #{first_survivor_index}, \
             not the newest entry (would indicate pop_back regression)"
        );
        // Last survivor is push #total.
        let expected_last_byte = total as u8;
        assert_eq!(
            recorded[recorded.len() - 1].0 .0[0],
            expected_last_byte,
            "FIFO eviction: newest survivor should be push #{total}"
        );
    }

    #[test]
    fn dm_outbox_constructs_with_empty_state() {
        let alice = OwnerAddr([0xaa; 16]);
        let o = make_outbox_synthetic("dev", alice);
        assert_eq!(o.device_id, "dev");
        assert_eq!(o.self_owner, OwnerAddr([0xaa; 16]));
        assert!(o.in_flight.is_empty());
        assert!(o.backoff.is_empty());
    }

    /// ZEB-339 Task 6: DmOutbox must carry the enrolled device #2 signing key
    /// (`community_signing_key`) and this device's own EnrollmentCert
    /// (`enrollment_cert`), DISTINCT from the Reticulum transport `signing_key`.
    ///
    /// Invariants:
    ///   - `enrollment_cert.verify()` must succeed (cert is well-formed).
    ///   - `enrollment_cert.device_pubkeys.classical.ed25519_verify` must match
    ///     `community_signing_key.verifying_key().to_bytes()` (key–cert binding).
    ///   - `community_signing_key` is a different Arc / key material from
    ///     `signing_key` (the Reticulum transport key).
    #[cfg(any(test, feature = "test-fixtures"))]
    #[test]
    fn dm_outbox_community_signing_key_and_enrollment_cert() {
        use crate::community_membership::mint_test_owner;

        let test_owner = mint_test_owner(0x77);

        // Build the community-signing materials from mint_test_owner output.
        let community_signing_key_arc = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(
            &test_owner.device_key.to_bytes(),
        ));
        let enrollment_cert = test_owner.cert.clone();

        // Build the separate Reticulum/transport signing key from a different
        // identity seed (the existing synthetic pattern from make_outbox_synthetic).
        let private_identity = harmony_identity::PrivateIdentity::from_seed(&[0x55; 32]);
        let priv_bytes = private_identity.to_private_bytes();
        let mut ed_seed = [0u8; 32];
        ed_seed.copy_from_slice(&priv_bytes[32..64]);
        let reticulum_signing_key =
            std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&ed_seed));
        let device_hash = DeviceIdentityHash(private_identity.identity.address_hash);
        let private_identity_arc = std::sync::Arc::new(private_identity);
        let self_owner = test_owner.owner;

        let outbox = DmOutbox::new(
            "dev".into(),
            self_owner,
            device_hash,
            reticulum_signing_key.clone(),
            private_identity_arc,
            community_signing_key_arc.clone(),
            enrollment_cert,
        );

        // 1. EnrollmentCert verifies.
        outbox
            .enrollment_cert
            .verify(0)
            .expect("enrollment_cert must verify");

        // 2. cert.device_pubkeys.classical.ed25519_verify matches community_signing_key.
        assert_eq!(
            outbox
                .enrollment_cert
                .device_pubkeys
                .classical
                .ed25519_verify,
            outbox.community_signing_key.verifying_key().to_bytes(),
            "enrollment_cert must bind to community_signing_key's verifying key"
        );

        // 3. community_signing_key is distinct from the Reticulum signing_key.
        assert_ne!(
            outbox.community_signing_key.verifying_key().to_bytes(),
            outbox.signing_key.verifying_key().to_bytes(),
            "community_signing_key (#2) must be distinct from the Reticulum signing_key (#3)"
        );
    }

    #[tokio::test]
    async fn send_dm_creates_outbox_entry() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let sp = make_dm_space(7, vec![alice, bob]);
        let space_id = sp.id;
        install_space(&mut state, sp);

        let cas = InMemoryStub::default();
        let mut o = make_outbox_synthetic("dev", alice);
        let (msg_id, _msg_cid) = o
            .send_dm(
                &mut state,
                &cas,
                space_id,
                b"hello".to_vec(),
                "text/plain".into(),
                1_000,
                None,
            )
            .await
            .expect("send_dm ok");

        let stored = state.outbox.get(&msg_id).expect("entry installed");
        assert_eq!(stored.space_id, space_id);
        assert_eq!(stored.recipient_owners, vec![bob], "Alice excluded");
        assert!(stored.delivered_to.is_empty());
        assert!(matches!(stored.delivery_status, DeliveryStatus::Pending));
    }

    #[tokio::test]
    async fn send_dm_writes_self_inbox_entry_alongside_outbox_entry() {
        // Phase 4 self-history persistence: send_dm must write a self-InboxEntry
        // alongside the OutboxEntry, so self-sent messages survive past
        // OutboxEntry's lifetime (Complete entries can be GC'd; InboxEntry is
        // the durable scrollback record).
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let sp = make_dm_space(7, vec![alice, bob]);
        let space_id = sp.id;
        install_space(&mut state, sp);

        let cas = InMemoryStub::default();
        let mut o = make_outbox_synthetic("dev", alice);
        let (_msg_id, _msg_cid) = o
            .send_dm(
                &mut state,
                &cas,
                space_id,
                b"hello".to_vec(),
                "text/plain".into(),
                1_000_000,
                None,
            )
            .await
            .expect("send_dm must succeed");

        // Self-InboxEntry exists at (space_id, message_cid) with from = self_owner.
        let self_inbox: Vec<&InboxEntry> = state
            .inbox
            .values()
            .filter(|e| e.space_id == space_id && e.from == o.self_owner)
            .collect();
        assert_eq!(
            self_inbox.len(),
            1,
            "send_dm must write exactly one self-InboxEntry"
        );
        assert_eq!(
            self_inbox[0].from, o.self_owner,
            "self-InboxEntry from = self_owner"
        );
    }

    #[tokio::test]
    async fn send_dm_invalid_space_kind_rejects() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let mut sp = make_dm_space(7, vec![alice, OwnerAddr([0x02; 16])]);
        // Mutate to a Folder Space — this is the kind that send_dm must reject.
        // Folder invariant requires transport=None, members=[], content_key=None
        // (and no prior_content_keys). Reset all four together so the fixture
        // installs cleanly.
        sp.kind = SpaceKind::Folder;
        sp.transport = None;
        sp.content_key = None;
        sp.members = vec![];
        let space_id = sp.id;
        install_space(&mut state, sp);

        let cas = InMemoryStub::default();
        let mut o = make_outbox_synthetic("dev", alice);
        let err = o
            .send_dm(
                &mut state,
                &cas,
                space_id,
                b"x".to_vec(),
                "text/plain".into(),
                1_000,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SendDmError::InvalidSpaceKind(_, "Folder")));
    }

    #[tokio::test]
    async fn send_dm_unknown_space_rejects() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let cas = InMemoryStub::default();
        let mut o = make_outbox_synthetic("dev", alice);
        let err = o
            .send_dm(
                &mut state,
                &cas,
                SpaceId([0x99; 16]),
                b"x".to_vec(),
                "text/plain".into(),
                1_000,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SendDmError::UnknownSpace(_)));
    }

    fn install_outbox_entry(state: &mut OwnerState, entry: OutboxEntry) {
        match state.apply_outbox(entry) {
            ApplyOutcome::Inserted => {}
            other => panic!("expected Inserted, got {other:?}"),
        }
    }

    fn outbox_entry_with_recipients(id: u8, recipients: Vec<OwnerAddr>) -> OutboxEntry {
        OutboxEntry {
            id: OutboxEntryId([id; 16]),
            space_id: SpaceId([1u8; 16]),
            recipient_owners: recipients,
            message_cid: Some(ContentId::from_bytes([3u8; 32])),
            created_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "dev".into(),
            },
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        }
    }

    #[test]
    fn mark_ack_delivered_updates_delivered_to() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = outbox_entry_with_recipients(7, vec![bob]);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let mut o = make_outbox_synthetic("dev", alice);
        let inserted = o.mark_ack_delivered(&mut state, entry_id, bob);

        assert!(inserted, "first ack inserts");
        let stored = state.outbox.get(&entry_id).unwrap();
        assert!(stored.delivered_to.contains(&bob));
        assert!(matches!(stored.delivery_status, DeliveryStatus::Complete));
    }

    #[test]
    fn mark_ack_delivered_duplicate_is_idempotent() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = outbox_entry_with_recipients(7, vec![bob]);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let mut o = make_outbox_synthetic("dev", alice);
        let first = o.mark_ack_delivered(&mut state, entry_id, bob);
        let second = o.mark_ack_delivered(&mut state, entry_id, bob);

        assert!(first);
        assert!(!second, "duplicate ack returns false");
        let stored = state.outbox.get(&entry_id).unwrap();
        assert_eq!(stored.delivered_to.len(), 1);
        assert!(matches!(stored.delivery_status, DeliveryStatus::Complete));
    }

    #[test]
    fn resolve_signed_origin_owner_single_match_returns_owner() {
        use crate::owner_state_types::{OwnerDeviceCache, OwnerDeviceEntry};
        let mut cache = OwnerDeviceCache::default();
        cache.devices.insert(
            OwnerAddr([1; 16]),
            OwnerDeviceEntry {
                devices: vec![DeviceIdentityHash([0xa1; 16])],
                device_identity_pubs: vec![Some([0x11; 64])],
                device_tunnel_contacts: vec![None],
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "d".into(),
                },
            },
        );
        let owner = resolve_signed_origin_owner(&cache, DeviceIdentityHash([0xa1; 16])).unwrap();
        assert_eq!(owner, OwnerAddr([1; 16]));
    }

    #[test]
    fn resolve_signed_origin_owner_no_matches_returns_unknown() {
        use crate::owner_state_types::OwnerDeviceCache;
        let cache = OwnerDeviceCache::default();
        let err = resolve_signed_origin_owner(&cache, DeviceIdentityHash([0xff; 16])).unwrap_err();
        assert!(matches!(err, DmReceiveError::UnknownSigningDevice));
    }

    #[test]
    fn resolve_signed_origin_owner_multi_match_returns_ambiguous() {
        // Two OwnerAddr entries claiming the same DeviceIdentityHash.
        // Reachable only via corrupted state or a malicious DmInvite that
        // asserted an existing device hash for a different owner.
        // Resolution untrustworthy — drop with telemetry.
        use crate::owner_state_types::{OwnerDeviceCache, OwnerDeviceEntry};
        let mut cache = OwnerDeviceCache::default();
        let shared = DeviceIdentityHash([0xa1; 16]);
        cache.devices.insert(
            OwnerAddr([1; 16]),
            OwnerDeviceEntry {
                devices: vec![shared],
                device_identity_pubs: vec![Some([0x11; 64])],
                device_tunnel_contacts: vec![None],
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "d".into(),
                },
            },
        );
        cache.devices.insert(
            OwnerAddr([2; 16]),
            OwnerDeviceEntry {
                devices: vec![shared], // same hash claimed by a different owner
                device_identity_pubs: vec![Some([0x22; 64])],
                device_tunnel_contacts: vec![None],
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "d".into(),
                },
            },
        );
        let err = resolve_signed_origin_owner(&cache, shared).unwrap_err();
        assert!(matches!(err, DmReceiveError::AmbiguousSigningDevice));
    }

    fn entry_with_age(id: u8, recipients: Vec<OwnerAddr>, created_wall_ms: u64) -> OutboxEntry {
        OutboxEntry {
            id: OutboxEntryId([id; 16]),
            space_id: SpaceId([1u8; 16]),
            recipient_owners: recipients,
            message_cid: Some(ContentId::from_bytes([3u8; 32])),
            created_at: Hlc {
                wall_ms: created_wall_ms,
                logical: 0,
                device_id: "dev".into(),
            },
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        }
    }

    #[tokio::test]
    async fn drain_advances_pending_to_complete_on_stub_success() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = entry_with_age(7, vec![bob], 1_000);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let transport = StubTransport::new();
        let mut o = make_outbox_synthetic("dev", alice);
        let outcome = o.drain(&mut state, &transport, 2_000).await;

        assert!(
            outcome.newly_delivered.is_empty(),
            "stub send is Ok but ack hasn't arrived; status stays Pending"
        );
        assert_eq!(transport.sends(), vec![(entry_id, bob)]);
        let stored = state.outbox.get(&entry_id).unwrap();
        assert!(matches!(stored.delivery_status, DeliveryStatus::Pending));

        // Now simulate the ack arriving (production routes deposit acks
        // through the sweep via mark_ack_delivered; Phase 2 callers do it
        // directly).
        let inserted = o.mark_ack_delivered(&mut state, entry_id, bob);
        assert!(inserted);
        let stored = state.outbox.get(&entry_id).unwrap();
        assert!(matches!(stored.delivery_status, DeliveryStatus::Complete));
    }

    #[test]
    fn drain_phase_c_clears_in_flight_for_skipped_pairs() {
        // ZEB-233 round 2 (closes ZEB-277): Phase C must clear
        // in_flight markers for (entry_id, recipient) pairs that
        // Phase B skipped, in addition to pairs that produced send
        // results. Otherwise the marker would leak and the next
        // drain tick's Phase A would skip this pair forever.
        //
        // Contract verified here:
        //   * Skipped pair's in_flight marker is cleared.
        //   * Skipped pair's backoff is NOT updated (skipping isn't
        //     a send attempt — bumping failure_count would unfairly
        //     throttle a healthy entry on the next tick).
        //   * Sent pair's in_flight is cleared AND backoff is
        //     updated (the existing post-Ok backoff entry).

        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let carol = OwnerAddr([0xcc; 16]);

        let entry_sent = entry_with_age(7, vec![bob], 1_000);
        let entry_sent_id = entry_sent.id;
        let entry_skipped = entry_with_age(8, vec![carol], 1_000);
        let entry_skipped_id = entry_skipped.id;
        install_outbox_entry(&mut state, entry_sent);
        install_outbox_entry(&mut state, entry_skipped);

        let mut o = make_outbox_synthetic("dev", alice);
        // Phase A would mark BOTH (entry, recipient) pairs in_flight;
        // simulate that here.
        o.in_flight.insert((entry_sent_id, bob));
        o.in_flight.insert((entry_skipped_id, carol));

        // Phase B's outcome: entry_sent was sent successfully,
        // entry_skipped was skipped (liveness check failed / lock
        // contention).
        let results = vec![DrainSendResult {
            entry_id: entry_sent_id,
            recipient: bob,
            result: Ok(()),
        }];
        let skipped = vec![(entry_skipped_id, carol)];

        let (_outcome, _candidates) = o.drain_phase_c(&mut state, results, skipped, 2_000, 2_000);

        // Both in_flight markers should be cleared.
        assert!(
            !o.in_flight.contains(&(entry_sent_id, bob)),
            "in_flight for sent pair must be cleared"
        );
        assert!(
            !o.in_flight.contains(&(entry_skipped_id, carol)),
            "in_flight for skipped pair must be cleared (otherwise next \
             drain tick would skip this pair forever)"
        );

        // Sent pair's backoff updated (failure_count=1 for "sent but
        // ack pending" — the existing post-Ok throttle).
        assert!(
            o.backoff.contains_key(&(entry_sent_id, bob)),
            "backoff updated for sent pair (post-Ok throttle)"
        );

        // Skipped pair's backoff NOT updated.
        assert!(
            !o.backoff.contains_key(&(entry_skipped_id, carol)),
            "backoff MUST NOT be updated for skipped pair — skipping \
             is not a send attempt; bumping failure_count would unfairly \
             throttle a healthy entry next tick"
        );
    }

    #[test]
    fn drain_phase_c_skips_backoff_for_recipient_already_acked() {
        // ZEB-233 round 4 (CodeRabbit Minor): Phase C runs in a
        // spawned task that can be delayed by Phase C lock acquisition
        // (e.g., contended by a concurrent handle_ack). If handle_ack
        // marks a recipient `delivered` between Phase B's send and
        // Phase C's lock acquisition, Phase C MUST NOT resurrect
        // stale per-recipient backoff for that recipient.
        //
        // Test: install entry with bob as a recipient already in
        // delivered_to (simulating handle_ack having completed bob
        // between Phase B and Phase C). Run Phase C with a result for
        // bob. Assert: backoff entry NOT created for (entry, bob).

        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let carol = OwnerAddr([0xcc; 16]);

        let mut entry = entry_with_age(7, vec![bob, carol], 1_000);
        // Simulate handle_ack: bob already delivered before Phase C runs.
        // Mark Partial status (some delivered, others pending).
        entry.delivered_to.insert(bob);
        entry.delivery_status = DeliveryStatus::Partial;
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let mut o = make_outbox_synthetic("dev", alice);
        o.in_flight.insert((entry_id, bob));
        o.in_flight.insert((entry_id, carol));

        // Phase B finished sending to both; results for both arrive.
        // But by the time Phase C runs, bob has already been acked
        // (via handle_ack between Phase B and Phase C).
        let results = vec![
            DrainSendResult {
                entry_id,
                recipient: bob,
                result: Ok(()),
            },
            DrainSendResult {
                entry_id,
                recipient: carol,
                result: Ok(()),
            },
        ];
        let (_outcome, _candidates) =
            o.drain_phase_c(&mut state, results, Vec::new(), 2_000, 2_000);

        // bob's backoff MUST NOT be set — handle_ack already cleared
        // his per-recipient retry state; Phase C must not resurrect.
        assert!(
            !o.backoff.contains_key(&(entry_id, bob)),
            "backoff for already-acked recipient must NOT be set by Phase C \
             (would resurrect stale retry state)"
        );

        // carol's backoff MUST be set (she's still Pending in delivered_to).
        assert!(
            o.backoff.contains_key(&(entry_id, carol)),
            "backoff for still-pending recipient MUST be set normally"
        );

        // Both in_flight markers cleared.
        assert!(!o.in_flight.contains(&(entry_id, bob)));
        assert!(!o.in_flight.contains(&(entry_id, carol)));
    }

    #[test]
    fn drain_phase_c_uses_expiration_clock_not_backoff_clock_for_sweep() {
        // ZEB-233 round 3 (CodeRabbit Major): drain_phase_c uses two
        // distinct timestamps:
        //
        //   backoff_now_ms       — fresh wall clock, post-Phase-B
        //                          (when the send outcome was recorded)
        //   expiration_now_ms    — original tick clock, Phase A admission
        //                          time (when the tick started)
        //
        // Without this split, a slow transport.send or contended
        // Phase C lock could push an entry past EXPIRATION_MS in the
        // same tick it was just sent, marking it Expired before its
        // ack can arrive. This test pins the contract: entries
        // admitted by Phase A as live must NOT be expired by Phase C
        // in the same tick, regardless of Phase B/C latency.

        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);

        // Entry created at wall=0. EXPIRATION_MS = 30 days.
        let entry = entry_with_age(7, vec![bob], 0);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let mut o = make_outbox_synthetic("dev", alice);
        o.in_flight.insert((entry_id, bob));

        // Simulate Phase A admitting the entry at wall = EXPIRATION_MS - 10s.
        // Phase B + Phase C lock contention then push wall past
        // EXPIRATION_MS before Phase C runs. backoff_now_ms reflects
        // the slow post-Phase-B time; expiration_now_ms reflects the
        // Phase A admission time.
        let expiration_now_ms = EXPIRATION_MS - 10_000; // Phase A's tick time
        let backoff_now_ms = EXPIRATION_MS + 20_000; // 20s past EXPIRATION

        let results = vec![DrainSendResult {
            entry_id,
            recipient: bob,
            result: Ok(()),
        }];
        let (outcome, _candidates) = o.drain_phase_c(
            &mut state,
            results,
            Vec::new(),
            backoff_now_ms,
            expiration_now_ms,
        );

        // Contract: entry admitted by Phase A as live (age <
        // EXPIRATION_MS at expiration_now_ms) MUST NOT be expired in
        // this same tick, even though backoff_now_ms is well past
        // EXPIRATION_MS.
        assert!(
            outcome.newly_expired.is_empty(),
            "entry admitted by Phase A must NOT be expired in same tick \
             due to Phase B/C latency; got newly_expired: {:?}",
            outcome.newly_expired
        );
        let stored = state.outbox.get(&entry_id).unwrap();
        assert!(
            matches!(stored.delivery_status, DeliveryStatus::Pending),
            "entry must stay Pending; expiration uses tick time, not Phase C time"
        );

        // Backoff bookkeeping uses backoff_now_ms (the fresh clock),
        // so the next is_due check anchors to the actual send time.
        let backoff_entry = o.backoff.get(&(entry_id, bob)).unwrap();
        assert_eq!(
            backoff_entry.last_attempt_wall_ms, backoff_now_ms,
            "backoff uses backoff_now_ms (Phase C clock) — accurate \
             post-send anchor for the next is_due check"
        );
    }

    // =================================================================
    // ZEB-418 SP2 P1 Task 8: sender-side butler deposit rung
    // =================================================================

    use crate::butler_deposit::{ButlerDepositClient, ButlerDepositRequest, DepositRungOutcome};

    /// Mock deposit channel: records every request the drain's deposit
    /// rung hands it and returns a preset outcome (mirrors how
    /// `StubTransport` mocks `DmTransport`).
    struct MockDepositClient {
        outcome: Mutex<DepositRungOutcome>,
        calls: Mutex<Vec<ButlerDepositRequest>>,
    }

    impl MockDepositClient {
        fn returning(outcome: DepositRungOutcome) -> Arc<Self> {
            Arc::new(Self {
                outcome: Mutex::new(outcome),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<ButlerDepositRequest> {
            self.calls.lock().expect("mock poisoned").clone()
        }
    }

    #[async_trait]
    impl ButlerDepositClient for MockDepositClient {
        async fn deposit(&self, req: &ButlerDepositRequest) -> DepositRungOutcome {
            self.calls.lock().expect("mock poisoned").push(req.clone());
            self.outcome.lock().expect("mock poisoned").clone()
        }
    }

    /// Shared scaffold for the deposit-rung tests: one Pending entry for
    /// `bob`, a `StubTransport`, and an outbox with the given mock client
    /// installed. Returns everything the tests poke at.
    fn deposit_rung_fixture(
        outcome: DepositRungOutcome,
    ) -> (
        OwnerState,
        StubTransport,
        DmOutbox,
        Arc<MockDepositClient>,
        OutboxEntryId,
        OwnerAddr,
    ) {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = entry_with_age(7, vec![bob], 1_000);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let transport = StubTransport::new();
        let mut o = make_outbox_synthetic("dev", alice);
        let mock = MockDepositClient::returning(outcome);
        o.set_butler_deposit_client(mock.clone());
        (state, transport, o, mock, entry_id, bob)
    }

    /// Drive one drain tick at `wall_now_ms` with a pre-seeded TRANSIENT
    /// direct-send failure for `(entry_id, bob)`.
    async fn drain_with_transient_failure(
        o: &mut DmOutbox,
        state: &mut OwnerState,
        transport: &StubTransport,
        entry_id: OutboxEntryId,
        bob: OwnerAddr,
        wall_now_ms: u64,
    ) -> DrainOutcome {
        transport.set_outcome(
            entry_id,
            bob,
            Err(TransportError::Transient("recipient unreachable".into())),
        );
        o.drain(state, transport, wall_now_ms).await
    }

    /// The deposit rung fires on a transient direct failure once the entry
    /// has been pending ≥ one backoff cycle (an `AttemptState` already
    /// existed), at most once per backoff window, with a request that
    /// carries the entry's identity + a CidNotify packet matching what the
    /// direct path would send.
    #[tokio::test]
    async fn transient_direct_failure_with_fresh_butler_set_attempts_deposit() {
        let (mut state, transport, mut o, mock, entry_id, bob) =
            deposit_rung_fixture(DepositRungOutcome::Failed("butlers unreachable".into()));
        let space_id = SpaceId([1u8; 16]);
        let message_cid = ContentId::from_bytes([3u8; 32]);

        // Tick 1 (t=10_000): FIRST attempt fails transiently. No prior
        // AttemptState → the entry has NOT been pending ≥ one backoff
        // cycle → the rung must not fire.
        let _ = drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 10_000)
            .await;
        assert!(
            mock.calls().is_empty(),
            "first transient failure must NOT attempt a deposit (entry not \
             yet pending >= one backoff cycle)"
        );

        // Tick 2 (t=15_000, base 5s window elapsed): SECOND attempt also
        // fails transiently. AttemptState existed → deposit attempted.
        let _ = drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 15_000)
            .await;
        let calls = mock.calls();
        assert_eq!(calls.len(), 1, "deposit rung must fire exactly once");
        let req = &calls[0];
        assert_eq!(req.entry_id, entry_id);
        assert_eq!(req.recipient_owner, bob);
        assert_eq!(req.space_id, space_id);
        assert_eq!(req.message_cid, Some(message_cid));
        assert_eq!(req.now_ms, 15_000);

        // The packet rides the SAME construction the direct path sends:
        // a signed CidNotify for this entry from this device.
        let packet = crate::dm_envelope::decode_packet(
            req.cidnotify_packet
                .as_deref()
                .expect("deposit message entry has cidnotify_packet"),
        )
        .expect("deposit request must carry a decodable DM packet");
        match packet {
            crate::dm_envelope::DmPacket::CidNotify { signed, .. } => {
                assert_eq!(signed.space_id, space_id);
                assert_eq!(signed.message_cid, message_cid);
                assert_eq!(signed.sender_owner_addr, o.self_owner);
                assert_eq!(signed.signing_device_hash, o.our_signing_device_hash);
            }
            other => panic!("expected CidNotify, got {other:?}"),
        }

        // Tick 3 (t=15_001, inside the new 10s window): direct send not
        // due → no new failure event → no second deposit (no hot loop).
        let _ = o.drain(&mut state, &transport, 15_001).await;
        assert_eq!(
            mock.calls().len(),
            1,
            "deposit must be attempted at most once per backoff window"
        );
    }

    /// ZEB-483: a DM-space deposit request carries a piggybacked signed
    /// DmInvite that a FRESH recipient state can apply (signature + admission
    /// gates pass) to bootstrap the DM Space from the deposit rung.
    #[tokio::test]
    async fn deposit_candidate_attaches_signed_invite_for_dm_space() {
        let (mut state, transport, mut o, mock, entry_id, bob) =
            deposit_rung_fixture(DepositRungOutcome::Failed("butlers unreachable".into()));
        // deposit_rung_fixture installs a DM Space for the entry; ensure it has
        // a content_key + both members so the invite rebuild has its inputs.
        // The entry's space_id is SpaceId([1u8; 16]) (entry_with_age), so the
        // DM space must share that id.
        let space_id = SpaceId([1u8; 16]);
        install_space(&mut state, make_dm_space(1, vec![o.self_owner, bob]));

        // Drive two transient failures to trip the deposit rung (the first never
        // deposits, the second does — matches the existing rung tests).
        let _ = drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 10_000)
            .await;
        let _ = drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 15_000)
            .await;

        let calls = mock.calls();
        assert_eq!(calls.len(), 1, "deposit rung fires once");
        let invite_bytes = calls[0]
            .invite_packet
            .as_ref()
            .expect("DM-space deposit must carry a piggybacked invite");

        // The invite decodes to a DmPacket::Invite for the same Space, inviter == sender.
        let packet = crate::dm_envelope::decode_packet(invite_bytes).expect("decode invite");
        let crate::dm_envelope::DmPacket::Invite {
            signed,
            signature,
            signed_bytes,
        } = packet
        else {
            panic!("expected Invite");
        };
        assert_eq!(signed.space_id, space_id);
        assert_eq!(signed.inviter, o.self_owner);
        assert!(signed.members.contains(&o.self_owner) && signed.members.contains(&bob));

        // And a FRESH recipient state applies it (signature + admission gates pass).
        let mut rx = OwnerState::default();
        // ZEB-236: the recipient auto-accepts only from an ACTIVE friend (tier
        // fork); the inviter here is `o.self_owner`, so befriend them.
        insert_active_friend(&mut rx, o.self_owner);
        let outcome = crate::dm_outbox::apply_invite(
            &mut rx,
            bob,       // recipient self
            "bob-dev", // recipient device id
            signed,
            signature,
            &signed_bytes,
            20_000,
            Some(o.self_owner), // expected inviter
            true,               // full apply (Space + cache) on a fresh recipient
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
        );
        assert!(
            outcome.is_ok(),
            "rebuilt invite must apply on a fresh recipient: {outcome:?}"
        );
        assert!(
            rx.spaces.contains_key(&space_id),
            "Space bootstrapped from the deposited invite"
        );
    }

    /// ZEB-483: a non-DM (Community) space yields no piggybacked invite.
    #[tokio::test]
    async fn deposit_candidate_omits_invite_for_non_dm_space() {
        let (mut state, transport, mut o, mock, entry_id, bob) =
            deposit_rung_fixture(DepositRungOutcome::Failed("x".into()));
        // Replace the space with a community (non-DM) space sharing the entry's
        // id. Insert directly into `state.spaces` rather than via `install_space`
        // — a Community space requires the full epoch/admin invariant set that
        // `apply_space_with_canonicalization` validates on insert, none of which
        // matters here: this test only exercises `build_invite_packet_bytes`'s
        // `SpaceKind` guard, which short-circuits before reading any other field.
        let mut community = make_dm_space(1, vec![o.self_owner, bob]);
        community.kind = SpaceKind::Community;
        community.content_key = None;
        state.spaces.insert(community.id, community);

        let _ = drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 10_000)
            .await;
        let _ = drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 15_000)
            .await;

        assert_eq!(
            mock.calls()[0].invite_packet,
            None,
            "non-DM deposit carries no invite"
        );
    }

    /// ZEB-483 (CodeRabbit): a DM-space invite rebuild FAILURE is fail-closed —
    /// the whole deposit candidate is skipped so the entry stays pending for
    /// retry, rather than depositing a CidNotify an offline recipient would
    /// recover into `SpaceNotFound`. Modelled by a DM Space missing its
    /// `content_key` (inserted directly to bypass the invariant that would
    /// normally reject such a Space).
    #[tokio::test]
    async fn deposit_candidate_skipped_when_dm_invite_rebuild_fails() {
        let (mut state, transport, mut o, mock, entry_id, bob) =
            deposit_rung_fixture(DepositRungOutcome::Failed("x".into()));
        // A DM Space at the entry's id but with NO content_key → invite rebuild
        // returns Err → the candidate must be skipped entirely.
        let mut broken = make_dm_space(1, vec![o.self_owner, bob]);
        broken.content_key = None;
        state.spaces.insert(broken.id, broken);

        let _ = drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 10_000)
            .await;
        let _ = drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 15_000)
            .await;

        assert!(
            mock.calls().is_empty(),
            "DM invite rebuild failure skips the deposit candidate (no CidNotify deposited)"
        );
    }

    /// A butler ack marks the recipient delivered through the existing
    /// `mark_ack_delivered` path and surfaces in `newly_delivered` (the
    /// `dm-delivered` IPC emit contract).
    #[tokio::test]
    async fn deposit_ack_marks_owner_delivered_and_emits_dm_delivered() {
        let (mut state, transport, mut o, mock, entry_id, bob) =
            deposit_rung_fixture(DepositRungOutcome::Acked);
        let space_id = SpaceId([1u8; 16]);
        let message_cid = ContentId::from_bytes([3u8; 32]);

        let outcome1 =
            drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 10_000)
                .await;
        assert!(outcome1.newly_delivered.is_empty());

        let outcome2 =
            drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 15_000)
                .await;
        assert_eq!(mock.calls().len(), 1, "deposit attempted on tick 2");
        assert_eq!(
            outcome2.newly_delivered,
            vec![(space_id, message_cid, bob)],
            "deposit ack must surface in newly_delivered (dm-delivered emit)"
        );

        let stored = state.outbox.get(&entry_id).expect("entry still present");
        assert!(stored.delivered_to.contains(&bob), "bob marked delivered");
        assert!(
            matches!(stored.delivery_status, DeliveryStatus::Complete),
            "sole recipient acked via butler -> Complete"
        );
        // mark_ack_delivered cleared the pair's retry state.
        assert_eq!(o.backoff_len(), 0);
        assert_eq!(o.in_flight_len(), 0);
    }

    /// A stale/missing butler set skips the rung silently: the entry keeps
    /// the exact transient-failure backoff and the existing direct retry
    /// chain proceeds unchanged (spec §6: never worse than today).
    #[tokio::test]
    async fn stale_or_missing_butler_set_skips_rung_falls_back_to_retry() {
        let (mut state, transport, mut o, mock, entry_id, bob) =
            deposit_rung_fixture(DepositRungOutcome::SkippedNoFreshButlerSet);

        let _ = drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 10_000)
            .await;
        let outcome =
            drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 15_000)
                .await;
        assert_eq!(mock.calls().len(), 1, "rung consulted on tick 2");
        assert!(outcome.newly_delivered.is_empty(), "skip marks nothing");

        let stored = state.outbox.get(&entry_id).expect("entry still present");
        assert!(matches!(stored.delivery_status, DeliveryStatus::Pending));
        assert!(stored.delivered_to.is_empty());

        // Backoff is EXACTLY what the two transient failures left: the
        // skipped rung neither bumps nor clears it.
        let st = o
            .backoff
            .get(&(entry_id, bob))
            .copied()
            .expect("AttemptState retained");
        assert_eq!(st.failure_count, 2);
        assert_eq!(st.last_attempt_wall_ms, 15_000);

        // Rung 3: the direct retry chain re-attempts when the shared
        // backoff window (10s at failure_count=2) elapses.
        let _ = o.drain(&mut state, &transport, 25_000).await;
        assert_eq!(
            transport.sends().len(),
            3,
            "direct retry chain must continue exactly as before"
        );
    }

    /// A failed deposit (all butlers unreachable / rejected) leaves the
    /// entry pending under the SAME shared `AttemptState` a transient
    /// direct failure produces — no extra bump, no drop, no tightening.
    #[tokio::test]
    async fn deposit_failure_leaves_entry_pending_with_backoff() {
        let (mut state, transport, mut o, mock, entry_id, bob) =
            deposit_rung_fixture(DepositRungOutcome::Failed("all entries failed".into()));

        let _ = drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 10_000)
            .await;
        let outcome =
            drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 15_000)
                .await;
        assert_eq!(mock.calls().len(), 1, "deposit attempted and failed");
        assert!(outcome.newly_delivered.is_empty());

        let stored = state.outbox.get(&entry_id).expect("entry still present");
        assert!(matches!(stored.delivery_status, DeliveryStatus::Pending));
        assert!(stored.delivered_to.is_empty());

        let st = o
            .backoff
            .get(&(entry_id, bob))
            .copied()
            .expect("shared AttemptState retained");
        assert_eq!(
            st.failure_count, 2,
            "rung failure adds NO extra failure_count bump"
        );
        assert_eq!(st.last_attempt_wall_ms, 15_000);

        // The shared backoff window is honored unchanged: no re-send just
        // before the 10s window ends, one exactly when it elapses.
        let _ = o.drain(&mut state, &transport, 24_999).await;
        assert_eq!(transport.sends().len(), 2, "still inside backoff window");
        let _ = o.drain(&mut state, &transport, 25_000).await;
        assert_eq!(
            transport.sends().len(),
            3,
            "retry resumes exactly per the shared AttemptState backoff"
        );
    }

    /// A late DIRECT ack arriving after the deposit ack already marked the
    /// recipient delivered must be a no-op — `mark_ack_delivered` is the
    /// single idempotent path both acks converge on.
    #[tokio::test]
    async fn late_direct_ack_after_deposit_ack_is_idempotent() {
        let (mut state, transport, mut o, mock, entry_id, bob) =
            deposit_rung_fixture(DepositRungOutcome::Acked);

        let _ = drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 10_000)
            .await;
        let _ = drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 15_000)
            .await;
        assert_eq!(mock.calls().len(), 1);
        assert!(state
            .outbox
            .get(&entry_id)
            .expect("entry present")
            .delivered_to
            .contains(&bob));

        // Late direct ack for the same (entry, recipient) — the path
        // handle_ack step 7 drives. Must report NOT newly delivered.
        let newly = o.mark_ack_delivered(&mut state, entry_id, bob);
        assert!(
            !newly,
            "late direct ack after deposit ack must be a no-op (idempotent)"
        );
        let stored = state.outbox.get(&entry_id).expect("entry present");
        assert_eq!(stored.delivered_to.len(), 1, "no duplicate delivered_to");
        assert!(matches!(stored.delivery_status, DeliveryStatus::Complete));
    }

    // =================================================================
    // ZEB-418 SP2 P2 (ZEB-422): sent-but-never-acked deposit candidacy
    // =================================================================

    /// ZEB-422: an Ok send that never acks ACCUMULATES `failure_count`
    /// across backoff windows instead of being overwritten to 1 each
    /// window — the substrate the sent-but-never-acked candidacy counts
    /// on. Intentional side effect: direct-send backoff grows toward the
    /// 5-min cap for unresponsive recipients, matching the Err path.
    #[tokio::test]
    async fn ok_send_without_ack_accumulates_failure_count() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = entry_with_age(7, vec![bob], 1_000);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let transport = StubTransport::new();
        let mut o = make_outbox_synthetic("dev", alice);

        // Window 1: Ok send at t=10s -> failure_count 0 -> 1.
        let _ = o.drain(&mut state, &transport, 10_000).await;
        // Window 2 (+6s, past the 5s base window): Ok send -> 1 -> 2.
        // The pre-ZEB-422 Ok-arm overwrote the count back to 1 here.
        let _ = o.drain(&mut state, &transport, 16_000).await;
        assert_eq!(transport.sends().len(), 2, "both windows sent");

        let st = o
            .backoff
            .get(&(entry_id, bob))
            .copied()
            .expect("AttemptState retained while unacked");
        assert_eq!(
            st.failure_count, 2,
            "ZEB-422: sent-but-never-acked windows must ACCUMULATE \
             failure_count (old Ok-arm overwrote it to 1 every window)"
        );
        assert_eq!(st.last_attempt_wall_ms, 16_000);
    }

    /// ZEB-422: the very first Ok send must NOT consult the deposit rung —
    /// candidacy starts only once the pair has sat unacked for
    /// `DEPOSIT_NOACK_WINDOWS` full backoff windows.
    #[tokio::test]
    async fn first_ok_send_does_not_trigger_rung() {
        let (mut state, transport, mut o, mock, _entry_id, _bob) =
            deposit_rung_fixture(DepositRungOutcome::Acked);

        // StubTransport's default (un-seeded) outcome is Ok(()).
        let _ = o.drain(&mut state, &transport, 10_000).await;
        assert_eq!(transport.sends().len(), 1, "direct send fired");
        assert!(
            mock.calls().is_empty(),
            "first Ok send must NOT attempt a deposit (pair has completed \
             zero unacked backoff windows)"
        );
    }

    /// ZEB-422 PRIMARY scenario: a cached-but-offline recipient — every
    /// direct send returns Ok (enqueued) but never acks. Once the pair has
    /// sat sent-but-never-acked for `DEPOSIT_NOACK_WINDOWS` full backoff
    /// windows, the next Ok-send window also tries the butler rung; a
    /// butler ack then marks the recipient delivered through the same
    /// idempotent `mark_ack_delivered` path the transient-failure rung
    /// uses (cf. `deposit_ack_marks_owner_delivered_and_emits_dm_delivered`,
    /// which drives the Err path).
    #[tokio::test]
    async fn noack_after_n_windows_triggers_deposit_rung() {
        let (mut state, transport, mut o, mock, entry_id, bob) =
            deposit_rung_fixture(DepositRungOutcome::Acked);
        let space_id = SpaceId([1u8; 16]);
        let message_cid = ContentId::from_bytes([3u8; 32]);
        assert_eq!(
            crate::butler_deposit::DEPOSIT_NOACK_WINDOWS,
            2,
            "tick cadence below assumes N=2; update the windows driven \
             here if the constant changes"
        );

        // Window 1 (t=10s): Ok send, pre_count=0 -> no rung.
        let outcome1 = o.drain(&mut state, &transport, 10_000).await;
        assert!(outcome1.newly_delivered.is_empty());
        // Window 2 (t=16s, +6s past the 5s window): Ok send, pre_count=1
        // -> still below DEPOSIT_NOACK_WINDOWS -> no rung.
        let outcome2 = o.drain(&mut state, &transport, 16_000).await;
        assert!(outcome2.newly_delivered.is_empty());
        assert!(
            mock.calls().is_empty(),
            "no deposit before DEPOSIT_NOACK_WINDOWS unacked windows"
        );

        // Window 3 (t=27s, +11s past the 10s window): Ok send,
        // pre_count=2 == DEPOSIT_NOACK_WINDOWS -> rung fires; mock acks.
        let outcome3 = o.drain(&mut state, &transport, 27_000).await;
        let calls = mock.calls();
        assert!(
            !calls.is_empty(),
            "deposit rung must fire once the pair sat unacked for \
             DEPOSIT_NOACK_WINDOWS full backoff windows"
        );
        assert_eq!(calls.len(), 1, "exactly one deposit this window");
        let req = &calls[0];
        assert_eq!(req.entry_id, entry_id);
        assert_eq!(req.recipient_owner, bob);
        assert_eq!(req.space_id, space_id);
        assert_eq!(req.message_cid, Some(message_cid));
        assert_eq!(
            req.now_ms, 27_000,
            "freshness clock = this tick's backoff clock (same as Err arm)"
        );

        // Butler ack -> delivered via mark_ack_delivered (dm-delivered emit).
        assert_eq!(
            outcome3.newly_delivered,
            vec![(space_id, message_cid, bob)],
            "deposit ack must surface in newly_delivered (dm-delivered emit)"
        );
        let stored = state.outbox.get(&entry_id).expect("entry still present");
        assert!(stored.delivered_to.contains(&bob), "bob marked delivered");
        assert!(
            matches!(stored.delivery_status, DeliveryStatus::Complete),
            "sole recipient acked via butler -> Complete"
        );
        // mark_ack_delivered cleared the pair's retry state.
        assert_eq!(o.backoff_len(), 0);
        assert_eq!(o.in_flight_len(), 0);
    }

    /// ZEB-422 never-worse invariant on the Ok path: a Failed rung outcome
    /// leaves the pair's `AttemptState` EXACTLY as the Ok-arm's own bump
    /// wrote it — no extra failure_count bump, no clock rewrite, no clear.
    #[tokio::test]
    async fn rung_outcome_never_touches_attempt_state_on_ok_path() {
        let (mut state, transport, mut o, mock, entry_id, bob) =
            deposit_rung_fixture(DepositRungOutcome::Failed("butlers unreachable".into()));

        // Two Ok windows to reach candidacy (failure_count 0 -> 1 -> 2).
        let _ = o.drain(&mut state, &transport, 10_000).await;
        let _ = o.drain(&mut state, &transport, 16_000).await;
        let before = o
            .backoff
            .get(&(entry_id, bob))
            .copied()
            .expect("AttemptState present after two unacked windows");
        assert_eq!(before.failure_count, 2);
        assert_eq!(before.last_attempt_wall_ms, 16_000);

        // Rung-bearing window (t=27s): the Ok-arm bumps 2 -> 3 and anchors
        // the clock; the rung fires and FAILS — adding nothing on top.
        let outcome = o.drain(&mut state, &transport, 27_000).await;
        assert_eq!(mock.calls().len(), 1, "rung consulted exactly once");
        assert!(
            outcome.newly_delivered.is_empty(),
            "failed rung marks nothing"
        );

        let after = o
            .backoff
            .get(&(entry_id, bob))
            .copied()
            .expect("AttemptState retained");
        assert_eq!(
            after.failure_count,
            before.failure_count + 1,
            "exactly the Ok-arm's own bump — the failed rung adds NOTHING"
        );
        assert_eq!(
            after.last_attempt_wall_ms, 27_000,
            "clock anchored by the Ok-arm only"
        );
        let stored = state.outbox.get(&entry_id).expect("entry still present");
        assert!(matches!(stored.delivery_status, DeliveryStatus::Pending));
        assert!(stored.delivered_to.is_empty());
    }

    // =================================================================
    // ZEB-424 D31: per-recipient mixed-state deposit candidacy on a
    // single fan-out OutboxEntry (CHARACTERIZATION — confirm, don't
    // change). Group-DM butler fans one logical message to N recipients
    // (one OutboxEntry, shared message_cid). D31 asserts the EXISTING
    // per-recipient candidacy already does the right thing when one
    // fan-out entry has recipients in MIXED delivery states. This test
    // pins that behavior; it must NOT drive any production change.
    // =================================================================

    /// One fan-out `OutboxEntry` with three recipients in mixed states:
    ///   - `alice_rcpt` (A): already acked (in `delivered_to`) — terminal.
    ///   - `bob` (B): sat sent-but-never-acked for `DEPOSIT_NOACK_WINDOWS`
    ///     full backoff windows (`failure_count == DEPOSIT_NOACK_WINDOWS`)
    ///     — a deposit candidate this tick.
    ///   - `carol` (C): fresh / below the no-ack threshold (no prior
    ///     `AttemptState`) — pending, NOT a candidate.
    ///
    /// One Phase C pass (the synchronous candidacy-producing phase, with
    /// Ok send results for the two OUTSTANDING recipients B and C — A is
    /// already delivered so no real drain tick would produce a send
    /// result for it) must:
    ///   1. produce a butler deposit candidate for B ONLY (not A, not C);
    ///   2. leave the entry's overall `DeliveryStatus` as `Partial` (a
    ///      deposit is a relay, not a direct ack — `drain_phase_c` only
    ///      returns candidates; it does NOT run them, so nothing flips to
    ///      Complete here);
    ///   3. not mutate A's per-recipient state as a side effect of B's
    ///      candidacy (A stays in `delivered_to`, never gets an
    ///      `AttemptState`), and leave C's `AttemptState` reflecting ONLY
    ///      C's own Ok-send bump (failure_count 0 -> 1) — B's deposit rung
    ///      never touches A's or C's backoff (spec §6 never-worse:
    ///      rung outcomes don't mutate AttemptState).
    #[test]
    fn drain_phase_c_mixed_state_fanout_deposits_only_for_noack_recipient() {
        // DEPOSIT_NOACK_WINDOWS is the candidacy threshold for the Ok
        // (sent-but-never-acked) arm; seed B exactly at it.
        let threshold = crate::butler_deposit::DEPOSIT_NOACK_WINDOWS;

        let mut state = OwnerState::default();
        let self_owner = OwnerAddr([0xaa; 16]);
        let alice_rcpt = OwnerAddr([0xa1; 16]); // A — acked (terminal)
        let bob = OwnerAddr([0xbb; 16]); // B — deposit candidate
        let carol = OwnerAddr([0xcc; 16]); // C — fresh / pending

        // One fan-out entry: all three recipients, A already acked.
        let mut entry = entry_with_age(7, vec![alice_rcpt, bob, carol], 1_000);
        entry.delivered_to.insert(alice_rcpt);
        entry.delivery_status = DeliveryStatus::Partial; // A acked, B+C outstanding
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let mut o = make_outbox_synthetic("dev", self_owner);
        // Butler client installed so the deposit rung is enabled at all.
        // Outcome is irrelevant — drain_phase_c only COLLECTS candidates;
        // it does not run them — but a Failed outcome documents that even
        // a non-acking butler can't flip the entry to Complete here.
        let mock = MockDepositClient::returning(DepositRungOutcome::Failed("unused".into()));
        o.set_butler_deposit_client(mock.clone());

        // Seed B at the no-ack threshold (it has sat unacked for
        // DEPOSIT_NOACK_WINDOWS full backoff windows). C gets NO prior
        // AttemptState — it is fresh / below threshold.
        o.backoff.insert(
            (entry_id, bob),
            AttemptState {
                last_attempt_wall_ms: 1_000,
                failure_count: threshold,
            },
        );

        // Phase B would have sent to the two OUTSTANDING recipients this
        // tick (A is delivered → no work unit / send result for it). Both
        // come back Ok (the cached-but-offline primary scenario: enqueued
        // but never acked).
        o.in_flight.insert((entry_id, bob));
        o.in_flight.insert((entry_id, carol));
        let results = vec![
            DrainSendResult {
                entry_id,
                recipient: bob,
                result: Ok(()),
            },
            DrainSendResult {
                entry_id,
                recipient: carol,
                result: Ok(()),
            },
        ];

        let (outcome, candidates) = o.drain_phase_c(&mut state, results, Vec::new(), 2_000, 2_000);

        // ---- Assertion 1: deposit candidate for B ONLY. ----
        // Bound the COUNT before the set conversion: a BTreeSet would silently
        // dedupe a duplicate-fanout regression (two candidates for B) and let
        // the value check below still pass.
        assert_eq!(
            candidates.len(),
            1,
            "exactly one deposit candidate expected (no duplicate fan-out)"
        );
        let candidate_recipients: BTreeSet<OwnerAddr> =
            candidates.iter().map(|c| c.recipient_owner).collect();
        assert_eq!(
            candidate_recipients,
            BTreeSet::from([bob]),
            "exactly one deposit candidate, for B (the no-ack recipient); \
             A is acked-terminal and C is below DEPOSIT_NOACK_WINDOWS"
        );
        // Every candidate carries the shared fan-out identity.
        for c in &candidates {
            assert_eq!(c.entry_id, entry_id, "candidate bound to the fan-out entry");
            assert_eq!(c.message_cid, Some(ContentId::from_bytes([3u8; 32])));
        }

        // ---- Assertion 2: entry stays Partial. ----
        let stored = state.outbox.get(&entry_id).expect("entry still present");
        assert!(
            matches!(stored.delivery_status, DeliveryStatus::Partial),
            "a butler deposit candidate is a relay, not a direct ack; \
             drain_phase_c returns candidates without running them, so the \
             entry must remain Partial (got {:?})",
            stored.delivery_status
        );
        assert!(
            outcome.newly_delivered.is_empty(),
            "no recipient was acked this pass — newly_delivered must be empty"
        );
        assert!(outcome.newly_expired.is_empty(), "entry is not expired");

        // ---- Assertion 3: B's candidacy doesn't perturb A or C. ----
        // A: still acked, never acquired per-recipient backoff state.
        assert!(
            stored.delivered_to.contains(&alice_rcpt),
            "A stays delivered; B's candidacy must not touch A"
        );
        assert!(
            !o.backoff.contains_key(&(entry_id, alice_rcpt)),
            "A (acked-terminal) must never acquire an AttemptState"
        );
        // C: AttemptState reflects ONLY its own Ok-send bump (0 -> 1) —
        // B's deposit rung adds nothing to C's backoff.
        let carol_state = o
            .backoff
            .get(&(entry_id, carol))
            .copied()
            .expect("C got its own Ok-send AttemptState this tick");
        assert_eq!(
            carol_state.failure_count, 1,
            "C's failure_count is exactly its own one-window Ok-send bump; \
             B's candidacy must not inflate it"
        );
        assert_eq!(
            carol_state.last_attempt_wall_ms, 2_000,
            "C's clock anchored by its own Ok-send only"
        );
        // B: its own Ok-arm bump (threshold -> threshold + 1); the rung
        // (candidacy) itself never mutates the AttemptState (spec §6).
        let bob_state = o
            .backoff
            .get(&(entry_id, bob))
            .copied()
            .expect("B's AttemptState retained while unacked");
        assert_eq!(
            bob_state.failure_count,
            threshold + 1,
            "B's failure_count is exactly the Ok-arm's own bump; the deposit \
             rung adds nothing"
        );

        // The mock was never INVOKED — drain_phase_c collects candidates
        // for the caller (drain_lifted) to run unlocked; it does not call
        // .deposit() itself. This pins that the status/AttemptState
        // observations above hold independent of any butler outcome.
        assert!(
            mock.calls().is_empty(),
            "drain_phase_c must only COLLECT candidates, never run the deposit"
        );
    }

    #[tokio::test]
    async fn drain_lifted_releases_outbox_lock_during_transport_send() {
        // ZEB-233 regression test: drain_lifted MUST release the outbox
        // + state locks for the duration of Phase B's
        // transport.send().await. Without this, concurrent send_dm IPCs
        // block on the slowest in-flight send.
        //
        // Verified by a custom transport (LockProbeTransport) that
        // attempts to `try_lock` the outbox from INSIDE its async send()
        // body. With the lock-lift working, Phase A's guard has been
        // dropped before Phase B awaits — try_lock succeeds. Without the
        // lock-lift, Phase A's guard is still held — try_lock returns
        // WouldBlock.
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::sync::Mutex;

        struct LockProbeTransport {
            outbox: Arc<Mutex<DmOutbox>>,
            state: Arc<Mutex<OwnerState>>,
            outbox_try_lock_succeeded: Arc<AtomicBool>,
            state_try_lock_succeeded: Arc<AtomicBool>,
            send_count: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl DmTransport for LockProbeTransport {
            async fn send(
                &self,
                _entry: &OutboxEntry,
                _recipient: OwnerAddr,
                _destinations: Vec<[u8; 16]>,
            ) -> Result<(), TransportError> {
                self.send_count.fetch_add(1, Ordering::SeqCst);
                // ZEB-233 round 1 (CodeRabbit Nitpick): probe BOTH the
                // outbox AND state locks. drain_lifted's lock-lift
                // releases both for the duration of Phase B; a
                // regression that releases only one would still pass
                // an outbox-only probe.
                if self.outbox.try_lock().is_ok() {
                    self.outbox_try_lock_succeeded.store(true, Ordering::SeqCst);
                }
                if self.state.try_lock().is_ok() {
                    self.state_try_lock_succeeded.store(true, Ordering::SeqCst);
                }
                Ok(())
            }
        }

        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = entry_with_age(7, vec![bob], 1_000);
        install_outbox_entry(&mut state, entry);

        let outbox = make_outbox_synthetic("dev", alice);
        let outbox_arc = Arc::new(Mutex::new(outbox));
        let state_arc = Arc::new(Mutex::new(state));

        let outbox_try_lock_succeeded = Arc::new(AtomicBool::new(false));
        let state_try_lock_succeeded = Arc::new(AtomicBool::new(false));
        let send_count = Arc::new(AtomicUsize::new(0));
        let transport = LockProbeTransport {
            outbox: Arc::clone(&outbox_arc),
            state: Arc::clone(&state_arc),
            outbox_try_lock_succeeded: Arc::clone(&outbox_try_lock_succeeded),
            state_try_lock_succeeded: Arc::clone(&state_try_lock_succeeded),
            send_count: Arc::clone(&send_count),
        };

        let app: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink> =
            std::sync::Arc::new(crate::node_event_sink::RecordingSink::new());

        super::drain_lifted(
            Arc::clone(&outbox_arc),
            Arc::clone(&state_arc),
            &transport,
            2_000,
            app,
            None, // ZEB-703: durability not exercised here
        )
        .await;

        // ZEB-233 round 1 (Qodo Reliability #2): the assertions below
        // only depend on Phase B (transport.send) having completed,
        // which happens before `drain_lifted(...).await` returns. No
        // post-await synchronization is needed. The spawned Phase C
        // is left to run (or be dropped on test exit) — tokio handles
        // its cleanup.
        assert!(
            send_count.load(Ordering::SeqCst) > 0,
            "transport.send must have been called at least once (1 outstanding recipient)"
        );
        assert!(
            outbox_try_lock_succeeded.load(Ordering::SeqCst),
            "ZEB-233 regression: outbox lock must be RELEASED during Phase B's transport.send. \
             The try_lock from inside send() failed, meaning Phase A's guard is still held."
        );
        assert!(
            state_try_lock_succeeded.load(Ordering::SeqCst),
            "ZEB-233 regression: state lock must be RELEASED during Phase B's transport.send. \
             The try_lock from inside send() failed, meaning Phase A's guard is still held."
        );
    }

    /// ZEB-703 (PR #485 Greptile P1): once the shutdown gate is set, a
    /// drain tick must be a complete no-op — no Phase B sends, no Phase C
    /// spawn — so no drain-path CRDT mutation can land after the pre-ack
    /// owner-state snapshot.
    #[tokio::test]
    async fn drain_lifted_shutdown_gate_skips_tick_zeb703() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        // Entry due for send at wall=2_000 (mirrors the lock-lift test's
        // fixture) — WITHOUT the gate this tick would produce a send.
        let entry = entry_with_age(7, vec![bob], 1_000);
        install_outbox_entry(&mut state, entry);

        let outbox_arc = Arc::new(tokio::sync::Mutex::new(make_outbox_synthetic("dev", alice)));
        let state_arc = Arc::new(tokio::sync::Mutex::new(state));
        let transport = StubTransport::new();
        let app: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink> =
            std::sync::Arc::new(crate::node_event_sink::RecordingSink::new());

        let (gate, phase_c_sem) = outbox_arc.lock().await.shutdown_fence_handles();
        gate.store(true, std::sync::atomic::Ordering::Release);

        super::drain_lifted(
            Arc::clone(&outbox_arc),
            Arc::clone(&state_arc),
            &transport,
            2_000,
            app,
            None, // gate short-circuits before any mutation; engine irrelevant
        )
        .await;

        assert!(
            transport.sends().is_empty(),
            "gated tick must not send (Phase B skipped)"
        );
        // No Phase C task spawned: all fence permits are immediately
        // available (a spawned task would hold one until it completed).
        assert_eq!(
            phase_c_sem.available_permits(),
            DRAIN_PHASE_C_FENCE_CAPACITY,
            "gated tick must not spawn a fenced Phase C task"
        );
        // And the sanity inverse: without the gate the same fixture sends.
        gate.store(false, std::sync::atomic::Ordering::Release);
        let app2: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink> =
            std::sync::Arc::new(crate::node_event_sink::RecordingSink::new());
        super::drain_lifted(
            Arc::clone(&outbox_arc),
            Arc::clone(&state_arc),
            &transport,
            2_000,
            app2,
            None,
        )
        .await;
        assert_eq!(
            transport.sends().len(),
            1,
            "ungated tick with a due entry must send (fixture sanity)"
        );
    }

    /// ZEB-710: Phase-C fence exhaustion (all `DRAIN_PHASE_C_FENCE_CAPACITY`
    /// permits held by wedged tasks) skips the tick's Phase C with a WARN —
    /// the skip must also increment the process-lived
    /// `DM_FENCE_STATS.phase_c_saturated_skips` counter so wedge visibility
    /// is not log-only. Delta-asserted: the counter is process-global.
    #[tokio::test]
    async fn drain_lifted_phase_c_saturation_increments_fence_counter_zeb710() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = entry_with_age(7, vec![bob], 1_000);
        install_outbox_entry(&mut state, entry);

        let outbox_arc = Arc::new(tokio::sync::Mutex::new(make_outbox_synthetic("dev", alice)));
        let state_arc = Arc::new(tokio::sync::Mutex::new(state));
        let transport = StubTransport::new();
        let app: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink> =
            std::sync::Arc::new(crate::node_event_sink::RecordingSink::new());

        // Wedge simulation: hold EVERY fence permit for the whole tick.
        let (_gate, phase_c_sem) = outbox_arc.lock().await.shutdown_fence_handles();
        let held = Arc::clone(&phase_c_sem)
            .acquire_many_owned(DRAIN_PHASE_C_FENCE_CAPACITY as u32)
            .await
            .expect("acquire all fence permits");

        let before = DM_FENCE_STATS.phase_c_saturated_skips();
        super::drain_lifted(
            Arc::clone(&outbox_arc),
            Arc::clone(&state_arc),
            &transport,
            2_000,
            app,
            None,
        )
        .await;
        assert_eq!(
            DM_FENCE_STATS.phase_c_saturated_skips(),
            before + 1,
            "a saturated-fence Phase C skip must increment the counter"
        );
        drop(held);
    }

    /// ZEB-710: `stop_inner`'s fence snapshot degrading to no-fence on a
    /// contended outbox lock must increment
    /// `DM_FENCE_STATS.stop_fence_skipped_contended`; the uncontended path
    /// must not count.
    #[tokio::test]
    async fn stop_fence_snapshot_contended_increments_counter_zeb710() {
        let alice = OwnerAddr([0xaa; 16]);
        let outbox_arc = Arc::new(tokio::sync::Mutex::new(make_outbox_synthetic("dev", alice)));

        let before = DM_FENCE_STATS.stop_fence_skipped_contended();
        {
            let _held = outbox_arc.try_lock().expect("hold for contention");
            assert!(
                DmOutbox::snapshot_shutdown_fence_at_stop(&outbox_arc).is_none(),
                "contended snapshot must degrade to no-fence"
            );
        }
        assert_eq!(
            DM_FENCE_STATS.stop_fence_skipped_contended(),
            before + 1,
            "the contended no-fence degrade must increment the counter"
        );

        assert!(
            DmOutbox::snapshot_shutdown_fence_at_stop(&outbox_arc).is_some(),
            "uncontended snapshot must return the fence handles"
        );
        assert_eq!(
            DM_FENCE_STATS.stop_fence_skipped_contended(),
            before + 1,
            "the uncontended path must not count"
        );
    }

    #[tokio::test]
    async fn drain_lifted_phase_b_reresolves_destinations_after_cache_mutation() {
        // ZEB-233 round 4 (CodeRabbit Trivial): regression test for the
        // Phase B destination-refresh fix. Phase A captures
        // `destinations` into the work unit; Phase B IGNORES that and
        // re-resolves from the CURRENT `owner_device_cache`. A device
        // rotation/revocation between Phase A and Phase B must reach
        // the transport — without this fix, drain would misdeliver to
        // a revoked device hash.
        //
        // Multi-recipient scenario so we can observe a mid-Phase-B
        // mutation: the entry targets [bob, carol]. The custom
        // transport mutates carol's cache entry during bob's send().
        // When the Phase B loop advances to carol, its try_lock +
        // resolve_destinations reads the NEW value. A regression that
        // uses `unit.destinations` (Phase A's snapshot) would observe
        // the OLD value instead.
        use crate::dm_signing::compute_dm_destination_hash;
        use crate::owner_state_types::OwnerDeviceEntry;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::sync::Mutex;

        type Captures = Arc<std::sync::Mutex<Vec<(OwnerAddr, Vec<[u8; 16]>)>>>;

        struct DestinationRefreshProbe {
            state: Arc<Mutex<OwnerState>>,
            captures: Captures,
            rotate_target: OwnerAddr,
            rotate_to: OwnerDeviceEntry,
            triggered: AtomicBool,
        }

        #[async_trait]
        impl DmTransport for DestinationRefreshProbe {
            async fn send(
                &self,
                _entry: &OutboxEntry,
                recipient: OwnerAddr,
                destinations: Vec<[u8; 16]>,
            ) -> Result<(), TransportError> {
                self.captures
                    .lock()
                    .expect("captures poisoned")
                    .push((recipient, destinations));
                if !self.triggered.swap(true, Ordering::SeqCst) {
                    // First send call (bob's). Phase B dropped both
                    // locks before awaiting send(), so state.lock()
                    // acquires cleanly. Mutate carol's cache BEFORE
                    // Phase B's next iteration runs its try_lock +
                    // re-resolve.
                    let mut s = self.state.lock().await;
                    s.owner_device_cache
                        .devices
                        .insert(self.rotate_target, self.rotate_to.clone());
                }
                Ok(())
            }
        }

        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let carol = OwnerAddr([0xcc; 16]);

        let bob_dev = DeviceIdentityHash([0xb1; 16]);
        let carol_dev_old = DeviceIdentityHash([0xc1; 16]);
        let carol_dev_new = DeviceIdentityHash([0xc2; 16]);

        let learned_at = Hlc {
            wall_ms: 0,
            logical: 0,
            device_id: "dev".into(),
        };
        let bob_entry = OwnerDeviceEntry {
            devices: vec![bob_dev],
            device_identity_pubs: vec![Some([0xbb; 64])],
            device_tunnel_contacts: vec![None],
            learned_at: learned_at.clone(),
        };
        let carol_old_entry = OwnerDeviceEntry {
            devices: vec![carol_dev_old],
            device_identity_pubs: vec![Some([0xc1; 64])],
            device_tunnel_contacts: vec![None],
            learned_at: learned_at.clone(),
        };
        let carol_new_entry = OwnerDeviceEntry {
            devices: vec![carol_dev_new],
            device_identity_pubs: vec![Some([0xc2; 64])],
            device_tunnel_contacts: vec![None],
            learned_at,
        };

        let mut state = OwnerState::default();
        state.owner_device_cache.devices.insert(bob, bob_entry);
        state
            .owner_device_cache
            .devices
            .insert(carol, carol_old_entry);
        // Single outbox entry targeting [bob, carol] in that order so
        // Phase A produces work units in that order (recipient_owners
        // is a Vec; drain_phase_a preserves Vec order — see line ~776).
        let entry = entry_with_age(7, vec![bob, carol], 1_000);
        install_outbox_entry(&mut state, entry);

        let outbox = make_outbox_synthetic("dev", alice);
        let outbox_arc = Arc::new(Mutex::new(outbox));
        let state_arc = Arc::new(Mutex::new(state));

        let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
        let transport = DestinationRefreshProbe {
            state: Arc::clone(&state_arc),
            captures: Arc::clone(&captures),
            rotate_target: carol,
            rotate_to: carol_new_entry,
            triggered: AtomicBool::new(false),
        };

        let app: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink> =
            std::sync::Arc::new(crate::node_event_sink::RecordingSink::new());
        super::drain_lifted(
            Arc::clone(&outbox_arc),
            Arc::clone(&state_arc),
            &transport,
            2_000,
            app,
            None, // ZEB-703: durability not exercised here
        )
        .await;

        let captured = captures.lock().expect("captures poisoned");
        assert_eq!(
            captured.len(),
            2,
            "expected send() called for both bob and carol; got {} calls",
            captured.len()
        );
        assert_eq!(
            captured[0],
            (bob, vec![compute_dm_destination_hash(bob_dev.0)]),
            "first send must be bob with bob's cached destination"
        );
        // Load-bearing assertion: carol's send must observe the
        // POST-rotation device hash. A regression that uses
        // `unit.destinations` (Phase A's snapshot) would observe
        // carol_dev_old here.
        assert_eq!(
            captured[1],
            (carol, vec![compute_dm_destination_hash(carol_dev_new.0)]),
            "ZEB-233 round 4 regression: carol's send must reflect the \
             POST-rotation device hash. If this assertion fails with \
             the pre-rotation hash, Phase B is using `unit.destinations` \
             (Phase A snapshot) instead of re-resolving from the \
             current `owner_device_cache`."
        );
    }

    #[tokio::test]
    async fn drain_partial_state_some_recipients_acked() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let carol = OwnerAddr([0xcc; 16]);
        let dave = OwnerAddr([0xdd; 16]);
        let mut entry = entry_with_age(7, vec![bob, carol, dave], 1_000);
        entry.delivered_to.insert(bob);
        entry.delivered_to.insert(carol);
        entry.delivery_status = DeliveryStatus::Partial;
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let transport = StubTransport::new();
        let mut o = make_outbox_synthetic("dev", alice);
        let _ = o.drain(&mut state, &transport, 2_000).await;

        // Only dave is outstanding.
        assert_eq!(transport.sends(), vec![(entry_id, dave)]);
        let stored = state.outbox.get(&entry_id).unwrap();
        assert!(matches!(stored.delivery_status, DeliveryStatus::Partial));
    }

    #[tokio::test]
    async fn drain_respects_backoff_skipping_recently_attempted() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = entry_with_age(7, vec![bob], 1_000);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let transport = StubTransport::new();
        // Pre-seed the first send to fail Transient so backoff is engaged.
        transport.set_outcome(
            entry_id,
            bob,
            Err(TransportError::Transient("net down".into())),
        );

        let mut o = make_outbox_synthetic("dev", alice);
        let _ = o.drain(&mut state, &transport, 10_000).await;
        assert_eq!(
            transport.sends(),
            vec![(entry_id, bob)],
            "first attempt fired"
        );

        // Tick again 1s later — should be skipped (backoff = 5s base).
        let _ = o.drain(&mut state, &transport, 11_000).await;
        assert_eq!(
            transport.sends().len(),
            1,
            "second attempt skipped by backoff"
        );

        // Tick at 16s — past 5s base; should fire.
        let _ = o.drain(&mut state, &transport, 16_000).await;
        assert_eq!(
            transport.sends().len(),
            2,
            "third attempt fired after backoff"
        );
    }

    #[tokio::test]
    async fn drain_expires_30day_old_entry() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = entry_with_age(7, vec![bob], 1_000);
        let entry_id = entry.id;
        let entry_space_id = entry.space_id;
        let entry_message_cid = entry.message_cid.expect("message entry has message_cid");
        install_outbox_entry(&mut state, entry);

        let transport = StubTransport::new();
        let mut o = make_outbox_synthetic("dev", alice);
        // wall_now = created + 30 days + 1s
        let wall_now = 1_000 + EXPIRATION_MS + 1_000;
        let outcome = o.drain(&mut state, &transport, wall_now).await;

        assert_eq!(
            outcome.newly_expired,
            vec![(entry_space_id, entry_message_cid)]
        );
        let stored = state.outbox.get(&entry_id).unwrap();
        assert!(matches!(stored.delivery_status, DeliveryStatus::Expired));
        assert!(
            transport.sends().is_empty(),
            "expired entry should not be re-attempted"
        );
    }

    #[tokio::test]
    async fn drain_complete_entry_is_no_op() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let mut entry = entry_with_age(7, vec![bob], 1_000);
        entry.delivered_to.insert(bob);
        entry.delivery_status = DeliveryStatus::Complete;
        install_outbox_entry(&mut state, entry);

        let transport = StubTransport::new();
        let mut o = make_outbox_synthetic("dev", alice);
        let outcome = o.drain(&mut state, &transport, 2_000).await;

        assert!(outcome.newly_delivered.is_empty());
        assert!(outcome.newly_expired.is_empty());
        assert!(transport.sends().is_empty());
    }

    #[tokio::test]
    async fn drain_in_flight_set_prevents_duplicate_send_within_tick() {
        // Repeat-call drain in a tight pair: first call records the entry as
        // in-flight (the stub's Ok response normally flushes in_flight before
        // returning, but we hold an outstanding fake "no-result-yet" by
        // pre-seeding two recipients on one entry and inspecting the stub
        // sends() vector for duplicates — i.e., one drain call must not send
        // the same (entry, recipient) twice).
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let carol = OwnerAddr([0xcc; 16]);
        let entry = entry_with_age(7, vec![bob, carol], 1_000);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let transport = StubTransport::new();
        let mut o = make_outbox_synthetic("dev", alice);
        let _ = o.drain(&mut state, &transport, 2_000).await;

        let sends = transport.sends();
        let unique: HashSet<(OutboxEntryId, OwnerAddr)> = sends.iter().copied().collect();
        assert_eq!(
            sends.len(),
            unique.len(),
            "no duplicate (entry, recipient) sends in one tick"
        );
        assert_eq!(unique.len(), 2, "exactly one send per recipient");
        let _ = entry_id;
    }

    #[tokio::test]
    async fn drain_throttles_post_ok_send_until_backoff_elapses() {
        // Fix A regression: the prior `Ok(()) => self.backoff.remove(...)`
        // branch let `is_due` return true on the very next 250ms tick,
        // producing tick-rate retry until handle_ack arrived. Verify the
        // post-Ok throttle: install entry, drain at t=0 (1 send), drain
        // 1s later (no new send — under 5s base), drain 6s later (one
        // more send — past 5s base).
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = entry_with_age(7, vec![bob], 0);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let transport = StubTransport::new();
        let mut o = make_outbox_synthetic("dev", alice);

        let _ = o.drain(&mut state, &transport, 0).await;
        assert_eq!(transport.sends().len(), 1, "first attempt fires at t=0");

        let _ = o.drain(&mut state, &transport, 1_000).await;
        assert_eq!(
            transport.sends().len(),
            1,
            "second attempt at t=1s skipped — under 5s base backoff"
        );

        let _ = o.drain(&mut state, &transport, 6_000).await;
        assert_eq!(
            transport.sends().len(),
            2,
            "third attempt at t=6s fires — past 5s base backoff"
        );
        let _ = entry_id;
    }

    #[tokio::test]
    async fn drain_cleans_backoff_for_complete_via_crdt_merge() {
        // Fix C regression: an entry can transition Pending → Complete
        // via CRDT replication (another device acks, owner-state sync
        // merges the OutboxEntry with delivered_to populated). In that
        // path handle_ack is never called locally, so the prior
        // expired-only cleanup leaked the (entry, recipient) backoff
        // and in_flight entries forever. Verify the broader sweep cleans
        // them after a CRDT-merge completion.
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = entry_with_age(7, vec![bob], 1_000);
        let entry_id = entry.id;
        install_outbox_entry(&mut state, entry);

        let transport = StubTransport::new();
        let mut o = make_outbox_synthetic("dev", alice);

        let _ = o.drain(&mut state, &transport, 2_000).await;
        assert_eq!(transport.sends().len(), 1);
        assert_eq!(
            o.backoff_len(),
            1,
            "post-Ok throttle inserted backoff entry (Fix A)"
        );

        // Simulate a peer device's ack replicating through CRDT merge:
        // mutate delivered_to + delivery_status directly (NOT via
        // handle_ack — that path already cleans up).
        {
            let stored = state.outbox.get_mut(&entry_id).unwrap();
            stored.delivered_to.insert(bob);
            stored.delivery_status = DeliveryStatus::Complete;
        }

        let _ = o.drain(&mut state, &transport, 10_000).await;
        assert_eq!(
            o.backoff_len(),
            0,
            "drain cleaned backoff for Complete-via-CRDT entry"
        );
        assert_eq!(
            o.in_flight_len(),
            0,
            "drain cleaned in_flight for Complete-via-CRDT entry"
        );
        assert_eq!(
            transport.sends().len(),
            1,
            "no further sends — entry is Complete"
        );
    }

    #[tokio::test]
    async fn send_dm_self_only_dm_rejects() {
        // Fix D regression: a Space whose members reduces (via
        // `derive_recipients`'s self-exclusion) to an empty list would
        // have minted an OutboxEntry with `recipient_owners: []`, which
        // drain never sent and the expiration sweep would mark Complete
        // via vacuous all-acked truth (`all(|r| ...)` over empty set).
        //
        // The DM invariant in `Space::canonical_invariants` forbids
        // single-member spaces, so we bypass canonicalization by
        // inserting directly into `state.spaces`. This mirrors the
        // shape of a Space that's been corrupted or where `self_owner`
        // is the only remaining valid member (defensive fallback).
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let mut sp = make_dm_space(7, vec![alice, OwnerAddr([0x02; 16])]);
        // Mutate to single-member after construction; insert directly to
        // skip apply_space_with_canonicalization's invariant check.
        sp.members = vec![alice];
        let space_id = sp.id;
        state.spaces.insert(space_id, sp);

        let cas = InMemoryStub::default();
        let mut o = make_outbox_synthetic("dev", alice);
        let err = o
            .send_dm(
                &mut state,
                &cas,
                space_id,
                b"x".to_vec(),
                "text/plain".into(),
                1_000,
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, SendDmError::NoRecipients(id) if id == space_id),
            "expected NoRecipients, got {err:?}"
        );
    }

    #[tokio::test]
    async fn runtime_unicast_transport_send_pushes_signed_event_into_channel() {
        // Synthetic identity_pub trick (per dm_signing.rs's empirical
        // finding that ed25519-dalek doesn't strict-check point membership
        // at construction): all-zero X25519 half + real Ed25519 half.
        // The address_hash for this synthetic input matches what
        // verify_dm_packet_signature will compute, so the
        // SigningKeyDoesNotMatchDeviceHash check passes; the Ed25519
        // signature still verifies under the real verifying key.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(8);
        let signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32]));
        let signing_pub = signing_key.verifying_key();
        let mut identity_pub = [0u8; 64];
        identity_pub[32..].copy_from_slice(signing_pub.as_bytes());
        let our_device = crate::dm_signing::derive_device_hash_from_identity_pub(&identity_pub)
            .expect("synthetic identity_pub should be valid");

        let recipient = OwnerAddr([1; 16]);
        let dest_hash = [0xd1u8; 16];

        let transport = RuntimeUnicastTransport::new(
            tx,
            OwnerAddr([0xff; 16]),
            our_device,
            signing_key.clone(),
        );

        let entry = OutboxEntry {
            id: OutboxEntryId([0xab; 16]),
            space_id: SpaceId([0xcc; 16]),
            recipient_owners: vec![recipient],
            message_cid: Some(ContentId::from_bytes([0xee; 32])),
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "d".into(),
            },
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        };

        transport
            .send(&entry, recipient, vec![dest_hash])
            .await
            .expect("send must succeed");

        let req = rx.recv().await.expect("channel produced no event");
        assert_eq!(req.destination_hash, dest_hash);

        // Decode wire packet → confirm shape + signature verifies.
        let packet = crate::dm_envelope::decode_packet(&req.packet).unwrap();
        match packet {
            crate::dm_envelope::DmPacket::CidNotify {
                signed,
                signature,
                signed_bytes,
            } => {
                assert_eq!(signed.space_id, SpaceId([0xcc; 16]));
                assert_eq!(signed.message_cid, ContentId::from_bytes([0xee; 32]));
                assert_eq!(signed.sender_owner_addr, OwnerAddr([0xff; 16]));
                assert_eq!(signed.signing_device_hash, our_device);
                assert_eq!(
                    signed.sender_devices,
                    vec![our_device],
                    "the test-only RuntimeUnicastTransport ships a single-device \
                     sender_devices (it has no OwnerState to resolve against); the \
                     production builders carry the full cached set — see ZEB-506"
                );
                // Signature must verify against our identity_pub +
                // claimed device hash.
                assert!(crate::dm_signing::verify_dm_packet_signature(
                    &signed_bytes,
                    &signature,
                    &identity_pub,
                    our_device,
                )
                .is_ok());
            }
            other => panic!("expected CidNotify, got {other:?}"),
        }
    }

    /// ZEB-506: a deposit CidNotify must carry the sender's FULL cached device
    /// set (via `resolve_sender_devices`), NOT a bare singleton. The recipient's
    /// ingestion refreshes its `OwnerDeviceCache` for the sender from
    /// `sender_devices` through the LWW-REPLACE `apply_owner_device_update`, so a
    /// singleton would shrink a multi-device sender's cached set down to the
    /// signing device and drop later messages signed by its other devices
    /// (`UnknownSigningKey`). Regression guard for the deposit builder; the
    /// live-tunnel builder (`IrohTunnelDmTransport::send`) shares the same
    /// `resolve_sender_devices` call.
    #[tokio::test]
    async fn build_cidnotify_carries_full_device_set_not_singleton() {
        use crate::owner_state_crdt::OwnerState;

        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let outbox = make_outbox_synthetic("dev", alice);
        let self_owner = outbox.self_owner;

        // Alice is multi-device: the signer plus two siblings, as a friend
        // handshake would have populated on the receiver side.
        let d_signer = outbox.our_signing_device_hash;
        let d2 = DeviceIdentityHash([0xd2; 16]);
        let d3 = DeviceIdentityHash([0xd3; 16]);
        let mut full_set = vec![d_signer, d2, d3];
        let apply = state.apply_owner_device_update(
            self_owner,
            full_set.clone(),
            vec![None, None, None],
            Vec::new(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "seed".into(),
            },
        );
        assert!(
            !matches!(apply, crate::owner_state_crdt::ApplyOutcome::Rejected(_)),
            "seeding the multi-device cache must succeed: {apply:?}"
        );

        let entry = OutboxEntry {
            id: OutboxEntryId([0xab; 16]),
            space_id: SpaceId([0xcc; 16]),
            recipient_owners: vec![OwnerAddr([1; 16])],
            message_cid: Some(ContentId::from_bytes([0xee; 32])),
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "d".into(),
            },
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        };

        let wire = outbox
            .build_cidnotify_packet_bytes(&state, &entry)
            .expect("build cidnotify");
        match crate::dm_envelope::decode_packet(&wire).unwrap() {
            crate::dm_envelope::DmPacket::CidNotify { signed, .. } => {
                full_set.sort();
                assert_eq!(
                    signed.sender_devices, full_set,
                    "deposit CidNotify must carry alice's full multi-device set, not a singleton"
                );
            }
            other => panic!("expected CidNotify, got {other:?}"),
        }
    }

    /// ZEB-506 (Qodo): when the cached device set is already at
    /// `MAX_DEVICES_PER_OWNER` and does NOT contain the signing device,
    /// `resolve_sender_devices` must still cap the result at MAX — the CidNotify
    /// / Invite decoder rejects `sender_devices.len() > MAX_DEVICES_PER_OWNER`
    /// (the packet would be silently dropped) — WHILE keeping the signer, since
    /// the decoder also requires `signer ∈ sender_devices`. The signer here is
    /// chosen to sort ABOVE every cached device, so a naive `truncate(MAX)`
    /// would evict it: this proves we evict a non-signer instead.
    #[test]
    fn resolve_sender_devices_caps_at_max_keeping_signer_when_signer_missing() {
        use crate::owner_state_crdt::OwnerState;
        use crate::owner_state_types::MAX_DEVICES_PER_OWNER;

        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        // Signer sorts above all cached devices (0xff..) and is NOT in the cache.
        let signer = DeviceIdentityHash([0xff; 16]);
        let devices: Vec<DeviceIdentityHash> = (0..MAX_DEVICES_PER_OWNER)
            .map(|i| DeviceIdentityHash([i as u8; 16]))
            .collect();
        let apply = state.apply_owner_device_update(
            alice,
            devices.clone(),
            vec![None; devices.len()],
            Vec::new(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "seed".into(),
            },
        );
        assert!(
            !matches!(apply, crate::owner_state_crdt::ApplyOutcome::Rejected(_)),
            "seeding a full (MAX) device cache must succeed: {apply:?}"
        );
        let cached = &state
            .owner_device_cache
            .devices
            .get(&alice)
            .unwrap()
            .devices;
        assert_eq!(
            cached.len(),
            MAX_DEVICES_PER_OWNER,
            "precondition: cache at MAX"
        );
        assert!(
            !cached.contains(&signer),
            "precondition: signer absent from cache"
        );

        let resolved = resolve_sender_devices(&state, alice, signer);
        assert!(
            resolved.len() <= MAX_DEVICES_PER_OWNER,
            "must re-cap to MAX (decoder rejects len > MAX), got {}",
            resolved.len()
        );
        assert!(
            resolved.contains(&signer),
            "signer must survive the cap (decoder requires signer ∈ sender_devices)"
        );
    }

    /// Empty `destinations` → `TransportError::Transient` so drain bumps
    /// backoff and a future tick (after Flow A surfaces the missing
    /// `OwnerDeviceCache` entry) retries. Replaces the original
    /// resolver-based variant: resolution moved out of the transport,
    /// but the empty-list contract stayed at the transport boundary so
    /// existing drain unit tests (which exercise drain → StubTransport
    /// without populating OwnerDeviceCache) continue to work — only
    /// `RuntimeUnicastTransport` cares about destinations.
    #[tokio::test]
    async fn runtime_unicast_transport_no_known_devices_is_transient_error() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(8);
        let signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32]));
        let our_device = DeviceIdentityHash([0xaa; 16]);

        let transport =
            RuntimeUnicastTransport::new(tx, OwnerAddr([0xff; 16]), our_device, signing_key);

        let entry = OutboxEntry {
            id: OutboxEntryId([0xab; 16]),
            space_id: SpaceId([0xcc; 16]),
            recipient_owners: vec![OwnerAddr([1; 16])],
            message_cid: Some(ContentId::from_bytes([0xee; 32])),
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "d".into(),
            },
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        };

        let err = transport
            .send(&entry, OwnerAddr([1; 16]), Vec::new())
            .await
            .unwrap_err();
        assert!(
            matches!(err, TransportError::Transient(_)),
            "empty destinations must surface as Transient (drives backoff retry), got {err:?}"
        );
    }

    #[tokio::test]
    async fn handle_invite_writes_space_and_cache_with_signing_pub() {
        use crate::owner_state_crdt::OwnerState;

        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let mut outbox = make_outbox_synthetic("device", alice);
        let self_owner = outbox.self_owner;

        // Build a real signed DmInvite via PrivateIdentity::from_seed.
        let private = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);

        let signed = crate::dm_envelope::DmInviteSigned {
            space_id: SpaceId([7; 16]),
            kind: SpaceKind::Dm,
            // self_owner must be in members for handle_invite's sanity gate 3.
            members: vec![OwnerAddr([1; 16]), self_owner],
            inviter: OwnerAddr([1; 16]),
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![device_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice".into(),
            },
            signing_device_hash: device_hash,
            inviter_identity_pub: identity_pub,
            inviter_enrollment: None,
        };

        let body_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&body_bytes);

        // ZEB-236: the accept path now requires the inviter to be an ACTIVE
        // friend (the tier fork). This test's intent is the auto-accept body, so
        // establish the friendship the fork gates on.
        insert_active_friend(&mut state, OwnerAddr([1; 16]));

        outbox
            .handle_invite(&mut state, signed.clone(), signature, &body_bytes, 200)
            .await
            .unwrap();

        // Space written.
        assert!(state.spaces.contains_key(&SpaceId([7; 16])));
        let space = state.spaces.get(&SpaceId([7; 16])).unwrap();
        assert_eq!(space.kind, SpaceKind::Dm);
        assert!(space.content_key.is_some());

        // OwnerDeviceCache updated under invite.inviter.
        let entry = state
            .owner_device_cache
            .devices
            .get(&OwnerAddr([1; 16]))
            .unwrap();
        assert_eq!(entry.devices, vec![device_hash]);
        // Cached pub is at index 0 (the only device, also the signer).
        assert_eq!(entry.device_identity_pubs[0], Some(identity_pub));
    }

    // ── ZEB-236 Task 2: tier fork (auto-accept ⟂ stage) + golden parity ─────────

    /// An invite from an ACTIVE friend keeps Phase 3b's auto-accept: the Space
    /// is written and (refresh=true) the OwnerDeviceCache is seeded.
    #[test]
    fn apply_invite_from_active_friend_auto_accepts() {
        let self_owner = OwnerAddr([0xaa; 16]);
        let mut state = OwnerState::default();
        insert_active_friend(&mut state, OwnerAddr([1; 16]));

        let (signed, signature, body_bytes) = build_valid_dm_invite(self_owner);
        let device_hash = signed.signing_device_hash;
        let identity_pub = signed.inviter_identity_pub;

        let outcome = apply_invite(
            &mut state,
            self_owner,
            "dev",
            signed,
            signature,
            &body_bytes,
            200,
            None,
            true, // refresh=true variant: cache row must be seeded
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
        )
        .unwrap();

        assert!(matches!(outcome, ApplyInviteOutcome::Accepted));
        assert!(state.spaces.contains_key(&SpaceId([7; 16])));
        let entry = state
            .owner_device_cache
            .devices
            .get(&OwnerAddr([1; 16]))
            .expect("cache row present on refresh=true auto-accept");
        assert_eq!(entry.devices, vec![device_hash]);
        assert_eq!(entry.device_identity_pubs[0], Some(identity_pub));
    }

    // ── ZEB-580 S1 (Task 3): cert-anchored #2 DM identity on the invite path ────

    /// ZEB-580 S1 (Task 5) sender-side helper: a `DmOutbox` built from a REAL
    /// minted owner (`mint_owner`, whose enrolled device carries a usable
    /// X25519 pub) so its `enrollment_cert` yields a `Some` #2 DM hash — the
    /// path where DM body signing flips to #2. Returns `(outbox, cert)`; the
    /// cert is the exact one attached to outbound invites. The #3 (Reticulum)
    /// material is a distinct synthetic identity (seed `[0x55; 32]`, mirroring
    /// `make_outbox_synthetic`) so #2 and #3 are provably different keys.
    fn outbox_from_mint() -> (DmOutbox, harmony_owner::certs::EnrollmentCert) {
        let minted = harmony_owner::lifecycle::mint_owner(1_700_000_000).expect("mint");
        let cert = minted.state.enrollments.values().next().unwrap().clone();
        // The enrolled device's #2 signing key (binds to `cert`'s device
        // ed25519) — this becomes the outbox's `community_signing_key`.
        let community_signing_key = std::sync::Arc::new(minted.device_signing_key);
        let self_owner = OwnerAddr(cert.owner_id);

        // Distinct #3 transport identity (never used for DM signing on the #2
        // path — pinned distinct so a regression that signs #3 is caught).
        let private_identity = harmony_identity::PrivateIdentity::from_seed(&[0x55; 32]);
        let priv_bytes = private_identity.to_private_bytes();
        let mut ed_seed = [0u8; 32];
        ed_seed.copy_from_slice(&priv_bytes[32..64]);
        let signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&ed_seed));
        let device_hash = DeviceIdentityHash(private_identity.identity.address_hash);
        let private_identity = std::sync::Arc::new(private_identity);

        let outbox = DmOutbox::new(
            "dev".into(),
            self_owner,
            device_hash,
            signing_key,
            private_identity,
            community_signing_key,
            cert.clone(),
        );
        (outbox, cert)
    }

    /// ZEB-580 S1 (Task 5) sender-side helper: an `OutboxEntry` for a message
    /// (so `build_cidnotify_packet_bytes` produces a CidNotify).
    fn message_entry() -> OutboxEntry {
        OutboxEntry {
            id: OutboxEntryId([0xab; 16]),
            space_id: SpaceId([0xcc; 16]),
            recipient_owners: vec![OwnerAddr([0x0b; 16])],
            message_cid: Some(ContentId::from_bytes([0xee; 32])),
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "d".into(),
            },
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        }
    }

    /// ZEB-580 S1 (Task 5): a `DmOutbox` built from a real (minted) enrollment
    /// cert signs its CidNotify with #2 and stamps the #2 DM hash, verifiable
    /// against the #2 combined pub — NOT the #3 transport key.
    #[test]
    fn dm_outbox_signs_cidnotify_with_device2() {
        let (outbox, cert) = outbox_from_mint();
        let device2_hash = crate::dm_signing::device2_signing_hash(&cert).unwrap();
        assert_eq!(outbox.our_device2_signing_hash, Some(device2_hash));
        // Guard: the #2 hash must differ from the #3 transport hash, so a
        // regression that signs #3 can't accidentally pass the assertions below.
        assert_ne!(device2_hash, outbox.our_signing_device_hash);

        let state = OwnerState::default();
        let entry = message_entry();
        let bytes = outbox
            .build_cidnotify_packet_bytes(&state, &entry)
            .expect("build cidnotify");
        match crate::dm_envelope::decode_packet(&bytes).unwrap() {
            crate::dm_envelope::DmPacket::CidNotify {
                signed,
                signature,
                signed_bytes,
            } => {
                assert_eq!(signed.signing_device_hash, device2_hash);
                crate::dm_signing::verify_dm_packet_signature(
                    &signed_bytes,
                    &signature,
                    &crate::dm_signing::device2_combined_pub(&cert),
                    signed.signing_device_hash,
                )
                .expect("CidNotify must verify against the #2 combined pub");
            }
            other => panic!("expected CidNotify, got {other:?}"),
        }
    }

    /// ZEB-580 S1 (Task 5): a rebuilt bootstrap invite carries
    /// `inviter_enrollment = Some(self cert)` (boxed) and a self-consistent #2
    /// `inviter_identity_pub` / `signing_device_hash`, so an updated receiver
    /// verifies it via the master-attested cert (Task 3 Check B).
    #[test]
    fn dm_outbox_invite_attaches_enrollment_cert() {
        let (outbox, cert) = outbox_from_mint();
        let self_owner = outbox.self_owner;
        let device2_pub = crate::dm_signing::device2_combined_pub(&cert);
        let device2_hash = crate::dm_signing::device2_signing_hash(&cert).unwrap();

        let space_id = SpaceId([7; 16]);
        let mut members = vec![self_owner, OwnerAddr([0x0b; 16])];
        members.sort_by(|a, b| a.0.cmp(&b.0));
        // Seed the DM Space the invite rebuild reads from (id/members/content_key).
        let space_seed = crate::dm_envelope::DmInviteSigned {
            space_id,
            kind: SpaceKind::Dm,
            members,
            inviter: self_owner,
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![device2_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "inviter".into(),
            },
            signing_device_hash: device2_hash,
            inviter_identity_pub: device2_pub,
            inviter_enrollment: None,
        };
        let mut state = OwnerState::default();
        insert_space_from_invite(&mut state, &space_seed);

        let bytes = outbox
            .build_invite_packet_bytes(&state, &space_id)
            .expect("invite rebuild must not error")
            .expect("DM space yields an invite");
        match crate::dm_envelope::decode_packet(&bytes).unwrap() {
            crate::dm_envelope::DmPacket::Invite { signed, .. } => {
                assert_eq!(
                    signed.inviter_enrollment,
                    Some(Box::new(cert)),
                    "invite must attach the sender's own #2 EnrollmentCert (boxed)"
                );
                assert_eq!(
                    signed.inviter_identity_pub, device2_pub,
                    "the #2 path must ship the cert's #2 combined pub inline (self-consistent)"
                );
                assert_eq!(signed.signing_device_hash, device2_hash);
            }
            other => panic!("expected Invite, got {other:?}"),
        }
    }

    /// ZEB-580 S1 (Task 5): the #3 DEGRADE path. A `DmOutbox` built via
    /// `new_synthetic` with a cert whose device X25519 is all-zero (the
    /// `mint_test_owner` shape `make_outbox_synthetic` uses) has
    /// `our_device2_signing_hash == None` and signs its CidNotify with the
    /// legacy #3 transport key / hash. This pins the synthetic-cert fallback
    /// that keeps every existing `make_outbox_synthetic` test byte-stable.
    #[test]
    fn dm_outbox_degrades_to_device3_when_cert_lacks_x25519() {
        let alice = OwnerAddr([0xaa; 16]);
        let outbox = make_outbox_synthetic("dev", alice);
        // The synthetic cert (mint_test_owner) has a zeroed device X25519.
        assert!(
            outbox.our_device2_signing_hash.is_none(),
            "a zeroed-X25519 cert must yield no #2 DM hash (degrade to #3)"
        );

        let state = OwnerState::default();
        let entry = message_entry();
        let bytes = outbox
            .build_cidnotify_packet_bytes(&state, &entry)
            .expect("build cidnotify");
        match crate::dm_envelope::decode_packet(&bytes).unwrap() {
            crate::dm_envelope::DmPacket::CidNotify { signed, .. } => {
                assert_eq!(
                    signed.signing_device_hash, outbox.our_signing_device_hash,
                    "degrade path must stamp the #3 transport hash"
                );
            }
            other => panic!("expected CidNotify, got {other:?}"),
        }
    }

    /// ZEB-580 S1: an invite carrying a valid `inviter_enrollment` (#2 cert)
    /// verifies via the cert's #2 combined pub and caches the #2 DM identity
    /// (not a #3), keyed by the #2 DM hash.
    #[test]
    fn apply_invite_with_cert_caches_device2_identity() {
        use ed25519_dalek::Signer;

        let self_owner = OwnerAddr([0xaa; 16]);
        let minted = harmony_owner::lifecycle::mint_owner(1_700_000_000).expect("mint");
        let cert = minted.state.enrollments.values().next().unwrap().clone();
        let sk2 = minted.device_signing_key; // the enrolled device's #2 key
        let inviter = OwnerAddr(cert.owner_id);
        let device2_pub = crate::dm_signing::device2_combined_pub(&cert);
        let device2_hash = crate::dm_signing::device2_signing_hash(&cert).unwrap();

        let mut state = OwnerState::default();
        let signed =
            build_dm_invite_signed_with_cert(&mut state, self_owner, inviter, cert.clone());
        let signed_bytes = canonical(&signed);
        let signature = sk2.sign(&signed_bytes).to_bytes();

        let out = apply_invite(
            &mut state,
            self_owner,
            "dev",
            signed,
            signature,
            &signed_bytes,
            1_700_000_100,
            Some(inviter),
            true,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
        )
        .expect("apply");
        assert!(matches!(out, ApplyInviteOutcome::Accepted));

        // The cache now holds the #2 combined pub keyed by the #2 DM hash.
        let entry = state.owner_device_cache.devices.get(&inviter).unwrap();
        let idx = entry
            .devices
            .iter()
            .position(|d| *d == device2_hash)
            .unwrap();
        assert_eq!(entry.device_identity_pubs[idx], Some(device2_pub));
    }

    /// ZEB-580 S1: a cert whose `owner_id` does NOT match `signed.inviter` is
    /// rejected — `verify_enrollment_any_issuer`'s owner bind fails and maps to
    /// `SignatureVerificationFailed`. Everything else about the invite is valid
    /// (self-consistent #2 pub/hash/signature, inviter befriended, all sanity
    /// gates pass), so this isolates the cert↔owner binding as the sole reason.
    #[test]
    fn apply_invite_cert_owner_mismatch_rejects() {
        use ed25519_dalek::Signer;

        let self_owner = OwnerAddr([0xaa; 16]);
        let minted = harmony_owner::lifecycle::mint_owner(1_700_000_000).expect("mint");
        let cert = minted.state.enrollments.values().next().unwrap().clone();
        let sk2 = minted.device_signing_key;
        // The claimed inviter is a DIFFERENT owner than the cert enrolls.
        let bogus_inviter = OwnerAddr([0x99; 16]);
        assert_ne!(cert.owner_id, bogus_inviter.0);

        let mut state = OwnerState::default();
        let signed =
            build_dm_invite_signed_with_cert(&mut state, self_owner, bogus_inviter, cert.clone());
        let signed_bytes = canonical(&signed);
        let signature = sk2.sign(&signed_bytes).to_bytes();

        let err = apply_invite(
            &mut state,
            self_owner,
            "dev",
            signed,
            signature,
            &signed_bytes,
            1_700_000_100,
            Some(bogus_inviter),
            true,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
        )
        .expect_err("cert owner mismatch must reject");
        assert!(
            matches!(err, DmReceiveError::SignatureVerificationFailed),
            "expected SignatureVerificationFailed, got {err:?}"
        );
    }

    /// ZEB-580 S1: cert/hash desync — the invite is signed by a FOREIGN device
    /// (self-consistent inline pub/hash/signature, so the legacy
    /// `verify_dm_packet_signature` alone would ACCEPT it) but carries the
    /// victim's cert with `inviter = victim`. The new cert-anchored check
    /// (`device2_signing_hash(cert) != signing_device_hash`) catches the
    /// mismatch and rejects, defeating a forged-sender attack that binds an
    /// attacker's device under the victim's owner.
    #[test]
    fn apply_invite_cert_hash_mismatch_rejects() {
        use ed25519_dalek::Signer;

        let self_owner = OwnerAddr([0xaa; 16]);
        // Victim: the cert we (honestly) attach; its owner is the claimed inviter.
        let victim = harmony_owner::lifecycle::mint_owner(1_700_000_000).expect("mint victim");
        let victim_cert = victim.state.enrollments.values().next().unwrap().clone();
        let victim_owner = OwnerAddr(victim_cert.owner_id);
        // Attacker: a foreign device that actually signs the frame.
        let attacker = harmony_owner::lifecycle::mint_owner(1_700_000_001).expect("mint attacker");
        let attacker_cert = attacker.state.enrollments.values().next().unwrap().clone();
        let attacker_sk2 = attacker.device_signing_key;
        let attacker_pub = crate::dm_signing::device2_combined_pub(&attacker_cert);
        let attacker_hash = crate::dm_signing::device2_signing_hash(&attacker_cert).unwrap();
        let victim_hash = crate::dm_signing::device2_signing_hash(&victim_cert).unwrap();
        assert_ne!(attacker_hash, victim_hash);

        let mut state = OwnerState::default();
        // Base the invite on the victim (cert owner == inviter == befriended),
        // then desync the SIGNING device to the attacker's.
        let mut signed =
            build_dm_invite_signed_with_cert(&mut state, self_owner, victim_owner, victim_cert);
        signed.sender_devices = vec![attacker_hash];
        signed.signing_device_hash = attacker_hash;
        signed.inviter_identity_pub = attacker_pub;
        let signed_bytes = canonical(&signed);
        let signature = attacker_sk2.sign(&signed_bytes).to_bytes();

        let err = apply_invite(
            &mut state,
            self_owner,
            "dev",
            signed,
            signature,
            &signed_bytes,
            1_700_000_100,
            Some(victim_owner),
            true,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
        )
        .expect_err("cert/hash desync must reject");
        assert!(
            matches!(err, DmReceiveError::SigningKeyDoesNotMatchDeviceHash),
            "expected SigningKeyDoesNotMatchDeviceHash, got {err:?}"
        );
    }

    /// ZEB-580 S1: a legacy invite (`inviter_enrollment = None`) still verifies
    /// via the inline #3 pub and caches that #3 identity — the pre-cert path is
    /// unchanged by the Task 3 dual-path fork.
    #[test]
    fn apply_invite_legacy_no_cert_caches_device3() {
        let self_owner = OwnerAddr([0xaa; 16]);
        let mut state = OwnerState::default();
        insert_active_friend(&mut state, OwnerAddr([1; 16]));

        let (signed, signature, body_bytes) = build_valid_dm_invite(self_owner);
        assert!(
            signed.inviter_enrollment.is_none(),
            "legacy fixture must carry no cert"
        );
        let device_hash = signed.signing_device_hash;
        let identity_pub = signed.inviter_identity_pub;

        let out = apply_invite(
            &mut state,
            self_owner,
            "dev",
            signed,
            signature,
            &body_bytes,
            200,
            None,
            true,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
        )
        .expect("apply");
        assert!(matches!(out, ApplyInviteOutcome::Accepted));

        let entry = state
            .owner_device_cache
            .devices
            .get(&OwnerAddr([1; 16]))
            .expect("cache row present on legacy auto-accept");
        let idx = entry
            .devices
            .iter()
            .position(|d| *d == device_hash)
            .unwrap();
        assert_eq!(entry.device_identity_pubs[idx], Some(identity_pub));
    }

    /// ZEB-580 S2: a Space invite carrying a valid `inviter_enrollment` (#2
    /// cert) is dropped BEFORE any Space/cache write when the signer's #2
    /// ed25519 is in the revoked-device projection. Reuses the S1 cert-carrying
    /// fixture from `apply_invite_with_cert_caches_device2_identity` — the only
    /// difference is the `revoked` argument. A clean (empty) projection still
    /// admits the identical invite, isolating the cutoff as the sole cause of
    /// the rejection.
    #[test]
    fn apply_invite_from_revoked_device2_is_cut_off() {
        use ed25519_dalek::Signer;

        let self_owner = OwnerAddr([0xaa; 16]);
        let minted = harmony_owner::lifecycle::mint_owner(1_700_000_000).expect("mint");
        let cert = minted.state.enrollments.values().next().unwrap().clone();
        let sk2 = minted.device_signing_key; // the enrolled device's #2 key
        let inviter = OwnerAddr(cert.owner_id);
        let ed25519: [u8; 32] = crate::dm_signing::device2_combined_pub(&cert)[32..64]
            .try_into()
            .unwrap();

        let mut state = OwnerState::default();
        let signed =
            build_dm_invite_signed_with_cert(&mut state, self_owner, inviter, cert.clone());
        let space_id = signed.space_id;
        let signed_bytes = canonical(&signed);
        let signature = sk2.sign(&signed_bytes).to_bytes();

        // Clean (empty) projection: the identical invite is still admitted —
        // isolates the revoked projection as the sole cause of the rejection
        // asserted below.
        let clean = crate::revoked_device_projection::RevokedDeviceProjection::new();
        let out = apply_invite(
            &mut state.clone(),
            self_owner,
            "dev",
            signed.clone(),
            signature,
            &signed_bytes,
            1_700_000_100,
            Some(inviter),
            true,
            &clean,
        )
        .expect("clean projection must not cut off the invite");
        assert!(matches!(out, ApplyInviteOutcome::Accepted));

        // Revoked projection: the SAME invite must now be cut off before any
        // Space/cache write. `state` here is still pristine (only the
        // `insert_active_friend` from `build_dm_invite_signed_with_cert` — the
        // call above ran against a clone).
        let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
        revoked.union_from_members(std::iter::once((
            inviter,
            &std::collections::BTreeSet::from([ed25519]),
        )));
        let err = apply_invite(
            &mut state,
            self_owner,
            "dev",
            signed,
            signature,
            &signed_bytes,
            1_700_000_100,
            Some(inviter),
            true,
            &revoked,
        )
        .expect_err("invite from a revoked device must be cut off");
        assert_eq!(err, DmReceiveError::SignerDeviceRevoked);
        assert!(
            !state.spaces.contains_key(&space_id),
            "no Space must be written for a cut-off invite"
        );
        assert!(
            !state.owner_device_cache.devices.contains_key(&inviter),
            "no cache entry must be written for the cut-off inviter"
        );
    }

    /// ZEB-580 S2: a legacy invite (`inviter_enrollment = None`, #3 signer) is
    /// still admitted even against a NON-EMPTY revoked-device projection, as
    /// long as the signer's own key is not a member of it. This pins the
    /// boundary the cutoff can actually see: `apply_invite`'s check is a plain
    /// `(owner, ed25519) ∈ revoked` membership test with no #2-vs-#3 branch, so
    /// it is a no-op for any signer whose key was never enrolled (and thus
    /// never appears in a real projection) — this test proves that boundary
    /// with an UNRELATED key in the set, not the #3 signer's own key.
    #[test]
    fn apply_invite_legacy_no_cert_not_subject_to_cutoff() {
        let self_owner = OwnerAddr([0xaa; 16]);
        let mut state = OwnerState::default();
        insert_active_friend(&mut state, OwnerAddr([1; 16]));

        let (signed, signature, body_bytes) = build_valid_dm_invite(self_owner);
        assert!(
            signed.inviter_enrollment.is_none(),
            "legacy fixture must carry no cert"
        );

        // Non-empty projection for the same inviter, but keyed to an unrelated
        // key — the legacy signer's own key is never in it.
        let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
        revoked.union_from_members(std::iter::once((
            OwnerAddr([1; 16]),
            &std::collections::BTreeSet::from([[0x99u8; 32]]),
        )));

        let out = apply_invite(
            &mut state,
            self_owner,
            "dev",
            signed,
            signature,
            &body_bytes,
            200,
            None,
            true,
            &revoked,
        )
        .expect("a signer whose key isn't in the revoked set must be admitted");
        assert!(matches!(out, ApplyInviteOutcome::Accepted));
    }

    /// An invite from a NON-friend is staged and writes NOTHING to owner state
    /// (spec: staging is process-local only; decline == offline).
    #[test]
    fn apply_invite_from_non_friend_stages_and_writes_nothing() {
        let self_owner = OwnerAddr([0xaa; 16]);
        let mut state = OwnerState::default(); // friend_graph EMPTY
        let before = crate::owner_state_persist::canonicalize(&state).unwrap();

        let (signed, signature, body_bytes) = build_valid_dm_invite(self_owner);
        let outcome = apply_invite(
            &mut state,
            self_owner,
            "dev",
            signed,
            signature,
            &body_bytes,
            4242,
            None,
            true,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
        )
        .unwrap();

        match outcome {
            ApplyInviteOutcome::Staged(s) => {
                assert_eq!(s.signed.space_id, SpaceId([7; 16]));
                assert_eq!(s.received_at_ms, 4242, "staged at the passed wall clock");
                assert!(
                    s.refresh_owner_device_cache,
                    "staged invite carries the ingest route's cache entitlement"
                );
            }
            other => panic!("expected Staged for a non-friend inviter, got {other:?}"),
        }

        let after = crate::owner_state_persist::canonicalize(&state).unwrap();
        assert_eq!(
            before, after,
            "staging a non-friend invite must write NOTHING"
        );
    }

    /// ZEB-639 (1): a NON-friend invite for a space that ALREADY EXISTS
    /// locally is IGNORED — never staged, never prompted. Staging it is the
    /// kicked-GroupDm co-member re-admit vector (forged fresh invite for the
    /// existing space_id). The call must also write nothing.
    #[test]
    fn non_friend_invite_for_existing_space_is_ignored_not_staged() {
        let self_owner = OwnerAddr([0xaa; 16]);
        let mut state = OwnerState::default(); // friend_graph EMPTY → non-friend tier

        // Arrange: the invite's target Space already exists locally.
        let (signed, _sig, _bytes) = build_valid_dm_invite(self_owner);
        insert_space_from_invite(&mut state, &signed);
        let before = crate::owner_state_persist::canonicalize(&state).unwrap();

        // Act: apply the (byte-identical, deterministic) invite from a
        // NON-friend inviter for that same space_id.
        let (signed, signature, body_bytes) = build_valid_dm_invite(self_owner);
        let outcome = apply_invite(
            &mut state,
            self_owner,
            "dev",
            signed,
            signature,
            &body_bytes,
            4242,
            None,
            true,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
        )
        .unwrap();

        assert!(
            matches!(outcome, ApplyInviteOutcome::IgnoredExistingSpace),
            "non-friend invite for an existing space must be ignored, got {outcome:?}"
        );
        let after = crate::owner_state_persist::canonicalize(&state).unwrap();
        assert_eq!(
            before, after,
            "ignoring an existing-space invite must write NOTHING"
        );
    }

    /// ZEB-642 (3): a TOMBSTONED space is NOT in `state.spaces`, so a
    /// non-friend invite for it still STAGES (consent re-asked; accept
    /// later surfaces the permanent tombstone rejection). Pins the
    /// `spaces.contains_key` gate comment in `apply_invite`.
    #[test]
    fn non_friend_invite_for_tombstoned_space_still_stages() {
        let self_owner = OwnerAddr([0xaa; 16]);
        let mut state = OwnerState::default(); // friend_graph EMPTY → non-friend tier

        // Arrange: the invite's target space is tombstoned (removed from
        // `spaces`, held only in `tombstones`).
        state.tombstone_space(SpaceId([7; 16]));
        let before = crate::owner_state_persist::canonicalize(&state).unwrap();

        let (signed, signature, body_bytes) = build_valid_dm_invite(self_owner);
        let outcome = apply_invite(
            &mut state,
            self_owner,
            "dev",
            signed,
            signature,
            &body_bytes,
            4242,
            None,
            true,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
        )
        .unwrap();

        assert!(
            matches!(outcome, ApplyInviteOutcome::Staged(_)),
            "tombstoned space must still stage, got {outcome:?}"
        );
        let after = crate::owner_state_persist::canonicalize(&state).unwrap();
        assert_eq!(before, after, "staging must write NOTHING");
    }

    /// ZEB-639 (1): the friend tier is NOT gated on space existence — an
    /// ACTIVE-friend invite for an existing space still runs the accept tail
    /// (the established idempotent redelivery-merge contract; ZEB-483
    /// co-deposits the invite with every message).
    #[test]
    fn friend_invite_for_existing_space_still_accepts_redelivery_merge() {
        let self_owner = OwnerAddr([0xaa; 16]);
        let inviter = OwnerAddr([1; 16]);
        let mut state = OwnerState::default();
        insert_active_friend(&mut state, inviter);

        // Arrange: the invite's target Space already exists locally
        // (redelivery: a prior accept already wrote it).
        let (signed, _sig, _bytes) = build_valid_dm_invite(self_owner);
        insert_space_from_invite(&mut state, &signed);

        let (signed, signature, body_bytes) = build_valid_dm_invite(self_owner);
        let outcome = apply_invite(
            &mut state,
            self_owner,
            "dev",
            signed,
            signature,
            &body_bytes,
            200,
            None,
            true,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
        )
        .unwrap();

        assert!(
            matches!(outcome, ApplyInviteOutcome::Accepted),
            "active-friend redelivery for an existing space must still accept, got {outcome:?}"
        );
        assert!(state.spaces.contains_key(&SpaceId([7; 16])));
    }

    /// Golden parity: the staged-then-accepted path (`apply_invite` → `Staged` →
    /// `run_invite_accept_tail`) produces byte-identical owner state to the
    /// inline auto-accept — for BOTH `refresh_owner_device_cache` variants. The
    /// only intended divergence is the friendship that GATED auto-accept (A
    /// carries it, the deferred path never befriends), so it is stripped from A
    /// before comparison to isolate the accept TAIL's effect.
    #[test]
    fn staged_then_accept_tail_matches_direct_auto_accept_golden() {
        let no_revocations = crate::revoked_device_projection::RevokedDeviceProjection::new();
        for refresh in [true, false] {
            let self_owner = OwnerAddr([0xaa; 16]);
            let inviter = OwnerAddr([1; 16]);

            // A: inviter is an ACTIVE friend → apply_invite auto-accepts inline.
            let mut a = OwnerState::default();
            insert_active_friend(&mut a, inviter);
            let (signed_a, sig_a, bytes_a) = build_valid_dm_invite(self_owner);
            let out_a = apply_invite(
                &mut a,
                self_owner,
                "dev",
                signed_a,
                sig_a,
                &bytes_a,
                777,
                None,
                refresh,
                &no_revocations,
            )
            .unwrap();
            assert!(matches!(out_a, ApplyInviteOutcome::Accepted));

            // B: inviter NOT a friend → apply_invite stages; the deferred accept
            // runs the extracted tail with EXACTLY the inputs Staged carries.
            let mut b = OwnerState::default();
            let (signed_b, sig_b, bytes_b) = build_valid_dm_invite(self_owner);
            let staged = match apply_invite(
                &mut b,
                self_owner,
                "dev",
                signed_b,
                sig_b,
                &bytes_b,
                777,
                None,
                refresh,
                &no_revocations,
            )
            .unwrap()
            {
                ApplyInviteOutcome::Staged(s) => s,
                other => panic!("expected Staged, got {other:?}"),
            };
            // Legacy fixture (no cert): the resolved signer pub is the inline
            // #3 pub — exactly what apply_invite's inline auto-accept resolves,
            // so the two legs stay byte-identical. Capture before the move.
            let deferred_signer_pub = staged.signed.inviter_identity_pub;
            run_invite_accept_tail(
                &mut b,
                "dev",
                staged.signed,
                777,
                staged.refresh_owner_device_cache,
                deferred_signer_pub,
            )
            .unwrap();

            // Neutralize the one intended difference (the consent-gate friendship).
            a.friend_graph.friends.remove(&inviter);

            assert_eq!(
                crate::owner_state_persist::canonicalize(&a).unwrap(),
                crate::owner_state_persist::canonicalize(&b).unwrap(),
                "staged-then-accept tail must be byte-identical to inline \
                 auto-accept (refresh_owner_device_cache = {refresh})"
            );
        }
    }

    /// ZEB-639 (2): a forged far-future `created_at` on an accepted invite
    /// must NOT become the Space's LWW driver. `lww_merge_space` is
    /// LWW-by-`updated_at` and GroupDm dedupe_key is id-derived, so echoing
    /// the invite-controlled HLC would pin the Space against every future
    /// legitimate update — the same denial-of-updates attack the cache
    /// `learned_at` rule already defeats. The tail must clamp `updated_at`
    /// to the local clock; `created_at` keeps the claimed value (provenance
    /// and display only, never LWW).
    #[test]
    fn forged_far_future_created_at_is_clamped_on_accept() {
        let self_owner = OwnerAddr([0xaa; 16]);
        let mut state = OwnerState::default(); // friend_graph EMPTY → non-friend tier

        let (signed, signature, body_bytes) = build_far_future_dm_invite(self_owner);
        let forged_created_at = signed.created_at.clone();
        let wall_now_ms = 5_000u64;

        // Non-friend invite → Staged; the deferred user-accept then runs the
        // tail with EXACTLY the inputs Staged carries (mirrors the golden-
        // parity test's deferred leg).
        let staged = match apply_invite(
            &mut state,
            self_owner,
            "dev",
            signed,
            signature,
            &body_bytes,
            wall_now_ms,
            None,
            true,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
        )
        .unwrap()
        {
            ApplyInviteOutcome::Staged(s) => s,
            other => panic!("expected Staged, got {other:?}"),
        };
        let deferred_signer_pub = staged.signed.inviter_identity_pub;
        run_invite_accept_tail(
            &mut state,
            "dev",
            staged.signed,
            wall_now_ms,
            staged.refresh_owner_device_cache,
            deferred_signer_pub,
        )
        .unwrap();

        let space = state
            .spaces
            .get(&SpaceId([7; 16]))
            .expect("accepted invite must write the Space");
        assert_eq!(
            space.updated_at.wall_ms, wall_now_ms,
            "updated_at must be clamped to the local wall clock, not echo the forged HLC"
        );
        assert_eq!(
            space.updated_at.device_id, "dev",
            "clamped updated_at must carry the LOCAL device_id, not the invite's"
        );
        assert_eq!(
            space.created_at, forged_created_at,
            "created_at keeps the claimed value — provenance/display, not LWW"
        );
    }

    /// ZEB-639 (2): after the clamped accept, a later legitimate Space update
    /// (`updated_at.wall_ms = wall_now_ms + 1`) must WIN the LWW merge and be
    /// visible — the point of the clamp. Pre-clamp, the forged `u64::MAX / 2`
    /// HLC silently kept the old state (a `Merged` outcome whose field change
    /// never lands).
    #[test]
    fn legit_update_wins_lww_after_clamped_accept() {
        let self_owner = OwnerAddr([0xaa; 16]);
        let mut state = OwnerState::default(); // friend_graph EMPTY → non-friend tier

        let (signed, signature, body_bytes) = build_far_future_dm_invite(self_owner);
        let wall_now_ms = 5_000u64;
        let staged = match apply_invite(
            &mut state,
            self_owner,
            "dev",
            signed,
            signature,
            &body_bytes,
            wall_now_ms,
            None,
            true,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
        )
        .unwrap()
        {
            ApplyInviteOutcome::Staged(s) => s,
            other => panic!("expected Staged, got {other:?}"),
        };
        let deferred_signer_pub = staged.signed.inviter_identity_pub;
        run_invite_accept_tail(
            &mut state,
            "dev",
            staged.signed,
            wall_now_ms,
            staged.refresh_owner_device_cache,
            deferred_signer_pub,
        )
        .unwrap();

        // A later legitimate update: newer local HLC + a visible field change.
        let mut update = state
            .spaces
            .get(&SpaceId([7; 16]))
            .expect("accepted invite must write the Space")
            .clone();
        update.updated_at = Hlc {
            wall_ms: wall_now_ms + 1,
            logical: 0,
            device_id: "dev".into(),
        };
        update.custom_name = Some("renamed".into());

        let outcome = state.apply_space_with_canonicalization(update);
        assert!(
            !matches!(outcome, crate::owner_state_crdt::ApplyOutcome::Rejected(_)),
            "later legit update must not be rejected, got {outcome:?}"
        );
        assert_eq!(
            state
                .spaces
                .get(&SpaceId([7; 16]))
                .unwrap()
                .custom_name
                .as_deref(),
            Some("renamed"),
            "the legit update's field change must be visible after LWW merge"
        );
    }

    /// The reinstated spec test: declining a staged invite writes no state.
    /// Decline == drop the `StagedDmInvite` (all decline does at this layer).
    #[test]
    fn decline_writes_no_state() {
        let self_owner = OwnerAddr([0xaa; 16]);
        let mut state = OwnerState::default();
        let before = crate::owner_state_persist::canonicalize(&state).unwrap();

        let (signed, signature, body_bytes) = build_valid_dm_invite(self_owner);
        let outcome = apply_invite(
            &mut state,
            self_owner,
            "dev",
            signed,
            signature,
            &body_bytes,
            900,
            None,
            true,
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
        )
        .unwrap();
        let staged = match outcome {
            ApplyInviteOutcome::Staged(s) => s,
            other => panic!("expected Staged, got {other:?}"),
        };
        drop(staged); // decline: the invite is simply dropped, never applied

        let after = crate::owner_state_persist::canonicalize(&state).unwrap();
        assert_eq!(
            before, after,
            "decline must leave owner state byte-identical"
        );
    }

    #[tokio::test]
    async fn handle_invite_uses_local_wall_now_ms_for_cache_lww_not_remote_created_at() {
        // SECURITY regression: handle_invite previously fed
        // `signed.created_at` (attacker-controlled remote HLC) into
        // `apply_owner_device_update` as the LWW timestamp. A forged
        // far-future HLC (e.g., wall_ms = u64::MAX / 2) on a single
        // malicious invite would pin the local cache and reject every
        // legitimate future update from the same owner as `StaleHlc`
        // — a denial-of-updates attack.
        //
        // The fix uses our local `wall_now_ms` + `self.device_id` to
        // build the LWW HLC (mirroring `verify_cidnotify_admission`). The
        // assertion: after handle_invite, the cache entry's
        // `learned_at.wall_ms` MUST be `wall_now_ms`, NOT the remote
        // far-future value.
        use crate::owner_state_crdt::OwnerState;

        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let mut outbox = make_outbox_synthetic("local-dev", alice);
        let self_owner = outbox.self_owner;

        let private = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);

        // Forged far-future HLC — under the bug, this would be written
        // into the cache and lock out legitimate updates.
        let attacker_hlc = Hlc {
            wall_ms: u64::MAX / 2,
            logical: 0,
            device_id: "attacker".into(),
        };

        let signed = crate::dm_envelope::DmInviteSigned {
            space_id: SpaceId([7; 16]),
            kind: SpaceKind::Dm,
            members: vec![OwnerAddr([1; 16]), self_owner],
            inviter: OwnerAddr([1; 16]),
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![device_hash],
            created_at: attacker_hlc.clone(),
            signing_device_hash: device_hash,
            inviter_identity_pub: identity_pub,
            inviter_enrollment: None,
        };
        let body_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&body_bytes);

        // ZEB-236: accept path requires an ACTIVE-friend inviter (tier fork).
        insert_active_friend(&mut state, OwnerAddr([1; 16]));

        let local_wall_now_ms: u64 = 12345;
        outbox
            .handle_invite(
                &mut state,
                signed,
                signature,
                &body_bytes,
                local_wall_now_ms,
            )
            .await
            .unwrap();

        let entry = state
            .owner_device_cache
            .devices
            .get(&OwnerAddr([1; 16]))
            .expect("cache entry must exist after handle_invite");
        assert_eq!(
            entry.learned_at.wall_ms, local_wall_now_ms,
            "cache LWW HLC MUST use local wall_now_ms, NOT attacker-controlled created_at"
        );
        assert_ne!(
            entry.learned_at.wall_ms, attacker_hlc.wall_ms,
            "cache LWW HLC MUST NOT echo the remote far-future timestamp"
        );
        assert_eq!(
            entry.learned_at.device_id, "local-dev",
            "cache LWW HLC device_id MUST be OUR device_id"
        );
    }

    #[tokio::test]
    async fn handle_invite_binds_inviter_field_not_members_zero() {
        // Group-DM where invite.inviter is the lex-LARGEST member (so
        // members[0] is a different OwnerAddr). Cache entry must be created
        // under invite.inviter, NOT members[0]. Regression for the lex-vs-
        // inviter binding bug surfaced in spec §"Application-signature
        // binding rule".
        use crate::owner_state_crdt::OwnerState;
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let mut outbox = make_outbox_synthetic("device", alice);
        let self_owner = outbox.self_owner;

        let private = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);

        let inviter_addr = OwnerAddr([0xff; 16]); // lex-largest
        let signed = crate::dm_envelope::DmInviteSigned {
            space_id: SpaceId([7; 16]),
            kind: SpaceKind::GroupDm,
            // self_owner must appear so handle_invite's sanity gate 3 passes.
            members: vec![OwnerAddr([1; 16]), self_owner, inviter_addr], // sorted ascending
            inviter: inviter_addr,
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![device_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice".into(),
            },
            signing_device_hash: device_hash,
            inviter_identity_pub: identity_pub,
            inviter_enrollment: None,
        };
        let body_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&body_bytes);

        // ZEB-236: accept path requires an ACTIVE-friend inviter (tier fork).
        insert_active_friend(&mut state, inviter_addr);

        outbox
            .handle_invite(&mut state, signed, signature, &body_bytes, 200)
            .await
            .unwrap();

        // Cache entry under inviter_addr, NOT members[0].
        assert!(state.owner_device_cache.devices.contains_key(&inviter_addr));
        assert!(!state
            .owner_device_cache
            .devices
            .contains_key(&OwnerAddr([1; 16])));
    }

    #[tokio::test]
    async fn handle_invite_inviter_not_in_members_drops() {
        use crate::owner_state_crdt::OwnerState;
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let mut outbox = make_outbox_synthetic("device", alice);

        let private = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);

        let signed = crate::dm_envelope::DmInviteSigned {
            space_id: SpaceId([7; 16]),
            kind: SpaceKind::Dm,
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            inviter: OwnerAddr([3; 16]), // NOT in members
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![device_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice".into(),
            },
            signing_device_hash: device_hash,
            inviter_identity_pub: identity_pub,
            inviter_enrollment: None,
        };
        let body_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&body_bytes);

        let err = outbox
            .handle_invite(&mut state, signed, signature, &body_bytes, 200)
            .await
            .unwrap_err();
        assert!(matches!(err, DmReceiveError::InviterNotInMembers));
        assert!(!state.spaces.contains_key(&SpaceId([7; 16])));
        assert!(state.owner_device_cache.devices.is_empty());
    }

    #[tokio::test]
    async fn handle_invite_signing_device_not_in_sender_devices_drops() {
        use crate::owner_state_crdt::OwnerState;
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let mut outbox = make_outbox_synthetic("device", alice);

        let private = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);

        // Construct an invite where signing_device_hash is NOT in
        // sender_devices. The sanity gate must reject before signature
        // verification even runs.
        let signed = crate::dm_envelope::DmInviteSigned {
            space_id: SpaceId([7; 16]),
            kind: SpaceKind::Dm,
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            inviter: OwnerAddr([1; 16]),
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![DeviceIdentityHash([0xab; 16])], // does NOT include device_hash
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice".into(),
            },
            signing_device_hash: device_hash,
            inviter_identity_pub: identity_pub,
            inviter_enrollment: None,
        };

        // NOTE: decode_packet would reject this packet as
        // DecodeError::Invalid (the same invariant). Because we're calling
        // handle_invite directly with a hand-constructed DmInviteSigned
        // that bypasses decode, this test exercises the defense-in-depth
        // gate inside handle_invite. In production the packet would never
        // reach handle_invite — it'd drop at decode_packet — but the gate
        // catches future regressions if decode_packet's invariant is ever
        // loosened.
        let body_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&body_bytes);

        let err = outbox
            .handle_invite(&mut state, signed, signature, &body_bytes, 200)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            DmReceiveError::SigningDeviceNotInSenderDevices
        ));
        assert!(!state.spaces.contains_key(&SpaceId([7; 16])));
    }

    #[tokio::test]
    async fn handle_invite_receiver_not_in_members_drops() {
        use crate::owner_state_crdt::OwnerState;
        let mut state = OwnerState::default();
        // self_owner NOT in invite.members.
        let alice = OwnerAddr([0xaa; 16]);
        let mut outbox = make_outbox_synthetic("device", alice);

        let private = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);

        let signed = crate::dm_envelope::DmInviteSigned {
            space_id: SpaceId([7; 16]),
            kind: SpaceKind::Dm,
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])], // self_owner not here
            inviter: OwnerAddr([1; 16]),
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![device_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice".into(),
            },
            signing_device_hash: device_hash,
            inviter_identity_pub: identity_pub,
            inviter_enrollment: None,
        };
        let body_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&body_bytes);

        let err = outbox
            .handle_invite(&mut state, signed, signature, &body_bytes, 200)
            .await
            .unwrap_err();
        assert!(matches!(err, DmReceiveError::ReceiverNotInMembers));
        assert!(!state.spaces.contains_key(&SpaceId([7; 16])));
    }

    #[tokio::test]
    async fn handle_invite_tampered_signature_drops() {
        use crate::owner_state_crdt::OwnerState;
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let mut outbox = make_outbox_synthetic("device", alice);
        let self_owner = outbox.self_owner;

        let private = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);

        let signed = crate::dm_envelope::DmInviteSigned {
            space_id: SpaceId([7; 16]),
            kind: SpaceKind::Dm,
            members: vec![OwnerAddr([1; 16]), self_owner],
            inviter: OwnerAddr([1; 16]),
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![device_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice".into(),
            },
            signing_device_hash: device_hash,
            inviter_identity_pub: identity_pub,
            inviter_enrollment: None,
        };
        let body_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let mut signature = private.sign(&body_bytes);
        // Flip a bit in the signature.
        signature[0] ^= 0xff;

        let err = outbox
            .handle_invite(&mut state, signed, signature, &body_bytes, 200)
            .await
            .unwrap_err();
        assert!(
            matches!(err, DmReceiveError::SignatureVerificationFailed),
            "expected SignatureVerificationFailed, got {:?}",
            err
        );
        assert!(!state.spaces.contains_key(&SpaceId([7; 16])));
    }

    /// CodeRabbit F2: an invite whose Space fails its Phase-1 invariants (here a
    /// `Dm`-kind Space carrying 3 members — DMs require exactly 2) must leave the
    /// `OwnerDeviceCache` UNCHANGED. The Space apply now runs BEFORE the cache
    /// write, so a rejected Space returns `Err` before any trust-state mutation —
    /// a malformed/rejected invite can no longer seed the inviter's device cache.
    #[tokio::test]
    async fn apply_invite_rejected_space_leaves_owner_device_cache_unchanged() {
        use crate::owner_state_crdt::OwnerState;
        let mut state = OwnerState::default();

        let alice = OwnerAddr([0xaa; 16]);
        let outbox = make_outbox_synthetic("device", alice);
        let self_owner = outbox.self_owner;

        let private = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);

        let inviter = OwnerAddr([0x01; 16]);
        // A 3-member `Dm` Space — fails `validate_invariants` ("dm must have
        // exactly 2 distinct members"). Members are sorted ascending and include
        // both inviter and self_owner so apply_invite's sanity gates 1+3 pass and
        // the Space apply is the ONLY thing that rejects.
        let mut members = vec![inviter, self_owner, OwnerAddr([0xee; 16])];
        members.sort();
        let signed = crate::dm_envelope::DmInviteSigned {
            space_id: SpaceId([7; 16]),
            kind: SpaceKind::Dm,
            members,
            inviter,
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![device_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice".into(),
            },
            signing_device_hash: device_hash,
            inviter_identity_pub: identity_pub,
            inviter_enrollment: None,
        };
        let body_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&body_bytes);

        // ZEB-236: make the inviter an ACTIVE friend so the tier fork takes the
        // accept path and reaches the Space apply — which is what this test
        // asserts REJECTS (3-member Dm). Without the friendship the invite would
        // Stage before ever reaching the Space invariant check, masking the F2
        // guard this test pins. Friendship touches only `friend_graph`, so the
        // "cache untouched" assertion below is unaffected.
        insert_active_friend(&mut state, inviter);

        // `None` for expected_inviter isolates the F2 (cache-on-reject) concern
        // from the F1 inviter-bind gate. The Space apply is what rejects here.
        let err = apply_invite(
            &mut state,
            self_owner,
            &outbox.device_id,
            signed,
            signature,
            &body_bytes,
            200,
            None,
            true, // F2 guard: cache write is sequenced after the Space apply (which rejects first)
            &crate::revoked_device_projection::RevokedDeviceProjection::new(),
        )
        .unwrap_err();
        assert!(
            matches!(err, DmReceiveError::CrdtRejected(_)),
            "a 3-member Dm Space must be rejected by apply_space_with_canonicalization, got {:?}",
            err
        );

        // The Space was never written...
        assert!(
            !state.spaces.contains_key(&SpaceId([7; 16])),
            "rejected Space must not be persisted"
        );
        // ...and the inviter's device cache was NEVER touched (F2: cache write is
        // sequenced AFTER a successful Space apply).
        assert!(
            !state.owner_device_cache.devices.contains_key(&inviter),
            "a Space-rejected invite must NOT mutate the OwnerDeviceCache"
        );
    }

    // ── Phase 3b Task 11: handle_ack tests ──────────────────────────────
    // (Named `handle_unicast_ack_*` historically; renamed when the unused
    // `handle_unicast` demux was deleted, ZEB-710. They drive `handle_ack`
    // directly. NOTE: there is no live inbound-DmAck route today — the
    // tunnel rejects Ack (`ingest_dm_packet_rejects_an_ack_packet`);
    // delivery confirmation flows through the deposit-ack sweep via
    // `mark_ack_delivered`.)

    /// Build the standard sender-side ack-receive fixture: self-owner Alice
    /// (the outbox's self_owner) has previously sent a DM to Bob and the
    /// OutboxEntry is still Pending. Bob's signing identity is pre-seeded
    /// into Alice's `OwnerDeviceCache.devices[bob].device_identity_pubs`.
    /// Returns (state, signed_ack, signature, signed_bytes, outbox_entry_id).
    #[allow(clippy::type_complexity)]
    fn build_handle_ack_fixture(
        alice: OwnerAddr,
        bob: OwnerAddr,
        space_id: SpaceId,
        message_cid: ContentId,
    ) -> (
        OwnerState,
        crate::dm_envelope::DmAckSigned,
        [u8; 64],
        Vec<u8>,
        OutboxEntryId,
    ) {
        let mut state = OwnerState::default();
        let private_bob = harmony_identity::PrivateIdentity::from_seed(&[0x77; 32]);
        let bob_pub_id = private_bob.public_identity();
        let bob_identity_pub = bob_pub_id.to_public_bytes();
        let bob_device_hash = DeviceIdentityHash(bob_pub_id.address_hash);

        // Pre-seed Alice's view of Bob in OwnerDeviceCache (post-bootstrap).
        state.apply_owner_device_update(
            bob,
            vec![bob_device_hash],
            vec![Some(bob_identity_pub)],
            vec![],
            Hlc {
                wall_ms: 50,
                logical: 0,
                device_id: "bob-dev".into(),
            },
        );

        // Install Alice's pending OutboxEntry — destined to bob.
        let entry_id = OutboxEntryId([0x77; 16]);
        let entry = OutboxEntry {
            id: entry_id,
            space_id,
            recipient_owners: vec![bob],
            message_cid: Some(message_cid),
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "alice-dev".into(),
            },
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        };
        match state.apply_outbox(entry) {
            ApplyOutcome::Inserted => {}
            other => panic!("fixture install failed: {other:?}"),
        }
        let _ = alice; // self_owner is used by callers via outbox, not state

        // Build + sign the DmAck packet.
        let signed = crate::dm_envelope::DmAckSigned {
            space_id,
            message_cid,
            ack_from_owner_addr: bob,
            ack_from_devices: vec![bob_device_hash],
            signing_device_hash: bob_device_hash,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_bob.sign(&signed_bytes);

        (state, signed, signature, signed_bytes, entry_id)
    }

    #[tokio::test]
    async fn handle_ack_updates_outbox_delivered_to() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let space_id = SpaceId([7; 16]);
        let message_cid = ContentId::from_bytes([0xee; 32]);
        let (mut state, signed, signature, signed_bytes, entry_id) =
            build_handle_ack_fixture(alice, bob, space_id, message_cid);

        let mut outbox = make_outbox_synthetic("alice-dev", alice);
        let outcome = outbox
            .handle_ack(
                &mut state,
                signed,
                signature,
                &signed_bytes,
                500,
                &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            )
            .await
            .expect("happy path returns Ok");

        assert_eq!(
            outcome.newly_delivered,
            vec![(space_id, message_cid, bob)],
            "newly_delivered must contain (space_id, message_cid, bob) on first ack"
        );
        let stored = state.outbox.get(&entry_id).unwrap();
        assert!(stored.delivered_to.contains(&bob));
        assert!(matches!(stored.delivery_status, DeliveryStatus::Complete));
    }

    /// ZEB-580 S2 (T5): defense-in-depth revocation cutoff on the dormant
    /// `handle_ack` path — mirrors the `handle_ack` happy-path fixture
    /// above, but with a projection that revokes the signer's (bob's) #2
    /// device key. Must be cut off with `SignerDeviceRevoked` rather than
    /// delivering the ack.
    #[tokio::test]
    async fn handle_ack_from_revoked_device2_is_cut_off() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let space_id = SpaceId([7; 16]);
        let message_cid = ContentId::from_bytes([0xee; 32]);
        let (mut state, signed, signature, signed_bytes, entry_id) =
            build_handle_ack_fixture(alice, bob, space_id, message_cid);

        // Bob's ed25519 half, derived the same way `build_handle_ack_fixture`
        // derives `bob_identity_pub` (same fixed seed).
        let bob_identity_pub = harmony_identity::PrivateIdentity::from_seed(&[0x77; 32])
            .public_identity()
            .to_public_bytes();
        let bob_ed25519: [u8; 32] = bob_identity_pub[32..64].try_into().unwrap();

        let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
        revoked.union_from_members(std::iter::once((
            bob,
            &std::collections::BTreeSet::from([bob_ed25519]),
        )));

        let mut outbox = make_outbox_synthetic("alice-dev", alice);
        let err = outbox
            .handle_ack(&mut state, signed, signature, &signed_bytes, 500, &revoked)
            .await
            .expect_err("ack from revoked device dropped");
        assert_eq!(err, DmReceiveError::SignerDeviceRevoked);

        // delivered_to must still be empty — the cutoff fired before
        // mark_ack_delivered.
        let stored = state.outbox.get(&entry_id).unwrap();
        assert!(stored.delivered_to.is_empty());
        assert!(matches!(stored.delivery_status, DeliveryStatus::Pending));
    }

    #[tokio::test]
    async fn handle_ack_owner_field_mismatch_drops() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let space_id = SpaceId([7; 16]);
        let message_cid = ContentId::from_bytes([0xee; 32]);
        let (mut state, mut signed, _sig, _bytes, _entry_id) =
            build_handle_ack_fixture(alice, bob, space_id, message_cid);

        // Swap ack_from_owner_addr to an attacker. Re-sign with bob's key
        // so Step 1 (signature verify) passes — Step 3 is the explicit
        // defense, NOT a downstream signature failure.
        let attacker = OwnerAddr([0xff; 16]);
        signed.ack_from_owner_addr = attacker;
        let private_bob = harmony_identity::PrivateIdentity::from_seed(&[0x77; 32]);
        let new_signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let new_signature = private_bob.sign(&new_signed_bytes);

        let mut outbox = make_outbox_synthetic("alice-dev", alice);
        let err = outbox
            .handle_ack(
                &mut state,
                signed,
                new_signature,
                &new_signed_bytes,
                500,
                &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, DmReceiveError::OwnerFieldMismatch),
            "expected OwnerFieldMismatch, got {err:?}"
        );
    }

    #[tokio::test]
    async fn handle_ack_from_non_recipient_drops() {
        // resolved_owner is in OwnerDeviceCache but NOT in the
        // OutboxEntry's recipient_owners list — forged ack from a peer
        // who wasn't on the recipient list. MUST NOT advance delivered_to.
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let mallory = OwnerAddr([0x03; 16]);
        let space_id = SpaceId([7; 16]);
        let message_cid = ContentId::from_bytes([0xee; 32]);

        // Build the standard fixture (entry's recipient_owners = [bob]).
        let (mut state, _signed_bob, _sig_bob, _bytes_bob, entry_id) =
            build_handle_ack_fixture(alice, bob, space_id, message_cid);

        // Now seed Mallory's identity into the cache too (cache-known but
        // NOT a legitimate recipient of this OutboxEntry).
        let private_mallory = harmony_identity::PrivateIdentity::from_seed(&[0x33; 32]);
        let mallory_pub_id = private_mallory.public_identity();
        let mallory_identity_pub = mallory_pub_id.to_public_bytes();
        let mallory_device_hash = DeviceIdentityHash(mallory_pub_id.address_hash);
        state.apply_owner_device_update(
            mallory,
            vec![mallory_device_hash],
            vec![Some(mallory_identity_pub)],
            vec![],
            Hlc {
                wall_ms: 60,
                logical: 0,
                device_id: "mallory-dev".into(),
            },
        );

        // Mallory crafts an ack and signs it with her own key.
        let signed = crate::dm_envelope::DmAckSigned {
            space_id,
            message_cid,
            ack_from_owner_addr: mallory,
            ack_from_devices: vec![mallory_device_hash],
            signing_device_hash: mallory_device_hash,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_mallory.sign(&signed_bytes);

        let mut outbox = make_outbox_synthetic("alice-dev", alice);
        let err = outbox
            .handle_ack(
                &mut state,
                signed,
                signature,
                &signed_bytes,
                500,
                &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, DmReceiveError::AckFromNonRecipient),
            "expected AckFromNonRecipient, got {err:?}"
        );
        // delivered_to must still be empty.
        let stored = state.outbox.get(&entry_id).unwrap();
        assert!(stored.delivered_to.is_empty());
        assert!(matches!(stored.delivery_status, DeliveryStatus::Pending));
    }

    #[tokio::test]
    async fn handle_ack_signature_invalid_drops() {
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let space_id = SpaceId([7; 16]);
        let message_cid = ContentId::from_bytes([0xee; 32]);
        let (mut state, signed, mut signature, signed_bytes, entry_id) =
            build_handle_ack_fixture(alice, bob, space_id, message_cid);

        // Flip a bit in the signature.
        signature[0] ^= 0xff;

        let mut outbox = make_outbox_synthetic("alice-dev", alice);
        let err = outbox
            .handle_ack(
                &mut state,
                signed,
                signature,
                &signed_bytes,
                500,
                &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, DmReceiveError::SignatureVerificationFailed),
            "expected SignatureVerificationFailed, got {err:?}"
        );
        // No mutation to delivered_to.
        let stored = state.outbox.get(&entry_id).unwrap();
        assert!(stored.delivered_to.is_empty());
    }

    #[tokio::test]
    async fn handle_ack_outbox_entry_not_found_drops() {
        // DmAck for (space_id, message_cid) we never sent — no matching
        // OutboxEntry. Drop with OutboxEntryNotFound.
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let space_id = SpaceId([7; 16]);
        let real_cid = ContentId::from_bytes([0xee; 32]);
        let (mut state, _signed, _sig, _bytes, _entry_id) =
            build_handle_ack_fixture(alice, bob, space_id, real_cid);

        // Build a DmAck for a DIFFERENT message_cid (one we never sent).
        let unknown_cid = ContentId::from_bytes([0x99; 32]);
        let private_bob = harmony_identity::PrivateIdentity::from_seed(&[0x77; 32]);
        let bob_pub_id = private_bob.public_identity();
        let bob_device_hash = DeviceIdentityHash(bob_pub_id.address_hash);
        let signed = crate::dm_envelope::DmAckSigned {
            space_id,
            message_cid: unknown_cid,
            ack_from_owner_addr: bob,
            ack_from_devices: vec![bob_device_hash],
            signing_device_hash: bob_device_hash,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_bob.sign(&signed_bytes);

        let mut outbox = make_outbox_synthetic("alice-dev", alice);
        let err = outbox
            .handle_ack(
                &mut state,
                signed,
                signature,
                &signed_bytes,
                500,
                &crate::revoked_device_projection::RevokedDeviceProjection::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, DmReceiveError::OutboxEntryNotFound),
            "expected OutboxEntryNotFound, got {err:?}"
        );
    }

    // ── ZEB-227 PR #80 review fix: try_send pressure regressions ─────────

    #[tokio::test]
    async fn runtime_unicast_transport_send_returns_transient_when_channel_full() {
        // RuntimeUnicastTransport::send must NOT .await on a full channel
        // (deadlocks the event loop on itself). Verify it returns
        // Transient — drain's per-recipient backoff drives the retry.
        let (tx, _rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(1);
        // Pre-fill the channel so the first try_send inside `send` hits
        // TrySendError::Full.
        tx.try_send(UnicastSendRequest {
            destination_hash: [0u8; 16],
            packet: vec![],
        })
        .expect("pre-fill must succeed on a fresh channel");

        let signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]));
        let transport = RuntimeUnicastTransport::new(
            tx,
            OwnerAddr([0x01; 16]),
            DeviceIdentityHash([0xaa; 16]),
            signing_key,
        );
        let entry = entry(7);
        // Non-empty destinations so we get past the empty-destinations
        // Transient short-circuit and exercise the channel-full path.
        let res = transport
            .send(&entry, OwnerAddr([0x02; 16]), vec![[0xbb; 16]])
            .await;
        match res {
            Err(TransportError::Transient(msg)) => {
                assert!(
                    msg.contains("unicast channel full"),
                    "expected 'unicast channel full' Transient, got: {msg}"
                );
            }
            other => panic!("expected Transient on full channel, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn runtime_unicast_transport_send_returns_permanent_when_channel_closed() {
        // Channel closed = event-loop receiver dropped (runtime shutdown
        // or panic). retry will never succeed, so `send` must return
        // Permanent — drain converts that to OutboxEntry failure once
        // instead of spinning every drain tick.
        let (tx, rx) = tokio::sync::mpsc::channel::<UnicastSendRequest>(8);
        // Drop the receiver BEFORE calling send → try_send sees Closed.
        drop(rx);

        let signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]));
        let transport = RuntimeUnicastTransport::new(
            tx,
            OwnerAddr([0x01; 16]),
            DeviceIdentityHash([0xaa; 16]),
            signing_key,
        );
        let entry = entry(7);
        // Non-empty destinations so we get past the empty-destinations
        // Transient short-circuit and exercise the channel-closed path.
        let res = transport
            .send(&entry, OwnerAddr([0x02; 16]), vec![[0xbb; 16]])
            .await;
        match res {
            Err(TransportError::Permanent(msg)) => {
                assert!(
                    msg.contains("event-loop channel closed"),
                    "expected 'event-loop channel closed' Permanent, got: {msg}"
                );
            }
            other => panic!("expected Permanent on closed channel, got {other:?}"),
        }
    }

    // ── Phase 4: delete_dm_outbox_entry (manual delete) ─────────────────
    //
    // Removes the OutboxEntry + the corresponding self-InboxEntry keyed
    // by `(space_id, message_cid)`, plus clears in_flight/backoff caches
    // so a stuck entry can't resurface.

    #[tokio::test]
    async fn delete_dm_outbox_entry_removes_outbox_and_self_inbox() {
        // Arrange: build a DM Space, send a DM (which writes both
        // OutboxEntry and self-InboxEntry), pre-populate in_flight +
        // backoff entries for that message to verify they're cleared.
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let sp = make_dm_space(7, vec![alice, bob]);
        let space_id = sp.id;
        install_space(&mut state, sp);

        let cas = InMemoryStub::default();
        let mut o = make_outbox_synthetic("dev", alice);
        let (msg_id, _msg_cid) = o
            .send_dm(
                &mut state,
                &cas,
                space_id,
                b"hello".to_vec(),
                "text/plain".into(),
                1_000,
                None,
            )
            .await
            .expect("send_dm ok");

        // Pre-condition: both records exist.
        let message_cid = state
            .outbox
            .get(&msg_id)
            .expect("outbox entry exists")
            .message_cid
            .expect("message entry has message_cid");
        let inbox_key = crate::owner_state_types::InboxKey {
            space_id,
            message_cid,
        };
        assert!(
            state.inbox.contains_key(&inbox_key),
            "self-InboxEntry must exist before delete"
        );

        // Pre-populate in_flight + backoff so we can verify they're cleared.
        o.in_flight.insert((msg_id, bob));
        o.backoff.insert(
            (msg_id, bob),
            AttemptState {
                last_attempt_wall_ms: 1_000,
                failure_count: 1,
            },
        );

        // Act. Delete past the ZEB-246 stuck threshold (entry created at
        // wall=1_000) so this Pending entry qualifies for manual delete.
        let outcome = o
            .delete_dm_outbox_entry(&mut state, msg_id, 1_000 + STUCK_THRESHOLD_MS + 1_000)
            .expect("delete_dm_outbox_entry ok");

        // Assert: OutboxEntry gone.
        assert!(
            !state.outbox.contains_key(&msg_id),
            "OutboxEntry must be removed"
        );
        // Self-InboxEntry gone.
        assert!(
            !state.inbox.contains_key(&inbox_key),
            "self-InboxEntry must be removed"
        );
        // in_flight + backoff cleared for this message_id (across all
        // recipients, not just bob).
        assert!(
            !o.in_flight.iter().any(|(eid, _)| *eid == msg_id),
            "in_flight must be cleared for deleted message_id"
        );
        assert!(
            !o.backoff.keys().any(|(eid, _)| *eid == msg_id),
            "backoff must be cleared for deleted message_id"
        );

        // Outcome carries the IPC payload data.
        assert_eq!(outcome.deleted_outbox_id, Some(msg_id));
        assert_eq!(outcome.deleted_inbox_key, Some(inbox_key));
        assert_eq!(outcome.space_id, Some(space_id));
        assert_eq!(outcome.message_cid, Some(message_cid));
    }

    #[tokio::test]
    async fn delete_dm_outbox_entry_idempotent_on_missing() {
        // Arrange: empty state, no OutboxEntry exists.
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let mut o = make_outbox_synthetic("dev", alice);
        let fake_id = OutboxEntryId([0xff; 16]);

        // Act.
        let outcome = o
            .delete_dm_outbox_entry(&mut state, fake_id, 1_000)
            .expect("idempotent: no error on missing");

        // Assert: all-None outcome, no error.
        assert_eq!(outcome.deleted_outbox_id, None);
        assert_eq!(outcome.deleted_inbox_key, None);
        assert_eq!(outcome.space_id, None);
        assert_eq!(outcome.message_cid, None);
    }

    #[tokio::test]
    async fn delete_dm_outbox_entry_rejects_completed() {
        // Arrange: send a DM, then mark the OutboxEntry Complete (every
        // recipient acked). delete_dm_outbox_entry must refuse — manual
        // delete is for stuck/expired entries, not delivered self-history.
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let sp = make_dm_space(7, vec![alice, bob]);
        let space_id = sp.id;
        install_space(&mut state, sp);

        let cas = InMemoryStub::default();
        let mut o = make_outbox_synthetic("dev", alice);
        let (msg_id, _msg_cid) = o
            .send_dm(
                &mut state,
                &cas,
                space_id,
                b"shipped".to_vec(),
                "text/plain".into(),
                1_000,
                None,
            )
            .await
            .expect("send_dm ok");

        // Force the entry to Complete: insert bob in delivered_to and
        // recompute status. (mark_ack_delivered would do the same thing
        // but with a forged ack path; we mutate directly to keep the
        // test focused on the delete behavior.)
        {
            let entry = state.outbox.get_mut(&msg_id).expect("entry exists");
            entry.delivered_to.insert(bob);
            entry.delivery_status = entry.compute_status(false);
            assert!(matches!(entry.delivery_status, DeliveryStatus::Complete));
        }
        // Pre-condition: self-InboxEntry exists.
        let message_cid = state
            .outbox
            .get(&msg_id)
            .unwrap()
            .message_cid
            .expect("message entry has message_cid");
        let inbox_key = crate::owner_state_types::InboxKey {
            space_id,
            message_cid,
        };
        assert!(state.inbox.contains_key(&inbox_key));

        // Act: must err.
        let err = o
            .delete_dm_outbox_entry(&mut state, msg_id, 2_000)
            .expect_err("Complete entries must not be deletable");
        match err {
            DeleteDmError::AlreadyDelivered(id) => assert_eq!(id, msg_id),
            other => panic!("expected AlreadyDelivered, got {other:?}"),
        }

        // Post-condition: nothing was removed.
        assert!(
            state.outbox.contains_key(&msg_id),
            "Complete entry must remain"
        );
        assert!(
            state.inbox.contains_key(&inbox_key),
            "self-InboxEntry must remain — delivered history is preserved"
        );
    }

    // ── ZEB-246: in-flight entries below STUCK_THRESHOLD_MS aren't deletable ─
    //
    // A direct IPC call must not be able to delete a fresh Pending/Partial
    // entry (that would be an unsend primitive). Only entries that have aged
    // past the 60s stuck threshold — or are Expired/terminal — may be
    // manually removed. Complete stays refused with AlreadyDelivered.

    #[tokio::test]
    async fn delete_dm_outbox_entry_rejects_fresh_pending() {
        // Arrange: send a DM at wall=1_000; it starts Pending (bob unacked).
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let sp = make_dm_space(7, vec![alice, bob]);
        let space_id = sp.id;
        install_space(&mut state, sp);

        let cas = InMemoryStub::default();
        let mut o = make_outbox_synthetic("dev", alice);
        let (msg_id, _msg_cid) = o
            .send_dm(
                &mut state,
                &cas,
                space_id,
                b"in-flight".to_vec(),
                "text/plain".into(),
                1_000,
                None,
            )
            .await
            .expect("send_dm ok");
        assert!(matches!(
            state.outbox.get(&msg_id).unwrap().delivery_status,
            DeliveryStatus::Pending
        ));
        let message_cid = state.outbox.get(&msg_id).unwrap().message_cid.unwrap();
        let inbox_key = crate::owner_state_types::InboxKey {
            space_id,
            message_cid,
        };

        // Act: delete only 30s later — below the 60s stuck threshold.
        let err = o
            .delete_dm_outbox_entry(&mut state, msg_id, 1_000 + 30_000)
            .expect_err("fresh in-flight entry must not be deletable");
        match err {
            DeleteDmError::NotYetStuck {
                age_ms,
                threshold_ms,
            } => {
                assert_eq!(age_ms, 30_000);
                assert_eq!(threshold_ms, STUCK_THRESHOLD_MS);
            }
            other => panic!("expected NotYetStuck, got {other:?}"),
        }

        // Post-condition: nothing removed, no tombstone written.
        assert!(state.outbox.contains_key(&msg_id), "entry must remain");
        assert!(
            state.inbox.contains_key(&inbox_key),
            "InboxEntry must remain"
        );
        assert!(
            !state.outbox_tombstones.contains_key(&msg_id),
            "no tombstone on a rejected delete"
        );
    }

    #[tokio::test]
    async fn delete_dm_outbox_entry_accepts_stuck_pending() {
        // Arrange: same fresh Pending entry created at wall=1_000.
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let sp = make_dm_space(7, vec![alice, bob]);
        let space_id = sp.id;
        install_space(&mut state, sp);

        let cas = InMemoryStub::default();
        let mut o = make_outbox_synthetic("dev", alice);
        let (msg_id, _msg_cid) = o
            .send_dm(
                &mut state,
                &cas,
                space_id,
                b"stuck".to_vec(),
                "text/plain".into(),
                1_000,
                None,
            )
            .await
            .expect("send_dm ok");
        let message_cid = state.outbox.get(&msg_id).unwrap().message_cid.unwrap();
        let inbox_key = crate::owner_state_types::InboxKey {
            space_id,
            message_cid,
        };

        // Act: delete 70s later — past the 60s stuck threshold.
        let outcome = o
            .delete_dm_outbox_entry(&mut state, msg_id, 1_000 + 70_000)
            .expect("stuck in-flight entry must be deletable");

        // Post-condition: entry + InboxEntry gone, tombstone written.
        assert_eq!(outcome.deleted_outbox_id, Some(msg_id));
        assert!(!state.outbox.contains_key(&msg_id), "entry removed");
        assert!(!state.inbox.contains_key(&inbox_key), "InboxEntry removed");
        assert!(
            state.outbox_tombstones.contains_key(&msg_id),
            "tombstone written on accepted delete"
        );
    }

    // ── ZEB-243: delete_dm_outbox_entry writes tombstone ─────────────────
    //
    // Deletion must write an outbox_tombstone alongside removing the
    // OutboxEntry so that paired-device sync cannot resurrect the deleted
    // message via apply_outbox. The tombstone HLC must be >= the entry's
    // created_at HLC (and is guaranteed strictly-greater because it's
    // minted from the same monotone wall-clock tracker on the same device
    // immediately after the entry's created_at was recorded).

    #[tokio::test]
    async fn delete_dm_outbox_entry_writes_tombstone() {
        // Arrange: build a DM Space, send a DM, verify the outbox entry
        // exists. Then call delete_dm_outbox_entry and assert:
        //   - outbox is empty for that msg_id
        //   - outbox_tombstones contains the msg_id with HLC >= entry.created_at
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let sp = make_dm_space(8, vec![alice, bob]);
        let space_id = sp.id;
        install_space(&mut state, sp);

        let cas = InMemoryStub::default();
        let mut o = make_outbox_synthetic("dev", alice);
        let (msg_id, _msg_cid) = o
            .send_dm(
                &mut state,
                &cas,
                space_id,
                b"tombstone me".to_vec(),
                "text/plain".into(),
                1_000,
                None,
            )
            .await
            .expect("send_dm ok");

        // Capture the entry's created_at before deletion.
        let created_at = state
            .outbox
            .get(&msg_id)
            .expect("outbox entry must exist before delete")
            .created_at
            .clone();

        // ZEB-246: force the entry Expired so the fresh-Pending stuck gate
        // doesn't block this same-millisecond delete — Expired entries are
        // stuck by definition and bypass the freshness check. The tombstone
        // mint path being tested here is identical regardless of status.
        state
            .outbox
            .get_mut(&msg_id)
            .expect("entry exists")
            .delivery_status = DeliveryStatus::Expired;

        // Pre-condition: outbox is populated, no tombstone yet.
        assert!(state.outbox.contains_key(&msg_id));
        assert!(!state.outbox_tombstones.contains_key(&msg_id));

        // Act: delete at the SAME wall-clock millisecond as the entry's
        // created_at so we exercise the fixed monotone-advance path.
        // Pre-fix (next_hlc(None, ...)), this would produce a tombstone
        // with logical=0 at the same wall_ms — equal to or less than
        // created_at if created_at.logical > 0 — which would fail the
        // strict-newer-than gate in apply_outbox. Post-fix (next_hlc with
        // Some(&entry.created_at)), the logical component is bumped even
        // on same-millisecond deletion, guaranteeing strict-newer-than.
        let wall_delete_ms = created_at.wall_ms; // same ms as entry creation
        o.delete_dm_outbox_entry(&mut state, msg_id, wall_delete_ms)
            .expect("delete_dm_outbox_entry ok");

        // Assert: outbox entry is gone.
        assert!(
            !state.outbox.contains_key(&msg_id),
            "OutboxEntry must be removed after delete"
        );

        // Assert: tombstone was written with HLC strictly newer than entry.created_at.
        // (same-millisecond delete must still advance the logical component.)
        let tombstone_hlc = state
            .outbox_tombstones
            .get(&msg_id)
            .expect("outbox_tombstones must contain the deleted msg_id");
        assert!(
            tombstone_hlc.is_strictly_newer_than(&created_at),
            "tombstone HLC must be strictly newer than entry.created_at \
             even on same-millisecond delete (got {:?} vs {:?})",
            tombstone_hlc,
            created_at
        );
    }

    // ── ZEB-267: reserve_next_hlc_for_device tests ─────────────────────
    //
    // Helper is the atomic read-bump-write primitive that replaces the
    // snapshot-then-release pattern at every membership-event IPC site.
    // These tests pin its three load-bearing properties:
    //
    //   1. Sequential reservations advance monotonically (sanity check).
    //   2. Concurrent reservations on the same tracker produce N distinct
    //      strictly-monotone HLCs (the actual bug fix — old pattern would
    //      collide here).
    //   3. Wall-clock regression (wall_now_ms < prev.wall_ms) still
    //      produces a strictly-greater HLC by clamping to prev.wall_ms +
    //      bumping logical (preserves monotonicity under clock skew).

    #[tokio::test]
    async fn reserve_next_hlc_for_device_advances_tracker_atomically() {
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let device_id = "test-dev-A";
        let tracker: Arc<Mutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>> = Arc::new(
            Mutex::new(harmony_crdt_sync::ReplayTracker::new(device_id.to_string())),
        );
        let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
        let wall_now_ms = 1_700_000_000_000u64;

        let first = reserve_next_hlc_for_device(&tracker, &floor, device_id, wall_now_ms).await;
        let second = reserve_next_hlc_for_device(&tracker, &floor, device_id, wall_now_ms).await;

        // Sort key is (wall_ms, logical, device_id) — strictly-greater
        // ordering is what the receive side expects for per-device events.
        assert!(
            (second.wall_ms, &second.logical, &second.device_id)
                > (first.wall_ms, &first.logical, &first.device_id),
            "second reservation must be strictly greater than first under sort key"
        );
        // Tracker must hold the SECOND (just-bumped) value, not the first.
        let stored = tracker
            .lock()
            .await
            .accepted()
            .get(device_id)
            .cloned()
            .expect("tracker has entry");
        assert_eq!(
            stored, second,
            "tracker must hold the most-recently-reserved HLC"
        );
    }

    #[tokio::test]
    async fn reserve_adopts_verified_future_stamp_within_cap() {
        // ZEB-790: the ZEB-788 621ms inversion, made impossible. A mint
        // that follows a verified-and-applied remote stamp W (W < now+CAP)
        // must exceed W — even when the remote carried logical > 0.
        let tracker = std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::BTreeMap::<
            String,
            Hlc,
        >::new()));
        let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
        let now = 1_785_021_611_000u64; // "Ildwyn's clock"
        let remote_wall = now + 600; // "AVALON's stamp", 600ms ahead
        floor.observe(remote_wall); // what the engines do post-verify
        let minted = reserve_next_hlc_for_device(&tracker, &floor, "ildwyn-dev", now).await;
        assert_eq!(minted.wall_ms, remote_wall + 1, "wall strictly exceeds W");
        assert_eq!(minted.logical, 0);
        // Strictly after the remote stamp for ANY remote logical (the +1 rule):
        let remote = Hlc {
            wall_ms: remote_wall,
            logical: u32::MAX,
            device_id: "avalon-dev".into(),
        };
        assert!(minted.is_strictly_newer_than(&remote));
    }

    #[tokio::test]
    async fn reserve_clamps_beyond_cap_and_stays_device_monotone() {
        let tracker = std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::BTreeMap::<
            String,
            Hlc,
        >::new()));
        let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
        let now = 1_000_000u64;
        floor.observe(now + 3_600_000); // hostile: one hour ahead
        let a = reserve_next_hlc_for_device(&tracker, &floor, "dev", now).await;
        assert_eq!(
            a.wall_ms,
            now + crate::hlc_adopt_floor::HLC_ADOPT_FORWARD_CAP_MS,
            "clamped to CAP"
        );
        // Per-device strict monotonicity survives adoption:
        let b = reserve_next_hlc_for_device(&tracker, &floor, "dev", now).await;
        assert!(
            b.is_strictly_newer_than(&a),
            "wall tied at clamp -> logical bumps"
        );
    }

    #[tokio::test]
    async fn reserve_with_empty_floor_is_todays_behavior() {
        let tracker = std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::BTreeMap::<
            String,
            Hlc,
        >::new()));
        let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
        let minted = reserve_next_hlc_for_device(&tracker, &floor, "dev", 42_000).await;
        assert_eq!(minted.wall_ms, 42_000, "identity: no observed remote");
        assert_eq!(minted.logical, 0);
    }

    #[tokio::test]
    async fn reserve_next_hlc_for_device_concurrent_reservations_distinct() {
        use std::collections::BTreeSet;
        use std::sync::Arc;
        use tokio::sync::Mutex;
        use tokio::task::JoinSet;

        let device_id = "test-dev-conc";
        let tracker: Arc<Mutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>> = Arc::new(
            Mutex::new(harmony_crdt_sync::ReplayTracker::new(device_id.to_string())),
        );
        let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
        let wall_now_ms = 1_700_000_111_222u64;

        // Spawn 64 concurrent reservations. Without the atomic helper,
        // the snapshot-then-release pattern would produce duplicate
        // (wall_ms, logical, device_id) tuples across these tasks.
        let mut set: JoinSet<Hlc> = JoinSet::new();
        for _ in 0..64 {
            let tracker = Arc::clone(&tracker);
            let floor = floor.clone();
            let device_id = device_id.to_string();
            set.spawn(async move {
                reserve_next_hlc_for_device(&tracker, &floor, &device_id, wall_now_ms).await
            });
        }

        let mut hlcs: Vec<Hlc> = Vec::with_capacity(64);
        while let Some(joined) = set.join_next().await {
            hlcs.push(joined.expect("task panic"));
        }

        // Use sort-key tuples as the dedupe key (Hlc itself is Eq, but
        // BTreeSet<(u64, u32, String)> makes the failure message clearer
        // by surfacing the colliding tuple directly).
        let unique: BTreeSet<(u64, u32, String)> = hlcs
            .iter()
            .map(|h| (h.wall_ms, h.logical, h.device_id.clone()))
            .collect();
        assert_eq!(
            unique.len(),
            64,
            "all 64 concurrent reservations must yield distinct sort keys; got {} unique out of 64",
            unique.len()
        );

        // Tracker's final value must equal the max-by-sort-key of all
        // reservations (last-write-wins under the helper's atomic
        // critical section).
        let max_observed = hlcs
            .iter()
            .max_by_key(|h| (h.wall_ms, h.logical, h.device_id.clone()))
            .expect("at least one reservation");
        let stored = tracker
            .lock()
            .await
            .accepted()
            .get(device_id)
            .cloned()
            .expect("tracker has entry");
        assert_eq!(
            &stored, max_observed,
            "tracker's final value must equal the max-by-sort-key reservation"
        );
    }

    // ── ZEB-241 Task 3: TOCTOU regression tests ────────────────────────
    //
    // The lifted handler runs in three phases: Phase A (locked, fast —
    // verify + snapshot Space), Phase B (unlocked, slow — CAS fetch),
    // Phase C (re-locked, fast — re-fetch Space + decrypt + apply).
    //
    // The TOCTOU window is the gap between Phase A's lock-drop and
    // Phase C's lock-acquire. To exercise it deterministically, the
    // tests below use a `GatedCasStub` whose `get()` blocks on a
    // `tokio::sync::Notify` until the test releases it. This pattern
    // lets the test:
    //   1. Spawn the lifted handler.
    //   2. Wait briefly so Phase A runs to completion.
    //   3. Acquire the state lock and mutate state (rotate key /
    //      remove Space / kick member).
    //   4. Drop the lock.
    //   5. Release the gate so Phase B unblocks.
    //   6. Await the spawned handler.
    //   7. Inspect post-call state.
    //
    // The 50ms post-spawn sleep gives Phase A (microsecond-scale, no
    // I/O) ample headroom to acquire+release locks before the test
    // mutates state. Bumping if any flake observed.

    #[tokio::test]
    async fn reserve_next_hlc_for_device_handles_wall_regression() {
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let device_id = "test-dev-regress";
        let tracker: Arc<Mutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>> = Arc::new(
            Mutex::new(harmony_crdt_sync::ReplayTracker::new(device_id.to_string())),
        );

        // Pre-seed the tracker with an HLC at wall_ms=1000, logical=5.
        {
            let mut t = tracker.lock().await;
            t.observe_local(Hlc {
                wall_ms: 1000,
                logical: 5,
                device_id: device_id.to_string(),
            });
        }

        // Reserve with wall_now_ms=500 — strictly less than the prior
        // wall_ms. next_hlc clamps to prev.wall_ms and bumps logical.
        let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
        let reserved = reserve_next_hlc_for_device(&tracker, &floor, device_id, 500).await;
        assert_eq!(
            reserved.wall_ms, 1000,
            "wall_ms must clamp to prev.wall_ms under regression"
        );
        assert_eq!(reserved.logical, 6, "logical must bump prev.logical + 1");
        assert_eq!(reserved.device_id, device_id);

        // Tracker must hold the new value.
        let stored = tracker
            .lock()
            .await
            .accepted()
            .get(device_id)
            .cloned()
            .expect("tracker has entry");
        assert_eq!(stored, reserved);
    }

    // =================================================================
    // ZEB-458 P4 Phase B: community-relay rung — last-resort after butler
    // no-ack
    // =================================================================

    struct MockRelay {
        acked: bool,
        calls: std::sync::Arc<std::sync::Mutex<Vec<OwnerAddr>>>,
    }

    impl MockRelay {
        fn new(acked: bool) -> Arc<Self> {
            Arc::new(Self {
                acked,
                calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            })
        }

        fn calls(&self) -> Vec<OwnerAddr> {
            self.calls.lock().expect("mock poisoned").clone()
        }
    }

    #[async_trait]
    impl crate::community_relay::CommunityRelayDepositClient for MockRelay {
        async fn deposit(&self, req: &ButlerDepositRequest) -> bool {
            self.calls
                .lock()
                .expect("mock poisoned")
                .push(req.recipient_owner);
            self.acked
        }
    }

    /// ZEB-458 P4B: when the butler rung does NOT ack (SkippedNoFreshButlerSet),
    /// the community-relay rung fires for the same candidate; if it acks,
    /// the recipient is marked delivered and appears in `newly_delivered`.
    #[tokio::test]
    async fn relay_rung_fires_and_marks_delivered_when_butler_does_not_ack() {
        let (mut state, transport, mut o, butler_mock, entry_id, bob) =
            deposit_rung_fixture(DepositRungOutcome::SkippedNoFreshButlerSet);
        let space_id = SpaceId([1u8; 16]);
        let message_cid = ContentId::from_bytes([3u8; 32]);

        let relay_mock = MockRelay::new(true);
        o.set_community_relay_deposit_client(relay_mock.clone());

        // Tick 1 (t=10_000): first attempt, no AttemptState yet → neither
        // rung fires (entry not pending ≥ one backoff cycle).
        let outcome1 =
            drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 10_000)
                .await;
        assert!(outcome1.newly_delivered.is_empty());
        assert!(butler_mock.calls().is_empty());
        assert!(
            relay_mock.calls().is_empty(),
            "relay must not fire before candidacy"
        );

        // Tick 2 (t=15_000): AttemptState exists → both rungs fire.
        let outcome2 =
            drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 15_000)
                .await;

        // Butler was consulted (and returned SkippedNoFreshButlerSet).
        assert_eq!(
            butler_mock.calls().len(),
            1,
            "butler rung consulted on tick 2"
        );

        // Relay was consulted for the same recipient.
        let relay_calls = relay_mock.calls();
        assert_eq!(relay_calls.len(), 1, "relay rung must fire exactly once");
        assert_eq!(
            relay_calls[0], bob,
            "relay called for the correct recipient"
        );

        // Relay ack → delivered, surfaces in newly_delivered.
        assert_eq!(
            outcome2.newly_delivered,
            vec![(space_id, message_cid, bob)],
            "relay ack must surface in newly_delivered (dm-delivered emit)"
        );
        let stored = state.outbox.get(&entry_id).expect("entry still present");
        assert!(
            stored.delivered_to.contains(&bob),
            "bob marked delivered via relay"
        );
        assert!(
            matches!(stored.delivery_status, DeliveryStatus::Complete),
            "sole recipient acked via relay -> Complete"
        );
        // mark_ack_delivered must have cleared the pair's retry state.
        assert_eq!(o.backoff_len(), 0);
        assert_eq!(o.in_flight_len(), 0);
    }

    /// ZEB-458 P4B: when the butler rung DOES ack, the relay rung must NOT
    /// be called — relay is strictly last-resort.
    #[tokio::test]
    async fn relay_rung_skipped_when_butler_acks() {
        let (mut state, transport, mut o, butler_mock, entry_id, bob) =
            deposit_rung_fixture(DepositRungOutcome::Acked);

        let relay_mock = MockRelay::new(true);
        o.set_community_relay_deposit_client(relay_mock.clone());

        // Tick 1: no AttemptState → neither rung fires.
        let _ = drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 10_000)
            .await;
        assert!(butler_mock.calls().is_empty());
        assert!(relay_mock.calls().is_empty());

        // Tick 2: butler acks → relay must NOT be consulted.
        let _outcome =
            drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 15_000)
                .await;
        assert_eq!(
            butler_mock.calls().len(),
            1,
            "butler rung consulted on tick 2"
        );
        assert!(
            relay_mock.calls().is_empty(),
            "relay must NOT be called when butler acks"
        );
    }

    /// ZEB-458 P4B: when ONLY the community relay client is installed (no
    /// butler client at all), a candidate that has reached the deposit-
    /// candidacy threshold must still be produced and the relay rung must
    /// fire and mark the recipient delivered.
    ///
    /// Before the drain_phase_c candidacy gate fix, the gate required
    /// `butler_deposit_client.is_some()`, so no candidate was ever produced
    /// when the butler was absent — the relay rung was never reached and the
    /// recipient was left undelivered.  After the fix the gate accepts either
    /// client, so the relay rung fires and delivers.
    #[tokio::test]
    async fn relay_rung_fires_without_butler_client() {
        // Build the outbox manually (not via deposit_rung_fixture, which
        // always installs a butler client).
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = entry_with_age(7, vec![bob], 1_000);
        let entry_id = entry.id;
        let space_id = SpaceId([1u8; 16]);
        let message_cid = ContentId::from_bytes([3u8; 32]);
        install_outbox_entry(&mut state, entry);

        let transport = StubTransport::new();
        let mut o = make_outbox_synthetic("dev", alice);
        // Intentionally do NOT call set_butler_deposit_client — butler is None.

        let relay_mock = MockRelay::new(true);
        o.set_community_relay_deposit_client(relay_mock.clone());

        // Tick 1 (t=10_000): first attempt, no AttemptState yet → candidacy
        // threshold not reached → relay must not fire.
        let outcome1 =
            drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 10_000)
                .await;
        assert!(outcome1.newly_delivered.is_empty());
        assert!(
            relay_mock.calls().is_empty(),
            "relay must not fire before candidacy threshold"
        );

        // Tick 2 (t=15_000): AttemptState exists (failure_count ≥ 1) →
        // candidacy gate must now be satisfied by the relay client alone →
        // relay rung fires and acks → recipient marked delivered.
        let outcome2 =
            drain_with_transient_failure(&mut o, &mut state, &transport, entry_id, bob, 15_000)
                .await;

        let relay_calls = relay_mock.calls();
        assert_eq!(
            relay_calls.len(),
            1,
            "relay rung must fire exactly once on tick 2"
        );
        assert_eq!(
            relay_calls[0], bob,
            "relay called for the correct recipient"
        );

        // Relay ack → delivered, surfaces in newly_delivered.
        assert_eq!(
            outcome2.newly_delivered,
            vec![(space_id, message_cid, bob)],
            "relay ack must surface in newly_delivered"
        );
        let stored = state.outbox.get(&entry_id).expect("entry still present");
        assert!(
            stored.delivered_to.contains(&bob),
            "bob marked delivered via relay (butler-free)"
        );
        assert!(
            matches!(stored.delivery_status, DeliveryStatus::Complete),
            "sole recipient acked via relay -> Complete"
        );
        assert_eq!(o.backoff_len(), 0);
        assert_eq!(o.in_flight_len(), 0);
    }

    /// ZEB-458 P4B (review fix): the relay-rung regression above only drives
    /// the LOCK-HELD `drain`. Production runs the separately-copied ladder in
    /// `drain_lifted`'s spawned Phase C (lock-reacquire + `dm-delivered`
    /// emit). This test exercises THAT path end-to-end: butler is absent, the
    /// relay client acks, and after the spawned Phase C settles the recipient
    /// is marked delivered, the relay was called for the right recipient, and
    /// a `dm-delivered` IPC frame was emitted via the NodeEventSink — the
    /// emit that only the spawned path performs.
    // Multi-thread runtime: `drain_lifted` spawns Phase C detached, and this
    // test observes its effects by polling shared Arcs from the test task. On
    // a current-thread runtime the detached task and the poller can starve
    // each other across `try_lock`/`.await` boundaries; a 2-worker runtime
    // lets Phase C make progress independently (and matches production, where
    // Phase C runs on the multi-thread node runtime).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_lifted_relay_rung_marks_delivered_and_emits_via_spawned_path() {
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let space_id = SpaceId([1u8; 16]);
        let message_cid = ContentId::from_bytes([3u8; 32]);

        // IMPORTANT timing note: unlike the lock-held `drain` (which stamps
        // `last_attempt_wall_ms` from the caller-supplied `wall_now_ms`),
        // `drain_lifted`'s spawned Phase C stamps it from the REAL wall clock
        // (`SystemTime::now()`). The next tick's `is_due` then compares the
        // caller-supplied `wall_now_ms` against that real-clock stamp. So the
        // two ticks must use realistic, real-clock-anchored timestamps: tick 1
        // ≈ now, tick 2 ≈ now + (one backoff window). We anchor on
        // `SystemTime::now()` and advance tick 2 by 6s (> the 5s base window)
        // so the entry becomes due again with an AttemptState present.
        let base_now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let tick1_ms = base_now_ms;
        let tick2_ms = base_now_ms + 6_000;

        // Entry created just before tick 1 so it is well within the 30-day
        // expiration window at both ticks.
        let entry = entry_with_age(7, vec![bob], base_now_ms.saturating_sub(1_000));
        let entry_id = entry.id;

        let mut state = OwnerState::default();
        install_outbox_entry(&mut state, entry);

        let mut o = make_outbox_synthetic("dev", alice);
        // No butler client — the relay rung must still fire (the drain_phase_c
        // candidacy gate accepts the relay client alone).
        let relay_mock = MockRelay::new(true);
        o.set_community_relay_deposit_client(relay_mock.clone());

        let outbox_arc = Arc::new(Mutex::new(o));
        let state_arc = Arc::new(Mutex::new(state));

        // A transport that always fails transiently, so each drain records a
        // direct-send failure → builds candidacy across two ticks.
        let transport = StubTransport::new();
        transport.set_outcome(
            entry_id,
            bob,
            Err(TransportError::Transient("recipient unreachable".into())),
        );

        let sink = crate::node_event_sink::RecordingSink::new();
        let app: Arc<dyn crate::node_event_sink::NodeEventSink> = Arc::new(sink.clone());

        // `drain_lifted` spawns Phase C detached; it settles shortly after the
        // call returns. Poll a predicate with a bounded budget so the test is
        // not racy. (The spawned task re-acquires outbox/state via
        // `.lock().await`, so we can observe its effects via the shared Arcs /
        // the mock / the sink once it completes.)
        async fn wait_until<F: Fn() -> bool>(pred: F) {
            for _ in 0..400 {
                if pred() {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            panic!("predicate did not become true within budget");
        }

        // Tick 1 (≈ now): first attempt, no prior AttemptState → candidacy
        // threshold not reached → relay must not fire. Wait for the spawned
        // Phase C to record the transient-failure backoff.
        super::drain_lifted(
            Arc::clone(&outbox_arc),
            Arc::clone(&state_arc),
            &transport,
            tick1_ms,
            Arc::clone(&app),
            None, // ZEB-703: durability not exercised here
        )
        .await;
        // After Phase C settles, an AttemptState exists for (entry, bob).
        wait_until(|| match outbox_arc.try_lock() {
            Ok(g) => g.backoff_len() == 1,
            Err(_) => false,
        })
        .await;
        assert!(
            relay_mock.calls().is_empty(),
            "relay must not fire before candidacy threshold (tick 1)"
        );

        // The StubTransport CONSUMES a configured outcome on each `send`
        // (returns it once, then defaults to Ok). Re-arm the transient failure
        // so tick 2's Phase B also fails directly → the Err-path candidacy
        // gate (pre_failure_count >= 1) produces a relay candidate. (The
        // lock-held `drain` tests do the same via `drain_with_transient_failure`,
        // which re-sets the outcome before every tick.)
        transport.set_outcome(
            entry_id,
            bob,
            Err(TransportError::Transient("recipient unreachable".into())),
        );

        // Tick 2 (≈ now + 6s, one base backoff window elapsed): AttemptState
        // exists and the entry is due again → candidate produced → relay rung
        // fires in the spawned Phase C.
        super::drain_lifted(
            Arc::clone(&outbox_arc),
            Arc::clone(&state_arc),
            &transport,
            tick2_ms,
            Arc::clone(&app),
            None, // ZEB-703: durability not exercised here
        )
        .await;

        // Wait on the spawned Phase C's FINAL observable effect: the
        // `dm-delivered` IPC emit. Phase C orders relay-ack → mark
        // `delivered_to` under the outbox/state locks → drop locks → emit
        // (the lock-drop-before-emit order is load-bearing, ZEB-233).
        // Waiting on the state mark alone (the previous condition) raced the
        // emit: the mark becomes observable the instant the locks drop —
        // strictly BEFORE the emit loop runs — so the frame assertions below
        // could read an empty sink under CI shard contention (ZEB-698 flake).
        // The emit is last, so once it lands every earlier effect has too.
        wait_until(|| sink.frames().iter().any(|(ev, _)| ev == "dm-delivered")).await;

        // Relay was consulted exactly once, for bob.
        let relay_calls = relay_mock.calls();
        assert_eq!(
            relay_calls.len(),
            1,
            "relay rung must fire exactly once via the spawned Phase C"
        );
        assert_eq!(
            relay_calls[0], bob,
            "relay called for the correct recipient"
        );

        // The recipient is marked delivered (sole recipient → Complete).
        {
            let s = state_arc.lock().await;
            let stored = s.outbox.get(&entry_id).expect("entry still present");
            assert!(
                stored.delivered_to.contains(&bob),
                "bob marked delivered via the relay rung (spawned path)"
            );
            assert!(
                matches!(stored.delivery_status, DeliveryStatus::Complete),
                "sole recipient acked via relay -> Complete"
            );
        }

        // The spawned path emitted a `dm-delivered` IPC frame for bob — the
        // emit the lock-held `drain` rung does NOT perform (its caller emits).
        let frames = sink.frames();
        let delivered: Vec<_> = frames
            .iter()
            .filter(|(ev, _)| ev == "dm-delivered")
            .collect();
        assert_eq!(
            delivered.len(),
            1,
            "exactly one dm-delivered frame must be emitted via the spawned Phase C; got {frames:?}"
        );
        let (_, payload) = delivered[0];
        assert_eq!(
            payload.get("recipientOwnerAddr").and_then(|v| v.as_str()),
            Some(hex::encode(bob.0).as_str()),
            "dm-delivered frame must name bob as the recipient"
        );
        assert_eq!(
            payload.get("spaceId").and_then(|v| v.as_str()),
            Some(hex::encode(space_id.0).as_str()),
        );
        assert_eq!(
            payload.get("messageCid").and_then(|v| v.as_str()),
            Some(hex::encode(message_cid.to_bytes()).as_str()),
        );
    }

    // ── ZEB-474 Task 1: DepositOnlyDmTransport unit test ─────────────────────

    #[tokio::test]
    async fn deposit_only_transport_send_signals_no_live_attempt_to_steer_into_deposit_rung() {
        // ZEB-474: the deposit-only transport must never claim a direct send
        // succeeded — returning an error is what steers the outbox into its
        // butler/community-relay deposit rung (which performs real delivery and
        // calls mark_ack_delivered on ack). An Ok here would be a silent
        // black-hole: the outbox would treat the DM as "sent, awaiting ack"
        // and never deposit it. ZEB-525: the flavor is `TransientNoLiveAttempt`
        // — this transport launches no live attempt, so the deposit fires on
        // the FIRST drain pass instead of after a one-window grace.
        let t = DepositOnlyDmTransport;
        let entry = OutboxEntry {
            id: OutboxEntryId([1u8; 16]),
            space_id: SpaceId([7u8; 16]),
            recipient_owners: vec![],
            message_cid: Some(ContentId::from_bytes([9u8; 32])),
            created_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "test".into(),
            },
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        };
        let recipient = OwnerAddr([3u8; 16]);
        let err = t
            .send(&entry, recipient, vec![[1u8; 16]])
            .await
            .expect_err("deposit-only send must signal an error, never Ok");
        assert!(matches!(err, TransportError::TransientNoLiveAttempt(_)));
    }

    // ── ZEB-474 Task 3: deposit-routing integration test ─────────────────────

    /// Drive one drain tick with `DepositOnlyDmTransport` (always
    /// `TransientNoLiveAttempt` — ZEB-525).
    async fn drain_with_deposit_only_transport(
        o: &mut DmOutbox,
        state: &mut OwnerState,
        wall_now_ms: u64,
    ) -> DrainOutcome {
        let transport = DepositOnlyDmTransport;
        o.drain(state, &transport, wall_now_ms).await
    }

    /// With `DepositOnlyDmTransport` wired and a butler deposit client
    /// installed + acking, the DM routes through the deposit rung on the
    /// FIRST drain pass (ZEB-525: `TransientNoLiveAttempt` bypasses the
    /// one-window `pre_failure_count >= 1` grace — the deposit-only transport
    /// launches no live attempt for the grace to wait on) and is marked
    /// delivered via `mark_ack_delivered` in the same tick.
    #[tokio::test]
    async fn deposit_only_transport_routes_dm_to_deposit_rung_on_first_tick() {
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = entry_with_age(7, vec![bob], 1_000);
        let entry_id = entry.id;
        let space_id = SpaceId([1u8; 16]);
        let message_cid = ContentId::from_bytes([3u8; 32]);

        let mut state = OwnerState::default();
        install_outbox_entry(&mut state, entry);

        let mut o = make_outbox_synthetic("dev", alice);
        let mock = MockDepositClient::returning(DepositRungOutcome::Acked);
        o.set_butler_deposit_client(mock.clone());

        // Tick 1 (t=10_000): the very first NoLiveAttempt failure fires the
        // deposit rung — no prior AttemptState needed (pre-ZEB-525 this
        // waited for tick 2) → butler acks → mark_ack_delivered → surfaces
        // in newly_delivered.
        let outcome1 = drain_with_deposit_only_transport(&mut o, &mut state, 10_000).await;
        assert_eq!(
            mock.calls().len(),
            1,
            "deposit rung must fire on the FIRST tick for a no-live-attempt transport"
        );
        let req = &mock.calls()[0];
        assert_eq!(req.entry_id, entry_id);
        assert_eq!(req.recipient_owner, bob);
        assert_eq!(req.space_id, space_id);
        assert_eq!(req.message_cid, Some(message_cid));
        assert_eq!(
            outcome1.newly_delivered,
            vec![(space_id, message_cid, bob)],
            "butler ack must surface in newly_delivered (dm-delivered emit)"
        );
        let stored = state.outbox.get(&entry_id).expect("entry still present");
        assert!(stored.delivered_to.contains(&bob), "bob marked delivered");
        assert!(
            matches!(stored.delivery_status, DeliveryStatus::Complete),
            "sole recipient acked via butler -> Complete"
        );

        // A second tick must not double-deposit: the entry completed on tick 1.
        let outcome2 = drain_with_deposit_only_transport(&mut o, &mut state, 15_000).await;
        assert_eq!(
            mock.calls().len(),
            1,
            "no further deposit for a completed entry"
        );
        assert!(outcome2.newly_delivered.is_empty());
    }

    /// ZEB-473 Task 8 / CR4 — always-deposit invariant for the LIVE tunnel
    /// carrier with the CONTACT-PRESENT path exercised: Bob has a cached,
    /// correctly-sized `DeviceTunnelContact`, so the transport actually resolves
    /// a tunnel target and fires `send_dm`. Even so, the transport returns the
    /// SAME `Transient` contract, so the deposit rung STILL fires on the second
    /// drain pass and the DM delivers — no durability regression. (Previously
    /// this test passed `OwnerState::default()`, covering only the resolver-miss
    /// path; CR4 seeds the contact so the contact-present rung is covered too.)
    #[tokio::test]
    async fn tunnel_transport_still_routes_dm_to_deposit_rung_and_delivers_on_ack() {
        use crate::owner_state_types::{DeviceTunnelContact, OwnerDeviceEntry};

        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = entry_with_age(7, vec![bob], 1_000);
        let entry_id = entry.id;
        let space_id = SpaceId([1u8; 16]);
        let message_cid = ContentId::from_bytes([3u8; 32]);

        let mut state = OwnerState::default();
        install_outbox_entry(&mut state, entry);

        let mut o = make_outbox_synthetic("dev", alice);
        let mock = MockDepositClient::returning(crate::butler_deposit::DepositRungOutcome::Acked);
        o.set_butler_deposit_client(mock.clone());

        // Build the live tunnel transport over a real loopback iroh endpoint.
        let endpoint = {
            let sk = iroh::SecretKey::generate();
            crate::iroh_endpoint::new_with_secret_and_relays_hermetic_dns(sk, None)
                .await
                .expect("bind loopback iroh endpoint")
        };
        let local_pq = Arc::new(harmony_identity::PqPrivateIdentity::generate(
            &mut rand::rngs::OsRng,
        ));
        let (ingest_tx, _ingest_rx) = tokio::sync::mpsc::channel(16);
        let mgr = Arc::new(crate::tunnel_manager::TunnelManager::new(
            Arc::new(endpoint),
            local_pq,
            ingest_tx,
            Arc::new(crate::protocol_versioning::ProtocolCompatRegistry::default())
                as Arc<dyn crate::tunnel_manager::CompatSink>,
        ));

        // CR4: seed Bob's cached tunnel contact into the transport's resolution
        // state so the CONTACT-PRESENT tunnel attempt is exercised (not just the
        // resolver-miss path). Correctly-sized PQ keys so it's a valid contact.
        let mut resolver_state = OwnerState::default();
        resolver_state.owner_device_cache.devices.insert(
            bob,
            OwnerDeviceEntry {
                devices: vec![DeviceIdentityHash([0xb0; 16])],
                device_identity_pubs: vec![None],
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "bob".into(),
                },
                device_tunnel_contacts: vec![Some(DeviceTunnelContact {
                    iroh_node_id: [0xb1; 32],
                    home_relay_url: None,
                    pq_dsa_pubkey: vec![0xb2; crate::owner_state_types::ML_DSA_65_PUBKEY_LEN],
                    pq_kem_pubkey: vec![0xb3; crate::owner_state_types::ML_KEM_768_PUBKEY_LEN],
                })],
            },
        );
        let transport = crate::iroh_tunnel_dm_transport::IrohTunnelDmTransport::new(
            mgr,
            Arc::new(tokio::sync::Mutex::new(resolver_state)),
            Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32])),
            alice,
            DeviceIdentityHash([0xaa; 16]),
            [0x55u8; 64],
            std::sync::Arc::new(crate::content_store::InMemoryStub::default()),
        );

        // Tick 1 (t=10_000): first Transient → no prior AttemptState → no rung.
        let outcome1 = o.drain(&mut state, &transport, 10_000).await;
        assert!(
            mock.calls().is_empty(),
            "first transient must not fire deposit rung (no prior AttemptState)"
        );
        assert!(outcome1.newly_delivered.is_empty());

        // Tick 2 (backoff window elapsed): second Transient → rung fires → ack.
        let outcome2 = o.drain(&mut state, &transport, 15_000).await;
        assert_eq!(
            mock.calls().len(),
            1,
            "deposit rung must fire exactly once on tick 2 even with the live tunnel transport"
        );
        assert_eq!(mock.calls()[0].entry_id, entry_id);
        assert_eq!(mock.calls()[0].recipient_owner, bob);
        assert_eq!(
            outcome2.newly_delivered,
            vec![(space_id, message_cid, bob)],
            "butler ack must surface (no durability regression from the tunnel carrier)"
        );
        let stored = state.outbox.get(&entry_id).expect("entry still present");
        assert!(stored.delivered_to.contains(&bob), "bob marked delivered");
        assert!(matches!(stored.delivery_status, DeliveryStatus::Complete));
    }

    /// With `DepositOnlyDmTransport` and NO deposit client installed, the
    /// entry remains queued (Pending) — never delivered, never errored —
    /// across several drain passes (the rung is never consulted).
    #[tokio::test]
    async fn deposit_only_transport_no_client_entry_remains_queued() {
        let alice = OwnerAddr([0xaa; 16]);
        let bob = OwnerAddr([0xbb; 16]);
        let entry = entry_with_age(9, vec![bob], 1_000);
        let entry_id = entry.id;

        let mut state = OwnerState::default();
        install_outbox_entry(&mut state, entry);

        let mut o = make_outbox_synthetic("dev", alice);
        // No deposit client installed — outbox has no deposit rung.

        for tick_ms in [10_000u64, 15_000, 25_000, 55_000] {
            let outcome = drain_with_deposit_only_transport(&mut o, &mut state, tick_ms).await;
            assert!(
                outcome.newly_delivered.is_empty(),
                "no delivery possible without deposit client (tick {tick_ms})"
            );
        }

        let stored = state
            .outbox
            .get(&entry_id)
            .expect("entry still present after all ticks");
        assert!(
            matches!(stored.delivery_status, DeliveryStatus::Pending),
            "entry stays Pending without a deposit client, never errored"
        );
        assert!(stored.delivered_to.is_empty());
    }

    // ── ZEB-580 S2 (T3): CidNotify signer-device revocation cutoff ─────────

    /// Minimal synchronous fixture for `verify_cidnotify_sender_binding`
    /// tests — the sender-binding-only slice of `build_cidnotify_fixture`
    /// (Phase 3b Task 10), dropping the Space/CAS/message-encryption
    /// machinery `verify_cidnotify_sender_binding` never touches (that's
    /// `verify_cidnotify_space`'s job). Returns `(state, signed, signature,
    /// signed_bytes, owner, combined_pub)`: `owner` is the signed-origin-
    /// resolved sender, `combined_pub` her cached 64-byte identity pub
    /// (X25519 || Ed25519) — `combined_pub[32..64]` is the ed25519 half the
    /// cutoff checks against the revoked-device projection.
    fn cidnotify_verify_fixture_with_seed(
        alice_seed: u8,
    ) -> (
        OwnerState,
        crate::dm_envelope::DmCidNotifySigned,
        [u8; 64],
        Vec<u8>,
        OwnerAddr,
        [u8; 64],
    ) {
        let alice = OwnerAddr([alice_seed; 16]);
        let space_id = SpaceId([0x5A; 16]);

        let mut state = OwnerState::default();
        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[alice_seed; 32]);
        let alice_pub_id = private_alice.public_identity();
        let alice_identity_pub = alice_pub_id.to_public_bytes();
        let alice_device_hash = DeviceIdentityHash(alice_pub_id.address_hash);

        state.apply_owner_device_update(
            alice,
            vec![alice_device_hash],
            vec![Some(alice_identity_pub)],
            vec![],
            Hlc {
                wall_ms: 50,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        );

        let message_cid = harmony_content::cid::ContentId::for_book(
            b"cidnotify-cutoff-fixture-body",
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .unwrap();

        let signed = crate::dm_envelope::DmCidNotifySigned {
            space_id,
            message_cid,
            sender_owner_addr: alice,
            sender_devices: vec![alice_device_hash],
            signing_device_hash: alice_device_hash,
        };
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private_alice.sign(&signed_bytes);

        (
            state,
            signed,
            signature,
            signed_bytes,
            alice,
            alice_identity_pub,
        )
    }

    /// The revocable case: a cached signer whose ed25519 half CAN be placed
    /// in the revoked set (mirrors an enrolled #2 device key).
    fn cidnotify_verify_fixture() -> (
        OwnerState,
        crate::dm_envelope::DmCidNotifySigned,
        [u8; 64],
        Vec<u8>,
        OwnerAddr,
        [u8; 64],
    ) {
        cidnotify_verify_fixture_with_seed(0xA1)
    }

    /// The no-downgrade-hole case: a DIFFERENT cached signer, standing in for
    /// a legacy #3 identity key. `verify_cidnotify_sender_binding` runs the
    /// UNIFORM cutoff check on whatever ed25519 half is cached — a #3 key is
    /// simply never a member of `revoked_device_keys` (community enrollment
    /// only ever revokes #2 keys), so this fixture demonstrates the no-op: a
    /// signer whose own key isn't in the revoked set is admitted even when
    /// the projection is non-empty for the same owner.
    fn cidnotify_verify_fixture_device3() -> (
        OwnerState,
        crate::dm_envelope::DmCidNotifySigned,
        [u8; 64],
        Vec<u8>,
        OwnerAddr,
        [u8; 64],
    ) {
        cidnotify_verify_fixture_with_seed(0xC3)
    }

    #[test]
    fn cidnotify_from_revoked_device2_is_cut_off() {
        // Build state with a cached #2 signer whose ed25519 the projection revokes.
        let (state, signed, signature, signed_bytes, owner, combined_pub) =
            cidnotify_verify_fixture();
        let ed25519: [u8; 32] = combined_pub[32..64].try_into().unwrap();

        // Empty projection -> admitted.
        let clean = crate::revoked_device_projection::RevokedDeviceProjection::new();
        assert!(verify_cidnotify_sender_binding(
            &state,
            &signed,
            &signature,
            &signed_bytes,
            &clean
        )
        .is_ok());

        // Revoked projection -> SignerDeviceRevoked.
        let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
        revoked.union_from_members(std::iter::once((
            owner,
            &std::collections::BTreeSet::from([ed25519]),
        )));
        let err =
            verify_cidnotify_sender_binding(&state, &signed, &signature, &signed_bytes, &revoked)
                .expect_err("revoked signer must be cut off");
        assert_eq!(err, DmReceiveError::SignerDeviceRevoked);
    }

    #[test]
    fn cidnotify_cutoff_admits_non_revoked_key_of_revoked_owner() {
        // A #3 signer's cached combined pub — its ed25519 half is a #3
        // identity key, which is never an enrolled #2 key and so can never be
        // in revoked_device_keys. Even with a non-empty projection for the
        // owner, a #3 packet is admitted. (The verify boundary sees only a
        // cached 64-byte pub and cannot structurally distinguish #2 from #3;
        // what this pins is that a key ABSENT from a revoked owner's set is
        // admitted — guarding against a per-owner blanket drop / over-rejection
        // — which is exactly the no-downgrade-hole property for legacy #3.)
        let (state, signed, signature, signed_bytes, owner, _combined3) =
            cidnotify_verify_fixture_device3();
        let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
        // Revoke some OTHER key for the same owner (a #2 key that isn't this
        // #3 signer).
        revoked.union_from_members(std::iter::once((
            owner,
            &std::collections::BTreeSet::from([[0x99; 32]]),
        )));
        assert!(
            verify_cidnotify_sender_binding(&state, &signed, &signature, &signed_bytes, &revoked)
                .is_ok(),
            "legacy #3 signer is not subject to the cutoff (no downgrade hole)"
        );
    }

    /// ZEB-214: a cached signer (alice) + a 1:1 DM space [alice, me]. Returns
    /// the signer's private identity so the test can sign a read-receipt body
    /// with the SAME key the cache holds.
    fn read_receipt_verify_fixture() -> (
        OwnerState,
        harmony_identity::PrivateIdentity,
        DeviceIdentityHash,
        OwnerAddr, // alice (signer)
        OwnerAddr, // me (the other member)
        SpaceId,
    ) {
        let alice = OwnerAddr([0xA1; 16]);
        let me = OwnerAddr([0x1E; 16]);
        let space_id = SpaceId([0x5A; 16]);
        let mut state = OwnerState::default();
        let private_alice = harmony_identity::PrivateIdentity::from_seed(&[0xA1; 32]);
        let alice_pub = private_alice.public_identity();
        let alice_identity_pub = alice_pub.to_public_bytes();
        let alice_device_hash = DeviceIdentityHash(alice_pub.address_hash);
        state.apply_owner_device_update(
            alice,
            vec![alice_device_hash],
            vec![Some(alice_identity_pub)],
            vec![],
            Hlc {
                wall_ms: 50,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        );
        let mut members = vec![alice, me];
        members.sort();
        let space = Space {
            id: space_id,
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "dm".into(),
            transport: None,
            members,
            custom_name: None,
            notification_pref: None,
            read_receipt_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            updated_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            content_key: Some(DmContentKey::new([0x22; 32])),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        };
        state.spaces.insert(space_id, space);
        (state, private_alice, alice_device_hash, alice, me, space_id)
    }

    fn signed_receipt(
        priv_id: &harmony_identity::PrivateIdentity,
        device_hash: DeviceIdentityHash,
        sender: OwnerAddr,
        space_id: SpaceId,
    ) -> (crate::dm_envelope::DmReadReceiptSigned, [u8; 64], Vec<u8>) {
        let signed = crate::dm_envelope::DmReadReceiptSigned {
            space_id,
            sender_owner_addr: sender,
            signing_device_hash: device_hash,
            read_up_to: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "d".into(),
            },
            sent_at: Hlc {
                wall_ms: 1500,
                logical: 0,
                device_id: "d".into(),
            },
        };
        let bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let sig = priv_id.sign(&bytes);
        (signed, sig, bytes)
    }

    #[test]
    fn read_receipt_admission_accepts_valid_and_rejects_failures() {
        let (state, priv_alice, dev, alice, _me, space_id) = read_receipt_verify_fixture();
        let clean = crate::revoked_device_projection::RevokedDeviceProjection::new();

        // Valid → resolved sender owner.
        let (signed, sig, bytes) = signed_receipt(&priv_alice, dev, alice, space_id);
        assert_eq!(
            verify_read_receipt_admission(&state, &signed, &sig, &bytes, &clean).unwrap(),
            alice
        );

        // Tampered signed_bytes → signature fails.
        let mut bad = bytes.clone();
        bad[0] ^= 0xFF;
        assert!(matches!(
            verify_read_receipt_admission(&state, &signed, &sig, &bad, &clean),
            Err(DmReceiveError::SignatureVerificationFailed)
        ));

        // Owner-field mismatch: re-sign with a bogus sender_owner_addr.
        let mut wrong = signed.clone();
        wrong.sender_owner_addr = OwnerAddr([0x99; 16]);
        let wb = crate::owner_state_crypto::canonical_cbor_encode(&wrong).unwrap();
        let ws = priv_alice.sign(&wb);
        assert!(matches!(
            verify_read_receipt_admission(&state, &wrong, &ws, &wb, &clean),
            Err(DmReceiveError::OwnerFieldMismatch)
        ));

        // A receipt for a space we don't hold → SpaceNotFound.
        let (s2, sig2, b2) = signed_receipt(&priv_alice, dev, alice, SpaceId([0xEE; 16]));
        assert!(matches!(
            verify_read_receipt_admission(&state, &s2, &sig2, &b2, &clean),
            Err(DmReceiveError::SpaceNotFound)
        ));

        // Revoked signer device → SignerDeviceRevoked.
        let alice_combined = priv_alice.public_identity().to_public_bytes();
        let ed: [u8; 32] = alice_combined[32..64].try_into().unwrap();
        let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
        revoked.union_from_members(std::iter::once((
            alice,
            &std::collections::BTreeSet::from([ed]),
        )));
        assert!(matches!(
            verify_read_receipt_admission(&state, &signed, &sig, &bytes, &revoked),
            Err(DmReceiveError::SignerDeviceRevoked)
        ));
    }
}

#[cfg(test)]
mod outhold_write_tests {
    use super::*;
    use crate::content_store::InMemoryStub;
    use crate::owner_state_types::{DmContentKey, Space};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    fn make_outbox_synthetic_local(device_id: &str, self_owner: OwnerAddr) -> DmOutbox {
        let private_identity = harmony_identity::PrivateIdentity::from_seed(&[0x55; 32]);
        let priv_bytes = private_identity.to_private_bytes();
        let mut ed_seed = [0u8; 32];
        ed_seed.copy_from_slice(&priv_bytes[32..64]);
        let signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&ed_seed));
        let device_hash = DeviceIdentityHash(private_identity.identity.address_hash);
        let private_identity = std::sync::Arc::new(private_identity);
        let test_owner = crate::community_membership::mint_test_owner(0xAB);
        let community_signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(
            &test_owner.device_key.to_bytes(),
        ));
        let enrollment_cert = test_owner.cert;
        DmOutbox::new_synthetic(
            device_id.into(),
            self_owner,
            device_hash,
            signing_key,
            private_identity,
            community_signing_key,
            enrollment_cert,
        )
    }

    fn make_dm_space_local(id_byte: u8, members: Vec<OwnerAddr>) -> Space {
        Space {
            id: SpaceId([id_byte; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "Bob".into(),
            transport: None,
            members,
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "dev".into(),
            },
            updated_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "dev".into(),
            },
            content_key: Some(DmContentKey::new([0x42u8; 32])),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            read_receipt_pref: None,
            pending_join_at: None,
        }
    }

    fn install_space_local(state: &mut OwnerState, sp: Space) {
        let outcome = state.apply_space_with_canonicalization(sp);
        assert!(
            matches!(outcome, ApplyOutcome::Inserted),
            "fixture install must succeed, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn send_dm_writes_outhold_row_alongside_outbox_entry() {
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let sp = make_dm_space_local(7, vec![alice, bob]);
        let space_id = sp.id;
        install_space_local(&mut state, sp);

        let cas = InMemoryStub::default();
        let mut o = make_outbox_synthetic_local("dev", alice);

        // Install outhold doc + notify flag.
        let outhold_doc = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::dm_outhold::DmOutholdDoc::default(),
        ));
        let notify_fired = Arc::new(AtomicBool::new(false));
        let notify_fired_clone = Arc::clone(&notify_fired);
        let notify: std::sync::Arc<dyn Fn() + Send + Sync> =
            std::sync::Arc::new(move || notify_fired_clone.store(true, Ordering::SeqCst));
        o.set_outhold(outhold_doc.clone(), notify);

        let (_msg_id, msg_cid) = o
            .send_dm(
                &mut state,
                &cas,
                space_id,
                b"hello outhold".to_vec(),
                "text/plain".into(),
                1_000,
                None,
            )
            .await
            .expect("send_dm ok");

        // (a) The doc must contain the expected key.
        let expected_key = crate::dm_outhold::DmOutholdDoc::key(&space_id.0, &msg_cid.to_bytes());
        let doc_guard = outhold_doc.lock().await;
        assert!(
            doc_guard.entries.contains_key(&expected_key),
            "outhold doc must contain key for the returned message_cid"
        );

        // (b) The blob in the outhold doc must equal the blob stored in CAS.
        let outhold_blob = doc_guard.entries[&expected_key].storage_blob.clone();
        drop(doc_guard);
        let cas_blob = cas
            .get(&msg_cid)
            .await
            .expect("CAS get ok")
            .expect("CAS must hold the blob");
        assert_eq!(
            outhold_blob, cas_blob,
            "outhold storage_blob must match what was written to CAS"
        );

        // (c) The notify closure must have fired.
        assert!(
            notify_fired.load(Ordering::SeqCst),
            "notify closure must fire after a successful hold write"
        );
    }

    #[tokio::test]
    async fn send_dm_without_outhold_installed_unchanged() {
        // No set_outhold → send_dm must succeed without panicking.
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let bob = OwnerAddr([0x02; 16]);
        let sp = make_dm_space_local(7, vec![alice, bob]);
        let space_id = sp.id;
        install_space_local(&mut state, sp);

        let cas = InMemoryStub::default();
        let mut o = make_outbox_synthetic_local("dev", alice);
        // outhold_doc is None by default — no set_outhold call.

        let result = o
            .send_dm(
                &mut state,
                &cas,
                space_id,
                b"no outhold".to_vec(),
                "text/plain".into(),
                1_000,
                None,
            )
            .await;
        assert!(
            result.is_ok(),
            "send_dm without outhold must succeed: {result:?}"
        );
    }

    #[tokio::test]
    async fn send_dm_rejected_entry_writes_no_outhold_row() {
        // Drive a rejection via self-only DM (space has only alice as member
        // after the space is mutated post-construction to bypass the
        // canonical invariant check, mirroring send_dm_self_only_dm_rejects).
        let mut state = OwnerState::default();
        let alice = OwnerAddr([0x01; 16]);
        let mut sp = make_dm_space_local(7, vec![alice, OwnerAddr([0x02; 16])]);
        // Mutate to single-member after construction; insert directly to skip
        // apply_space_with_canonicalization's invariant check.
        sp.members = vec![alice];
        let space_id = sp.id;
        state.spaces.insert(space_id, sp);

        let cas = InMemoryStub::default();
        let mut o = make_outbox_synthetic_local("dev", alice);

        let outhold_doc = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::dm_outhold::DmOutholdDoc::default(),
        ));
        let notify_fired = Arc::new(AtomicBool::new(false));
        let notify_fired_clone = Arc::clone(&notify_fired);
        let notify: std::sync::Arc<dyn Fn() + Send + Sync> =
            std::sync::Arc::new(move || notify_fired_clone.store(true, Ordering::SeqCst));
        o.set_outhold(outhold_doc.clone(), notify);

        let err = o
            .send_dm(
                &mut state,
                &cas,
                space_id,
                b"self-only".to_vec(),
                "text/plain".into(),
                1_000,
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, SendDmError::NoRecipients(id) if id == space_id),
            "expected NoRecipients, got {err:?}"
        );

        // The outhold doc must remain empty — no row written on rejection.
        let doc_guard = outhold_doc.lock().await;
        assert!(
            doc_guard.entries.is_empty(),
            "outhold doc must stay empty when send_dm is rejected"
        );
        drop(doc_guard);

        // The notify closure must NOT have fired.
        assert!(
            !notify_fired.load(Ordering::SeqCst),
            "notify must not fire when send_dm is rejected"
        );
    }
}
