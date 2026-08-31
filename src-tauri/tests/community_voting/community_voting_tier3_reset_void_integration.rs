//! ZEB-1031 Task 7: Tier-3 poll voiding on committee reset + prompted
//! relaunch. See docs/superpowers/specs/2026-08-30-zeb1031-dfrost-committee-reset-design.md §7.
//!
//! Covers the pure voting-engine-level surface that doesn't need a real
//! dfrost committee/apply path (which lives in
//! `community_dfrost_integration.rs::{live_ingest_reset_marker_voids_open_tier3_polls_zeb1031,
//! chain_adoption_reset_marker_voids_open_tier3_polls_zeb1031}`):
//! - `VotingLogEngine::relaunch_voided_poll` produces a new poll carrying
//!   `predecessor`, at the current epoch, leaving the old poll voided.
//! - Authorization: original creator or power-100 only.
//! - `voided` survives the `community_voting_persist.rs` round-trip.

#![cfg(feature = "test-fixtures")]

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};

use harmony_app::community_dfrost_log::{CommitteeState, DfrostLog};
use harmony_app::community_dfrost_log_engine::{DfrostLogEngineParams, DfrostLogRegistry};
use harmony_app::community_membership::EventId;
use harmony_app::community_voting_core::{
    derive_poll_id, Eligibility, Lifecycle, PollEventKindCode, PollId, PollMeta, SignedVotingEvent,
    Tier, Tier3PollConfigPayload,
};
use harmony_app::community_voting_log::{PollState, TierState, VotingLog};
use harmony_app::community_voting_log_engine::{
    BeaconRequester, VotingLogEngine, VotingLogEngineParams,
};
use harmony_app::community_voting_tier3::{ApplyError, Tier3PollMeta, Tier3PollState, VoidedInfo};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

fn hlc(wall_ms: u64, device: &str) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: device.to_string(),
    }
}

fn se_config() -> Tier3PollConfigPayload {
    Tier3PollConfigPayload {
        proposal_text: "ZEB-1031 Task 7 relaunch smoke".into(),
        sortition_size: 20,
        // validate_tier3_poll_config enforces a 60s floor on every window —
        // relaunch runs the copied config back through that validator.
        deliberation_window_seconds: 60,
        drafting_window_seconds: 60,
        ratification_window_seconds: 60,
        privacy_mode: "se".into(),
        incentive_mode: "d".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: None,
    }
}

