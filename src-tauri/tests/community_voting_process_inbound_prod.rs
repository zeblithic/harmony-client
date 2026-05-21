//! ZEB-298+ZEB-312 PR 1: production-build verification that the inbound
//! voting feature-gate is gone.
//!
//! This file is NOT gated by `cfg(any(test, feature = "test-fixtures"))`
//! at the top level — it must compile + pass under a vanilla
//! `cargo nextest run --locked --test community_voting_process_inbound_prod`
//! invocation that does NOT pass `--features test-fixtures`. That is the
//! load-bearing assertion that the gate was removed for production builds.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use tokio::sync::Mutex;

use harmony_app::community_membership::ChannelId;
use harmony_app::community_voting_approval::Tier1PollConfig;
use harmony_app::community_voting_conviction::{AutoExecAction, SignalPayload, Tier2PollConfig};
use harmony_app::community_voting_core::{
    build_signed_poll_create_tier1, derive_poll_id, Eligibility, MemberAttrs, MembershipSnapshot,
    PollEventKindCode, SignedVotingEvent, Tier, VotingIdentityResolver,
};
use harmony_app::community_voting_log::{
    MembershipSnapshotResolver, SnapshotResolverError, VotingLog,
};
use harmony_app::community_voting_log_engine::{process_inbound_for_test, VotingReplayTracker};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

// ── Fixed resolver pair for this test ────────────────────────────────────────

struct FixedResolvers {
    identity: HashMap<OwnerAddr, ed25519_dalek::VerifyingKey>,
    snapshot: MembershipSnapshot,
}

#[async_trait]
impl VotingIdentityResolver for FixedResolvers {
    async fn verifying_key_for(&self, owner: &OwnerAddr) -> Option<ed25519_dalek::VerifyingKey> {
        self.identity.get(owner).copied()
    }
}

#[async_trait]
impl MembershipSnapshotResolver for FixedResolvers {
    async fn snapshot_at(
        &self,
        _community_id: SpaceId,
        _hlc: &Hlc,
    ) -> Result<MembershipSnapshot, SnapshotResolverError> {
        Ok(self.snapshot.clone())
    }
}

// ── Test ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn process_inbound_peer_apply_succeeds_in_production_build() {
    // Build a peer PollCreate event signed by `keypair` for `actor`.
    let keypair = SigningKey::generate(&mut OsRng);
    let actor = OwnerAddr([0xaa; 16]);
    let community_id = SpaceId([0xcc; 16]);
    let channel_id = ChannelId([0xbb; 16]);

    let cfg = Tier1PollConfig {
        options: vec!["a".into(), "b".into()],
        window_seconds: 600,
        quorum: None,
        threshold_percent: None,
        multi_winner: None,
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        channel_id,
    };
    let event_hlc = Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 0,
        device_id: "peer".into(),
    };
    let event =
        build_signed_poll_create_tier1(&keypair, actor, &cfg, event_hlc).expect("build event");

    let mut packet = Vec::new();
    ciborium::ser::into_writer(&event, &mut packet).expect("encode");

    let mut members = HashMap::new();
    members.insert(
        actor,
        MemberAttrs {
            power: 1,
            vouching_depth: 1,
        },
    );
    let resolvers = Arc::new(FixedResolvers {
        identity: HashMap::from([(actor, keypair.verifying_key())]),
        snapshot: MembershipSnapshot { members },
    });

    let voting_log = Arc::new(Mutex::new(VotingLog::default()));
    let tracker = Arc::new(Mutex::new(VotingReplayTracker::new()));

    let id_resolver: Arc<dyn VotingIdentityResolver> = resolvers.clone();
    let mem_resolver: Arc<dyn MembershipSnapshotResolver> = resolvers.clone();

    let result = process_inbound_for_test(
        community_id,
        &voting_log,
        &tracker,
        Some(&id_resolver),
        Some(&mem_resolver),
        &packet,
    )
    .await;

    assert!(
        result.is_ok(),
        "process_inbound must succeed in production build; got: {result:?}"
    );

    // Verify the event was applied to the log.
    let signing_bytes = event.signing_bytes().expect("signing_bytes");
    let pid = derive_poll_id(&community_id, &signing_bytes);
    let log = voting_log.lock().await;
    assert!(
        log.has_poll(&pid),
        "poll must be applied after process_inbound returns Ok"
    );
}

// ── Tier 2 eligibility regression test ───────────────────────────────────────

