//! ZEB-298+ZEB-312 PR 1: end-to-end test of voting outbound→inbound
//! through TWO real `zenoh::Session` instances (not the mpsc test
//! bridge). Verifies that a peer-delivered voting event arrives via
//! the Zenoh subscriber, passes through `verify_voting_event` on the
//! receiving engine, and applies to its `VotingLog`.
//!
//! Why this test exists: the mpsc-bridged tests prove the engine's
//! pub/sub channels work, but they cannot verify the Zenoh transport
//! layer itself (key_expr parsing, session.put, declare_subscriber,
//! recv_async). This is the missing end-to-end coverage that proves
//! voting events actually cross the wire.

#![cfg(feature = "test-fixtures")]

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use async_trait::async_trait;
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
use harmony_app::community_voting_log_engine::{VotingLogEngine, VotingLogEngineParams};
use harmony_app::event_loop::spawn_voting_log_zenoh_adapter;
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};

/// Build a `(SigningKey, OwnerAddr, [u8; 64])` triple from a single-byte seed.
/// The returned `owner`'s `address_hash` is derived from the public key bytes —
/// the same binding enforced by `verify_voting_event`'s defense-in-depth check.
fn fixture_identity(seed: u8) -> (ed25519_dalek::SigningKey, OwnerAddr, [u8; 64]) {
    let priv_id = harmony_identity::PrivateIdentity::from_seed(&[seed; 32]);
    let owner = OwnerAddr(priv_id.identity.address_hash);
    let pub_64 = priv_id.identity.to_public_bytes();
    let private_bytes = priv_id.to_private_bytes();
    let mut ed_secret = [0u8; 32];
    ed_secret.copy_from_slice(&private_bytes[32..64]);
    let signing = ed25519_dalek::SigningKey::from_bytes(&ed_secret);
    (signing, owner, pub_64)
}

/// Test resolvers that satisfy both VotingIdentityResolver and
/// MembershipSnapshotResolver from a single Arc — same pattern used in
/// the production-build process_inbound test.
struct FixedResolvers {
    identity: HashMap<OwnerAddr, [u8; 64]>,
    snapshot: MembershipSnapshot,
}

#[async_trait]
impl VotingIdentityResolver for FixedResolvers {
    async fn resolve(&self, owner: &OwnerAddr) -> Option<[u8; 64]> {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn voting_event_flows_through_two_zenoh_sessions() {
    // Open two default-config peer sessions — they discover each other
    // via in-memory gossip.
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A open"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B open"));

    let community_id = SpaceId([0xab; 16]);
    let community_id_hex = hex::encode(community_id.0);

    // ZEB-717: the voting adapters encrypt/decrypt at the wire boundary using
    // the community's live epoch key. A + B share one crdt_state (same key,
    // epoch 0) — the realistic post-enrollment state — so events round-trip.
    let crdt_state = Arc::new(Mutex::new({
        let mut os = harmony_app::owner_state_crdt::OwnerState::default();
        os.spaces.insert(
            community_id,
            harmony_app::community_state_sync::test_community_space(
                community_id,
                0,
                EpochKey::new([0x11; 32]),
            ),
        );
        os
    }));

    // Build the peer event using a bound fixture identity.
    let (keypair, actor, pub_64) = fixture_identity(0xcd);

    let resolvers = Arc::new(FixedResolvers {
        identity: HashMap::from([(actor, pub_64)]),
        snapshot: MembershipSnapshot {
            members: HashMap::from([(
                actor,
                MemberAttrs {
                    power: 1,
                    vouching_depth: 1,
                },
            )]),
        },
    });
    let id_resolver: Arc<dyn VotingIdentityResolver> = resolvers.clone();
    let mem_resolver: Arc<dyn MembershipSnapshotResolver> = resolvers.clone();

