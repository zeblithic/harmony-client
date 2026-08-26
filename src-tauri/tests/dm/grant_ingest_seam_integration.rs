//! ZEB-548 Stage 2: the file-grant ingest SEAM tests, relocated from
//! `dm_inbox_ingest`'s inline test mod.
//!
//! These are the ZEB-674 / ZEB-723 / ZEB-730 tests that drive the PRODUCTION
//! `file_sharing::ProdFileGrantIngestor` through the spine's
//! `DmInboxIngestCtx` — integration tests of the PR-4 trait inversion. They
//! live at the harness tier because they need both sides of the seam: the
//! spine sweeper AND the orchestration-tier ingestor impl (which the spine
//! crate cannot name after extraction). The spine module's remaining inline
//! tests use a no-op ingestor and never reach up.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;

use harmony_app::butler_deposit::{
    self, decode_deposit_payload, encode_deposit_payload, DepositPayload,
};
use harmony_app::dm_inbox_crdt::{DmInboxDoc, DmInboxEntry};
use harmony_app::dm_inbox_ingest::{ingest_pending, DmInboxIngestCtx, ProdDmInboxIngestCtx};
use harmony_app::dm_signing::open_from_owner_with_info;
use harmony_app::file_sharing::{
    open_dek_at_rest, open_received_file, seal_grant_for_devices, FileGrantInner,
    ProdFileGrantIngestor,
};
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_crypto::canonical_cbor_decode;
use harmony_app::owner_state_types::{ContentId, Hlc, OwnerAddr, ReceivedFileGrant};

const SELF_ID: &str = "self-device-64hex";
/// ZEB-674: deterministic seeds for the grant-path Prod-ctx test. The device
/// ed25519 seed drives BOTH the ctx's X25519 private (via
/// `ed25519_priv_to_x25519`) AND the pubkey a grant is sealed to (via
/// `ed25519_pub_to_x25519`) — mirroring production's device-key derivation —
/// and the keytree seed drives the grantee's shared re-seal tree.
const TEST_DEVICE_ED25519_SEED: [u8; 32] = [0x33; 32];
const TEST_KEYTREE_SEED: [u8; 32] = [0x44; 32];

/// The X25519 public key a grant must be sealed to so the prod ctx's device
/// key opens it (production's `birational(vk)` seal target).
fn test_device_x25519_pub() -> [u8; 32] {
    let sk = ed25519_dalek::SigningKey::from_bytes(&TEST_DEVICE_ED25519_SEED);
    harmony_app::dm_signing::ed25519_pub_to_x25519(&sk.verifying_key().to_bytes())
        .expect("valid x25519 pub")
}

/// The grantee's shared KeyTree matching the prod ctx's `owner_keytree` —
/// used to open the re-sealed DEK from another "device".
fn test_owner_keytree() -> harmony_app::owner_state_crypto::KeyTree {
    harmony_app::owner_state_crypto::KeyTree::derive(&TEST_KEYTREE_SEED).expect("keytree")
}

fn hlc(wall_ms: u64) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: "butler-device".into(),
    }
}

/// Build a `ProdDmInboxIngestCtx` over stub handles with a counting
/// `notify_owner_state_dirty` and the REAL `ProdFileGrantIngestor`, also
/// returning the `RecordingSink` handle so callers can assert emitted frames.
/// Returns `(ctx, crdt_state, dirty_counter, sink_handle)`.
fn prod_ctx_with_dirty_and_sink() -> (
    ProdDmInboxIngestCtx,
    Arc<Mutex<OwnerState>>,
    Arc<AtomicUsize>,
    Arc<harmony_app::node_event_sink::RecordingSink>,
) {
    let crdt_state = Arc::new(Mutex::new(OwnerState::default()));
    let content_store: Arc<dyn harmony_app::content_store::ContentStore> =
        Arc::new(harmony_app::content_store::InMemoryStub::default());
    let sink_handle = harmony_app::node_event_sink::RecordingSink::new();
    let sink: Arc<dyn harmony_app::node_event_sink::NodeEventSink> =
        Arc::new(Arc::clone(&sink_handle));
    let dirty = Arc::new(AtomicUsize::new(0));
    let notify: Arc<dyn Fn() + Send + Sync> = {
        let dirty = Arc::clone(&dirty);
        Arc::new(move || {
            dirty.fetch_add(1, Ordering::SeqCst);
        })
    };
    let ctx = ProdDmInboxIngestCtx {
        device_id: SELF_ID.to_string(),
        self_owner: OwnerAddr([0x01; 16]),
        crdt_state: Arc::clone(&crdt_state),
        content_store,
        sink,
        pending_dm_invites: None,
        enrolled: BTreeSet::new(),
        revoked: harmony_app::revoked_device_projection::RevokedDeviceProjection::new(),
        notify_owner_state_dirty: Some(notify),
        device_x25519_priv: harmony_app::dm_signing::ed25519_priv_to_x25519(
            &ed25519_dalek::SigningKey::from_bytes(&TEST_DEVICE_ED25519_SEED),
        ),
        owner_keytree: Arc::new(test_owner_keytree()),
        file_grant_ingestor: Arc::new(ProdFileGrantIngestor),
    };
    (ctx, crdt_state, dirty, sink_handle)
}

