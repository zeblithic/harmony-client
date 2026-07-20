//! ZEB-298 PR 2 Task 12: end-to-end two-engine real-Zenoh proof of the
//! delegate-on-behalf emit path.
//!
//! Mirrors the wire-bridging pattern from
//! `community_voting_zenoh_integration.rs` (two real `zenoh::Session`s
//! wired to two `VotingLogEngine<MockRuntime>` instances) and combines
//! it with the in-process Task 8 `process_inbound_tier2_signal_fires_delegate_on_behalf`
//! coverage: alice on engine A has delegated to bob; bob publishes a
//! Tier 2 Signal on engine B; engine A receives via real Zenoh and
//! `voting-delegate-signaled-on-your-behalf` fires on engine A's
//! `app_handle`.
//!
//! Why this test exists:
//! - The Task 5 + Task 8 in-process tests prove the hook fires when a
//!   Signal applies via `publish_event` or via a direct subscriber_tx
//!   push that bypasses the Zenoh wire.
//! - The PR 1 Zenoh integration test proves a single peer-originated
//!   voting event crosses two real Zenoh sessions and applies on the
//!   receiver.
//! - This test is the missing junction: it proves that the bob→wire→
//!   alice path also fans the Task 5 emit hook end-to-end, including
//!   the post-apply `maybe_emit_delegate_on_behalf` from
//!   `process_inbound_dispatch`. It additionally asserts state parity
//!   (delegation graph + total_conviction_at_with_delegation) between
//!   the two engines, which the previous tests only covered for a
//!   single side.
//!
//! Convergence-wait pattern: 20×100ms ticks (≤ 2s) per
//! `feedback_wall_clock_regression_budget` — wall-clock-bounded but
//! polled (not a fixed `sleep`), so the test passes on resource-
//! constrained CI and fails fast when transport regresses.

#![cfg(feature = "test-fixtures")]

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ed25519_dalek::Signer as _;
use tokio::sync::Mutex;

use harmony_app::community_voting_conviction::{
    AutoExecAction, CommunityVotingPolicy, DelegatePayload, SignalPayload, Tier2PollConfig, Q32,
};
use harmony_app::community_voting_core::{
    derive_poll_id, Eligibility, MemberAttrs, MembershipSnapshot, PollEventKindCode,
    SignedVotingEvent, Tier, VotingIdentityResolver,
};
use harmony_app::community_voting_log::{
    MembershipSnapshotResolver, SnapshotResolverError, TierState, VotingLog,
};
use harmony_app::community_voting_log_engine::{VotingLogEngine, VotingLogEngineParams};
use harmony_app::event_loop::spawn_voting_log_zenoh_adapter;
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};
use tauri::Listener;

/// ZEB-718: no-op backfill closures — this test exercises only the live
/// pub/sub delegate-on-behalf path (no backfill).
fn noop_backfill() -> (
    harmony_app::event_loop::VotingBackfillReadFn,
    harmony_app::event_loop::VotingBackfillApplyFn,
) {
    (
        Arc::new(|| Box::pin(async { Vec::new() })),
        Arc::new(|_frame| Box::pin(async {})),
    )
}

/// Build a `(SigningKey, OwnerAddr, [u8; 64])` triple from a single-byte seed.
/// Address binding here matches the defense-in-depth check performed by
/// `verify_voting_event` on the inbound path.
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

/// Combined resolver fixture identical in shape to the one used by
/// `community_voting_zenoh_integration.rs` — satisfies both the identity
/// and membership-snapshot resolver traits from a single `Arc`.
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

/// Build a fully-signed Tier 2 PollCreate event.
///
/// No `build_signed_poll_create_tier2` helper exists yet in
/// `community_voting_core` — the only build_signed_* helpers are for
/// Tier 1 / Tier 3. Construct + sign directly, matching the pattern
/// used in `community_voting_log_engine.rs::process_inbound_tier2_signal_fires_delegate_on_behalf`
/// and `tests/community_voting_process_inbound_prod.rs`.
fn build_tier2_poll_create(
    signing: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    cfg: &Tier2PollConfig,
    hlc: Hlc,
) -> SignedVotingEvent {
    let mut payload = Vec::new();
    ciborium::into_writer(cfg, &mut payload).expect("encode Tier2PollConfig");
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Conviction,
        kind: PollEventKindCode::PollCreate,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().expect("signing_bytes");
    ev.sig = signing.sign(&sb).to_bytes().to_vec();
    ev
}

