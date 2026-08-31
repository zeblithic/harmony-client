//! ZEB-319 Task 2: two-engine integration tests for Tier 3 granular events.
//!
//! ## What this tests
//!
//! Verifies that the three new Tauri event emissions added in ZEB-319 Task 1
//! (`voting-tier3-mini-public-decline`, `voting-tier3-draft-candidate`,
//! `voting-tier3-draft-approval`) fire on **both**:
//!
//! 1. The **originator engine** (Engine A) via the `publish_event` path.
//! 2. The **peer engine** (Engine B) via the `process_inbound_dispatch` path,
//!    after Engine A's broadcast packet is forwarded to Engine B's subscriber
//!    channel.
//!
//! Additionally each test asserts **exactly-once semantics**:
//! - Engine A emits exactly 1 event per publish.
//! - Engine B emits exactly 1 event per inbound dispatch.
//! - Engine A's captured count stays at 1 after the forward (originator does
//!   NOT re-enter inbound dispatch — no double emit).
//!
//! ## Harness pattern
//!
//! Mirrors `community_voting_tier3_secret_kd_ts_emission_integration.rs`
//! (the ZEB-295 kd=ts test) for engine setup, and
//! `process_inbound_tier2_signal_fires_delegate_on_behalf` (the ZEB-298
//! Task 8 unit test) for the resolver-wired two-engine forwarding pattern.
//!
//! Engine B uses `FixedTestResolvers` equivalents (identity + membership
//! snapshot) so `process_inbound`'s verify + apply path activates rather
//! than short-circuiting on "resolvers not wired".

#![cfg(feature = "test-fixtures")]

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};

use harmony_app::community_voting_core::{
    build_signed_draft_approval, build_signed_draft_candidate, build_signed_mini_public_decline,
    build_signed_poll_create_tier3, build_signed_sortition_selection, derive_poll_id, Eligibility,
    MemberAttrs, MembershipSnapshot, PollId, Tier3PollConfigPayload, VotingIdentityResolver,
};
use harmony_app::community_voting_log::{
    MembershipSnapshotResolver, SnapshotResolverError, VotingLog,
};
use harmony_app::community_voting_log_engine::{VotingLogEngine, VotingLogEngineParams};
use harmony_app::community_voting_tier3::event_hash_of;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

// ─── Resolver shims ───────────────────────────────────────────────────────────

/// Fixed-snapshot resolver used for Engine B's inbound verify + apply path.
struct FixedResolver {
    identity: HashMap<OwnerAddr, [u8; 64]>,
    snapshot: MembershipSnapshot,
}

#[async_trait::async_trait]
impl VotingIdentityResolver for FixedResolver {
    async fn resolve(&self, owner: &OwnerAddr) -> Option<[u8; 64]> {
        self.identity.get(owner).copied()
    }
}

#[async_trait::async_trait]
impl MembershipSnapshotResolver for FixedResolver {
    async fn snapshot_at(
        &self,
        _community_id: SpaceId,
        _hlc: &Hlc,
    ) -> Result<MembershipSnapshot, SnapshotResolverError> {
        Ok(self.snapshot.clone())
    }
}

// ─── Identity helper ──────────────────────────────────────────────────────────

/// Derive `(SigningKey, OwnerAddr, [u8; 64])` from a single-byte seed.
/// Mirrors the private `fixture_identity_engine` helper in the engine unit
/// tests; reimplemented here because integration tests compile against the
/// public crate API only.
fn fixture_identity(seed: u8) -> (ed25519_dalek::SigningKey, OwnerAddr, [u8; 64]) {
    let priv_id = harmony_identity::PrivateIdentity::from_seed(&[seed; 32]);
    let owner = OwnerAddr(priv_id.identity.address_hash);
    let pub_64 = priv_id.identity.to_public_bytes();
    let private_bytes = priv_id.to_private_bytes();
    let mut ed_secret = [0u8; 32];
    ed_secret.copy_from_slice(&private_bytes[32..64]);
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_secret);
    (signing_key, owner, pub_64)
}

// ─── Wait helper ─────────────────────────────────────────────────────────────