/// ZEB-730 seed helper: install a `ReceivedFileGrant` on a seeded owner-state
/// with `granter_owner == granter` so `apply_grant_revoke`'s granter-of-record
/// authorization can be exercised without a full grant_push ingest.
async fn seed_received_grant(
    crdt_state: &Arc<Mutex<OwnerState>>,
    cid: [u8; 32],
    granter: OwnerAddr,
) {
    let mut state = crdt_state.lock().await;
    state.received_file_grants.insert(
        cid,
        ReceivedFileGrant {
            granter_owner: granter,
            cid,
            file_name: "doc.pdf".into(),
            file_size: 10,
            mime: "application/pdf".into(),
            sealed_dek: vec![1, 2, 3],
            received_at: 100,
        },
    );
}

/// ZEB-674 (C4) sweeper integration: a grant-only entry carrying a REAL
/// `grant_push` (sealed to this ctx's device key) is swept end-to-end through
/// the PRODUCTION `apply_grant_push` — it lands on `received_file_grants`,
/// fires `notify_owner_state_dirty` exactly once, and the stored DEK is
/// openable BOTH via `open_received_file` (the grantee read path) AND
/// directly via `open_dek_at_rest` with a freshly-derived KeyTree of the same
/// material (a DIFFERENT device with the same shared KeyTree — device-
/// agnostic, mirroring `file_deks`). The granter recorded is the entry's
/// butler-verified `sender_owner`.
#[tokio::test]
async fn sweep_ingests_real_grant_push_via_prod_ctx_device_agnostic() {
    let (ctx, crdt_state, dirty, _sink) = prod_ctx_with_dirty_and_sink();

    // A real sealed grant, targeted at the ctx device's X25519 pubkey.
    let dek_bytes = [0x5Au8; 32];
    let cid_bytes = [0xC1u8; 32];
    let inner = FileGrantInner {
        cid: cid_bytes,
        file_name: "shared.md".into(),
        file_size: 42,
        mime: "text/markdown".into(),
        dek: dek_bytes,
    };
    let sealed = seal_grant_for_devices(&inner, &[test_device_x25519_pub()]).expect("seal");
    let list: Vec<serde_bytes::ByteBuf> =
        sealed.into_iter().map(serde_bytes::ByteBuf::from).collect();
    let mut grant_push = Vec::new();
    ciborium::into_writer(&list, &mut grant_push).expect("encode grant_push");

    let granter = OwnerAddr([0xB0; 16]);
    let key = DmInboxDoc::grant_key(&granter.0, &grant_push);
    // Deposit "now" (the Prod ctx's `now_ms` is the real wall clock, and an
    // empty enrolled set disables coverage-GC) so the entry survives the
    // sweep's TTL check and we can assert it was marked ingested.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let entry = DmInboxEntry {
        sender_owner: granter.0,
        cidnotify_packet: None,
        storage_blob: Vec::new(),
        invite_packet: None,
        revocation_push: None,
        grant_push: Some(grant_push),
        grant_revoke: None,
        deposited_at: hlc(now),
        deposited_by: "butler-device".into(),
        ingested_by: Default::default(),
    };
    let mut doc = DmInboxDoc::default();
    doc.entries.insert(key.clone(), entry);

    let changed = ingest_pending(&mut doc, &ctx).await;
    assert!(changed, "grant sweep mutated the doc (ig growth)");
    assert!(
        doc.entries[&key].ingested_by.contains(SELF_ID),
        "entry marked ingested"
    );
    assert_eq!(
        dirty.load(Ordering::SeqCst),
        1,
        "a recorded grant fires notify_owner_state_dirty exactly once"
    );

    let cid = ContentId::from_bytes(cid_bytes);
    let state = crdt_state.lock().await;
    let rec = state
        .received_file_grants
        .get(&cid_bytes)
        .expect("received_file_grants populated");
    assert_eq!(
        rec.granter_owner, granter,
        "granter is the butler-verified deposit sender"
    );
    assert_ne!(
        rec.sealed_dek.as_slice(),
        dek_bytes.as_slice(),
        "stored blob is the KeyTree-sealed envelope, never the raw DEK"
    );

    // (a) grantee read path recovers the DEK.
    let recovered =
        open_received_file(&state, &test_owner_keytree(), cid).expect("open received file");
    assert_eq!(recovered.as_bytes(), &dek_bytes, "recovered DEK matches");

    // (b) device-agnostic: a FRESH KeyTree of the same shared material (a
    // different device of the same owner) opens the stored blob directly.
    let other_device_tree = test_owner_keytree();
    let via_tree =
        open_dek_at_rest(&other_device_tree, &rec.sealed_dek).expect("open via shared KeyTree");
    assert_eq!(
        via_tree.as_bytes(),
        &dek_bytes,
        "any device with the shared KeyTree opens the re-sealed grant"
    );
}

