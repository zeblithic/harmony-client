//! ZEB-1043: DFROST committee-reset MEMBERSHIP events across a REAL
//! `CommunitySyncEngine` wire + cert verify.
//!
//! ## The gap this closes
//!
//! `community_dfrost_reset_e2e.rs` drives the REAL reset lifecycle
//! (`materialize_with_now` / `evaluate_reset_phases`) and REAL FROST crypto,
//! but admits every membership event via `insert_verified_for_test` — a
//! `test-fixtures` trusted-write seam that BYPASSES `verify_event`. So no test
//! proved a `DfrostResetProposal` / `DfrostResetCosign` / `DfrostResetResponse`
//! event actually crossing a live wire and passing the receive-side verify
//! gate (RS-P*, RS-C1, RS-R1/R3/R4). `community_open_flow_integration.rs`
//! proves that wire+cert mechanism generally — but only for Join/Leave, never
//! the reset kinds. This file is the missing intersection.
//!
//! ## Why the two identity worlds don't have to meet (fix direction (a))
//!
//! `OwnerAddr` has two cryptographically incompatible derivations (see the
//! `community_dfrost_reset_e2e.rs` module doc): DFROST committee-event verify
//! needs `harmony_identity`'s `SHA256(x25519‖ed25519)`; membership cert
//! bootstrap needs `harmony_owner`'s `SHA256(ed25519)` alone. A single
//! identity cannot be both. The reset verify gates, however, split exactly
//! along that boundary:
//!
//!   * `DfrostResetProposal` (RS-P1) and `DfrostResetCosign` (RS-C1) verify
//!     the ACTOR as a currently-Joined power-100 admin — a pure membership
//!     `SHA256(ed25519)` check. No committee identity is involved.
//!   * `DfrostResetResponse` touches the committee only in RS-R3, which
//!     verifies the embedded `group_sig` as a Schnorr threshold signature over
//!     `dfrost_reset_message_hash(...)` against the joint `target_vk` — the
//!     bytes are carried in and checked as raw bytes, entirely separate from
//!     the actor's membership signature (RS-R1).
//!
//! So membership-only identities (`mint_test_owner`) author all three events
//! over the real wire; the committee produces ONLY the 64 signature bytes.
//! This file generates those bytes from a real 2-of-3 FROST-Ristretto255
//! ceremony (`FrostCommittee`) over the EXACT message hash the gate recomputes
//! (`dfrost_reset_message_hash`, the same production fn) — so there is zero
//! drift between what is signed and what RS-R3 checks, without standing up any
//! `DfrostLog` orchestration.
//!
//! ## Proving RS-R3 actually ran (not lenient-skip)
//!
//! RS-R3 is a lenient forward-ref: it only fires
//! `if let Some(target) = prior_state.reset_proposals.find(id)`. If a Response
//! is verified before its Proposal has materialized into the receiver's
//! derived `reset_proposals` view, the signature is NEVER checked and the
//! Response applies anyway. Two guards:
//!   1. Ordering — the Proposal is crossed and observed on the receiver BEFORE
//!      the Response is crossed.
//!   2. A deterministic negative control
//!      (`reset_response_bad_group_sig_is_rejected_by_verify_gate`) inserts a
//!      Response with a corrupted `group_sig` into an engine that already holds
//!      the Proposal and asserts `Rejected(DfrostResetResponseSigInvalid)`.
//!      The local-insert and wire-ingest paths share the same
//!      `insert_event_inner` → `verify_event` seam, so this proves the valid
//!      Response's acceptance over the wire is a real RS-R3 pass.

#![cfg(feature = "test-fixtures")]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use frost_ristretto255::{
    self as frost,
    keys::{IdentifierList, KeyPackage},
    rand_core::OsRng,
    Identifier,
};
use tokio::sync::{mpsc, Mutex};