/// Poll `captured` until it holds at least `min_count` entries or `timeout`
/// elapses, then return a snapshot. Mirrors the private `wait_for_emits`
/// helper in the engine unit tests.
async fn wait_for_emits(
    captured: Arc<std::sync::Mutex<Vec<String>>>,
    min_count: usize,
    timeout: std::time::Duration,
) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        {
            let v = captured.lock().expect("captured lock");
            if v.len() >= min_count {
                return v.clone();
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let v = captured.lock().expect("captured lock");
            return v.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

// ─── Shared test harness ──────────────────────────────────────────────────────

/// Build a Tier 3 poll config used by all three tests.
fn tier3_config() -> Tier3PollConfigPayload {
    Tier3PollConfigPayload {
        proposal_text: "ZEB-319 two-engine integration test".into(),
        sortition_size: 20,
        deliberation_window_seconds: 7200,
        drafting_window_seconds: 7200,
        ratification_window_seconds: 7200,
        privacy_mode: "pu".into(),
        incentive_mode: "a".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: None,
        ce: None,
    }
}

fn hlc_at(wall_ms: u64, device_id: &str) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: device_id.into(),
    }
}

/// Shared fixture: build two connected VotingLogEngine instances (A + B) with
/// a Tier 3 poll pre-seeded past sortition on both engines.
///
/// Returns:
/// - `engine_a`: the originator engine (no resolvers — publishes via IPC path)
/// - `engine_b`: the peer engine (resolvers wired — receives inbound packets)
/// - `app_handle_a` / `app_handle_b`: mock Tauri handles for event capture
/// - `publisher_rx_a`: receives packets that Engine A broadcasts (forwarded to B)
/// - `subscriber_tx_b`: send packets here to drive Engine B's inbound path
/// - `poll_id`: the PollId of the pre-seeded Tier 3 poll
/// - `actor_owner`: OwnerAddr of the mini-public member used as actor in tests
/// - `actor_signing_key`: the actor's signing key
#[allow(clippy::type_complexity)]
async fn two_engine_fixture(
    community_id: SpaceId,
    actor_seed: u8,
    proposer_seed: u8,
) -> (
    Arc<VotingLogEngine<tauri::test::MockRuntime>>,
    Arc<VotingLogEngine<tauri::test::MockRuntime>>,
    tauri::AppHandle<tauri::test::MockRuntime>,
    tauri::AppHandle<tauri::test::MockRuntime>,
    mpsc::Receiver<Vec<u8>>,
    mpsc::Sender<Vec<u8>>,
    PollId,
    OwnerAddr,
    ed25519_dalek::SigningKey,
) {
    let (actor_key, actor_owner, actor_pub64) = fixture_identity(actor_seed);
    let (proposer_key, proposer_owner, proposer_pub64) = fixture_identity(proposer_seed);

    // Build membership snapshot: 40 electorate members + actor + proposer.
    // sortition_size = 20; electorate needs >= 2 * sortition_size = 40 members
    // for the SortitionSelection apply to pass.
    let electorate: Vec<OwnerAddr> = (0usize..40)
        .map(|i| {
            let mut a = [0u8; 16];
            a[0] = (i & 0xFF) as u8;
            a[1] = community_id.0[0]; // namespace by community to avoid collisions
            OwnerAddr(a)
        })
        .collect();

    // Primary mini-public: actor_owner + 19 more from electorate.
    let primary: Vec<OwnerAddr> = {
        let mut p = vec![actor_owner];
        p.extend(electorate.iter().take(19).copied());
        p
    };

    let mut members: HashMap<OwnerAddr, MemberAttrs> = electorate
        .iter()
        .map(|o| {
            (
                *o,
                MemberAttrs {
                    power: 1,
                    vouching_depth: 0,
                },
            )
        })
        .collect();
    members.insert(
        actor_owner,
        MemberAttrs {
            power: 1,
            vouching_depth: 0,
        },
    );
    members.insert(
        proposer_owner,
        MemberAttrs {
            power: 1,
            vouching_depth: 0,
        },
    );
    let snapshot = MembershipSnapshot { members };

    // Build and apply PollCreate + SortitionSelection into a shared
    // VotingLog base. Both engines get independent copies of this log
    // (Engine A and Engine B each hold their own Arc<Mutex<VotingLog>>).
    let config = tier3_config();

    let event_cr = build_signed_poll_create_tier3(
        &proposer_key,
        proposer_owner,
        &config,
        hlc_at(1_000_000, "dev-cr"),
    )
    .expect("build kd=cr");
    let signing_bytes_cr = event_cr.signing_bytes().expect("signing_bytes kd=cr");
    let poll_id = derive_poll_id(&community_id, &signing_bytes_cr);

    let event_ss = build_signed_sortition_selection(
        &proposer_key,
        proposer_owner,
        poll_id,
        primary.clone(),
        vec![],
        hlc_at(1_000_001, "dev-ss"),
    )
    .expect("build kd=ss");

    // ── Engine A setup ────────────────────────────────────────────────────

    let voting_log_a = Arc::new(Mutex::new(VotingLog::new()));
    {
        let mut log = voting_log_a.lock().await;
        log.apply_with_snapshot(event_cr.clone(), &community_id, Some(snapshot.clone()))
            .expect("A: apply kd=cr");
        log.apply_with_snapshot(event_ss.clone(), &community_id, None)
            .expect("A: apply kd=ss");
    }

    let (pub_tx_a, pub_rx_a) = mpsc::channel::<Vec<u8>>(32);
    let (_sub_tx_a, sub_rx_a) = mpsc::channel::<Vec<u8>>(32);

    let app_a = tauri::test::mock_app();
    let app_handle_a = app_a.handle().clone();

    let engine_a = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id,
        voting_log: Arc::clone(&voting_log_a),
        publisher_tx: pub_tx_a,
        subscriber_rx: sub_rx_a,
        hlc_tracker: None,
        device_id: None,
        app_handle: Some(app_handle_a.clone()),
        identity_resolver: None,
        membership_resolver: None,
    })
    .await;

    engine_a
        .install_local_signing_key(Arc::new(actor_key.clone()), actor_owner)
        .await;

    // ── Engine B setup ────────────────────────────────────────────────────

    let voting_log_b = Arc::new(Mutex::new(VotingLog::new()));
    {
        let mut log = voting_log_b.lock().await;
        log.apply_with_snapshot(event_cr.clone(), &community_id, Some(snapshot.clone()))
            .expect("B: apply kd=cr");
        log.apply_with_snapshot(event_ss.clone(), &community_id, None)
            .expect("B: apply kd=ss");
    }

    let (_pub_tx_b, pub_rx_b_discard) = mpsc::channel::<Vec<u8>>(32);
    let (sub_tx_b, sub_rx_b) = mpsc::channel::<Vec<u8>>(32);

    // Engine B needs identity + membership resolvers so process_inbound
    // activates the verify + apply path rather than silently dropping.
    let mut id_map = HashMap::new();
    id_map.insert(actor_owner, actor_pub64);
    id_map.insert(proposer_owner, proposer_pub64);
    let resolvers_b = Arc::new(FixedResolver {
        identity: id_map,
        snapshot: snapshot.clone(),
    });

    let app_b = tauri::test::mock_app();
    let app_handle_b = app_b.handle().clone();

    let engine_b = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id,
        voting_log: Arc::clone(&voting_log_b),
        publisher_tx: _pub_tx_b,
        subscriber_rx: sub_rx_b,
        hlc_tracker: None,
        device_id: None,
        app_handle: Some(app_handle_b.clone()),
        identity_resolver: Some(resolvers_b.clone()),
        membership_resolver: Some(resolvers_b),
    })
    .await;

    // Keep pub_rx_b_discard alive so Engine B's publisher channel stays open.
    // We store it as part of context — the test must hold it alive.
    // (Actually, we return only what we need and let Rust drop the rest.)
    drop(pub_rx_b_discard);

    (
        engine_a,
        engine_b,
        app_handle_a,
        app_handle_b,
        pub_rx_a,
        sub_tx_b,
        poll_id,
        actor_owner,
        actor_key,
    )
}

