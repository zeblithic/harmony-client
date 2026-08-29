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
//! wrappers) so DKG `dkg_part1` → `part2` → `part3` and threshold-sign
//! `round1::commit` → `round2::sign` → `aggregate` run end-to-end.
//! Envelope sigs are synthetic `vec![0u8; 64]` because `apply()` does not
//! verify the outer Ed25519 sig (caller's responsibility — IPC layer in
//! Tasks 5-6). Only the FROST inner crypto matters for convergence.

use std::collections::BTreeMap;

use frost_ristretto255::{
    self as frost,
    keys::{KeyPackage, PublicKeyPackage},
    rand_core, Identifier,
};
use harmony_app::community_dfrost_crypto::{
    dkg_part1_local, dkg_part2_local, dkg_part3_local, identifier_for_index,
    verifying_key_to_bytes, verifying_share_to_bytes,
};
use harmony_app::community_dfrost_log::{ApplyError, CommitteeState, DfrostLog, PendingCeremony};
use harmony_app::community_dfrost_types::{
    derive_vrf_output, DfrostEventKind, DkgCompletePayload, DkgRoundPayload, MemberVerifyingShare,
    RefreshRoundPayload, SignedCommitteeEvent, ThresholdSignPayload, VrfBeaconPayload,
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

fn build_ts_event(
    actor: OwnerAddr,
    wall_ms: u64,
    node_id: &str,
    payload: ThresholdSignPayload,
) -> SignedCommitteeEvent {
    let mut pd = Vec::new();
    ciborium::into_writer(&payload, &mut pd).expect("encode ts payload");
    SignedCommitteeEvent {
        tag: 'd',
        version: 1,
        committee_tier: 0,
        kind: DfrostEventKind::ThresholdSign,
        hlc: hlc(wall_ms, node_id),
        actor,
        payload: pd,
        sig: vec![0u8; 64],
    }
}

fn build_rf_event(
    actor: OwnerAddr,
    wall_ms: u64,
    node_id: &str,
    payload: RefreshRoundPayload,
) -> SignedCommitteeEvent {
    let mut pd = Vec::new();
    ciborium::into_writer(&payload, &mut pd).expect("encode rf payload");
    SignedCommitteeEvent {
        tag: 'd',
        version: 1,
        committee_tier: 0,
        kind: DfrostEventKind::ProactiveRefresh,
        hlc: hlc(wall_ms, node_id),
        actor,
        payload: pd,
        sig: vec![0u8; 64],
    }
}

fn build_vb_event(
    actor: OwnerAddr,
    wall_ms: u64,
    node_id: &str,
    payload: VrfBeaconPayload,
) -> SignedCommitteeEvent {
    let mut pd = Vec::new();
    ciborium::into_writer(&payload, &mut pd).expect("encode vb payload");
    SignedCommitteeEvent {
        tag: 'd',
        version: 1,
        committee_tier: 0,
        kind: DfrostEventKind::VrfBeacon,
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

// ─── Shared setup ───────────────────────────────────────────────────────────

/// State left behind by a successful 2-of-2 DKG ceremony — both engines
/// active on the same joint verifying key, plus the FROST `KeyPackage`s
/// and `PublicKeyPackage` needed for downstream sign/refresh tests.
struct ActivatedCommittee {
    engine_a: DfrostLog,
    engine_b: DfrostLog,
    alice_key_pkg: KeyPackage,
    bob_key_pkg: KeyPackage,
    pub_pkg: PublicKeyPackage,
    joint_vk: [u8; 32],
    id_alice: Identifier,
    id_bob: Identifier,
}

/// Drive a full 2-of-2 DKG ceremony cross-engine and return the activated
/// state. Shared by Tasks 2, 3, and 4 so downstream tests don't reimplement
/// the (long) DKG setup. The convergence assertions live in the Task 2
/// test that calls this; Tasks 3 + 4 inherit the convergence as a
/// precondition.
fn dkg_2of2_setup() -> ActivatedCommittee {
    let (alice_priv, alice_pub) = alice_x25519();
    let (bob_priv, bob_pub) = bob_x25519();

    let mut engine_a = fresh_log_with_pending();
    let mut engine_b = fresh_log_with_pending();

    let id_alice = identifier_for_index(0); // alice sorts first
    let id_bob = identifier_for_index(1);

    // Round 1
    let (r1_secret_alice, r1_pkg_alice_bytes) =
        dkg_part1_local(id_alice, 2, 2).expect("alice part1");
    let (r1_secret_bob, r1_pkg_bob_bytes) = dkg_part1_local(id_bob, 2, 2).expect("bob part1");
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
    engine_a.apply(dr1_alice.clone()).expect("a applies dr1a");
    engine_a.apply(dr1_bob.clone()).expect("a applies dr1b");
    engine_b.apply(dr1_alice).expect("b applies dr1a");
    engine_b.apply(dr1_bob).expect("b applies dr1b");

    // Round 2
    let r1_recv_for_alice: BTreeMap<_, _> =
        [(id_bob, r1_pkg_bob_bytes.clone())].into_iter().collect();
    let (r2_secret_alice, r2_pkgs_from_alice) =
        dkg_part2_local(r1_secret_alice, &r1_recv_for_alice).expect("alice part2");

    let r1_recv_for_bob: BTreeMap<_, _> = [(id_alice, r1_pkg_alice_bytes.clone())]
        .into_iter()
        .collect();
    let (r2_secret_bob, r2_pkgs_from_bob) =
        dkg_part2_local(r1_secret_bob, &r1_recv_for_bob).expect("bob part2");

    let r2_pkg_alice_to_bob = r2_pkgs_from_alice
        .get(&id_bob)
        .expect("alice r2 for bob")
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
    engine_a
        .apply_with_identity(dr2_alice.clone(), &ALICE, &alice_priv)
        .expect("a applies own dr2");
    engine_b
        .apply_with_identity(dr2_alice, &BOB, &bob_priv)
        .expect("b decrypts dr2 from alice");

    let r2_pkg_bob_to_alice = r2_pkgs_from_bob
        .get(&id_alice)
        .expect("bob r2 for alice")
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
        .expect("a decrypts dr2 from bob");
    engine_b
        .apply_with_identity(dr2_bob, &BOB, &bob_priv)
        .expect("b applies own dr2");

    // Qodo R3 (Bug): explicitly assert that `apply_with_identity` actually
    // decrypted the round-2 packages addressed to each engine and stored
    // the resulting plaintexts in `pending_dkg.round2_packages[sender]`.
    // Without this check, a regression in recipient matching or the
    // X25519 decrypt path would slip past the test — the FROST
    // finalization below uses the LOCALLY-produced plaintexts (which the
    // test holds out-of-band), so a broken decrypt would still produce
    // a converged committee in this test. Compare the decrypted bytes
    // against the originals to prove the round-2 cross-engine path
    // round-trips cleanly.
    {
        let pending_a = engine_a
            .committee_state
            .pending_dkg
            .as_ref()
            .expect("a pending after r2");
        let pending_b = engine_b
            .committee_state
            .pending_dkg
            .as_ref()
            .expect("b pending after r2");
        // Alice's engine should have decrypted Bob's r2 package for her.
        assert_eq!(
            pending_a.round2_packages.get(&BOB),
            Some(&r2_pkg_bob_to_alice),
            "engine A must have decrypted Bob's sealed r2 package via apply_with_identity"
        );
        // Bob's engine should have decrypted Alice's r2 package for him.
        assert_eq!(
            pending_b.round2_packages.get(&ALICE),
            Some(&r2_pkg_alice_to_bob),
            "engine B must have decrypted Alice's sealed r2 package via apply_with_identity"
        );
        // Each engine should NOT have stored a decrypt for its own
        // outgoing package (no ciphertext was targeted at self).
        assert!(
            !pending_a.round2_packages.contains_key(&ALICE),
            "engine A must not have a self-addressed r2 decrypt"
        );
        assert!(
            !pending_b.round2_packages.contains_key(&BOB),
            "engine B must not have a self-addressed r2 decrypt"
        );
    }

    // Round 3 — local on each engine
    let alice_r1_recv: BTreeMap<_, _> = [(id_bob, r1_pkg_bob_bytes.clone())].into_iter().collect();
    let alice_r2_recv: BTreeMap<_, _> = [(id_bob, r2_pkg_bob_to_alice.clone())]
        .into_iter()
        .collect();
    let (alice_key_pkg, pub_pkg) =
        dkg_part3_local(&r2_secret_alice, &alice_r1_recv, &alice_r2_recv).expect("alice part3");

    let bob_r1_recv: BTreeMap<_, _> = [(id_alice, r1_pkg_alice_bytes.clone())]
        .into_iter()
        .collect();
    let bob_r2_recv: BTreeMap<_, _> = [(id_alice, r2_pkg_alice_to_bob.clone())]
        .into_iter()
        .collect();
    let (bob_key_pkg, _pub_pkg_bob) =
        dkg_part3_local(&r2_secret_bob, &bob_r1_recv, &bob_r2_recv).expect("bob part3");

    let joint_vk = verifying_key_to_bytes(pub_pkg.verifying_key());

    // dk broadcast — BOTH members confirm (threshold=2 requires 2 confirmations)
    let identifier_map = CommitteeState::build_identifier_map(&members());
    let mut verifying_shares = Vec::with_capacity(2);
    for member in members() {
        let id = identifier_map[&member];
        let vs = pub_pkg
            .verifying_shares()
            .get(&id)
            .expect("verifying share");
        verifying_shares.push(MemberVerifyingShare {
            member,
            verifying_share: verifying_share_to_bytes(vs),
        });
    }
    let dk_payload = DkgCompletePayload {
        ceremony_id: CEREMONY_ID,
        joint_verifying_key: joint_vk,
        verifying_shares,
        epoch: 1,
        members: members(),
        threshold: 2,
        max_signers: 2,
    };
    let dk_alice = build_dk_event(ALICE, 3_000, "alice", dk_payload.clone());
    let dk_bob = build_dk_event(BOB, 3_100, "bob", dk_payload);
    engine_a.apply(dk_alice.clone()).expect("a dk_alice");
    engine_b.apply(dk_alice).expect("b dk_alice");
    engine_a.apply(dk_bob.clone()).expect("a dk_bob");
    engine_b.apply(dk_bob).expect("b dk_bob");

    ActivatedCommittee {
        engine_a,
        engine_b,
        alice_key_pkg,
        bob_key_pkg,
        pub_pkg,
        joint_vk,
        id_alice,
        id_bob,
    }
}

// ─── Task 2: DKG convergence ────────────────────────────────────────────────

/// 2-of-2 DKG ceremony driven through real FROST crypto, with two engines
/// observing the full broadcast/sealed message sequence. Asserts both
/// engines materialize identical `CommitteeState` (joint vk + verifying
/// shares + epoch + active flag).
#[test]
fn dkg_two_engine_2of2_converges_on_joint_vk() {
    let c = dkg_2of2_setup();

    assert!(c.engine_a.committee_state.active, "engine A must be active");
    assert!(c.engine_b.committee_state.active, "engine B must be active");
    assert_eq!(c.engine_a.committee_state.current_epoch, 1);
    assert_eq!(c.engine_b.committee_state.current_epoch, 1);
    assert_eq!(
        c.engine_a.committee_state.joint_verifying_key,
        c.engine_b.committee_state.joint_verifying_key
    );
    assert_eq!(
        c.engine_a.committee_state.joint_verifying_key,
        Some(c.joint_vk)
    );
    assert_eq!(
        c.engine_a.committee_state.verifying_shares,
        c.engine_b.committee_state.verifying_shares
    );
    assert_eq!(
        c.engine_a.committee_state.members,
        c.engine_b.committee_state.members
    );
    assert_eq!(c.engine_a.committee_state.threshold, 2);
    assert_eq!(c.engine_a.committee_state.max_signers, 2);
    assert!(c.engine_a.committee_state.pending_dkg.is_none());
    assert!(c.engine_b.committee_state.pending_dkg.is_none());
}

// ─── Task 3: Threshold sign + VRF beacon ────────────────────────────────────

/// Full FROST threshold-sign → aggregate → vb cycle driven cross-engine.
/// After Task 2's DKG converges, both engines have an active committee on
/// the same joint vk. This test runs `round1::commit` per signer, builds a
/// SigningPackage, runs `round2::sign` per signer, aggregates into a real
/// 64-byte Schnorr signature, and broadcasts a `vb` event. Both engines
/// must apply the `vb` cleanly (the apply path verifies the Schnorr
/// signature against the joint vk via `verify_schnorr_signature` — R2
/// fix in PR #137).
#[test]
fn threshold_sign_two_engine_vrf_beacon_verifies() {
    let mut c = dkg_2of2_setup();

    // Choose a deterministic message hash for the signing ceremony.
    let sign_ceremony_id: [u8; 32] = [0xab; 32];
    let message_hash: [u8; 32] = [0x77; 32];

    // ── Round 1: each signer commits ──────────────────────────────────────
    let (alice_nonces, alice_commitments) =
        frost::round1::commit(c.alice_key_pkg.signing_share(), &mut rand_core::OsRng);
    let (bob_nonces, bob_commitments) =
        frost::round1::commit(c.bob_key_pkg.signing_share(), &mut rand_core::OsRng);

    // Encode commitments for the wire (`ts` event carries opaque commitment_bytes).
    let mut alice_cm_bytes = Vec::new();
    ciborium::into_writer(&alice_commitments, &mut alice_cm_bytes).expect("encode alice cm");
    let mut bob_cm_bytes = Vec::new();
    ciborium::into_writer(&bob_commitments, &mut bob_cm_bytes).expect("encode bob cm");

    // ── SigningPackage: collected commitments + message ───────────────────
    let mut commitments_map: BTreeMap<Identifier, frost::round1::SigningCommitments> =
        BTreeMap::new();
    commitments_map.insert(c.id_alice, alice_commitments);
    commitments_map.insert(c.id_bob, bob_commitments);
    let signing_package = frost::SigningPackage::new(commitments_map, &message_hash);

    // ── Round 2: each signer produces a signature share ───────────────────
    let alice_share = frost::round2::sign(&signing_package, &alice_nonces, &c.alice_key_pkg)
        .expect("alice round2 sign");
    let bob_share =
        frost::round2::sign(&signing_package, &bob_nonces, &c.bob_key_pkg).expect("bob round2");

    let mut alice_share_bytes = Vec::new();
    ciborium::into_writer(&alice_share, &mut alice_share_bytes).expect("encode alice share");
    let mut bob_share_bytes = Vec::new();
    ciborium::into_writer(&bob_share, &mut bob_share_bytes).expect("encode bob share");

    // ── ts events: both members contribute ────────────────────────────────
    let ts_alice = build_ts_event(
        ALICE,
        4_000,
        "alice",
        ThresholdSignPayload {
            ceremony_id: sign_ceremony_id,
            message_hash,
            commitment_bytes: alice_cm_bytes,
            share_bytes: alice_share_bytes,
        },
    );
    let ts_bob = build_ts_event(
        BOB,
        4_100,
        "bob",
        ThresholdSignPayload {
            ceremony_id: sign_ceremony_id,
            message_hash,
            commitment_bytes: bob_cm_bytes,
            share_bytes: bob_share_bytes,
        },
    );
    c.engine_a.apply(ts_alice.clone()).expect("a ts_alice");
    c.engine_a.apply(ts_bob.clone()).expect("a ts_bob");
    c.engine_b.apply(ts_alice).expect("b ts_alice");
    c.engine_b.apply(ts_bob).expect("b ts_bob");

    // Both engines now have pending_sign[sign_ceremony_id] with two contributions.
    let pending_a = c
        .engine_a
        .committee_state
        .pending_sign
        .get(&sign_ceremony_id)
        .expect("a has pending sign session");
    let pending_b = c
        .engine_b
        .committee_state
        .pending_sign
        .get(&sign_ceremony_id)
        .expect("b has pending sign session");
    assert_eq!(pending_a.contributions.len(), 2);
    assert_eq!(pending_b.contributions.len(), 2);
    assert_eq!(pending_a.message_hash, message_hash);
    assert_eq!(pending_b.message_hash, message_hash);

    // ── Aggregate the signature shares → final Schnorr sig ────────────────
    let mut shares_map: BTreeMap<Identifier, frost::round2::SignatureShare> = BTreeMap::new();
    shares_map.insert(c.id_alice, alice_share);
    shares_map.insert(c.id_bob, bob_share);
    let group_signature = frost::aggregate(&signing_package, &shares_map, &c.pub_pkg)
        .expect("aggregate threshold sig");

    let sig_bytes = group_signature.serialize().expect("serialize sig");
    assert_eq!(sig_bytes.len(), 64, "Schnorr sig must be 64 bytes");

    // Independently verify the signature against the joint vk (sanity check).
    c.pub_pkg
        .verifying_key()
        .verify(&message_hash, &group_signature)
        .expect("aggregated signature verifies under joint vk");

    // ── Compute VRF output from R component (first 32 bytes of sig) ───────
    let mut r_compressed = [0u8; 32];
    r_compressed.copy_from_slice(&sig_bytes[..32]);
    let vrf_output = derive_vrf_output(&r_compressed);

    // ── vb event: broadcast the aggregated beacon ─────────────────────────
    let vb = build_vb_event(
        ALICE,
        5_000,
        "alice",
        VrfBeaconPayload {
            ceremony_id: sign_ceremony_id,
            message_hash,
            signature: sig_bytes.clone(),
            vrf_output,
        },
    );

    // R4 (CodeRabbit): negative subcase — a tampered vb MUST be rejected
    // AND must leave pending_sign untouched. Without this check, a
    // regression that disabled the Schnorr-verify or the VRF-output
    // binding step inside apply_vrf_beacon would still pass this test
    // (the happy path below would just clear pending_sign).
    //
    // Tamper one byte of the signature; apply_vrf_beacon should reject
    // with InvariantViolation (either the SHA-256(R) binding check fails
    // because R bytes differ, OR the Schnorr verify against joint_vk
    // fails). Either way, pending_sign must be intact for the happy-path
    // apply below.
    {
        let mut tampered_sig = sig_bytes.clone();
        tampered_sig[0] ^= 0x01; // flip lowest bit of R's first byte
        let vb_tampered = build_vb_event(
            ALICE,
            4_500,
            "alice",
            VrfBeaconPayload {
                ceremony_id: sign_ceremony_id,
                message_hash,
                signature: tampered_sig,
                vrf_output, // unchanged — binding check will fail
            },
        );
        let err = c
            .engine_a
            .apply(vb_tampered)
            .expect_err("tampered vb must be rejected");
        assert!(
            matches!(err, ApplyError::InvariantViolation),
            "expected InvariantViolation for tampered vb, got {err:?}"
        );
        // pending_sign on engine A still intact — the tampered apply failed
        // BEFORE the pending_sign.remove() at the end of apply_vrf_beacon.
        assert!(
            c.engine_a
                .committee_state
                .pending_sign
                .contains_key(&sign_ceremony_id),
            "engine A pending_sign must be intact after rejected vb"
        );
    }

    c.engine_a.apply(vb.clone()).expect("a applies vb");
    c.engine_b.apply(vb).expect("b applies vb");

    // ── Convergence: both engines cleared pending_sign for this ceremony ──
    assert!(
        !c.engine_a
            .committee_state
            .pending_sign
            .contains_key(&sign_ceremony_id),
        "engine A pending_sign must clear after successful vb"
    );
    assert!(
        !c.engine_b
            .committee_state
            .pending_sign
            .contains_key(&sign_ceremony_id),
        "engine B pending_sign must clear after successful vb"
    );

    // Both engines retain the same active committee + joint vk.
    assert_eq!(
        c.engine_a.committee_state.joint_verifying_key,
        c.engine_b.committee_state.joint_verifying_key
    );
}

// ─── Delta #3 coverage: post-activation pending_dkg rejection ──────────────

/// R5 invariant from PR #137: once `committee_state.active`, a `dk` event
/// finalizing a `pending_dkg` (vs. `pending_refresh`) slot MUST be rejected
/// with `InvariantViolation`. Without this guard, a stale or race-condition
/// `pending_dkg` could silently rewrite the active committee's members /
/// threshold / max_signers / current_epoch while reusing the existing
/// joint vk — a covert committee-shape swap.
///
/// This integration test reproduces the cross-engine scenario: after a
/// successful 2-of-2 DKG activates both engines, simulate a stale
/// `pending_dkg` slot lingering on engine A and attempt a `dk` against it.
/// `apply_dkg_complete` must reject with `InvariantViolation` and leave the
/// active committee state untouched.
///
/// Single-engine coverage of the same invariant lives at
/// `community_dfrost_log.rs::dk_against_pending_dkg_after_activation_rejected`
/// (PR #137 R5 fix). This integration test confirms the same guard is
/// reachable from the cross-engine apply path.
#[test]
fn dk_rejected_after_active_with_pending_dkg_slot() {
    let mut c = dkg_2of2_setup();
    assert!(c.engine_a.committee_state.active, "precondition: active");
    let original_vk = c.engine_a.committee_state.joint_verifying_key;
    let original_epoch = c.engine_a.committee_state.current_epoch;
    let original_members = c.engine_a.committee_state.members.clone();

    // Simulate a stale pending_dkg lingering on engine A (e.g., race or
    // corrupted state). Use a DIFFERENT ceremony_id from the activating
    // ceremony so the lookup actually finds this slot vs. mismatching.
    let stale_ceremony_id: [u8; 32] = [0xff; 32];
    c.engine_a.committee_state.pending_dkg = Some(PendingCeremony {
        ceremony_id: stale_ceremony_id,
        members: members(),
        threshold: 2,
        max_signers: 2,
        proposed_epoch: 99, // arbitrary, would silently overwrite if guard missing
        ..Default::default()
    });

    // Build a dk that would otherwise be valid (joint_vk matches the
    // active vk — passes the "no vk drift" check at apply_dkg_complete:362).
    let identifier_map = CommitteeState::build_identifier_map(&members());
    let mut verifying_shares = Vec::with_capacity(2);
    for member in members() {
        let id = identifier_map[&member];
        let vs = c
            .pub_pkg
            .verifying_shares()
            .get(&id)
            .expect("verifying share");
        verifying_shares.push(MemberVerifyingShare {
            member,
            verifying_share: verifying_share_to_bytes(vs),
        });
    }
    let stale_dk = build_dk_event(
        ALICE,
        9_000,
        "alice",
        DkgCompletePayload {
            ceremony_id: stale_ceremony_id,
            joint_verifying_key: original_vk.expect("active vk"),
            verifying_shares,
            epoch: 99,
            members: members(),
            threshold: 2,
            max_signers: 2,
        },
    );

    // R5 invariant fires: active + PendingSlot::Dkg → InvariantViolation
    let err = c
        .engine_a
        .apply(stale_dk)
        .expect_err("post-activation pending_dkg dk must be rejected");
    assert!(
        matches!(err, ApplyError::InvariantViolation),
        "expected InvariantViolation, got {err:?}"
    );

    // Active committee state untouched — no covert rewrite.
    assert!(c.engine_a.committee_state.active);
    assert_eq!(c.engine_a.committee_state.joint_verifying_key, original_vk);
    assert_eq!(c.engine_a.committee_state.current_epoch, original_epoch);
    assert_eq!(c.engine_a.committee_state.members, original_members);
}

// ─── Task 4: Proactive refresh preserves joint vk ───────────────────────────

/// Proactive refresh ceremony rotates secret shares within the same
/// committee membership, advancing the epoch and (by protocol invariant)
/// preserving the joint verifying key. Both engines apply the rf rn=1
/// events + dk finalizations and must end up with current_epoch=2 and
/// joint_verifying_key unchanged from epoch 1.
///
/// The apply layer enforces the vk-preservation invariant: while
/// `committee_state.active`, any incoming `dk` payload whose
/// `joint_verifying_key` differs from the existing vk is rejected with
/// InvariantViolation. The refresh dk events here REUSE the epoch-1
/// joint vk to satisfy this.
///
/// Note: the apply path does NOT verify that the per-member
/// `verifying_shares` are cryptographically consistent with a new share
/// polynomial — it only stores them. A real refresh would derive new
/// shares via FROST repair / resharing protocol; the integration test
/// scopes to the apply-layer convergence, not the FROST refresh primitive
/// itself (which lives in `community_dfrost_crypto` and has its own unit
/// coverage).
#[test]
fn refresh_two_engine_preserves_joint_vk() {
    let mut c = dkg_2of2_setup();
    let (alice_priv, alice_pub) = alice_x25519();
    let (bob_priv, bob_pub) = bob_x25519();

    let refresh_ceremony_id: [u8; 32] = [0xee; 32];
    let preserved_vk = c.joint_vk;

    // ── rf rn=1 (ZEB-1027): each member broadcasts its PUBLIC
    //    zero-sharing round-1 commitment — same shape as dr rn=1.
    //    Synthetic package bytes: the apply path stores them without
    //    interpretation (the FROST content is unit-covered in
    //    community_dfrost_crypto).
    let alice_r1_pkg = vec![0xaa; 32];
    let bob_r1_pkg = vec![0xbb; 32];
    for (actor, pkg, wall) in [
        (ALICE, alice_r1_pkg.clone(), 6_000u64),
        (BOB, bob_r1_pkg.clone(), 6_050),
    ] {
        let rf1 = build_rf_event(
            actor,
            wall,
            if actor == ALICE { "alice" } else { "bob" },
            RefreshRoundPayload {
                ceremony_id: refresh_ceremony_id,
                round_num: 1,
                recipient_ciphertexts: None,
                package: Some(pkg),
            },
        );
        c.engine_a
            .apply_with_identity(rf1.clone(), &ALICE, &alice_priv)
            .expect("a applies rf1");
        c.engine_b
            .apply_with_identity(rf1, &BOB, &bob_priv)
            .expect("b applies rf1");
    }

    // ── rf rn=2: sealed per-recipient round-2 packages (here: alice's
    //    only, targeting bob — plus a sealed-to-self entry, matching the
    //    production core's uniform distribution).
    let synthetic_share_for_alice = vec![0xa1; 64];
    let synthetic_share_for_bob = vec![0xb1; 64];
    let rf2 = build_rf_event(
        ALICE,
        6_100,
        "alice",
        RefreshRoundPayload {
            ceremony_id: refresh_ceremony_id,
            round_num: 2,
            recipient_ciphertexts: Some(vec![
                RecipientCiphertext {
                    recipient: ALICE,
                    sealed: dm_signing::seal_to_owner(&alice_pub, &synthetic_share_for_alice)
                        .expect("seal alice"),
                },
                RecipientCiphertext {
                    recipient: BOB,
                    sealed: dm_signing::seal_to_owner(&bob_pub, &synthetic_share_for_bob)
                        .expect("seal bob"),
                },
            ]),
            package: None,
        },
    );
    c.engine_a
        .apply_with_identity(rf2.clone(), &ALICE, &alice_priv)
        .expect("a applies rf2 + decrypts");
    c.engine_b
        .apply_with_identity(rf2, &BOB, &bob_priv)
        .expect("b applies rf2 + decrypts");

    // ── Both engines now have pending_refresh populated ──────────────────
    let pr_a = c
        .engine_a
        .committee_state
        .pending_refresh
        .as_ref()
        .expect("a pending_refresh");
    let pr_b = c
        .engine_b
        .committee_state
        .pending_refresh
        .as_ref()
        .expect("b pending_refresh");
    assert_eq!(pr_a.ceremony_id, refresh_ceremony_id);
    assert_eq!(pr_b.ceremony_id, refresh_ceremony_id);
    assert_eq!(pr_a.proposed_epoch, 2);
    assert_eq!(pr_b.proposed_epoch, 2);
    // rn=1 public packages accumulated on both engines (ZEB-1027).
    assert_eq!(pr_a.round1_packages.get(&ALICE), Some(&alice_r1_pkg));
    assert_eq!(pr_a.round1_packages.get(&BOB), Some(&bob_r1_pkg));
    assert_eq!(pr_b.round1_packages.get(&ALICE), Some(&alice_r1_pkg));
    assert_eq!(pr_b.round1_packages.get(&BOB), Some(&bob_r1_pkg));

    // R4 (CodeRabbit) lineage: each engine must have decrypted the rn=2
    // ciphertext addressed to it (keyed by sender = ALICE).
    assert_eq!(
        pr_a.round2_packages.get(&ALICE),
        Some(&synthetic_share_for_alice),
        "engine A must have decrypted refresh share targeted at ALICE"
    );
    assert_eq!(
        pr_b.round2_packages.get(&ALICE),
        Some(&synthetic_share_for_bob),
        "engine B must have decrypted refresh share targeted at BOB"
    );
    // Each engine stored exactly one entry (only one sender's rf rn=2).
    assert_eq!(
        pr_a.round2_packages.len(),
        1,
        "engine A round2_packages must have exactly one entry"
    );
    assert_eq!(
        pr_b.round2_packages.len(),
        1,
        "engine B round2_packages must have exactly one entry"
    );

    // ── dk finalization: both members confirm, joint_vk preserved ────────
    // Verifying_shares: reuse epoch-1 shares (the apply path doesn't
    // verify shares-to-secret consistency).
    let identifier_map = CommitteeState::build_identifier_map(&members());
    let mut verifying_shares = Vec::with_capacity(2);
    for member in members() {
        let id = identifier_map[&member];
        let vs = c
            .pub_pkg
            .verifying_shares()
            .get(&id)
            .expect("verifying share");
        verifying_shares.push(MemberVerifyingShare {
            member,
            verifying_share: verifying_share_to_bytes(vs),
        });
    }
    let dk_payload = DkgCompletePayload {
        ceremony_id: refresh_ceremony_id,
        joint_verifying_key: preserved_vk, // MUST equal existing for refresh
        verifying_shares,
        epoch: 2,
        members: members(),
        threshold: 2,
        max_signers: 2,
    };
    let dk_alice = build_dk_event(ALICE, 7_000, "alice", dk_payload.clone());
    let dk_bob = build_dk_event(BOB, 7_100, "bob", dk_payload);
    c.engine_a
        .apply(dk_alice.clone())
        .expect("a applies refresh dk_alice");
    c.engine_b
        .apply(dk_alice)
        .expect("b applies refresh dk_alice");
    c.engine_a
        .apply(dk_bob.clone())
        .expect("a applies refresh dk_bob");
    c.engine_b.apply(dk_bob).expect("b applies refresh dk_bob");

    // ── Convergence assertions ───────────────────────────────────────────
    assert!(
        c.engine_a.committee_state.active,
        "engine A remains active across refresh"
    );
    assert!(
        c.engine_b.committee_state.active,
        "engine B remains active across refresh"
    );
    assert_eq!(c.engine_a.committee_state.current_epoch, 2);
    assert_eq!(c.engine_b.committee_state.current_epoch, 2);
    // The point of refresh: joint_verifying_key unchanged.
    assert_eq!(
        c.engine_a.committee_state.joint_verifying_key,
        Some(preserved_vk),
        "refresh MUST preserve joint vk"
    );
    assert_eq!(
        c.engine_b.committee_state.joint_verifying_key,
        Some(preserved_vk),
        "refresh MUST preserve joint vk on engine B too"
    );
    // pending_refresh cleared on both engines.
    assert!(c.engine_a.committee_state.pending_refresh.is_none());
    assert!(c.engine_b.committee_state.pending_refresh.is_none());
}
