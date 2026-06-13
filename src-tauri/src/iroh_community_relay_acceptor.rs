//! ZEB-458 Phase A Task 3: community relay deposit acceptor — admission +
//! opaque persist-with-caps.
//!
//! A relay that holds sealed DMs OPAQUE: the relay verifies co-membership,
//! cert chain, and frame signature, but **never opens** `sealed_blob` (which
//! is sealed to the *recipient's* device key, not the relay's). The blob is
//! stored verbatim and later pulled by the recipient.
//!
//! ## Verification order (spec D36 — cheap/local checks first; uniform reject
//! = no oracle)
//!
//! 0. `serves_community(frame.community_id)` — this relay opted in and is a
//!    Joined member of the named community; else [`RelayDepositReject::WrongCommunity`].
//! 1. `both_co_members(community_id, sender_owner, recipient_owner)` — both
//!    are Joined members of that community in the relay's local replicated
//!    membership state; else [`RelayDepositReject::NotCoMember`].
//! 2. Decode + verify the sender's `EnrollmentCert`; extract Master issuer
//!    pubkey; require `cert.owner_id == frame.sender_owner`; verify
//!    owner-id-derived anchor `owner_id_from_master_ed25519(cert_master) ==
//!    OwnerAddr(frame.sender_owner)`; else [`RelayDepositReject::BadCert`].
//!    (Mirrors the butler co-member branch: there is NO friend-graph pin here;
//!    the derived anchor is the trust anchor, exactly as ZEB-424 D29.1.)
//! 3. Verify `frame.sig` over `relay_deposit_sig_payload(recipient_owner,
//!    community_id, sealed_blob)` against the cert-bound device key; else
//!    [`RelayDepositReject::BadSig`].
//! 4. **NO DECRYPT.** The relay NEVER opens `sealed_blob`. Compute
//!    `content_id = ContentId::for_book(&sealed_blob, ContentFlags{encrypted:true,..})`,
//!    build a [`RelayHoldEntry`], persist via [`RelayDepositCtx::persist_hold`],
//!    return [`RelayDepositAck`].
//!
//! The cert/master-anchor step mirrors `iroh_butler_acceptor`'s `CoMember`
//! branch verbatim: both paths derive the trust anchor from
//! `owner_id_from_master_ed25519(cert_master)` and compare it to
//! `OwnerAddr(frame.sender_owner)`. The relay ONLY uses this branch — there
//! is no friend-graph on a relay; any sender must be a co-member.

use std::collections::BTreeSet;

use async_trait::async_trait;
use ed25519_dalek::{Signature, VerifyingKey};
use harmony_content::cid::{ContentFlags, ContentId};
use harmony_owner::certs::{EnrollmentCert, EnrollmentIssuer};

use crate::community_relay::{relay_deposit_sig_payload, RelayDepositAck, RelayDepositFrame};
use crate::community_relay_hold_crdt::{RelayHoldDoc, RelayHoldEntry};
use crate::owner_state_types::{Hlc, SpaceId};

// =====================================================================
// Outcome and reject types
// =====================================================================

/// Outcome of the atomic persist step. Mirrors [`crate::iroh_butler_acceptor::DepositPersistVerdict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayPersistVerdict {
    Inserted,
    /// Key already present — re-acked idempotently (D7: a redelivery after a
    /// failed first flush must not ack non-durable state).
    Duplicate,
    /// Inserting a NEW key would exceed [`RELAY_HOLD_PER_SENDER_CAP`] or
    /// [`RELAY_HOLD_GLOBAL_CAP`]. Nothing inserted, nothing flushed.
    CapExceeded,
}

