use crate::pairing::types::PairingWireMessage;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

#[async_trait]
pub trait PairingTransport: Send + Sync {
    /// Publish a wire message to the pairing scope. Returns Ok once the
    /// transport has accepted the message for delivery (not necessarily
    /// once the peer has acked — Zenoh pub/sub is best-effort).
    async fn publish(&self, message: PairingWireMessage) -> Result<(), String>;

    /// Receive the next wire message from any peer in the pairing scope.
    /// Returns None when the transport is shut down.
    async fn recv(&self) -> Option<PairingWireMessage>;
}

/// In-memory transport for tests: `InMemoryBroker::pair()` returns two
/// `InMemoryTransport` endpoints. Anything published on one is delivered
/// to the other (and only the other — not echoed back to the sender).
pub struct InMemoryBroker;

pub struct InMemoryTransport {
    /// Sender used by THIS side's publish() to push to the OTHER side.
    publish_tx: mpsc::Sender<PairingWireMessage>,
    /// Receiver this side uses to pull incoming messages.
    recv_rx: Arc<Mutex<mpsc::Receiver<PairingWireMessage>>>,
}

impl InMemoryBroker {
    pub fn pair() -> (InMemoryTransport, InMemoryTransport) {
        // Channel capacity 64 is generous for the test scenario (a happy path
        // involves <10 messages); the real Zenoh transport has no fixed buffer.
        let (a_tx, a_rx) = mpsc::channel(64);
        let (b_tx, b_rx) = mpsc::channel(64);
        let side_a = InMemoryTransport {
            publish_tx: b_tx, // A publishes → goes to B's recv
            recv_rx: Arc::new(Mutex::new(a_rx)),
        };
        let side_b = InMemoryTransport {
            publish_tx: a_tx, // B publishes → goes to A's recv
            recv_rx: Arc::new(Mutex::new(b_rx)),
        };
        (side_a, side_b)
    }
}

#[async_trait]
impl PairingTransport for InMemoryTransport {
    async fn publish(&self, message: PairingWireMessage) -> Result<(), String> {
        self.publish_tx
            .send(message)
            .await
            .map_err(|e| format!("in-memory publish: {e}"))
    }

    async fn recv(&self) -> Option<PairingWireMessage> {
        self.recv_rx.lock().await.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::types::{PairingRole, PairingWireMessage};
    use uuid::Uuid;

    #[tokio::test]
    async fn in_memory_broker_routes_messages_one_way() {
        let (a, b) = InMemoryBroker::pair();
        let msg = PairingWireMessage::Discover {
            session_id: Uuid::nil(),
            role: PairingRole::Inviter,
            ephemeral_pubkey_hex: "00".repeat(32),
            display_name: "test".to_string(),
            owner_id_if_inviter: None,
            joiner_ed25519_verify_hex: None,
        };
        a.publish(msg.clone()).await.unwrap();
        let received = b.recv().await.unwrap();
        match received {
            PairingWireMessage::Discover { display_name, .. } => {
                assert_eq!(display_name, "test");
            }
            _ => panic!("wrong variant"),
        }
        // Confirm A didn't echo back to itself.
        match tokio::time::timeout(std::time::Duration::from_millis(1), a.recv()).await {
            Ok(Some(msg)) => panic!("A should not receive its own message: {:?}", msg),
            Ok(None) => panic!("A's transport closed unexpectedly"),
            Err(_) => {} // expected: timed out, no echo
        }
    }
}
