//! ZEB-596 coverage for the public
//! `pkarr_resolver_adapter::community_contexts_for_target`.
//!
//! The function resolves the target's 64-byte identity_pub ONCE up front (a
//! `None` short-circuits to an empty Vec), then walks the live
//! `CommunitySyncRegistry`. For every community where the target is a
//! currently-`Joined` member AND whose live epoch key is present in the
//! owner-state CRDT (`spaces[cid].current_epoch_key` + `current_epoch` both
//! `Some`, read via `live_epoch_key`), it yields one `PkarrCommunityContext`.
//! Communities where the target isn't Joined, or whose live epoch key is
//! missing, are skipped.
//!
//! Each test builds a real registry + spawned community engine, drives genuine
//! cert-bearing `Join` events into the per-community CRDT, and shares ONE
//! `Arc<Mutex<OwnerState>>` between the resolver (reads `owner_device_cache`)
//! and the `crdt_state` arg (reads `spaces[cid].current_epoch_key`) — mirroring
//! production's single owner-state CRDT.
//!
//! Cases:
//! - happy path: Joined + identity cached + live key present -> one context
//!   carrying the LIVE `Space.current_epoch_key` (deliberately different from
//!   the engine's spawn-time `membership_key`, proving the live key is used).
//! - rotation: resolve, mutate `spaces[cid].current_epoch_key`, resolve again
//!   -> the context carries the NEW key (live read on each call).
//! - non-member: target not Joined (identity still cached) -> empty.
//! - unresolvable identity: Joined + live key present, identity not cached ->
//!   empty via the hoisted-resolve early return.
//!
//! FIXTURE NOTE (identity_pub): members are minted via `mint_test_owner`, whose
//! `OwnerAddr` is the master signing-material hash (not `SHA256(composite)`),
//! so the 64-byte composite we cache (`[0;32] || master_ed25519`, mirroring
//! `mint_test_owner`'s master `PubKeyBundle` layout) is a faithful,
//! deterministic stand-in rather than a hash-consistent identity. The path
//! under test never re-derives that relationship and `lookup_pubkey_for_device`
//! matches purely by device-hash presence, so this stays a genuine end-to-end
//! exercise of the real `OwnerDeviceCacheResolver`.

use std::collections::{BTreeMap, HashMap};
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
    DeviceIdentityHash, EpochKey, Hlc, OwnerAddr, OwnerDeviceEntry, Space, SpaceId, SpaceKind,
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
/// enrolled device key from the carried cert and admit the Join into an open
/// (non-invite-only) community.
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

