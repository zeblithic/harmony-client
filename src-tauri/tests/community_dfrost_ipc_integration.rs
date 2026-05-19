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
    verifying_key_to_bytes, verifying_share_to_bytes,
};
use harmony_app::community_dfrost_log::{
    build_signed_dfrost_event, CommitteeState, DfrostLog, PendingCeremony,
};
use harmony_app::community_dfrost_types::{
    DfrostEventKind, DkgCompletePayload, DkgRoundPayload, MemberVerifyingShare,
};
use harmony_app::community_membership::RecipientCiphertext;
use harmony_app::dm_signing;
use harmony_app::owner_state_types::{Hlc, OwnerAddr};
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
