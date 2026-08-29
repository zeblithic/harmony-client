//! ZEB-307 Task 10: two-engine peer-driven DKG convergence via the
//! engine's transport channels.
//!
//! ## Load-bearing claim
//!
//! Earlier tasks proved each engine subsystem in isolation:
//!
//! * Tasks 1-3 — `DfrostLogEngine::start` + `process_inbound`
//!   (decode → sig-verify → dedup → apply → record).
//! * Task 4 — emit Tauri progress events from inbound apply.
//! * Task 5 — `publish_event` (record-before-send + outbound mpsc).
//! * Tasks 6-9 — IPC handlers cross-wire `apply_with_identity` ↔
//!   `publish_event`.
//!
//! What was NOT yet covered: that the engine's publisher mpsc on node A
//! actually delivers (via byte-relay glue) into node B's subscriber mpsc,
//! `process_inbound` re-runs the full verify/dedup/apply chain on the
//! peer side, and both engines converge on the same `joint_verifying_key`.
//!
//! This test wires `alice.publisher_tx → bob.subscriber_rx` and the
//! symmetric reverse via two `tokio::spawn` forwarder tasks (the minimum
//! "transport" needed to exercise the engine boundary). It then drives a
//! full 2-of-2 FROST-Ristretto255 DKG entirely through `publish_event`
//! calls — no direct `apply` on the peer side. With this test green, the
//! Zenoh adapter (next ticket) is reduced to swap-in byte-relay glue.
//!
//! ## What changed vs `community_dfrost_ipc_integration.rs`
//!
//! The IPC integration test cross-applies events via direct
//! `log.apply_with_identity(peer_event, …)` calls; this file replaces
//! every such peer-side direct apply with an `engine.publish_event(…)`
//! plus a bounded wait for the bridge to drain into the peer's inbound
//! task. The local-side apply is retained (matches `dfrost_initiate_dkg`
//! / `dfrost_contribute_dkg_round` IPC sequencing: local apply first,
//! then broadcast).
//!
//! ## Identity binding
//!
//! `verify_signed_committee_event` enforces
//! `identity.address_hash == event.actor.0`. The IPC integration test
//! sidesteps this by never going through the verify path (direct apply
//! only). Here we MUST go through verify, so the test uses
//! `fixture_identity(seed)` — same helper shape as the engine's in-file
//! tests — to obtain Ed25519 + OwnerAddr pairs where the binding holds.
//! The synthetic `[0x01; 16]` / `[0x02; 16]` addresses from the IPC test
//! would fail verify.

use std::collections::BTreeMap;
use std::sync::Arc;

use frost_ristretto255::{keys::KeyPackage, Identifier};
use harmony_app::community_dfrost_crypto::{
    dkg_part1_local, dkg_part2_local, dkg_part3_local, identifier_for_index,
    verifying_key_to_bytes, verifying_share_to_bytes,
};
use harmony_app::community_dfrost_log::{build_signed_dfrost_event, DfrostLog, PendingCeremony};
use harmony_app::community_dfrost_log_engine::{DfrostLogEngine, DfrostLogEngineParams};
use harmony_app::community_dfrost_types::{
    DfrostEventKind, DkgCompletePayload, DkgRoundPayload, MemberVerifyingShare,
};
use harmony_app::community_membership::RecipientCiphertext;
use harmony_app::community_state_sync::IdentityResolver;
use harmony_app::dm_signing;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use std::collections::HashMap;
use tokio::sync::{mpsc, Mutex};

// ─── Identity-resolver helper ──────────────────────────────────────────────

/// Minimal `IdentityResolver` backed by a `HashMap`. Same shape as the
/// `StaticResolver` used by the engine's in-file tests. Each engine
/// MUST have a resolver populated with BOTH members' 64-byte identity
/// composites — verify_signed_committee_event resolves `event.actor` →
/// composite and then checks the address-hash binding.
struct StaticResolver(HashMap<OwnerAddr, [u8; 64]>);

#[async_trait::async_trait]
impl IdentityResolver for StaticResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        self.0.get(addr).copied()
    }
}

// ─── Test identities (matching address_hash binding) ──────────────────────

/// Build `(SigningKey, OwnerAddr, identity_pub_64)` from a seed where
/// `address_hash` binds correctly. Mirrors
/// `community_dfrost_log_engine::tests::fixture_identity`.
fn fixture_identity(seed: u8) -> (ed25519_dalek::SigningKey, OwnerAddr, [u8; 64]) {
    let priv_id = harmony_identity::PrivateIdentity::from_seed(&[seed; 32]);
    let owner = OwnerAddr(priv_id.identity.address_hash);
    let pub_64 = priv_id.identity.to_public_bytes();
    let private_bytes = priv_id.to_private_bytes();
    let mut ed_secret = [0u8; 32];
    ed_secret.copy_from_slice(&private_bytes[32..64]);
    let signing = ed25519_dalek::SigningKey::from_bytes(&ed_secret);
    (signing, owner, pub_64)
}

fn hlc_at(wall_ms: u64, device_id: &str) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: device_id.into(),
    }
}

// ─── Ceremony driver helpers (lifted from IPC integration test) ───────────
//
// These mirror `community_dfrost_ipc_integration.rs::initiate_dkg_local` /
// `contribute_dkg_round{2,3}_local` but with one transformation: each
// helper performs the LOCAL apply (so the originating node sees its own
// event), then returns the signed event for the caller to push via
// `engine.publish_event(…)`. The peer side does NOT call apply directly
// — the transport bridge + engine inbound loop does.

#[allow(clippy::too_many_arguments)]
fn initiate_dkg_local(
    log: &mut DfrostLog,
    self_addr: OwnerAddr,
    self_id: Identifier,
    members: &[OwnerAddr],
    threshold: u16,
    max_signers: u16,
    proposed_epoch: u64,
    ceremony_id: [u8; 32],
    hlc: Hlc,
    signing_key: &ed25519_dalek::SigningKey,
    self_x25519_priv: &[u8; 32],
) -> harmony_app::community_dfrost_types::SignedCommitteeEvent {
    log.committee_state.pending_dkg = Some(PendingCeremony {
        ceremony_id,
        members: members.to_vec(),
        threshold,
        max_signers,
        proposed_epoch,
        ..Default::default()
    });

    let (r1_secret, r1_pkg_bytes) =
        dkg_part1_local(self_id, max_signers, threshold).expect("dkg_part1_local");
    log.local_dkg_secret = Some(r1_secret);

    let payload = DkgRoundPayload {
        ceremony_id,
        round_num: 1,
        round1_package: Some(r1_pkg_bytes),
        recipient_ciphertexts: None,
    };
    let event = build_signed_dfrost_event(
        signing_key,
        self_addr,
        DfrostEventKind::DkgRound,
        &payload,
        hlc,
    )
    .expect("build_signed_dfrost_event rn=1");

    log.apply_with_identity(event.clone(), &self_addr, self_x25519_priv)
        .expect("apply own dr rn=1 locally");
    event
}

