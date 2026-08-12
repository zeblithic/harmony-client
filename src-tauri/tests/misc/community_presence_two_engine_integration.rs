//! ZEB-537 community-presence two-engine integration test.
//!
//! Proves the full community-presence exchange path over two *real* Zenoh
//! sessions (loopback peer router, same harness as
//! `voice_presence_two_engine_integration.rs`):
//!
//! 1. **Online convergence** — A runs the real
//!    `spawn_community_presence_publisher`; B runs the real
//!    `spawn_community_presence_subscriber` wired to a real
//!    `CommunitySyncRegistry` whose materialized membership lists A as a
//!    `Joined`, enrolled member. B's shared `CommunityPresenceMap` converges to
//!    show A online.
//! 2. **Offline via TTL sweep** — A's publisher is stopped, in-flight beacons
//!    drained, then B's map is swept with an injected clock advanced past
//!    `STALE_MS`; A is evicted (community presence has NO tombstone — TTL is the
//!    only way off the roster).
//! 3. **Negative membership** — a non-enrolled rogue signer whose beacon opens
//!    and self-verifies under the REAL presence key is still gated out by
//!    `beacon_signer_is_member`.
//!
//! Mirrors the voice template's structure (seeded real registry + real
//! pub/sub + injected clock); the differences are community-scoped (no
//! channel), `derive_presence_key`, and no tombstone.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use harmony_app::community_channel_log::derive_presence_key;
use harmony_app::community_membership::{MaterializedMembership, MemberState, MemberStatus};
use harmony_app::community_presence::{
    seal_presence_beacon, sign_presence_beacon, spawn_community_presence_publisher,
    spawn_community_presence_subscriber, CommunityPresenceMap, PresenceBeacon, BEACON_INTERVAL_MS,
    STALE_MS,
};
use harmony_app::community_state_sync::{
    CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{ContentStore, RuntimeContentStore};
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};
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
/// Mirrors the voice template's helper.
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
/// transport problem during convergence polling.
///
/// Copied ~verbatim from `voice_presence_two_engine_integration.rs`.
async fn seeded_registry(
    community: SpaceId,
    membership_key: &EpochKey,
    members_in: &[(OwnerAddr, [u8; 32])],
) -> Arc<CommunitySyncRegistry> {
    let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        Duration::from_millis(1000),
    ));
    let dir = tempfile::tempdir().expect("tempdir");

    let registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
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
        presence_resync_rx: None,
    }));

    // Keep the tempdir alive for the test's lifetime — leak it (test process is
    // short-lived; this avoids the engine's persist paths vanishing mid-run).
    std::mem::forget(dir);

    let admin = OwnerAddr([0xad; 16]);
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
async fn community_presence_two_engine_online_and_sweep() {
    // Outer guard so a hang surfaces as a failure, not an indefinite stall.
    // The outer guard must comfortably exceed the SUM of the inner readiness +
    // convergence + drain waits — `run_inner`'s worst-case (slow CI Zenoh
    // discovery) can approach 30s, so a 30s outer budget produced flaky false
    // timeouts. 90s leaves ample headroom while still catching a true hang.
    tokio::time::timeout(Duration::from_secs(90), run_inner())
        .await
        .expect("community presence two-engine test timed out");
}

/// ZEB-600: a publisher spawned INVISIBLE (`presence_visible = false`) must emit
/// NO beacons — even after its session has matched a remote subscriber, so a
/// "no beacon" result cannot be a lost-before-matched transport race. Then a
/// live flip to visible converges (proving the gate was the only suppressor and
/// the flip takes effect without re-spawning).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invisible_publisher_emits_no_beacon() {
    tokio::time::timeout(Duration::from_secs(60), invisible_inner())
        .await
        .expect("invisible presence test timed out");
}

