//! ZEB-418 SP2 Phase 1 Task 9: end-to-end two-engine butler deposit
//! integration — the assembly proof for Tasks 1–8.
//!
//! Two `FleetSyncEngine<DmInboxDoc>` instances (same owner KeyTree, devices
//! A and B) are bridged by in-memory channels — the SP1 integration harness
//! pattern from `fleet_sync.rs`'s `engine_tests` (`two_engines_converge`).
//! A third "sender" identity (its own minted owner + EnrollmentCert) builds
//! a real sealed deposit frame via `butler_deposit::build_deposit_frame`
//! (the exact Task 8 sender construction) and deposits it on A through
//! `iroh_butler_acceptor::handle_deposit_core` (the exact Task 5 acceptor
//! pipeline).
//!
//! The five flow assertions (plan Task 9):
//!   1. the entry is durably persisted on A BEFORE the ack exists (D7),
//!      probed by a recording `FleetPersist` sink shared with an event
//!      chronology the test stamps "ack" into;
//!   2. B receives the entry via the SP1 fan-out (A's `flush_now` inside
//!      `persist_entry` → bridge → B's engine apply);
//!   3. B's ingestion sweep (`dm_inbox_ingest::ingest_pending`, Task 6)
//!      CAS-puts the storage blob, records `apply_inbox` with the
//!      plan-pinned `InboxEntry` shape, emits `dm-received`, and adds B to
//!      the entry's grow-only `ig` set;
//!   4. A observes B's `ig` ack after B's flush (doc-merge propagation);
//!   5. GC fires when coverage is complete: A ingests too (it is also a
//!      recipient device), the covering `ig = {A, B}` state replicates to
//!      B (publish-before-GC — coverage-GC removes only entries already
//!      covered at sweep START, so the covering state escapes before any
//!      replica destroys it), and BOTH replicas converge to an empty doc.
//!
//! Timing: explicit `flush_now` + bounded condition-polling — no fixed
//! sleeps (the SP1 harness's `wait_until` discipline).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use async_trait::async_trait;
use ed25519_dalek::SigningKey;
use harmony_app::butler_deposit::{
    build_deposit_frame, DepositPayload, BUTLER_DEPOSIT_SEAL_INFO, INBOX_GLOBAL_CAP,
    INBOX_PER_SENDER_CAP,
};
use harmony_app::community_membership::{mint_test_owner, TestOwner};
use harmony_app::content_store::{ContentStore, InMemoryStub};
use harmony_app::dm_envelope::{
    build_signed_cidnotify, decode_packet, encode_packet, DmCidNotifySigned, DmPacket,
};
use harmony_app::dm_inbox_crdt::{DmInboxDoc, DmInboxEntry};
use harmony_app::dm_inbox_ingest::{ingest_pending, DmInboxIngestCtx, VerifiedDmDeposit};
use harmony_app::dm_inbox_persist::DmInboxPersist;
use harmony_app::dm_signing::{
    derive_device_hash_from_identity_pub, ed25519_priv_to_x25519, open_from_owner_with_info,
    verify_dm_packet_signature,
};
use harmony_app::fleet_sync::{
    mint_next_hlc, FleetPersist, FleetSyncConfig, FleetSyncEngine, Merger, SyncError,
};
use harmony_app::friend_graph::FriendStatus;
use harmony_app::iroh_butler_acceptor::{
    handle_deposit_core, ButlerDepositCtx, DepositPersistVerdict, DepositReject,
};
use harmony_app::owner_state_crypto::KeyTree;
use harmony_app::owner_state_types::{
    DeviceIdentityHash, Hlc, InboxEntry, OwnerAddr, ReceivedMessage, SpaceId,
};
use harmony_content::cid::{ContentFlags, ContentId};
use harmony_owner::certs::{EnrollmentCert, EnrollmentIssuer};
use tokio::sync::{mpsc, Mutex};

/// The recipient owner (A + B's owner) address bytes.
const RECIPIENT_OWNER: [u8; 16] = [0x42; 16];

/// Body stand-in returned by the probe `verify` (production decrypts the
/// storage blob with the Space key — covered by `ProdDmInboxIngestCtx` /
/// the dm_outbox receive-path tests; this test pins the PIPELINE).
const TEST_BODY: &[u8] = b"deposited-dm-plaintext";

/// Device A — the butler that accepts the deposit. Its X25519 birational
/// twin is the frame's seal target, exactly the production derivation.
fn device_a_sk() -> SigningKey {
    SigningKey::from_bytes(&[0x61; 32])
}

/// Device B — the sibling that receives via fan-out and ingests.
fn device_b_sk() -> SigningKey {
    SigningKey::from_bytes(&[0x62; 32])
}

/// SP1 device id form: 64-hex (lowercase) of the device ed25519 verify key.
fn device_id(sk: &SigningKey) -> String {
    hex::encode(sk.verifying_key().to_bytes())
}

fn master_from_cert(cert: &EnrollmentCert) -> [u8; 32] {
    match &cert.issuer {
        EnrollmentIssuer::Master { master_pubkey } => master_pubkey.classical.ed25519_verify,
        other => panic!("test certs are Master-issued, got {other:?}"),
    }
}

/// The sender's DM-transport device identity (signs the inner CidNotify —
/// distinct from the cert-bound deposit device key). Synthetic identity_pub
/// per the dm_outbox/acceptor test convention: all-zero X25519 half + real
/// Ed25519 half.
fn dm_identity() -> (SigningKey, [u8; 64], DeviceIdentityHash) {
    let sk = SigningKey::from_bytes(&[0x53; 32]);
    let mut identity_pub = [0u8; 64];
    identity_pub[32..].copy_from_slice(sk.verifying_key().as_bytes());
    let hash = derive_device_hash_from_identity_pub(&identity_pub)
        .expect("synthetic identity_pub is valid");
    (sk, identity_pub, hash)
}

struct Fixture {
    sender: TestOwner,
    sender_master: [u8; 32],
    space_id: SpaceId,
    message_cid: ContentId,
    cidnotify_packet: Vec<u8>,
    storage_blob: Vec<u8>,
    identity_pub: [u8; 64],
    dm_device_hash: DeviceIdentityHash,
}

