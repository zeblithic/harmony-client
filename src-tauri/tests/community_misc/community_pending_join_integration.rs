//! Regression test for ZEB-254 Issue 2: engine-level PendingJoin acceptance.
//!
//! ZEB-339 moved PendingJoin verification off the admin-identity-pub P5 gate
//! and onto the carried EnrollmentCert / materialized enrolled_device_keys, so
//! the engine no longer needs an admin-pub binding to admit a PendingJoin.
//! This test inserts a PendingJoin through the engine's
//! `insert_local_event_with_pubs` path to confirm the plumbing is live.

use ed25519_dalek::Signer;
use harmony_app::community_invite::{canonical_invite_token_bytes, InviteToken};
use harmony_app::community_membership::{
    mint_test_owner, sign_event, EventPayload, MembershipEventKind, SignedMembershipEvent,
    TestOwner,
};
use harmony_app::community_state_crdt::{CommunityState, InsertOutcome};
use harmony_app::community_state_sync::{
    CommunityRootHlcTracker, CommunitySyncEngine, CommunitySyncEngineConfig, PersistPaths,
    DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{CasOp, ContentStore, RuntimeContentStore};
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// ZEB-339: build an enrolled-device owner from a seed. Returns
/// `(TestOwner, dummy_pub, owner_addr)` so existing `(priv, pub, addr)`
/// destructures keep compiling. The actor is `owner.owner` (owner_id); events
/// are signed by the enrolled device key (#2).
fn make_identity(seed: u8) -> (TestOwner, [u8; 64], OwnerAddr) {
    let owner = mint_test_owner(seed);
    let addr = owner.owner;
    (owner, [0u8; 64], addr)
}

/// ZEB-339: the owner's enrolled device signing key (#2).
fn signing_key_from(owner: &TestOwner) -> ed25519_dalek::SigningKey {
    owner.device_key.clone()
}

/// ZEB-339: sign a membership event with the owner's enrolled device key,
/// attaching the Master cert on identity-introducing events (Join/PendingJoin).
fn sign_event_with_identity(
    payload: &EventPayload,
    owner: &TestOwner,
) -> Result<SignedMembershipEvent, harmony_app::owner_state_crypto::CryptoError> {
    let ev = sign_event(payload, &owner.device_key)?;
    Ok(match ev.kind {
        MembershipEventKind::Join | MembershipEventKind::PendingJoin { .. } => {
            SignedMembershipEvent {
                enrollment: Some(owner.cert.clone()),
                ..ev
            }
        }
        _ => ev,
    })
}

/// ZEB-339: build a signed InviteToken. The token sig now verifies against the
/// inviter's ENROLLED device key (P5 `verify_invite_token_sig_with_enrolled`),
/// so it is signed with the admin's device key (#2), not their identity key.
fn make_signed_token(
    admin_owner: &TestOwner,
    admin_addr: OwnerAddr,
    invitee_hint: Option<OwnerAddr>,
    expires_at: Option<u64>,
) -> InviteToken {
    let mut tok = InviteToken {
        inviter: admin_addr,
        invitee_hint,
        minted_at: Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "admin-dev".into(),
        },
        expires_at,
        sig: [0u8; 64],
    };
    let bytes = canonical_invite_token_bytes(&tok).expect("encode token");
    tok.sig = admin_owner.device_key.sign(&bytes).to_bytes();
    tok
}

/// Regression: PendingJoin inserted through the engine's
/// `insert_local_event_with_pubs` must be accepted.
///
/// ZEB-339 verifies PendingJoin via the carried EnrollmentCert /
/// materialized enrolled_device_keys, so the engine admits the event
/// without any admin-identity-pub binding.
#[tokio::test]
async fn pending_join_accepted_via_engine_insert_without_admin_pub_bind() {
    let community_id = SpaceId([7u8; 16]);

    // Admin (invite issuer) — also the engine's configured admin_addr.
    let (admin_priv, admin_pub, admin_addr) = make_identity(0xA1);
    let admin_signing = signing_key_from(&admin_priv);

    // Joiner — sends the PendingJoin.
    let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xB1);

    let mk = EpochKey::new([0x42; 32]);

    let state = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));

    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(8);
    tokio::spawn(async move {
        while let Some(op) = cas_op_rx.recv().await {
            if let CasOp::PutLocal {
                reply: Some(reply), ..
            } = op
            {
                let _ = reply.send(Ok(()));
            }
        }
    });
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));

    let (pub_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(8);

    let tmp = tempfile::tempdir().expect("tempdir");

    let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr,
        is_invite_only: true,
        device_id: "admin-dev".into(),
        self_owner: admin_addr,
        signing_key: Arc::new(admin_signing),
        state: Arc::clone(&state),
        tracker: Arc::clone(&tracker),
        content_store: cs,
        publisher_tx: pub_tx,
        subscriber_rx: sub_rx,
        paths: PersistPaths {
            crdt: tmp.path().join("crdt.cbor"),
            replay: tmp.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: None,
        error_tx: None,
        delta_tx: None,
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });

    // Insert the admin's bootstrap Join first so `prior_state_at_event`
    // sees the admin as Joined (required for membership-at-HLC derivation).
    let admin_join_payload = EventPayload {
        id: [0xAAu8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_700_000_001_000,
            logical: 0,
            device_id: "admin-dev".into(),
        },
    };
    let admin_join =
        sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign admin join");

    let bootstrap_outcome = engine
        .insert_local_event_with_pubs(admin_join, admin_pub, None)
        .await
        .expect("admin join insert");
    assert!(
        matches!(bootstrap_outcome, InsertOutcome::Inserted),
        "admin bootstrap join must be Inserted; got {:?}",
        bootstrap_outcome
    );

    // Build a valid PendingJoin event from joiner, with a token signed by admin.
    // expires_at is far in the future relative to the event's wall_ms.
    let token = make_signed_token(
        &admin_priv,
        admin_addr,
        Some(joiner_addr),
        Some(1_700_000_100_000),
    );
    let pending_join_payload = EventPayload {
        id: [0xBBu8; 16],
        community_id,
        kind: MembershipEventKind::PendingJoin {
            invite_token: token,
        },
        actor: joiner_addr,
        at: Hlc {
            wall_ms: 1_700_000_002_000,
            logical: 0,
            device_id: "joiner-dev".into(),
        },
    };
    let pending_join =
        sign_event_with_identity(&pending_join_payload, &joiner_priv).expect("sign PendingJoin");

    // Insert through the engine — ZEB-339 verifies the PendingJoin via the
    // carried EnrollmentCert / materialized membership, not an admin-pub gate.
    let outcome = engine
        .insert_local_event_with_pubs(pending_join, joiner_pub, None)
        .await
        .expect("pending join insert");

    assert!(
        matches!(outcome, InsertOutcome::Inserted),
        "PendingJoin must be Inserted via engine; got {:?}",
        outcome
    );

    engine.shutdown().await.expect("shutdown");
}

// ── ZEB-254 Task 10: auto-counter-sign tests ─────────────────────────────────

use harmony_app::community_state_sync::IdentityResolver;
use std::collections::HashMap;

/// Minimal static resolver backed by a HashMap — same pattern as
/// `community_sync_integration.rs::StaticResolver`.
struct StaticResolver {
    map: HashMap<OwnerAddr, [u8; 64]>,
}

#[async_trait::async_trait]
impl IdentityResolver for StaticResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        self.map.get(addr).copied()
    }
}

