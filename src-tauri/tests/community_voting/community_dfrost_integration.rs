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

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use frost_ristretto255::{
    self as frost,
    keys::{KeyPackage, PublicKeyPackage},
    rand_core, Identifier,
};
use harmony_app::community_dfrost_catchup::{
    decode_frame, encode_frame, CatchupBody, CatchupFrame, CatchupStatus, ResetChainLink,
    CATCHUP_VERSION,
};
use harmony_app::community_dfrost_crypto::{
    dkg_part1_local, dkg_part2_local, dkg_part3_local, verifying_key_to_bytes,
    verifying_share_to_bytes,
};
use harmony_app::community_dfrost_log::{
    build_signed_dfrost_event, ApplyError, CommitteeState, DfrostLog, PendingCeremony,
    ResetMarkerApplied,
};
use harmony_app::community_dfrost_log_engine::{
    verify_reset_marker_admissible, CatchupOutcome, DfrostLogEngine, DfrostLogEngineParams,
    DfrostLogRegistry,
};
use harmony_app::community_dfrost_types::{
    derive_vrf_output, DfrostEventKind, DkgCompletePayload, DkgRoundPayload, MemberVerifyingShare,
    RefreshRoundPayload, RepairRoundPayload, ResetMarkerPayload, SignedCommitteeEvent,
    ThresholdSignPayload, VrfBeaconPayload,
};
use harmony_app::community_membership::{
    dfrost_reset_digest, EventId, MaterializedMembership, MemberState, MemberStatus,
    RecipientCiphertext, ResetPhase, ResetProposalView,
};
use harmony_app::community_state_sync::IdentityResolver;
use harmony_app::community_voting_core::{MemberAttrs, MembershipSnapshot};
use harmony_app::community_voting_log::{MembershipSnapshotResolver, SnapshotResolverError};
use harmony_app::dm_signing;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_app::{production_dkg_driver, DfrostCoreHandles, DfrostLogsMap};
use tokio::sync::{mpsc, Mutex};
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

