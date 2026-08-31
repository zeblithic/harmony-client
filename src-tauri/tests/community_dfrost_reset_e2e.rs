//! ZEB-1031 Task 10: two-node headless e2e for the D-FROST committee-reset
//! ceremony (spec §11: `docs/superpowers/specs/2026-08-30-zeb1031-dfrost-committee-reset-design.md`).
//!
//! ## Harness shape and why
//!
//! Every prior ZEB-1031 task (and ZEB-1030's own catch-up e2e,
//! `community_dfrost_integration.rs`) tests at the ENGINE level: two real
//! `DfrostLogEngine`/`CommunitySyncEngine`-shaped objects wired via mpsc
//! byte-relay bridges, never a spawned `harmony-app serve` binary and never
//! a `tauri::test` mock-IPC dispatch. This file follows that same
//! established convention — it is the "headless two-node harness" ZEB-1030
//! actually uses, not the separate `e2e-harness` crate (which spawns real
//! processes for a different, older test family and has never been touched
//! by any ZEB-1031 commit).
//!
//! This file drives the DFROST side through REAL, wire-connected
//! `DfrostLogEngine`s (the orchestrated-node pattern from
//! `community_dfrost_transport_integration.rs`) — real FROST-Ristretto255
//! DKG, real threshold-sign ceremonies, real auto-drive ticks. It is the
//! first ZEB-1031 test to complete the "last mile" that
//! `reset_marker_and_consumed_response_auto_drive_across_two_engines_zeb1031`
//! (Task 8) explicitly left uncovered: turning a completed threshold
//! signature into a `DfrostResetResponse` MEMBERSHIP event.
//!
//! ## Membership-layer scoping decision (read before extending this file)
//!
//! The membership layer (propose/cosign/response events, admin quorum,
//! phase evaluation) is driven through a REAL `CommunityState` — the exact
//! `materialize_with_now` / `evaluate_reset_phases` state machine
//! production runs — but events are admitted via
//! `CommunityState::insert_verified_for_test` (a `test-fixtures`-gated
//! trusted-write seam) rather than `CommunitySyncEngine::insert_local_event`
//! wired to a live wire+CAS transport, and applied identically to BOTH
//! nodes' `CommunityState` directly rather than crossing a membership wire.
//!
//! This is a deliberate, load-bearing scoping decision, not a shortcut of
//! convenience: `OwnerAddr` in this codebase has TWO cryptographically
//! incompatible derivations that both happen to produce a bare `[u8; 16]`.
//! DFROST's `verify_signed_committee_event` requires
//! `harmony_identity::Identity::address_hash` (`SHA256(x25519 || ed25519)`).
//! Membership's cert-bootstrap (`EnrollmentCert` / `PubKeyBundle`, the
//! `mint_test_owner` test helper's own scheme) computes
//! `SHA256(ed25519)` alone — a DIFFERENT hash over a DIFFERENT input, with
//! no shared preimage. A single `OwnerAddr` cannot satisfy both hash
//! functions at once, so a person cannot simultaneously be a real DFROST
//! committee member (needs `harmony_identity`-derived address) and pass
//! through the membership layer's mandatory `EnrollmentCert` bootstrap
//! (which can only mint `harmony_owner`-derived addresses) — every
//! membership event's signature resolves EITHER via a carried
//! `EnrollmentCert` (Join/PendingJoin) or via `enrolled_device_keys`
//! learned from one; there is no third path. This is genuinely new ground:
//! no earlier ZEB-1031 task combines a real live DFROST committee with real
//! membership event verification for the SAME identities. Using
//! `insert_verified_for_test` sidesteps the incompatible envelope scheme
//! while still exercising the actual reset-lifecycle state machine (phase
//! evaluation, admin quorum, marker admissibility, RS-M gates) with real
//! events — the wire+cert path for MEMBERSHIP specifically is already
//! covered by `community_open_flow_integration.rs`. The DFROST layer's own
//! wire+verify path (the genuinely novel, previously-untested half) stays
//! fully real throughout.

#![cfg(feature = "test-fixtures")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use frost_ristretto255::{
    self as frost,
    keys::{IdentifierList, KeyPackage, PublicKeyPackage},
    rand_core::OsRng,
    Identifier,
};
use tokio::sync::{mpsc, Mutex};

use harmony_app::community_dfrost_crypto::{
    identifier_for_index, verifying_key_to_bytes, verifying_share_to_bytes,
};
use harmony_app::community_dfrost_log::{CommitteeState, DfrostLog};
use harmony_app::community_dfrost_log_engine::{
    CatchupOutcome, DfrostLogEngine, DfrostLogEngineParams, DfrostLogRegistry,
    DfrostOrchestratorConfig,
};
use harmony_app::community_dfrost_types::{DfrostEventKind, ThresholdSignPayload};
use harmony_app::community_membership::{
    EventId, EventPayload, MaterializedMembership, MembershipEventKind, ProposalKind, ResetPhase,
    ResetVerdict, SignedMembershipEvent, RESET_FINALITY_MS, RESET_VETO_WINDOW_FLOOR_MS,
};
use harmony_app::community_state_crdt::CommunityState;
use harmony_app::community_state_sync::IdentityResolver;
use harmony_app::community_voting_core::{MemberAttrs, MembershipSnapshot};
use harmony_app::community_voting_log::{MembershipSnapshotResolver, SnapshotResolverError};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_app::{
    dfrost_initiate_dkg_core, dfrost_reset_membership_from_state, dm_signing,
    production_dkg_driver, DfrostCoreHandles,
};

type MockRt = tauri::test::MockRuntime;
type DfrostLogsMap = Arc<Mutex<std::collections::HashMap<SpaceId, Arc<Mutex<DfrostLog>>>>>;

// ─── Identity ───────────────────────────────────────────────────────────────

/// One person's identity, uniform across BOTH layers (see module doc — this
/// is what makes that uniformity possible: `harmony_identity`-derived, real
/// FROST-verifiable, used for every event this person signs).
#[derive(Clone)]
struct Person {
    owner: OwnerAddr,
    pub64: [u8; 64],
    signing_key: ed25519_dalek::SigningKey,
    x25519_priv: [u8; 32],
}

fn person(seed: u8) -> Person {
    let priv_id = harmony_identity::PrivateIdentity::from_seed(&[seed; 32]);
    let owner = OwnerAddr(priv_id.identity.address_hash);
    let pub64 = priv_id.identity.to_public_bytes();
    let private_bytes = priv_id.to_private_bytes();
    let mut ed_secret = [0u8; 32];
    ed_secret.copy_from_slice(&private_bytes[32..64]);
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_secret);
    let x25519_priv = *dm_signing::ed25519_priv_to_x25519(&signing_key);
    Person {
        owner,
        pub64,
        signing_key,
        x25519_priv,
    }
}

/// RS-P2 mirror: `propose_dfrost_reset_impl` sorts+dedups `new_members`
/// before minting — `check_ceremony_init_admissible`'s pinned-successor
/// check compares `pending_reset.new_members` (carried verbatim from the
/// proposal) against the DKG initiator's OWN sorted member list by exact
/// `Vec` equality, so an unsorted proposal would spuriously reject a
/// correctly-sorted `dfrost_initiate_dkg_core` call.
fn sorted_pair(a: OwnerAddr, b: OwnerAddr) -> Vec<OwnerAddr> {
    let mut v = vec![a, b];
    v.sort();
    v
}

fn hlc(wall_ms: u64, device: &str) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: device.to_string(),
    }
}

struct StaticResolver(BTreeMap<OwnerAddr, [u8; 64]>);

#[async_trait::async_trait]
impl IdentityResolver for StaticResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        self.0.get(addr).copied()
    }
}

fn resolver_of(people: &[&Person]) -> Arc<dyn IdentityResolver + Send + Sync> {
    let mut map = BTreeMap::new();
    for p in people {
        map.insert(p.owner, p.pub64);
    }
    Arc::new(StaticResolver(map))
}

// ─── Membership genesis + direct-insert helpers ────────────────────────────

fn mint(
    kind: MembershipEventKind,
    actor: OwnerAddr,
    at: Hlc,
    signing_key: &ed25519_dalek::SigningKey,
) -> SignedMembershipEvent {
    let mut id = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut id);
    let payload = EventPayload {
        id,
        community_id: SPACE_ID,
        kind,
        actor,
        at,
    };
    harmony_app::community_membership::sign_event(&payload, signing_key)
        .expect("sign membership event")
}

/// Insert `event` into BOTH nodes' `CommunityState` via the trusted-write
/// seam (module doc: sidesteps the harmony_identity/harmony_owner envelope
/// mismatch; the real reset-lifecycle state machine below is otherwise
/// exercised unmodified).
async fn insert_both(
    state_a: &Arc<Mutex<CommunityState>>,
    state_b: &Arc<Mutex<CommunityState>>,
    event: SignedMembershipEvent,
) {
    state_a.lock().await.insert_verified_for_test(event.clone());
    state_b.lock().await.insert_verified_for_test(event);
}

