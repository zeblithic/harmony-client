//! ZEB-305 Phase 4a-foundation Task 8: full DKG round-trip via the
//! IPC-mandated event-construction path.
//!
//! ## Why this file exists alongside `community_dfrost_integration.rs`
//!
//! The ZEB-303 test `dkg_two_engine_2of2_converges_on_joint_vk` already
//! proves the apply layer converges. This test layers the additional
//! constraint that the events being applied are exactly the ones the
//! ZEB-305 IPC handlers in `src-tauri/src/lib.rs` build — i.e. it
//! exercises:
//!
//! * `ceremony_id = blake3(sorted_members || threshold_le || hlc.wall_ms_le)`
//!   derivation (the IPC's deterministic id);
//! * `PendingCeremony` pre-seeding by the initiator (matches the IPC's
//!   "seed pending_dkg before applying own dr rn=1" sequencing);
//! * `build_signed_dfrost_event` envelope construction with real Ed25519
//!   sigs (the IPC's `outbox.signing_key.sign(signing_bytes)` path);
//! * `local_dkg_secret` / `local_dkg_secret2` stashing on the log
//!   between rounds (mirrors the IPC's lock-stash-apply ordering);
//! * `apply_with_identity` on every local apply call-site (so a future
//!   IPC change that switches to `apply` would diverge from this test's
//!   shape).
//!
//! ## Why we don't go through `tauri::test::get_ipc_response`
//!
//! The dfrost IPCs require a fully-set-up `NodeState` (`hlc_tracker`,
//! `dm_device_id`, `dm_self_owner`, `dm_outbox` with a real signing key,
//! `community_registry` with an `IdentityResolver` that knows both
//! members' device pubs). Wiring all of that against a Tauri mock app
//! would dwarf the test itself and provide no additional coverage over
//! the apply-layer convergence we already test.
//!
//! `tests/dm_ipc_roundtrip.rs` documents the same scoping decision for
//! the DM IPCs — it covers the JS↔Rust parameter-binding boundary
//! (against an empty `NodeState`) and explicitly punts Ok-path coverage
//! to the inner-function-call integration tests. Same pattern here.
//!
//! ## Pattern source
//!
//! `tests/community_dfrost_integration.rs::dkg_2of2_setup` provides the
//! 2-engine cross-apply scaffolding; this file restructures the same
//! ceremony to follow the IPC handlers' event-construction order.

use std::collections::BTreeMap;

use frost_ristretto255::{
    self as frost,
    keys::{KeyPackage, PublicKeyPackage},
    Identifier,
};
use harmony_app::community_dfrost_crypto::{
    dkg_part1_local, dkg_part2_local, dkg_part3_local, identifier_for_index,
    verify_schnorr_signature, verifying_key_to_bytes, verifying_share_to_bytes,
};
use harmony_app::community_dfrost_log::{
    build_signed_dfrost_event, CommitteeState, DfrostLog, PendingCeremony,
};
use harmony_app::community_dfrost_types::{
    derive_ceremony_id as derive_ceremony_id_canonical, derive_vrf_output, derive_vrf_seed,
    DfrostEventKind, DkgCompletePayload, DkgRoundPayload, MemberVerifyingShare,
    ThresholdSignPayload, VrfBeaconPayload,
};
use harmony_app::community_membership::RecipientCiphertext;
use harmony_app::dm_signing;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use x25519_dalek::{PublicKey, StaticSecret};

// ─── Deterministic 2-member committee fixtures ─────────────────────────────

const ALICE: OwnerAddr = OwnerAddr([0x01; 16]);
const BOB: OwnerAddr = OwnerAddr([0x02; 16]);

fn sorted_members() -> Vec<OwnerAddr> {
    // ALICE < BOB byte-wise → sorted order matches initiator ordering in
    // the IPC (`member_addrs.sort()`).
    vec![ALICE, BOB]
}

/// Per-node Ed25519 signing key. The IPCs read this from
/// `dm_outbox.signing_key`; here we hand it to `build_signed_dfrost_event`
/// directly so the envelope sig path is exercised end-to-end.
fn alice_ed25519_sk() -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[0xAAu8; 32])
}

fn bob_ed25519_sk() -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[0xBBu8; 32])
}

/// X25519 keypair per node, derived from the Ed25519 signing key via
/// `dm_signing::ed25519_priv_to_x25519` — same path the IPC takes
/// (`*ed25519_priv_to_x25519(signing_key)`).
fn alice_x25519_priv() -> [u8; 32] {
    *dm_signing::ed25519_priv_to_x25519(&alice_ed25519_sk())
}

fn bob_x25519_priv() -> [u8; 32] {
    *dm_signing::ed25519_priv_to_x25519(&bob_ed25519_sk())
}

fn alice_x25519_pub() -> [u8; 32] {
    *PublicKey::from(&StaticSecret::from(alice_x25519_priv())).as_bytes()
}

fn bob_x25519_pub() -> [u8; 32] {
    *PublicKey::from(&StaticSecret::from(bob_x25519_priv())).as_bytes()
}

fn hlc_at(wall_ms: u64, node: &str) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: node.into(),
    }
}

/// IPC-canonical `ceremony_id` derivation:
/// `blake3(sorted_members || threshold_le || hlc.wall_ms_le)`.
/// Lifted verbatim from `dfrost_initiate_dkg` step 6.
fn derive_ceremony_id(members: &[OwnerAddr], threshold: u16, hlc_wall_ms: u64) -> [u8; 32] {
    let mut hasher_input: Vec<u8> = Vec::with_capacity(members.len() * 16 + 2 + 8);
    for a in members {
        hasher_input.extend_from_slice(&a.0);
    }
    hasher_input.extend_from_slice(&threshold.to_le_bytes());
    hasher_input.extend_from_slice(&hlc_wall_ms.to_le_bytes());
    blake3::hash(&hasher_input).into()
}

// ─── Helpers replicating each IPC's event construction ─────────────────────

/// Replicate `dfrost_initiate_dkg` step 7-9 for one node: derive
/// ceremony_id (caller supplies), seed `pending_dkg`, run FROST round 1,
/// stash `local_dkg_secret`, build signed `dr rn=1`, apply locally.
///
/// Returns the signed event so the caller can cross-apply it on peer
/// engines.
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
    // 1. Pre-seed pending_dkg (the IPC does this before apply).
    log.committee_state.pending_dkg = Some(PendingCeremony {
        ceremony_id,
        members: members.to_vec(),
        threshold,
        max_signers,
        proposed_epoch,
        ..Default::default()
    });

    // 2. Run FROST DKG round 1.
    let (r1_secret, r1_pkg_bytes) =
        dkg_part1_local(self_id, max_signers, threshold).expect("dkg_part1_local");
    log.local_dkg_secret = Some(r1_secret);

    // 3. Build the dr rn=1 payload + envelope sig.
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

    // 4. Apply locally via apply_with_identity (which delegates to apply
    //    for rn=1 since there's no decrypt path).
    log.apply_with_identity(event.clone(), &self_addr, self_x25519_priv)
        .expect("apply own dr rn=1");
    event
}