use harmony_app::community_dfrost_crypto::{verify_schnorr_signature, verifying_key_to_bytes};
use harmony_app::community_membership::{
    dfrost_reset_digest, dfrost_reset_message_hash, mint_test_owner, sign_event, EventId,
    EventPayload, MembershipEventKind, ProposalKind, ResetVerdict, SignedMembershipEvent,
    TestOwner, DFROST_RESET_CONSUMED_DOMAIN, DFROST_RESET_ENDORSE_DOMAIN, DFROST_RESET_VETO_DOMAIN,
    RESET_VETO_WINDOW_FLOOR_MS,
};
use harmony_app::community_state_crdt::{CommunityState, InsertOutcome};
use harmony_app::community_state_sync::{
    CommunityMembershipDelta, CommunityReplayTracker, CommunitySyncEngine,
    CommunitySyncEngineConfig, IdentityResolver, PersistPaths, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{CasOp, ContentStore, RuntimeContentStore};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_app::{mint_community_creation, mint_redemption};

// ─── FROST committee fixture (option (ii): raw primitives, no DfrostLog) ─────

/// A real 2-of-3 FROST-Ristretto255 committee, dealer-generated. Produces the
/// `target_vk` a reset proposal pins and, on demand, a threshold `group_sig`
/// over any message — exactly the two things RS-R3 needs, and nothing more.
struct FrostCommittee {
    pub_pkg: frost::keys::PublicKeyPackage,
    /// The 2 signers actually used (of the 3 dealt) — a live 2-of-3 quorum.
    signers: Vec<(Identifier, KeyPackage)>,
    /// Compressed joint verifying key — the proposal's `target_vk`.
    target_vk: [u8; 32],
}

impl FrostCommittee {
    fn generate() -> Self {
        let (shares, pub_pkg) =
            frost::keys::generate_with_dealer(3, 2, IdentifierList::Default, OsRng)
                .expect("dealer keygen 2-of-3");
        let target_vk = verifying_key_to_bytes(pub_pkg.verifying_key());
        // BTreeMap iterates in Identifier order (1,2,3); take the first two as
        // the live signing quorum. Any 2-of-3 subset aggregates to the same vk.
        let signers = shares
            .iter()
            .take(2)
            .map(|(id, share)| {
                (
                    *id,
                    KeyPackage::try_from(share.clone()).expect("key package from dealt share"),
                )
            })
            .collect();
        FrostCommittee {
            pub_pkg,
            signers,
            target_vk,
        }
    }

    /// Real round1→round2→aggregate over `message`, serialized to the 64-byte
    /// Schnorr signature RS-R3's `verify_schnorr_signature` deserializes.
    fn sign(&self, message: &[u8]) -> [u8; 64] {
        let mut nonces_map = BTreeMap::new();
        let mut commitments_map = BTreeMap::new();
        for (id, kp) in &self.signers {
            let (nonces, commitments) = frost::round1::commit(kp.signing_share(), &mut OsRng);
            nonces_map.insert(*id, nonces);
            commitments_map.insert(*id, commitments);
        }
        let signing_package = frost::SigningPackage::new(commitments_map, message);
        let mut shares_map = BTreeMap::new();
        for (id, kp) in &self.signers {
            let share =
                frost::round2::sign(&signing_package, &nonces_map[id], kp).expect("round2 sign");
            shares_map.insert(*id, share);
        }
        let sig = frost::aggregate(&signing_package, &shares_map, &self.pub_pkg)
            .expect("aggregate threshold signature");
        sig.serialize()
            .expect("serialize signature")
            .try_into()
            .expect("Ristretto Schnorr signature is 64 bytes")
    }
}

/// Sorted, deduped 2-member successor pin for a proposal (RS-P2 requires
/// ascending + deduped, and the derived `reset_proposals` view carries the
/// vec verbatim — so the digest must use the exact same ordering).
fn sorted_pair(a: OwnerAddr, b: OwnerAddr) -> Vec<OwnerAddr> {
    let mut v = vec![a, b];
    v.sort();
    v.dedup();
    v
}

/// Proposal `target_epoch` / `new_threshold`, shared by every proposal and
/// every response digest in this file — a single source so the digest the
/// response signs can never drift from the proposal RS-R3 recomputes against.
const TARGET_EPOCH: u64 = 1;
const NEW_THRESHOLD: u16 = 2;

// ─── Membership event minting (steady-state kinds: signer via enrolled key) ──

fn mint_event(
    community_id: SpaceId,
    kind: MembershipEventKind,
    actor: OwnerAddr,
    at: Hlc,
    signing_key: &ed25519_dalek::SigningKey,
) -> SignedMembershipEvent {
    let mut id = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut id);
    let payload = EventPayload {
        id,
        community_id,
        kind,
        actor,
        at,
    };
    sign_event(&payload, signing_key).expect("sign membership event")
}

