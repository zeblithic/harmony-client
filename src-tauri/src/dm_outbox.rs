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
//!   - Add `handle_unicast` for inbound `DmInvite`/`DmCidNotify`/`DmAck`
//!     demux (which routes `DmAck` packets through `handle_ack`).

// Imports staged for subsequent Phase 2 tasks (DmOutbox orchestrator,
// send_dm, drain, handle_ack). Allowed-unused at module level so the
// Phase 2 Task 1 skeleton compiles cleanly under `clippy -D warnings`;
// later tasks consume them.
#[allow(unused_imports)]
use crate::content_store::{ContentStore, ContentStoreError};
#[allow(unused_imports)]
use crate::dm_crypto::{compute_aad, encrypt_dm_message, DmEncryptError};
#[allow(unused_imports)]
use crate::dm_envelope::MessagePayload;
#[allow(unused_imports)]
use crate::owner_state_crdt::{ApplyOutcome, OwnerState, RejectionReason};
#[allow(unused_imports)]
use crate::owner_state_types::{
    ContentId, DeliveryStatus, Hlc, OutboxEntry, OutboxEntryId, OwnerAddr, Space, SpaceId,
    SpaceKind,
};
use async_trait::async_trait;
#[allow(unused_imports)]
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Mutex;

pub type MessageId = OutboxEntryId;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("transport temporarily unavailable: {0}")]
    Transient(String),
    #[error("transport permanently failed: {0}")]
    Permanent(String),
}

#[async_trait]
pub trait DmTransport: Send + Sync {
    async fn send(&self, entry: &OutboxEntry, recipient: OwnerAddr) -> Result<(), TransportError>;
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
    sends: Vec<(OutboxEntryId, OwnerAddr)>,
    /// Pre-seeded outcomes; if absent, default = Ok(()).
    outcomes: HashMap<(OutboxEntryId, OwnerAddr), Result<(), TransportError>>,
}

impl StubTransport {
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

    /// Snapshot all recorded sends (in call order).
    pub fn sends(&self) -> Vec<(OutboxEntryId, OwnerAddr)> {
        self.inner
            .lock()
            .expect("StubTransport poisoned")
            .sends
            .clone()
    }
}

// `TransportError` is not Clone (thiserror + io-style errors rarely are).
// `remove` instead of `get/clone` so each pre-seeded outcome fires once;
// repeat calls without re-seeding fall through to the default Ok(()).
#[async_trait]
impl DmTransport for StubTransport {
    async fn send(&self, entry: &OutboxEntry, recipient: OwnerAddr) -> Result<(), TransportError> {
        let mut inner = self.inner.lock().expect("StubTransport poisoned");
        inner.sends.push((entry.id, recipient));
        inner
            .outcomes
            .remove(&(entry.id, recipient))
            .unwrap_or(Ok(()))
    }
}

#[cfg(test)]
mod stub_tests {
    use super::*;
    use crate::owner_state_types::ContentId;
    use std::collections::BTreeSet;

    fn entry(id: u8) -> OutboxEntry {
        OutboxEntry {
            id: OutboxEntryId([id; 16]),
            space_id: SpaceId([1u8; 16]),
            recipient_owners: vec![OwnerAddr([2u8; 16])],
            message_cid: ContentId::from_bytes([3u8; 32]),
            created_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "test".into(),
            },
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        }
    }

    #[tokio::test]
    async fn stub_records_sends_and_returns_default_ok() {
        let t = StubTransport::new();
        let e = entry(1);
        let r = OwnerAddr([2u8; 16]);
        let res = t.send(&e, r).await;
        assert!(res.is_ok(), "default outcome is Ok: {res:?}");
        assert_eq!(t.sends(), vec![(e.id, r)]);
    }
}
