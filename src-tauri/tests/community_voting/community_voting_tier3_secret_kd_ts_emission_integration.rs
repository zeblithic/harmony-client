//! ZEB-295 Phase 6: load-bearing integration test for the engine-auto
//! `kd=ts` (TallyShare) emission added in this batch.
//!
//! The pre-existing `community_voting_tier3_secret_*_integration.rs` files
//! exercise the **apply path** for kd=ts (verify rules T1/T2, LWW upsert,
//! shape gating) by constructing payloads from a mock committee and
//! feeding them through `Tier3PollState::apply_event`. They do NOT drive
//! the **engine-side** auto-emission added in this batch.
//!
//! This test fills that gap. CodeAnt's PR #155 critical finding was that
//! `maybe_emit_tally_share` was a stub — gated correctly but with no
//! emission body. Without an engine-driven smoke test, regressions there
//! would silently re-stub the function and break ratification recovery
//! for every se-mode poll. This test:
//!
//! 1. Runs a real 3-party FROST-Ristretto255 DKG (2-of-3 threshold) so
//!    we have authentic `KeyPackage`s + `PublicKeyPackage` matching
//!    a single joint Y.
//! 2. Wires a `DfrostLogEngine` for the test community, seeding its
//!    `local_key_package` + `committee_state` so the voting engine's
//!    `maybe_emit_tally_share` can resolve `(x_i, Y_i)` via the
//!    `DfrostLogRegistry`.
//! 3. Wires a `VotingLogEngine`, installs the local signing key for
//!    committee member 0, and seeds the voting log with a se-mode
//!    Tier 3 poll past `kd=cl` (close_event_hash set) — bypassing
//!    `publish_event` since the apply chain to reach Ratification stage
//!    with valid signed events is out of scope for this load-bearing
//!    test.
//! 4. Drives the orchestration hook via the `test_invoke_*` test seam
//!    (the private hook is otherwise only reached from `publish_event`)
//!    and asserts that exactly one kd=ts envelope is broadcast on the
//!    publisher channel with the expected committee_epoch + entry count.
//!
//! Per the brief: "there should be a test that drives the engine through
//! the full cycle and asserts kd=ts events are actually emitted to the log."

#![cfg(feature = "test-fixtures")]

use std::collections::BTreeMap;
use std::sync::Arc;

use frost_ristretto255::keys::dkg;
use frost_ristretto255::keys::{KeyPackage, PublicKeyPackage};
use frost_ristretto255::Identifier;
use tokio::sync::{mpsc, Mutex};

use harmony_app::community_dfrost_crypto::{
    dkg_part1_local, dkg_part2_local, dkg_part3_local, identifier_for_index,
};
use harmony_app::community_dfrost_log::{CommitteeState, DfrostLog};
use harmony_app::community_dfrost_log_engine::{DfrostLogEngineParams, DfrostLogRegistry};
use harmony_app::community_state_sync::IdentityResolver;
use harmony_app::community_voting_core::{
    BallotNIZKProof, Eligibility, PollEventKindCode, PollId, RatificationBallotPayload,
    SignedVotingEvent, TallySharePayload, Tier, Tier3PollConfigPayload,
};
use harmony_app::community_voting_log::VotingLog;
use harmony_app::community_voting_log_engine::{
    DfrostLogCommitteeOracle, VotingLogEngine, VotingLogEngineParams,
};
use harmony_app::community_voting_tier3::{DraftCandidateState, Tier3PollMeta, Tier3PollState};
use harmony_app::community_voting_tier3_nizk::prove_ballot_bundle_with_outputs;
use harmony_app::dm_outbox::reserve_next_hlc_for_device;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

// ─── Resolver shim ────────────────────────────────────────────────────────────

struct NoopResolver;
#[async_trait::async_trait]
impl IdentityResolver for NoopResolver {
    async fn resolve(&self, _addr: &OwnerAddr) -> Option<[u8; 64]> {
        None
    }
}

// ─── DKG ceremony helpers (3 parties, 2-of-3) ────────────────────────────────

