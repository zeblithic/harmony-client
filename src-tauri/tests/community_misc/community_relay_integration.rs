//! ZEB-458 Phase A Task 6: community relay E2E integration test.
//!
//! Two test ctx types back the relay + recipient pipelines with real
//! `FleetSyncEngine<RelayHoldDoc>` instances and real crypto. No Tauri, no
//! `start_node`, no iroh shells — exactly the `butler_deposit_integration`
//! harness pattern applied to the relay modules.
//!
//! Scenarios:
//! 1. **Happy path** — full round-trip: deposit → pull → open_and_ingest →
//!    pull_ack → mark_pulled → gc empty.
//! 2. **Non-member sender** — deposit rejects with `NotCoMember`; hold doc
//!    untouched.
//! 3. **Wrong-owner pull** — pull query with a cert for a different owner
//!    rejects with `BadCert`.
//! 4. **Restart durability** — drop + recreate engine from persisted state;
//!    blob still present and pullable.
//! 5. **Opacity (structural)** — `RelayDepositCtx` has no decrypt hook;
//!    relay ctx event log never records "decrypt".

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use async_trait::async_trait;
use ed25519_dalek::Signer;
use harmony_app::butler_deposit::DepositPayload;
use harmony_app::community_membership::{mint_test_owner, TestOwner};
use harmony_app::community_relay::{
    build_relay_deposit_frame, relay_pull_ack_sig_payload, relay_pull_sig_payload, RelayHeldBlob,
    RelayPullAck, RelayPullQuery, COMMUNITY_RELAY_SEAL_INFO, RELAY_HOLD_GLOBAL_CAP,
    RELAY_HOLD_PER_SENDER_CAP,
};
use harmony_app::community_relay_hold_crdt::{RelayHoldDoc, RelayHoldEntry};
use harmony_app::community_relay_optin::RelayOptInDoc;
use harmony_app::community_relay_prod::{
    CommunityMembershipLookup, EngineRelayHoldFlush, ProdRelayDepositCtx, ProdRelayPullCtx,
};
use harmony_app::community_relay_pull::{open_and_ingest, RelayIngestCtx};
use harmony_app::dm_signing::{ed25519_priv_to_x25519, open_from_owner_with_info};
use harmony_app::fleet_sync::{
    mint_next_hlc, FleetPersist, FleetSyncConfig, FleetSyncEngine, Merger, SyncError,
};
use harmony_app::iroh_community_relay_acceptor::{
    handle_relay_deposit_core, handle_relay_pull_ack, handle_relay_pull_query, RelayDepositCtx,
    RelayDepositReject, RelayPersistVerdict, RelayPullCtx, RelayPullReject,
};
use harmony_app::owner_state_crypto::KeyTree;
use harmony_app::owner_state_types::{Hlc, SpaceId};
use harmony_content::cid::{ContentFlags, ContentId};
use tokio::sync::{mpsc, Mutex};
use zeroize::Zeroizing;

// =====================================================================
// Deterministic test identities and community setup
// =====================================================================

/// Relay device — accepts deposits and serves pull queries.
fn relay_owner() -> TestOwner {
    mint_test_owner(0x01)
}

/// Sender — deposits sealed blobs.
fn sender_owner() -> TestOwner {
    mint_test_owner(0x10)
}

/// Recipient — pulls + opens blobs.
fn recipient_owner() -> TestOwner {
    mint_test_owner(0x20)
}

/// The community all three parties share.
fn community_id() -> SpaceId {
    SpaceId([0xCC; 16])
}

// =====================================================================
// Relay-hold on-disk persist (mirrors DmInboxPersist exactly, just for
// RelayHoldDoc — used by the restart-durability scenario).
// =====================================================================

/// Trivial CBOR persist for `RelayHoldDoc` (one-byte version prefix, same
/// pattern as `DmInboxPersist` and `NotesPersist`). Used ONLY in the durability
/// scenario; the other scenarios use `NoopRelayPersist` to avoid FS I/O.
struct RelayHoldPersist {
    doc_path: PathBuf,
    replay_path: PathBuf,
}

impl FleetPersist<RelayHoldDoc> for RelayHoldPersist {
    fn persist(
        &self,
        state: &RelayHoldDoc,
        tracker: &BTreeMap<String, Hlc>,
    ) -> Result<(), SyncError> {
        // Write doc
        if let Some(p) = self.doc_path.parent() {
            std::fs::create_dir_all(p)
                .map_err(|e| SyncError::Persist(format!("create_dir: {e}")))?;
        }
        let mut doc_bytes = vec![1u8]; // schema v1
        ciborium::into_writer(state, &mut doc_bytes)
            .map_err(|e| SyncError::CborEncode(format!("encode relay_hold: {e}")))?;
        std::fs::write(&self.doc_path, &doc_bytes)
            .map_err(|e| SyncError::Persist(format!("write relay_hold: {e}")))?;

        // Write replay tracker
        let mut rep_bytes = vec![1u8];
        ciborium::into_writer(tracker, &mut rep_bytes)
            .map_err(|e| SyncError::CborEncode(format!("encode replay: {e}")))?;
        std::fs::write(&self.replay_path, &rep_bytes)
            .map_err(|e| SyncError::Persist(format!("write replay: {e}")))?;
        Ok(())
    }
}

/// Load a `RelayHoldDoc` from the persisted file (version-prefixed CBOR).
/// Returns `RelayHoldDoc::default()` if the file doesn't exist.
fn load_relay_hold_doc(path: &PathBuf) -> RelayHoldDoc {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return RelayHoldDoc::default(),
        Err(e) => panic!("read relay_hold doc: {e}"),
    };
    // Validate the schema-version prefix BEFORE decoding, so this durability
    // test fails at the version boundary it is meant to exercise rather than
    // silently decoding an incompatible future format.
    let (&version, payload) = bytes.split_first().expect("relay_hold doc file is empty");
    assert_eq!(
        version, 1u8,
        "unexpected relay_hold schema version byte: {version}"
    );
    let mut cursor = std::io::Cursor::new(payload);
    let doc: RelayHoldDoc =
        ciborium::from_reader(&mut cursor).expect("decode relay_hold doc from disk");
    // Reject trailing bytes — a partial/garbage tail must fail loudly, not be
    // silently ignored.
    assert_eq!(
        cursor.position() as usize,
        payload.len(),
        "trailing bytes after relay_hold doc CBOR ({} of {} consumed)",
        cursor.position(),
        payload.len()
    );
    doc
}

// =====================================================================
// No-op persist (all other scenarios)
// =====================================================================

struct NoopRelayPersist;

impl FleetPersist<RelayHoldDoc> for NoopRelayPersist {
    fn persist(
        &self,
        _state: &RelayHoldDoc,
        _tracker: &BTreeMap<String, Hlc>,
    ) -> Result<(), SyncError> {
        Ok(())
    }
}

// =====================================================================
// Engine builder (mirrors butler_deposit_integration::build_engine)
// =====================================================================

