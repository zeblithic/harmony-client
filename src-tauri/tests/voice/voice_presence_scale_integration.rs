//! ZEB-353 Voice V5 N-publisher presence-scale integration test.
//!
//! Mirrors `voice_presence_two_engine_integration.rs` but scales the publisher
//! count toward the 64 soft-cap: a single subscriber over a real Zenoh session
//! converges its `VoicePresenceMap` roster to include all N=64 distinct,
//! enrolled members, then the TTL sweep evicts all N at once. This validates
//! that the roster converges and the TTL sweep handles the load — no latency
//! bounds, just convergence + eviction.
//!
//! # Transport-flake class
//!
//! This test exercises a Zenoh loopback relay and belongs to the same known
//! transport-flake class as `voice_presence_two_engine_integration` and the
//! iroh-zenoh loopback tests. It passes on CI; it may flake or run slow on a
//! loopback-restricted local box. Do NOT add retries that mask real failures.
//! If the test fails locally but the convergence/eviction assertion path is
//! sound, note it and let CI be authoritative.

#![cfg(feature = "test-fixtures")]

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use harmony_app::community_channel_log::derive_channel_key;
use harmony_app::community_membership::{
    mint_test_owner, ChannelId, MaterializedMembership, MemberState, MemberStatus,
};
use harmony_app::community_state_sync::{
    CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{ContentStore, RuntimeContentStore};
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};
use harmony_app::voice_presence::{
    spawn_voice_presence_publisher, spawn_voice_presence_subscriber, VoicePresenceMap,
};
use std::collections::{BTreeMap, BTreeSet};
use tokio::sync::{mpsc, Mutex};

/// Number of distinct publishers to simulate — the presence soft-cap. Tunable.
const N: usize = 64;

/// Resolver stub — the presence path never calls it (membership comes from the
/// seeded bootstrap hint, not from receive-side `verify_event`), so returning
/// `None` is fine. Mirrors the two-engine template.
struct NopResolver;
#[async_trait::async_trait]
impl IdentityResolver for NopResolver {
    async fn resolve(&self, _: &OwnerAddr) -> Option<[u8; 64]> {
        None
    }
}

/// Poll an async predicate until it returns true or `timeout` elapses.
/// Mirrors the template's helper.
async fn wait_until<F, Fut>(timeout: Duration, mut predicate: F) -> Result<(), ()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if predicate().await {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Build a `CommunitySyncRegistry` and spawn an engine for `community`, then
/// seed its `CommunityState` materialized-membership bootstrap hint so that
/// `beacon_signer_is_member(community, owner, device)` resolves true for every
/// `(owner, device)` in `members_in` while the event log stays empty. Each
/// `device` is that owner's enrolled ed25519 verify key.
///
/// A sanity pre-assertion confirms the FIRST member resolves as Joined+enrolled
/// so a subscriber-side false negative can't be silently mistaken for a
/// transport problem during convergence polling. Mirrors the two-engine
/// template verbatim.
async fn seeded_registry(
    community: SpaceId,
    membership_key: &EpochKey,
    admin: OwnerAddr,
    members_in: &[(OwnerAddr, [u8; 32])],
) -> Arc<CommunitySyncRegistry> {
    let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        Duration::from_millis(1000),
    ));
    let dir = tempfile::tempdir().expect("tempdir");

    let registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_cipher: harmony_app::device_dataset_file::test_cipher(),
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        device_id: "device-sub".into(),
        content_store: cs,
        identity_resolver: Arc::new(NopResolver),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: OwnerAddr([0x0b; 16]),
        signing_key: Arc::new(SigningKey::from_bytes(&[0x42; 32])),
        crdt_state: None,
        nav_emitter: None,
        membership_updated_emitter: None,
        presence_resync_rx: None,
    }));

    // Keep the tempdir alive for the test's lifetime — leak it (test process is
    // short-lived; this avoids the engine's persist paths vanishing mid-run).
    std::mem::forget(dir);

    let (pub_tx, _pub_rx) = mpsc::channel(8);
    let (_sub_tx, sub_rx) = mpsc::channel(8);
    registry
        .spawn_engine_inner_now(
            community,
            membership_key.clone(),
            admin,
            /* is_invite_only */ false,
            pub_tx,
            sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn membership engine");

    // Seed the materialized membership: each `(owner, device)` in `members_in`
    // is Joined with its device enrolled. `materialized()` returns this hint
    // verbatim while `events` is empty (which it is — no CRDT events flow in
    // this test).
    let engine = registry
        .engine_arc(&community)
        .await
        .expect("engine present after spawn");
    let mut members = BTreeMap::new();
    for (member_owner, member_device) in members_in {
        let mut keys = BTreeSet::new();
        keys.insert(*member_device);
        members.insert(
            *member_owner,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "seed".into(),
                },
                left_at: None,
                enrolled_device_keys: keys,
                revoked_device_keys: std::collections::BTreeSet::new(),
            },
        );
    }
    let hint = MaterializedMembership {
        members,
        ..Default::default()
    };
    engine.state().lock().await.seed_bootstrap_hint(hint);

    // Sanity: confirm the seeded hint resolves the FIRST member as
    // enrolled+Joined, so a subscriber-side false negative can't be silently
    // mistaken for a transport problem during convergence polling.
    let (first_owner, first_device) = members_in
        .first()
        .expect("seeded_registry requires at least one member");
    assert!(
        harmony_app::voice_presence::beacon_signer_is_member(
            &registry,
            &community,
            first_owner,
            first_device,
        )
        .await,
        "seeded registry must admit the first member as enrolled + Joined"
    );

    registry
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n_publishers_converge_and_sweep() {
    // Outer guard so a hang surfaces as a failure, not an indefinite stall. The
    // N=64-publisher convergence over loopback can be slow on a dev box, so the
    // budget is generous (this is the loopback-flake class).
    tokio::time::timeout(Duration::from_secs(45), run_inner())
        .await
        .expect("voice presence N-publisher scale test timed out");
}

