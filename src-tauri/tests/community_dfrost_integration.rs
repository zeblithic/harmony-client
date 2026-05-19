//! ZEB-303 Task 2-4: two-engine integration tests for the D-FROST committee
//! data layer.
//!
//! Exercises the merged [ZEB-301](https://linear.app/zeblith/issue/ZEB-301)
//! `community_dfrost_log` apply paths cross-engine: two independent
//! `DfrostLog` instances observe the same event sequence and must converge
//! on identical materialized `CommitteeState`. This is the cross-replica
//! correctness proof for the apply layer; the unit tests in
//! `community_dfrost_log.rs` cover single-engine semantics.
//!
//! Tests use REAL FROST-Ristretto255 crypto (via `community_dfrost_crypto`
//! wrappers) so DKG `dkg_part1` → `part2` → `part3` runs end-to-end.
//! Envelope sigs are synthetic `vec![0u8; 64]` because `apply()` does not
//! verify the outer Ed25519 sig (caller's responsibility — IPC layer in
//! Tasks 5-6). Only the FROST inner crypto matters for convergence.

use std::collections::BTreeMap;

use harmony_app::community_dfrost_crypto::{
    dkg_part1_local, dkg_part2_local, dkg_part3_local, verifying_key_to_bytes,
    verifying_share_to_bytes,
};
use harmony_app::community_dfrost_log::{CommitteeState, DfrostLog, PendingCeremony};
use harmony_app::community_dfrost_types::{
    DfrostEventKind, DkgCompletePayload, DkgRoundPayload, MemberVerifyingShare,
    SignedCommitteeEvent,
};
use harmony_app::community_membership::RecipientCiphertext;
use harmony_app::dm_signing;
use harmony_app::owner_state_types::{Hlc, OwnerAddr};
use x25519_dalek::{PublicKey, StaticSecret};

// ─── Fixtures ───────────────────────────────────────────────────────────────

/// Deterministic 2-member committee: Alice (addr=01..01) and Bob (addr=02..02).
const ALICE: OwnerAddr = OwnerAddr([0x01; 16]);
const BOB: OwnerAddr = OwnerAddr([0x02; 16]);
const CEREMONY_ID: [u8; 32] = [0xc0; 32];

fn members() -> Vec<OwnerAddr> {
    vec![ALICE, BOB]
}

/// X25519 keypair per node. Deterministic so the test is reproducible.
fn alice_x25519() -> ([u8; 32], [u8; 32]) {
    let priv_bytes = [0x42u8; 32];
    let pub_bytes = *PublicKey::from(&StaticSecret::from(priv_bytes)).as_bytes();
    (priv_bytes, pub_bytes)
}

fn bob_x25519() -> ([u8; 32], [u8; 32]) {
    let priv_bytes = [0x43u8; 32];
    let pub_bytes = *PublicKey::from(&StaticSecret::from(priv_bytes)).as_bytes();
    (priv_bytes, pub_bytes)
}

fn hlc(wall_ms: u64, node: &str) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: node.into(),
    }
}

/// Build a `dr` (DKG round) event with the given payload, applied as if from
/// `actor`. Sig is synthetic — `apply()` doesn't verify the envelope.
fn build_dr_event(
    actor: OwnerAddr,
    wall_ms: u64,
    node_id: &str,
    payload: DkgRoundPayload,
) -> SignedCommitteeEvent {
    let mut pd = Vec::new();
    ciborium::into_writer(&payload, &mut pd).expect("encode dr payload");
    SignedCommitteeEvent {
        tag: 'd',
        version: 1,
        committee_tier: 0,
        kind: DfrostEventKind::DkgRound,
        hlc: hlc(wall_ms, node_id),
        actor,
        payload: pd,
        sig: vec![0u8; 64],
    }
}

fn build_dk_event(
    actor: OwnerAddr,
    wall_ms: u64,
    node_id: &str,
    payload: DkgCompletePayload,
) -> SignedCommitteeEvent {
    let mut pd = Vec::new();
    ciborium::into_writer(&payload, &mut pd).expect("encode dk payload");
    SignedCommitteeEvent {
        tag: 'd',
        version: 1,
        committee_tier: 0,
        kind: DfrostEventKind::DkgComplete,
        hlc: hlc(wall_ms, node_id),
        actor,
        payload: pd,
        sig: vec![0u8; 64],
    }
}