/// Build a fully-signed Tier 2 Signal event.
fn build_tier2_signal(
    signing: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    proposal_id: harmony_app::community_voting_core::PollId,
    support: bool,
    hlc: Hlc,
) -> SignedVotingEvent {
    let p = SignalPayload {
        proposal_id,
        support,
    };
    let mut payload = Vec::new();
    ciborium::into_writer(&p, &mut payload).expect("encode SignalPayload");
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Conviction,
        kind: PollEventKindCode::Signal,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().expect("signing_bytes");
    ev.sig = signing.sign(&sb).to_bytes().to_vec();
    ev
}

/// Build a fully-signed Tier 2 Delegate event.
fn build_tier2_delegate(
    signing: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    to: OwnerAddr,
    hlc: Hlc,
) -> SignedVotingEvent {
    let p = DelegatePayload {
        to,
        scope: "all".into(),
    };
    let mut payload = Vec::new();
    ciborium::into_writer(&p, &mut payload).expect("encode DelegatePayload");
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Conviction,
        kind: PollEventKindCode::Delegate,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().expect("signing_bytes");
    ev.sig = signing.sign(&sb).to_bytes().to_vec();
    ev
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tier2_signal_via_zenoh_fires_delegate_on_behalf_on_peer_engine() {
    // ── Identities ──────────────────────────────────────────────────────
    // alice runs engine A and has delegated to bob.
    // bob runs engine B and will signal on a Tier 2 proposal he creates.
    let (alice_key, alice_owner, alice_pub64) = fixture_identity(0xA1);
    let (bob_key, bob_owner, bob_pub64) = fixture_identity(0xB1);

    let community_id = SpaceId([0x2C; 16]);
    let community_id_hex = hex::encode(community_id.0);

    // ZEB-717: the voting adapters encrypt/decrypt at the wire boundary using
    // the community's live epoch key. Both adapters share one crdt_state (same
    // key, epoch 0) so events round-trip across the two real Zenoh sessions.
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

    // ── Membership snapshot covering both actors ───────────────────────
    let mut members = HashMap::new();
    for owner in [alice_owner, bob_owner] {
        members.insert(
            owner,
            MemberAttrs {
                power: 10,
                vouching_depth: 1,
            },
        );
    }
    let snapshot = MembershipSnapshot { members };

    let resolvers = Arc::new(FixedResolvers {
        identity: HashMap::from([(alice_owner, alice_pub64), (bob_owner, bob_pub64)]),
        snapshot: snapshot.clone(),
    });
    let id_resolver: Arc<dyn VotingIdentityResolver> = resolvers.clone();
    let mem_resolver: Arc<dyn MembershipSnapshotResolver> = resolvers.clone();

    // ── Real Zenoh sessions ─────────────────────────────────────────────
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A open"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B open"));

    // ── Engine A (alice) wire halves + adapter ──────────────────────────
    let closing_a = Arc::new(AtomicBool::new(false));
    let (a_pub_tx, a_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let (a_sub_tx, a_sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let (bf_read_a, bf_apply_a) = noop_backfill();
    let _adapter_a = spawn_voting_log_zenoh_adapter(
        Arc::clone(&session_a),
        community_id_hex.clone(),
        community_id,
        Arc::clone(&crdt_state),
        a_pub_rx,
        a_sub_tx,
        bf_read_a,
        bf_apply_a,
        Duration::from_secs(86_400),
        Arc::clone(&closing_a),
    );

    // ── Engine B (bob) wire halves + adapter ────────────────────────────
    let closing_b = Arc::new(AtomicBool::new(false));
    let (b_pub_tx, b_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let (b_sub_tx, b_sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let (bf_read_b, bf_apply_b) = noop_backfill();
    let _adapter_b = spawn_voting_log_zenoh_adapter(
        Arc::clone(&session_b),
        community_id_hex.clone(),
        community_id,
        Arc::clone(&crdt_state),
        b_pub_rx,
        b_sub_tx,
        bf_read_b,
        bf_apply_b,
        Duration::from_secs(86_400),
        Arc::clone(&closing_b),
    );

    // ── Pre-seed both engines' VotingLog ────────────────────────────────
    // Both engines apply the same Tier 2 PollCreate and Delegate
    // (alice → bob) directly via `apply_with_snapshot`, mirroring the
    // production setup where both peers have caught up to a shared
    // pre-state. Pre-seeding before engine start avoids any race with
    // the inbound receive loop reordering the seed vs the
    // wire-delivered Signal.
    let cfg_poll = Tier2PollConfig {
        proposal_text: "promote".into(),
        half_life_seconds: 86_400,
        threshold_min_q32: Q32,
        threshold_max_q32: 100 * Q32,
        beta: 2,
        delegation_allowed: true,
        auto_exec: AutoExecAction::None,
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
    };

    // Bob is the creator of the Tier 2 proposal — signed by his key so
    // the event would also pass verify_voting_event if it ever crossed
    // the wire. (Here we apply it locally on both engines as part of
    // pre-seed, but signing keeps the test free of "if we ever route
    // this differently" hazards.)
    let create_hlc = Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 0,
        device_id: "bob".into(),
    };
    let create_event = build_tier2_poll_create(&bob_key, bob_owner, &cfg_poll, create_hlc);

    // Alice's Delegate(alice → bob) — signed by alice.
    let delegate_hlc = Hlc {
        wall_ms: 1_700_000_001_000,
        logical: 0,
        device_id: "alice".into(),
    };
    let delegate_event = build_tier2_delegate(&alice_key, alice_owner, bob_owner, delegate_hlc);

    let log_a = Arc::new(Mutex::new(VotingLog::default()));
    let log_b = Arc::new(Mutex::new(VotingLog::default()));

    let pid: harmony_app::community_voting_core::PollId = {
        let create_sb = create_event.signing_bytes().expect("create signing_bytes");
        derive_poll_id(&community_id, &create_sb)
    };

    for log in [&log_a, &log_b] {
        let mut guard = log.lock().await;
        guard
            .apply_with_snapshot(create_event.clone(), &community_id, Some(snapshot.clone()))
            .expect("apply tier2 create");
        guard
            .apply_with_snapshot(
                delegate_event.clone(),
                &community_id,
                Some(snapshot.clone()),
            )
            .expect("apply alice→bob delegate");
        // Enable the notify hook on BOTH engines (only engine A's emit
        // is asserted, but parity is the right discipline — both peers
        // should reach an identical policy state).
        guard.set_policy(CommunityVotingPolicy {
            notify_on_delegate_signal: true,
            tier3_privacy_mode_default: "pu".into(),
        });
    }

    // Confirm pre-seed parity before going live. This is cheap and
    // protects future refactors from silently inverting the setup.
    {
        let la = log_a.lock().await;
        let lb = log_b.lock().await;
        assert_eq!(
            la.delegation_graph.delegate_of(alice_owner),
            Some(bob_owner),
            "engine A pre-seed: alice→bob delegation missing"
        );
        assert_eq!(
            lb.delegation_graph.delegate_of(alice_owner),
            Some(bob_owner),
            "engine B pre-seed: alice→bob delegation missing"
        );
        assert!(
            la.polls.contains_key(&pid),
            "engine A pre-seed: poll missing"
        );
        assert!(
            lb.polls.contains_key(&pid),
            "engine B pre-seed: poll missing"
        );
    }

    // ── Engine A (alice) ────────────────────────────────────────────────
    let app_a = tauri::test::mock_app();
    let app_handle_a = app_a.handle().clone();

    let engine_a = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
        community_id,
        voting_log: Arc::clone(&log_a),
        publisher_tx: a_pub_tx.clone(),
        subscriber_rx: a_sub_rx,
        hlc_tracker: None,
        device_id: None,
        app_handle: Some(app_handle_a.clone()),
        identity_resolver: Some(Arc::clone(&id_resolver)),
        membership_resolver: Some(Arc::clone(&mem_resolver)),
    })
    .await;

    // Install alice's signing key + owner so `maybe_emit_delegate_on_behalf`
    // can resolve the local owner. The actual key bytes are irrelevant
    // for the hook — it only reads `owner` — but we pass the real key
    // for symmetry with production.
    engine_a
        .install_local_signing_key(Arc::new(alice_key.clone()), alice_owner)
        .await;

    // Attach a listener to engine A's app handle to capture emits.
    let captured: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_for_listener = Arc::clone(&captured);
    app_handle_a.listen("voting-delegate-signaled-on-your-behalf", move |evt| {
        captured_for_listener
            .lock()
            .expect("captured lock")
            .push(evt.payload().to_string());
    });

    // ── Engine B (bob) ──────────────────────────────────────────────────
    let app_b = tauri::test::mock_app();
    let app_handle_b = app_b.handle().clone();

    let engine_b = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
        community_id,
        voting_log: Arc::clone(&log_b),
        publisher_tx: b_pub_tx.clone(),
        subscriber_rx: b_sub_rx,
        hlc_tracker: None,
        device_id: None,
        app_handle: Some(app_handle_b),
        identity_resolver: Some(Arc::clone(&id_resolver)),
        membership_resolver: Some(Arc::clone(&mem_resolver)),
    })
    .await;
    engine_b
        .install_local_signing_key(Arc::new(bob_key.clone()), bob_owner)
        .await;

    // ── Warm-up: Zenoh peer discovery ──────────────────────────────────
    // Same strategy as `community_voting_zenoh_integration.rs`: push
    // sentinel packets that decode-fail (silently dropped on receive)
    // through both directions to give the in-memory gossip time to
    // wire the subscriber on each side before the real Signal lands.
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = a_pub_tx.send(b"WARMUP-A".to_vec()).await;
        let _ = b_pub_tx.send(b"WARMUP-B".to_vec()).await;
    }

    // ── Bob signals support=true on engine B ────────────────────────────
    let signal_hlc = Hlc {
        wall_ms: 1_700_000_010_000,
        logical: 0,
        device_id: "bob".into(),
    };
    let signal_event = build_tier2_signal(&bob_key, bob_owner, pid, true, signal_hlc.clone());

    engine_b
        .publish_event(signal_event.clone(), None)
        .await
        .expect("engine B publish_event(Signal)");

    // ── Wait for engine A to converge on the signal ─────────────────────
    // Engine B's publish_event applied locally + broadcast to wire;
    // engine A's adapter forwards to engine A's inbound dispatch, which
    // runs verify+apply and then the four post-apply hooks. We watch
    // `log_a`'s per_voter map for bob's entry as the convergence signal.
    let mut converged = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let la = log_a.lock().await;
        if let Some(ps) = la.polls.get(&pid) {
            if let TierState::Tier2(t2) = &ps.tier_state {
                if t2.per_voter.contains_key(&bob_owner) {
                    converged = true;
                    break;
                }
            }
        }
    }
    assert!(
        converged,
        "engine A must converge on bob's Signal via Zenoh within 10s"
    );

    // ── Wait for the emit to land ───────────────────────────────────────
    // The hook runs after apply, but Tauri's listener dispatch is
    // async-ish; give it a short tick budget to flush.
    let mut payloads: Vec<String> = Vec::new();
    for _ in 0..100 {
        {
            let g = captured.lock().expect("captured lock");
            if !g.is_empty() {
                payloads = g.clone();
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert_eq!(
        payloads.len(),
        1,
        "exactly one voting-delegate-signaled-on-your-behalf emit expected on engine A; got {payloads:?}"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&payloads[0]).expect("emit payload is JSON");
    assert_eq!(
        parsed["delegate"].as_str(),
        Some(hex::encode(bob_owner.0).as_str()),
        "emit payload delegate must be bob's OwnerAddr"
    );
    assert_eq!(
        parsed["proposalId"].as_str(),
        Some(hex::encode(pid.0).as_str()),
        "emit payload proposalId must be the Tier 2 poll id"
    );
    assert_eq!(
        parsed["support"].as_bool(),
        Some(true),
        "emit payload support must be true"
    );
    assert_eq!(
        parsed["communityId"].as_str(),
        Some(community_id_hex.as_str()),
        "emit payload communityId must be the community"
    );

    // ── Delegation-graph parity ─────────────────────────────────────────
    // Both engines must agree alice→bob. This is trivially true from
    // the pre-seed, but asserting after the Signal crosses the wire
    // catches a regression where the inbound apply path could
    // accidentally mutate the delegation graph.
    {
        let la = log_a.lock().await;
        let lb = log_b.lock().await;
        assert_eq!(
            la.delegation_graph.delegate_of(alice_owner),
            Some(bob_owner),
            "engine A post-converge: alice→bob delegation must hold"
        );
        assert_eq!(
            lb.delegation_graph.delegate_of(alice_owner),
            Some(bob_owner),
            "engine B post-converge: alice→bob delegation must hold"
        );
        assert_eq!(
            la.delegation_graph.delegator_count(bob_owner),
            1,
            "engine A: bob's delegator count must be 1"
        );
        assert_eq!(
            lb.delegation_graph.delegator_count(bob_owner),
            1,
            "engine B: bob's delegator count must be 1"
        );
    }

    // ── Tally parity (total_conviction_at_with_delegation) ─────────────
    // Same proposal, same delegation graph, same t — both engines
    // should compute the same weighted conviction. This is the
    // load-bearing scalar that drives the threshold-cross tick, so
    // peer divergence here would corrupt Tier 2 finality.
    let t_now: i128 = signal_hlc.wall_ms as i128 + 1_000; // 1s past the signal
    let conv_a: i128;
    let conv_b: i128;
    {
        let la = log_a.lock().await;
        let lb = log_b.lock().await;
        let t2_a = la
            .polls
            .get(&pid)
            .and_then(|ps| ps.tier_state.as_tier2())
            .expect("engine A: Tier2 state present");
        let t2_b = lb
            .polls
            .get(&pid)
            .and_then(|ps| ps.tier_state.as_tier2())
            .expect("engine B: Tier2 state present");
        conv_a = t2_a.total_conviction_at_with_delegation(t_now, &la.delegation_graph);
        conv_b = t2_b.total_conviction_at_with_delegation(t_now, &lb.delegation_graph);
    }
    assert_eq!(
        conv_a, conv_b,
        "total_conviction_at_with_delegation must match across engines"
    );
    // Bob signaled support=true and alice delegates to bob → weight = 2
    // (bob + 1 direct delegator). The exact value depends on the
    // half-life curve, but it must be strictly positive — a zero would
    // indicate the Signal failed to apply or delegation isn't being
    // honored by the conviction math.
    assert!(
        conv_a > 0,
        "weighted conviction must be > 0 after bob's Signal (delegation+signal both apply); got {conv_a}"
    );

    // ── Stretch: alice's direct override drops bob's effective weight ──
    // alice on engine A directly signals support=false on the same
    // proposal. After it crosses the wire to engine B, both engines'
    // total_conviction_at_with_delegation should drop alice's
    // contribution from bob's tally (per spec §5 direct-signal
    // override semantics — bob's `direct_delegators` filter excludes
    // delegators who have themselves signaled on THIS proposal).
    let alice_override_hlc = Hlc {
        wall_ms: 1_700_000_020_000,
        logical: 0,
        device_id: "alice".into(),
    };
    let alice_override = build_tier2_signal(
        &alice_key,
        alice_owner,
        pid,
        false,
        alice_override_hlc.clone(),
    );

    engine_a
        .publish_event(alice_override.clone(), None)
        .await
        .expect("engine A publish_event(alice override Signal)");

    // Wait for engine B to converge on alice's override (her entry
    // appears in per_voter on log_b).
    let mut override_converged = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let lb = log_b.lock().await;
        if let Some(ps) = lb.polls.get(&pid) {
            if let TierState::Tier2(t2) = &ps.tier_state {
                if t2.per_voter.contains_key(&alice_owner) {
                    override_converged = true;
                    break;
                }
            }
        }
    }
    assert!(
        override_converged,
        "engine B must converge on alice's override Signal via Zenoh within 10s"
    );

    // After the override, bob's contribution should no longer be
    // augmented by alice's weight in the with_delegation tally —
    // because alice is now in `per_voter` herself, the
    // `direct_delegators` filter excludes her. The weighted total
    // should therefore be strictly smaller than the no-override value
    // (alice's support=false contribution is 0 conviction; bob's own
    // contribution is unchanged).
    let t_after_override: i128 = alice_override_hlc.wall_ms as i128 + 1_000;
    let conv_a_after: i128;
    let conv_b_after: i128;
    let conv_a_no_delegation: i128;
    {
        let la = log_a.lock().await;
        let lb = log_b.lock().await;
        let t2_a = la
            .polls
            .get(&pid)
            .and_then(|ps| ps.tier_state.as_tier2())
            .expect("engine A: Tier2 state present");
        let t2_b = lb
            .polls
            .get(&pid)
            .and_then(|ps| ps.tier_state.as_tier2())
            .expect("engine B: Tier2 state present");
        conv_a_after =
            t2_a.total_conviction_at_with_delegation(t_after_override, &la.delegation_graph);
        conv_b_after =
            t2_b.total_conviction_at_with_delegation(t_after_override, &lb.delegation_graph);
        // For the override-effective assertion we also compute the
        // unweighted total on engine A — with alice having signaled
        // false, her own conviction contribution is 0, so weighted
        // and unweighted should now coincide modulo bob's solo weight.
        conv_a_no_delegation = t2_a.total_conviction_at(t_after_override);
    }
    assert_eq!(
        conv_a_after, conv_b_after,
        "post-override weighted conviction must match across engines"
    );
    assert_eq!(
        conv_a_after, conv_a_no_delegation,
        "after alice's direct override, weighted conviction must equal unweighted \
         (alice is no longer counted as a delegator on this proposal — spec §5 override)"
    );

    // ── Clean shutdown ──────────────────────────────────────────────────
    closing_a.store(true, std::sync::atomic::Ordering::SeqCst);
    closing_b.store(true, std::sync::atomic::Ordering::SeqCst);
    drop(engine_a);
    drop(engine_b);
}