fn hlc(wall_ms: u64, device: &str) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: device.to_string(),
    }
}

// ─── Two-engine wire harness (factored from community_open_flow_integration) ─

/// Everything a reset test needs after the roster is converged: two live
/// engines sharing an in-memory wire + CAS, both holding {A,B} as power-100
/// admins with admin_quorum=2.
struct WirePair {
    engine_a: CommunitySyncEngine,
    engine_b: CommunitySyncEngine,
    state_a: Arc<Mutex<CommunityState>>,
    state_b: Arc<Mutex<CommunityState>>,
    community_id: SpaceId,
    owner_a: OwnerAddr,
    owner_b: OwnerAddr,
    signing_a: ed25519_dalek::SigningKey,
    signing_b: ed25519_dalek::SigningKey,
    // Keep background tasks / tempdirs / resolver alive for the test's lifetime.
    _tmp_a: tempfile::TempDir,
    _tmp_b: tempfile::TempDir,
    _resolver: Arc<dyn IdentityResolver>,
    _cas: Arc<Mutex<HashMap<harmony_content::cid::ContentId, Vec<u8>>>>,
}

struct TwoIdentityResolver {
    a: (OwnerAddr, [u8; 64]),
    b: (OwnerAddr, [u8; 64]),
}

#[async_trait::async_trait]
impl IdentityResolver for TwoIdentityResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        if *addr == self.a.0 {
            Some(self.a.1)
        } else if *addr == self.b.0 {
            Some(self.b.1)
        } else {
            None
        }
    }
}

async fn wait_until<F, Fut>(mut cond: F, timeout: Duration) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond().await {
            return true;
        }
        if tokio::time::Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn event_count(state: &Arc<Mutex<CommunityState>>) -> usize {
    state.lock().await.event_count()
}