struct Built {
    engine: Arc<FleetSyncEngine<RelayHoldDoc>>,
    doc: Arc<Mutex<RelayHoldDoc>>,
    tracker: Arc<Mutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>>,
    /// Keep the outbound receiver alive so the `publisher_tx` channel
    /// remains open — dropping it would close the publish channel and cause
    /// `flush_now` to return `TransportClosed`.
    _out_rx: mpsc::Receiver<Vec<u8>>,
}

fn build_relay_engine(device_id: &str, persist: Arc<dyn FleetPersist<RelayHoldDoc>>) -> Built {
    let (out_tx, out_rx) = mpsc::channel(64);
    let (_in_tx, in_rx) = mpsc::channel(64);
    let doc = Arc::new(Mutex::new(RelayHoldDoc::default()));
    let tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        device_id.to_string(),
    )));
    let kt = Arc::new(KeyTree::derive(&[0xAA; 32]).expect("kt"));
    let merger: Merger<RelayHoldDoc> = Arc::new(|l: &mut RelayHoldDoc, r| l.merge_from(r));
    let engine = Arc::new(FleetSyncEngine::new(FleetSyncConfig {
        keys: harmony_app::owner_state_crypto::FleetKeySet::new(kt),
        device_id: device_id.to_string(),
        state: Arc::clone(&doc),
        merger,
        replay_tracker: Arc::clone(&tracker),
        content_store: Arc::new(harmony_app::content_store::InMemoryStub::default()),
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        persist,
        lookup_key_tag: b"relay-hold-v1",
        debounce_ms: 50,
        publish_seen: false,
        on_applied: None,
        sibling_acks: Arc::new(Mutex::new(harmony_crdt_sync::MonotoneMap::new())),
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
    }));
    Built {
        engine,
        doc,
        tracker,
        _out_rx: out_rx,
    }
}

/// Build from a pre-loaded doc (durability scenario: inject previously
/// persisted state without replaying the full engine).
fn build_relay_engine_from_doc(
    device_id: &str,
    persisted_doc: RelayHoldDoc,
    persist: Arc<dyn FleetPersist<RelayHoldDoc>>,
) -> Built {
    let (out_tx, out_rx) = mpsc::channel(64);
    let (_in_tx, in_rx) = mpsc::channel(64);
    let doc = Arc::new(Mutex::new(persisted_doc));
    let tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        device_id.to_string(),
    )));
    let kt = Arc::new(KeyTree::derive(&[0xAA; 32]).expect("kt"));
    let merger: Merger<RelayHoldDoc> = Arc::new(|l: &mut RelayHoldDoc, r| l.merge_from(r));
    let engine = Arc::new(FleetSyncEngine::new(FleetSyncConfig {
        keys: harmony_app::owner_state_crypto::FleetKeySet::new(kt),
        device_id: device_id.to_string(),
        state: Arc::clone(&doc),
        merger,
        replay_tracker: Arc::clone(&tracker),
        content_store: Arc::new(harmony_app::content_store::InMemoryStub::default()),
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        persist,
        lookup_key_tag: b"relay-hold-v1",
        debounce_ms: 50,
        publish_seen: false,
        on_applied: None,
        sibling_acks: Arc::new(Mutex::new(harmony_crdt_sync::MonotoneMap::new())),
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
    }));
    Built {
        engine,
        doc,
        tracker,
        _out_rx: out_rx,
    }
}

// =====================================================================
// Relay test ctx: implements BOTH RelayDepositCtx and RelayPullCtx,
// backed by a real FleetSyncEngine<RelayHoldDoc>.
//
// `serves_community` / `both_co_members` / `is_joined_member` read from
// injected BTreeSets.  `persist_hold` does the atomic caps-enforcing
// persist under the doc lock + flush_now(), mirroring ProdButlerDepositCtx.
// `held_for` scans the doc under lock.
// `mark_pulled` unions pulled_by under the doc lock + flush_now().
// =====================================================================

struct TestRelayCtx {
    /// The relay's device id (64-hex of its device ed25519 vk).
    relay_device_id: String,
    /// Set of community_ids this relay serves.
    served_communities: BTreeSet<SpaceId>,
    /// (community_id, sender_owner, recipient_owner) triples that pass
    /// `both_co_members`.
    co_members: BTreeSet<([u8; 16], [u8; 16], [u8; 16])>,
    /// (community_id, owner) pairs that pass `is_joined_member`.
    members: BTreeSet<([u8; 16], [u8; 16])>,
    /// Wall-clock now in epoch-seconds (for cert expiry).
    now_secs: u64,
    /// Ordered event log for opacity / call-order assertions.
    events: StdMutex<Vec<String>>,
    /// The relay's hold doc (under engine lock).
    doc: Arc<Mutex<RelayHoldDoc>>,
    /// HLC tracker.
    tracker: Arc<Mutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>>,
    /// Engine handle for notify_dirty + flush_now.
    engine: Arc<FleetSyncEngine<RelayHoldDoc>>,
}

impl TestRelayCtx {
    fn push_event(&self, e: impl Into<String>) {
        self.events.lock().unwrap().push(e.into());
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait]
impl RelayDepositCtx for TestRelayCtx {
    fn relay_device_id(&self) -> String {
        self.relay_device_id.clone()
    }

    async fn serves_community(&self, community_id: &SpaceId) -> bool {
        self.push_event("serves_community");
        self.served_communities.contains(community_id)
    }

    async fn both_co_members(
        &self,
        community_id: &SpaceId,
        sender_owner: &[u8; 16],
        recipient_owner: &[u8; 16],
    ) -> bool {
        self.push_event("both_co_members");
        self.co_members
            .contains(&(community_id.0, *sender_owner, *recipient_owner))
    }

    fn now_secs(&self) -> u64 {
        self.now_secs
    }

    async fn mint_hlc(&self) -> Hlc {
        mint_next_hlc(
            &self.tracker,
            &harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
            &self.relay_device_id,
        )
        .await
    }

