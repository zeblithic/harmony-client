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
//! 5. **Wrong-scope reject (delivery-canary)** — on one wire topic and one
//!    subscriber, a beacon sealed under the RIGHT scope is accepted (proving
//!    delivery) while a beacon sealed under the WRONG scope is dropped at AEAD
//!    open — distinguished by identity, so the rejection is observable.
//!
//! Plus a **negative membership path**: a non-enrolled rogue signer whose
//! beacon opens and self-verifies is still gated out by `beacon_signer_is_member`.
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
    build_presence_tombstone, seal_presence_beacon, sign_presence_beacon,
    spawn_voice_presence_publisher, spawn_voice_presence_subscriber, VoicePresenceBeacon,
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
/// `beacon_signer_is_member(community, owner, device)` resolves true for every
/// `(owner, device)` in `members` while the event log stays empty. Each
/// `device` is that owner's enrolled ed25519 verify key.
///
/// A sanity pre-assertion confirms the FIRST member resolves as Joined+enrolled
/// so a subscriber-side false negative can't be silently mistaken for a
/// transport problem during convergence polling.
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

    // ── Identity C: a SECOND enrolled member, used as the right-scope delivery
    //    canary in Assertion 5. Distinct identity from A so a beacon it sends
    //    under `channel_other` is observable in `map_other` by owner id.
    let owner_c_seed = mint_test_owner(0xC3);
    let owner_c = owner_c_seed.owner;
    let signing_c = Arc::new(owner_c_seed.device_key);
    let device_c: [u8; 32] = signing_c.verifying_key().to_bytes();

    // ── Identity R: a ROGUE identity that is NEVER enrolled in B's registry.
    //    Used in the negative-membership assertion: a beacon it self-signs and
    //    seals under the REAL channel key opens fine and verifies, but the
    //    `beacon_signer_is_member` gate must drop it.
    let owner_r_seed = mint_test_owner(0x55);
    let owner_r = owner_r_seed.owner;
    let signing_r = Arc::new(owner_r_seed.device_key);
    let device_r: [u8; 32] = signing_r.verifying_key().to_bytes();

    // ── Shared community + channel + derived ChannelKey ─────────────────────
    let community = SpaceId([0xc0; 16]);
    let channel = ChannelId([0xc1; 16]);
    let admin = OwnerAddr([0xad; 16]);
    let membership_key = EpochKey::new([0x77; 32]);
    let channel_key = Arc::new(derive_channel_key(&membership_key, &community, &channel));

    // ── B's real registry, seeded so BOTH A and C are Joined+enrolled members.
    //    A drives convergence/tombstone/eviction; C is the right-scope canary
    //    that proves wire delivery in the wrong-scope-reject assertion. ──────
    let registry_b = seeded_registry(
        community,
        &membership_key,
        admin,
        &[(owner_a, device_a), (owner_c, device_c)],
    )
    .await;

    // ── Shared map + clock for B's subscriber ───────────────────────────────
    let map_b = Arc::new(Mutex::new(VoicePresenceMap::new()));
    // Injectable monotonic clock: starts at 0, lets the eviction phase advance
    // logically without a real 12 s sleep. Backed by an AtomicU64 we bump.
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
        Arc::clone(&no_emit_sink),
        Arc::clone(&closing),
        Arc::clone(&now_ms),
    );

    // ZEB-469: deterministically wait until A's session has discovered B's REMOTE
    // presence subscriber before A starts publishing, rather than a fixed sleep that
    // races CI scheduling (same flake class as the channel-messages template, fixed in
    // ZEB-459). A's heartbeats go out on the live wire (no replay), so any beacon
    // published before B's subscriber is matched is lost. `Locality::Remote` matches
    // B's subscriber specifically (A runs no local presence subscriber). matching_status
    // reads the session routing tables (no network round-trip), so an Err is a real
    // session failure — surface it loudly rather than masking it as a timeout.
    {
        let readiness_pub = session_a
            .declare_publisher(
                zenoh::key_expr::KeyExpr::try_from(pres_topic.clone()).expect("presence topic key"),
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

    // ── Spawn A's REAL heartbeat publisher (4 s cadence; fires immediately) ──
    let joined_hlc = Hlc {
        wall_ms: 1_000,
        logical: 0,
        device_id: hex::encode(device_a),
    };
    // ZEB-351 Voice V3: the publisher now reads a shared mute flag. This test's
    // assertions don't depend on the mute bit (only presence/scope/tombstone),
    // so a `true` flag preserves the prior `muted: true` semantics.
    let mute_flag_a = Arc::new(AtomicBool::new(true));
    let self_kicked_a = Arc::new(AtomicBool::new(false));
    let seq_counter_a = Arc::new(std::sync::atomic::AtomicU64::new(0));
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
        Arc::clone(&mute_flag_a),
        Arc::new(std::sync::atomic::AtomicU64::new(0)), // hand: lowered (ZEB-612)
        Arc::clone(&self_kicked_a),
        Arc::clone(&seq_counter_a),
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

    // ── Negative membership path: a non-enrolled signer is gated out ─────────
    //    R holds the real channel key (so its beacon opens) and self-signs a
    //    valid beacon (so the signature verifies), but R is NOT enrolled in
    //    B's registry. The subscriber's `beacon_signer_is_member` gate must
    //    drop it. We publish R's beacon directly to the MAIN presence topic
    //    while the MAIN subscriber (`sub_handle`) is still alive and `closing`
    //    is false. If the gate regressed (e.g. it was removed), R would land in
    //    `map_b` — so this assertion is load-bearing for the membership check.
    let rogue_beacon = VoicePresenceBeacon {
        owner: owner_r.0,
        device: device_r,
        muted: false,
        joined_hlc: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: hex::encode(device_r),
        },
        seq: 0,
        left: false,
        hand: None,
    };
    let signed_rogue = sign_presence_beacon(rogue_beacon, &signing_r).expect("sign rogue beacon");
    // Sealed under the REAL channel key + scope: opens cleanly on B's side,
    // signature verifies — the ONLY thing that can stop it is the member gate.
    let sealed_rogue = seal_presence_beacon(&channel_key, &community, &channel, &signed_rogue)
        .expect("seal rogue beacon");
    session_a
        .put(&pres_topic, sealed_rogue)
        .await
        .expect("publish rogue beacon");

    // Give the subscriber several poll cycles to (attempt to) process it, then
    // assert R never appears while A is still present (subscriber is alive and
    // processing — so R's absence is a genuine gate rejection, not a dead loop).
    //
    // ZEB-469 audit: retained. This is a *negative* assertion (R must NOT appear), so
    // its only failure mode is a spurious pass, never a spurious CI failure — the "A
    // still present" check below already proves the subscriber is live. The
    // deterministic delivery-canary form (used in Assertion 5 just below) would make
    // it strictly stronger; converting it is a larger change than this flake-hardening
    // pass (the sleep is not a CI-flake source).
    tokio::time::sleep(Duration::from_secs(2)).await;
    {
        let g = map_b.lock().await;
        let roster = g.roster(&community, &channel);
        assert!(
            !roster.iter().any(|r| r.owner == owner_r.0),
            "a non-enrolled (rogue) signer must be gated out by beacon_signer_is_member"
        );
        assert!(
            roster
                .iter()
                .any(|r| r.owner == owner_a.0 && r.device == device_a),
            "A must still be present — proves the subscriber is alive and processing, \
             so R's absence is a real gate rejection, not a stalled loop"
        );
    }

    // ── Assertion 5 (wrong-scope reject, delivery-canary form): on the SAME
    //    wire topic and SAME subscriber, a beacon sealed under the RIGHT scope
    //    (`channel_other`) is accepted, while one sealed under the WRONG scope
    //    (`channel`) is rejected — distinguished by IDENTITY.
    //
    //    Why the canary is load-bearing: the two beacons are published
    //    back-to-back on the same topic, and Zenoh delivers same-topic samples
    //    in order. So once we observe the RIGHT-scope canary (`owner_c`) land
    //    in `map_other`, we KNOW the wrong-scope beacon (`owner_a`) was also
    //    delivered to `sub_other`. Its absence is therefore a genuine AEAD-open
    //    rejection (wrong scope key + AAD), not a transport miss. The old
    //    "sleep 2s, assert empty" form proved neither delivery nor rejection.
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
        Arc::clone(&no_emit_sink),
        Arc::clone(&closing),
        Arc::clone(&now_ms),
    );
    // ZEB-469 audit: retained (readiness barrier, but a deterministic conversion is
    // blocked here). A publisher matching_status barrier — the fix used for the other
    // readiness waits in this file — does NOT work for `sub_other`: the main presence
    // subscriber (`sub_handle`) is already declared on this SAME wire topic on
    // `session_b`, so session_a's matching_status reports 'matched' before `sub_other`
    // has registered. And `spawn_voice_presence_subscriber` returns only a JoinHandle
    // (it declares its subscriber inside the spawned task), exposing no readiness
    // signal to await. A deterministic conversion needs a readiness-probe loop
    // (poll-publish the right-scope canary until owner_c lands, then a seq-advancing
    // follow-up to prove in-order wrong-scope delivery) — deferred to a focused
    // follow-up. `sub_other` shares session_b's already-routed pres_topic, so this is a
    // local declaration only; the 1s wait is conservative and low-flake in practice.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // (1) RIGHT-scope canary: C, sealed under `channel_key_other`. This is the
    //     correct scope for `sub_other`, so it must be accepted.
    let canary_beacon = VoicePresenceBeacon {
        owner: owner_c.0,
        device: device_c,
        muted: false,
        joined_hlc: Hlc {
            wall_ms: 3_000,
            logical: 0,
            device_id: hex::encode(device_c),
        },
        seq: 0,
        left: false,
        hand: None,
    };
    let signed_c = sign_presence_beacon(canary_beacon, &signing_c).expect("sign canary beacon");
    let sealed_canary =
        seal_presence_beacon(&channel_key_other, &community, &channel_other, &signed_c)
            .expect("seal right-scope canary");

    // (2) WRONG-scope: A, sealed under `channel_key` (the `channel` scope).
    //     Delivered on the same topic but to a subscriber configured for
    //     `channel_other` — the AEAD open with the wrong key/AAD must fail.
    let wrong_beacon = VoicePresenceBeacon {
        owner: owner_a.0,
        device: device_a,
        muted: false,
        joined_hlc: joined_hlc.clone(),
        seq: 1,
        left: false,
        hand: None,
    };
    let signed_wrong =
        sign_presence_beacon(wrong_beacon, &signing_a).expect("sign wrong-scope beacon");
    let sealed_wrong = seal_presence_beacon(&channel_key, &community, &channel, &signed_wrong)
        .expect("seal wrong-scope beacon");

    // Publish back-to-back on the same topic (in-order delivery to sub_other).
    session_a
        .put(&pres_topic, sealed_canary)
        .await
        .expect("publish right-scope canary");
    session_a
        .put(&pres_topic, sealed_wrong)
        .await
        .expect("publish wrong-scope beacon");

    // The canary's arrival PROVES the wire delivers to sub_other and its
    // open→verify→apply pipeline works for the correct scope.
    wait_until(Duration::from_secs(5), || {
        let map_other = Arc::clone(&map_other);
        async move {
            map_other
                .lock()
                .await
                .roster(&community, &channel_other)
                .iter()
                .any(|r| r.owner == owner_c.0 && r.device == device_c)
        }
    })
    .await
    .expect("right-scope canary (owner_c) must be delivered + applied by sub_other");

    // Now the wrong-scope beacon: it was delivered on the same topic before the
    // canary's pipeline completed (same-topic in-order), so its absence is a
    // genuine AEAD-open rejection, not a transport miss.
    assert!(
        !map_other
            .lock()
            .await
            .roster(&community, &channel_other)
            .iter()
            .any(|r| r.owner == owner_a.0),
        "a wrong-scope beacon (sealed under `channel`) must NOT be applied by a \
         `channel_other` subscriber — it is delivered but fails AEAD open"
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
    // ZEB-469: deterministically wait until A's session has discovered B's REMOTE
    // media subscriber before publishing the sealed frame, rather than a fixed settle.
    // This is a fresh wire topic (`harmony/voice/{c}/{ch}/*`) with no prior matching,
    // so a publisher matching_status barrier is well-defined here (a concrete-key
    // publisher on `…/{deviceA}` intersects the wildcard subscriber). The frame goes
    // out live (no replay); if put before the subscriber is matched it is lost and the
    // recv loop below stalls to its 10s ceiling.
    {
        let media_ready_pub = session_a
            .declare_publisher(
                zenoh::key_expr::KeyExpr::try_from(media_topic_a.clone()).expect("media topic key"),
            )
            .allowed_destination(zenoh::sample::Locality::Remote)
            .await
            .expect("declare media readiness publisher");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if media_ready_pub
                .matching_status()
                .await
                .expect("media matching_status query failed")
                .matching()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "A never matched B's remote media subscriber within 30s"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

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
    //
    // After Assertion 2's tombstone, B holds a hidden `left` *gravestone* for A
    // (joined_hlc + u64::MAX seq barrier). A same-session beacon would now be
    // (correctly) rejected as a reordered post-tombstone duplicate — that is the
    // resurrection guard. So the re-seed models a genuine REJOIN: a strictly
    // newer `joined_hlc`, which revives the gravestone into a fresh live entry.
    {
        let mut g = map_b.lock().await;
        let rejoin_hlc = Hlc {
            wall_ms: joined_hlc.wall_ms + 1,
            logical: joined_hlc.logical,
            device_id: joined_hlc.device_id.clone(),
        };
        // last_seen at clock=0; entry is live.
        let beacon = harmony_app::voice_presence::VoicePresenceBeacon {
            owner: owner_a.0,
            device: device_a,
            muted: true,
            joined_hlc: rejoin_hlc,
            seq: 0,
            left: false,
            hand: None,
        };
        assert!(
            g.apply(&community, &channel, &beacon, 0),
            "re-seeded entry (newer session) should revive the gravestone — a roster change"
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
