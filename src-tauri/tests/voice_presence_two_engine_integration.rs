//! ZEB-350 Voice V2 two-engine integration test.
//!
//! Proves the full presence exchange + sealed media-relay path over two
//! *real* Zenoh sessions (loopback peer router, same as
//! `community_channel_messages_integration.rs`):
//!
//! 1. **Roster convergence** — A runs the real
//!    `spawn_voice_presence_publisher`; B runs the real
//!    `spawn_voice_presence_subscriber` wired to a real `CommunitySyncRegistry`
//!    whose materialized membership lists A as a `Joined`, enrolled member. B's
//!    shared `VoicePresenceMap` converges to include A.
//! 2. **Tombstone instant removal** — A publishes a `build_presence_tombstone`
//!    beacon; B's roster empties promptly (no TTL wait).
//! 3. **Eviction** — A's publisher is stopped, then B's map is swept with an
//!    injected clock advanced past the 12 s TTL; A is evicted.
//! 4. **Sealed media relay** — A `encrypt_voice_packet`s a frame and `put`s it
//!    to `harmony/voice/{c}/{ch}/{deviceA}`; B opens it on
//!    `harmony/voice/{c}/{ch}/*` and recovers the original frame.
//! 5. **Wrong-scope reject** — a beacon sealed under channel A is NOT applied
//!    by a channel-B subscriber.
//!
//! Path taken: the **real `spawn_voice_presence_subscriber` + real
//! `CommunitySyncRegistry`** path (NOT the manual fallback). Seeding membership
//! is cheap: `CommunityState::materialized()` returns a `seed_bootstrap_hint`
//! verbatim while the event log is empty, so we spawn an engine via
//! `spawn_engine_inner_now`, grab its `state()`, and seed a hint making A a
//! `Joined` member with A's device key enrolled. ~50 lines, no event plumbing.

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
use harmony_app::voice_crypto::{decrypt_voice_packet, encrypt_voice_packet, VOICE_PACKET_AAD};
use harmony_app::voice_presence::{
    build_presence_tombstone, spawn_voice_presence_publisher, spawn_voice_presence_subscriber,
    VoicePresenceMap,
};
use std::collections::{BTreeMap, BTreeSet};
use tokio::sync::{mpsc, Mutex};

/// Resolver stub — the presence path never calls it (membership comes from the
/// seeded bootstrap hint, not from receive-side `verify_event`), so returning
/// `None` is fine.
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
/// `beacon_signer_is_member(community, owner, device)` resolves true while the
/// event log stays empty. `device` is A's enrolled ed25519 verify key.
async fn seeded_registry(
    community: SpaceId,
    membership_key: &EpochKey,
    admin: OwnerAddr,
    member_owner: OwnerAddr,
    member_device: [u8; 32],
) -> Arc<CommunitySyncRegistry> {
    let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        Duration::from_millis(1000),
    ));
    let dir = tempfile::tempdir().expect("tempdir");

    let registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "device-b".into(),
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
        )
        .await
        .expect("spawn membership engine");

    // Seed the materialized membership: member_owner is Joined with
    // member_device enrolled. `materialized()` returns this hint verbatim while
    // `events` is empty (which it is — no CRDT events flow in this test).
    let engine = registry
        .engine_arc(&community)
        .await
        .expect("engine present after spawn");
    let mut members = BTreeMap::new();
    let mut keys = BTreeSet::new();
    keys.insert(member_device);
    members.insert(
        member_owner,
        MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "seed".into(),
            },
            left_at: None,
            enrolled_device_keys: keys,
        },
    );
    let hint = MaterializedMembership {
        members,
        ..Default::default()
    };
    engine.state().lock().await.seed_bootstrap_hint(hint);

    // Sanity: confirm the seeded hint resolves the member as enrolled+Joined,
    // so a subscriber-side false negative can't be silently mistaken for a
    // transport problem during convergence polling.
    assert!(
        harmony_app::voice_presence::beacon_signer_is_member(
            &registry,
            &community,
            &member_owner,
            &member_device,
        )
        .await,
        "seeded registry must admit A as an enrolled Joined member"
    );

    registry
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn voice_presence_two_engine_exchange_and_sealed_relay() {
    // Outer guard so a hang surfaces as a failure, not an indefinite stall.
    tokio::time::timeout(Duration::from_secs(30), run_inner())
        .await
        .expect("voice presence two-engine test timed out");
}

