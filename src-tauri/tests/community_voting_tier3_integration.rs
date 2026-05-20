//! ZEB-309 Phase 4a-main: multi-engine integration tests for Tier 3
//! sortition + STAR ratification + drafting. Builds on the ZEB-307
//! two-engine bidirectional-mpsc bridge pattern from
//! community_dfrost_transport_integration.rs.
//!
//! ## Layout
//!
//! - `fixture_identity` — deterministic Ed25519 keys + OwnerAddrs that bind
//!   through the address_hash verify gate.
//! - `StaticIdentityResolver` — minimal IdentityResolver for tests.
//! - `setup_two_voting_engine_bridge` — creates two VotingLogEngine instances
//!   wired with bidirectional mpsc bridges (same shape as dfrost transport test).
//! - `wait_for` — polling helper that avoids tokio::time::sleep flakiness.
//! - Event builders: `build_tier3_poll_create_event`, `build_decline_event`,
//!   `build_draft_candidate_event`, `build_draft_approval_event`,
//!   `build_ratification_ballot_event`, `build_sortition_failed_event`.
//! - Smoke tests: bridge starts/shuts down cleanly; Tier 1 ballot exchange
//!   works through the bridge (sanity check before Tier 3 behavior tests in
//!   Tasks 13-16).
//!
//! ## Why no DKG setup here
//!
//! A full DfrostLog DKG flow is heavy and already tested in ZEB-301/303/307.
//! Task 12's scope is fixture infrastructure. DfrostLog integration (beacon
//! path) is deferred to Task 13 which sets up a pre-computed kd=vb state.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use harmony_app::community_membership::ChannelId;
use harmony_app::community_state_sync::IdentityResolver;
use harmony_app::community_voting_approval::Tier1PollConfig;
use harmony_app::community_voting_core::{
    build_signed_poll_create_tier1, derive_poll_id, CandidateEventHash, DraftApprovalPayload,
    DraftCandidatePayload, Eligibility, MemberAttrs, MembershipSnapshot, MiniPublicDeclinePayload,
    PollEventKindCode, RatificationBallotPayload, SignedVotingEvent, SortitionFailedPayload,
    SortitionSelectionPayload, Tier, Tier3PollConfigPayload,
};
use harmony_app::community_voting_log::VotingLog;
use harmony_app::community_voting_log_engine::{VotingLogEngine, VotingLogEngineParams};
use harmony_app::community_voting_sortition::fisher_yates_select;
use harmony_app::community_voting_star::tally_star;
use harmony_app::community_voting_tier3::{
    drafting_advancers, ratification_candidates_ordering, synthesize_status_quo, Stage,
    Tier3PollResultPayload,
};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use tokio::sync::{mpsc, Mutex};

// ─── Identity-resolver helper ──────────────────────────────────────────────────

/// Minimal `IdentityResolver` backed by a `HashMap`. Mirrors `StaticResolver`
/// in `community_dfrost_transport_integration.rs`. Tests that exercise the
/// VotingLogEngine inbound path need a resolver if the verify gate is ever
/// wired; for now it is present for completeness.
#[allow(dead_code)]
struct StaticIdentityResolver(HashMap<OwnerAddr, [u8; 64]>);

#[async_trait::async_trait]
impl IdentityResolver for StaticIdentityResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        self.0.get(addr).copied()
    }
}

// ─── Test identity ─────────────────────────────────────────────────────────────

/// A convenience bundle returned by `fixture_identity`.
pub struct TestIdentity {
    pub owner: OwnerAddr,
    pub signing_key: ed25519_dalek::SigningKey,
    #[allow(dead_code)]
    pub verifying_key: ed25519_dalek::VerifyingKey,
    /// Raw 64-byte identity composite (X25519 || Ed25519). Needed to
    /// populate `StaticIdentityResolver` so the address_hash binding passes
    /// through the dfrost engine's `verify_signed_committee_event` gate.
    pub pub_64: [u8; 64],
}

/// Build a `TestIdentity` from a single-byte seed. The returned `owner`'s
/// `address_hash` field is derived from the public key bytes — the same
/// binding enforced by `verify_signed_committee_event` in the dfrost engine.
///
/// Mirrors `fixture_identity` in `community_dfrost_transport_integration.rs`.
pub fn fixture_identity(seed: u8) -> TestIdentity {
    let priv_id = harmony_identity::PrivateIdentity::from_seed(&[seed; 32]);
    let owner = OwnerAddr(priv_id.identity.address_hash);
    let pub_64 = priv_id.identity.to_public_bytes();
    let private_bytes = priv_id.to_private_bytes();
    let mut ed_secret = [0u8; 32];
    ed_secret.copy_from_slice(&private_bytes[32..64]);
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_secret);
    let verifying_key = signing_key.verifying_key();
    TestIdentity {
        owner,
        signing_key,
        verifying_key,
        pub_64,
    }
}

// ─── HLC helpers ──────────────────────────────────────────────────────────────

fn hlc_at(wall_ms: u64, device_id: &str) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: device_id.into(),
    }
}