/// A `MemberState` for a hand-built `MaterializedMembership` test
/// fixture — Joined, no left_at, no device-key bookkeeping needed for
/// these tests. ZEB-1031 review I1: `verify_reset_marker_admissible`'s
/// RS-M5 pairs the power/`nm` read with `is_joined_member`, so any
/// fixture actor authoring a marker (or claimed as a pinned successor
/// member checked via that leg) must have an entry here, not just in
/// `power_levels`.
fn joined_member_state() -> MemberState {
    MemberState {
        status: MemberStatus::Joined,
        joined_at: hlc(0, "t"),
        left_at: None,
        enrolled_device_keys: Default::default(),
        revoked_device_keys: Default::default(),
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

/// ZEB-1030 Task 5: RTS repair-request (`rp`) event builder, same shape
/// as the other `build_*_event` helpers — synthetic sig, since the
/// repair-admissibility check below drives it through `apply()`
/// directly (never wire-crossed, so no envelope-verify requirement).
fn build_rp_event(
    actor: OwnerAddr,
    wall_ms: u64,
    node_id: &str,
    payload: RepairRoundPayload,
) -> SignedCommitteeEvent {
    let mut pd = Vec::new();
    ciborium::into_writer(&payload, &mut pd).expect("encode rp payload");
    SignedCommitteeEvent {
        tag: 'd',
        version: 1,
        committee_tier: 0,
        kind: DfrostEventKind::RepairShare,
        hlc: hlc(wall_ms, node_id),
        actor,
        payload: pd,
        sig: vec![0u8; 64],
    }
}

/// Seed `pending_dkg` on a fresh log so the apply path has a ceremony to
/// match against. In production this is populated by the
/// `dfrost_initiate_dkg` IPC (Task 5); for the integration test we set it
/// directly on both engines. Parametrized over the member set and
/// `max_signers` so [`dkg_2of2_setup_for`] can seed a ceremony over
/// identity-derived addresses (ZEB-1030 Task 5 needs the wire-crossing
/// `dk`/`vb` events it builds to carry REAL Ed25519 signatures — see
/// that function's doc) with an optional bystander member widening the
/// committee beyond 2 (see its `bystanders` param). Threshold stays
/// fixed at 2 — nothing in this file needs a different threshold.
fn fresh_log_with_pending_for(members: Vec<OwnerAddr>, max_signers: u16) -> DfrostLog {
    let mut log = DfrostLog::new();
    log.committee_state.pending_dkg = Some(PendingCeremony {
        ceremony_id: CEREMONY_ID,
        members,
        threshold: 2,
        max_signers,
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
    dkg_2of2_setup_for(ALICE, BOB, alice_x25519(), bob_x25519(), Vec::new())
}

/// Parametrized DKG-setup body behind [`dkg_2of2_setup`]. ZEB-1030 Task 5's
/// catch-up wire-crossing tests need this SAME ceremony shape, but over
/// identity-derived member addresses: the later refresh `dk`/`vb` events
/// those tests build have to carry REAL Ed25519 signatures that
/// `verify_signed_committee_event` accepts, which requires
/// `actor.0 == address_hash(signing key)` — a binding the fixed `ALICE`/
/// `BOB` constants (arbitrary bytes, no real keypair behind them) can
/// never satisfy. `identifier_for_index(0/1)` is replaced by an explicit
/// `build_identifier_map` lookup so this stays correct regardless of how
/// the caller's addresses happen to sort.
///
/// `bystanders`: extra committee-member addresses that are declared
/// members (and get a fabricated, non-cryptographic `verifying_shares`
/// entry) but never run the real FROST DKG rounds below — only
/// `alice`/`bob` do. Empty for every existing caller (a real 2-of-2).
/// ZEB-1030 Task 5's straggler test passes one: `check_repair_request_
/// admissible` requires `helpers.len() >= threshold` with helpers drawn
/// from `members ∖ {participant}`, which is structurally unreachable in
/// a strict 2-of-2 (only 1 other member to draw from, below
/// threshold=2 — the same reason the ZEB-1029 restart test above calls
/// full-committee RTS repair "structurally unreachable" for a 2-of-2).
/// A bystander gives that test a real THIRD member to name as a helper
/// without needing a genuine 3-party DKG — `apply()`/`adopt_*` never
/// cross-check a `dk` payload's declared shares against the actual FROST
/// polynomial (see the epoch-1/refresh dk payloads below and
/// `refresh_two_engine_preserves_joint_vk`'s doc comment), so a
/// placeholder share for a member who never signs is a faithful exercise
/// of the membership/repair bookkeeping without touching the real
/// alice/bob crypto at all.
fn dkg_2of2_setup_for(
    alice: OwnerAddr,
    bob: OwnerAddr,
    (alice_priv, alice_pub): ([u8; 32], [u8; 32]),
    (bob_priv, bob_pub): ([u8; 32], [u8; 32]),
    bystanders: Vec<OwnerAddr>,
) -> ActivatedCommittee {
    let mut members = vec![alice, bob];
    members.extend(bystanders.iter().copied());
    let max_signers = members.len() as u16;

    let mut engine_a = fresh_log_with_pending_for(members.clone(), max_signers);
    let mut engine_b = fresh_log_with_pending_for(members.clone(), max_signers);

    let identifier_map = CommitteeState::build_identifier_map(&members);
    let id_alice = identifier_map[&alice];
    let id_bob = identifier_map[&bob];

    // Round 1
    let (r1_secret_alice, r1_pkg_alice_bytes) =
        dkg_part1_local(id_alice, 2, 2).expect("alice part1");
    let (r1_secret_bob, r1_pkg_bob_bytes) = dkg_part1_local(id_bob, 2, 2).expect("bob part1");
    let dr1_alice = build_dr_event(
        alice,
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
        bob,
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
        alice,
        2_000,
        "alice",
        DkgRoundPayload {
            ceremony_id: CEREMONY_ID,
            round_num: 2,
            round1_package: None,
            recipient_ciphertexts: Some(vec![RecipientCiphertext {
                recipient: bob,
                sealed: sealed_alice_to_bob,
            }]),
        },
    );
    engine_a
        .apply_with_identity(dr2_alice.clone(), &alice, &alice_priv)
        .expect("a applies own dr2");
    engine_b
        .apply_with_identity(dr2_alice, &bob, &bob_priv)
        .expect("b decrypts dr2 from alice");

    let r2_pkg_bob_to_alice = r2_pkgs_from_bob
        .get(&id_alice)
        .expect("bob r2 for alice")
        .clone();
    let sealed_bob_to_alice =
        dm_signing::seal_to_owner(&alice_pub, &r2_pkg_bob_to_alice).expect("seal b→a");
    let dr2_bob = build_dr_event(
        bob,
        2_100,
        "bob",
        DkgRoundPayload {
            ceremony_id: CEREMONY_ID,
            round_num: 2,
            round1_package: None,
            recipient_ciphertexts: Some(vec![RecipientCiphertext {
                recipient: alice,
                sealed: sealed_bob_to_alice,
            }]),
        },
    );
    engine_a
        .apply_with_identity(dr2_bob.clone(), &alice, &alice_priv)
        .expect("a decrypts dr2 from bob");
    engine_b
        .apply_with_identity(dr2_bob, &bob, &bob_priv)
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
            pending_a.round2_packages.get(&bob),
            Some(&r2_pkg_bob_to_alice),
            "engine A must have decrypted Bob's sealed r2 package via apply_with_identity"
        );
        // Bob's engine should have decrypted Alice's r2 package for him.
        assert_eq!(
            pending_b.round2_packages.get(&alice),
            Some(&r2_pkg_alice_to_bob),
            "engine B must have decrypted Alice's sealed r2 package via apply_with_identity"
        );
        // Each engine should NOT have stored a decrypt for its own
        // outgoing package (no ciphertext was targeted at self).
        assert!(
            !pending_a.round2_packages.contains_key(&alice),
            "engine A must not have a self-addressed r2 decrypt"
        );
        assert!(
            !pending_b.round2_packages.contains_key(&bob),
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

    // dk broadcast — BOTH real members confirm (threshold=2 requires 2
    // confirmations; a bystander never confirms — `apply_dkg_complete`
    // only requires `dk_confirmations.len() >= threshold`, so 2 of
    // however many members suffices regardless of `bystanders.len()`).
    let mut verifying_shares = Vec::with_capacity(members.len());
    for member in members.clone() {
        let vs_bytes = if member == alice || member == bob {
            let id = identifier_map[&member];
            let vs = pub_pkg
                .verifying_shares()
                .get(&id)
                .expect("verifying share");
            verifying_share_to_bytes(vs)
        } else {
            // Bystander (see `dkg_2of2_setup_for`'s doc): no real FROST
            // key package exists for this identifier — a fixed
            // placeholder is fine, nothing cross-checks it.
            [0xB9; 32]
        };
        verifying_shares.push(MemberVerifyingShare {
            member,
            verifying_share: vs_bytes,
        });
    }
    let dk_payload = DkgCompletePayload {
        ceremony_id: CEREMONY_ID,
        joint_verifying_key: joint_vk,
        verifying_shares,
        epoch: 1,
        members: members.clone(),
        threshold: 2,
        max_signers,
        space_id: None,
    };
    let dk_alice = build_dk_event(alice, 3_000, "alice", dk_payload.clone());
    let dk_bob = build_dk_event(bob, 3_100, "bob", dk_payload);
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
            space_id: None,
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
    let (alice_priv, _alice_pub) = alice_x25519();
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
                attempt: 0,
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
    //    only, targeting bob — refresh part2 produces packages for every
    //    OTHER member, never self, and the apply path enforces exactly
    //    that recipient set since #775 round 2).
    let synthetic_share_for_bob = vec![0xb1; 64];
    let rf2 = build_rf_event(
        ALICE,
        6_100,
        "alice",
        RefreshRoundPayload {
            ceremony_id: refresh_ceremony_id,
            round_num: 2,
            recipient_ciphertexts: Some(vec![RecipientCiphertext {
                recipient: BOB,
                sealed: dm_signing::seal_to_owner(&bob_pub, &synthetic_share_for_bob)
                    .expect("seal bob"),
            }]),
            package: None,
            attempt: 0,
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

    // R4 (CodeRabbit) lineage: the recipient engine must have decrypted
    // the rn=2 ciphertext addressed to it (keyed by sender = ALICE);
    // the sender stores nothing from its own event (no self-addressed
    // ciphertext exists — part2 seals to OTHER members only).
    assert_eq!(
        pr_b.round2_packages.get(&ALICE),
        Some(&synthetic_share_for_bob),
        "engine B must have decrypted refresh share targeted at BOB"
    );
    assert!(
        pr_a.round2_packages.is_empty(),
        "engine A (the sender) must have no self-addressed r2 decrypt"
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
        space_id: None,
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

// ─── ZEB-1029: full-committee restart via sealed share persistence ──────────

/// The ZEB-1029 headline, real crypto end-to-end: BOTH members of a
/// 2-of-2 committee persist (sealed snapshot + sealed share sidecar),
/// go down simultaneously — the case where RTS repair (needs ≥ t live
/// share-holders) and refresh (needs every member's old share) are both
/// structurally unreachable — and come back from DISK ALONE. The
/// restored committee must then complete a fresh threshold-sign → vb
/// cycle with ZERO repair or refresh events.
#[test]
fn full_committee_restart_signs_after_sealed_share_restore_zeb1029() {
    use harmony_app::community_dfrost_persist as persist;
    let c = dkg_2of2_setup();
    let cid = harmony_app::owner_state_types::SpaceId([0x29; 16]);
    let cipher = harmony_app::device_dataset_file::test_cipher();
    let dir_a = tempfile::tempdir().expect("alice identity dir");
    let dir_b = tempfile::tempdir().expect("bob identity dir");

    // ── Persist both nodes exactly as the engine funnel does: ONE sealed
    // image carrying committee state and the signing share atomically ────
    let persist_node = |dir: &std::path::Path, log: &mut DfrostLog, kp| {
        log.local_key_package = Some(kp);
        persist::write_snapshot(
            &cipher,
            &persist::dfrost_path_for(dir, &cid),
            &persist::snapshot_for_persist(log, &cid),
        )
        .expect("write snapshot");
    };
    let mut log_a = c.engine_a;
    let mut log_b = c.engine_b;
    persist_node(dir_a.path(), &mut log_a, c.alice_key_pkg);
    persist_node(dir_b.path(), &mut log_b, c.bob_key_pkg);

    // ── Full-committee restart: every in-memory share dies at once ───────
    drop(log_a);
    drop(log_b);

    let restore_node = |dir: &std::path::Path, self_addr: &OwnerAddr| -> DfrostLog {
        persist::load_dfrost(
            &cipher,
            &persist::dfrost_path_for(dir, &cid),
            &cid,
            Some(self_addr),
        )
        .expect("snapshot loads and embedded share installs")
    };
    let mut restored_a = restore_node(dir_a.path(), &ALICE);
    let mut restored_b = restore_node(dir_b.path(), &BOB);

    for (label, log) in [("alice", &restored_a), ("bob", &restored_b)] {
        assert!(log.committee_state.active, "{label}: committee active");
        assert_eq!(log.committee_state.current_epoch, 1, "{label}: epoch");
        assert_eq!(
            log.committee_state.joint_verifying_key,
            Some(c.joint_vk),
            "{label}: joint vk preserved"
        );
        assert!(
            log.local_key_package.is_some(),
            "{label}: signing share reinstalled from the sealed sidecar"
        );
    }

    // ── Fresh threshold-sign round on the RESTORED shares ────────────────
    let kp_a = restored_a.local_key_package.clone().expect("alice kp");
    let kp_b = restored_b.local_key_package.clone().expect("bob kp");
    let pub_a = restored_a.local_pub_key_package.clone().expect("alice pkp");
    // CR-3 (#777): bob's pub package must have been rebuilt too — signing
    // below only needs kp_b, so without this a restore that installed the
    // scalar but failed the pub-package rebuild would still pass.
    let pub_b = restored_b.local_pub_key_package.clone().expect("bob pkp");
    assert_eq!(
        pub_b.verifying_key(),
        pub_a.verifying_key(),
        "both restored nodes rebuild the same joint verifying key"
    );

    let sign_ceremony_id: [u8; 32] = [0x29; 32];
    let message_hash: [u8; 32] = [0x92; 32];
    let (nonces_a, cm_a) = frost::round1::commit(kp_a.signing_share(), &mut rand_core::OsRng);
    let (nonces_b, cm_b) = frost::round1::commit(kp_b.signing_share(), &mut rand_core::OsRng);
    let mut cm_a_bytes = Vec::new();
    ciborium::into_writer(&cm_a, &mut cm_a_bytes).expect("encode cm");
    let mut cm_b_bytes = Vec::new();
    ciborium::into_writer(&cm_b, &mut cm_b_bytes).expect("encode cm b");
    let mut commitments_map: BTreeMap<Identifier, frost::round1::SigningCommitments> =
        BTreeMap::new();
    commitments_map.insert(c.id_alice, cm_a);
    commitments_map.insert(c.id_bob, cm_b);
    let signing_package = frost::SigningPackage::new(commitments_map, &message_hash);
    let share_a =
        frost::round2::sign(&signing_package, &nonces_a, &kp_a).expect("alice round2 sign");
    let share_b = frost::round2::sign(&signing_package, &nonces_b, &kp_b).expect("bob round2 sign");
    let mut share_a_bytes = Vec::new();
    ciborium::into_writer(&share_a, &mut share_a_bytes).expect("encode share");
    let mut share_b_bytes = Vec::new();
    ciborium::into_writer(&share_b, &mut share_b_bytes).expect("encode share b");
    let mut shares_map: BTreeMap<Identifier, frost::round2::SignatureShare> = BTreeMap::new();
    shares_map.insert(c.id_alice, share_a);
    shares_map.insert(c.id_bob, share_b);
    let group_signature =
        frost::aggregate(&signing_package, &shares_map, &pub_a).expect("aggregate under restored");
    let sig_bytes = group_signature.serialize().expect("serialize sig");

    // The joint vk from BEFORE the restart verifies the post-restart sig.
    pub_a
        .verifying_key()
        .verify(&message_hash, &group_signature)
        .expect("post-restart signature verifies under the original joint vk");

    // ── vb applies on both restored logs (Schnorr-verified) ──────────────
    let mut r_compressed = [0u8; 32];
    r_compressed.copy_from_slice(&sig_bytes[..32]);
    let vrf_output = derive_vrf_output(&r_compressed);
    let ts = build_ts_event(
        ALICE,
        6_000,
        "alice",
        ThresholdSignPayload {
            ceremony_id: sign_ceremony_id,
            message_hash,
            commitment_bytes: cm_a_bytes,
            share_bytes: share_a_bytes,
        },
    );
    restored_a.apply(ts.clone()).expect("a ts");
    restored_b.apply(ts).expect("b ts");
    // CR-4 (#777): apply bob's contribution too, like the sibling
    // threshold-sign test — the log path must accept a SECOND member's
    // `ts` after restore, not just tolerate an out-of-band aggregate.
    let ts_bob = build_ts_event(
        BOB,
        6_100,
        "bob",
        ThresholdSignPayload {
            ceremony_id: sign_ceremony_id,
            message_hash,
            commitment_bytes: cm_b_bytes,
            share_bytes: share_b_bytes,
        },
    );
    restored_a.apply(ts_bob.clone()).expect("a ts bob");
    restored_b.apply(ts_bob).expect("b ts bob");
    for (label, log) in [("alice", &restored_a), ("bob", &restored_b)] {
        let pending = log
            .committee_state
            .pending_sign
            .get(&sign_ceremony_id)
            .expect("pending sign session");
        assert_eq!(
            pending.contributions.len(),
            2,
            "{label}: both restored members' ts contributions recorded"
        );
    }
    let vb = build_vb_event(
        ALICE,
        7_000,
        "alice",
        VrfBeaconPayload {
            ceremony_id: sign_ceremony_id,
            message_hash,
            signature: sig_bytes,
            vrf_output,
        },
    );
    restored_a.apply(vb.clone()).expect("a vb applies");
    restored_b.apply(vb).expect("b vb applies");
    for (label, log) in [("alice", &restored_a), ("bob", &restored_b)] {
        assert_eq!(
            log.beacon_index.get(&message_hash),
            Some(&vrf_output),
            "{label}: post-restart beacon indexed"
        );
    }
}

// ─── ZEB-1030 Task 5: catch-up wire-crossing (straggler + joiner) ──────────
//
// Engine-level wire-crossing tests: every frame `catchup_respond` builds
// is round-tripped through `encode_frame` → bytes → `decode_frame` before
// `catchup_ingest` ever sees it — that byte boundary is the arbiter, not
// a direct in-process handoff (matches the voting plane's test posture;
// Zenoh-session-level coverage is explicitly out of scope here).
//
// Both tests need REAL Ed25519 identities (not this file's `ALICE`/`BOB`
// constants) for the `dk`/`vb` events that cross the wire —
// `verify_signed_committee_event` enforces `identity.address_hash ==
// event.actor.0`, a binding the fixed constants can never satisfy (see
// `dkg_2of2_setup_for`'s doc). `fixture_identity`/`StaticResolver` mirror
// the identical helpers already used in `community_dfrost_transport_
// integration.rs` and `community_dfrost_log_engine.rs`'s own test module.

/// Build `(SigningKey, OwnerAddr, identity_pub_64)` from a seed where
/// `address_hash` binds correctly. Mirrors `community_dfrost_log_engine::
/// tests::fixture_identity`.
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

/// Minimal `IdentityResolver` backed by a `HashMap`, same shape as the
/// engine's own in-file `StaticResolver` test double.
struct StaticResolver(HashMap<OwnerAddr, [u8; 64]>);

#[async_trait::async_trait]
impl IdentityResolver for StaticResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        self.0.get(addr).copied()
    }
}

fn resolver_with(entries: &[(OwnerAddr, [u8; 64])]) -> Arc<dyn IdentityResolver + Send + Sync> {
    Arc::new(StaticResolver(entries.iter().copied().collect()))
}

/// Wire-cross every frame: `encode_frame` → bytes → `decode_frame`. The
/// point of these tests is that THIS byte boundary — not a direct
/// in-process handoff — separates `catchup_respond` from `catchup_ingest`.
fn wire_cross(frames: Vec<CatchupFrame>) -> Vec<CatchupFrame> {
    frames
        .iter()
        .map(|f| decode_frame(&encode_frame(f).expect("encode frame")).expect("decode frame"))
        .collect()
}

/// Flip every bit of a verifying share. Used to make a "rotated" epoch-2
/// share provably different from its epoch-1 value, so a stale
/// `local_key_package` genuinely fails the adopted-consensus match
/// (rather than trivially matching because nothing actually changed).
fn perturbed(vs: [u8; 32]) -> [u8; 32] {
    let mut out = vs;
    for b in out.iter_mut() {
        *b ^= 0xFF;
    }
    out
}

/// ZEB-1030 Task 5, Step 1: bob is a partitioned straggler at epoch 1
/// while alice completes a full proactive refresh to epoch 2. Bob
/// catches up via `catchup_build_request` → `catchup_respond` → (wire-
/// cross) → `catchup_ingest`, must land on alice's epoch/shares, drop
/// his now-stale epoch-1 signing share, and — now that adoption unlocked
/// it — accept a repair request that was inadmissible before. Finally, a
/// real epoch-2 beacon minted on alice reaches bob via a second round.
#[tokio::test]
async fn straggler_catches_up_after_missed_refresh_zeb1030() {
    let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0x51);
    let (bob_sk, bob_addr, bob_pub64) = fixture_identity(0x52);
    // Bystander member — see `dkg_2of2_setup_for`'s doc: repair
    // admissibility needs a real THIRD member (helpers drawn from
    // members ∖ {participant} must reach threshold=2), structurally
    // unreachable in a strict 2-of-2. Never signs anything real.
    let carol_addr = OwnerAddr([0x53; 16]);

    let alice_x = alice_x25519();
    let bob_x = bob_x25519();
    let mut c = dkg_2of2_setup_for(alice_addr, bob_addr, alice_x, bob_x, vec![carol_addr]);

    // Bob installs his real epoch-1 signing share, mirroring a real node
    // (`dkg_2of2_setup`'s ceremony never does this by itself) — needed so
    // the stale-share-drop assertion below genuinely exercises `adopt_
    // refresh_quorum`'s consensus check instead of asserting a value
    // that was already trivially `None`.
    c.engine_b.local_key_package = Some(c.bob_key_pkg.clone());

    // ── Drive the FULL refresh sequence on alice's log ONLY (bob is the
    //    partitioned straggler, still at epoch 1) — same event shape as
    //    `refresh_two_engine_preserves_joint_vk`, above.
    let refresh_ceremony_id: [u8; 32] = [0xf1; 32];
    let preserved_vk = c.joint_vk;
    let (alice_x_priv, _alice_x_pub) = alice_x;
    let (bob_x_priv, bob_x_pub) = bob_x;

    let alice_r1_pkg = vec![0xaa; 32];
    let bob_r1_pkg = vec![0xbb; 32];
    for (actor, pkg, wall, node) in [
        (alice_addr, alice_r1_pkg.clone(), 6_000u64, "alice"),
        (bob_addr, bob_r1_pkg.clone(), 6_050, "bob"),
    ] {
        let rf1 = build_rf_event(
            actor,
            wall,
            node,
            RefreshRoundPayload {
                ceremony_id: refresh_ceremony_id,
                round_num: 1,
                recipient_ciphertexts: None,
                package: Some(pkg),
                attempt: 0,
            },
        );
        c.engine_a
            .apply_with_identity(rf1, &alice_addr, &alice_x_priv)
            .expect("alice applies rf1");
    }

    // rn=2 recipients must cover EVERY other committee member exactly
    // once (`apply_proactive_refresh`'s Qodo #2 check) — with carol as a
    // bystander member, that means bob AND carol, even though carol
    // never actually decrypts anything real in this test.
    let synthetic_share_for_bob = vec![0xb1; 64];
    let synthetic_share_for_carol = vec![0xb2; 64];
    let carol_x_pub = *PublicKey::from(&StaticSecret::from([0x54u8; 32])).as_bytes();
    let rf2 = build_rf_event(
        alice_addr,
        6_100,
        "alice",
        RefreshRoundPayload {
            ceremony_id: refresh_ceremony_id,
            round_num: 2,
            recipient_ciphertexts: Some(vec![
                RecipientCiphertext {
                    recipient: bob_addr,
                    sealed: dm_signing::seal_to_owner(&bob_x_pub, &synthetic_share_for_bob)
                        .expect("seal bob"),
                },
                RecipientCiphertext {
                    recipient: carol_addr,
                    sealed: dm_signing::seal_to_owner(&carol_x_pub, &synthetic_share_for_carol)
                        .expect("seal carol"),
                },
            ]),
            package: None,
            attempt: 0,
        },
    );
    c.engine_a
        .apply_with_identity(rf2, &alice_addr, &alice_x_priv)
        .expect("alice applies rf2");

    // ── dk finalization: REAL Ed25519 signatures (these events cross the
    //    catch-up wire). vk preserved; alice/bob's verifying_shares are
    //    PERTURBED from epoch-1's (XOR 0xFF) — a real refresh rotates
    //    shares, and bob's stale epoch-1 KeyPackage must provably no
    //    longer match the adopted consensus. Carol (bystander) gets a
    //    fixed placeholder — never checked against anything real; see
    //    `dkg_2of2_setup_for`'s doc.
    let members = vec![alice_addr, bob_addr, carol_addr];
    let alice_vs_e1 = verifying_share_to_bytes(
        c.pub_pkg
            .verifying_shares()
            .get(&c.id_alice)
            .expect("alice vs e1"),
    );
    let bob_vs_e1 = verifying_share_to_bytes(
        c.pub_pkg
            .verifying_shares()
            .get(&c.id_bob)
            .expect("bob vs e1"),
    );
    let verifying_shares_e2 = vec![
        MemberVerifyingShare {
            member: alice_addr,
            verifying_share: perturbed(alice_vs_e1),
        },
        MemberVerifyingShare {
            member: bob_addr,
            verifying_share: perturbed(bob_vs_e1),
        },
        MemberVerifyingShare {
            member: carol_addr,
            verifying_share: [0xC0; 32],
        },
    ];
    let dk_payload_e2 = DkgCompletePayload {
        ceremony_id: refresh_ceremony_id,
        joint_verifying_key: preserved_vk,
        verifying_shares: verifying_shares_e2,
        epoch: 2,
        members: members.clone(),
        threshold: 2,
        max_signers: 3,
        space_id: None,
    };
    let dk_alice_e2 = build_signed_dfrost_event(
        &alice_sk,
        alice_addr,
        DfrostEventKind::DkgComplete,
        &dk_payload_e2,
        hlc(7_000, "alice"),
    )
    .expect("sign dk_alice e2");
    let dk_bob_e2 = build_signed_dfrost_event(
        &bob_sk,
        bob_addr,
        DfrostEventKind::DkgComplete,
        &dk_payload_e2,
        hlc(7_100, "bob"),
    )
    .expect("sign dk_bob e2");
    c.engine_a
        .apply(dk_alice_e2)
        .expect("alice applies dk_alice e2");
    c.engine_a
        .apply(dk_bob_e2)
        .expect("alice applies dk_bob e2");
    assert_eq!(
        c.engine_a.committee_state.current_epoch, 2,
        "alice reached epoch 2"
    );

    // ── Wrap both logs; bob stays untouched at epoch 1 ─────────────────
    let alice_log = Arc::new(Mutex::new(c.engine_a));
    let bob_log = Arc::new(Mutex::new(c.engine_b));

    // ── Repair inadmissible BEFORE adoption: bob is still at epoch 1, so
    //    the SAME epoch-2 repair request trips `apply_repair_round`'s
    //    epoch-binding gate before `check_repair_request_admissible` even
    //    runs — proving the failure below is about staleness, not a
    //    malformed request (the identical bytes succeed later, once bob
    //    has actually adopted epoch 2).
    let mut repair_helpers = vec![alice_addr, carol_addr];
    repair_helpers.sort();
    let repair_payload = RepairRoundPayload {
        ceremony_id: [0xd0; 32],
        round_num: 1,
        epoch: 2,
        helpers: Some(repair_helpers),
        minted_wall_ms: Some(9_000),
        minted_logical: Some(0),
        recipient_ciphertexts: None,
    };
    let rp_event = build_rp_event(bob_addr, 9_000, "bob", repair_payload);
    {
        let mut bg = bob_log.lock().await;
        let err = bg
            .apply(rp_event.clone())
            .expect_err("stale-epoch repair request must be rejected");
        assert!(
            matches!(err, ApplyError::InvariantViolation),
            "expected InvariantViolation, got {err:?}"
        );
    }

    // ── Build engines over both logs ────────────────────────────────────
    let community_id = SpaceId([0xF1; 16]);
    let (a_pub_tx, _a_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let alice_engine = DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
        community_id,
        dfrost_log: alice_log.clone(),
        publisher_tx: a_pub_tx,
        subscriber_rx: a_sub_rx,
        app_handle: None,
        self_addr: alice_addr,
        self_x25519_priv: alice_x_priv,
        identity_resolver: resolver_with(&[(alice_addr, alice_pub64), (bob_addr, bob_pub64)]),
        registry_weak: None,
        driver: None,
        membership_resolver: None,
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;
    let (b_pub_tx, _b_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_b_sub_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let bob_engine = DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
        community_id,
        dfrost_log: bob_log.clone(),
        publisher_tx: b_pub_tx,
        subscriber_rx: b_sub_rx,
        app_handle: None,
        self_addr: bob_addr,
        self_x25519_priv: bob_x_priv,
        identity_resolver: resolver_with(&[(alice_addr, alice_pub64), (bob_addr, bob_pub64)]),
        registry_weak: None,
        driver: None,
        membership_resolver: None,
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    // ── Wire-crossing catch-up: bob requests, alice responds, every
    //    frame round-trips encode_frame → bytes → decode_frame before
    //    bob's ingest ever sees it ────────────────────────────────────
    let req = bob_engine.catchup_build_request().await;
    assert_eq!(req.epoch, 1);
    assert!(req.active);
    let frames = alice_engine
        .catchup_respond(req)
        .await
        .expect("alice has a newer epoch to serve");
    let outcome = bob_engine.catchup_ingest(wire_cross(frames)).await;
    assert!(
        matches!(outcome, CatchupOutcome::AdoptedRefresh { epoch: 2, .. }),
        "expected AdoptedRefresh at epoch 2, got {outcome:?}"
    );

    {
        let ag = alice_log.lock().await;
        let bg = bob_log.lock().await;
        assert_eq!(bg.committee_state.current_epoch, 2);
        assert_eq!(bg.committee_state.joint_verifying_key, Some(preserved_vk));
        assert_eq!(
            bg.committee_state.verifying_shares, ag.committee_state.verifying_shares,
            "bob's adopted shares must equal alice's"
        );
        assert!(
            bg.local_key_package.is_none(),
            "bob's stale epoch-1 share must be dropped as provably stale"
        );
        assert!(bg.committee_state.pending_sign.is_empty());
    }

    // ── Repair admissible AFTER adoption: the identical rp bytes now
    //    succeed — adoption is what unlocked it. ───────────────────────
    {
        let mut bg = bob_log.lock().await;
        bg.apply(rp_event)
            .expect("repair request must be admissible once bob is at epoch 2");
        assert!(
            bg.committee_state.pending_repair.is_some(),
            "repair request seeded a pending slot"
        );
    }

    // ── A real epoch-2 beacon minted on alice reaches bob via a second
    //    catch-up round (BeaconsOnly) ───────────────────────────────────
    let sign_ceremony_id: [u8; 32] = [0xf2; 32];
    let message_hash: [u8; 32] = [0xf3; 32];
    let (alice_nonces, alice_commitments) =
        frost::round1::commit(c.alice_key_pkg.signing_share(), &mut rand_core::OsRng);
    let (bob_nonces, bob_commitments) =
        frost::round1::commit(c.bob_key_pkg.signing_share(), &mut rand_core::OsRng);
    let mut alice_cm_bytes = Vec::new();
    ciborium::into_writer(&alice_commitments, &mut alice_cm_bytes).expect("encode alice cm");
    let mut bob_cm_bytes = Vec::new();
    ciborium::into_writer(&bob_commitments, &mut bob_cm_bytes).expect("encode bob cm");
    let mut commitments_map: BTreeMap<Identifier, frost::round1::SigningCommitments> =
        BTreeMap::new();
    commitments_map.insert(c.id_alice, alice_commitments);
    commitments_map.insert(c.id_bob, bob_commitments);
    let signing_package = frost::SigningPackage::new(commitments_map, &message_hash);
    let alice_share = frost::round2::sign(&signing_package, &alice_nonces, &c.alice_key_pkg)
        .expect("alice round2 sign");
    let bob_share = frost::round2::sign(&signing_package, &bob_nonces, &c.bob_key_pkg)
        .expect("bob round2 sign");
    let mut alice_share_bytes = Vec::new();
    ciborium::into_writer(&alice_share, &mut alice_share_bytes).expect("encode alice share");
    let mut bob_share_bytes = Vec::new();
    ciborium::into_writer(&bob_share, &mut bob_share_bytes).expect("encode bob share");
    let mut shares_map: BTreeMap<Identifier, frost::round2::SignatureShare> = BTreeMap::new();
    shares_map.insert(c.id_alice, alice_share);
    shares_map.insert(c.id_bob, bob_share);
    let group_signature = frost::aggregate(&signing_package, &shares_map, &c.pub_pkg)
        .expect("aggregate threshold sig");
    let sig_bytes = group_signature.serialize().expect("serialize sig");
    let mut r_compressed = [0u8; 32];
    r_compressed.copy_from_slice(&sig_bytes[..32]);
    let vrf_output = derive_vrf_output(&r_compressed);

    let ts_alice = build_ts_event(
        alice_addr,
        10_000,
        "alice",
        ThresholdSignPayload {
            ceremony_id: sign_ceremony_id,
            message_hash,
            commitment_bytes: alice_cm_bytes,
            share_bytes: alice_share_bytes,
        },
    );
    let ts_bob = build_ts_event(
        bob_addr,
        10_100,
        "bob",
        ThresholdSignPayload {
            ceremony_id: sign_ceremony_id,
            message_hash,
            commitment_bytes: bob_cm_bytes,
            share_bytes: bob_share_bytes,
        },
    );
    let vb = build_signed_dfrost_event(
        &alice_sk,
        alice_addr,
        DfrostEventKind::VrfBeacon,
        &VrfBeaconPayload {
            ceremony_id: sign_ceremony_id,
            message_hash,
            signature: sig_bytes,
            vrf_output,
        },
        hlc(10_200, "alice"),
    )
    .expect("sign vb");
    {
        let mut ag = alice_log.lock().await;
        ag.apply(ts_alice).expect("alice applies ts_alice");
        ag.apply(ts_bob).expect("alice applies ts_bob");
        ag.apply(vb).expect("alice applies vb");
    }

    let req2 = bob_engine.catchup_build_request().await;
    let frames2 = alice_engine
        .catchup_respond(req2)
        .await
        .expect("alice has a new beacon to serve");
    let outcome2 = bob_engine.catchup_ingest(wire_cross(frames2)).await;
    assert_eq!(outcome2, CatchupOutcome::BeaconsOnly(1));
    {
        let bg = bob_log.lock().await;
        assert_eq!(
            bg.beacon_index.get(&message_hash),
            Some(&vrf_output),
            "bob's beacon_index must contain alice's beacon"
        );
    }
}

/// ZEB-1030 Task 5, Step 2: a fresh, never-seen-the-DKG log/engine adopts
/// alice's committee state wholesale via catch-up, can then self-
/// certify-verify a beacon alice mints afterward, and a fresh SECOND
/// joiner presented with two disagreeing responder groups (alice's real
/// one + a hand-built hostile one claiming a different joint vk) adopts
/// nothing and stays inactive.
#[tokio::test]
async fn fresh_joiner_adopts_committee_state_zeb1030() {
    let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0x61);
    let (bob_sk, bob_addr, bob_pub64) = fixture_identity(0x62);
    let (mallory_sk, mallory_addr, mallory_pub64) = fixture_identity(0x63);

    let alice_x = alice_x25519();
    let bob_x = bob_x25519();
    let mut c = dkg_2of2_setup_for(alice_addr, bob_addr, alice_x, bob_x, Vec::new());

    // Re-mint alice's epoch-1 dk confirmations with REAL Ed25519
    // signatures and seed them as ADDITIONAL retained history —
    // `dkg_2of2_setup_for`'s own dk events use synthetic sigs (`apply()`
    // never verifies them), but `catchup_respond`'s served events DO go
    // through `verify_signed_committee_event`. `insert_event_for_test`
    // only touches retained history, never `committee_state` (already
    // correctly materialized by the real ceremony above) — same pattern
    // as Task 3's `build_dk_quorum_fixture`. A later HLC than the
    // originals (3_000/3_100) makes `select_catchup`'s newest-per-actor
    // collapse pick these instead.
    let verifying_shares: Vec<MemberVerifyingShare> = c
        .engine_a
        .committee_state
        .verifying_shares
        .iter()
        .map(|(member, vs)| MemberVerifyingShare {
            member: *member,
            verifying_share: *vs,
        })
        .collect();
    let dk_payload_e1 = DkgCompletePayload {
        ceremony_id: CEREMONY_ID,
        joint_verifying_key: c.joint_vk,
        verifying_shares,
        epoch: 1,
        members: c.engine_a.committee_state.members.clone(),
        threshold: c.engine_a.committee_state.threshold,
        max_signers: c.engine_a.committee_state.max_signers,
        // ZEB-1034: bind to the community both engines below run as —
        // `adopt_initial_quorum` now requires the match on the joiner.
        space_id: Some(SpaceId([0x7A; 16])),
    };
    let dk_alice_signed = build_signed_dfrost_event(
        &alice_sk,
        alice_addr,
        DfrostEventKind::DkgComplete,
        &dk_payload_e1,
        hlc(3_500, "alice"),
    )
    .expect("sign dk_alice e1");
    let dk_bob_signed = build_signed_dfrost_event(
        &bob_sk,
        bob_addr,
        DfrostEventKind::DkgComplete,
        &dk_payload_e1,
        hlc(3_600, "bob"),
    )
    .expect("sign dk_bob e1");
    c.engine_a.insert_event_for_test(dk_alice_signed);
    c.engine_a.insert_event_for_test(dk_bob_signed);

    let alice_log = Arc::new(Mutex::new(c.engine_a));
    let community_id = SpaceId([0x7A; 16]);
    let (a_pub_tx, _a_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let alice_engine = DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
        community_id,
        dfrost_log: alice_log.clone(),
        publisher_tx: a_pub_tx,
        subscriber_rx: a_sub_rx,
        app_handle: None,
        self_addr: alice_addr,
        self_x25519_priv: alice_x.0,
        identity_resolver: resolver_with(&[(alice_addr, alice_pub64), (bob_addr, bob_pub64)]),
        registry_weak: None,
        driver: None,
        membership_resolver: None,
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    // ── Fresh joiner C: catch-up → AdoptedInitial ───────────────────────
    let c_log = Arc::new(Mutex::new(DfrostLog::new()));
    let (c_pub_tx, _c_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_c_sub_tx, c_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let c_engine = DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
        community_id,
        dfrost_log: c_log.clone(),
        publisher_tx: c_pub_tx,
        subscriber_rx: c_sub_rx,
        app_handle: None,
        self_addr: OwnerAddr([0xC0; 16]),
        self_x25519_priv: [0u8; 32],
        identity_resolver: resolver_with(&[(alice_addr, alice_pub64), (bob_addr, bob_pub64)]),
        registry_weak: None,
        driver: None,
        membership_resolver: None,
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    let req = c_engine.catchup_build_request().await;
    assert_eq!(req.epoch, 0);
    assert!(!req.active);
    let frames = alice_engine
        .catchup_respond(req)
        .await
        .expect("alice serves a fresh joiner");
    let outcome = c_engine.catchup_ingest(wire_cross(frames)).await;
    assert!(
        matches!(outcome, CatchupOutcome::AdoptedInitial { epoch: 1, .. }),
        "expected AdoptedInitial at epoch 1, got {outcome:?}"
    );
    {
        let cg = c_log.lock().await;
        assert!(cg.committee_state.active);
        assert_eq!(cg.committee_state.joint_verifying_key, Some(c.joint_vk));
        assert_eq!(
            cg.committee_state.identifier_map.len(),
            2,
            "identifier_map built for both members"
        );
    }

    // ── C can now VERIFY an alice-minted beacon (self-certifying
    //    adopt_beacons — no in-flight sign session of C's own needed) ──
    let sign_ceremony_id: [u8; 32] = [0x64; 32];
    let message_hash: [u8; 32] = [0x65; 32];
    let (alice_nonces, alice_commitments) =
        frost::round1::commit(c.alice_key_pkg.signing_share(), &mut rand_core::OsRng);
    let (bob_nonces, bob_commitments) =
        frost::round1::commit(c.bob_key_pkg.signing_share(), &mut rand_core::OsRng);
    let mut alice_cm_bytes = Vec::new();
    ciborium::into_writer(&alice_commitments, &mut alice_cm_bytes).expect("encode alice cm");
    let mut bob_cm_bytes = Vec::new();
    ciborium::into_writer(&bob_commitments, &mut bob_cm_bytes).expect("encode bob cm");
    let mut commitments_map: BTreeMap<Identifier, frost::round1::SigningCommitments> =
        BTreeMap::new();
    commitments_map.insert(c.id_alice, alice_commitments);
    commitments_map.insert(c.id_bob, bob_commitments);
    let signing_package = frost::SigningPackage::new(commitments_map, &message_hash);
    let alice_share = frost::round2::sign(&signing_package, &alice_nonces, &c.alice_key_pkg)
        .expect("alice round2 sign");
    let bob_share = frost::round2::sign(&signing_package, &bob_nonces, &c.bob_key_pkg)
        .expect("bob round2 sign");
    let mut alice_share_bytes = Vec::new();
    ciborium::into_writer(&alice_share, &mut alice_share_bytes).expect("encode alice share");
    let mut bob_share_bytes = Vec::new();
    ciborium::into_writer(&bob_share, &mut bob_share_bytes).expect("encode bob share");
    let mut shares_map: BTreeMap<Identifier, frost::round2::SignatureShare> = BTreeMap::new();
    shares_map.insert(c.id_alice, alice_share);
    shares_map.insert(c.id_bob, bob_share);
    let group_signature = frost::aggregate(&signing_package, &shares_map, &c.pub_pkg)
        .expect("aggregate threshold sig");
    let sig_bytes = group_signature.serialize().expect("serialize sig");
    let mut r_compressed = [0u8; 32];
    r_compressed.copy_from_slice(&sig_bytes[..32]);
    let vrf_output = derive_vrf_output(&r_compressed);

    let ts_alice = build_ts_event(
        alice_addr,
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
        bob_addr,
        4_100,
        "bob",
        ThresholdSignPayload {
            ceremony_id: sign_ceremony_id,
            message_hash,
            commitment_bytes: bob_cm_bytes,
            share_bytes: bob_share_bytes,
        },
    );
    let vb = build_signed_dfrost_event(
        &alice_sk,
        alice_addr,
        DfrostEventKind::VrfBeacon,
        &VrfBeaconPayload {
            ceremony_id: sign_ceremony_id,
            message_hash,
            signature: sig_bytes,
            vrf_output,
        },
        hlc(4_200, "alice"),
    )
    .expect("sign vb");
    {
        let mut ag = alice_log.lock().await;
        ag.apply(ts_alice).expect("alice applies ts_alice");
        ag.apply(ts_bob).expect("alice applies ts_bob");
        ag.apply(vb).expect("alice applies vb");
    }

    let req2 = c_engine.catchup_build_request().await;
    let frames2 = alice_engine
        .catchup_respond(req2)
        .await
        .expect("alice has a new beacon to serve");
    let outcome2 = c_engine.catchup_ingest(wire_cross(frames2)).await;
    assert_eq!(outcome2, CatchupOutcome::BeaconsOnly(1));
    {
        let cg = c_log.lock().await;
        assert_eq!(
            cg.beacon_index.get(&message_hash),
            Some(&vrf_output),
            "C verified and indexed alice's beacon"
        );
    }

    // ── Hostile second responder group: a fresh joiner D presented with
    //    alice's real group PLUS a hand-built group (valid signature,
    //    different joint vk) must adopt nothing and stay inactive. ─────
    let hostile_payload = DkgCompletePayload {
        ceremony_id: [0xee; 32],
        joint_verifying_key: [0xee; 32],
        verifying_shares: vec![MemberVerifyingShare {
            member: mallory_addr,
            verifying_share: [0xee; 32],
        }],
        epoch: 1,
        members: vec![mallory_addr],
        threshold: 1,
        max_signers: 1,
        space_id: None,
    };
    let hostile_dk = build_signed_dfrost_event(
        &mallory_sk,
        mallory_addr,
        DfrostEventKind::DkgComplete,
        &hostile_payload,
        hlc(9_000, "mallory"),
    )
    .expect("sign hostile dk");
    let mut hostile_buf = Vec::new();
    ciborium::ser::into_writer(&hostile_dk, &mut hostile_buf).expect("encode hostile dk");
    let hostile_frames = vec![
        CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: [0x99; 8],
            body: CatchupBody::Status(CatchupStatus {
                epoch: 1,
                active: true,
            }),
        },
        CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: [0x99; 8],
            body: CatchupBody::DkEvidence(hostile_buf),
        },
    ];

    let d_log = Arc::new(Mutex::new(DfrostLog::new()));
    let (d_pub_tx, _d_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_d_sub_tx, d_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let d_engine = DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
        community_id,
        dfrost_log: d_log.clone(),
        publisher_tx: d_pub_tx,
        subscriber_rx: d_sub_rx,
        app_handle: None,
        self_addr: OwnerAddr([0xD0; 16]),
        self_x25519_priv: [0u8; 32],
        identity_resolver: resolver_with(&[
            (alice_addr, alice_pub64),
            (bob_addr, bob_pub64),
            (mallory_addr, mallory_pub64),
        ]),
        registry_weak: None,
        driver: None,
        membership_resolver: None,
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    let req_d = d_engine.catchup_build_request().await;
    let mut all_frames = alice_engine
        .catchup_respond(req_d)
        .await
        .expect("alice serves D too");
    all_frames.extend(hostile_frames);
    let outcome_d = d_engine.catchup_ingest(wire_cross(all_frames)).await;
    assert_eq!(outcome_d, CatchupOutcome::Disagreement);
    {
        let dg = d_log.lock().await;
        assert!(!dg.committee_state.active, "D must stay inactive");
    }
}

// ─── Task 5: ZEB-1031 provenance / reset marker / catch-up chain ───────────

/// Test `MembershipSnapshotResolver` backing `verify_reset_marker_
/// admissible` (RS-M3/M4/M5) and the `dfrost_reset_rejected_vks` gate
/// with a hand-built `MaterializedMembership` (a real membership-log
/// event pipeline is Task 1/2's own test responsibility; Task 5's
/// tests exercise what THIS task consumes, given upstream state).
/// `snapshot_at` covers the pre-existing `di`/joiner membership gates
/// too, so the SAME resolver instance is wired to every engine below.
struct TestResetResolver {
    members: Vec<OwnerAddr>,
    membership: Mutex<MaterializedMembership>,
}

impl TestResetResolver {
    fn new(members: Vec<OwnerAddr>, membership: MaterializedMembership) -> Self {
        Self {
            members,
            membership: Mutex::new(membership),
        }
    }
}

#[async_trait::async_trait]
impl MembershipSnapshotResolver for TestResetResolver {
    async fn snapshot_at(
        &self,
        _community_id: SpaceId,
        _hlc: &Hlc,
    ) -> Result<MembershipSnapshot, SnapshotResolverError> {
        let m = self.membership.lock().await;
        let mut members = HashMap::new();
        for addr in &self.members {
            let power = *m.power_levels.get(addr).unwrap_or(&0);
            members.insert(
                *addr,
                MemberAttrs {
                    power,
                    vouching_depth: 0,
                },
            );
        }
        Ok(MembershipSnapshot { members })
    }

    async fn reset_membership_at(
        &self,
        _community_id: SpaceId,
        _hlc: &Hlc,
    ) -> Result<MaterializedMembership, SnapshotResolverError> {
        Ok(self.membership.lock().await.clone())
    }

    async fn reset_membership_now(
        &self,
        _community_id: SpaceId,
    ) -> Result<MaterializedMembership, SnapshotResolverError> {
        Ok(self.membership.lock().await.clone())
    }
}

/// ZEB-1031 Task 5: the full committee-reset lifecycle. An active 2-of-2
/// committee is driven through a membership-authorized reset: a marker
/// deactivates it, a successor DKG promotes a new joint vk, a straggler
/// still holding the pre-reset state heals via the catch-up reset chain
/// (ending active at the new vk with `vk_history.len() == 1`), and a
/// fresh joiner offered ONLY the stale pre-reset quorum while the reset
/// is Authorized is rejected (stale-committee replay, spec §6.1).
#[tokio::test]
async fn committee_reset_marker_successor_dkg_and_straggler_healing_zeb1031() {
    let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0x71);
    let (bob_sk, bob_addr, bob_pub64) = fixture_identity(0x72);

    let alice_x = alice_x25519();
    let bob_x = bob_x25519();
    let mut c = dkg_2of2_setup_for(alice_addr, bob_addr, alice_x, bob_x, Vec::new());
    let old_vk = c.joint_vk;
    let community_id = SpaceId([0x71; 16]);
    // Sorted ascending: adopt_initial_quorum requires it (the reset
    // path's ceremonies never go through dkg_2of2_setup_for's own
    // internal member list, which is unsorted-tolerant since apply()
    // never checks it).
    let mut new_members = vec![alice_addr, bob_addr];
    new_members.sort();
    let new_threshold: u16 = 2;

    // ── Membership view: alice is the power-100 admin; one reset
    //    proposal targets the current committee, Authorized, pinning
    //    the (same-shape, for simplicity) successor committee.
    let reset_id: EventId = [0x80; 16];
    let digest = dfrost_reset_digest(
        &community_id,
        &reset_id,
        &old_vk,
        1,
        &new_members,
        new_threshold,
    )
    .expect("digest");
    let mut membership = MaterializedMembership::default();
    membership.power_levels.insert(alice_addr, 100);
    membership.members.insert(alice_addr, joined_member_state());
    membership.members.insert(bob_addr, joined_member_state());
    membership.reset_proposals.push(ResetProposalView {
        id: reset_id,
        proposer: alice_addr,
        target_vk: old_vk,
        target_epoch: 1,
        new_members: new_members.clone(),
        new_threshold,
        veto_window_ms: 86_400_000,
        signers: [alice_addr].into_iter().collect(),
        proposed_at_wall_ms: 1_000,
        deadline_ms: Some(9_000),
        authorized_at_ms: Some(9_000),
        endorsed: false,
        phase: ResetPhase::Authorized,
        consumed_new_vk: None,
        consumption_superseded: false,
        effective_quorum: None,
    });
    let resolver = Arc::new(TestResetResolver::new(new_members.clone(), membership));

    // ── Marker: alice (power-100) authors it, REAL Ed25519 signature
    //    (it crosses the catch-up wire below).
    let marker_payload = ResetMarkerPayload {
        reset_proposal_id: reset_id,
        reset_digest: digest,
        old_vk,
        old_epoch: 1,
        space_id: community_id,
    };
    let marker_event = build_signed_dfrost_event(
        &alice_sk,
        alice_addr,
        DfrostEventKind::ResetMarker,
        &marker_payload,
        hlc(5_000, "alice"),
    )
    .expect("sign marker");

    // Admissibility (RS-M3/M4/M5) then apply — mirrors the engine's
    // process_inbound routing for `rs` (verify_reset_marker_admissible
    // is the sole entry point `apply_reset_marker` can't reach without
    // it, since the successor pin comes from membership, not the wire).
    let membership_at_marker = resolver.membership.lock().await.clone();
    let (nm, nt) = verify_reset_marker_admissible(
        &marker_payload,
        &alice_addr,
        &community_id,
        &membership_at_marker,
    )
    .expect("marker admissible");
    let applied = c
        .engine_a
        .apply_reset_marker(&marker_event, &community_id, nm, nt)
        .expect("marker applies");
    assert!(
        matches!(applied, ResetMarkerApplied::Applied { old_epoch: 1, .. }),
        "expected Applied at old_epoch 1, got {applied:?}"
    );
    assert!(!c.engine_a.committee_state.active, "committee deactivates");
    assert_eq!(c.engine_a.committee_state.vk_history.len(), 1);
    assert!(c.engine_a.committee_state.pending_reset.is_some());

    // ── Successor DKG (epoch 2): fabricated verifying shares — apply()
    //    never cross-checks a dk payload against real FROST material
    //    (see dkg_2of2_setup_for's doc); only the reset/adoption
    //    bookkeeping matters here.
    let successor_ceremony_id: [u8; 32] = [0x81; 32];
    c.engine_a.committee_state.pending_dkg = Some(PendingCeremony {
        ceremony_id: successor_ceremony_id,
        members: new_members.clone(),
        threshold: new_threshold,
        max_signers: 2,
        proposed_epoch: 2,
        ..Default::default()
    });
    let new_vk = [0x82; 32];
    let successor_dk_payload = DkgCompletePayload {
        ceremony_id: successor_ceremony_id,
        joint_verifying_key: new_vk,
        verifying_shares: vec![
            MemberVerifyingShare {
                member: alice_addr,
                verifying_share: [0x83; 32],
            },
            MemberVerifyingShare {
                member: bob_addr,
                verifying_share: [0x84; 32],
            },
        ],
        epoch: 2,
        members: new_members.clone(),
        threshold: new_threshold,
        max_signers: 2,
        space_id: Some(community_id),
    };
    let dk_alice_e2 = build_signed_dfrost_event(
        &alice_sk,
        alice_addr,
        DfrostEventKind::DkgComplete,
        &successor_dk_payload,
        hlc(6_000, "alice"),
    )
    .expect("sign dk alice e2");
    let dk_bob_e2 = build_signed_dfrost_event(
        &bob_sk,
        bob_addr,
        DfrostEventKind::DkgComplete,
        &successor_dk_payload,
        hlc(6_100, "bob"),
    )
    .expect("sign dk bob e2");
    c.engine_a.apply(dk_alice_e2).expect("alice dk e2 applies");
    c.engine_a.apply(dk_bob_e2).expect("bob dk e2 applies");
    assert!(c.engine_a.committee_state.active, "successor promotes");
    assert_eq!(c.engine_a.committee_state.joint_verifying_key, Some(new_vk));
    assert_eq!(c.engine_a.committee_state.current_epoch, 2);
    assert!(
        c.engine_a.committee_state.pending_reset.is_none(),
        "promotion clears the pin"
    );

    // ── Straggler: engine_b is untouched — still active at epoch 1 /
    //    old_vk. Wrap the promoted responder (engine_a) and the
    //    straggler (engine_b) in real DfrostLogEngines and drive one
    //    catch-up round.
    let responder_log = Arc::new(Mutex::new(c.engine_a));
    let straggler_log = Arc::new(Mutex::new(c.engine_b));
    let identity_resolver = resolver_with(&[(alice_addr, alice_pub64), (bob_addr, bob_pub64)]);

    let (r_pub_tx, _r_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_r_sub_tx, r_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let responder_engine =
        DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
            community_id,
            dfrost_log: responder_log.clone(),
            publisher_tx: r_pub_tx,
            subscriber_rx: r_sub_rx,
            app_handle: None,
            self_addr: alice_addr,
            self_x25519_priv: alice_x.0,
            identity_resolver: identity_resolver.clone(),
            registry_weak: None,
            driver: None,
            membership_resolver: Some(resolver.clone()),
            orchestrator_config: Default::default(),
            persist: None,
        })
        .await;
    let (s_pub_tx, _s_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_s_sub_tx, s_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let straggler_engine =
        DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
            community_id,
            dfrost_log: straggler_log.clone(),
            publisher_tx: s_pub_tx,
            subscriber_rx: s_sub_rx,
            app_handle: None,
            self_addr: bob_addr,
            self_x25519_priv: bob_x.0,
            identity_resolver: identity_resolver.clone(),
            registry_weak: None,
            driver: None,
            membership_resolver: Some(resolver.clone()),
            orchestrator_config: Default::default(),
            persist: None,
        })
        .await;

    let req = straggler_engine.catchup_build_request().await;
    assert_eq!(req.epoch, 1);
    assert!(req.active);
    let frames = responder_engine
        .catchup_respond(req)
        .await
        .expect("responder serves the reset chain");
    let outcome = straggler_engine.catchup_ingest(wire_cross(frames)).await;
    assert!(
        matches!(
            outcome,
            CatchupOutcome::AdoptedResetChain { epoch: 2, links: 1 }
        ),
        "expected AdoptedResetChain{{epoch:2, links:1}}, got {outcome:?}"
    );
    {
        let sg = straggler_log.lock().await;
        assert!(sg.committee_state.active, "straggler ends active");
        assert_eq!(
            sg.committee_state.joint_verifying_key,
            Some(new_vk),
            "straggler adopts the new vk"
        );
        assert_eq!(
            sg.committee_state.vk_history.len(),
            1,
            "straggler recorded exactly one retired committee"
        );
        assert_eq!(sg.committee_state.current_epoch, 2);
    }

    // ── Fresh joiner D offered ONLY the stale pre-reset quorum while
    //    the reset is Authorized → rejected (stale-committee replay,
    //    spec §6.1), never adopted.
    let stale_dk_payload = DkgCompletePayload {
        ceremony_id: CEREMONY_ID,
        joint_verifying_key: old_vk,
        verifying_shares: vec![
            MemberVerifyingShare {
                member: alice_addr,
                verifying_share: [0x01; 32],
            },
            MemberVerifyingShare {
                member: bob_addr,
                verifying_share: [0x02; 32],
            },
        ],
        epoch: 1,
        members: new_members.clone(),
        threshold: 2,
        max_signers: 2,
        space_id: Some(community_id),
    };
    let stale_dk_alice = build_signed_dfrost_event(
        &alice_sk,
        alice_addr,
        DfrostEventKind::DkgComplete,
        &stale_dk_payload,
        hlc(3_500, "alice"),
    )
    .expect("sign stale dk alice");
    let stale_dk_bob = build_signed_dfrost_event(
        &bob_sk,
        bob_addr,
        DfrostEventKind::DkgComplete,
        &stale_dk_payload,
        hlc(3_600, "bob"),
    )
    .expect("sign stale dk bob");
    let stale_responder_id: [u8; 8] = [0x55; 8];
    let mut stale_alice_buf = Vec::new();
    ciborium::ser::into_writer(&stale_dk_alice, &mut stale_alice_buf).expect("encode stale dk a");
    let mut stale_bob_buf = Vec::new();
    ciborium::ser::into_writer(&stale_dk_bob, &mut stale_bob_buf).expect("encode stale dk b");
    let stale_frames = vec![
        CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: stale_responder_id,
            body: CatchupBody::Status(CatchupStatus {
                epoch: 1,
                active: true,
            }),
        },
        CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: stale_responder_id,
            body: CatchupBody::DkEvidence(stale_alice_buf),
        },
        CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: stale_responder_id,
            body: CatchupBody::DkEvidence(stale_bob_buf),
        },
    ];

    let d_log = Arc::new(Mutex::new(DfrostLog::new()));
    let (d_pub_tx, _d_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_d_sub_tx, d_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let d_engine = DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
        community_id,
        dfrost_log: d_log.clone(),
        publisher_tx: d_pub_tx,
        subscriber_rx: d_sub_rx,
        app_handle: None,
        self_addr: OwnerAddr([0xD0; 16]),
        self_x25519_priv: [0u8; 32],
        identity_resolver,
        registry_weak: None,
        driver: None,
        membership_resolver: Some(resolver.clone()),
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    let outcome_d = d_engine.catchup_ingest(wire_cross(stale_frames)).await;
    assert_eq!(
        outcome_d,
        CatchupOutcome::NothingUsable,
        "stale pre-reset quorum must be rejected while the reset is Authorized"
    );
    {
        let dg = d_log.lock().await;
        assert!(!dg.committee_state.active, "D must stay inactive");
    }
}

// ─── Task 5 review round 1: C1 (skew gate), C2 (multi-reset chain) ─────────

/// Poll `log` until `predicate` holds or 2s elapse — the same pattern
/// `community_dfrost_log_engine.rs`'s own test module uses for the
/// receive-loop-driven (background-task) live ingest path.
async fn wait_for_dfrost_log<F>(label: &str, log: &Arc<Mutex<DfrostLog>>, mut predicate: F)
where
    F: FnMut(&DfrostLog) -> bool,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        {
            let guard = log.lock().await;
            if predicate(&guard) {
                return;
            }
        }
        if std::time::Instant::now() >= deadline {
            panic!("wait_for_dfrost_log({label}) timed out after 2s");
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

fn now_wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_millis() as u64
}

/// A single active committee (alice/bob, 2-of-2) plus a matching
/// Authorized reset proposal targeting it — the minimal fixture shared
/// by the C1 skew-gate tests. `marker_wall_ms` lets each test pick a
/// clearly-future or clearly-in-tolerance envelope HLC without hunting
/// for the exact single-ms boundary (real-wall-clock tests need margin
/// against test/engine clock-read drift).
struct SkewFixture {
    community_id: SpaceId,
    resolver: Arc<TestResetResolver>,
    identity_resolver: Arc<dyn IdentityResolver + Send + Sync>,
    alice_addr: OwnerAddr,
    alice_x_priv: [u8; 32],
    bob_sk: ed25519_dalek::SigningKey,
    bob_addr: OwnerAddr,
    old_vk: [u8; 32],
    marker: SignedCommitteeEvent,
    active_log: DfrostLog,
}

fn build_skew_fixture(marker_wall_ms: u64) -> SkewFixture {
    let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0x81);
    let (bob_sk, bob_addr, bob_pub64) = fixture_identity(0x82);
    let alice_x = alice_x25519();
    let bob_x = bob_x25519();
    let c = dkg_2of2_setup_for(alice_addr, bob_addr, alice_x, bob_x, Vec::new());
    let old_vk = c.joint_vk;
    let community_id = SpaceId([0x81; 16]);
    let mut new_members = vec![alice_addr, bob_addr];
    new_members.sort();

    let reset_id: EventId = [0x90; 16];
    let digest =
        dfrost_reset_digest(&community_id, &reset_id, &old_vk, 1, &new_members, 2).expect("digest");
    let mut membership = MaterializedMembership::default();
    membership.power_levels.insert(alice_addr, 100);
    membership.members.insert(alice_addr, joined_member_state());
    membership.members.insert(bob_addr, joined_member_state());
    membership.reset_proposals.push(ResetProposalView {
        id: reset_id,
        proposer: alice_addr,
        target_vk: old_vk,
        target_epoch: 1,
        new_members: new_members.clone(),
        new_threshold: 2,
        veto_window_ms: 86_400_000,
        signers: [alice_addr].into_iter().collect(),
        proposed_at_wall_ms: 1_000,
        deadline_ms: Some(9_000),
        authorized_at_ms: Some(9_000),
        endorsed: false,
        phase: ResetPhase::Authorized,
        consumed_new_vk: None,
        consumption_superseded: false,
        effective_quorum: None,
    });
    let resolver = Arc::new(TestResetResolver::new(new_members, membership));

    let marker_payload = ResetMarkerPayload {
        reset_proposal_id: reset_id,
        reset_digest: digest,
        old_vk,
        old_epoch: 1,
        space_id: community_id,
    };
    let marker = build_signed_dfrost_event(
        &alice_sk,
        alice_addr,
        DfrostEventKind::ResetMarker,
        &marker_payload,
        hlc(marker_wall_ms, "alice"),
    )
    .expect("sign marker");

    SkewFixture {
        community_id,
        resolver,
        identity_resolver: resolver_with(&[(alice_addr, alice_pub64), (bob_addr, bob_pub64)]),
        alice_addr,
        alice_x_priv: alice_x.0,
        bob_sk,
        bob_addr,
        old_vk,
        marker,
        active_log: c.engine_a,
    }
}

/// ZEB-1031 review C1: a marker whose envelope HLC is stamped well
/// past `now + MAX_FORWARD_SKEW_MS` must be dropped by the LIVE
/// ingest path (`process_inbound`'s subscriber-channel route) — never
/// applied, regardless of otherwise-genuine RS-M3/M4/M5 admissibility.
/// Without this gate, a marker author skips the veto window + 48h
/// finality margin entirely (spec §10's named threat).
#[tokio::test]
async fn reset_marker_forward_skewed_rejected_on_live_ingest_zeb1031() {
    let far_future = now_wall_ms() + harmony_app::clock_trust::MAX_FORWARD_SKEW_MS + 60_000;
    let fx = build_skew_fixture(far_future);

    // Seed an UNRELATED pending DKG ceremony so a real, applyable
    // sentinel event exists to prove the receive loop drained past the
    // (dropped, never reaching apply) marker packet — FIFO mpsc, no
    // fixed sleep needed.
    let mut active_log = fx.active_log;
    let sentinel_ceremony_id: [u8; 32] = [0x77; 32];
    active_log.committee_state.pending_dkg = Some(PendingCeremony {
        ceremony_id: sentinel_ceremony_id,
        members: vec![fx.alice_addr, fx.bob_addr],
        threshold: 2,
        max_signers: 2,
        proposed_epoch: 999,
        ..Default::default()
    });

    let log = Arc::new(Mutex::new(active_log));
    let (pub_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let _engine = DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
        community_id: fx.community_id,
        dfrost_log: log.clone(),
        publisher_tx: pub_tx,
        subscriber_rx: sub_rx,
        app_handle: None,
        self_addr: fx.alice_addr,
        self_x25519_priv: fx.alice_x_priv,
        identity_resolver: fx.identity_resolver,
        registry_weak: None,
        driver: None,
        membership_resolver: Some(fx.resolver),
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    let mut packet = Vec::new();
    ciborium::ser::into_writer(&fx.marker, &mut packet).expect("encode marker packet");
    sub_tx.send(packet).await.expect("send marker inbound");

    // Real Ed25519 signature (this event crosses `process_inbound`'s
    // `verify_signed_committee_event` gate on the wire, unlike the
    // synthetic-sig `build_dr_event` helper used by tests that apply
    // directly to a `DfrostLog`).
    let sentinel = build_signed_dfrost_event(
        &fx.bob_sk,
        fx.bob_addr,
        DfrostEventKind::DkgRound,
        &DkgRoundPayload {
            ceremony_id: sentinel_ceremony_id,
            round_num: 1,
            round1_package: Some(vec![0xAA]),
            recipient_ciphertexts: None,
        },
        hlc(50_000, "bob"),
    )
    .expect("sign sentinel");
    let mut sentinel_packet = Vec::new();
    ciborium::ser::into_writer(&sentinel, &mut sentinel_packet).expect("encode sentinel");
    sub_tx
        .send(sentinel_packet)
        .await
        .expect("send sentinel inbound");

    wait_for_dfrost_log("skewed marker sentinel drains", &log, |l| {
        l.committee_state
            .pending_dkg
            .as_ref()
            .is_some_and(|p| p.round1_packages.contains_key(&fx.bob_addr))
    })
    .await;

    let g = log.lock().await;
    assert!(
        g.committee_state.active,
        "forward-skewed marker must not deactivate the committee"
    );
    assert_eq!(g.committee_state.joint_verifying_key, Some(fx.old_vk));
    assert!(
        g.committee_state.vk_history.is_empty(),
        "forward-skewed marker must not be recorded"
    );
}

/// ZEB-1031 review C1: the same skew gate on the catch-up reset-chain
/// APPLY path (`apply_reset_chain`) — a future-stamped marker inside a
/// served `ResetChainLink` must not apply either.
#[tokio::test]
async fn reset_chain_link_forward_skewed_marker_rejected_zeb1031() {
    let far_future = now_wall_ms() + harmony_app::clock_trust::MAX_FORWARD_SKEW_MS + 60_000;
    let fx = build_skew_fixture(far_future);

    let link_bytes = {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(
            &vec![ResetChainLink {
                marker: fx.marker.clone(),
                dk_events: Vec::new(),
            }],
            &mut buf,
        )
        .expect("encode reset chain");
        buf
    };
    let frames = vec![
        CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: [0x11; 8],
            body: CatchupBody::Status(CatchupStatus {
                epoch: 1,
                active: true,
            }),
        },
        CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: [0x11; 8],
            body: CatchupBody::ResetChain(link_bytes),
        },
    ];

    let straggler_log = Arc::new(Mutex::new(fx.active_log));
    let (pub_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let straggler = DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
        community_id: fx.community_id,
        dfrost_log: straggler_log.clone(),
        publisher_tx: pub_tx,
        subscriber_rx: sub_rx,
        app_handle: None,
        self_addr: fx.alice_addr,
        self_x25519_priv: fx.alice_x_priv,
        identity_resolver: fx.identity_resolver,
        registry_weak: None,
        driver: None,
        membership_resolver: Some(fx.resolver),
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    let outcome = straggler.catchup_ingest(wire_cross(frames)).await;
    assert!(
        !matches!(outcome, CatchupOutcome::AdoptedResetChain { .. }),
        "forward-skewed chain-link marker must not apply, got {outcome:?}"
    );
    let g = straggler_log.lock().await;
    assert!(g.committee_state.active, "committee stays untouched");
    assert!(g.committee_state.vk_history.is_empty());
}

/// ZEB-1031 review C1: a marker comfortably WITHIN the skew tolerance
/// (well short of `now + MAX_FORWARD_SKEW_MS`) is admissible and
/// applies normally through the same chain-apply path.
#[tokio::test]
async fn reset_chain_link_in_tolerance_marker_accepted_zeb1031() {
    let near_future = now_wall_ms() + (harmony_app::clock_trust::MAX_FORWARD_SKEW_MS / 2);
    let fx = build_skew_fixture(near_future);

    let link_bytes = {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(
            &vec![ResetChainLink {
                marker: fx.marker.clone(),
                dk_events: Vec::new(),
            }],
            &mut buf,
        )
        .expect("encode reset chain");
        buf
    };
    let frames = vec![
        CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: [0x12; 8],
            body: CatchupBody::Status(CatchupStatus {
                epoch: 1,
                active: true,
            }),
        },
        CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: [0x12; 8],
            body: CatchupBody::ResetChain(link_bytes),
        },
    ];

    let straggler_log = Arc::new(Mutex::new(fx.active_log));
    let (pub_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let straggler = DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
        community_id: fx.community_id,
        dfrost_log: straggler_log.clone(),
        publisher_tx: pub_tx,
        subscriber_rx: sub_rx,
        app_handle: None,
        self_addr: fx.alice_addr,
        self_x25519_priv: fx.alice_x_priv,
        identity_resolver: fx.identity_resolver,
        registry_weak: None,
        driver: None,
        membership_resolver: Some(fx.resolver),
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    let outcome = straggler.catchup_ingest(wire_cross(frames)).await;
    assert!(
        matches!(outcome, CatchupOutcome::AdoptedResetChain { links: 1, .. }),
        "in-tolerance marker must apply, got {outcome:?}"
    );
    let g = straggler_log.lock().await;
    assert!(
        !g.committee_state.active,
        "marker deactivated the committee"
    );
    assert_eq!(g.committee_state.vk_history.len(), 1);
}