/// Stand up two engines on a shared in-memory wire + CAS, create an OPEN
/// community with admin A, redeem B, and drive B → power-100 admin +
/// admin_quorum=2 — converged on BOTH engines through the REAL verify path.
/// Setup events are inserted on both engines directly (scaffolding); the
/// reset events under test cross the wire on their own.
async fn spawn_wire_pair(seed_a: u8, seed_b: u8) -> WirePair {
    let owner_a_test: TestOwner = mint_test_owner(seed_a);
    let owner_b_test: TestOwner = mint_test_owner(seed_b);
    let owner_a = owner_a_test.owner;
    let owner_b = owner_b_test.owner;
    let signing_a = owner_a_test.device_key.clone();
    let signing_b = owner_b_test.device_key.clone();

    // ZEB-339: membership signer resolution uses the EnrollmentCert /
    // materialized enrolled keys, not the resolver — identity_pubs unused.
    let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
        a: (owner_a, [0u8; 64]),
        b: (owner_b, [0u8; 64]),
    });

    // Shared in-memory CAS servicer — A and B route their RuntimeContentStore
    // ops through one channel so blobs A puts are visible to B (and vice versa).
    let cas: Arc<Mutex<HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(64);
    let cas_for_servicer = Arc::clone(&cas);
    tokio::spawn(async move {
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal {
                    cid, blob, reply, ..
                } => {
                    cas_for_servicer.lock().await.insert(cid, blob);
                    if let Some(r) = reply {
                        let _ = r.send(Ok(()));
                    }
                }
                CasOp::GetOrFetch { cid, reply, .. } => {
                    let v = cas_for_servicer.lock().await.get(&cid).cloned();
                    let _ = reply.send(Ok(v));
                }
                CasOp::GetLocal { cid, reply } => {
                    let v = cas_for_servicer.lock().await.get(&cid).cloned();
                    let _ = reply.send(v);
                }
                CasOp::AllowServeSubtree { reply, .. } => {
                    let _ = reply.send(Ok(0));
                }
            }
        }
    });

    // Wire: A's publisher → B's subscriber and B's publisher → A's subscriber.
    let (a_out_tx, mut a_out_rx) = mpsc::channel::<Vec<u8>>(64);
    let (a_in_tx, a_in_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_out_tx, mut b_out_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_in_tx, b_in_rx) = mpsc::channel::<Vec<u8>>(64);
    let a_in_for_fwd = a_in_tx.clone();
    tokio::spawn(async move {
        while let Some(bytes) = b_out_rx.recv().await {
            let _ = a_in_for_fwd.send(bytes).await;
        }
    });
    let b_in_for_fwd = b_in_tx.clone();
    tokio::spawn(async move {
        while let Some(bytes) = a_out_rx.recv().await {
            let _ = b_in_for_fwd.send(bytes).await;
        }
    });

    // A mints a fresh OPEN community + bootstrap Join (carries A's cert).
    let minted_a = mint_community_creation(
        "ResetWireTest",
        false,
        owner_a,
        &signing_a,
        &owner_a_test.cert,
        hlc(100_000, "a-dev"),
    )
    .expect("mint create");
    let community_id = minted_a.community_id;

    let cs_a: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_secs(2),
    ));
    let cs_b: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_secs(2),
    ));

    let state_a = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let state_b = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker_a = Arc::new(Mutex::new(CommunityReplayTracker::new((
        owner_a,
        "a-dev".to_string(),
    ))));
    let tracker_b = Arc::new(Mutex::new(CommunityReplayTracker::new((
        owner_b,
        "b-dev".to_string(),
    ))));
    let (delta_a_tx, _delta_a_rx) = mpsc::channel::<CommunityMembershipDelta>(64);
    let (delta_b_tx, _delta_b_rx) = mpsc::channel::<CommunityMembershipDelta>(64);
    let tmp_a = tempfile::tempdir().expect("tmp a");
    let tmp_b = tempfile::tempdir().expect("tmp b");

    let engine_a = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        device_cipher: harmony_app::device_dataset_file::test_cipher(),
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id,
        membership_key: minted_a.membership_key.clone(),
        admin_addr: owner_a,
        is_invite_only: false,
        device_id: "a-dev".into(),
        self_owner: owner_a,
        signing_key: Arc::new(signing_a.clone()),
        state: Arc::clone(&state_a),
        tracker: Arc::clone(&tracker_a),
        content_store: cs_a,
        publisher_tx: a_out_tx,
        subscriber_rx: a_in_rx,
        paths: PersistPaths {
            crdt: tmp_a.path().join("crdt.cbor"),
            replay: tmp_a.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(Arc::clone(&resolver)),
        error_tx: None,
        delta_tx: Some(delta_a_tx),
        pending_redemptions: None,
        crdt_state: None,
        inviter_identity_pub: None,
        nav_emitter: None,
        membership_updated_emitter: None,
        root_serve_rx: None,
    });
    let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        device_cipher: harmony_app::device_dataset_file::test_cipher(),
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id,
        membership_key: minted_a.membership_key.clone(),
        admin_addr: owner_a,
        is_invite_only: false,
        device_id: "b-dev".into(),
        self_owner: owner_b,
        signing_key: Arc::new(signing_b.clone()),
        state: Arc::clone(&state_b),
        tracker: Arc::clone(&tracker_b),
        content_store: cs_b,
        publisher_tx: b_out_tx,
        subscriber_rx: b_in_rx,
        paths: PersistPaths {
            crdt: tmp_b.path().join("crdt.cbor"),
            replay: tmp_b.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(Arc::clone(&resolver)),
        error_tx: None,
        delta_tx: Some(delta_b_tx),
        pending_redemptions: None,
        crdt_state: None,
        inviter_identity_pub: None,
        nav_emitter: None,
        membership_updated_emitter: None,
        root_serve_rx: None,
    });

    // ── Roster scaffolding: applied on BOTH engines via the real verify path.
    // B's Join comes from an open-invite redemption (carries B's cert).
    let invite_payload = harmony_app::community_invite::CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id,
        epoch_snapshot: harmony_app::community_invite::InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: minted_a.membership_key.as_bytes().to_vec(),
            sealed_epoch_keys: Vec::new(),
            state_snapshot: harmony_app::community_invite::MaterializedCommunityState::default(),
        },
        admin_addr: owner_a,
        community_name: "ResetWireTest".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        inviter_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
    };
    let minted_b = mint_redemption(
        &invite_payload,
        owner_b,
        &signing_b,
        &owner_b_test.cert,
        hlc(200_000, "b-dev"),
    )
    .expect("mint redeem");

    // Steady-state scaffolding events (signer resolves via enrolled keys).
    let set_power_b = mint_event(
        community_id,
        MembershipEventKind::SetPower {
            target: owner_b,
            level: 100,
        },
        owner_a,
        hlc(300_000, "a-dev"),
        &signing_a,
    );
    let change_quorum = mint_event(
        community_id,
        MembershipEventKind::AdminProposal {
            proposal_kind: ProposalKind::ChangeQuorum { new_quorum: 2 },
        },
        owner_a,
        hlc(400_000, "a-dev"),
        &signing_a,
    );

    for engine in [&engine_a, &engine_b] {
        engine
            .insert_local_event(minted_a.bootstrap_join.clone())
            .await
            .expect("seed A bootstrap Join");
        engine
            .insert_local_event(minted_b.bootstrap_join.clone())
            .await
            .expect("seed B redemption Join");
        engine
            .insert_local_event(set_power_b.clone())
            .await
            .expect("seed SetPower B=100");
        engine
            .insert_local_event(change_quorum.clone())
            .await
            .expect("seed ChangeQuorum=2");
    }

    // Both engines converge on the 4-event roster before any reset event.
    assert!(
        wait_until(
            || async { event_count(&state_a).await == 4 && event_count(&state_b).await == 4 },
            Duration::from_secs(10),
        )
        .await,
        "both engines must converge on the 4-event admin roster"
    );

    WirePair {
        engine_a,
        engine_b,
        state_a,
        state_b,
        community_id,
        owner_a,
        owner_b,
        signing_a,
        signing_b,
        _tmp_a: tmp_a,
        _tmp_b: tmp_b,
        _resolver: resolver,
        _cas: cas,
    }
}

