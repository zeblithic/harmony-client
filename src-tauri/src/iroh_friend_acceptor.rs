//! ZEB-370 Phase 1 (Tasks 7-8): the `harmony/friend/v1` friend-link control
//! protocol — wire types, length-prefixed codec, point-to-point enrolled-device
//! authentication, and the inbound handshake acceptor.
//!
//! ## Identity & auth model (spec §3)
//!
//! A friend link is authenticated by the requester's **device-#2 Ed25519
//! signature** plus their **`EnrollmentCert`** (the ZEB-339 model), applied
//! point-to-point (no `SignedMembershipEvent` wrapper). The verifier:
//!   1. runs `cert.verify()` (master→device chain + `owner_id` binding),
//!   2. requires `cert.issuer == Master` (Quorum certs can't be fully verified
//!      here — mirrors `community_membership::enrolled_key_from_cert`),
//!   3. checks `cert.owner_id == claimed owner_id`, and
//!   4. returns `cert.device_pubkeys.classical.ed25519_verify` (the device key
//!      the handshake signature is verified against).
//!
//! This 4-step core is [`verify_enrolled_device`].
//!
//! Friends are keyed on the master `owner_id`; a friend's `master_ed25519` is
//! extracted from their cert's `EnrollmentIssuer::Master { master_pubkey }`.
//!
//! ## Wire protocol
//!
//! Both directions use `[u32 LE length-prefix][canonical-ish CBOR body]` over an
//! iroh bi-stream on the `harmony/friend/v1` ALPN, mirroring
//! `iroh_invite_acceptor`'s framing. Bodies are encoded with `ciborium`
//! (`into_writer`/`from_reader`); decode bounds the body at
//! [`FRIEND_MAX_PACKET_LEN`].
//!
//! Requester → acceptor: [`FriendLinkRequest`].
//! Acceptor → requester: [`FriendLinkAccepted`].

use crate::owner_state_types::{deserialize_bytes_from_bstr, serialize_bytes_as_bstr, OwnerAddr};
use harmony_owner::certs::{EnrollmentCert, EnrollmentIssuer};
use serde::{Deserialize, Serialize};

/// Maximum bytes the acceptor reads per friend-handshake packet. The wire shape
/// is `[u32 LE length-prefix][body]`; any prefix exceeding this is rejected to
/// defend against memory-exhaustion by an adversarial dialer. 256 KiB matches
/// `iroh_invite_acceptor::HANDSHAKE_MAX_PACKET_LEN` and is far larger than any
/// legitimate request (an `EnrollmentCert` + two `[u8;64]` sigs fit in single-
/// digit KB).
pub const FRIEND_MAX_PACKET_LEN: usize = 256 * 1024;

/// A friend-link request: "I am owner `from_addr`; here is my proof (cert +
/// device-#2 signature) and the friend-token signature I am redeeming; please
/// add me and reply with your own proof."
///
/// `sig` is the requester's device-#2 Ed25519 signature over
/// [`friend_request_sig_preimage`]`(from_addr, token_sig)`. `token_sig` binds
/// the request to a specific minted friend token (the ZEB-367 `InviteToken.sig`
/// the inviter published Case-A), so an acceptor can `unregister_friend_token`
/// the consumed one-shot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendLinkRequest {
    /// The requester's master `OwnerAddr` (their `owner_id`). MUST equal
    /// `enrollment.owner_id` (checked by `verify_enrolled_device`).
    pub from_addr: OwnerAddr,
    /// The requester's advertised display name (UX hint). `None` when unset.
    ///
    /// Capped at `MAX_FRIEND_DISPLAY_LEN` at the WIRE boundary (oversized →
    /// hard decode error, not truncation) via the same strict deserializer
    /// `FriendEntry.display` uses. Without this cap an authenticated peer could
    /// push an oversized `display` through the handshake into a `FriendEntry`,
    /// which would then fail to deserialize on the owner's other devices during
    /// owner-state sync.
    #[serde(
        default,
        deserialize_with = "crate::friend_graph::deserialize_capped_display"
    )]
    pub display: Option<String>,
    /// The friend-token signature being redeemed (the inviter's published
    /// `InviteToken.sig`). Bound into the request preimage; lets the acceptor
    /// unregister the consumed Case-A one-shot. Stored as a CBOR bstr(64).
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub token_sig: [u8; 64],
    /// The requester's Master `EnrollmentCert` (their owner→device-#2 binding).
    pub enrollment: EnrollmentCert,
    /// Requester's device-#2 Ed25519 signature over
    /// `friend_request_sig_preimage(from_addr, token_sig)`. Stored as a CBOR
    /// bstr(64).
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
}

/// The acceptor's reply: "accepted; here is my own proof so you can add me back
/// (the mutual link)."
///
/// `sig` is the acceptor's device-#2 Ed25519 signature over
/// [`friend_accept_sig_preimage`]`(from_addr, token_sig)`, where `token_sig` is
/// the same token signature from the originating request (binding the accept to
/// the request it answers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendLinkAccepted {
    /// The accepter's master `OwnerAddr` (their `owner_id`). MUST equal
    /// `enrollment.owner_id`.
    pub from_addr: OwnerAddr,
    /// The accepter's advertised display name (UX hint). `None` when unset.
    ///
    /// Capped at `MAX_FRIEND_DISPLAY_LEN` at the WIRE boundary, same as
    /// `FriendLinkRequest.display` — matters for the future Task-10 redeem path
    /// that turns an accept into a local `FriendEntry`.
    #[serde(
        default,
        deserialize_with = "crate::friend_graph::deserialize_capped_display"
    )]
    pub display: Option<String>,
    /// The accepter's Master `EnrollmentCert`.
    pub enrollment: EnrollmentCert,
    /// Accepter's device-#2 Ed25519 signature over
    /// `friend_accept_sig_preimage(from_addr, token_sig)`. Stored as a CBOR
    /// bstr(64).
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
}