/// Run a complete 2-of-3 FROST-Ristretto255 DKG and return per-participant
/// `(Identifier, KeyPackage, PublicKeyPackage)` triples. Mirrors the
/// private test fixture in `community_dfrost_crypto.rs::tests` (which is
/// `#[cfg(test)]` and thus unreachable from integration test binaries).
fn run_2_of_3_dkg() -> Vec<(Identifier, KeyPackage, PublicKeyPackage)> {
    let ids: Vec<Identifier> = (0..3).map(identifier_for_index).collect();

    // Round 1.
    let mut r1_secrets: BTreeMap<Identifier, dkg::round1::SecretPackage> = BTreeMap::new();
    let mut r1_pkgs: BTreeMap<Identifier, Vec<u8>> = BTreeMap::new();
    for id in &ids {
        let (sec, pkg) = dkg_part1_local(*id, 3, 2).expect("part1");
        r1_secrets.insert(*id, sec);
        r1_pkgs.insert(*id, pkg);
    }

    // Round 2.
    let mut r2_secrets: BTreeMap<Identifier, dkg::round2::SecretPackage> = BTreeMap::new();
    let mut r2_outbound: BTreeMap<Identifier, BTreeMap<Identifier, Vec<u8>>> = BTreeMap::new();
    for id in &ids {
        let sec = r1_secrets.remove(id).expect("r1 secret");
        let received_r1: BTreeMap<Identifier, Vec<u8>> = r1_pkgs
            .iter()
            .filter(|(other, _)| *other != id)
            .map(|(other, bytes)| (*other, bytes.clone()))
            .collect();
        let (r2_sec, r2_out) = dkg_part2_local(sec, &received_r1).expect("part2");
        r2_secrets.insert(*id, r2_sec);
        r2_outbound.insert(*id, r2_out);
    }

    // Round 3.
    let mut out = Vec::new();
    for id in &ids {
        let received_r1: BTreeMap<Identifier, Vec<u8>> = r1_pkgs
            .iter()
            .filter(|(other, _)| *other != id)
            .map(|(other, bytes)| (*other, bytes.clone()))
            .collect();
        let mut received_r2: BTreeMap<Identifier, Vec<u8>> = BTreeMap::new();
        for (sender, out_map) in &r2_outbound {
            if sender == id {
                continue;
            }
            received_r2.insert(*sender, out_map.get(id).expect("r2 pkg").clone());
        }
        let r2_sec = r2_secrets.get(id).expect("r2 secret");
        let (kp, pkp) = dkg_part3_local(r2_sec, &received_r1, &received_r2).expect("part3");
        out.push((*id, kp, pkp));
    }
    out
}

// ─── End-to-end test ──────────────────────────────────────────────────────────