const SPACE_ID: SpaceId = SpaceId([0x51; 16]);

/// Genesis: alice (founder/admin), bob, carol all Join + power=100, then
/// admin_quorum bumped 1→2 via a self-satisfying `ChangeQuorum` proposal
/// (spec §3.2's "proposer counts as 1" — at quorum=1 alice's own proposal
/// resolves immediately, no separate countersign ceremony needed to reach
/// the quorum=2 the reset propose/cosign flows below actually exercise).
async fn seed_genesis(
    state_a: &Arc<Mutex<CommunityState>>,
    state_b: &Arc<Mutex<CommunityState>>,
    alice: &Person,
    bob: &Person,
    carol: &Person,
) {
    insert_both(
        state_a,
        state_b,
        mint(
            MembershipEventKind::Join,
            alice.owner,
            hlc(100, "alice"),
            &alice.signing_key,
        ),
    )
    .await;
    insert_both(
        state_a,
        state_b,
        mint(
            MembershipEventKind::Join,
            bob.owner,
            hlc(200, "bob"),
            &bob.signing_key,
        ),
    )
    .await;
    insert_both(
        state_a,
        state_b,
        mint(
            MembershipEventKind::Join,
            carol.owner,
            hlc(300, "carol"),
            &carol.signing_key,
        ),
    )
    .await;
    insert_both(
        state_a,
        state_b,
        mint(
            MembershipEventKind::SetPower {
                target: bob.owner,
                level: 100,
            },
            alice.owner,
            hlc(400, "alice"),
            &alice.signing_key,
        ),
    )
    .await;
    insert_both(
        state_a,
        state_b,
        mint(
            MembershipEventKind::SetPower {
                target: carol.owner,
                level: 100,
            },
            alice.owner,
            hlc(500, "alice"),
            &alice.signing_key,
        ),
    )
    .await;
    insert_both(
        state_a,
        state_b,
        mint(
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::ChangeQuorum { new_quorum: 2 },
            },
            alice.owner,
            hlc(600, "alice"),
            &alice.signing_key,
        ),
    )
    .await;
}

/// Read the materialized reset-proposal view for `proposal_id` at `now_ms`,
/// on whichever `state` is passed.
async fn reset_view(
    state: &Arc<Mutex<CommunityState>>,
    admin: OwnerAddr,
    now_ms: u64,
    proposal_id: EventId,
) -> Option<harmony_app::community_membership::ResetProposalView> {
    let g = state.lock().await;
    let m = g.materialized_with_now(admin, now_ms);
    m.reset_proposals.into_iter().find(|p| p.id == proposal_id)
}

// ─── Live membership resolver (DFROST engine's window into the real state) ─

/// Wraps a real `CommunityState` + a shared, test-controlled clock so
/// `DfrostLogEngine`'s auto-drive tick sees the SAME materialized view
/// `reset_view` reads — advancing `now_ms` is how the disaster flow
/// simulates the veto-window + finality wait without any real sleeping.
///
/// ZEB-1031 final whole-branch review C1: `reset_membership_now` routes
/// through the PRODUCTION `dfrost_reset_membership_from_state` helper
/// (injecting this struct's test clock through that helper's `now_ms`
/// seam) rather than calling `materialized_with_now` directly — every
/// prior reset-auto-drive test exercised a resolver that diverged from
/// production on exactly the now-floor property the C1 fix restores, so
/// this file drives the real production seam to close that class of
/// masked regression.
struct LiveMembershipResolver {
    state: Arc<Mutex<CommunityState>>,
    admin: OwnerAddr,
    now_ms: Arc<AtomicU64>,
    members_for_snapshot: Vec<OwnerAddr>,
}

#[async_trait::async_trait]
impl MembershipSnapshotResolver for LiveMembershipResolver {
    async fn snapshot_at(
        &self,
        _community_id: SpaceId,
        _at: &Hlc,
    ) -> Result<MembershipSnapshot, SnapshotResolverError> {
        let members = self
            .members_for_snapshot
            .iter()
            .map(|a| {
                (
                    *a,
                    MemberAttrs {
                        power: 100,
                        vouching_depth: 0,
                    },
                )
            })
            .collect();
        Ok(MembershipSnapshot { members })
    }

    async fn reset_membership_now(
        &self,
        _community_id: SpaceId,
    ) -> Result<MaterializedMembership, SnapshotResolverError> {
        let g = self.state.lock().await;
        Ok(dfrost_reset_membership_from_state(
            &g,
            self.admin,
            Some(self.now_ms.load(Ordering::SeqCst)),
        ))
    }
}

// ─── Orchestrated DFROST node (mirrors community_dfrost_transport_integration.rs) ─

struct OrchestratedNode {
    log: Arc<Mutex<DfrostLog>>,
    registry: Arc<DfrostLogRegistry<MockRt>>,
    handles: DfrostCoreHandles<MockRt>,
}

fn orchestrated_node(
    device_id: &str,
    self_addr: OwnerAddr,
    signing_key: &ed25519_dalek::SigningKey,
    resolver_people: &[&Person],
) -> OrchestratedNode {
    let log = Arc::new(Mutex::new(DfrostLog::new()));
    let dfrost_logs: DfrostLogsMap = Arc::new(Mutex::new(std::collections::HashMap::new()));
    {
        let map = dfrost_logs.clone();
        map.try_lock()
            .expect("fresh map")
            .insert(SPACE_ID, log.clone());
    }
    let registry: Arc<DfrostLogRegistry<MockRt>> = Arc::new(DfrostLogRegistry::new());
    let handles = DfrostCoreHandles::<MockRt>::for_tests(
        Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            device_id.to_string(),
        ))),
        harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        device_id.to_string(),
        self_addr,
        Arc::new(signing_key.clone()),
        dfrost_logs,
        Some(resolver_of(resolver_people)),
        Some(registry.clone()),
    );
    OrchestratedNode {
        log,
        registry,
        handles,
    }
}

fn test_orchestrator_config() -> DfrostOrchestratorConfig {
    DfrostOrchestratorConfig {
        tick_interval: Duration::from_millis(15),
        rebroadcast_interval: Duration::from_millis(100),
        initiator_quiet_deadline: Duration::from_secs(60),
        stale_replace_threshold: Duration::from_secs(60),
        max_restart_attempts: 3,
        recovery_quiet_deadline: Duration::from_secs(60),
    }
}

#[allow(clippy::too_many_arguments)]
async fn start_orchestrated(
    node: &OrchestratedNode,
    self_addr: OwnerAddr,
    self_x_priv: [u8; 32],
    resolver_people: &[&Person],
    membership_resolver: Arc<dyn MembershipSnapshotResolver>,
    publisher_tx: mpsc::Sender<Vec<u8>>,
    subscriber_rx: mpsc::Receiver<Vec<u8>>,
) -> Arc<DfrostLogEngine<MockRt>> {
    let driver = production_dkg_driver::<MockRt>(node.handles.clone(), None);
    DfrostLogRegistry::register(
        &node.registry,
        DfrostLogEngineParams {
            community_id: SPACE_ID,
            dfrost_log: node.log.clone(),
            publisher_tx,
            subscriber_rx,
            app_handle: None,
            self_addr,
            self_x25519_priv: self_x_priv,
            identity_resolver: resolver_of(resolver_people),
            registry_weak: None,
            driver: Some(driver),
            membership_resolver: Some(membership_resolver),
            orchestrator_config: test_orchestrator_config(),
            persist: None,
        },
    )
    .await
}