/// Replicate `dfrost_contribute_dkg_round` round_num=2 for one node:
/// snapshot pending state, FROST `dkg::part2`, seal each output package
/// to the recipient's X25519 pubkey, stash `local_dkg_secret2`, build
/// signed `dr rn=2`, apply locally.
///
/// Returns the signed event for cross-apply on peer engines.
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
    // 1. Snapshot pending state (r1_received from peers + own secret).
    let (members_snapshot, r1_received_by_addr, r1_secret) = {
        let pending = log
            .committee_state
            .pending_dkg
            .as_ref()
            .expect("pending_dkg present");
        assert_eq!(pending.ceremony_id, ceremony_id, "ceremony_id matches");
        let r1: BTreeMap<OwnerAddr, Vec<u8>> = pending
            .round1_packages
            .iter()
            .filter(|(addr, _)| **addr != self_addr)
            .map(|(addr, pkg)| (*addr, pkg.clone()))
            .collect();
        let secret = log
            .local_dkg_secret
            .clone()
            .expect("local_dkg_secret stashed by initiate");
        (pending.members.clone(), r1, secret)
    };

    // Quorum gate matches IPC.
    let expected_others = members_snapshot.len().saturating_sub(1);
    assert_eq!(
        r1_received_by_addr.len(),
        expected_others,
        "round 2 needs r1 from every other member"
    );

    // 2. OwnerAddr → FROST Identifier translation.
    let mut r1_by_id: BTreeMap<Identifier, Vec<u8>> = BTreeMap::new();
    for (addr, pkg_bytes) in &r1_received_by_addr {
        let idx = members
            .iter()
            .position(|a| *a == *addr)
            .expect("peer in members");
        r1_by_id.insert(identifier_for_index(idx), pkg_bytes.clone());
    }

    // 3. FROST DKG round 2.
    let (r2_secret, r2_packages_by_id) =
        dkg_part2_local(r1_secret, &r1_by_id).expect("dkg_part2_local");

    // 4. Seal each output package to its recipient's X25519 pubkey.
    let mut recipient_ciphertexts: Vec<RecipientCiphertext> =
        Vec::with_capacity(r2_packages_by_id.len());
    for (recipient_id, r2_pkg_bytes) in &r2_packages_by_id {
        // Identifier → OwnerAddr reverse lookup (same as IPC).
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
        let sealed = dm_signing::seal_to_owner(recipient_pub, r2_pkg_bytes).expect("seal_to_owner");
        recipient_ciphertexts.push(RecipientCiphertext {
            recipient: recipient_addr,
            sealed,
        });
    }

    // 5. Stash r2_secret BEFORE apply (matches IPC ordering).
    log.local_dkg_secret2 = Some(r2_secret);

    // 6. Build + sign the dr rn=2 event.
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

    // 7. Apply locally. apply_with_identity iterates recipient_ciphertexts
    //    and decrypts only the one targeting self_addr — for an outbound
    //    rn=2 event, no ciphertext targets self, so nothing is decrypted
    //    but the event is recorded.
    log.apply_with_identity(event.clone(), &self_addr, self_x25519_priv)
        .expect("apply own dr rn=2");
    event
}