/// Build a `DfrostResetResponse` for `proposal_id` with a REAL threshold
/// signature over the exact RS-R3 message hash, for any verdict:
///
///   * `Endorse` / `Veto` — `new_vk` is `None`; the OLD committee (the one
///     the proposal pinned via `target_vk`) signs, and RS-R3 verifies against
///     `old_committee.target_vk`. `sig_committee` is the old committee.
///   * `Consumed` — `new_vk` is `Some(successor vk)`; the SUCCESSOR committee
///     signs, and RS-R3 verifies against `new_vk`. `sig_committee` is the
///     successor committee, and `new_vk` must equal `sig_committee.target_vk`.
///
/// The digest always binds the OLD `target_vk` (from `old_committee`) — only
/// the verdict domain and the verify key differ. `tamper` flips a byte of the
/// honest signature for the negative control; the honest signature is ALWAYS
/// verified first, so a rejection can only be attributed to the tamper.
#[allow(clippy::too_many_arguments)]
fn mint_response(
    community_id: SpaceId,
    old_committee: &FrostCommittee,
    verdict: ResetVerdict,
    sig_committee: &FrostCommittee,
    new_vk: Option<[u8; 32]>,
    proposal_id: EventId,
    new_members: &[OwnerAddr],
    actor: OwnerAddr,
    at: Hlc,
    signing_key: &ed25519_dalek::SigningKey,
    tamper: bool,
) -> SignedMembershipEvent {
    let digest = dfrost_reset_digest(
        &community_id,
        &proposal_id,
        &old_committee.target_vk,
        TARGET_EPOCH,
        new_members,
        NEW_THRESHOLD,
    )
    .expect("reset digest");
    let domain = match verdict {
        ResetVerdict::Endorse => DFROST_RESET_ENDORSE_DOMAIN,
        ResetVerdict::Veto => DFROST_RESET_VETO_DOMAIN,
        ResetVerdict::Consumed => DFROST_RESET_CONSUMED_DOMAIN,
    };
    let message_hash = dfrost_reset_message_hash(domain, &digest, new_vk.as_ref());
    // RS-R3 verifies against `new_vk` for Consumed, else the old `target_vk`.
    let verify_vk = new_vk.unwrap_or(old_committee.target_vk);
    let mut group_sig = sig_committee.sign(&message_hash);
    // Always confirm the honest signature verifies against the exact gate
    // inputs — so a negative-control rejection can only be the tamper below.
    verify_schnorr_signature(&verify_vk, &message_hash, &group_sig)
        .expect("honest group_sig must verify against the verdict's verify key");
    if tamper {
        group_sig[0] ^= 0xff;
    }
    mint_event(
        community_id,
        MembershipEventKind::DfrostResetResponse {
            target_event_id: proposal_id,
            verdict,
            group_sig,
            new_vk,
        },
        actor,
        at,
        signing_key,
    )
}