/// Seed `pending_dkg` on a fresh log so the apply path has a ceremony to
/// match against. In production this is populated by the
/// `dfrost_initiate_dkg` IPC (Task 5); for the integration test we set it
/// directly on both engines.
fn fresh_log_with_pending() -> DfrostLog {
    let mut log = DfrostLog::new();
    log.committee_state.pending_dkg = Some(PendingCeremony {
        ceremony_id: CEREMONY_ID,
        members: members(),
        threshold: 2,
        max_signers: 2,
        proposed_epoch: 1,
        ..Default::default()
    });
    log
}

// ─── Test ───────────────────────────────────────────────────────────────────

/// 2-of-2 DKG ceremony driven through real FROST crypto, with two engines
/// observing the full broadcast/sealed message sequence. Asserts both
/// engines materialize identical `CommitteeState` (joint vk + verifying
/// shares + epoch + active flag).
///
/// Ceremony flow (each step applied on BOTH engines unless noted):
///   1. dr(rn=1) from Alice with her round-1 package
///   2. dr(rn=1) from Bob with his round-1 package
///   3. dr(rn=2) from Alice — sealed package for Bob (decrypted on B's engine
///      via apply_with_identity; A's engine has no ciphertext targeted at A
///      so the decrypt is a no-op there)
///   4. dr(rn=2) from Bob — sealed package for Alice (decrypted on A's engine)
///   5. Each engine LOCALLY runs dkg_part3_local on its own r2_secret +
///      received r1/r2 packages → PublicKeyPackage. Both engines MUST derive
///      the same joint_verifying_key (FROST guarantees this — that's the
///      whole point of DKG).
///   6. dk from Alice (or anyone — any committee member can broadcast the
///      finalization; the payload content is what's verified). Apply on both.
///   7. Assert: both engines have `committee_state.active = true`,
///      `current_epoch = 1`, identical `joint_verifying_key`, identical
///      `verifying_shares` per member.
#[test]
fn dkg_two_engine_2of2_converges_on_joint_vk() {
    let (alice_priv, alice_pub) = alice_x25519();
    let (bob_priv, bob_pub) = bob_x25519();

    // Two independent engines (one per node). Both seed pending_dkg with
    // the same ceremony — in production this comes from observing the
    // initiator's `dfrost_initiate_dkg` IPC or its corresponding broadcast.
    let mut engine_a = fresh_log_with_pending();
    let mut engine_b = fresh_log_with_pending();

    // Identifier assignment is deterministic from sorted member list.
    let id_alice = harmony_app::community_dfrost_crypto::identifier_for_index(0); // alice sorts first
    let id_bob = harmony_app::community_dfrost_crypto::identifier_for_index(1);

    // ── Round 1 ──────────────────────────────────────────────────────────
    let (r1_secret_alice, r1_pkg_alice_bytes) =
        dkg_part1_local(id_alice, 2, 2).expect("alice part1");
    let (r1_secret_bob, r1_pkg_bob_bytes) = dkg_part1_local(id_bob, 2, 2).expect("bob part1");

    // Broadcast dr(rn=1) from both nodes — both engines see both events.
    let dr1_alice = build_dr_event(
        ALICE,
        1_000,
        "alice",
        DkgRoundPayload {
            ceremony_id: CEREMONY_ID,
            round_num: 1,
            round1_package: Some(r1_pkg_alice_bytes.clone()),
            recipient_ciphertexts: None,
        },
    );
    let dr1_bob = build_dr_event(
        BOB,
        1_100,
        "bob",
        DkgRoundPayload {
            ceremony_id: CEREMONY_ID,
            round_num: 1,
            round1_package: Some(r1_pkg_bob_bytes.clone()),
            recipient_ciphertexts: None,
        },
    );
    engine_a
        .apply(dr1_alice.clone())
        .expect("a applies dr1_alice");
    engine_a.apply(dr1_bob.clone()).expect("a applies dr1_bob");
    engine_b.apply(dr1_alice).expect("b applies dr1_alice");
    engine_b.apply(dr1_bob).expect("b applies dr1_bob");

    // ── Round 2 ──────────────────────────────────────────────────────────
    // Each node runs part2 with the OTHER node's round-1 package; produces
    // a per-recipient map of round-2 packages (one entry per other member).
    let mut r1_recv_for_alice = BTreeMap::new();
    r1_recv_for_alice.insert(id_bob, r1_pkg_bob_bytes.clone());
    let (r2_secret_alice, r2_pkgs_from_alice) =
        dkg_part2_local(r1_secret_alice, &r1_recv_for_alice).expect("alice part2");

    let mut r1_recv_for_bob = BTreeMap::new();
    r1_recv_for_bob.insert(id_alice, r1_pkg_alice_bytes.clone());
    let (r2_secret_bob, r2_pkgs_from_bob) =
        dkg_part2_local(r1_secret_bob, &r1_recv_for_bob).expect("bob part2");

    // Alice's r2 package destined for Bob: seal to Bob's x25519 pubkey.
    let r2_pkg_alice_to_bob = r2_pkgs_from_alice
        .get(&id_bob)
        .expect("alice produced r2 pkg for bob")
        .clone();
    let sealed_alice_to_bob =
        dm_signing::seal_to_owner(&bob_pub, &r2_pkg_alice_to_bob).expect("seal a→b");

    let dr2_alice = build_dr_event(
        ALICE,
        2_000,
        "alice",
        DkgRoundPayload {
            ceremony_id: CEREMONY_ID,
            round_num: 2,
            round1_package: None,
            recipient_ciphertexts: Some(vec![RecipientCiphertext {
                recipient: BOB,
                sealed: sealed_alice_to_bob,
            }]),
        },
    );

    // Alice's engine applies its own outgoing rn=2 via apply_with_identity
    // (no decrypt for self since the ciphertext is for Bob).
    engine_a
        .apply_with_identity(dr2_alice.clone(), &ALICE, &alice_priv)
        .expect("a applies own dr2");
    // Bob's engine applies + decrypts.
    engine_b
        .apply_with_identity(dr2_alice, &BOB, &bob_priv)
        .expect("b applies+decrypts dr2 from alice");

    // Symmetric for Bob → Alice
    let r2_pkg_bob_to_alice = r2_pkgs_from_bob
        .get(&id_alice)
        .expect("bob produced r2 pkg for alice")
        .clone();
    let sealed_bob_to_alice =
        dm_signing::seal_to_owner(&alice_pub, &r2_pkg_bob_to_alice).expect("seal b→a");
    let dr2_bob = build_dr_event(
        BOB,
        2_100,
        "bob",
        DkgRoundPayload {
            ceremony_id: CEREMONY_ID,
            round_num: 2,
            round1_package: None,
            recipient_ciphertexts: Some(vec![RecipientCiphertext {
                recipient: ALICE,
                sealed: sealed_bob_to_alice,
            }]),
        },
    );
    engine_a
        .apply_with_identity(dr2_bob.clone(), &ALICE, &alice_priv)
        .expect("a applies+decrypts dr2 from bob");
    engine_b
        .apply_with_identity(dr2_bob, &BOB, &bob_priv)
        .expect("b applies own dr2");

    // ── Sanity: both engines have the same r1_packages map ───────────────
    let pending_a = engine_a
        .committee_state
        .pending_dkg
        .as_ref()
        .expect("a pending");
    let pending_b = engine_b
        .committee_state
        .pending_dkg
        .as_ref()
        .expect("b pending");
    assert_eq!(pending_a.round1_packages, pending_b.round1_packages);

    // ── Round 3 (local finalization on each engine) ──────────────────────
    // dkg_part3_local takes the OTHER participants' r1 + r2 packages. Each
    // engine has access to (a) the other's r1 package (from broadcast),
    // (b) the other's r2 package addressed to self (decrypted via
    // apply_with_identity above).
    let alice_r1_recv: BTreeMap<_, _> = [(id_bob, r1_pkg_bob_bytes.clone())].into_iter().collect();
    let alice_r2_recv: BTreeMap<_, _> = [(id_bob, r2_pkg_bob_to_alice.clone())]
        .into_iter()
        .collect();
    let (_key_pkg_alice, pub_pkg_alice) =
        dkg_part3_local(&r2_secret_alice, &alice_r1_recv, &alice_r2_recv).expect("alice part3");

    let bob_r1_recv: BTreeMap<_, _> = [(id_alice, r1_pkg_alice_bytes.clone())]
        .into_iter()
        .collect();
    let bob_r2_recv: BTreeMap<_, _> = [(id_alice, r2_pkg_alice_to_bob.clone())]
        .into_iter()
        .collect();
    let (_key_pkg_bob, pub_pkg_bob) =
        dkg_part3_local(&r2_secret_bob, &bob_r1_recv, &bob_r2_recv).expect("bob part3");

    // FROST guarantees this — the whole point of DKG.
    let joint_vk_alice = verifying_key_to_bytes(pub_pkg_alice.verifying_key());
    let joint_vk_bob = verifying_key_to_bytes(pub_pkg_bob.verifying_key());
    assert_eq!(
        joint_vk_alice, joint_vk_bob,
        "FROST DKG cross-engine joint vk MUST match (this is the protocol's correctness criterion)"
    );

    // ── dk broadcast finalization ────────────────────────────────────────
    // Either party can build the dk event (the payload content, not the
    // actor, is what's verified). Use Alice as the broadcaster.
    let identifier_map = CommitteeState::build_identifier_map(&members());
    let mut verifying_shares = Vec::with_capacity(2);
    for member in members() {
        let id = identifier_map[&member];
        let vs = pub_pkg_alice
            .verifying_shares()
            .get(&id)
            .expect("verifying share for id");
        verifying_shares.push(MemberVerifyingShare {
            member,
            verifying_share: verifying_share_to_bytes(vs),
        });
    }

    let dk_payload = DkgCompletePayload {
        ceremony_id: CEREMONY_ID,
        joint_verifying_key: joint_vk_alice,
        verifying_shares,
        epoch: 1,
        members: members(),
        threshold: 2,
        max_signers: 2,
    };
    // Activation requires `dk_confirmations.len() >= threshold` (=2 here),
    // so BOTH members must broadcast a `dk` event. Cross-confirmation
    // consensus (R4) enforces both dk payloads have identical
    // joint_vk + verifying_shares — guaranteed by FROST, but the apply
    // path checks it loudly.
    let dk_alice = build_dk_event(ALICE, 3_000, "alice", dk_payload.clone());
    let dk_bob = build_dk_event(BOB, 3_100, "bob", dk_payload);
    engine_a
        .apply(dk_alice.clone())
        .expect("a applies alice's dk");
    engine_b.apply(dk_alice).expect("b applies alice's dk");
    engine_a.apply(dk_bob.clone()).expect("a applies bob's dk");
    engine_b.apply(dk_bob).expect("b applies bob's dk");

    // ── Convergence assertions ───────────────────────────────────────────
    assert!(engine_a.committee_state.active, "engine A must be active");
    assert!(engine_b.committee_state.active, "engine B must be active");
    assert_eq!(engine_a.committee_state.current_epoch, 1);
    assert_eq!(engine_b.committee_state.current_epoch, 1);
    assert_eq!(
        engine_a.committee_state.joint_verifying_key,
        engine_b.committee_state.joint_verifying_key
    );
    assert_eq!(
        engine_a.committee_state.joint_verifying_key,
        Some(joint_vk_alice)
    );
    assert_eq!(
        engine_a.committee_state.verifying_shares,
        engine_b.committee_state.verifying_shares
    );
    assert_eq!(
        engine_a.committee_state.members,
        engine_b.committee_state.members
    );
    assert_eq!(engine_a.committee_state.threshold, 2);
    assert_eq!(engine_a.committee_state.max_signers, 2);

    // pending_dkg should be cleared after activation on BOTH engines.
    assert!(engine_a.committee_state.pending_dkg.is_none());
    assert!(engine_b.committee_state.pending_dkg.is_none());
}
