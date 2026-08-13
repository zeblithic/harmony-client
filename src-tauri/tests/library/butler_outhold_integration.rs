//! ZEB-418 SP2 P2 Task 9: two-engine fleet-handoff butler-deposit
//! integration — the P2 capstone assembling Tasks 1–8 end to end through
//! public crate APIs only.
//!
//! The headline path: a DM sent on device A1 is delivered by SIBLING
//! device A2 via the recipient's butler (owner B's device B1) after A1
//! goes offline. Staged exactly per the plan:
//!
//!   1. Owner A fleet: two `FleetSyncEngine<DmOutholdDoc>` (A1 + A2)
//!      bridged in-process (the P1 harness pattern), each device with its
//!      own `OwnerState` and its own DM CAS. The outbox entry replicates
//!      A1 → A2 via `apply_outbox` (the owner-state-sync shim — no full
//!      second engine bridge, per plan).
//!   2. A1 sends: `send_dm` with the outhold installed writes the hold
//!      row (Task 3); `flush_now` fans it out over the bridge; A2's
//!      apply sweep (`sweep_once`, Task 5) admits the blob into A2's
//!      local CAS.
//!   3. A1 stops: engine shut down, outbox + state dropped — no further
//!      A1 activity.
//!   4. A2 delivers via butler: drain ticks past the ZEB-422
//!      sent-but-never-acked windows (StubTransport = Ok-enqueued, never
//!      acks) until the deposit rung fires; the test deposit client
//!      builds the REAL sender frame (`build_deposit_frame`, the Task 8
//!      construction) signed with A2's OWN distinct enrollment cert (not
//!      A1's — proving cross-device acceptance, PR #222 round 1) and
//!      drives owner B's REAL acceptor pipeline
//!      (`handle_deposit_core`, Task 5/P1) in-process; B persists into
//!      its dm-inbox doc and acks; the ack marks the recipient delivered
//!      → `DeliveryStatus::Complete` (1:1 DM, single recipient).
//!   5. GC: A2's next outhold sweep removes the now-Complete row and
//!      fires notify_dirty (the deletion would publish).
//!
//! Plan note (spec §9): the "sibling direct-delivery variant" is
//! deliberately covered by the CAS-presence assertion in stage 2 — once
//! the blob is in A2's local CAS, serving the CidNotify fetch-back is the
//! ZEB-343/P1-tested CAS-serve machinery, NOT re-proven here with a full
//! zenoh serve harness.
//!
//! Timing: explicit `flush_now` + bounded condition-polling — no fixed
//! sleeps (the SP1 harness's `wait_until` discipline). Drain timestamps
//! (10s/16s/27s) copy the T4 unit test
//! `noack_after_n_windows_triggers_deposit_rung` window arithmetic.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use async_trait::async_trait;
use ed25519_dalek::SigningKey;
use harmony_app::butler_deposit::{
    build_deposit_frame, ButlerDepositClient, ButlerDepositRequest, DepositPayload,
    DepositRungOutcome, BUTLER_DEPOSIT_SEAL_INFO, DEPOSIT_NOACK_WINDOWS, INBOX_GLOBAL_CAP,
    INBOX_PER_SENDER_CAP,
};
use harmony_app::community_membership::{mint_test_owner, TestOwner};
use harmony_app::content_store::{ContentStore, InMemoryStub};
use harmony_app::dm_inbox_crdt::{DmInboxDoc, DmInboxEntry};
use harmony_app::dm_inbox_persist::DmInboxPersist;
use harmony_app::dm_outbox::{DmOutbox, StubTransport};
use harmony_app::dm_outhold::DmOutholdDoc;
use harmony_app::dm_outhold_apply::{sweep_once, ProdDmOutholdCtx};
use harmony_app::dm_outhold_persist::DmOutholdPersist;
use harmony_app::dm_signing::{
    derive_device_hash_from_identity_pub, ed25519_priv_to_x25519, open_from_owner_with_info,
};
use harmony_app::fleet_sync::{mint_next_hlc, FleetSyncConfig, FleetSyncEngine, Merger};
use harmony_app::friend_graph::FriendStatus;
use harmony_app::iroh_butler_acceptor::{
    handle_deposit_core, ButlerDepositCtx, DepositPersistVerdict,
};
use harmony_app::owner_state_crdt::{ApplyOutcome, OwnerState};
use harmony_app::owner_state_crypto::KeyTree;
use harmony_app::owner_state_types::{
    DeliveryStatus, DeviceIdentityHash, DmContentKey, Hlc, OwnerAddr, Space, SpaceId, SpaceKind,
};
use harmony_owner::certs::{EnrollmentCert, EnrollmentIssuer};
use tokio::sync::{mpsc, Mutex};

/// Owner B — the DM recipient whose butler (device B1) completes delivery.
const RECIPIENT_OWNER: [u8; 16] = [0x42; 16];

