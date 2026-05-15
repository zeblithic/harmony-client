//! Regression test for ZEB-254 Issue 2: engine-level PendingJoin acceptance
//! when `admin_identity_pub` is bound via `bind_admin_identity_pub`.
//!
//! Before the fix, all engine-level callsites hard-coded
//! `admin_identity_pub: None`, so the P5 gate (InviteToken sig verification)
//! unconditionally rejected every PendingJoin event with
//! `PendingJoinTokenInvalid`. Tests passed only because they constructed
//! `VerifyContext` directly with `Some(&admin_pub)`, bypassing the engine.
//! This test inserts a PendingJoin through the engine's
//! `insert_local_event_with_pubs` path to confirm the plumbing is live.

use harmony_app::community_invite::{canonical_invite_token_bytes, InviteToken};
use harmony_app::community_membership::{
    sign_event_with_identity, EventPayload, MembershipEventKind,
};
use harmony_app::community_state_crdt::{CommunityState, InsertOutcome};
use harmony_app::community_state_sync::{
    CommunityRootHlcTracker, CommunitySyncEngine, CommunitySyncEngineConfig, PersistPaths,
    DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{CasOp, ContentStore, RuntimeContentStore};
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};
use harmony_identity::PrivateIdentity;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Build a `PrivateIdentity` from a seed byte, plus its 64-byte pub and OwnerAddr.
fn make_identity(seed: u8) -> (PrivateIdentity, [u8; 64], OwnerAddr) {
    let private = PrivateIdentity::from_seed(&[seed; 32]);
    let public = private.public_identity();
    let pub_bytes = public.to_public_bytes();
    let addr = OwnerAddr(public.address_hash);
    (private, pub_bytes, addr)
}

/// Extract an ed25519 `SigningKey` from a `PrivateIdentity`'s raw secret bytes.
/// The layout is `X25519_secret(32) || Ed25519_secret(32)`.
fn signing_key_from(id: &PrivateIdentity) -> ed25519_dalek::SigningKey {
    let bytes = id.to_private_bytes();
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&bytes[32..64]);
    ed25519_dalek::SigningKey::from_bytes(&secret)
}

/// Build a signed InviteToken with the admin's signing key.
fn make_signed_token(
    admin_private: &PrivateIdentity,
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
    tok.sig = admin_private.sign(&bytes);
    tok
}

/// Regression: PendingJoin inserted through the engine's
/// `insert_local_event_with_pubs` must be accepted (not rejected with
/// `PendingJoinTokenInvalid`) after `bind_admin_identity_pub` is called.
///
/// Before the ZEB-254 R1 fix, the 4 engine-level `VerifyContext`
/// constructions hard-coded `admin_identity_pub: None`, so P5 gate
/// unconditionally returned `PendingJoinTokenInvalid`.
#[tokio::test]
async fn pending_join_accepted_via_engine_insert_after_bind_admin_identity_pub() {
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
        admin_identity_pub: None, // starts unset — bind_admin_identity_pub sets it below
    });

    // ZEB-254 R1 fix: bind the admin identity pub so the engine's P5 gate
    // can verify PendingJoin InviteToken signatures.
    engine.bind_admin_identity_pub(admin_pub);

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
            joiner_identity_pub: joiner_pub,
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

    // Insert through the engine — this exercises the P5 gate using
    // `self.admin_identity_pub.get()` populated by `bind_admin_identity_pub`.
    let outcome = engine
        .insert_local_event_with_pubs(pending_join, joiner_pub, None)
        .await
        .expect("pending join insert");

    assert!(
        matches!(outcome, InsertOutcome::Inserted),
        "PendingJoin must be Inserted via engine after bind_admin_identity_pub; \
         got {:?} — P5 gate likely still using None admin_identity_pub",
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
            joiner_identity_pub: joiner_pub,
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
                let found = g.events.values().any(|e| {
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
            joiner_identity_pub: joiner_pub,
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
                    .events
                    .values()
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
        .events
        .values()
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
