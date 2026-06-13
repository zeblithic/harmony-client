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

use crate::community_relay::{
    relay_deposit_sig_payload, relay_pull_sig_payload, RelayDepositAck, RelayDepositFrame,
    RelayHeldBlob, RelayPullAck, RelayPullQuery, RelayPullResponse,
};
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
// Task 4: Relay PULL acceptor — requester auth + serve + ack→pulled_by→GC
// =====================================================================

/// Why a relay pull request (or pull ack) was rejected. For local logging
/// and test assertions only; the wire never exposes a detailed reason (uniform
/// reject = no oracle for enumerating held blobs).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RelayPullReject {
    /// This relay does not serve the named community.
    #[error("relay does not serve this community")]
    WrongCommunity,
    /// The requester is not a Joined member of the named community.
    #[error("requester is not a member of the community")]
    NotMember,
    /// The embedded `EnrollmentCert` failed to decode, failed verification,
    /// is not Master-issued, has `owner_id != query.recipient_owner`, or its
    /// master key does not match the owner-id-derived anchor.
    #[error("requester enrollment cert invalid")]
    BadCert,
    /// The pull query (or ack) signature is malformed or does not verify
    /// against the cert-bound device key over `relay_pull_sig_payload`.
    #[error("pull query signature invalid")]
    BadSig,
    /// `mark_pulled` returned an error (logged; the requester may retry).
    #[error("mark-pulled failed: {0}")]
    MarkFailed(String),
}

/// Injectable context for [`handle_relay_pull_query`] and
/// [`handle_relay_pull_ack`]. Production implements this over `NodeState`'s
/// community and relay-hold state; tests implement it with probes.
#[async_trait]
pub trait RelayPullCtx: Send + Sync {
    /// Opt-in + membership check: this relay is a Joined member of
    /// `community_id` AND has opted in to relay for it.
    async fn serves_community(&self, community_id: &SpaceId) -> bool;

    /// `owner` is a Joined member of `community_id` in the relay's local
    /// replicated membership state.
    async fn is_joined_member(&self, community_id: &SpaceId, owner: &[u8; 16]) -> bool;

    /// Wall-clock now in epoch-SECONDS for `EnrollmentCert` expiry checks.
    fn now_secs(&self) -> u64;

    /// All held blobs for `recipient_owner` (any sealed-to device). Returns
    /// `(storage_key, RelayHeldBlob)` pairs so the ack handler can translate
    /// content IDs to storage keys without a second lookup.
    async fn held_for(&self, recipient_owner: &[u8; 16]) -> Vec<(String, RelayHeldBlob)>;

    /// Record that `requester_device` pulled + acked the blobs at `keys`, then
    /// run GC. Implementations should treat missing keys as a no-op so an ack
    /// for an already-GC'd blob does not return an error. Returns `Err(String)`
    /// only for genuine storage failures.
    async fn mark_pulled(&self, keys: &[String], requester_device: String) -> Result<(), String>;
}

// ----------------------------------------------------------------
// Shared pull-auth helper (cert decode + owner-id anchor + membership)
// ----------------------------------------------------------------