    /// Atomic persist-with-caps under the doc lock (mirrors
    /// `TestButlerCtx::persist_entry` exactly, adapted to relay types).
    async fn persist_hold(
        &self,
        key: String,
        entry: RelayHoldEntry,
    ) -> Result<RelayPersistVerdict, String> {
        self.push_event(format!("persist:{}", &key[..16])); // abbreviated key for assertions
        let verdict = {
            let mut doc = self.doc.lock().await;
            if doc.entries.contains_key(&key) {
                RelayPersistVerdict::Duplicate
            } else {
                let sender_count = doc.count_for_sender(&entry.community_id, &entry.sender_owner);
                if sender_count >= RELAY_HOLD_PER_SENDER_CAP
                    || doc.live_count() >= RELAY_HOLD_GLOBAL_CAP
                {
                    return Ok(RelayPersistVerdict::CapExceeded);
                }
                doc.entries.insert(key, entry);
                RelayPersistVerdict::Inserted
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

#[async_trait]
impl RelayPullCtx for TestRelayCtx {
    async fn serves_community(&self, community_id: &SpaceId) -> bool {
        self.push_event("serves_community");
        self.served_communities.contains(community_id)
    }

    async fn is_joined_member(&self, community_id: &SpaceId, owner: &[u8; 16]) -> bool {
        self.push_event("is_joined_member");
        self.members.contains(&(community_id.0, *owner))
    }

    fn now_secs(&self) -> u64 {
        self.now_secs
    }

    async fn held_for(
        &self,
        recipient_owner: &[u8; 16],
        requester_device: &str,
    ) -> Vec<(String, RelayHeldBlob)> {
        self.push_event("held_for");
        let doc = self.doc.lock().await;
        doc.entries
            .iter()
            .filter(|(_, e)| {
                &e.recipient_owner == recipient_owner && !e.pulled_by.contains(requester_device)
            })
            .map(|(k, e)| {
                (
                    k.clone(),
                    RelayHeldBlob {
                        sender_owner: e.sender_owner,
                        sealed_blob: e.sealed_blob.clone(),
                    },
                )
            })
            .collect()
    }

    /// Union pulled_by for the given keys under the doc lock, then flush.
    /// Missing keys are silently ignored (no-op, as per trait contract).
    async fn mark_pulled(&self, keys: &[String], requester_device: String) -> Result<(), String> {
        self.push_event(format!("mark_pulled:{}", keys.len()));
        {
            let mut doc = self.doc.lock().await;
            for k in keys {
                if let Some(e) = doc.entries.get_mut(k) {
                    e.pulled_by.insert(requester_device.clone());
                }
            }
        }
        self.engine.notify_dirty();
        self.engine
            .flush_now()
            .await
            .map_err(|e| format!("flush_now mark_pulled: {e}"))?;
        Ok(())
    }
}

// =====================================================================
// Recipient ingest ctx: implements RelayIngestCtx backed by the device's
// X25519 private keys.  `ingest_recovered` records the DepositPayload.
// (In a full production ctx this would call verify_cidnotify_admission,
// decrypt_and_bind_dm_blob, apply_inbox, emit_dm_received — Phase B wires
// those. Here we verify the open-and-ingest seam by recording the payload.)
// =====================================================================

struct TestRecipientIngestCtx {
    /// X25519 privs for all the recipient's enrolled devices.
    device_privs: Vec<Zeroizing<[u8; 32]>>,
    /// Recorded payloads from successful ingest calls.
    ingested: StdMutex<Vec<DepositPayload>>,
}

impl TestRecipientIngestCtx {
    fn new(device_privs: Vec<Zeroizing<[u8; 32]>>) -> Self {
        Self {
            device_privs,
            ingested: StdMutex::new(Vec::new()),
        }
    }

    fn ingested(&self) -> Vec<DepositPayload> {
        self.ingested.lock().unwrap().clone()
    }
}

#[async_trait]
impl RelayIngestCtx for TestRecipientIngestCtx {
    fn device_x25519_privs(&self) -> Vec<Zeroizing<[u8; 32]>> {
        self.device_privs.clone()
    }

    async fn ingest_recovered(
        &self,
        _sender_owner: [u8; 16],
        payload: DepositPayload,
    ) -> Result<(), String> {
        self.ingested.lock().unwrap().push(payload);
        Ok(())
    }
}

// =====================================================================
// Helper: bounded condition-polling (mirrors butler_deposit_integration)
// =====================================================================

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

// =====================================================================
// Fixture builder: build a valid RelayDepositFrame sealed to the recipient
// =====================================================================

struct RelayFixture {
    frame: harmony_app::community_relay::RelayDepositFrame,
    payload: DepositPayload,
    recipient: TestOwner,
    sender: TestOwner,
    /// `ContentId::for_book(&frame.sealed_blob, encrypted=true).to_bytes()`
    sealed_content_id: [u8; 32],
}

fn build_fixture() -> RelayFixture {
    let sender = sender_owner();
    let recipient = recipient_owner();
    let cid = community_id();
    let payload = DepositPayload {
        cidnotify_packet: Some(vec![0x01, 0x02, 0x03, 0x04]),
        storage_blob: vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE],
        invite_packet: None,
        revocation_push: None,
        grant_push: None,
        grant_revoke: None,
    };
    let cert_bytes = harmony_owner::cbor::to_canonical(&sender.cert).expect("encode cert");
    let frame = build_relay_deposit_frame(
        recipient.owner.0,
        &recipient.cert.device_pubkeys.classical.ed25519_verify,
        sender.owner.0,
        cid,
        cert_bytes,
        &sender.device_key,
        &payload,
    )
    .expect("build_relay_deposit_frame");
    let sealed_content_id = ContentId::for_book(
        &frame.sealed_blob,
        ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .expect("content_id for sealed blob")
    .to_bytes();
    RelayFixture {
        frame,
        payload,
        recipient,
        sender,
        sealed_content_id,
    }
}

/// Build the `TestRelayCtx` wired for the happy-path (relay serves the
/// community; sender and recipient are both co-members; all three are
/// `is_joined_member` for pull).
fn build_relay_ctx(built: &Built, relay_device_id: String) -> TestRelayCtx {
    let sender = sender_owner();
    let recipient = recipient_owner();
    let relay = relay_owner();
    let cid = community_id();

    let mut served = BTreeSet::new();
    served.insert(cid);

    let mut co_members = BTreeSet::new();
    co_members.insert((cid.0, sender.owner.0, recipient.owner.0));

    let mut members = BTreeSet::new();
    members.insert((cid.0, relay.owner.0));
    members.insert((cid.0, sender.owner.0));
    members.insert((cid.0, recipient.owner.0));

    TestRelayCtx {
        relay_device_id,
        served_communities: served,
        co_members,
        members,
        now_secs: 1_700_000_100,
        events: StdMutex::new(Vec::new()),
        doc: Arc::clone(&built.doc),
        tracker: Arc::clone(&built.tracker),
        engine: Arc::clone(&built.engine),
    }
}

/// Build the relay device id string (64-hex of the relay's device ed25519 vk).
fn relay_device_id() -> String {
    hex::encode(relay_owner().cert.device_pubkeys.classical.ed25519_verify)
}

// =====================================================================
// Test 1: Happy path — full relay round-trip
// =====================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_happy_path_full_round_trip() {
    let fx = build_fixture();
    let relay_dev_id = relay_device_id();

    let built = build_relay_engine(&relay_dev_id, Arc::new(NoopRelayPersist));
    let relay_ctx = build_relay_ctx(&built, relay_dev_id.clone());

    // ----- (1) Sender deposits -----
    let ack = handle_relay_deposit_core(&fx.frame, &relay_ctx)
        .await
        .expect("valid co-member deposit must be accepted");

    // Ack content_id matches ContentId::for_book(sealed_blob).
    assert_eq!(ack.content_id, fx.sealed_content_id);

    // The hold doc has the entry, sealed_blob byte-identical to frame.
    {
        let doc = built.doc.lock().await;
        let key = RelayHoldDoc::key(&fx.recipient.owner.0, &ack.content_id);
        let entry = doc.entries.get(&key).expect("entry in hold doc");
        assert_eq!(
            entry.sealed_blob, fx.frame.sealed_blob,
            "relay holds sealed_blob verbatim (no decrypt)"
        );
        assert_eq!(entry.recipient_owner, fx.recipient.owner.0);
        assert_eq!(entry.sender_owner, fx.sender.owner.0);
        assert_eq!(entry.community_id, community_id());
        assert!(entry.pulled_by.is_empty(), "fresh hold has no pulls");
    }

    // Call-order: serves_community → both_co_members → persist (no "decrypt").
    let ev = relay_ctx.events();
    let sc = ev.iter().position(|e| e == "serves_community").unwrap();
    let bc = ev.iter().position(|e| e == "both_co_members").unwrap();
    let ps = ev
        .iter()
        .position(|e| e.starts_with("persist:"))
        .expect("persist event");
    assert!(sc < bc, "serves_community before both_co_members");
    assert!(bc < ps, "both_co_members before persist");
    assert!(
        !ev.iter().any(|e| e == "decrypt"),
        "relay ctx must NEVER record decrypt"
    );

    // ----- (2) Recipient builds authed pull query -----
    let cert_bytes = harmony_owner::cbor::to_canonical(&fx.recipient.cert).expect("encode cert");
    let pull_sig = fx
        .recipient
        .device_key
        .sign(&relay_pull_sig_payload(
            &fx.recipient.owner.0,
            &community_id(),
        ))
        .to_bytes()
        .to_vec();
    let query = RelayPullQuery {
        signer_certs_cbor: Vec::new(),
        recipient_owner: fx.recipient.owner.0,
        community_id: community_id(),
        requester_enrollment_cert: cert_bytes.clone(),
        sig: pull_sig.clone(),
    };

    let resp = handle_relay_pull_query(&query, &relay_ctx)
        .await
        .expect("authed pull query must succeed");

    assert_eq!(resp.entries.len(), 1, "one held blob for this recipient");
    let held_blob = &resp.entries[0];
    assert_eq!(
        held_blob.sealed_blob, fx.frame.sealed_blob,
        "pull response delivers the verbatim sealed blob"
    );
    assert_eq!(held_blob.sender_owner, fx.sender.owner.0);

    // ----- (3) Recipient opens + ingests via open_and_ingest -----
    let recipient_x25519 = ed25519_priv_to_x25519(&fx.recipient.device_key);
    let ingest_ctx = TestRecipientIngestCtx::new(vec![recipient_x25519]);
    let acked_content_ids = open_and_ingest(&resp.entries, &ingest_ctx).await;

    // One blob acked = the sealed_content_id.
    assert_eq!(acked_content_ids.len(), 1);
    assert_eq!(acked_content_ids[0], fx.sealed_content_id);

    // ingest_recovered received the exact DepositPayload.
    let ingested = ingest_ctx.ingested();
    assert_eq!(ingested.len(), 1);
    assert_eq!(
        ingested[0], fx.payload,
        "recovered payload matches original"
    );

    // Verify: the X25519 priv opens the sealed_blob independently.
    let plaintext = open_from_owner_with_info(
        &ed25519_priv_to_x25519(&fx.recipient.device_key),
        &fx.frame.sealed_blob,
        COMMUNITY_RELAY_SEAL_INFO,
    )
    .expect("recipient device key must open sealed blob");
    assert!(!plaintext.is_empty(), "plaintext is non-empty after open");

    // ----- (4) Recipient sends pull ack -----
    // The ack must be signed over relay_pull_ack_sig_payload (distinct from the
    // query sig — binds the content_id list so the ack is self-authenticating).
    let ack_msg = RelayPullAck {
        content_ids: acked_content_ids.clone(),
    };
    let pull_ack_sig = fx
        .recipient
        .device_key
        .sign(&relay_pull_ack_sig_payload(
            &fx.recipient.owner.0,
            &community_id(),
            &acked_content_ids,
        ))
        .to_bytes()
        .to_vec();
    handle_relay_pull_ack(
        &fx.recipient.owner.0,
        &community_id(),
        &ack_msg,
        &cert_bytes,
        &[],
        &pull_ack_sig,
        &relay_ctx,
    )
    .await
    .expect("pull ack must succeed");

    // mark_pulled was called and pulled_by is set.
    let key = RelayHoldDoc::key(&fx.recipient.owner.0, &fx.sealed_content_id);
    {
        let doc = built.doc.lock().await;
        let entry = doc.entries.get(&key).expect("entry still in doc before gc");
        let recipient_device_id =
            hex::encode(fx.recipient.cert.device_pubkeys.classical.ed25519_verify);
        assert!(
            entry.pulled_by.contains(&recipient_device_id),
            "pulled_by must contain the recipient device id"
        );
    }

    // ----- (5) GC sweep removes the covered entry -----
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let changed = built.doc.lock().await.gc(now_ms);
    assert!(changed, "gc must remove the covered (acked) entry");
    assert!(
        built.doc.lock().await.entries.is_empty(),
        "hold doc must be empty after gc removes the covered entry"
    );

    let _ = built.engine.shutdown().await;
}

// =====================================================================
// Test 2: Non-member sender — deposit rejected, hold doc untouched
// =====================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_non_member_sender_deposit_rejected_doc_empty() {
    let fx = build_fixture();
    let relay_dev_id = relay_device_id();

    let built = build_relay_engine(&relay_dev_id, Arc::new(NoopRelayPersist));

    // Build a relay ctx with an EMPTY co_members set (sender not a co-member).
    let relay_ctx = TestRelayCtx {
        relay_device_id: relay_dev_id.clone(),
        served_communities: [community_id()].into(),
        co_members: BTreeSet::new(), // ← sender NOT admitted
        members: BTreeSet::new(),
        now_secs: 1_700_000_100,
        events: StdMutex::new(Vec::new()),
        doc: Arc::clone(&built.doc),
        tracker: Arc::clone(&built.tracker),
        engine: Arc::clone(&built.engine),
    };

    let err = handle_relay_deposit_core(&fx.frame, &relay_ctx)
        .await
        .expect_err("non-co-member sender must be rejected");

    assert!(
        matches!(err, RelayDepositReject::NotCoMember),
        "expected NotCoMember, got {err:?}"
    );

    // Hold doc untouched.
    {
        let doc = built.doc.lock().await;
        assert!(
            doc.entries.is_empty(),
            "hold doc must be empty after reject"
        );
    }

    // No persist event (rejected before step 4 — before any persist call).
    assert!(
        !relay_ctx.events().iter().any(|e| e.starts_with("persist:")),
        "no persist event on NotCoMember reject"
    );

    let _ = built.engine.shutdown().await;
}

// =====================================================================
// Test 3: Wrong-owner pull — pull query cert for different owner → BadCert
// =====================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_wrong_owner_pull_cert_rejected() {
    let fx = build_fixture();
    let relay_dev_id = relay_device_id();

    let built = build_relay_engine(&relay_dev_id, Arc::new(NoopRelayPersist));
    let relay_ctx = build_relay_ctx(&built, relay_dev_id.clone());

    // Pre-deposit a blob so there is something to pull.
    handle_relay_deposit_core(&fx.frame, &relay_ctx)
        .await
        .expect("deposit");

    // Build a pull query where the cert belongs to a DIFFERENT owner than
    // `recipient_owner` names.
    let other = mint_test_owner(0x77);
    let other_cert_bytes = harmony_owner::cbor::to_canonical(&other.cert).expect("encode cert");
    // Sign over the RECIPIENT's owner bytes (so the sig would pass for other's cert)
    // but cert.owner_id != fx.recipient.owner.0 → BadCert.
    let bad_sig = other
        .device_key
        .sign(&relay_pull_sig_payload(
            &fx.recipient.owner.0,
            &community_id(),
        ))
        .to_bytes()
        .to_vec();
    let bad_query = RelayPullQuery {
        signer_certs_cbor: Vec::new(),
        recipient_owner: fx.recipient.owner.0,
        community_id: community_id(),
        requester_enrollment_cert: other_cert_bytes,
        sig: bad_sig,
    };

    let err = handle_relay_pull_query(&bad_query, &relay_ctx)
        .await
        .expect_err("wrong-owner cert must be rejected");

    assert!(
        matches!(err, RelayPullReject::BadCert),
        "expected BadCert, got {err:?}"
    );

    // held_for must NOT have been called (reject is before blob serving).
    assert!(
        !relay_ctx.events().iter().any(|e| e == "held_for"),
        "held_for must not be called on BadCert"
    );

    // Verify the blob is still in the hold doc (pull was rejected).
    {
        let doc = built.doc.lock().await;
        assert_eq!(doc.entries.len(), 1, "hold doc entry must still be present");
    }

    let _ = built.engine.shutdown().await;
}

// =====================================================================
// Test 4: Restart durability — drop + recreate engine from persisted state
// =====================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_restart_durability_blob_survives_engine_drop() {
    let fx = build_fixture();
    let relay_dev_id = relay_device_id();