/// Seed a Tier-3 poll directly into `voting_log`, voided by `voided`
/// (`None` for a live poll), created by `creator` at `community_epoch`.
fn seed_poll(
    voting_log: &mut VotingLog,
    poll_id: PollId,
    creator: OwnerAddr,
    community_id: SpaceId,
    community_epoch: u64,
    voided: Option<VoidedInfo>,
) {
    let config = se_config();
    let create_hlc = hlc(0, "seed");
    let meta = Tier3PollMeta {
        poll_id,
        proposer: creator,
        poll_create_hlc: create_hlc.clone(),
        config: config.clone(),
        poll_create_event_hash: poll_id.0,
        community_epoch,
    };
    let mut t3 = Tier3PollState::new_from_create(meta, vec![creator]);
    t3.voided = voided;
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

/// Wire a `VotingLogEngine` with an active dfrost committee at `epoch`
/// installed (so `publish_event`'s Tier-3 PollCreate epoch pre-read
/// succeeds) and NO membership/identity resolvers (this file never drives
/// inbound peer events — only local `relaunch_voided_poll` calls, which
/// (like `voting_create_tier3_proposal`) rely on the IPC-layer eligibility
/// pre-check rather than an engine-internal one for the local-mint path).
async fn engine_with_active_committee(
    community_id: SpaceId,
    voting_log: Arc<Mutex<VotingLog>>,
    epoch: u64,
) -> Arc<VotingLogEngine<tauri::test::MockRuntime>> {
    let mut dfrost_log = DfrostLog::new();
    let members = vec![OwnerAddr([0x01; 16]), OwnerAddr([0x02; 16])];
    dfrost_log.committee_state = CommitteeState {
        active: true,
        current_epoch: epoch,
        joint_verifying_key: Some([0xEE; 32]),
        verifying_shares: BTreeMap::new(),
        members: members.clone(),
        threshold: 2,
        max_signers: 2,
        identifier_map: CommitteeState::build_identifier_map(&members),
        pending_dkg: None,
        pending_sign: BTreeMap::new(),
        pending_refresh: None,
        pending_repair: None,
        vk_history: Vec::new(),
        pending_reset: None,
    };
    let dfrost_reg = Arc::new(DfrostLogRegistry::<tauri::test::MockRuntime>::new());
    let (dpub_tx, _dpub_rx) = mpsc::channel::<Vec<u8>>(4);
    let (_dsub_tx, dsub_rx) = mpsc::channel::<Vec<u8>>(4);
    DfrostLogRegistry::register(
        &dfrost_reg,
        DfrostLogEngineParams {
            community_id,
            dfrost_log: Arc::new(Mutex::new(dfrost_log)),
            publisher_tx: dpub_tx,
            subscriber_rx: dsub_rx,
            app_handle: None,
            self_addr: members[0],
            self_x25519_priv: [0u8; 32],
            identity_resolver: Arc::new(NoopIdentityResolver),
            registry_weak: None,
            driver: None,
            membership_resolver: None,
            orchestrator_config: Default::default(),
            persist: None,
        },
    )
    .await;

    let (v_pub_tx, mut v_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    // Drain in the background for the test's life — dropping the receiver
    // here (as the underscore-prefixed local previously did) closes the
    // channel the instant this helper returns, which then makes every
    // subsequent `publish_event` (e.g. from `relaunch_voided_poll`) fail
    // with "channel closed".
    tokio::spawn(async move { while v_pub_rx.recv().await.is_some() {} });
    let (_v_sub_tx, v_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let device_id = "dev-zeb1031-t7-relaunch".to_string();
    let hlc_tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        device_id.clone(),
    )));
    let engine = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id,
        voting_log,
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
    VotingLogEngine::install_dfrost_handle(&engine, dfrost_reg, requester).await;
    engine
}

struct NoopIdentityResolver;
#[async_trait::async_trait]
impl harmony_app::community_state_sync::IdentityResolver for NoopIdentityResolver {
    async fn resolve(&self, _addr: &OwnerAddr) -> Option<[u8; 64]> {
        None
    }
}

/// ZEB-1031 Task 7 (spec §7): `relaunch_voided_poll` authors a fresh
/// Tier-3 PollCreate copying the voided poll's parameters, carrying
/// `predecessor: Some(old_id)`, stamped at the CURRENT epoch — and the
/// old poll stays voided (relaunch never un-voids it).
#[tokio::test]
async fn relaunch_produces_new_poll_with_predecessor_old_stays_voided() {
    let community_id = SpaceId([0x51; 16]);
    let creator = OwnerAddr([0xA0; 16]);
    let old_poll_id = PollId([0x61; 32]);
    let reset_id: EventId = [0x71; 16];

    let voting_log = Arc::new(Mutex::new(VotingLog::new()));
    {
        let mut log = voting_log.lock().await;
        seed_poll(
            &mut log,
            old_poll_id,
            creator,
            community_id,
            1,
            Some(VoidedInfo {
                reset_id,
                old_epoch: 1,
            }),
        );
    }

    // Committee now active at epoch 2 (post-reset successor).
    let engine = engine_with_active_committee(community_id, voting_log.clone(), 2).await;

    let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]);
    let new_poll_id = engine
        .relaunch_voided_poll(
            old_poll_id,
            creator,
            /* caller_is_power_100 */ false,
            &signing_key,
            hlc(10_000, "relauncher"),
            None,
        )
        .await
        .expect("relaunch by the original creator must succeed");

    assert_ne!(new_poll_id, old_poll_id);

    let log = voting_log.lock().await;
    let new_t3 = log.polls[&new_poll_id].tier_state.as_tier3().unwrap();
    assert_eq!(
        new_t3.meta.config.predecessor,
        Some(old_poll_id),
        "relaunched poll must carry predecessor = Some(old_id)"
    );
    assert_eq!(
        new_t3.meta.community_epoch, 2,
        "relaunched poll must be stamped at the CURRENT epoch, not the voided poll's epoch"
    );
    assert_eq!(
        new_t3.meta.config.proposal_text,
        se_config().proposal_text,
        "relaunch copies the voided poll's parameters"
    );
    assert!(
        new_t3.voided.is_none(),
        "the NEW poll must not itself be voided"
    );

    let old_t3 = log.polls[&old_poll_id].tier_state.as_tier3().unwrap();
    assert!(
        old_t3.voided.is_some(),
        "the OLD poll must still be voided after relaunch"
    );
}

