//! ZEB-677 S4: A-side quorum enrollment completion port.
//!
//! The seedless-inviter pairing state machine drives the quorum enrollment
//! ceremony through this narrow port so the SM stays decoupled from the
//! quorum engine — it depends only on the trait, which its unit tests mock.
//! The live impl writes an `Enrollment` request into `owner-quorum-req-v1`,
//! polls for the armed sibling's co-signature, and assembles the K=2
//! `EnrollmentCert`. Polling (rather than an `on_applied` `Notify`) keeps
//! the port self-contained: the ceremony resolves in seconds and the poll
//! is bounded by the SM's 120 s deadline.

use std::sync::Arc;
use std::time::Duration;

use harmony_owner::certs::EnrollmentCert;
use harmony_owner::pubkey_bundle::PubKeyBundle;

use crate::owner_quorum_sync::{plan_enrollment_request, try_assemble_enrollment, QuorumReqDoc};

/// Poll cadence while waiting for the sibling co-signature to merge in.
const COSIGN_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// The narrow interface the pairing SM calls to run an A-side quorum
/// enrollment. Implemented by [`LiveQuorumEnrollPort`] (real) and by a mock
/// in the SM's unit tests.
#[async_trait::async_trait]
pub trait QuorumEnrollPort: Send + Sync {
    /// Sign THIS device's part over the joiner's enrollment payload and
    /// write an `Enrollment` request addressed to an armed, active,
    /// Master-certed sibling. Returns the request id.
    async fn open_enrollment_request(
        &self,
        joiner_device_id: [u8; 16],
        joiner_pubkeys: PubKeyBundle,
        issued_at: u64,
    ) -> Result<String, String>;

    /// Poll for the sibling's co-signature (bounded by `timeout`) and
    /// assemble the K=2 `EnrollmentCert`. Prunes the request on success.
    async fn await_cosign_and_assemble(
        &self,
        request_id: String,
        timeout: Duration,
    ) -> Result<EnrollmentCert, String>;
}

/// Live port over the resident quorum + trust docs and the quorum engine.
pub struct LiveQuorumEnrollPort {
    quorum_doc: Arc<tokio::sync::Mutex<QuorumReqDoc>>,
    quorum_engine: Arc<crate::fleet_sync::FleetSyncEngine<QuorumReqDoc>>,
    trust_doc: Arc<tokio::sync::Mutex<harmony_owner::state::OwnerState>>,
    device_signing_key: ed25519_dalek::SigningKey,
    self_id: [u8; 16],
}

impl LiveQuorumEnrollPort {
    pub fn new(
        quorum_doc: Arc<tokio::sync::Mutex<QuorumReqDoc>>,
        quorum_engine: Arc<crate::fleet_sync::FleetSyncEngine<QuorumReqDoc>>,
        trust_doc: Arc<tokio::sync::Mutex<harmony_owner::state::OwnerState>>,
        device_signing_key: ed25519_dalek::SigningKey,
        self_id: [u8; 16],
    ) -> Self {
        Self {
            quorum_doc,
            quorum_engine,
            trust_doc,
            device_signing_key,
            self_id,
        }
    }
}