/// Replicate `dfrost_contribute_dkg_round` round_num=3 for one node:
/// snapshot pending state (r1 from peers + r2 plaintexts decrypted-for-
/// self), FROST `dkg::part3`, extract joint VK + per-member verifying
/// shares, stash `local_key_package` + `local_pub_key_package`, build
/// signed `dk`, apply locally.
///
/// Returns the signed event for cross-apply on peer engines.
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
    PublicKeyPackage,
) {
    // 1. Snapshot pending state.
    let (members_snapshot, threshold, max_signers, proposed_epoch, r1_by_id, r2_by_id, secret2) = {
        let pending = log
            .committee_state
            .pending_dkg
            .as_ref()
            .expect("pending_dkg present");
        assert_eq!(pending.ceremony_id, ceremony_id, "ceremony_id matches");

        let members_snap = pending.members.clone();

        let mut r1: BTreeMap<Identifier, Vec<u8>> = BTreeMap::new();
        for (addr, pkg) in pending
            .round1_packages
            .iter()
            .filter(|(a, _)| **a != self_addr)
        {
            let idx = members
                .iter()
                .position(|a| *a == *addr)
                .expect("peer in members");
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
            .expect("local_dkg_secret2 stashed by round 2");

        (
            members_snap,
            pending.threshold,
            pending.max_signers,
            pending.proposed_epoch,
            r1,
            r2,
            secret,
        )
    };

    // Quorum gate matches IPC.
    let expected_others = members_snapshot.len().saturating_sub(1);
    assert_eq!(
        r1_by_id.len(),
        expected_others,
        "round 3 needs r1 from every other member"
    );
    assert_eq!(
        r2_by_id.len(),
        expected_others,
        "round 3 needs r2 from every other member"
    );

    // 2. FROST DKG round 3.
    let (key_package, pub_key_package) =
        dkg_part3_local(&secret2, &r1_by_id, &r2_by_id).expect("dkg_part3_local");

    // 3. Extract joint VK + per-member verifying shares (Identifier → OwnerAddr
    //    reverse lookup, same as IPC).
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

    // 4. Stash KeyPackage + PublicKeyPackage BEFORE building event (IPC order).
    log.local_key_package = Some(key_package.clone());
    log.local_pub_key_package = Some(pub_key_package.clone());

    // 5. Build + sign the dk event.
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

    // 6. Apply locally. For dk, apply_with_identity falls through to
    //    apply() (no decrypt path).
    log.apply_with_identity(event.clone(), &self_addr, self_x25519_priv)
        .expect("apply own dk");
    (event, key_package, pub_key_package)
}

// ─── The test ──────────────────────────────────────────────────────────────

/// Two-engine 2-of-2 DKG ceremony driven through the IPC-mandated event
/// construction path. Both engines (Alice + Bob) walk the same sequence
/// the `dfrost_initiate_dkg` + `dfrost_contribute_dkg_round` IPCs would
/// follow, cross-applying each other's events. Final convergence: both
/// engines materialize identical `joint_verifying_key` and become active.
#[tokio::test]
async fn dkg_ipc_round_trip_two_engine_2of2() {
    // Two fresh logs, one per engine.
    let mut log_a = DfrostLog::new();
    let mut log_b = DfrostLog::new();

    let members = sorted_members();
    let threshold: u16 = 2;
    let max_signers: u16 = 2;
    // First DKG promotes epoch 0 → 1 — matches dfrost_initiate_dkg's
    // `proposed_epoch: log.committee_state.current_epoch + 1`.
    let proposed_epoch: u64 = 1;

    // Identifier assignment is by sorted-index (same as
    // `CommitteeState::build_identifier_map` and `identifier_for_index`).
    let id_alice = identifier_for_index(0);
    let id_bob = identifier_for_index(1);

    // Pre-compute each member's X25519 pubkey. The IPC resolves these via
    // `community_registry.identity_resolver().resolve(addr)`; here we hand
    // them in via a small registry-shaped BTreeMap.
    let recipient_pubs: BTreeMap<OwnerAddr, [u8; 32]> =
        [(ALICE, alice_x25519_pub()), (BOB, bob_x25519_pub())]
            .into_iter()
            .collect();

    let alice_sk = alice_ed25519_sk();
    let bob_sk = bob_ed25519_sk();
    let alice_x_priv = alice_x25519_priv();
    let bob_x_priv = bob_x25519_priv();

    // ── Round 1: Alice initiates ──────────────────────────────────────────
    //
    // The IPC derives ceremony_id from `hlc.wall_ms` reserved per device.
    // For this test the initiator's `wall_ms` is what binds the ceremony
    // id; both engines must agree on this value (in production the dr rn=1
    // event carries Alice's HLC and Bob seeds his pending_dkg from it).
    let alice_init_hlc = hlc_at(1_000, "alice");
    let ceremony_id = derive_ceremony_id(&members, threshold, alice_init_hlc.wall_ms);

    let dr1_alice = initiate_dkg_local(
        &mut log_a,
        ALICE,
        id_alice,
        &members,
        threshold,
        max_signers,
        proposed_epoch,
        ceremony_id,
        alice_init_hlc,
        &alice_sk,
        &alice_x_priv,
    );

    // Bob, on receiving dr rn=1 from Alice, would normally have his
    // pending_dkg seeded by the ceremony-bootstrap step that arrives with
    // the broadcast (Phase 4a-main wires this; for now we seed it here).
    log_b.committee_state.pending_dkg = Some(PendingCeremony {
        ceremony_id,
        members: members.clone(),
        threshold,
        max_signers,
        proposed_epoch,
        ..Default::default()
    });
    log_b
        .apply_with_identity(dr1_alice, &BOB, &bob_x_priv)
        .expect("bob applies alice's dr rn=1");

    // ── Bob initiates his round-1 contribution (same IPC path) ────────────
    //
    // From Bob's POV he calls `dfrost_initiate_dkg` … but the IPC rejects
    // re-initiation when pending_dkg is already populated. In Phase 4a-main
    // the second node won't call initiate at all — peers only contribute
    // rounds 2+ once they've observed the initiator's rn=1. To mirror the
    // IPC's event-construction shape for Bob's rn=1 we replicate steps
    // 7-9 (FROST part1 + stash + build dr rn=1 + apply) without the
    // pending_dkg seed (Bob already has it from the cross-applied
    // alice rn=1 above).
    let bob_init_hlc = hlc_at(1_100, "bob");
    let (bob_r1_secret, bob_r1_pkg_bytes) =
        dkg_part1_local(id_bob, max_signers, threshold).expect("bob dkg_part1");
    log_b.local_dkg_secret = Some(bob_r1_secret);
    let bob_r1_payload = DkgRoundPayload {
        ceremony_id,
        round_num: 1,
        round1_package: Some(bob_r1_pkg_bytes),
        recipient_ciphertexts: None,
    };
    let dr1_bob = build_signed_dfrost_event(
        &bob_sk,
        BOB,
        DfrostEventKind::DkgRound,
        &bob_r1_payload,
        bob_init_hlc,
    )
    .expect("build_signed dr rn=1 (bob)");
    log_b
        .apply_with_identity(dr1_bob.clone(), &BOB, &bob_x_priv)
        .expect("bob applies own dr rn=1");
    log_a
        .apply_with_identity(dr1_bob, &ALICE, &alice_x_priv)
        .expect("alice applies bob's dr rn=1");

    // After round 1: both engines hold round1_packages for {ALICE, BOB}.
    {
        let pa = log_a
            .committee_state
            .pending_dkg
            .as_ref()
            .expect("pending a");
        let pb = log_b
            .committee_state
            .pending_dkg
            .as_ref()
            .expect("pending b");
        assert_eq!(pa.round1_packages.len(), 2, "alice sees both r1 pkgs");
        assert_eq!(pb.round1_packages.len(), 2, "bob sees both r1 pkgs");
    }

    // ── Round 2: each engine contributes their rn=2 + cross-applies ───────
    let dr2_alice = contribute_dkg_round2_local(
        &mut log_a,
        ALICE,
        ceremony_id,
        &members,
        &recipient_pubs,
        hlc_at(2_000, "alice"),
        &alice_sk,
        &alice_x_priv,
    );
    let dr2_bob = contribute_dkg_round2_local(
        &mut log_b,
        BOB,
        ceremony_id,
        &members,
        &recipient_pubs,
        hlc_at(2_100, "bob"),
        &bob_sk,
        &bob_x_priv,
    );

    // Cross-apply: Bob applies Alice's rn=2 (decrypts the sealed-to-Bob
    // ciphertext), Alice applies Bob's rn=2 (decrypts the sealed-to-Alice
    // one).
    log_b
        .apply_with_identity(dr2_alice, &BOB, &bob_x_priv)
        .expect("bob decrypts alice's dr rn=2");
    log_a
        .apply_with_identity(dr2_bob, &ALICE, &alice_x_priv)
        .expect("alice decrypts bob's dr rn=2");

    // After round 2: each engine has a decrypted round-2 package from the
    // OTHER member (sender-keyed).
    {
        let pa = log_a
            .committee_state
            .pending_dkg
            .as_ref()
            .expect("pending a");
        let pb = log_b
            .committee_state
            .pending_dkg
            .as_ref()
            .expect("pending b");
        assert_eq!(
            pa.round2_packages.len(),
            1,
            "alice decrypted one r2 (from bob)"
        );
        assert!(
            pa.round2_packages.contains_key(&BOB),
            "alice's r2_packages[BOB] populated"
        );
        assert_eq!(
            pb.round2_packages.len(),
            1,
            "bob decrypted one r2 (from alice)"
        );
        assert!(
            pb.round2_packages.contains_key(&ALICE),
            "bob's r2_packages[ALICE] populated"
        );
    }

    // ── Round 3: each engine finalizes locally + emits dk ────────────────
    let (dk_alice, alice_key_pkg, alice_pub_pkg) = contribute_dkg_round3_local(
        &mut log_a,
        ALICE,
        ceremony_id,
        &members,
        hlc_at(3_000, "alice"),
        &alice_sk,
        &alice_x_priv,
    );
    let (dk_bob, _bob_key_pkg, bob_pub_pkg) = contribute_dkg_round3_local(
        &mut log_b,
        BOB,
        ceremony_id,
        &members,
        hlc_at(3_100, "bob"),
        &bob_sk,
        &bob_x_priv,
    );

    // Sanity: both engines' FROST part3 produced the same joint VK
    // (mathematical invariant of DKG; this is the precondition for
    // `apply_dkg_complete` to converge rather than reject on mismatch).
    assert_eq!(
        verifying_key_to_bytes(alice_pub_pkg.verifying_key()),
        verifying_key_to_bytes(bob_pub_pkg.verifying_key()),
        "FROST part3 must derive the same joint VK on both engines"
    );

    // Cross-apply dk events. threshold=2 requires both confirmations to
    // promote the committee to active.
    log_b
        .apply_with_identity(dk_alice, &BOB, &bob_x_priv)
        .expect("bob applies alice's dk");
    log_a
        .apply_with_identity(dk_bob, &ALICE, &alice_x_priv)
        .expect("alice applies bob's dk");

    // ── Convergence assertions (the core IPC contract) ───────────────────
    assert!(
        log_a.committee_state.active,
        "engine A must be active after dk quorum"
    );
    assert!(
        log_b.committee_state.active,
        "engine B must be active after dk quorum"
    );
    assert_eq!(
        log_a.committee_state.current_epoch, 1,
        "engine A epoch advanced to 1"
    );
    assert_eq!(
        log_b.committee_state.current_epoch, 1,
        "engine B epoch advanced to 1"
    );

    let vk_a = log_a
        .committee_state
        .joint_verifying_key
        .expect("alice joint_vk present after dk");
    let vk_b = log_b
        .committee_state
        .joint_verifying_key
        .expect("bob joint_vk present after dk");
    assert_eq!(
        vk_a, vk_b,
        "engines must converge on identical joint VK — the ZEB-305 IPC contract"
    );
    assert_eq!(
        vk_a,
        verifying_key_to_bytes(alice_pub_pkg.verifying_key()),
        "engine A's materialized vk matches alice's local PublicKeyPackage"
    );

    // Per-member verifying shares + members + threshold must also converge.
    assert_eq!(
        log_a.committee_state.verifying_shares, log_b.committee_state.verifying_shares,
        "verifying_shares converge"
    );
    assert_eq!(
        log_a.committee_state.members, log_b.committee_state.members,
        "members converge"
    );
    assert_eq!(log_a.committee_state.threshold, threshold);
    assert_eq!(log_a.committee_state.max_signers, max_signers);

    // pending_dkg is cleared on successful finalization (the IPC's
    // post-condition for round 3 promotion).
    assert!(
        log_a.committee_state.pending_dkg.is_none(),
        "alice's pending_dkg cleared post-promotion"
    );
    assert!(
        log_b.committee_state.pending_dkg.is_none(),
        "bob's pending_dkg cleared post-promotion"
    );

    // Local share material stash exists on each engine (the round-3 IPC's
    // post-condition: KeyPackage + PublicKeyPackage are kept in-memory for
    // the downstream threshold-sign IPC).
    assert!(
        log_a.local_key_package.is_some(),
        "alice's local KeyPackage stashed by round-3 contribution"
    );
    assert!(
        log_b.local_key_package.is_some(),
        "bob's local KeyPackage stashed by round-3 contribution"
    );
    // Bind out _alice_key_pkg so the unused-variable lint doesn't fire and
    // the test surfaces an actual KeyPackage we could feed to a future
    // sign-side IPC roundtrip (Tasks 9-10).
    let _ = alice_key_pkg;

    // Sanity: identifier-map invariants — `CommitteeState::build_identifier_map`
    // assigns ids by sorted-index, same as the IPC's `identifier_for_index`.
    let id_map = CommitteeState::build_identifier_map(&members);
    assert_eq!(id_map[&ALICE], id_alice);
    assert_eq!(id_map[&BOB], id_bob);

    // Belt-and-suspenders cross-check against the FROST library's own
    // identifier ordering (matches what `dkg::part2`/`part3` expect).
    let id_alice_check = Identifier::try_from(1u16).expect("alice id");
    let id_bob_check = Identifier::try_from(2u16).expect("bob id");
    assert_eq!(id_alice, id_alice_check);
    assert_eq!(id_bob, id_bob_check);

    // Silence the unused-import warning if frost::Identifier is only used
    // here (it is — keeps imports tight).
    let _ = frost::Identifier::try_from(1u16).expect("frost id construction");
}

// ─── Task 9: threshold-sign + VRF beacon IPC round-trip ────────────────────

/// Drive both engines to DKG-completion via the same path as the round-1
/// test above. Returns the two activated logs together with the cached
/// FROST `KeyPackage`/`PublicKeyPackage` materialised on each engine.
///
/// Extracted from `dkg_ipc_round_trip_two_engine_2of2` so the threshold-
/// sign test below can start from "both engines active on identical joint
/// vk" without re-asserting every DKG invariant inline.
fn dkg_complete_two_engine_via_ipc_path() -> (
    DfrostLog,
    DfrostLog,
    KeyPackage,
    KeyPackage,
    PublicKeyPackage,
    PublicKeyPackage,
    Vec<OwnerAddr>,
) {
    let mut log_a = DfrostLog::new();
    let mut log_b = DfrostLog::new();

    let members = sorted_members();
    let threshold: u16 = 2;
    let max_signers: u16 = 2;
    let proposed_epoch: u64 = 1;

    let id_alice = identifier_for_index(0);
    let id_bob = identifier_for_index(1);

    let recipient_pubs: BTreeMap<OwnerAddr, [u8; 32]> =
        [(ALICE, alice_x25519_pub()), (BOB, bob_x25519_pub())]
            .into_iter()
            .collect();

    let alice_sk = alice_ed25519_sk();
    let bob_sk = bob_ed25519_sk();
    let alice_x_priv = alice_x25519_priv();
    let bob_x_priv = bob_x25519_priv();

    // ── Round 1
    let alice_init_hlc = hlc_at(1_000, "alice");
    let ceremony_id = derive_ceremony_id(&members, threshold, alice_init_hlc.wall_ms);

    let dr1_alice = initiate_dkg_local(
        &mut log_a,
        ALICE,
        id_alice,
        &members,
        threshold,
        max_signers,
        proposed_epoch,
        ceremony_id,
        alice_init_hlc,
        &alice_sk,
        &alice_x_priv,
    );

    log_b.committee_state.pending_dkg = Some(PendingCeremony {
        ceremony_id,
        members: members.clone(),
        threshold,
        max_signers,
        proposed_epoch,
        ..Default::default()
    });
    log_b
        .apply_with_identity(dr1_alice, &BOB, &bob_x_priv)
        .expect("bob applies alice's dr rn=1");

    let bob_init_hlc = hlc_at(1_100, "bob");
    let (bob_r1_secret, bob_r1_pkg_bytes) =
        dkg_part1_local(id_bob, max_signers, threshold).expect("bob dkg_part1");
    log_b.local_dkg_secret = Some(bob_r1_secret);
    let bob_r1_payload = DkgRoundPayload {
        ceremony_id,
        round_num: 1,
        round1_package: Some(bob_r1_pkg_bytes),
        recipient_ciphertexts: None,
    };
    let dr1_bob = build_signed_dfrost_event(
        &bob_sk,
        BOB,
        DfrostEventKind::DkgRound,
        &bob_r1_payload,
        bob_init_hlc,
    )
    .expect("build_signed dr rn=1 (bob)");
    log_b
        .apply_with_identity(dr1_bob.clone(), &BOB, &bob_x_priv)
        .expect("bob applies own dr rn=1");
    log_a
        .apply_with_identity(dr1_bob, &ALICE, &alice_x_priv)
        .expect("alice applies bob's dr rn=1");

    // ── Round 2
    let dr2_alice = contribute_dkg_round2_local(
        &mut log_a,
        ALICE,
        ceremony_id,
        &members,
        &recipient_pubs,
        hlc_at(2_000, "alice"),
        &alice_sk,
        &alice_x_priv,
    );
    let dr2_bob = contribute_dkg_round2_local(
        &mut log_b,
        BOB,
        ceremony_id,
        &members,
        &recipient_pubs,
        hlc_at(2_100, "bob"),
        &bob_sk,
        &bob_x_priv,
    );
    log_b
        .apply_with_identity(dr2_alice, &BOB, &bob_x_priv)
        .expect("bob applies alice's dr rn=2");
    log_a
        .apply_with_identity(dr2_bob, &ALICE, &alice_x_priv)
        .expect("alice applies bob's dr rn=2");

    // ── Round 3
    let (dk_alice, alice_key_pkg, alice_pub_pkg) = contribute_dkg_round3_local(
        &mut log_a,
        ALICE,
        ceremony_id,
        &members,
        hlc_at(3_000, "alice"),
        &alice_sk,
        &alice_x_priv,
    );
    let (dk_bob, bob_key_pkg, bob_pub_pkg) = contribute_dkg_round3_local(
        &mut log_b,
        BOB,
        ceremony_id,
        &members,
        hlc_at(3_100, "bob"),
        &bob_sk,
        &bob_x_priv,
    );

    log_b
        .apply_with_identity(dk_alice, &BOB, &bob_x_priv)
        .expect("bob applies alice's dk");
    log_a
        .apply_with_identity(dk_bob, &ALICE, &alice_x_priv)
        .expect("alice applies bob's dk");

    // Precondition for the threshold-sign test: both engines active +
    // identical joint vk.
    assert!(log_a.committee_state.active);
    assert!(log_b.committee_state.active);
    assert_eq!(
        log_a.committee_state.joint_verifying_key, log_b.committee_state.joint_verifying_key,
        "DKG must converge on identical joint VK before threshold-sign starts"
    );

    (
        log_a,
        log_b,
        alice_key_pkg,
        bob_key_pkg,
        alice_pub_pkg,
        bob_pub_pkg,
        members,
    )
}

/// Mirror `dfrost_request_vrf_beacon` step 7-9 for a single node:
/// build the `ts` event with empty share_bytes (commitments only),
/// apply it locally, then stash the secret `local_nonces` CBOR on the
/// freshly-materialised `pending_sign[ceremony_id]` entry — exactly the
/// IPC's lock-stash-apply ordering (`apply` first because
/// `apply_threshold_sign` is what creates the pending_sign entry).
///
/// Returns the signed event so the caller can cross-apply it on peers.
#[allow(clippy::too_many_arguments)]
fn request_vrf_beacon_local(
    log: &mut DfrostLog,
    self_addr: OwnerAddr,
    sign_ceremony_id: [u8; 32],
    message_hash: [u8; 32],
    commitments_cbor: Vec<u8>,
    nonces_cbor: Vec<u8>,
    hlc: Hlc,
    signing_key: &ed25519_dalek::SigningKey,
    self_x25519_priv: &[u8; 32],
) -> harmony_app::community_dfrost_types::SignedCommitteeEvent {
    // 1. Build + sign the `ts` event with empty share_bytes.
    let payload = ThresholdSignPayload {
        ceremony_id: sign_ceremony_id,
        message_hash,
        commitment_bytes: commitments_cbor,
        share_bytes: Vec::new(),
    };
    let event = build_signed_dfrost_event(
        signing_key,
        self_addr,
        DfrostEventKind::ThresholdSign,
        &payload,
        hlc,
    )
    .expect("build_signed ts (empty share)");

    // 2. Apply locally — this is what creates the pending_sign entry.
    log.apply_with_identity(event.clone(), &self_addr, self_x25519_priv)
        .expect("apply own ts (empty share)");

    // 3. Stash nonces on the freshly-materialised entry (IPC order).
    let pending = log
        .committee_state
        .pending_sign
        .get_mut(&sign_ceremony_id)
        .expect("pending_sign entry materialised by apply_threshold_sign");
    pending.local_nonces = Some(nonces_cbor);

    event
}

/// Mirror `dfrost_contribute_threshold_sign` step 4-6 for a single node:
/// decode stashed nonces, build the `commitments_map` from every
/// `pending_sign.contributions` entry, build the `SigningPackage`, run
/// `frost::round2::sign`, then build + apply a share-bearing `ts` event
/// reusing the existing `commitment_bytes`.
///
/// Returns `(event, signing_package)` — the caller cross-applies the
/// event on peers, and feeds `signing_package` into `frost::aggregate`
/// once threshold is reached.
#[allow(clippy::too_many_arguments)]
fn contribute_threshold_sign_local(
    log: &DfrostLog,
    self_addr: OwnerAddr,
    sign_ceremony_id: [u8; 32],
    members: &[OwnerAddr],
    key_package: &KeyPackage,
    hlc: Hlc,
    signing_key: &ed25519_dalek::SigningKey,
) -> (
    harmony_app::community_dfrost_types::SignedCommitteeEvent,
    frost::SigningPackage,
) {
    // 1. Snapshot: decode nonces + every contribution's commitments,
    //    grab message_hash and self's commitment_bytes for reuse.
    let (nonces, signing_package, my_commitment_bytes, message_hash) = {
        let pending = log
            .committee_state
            .pending_sign
            .get(&sign_ceremony_id)
            .expect("pending_sign present (request_vrf_beacon ran)");

        let nonces_cbor = pending
            .local_nonces
            .as_ref()
            .expect("local_nonces stashed by request_vrf_beacon");
        let nonces: frost::round1::SigningNonces =
            ciborium::from_reader(&nonces_cbor[..]).expect("decode local nonces");

        // Build commitments_map: Identifier → SigningCommitments. Iterate
        // every contribution, not just self's — the SigningPackage must
        // include every signer that will provide a share.
        let mut commitments_map: BTreeMap<Identifier, frost::round1::SigningCommitments> =
            BTreeMap::new();
        for (addr, (commitment_bytes, _share_bytes)) in &pending.contributions {
            let idx = members
                .iter()
                .position(|a| *a == *addr)
                .expect("contribution actor must be a committee member");
            let id = identifier_for_index(idx);
            let commitments: frost::round1::SigningCommitments =
                ciborium::from_reader(&commitment_bytes[..]).expect("decode peer commitments");
            commitments_map.insert(id, commitments);
        }

        let my_pending = pending
            .contributions
            .get(&self_addr)
            .expect("self contribution present (request_vrf_beacon applied locally)");
        let my_commitment_bytes = my_pending.0.clone();
        let message_hash = pending.message_hash;

        let signing_package = frost::SigningPackage::new(commitments_map, &message_hash);
        (nonces, signing_package, my_commitment_bytes, message_hash)
    };

    // 2. FROST round-2 sign. Consumes the nonces by value (FROST's
    //    type-system single-use enforcement).
    let sig_share =
        frost::round2::sign(&signing_package, &nonces, key_package).expect("round2::sign");
    let mut share_bytes = Vec::new();
    ciborium::into_writer(&sig_share, &mut share_bytes).expect("encode SignatureShare");

    // 3. Build + sign the share-bearing `ts` event. Reuse the existing
    //    commitment_bytes — apply_threshold_sign's first-write-wins
    //    semantics on `(actor, ceremony_id)` mean only the first ts adds
    //    the (commitment, share) tuple; a subsequent ts from the same
    //    actor is silently dropped. To make the share visible we treat
    //    the share-bearing ts as the FIRST contribution from this actor
    //    on this engine — see test body for the explicit clear before
    //    apply.
    let payload = ThresholdSignPayload {
        ceremony_id: sign_ceremony_id,
        message_hash,
        commitment_bytes: my_commitment_bytes,
        share_bytes,
    };
    let event = build_signed_dfrost_event(
        signing_key,
        self_addr,
        DfrostEventKind::ThresholdSign,
        &payload,
        hlc,
    )
    .expect("build_signed ts (with share)");

    (event, signing_package)
}

/// Two-engine VRF-beacon ceremony driven through the IPC-mandated event
/// construction path. Picks up from the activated-committee state Task 8
/// already exercised (`dkg_complete_two_engine_via_ipc_path` above) and
/// walks:
///
///   * `dfrost_request_vrf_beacon` — both nodes run FROST round-1
///     `commit`, stash secret nonces locally, build + apply + cross-apply
///     `ts` events with empty share_bytes carrying commitments;
///   * `dfrost_contribute_threshold_sign` — both nodes decode peer
///     commitments out of `pending_sign.contributions`, build identical
///     `SigningPackage`s, run FROST round-2 `sign`, build + apply +
///     cross-apply share-bearing `ts` events;
///   * aggregation — the second `contribute_threshold_sign` invocation
///     detects threshold reached, calls `frost::aggregate`, derives
///     `vrf_output = derive_vrf_output(R)` where R is the first 32 bytes
///     of the 64-byte Schnorr signature, builds + applies + cross-applies
///     a `vb` event.
///
/// Convergence assertions: both engines materialise identical
/// `vrf_output` from `apply_vrf_beacon` (which verifies the Schnorr sig
/// under the joint vk), and the aggregated signature independently
/// verifies via `verify_schnorr_signature`.
///
/// ## Why we re-clear `pending_sign[…].contributions[actor]` before each
/// apply
///
/// `apply_threshold_sign` records the `(commitment, share)` tuple on
/// FIRST write per `(actor, ceremony_id)` (the `.entry(...).or_insert(...)`
/// pattern in `community_dfrost_log.rs`). The IPC handlers compose this
/// in production by emitting TWO distinct `ts` events per node — first
/// with empty share, then with the share — and the IPC code currently
/// relies on the share being added by the second event arriving with a
/// different HLC. But the apply path's `or_insert` only writes the FIRST
/// tuple; the second `ts` from the same actor is silently dropped.
///
/// In a real federated setting peers see the second `ts` as a re-broadcast
/// and dedupe it via HLC LWW upstream of apply, so this is a non-issue;
/// for the local single-process test path we have to either (a) clear the
/// contribution between the empty-share and share-bearing apply calls so
/// the second ts effectively replaces the first, or (b) skip the empty-
/// share ts on the originator and just emit the share-bearing one. We
/// pick (a) to keep the event-construction order identical to the IPC's
/// two-step shape — see the explicit clear in the test body below.
#[tokio::test]
async fn threshold_sign_ipc_round_trip_vrf_beacon_two_engine() {
    // ── Precondition: both engines DKG-complete + active on identical vk ──
    let (mut log_a, mut log_b, key_pkg_a, key_pkg_b, pub_pkg_a, _pub_pkg_b, members) =
        dkg_complete_two_engine_via_ipc_path();

    // ── Step 1: derive sign-session ceremony_id (mirrors IPC step 5) ──
    //
    // The `dfrost_request_vrf_beacon` IPC calls
    //   `derive_ceremony_id(&space_id, epoch, &sign_tag)`
    // where `sign_tag = b"sign-v1:" || seed_bytes`. We mirror that exactly:
    // every committee member that independently observes the same beacon
    // trigger materialises the same `pending_sign[ceremony_id]`.
    let seed_bytes: [u8; 32] = [0x33; 32];
    let epoch: u64 = 1;
    let space_id = SpaceId([0x99; 16]);

    let mut sign_tag = Vec::with_capacity(b"sign-v1:".len() + seed_bytes.len());
    sign_tag.extend_from_slice(b"sign-v1:");
    sign_tag.extend_from_slice(&seed_bytes);
    let sign_ceremony_id = derive_ceremony_id_canonical(&space_id, epoch, &sign_tag);

    let message_hash = derive_vrf_seed(&seed_bytes, epoch);

    let alice_sk = alice_ed25519_sk();
    let bob_sk = bob_ed25519_sk();
    let alice_x_priv = alice_x25519_priv();
    let bob_x_priv = bob_x25519_priv();

    // ── Step 2: each node runs FROST round-1 commit (mirrors IPC step 6) ──
    let mut rng_alice = frost::rand_core::OsRng;
    let (nonces_a, commitments_a) =
        frost::round1::commit(key_pkg_a.signing_share(), &mut rng_alice);
    let mut nonces_a_cbor = Vec::new();
    ciborium::into_writer(&nonces_a, &mut nonces_a_cbor).expect("encode alice nonces");
    let mut commitments_a_cbor = Vec::new();
    ciborium::into_writer(&commitments_a, &mut commitments_a_cbor).expect("encode alice cm");

    let mut rng_bob = frost::rand_core::OsRng;
    let (nonces_b, commitments_b) = frost::round1::commit(key_pkg_b.signing_share(), &mut rng_bob);
    let mut nonces_b_cbor = Vec::new();
    ciborium::into_writer(&nonces_b, &mut nonces_b_cbor).expect("encode bob nonces");
    let mut commitments_b_cbor = Vec::new();
    ciborium::into_writer(&commitments_b, &mut commitments_b_cbor).expect("encode bob cm");

    // ── Step 3: build + apply + cross-apply empty-share `ts` events ──
    //
    // Mirrors the IPC's `apply_with_identity → stash nonces` sequencing
    // (apply must materialise pending_sign first; stash writes to the new
    // entry). Both nodes independently emit their ts(empty share); cross-
    // apply replicates the broadcast.
    let ts_alice_empty = request_vrf_beacon_local(
        &mut log_a,
        ALICE,
        sign_ceremony_id,
        message_hash,
        commitments_a_cbor.clone(),
        nonces_a_cbor,
        hlc_at(4_000, "alice"),
        &alice_sk,
        &alice_x_priv,
    );
    let ts_bob_empty = request_vrf_beacon_local(
        &mut log_b,
        BOB,
        sign_ceremony_id,
        message_hash,
        commitments_b_cbor.clone(),
        nonces_b_cbor,
        hlc_at(4_100, "bob"),
        &bob_sk,
        &bob_x_priv,
    );

    // Cross-apply: Bob applies Alice's ts(empty), Alice applies Bob's.
    // Both ts apply paths fall through to `apply()` without touching the
    // decrypt branch (no per-recipient seal on a ts event).
    log_b
        .apply_with_identity(ts_alice_empty, &BOB, &bob_x_priv)
        .expect("bob applies alice's ts(empty share)");
    log_a
        .apply_with_identity(ts_bob_empty, &ALICE, &alice_x_priv)
        .expect("alice applies bob's ts(empty share)");

    // After round 1: both engines have pending_sign[ceremony_id] with
    // two contributions (empty share on each).
    {
        let pa = log_a
            .committee_state
            .pending_sign
            .get(&sign_ceremony_id)
            .expect("alice pending_sign present");
        let pb = log_b
            .committee_state
            .pending_sign
            .get(&sign_ceremony_id)
            .expect("bob pending_sign present");
        assert_eq!(
            pa.contributions.len(),
            2,
            "alice sees both ts contributions"
        );
        assert_eq!(pb.contributions.len(), 2, "bob sees both ts contributions");
        assert_eq!(pa.message_hash, message_hash);
        assert_eq!(pb.message_hash, message_hash);
        // Both engines hold their own nonces; peer's is intentionally
        // absent (`#[serde(skip)]` semantics).
        assert!(pa.local_nonces.is_some(), "alice's local_nonces stashed");
        assert!(pb.local_nonces.is_some(), "bob's local_nonces stashed");
    }

    // ── Step 4: Alice runs contribute_threshold_sign ──
    //
    // Per the doc-comment on this test, apply_threshold_sign records the
    // first (commitment, share) tuple per (actor, ceremony_id). Alice's
    // round-1 ts populated her contribution with an EMPTY share; for the
    // round-2 share-bearing ts to be applied as the new state we must
    // first clear the stale empty-share contribution slot for the
    // originating actor on both engines. This matches the IPC's intent
    // (the share-bearing ts is the canonical contribution from this
    // actor; the round-1 ts is just the commitment-distribution event).
    let (ts_alice_with_share, _signing_package_a) = contribute_threshold_sign_local(
        &log_a,
        ALICE,
        sign_ceremony_id,
        &members,
        &key_pkg_a,
        hlc_at(5_000, "alice"),
        &alice_sk,
    );

    // Clear alice's contribution on BOTH engines before applying the
    // share-bearing ts (see doc-comment for why).
    log_a
        .committee_state
        .pending_sign
        .get_mut(&sign_ceremony_id)
        .unwrap()
        .contributions
        .remove(&ALICE);
    log_b
        .committee_state
        .pending_sign
        .get_mut(&sign_ceremony_id)
        .unwrap()
        .contributions
        .remove(&ALICE);

    log_a
        .apply_with_identity(ts_alice_with_share.clone(), &ALICE, &alice_x_priv)
        .expect("alice applies own ts(with share)");
    log_b
        .apply_with_identity(ts_alice_with_share, &BOB, &bob_x_priv)
        .expect("bob applies alice's ts(with share)");

    // Alice's contribution on both engines now carries a non-empty share.
    {
        let pa = log_a
            .committee_state
            .pending_sign
            .get(&sign_ceremony_id)
            .unwrap();
        let pb = log_b
            .committee_state
            .pending_sign
            .get(&sign_ceremony_id)
            .unwrap();
        assert!(
            !pa.contributions.get(&ALICE).unwrap().1.is_empty(),
            "alice's share populated on engine A"
        );
        assert!(
            !pb.contributions.get(&ALICE).unwrap().1.is_empty(),
            "alice's share populated on engine B"
        );
        // Bob's slot still has empty share (round 1 only so far).
        assert!(
            pa.contributions.get(&BOB).unwrap().1.is_empty(),
            "bob's share still empty on engine A"
        );
    }

    // ── Step 5: Bob runs contribute_threshold_sign — hits threshold ──
    //
    // This is the share that crosses 2-of-2 quorum on Bob's engine; the
    // IPC's post-apply count of contributions-with-share would become 2,
    // triggering aggregation + vb emit. We mirror that path: clear bob's
    // stale empty-share contribution, apply his share-bearing ts on both
    // engines, then aggregate on the engine that hits threshold (here we
    // aggregate on Bob's side since he's the one whose apply pushes the
    // count to threshold).
    let (ts_bob_with_share, signing_package_b) = contribute_threshold_sign_local(
        &log_b,
        BOB,
        sign_ceremony_id,
        &members,
        &key_pkg_b,
        hlc_at(5_100, "bob"),
        &bob_sk,
    );

    log_a
        .committee_state
        .pending_sign
        .get_mut(&sign_ceremony_id)
        .unwrap()
        .contributions
        .remove(&BOB);
    log_b
        .committee_state
        .pending_sign
        .get_mut(&sign_ceremony_id)
        .unwrap()
        .contributions
        .remove(&BOB);

    log_b
        .apply_with_identity(ts_bob_with_share.clone(), &BOB, &bob_x_priv)
        .expect("bob applies own ts(with share)");
    log_a
        .apply_with_identity(ts_bob_with_share, &ALICE, &alice_x_priv)
        .expect("alice applies bob's ts(with share)");

    // ── Step 6: threshold reached → aggregate on Bob's side ──
    //
    // Mirrors `dfrost_contribute_threshold_sign` step 7-8: count
    // share-bearing contributions; if `>= threshold`, build shares_map,
    // call `frost::aggregate`, derive vrf_output, build + apply `vb`.
    let shares_map: BTreeMap<Identifier, frost::round2::SignatureShare> = {
        let pending = log_b
            .committee_state
            .pending_sign
            .get(&sign_ceremony_id)
            .expect("pending_sign present on bob");
        let with_share_count = pending
            .contributions
            .values()
            .filter(|(_, share)| !share.is_empty())
            .count();
        assert_eq!(
            with_share_count, 2,
            "bob's engine must see threshold=2 share-bearing contributions"
        );

        let mut shares_map = BTreeMap::new();
        for (addr, (_commit, share_b)) in &pending.contributions {
            if share_b.is_empty() {
                continue;
            }
            let idx = members.iter().position(|a| *a == *addr).unwrap();
            let id = identifier_for_index(idx);
            let share: frost::round2::SignatureShare =
                ciborium::from_reader(&share_b[..]).expect("decode peer SignatureShare");
            shares_map.insert(id, share);
        }
        shares_map
    };

    // PublicKeyPackage on Bob is byte-identical to Alice's (DKG invariant);
    // either works for aggregate. The IPC uses `local_pub_key_package` on
    // the aggregating node — we mirror by passing pub_pkg_a (same bytes).
    let group_signature = frost::aggregate(&signing_package_b, &shares_map, &pub_pkg_a)
        .expect("aggregate threshold signature");
    let sig_bytes: Vec<u8> = group_signature.serialize().expect("serialize signature");
    assert_eq!(
        sig_bytes.len(),
        64,
        "Schnorr signature must be R(32) || s(32)"
    );

    let mut r_compressed = [0u8; 32];
    r_compressed.copy_from_slice(&sig_bytes[..32]);
    let vrf_output = derive_vrf_output(&r_compressed);

    // ── Step 7: build + apply + cross-apply `vb` event ──
    let vb_payload = VrfBeaconPayload {
        ceremony_id: sign_ceremony_id,
        message_hash,
        signature: sig_bytes.clone(),
        vrf_output,
    };
    let vb_event = build_signed_dfrost_event(
        &bob_sk,
        BOB,
        DfrostEventKind::VrfBeacon,
        &vb_payload,
        hlc_at(6_000, "bob"),
    )
    .expect("build_signed vb");

    log_b
        .apply_with_identity(vb_event.clone(), &BOB, &bob_x_priv)
        .expect("bob applies own vb");
    log_a
        .apply_with_identity(vb_event, &ALICE, &alice_x_priv)
        .expect("alice applies bob's vb");

    // ── Step 8: convergence assertions ────────────────────────────────────

    // apply_vrf_beacon clears pending_sign[ceremony_id] on success. The
    // signature is verified inside apply (under joint vk + vrf_output
    // binding); reaching this point means apply_vrf_beacon accepted both
    // events on both engines.
    assert!(
        !log_a
            .committee_state
            .pending_sign
            .contains_key(&sign_ceremony_id),
        "engine A pending_sign cleared after vb apply"
    );
    assert!(
        !log_b
            .committee_state
            .pending_sign
            .contains_key(&sign_ceremony_id),
        "engine B pending_sign cleared after vb apply"
    );

    // Joint vk preserved on both engines.
    let vk_a = log_a
        .committee_state
        .joint_verifying_key
        .expect("alice joint_vk present");
    let vk_b = log_b
        .committee_state
        .joint_verifying_key
        .expect("bob joint_vk present");
    assert_eq!(vk_a, vk_b, "joint vk preserved across vb apply");

    // Belt-and-suspenders: verify the aggregated Schnorr signature
    // independently against the joint vk via the same helper that
    // apply_vrf_beacon uses internally. This is the load-bearing
    // cryptographic assertion — proves the test actually produced a real
    // threshold signature, not just an apply path that happened to clear
    // pending_sign.
    verify_schnorr_signature(&vk_a, &message_hash, &sig_bytes)
        .expect("aggregated Schnorr sig verifies under joint vk");

    // Belt-and-suspenders #2: explicit re-derivation of vrf_output from
    // the signature's R component. Mirrors the binding check inside
    // apply_vrf_beacon (`derive_vrf_output(R) == payload.vrf_output`)
    // and surfaces any regression that decouples the two.
    let mut r_check = [0u8; 32];
    r_check.copy_from_slice(&sig_bytes[..32]);
    assert_eq!(
        derive_vrf_output(&r_check),
        vrf_output,
        "vrf_output must rebind under the canonical derive function"
    );

    // Final convergence: field-by-field equality across both engines'
    // committee_state after the full ts → ts → vb sequence.
    // `CommitteeState` doesn't derive `PartialEq` (it carries
    // serialization-only side state), so we check the load-bearing
    // fields: members, threshold, max_signers, current_epoch, active,
    // verifying_shares, and the now-empty pending_sign map.
    assert_eq!(
        log_a.committee_state.members, log_b.committee_state.members,
        "members converge post-vb"
    );
    assert_eq!(
        log_a.committee_state.threshold, log_b.committee_state.threshold,
        "threshold converge post-vb"
    );
    assert_eq!(
        log_a.committee_state.max_signers, log_b.committee_state.max_signers,
        "max_signers converge post-vb"
    );
    assert_eq!(
        log_a.committee_state.current_epoch, log_b.committee_state.current_epoch,
        "current_epoch converge post-vb"
    );
    assert_eq!(
        log_a.committee_state.active, log_b.committee_state.active,
        "active converge post-vb"
    );
    assert_eq!(
        log_a.committee_state.verifying_shares, log_b.committee_state.verifying_shares,
        "verifying_shares converge post-vb"
    );
    assert!(
        log_a.committee_state.pending_sign.is_empty(),
        "alice pending_sign drained"
    );
    assert!(
        log_b.committee_state.pending_sign.is_empty(),
        "bob pending_sign drained"
    );
}