    let dir = tempfile::tempdir().expect("tempdir");
    let doc_path = dir.path().join("relay_hold.cbor");
    let replay_path = dir.path().join("relay_hold_replay.cbor");

    // ----- Phase 1: deposit + flush -----
    let built = build_relay_engine(
        &relay_dev_id,
        Arc::new(RelayHoldPersist {
            doc_path: doc_path.clone(),
            replay_path: replay_path.clone(),
        }),
    );
    let relay_ctx = build_relay_ctx(&built, relay_dev_id.clone());

    let ack = handle_relay_deposit_core(&fx.frame, &relay_ctx)
        .await
        .expect("deposit succeeds");

    // Flush is already done inside persist_hold. Wait briefly for background work.
    let key = RelayHoldDoc::key(&fx.recipient.owner.0, &ack.content_id);
    let doc_has_entry = wait_until(
        || {
            let doc = Arc::clone(&built.doc);
            let key = key.clone();
            async move { doc.lock().await.entries.contains_key(&key) }
        },
        Duration::from_secs(3),
    )
    .await;
    assert!(doc_has_entry, "entry must be in the doc after deposit");

    // Confirm file was written.
    assert!(doc_path.exists(), "relay_hold.cbor must exist after flush");

    // ----- Phase 2: drop the engine (simulated restart) -----
    let _ = built.engine.shutdown().await;
    drop(built);
    drop(relay_ctx);