/// Build (and locally apply) Bob's rn=1 contribution. Bob's
/// `pending_dkg` is seeded by the inbound apply of Alice's rn=1 (via
/// the engine bridge), not by `initiate_dkg_local`'s seed path.
#[allow(clippy::too_many_arguments)]
fn contribute_rn1_local(
    log: &mut DfrostLog,
    self_addr: OwnerAddr,
    self_id: Identifier,
    ceremony_id: [u8; 32],
    threshold: u16,
    max_signers: u16,
    hlc: Hlc,
    signing_key: &ed25519_dalek::SigningKey,
    self_x25519_priv: &[u8; 32],
) -> harmony_app::community_dfrost_types::SignedCommitteeEvent {
    let (r1_secret, r1_pkg_bytes) =
        dkg_part1_local(self_id, max_signers, threshold).expect("contributor dkg_part1");
    log.local_dkg_secret = Some(r1_secret);
    let payload = DkgRoundPayload {
        ceremony_id,
        round_num: 1,
        round1_package: Some(r1_pkg_bytes),
        recipient_ciphertexts: None,
    };
    let event = build_signed_dfrost_event(
        signing_key,
        self_addr,
        DfrostEventKind::DkgRound,
        &payload,
        hlc,
    )
    .expect("build_signed dr rn=1 (contributor)");
    log.apply_with_identity(event.clone(), &self_addr, self_x25519_priv)
        .expect("contributor applies own dr rn=1 locally");
    event
}

#[allow(clippy::too_many_arguments)]
fn contribute_dkg_round2_local(
    log: &mut DfrostLog,
    self_addr: OwnerAddr,
    ceremony_id: [u8; 32],
    members: &[OwnerAddr],
    recipient_x25519_pubs: &BTreeMap<OwnerAddr, [u8; 32]>,
    hlc: Hlc,
    signing_key: &ed25519_dalek::SigningKey,
    self_x25519_priv: &[u8; 32],
) -> harmony_app::community_dfrost_types::SignedCommitteeEvent {
    let (members_snapshot, r1_received_by_addr, r1_secret) = {
        let pending = log
            .committee_state
            .pending_dkg
            .as_ref()
            .expect("pending_dkg present for rn=2");
        assert_eq!(pending.ceremony_id, ceremony_id);
        let r1: BTreeMap<OwnerAddr, Vec<u8>> = pending
            .round1_packages
            .iter()
            .filter(|(addr, _)| **addr != self_addr)
            .map(|(addr, pkg)| (*addr, pkg.clone()))
            .collect();
        let secret = log
            .local_dkg_secret
            .clone()
            .expect("local_dkg_secret stashed by rn=1");
        (pending.members.clone(), r1, secret)
    };

    let expected_others = members_snapshot.len().saturating_sub(1);
    assert_eq!(
        r1_received_by_addr.len(),
        expected_others,
        "rn=2 needs r1 from every other member (sender: {self_addr:?})"
    );

    let mut r1_by_id: BTreeMap<Identifier, Vec<u8>> = BTreeMap::new();
    for (addr, pkg_bytes) in &r1_received_by_addr {
        let idx = members
            .iter()
            .position(|a| *a == *addr)
            .expect("peer in members");
        r1_by_id.insert(identifier_for_index(idx), pkg_bytes.clone());
    }

    let (r2_secret, r2_packages_by_id) =
        dkg_part2_local(r1_secret, &r1_by_id).expect("dkg_part2_local");

    let mut recipient_ciphertexts: Vec<RecipientCiphertext> =
        Vec::with_capacity(r2_packages_by_id.len());
    for (recipient_id, r2_pkg_bytes) in &r2_packages_by_id {
        let idx = members
            .iter()
            .enumerate()
            .find_map(|(i, _)| {
                if identifier_for_index(i) == *recipient_id {
                    Some(i)
                } else {
                    None
                }
            })
            .expect("identifier→addr lookup");
        let recipient_addr = members[idx];
        let recipient_pub = recipient_x25519_pubs
            .get(&recipient_addr)
            .expect("recipient X25519 pub registered");
        let sealed =
            dm_signing::seal_to_owner(recipient_pub, r2_pkg_bytes).expect("seal_to_owner rn=2");
        recipient_ciphertexts.push(RecipientCiphertext {
            recipient: recipient_addr,
            sealed,
        });
    }

    log.local_dkg_secret2 = Some(r2_secret);

    let payload = DkgRoundPayload {
        ceremony_id,
        round_num: 2,
        round1_package: None,
        recipient_ciphertexts: Some(recipient_ciphertexts),
    };
    let event = build_signed_dfrost_event(
        signing_key,
        self_addr,
        DfrostEventKind::DkgRound,
        &payload,
        hlc,
    )
    .expect("build_signed_dfrost_event rn=2");

    log.apply_with_identity(event.clone(), &self_addr, self_x25519_priv)
        .expect("apply own dr rn=2 locally");
    event
}

fn contribute_dkg_round3_local(
    log: &mut DfrostLog,
    self_addr: OwnerAddr,
    ceremony_id: [u8; 32],
    members: &[OwnerAddr],
    hlc: Hlc,
    signing_key: &ed25519_dalek::SigningKey,
    self_x25519_priv: &[u8; 32],
) -> (
    harmony_app::community_dfrost_types::SignedCommitteeEvent,
    KeyPackage,
) {
    let (threshold, max_signers, proposed_epoch, r1_by_id, r2_by_id, secret2) = {
        let pending = log
            .committee_state
            .pending_dkg
            .as_ref()
            .expect("pending_dkg present for dk");
        assert_eq!(pending.ceremony_id, ceremony_id);

        let mut r1: BTreeMap<Identifier, Vec<u8>> = BTreeMap::new();
        for (addr, pkg) in pending
            .round1_packages
            .iter()
            .filter(|(a, _)| **a != self_addr)
        {
            let idx = members
                .iter()
                .position(|a| *a == *addr)
                .expect("peer in members (r1)");
            r1.insert(identifier_for_index(idx), pkg.clone());
        }

        let mut r2: BTreeMap<Identifier, Vec<u8>> = BTreeMap::new();
        for (addr, pkg) in &pending.round2_packages {
            let idx = members
                .iter()
                .position(|a| *a == *addr)
                .expect("r2 sender in members");
            r2.insert(identifier_for_index(idx), pkg.clone());
        }

        let secret = log
            .local_dkg_secret2
            .clone()
            .expect("local_dkg_secret2 stashed by rn=2");
        (
            pending.threshold,
            pending.max_signers,
            pending.proposed_epoch,
            r1,
            r2,
            secret,
        )
    };

    let (key_package, pub_key_package) =
        dkg_part3_local(&secret2, &r1_by_id, &r2_by_id).expect("dkg_part3_local");

    let joint_vk = verifying_key_to_bytes(pub_key_package.verifying_key());
    let mut verifying_shares: Vec<MemberVerifyingShare> = Vec::with_capacity(members.len());
    for (id, vs) in pub_key_package.verifying_shares() {
        let idx = members
            .iter()
            .enumerate()
            .find_map(|(i, _)| {
                if identifier_for_index(i) == *id {
                    Some(i)
                } else {
                    None
                }
            })
            .expect("VerifyingShare identifier→addr lookup");
        verifying_shares.push(MemberVerifyingShare {
            member: members[idx],
            verifying_share: verifying_share_to_bytes(vs),
        });
    }

    log.local_key_package = Some(key_package.clone());
    log.local_pub_key_package = Some(pub_key_package.clone());

    let payload = DkgCompletePayload {
        ceremony_id,
        joint_verifying_key: joint_vk,
        verifying_shares,
        epoch: proposed_epoch,
        members: members.to_vec(),
        threshold,
        max_signers,
    };
    let event = build_signed_dfrost_event(
        signing_key,
        self_addr,
        DfrostEventKind::DkgComplete,
        &payload,
        hlc,
    )
    .expect("build_signed_dfrost_event dk");

    log.apply_with_identity(event.clone(), &self_addr, self_x25519_priv)
        .expect("apply own dk locally");
    (event, key_package)
}

