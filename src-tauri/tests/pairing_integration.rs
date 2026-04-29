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
    persist::{install_inviter_state_inner, install_joiner_state_inner},
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

/// Critical regression test for the post-Complete persistence wiring.
///
/// Reproduces the bug found by the final ZEB-197 reviewer: the SM emitted
/// {Joiner,Inviter}EnrollResult, but nothing in `start_node` drained the
/// receivers, so the on-disk `.cbor` was never updated. The integration
/// test below pairs two devices, drains BOTH receivers, calls the
/// persistence helpers against per-side tempdirs, and asserts that:
///
///   - Inviter side: `owner_state.cbor` reflects the new enrollment;
///     `master_seed.enc` is preserved (Inviter keeps master);
///     `device_sk.enc` is preserved (existing signing key untouched).
///   - Joiner side: `owner_state.cbor` exists with both enrollments;
///     `device_sk.enc` exists; `master_seed.enc` does NOT exist
///     (cert-only model — Joiner has no master).
#[tokio::test]
async fn end_to_end_persists_state_to_disk() {
    use harmony_app::owner_state::{load_owner_state, save_owner_state_atomic};
    use tempfile::tempdir;

    // Inviter side: fresh mint persisted to disk so install_inviter_state
    // has something to update. The encrypted-file fallback needs a passphrase.
    std::env::set_var("HARMONY_PASSPHRASE", "test-pp");
    let inviter_dir = tempdir().unwrap();
    let joiner_dir = tempdir().unwrap();

    let MintResult {
        state: original_state,
        recovery_artifact,
        device_signing_key: inviter_signing_key,
    } = mint_owner(1_700_000_000).unwrap();
    let master_seed_bytes: [u8; 32] = *recovery_artifact.as_bytes();
    save_owner_state_atomic(
        inviter_dir.path(),
        &original_state,
        &inviter_signing_key,
        Some(&master_seed_bytes),
        None,
    )
    .unwrap();
    let original_inviter_signing_bytes = inviter_signing_key.to_bytes();

    let master_seed = Zeroizing::new(master_seed_bytes);
    let joiner_sk = SigningKey::generate(&mut OsRng);

    let (inviter_t, joiner_t) = InMemoryBroker::pair();
    let inviter_t_arc: Arc<dyn PairingTransport> = Arc::new(inviter_t);
    let joiner_t_arc: Arc<dyn PairingTransport> = Arc::new(joiner_t);
    let now_fn: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| 1_700_000_001);
    let inviter_handle = spawn_state_machine(inviter_t_arc, now_fn.clone());
    let joiner_handle = spawn_state_machine(joiner_t_arc, now_fn.clone());

    inviter_handle
        .cmd_tx
        .send(PairingCommand::StartInviter {
            display_name: "KRILE".to_string(),
            owner_state: original_state.clone(),
            master_seed,
        })
        .await
        .unwrap();
    joiner_handle
        .cmd_tx
        .send(PairingCommand::StartJoiner {
            display_name: "AVALON".to_string(),
            signing_key: joiner_sk,
        })
        .await
        .unwrap();

    drive_to_handshake(&inviter_handle, &joiner_handle).await;
    drive_to_complete(&inviter_handle, &joiner_handle).await;

    // Drain BOTH result receivers — this is exactly what the start_node
    // drainer tasks do in production.
    let mut joiner_rx = joiner_handle.joiner_result_rx.expect("joiner result rx");
    let joiner_result = timeout(Duration::from_secs(2), joiner_rx.recv())
        .await
        .expect("joiner result arrives")
        .expect("not None");
    let mut inviter_rx = inviter_handle.inviter_result_rx.expect("inviter result rx");
    let inviter_result = timeout(Duration::from_secs(2), inviter_rx.recv())
        .await
        .expect("inviter result arrives")
        .expect("not None");

    // Persist both sides to their respective tempdirs. Use the *_inner
    // variants with `keychain: None` so we exercise the encrypted-file
    // fallback (HARMONY_PASSPHRASE) without polluting the developer's real
    // OS keychain — see persist.rs for why the public wrappers can't be
    // used here.
    let new_device_id = joiner_result.our_device_id;
    install_joiner_state_inner(joiner_dir.path(), joiner_result, None).expect("joiner persist");
    install_inviter_state_inner(inviter_dir.path(), inviter_result, None, None)
        .expect("inviter persist");

    // ── Inviter side assertions ───────────────────────────────────────
    let reloaded_inviter = load_owner_state(inviter_dir.path(), None)
        .expect("load inviter")
        .expect("inviter state present");
    assert!(
        reloaded_inviter
            .state
            .enrollments
            .contains_key(&new_device_id),
        "Inviter's persisted state contains the new joiner's enrollment"
    );
    assert_eq!(
        reloaded_inviter.state.enrollments.len(),
        original_state.enrollments.len() + 1,
        "Inviter's persisted state has exactly one new enrollment"
    );
    assert_eq!(
        reloaded_inviter.device_signing_key.to_bytes(),
        original_inviter_signing_bytes,
        "Inviter's signing key preserved across pairing+persist"
    );
    let reloaded_inviter_seed = reloaded_inviter
        .master_seed
        .expect("Inviter master seed preserved");
    assert_eq!(
        *reloaded_inviter_seed, master_seed_bytes,
        "Inviter's master seed preserved across pairing+persist"
    );

    // ── Joiner side assertions ────────────────────────────────────────
    let joiner_cbor = joiner_dir.path().join("owner_state.cbor");
    assert!(joiner_cbor.exists(), "Joiner owner_state.cbor written");
    let joiner_master = joiner_dir.path().join("master_seed.enc");
    assert!(
        !joiner_master.exists(),
        "Joiner master_seed.enc must NOT exist (cert-only model)"
    );
    let joiner_device_sk = joiner_dir.path().join("device_sk.enc");
    assert!(joiner_device_sk.exists(), "Joiner device_sk.enc written");
    let reloaded_joiner = load_owner_state(joiner_dir.path(), None)
        .expect("load joiner")
        .expect("joiner state present");
    assert!(
        reloaded_joiner
            .state
            .enrollments
            .contains_key(&new_device_id),
        "Joiner's persisted state contains its own enrollment"
    );
    assert!(
        reloaded_joiner.master_seed.is_none(),
        "Joiner has no master seed on disk"
    );
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