// ─── Task 5 review round 1: C2 mandatory multi-reset chain tests ───────────

/// Two committee resets deep: vk1@epoch1 → (reset1) → vk2@epoch2 →
/// (reset2) → vk3@epoch3. `promoted_log` has walked the whole chain
/// (both markers + both successor DKGs, real Ed25519 throughout);
/// `straggler_log` is untouched at vk1/epoch1 — the SAME `dkg_2of2_
/// setup_for` ceremony's other engine, never advanced.
struct TwoResetFixture {
    community_id: SpaceId,
    resolver: Arc<TestResetResolver>,
    identity_resolver: Arc<dyn IdentityResolver + Send + Sync>,
    alice_sk: ed25519_dalek::SigningKey,
    alice_addr: OwnerAddr,
    alice_x_priv: [u8; 32],
    bob_x_priv: [u8; 32],
    bob_addr: OwnerAddr,
    vk3: [u8; 32],
    marker1: SignedCommitteeEvent,
    marker2: SignedCommitteeEvent,
    dk_e2: Vec<SignedCommitteeEvent>,
    straggler_log: DfrostLog,
    promoted_log: DfrostLog,
}

fn build_two_reset_fixture() -> TwoResetFixture {
    let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0x91);
    let (bob_sk, bob_addr, bob_pub64) = fixture_identity(0x92);
    let alice_x = alice_x25519();
    let bob_x = bob_x25519();
    let mut c = dkg_2of2_setup_for(alice_addr, bob_addr, alice_x, bob_x, Vec::new());
    let vk1 = c.joint_vk;
    let community_id = SpaceId([0x91; 16]);
    let mut members = vec![alice_addr, bob_addr];
    members.sort();

    // ── Reset 1: vk1@epoch1 → vk2@epoch2 ────────────────────────────
    let reset1_id: EventId = [0xA1; 16];
    let digest1 =
        dfrost_reset_digest(&community_id, &reset1_id, &vk1, 1, &members, 2).expect("digest1");
    let marker1_payload = ResetMarkerPayload {
        reset_proposal_id: reset1_id,
        reset_digest: digest1,
        old_vk: vk1,
        old_epoch: 1,
        space_id: community_id,
    };
    let marker1 = build_signed_dfrost_event(
        &alice_sk,
        alice_addr,
        DfrostEventKind::ResetMarker,
        &marker1_payload,
        hlc(5_000, "alice"),
    )
    .expect("sign marker1");
    let applied1 = c
        .engine_a
        .apply_reset_marker(&marker1, &community_id, members.clone(), 2)
        .expect("marker1 applies");
    assert!(matches!(applied1, ResetMarkerApplied::Applied { .. }));

    let vk2 = [0xB2; 32];
    let ceremony2: [u8; 32] = [0xC2; 32];
    c.engine_a.committee_state.pending_dkg = Some(PendingCeremony {
        ceremony_id: ceremony2,
        members: members.clone(),
        threshold: 2,
        max_signers: 2,
        proposed_epoch: 2,
        ..Default::default()
    });
    let dk_payload2 = DkgCompletePayload {
        ceremony_id: ceremony2,
        joint_verifying_key: vk2,
        verifying_shares: vec![
            MemberVerifyingShare {
                member: alice_addr,
                verifying_share: [0xB3; 32],
            },
            MemberVerifyingShare {
                member: bob_addr,
                verifying_share: [0xB4; 32],
            },
        ],
        epoch: 2,
        members: members.clone(),
        threshold: 2,
        max_signers: 2,
        space_id: Some(community_id),
    };
    let dk_alice_e2 = build_signed_dfrost_event(
        &alice_sk,
        alice_addr,
        DfrostEventKind::DkgComplete,
        &dk_payload2,
        hlc(6_000, "alice"),
    )
    .expect("sign dk alice e2");
    let dk_bob_e2 = build_signed_dfrost_event(
        &bob_sk,
        bob_addr,
        DfrostEventKind::DkgComplete,
        &dk_payload2,
        hlc(6_100, "bob"),
    )
    .expect("sign dk bob e2");
    c.engine_a
        .apply(dk_alice_e2.clone())
        .expect("apply dk alice e2");
    c.engine_a
        .apply(dk_bob_e2.clone())
        .expect("apply dk bob e2");
    assert!(c.engine_a.committee_state.active);
    assert_eq!(c.engine_a.committee_state.joint_verifying_key, Some(vk2));

    // ── Reset 2: vk2@epoch2 → vk3@epoch3 ────────────────────────────
    let reset2_id: EventId = [0xA2; 16];
    let digest2 =
        dfrost_reset_digest(&community_id, &reset2_id, &vk2, 2, &members, 2).expect("digest2");
    let marker2_payload = ResetMarkerPayload {
        reset_proposal_id: reset2_id,
        reset_digest: digest2,
        old_vk: vk2,
        old_epoch: 2,
        space_id: community_id,
    };
    let marker2 = build_signed_dfrost_event(
        &alice_sk,
        alice_addr,
        DfrostEventKind::ResetMarker,
        &marker2_payload,
        hlc(7_000, "alice"),
    )
    .expect("sign marker2");
    let applied2 = c
        .engine_a
        .apply_reset_marker(&marker2, &community_id, members.clone(), 2)
        .expect("marker2 applies");
    assert!(matches!(applied2, ResetMarkerApplied::Applied { .. }));

    let vk3 = [0xD2; 32];
    let ceremony3: [u8; 32] = [0xE2; 32];
    c.engine_a.committee_state.pending_dkg = Some(PendingCeremony {
        ceremony_id: ceremony3,
        members: members.clone(),
        threshold: 2,
        max_signers: 2,
        proposed_epoch: 3,
        ..Default::default()
    });
    let dk_payload3 = DkgCompletePayload {
        ceremony_id: ceremony3,
        joint_verifying_key: vk3,
        verifying_shares: vec![
            MemberVerifyingShare {
                member: alice_addr,
                verifying_share: [0xD3; 32],
            },
            MemberVerifyingShare {
                member: bob_addr,
                verifying_share: [0xD4; 32],
            },
        ],
        epoch: 3,
        members: members.clone(),
        threshold: 2,
        max_signers: 2,
        space_id: Some(community_id),
    };
    let dk_alice_e3 = build_signed_dfrost_event(
        &alice_sk,
        alice_addr,
        DfrostEventKind::DkgComplete,
        &dk_payload3,
        hlc(8_000, "alice"),
    )
    .expect("sign dk alice e3");
    let dk_bob_e3 = build_signed_dfrost_event(
        &bob_sk,
        bob_addr,
        DfrostEventKind::DkgComplete,
        &dk_payload3,
        hlc(8_100, "bob"),
    )
    .expect("sign dk bob e3");
    c.engine_a
        .apply(dk_alice_e3.clone())
        .expect("apply dk alice e3");
    c.engine_a
        .apply(dk_bob_e3.clone())
        .expect("apply dk bob e3");
    assert!(c.engine_a.committee_state.active);
    assert_eq!(c.engine_a.committee_state.joint_verifying_key, Some(vk3));
    assert_eq!(c.engine_a.committee_state.vk_history.len(), 2);

    let mut membership = MaterializedMembership::default();
    membership.power_levels.insert(alice_addr, 100);
    membership.members.insert(alice_addr, joined_member_state());
    membership.members.insert(bob_addr, joined_member_state());
    membership.reset_proposals.push(ResetProposalView {
        id: reset1_id,
        proposer: alice_addr,
        target_vk: vk1,
        target_epoch: 1,
        new_members: members.clone(),
        new_threshold: 2,
        veto_window_ms: 86_400_000,
        signers: [alice_addr].into_iter().collect(),
        proposed_at_wall_ms: 1_000,
        deadline_ms: Some(9_000),
        authorized_at_ms: Some(9_000),
        endorsed: false,
        phase: ResetPhase::Authorized,
        consumed_new_vk: None,
        consumption_superseded: false,
        effective_quorum: None,
    });
    membership.reset_proposals.push(ResetProposalView {
        id: reset2_id,
        proposer: alice_addr,
        target_vk: vk2,
        target_epoch: 2,
        new_members: members.clone(),
        new_threshold: 2,
        veto_window_ms: 86_400_000,
        signers: [alice_addr].into_iter().collect(),
        proposed_at_wall_ms: 6_500,
        deadline_ms: Some(9_500),
        authorized_at_ms: Some(9_500),
        endorsed: false,
        phase: ResetPhase::Authorized,
        consumed_new_vk: None,
        consumption_superseded: false,
        effective_quorum: None,
    });
    let resolver = Arc::new(TestResetResolver::new(members, membership));

    TwoResetFixture {
        community_id,
        resolver,
        identity_resolver: resolver_with(&[(alice_addr, alice_pub64), (bob_addr, bob_pub64)]),
        alice_sk,
        alice_addr,
        alice_x_priv: alice_x.0,
        bob_x_priv: bob_x.0,
        bob_addr,
        vk3,
        marker1,
        marker2,
        dk_e2: vec![dk_alice_e2, dk_bob_e2],
        straggler_log: c.engine_b,
        promoted_log: c.engine_a,
    }
}