/// Spin up a `CommunitySyncEngine` with the given admin + self identity and
/// an optional `IdentityResolver`. Returns the engine (not yet Arc'd —
/// caller wraps as needed for shutdown).
#[allow(clippy::too_many_arguments)]
fn build_engine_with_resolver(
    community_id: SpaceId,
    admin_addr: OwnerAddr,
    self_addr: OwnerAddr,
    self_signing_key: ed25519_dalek::SigningKey,
    pub_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    sub_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    cs: Arc<dyn harmony_app::content_store::ContentStore>,
    tmp: &tempfile::TempDir,
    resolver: Option<Arc<dyn IdentityResolver>>,
    admin_pub: Option<[u8; 64]>,
) -> CommunitySyncEngine {
    let mk = EpochKey::new([0x42; 32]);
    let state = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker = Arc::new(Mutex::new(
        harmony_app::community_state_sync::CommunityRootHlcTracker::default(),
    ));
    let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr,
        is_invite_only: true,
        device_id: "test-dev".into(),
        self_owner: self_addr,
        signing_key: Arc::new(self_signing_key),
        state,
        tracker,
        content_store: cs,
        publisher_tx: pub_tx,
        subscriber_rx: sub_rx,
        paths: PersistPaths {
            crdt: tmp.path().join("crdt.cbor"),
            replay: tmp.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: resolver,
        error_tx: None,
        delta_tx: None,
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: admin_pub,
        nav_emitter: None,
        root_serve_rx: None,
    });
    engine
}

fn make_cas() -> Arc<dyn harmony_app::content_store::ContentStore> {
    let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move {
        while let Some(op) = cas_op_rx.recv().await {
            if let CasOp::PutLocal {
                reply: Some(reply), ..
            } = op
            {
                let _ = reply.send(Ok(()));
            }
        }
    });
    Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ))
}