// ─── Test 1: kd=md MiniPublicDecline ─────────────────────────────────────────

/// Engine A publishes `kd=md` (MiniPublicDecline) and emits
/// `voting-tier3-mini-public-decline` once. The broadcast packet is
/// forwarded to Engine B's subscriber channel; Engine B processes it
/// inbound and emits the same event once. Engine A's captured count
/// stays at 1 (no double-emit on originator side).
#[tokio::test]
async fn voting_tier3_mini_public_decline_emits_on_originator_and_peer() {
    use std::time::Duration;
    use tauri::Listener;

    let community_id = SpaceId([0xD1; 16]);
    let (
        engine_a,
        engine_b,
        app_handle_a,
        app_handle_b,
        mut pub_rx_a,
        sub_tx_b,
        poll_id,
        actor_owner,
        actor_key,
    ) = two_engine_fixture(community_id, 0xA1, 0xA2).await;

    // Install listeners on both engines' app handles.
    let captured_a: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_b: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_a_clone = Arc::clone(&captured_a);
    let captured_b_clone = Arc::clone(&captured_b);

    app_handle_a.listen("voting-tier3-mini-public-decline", move |evt| {
        captured_a_clone
            .lock()
            .expect("captured_a lock")
            .push(evt.payload().to_string());
    });
    app_handle_b.listen("voting-tier3-mini-public-decline", move |evt| {
        captured_b_clone
            .lock()
            .expect("captured_b lock")
            .push(evt.payload().to_string());
    });

    // Engine A: publish a kd=md event.
    let md_event = build_signed_mini_public_decline(
        &actor_key,
        actor_owner,
        poll_id,
        None,
        hlc_at(1_100_000, "dev-md"),
    )
    .expect("build kd=md");

    engine_a
        .publish_event(md_event, None)
        .await
        .expect("Engine A: publish kd=md");

    // Assert Engine A emitted exactly once.
    let payloads_a = wait_for_emits(Arc::clone(&captured_a), 1, Duration::from_secs(2)).await;
    assert_eq!(
        payloads_a.len(),
        1,
        "Engine A must emit voting-tier3-mini-public-decline exactly once; got {payloads_a:?}"
    );
    let parsed_a: serde_json::Value =
        serde_json::from_str(&payloads_a[0]).expect("Engine A payload is valid JSON");
    assert!(parsed_a["pollId"].is_string(), "A: pollId present");
    assert!(
        parsed_a["communityId"].is_string(),
        "A: communityId present"
    );
    assert_eq!(
        parsed_a["decliner"].as_str(),
        Some(hex::encode(actor_owner.0).as_str()),
        "A: decliner == hex(actor_owner.0)"
    );
    assert_eq!(
        parsed_a["declineHlcMs"].as_u64(),
        Some(1_100_000u64),
        "A: declineHlcMs == wall_ms of the md event"
    );
    {
        let obj = parsed_a.as_object().expect("A: payload is object");
        assert_eq!(
            obj.len(),
            4,
            "A: payload must have exactly 4 keys (pollId, communityId, decliner, declineHlcMs); \
             got {obj:?}"
        );
    }

    // Forward Engine A's broadcast packet to Engine B's subscriber channel.
    let packet = tokio::time::timeout(Duration::from_secs(2), pub_rx_a.recv())
        .await
        .expect("Engine A broadcast packet within timeout")
        .expect("publisher channel open");

    sub_tx_b
        .send(packet)
        .await
        .expect("forward packet to Engine B subscriber channel");

    // Assert Engine B emitted exactly once.
    let payloads_b = wait_for_emits(Arc::clone(&captured_b), 1, Duration::from_secs(2)).await;
    assert_eq!(
        payloads_b.len(),
        1,
        "Engine B must emit voting-tier3-mini-public-decline exactly once; got {payloads_b:?}"
    );
    let parsed_b: serde_json::Value =
        serde_json::from_str(&payloads_b[0]).expect("Engine B payload is valid JSON");
    assert!(parsed_b["pollId"].is_string(), "B: pollId present");
    assert!(
        parsed_b["communityId"].is_string(),
        "B: communityId present"
    );
    assert_eq!(
        parsed_b["decliner"].as_str(),
        Some(hex::encode(actor_owner.0).as_str()),
        "B: decliner == hex(actor_owner.0)"
    );
    assert_eq!(
        parsed_b["declineHlcMs"].as_u64(),
        Some(1_100_000u64),
        "B: declineHlcMs == wall_ms of the md event"
    );

    // Exactly-once check: Engine A's captured count must still be 1.
    // Give the engine a brief window to process any stray second emit.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let final_a = captured_a.lock().expect("captured_a final lock").clone();
    assert_eq!(
        final_a.len(),
        1,
        "Engine A (originator) must NOT emit a second time after forwarding to B; \
         got {final_a:?}"
    );

    drop(engine_a);
    drop(engine_b);
    drop(sub_tx_b);
}