/// Wire two mpsc pairs into a symmetric byte-relay bridge: A's publisher →
/// B's subscriber, B's publisher → A's subscriber. Returns the four
/// channel halves each side's engine actually needs.
#[allow(clippy::type_complexity)]
fn make_bridge() -> (
    mpsc::Sender<Vec<u8>>,
    mpsc::Receiver<Vec<u8>>,
    mpsc::Sender<Vec<u8>>,
    mpsc::Receiver<Vec<u8>>,
) {
    let (a_pub_tx, mut a_pub_rx) = mpsc::channel::<Vec<u8>>(128);
    let (b_pub_tx, mut b_pub_rx) = mpsc::channel::<Vec<u8>>(128);
    let (a_sub_tx, a_sub_rx) = mpsc::channel::<Vec<u8>>(128);
    let (b_sub_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(128);
    tokio::spawn(async move {
        while let Some(p) = a_pub_rx.recv().await {
            if b_sub_tx.send(p).await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        while let Some(p) = b_pub_rx.recv().await {
            if a_sub_tx.send(p).await.is_err() {
                break;
            }
        }
    });
    (a_pub_tx, a_sub_rx, b_pub_tx, b_sub_rx)
}

// ─── Generic poll helper ────────────────────────────────────────────────────

async fn poll_until<F, Fut>(label: &str, timeout: Duration, mut cond: F) -> Result<(), String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond().await {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("poll_until({label}) timed out after {timeout:?}"));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ─── Old (pre-reset) committee: real 2-of-3 dealer-generated material ─────
//
// alice + bob are the two REAL live participants ("2-of-3-ish committee
// live across two nodes" — brief's own framing); carol is a genuine THIRD
// dealer share-holder who never runs, matching the disaster scenario where
// only 2 of 3 committee members are actually reachable. Dealer-generated
// (not a live DKG) is the house shortcut for seeding an ALREADY-ACTIVE
// committee — proven by `reset_response_ceremony_converges_and_tags_purpose_across_two_engines_zeb1031`.
struct OldCommittee {
    joint_vk: [u8; 32],
    identifier_map: BTreeMap<OwnerAddr, Identifier>,
    verifying_shares: BTreeMap<OwnerAddr, [u8; 32]>,
    key_pkg_alice: KeyPackage,
    key_pkg_bob: KeyPackage,
}

fn seed_old_committee(
    log_a: &mut DfrostLog,
    log_b: &mut DfrostLog,
    alice: &Person,
    bob: &Person,
    carol: &Person,
) -> OldCommittee {
    let mut members = vec![alice.owner, bob.owner, carol.owner];
    members.sort();
    let (shares, pub_pkg) = frost::keys::generate_with_dealer(3, 2, IdentifierList::Default, OsRng)
        .expect("dealer keygen 2-of-3");
    let joint_vk = verifying_key_to_bytes(pub_pkg.verifying_key());
    let identifier_map = CommitteeState::build_identifier_map(&members);
    let mut verifying_shares = BTreeMap::new();
    for addr in &members {
        let id = identifier_map.get(addr).unwrap();
        let share = pub_pkg.verifying_shares().get(id).unwrap();
        verifying_shares.insert(*addr, verifying_share_to_bytes(share));
    }
    let key_pkg_alice = KeyPackage::try_from(
        shares
            .get(identifier_map.get(&alice.owner).unwrap())
            .unwrap()
            .clone(),
    )
    .expect("alice key package");
    let key_pkg_bob = KeyPackage::try_from(
        shares
            .get(identifier_map.get(&bob.owner).unwrap())
            .unwrap()
            .clone(),
    )
    .expect("bob key package");

    let seed_log = |log: &mut DfrostLog, kp: KeyPackage| {
        log.committee_state.active = true;
        log.committee_state.current_epoch = 1;
        log.committee_state.members = members.clone();
        log.committee_state.threshold = 2;
        log.committee_state.max_signers = 3;
        log.committee_state.joint_verifying_key = Some(joint_vk);
        log.committee_state.verifying_shares = verifying_shares.clone();
        log.committee_state.identifier_map = identifier_map.clone();
        log.local_key_package = Some(kp);
    };
    seed_log(log_a, key_pkg_alice.clone());
    seed_log(log_b, key_pkg_bob.clone());

    OldCommittee {
        joint_vk,
        identifier_map,
        verifying_shares,
        key_pkg_alice,
        key_pkg_bob,
    }
}

// ─── Threshold-sign round-2 + aggregate + membership-mint (the "last mile") ─
//
// Mirrors `dfrost_contribute_threshold_sign`'s round-2 half exactly
// (`community_dfrost_ipc_integration.rs::contribute_threshold_sign_local`
// is the proven precedent for the round-2-sign shape; the aggregate +
// ResetResponse-mint half mirrors `lib.rs`'s `SignPurpose::ResetResponse`
// completion arm). Round-1 (the empty-share `ts` commit) is auto-driven by
// the orchestrator via `initiate_reset_response_ceremony` — this helper
// only covers what Task 8's own test explicitly left uncovered.

/// Round-2 sign for `self_addr` on `ceremony_id`: consumes stashed nonces,
/// builds the canonical signing-set `SigningPackage`, produces a
/// share-bearing `ts` event. Caller applies + (if wired to a live engine)
/// broadcasts it.
fn contribute_round2(
    log: &mut DfrostLog,
    self_addr: OwnerAddr,
    ceremony_id: [u8; 32],
    members: &[OwnerAddr],
    key_package: &KeyPackage,
    at: Hlc,
    signing_key: &ed25519_dalek::SigningKey,
) -> (
    harmony_app::community_dfrost_types::SignedCommitteeEvent,
    frost::SigningPackage,
) {
    let threshold = log.committee_state.threshold;
    let (nonces, signing_package, my_commitment_bytes, message_hash) = {
        let pending = log
            .committee_state
            .pending_sign
            .get_mut(&ceremony_id)
            .expect("pending_sign present (round-1 already landed)");
        let nonces_cbor = pending
            .local_nonces
            .take()
            .expect("local_nonces stashed by round-1 initiate");
        let nonces: frost::round1::SigningNonces =
            ciborium::from_reader(&nonces_cbor[..]).expect("decode local nonces");

        let signing_set: Vec<OwnerAddr> = pending
            .contributions
            .keys()
            .copied()
            .take(threshold as usize)
            .collect();
        assert!(
            signing_set.contains(&self_addr),
            "self must be in the canonical signing set"
        );

        let mut commitments_map: BTreeMap<Identifier, frost::round1::SigningCommitments> =
            BTreeMap::new();
        for addr in &signing_set {
            let (commitment_bytes, _share) = pending
                .contributions
                .get(addr)
                .expect("canonical signer present");
            let idx = members
                .iter()
                .position(|a| *a == *addr)
                .expect("signer must be a committee member");
            let id = identifier_for_index(idx);
            let commitments: frost::round1::SigningCommitments =
                ciborium::from_reader(&commitment_bytes[..]).expect("decode peer commitments");
            commitments_map.insert(id, commitments);
        }
        let my_commitment_bytes = pending
            .contributions
            .get(&self_addr)
            .expect("self contribution present")
            .0
            .clone();
        let message_hash = pending.message_hash;
        let signing_package = frost::SigningPackage::new(commitments_map, &message_hash);
        (nonces, signing_package, my_commitment_bytes, message_hash)
    };

    let sig_share =
        frost::round2::sign(&signing_package, &nonces, key_package).expect("round2::sign");
    let mut share_bytes = Vec::new();
    ciborium::into_writer(&sig_share, &mut share_bytes).expect("encode SignatureShare");

    let payload = ThresholdSignPayload {
        ceremony_id,
        message_hash,
        commitment_bytes: my_commitment_bytes,
        share_bytes,
    };
    let event = harmony_app::community_dfrost_log::build_signed_dfrost_event(
        signing_key,
        self_addr,
        DfrostEventKind::ThresholdSign,
        &payload,
        at,
    )
    .expect("build_signed ts (with share)");
    (event, signing_package)
}

/// Once `log`'s `pending_sign[ceremony_id]` holds `threshold` share-bearing
/// contributions, aggregate + mint the `DfrostResetResponse` membership
/// event and insert it on BOTH membership states. Returns `Some(group_sig)`
/// when aggregation actually ran (the caller on whichever side crosses
/// threshold first).
#[allow(clippy::too_many_arguments)]
async fn maybe_complete_reset_response(
    log: &Arc<Mutex<DfrostLog>>,
    members: &[OwnerAddr],
    pub_key_package: &PublicKeyPackage,
    ceremony_id: [u8; 32],
    proposal_id: EventId,
    verdict: ResetVerdict,
    new_vk: Option<[u8; 32]>,
    aggregator_addr: OwnerAddr,
    aggregator_signing_key: &ed25519_dalek::SigningKey,
    at: Hlc,
    state_a: &Arc<Mutex<CommunityState>>,
    state_b: &Arc<Mutex<CommunityState>>,
) -> Option<[u8; 64]> {
    let (threshold, shares_map, signing_package) = {
        let g = log.lock().await;
        let pending = g.committee_state.pending_sign.get(&ceremony_id)?;
        let threshold = g.committee_state.threshold as usize;
        let with_share: Vec<_> = pending
            .contributions
            .iter()
            .filter(|(_, (_, s))| !s.is_empty())
            .collect();
        if with_share.len() < threshold {
            return None;
        }
        let mut shares_map = BTreeMap::new();
        let mut commitments_map: BTreeMap<Identifier, frost::round1::SigningCommitments> =
            BTreeMap::new();
        for (addr, (commit_b, share_b)) in &pending.contributions {
            if share_b.is_empty() {
                continue;
            }
            let idx = members
                .iter()
                .position(|a| a == addr)
                .expect("committee member");
            let id = identifier_for_index(idx);
            let share: frost::round2::SignatureShare =
                ciborium::from_reader(&share_b[..]).expect("decode SignatureShare");
            let commit: frost::round1::SigningCommitments =
                ciborium::from_reader(&commit_b[..]).expect("decode SigningCommitments");
            shares_map.insert(id, share);
            commitments_map.insert(id, commit);
        }
        let signing_package = frost::SigningPackage::new(commitments_map, &pending.message_hash);
        (threshold, shares_map, signing_package)
    };
    let _ = threshold;

    let group_signature = frost::aggregate(&signing_package, &shares_map, pub_key_package)
        .expect("aggregate threshold signature");
    let sig_bytes: Vec<u8> = group_signature.serialize().expect("serialize signature");
    let group_sig: [u8; 64] = sig_bytes.try_into().expect("Schnorr signature is 64 bytes");

    let response_event = mint(
        MembershipEventKind::DfrostResetResponse {
            target_event_id: proposal_id,
            verdict,
            group_sig,
            new_vk,
        },
        aggregator_addr,
        at,
        aggregator_signing_key,
    );
    insert_both(state_a, state_b, response_event).await;

    // Mirror production's success-only clear (`dfrost_contribute_threshold_sign`).
    log.lock()
        .await
        .committee_state
        .pending_sign
        .remove(&ceremony_id);
    Some(group_sig)
}

/// Drive a full endorse/veto/consumed reset-response ceremony to
/// completion across the two orchestrated nodes: round-1 auto-fires from
/// `initiate_reset_response_ceremony` (already wired via the orchestrator
/// membership_resolver), this drives round-2 on both sides and completes
/// the ceremony on whichever side crosses threshold.
#[allow(clippy::too_many_arguments)]
async fn drive_reset_response(
    proposal_id: EventId,
    verdict: ResetVerdict,
    new_vk: Option<[u8; 32]>,
    alice: &Person,
    bob: &Person,
    log_a: &Arc<Mutex<DfrostLog>>,
    log_b: &Arc<Mutex<DfrostLog>>,
    engine_a: &Arc<DfrostLogEngine<MockRt>>,
    engine_b: &Arc<DfrostLogEngine<MockRt>>,
    members: &[OwnerAddr],
    pub_key_package: &PublicKeyPackage,
    key_pkg_alice: &KeyPackage,
    key_pkg_bob: &KeyPackage,
    at_ms: u64,
    state_a: &Arc<Mutex<CommunityState>>,
    state_b: &Arc<Mutex<CommunityState>>,
) -> [u8; 64] {
    // Alice initiates round-1 (or, for the auto-driven Consumed case, this
    // may already be in flight — `initiate_reset_response_ceremony` is
    // idempotent-safe to call again only via a fresh id, so callers that
    // know round-1 already fired should skip straight to convergence-wait).
    engine_a
        .initiate_reset_response_ceremony(proposal_id, verdict)
        .await
        .expect("alice initiates the reset-response ceremony");
    if let Err(e) = engine_b
        .initiate_reset_response_ceremony(proposal_id, verdict)
        .await
    {
        // Benign when bob's inbound apply already seeded the session.
        eprintln!("bob's initiate returned: {e}");
    }

    poll_until(
        "both sides see 2 round-1 contributions",
        Duration::from_secs(10),
        || async {
            let a2 = log_a
                .lock()
                .await
                .committee_state
                .pending_sign
                .values()
                .any(|p| p.contributions.len() >= 2);
            let b2 = log_b
                .lock()
                .await
                .committee_state
                .pending_sign
                .values()
                .any(|p| p.contributions.len() >= 2);
            a2 && b2
        },
    )
    .await
    .expect("round-1 convergence");

    let ceremony_id = {
        let g = log_a.lock().await;
        *g.committee_state
            .pending_sign
            .iter()
            .find(|(_, p)| p.contributions.len() >= 2)
            .map(|(id, _)| id)
            .expect("ceremony present")
    };

    let (event_a, _sp_a) = {
        let mut g = log_a.lock().await;
        contribute_round2(
            &mut g,
            alice.owner,
            ceremony_id,
            members,
            key_pkg_alice,
            hlc(at_ms, "alice"),
            &alice.signing_key,
        )
    };
    log_a
        .lock()
        .await
        .apply_with_identity(event_a.clone(), &alice.owner, &alice.x25519_priv)
        .expect("alice applies own round-2 ts");
    engine_a
        .publish_event(event_a)
        .await
        .expect("alice broadcasts round-2 ts");

    poll_until(
        "bob sees alice's round-2 share",
        Duration::from_secs(10),
        || async {
            log_b
                .lock()
                .await
                .committee_state
                .pending_sign
                .get(&ceremony_id)
                .map(|p| {
                    !p.contributions
                        .get(&alice.owner)
                        .map(|(_, s)| s.is_empty())
                        .unwrap_or(true)
                })
                .unwrap_or(false)
        },
    )
    .await
    .expect("alice's round-2 share converges to bob");

    let (event_b, _sp_b) = {
        let mut g = log_b.lock().await;
        contribute_round2(
            &mut g,
            bob.owner,
            ceremony_id,
            members,
            key_pkg_bob,
            hlc(at_ms + 100, "bob"),
            &bob.signing_key,
        )
    };
    log_b
        .lock()
        .await
        .apply_with_identity(event_b.clone(), &bob.owner, &bob.x25519_priv)
        .expect("bob applies own round-2 ts");
    engine_b
        .publish_event(event_b)
        .await
        .expect("bob broadcasts round-2 ts");

    poll_until(
        "alice sees bob's round-2 share",
        Duration::from_secs(10),
        || async {
            log_a
                .lock()
                .await
                .committee_state
                .pending_sign
                .get(&ceremony_id)
                .map(|p| {
                    !p.contributions
                        .get(&bob.owner)
                        .map(|(_, s)| s.is_empty())
                        .unwrap_or(true)
                })
                .unwrap_or(false)
        },
    )
    .await
    .expect("bob's round-2 share converges to alice");

    // Bob crosses threshold last on his own log (he applied his own share
    // there directly) — aggregate on his side.
    let sig = maybe_complete_reset_response(
        log_b,
        members,
        pub_key_package,
        ceremony_id,
        proposal_id,
        verdict,
        new_vk,
        bob.owner,
        &bob.signing_key,
        hlc(at_ms + 200, "bob"),
        state_a,
        state_b,
    )
    .await
    .expect("threshold reached — aggregate must produce a signature");

    // Clear the mirrored session on alice's log too (production's
    // per-node clear happens on whichever node calls
    // `dfrost_contribute_threshold_sign`; here both sides drove round-2
    // manually, so both sides' sessions are cleared explicitly).
    log_a
        .lock()
        .await
        .committee_state
        .pending_sign
        .remove(&ceremony_id);

    sig
}

// ─── Flow 1: disaster to completion ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flow1_disaster_to_completion() {
    let alice = person(0xA1);
    let bob = person(0xB2);
    let carol = person(0xC3);

    let state_a = Arc::new(Mutex::new(CommunityState::new(SPACE_ID)));
    let state_b = Arc::new(Mutex::new(CommunityState::new(SPACE_ID)));
    seed_genesis(&state_a, &state_b, &alice, &bob, &carol).await;

    let node_a = orchestrated_node(
        "node-a",
        alice.owner,
        &alice.signing_key,
        &[&alice, &bob, &carol],
    );
    let node_b = orchestrated_node(
        "node-b",
        bob.owner,
        &bob.signing_key,
        &[&alice, &bob, &carol],
    );
    let old = seed_old_committee(
        &mut node_a.log.try_lock().unwrap(),
        &mut node_b.log.try_lock().unwrap(),
        &alice,
        &bob,
        &carol,
    );

    let now_ms = Arc::new(AtomicU64::new(10_000));
    let members_all = {
        let mut m = vec![alice.owner, bob.owner, carol.owner];
        m.sort();
        m
    };
    let resolver_a: Arc<dyn MembershipSnapshotResolver> = Arc::new(LiveMembershipResolver {
        state: state_a.clone(),
        admin: alice.owner,
        now_ms: now_ms.clone(),
        members_for_snapshot: members_all.clone(),
    });
    let resolver_b: Arc<dyn MembershipSnapshotResolver> = Arc::new(LiveMembershipResolver {
        state: state_b.clone(),
        admin: alice.owner,
        now_ms: now_ms.clone(),
        members_for_snapshot: members_all.clone(),
    });

    let (a_pub, a_sub, b_pub, b_sub) = make_bridge();
    let engine_a = start_orchestrated(
        &node_a,
        alice.owner,
        alice.x25519_priv,
        &[&alice, &bob, &carol],
        resolver_a,
        a_pub,
        a_sub,
    )
    .await;
    let engine_b = start_orchestrated(
        &node_b,
        bob.owner,
        bob.x25519_priv,
        &[&alice, &bob, &carol],
        resolver_b,
        b_pub,
        b_sub,
    )
    .await;

    // ── Propose (alice) + cosign (bob) to admin quorum=2 ──
    let target_epoch = 1u64;
    let proposal = mint(
        MembershipEventKind::DfrostResetProposal {
            target_vk: old.joint_vk,
            target_epoch,
            new_members: sorted_pair(alice.owner, bob.owner),
            new_threshold: 2,
            veto_window_ms: RESET_VETO_WINDOW_FLOOR_MS,
        },
        alice.owner,
        hlc(10_000, "alice"),
        &alice.signing_key,
    );
    let proposal_id = proposal.id;
    insert_both(&state_a, &state_b, proposal).await;

    let cosign = mint(
        MembershipEventKind::DfrostResetCosign {
            target_event_id: proposal_id,
        },
        bob.owner,
        hlc(10_100, "bob"),
        &bob.signing_key,
    );
    insert_both(&state_a, &state_b, cosign).await;

    let view = reset_view(
        &state_a,
        alice.owner,
        now_ms.load(Ordering::SeqCst),
        proposal_id,
    )
    .await
    .expect("proposal present");
    assert_eq!(
        view.phase,
        ResetPhase::Window,
        "quorum reached, veto window open"
    );
    assert_eq!(view.signers.len(), 2);

    // ── Advance past veto window + 48h finality — no real sleeping ──
    let authorize_at = 10_100 + RESET_VETO_WINDOW_FLOOR_MS + RESET_FINALITY_MS + 1;
    now_ms.store(authorize_at, Ordering::SeqCst);

    let view = reset_view(&state_a, alice.owner, authorize_at, proposal_id)
        .await
        .expect("proposal present");
    assert_eq!(
        view.phase,
        ResetPhase::Authorized,
        "disaster path authorizes past deadline+finality with no veto"
    );

    // ── Marker auto-applies on BOTH nodes (no manual author_dfrost_reset_marker call) ──
    poll_until(
        "both nodes deactivate the old committee via auto-authored marker",
        Duration::from_secs(10),
        || async {
            let a = !node_a.log.lock().await.committee_state.active;
            let b = !node_b.log.lock().await.committee_state.active;
            a && b
        },
    )
    .await
    .expect("marker auto-drive");

    {
        let la = node_a.log.lock().await;
        let lb = node_b.log.lock().await;
        assert_eq!(
            la.committee_state.vk_history.len(),
            1,
            "alice's vk_history records the retired committee"
        );
        assert_eq!(
            lb.committee_state.vk_history.len(),
            1,
            "bob's vk_history records the retired committee"
        );
        assert_eq!(la.committee_state.vk_history[0].reset_id, proposal_id);
        assert_eq!(
            la.committee_state
                .pending_reset
                .as_ref()
                .map(|p| p.new_members.clone()),
            Some(sorted_pair(alice.owner, bob.owner))
        );
    }

    // ── Successor DKG — the ONE manual step (pinned members/threshold) ──
    let ceremony_hex = dfrost_initiate_dkg_core::<MockRt, MockRt>(
        &node_a.handles,
        None,
        SPACE_ID,
        vec![alice.owner, bob.owner],
        2,
    )
    .await
    .expect("alice initiates successor DKG");
    assert_eq!(ceremony_hex.len(), 64);

    poll_until(
        "both nodes converge to the new active committee",
        Duration::from_secs(20),
        || async {
            let a = node_a.log.lock().await.committee_state.active;
            let b = node_b.log.lock().await.committee_state.active;
            a && b
        },
    )
    .await
    .expect("successor DKG convergence");

    let (new_vk, _new_epoch, new_pub_key_package, new_key_pkg_alice, new_key_pkg_bob) = {
        let la = node_a.log.lock().await;
        let lb = node_b.log.lock().await;
        assert_eq!(
            la.committee_state.joint_verifying_key,
            lb.committee_state.joint_verifying_key
        );
        assert_eq!(
            la.committee_state.current_epoch,
            target_epoch + 1,
            "successor epoch is old+1"
        );
        assert_eq!(lb.committee_state.current_epoch, target_epoch + 1);
        let vk = la
            .committee_state
            .joint_verifying_key
            .expect("new vk present");
        let pkp = PublicKeyPackage::new(
            {
                let mut m = BTreeMap::new();
                for (addr, bytes) in &la.committee_state.verifying_shares {
                    let id = la.committee_state.identifier_map.get(addr).unwrap();
                    m.insert(
                        *id,
                        frost::keys::VerifyingShare::deserialize(bytes).expect("verifying share"),
                    );
                }
                m
            },
            frost::VerifyingKey::deserialize(&vk).expect("verifying key"),
            None,
        );
        (
            vk,
            la.committee_state.current_epoch,
            pkp,
            la.local_key_package
                .clone()
                .expect("alice's new key package"),
            lb.local_key_package.clone().expect("bob's new key package"),
        )
    };

    // ── Consumed response — round-1 is auto-driven by maybe_auto_drive_reset ──
    poll_until(
        "both nodes auto-initiate the Consumed ceremony round-1",
        Duration::from_secs(10),
        || async {
            let a = node_a
                .log
                .lock()
                .await
                .committee_state
                .pending_sign
                .values()
                .any(|p| {
                    p.purpose
                        == harmony_app::community_dfrost_log::SignPurpose::ResetResponse {
                            proposal_id,
                            verdict: ResetVerdict::Consumed,
                            new_vk: Some(new_vk),
                        }
                });
            let b = node_b
                .log
                .lock()
                .await
                .committee_state
                .pending_sign
                .values()
                .any(|p| {
                    p.purpose
                        == harmony_app::community_dfrost_log::SignPurpose::ResetResponse {
                            proposal_id,
                            verdict: ResetVerdict::Consumed,
                            new_vk: Some(new_vk),
                        }
                });
            a && b
        },
    )
    .await
    .expect("Consumed ceremony auto-drive round-1");

    let new_members = {
        let mut m = vec![alice.owner, bob.owner];
        m.sort();
        m
    };

    poll_until(
        "both sides see 2 round-1 contributions on the Consumed ceremony",
        Duration::from_secs(10),
        || async {
            let a2 = node_a
                .log
                .lock()
                .await
                .committee_state
                .pending_sign
                .values()
                .any(|p| {
                    p.purpose
                        == harmony_app::community_dfrost_log::SignPurpose::ResetResponse {
                            proposal_id,
                            verdict: ResetVerdict::Consumed,
                            new_vk: Some(new_vk),
                        }
                        && p.contributions.len() >= 2
                });
            let b2 = node_b
                .log
                .lock()
                .await
                .committee_state
                .pending_sign
                .values()
                .any(|p| {
                    p.purpose
                        == harmony_app::community_dfrost_log::SignPurpose::ResetResponse {
                            proposal_id,
                            verdict: ResetVerdict::Consumed,
                            new_vk: Some(new_vk),
                        }
                        && p.contributions.len() >= 2
                });
            a2 && b2
        },
    )
    .await
    .expect("Consumed round-1 convergence");

    let consumed_ceremony_id = {
        let g = node_a.log.lock().await;
        *g.committee_state
            .pending_sign
            .iter()
            .find(|(_, p)| {
                p.purpose
                    == harmony_app::community_dfrost_log::SignPurpose::ResetResponse {
                        proposal_id,
                        verdict: ResetVerdict::Consumed,
                        new_vk: Some(new_vk),
                    }
            })
            .map(|(id, _)| id)
            .expect("consumed ceremony present")
    };

    let (event_a, _) = {
        let mut g = node_a.log.lock().await;
        contribute_round2(
            &mut g,
            alice.owner,
            consumed_ceremony_id,
            &new_members,
            &new_key_pkg_alice,
            hlc(authorize_at + 1_000, "alice"),
            &alice.signing_key,
        )
    };
    node_a
        .log
        .lock()
        .await
        .apply_with_identity(event_a.clone(), &alice.owner, &alice.x25519_priv)
        .expect("alice applies own consumed round-2");
    engine_a
        .publish_event(event_a)
        .await
        .expect("alice broadcasts consumed round-2");

    poll_until(
        "bob sees alice's consumed round-2 share",
        Duration::from_secs(10),
        || async {
            node_b
                .log
                .lock()
                .await
                .committee_state
                .pending_sign
                .get(&consumed_ceremony_id)
                .map(|p| {
                    !p.contributions
                        .get(&alice.owner)
                        .map(|(_, s)| s.is_empty())
                        .unwrap_or(true)
                })
                .unwrap_or(false)
        },
    )
    .await
    .expect("alice's consumed round-2 converges to bob");

    let (event_b, _) = {
        let mut g = node_b.log.lock().await;
        contribute_round2(
            &mut g,
            bob.owner,
            consumed_ceremony_id,
            &new_members,
            &new_key_pkg_bob,
            hlc(authorize_at + 1_100, "bob"),
            &bob.signing_key,
        )
    };
    node_b
        .log
        .lock()
        .await
        .apply_with_identity(event_b.clone(), &bob.owner, &bob.x25519_priv)
        .expect("bob applies own consumed round-2");
    engine_b
        .publish_event(event_b)
        .await
        .expect("bob broadcasts consumed round-2");

    poll_until(
        "alice sees bob's consumed round-2 share",
        Duration::from_secs(10),
        || async {
            node_a
                .log
                .lock()
                .await
                .committee_state
                .pending_sign
                .get(&consumed_ceremony_id)
                .map(|p| {
                    !p.contributions
                        .get(&bob.owner)
                        .map(|(_, s)| s.is_empty())
                        .unwrap_or(true)
                })
                .unwrap_or(false)
        },
    )
    .await
    .expect("bob's consumed round-2 converges to alice");

    maybe_complete_reset_response(
        &node_b.log,
        &new_members,
        &new_pub_key_package,
        consumed_ceremony_id,
        proposal_id,
        ResetVerdict::Consumed,
        Some(new_vk),
        bob.owner,
        &bob.signing_key,
        hlc(authorize_at + 1_200, "bob"),
        &state_a,
        &state_b,
    )
    .await
    .expect("Consumed threshold reached — aggregate must produce a signature");
    node_a
        .log
        .lock()
        .await
        .committee_state
        .pending_sign
        .remove(&consumed_ceremony_id);

    // ── Final assertions ──
    {
        let la = node_a.log.lock().await;
        let lb = node_b.log.lock().await;
        assert!(la.committee_state.active && lb.committee_state.active);
        assert_eq!(la.committee_state.joint_verifying_key, Some(new_vk));
        assert_eq!(lb.committee_state.joint_verifying_key, Some(new_vk));
        assert_eq!(la.committee_state.vk_history.len(), 1);
        assert_eq!(lb.committee_state.vk_history.len(), 1);
    }
    let final_view = reset_view(&state_a, alice.owner, authorize_at + 2_000, proposal_id)
        .await
        .expect("proposal present");
    assert_eq!(
        final_view.phase,
        ResetPhase::Consumed,
        "Consumed response present in membership state"
    );
    let final_view_b = reset_view(&state_b, alice.owner, authorize_at + 2_000, proposal_id)
        .await
        .expect("proposal present on b");
    assert_eq!(
        final_view_b.phase,
        ResetPhase::Consumed,
        "both nodes converge on Consumed"
    );

    drop(engine_a);
    drop(engine_b);
}

// ─── Flow 2: disaster vetoed ────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flow2_disaster_vetoed() {
    let alice = person(0xA4);
    let bob = person(0xB5);
    let carol = person(0xC6);

    let state_a = Arc::new(Mutex::new(CommunityState::new(SPACE_ID)));
    let state_b = Arc::new(Mutex::new(CommunityState::new(SPACE_ID)));
    seed_genesis(&state_a, &state_b, &alice, &bob, &carol).await;

    let node_a = orchestrated_node(
        "node-a",
        alice.owner,
        &alice.signing_key,
        &[&alice, &bob, &carol],
    );
    let node_b = orchestrated_node(
        "node-b",
        bob.owner,
        &bob.signing_key,
        &[&alice, &bob, &carol],
    );
    let old = seed_old_committee(
        &mut node_a.log.try_lock().unwrap(),
        &mut node_b.log.try_lock().unwrap(),
        &alice,
        &bob,
        &carol,
    );

    let now_ms = Arc::new(AtomicU64::new(10_000));
    let members_all = {
        let mut m = vec![alice.owner, bob.owner, carol.owner];
        m.sort();
        m
    };
    let resolver_a: Arc<dyn MembershipSnapshotResolver> = Arc::new(LiveMembershipResolver {
        state: state_a.clone(),
        admin: alice.owner,
        now_ms: now_ms.clone(),
        members_for_snapshot: members_all.clone(),
    });
    let resolver_b: Arc<dyn MembershipSnapshotResolver> = Arc::new(LiveMembershipResolver {
        state: state_b.clone(),
        admin: alice.owner,
        now_ms: now_ms.clone(),
        members_for_snapshot: members_all.clone(),
    });

    let (a_pub, a_sub, b_pub, b_sub) = make_bridge();
    let engine_a = start_orchestrated(
        &node_a,
        alice.owner,
        alice.x25519_priv,
        &[&alice, &bob, &carol],
        resolver_a,
        a_pub,
        a_sub,
    )
    .await;
    let engine_b = start_orchestrated(
        &node_b,
        bob.owner,
        bob.x25519_priv,
        &[&alice, &bob, &carol],
        resolver_b,
        b_pub,
        b_sub,
    )
    .await;

    let target_epoch = 1u64;
    let proposal = mint(
        MembershipEventKind::DfrostResetProposal {
            target_vk: old.joint_vk,
            target_epoch,
            new_members: sorted_pair(alice.owner, bob.owner),
            new_threshold: 2,
            veto_window_ms: RESET_VETO_WINDOW_FLOOR_MS,
        },
        alice.owner,
        hlc(10_000, "alice"),
        &alice.signing_key,
    );
    let proposal_id = proposal.id;
    insert_both(&state_a, &state_b, proposal).await;
    let cosign = mint(
        MembershipEventKind::DfrostResetCosign {
            target_event_id: proposal_id,
        },
        bob.owner,
        hlc(10_100, "bob"),
        &bob.signing_key,
    );
    insert_both(&state_a, &state_b, cosign).await;

    let pub_key_package = {
        let g = node_a.log.lock().await;
        PublicKeyPackage::new(
            {
                let mut m = BTreeMap::new();
                for (addr, bytes) in &g.committee_state.verifying_shares {
                    let id = g.committee_state.identifier_map.get(addr).unwrap();
                    m.insert(
                        *id,
                        frost::keys::VerifyingShare::deserialize(bytes).expect("verifying share"),
                    );
                }
                m
            },
            frost::VerifyingKey::deserialize(&old.joint_vk).expect("verifying key"),
            None,
        )
    };

    // ── Mid-window: committee runs the veto ceremony ──
    drive_reset_response(
        proposal_id,
        ResetVerdict::Veto,
        None,
        &alice,
        &bob,
        &node_a.log,
        &node_b.log,
        &engine_a,
        &engine_b,
        &members_all,
        &pub_key_package,
        &old.key_pkg_alice,
        &old.key_pkg_bob,
        10_500,
        &state_a,
        &state_b,
    )
    .await;

    let view_a = reset_view(
        &state_a,
        alice.owner,
        now_ms.load(Ordering::SeqCst),
        proposal_id,
    )
    .await
    .expect("proposal present on a");
    let view_b = reset_view(
        &state_b,
        alice.owner,
        now_ms.load(Ordering::SeqCst),
        proposal_id,
    )
    .await
    .expect("proposal present on b");
    assert_eq!(
        view_a.phase,
        ResetPhase::Vetoed,
        "both nodes converge on Vetoed"
    );
    assert_eq!(
        view_b.phase,
        ResetPhase::Vetoed,
        "both nodes converge on Vetoed"
    );

    // Even advancing far past the disaster deadline, a terminal veto never
    // re-litigates into Authorized (fix-round-2 M1 regression guard).
    now_ms.store(
        10_100 + RESET_VETO_WINDOW_FLOOR_MS + RESET_FINALITY_MS + 1,
        Ordering::SeqCst,
    );
    let view_a_later = reset_view(
        &state_a,
        alice.owner,
        now_ms.load(Ordering::SeqCst),
        proposal_id,
    )
    .await
    .expect("proposal present on a");
    assert_eq!(view_a_later.phase, ResetPhase::Vetoed);

    {
        let la = node_a.log.lock().await;
        let lb = node_b.log.lock().await;
        assert!(la.committee_state.active, "committee stays active on veto");
        assert!(lb.committee_state.active);
        assert_eq!(
            la.committee_state.joint_verifying_key,
            Some(old.joint_vk),
            "old vk intact"
        );
        assert_eq!(lb.committee_state.joint_verifying_key, Some(old.joint_vk));
        assert!(
            la.committee_state.vk_history.is_empty(),
            "no marker ever authored"
        );
        assert!(lb.committee_state.vk_history.is_empty());
    }

    // Give the auto-drive orchestrator a few ticks to prove it does NOT
    // author a marker for a Vetoed proposal (negative control on the
    // no-marker assertion above, not just an absence observed too early).
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        node_a.log.lock().await.committee_state.active,
        "still active after settle window"
    );
    assert!(
        node_b.log.lock().await.committee_state.active,
        "still active after settle window"
    );

    let _ = &old.identifier_map;
    let _ = &old.verifying_shares;
}