/// ZEB-1031 review C2 (mandatory, spec §11 "straggler across one and
/// two resets"): a straggler stuck at the VERY FIRST pre-reset state
/// walks a two-reset chain end-to-end in ONE catch-up round, ending
/// active at the final vk with `vk_history.len() == 2`.
#[tokio::test]
async fn straggler_heals_two_reset_chain_end_to_end_zeb1031() {
    let fx = build_two_reset_fixture();

    let straggler_log = Arc::new(Mutex::new(fx.straggler_log));
    let promoted_log = Arc::new(Mutex::new(fx.promoted_log));

    let (r_pub_tx, _r_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_r_sub_tx, r_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let responder = DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
        community_id: fx.community_id,
        dfrost_log: promoted_log.clone(),
        publisher_tx: r_pub_tx,
        subscriber_rx: r_sub_rx,
        app_handle: None,
        self_addr: fx.alice_addr,
        self_x25519_priv: fx.alice_x_priv,
        identity_resolver: fx.identity_resolver.clone(),
        registry_weak: None,
        driver: None,
        membership_resolver: Some(fx.resolver.clone()),
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    let (s_pub_tx, _s_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_s_sub_tx, s_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let straggler = DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
        community_id: fx.community_id,
        dfrost_log: straggler_log.clone(),
        publisher_tx: s_pub_tx,
        subscriber_rx: s_sub_rx,
        app_handle: None,
        self_addr: fx.bob_addr,
        self_x25519_priv: fx.bob_x_priv,
        identity_resolver: fx.identity_resolver,
        registry_weak: None,
        driver: None,
        membership_resolver: Some(fx.resolver),
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    let req = straggler.catchup_build_request().await;
    assert_eq!(req.epoch, 1);
    assert!(req.active);
    let frames = responder
        .catchup_respond(req)
        .await
        .expect("responder serves the 2-link chain");
    let outcome = straggler.catchup_ingest(wire_cross(frames)).await;
    assert!(
        matches!(outcome, CatchupOutcome::AdoptedResetChain { links: 2, .. }),
        "expected a 2-link chain adoption in one round, got {outcome:?}"
    );
    let g = straggler_log.lock().await;
    assert!(g.committee_state.active);
    assert_eq!(g.committee_state.joint_verifying_key, Some(fx.vk3));
    assert_eq!(g.committee_state.vk_history.len(), 2);
    assert_eq!(g.committee_state.current_epoch, 3);
}

/// ZEB-1031 review C2 (mandatory): a chain link whose successor quorum
/// stalls (inadmissible shape) must not lose the progress already made
/// — both markers apply (`vk_history.len() == 2`, no truncation), and
/// the node — now `!active` mid-chain — heals the remainder on a LATER
/// round via the SAME reset-chain-first routing (review C2a: reachable
/// regardless of the local active flag).
#[tokio::test]
async fn reset_chain_mid_link_failure_preserves_progress_and_retries_zeb1031() {
    let fx = build_two_reset_fixture();

    let straggler_log = Arc::new(Mutex::new(fx.straggler_log));
    let (pub_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let straggler = DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
        community_id: fx.community_id,
        dfrost_log: straggler_log.clone(),
        publisher_tx: pub_tx,
        subscriber_rx: sub_rx,
        app_handle: None,
        self_addr: fx.bob_addr,
        self_x25519_priv: fx.bob_x_priv,
        identity_resolver: fx.identity_resolver.clone(),
        registry_weak: None,
        driver: None,
        membership_resolver: Some(fx.resolver.clone()),
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    // Round 1: link1 genuine; link2's dk_events CORRUPTED — a shape
    // that does not match the pin2 `adopt_initial_quorum` enforces
    // ([alice] instead of [alice, bob]).
    let corrupt_payload = DkgCompletePayload {
        ceremony_id: [0xFF; 32],
        joint_verifying_key: fx.vk3,
        verifying_shares: vec![MemberVerifyingShare {
            member: fx.alice_addr,
            verifying_share: [0x00; 32],
        }],
        epoch: 3,
        members: vec![fx.alice_addr],
        threshold: 1,
        max_signers: 1,
        space_id: Some(fx.community_id),
    };
    let corrupt_dk = build_signed_dfrost_event(
        &fx.alice_sk,
        fx.alice_addr,
        DfrostEventKind::DkgComplete,
        &corrupt_payload,
        hlc(8_000, "alice"),
    )
    .expect("sign corrupt dk");

    let round1_links = vec![
        ResetChainLink {
            marker: fx.marker1.clone(),
            dk_events: fx.dk_e2.clone(),
        },
        ResetChainLink {
            marker: fx.marker2.clone(),
            dk_events: vec![corrupt_dk],
        },
    ];
    let mut round1_bytes = Vec::new();
    ciborium::ser::into_writer(&round1_links, &mut round1_bytes).expect("encode round1 chain");
    let round1_frames = vec![
        CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: [0x21; 8],
            body: CatchupBody::Status(CatchupStatus {
                epoch: 3,
                active: true,
            }),
        },
        CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: [0x21; 8],
            body: CatchupBody::ResetChain(round1_bytes),
        },
    ];

    let outcome1 = straggler.catchup_ingest(wire_cross(round1_frames)).await;
    assert!(
        matches!(outcome1, CatchupOutcome::AdoptedResetChain { links: 2, .. }),
        "both markers apply even though link2's quorum stalls, got {outcome1:?}"
    );
    {
        let g = straggler_log.lock().await;
        assert!(
            !g.committee_state.active,
            "stuck mid-chain: link2's quorum never adopted"
        );
        assert_eq!(
            g.committee_state.vk_history.len(),
            2,
            "both markers persisted — no truncation of earlier progress"
        );
        assert!(
            g.committee_state.pending_reset.is_some(),
            "pin2 still set — ready to retry"
        );
        assert_eq!(g.committee_state.current_epoch, 2);
    }

    // Round 2: a genuine, fully-promoted responder serves the SAME
    // straggler again — select_reset_chain naturally resumes at entry2
    // only, since the straggler's own request epoch is now oe2 = 2.
    let promoted_log = Arc::new(Mutex::new(fx.promoted_log));
    let (r_pub_tx, _r_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_r_sub_tx, r_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let responder = DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
        community_id: fx.community_id,
        dfrost_log: promoted_log.clone(),
        publisher_tx: r_pub_tx,
        subscriber_rx: r_sub_rx,
        app_handle: None,
        self_addr: fx.alice_addr,
        self_x25519_priv: fx.alice_x_priv,
        identity_resolver: fx.identity_resolver,
        registry_weak: None,
        driver: None,
        membership_resolver: Some(fx.resolver),
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    let req2 = straggler.catchup_build_request().await;
    assert_eq!(req2.epoch, 2, "straggler resumes from its stuck epoch");
    assert!(!req2.active);
    let frames2 = responder
        .catchup_respond(req2)
        .await
        .expect("responder serves the remaining link");
    let outcome2 = straggler.catchup_ingest(wire_cross(frames2)).await;
    // Review NB1: round 2 re-serves marker2 (still <= the straggler's
    // resumed request epoch), which the straggler already applied in
    // round 1 — that re-delivery is RS-M6's `AlreadyMoved` no-op and no
    // longer counts toward `links` (only a genuine `Applied` does). The
    // real progress this round is the quorum adoption, reflected in
    // `epoch` — assert on that and on state, not the link count.
    assert!(
        matches!(
            outcome2,
            CatchupOutcome::AdoptedResetChain { epoch: 3, links: 0 }
        ),
        "round 2 completes the remaining quorum via an AlreadyMoved marker \
         re-delivery + fresh dk adoption, got {outcome2:?}"
    );
    let g = straggler_log.lock().await;
    assert!(g.committee_state.active, "fully healed after round 2");
    assert_eq!(g.committee_state.joint_verifying_key, Some(fx.vk3));
    assert_eq!(
        g.committee_state.vk_history.len(),
        2,
        "still exactly 2 — no duplicate marker recorded"
    );
    assert_eq!(g.committee_state.current_epoch, 3);
}

// ─── Task 5 review round 2 (NB1): AlreadyMoved must not count as progress ──

/// ZEB-1031 review round 2 (NB1): `apply_reset_chain` must NOT report
/// progress — and must NOT short-circuit `catchup_ingest`'s reset-chain-try
/// loop (review C2a) — for a group whose only chain link is an
/// `AlreadyMoved` (RS-M6) re-delivery with an empty `dk_events` list.
/// `select_reset_chain`'s own doc calls this a normal serve (a responder
/// that hasn't itself finished the successor DKG yet), so it happens on
/// every round a straggler talks to a lagging responder. Before the fix,
/// such a group unconditionally returned `Some(AdoptedResetChain{links:1,..})`,
/// and `catchup_ingest`'s hoisted loop returns on the FIRST group that
/// yields `Some` — discarding every OTHER group in the same round,
/// including one carrying the genuine successor dk evidence this straggler
/// actually needs. Ordering decides which group goes first, so a single
/// stale-but-honest responder (or a hostile one, deliberately) could starve
/// healing indefinitely just by sorting ahead of the group with real
/// evidence.
#[tokio::test]
async fn apply_reset_chain_no_progress_falls_through_to_other_groups_dk_evidence_zeb1031() {
    let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0x73);
    let (bob_sk, bob_addr, bob_pub64) = fixture_identity(0x74);
    let alice_x = alice_x25519();
    let bob_x = bob_x25519();
    let mut c = dkg_2of2_setup_for(alice_addr, bob_addr, alice_x, bob_x, Vec::new());
    let old_vk = c.joint_vk;
    let community_id = SpaceId([0x73; 16]);
    let mut new_members = vec![alice_addr, bob_addr];
    new_members.sort();
    let new_threshold: u16 = 2;

    let reset_id: EventId = [0x93; 16];
    let digest = dfrost_reset_digest(
        &community_id,
        &reset_id,
        &old_vk,
        1,
        &new_members,
        new_threshold,
    )
    .expect("digest");
    let mut membership = MaterializedMembership::default();
    membership.power_levels.insert(alice_addr, 100);
    membership.members.insert(alice_addr, joined_member_state());
    membership.members.insert(bob_addr, joined_member_state());
    membership.reset_proposals.push(ResetProposalView {
        id: reset_id,
        proposer: alice_addr,
        target_vk: old_vk,
        target_epoch: 1,
        new_members: new_members.clone(),
        new_threshold,
        veto_window_ms: 86_400_000,
        signers: [alice_addr].into_iter().collect(),
        proposed_at_wall_ms: 1_000,
        deadline_ms: Some(9_000),
        authorized_at_ms: Some(9_000),
        endorsed: false,
        phase: ResetPhase::Authorized,
        consumed_new_vk: None,
        consumption_superseded: false,
        effective_quorum: None,
    });
    let resolver = Arc::new(TestResetResolver::new(new_members.clone(), membership));

    let marker_payload = ResetMarkerPayload {
        reset_proposal_id: reset_id,
        reset_digest: digest,
        old_vk,
        old_epoch: 1,
        space_id: community_id,
    };
    let marker_event = build_signed_dfrost_event(
        &alice_sk,
        alice_addr,
        DfrostEventKind::ResetMarker,
        &marker_payload,
        hlc(5_000, "alice"),
    )
    .expect("sign marker");

    let membership_at_marker = resolver.membership.lock().await.clone();
    let (nm, nt) = verify_reset_marker_admissible(
        &marker_payload,
        &alice_addr,
        &community_id,
        &membership_at_marker,
    )
    .expect("marker admissible");

    // The straggler already applied this marker DIRECTLY — mirrors a
    // node that made it through link 1 on a PRIOR catch-up round. This
    // is what makes the SAME marker's re-delivery below an `AlreadyMoved`
    // no-op rather than a fresh `Applied`.
    let applied_b = c
        .engine_b
        .apply_reset_marker(&marker_event, &community_id, nm, nt)
        .expect("marker applies to straggler");
    assert!(
        matches!(applied_b, ResetMarkerApplied::Applied { .. }),
        "precondition: straggler must have genuinely applied it once already"
    );
    assert!(!c.engine_b.committee_state.active);
    assert!(c.engine_b.committee_state.pending_reset.is_some());

    // Successor DKG (epoch 2) — the genuine evidence group B will carry.
    let successor_ceremony_id: [u8; 32] = [0x94; 32];
    let new_vk = [0x95; 32];
    let successor_dk_payload = DkgCompletePayload {
        ceremony_id: successor_ceremony_id,
        joint_verifying_key: new_vk,
        verifying_shares: vec![
            MemberVerifyingShare {
                member: alice_addr,
                verifying_share: [0x96; 32],
            },
            MemberVerifyingShare {
                member: bob_addr,
                verifying_share: [0x97; 32],
            },
        ],
        epoch: 2,
        members: new_members.clone(),
        threshold: new_threshold,
        max_signers: 2,
        space_id: Some(community_id),
    };
    let dk_alice_e2 = build_signed_dfrost_event(
        &alice_sk,
        alice_addr,
        DfrostEventKind::DkgComplete,
        &successor_dk_payload,
        hlc(6_000, "alice"),
    )
    .expect("sign dk alice e2");
    let dk_bob_e2 = build_signed_dfrost_event(
        &bob_sk,
        bob_addr,
        DfrostEventKind::DkgComplete,
        &successor_dk_payload,
        hlc(6_100, "bob"),
    )
    .expect("sign dk bob e2");

    let straggler_log = Arc::new(Mutex::new(c.engine_b));
    let identity_resolver = resolver_with(&[(alice_addr, alice_pub64), (bob_addr, bob_pub64)]);
    let (s_pub_tx, _s_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_s_sub_tx, s_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let straggler_engine =
        DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
            community_id,
            dfrost_log: straggler_log.clone(),
            publisher_tx: s_pub_tx,
            subscriber_rx: s_sub_rx,
            app_handle: None,
            self_addr: bob_addr,
            self_x25519_priv: bob_x.0,
            identity_resolver,
            registry_weak: None,
            driver: None,
            membership_resolver: Some(resolver.clone()),
            orchestrator_config: Default::default(),
            persist: None,
        })
        .await;

    // Hand-built round, group A ordered FIRST so a starvation bug fires
    // deterministically: group A serves ONLY the already-applied marker
    // with an EMPTY `dk_events` — exactly `select_reset_chain`'s own
    // documented normal case for a responder that hasn't itself finished
    // the successor DKG yet. Group B is a SEPARATE, ordinary (non-chain)
    // responder carrying the genuine successor dk evidence.
    let group_a_link = ResetChainLink {
        marker: marker_event.clone(),
        dk_events: Vec::new(),
    };
    let mut group_a_chain_bytes = Vec::new();
    ciborium::ser::into_writer(&vec![group_a_link], &mut group_a_chain_bytes)
        .expect("encode group A chain");

    let mut dk_alice_buf = Vec::new();
    ciborium::ser::into_writer(&dk_alice_e2, &mut dk_alice_buf).expect("encode dk alice e2");
    let mut dk_bob_buf = Vec::new();
    ciborium::ser::into_writer(&dk_bob_e2, &mut dk_bob_buf).expect("encode dk bob e2");

    let frames = vec![
        CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: [0x31; 8],
            body: CatchupBody::Status(CatchupStatus {
                epoch: 1,
                active: false,
            }),
        },
        CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: [0x31; 8],
            body: CatchupBody::ResetChain(group_a_chain_bytes),
        },
        CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: [0x32; 8],
            body: CatchupBody::Status(CatchupStatus {
                epoch: 2,
                active: true,
            }),
        },
        CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: [0x32; 8],
            body: CatchupBody::DkEvidence(dk_alice_buf),
        },
        CatchupFrame {
            version: CATCHUP_VERSION,
            responder_id: [0x32; 8],
            body: CatchupBody::DkEvidence(dk_bob_buf),
        },
    ];

    let outcome = straggler_engine.catchup_ingest(wire_cross(frames)).await;
    assert!(
        matches!(outcome, CatchupOutcome::AdoptedInitial { epoch: 2, .. }),
        "group A's no-progress AlreadyMoved link must fall through to group B's \
         dk evidence via the ordinary joiner path, got {outcome:?}"
    );
    let g = straggler_log.lock().await;
    assert!(
        g.committee_state.active,
        "straggler heals via group B despite group A yielding nothing"
    );
    assert_eq!(g.committee_state.joint_verifying_key, Some(new_vk));
    assert_eq!(g.committee_state.current_epoch, 2);
}

