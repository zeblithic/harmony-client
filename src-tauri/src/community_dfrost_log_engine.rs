//! D-FROST per-community signed-event log engine. Mirrors the
//! `community_voting_log_engine.rs` pattern: one topic per community
//! at `harmony/community/{community_id}/dfrost`; mpsc-based publisher
//! and subscriber channels bridged to Zenoh by the event-loop adapter
//! (deferred — out of scope for ZEB-307; this ticket ships the engine,
//! a follow-up ships the adapter).

use crate::community_dfrost_log::DfrostLog;
use crate::community_dfrost_types::SignedCommitteeEvent;
use crate::owner_state_types::{OwnerAddr, SpaceId};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Replay-defense tracker keyed on `(actor, device_id)`. Records the
/// max-observed `(wall_ms, logical)` HLC per signer; any inbound event
/// whose HLC is at-or-below the recorded max is considered a replay /
/// loopback and silently dropped.
#[derive(Default)]
pub struct DfrostReplayTracker {
    seen_max: HashMap<(OwnerAddr, String), (u64, u32)>,
}

impl DfrostReplayTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contains(&self, event: &SignedCommitteeEvent) -> bool {
        match self
            .seen_max
            .get(&(event.actor, event.hlc.device_id.clone()))
        {
            Some((w, l)) => (event.hlc.wall_ms, event.hlc.logical) <= (*w, *l),
            None => false,
        }
    }

    pub fn record(&mut self, event: &SignedCommitteeEvent) {
        let key = (event.actor, event.hlc.device_id.clone());
        let new_hlc = (event.hlc.wall_ms, event.hlc.logical);
        self.seen_max
            .entry(key)
            .and_modify(|cur| {
                if new_hlc > *cur {
                    *cur = new_hlc;
                }
            })
            .or_insert(new_hlc);
    }
}

/// Parameters bundle for `DfrostLogEngine::start`. Tauri-runtime-generic so
/// tests can pass `tauri::test::MockRuntime` and production can pass the
/// default Wry runtime.
pub struct DfrostLogEngineParams<R: tauri::Runtime> {
    pub community_id: SpaceId,
    pub dfrost_log: Arc<Mutex<DfrostLog>>,
    pub publisher_tx: mpsc::Sender<Vec<u8>>,
    pub subscriber_rx: mpsc::Receiver<Vec<u8>>,
    pub app_handle: tauri::AppHandle<R>,
    pub self_addr: OwnerAddr,
    pub self_x25519_priv: [u8; 32],
}

/// Per-community D-FROST signed-event engine. Owns the inbound receive loop
/// and a handle to the publish side; both wire up to a Zenoh topic adapter
/// in a follow-up ticket.
pub struct DfrostLogEngine<R: tauri::Runtime> {
    community_id: SpaceId,
    // `dfrost_log`, `tracker`, and `publisher_tx` are wired into IPC commands
    // and the receive loop in Tasks 3+. Holding them on the engine now keeps
    // the public construction shape stable across the task sequence.
    #[allow(dead_code)]
    dfrost_log: Arc<Mutex<DfrostLog>>,
    #[allow(dead_code)]
    tracker: Arc<Mutex<DfrostReplayTracker>>,
    #[allow(dead_code)]
    publisher_tx: mpsc::Sender<Vec<u8>>,
    // JoinHandle held to abort-on-drop the receive task. Read in Task 6 when
    // we add a shutdown path; for now the implicit Drop is sufficient.
    #[allow(dead_code)]
    _receive_handle: tokio::task::JoinHandle<()>,
    _phantom: std::marker::PhantomData<R>,
}

impl<R: tauri::Runtime> DfrostLogEngine<R> {
    pub fn community_id(&self) -> SpaceId {
        self.community_id
    }