/// ZEB-298+ZEB-312 PR 1 regression: Tier 2 Signal from an ineligible peer must
/// be rejected before apply even if the signature is valid and the peer is a
/// community member (just below min_power). Without the eligibility guard,
/// ineligible signals would reach `apply_with_snapshot` and silently influence
/// threshold math.
#[tokio::test]
async fn process_inbound_tier2_signal_from_ineligible_actor_is_rejected() {
    use ed25519_dalek::Signer as _;

    let community_id = SpaceId([0xcc; 16]);

    // Creator: power=5 — satisfies eligibility.min_power=5
    let creator_keypair = SigningKey::generate(&mut OsRng);
    let creator = OwnerAddr([0xaa; 16]);

    // Signaler: power=0 — below min_power=5
    let signaler_keypair = SigningKey::generate(&mut OsRng);
    let signaler = OwnerAddr([0xbb; 16]);

    // Tier 2 config with min_power=5
    let cfg = Tier2PollConfig {
        proposal_text: "Eligibility regression test".into(),
        half_life_seconds: 604_800,
        threshold_min_q32: 10 * harmony_app::community_voting_conviction::Q32,
        threshold_max_q32: 1_000 * harmony_app::community_voting_conviction::Q32,
        beta: 2,
        delegation_allowed: false,
        auto_exec: AutoExecAction::SetPower {
            target_pubkey: OwnerAddr([0x42; 16]),
            new_power: 10,
        },
        eligibility: Eligibility {
            min_power: 5,
            min_vouching_depth: None,
            sortition_size: None,
        },
    };

    // Encode Tier2PollConfig as payload.
    let mut create_payload = Vec::new();
    ciborium::into_writer(&cfg, &mut create_payload).expect("encode cfg");

    let create_hlc = Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 0,
        device_id: "creator".into(),
    };

    // Build and sign the PollCreate event.
    let mut create_ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Conviction,
        kind: PollEventKindCode::PollCreate,
        hlc: create_hlc.clone(),
        actor: creator,
        payload: create_payload,
        sig: vec![0u8; 64],
    };
    let sb = create_ev.signing_bytes().expect("signing_bytes");
    create_ev.sig = creator_keypair.sign(&sb).to_bytes().to_vec();

    let mut create_packet = Vec::new();
    ciborium::into_writer(&create_ev, &mut create_packet).expect("encode");

    // Snapshot: both actors are members; creator has power=5, signaler has power=0.
    let mut members = HashMap::new();
    members.insert(
        creator,
        MemberAttrs {
            power: 5,
            vouching_depth: 1,
        },
    );
    members.insert(
        signaler,
        MemberAttrs {
            power: 0,
            vouching_depth: 0,
        },
    );
    let resolvers = Arc::new(FixedResolvers {
        identity: HashMap::from([
            (creator, creator_keypair.verifying_key()),
            (signaler, signaler_keypair.verifying_key()),
        ]),
        snapshot: MembershipSnapshot { members },
    });

    let voting_log = Arc::new(Mutex::new(VotingLog::default()));
    let tracker = Arc::new(Mutex::new(VotingReplayTracker::new()));

    let id_resolver: Arc<dyn VotingIdentityResolver> = resolvers.clone();
    let mem_resolver: Arc<dyn MembershipSnapshotResolver> = resolvers.clone();

    // Apply the PollCreate first (creator is eligible).
    let create_result = process_inbound_for_test(
        community_id,
        &voting_log,
        &tracker,
        Some(&id_resolver),
        Some(&mem_resolver),
        &create_packet,
    )
    .await;
    assert!(
        create_result.is_ok(),
        "Tier 2 PollCreate from eligible creator must succeed; got: {create_result:?}"
    );

    // Derive the poll_id for the signal payload.
    let create_sb = create_ev.signing_bytes().expect("signing_bytes");
    let poll_id = derive_poll_id(&community_id, &create_sb);

    // Build a Signal from the ineligible signaler (power=0 < min_power=5).
    let signal = SignalPayload {
        proposal_id: poll_id,
        support: true,
    };
    let mut signal_payload = Vec::new();
    ciborium::into_writer(&signal, &mut signal_payload).expect("encode signal");

    let signal_hlc = Hlc {
        wall_ms: 1_700_000_001_000,
        logical: 0,
        device_id: "signaler".into(),
    };
    let mut signal_ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Conviction,
        kind: PollEventKindCode::Signal,
        hlc: signal_hlc,
        actor: signaler,
        payload: signal_payload,
        sig: vec![0u8; 64],
    };
    let signal_sb = signal_ev.signing_bytes().expect("signing_bytes");
    signal_ev.sig = signaler_keypair.sign(&signal_sb).to_bytes().to_vec();

    let mut signal_packet = Vec::new();
    ciborium::into_writer(&signal_ev, &mut signal_packet).expect("encode");

    let signal_result = process_inbound_for_test(
        community_id,
        &voting_log,
        &tracker,
        Some(&id_resolver),
        Some(&mem_resolver),
        &signal_packet,
    )
    .await;

    assert!(
        signal_result.is_err(),
        "Tier 2 Signal from ineligible actor must be rejected; got Ok"
    );
    let err_msg = signal_result.unwrap_err();
    assert!(
        err_msg.contains("actor not eligible") || err_msg.contains("not eligible"),
        "rejection must mention eligibility; got: {err_msg:?}"
    );

    // Verify per_voter is unchanged (no signal applied).
    let log = voting_log.lock().await;
    let ps = log.polls.get(&poll_id).expect("poll must exist");
    let t2 = match &ps.tier_state {
        harmony_app::community_voting_log::TierState::Tier2(t) => t,
        _ => panic!("expected Tier2 state"),
    };
    assert!(
        !t2.per_voter.contains_key(&signaler),
        "ineligible signaler must not appear in per_voter after rejected Signal"
    );
}