/// Author the reset Proposal (A → B) then the Cosign (B → A) over the real
/// wire, asserting each crosses and verifies, and return the proposal id.
/// Shared setup for the per-verdict Response tests; the dedicated
/// `reset_proposal_and_cosign_cross_real_wire` test pins the crossings
/// explicitly. Leaves both engines at `event_count == 6` (the 4-event roster
/// plus proposal and cosign), with the proposal materialized into each
/// engine's `reset_proposals` view.
async fn cross_proposal_and_cosign(
    pair: &WirePair,
    committee: &FrostCommittee,
    new_members: &[OwnerAddr],
) -> EventId {
    let proposal = mint_event(
        pair.community_id,
        MembershipEventKind::DfrostResetProposal {
            target_vk: committee.target_vk,
            target_epoch: TARGET_EPOCH,
            new_members: new_members.to_vec(),
            new_threshold: NEW_THRESHOLD,
            veto_window_ms: RESET_VETO_WINDOW_FLOOR_MS,
        },
        pair.owner_a,
        hlc(700_000, "a-dev"),
        &pair.signing_a,
    );
    let proposal_id: EventId = proposal.id;
    pair.engine_a
        .insert_local_event(proposal)
        .await
        .expect("A insert proposal");
    assert!(
        wait_until(
            || async { event_count(&pair.state_b).await == 5 },
            Duration::from_secs(10),
        )
        .await,
        "proposal must materialize on B before the Response"
    );

    let cosign = mint_event(
        pair.community_id,
        MembershipEventKind::DfrostResetCosign {
            target_event_id: proposal_id,
        },
        pair.owner_b,
        hlc(800_000, "b-dev"),
        &pair.signing_b,
    );
    pair.engine_b
        .insert_local_event(cosign)
        .await
        .expect("B insert cosign");
    assert!(
        wait_until(
            || async { event_count(&pair.state_a).await == 6 },
            Duration::from_secs(10),
        )
        .await,
        "cosign must cross to A"
    );
    proposal_id
}

// ─── Tests ───────────────────────────────────────────────────────────────