/// ZEB-723: a genuinely-recorded grant (the `Some(cid)` branch of
/// `apply_grant_push`, same gate as `notify_owner_state_dirty`) must also
/// emit `shared-with-me-updated` so the grantee's "Shared with me" UI can
/// refresh and bump its unread badge. Drives a REAL per-device-sealed
/// `grant_push` through the production sweeper, exactly like
/// `sweep_ingests_real_grant_push_via_prod_ctx_device_agnostic`, and
/// asserts the emitted frame via the `RecordingSink` handle.
#[tokio::test]
async fn sweep_ingested_grant_emits_shared_with_me_updated() {
    let (ctx, _crdt_state, dirty, sink_handle) = prod_ctx_with_dirty_and_sink();

    let cid_bytes = [0xC1u8; 32];
    let inner = FileGrantInner {
        cid: cid_bytes,
        file_name: "shared.md".into(),
        file_size: 42,
        mime: "text/markdown".into(),
        dek: [0x5Au8; 32],
    };
    let sealed = seal_grant_for_devices(&inner, &[test_device_x25519_pub()]).expect("seal");
    let list: Vec<serde_bytes::ByteBuf> =
        sealed.into_iter().map(serde_bytes::ByteBuf::from).collect();
    let mut grant_push = Vec::new();
    ciborium::into_writer(&list, &mut grant_push).expect("encode grant_push");

    let granter = OwnerAddr([0xB0; 16]);
    let key = DmInboxDoc::grant_key(&granter.0, &grant_push);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let entry = DmInboxEntry {
        sender_owner: granter.0,
        cidnotify_packet: None,
        storage_blob: Vec::new(),
        invite_packet: None,
        revocation_push: None,
        grant_push: Some(grant_push),
        grant_revoke: None,
        deposited_at: hlc(now),
        deposited_by: "butler-device".into(),
        ingested_by: Default::default(),
    };
    let mut doc = DmInboxDoc::default();
    doc.entries.insert(key, entry);

    let changed = ingest_pending(&mut doc, &ctx).await;
    assert!(changed, "grant sweep mutated the doc");

    let frames = sink_handle.frames();
    let matching = frames
        .iter()
        .filter(|(name, payload)| {
            name == "shared-with-me-updated"
                && payload["cid"] == serde_json::json!(hex::encode(cid_bytes))
        })
        .count();
    assert_eq!(
        matching, 1,
        "exactly one shared-with-me-updated frame for the recorded grant's cid \
         (cardinality + idempotency — a single record must not double-emit); got {frames:?}"
    );

    // Re-delivery pass (CodeAnt PR #750): sweeping the same doc again must be
    // a no-op — the entry is already marked `ingested_by` this device, so no
    // second frame is emitted and owner-state dirty is not re-notified.
    let dirty_after_first = dirty.load(Ordering::SeqCst);
    let changed_again = ingest_pending(&mut doc, &ctx).await;
    assert!(
        !changed_again,
        "re-delivered sweep over an already-ingested entry must not mutate the doc"
    );
    let matching_after = sink_handle
        .frames()
        .iter()
        .filter(|(name, payload)| {
            name == "shared-with-me-updated"
                && payload["cid"] == serde_json::json!(hex::encode(cid_bytes))
        })
        .count();
    assert_eq!(
        matching_after, 1,
        "re-delivery must not emit a second shared-with-me-updated frame"
    );
    assert_eq!(
        dirty.load(Ordering::SeqCst),
        dirty_after_first,
        "re-delivery must not re-notify owner-state dirty"
    );
}

