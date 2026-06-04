//! ZEB-375 (Friends Phase 2a): friend-PEX catalog acceptor. Serves a signed
//! ReferralCatalog on the `harmony/friend-pex/v1` ALPN to an authenticated
//! Active friend (empty, benign catalog to anyone else). Read-only: never
//! mutates owner-state.

use std::sync::Arc;

use iroh::endpoint::Connection;
use tokio::sync::Mutex as TokioMutex;

use crate::friend_graph::{FriendGraph, FriendStatus};
use crate::iroh_friend_acceptor::FriendAcceptorConfig;
use crate::owner_state_crdt::OwnerState;
use crate::owner_state_types::{Hlc, OwnerAddr};
use crate::referral_catalog::*;
// EnrollmentCert via the SAME path `iroh_friend_acceptor.rs` uses.
use harmony_owner::certs::EnrollmentCert;

/// PURE serve decision. Authenticate (`to_addr` must be us) → friend-gate →
/// build the signed catalog.
///
/// * Active friend → full catalog (their referrable friends).
/// * authenticated non-friend → EMPTY catalog (still signed + subject-bound +
///   benign — it deliberately does NOT distinguish "I have no referrables" from
///   "you are not my friend").
/// * auth / `to_addr` failure → `Err` (caller closes the stream, serves
///   nothing — fail closed).
///
/// Read-only: takes `fg` by shared ref; never mutates owner-state.
pub fn serve_catalog_for_request(
    fg: &FriendGraph,
    req: &CatalogRequest,
    self_owner: OwnerAddr,
    self_enrollment: EnrollmentCert,
    device2: &ed25519_dalek::SigningKey,
    at: Hlc,
) -> Result<ReferralCatalog, ReferralAuthError> {
    // 1. Authenticate the request against OUR owner address (rejects a request
    //    addressed to someone else, a bad cert, or a bad signature).
    authenticate_catalog_request(req, self_owner)?;
    // 2. Friend-gate: only an ACTIVE friend gets a non-empty catalog. Anyone
    //    else (authenticated but unknown, or Pending/Revoked) gets EMPTY.
    let is_friend = fg
        .friends
        .get(&req.from_addr)
        .map(|e| e.status == FriendStatus::Active)
        .unwrap_or(false);
    let entries = if is_friend {
        collect_referrable_entries(fg)
    } else {
        Vec::new()
    };
    // 3. Sign the (possibly empty) catalog, subject-bound to the requester.
    Ok(sign_referral_catalog(
        device2,
        self_owner,
        req.from_addr,
        entries,
        at,
        self_enrollment,
    ))
}

/// Inbound dispatcher for the `harmony/friend-pex/v1` ALPN. Holds the read-only
/// handles the pure serve-decision needs plus the IO plumbing. Mirrors
/// `iroh_friend_acceptor::IrohFriendHandshakeAcceptor` (same `crdt_state` type,
/// same HLC tracker, same framing) but is strictly read-only: it never writes
/// owner-state.
pub struct IrohFriendPexAcceptor {
    /// Owner-state CRDT root (SAME type the handshake acceptor holds). Read-only
    /// here: we snapshot `friend_graph` under the lock and drop the guard before
    /// any network write — the owner-state lock is NEVER held across IO.
    crdt_state: Arc<TokioMutex<OwnerState>>,
    /// Shared HLC tracker (`device_id → last Hlc`), bumped per served catalog to
    /// stamp `ReferralCatalog.at`. Same map the handshake acceptor uses.
    hlc_tracker: Arc<TokioMutex<std::collections::BTreeMap<String, Hlc>>>,
    device_id: String,
    self_owner: OwnerAddr,
    self_enrollment: EnrollmentCert,
    device2_signing_key: Arc<ed25519_dalek::SigningKey>,
    config: FriendAcceptorConfig,
}

impl IrohFriendPexAcceptor {
    /// Build a PEX acceptor with the default timeouts.
    pub fn new(
        crdt_state: Arc<TokioMutex<OwnerState>>,
        hlc_tracker: Arc<TokioMutex<std::collections::BTreeMap<String, Hlc>>>,
        device_id: String,
        self_owner: OwnerAddr,
        self_enrollment: EnrollmentCert,
        device2_signing_key: Arc<ed25519_dalek::SigningKey>,
    ) -> Self {
        Self::with_config(
            crdt_state,
            hlc_tracker,
            device_id,
            self_owner,
            self_enrollment,
            device2_signing_key,
            FriendAcceptorConfig::default(),
        )
    }

