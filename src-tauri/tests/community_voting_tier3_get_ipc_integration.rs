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
    build_signed_draft_approval, build_signed_draft_candidate, build_signed_poll_close_tier3,
    build_signed_poll_create_tier1, build_signed_poll_create_tier3, build_signed_poll_result_tier3,
    build_signed_sortition_selection, derive_poll_id, Eligibility, MemberAttrs, MembershipSnapshot,
    Tier3PollConfigPayload,
};
use harmony_app::community_voting_log::VotingLog;
use harmony_app::community_voting_sortition::fisher_yates_select;
use harmony_app::community_voting_star::{CandidateRef, StarResult};
use harmony_app::community_voting_tier3::event_hash_of;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_app::{
    voting_get_tier3_poll_impl, voting_list_tier3_polls_impl, NodeState, Tier3MyRole, Tier3StageTag,
};
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

    /// NodeState with two Tier 3 polls in Sortition stage at different HLCs.
    /// The poll created later has a higher `poll_create_hlc_ms`.
    async fn with_two_polls_in_sortition_stage_at_different_hlcs() -> Self {
        let community_id = SpaceId([0xA4; 16]);
        let proposer = fixture_identity(4);

        // Poll A: created at HLC 1_000_000.
        let config_a = tier3_config();
        let hlc_a = hlc_at(1_000_000);
        let event_a =
            build_signed_poll_create_tier3(&proposer.signing_key, proposer.owner, &config_a, hlc_a)
                .expect("build_signed_poll_create_tier3 A");
        let signing_bytes_a = event_a.signing_bytes().expect("signing_bytes A");
        let poll_id_a = derive_poll_id(&community_id, &signing_bytes_a);

        // Poll B: same proposer, slightly different hlc so signing_bytes differ → distinct PollId.
        let hlc_b = hlc_at(2_000_000);
        let event_b =
            build_signed_poll_create_tier3(&proposer.signing_key, proposer.owner, &config_a, hlc_b)
                .expect("build_signed_poll_create_tier3 B");

        let snapshot = single_member_snapshot(proposer.owner);
        let mut log = VotingLog::new();
        log.apply_with_snapshot(event_a, &community_id, Some(snapshot.clone()))
            .expect("apply poll A");
        log.apply_with_snapshot(event_b, &community_id, Some(snapshot))
            .expect("apply poll B");

        let state = build_node_state_with_log(community_id, log, None).await;
        Tier3TestHarness {
            state,
            poll_id_hex: hex::encode(poll_id_a.0),
            community_id_hex: hex::encode(community_id.0),
        }
    }

    /// NodeState with one Tier 3 poll and one Tier 1 poll in the same community.
    async fn with_one_tier3_and_one_tier1_in_same_community() -> Self {
        let community_id = SpaceId([0xA5; 16]);
        let proposer = fixture_identity(5);

        // Tier 3 poll.
        let config = tier3_config();
        let hlc_t3 = hlc_at(1_000_000);
        let event_t3 =
            build_signed_poll_create_tier3(&proposer.signing_key, proposer.owner, &config, hlc_t3)
                .expect("build tier3 poll");
        let signing_bytes_t3 = event_t3.signing_bytes().expect("signing_bytes t3");
        let poll_id_t3 = derive_poll_id(&community_id, &signing_bytes_t3);

        // Tier 1 poll.
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
            channel_id: ChannelId([0x22; 16]),
        };
        let hlc_t1 = hlc_at(2_000_000);
        let event_t1 = build_signed_poll_create_tier1(
            &proposer.signing_key,
            proposer.owner,
            &tier1_cfg,
            hlc_t1,
        )
        .expect("build tier1 poll");

        let snapshot = single_member_snapshot(proposer.owner);
        let mut log = VotingLog::new();
        log.apply_with_snapshot(event_t3, &community_id, Some(snapshot.clone()))
            .expect("apply tier3");
        log.apply_with_snapshot(event_t1, &community_id, Some(snapshot))
            .expect("apply tier1");

        let state = build_node_state_with_log(community_id, log, None).await;
        Tier3TestHarness {
            state,
            poll_id_hex: hex::encode(poll_id_t3.0),
            community_id_hex: hex::encode(community_id.0),
        }
    }

    /// NodeState with a Tier 3 poll that has reached Stage::Finalized.
    ///
    /// Uses the minimum valid config (sortition_size=20, window=60s each).
    /// With sortition_size=20, threshold = ceil(20/2) = 10 approvals.
    /// Sequence:
    ///   kd=cr → kd=ss (20 primary) → kd=dc by primary[0] (self-approval=1)
    ///   → kd=da by primary[1..9] (9 more → 10 total, hits threshold)
    ///   → kd=cl → kd=rs (poll result, winner = dc candidate)
    ///
    /// apply_event does NOT verify tally (verify_sr is an upstream gate);
    /// we construct a StarResult whose winner.event_hash matches the real
    /// kd=dc candidate so that winner_text resolves to "Winner proposal text".
    async fn with_finalized_tier3_poll() -> Self {
        const SORTITION_SIZE: u16 = 20;
        let community_id = SpaceId([0xA6; 16]);
        let proposer = fixture_identity(6);
        // primary[0..20] are identities 30..49; proposer (seed=6) is outside.
        let primary: Vec<_> = (30u8..50).map(fixture_identity).collect();

        let config = Tier3PollConfigPayload {
            proposal_text: "Finalized proposal text".into(),
            sortition_size: SORTITION_SIZE,
            deliberation_window_seconds: 60, // minimum valid
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
        let hlc_create = hlc_at(1_000_000);
        let event_create = build_signed_poll_create_tier3(
            &proposer.signing_key,
            proposer.owner,
            &config,
            hlc_create,
        )
        .expect("build tier3 poll");
        let signing_bytes = event_create.signing_bytes().expect("signing_bytes");
        let poll_id = derive_poll_id(&community_id, &signing_bytes);

        let snapshot = single_member_snapshot(proposer.owner);
        let mut log = VotingLog::new();
        log.apply_with_snapshot(event_create, &community_id, Some(snapshot))
            .expect("apply kd=cr");

        // kd=ss: 20 primary (primary identities), 0 backup.
        let primary_owners: Vec<OwnerAddr> = primary.iter().map(|id| id.owner).collect();
        let ss_event = build_signed_sortition_selection(
            &proposer.signing_key,
            proposer.owner,
            poll_id,
            primary_owners,
            vec![],
            hlc_at(1_000_001),
        )
        .expect("build kd=ss");
        log.apply_with_snapshot(ss_event, &community_id, None)
            .expect("apply kd=ss");

        // kd=dc: draft candidate from primary[0]; gets implicit self-approval (count=1).
        let dc_actor = &primary[0];
        let dc_event = build_signed_draft_candidate(
            &dc_actor.signing_key,
            dc_actor.owner,
            poll_id,
            "Winner proposal text".into(),
            hlc_at(1_000_002),
        )
        .expect("build kd=dc");
        let dc_hash: [u8; 32] = event_hash_of(&dc_event);
        log.apply_with_snapshot(dc_event, &community_id, None)
            .expect("apply kd=dc");

        // kd=da: 9 more approvals from primary[1..10] → total 10, hits threshold=ceil(20/2)=10.
        // apply_event for kd=da is a HashSet insert — no threshold validation at materialize layer.
        for (i, approver) in primary[1..10].iter().enumerate() {
            let da_event = build_signed_draft_approval(
                &approver.signing_key,
                approver.owner,
                poll_id,
                dc_hash,
                hlc_at(1_000_003 + i as u64),
            )
            .expect("build kd=da");
            log.apply_with_snapshot(da_event, &community_id, None)
                .expect("apply kd=da");
        }

        // kd=cl: poll close.
        let cl_event = build_signed_poll_close_tier3(
            &proposer.signing_key,
            proposer.owner,
            poll_id,
            hlc_at(1_001_000),
        )
        .expect("build kd=cl");
        log.apply_with_snapshot(cl_event, &community_id, None)
            .expect("apply kd=cl");

        // kd=rs: poll result. Construct StarResult with dc_hash as winner.
        // apply_event does NOT verify tally (verify_sr is an upstream gate);
        // we only need winner.event_hash to match a real t3.candidates entry
        // so that winner_text resolves to "Winner proposal text".
        let star_result = StarResult {
            winner: CandidateRef {
                event_hash: dc_hash,
                approval_count: 10,
            },
            finalists: vec![CandidateRef {
                event_hash: dc_hash,
                approval_count: 10,
            }],
            total_scores: vec![0],
            runoff_votes: vec![1],
        };
        let rs_event = build_signed_poll_result_tier3(
            &proposer.signing_key,
            proposer.owner,
            poll_id,
            star_result,
            hlc_at(1_001_001),
        )
        .expect("build kd=rs");
        log.apply_with_snapshot(rs_event, &community_id, None)
            .expect("apply kd=rs");

        let state = build_node_state_with_log(community_id, log, None).await;
        Tier3TestHarness {
            state,
            poll_id_hex: hex::encode(poll_id.0),
            community_id_hex: hex::encode(community_id.0),
        }
    }

    /// R2 follow-up: Finalized harness with TWO finalists so the export's
    /// `runner_up_event_hash` derivation path (find first finalist whose
    /// hash differs from `winner`) is exercised end-to-end. Without this,
    /// a regression that returns the winner as its own runner-up would
    /// go undetected.
    async fn with_finalized_tier3_poll_two_finalists() -> Self {
        const SORTITION_SIZE: u16 = 20;
        let community_id = SpaceId([0xC4; 16]);
        let proposer = fixture_identity(7);
        let primary: Vec<_> = (50u8..70).map(fixture_identity).collect();

        let config = Tier3PollConfigPayload {
            proposal_text: "Two-finalist proposal text".into(),
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
        let hlc_create = hlc_at(2_000_000);
        let event_create = build_signed_poll_create_tier3(
            &proposer.signing_key,
            proposer.owner,
            &config,
            hlc_create,
        )
        .expect("build tier3 poll");
        let signing_bytes = event_create.signing_bytes().expect("signing_bytes");
        let poll_id = derive_poll_id(&community_id, &signing_bytes);

        let snapshot = single_member_snapshot(proposer.owner);
        let mut log = VotingLog::new();
        log.apply_with_snapshot(event_create, &community_id, Some(snapshot))
            .expect("apply kd=cr");

        let primary_owners: Vec<OwnerAddr> = primary.iter().map(|id| id.owner).collect();
        let ss_event = build_signed_sortition_selection(
            &proposer.signing_key,
            proposer.owner,
            poll_id,
            primary_owners,
            vec![],
            hlc_at(2_000_001),
        )
        .expect("build kd=ss");
        log.apply_with_snapshot(ss_event, &community_id, None)
            .expect("apply kd=ss");

        // Two kd=dc draft candidates, each authored by a distinct primary
        // member so their event_hashes differ. Implicit self-approval gives
        // each candidate count=1; we boost both above the threshold=10 with
        // kd=da from primaries[2..11].
        let dc_a_actor = &primary[0];
        let dc_a_event = build_signed_draft_candidate(
            &dc_a_actor.signing_key,
            dc_a_actor.owner,
            poll_id,
            "Winner candidate text".into(),
            hlc_at(2_000_002),
        )
        .expect("build kd=dc A");
        let dc_a_hash: [u8; 32] = event_hash_of(&dc_a_event);
        log.apply_with_snapshot(dc_a_event, &community_id, None)
            .expect("apply kd=dc A");

        let dc_b_actor = &primary[1];
        let dc_b_event = build_signed_draft_candidate(
            &dc_b_actor.signing_key,
            dc_b_actor.owner,
            poll_id,
            "Runner-up candidate text".into(),
            hlc_at(2_000_003),
        )
        .expect("build kd=dc B");
        let dc_b_hash: [u8; 32] = event_hash_of(&dc_b_event);
        log.apply_with_snapshot(dc_b_event, &community_id, None)
            .expect("apply kd=dc B");
        assert_ne!(
            dc_a_hash, dc_b_hash,
            "harness invariant: two distinct candidates must have distinct hashes"
        );

        // 9 approvals each from primaries [2..11] for A and [11..20] for B
        // (clear of A's slot). Materialize is a HashSet insert; threshold
        // validation lives at verify, which we bypass here.
        for (i, approver) in primary[2..11].iter().enumerate() {
            let da_event = build_signed_draft_approval(
                &approver.signing_key,
                approver.owner,
                poll_id,
                dc_a_hash,
                hlc_at(2_000_004 + i as u64),
            )
            .expect("build kd=da A");
            log.apply_with_snapshot(da_event, &community_id, None)
                .expect("apply kd=da A");
        }
        for (i, approver) in primary[11..20].iter().enumerate() {
            let da_event = build_signed_draft_approval(
                &approver.signing_key,
                approver.owner,
                poll_id,
                dc_b_hash,
                hlc_at(2_000_020 + i as u64),
            )
            .expect("build kd=da B");
            log.apply_with_snapshot(da_event, &community_id, None)
                .expect("apply kd=da B");
        }

        let cl_event = build_signed_poll_close_tier3(
            &proposer.signing_key,
            proposer.owner,
            poll_id,
            hlc_at(2_001_000),
        )
        .expect("build kd=cl");
        log.apply_with_snapshot(cl_event, &community_id, None)
            .expect("apply kd=cl");

        // kd=rs: winner = A, runner-up = B. Both finalists present.
        let star_result = StarResult {
            winner: CandidateRef {
                event_hash: dc_a_hash,
                approval_count: 10,
            },
            finalists: vec![
                CandidateRef {
                    event_hash: dc_a_hash,
                    approval_count: 10,
                },
                CandidateRef {
                    event_hash: dc_b_hash,
                    approval_count: 10,
                },
            ],
            total_scores: vec![50, 30],
            runoff_votes: vec![12, 8],
        };
        let rs_event = build_signed_poll_result_tier3(
            &proposer.signing_key,
            proposer.owner,
            poll_id,
            star_result,
            hlc_at(2_001_001),
        )
        .expect("build kd=rs");
        log.apply_with_snapshot(rs_event, &community_id, None)
            .expect("apply kd=rs");

        let state = build_node_state_with_log(community_id, log, None).await;
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

    /// Call `voting_list_tier3_polls_impl` — the decoupled inner implementation.
    async fn list_tier3_polls(
        &self,
        community_id_hex: &str,
    ) -> Result<Vec<harmony_app::Tier3PollSummary>, String> {
        voting_list_tier3_polls_impl(&self.state, community_id_hex.to_string()).await
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

// ─── list_tier3_polls tests ───────────────────────────────────────────────────

#[tokio::test]
async fn list_tier3_polls_returns_polls_ordered_newest_first() {
    let h = Tier3TestHarness::with_two_polls_in_sortition_stage_at_different_hlcs().await;
    let summaries = h.list_tier3_polls(&h.community_id_hex).await.expect("ok");
    assert_eq!(summaries.len(), 2, "expected 2 Tier 3 polls");
    assert!(
        summaries[0].poll_create_hlc_ms >= summaries[1].poll_create_hlc_ms,
        "polls must be ordered newest first: [{}, {}]",
        summaries[0].poll_create_hlc_ms,
        summaries[1].poll_create_hlc_ms,
    );
}

#[tokio::test]
async fn list_tier3_polls_excludes_tier1_and_tier2_polls() {
    let h = Tier3TestHarness::with_one_tier3_and_one_tier1_in_same_community().await;
    let summaries = h.list_tier3_polls(&h.community_id_hex).await.expect("ok");
    assert_eq!(
        summaries.len(),
        1,
        "list must exclude the Tier 1 poll; got {summaries:?}"
    );
}

#[tokio::test]
async fn list_tier3_polls_includes_finalized_with_winner_text() {
    let h = Tier3TestHarness::with_finalized_tier3_poll().await;
    let summaries = h.list_tier3_polls(&h.community_id_hex).await.expect("ok");
    assert_eq!(summaries.len(), 1, "expected exactly 1 finalized poll");
    assert_eq!(
        summaries[0].stage,
        Tier3StageTag::Finalized,
        "stage must be Finalized"
    );
    assert!(
        summaries[0].winner_text.is_some(),
        "winner_text must be set for a Finalized poll; got {:?}",
        summaries[0].winner_text
    );
}

#[tokio::test]
async fn get_tier3_poll_two_finalist_finalized_exposes_runner_up() {
    // R2 follow-up to Greptile: the single-finalist `with_finalized_tier3_poll`
    // never exercises the `runner_up_event_hash` derivation. This harness
    // builds a 2-finalist StarResult so the export must report runner_up.
    let h = Tier3TestHarness::with_finalized_tier3_poll_two_finalists().await;
    let export = h.get_tier3_poll(&h.poll_id_hex).await.expect("ok");
    assert_eq!(export.stage, Tier3StageTag::Finalized);
    let winner = export
        .winner_event_hash
        .as_ref()
        .expect("winner_event_hash must be set");
    let runner_up = export
        .runner_up_event_hash
        .as_ref()
        .expect("runner_up_event_hash must be set when finalists.len() >= 2");
    assert_ne!(
        winner, runner_up,
        "runner_up must differ from winner: winner={winner}, runner_up={runner_up}"
    );
    // ratification_candidates exposes both finalists with their text.
    assert_eq!(
        export.ratification_candidates.len(),
        2,
        "finalists projection must include both candidates"
    );
    let texts: Vec<&str> = export
        .ratification_candidates
        .iter()
        .map(|c| c.text.as_str())
        .collect();
    assert!(texts.contains(&"Winner candidate text"));
    assert!(texts.contains(&"Runner-up candidate text"));
}

#[tokio::test]
async fn list_tier3_polls_returns_empty_for_unknown_community() {
    let h = Tier3TestHarness::empty().await;
    let summaries = h
        .list_tier3_polls(&"00".repeat(16))
        .await
        .expect("ok — unknown community returns empty vec, not error");
    assert!(
        summaries.is_empty(),
        "unknown community must return empty vec"
    );
}
