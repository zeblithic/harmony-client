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
use harmony_app::community_voting_core::{
    build_signed_poll_create_tier1, derive_poll_id, Eligibility, MemberAttrs, MembershipSnapshot,
    VotingIdentityResolver,
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