// ─── Polling helpers ──────────────────────────────────────────────────────

/// Wait until `predicate(log)` returns `true`, polling at 5ms intervals.
/// Returns `Ok(())` on success, `Err(label)` on timeout (5 seconds —
/// orders of magnitude above the actual inbound chain latency, which is
/// dominated by Ed25519 verify ≈ microseconds).
async fn wait_for<F>(
    label: &'static str,
    log: &Arc<Mutex<DfrostLog>>,
    predicate: F,
) -> Result<(), String>
where
    F: Fn(&DfrostLog) -> bool,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        {
            let guard = log.lock().await;
            if predicate(&guard) {
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("wait_for({label}) timed out after 5s"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

// ─── The test ─────────────────────────────────────────────────────────────

/// Two engines, peer-driven 2-of-2 DKG. Asserts both engines converge on
/// identical `committee_state.joint_verifying_key` and both promote to
/// `active`. This is the load-bearing transport demonstration for the
/// ZEB-307 engine: with this green, the Zenoh adapter is byte-relay glue.
#[tokio::test]
async fn dkg_two_engine_peer_driven_via_transport_bridge_converges() {
    // ── Identity setup (binding address_hash → SigningKey) ──────────────
    let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0xA1);
    let (bob_sk, bob_addr, bob_pub64) = fixture_identity(0xB1);
    let alice_x_priv = *dm_signing::ed25519_priv_to_x25519(&alice_sk);
    let bob_x_priv = *dm_signing::ed25519_priv_to_x25519(&bob_sk);
    let alice_x_pub =
        *x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(alice_x_priv)).as_bytes();
    let bob_x_pub =
        *x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(bob_x_priv)).as_bytes();

    // Canonical member ordering follows OwnerAddr's byte-wise sort (same
    // as the IPCs' `member_addrs.sort()`). The smaller-bytes addr gets
    // identifier index 0.
    let mut members = vec![alice_addr, bob_addr];
    members.sort();
    let id_alice_idx = members.iter().position(|a| *a == alice_addr).unwrap();
    let id_bob_idx = members.iter().position(|a| *a == bob_addr).unwrap();
    let id_alice = identifier_for_index(id_alice_idx);
    let id_bob = identifier_for_index(id_bob_idx);

    let recipient_pubs: BTreeMap<OwnerAddr, [u8; 32]> =
        [(alice_addr, alice_x_pub), (bob_addr, bob_x_pub)]
            .into_iter()
            .collect();

    // ── Resolvers: each engine must resolve BOTH peers ──────────────────
    let mut resolver_map = HashMap::new();
    resolver_map.insert(alice_addr, alice_pub64);
    resolver_map.insert(bob_addr, bob_pub64);
    let alice_resolver: Arc<dyn IdentityResolver + Send + Sync> =
        Arc::new(StaticResolver(resolver_map.clone()));
    let bob_resolver: Arc<dyn IdentityResolver + Send + Sync> =
        Arc::new(StaticResolver(resolver_map));

    // ── Bridge wiring ───────────────────────────────────────────────────
    //
    // alice_pub_rx (output of Alice's publisher) → bob_sub_tx (input to
    // Bob's subscriber). Symmetric reverse for Bob → Alice.
    //
    // Buffer 64 matches the engine's in-file tests. The forwarder tasks
    // are bounded by the receiver-drop side: once their `pub_rx` end
    // returns `None` (engine dropped), they exit.
    let (alice_pub_tx, mut alice_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (bob_pub_tx, mut bob_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (alice_sub_tx, alice_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (bob_sub_tx, bob_sub_rx) = mpsc::channel::<Vec<u8>>(64);

    let bob_sub_tx_clone = bob_sub_tx.clone();
    let _alice_to_bob_forwarder = tokio::spawn(async move {
        while let Some(packet) = alice_pub_rx.recv().await {
            if bob_sub_tx_clone.send(packet).await.is_err() {
                // Bob's engine dropped — exit cleanly.
                break;
            }
        }
    });

    let alice_sub_tx_clone = alice_sub_tx.clone();
    let _bob_to_alice_forwarder = tokio::spawn(async move {
        while let Some(packet) = bob_pub_rx.recv().await {
            if alice_sub_tx_clone.send(packet).await.is_err() {
                break;
            }
        }
    });

    // ── Engines ─────────────────────────────────────────────────────────
    let community_id = SpaceId([0xCD; 16]);
    let alice_log = Arc::new(Mutex::new(DfrostLog::new()));
    let bob_log = Arc::new(Mutex::new(DfrostLog::new()));

    let app = tauri::test::mock_app();
    let alice_app_handle = app.handle().clone();
    let bob_app_handle = app.handle().clone();

    let alice_engine = DfrostLogEngine::start(DfrostLogEngineParams {
        community_id,
        dfrost_log: alice_log.clone(),
        publisher_tx: alice_pub_tx,
        subscriber_rx: alice_sub_rx,
        app_handle: Some(alice_app_handle),
        self_addr: alice_addr,
        self_x25519_priv: alice_x_priv,
        identity_resolver: alice_resolver,
        registry_weak: None,
        driver: None,
        membership_resolver: None,
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    let bob_engine = DfrostLogEngine::start(DfrostLogEngineParams {
        community_id,
        dfrost_log: bob_log.clone(),
        publisher_tx: bob_pub_tx,
        subscriber_rx: bob_sub_rx,
        app_handle: Some(bob_app_handle),
        self_addr: bob_addr,
        self_x25519_priv: bob_x_priv,
        identity_resolver: bob_resolver,
        registry_weak: None,
        driver: None,
        membership_resolver: None,
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    // ── DKG parameters ─────────────────────────────────────────────────
    let threshold: u16 = 2;
    let max_signers: u16 = 2;
    let proposed_epoch: u64 = 1;

    // Ceremony id derived deterministically from sorted members + hlc.
    // Caller-driven (in production the IPC derives this from the
    // initiator's `hlc_tracker.reserve_for_apply()`). The actual bytes
    // don't matter as long as BOTH engines hash the SAME inputs to land
    // on the same ceremony_id; both sides receive this via the dr rn=1
    // ceremony-bootstrap step (in the engine path, Bob's pending_dkg is
    // seeded by the inbound apply of Alice's dr rn=1).
    let alice_init_hlc = hlc_at(1_000, "alice-dev");
    let ceremony_id = {
        let mut hasher_input: Vec<u8> = Vec::with_capacity(members.len() * 16 + 2 + 8 + 4 + 16);
        for a in &members {
            hasher_input.extend_from_slice(&a.0);
        }
        hasher_input.extend_from_slice(&threshold.to_le_bytes());
        hasher_input.extend_from_slice(&alice_init_hlc.wall_ms.to_le_bytes());
        hasher_input.extend_from_slice(&alice_init_hlc.logical.to_le_bytes());
        hasher_input.extend_from_slice(&community_id.0);
        let bytes: [u8; 32] = blake3::hash(&hasher_input).into();
        bytes
    };

    // ── Round 1: Alice initiates ───────────────────────────────────────
    let dr1_alice = {
        let mut log = alice_log.lock().await;
        initiate_dkg_local(
            &mut log,
            alice_addr,
            id_alice,
            &members,
            threshold,
            max_signers,
            proposed_epoch,
            ceremony_id,
            alice_init_hlc.clone(),
            &alice_sk,
            &alice_x_priv,
        )
    };

    // Seed Bob's pending_dkg from the cross-applied dr rn=1. In Phase 4a
    // the inbound apply path will seed this from the broadcast event;
    // for now we seed it explicitly so Bob's apply_with_identity has a
    // ceremony to attach the rn=1 contribution to. The local-apply path
    // on Bob still runs through the engine inbound chain (sig verify +
    // dedup + apply) once we publish Alice's event.
    {
        let mut log = bob_log.lock().await;
        log.committee_state.pending_dkg = Some(PendingCeremony {
            ceremony_id,
            members: members.clone(),
            threshold,
            max_signers,
            proposed_epoch,
            ..Default::default()
        });
    }

    // Broadcast Alice's rn=1 to Bob via the transport bridge.
    alice_engine
        .publish_event(dr1_alice)
        .await
        .expect("alice publishes dr rn=1");
    wait_for("bob applies alice's dr rn=1", &bob_log, |log| {
        log.committee_state
            .pending_dkg
            .as_ref()
            .map(|p| p.round1_packages.contains_key(&alice_addr))
            .unwrap_or(false)
    })
    .await
    .expect("bob's inbound applies alice's dr rn=1");

    // Bob now contributes his own dr rn=1 (local apply + broadcast).
    let bob_init_hlc = hlc_at(1_100, "bob-dev");
    let dr1_bob = {
        let mut log = bob_log.lock().await;
        contribute_rn1_local(
            &mut log,
            bob_addr,
            id_bob,
            ceremony_id,
            threshold,
            max_signers,
            bob_init_hlc.clone(),
            &bob_sk,
            &bob_x_priv,
        )
    };
    bob_engine
        .publish_event(dr1_bob)
        .await
        .expect("bob publishes dr rn=1");
    wait_for("alice applies bob's dr rn=1", &alice_log, |log| {
        log.committee_state
            .pending_dkg
            .as_ref()
            .map(|p| p.round1_packages.contains_key(&bob_addr))
            .unwrap_or(false)
    })
    .await
    .expect("alice's inbound applies bob's dr rn=1");

    // Post-rn=1 invariant: both engines hold both rn=1 packages.
    {
        let la = alice_log.lock().await;
        let lb = bob_log.lock().await;
        assert_eq!(
            la.committee_state
                .pending_dkg
                .as_ref()
                .unwrap()
                .round1_packages
                .len(),
            2,
            "alice sees both rn=1 pkgs after bridge delivery"
        );
        assert_eq!(
            lb.committee_state
                .pending_dkg
                .as_ref()
                .unwrap()
                .round1_packages
                .len(),
            2,
            "bob sees both rn=1 pkgs after bridge delivery"
        );
    }

    // ── Round 2: each engine contributes rn=2 + broadcasts ─────────────
    let dr2_alice = {
        let mut log = alice_log.lock().await;
        contribute_dkg_round2_local(
            &mut log,
            alice_addr,
            ceremony_id,
            &members,
            &recipient_pubs,
            hlc_at(2_000, "alice-dev"),
            &alice_sk,
            &alice_x_priv,
        )
    };
    alice_engine
        .publish_event(dr2_alice)
        .await
        .expect("alice publishes dr rn=2");
    wait_for("bob decrypts alice's dr rn=2", &bob_log, |log| {
        log.committee_state
            .pending_dkg
            .as_ref()
            .map(|p| p.round2_packages.contains_key(&alice_addr))
            .unwrap_or(false)
    })
    .await
    .expect("bob's inbound applies alice's dr rn=2");

    let dr2_bob = {
        let mut log = bob_log.lock().await;
        contribute_dkg_round2_local(
            &mut log,
            bob_addr,
            ceremony_id,
            &members,
            &recipient_pubs,
            hlc_at(2_100, "bob-dev"),
            &bob_sk,
            &bob_x_priv,
        )
    };
    bob_engine
        .publish_event(dr2_bob)
        .await
        .expect("bob publishes dr rn=2");
    wait_for("alice decrypts bob's dr rn=2", &alice_log, |log| {
        log.committee_state
            .pending_dkg
            .as_ref()
            .map(|p| p.round2_packages.contains_key(&bob_addr))
            .unwrap_or(false)
    })
    .await
    .expect("alice's inbound applies bob's dr rn=2");

    // ── Round 3 / dk: each engine finalizes + broadcasts dk ────────────
    let (dk_alice, _alice_kp) = {
        let mut log = alice_log.lock().await;
        contribute_dkg_round3_local(
            &mut log,
            alice_addr,
            ceremony_id,
            &members,
            hlc_at(3_000, "alice-dev"),
            &alice_sk,
            &alice_x_priv,
        )
    };
    alice_engine
        .publish_event(dk_alice)
        .await
        .expect("alice publishes dk");

    let (dk_bob, _bob_kp) = {
        let mut log = bob_log.lock().await;
        contribute_dkg_round3_local(
            &mut log,
            bob_addr,
            ceremony_id,
            &members,
            hlc_at(3_100, "bob-dev"),
            &bob_sk,
            &bob_x_priv,
        )
    };
    bob_engine
        .publish_event(dk_bob)
        .await
        .expect("bob publishes dk");

    // Wait for both engines to converge to active + joint_vk populated.
    // `apply_dkg_complete` requires `threshold` confirmations before
    // promoting the committee, so we wait on `active` being true.
    wait_for(
        "alice committee active after dk quorum",
        &alice_log,
        |log| log.committee_state.active,
    )
    .await
    .expect("alice promotes to active");
    wait_for("bob committee active after dk quorum", &bob_log, |log| {
        log.committee_state.active
    })
    .await
    .expect("bob promotes to active");

    // ── Convergence assertions ─────────────────────────────────────────
    let la = alice_log.lock().await;
    let lb = bob_log.lock().await;

    let vk_a = la
        .committee_state
        .joint_verifying_key
        .expect("alice joint_vk present");
    let vk_b = lb
        .committee_state
        .joint_verifying_key
        .expect("bob joint_vk present");
    assert_eq!(
        vk_a, vk_b,
        "engines must converge on identical joint VK through the engine transport path"
    );

    assert!(la.committee_state.active, "alice committee active");
    assert!(lb.committee_state.active, "bob committee active");
    assert_eq!(la.committee_state.current_epoch, 1);
    assert_eq!(lb.committee_state.current_epoch, 1);
    assert_eq!(la.committee_state.threshold, threshold);
    assert_eq!(la.committee_state.max_signers, max_signers);
    assert_eq!(
        la.committee_state.verifying_shares, lb.committee_state.verifying_shares,
        "per-member verifying shares converge"
    );
    assert_eq!(
        la.committee_state.members, lb.committee_state.members,
        "members converge"
    );
    assert!(
        la.committee_state.pending_dkg.is_none(),
        "alice pending_dkg cleared post-promotion"
    );
    assert!(
        lb.committee_state.pending_dkg.is_none(),
        "bob pending_dkg cleared post-promotion"
    );

    // Drop the engine Arcs explicitly so the forwarders' downstream senders
    // close + the bridge tasks exit cleanly before the test returns.
    drop(la);
    drop(lb);
    drop(alice_engine);
    drop(bob_engine);
}

// ─── ZEB-1022: auto-orchestrated ceremonies (di bootstrap + DkgDriver) ─────
//
// The test above drives every protocol step by hand and hand-seeds Bob's
// `pending_dkg` — exactly the gap ZEB-1022 closes. The tests below wire
// the PRODUCTION `DkgDriver` (via `production_dkg_driver` over
// `DfrostCoreHandles`) into both engines and assert that a single
// `dfrost_initiate_dkg_core` call on Alice converges BOTH engines to an
// active committee with zero manual seeding and zero manual peer
// contributions: Bob seeds from the broadcast `di`, and every remaining
// round fires from the engines' orchestration layer through the same
// core code the IPCs run.

use harmony_app::community_dfrost_log_engine::{DfrostLogRegistry, DfrostOrchestratorConfig};
use harmony_app::{
    dfrost_initiate_dkg_core, dfrost_propose_refresh_core, production_dkg_driver,
    DfrostCoreHandles, DfrostLogsMap,
};

type MockRt = tauri::test::MockRuntime;

/// Per-node harness for the orchestrated tests: shared log + logs-map +
/// registry + the production driver's handle bundle.
struct OrchestratedNode {
    log: Arc<Mutex<DfrostLog>>,
    registry: Arc<DfrostLogRegistry<MockRt>>,
    handles: DfrostCoreHandles<MockRt>,
}

fn orchestrated_node(
    community_id: SpaceId,
    device_id: &str,
    self_addr: OwnerAddr,
    signing_key: &ed25519_dalek::SigningKey,
    resolver_map: &HashMap<OwnerAddr, [u8; 64]>,
) -> OrchestratedNode {
    let log = Arc::new(Mutex::new(DfrostLog::new()));
    let dfrost_logs: DfrostLogsMap = Arc::new(Mutex::new(HashMap::new()));
    // The map MUST hold the same Arc the engine applies into — the
    // cores get-or-insert from this map.
    {
        let log = log.clone();
        let map = dfrost_logs.clone();
        // No await context here; try_lock on a fresh mutex cannot fail.
        map.try_lock().expect("fresh map").insert(community_id, log);
    }
    let registry: Arc<DfrostLogRegistry<MockRt>> = Arc::new(DfrostLogRegistry::new());
    let identity_resolver: Arc<dyn IdentityResolver + Send + Sync> =
        Arc::new(StaticResolver(resolver_map.clone()));
    let handles = DfrostCoreHandles::<MockRt>::for_tests(
        Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            device_id.to_string(),
        ))),
        harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        device_id.to_string(),
        self_addr,
        Arc::new(signing_key.clone()),
        dfrost_logs,
        Some(identity_resolver),
        Some(registry.clone()),
    );
    OrchestratedNode {
        log,
        registry,
        handles,
    }
}

/// Fast orchestration timers for tests: 25ms ticks, 150ms re-broadcast,
/// deadlines far out of frame so no abort fires mid-test.
fn test_orchestrator_config() -> DfrostOrchestratorConfig {
    DfrostOrchestratorConfig {
        tick_interval: std::time::Duration::from_millis(25),
        rebroadcast_interval: std::time::Duration::from_millis(150),
        initiator_quiet_deadline: std::time::Duration::from_secs(60),
        stale_replace_threshold: std::time::Duration::from_secs(60),
        max_restart_attempts: 3,
        recovery_quiet_deadline: std::time::Duration::from_secs(60),
    }
}

async fn start_orchestrated(
    node: &OrchestratedNode,
    community_id: SpaceId,
    self_addr: OwnerAddr,
    self_x_priv: [u8; 32],
    resolver_map: &HashMap<OwnerAddr, [u8; 64]>,
    publisher_tx: mpsc::Sender<Vec<u8>>,
    subscriber_rx: mpsc::Receiver<Vec<u8>>,
) -> Arc<DfrostLogEngine<MockRt>> {
    let resolver: Arc<dyn IdentityResolver + Send + Sync> =
        Arc::new(StaticResolver(resolver_map.clone()));
    let driver = production_dkg_driver::<MockRt>(node.handles.clone(), None);
    DfrostLogRegistry::register(
        &node.registry,
        DfrostLogEngineParams {
            community_id,
            dfrost_log: node.log.clone(),
            publisher_tx,
            subscriber_rx,
            app_handle: None,
            self_addr,
            self_x25519_priv: self_x_priv,
            identity_resolver: resolver,
            registry_weak: None,
            driver: Some(driver),
            membership_resolver: None,
            orchestrator_config: test_orchestrator_config(),
            persist: None,
        },
    )
    .await
}

/// Bounded convergence wait (30s — the orchestrated flow includes tick
/// cadences, not just inbound latency, and the 4-vCPU CI runners run
/// many test processes concurrently; locally the full ceremony
/// converges in ~25ms, so 30s is a >1000x margin, well under the
/// suite's slowest tests).
async fn wait_converged<F>(
    label: &'static str,
    log: &Arc<Mutex<DfrostLog>>,
    predicate: F,
) -> Result<(), String>
where
    F: Fn(&DfrostLog) -> bool,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        {
            let guard = log.lock().await;
            if predicate(&guard) {
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("wait_converged({label}) timed out after 30s"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// One-line orchestration-state snapshot for timeout diagnostics —
/// CI-only failures need the stall point in the panic message.
async fn dump_log_state(label: &'static str, log: &Arc<Mutex<DfrostLog>>) -> String {
    let l = log.lock().await;
    let pending = match l.committee_state.pending_dkg.as_ref() {
        None => "none".to_string(),
        Some(p) => format!(
            "id={} r1={} r2recv={} dk={} init={:?}",
            hex::encode(&p.ceremony_id[..4]),
            p.round1_packages.len(),
            p.round2_packages.len(),
            p.dk_confirmations.len(),
            p.initiator.map(|a| hex::encode(&a.0[..4])),
        ),
    };
    format!(
        "{label}: active={} epoch={} events={} s1={} s2={} kp={} pending=[{}]",
        l.committee_state.active,
        l.committee_state.current_epoch,
        l.event_count(),
        l.local_dkg_secret.is_some(),
        l.local_dkg_secret2.is_some(),
        l.local_key_package.is_some(),
        pending,
    )
}

/// Unwrap a convergence wait, panicking with BOTH nodes' state dumps on
/// timeout so a CI-only stall is diagnosable from the failure alone.
async fn expect_converged(
    result: Result<(), String>,
    alice_log: &Arc<Mutex<DfrostLog>>,
    bob_log: &Arc<Mutex<DfrostLog>>,
) {
    if let Err(e) = result {
        let a = dump_log_state("alice", alice_log).await;
        let b = dump_log_state("bob", bob_log).await;
        panic!("{e}\n{a}\n{b}");
    }
}

fn assert_converged_active(la: &DfrostLog, lb: &DfrostLog) {
    let vk_a = la
        .committee_state
        .joint_verifying_key
        .expect("alice joint_vk present");
    let vk_b = lb
        .committee_state
        .joint_verifying_key
        .expect("bob joint_vk present");
    assert_eq!(vk_a, vk_b, "joint VK converges");
    assert!(la.committee_state.active && lb.committee_state.active);
    assert_eq!(la.committee_state.current_epoch, 1);
    assert_eq!(lb.committee_state.current_epoch, 1);
    assert_eq!(
        la.committee_state.verifying_shares,
        lb.committee_state.verifying_shares
    );
    assert!(la.committee_state.pending_dkg.is_none());
    assert!(lb.committee_state.pending_dkg.is_none());
    assert!(
        la.local_key_package.is_some(),
        "alice materialised her signing share"
    );
    assert!(
        lb.local_key_package.is_some(),
        "bob materialised his signing share"
    );
}

/// ZEB-1022 headline: one `dfrost_initiate_dkg_core` call on Alice →
/// both engines converge to an active 2-of-2 committee. NO manual
/// pending_dkg seeding anywhere, NO manual peer contributions — Bob
/// bootstraps from the broadcast `di` and the engines' orchestration
/// auto-drives rounds 1-3 through the production cores.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dkg_two_engine_auto_orchestrated_from_initiate_no_manual_seeding() {
    let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0xC5);
    let (bob_sk, bob_addr, bob_pub64) = fixture_identity(0xC6);
    let alice_x_priv = *dm_signing::ed25519_priv_to_x25519(&alice_sk);
    let bob_x_priv = *dm_signing::ed25519_priv_to_x25519(&bob_sk);

    let mut resolver_map = HashMap::new();
    resolver_map.insert(alice_addr, alice_pub64);
    resolver_map.insert(bob_addr, bob_pub64);

    let community_id = SpaceId([0xCE; 16]);
    let alice = orchestrated_node(
        community_id,
        "alice-dev",
        alice_addr,
        &alice_sk,
        &resolver_map,
    );
    let bob = orchestrated_node(community_id, "bob-dev", bob_addr, &bob_sk, &resolver_map);

    // Bridge: alice.pub → bob.sub, bob.pub → alice.sub.
    let (alice_pub_tx, mut alice_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (bob_pub_tx, mut bob_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (alice_sub_tx, alice_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (bob_sub_tx, bob_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    let _a2b = tokio::spawn(async move {
        while let Some(p) = alice_pub_rx.recv().await {
            if bob_sub_tx.send(p).await.is_err() {
                break;
            }
        }
    });
    let _b2a = tokio::spawn(async move {
        while let Some(p) = bob_pub_rx.recv().await {
            if alice_sub_tx.send(p).await.is_err() {
                break;
            }
        }
    });

    let alice_engine = start_orchestrated(
        &alice,
        community_id,
        alice_addr,
        alice_x_priv,
        &resolver_map,
        alice_pub_tx,
        alice_sub_rx,
    )
    .await;
    let bob_engine = start_orchestrated(
        &bob,
        community_id,
        bob_addr,
        bob_x_priv,
        &resolver_map,
        bob_pub_tx,
        bob_sub_rx,
    )
    .await;

    // THE single manual step: Alice initiates.
    let ceremony_hex = dfrost_initiate_dkg_core::<MockRt, MockRt>(
        &alice.handles,
        None,
        community_id,
        vec![alice_addr, bob_addr],
        2,
    )
    .await
    .expect("alice initiates");
    assert_eq!(ceremony_hex.len(), 64);

    expect_converged(
        wait_converged("alice converges to active", &alice.log, |l| {
            l.committee_state.active
        })
        .await,
        &alice.log,
        &bob.log,
    )
    .await;
    expect_converged(
        wait_converged("bob converges to active", &bob.log, |l| {
            l.committee_state.active
        })
        .await,
        &alice.log,
        &bob.log,
    )
    .await;

    let la = alice.log.lock().await;
    let lb = bob.log.lock().await;
    assert_converged_active(&la, &lb);
    assert!(
        lb.events().any(|e| e.kind == DfrostEventKind::CeremonyInit),
        "bob's log must contain the (re-minted) di that seeded him"
    );
    drop(la);
    drop(lb);
    drop(alice_engine);
    drop(bob_engine);
}

/// ZEB-1022 facet B: the initiator's first broadcasts (`di` + `dr` rn=1)
/// are LOST on the wire (boot-window loss / late subscriber). The
/// orchestrator's periodic re-broadcast re-mints them with fresh HLCs
/// (original bytes would be dedup-dropped once Bob has seen any newer
/// alice event) and the ceremony still converges end-to-end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dkg_recovers_when_initial_di_and_round1_are_lost() {
    let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0xC7);
    let (bob_sk, bob_addr, bob_pub64) = fixture_identity(0xC8);
    let alice_x_priv = *dm_signing::ed25519_priv_to_x25519(&alice_sk);
    let bob_x_priv = *dm_signing::ed25519_priv_to_x25519(&bob_sk);

    let mut resolver_map = HashMap::new();
    resolver_map.insert(alice_addr, alice_pub64);
    resolver_map.insert(bob_addr, bob_pub64);

    let community_id = SpaceId([0xCF; 16]);
    let alice = orchestrated_node(
        community_id,
        "alice-dev",
        alice_addr,
        &alice_sk,
        &resolver_map,
    );
    let bob = orchestrated_node(community_id, "bob-dev", bob_addr, &bob_sk, &resolver_map);

    let (alice_pub_tx, mut alice_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (bob_pub_tx, mut bob_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (alice_sub_tx, alice_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (bob_sub_tx, bob_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    // Lossy alice→bob leg: the FIRST `di` and the FIRST `dr` rn=1 seen
    // on the wire vanish — dropped by CONTENT, not arrival count
    // (CodeRabbit on #771: the orchestrator's tick re-broadcast can
    // race the initiate call's own publishes, so position on the wire
    // is not a reliable identity for "the originals"). Everything
    // else — including the re-mints of those same events — is
    // delivered.
    let _a2b = tokio::spawn(async move {
        let (mut di_dropped, mut dr1_dropped) = (false, false);
        while let Some(p) = alice_pub_rx.recv().await {
            if let Ok(ev) = ciborium::de::from_reader::<
                harmony_app::community_dfrost_types::SignedCommitteeEvent,
                _,
            >(&p[..])
            {
                match ev.kind {
                    DfrostEventKind::CeremonyInit if !di_dropped => {
                        di_dropped = true;
                        continue;
                    }
                    DfrostEventKind::DkgRound if !dr1_dropped => {
                        let is_rn1 =
                            ciborium::de::from_reader::<DkgRoundPayload, _>(&ev.payload[..])
                                .map(|pl| pl.round_num == 1)
                                .unwrap_or(false);
                        if is_rn1 {
                            dr1_dropped = true;
                            continue;
                        }
                    }
                    _ => {}
                }
            }
            if bob_sub_tx.send(p).await.is_err() {
                break;
            }
        }
    });
    let _b2a = tokio::spawn(async move {
        while let Some(p) = bob_pub_rx.recv().await {
            if alice_sub_tx.send(p).await.is_err() {
                break;
            }
        }
    });

    let alice_engine = start_orchestrated(
        &alice,
        community_id,
        alice_addr,
        alice_x_priv,
        &resolver_map,
        alice_pub_tx,
        alice_sub_rx,
    )
    .await;
    let bob_engine = start_orchestrated(
        &bob,
        community_id,
        bob_addr,
        bob_x_priv,
        &resolver_map,
        bob_pub_tx,
        bob_sub_rx,
    )
    .await;

    dfrost_initiate_dkg_core::<MockRt, MockRt>(
        &alice.handles,
        None,
        community_id,
        vec![alice_addr, bob_addr],
        2,
    )
    .await
    .expect("alice initiates");

    // Bob missed the originals; only the re-mints can seed him. The
    // pending slot may already have been promoted away by the time we
    // poll (a 2-of-2 ceremony completes in single-digit milliseconds
    // once seeded), so accept either state — the di-in-log assertion
    // below pins that the seed really came through the wire.
    expect_converged(
        wait_converged("bob seeds from re-minted di", &bob.log, |l| {
            l.committee_state.pending_dkg.is_some() || l.committee_state.active
        })
        .await,
        &alice.log,
        &bob.log,
    )
    .await;

    expect_converged(
        wait_converged("alice converges to active (lossy start)", &alice.log, |l| {
            l.committee_state.active
        })
        .await,
        &alice.log,
        &bob.log,
    )
    .await;
    expect_converged(
        wait_converged("bob converges to active (lossy start)", &bob.log, |l| {
            l.committee_state.active
        })
        .await,
        &alice.log,
        &bob.log,
    )
    .await;

    let la = alice.log.lock().await;
    let lb = bob.log.lock().await;
    assert_converged_active(&la, &lb);
    assert!(
        lb.events().any(|e| e.kind == DfrostEventKind::CeremonyInit),
        "bob's log must contain the re-minted di that seeded him"
    );
    drop(la);
    drop(lb);
    drop(alice_engine);
    drop(bob_engine);
}

/// ZEB-1022 straggler heal (the exact CI stall on #771): Alice's
/// ORIGINAL `dk` is lost on the wire. Bob wedges one confirmation short
/// of quorum while Alice promotes — and a promoted node's pending slot
/// is gone, so ordinary re-broadcast no longer covers the ceremony.
/// Bob's own re-mints then hit Alice's engine as apply failures
/// (`UnknownCeremony` — she has no pending), which triggers the
/// active-committee heal: Alice re-mints her `dk` (fresh HLC) and Bob
/// reaches quorum.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dkg_straggler_heals_after_peer_promotion_when_dk_is_lost() {
    let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0xCC);
    let (bob_sk, bob_addr, bob_pub64) = fixture_identity(0xCE);
    let alice_x_priv = *dm_signing::ed25519_priv_to_x25519(&alice_sk);
    let bob_x_priv = *dm_signing::ed25519_priv_to_x25519(&bob_sk);

    let mut resolver_map = HashMap::new();
    resolver_map.insert(alice_addr, alice_pub64);
    resolver_map.insert(bob_addr, bob_pub64);

    let community_id = SpaceId([0xD0; 16]);
    let alice = orchestrated_node(
        community_id,
        "alice-dev",
        alice_addr,
        &alice_sk,
        &resolver_map,
    );
    let bob = orchestrated_node(community_id, "bob-dev", bob_addr, &bob_sk, &resolver_map);

    let (alice_pub_tx, mut alice_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (bob_pub_tx, mut bob_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (alice_sub_tx, alice_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (bob_sub_tx, bob_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    // Lossy alice→bob leg: the FIRST DkgComplete from Alice vanishes —
    // Bob is left one dk short of quorum exactly as the CI stall showed.
    let _a2b = tokio::spawn(async move {
        let mut dk_dropped = false;
        while let Some(p) = alice_pub_rx.recv().await {
            if !dk_dropped {
                if let Ok(ev) = ciborium::de::from_reader::<
                    harmony_app::community_dfrost_types::SignedCommitteeEvent,
                    _,
                >(&p[..])
                {
                    if ev.kind == DfrostEventKind::DkgComplete {
                        dk_dropped = true;
                        continue;
                    }
                }
            }
            if bob_sub_tx.send(p).await.is_err() {
                break;
            }
        }
    });
    let _b2a = tokio::spawn(async move {
        while let Some(p) = bob_pub_rx.recv().await {
            if alice_sub_tx.send(p).await.is_err() {
                break;
            }
        }
    });

    let alice_engine = start_orchestrated(
        &alice,
        community_id,
        alice_addr,
        alice_x_priv,
        &resolver_map,
        alice_pub_tx,
        alice_sub_rx,
    )
    .await;
    let bob_engine = start_orchestrated(
        &bob,
        community_id,
        bob_addr,
        bob_x_priv,
        &resolver_map,
        bob_pub_tx,
        bob_sub_rx,
    )
    .await;

    dfrost_initiate_dkg_core::<MockRt, MockRt>(
        &alice.handles,
        None,
        community_id,
        vec![alice_addr, bob_addr],
        2,
    )
    .await
    .expect("alice initiates");

    // Both must converge even though Alice's original dk never arrived —
    // her heal path re-mints it once Bob's re-broadcasts reach her
    // post-promotion engine.
    expect_converged(
        wait_converged("alice converges to active (dk lost)", &alice.log, |l| {
            l.committee_state.active
        })
        .await,
        &alice.log,
        &bob.log,
    )
    .await;
    expect_converged(
        wait_converged("bob converges to active (dk lost)", &bob.log, |l| {
            l.committee_state.active
        })
        .await,
        &alice.log,
        &bob.log,
    )
    .await;

    let la = alice.log.lock().await;
    let lb = bob.log.lock().await;
    assert_converged_active(&la, &lb);
    drop(la);
    drop(lb);
    drop(alice_engine);
    drop(bob_engine);
}

/// ZEB-1028 headline: a refresh round LOST on the wire no longer stalls
/// the ceremony until restart. Alice's FIRST rf rn=2 (her sealed share
/// distribution) vanishes on the alice→bob leg; the orchestrator's
/// recovery re-broadcast cadence re-mints it (fresh HLC, identical
/// payload) and the refresh still completes end-to-end on both engines
/// — epoch advanced, joint verifying key preserved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_heals_lost_round_via_rebroadcast_zeb1028() {
    let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0xD6);
    let (bob_sk, bob_addr, bob_pub64) = fixture_identity(0xD7);
    let alice_x_priv = *dm_signing::ed25519_priv_to_x25519(&alice_sk);
    let bob_x_priv = *dm_signing::ed25519_priv_to_x25519(&bob_sk);

    let mut resolver_map = HashMap::new();
    resolver_map.insert(alice_addr, alice_pub64);
    resolver_map.insert(bob_addr, bob_pub64);

    let community_id = SpaceId([0xD8; 16]);
    let alice = orchestrated_node(
        community_id,
        "alice-dev",
        alice_addr,
        &alice_sk,
        &resolver_map,
    );
    let bob = orchestrated_node(community_id, "bob-dev", bob_addr, &bob_sk, &resolver_map);

    let (alice_pub_tx, mut alice_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (bob_pub_tx, mut bob_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (alice_sub_tx, alice_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (bob_sub_tx, bob_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    // Lossy alice→bob leg: the FIRST ProactiveRefresh rn=2 from Alice
    // vanishes — without re-broadcast healing, Bob can never finalize
    // (he is missing Alice's sealed package) and the refresh wedges.
    let _a2b = tokio::spawn(async move {
        let mut rf2_dropped = false;
        while let Some(p) = alice_pub_rx.recv().await {
            if !rf2_dropped {
                if let Ok(ev) = ciborium::de::from_reader::<
                    harmony_app::community_dfrost_types::SignedCommitteeEvent,
                    _,
                >(&p[..])
                {
                    if ev.kind == DfrostEventKind::ProactiveRefresh {
                        if let Ok(pl) = ciborium::de::from_reader::<
                            harmony_app::community_dfrost_types::RefreshRoundPayload,
                            _,
                        >(&ev.payload[..])
                        {
                            if pl.round_num == 2 {
                                rf2_dropped = true;
                                continue;
                            }
                        }
                    }
                }
            }
            if bob_sub_tx.send(p).await.is_err() {
                break;
            }
        }
    });
    let _b2a = tokio::spawn(async move {
        while let Some(p) = bob_pub_rx.recv().await {
            if alice_sub_tx.send(p).await.is_err() {
                break;
            }
        }
    });

    let alice_engine = start_orchestrated(
        &alice,
        community_id,
        alice_addr,
        alice_x_priv,
        &resolver_map,
        alice_pub_tx,
        alice_sub_rx,
    )
    .await;
    let bob_engine = start_orchestrated(
        &bob,
        community_id,
        bob_addr,
        bob_x_priv,
        &resolver_map,
        bob_pub_tx,
        bob_sub_rx,
    )
    .await;

    // Phase 1: a clean DKG activates the committee (nothing dropped —
    // the filter only ever eats an rf rn=2).
    dfrost_initiate_dkg_core::<MockRt, MockRt>(
        &alice.handles,
        None,
        community_id,
        vec![alice_addr, bob_addr],
        2,
    )
    .await
    .expect("alice initiates DKG");
    // Wait on EACH log before the combined assert (CI shard flush on
    // #776: bob reaches dk quorum first — he holds alice's dk plus his
    // own — so waiting on bob alone races alice's promotion).
    expect_converged(
        wait_converged("alice active after DKG", &alice.log, |l| {
            l.committee_state.active
        })
        .await,
        &alice.log,
        &bob.log,
    )
    .await;
    expect_converged(
        wait_converged("bob active after DKG", &bob.log, |l| {
            l.committee_state.active
        })
        .await,
        &alice.log,
        &bob.log,
    )
    .await;
    let (vk_before, epoch_before) = {
        let la = alice.log.lock().await;
        let lb = bob.log.lock().await;
        assert_converged_active(&la, &lb);
        (
            la.committee_state.joint_verifying_key,
            la.committee_state.current_epoch,
        )
    };

    // Phase 2: Alice proposes the refresh; auto-drive carries both
    // members through rounds 1–3 + dk — surviving the lost rn=2.
    let mut members = vec![alice_addr, bob_addr];
    members.sort();
    dfrost_propose_refresh_core::<MockRt, MockRt>(
        &alice.handles,
        None,
        community_id,
        members,
        2,
        None,
    )
    .await
    .expect("alice proposes refresh");

    expect_converged(
        wait_converged("alice promotes the refresh epoch", &alice.log, move |l| {
            l.committee_state.current_epoch == epoch_before + 1
                && l.committee_state.pending_refresh.is_none()
        })
        .await,
        &alice.log,
        &bob.log,
    )
    .await;
    expect_converged(
        wait_converged("bob promotes the refresh epoch", &bob.log, move |l| {
            l.committee_state.current_epoch == epoch_before + 1
                && l.committee_state.pending_refresh.is_none()
        })
        .await,
        &alice.log,
        &bob.log,
    )
    .await;

    let la = alice.log.lock().await;
    let lb = bob.log.lock().await;
    assert_eq!(
        la.committee_state.joint_verifying_key, vk_before,
        "refresh must preserve the joint verifying key"
    );
    assert_eq!(
        lb.committee_state.joint_verifying_key, vk_before,
        "refresh must preserve the joint verifying key on the healed peer"
    );
    assert!(
        la.local_key_package.is_some() && lb.local_key_package.is_some(),
        "both members hold rotated shares after promotion"
    );
    drop(la);
    drop(lb);
    drop(alice_engine);
    drop(bob_engine);
}

/// ZEB-1028 / CodeAnt Major on #776: an explicit-attempt propose call
/// is a retry/join of an OBSERVED ceremony; if the pending slot emptied
/// between the caller's decision and the core's lock (the stalled
/// refresh completed — epoch moved — or was aborted), the call must be
/// dropped as stale instead of seeding a spurious new ceremony against
/// the moved state.
#[tokio::test]
async fn refresh_retry_with_empty_slot_is_dropped_zeb1028() {
    let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0xDA);
    let (_bob_sk, bob_addr, bob_pub64) = fixture_identity(0xDB);
    let mut resolver_map = HashMap::new();
    resolver_map.insert(alice_addr, alice_pub64);
    resolver_map.insert(bob_addr, bob_pub64);
    let community_id = SpaceId([0xDC; 16]);
    let alice = orchestrated_node(
        community_id,
        "alice-dev",
        alice_addr,
        &alice_sk,
        &resolver_map,
    );

    let mut members = vec![alice_addr, bob_addr];
    members.sort();
    {
        // Active committee, NO pending refresh — the state a completed
        // (or aborted) ceremony leaves behind.
        let mut log = alice.log.lock().await;
        log.committee_state.active = true;
        log.committee_state.current_epoch = 2;
        log.committee_state.members = members.clone();
        log.committee_state.threshold = 2;
        log.committee_state.max_signers = 2;
        log.committee_state.identifier_map =
            harmony_app::community_dfrost_log::CommitteeState::build_identifier_map(&members);
    }

    let err = dfrost_propose_refresh_core::<MockRt, MockRt>(
        &alice.handles,
        None,
        community_id,
        members,
        2,
        Some(1),
    )
    .await
    .expect_err("stale retry against an empty slot must be dropped");
    assert!(err.contains("no longer pending"), "unexpected error: {err}");
    assert!(
        alice
            .log
            .lock()
            .await
            .committee_state
            .pending_refresh
            .is_none(),
        "the stale retry must not seed a new ceremony"
    );
}