// ─── Flow 3: cooperative (endorse, immediate) ──────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flow3_cooperative_endorse() {
    let alice = person(0xA7);
    let bob = person(0xB8);
    let carol = person(0xC9);

    let state_a = Arc::new(Mutex::new(CommunityState::new(SPACE_ID)));
    let state_b = Arc::new(Mutex::new(CommunityState::new(SPACE_ID)));
    seed_genesis(&state_a, &state_b, &alice, &bob, &carol).await;

    let node_a = orchestrated_node(
        "node-a",
        alice.owner,
        &alice.signing_key,
        &[&alice, &bob, &carol],
    );
    let node_b = orchestrated_node(
        "node-b",
        bob.owner,
        &bob.signing_key,
        &[&alice, &bob, &carol],
    );
    let old = seed_old_committee(
        &mut node_a.log.try_lock().unwrap(),
        &mut node_b.log.try_lock().unwrap(),
        &alice,
        &bob,
        &carol,
    );

    let now_ms = Arc::new(AtomicU64::new(10_000));
    let members_all = {
        let mut m = vec![alice.owner, bob.owner, carol.owner];
        m.sort();
        m
    };
    let resolver_a: Arc<dyn MembershipSnapshotResolver> = Arc::new(LiveMembershipResolver {
        state: state_a.clone(),
        admin: alice.owner,
        now_ms: now_ms.clone(),
        members_for_snapshot: members_all.clone(),
    });
    let resolver_b: Arc<dyn MembershipSnapshotResolver> = Arc::new(LiveMembershipResolver {
        state: state_b.clone(),
        admin: alice.owner,
        now_ms: now_ms.clone(),
        members_for_snapshot: members_all.clone(),
    });

    let (a_pub, a_sub, b_pub, b_sub) = make_bridge();
    let engine_a = start_orchestrated(
        &node_a,
        alice.owner,
        alice.x25519_priv,
        &[&alice, &bob, &carol],
        resolver_a,
        a_pub,
        a_sub,
    )
    .await;
    let engine_b = start_orchestrated(
        &node_b,
        bob.owner,
        bob.x25519_priv,
        &[&alice, &bob, &carol],
        resolver_b,
        b_pub,
        b_sub,
    )
    .await;

    let target_epoch = 1u64;
    let proposal = mint(
        MembershipEventKind::DfrostResetProposal {
            target_vk: old.joint_vk,
            target_epoch,
            new_members: sorted_pair(alice.owner, bob.owner),
            new_threshold: 2,
            veto_window_ms: RESET_VETO_WINDOW_FLOOR_MS,
        },
        alice.owner,
        hlc(10_000, "alice"),
        &alice.signing_key,
    );
    let proposal_id = proposal.id;
    insert_both(&state_a, &state_b, proposal).await;
    let cosign = mint(
        MembershipEventKind::DfrostResetCosign {
            target_event_id: proposal_id,
        },
        bob.owner,
        hlc(10_100, "bob"),
        &bob.signing_key,
    );
    insert_both(&state_a, &state_b, cosign).await;

    let pub_key_package = {
        let g = node_a.log.lock().await;
        PublicKeyPackage::new(
            {
                let mut m = BTreeMap::new();
                for (addr, bytes) in &g.committee_state.verifying_shares {
                    let id = g.committee_state.identifier_map.get(addr).unwrap();
                    m.insert(
                        *id,
                        frost::keys::VerifyingShare::deserialize(bytes).expect("verifying share"),
                    );
                }
                m
            },
            frost::VerifyingKey::deserialize(&old.joint_vk).expect("verifying key"),
            None,
        )
    };

    // ── Endorse ceremony — no window wait: authorizes immediately ──
    drive_reset_response(
        proposal_id,
        ResetVerdict::Endorse,
        None,
        &alice,
        &bob,
        &node_a.log,
        &node_b.log,
        &engine_a,
        &engine_b,
        &members_all,
        &pub_key_package,
        &old.key_pkg_alice,
        &old.key_pkg_bob,
        10_500,
        &state_a,
        &state_b,
    )
    .await;

    // now_ms is still 10_000-ish (no advance at all) — cooperative
    // authorization is a function of the events' own wall_ms, not read time.
    let view = reset_view(
        &state_a,
        alice.owner,
        now_ms.load(Ordering::SeqCst),
        proposal_id,
    )
    .await
    .expect("proposal present");
    assert_eq!(
        view.phase,
        ResetPhase::Authorized,
        "endorse authorizes immediately, no window wait"
    );

    // ── From here on, identical to flow 1: marker auto-applies, drive successor DKG, Consumed auto-drives round-1, manual round-2 completes it ──
    poll_until(
        "both nodes deactivate the old committee via auto-authored marker",
        Duration::from_secs(10),
        || async {
            let a = !node_a.log.lock().await.committee_state.active;
            let b = !node_b.log.lock().await.committee_state.active;
            a && b
        },
    )
    .await
    .expect("marker auto-drive");

    let ceremony_hex = dfrost_initiate_dkg_core::<MockRt, MockRt>(
        &node_a.handles,
        None,
        SPACE_ID,
        vec![alice.owner, bob.owner],
        2,
    )
    .await
    .expect("alice initiates successor DKG");
    assert_eq!(ceremony_hex.len(), 64);

    poll_until(
        "both nodes converge to the new active committee",
        Duration::from_secs(20),
        || async {
            let a = node_a.log.lock().await.committee_state.active;
            let b = node_b.log.lock().await.committee_state.active;
            a && b
        },
    )
    .await
    .expect("successor DKG convergence");

    let (new_vk, new_pub_key_package, new_key_pkg_alice, new_key_pkg_bob) = {
        let la = node_a.log.lock().await;
        let lb = node_b.log.lock().await;
        assert_eq!(
            la.committee_state.joint_verifying_key,
            lb.committee_state.joint_verifying_key
        );
        let vk = la
            .committee_state
            .joint_verifying_key
            .expect("new vk present");
        let pkp = PublicKeyPackage::new(
            {
                let mut m = BTreeMap::new();
                for (addr, bytes) in &la.committee_state.verifying_shares {
                    let id = la.committee_state.identifier_map.get(addr).unwrap();
                    m.insert(
                        *id,
                        frost::keys::VerifyingShare::deserialize(bytes).expect("verifying share"),
                    );
                }
                m
            },
            frost::VerifyingKey::deserialize(&vk).expect("verifying key"),
            None,
        );
        (
            vk,
            pkp,
            la.local_key_package
                .clone()
                .expect("alice's new key package"),
            lb.local_key_package.clone().expect("bob's new key package"),
        )
    };

    poll_until(
        "both nodes auto-initiate the Consumed ceremony round-1",
        Duration::from_secs(10),
        || async {
            let a = node_a
                .log
                .lock()
                .await
                .committee_state
                .pending_sign
                .values()
                .any(|p| {
                    p.purpose
                        == harmony_app::community_dfrost_log::SignPurpose::ResetResponse {
                            proposal_id,
                            verdict: ResetVerdict::Consumed,
                            new_vk: Some(new_vk),
                        }
                });
            let b = node_b
                .log
                .lock()
                .await
                .committee_state
                .pending_sign
                .values()
                .any(|p| {
                    p.purpose
                        == harmony_app::community_dfrost_log::SignPurpose::ResetResponse {
                            proposal_id,
                            verdict: ResetVerdict::Consumed,
                            new_vk: Some(new_vk),
                        }
                });
            a && b
        },
    )
    .await
    .expect("Consumed ceremony auto-drive round-1");

    let new_members = {
        let mut m = vec![alice.owner, bob.owner];
        m.sort();
        m
    };

    poll_until(
        "both sides see 2 round-1 contributions on the Consumed ceremony",
        Duration::from_secs(10),
        || async {
            let a2 = node_a
                .log
                .lock()
                .await
                .committee_state
                .pending_sign
                .values()
                .any(|p| {
                    p.purpose
                        == harmony_app::community_dfrost_log::SignPurpose::ResetResponse {
                            proposal_id,
                            verdict: ResetVerdict::Consumed,
                            new_vk: Some(new_vk),
                        }
                        && p.contributions.len() >= 2
                });
            let b2 = node_b
                .log
                .lock()
                .await
                .committee_state
                .pending_sign
                .values()
                .any(|p| {
                    p.purpose
                        == harmony_app::community_dfrost_log::SignPurpose::ResetResponse {
                            proposal_id,
                            verdict: ResetVerdict::Consumed,
                            new_vk: Some(new_vk),
                        }
                        && p.contributions.len() >= 2
                });
            a2 && b2
        },
    )
    .await
    .expect("Consumed round-1 convergence");

    let consumed_ceremony_id = {
        let g = node_a.log.lock().await;
        *g.committee_state
            .pending_sign
            .iter()
            .find(|(_, p)| {
                p.purpose
                    == harmony_app::community_dfrost_log::SignPurpose::ResetResponse {
                        proposal_id,
                        verdict: ResetVerdict::Consumed,
                        new_vk: Some(new_vk),
                    }
            })
            .map(|(id, _)| id)
            .expect("consumed ceremony present")
    };

    let (event_a, _) = {
        let mut g = node_a.log.lock().await;
        contribute_round2(
            &mut g,
            alice.owner,
            consumed_ceremony_id,
            &new_members,
            &new_key_pkg_alice,
            hlc(20_000, "alice"),
            &alice.signing_key,
        )
    };
    node_a
        .log
        .lock()
        .await
        .apply_with_identity(event_a.clone(), &alice.owner, &alice.x25519_priv)
        .expect("alice applies own consumed round-2");
    engine_a
        .publish_event(event_a)
        .await
        .expect("alice broadcasts consumed round-2");

    poll_until(
        "bob sees alice's consumed round-2 share",
        Duration::from_secs(10),
        || async {
            node_b
                .log
                .lock()
                .await
                .committee_state
                .pending_sign
                .get(&consumed_ceremony_id)
                .map(|p| {
                    !p.contributions
                        .get(&alice.owner)
                        .map(|(_, s)| s.is_empty())
                        .unwrap_or(true)
                })
                .unwrap_or(false)
        },
    )
    .await
    .expect("alice's consumed round-2 converges to bob");

    let (event_b, _) = {
        let mut g = node_b.log.lock().await;
        contribute_round2(
            &mut g,
            bob.owner,
            consumed_ceremony_id,
            &new_members,
            &new_key_pkg_bob,
            hlc(20_100, "bob"),
            &bob.signing_key,
        )
    };
    node_b
        .log
        .lock()
        .await
        .apply_with_identity(event_b.clone(), &bob.owner, &bob.x25519_priv)
        .expect("bob applies own consumed round-2");
    engine_b
        .publish_event(event_b)
        .await
        .expect("bob broadcasts consumed round-2");

    poll_until(
        "alice sees bob's consumed round-2 share",
        Duration::from_secs(10),
        || async {
            node_a
                .log
                .lock()
                .await
                .committee_state
                .pending_sign
                .get(&consumed_ceremony_id)
                .map(|p| {
                    !p.contributions
                        .get(&bob.owner)
                        .map(|(_, s)| s.is_empty())
                        .unwrap_or(true)
                })
                .unwrap_or(false)
        },
    )
    .await
    .expect("bob's consumed round-2 converges to alice");

    maybe_complete_reset_response(
        &node_b.log,
        &new_members,
        &new_pub_key_package,
        consumed_ceremony_id,
        proposal_id,
        ResetVerdict::Consumed,
        Some(new_vk),
        bob.owner,
        &bob.signing_key,
        hlc(20_200, "bob"),
        &state_a,
        &state_b,
    )
    .await
    .expect("Consumed threshold reached — aggregate must produce a signature");
    node_a
        .log
        .lock()
        .await
        .committee_state
        .pending_sign
        .remove(&consumed_ceremony_id);

    let final_view = reset_view(&state_a, alice.owner, 20_500, proposal_id)
        .await
        .expect("proposal present");
    assert_eq!(final_view.phase, ResetPhase::Consumed);
    let final_view_b = reset_view(&state_b, alice.owner, 20_500, proposal_id)
        .await
        .expect("proposal present on b");
    assert_eq!(final_view_b.phase, ResetPhase::Consumed);
}

