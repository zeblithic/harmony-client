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
    state_machine::{spawn_state_machine, InviterEnrollResult, PairingCommand, PairingHandle},
    transport::{InMemoryBroker, PairingTransport},
    types::{PairingState, PairingWireMessage},
};
use harmony_owner::lifecycle::{mint_owner, MintResult};
use rand::rngs::OsRng;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::timeout;
use zeroize::Zeroizing;

/// RAII guard mirroring the unit-test pattern in `pairing/persist.rs`:
/// sets an env var on construction and removes it on drop, including on
/// panic. Without this, a panicking test leaks `HARMONY_PASSPHRASE` into
/// any later test in the same process.
struct EnvVarGuard {
    name: &'static str,
}
impl EnvVarGuard {
    fn set(name: &'static str, value: &str) -> Self {
        std::env::set_var(name, value);
        Self { name }
    }
}
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        std::env::remove_var(self.name);
    }
}

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
    // Long interval — these tests don't exercise rebroadcast; the per-event
    // master-seed leak scan would still trip on a re-emitted DISCOVER but
    // we'd rather not trade correctness checks for incidental message
    // volume in this test.
    let test_interval = Duration::from_secs(60);
    let mut inviter_handle = spawn_state_machine(inviter_t.clone(), now_fn.clone(), test_interval);
    let joiner_handle = spawn_state_machine(joiner_t.clone(), now_fn.clone(), test_interval);

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
    drive_to_complete(&mut inviter_handle, &joiner_handle).await;

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

    // PR #63 review: scan for BOTH the raw 32-byte form AND the 64-char
    // hex form. The pairing wire model hex-encodes binary fields (cert
    // payload, ephemeral pubkeys, nonces, ciphertexts), so a leak that
    // serialises master_seed as hex would slip past the raw-window check.
    // We check both lowercase and uppercase to cover any hex casing
    // accidentally introduced by future formatting changes.
    let master_seed_hex_lower = hex::encode(master_seed_bytes);
    let master_seed_hex_upper = hex::encode_upper(master_seed_bytes);
    for msg in inviter_msgs.iter().chain(joiner_msgs.iter()) {
        let mut bytes = Vec::new();
        ciborium::into_writer(msg, &mut bytes).unwrap();
        assert!(
            !bytes.windows(32).any(|w| w == master_seed_bytes),
            "master_seed leaked in raw 32-byte form in {msg:?}"
        );
        assert!(
            !bytes
                .windows(master_seed_hex_lower.len())
                .any(|w| w == master_seed_hex_lower.as_bytes()),
            "master_seed leaked in lowercase hex form in {msg:?}"
        );
        assert!(
            !bytes
                .windows(master_seed_hex_upper.len())
                .any(|w| w == master_seed_hex_upper.as_bytes()),
            "master_seed leaked in uppercase hex form in {msg:?}"
        );
    }
}

