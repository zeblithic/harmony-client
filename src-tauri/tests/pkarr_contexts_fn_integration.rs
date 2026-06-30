//! ZEB-596 coverage for the newly-public
//! `pkarr_resolver_adapter::community_contexts_for_target`.
//!
//! `community_contexts_for_target` walks the live `CommunitySyncRegistry`,
//! and for every community where the queried `target` is a currently-`Joined`
//! member AND whose 64-byte harmony identity_pub the supplied resolver can
//! resolve, yields one `PkarrCommunityContext { community_id, epoch_key,
//! target_member_identity_pub }`. It SKIPS communities where the target is not
//! Joined, and SKIPS communities where `resolver.resolve(target)` returns
//! `None` (without the identity_pub the case-C HKDF key can't be derived).
//!
//! These three tests build a real registry + spawned community engine (the
//! same harness shape as `community_sync/community_sync_integration.rs`), drive
//! genuine cert-bearing `Join` events into the per-community CRDT so the
//! materialized membership shows the target as Joined, and exercise the
//! function against a real `OwnerDeviceCacheResolver` reading a real
//! owner-device cache:
//!
//!   1. happy path — target Joined + identity_pub cached -> exactly one
//!      context, with the engine's epoch_key and the target's cached
//!      identity_pub.
//!   2. non-member skipped — querying a never-joined owner -> empty Vec (even
//!      though that owner's pub IS in the cache, proving the skip is
//!      membership-driven).
//!   3. unknown identity — target IS Joined but the resolver can't resolve its
//!      identity_pub (empty cache) -> empty Vec.
//!
//! FIXTURE NOTE (identity_pub): members are minted via `mint_test_owner`, whose
//! `OwnerAddr` is the master *signing-material* hash (not `SHA256(composite)`),
//! so the 64-byte composite we cache for a member (`[0;32] || master_ed25519`,
//! mirroring `mint_test_owner`'s master `PubKeyBundle` layout) is a faithful,
//! deterministic stand-in rather than a hash-consistent identity. The path under
//! test never re-derives the `OwnerAddr <-> composite` relationship (and
//! `lookup_pubkey_for_device` matches purely by device-hash presence), so this
//! is a genuine end-to-end exercise of the real `OwnerDeviceCacheResolver`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use harmony_app::community_membership::{
    mint_test_owner, sign_event, EventPayload, MemberStatus, MembershipEventKind,
    SignedMembershipEvent, TestOwner, VerifyContext,
};
use harmony_app::community_state_crdt::InsertOutcome;
use harmony_app::community_state_sync::{
    CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver, OwnerDeviceCacheResolver,
    DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{CasOp, ContentStore, RuntimeContentStore};
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{
    DeviceIdentityHash, EpochKey, Hlc, OwnerAddr, OwnerDeviceEntry, SpaceId,
};
use harmony_app::pkarr_resolver_adapter::community_contexts_for_target;
use tokio::sync::{mpsc, Mutex};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `mint_test_owner`'s enrolled device signing key, wrapped in `Arc` for the
/// registry/engine config and for signing membership events.
fn signing_key_from(owner: &TestOwner) -> Arc<ed25519_dalek::SigningKey> {
    Arc::new(owner.device_key.clone())
}

/// The 64-byte composite identity_pub for a `mint_test_owner(seed)` member's
/// MASTER identity: `[0u8; 32] || master_ed25519_verify`. `mint_test_owner`
/// builds its master `PubKeyBundle` with `x25519_pub = [0; 32]` and
/// `ed25519_verify = SigningKey::from_bytes(&[seed; 32]).verifying_key()`, so
/// this reconstructs that exact composite deterministically from the seed.
fn master_identity_pub(seed: u8) -> [u8; 64] {
    let vk = ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
        .verifying_key()
        .to_bytes();
    let mut out = [0u8; 64];
    out[32..].copy_from_slice(&vk);
    out
}

/// Build a cert-bearing self-`Join` event for `owner`. The enrollment cert is
/// attached (as the production `sign_event_with_identity` does for
/// identity-introducing events) so `verify_event` can resolve the signer's
/// enrolled device key from the carried cert and admit the Join into an
/// open (non-invite-only) community.
fn cert_bearing_join(
    owner: &TestOwner,
    community_id: SpaceId,
    id: u8,
    wall_ms: u64,
    device_id: &str,
) -> SignedMembershipEvent {
    let payload = EventPayload {
        id: [id; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: owner.owner,
        at: Hlc {
            wall_ms,
            logical: 0,
            device_id: device_id.into(),
        },
    };
    let mut ev = sign_event(&payload, &owner.device_key).expect("sign join");
    ev.enrollment = Some(owner.cert.clone());
    ev
}

/// In-memory CAS servicer shared by the engine's `RuntimeContentStore`. The
/// function under test never touches CAS, but `spawn_engine_inner_now` requires
/// a live content store; this is the minimal servicer the sync integration
/// tests use.
fn spawn_shared_cas() -> mpsc::Sender<CasOp> {
    let (tx, mut rx) = mpsc::channel::<CasOp>(64);
    let store: Arc<Mutex<HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    tokio::spawn(async move {
        while let Some(op) = rx.recv().await {
            match op {
                CasOp::PutLocal {
                    cid, blob, reply, ..
                } => {
                    store.lock().await.insert(cid, blob);
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(()));
                    }
                }
                CasOp::GetOrFetch { cid, reply, .. } => {
                    let v = store.lock().await.get(&cid).cloned();
                    let _ = reply.send(Ok(v));
                }
                CasOp::GetLocal { cid, reply } => {
                    let v = store.lock().await.get(&cid).cloned();
                    let _ = reply.send(v);
                }
                CasOp::AllowServeSubtree { reply, .. } => {
                    let _ = reply.send(Ok(0));
                }
            }
        }
    });
    tx
}