async fn run_inner() {
    // ── Two real Zenoh sessions on the shared loopback peer router ──────────
    let cfg = zenoh::Config::default();
    let session_a = zenoh::open(cfg.clone()).await.expect("session A");
    let session_b = zenoh::open(cfg).await.expect("session B");

    // ── Identity A: owner + enrolled device key, consistently bound ─────────
    // mint_test_owner gives an OwnerAddr (master-derived) and a device signing
    // key whose verify key is what we enroll. The publisher signs beacons with
    // this device key and embeds owner.0 / device_vk, so the subscriber's
    // sig-verify AND membership check both line up.
    let owner_a_seed = mint_test_owner(0xA1);
    let owner_a = owner_a_seed.owner;
    let signing_a = Arc::new(owner_a_seed.device_key);
    let device_a: [u8; 32] = signing_a.verifying_key().to_bytes();

    // ── Shared community + channel + derived ChannelKey ─────────────────────
    let community = SpaceId([0xc0; 16]);
    let channel = ChannelId([0xc1; 16]);
    let admin = OwnerAddr([0xad; 16]);
    let membership_key = EpochKey::new([0x77; 32]);
    let channel_key = Arc::new(derive_channel_key(&membership_key, &community, &channel));

    // ── B's real registry, seeded so A is a Joined+enrolled member ──────────
    let registry_b = seeded_registry(community, &membership_key, admin, owner_a, device_a).await;

    // ── Shared map + clock for B's subscriber ───────────────────────────────
    let map_b = Arc::new(Mutex::new(VoicePresenceMap::new()));
    // Injectable monotonic clock: starts at 0, lets the eviction phase advance
    // logically without a real 12 s sleep. Backed by an AtomicU64 we bump.
    let clock = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let clock_for_now = Arc::clone(&clock);
    let now_ms: Arc<dyn Fn() -> u64 + Send + Sync> =
        Arc::new(move || clock_for_now.load(std::sync::atomic::Ordering::SeqCst));

    let app_b = tauri::test::mock_app();
    let closing = Arc::new(AtomicBool::new(false));

    let pres_topic = format!(
        "harmony/voice-presence/{}/{}",
        hex::encode(community.0),
        hex::encode(channel.0),
    );

    // ── Spawn B's REAL presence subscriber (open→verify-sig→verify-member→
    //    apply) wired to the seeded registry + shared map ──────────────────
    let sub_handle = spawn_voice_presence_subscriber(
        session_b.clone(),
        pres_topic.clone(),
        Arc::clone(&channel_key),
        community,
        channel,
        Arc::clone(&registry_b),
        Arc::clone(&map_b),
        app_b.handle().clone(),
        Arc::clone(&closing),
        Arc::clone(&now_ms),
    );

    // Loopback subscriber declaration needs ~1 s to settle + peers to discover
    // before A starts publishing (same as the channel-messages template).
    tokio::time::sleep(Duration::from_secs(1)).await;

    // ── Spawn A's REAL heartbeat publisher (4 s cadence; fires immediately) ──
    let joined_hlc = Hlc {
        wall_ms: 1_000,
        logical: 0,
        device_id: hex::encode(device_a),
    };
    let pub_handle = spawn_voice_presence_publisher(
        session_a.clone(),
        pres_topic.clone(),
        Arc::clone(&channel_key),
        community,
        channel,
        Arc::clone(&signing_a),
        owner_a,
        device_a,
        joined_hlc.clone(),
        Duration::from_secs(4),
        Arc::clone(&closing),
    );

    // ── Assertion 1: B's roster converges to include A ──────────────────────
    wait_until(Duration::from_secs(10), || {
        let map_b = Arc::clone(&map_b);
        async move {
            map_b
                .lock()
                .await
                .roster(&community, &channel)
                .iter()
                .any(|r| r.owner == owner_a.0 && r.device == device_a)
        }
    })
    .await
    .expect("B's roster should converge to include A");

    // ── Assertion 5 (wrong-scope reject): a channel-B subscriber must not ───
    //    apply a beacon sealed under a DIFFERENT channel. We open a second
    //    subscriber bound to channel_other (own map) on the SAME presence
    //    topic family, then confirm A's channel-`channel` beacons never land
    //    in it (the seal AAD binds to (community, channel), so the open fails).
    let channel_other = ChannelId([0xc2; 16]);
    let channel_key_other = Arc::new(derive_channel_key(
        &membership_key,
        &community,
        &channel_other,
    ));
    let map_other = Arc::new(Mutex::new(VoicePresenceMap::new()));
    let sub_other = spawn_voice_presence_subscriber(
        session_b.clone(),
        pres_topic.clone(), // SAME wire topic — only the seal scope differs
        Arc::clone(&channel_key_other),
        community,
        channel_other,
        Arc::clone(&registry_b),
        Arc::clone(&map_other),
        app_b.handle().clone(),
        Arc::clone(&closing),
        Arc::clone(&now_ms),
    );
    // Give it time to settle + receive several of A's heartbeats; it must stay
    // empty because every beacon's AAD is bound to `channel`, not `channel_other`.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        map_other
            .lock()
            .await
            .roster(&community, &channel_other)
            .is_empty(),
        "a channel-A beacon must NOT be applied by a wrong-scope (channel-B) subscriber"
    );
    sub_other.abort();

    // ── Sealed media relay leg (Assertion 4) ────────────────────────────────
    // A seals a frame and publishes to its own-device topic; B opens it on the
    // wildcard media topic and recovers the original frame. Validates topic +
    // transport (the crypto itself is unit-tested).
    let media_topic_a = format!(
        "harmony/voice/{}/{}/{}",
        hex::encode(community.0),
        hex::encode(channel.0),
        hex::encode(device_a),
    );
    let media_sub_key = format!(
        "harmony/voice/{}/{}/*",
        hex::encode(community.0),
        hex::encode(channel.0),
    );
    let media_sub = session_b
        .declare_subscriber(&media_sub_key)
        .await
        .expect("declare media subscriber");
    tokio::time::sleep(Duration::from_secs(1)).await; // settle

    let original_frame: Vec<u8> = (0u8..40).collect();
    let sealed_frame = encrypt_voice_packet(
        &channel_key,
        &community,
        &channel,
        VOICE_PACKET_AAD,
        &original_frame,
    )
    .expect("seal media frame");
    session_a
        .put(&media_topic_a, sealed_frame)
        .await
        .expect("publish sealed media");

    let recovered = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let sample = media_sub.recv_async().await.expect("media recv");
            let bytes = sample.payload().to_bytes().to_vec();
            if let Ok(frame) =
                decrypt_voice_packet(&channel_key, &community, &channel, VOICE_PACKET_AAD, &bytes)
            {
                break frame;
            }
        }
    })
    .await
    .expect("B should receive + open the sealed media frame");
    assert_eq!(
        recovered, original_frame,
        "recovered media frame must equal the original"
    );
    // `media_sub` is a Zenoh subscriber (not a JoinHandle) — dropping it
    // undeclares the subscription.
    drop(media_sub);

    // ── Assertion 2: tombstone removes A instantly (no TTL wait) ────────────
    // Stop A's heartbeat first so it can't re-add A after the tombstone, then
    // publish a `left=true` tombstone from A's session.
    pub_handle.abort();
    let tombstone = build_presence_tombstone(
        &channel_key,
        &community,
        &channel,
        &signing_a,
        owner_a,
        device_a,
        joined_hlc.clone(),
    )
    .expect("build tombstone");
    session_a
        .put(&pres_topic, tombstone)
        .await
        .expect("publish tombstone");

    wait_until(Duration::from_secs(5), || {
        let map_b = Arc::clone(&map_b);
        async move { map_b.lock().await.roster(&community, &channel).is_empty() }
    })
    .await
    .expect("tombstone should empty B's roster promptly (well under the 12 s TTL)");

    // ── Assertion 3: eviction via injected-clock sweep past the TTL ─────────
    // Re-seed one live entry for A (publisher is stopped), then prove a sweep
    // with `now` advanced past 12 s evicts it — using logical time, not a real
    // 12 s sleep. We drive `apply`/`sweep` on B's map directly with the
    // injected clock.
    {
        let mut g = map_b.lock().await;
        // last_seen at clock=0; entry is live.
        let beacon = harmony_app::voice_presence::VoicePresenceBeacon {
            owner: owner_a.0,
            device: device_a,
            muted: true,
            joined_hlc: joined_hlc.clone(),
            seq: 0,
            left: false,
        };
        assert!(
            g.apply(&community, &channel, &beacon, 0),
            "re-seeded entry should be a roster change"
        );
        assert!(
            !g.roster(&community, &channel).is_empty(),
            "re-seeded entry should be visible before sweep"
        );
        // Within TTL: not evicted.
        assert!(
            g.sweep(11_000, 12_000).is_empty(),
            "entry within 12 s TTL must survive sweep"
        );
        assert!(!g.roster(&community, &channel).is_empty());
        // Past TTL: evicted.
        let evicted = g.sweep(13_000, 12_000);
        assert_eq!(
            evicted,
            vec![((community, channel), owner_a.0, device_a)],
            "12 s of silence must evict A"
        );
        assert!(
            g.roster(&community, &channel).is_empty(),
            "roster empties after eviction"
        );
    }

    // ── Teardown ────────────────────────────────────────────────────────────
    closing.store(true, std::sync::atomic::Ordering::SeqCst);
    sub_handle.abort();
    registry_b
        .shutdown_all()
        .await
        .expect("shutdown registry B");
}