    // ----- Phase 3: reload from disk -----
    let reloaded_doc = load_relay_hold_doc(&doc_path);
    assert!(
        reloaded_doc.entries.contains_key(&key),
        "entry must survive across the engine drop+reload (durability)"
    );
    assert_eq!(
        reloaded_doc.entries[&key].sealed_blob, fx.frame.sealed_blob,
        "reloaded sealed_blob is byte-identical to the deposited blob"
    );

    // ----- Phase 4: the reloaded engine can serve a pull query -----
    let built2 = build_relay_engine_from_doc(
        &relay_dev_id,
        reloaded_doc,
        Arc::new(NoopRelayPersist), // no further writes needed for this assertion
    );
    let relay_ctx2 = build_relay_ctx(&built2, relay_dev_id.clone());

    let cert_bytes = harmony_owner::cbor::to_canonical(&fx.recipient.cert).expect("encode cert");
    let pull_sig = fx
        .recipient
        .device_key
        .sign(&relay_pull_sig_payload(
            &fx.recipient.owner.0,
            &community_id(),
        ))
        .to_bytes()
        .to_vec();
    let query = RelayPullQuery {
        signer_certs_cbor: Vec::new(),
        recipient_owner: fx.recipient.owner.0,
        community_id: community_id(),
        requester_enrollment_cert: cert_bytes,
        sig: pull_sig,
    };

    let resp = handle_relay_pull_query(&query, &relay_ctx2)
        .await
        .expect("reloaded relay must serve pull query");

    assert_eq!(resp.entries.len(), 1, "one held blob after reload");
    assert_eq!(
        resp.entries[0].sealed_blob, fx.frame.sealed_blob,
        "sealed blob is byte-identical after reload"
    );

    let _ = built2.engine.shutdown().await;
}

// =====================================================================
// Test 5: Opacity (structural) — relay ctx has no decrypt hook;
//         relay NEVER opens the sealed blob.
// =====================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_opacity_no_decrypt_on_relay_side() {
    // STRUCTURAL ASSERTION: `RelayDepositCtx` has no `decrypt` method.
    // The relay receives `sealed_blob` and stores it verbatim. The blob is
    // sealed to the RECIPIENT's device key; without the recipient's X25519
    // private key the relay cannot open it. This test confirms:
    //   (a) the event log never records "decrypt" after a deposit,
    //   (b) attempting to open the stored blob with the RELAY's device key fails,
    //   (c) the stored bytes are byte-identical to the frame's sealed_blob.

    let fx = build_fixture();
    let relay_dev_id = relay_device_id();

    let built = build_relay_engine(&relay_dev_id, Arc::new(NoopRelayPersist));
    let relay_ctx = build_relay_ctx(&built, relay_dev_id.clone());

    handle_relay_deposit_core(&fx.frame, &relay_ctx)
        .await
        .expect("deposit");

    // (a) No "decrypt" event.
    let ev = relay_ctx.events();
    assert!(
        !ev.iter().any(|e| e == "decrypt"),
        "relay ctx must NEVER record a decrypt event: {ev:?}"
    );

    // (b) The relay's own device key CANNOT open the blob (it's sealed to the
    //     recipient's key, not the relay's). This is the cryptographic opacity proof.
    let relay_x25519 = ed25519_priv_to_x25519(&relay_owner().device_key);
    let open_result = open_from_owner_with_info(
        &relay_x25519,
        &fx.frame.sealed_blob,
        COMMUNITY_RELAY_SEAL_INFO,
    );
    assert!(
        open_result.is_err(),
        "relay's device key must NOT be able to open the sealed blob"
    );