/// A device's DM-transport identity (signs CidNotify packets). Synthetic
/// identity_pub per the dm_outbox/acceptor test convention: all-zero
/// X25519 half + real Ed25519 half (the P1 harness's `dm_identity`,
/// parameterized by seed so A1 and A2 get DISTINCT device identities).
fn dm_identity(seed: u8) -> (SigningKey, [u8; 64], DeviceIdentityHash) {
    let sk = SigningKey::from_bytes(&[seed; 32]);
    let mut identity_pub = [0u8; 64];
    identity_pub[32..].copy_from_slice(sk.verifying_key().as_bytes());
    let hash = derive_device_hash_from_identity_pub(&identity_pub)
        .expect("synthetic identity_pub is valid");
    (sk, identity_pub, hash)
}

/// SP1 device id form: 64-hex (lowercase) of the device ed25519 verify key.
fn device_id_hex(sk: &SigningKey) -> String {
    hex::encode(sk.verifying_key().to_bytes())
}

fn master_from_cert(cert: &EnrollmentCert) -> [u8; 32] {
    match &cert.issuer {
        EnrollmentIssuer::Master { master_pubkey } => master_pubkey.classical.ed25519_verify,
        other => panic!("test certs are Master-issued, got {other:?}"),
    }
}

/// Mint a SECOND enrolled device under the same owner that
/// `mint_test_owner(master_seed)` produces: the same master key + bundle, a
/// NEW device key from `device_seed`, and a Master-signed `EnrollmentCert`
/// binding them (mirrors `mint_test_owner` in community_membership.rs
/// step-for-step). Lets the capstone prove the P1 acceptor admits a deposit
/// signed by a DIFFERENT enrolled device of the sender's fleet — the PR's
/// load-bearing "any online device can complete delivery" claim.
///
/// Seed caveat (documented on `mint_test_owner`): seeds `N` and `N ^ 0xFF`
/// share raw key material. The seeds chosen in this file — master 0x51
/// (default device 0x51^0xFF = 0xAE), sibling device 0x55 (complement
/// 0xAA), DM-transport identities 0x53/0x54/0x61, PrivateIdentity seeds
/// 0x71/0x72 — avoid every such pairing.
fn mint_sibling_device(master_seed: u8, device_seed: u8) -> (SigningKey, EnrollmentCert) {
    use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};
    assert_ne!(device_seed, master_seed, "would duplicate the master key");
    assert_ne!(
        device_seed,
        master_seed ^ 0xFF,
        "would duplicate the owner's default device key"
    );
    let master_sk = SigningKey::from_bytes(&[master_seed; 32]);
    let master_bundle = PubKeyBundle {
        classical: ClassicalKeys {
            ed25519_verify: master_sk.verifying_key().to_bytes(),
            x25519_pub: [0u8; 32],
        },
        post_quantum: None,
    };
    let device_sk = SigningKey::from_bytes(&[device_seed; 32]);
    let device_bundle = PubKeyBundle {
        classical: ClassicalKeys {
            ed25519_verify: device_sk.verifying_key().to_bytes(),
            x25519_pub: [0u8; 32],
        },
        post_quantum: None,
    };
    let device_id = device_bundle.identity_hash();
    let cert = EnrollmentCert::sign_master(
        &master_sk,
        master_bundle,
        device_id,
        device_bundle,
        1_700_000_000,
        None,
    )
    .expect("sign_master sibling cert");
    cert.verify(0).expect("sibling cert verifies");
    (device_sk, cert)
}

/// A production-checked `DmOutbox` for one of owner A's devices: shared
/// owner identity but a per-device DM-transport identity AND per-device
/// enrolled deposit credentials (`deposit_device_key` + `deposit_cert` —
/// the cert-bound key that signs deposit frames). A1 passes the owner's
/// default device creds; A2 passes the DISTINCT sibling-device creds from
/// `mint_sibling_device`, so the capstone proves cross-device deposit
/// acceptance. Routes through `DmOutbox::new` so the ZEB-339
/// cert↔owner↔key asserts run (the material is fully consistent).
fn make_owner_outbox(
    device_id: &str,
    owner: &TestOwner,
    dm_sk: &SigningKey,
    dm_hash: DeviceIdentityHash,
    identity_seed: u8,
    deposit_device_key: &SigningKey,
    deposit_cert: EnrollmentCert,
) -> DmOutbox {
    DmOutbox::new(
        device_id.to_string(),
        owner.owner,
        dm_hash,
        Arc::new(SigningKey::from_bytes(&dm_sk.to_bytes())),
        Arc::new(harmony_identity::PrivateIdentity::from_seed(
            &[identity_seed; 32],
        )),
        Arc::new(SigningKey::from_bytes(&deposit_device_key.to_bytes())),
        deposit_cert,
    )
}