    // ── Engine A side (publisher only) ──────────────────────────────────
    // We only need session A to publish to the topic via the production
    // adapter. Engine A doesn't hold a real engine — we just drive
    // a_pub_tx from the test body.
    let closing_a = Arc::new(AtomicBool::new(false));
    let (a_pub_tx, a_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    // Engine A doesn't need to receive inbound; give it a dummy subscriber_tx.
    let (a_sub_tx_unused, _a_sub_rx_unused) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let _adapter_a_handle = spawn_voting_log_zenoh_adapter(
        Arc::clone(&session_a),
        community_id_hex.clone(),
        community_id,
        Arc::clone(&crdt_state),
        a_pub_rx,
        a_sub_tx_unused,
        Arc::clone(&closing_a),
    );

    // ── Engine B side (subscriber + engine) ─────────────────────────────
    // The adapter on session_b subscribes to the topic and forwards
    // received packets → b_sub_tx → engine B's inbound receive loop,
    // which calls verify_voting_event then applies on success.
    let closing_b = Arc::new(AtomicBool::new(false));
    let (b_pub_tx, b_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let (b_sub_tx, b_sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let _adapter_b_handle = spawn_voting_log_zenoh_adapter(
        Arc::clone(&session_b),
        community_id_hex.clone(),
        community_id,
        Arc::clone(&crdt_state),
        b_pub_rx,
        b_sub_tx,
        Arc::clone(&closing_b),
    );

    // Warm-up: poll until Zenoh peer-discovery + subscriber declaration are
    // complete before publishing the real event. A fixed sleep is fragile
    // on resource-constrained CI (Greptile P2). Instead, we publish a
    // sentinel packet that the engine will silently reject (malformed CBOR),
    // then retry the real publish in a poll loop. The 3-second poll at the
    // bottom already tolerates apply latency; this warm-up ensures the
    // first real publish doesn't fire into an unwired subscriber.
    //
    // Strategy: publish a "warm-up" byte over a_pub_tx repeatedly
    // (up to 20 × 100ms = 2s total) until Zenoh has had time to wire the
    // peer link. We then publish the real event below. The engine on the
    // B side silently drops the malformed bytes (decode error logged as
    // warn, not fatal) and continues. The real event is published after
    // the warm-up window, before the 3-second poll loop.
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // Ignore send errors — the adapter may not be up yet.
        let _ = a_pub_tx.send(b"WARMUP".to_vec()).await;
    }

    // Engine B: real VotingLogEngine that consumes b_sub_rx, calls
    // verify_voting_event, and applies on success.
    let log_b = Arc::new(Mutex::new(VotingLog::default()));
    let _engine_b = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
        community_id,
        voting_log: Arc::clone(&log_b),
        publisher_tx: b_pub_tx,
        subscriber_rx: b_sub_rx,
        hlc_tracker: None,
        device_id: None,
        app_handle: None,
        identity_resolver: Some(id_resolver),
        membership_resolver: Some(mem_resolver),
    })
    .await;

    // ── Mint a Tier 1 PollCreate event and push it via engine A ─────────
    let cfg_poll = Tier1PollConfig {
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
        channel_id: ChannelId([0xef; 16]),
    };
    let event_hlc = Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 0,
        device_id: "peer-a".into(),
    };
    let event = build_signed_poll_create_tier1(&keypair, actor, &cfg_poll, event_hlc)
        .expect("build poll create");

    let mut packet = Vec::new();
    ciborium::ser::into_writer(&event, &mut packet).expect("encode");

    // Push packet through engine A's outbound channel → adapter A →
    // session_a.put → in-memory peer link → session_b subscriber →
    // adapter B → b_sub_rx → engine B's inbound loop → verify+apply.
    a_pub_tx.send(packet).await.expect("push to a_pub_tx");

    // ── Wait for engine B to apply the event (poll up to 3 seconds) ─────
    let sb = event.signing_bytes().expect("signing_bytes");
    let pid = derive_poll_id(&community_id, &sb);

    let mut applied = false;
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let log = log_b.lock().await;
        if log.has_poll(&pid) {
            applied = true;
            break;
        }
    }

    assert!(
        applied,
        "engine B must apply the peer event via Zenoh within 3s"
    );

    // ── Clean shutdown ───────────────────────────────────────────────────
    closing_a.store(true, std::sync::atomic::Ordering::SeqCst);
    closing_b.store(true, std::sync::atomic::Ordering::SeqCst);
    drop(_engine_b);
}