    // (c) Stored blob byte-identical to the frame's sealed_blob.
    let key = RelayHoldDoc::key(&fx.recipient.owner.0, &fx.sealed_content_id);
    {
        // Scope the lock guard so it is dropped before shutdown, preventing a
        // deadlock: shutdown calls persist_now which also locks the doc.
        let doc = built.doc.lock().await;
        let entry = doc.entries.get(&key).expect("entry in hold doc");
        assert_eq!(
            entry.sealed_blob, fx.frame.sealed_blob,
            "relay stores sealed_blob verbatim — never transforms or decrypts it"
        );
    }

    let _ = built.engine.shutdown().await;
}

// =====================================================================
// PRODUCTION-ctx integration (ZEB-458 P4 Phase B, D45)
//
// The scenarios below exercise the SAME core handlers
// (`handle_relay_deposit_core` / `handle_relay_pull_query` /
// `handle_relay_pull_ack`) against the REAL production context impls
// (`ProdRelayDepositCtx` / `ProdRelayPullCtx`) instead of the in-file
// `TestRelayCtx`. The core handlers take `&dyn RelayDepositCtx` /
// `&dyn RelayPullCtx`, so this is a pure ctx swap that additionally proves:
//   - prod admission: opt-in gate (`RelayOptInDoc`) + co-membership gate
//     (`CommunityMembershipLookup`) + the enrollment-cert/sig crypto;
//   - prod persist/flush over the real `FleetSyncEngine<RelayHoldDoc>` via
//     `EngineRelayHoldFlush` (durable flush BEFORE the ack, caps enforced);
//   - prod `held_for` / `mark_pulled` (pulled_by union, no inline GC).
//
// The membership + flush seams are recreated here (the production module's
// `FakeMembership` / `NoopFlush` are `#[cfg(test)]`-private, so an integration
// binary — which compiles against the public API — cannot see them).
//
// The recipient-side ingest keeps the in-file `TestRecipientIngestCtx`: the
// production `ProdRelayIngestCtx::ingest_recovered` runs the full receive path
// (verify_cidnotify_admission → apply_inbox → emit) which needs a fully set-up
// recipient `OwnerState`; its correctness is unit-reviewed in T9a, and the
// open-and-ingest seam is what this test exercises.
// =====================================================================

/// Integration-local membership seam: a fixed set of `(community, owner)`
/// Joined pairs. Mirrors the production module's `#[cfg(test)]` `FakeMembership`.
struct FakeMembership {
    joined: BTreeSet<([u8; 16], [u8; 16])>,
}

#[async_trait]
impl CommunityMembershipLookup for FakeMembership {
    async fn is_joined(&self, c: &SpaceId, o: &[u8; 16]) -> bool {
        self.joined.contains(&(c.0, *o))
    }
}

/// An opt-in doc that has opted in to `community`.
fn opted_in_doc(community: SpaceId) -> Arc<Mutex<RelayOptInDoc>> {
    let mut d = RelayOptInDoc::default();
    d.set(
        community,
        true,
        Hlc {
            wall_ms: 1,
            logical: 0,
            device_id: "relay".into(),
        },
    );
    Arc::new(Mutex::new(d))
}

/// Build the PRODUCTION deposit + pull ctxs over the SAME real engine in
/// `built` (so a deposit's flush + the pull's `held_for` see one shared doc).
/// `members` is the set of `(community.0, owner.0)` pairs the `FakeMembership`
/// treats as Joined; `opted_in` controls whether the opt-in doc has the
/// community opted in (false → `serves_community` is false → WrongCommunity).
fn build_prod_ctxs(
    built: &Built,
    opted_in: bool,
    members: BTreeSet<([u8; 16], [u8; 16])>,
) -> (ProdRelayDepositCtx, ProdRelayPullCtx) {
    let relay = relay_owner();
    let cid = community_id();
    let relay_dev_id = relay_device_id();

    let membership: Arc<dyn CommunityMembershipLookup> =
        Arc::new(FakeMembership { joined: members });

    let optin = if opted_in {
        opted_in_doc(cid)
    } else {
        Arc::new(Mutex::new(RelayOptInDoc::default()))
    };

    // BOTH ctxs flush through the SAME engine (EngineRelayHoldFlush) and share
    // the engine's doc + tracker Arcs, exactly as start_node wires them.
    let deposit = ProdRelayDepositCtx {
        self_owner: relay.owner.0,
        relay_device_id: relay_dev_id,
        relay_hold_doc: Arc::clone(&built.doc),
        relay_hold_tracker: Arc::clone(&built.tracker),
        flush: Arc::new(EngineRelayHoldFlush(Arc::clone(&built.engine))),
        optin: Arc::clone(&optin),
        membership: Arc::clone(&membership),
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
    };
    let pull = ProdRelayPullCtx {
        self_owner: relay.owner.0,
        relay_hold_doc: Arc::clone(&built.doc),
        flush: Arc::new(EngineRelayHoldFlush(Arc::clone(&built.engine))),
        optin,
        membership,
    };
    (deposit, pull)
}

/// The `(community, owner)` membership set for the happy path: relay, sender,
/// and recipient are ALL Joined members of the community.
fn all_joined_members() -> BTreeSet<([u8; 16], [u8; 16])> {
    let cid = community_id();
    let mut m = BTreeSet::new();
    m.insert((cid.0, relay_owner().owner.0));
    m.insert((cid.0, sender_owner().owner.0));
    m.insert((cid.0, recipient_owner().owner.0));
    m
}