/// Why a relay deposit was rejected. The wire NEVER carries a detailed error
/// back to the sender (uniform reject = no oracle for probing membership);
/// this enum is for local logging/counters/tests only.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RelayDepositReject {
    /// This relay does not serve the named community (not a Joined member or
    /// not opted-in as a relay for it).
    #[error("relay does not serve this community")]
    WrongCommunity,
    /// Sender or recipient (or both) are not Joined members of the community.
    #[error("sender or recipient is not a co-member of the community")]
    NotCoMember,
    /// The embedded `EnrollmentCert` failed to decode, failed verification,
    /// is not Master-issued, has `owner_id != frame.sender_owner`, or its
    /// master key does not match the owner-id-derived anchor.
    #[error("sender enrollment cert invalid")]
    BadCert,
    /// `frame.sig` is malformed or does not verify over
    /// `COMMUNITY_RELAY_DEPOSIT_SIG_DOMAIN ‖ recipient_owner ‖ community_id ‖ sealed_blob`
    /// against the cert-bound device key.
    #[error("deposit frame signature invalid")]
    BadSig,
    /// Inserting a NEW relay-hold key would exceed [`RELAY_HOLD_PER_SENDER_CAP`]
    /// or [`RELAY_HOLD_GLOBAL_CAP`]. Enforced atomically inside
    /// `persist_hold`'s critical section; a redelivery of an already-stored key
    /// is exempt.
    #[error("relay hold cap exceeded")]
    CapExceeded,
    /// The relay-hold write or its durable flush failed — NO ack may be
    /// produced (an ack never lies, D7). The sender retries; the redelivery is
    /// absorbed by the insert-once key dedupe.
    #[error("relay hold persist failed: {0}")]
    PersistFailed(String),
}

// =====================================================================
// Injectable context trait
// =====================================================================

/// Injectable context for [`handle_relay_deposit_core`]: community membership
/// checks and the persist sink (which also enforces caps atomically).
/// Production implements this over `NodeState`'s community state; tests
/// implement it with probes that record call order.
#[async_trait]
pub trait RelayDepositCtx: Send + Sync {
    /// This relay device's id (64-hex of the device ed25519 verify key),
    /// stamped as `held_by`.
    fn relay_device_id(&self) -> String;

    /// Opt-in + membership check: this relay is a Joined member of
    /// `community_id` AND has opted in to relay for it. Cheapest local check
    /// (community membership is replicated; the opt-in is a local setting).
    async fn serves_community(&self, community_id: &SpaceId) -> bool;

    /// Both `sender_owner` and `recipient_owner` are Joined members of
    /// `community_id` in the relay's local replicated C-membership state.
    async fn both_co_members(
        &self,
        community_id: &SpaceId,
        sender_owner: &[u8; 16],
        recipient_owner: &[u8; 16],
    ) -> bool;

    /// Wall-clock now in epoch-SECONDS for `EnrollmentCert` expiry checks.
    fn now_secs(&self) -> u64;

    /// Mint a fresh monotone HLC for `held_at`.
    async fn mint_hlc(&self) -> Hlc;

    /// Atomic persist-with-caps over [`RelayHoldDoc`] (mirrors
    /// [`crate::iroh_butler_acceptor::ButlerDepositCtx::persist_entry`]):
    ///
    /// - Occupied `key` → [`RelayPersistVerdict::Duplicate`] (caps bypassed,
    ///   entry already stored — idempotent redelivery);
    /// - Vacant `key`, within caps → insert + durable flush →
    ///   [`RelayPersistVerdict::Inserted`];
    /// - Vacant `key`, over caps → [`RelayPersistVerdict::CapExceeded`],
    ///   nothing inserted or flushed;
    /// - I/O failure → `Err(String)`, nothing durable may be assumed.
    ///
    /// The `key` is built by the caller via
    /// `RelayHoldDoc::key(&recipient_owner, &content_id.to_bytes())`.
    async fn persist_hold(
        &self,
        key: String,
        entry: RelayHoldEntry,
    ) -> Result<RelayPersistVerdict, String>;
}

// =====================================================================
// Strict cert decode helper (mirrors iroh_butler_acceptor)
// =====================================================================

/// Strict canonical-CBOR decode of the embedded [`EnrollmentCert`]; trailing
/// bytes rejected (mirrors `iroh_butler_acceptor::decode_enrollment_cert_strict`).
fn decode_enrollment_cert_strict(bytes: &[u8]) -> Result<EnrollmentCert, RelayDepositReject> {
    let mut cursor = std::io::Cursor::new(bytes);
    let cert: EnrollmentCert =
        ciborium::from_reader(&mut cursor).map_err(|_| RelayDepositReject::BadCert)?;
    if cursor.position() as usize != bytes.len() {
        return Err(RelayDepositReject::BadCert);
    }
    Ok(cert)
}

