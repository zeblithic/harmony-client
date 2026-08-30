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
    dkg_part1_local, dkg_part2_local, dkg_part3_local, identifier_for_index, refresh_part1_local,
    refresh_part2_local, refresh_part3_local, verify_schnorr_signature, verifying_key_to_bytes,
    verifying_share_to_bytes,
};
use harmony_app::community_dfrost_log::{
    build_signed_dfrost_event, CommitteeState, DfrostLog, PendingCeremony,
};
use harmony_app::community_dfrost_types::{
    derive_ceremony_id as derive_ceremony_id_canonical, derive_refresh_ceremony_id,
    derive_vrf_output, derive_vrf_seed, DfrostEventKind, DkgCompletePayload, DkgRoundPayload,
    MemberVerifyingShare, RefreshRoundPayload, ThresholdSignPayload, VrfBeaconPayload,
};
use harmony_app::community_membership::RecipientCiphertext;
use harmony_app::dm_signing;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use x25519_dalek::{PublicKey, StaticSecret};

// ─── Deterministic 2-member committee fixtures ─────────────────────────────

const ALICE: OwnerAddr = OwnerAddr([0x01; 16]);
const BOB: OwnerAddr = OwnerAddr([0x02; 16]);
// Test-wide community handle used by ceremony_id derivation. The actual
// bytes don't matter for the test contract; what matters is that BOTH
// engines hash the SAME space_id into the ceremony_id (so they converge
// on the same id) and that the helper mirrors the IPC's derivation
// shape (R1 round-1 bot-review MAJOR: ceremony_id now includes
// hlc.logical + space_id).
const TEST_SPACE_ID: SpaceId = SpaceId([0x55; 16]);

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
/// `blake3(sorted_members || threshold_le || hlc.wall_ms_le ||
/// hlc.logical_le || space_id)`.
/// Lifted verbatim from `dfrost_initiate_dkg` step 6.
fn derive_ceremony_id(
    members: &[OwnerAddr],
    threshold: u16,
    hlc_wall_ms: u64,
    hlc_logical: u32,
    space_id: &SpaceId,
) -> [u8; 32] {
    let mut hasher_input: Vec<u8> = Vec::with_capacity(members.len() * 16 + 2 + 8 + 4 + 16);
    for a in members {
        hasher_input.extend_from_slice(&a.0);
    }
    hasher_input.extend_from_slice(&threshold.to_le_bytes());
    hasher_input.extend_from_slice(&hlc_wall_ms.to_le_bytes());
    hasher_input.extend_from_slice(&hlc_logical.to_le_bytes());
    hasher_input.extend_from_slice(&space_id.0);
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
        space_id: None,
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
    let ceremony_id = derive_ceremony_id(
        &members,
        threshold,
        alice_init_hlc.wall_ms,
        alice_init_hlc.logical,
        &TEST_SPACE_ID,
    );

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
    let ceremony_id = derive_ceremony_id(
        &members,
        threshold,
        alice_init_hlc.wall_ms,
        alice_init_hlc.logical,
        &TEST_SPACE_ID,
    );

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
/// decode stashed nonces, build the `commitments_map` from the canonical
/// signing set (first `threshold` peers by sorted OwnerAddr among
/// contributors — R4 Greptile P1 fix), build the `SigningPackage`, run
/// `frost::round2::sign`, then build + apply a share-bearing `ts` event
/// reusing the existing `commitment_bytes`.
///
/// Returns `(event, signing_package)` — the caller cross-applies the
/// event on peers, and feeds the SAME `signing_package` into
/// `frost::aggregate` once threshold is reached (R4 critical: signers +
/// aggregator must use byte-identical SigningPackages or FROST shares
/// won't verify under the recomputed Fiat-Shamir challenge).
#[allow(clippy::too_many_arguments)]
fn contribute_threshold_sign_local(
    // R5-5 (MAJOR test): take `&mut DfrostLog` so we can `.take()` the
    // stashed `local_nonces` — mirrors the production IPC's
    // `Option::take()` single-use invariant (R2 atomic-take fix). With a
    // shared `&DfrostLog` ref the helper used to borrow nonces by `&`
    // and leave them in place, so a buggy test path could effectively
    // reuse the same nonces twice even though production refuses to.
    // FROST §6.2: nonce reuse leaks the secret signing share.
    log: &mut DfrostLog,
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
    let threshold = log.committee_state.threshold;
    // 1. Snapshot: decode nonces + the canonical-signing-set's commitments,
    //    grab message_hash and self's commitment_bytes for reuse.
    let (nonces, signing_package, my_commitment_bytes, message_hash) = {
        let pending = log
            .committee_state
            .pending_sign
            .get_mut(&sign_ceremony_id)
            .expect("pending_sign present (request_vrf_beacon ran)");

        // R5-5: consume the stashed nonces via Option::take() so a
        // second call to this helper for the same ceremony surfaces a
        // missing-nonces panic instead of silently re-signing with
        // already-consumed nonces.
        let nonces_cbor = pending
            .local_nonces
            .take()
            .expect("local_nonces stashed by request_vrf_beacon");
        let nonces: frost::round1::SigningNonces =
            ciborium::from_reader(&nonces_cbor[..]).expect("decode local nonces");

        // R4 Greptile P1 fix: build commitments_map from the canonical
        // signing set — first `threshold` peers by sorted OwnerAddr
        // (BTreeMap iteration is sorted by key). Every signer + the
        // aggregator computes the SAME set deterministically, so every
        // party's SigningPackage is byte-identical — Fiat-Shamir
        // challenge matches across signers and the aggregator.
        let signing_set: Vec<OwnerAddr> = pending
            .contributions
            .keys()
            .copied()
            .take(threshold as usize)
            .collect();
        assert!(
            signing_set.contains(&self_addr),
            "test helper requires self to be in canonical signing set"
        );

        let mut commitments_map: BTreeMap<Identifier, frost::round1::SigningCommitments> =
            BTreeMap::new();
        for addr in &signing_set {
            let (commitment_bytes, _share_bytes) = pending
                .contributions
                .get(addr)
                .expect("canonical signer present (derived from contributions.keys())");
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
/// ## Empty-then-filled `ts` upsert (apply_threshold_sign R1 fix)
///
/// `apply_threshold_sign` UPSERTS the `(commitment, share)` tuple per
/// `(actor, ceremony_id)`: round-1 ts (empty share) inserts; round-2 ts
/// (filled share, same commitment) updates the share_bytes in place.
/// Reuse of a filled share or commitment swap is rejected as
/// `InvariantViolation` — see the unit tests
/// `ts_round2_filled_share_upserts_over_round1_empty_share` /
/// `ts_second_filled_share_with_different_bytes_rejected` /
/// `ts_with_mismatched_commitment_bytes_rejected` /
/// `ts_late_empty_share_does_not_downgrade_existing_filled_share` in
/// `community_dfrost_log.rs`. The test follows the natural two-step
/// IPC flow without any direct mutation of `pending_sign.contributions`.
#[tokio::test]
async fn threshold_sign_ipc_round_trip_vrf_beacon_two_engine() {
    // ── Precondition: both engines DKG-complete + active on identical vk ──
    let (mut log_a, mut log_b, key_pkg_a, key_pkg_b, _pub_pkg_a, pub_pkg_b, members) =
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
    // Alice's round-1 ts populated her contribution with an EMPTY share;
    // her round-2 share-bearing ts upserts the share_bytes in place via
    // `apply_threshold_sign`'s upsert path (R1 fix). No direct mutation
    // of `pending_sign.contributions` needed.
    let (ts_alice_with_share, _signing_package_a) = contribute_threshold_sign_local(
        &mut log_a,
        ALICE,
        sign_ceremony_id,
        &members,
        &key_pkg_a,
        hlc_at(5_000, "alice"),
        &alice_sk,
    );

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
    // IPC's post-apply count of contributions-with-share becomes 2,
    // triggering aggregation + vb emit. Bob's round-2 ts upserts over
    // his round-1 empty-share contribution via `apply_threshold_sign`
    // (R1 fix). Aggregate on Bob's side since he's the one whose apply
    // pushes the count to threshold.
    let (ts_bob_with_share, signing_package_b) = contribute_threshold_sign_local(
        &mut log_b,
        BOB,
        sign_ceremony_id,
        &members,
        &key_pkg_b,
        hlc_at(5_100, "bob"),
        &bob_sk,
    );

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
    // the aggregating node — Bob aggregates here, so we use Bob's
    // pub_pkg_b (R1 round-1 bot-review MAJOR: previously this passed
    // pub_pkg_a, which happened to work because both engines converge
    // on identical PublicKeyPackage bytes after DKG, but the assertion
    // shape was wrong — the aggregating node must use its OWN copy).
    let group_signature = frost::aggregate(&signing_package_b, &shares_map, &pub_pkg_b)
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

// ─── Task 10 (reworked for ZEB-1027): proactive-refresh COMPLETION ─────────

/// Replicate `dfrost_propose_refresh` (rn=1) for one node: run the
/// zero-sharing `refresh_part1_local`, broadcast the PUBLIC round-1
/// package in an `rf` rn=1 event, stash `local_dkg_secret`, apply
/// locally. Returns the signed event for cross-application.
///
/// Pre-ZEB-1027 this helper sealed a `dkg::part1` package per recipient
/// (the placeholder shape); the real protocol's rn=1 is a public
/// commitment, exactly like `dr` rn=1.
#[allow(clippy::too_many_arguments)]
fn propose_refresh_local(
    log: &mut DfrostLog,
    self_addr: OwnerAddr,
    self_id: Identifier,
    threshold: u16,
    max_signers: u16,
    refresh_ceremony_id: [u8; 32],
    hlc: Hlc,
    signing_key: &ed25519_dalek::SigningKey,
    self_x25519_priv: &[u8; 32],
) -> harmony_app::community_dfrost_types::SignedCommitteeEvent {
    let (r1_secret, r1_pkg_bytes) =
        refresh_part1_local(self_id, max_signers, threshold).expect("refresh_part1_local");
    let payload = RefreshRoundPayload {
        ceremony_id: refresh_ceremony_id,
        round_num: 1,
        recipient_ciphertexts: None,
        package: Some(r1_pkg_bytes),
        attempt: 0,
    };
    let event = build_signed_dfrost_event(
        signing_key,
        self_addr,
        DfrostEventKind::ProactiveRefresh,
        &payload,
        hlc,
    )
    .expect("build_signed rf rn=1");
    // Stash BEFORE apply (matches the IPC's loss-of-secret ordering).
    log.local_dkg_secret = Some(r1_secret);
    log.apply_with_identity(event.clone(), &self_addr, self_x25519_priv)
        .expect("apply own rf rn=1");
    event
}

/// ZEB-1027: full two-engine proactive-refresh round-trip through the
/// IPC-mandated event shapes — proposal (rn=1 public zero-sharing
/// commitments from BOTH members converging on the deterministic
/// `derive_refresh_ceremony_id`), rn=2 sealed share distribution,
/// `refresh_part3_local` finalization, and the `dk` completion that
/// advances the epoch while PRESERVING the joint verifying key. Ends
/// with a real 2-of-2 FROST signature under the ROTATED shares
/// verifying against the ORIGINAL vk — the refresh contract end-to-end.
#[tokio::test]
async fn refresh_ipc_round_trip_completes_and_preserves_joint_vk() {
    let (mut log_a, mut log_b, key_pkg_a, key_pkg_b, pub_pkg_a, pub_pkg_b, members) =
        dkg_complete_two_engine_via_ipc_path();

    let threshold: u16 = 2;
    let max_signers: u16 = 2;

    let joint_vk_before = log_a
        .committee_state
        .joint_verifying_key
        .expect("joint_vk materialised by DKG completion");
    let epoch_before = log_a.committee_state.current_epoch;
    let verifying_shares_before = log_a.committee_state.verifying_shares.clone();
    assert_eq!(
        log_b.committee_state.joint_verifying_key,
        Some(joint_vk_before),
        "engines must agree on joint_vk before refresh starts"
    );

    // The finalization consumes the OLD signing share; make sure both
    // logs hold theirs (the IPC's DKG rn=3 stashes them the same way).
    log_a.local_key_package = Some(key_pkg_a.clone());
    log_b.local_key_package = Some(key_pkg_b.clone());

    let id_alice = identifier_for_index(0);
    let id_bob = identifier_for_index(1);
    let alice_sk = alice_ed25519_sk();
    let bob_sk = bob_ed25519_sk();
    let alice_x_priv = alice_x25519_priv();
    let bob_x_priv = bob_x25519_priv();

    let proposed_epoch = epoch_before + 1;
    let refresh_ceremony_id =
        derive_refresh_ceremony_id(&members, threshold, proposed_epoch, 0, &TEST_SPACE_ID);

    // ── rn=1 from both members (R9: shared deterministic ceremony) ───────
    let rf1_alice = propose_refresh_local(
        &mut log_a,
        ALICE,
        id_alice,
        threshold,
        max_signers,
        refresh_ceremony_id,
        hlc_at(7_000, "alice"),
        &alice_sk,
        &alice_x_priv,
    );
    log_b
        .apply_with_identity(rf1_alice, &BOB, &bob_x_priv)
        .expect("bob applies alice's rf rn=1");
    let rf1_bob = propose_refresh_local(
        &mut log_b,
        BOB,
        id_bob,
        threshold,
        max_signers,
        refresh_ceremony_id,
        hlc_at(7_100, "bob"),
        &bob_sk,
        &bob_x_priv,
    );
    log_a
        .apply_with_identity(rf1_bob, &ALICE, &alice_x_priv)
        .expect("alice applies bob's rf rn=1");

    // Proposal rounds must not touch the active identity.
    assert_eq!(
        log_a.committee_state.joint_verifying_key,
        Some(joint_vk_before)
    );
    assert_eq!(log_a.committee_state.current_epoch, epoch_before);

    // ── rn=2: refresh part2 over the peer's round-1 package, sealed ──────
    let recipient_pubs: BTreeMap<OwnerAddr, [u8; 32]> =
        [(ALICE, alice_x25519_pub()), (BOB, bob_x25519_pub())]
            .into_iter()
            .collect();
    let mut r2_secrets: BTreeMap<OwnerAddr, frost::keys::dkg::round2::SecretPackage> =
        BTreeMap::new();
    for (me, sk, wall) in [(ALICE, &alice_sk, 7_200u64), (BOB, &bob_sk, 7_300)] {
        let (r1_secret, r1_others) = {
            let log = if me == ALICE { &mut log_a } else { &mut log_b };
            let pending = log
                .committee_state
                .pending_refresh
                .as_ref()
                .expect("pending_refresh");
            let others: BTreeMap<Identifier, Vec<u8>> = pending
                .round1_packages
                .iter()
                .filter(|(a, _)| **a != me)
                .map(|(a, b)| {
                    let idx = members.iter().position(|m| m == a).unwrap();
                    (identifier_for_index(idx), b.clone())
                })
                .collect();
            (
                log.local_dkg_secret.take().expect("r1 secret stashed"),
                others,
            )
        };
        let (r2_secret, r2_out) =
            refresh_part2_local(r1_secret, &r1_others).expect("refresh part2");
        r2_secrets.insert(me, r2_secret);
        let mut cts = Vec::new();
        for (rid, pkg) in &r2_out {
            let addr = if *rid == id_alice { ALICE } else { BOB };
            cts.push(RecipientCiphertext {
                recipient: addr,
                sealed: dm_signing::seal_to_owner(recipient_pubs.get(&addr).unwrap(), pkg)
                    .expect("seal r2"),
            });
        }
        let ev = build_signed_dfrost_event(
            sk,
            me,
            DfrostEventKind::ProactiveRefresh,
            &RefreshRoundPayload {
                ceremony_id: refresh_ceremony_id,
                round_num: 2,
                recipient_ciphertexts: Some(cts),
                package: None,
                attempt: 0,
            },
            hlc_at(wall, if me == ALICE { "alice" } else { "bob" }),
        )
        .expect("build rf rn=2");
        log_a
            .apply_with_identity(ev.clone(), &ALICE, &alice_x_priv)
            .expect("a applies rf rn=2");
        log_b
            .apply_with_identity(ev, &BOB, &bob_x_priv)
            .expect("b applies rf rn=2");
    }

    // ── finalization: refresh_part3_local per member ─────────────────────
    let mut rotated: BTreeMap<OwnerAddr, (KeyPackage, PublicKeyPackage)> = BTreeMap::new();
    for me in [ALICE, BOB] {
        let log = if me == ALICE { &log_a } else { &log_b };
        let pending = log
            .committee_state
            .pending_refresh
            .as_ref()
            .expect("pending_refresh");
        let r1_others: BTreeMap<Identifier, Vec<u8>> = pending
            .round1_packages
            .iter()
            .filter(|(a, _)| **a != me)
            .map(|(a, b)| {
                let idx = members.iter().position(|m| m == a).unwrap();
                (identifier_for_index(idx), b.clone())
            })
            .collect();
        let r2_others: BTreeMap<Identifier, Vec<u8>> = pending
            .round2_packages
            .iter()
            .map(|(a, b)| {
                let idx = members.iter().position(|m| m == a).unwrap();
                (identifier_for_index(idx), b.clone())
            })
            .collect();
        let old_kp = if me == ALICE {
            key_pkg_a.clone()
        } else {
            key_pkg_b.clone()
        };
        // CR-5 (#775 round 1): each member feeds its OWN stashed public
        // package, matching the production path (byte-identical after
        // DKG, but the assertion shape should not depend on that).
        let old_pkp = if me == ALICE {
            pub_pkg_a.clone()
        } else {
            pub_pkg_b.clone()
        };
        let (new_kp, new_pkp) = refresh_part3_local(
            r2_secrets.get(&me).unwrap(),
            &r1_others,
            &r2_others,
            old_pkp,
            old_kp.clone(),
        )
        .expect("refresh part3");
        assert_eq!(
            verifying_key_to_bytes(new_pkp.verifying_key()),
            joint_vk_before,
            "zero-sharing preserves the joint verifying key"
        );
        assert_ne!(
            old_kp.signing_share().serialize(),
            new_kp.signing_share().serialize(),
            "the signing share must rotate"
        );
        rotated.insert(me, (new_kp, new_pkp));
    }

    // ── dk completion from both members ──────────────────────────────────
    let (_, new_pkp) = rotated.get(&ALICE).unwrap();
    let mut verifying_shares = Vec::new();
    for (i, member) in members.iter().enumerate() {
        let vs = new_pkp
            .verifying_shares()
            .get(&identifier_for_index(i))
            .unwrap();
        verifying_shares.push(MemberVerifyingShare {
            member: *member,
            verifying_share: verifying_share_to_bytes(vs),
        });
    }
    let dk_payload = DkgCompletePayload {
        ceremony_id: refresh_ceremony_id,
        joint_verifying_key: joint_vk_before,
        verifying_shares,
        epoch: proposed_epoch,
        members: members.clone(),
        threshold,
        max_signers,
        space_id: None,
    };
    // CR-6 (#775 round 1): re-stash round secrets immediately before the
    // dk events so the post-promotion assertion below proves PROMOTION
    // (not the test's own `.take()` during rn=2) cleared them. Both
    // secrets (#775 round 2 follow-up): `local_dkg_secret2` must be
    // populated too, or its half of the assertion can never fail.
    for (log, id, me) in [(&mut log_a, id_alice, ALICE), (&mut log_b, id_bob, BOB)] {
        let (r1_secret, _) =
            refresh_part1_local(id, max_signers, threshold).expect("re-stash r1 secret");
        log.local_dkg_secret = Some(r1_secret);
        log.local_dkg_secret2 = Some(r2_secrets.get(&me).unwrap().clone());
    }
    for (actor, sk, wall) in [(ALICE, &alice_sk, 7_400u64), (BOB, &bob_sk, 7_500)] {
        let dk = build_signed_dfrost_event(
            sk,
            actor,
            DfrostEventKind::DkgComplete,
            &dk_payload,
            hlc_at(wall, if actor == ALICE { "alice" } else { "bob" }),
        )
        .expect("build dk");
        log_a
            .apply_with_identity(dk.clone(), &ALICE, &alice_x_priv)
            .expect("a applies dk");
        log_b
            .apply_with_identity(dk, &BOB, &bob_x_priv)
            .expect("b applies dk");
    }

    for log in [&log_a, &log_b] {
        assert!(log.committee_state.active);
        assert_eq!(
            log.committee_state.current_epoch, proposed_epoch,
            "epoch advanced at completion"
        );
        assert_eq!(
            log.committee_state.joint_verifying_key,
            Some(joint_vk_before),
            "refresh completion preserves the joint vk"
        );
        assert_ne!(
            log.committee_state.verifying_shares, verifying_shares_before,
            "per-member verifying shares rotate at completion"
        );
        assert!(log.committee_state.pending_refresh.is_none());
        assert!(
            log.local_dkg_secret.is_none() && log.local_dkg_secret2.is_none(),
            "promotion clears the round secrets (ZEB-1027)"
        );
    }

    // ── rotated shares SIGN under the original joint vk ──────────────────
    let mut rng = frost::rand_core::OsRng;
    let mut nonces = BTreeMap::new();
    let mut commitments = BTreeMap::new();
    for (me, id) in [(ALICE, id_alice), (BOB, id_bob)] {
        let (kp, _) = rotated.get(&me).unwrap();
        let (n, c) = frost::round1::commit(kp.signing_share(), &mut rng);
        nonces.insert(id, n);
        commitments.insert(id, c);
    }
    let msg = b"zeb1027 refresh completion";
    let signing_package = frost::SigningPackage::new(commitments, msg);
    let mut shares = BTreeMap::new();
    for (me, id) in [(ALICE, id_alice), (BOB, id_bob)] {
        let (kp, _) = rotated.get(&me).unwrap();
        shares.insert(
            id,
            frost::round2::sign(&signing_package, nonces.get(&id).unwrap(), kp).expect("sign"),
        );
    }
    let sig = frost::aggregate(&signing_package, &shares, &rotated.get(&ALICE).unwrap().1)
        .expect("aggregate");
    verify_schnorr_signature(
        &joint_vk_before,
        msg,
        &sig.serialize().expect("sig serialize"),
    )
    .expect("rotated-share signature verifies under the ORIGINAL joint vk");
}

// ─── R4-1 regression: threshold-sign with `threshold < max_signers` ────────
//
// The R4 (round-4 Greptile P1 CRITICAL) bug: `round2::sign` was called with
// a `SigningPackage` built from EVERY contribution in `pending.contributions`
// (full set), but `frost::aggregate` rebuilt a SMALLER `selection_signing_package`
// from the first `threshold` non-empty-share contributions. FROST partial
// signature shares bind to the package's Fiat-Shamir challenge
// `c = H(R || M || X)`, where R is the aggregate commitment over the
// signing set; a different signing set yields a different R, a different c,
// and `s_aggregate != R + c · X` — aggregate fails verification.
//
// Invisible at 2-of-2 because selection == full set, but breaks every
// real-world threshold scheme. This test exercises 2-of-3 to lock the fix
// in: BEFORE R4 fix this test FAILS at `frost::aggregate`; AFTER R4 fix it
// PASSES because every signer + the aggregator builds the SAME signing
// package from the canonical signing set (first `threshold` sorted-addr
// contributors).

const CAROL: OwnerAddr = OwnerAddr([0x03; 16]);

fn carol_ed25519_sk() -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[0xCCu8; 32])
}
fn carol_x25519_priv() -> [u8; 32] {
    *dm_signing::ed25519_priv_to_x25519(&carol_ed25519_sk())
}
fn carol_x25519_pub() -> [u8; 32] {
    *PublicKey::from(&StaticSecret::from(carol_x25519_priv())).as_bytes()
}

fn sorted_members_3() -> Vec<OwnerAddr> {
    // ALICE < BOB < CAROL byte-wise — sorted order matches the IPC's
    // `member_addrs.sort()`. Canonical signing set for threshold=2 is
    // therefore {ALICE, BOB}: the first two by sorted OwnerAddr.
    vec![ALICE, BOB, CAROL]
}

/// 3-engine 2-of-3 DKG ceremony via the IPC event-construction path.
/// Mirrors `dkg_complete_two_engine_via_ipc_path()` but adds a third
/// member; returns all three engines + their KeyPackages + a shared
/// PublicKeyPackage (every DKG member converges on byte-identical pub
/// pkg per the DKG invariant) + the sorted member list.
#[allow(clippy::type_complexity)]
fn dkg_complete_three_engine_2of3_via_ipc_path() -> (
    DfrostLog,
    DfrostLog,
    DfrostLog,
    KeyPackage,
    KeyPackage,
    KeyPackage,
    PublicKeyPackage,
    Vec<OwnerAddr>,
) {
    let mut log_a = DfrostLog::new();
    let mut log_b = DfrostLog::new();
    let mut log_c = DfrostLog::new();

    let members = sorted_members_3();
    let threshold: u16 = 2;
    let max_signers: u16 = 3;
    let proposed_epoch: u64 = 1;

    let id_alice = identifier_for_index(0);
    let id_bob = identifier_for_index(1);
    let id_carol = identifier_for_index(2);

    let recipient_pubs: BTreeMap<OwnerAddr, [u8; 32]> = [
        (ALICE, alice_x25519_pub()),
        (BOB, bob_x25519_pub()),
        (CAROL, carol_x25519_pub()),
    ]
    .into_iter()
    .collect();

    let alice_sk = alice_ed25519_sk();
    let bob_sk = bob_ed25519_sk();
    let carol_sk = carol_ed25519_sk();
    let alice_x_priv = alice_x25519_priv();
    let bob_x_priv = bob_x25519_priv();
    let carol_x_priv = carol_x25519_priv();

    // ── Round 1: Alice initiates; Bob + Carol contribute their own dr rn=1
    let alice_init_hlc = hlc_at(1_000, "alice");
    let ceremony_id = derive_ceremony_id(
        &members,
        threshold,
        alice_init_hlc.wall_ms,
        alice_init_hlc.logical,
        &TEST_SPACE_ID,
    );

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

    // Bob + Carol need pending_dkg pre-seeded (in production Phase 4a-main
    // will broadcast it; in this test we seed it directly).
    for (log, x_priv, addr) in [
        (&mut log_b, &bob_x_priv, BOB),
        (&mut log_c, &carol_x_priv, CAROL),
    ] {
        log.committee_state.pending_dkg = Some(PendingCeremony {
            ceremony_id,
            members: members.clone(),
            threshold,
            max_signers,
            proposed_epoch,
            ..Default::default()
        });
        log.apply_with_identity(dr1_alice.clone(), &addr, x_priv)
            .expect("peer applies alice's dr rn=1");
    }

    // Bob's own dr rn=1.
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
        .apply_with_identity(dr1_bob.clone(), &ALICE, &alice_x_priv)
        .expect("alice applies bob's dr rn=1");
    log_c
        .apply_with_identity(dr1_bob, &CAROL, &carol_x_priv)
        .expect("carol applies bob's dr rn=1");

    // Carol's own dr rn=1.
    let carol_init_hlc = hlc_at(1_200, "carol");
    let (carol_r1_secret, carol_r1_pkg_bytes) =
        dkg_part1_local(id_carol, max_signers, threshold).expect("carol dkg_part1");
    log_c.local_dkg_secret = Some(carol_r1_secret);
    let carol_r1_payload = DkgRoundPayload {
        ceremony_id,
        round_num: 1,
        round1_package: Some(carol_r1_pkg_bytes),
        recipient_ciphertexts: None,
    };
    let dr1_carol = build_signed_dfrost_event(
        &carol_sk,
        CAROL,
        DfrostEventKind::DkgRound,
        &carol_r1_payload,
        carol_init_hlc,
    )
    .expect("build_signed dr rn=1 (carol)");
    log_c
        .apply_with_identity(dr1_carol.clone(), &CAROL, &carol_x_priv)
        .expect("carol applies own dr rn=1");
    log_a
        .apply_with_identity(dr1_carol.clone(), &ALICE, &alice_x_priv)
        .expect("alice applies carol's dr rn=1");
    log_b
        .apply_with_identity(dr1_carol, &BOB, &bob_x_priv)
        .expect("bob applies carol's dr rn=1");

    // ── Round 2: each engine emits its rn=2 + cross-applies to the other two.
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
    let dr2_carol = contribute_dkg_round2_local(
        &mut log_c,
        CAROL,
        ceremony_id,
        &members,
        &recipient_pubs,
        hlc_at(2_200, "carol"),
        &carol_sk,
        &carol_x_priv,
    );

    log_b
        .apply_with_identity(dr2_alice.clone(), &BOB, &bob_x_priv)
        .expect("bob applies alice's dr rn=2");
    log_c
        .apply_with_identity(dr2_alice, &CAROL, &carol_x_priv)
        .expect("carol applies alice's dr rn=2");
    log_a
        .apply_with_identity(dr2_bob.clone(), &ALICE, &alice_x_priv)
        .expect("alice applies bob's dr rn=2");
    log_c
        .apply_with_identity(dr2_bob, &CAROL, &carol_x_priv)
        .expect("carol applies bob's dr rn=2");
    log_a
        .apply_with_identity(dr2_carol.clone(), &ALICE, &alice_x_priv)
        .expect("alice applies carol's dr rn=2");
    log_b
        .apply_with_identity(dr2_carol, &BOB, &bob_x_priv)
        .expect("bob applies carol's dr rn=2");

    // ── Round 3: each engine finalises; cross-apply dk events.
    let (dk_alice, alice_key_pkg, alice_pub_pkg) = contribute_dkg_round3_local(
        &mut log_a,
        ALICE,
        ceremony_id,
        &members,
        hlc_at(3_000, "alice"),
        &alice_sk,
        &alice_x_priv,
    );
    let (dk_bob, bob_key_pkg, _bob_pub_pkg) = contribute_dkg_round3_local(
        &mut log_b,
        BOB,
        ceremony_id,
        &members,
        hlc_at(3_100, "bob"),
        &bob_sk,
        &bob_x_priv,
    );
    let (dk_carol, carol_key_pkg, _carol_pub_pkg) = contribute_dkg_round3_local(
        &mut log_c,
        CAROL,
        ceremony_id,
        &members,
        hlc_at(3_200, "carol"),
        &carol_sk,
        &carol_x_priv,
    );

    // Threshold=2 promotes the committee at 2 dk confirmations; after
    // promotion `pending_dkg` is cleared, so any further dk against the
    // same ceremony_id returns `UnknownCeremony`. `contribute_dkg_round3_local`
    // already applies each engine's OWN dk locally (1 confirmation), so
    // each engine only needs to cross-apply ONE peer's dk to reach
    // quorum (2 confirmations) and promote.
    log_a
        .apply_with_identity(dk_bob.clone(), &ALICE, &alice_x_priv)
        .expect("alice applies bob's dk → promotes");
    log_b
        .apply_with_identity(dk_alice.clone(), &BOB, &bob_x_priv)
        .expect("bob applies alice's dk → promotes");
    // Carol cross-applies alice's dk to reach quorum (her own dk_carol
    // already counts as 1 confirmation).
    log_c
        .apply_with_identity(dk_alice, &CAROL, &carol_x_priv)
        .expect("carol applies alice's dk → promotes");
    // dk_carol + dk_bob are intentionally not cross-applied beyond what's
    // needed — every engine already promoted on a 2-of-3 quorum. Carol's
    // KeyPackage is stashed locally by `contribute_dkg_round3_local`,
    // which is what threshold-sign needs.
    let _ = (dk_bob, dk_carol);

    // Convergence: all three engines active with identical joint VK.
    assert!(log_a.committee_state.active);
    assert!(log_b.committee_state.active);
    assert!(log_c.committee_state.active);
    assert_eq!(
        log_a.committee_state.joint_verifying_key, log_b.committee_state.joint_verifying_key,
        "alice + bob converge on joint VK"
    );
    assert_eq!(
        log_a.committee_state.joint_verifying_key, log_c.committee_state.joint_verifying_key,
        "alice + carol converge on joint VK"
    );

    (
        log_a,
        log_b,
        log_c,
        alice_key_pkg,
        bob_key_pkg,
        carol_key_pkg,
        alice_pub_pkg,
        members,
    )
}

/// R4-1 regression: 2-of-3 threshold-sign exercises the canonical
/// signing-set selection END-TO-END via `contribute_threshold_sign_local`
/// (which now mirrors the IPC's R4 fix: build SigningPackage from the
/// canonical signing set — first `threshold` peers by sorted OwnerAddr
/// — for both round2::sign AND aggregate).
///
/// Two assertions:
///   1. **Positive (post-fix)**: Alice + Bob use the helper to sign
///      against the canonical {A, B} SigningPackage; aggregation with
///      the SAME package succeeds → produces a valid threshold sig.
///   2. **Negative (pre-fix simulation)**: build a separate "full-set"
///      SigningPackage from {A, B, C}'s commitments, simulate signers
///      using THAT (the pre-fix behaviour). Aggregating against the
///      canonical {A, B} package — which the pre-fix aggregator did —
///      MUST fail. This is the bug-trigger: locked in here so any
///      future regression that re-introduces the mismatch is caught.
#[tokio::test]
async fn threshold_sign_ipc_2of3_canonical_set_aggregates() {
    let (mut log_a, mut log_b, mut log_c, key_pkg_a, key_pkg_b, key_pkg_c, pub_pkg_shared, members) =
        dkg_complete_three_engine_2of3_via_ipc_path();

    // ── Sign-session ceremony_id (mirrors IPC step 5).
    let seed_bytes: [u8; 32] = [0x77; 32];
    let epoch: u64 = 1;
    let space_id = SpaceId([0xAA; 16]);

    let mut sign_tag = Vec::with_capacity(b"sign-v1:".len() + seed_bytes.len());
    sign_tag.extend_from_slice(b"sign-v1:");
    sign_tag.extend_from_slice(&seed_bytes);
    let sign_ceremony_id = derive_ceremony_id_canonical(&space_id, epoch, &sign_tag);
    let message_hash = derive_vrf_seed(&seed_bytes, epoch);

    let alice_sk = alice_ed25519_sk();
    let bob_sk = bob_ed25519_sk();
    let carol_sk = carol_ed25519_sk();
    let alice_x_priv = alice_x25519_priv();
    let bob_x_priv = bob_x25519_priv();
    let carol_x_priv = carol_x25519_priv();

    // ── Round 1: every member commits + cross-applies. Carol's commitment
    //    is what makes this test load-bearing: it lands in every engine's
    //    `pending_sign.contributions`, so a buggy implementation that
    //    iterates ALL contributions for SigningPackage construction will
    //    build a different package than the aggregator's canonical-set
    //    package, producing a Fiat-Shamir-challenge mismatch.
    let mut rng = frost::rand_core::OsRng;
    let (nonces_a, commitments_a) = frost::round1::commit(key_pkg_a.signing_share(), &mut rng);
    let (nonces_b, commitments_b) = frost::round1::commit(key_pkg_b.signing_share(), &mut rng);
    let (nonces_c, commitments_c) = frost::round1::commit(key_pkg_c.signing_share(), &mut rng);

    let mut nonces_a_cbor = Vec::new();
    ciborium::into_writer(&nonces_a, &mut nonces_a_cbor).expect("encode alice nonces");
    let mut commitments_a_cbor = Vec::new();
    ciborium::into_writer(&commitments_a, &mut commitments_a_cbor).expect("encode alice cm");
    let mut nonces_b_cbor = Vec::new();
    ciborium::into_writer(&nonces_b, &mut nonces_b_cbor).expect("encode bob nonces");
    let mut commitments_b_cbor = Vec::new();
    ciborium::into_writer(&commitments_b, &mut commitments_b_cbor).expect("encode bob cm");
    let mut nonces_c_cbor = Vec::new();
    ciborium::into_writer(&nonces_c, &mut nonces_c_cbor).expect("encode carol nonces");
    let mut commitments_c_cbor = Vec::new();
    ciborium::into_writer(&commitments_c, &mut commitments_c_cbor).expect("encode carol cm");

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
    let ts_carol_empty = request_vrf_beacon_local(
        &mut log_c,
        CAROL,
        sign_ceremony_id,
        message_hash,
        commitments_c_cbor.clone(),
        nonces_c_cbor,
        hlc_at(4_200, "carol"),
        &carol_sk,
        &carol_x_priv,
    );

    // Cross-apply round-1 commitments across all three engines.
    log_b
        .apply_with_identity(ts_alice_empty.clone(), &BOB, &bob_x_priv)
        .expect("bob applies alice's ts(empty)");
    log_c
        .apply_with_identity(ts_alice_empty, &CAROL, &carol_x_priv)
        .expect("carol applies alice's ts(empty)");
    log_a
        .apply_with_identity(ts_bob_empty.clone(), &ALICE, &alice_x_priv)
        .expect("alice applies bob's ts(empty)");
    log_c
        .apply_with_identity(ts_bob_empty, &CAROL, &carol_x_priv)
        .expect("carol applies bob's ts(empty)");
    log_a
        .apply_with_identity(ts_carol_empty.clone(), &ALICE, &alice_x_priv)
        .expect("alice applies carol's ts(empty)");
    log_b
        .apply_with_identity(ts_carol_empty, &BOB, &bob_x_priv)
        .expect("bob applies carol's ts(empty)");

    // After round-1: all three engines have 3 contributions (all empty).
    for (log, name) in [(&log_a, "alice"), (&log_b, "bob"), (&log_c, "carol")] {
        let p = log
            .committee_state
            .pending_sign
            .get(&sign_ceremony_id)
            .unwrap_or_else(|| panic!("{name} pending_sign present"));
        assert_eq!(p.contributions.len(), 3, "{name} sees 3 commitments");
    }

    // ── Alice signs via the canonical-set helper.
    //
    // `contribute_threshold_sign_local` (R4 fix) derives canonical
    // signing set = first 2 sorted addrs from contribution keys =
    // {ALICE, BOB} — Carol is excluded. Alice's round2::sign therefore
    // binds to the {ALICE, BOB} SigningPackage's Fiat-Shamir challenge.
    let (ts_alice_with_share, signing_package_alice) = contribute_threshold_sign_local(
        &mut log_a,
        ALICE,
        sign_ceremony_id,
        &members,
        &key_pkg_a,
        hlc_at(5_000, "alice"),
        &alice_sk,
    );
    log_a
        .apply_with_identity(ts_alice_with_share.clone(), &ALICE, &alice_x_priv)
        .expect("alice applies own ts(share)");
    log_b
        .apply_with_identity(ts_alice_with_share.clone(), &BOB, &bob_x_priv)
        .expect("bob applies alice's ts(share)");
    log_c
        .apply_with_identity(ts_alice_with_share, &CAROL, &carol_x_priv)
        .expect("carol applies alice's ts(share)");

    // ── Bob signs via the same canonical-set helper — produces the
    //    IDENTICAL SigningPackage as Alice's (sorted-key BTreeMap is
    //    deterministic across replicas).
    let (ts_bob_with_share, signing_package_bob) = contribute_threshold_sign_local(
        &mut log_b,
        BOB,
        sign_ceremony_id,
        &members,
        &key_pkg_b,
        hlc_at(5_100, "bob"),
        &bob_sk,
    );
    log_a
        .apply_with_identity(ts_bob_with_share.clone(), &ALICE, &alice_x_priv)
        .expect("alice applies bob's ts(share)");
    log_b
        .apply_with_identity(ts_bob_with_share.clone(), &BOB, &bob_x_priv)
        .expect("bob applies own ts(share)");
    log_c
        .apply_with_identity(ts_bob_with_share, &CAROL, &carol_x_priv)
        .expect("carol applies bob's ts(share)");

    // Sanity: Alice's and Bob's SigningPackages must serialize identically
    // (the canonical-set rule guarantees this; without it the Fiat-Shamir
    // challenge differs per signer and aggregate fails). Implements the
    // load-bearing constraint via a byte-equality check.
    {
        let mut a_bytes = Vec::new();
        let mut b_bytes = Vec::new();
        ciborium::into_writer(&signing_package_alice, &mut a_bytes)
            .expect("encode alice signing_package");
        ciborium::into_writer(&signing_package_bob, &mut b_bytes)
            .expect("encode bob signing_package");
        assert_eq!(
            a_bytes, b_bytes,
            "R4 invariant: every signer builds the same canonical-set SigningPackage \
             (sorted-addr BTreeMap iteration is deterministic across replicas)"
        );
    }

    // ── Positive aggregation (post-fix): use the SAME canonical
    //    signing_package + shares_map from the canonical set → succeeds.
    let canonical_signing_set = vec![ALICE, BOB];
    let shares_map_canonical: BTreeMap<Identifier, frost::round2::SignatureShare> = {
        let pending = log_a
            .committee_state
            .pending_sign
            .get(&sign_ceremony_id)
            .unwrap();
        let mut sm = BTreeMap::new();
        for addr in &canonical_signing_set {
            let (_, share_bytes) = pending
                .contributions
                .get(addr)
                .expect("canonical signer present");
            assert!(
                !share_bytes.is_empty(),
                "canonical signer must have non-empty share"
            );
            let idx = members.iter().position(|a| a == addr).unwrap();
            let id = identifier_for_index(idx);
            let share: frost::round2::SignatureShare =
                ciborium::from_reader(&share_bytes[..]).expect("decode share");
            sm.insert(id, share);
        }
        sm
    };

    let group_signature = frost::aggregate(
        &signing_package_alice,
        &shares_map_canonical,
        &pub_pkg_shared,
    )
    .expect("R4 post-fix: canonical signing_package + canonical shares → aggregate OK");
    let sig_bytes: Vec<u8> = group_signature.serialize().expect("serialize signature");
    assert_eq!(
        sig_bytes.len(),
        64,
        "Schnorr signature must be R(32) || s(32)"
    );

    let vk = log_a.committee_state.joint_verifying_key.expect("joint vk");
    verify_schnorr_signature(&vk, &message_hash, &sig_bytes)
        .expect("aggregated Schnorr sig verifies under joint vk");

    let mut r_check = [0u8; 32];
    r_check.copy_from_slice(&sig_bytes[..32]);
    let _vrf_output = derive_vrf_output(&r_check);

    // ── Negative bug-trigger (pre-fix simulation): build a full-set
    //    SigningPackage (what the buggy IPC used to construct), have
    //    BOTH signers sign against it, then aggregate against the
    //    canonical-set package (what the buggy aggregator built).
    //    This MUST fail — locks the bug in regression-test form.
    //
    //    Fresh nonces so the signers don't reuse the canonical-path
    //    ones (FROST §6.2: nonce reuse leaks the secret share — even
    //    in a test, reusing once-consumed nonces would crash).
    let (nonces_a_full, commitments_a_full) =
        frost::round1::commit(key_pkg_a.signing_share(), &mut rng);
    let (nonces_b_full, commitments_b_full) =
        frost::round1::commit(key_pkg_b.signing_share(), &mut rng);
    let (_nonces_c_full, commitments_c_full) =
        frost::round1::commit(key_pkg_c.signing_share(), &mut rng);

    let mut full_commitments_map: BTreeMap<Identifier, frost::round1::SigningCommitments> =
        BTreeMap::new();
    full_commitments_map.insert(identifier_for_index(0), commitments_a_full);
    full_commitments_map.insert(identifier_for_index(1), commitments_b_full);
    full_commitments_map.insert(identifier_for_index(2), commitments_c_full);
    let full_signing_package = frost::SigningPackage::new(full_commitments_map, &message_hash);

    let alice_share_buggy = frost::round2::sign(&full_signing_package, &nonces_a_full, &key_pkg_a)
        .expect("round2::sign against full-set package");
    let bob_share_buggy = frost::round2::sign(&full_signing_package, &nonces_b_full, &key_pkg_b)
        .expect("round2::sign against full-set package");

    // Aggregator builds a canonical-only package from {A, B}'s commitments
    // — mirroring the pre-fix aggregator's "selection_signing_package"
    // path. Since the commitments inside the FULL package include Carol's
    // (which the partial package omits), the recomputed challenge differs,
    // and Alice's/Bob's shares (which bound to the full challenge)
    // don't verify under the partial one.
    let mut partial_commitments_map: BTreeMap<Identifier, frost::round1::SigningCommitments> =
        BTreeMap::new();
    let alice_commit_from_full: frost::round1::SigningCommitments = full_signing_package
        .signing_commitments()
        .get(&identifier_for_index(0))
        .cloned()
        .expect("alice commitment");
    let bob_commit_from_full: frost::round1::SigningCommitments = full_signing_package
        .signing_commitments()
        .get(&identifier_for_index(1))
        .cloned()
        .expect("bob commitment");
    partial_commitments_map.insert(identifier_for_index(0), alice_commit_from_full);
    partial_commitments_map.insert(identifier_for_index(1), bob_commit_from_full);
    let partial_signing_package =
        frost::SigningPackage::new(partial_commitments_map, &message_hash);

    let mut buggy_shares_map: BTreeMap<Identifier, frost::round2::SignatureShare> = BTreeMap::new();
    buggy_shares_map.insert(identifier_for_index(0), alice_share_buggy);
    buggy_shares_map.insert(identifier_for_index(1), bob_share_buggy);

    let buggy_result =
        frost::aggregate(&partial_signing_package, &buggy_shares_map, &pub_pkg_shared);
    assert!(
        buggy_result.is_err(),
        "R4 bug-trigger: aggregating canonical-set package against shares signed under \
         full-set package MUST fail — Fiat-Shamir challenge mismatch. If this assertion \
         starts passing, the canonical-set selection has regressed: every signer + the \
         aggregator must use a byte-identical SigningPackage. Got: {:?}",
        buggy_result.map(|sig| sig.serialize())
    );

    // Carol's contribution remained an empty-share commitment on every
    // engine — the canonical-set rule never picks her up for signing,
    // so her ts(empty) lingers harmlessly in pending_sign. Confirms
    // the canonical-set selection ignores extra contributors that
    // aren't in the first-threshold-sorted-addr window.
    //
    // R5-6: replicate the empty-share assertion across all three logs.
    // A replica-specific bug in Bob's or Carol's apply / upsert path
    // could populate Carol's share locally on those engines even while
    // log_a stays clean — a single-replica check would silently miss
    // it.
    for (log, name) in [(&log_a, "alice"), (&log_b, "bob"), (&log_c, "carol")] {
        let p = log
            .committee_state
            .pending_sign
            .get(&sign_ceremony_id)
            .unwrap_or_else(|| panic!("{name} pending_sign present"));
        assert!(
            p.contributions
                .get(&CAROL)
                .map(|(_, share)| share.is_empty())
                .unwrap_or(false),
            "carol's contribution still empty on {name} (she's not in canonical signing set)"
        );
    }
}
