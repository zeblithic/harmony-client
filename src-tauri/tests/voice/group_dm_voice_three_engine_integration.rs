//! ZEB-360 group-DM voice three-engine end-to-end integration test.
//!
//! Proves the full group-DM voice path over THREE real Zenoh sessions on the
//! shared loopback peer router (same transport class as
//! `voice_presence_two_engine_integration` and `voice_dm_two_engine_integration`):
//!
//! 1. **Roster convergence to 3** — A/B/C each run the real
//!    `spawn_groupdm_presence_publisher`; B and C each run the real
//!    `spawn_groupdm_presence_subscriber` wired to a CRDT `OwnerState` whose
//!    `GroupDm` space lists a, b, c as members with each member's device
//!    enrolled. B's shared `VoicePresenceMap` converges to all three.
//! 2. **Media relay across 3** — A seals a frame under `K_voice =
//!    derive_dm_voice_key(content_key, call_id)` and publishes it on
//!    `harmony/voice/dm/{callHex}/{deviceAHex}`; C opens it on
//!    `harmony/voice/dm/{callHex}/*` and recovers the original frame.
//! 3. **Leave tombstone drops a participant** — C publishes a `left` tombstone;
//!    B's roster drops to {a, b}.
//! 4. **Last-leave clears the roster** — A and B tombstone too; B's roster
//!    empties.
//! 5. **Negative (wrong call_id key)** — a frame sealed under `K_voice` for a
//!    DIFFERENT call_id fails to open under the real `K_voice` (per-call key
//!    binding).
//! 6. **Negative (non-member beacon dropped)** — a rogue identity NOT in
//!    `members` signs + wraps + seals a valid beacon under the SAME presence key
//!    and publishes it; B's roster NEVER includes the rogue, proving
//!    `groupdm_beacon_signer_is_member` gates it out.
//!
//! # Transport-flake class
//!
//! This test exercises a Zenoh loopback relay and belongs to the same known
//! transport-flake class as the two-engine voice tests. It passes on CI; it may
//! flake on a loopback-restricted local box. Do NOT add retries that mask real
//! failures or weaken assertions to force a pass.

#![cfg(feature = "test-fixtures")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use harmony_app::community_channel_log::{derive_dm_voice_key, derive_groupdm_presence_key};
use harmony_app::community_membership::{mint_test_owner, ChannelId};
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{
    DeviceIdentityHash, DmContentKey, Hlc, OwnerAddr, OwnerDeviceEntry, Space, SpaceId, SpaceKind,
};
use harmony_app::voice_crypto::{
    decrypt_dm_voice_packet, encrypt_dm_voice_packet, VoiceCryptoError, VOICE_DM_PACKET_AAD,
};
use harmony_app::voice_presence::{
    publish_groupdm_leave_tombstone, seal_groupdm_presence_beacon, sign_presence_beacon,
    spawn_groupdm_presence_publisher, spawn_groupdm_presence_subscriber, GroupSignedPresenceBeacon,
    VoicePresenceBeacon, VoicePresenceMap,
};

/// Poll an async predicate until it returns true or `timeout` elapses.
/// Mirrors the two-engine templates' helper.
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

fn hlc_seed() -> Hlc {
    Hlc {
        wall_ms: 1,
        logical: 0,
        device_id: "seed".into(),
    }
}

/// A 64-byte identity_pub `[X25519(32) || Ed25519(32)]` whose Ed25519 half
/// (bytes `[32..64]`) is `device`. The X25519 half is filler — the membership
/// check only inspects the upper half. Mirrors the `groupdm_membership_tests`
/// idiom added in Task 5.
fn identity_pub_for(device: &[u8; 32]) -> [u8; 64] {
    let mut ip = [0u8; 64];
    ip[32..64].copy_from_slice(device);
    ip
}