// ─── Two-engine bridge ─────────────────────────────────────────────────────────

/// Handles for both sides of a two-VotingLogEngine bridge. Drop to shut
/// everything down cleanly.
pub struct TwoVotingEngines {
    pub engine_a: Arc<VotingLogEngine<tauri::test::MockRuntime>>,
    pub engine_b: Arc<VotingLogEngine<tauri::test::MockRuntime>>,
    pub log_a: Arc<Mutex<VotingLog>>,
    pub log_b: Arc<Mutex<VotingLog>>,
}

/// Wire two `VotingLogEngine` instances with bidirectional mpsc channels,
/// mirroring the two-engine bridge in `community_dfrost_transport_integration.rs`.
///
/// Spawns two forwarder tasks that relay published bytes from engine A's
/// outbound channel into engine B's inbound channel and vice versa. When
/// either engine is dropped its publisher sender closes, the forwarder
/// receives `None`, and exits cleanly.
pub async fn setup_two_voting_engine_bridge(community_id: SpaceId) -> TwoVotingEngines {
    // Channel pairs: each engine gets its own pub_tx/sub_rx.
    // The forwarder tasks relay: a_pub_rx → b_sub_tx and b_pub_rx → a_sub_tx.
    let (a_pub_tx, mut a_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_pub_tx, mut b_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (a_sub_tx, a_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_sub_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(64);

    // Forwarder: A → B
    let b_sub_tx_clone = b_sub_tx.clone();
    tokio::spawn(async move {
        while let Some(packet) = a_pub_rx.recv().await {
            if b_sub_tx_clone.send(packet).await.is_err() {
                break;
            }
        }
    });

    // Forwarder: B → A
    let a_sub_tx_clone = a_sub_tx.clone();
    tokio::spawn(async move {
        while let Some(packet) = b_pub_rx.recv().await {
            if a_sub_tx_clone.send(packet).await.is_err() {
                break;
            }
        }
    });

    let log_a = Arc::new(Mutex::new(VotingLog::new()));
    let log_b = Arc::new(Mutex::new(VotingLog::new()));

    let engine_a = VotingLogEngine::start(VotingLogEngineParams {
        community_id,
        voting_log: Arc::clone(&log_a),
        publisher_tx: a_pub_tx,
        subscriber_rx: a_sub_rx,
    })
    .await;

    let engine_b = VotingLogEngine::start(VotingLogEngineParams {
        community_id,
        voting_log: Arc::clone(&log_b),
        publisher_tx: b_pub_tx,
        subscriber_rx: b_sub_rx,
    })
    .await;

    TwoVotingEngines {
        engine_a,
        engine_b,
        log_a,
        log_b,
    }
}

// ─── Polling helper ────────────────────────────────────────────────────────────

/// Poll `predicate()` every `poll_interval_ms` until it returns `Some(T)`
/// or `timeout_ms` elapses. Returns `None` on timeout.
///
/// This is the ZEB-307 R3 pattern: avoids a fixed `tokio::time::sleep`
/// that would make tests flaky under heavy load.
pub async fn wait_for<F, T>(timeout_ms: u64, poll_interval_ms: u64, mut predicate: F) -> Option<T>
where
    F: FnMut() -> Option<T>,
{
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(timeout_ms) {
        if let Some(v) = predicate() {
            return Some(v);
        }
        tokio::time::sleep(Duration::from_millis(poll_interval_ms)).await;
    }
    None
}

/// Convenience wrapper: poll log_a + log_b until `predicate(log)` is true
/// on the target log. Times out after 5 000 ms.
pub async fn wait_for_log<F>(
    label: &str,
    log: &Arc<Mutex<VotingLog>>,
    predicate: F,
) -> Result<(), String>
where
    F: Fn(&VotingLog) -> bool,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        {
            let guard = log.lock().await;
            if predicate(&guard) {
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("wait_for_log({label}) timed out after 5s"));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ─── Event builders ────────────────────────────────────────────────────────────

/// Default valid Tier 3 poll config (sortition_size 20 satisfies the [20,300]
/// validation range; windows satisfy the ≥60s floor).
pub fn default_tier3_config() -> Tier3PollConfigPayload {
    Tier3PollConfigPayload {
        proposal_text: "Should the community adopt proposal X?".into(),
        sortition_size: 20,
        deliberation_window_seconds: 3600,
        drafting_window_seconds: 3600,
        ratification_window_seconds: 3600,
        privacy_mode: "pu".into(),
        incentive_mode: "a".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
    }
}

/// Build a signed Tier 3 PollCreate (kd=cr, tier=Sortition) event.
pub fn build_tier3_poll_create_event(
    proposer: &TestIdentity,
    config: &Tier3PollConfigPayload,
    hlc: Hlc,
) -> SignedVotingEvent {
    use ed25519_dalek::Signer;
    let mut payload = Vec::new();
    ciborium::into_writer(config, &mut payload).expect("encode Tier3PollConfigPayload");
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::PollCreate,
        hlc,
        actor: proposer.owner,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().expect("signing_bytes for tier3 create");
    ev.sig = proposer.signing_key.sign(&sb).to_bytes().to_vec();
    ev
}

/// Build a signed kd=md MiniPublicDecline event.
pub fn build_decline_event(
    actor: &TestIdentity,
    poll_id: harmony_app::community_voting_core::PollId,
    hlc: Hlc,
) -> SignedVotingEvent {
    use ed25519_dalek::Signer;
    let payload_struct = MiniPublicDeclinePayload {
        poll_id,
        reason: None,
    };
    let mut payload = Vec::new();
    ciborium::into_writer(&payload_struct, &mut payload).expect("encode MiniPublicDeclinePayload");
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::MiniPublicDecline,
        hlc,
        actor: actor.owner,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().expect("signing_bytes for decline");
    ev.sig = actor.signing_key.sign(&sb).to_bytes().to_vec();
    ev
}

/// Build a signed kd=dc DraftCandidate event.
pub fn build_draft_candidate_event(
    actor: &TestIdentity,
    poll_id: harmony_app::community_voting_core::PollId,
    text: &str,
    hlc: Hlc,
) -> SignedVotingEvent {
    use ed25519_dalek::Signer;
    let payload_struct = DraftCandidatePayload {
        poll_id,
        text: text.into(),
    };
    let mut payload = Vec::new();
    ciborium::into_writer(&payload_struct, &mut payload).expect("encode DraftCandidatePayload");
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::DraftCandidate,
        hlc,
        actor: actor.owner,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev
        .signing_bytes()
        .expect("signing_bytes for draft candidate");
    ev.sig = actor.signing_key.sign(&sb).to_bytes().to_vec();
    ev
}

/// Build a signed kd=da DraftApproval event.
pub fn build_draft_approval_event(
    actor: &TestIdentity,
    poll_id: harmony_app::community_voting_core::PollId,
    candidate_event_hash: CandidateEventHash,
    hlc: Hlc,
) -> SignedVotingEvent {
    use ed25519_dalek::Signer;
    let payload_struct = DraftApprovalPayload {
        poll_id,
        candidate_event_hash,
    };
    let mut payload = Vec::new();
    ciborium::into_writer(&payload_struct, &mut payload).expect("encode DraftApprovalPayload");
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::DraftApproval,
        hlc,
        actor: actor.owner,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev
        .signing_bytes()
        .expect("signing_bytes for draft approval");
    ev.sig = actor.signing_key.sign(&sb).to_bytes().to_vec();
    ev
}

/// Build a signed kd=rb RatificationBallot event.
/// `scores` is one byte per ratification candidate (0..=5).
pub fn build_ratification_ballot_event(
    actor: &TestIdentity,
    poll_id: harmony_app::community_voting_core::PollId,
    scores: Vec<u8>,
    hlc: Hlc,
) -> SignedVotingEvent {
    use ed25519_dalek::Signer;
    let payload_struct = RatificationBallotPayload { poll_id, scores };
    let mut payload = Vec::new();
    ciborium::into_writer(&payload_struct, &mut payload).expect("encode RatificationBallotPayload");
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::RatificationBallot,
        hlc,
        actor: actor.owner,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev
        .signing_bytes()
        .expect("signing_bytes for ratification ballot");
    ev.sig = actor.signing_key.sign(&sb).to_bytes().to_vec();
    ev
}

/// Build a signed kd=sf SortitionFailed event.
pub fn build_sortition_failed_event(
    proposer: &TestIdentity,
    poll_id: harmony_app::community_voting_core::PollId,
    hlc: Hlc,
) -> SignedVotingEvent {
    use ed25519_dalek::Signer;
    let payload_struct = SortitionFailedPayload { poll_id };
    let mut payload = Vec::new();
    ciborium::into_writer(&payload_struct, &mut payload).expect("encode SortitionFailedPayload");
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::SortitionFailed,
        hlc,
        actor: proposer.owner,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev
        .signing_bytes()
        .expect("signing_bytes for sortition failed");
    ev.sig = proposer.signing_key.sign(&sb).to_bytes().to_vec();
    ev
}

/// Build a signed kd=ss SortitionSelection event (engine-generated shape:
/// zero-sig actor is `OwnerAddr([0; 16])`). Used in tests that inject a
/// pre-computed sortition result without a real DKG/beacon.
pub fn build_sortition_selection_event(
    poll_id: harmony_app::community_voting_core::PollId,
    primary: Vec<OwnerAddr>,
    backup: Vec<OwnerAddr>,
    hlc: Hlc,
) -> SignedVotingEvent {
    let payload_struct = SortitionSelectionPayload {
        poll_id,
        primary,
        backup,
    };
    let mut payload = Vec::new();
    ciborium::into_writer(&payload_struct, &mut payload).expect("encode SortitionSelectionPayload");
    // Engine-generated events use zero actor + zero sig (Task 19 wires real signing).
    // Per community_voting_log_engine.rs::publish_sortition_selection comments,
    // this is accepted for Phase 4a-main test fixtures.
    SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::SortitionSelection,
        hlc,
        actor: OwnerAddr([0u8; 16]),
        payload,
        sig: vec![0u8; 64],
    }
}

/// Build a signed kd=cl PollClose event (Tier 3).
pub fn build_poll_close_event(
    actor: &TestIdentity,
    poll_id: harmony_app::community_voting_core::PollId,
    hlc: Hlc,
) -> SignedVotingEvent {
    use ed25519_dalek::Signer;
    // PollClose for Tier 3 carries a minimal payload with just the poll_id reference.
    // The payload is a CBOR map with key "pi" → poll_id bytes.
    #[derive(serde::Serialize)]
    struct PollClosePayload {
        #[serde(rename = "pi")]
        pi: harmony_app::community_voting_core::PollId,
    }
    let payload_struct = PollClosePayload { pi: poll_id };
    let mut payload = Vec::new();
    ciborium::into_writer(&payload_struct, &mut payload).expect("encode PollClosePayload");
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::PollClose,
        hlc,
        actor: actor.owner,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().expect("signing_bytes for poll close");
    ev.sig = actor.signing_key.sign(&sb).to_bytes().to_vec();
    ev
}

/// Build a signed kd=rs PollResult event (Tier 3).
pub fn build_poll_result_event(
    actor: &TestIdentity,
    poll_id: harmony_app::community_voting_core::PollId,
    result: harmony_app::community_voting_star::StarResult,
    hlc: Hlc,
) -> SignedVotingEvent {
    use ed25519_dalek::Signer;
    let payload_struct = Tier3PollResultPayload { poll_id, result };
    let mut payload = Vec::new();
    ciborium::into_writer(&payload_struct, &mut payload).expect("encode Tier3PollResultPayload");
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::PollResult,
        hlc,
        actor: actor.owner,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().expect("signing_bytes for poll result");
    ev.sig = actor.signing_key.sign(&sb).to_bytes().to_vec();
    ev
}

// ─── SMOKE TESTS ───────────────────────────────────────────────────────────────

/// Verify that `setup_two_voting_engine_bridge` constructs + tears down
/// without panics. This is a fixture meta-test — if this fails, all
/// subsequent Task 13-16 tests are invalid.
#[tokio::test]
async fn smoke_two_engine_bridge_tier3_starts_and_shuts_down() {
    let community_id = SpaceId([0xaa; 16]);
    let engines = setup_two_voting_engine_bridge(community_id).await;
    // Drop explicitly so forwarder tasks see channel closure and exit.
    drop(engines);
    // No assertion — just no panic.
}

/// Sanity check: an existing Tier 1 PollCreate published via engine_a
/// crosses the bridge and is applied by engine_b. Confirms the bridge is
/// functional before Tier 3 tests build on it.
///
/// Note: VotingLogEngine inbound path is gated behind
/// `cfg(any(test, feature = "test-fixtures"))` in production, so this
/// only runs with `--features test-fixtures` (which the test harness
/// always passes — see CLAUDE.md).
#[tokio::test]
async fn smoke_tier3_bridge_tier1_event_crosses_to_peer() {
    let community_id = SpaceId([0xbb; 16]);
    let engines = setup_two_voting_engine_bridge(community_id).await;

    let alice = fixture_identity(0xA1);

    let tier1_cfg = Tier1PollConfig {
        options: vec!["Yes".into(), "No".into()],
        window_seconds: 3600,
        quorum: None,
        threshold_percent: None,
        multi_winner: None,
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        channel_id: ChannelId([0x11; 16]),
    };
    let hlc = hlc_at(1_000, "alice-dev");
    let event = build_signed_poll_create_tier1(&alice.signing_key, alice.owner, &tier1_cfg, hlc)
        .expect("build_signed_poll_create_tier1");

    engines
        .engine_a
        .publish_event(event)
        .await
        .expect("engine_a publish Tier1 PollCreate");

    // Engine A should have applied it locally.
    {
        let log = engines.log_a.lock().await;
        assert_eq!(
            log.polls.len(),
            1,
            "engine_a must have exactly one poll after publish"
        );
    }

    // Engine B must receive + apply via the bridge.
    wait_for_log("engine_b sees tier1 poll", &engines.log_b, |log| {
        log.polls.len() == 1
    })
    .await
    .expect("engine_b must apply the Tier1 PollCreate from engine_a");

    drop(engines);
}

/// Multi-engine 4-stage E2E happy path for Tier 3.
///
/// Design spec §12 AC #7: two voting engines must converge bit-identically on
/// sortition_result, drafting advancers, ratification candidate ordering, and
/// StarResult after driving the full PollCreate → kd=ss → Drafting → Ratification
/// → Close → Result lifecycle.
///
/// ## Beacon injection
///
/// Rather than running a real DKG ceremony, we inject a kd=ss SortitionSelection
/// event directly (zero-sig engine-generated shape) after computing the sortition
/// deterministically with a fixed VRF output `[0xAB; 32]`. This simulates exactly
/// what `VotingLogEngine::on_dfrost_beacon` would publish after receiving a matching
/// VRF beacon from DfrostLog.
///
/// ## HLC advancement
///
/// Windows are 60s (minimum valid). Events are assigned explicit HLC wall_ms values
/// that skip past each window boundary so we never wait real clock time.
///
/// ## sortition_size
///
/// Minimum valid per `validate_tier3_poll_config` is 20. We create 50 fixture
/// identities as the electorate (20 primary + 20 backup = 40 slots, 50 total).
#[tokio::test]
async fn tier3_full_lifecycle_4_stage_convergence() {
    const SORTITION_SIZE: u16 = 20;
    const N_IDENTITIES: usize = 50;
    const COMMUNITY_ID: SpaceId = SpaceId([0xE2; 16]);

    // ── Step 1: build electorate ──────────────────────────────────────────────

    let identities: Vec<TestIdentity> = (0..N_IDENTITIES as u8).map(fixture_identity).collect();

    // All identity owner addresses — used as the electorate snapshot at PollCreate.
    let electorate: Vec<OwnerAddr> = identities.iter().map(|id| id.owner).collect();

    // ── Step 2: set up two-engine bridge ──────────────────────────────────────

    let engines = setup_two_voting_engine_bridge(COMMUNITY_ID).await;

    // ── Step 3: PollCreate (proposer = identities[0]) ─────────────────────────

    let t0: u64 = 1_000_000; // base wall_ms
    let proposer = &identities[0];

    let config = Tier3PollConfigPayload {
        proposal_text: "E2E lifecycle test proposal".into(),
        sortition_size: SORTITION_SIZE,
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
    };

    let create_event = build_tier3_poll_create_event(proposer, &config, hlc_at(t0, "proposer-dev"));

    // Compute poll_id and poll_create_event_hash from the create event.
    let create_signing_bytes = create_event
        .signing_bytes()
        .expect("create event signing_bytes");
    let poll_id = derive_poll_id(&COMMUNITY_ID, &create_signing_bytes);
    let _poll_create_event_hash: [u8; 32] = {
        use sha2::{Digest, Sha256};
        Sha256::digest(&create_signing_bytes).into()
    };

    // Apply PollCreate via engine_a with the full electorate snapshot.
    let snapshot = MembershipSnapshot {
        members: electorate
            .iter()
            .map(|addr| {
                (
                    *addr,
                    MemberAttrs {
                        power: 1,
                        vouching_depth: 0,
                    },
                )
            })
            .collect(),
    };
    {
        let mut log = engines.log_a.lock().await;
        log.apply_with_snapshot(create_event.clone(), &COMMUNITY_ID, Some(snapshot))
            .expect("engine_a: PollCreate apply");
    }
    // Also feed to engine_b via the bridge by publishing via engine_a.
    // But since we applied directly to log_a, we need to broadcast manually.
    // Use the engine_a's publish_event path but that would double-apply to log_a.
    // Instead: encode and send directly to engine_b's inbound via the bridge.
    //
    // Actually: re-apply directly to log_b with the same snapshot.
    let snapshot_b = MembershipSnapshot {
        members: electorate
            .iter()
            .map(|addr| {
                (
                    *addr,
                    MemberAttrs {
                        power: 1,
                        vouching_depth: 0,
                    },
                )
            })
            .collect(),
    };
    {
        let mut log = engines.log_b.lock().await;
        log.apply_with_snapshot(create_event, &COMMUNITY_ID, Some(snapshot_b))
            .expect("engine_b: PollCreate apply");
    }

    // Verify both logs have the poll in Sortition stage.
    {
        let log_a = engines.log_a.lock().await;
        let t3 = log_a
            .polls
            .get(&poll_id)
            .expect("poll in log_a")
            .tier_state
            .as_tier3()
            .expect("tier3 state in log_a");
        assert_eq!(
            t3.stage,
            Stage::Sortition,
            "log_a: should start in Sortition"
        );
        assert!(t3.sortition_result.is_none(), "log_a: no sortition yet");
        assert_eq!(
            t3.eligible_electorate_snapshot.len(),
            N_IDENTITIES,
            "log_a: full electorate snapshotted"
        );
    }
    {
        let log_b = engines.log_b.lock().await;
        let t3 = log_b
            .polls
            .get(&poll_id)
            .expect("poll in log_b")
            .tier_state
            .as_tier3()
            .expect("tier3 state in log_b");
        assert_eq!(
            t3.stage,
            Stage::Sortition,
            "log_b: should start in Sortition"
        );
    }

    // ── Step 4: inject kd=ss via engine_a (beacon simulation) ────────────────
    //
    // Fixed VRF output. Mirrors exactly what VotingLogEngine::on_dfrost_beacon
    // would publish after receiving a matching VRfBeaconPayload with
    // vrf_output = [0xAB; 32].
    let vrf_output: [u8; 32] = [0xAB; 32];
    let sortition_result = fisher_yates_select(
        &vrf_output,
        &electorate,
        SORTITION_SIZE as usize,
        SORTITION_SIZE as usize,
    );

    let ss_event = build_sortition_selection_event(
        poll_id,
        sortition_result.primary.clone(),
        sortition_result.backup.clone(),
        hlc_at(t0 + 1, "engine"),
    );

    // Publish via engine_a — applies to log_a and broadcasts to engine_b via bridge.
    engines
        .engine_a
        .publish_event(ss_event)
        .await
        .expect("engine_a: publish kd=ss");

    // Wait for engine_b to apply the kd=ss (via bridge forwarder).
    wait_for_log("engine_b: sortition_result set", &engines.log_b, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.sortition_result.is_some())
            .unwrap_or(false)
    })
    .await
    .expect("engine_b: kd=ss must arrive via bridge within 5s");

    // Assert: both engines have identical sortition_result.
    let primary_a = {
        let log = engines.log_a.lock().await;
        let t3 = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        t3.sortition_result
            .clone()
            .expect("log_a: sortition_result Some")
    };
    let primary_b = {
        let log = engines.log_b.lock().await;
        let t3 = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        t3.sortition_result
            .clone()
            .expect("log_b: sortition_result Some")
    };
    assert_eq!(
        primary_a, primary_b,
        "CONVERGENCE: both engines must have identical sortition_result"
    );
    assert_eq!(primary_a.primary.len(), SORTITION_SIZE as usize);
    assert_eq!(primary_a.backup.len(), SORTITION_SIZE as usize);

    // ── Step 5: Drafting stage — mini-public publishes kd=dc + kd=da ─────────
    //
    // Advance HLC past deliberation_window (60s = 60_000ms).
    // Drafting opens at t0 + dw_ms.
    let t_drafting = t0 + 60_001; // past deliberation window

    // The actual mini-public is the primary set (no declines in this test).
    // Find which TestIdentity objects correspond to primary OwnerAddrs.
    let primary_set: std::collections::HashSet<OwnerAddr> =
        primary_a.primary.iter().copied().collect();
    let mini_public_ids: Vec<&TestIdentity> = identities
        .iter()
        .filter(|id| primary_set.contains(&id.owner))
        .collect();
    assert_eq!(
        mini_public_ids.len(),
        SORTITION_SIZE as usize,
        "must find all primary members in identity list"
    );

    // Build 3 draft candidates from the first 3 mini-public members.
    let dc_texts = ["Option A", "Option B", "Option C"];
    let mut dc_events: Vec<SignedVotingEvent> = Vec::new();
    for (i, text) in dc_texts.iter().enumerate() {
        let actor = mini_public_ids[i];
        let dc_ev = build_draft_candidate_event(
            actor,
            poll_id,
            text,
            hlc_at(t_drafting + i as u64 * 10, &format!("member-{i}-dev")),
        );
        engines
            .engine_a
            .publish_event(dc_ev.clone())
            .await
            .expect("engine_a: publish kd=dc");
        dc_events.push(dc_ev);
    }

    // Compute candidate_event_hash for each kd=dc event.
    let candidate_hashes: Vec<CandidateEventHash> = dc_events
        .iter()
        .map(|ev| {
            use sha2::{Digest, Sha256};
            Sha256::digest(ev.signing_bytes().expect("dc signing bytes")).into()
        })
        .collect();

    // All mini-public members approve each candidate (threshold = ceil(20/2) = 10).
    // We need at least 10 approvals per candidate to advance to ratification.
    // The proposer already has self-approval (1 approval each). Add 9 more per candidate.
    // Use mini_public_ids[1..10] for candidate 0, mini_public_ids[0]+[2..10] for candidate 1, etc.
    //
    // Simple approach: all 20 mini-public members approve all 3 candidates.
    for (c_idx, &candidate_hash) in candidate_hashes.iter().enumerate() {
        for (m_idx, actor) in mini_public_ids.iter().enumerate() {
            // Skip if this actor is the dc proposer (they already have self-approval,
            // but adding again is idempotent — HashSet.insert is safe).
            let da_ev = build_draft_approval_event(
                actor,
                poll_id,
                candidate_hash,
                hlc_at(
                    t_drafting + 100 + (c_idx * 100 + m_idx) as u64,
                    &format!("member-{m_idx}-dev"),
                ),
            );
            engines
                .engine_a
                .publish_event(da_ev)
                .await
                .expect("engine_a: publish kd=da");
        }
    }

    // Wait for engine_b to receive all drafting events.
    let expected_events_after_drafting = 1 // kd=cr
        + 1 // kd=ss
        + dc_texts.len() // kd=dc ×3
        + mini_public_ids.len() * dc_texts.len(); // kd=da ×(20*3=60)
    wait_for_log(
        "engine_b: all drafting events applied",
        &engines.log_b,
        |log| log.events.len() >= expected_events_after_drafting,
    )
    .await
    .expect("engine_b: must receive all drafting events within 5s");

    // ── Step 6: Ratification stage — full electorate casts kd=rb ──────────────
    //
    // Advance past drafting_window: t0 + dw + fw = t0 + 120_001ms.
    let t_ratification = t0 + 120_001;

    // Compute the ratification candidate ordering (same as both engines will see).
    let sq = synthesize_status_quo(&poll_id);
    let sq_hash = sq.event_hash;
    let mini_public_size = SORTITION_SIZE as usize; // 20 (no declines)
    let advancers = {
        // We need the candidates from log_a to compute advancers.
        let log = engines.log_a.lock().await;
        let t3 = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        // Build a candidates slice that includes status_quo (synthesized, not in state yet).
        let mut all_candidates = t3.candidates.clone();
        all_candidates.push(sq.clone());
        drafting_advancers(&all_candidates, mini_public_size, sq_hash)
    };
    assert_eq!(
        advancers.len(),
        4, // 3 real + 1 status_quo (all 3 pass threshold of 10 since all 20 approved them)
        "all 3 candidates + status_quo should advance"
    );
    let ordered_candidates = ratification_candidates_ordering(&advancers, sq_hash);
    let n_candidates = ordered_candidates.len();

    // All 50 electorate members cast ballots with simple scores.
    // Scores are indexed by position in ordered_candidates. Give candidate 0 the
    // highest score so it wins the STAR tally.
    let mut ballots: Vec<RatificationBallotPayload> = Vec::new();
    for (e_idx, actor) in identities.iter().enumerate() {
        // Score pattern: [5, 3, 2, 1] with slight variation for voter index.
        // Ensures a clear winner at position 0 without all ties.
        let scores: Vec<u8> = (0..n_candidates as u8)
            .map(|i| if i == 0 { 5 } else { 4u8.saturating_sub(i) })
            .collect();
        let rb_ev = build_ratification_ballot_event(
            actor,
            poll_id,
            scores.clone(),
            hlc_at(
                t_ratification + e_idx as u64 * 2,
                &format!("voter-{e_idx}-dev"),
            ),
        );
        ballots.push(RatificationBallotPayload { poll_id, scores });
        engines
            .engine_a
            .publish_event(rb_ev)
            .await
            .expect("engine_a: publish kd=rb");
    }

    // Wait for engine_b to see all ballots.
    let expected_events_after_ratification = expected_events_after_drafting + N_IDENTITIES;
    wait_for_log(
        "engine_b: all ratification ballots applied",
        &engines.log_b,
        |log| log.events.len() >= expected_events_after_ratification,
    )
    .await
    .expect("engine_b: must receive all kd=rb events within 5s");

    // ── Step 7: Close + Result ────────────────────────────────────────────────
    //
    // Advance past ratification_window: t0 + dw + fw + rw = t0 + 180_001ms.
    let t_close = t0 + 180_001;

    let close_ev = build_poll_close_event(proposer, poll_id, hlc_at(t_close, "proposer-dev"));
    engines
        .engine_a
        .publish_event(close_ev)
        .await
        .expect("engine_a: publish kd=cl");

    // Compute the STAR result deterministically (same computation as SR1 verify).
    let star_result = tally_star(&ordered_candidates, &ballots);

    let result_ev = build_poll_result_event(
        proposer,
        poll_id,
        star_result.clone(),
        hlc_at(t_close + 1, "proposer-dev"),
    );
    engines
        .engine_a
        .publish_event(result_ev)
        .await
        .expect("engine_a: publish kd=rs");

    // Wait for engine_b to see Close + Result.
    let expected_final_events = expected_events_after_ratification + 2;
    wait_for_log(
        "engine_b: PollClose + PollResult applied",
        &engines.log_b,
        |log| log.events.len() >= expected_final_events,
    )
    .await
    .expect("engine_b: must receive kd=cl + kd=rs within 5s");

    // ── Step 8: Assert full convergence ──────────────────────────────────────

    // Give any async tasks a moment to settle.
    tokio::time::sleep(Duration::from_millis(20)).await;

    let (t3_a, t3_b) = {
        let log_a = engines.log_a.lock().await;
        let log_b = engines.log_b.lock().await;
        let t3_a = log_a.polls[&poll_id]
            .tier_state
            .as_tier3()
            .expect("log_a tier3")
            .clone();
        let t3_b = log_b.polls[&poll_id]
            .tier_state
            .as_tier3()
            .expect("log_b tier3")
            .clone();
        (t3_a, t3_b)
    };

    // Both engines must be in Finalized stage.
    assert_eq!(
        t3_a.stage,
        Stage::Finalized,
        "engine_a: stage must be Finalized"
    );
    assert_eq!(
        t3_b.stage,
        Stage::Finalized,
        "engine_b: stage must be Finalized"
    );

    // Convergence: sortition_result identical.
    assert_eq!(
        t3_a.sortition_result, t3_b.sortition_result,
        "CONVERGENCE: sortition_result must be identical"
    );

    // Convergence: candidates set identical (count + hashes).
    assert_eq!(
        t3_a.candidates.len(),
        t3_b.candidates.len(),
        "CONVERGENCE: candidate count must be identical"
    );
    // Compare candidate event_hashes (order is insertion-order, same on both since events arrive in order).
    let hashes_a: Vec<_> = t3_a.candidates.iter().map(|c| c.event_hash).collect();
    let hashes_b: Vec<_> = t3_b.candidates.iter().map(|c| c.event_hash).collect();
    assert_eq!(
        hashes_a, hashes_b,
        "CONVERGENCE: candidate hashes must be identical"
    );

    // Convergence: ratification ballot count.
    assert_eq!(
        t3_a.ratification_ballots.len(),
        N_IDENTITIES,
        "engine_a: all {N_IDENTITIES} ballots applied"
    );
    assert_eq!(
        t3_b.ratification_ballots.len(),
        N_IDENTITIES,
        "engine_b: all {N_IDENTITIES} ballots applied"
    );

    // Convergence: StarResult identical.
    assert_eq!(
        t3_a.result, t3_b.result,
        "CONVERGENCE: StarResult must be identical on both engines"
    );

    // StarResult must match what we computed locally.
    let result_a = t3_a.result.as_ref().expect("engine_a: result Some");
    assert_eq!(
        result_a, &star_result,
        "engine_a: StarResult must match local tally_star computation"
    );

    // The winner should be the first ordered candidate (highest score = 5 × 50 voters).
    assert_eq!(
        result_a.winner.event_hash, ordered_candidates[0].event_hash,
        "STAR winner must be the highest-scored candidate"
    );

    // Log event count convergence.
    {
        let log_a = engines.log_a.lock().await;
        let log_b = engines.log_b.lock().await;
        assert_eq!(
            log_a.events.len(),
            log_b.events.len(),
            "CONVERGENCE: both logs must have identical event count"
        );
    }

    drop(engines);
}
#[tokio::test]
async fn smoke_tier3_event_builders_encode_without_panic() {
    let alice = fixture_identity(0xA2);
    let bob = fixture_identity(0xB2);

    let config = default_tier3_config();
    let poll_id = harmony_app::community_voting_core::PollId([0x42; 32]);

    // PollCreate
    let create_ev = build_tier3_poll_create_event(&alice, &config, hlc_at(1_000, "alice-dev"));
    assert_eq!(create_ev.tier, Tier::Sortition);
    assert_eq!(create_ev.kind, PollEventKindCode::PollCreate);
    assert_eq!(create_ev.actor, alice.owner);

    // MiniPublicDecline
    let decline_ev = build_decline_event(&bob, poll_id, hlc_at(2_000, "bob-dev"));
    assert_eq!(decline_ev.kind, PollEventKindCode::MiniPublicDecline);

    // DraftCandidate
    let draft_ev = build_draft_candidate_event(
        &bob,
        poll_id,
        "Proposal text here",
        hlc_at(3_000, "bob-dev"),
    );
    assert_eq!(draft_ev.kind, PollEventKindCode::DraftCandidate);

    // Derive candidate_event_hash from draft event signing bytes (SHA-256).
    let draft_signing_bytes = draft_ev.signing_bytes().expect("draft signing bytes");
    let candidate_event_hash: [u8; 32] = {
        use sha2::{Digest, Sha256};
        Sha256::digest(&draft_signing_bytes).into()
    };

    // DraftApproval
    let approval_ev = build_draft_approval_event(
        &alice,
        poll_id,
        candidate_event_hash,
        hlc_at(4_000, "alice-dev"),
    );
    assert_eq!(approval_ev.kind, PollEventKindCode::DraftApproval);

    // RatificationBallot
    let ballot_ev =
        build_ratification_ballot_event(&alice, poll_id, vec![5, 3, 1], hlc_at(5_000, "alice-dev"));
    assert_eq!(ballot_ev.kind, PollEventKindCode::RatificationBallot);

    // SortitionFailed
    let failed_ev = build_sortition_failed_event(&alice, poll_id, hlc_at(6_000, "alice-dev"));
    assert_eq!(failed_ev.kind, PollEventKindCode::SortitionFailed);

    // SortitionSelection (engine-generated, zero actor)
    let primary: Vec<OwnerAddr> = (0..20u8).map(|i| OwnerAddr([i; 16])).collect();
    let backup: Vec<OwnerAddr> = (20..40u8).map(|i| OwnerAddr([i; 16])).collect();
    let ss_ev = build_sortition_selection_event(poll_id, primary, backup, hlc_at(7_000, "engine"));
    assert_eq!(ss_ev.kind, PollEventKindCode::SortitionSelection);
    assert_eq!(ss_ev.actor, OwnerAddr([0u8; 16]));

    // Verify all events encode without panic (CBOR round-trip).
    for ev in &[
        &create_ev,
        &decline_ev,
        &draft_ev,
        &approval_ev,
        &ballot_ev,
        &failed_ev,
        &ss_ev,
    ] {
        let mut buf = Vec::new();
        ciborium::into_writer(ev, &mut buf).expect("CBOR encode must not panic");
        let decoded: SignedVotingEvent =
            ciborium::from_reader(&buf[..]).expect("CBOR decode must round-trip");
        assert_eq!(decoded.kind, ev.kind, "round-trip kind must match");
        assert_eq!(decoded.tier, ev.tier, "round-trip tier must match");
    }
}