// ─── Flow 4: joiner bootstrap post-reset (dfrost catch-up adoption) ────────
//
// Asserts only the positive per the brief: a fresh node's DfrostLogEngine,
// with no prior committee state, adopts the ACTIVE successor committee's
// vk/epoch via the ZEB-1030 evidence-based catch-up path
// (`catchup_build_request` → `catchup_respond` → `catchup_ingest`, the
// exact proven pattern from
// `community_dfrost_integration.rs::fresh_joiner_adopts_committee_state_zeb1030`).
// The negative (stale-quorum rejection) is covered at log level by Task 5's
// tests, per the brief — not re-asserted here. This exercises the DFROST
// layer specifically (the genuinely new-ground half of joiner bootstrap);
// the membership-side join/redeem flow itself is proven separately by
// `community_open_flow_integration.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flow4_joiner_bootstrap_post_reset() {
    let alice = person(0xAA);
    let bob = person(0xBB);
    let carol = person(0xCC);
    let dave = person(0xDD);

    let state_a = Arc::new(Mutex::new(CommunityState::new(SPACE_ID)));
    let state_b = Arc::new(Mutex::new(CommunityState::new(SPACE_ID)));
    seed_genesis(&state_a, &state_b, &alice, &bob, &carol).await;

    let node_a = orchestrated_node(
        "node-a",
        alice.owner,
        &alice.signing_key,
        &[&alice, &bob, &carol],
    );
    let node_b = orchestrated_node(
        "node-b",
        bob.owner,
        &bob.signing_key,
        &[&alice, &bob, &carol],
    );
    let old = seed_old_committee(
        &mut node_a.log.try_lock().unwrap(),
        &mut node_b.log.try_lock().unwrap(),
        &alice,
        &bob,
        &carol,
    );

    let now_ms = Arc::new(AtomicU64::new(10_000));
    let members_all = {
        let mut m = vec![alice.owner, bob.owner, carol.owner];
        m.sort();
        m
    };
    let resolver_a: Arc<dyn MembershipSnapshotResolver> = Arc::new(LiveMembershipResolver {
        state: state_a.clone(),
        admin: alice.owner,
        now_ms: now_ms.clone(),
        members_for_snapshot: members_all.clone(),
    });
    let resolver_b: Arc<dyn MembershipSnapshotResolver> = Arc::new(LiveMembershipResolver {
        state: state_b.clone(),
        admin: alice.owner,
        now_ms: now_ms.clone(),
        members_for_snapshot: members_all.clone(),
    });

    let (a_pub, a_sub, b_pub, b_sub) = make_bridge();
    let engine_a = start_orchestrated(
        &node_a,
        alice.owner,
        alice.x25519_priv,
        &[&alice, &bob, &carol],
        resolver_a,
        a_pub,
        a_sub,
    )
    .await;
    let engine_b = start_orchestrated(
        &node_b,
        bob.owner,
        bob.x25519_priv,
        &[&alice, &bob, &carol],
        resolver_b,
        b_pub,
        b_sub,
    )
    .await;

    let target_epoch = 1u64;
    let proposal = mint(
        MembershipEventKind::DfrostResetProposal {
            target_vk: old.joint_vk,
            target_epoch,
            new_members: sorted_pair(alice.owner, bob.owner),
            new_threshold: 2,
            veto_window_ms: RESET_VETO_WINDOW_FLOOR_MS,
        },
        alice.owner,
        hlc(10_000, "alice"),
        &alice.signing_key,
    );
    let proposal_id = proposal.id;
    insert_both(&state_a, &state_b, proposal).await;
    let cosign = mint(
        MembershipEventKind::DfrostResetCosign {
            target_event_id: proposal_id,
        },
        bob.owner,
        hlc(10_100, "bob"),
        &bob.signing_key,
    );
    insert_both(&state_a, &state_b, cosign).await;

    now_ms.store(
        10_100 + RESET_VETO_WINDOW_FLOOR_MS + RESET_FINALITY_MS + 1,
        Ordering::SeqCst,
    );
    poll_until(
        "both nodes deactivate the old committee via auto-authored marker",
        Duration::from_secs(10),
        || async {
            let a = !node_a.log.lock().await.committee_state.active;
            let b = !node_b.log.lock().await.committee_state.active;
            a && b
        },
    )
    .await
    .expect("marker auto-drive");

    dfrost_initiate_dkg_core::<MockRt, MockRt>(
        &node_a.handles,
        None,
        SPACE_ID,
        vec![alice.owner, bob.owner],
        2,
    )
    .await
    .expect("alice initiates successor DKG");
    poll_until(
        "both nodes converge to the new active committee",
        Duration::from_secs(20),
        || async {
            let a = node_a.log.lock().await.committee_state.active;
            let b = node_b.log.lock().await.committee_state.active;
            a && b
        },
    )
    .await
    .expect("successor DKG convergence");

    let new_vk = node_a
        .log
        .lock()
        .await
        .committee_state
        .joint_verifying_key
        .expect("new vk present");
    let new_epoch = node_a.log.lock().await.committee_state.current_epoch;

    // ── Dave: fresh DfrostLogEngine, empty log, catches up from alice ──
    let dave_log = Arc::new(Mutex::new(DfrostLog::new()));
    let (d_pub_tx, _d_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_d_sub_tx, d_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let dave_engine = DfrostLogEngine::<MockRt>::start(DfrostLogEngineParams {
        community_id: SPACE_ID,
        dfrost_log: dave_log.clone(),
        publisher_tx: d_pub_tx,
        subscriber_rx: d_sub_rx,
        app_handle: None,
        self_addr: dave.owner,
        self_x25519_priv: dave.x25519_priv,
        identity_resolver: resolver_of(&[&alice, &bob, &carol, &dave]),
        registry_weak: None,
        driver: None,
        membership_resolver: None,
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    let req = dave_engine.catchup_build_request().await;
    assert_eq!(req.epoch, 0);
    assert!(!req.active);
    let frames = engine_a
        .catchup_respond(req)
        .await
        .expect("alice serves the fresh joiner");
    let outcome = dave_engine.catchup_ingest(frames).await;
    match outcome {
        CatchupOutcome::AdoptedInitial { epoch, .. } => {
            assert_eq!(
                epoch, new_epoch,
                "joiner adopts the SUCCESSOR committee's epoch"
            );
        }
        other => panic!("expected AdoptedInitial at epoch {new_epoch}, got {other:?}"),
    }

    let dg = dave_log.lock().await;
    assert!(
        dg.committee_state.active,
        "joiner lands on an active committee"
    );
    assert_eq!(
        dg.committee_state.joint_verifying_key,
        Some(new_vk),
        "joiner lands on the NEW vk, not the retired one"
    );
    assert_eq!(dg.committee_state.current_epoch, new_epoch);

    drop(engine_b);
    drop(dave_engine);
}