    /// Build a PEX acceptor with explicit timeouts (tests pass sub-second
    /// deadlines; production passes the handshake acceptor's `config`).
    pub fn with_config(
        crdt_state: Arc<TokioMutex<OwnerState>>,
        hlc_tracker: Arc<TokioMutex<std::collections::BTreeMap<String, Hlc>>>,
        device_id: String,
        self_owner: OwnerAddr,
        self_enrollment: EnrollmentCert,
        device2_signing_key: Arc<ed25519_dalek::SigningKey>,
        config: FriendAcceptorConfig,
    ) -> Self {
        Self {
            crdt_state,
            hlc_tracker,
            device_id,
            self_owner,
            self_enrollment,
            device2_signing_key,
            config,
        }
    }

    /// Bump-and-return a fresh HLC stamped with this device's id. Mirrors
    /// `iroh_friend_acceptor::IrohFriendHandshakeAcceptor::next_hlc`.
    async fn next_hlc(&self) -> Hlc {
        let now_ms = wall_now_ms();
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

    /// Inbound bi-stream handler: read the length-prefixed `CatalogRequest`,
    /// build the signed `ReferralCatalog` via the pure serve-decision, and write
    /// the length-prefixed catalog back. Framing mirrors
    /// `iroh_friend_acceptor::handle_friend_handshake_inbound`
    /// (`accept_bi` under `io_deadline`; `[u32 LE len][body]` with a
    /// `PEX_MAX_PACKET_LEN` bound; `send.finish()`). Any error returns `Err`,
    /// which closes the stream — serving nothing (fail closed).
    async fn serve(&self, conn: &Connection) -> Result<(), String> {
        let (mut send, mut recv) = tokio::time::timeout(self.config.io_deadline, conn.accept_bi())
            .await
            .map_err(|_| "io timeout in accept_bi".to_string())?
            .map_err(|e| format!("accept_bi failed: {e}"))?;

        // Read [u32 LE length-prefix][body].
        let mut len_buf = [0u8; 4];
        tokio::time::timeout(self.config.io_deadline, recv.read_exact(&mut len_buf))
            .await
            .map_err(|_| "io timeout reading length-prefix".to_string())?
            .map_err(|e| format!("read length-prefix: {e}"))?;
        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 || len > PEX_MAX_PACKET_LEN {
            return Err(format!(
                "length-prefix out of bounds: len={len} max={PEX_MAX_PACKET_LEN}"
            ));
        }
        let mut body = vec![0u8; len];
        tokio::time::timeout(self.config.io_deadline, recv.read_exact(&mut body))
            .await
            .map_err(|_| "io timeout reading body".to_string())?
            .map_err(|e| format!("read body: {e}"))?;

        let req = decode_catalog_request(&body).map_err(|e| format!("decode request: {e}"))?;

        // Stamp the catalog clock BEFORE taking the crdt lock so the two locks
        // (hlc_tracker, crdt_state) are never nested.
        let at = self.next_hlc().await;

        // Build the catalog under the crdt lock: snapshot the friend graph, run
        // the pure serve-decision, then DROP the guard before any network write.
        // The owner-state lock is never held across IO. Read-only: no mutation.
        let cat = {
            let state = self.crdt_state.lock().await;
            serve_catalog_for_request(
                &state.friend_graph,
                &req,
                self.self_owner,
                self.self_enrollment.clone(),
                &self.device2_signing_key,
                at,
            )
            .map_err(|e| format!("serve decision: {e}"))?
        }; // guard dropped here — owner-state lock released before the write.

        let resp = encode_referral_catalog(&cat).map_err(|e| format!("encode catalog: {e}"))?;
        if resp.len() > PEX_MAX_PACKET_LEN {
            return Err(format!(
                "response too large: len={} max={PEX_MAX_PACKET_LEN}",
                resp.len()
            ));
        }
        let resp_len = resp.len() as u32;

        // Write [u32 LE length-prefix][catalog CBOR] then finish().
        tokio::time::timeout(
            self.config.io_deadline,
            send.write_all(&resp_len.to_le_bytes()),
        )
        .await
        .map_err(|_| "io timeout writing length-prefix".to_string())?
        .map_err(|e| format!("write length-prefix: {e}"))?;
        tokio::time::timeout(self.config.io_deadline, send.write_all(&resp))
            .await
            .map_err(|_| "io timeout writing body".to_string())?
            .map_err(|e| format!("write body: {e}"))?;
        // `send.finish()` is sync — no timeout needed.
        send.finish().map_err(|e| format!("send.finish: {e}"))?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl crate::iroh_invite_acceptor::IrohHandshakeDispatcher for IrohFriendPexAcceptor {
    async fn handle_connection(&self, conn: Connection) {
        if let Err(e) = self.serve(&conn).await {
            tracing::debug!(error = %e, "friend-pex serve ended");
        }
        // Wait for the dialer to drive the close so the response bytes flush
        // before `conn` drops (same race-avoidance as the handshake acceptors).
        let _ = tokio::time::timeout(self.config.io_deadline, conn.closed()).await;
    }
}

/// Wall-clock now in epoch-milliseconds — the same one-syscall pattern
/// `iroh_friend_acceptor::wall_now_ms` uses for HLC stamping. Saturates to `0`
/// if the clock is before the epoch.
fn wall_now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_membership::mint_test_owner;
    use crate::friend_graph::{FriendEntry, FriendOrigin};

    /// Deterministic HLC for fixtures.
    fn hlc(n: u64) -> Hlc {
        Hlc {
            wall_ms: n,
            logical: 0,
            device_id: "test-device".to_string(),
        }
    }

    /// Build a FULL valid `FriendEntry`, varying only the lifecycle/opt-in/label
    /// the serve-decision keys on (mirrors `referral_catalog::tests::entry`).
    fn entry(status: FriendStatus, referrable: bool, display: Option<&str>) -> FriendEntry {
        FriendEntry {
            master_ed25519: [0x11; 32],
            display: display.map(str::to_string),
            status,
            established_via: FriendOrigin::Token,
            referrable,
            learned_at: hlc(1),
            sealed_secret: None,
        }
    }

    #[test]
    fn serves_full_catalog_to_active_friend() {
        let f = mint_test_owner(0x11); // server
        let r = mint_test_owner(0x22); // requester (an active friend of F)

        let mut fg = FriendGraph::default();
        // R is an Active friend of F (but not itself referrable — irrelevant: the
        // gate is on R being a friend, not on R being referrable).
        fg.friends
            .insert(r.owner, entry(FriendStatus::Active, false, Some("r")));
        // F has one referrable friend that should appear in the served catalog.
        fg.friends.insert(
            OwnerAddr([7; 16]),
            entry(FriendStatus::Active, true, Some("g")),
        );

        let req = sign_catalog_request(&r.device_key, r.owner, f.owner, r.cert.clone());
        let cat =
            serve_catalog_for_request(&fg, &req, f.owner, f.cert.clone(), &f.device_key, hlc(7))
                .expect("active friend is served a catalog");

        assert_eq!(
            cat.entries.len(),
            1,
            "the single referrable friend is served"
        );
        assert_eq!(cat.entries[0].peer_owner, OwnerAddr([7; 16]));
        // The catalog is validly signed by F and subject-bound to R.
        assert!(verify_referral_catalog(&cat, f.owner, r.owner).is_ok());
    }

    #[test]
    fn serves_empty_catalog_to_non_friend() {
        let f = mint_test_owner(0x11); // server
        let stranger = mint_test_owner(0x33); // authenticated, but NOT F's friend

        let mut fg = FriendGraph::default();
        // F has a referrable friend — but the stranger must NOT see it.
        fg.friends.insert(
            OwnerAddr([7; 16]),
            entry(FriendStatus::Active, true, Some("g")),
        );

        let req = sign_catalog_request(
            &stranger.device_key,
            stranger.owner,
            f.owner,
            stranger.cert.clone(),
        );
        let cat =
            serve_catalog_for_request(&fg, &req, f.owner, f.cert.clone(), &f.device_key, hlc(7))
                .expect("a non-friend still gets a (benign, empty) signed catalog");

        // SECURITY: a non-friend leaks NOTHING about F's referrable friends.
        assert!(
            cat.entries.is_empty(),
            "non-friend must not learn any referrable friends"
        );
        // The empty catalog is still validly signed + subject-bound to the
        // stranger (benign: indistinguishable from "F has no referrables").
        assert!(verify_referral_catalog(&cat, f.owner, stranger.owner).is_ok());
    }

    #[test]
    fn rejects_request_addressed_to_someone_else() {
        let f = mint_test_owner(0x11); // server
        let r = mint_test_owner(0x22);

        let fg = FriendGraph::default();
        // Request addressed to 0x99, NOT to F → must be rejected before serving.
        let req = sign_catalog_request(
            &r.device_key,
            r.owner,
            OwnerAddr([0x99; 16]),
            r.cert.clone(),
        );
        let res =
            serve_catalog_for_request(&fg, &req, f.owner, f.cert.clone(), &f.device_key, hlc(7));
        assert_eq!(
            res.unwrap_err(),
            ReferralAuthError::WrongTarget,
            "a request addressed to a different owner must be rejected"
        );
    }
}
