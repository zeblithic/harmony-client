//! ZEB-1018: end-to-end test of the D-FROST committee-log plane through
//! TWO real `zenoh::Session` instances (not the mpsc byte-relay bridge).
//!
//! `community_dfrost_transport_integration.rs` proved the full 2-of-2 DKG
//! across the engine boundary with in-test forwarder glue, deliberately
//! deferring the Zenoh adapter ("reduced to swap-in byte-relay glue").
//! This file tests that swap-in: `spawn_dfrost_log_zenoh_adapter`'s
//! key_expr, epoch-encryption (`DFROST_TOPIC_AAD`), session.put,
//! declare_subscriber, and the current-epoch-only decrypt cut.
//!
//! Covered:
//! 1. A signed `dr` rn=1 event published via engine A's `publish_event`
//!    crosses two real sessions and applies on engine B (verify → dedup →
//!    apply chain, exactly the production path).
//! 2. Epoch containment: an envelope sealed under a different epoch is
//!    dropped at B's adapter before any engine work — then the SAME event
//!    delivered under the correct epoch applies, proving the earlier
//!    non-delivery was the gate and not a dead pipe.

#![cfg(feature = "test-fixtures")]

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use tokio::sync::Mutex;

use harmony_app::community_dfrost_crypto::{dkg_part1_local, identifier_for_index};
use harmony_app::community_dfrost_log::{
    build_signed_dfrost_event, CommitteeState, DfrostLog, PendingCeremony,
};
use harmony_app::community_dfrost_log_engine::{DfrostLogEngine, DfrostLogEngineParams};
use harmony_app::community_dfrost_types::{
    DfrostEventKind, DkgCompletePayload, DkgRoundPayload, MemberVerifyingShare,
};
use harmony_app::community_state_sync::IdentityResolver;
use harmony_app::dm_signing;
use harmony_app::event_loop::{spawn_dfrost_log_zenoh_adapter, DfrostCatchupHooks};
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};

/// Build a `(SigningKey, OwnerAddr, [u8; 64])` triple from a single-byte
/// seed with the address-hash binding `verify_signed_committee_event`
/// enforces. Same helper shape as the transport integration test.
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

struct StaticResolver(HashMap<OwnerAddr, [u8; 64]>);

#[async_trait::async_trait]
impl IdentityResolver for StaticResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        self.0.get(addr).copied()
    }
}

fn hlc_at(wall_ms: u64, device_id: &str) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: device_id.into(),
    }
}