async fn invisible_inner() {
    let cfg = zenoh::Config::default();
    let session_a = zenoh::open(cfg.clone()).await.expect("session A");
    let session_b = zenoh::open(cfg).await.expect("session B");

    // Distinct community/owner/key from the sibling two-engine test. Both run as
    // separate processes, but zenoh's loopback scouting means their sessions
    // discover each other — so sharing a topic would let THIS test's B receive
    // the OTHER (visible) test's A beacons, a false failure. A unique community
    // (hence a unique topic) isolates us.
    let owner_a = OwnerAddr([0xa6; 16]);
    let sk_a = SigningKey::from_bytes(&[6u8; 32]);
    let device_a: [u8; 32] = sk_a.verifying_key().to_bytes();

    let membership_key = EpochKey::new([0x66; 32]);
    let community = SpaceId([0xc6; 16]);
    let topic = format!("harmony/presence/{}/beacons", hex::encode(community.0));

    let registry_b = seeded_registry(community, &membership_key, &[(owner_a, device_a)]).await;
    let registry_a = seeded_registry(community, &membership_key, &[(owner_a, device_a)]).await;

    let map_b = Arc::new(Mutex::new(CommunityPresenceMap::new()));
    let clock = Arc::new(AtomicU64::new(1_000));
    let now_ms: Arc<dyn Fn() -> u64 + Send + Sync> = {
        let c = clock.clone();
        Arc::new(move || c.load(Ordering::SeqCst))
    };
    let no_emit_sink: Arc<dyn harmony_app::node_event_sink::NodeEventSink> =
        Arc::new(harmony_app::node_event_sink::FanoutSink(vec![]));

    let closing_a = Arc::new(AtomicBool::new(false));
    let closing_b = Arc::new(AtomicBool::new(false));

    let sub_handle = spawn_community_presence_subscriber(
        session_b.clone(),
        topic.clone(),
        Arc::clone(&registry_b),
        community,
        Arc::clone(&map_b),
        Arc::clone(&no_emit_sink),
        Arc::clone(&closing_b),
        Arc::clone(&now_ms),
        // ZEB-599 Direction 1: throwaway resync sender (no receivers → no-op).
        tokio::sync::watch::channel(0u64).0,
        // ZEB-620 Task 5: no reconnect supervisor in this link-layer test.
        None,
        // ZEB-815: no address-book pool in this link-layer test.
        None,
        // ZEB-919: no owner-state handle — spawn-key degraded mode.
        None,
    );

    // Match A's session to B's REMOTE subscriber first, so a later "no beacon"
    // is attributable to the invisible gate, not a discovery race.
    {
        let readiness_pub = session_a
            .declare_publisher(
                zenoh::key_expr::KeyExpr::try_from(topic.clone()).expect("presence topic key"),
            )
            .allowed_destination(zenoh::sample::Locality::Remote)
            .await
            .expect("declare presence readiness publisher");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if readiness_pub
                .matching_status()
                .await
                .expect("presence matching_status query failed")
                .matching()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "A never matched B's remote presence subscriber within 30s"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // A's REAL publisher, but INVISIBLE.
    let started_hlc = Hlc {
        wall_ms: 1,
        logical: 0,
        device_id: hex::encode(device_a),
    };
    let seq_counter_a = Arc::new(AtomicU64::new(0));
    let a_visible = Arc::new(AtomicBool::new(false));
    let pub_handle = spawn_community_presence_publisher(
        session_a.clone(),
        topic.clone(),
        Arc::clone(&registry_a),
        community,
        Arc::new(sk_a.clone()),
        owner_a,
        device_a,
        started_hlc,
        Arc::clone(&seq_counter_a),
        Duration::from_millis(200),
        Arc::clone(&a_visible),
        Arc::clone(&closing_a),
        // ZEB-919: no owner-state handle — spawn-key degraded mode.
        None,
    );

    // Over ~2s (10 beacon intervals) B must NEVER see A.
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let seen = map_b
            .lock()
            .await
            .online_owners(&community)
            .iter()
            .any(|o| o.owner == owner_a.0);
        assert!(!seen, "invisible A must never appear in B's roster");
    }

    // Flip A visible → B converges. Proves the gate was the sole suppressor and
    // the live flip works without a re-spawn.
    a_visible.store(true, Ordering::SeqCst);
    wait_until(Duration::from_secs(10), || {
        let map_b = Arc::clone(&map_b);
        async move {
            map_b
                .lock()
                .await
                .online_owners(&community)
                .iter()
                .any(|o| o.owner == owner_a.0)
        }
    })
    .await
    .expect("after flipping visible, B should see A online");

    // Teardown: the subscriber blocks in `recv_async().await` (it wakes on
    // samples, not the closing flag), so abort rather than await to avoid a hang.
    closing_a.store(true, Ordering::SeqCst);
    closing_b.store(true, Ordering::SeqCst);
    pub_handle.abort();
    sub_handle.abort();
    // Tear down the seeded membership engines (parity with run_inner) so this
    // test doesn't leave detached registries running (CodeRabbit #381).
    registry_b
        .shutdown_all()
        .await
        .expect("shutdown registry B");
    registry_a
        .shutdown_all()
        .await
        .expect("shutdown registry A");
}

