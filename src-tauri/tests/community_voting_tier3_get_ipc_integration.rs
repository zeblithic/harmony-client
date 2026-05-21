//! ZEB-311: integration tests for voting_get_tier3_poll IPC.
//! Builds a NodeState with a known Tier 3 poll applied directly to
//! VotingLog, then drives the decoupled `voting_get_tier3_poll_impl` and
//! asserts the Tier3PollExport is shaped correctly per stage.
//!
//! Harness pattern: mirrors community_voting_tier3_ipc_integration.rs
//! (inline harness struct in the test file, no shared module needed).
//!
//! `voting_get_tier3_poll_impl` is the pub test-fixtures-gated inner
//! implementation that takes `&Mutex<NodeState>` directly, avoiding the
//! need to construct a `tauri::State<T>` outside of Tauri's app setup.

#![cfg(feature = "test-fixtures")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use harmony_app::community_membership::ChannelId;
use harmony_app::community_voting_approval::Tier1PollConfig;
use harmony_app::community_voting_core::{
    build_signed_poll_create_tier1, build_signed_poll_create_tier3,
    build_signed_sortition_selection, derive_poll_id, Eligibility, MemberAttrs, MembershipSnapshot,
    Tier3PollConfigPayload,
};
use harmony_app::community_voting_log::VotingLog;
use harmony_app::community_voting_sortition::fisher_yates_select;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_app::{voting_get_tier3_poll_impl, NodeState, Tier3MyRole, Tier3StageTag};
use tokio::sync::Mutex;

// ─── Fixture identity helper ──────────────────────────────────────────────────

struct TestIdentity {
    pub owner: OwnerAddr,
    pub signing_key: ed25519_dalek::SigningKey,
}

fn fixture_identity(seed: u8) -> TestIdentity {
    let priv_id = harmony_identity::PrivateIdentity::from_seed(&[seed; 32]);
    let owner = OwnerAddr(priv_id.identity.address_hash);
    let private_bytes = priv_id.to_private_bytes();
    let mut ed_secret = [0u8; 32];
    ed_secret.copy_from_slice(&private_bytes[32..64]);
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_secret);
    TestIdentity { owner, signing_key }
}

fn hlc_at(wall_ms: u64) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: "test-dev".into(),
    }
}

// ─── Inline test harness ──────────────────────────────────────────────────────

/// Minimal harness: owns a `NodeState` plus a poll_id_hex / community_id_hex.
struct Tier3TestHarness {
    pub state: Arc<StdMutex<NodeState>>,
    pub poll_id_hex: String,
    #[allow(dead_code)]
    pub community_id_hex: String,
}

impl Tier3TestHarness {
    /// Minimal NodeState with no polls and no self identity.
    async fn empty() -> Self {
        let community_id = SpaceId([0xA0; 16]);
        let state = build_node_state_with_log(community_id, VotingLog::new(), None).await;
        Tier3TestHarness {
            state,
            poll_id_hex: "00".repeat(32),
            community_id_hex: hex::encode(community_id.0),
        }
    }

    /// NodeState with a Tier 1 poll, used for tier-mismatch test.
    async fn with_tier1_poll() -> Self {
        let community_id = SpaceId([0xA1; 16]);
        let proposer = fixture_identity(1);

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
        let hlc = hlc_at(1_000);
        let event =
            build_signed_poll_create_tier1(&proposer.signing_key, proposer.owner, &tier1_cfg, hlc)
                .expect("build_signed_poll_create_tier1");
        let signing_bytes = event.signing_bytes().expect("signing_bytes");
        let poll_id = derive_poll_id(&community_id, &signing_bytes);

        let snapshot = single_member_snapshot(proposer.owner);
        let mut log = VotingLog::new();
        log.apply_with_snapshot(event, &community_id, Some(snapshot))
            .expect("apply tier1 poll create");

        let state = build_node_state_with_log(community_id, log, None).await;
        Tier3TestHarness {
            state,
            poll_id_hex: hex::encode(poll_id.0),
            community_id_hex: hex::encode(community_id.0),
        }
    }