/// Poll `predicate(log)` at 10ms intervals up to `secs`. Mirrors the
/// transport test's `wait_for` (sleep-based waits are CI-fragile).
///
/// ZEB-1030 PR#778 round-1: `hint`, when given, is notified on every
/// iteration — nudging a requester task parked in its hint-or-interval
/// wait to retry its GET immediately rather than sleep out the full
/// `DFROST_CATCHUP_INTERVAL` (300s, far past this helper's bounded
/// deadline). Self-heals an early miss caused by queryable-discovery lag
/// surviving past the fixed warm-up sleep, so the deadline below — not
/// the warm-up sleep — is what determinism actually rests on.
async fn wait_for<F>(
    label: &str,
    log: &Arc<Mutex<DfrostLog>>,
    secs: u64,
    hint: Option<&Arc<tokio::sync::Notify>>,
    mut predicate: F,
) where
    F: FnMut(&DfrostLog) -> bool,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        {
            let guard = log.lock().await;
            if predicate(&guard) {
                return;
            }
        }
        if let Some(h) = hint {
            h.notify_one();
        }
        if std::time::Instant::now() >= deadline {
            panic!("wait_for({label}) timed out after {secs}s");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dfrost_event_flows_through_two_zenoh_sessions_with_epoch_gate() {
    // ── Identities + resolvers (each side must resolve BOTH peers) ──────
    let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0xA7);
    let (bob_sk, bob_addr, bob_pub64) = fixture_identity(0xB7);
    let alice_x_priv = *dm_signing::ed25519_priv_to_x25519(&alice_sk);
    let bob_x_priv = *dm_signing::ed25519_priv_to_x25519(&bob_sk);

    let mut resolver_map = HashMap::new();
    resolver_map.insert(alice_addr, alice_pub64);
    resolver_map.insert(bob_addr, bob_pub64);
    let alice_resolver: Arc<dyn IdentityResolver + Send + Sync> =
        Arc::new(StaticResolver(resolver_map.clone()));
    let bob_resolver: Arc<dyn IdentityResolver + Send + Sync> =
        Arc::new(StaticResolver(resolver_map));

    let mut members = vec![alice_addr, bob_addr];
    members.sort();
    let id_alice = identifier_for_index(members.iter().position(|a| *a == alice_addr).unwrap());
    let id_bob = identifier_for_index(members.iter().position(|a| *a == bob_addr).unwrap());
    let threshold: u16 = 2;
    let max_signers: u16 = 2;

    // ── Two real Zenoh sessions + the shared epoch-key state ────────────
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A open"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B open"));

    let community_id = SpaceId([0xD7; 16]);
    let community_id_hex = hex::encode(community_id.0);

    // A + B share one crdt_state (same key, epoch 0) — the realistic
    // post-enrollment state — so envelopes round-trip.
    let crdt_state = Arc::new(Mutex::new({
        let mut os = harmony_app::owner_state_crdt::OwnerState::default();
        os.spaces.insert(
            community_id,
            harmony_app::community_state_sync::test_community_space(
                community_id,
                0,
                EpochKey::new([0x33; 32]),
            ),
        );
        os
    }));

    // ── Engine + adapter per side ───────────────────────────────────────
    let closing = Arc::new(AtomicBool::new(false));

    let (alice_pub_tx, alice_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (alice_sub_tx, alice_sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let _adapter_a = spawn_dfrost_log_zenoh_adapter(
        Arc::clone(&session_a),
        community_id_hex.clone(),
        community_id,
        Arc::clone(&crdt_state),
        alice_pub_rx,
        alice_sub_tx,
        None,
        Arc::clone(&closing),
    );

    let (bob_pub_tx, bob_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (bob_sub_tx, bob_sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let _adapter_b = spawn_dfrost_log_zenoh_adapter(
        Arc::clone(&session_b),
        community_id_hex.clone(),
        community_id,
        Arc::clone(&crdt_state),
        bob_pub_rx,
        bob_sub_tx,
        None,
        Arc::clone(&closing),
    );

    let alice_log = Arc::new(Mutex::new(DfrostLog::new()));
    let bob_log = Arc::new(Mutex::new(DfrostLog::new()));

    let alice_engine = DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
        community_id,
        dfrost_log: alice_log.clone(),
        publisher_tx: alice_pub_tx.clone(),
        subscriber_rx: alice_sub_rx,
        app_handle: None,
        self_addr: alice_addr,
        self_x25519_priv: alice_x_priv,
        identity_resolver: alice_resolver,
        registry_weak: None,
        driver: None,
        membership_resolver: None,
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    let _bob_engine = DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
        community_id,
        dfrost_log: bob_log.clone(),
        publisher_tx: bob_pub_tx,
        subscriber_rx: bob_sub_rx,
        app_handle: None,
        self_addr: bob_addr,
        self_x25519_priv: bob_x_priv,
        identity_resolver: bob_resolver,
        registry_weak: None,
        driver: None,
        membership_resolver: None,
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    // ── Warm-up: wait out Zenoh peer discovery + subscriber declaration ─
    // Same strategy as the voting zenoh test: push garbage plaintext that
    // B's engine silently rejects at CBOR decode; once the link is wired
    // these arrive, but arrival is not observable — the loop is purely a
    // bounded time-buffer, so keep it short and rely on the wait_for
    // polls below for correctness.
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = alice_pub_tx.send(b"WARMUP".to_vec()).await;
    }

    // ── Phase 1: Alice's dr rn=1 crosses the wire and applies on Bob ────
    let ceremony_id: [u8; 32] = blake3::hash(b"zeb1018-zenoh-ceremony").into();
    let dr1_alice = {
        let mut log = alice_log.lock().await;
        log.committee_state.pending_dkg = Some(PendingCeremony {
            ceremony_id,
            members: members.clone(),
            threshold,
            max_signers,
            proposed_epoch: 1,
            ..Default::default()
        });
        let (r1_secret, r1_pkg_bytes) =
            dkg_part1_local(id_alice, max_signers, threshold).expect("alice dkg_part1");
        log.local_dkg_secret = Some(r1_secret);
        let payload = DkgRoundPayload {
            ceremony_id,
            round_num: 1,
            round1_package: Some(r1_pkg_bytes),
            recipient_ciphertexts: None,
        };
        let event = build_signed_dfrost_event(
            &alice_sk,
            alice_addr,
            DfrostEventKind::DkgRound,
            &payload,
            hlc_at(1_000, "alice-dev"),
        )
        .expect("build alice dr rn=1");
        log.apply_with_identity(event.clone(), &alice_addr, &alice_x_priv)
            .expect("alice applies own dr rn=1");
        event
    };

    // Seed Bob's pending ceremony (in production this arrives via the
    // initiator's ceremony-bootstrap; same explicit seed as the transport
    // integration test).
    {
        let mut log = bob_log.lock().await;
        log.committee_state.pending_dkg = Some(PendingCeremony {
            ceremony_id,
            members: members.clone(),
            threshold,
            max_signers,
            proposed_epoch: 1,
            ..Default::default()
        });
    }

    alice_engine
        .publish_event(dr1_alice)
        .await
        .expect("alice publishes dr rn=1");
    wait_for(
        "bob applies alice's dr rn=1 via zenoh",
        &bob_log,
        15,
        None,
        |log| {
            log.committee_state
                .pending_dkg
                .as_ref()
                .map(|p| p.round1_packages.contains_key(&alice_addr))
                .unwrap_or(false)
        },
    )
    .await;

    // ── Phase 2: a stale-epoch envelope is dropped at B's adapter ───────
    // A second publisher adapter on session A whose state carries epoch 1
    // with a different key. B (epoch 0) must drop its envelopes at the
    // current-epoch cut, before decrypt or any engine work.
    let stale_state = Arc::new(Mutex::new({
        let mut os = harmony_app::owner_state_crdt::OwnerState::default();
        os.spaces.insert(
            community_id,
            harmony_app::community_state_sync::test_community_space(
                community_id,
                1,
                EpochKey::new([0x44; 32]),
            ),
        );
        os
    }));
    let (stale_pub_tx, stale_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (stale_sub_tx_unused, _stale_sub_rx_unused) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let _adapter_stale = spawn_dfrost_log_zenoh_adapter(
        Arc::clone(&session_a),
        community_id_hex.clone(),
        community_id,
        stale_state,
        stale_pub_rx,
        stale_sub_tx_unused,
        None,
        Arc::clone(&closing),
    );

    // Bob's own rn=1 contribution, built WITHOUT a local apply so
    // `round1_packages.contains_key(&bob_addr)` is the delivery marker.
    let dr1_bob = {
        let (_r1_secret, r1_pkg_bytes) =
            dkg_part1_local(id_bob, max_signers, threshold).expect("bob dkg_part1");
        let payload = DkgRoundPayload {
            ceremony_id,
            round_num: 1,
            round1_package: Some(r1_pkg_bytes),
            recipient_ciphertexts: None,
        };
        build_signed_dfrost_event(
            &bob_sk,
            bob_addr,
            DfrostEventKind::DkgRound,
            &payload,
            hlc_at(2_000, "bob-dev"),
        )
        .expect("build bob dr rn=1")
    };

    // Encode exactly as publish_event would and push through the
    // stale-epoch adapter.
    let mut dr1_bob_bytes = Vec::new();
    ciborium::ser::into_writer(&dr1_bob, &mut dr1_bob_bytes).expect("encode bob dr rn=1");
    stale_pub_tx
        .send(dr1_bob_bytes)
        .await
        .expect("send via stale adapter");

    // Grace period: if the epoch gate were broken, the event would verify
    // (bob is resolvable) and apply (the ceremony is pending). Bounded
    // negative check, then a positive control below.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    {
        let log = bob_log.lock().await;
        assert!(
            !log.committee_state
                .pending_dkg
                .as_ref()
                .map(|p| p.round1_packages.contains_key(&bob_addr))
                .unwrap_or(false),
            "stale-epoch envelope must be dropped at the adapter's epoch cut"
        );
    }

    // ── Phase 3: positive control — the SAME event under the good epoch ─
    // Published via Alice's engine (the envelope carries the current
    // epoch), it must now apply. This proves the pipe was live and the
    // phase-2 non-delivery was the gate, not a dead subscriber.
    alice_engine
        .publish_event(dr1_bob)
        .await
        .expect("re-publish bob's dr rn=1 under the current epoch");
    wait_for(
        "bob's dr rn=1 applies under the good epoch",
        &bob_log,
        15,
        None,
        |log| {
            log.committee_state
                .pending_dkg
                .as_ref()
                .map(|p| p.round1_packages.contains_key(&bob_addr))
                .unwrap_or(false)
        },
    )
    .await;
}

/// ZEB-1030 final-review — Ruling 2 residual / Zenoh round-trip proof.
///
/// `dfrost_event_flows_through_two_zenoh_sessions_with_epoch_gate` above
/// passes `None` for `catchup_hooks` on both adapters, so nothing
/// anywhere exercised the catch-up transport wiring —
/// `declare_queryable`, the GET params, the reply loop, and the
/// requester open-loop — over a real `zenoh::Session`; only the
/// engine-level `catchup_respond` → (in-test byte handoff) →
/// `catchup_ingest` path was proven
/// (`community_dfrost_integration.rs::fresh_joiner_adopts_committee_state_zeb1030`).
///
/// This test wires `Some(DfrostCatchupHooks)` on both sides, mirroring
/// exactly how `ensure_dfrost_engine_for` (`lib.rs`) builds the hooks
/// over a shared engine `Arc`, and asserts a fresh joiner's periodic
/// requester (which fires its first attempt immediately on spawn, no
/// wait) completes one real GET → reply round over the live session and
/// adopts the responder's committee state. Alice's committee is seeded
/// directly (not via a real DKG ceremony) — `adopt_initial_quorum`
/// checks only structural agreement across `dk` events (payload
/// identity, membership shape, share-map 1:1), never a FROST proof
/// (see its `adopt_initial_quorum_happy_path_zeb1030` unit test), so a
/// hand-built but internally-consistent payload is sufficient and the
/// scaffolding stays proportional to what this test needs to prove:
/// that the BYTES cross the real session end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dfrost_catchup_round_crosses_real_zenoh_session_zeb1030() {
    // ── Identities + resolvers (each side must resolve BOTH peers) ──────
    let (alice_sk, alice_addr, alice_pub64) = fixture_identity(0xC1);
    let (bob_sk, bob_addr, bob_pub64) = fixture_identity(0xC2);
    let alice_x_priv = *dm_signing::ed25519_priv_to_x25519(&alice_sk);

    let mut resolver_map = HashMap::new();
    resolver_map.insert(alice_addr, alice_pub64);
    resolver_map.insert(bob_addr, bob_pub64);
    let alice_resolver: Arc<dyn IdentityResolver + Send + Sync> =
        Arc::new(StaticResolver(resolver_map.clone()));
    let joiner_resolver: Arc<dyn IdentityResolver + Send + Sync> =
        Arc::new(StaticResolver(resolver_map));

    let mut members = vec![alice_addr, bob_addr];
    members.sort();

    // ── Alice's log: an ACTIVE committee at epoch 1, seeded with two
    // real Ed25519-signed `dk` confirmations (`catchup_respond`'s served
    // events DO go through `verify_signed_committee_event`, unlike the
    // FROST material itself).
    let joint_vk = [0xAB; 32];
    let verifying_shares = vec![
        MemberVerifyingShare {
            member: alice_addr,
            verifying_share: [0x11; 32],
        },
        MemberVerifyingShare {
            member: bob_addr,
            verifying_share: [0x22; 32],
        },
    ];
    let dk_payload = DkgCompletePayload {
        ceremony_id: [0xCE; 32],
        joint_verifying_key: joint_vk,
        verifying_shares: verifying_shares.clone(),
        epoch: 1,
        members: members.clone(),
        threshold: 2,
        max_signers: 2,
    };
    let dk_alice = build_signed_dfrost_event(
        &alice_sk,
        alice_addr,
        DfrostEventKind::DkgComplete,
        &dk_payload,
        hlc_at(1_000, "alice-dev"),
    )
    .expect("sign alice dk");
    let dk_bob = build_signed_dfrost_event(
        &bob_sk,
        bob_addr,
        DfrostEventKind::DkgComplete,
        &dk_payload,
        hlc_at(1_100, "bob-dev"),
    )
    .expect("sign bob dk");

    let mut alice_dlog = DfrostLog::new();
    alice_dlog.committee_state = CommitteeState {
        active: true,
        current_epoch: 1,
        joint_verifying_key: Some(joint_vk),
        verifying_shares: verifying_shares
            .iter()
            .map(|mvs| (mvs.member, mvs.verifying_share))
            .collect(),
        members: members.clone(),
        threshold: 2,
        max_signers: 2,
        identifier_map: CommitteeState::build_identifier_map(&members),
        ..Default::default()
    };
    alice_dlog.insert_event_for_test(dk_alice);
    alice_dlog.insert_event_for_test(dk_bob);
    let alice_log = Arc::new(Mutex::new(alice_dlog));

    // ── Two real Zenoh sessions + one shared crdt_state (both sides
    // already hold the community's current epoch key — this test proves
    // the catch-up WIRE plumbing, not the separate joiner-enrollment /
    // key-distribution problem, which is out of scope here) ────────────
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A open"));
    let session_c = Arc::new(zenoh::open(cfg).await.expect("session C open"));

    let community_id = SpaceId([0xD8; 16]);
    let community_id_hex = hex::encode(community_id.0);
    let crdt_state = Arc::new(Mutex::new({
        let mut os = harmony_app::owner_state_crdt::OwnerState::default();
        os.spaces.insert(
            community_id,
            harmony_app::community_state_sync::test_community_space(
                community_id,
                0,
                EpochKey::new([0x55; 32]),
            ),
        );
        os
    }));

    let closing = Arc::new(AtomicBool::new(false));

    // ── Alice's engine + adapter (responder: declares the queryable) ───
    let (alice_pub_tx, alice_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (alice_sub_tx, alice_sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let alice_engine = DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
        community_id,
        dfrost_log: alice_log.clone(),
        publisher_tx: alice_pub_tx.clone(),
        subscriber_rx: alice_sub_rx,
        app_handle: None,
        self_addr: alice_addr,
        self_x25519_priv: alice_x_priv,
        identity_resolver: alice_resolver,
        registry_weak: None,
        driver: None,
        membership_resolver: None,
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    // Mirrors `ensure_dfrost_engine_for`'s (lib.rs) hook construction:
    // one closure per hook, each cloning the shared engine `Arc`.
    let alice_hooks = {
        let e_build = Arc::clone(&alice_engine);
        let e_respond = Arc::clone(&alice_engine);
        let e_ingest = Arc::clone(&alice_engine);
        DfrostCatchupHooks {
            build_request: Arc::new(move || {
                let e = Arc::clone(&e_build);
                Box::pin(async move { e.catchup_build_request().await })
            }),
            respond: Arc::new(move |request| {
                let e = Arc::clone(&e_respond);
                Box::pin(async move { e.catchup_respond(request).await })
            }),
            ingest: Arc::new(move |frames| {
                let e = Arc::clone(&e_ingest);
                Box::pin(async move { e.catchup_ingest(frames).await })
            }),
            hint: alice_engine.catchup_hint(),
        }
    };
    let _adapter_a = spawn_dfrost_log_zenoh_adapter(
        Arc::clone(&session_a),
        community_id_hex.clone(),
        community_id,
        Arc::clone(&crdt_state),
        alice_pub_rx,
        alice_sub_tx,
        Some(alice_hooks),
        Arc::clone(&closing),
    );

    // Settle: give the two real sessions time to discover each other and
    // Alice's queryable time to register before the joiner's requester
    // fires its first (immediate, no-wait) GET below — same rationale as
    // the pub/sub warm-up in the sibling test above, but there is no
    // equivalent "throwaway GET" to warm a queryable with, so this is a
    // plain bounded sleep rather than an observable loop.
    //
    // ZEB-1030 PR#778 round-1: this sleep is a best-effort head start, not
    // the determinism backstop — on slow CI, discovery can still lag past
    // it, and the joiner's requester task would otherwise wait out the
    // full 300s `DFROST_CATCHUP_INTERVAL` before its next attempt (past
    // the 25s `wait_for` deadline below). `wait_for`'s hint-nudge loop is
    // what actually self-heals that: it wakes the requester every poll
    // tick, so a late-discovered queryable still gets retried well within
    // the deadline.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // ── Fresh joiner's engine + adapter (requester) ─────────────────────
    let c_log = Arc::new(Mutex::new(DfrostLog::new()));
    let (c_pub_tx, c_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (c_sub_tx, c_sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let c_engine = DfrostLogEngine::<tauri::test::MockRuntime>::start(DfrostLogEngineParams {
        community_id,
        dfrost_log: c_log.clone(),
        publisher_tx: c_pub_tx.clone(),
        subscriber_rx: c_sub_rx,
        app_handle: None,
        self_addr: OwnerAddr([0xC3; 16]),
        self_x25519_priv: [0u8; 32],
        identity_resolver: joiner_resolver,
        registry_weak: None,
        driver: None,
        membership_resolver: None,
        orchestrator_config: Default::default(),
        persist: None,
    })
    .await;

    let c_hooks = {
        let e_build = Arc::clone(&c_engine);
        let e_respond = Arc::clone(&c_engine);
        let e_ingest = Arc::clone(&c_engine);
        DfrostCatchupHooks {
            build_request: Arc::new(move || {
                let e = Arc::clone(&e_build);
                Box::pin(async move { e.catchup_build_request().await })
            }),
            respond: Arc::new(move |request| {
                let e = Arc::clone(&e_respond);
                Box::pin(async move { e.catchup_respond(request).await })
            }),
            ingest: Arc::new(move |frames| {
                let e = Arc::clone(&e_ingest);
                Box::pin(async move { e.catchup_ingest(frames).await })
            }),
            hint: c_engine.catchup_hint(),
        }
    };
    // Kept separately from `c_hooks` (which is moved into the adapter
    // below) so `wait_for` can nudge the joiner's requester task directly
    // — same underlying `Arc<Notify>` as the hint the hooks/orchestrator
    // hold.
    let c_hint = c_engine.catchup_hint();
    let _adapter_c = spawn_dfrost_log_zenoh_adapter(
        Arc::clone(&session_c),
        community_id_hex,
        community_id,
        Arc::clone(&crdt_state),
        c_pub_rx,
        c_sub_tx,
        Some(c_hooks),
        Arc::clone(&closing),
    );

    // ── The bytes MUST cross the real session: bounded poll, no sleep ──
    // The hint nudge (via `c_hint`) drives retries if the joiner's first
    // GET missed Alice's queryable due to discovery lag past the warm-up
    // sleep above.
    wait_for(
        "joiner adopts alice's committee via a real zenoh catchup round",
        &c_log,
        25,
        Some(&c_hint),
        |log| log.committee_state.active,
    )
    .await;

    let cg = c_log.lock().await;
    assert!(
        cg.committee_state.active,
        "joiner's committee must be active after catch-up"
    );
    assert_eq!(cg.committee_state.current_epoch, 1);
    assert_eq!(cg.committee_state.joint_verifying_key, Some(joint_vk));
    assert_eq!(cg.committee_state.members, members);
}