/// Common auth steps shared by the query and ack handlers:
///
/// 1. Decode + verify the requester's `EnrollmentCert`.
/// 2. Require Master issuer and `cert.owner_id == recipient_owner`.
/// 3. Verify owner-id-derived anchor.
/// 4. Check `is_joined_member`.
/// 5. Verify the pull signature over `relay_pull_sig_payload`.
///
/// Returns `(cert_device_vk_bytes, requester_device_id)` on success.
async fn auth_pull_requester(
    cert_bytes: &[u8],
    sig_bytes_raw: &[u8],
    recipient_owner: &[u8; 16],
    community_id: &SpaceId,
    ctx: &dyn RelayPullCtx,
) -> Result<([u8; 32], String), RelayPullReject> {
    // Step 1 — decode + verify the requester's EnrollmentCert (same strict
    // helper as the deposit acceptor, adapted for pull rejects).
    let cert = decode_pull_cert_strict(cert_bytes)?;
    cert.verify(ctx.now_secs())
        .map_err(|_| RelayPullReject::BadCert)?;

    // Step 2 — require Master issuer; extract master pubkey.
    let cert_master = match &cert.issuer {
        EnrollmentIssuer::Master { master_pubkey } => master_pubkey.classical.ed25519_verify,
        _ => return Err(RelayPullReject::BadCert),
    };

    // Bind cert to the claimed recipient_owner identity.
    if cert.owner_id != *recipient_owner {
        return Err(RelayPullReject::BadCert);
    }

    // Step 3 — owner-id-derived anchor (D29.1 co-member branch).
    if crate::friend_graph::owner_id_from_master_ed25519(&cert_master)
        != crate::owner_state_types::OwnerAddr(*recipient_owner)
    {
        return Err(RelayPullReject::BadCert);
    }

    let device_vk_bytes = cert.device_pubkeys.classical.ed25519_verify;

    // Step 4 — membership gate. This defeats held-blob enumeration by
    // ex-members or strangers; the blobs are still sealed regardless.
    if !ctx.is_joined_member(community_id, recipient_owner).await {
        return Err(RelayPullReject::NotMember);
    }

    // Step 5 — verify the pull signature over
    // `COMMUNITY_RELAY_PULL_SIG_DOMAIN ‖ recipient_owner ‖ community_id`.
    let sig_arr: [u8; 64] = sig_bytes_raw
        .try_into()
        .map_err(|_| RelayPullReject::BadSig)?;
    let device_vk =
        VerifyingKey::from_bytes(&device_vk_bytes).map_err(|_| RelayPullReject::BadCert)?;
    device_vk
        .verify_strict(
            &relay_pull_sig_payload(recipient_owner, community_id),
            &Signature::from_bytes(&sig_arr),
        )
        .map_err(|_| RelayPullReject::BadSig)?;

    // SP1 device id = 64-hex (lowercase) of the device ed25519 verify key,
    // mirroring `iroh_butler_acceptor`'s `device_id()` and the SP1 definition
    // in `butler_outhold_integration.rs::device_id_hex`.
    let requester_device_id = hex::encode(device_vk_bytes);

    Ok((device_vk_bytes, requester_device_id))
}

/// Strict canonical-CBOR decode of an [`EnrollmentCert`] for the pull
/// path; trailing bytes rejected. Produces [`RelayPullReject::BadCert`].
fn decode_pull_cert_strict(bytes: &[u8]) -> Result<EnrollmentCert, RelayPullReject> {
    let mut cursor = std::io::Cursor::new(bytes);
    let cert: EnrollmentCert =
        ciborium::from_reader(&mut cursor).map_err(|_| RelayPullReject::BadCert)?;
    if cursor.position() as usize != bytes.len() {
        return Err(RelayPullReject::BadCert);
    }
    Ok(cert)
}

// ----------------------------------------------------------------
// Query handler
// ----------------------------------------------------------------

/// Relay pull query pipeline (spec D39 steps 0-4):
///
/// 0. `serves_community` — relay opted in; else [`RelayPullReject::WrongCommunity`].
/// 1. Auth the requester: cert decode + Master-issuer + owner-id-derived
///    anchor + `cert.owner_id == query.recipient_owner`;
///    else [`RelayPullReject::BadCert`].
/// 2. `is_joined_member` — gates pull to current members;
///    else [`RelayPullReject::NotMember`].
/// 3. Frame sig: `cert.device_pubkeys.classical.ed25519_verify_strict`
///    over `relay_pull_sig_payload`; else [`RelayPullReject::BadSig`].
/// 4. Return `RelayPullResponse { entries: held_for(recipient_owner) }`.
pub async fn handle_relay_pull_query(
    query: &RelayPullQuery,
    ctx: &dyn RelayPullCtx,
) -> Result<RelayPullResponse, RelayPullReject> {
    // Step 0 — community gate.
    if !ctx.serves_community(&query.community_id).await {
        return Err(RelayPullReject::WrongCommunity);
    }

    // Steps 1-5 — shared auth (cert + anchor + membership + sig).
    let (_device_vk_bytes, _requester_device_id) = auth_pull_requester(
        &query.requester_enrollment_cert,
        &query.sig,
        &query.recipient_owner,
        &query.community_id,
        ctx,
    )
    .await?;

    // Step 4 — serve held blobs for this recipient (opaque).
    let held = ctx.held_for(&query.recipient_owner).await;
    let entries = held.into_iter().map(|(_, blob)| blob).collect();
    Ok(RelayPullResponse { entries })
}

// ----------------------------------------------------------------
// Ack handler
// ----------------------------------------------------------------