    pub async fn start(params: DfrostLogEngineParams<R>) -> Arc<Self> {
        let tracker = Arc::new(Mutex::new(DfrostReplayTracker::new()));
        let community_id = params.community_id;
        let log_for_loop = params.dfrost_log.clone();
        let tracker_for_loop = tracker.clone();
        let app_for_loop = params.app_handle;
        let self_addr_for_loop = params.self_addr;
        let self_x_priv_for_loop = params.self_x25519_priv;
        let mut rx = params.subscriber_rx;

        let receive_handle = tokio::spawn(async move {
            while let Some(packet) = rx.recv().await {
                // Task 3+ will populate `process_inbound`. For now drop the
                // packet to validate the startup shape and capture every
                // future-used dependency by reference so the closure type
                // is stable.
                let _ = (
                    &community_id,
                    &log_for_loop,
                    &tracker_for_loop,
                    &app_for_loop,
                    &self_addr_for_loop,
                    &self_x_priv_for_loop,
                    packet,
                );
            }
        });

        Arc::new(Self {
            community_id,
            dfrost_log: params.dfrost_log,
            tracker,
            publisher_tx: params.publisher_tx,
            _receive_handle: receive_handle,
            _phantom: std::marker::PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::community_dfrost_log_engine::{
        DfrostLogEngine, DfrostLogEngineParams, DfrostReplayTracker,
    };
    use crate::community_dfrost_types::{
        DfrostEventKind, SignedCommitteeEvent, ThresholdSignPayload,
    };
    use crate::owner_state_types::{Hlc, OwnerAddr};

    fn test_event(actor: OwnerAddr, wall_ms: u64, logical: u32) -> SignedCommitteeEvent {
        let payload = ThresholdSignPayload {
            ceremony_id: [0u8; 32],
            message_hash: [0u8; 32],
            commitment_bytes: vec![],
            share_bytes: vec![],
        };
        let mut payload_bytes = Vec::new();
        ciborium::ser::into_writer(&payload, &mut payload_bytes).unwrap();
        SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::ThresholdSign,
            hlc: Hlc {
                wall_ms,
                logical,
                device_id: "dev-a".into(),
            },
            actor,
            payload: payload_bytes,
            sig: vec![0u8; 64],
        }
    }

    #[test]
    fn replay_tracker_dedups_repeat_event() {
        let mut t = DfrostReplayTracker::new();
        let addr = OwnerAddr([1u8; 16]);
        let e = test_event(addr, 100, 0);
        assert!(!t.contains(&e), "fresh event not contained");
        t.record(&e);
        assert!(t.contains(&e), "recorded event is contained");
    }

    #[test]
    fn replay_tracker_dedups_per_actor_device() {
        let mut t = DfrostReplayTracker::new();
        let addr_a = OwnerAddr([1u8; 16]);
        let addr_b = OwnerAddr([2u8; 16]);
        t.record(&test_event(addr_a, 100, 0));
        assert!(
            !t.contains(&test_event(addr_b, 100, 0)),
            "different actor not deduped"
        );
    }

    #[test]
    fn replay_tracker_advances_on_higher_hlc() {
        let mut t = DfrostReplayTracker::new();
        let addr = OwnerAddr([1u8; 16]);
        t.record(&test_event(addr, 100, 0));
        let later = test_event(addr, 100, 1);
        assert!(!t.contains(&later), "advancing logical not deduped");
        t.record(&later);
        assert!(t.contains(&later), "advanced event recorded");
        assert!(t.contains(&test_event(addr, 100, 0)));
    }

    #[tokio::test]
    async fn engine_start_returns_handle_and_drops_cleanly() {
        let (pub_tx, _pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let log = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::community_dfrost_log::DfrostLog::new(),
        ));
        let community_id = crate::owner_state_types::SpaceId([0u8; 16]);
        let app = tauri::test::mock_app();
        let app_handle = app.handle().clone();

        let engine = DfrostLogEngine::start(DfrostLogEngineParams {
            community_id,
            dfrost_log: log.clone(),
            publisher_tx: pub_tx,
            subscriber_rx: sub_rx,
            app_handle,
            self_addr: crate::owner_state_types::OwnerAddr([0u8; 16]),
            self_x25519_priv: [0u8; 32],
        })
        .await;

        assert_eq!(engine.community_id(), community_id);
        drop(sub_tx); // signal end-of-stream
        drop(engine);
    }
}