/// Build a `GroupDm` `Space` listing `members`. Mirrors the Task 5
/// `group_dm_space` helper (replicated here because that module is
/// `#[cfg(test)]`-private; this integration test compiles against the public
/// API).
fn group_dm_space(id: SpaceId, members: Vec<OwnerAddr>) -> Space {
    Space {
        id,
        kind: SpaceKind::GroupDm,
        parent: None,
        community_id: None,
        name: "Group".into(),
        transport: None,
        members,
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: hlc_seed(),
        updated_at: hlc_seed(),
        content_key: Some(DmContentKey::new([0xaa; 32])),
        prior_content_keys: vec![],
        current_epoch: None,
        current_epoch_key: None,
        old_epoch_keys: BTreeMap::new(),
        admin_addr: None,
        is_invite_only: None,
        shared_in_profile: false,
        read_receipt_pref: None,
        pending_join_at: None,
    }
}

/// Build an `OwnerState` holding a `GroupDm` space whose members are the
/// (sorted) owners of `enrolled`, with each member's device cached + enrolled so
/// `groupdm_beacon_signer_is_member` resolves for every `(owner, device)`.
fn state_with(space_id: SpaceId, enrolled: &[(OwnerAddr, [u8; 32])]) -> OwnerState {
    let mut members: Vec<OwnerAddr> = enrolled.iter().map(|(o, _)| *o).collect();
    members.sort();
    let mut os = OwnerState::default();
    os.spaces
        .insert(space_id, group_dm_space(space_id, members));
    for (owner, device) in enrolled {
        os.owner_device_cache.devices.insert(
            *owner,
            OwnerDeviceEntry {
                devices: vec![DeviceIdentityHash([0u8; 16])],
                device_identity_pubs: vec![Some(identity_pub_for(device))],
                learned_at: hlc_seed(),
                device_tunnel_contacts: vec![None],
            },
        );
    }
    os
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_dm_voice_three_engine_e2e() {
    // Outer guard so a hang surfaces as a failure, not an indefinite stall.
    tokio::time::timeout(Duration::from_secs(30), run_inner())
        .await
        .expect("group-DM voice three-engine test timed out");
}

async fn run_inner() {
    // ── Three real Zenoh sessions on the shared loopback peer router ─────────
    let cfg = zenoh::Config::default();
    let session_a = zenoh::open(cfg.clone()).await.expect("session A");
    let session_b = zenoh::open(cfg.clone()).await.expect("session B");
    let session_c = zenoh::open(cfg).await.expect("session C");

    // ── Three identities: owner + enrolled device key, consistently bound ────
    // mint_test_owner gives an OwnerAddr (master-derived) and a device signing
    // key whose verify key is what we enroll. The publisher signs beacons with
    // this device key and embeds owner.0 / device_vk, so the subscriber's
    // sig-verify AND membership check both line up.
    let seed_a = mint_test_owner(0xA1);
    let owner_a = seed_a.owner;
    let signing_a = Arc::new(seed_a.device_key);
    let device_a: [u8; 32] = signing_a.verifying_key().to_bytes();

    let seed_b = mint_test_owner(0xB2);
    let owner_b = seed_b.owner;
    let signing_b = Arc::new(seed_b.device_key);
    let device_b: [u8; 32] = signing_b.verifying_key().to_bytes();

    let seed_c = mint_test_owner(0xC3);
    let owner_c = seed_c.owner;
    let signing_c = Arc::new(seed_c.device_key);
    let device_c: [u8; 32] = signing_c.verifying_key().to_bytes();

    // ── Shared group: space_id, content key, call_id, derived keys ───────────
    let space_id = SpaceId([0x4d; 16]);
    let content_key = DmContentKey::new([0xAB; 32]);
    let call_id: [u8; 16] = [0xCD; 16];
    let k_voice = derive_dm_voice_key(&content_key, &call_id);
    let presence_key = Arc::new(derive_groupdm_presence_key(&content_key));

    // ── CRDT OwnerState for each subscribing engine (B and C): GroupDm space
    //    with members = [a, b, c] + each member's device enrolled ─────────────
    let enrolled = [
        (owner_a, device_a),
        (owner_b, device_b),
        (owner_c, device_c),
    ];
    let crdt_b = Arc::new(Mutex::new(state_with(space_id, &enrolled)));
    let crdt_c = Arc::new(Mutex::new(state_with(space_id, &enrolled)));

    // ── Shared map + injectable monotonic clock per subscribing engine ───────
    let map_b = Arc::new(Mutex::new(VoicePresenceMap::new()));
    let map_c = Arc::new(Mutex::new(VoicePresenceMap::new()));
    let clock = Arc::new(AtomicU64::new(0));
    let clock_for_now = Arc::clone(&clock);
    let now_ms: Arc<dyn Fn() -> u64 + Send + Sync> =
        Arc::new(move || clock_for_now.load(Ordering::SeqCst));

    // ZEB-445: the presence subscriber takes a mode-agnostic NodeEventSink;
    // this test asserts via VoicePresenceMap state, not emissions.
    let no_emit_sink: Arc<dyn harmony_app::node_event_sink::NodeEventSink> =
        Arc::new(harmony_app::node_event_sink::FanoutSink(vec![]));
    let closing = Arc::new(AtomicBool::new(false));

    let pres_topic = format!(
        "harmony/voice-presence/group-dm/{}",
        hex::encode(space_id.0),
    );

    // ── Spawn B's + C's REAL group-DM presence subscribers ───────────────────
    let sub_b = spawn_groupdm_presence_subscriber(
        session_b.clone(),
        pres_topic.clone(),
        Arc::clone(&presence_key),
        space_id,
        Arc::clone(&crdt_b),
        Arc::clone(&map_b),
        Arc::clone(&no_emit_sink),
        Arc::clone(&closing),
        Arc::clone(&now_ms),
    );
    let sub_c = spawn_groupdm_presence_subscriber(
        session_c.clone(),
        pres_topic.clone(),
        Arc::clone(&presence_key),
        space_id,
        Arc::clone(&crdt_c),
        Arc::clone(&map_c),
        Arc::clone(&no_emit_sink),
        Arc::clone(&closing),
        Arc::clone(&now_ms),
    );

    // Loopback subscriber declarations need ~1 s to settle + peers to discover
    // before the publishers start (same as the channel-messages template).
    tokio::time::sleep(Duration::from_secs(1)).await;

    // ── Spawn A/B/C's REAL group-DM presence publishers (4 s cadence; each
    //    fires immediately) ────────────────────────────────────────────────
    let joined_hlc_a = Hlc {
        wall_ms: 1_000,
        logical: 0,
        device_id: hex::encode(device_a),
    };
    let joined_hlc_b = Hlc {
        wall_ms: 1_000,
        logical: 0,
        device_id: hex::encode(device_b),
    };
    let joined_hlc_c = Hlc {
        wall_ms: 1_000,
        logical: 0,
        device_id: hex::encode(device_c),
    };

    let pub_a = spawn_groupdm_presence_publisher(
        session_a.clone(),
        pres_topic.clone(),
        Arc::clone(&presence_key),
        space_id,
        call_id,
        Arc::clone(&signing_a),
        owner_a,
        device_a,
        joined_hlc_a.clone(),
        Arc::new(AtomicBool::new(true)),
        Arc::new(AtomicU64::new(0)),
        Duration::from_secs(4),
        Arc::clone(&closing),
    );
    let pub_b = spawn_groupdm_presence_publisher(
        session_b.clone(),
        pres_topic.clone(),
        Arc::clone(&presence_key),
        space_id,
        call_id,
        Arc::clone(&signing_b),
        owner_b,
        device_b,
        joined_hlc_b.clone(),
        Arc::new(AtomicBool::new(true)),
        Arc::new(AtomicU64::new(0)),
        Duration::from_secs(4),
        Arc::clone(&closing),
    );
    let pub_c = spawn_groupdm_presence_publisher(
        session_c.clone(),
        pres_topic.clone(),
        Arc::clone(&presence_key),
        space_id,
        call_id,
        Arc::clone(&signing_c),
        owner_c,
        device_c,
        joined_hlc_c.clone(),
        Arc::new(AtomicBool::new(true)),
        Arc::new(AtomicU64::new(0)),
        Duration::from_secs(4),
        Arc::clone(&closing),
    );

    let call_chan = ChannelId(call_id);

    // ── Assertion 1: B's roster converges to all three (a, b, c) ─────────────
    wait_until(Duration::from_secs(10), || {
        let map_b = Arc::clone(&map_b);
        async move {
            let roster = map_b.lock().await.roster(&space_id, &call_chan);
            let has = |o: OwnerAddr| roster.iter().any(|r| r.owner == o.0);
            has(owner_a) && has(owner_b) && has(owner_c)
        }
    })
    .await
    .expect("B's roster should converge to all three members (a, b, c)");

    // ── Assertion 2: sealed media relay across three engines ─────────────────
    // A seals a frame under K_voice and publishes to its own-device DM media
    // topic; C opens it on the wildcard media topic and recovers the original.
    let call_id_hex = hex::encode(call_id);
    let device_a_hex = hex::encode(device_a);
    let media_topic_a = format!("harmony/voice/dm/{}/{}", call_id_hex, device_a_hex);
    let media_sub_key = format!("harmony/voice/dm/{}/*", call_id_hex);

    let media_sub_c = session_c
        .declare_subscriber(&media_sub_key)
        .await
        .expect("declare DM media subscriber on C");
    // ZEB-502: deterministically wait until A's session has discovered C's REMOTE
    // media subscriber before the one-shot put, rather than a fixed 1s settle. The
    // frame goes out live (no replay); a put that races C's subscriber declaration
    // is silently dropped and the recv loop below stalls to its 10s ceiling. A
    // concrete-key publisher on `…/{deviceA}` intersects C's wildcard subscriber, so
    // a `matching_status(Locality::Remote)` barrier is well-defined (mirrors
    // `voice_presence_two_engine_integration.rs`).
    {
        let media_ready_pub = session_a
            .declare_publisher(
                zenoh::key_expr::KeyExpr::try_from(media_topic_a.clone()).expect("media topic key"),
            )
            .allowed_destination(zenoh::sample::Locality::Remote)
            .await
            .expect("declare media readiness publisher");
        // 10s is far above the sub-second loopback subscriber-discovery time but
        // comfortably under the test's 30s outer timeout, so a genuine discovery
        // failure surfaces as the informative assertion below rather than the
        // less-specific outer guard (and leaves budget for the 10s recv).
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
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
                "A never matched C's remote media subscriber within 10s"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    let original_frame: Vec<u8> = (0u8..40).collect();
    let sealed_frame =
        encrypt_dm_voice_packet(&k_voice, &call_id, VOICE_DM_PACKET_AAD, &original_frame)
            .expect("seal DM media frame");
    session_a
        .put(&media_topic_a, sealed_frame)
        .await
        .expect("publish sealed DM media frame from A");

    let recovered = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let sample = media_sub_c.recv_async().await.expect("DM media recv on C");
            let bytes = sample.payload().to_bytes().to_vec();
            if let Ok(frame) =
                decrypt_dm_voice_packet(&k_voice, &call_id, VOICE_DM_PACKET_AAD, &bytes)
            {
                break frame;
            }
        }
    })
    .await
    .expect("C should receive + open the sealed DM media frame relayed from A");
    assert_eq!(
        recovered, original_frame,
        "recovered DM media frame must equal the original (3-engine relay)"
    );
    drop(media_sub_c);

    // ── Assertion 6 (negative — non-member beacon dropped) ───────────────────
    // A rogue identity (seed 0x55) NOT in `members` self-signs a valid beacon,
    // wraps it with the right call_id, and seals it under the SAME presence key.
    // It opens cleanly and its signature verifies — the ONLY thing that can stop
    // it is `groupdm_beacon_signer_is_member`. Publish it on the live presence
    // topic and assert B's roster NEVER includes the rogue owner. Done BEFORE the
    // leave assertions so the publishers are still beaconing (proves the
    // subscriber is alive, so the rogue's absence is a real gate rejection).
    let seed_r = mint_test_owner(0x55);
    let owner_r = seed_r.owner;
    let signing_r = Arc::new(seed_r.device_key);
    let device_r: [u8; 32] = signing_r.verifying_key().to_bytes();
    // Sanity: the rogue must not collide with a real member.
    assert!(
        owner_r != owner_a && owner_r != owner_b && owner_r != owner_c,
        "rogue identity must be distinct from the three members"
    );
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
    let wrapped_rogue = GroupSignedPresenceBeacon {
        call_id,
        signed: signed_rogue,
    };
    let sealed_rogue = seal_groupdm_presence_beacon(&presence_key, &space_id, &wrapped_rogue)
        .expect("seal rogue beacon under the real presence key");
    session_a
        .put(&pres_topic, sealed_rogue)
        .await
        .expect("publish rogue beacon");

    // Give the subscriber the same settle window a real beacon gets, then assert
    // the rogue never appears while the three members are still present.
    tokio::time::sleep(Duration::from_secs(2)).await;
    {
        let roster = map_b.lock().await.roster(&space_id, &call_chan);
        assert!(
            !roster.iter().any(|r| r.owner == owner_r.0),
            "a non-member (rogue) signer must be gated out by groupdm_beacon_signer_is_member"
        );
        assert!(
            roster.iter().any(|r| r.owner == owner_a.0),
            "A must still be present — proves the subscriber is alive and processing, \
             so the rogue's absence is a real gate rejection, not a stalled loop"
        );
    }

    // ── Assertion 5 (negative — wrong call_id key) ───────────────────────────
    // A frame sealed under K_voice for a DIFFERENT call_id must fail to open
    // under the real K_voice (per-call HKDF key binding).
    let k_voice_other = derive_dm_voice_key(&content_key, &[0xEE; 16]);
    let frame2: Vec<u8> = (0u8..20).collect();
    let sealed_other =
        encrypt_dm_voice_packet(&k_voice_other, &[0xEE; 16], VOICE_DM_PACKET_AAD, &frame2)
            .expect("seal frame under the other call_id key");
    let open_wrong =
        decrypt_dm_voice_packet(&k_voice, &call_id, VOICE_DM_PACKET_AAD, &sealed_other);
    assert_eq!(
        open_wrong,
        Err(VoiceCryptoError::OpenFailed),
        "a frame sealed under a different call_id's K_voice must FAIL to open under the real K_voice"
    );

    // ── Assertion 3: leave tombstone drops C from the roster ─────────────────
    // Stop C's publisher first so it can't re-add C after the tombstone, then
    // publish C's leave tombstone. B's roster must drop to {a, b}.
    pub_c.abort();
    publish_groupdm_leave_tombstone(
        &session_c,
        &pres_topic,
        &presence_key,
        &space_id,
        call_id,
        &signing_c,
        owner_c,
        device_c,
        &joined_hlc_c,
    )
    .await;

    wait_until(Duration::from_secs(10), || {
        let map_b = Arc::clone(&map_b);
        async move {
            let roster = map_b.lock().await.roster(&space_id, &call_chan);
            let has = |o: OwnerAddr| roster.iter().any(|r| r.owner == o.0);
            !has(owner_c) && has(owner_a) && has(owner_b)
        }
    })
    .await
    .expect("C's leave tombstone should drop C from B's roster (leaving a, b)");

    // ── Assertion 4: last-leave clears B's roster ────────────────────────────
    // Stop A and B's publishers, then tombstone both. B's roster must empty.
    pub_a.abort();
    pub_b.abort();
    publish_groupdm_leave_tombstone(
        &session_a,
        &pres_topic,
        &presence_key,
        &space_id,
        call_id,
        &signing_a,
        owner_a,
        device_a,
        &joined_hlc_a,
    )
    .await;
    publish_groupdm_leave_tombstone(
        &session_b,
        &pres_topic,
        &presence_key,
        &space_id,
        call_id,
        &signing_b,
        owner_b,
        device_b,
        &joined_hlc_b,
    )
    .await;

    wait_until(Duration::from_secs(10), || {
        let map_b = Arc::clone(&map_b);
        async move { map_b.lock().await.roster(&space_id, &call_chan).is_empty() }
    })
    .await
    .expect("all three leave tombstones should empty B's roster (last-leave clears)");

    // ── Teardown ─────────────────────────────────────────────────────────────
    closing.store(true, Ordering::SeqCst);
    sub_b.abort();
    sub_c.abort();
    // `map_c` is wired through C's subscriber for a complete 3-engine topology;
    // keep the binding live until teardown.
    let _ = &map_c;
}