/// Canonical preimage bytes the requester's device-#2 key signs for a
/// [`FriendLinkRequest`]. A small CBOR-encoded tuple `("hfr1", from_addr,
/// token_sig)` — the `"hfr1"` domain tag makes a friend-request signature
/// unmistakable for any other Ed25519 signature this device produces.
pub fn friend_request_sig_preimage(from_addr: OwnerAddr, token_sig: &[u8; 64]) -> Vec<u8> {
    sig_preimage("hfr1", from_addr, token_sig)
}

/// Canonical preimage bytes the accepter's device-#2 key signs for a
/// [`FriendLinkAccepted`]. Domain-separated from the request preimage by the
/// `"hfa1"` tag so a request signature can never be replayed as an accept.
pub fn friend_accept_sig_preimage(from_addr: OwnerAddr, token_sig: &[u8; 64]) -> Vec<u8> {
    sig_preimage("hfa1", from_addr, token_sig)
}

/// Shared preimage builder. The `[u8;64]` is wrapped via `serde_bytes` so it
/// encodes as a CBOR bstr (not a 64-element array), keeping the preimage compact
/// and stable.
fn sig_preimage(domain: &'static str, from_addr: OwnerAddr, token_sig: &[u8; 64]) -> Vec<u8> {
    #[derive(Serialize)]
    struct Preimage<'a> {
        domain: &'a str,
        from_addr: OwnerAddr,
        #[serde(with = "serde_bytes")]
        token_sig: &'a [u8; 64],
    }
    let mut out = Vec::new();
    // Infallible for this fixed-shape value; an encode error would be a logic
    // bug, so surface it loudly rather than silently signing empty bytes.
    ciborium::into_writer(
        &Preimage {
            domain,
            from_addr,
            token_sig,
        },
        &mut out,
    )
    .expect("friend sig preimage always encodes");
    out
}

/// Errors raised while encoding/decoding or authenticating a friend handshake.
#[derive(Debug, thiserror::Error)]
pub enum FriendHandshakeError {
    #[error("CBOR encode failed: {0}")]
    Encode(String),
    #[error("CBOR decode failed: {0}")]
    Decode(String),
    /// Trailing bytes remained after the first CBOR item. `ciborium::from_reader`
    /// stops at the first item and ignores the rest; we reject the remainder so
    /// the friend decoders match the codebase's strict `canonical_cbor_decode`
    /// (no smuggling extra bytes inside an otherwise-valid packet).
    #[error("trailing bytes after CBOR: consumed={consumed} len={len}")]
    TrailingBytes { consumed: usize, len: usize },
    /// The body exceeds [`FRIEND_MAX_PACKET_LEN`]. Bounds work on hostile input.
    #[error("friend packet exceeds size limit: len={len} max={max}")]
    TooLarge { len: usize, max: usize },
    /// `cert.verify()` failed, or the cert's issuer is not `Master`.
    #[error("enrollment cert invalid (verify failed or non-Master issuer)")]
    EnrollmentCertInvalid,
    /// `cert.owner_id` does not equal the claimed owner address.
    #[error("enrollment owner mismatch: cert binds a different owner_id")]
    EnrollmentOwnerMismatch,
    /// The handshake signature did not verify against the enrolled device key.
    #[error("handshake signature invalid")]
    SignatureInvalid,
    /// Applying the resulting `FriendEntry` to the CRDT was rejected (e.g. a
    /// stale HLC or a key↔master-key invariant failure).
    #[error("friend-graph apply rejected: {0}")]
    ApplyRejected(String),
}

/// Encode a [`FriendLinkRequest`] to CBOR bytes (no length prefix). The caller
/// frames it with a `u32 LE` length prefix on the wire.
pub fn encode_friend_request(req: &FriendLinkRequest) -> Result<Vec<u8>, FriendHandshakeError> {
    let mut out = Vec::new();
    ciborium::into_writer(req, &mut out)
        .map_err(|e| FriendHandshakeError::Encode(e.to_string()))?;
    Ok(out)
}

/// Decode a [`FriendLinkRequest`] from CBOR bytes, bounding the input at
/// [`FRIEND_MAX_PACKET_LEN`] before decoding.
pub fn decode_friend_request(bytes: &[u8]) -> Result<FriendLinkRequest, FriendHandshakeError> {
    if bytes.len() > FRIEND_MAX_PACKET_LEN {
        return Err(FriendHandshakeError::TooLarge {
            len: bytes.len(),
            max: FRIEND_MAX_PACKET_LEN,
        });
    }
    decode_strict(bytes)
}

/// Encode a [`FriendLinkAccepted`] to CBOR bytes (no length prefix).
pub fn encode_friend_accepted(acc: &FriendLinkAccepted) -> Result<Vec<u8>, FriendHandshakeError> {
    let mut out = Vec::new();
    ciborium::into_writer(acc, &mut out)
        .map_err(|e| FriendHandshakeError::Encode(e.to_string()))?;
    Ok(out)
}