/// ZEB-1031 Task 7: relaunch by someone who is neither the original
/// creator nor power-100 is rejected — no new poll is authored.
#[tokio::test]
async fn relaunch_by_non_creator_non_admin_rejected() {
    let community_id = SpaceId([0x52; 16]);
    let creator = OwnerAddr([0xA0; 16]);
    let stranger = OwnerAddr([0xB0; 16]);
    let old_poll_id = PollId([0x62; 32]);
    let reset_id: EventId = [0x72; 16];

    let voting_log = Arc::new(Mutex::new(VotingLog::new()));
    {
        let mut log = voting_log.lock().await;
        seed_poll(
            &mut log,
            old_poll_id,
            creator,
            community_id,
            1,
            Some(VoidedInfo {
                reset_id,
                old_epoch: 1,
            }),
        );
    }
    let engine = engine_with_active_committee(community_id, voting_log.clone(), 2).await;
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x43; 32]);

    let result = engine
        .relaunch_voided_poll(
            old_poll_id,
            stranger,
            /* caller_is_power_100 */ false,
            &signing_key,
            hlc(10_000, "stranger"),
            None,
        )
        .await;
    assert!(
        result.is_err(),
        "a non-creator, non-admin caller must be rejected"
    );

    // No new poll was authored — the log has exactly the one (still-voided) poll.
    let log = voting_log.lock().await;
    assert_eq!(
        log.polls.len(),
        1,
        "rejected relaunch must not create a poll"
    );
    assert!(log.polls[&old_poll_id]
        .tier_state
        .as_tier3()
        .unwrap()
        .voided
        .is_some());
}

/// ZEB-1031 Task 7: relaunch by power-100 (not the original creator)
/// succeeds — admin override.
#[tokio::test]
async fn relaunch_by_power_100_admin_succeeds() {
    let community_id = SpaceId([0x53; 16]);
    let creator = OwnerAddr([0xA0; 16]);
    let admin = OwnerAddr([0xC0; 16]);
    let old_poll_id = PollId([0x63; 32]);
    let reset_id: EventId = [0x73; 16];

    let voting_log = Arc::new(Mutex::new(VotingLog::new()));
    {
        let mut log = voting_log.lock().await;
        seed_poll(
            &mut log,
            old_poll_id,
            creator,
            community_id,
            1,
            Some(VoidedInfo {
                reset_id,
                old_epoch: 1,
            }),
        );
    }
    let engine = engine_with_active_committee(community_id, voting_log.clone(), 2).await;
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x44; 32]);

    let new_poll_id = engine
        .relaunch_voided_poll(
            old_poll_id,
            admin,
            /* caller_is_power_100 */ true,
            &signing_key,
            hlc(10_000, "admin"),
            None,
        )
        .await
        .expect("power-100 admin relaunch must succeed");

    let log = voting_log.lock().await;
    assert!(log.polls.contains_key(&new_poll_id));
}

/// ZEB-1031 Task 7: relaunching a poll that was never voided is rejected.
#[tokio::test]
async fn relaunch_of_non_voided_poll_rejected() {
    let community_id = SpaceId([0x54; 16]);
    let creator = OwnerAddr([0xA0; 16]);
    let poll_id = PollId([0x64; 32]);

    let voting_log = Arc::new(Mutex::new(VotingLog::new()));
    {
        let mut log = voting_log.lock().await;
        seed_poll(&mut log, poll_id, creator, community_id, 1, None);
    }
    let engine = engine_with_active_committee(community_id, voting_log.clone(), 1).await;
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x45; 32]);

    let result = engine
        .relaunch_voided_poll(
            poll_id,
            creator,
            false,
            &signing_key,
            hlc(10_000, "c"),
            None,
        )
        .await;
    assert!(
        result.is_err(),
        "relaunching a live (non-voided) poll must be rejected"
    );
}