// =====================================================================
// Prod-ctx Test 1: Happy path over the PRODUCTION ctxs — admission (opt-in +
// co-membership + cert/sig) → durable persist/flush → pull → open_and_ingest
// → ack → pulled_by → gc. Opacity (verbatim sealed_blob) re-asserted.
// =====================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_prod_ctx_happy_path_full_round_trip() {
    let fx = build_fixture();
    let relay_dev_id = relay_device_id();

    let built = build_relay_engine(&relay_dev_id, Arc::new(NoopRelayPersist));
    let (deposit_ctx, pull_ctx) = build_prod_ctxs(&built, true, all_joined_members());

    // ----- (1) Sender deposits through the PROD deposit ctx -----
    // serves_community (opt-in + relay self-joined) → both_co_members (sender +
    // recipient joined) → cert/sig verify → atomic persist-with-caps + flush.
    let ack = handle_relay_deposit_core(&fx.frame, &deposit_ctx)
        .await
        .expect("valid co-member deposit must be accepted by the prod ctx");
    assert_eq!(ack.content_id, fx.sealed_content_id);

    // Hold doc has the entry, sealed_blob verbatim (relay never decrypts).
    let key = RelayHoldDoc::key(&fx.recipient.owner.0, &ack.content_id);
    {
        let doc = built.doc.lock().await;
        let entry = doc.entries.get(&key).expect("entry in hold doc");
        assert_eq!(
            entry.sealed_blob, fx.frame.sealed_blob,
            "prod relay holds sealed_blob verbatim (no decrypt)"
        );
        assert_eq!(entry.recipient_owner, fx.recipient.owner.0);
        assert_eq!(entry.sender_owner, fx.sender.owner.0);
        assert_eq!(entry.community_id, community_id());
        assert_eq!(
            entry.held_by, relay_dev_id,
            "held_by stamped with the prod relay device id"
        );
        assert!(entry.pulled_by.is_empty(), "fresh hold has no pulls");
    }

    // ----- (2) Recipient builds + sends an authed pull query (PROD pull ctx) -----
    let cert_bytes = harmony_owner::cbor::to_canonical(&fx.recipient.cert).expect("encode cert");
    let pull_sig = fx
        .recipient
        .device_key
        .sign(&relay_pull_sig_payload(
            &fx.recipient.owner.0,
            &community_id(),
        ))
        .to_bytes()
        .to_vec();
    let query = RelayPullQuery {
        signer_certs_cbor: Vec::new(),
        recipient_owner: fx.recipient.owner.0,
        community_id: community_id(),
        requester_enrollment_cert: cert_bytes.clone(),
        sig: pull_sig,
    };
    let resp = handle_relay_pull_query(&query, &pull_ctx)
        .await
        .expect("authed pull query must succeed against the prod ctx");
    assert_eq!(resp.entries.len(), 1, "one held blob for this recipient");
    assert_eq!(
        resp.entries[0].sealed_blob, fx.frame.sealed_blob,
        "prod pull delivers the verbatim sealed blob"
    );
    assert_eq!(resp.entries[0].sender_owner, fx.sender.owner.0);

    // ----- (3) Recipient opens + ingests via open_and_ingest -----
    let recipient_x25519 = ed25519_priv_to_x25519(&fx.recipient.device_key);
    let ingest_ctx = TestRecipientIngestCtx::new(vec![recipient_x25519]);
    let acked_content_ids = open_and_ingest(&resp.entries, &ingest_ctx).await;
    assert_eq!(acked_content_ids.len(), 1);
    assert_eq!(acked_content_ids[0], fx.sealed_content_id);
    let ingested = ingest_ctx.ingested();
    assert_eq!(ingested.len(), 1);
    assert_eq!(
        ingested[0], fx.payload,
        "recovered payload matches original"
    );

    // ----- (4) Recipient sends pull ack through the PROD pull ctx -----
    let ack_msg = RelayPullAck {
        content_ids: acked_content_ids.clone(),
    };
    let pull_ack_sig = fx
        .recipient
        .device_key
        .sign(&relay_pull_ack_sig_payload(
            &fx.recipient.owner.0,
            &community_id(),
            &acked_content_ids,
        ))
        .to_bytes()
        .to_vec();
    handle_relay_pull_ack(
        &fx.recipient.owner.0,
        &community_id(),
        &ack_msg,
        &cert_bytes,
        &[],
        &pull_ack_sig,
        &pull_ctx,
    )
    .await
    .expect("pull ack must succeed against the prod ctx");

    // mark_pulled unioned pulled_by under the prod ctx (no inline GC).
    {
        let doc = built.doc.lock().await;
        let entry = doc
            .entries
            .get(&key)
            .expect("entry still present before gc");
        let recipient_device_id =
            hex::encode(fx.recipient.cert.device_pubkeys.classical.ed25519_verify);
        assert!(
            entry.pulled_by.contains(&recipient_device_id),
            "prod mark_pulled must record the recipient device in pulled_by"
        );
    }

    // ----- (5) The SEPARATE periodic GC sweep removes the covered entry -----
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let changed = built.doc.lock().await.gc(now_ms);
    assert!(changed, "gc must remove the covered (acked) entry");
    assert!(
        built.doc.lock().await.entries.is_empty(),
        "hold doc empty after gc reclaims the covered entry"
    );

    let _ = built.engine.shutdown().await;
}

// =====================================================================
// Prod-ctx Test 2: Non-member sender — the PROD deposit ctx's co-membership
// gate (FakeMembership WITHOUT the sender) rejects with NotCoMember, and the
// hold doc stays EMPTY (nothing is held pre-decrypt).
// =====================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_prod_ctx_non_member_sender_rejected_doc_empty() {
    let fx = build_fixture();
    let relay_dev_id = relay_device_id();

    let built = build_relay_engine(&relay_dev_id, Arc::new(NoopRelayPersist));

    // Membership has the relay + recipient joined, but NOT the sender — so
    // both_co_members is false → NotCoMember (and we never reach persist).
    let cid = community_id();
    let mut members = BTreeSet::new();
    members.insert((cid.0, relay_owner().owner.0));
    members.insert((cid.0, recipient_owner().owner.0));
    // (sender_owner deliberately omitted)
    let (deposit_ctx, _pull_ctx) = build_prod_ctxs(&built, true, members);

    let err = handle_relay_deposit_core(&fx.frame, &deposit_ctx)
        .await
        .expect_err("non-co-member sender must be rejected by the prod ctx");
    assert!(
        matches!(err, RelayDepositReject::NotCoMember),
        "expected NotCoMember, got {err:?}"
    );

    // The hold doc must be EMPTY — nothing is held before admission passes.
    {
        let doc = built.doc.lock().await;
        assert!(
            doc.entries.is_empty(),
            "hold doc must be EMPTY after a non-member reject (nothing held pre-decrypt)"
        );
    }

    let _ = built.engine.shutdown().await;
}

// =====================================================================
// Prod-ctx Test 3: Restart survival through the PROD pull ctx — deposit via the
// prod deposit ctx, drop + reload the relay engine from the persisted
// RelayHoldDoc, then serve a pull query off the reloaded engine through the
// PROD pull ctx.
// =====================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_prod_ctx_restart_durability_pull_via_prod_ctx() {
    let fx = build_fixture();
    let relay_dev_id = relay_device_id();

    let dir = tempfile::tempdir().expect("tempdir");
    let doc_path = dir.path().join("relay_hold.cbor");
    let replay_path = dir.path().join("relay_hold_replay.cbor");

    // ----- Phase 1: deposit + durable flush through the PROD deposit ctx -----
    let built = build_relay_engine(
        &relay_dev_id,
        Arc::new(RelayHoldPersist {
            doc_path: doc_path.clone(),
            replay_path: replay_path.clone(),
        }),
    );
    let (deposit_ctx, _pull_ctx) = build_prod_ctxs(&built, true, all_joined_members());

    let ack = handle_relay_deposit_core(&fx.frame, &deposit_ctx)
        .await
        .expect("deposit through the prod ctx succeeds");
    let key = RelayHoldDoc::key(&fx.recipient.owner.0, &ack.content_id);

    // The prod persist_hold flushes synchronously BEFORE returning the ack, so
    // the file is already written; assert it before the drop.
    assert!(
        built.doc.lock().await.entries.contains_key(&key),
        "entry must be in the doc after the prod deposit"
    );
    assert!(doc_path.exists(), "relay_hold.cbor must exist after flush");

    // ----- Phase 2: drop the engine + ctxs (simulated restart) -----
    let _ = built.engine.shutdown().await;
    drop(deposit_ctx);
    drop(built);

    // ----- Phase 3: reload the doc from disk -----
    let reloaded_doc = load_relay_hold_doc(&doc_path);
    assert!(
        reloaded_doc.entries.contains_key(&key),
        "entry must survive the engine drop+reload (durability)"
    );
    assert_eq!(
        reloaded_doc.entries[&key].sealed_blob, fx.frame.sealed_blob,
        "reloaded sealed_blob is byte-identical to the deposited blob"
    );

    // ----- Phase 4: the reloaded engine serves a pull query via the PROD ctx -----
    let built2 =
        build_relay_engine_from_doc(&relay_dev_id, reloaded_doc, Arc::new(NoopRelayPersist));
    let (_deposit2, pull_ctx2) = build_prod_ctxs(&built2, true, all_joined_members());

    let cert_bytes = harmony_owner::cbor::to_canonical(&fx.recipient.cert).expect("encode cert");
    let pull_sig = fx
        .recipient
        .device_key
        .sign(&relay_pull_sig_payload(
            &fx.recipient.owner.0,
            &community_id(),
        ))
        .to_bytes()
        .to_vec();
    let query = RelayPullQuery {
        signer_certs_cbor: Vec::new(),
        recipient_owner: fx.recipient.owner.0,
        community_id: community_id(),
        requester_enrollment_cert: cert_bytes,
        sig: pull_sig,
    };

    let resp = handle_relay_pull_query(&query, &pull_ctx2)
        .await
        .expect("reloaded relay must serve a pull query through the prod ctx");
    assert_eq!(resp.entries.len(), 1, "one held blob after reload");
    assert_eq!(
        resp.entries[0].sealed_blob, fx.frame.sealed_blob,
        "sealed blob is byte-identical after reload + prod-ctx pull"
    );

    let _ = built2.engine.shutdown().await;
}