/// Build a minimal-but-valid DM Space (the dm_outbox test fixture shape):
/// members sorted ascending, Reticulum transport, content_key Some.
fn make_dm_space(id_byte: u8, mut members: Vec<OwnerAddr>) -> Space {
    members.sort();
    Space {
        id: SpaceId([id_byte; 16]),
        kind: SpaceKind::Dm,
        parent: None,
        community_id: None,
        name: "Bob".into(),
        transport: None,
        members,
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc {
            wall_ms: 0,
            logical: 0,
            device_id: "dev".into(),
        },
        updated_at: Hlc {
            wall_ms: 0,
            logical: 0,
            device_id: "dev".into(),
        },
        content_key: Some(DmContentKey::new([0x42u8; 32])),
        prior_content_keys: vec![],
        current_epoch: None,
        current_epoch_key: None,
        old_epoch_keys: BTreeMap::new(),
        admin_addr: None,
        is_invite_only: None,
        shared_in_profile: false,
        read_receipt_pref: None,
        pending_join_at: None,
    }
}

fn install_space(state: &mut OwnerState, sp: Space) {
    let outcome = state.apply_space_with_canonicalization(sp);
    assert!(
        matches!(outcome, ApplyOutcome::Inserted),
        "fixture install must succeed, got {outcome:?}"
    );
}

// =====================================================================
// SP1 two-engine harness (the P1 `butler_deposit_integration` pattern:
// in-memory mpsc bridge, per-engine doc + tracker, real persist sinks)
// =====================================================================

struct Built<S: Send + 'static> {
    engine: Arc<FleetSyncEngine<S>>,
    doc: Arc<Mutex<S>>,
    tracker: Arc<Mutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>>,
    out_rx: mpsc::Receiver<Vec<u8>>,
    in_tx: mpsc::Sender<Vec<u8>>,
}

/// A dm-outhold engine configured as `lib.rs::start_node` configures it
/// (DmOutholdPersist sink, `merge_from` merger, `publish_seen: true`,
/// lookup tag `b"dm-outhold-v1"`), except `on_applied: None` — the apply
/// sweeps are driven explicitly in these tests (the P1 harness
/// discipline) rather than via the production nudge channel.
fn build_outhold_engine(
    device_id: &str,
    kt: Arc<KeyTree>,
    cas: Arc<dyn ContentStore>,
    dir: &std::path::Path,
) -> Built<DmOutholdDoc> {
    let (out_tx, out_rx) = mpsc::channel(64);
    let (in_tx, in_rx) = mpsc::channel(64);
    let doc = Arc::new(Mutex::new(DmOutholdDoc::default()));
    let tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        device_id.to_string(),
    )));
    let merger: Merger<DmOutholdDoc> = Arc::new(|local, remote| local.merge_from(remote));
    let engine = Arc::new(FleetSyncEngine::new(FleetSyncConfig {
        keys: Some(harmony_app::owner_state_crypto::FleetKeySet::new(kt)),
        device_id: device_id.to_string(),
        state: Arc::clone(&doc),
        merger,
        replay_tracker: Arc::clone(&tracker),
        content_store: cas,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        persist: Arc::new(DmOutholdPersist {
            doc_path: dir.join("dm_outhold.cbor"),
            replay_path: dir.join("dm_outhold_replay.cbor"),
        }),
        lookup_key_tag: b"dm-outhold-v1",
        debounce_ms: 50,
        publish_seen: true,
        on_applied: None, // sweeps driven explicitly in this test
        sibling_acks: Arc::new(Mutex::new(harmony_crdt_sync::MonotoneMap::new())),
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
    }));
    Built {
        engine,
        doc,
        tracker,
        out_rx,
        in_tx,
    }
}

/// Owner B's dm-inbox engine (the P1 `build_engine` shape) — backs the
/// butler acceptor ctx's persist-before-ack path.
fn build_inbox_engine(
    device_id: &str,
    kt: Arc<KeyTree>,
    cas: Arc<dyn ContentStore>,
    dir: &std::path::Path,
) -> Built<DmInboxDoc> {
    let (out_tx, out_rx) = mpsc::channel(64);
    let (in_tx, in_rx) = mpsc::channel(64);
    let doc = Arc::new(Mutex::new(DmInboxDoc::default()));
    let tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        device_id.to_string(),
    )));
    let merger: Merger<DmInboxDoc> = Arc::new(|local, remote| local.merge_from(remote));
    let engine = Arc::new(FleetSyncEngine::new(FleetSyncConfig {
        keys: Some(harmony_app::owner_state_crypto::FleetKeySet::new(kt)),
        device_id: device_id.to_string(),
        state: Arc::clone(&doc),
        merger,
        replay_tracker: Arc::clone(&tracker),
        content_store: cas,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        persist: Arc::new(DmInboxPersist {
            doc_path: dir.join("dm_inbox.cbor"),
            replay_path: dir.join("dm_inbox_replay.cbor"),
            first_observed_path: dir.join("dm_inbox_first_observed.cbor"),
            expired_path: dir.join("dm_inbox_expired.cbor"),
        }),
        lookup_key_tag: b"dm-inbox-v1",
        debounce_ms: 50,
        publish_seen: true,
        on_applied: None,
        sibling_acks: Arc::new(Mutex::new(harmony_crdt_sync::MonotoneMap::new())),
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
    }));
    Built {
        engine,
        doc,
        tracker,
        out_rx,
        in_tx,
    }
}