// ─── Test 2: kd=dc DraftCandidate ────────────────────────────────────────────

/// Engine A publishes `kd=dc` (DraftCandidate) and emits
/// `voting-tier3-draft-candidate` once. The broadcast packet is forwarded
/// to Engine B; Engine B emits the same event once. Engine A's count
/// stays at 1.
#[tokio::test]
async fn voting_tier3_draft_candidate_emits_on_originator_and_peer() {
    use std::time::Duration;
    use tauri::Listener;

    let community_id = SpaceId([0xD2; 16]);
    let (
        engine_a,
        engine_b,
        app_handle_a,
        app_handle_b,
        mut pub_rx_a,
        sub_tx_b,
        poll_id,
        actor_owner,
        actor_key,
    ) = two_engine_fixture(community_id, 0xB1, 0xB2).await;

    let captured_a: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_b: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_a_clone = Arc::clone(&captured_a);
    let captured_b_clone = Arc::clone(&captured_b);

    app_handle_a.listen("voting-tier3-draft-candidate", move |evt| {
        captured_a_clone
            .lock()
            .expect("captured_a lock")
            .push(evt.payload().to_string());
    });
    app_handle_b.listen("voting-tier3-draft-candidate", move |evt| {
        captured_b_clone
            .lock()
            .expect("captured_b lock")
            .push(evt.payload().to_string());
    });

    let candidate_text = "Two-engine integration candidate proposal".to_string();
    let dc_event = build_signed_draft_candidate(
        &actor_key,
        actor_owner,
        poll_id,
        candidate_text.clone(),
        hlc_at(1_200_000, "dev-dc"),
    )
    .expect("build kd=dc");

    // Capture expected event_hash BEFORE publish_event consumes the event.
    let expected_event_hash = event_hash_of(&dc_event);

    engine_a
        .publish_event(dc_event, None)
        .await
        .expect("Engine A: publish kd=dc");

    // Assert Engine A emitted exactly once with correct payload.
    let payloads_a = wait_for_emits(Arc::clone(&captured_a), 1, Duration::from_secs(2)).await;
    assert_eq!(
        payloads_a.len(),
        1,
        "Engine A must emit voting-tier3-draft-candidate exactly once; got {payloads_a:?}"
    );
    let parsed_a: serde_json::Value =
        serde_json::from_str(&payloads_a[0]).expect("Engine A payload is valid JSON");
    assert!(parsed_a["pollId"].is_string(), "A: pollId present");
    assert!(
        parsed_a["communityId"].is_string(),
        "A: communityId present"
    );
    assert_eq!(
        parsed_a["proposer"].as_str(),
        Some(hex::encode(actor_owner.0).as_str()),
        "A: proposer == hex(actor_owner.0)"
    );
    assert_eq!(
        parsed_a["eventHash"].as_str(),
        Some(hex::encode(expected_event_hash).as_str()),
        "A: eventHash == hex(event_hash_of(dc_event))"
    );
    assert_eq!(
        parsed_a["candidateText"].as_str(),
        Some(candidate_text.as_str()),
        "A: candidateText matches"
    );
    {
        let obj = parsed_a.as_object().expect("A: payload is object");
        assert_eq!(
            obj.len(),
            5,
            "A: payload must have exactly 5 keys (pollId, communityId, proposer, eventHash, \
             candidateText); got {obj:?}"
        );
    }

    // Forward to Engine B.
    let packet = tokio::time::timeout(Duration::from_secs(2), pub_rx_a.recv())
        .await
        .expect("Engine A broadcast packet within timeout")
        .expect("publisher channel open");

    sub_tx_b
        .send(packet)
        .await
        .expect("forward packet to Engine B subscriber channel");

    // Assert Engine B emitted exactly once.
    let payloads_b = wait_for_emits(Arc::clone(&captured_b), 1, Duration::from_secs(2)).await;
    assert_eq!(
        payloads_b.len(),
        1,
        "Engine B must emit voting-tier3-draft-candidate exactly once; got {payloads_b:?}"
    );
    let parsed_b: serde_json::Value =
        serde_json::from_str(&payloads_b[0]).expect("Engine B payload is valid JSON");
    assert!(parsed_b["pollId"].is_string(), "B: pollId present");
    assert!(
        parsed_b["communityId"].is_string(),
        "B: communityId present"
    );
    assert_eq!(
        parsed_b["proposer"].as_str(),
        Some(hex::encode(actor_owner.0).as_str()),
        "B: proposer == hex(actor_owner.0)"
    );
    assert_eq!(
        parsed_b["eventHash"].as_str(),
        Some(hex::encode(expected_event_hash).as_str()),
        "B: eventHash matches originator's"
    );
    assert_eq!(
        parsed_b["candidateText"].as_str(),
        Some(candidate_text.as_str()),
        "B: candidateText matches"
    );

    // Exactly-once: Engine A count stays at 1.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let final_a = captured_a.lock().expect("captured_a final lock").clone();
    assert_eq!(
        final_a.len(),
        1,
        "Engine A (originator) must NOT emit a second time after forwarding to B; \
         got {final_a:?}"
    );

    drop(engine_a);
    drop(engine_b);
    drop(sub_tx_b);
}