/// ZEB-1031 Task 7: casting a ballot on a voided poll is rejected with the
/// exact `PollVoided` error — the terminal-state gate runs before payload
/// decode, so even a garbage payload doesn't leak a decode error instead.
#[tokio::test]
async fn ballot_cast_on_voided_poll_rejected_with_exact_error() {
    let community_id = SpaceId([0x55; 16]);
    let creator = OwnerAddr([0xA0; 16]);
    let poll_id = PollId([0x65; 32]);
    let reset_id: EventId = [0x75; 16];

    let voting_log = Arc::new(Mutex::new(VotingLog::new()));
    {
        let mut log = voting_log.lock().await;
        seed_poll(
            &mut log,
            poll_id,
            creator,
            community_id,
            1,
            Some(VoidedInfo {
                reset_id,
                old_epoch: 1,
            }),
        );
    }

    let mut log = voting_log.lock().await;
    let t3 = log
        .polls
        .get_mut(&poll_id)
        .unwrap()
        .tier_state
        .as_tier3_mut()
        .unwrap();
    let ballot_event = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::RatificationBallot,
        hlc: hlc(20_000, "voter"),
        actor: OwnerAddr([0xD0; 16]),
        payload: vec![],
        sig: vec![0u8; 64],
    };
    let err = t3
        .apply_event(&ballot_event)
        .expect_err("ballot on a voided poll must be rejected");
    assert!(
        matches!(err, ApplyError::PollVoided),
        "expected the exact PollVoided error, got {err:?}"
    );
}

/// ZEB-1031 Task 7: `voided` round-trips through the persist layer
/// (`community_voting_persist.rs`'s `PollRestore` overlay) — a restart
/// must not resurrect a reset-voided poll as live-and-mutable.
#[tokio::test]
async fn voided_state_survives_persist_round_trip() {
    use harmony_app::community_voting_persist::{load_voting_log, save_voting_log};

    let community_id = SpaceId([0x56; 16]);
    let creator = OwnerAddr([0xA0; 16]);
    let reset_id: EventId = [0x76; 16];

    // A poll needs a real kd=cr event in `events` for persist to have
    // something to replay on load — mirrors the persist module's own
    // round-trip test shape (`save_then_load_round_trips_events_and_policy`).
    // The poll's id is derived from the event's signing bytes (same as
    // production), not chosen up front.
    let mut log = VotingLog::new();
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x46; 32]);
    let config = se_config();
    let event = harmony_app::community_voting_core::build_signed_poll_create_tier3(
        &signing_key,
        creator,
        &config,
        hlc(0, "d1"),
    )
    .expect("build kd=cr");
    let sb = event.signing_bytes().expect("signing bytes");
    let poll_id = derive_poll_id(&community_id, &sb);
    log.apply_with_snapshot(event, &community_id, None)
        .expect("apply kd=cr");

    // Void it directly (mirrors what void_tier3_polls_for_reset does).
    {
        let t3 = log
            .polls
            .get_mut(&poll_id)
            .unwrap()
            .tier_state
            .as_tier3_mut()
            .unwrap();
        t3.voided = Some(VoidedInfo {
            reset_id,
            old_epoch: 1,
        });
    }

    let dir = tempfile::tempdir().unwrap();
    let path = harmony_app::community_voting_persist::voting_path_for(dir.path(), &community_id);
    let cipher = harmony_app::device_dataset_file::test_cipher();
    save_voting_log(&cipher, &path, &log, &community_id).expect("save");

    let (_events, _policy, poll_restore) =
        load_voting_log(&cipher, &path, &community_id).expect("load");
    let restore = poll_restore
        .get(&poll_id)
        .expect("poll_restore entry for the voided poll");
    let restored_voided = restore.voided.expect("voided info persisted");
    assert_eq!(restored_voided.reset_id, reset_id);
    assert_eq!(restored_voided.old_epoch, 1);
}