/// Everything a test needs after standing up a one-community registry. The
/// underscore-prefixed fields are keep-alive handles (the tempdir the engine
/// persists into, the CAS servicer sender, and the engine's publisher-receiver
/// / subscriber-sender) that must outlive the function-under-test call so the
/// engine task doesn't latch shut on a closed channel.
struct Setup {
    registry: CommunitySyncRegistry,
    community_id: SpaceId,
    mk: EpochKey,
    admin_addr: OwnerAddr,
    _dir: tempfile::TempDir,
    _cas_tx: mpsc::Sender<CasOp>,
    _pub_rx: mpsc::Receiver<Vec<u8>>,
    _sub_tx: mpsc::Sender<Vec<u8>>,
}

/// Stand up a registry with a single OPEN community whose admin (bootstrap
/// Join) and every owner in `extra_joined` are inserted as `Joined` members via
/// real `insert_event` (cert-verified, version-bumping) calls.
async fn setup_one_community(
    admin: &TestOwner,
    extra_joined: &[&TestOwner],
    mk_bytes: [u8; 32],
    cid_bytes: [u8; 16],
) -> Setup {
    let cas_tx = spawn_shared_cas();
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_tx.clone(),
        Duration::from_secs(2),
    ));

    // The registry's own resolver is only consulted on the engine's
    // receive/publish verify paths — neither `insert_event` (cert-based) nor
    // the function under test use it. An empty-cache `OwnerDeviceCacheResolver`
    // satisfies the config's `Arc<dyn IdentityResolver>` bound.
    let reg_resolver: Arc<dyn IdentityResolver> = Arc::new(OwnerDeviceCacheResolver::new(
        Arc::new(Mutex::new(OwnerState::default())),
        admin.owner,
        [0u8; 64],
    ));

    let dir = tempfile::tempdir().expect("tempdir");
    let registry = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "test-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: reg_resolver,
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: admin.owner,
        signing_key: signing_key_from(admin),
        crdt_state: None,
        nav_emitter: None,
    });

    let community_id = SpaceId(cid_bytes);
    let mk = EpochKey::new(mk_bytes);

    let (pub_tx, pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(8);

    registry
        .spawn_engine_inner_now(
            community_id,
            mk.clone(),
            admin.owner,
            false, // open community: plain self-Join is authorized
            pub_tx,
            sub_rx,
            None,
            None,
            None,
        )
        .await
        .expect("spawn engine");

    let ctx = VerifyContext {
        expected_community_id: community_id,
        admin_addr: admin.owner,
        is_invite_only: false,
    };
    let state = registry
        .state_for(&community_id)
        .await
        .expect("engine spawned");

    // Admin bootstrap Join.
    {
        let mut g = state.lock().await;
        let outcome = g.insert_event(
            cert_bearing_join(admin, community_id, 1, 100, "admin-dev"),
            &ctx,
        );
        assert!(
            matches!(outcome, InsertOutcome::Inserted),
            "admin bootstrap Join must insert: {outcome:?}"
        );
    }
    // Every extra member's Join.
    for (i, owner) in extra_joined.iter().enumerate() {
        let id = 2 + i as u8;
        let wall = 110 + i as u64;
        let dev = format!("member-{i}-dev");
        let mut g = state.lock().await;
        let outcome = g.insert_event(cert_bearing_join(owner, community_id, id, wall, &dev), &ctx);
        assert!(
            matches!(outcome, InsertOutcome::Inserted),
            "extra member Join must insert: {outcome:?}"
        );
    }

    Setup {
        registry,
        community_id,
        mk,
        admin_addr: admin.owner,
        _dir: dir,
        _cas_tx: cas_tx,
        _pub_rx: pub_rx,
        _sub_tx: sub_tx,
    }
}