/// A fully valid deposit fixture from a third "sender" owner: minted owner
/// (master + enrolled device + Master EnrollmentCert), a signed CidNotify
/// packet, and a storage blob whose `ContentId::for_book(encrypted: true)`
/// matches the packet's message_cid.
fn fixture() -> Fixture {
    let sender = mint_test_owner(0x51);
    let space_id = SpaceId([0x77; 16]);
    let storage_blob = b"encrypted-dm-storage-blob-bytes".to_vec();
    let message_cid = ContentId::for_book(
        &storage_blob,
        ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .expect("cid for blob");
    let (dm_sk, identity_pub, dm_device_hash) = dm_identity();
    let signed = DmCidNotifySigned {
        space_id,
        message_cid,
        sender_owner_addr: sender.owner,
        sender_devices: vec![dm_device_hash],
        signing_device_hash: dm_device_hash,
    };
    let packet = build_signed_cidnotify(signed, &dm_sk).expect("build cidnotify");
    let cidnotify_packet = encode_packet(&packet).expect("encode cidnotify packet");
    let sender_master = master_from_cert(&sender.cert);
    Fixture {
        sender,
        sender_master,
        space_id,
        message_cid,
        cidnotify_packet,
        storage_blob,
        identity_pub,
        dm_device_hash,
    }
}

// =====================================================================
// SP1 two-engine harness (mirrors fleet_sync.rs engine_tests::build_engine
// / two_engines_converge — same owner KeyTree, shared CAS, in-memory mpsc
// bridge)
// =====================================================================

struct Built {
    engine: Arc<FleetSyncEngine<DmInboxDoc>>,
    doc: Arc<Mutex<DmInboxDoc>>,
    tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
    out_rx: mpsc::Receiver<Vec<u8>>,
    in_tx: mpsc::Sender<Vec<u8>>,
}

fn build_engine(
    device_id: &str,
    kt: Arc<KeyTree>,
    cas: Arc<dyn ContentStore>,
    persist: Arc<dyn FleetPersist<DmInboxDoc>>,
) -> Built {
    let (out_tx, out_rx) = mpsc::channel(64);
    let (in_tx, in_rx) = mpsc::channel(64);
    let doc = Arc::new(Mutex::new(DmInboxDoc::default()));
    let tracker = Arc::new(Mutex::new(BTreeMap::new()));
    let merger: Merger<DmInboxDoc> = Arc::new(|local, remote| local.merge_from(remote));
    let engine = Arc::new(FleetSyncEngine::new(FleetSyncConfig {
        keys: harmony_app::owner_state_crypto::FleetKeySet::new(kt),
        device_id: device_id.to_string(),
        state: Arc::clone(&doc),
        merger,
        replay_tracker: Arc::clone(&tracker),
        content_store: cas,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        persist,
        lookup_key_tag: b"dm-inbox-v1",
        debounce_ms: 50,
        publish_seen: true,
        on_applied: None, // sweeps are driven explicitly in this test
        sibling_acks: Arc::new(Mutex::new(BTreeMap::new())),
    }));
    Built {
        engine,
        doc,
        tracker,
        out_rx,
        in_tx,
    }
}

/// Bounded condition-polling helper (no fixed sleeps) — the SP1 harness's
/// `wait_until` shape.
async fn wait_until<F, Fut>(mut cond: F, timeout: Duration) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond().await {
            return true;
        }
        if tokio::time::Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

/// A's durability sink: delegates to the REAL `DmInboxPersist` (Task 2),
/// then stamps the shared chronology with whether the deposit key was in
/// the persisted snapshot. The stamp lands strictly AFTER the durable
/// write returns, and the test stamps "ack" right after
/// `handle_deposit_core` returns — so "persist:with-entry" preceding "ack"
/// in the chronology proves persist-before-ack (D7).
struct RecordingPersist {
    inner: DmInboxPersist,
    expected_key: String,
    chronology: Arc<StdMutex<Vec<String>>>,
}

impl FleetPersist<DmInboxDoc> for RecordingPersist {
    fn persist(
        &self,
        state: &DmInboxDoc,
        replay_tracker: &BTreeMap<String, Hlc>,
    ) -> Result<(), SyncError> {
        self.inner.persist(state, replay_tracker)?;
        let tag = if state.entries.contains_key(&self.expected_key) {
            "persist:with-entry"
        } else {
            "persist:without-entry"
        };
        self.chronology.lock().unwrap().push(tag.to_string());
        Ok(())
    }
}

// =====================================================================
// Butler deposit ctx over REAL doc/engine handles (mirrors
// ProdButlerDepositCtx method-for-method; the friend graph / device cache
// lookups come from test maps instead of a full OwnerState)
// =====================================================================

struct TestButlerCtx {
    self_owner: [u8; 16],
    device_id: String,
    friends: BTreeMap<[u8; 16], ([u8; 32], FriendStatus)>,
    /// ZEB-424: owners that share a live group-DM with self (the
    /// `shares_live_group_dm` source). Empty by default.
    group_co_members: std::collections::BTreeSet<[u8; 16]>,
    /// ZEB-424 (D28.1): `(space_id, sender_owner)` pairs the
    /// `space_live_group_dm_co_member` post-decrypt bind treats as a live
    /// `GroupDm` with both members. Empty by default.
    group_co_member_spaces: std::collections::BTreeSet<([u8; 16], [u8; 16])>,
    /// Sender DEVICE → (owner id, identity pub) — the
    /// `resolve_sender_device` source (mirrors the production
    /// `owner_device_cache` resolution).
    device_owners: BTreeMap<DeviceIdentityHash, ([u8; 16], [u8; 64])>,
    butler_sk: SigningKey,
    doc: Arc<Mutex<DmInboxDoc>>,
    tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
    engine: Arc<FleetSyncEngine<DmInboxDoc>>,
}

#[async_trait]
impl ButlerDepositCtx for TestButlerCtx {
    fn self_owner(&self) -> [u8; 16] {
        self.self_owner
    }

    async fn lookup_friend(&self, sender_owner: &[u8; 16]) -> Option<([u8; 32], FriendStatus)> {
        self.friends.get(sender_owner).copied()
    }

    async fn shares_live_group_dm(&self, sender_owner: &[u8; 16]) -> bool {
        self.group_co_members.contains(sender_owner)
    }

    async fn space_live_group_dm_co_member(
        &self,
        space_id: &[u8; 16],
        sender_owner: &[u8; 16],
    ) -> bool {
        self.group_co_member_spaces
            .contains(&(*space_id, *sender_owner))
    }

    fn now_secs(&self) -> u64 {
        // After the fixture cert's 1_700_000_000 signing timestamp.
        1_700_000_100
    }

    fn decrypt(&self, sealed_blob: &[u8]) -> Result<Vec<u8>, String> {
        open_from_owner_with_info(
            &ed25519_priv_to_x25519(&self.butler_sk),
            sealed_blob,
            BUTLER_DEPOSIT_SEAL_INFO,
        )
        .map_err(|e| format!("{e:?}"))
    }

    async fn resolve_sender_device(
        &self,
        device_hash: DeviceIdentityHash,
    ) -> Option<([u8; 16], [u8; 64])> {
        self.device_owners.get(&device_hash).copied()
    }

    fn device_id(&self) -> String {
        self.device_id.clone()
    }

    async fn mint_hlc(&self) -> Hlc {
        mint_next_hlc(&self.tracker, &self.device_id).await
    }

    /// ProdButlerDepositCtx::persist_entry verbatim: atomic
    /// persist-with-caps under the doc lock (occupied key → Duplicate, caps
    /// bypassed; vacant → per-sender + global quota check, CapExceeded
    /// inserts and flushes nothing), then `notify_dirty` + `flush_now()
    /// .await` (durable publish + persist) BEFORE returning —
    /// persist-before-ack, D7.
    async fn persist_entry(
        &self,
        key: String,
        entry: DmInboxEntry,
    ) -> Result<DepositPersistVerdict, String> {
        let verdict = {
            let mut doc = self.doc.lock().await;
            if doc.entries.contains_key(&key) {
                DepositPersistVerdict::Duplicate
            } else {
                let sender_pending = doc
                    .entries
                    .values()
                    .filter(|e| e.sender_owner == entry.sender_owner)
                    .count();
                if sender_pending >= INBOX_PER_SENDER_CAP || doc.entries.len() >= INBOX_GLOBAL_CAP {
                    return Ok(DepositPersistVerdict::CapExceeded);
                }
                doc.entries.insert(key, entry);
                DepositPersistVerdict::Inserted
            }
        };
        self.engine.notify_dirty();
        self.engine
            .flush_now()
            .await
            .map_err(|e| format!("flush_now: {e}"))?;
        Ok(verdict)
    }
}

// =====================================================================
// Ingest ctx with probe recorders (CAS / apply_inbox / emit) and REAL
// CidNotify verification (decode + signature + blob↔packet CID binding —
// the public-API slice of ProdDmInboxIngestCtx::verify; the OwnerState
// admission walk + Space-key decrypt are Task 7's prod ctx, unit-covered)
// =====================================================================

struct ProbeIngestCtx {
    device_id: String,
    identity_pubs: BTreeMap<DeviceIdentityHash, [u8; 64]>,
    enrolled: BTreeSet<String>,
    cas_blobs: StdMutex<Vec<Vec<u8>>>,
    applied: StdMutex<Vec<InboxEntry>>,
    /// (space_id, message_cid) byte pairs already in the "DM store" —
    /// drives apply_inbox's Inserted-vs-Merged idempotency like production
    /// (byte form because `ContentId` does not implement `Ord`).
    dm_store: StdMutex<BTreeSet<([u8; 16], Vec<u8>)>>,
    emitted: StdMutex<Vec<ReceivedMessage>>,
}

impl ProbeIngestCtx {
    fn new(
        device_id: String,
        identity_pubs: BTreeMap<DeviceIdentityHash, [u8; 64]>,
        enrolled: BTreeSet<String>,
    ) -> Self {
        Self {
            device_id,
            identity_pubs,
            enrolled,
            cas_blobs: StdMutex::new(Vec::new()),
            applied: StdMutex::new(Vec::new()),
            dm_store: StdMutex::new(BTreeSet::new()),
            emitted: StdMutex::new(Vec::new()),
        }
    }

    fn applied(&self) -> Vec<InboxEntry> {
        self.applied.lock().unwrap().clone()
    }

    fn emitted(&self) -> Vec<ReceivedMessage> {
        self.emitted.lock().unwrap().clone()
    }
}

#[async_trait]
impl DmInboxIngestCtx for ProbeIngestCtx {
    fn self_device_id(&self) -> String {
        self.device_id.clone()
    }

    async fn cas_put(&self, storage_blob: &[u8]) -> Result<(), String> {
        self.cas_blobs.lock().unwrap().push(storage_blob.to_vec());
        Ok(())
    }

    async fn verify(&self, entry: &DmInboxEntry) -> Result<VerifiedDmDeposit, String> {
        // Real packet decode — must be a CidNotify.
        let packet = decode_packet(
            entry
                .cidnotify_packet
                .as_deref()
                .expect("probe message entry has cidnotify_packet"),
        )
        .map_err(|e| format!("decode: {e}"))?;
        let DmPacket::CidNotify {
            signed,
            signature,
            signed_bytes,
        } = packet
        else {
            return Err("deposited packet is not a CidNotify".into());
        };
        // Sender consistency (mirrors the prod ctx's step 2).
        if signed.sender_owner_addr.0 != entry.sender_owner {
            return Err("packet sender_owner does not match deposit entry".into());
        }
        // Real signature verification against the cached identity pub.
        let identity_pub = self
            .identity_pubs
            .get(&signed.signing_device_hash)
            .ok_or("unknown signing device")?;
        verify_dm_packet_signature(
            &signed_bytes,
            &signature,
            identity_pub,
            signed.signing_device_hash,
        )
        .map_err(|e| format!("packet signature: {e:?}"))?;
        // Real blob ↔ packet CID binding (the DM send path's for_book flags).
        let computed = ContentId::for_book(
            &entry.storage_blob,
            ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .map_err(|e| format!("for_book: {e:?}"))?;
        if computed != signed.message_cid {
            return Err("storage blob CID does not match packet message_cid".into());
        }
        Ok(VerifiedDmDeposit {
            space_id: signed.space_id,
            message_cid: signed.message_cid,
            body: TEST_BODY.to_vec(),
            mime_type: "text/plain".into(),
            sent_at: Hlc {
                wall_ms: 42,
                logical: 0,
                device_id: "sender-dm-device".into(),
            },
        })
    }

    async fn apply_invite_only(&self, _entry: &DmInboxEntry) -> Result<(), String> {
        // ZEB-505 standalone invite-only deposits (`cidnotify_packet == None`)
        // are not exercised by these message-entry probes; the recovery path is
        // unit-covered in `dm_inbox_ingest.rs`. Succeed without applying.
        Ok(())
    }

    async fn apply_revocation(&self, _entry: &DmInboxEntry) -> Result<bool, String> {
        // ZEB-691 standalone device-revocation deposits (`cidnotify_packet` and
        // `invite_packet` both `None`) are not exercised by these message-entry
        // probes; the recover path is unit-covered in `dm_inbox_ingest.rs`.
        Ok(true)
    }

    async fn apply_grant_push(&self, _entry: &DmInboxEntry) -> Result<(), String> {
        // ZEB-674 standalone file-share grant deposits (only `grant_push` set)
        // are not exercised by these message-entry probes; the recover path is
        // unit-covered in `dm_inbox_ingest.rs`. Succeed without applying.
        Ok(())
    }

    async fn apply_inbox(&self, entry: InboxEntry) -> Result<bool, String> {
        let inserted = self
            .dm_store
            .lock()
            .unwrap()
            .insert((entry.space_id.0, entry.message_cid.to_bytes().to_vec()));
        self.applied.lock().unwrap().push(entry);
        Ok(inserted)
    }

    fn emit_dm_received(&self, msg: &ReceivedMessage) {
        self.emitted.lock().unwrap().push(msg.clone());
    }

    async fn enrolled_device_ids(&self) -> BTreeSet<String> {
        self.enrolled.clone()
    }

    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

/// One explicit ingestion + GC sweep under the doc lock, mirroring
/// `dm_inbox_ingest::sweep_once`'s lock + notify_dirty contract.
async fn sweep(
    doc: &Arc<Mutex<DmInboxDoc>>,
    ctx: &ProbeIngestCtx,
    engine: &Arc<FleetSyncEngine<DmInboxDoc>>,
) -> bool {
    let changed = {
        let mut guard = doc.lock().await;
        ingest_pending(&mut guard, ctx).await
    };
    if changed {
        engine.notify_dirty();
        engine.flush_now().await.expect("flush after sweep");
    }
    changed
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn butler_deposit_fans_out_ingests_acks_and_gcs() {
    let fx = fixture();
    let a_sk = device_a_sk();
    let b_sk = device_b_sk();
    let a_id = device_id(&a_sk);
    let b_id = device_id(&b_sk);
    let enrolled: BTreeSet<String> = [a_id.clone(), b_id.clone()].into();
    let key = DmInboxDoc::key(&fx.space_id.0, &fx.message_cid.to_bytes());

    // Same owner KeyTree on both devices + ONE shared CAS so both engines
    // name (and can fetch) the same root blobs — the SP1 harness setup.
    let kt = Arc::new(KeyTree::derive(&[0x42u8; 32]).expect("kt"));
    let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());

    let chronology: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let a_dir = tempfile::tempdir().expect("tempdir A");
    let b_dir = tempfile::tempdir().expect("tempdir B");
    let mut a = build_engine(
        &a_id,
        Arc::clone(&kt),
        Arc::clone(&cas),
        Arc::new(RecordingPersist {
            inner: DmInboxPersist {
                doc_path: a_dir.path().join("dm_inbox.cbor"),
                replay_path: a_dir.path().join("dm_inbox_replay.cbor"),
            },
            expected_key: key.clone(),
            chronology: Arc::clone(&chronology),
        }),
    );
    let mut b = build_engine(
        &b_id,
        Arc::clone(&kt),
        Arc::clone(&cas),
        Arc::new(DmInboxPersist {
            doc_path: b_dir.path().join("dm_inbox.cbor"),
            replay_path: b_dir.path().join("dm_inbox_replay.cbor"),
        }),
    );

    // In-memory bridge: A.out -> B.in and B.out -> A.in (the
    // two_engines_converge forwarder verbatim).
    let a_in = a.in_tx.clone();
    let b_in = b.in_tx.clone();
    let mut a_out = std::mem::replace(&mut a.out_rx, mpsc::channel(1).1);
    let mut b_out = std::mem::replace(&mut b.out_rx, mpsc::channel(1).1);
    let forwarder = tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(frame) = a_out.recv() => { let _ = b_in.send(frame).await; }
                Some(frame) = b_out.recv() => { let _ = a_in.send(frame).await; }
                else => break,
            }
        }
    });

    // ----- (1) Sender builds the real sealed frame; A accepts it ---------
    let cert_bytes = harmony_owner::cbor::to_canonical(&fx.sender.cert).expect("encode cert");
    let frame = build_deposit_frame(
        &RECIPIENT_OWNER,
        &fx.sender.owner.0,
        &cert_bytes,
        &fx.sender.device_key,
        &a_sk.verifying_key().to_bytes(),
        &DepositPayload {
            cidnotify_packet: Some(fx.cidnotify_packet.clone()),
            storage_blob: fx.storage_blob.clone(),
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
        },
    )
    .expect("sender-side frame build");

    // Admission prerequisites on A: the sender owner is an Active friend
    // (pinned master from its cert), and the sender's DM device identity
    // is cached UNDER THE SENDER'S OWNER (the inner CidNotify verification
    // resolves the signing device to its owner and binds it to
    // frame.sender_owner).
    let a_butler_ctx = TestButlerCtx {
        self_owner: RECIPIENT_OWNER,
        device_id: a_id.clone(),
        friends: [(fx.sender.owner.0, (fx.sender_master, FriendStatus::Active))].into(),
        group_co_members: std::collections::BTreeSet::new(),
        group_co_member_spaces: std::collections::BTreeSet::new(),
        device_owners: [(fx.dm_device_hash, (fx.sender.owner.0, fx.identity_pub))].into(),
        butler_sk: a_sk,
        doc: Arc::clone(&a.doc),
        tracker: Arc::clone(&a.tracker),
        engine: Arc::clone(&a.engine),
    };

    let ack = handle_deposit_core(&frame, &a_butler_ctx)
        .await
        .expect("valid deposit from an Active friend must be accepted");
    chronology.lock().unwrap().push("ack".to_string());

    assert_eq!(ack.space_id, fx.space_id.0);
    assert_eq!(ack.message_cid, fx.message_cid.to_bytes().to_vec());

    // Persist-before-ack (D7): the durability sink wrote a snapshot
    // CONTAINING the entry strictly before the ack value existed.
    {
        let log = chronology.lock().unwrap().clone();
        let first_persist_with_entry = log.iter().position(|e| e == "persist:with-entry");
        let ack_pos = log.iter().position(|e| e == "ack").expect("ack stamped");
        assert!(
            first_persist_with_entry.is_some_and(|p| p < ack_pos),
            "entry must be durably persisted on A BEFORE the ack exists; chronology: {log:?}"
        );
    }

    // A's doc holds the entry at ack time, exactly as deposited.
    {
        let doc = a.doc.lock().await;
        let entry = doc.entries.get(&key).expect("entry in A's doc at ack");
        assert_eq!(entry.sender_owner, fx.sender.owner.0);
        assert_eq!(entry.cidnotify_packet, Some(fx.cidnotify_packet.clone()));
        assert_eq!(entry.storage_blob, fx.storage_blob);
        assert_eq!(entry.deposited_by, a_id);
        assert!(entry.ingested_by.is_empty());
    }

    // ----- (2) Fan-out: A's persist_entry flush → bridge → B applies -----
    let b_doc_handle = Arc::clone(&b.doc);
    let converged = wait_until(
        || {
            let b_doc = Arc::clone(&b_doc_handle);
            let key = key.clone();
            async move { b_doc.lock().await.entries.contains_key(&key) }
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(
        converged,
        "B did not receive the entry via fan-out within 5s"
    );
    let deposited_at = {
        let doc = b.doc.lock().await;
        let entry = &doc.entries[&key];
        assert_eq!(entry.sender_owner, fx.sender.owner.0);
        assert_eq!(entry.cidnotify_packet, Some(fx.cidnotify_packet));
        assert_eq!(entry.storage_blob, fx.storage_blob);
        assert_eq!(entry.deposited_by, a_id);
        entry.deposited_at.clone()
    };

    // ----- (3) B's ingestion sweep: the normal DM receive path ----------
    let b_ingest_ctx = ProbeIngestCtx::new(
        b_id.clone(),
        [(fx.dm_device_hash, fx.identity_pub)].into(),
        enrolled.clone(),
    );
    let changed = sweep(&b.doc, &b_ingest_ctx, &b.engine).await;
    assert!(changed, "B's ingestion must mutate the doc (ig growth)");

    // CAS-put recorded with the deposited storage blob.
    assert_eq!(
        *b_ingest_ctx.cas_blobs.lock().unwrap(),
        vec![fx.storage_blob.clone()],
        "B must CAS-put the deposited storage blob"
    );
    // apply_inbox recorded with the plan-pinned InboxEntry shape
    // (InboxKey = space_id + message_cid; from = sender owner;
    // received_at = the butler's deposit HLC).
    let expected_inbox = InboxEntry {
        space_id: fx.space_id,
        message_cid: fx.message_cid,
        from: OwnerAddr(fx.sender.owner.0),
        received_at: deposited_at,
    };
    assert_eq!(b_ingest_ctx.applied(), vec![expected_inbox.clone()]);
    // The same dm-received emit the normal path produces.
    let emitted = b_ingest_ctx.emitted();
    assert_eq!(emitted.len(), 1, "exactly one dm-received emit on B");
    assert_eq!(emitted[0].inbox_entry, expected_inbox);
    assert_eq!(emitted[0].body, TEST_BODY.to_vec());
    assert_eq!(emitted[0].mime_type, "text/plain");
    // B added itself to the grow-only ig set; entry retained (coverage
    // incomplete — A has not ingested yet).
    {
        let doc = b.doc.lock().await;
        let entry = doc.entries.get(&key).expect("entry retained on B");
        assert!(entry.ingested_by.contains(&b_id));
        assert!(!entry.ingested_by.contains(&a_id));
    }

    // ----- (4) A observes B's ig ack (B's sweep flushed → bridge → A) ----
    let a_doc_handle = Arc::clone(&a.doc);
    let observed = wait_until(
        || {
            let a_doc = Arc::clone(&a_doc_handle);
            let key = key.clone();
            let b_id = b_id.clone();
            async move {
                a_doc
                    .lock()
                    .await
                    .entries
                    .get(&key)
                    .is_some_and(|e| e.ingested_by.contains(&b_id))
            }
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(observed, "A did not observe B's ig ack within 5s");

    // ----- (5) A ingests too, then GC fires when coverage completes ------
    let a_ingest_ctx = ProbeIngestCtx::new(
        a_id.clone(),
        [(fx.dm_device_hash, fx.identity_pub)].into(),
        enrolled.clone(),
    );
    let changed = sweep(&a.doc, &a_ingest_ctx, &a.engine).await;
    assert!(changed, "A's ingestion must mutate the doc");
    assert_eq!(
        a_ingest_ctx.applied(),
        vec![expected_inbox.clone()],
        "A (also a recipient device) ingests through the same path"
    );
    assert_eq!(a_ingest_ctx.emitted().len(), 1);

    // Publish-before-GC: the covering ig = {A, B} state is retained for
    // one sweep (coverage-GC removes only entries already covered at sweep
    // START) so it replicates before any replica destroys it.
    {
        let doc = a.doc.lock().await;
        let entry = doc
            .entries
            .get(&key)
            .expect("covered entry retained on A for one sweep so the ig state replicates");
        assert_eq!(entry.ingested_by, enrolled, "ig covers the enrolled set");
    }

    // B observes the full-coverage state via the bridge...
    let b_doc_handle = Arc::clone(&b.doc);
    let b_saw_coverage = wait_until(
        || {
            let b_doc = Arc::clone(&b_doc_handle);
            let key = key.clone();
            let a_id = a_id.clone();
            async move {
                b_doc
                    .lock()
                    .await
                    .entries
                    .get(&key)
                    .is_some_and(|e| e.ingested_by.contains(&a_id))
            }
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(b_saw_coverage, "B did not observe A's ig ack within 5s");

    // ...and B's next sweep coverage-GCs (no re-ingestion: B is in ig).
    let changed = sweep(&b.doc, &b_ingest_ctx, &b.engine).await;
    assert!(changed, "B's GC removal is a doc mutation");
    assert!(
        b.doc.lock().await.entries.is_empty(),
        "GC must remove the entry on B once coverage is complete"
    );
    assert_eq!(
        b_ingest_ctx.applied().len(),
        1,
        "no duplicate ingestion on B during the GC sweep"
    );

    // A's next sweep coverage-GCs as well — the fleet converges to empty.
    let changed = sweep(&a.doc, &a_ingest_ctx, &a.engine).await;
    assert!(changed, "A's GC removal is a doc mutation");
    assert!(
        a.doc.lock().await.entries.is_empty(),
        "GC must remove the entry on A once coverage is complete"
    );
    assert_eq!(
        a_ingest_ctx.applied().len(),
        1,
        "no duplicate ingestion on A during the GC sweep"
    );
    assert_eq!(a_ingest_ctx.emitted().len(), 1, "no duplicate emit on A");

    let _ = a.engine.shutdown().await;
    let _ = b.engine.shutdown().await;
    forwarder.abort();
}

/// ZEB-424: a deposit from a sender who is NOT an Active friend but DOES
/// share a live group-DM with the recipient is admitted through the
/// co-member branch and runs the receive path through ingestion — admitted,
/// persisted (persist-before-ack), acked, fanned out to B, and ingested by B.
/// This mirrors `butler_deposit_fans_out_ingests_acks_and_gcs` *through B's
/// ingestion* (its phases 1–3), with exactly two deltas before the deposit:
/// the sender is REMOVED from `friends` (so the friend branch misses) and
/// ADDED to `group_co_members` (so the co-member branch admits).
///
/// It intentionally stops at ingestion and does NOT re-assert that test's
/// phases 4–5 (ig-ack propagation back to A + coverage-GC convergence): those
/// run on the shared post-admission machinery, which is byte-for-byte
/// identical regardless of admission flavor — admission is the ONLY divergence
/// and is complete by step 1, long before persist. The ig-ack + GC tail is
/// fully covered by the friend gold-standard test; duplicating its ~90 lines
/// here would add no distinct-code-path coverage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_dm_co_member_non_friend_deposit_is_accepted_and_ingested() {
    let fx = fixture();
    let a_sk = device_a_sk();
    let b_sk = device_b_sk();
    let a_id = device_id(&a_sk);
    let b_id = device_id(&b_sk);
    let enrolled: BTreeSet<String> = [a_id.clone(), b_id.clone()].into();
    let key = DmInboxDoc::key(&fx.space_id.0, &fx.message_cid.to_bytes());

    let kt = Arc::new(KeyTree::derive(&[0x42u8; 32]).expect("kt"));
    let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());

    let chronology: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let a_dir = tempfile::tempdir().expect("tempdir A");
    let b_dir = tempfile::tempdir().expect("tempdir B");
    let mut a = build_engine(
        &a_id,
        Arc::clone(&kt),
        Arc::clone(&cas),
        Arc::new(RecordingPersist {
            inner: DmInboxPersist {
                doc_path: a_dir.path().join("dm_inbox.cbor"),
                replay_path: a_dir.path().join("dm_inbox_replay.cbor"),
            },
            expected_key: key.clone(),
            chronology: Arc::clone(&chronology),
        }),
    );
    let mut b = build_engine(
        &b_id,
        Arc::clone(&kt),
        Arc::clone(&cas),
        Arc::new(DmInboxPersist {
            doc_path: b_dir.path().join("dm_inbox.cbor"),
            replay_path: b_dir.path().join("dm_inbox_replay.cbor"),
        }),
    );

    let a_in = a.in_tx.clone();
    let b_in = b.in_tx.clone();
    let mut a_out = std::mem::replace(&mut a.out_rx, mpsc::channel(1).1);
    let mut b_out = std::mem::replace(&mut b.out_rx, mpsc::channel(1).1);
    let forwarder = tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(frame) = a_out.recv() => { let _ = b_in.send(frame).await; }
                Some(frame) = b_out.recv() => { let _ = a_in.send(frame).await; }
                else => break,
            }
        }
    });

    // ----- (1) Sender builds the real sealed frame; A accepts it ---------
    let cert_bytes = harmony_owner::cbor::to_canonical(&fx.sender.cert).expect("encode cert");
    let frame = build_deposit_frame(
        &RECIPIENT_OWNER,
        &fx.sender.owner.0,
        &cert_bytes,
        &fx.sender.device_key,
        &a_sk.verifying_key().to_bytes(),
        &DepositPayload {
            cidnotify_packet: Some(fx.cidnotify_packet.clone()),
            storage_blob: fx.storage_blob.clone(),
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
        },
    )
    .expect("sender-side frame build");

    // The ONLY deltas vs the friend happy-path: the sender is NOT a friend
    // (empty `friends`), but IS a live group-DM co-member (the sender owner
    // is in `group_co_members`). Admission must take the co-member branch;
    // its cert-anchor check is derived from the owner id, which the minted
    // sender cert satisfies automatically.
    let a_butler_ctx = TestButlerCtx {
        self_owner: RECIPIENT_OWNER,
        device_id: a_id.clone(),
        friends: BTreeMap::new(),
        group_co_members: [fx.sender.owner.0].into(),
        // D28.1: the deposit's own space is a live GroupDm with both members.
        group_co_member_spaces: [(fx.space_id.0, fx.sender.owner.0)].into(),
        device_owners: [(fx.dm_device_hash, (fx.sender.owner.0, fx.identity_pub))].into(),
        butler_sk: a_sk,
        doc: Arc::clone(&a.doc),
        tracker: Arc::clone(&a.tracker),
        engine: Arc::clone(&a.engine),
    };

    let ack = handle_deposit_core(&frame, &a_butler_ctx)
        .await
        .expect("valid deposit from a live group-DM co-member must be accepted");
    chronology.lock().unwrap().push("ack".to_string());

    assert_eq!(ack.space_id, fx.space_id.0);
    assert_eq!(ack.message_cid, fx.message_cid.to_bytes().to_vec());

    // Persist-before-ack (D7): same chronology guarantee as the friend path.
    {
        let log = chronology.lock().unwrap().clone();
        let first_persist_with_entry = log.iter().position(|e| e == "persist:with-entry");
        let ack_pos = log.iter().position(|e| e == "ack").expect("ack stamped");
        assert!(
            first_persist_with_entry.is_some_and(|p| p < ack_pos),
            "entry must be durably persisted on A BEFORE the ack exists; chronology: {log:?}"
        );
    }

    // A's doc holds the entry at ack time, exactly as deposited.
    {
        let doc = a.doc.lock().await;
        let entry = doc.entries.get(&key).expect("entry in A's doc at ack");
        assert_eq!(entry.sender_owner, fx.sender.owner.0);
        assert_eq!(entry.cidnotify_packet, Some(fx.cidnotify_packet.clone()));
        assert_eq!(entry.storage_blob, fx.storage_blob);
        assert_eq!(entry.deposited_by, a_id);
        assert!(entry.ingested_by.is_empty());
    }

    // ----- (2) Fan-out: A's persist_entry flush → bridge → B applies -----
    let b_doc_handle = Arc::clone(&b.doc);
    let converged = wait_until(
        || {
            let b_doc = Arc::clone(&b_doc_handle);
            let key = key.clone();
            async move { b_doc.lock().await.entries.contains_key(&key) }
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(
        converged,
        "B did not receive the co-member entry via fan-out within 5s"
    );
    let deposited_at = {
        let doc = b.doc.lock().await;
        let entry = &doc.entries[&key];
        assert_eq!(entry.sender_owner, fx.sender.owner.0);
        assert_eq!(entry.cidnotify_packet, Some(fx.cidnotify_packet));
        assert_eq!(entry.storage_blob, fx.storage_blob);
        assert_eq!(entry.deposited_by, a_id);
        entry.deposited_at.clone()
    };

    // ----- (3) B's ingestion sweep: the normal DM receive path ----------
    let b_ingest_ctx = ProbeIngestCtx::new(
        b_id.clone(),
        [(fx.dm_device_hash, fx.identity_pub)].into(),
        enrolled.clone(),
    );
    let changed = sweep(&b.doc, &b_ingest_ctx, &b.engine).await;
    assert!(changed, "B's ingestion must mutate the doc (ig growth)");

    assert_eq!(
        *b_ingest_ctx.cas_blobs.lock().unwrap(),
        vec![fx.storage_blob.clone()],
        "B must CAS-put the deposited storage blob"
    );
    let expected_inbox = InboxEntry {
        space_id: fx.space_id,
        message_cid: fx.message_cid,
        from: OwnerAddr(fx.sender.owner.0),
        received_at: deposited_at,
    };
    assert_eq!(b_ingest_ctx.applied(), vec![expected_inbox.clone()]);
    let emitted = b_ingest_ctx.emitted();
    assert_eq!(emitted.len(), 1, "exactly one dm-received emit on B");
    assert_eq!(emitted[0].inbox_entry, expected_inbox);
    assert_eq!(emitted[0].body, TEST_BODY.to_vec());
    assert_eq!(emitted[0].mime_type, "text/plain");
    {
        let doc = b.doc.lock().await;
        let entry = doc.entries.get(&key).expect("entry retained on B");
        assert!(entry.ingested_by.contains(&b_id));
        assert!(!entry.ingested_by.contains(&a_id));
    }

    let _ = a.engine.shutdown().await;
    let _ = b.engine.shutdown().await;
    forwarder.abort();
}

/// ZEB-424: a deposit from a sender who is NEITHER an Active friend NOR a
/// live group-DM co-member is rejected with `NotAuthorized`, and nothing is
/// persisted into A's dm-inbox store. Same construction as the happy-path,
/// but BOTH `friends` and `group_co_members` are empty for this sender, so
/// both admission branches miss at step 1 — before any decrypt/persist.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_member_non_friend_deposit_is_rejected_and_not_persisted() {
    let fx = fixture();
    let a_sk = device_a_sk();
    let a_id = device_id(&a_sk);
    let key = DmInboxDoc::key(&fx.space_id.0, &fx.message_cid.to_bytes());

    let kt = Arc::new(KeyTree::derive(&[0x42u8; 32]).expect("kt"));
    let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());

    let chronology: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let a_dir = tempfile::tempdir().expect("tempdir A");
    let a = build_engine(
        &a_id,
        Arc::clone(&kt),
        Arc::clone(&cas),
        Arc::new(RecordingPersist {
            inner: DmInboxPersist {
                doc_path: a_dir.path().join("dm_inbox.cbor"),
                replay_path: a_dir.path().join("dm_inbox_replay.cbor"),
            },
            expected_key: key.clone(),
            chronology: Arc::clone(&chronology),
        }),
    );

    let cert_bytes = harmony_owner::cbor::to_canonical(&fx.sender.cert).expect("encode cert");
    let frame = build_deposit_frame(
        &RECIPIENT_OWNER,
        &fx.sender.owner.0,
        &cert_bytes,
        &fx.sender.device_key,
        &a_sk.verifying_key().to_bytes(),
        &DepositPayload {
            cidnotify_packet: Some(fx.cidnotify_packet.clone()),
            storage_blob: fx.storage_blob.clone(),
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
        },
    )
    .expect("sender-side frame build");

    // BOTH admission sources empty for this sender: not a friend, not a
    // co-member → step 1 returns NotAuthorized before any persist.
    let a_butler_ctx = TestButlerCtx {
        self_owner: RECIPIENT_OWNER,
        device_id: a_id.clone(),
        friends: BTreeMap::new(),
        group_co_members: BTreeSet::new(),
        group_co_member_spaces: BTreeSet::new(),
        device_owners: [(fx.dm_device_hash, (fx.sender.owner.0, fx.identity_pub))].into(),
        butler_sk: a_sk,
        doc: Arc::clone(&a.doc),
        tracker: Arc::clone(&a.tracker),
        engine: Arc::clone(&a.engine),
    };

    let err = handle_deposit_core(&frame, &a_butler_ctx)
        .await
        .expect_err("deposit from a non-friend non-co-member must be rejected");
    assert_eq!(err, DepositReject::NotAuthorized);

    // Nothing was persisted: A's dm-inbox doc is untouched (no entry under
    // the deposit key, and the store is empty overall).
    {
        let doc = a.doc.lock().await;
        assert!(
            !doc.entries.contains_key(&key),
            "rejected deposit must not persist its entry"
        );
        assert!(
            doc.entries.is_empty(),
            "A's dm-inbox store must be empty after a rejected deposit"
        );
    }
    // The persist sink was never invoked with the entry (no persist stamp).
    assert!(
        chronology.lock().unwrap().is_empty(),
        "no persist should occur for a rejected deposit"
    );

    let _ = a.engine.shutdown().await;
}