async fn drive_to_handshake(inviter_handle: &PairingHandle, joiner_handle: &PairingHandle) {
    // Use `wait_for` rather than `changed-then-check`: a freshly-cloned
    // watch::Receiver hasn't observed the current value, so if the SM has
    // already advanced past the target state before clone+await, the
    // changed-then-check loop blocks for the NEXT transition and the test
    // hangs (or times out). `wait_for` checks the current value first.
    let mut inviter_state = inviter_handle.state_rx.clone();
    let mut joiner_state = joiner_handle.state_rx.clone();

    timeout(Duration::from_secs(2), async {
        inviter_state
            .wait_for(|s| matches!(s, PairingState::Discovered { .. }))
            .await
            .unwrap();
    })
    .await
    .expect("inviter discovers");

    timeout(Duration::from_secs(2), async {
        joiner_state
            .wait_for(|s| matches!(s, PairingState::Discovered { .. }))
            .await
            .unwrap();
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
        inviter_state
            .wait_for(|s| matches!(s, PairingState::Handshaking { .. }))
            .await
            .unwrap();
    })
    .await
    .expect("inviter handshake");
    timeout(Duration::from_secs(2), async {
        joiner_state
            .wait_for(|s| matches!(s, PairingState::Handshaking { .. }))
            .await
            .unwrap();
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
    let _pp_guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "test-pp");
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
    let test_interval = Duration::from_secs(60);
    let mut inviter_handle = spawn_state_machine(inviter_t_arc, now_fn.clone(), test_interval);
    let joiner_handle = spawn_state_machine(joiner_t_arc, now_fn.clone(), test_interval);

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
    let inviter_result = drive_to_complete(&mut inviter_handle, &joiner_handle).await;

    // Drain the joiner result receiver — exactly what the start_node joiner
    // drainer does in production. (The inviter handoff is drained + acked
    // inside drive_to_complete, which returns its result above.)
    let mut joiner_rx = joiner_handle.joiner_result_rx.expect("joiner result rx");
    let joiner_result = timeout(Duration::from_secs(2), joiner_rx.recv())
        .await
        .expect("joiner result arrives")
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

/// Drive both sides from Handshaking to `Complete`, returning the inviter's
/// `InviterEnrollResult`.
///
/// ZEB-491: the inviter no longer reaches `Complete` until its enrollment is
/// durably persisted. `maybe_advance_to_enroll` now emits an
/// `InviterEnrollHandoff` and parks at `Enrolling` awaiting the oneshot
/// `persisted_ack`; the production `start_node` drainer fires that ack after
/// writing `owner_state.cbor`. Here we simulate the drainer — drain the handoff
/// and ack `Ok(())` — so the inviter can advance. The drained `result` is
/// returned so callers that persist it don't re-drain the (now-taken) channel.
async fn drive_to_complete(
    inviter_handle: &mut PairingHandle,
    joiner_handle: &PairingHandle,
) -> InviterEnrollResult {
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

    // wait_for checks the current value first — see drive_to_handshake. The
    // joiner completes on its own (it consumes ENROLL directly, no ack gate).
    let mut joiner_state = joiner_handle.state_rx.clone();
    timeout(Duration::from_secs(3), async {
        joiner_state
            .wait_for(|s| matches!(s, PairingState::Complete { .. }))
            .await
            .unwrap();
    })
    .await
    .expect("joiner completes");

    // Simulate the start_node inviter drainer: drain the handoff and ack the
    // persist so the inviter SM can advance past `Enrolling` to `Complete`.
    let mut inviter_rx = inviter_handle
        .inviter_result_rx
        .take()
        .expect("inviter result rx present");
    let handoff = timeout(Duration::from_secs(2), inviter_rx.recv())
        .await
        .expect("inviter handoff arrives")
        .expect("handoff not None");
    handoff
        .persisted_ack
        .send(Ok(()))
        .expect("inviter SM's detached ack task must still hold the persist-ack receiver");

    let mut inviter_state = inviter_handle.state_rx.clone();
    timeout(Duration::from_secs(3), async {
        inviter_state
            .wait_for(|s| matches!(s, PairingState::Complete { .. }))
            .await
            .unwrap();
    })
    .await
    .expect("inviter completes");

    handoff.result
}

/// Transport wrapper that returns Err on the Nth `Encrypted` publish, to
/// simulate a network drop right at ENROLL time. The Inviter's outgoing
/// publishes during the happy path are: DISCOVER (×N), SELECT, then two
/// `Encrypted` payloads — CONFIRM (#0) and ENROLL (#1). Letting CONFIRM
/// through and failing ENROLL exercises exactly the post-CONFIRM /
/// pre-ENROLL drop the spec calls out.
struct FailNthEncryptedTransport {
    inner: Arc<dyn PairingTransport>,
    encrypted_count: Arc<std::sync::atomic::AtomicUsize>,
    fail_at: usize,
}

#[async_trait::async_trait]
impl PairingTransport for FailNthEncryptedTransport {
    async fn publish(&self, message: PairingWireMessage) -> Result<(), String> {
        if matches!(message, PairingWireMessage::Encrypted { .. }) {
            let n = self
                .encrypted_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n == self.fail_at {
                return Err("simulated network drop at ENROLL".to_string());
            }
        }
        self.inner.publish(message).await
    }
    async fn recv(&self) -> Option<PairingWireMessage> {
        self.inner.recv().await
    }
}

/// ZEB-198: network drop during ENROLL.
///
/// Both sides confirm the SAS; the Inviter's CONFIRM lands successfully so
/// the Joiner's `peer_confirmed` flips and it advances to `Enrolling`. The
/// Inviter then signs and tries to publish ENROLL — but the wire channel
/// is "dropped" at that exact instant (we simulate this with a transport
/// wrapper that returns Err on the second Encrypted publish).
///
/// What we want to pin down:
///
/// 1. **Inviter surfaces a recoverable Failed state** rather than silently
///    half-committing. Pre-PR-#63 the Inviter would have already mutated
///    `ctx.owner_state` (added the new enrollment) before publishing ENROLL,
///    so a failed publish left the Inviter persistently bound to a
///    "phantom device" the Joiner never received the cert for. PR #63's
///    publish-then-commit ordering moved the local mutation strictly AFTER
///    a successful publish; this test pins that down so a future refactor
///    can't regress to the old order.
///
/// 2. **No `InviterEnrollResult` is emitted** when publish fails. The
///    `inviter_result_rx` drainer in `start_node` would otherwise persist
///    the freshly-mutated OwnerState to disk, re-introducing the phantom-
///    device bug at the persistence layer.
///
/// 3. **Joiner remains at Enrolling, not Complete.** Without a built-in
///    Joiner-side timeout (a follow-up — the SM has no wall-clock-based
///    expiry today), the Joiner sits indefinitely until the user manually
///    cancels. Cancel from this state must transition cleanly to Idle —
///    that's what makes "retry" possible.
#[tokio::test]
async fn network_drop_during_enroll() {
    let MintResult {
        state: original_state,
        recovery_artifact,
        ..
    } = mint_owner(1_700_000_000).unwrap();
    let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());
    let joiner_sk = SigningKey::generate(&mut OsRng);

    let (inviter_t, joiner_t) = InMemoryBroker::pair();
    // Wrap the Inviter side so its SECOND Encrypted publish (== ENROLL)
    // returns Err, while CONFIRM (the first Encrypted publish) goes through
    // normally and reaches the Joiner.
    let inviter_t_wrapped: Arc<dyn PairingTransport> = Arc::new(FailNthEncryptedTransport {
        inner: Arc::new(inviter_t),
        encrypted_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        fail_at: 1,
    });
    let joiner_t_arc: Arc<dyn PairingTransport> = Arc::new(joiner_t);
    let now_fn: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| 1_700_000_001);
    let test_interval = Duration::from_secs(60);
    let inviter_handle = spawn_state_machine(inviter_t_wrapped, now_fn.clone(), test_interval);
    let joiner_handle = spawn_state_machine(joiner_t_arc, now_fn.clone(), test_interval);

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

    // Both confirm. CONFIRM publishes succeed; the Inviter's ENROLL publish
    // is the one we drop.
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

    // ── Assertion 1: Inviter surfaces Failed with the right reason ────
    let mut inviter_state = inviter_handle.state_rx.clone();
    timeout(Duration::from_secs(3), async {
        inviter_state
            .wait_for(|s| matches!(s, PairingState::Failed { .. }))
            .await
            .unwrap();
    })
    .await
    .expect("inviter surfaces Failed within 3s of ENROLL drop");
    let inviter_failed_reason = match &*inviter_handle.state_rx.borrow() {
        PairingState::Failed { reason } => reason.clone(),
        other => panic!("expected Failed, got {other:?}"),
    };
    assert!(
        inviter_failed_reason.contains("publish enroll"),
        "Failed reason must call out the ENROLL publish failure so the user \
         can interpret the recovery path; got: {inviter_failed_reason}"
    );

    // ── Assertion 2: No InviterEnrollResult was emitted ───────────────
    // The drainer task in start_node persists this struct to disk; emitting
    // it on a failed publish would re-introduce the phantom-device bug at
    // the persistence layer.
    //
    // The 1500ms window is well above tokio scheduling jitter on slow CI
    // (PR #64 review/Qodo flagged the original 200ms as too aggressive for
    // proving a negative). Anything emitted here within 1.5s would be a
    // genuine post-Failed leak; missing a leak that arrives >1.5s late is
    // possible but exceedingly unlikely given the SM has no async wait
    // between Failed-emit and the would-be inviter_result_tx.send.
    let mut inviter_result_rx = inviter_handle
        .inviter_result_rx
        .expect("inviter_result_rx present");
    match tokio::time::timeout(Duration::from_millis(1500), inviter_result_rx.recv()).await {
        Err(_) => {
            // Timeout — no result emitted. This is the desired behavior.
        }
        Ok(Some(_)) => {
            panic!(
                "InviterEnrollResult was emitted despite ENROLL publish \
                 failing — would persist a phantom-device enrollment to disk"
            );
        }
        Ok(None) => {
            // Channel closed without a value. Also acceptable (no leak).
        }
    }

    // (No "snapshot sanity" assert here — comparing original_state.len() to
    // a value derived from the same snapshot is tautological; PR #64 review
    // (CodeAnt) called this out. The Assertion-2 check above is the real
    // proof that no phantom commit happened.)

    // ── Assertion 3: Joiner settles at Enrolling, not Complete ────────
    // The Joiner has both confirms and emitted Enrolling, then waits for
    // the ENROLL wire message that never arrives. With no built-in
    // timeout (separate follow-up), the Joiner's only recovery path is
    // user Cancel — which we exercise below.
    //
    // A bare `matches!` snapshot at this instant would race: the Joiner
    // may briefly be at WaitingPeerConfirm before the Inviter's CONFIRM
    // (already in flight on the channel) bumps peer_confirmed and lands
    // it at Enrolling. Wait for the settled state instead. Cursor flagged
    // this race on PR #64.
    let mut joiner_state_rx = joiner_handle.state_rx.clone();
    timeout(Duration::from_secs(2), async {
        joiner_state_rx
            .wait_for(|s| matches!(s, PairingState::Enrolling))
            .await
            .unwrap();
    })
    .await
    .expect("Joiner settles at Enrolling within 2s of the Inviter going Failed");
    // Follow-up snapshot: after settling, verify Joiner has NOT advanced
    // past Enrolling (no ENROLL ever arrived; Complete would mean a
    // phantom delivery slipped past the FailNthEncryptedTransport).
    assert!(
        matches!(*joiner_handle.state_rx.borrow(), PairingState::Enrolling),
        "Joiner must remain at Enrolling (Complete would mean ENROLL slipped past the drop wrapper)"
    );

    // ── Recovery: Cancel allows the user to retry ─────────────────────
    joiner_handle
        .cmd_tx
        .send(PairingCommand::Cancel)
        .await
        .unwrap();
    let mut joiner_state_rx = joiner_handle.state_rx.clone();
    timeout(Duration::from_secs(2), async {
        joiner_state_rx
            .wait_for(|s| matches!(s, PairingState::Idle))
            .await
            .unwrap();
    })
    .await
    .expect("joiner Cancel transitions to Idle (retry possible)");
}