async fn run_inner() {
    // ── Two real Zenoh sessions on the shared loopback peer router ───────────
    let cfg = zenoh::Config::default();
    let session_a = zenoh::open(cfg.clone()).await.expect("session A");
    let session_b = zenoh::open(cfg).await.expect("session B");

    // ── Identity A: owner + enrolled device key, consistently bound ─────────
    // The device key is the ed25519 signing key; its verify key is what we
    // enroll. The publisher signs beacons with this key and embeds owner.0 /
    // device_vk, so the subscriber's sig-verify AND membership check both line up.
    let owner_a = OwnerAddr([0xa1; 16]);
    let sk_a = SigningKey::from_bytes(&[5u8; 32]);
    let device_a: [u8; 32] = sk_a.verifying_key().to_bytes();

    // ── Shared community + membership (epoch) key + derived presence key ─────
    let membership_key = EpochKey::new([0x77; 32]);
    let community = SpaceId([0xc0; 16]);
    let topic = format!("harmony/presence/{}/beacons", hex::encode(community.0));

    // ── B's real registry, seeded so A is a Joined+enrolled member. A's own
    //    registry is seeded the same so its publisher can re-derive the presence
    //    key via `engine_arc(community).membership_key()`. ─────────────────────
    let registry_b = seeded_registry(community, &membership_key, &[(owner_a, device_a)]).await;
    let registry_a = seeded_registry(community, &membership_key, &[(owner_a, device_a)]).await;

    // ── Shared map + injectable clock for B's subscriber ────────────────────
    // The subscriber stamps `last_seen = now_ms()`; pinning it at 1_000 while A
    // beacons lets the sweep phase advance logical time past STALE_MS without a
    // real 30 s sleep.
    let map_b = Arc::new(Mutex::new(CommunityPresenceMap::new()));
    let initial_last_seen: u64 = 1_000;
    let clock = Arc::new(AtomicU64::new(initial_last_seen));
    let now_ms: Arc<dyn Fn() -> u64 + Send + Sync> = {
        let c = clock.clone();
        Arc::new(move || c.load(Ordering::SeqCst))
    };

    // The subscriber takes a mode-agnostic NodeEventSink; this test asserts via
    // CommunityPresenceMap state, not emissions — so a no-op fanout sink.
    let no_emit_sink: Arc<dyn harmony_app::node_event_sink::NodeEventSink> =
        Arc::new(harmony_app::node_event_sink::FanoutSink(vec![]));

    let closing_a = Arc::new(AtomicBool::new(false));
    let closing_b = Arc::new(AtomicBool::new(false));

    // ── Spawn B's REAL presence subscriber (open→verify-sig→verify-member→
    //    apply) wired to the seeded registry + shared map ─────────────────────
    let sub_handle = spawn_community_presence_subscriber(
        session_b.clone(),
        topic.clone(),
        Arc::clone(&registry_b),
        community,
        Arc::clone(&map_b),
        Arc::clone(&no_emit_sink),
        Arc::clone(&closing_b),
        Arc::clone(&now_ms),
        // ZEB-599 Direction 1: throwaway resync sender (no receivers → no-op).
        tokio::sync::watch::channel(0u64).0,
        // ZEB-620 Task 5: no reconnect supervisor in this link-layer test.
        None,
        // ZEB-815: no address-book pool in this link-layer test.
        None,
        // ZEB-919: no owner-state handle — spawn-key degraded mode.
        None,
    );

    // Deterministically wait until A's session has discovered B's REMOTE presence
    // subscriber before A starts publishing (same flake class as the voice
    // template: live beacons published before B's subscriber is matched are
    // lost). `Locality::Remote` matches B's subscriber specifically. An Err from
    // matching_status is a real session failure — surface it loudly.
    {
        let readiness_pub = session_a
            .declare_publisher(
                zenoh::key_expr::KeyExpr::try_from(topic.clone()).expect("presence topic key"),
            )
            .allowed_destination(zenoh::sample::Locality::Remote)
            .await
            .expect("declare presence readiness publisher");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if readiness_pub
                .matching_status()
                .await
                .expect("presence matching_status query failed")
                .matching()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "A never matched B's remote presence subscriber within 30s \
                 (inter-session zenoh discovery did not complete)"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // ── Spawn A's REAL heartbeat publisher (SHORT 200 ms cadence; fires
    //    immediately) so convergence is fast. ─────────────────────────────────
    let started_hlc = Hlc {
        wall_ms: 1,
        logical: 0,
        device_id: hex::encode(device_a),
    };
    let seq_counter_a = Arc::new(AtomicU64::new(0));
    let pub_handle = spawn_community_presence_publisher(
        session_a.clone(),
        topic.clone(),
        Arc::clone(&registry_a),
        community,
        Arc::new(sk_a.clone()),
        owner_a,
        device_a,
        started_hlc.clone(),
        Arc::clone(&seq_counter_a),
        Duration::from_millis(200),
        Arc::new(AtomicBool::new(true)),
        Arc::clone(&closing_a),
        // ZEB-919: no owner-state handle — spawn-key degraded mode.
        None,
    );

    // ── Assertion 1 (ONLINE): B sees A online ───────────────────────────────
    wait_until(Duration::from_secs(10), || {
        let map_b = Arc::clone(&map_b);
        async move {
            map_b
                .lock()
                .await
                .online_owners(&community)
                .iter()
                .any(|o| o.owner == owner_a.0)
        }
    })
    .await
    .expect("B should see A online");

    // ── Negative membership: a non-enrolled rogue signer is gated out ────────
    //    R holds the real presence key (so its beacon opens) and self-signs a
    //    valid beacon (so the signature verifies), but R is NOT enrolled in B's
    //    registry. The subscriber's `beacon_signer_is_member` gate must drop it.
    //    Published directly to the MAIN topic while the MAIN subscriber is alive.
    let owner_r = OwnerAddr([0x99; 16]);
    let sk_r = SigningKey::from_bytes(&[9u8; 32]);
    let device_r: [u8; 32] = sk_r.verifying_key().to_bytes();
    let presence_key = derive_presence_key(&membership_key, &community);
    let rogue_beacon = PresenceBeacon {
        owner: owner_r.0,
        device: device_r,
        started_hlc: Hlc {
            wall_ms: 2,
            logical: 0,
            device_id: hex::encode(device_r),
        },
        seq: 0,
    };
    let signed_rogue = sign_presence_beacon(rogue_beacon, &sk_r).expect("sign rogue beacon");
    // Sealed under the REAL presence key + community: opens cleanly on B's side,
    // signature verifies — the ONLY thing that can stop it is the member gate.
    let sealed_rogue =
        seal_presence_beacon(&presence_key, &community, &signed_rogue).expect("seal rogue beacon");
    session_a
        .put(&topic, sealed_rogue)
        .await
        .expect("publish rogue beacon");

    // Give the subscriber several poll cycles to (attempt to) process it, then
    // assert R never appears while A is still online (subscriber is alive and
    // processing — so R's absence is a genuine gate rejection, not a dead loop).
    tokio::time::sleep(Duration::from_secs(2)).await;
    {
        let g = map_b.lock().await;
        let owners = g.online_owners(&community);
        assert!(
            !owners.iter().any(|o| o.owner == owner_r.0),
            "a non-enrolled (rogue) signer must be gated out by beacon_signer_is_member"
        );
        assert!(
            owners.iter().any(|o| o.owner == owner_a.0),
            "A must still be online — proves the subscriber is alive and processing, \
             so R's absence is a real gate rejection, not a stalled loop"
        );
    }

    // ── Assertion 2 (OFFLINE via TTL sweep) ─────────────────────────────────
    // Stop A's publisher so no late beacon can re-arm A's last_seen, drain
    // in-flight beacons, then advance the injected clock past STALE_MS and sweep
    // directly — no real 30 s wait. While A beaconed, last_seen == clock value
    // (1_000); after stopping A and advancing the clock, sweep must evict.
    closing_a.store(true, Ordering::SeqCst);
    pub_handle.abort();
    // Drain any in-flight beacons already on the wire (so they don't land after
    // we advance the clock and re-arm last_seen at the advanced value).
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Advance the clock past the staleness window, then sweep. Sweep is driven
    // directly on B's map with the advanced clock value as `now_ms`.
    let swept_now = initial_last_seen + STALE_MS + 1;
    clock.store(swept_now, Ordering::SeqCst);
    let evicted = {
        let mut g = map_b.lock().await;
        g.sweep(swept_now, STALE_MS)
    };
    assert!(
        !evicted.is_empty(),
        "sweep past STALE_MS must evict A (was last seen at {initial_last_seen}, now {swept_now})"
    );
    assert!(
        evicted
            .iter()
            .any(|(c, o, d)| *c == community && *o == owner_a.0 && *d == device_a),
        "sweep eviction list must name A's (community, owner, device)"
    );
    assert!(
        !map_b
            .lock()
            .await
            .online_owners(&community)
            .iter()
            .any(|o| o.owner == owner_a.0),
        "A must no longer be online after the TTL sweep"
    );

    // ── Sanity: the production cadence constants relate as designed (TTL must
    //    exceed the beacon interval). Read through `std::hint::black_box` so this
    //    stays a runtime check on the imported constants, not a const assertion.
    assert!(
        std::hint::black_box(STALE_MS) > std::hint::black_box(BEACON_INTERVAL_MS),
        "TTL must exceed the beacon interval"
    );

    // ── Teardown ────────────────────────────────────────────────────────────
    closing_b.store(true, Ordering::SeqCst);
    sub_handle.abort();
    registry_b
        .shutdown_all()
        .await
        .expect("shutdown registry B");
    registry_a
        .shutdown_all()
        .await
        .expect("shutdown registry A");
}