// ─────────────────────────────────────────────────────────────────────────
// ZEB-1031 Task 7: poll voiding on committee reset + prompted relaunch.
// See docs/superpowers/specs/2026-08-30-zeb1031-dfrost-committee-reset-design.md §7.
//
// Both tests below drive a reset marker through a REAL `DfrostLogEngine`
// (never hand-calling `VotingLogEngine::void_tier3_polls_for_reset`
// directly) so the Weak-hook dispatch wiring
// (`DfrostLogRegistry::dispatch_reset_marker_callbacks` →
// `VotingLogEngine::on_dfrost_reset_marker`) is exercised end-to-end at
// BOTH apply sites: live ingest (`process_inbound`) here, and catch-up
// chain adoption (`apply_reset_chain`) in the second test.
// ─────────────────────────────────────────────────────────────────────────

/// Minimal `DfrostLog` with an ACTIVE 2-member committee at `epoch`, no real
/// DKG run — `verify_reset_marker_admissible` (RS-M3/M4/M5) only checks
/// digest/membership-evidence equality, not FROST zero-knowledge proofs, so
/// an arbitrary `vk` byte pattern is sufficient (mirrors the shape
/// `engine_orchestration_emits_kd_ts_after_kd_cl_se_mode` builds directly
/// for the same reason).
fn active_committee_dfrost_log(members: &[OwnerAddr], vk: [u8; 32], epoch: u64) -> DfrostLog {
    let mut log = DfrostLog::new();
    log.committee_state = CommitteeState {
        active: true,
        current_epoch: epoch,
        joint_verifying_key: Some(vk),
        verifying_shares: BTreeMap::new(),
        members: members.to_vec(),
        threshold: 2,
        max_signers: members.len() as u16,
        identifier_map: CommitteeState::build_identifier_map(members),
        pending_dkg: None,
        pending_sign: BTreeMap::new(),
        pending_refresh: None,
        pending_repair: None,
        vk_history: Vec::new(),
        pending_reset: None,
    };
    log
}