/// ZEB-717 acceptance criterion: a member kicked at the N→N+1 epoch rotation,
/// holding only the stale epoch-N key, cannot inject a voting event — even
/// though the receiver still RETAINS K(N) in `old_epoch_keys`. The transport's
/// current-epoch-only cut drops the stale-epoch envelope before the engine ever
/// sees it. Verify-layer membership can't distinguish this (the injection is a
/// validly-signed member event with a backdated HLC); the transport is the only
/// gate that can, and it must key on the CURRENT epoch, not merely on whether
/// the receiver holds the key.
///
/// Control (second phase): once the "kicked" node rotates its own view to
/// epoch N+1 with the current key, a same-shaped event DOES apply on B — proving
/// the earlier drop was epoch-specific, not a broken pipe.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kicked_then_rotated_member_injection_is_dropped() {
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A open"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B open"));

    let community_id = SpaceId([0xac; 16]);
    let community_id_hex = hex::encode(community_id.0);

    let k0 = EpochKey::new([0xa0; 32]); // epoch-0 key the kicked member still holds
    let k1 = EpochKey::new([0xb1; 32]); // epoch-1 key installed by the rotation

    // Kicked member A: stuck at epoch 0 (only holds K0).
    let crdt_state_a = Arc::new(Mutex::new({
        let mut os = harmony_app::owner_state_crdt::OwnerState::default();
        os.spaces.insert(
            community_id,
            harmony_app::community_state_sync::test_community_space(community_id, 0, k0.clone()),
        );
        os
    }));

    // Receiver B: rotated to epoch 1, but RETAINS K0 in old_epoch_keys — the
    // whole point is that key retention alone must NOT admit the stale injection.
    let crdt_state_b = Arc::new(Mutex::new({
        let mut os = harmony_app::owner_state_crdt::OwnerState::default();
        let mut space =
            harmony_app::community_state_sync::test_community_space(community_id, 1, k1.clone());
        space.old_epoch_keys.insert(0, k0.clone());
        os.spaces.insert(community_id, space);
        os
    }));

    // Actor is a valid member in B's snapshot, so verify_voting_event passes for
    // BOTH the stale and the control event — only the transport epoch differs.
    let (keypair, actor, pub_64) = fixture_identity(0xcd);
    let resolvers = Arc::new(FixedResolvers {
        identity: HashMap::from([(actor, pub_64)]),
        snapshot: MembershipSnapshot {
            members: HashMap::from([(
                actor,
                MemberAttrs {
                    power: 1,
                    vouching_depth: 1,
                },
            )]),
        },
    });
    let id_resolver: Arc<dyn VotingIdentityResolver> = resolvers.clone();
    let mem_resolver: Arc<dyn MembershipSnapshotResolver> = resolvers.clone();

    // Adapter A (publisher) — encrypts under the kicked member's stale epoch view.
    let closing_a = Arc::new(AtomicBool::new(false));
    let (a_pub_tx, a_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let (a_sub_tx_unused, _a_sub_rx_unused) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let _adapter_a = spawn_voting_log_zenoh_adapter(
        Arc::clone(&session_a),
        community_id_hex.clone(),
        community_id,
        Arc::clone(&crdt_state_a),
        a_pub_rx,
        a_sub_tx_unused,
        Arc::clone(&closing_a),
    );

    // Adapter B (subscriber) + engine B.
    let closing_b = Arc::new(AtomicBool::new(false));
    let (b_pub_tx, b_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let (b_sub_tx, b_sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let _adapter_b = spawn_voting_log_zenoh_adapter(
        Arc::clone(&session_b),
        community_id_hex.clone(),
        community_id,
        Arc::clone(&crdt_state_b),
        b_pub_rx,
        b_sub_tx,
        Arc::clone(&closing_b),
    );

    // Warm up the Zenoh peer link (encrypted-then-dropped WARMUP bytes are fine —
    // they're epoch-0 envelopes that B's gate drops, but the put/subscribe
    // handshake still establishes the link).
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = a_pub_tx.send(b"WARMUP".to_vec()).await;
    }

    let log_b = Arc::new(Mutex::new(VotingLog::default()));
    let _engine_b = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
        community_id,
        voting_log: Arc::clone(&log_b),
        publisher_tx: b_pub_tx,
        subscriber_rx: b_sub_rx,
        hlc_tracker: None,
        device_id: None,
        app_handle: None,
        identity_resolver: Some(id_resolver),
        membership_resolver: Some(mem_resolver),
    })
    .await;

    let make_event = |wall_ms: u64, dev: &str| {
        let cfg_poll = Tier1PollConfig {
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
            channel_id: ChannelId([0xef; 16]),
        };
        let hlc = Hlc {
            wall_ms,
            logical: 0,
            device_id: dev.into(),
        };
        build_signed_poll_create_tier1(&keypair, actor, &cfg_poll, hlc).expect("build poll create")
    };

    // ── Phase 1: STALE injection (A encrypts under epoch 0) — must be dropped ──
    let stale = make_event(1_700_000_000_000, "kicked-a");
    let stale_pid = derive_poll_id(&community_id, &stale.signing_bytes().expect("sb"));
    let mut pkt = Vec::new();
    ciborium::ser::into_writer(&stale, &mut pkt).expect("encode");
    a_pub_tx.send(pkt).await.expect("push stale");

    // The link is warm, so 1.5s is ample: if it were going to apply it would
    // have. It must NOT — epoch-0 envelope vs B.current_epoch = 1.
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !log_b.lock().await.has_poll(&stale_pid),
            "stale-epoch injection from a kicked member was applied — ZEB-717 cut failed"
        );
    }

    // ── Phase 2: CONTROL — A rotates its own view to epoch 1 + K1; must apply ──
    {
        let mut g = crdt_state_a.lock().await;
        let space = g.spaces.get_mut(&community_id).expect("space present");
        space.current_epoch = Some(1);
        space.current_epoch_key = Some(k1.clone());
    }
    let control = make_event(1_700_000_001_000, "member-a");
    let control_pid = derive_poll_id(&community_id, &control.signing_bytes().expect("sb"));
    let mut pkt2 = Vec::new();
    ciborium::ser::into_writer(&control, &mut pkt2).expect("encode");
    a_pub_tx.send(pkt2).await.expect("push control");

    let mut applied = false;
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if log_b.lock().await.has_poll(&control_pid) {
            applied = true;
            break;
        }
    }
    assert!(
        applied,
        "current-epoch event must apply on B — the earlier drop was epoch-specific, not a broken pipe"
    );

    closing_a.store(true, std::sync::atomic::Ordering::SeqCst);
    closing_b.store(true, std::sync::atomic::Ordering::SeqCst);
    drop(_engine_b);
}