/// In-memory bridge: a.out -> b.in and b.out -> a.in (the
/// `two_engines_converge` forwarder verbatim).
fn bridge<S: Send + 'static>(a: &mut Built<S>, b: &mut Built<S>) -> tokio::task::JoinHandle<()> {
    let a_in = a.in_tx.clone();
    let b_in = b.in_tx.clone();
    let mut a_out = std::mem::replace(&mut a.out_rx, mpsc::channel(1).1);
    let mut b_out = std::mem::replace(&mut b.out_rx, mpsc::channel(1).1);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(frame) = a_out.recv() => { let _ = b_in.send(frame).await; }
                Some(frame) = b_out.recv() => { let _ = a_in.send(frame).await; }
                else => break,
            }
        }
    })
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

// =====================================================================
// Owner B's butler deposit ctx over REAL doc/engine handles (the P1
// integration test's `TestButlerCtx`, mirroring ProdButlerDepositCtx
// method-for-method; friend graph / device cache lookups come from test
// maps instead of a full OwnerState)
// =====================================================================

struct TestButlerCtx {
    self_owner: [u8; 16],
    device_id: String,
    friends: BTreeMap<[u8; 16], ([u8; 32], FriendStatus)>,
    /// Sender DEVICE → (owner id, identity pub) — the
    /// `resolve_sender_device` source (mirrors the production
    /// `owner_device_cache` resolution).
    device_owners: BTreeMap<DeviceIdentityHash, ([u8; 16], [u8; 64])>,
    butler_sk: SigningKey,
    doc: Arc<Mutex<DmInboxDoc>>,
    tracker: Arc<Mutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>>,
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

    // ZEB-424: this outhold suite only deposits from friend senders, so the
    // co-member admission fallback is never exercised here — always false.
    async fn shares_live_group_dm(&self, _sender_owner: &[u8; 16]) -> bool {
        false
    }

    // ZEB-424 (D28.1): co-member path unused here (friend-only senders).
    async fn space_live_group_dm_co_member(
        &self,
        _space_id: &[u8; 16],
        _sender_owner: &[u8; 16],
    ) -> bool {
        false
    }

    fn now_secs(&self) -> u64 {
        // After the mint_test_owner cert's 1_700_000_000 signing timestamp.
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
        mint_next_hlc(
            &self.tracker,
            &harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
            &self.device_id,
        )
        .await
    }

    /// ProdButlerDepositCtx::persist_entry verbatim: atomic
    /// persist-with-caps under the doc lock, then `notify_dirty` +
    /// `flush_now().await` (durable publish + persist) BEFORE returning —
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
// A2 → B deposit client: implements the sender-side ButlerDepositClient
// seam by building the REAL sealed frame (`build_deposit_frame` — the
// exact Task 8 / IrohButlerDepositClient construction, blob fetched from
// the sender device's CAS) and invoking owner B's acceptor pipeline
// in-process instead of over an iroh dial. Ack verification mirrors
// `IrohButlerDepositClient::deposit_to_entry`.
// =====================================================================

struct InProcessButlerClient {
    butler_ctx: Arc<TestButlerCtx>,
    /// The butler device's ed25519 verify key — the seal target
    /// (birational X25519 derivation happens inside build_deposit_frame).
    butler_device_vk: [u8; 32],
    /// Sender (owner A) address bytes — `DepositFrame.sender_owner`.
    sender_owner: [u8; 16],
    /// Canonical CBOR of owner A's Master EnrollmentCert.
    enrollment_cert_bytes: Vec<u8>,
    /// The cert-bound enrolled device signing key (#2, ZEB-339).
    device_signing_key: SigningKey,
    /// The DEPOSITING device's CAS (A2) — the storage blob source,
    /// exactly like IrohButlerDepositClient's `cas` field.
    cas: Arc<dyn ContentStore>,
    /// Every request the drain's deposit rung handed us, for assertions.
    requests: StdMutex<Vec<ButlerDepositRequest>>,
}

impl InProcessButlerClient {
    fn requests(&self) -> Vec<ButlerDepositRequest> {
        self.requests.lock().expect("client poisoned").clone()
    }
}

#[async_trait]
impl ButlerDepositClient for InProcessButlerClient {
    async fn deposit(&self, req: &ButlerDepositRequest) -> DepositRungOutcome {
        self.requests
            .lock()
            .expect("client poisoned")
            .push(req.clone());
        // Blob from the sender device's CAS (IrohButlerDepositClient step 2).
        let storage_blob = match self
            .cas
            .get(
                req.message_cid
                    .as_ref()
                    .expect("message deposit has message_cid"),
            )
            .await
        {
            Ok(Some(blob)) => blob,
            Ok(None) => {
                return DepositRungOutcome::Failed("storage blob missing from CAS".to_string())
            }
            Err(e) => return DepositRungOutcome::Failed(format!("CAS get: {e}")),
        };
        let payload = DepositPayload {
            cidnotify_packet: req.cidnotify_packet.clone(),
            storage_blob,
            invite_packet: req.invite_packet.clone(),
            revocation_push: req.revocation_push.clone(),
            grant_push: None,
            grant_revoke: None,
        };
        // The EXACT sender construction (Task 8) — sealed to the butler
        // device, signed by the cert-bound enrolled device key.
        let frame = match build_deposit_frame(
            &req.recipient_owner.0,
            &self.sender_owner,
            &self.enrollment_cert_bytes,
            &self.device_signing_key,
            &self.butler_device_vk,
            &payload,
        ) {
            Ok(f) => f,
            Err(e) => return DepositRungOutcome::Failed(e),
        };
        // In-process "dial": owner B's full acceptor pipeline.
        match handle_deposit_core(&frame, self.butler_ctx.as_ref()).await {
            Ok(ack) => {
                // Never mark delivered off a mismatched ack
                // (deposit_to_entry's check, verbatim semantics).
                if ack.space_id != req.space_id.0
                    || ack.message_cid
                        != req
                            .message_cid
                            .as_ref()
                            .expect("message deposit has message_cid")
                            .to_bytes()
                            .to_vec()
                {
                    return DepositRungOutcome::Failed("ack space/cid mismatch".to_string());
                }
                DepositRungOutcome::Acked
            }
            Err(e) => DepositRungOutcome::Failed(format!("{e:?}")),
        }
    }
}

// =====================================================================
// The capstone test
// =====================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sibling_completes_delivery_via_butler_after_originator_stops() {
    assert_eq!(
        DEPOSIT_NOACK_WINDOWS, 2,
        "drain tick cadence below assumes N=2 (10s/16s/27s — the T4 unit \
         test arithmetic); update the windows driven here if the constant \
         changes"
    );