// =====================================================================
// Prod-ctx Test 4: multi-blob backlog DRAIN — the per-device `held_for`
// exclusion means a recipient device that pulls + acks its held blobs sees an
// EMPTY response on its next pull (no re-serve of acked-but-not-yet-GC'd
// entries), proving the bounded/drain contract end-to-end over the prod ctxs.
// =====================================================================

/// Build a deposit frame for the standard sender/recipient/community with a
/// custom `storage_blob` (distinct content → distinct sealed content id →
/// distinct hold-doc key). Returns `(frame, sealed_content_id)`.
fn build_deposit_frame_with_blob(
    sender: &TestOwner,
    recipient: &TestOwner,
    storage_blob: Vec<u8>,
) -> (harmony_app::community_relay::RelayDepositFrame, [u8; 32]) {
    let cid = community_id();
    let payload = DepositPayload {
        cidnotify_packet: Some(vec![0x01, 0x02, 0x03, 0x04]),
        storage_blob,
        invite_packet: None,
        revocation_push: None,
        grant_push: None,
        grant_revoke: None,
    };
    let cert_bytes = harmony_owner::cbor::to_canonical(&sender.cert).expect("encode cert");
    let frame = build_relay_deposit_frame(
        recipient.owner.0,
        &recipient.cert.device_pubkeys.classical.ed25519_verify,
        sender.owner.0,
        cid,
        cert_bytes,
        &sender.device_key,
        &payload,
    )
    .expect("build_relay_deposit_frame");
    let sealed_content_id = ContentId::for_book(
        &frame.sealed_blob,
        ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .expect("content_id for sealed blob")
    .to_bytes();
    (frame, sealed_content_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_prod_ctx_multi_blob_drain_no_reserve_after_ack() {
    let sender = sender_owner();
    let recipient = recipient_owner();
    let relay_dev_id = relay_device_id();

    let built = build_relay_engine(&relay_dev_id, Arc::new(NoopRelayPersist));
    let (deposit_ctx, pull_ctx) = build_prod_ctxs(&built, true, all_joined_members());

    // ----- (1) Deposit THREE distinct blobs for the recipient -----
    let frames: Vec<_> = [
        b"blob-one".to_vec(),
        b"blob-two".to_vec(),
        b"blob-three".to_vec(),
    ]
    .into_iter()
    .map(|b| build_deposit_frame_with_blob(&sender, &recipient, b))
    .collect();
    for (frame, _cid) in &frames {
        handle_relay_deposit_core(frame, &deposit_ctx)
            .await
            .expect("co-member deposit must be accepted");
    }
    assert_eq!(
        built.doc.lock().await.entries.len(),
        3,
        "three held blobs after three deposits"
    );

    // ----- (2) First pull returns the full un-delivered set -----
    let cert_bytes = harmony_owner::cbor::to_canonical(&recipient.cert).expect("encode cert");
    let pull_sig = recipient
        .device_key
        .sign(&relay_pull_sig_payload(&recipient.owner.0, &community_id()))
        .to_bytes()
        .to_vec();
    let query = RelayPullQuery {
        signer_certs_cbor: Vec::new(),
        recipient_owner: recipient.owner.0,
        community_id: community_id(),
        requester_enrollment_cert: cert_bytes.clone(),
        sig: pull_sig.clone(),
    };
    let resp1 = handle_relay_pull_query(&query, &pull_ctx)
        .await
        .expect("first pull must succeed");
    assert_eq!(
        resp1.entries.len(),
        3,
        "first pull serves all three un-delivered blobs"
    );

    // ----- (3) Ack ALL three content ids (as the honest client would after
    // a successful open+ingest) -----
    let acked: Vec<[u8; 32]> = frames.iter().map(|(_, cid)| *cid).collect();
    let ack_msg = RelayPullAck {
        content_ids: acked.clone(),
    };
    let ack_sig = recipient
        .device_key
        .sign(&relay_pull_ack_sig_payload(
            &recipient.owner.0,
            &community_id(),
            &acked,
        ))
        .to_bytes()
        .to_vec();
    handle_relay_pull_ack(
        &recipient.owner.0,
        &community_id(),
        &ack_msg,
        &cert_bytes,
        &[],
        &ack_sig,
        &pull_ctx,
    )
    .await
    .expect("pull ack must succeed");

    // The entries linger (GC is a separate sweep), each now carrying this
    // device in pulled_by.
    let recipient_device_id = hex::encode(recipient.cert.device_pubkeys.classical.ed25519_verify);
    {
        let doc = built.doc.lock().await;
        assert_eq!(doc.entries.len(), 3, "entries still present pre-GC");
        assert!(
            doc.entries
                .values()
                .all(|e| e.pulled_by.contains(&recipient_device_id)),
            "every entry now carries this device in pulled_by"
        );
    }

    // ----- (4) Second pull (SAME device) returns EMPTY — none re-served -----
    let query2 = RelayPullQuery {
        signer_certs_cbor: Vec::new(),
        recipient_owner: recipient.owner.0,
        community_id: community_id(),
        requester_enrollment_cert: cert_bytes,
        sig: pull_sig,
    };
    let resp2 = handle_relay_pull_query(&query2, &pull_ctx)
        .await
        .expect("second pull must succeed");
    assert!(
        resp2.entries.is_empty(),
        "second pull re-serves nothing — all blobs already delivered+acked to this device"
    );

    let _ = built.engine.shutdown().await;
}