/// Seed a minimal open Tier-3 (se-mode) poll at `community_epoch` directly
/// into `voting_log`, bypassing `publish_event` — same "insert the
/// synthesized PollState" idiom `engine_orchestration_emits_kd_ts_after_kd_cl_se_mode`
/// uses. The void mechanism doesn't touch ballots/crypto, so no committee
/// oracle or ratification state is needed.
fn seed_open_se_poll(
    voting_log: &mut harmony_app::community_voting_log::VotingLog,
    poll_id: harmony_app::community_voting_core::PollId,
    creator: OwnerAddr,
    community_id: SpaceId,
    community_epoch: u64,
) {
    use harmony_app::community_voting_core::{
        Eligibility, Lifecycle, PollMeta, Tier, Tier3PollConfigPayload,
    };
    use harmony_app::community_voting_log::{PollState, TierState};
    use harmony_app::community_voting_tier3::{Tier3PollMeta, Tier3PollState};

    let config = Tier3PollConfigPayload {
        proposal_text: "ZEB-1031 Task 7 void smoke".into(),
        sortition_size: 20,
        deliberation_window_seconds: 10,
        drafting_window_seconds: 10,
        ratification_window_seconds: 10,
        privacy_mode: "se".into(),
        incentive_mode: "d".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: None,
        ce: None,
    };
    let create_hlc = hlc(0, "seed");
    let meta = Tier3PollMeta {
        poll_id,
        proposer: creator,
        poll_create_hlc: create_hlc.clone(),
        config: config.clone(),
        poll_create_event_hash: poll_id.0,
        community_epoch,
    };
    let t3 = Tier3PollState::new_from_create(meta, vec![creator]);
    let poll_state = PollState {
        meta: PollMeta {
            poll_id,
            community_id,
            creator,
            tier: Tier::Sortition,
            eligibility: config.eligibility,
            lifecycle: Lifecycle::Open,
            created_at: create_hlc.clone(),
            opens_at: create_hlc.clone(),
            closes_at: Hlc {
                wall_ms: create_hlc.wall_ms + 30_000,
                logical: 0,
                device_id: create_hlc.device_id.clone(),
            },
            extends_at: None,
            channel_id: None,
            finalized_at_ms: None,
        },
        events: vec![],
        tier_state: TierState::Tier3(Box::new(t3)),
        tier1_cfg: None,
        tier1_snapshot: None,
    };
    voting_log.polls.insert(poll_id, poll_state);
}

/// Poll `voting_log` until `predicate` holds or 2s elapse — mirrors
/// `wait_for_dfrost_log` above for the voting side (the reset-marker
/// callback dispatch runs in a spawned task, so voiding lands
/// asynchronously relative to the inbound packet send).
async fn wait_for_voting_log<F>(
    label: &str,
    voting_log: &Arc<Mutex<harmony_app::community_voting_log::VotingLog>>,
    mut predicate: F,
) where
    F: FnMut(&harmony_app::community_voting_log::VotingLog) -> bool,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        {
            let guard = voting_log.lock().await;
            if predicate(&guard) {
                return;
            }
        }
        if std::time::Instant::now() >= deadline {
            panic!("wait_for_voting_log({label}) timed out after 2s");
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// ZEB-1031 Task 7 (spec §7): a reset marker applied through the dfrost
/// engine's LIVE-INGEST path (`process_inbound`, never hand-calling
/// `void_tier3_polls_for_reset`) voids every open Tier-3 poll whose
/// `community_epoch <= old_epoch`, tagging it with the marker's
/// `(reset_id, old_epoch)`; a poll minted at a later epoch is untouched.
/// A voided poll then rejects every further mutation — including one
/// whose payload wouldn't even decode — with the SAME `PollVoided` error
/// win before payload decode is ever attempted. Re-voiding is a no-op.
#[tokio::test]
async fn live_ingest_reset_marker_voids_open_tier3_polls_zeb1031() {
    use harmony_app::community_voting_core::PollId;
    use harmony_app::community_voting_log::VotingLog;
    use harmony_app::community_voting_log_engine::{
        BeaconRequester, VotingLogEngine, VotingLogEngineParams,
    };

    let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0xB1);
    let (_bob_sk, bob_addr, bob_pub64) = fixture_identity(0xB2);
    let mut members = vec![alice_addr, bob_addr];
    members.sort();
    let community_id = SpaceId([0xB1; 16]);
    let vk1: [u8; 32] = [0xC1; 32];

    // ── Dfrost side: an active epoch-1 committee, wired for live ingest ──
    let dfrost_log = Arc::new(Mutex::new(active_committee_dfrost_log(&members, vk1, 1)));

    let reset1_id: EventId = [0xD1; 16];
    let digest1 =
        dfrost_reset_digest(&community_id, &reset1_id, &vk1, 1, &members, 2).expect("digest1");
    let marker1_payload = ResetMarkerPayload {
        reset_proposal_id: reset1_id,
        reset_digest: digest1,
        old_vk: vk1,
        old_epoch: 1,
        space_id: community_id,
    };
    let marker1 = build_signed_dfrost_event(
        &alice_sk,
        alice_addr,
        DfrostEventKind::ResetMarker,
        &marker1_payload,
        hlc(5_000, "alice"),
    )
    .expect("sign marker1");

    let mut membership = MaterializedMembership::default();
    membership.power_levels.insert(alice_addr, 100);
    membership.members.insert(alice_addr, joined_member_state());
    membership.members.insert(bob_addr, joined_member_state());
    membership.reset_proposals.push(ResetProposalView {
        id: reset1_id,
        proposer: alice_addr,
        target_vk: vk1,
        target_epoch: 1,
        new_members: members.clone(),
        new_threshold: 2,
        veto_window_ms: 86_400_000,
        signers: [alice_addr].into_iter().collect(),
        proposed_at_wall_ms: 1_000,
        deadline_ms: Some(9_000),
        authorized_at_ms: Some(9_000),
        endorsed: false,
        phase: ResetPhase::Authorized,
        consumed_new_vk: None,
        consumption_superseded: false,
        effective_quorum: None,
    });
    let resolver = Arc::new(TestResetResolver::new(members.clone(), membership));
    let identity_resolver = resolver_with(&[(alice_addr, alice_pub64), (bob_addr, bob_pub64)]);

    let dfrost_reg = Arc::new(DfrostLogRegistry::<tauri::test::MockRuntime>::new());
    // Keep the subscriber sender so the test can feed `marker1` in as a
    // live inbound packet — mirrors a peer node's Zenoh delivery.
    let (dtx, drx) = mpsc::channel::<Vec<u8>>(4);
    let (_dpub_tx, _dpub_rx) = mpsc::channel::<Vec<u8>>(4);
    DfrostLogRegistry::register(
        &dfrost_reg,
        DfrostLogEngineParams {
            community_id,
            dfrost_log: dfrost_log.clone(),
            publisher_tx: _dpub_tx,
            subscriber_rx: drx,
            app_handle: None,
            self_addr: bob_addr,
            self_x25519_priv: [0u8; 32],
            identity_resolver: identity_resolver.clone(),
            registry_weak: None,
            driver: None,
            membership_resolver: Some(resolver.clone()),
            orchestrator_config: Default::default(),
            persist: None,
        },
    )
    .await;

    // ── Voting side: seed one poll at epoch 1 (will be voided) and one
    //    at epoch 2 (created post-reset — must stay untouched). ──
    let voting_log = Arc::new(Mutex::new(VotingLog::new()));
    let poll1_id = PollId([0x01; 32]);
    let poll2_id = PollId([0x02; 32]);
    {
        let mut log = voting_log.lock().await;
        seed_open_se_poll(&mut log, poll1_id, alice_addr, community_id, 1);
        seed_open_se_poll(&mut log, poll2_id, alice_addr, community_id, 2);
    }

    let (v_pub_tx, _v_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_v_sub_tx, v_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let device_id = "dev-zeb1031-t7-live".to_string();
    let hlc_tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        device_id.clone(),
    )));
    let voting_engine = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id,
        voting_log: Arc::clone(&voting_log),
        publisher_tx: v_pub_tx,
        subscriber_rx: v_sub_rx,
        hlc_tracker: Some(hlc_tracker),
        device_id: Some(device_id),
        app_handle: None,
        identity_resolver: None,
        membership_resolver: None,
    })
    .await;
    let requester: BeaconRequester =
        Arc::new(move |_cid, _seed, _epoch| Box::pin(async { Ok("noop".to_string()) }));
    VotingLogEngine::install_dfrost_handle(&voting_engine, dfrost_reg.clone(), requester).await;

    // ── Feed marker1 in as a live inbound packet (never hand-calling
    //    void_tier3_polls_for_reset) ──
    let mut packet = Vec::new();
    ciborium::into_writer(&marker1, &mut packet).expect("encode marker1");
    dtx.send(packet)
        .await
        .expect("dfrost subscriber channel open");

    // The reset-marker callback dispatch spawns the void handler
    // asynchronously — poll until it lands.
    wait_for_voting_log("poll1 voided", &voting_log, |log| {
        log.polls
            .get(&poll1_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .and_then(|t3| t3.voided)
            .is_some()
    })
    .await;

    {
        let log = voting_log.lock().await;
        let t3_1 = log.polls[&poll1_id].tier_state.as_tier3().unwrap();
        let voided = t3_1.voided.expect("poll1 voided");
        assert_eq!(
            voided.reset_id, reset1_id,
            "voided.reset_id must echo the marker's ri"
        );
        assert_eq!(
            voided.old_epoch, 1,
            "voided.old_epoch must echo the marker's oe"
        );

        let t3_2 = log.polls[&poll2_id].tier_state.as_tier3().unwrap();
        assert!(
            t3_2.voided.is_none(),
            "poll at epoch 2 (post-reset) must stay untouched by a marker retiring epoch 1"
        );
    }

    // ── Mutation on the voided poll is rejected — with PollVoided, not a
    //    payload-decode error, confirming the check runs before decode. ──
    {
        use harmony_app::community_voting_core::{PollEventKindCode, SignedVotingEvent, Tier};
        let mut log = voting_log.lock().await;
        let t3 = log
            .polls
            .get_mut(&poll1_id)
            .unwrap()
            .tier_state
            .as_tier3_mut()
            .unwrap();
        let garbage_ballot = SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Sortition,
            kind: PollEventKindCode::RatificationBallot,
            hlc: hlc(6_000, "someone"),
            actor: bob_addr,
            payload: vec![], // deliberately undecodable — voided must win first
            sig: vec![0u8; 64],
        };
        let err = t3
            .apply_event(&garbage_ballot)
            .expect_err("ballot on a voided poll must be rejected");
        assert!(
            matches!(
                err,
                harmony_app::community_voting_tier3::ApplyError::PollVoided
            ),
            "expected PollVoided, got {err:?}"
        );
    }

    // ── Re-voiding is idempotent: 0 additional. ──
    let revoided = voting_engine.void_tier3_polls_for_reset(1, reset1_id).await;
    assert_eq!(
        revoided, 0,
        "re-running void for the same reset must be a no-op"
    );
}