/// Decode a [`FriendLinkAccepted`] from CBOR bytes, bounding the input at
/// [`FRIEND_MAX_PACKET_LEN`] before decoding.
pub fn decode_friend_accepted(bytes: &[u8]) -> Result<FriendLinkAccepted, FriendHandshakeError> {
    if bytes.len() > FRIEND_MAX_PACKET_LEN {
        return Err(FriendHandshakeError::TooLarge {
            len: bytes.len(),
            max: FRIEND_MAX_PACKET_LEN,
        });
    }
    decode_strict(bytes)
}

/// Decode a single CBOR item from `bytes` and reject any trailing bytes.
/// `ciborium::from_reader` reads the first item and silently ignores the rest;
/// decoding via a cursor lets us assert the whole buffer was consumed, matching
/// the codebase's strict `canonical_cbor_decode` (no extra bytes smuggled inside
/// an otherwise-valid friend packet).
fn decode_strict<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, FriendHandshakeError> {
    let mut cursor = std::io::Cursor::new(bytes);
    let val = ciborium::from_reader(&mut cursor)
        .map_err(|e| FriendHandshakeError::Decode(e.to_string()))?;
    let consumed = cursor.position() as usize;
    if consumed != bytes.len() {
        return Err(FriendHandshakeError::TrailingBytes {
            consumed,
            len: bytes.len(),
        });
    }
    Ok(val)
}

/// Point-to-point enrolled-device authentication: the 4-step core of
/// `community_membership::enrolled_key_from_cert`, applied without the
/// `SignedMembershipEvent` wrapper.
///
/// Verifies `cert`, requires a `Master` issuer, binds `cert.owner_id ==
/// claimed_owner.0`, and returns the enrolled device-#2 Ed25519 verify key the
/// handshake signature must be checked against.
pub fn verify_enrolled_device(
    cert: &EnrollmentCert,
    claimed_owner: OwnerAddr,
) -> Result<[u8; 32], FriendHandshakeError> {
    cert.verify()
        .map_err(|_| FriendHandshakeError::EnrollmentCertInvalid)?;
    // Reject non-Master issuers: cert.verify() only structurally-checks Quorum
    // certs (it cannot verify the quorum signatures without an OwnerState walk-
    // back), so accepting one here would admit unverified signatures. Mirrors
    // enrolled_key_from_cert.
    if !matches!(cert.issuer, EnrollmentIssuer::Master { .. }) {
        return Err(FriendHandshakeError::EnrollmentCertInvalid);
    }
    if cert.owner_id != claimed_owner.0 {
        return Err(FriendHandshakeError::EnrollmentOwnerMismatch);
    }
    Ok(cert.device_pubkeys.classical.ed25519_verify)
}

/// Extract the friend's master Ed25519 verify key from a verified Master
/// `EnrollmentCert`'s issuer. Used to populate `FriendEntry.master_ed25519` (the
/// friend-graph key anchor). Returns `EnrollmentCertInvalid` if the issuer is
/// not `Master` — callers always run `verify_enrolled_device` first, so this is
/// belt-and-suspenders.
pub fn master_ed25519_from_cert(cert: &EnrollmentCert) -> Result<[u8; 32], FriendHandshakeError> {
    match &cert.issuer {
        EnrollmentIssuer::Master { master_pubkey } => Ok(master_pubkey.classical.ed25519_verify),
        _ => Err(FriendHandshakeError::EnrollmentCertInvalid),
    }
}

// =====================================================================
// Task 8 — ALPN acceptor
// =====================================================================

use crate::friend_graph::{FriendEntry, FriendOrigin, FriendStatus};
use crate::owner_state_crdt::{ApplyOutcome, OwnerState};
use crate::owner_state_types::Hlc;
use async_trait::async_trait;
use ed25519_dalek::{Signature, Signer, VerifyingKey};
use iroh::endpoint::Connection;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex as TokioMutex;

/// Tiny emit trait so the acceptor can signal the UI a friend was added
/// without depending on `tauri` directly (mirrors
/// `community_invite::AppHandleEmit`). Production impl on `tauri::AppHandle`
/// lives in `lib.rs`; the unit-type impl lets tests pass `None::<Arc<()>>`.
pub trait FriendEventEmit: Send + Sync + 'static {
    /// Emit a `friend-list-changed` Tauri event (no payload — the frontend
    /// re-fetches the friend list on receipt).
    fn emit_friend_list_changed(&self);
}

impl FriendEventEmit for () {
    fn emit_friend_list_changed(&self) {}
}

/// Default per-await IO deadline for the inbound friend handshake. Mirrors
/// `iroh_invite_acceptor::DEFAULT_ACCEPTOR_IO_DEADLINE_MS`.
pub const DEFAULT_FRIEND_IO_DEADLINE_MS: u64 = 30_000;

/// Tunable timeouts for the friend handshake handler. Tests construct this
/// directly with sub-second values; production uses [`Self::default`] (or an
/// env override at the call site).
#[derive(Debug, Clone, Copy)]
pub struct FriendAcceptorConfig {
    /// Per-await IO timeout bounding `accept_bi`, both `read_exact`s, both
    /// `write_all`s, and `conn.closed()`.
    pub io_deadline: Duration,
}