/// A `DfrostResetProposal` and a `DfrostResetCosign` each cross the wire in
/// one direction and pass the receive-side verify gate (RS-P1-P5 / RS-C1).
/// Neither touches the committee — pure membership-admin authorization.
#[tokio::test]
async fn reset_proposal_and_cosign_cross_real_wire() {
    let pair = spawn_wire_pair(0x1A, 0x1B).await;
    let committee = FrostCommittee::generate();
    let new_members = sorted_pair(pair.owner_a, pair.owner_b);

    // A authors the proposal; it crosses A → B and verifies on B.
    let proposal = mint_event(
        pair.community_id,
        MembershipEventKind::DfrostResetProposal {
            target_vk: committee.target_vk,
            target_epoch: 1,
            new_members: new_members.clone(),
            new_threshold: 2,
            veto_window_ms: RESET_VETO_WINDOW_FLOOR_MS,
        },
        pair.owner_a,
        hlc(700_000, "a-dev"),
        &pair.signing_a,
    );
    let proposal_id: EventId = proposal.id;
    let outcome = pair
        .engine_a
        .insert_local_event(proposal.clone())
        .await
        .expect("A local insert proposal");
    assert_eq!(outcome, InsertOutcome::Inserted, "proposal valid on A");
    assert!(
        wait_until(
            || async { event_count(&pair.state_b).await == 5 },
            Duration::from_secs(10),
        )
        .await,
        "proposal must cross the wire and verify on B (RS-P1-P5)"
    );

    // B authors the cosign; it crosses B → A and verifies on A (RS-C1).
    let cosign = mint_event(
        pair.community_id,
        MembershipEventKind::DfrostResetCosign {
            target_event_id: proposal_id,
        },
        pair.owner_b,
        hlc(800_000, "b-dev"),
        &pair.signing_b,
    );
    let outcome = pair
        .engine_b
        .insert_local_event(cosign)
        .await
        .expect("B local insert cosign");
    assert_eq!(outcome, InsertOutcome::Inserted, "cosign valid on B");
    assert!(
        wait_until(
            || async { event_count(&pair.state_a).await == 6 },
            Duration::from_secs(10),
        )
        .await,
        "cosign must cross the wire and verify on A (RS-C1)"
    );

    pair.engine_a.shutdown().await.expect("shutdown a");
    pair.engine_b.shutdown().await.expect("shutdown b");
}

/// Author `response` on A, assert it verifies locally (RS-R1 + RS-R3), and
/// assert it crosses A → B and verifies there too (both engines end at
/// `event_count == 7`: 4 roster + proposal + cosign + response).
async fn author_and_assert_response_crosses(pair: &WirePair, response: SignedMembershipEvent) {
    let outcome = pair
        .engine_a
        .insert_local_event(response)
        .await
        .expect("A insert response");
    assert_eq!(
        outcome,
        InsertOutcome::Inserted,
        "honest Response must verify on A (RS-R3 passes)"
    );
    assert!(
        wait_until(
            || async { event_count(&pair.state_b).await == 7 },
            Duration::from_secs(10),
        )
        .await,
        "Response must cross the wire and verify on B (RS-R1 + RS-R3)"
    );
}

/// Endorse: a `DfrostResetResponse` carrying a REAL 2-of-3 FROST threshold
/// signature crosses the wire and passes RS-R1 (member) + RS-R3 (Schnorr
/// verify against the OLD committee `target_vk`) on the receiver.
#[tokio::test]
async fn reset_response_endorse_crosses_real_wire() {
    let pair = spawn_wire_pair(0x2A, 0x2B).await;
    let committee = FrostCommittee::generate();
    let new_members = sorted_pair(pair.owner_a, pair.owner_b);
    let proposal_id = cross_proposal_and_cosign(&pair, &committee, &new_members).await;

    let response = mint_response(
        pair.community_id,
        &committee,
        ResetVerdict::Endorse,
        &committee, // Endorse: the OLD committee signs.
        None,
        proposal_id,
        &new_members,
        pair.owner_a,
        hlc(900_000, "a-dev"),
        &pair.signing_a,
        false,
    );
    author_and_assert_response_crosses(&pair, response).await;

    pair.engine_a.shutdown().await.expect("shutdown a");
    pair.engine_b.shutdown().await.expect("shutdown b");
}

