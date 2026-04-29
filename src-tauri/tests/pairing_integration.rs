//! End-to-end test for Track B v2 pairing.
//!
//! Spawns two state machines on linked InMemoryBroker transports (this is the
//! same primitive used by the unit tests in pairing/state_machine.rs but in
//! the integration-test position). We don't spin up two real Zenoh sessions
//! here — that would intersect with ZEB-165's UDP-port collision. The
//! transport abstraction guarantees behaviour is the same modulo Zenoh.
//!
//! Asserts: both reach Complete; Joiner's installed OwnerState contains both
//! enrollments; the master_seed bytes never appear in any wire payload
//! captured during the run.

use ed25519_dalek::SigningKey;
use harmony_app::pairing::{
    state_machine::{spawn_state_machine, PairingCommand, PairingHandle},
    transport::{InMemoryBroker, PairingTransport},
    types::{PairingState, PairingWireMessage},
};
use harmony_owner::lifecycle::{mint_owner, MintResult};
use rand::rngs::OsRng;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::timeout;
use zeroize::Zeroizing;

/// A wrapping transport that captures every published wire message for
/// post-test inspection.
struct CapturingTransport {
    inner: Arc<dyn PairingTransport>,
    captured: Arc<Mutex<Vec<PairingWireMessage>>>,
}

#[async_trait::async_trait]
impl PairingTransport for CapturingTransport {
    async fn publish(&self, message: PairingWireMessage) -> Result<(), String> {
        self.captured.lock().unwrap().push(message.clone());
        self.inner.publish(message).await
    }
    async fn recv(&self) -> Option<PairingWireMessage> {
        self.inner.recv().await
    }
}

#[tokio::test]
async fn end_to_end_pair_two_devices() {
    let MintResult {
        state,
        recovery_artifact,
        ..
    } = mint_owner(1_700_000_000).unwrap();
    let master_seed_bytes = *recovery_artifact.as_bytes();
    let master_seed = Zeroizing::new(master_seed_bytes);

    let joiner_sk = SigningKey::generate(&mut OsRng);

    let (inviter_t, joiner_t) = InMemoryBroker::pair();
    let inviter_captured = Arc::new(Mutex::new(Vec::new()));
    let joiner_captured = Arc::new(Mutex::new(Vec::new()));
    let inviter_t = Arc::new(CapturingTransport {
        inner: Arc::new(inviter_t),
        captured: inviter_captured.clone(),
    });
    let joiner_t = Arc::new(CapturingTransport {
        inner: Arc::new(joiner_t),
        captured: joiner_captured.clone(),
    });

    let now_fn: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| 1_700_000_001);
    let inviter_handle = spawn_state_machine(inviter_t.clone(), now_fn.clone());
    let joiner_handle = spawn_state_machine(joiner_t.clone(), now_fn.clone());

    inviter_handle
        .cmd_tx
        .send(PairingCommand::StartInviter {
            display_name: "KRILE".to_string(),
            owner_state: state.clone(),
            master_seed,
        })
        .await
        .unwrap();
    joiner_handle
        .cmd_tx
        .send(PairingCommand::StartJoiner {
            display_name: "AVALON".to_string(),
            signing_key: joiner_sk.clone(),
        })
        .await
        .unwrap();

    drive_to_handshake(&inviter_handle, &joiner_handle).await;
    drive_to_complete(&inviter_handle, &joiner_handle).await;

    let mut jrx = joiner_handle.joiner_result_rx.expect("result rx");
    let result = timeout(Duration::from_secs(2), jrx.recv())
        .await
        .expect("joiner result")
        .expect("not None");

    // Joiner state has both enrollments.
    assert!(result.owner_state.enrollments.len() >= 2);
    assert!(result
        .owner_state
        .enrollments
        .contains_key(&result.our_device_id));

    // master_seed never appears in any captured wire payload.
    let inviter_msgs = inviter_captured.lock().unwrap().clone();
    let joiner_msgs = joiner_captured.lock().unwrap().clone();

    // Sanity check: we actually captured messages (prove the capture mechanism works).
    assert!(
        !inviter_msgs.is_empty() || !joiner_msgs.is_empty(),
        "expected at least one captured wire message"
    );

    for msg in inviter_msgs.iter().chain(joiner_msgs.iter()) {
        let mut bytes = Vec::new();
        ciborium::into_writer(msg, &mut bytes).unwrap();
        assert!(
            !bytes.windows(32).any(|w| w == master_seed_bytes),
            "master_seed leaked in {msg:?}"
        );
    }
}

async fn drive_to_handshake(inviter_handle: &PairingHandle, joiner_handle: &PairingHandle) {
    let mut inviter_state = inviter_handle.state_rx.clone();
    let mut joiner_state = joiner_handle.state_rx.clone();

    timeout(Duration::from_secs(2), async {
        loop {
            inviter_state.changed().await.unwrap();
            if matches!(*inviter_state.borrow(), PairingState::Discovered { .. }) {
                break;
            }
        }
    })
    .await
    .expect("inviter discovers");

    timeout(Duration::from_secs(2), async {
        loop {
            joiner_state.changed().await.unwrap();
            if matches!(*joiner_state.borrow(), PairingState::Discovered { .. }) {
                break;
            }
        }
    })
    .await
    .expect("joiner discovers");

    let inviter_peer = match &*inviter_handle.state_rx.borrow() {
        PairingState::Discovered { peers } => peers[0].session_id,
        _ => panic!(),
    };
    let joiner_peer = match &*joiner_handle.state_rx.borrow() {
        PairingState::Discovered { peers } => peers[0].session_id,
        _ => panic!(),
    };
    inviter_handle
        .cmd_tx
        .send(PairingCommand::SelectPeer {
            peer_session_id: inviter_peer,
        })
        .await
        .unwrap();
    joiner_handle
        .cmd_tx
        .send(PairingCommand::SelectPeer {
            peer_session_id: joiner_peer,
        })
        .await
        .unwrap();

    timeout(Duration::from_secs(2), async {
        loop {
            inviter_state.changed().await.unwrap();
            if matches!(*inviter_state.borrow(), PairingState::Handshaking { .. }) {
                break;
            }
        }
    })
    .await
    .expect("inviter handshake");
    timeout(Duration::from_secs(2), async {
        loop {
            joiner_state.changed().await.unwrap();
            if matches!(*joiner_state.borrow(), PairingState::Handshaking { .. }) {
                break;
            }
        }
    })
    .await
    .expect("joiner handshake");
}

async fn drive_to_complete(inviter_handle: &PairingHandle, joiner_handle: &PairingHandle) {
    inviter_handle
        .cmd_tx
        .send(PairingCommand::ConfirmSas)
        .await
        .unwrap();
    joiner_handle
        .cmd_tx
        .send(PairingCommand::ConfirmSas)
        .await
        .unwrap();

    let mut joiner_state = joiner_handle.state_rx.clone();
    timeout(Duration::from_secs(3), async {
        loop {
            joiner_state.changed().await.unwrap();
            if matches!(*joiner_state.borrow(), PairingState::Complete { .. }) {
                break;
            }
        }
    })
    .await
    .expect("joiner completes");

    let mut inviter_state = inviter_handle.state_rx.clone();
    timeout(Duration::from_secs(3), async {
        loop {
            inviter_state.changed().await.unwrap();
            if matches!(*inviter_state.borrow(), PairingState::Complete { .. }) {
                break;
            }
        }
    })
    .await
    .expect("inviter completes");
}