impl Default for FriendAcceptorConfig {
    fn default() -> Self {
        Self {
            io_deadline: Duration::from_millis(DEFAULT_FRIEND_IO_DEADLINE_MS),
        }
    }
}

/// PURE, testable core of the friend handshake. Authenticates `req`, writes the
/// resulting `FriendEntry` into `state`, and returns a signed
/// `FriendLinkAccepted` for the requester. No I/O.
///
/// Steps (spec §5.2 accept side):
/// 1. `verify_enrolled_device(&req.enrollment, req.from_addr)` → device key,
/// 2. verify `req.sig` over the request preimage against that device key,
/// 3. extract the requester's `master_ed25519` from their Master cert,
/// 4. build `FriendEntry { master_ed25519, display, Active, Token, referrable:
///    false, learned_at }`,
/// 5. `state.apply_friend_update(req.from_addr, entry)` — must be
///    `Inserted`/`Merged` (a `Rejected` is a hard error),
/// 6. build + device-#2-sign a `FriendLinkAccepted` from `self_owner` /
///    `self_enrollment`, signing `friend_accept_sig_preimage(self_owner,
///    req.token_sig)`.
#[allow(clippy::too_many_arguments)]
pub fn process_friend_request(
    state: &mut OwnerState,
    learned_at: Hlc,
    req: &FriendLinkRequest,
    self_owner: OwnerAddr,
    self_display: Option<String>,
    self_enrollment: &EnrollmentCert,
    self_device2: &ed25519_dalek::SigningKey,
) -> Result<FriendLinkAccepted, FriendHandshakeError> {
    // 1. Authenticate the requester's cert → enrolled device-#2 key.
    let device_key = verify_enrolled_device(&req.enrollment, req.from_addr)?;

    // 2. Verify the request signature over the canonical preimage.
    let vk = VerifyingKey::from_bytes(&device_key)
        .map_err(|_| FriendHandshakeError::SignatureInvalid)?;
    let preimage = friend_request_sig_preimage(req.from_addr, &req.token_sig);
    vk.verify_strict(&preimage, &Signature::from_bytes(&req.sig))
        .map_err(|_| FriendHandshakeError::SignatureInvalid)?;

    // 3. Extract the requester's master key (their friend-graph anchor).
    let master_ed25519 = master_ed25519_from_cert(&req.enrollment)?;

    // 4-5. Apply the new friend entry to the CRDT. apply_friend_update re-checks
    // the key↔master-key invariant; a Rejected is a hard error here.
    let entry = FriendEntry {
        master_ed25519,
        display: req.display.clone(),
        status: FriendStatus::Active,
        established_via: FriendOrigin::Token,
        referrable: false,
        learned_at,
    };
    match state.apply_friend_update(req.from_addr, entry) {
        ApplyOutcome::Inserted | ApplyOutcome::Merged { .. } => {}
        ApplyOutcome::Rejected(reason) => {
            return Err(FriendHandshakeError::ApplyRejected(format!("{reason:?}")));
        }
    }

    // 6. Build + sign the mutual accept reply. The accept sig binds to the same
    // token_sig as the request it answers (domain-separated from the request).
    let accept_preimage = friend_accept_sig_preimage(self_owner, &req.token_sig);
    let sig = self_device2.sign(&accept_preimage).to_bytes();
    Ok(FriendLinkAccepted {
        from_addr: self_owner,
        display: self_display,
        enrollment: self_enrollment.clone(),
        sig,
    })
}

/// Inbound dispatcher for the `harmony/friend/v1` ALPN. Holds the handles the
/// pure core needs plus the IO plumbing. Generic over the `FriendEventEmit`
/// impl so tests can stub with `()`.
///
/// Structural template: `iroh_invite_acceptor::IrohInviteHandshakeAcceptor`.
pub struct IrohFriendHandshakeAcceptor<H>
where
    H: FriendEventEmit,
{
    crdt_state: Arc<TokioMutex<OwnerState>>,
    /// Shared HLC tracker (`device_id → last Hlc`), bumped per accepted request
    /// to stamp `FriendEntry.learned_at`. Same map the profile broadcaster uses.
    hlc_tracker: Arc<TokioMutex<std::collections::BTreeMap<String, Hlc>>>,
    device_id: String,
    self_owner: OwnerAddr,
    self_display: Option<String>,
    self_enrollment: EnrollmentCert,
    device2_signing_key: Arc<ed25519_dalek::SigningKey>,
    /// `Some(app)` emits `friend-list-changed`; `None` warn-logs only (tests).
    app: Option<Arc<H>>,
    /// `Some` unregisters the consumed Case-A friend-token one-shot on success.
    pkarr_invite_publisher: Option<Arc<crate::pkarr_invite_publisher::PkarrInvitePublisher>>,
    config: FriendAcceptorConfig,
}