/// Veto: same wire path as Endorse but a DISTINCT RS-R3 branch — the veto
/// domain (`DFROST_RESET_VETO_DOMAIN`), still verified against the OLD
/// committee `target_vk` with `new_vk = None`.
#[tokio::test]
async fn reset_response_veto_crosses_real_wire() {
    let pair = spawn_wire_pair(0x5A, 0x5B).await;
    let committee = FrostCommittee::generate();
    let new_members = sorted_pair(pair.owner_a, pair.owner_b);
    let proposal_id = cross_proposal_and_cosign(&pair, &committee, &new_members).await;

    let response = mint_response(
        pair.community_id,
        &committee,
        ResetVerdict::Veto,
        &committee, // Veto: the OLD committee signs.
        None,
        proposal_id,
        &new_members,
        pair.owner_a,
        hlc(900_000, "a-dev"),
        &pair.signing_a,
        false,
    );
    author_and_assert_response_crosses(&pair, response).await;

    pair.engine_a.shutdown().await.expect("shutdown a");
    pair.engine_b.shutdown().await.expect("shutdown b");
}

/// Consumed: the successful-reset path and the most distinct RS-R3 branch —
/// the SUCCESSOR committee signs, `new_vk = Some(successor vk)` is folded into
/// the message hash (consumed domain), and RS-R3 verifies against `new_vk`
/// (not the old `target_vk`). The authoring member (A) must be a pinned
/// successor member (RS-R1 consumed half) — A is in `new_members`. RS-R4
/// requires `new_vk` present iff Consumed.
#[tokio::test]
async fn reset_response_consumed_crosses_real_wire() {
    let pair = spawn_wire_pair(0x4A, 0x4B).await;
    let old_committee = FrostCommittee::generate();
    let successor = FrostCommittee::generate();
    let new_members = sorted_pair(pair.owner_a, pair.owner_b);
    let proposal_id = cross_proposal_and_cosign(&pair, &old_committee, &new_members).await;

    let response = mint_response(
        pair.community_id,
        &old_committee, // digest still binds the OLD target_vk
        ResetVerdict::Consumed,
        &successor, // Consumed: the SUCCESSOR committee signs
        Some(successor.target_vk),
        proposal_id,
        &new_members,
        pair.owner_a,
        hlc(900_000, "a-dev"),
        &pair.signing_a,
        false,
    );
    author_and_assert_response_crosses(&pair, response).await;

    pair.engine_a.shutdown().await.expect("shutdown a");
    pair.engine_b.shutdown().await.expect("shutdown b");
}

/// Negative control: a `DfrostResetResponse` with a corrupted `group_sig`
/// is REJECTED by the verify gate once the proposal is present — proving
/// RS-R3 actually runs (rather than lenient-skipping) on the same
/// `verify_event` seam the wire-ingest path uses.
#[tokio::test]
async fn reset_response_bad_group_sig_is_rejected_by_verify_gate() {
    let pair = spawn_wire_pair(0x3A, 0x3B).await;
    let committee = FrostCommittee::generate();
    let new_members = sorted_pair(pair.owner_a, pair.owner_b);
    // Cross the proposal so it is materialized on A (RS-R3 will fire, not skip).
    let proposal_id = cross_proposal_and_cosign(&pair, &committee, &new_members).await;

    let tampered = mint_response(
        pair.community_id,
        &committee,
        ResetVerdict::Endorse,
        &committee,
        None,
        proposal_id,
        &new_members,
        pair.owner_a,
        hlc(900_000, "a-dev"),
        &pair.signing_a,
        true,
    );
    let outcome = pair
        .engine_a
        .insert_local_event(tampered)
        .await
        .expect("insert returns (rejection is Ok(Rejected), not Err)");
    assert!(
        matches!(
            outcome,
            InsertOutcome::Rejected(
                harmony_app::community_membership::VerifyError::DfrostResetResponseSigInvalid
            )
        ),
        "corrupted group_sig must be rejected by RS-R3, got {outcome:?}"
    );

    pair.engine_a.shutdown().await.expect("shutdown a");
    pair.engine_b.shutdown().await.expect("shutdown b");
}
