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
    build_signed_poll_create_tier1, CandidateEventHash, DraftApprovalPayload,
    DraftCandidatePayload, Eligibility, MiniPublicDeclinePayload, PollEventKindCode,
    RatificationBallotPayload, SignedVotingEvent, SortitionFailedPayload,
    SortitionSelectionPayload, Tier, Tier3PollConfigPayload,
};
use harmony_app::community_voting_log::VotingLog;
use harmony_app::community_voting_log_engine::{VotingLogEngine, VotingLogEngineParams};
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

/// Confirm the event builders produce valid CBOR and round-trip without
/// panics. This is a compile + encode sanity check — does NOT apply to
/// a VotingLog (that requires proper lifecycle context which is Task 13's
/// job).
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