#[async_trait::async_trait]
impl QuorumEnrollPort for LiveQuorumEnrollPort {
    async fn open_enrollment_request(
        &self,
        joiner_device_id: [u8; 16],
        joiner_pubkeys: PubKeyBundle,
        issued_at: u64,
    ) -> Result<String, String> {
        let now_ms = issued_at.saturating_mul(1000);
        let trust_snapshot = self.trust_doc.lock().await.clone();
        let mut request_id = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut request_id);
        let (id_hex, request) = {
            let doc = self.quorum_doc.lock().await;
            plan_enrollment_request(
                &trust_snapshot,
                &doc.enroll_arms,
                &self.device_signing_key,
                self.self_id,
                joiner_device_id,
                &joiner_pubkeys,
                issued_at,
                now_ms,
                request_id,
            )?
        };
        {
            let mut doc = self.quorum_doc.lock().await;
            doc.requests.insert(id_hex.clone(), request);
        }
        self.quorum_engine.notify_dirty();
        if let Err(e) = self.quorum_engine.flush_now().await {
            tracing::warn!(error = %e, "open_enrollment_request: quorum flush failed; dirty latch will retry");
        }
        Ok(id_hex)
    }

    async fn await_cosign_and_assemble(
        &self,
        request_id: String,
        timeout: Duration,
    ) -> Result<EnrollmentCert, String> {
        let start = std::time::Instant::now();
        loop {
            // Re-snapshot trust EACH poll: a cosigner revoked mid-ceremony
            // must be rejected when its co-signature merges in, so
            // `try_assemble_enrollment`'s `is_revoked` check sees current
            // state, not a stale pre-loop snapshot. (Clone-then-release, so
            // the trust and quorum locks are never held together.)
            let trust_snapshot = self.trust_doc.lock().await.clone();
            let outcome = {
                let doc = self.quorum_doc.lock().await;
                if !doc.requests.contains_key(&request_id) {
                    None
                } else {
                    Some(try_assemble_enrollment(
                        &doc,
                        &trust_snapshot,
                        self.self_id,
                        &request_id,
                    ))
                }
            };
            match outcome {
                None => {
                    return Err(
                        "requestGone: the enrollment request expired or was removed before a \
                         co-signature arrived"
                            .to_string(),
                    )
                }
                Some(Some(cert)) => {
                    self.remove_request(&request_id).await;
                    return Ok(cert);
                }
                Some(None) => {
                    if start.elapsed() >= timeout {
                        // Convergently abandon the request so a delayed sibling
                        // does not later co-sign + vouch + burn its single-use
                        // arm on a ceremony the inviter has already given up on.
                        // A plain delete is NOT convergent (the grow-only union
                        // re-merges it from a lagging replica), so we write a
                        // signed `declined_by[self]` marker instead (Greptile).
                        self.abandon_request(&request_id).await;
                        return Err(
                            "cosignTimeout: your other device didn't co-sign in time — re-arm \
                             and retry"
                                .to_string(),
                        );
                    }
                    tokio::time::sleep(COSIGN_POLL_INTERVAL).await;
                }
            }
        }
    }
}

impl LiveQuorumEnrollPort {
    /// Drop a request from the resident quorum doc and publish the removal
    /// (best-effort flush — the dirty latch retries). Used on SUCCESS: the
    /// cosigner has already consumed its single-use arm, so a re-merge cannot
    /// drive a second co-sign, and this is just local cleanup.
    async fn remove_request(&self, request_id: &str) {
        {
            let mut doc = self.quorum_doc.lock().await;
            doc.requests.remove(request_id);
        }
        self.quorum_engine.notify_dirty();
        if let Err(e) = self.quorum_engine.flush_now().await {
            tracing::warn!(error = %e, "quorum enroll: request-removal flush failed; dirty latch will retry");
        }
    }

    /// CONVERGENTLY abandon a request on timeout: sign an abandon marker into
    /// `declined_by[self]` (the initiator). Unlike a delete, the grow-only
    /// `declined_by` union carries the marker to every replica, so a lagging
    /// sibling re-merging the stale request still sees it as abandoned
    /// (`initiator_abandoned`) and does not co-sign it.
    async fn abandon_request(&self, request_id: &str) {
        let owner_id = self.trust_doc.lock().await.owner_id;
        let payload = crate::owner_quorum_sync::decline_signing_payload(owner_id, request_id);
        let sig = harmony_owner::signing::sign_with_tag(
            &self.device_signing_key,
            harmony_owner::signing::tags::REVOCATION,
            &payload,
        );
        {
            let mut doc = self.quorum_doc.lock().await;
            if let Some(req) = doc.requests.get_mut(request_id) {
                req.declined_by
                    .entry(hex::encode(self.self_id))
                    .or_insert_with(|| hex::encode(sig));
            }
        }
        self.quorum_engine.notify_dirty();
        if let Err(e) = self.quorum_engine.flush_now().await {
            tracing::warn!(error = %e, "quorum enroll: abandon flush failed; dirty latch will retry");
        }
    }
}