impl<H> IrohFriendHandshakeAcceptor<H>
where
    H: FriendEventEmit,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        crdt_state: Arc<TokioMutex<OwnerState>>,
        hlc_tracker: Arc<TokioMutex<std::collections::BTreeMap<String, Hlc>>>,
        device_id: String,
        self_owner: OwnerAddr,
        self_display: Option<String>,
        self_enrollment: EnrollmentCert,
        device2_signing_key: Arc<ed25519_dalek::SigningKey>,
        app: Option<Arc<H>>,
        pkarr_invite_publisher: Option<Arc<crate::pkarr_invite_publisher::PkarrInvitePublisher>>,
    ) -> Self {
        Self::with_config(
            crdt_state,
            hlc_tracker,
            device_id,
            self_owner,
            self_display,
            self_enrollment,
            device2_signing_key,
            app,
            pkarr_invite_publisher,
            FriendAcceptorConfig::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_config(
        crdt_state: Arc<TokioMutex<OwnerState>>,
        hlc_tracker: Arc<TokioMutex<std::collections::BTreeMap<String, Hlc>>>,
        device_id: String,
        self_owner: OwnerAddr,
        self_display: Option<String>,
        self_enrollment: EnrollmentCert,
        device2_signing_key: Arc<ed25519_dalek::SigningKey>,
        app: Option<Arc<H>>,
        pkarr_invite_publisher: Option<Arc<crate::pkarr_invite_publisher::PkarrInvitePublisher>>,
        config: FriendAcceptorConfig,
    ) -> Self {
        Self {
            crdt_state,
            hlc_tracker,
            device_id,
            self_owner,
            self_display,
            self_enrollment,
            device2_signing_key,
            app,
            pkarr_invite_publisher,
            config,
        }
    }

    /// Bump-and-return a fresh HLC stamped with this device's id. Mirrors
    /// `profile_broadcast::OwnerStateBroadcastSource::next_hlc`.
    async fn next_hlc(&self) -> Hlc {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut tracker = self.hlc_tracker.lock().await;
        let entry = tracker.entry(self.device_id.clone()).or_insert(Hlc {
            wall_ms: 0,
            logical: 0,
            device_id: self.device_id.clone(),
        });
        if now_ms > entry.wall_ms {
            entry.wall_ms = now_ms;
            entry.logical = 0;
        } else {
            entry.logical = entry.logical.saturating_add(1);
        }
        entry.clone()
    }

    /// Inbound bi-stream handler: read the length-prefixed `FriendLinkRequest`,
    /// run the pure core under the CRDT lock, side-effect (unregister token,
    /// emit event), and write the length-prefixed `FriendLinkAccepted`.
    async fn handle_friend_handshake_inbound(
        &self,
        conn: &Connection,
    ) -> Result<(), FriendAcceptError> {
        let (mut send, mut recv) = tokio::time::timeout(self.config.io_deadline, conn.accept_bi())
            .await
            .map_err(|_| FriendAcceptError::IoTimeout { step: "accept_bi" })?
            .map_err(|e| FriendAcceptError::AcceptBi(e.to_string()))?;

        // Read [u32 LE length-prefix][body].
        let mut len_buf = [0u8; 4];
        tokio::time::timeout(self.config.io_deadline, recv.read_exact(&mut len_buf))
            .await
            .map_err(|_| FriendAcceptError::IoTimeout {
                step: "read length-prefix",
            })?
            .map_err(|e| FriendAcceptError::ReadPrefix(e.to_string()))?;
        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 || len > FRIEND_MAX_PACKET_LEN {
            return Err(FriendAcceptError::PrefixOutOfBounds {
                len,
                max: FRIEND_MAX_PACKET_LEN,
            });
        }
        let mut body = vec![0u8; len];
        tokio::time::timeout(self.config.io_deadline, recv.read_exact(&mut body))
            .await
            .map_err(|_| FriendAcceptError::IoTimeout { step: "read body" })?
            .map_err(|e| FriendAcceptError::ReadBody(e.to_string()))?;

        let req = decode_friend_request(&body).map_err(FriendAcceptError::Handshake)?;

        // Run the pure core under the CRDT lock with a fresh HLC.
        let learned_at = self.next_hlc().await;
        let accepted = {
            let mut state = self.crdt_state.lock().await;
            process_friend_request(
                &mut state,
                learned_at,
                &req,
                self.self_owner,
                self.self_display.clone(),
                &self.self_enrollment,
                &self.device2_signing_key,
            )
            .map_err(FriendAcceptError::Handshake)?
        };

        // Success side-effects: free the consumed Case-A one-shot, signal UI.
        if let Some(pub_) = self.pkarr_invite_publisher.as_ref() {
            pub_.unregister_friend_token(&req.token_sig).await;
        }
        match self.app.as_ref() {
            Some(app) => app.emit_friend_list_changed(),
            None => tracing::debug!(
                from_addr = %hex::encode(req.from_addr.0),
                "friend added (no app handle); not emitting friend-list-changed"
            ),
        }

        // Write [u32 LE length-prefix][accepted CBOR].
        let resp = encode_friend_accepted(&accepted).map_err(FriendAcceptError::Handshake)?;
        if resp.len() > FRIEND_MAX_PACKET_LEN {
            return Err(FriendAcceptError::ResponseTooLarge {
                len: resp.len(),
                max: FRIEND_MAX_PACKET_LEN,
            });
        }
        let resp_len = resp.len() as u32;
        tokio::time::timeout(
            self.config.io_deadline,
            send.write_all(&resp_len.to_le_bytes()),
        )
        .await
        .map_err(|_| FriendAcceptError::IoTimeout {
            step: "write length-prefix",
        })?
        .map_err(|e| FriendAcceptError::WritePrefix(e.to_string()))?;
        tokio::time::timeout(self.config.io_deadline, send.write_all(&resp))
            .await
            .map_err(|_| FriendAcceptError::IoTimeout { step: "write body" })?
            .map_err(|e| FriendAcceptError::WriteBody(e.to_string()))?;
        // `send.finish()` is sync — no timeout needed.
        send.finish()
            .map_err(|e| FriendAcceptError::Finish(e.to_string()))?;
        Ok(())
    }
}

#[async_trait]
impl<H> crate::iroh_invite_acceptor::IrohHandshakeDispatcher for IrohFriendHandshakeAcceptor<H>
where
    H: FriendEventEmit,
{
    async fn handle_connection(&self, conn: Connection) {
        match self.handle_friend_handshake_inbound(&conn).await {
            Ok(()) => tracing::info!(
                remote_id = ?conn.remote_id(),
                "ZEB-370: friend handshake completed (accept delivered)"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                remote_id = ?conn.remote_id(),
                "ZEB-370: friend handshake failed"
            ),
        }
        // Wait for the dialer to drive the close so the response bytes flush
        // before `conn` drops (same race-avoidance as iroh_invite_acceptor).
        let _ = tokio::time::timeout(self.config.io_deadline, conn.closed()).await;
    }
}

/// Errors that can short-circuit the inbound friend handshake. The crypto/codec
/// failures are wrapped from [`FriendHandshakeError`]; the rest are IO framing.
#[derive(Debug, thiserror::Error)]
pub enum FriendAcceptError {
    #[error("accept_bi failed: {0}")]
    AcceptBi(String),
    #[error("read length-prefix: {0}")]
    ReadPrefix(String),
    #[error("length-prefix out of bounds: len={len} max={max}")]
    PrefixOutOfBounds { len: usize, max: usize },
    #[error("read body: {0}")]
    ReadBody(String),
    #[error("handshake: {0}")]
    Handshake(#[source] FriendHandshakeError),
    #[error("response too large: len={len} max={max}")]
    ResponseTooLarge { len: usize, max: usize },
    #[error("write length-prefix: {0}")]
    WritePrefix(String),
    #[error("write body: {0}")]
    WriteBody(String),
    #[error("send.finish: {0}")]
    Finish(String),
    #[error("IO timeout in {step}")]
    IoTimeout { step: &'static str },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_membership::mint_test_owner;
    use ed25519_dalek::Signer;

    /// Build a signed, well-formed `FriendLinkRequest` from a test owner.
    fn signed_request(owner_seed: u8, token_sig: [u8; 64]) -> (FriendLinkRequest, [u8; 32]) {
        let owner = mint_test_owner(owner_seed);
        let device_key = owner.cert.device_pubkeys.classical.ed25519_verify;
        let preimage = friend_request_sig_preimage(owner.owner, &token_sig);
        let sig = owner.device_key.sign(&preimage).to_bytes();
        let req = FriendLinkRequest {
            from_addr: owner.owner,
            display: Some("alice".into()),
            token_sig,
            enrollment: owner.cert,
            sig,
        };
        (req, device_key)
    }

    #[test]
    fn friend_request_round_trips() {
        let (req, _) = signed_request(0x21, [9u8; 64]);
        let bytes = encode_friend_request(&req).expect("encode");
        let back = decode_friend_request(&bytes).expect("decode");
        assert_eq!(req, back);
    }

    #[test]
    fn friend_accepted_round_trips() {
        let owner = mint_test_owner(0x22);
        let acc = FriendLinkAccepted {
            from_addr: owner.owner,
            display: None,
            enrollment: owner.cert,
            sig: [4u8; 64],
        };
        let bytes = encode_friend_accepted(&acc).expect("encode");
        let back = decode_friend_accepted(&bytes).expect("decode");
        assert_eq!(acc, back);
    }

    #[test]
    fn decode_rejects_oversized_request() {
        let huge = vec![0u8; FRIEND_MAX_PACKET_LEN + 1];
        match decode_friend_request(&huge) {
            Err(FriendHandshakeError::TooLarge { .. }) => {}
            other => panic!("expected TooLarge, got {other:?}"),
        }
        match decode_friend_accepted(&huge) {
            Err(FriendHandshakeError::TooLarge { .. }) => {}
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_oversized_display_request() {
        // FIX 1: an authenticated peer must not be able to push a display longer
        // than MAX_FRIEND_DISPLAY_LEN through the handshake. The cap is enforced
        // at decode (wire ingress), mirroring FriendEntry.display.
        use crate::friend_graph::MAX_FRIEND_DISPLAY_LEN;
        let (mut req, _) = signed_request(0x40, [3u8; 64]);

        // 257-byte display → must FAIL to decode.
        req.display = Some("x".repeat(MAX_FRIEND_DISPLAY_LEN + 1));
        let bytes = encode_friend_request(&req).expect("encode (serialize is uncapped)");
        let err = decode_friend_request(&bytes).expect_err("oversized display rejected");
        assert!(
            matches!(err, FriendHandshakeError::Decode(_)),
            "expected Decode error, got {err:?}"
        );

        // 256-byte display (exactly at the cap) → still decodes.
        req.display = Some("y".repeat(MAX_FRIEND_DISPLAY_LEN));
        let bytes = encode_friend_request(&req).expect("encode");
        let back = decode_friend_request(&bytes).expect("at-cap display decodes");
        assert_eq!(req, back);
    }

    #[test]
    fn decode_rejects_oversized_display_accepted() {
        // FIX 1, accept side (matters for the Task-10 redeem path that turns an
        // accept into a local FriendEntry).
        use crate::friend_graph::MAX_FRIEND_DISPLAY_LEN;
        let owner = mint_test_owner(0x41);
        let mut acc = FriendLinkAccepted {
            from_addr: owner.owner,
            display: Some("x".repeat(MAX_FRIEND_DISPLAY_LEN + 1)),
            enrollment: owner.cert,
            sig: [4u8; 64],
        };

        // 257-byte display → must FAIL to decode.
        let bytes = encode_friend_accepted(&acc).expect("encode (serialize is uncapped)");
        let err = decode_friend_accepted(&bytes).expect_err("oversized display rejected");
        assert!(
            matches!(err, FriendHandshakeError::Decode(_)),
            "expected Decode error, got {err:?}"
        );

        // 256-byte display (exactly at the cap) → still decodes.
        acc.display = Some("y".repeat(MAX_FRIEND_DISPLAY_LEN));
        let bytes = encode_friend_accepted(&acc).expect("encode");
        let back = decode_friend_accepted(&bytes).expect("at-cap display decodes");
        assert_eq!(acc, back);
    }

    #[test]
    fn decode_rejects_trailing_bytes_request() {
        // FIX 2: a valid request packet with extra trailing bytes appended (still
        // within FRIEND_MAX_PACKET_LEN) must be rejected; the clean packet still
        // round-trips.
        let (req, _) = signed_request(0x42, [5u8; 64]);
        let bytes = encode_friend_request(&req).expect("encode");

        // Clean packet round-trips.
        let back = decode_friend_request(&bytes).expect("clean packet decodes");
        assert_eq!(req, back);

        // Append trailing garbage → must be rejected as TrailingBytes.
        let mut trailing = bytes.clone();
        trailing.extend_from_slice(&[0xff, 0x00, 0x42]);
        assert!(trailing.len() <= FRIEND_MAX_PACKET_LEN);
        let err = decode_friend_request(&trailing).expect_err("trailing bytes rejected");
        assert!(
            matches!(err, FriendHandshakeError::TrailingBytes { .. }),
            "expected TrailingBytes, got {err:?}"
        );
    }

    #[test]
    fn decode_rejects_trailing_bytes_accepted() {
        // FIX 2, accept side.
        let owner = mint_test_owner(0x43);
        let acc = FriendLinkAccepted {
            from_addr: owner.owner,
            display: None,
            enrollment: owner.cert,
            sig: [6u8; 64],
        };
        let bytes = encode_friend_accepted(&acc).expect("encode");

        // Clean packet round-trips.
        let back = decode_friend_accepted(&bytes).expect("clean packet decodes");
        assert_eq!(acc, back);

        // Append trailing garbage → must be rejected as TrailingBytes.
        let mut trailing = bytes.clone();
        trailing.extend_from_slice(&[0x01, 0x02]);
        assert!(trailing.len() <= FRIEND_MAX_PACKET_LEN);
        let err = decode_friend_accepted(&trailing).expect_err("trailing bytes rejected");
        assert!(
            matches!(err, FriendHandshakeError::TrailingBytes { .. }),
            "expected TrailingBytes, got {err:?}"
        );
    }

    #[test]
    fn verify_enrolled_device_accepts_valid_cert() {
        let owner = mint_test_owner(0x31);
        let device_key = verify_enrolled_device(&owner.cert, owner.owner).expect("valid");
        assert_eq!(
            device_key,
            owner.cert.device_pubkeys.classical.ed25519_verify
        );
    }

    #[test]
    fn verify_enrolled_device_rejects_wrong_owner() {
        let owner = mint_test_owner(0x32);
        let other = mint_test_owner(0x33);
        // Cert is owner's, but we claim it belongs to `other` → owner mismatch.
        match verify_enrolled_device(&owner.cert, other.owner) {
            Err(FriendHandshakeError::EnrollmentOwnerMismatch) => {}
            other => panic!("expected EnrollmentOwnerMismatch, got {other:?}"),
        }
    }

    #[test]
    fn verify_enrolled_device_rejects_tampered_cert() {
        let owner = mint_test_owner(0x34);
        let mut cert = owner.cert.clone();
        // Structurally tamper: flip issued_at so the master signature no longer
        // covers the payload → cert.verify() fails.
        cert.issued_at ^= 0xFFFF;
        match verify_enrolled_device(&cert, owner.owner) {
            Err(FriendHandshakeError::EnrollmentCertInvalid) => {}
            other => panic!("expected EnrollmentCertInvalid, got {other:?}"),
        }
    }

    #[test]
    fn verify_enrolled_device_rejects_non_master_issuer() {
        let owner = mint_test_owner(0x35);
        let mut cert = owner.cert.clone();
        // Swap a Quorum issuer in. cert.verify() will structurally pass the
        // device-id check but verify_enrolled_device must reject the non-Master
        // issuer before trusting it.
        cert.issuer = EnrollmentIssuer::Quorum {
            signers: vec![[1u8; 16], [2u8; 16]],
            signatures: vec![vec![0u8; 64], vec![0u8; 64]],
        };
        match verify_enrolled_device(&cert, owner.owner) {
            Err(FriendHandshakeError::EnrollmentCertInvalid) => {}
            other => panic!("expected EnrollmentCertInvalid, got {other:?}"),
        }
    }

    #[test]
    fn request_signature_verifies_against_enrolled_key_and_tamper_fails() {
        use ed25519_dalek::{Signature, VerifyingKey};
        let token_sig = [7u8; 64];
        let (req, device_key) = signed_request(0x36, token_sig);

        // The enrolled device key resolved from the cert must verify the sig
        // over the request preimage.
        let resolved = verify_enrolled_device(&req.enrollment, req.from_addr).expect("valid cert");
        assert_eq!(resolved, device_key);
        let vk = VerifyingKey::from_bytes(&resolved).expect("vk");
        let preimage = friend_request_sig_preimage(req.from_addr, &req.token_sig);
        vk.verify_strict(&preimage, &Signature::from_bytes(&req.sig))
            .expect("untampered sig verifies");

        // A tampered sig (or a preimage over a different token_sig) must fail.
        let bad_preimage = friend_request_sig_preimage(req.from_addr, &[0u8; 64]);
        assert!(vk
            .verify_strict(&bad_preimage, &Signature::from_bytes(&req.sig))
            .is_err());
    }

    #[test]
    fn master_ed25519_from_cert_matches_owner_id() {
        let owner = mint_test_owner(0x37);
        let master = master_ed25519_from_cert(&owner.cert).expect("master cert");
        // The friend-graph key invariant: owner_id derived from this master key
        // equals the cert's owner_id.
        assert_eq!(
            crate::friend_graph::owner_id_from_master_ed25519(&master),
            owner.owner
        );
    }

    fn test_hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "self".into(),
        }
    }

    #[test]
    fn process_friend_request_adds_active_token_friend_and_returns_verifiable_accept() {
        use ed25519_dalek::{Signature, VerifyingKey};
        let me = mint_test_owner(0x60); // the acceptor (self)
        let token_sig = [0x5a; 64];
        let (req, _requester_device) = signed_request(0x61, token_sig);

        let mut state = OwnerState::default();
        let accepted = process_friend_request(
            &mut state,
            test_hlc(1_000),
            &req,
            me.owner,
            Some("me".into()),
            &me.cert,
            &me.device_key,
        )
        .expect("valid request processed");

        // The requester is now an Active/Token friend keyed on req.from_addr,
        // anchored to the requester's master key.
        let entry = state
            .friend_graph
            .friends
            .get(&req.from_addr)
            .expect("friend inserted");
        assert_eq!(entry.status, FriendStatus::Active);
        assert_eq!(entry.established_via, FriendOrigin::Token);
        assert!(!entry.referrable);
        assert_eq!(entry.display.as_deref(), Some("alice"));
        assert_eq!(
            crate::friend_graph::owner_id_from_master_ed25519(&entry.master_ed25519),
            req.from_addr
        );

        // The returned accept is from self and signed by self's device-#2 key
        // over the accept preimage (same token_sig, accept domain tag).
        assert_eq!(accepted.from_addr, me.owner);
        assert_eq!(accepted.display.as_deref(), Some("me"));
        let self_device_key = verify_enrolled_device(&accepted.enrollment, accepted.from_addr)
            .expect("self cert verifies");
        let vk = VerifyingKey::from_bytes(&self_device_key).expect("vk");
        let accept_preimage = friend_accept_sig_preimage(accepted.from_addr, &token_sig);
        vk.verify_strict(&accept_preimage, &Signature::from_bytes(&accepted.sig))
            .expect("accept sig verifies against self enrolled device key");
    }

    #[test]
    fn process_friend_request_rejects_bad_signature_and_writes_nothing() {
        let me = mint_test_owner(0x62);
        let (mut req, _) = signed_request(0x63, [0x11; 64]);
        // Corrupt the request signature.
        req.sig[0] ^= 0xFF;

        let mut state = OwnerState::default();
        let err = process_friend_request(
            &mut state,
            test_hlc(1),
            &req,
            me.owner,
            None,
            &me.cert,
            &me.device_key,
        )
        .expect_err("bad sig rejected");
        assert!(matches!(err, FriendHandshakeError::SignatureInvalid));
        assert!(
            state.friend_graph.friends.is_empty(),
            "a rejected request must not write a friend entry"
        );
    }

    #[test]
    fn process_friend_request_rejects_wrong_owner_cert_and_writes_nothing() {
        let me = mint_test_owner(0x64);
        // Build a request whose from_addr does NOT match its embedded cert's
        // owner_id (cert/owner mismatch) — verify_enrolled_device must reject.
        let requester = mint_test_owner(0x65);
        let imposter = mint_test_owner(0x66);
        let token_sig = [0x22; 64];
        // Sign with the imposter's owner addr in the preimage so the request is
        // internally consistent except for the cert↔from_addr binding.
        let preimage = friend_request_sig_preimage(imposter.owner, &token_sig);
        let sig = imposter.device_key.sign(&preimage).to_bytes();
        let req = FriendLinkRequest {
            from_addr: imposter.owner, // claims to be imposter…
            display: None,
            token_sig,
            enrollment: requester.cert, // …but presents requester's cert
            sig,
        };

        let mut state = OwnerState::default();
        let err = process_friend_request(
            &mut state,
            test_hlc(1),
            &req,
            me.owner,
            None,
            &me.cert,
            &me.device_key,
        )
        .expect_err("owner-mismatched cert rejected");
        assert!(matches!(err, FriendHandshakeError::EnrollmentOwnerMismatch));
        assert!(
            state.friend_graph.friends.is_empty(),
            "a rejected request must not write a friend entry"
        );
    }
}