// =====================================================================
// Core pipeline
// =====================================================================

/// The Tauri-free relay deposit pipeline (spec D36 order — see module docs).
///
/// Returns the ack to write on success; any reject means the shell closes
/// the stream without detail (no oracle).
pub async fn handle_relay_deposit_core(
    frame: &RelayDepositFrame,
    ctx: &dyn RelayDepositCtx,
) -> Result<RelayDepositAck, RelayDepositReject> {
    // Step 0 — opt-in + relay membership: this relay serves the named
    // community. Cheapest local check, before any peer-state lookup or crypto.
    if !ctx.serves_community(&frame.community_id).await {
        return Err(RelayDepositReject::WrongCommunity);
    }

    // Step 1 — co-membership admission: both the sender and the recipient
    // must be Joined members of the community in the relay's local replicated
    // membership state. This is an O(members) local scan — no crypto yet.
    if !ctx
        .both_co_members(
            &frame.community_id,
            &frame.sender_owner,
            &frame.recipient_owner,
        )
        .await
    {
        return Err(RelayDepositReject::NotCoMember);
    }

    // Step 2 — decode + verify the sender device's EnrollmentCert and bind
    // its issuing master to the admitted identity via the owner-id-derived
    // anchor (D29.1 co-member branch — there is NO friend-graph pin on a
    // relay; the derived anchor IS the trust anchor):
    //
    //   cert decode → Master-issued → `cert.verify(now_secs())` →
    //   `cert.owner_id == frame.sender_owner` →
    //   `owner_id_from_master_ed25519(cert_master) == OwnerAddr(sender_owner)`
    //
    // The last check is defense-in-depth: `cert.verify()` already rejects
    // `hash(master) != owner_id`, and we've required `cert.owner_id ==
    // sender_owner`, so a well-formed cert reaching here necessarily satisfies
    // the derived check. We keep it explicit for clarity and resilience.
    let cert = decode_enrollment_cert_strict(&frame.sender_enrollment_cert)?;
    cert.verify(ctx.now_secs())
        .map_err(|_| RelayDepositReject::BadCert)?;
    let cert_master = match &cert.issuer {
        EnrollmentIssuer::Master { master_pubkey } => master_pubkey.classical.ed25519_verify,
        // Non-Master issuers (Quorum certs) cannot be verified without an
        // OwnerState walk-back; reject outright, mirroring the butler acceptor.
        _ => return Err(RelayDepositReject::BadCert),
    };
    if cert.owner_id != frame.sender_owner {
        return Err(RelayDepositReject::BadCert);
    }
    // Owner-id-derived anchor (D29.1 co-member branch, identical to the
    // butler's CoMember arm in iroh_butler_acceptor):
    if crate::friend_graph::owner_id_from_master_ed25519(&cert_master)
        != crate::owner_state_types::OwnerAddr(frame.sender_owner)
    {
        return Err(RelayDepositReject::BadCert);
    }
    let device_vk_bytes = cert.device_pubkeys.classical.ed25519_verify;

    // Step 3 — verify the frame signature over
    // `COMMUNITY_RELAY_DEPOSIT_SIG_DOMAIN ‖ recipient_owner ‖ community_id ‖ sealed_blob`
    // against the cert-bound device key.
    let sig_bytes: [u8; 64] = frame
        .sig
        .as_slice()
        .try_into()
        .map_err(|_| RelayDepositReject::BadSig)?;
    let device_vk =
        VerifyingKey::from_bytes(&device_vk_bytes).map_err(|_| RelayDepositReject::BadCert)?;
    device_vk
        .verify_strict(
            &relay_deposit_sig_payload(
                &frame.recipient_owner,
                &frame.community_id,
                &frame.sealed_blob,
            ),
            &Signature::from_bytes(&sig_bytes),
        )
        .map_err(|_| RelayDepositReject::BadSig)?;

    // Step 4 — NO DECRYPT. The relay NEVER opens sealed_blob (which is sealed
    // to the RECIPIENT's device key, not the relay's). Compute the content id
    // over the opaque sealed bytes so the recipient can identify what to pull.
    let content_id = ContentId::for_book(
        &frame.sealed_blob,
        ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .map_err(|e| RelayDepositReject::PersistFailed(format!("content_id: {e}")))?;

    let entry = RelayHoldEntry {
        recipient_owner: frame.recipient_owner,
        sender_owner: frame.sender_owner,
        community_id: frame.community_id,
        sealed_blob: frame.sealed_blob.clone(),
        held_at: ctx.mint_hlc().await,
        held_by: ctx.relay_device_id(),
        pulled_by: BTreeSet::new(),
    };
    let key = RelayHoldDoc::key(&frame.recipient_owner, &content_id.to_bytes());

    // Step 5 — atomic persist-with-caps + durable flush BEFORE the ack
    // exists (D7: an ack never lies). Insert-once on the key; an occupied key
    // bypasses the caps so a redelivery after a lost ack is absorbed even at a
    // full hold store.
    match ctx.persist_hold(key, entry).await {
        Ok(RelayPersistVerdict::Inserted) | Ok(RelayPersistVerdict::Duplicate) => {}
        Ok(RelayPersistVerdict::CapExceeded) => return Err(RelayDepositReject::CapExceeded),
        Err(e) => return Err(RelayDepositReject::PersistFailed(e)),
    }

    // Step 6 — ack.
    Ok(RelayDepositAck {
        content_id: content_id.to_bytes(),
    })
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::butler_deposit::DepositPayload;
    use crate::community_membership::{mint_test_owner, TestOwner};
    use crate::community_relay::{
        build_relay_deposit_frame, RELAY_HOLD_GLOBAL_CAP, RELAY_HOLD_PER_SENDER_CAP,
    };
    use harmony_content::cid::{ContentFlags, ContentId};
    use std::collections::BTreeMap;
    use std::sync::Mutex as StdMutex;

    // ----------------------------------------------------------------
    // Test identity helpers
    // ----------------------------------------------------------------

    /// The relay's (community-volunteer's) device id.
    const RELAY_DEVICE_ID: &str = "relay-device-64hex";

    /// A community SpaceId used across tests.
    fn community_id() -> SpaceId {
        SpaceId([0xCC; 16])
    }

    /// The sender identity: master + enrolled device + cert.
    fn sender() -> TestOwner {
        mint_test_owner(0x51)
    }

    /// The recipient identity.
    fn recipient() -> TestOwner {
        mint_test_owner(0x42)
    }

    // ----------------------------------------------------------------
    // Valid fixture
    // ----------------------------------------------------------------

    struct Fixture {
        frame: RelayDepositFrame,
        sender_owner: [u8; 16],
        recipient_owner: [u8; 16],
        sealed_blob: Vec<u8>,
        expected_content_id: ContentId,
    }

    /// Build a fully valid relay deposit frame sealed to the recipient's
    /// device key, signed by the sender's cert-bound device key.
    fn valid_fixture() -> Fixture {
        let s = sender();
        let r = recipient();
        let cid = community_id();

        let storage_blob = b"dm-storage-blob-opaque-to-relay".to_vec();
        let cidnotify_bytes = b"fake-cidnotify-packet".to_vec();
        let payload = DepositPayload {
            cidnotify_packet: cidnotify_bytes,
            storage_blob,
        };

        let cert_bytes = harmony_owner::cbor::to_canonical(&s.cert).expect("encode cert");
        let frame = build_relay_deposit_frame(
            r.owner.0,
            &r.cert.device_pubkeys.classical.ed25519_verify,
            s.owner.0,
            cid,
            cert_bytes,
            &s.device_key,
            &payload,
        )
        .expect("build relay deposit frame");

        let expected_content_id = ContentId::for_book(
            &frame.sealed_blob,
            ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .expect("content_id for sealed blob");

        Fixture {
            sealed_blob: frame.sealed_blob.clone(),
            sender_owner: s.owner.0,
            recipient_owner: r.owner.0,
            frame,
            expected_content_id,
        }
    }

    // ----------------------------------------------------------------
    // TestRelayDepositCtx: call-order probe + insert-once store with
    // production cap logic
    // ----------------------------------------------------------------

    struct TestRelayDepositCtx {
        /// Communities this relay serves.
        served_communities: std::collections::BTreeSet<SpaceId>,
        /// `(community_id, sender_owner, recipient_owner)` triples that pass
        /// `both_co_members`.
        co_members: std::collections::BTreeSet<([u8; 16], [u8; 16], [u8; 16])>,
        /// Whether `persist_hold` should simulate a flush failure.
        persist_fail: bool,
        /// Insert-once store with production cap logic.
        store: StdMutex<BTreeMap<String, RelayHoldEntry>>,
        /// Ordered event log for call-order assertions.
        events: StdMutex<Vec<String>>,
    }

    impl TestRelayDepositCtx {
        /// Ctx where the fixture's sender + recipient are co-members of the
        /// fixture's community, and the relay serves that community.
        fn for_fixture(f: &Fixture) -> Self {
            let mut served = std::collections::BTreeSet::new();
            served.insert(community_id());
            let mut co = std::collections::BTreeSet::new();
            co.insert((community_id().0, f.sender_owner, f.recipient_owner));
            Self {
                served_communities: served,
                co_members: co,
                persist_fail: false,
                store: StdMutex::new(BTreeMap::new()),
                events: StdMutex::new(Vec::new()),
            }
        }

        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }

        fn push_event(&self, e: impl Into<String>) {
            self.events.lock().unwrap().push(e.into());
        }
    }

    #[async_trait]
    impl RelayDepositCtx for TestRelayDepositCtx {
        fn relay_device_id(&self) -> String {
            RELAY_DEVICE_ID.into()
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
            1_700_000_100
        }

        async fn mint_hlc(&self) -> Hlc {
            Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: RELAY_DEVICE_ID.into(),
            }
        }

        /// Production atomic-cap logic over the test store: occupied key →
        /// Duplicate (caps bypassed); vacant key → quota check then insert.
        /// A CapExceeded verdict writes nothing.
        async fn persist_hold(
            &self,
            key: String,
            entry: RelayHoldEntry,
        ) -> Result<RelayPersistVerdict, String> {
            if self.persist_fail {
                return Err("simulated flush failure".into());
            }
            let mut store = self.store.lock().unwrap();
            if store.contains_key(&key) {
                self.push_event(format!("persist:{key}"));
                return Ok(RelayPersistVerdict::Duplicate);
            }
            // Community-scoped per-sender cap (mirrors butler per-sender cap
            // but scoped to the same community_id, matching count_for_sender).
            let sender_pending = store
                .values()
                .filter(|e| {
                    e.community_id == entry.community_id && e.sender_owner == entry.sender_owner
                })
                .count();
            if sender_pending >= RELAY_HOLD_PER_SENDER_CAP || store.len() >= RELAY_HOLD_GLOBAL_CAP {
                return Ok(RelayPersistVerdict::CapExceeded);
            }
            store.insert(key.clone(), entry);
            // Record AFTER the write so "persist:<key>" means the entry is
            // durably in the store.
            self.push_event(format!("persist:{key}"));
            Ok(RelayPersistVerdict::Inserted)
        }
    }

    /// Minimal filler entry for the cap tests — only ever counted by the
    /// persist-level quota logic, never decoded.
    ///
    /// `idx` is used to produce unique 32-byte content-id keys (stored as
    /// little-endian u64 in the first 8 bytes, rest zero), supporting up to
    /// `u64::MAX` unique entries — far beyond any realistic cap value.
    fn filler_entry(
        sender_owner: [u8; 16],
        recipient_owner: [u8; 16],
        community: SpaceId,
        idx: usize,
    ) -> (String, RelayHoldEntry) {
        let mut content_id = [0u8; 32];
        let idx_bytes = (idx as u64).to_le_bytes();
        content_id[..8].copy_from_slice(&idx_bytes);
        let key = RelayHoldDoc::key(&recipient_owner, &content_id);
        let entry = RelayHoldEntry {
            recipient_owner,
            sender_owner,
            community_id: community,
            sealed_blob: vec![(idx & 0xFF) as u8],
            held_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "filler".into(),
            },
            held_by: "filler".into(),
            pulled_by: BTreeSet::new(),
        };
        (key, entry)
    }

    // ----------------------------------------------------------------
    // Test 1: co-member deposit accepted + held
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn relay_co_member_deposit_accepted_blob_stored_verbatim() {
        let f = valid_fixture();
        let ctx = TestRelayDepositCtx::for_fixture(&f);

        let ack = handle_relay_deposit_core(&f.frame, &ctx)
            .await
            .expect("valid co-member deposit must be accepted");

        // Ack carries content_id = ContentId::for_book(sealed_blob).
        assert_eq!(
            ack.content_id,
            f.expected_content_id.to_bytes(),
            "ack content_id must match ContentId::for_book(sealed_blob)"
        );

        // The stored entry's sealed_blob is byte-identical to the frame's.
        let key = RelayHoldDoc::key(&f.recipient_owner, &f.expected_content_id.to_bytes());
        {
            let store = ctx.store.lock().unwrap();
            let entry = store
                .get(&key)
                .expect("entry must be persisted under relay hold key");
            assert_eq!(
                entry.sealed_blob, f.sealed_blob,
                "stored sealed_blob must be byte-identical to frame sealed_blob"
            );
            assert_eq!(entry.sender_owner, f.sender_owner);
            assert_eq!(entry.recipient_owner, f.recipient_owner);
            assert_eq!(entry.community_id, community_id());
            assert_eq!(entry.held_by, RELAY_DEVICE_ID);
            assert!(entry.pulled_by.is_empty(), "fresh entry has no pulls");
        }

        // Call-order probe: serves_community → both_co_members → persist.
        // There is NO decrypt event (the relay NEVER opens the blob).
        let ev = ctx.events();
        let sc = ev.iter().position(|e| e == "serves_community").unwrap();
        let bc = ev.iter().position(|e| e == "both_co_members").unwrap();
        let ps = ev
            .iter()
            .position(|e| e.starts_with("persist:"))
            .expect("persist must be recorded");
        assert!(sc < bc, "serves_community before both_co_members: {ev:?}");
        assert!(bc < ps, "both_co_members before persist: {ev:?}");
        assert!(
            !ev.iter().any(|e| e == "decrypt"),
            "relay must NEVER decrypt: {ev:?}"
        );
    }

    // ----------------------------------------------------------------
    // Test 2: non-served community → WrongCommunity, nothing persisted
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn relay_wrong_community_rejected_nothing_persisted() {
        let f = valid_fixture();
        let ctx = TestRelayDepositCtx::for_fixture(&f);

        // Frame for an unknown community.
        let mut frame = f.frame.clone();
        frame.community_id = SpaceId([0xFF; 16]);

        let err = handle_relay_deposit_core(&frame, &ctx)
            .await
            .expect_err("unknown community must be rejected");
        assert!(
            matches!(err, RelayDepositReject::WrongCommunity),
            "got {err:?}"
        );

        let ev = ctx.events();
        assert!(
            ev.iter().any(|e| e == "serves_community"),
            "must probe serves_community"
        );
        assert!(
            !ev.iter().any(|e| e.starts_with("persist:")),
            "no persist on WrongCommunity: {ev:?}"
        );
        assert!(ctx.store.lock().unwrap().is_empty());
    }

    // ----------------------------------------------------------------
    // Test 3: not-co-member → NotCoMember, BEFORE any persist
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn relay_not_co_member_rejected_before_persist() {
        let f = valid_fixture();
        // Clear co_members so both_co_members returns false.
        let mut ctx = TestRelayDepositCtx::for_fixture(&f);
        ctx.co_members.clear();

        let err = handle_relay_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("non-co-member must be rejected");
        assert!(
            matches!(err, RelayDepositReject::NotCoMember),
            "got {err:?}"
        );

        let ev = ctx.events();
        assert!(
            !ev.iter().any(|e| e.starts_with("persist:")),
            "no persist on NotCoMember: {ev:?}"
        );
        assert!(ctx.store.lock().unwrap().is_empty());
        // Rejection is before cert work (cert not decoded yet) and before persist.
    }

    // ----------------------------------------------------------------
    // Test 4: bad cert → BadCert
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn relay_bad_cert_rejected() {
        let f = valid_fixture();

        // (a) Garbage cert bytes → BadCert.
        let ctx = TestRelayDepositCtx::for_fixture(&f);
        let mut garbage_cert = f.frame.clone();
        garbage_cert.sender_enrollment_cert = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let err = handle_relay_deposit_core(&garbage_cert, &ctx)
            .await
            .expect_err("garbage cert must be rejected");
        assert!(matches!(err, RelayDepositReject::BadCert), "got {err:?}");
        assert!(
            !ctx.events().iter().any(|e| e.starts_with("persist:")),
            "no persist on bad cert"
        );

        // (b) Someone else's valid cert (cert.owner_id != frame.sender_owner) → BadCert.
        let other = mint_test_owner(0x52);
        let mut wrong_owner_cert = f.frame.clone();
        wrong_owner_cert.sender_enrollment_cert =
            harmony_owner::cbor::to_canonical(&other.cert).expect("encode cert");
        let ctx = TestRelayDepositCtx::for_fixture(&f);
        let err = handle_relay_deposit_core(&wrong_owner_cert, &ctx)
            .await
            .expect_err("wrong owner cert must be rejected");
        assert!(
            matches!(err, RelayDepositReject::BadCert),
            "wrong owner: got {err:?}"
        );

        // Note: forged-master-anchor sub-case intentionally OMITTED — a cert
        // with hash(master) != owner_id cannot pass cert.verify(), so there is
        // no reachable code path for that sub-case with the real minting helpers
        // (exactly as the butler acceptor documents for the co-member branch).
    }

    // ----------------------------------------------------------------
    // Test 5: frame sig mismatch → BadSig
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn relay_bad_sig_rejected() {
        let f = valid_fixture();

        // (a) Tamper a byte of the signature → BadSig.
        let mut tampered_sig = f.frame.clone();
        tampered_sig.sig[10] ^= 0xFF;
        let ctx = TestRelayDepositCtx::for_fixture(&f);
        let err = handle_relay_deposit_core(&tampered_sig, &ctx)
            .await
            .expect_err("tampered sig must be rejected");
        assert!(matches!(err, RelayDepositReject::BadSig), "got {err:?}");
        assert!(
            !ctx.events().iter().any(|e| e.starts_with("persist:")),
            "no persist on bad sig"
        );

        // (b) Wrong-length signature → BadSig.
        let mut short_sig = f.frame.clone();
        short_sig.sig = vec![0x01; 12];
        let ctx = TestRelayDepositCtx::for_fixture(&f);
        let err = handle_relay_deposit_core(&short_sig, &ctx)
            .await
            .expect_err("short sig must be rejected");
        assert!(
            matches!(err, RelayDepositReject::BadSig),
            "short sig: got {err:?}"
        );
    }

    // ----------------------------------------------------------------
    // Test 6: per-sender cap
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn relay_per_sender_cap_exceeded() {
        let f = valid_fixture();
        let ctx = TestRelayDepositCtx::for_fixture(&f);
        let expected_key = RelayHoldDoc::key(&f.recipient_owner, &f.expected_content_id.to_bytes());

        // Pre-fill with RELAY_HOLD_PER_SENDER_CAP entries from this sender in this community.
        {
            let mut store = ctx.store.lock().unwrap();
            for i in 0..RELAY_HOLD_PER_SENDER_CAP {
                let (k, e) = filler_entry(f.sender_owner, f.recipient_owner, community_id(), i);
                store.insert(k, e);
            }
        }

        let err = handle_relay_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("per-sender cap must be enforced");
        assert!(
            matches!(err, RelayDepositReject::CapExceeded),
            "got {err:?}"
        );
        {
            let store = ctx.store.lock().unwrap();
            assert_eq!(
                store.len(),
                RELAY_HOLD_PER_SENDER_CAP,
                "CapExceeded must not insert the overflow entry"
            );
            assert!(!store.contains_key(&expected_key));
        }
    }

    #[tokio::test]
    async fn relay_global_cap_exceeded() {
        let f = valid_fixture();
        let ctx = TestRelayDepositCtx::for_fixture(&f);
        let expected_key = RelayHoldDoc::key(&f.recipient_owner, &f.expected_content_id.to_bytes());

        // Pre-fill RELAY_HOLD_GLOBAL_CAP entries from OTHER senders.
        {
            let mut store = ctx.store.lock().unwrap();
            for i in 0..RELAY_HOLD_GLOBAL_CAP {
                let (k, e) = filler_entry([0xEE; 16], f.recipient_owner, community_id(), i);
                store.insert(k, e);
            }
        }

        let err = handle_relay_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("global cap must be enforced");
        assert!(
            matches!(err, RelayDepositReject::CapExceeded),
            "got {err:?}"
        );
        {
            let store = ctx.store.lock().unwrap();
            assert_eq!(store.len(), RELAY_HOLD_GLOBAL_CAP);
            assert!(!store.contains_key(&expected_key));
        }
    }

    // ----------------------------------------------------------------
    // Test 7: duplicate redelivery → idempotent Ok, caps bypassed
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn relay_duplicate_redelivery_is_idempotent() {
        let f = valid_fixture();
        let ctx = TestRelayDepositCtx::for_fixture(&f);

        let ack1 = handle_relay_deposit_core(&f.frame, &ctx)
            .await
            .expect("first deposit accepted");

        // Fill the store to global cap with other senders.
        {
            let mut store = ctx.store.lock().unwrap();
            let mut i = 0usize;
            while store.len() < RELAY_HOLD_GLOBAL_CAP {
                let (k, e) = filler_entry([0xEE; 16], f.recipient_owner, community_id(), i);
                store.insert(k, e);
                i += 1;
            }
        }

        // Redelivery of the SAME frame at full store → Duplicate, still acked.
        let ack2 = handle_relay_deposit_core(&f.frame, &ctx)
            .await
            .expect("redelivered frame at full store must still be acked");
        assert_eq!(ack1, ack2, "duplicate ack must be identical");

        let store = ctx.store.lock().unwrap();
        assert_eq!(store.len(), RELAY_HOLD_GLOBAL_CAP, "no growth at the cap");
        let key = RelayHoldDoc::key(&f.recipient_owner, &f.expected_content_id.to_bytes());
        assert!(store.contains_key(&key));
    }

    // ----------------------------------------------------------------
    // Test 8: opacity assertion — sealed_blob stored verbatim, no decrypt
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn relay_blob_stored_opaquely_no_decrypt_path() {
        let f = valid_fixture();
        let ctx = TestRelayDepositCtx::for_fixture(&f);

        handle_relay_deposit_core(&f.frame, &ctx)
            .await
            .expect("accepted");

        // The stored entry holds the blob verbatim (byte-identical to frame).
        let key = RelayHoldDoc::key(&f.recipient_owner, &f.expected_content_id.to_bytes());
        let store = ctx.store.lock().unwrap();
        let entry = store.get(&key).expect("entry persisted");
        assert_eq!(
            entry.sealed_blob, f.frame.sealed_blob,
            "stored sealed_blob must be the exact frame bytes — relay never decrypts"
        );

        // Structural opacity: the ctx trait has no decrypt hook; the event log
        // never records "decrypt".
        drop(store);
        let ev = ctx.events();
        assert!(
            !ev.iter().any(|e| e == "decrypt"),
            "no decrypt event must appear — relay is structurally prevented from opening the blob: {ev:?}"
        );
    }

    // ----------------------------------------------------------------
    // Test: D7 — persist failure → no ack
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn relay_persist_failure_produces_no_ack() {
        let f = valid_fixture();
        let ctx = TestRelayDepositCtx {
            persist_fail: true,
            ..TestRelayDepositCtx::for_fixture(&f)
        };
        let err = handle_relay_deposit_core(&f.frame, &ctx)
            .await
            .expect_err("persist failure must never be acked");
        assert!(
            matches!(err, RelayDepositReject::PersistFailed(_)),
            "expected PersistFailed, got {err:?}"
        );
        assert!(ctx.store.lock().unwrap().is_empty());
    }

    // ----------------------------------------------------------------
    // Test: build_relay_deposit_frame cross-check — full pipeline
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn sender_built_frame_passes_acceptor_pipeline() {
        let f = valid_fixture();
        let ctx = TestRelayDepositCtx::for_fixture(&f);

        let ack = handle_relay_deposit_core(&f.frame, &ctx)
            .await
            .expect("sender-built frame must pass every acceptor check");

        assert_eq!(ack.content_id, f.expected_content_id.to_bytes());

        let key = RelayHoldDoc::key(&f.recipient_owner, &f.expected_content_id.to_bytes());
        let store = ctx.store.lock().unwrap();
        let entry = store.get(&key).expect("entry persisted");
        assert_eq!(entry.sealed_blob, f.frame.sealed_blob);
    }
}