/// Build an `OwnerDeviceCacheResolver` over a cache that maps each
/// `(owner, identity_pub)` pair so `resolve(owner)` returns that pub.
fn resolver_with_cached(
    self_owner: OwnerAddr,
    entries: &[(OwnerAddr, [u8; 64])],
) -> OwnerDeviceCacheResolver {
    let mut owner_state = OwnerState::default();
    for (owner, pub64) in entries {
        owner_state.owner_device_cache.devices.insert(
            *owner,
            OwnerDeviceEntry {
                devices: vec![DeviceIdentityHash(owner.0)],
                device_identity_pubs: vec![Some(*pub64)],
                device_tunnel_contacts: vec![None],
                learned_at: Hlc {
                    wall_ms: 50_000,
                    logical: 0,
                    device_id: "cache-dev".into(),
                },
            },
        );
    }
    OwnerDeviceCacheResolver::new(Arc::new(Mutex::new(owner_state)), self_owner, [0u8; 64])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Happy path: a community where both the admin and "bob" are Joined and bob's
/// identity_pub is present in the owner-device cache the resolver reads. The
/// function must return exactly one context whose `community_id` is the
/// community, `epoch_key` is the engine's membership key, and
/// `target_member_identity_pub` is bob's cached 64-byte identity_pub.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn happy_path_returns_one_context_with_epoch_key_and_identity_pub() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let admin = mint_test_owner(0xA1);
        let bob = mint_test_owner(0xB2);
        let bob_pub = master_identity_pub(0xB2);

        let s = setup_one_community(&admin, &[&bob], [0x42u8; 32], [0x11u8; 16]).await;

        // Genuine resolver over a real owner-device cache holding bob's pub.
        let resolver = resolver_with_cached(admin.owner, &[(bob.owner, bob_pub)]);

        let ctxs = community_contexts_for_target(&s.registry, &resolver, bob.owner).await;

        assert_eq!(
            ctxs.len(),
            1,
            "exactly one community has bob Joined with a resolvable identity_pub"
        );
        let ctx = &ctxs[0];
        assert_eq!(
            ctx.community_id, s.community_id,
            "context must carry the community bob is Joined in"
        );
        assert_eq!(
            ctx.epoch_key,
            *s.mk.as_bytes(),
            "epoch_key must equal the engine's membership_key bytes"
        );
        assert_eq!(
            ctx.target_member_identity_pub, bob_pub,
            "target_member_identity_pub must be bob's resolved 64-byte identity_pub"
        );
    })
    .await
    .expect("happy_path timed out at 30s");
}

/// Non-member skipped: querying an owner who is NOT a Joined member of any known
/// community yields an empty Vec — even though that owner's identity_pub IS in
/// the cache, proving the skip is driven by membership, not by resolution.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_member_target_yields_empty() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let admin = mint_test_owner(0xA1);
        let bob = mint_test_owner(0xB2);
        let carol = mint_test_owner(0xC3); // minted but never joined
        let carol_pub = master_identity_pub(0xC3);

        let s = setup_one_community(&admin, &[&bob], [0x42u8; 32], [0x22u8; 16]).await;

        // Carol's pub IS resolvable — so an empty result can only mean the
        // function correctly skipped her on the not-Joined gate.
        let resolver = resolver_with_cached(admin.owner, &[(carol.owner, carol_pub)]);

        let ctxs = community_contexts_for_target(&s.registry, &resolver, carol.owner).await;

        assert!(
            ctxs.is_empty(),
            "a non-member target must produce no contexts; got {} ",
            ctxs.len()
        );
    })
    .await
    .expect("non_member timed out at 30s");
}

/// Unknown identity skipped: the target IS a Joined member, but the resolver
/// cannot resolve its identity_pub (empty cache, target != resolver-self), so
/// the community is skipped and the result is empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joined_but_unresolvable_identity_yields_empty() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let admin = mint_test_owner(0xA1);
        let bob = mint_test_owner(0xB2);

        let s = setup_one_community(&admin, &[&bob], [0x42u8; 32], [0x33u8; 16]).await;

        // Sanity: bob really is Joined in the materialized membership, so the
        // empty result below is provably caused by the None-resolution skip and
        // not by missing membership.
        {
            let state = s
                .registry
                .state_for(&s.community_id)
                .await
                .expect("engine spawned");
            let g = state.lock().await;
            let members = g.materialized(s.admin_addr).members;
            assert_eq!(
                members.get(&bob.owner).map(|m| m.status),
                Some(MemberStatus::Joined),
                "bob must be Joined for this test to exercise the None-resolution skip"
            );
        }

        // Empty cache + self_owner = admin => resolve(bob) is None.
        let resolver = resolver_with_cached(admin.owner, &[]);

        let ctxs = community_contexts_for_target(&s.registry, &resolver, bob.owner).await;

        assert!(
            ctxs.is_empty(),
            "a Joined target whose identity_pub doesn't resolve must produce no \
             contexts; got {}",
            ctxs.len()
        );
    })
    .await
    .expect("joined_but_unresolvable timed out at 30s");
}
