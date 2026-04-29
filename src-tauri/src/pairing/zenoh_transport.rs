use crate::pairing::transport::PairingTransport;
use crate::pairing::types::PairingWireMessage;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Zenoh-backed transport. Publishes go through `publish_tx` (the existing
/// PublishRequest channel into the event loop). Receives are pumped from
/// `pairing_in_rx`, which the event loop fills with samples on
/// `harmony/pairing/v2/lan/**` keys.
///
/// The subscription on `harmony/pairing/v2/lan/**` is always-on at the Zenoh
/// level (declared in event_loop startup_actions), but the state machine
/// only acts on incoming messages when an in-progress session context exists
/// (per the `if ctx.is_some()` guard). So idle devices receive but discard
/// other devices' pairing broadcasts — preserving the "no idle broadcasting,
/// pairing mode is opt-in" property at the publish layer.
pub struct ZenohPairingTransport {
    publish_tx: mpsc::Sender<crate::event_loop::PublishRequest>,
    pairing_in_rx: Arc<Mutex<mpsc::Receiver<PairingWireMessage>>>,
}

impl ZenohPairingTransport {
    pub fn new(
        publish_tx: mpsc::Sender<crate::event_loop::PublishRequest>,
        pairing_in_rx: mpsc::Receiver<PairingWireMessage>,
    ) -> Self {
        Self {
            publish_tx,
            pairing_in_rx: Arc::new(Mutex::new(pairing_in_rx)),
        }
    }
}

const PAIRING_KEY_PREFIX: &str = "harmony/pairing/v2/lan";

fn key_for(message: &PairingWireMessage) -> String {
    let session_id = match message {
        PairingWireMessage::Discover { session_id, .. } => session_id,
        PairingWireMessage::Select { my_session_id, .. } => my_session_id,
        PairingWireMessage::Encrypted { my_session_id, .. } => my_session_id,
        PairingWireMessage::Cancel { my_session_id, .. } => my_session_id,
    };
    let phase = match message {
        PairingWireMessage::Discover { .. } => "discover",
        PairingWireMessage::Select { .. } => "select",
        PairingWireMessage::Encrypted { .. } => "encrypted",
        PairingWireMessage::Cancel { .. } => "cancel",
    };
    format!("{PAIRING_KEY_PREFIX}/{session_id}/{phase}")
}

#[async_trait]
impl PairingTransport for ZenohPairingTransport {
    async fn publish(&self, message: PairingWireMessage) -> Result<(), String> {
        let key = key_for(&message);
        let mut payload = Vec::new();
        ciborium::into_writer(&message, &mut payload).map_err(|e| format!("cbor: {e}"))?;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.publish_tx
            .send(crate::event_loop::PublishRequest {
                key_expr: key,
                payload,
                reply: reply_tx,
            })
            .await
            .map_err(|_| "event loop not running".to_string())?;
        reply_rx
            .await
            .map_err(|_| "publish reply dropped".to_string())?
    }

    async fn recv(&self) -> Option<PairingWireMessage> {
        self.pairing_in_rx.lock().await.recv().await
    }
}