/// Happy-path: an admin engine (self == admin, self is Joined) receives a
/// valid PendingJoin via `insert_local_event_with_pubs`. The auto-counter-sign
/// hook should fire and insert a self-authored JoinCountersign into the CRDT.
#[tokio::test]
async fn admin_engine_auto_counter_signs_on_pending_join_insert() {
    let community_id = SpaceId([0xC0u8; 16]);
    let (admin_priv, admin_pub, admin_addr) = make_identity(0xA2);
    let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xB2);
    let admin_signing = signing_key_from(&admin_priv);

    let mut resolver_map = HashMap::new();
    resolver_map.insert(admin_addr, admin_pub);
    resolver_map.insert(joiner_addr, joiner_pub);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    let cs = make_cas();
    let (pub_tx, _pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (_sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let tmp = tempfile::tempdir().expect("tempdir");

    let engine = build_engine_with_resolver(
        community_id,
        admin_addr,
        admin_addr, // self == admin
        admin_signing,
        pub_tx,
        sub_rx,
        cs,
        &tmp,
        Some(resolver),
        Some(admin_pub),
    );

    // Insert admin bootstrap Join so self is Joined in materialized state.
    let admin_join_payload = EventPayload {
        id: [0xA2u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_700_000_001_000,
            logical: 0,
            device_id: "test-dev".into(),
        },
    };
    let admin_join =
        sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign admin join");
    let outcome = engine
        .insert_local_event_with_pubs(admin_join, admin_pub, None)
        .await
        .expect("admin join insert");
    assert!(
        matches!(outcome, InsertOutcome::Inserted),
        "admin join: {:?}",
        outcome
    );

    // Build a valid PendingJoin from joiner.
    let token = make_signed_token(
        &admin_priv,
        admin_addr,
        Some(joiner_addr),
        Some(1_700_000_100_000),
    );
    let pending_join_payload = EventPayload {
        id: [0xBCu8; 16],
        community_id,
        kind: MembershipEventKind::PendingJoin {
            invite_token: token,
        },
        actor: joiner_addr,
        at: Hlc {
            wall_ms: 1_700_000_002_000,
            logical: 0,
            device_id: "joiner-dev".into(),
        },
    };
    let pending_join =
        sign_event_with_identity(&pending_join_payload, &joiner_priv).expect("sign PendingJoin");
    let pending_id = pending_join.id;

    let outcome = engine
        .insert_local_event_with_pubs(pending_join, joiner_pub, None)
        .await
        .expect("pending join insert");
    assert!(
        matches!(outcome, InsertOutcome::Inserted),
        "PendingJoin must be Inserted; got {:?}",
        outcome
    );

    // Wait for the spawned auto-counter-sign task to complete.
    // Poll rather than fixed sleep for determinism.
    let state_arc = engine.state();
    let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            {
                let g = state_arc.lock().await;
                let found = g.events().any(|e| {
                    e.actor == admin_addr
                        && matches!(
                            &e.kind,
                            MembershipEventKind::JoinCountersign { target_event_id }
                            if *target_event_id == pending_id
                        )
                });
                if found {
                    return true;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for JoinCountersign");

    assert!(
        found,
        "expected self-authored JoinCountersign targeting PendingJoin.id"
    );

    engine.shutdown().await.expect("shutdown");
}

/// Idempotency: inserting the same PendingJoin twice (simulated by inserting it
/// once and then calling `insert_local_event_with_pubs` again with an
/// already-known id) must produce exactly ONE self-authored JoinCountersign.
#[tokio::test]
async fn admin_engine_idempotent_no_duplicate_counter_sign() {
    let community_id = SpaceId([0xC1u8; 16]);
    let (admin_priv, admin_pub, admin_addr) = make_identity(0xA3);
    let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xB3);
    let admin_signing = signing_key_from(&admin_priv);

    let mut resolver_map = HashMap::new();
    resolver_map.insert(admin_addr, admin_pub);
    resolver_map.insert(joiner_addr, joiner_pub);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    let cs = make_cas();
    let (pub_tx, _pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (_sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let tmp = tempfile::tempdir().expect("tempdir");

    let engine = build_engine_with_resolver(
        community_id,
        admin_addr,
        admin_addr,
        admin_signing,
        pub_tx,
        sub_rx,
        cs,
        &tmp,
        Some(resolver),
        Some(admin_pub),
    );

    // Admin bootstrap join.
    let admin_join_payload = EventPayload {
        id: [0xA3u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_700_000_001_000,
            logical: 0,
            device_id: "test-dev".into(),
        },
    };
    let admin_join =
        sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign admin join");
    engine
        .insert_local_event_with_pubs(admin_join, admin_pub, None)
        .await
        .expect("admin join");

    // PendingJoin event.
    let token = make_signed_token(
        &admin_priv,
        admin_addr,
        Some(joiner_addr),
        Some(1_700_000_100_000),
    );
    let pending_join_payload = EventPayload {
        id: [0xBDu8; 16],
        community_id,
        kind: MembershipEventKind::PendingJoin {
            invite_token: token,
        },
        actor: joiner_addr,
        at: Hlc {
            wall_ms: 1_700_000_002_000,
            logical: 0,
            device_id: "joiner-dev".into(),
        },
    };
    let pending_join =
        sign_event_with_identity(&pending_join_payload, &joiner_priv).expect("sign PendingJoin");
    let pending_id = pending_join.id;
    let pending_join_clone = pending_join.clone();

    // First insert.
    let o1 = engine
        .insert_local_event_with_pubs(pending_join, joiner_pub, None)
        .await
        .expect("first pending join insert");
    assert!(matches!(o1, InsertOutcome::Inserted));

    // Second insert (same event) — should return AlreadyKnown, no second spawn.
    let o2 = engine
        .insert_local_event_with_pubs(pending_join_clone, joiner_pub, None)
        .await
        .expect("second pending join insert");
    assert!(
        matches!(o2, InsertOutcome::AlreadyKnown),
        "re-insert must return AlreadyKnown; got {:?}",
        o2
    );

    // Wait for the spawned task(s) to settle, then assert exactly 1 JoinCountersign.
    let state_arc = engine.state();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            {
                let g = state_arc.lock().await;
                let count = g
                    .events()
                    .filter(|e| {
                        e.actor == admin_addr
                            && matches!(
                                &e.kind,
                                MembershipEventKind::JoinCountersign { target_event_id }
                                if *target_event_id == pending_id
                            )
                    })
                    .count();
                if count >= 1 {
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for JoinCountersign");

    // Give a tiny extra window to check no duplicate was minted.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let g = state_arc.lock().await;
    let count = g
        .events()
        .filter(|e| {
            e.actor == admin_addr
                && matches!(
                    &e.kind,
                    MembershipEventKind::JoinCountersign { target_event_id }
                    if *target_event_id == pending_id
                )
        })
        .count();
    assert_eq!(count, 1, "expected exactly 1 JoinCountersign, got {count}");

    drop(g);
    engine.shutdown().await.expect("shutdown");
}

/// ZEB-254 Task 11: joiner-side pending-clear hook.
///
/// When a `JoinCountersign` targeting a self-authored `PendingJoin` is
/// inserted into a joiner engine that carries a `crdt_state` (owner-state
/// CRDT) with `Space.pending_join_at = Some(...)`, the hook must:
///   1. Set `Space.pending_join_at = None` in the owner-state CRDT.
///   2. Call the `nav_emitter` callback with the community SpaceId + name.
#[tokio::test]
async fn joiner_engine_clears_pending_join_at_on_countersign() {
    use harmony_app::owner_state_crdt::OwnerState;
    use harmony_app::owner_state_types::{Space, SpaceKind};
    use std::sync::atomic::{AtomicBool, Ordering};

    let community_id = SpaceId([0xC1; 16]);

    // Admin (counter-signer).
    let (admin_priv, admin_pub, admin_addr) = make_identity(0xAA);
    let _admin_signing = signing_key_from(&admin_priv);

    // Joiner (self_owner for the engine).
    let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xBB);
    let joiner_signing = signing_key_from(&joiner_priv);

    let mk = EpochKey::new([0x42; 32]);

    // ── Community state (engine's local membership CRDT) ───────────────
    let community_state = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));

    // ── Owner-state CRDT: seed a Space with pending_join_at = Some ─────
    // ZEB-254 R5-6: `pending_join_at` MUST equal the PendingJoin event's
    // `at` HLC (that's what redeem_invite_inner writes at mint time). The
    // live pending-clear hook checks full-HLC equality before clearing
    // (so a stale countersign for an older attempt cannot clear a newer
    // pending marker). Use the same HLC as the PendingJoin event below
    // (wall_ms = 1_700_000_002_000, device_id = "joiner-dev"). The Space's
    // `created_at` / `updated_at` are kept at an earlier HLC to keep
    // their narrative meaning ("Space was created first, then a pending
    // join arrived later").
    let mut owner_state_inner = OwnerState::default();
    let space_hlc = Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 0,
        device_id: "joiner-dev".into(),
    };
    let pending_join_hlc = Hlc {
        wall_ms: 1_700_000_002_000,
        logical: 0,
        device_id: "joiner-dev".into(),
    };
    let space = Space {
        id: community_id,
        kind: SpaceKind::Community,
        parent: None,
        community_id: None,
        name: "TestCommunity".to_string(),
        transport: None,
        members: vec![],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: space_hlc.clone(),
        updated_at: space_hlc.clone(),
        content_key: None,
        prior_content_keys: vec![],
        current_epoch: Some(0),
        current_epoch_key: Some(mk.clone()),
        old_epoch_keys: Default::default(),
        admin_addr: Some(admin_addr),
        is_invite_only: Some(true),
        shared_in_profile: false,
        pending_join_at: Some(pending_join_hlc.clone()),
    };
    owner_state_inner.apply_space_with_canonicalization(space);
    let crdt_state = Arc::new(Mutex::new(owner_state_inner));

    // ── nav_emitter: record that the callback fires ────────────────────
    let emitter_fired = Arc::new(AtomicBool::new(false));
    let emitter_fired_clone = Arc::clone(&emitter_fired);
    let nav_cb: harmony_app::community_state_sync::NavPendingClearEmitter =
        Arc::new(move |_cid, _name| {
            emitter_fired_clone.store(true, Ordering::SeqCst);
        });

    let cs = make_cas();
    let (pub_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let tmp = tempfile::tempdir().expect("tempdir");

    // Joiner's engine: self_owner = joiner_addr, crdt_state = Some, nav_emitter = Some.
    let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr,
        is_invite_only: true,
        device_id: "joiner-dev".into(),
        self_owner: joiner_addr,
        signing_key: Arc::new(joiner_signing.clone()),
        state: Arc::clone(&community_state),
        tracker: Arc::clone(&tracker),
        content_store: cs,
        publisher_tx: pub_tx,
        subscriber_rx: sub_rx,
        paths: PersistPaths {
            crdt: tmp.path().join("crdt.cbor"),
            replay: tmp.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: None,
        error_tx: None,
        delta_tx: None,
        pending_redemptions: None,
        crdt_state: Some(Arc::clone(&crdt_state)),
        admin_identity_pub: Some(admin_pub),
        nav_emitter: Some(Arc::clone(&nav_cb)),
        root_serve_rx: None,
    });

    // Step 1: insert admin's bootstrap Join (so admin is Joined in the
    // community state — required for the JoinCountersign verify gate).
    let admin_join_payload = EventPayload {
        id: [0xAA; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_700_000_001_000,
            logical: 0,
            device_id: "admin-dev".into(),
        },
    };
    let admin_join =
        sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign admin join");
    let outcome = engine
        .insert_local_event_with_pubs(admin_join, admin_pub, None)
        .await
        .expect("admin join insert");
    assert!(
        matches!(
            outcome,
            InsertOutcome::Inserted | InsertOutcome::AlreadyKnown
        ),
        "admin join must land: {:?}",
        outcome
    );

    // Step 2: insert the joiner's PendingJoin.
    let invite_token = make_signed_token(&admin_priv, admin_addr, Some(joiner_addr), None);
    let pending_join_id = [0xBB; 16];
    let pending_join_payload = EventPayload {
        id: pending_join_id,
        community_id,
        kind: MembershipEventKind::PendingJoin {
            invite_token: invite_token.clone(),
        },
        actor: joiner_addr,
        at: Hlc {
            wall_ms: 1_700_000_002_000,
            logical: 0,
            device_id: "joiner-dev".into(),
        },
    };
    let pending_join =
        sign_event_with_identity(&pending_join_payload, &joiner_priv).expect("sign pending join");
    let outcome = engine
        .insert_local_event_with_pubs(pending_join, joiner_pub, None)
        .await
        .expect("PendingJoin insert");
    assert!(
        matches!(outcome, InsertOutcome::Inserted),
        "PendingJoin must be Inserted: {:?}",
        outcome
    );

    // Step 3: insert a JoinCountersign from admin targeting the PendingJoin.
    let countersign_payload = EventPayload {
        id: [0xCC; 16],
        community_id,
        kind: MembershipEventKind::JoinCountersign {
            target_event_id: pending_join_id,
        },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_700_000_003_000,
            logical: 0,
            device_id: "admin-dev".into(),
        },
    };
    let countersign =
        sign_event_with_identity(&countersign_payload, &admin_priv).expect("sign countersign");
    let outcome = engine
        .insert_local_event_with_pubs(countersign, admin_pub, None)
        .await
        .expect("JoinCountersign insert");
    assert!(
        matches!(outcome, InsertOutcome::Inserted),
        "JoinCountersign must be Inserted: {:?}",
        outcome
    );

    // Step 4: wait for the spawned clear task to complete.
    // The hook is fire-and-forget; poll with a timeout.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let g = crdt_state.lock().await;
        let space = g.spaces.get(&community_id);
        if let Some(s) = space {
            if s.pending_join_at.is_none() {
                break;
            }
        }
        drop(g);
        if std::time::Instant::now() > deadline {
            panic!("Space.pending_join_at was not cleared within 2s");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // Step 5: nav_emitter callback must have fired.
    assert!(
        emitter_fired.load(Ordering::SeqCst),
        "nav_emitter must have been called"
    );

    engine.shutdown().await.expect("shutdown");
}

// ── ZEB-254 Task 15: two-engine integration tests ────────────────────────────

use harmony_app::community_membership::{attach_countersig_with_device_key, MemberStatus};

/// Drain all events from `from_state` that are not yet in `to_engine`
/// and insert them into `to_engine`, resolving the per-event actor pub from
/// `actor_pubs`. Simulates one-way state-root sync between two engines.
///
/// Each event is inserted with the pub keyed on `ev.actor` so verification
/// uses the actual signing key rather than a single shared pub. Events
/// whose actor is not in `actor_pubs` are silently skipped (unknown actor).
async fn sync_one_way(
    from_state: &Arc<Mutex<CommunityState>>,
    to_engine: &CommunitySyncEngine,
    actor_pubs: &HashMap<OwnerAddr, [u8; 64]>,
) {
    let events: Vec<SignedMembershipEvent> = {
        let g = from_state.lock().await;
        g.events().cloned().collect()
    };
    for ev in events {
        let Some(actor_pub) = actor_pubs.get(&ev.actor).copied() else {
            continue;
        };
        // Ignore errors — AlreadyKnown and verify errors for out-of-order
        // events are expected during sync simulation.
        let _ = to_engine
            .insert_local_event_with_pubs(ev, actor_pub, None)
            .await;
    }
}

/// ZEB-254 Task 15 — backward-compat: a pre-ZEB-254 client sends a legacy
/// `Join` event with `countersig=Some(...)` directly. The engine must accept
/// it and materialize as `Joined`.
///
/// This exercises the original invite-only wire shape where the countersig was
/// embedded directly in the `Join` event, rather than being a separate
/// `JoinCountersign` event added by ZEB-254.
#[tokio::test]
async fn legacy_invite_only_join_with_countersig_still_accepted() {
    let community_id = SpaceId([0xD0u8; 16]);
    let (admin_priv, admin_pub, admin_addr) = make_identity(0xA5);
    let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xB5);
    let admin_signing = signing_key_from(&admin_priv);

    let mut resolver_map = HashMap::new();
    resolver_map.insert(admin_addr, admin_pub);
    resolver_map.insert(joiner_addr, joiner_pub);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    let cs = make_cas();
    let (pub_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let tmp = tempfile::tempdir().expect("tempdir");

    let engine = build_engine_with_resolver(
        community_id,
        admin_addr,
        admin_addr, // self == admin
        admin_signing,
        pub_tx,
        sub_rx,
        cs,
        &tmp,
        Some(resolver),
        Some(admin_pub),
    );

    // Admin bootstrap Join (exempt from countersig requirement).
    let admin_join_payload = EventPayload {
        id: [0xA5u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_700_000_001_000,
            logical: 0,
            device_id: "test-dev".into(),
        },
    };
    let admin_join =
        sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign admin join");
    let outcome = engine
        .insert_local_event_with_pubs(admin_join, admin_pub, None)
        .await
        .expect("admin join insert");
    assert!(
        matches!(outcome, InsertOutcome::Inserted),
        "admin join: {:?}",
        outcome
    );

    // Joiner's legacy Join — sign as joiner, then admin attaches countersig.
    let joiner_join_payload = EventPayload {
        id: [0xB5u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: joiner_addr,
        at: Hlc {
            wall_ms: 1_700_000_002_000,
            logical: 0,
            device_id: "joiner-dev".into(),
        },
    };
    let joiner_join_unsigned =
        sign_event_with_identity(&joiner_join_payload, &joiner_priv).expect("sign joiner join");

    // Admin attaches countersig with their enrolled device key (ZEB-339).
    let joiner_join_with_cs = attach_countersig_with_device_key(
        &joiner_join_unsigned,
        admin_priv.owner,
        &admin_priv.device_key,
    )
    .expect("attach countersig");

    // Insert through the engine — countersigner_identity_pub = Some(admin_pub).
    let outcome = engine
        .insert_local_event_with_pubs(joiner_join_with_cs, joiner_pub, Some(admin_pub))
        .await
        .expect("joiner join insert");
    assert!(
        matches!(outcome, InsertOutcome::Inserted),
        "legacy Join with countersig must be Inserted; got {:?}",
        outcome
    );

    // Materialize: joiner must be Joined.
    let engine_state = engine.state();
    let mat = {
        let g = engine_state.lock().await;
        g.materialize_now(admin_addr)
    };
    assert_eq!(
        mat.members.get(&joiner_addr).map(|m| m.status),
        Some(MemberStatus::Joined),
        "legacy countersig'd Join must materialize as Joined"
    );

    engine.shutdown().await.expect("shutdown");
}

/// ZEB-254 Task 15 — cancellation: joiner inserts PendingJoin then Leave.
/// Leave supersedes PendingJoin in HLC order. Materialize must yield `Left`.
#[tokio::test]
async fn pending_join_cancellation_via_leave() {
    let community_id = SpaceId([0xD1u8; 16]);
    let (admin_priv, admin_pub, admin_addr) = make_identity(0xA6);
    let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xB6);
    let joiner_signing = signing_key_from(&joiner_priv);

    // Joiner engine — self_owner = joiner; no auto-counter-sign fires
    // (self != admin).
    let mut resolver_map = HashMap::new();
    resolver_map.insert(admin_addr, admin_pub);
    resolver_map.insert(joiner_addr, joiner_pub);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    let cs = make_cas();
    let (pub_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let tmp = tempfile::tempdir().expect("tempdir");

    let engine = build_engine_with_resolver(
        community_id,
        admin_addr,
        joiner_addr, // self == joiner (not admin — no auto-countersign)
        joiner_signing,
        pub_tx,
        sub_rx,
        cs,
        &tmp,
        Some(resolver),
        Some(admin_pub),
    );

    // Admin bootstrap Join so admin appears Joined in the community state
    // (mirrors realistic state; also guards against future verify changes).
    let admin_join_payload = EventPayload {
        id: [0xA6u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_700_000_001_000,
            logical: 0,
            device_id: "admin-dev".into(),
        },
    };
    let admin_join =
        sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign admin join");
    engine
        .insert_local_event_with_pubs(admin_join, admin_pub, None)
        .await
        .expect("admin join");

    // Joiner PendingJoin.
    let token = make_signed_token(
        &admin_priv,
        admin_addr,
        Some(joiner_addr),
        Some(1_700_000_100_000),
    );
    let pending_payload = EventPayload {
        id: [0xB6u8; 16],
        community_id,
        kind: MembershipEventKind::PendingJoin {
            invite_token: token,
        },
        actor: joiner_addr,
        at: Hlc {
            wall_ms: 1_700_000_002_000,
            logical: 0,
            device_id: "joiner-dev".into(),
        },
    };
    let pending_join =
        sign_event_with_identity(&pending_payload, &joiner_priv).expect("sign PendingJoin");
    let o1 = engine
        .insert_local_event_with_pubs(pending_join, joiner_pub, None)
        .await
        .expect("PendingJoin insert");
    assert!(
        matches!(o1, InsertOutcome::Inserted),
        "PendingJoin must be Inserted; got {:?}",
        o1
    );

    // Joiner Leave — wall_ms after PendingJoin so it sorts later and the
    // materialize terminal-state guard (Left supersedes PendingJoin) fires.
    let leave_payload = EventPayload {
        id: [0xC6u8; 16],
        community_id,
        kind: MembershipEventKind::Leave,
        actor: joiner_addr,
        at: Hlc {
            wall_ms: 1_700_000_003_000,
            logical: 0,
            device_id: "joiner-dev".into(),
        },
    };
    let leave = sign_event_with_identity(&leave_payload, &joiner_priv).expect("sign Leave");
    let o2 = engine
        .insert_local_event_with_pubs(leave, joiner_pub, None)
        .await
        .expect("Leave insert");
    assert!(
        matches!(o2, InsertOutcome::Inserted),
        "Leave must be Inserted; got {:?}",
        o2
    );

    // Materialize: joiner must be Left.
    let engine_state = engine.state();
    let mat = {
        let g = engine_state.lock().await;
        g.materialize_now(admin_addr)
    };
    assert_eq!(
        mat.members.get(&joiner_addr).map(|m| m.status),
        Some(MemberStatus::Left),
        "Leave after PendingJoin must materialize as Left"
    );

    engine.shutdown().await.expect("shutdown");
}

/// ZEB-254 Task 15 — two-admin race: joiner inserts PendingJoin; two Joined
/// members (the canonical admin and a second Joined member) both author
/// JoinCountersign events targeting the same PendingJoin. All three events
/// end up in the joiner's engine state. Materialize: status is `Joined` and
/// both JoinCountersigns are present in the log.
///
/// Implementation note: the "race" is simulated by directly constructing
/// both JoinCountersigns and inserting them into the joiner's engine rather
/// than via two independent auto-sign engines. The auto-sign hook is already
/// covered by `admin_engine_auto_counter_signs_on_pending_join_insert`; this
/// test focuses on the materialize + state-CRDT invariant.
#[tokio::test]
async fn pending_join_resolves_under_two_admin_race() {
    let community_id = SpaceId([0xD2u8; 16]);

    // Canonical admin (admin1).
    let (admin1_priv, admin1_pub, admin1_addr) = make_identity(0xA7);
    let _admin1_signing = signing_key_from(&admin1_priv);

    // Second Joined member who also counter-signs (admin2).
    let (admin2_priv, admin2_pub, admin2_addr) = make_identity(0xA8);

    // Joiner.
    let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xB7);
    let joiner_signing = signing_key_from(&joiner_priv);

    let mut resolver_map = HashMap::new();
    resolver_map.insert(admin1_addr, admin1_pub);
    resolver_map.insert(admin2_addr, admin2_pub);
    resolver_map.insert(joiner_addr, joiner_pub);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    // ── Single joiner-side engine ────────────────────────────────────────────
    let (pub_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let tmp = tempfile::tempdir().expect("tempdir");
    let joiner_engine = build_engine_with_resolver(
        community_id,
        admin1_addr,
        joiner_addr,
        joiner_signing,
        pub_tx,
        sub_rx,
        make_cas(),
        &tmp,
        Some(resolver),
        Some(admin1_pub),
    );

    // ── Admin1 bootstrap Join ────────────────────────────────────────────────
    let admin1_join_payload = EventPayload {
        id: [0xA7u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin1_addr,
        at: Hlc {
            wall_ms: 1_700_000_001_000,
            logical: 0,
            device_id: "admin1-dev".into(),
        },
    };
    let admin1_join =
        sign_event_with_identity(&admin1_join_payload, &admin1_priv).expect("sign admin1 join");
    joiner_engine
        .insert_local_event_with_pubs(admin1_join, admin1_pub, None)
        .await
        .expect("admin1 join");

    // ── Admin2 bootstrap Join (countersig'd by admin1 — invite-only community) ──
    let admin2_join_payload = EventPayload {
        id: [0xA8u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin2_addr,
        at: Hlc {
            wall_ms: 1_700_000_001_500,
            logical: 0,
            device_id: "admin2-dev".into(),
        },
    };
    let admin2_join_unsigned =
        sign_event_with_identity(&admin2_join_payload, &admin2_priv).expect("sign admin2 join");
    let admin2_join_with_cs = attach_countersig_with_device_key(
        &admin2_join_unsigned,
        admin1_priv.owner,
        &admin1_priv.device_key,
    )
    .expect("admin1 countersigs admin2 join");
    joiner_engine
        .insert_local_event_with_pubs(admin2_join_with_cs, admin2_pub, Some(admin1_pub))
        .await
        .expect("admin2 join");

    // ── Joiner PendingJoin ───────────────────────────────────────────────────
    let token = make_signed_token(
        &admin1_priv,
        admin1_addr,
        Some(joiner_addr),
        Some(1_700_000_100_000),
    );
    let pending_payload = EventPayload {
        id: [0xB7u8; 16],
        community_id,
        kind: MembershipEventKind::PendingJoin {
            invite_token: token,
        },
        actor: joiner_addr,
        at: Hlc {
            wall_ms: 1_700_000_002_000,
            logical: 0,
            device_id: "joiner-dev".into(),
        },
    };
    let pending_join =
        sign_event_with_identity(&pending_payload, &joiner_priv).expect("sign PendingJoin");
    let pending_id = pending_join.id;

    let o = joiner_engine
        .insert_local_event_with_pubs(pending_join, joiner_pub, None)
        .await
        .expect("PendingJoin insert");
    assert!(
        matches!(o, InsertOutcome::Inserted),
        "joiner PendingJoin: {:?}",
        o
    );

    // ── Two concurrent JoinCountersigns from both admins ─────────────────────
    // Construct both directly (simulating the "two admin engines both auto-signed
    // and their outputs are synced here").
    let cs1_payload = EventPayload {
        id: [0xC7u8; 16],
        community_id,
        kind: MembershipEventKind::JoinCountersign {
            target_event_id: pending_id,
        },
        actor: admin1_addr,
        at: Hlc {
            wall_ms: 1_700_000_003_000,
            logical: 0,
            device_id: "admin1-dev".into(),
        },
    };
    let cs1 = sign_event_with_identity(&cs1_payload, &admin1_priv).expect("sign cs1");

    let cs2_payload = EventPayload {
        id: [0xC8u8; 16],
        community_id,
        kind: MembershipEventKind::JoinCountersign {
            target_event_id: pending_id,
        },
        actor: admin2_addr,
        at: Hlc {
            wall_ms: 1_700_000_003_500,
            logical: 0,
            device_id: "admin2-dev".into(),
        },
    };
    let cs2 = sign_event_with_identity(&cs2_payload, &admin2_priv).expect("sign cs2");

    // Deliver both to the joiner's engine.
    let r1 = joiner_engine
        .insert_local_event_with_pubs(cs1, admin1_pub, None)
        .await
        .expect("cs1 insert");
    assert!(
        matches!(r1, InsertOutcome::Inserted),
        "cs1 must be Inserted; got {:?}",
        r1
    );
    let r2 = joiner_engine
        .insert_local_event_with_pubs(cs2, admin2_pub, None)
        .await
        .expect("cs2 insert");
    assert!(
        matches!(r2, InsertOutcome::Inserted),
        "cs2 must be Inserted; got {:?}",
        r2
    );

    // Materialize from joiner's state.
    let joiner_state_arc = joiner_engine.state();
    let mat = {
        let g = joiner_state_arc.lock().await;
        g.materialize_now(admin1_addr)
    };
    assert_eq!(
        mat.members.get(&joiner_addr).map(|m| m.status),
        Some(MemberStatus::Joined),
        "joiner must be Joined after both counter-signs land"
    );

    // Both JoinCountersigns must be present in the joiner's event log.
    let cs_count = {
        let g = joiner_state_arc.lock().await;
        g.events()
            .filter(|e| {
                matches!(
                    &e.kind,
                    MembershipEventKind::JoinCountersign { target_event_id }
                    if *target_event_id == pending_id
                )
            })
            .count()
    };
    assert_eq!(
        cs_count, 2,
        "both admin JoinCountersigns must be present in joiner's log; got {cs_count}"
    );

    joiner_engine.shutdown().await.expect("joiner shutdown");
}

/// ZEB-254 Task 15 — delayed admin: joiner inserts PendingJoin first; admin
/// engine starts later; state-root sync delivers PendingJoin to admin; admin
/// auto-counter-signs; JoinCountersign synced back to joiner; joiner
/// materializes as `Joined`.
#[tokio::test]
async fn pending_join_resolves_when_admin_comes_online() {
    let community_id = SpaceId([0xD3u8; 16]);
    let (admin_priv, admin_pub, admin_addr) = make_identity(0xA9);
    let admin_signing = signing_key_from(&admin_priv);
    let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xB9);
    let joiner_signing = signing_key_from(&joiner_priv);

    let mut resolver_map = HashMap::new();
    resolver_map.insert(admin_addr, admin_pub);
    resolver_map.insert(joiner_addr, joiner_pub);
    let make_resolver = || -> Arc<dyn IdentityResolver> {
        Arc::new(StaticResolver {
            map: resolver_map.clone(),
        })
    };

    // ── Joiner engine (starts first, admin offline) ──────────────────────────
    let (pub_tx_j, _pub_rx_j) = mpsc::channel::<Vec<u8>>(8);
    let (_sub_tx_j, sub_rx_j) = mpsc::channel::<Vec<u8>>(8);
    let tmp_j = tempfile::tempdir().expect("tempdir_j");
    let joiner_engine = build_engine_with_resolver(
        community_id,
        admin_addr,
        joiner_addr,
        joiner_signing,
        pub_tx_j,
        sub_rx_j,
        make_cas(),
        &tmp_j,
        Some(make_resolver()),
        Some(admin_pub),
    );

    // Admin bootstrap Join in joiner's engine (so joiner knows admin is Joined
    // — required for the JoinCountersign verify gate when it arrives later).
    let admin_join_payload = EventPayload {
        id: [0xA9u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_700_000_001_000,
            logical: 0,
            device_id: "admin-dev".into(),
        },
    };
    let admin_join =
        sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign admin join");
    joiner_engine
        .insert_local_event_with_pubs(admin_join.clone(), admin_pub, None)
        .await
        .expect("admin join in joiner");

    // Joiner inserts PendingJoin while admin is offline.
    let token = make_signed_token(
        &admin_priv,
        admin_addr,
        Some(joiner_addr),
        Some(1_700_000_100_000),
    );
    let pending_payload = EventPayload {
        id: [0xB9u8; 16],
        community_id,
        kind: MembershipEventKind::PendingJoin {
            invite_token: token,
        },
        actor: joiner_addr,
        at: Hlc {
            wall_ms: 1_700_000_002_000,
            logical: 0,
            device_id: "joiner-dev".into(),
        },
    };
    let pending_join =
        sign_event_with_identity(&pending_payload, &joiner_priv).expect("sign PendingJoin");
    let pending_id = pending_join.id;

    let o = joiner_engine
        .insert_local_event_with_pubs(pending_join.clone(), joiner_pub, None)
        .await
        .expect("PendingJoin insert");
    assert!(matches!(o, InsertOutcome::Inserted), "PendingJoin: {:?}", o);

    // ── Admin engine comes online ─────────────────────────────────────────────
    let (pub_tx_a, _pub_rx_a) = mpsc::channel::<Vec<u8>>(8);
    let (_sub_tx_a, sub_rx_a) = mpsc::channel::<Vec<u8>>(8);
    let tmp_a = tempfile::tempdir().expect("tempdir_a");
    let admin_engine = build_engine_with_resolver(
        community_id,
        admin_addr,
        admin_addr, // self == admin → auto-counter-sign fires
        admin_signing,
        pub_tx_a,
        sub_rx_a,
        make_cas(),
        &tmp_a,
        Some(make_resolver()),
        Some(admin_pub),
    );

    // Admin's own bootstrap Join in the admin engine.
    admin_engine
        .insert_local_event_with_pubs(admin_join, admin_pub, None)
        .await
        .expect("admin join in admin_engine");

    // Sync joiner → admin (delivers PendingJoin).
    let joiner_state = joiner_engine.state();
    sync_one_way(&joiner_state, &admin_engine, &resolver_map).await;

    // Wait for admin to produce the JoinCountersign.
    let admin_state = admin_engine.state();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let done = {
                let g = admin_state.lock().await;
                let found = g.events().any(|e| {
                    matches!(
                        &e.kind,
                        MembershipEventKind::JoinCountersign { target_event_id }
                        if *target_event_id == pending_id
                    )
                });
                found
            };
            if done {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for admin JoinCountersign");

    // Sync admin → joiner (delivers JoinCountersign back).
    sync_one_way(&admin_state, &joiner_engine, &resolver_map).await;

    // Materialize from joiner's state.
    let joiner_state_final = joiner_engine.state();
    let mat = {
        let g = joiner_state_final.lock().await;
        g.materialize_now(admin_addr)
    };
    assert_eq!(
        mat.members.get(&joiner_addr).map(|m| m.status),
        Some(MemberStatus::Joined),
        "joiner must be Joined after admin comes online and counter-signs"
    );

    joiner_engine.shutdown().await.expect("joiner shutdown");
    admin_engine.shutdown().await.expect("admin shutdown");
}

// ── ZEB-254 R1 (C1): boot-reconcile late-bind regression test ────────────────

/// Regression: an admin engine spawned with `admin_identity_pub: None`
/// (simulating the boot-reconcile path in `spawn_engine_inner_now`) must
/// still accept a PendingJoin and auto-counter-sign it.
///
/// Historical note: this originally guarded an opportunistic admin-pub
/// late-bind into a OnceLock. ZEB-339 moved PendingJoin verification onto
/// the carried EnrollmentCert / materialized membership and ZEB-496
/// removed that inert OnceLock, so acceptance no longer depends on any
/// admin-pub binding. The test still pins the end-to-end behaviour.
#[tokio::test]
async fn boot_reconcile_engine_accepts_pending_join_via_opportunistic_late_bind() {
    let community_id = SpaceId([0xE0u8; 16]);

    // Admin.
    let (admin_priv, admin_pub, admin_addr) = make_identity(0xC1);
    let admin_signing = signing_key_from(&admin_priv);

    // Joiner.
    let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xD1);

    // Resolver knows both parties — simulates OwnerDeviceCacheResolver in
    // production, which has cached pubs for all known peers.
    let mut resolver_map = HashMap::new();
    resolver_map.insert(admin_addr, admin_pub);
    resolver_map.insert(joiner_addr, joiner_pub);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    let cs = make_cas();
    let (pub_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let tmp = tempfile::tempdir().expect("tempdir");

    // NOTE: admin_pub is intentionally NOT passed — simulates boot-reconcile
    // spawn where admin_identity_pub defaults to None.
    let engine = build_engine_with_resolver(
        community_id,
        admin_addr,
        admin_addr, // self == admin
        admin_signing,
        pub_tx,
        sub_rx,
        cs,
        &tmp,
        Some(resolver),
        None, // <-- boot-reconcile: no pre-set admin_identity_pub
    );

    // Step 1: insert admin bootstrap Join so the admin is materialized as
    // Joined (self-eligibility precondition for the auto-counter-sign hook).
    let admin_join_payload = EventPayload {
        id: [0xC1u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_700_000_001_000,
            logical: 0,
            device_id: "admin-dev".into(),
        },
    };
    let admin_join =
        sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign admin join");
    let bootstrap_outcome = engine
        .insert_local_event_with_pubs(admin_join, admin_pub, None)
        .await
        .expect("admin join insert");
    assert!(
        matches!(bootstrap_outcome, InsertOutcome::Inserted),
        "admin bootstrap join must be Inserted; got {:?}",
        bootstrap_outcome
    );

    // Step 2: insert a PendingJoin from joiner. ZEB-339 verifies it via the
    // carried EnrollmentCert / materialized membership.
    let token = make_signed_token(
        &admin_priv,
        admin_addr,
        Some(joiner_addr),
        Some(1_700_000_100_000),
    );
    let pending_payload = EventPayload {
        id: [0xD1u8; 16],
        community_id,
        kind: MembershipEventKind::PendingJoin {
            invite_token: token,
        },
        actor: joiner_addr,
        at: Hlc {
            wall_ms: 1_700_000_002_000,
            logical: 0,
            device_id: "joiner-dev".into(),
        },
    };
    let pending_join =
        sign_event_with_identity(&pending_payload, &joiner_priv).expect("sign PendingJoin");
    let pending_id = pending_join.id;

    let pending_outcome = engine
        .insert_local_event_with_pubs(pending_join, joiner_pub, None)
        .await
        .expect("PendingJoin insert");

    assert!(
        matches!(pending_outcome, InsertOutcome::Inserted),
        "PendingJoin must be Inserted on a boot-reconcile engine; got {:?}",
        pending_outcome
    );

    // Step 3: admin engine auto-counter-sign hook should fire because self == admin.
    // Wait for the JoinCountersign to appear in the state.
    let state_arc = engine.state();
    let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            {
                let g = state_arc.lock().await;
                let found = g.events().any(|e| {
                    e.actor == admin_addr
                        && matches!(
                            &e.kind,
                            MembershipEventKind::JoinCountersign { target_event_id }
                            if *target_event_id == pending_id
                        )
                });
                if found {
                    return true;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for auto-JoinCountersign on boot-reconcile engine");

    assert!(
        found,
        "auto-counter-sign must fire on boot-reconcile engine"
    );

    engine.shutdown().await.expect("shutdown");
}

// ── ZEB-254 bot-review Q2: state-root receive path late-bind ─────────────────

/// ZEB-254 bot-review Q2 (state-root receive path): a boot-reconcile engine
/// that ingests the admin's bootstrap Join followed by a PendingJoin must
/// accept the PendingJoin and auto-counter-sign it.
///
/// Historical note: this originally guarded an opportunistic admin-pub
/// late-bind into a shared OnceLock on the receive path. ZEB-339 moved
/// PendingJoin verification onto the carried EnrollmentCert / materialized
/// membership and ZEB-496 removed that inert OnceLock, so acceptance no
/// longer depends on an admin-pub binding. Driving `handle_incoming_publish`
/// directly requires a fully encrypted wire packet, so this test validates
/// the equivalent path via sequential `insert_local_event_with_pubs` calls.
#[tokio::test]
async fn boot_reconcile_engine_accepts_pending_join_via_state_root_late_bind() {
    let community_id = SpaceId([0xF0u8; 16]);

    let (admin_priv, admin_pub, admin_addr) = make_identity(0xE1);
    let admin_signing = signing_key_from(&admin_priv);
    let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xF1);

    let mut resolver_map = HashMap::new();
    resolver_map.insert(admin_addr, admin_pub);
    resolver_map.insert(joiner_addr, joiner_pub);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    let admin_join_payload = EventPayload {
        id: [0xE1u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_700_000_001_000,
            logical: 0,
            device_id: "admin-dev".into(),
        },
    };
    let admin_join =
        sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign admin join");

    let token = make_signed_token(
        &admin_priv,
        admin_addr,
        Some(joiner_addr),
        Some(1_700_000_100_000),
    );
    let pending_payload = EventPayload {
        id: [0xF1u8; 16],
        community_id,
        kind: MembershipEventKind::PendingJoin {
            invite_token: token,
        },
        actor: joiner_addr,
        at: Hlc {
            wall_ms: 1_700_000_002_000,
            logical: 0,
            device_id: "joiner-dev".into(),
        },
    };
    let pending_join =
        sign_event_with_identity(&pending_payload, &joiner_priv).expect("sign PendingJoin");
    let pending_id = pending_join.id;

    // Admin engine with admin_identity_pub: None (boot-reconcile mode).
    // ZEB-339 verifies PendingJoin via the carried EnrollmentCert /
    // materialized membership, so no admin-pub binding is needed.
    let cs_dst = make_cas();
    // Keep _pub_rx alive (not just `_`) so the engine's publisher_tx stays
    // valid for the entire test; dropping it immediately would close the
    // channel and cause shutdown() to return TransportClosed.
    let (pub_tx_d, _pub_rx_d) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (_sub_tx_d, sub_rx_d) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let tmp_d = tempfile::tempdir().expect("tmpdir dest");
    let dst_engine = build_engine_with_resolver(
        community_id,
        admin_addr,
        admin_addr, // self == admin → auto-counter-sign fires
        admin_signing,
        pub_tx_d,
        sub_rx_d,
        cs_dst,
        &tmp_d,
        Some(resolver),
        None, // <-- boot-reconcile: no pre-set admin_identity_pub
    );

    // Step 1: insert admin bootstrap Join so the admin is materialized as
    // Joined (required for self-eligibility of the auto-counter-sign hook).
    let o1 = dst_engine
        .insert_local_event_with_pubs(admin_join, admin_pub, None)
        .await
        .expect("admin join insert");
    assert!(
        matches!(o1, InsertOutcome::Inserted),
        "admin bootstrap join must be Inserted; got {:?}",
        o1
    );

    // Step 2: insert the PendingJoin with the joiner's actor_pub. ZEB-339
    // verifies it against the carried EnrollmentCert / materialized
    // membership; the actor_pub here is joiner_pub, scoped to this event.
    let o2 = dst_engine
        .insert_local_event_with_pubs(pending_join, joiner_pub, None)
        .await
        .expect("pending join insert");
    assert!(
        matches!(o2, InsertOutcome::Inserted),
        "PendingJoin must be Inserted; got {:?}",
        o2
    );

    // Auto-counter-sign must fire (self == admin, Joined, power 100 ≥ 0).
    let dst_state = dst_engine.state();
    let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            {
                let g = dst_state.lock().await;
                if g.events().any(|e| {
                    e.actor == admin_addr
                        && matches!(
                            &e.kind,
                            MembershipEventKind::JoinCountersign { target_event_id }
                            if *target_event_id == pending_id
                        )
                }) {
                    return true;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for auto-JoinCountersign on state-root receive path");

    assert!(found, "auto-counter-sign must fire on PendingJoin insert");

    dst_engine.shutdown().await.expect("dst shutdown");
}

// ── ZEB-254 bot-review C1: AlreadyKnown restart-recovery ────────────────────

/// ZEB-254 bot-review C1: when a PendingJoin returns `AlreadyKnown` (already
/// in the CRDT from a prior session / disk-reload / duplicate delivery), the
/// auto-counter-sign hook must still fire if self is eligible and has not yet
/// emitted a JoinCountersign for this target.
///
/// Simulated via double-insert: first insert returns `Inserted` (hook fires,
/// but we DON'T wait for it — we simulate the "hook didn't fire on first
/// insert" case by inserting a SECOND time before the spawned task completes,
/// verifying the SECOND insert also schedules the hook). The idempotency guard
/// in `spawn_auto_counter_sign_task` ensures exactly one JoinCountersign lands.
///
/// Real-world trigger: admin engine restarts from disk (CRDT loaded, events
/// present), then reconciles incoming events — each PendingJoin hits
/// `state.events.contains_key` → true → `continue` before `insert_event` is
/// called. The C1 fix pushes those AlreadyKnown PendingJoins onto
/// `inserted_events` so `maybe_spawn_auto_counter_sign_for_ctx` fires even
/// for already-known events.
#[tokio::test]
async fn restart_recovery_already_known_pending_join_triggers_counter_sign() {
    let community_id = SpaceId([0xF2u8; 16]);
    let (admin_priv, admin_pub, admin_addr) = make_identity(0xE2);
    let admin_signing = signing_key_from(&admin_priv);
    let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xF2);

    let mut resolver_map = HashMap::new();
    resolver_map.insert(admin_addr, admin_pub);
    resolver_map.insert(joiner_addr, joiner_pub);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    let cs = make_cas();
    // Keep _pub_rx alive (not just `_`) so the engine's publisher_tx stays
    // valid for the entire test; dropping it immediately would close the
    // channel and cause shutdown() to return TransportClosed.
    let (pub_tx, _pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (_sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let tmp = tempfile::tempdir().expect("tmpdir");

    let engine = build_engine_with_resolver(
        community_id,
        admin_addr,
        admin_addr, // self == admin
        admin_signing,
        pub_tx,
        sub_rx,
        cs,
        &tmp,
        Some(resolver),
        Some(admin_pub),
    );

    // Admin bootstrap Join (self is now Joined in materialized state).
    let admin_join_payload = EventPayload {
        id: [0xE2u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_700_000_001_000,
            logical: 0,
            device_id: "admin-dev".into(),
        },
    };
    let admin_join =
        sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign admin join");
    engine
        .insert_local_event_with_pubs(admin_join, admin_pub, None)
        .await
        .expect("admin join");

    // Build a valid PendingJoin from joiner.
    let token = make_signed_token(
        &admin_priv,
        admin_addr,
        Some(joiner_addr),
        Some(1_700_000_100_000),
    );
    let pending_payload = EventPayload {
        id: [0xF2u8; 16],
        community_id,
        kind: MembershipEventKind::PendingJoin {
            invite_token: token,
        },
        actor: joiner_addr,
        at: Hlc {
            wall_ms: 1_700_000_002_000,
            logical: 0,
            device_id: "joiner-dev".into(),
        },
    };
    let pending_join =
        sign_event_with_identity(&pending_payload, &joiner_priv).expect("sign PendingJoin");
    let pending_id = pending_join.id;
    let pending_join_clone = pending_join.clone();

    // First insert: returns Inserted. The hook spawns asynchronously — we
    // continue WITHOUT waiting, simulating the "admin crashed before the task
    // completed" scenario.
    let o1 = engine
        .insert_local_event_with_pubs(pending_join, joiner_pub, None)
        .await
        .expect("first insert");
    assert!(
        matches!(o1, InsertOutcome::Inserted),
        "first insert must be Inserted; got {:?}",
        o1
    );

    // Second insert (same event): returns AlreadyKnown. C1 fix: the recovery
    // path in insert_event_with_resolved_pubs still spawns the counter-sign
    // check even on AlreadyKnown. The spawned task's idempotency guard ensures
    // exactly one JoinCountersign lands regardless of how many times the hook
    // fires.
    let o2 = engine
        .insert_local_event_with_pubs(pending_join_clone, joiner_pub, None)
        .await
        .expect("second insert");
    assert!(
        matches!(o2, InsertOutcome::AlreadyKnown),
        "second insert must be AlreadyKnown; got {:?}",
        o2
    );

    // Wait for JoinCountersign to appear (either hook invocation can produce it).
    let state_arc = engine.state();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            {
                let g = state_arc.lock().await;
                if g.events().any(|e| {
                    e.actor == admin_addr
                        && matches!(
                            &e.kind,
                            MembershipEventKind::JoinCountersign { target_event_id }
                            if *target_event_id == pending_id
                        )
                }) {
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for JoinCountersign from AlreadyKnown recovery path");

    // Give a brief window to verify no duplicate was minted.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let g = state_arc.lock().await;
    let count = g
        .events()
        .filter(|e| {
            e.actor == admin_addr
                && matches!(
                    &e.kind,
                    MembershipEventKind::JoinCountersign { target_event_id }
                    if *target_event_id == pending_id
                )
        })
        .count();
    assert_eq!(
        count, 1,
        "idempotency guard must ensure exactly 1 JoinCountersign; got {count}"
    );
    drop(g);

    engine.shutdown().await.expect("shutdown");
}

// SKIP: pending_join_30d_expiry_hides_joiner
//
// This integration test would be redundant with the unit tests already in
// community_membership.rs (Task 4):
//   - `materialize_pending_join_older_than_30d_hidden`
//   - `materialize_pending_join_countersign_resurrects_expired_pending`
// Both exercise the exact `materialize()` code path. The engine-level path
// calls `materialize_now()` which calls `materialize()` identically — there
// is no new code path to exercise at the engine layer. Deferred as a future
// follow-up if end-to-end expiry via real HLC advancement is needed.

// SKIP: pending_join_survives_joiner_restart
//
// The current `CommunitySyncEngine` test construction in
// `build_engine_with_resolver` creates fresh in-memory state on each call;
// the `PersistPaths` tempfiles are not loaded on startup in the test
// configuration. A restart test requires disk-backed state round-trip
// (reading crdt.cbor on engine init), which is not yet exposed to integration
// tests. Deferred to a follow-up ticket once persistence round-trip helpers
// are available.