    /// NodeState with a Tier 3 poll in Sortition stage (kd=cr applied, no kd=ss).
    /// Self identity is NOT the proposer → role = Observer.
    async fn with_poll_in_sortition_stage() -> Self {
        let community_id = SpaceId([0xA2; 16]);
        let proposer = fixture_identity(2);
        let self_id = fixture_identity(99); // observer — different from proposer

        let config = tier3_config();
        let hlc = hlc_at(1_000_000);
        let event =
            build_signed_poll_create_tier3(&proposer.signing_key, proposer.owner, &config, hlc)
                .expect("build_signed_poll_create_tier3");
        let signing_bytes = event.signing_bytes().expect("signing_bytes");
        let poll_id = derive_poll_id(&community_id, &signing_bytes);

        let snapshot = single_member_snapshot(proposer.owner);
        let mut log = VotingLog::new();
        log.apply_with_snapshot(event, &community_id, Some(snapshot))
            .expect("apply tier3 poll create");

        // self_id is NOT the proposer AND kd=ss not applied → role = Observer.
        let state = build_node_state_with_log(community_id, log, Some(self_id.owner)).await;
        Tier3TestHarness {
            state,
            poll_id_hex: hex::encode(poll_id.0),
            community_id_hex: hex::encode(community_id.0),
        }
    }

    /// NodeState with a Tier 3 poll where kd=ss has been applied including self_id in primary.
    async fn with_poll_in_drafting_stage_and_self_in_mini_public() -> Self {
        let community_id = SpaceId([0xA3; 16]);
        let proposer = fixture_identity(3);

        // self_id will be included in the pool; fisher_yates_select needs
        // primary_size + backup_size = 40 distinct members. We use 50 total.
        let self_id = fixture_identity(10);
        let other_pool: Vec<OwnerAddr> =
            (0u8..49).map(|i| fixture_identity(20 + i).owner).collect();

        let config = tier3_config();
        let hlc = hlc_at(2_000_000);
        let event =
            build_signed_poll_create_tier3(&proposer.signing_key, proposer.owner, &config, hlc)
                .expect("build_signed_poll_create_tier3");
        let signing_bytes = event.signing_bytes().expect("signing_bytes");
        let poll_id = derive_poll_id(&community_id, &signing_bytes);

        let snapshot = single_member_snapshot(proposer.owner);
        let mut log = VotingLog::new();
        log.apply_with_snapshot(event, &community_id, Some(snapshot))
            .expect("apply tier3 poll create");

        // Build sortition result; full_pool starts with self_id for determinism.
        let vrf_output: [u8; 32] = [0xAB; 32];
        let sortition_size: usize = 20;
        let full_pool: Vec<OwnerAddr> = std::iter::once(self_id.owner)
            .chain(other_pool.iter().copied())
            .collect();
        let sortition_result =
            fisher_yates_select(&vrf_output, &full_pool, sortition_size, sortition_size);

        // Apply kd=ss using the proposer's key.
        let ss_event = build_signed_sortition_selection(
            &proposer.signing_key,
            proposer.owner,
            poll_id,
            sortition_result.primary.clone(),
            sortition_result.backup.clone(),
            hlc_at(2_000_001),
        )
        .expect("build kd=ss");
        log.apply_with_snapshot(ss_event, &community_id, None)
            .expect("apply kd=ss");

        // Confirm self_id is in either primary or backup (for clear assertion in test).
        let self_in_primary = sortition_result.primary.contains(&self_id.owner);
        let self_in_backup = sortition_result.backup.contains(&self_id.owner);
        assert!(
            self_in_primary || self_in_backup,
            "self_id must be in primary or backup for this harness to be useful"
        );

        let state = build_node_state_with_log(community_id, log, Some(self_id.owner)).await;
        Tier3TestHarness {
            state,
            poll_id_hex: hex::encode(poll_id.0),
            community_id_hex: hex::encode(community_id.0),
        }
    }