/// ZEB-1031 Task 8 review I1: the reset-marker callback dispatch must ALSO
/// fire on the LOCAL-AUTHOR path (`dfrost_author_reset_marker_core`, via
/// the manual `author_dfrost_reset_marker` IPC or the orchestrator's
/// `maybe_auto_drive_reset`) — not only on inbound ingest / catch-up
/// adoption. Before the I1 fix, the authoring node's own open Tier-3 polls
/// stayed open (never voided) and its `retired_epoch_watermark` never
/// advanced, because `publish_event` records into the replay tracker
/// BEFORE sending specifically so self-loopback is dropped at
/// `process_inbound`'s dedup step — the author never re-ingests its own
/// marker, so the ingest-side dispatch alone can never reach it.
///
/// Same fixture shape as `live_ingest_reset_marker_voids_open_tier3_polls_zeb1031`
/// (one poll at epoch 1 — must void; one at epoch 2 — must stay untouched),
/// but instead of feeding `marker1` in as an inbound packet, this authors
/// it directly through the production `DkgDriver::author_reset_marker`
/// path (`production_dkg_driver` over `DfrostCoreHandles`, sharing the SAME
/// `dfrost_log`/`dfrost_reg` the voting engine's callback is subscribed to)
/// — no second node, no inbound packet, involved.
#[tokio::test]
async fn local_author_reset_marker_voids_open_tier3_polls_zeb1031() {
    use harmony_app::community_voting_core::PollId;
    use harmony_app::community_voting_log::VotingLog;
    use harmony_app::community_voting_log_engine::{
        BeaconRequester, VotingLogEngine, VotingLogEngineParams,
    };

    let (alice_sk, alice_addr, _alice_pub64) = fixture_identity(0xB3);
    let (_bob_sk, bob_addr, _bob_pub64) = fixture_identity(0xB4);
    let mut members = vec![alice_addr, bob_addr];
    members.sort();
    let community_id = SpaceId([0xB3; 16]);
    let vk1: [u8; 32] = [0xC3; 32];

    // ── Dfrost side: an active epoch-1 committee. Alice (the authoring
    //    node) holds this same log. ──
    let dfrost_log = Arc::new(Mutex::new(active_committee_dfrost_log(&members, vk1, 1)));

    let reset1_id: EventId = [0xD3; 16];
    let digest1 =
        dfrost_reset_digest(&community_id, &reset1_id, &vk1, 1, &members, 2).expect("digest1");

    let dfrost_reg = Arc::new(DfrostLogRegistry::<tauri::test::MockRuntime>::new());
    let (dpub_tx, mut dpub_rx) = mpsc::channel::<Vec<u8>>(4);
    tokio::spawn(async move { while dpub_rx.recv().await.is_some() {} });
    let (_dsub_tx, dsub_rx) = mpsc::channel::<Vec<u8>>(4);
    DfrostLogRegistry::register(
        &dfrost_reg,
        DfrostLogEngineParams {
            community_id,
            dfrost_log: dfrost_log.clone(),
            publisher_tx: dpub_tx,
            subscriber_rx: dsub_rx,
            app_handle: None,
            self_addr: alice_addr,
            self_x25519_priv: [0u8; 32],
            identity_resolver: resolver_with(&[]),
            registry_weak: None,
            driver: None,
            membership_resolver: None,
            orchestrator_config: Default::default(),
            persist: None,
        },
    )
    .await;

    // ── Voting side: seed one poll at epoch 1 (will be voided) and one
    //    at epoch 2 (created post-reset — must stay untouched). ──
    let voting_log = Arc::new(Mutex::new(VotingLog::new()));
    let poll1_id = PollId([0x03; 32]);
    let poll2_id = PollId([0x04; 32]);
    {
        let mut log = voting_log.lock().await;
        seed_open_se_poll(&mut log, poll1_id, alice_addr, community_id, 1);
        seed_open_se_poll(&mut log, poll2_id, alice_addr, community_id, 2);
    }

    let (v_pub_tx, mut v_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    tokio::spawn(async move { while v_pub_rx.recv().await.is_some() {} });
    let (_v_sub_tx, v_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let device_id = "dev-zeb1031-t8-author".to_string();
    let hlc_tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        device_id.clone(),
    )));
    let voting_engine = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id,
        voting_log: Arc::clone(&voting_log),
        publisher_tx: v_pub_tx,
        subscriber_rx: v_sub_rx,
        hlc_tracker: Some(hlc_tracker),
        device_id: Some(device_id),
        app_handle: None,
        identity_resolver: None,
        membership_resolver: None,
    })
    .await;
    let requester: BeaconRequester =
        Arc::new(move |_cid, _seed, _epoch| Box::pin(async { Ok("noop".to_string()) }));
    VotingLogEngine::install_dfrost_handle(&voting_engine, dfrost_reg.clone(), requester).await;

    // ── Author marker1 LOCALLY via the production driver — the exact
    //    path `maybe_auto_drive_reset` / `author_dfrost_reset_marker`
    //    take, never touching the inbound/catch-up ingest paths. ──
    let mut dfrost_logs_map = HashMap::new();
    dfrost_logs_map.insert(community_id, dfrost_log.clone());
    let handles = DfrostCoreHandles::<tauri::test::MockRuntime>::for_tests(
        Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            "dev-zeb1031-t8-author".to_string(),
        ))),
        harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        "dev-zeb1031-t8-author".to_string(),
        alice_addr,
        Arc::new(alice_sk),
        Arc::new(Mutex::new(dfrost_logs_map)) as DfrostLogsMap,
        None,
        Some(dfrost_reg.clone()),
    );
    let driver = production_dkg_driver::<tauri::test::MockRuntime>(handles, None);
    driver
        .author_reset_marker(community_id, reset1_id, digest1, vk1, 1, members.clone(), 2)
        .await
        .expect("author_reset_marker succeeds");

    // The reset-marker callback dispatch spawns the void handler
    // asynchronously — poll until it lands, exactly like the ingest-path
    // sibling test above.
    wait_for_voting_log("poll1 voided (authoring node)", &voting_log, |log| {
        log.polls
            .get(&poll1_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .and_then(|t3| t3.voided)
            .is_some()
    })
    .await;

    let log = voting_log.lock().await;
    let t3_1 = log.polls[&poll1_id].tier_state.as_tier3().unwrap();
    let voided = t3_1.voided.expect("poll1 voided");
    assert_eq!(
        voided.reset_id, reset1_id,
        "voided.reset_id must echo the marker's ri"
    );
    assert_eq!(
        voided.old_epoch, 1,
        "voided.old_epoch must echo the marker's oe"
    );

    let t3_2 = log.polls[&poll2_id].tier_state.as_tier3().unwrap();
    assert!(
        t3_2.voided.is_none(),
        "poll at epoch 2 (post-reset) must stay untouched by a marker retiring epoch 1"
    );
}

/// ZEB-1031 Task 7 (spec §7): the SAME void hook fires from the catch-up
/// CHAIN-ADOPTION apply site (`apply_reset_chain`) — a straggler healing
/// through a multi-reset chain voids its stale Tier-3 polls exactly like a
/// live node. Builds on Task 5's `TwoResetFixture` (already a proven,
/// working two-reset chain) rather than re-deriving one.
#[tokio::test]
async fn chain_adoption_reset_marker_voids_open_tier3_polls_zeb1031() {
    use harmony_app::community_voting_core::PollId;
    use harmony_app::community_voting_log::VotingLog;
    use harmony_app::community_voting_log_engine::{
        BeaconRequester, VotingLogEngine, VotingLogEngineParams,
    };

    let fx = build_two_reset_fixture();

    let straggler_log = Arc::new(Mutex::new(fx.straggler_log));
    let promoted_log = Arc::new(Mutex::new(fx.promoted_log));

    // Responder side (already-promoted, holds the 2-link chain).
    let (r_pub_tx, _r_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_r_sub_tx, r_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let responder = DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
        community_id: fx.community_id,
        dfrost_log: promoted_log.clone(),
        publisher_tx: r_pub_tx,
        subscriber_rx: r_sub_rx,
        app_handle: None,
        self_addr: fx.alice_addr,
        self_x25519_priv: fx.alice_x_priv,
        identity_resolver: fx.identity_resolver.clone(),
        registry_weak: None,
        driver: None,
        membership_resolver: Some(fx.resolver.clone()),
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    // Straggler side — registered in a REAL DfrostLogRegistry so
    // `apply_reset_chain`'s own `registry_weak` (not a fn param, unlike
    // the live-ingest path) can dispatch the void callback.
    let straggler_reg = Arc::new(DfrostLogRegistry::<tauri::test::MockRuntime>::new());
    let (s_pub_tx, _s_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_s_sub_tx, s_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let straggler = DfrostLogRegistry::register(
        &straggler_reg,
        DfrostLogEngineParams {
            community_id: fx.community_id,
            dfrost_log: straggler_log.clone(),
            publisher_tx: s_pub_tx,
            subscriber_rx: s_sub_rx,
            app_handle: None,
            self_addr: fx.bob_addr,
            self_x25519_priv: fx.bob_x_priv,
            identity_resolver: fx.identity_resolver,
            registry_weak: None,
            driver: None,
            membership_resolver: Some(fx.resolver),
            orchestrator_config: Default::default(),
            persist: None,
        },
    )
    .await;

    // Voting engine on the straggler's registry: one poll at epoch 1
    // (pre-BOTH resets — must void on marker1) and one at epoch 3
    // (the chain's final epoch, i.e. created post-reset — untouched).
    let voting_log = Arc::new(Mutex::new(VotingLog::new()));
    let poll1_id = PollId([0x11; 32]);
    let poll3_id = PollId([0x33; 32]);
    {
        let mut log = voting_log.lock().await;
        seed_open_se_poll(&mut log, poll1_id, fx.alice_addr, fx.community_id, 1);
        seed_open_se_poll(&mut log, poll3_id, fx.alice_addr, fx.community_id, 3);
    }
    let (v_pub_tx, _v_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_v_sub_tx, v_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let device_id = "dev-zeb1031-t7-chain".to_string();
    let hlc_tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        device_id.clone(),
    )));
    let voting_engine = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id: fx.community_id,
        voting_log: Arc::clone(&voting_log),
        publisher_tx: v_pub_tx,
        subscriber_rx: v_sub_rx,
        hlc_tracker: Some(hlc_tracker),
        device_id: Some(device_id),
        app_handle: None,
        identity_resolver: None,
        membership_resolver: None,
    })
    .await;
    let requester: BeaconRequester =
        Arc::new(move |_cid, _seed, _epoch| Box::pin(async { Ok("noop".to_string()) }));
    VotingLogEngine::install_dfrost_handle(&voting_engine, straggler_reg.clone(), requester).await;

    // Drive the SAME catch-up round the Task 5 test does.
    let req = straggler.catchup_build_request().await;
    let frames = responder
        .catchup_respond(req)
        .await
        .expect("responder serves the 2-link chain");
    let outcome = straggler.catchup_ingest(wire_cross(frames)).await;
    assert!(
        matches!(outcome, CatchupOutcome::AdoptedResetChain { links: 2, .. }),
        "expected a 2-link chain adoption, got {outcome:?}"
    );

    wait_for_voting_log("poll1 voided via chain adoption", &voting_log, |log| {
        log.polls
            .get(&poll1_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .and_then(|t3| t3.voided)
            .is_some()
    })
    .await;

    let log = voting_log.lock().await;
    let t3_1 = log.polls[&poll1_id].tier_state.as_tier3().unwrap();
    assert_eq!(t3_1.voided.expect("poll1 voided").old_epoch, 1);
    let t3_3 = log.polls[&poll3_id].tier_state.as_tier3().unwrap();
    assert!(
        t3_3.voided.is_none(),
        "poll at the chain's final epoch (3) must stay untouched"
    );
}

