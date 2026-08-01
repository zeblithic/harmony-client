//! ZEB-668 S3: community retire-announce — deposit side.
//!
//! Level-triggered sweeper that diffs the owner's trust state
//! (`harmony_owner::state::OwnerState.revocations`, replicated by the S1
//! trust engine) against the community-device-intro fleet dataset and
//! deposits ONE signed `DeviceRetire` membership event per (depositable
//! community × revoked device) under `CommunityDeviceIntroDoc::retire_key`.
//! The existing intro relay sweeper (`community_device_intro_ingest`) then
//! drives each entry into its community engine exactly like an intro —
//! grow-only `relayed_by`, coverage/TTL GC unchanged.
//!
//! Level-triggered (state diff, not edge events) so it is restart-proof:
//! one startup pass plus one debounced pass per nudge burst. Nudge sources:
//! the trust engine's `on_applied` (a sibling's or remote's revocation
//! replicated in) and local `revoke_device` completion (local writes bypass
//! `on_applied` — it fires on REMOTE merges only).
//!
//! Depositable community = engine spawned + owner Joined + THIS device's
//! key enrolled there (the preconditions for the deposited event to verify
//! at receivers). Deliberately NO "retired key enrolled there"
//! precondition: that check would race an in-flight `DeviceAnnounce`;
//! deposit-always is safe because the receiver side is remove-wins
//! (tombstones), and a retire for a never-announced key materializes as a
//! tombstone-only no-op.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use harmony_owner::certs::{EnrollmentCert, RevocationCert};

use crate::community_device_intro_crdt::{CommunityDeviceIntroDoc, CommunityDeviceIntroEntry};
use crate::community_membership::{EventPayload, MembershipEventKind};
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

/// Debounce window between a nudge and the sweep it triggers — coalesces a
/// burst (e.g. a trust merge folding several certs) into one sweep. Mirrors
/// `RELAY_SWEEP_DEBOUNCE_MS`.
pub const RETIRE_DEPOSIT_DEBOUNCE_MS: u64 = 250;

/// Injectable context for [`deposit_pending_retires`]: the trust-state
/// revocation pairs, the communities this device can author into, HLC
/// reservation, and event signing. Production implements this over the
/// trust doc + `CommunitySyncRegistry`; tests implement it with a probe.
#[async_trait]
pub trait CommunityDeviceRetireDepositCtx: Send + Sync {
    /// (revocation, retired-device enrollment) pairs from the owner's trust
    /// state. Pairs whose enrollment record is missing are skipped by the
    /// provider — without the cert there is nothing that binds the 16-byte
    /// revocation target to the 32-byte key communities store; the
    /// level-triggered sweep retries once the enrollment replicates in.
    async fn revoked_pairs(&self) -> Vec<(RevocationCert, EnrollmentCert)>;

    /// Communities where THIS device can author a verifiable `DeviceRetire`:
    /// engine spawned, owner Joined, self key enrolled.
    async fn depositable_communities(&self) -> Vec<SpaceId>;

    /// Reserve the next HLC for this device (monotonic per device).
    async fn next_hlc(&self) -> Hlc;

    /// Sign `payload` with this device's membership signing key and return
    /// the canonical-CBOR bytes of the signed event.
    fn sign_and_encode(&self, payload: &EventPayload) -> Result<Vec<u8>, String>;

    /// This owner's address (event actor).
    fn self_owner(&self) -> OwnerAddr;
}