/// ZEB-424 D28.1 security follow-up: a sender who shares SOME live group DM
/// with the recipient (so step-1 admission passes) but deposits a packet whose
/// inner `space_id` names a space they are NOT a live member of is rejected
/// `NotAuthorizedForScope` (ZEB-702 split: an ADMITTED sender failing the
/// space bind — deliberately not the roster-miss signal) and nothing is
/// persisted. This is the integration-level
/// proof that co-member admission is bound to the DEPOSITED space, not merely
/// to "shares any group" — closing the un-ingestible-entry inbox-slot-pinning
/// vector. Same construction as the co-member happy-path, with one delta:
/// `group_co_member_spaces` is EMPTY, so the post-decrypt bind rejects the
/// fixture's own `space_id`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn co_member_deposit_for_unrelated_space_is_rejected_and_not_persisted() {
    let fx = fixture();
    let a_sk = device_a_sk();
    let a_id = device_id(&a_sk);
    let key = DmInboxDoc::key(&fx.space_id.0, &fx.message_cid.to_bytes());

    let kt = Arc::new(KeyTree::derive(&[0x42u8; 32]).expect("kt"));
    let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());

    let chronology: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let a_dir = tempfile::tempdir().expect("tempdir A");
    let a = build_engine(
        &a_id,
        Arc::clone(&kt),
        Arc::clone(&cas),
        Arc::new(RecordingPersist {
            inner: DmInboxPersist {
                doc_path: a_dir.path().join("dm_inbox.cbor"),
                replay_path: a_dir.path().join("dm_inbox_replay.cbor"),
            },
            expected_key: key.clone(),
            chronology: Arc::clone(&chronology),
        }),
    );

    let cert_bytes = harmony_owner::cbor::to_canonical(&fx.sender.cert).expect("encode cert");
    let frame = build_deposit_frame(
        &RECIPIENT_OWNER,
        &fx.sender.owner.0,
        &cert_bytes,
        &fx.sender.device_key,
        &a_sk.verifying_key().to_bytes(),
        &DepositPayload {
            cidnotify_packet: Some(fx.cidnotify_packet.clone()),
            storage_blob: fx.storage_blob.clone(),
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
        },
    )
    .expect("sender-side frame build");

    // The sender shares SOME live group DM (step 1 admits via the co-member
    // branch), but `group_co_member_spaces` is EMPTY, so the deposit's own
    // space (fx.space_id) is NOT a live GroupDm with both members → the
    // post-decrypt bind (step 5.5) rejects before any persist/ack.
    let a_butler_ctx = TestButlerCtx {
        self_owner: RECIPIENT_OWNER,
        device_id: a_id.clone(),
        friends: BTreeMap::new(),
        group_co_members: [fx.sender.owner.0].into(),
        group_co_member_spaces: BTreeSet::new(),
        device_owners: [(fx.dm_device_hash, (fx.sender.owner.0, fx.identity_pub))].into(),
        butler_sk: a_sk,
        doc: Arc::clone(&a.doc),
        tracker: Arc::clone(&a.tracker),
        engine: Arc::clone(&a.engine),
    };

    let err = handle_deposit_core(&frame, &a_butler_ctx)
        .await
        .expect_err("co-member depositing into a non-member space must be rejected");
    assert_eq!(err, DepositReject::NotAuthorizedForScope);

    // Nothing was persisted, even though step-1 admission passed: the reject
    // landed at the post-decrypt space bind, before persist/ack.
    {
        let doc = a.doc.lock().await;
        assert!(
            !doc.entries.contains_key(&key),
            "a space-bind reject must not persist its entry"
        );
        assert!(
            doc.entries.is_empty(),
            "A's dm-inbox store must be empty after a space-bind reject"
        );
    }
    assert!(
        chronology.lock().unwrap().is_empty(),
        "no persist should occur for a space-bind reject"
    );

    let _ = a.engine.shutdown().await;
}