/// Minimal valid Community `Space` carrying a live epoch key/counter — the
/// pair `live_epoch_key` reads to source the per-community case-C key.
fn community_space(cid: SpaceId, admin: OwnerAddr, live_key: [u8; 32], epoch: u64) -> Space {
    let at = Hlc {
        wall_ms: 100,
        logical: 0,
        device_id: "space-dev".into(),
    };
    Space {
        id: cid,
        kind: SpaceKind::Community,
        parent: None,
        community_id: None, // a community Space IS the community
        name: "ctx-test-community".into(),
        transport: None,
        members: vec![],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: at.clone(),
        updated_at: at,
        content_key: None,
        prior_content_keys: vec![],
        current_epoch: Some(epoch),
        current_epoch_key: Some(EpochKey::new(live_key)),
        old_epoch_keys: BTreeMap::new(),
        admin_addr: Some(admin),
        is_invite_only: Some(false),
        shared_in_profile: false,
        read_receipt_pref: None,
        pending_join_at: None,
    }
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

/// Keep-alive handles for a standing one-community registry. The underscore-
/// prefixed fields (tempdir the engine persists into, CAS servicer sender,
/// engine publisher-receiver / subscriber-sender) must outlive the
/// function-under-test call so the engine task doesn't latch shut.
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
        device_cipher: harmony_app::device_dataset_file::test_cipher(),
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
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
        presence_resync_rx: None,
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
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn engine");

    let ctx = VerifyContext {
        now_ms: None,
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

/// Build ONE shared owner-state CRDT holding both the resolver's device-cache
/// entries and the community `Space`s (with live epoch keys). The SAME Arc is
/// handed to `OwnerDeviceCacheResolver` and passed as the `crdt_state` arg.
fn build_shared_state(
    cache: &[(OwnerAddr, [u8; 64])],
    spaces: &[(SpaceId, OwnerAddr, [u8; 32], u64)],
) -> Arc<Mutex<OwnerState>> {
    let mut os = OwnerState::default();
    for (owner, pub64) in cache {
        os.owner_device_cache.devices.insert(
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
    for (cid, admin, live_key, epoch) in spaces {
        os.spaces
            .insert(*cid, community_space(*cid, *admin, *live_key, *epoch));
    }
    Arc::new(Mutex::new(os))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Happy path: admin + bob Joined, bob's identity_pub cached, and the
/// community's live `Space.current_epoch_key` is set to a key DELIBERATELY
/// different from the engine's spawn-time `membership_key` (`mk`). The function
/// must return exactly one context whose `epoch_key` is the LIVE key (not `mk`)
/// and whose `target_member_identity_pub` is bob's cached pub.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn happy_path_uses_live_epoch_key_not_spawn_membership_key() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let admin = mint_test_owner(0xA1);
        let bob = mint_test_owner(0xB2);
        let bob_pub = master_identity_pub(0xB2);

        let mk_bytes = [0x42u8; 32];
        let live_key = [0x77u8; 32]; // != mk on purpose
        let s = setup_one_community(&admin, &[&bob], mk_bytes, [0x11u8; 16]).await;

        let shared = build_shared_state(
            &[(bob.owner, bob_pub)],
            &[(s.community_id, admin.owner, live_key, 3)],
        );
        let resolver = OwnerDeviceCacheResolver::new(Arc::clone(&shared), admin.owner, [0u8; 64]);

        let ctxs = community_contexts_for_target(&s.registry, &shared, &resolver, bob.owner).await;

        assert_eq!(
            ctxs.len(),
            1,
            "exactly one Joined community with a live key"
        );
        let ctx = &ctxs[0];
        assert_eq!(ctx.community_id, s.community_id);
        assert_ne!(
            live_key,
            *s.mk.as_bytes(),
            "test misconfigured: live key must differ from the spawn-time mk"
        );
        assert_eq!(
            ctx.epoch_key, live_key,
            "epoch_key must be the LIVE Space.current_epoch_key, not the engine's mk"
        );
        assert_eq!(
            ctx.target_member_identity_pub, bob_pub,
            "target_member_identity_pub must be bob's resolved 64-byte identity_pub"
        );
    })
    .await
    .expect("happy_path timed out at 30s");
}

/// Rotation: the walk reads the live key on EVERY call. Resolve once (live key
/// A), then mutate `spaces[cid].current_epoch_key` to a third value (simulating
/// an epoch rotation) and resolve again — the new context must carry the NEW
/// key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_reflects_updated_live_epoch_key() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let admin = mint_test_owner(0xA1);
        let bob = mint_test_owner(0xB2);
        let bob_pub = master_identity_pub(0xB2);

        let key_a = [0x77u8; 32];
        let key_b = [0x99u8; 32];
        let s = setup_one_community(&admin, &[&bob], [0x42u8; 32], [0x44u8; 16]).await;

        let shared = build_shared_state(
            &[(bob.owner, bob_pub)],
            &[(s.community_id, admin.owner, key_a, 3)],
        );
        let resolver = OwnerDeviceCacheResolver::new(Arc::clone(&shared), admin.owner, [0u8; 64]);

        let before =
            community_contexts_for_target(&s.registry, &shared, &resolver, bob.owner).await;
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].epoch_key, key_a, "first resolve uses epoch key A");

        // Simulate an epoch rotation: bump the live key + counter in place.
        {
            let mut g = shared.lock().await;
            let space = g.spaces.get_mut(&s.community_id).expect("space present");
            space.current_epoch_key = Some(EpochKey::new(key_b));
            space.current_epoch = Some(4);
        }

        let after = community_contexts_for_target(&s.registry, &shared, &resolver, bob.owner).await;
        assert_eq!(after.len(), 1);
        assert_eq!(
            after[0].epoch_key, key_b,
            "after rotation the context must carry the NEW live epoch key"
        );
        assert_ne!(
            after[0].epoch_key, before[0].epoch_key,
            "the live key must actually have changed between calls"
        );
    })
    .await
    .expect("rotation timed out at 30s");
}

/// Non-member skipped: querying an owner who is NOT a Joined member yields an
/// empty Vec — even though that owner's identity_pub IS cached (so the hoisted
/// resolve succeeds) and the community has a live key, proving the skip is
/// driven by the membership gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_member_target_yields_empty() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let admin = mint_test_owner(0xA1);
        let bob = mint_test_owner(0xB2);
        let carol = mint_test_owner(0xC3); // minted but never joined
        let carol_pub = master_identity_pub(0xC3);

        let s = setup_one_community(&admin, &[&bob], [0x42u8; 32], [0x22u8; 16]).await;

        // Carol's pub IS resolvable and the community has a live key — so an
        // empty result can only mean the not-Joined gate fired.
        let shared = build_shared_state(
            &[(carol.owner, carol_pub)],
            &[(s.community_id, admin.owner, [0x77u8; 32], 3)],
        );
        let resolver = OwnerDeviceCacheResolver::new(Arc::clone(&shared), admin.owner, [0u8; 64]);

        let ctxs =
            community_contexts_for_target(&s.registry, &shared, &resolver, carol.owner).await;

        assert!(
            ctxs.is_empty(),
            "a non-member target must produce no contexts; got {}",
            ctxs.len()
        );
    })
    .await
    .expect("non_member timed out at 30s");
}

/// Unresolvable identity: the target IS Joined and the community has a live
/// key, but the target's identity_pub is NOT cached, so the hoisted
/// `resolver.resolve` returns `None` and the function returns empty before
/// walking any community.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joined_but_unresolvable_identity_yields_empty() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let admin = mint_test_owner(0xA1);
        let bob = mint_test_owner(0xB2);

        let s = setup_one_community(&admin, &[&bob], [0x42u8; 32], [0x33u8; 16]).await;

        // Sanity: bob really is Joined, so the empty result below is provably
        // caused by the None-resolution early return, not missing membership.
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
                "bob must be Joined for this test to exercise the None-resolution path"
            );
        }

        // Live key present, but bob's identity_pub is absent from the cache.
        let shared = build_shared_state(&[], &[(s.community_id, admin.owner, [0x77u8; 32], 3)]);
        let resolver = OwnerDeviceCacheResolver::new(Arc::clone(&shared), admin.owner, [0u8; 64]);

        let ctxs = community_contexts_for_target(&s.registry, &shared, &resolver, bob.owner).await;

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