    // ----- Stage 1: owner A fleet (A1 + A2) + identities -----------------
    let owner_a = mint_test_owner(0x51);
    // A2's OWN enrolled-device credentials (distinct device key + its own
    // Master-signed cert under the same owner). The deposit A2 builds in
    // stage 4 must verify against THIS cert — proving the acceptor admits
    // a sibling device's deposit, not just the originator's.
    let (a2_device_sk, a2_cert) = mint_sibling_device(0x51, 0x55);
    assert_ne!(
        a2_cert.device_pubkeys.classical.ed25519_verify,
        owner_a.cert.device_pubkeys.classical.ed25519_verify,
        "sibling cert must bind a DISTINCT enrolled device key"
    );
    let owner_b_addr = OwnerAddr(RECIPIENT_OWNER);
    let (a1_dm_sk, _a1_identity_pub, a1_dm_hash) = dm_identity(0x53);
    let (a2_dm_sk, a2_identity_pub, a2_dm_hash) = dm_identity(0x54);
    let a1_id = device_id_hex(&a1_dm_sk);
    let a2_id = device_id_hex(&a2_dm_sk);
    let b1_sk = SigningKey::from_bytes(&[0x61; 32]);
    let b1_id = device_id_hex(&b1_sk);

    // Same owner KeyTree on A1 + A2, and ONE shared fleet CAS so the two
    // engines can fetch each other's encrypted ROOT blobs (the SP1 bridge
    // requirement). The per-device DM CAS stores below are deliberately
    // SEPARATE (in production all of these are one RuntimeContentStore):
    // keeping A1's and A2's DM CAS apart makes the stage-2 assertion —
    // that the message blob reaches A2's CAS — prove transfer through the
    // outhold ROW rather than through store sharing.
    let kt_a = Arc::new(KeyTree::derive(&[0xA7u8; 32]).expect("kt A"));
    let fleet_cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
    let a1_cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
    let a2_cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());

    let a1_dir = tempfile::tempdir().expect("tempdir A1");
    let a2_dir = tempfile::tempdir().expect("tempdir A2");
    let mut a1 = build_outhold_engine(
        &a1_id,
        Arc::clone(&kt_a),
        Arc::clone(&fleet_cas),
        a1_dir.path(),
    );
    let mut a2 = build_outhold_engine(
        &a2_id,
        Arc::clone(&kt_a),
        Arc::clone(&fleet_cas),
        a2_dir.path(),
    );
    let forwarder = bridge(&mut a1, &mut a2);

    // Per-device OwnerState. A1's holds the DM Space (the device the user
    // sends from); A2's receives the outbox entry via the apply_outbox
    // replication shim below. A2's state is Arc<Mutex<_>> because the
    // production outhold sweep ctx (ProdDmOutholdCtx) locks it internally.
    let mut a1_state = OwnerState::default();
    let a2_state = Arc::new(Mutex::new(OwnerState::default()));
    let space = make_dm_space(0x77, vec![owner_a.owner, owner_b_addr]);
    let space_id = space.id;
    install_space(&mut a1_state, space);

    // ----- Stage 2: A1 sends with the outhold installed ------------------
    let mut a1_outbox = make_owner_outbox(
        &a1_id,
        &owner_a,
        &a1_dm_sk,
        a1_dm_hash,
        0x71,
        &owner_a.device_key,
        owner_a.cert.clone(),
    );
    let a1_notify: Arc<dyn Fn() + Send + Sync> = {
        let engine = Arc::clone(&a1.engine);
        Arc::new(move || engine.notify_dirty())
    };
    a1_outbox.set_outhold(Arc::clone(&a1.doc), a1_notify);

    let (msg_id, message_cid) = a1_outbox
        .send_dm(
            &mut a1_state,
            a1_cas.as_ref(),
            space_id,
            b"butler-held DM body".to_vec(),
            "text/plain".into(),
            1_000,
            None,
        )
        .await
        .expect("send_dm on A1 must succeed");

    let outhold_key = DmOutholdDoc::key(&space_id.0, &message_cid.to_bytes());
    let a1_blob = a1_cas
        .get(&message_cid)
        .await
        .expect("A1 CAS get")
        .expect("send_dm wrote the storage blob to A1's CAS");
    {
        let doc = a1.doc.lock().await;
        let row = doc
            .entries
            .get(&outhold_key)
            .expect("send_dm wrote the outhold row into A1's doc");
        assert_eq!(
            row.storage_blob, a1_blob,
            "outhold row holds the exact CAS storage blob"
        );
        assert_eq!(row.space_id, space_id.0);
    }

    // Fan-out: A1 flush → bridge → A2's doc.
    a1.engine.flush_now().await.expect("flush A1");
    let a2_doc_handle = Arc::clone(&a2.doc);
    let replicated = wait_until(
        || {
            let doc = Arc::clone(&a2_doc_handle);
            let key = outhold_key.clone();
            async move { doc.lock().await.entries.contains_key(&key) }
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(
        replicated,
        "A2 did not receive the outhold row via the bridge within 5s"
    );

    // Owner-state replication shim: apply A1's outbox entry into A2's
    // state the way owner-state sync would (plan: no second engine bridge).
    let entry_clone = a1_state
        .outbox
        .get(&msg_id)
        .expect("entry in A1's outbox")
        .clone();
    {
        let mut state = a2_state.lock().await;
        let outcome = state.apply_outbox(entry_clone);
        assert!(
            matches!(outcome, ApplyOutcome::Inserted),
            "outbox entry must replicate to A2's state, got {outcome:?}"
        );
    }

    // A2's outhold apply sweep (Task 5): Pending outbox entry → blob
    // admitted into A2's LOCAL CAS. This CAS-presence assertion is the
    // deliberate coverage for spec §9's sibling direct-delivery variant
    // (serving the fetch-back from here is ZEB-343/P1-tested machinery).
    let a2_sweep_ctx = ProdDmOutholdCtx {
        crdt_state: Arc::clone(&a2_state),
        content_store: Arc::clone(&a2_cas),
    };
    let a2_engine_notify = {
        let engine = Arc::clone(&a2.engine);
        move || engine.notify_dirty()
    };
    // One orphan-grace tracker for the lifetime of A2's sweeper (the
    // run_dm_outhold_sweeper ownership shape). Unused by these sweeps —
    // the statuses here are Pending then Complete, never None.
    let mut a2_orphan_tracker = HashMap::new();
    let stats = sweep_once(
        &a2.doc,
        &a2_sweep_ctx,
        &a2_engine_notify,
        &mut a2_orphan_tracker,
        5_000,
    )
    .await;
    assert_eq!(stats.admitted, 1, "A2's sweep admits the pending row");
    assert_eq!(stats.gc_removed, 0, "live row is retained");
    let a2_blob = a2_cas
        .get(&message_cid)
        .await
        .expect("A2 CAS get")
        .expect("sweep admitted the blob into A2's CAS");
    assert_eq!(
        a2_blob, a1_blob,
        "A2's CAS copy is byte-identical to A1's original"
    );

    // ----- Stage 3: A1 stops (no more A1 activity) ------------------------
    a1.engine
        .shutdown()
        .await
        .expect("A1 engine shutdown must flush cleanly");
    drop(a1_outbox);
    drop(a1_state);

    // ----- Stage 4: A2 delivers via owner B's butler ----------------------
    let kt_b = Arc::new(KeyTree::derive(&[0xB7u8; 32]).expect("kt B"));
    let b_cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
    let b_dir = tempfile::tempdir().expect("tempdir B");
    let b = build_inbox_engine(&b1_id, kt_b, b_cas, b_dir.path());

    // Admission prerequisites on B: owner A is an Active friend (pinned
    // master from its cert), and A2's DM device identity is cached under
    // owner A (the inner CidNotify resolves the signing device to its
    // owner and binds it to frame.sender_owner).
    let butler_ctx = Arc::new(TestButlerCtx {
        self_owner: RECIPIENT_OWNER,
        device_id: b1_id.clone(),
        friends: [(
            owner_a.owner.0,
            (master_from_cert(&owner_a.cert), FriendStatus::Active),
        )]
        .into(),
        device_owners: [(a2_dm_hash, (owner_a.owner.0, a2_identity_pub))].into(),
        butler_sk: SigningKey::from_bytes(&b1_sk.to_bytes()),
        doc: Arc::clone(&b.doc),
        tracker: Arc::clone(&b.tracker),
        engine: Arc::clone(&b.engine),
    });
    // A2 deposits with its OWN sibling cert + device key (NOT the
    // originator A1's): the acceptor must verify the frame against the
    // sibling's distinct enrollment under owner A's pinned master.
    let cert_bytes = harmony_owner::cbor::to_canonical(&a2_cert).expect("encode sibling cert");
    let client = Arc::new(InProcessButlerClient {
        butler_ctx,
        butler_device_vk: b1_sk.verifying_key().to_bytes(),
        sender_owner: owner_a.owner.0,
        enrollment_cert_bytes: cert_bytes,
        device_signing_key: SigningKey::from_bytes(&a2_device_sk.to_bytes()),
        cas: Arc::clone(&a2_cas),
        requests: StdMutex::new(Vec::new()),
    });

    let mut a2_outbox = make_owner_outbox(
        &a2_id,
        &owner_a,
        &a2_dm_sk,
        a2_dm_hash,
        0x72,
        &a2_device_sk,
        a2_cert.clone(),
    );
    a2_outbox.set_butler_deposit_client(Arc::clone(&client) as Arc<dyn ButlerDepositClient>);

    // StubTransport's default outcome is Ok — exactly the "Ok-enqueued,
    // never acks" cached-but-offline-recipient shape ZEB-422 targets.
    let transport = StubTransport::new();

    // Window 1 (t=10s): Ok send, pre_count=0 → no rung.
    let outcome1 = {
        let mut state = a2_state.lock().await;
        a2_outbox.drain(&mut state, &transport, 10_000).await
    };
    assert!(outcome1.newly_delivered.is_empty());
    // Window 2 (t=16s, +6s past the 5s window): Ok send, pre_count=1 →
    // still below DEPOSIT_NOACK_WINDOWS → no rung.
    let outcome2 = {
        let mut state = a2_state.lock().await;
        a2_outbox.drain(&mut state, &transport, 16_000).await
    };
    assert!(outcome2.newly_delivered.is_empty());
    assert!(
        client.requests().is_empty(),
        "no deposit before DEPOSIT_NOACK_WINDOWS unacked windows"
    );

    // Window 3 (t=27s, +11s past the 10s window): Ok send, pre_count=2 ==
    // DEPOSIT_NOACK_WINDOWS → rung fires → in-process deposit to B.
    let outcome3 = {
        let mut state = a2_state.lock().await;
        a2_outbox.drain(&mut state, &transport, 27_000).await
    };
    assert_eq!(
        transport.sends().len(),
        3,
        "one Ok-enqueued direct send per backoff window, none acked"
    );
    let requests = client.requests();
    assert_eq!(requests.len(), 1, "exactly one deposit attempt");
    assert_eq!(requests[0].entry_id, msg_id);
    assert_eq!(requests[0].recipient_owner, owner_b_addr);
    assert_eq!(requests[0].space_id, space_id);
    assert_eq!(
        requests[0].message_cid,
        Some(message_cid),
        "deposit request carries the held message's CID"
    );

    // B persisted the deposit into its dm-inbox doc (persist-before-ack)
    // with exactly the payload A2 held.
    let inbox_key = DmInboxDoc::key(&space_id.0, &message_cid.to_bytes());
    {
        let doc = b.doc.lock().await;
        let entry = doc
            .entries
            .get(&inbox_key)
            .expect("B's dm-inbox doc contains the deposited entry");
        assert_eq!(entry.sender_owner, owner_a.owner.0);
        assert_eq!(
            entry.storage_blob, a1_blob,
            "B holds the exact blob A1 originally encrypted"
        );
        assert_eq!(entry.deposited_by, b1_id);
        assert!(entry.ingested_by.is_empty());
    }

    // The butler ack marked the recipient delivered through the existing
    // idempotent mark_ack_delivered path (dm-delivered emit contract).
    assert_eq!(
        outcome3.newly_delivered,
        vec![(space_id, message_cid, owner_b_addr)],
        "deposit ack must surface in newly_delivered"
    );
    {
        let state = a2_state.lock().await;
        let entry = state.outbox.get(&msg_id).expect("entry in A2's outbox");
        assert!(entry.delivered_to.contains(&owner_b_addr));
        assert!(
            matches!(entry.delivery_status, DeliveryStatus::Complete),
            "1:1 DM with its sole recipient acked via butler → Complete, got {:?}",
            entry.delivery_status
        );
    }

    // ----- Stage 5: GC — the Complete row is removed and published --------
    let gc_notify_count = Arc::new(AtomicUsize::new(0));
    let gc_notify = {
        let count = Arc::clone(&gc_notify_count);
        let engine = Arc::clone(&a2.engine);
        move || {
            count.fetch_add(1, Ordering::SeqCst);
            engine.notify_dirty();
        }
    };
    let stats = sweep_once(
        &a2.doc,
        &a2_sweep_ctx,
        &gc_notify,
        &mut a2_orphan_tracker,
        30_000,
    )
    .await;
    assert_eq!(stats.gc_removed, 1, "Complete outbox status GCs the row");
    assert_eq!(stats.admitted, 0, "no re-admit during the GC sweep");
    assert!(
        !a2.doc.lock().await.entries.contains_key(&outhold_key),
        "outhold row removed from A2's doc once delivery completed"
    );
    assert_eq!(
        gc_notify_count.load(Ordering::SeqCst),
        1,
        "notify_dirty fired for the removal (the deletion would publish)"
    );

    let _ = a2.engine.shutdown().await;
    let _ = b.engine.shutdown().await;
    forwarder.abort();
}

/// Cheap focused variant of stage 2: the replicated outhold row carries
/// the storage blob byte-intact — A2's doc row equals A1's CAS bytes for
/// the message CID.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outhold_row_replicates_with_blob_intact() {
    let owner_a = mint_test_owner(0x51);
    let owner_b_addr = OwnerAddr(RECIPIENT_OWNER);
    let (a1_dm_sk, _a1_identity_pub, a1_dm_hash) = dm_identity(0x53);
    let (a2_dm_sk, _a2_identity_pub, _a2_dm_hash) = dm_identity(0x54);
    let a1_id = device_id_hex(&a1_dm_sk);
    let a2_id = device_id_hex(&a2_dm_sk);

    let kt_a = Arc::new(KeyTree::derive(&[0xA7u8; 32]).expect("kt A"));
    let fleet_cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
    let a1_cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());

    let a1_dir = tempfile::tempdir().expect("tempdir A1");
    let a2_dir = tempfile::tempdir().expect("tempdir A2");
    let mut a1 = build_outhold_engine(
        &a1_id,
        Arc::clone(&kt_a),
        Arc::clone(&fleet_cas),
        a1_dir.path(),
    );
    let mut a2 = build_outhold_engine(
        &a2_id,
        Arc::clone(&kt_a),
        Arc::clone(&fleet_cas),
        a2_dir.path(),
    );
    let forwarder = bridge(&mut a1, &mut a2);

    let mut a1_state = OwnerState::default();
    let space = make_dm_space(0x77, vec![owner_a.owner, owner_b_addr]);
    let space_id = space.id;
    install_space(&mut a1_state, space);

    let mut a1_outbox = make_owner_outbox(
        &a1_id,
        &owner_a,
        &a1_dm_sk,
        a1_dm_hash,
        0x71,
        &owner_a.device_key,
        owner_a.cert.clone(),
    );
    let a1_notify: Arc<dyn Fn() + Send + Sync> = {
        let engine = Arc::clone(&a1.engine);
        Arc::new(move || engine.notify_dirty())
    };
    a1_outbox.set_outhold(Arc::clone(&a1.doc), a1_notify);

    let (_msg_id, message_cid) = a1_outbox
        .send_dm(
            &mut a1_state,
            a1_cas.as_ref(),
            space_id,
            b"replicate me byte-for-byte".to_vec(),
            "text/plain".into(),
            1_000,
            None,
        )
        .await
        .expect("send_dm on A1 must succeed");
    a1.engine.flush_now().await.expect("flush A1");

    let outhold_key = DmOutholdDoc::key(&space_id.0, &message_cid.to_bytes());
    let a2_doc_handle = Arc::clone(&a2.doc);
    let replicated = wait_until(
        || {
            let doc = Arc::clone(&a2_doc_handle);
            let key = outhold_key.clone();
            async move { doc.lock().await.entries.contains_key(&key) }
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(
        replicated,
        "A2 did not receive the outhold row via the bridge within 5s"
    );

    let a1_blob = a1_cas
        .get(&message_cid)
        .await
        .expect("A1 CAS get")
        .expect("send_dm wrote the storage blob to A1's CAS");
    {
        let doc = a2.doc.lock().await;
        let row = doc.entries.get(&outhold_key).expect("row on A2");
        assert_eq!(
            row.storage_blob, a1_blob,
            "replicated row's blob must be byte-equal to A1's CAS bytes"
        );
        assert_eq!(row.space_id, space_id.0);
    }

    let _ = a1.engine.shutdown().await;
    let _ = a2.engine.shutdown().await;
    forwarder.abort();
}