/// ZEB-1031 Task 7 review C1 (MANDATORY regression): drives the void sweep
/// against a poll materialized through the REAL peer path — never
/// hand-seeded, never dual-applied. Author (alice) mints a Tier-3
/// PollCreate on engine A at community epoch 1; peer (bob) engine B
/// materializes it via `process_inbound` deriving `community_epoch` from
/// the wire-carried `cfg.ce` (NOT a local `set_tier3_poll_epoch` patch,
/// which never runs for peer-ingested creates). A reset marker for
/// `old_epoch = 1` applied on B's dfrost side via LIVE INGEST then voids
/// B's peer-materialized copy of the poll — proving `community_epoch` is
/// no longer a creator-local cache that only the author ever sees
/// correctly. A second, post-reset poll at epoch 2 (same author→peer
/// path) survives B's sweep untouched.
#[tokio::test]
async fn peer_materialized_poll_voids_via_wire_carried_epoch_zeb1031() {
    use harmony_app::community_voting_core::{
        build_signed_poll_create_tier3, derive_poll_id, Eligibility, MemberAttrs,
        Tier3PollConfigPayload, VotingIdentityResolver,
    };
    use harmony_app::community_voting_log::VotingLog;
    use harmony_app::community_voting_log_engine::{
        BeaconRequester, VotingLogEngine, VotingLogEngineParams,
    };

    let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0xF1);
    let (_bob_sk, bob_addr, bob_pub64) = fixture_identity(0xF2);
    let mut members = vec![alice_addr, bob_addr];
    members.sort();
    let community_id = SpaceId([0xF1; 16]);
    let vk1: [u8; 32] = [0xA9; 32];

    // ── Membership evidence for B's reset-marker admissibility (RS-M3/4/5) ──
    let reset1_id: EventId = [0xB9; 16];
    let digest1 =
        dfrost_reset_digest(&community_id, &reset1_id, &vk1, 1, &members, 2).expect("digest1");
    let mut membership = MaterializedMembership::default();
    membership.power_levels.insert(alice_addr, 100);
    membership.members.insert(alice_addr, joined_member_state());
    membership.members.insert(bob_addr, joined_member_state());
    membership.reset_proposals.push(ResetProposalView {
        id: reset1_id,
        proposer: alice_addr,
        target_vk: vk1,
        target_epoch: 1,
        new_members: members.clone(),
        new_threshold: 2,
        veto_window_ms: 86_400_000,
        signers: [alice_addr].into_iter().collect(),
        proposed_at_wall_ms: 1_000,
        deadline_ms: Some(9_000),
        authorized_at_ms: Some(9_000),
        endorsed: false,
        phase: ResetPhase::Authorized,
        consumed_new_vk: None,
        consumption_superseded: false,
        effective_quorum: None,
    });
    let identity_resolver = resolver_with(&[(alice_addr, alice_pub64), (bob_addr, bob_pub64)]);

    // ── A's dfrost side (epoch 1) — so A can embed `ce` before signing ──
    let dfrost_log_a = Arc::new(Mutex::new(active_committee_dfrost_log(&members, vk1, 1)));
    let dfrost_reg_a = Arc::new(DfrostLogRegistry::<tauri::test::MockRuntime>::new());
    {
        let (a_dpub_tx, _a_dpub_rx) = mpsc::channel::<Vec<u8>>(4);
        let (_a_dsub_tx, a_dsub_rx) = mpsc::channel::<Vec<u8>>(4);
        DfrostLogRegistry::register(
            &dfrost_reg_a,
            DfrostLogEngineParams {
                community_id,
                dfrost_log: dfrost_log_a.clone(),
                publisher_tx: a_dpub_tx,
                subscriber_rx: a_dsub_rx,
                app_handle: None,
                self_addr: alice_addr,
                self_x25519_priv: [0u8; 32],
                identity_resolver: identity_resolver.clone(),
                registry_weak: None,
                driver: None,
                membership_resolver: None,
                orchestrator_config: Default::default(),
                persist: None,
            },
        )
        .await;
    }

    // ── B's dfrost side (epoch 1, same vk) — for applying the live reset marker ──
    let dfrost_log_b = Arc::new(Mutex::new(active_committee_dfrost_log(&members, vk1, 1)));
    let dfrost_reg_b = Arc::new(DfrostLogRegistry::<tauri::test::MockRuntime>::new());
    let resolver_b = Arc::new(TestResetResolver::new(members.clone(), membership));
    let (b_dtx, b_drx) = mpsc::channel::<Vec<u8>>(4);
    {
        let (b_dpub_tx, _b_dpub_rx) = mpsc::channel::<Vec<u8>>(4);
        DfrostLogRegistry::register(
            &dfrost_reg_b,
            DfrostLogEngineParams {
                community_id,
                dfrost_log: dfrost_log_b.clone(),
                publisher_tx: b_dpub_tx,
                subscriber_rx: b_drx,
                app_handle: None,
                self_addr: bob_addr,
                self_x25519_priv: [0u8; 32],
                identity_resolver: identity_resolver.clone(),
                registry_weak: None,
                driver: None,
                membership_resolver: Some(resolver_b.clone()),
                orchestrator_config: Default::default(),
                persist: None,
            },
        )
        .await;
    }

    // ── Voting engines A (author) and B (peer), bridged directly (real
    //    packet delivery — mirrors a Zenoh pub/sub hop) ──
    let voting_log_a = Arc::new(Mutex::new(VotingLog::new()));
    let voting_log_b = Arc::new(Mutex::new(VotingLog::new()));

    struct BridgeIdentity(Arc<dyn IdentityResolver + Send + Sync>);
    #[async_trait::async_trait]
    impl VotingIdentityResolver for BridgeIdentity {
        async fn resolve(&self, owner: &OwnerAddr) -> Option<[u8; 64]> {
            self.0.resolve(owner).await
        }
    }
    struct BridgeMembership(MembershipSnapshot);
    #[async_trait::async_trait]
    impl MembershipSnapshotResolver for BridgeMembership {
        async fn snapshot_at(
            &self,
            _community_id: SpaceId,
            _hlc: &Hlc,
        ) -> Result<MembershipSnapshot, SnapshotResolverError> {
            Ok(self.0.clone())
        }
    }
    let voting_snapshot = MembershipSnapshot {
        members: [
            (
                alice_addr,
                MemberAttrs {
                    power: 100,
                    vouching_depth: 0,
                },
            ),
            (
                bob_addr,
                MemberAttrs {
                    power: 1,
                    vouching_depth: 0,
                },
            ),
        ]
        .into_iter()
        .collect(),
    };
    let voting_identity_resolver = Arc::new(BridgeIdentity(identity_resolver.clone()));
    let voting_membership_resolver = Arc::new(BridgeMembership(voting_snapshot.clone()));

    let (a_pub_tx, mut a_pub_rx) = mpsc::channel::<Vec<u8>>(16);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel::<Vec<u8>>(16);
    let device_id_a = "dev-zeb1031-c1-a".to_string();
    let hlc_tracker_a = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        device_id_a.clone(),
    )));
    let engine_a = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id,
        voting_log: voting_log_a.clone(),
        publisher_tx: a_pub_tx,
        subscriber_rx: a_sub_rx,
        hlc_tracker: Some(hlc_tracker_a),
        device_id: Some(device_id_a),
        app_handle: None,
        identity_resolver: None,
        membership_resolver: None,
    })
    .await;
    let requester_a: BeaconRequester =
        Arc::new(move |_c, _s, _e| Box::pin(async { Ok("noop".to_string()) }));
    VotingLogEngine::install_dfrost_handle(&engine_a, dfrost_reg_a.clone(), requester_a).await;

    let (b_pub_tx, _b_pub_rx) = mpsc::channel::<Vec<u8>>(16);
    let (b_sub_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(16);
    let device_id_b = "dev-zeb1031-c1-b".to_string();
    let hlc_tracker_b = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        device_id_b.clone(),
    )));
    let engine_b = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id,
        voting_log: voting_log_b.clone(),
        publisher_tx: b_pub_tx,
        subscriber_rx: b_sub_rx,
        hlc_tracker: Some(hlc_tracker_b),
        device_id: Some(device_id_b),
        app_handle: None,
        identity_resolver: Some(voting_identity_resolver),
        membership_resolver: Some(voting_membership_resolver),
    })
    .await;
    let requester_b: BeaconRequester =
        Arc::new(move |_c, _s, _e| Box::pin(async { Ok("noop".to_string()) }));
    VotingLogEngine::install_dfrost_handle(&engine_b, dfrost_reg_b.clone(), requester_b).await;

    // Bridge A's publisher output into B's subscriber input — real peer
    // delivery, never a direct dual-apply.
    tokio::spawn(async move {
        while let Some(packet) = a_pub_rx.recv().await {
            let _ = b_sub_tx.send(packet).await;
        }
    });

    // ── A mints a REAL Tier-3 PollCreate at epoch 1 ──
    let epoch1 = dfrost_reg_a
        .get(community_id)
        .await
        .expect("A's dfrost engine")
        .latest_committee_epoch()
        .await
        .expect("A's committee active");
    assert_eq!(epoch1, 1);
    let cfg1 = Tier3PollConfigPayload {
        proposal_text: "pre-reset poll".into(),
        sortition_size: 20,
        deliberation_window_seconds: 60,
        drafting_window_seconds: 60,
        ratification_window_seconds: 60,
        privacy_mode: "pu".into(),
        incentive_mode: "d".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: None,
        ce: Some(epoch1),
    };
    let event1 = build_signed_poll_create_tier3(&alice_sk, alice_addr, &cfg1, hlc(1_000, "alice"))
        .expect("sign poll1");
    let poll1_id = derive_poll_id(
        &community_id,
        &event1.signing_bytes().expect("signing bytes"),
    );
    engine_a
        .publish_event(event1, Some(voting_snapshot.clone()))
        .await
        .expect("A publishes poll1");

    // ── B materializes poll1 via process_inbound — the real peer path ──
    wait_for_voting_log("B materializes poll1", &voting_log_b, |log| {
        log.polls.contains_key(&poll1_id)
    })
    .await;
    {
        let log = voting_log_b.lock().await;
        let t3 = log.polls[&poll1_id].tier_state.as_tier3().unwrap();
        assert_eq!(
            t3.meta.community_epoch, 1,
            "B must derive community_epoch from the wire-carried ce, not a \
             local set_tier3_poll_epoch patch (which never runs for \
             peer-ingested creates)"
        );
        assert!(t3.voided.is_none());
    }

    // ── Apply the reset marker for old_epoch=1 on B's dfrost side, LIVE
    //    INGEST (never hand-calling void_tier3_polls_for_reset) ──
    let marker1_payload = ResetMarkerPayload {
        reset_proposal_id: reset1_id,
        reset_digest: digest1,
        old_vk: vk1,
        old_epoch: 1,
        space_id: community_id,
    };
    let marker1 = build_signed_dfrost_event(
        &alice_sk,
        alice_addr,
        DfrostEventKind::ResetMarker,
        &marker1_payload,
        hlc(5_000, "alice"),
    )
    .expect("sign marker1");
    let mut packet = Vec::new();
    ciborium::into_writer(&marker1, &mut packet).expect("encode marker1");
    b_dtx
        .send(packet)
        .await
        .expect("B's dfrost subscriber open");

    // ── B's PEER-MATERIALIZED poll1 (never hand-seeded) gets voided ──
    wait_for_voting_log(
        "poll1 voided on B via wire-carried epoch",
        &voting_log_b,
        |log| {
            log.polls
                .get(&poll1_id)
                .and_then(|ps| ps.tier_state.as_tier3())
                .and_then(|t3| t3.voided)
                .is_some()
        },
    )
    .await;

    // ── A mints a SECOND, post-reset poll at epoch 2 (successor committee) ──
    {
        let mut log = dfrost_log_a.lock().await;
        log.committee_state.current_epoch = 2;
    }
    let epoch2 = dfrost_reg_a
        .get(community_id)
        .await
        .expect("A's dfrost engine")
        .latest_committee_epoch()
        .await
        .expect("A's committee active");
    assert_eq!(epoch2, 2);
    let cfg2 = Tier3PollConfigPayload {
        proposal_text: "post-reset poll".into(),
        ce: Some(epoch2),
        ..cfg1
    };
    let event2 = build_signed_poll_create_tier3(&alice_sk, alice_addr, &cfg2, hlc(6_000, "alice"))
        .expect("sign poll2");
    let poll2_id = derive_poll_id(
        &community_id,
        &event2.signing_bytes().expect("signing bytes"),
    );
    engine_a
        .publish_event(event2, Some(voting_snapshot))
        .await
        .expect("A publishes poll2");

    // ── B materializes poll2 via process_inbound and it survives the
    //    (already-run) sweep untouched ──
    wait_for_voting_log("B materializes poll2", &voting_log_b, |log| {
        log.polls.contains_key(&poll2_id)
    })
    .await;
    let log = voting_log_b.lock().await;
    let t3_2 = log.polls[&poll2_id].tier_state.as_tier3().unwrap();
    assert_eq!(t3_2.meta.community_epoch, 2);
    assert!(
        t3_2.voided.is_none(),
        "post-reset poll (peer-materialized via B's own process_inbound) \
         must survive B's earlier sweep"
    );
}

/// ZEB-1031 Task 7 review M1 (folded-in, spec §7 no-silent-stall
/// guarantee): a pre-reset-epoch poll whose `kd=cr` event syncs in AFTER
/// the local void sweep already ran (independent anti-entropy — dfrost
/// catch-up and voting-log RBSR are separate wire protocols with no
/// ordering guarantee between them) must arrive ALREADY VOIDED, not live.
/// Order: sweep runs first (marker applied while the voting log is still
/// empty — the retired-epoch watermark is set with zero polls to void),
/// THEN the pre-reset poll's `kd=cr` syncs in via the real inbound path
/// (never hand-seeded). A post-reset poll delivered the same way is
/// unaffected.
#[tokio::test]
async fn late_syncing_pre_reset_poll_arrives_already_voided_via_watermark_zeb1031() {
    use harmony_app::community_voting_core::{
        build_signed_poll_create_tier3, derive_poll_id, Eligibility, Tier3PollConfigPayload,
    };
    use harmony_app::community_voting_log::VotingLog;
    use harmony_app::community_voting_log_engine::{
        BeaconRequester, VotingLogEngine, VotingLogEngineParams,
    };

    let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0xD1);
    let (_bob_sk, bob_addr, bob_pub64) = fixture_identity(0xD2);
    let mut members = vec![alice_addr, bob_addr];
    members.sort();
    let community_id = SpaceId([0xD1; 16]);
    let vk1: [u8; 32] = [0xE9; 32];

    let reset1_id: EventId = [0xC9; 16];
    let digest1 =
        dfrost_reset_digest(&community_id, &reset1_id, &vk1, 1, &members, 2).expect("digest1");
    let mut membership = MaterializedMembership::default();
    membership.power_levels.insert(alice_addr, 100);
    membership.members.insert(alice_addr, joined_member_state());
    membership.members.insert(bob_addr, joined_member_state());
    membership.reset_proposals.push(ResetProposalView {
        id: reset1_id,
        proposer: alice_addr,
        target_vk: vk1,
        target_epoch: 1,
        new_members: members.clone(),
        new_threshold: 2,
        veto_window_ms: 86_400_000,
        signers: [alice_addr].into_iter().collect(),
        proposed_at_wall_ms: 1_000,
        deadline_ms: Some(9_000),
        authorized_at_ms: Some(9_000),
        endorsed: false,
        phase: ResetPhase::Authorized,
        consumed_new_vk: None,
        consumption_superseded: false,
        effective_quorum: None,
    });
    let identity_resolver = resolver_with(&[(alice_addr, alice_pub64), (bob_addr, bob_pub64)]);
    let resolver = Arc::new(TestResetResolver::new(members.clone(), membership));

    // ── B's dfrost side (epoch 1) — wired for live-ingest marker apply ──
    let dfrost_log = Arc::new(Mutex::new(active_committee_dfrost_log(&members, vk1, 1)));
    let dfrost_reg = Arc::new(DfrostLogRegistry::<tauri::test::MockRuntime>::new());
    let (dtx, drx) = mpsc::channel::<Vec<u8>>(4);
    let (dpub_tx, _dpub_rx) = mpsc::channel::<Vec<u8>>(4);
    DfrostLogRegistry::register(
        &dfrost_reg,
        DfrostLogEngineParams {
            community_id,
            dfrost_log: dfrost_log.clone(),
            publisher_tx: dpub_tx,
            subscriber_rx: drx,
            app_handle: None,
            self_addr: bob_addr,
            self_x25519_priv: [0u8; 32],
            identity_resolver: identity_resolver.clone(),
            registry_weak: None,
            driver: None,
            membership_resolver: Some(resolver.clone()),
            orchestrator_config: Default::default(),
            persist: None,
        },
    )
    .await;

    // ── B's voting engine — EMPTY at the moment the sweep runs ──
    let voting_log = Arc::new(Mutex::new(VotingLog::new()));
    // `verify_voting_event` (the inbound sig-verify gate) needs a REAL
    // resolver — alice's pub64 must resolve for her signed kd=cr events
    // to verify, or process_inbound silently drops them and this test's
    // "arrives via the real inbound path" premise never fires.
    struct BridgeVotingIdentity(Arc<dyn IdentityResolver + Send + Sync>);
    #[async_trait::async_trait]
    impl harmony_app::community_voting_core::VotingIdentityResolver for BridgeVotingIdentity {
        async fn resolve(&self, owner: &OwnerAddr) -> Option<[u8; 64]> {
            self.0.resolve(owner).await
        }
    }
    struct BridgeMembership(Arc<TestResetResolver>);
    #[async_trait::async_trait]
    impl MembershipSnapshotResolver for BridgeMembership {
        async fn snapshot_at(
            &self,
            community_id: SpaceId,
            hlc: &Hlc,
        ) -> Result<MembershipSnapshot, SnapshotResolverError> {
            self.0.snapshot_at(community_id, hlc).await
        }
    }
    let (v_pub_tx, _v_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (v_sub_tx, v_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let device_id = "dev-zeb1031-m1".to_string();
    let hlc_tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        device_id.clone(),
    )));
    let voting_engine = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id,
        voting_log: voting_log.clone(),
        publisher_tx: v_pub_tx,
        subscriber_rx: v_sub_rx,
        hlc_tracker: Some(hlc_tracker),
        device_id: Some(device_id),
        app_handle: None,
        identity_resolver: Some(Arc::new(BridgeVotingIdentity(identity_resolver.clone()))),
        membership_resolver: Some(Arc::new(BridgeMembership(resolver))),
    })
    .await;
    let requester: BeaconRequester =
        Arc::new(move |_c, _s, _e| Box::pin(async { Ok("noop".to_string()) }));
    VotingLogEngine::install_dfrost_handle(&voting_engine, dfrost_reg, requester).await;

    // ── Sweep runs FIRST: apply the reset marker while the voting log is
    //    still empty (zero polls to void — only the watermark advances) ──
    let marker1_payload = ResetMarkerPayload {
        reset_proposal_id: reset1_id,
        reset_digest: digest1,
        old_vk: vk1,
        old_epoch: 1,
        space_id: community_id,
    };
    let marker1 = build_signed_dfrost_event(
        &alice_sk,
        alice_addr,
        DfrostEventKind::ResetMarker,
        &marker1_payload,
        hlc(5_000, "alice"),
    )
    .expect("sign marker1");
    let mut packet = Vec::new();
    ciborium::into_writer(&marker1, &mut packet).expect("encode marker1");
    dtx.send(packet).await.expect("dfrost subscriber open");

    // Wait for the sweep's async dispatch to land — no polls exist yet to
    // observe voiding on, so poll the watermark directly instead.
    wait_for_voting_log(
        "watermark advances from the empty-log sweep",
        &voting_log,
        |log| log.retired_epoch_watermark.is_some(),
    )
    .await;
    {
        let log = voting_log.lock().await;
        let watermark = log.retired_epoch_watermark.expect("watermark set");
        assert_eq!(watermark.reset_id, reset1_id);
        assert_eq!(watermark.old_epoch, 1);
        assert!(
            log.polls.is_empty(),
            "the sweep ran against an empty log — nothing to void yet"
        );
    }

    // ── NOW the pre-reset poll's kd=cr syncs in — real inbound delivery,
    //    never hand-seeded, never a dual-apply ──
    let cfg1 = Tier3PollConfigPayload {
        proposal_text: "late-syncing pre-reset poll".into(),
        sortition_size: 20,
        deliberation_window_seconds: 60,
        drafting_window_seconds: 60,
        ratification_window_seconds: 60,
        privacy_mode: "pu".into(),
        incentive_mode: "d".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: None,
        ce: Some(1),
    };
    let event1 = build_signed_poll_create_tier3(&alice_sk, alice_addr, &cfg1, hlc(1_000, "alice"))
        .expect("sign poll1");
    let poll1_id = derive_poll_id(
        &community_id,
        &event1.signing_bytes().expect("signing bytes"),
    );
    let mut packet1 = Vec::new();
    ciborium::into_writer(&event1, &mut packet1).expect("encode poll1");
    v_sub_tx
        .send(packet1)
        .await
        .expect("voting subscriber open");

    wait_for_voting_log(
        "B materializes the late pre-reset poll",
        &voting_log,
        |log| log.polls.contains_key(&poll1_id),
    )
    .await;
    {
        let log = voting_log.lock().await;
        let t3 = log.polls[&poll1_id].tier_state.as_tier3().unwrap();
        let voided = t3.voided.expect(
            "a pre-reset-epoch poll syncing in AFTER the sweep must materialize \
             already voided via the retired-epoch watermark, not live",
        );
        assert_eq!(voided.reset_id, reset1_id);
        assert_eq!(voided.old_epoch, 1);
    }

    // ── A post-reset poll delivered the same way is unaffected ──
    let cfg2 = Tier3PollConfigPayload {
        proposal_text: "post-reset poll, late sync".into(),
        ce: Some(2),
        ..cfg1
    };
    let event2 = build_signed_poll_create_tier3(&alice_sk, alice_addr, &cfg2, hlc(6_000, "alice"))
        .expect("sign poll2");
    let poll2_id = derive_poll_id(
        &community_id,
        &event2.signing_bytes().expect("signing bytes"),
    );
    let mut packet2 = Vec::new();
    ciborium::into_writer(&event2, &mut packet2).expect("encode poll2");
    v_sub_tx
        .send(packet2)
        .await
        .expect("voting subscriber open");

    wait_for_voting_log("B materializes the post-reset poll", &voting_log, |log| {
        log.polls.contains_key(&poll2_id)
    })
    .await;
    let log = voting_log.lock().await;
    let t3_2 = log.polls[&poll2_id].tier_state.as_tier3().unwrap();
    assert!(
        t3_2.voided.is_none(),
        "a post-reset poll must NOT be voided by a watermark from an earlier reset"
    );
}
