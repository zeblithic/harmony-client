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