/// ZEB-730 (prod path): an AUTHORIZED grant-revoke (deposit sender ==
/// granter-of-record) applied through the PRODUCTION `apply_grant_revoke`
/// GCs the received-grant entry, stamps ZEB-727's tombstone, fires
/// `notify_owner_state_dirty` exactly once, and emits exactly one
/// `shared-with-me-updated` frame carrying the canonical lowercase-hex cid.
#[tokio::test]
async fn apply_grant_revoke_authorized_gcs_notifies_and_emits() {
    let (ctx, crdt_state, dirty, sink_handle) = prod_ctx_with_dirty_and_sink();

    let granter = OwnerAddr([0xB0; 16]);
    let cid = [0xC1u8; 32];
    seed_received_grant(&crdt_state, cid, granter).await;

    let entry = DmInboxEntry {
        sender_owner: granter.0, // butler-verified sender == granter-of-record
        cidnotify_packet: None,
        storage_blob: Vec::new(),
        invite_packet: None,
        revocation_push: None,
        grant_push: None,
        grant_revoke: Some(butler_deposit::encode_grant_revoke(cid)),
        deposited_at: hlc(500),
        deposited_by: "butler-device".into(),
        ingested_by: Default::default(),
    };

    ctx.apply_grant_revoke(&entry)
        .await
        .expect("an authorized grant-revoke applies");

    {
        let state = crdt_state.lock().await;
        assert!(
            !state.received_file_grants.contains_key(&cid),
            "authorized revoke GCs the received-grant entry"
        );
        assert!(
            state.dismissed_received_grants.contains_key(&cid),
            "authorized revoke stamps the ZEB-727 dismiss tombstone"
        );
    }
    assert_eq!(
        dirty.load(Ordering::SeqCst),
        1,
        "an authorized revoke fires notify_owner_state_dirty exactly once"
    );
    let frames = sink_handle.frames();
    let matching = frames
        .iter()
        .filter(|(name, payload)| {
            name == "shared-with-me-updated"
                && payload["cid"] == serde_json::json!(hex::encode(cid))
        })
        .count();
    assert_eq!(
        matching, 1,
        "exactly one shared-with-me-updated frame carrying the canonical \
         lowercase-hex cid; got {frames:?}"
    );
}

/// ZEB-730 SECURITY (prod path, griefing guard): a grant-revoke whose
/// butler-verified deposit sender is NOT the granter-of-record is a silent
/// no-op — entry intact, no tombstone, no notify, no emit — so no Active
/// friend can grief a grantee into losing a file they did not share. Returns
/// `Ok(())` (a dropped revoke is not an error).
#[tokio::test]
async fn apply_grant_revoke_unauthorized_is_noop() {
    let (ctx, crdt_state, dirty, sink_handle) = prod_ctx_with_dirty_and_sink();

    let granter = OwnerAddr([0xB0; 16]);
    let attacker = OwnerAddr([0x1A; 16]);
    let cid = [0xC1u8; 32];
    seed_received_grant(&crdt_state, cid, granter).await;

    // Deposit sender is the attacker, NOT the granter-of-record.
    let entry = DmInboxEntry {
        sender_owner: attacker.0,
        cidnotify_packet: None,
        storage_blob: Vec::new(),
        invite_packet: None,
        revocation_push: None,
        grant_push: None,
        grant_revoke: Some(butler_deposit::encode_grant_revoke(cid)),
        deposited_at: hlc(500),
        deposited_by: "butler-device".into(),
        ingested_by: Default::default(),
    };

    ctx.apply_grant_revoke(&entry)
        .await
        .expect("a dropped (unauthorized) revoke is not an error");

    {
        let state = crdt_state.lock().await;
        assert!(
            state.received_file_grants.contains_key(&cid),
            "the received grant is intact (griefing guard)"
        );
        assert!(
            state.dismissed_received_grants.is_empty(),
            "no tombstone minted from an unauthorized revoke"
        );
    }
    assert_eq!(
        dirty.load(Ordering::SeqCst),
        0,
        "no notify on an unauthorized revoke"
    );
    assert!(
        sink_handle.frames().is_empty(),
        "no shared-with-me-updated emit on an unauthorized revoke"
    );
}