/// Relay pull ack pipeline:
///
/// 1. Run the SAME auth (cert + `is_joined_member` + sig) on
///    `requester_cert_bytes` / `ack_sig`.
/// 2. Translate `ack.content_ids` → storage keys via
///    `RelayHoldDoc::key(recipient_owner, content_id)`.
/// 3. Call `mark_pulled(keys, requester_device_id)`. An ack for a content id
///    not currently held is a no-op (mark_pulled tolerates missing keys).
pub async fn handle_relay_pull_ack(
    recipient_owner: &[u8; 16],
    community_id: &SpaceId,
    ack: &RelayPullAck,
    requester_cert_bytes: &[u8],
    ack_sig: &[u8],
    ctx: &dyn RelayPullCtx,
) -> Result<(), RelayPullReject> {
    // Community gate first (cheapest check).
    if !ctx.serves_community(community_id).await {
        return Err(RelayPullReject::WrongCommunity);
    }

    // Shared auth — cert + anchor + membership + sig.
    let (_device_vk_bytes, requester_device_id) = auth_pull_requester(
        requester_cert_bytes,
        ack_sig,
        recipient_owner,
        community_id,
        ctx,
    )
    .await?;

    // Translate content IDs to storage keys.
    let keys: Vec<String> = ack
        .content_ids
        .iter()
        .map(|cid| RelayHoldDoc::key(recipient_owner, cid))
        .collect();

    // Mark pulled + run GC. Missing keys are no-ops inside mark_pulled.
    ctx.mark_pulled(&keys, requester_device_id)
        .await
        .map_err(RelayPullReject::MarkFailed)
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

    // ================================================================
    // Task 4 pull tests
    // ================================================================

    use crate::community_relay::{relay_pull_sig_payload, RelayPullQuery};
    use ed25519_dalek::Signer;

    // ----------------------------------------------------------------
    // TestRelayPullCtx — RelayHoldDoc-backed store + events probe
    // ----------------------------------------------------------------

    /// Build a valid `RelayPullQuery` signed by the recipient's device key.
    fn build_pull_query(recipient: &TestOwner, community_id: SpaceId) -> RelayPullQuery {
        let cert_bytes = harmony_owner::cbor::to_canonical(&recipient.cert).expect("encode cert");
        let sig = recipient
            .device_key
            .sign(&relay_pull_sig_payload(&recipient.owner.0, &community_id))
            .to_bytes()
            .to_vec();
        RelayPullQuery {
            recipient_owner: recipient.owner.0,
            community_id,
            requester_enrollment_cert: cert_bytes,
            sig,
        }
    }

    struct TestRelayPullCtx {
        /// Communities this relay serves.
        served_communities: std::collections::BTreeSet<SpaceId>,
        /// (community_id, owner) pairs that pass `is_joined_member`.
        members: std::collections::BTreeSet<([u8; 16], [u8; 16])>,
        /// Backing store (RelayHoldDoc).
        doc: StdMutex<RelayHoldDoc>,
        /// Wall-clock now in seconds (for cert expiry).
        now_secs: u64,
        /// Wall-clock now in milliseconds (for gc).
        now_ms: u64,
        /// Ordered event log for call-order assertions.
        events: StdMutex<Vec<String>>,
    }

    impl TestRelayPullCtx {
        fn new(now_secs: u64, now_ms: u64) -> Self {
            Self {
                served_communities: Default::default(),
                members: Default::default(),
                doc: StdMutex::new(RelayHoldDoc::default()),
                now_secs,
                now_ms,
                events: StdMutex::new(Vec::new()),
            }
        }

        fn serve(&mut self, community_id: SpaceId) {
            self.served_communities.insert(community_id);
        }

        fn admit(&mut self, community_id: SpaceId, owner: [u8; 16]) {
            self.members.insert((community_id.0, owner));
        }

        /// Pre-load an entry into the doc store (simulates a prior deposit).
        fn preload_entry(&self, key: String, entry: RelayHoldEntry) {
            self.doc.lock().unwrap().entries.insert(key, entry);
        }

        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }

        fn push_event(&self, e: impl Into<String>) {
            self.events.lock().unwrap().push(e.into());
        }

        /// Snapshot the current pulled_by set for a key.
        fn pulled_by_for(&self, key: &str) -> BTreeSet<String> {
            self.doc
                .lock()
                .unwrap()
                .entries
                .get(key)
                .map(|e| e.pulled_by.clone())
                .unwrap_or_default()
        }

        /// True iff the entry key still exists in the doc.
        fn entry_exists(&self, key: &str) -> bool {
            self.doc.lock().unwrap().entries.contains_key(key)
        }

        /// Drive one GC sweep (for test assertions about the one-sweep
        /// deferral — separate from `mark_pulled` so tests control sweep
        /// timing explicitly). Returns true iff the doc changed.
        fn gc_sweep(&self) -> bool {
            self.doc.lock().unwrap().gc(self.now_ms)
        }
    }

    #[async_trait]
    impl RelayPullCtx for TestRelayPullCtx {
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

        async fn held_for(&self, recipient_owner: &[u8; 16]) -> Vec<(String, RelayHeldBlob)> {
            self.push_event("held_for");
            self.doc
                .lock()
                .unwrap()
                .entries
                .iter()
                .filter(|(_, e)| &e.recipient_owner == recipient_owner)
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

        /// Set `pulled_by` for matched keys (silently skip missing keys).
        /// Does NOT run GC — the test drives gc_sweep() manually to verify
        /// the one-sweep deferral invariant.
        async fn mark_pulled(
            &self,
            keys: &[String],
            requester_device: String,
        ) -> Result<(), String> {
            self.push_event(format!("mark_pulled:{}", keys.len()));
            let mut doc = self.doc.lock().unwrap();
            for k in keys {
                if let Some(e) = doc.entries.get_mut(k) {
                    e.pulled_by.insert(requester_device.clone());
                }
                // Missing keys are silently ignored (no-op).
            }
            Ok(())
        }
    }

    // ----------------------------------------------------------------
    // Helper: build a RelayHoldEntry for a given recipient+sender
    // ----------------------------------------------------------------

    fn pull_test_entry(
        recipient: &TestOwner,
        sender: &TestOwner,
        community_id: SpaceId,
        blob: Vec<u8>,
        held_at_ms: u64,
    ) -> (String, RelayHoldEntry) {
        let content_id = ContentId::for_book(
            &blob,
            ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .expect("content_id");
        let key = RelayHoldDoc::key(&recipient.owner.0, &content_id.to_bytes());
        let entry = RelayHoldEntry {
            recipient_owner: recipient.owner.0,
            sender_owner: sender.owner.0,
            community_id,
            sealed_blob: blob,
            held_at: Hlc {
                wall_ms: held_at_ms,
                logical: 0,
                device_id: "relay-device".into(),
            },
            held_by: "relay-device".into(),
            pulled_by: BTreeSet::new(),
        };
        (key, entry)
    }

    // ----------------------------------------------------------------
    // Test 1: authed recipient pull returns exactly their blobs
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn relay_pull_authed_recipient_returns_their_blobs_only() {
        let recipient = mint_test_owner(0x42);
        let other_recipient = mint_test_owner(0x43);
        let sender = mint_test_owner(0x51);
        let cid = community_id();

        // now_secs=1_700_000_100 (cert minted at 1_700_000_000 with no expiry)
        let now_secs = 1_700_000_100u64;
        let now_ms = 1_700_000_100_000u64;

        let mut ctx = TestRelayPullCtx::new(now_secs, now_ms);
        ctx.serve(cid);
        ctx.admit(cid, recipient.owner.0);

        // One blob for our recipient.
        let blob1 = b"blob-for-recipient".to_vec();
        let (key1, entry1) =
            pull_test_entry(&recipient, &sender, cid, blob1.clone(), now_ms - 1_000);
        ctx.preload_entry(key1.clone(), entry1);

        // One blob for another recipient — must NOT appear in the response.
        let blob2 = b"blob-for-other".to_vec();
        let (key2, entry2) = pull_test_entry(
            &other_recipient,
            &sender,
            cid,
            blob2.clone(),
            now_ms - 1_000,
        );
        ctx.preload_entry(key2.clone(), entry2);

        let query = build_pull_query(&recipient, cid);
        let resp = handle_relay_pull_query(&query, &ctx)
            .await
            .expect("valid pull query must succeed");

        assert_eq!(resp.entries.len(), 1, "exactly one blob for this recipient");
        assert_eq!(
            resp.entries[0].sealed_blob, blob1,
            "sealed_blob is byte-identical to deposited blob"
        );
        assert_eq!(resp.entries[0].sender_owner, sender.owner.0);

        // Call-order probe: serves_community → (auth → held_for).
        let ev = ctx.events();
        assert!(ev.iter().any(|e| e == "serves_community"));
        assert!(ev.iter().any(|e| e == "is_joined_member"));
        assert!(ev.iter().any(|e| e == "held_for"));
    }

    // ----------------------------------------------------------------
    // Test 2: wrong-owner cert → BadCert
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn relay_pull_wrong_owner_cert_bad_cert() {
        let recipient = mint_test_owner(0x42);
        let other = mint_test_owner(0x55); // different owner
        let cid = community_id();

        let now_secs = 1_700_000_100u64;
        let now_ms = 1_700_000_100_000u64;

        let mut ctx = TestRelayPullCtx::new(now_secs, now_ms);
        ctx.serve(cid);
        ctx.admit(cid, recipient.owner.0);

        // Build a query where the cert belongs to `other` but recipient_owner
        // is `recipient` — cert.owner_id != query.recipient_owner.
        let other_cert_bytes = harmony_owner::cbor::to_canonical(&other.cert).expect("encode cert");
        // Sign with the OTHER's device key (so the sig is valid for the other's cert,
        // but the cert.owner_id != recipient.owner.0).
        let sig = other
            .device_key
            .sign(&relay_pull_sig_payload(&recipient.owner.0, &cid))
            .to_bytes()
            .to_vec();
        let query = RelayPullQuery {
            recipient_owner: recipient.owner.0,
            community_id: cid,
            requester_enrollment_cert: other_cert_bytes,
            sig,
        };

        let err = handle_relay_pull_query(&query, &ctx)
            .await
            .expect_err("wrong-owner cert must be rejected");
        assert!(
            matches!(err, RelayPullReject::BadCert),
            "expected BadCert, got {err:?}"
        );

        // No held_for call (rejected before serve step).
        assert!(
            !ctx.events().iter().any(|e| e == "held_for"),
            "held_for must not be called on BadCert"
        );
    }

    // ----------------------------------------------------------------
    // Test 3: non-served community → WrongCommunity
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn relay_pull_wrong_community_rejected() {
        let recipient = mint_test_owner(0x42);
        let cid = community_id();
        let other_cid = SpaceId([0xFF; 16]);

        let now_secs = 1_700_000_100u64;
        let now_ms = 1_700_000_100_000u64;

        let mut ctx = TestRelayPullCtx::new(now_secs, now_ms);
        ctx.serve(cid); // serves cid, NOT other_cid

        let query = build_pull_query(&recipient, other_cid);
        let err = handle_relay_pull_query(&query, &ctx)
            .await
            .expect_err("non-served community must be rejected");
        assert!(
            matches!(err, RelayPullReject::WrongCommunity),
            "expected WrongCommunity, got {err:?}"
        );

        let ev = ctx.events();
        assert!(
            ev.iter().any(|e| e == "serves_community"),
            "must probe serves_community"
        );
        assert!(
            !ev.iter().any(|e| e == "held_for"),
            "held_for must not be called on WrongCommunity"
        );
    }

    // ----------------------------------------------------------------
    // Test 4: non-member recipient → NotMember
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn relay_pull_not_member_rejected() {
        let recipient = mint_test_owner(0x42);
        let cid = community_id();

        let now_secs = 1_700_000_100u64;
        let now_ms = 1_700_000_100_000u64;

        let mut ctx = TestRelayPullCtx::new(now_secs, now_ms);
        ctx.serve(cid);
        // Do NOT admit the recipient → is_joined_member returns false.

        let query = build_pull_query(&recipient, cid);
        let err = handle_relay_pull_query(&query, &ctx)
            .await
            .expect_err("non-member must be rejected");
        assert!(
            matches!(err, RelayPullReject::NotMember),
            "expected NotMember, got {err:?}"
        );

        assert!(
            !ctx.events().iter().any(|e| e == "held_for"),
            "held_for must not be called on NotMember"
        );
    }

    // ----------------------------------------------------------------
    // Test 5: tampered sig → BadSig
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn relay_pull_bad_sig_rejected() {
        let recipient = mint_test_owner(0x42);
        let cid = community_id();

        let now_secs = 1_700_000_100u64;
        let now_ms = 1_700_000_100_000u64;

        let mut ctx = TestRelayPullCtx::new(now_secs, now_ms);
        ctx.serve(cid);
        ctx.admit(cid, recipient.owner.0);

        let mut query = build_pull_query(&recipient, cid);
        query.sig[10] ^= 0xFF; // tamper a byte

        let err = handle_relay_pull_query(&query, &ctx)
            .await
            .expect_err("tampered sig must be rejected");
        assert!(
            matches!(err, RelayPullReject::BadSig),
            "expected BadSig, got {err:?}"
        );

        assert!(
            !ctx.events().iter().any(|e| e == "held_for"),
            "held_for must not be called on BadSig"
        );
    }

    // ----------------------------------------------------------------
    // Test 6: ack marks pulled_by + GC removes covered entry
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn relay_pull_ack_marks_pulled_and_gc_removes_on_next_sweep() {
        let recipient = mint_test_owner(0x42);
        let sender = mint_test_owner(0x51);
        let cid = community_id();

        // Use a well-past now_ms so TTL isn't a factor.
        let now_secs = 1_700_000_100u64;
        let now_ms = 1_700_000_100_000u64;

        let mut ctx = TestRelayPullCtx::new(now_secs, now_ms);
        ctx.serve(cid);
        ctx.admit(cid, recipient.owner.0);

        // Deposit one blob.
        let blob = b"ack-me-blob".to_vec();
        let (key, entry) = pull_test_entry(&recipient, &sender, cid, blob.clone(), now_ms - 1_000);
        ctx.preload_entry(key.clone(), entry);

        // Compute the content_id for the ack.
        let content_id = ContentId::for_book(
            &blob,
            ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .expect("content_id");

        let cert_bytes = harmony_owner::cbor::to_canonical(&recipient.cert).expect("encode cert");
        let ack_sig = recipient
            .device_key
            .sign(&relay_pull_sig_payload(&recipient.owner.0, &cid))
            .to_bytes()
            .to_vec();
        let ack = RelayPullAck {
            content_ids: vec![content_id.to_bytes()],
        };

        // Ack: mark_pulled records the requester device in pulled_by for
        // the acked content IDs (does NOT call gc in the test mock).
        handle_relay_pull_ack(&recipient.owner.0, &cid, &ack, &cert_bytes, &ack_sig, &ctx)
            .await
            .expect("ack must succeed");

        // After mark_pulled: pulled_by is set.
        let pb = ctx.pulled_by_for(&key);
        let recipient_device_id =
            hex::encode(recipient.cert.device_pubkeys.classical.ed25519_verify);
        assert!(
            pb.contains(&recipient_device_id),
            "pulled_by must contain the requester device id after ack"
        );

        // One-sweep deferral (spec + RelayHoldDoc::gc() comment): the FIRST gc
        // sweep after the ack defers removal because covered_at_start is
        // snapshotted at the TOP of gc() — the entry IS in covered_at_start
        // (pulled_by was set before this call), so it IS removed immediately.
        // Wait — the deferral only protects entries that become covered DURING
        // a gc call; entries that were already covered AT gc() entry are removed
        // on that very call. So: first gc_sweep removes the entry.
        let changed = ctx.gc_sweep();
        assert!(
            changed,
            "first gc sweep after ack must remove the covered entry"
        );
        assert!(
            !ctx.entry_exists(&key),
            "entry must be removed by the first gc sweep after ack"
        );

        // Ack for an already-GC'd key is a no-op (not an error).
        handle_relay_pull_ack(&recipient.owner.0, &cid, &ack, &cert_bytes, &ack_sig, &ctx)
            .await
            .expect("second ack for already-removed key must be a no-op");
    }

    // ----------------------------------------------------------------
    // Test 6b: ack for unknown content_id is a no-op
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn relay_pull_ack_unknown_content_id_is_noop() {
        let recipient = mint_test_owner(0x42);
        let cid = community_id();

        let now_secs = 1_700_000_100u64;
        let now_ms = 1_700_000_100_000u64;

        let mut ctx = TestRelayPullCtx::new(now_secs, now_ms);
        ctx.serve(cid);
        ctx.admit(cid, recipient.owner.0);
        // Store is empty — no held blobs.

        let cert_bytes = harmony_owner::cbor::to_canonical(&recipient.cert).expect("encode cert");
        let ack_sig = recipient
            .device_key
            .sign(&relay_pull_sig_payload(&recipient.owner.0, &cid))
            .to_bytes()
            .to_vec();
        let unknown_content_id = [0xAB; 32];
        let ack = RelayPullAck {
            content_ids: vec![unknown_content_id],
        };

        // Must succeed — unknown content IDs are silently ignored.
        handle_relay_pull_ack(&recipient.owner.0, &cid, &ack, &cert_bytes, &ack_sig, &ctx)
            .await
            .expect("ack for unknown content_id must be a no-op, not an error");

        // Nothing was inserted or removed.
        assert!(ctx.doc.lock().unwrap().entries.is_empty());
    }
}