// ─── Test 3: kd=da DraftApproval ─────────────────────────────────────────────

/// Engine A publishes `kd=dc` (to create a candidate), then `kd=da`
/// (DraftApproval referencing the candidate). Only the `kd=da` emit
/// (`voting-tier3-draft-approval`) is under test. Both packets are
/// forwarded to Engine B; Engine B emits `voting-tier3-draft-approval`
/// exactly once. Engine A's count stays at 1.
#[tokio::test]
async fn voting_tier3_draft_approval_emits_on_originator_and_peer() {
    use std::time::Duration;
    use tauri::Listener;

    let community_id = SpaceId([0xD3; 16]);
    let (
        engine_a,
        engine_b,
        app_handle_a,
        app_handle_b,
        mut pub_rx_a,
        sub_tx_b,
        poll_id,
        actor_owner,
        actor_key,
    ) = two_engine_fixture(community_id, 0xC1, 0xC2).await;

    // Listen for both kd=dc and kd=da on Engine A (need to drain kd=dc
    // from publisher_rx_a first before the kd=da packet arrives).
    let captured_a_dc: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_a_da: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_b_dc: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_b_da: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    let captured_a_dc_clone = Arc::clone(&captured_a_dc);
    let captured_a_da_clone = Arc::clone(&captured_a_da);
    let captured_b_dc_clone = Arc::clone(&captured_b_dc);
    let captured_b_da_clone = Arc::clone(&captured_b_da);

    app_handle_a.listen("voting-tier3-draft-candidate", move |evt| {
        captured_a_dc_clone
            .lock()
            .expect("captured_a_dc lock")
            .push(evt.payload().to_string());
    });
    app_handle_a.listen("voting-tier3-draft-approval", move |evt| {
        captured_a_da_clone
            .lock()
            .expect("captured_a_da lock")
            .push(evt.payload().to_string());
    });
    app_handle_b.listen("voting-tier3-draft-candidate", move |evt| {
        captured_b_dc_clone
            .lock()
            .expect("captured_b_dc lock")
            .push(evt.payload().to_string());
    });
    app_handle_b.listen("voting-tier3-draft-approval", move |evt| {
        captured_b_da_clone
            .lock()
            .expect("captured_b_da lock")
            .push(evt.payload().to_string());
    });

    // Step 1: Engine A publishes kd=dc to establish a candidate.
    let dc_event = build_signed_draft_candidate(
        &actor_key,
        actor_owner,
        poll_id,
        "Approval test candidate".into(),
        hlc_at(1_200_000, "dev-dc-for-da"),
    )
    .expect("build kd=dc");
    let candidate_event_hash = event_hash_of(&dc_event);

    engine_a
        .publish_event(dc_event, None)
        .await
        .expect("Engine A: publish kd=dc for approval test");

    // Wait for Engine A's kd=dc emit so we're sure the candidate is in the
    // projection before publishing kd=da.
    wait_for_emits(Arc::clone(&captured_a_dc), 1, Duration::from_secs(2)).await;

    // Drain the kd=dc broadcast packet from publisher_rx_a and forward to
    // Engine B so B also has the candidate in its projection before kd=da
    // arrives there.
    let dc_packet = tokio::time::timeout(Duration::from_secs(2), pub_rx_a.recv())
        .await
        .expect("Engine A dc broadcast packet within timeout")
        .expect("publisher channel open");
    sub_tx_b
        .send(dc_packet)
        .await
        .expect("forward kd=dc packet to Engine B");

    // Wait deterministically for Engine B to emit voting-tier3-draft-candidate,
    // which implies the candidate is in its projection before kd=da arrives.
    // Replacing the previous 100ms wall-clock sleep which was flaky on slow machines.
    let payloads_b_dc = wait_for_emits(Arc::clone(&captured_b_dc), 1, Duration::from_secs(2)).await;
    assert_eq!(
        payloads_b_dc.len(),
        1,
        "Engine B must process kd=dc before kd=da is published; got {payloads_b_dc:?}"
    );

    // Step 2: Engine A publishes kd=da referencing the candidate.
    let da_event = build_signed_draft_approval(
        &actor_key,
        actor_owner,
        poll_id,
        candidate_event_hash,
        hlc_at(1_300_000, "dev-da"),
    )
    .expect("build kd=da");

    engine_a
        .publish_event(da_event, None)
        .await
        .expect("Engine A: publish kd=da");

    // Assert Engine A emitted kd=da exactly once with correct payload.
    let payloads_a = wait_for_emits(Arc::clone(&captured_a_da), 1, Duration::from_secs(2)).await;
    assert_eq!(
        payloads_a.len(),
        1,
        "Engine A must emit voting-tier3-draft-approval exactly once; got {payloads_a:?}"
    );
    let parsed_a: serde_json::Value =
        serde_json::from_str(&payloads_a[0]).expect("Engine A kd=da payload is valid JSON");
    assert!(parsed_a["pollId"].is_string(), "A: pollId present");
    assert!(
        parsed_a["communityId"].is_string(),
        "A: communityId present"
    );
    assert_eq!(
        parsed_a["approver"].as_str(),
        Some(hex::encode(actor_owner.0).as_str()),
        "A: approver == hex(actor_owner.0)"
    );
    assert_eq!(
        parsed_a["targetEventHash"].as_str(),
        Some(hex::encode(candidate_event_hash).as_str()),
        "A: targetEventHash == hex(candidate_event_hash)"
    );
    {
        let obj = parsed_a.as_object().expect("A: kd=da payload is object");
        assert_eq!(
            obj.len(),
            4,
            "A: payload must have exactly 4 keys (pollId, communityId, approver, \
             targetEventHash); got {obj:?}"
        );
    }

    // Forward kd=da broadcast packet to Engine B.
    let da_packet = tokio::time::timeout(Duration::from_secs(2), pub_rx_a.recv())
        .await
        .expect("Engine A da broadcast packet within timeout")
        .expect("publisher channel open");
    sub_tx_b
        .send(da_packet)
        .await
        .expect("forward kd=da packet to Engine B");

    // Assert Engine B emitted kd=da exactly once.
    let payloads_b = wait_for_emits(Arc::clone(&captured_b_da), 1, Duration::from_secs(2)).await;
    assert_eq!(
        payloads_b.len(),
        1,
        "Engine B must emit voting-tier3-draft-approval exactly once; got {payloads_b:?}"
    );
    let parsed_b: serde_json::Value =
        serde_json::from_str(&payloads_b[0]).expect("Engine B kd=da payload is valid JSON");
    assert!(parsed_b["pollId"].is_string(), "B: pollId present");
    assert!(
        parsed_b["communityId"].is_string(),
        "B: communityId present"
    );
    assert_eq!(
        parsed_b["approver"].as_str(),
        Some(hex::encode(actor_owner.0).as_str()),
        "B: approver == hex(actor_owner.0)"
    );
    assert_eq!(
        parsed_b["targetEventHash"].as_str(),
        Some(hex::encode(candidate_event_hash).as_str()),
        "B: targetEventHash matches originator's"
    );

    // Exactly-once: Engine A's kd=da count stays at 1.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let final_a = captured_a_da
        .lock()
        .expect("captured_a_da final lock")
        .clone();
    assert_eq!(
        final_a.len(),
        1,
        "Engine A (originator) must NOT emit a second kd=da after forwarding to B; \
         got {final_a:?}"
    );

    drop(engine_a);
    drop(engine_b);
    drop(sub_tx_b);
}