/// Deterministic X25519 keypair for the butler-cannot-open seam test
/// (copied with the test from `butler_deposit::tests`).
fn make_x25519_keypair(seed_byte: u8) -> ([u8; 32], [u8; 32]) {
    use hkdf::Hkdf;
    use sha2::Sha256;
    use x25519_dalek::{PublicKey, StaticSecret};

    let seed = [seed_byte; 32];
    let hk = Hkdf::<Sha256>::new(None, &seed);
    let mut scalar = [0u8; 32];
    hk.expand(b"harmony-zeb-418-test-x25519-scalar", &mut scalar)
        .expect("HKDF 32 bytes always works");

    let secret = StaticSecret::from(scalar);
    let public = PublicKey::from(&secret);
    (scalar, *public.as_bytes())
}

/// ZEB-674 Task 3: the butler/relay carrying `grant_push` cannot open the
/// per-device sealed grant blobs inside it — each blob is sealed
/// end-to-end to a SPECIFIC grantee device's X25519 key
/// (`file_sharing::seal_grant_for_devices`), so an unrelated private key
/// (standing in for the butler, which never holds a grantee device's
/// key) fails to open it.
#[test]
fn butler_cannot_open_grant_push() {
    use harmony_app::file_sharing::FILE_GRANT_SEAL_INFO;

    let (grantee_priv, grantee_pub) = make_x25519_keypair(0x03);
    let (unrelated_priv, _unrelated_pub) = make_x25519_keypair(0x04);

    let inner = FileGrantInner {
        cid: [0x07; 32],
        file_name: "report.pdf".into(),
        file_size: 4096,
        mime: "application/pdf".into(),
        dek: [0x08; 32],
    };
    let sealed_blobs =
        seal_grant_for_devices(&inner, &[grantee_pub]).expect("seal grant for devices");

    // Build the realistic `grant_push` wire value: CBOR of
    // `Vec<serde_bytes Vec<u8>>` (each element a byte-string, not a
    // nested array of integers). `ByteBuf` is `serde_bytes`'s owned
    // byte-string newtype — encoding via `ciborium` directly (this local
    // `Vec<ByteBuf>` can't satisfy the module-private `CanonicalPayload`
    // sealed trait that `canonical_cbor_encode` requires; mirrors the
    // `LegacyDepositPayload` pattern above).
    let grant_push_list: Vec<serde_bytes::ByteBuf> = sealed_blobs
        .iter()
        .cloned()
        .map(serde_bytes::ByteBuf::from)
        .collect();
    let mut grant_push_bytes = Vec::new();
    ciborium::into_writer(&grant_push_list, &mut grant_push_bytes).expect("encode gp list");

    let payload = DepositPayload {
        cidnotify_packet: None,
        storage_blob: Vec::new(),
        invite_packet: None,
        revocation_push: None,
        grant_push: Some(grant_push_bytes.clone()),
        grant_revoke: None,
    };
    let wire = encode_deposit_payload(&payload).expect("encode payload with grant_push");
    let decoded = decode_deposit_payload(&wire).expect("decode payload with grant_push");
    let gp = decoded.grant_push.expect("grant_push present");

    // Decode the outer Vec<Vec<u8>> back out and attempt to open the
    // single per-device seal with an UNRELATED X25519 private key — the
    // butler/relay never holds a grantee device's private key.
    let blobs: Vec<serde_bytes::ByteBuf> =
        ciborium::from_reader(gp.as_slice()).expect("decode gp list");
    assert_eq!(blobs.len(), 1);
    let err = open_from_owner_with_info(&unrelated_priv, &blobs[0], FILE_GRANT_SEAL_INFO)
        .expect_err("unrelated key must not open the sealed grant");
    assert!(
        matches!(err, harmony_app::dm_signing::DmSignError::DecryptionFailed),
        "expected DecryptionFailed, got {err:?}"
    );

    // Sanity: the intended grantee CAN open it (proves the fixture is
    // realistic, not a vacuously-failing seal).
    let opened = open_from_owner_with_info(&grantee_priv, &blobs[0], FILE_GRANT_SEAL_INFO)
        .expect("grantee must open its own sealed grant");
    let back: FileGrantInner = canonical_cbor_decode(&opened).expect("decode inner");
    assert_eq!(back, inner);
}