#[tokio::test]
async fn engine_orchestration_emits_kd_ts_after_kd_cl_se_mode() {
    use harmony_app::community_dfrost_crypto::{
        joint_verifying_key_to_point, verifying_key_to_bytes, verifying_share_to_bytes,
    };

    // (1) Run a real 3-party DKG so we have authentic Y / Y_i / x_i.
    let parties = run_2_of_3_dkg();
    assert_eq!(parties.len(), 3);

    // Member addrs are 0x10..0x12 so OwnerAddr sort order matches FROST
    // identifier order 1..3. The `build_identifier_map` rebuild after
    // serde round-trip depends on this canonical bytewise ordering.
    let member_addrs: Vec<OwnerAddr> = (0u8..3).map(|i| OwnerAddr([0x10 + i; 16])).collect();
    let pkp = parties[0].2.clone();
    let joint_verifying_key = verifying_key_to_bytes(pkp.verifying_key());
    let mut verifying_shares: BTreeMap<OwnerAddr, [u8; 32]> = BTreeMap::new();
    for (i, addr) in member_addrs.iter().enumerate() {
        let id = identifier_for_index(i);
        let vs = pkp.verifying_shares().get(&id).expect("vs in pkp");
        verifying_shares.insert(*addr, verifying_share_to_bytes(vs));
    }

    // (2) Build a DfrostLog with member 0's KeyPackage materialized + an
    // active committee_state.
    let community_id = SpaceId([0xC5; 16]);
    let local_kp = parties[0].1.clone();
    let dfrost_log = {
        let mut log = DfrostLog::new();
        log.local_key_package = Some(local_kp.clone());
        log.local_pub_key_package = Some(pkp.clone());
        let identifier_map =
            harmony_app::community_dfrost_log::CommitteeState::build_identifier_map(&member_addrs);
        log.committee_state = CommitteeState {
            active: true,
            current_epoch: 0,
            joint_verifying_key: Some(joint_verifying_key),
            verifying_shares: verifying_shares.clone(),
            members: member_addrs.clone(),
            threshold: 2,
            max_signers: 3,
            identifier_map,
            pending_dkg: None,
            pending_sign: BTreeMap::new(),
            pending_refresh: None,
        };
        Arc::new(Mutex::new(log))
    };

    // Wrap in DfrostLogEngine + register.
    let dfrost_reg = Arc::new(DfrostLogRegistry::<tauri::test::MockRuntime>::new());
    {
        let (dtx, _drx) = mpsc::channel::<Vec<u8>>(4);
        let (_dstx, dsrx) = mpsc::channel::<Vec<u8>>(4);
        let app = tauri::test::mock_app();
        let app_handle = app.handle().clone();
        DfrostLogRegistry::register(
            &dfrost_reg,
            DfrostLogEngineParams {
                community_id,
                dfrost_log,
                publisher_tx: dtx,
                subscriber_rx: dsrx,
                app_handle: Some(app_handle),
                self_addr: member_addrs[0],
                self_x25519_priv: [0u8; 32],
                identity_resolver: Arc::new(NoopResolver),
                registry_weak: None,
            },
        )
        .await;
    }

    // (3) Voting engine with full local-signing wiring.
    let voting_log = Arc::new(Mutex::new(VotingLog::new()));
    let (publisher_tx, mut publisher_rx) = mpsc::channel::<Vec<u8>>(32);
    let (_sub_tx, subscriber_rx) = mpsc::channel::<Vec<u8>>(32);

    let device_id = "dev-test-kd-ts".to_string();
    let hlc_tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        device_id.clone(),
    )));

    let engine = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id,
        voting_log: Arc::clone(&voting_log),
        publisher_tx,
        subscriber_rx,
        hlc_tracker: Some(hlc_tracker.clone()),
        device_id: Some(device_id.clone()),
        app_handle: None,
        identity_resolver: None,
        membership_resolver: None,
    })
    .await;

    // Install the dfrost handle so `dfrost_registry` is wired for the
    // hook's `dr.as_ref()?.get(community_id)` lookup. The beacon
    // requester is a no-op for this test (we never PollCreate, so
    // `maybe_trigger_beacon_for_tier3_create` won't fire).
    use harmony_app::community_voting_log_engine::BeaconRequester;
    let requester: BeaconRequester =
        Arc::new(move |_cid, _seed, _epoch| Box::pin(async { Ok("noop".to_string()) }));
    VotingLogEngine::install_dfrost_handle(&engine, dfrost_reg.clone(), requester).await;

    // Install member 0's local signing key (the signing identity is
    // unrelated to FROST shares; we use a fresh Ed25519 keypair for the
    // event signature surface — that's what voting events carry).
    let signing_key = Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]));
    let self_owner = member_addrs[0];
    engine
        .install_local_signing_key(Arc::clone(&signing_key), self_owner)
        .await;

    // (4) Seed the voting log with a se-mode Tier 3 poll in
    // post-ratification state with `close_event_hash` set. We
    // bypass `publish_event` because getting through the full chain
    // (PollCreate → SortitionSelection → DraftCandidate ×n → DraftApproval
    //  → RatificationBallot ×n → PollClose) with valid signed events
    // is out of scope — this test isolates the kd=ts emission body.
    let poll_id = PollId([0x77; 32]);
    let config = Tier3PollConfigPayload {
        proposal_text: "kd=ts emission smoke".into(),
        // Drafting threshold = ceil(sortition_size / 2). Keep small so 3
        // synthetic approvers per candidate (the test's full committee)
        // exceeds the threshold and drafting_advancers returns both
        // candidates → n=3 (2 candidates + status_quo).
        sortition_size: 20,
        deliberation_window_seconds: 10,
        drafting_window_seconds: 10,
        ratification_window_seconds: 10,
        privacy_mode: "se".into(),
        incentive_mode: "d".into(),
        eligibility: Eligibility {
            min_power: 1,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
    };
    // Build a poll state with 2 candidates (n=3 with status_quo).
    let meta = Tier3PollMeta {
        poll_id,
        proposer: self_owner,
        poll_create_hlc: Hlc {
            wall_ms: 0,
            logical: 0,
            device_id: device_id.clone(),
        },
        config: config.clone(),
        poll_create_event_hash: [0xaa; 32],
        community_epoch: 0,
    };
    let electorate: Vec<OwnerAddr> = member_addrs.clone();
    let mut t3 = Tier3PollState::new_from_create(meta.clone(), electorate);
    // Install the production CommitteeOracle (DfrostLogCommitteeOracle)
    // sourced from the dfrost engine — same path the real apply-time
    // install uses.
    let oracle = {
        let engine_arc = dfrost_reg.get(community_id).await.expect("dfrost engine");
        DfrostLogCommitteeOracle::from_dfrost_engine(engine_arc.as_ref())
            .await
            .expect("oracle from dfrost engine")
    };
    t3.install_committee_oracle(Arc::new(oracle));

    // Add 2 synthetic candidates with ≥ ceil(sortition_size/2) = 10
    // synthetic approvers each so drafting_advancers admits them and
    // ratification_candidates_ordering returns n=3 (2 candidates +
    // status_quo). The approver OwnerAddrs don't need to be in the
    // electorate — drafting_advancers only counts set cardinality of
    // `approvals` against the threshold.
    let approvals: std::collections::HashSet<OwnerAddr> =
        (0u8..10).map(|i| OwnerAddr([0xA0 | i; 16])).collect();
    for i in 0..2u8 {
        t3.candidates.push(DraftCandidateState {
            event_hash: [0xC0 | i; 32],
            text: format!("candidate {i}"),
            proposer: Some(self_owner),
            approvals: approvals.clone(),
        });
    }
    // Synthesize ratification ballots so `aggregate_se_ballots` returns
    // Some — each voter votes [5, 1, 0] (deterministic scores).
    let joint_y_point = joint_verifying_key_to_point(pkp.verifying_key());
    let mut ratification_ballots: Vec<RatificationBallotPayload> = Vec::new();
    for _ in 0..3 {
        let (bundle, cs, ci) = prove_ballot_bundle_with_outputs(&joint_y_point, &[5u64, 1, 0])
            .expect("test scores are well-formed");
        ratification_ballots.push(RatificationBallotPayload {
            poll_id,
            scores: None,
            ciphertexts_scores: Some(cs),
            ciphertexts_indicators: Some(ci),
            proof: Some(BallotNIZKProof {
                range_proofs: bundle.range_proofs,
                consistency_proofs: bundle.consistency_proofs,
            }),
        });
    }
    t3.ratification_ballots = ratification_ballots;
    t3.close_event_hash = Some([0xCC; 32]);

    // Insert the synthesized PollState into the engine's voting log.
    use harmony_app::community_voting_core::{Lifecycle, PollMeta};
    use harmony_app::community_voting_log::{PollState, TierState};
    let poll_state = PollState {
        meta: PollMeta {
            poll_id,
            community_id,
            creator: self_owner,
            tier: Tier::Sortition,
            eligibility: meta.config.eligibility,
            lifecycle: Lifecycle::Open,
            created_at: meta.poll_create_hlc.clone(),
            opens_at: meta.poll_create_hlc.clone(),
            closes_at: Hlc {
                wall_ms: meta.poll_create_hlc.wall_ms + 30_000,
                logical: 0,
                device_id: meta.poll_create_hlc.device_id.clone(),
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
    {
        let mut log = voting_log.lock().await;
        log.polls.insert(poll_id, poll_state);
    }

    // Reserve at least one HLC on the device's lane so subsequent
    // `reserve_next_local_hlc` calls inside the hook don't collide
    // with wall_now_ms=0.
    let _ = reserve_next_hlc_for_device(
        &hlc_tracker,
        &harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        &device_id,
        1_000_000,
    )
    .await;

    // (5) Drive the kd=ts emission hook directly via the test seam.
    engine
        .test_invoke_maybe_emit_tally_share(&poll_id, &signing_key, self_owner)
        .await;

    // (6) Assert a kd=ts envelope was broadcast on publisher_tx.
    // The hook publishes via `publish_event` which records-then-broadcasts
    // synchronously (no spawn); the packet should be on the channel
    // immediately. Use a generous timeout to absorb scheduler jitter
    // on busy CI.
    let packet = tokio::time::timeout(std::time::Duration::from_secs(2), publisher_rx.recv())
        .await
        .expect("kd=ts broadcast within timeout")
        .expect("publisher channel open");
    let decoded: SignedVotingEvent =
        ciborium::from_reader(&packet[..]).expect("decode broadcast packet");
    assert_eq!(decoded.tier, Tier::Sortition);
    assert_eq!(decoded.kind, PollEventKindCode::TallyShare);
    assert_eq!(decoded.actor, self_owner);
    let payload: TallySharePayload =
        ciborium::from_reader(&decoded.payload[..]).expect("decode TallySharePayload");
    assert_eq!(payload.poll_id, poll_id);
    assert_eq!(payload.committee_epoch, 0);
    // n=3 (2 candidates + status_quo); n + C(n,2) = 3 + 3 = 6 entries.
    assert_eq!(payload.entries.len(), 6);

    // (7) Soundness: the emitted kd=ts must round-trip through the
    // apply path. `publish_event` already self-applies before broadcast
    // (the self-loopback fix from ZEB-270), so by this point the kd=ts
    // is in the projection — assert the state matches what the apply
    // path would have produced from the broadcast.
    {
        let log = voting_log.lock().await;
        let t3 = log
            .polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .expect("t3 present");
        assert_eq!(
            t3.secret_tally.tally_shares.len(),
            1,
            "engine-auto kd=ts must self-apply via publish_event"
        );
        let record = t3
            .secret_tally
            .tally_shares
            .get(&(self_owner, 0u64))
            .expect("record at (owner, epoch=0)");
        assert_eq!(record.entries.len(), 6);
        // Bit-exactness check: the projection's TallyShareEntry vec
        // must equal the broadcast envelope's entries.
        assert_eq!(
            record.entries, payload.entries,
            "self-applied entries must match broadcast envelope's entries"
        );
    }

    // (8) Idempotence: invoking the hook again must NOT emit a second
    // kd=ts (already-emitted gate).
    engine
        .test_invoke_maybe_emit_tally_share(&poll_id, &signing_key, self_owner)
        .await;
    let second =
        tokio::time::timeout(std::time::Duration::from_millis(200), publisher_rx.recv()).await;
    assert!(
        second.is_err(),
        "second invocation must NOT publish another kd=ts (already-emitted gate); \
         received {second:?}"
    );

    drop(engine);
}

/// Silent-bail when the local engine is NOT on the committee.
///
/// Without this gate, a non-committee node would try to derive `x_i` from
/// a missing local KeyPackage and panic (or worse, return zero shares
/// that pollute the tally with garbage DLEQ proofs). The hook must
/// short-circuit cleanly.
#[tokio::test]
async fn no_kd_ts_emission_when_not_committee_member() {
    use harmony_app::community_voting_log_engine::BeaconRequester;

    // Same DKG ceremony — but install signing key for an OwnerAddr that
    // is NOT in `member_addrs`. The committee oracle's `verifying_shares`
    // map won't contain the local owner → the early "not on committee"
    // gate trips before any FROST KeyPackage lookup.
    let parties = run_2_of_3_dkg();
    let member_addrs: Vec<OwnerAddr> = (0u8..3).map(|i| OwnerAddr([0x10 + i; 16])).collect();
    let pkp = parties[0].2.clone();
    use harmony_app::community_dfrost_crypto::{verifying_key_to_bytes, verifying_share_to_bytes};
    let joint_verifying_key = verifying_key_to_bytes(pkp.verifying_key());
    let mut verifying_shares: BTreeMap<OwnerAddr, [u8; 32]> = BTreeMap::new();
    for (i, addr) in member_addrs.iter().enumerate() {
        let id = identifier_for_index(i);
        let vs = pkp.verifying_shares().get(&id).expect("vs in pkp");
        verifying_shares.insert(*addr, verifying_share_to_bytes(vs));
    }

    let community_id = SpaceId([0xC6; 16]);
    let local_kp = parties[0].1.clone();
    let dfrost_log = {
        let mut log = DfrostLog::new();
        log.local_key_package = Some(local_kp);
        log.local_pub_key_package = Some(pkp.clone());
        let identifier_map =
            harmony_app::community_dfrost_log::CommitteeState::build_identifier_map(&member_addrs);
        log.committee_state = CommitteeState {
            active: true,
            current_epoch: 0,
            joint_verifying_key: Some(joint_verifying_key),
            verifying_shares: verifying_shares.clone(),
            members: member_addrs.clone(),
            threshold: 2,
            max_signers: 3,
            identifier_map,
            pending_dkg: None,
            pending_sign: BTreeMap::new(),
            pending_refresh: None,
        };
        Arc::new(Mutex::new(log))
    };
    let dfrost_reg = Arc::new(DfrostLogRegistry::<tauri::test::MockRuntime>::new());
    {
        let (dtx, _drx) = mpsc::channel::<Vec<u8>>(4);
        let (_dstx, dsrx) = mpsc::channel::<Vec<u8>>(4);
        let app = tauri::test::mock_app();
        let app_handle = app.handle().clone();
        DfrostLogRegistry::register(
            &dfrost_reg,
            DfrostLogEngineParams {
                community_id,
                dfrost_log,
                publisher_tx: dtx,
                subscriber_rx: dsrx,
                app_handle: Some(app_handle),
                self_addr: member_addrs[0],
                self_x25519_priv: [0u8; 32],
                identity_resolver: Arc::new(NoopResolver),
                registry_weak: None,
            },
        )
        .await;
    }

    let voting_log = Arc::new(Mutex::new(VotingLog::new()));
    let (publisher_tx, mut publisher_rx) = mpsc::channel::<Vec<u8>>(32);
    let (_sub_tx, subscriber_rx) = mpsc::channel::<Vec<u8>>(32);
    let device_id = "dev-non-committee".to_string();
    let hlc_tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        device_id.clone(),
    )));

    let engine = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id,
        voting_log: Arc::clone(&voting_log),
        publisher_tx,
        subscriber_rx,
        hlc_tracker: Some(hlc_tracker.clone()),
        device_id: Some(device_id.clone()),
        app_handle: None,
        identity_resolver: None,
        membership_resolver: None,
    })
    .await;
    let requester: BeaconRequester =
        Arc::new(move |_cid, _seed, _epoch| Box::pin(async { Ok("noop".to_string()) }));
    VotingLogEngine::install_dfrost_handle(&engine, dfrost_reg.clone(), requester).await;

    // Install a signing key for an OwnerAddr NOT in the committee.
    let non_committee_owner = OwnerAddr([0xEE; 16]);
    let signing_key = Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]));
    engine
        .install_local_signing_key(Arc::clone(&signing_key), non_committee_owner)
        .await;

    // Seed a se-mode poll with the committee oracle but with the
    // non-committee owner NOT in verifying_shares.
    let poll_id = PollId([0x88; 32]);
    let config = Tier3PollConfigPayload {
        proposal_text: "non-committee bail".into(),
        sortition_size: 20,
        deliberation_window_seconds: 10,
        drafting_window_seconds: 10,
        ratification_window_seconds: 10,
        privacy_mode: "se".into(),
        incentive_mode: "d".into(),
        eligibility: Eligibility {
            min_power: 1,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
    };
    let meta = Tier3PollMeta {
        poll_id,
        proposer: non_committee_owner,
        poll_create_hlc: Hlc {
            wall_ms: 0,
            logical: 0,
            device_id: device_id.clone(),
        },
        config,
        poll_create_event_hash: [0xaa; 32],
        community_epoch: 0,
    };
    let mut t3 = Tier3PollState::new_from_create(meta.clone(), member_addrs.clone());
    let oracle = DfrostLogCommitteeOracle::from_dfrost_engine(
        dfrost_reg
            .get(community_id)
            .await
            .expect("dfrost engine")
            .as_ref(),
    )
    .await
    .expect("oracle");
    t3.install_committee_oracle(Arc::new(oracle));
    t3.close_event_hash = Some([0xCC; 32]);

    use harmony_app::community_voting_core::{Lifecycle, PollMeta};
    use harmony_app::community_voting_log::{PollState, TierState};
    {
        let mut log = voting_log.lock().await;
        log.polls.insert(
            poll_id,
            PollState {
                meta: PollMeta {
                    poll_id,
                    community_id,
                    creator: non_committee_owner,
                    tier: Tier::Sortition,
                    eligibility: meta.config.eligibility,
                    lifecycle: Lifecycle::Open,
                    created_at: meta.poll_create_hlc.clone(),
                    opens_at: meta.poll_create_hlc.clone(),
                    closes_at: Hlc {
                        wall_ms: meta.poll_create_hlc.wall_ms + 30_000,
                        logical: 0,
                        device_id: meta.poll_create_hlc.device_id.clone(),
                    },
                    extends_at: None,
                    channel_id: None,
                    finalized_at_ms: None,
                },
                events: vec![],
                tier_state: TierState::Tier3(Box::new(t3)),
                tier1_cfg: None,
                tier1_snapshot: None,
            },
        );
    }
    let _ = reserve_next_hlc_for_device(
        &hlc_tracker,
        &harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        &device_id,
        1_000_000,
    )
    .await;

    engine
        .test_invoke_maybe_emit_tally_share(&poll_id, &signing_key, non_committee_owner)
        .await;

    let recv =
        tokio::time::timeout(std::time::Duration::from_millis(300), publisher_rx.recv()).await;
    assert!(
        recv.is_err(),
        "non-committee node must NOT emit kd=ts; got {recv:?}"
    );

    drop(engine);
}