/// One deposit pass. Returns `true` when at least one entry was inserted —
/// the caller must then `notify_dirty()` on the dataset engine so the
/// deposit replicates to siblings (and the relay sweeper is nudged via the
/// engine's own `on_applied` echo on THEIR side; locally the caller's
/// relay-sweeper nudge fires through the same engine).
pub async fn deposit_pending_retires(
    doc: &Arc<tokio::sync::Mutex<CommunityDeviceIntroDoc>>,
    ctx: &dyn CommunityDeviceRetireDepositCtx,
) -> bool {
    let pairs = ctx.revoked_pairs().await;
    if pairs.is_empty() {
        return false;
    }
    let communities = ctx.depositable_communities().await;
    if communities.is_empty() {
        return false;
    }
    let mut changed = false;
    for community_id in communities {
        for (rc, ec) in &pairs {
            let retired_vk_hex = hex::encode(ec.device_pubkeys.classical.ed25519_verify);
            let key = CommunityDeviceIntroDoc::retire_key(&community_id, &retired_vk_hex);
            {
                let g = doc.lock().await;
                if g.entries.contains_key(&key) {
                    continue;
                }
            }
            let hlc = ctx.next_hlc().await;
            let event_id: [u8; 16] = {
                use rand::RngCore;
                let mut buf = [0u8; 16];
                rand::thread_rng().fill_bytes(&mut buf);
                buf
            };
            let payload = EventPayload {
                id: event_id,
                community_id,
                kind: MembershipEventKind::DeviceRetire {
                    revocation: rc.clone(),
                    enrollment: Box::new(ec.clone()),
                },
                actor: ctx.self_owner(),
                at: hlc.clone(),
            };
            match ctx.sign_and_encode(&payload) {
                Ok(bytes) => {
                    let mut g = doc.lock().await;
                    // Insert-once under the write lock — a sibling's deposit
                    // may have merged in between our peek and now. `changed`
                    // and the log fire only on a REAL insert (Qodo/CodeRabbit
                    // PR #453): a lost race is not a deposit and must not
                    // trigger notify_dirty.
                    if let std::collections::btree_map::Entry::Vacant(slot) = g.entries.entry(key) {
                        slot.insert(CommunityDeviceIntroEntry {
                            signed_event: bytes,
                            community_id,
                            deposited_at: hlc,
                            relayed_by: BTreeSet::new(),
                        });
                        changed = true;
                        tracing::info!(
                            community_id = ?community_id,
                            retired = %retired_vk_hex,
                            "ZEB-668 S3: DeviceRetire deposited for relay"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        community_id = ?community_id,
                        error = %e,
                        "ZEB-668 S3: sign/encode of DeviceRetire failed; will retry on next nudge"
                    );
                }
            }
        }
    }
    changed
}

/// Non-blocking level-trigger nudge for the deposit sweeper (same shape as
/// `relay_nudge_on_applied`): a full buffer means a sweep is already
/// pending — dropping the extra nudge is correct.
pub fn retire_deposit_nudge(
    nudge_tx: tokio::sync::mpsc::Sender<()>,
) -> Arc<dyn Fn() + Send + Sync> {
    Arc::new(move || {
        let _ = nudge_tx.try_send(());
    })
}

/// The deposit-sweeper task: one startup pass (revocations that replicated
/// or were minted while this device was offline), then one debounced pass
/// per nudge burst. Exits when every nudge sender is dropped. Mirrors
/// `run_community_device_intro_sweeper`.
pub async fn run_community_device_retire_deposit_task(
    doc: Arc<tokio::sync::Mutex<CommunityDeviceIntroDoc>>,
    ctx: Arc<dyn CommunityDeviceRetireDepositCtx>,
    mut nudge_rx: tokio::sync::mpsc::Receiver<()>,
    notify_dirty: Arc<dyn Fn() + Send + Sync>,
    debounce: std::time::Duration,
) {
    if deposit_pending_retires(&doc, ctx.as_ref()).await {
        notify_dirty();
    }
    while nudge_rx.recv().await.is_some() {
        tokio::time::sleep(debounce).await;
        // Drain the burst — one sweep covers every nudge received meanwhile.
        while nudge_rx.try_recv().is_ok() {}
        if deposit_pending_retires(&doc, ctx.as_ref()).await {
            notify_dirty();
        }
    }
}

/// Production [`CommunityDeviceRetireDepositCtx`] over real `start_node`
/// handles: the S1 trust doc, the community-sync registry, and this
/// device's membership signing identity.
pub struct ProdCommunityDeviceRetireDepositCtx {
    /// The owner trust doc (harmony-owner `OwnerState`), shared with the
    /// S1 trust sync engine.
    pub trust_doc: Arc<tokio::sync::Mutex<harmony_owner::state::OwnerState>>,
    /// The community-sync registry (engine lookup + enumeration).
    pub registry: Arc<crate::community_state_sync::CommunitySyncRegistry>,
    /// This device's membership signing key.
    pub signing_key: Arc<ed25519_dalek::SigningKey>,
    /// This device's ed25519 verify key (the `enrolled_device_keys` form).
    pub self_vk: [u8; 32],
    pub self_owner: OwnerAddr,
    /// 64-hex device id for HLC reservation.
    pub device_id: String,
    /// Shared per-device HLC replay tracker (same one the intro deposit
    /// uses, so retire HLCs are monotonic with every other event this
    /// device stamps).
    pub hlc_tracker: Arc<tokio::sync::Mutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>>,
    /// ZEB-790: node-wide bounded-adoption floor (see `hlc_adopt_floor` module
    /// docs).
    pub adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor,
}

#[async_trait]
impl CommunityDeviceRetireDepositCtx for ProdCommunityDeviceRetireDepositCtx {
    async fn revoked_pairs(&self) -> Vec<(RevocationCert, EnrollmentCert)> {
        let g = self.trust_doc.lock().await;
        g.revocations
            .iter()
            .filter_map(|rc| {
                // Enrollment rows are never deleted on revoke (S2), so the
                // cert is normally present. A revocation whose enrollment
                // hasn't replicated yet is skipped — the level-triggered
                // sweep retries on the next nudge once it merges in.
                g.enrollments
                    .get(&rc.target)
                    .map(|ec| (rc.clone(), ec.clone()))
            })
            .collect()
    }

    async fn depositable_communities(&self) -> Vec<SpaceId> {
        let ids = self.registry.known_ids().await;
        let mut out = Vec::new();
        for id in ids {
            // Re-resolve per id — an engine may despawn between enumerate
            // and use; a missing engine simply isn't depositable this pass.
            let Some(engine) = self.registry.engine_arc(&id).await else {
                continue;
            };
            let state_arc = engine.state();
            let st = state_arc.lock().await;
            let mat = st.materialized(engine.admin_addr());
            if let Some(m) = mat.members.get(&self.self_owner) {
                if m.status == crate::community_membership::MemberStatus::Joined
                    && m.enrolled_device_keys.contains(&self.self_vk)
                {
                    out.push(id);
                }
            }
        }
        out
    }

    async fn next_hlc(&self) -> Hlc {
        let wall_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        crate::dm_outbox::reserve_next_hlc_for_device(
            &self.hlc_tracker,
            &self.adopt_floor,
            &self.device_id,
            wall_ms,
        )
        .await
    }

    fn sign_and_encode(&self, payload: &EventPayload) -> Result<Vec<u8>, String> {
        let signed = crate::community_membership::sign_event(payload, self.signing_key.as_ref())
            .map_err(|e| format!("sign DeviceRetire: {e:?}"))?;
        crate::owner_state_crypto::canonical_cbor_encode(&signed)
            .map_err(|e| format!("encode DeviceRetire: {e:?}"))
    }

    fn self_owner(&self) -> OwnerAddr {
        self.self_owner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_membership::{mint_test_owner, SignedMembershipEvent, TestOwner};
    use harmony_owner::certs::RevocationReason;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    /// Probe ctx: canned pairs/communities behind mutexes (mutable
    /// mid-test), deterministic HLC, real signing with a fixture owner.
    struct ProbeCtx {
        owner: TestOwner,
        pairs: StdMutex<Vec<(RevocationCert, EnrollmentCert)>>,
        communities: StdMutex<Vec<SpaceId>>,
        wall: AtomicU64,
        sign_calls: AtomicUsize,
        fail_sign: bool,
    }

    impl ProbeCtx {
        fn new(owner: TestOwner) -> Self {
            Self {
                owner,
                pairs: StdMutex::new(Vec::new()),
                communities: StdMutex::new(Vec::new()),
                wall: AtomicU64::new(1_000),
                sign_calls: AtomicUsize::new(0),
                fail_sign: false,
            }
        }
    }

    #[async_trait]
    impl CommunityDeviceRetireDepositCtx for ProbeCtx {
        async fn revoked_pairs(&self) -> Vec<(RevocationCert, EnrollmentCert)> {
            self.pairs.lock().unwrap().clone()
        }
        async fn depositable_communities(&self) -> Vec<SpaceId> {
            self.communities.lock().unwrap().clone()
        }
        async fn next_hlc(&self) -> Hlc {
            Hlc {
                wall_ms: self.wall.fetch_add(1, Ordering::SeqCst),
                logical: 0,
                device_id: "device1".into(),
            }
        }
        fn sign_and_encode(&self, payload: &EventPayload) -> Result<Vec<u8>, String> {
            self.sign_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_sign {
                return Err("probe sign failure".into());
            }
            let signed = crate::community_membership::sign_event(payload, &self.owner.device_key)
                .map_err(|e| format!("{e:?}"))?;
            crate::owner_state_crypto::canonical_cbor_encode(&signed).map_err(|e| format!("{e:?}"))
        }
        fn self_owner(&self) -> OwnerAddr {
            self.owner.owner
        }
    }

    /// A (revocation, enrollment) pair for a second device of `owner`'s
    /// master (`master_seed` must match the owner's mint seed).
    fn revoked_pair(master_seed: u8, device_seed: u8) -> (RevocationCert, EnrollmentCert) {
        use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};
        let master_sk = ed25519_dalek::SigningKey::from_bytes(&[master_seed; 32]);
        let master_bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: master_sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let device_sk = ed25519_dalek::SigningKey::from_bytes(&[device_seed; 32]);
        let device_bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: device_sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let device_id = device_bundle.identity_hash();
        let ec = EnrollmentCert::sign_master(
            &master_sk,
            master_bundle.clone(),
            device_id,
            device_bundle,
            1_700_000_000,
            None,
        )
        .expect("sign_master enrollment");
        let rc = RevocationCert::sign_master(
            &master_sk,
            master_bundle,
            device_id,
            1_700_000_100,
            RevocationReason::Lost,
        )
        .expect("sign_master revocation");
        (rc, ec)
    }

    fn fresh_doc() -> Arc<tokio::sync::Mutex<CommunityDeviceIntroDoc>> {
        Arc::new(tokio::sync::Mutex::new(CommunityDeviceIntroDoc::default()))
    }

    #[tokio::test]
    async fn deposits_one_entry_per_community_times_revocation() {
        let owner = mint_test_owner(0x85);
        let mut ctx = ProbeCtx::new(owner);
        let (rc, ec) = revoked_pair(0x85, 0x86);
        let retired_vk_hex = hex::encode(ec.device_pubkeys.classical.ed25519_verify);
        *ctx.pairs.lock().unwrap() = vec![(rc.clone(), ec.clone())];
        let c1 = SpaceId([0x01; 16]);
        let c2 = SpaceId([0x02; 16]);
        *ctx.communities.lock().unwrap() = vec![c1, c2];
        ctx.fail_sign = false;

        let doc = fresh_doc();
        assert!(deposit_pending_retires(&doc, &ctx).await, "changed");

        let g = doc.lock().await;
        assert_eq!(g.entries.len(), 2, "one entry per community");
        for community in [c1, c2] {
            let key = CommunityDeviceIntroDoc::retire_key(&community, &retired_vk_hex);
            let entry = g.entries.get(&key).expect("retire entry present");
            assert_eq!(entry.community_id, community);
            assert!(entry.relayed_by.is_empty(), "no relay acks yet");
            // The deposited bytes decode to a DeviceRetire carrying the
            // exact cert pair, actored by the owner.
            let signed: SignedMembershipEvent =
                crate::owner_state_crypto::canonical_cbor_decode(&entry.signed_event)
                    .expect("decode deposited event");
            assert_eq!(signed.actor, ctx.owner.owner);
            match &signed.kind {
                MembershipEventKind::DeviceRetire {
                    revocation,
                    enrollment,
                } => {
                    assert_eq!(revocation, &rc);
                    assert_eq!(enrollment.as_ref(), &ec);
                }
                other => panic!("expected DeviceRetire, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn no_revocations_is_a_cheap_noop() {
        let owner = mint_test_owner(0x87);
        let ctx = ProbeCtx::new(owner);
        *ctx.communities.lock().unwrap() = vec![SpaceId([0x03; 16])];
        let doc = fresh_doc();
        assert!(!deposit_pending_retires(&doc, &ctx).await);
        assert!(doc.lock().await.entries.is_empty());
        assert_eq!(
            ctx.sign_calls.load(Ordering::SeqCst),
            0,
            "no signing work when there is nothing to retire"
        );
    }

    #[tokio::test]
    async fn existing_entry_is_not_redeposited() {
        let owner = mint_test_owner(0x88);
        let ctx = ProbeCtx::new(owner);
        let (rc, ec) = revoked_pair(0x88, 0x89);
        let retired_vk_hex = hex::encode(ec.device_pubkeys.classical.ed25519_verify);
        *ctx.pairs.lock().unwrap() = vec![(rc, ec)];
        let c1 = SpaceId([0x04; 16]);
        let c2 = SpaceId([0x05; 16]);
        *ctx.communities.lock().unwrap() = vec![c1, c2];

        let doc = fresh_doc();
        // Pre-seed c1's retire entry (as if a sibling already deposited it).
        {
            let mut g = doc.lock().await;
            g.entries.insert(
                CommunityDeviceIntroDoc::retire_key(&c1, &retired_vk_hex),
                CommunityDeviceIntroEntry {
                    signed_event: vec![0xEE],
                    community_id: c1,
                    deposited_at: Hlc {
                        wall_ms: 5,
                        logical: 0,
                        device_id: "sib".into(),
                    },
                    relayed_by: BTreeSet::new(),
                },
            );
        }

        assert!(deposit_pending_retires(&doc, &ctx).await, "c2 deposited");
        {
            let g = doc.lock().await;
            assert_eq!(g.entries.len(), 2);
            let seeded = g
                .entries
                .get(&CommunityDeviceIntroDoc::retire_key(&c1, &retired_vk_hex))
                .unwrap();
            assert_eq!(
                seeded.signed_event,
                vec![0xEE],
                "pre-seeded entry untouched (insert-once)"
            );
        }
        // Second sweep: everything present → no change.
        assert!(!deposit_pending_retires(&doc, &ctx).await, "idempotent");
    }

    #[tokio::test]
    async fn sign_failure_leaves_entry_missing_for_retry() {
        let owner = mint_test_owner(0x8a);
        let mut ctx = ProbeCtx::new(owner);
        let (rc, ec) = revoked_pair(0x8a, 0x8b);
        *ctx.pairs.lock().unwrap() = vec![(rc, ec)];
        *ctx.communities.lock().unwrap() = vec![SpaceId([0x06; 16])];
        ctx.fail_sign = true;

        let doc = fresh_doc();
        assert!(
            !deposit_pending_retires(&doc, &ctx).await,
            "nothing deposited on sign failure"
        );
        assert!(
            doc.lock().await.entries.is_empty(),
            "entry left missing so the next nudge retries"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn startup_pass_deposits_then_nudge_drives_followup() {
        let owner = mint_test_owner(0x8c);
        let ctx = Arc::new(ProbeCtx::new(owner));
        // Startup: no revocations yet.
        *ctx.communities.lock().unwrap() = vec![SpaceId([0x07; 16])];

        let doc = fresh_doc();
        let (nudge_tx, nudge_rx) = tokio::sync::mpsc::channel::<()>(1);
        let notified = Arc::new(AtomicUsize::new(0));
        let notified_probe = Arc::clone(&notified);
        let task = tokio::spawn(run_community_device_retire_deposit_task(
            Arc::clone(&doc),
            ctx.clone() as Arc<dyn CommunityDeviceRetireDepositCtx>,
            nudge_rx,
            Arc::new(move || {
                notified_probe.fetch_add(1, Ordering::SeqCst);
            }),
            std::time::Duration::from_millis(RETIRE_DEPOSIT_DEBOUNCE_MS),
        ));

        // Let the startup pass run: nothing to deposit, no notify.
        tokio::task::yield_now().await;
        assert_eq!(notified.load(Ordering::SeqCst), 0, "empty startup pass");

        // A revocation replicates in; the trust engine nudges us.
        let (rc, ec) = revoked_pair(0x8c, 0x8d);
        *ctx.pairs.lock().unwrap() = vec![(rc, ec)];
        retire_deposit_nudge(nudge_tx.clone())();

        // Advance through the debounce; the sweep deposits + notifies.
        loop {
            tokio::time::advance(std::time::Duration::from_millis(50)).await;
            tokio::task::yield_now().await;
            if notified.load(Ordering::SeqCst) > 0 {
                break;
            }
        }
        assert_eq!(doc.lock().await.entries.len(), 1, "retire deposited");
        assert_eq!(notified.load(Ordering::SeqCst), 1);

        // Dropping every sender ends the task.
        drop(nudge_tx);
        task.await.expect("task exits when senders drop");
    }
}