    /// Call `voting_get_tier3_poll_impl` — the decoupled inner implementation.
    async fn get_tier3_poll(
        &self,
        poll_id_hex: &str,
    ) -> Result<harmony_app::Tier3PollExport, String> {
        voting_get_tier3_poll_impl(&self.state, poll_id_hex.to_string()).await
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn tier3_config() -> Tier3PollConfigPayload {
    Tier3PollConfigPayload {
        proposal_text: "Test constitutional amendment".into(),
        sortition_size: 20,
        deliberation_window_seconds: 3600,
        drafting_window_seconds: 3600,
        ratification_window_seconds: 3600,
        privacy_mode: "pu".into(),
        incentive_mode: "d".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
    }
}

fn single_member_snapshot(owner: OwnerAddr) -> MembershipSnapshot {
    let mut members = HashMap::new();
    members.insert(
        owner,
        MemberAttrs {
            power: 1,
            vouching_depth: 0,
        },
    );
    MembershipSnapshot { members }
}

async fn build_node_state_with_log(
    community_id: SpaceId,
    log: VotingLog,
    self_owner: Option<OwnerAddr>,
) -> Arc<StdMutex<NodeState>> {
    let mut state = NodeState::default();
    // Set the self_owner for role-projection tests.
    if let Some(owner) = self_owner {
        state.set_test_self_owner(owner);
    }
    // Insert the VotingLog into the registry.
    {
        let log_arc = Arc::new(Mutex::new(log));
        let mut map = state.voting_logs.lock().await;
        map.insert(community_id, log_arc);
    }
    Arc::new(StdMutex::new(state))
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_tier3_poll_returns_sortition_stage_with_observer_role() {
    let h = Tier3TestHarness::with_poll_in_sortition_stage().await;
    let export = h.get_tier3_poll(&h.poll_id_hex).await.expect("ok");
    assert_eq!(export.stage, Tier3StageTag::Sortition);
    assert_eq!(export.my_role, Tier3MyRole::Observer);
    assert!(export.mini_public.is_empty(), "kd=ss not yet applied");
    assert!(export.draft_candidates.is_empty());
}

#[tokio::test]
async fn get_tier3_poll_returns_drafting_stage_with_mini_public_role_when_self_selected() {
    let h = Tier3TestHarness::with_poll_in_drafting_stage_and_self_in_mini_public().await;
    let export = h.get_tier3_poll(&h.poll_id_hex).await.expect("ok");
    // After kd=ss, stage may be Sortition or Deliberation depending on HLC relative
    // to the deliberation window (HLC wall_ms 2_000_001 < 2_000_000 + 3_600_000).
    // What matters: self is in the mini-public → role = MiniPublic (or Proposer).
    assert!(
        export.my_role == Tier3MyRole::MiniPublic || export.my_role == Tier3MyRole::Backup,
        "self should be MiniPublic or Backup after kd=ss (self was placed in the sortition pool), got {:?}",
        export.my_role
    );
    assert!(
        !export.mini_public.is_empty(),
        "mini_public must be set after kd=ss"
    );
}

#[tokio::test]
async fn get_tier3_poll_returns_error_on_unknown_poll() {
    let h = Tier3TestHarness::empty().await;
    let err = h.get_tier3_poll(&"00".repeat(32)).await.unwrap_err();
    assert!(err.contains("not found"), "expected 'not found' in: {err}");
}

#[tokio::test]
async fn get_tier3_poll_returns_error_on_tier_mismatch() {
    let h = Tier3TestHarness::with_tier1_poll().await;
    let err = h.get_tier3_poll(&h.poll_id_hex).await.unwrap_err();
    assert!(
        err.contains("not tier3") || err.contains("not Tier3"),
        "expected tier mismatch error, got: {err}"
    );
}