async fn run_inner() {
    // ── Two real Zenoh sessions on the shared loopback peer router ──────────
    // One subscriber session; one publisher session shared across all N
    // publisher tasks (each task still emits a DISTINCT signed+sealed beacon,
    // so the roster must converge to N distinct entries). Sharing one publisher
    // session keeps us within Zenoh loopback session limits for N=64.
    let cfg = zenoh::Config::default();
    let session_sub = zenoh::open(cfg.clone()).await.expect("subscriber session");
    let session_pub = zenoh::open(cfg).await.expect("publisher session");

    // ── Shared community + channel + derived ChannelKey ─────────────────────
    let community = SpaceId([0xc0; 16]);
    let channel = ChannelId([0xc1; 16]);
    let admin = OwnerAddr([0xad; 16]);
    let membership_key = EpochKey::new([0x77; 32]);
    let channel_key = Arc::new(derive_channel_key(&membership_key, &community, &channel));

    // ── N distinct enrolled members ─────────────────────────────────────────
    // `mint_test_owner(seed)` derives the master key from `[seed; 32]` and the
    // device key from `[seed ^ 0xFF; 32]`. Seeds 1..=64 are all in 0x01..=0x40,
    // so no seed collides with another's `N ^ 0xFF` partner (those land in
    // 0xBF..=0xFE) — every member is a genuinely distinct identity + device.
    struct Member {
        owner: OwnerAddr,
        signing: Arc<SigningKey>,
        device: [u8; 32],
        joined_hlc: Hlc,
    }
    let mut members: Vec<Member> = Vec::with_capacity(N);
    let mut seeded_input: Vec<(OwnerAddr, [u8; 32])> = Vec::with_capacity(N);
    for i in 0..N {
        let seed = (i + 1) as u8; // 1..=64, all in 0x01..=0x40
        let owner_seed = mint_test_owner(seed);
        let owner = owner_seed.owner;
        let signing = Arc::new(owner_seed.device_key);
        let device: [u8; 32] = signing.verifying_key().to_bytes();
        let joined_hlc = Hlc {
            wall_ms: 1_000 + i as u64,
            logical: 0,
            device_id: hex::encode(device),
        };
        seeded_input.push((owner, device));
        members.push(Member {
            owner,
            signing,
            device,
            joined_hlc,
        });
    }

    // ── The subscriber's real registry, seeded so ALL N members are
    //    Joined+enrolled. ────────────────────────────────────────────────────
    let registry_sub = seeded_registry(community, &membership_key, admin, &seeded_input).await;

    // ── Shared map + injectable clock for the subscriber ────────────────────
    // The clock starts at 0; every beacon the subscriber applies stamps
    // `last_seen = 0`. The eviction phase then sweeps with `now` advanced past
    // the 12 s TTL — logical time, no real sleep.
    let map = Arc::new(Mutex::new(VoicePresenceMap::new()));
    let clock = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let clock_for_now = Arc::clone(&clock);
    let now_ms: Arc<dyn Fn() -> u64 + Send + Sync> =
        Arc::new(move || clock_for_now.load(std::sync::atomic::Ordering::SeqCst));

    // ZEB-445: the presence subscriber takes a mode-agnostic NodeEventSink;
    // this test asserts via VoicePresenceMap state, not emissions.
    let no_emit_sink: Arc<dyn harmony_app::node_event_sink::NodeEventSink> =
        Arc::new(harmony_app::node_event_sink::FanoutSink(vec![]));
    let closing = Arc::new(AtomicBool::new(false));

    let pres_topic = format!(
        "harmony/voice-presence/{}/{}",
        hex::encode(community.0),
        hex::encode(channel.0),
    );

    // ── Spawn the REAL presence subscriber (open→verify-sig→verify-member→
    //    apply) wired to the seeded registry + shared map ────────────────────
    let sub_handle = spawn_voice_presence_subscriber(
        session_sub.clone(),
        pres_topic.clone(),
        Arc::clone(&channel_key),
        community,
        channel,
        Arc::clone(&registry_sub),
        Arc::clone(&map),
        Arc::clone(&no_emit_sink),
        Arc::clone(&closing),
        Arc::clone(&now_ms),
    );

    // Loopback subscriber declaration needs ~1 s to settle + peers to discover
    // before publishers start (same as the two-engine template).
    tokio::time::sleep(Duration::from_secs(1)).await;

    // ── Spawn N REAL heartbeat publishers, one per member, sharing the
    //    publisher session. Each fires immediately then every 4 s, emitting a
    //    DISTINCT signed+sealed beacon for its member. ───────────────────────
    let mut pub_handles = Vec::with_capacity(N);
    for m in &members {
        let mute_flag = Arc::new(AtomicBool::new(true));
        let self_kicked = Arc::new(AtomicBool::new(false));
        let seq_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let handle = spawn_voice_presence_publisher(
            session_pub.clone(),
            pres_topic.clone(),
            Arc::clone(&channel_key),
            community,
            channel,
            Arc::clone(&m.signing),
            m.owner,
            m.device,
            m.joined_hlc.clone(),
            mute_flag,
            Arc::new(std::sync::atomic::AtomicU64::new(0)), // hand: lowered (ZEB-612)
            self_kicked,
            seq_counter,
            Duration::from_secs(4),
            Arc::clone(&closing),
        );
        pub_handles.push(handle);
    }

    // ── Convergence: the subscriber's roster must include all N members ──────
    wait_until(Duration::from_secs(30), || {
        let map = Arc::clone(&map);
        async move { map.lock().await.roster(&community, &channel).len() >= N }
    })
    .await
    .expect("subscriber roster should converge to all N publishers");

    // Stronger check: every distinct member is present, not just the count.
    {
        let g = map.lock().await;
        let roster = g.roster(&community, &channel);
        assert_eq!(
            roster.len(),
            N,
            "roster should hold exactly N distinct entries"
        );
        for m in &members {
            assert!(
                roster
                    .iter()
                    .any(|r| r.owner == m.owner.0 && r.device == m.device),
                "every seeded member must appear in the converged roster"
            );
        }
    }

    // ── TTL sweep evicts all N at once ──────────────────────────────────────
    // Stop the publishers first so they can't re-stamp liveness mid-sweep, then
    // drive `sweep` on the map directly with `now` advanced past the 12 s TTL —
    // logical time, no real 12 s sleep (mirrors the two-engine eviction leg).
    for h in &pub_handles {
        h.abort();
    }
    // Brief settle so no in-flight beacon re-stamps after the abort.
    tokio::time::sleep(Duration::from_millis(200)).await;
    {
        let mut g = map.lock().await;
        // Within TTL: nothing evicted (all stamped at clock=0).
        assert!(
            g.sweep(11_000, 12_000).is_empty(),
            "entries within the 12 s TTL must survive the sweep"
        );
        assert_eq!(
            g.roster(&community, &channel).len(),
            N,
            "roster intact within TTL"
        );
        // Past TTL: all N evicted.
        let evicted = g.sweep(13_000, 12_000);
        assert_eq!(
            evicted.len(),
            N,
            "12 s of silence must evict all N publishers in one sweep"
        );
        assert!(
            g.roster(&community, &channel).is_empty(),
            "roster empties after the TTL sweep evicts all N"
        );
    }

    // ── Teardown ────────────────────────────────────────────────────────────
    closing.store(true, std::sync::atomic::Ordering::SeqCst);
    sub_handle.abort();
    registry_sub
        .shutdown_all()
        .await
        .expect("shutdown subscriber registry");
}
