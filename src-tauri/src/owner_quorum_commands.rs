//! ZEB-677 S3: quorum revocation ceremony IPCs — `request_quorum_revocation`
//! / `cosign_quorum_request` / `decline_quorum_request`. The pure planner
//! and the doc-mutating cores are NodeState-free (unit- and
//! integration-testable); the `_inner` seams follow `revoke_device_inner`
//! (keychain injected via `KeychainFactory`, ZEB-428), and the `_impl`
//! seams mirror `revoke_device_impl` for the RPC/IPC split (ZEB-445).
//!
//! Ceremony shape (spec §4 + `owner_quorum_sync` module docs): the S1
//! payload binds the signer set, so every detached signature covers the
//! sorted pair `[initiator, cosigner]`. The initiator pre-signs one
//! payload per eligible cosigner at creation (authenticating the request);
//! a co-signer verifies that part before adding its own; the initiator's
//! completion sweep (`owner_quorum_sync::run_quorum_sweep`) assembles and
//! applies the cert through the trust doc's validating `add_revocation`.

use crate::identity_commands::run_blocking;
use crate::owner_commands::{parse_revoke_reason, prod_keychain, KeychainFactory};
use crate::owner_quorum_sync::{
    parse_device_id_hex, revocation_pair_payload, QuorumReqDoc, QuorumRequest, QuorumRequestKind,
    QuorumRequestSigs, MAX_QUORUM_REQUESTS, QUORUM_REVOCATION_TTL_MS,
};
use crate::owner_state::load_owner_state;
use crate::owner_state_types::Hlc;
use harmony_owner::certs::{EnrollmentIssuer, RevocationCert};
use harmony_owner::signing::{tags, verify_with_tag};
use harmony_owner::state::OwnerState;
use harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Depth-1 policy: quorum signers must hold Master-issued enrollments.
pub(crate) fn is_master_issued(cert: &harmony_owner::certs::EnrollmentCert) -> bool {
    matches!(cert.issuer, EnrollmentIssuer::Master { .. })
}

/// Devices that can co-sign a request from `self_id` against `target`:
/// active (liveness within the 90-day window), Master-issued enrollment,
/// and neither the initiator nor the target.
pub(crate) fn eligible_cosigners(
    trust: &OwnerState,
    now_secs: u64,
    self_id: [u8; 16],
    target: [u8; 16],
) -> Vec<[u8; 16]> {
    trust
        .active_devices(now_secs, DEFAULT_ACTIVE_WINDOW_SECS)
        .into_iter()
        .filter(|id| *id != self_id && *id != target)
        .filter(|id| trust.enrollments.get(id).is_some_and(is_master_issued))
        .collect()
}

/// Pure request planner: validates against a trust snapshot and builds the
/// request with the initiator's per-candidate pair signatures. No I/O, no
/// locks. `request_id` is caller-supplied (16 random bytes) so the planner
/// stays deterministic under test.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_quorum_revocation_request(
    trust: &OwnerState,
    device_signing_key: &ed25519_dalek::SigningKey,
    master_seed_present: bool,
    device_vk_hex: &str,
    reason_str: &str,
    now_secs: u64,
    now_ms: u64,
    request_id: [u8; 16],
    // ZEB-677 S5 — the fleet's current KeyTree epoch, so the request can carry
    // the pre-built next-epoch carrier doc (bundled crypto cutoff, §7). `None`
    // when the node isn't carrying fleet keys → revoke-only.
    current_fleet_epoch: Option<u32>,
) -> Result<(String, QuorumRequest), String> {
    if master_seed_present {
        return Err(
            "hasMaster: this device holds the master key — remove the device directly".to_string(),
        );
    }
    let reason = parse_revoke_reason(reason_str)?;
    let vk_bytes: [u8; 32] = hex::decode(device_vk_hex)
        .map_err(|e| format!("badDeviceVk: {e}"))?
        .try_into()
        .map_err(|_| "badDeviceVk: expected 32 bytes".to_string())?;
    let target = trust
        .enrollments
        .values()
        .find(|c| c.device_pubkeys.classical.ed25519_verify == vk_bytes)
        .map(|c| c.device_id)
        .ok_or_else(|| "unknownDevice: no enrollment matches that key".to_string())?;
    let self_id = crate::owner_state::device_id_from_signing_key(device_signing_key);
    if target == self_id {
        return Err(
            "selfTarget: use Remove this device — self-removal needs no co-sign".to_string(),
        );
    }
    if trust.is_revoked(target) {
        return Err("alreadyRevoked: that device is already removed".to_string());
    }
    let self_cert = trust.enrollments.get(&self_id).ok_or_else(|| {
        "notEnrolled: this device has no enrollment in the trust state".to_string()
    })?;
    if !is_master_issued(self_cert) {
        return Err(
            "notEligible: this device's enrollment is not master-issued, so it cannot \
             sign a co-sign request"
                .to_string(),
        );
    }
    let mut cosigners = eligible_cosigners(trust, now_secs, self_id, target);
    if cosigners.is_empty() {
        return Err(
            "noQuorum: no other active device with a master-issued enrollment can co-sign"
                .to_string(),
        );
    }
    // The merge drops requests whose per-device maps exceed
    // MAX_QUORUM_SIG_ENTRIES (`within_caps`), so an uncapped candidate set
    // in a 17+-device fleet would make the request unreplicable. Keep the
    // most-recently-live candidates (Qodo PR #459 round 1).
    if cosigners.len() > crate::owner_quorum_sync::MAX_QUORUM_SIG_ENTRIES {
        cosigners.sort_by_key(|id| {
            std::cmp::Reverse(trust.liveness.get(id).map(|l| l.timestamp).unwrap_or(0))
        });
        cosigners.truncate(crate::owner_quorum_sync::MAX_QUORUM_SIG_ENTRIES);
    }
    let owner_id = trust.owner_id;
    let mut initiator_sigs = std::collections::BTreeMap::new();
    for cosigner in cosigners {
        let payload =
            revocation_pair_payload(owner_id, target, now_secs, &reason, self_id, cosigner)?;
        initiator_sigs.insert(
            hex::encode(cosigner),
            hex::encode(RevocationCert::sign_quorum_part(
                device_signing_key,
                &payload,
            )),
        );
    }
    // ZEB-677 S5 — bundle the pre-built UNSIGNED next-epoch carrier doc (target
    // excluded from the sealed set) when the node is carrying fleet keys. The
    // co-signer signs its hash too, so a single approval yields both the
    // RevocationCert quorum and the crypto cutoff. The initiator ALSO signs the
    // doc so a co-signer can bind it to this request (Qodo PR #461).
    let (epoch_doc_cbor_hex, epoch_doc_initiator_sig_hex) = match current_fleet_epoch {
        Some(epoch) => {
            let (unsigned, _kt) = crate::owner_commands::plan_fleet_epoch_bump_quorum(
                trust,
                epoch,
                now_ms,
                Some(target),
            )?;
            let bytes = crate::owner_state_crypto::canonical_cbor_encode(&unsigned)
                .map_err(|e| format!("encode bundled epoch doc: {e}"))?;
            let initiator_sig = unsigned
                .quorum_part_over(device_signing_key)
                .map_err(|e| format!("sign bundled epoch doc: {e}"))?;
            (Some(hex::encode(bytes)), Some(hex::encode(initiator_sig)))
        }
        None => (None, None),
    };
    let self_hex = hex::encode(self_id);
    let request = QuorumRequest {
        created_at: Hlc {
            wall_ms: now_ms,
            logical: 0,
            device_id: self_hex.clone(),
        },
        declined_by: Default::default(),
        initiator_hex: self_hex,
        kind: QuorumRequestKind::Revocation {
            reason: reason_str.to_string(),
            target_hex: hex::encode(target),
            epoch_doc_cbor_hex,
            epoch_doc_initiator_sig_hex,
        },
        initiator_sigs,
        signatures: Default::default(),
        issued_at: now_secs,
        expires_at_ms: now_ms.saturating_add(QUORUM_REVOCATION_TTL_MS),
    };
    Ok((hex::encode(request_id), request))
}

/// ZEB-677 S5 — pure planner for a STANDALONE quorum fleet-epoch rotation (no
/// revocation), the `fleetEpochStale` retry surface on a master-less fleet.
/// Builds the unsigned next-epoch carrier doc (all survivors) and pre-signs its
/// hash per eligible co-signer so B can authenticate the request before adding
/// its own part into `primary_sig_hex`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_quorum_epoch_bump_request(
    trust: &OwnerState,
    device_signing_key: &ed25519_dalek::SigningKey,
    master_seed_present: bool,
    current_fleet_epoch: u32,
    now_secs: u64,
    now_ms: u64,
    request_id: [u8; 16],
) -> Result<(String, QuorumRequest), String> {
    if master_seed_present {
        return Err(
            "hasMaster: this device holds the master key — rotate fleet keys directly".to_string(),
        );
    }
    let self_id = crate::owner_state::device_id_from_signing_key(device_signing_key);
    let self_cert = trust.enrollments.get(&self_id).ok_or_else(|| {
        "notEnrolled: this device has no enrollment in the trust state".to_string()
    })?;
    if !is_master_issued(self_cert) {
        return Err(
            "notEligible: this device's enrollment is not master-issued, so it cannot rotate \
             fleet keys via quorum"
                .to_string(),
        );
    }
    // Eligible co-signers = active Master-issued devices other than self
    // (passing `self_id` as the "target" collapses the two exclusions to one).
    let mut cosigners = eligible_cosigners(trust, now_secs, self_id, self_id);
    if cosigners.is_empty() {
        return Err(
            "noQuorum: no other active device with a master-issued enrollment can co-sign"
                .to_string(),
        );
    }
    if cosigners.len() > crate::owner_quorum_sync::MAX_QUORUM_SIG_ENTRIES {
        cosigners.sort_by_key(|id| {
            std::cmp::Reverse(trust.liveness.get(id).map(|l| l.timestamp).unwrap_or(0))
        });
        cosigners.truncate(crate::owner_quorum_sync::MAX_QUORUM_SIG_ENTRIES);
    }
    let (unsigned, _kt) = crate::owner_commands::plan_fleet_epoch_bump_quorum(
        trust,
        current_fleet_epoch,
        now_ms,
        None,
    )?;
    let epoch_doc_cbor_hex = hex::encode(
        crate::owner_state_crypto::canonical_cbor_encode(&unsigned)
            .map_err(|e| format!("encode epoch doc: {e}"))?,
    );
    // A's part over the epoch-doc hash authenticates the request to co-signers.
    let a_epoch_sig = hex::encode(
        unsigned
            .quorum_part_over(device_signing_key)
            .map_err(|e| format!("sign epoch doc: {e}"))?,
    );
    let mut initiator_sigs = std::collections::BTreeMap::new();
    for cosigner in cosigners {
        initiator_sigs.insert(hex::encode(cosigner), a_epoch_sig.clone());
    }
    let self_hex = hex::encode(self_id);
    let request = QuorumRequest {
        created_at: Hlc {
            wall_ms: now_ms,
            logical: 0,
            device_id: self_hex.clone(),
        },
        declined_by: Default::default(),
        initiator_hex: self_hex,
        kind: QuorumRequestKind::EpochBump { epoch_doc_cbor_hex },
        initiator_sigs,
        signatures: Default::default(),
        issued_at: now_secs,
        expires_at_ms: now_ms.saturating_add(QUORUM_REVOCATION_TTL_MS),
    };
    Ok((hex::encode(request_id), request))
}

/// Doc-mutating core for co-sign: validate + sign + union. Returns
/// `Ok(true)` when a signature was added, `Ok(false)` when this device had
/// already signed (idempotent re-approve). NodeState-free for the
/// two-engine integration tests.
pub(crate) fn cosign_request_core(
    doc: &mut QuorumReqDoc,
    trust: &OwnerState,
    device_signing_key: &ed25519_dalek::SigningKey,
    self_id: [u8; 16],
    request_id: &str,
    now_ms: u64,
) -> Result<bool, String> {
    let self_hex = hex::encode(self_id);
    let req = doc
        .requests
        .get_mut(request_id)
        .ok_or_else(|| "unknownRequest: no pending request with that id".to_string())?;
    // ZEB-677 S5 — standalone quorum epoch bump: verify the initiator's part
    // over the epoch-doc hash, then sign it into `primary_sig_hex`.
    if let QuorumRequestKind::EpochBump { epoch_doc_cbor_hex } = &req.kind {
        let epoch_hex = epoch_doc_cbor_hex.clone();
        if now_ms > req.expires_at_ms {
            return Err(
                "expired: this request has expired — ask the other device to retry".to_string(),
            );
        }
        let decliners = crate::owner_quorum_sync::verified_decliners(trust, request_id, req);
        if decliners.contains(&self_hex) {
            return Err("declined: this device already declined the request".to_string());
        }
        if !decliners.is_empty() {
            return Err("declined: another device declined the request".to_string());
        }
        if req.signatures.contains_key(&self_hex) {
            return Ok(false);
        }
        if req.initiator_hex == self_hex {
            return Err(
                "ownRequest: the requesting device cannot co-sign its own request".to_string(),
            );
        }
        let self_cert = trust.enrollments.get(&self_id).ok_or_else(|| {
            "notEnrolled: this device has no enrollment in the trust state".to_string()
        })?;
        if !is_master_issued(self_cert) {
            return Err(
                "notEligible: this device's enrollment is not master-issued, so it cannot co-sign"
                    .to_string(),
            );
        }
        let initiator = parse_device_id_hex(&req.initiator_hex)?;
        let initiator_cert = trust
            .enrollments
            .get(&initiator)
            .ok_or_else(|| "unknownInitiator: the requesting device is not enrolled".to_string())?;
        if !is_master_issued(initiator_cert) {
            return Err(
                "initiatorNotEligible: the requesting device's enrollment is not master-issued"
                    .to_string(),
            );
        }
        if trust.is_revoked(initiator) {
            return Err("initiatorRevoked: the requesting device has been removed".to_string());
        }
        let a_sig_hex = req
            .initiator_sigs
            .get(&self_hex)
            .ok_or_else(|| {
                "notAddressed: this request has no co-sign slot for this device".to_string()
            })?
            .clone();
        let bytes = hex::decode(&epoch_hex).map_err(|e| format!("badEpochDoc: not hex ({e})"))?;
        let unsigned: crate::fleet_key_epoch::FleetKeyEpochDoc =
            crate::owner_state_crypto::canonical_cbor_decode(&bytes)
                .map_err(|e| format!("badEpochDoc: decode ({e})"))?;
        let a_vk = ed25519_dalek::VerifyingKey::from_bytes(
            &initiator_cert.device_pubkeys.classical.ed25519_verify,
        )
        .map_err(|_| "badInitiatorSig: initiator enrollment carries an unusable key".to_string())?;
        let a_sig =
            hex::decode(&a_sig_hex).map_err(|e| format!("badInitiatorSig: not hex ({e})"))?;
        if !unsigned.verify_quorum_part(&a_vk, &a_sig) {
            return Err(
                "badInitiatorSig: initiator epoch-doc signature failed verification".to_string(),
            );
        }
        let own_sig = unsigned
            .quorum_part_over(device_signing_key)
            .map_err(|e| format!("signEpochDoc: {e}"))?;
        req.signatures.insert(
            self_hex,
            QuorumRequestSigs {
                epoch_doc_sig_hex: None,
                primary_sig_hex: hex::encode(own_sig),
            },
        );
        return Ok(true);
    }
    // Enrollment requests are co-signed automatically by an armed sibling
    // (the sweep), never through this manual IPC.
    let QuorumRequestKind::Revocation {
        reason,
        target_hex,
        epoch_doc_cbor_hex,
        epoch_doc_initiator_sig_hex,
    } = &req.kind
    else {
        return Err(
            "notRevocation: enrollment requests are co-signed automatically by an armed device"
                .to_string(),
        );
    };
    // ZEB-677 S5 — clone the optional bundled epoch doc + its initiator binding
    // up front so their later use doesn't hold an immutable borrow of `req`
    // across the signature insert.
    let epoch_doc_hex = epoch_doc_cbor_hex.clone();
    let epoch_doc_initiator_sig = epoch_doc_initiator_sig_hex.clone();
    if now_ms > req.expires_at_ms {
        return Err(
            "expired: this request has expired — ask the other device to retry".to_string(),
        );
    }
    // Only VERIFIED declines count (a forged entry naming this device must
    // not block its co-sign; a real decline from any eligible voter kills
    // the request for everyone).
    let decliners = crate::owner_quorum_sync::verified_decliners(trust, request_id, req);
    if decliners.contains(&self_hex) {
        return Err("declined: this device already declined the request".to_string());
    }
    if !decliners.is_empty() {
        return Err("declined: another device declined the request".to_string());
    }
    if req.signatures.contains_key(&self_hex) {
        return Ok(false);
    }
    if req.initiator_hex == self_hex {
        return Err("ownRequest: the requesting device cannot co-sign its own request".to_string());
    }
    let target = parse_device_id_hex(target_hex)?;
    if target == self_id {
        return Err(
            "selfTarget: this request removes this device — decline it instead".to_string(),
        );
    }
    let reason_parsed = parse_revoke_reason(reason)?;
    let self_cert = trust.enrollments.get(&self_id).ok_or_else(|| {
        "notEnrolled: this device has no enrollment in the trust state".to_string()
    })?;
    if !is_master_issued(self_cert) {
        return Err(
            "notEligible: this device's enrollment is not master-issued, so it cannot co-sign"
                .to_string(),
        );
    }
    let initiator = parse_device_id_hex(&req.initiator_hex)?;
    let initiator_cert = trust
        .enrollments
        .get(&initiator)
        .ok_or_else(|| "unknownInitiator: the requesting device is not enrolled".to_string())?;
    if !is_master_issued(initiator_cert) {
        return Err(
            "initiatorNotEligible: the requesting device's enrollment is not master-issued"
                .to_string(),
        );
    }
    if trust.is_revoked(initiator) {
        return Err("initiatorRevoked: the requesting device has been removed".to_string());
    }
    if !trust.enrollments.contains_key(&target) {
        return Err("unknownDevice: the target device is not enrolled".to_string());
    }
    if trust.is_revoked(target) {
        return Err("alreadyRevoked: that device is already removed".to_string());
    }
    let initiator_sig_hex = req.initiator_sigs.get(&self_hex).ok_or_else(|| {
        "notAddressed: this request has no co-sign slot for this device".to_string()
    })?;
    let payload = revocation_pair_payload(
        trust.owner_id,
        target,
        req.issued_at,
        &reason_parsed,
        initiator,
        self_id,
    )?;
    let initiator_vk = ed25519_dalek::VerifyingKey::from_bytes(
        &initiator_cert.device_pubkeys.classical.ed25519_verify,
    )
    .map_err(|_| "badInitiatorSig: initiator enrollment carries an unusable key".to_string())?;
    let initiator_sig =
        hex::decode(initiator_sig_hex).map_err(|e| format!("badInitiatorSig: not hex ({e})"))?;
    verify_with_tag(
        &initiator_vk,
        tags::REVOCATION,
        &payload,
        &initiator_sig,
        "Revocation-Quorum-Part",
    )
    .map_err(|e| format!("badInitiatorSig: {e}"))?;
    let own_sig = RevocationCert::sign_quorum_part(device_signing_key, &payload);
    // ZEB-677 S5 — if the request bundles a next-epoch carrier doc, produce the
    // SECOND detached signature over its hash. One approval → revoke + cutoff.
    // BUT first bind the doc to the initiator: verify the initiator's own part
    // over the exact epoch-doc bytes, so a replicated-doc write cannot swap in
    // a different epoch doc for this device to bless (Qodo PR #461).
    let epoch_doc_sig_hex = match &epoch_doc_hex {
        Some(hex_doc) => {
            let bytes = hex::decode(hex_doc).map_err(|e| format!("badEpochDoc: not hex ({e})"))?;
            let unsigned: crate::fleet_key_epoch::FleetKeyEpochDoc =
                crate::owner_state_crypto::canonical_cbor_decode(&bytes)
                    .map_err(|e| format!("badEpochDoc: decode ({e})"))?;
            let a_sig_hex = epoch_doc_initiator_sig.as_deref().ok_or_else(|| {
                "badEpochDoc: bundled epoch doc has no initiator signature".to_string()
            })?;
            let a_sig = hex::decode(a_sig_hex)
                .map_err(|e| format!("badEpochDoc: initiator sig not hex ({e})"))?;
            if !unsigned.verify_quorum_part(&initiator_vk, &a_sig) {
                return Err(
                    "badEpochDoc: initiator signature does not match the bundled epoch doc"
                        .to_string(),
                );
            }
            let epoch_sig = unsigned
                .quorum_part_over(device_signing_key)
                .map_err(|e| format!("badEpochDoc: sign ({e})"))?;
            Some(hex::encode(epoch_sig))
        }
        None => None,
    };
    req.signatures.insert(
        self_hex,
        QuorumRequestSigs {
            epoch_doc_sig_hex,
            primary_sig_hex: hex::encode(own_sig),
        },
    );
    Ok(true)
}

/// Doc-mutating core for decline: a SIGNED veto (unsigned entries never
/// count — see `owner_quorum_sync::verified_decliners`). Returns
/// `Ok(true)` when the tombstone was added, `Ok(false)` when already
/// declined (idempotent).
pub(crate) fn decline_request_core(
    doc: &mut QuorumReqDoc,
    trust: &OwnerState,
    device_signing_key: &ed25519_dalek::SigningKey,
    self_id: [u8; 16],
    request_id: &str,
) -> Result<bool, String> {
    let self_hex = hex::encode(self_id);
    let req = doc
        .requests
        .get_mut(request_id)
        .ok_or_else(|| "unknownRequest: no pending request with that id".to_string())?;
    if req.initiator_hex == self_hex {
        return Err(
            "ownRequest: the requesting device cannot decline its own request — it expires on \
             its own after 24 hours"
                .to_string(),
        );
    }
    if req.declined_by.contains_key(&self_hex) {
        return Ok(false);
    }
    let payload = crate::owner_quorum_sync::decline_signing_payload(trust.owner_id, request_id);
    let sig = harmony_owner::signing::sign_with_tag(device_signing_key, tags::REVOCATION, &payload);
    req.declined_by.insert(self_hex, hex::encode(sig));
    Ok(true)
}

/// Snapshot the resident handles the ceremony IPCs need. All three
/// require the node running with an owner loaded.
type QuorumHandles = (
    std::sync::Arc<tokio::sync::Mutex<QuorumReqDoc>>,
    std::sync::Arc<crate::fleet_sync::FleetSyncEngine<QuorumReqDoc>>,
    std::sync::Arc<tokio::sync::Mutex<OwnerState>>,
    std::path::PathBuf,
);

fn snapshot_handles(state: &Mutex<crate::NodeState>) -> Result<QuorumHandles, String> {
    let g = state
        .lock()
        .map_err(|e| format!("NodeState poisoned: {e}"))?;
    let (Some(doc), Some(engine), Some(trust_doc)) = (
        g.owner_quorum_doc.clone(),
        g.owner_quorum_sync.clone(),
        g.owner_trust_doc.clone(),
    ) else {
        return Err(
            "nodeNotRunning: start the node to run a co-sign ceremony (requests must replicate \
             to your other devices)"
                .to_string(),
        );
    };
    let dir = match g.identity_dir.clone() {
        Some(d) => d,
        None => crate::owner_commands::resolve_identity_dir()?,
    };
    Ok((doc, engine, trust_doc, dir))
}

/// Load device keys off the blocking pool under the owner-state write lock
/// (donor: `revoke_device_inner`).
async fn load_keys(
    dir: std::path::PathBuf,
    keychain: KeychainFactory,
) -> Result<crate::owner_state::LoadedOwnerState, String> {
    run_blocking(move || {
        let _guard = crate::owner_commands::OWNER_STATE_WRITE_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        load_owner_state(&dir, keychain())
    })
    .await?
    .ok_or_else(|| "noOwner: no owner identity on this device".to_string())
}

/// Flush best-effort: the mutation is already durable in the resident doc
/// and the dirty latch retries the publish+persist.
async fn flush_warn_only(engine: &crate::fleet_sync::FleetSyncEngine<QuorumReqDoc>, what: &str) {
    if let Err(e) = engine.flush_now().await {
        tracing::warn!(error = %e, "{what}: quorum flush failed; dirty latch will retry");
    }
}

pub(crate) async fn request_quorum_revocation_inner(
    state: &Mutex<crate::NodeState>,
    keychain: KeychainFactory,
    emit: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
    device_vk_hex: String,
    reason: String,
) -> Result<String, String> {
    let (doc, engine, trust_doc, dir) = snapshot_handles(state)?;
    // ZEB-677 S5 — the fleet's current epoch, so the request bundles the
    // pre-built next-epoch carrier doc (crypto cutoff). `None` when the node
    // isn't carrying fleet keys → revoke-only.
    let (carrier_doc_opt, fleet_keys_opt) = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (g.fleet_key_epoch_doc.clone(), g.fleet_keys.clone())
    };
    let current_fleet_epoch: Option<u32> = match (&carrier_doc_opt, &fleet_keys_opt) {
        (Some(carrier), Some(keys)) => Some(carrier.lock().await.epoch.max(keys.newest().epoch())),
        _ => None,
    };
    let loaded = load_keys(dir, keychain).await?;
    let trust_snapshot = trust_doc.lock().await.clone();
    let mut request_id = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut request_id);
    let (id_hex, request) = plan_quorum_revocation_request(
        &trust_snapshot,
        &loaded.device_signing_key,
        loaded.master_seed.is_some(),
        &device_vk_hex,
        &reason,
        now_unix_secs(),
        now_unix_ms(),
        request_id,
        current_fleet_epoch,
    )?;
    {
        let mut g = doc.lock().await;
        // Sweep-on-write: settled residue (expired / already-revoked
        // targets) must not trip the duplicate or cap checks below.
        let now_ms = now_unix_ms();
        crate::owner_quorum_sync::prune_settled_requests(&mut g, &trust_snapshot, now_ms);
        // One live request per target: a duplicate would double the banner
        // on every sibling and complete idempotently anyway. A request
        // dead under a VERIFIED decline doesn't block a retry.
        let QuorumRequestKind::Revocation { target_hex, .. } = &request.kind else {
            return Err("internal: request_quorum_revocation must build a revocation".to_string());
        };
        let duplicate = g.requests.iter().any(|(rid, r)| {
            // Only another live revocation for the same target is a duplicate;
            // enrollment requests never collide with a revocation target.
            let QuorumRequestKind::Revocation {
                target_hex: existing,
                ..
            } = &r.kind
            else {
                return false;
            };
            existing == target_hex
                && now_ms <= r.expires_at_ms
                && crate::owner_quorum_sync::verified_decliners(&trust_snapshot, rid, r).is_empty()
        });
        if duplicate {
            return Err(
                "duplicateRequest: a co-sign request for that device is already pending"
                    .to_string(),
            );
        }
        if g.requests.len() >= MAX_QUORUM_REQUESTS {
            return Err(
                "tooManyRequests: too many pending co-sign requests — wait for them to expire"
                    .to_string(),
            );
        }
        g.requests.insert(id_hex.clone(), request);
    }
    engine.notify_dirty();
    flush_warn_only(&engine, "request_quorum_revocation").await;
    emit("owner-quorum-updated");
    Ok(id_hex)
}

/// ZEB-677 S5 — open a STANDALONE quorum fleet-epoch rotation request (the
/// `fleetEpochStale` retry surface on a master-less fleet). Requires the node
/// to be carrying fleet keys; errors on a master-holding device (use the
/// direct `bump_fleet_epoch`).
pub(crate) async fn request_quorum_epoch_bump_inner(
    state: &Mutex<crate::NodeState>,
    keychain: KeychainFactory,
    emit: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<String, String> {
    let (doc, engine, trust_doc, dir) = snapshot_handles(state)?;
    let (carrier_doc_opt, fleet_keys_opt) = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (g.fleet_key_epoch_doc.clone(), g.fleet_keys.clone())
    };
    let current_fleet_epoch = match (&carrier_doc_opt, &fleet_keys_opt) {
        (Some(carrier), Some(keys)) => carrier.lock().await.epoch.max(keys.newest().epoch()),
        _ => {
            return Err(
                "noFleetKeys: this node is not carrying fleet keys; nothing to rotate".to_string(),
            )
        }
    };
    let loaded = load_keys(dir, keychain).await?;
    let trust_snapshot = trust_doc.lock().await.clone();
    let mut request_id = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut request_id);
    let (id_hex, request) = plan_quorum_epoch_bump_request(
        &trust_snapshot,
        &loaded.device_signing_key,
        loaded.master_seed.is_some(),
        current_fleet_epoch,
        now_unix_secs(),
        now_unix_ms(),
        request_id,
    )?;
    {
        let mut g = doc.lock().await;
        let now_ms = now_unix_ms();
        crate::owner_quorum_sync::prune_settled_requests(&mut g, &trust_snapshot, now_ms);
        // One live rotation at a time (a declined one is already pruned above).
        let already_rotating = g.requests.values().any(|r| {
            matches!(r.kind, QuorumRequestKind::EpochBump { .. }) && now_ms <= r.expires_at_ms
        });
        if already_rotating {
            return Err(
                "alreadyRotating: a fleet-key rotation is already pending co-signature".to_string(),
            );
        }
        if g.requests.len() >= MAX_QUORUM_REQUESTS {
            return Err(
                "tooManyRequests: too many pending co-sign requests — wait for them to expire"
                    .to_string(),
            );
        }
        g.requests.insert(id_hex.clone(), request);
    }
    engine.notify_dirty();
    flush_warn_only(&engine, "request_quorum_epoch_bump").await;
    emit("owner-quorum-updated");
    Ok(id_hex)
}

pub(crate) async fn cosign_quorum_request_inner(
    state: &Mutex<crate::NodeState>,
    keychain: KeychainFactory,
    emit: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
    request_id: String,
) -> Result<(), String> {
    let (doc, engine, trust_doc, dir) = snapshot_handles(state)?;
    let loaded = load_keys(dir, keychain).await?;
    let self_id = crate::owner_state::device_id_from_signing_key(&loaded.device_signing_key);
    let trust_snapshot = trust_doc.lock().await.clone();
    let signed = {
        let mut g = doc.lock().await;
        cosign_request_core(
            &mut g,
            &trust_snapshot,
            &loaded.device_signing_key,
            self_id,
            &request_id,
            now_unix_ms(),
        )?
    };
    if signed {
        engine.notify_dirty();
        flush_warn_only(&engine, "cosign_quorum_request").await;
        emit("owner-quorum-updated");
    }
    Ok(())
}

pub(crate) async fn decline_quorum_request_inner(
    state: &Mutex<crate::NodeState>,
    keychain: KeychainFactory,
    emit: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
    request_id: String,
) -> Result<(), String> {
    let (doc, engine, trust_doc, dir) = snapshot_handles(state)?;
    let loaded = load_keys(dir, keychain).await?;
    let self_id = crate::owner_state::device_id_from_signing_key(&loaded.device_signing_key);
    let trust_snapshot = trust_doc.lock().await.clone();
    let declined = {
        let mut g = doc.lock().await;
        decline_request_core(
            &mut g,
            &trust_snapshot,
            &loaded.device_signing_key,
            self_id,
            &request_id,
        )?
    };
    if declined {
        engine.notify_dirty();
        flush_warn_only(&engine, "decline_quorum_request").await;
        emit("owner-quorum-updated");
    }
    Ok(())
}

/// Write (or supersede) THIS device's arm cell, then publish. Flush is
/// best-effort — the replicated doc + dirty latch is the durability
/// boundary (same as every other quorum write path).
async fn write_arm_cell(
    doc: &std::sync::Arc<tokio::sync::Mutex<QuorumReqDoc>>,
    engine: &crate::fleet_sync::FleetSyncEngine<QuorumReqDoc>,
    self_id: [u8; 16],
    armed_until_ms: u64,
    now_ms: u64,
) {
    {
        let mut g = doc.lock().await;
        crate::owner_quorum_sync::stamp_arm_cell(&mut g, self_id, armed_until_ms, now_ms);
    }
    engine.notify_dirty();
    flush_warn_only(engine, "arm_quorum_enrollment").await;
}

/// Arm a 15-minute single-use enrollment co-sign window on THIS device
/// (spec §5.1). Only a master-less device arms — a master-holding device
/// adds devices through normal pairing.
pub(crate) async fn arm_quorum_enrollment_inner(
    state: &Mutex<crate::NodeState>,
    keychain: KeychainFactory,
    emit: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<u64, String> {
    let (doc, engine, trust_doc, dir) = snapshot_handles(state)?;
    let loaded = load_keys(dir, keychain).await?;
    if loaded.master_seed.is_some() {
        return Err(
            "hasMaster: this device holds the master key — use normal pairing to add a device"
                .to_string(),
        );
    }
    let self_id = crate::owner_state::device_id_from_signing_key(&loaded.device_signing_key);
    // Depth-1: a device that is not an active Master-certed member can never
    // be a quorum signer, so its arm would be dead weight (the sweep and the
    // enrollment planner both skip it). Reject at the IPC boundary — the UI
    // gates this via `canArmEnrollment`, but a direct RPC caller does not.
    {
        let trust = trust_doc.lock().await;
        let eligible = trust
            .enrollments
            .get(&self_id)
            .is_some_and(is_master_issued)
            && !trust.is_revoked(self_id);
        if !eligible {
            return Err(
                "notEligible: this device does not hold an active Master-issued certificate"
                    .to_string(),
            );
        }
    }
    let now_ms = now_unix_ms();
    let armed_until = now_ms.saturating_add(crate::owner_quorum_sync::ARM_WINDOW_MS);
    write_arm_cell(&doc, &engine, self_id, armed_until, now_ms).await;
    emit("owner-quorum-updated");
    Ok(armed_until)
}

/// Disarm THIS device's enrollment window early (spec §5.1 Cancel). Writes
/// an already-expired cell (never deletes — see `stamp_arm_cell`).
pub(crate) async fn disarm_quorum_enrollment_inner(
    state: &Mutex<crate::NodeState>,
    keychain: KeychainFactory,
    emit: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<(), String> {
    let (doc, engine, _trust_doc, dir) = snapshot_handles(state)?;
    let loaded = load_keys(dir, keychain).await?;
    let self_id = crate::owner_state::device_id_from_signing_key(&loaded.device_signing_key);
    let now_ms = now_unix_ms();
    write_arm_cell(&doc, &engine, self_id, now_ms.saturating_sub(1), now_ms).await;
    emit("owner-quorum-updated");
    Ok(())
}

fn sink_emit(
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
) -> std::sync::Arc<dyn Fn(&str) + Send + Sync> {
    std::sync::Arc::new(move |name: &str| {
        crate::node_event_sink::emit_ser(&*sink, name, &serde_json::Value::Null);
    })
}

/// ZEB-445-style shared IPC/RPC seams.
pub(crate) async fn request_quorum_revocation_impl(
    state: &Mutex<crate::NodeState>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    device_vk_hex: String,
    reason: String,
) -> Result<String, String> {
    request_quorum_revocation_inner(state, prod_keychain, sink_emit(sink), device_vk_hex, reason)
        .await
}

pub(crate) async fn cosign_quorum_request_impl(
    state: &Mutex<crate::NodeState>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    request_id: String,
) -> Result<(), String> {
    cosign_quorum_request_inner(state, prod_keychain, sink_emit(sink), request_id).await
}

pub(crate) async fn decline_quorum_request_impl(
    state: &Mutex<crate::NodeState>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    request_id: String,
) -> Result<(), String> {
    decline_quorum_request_inner(state, prod_keychain, sink_emit(sink), request_id).await
}

pub(crate) async fn arm_quorum_enrollment_impl(
    state: &Mutex<crate::NodeState>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
) -> Result<u64, String> {
    arm_quorum_enrollment_inner(state, prod_keychain, sink_emit(sink)).await
}

pub(crate) async fn disarm_quorum_enrollment_impl(
    state: &Mutex<crate::NodeState>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
) -> Result<(), String> {
    disarm_quorum_enrollment_inner(state, prod_keychain, sink_emit(sink)).await
}

pub(crate) async fn request_quorum_epoch_bump_impl(
    state: &Mutex<crate::NodeState>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
) -> Result<String, String> {
    request_quorum_epoch_bump_inner(state, prod_keychain, sink_emit(sink)).await
}

#[tauri::command]
pub async fn request_quorum_revocation(
    app: tauri::AppHandle,
    device_vk_hex: String,
    reason: String,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<String, String> {
    request_quorum_revocation_impl(
        state.inner(),
        std::sync::Arc::new(crate::node_event_sink::AppHandleSink(app)),
        device_vk_hex,
        reason,
    )
    .await
}

#[tauri::command]
pub async fn cosign_quorum_request(
    app: tauri::AppHandle,
    request_id: String,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<(), String> {
    cosign_quorum_request_impl(
        state.inner(),
        std::sync::Arc::new(crate::node_event_sink::AppHandleSink(app)),
        request_id,
    )
    .await
}

#[tauri::command]
pub async fn decline_quorum_request(
    app: tauri::AppHandle,
    request_id: String,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<(), String> {
    decline_quorum_request_impl(
        state.inner(),
        std::sync::Arc::new(crate::node_event_sink::AppHandleSink(app)),
        request_id,
    )
    .await
}

#[tauri::command]
pub async fn arm_quorum_enrollment(
    app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<u64, String> {
    arm_quorum_enrollment_impl(
        state.inner(),
        std::sync::Arc::new(crate::node_event_sink::AppHandleSink(app)),
    )
    .await
}

#[tauri::command]
pub async fn disarm_quorum_enrollment(
    app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<(), String> {
    disarm_quorum_enrollment_impl(
        state.inner(),
        std::sync::Arc::new(crate::node_event_sink::AppHandleSink(app)),
    )
    .await
}

#[tauri::command]
pub async fn request_quorum_epoch_bump(
    app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<String, String> {
    request_quorum_epoch_bump_impl(
        state.inner(),
        std::sync::Arc::new(crate::node_event_sink::AppHandleSink(app)),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_owner::certs::LivenessCert;
    use harmony_owner::lifecycle::{enroll_via_master, mint_owner, MintResult, RecoveryArtifact};
    use harmony_owner::pubkey_bundle::PubKeyBundle;

    const NOW: u64 = 1_700_000_000;
    const NOW_MS: u64 = NOW * 1000;

    struct Fleet {
        trust: OwnerState,
        artifact: RecoveryArtifact,
        a_sk: ed25519_dalek::SigningKey,
        a_id: [u8; 16],
        b_sk: ed25519_dalek::SigningKey,
        b_id: [u8; 16],
        c_id: [u8; 16],
        c_vk_hex: String,
    }

    /// Three master-enrolled devices, all with fresh liveness: A (initiator),
    /// B (cosigner), C (revocation target).
    fn three_device_fleet() -> Fleet {
        let MintResult {
            mut state,
            recovery_artifact,
            device_signing_key: a_sk,
        } = mint_owner(NOW).expect("mint");
        let a_id = crate::owner_state::device_id_from_signing_key(&a_sk);
        let owner_id = state.owner_id;
        let mut enroll = |now: u64| {
            let sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
            let res = enroll_via_master(
                &state,
                &recovery_artifact,
                &sk,
                PubKeyBundle::classical_only(sk.verifying_key().to_bytes()),
                now,
                DEFAULT_ACTIVE_WINDOW_SECS,
            )
            .expect("enroll");
            let id = res.enrollment_cert.device_id;
            state
                .add_enrollment(res.enrollment_cert, now, DEFAULT_ACTIVE_WINDOW_SECS)
                .expect("add enrollment");
            (sk, id)
        };
        let (b_sk, b_id) = enroll(NOW + 1);
        let (c_sk, c_id) = enroll(NOW + 2);
        let c_vk_hex = hex::encode(c_sk.verifying_key().to_bytes());
        for sk in [&a_sk, &b_sk, &c_sk] {
            state
                .add_liveness(LivenessCert::sign(sk, owner_id, NOW + 3).unwrap())
                .expect("liveness");
        }
        Fleet {
            trust: state,
            artifact: recovery_artifact,
            a_sk,
            a_id,
            b_sk,
            b_id,
            c_id,
            c_vk_hex,
        }
    }

    fn plan(
        f: &Fleet,
        master_present: bool,
        vk_hex: &str,
        reason: &str,
    ) -> Result<(String, QuorumRequest), String> {
        plan_quorum_revocation_request(
            &f.trust,
            &f.a_sk,
            master_present,
            vk_hex,
            reason,
            NOW + 10,
            NOW_MS + 10_000,
            [0xab; 16],
            None,
        )
    }

    #[test]
    fn planner_guard_matrix() {
        let f = three_device_fleet();
        assert!(plan(&f, true, &f.c_vk_hex, "lost")
            .unwrap_err()
            .starts_with("hasMaster:"));
        assert!(plan(&f, false, &f.c_vk_hex, "nonsense")
            .unwrap_err()
            .starts_with("invalidReason:"));
        assert!(plan(&f, false, "zz", "lost")
            .unwrap_err()
            .starts_with("badDeviceVk:"));
        assert!(plan(&f, false, &"00".repeat(32), "lost")
            .unwrap_err()
            .starts_with("unknownDevice:"));
        let a_vk_hex = hex::encode(f.a_sk.verifying_key().to_bytes());
        assert!(plan(&f, false, &a_vk_hex, "lost")
            .unwrap_err()
            .starts_with("selfTarget:"));

        // Already revoked target.
        let mut revoked = three_device_fleet();
        let rev = RevocationCert::sign_master(
            &revoked.artifact.master_signing_key(),
            revoked.artifact.master_pubkey_bundle(),
            revoked.c_id,
            NOW + 5,
            harmony_owner::certs::RevocationReason::Lost,
        )
        .unwrap();
        revoked
            .trust
            .add_revocation(rev, NOW + 5, DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        assert!(plan(&revoked, false, &revoked.c_vk_hex.clone(), "lost")
            .unwrap_err()
            .starts_with("alreadyRevoked:"));

        // No second master-certed active sibling: two-device fleet where
        // the only other device is the target.
        let two = {
            let mut f2 = three_device_fleet();
            // Revoke B so only A (initiator) and C (target) remain active.
            let rev_b = RevocationCert::sign_master(
                &f2.artifact.master_signing_key(),
                f2.artifact.master_pubkey_bundle(),
                f2.b_id,
                NOW + 5,
                harmony_owner::certs::RevocationReason::Decommissioned,
            )
            .unwrap();
            f2.trust
                .add_revocation(rev_b, NOW + 5, DEFAULT_ACTIVE_WINDOW_SECS)
                .unwrap();
            f2
        };
        assert!(plan(&two, false, &two.c_vk_hex.clone(), "lost")
            .unwrap_err()
            .starts_with("noQuorum:"));
    }

    #[test]
    fn planner_happy_path_pre_signs_each_eligible_cosigner() {
        let f = three_device_fleet();
        let (id_hex, req) = plan(&f, false, &f.c_vk_hex, "lost").expect("plan ok");
        assert_eq!(id_hex, hex::encode([0xab; 16]));
        assert_eq!(req.initiator_hex, hex::encode(f.a_id));
        // Only B is eligible (C is the target, A the initiator).
        assert_eq!(
            req.initiator_sigs.keys().cloned().collect::<Vec<_>>(),
            vec![hex::encode(f.b_id)]
        );
        // The pre-signed part verifies over the recomputed pair payload.
        let payload = revocation_pair_payload(
            f.trust.owner_id,
            f.c_id,
            req.issued_at,
            &harmony_owner::certs::RevocationReason::Lost,
            f.a_id,
            f.b_id,
        )
        .unwrap();
        let sig = hex::decode(&req.initiator_sigs[&hex::encode(f.b_id)]).unwrap();
        verify_with_tag(
            &f.a_sk.verifying_key(),
            tags::REVOCATION,
            &payload,
            &sig,
            "Revocation-Quorum-Part",
        )
        .expect("initiator part verifies");
        assert!(req.signatures.is_empty());
        assert_eq!(
            req.expires_at_ms,
            NOW_MS + 10_000 + QUORUM_REVOCATION_TTL_MS
        );
    }

    fn doc_with_request(f: &Fleet) -> (QuorumReqDoc, String) {
        let (id_hex, req) = plan(f, false, &f.c_vk_hex, "lost").expect("plan ok");
        let mut doc = QuorumReqDoc::default();
        doc.requests.insert(id_hex.clone(), req);
        (doc, id_hex)
    }

    #[test]
    fn cosign_core_happy_path_and_idempotency() {
        let f = three_device_fleet();
        let (mut doc, id) = doc_with_request(&f);
        let signed = cosign_request_core(&mut doc, &f.trust, &f.b_sk, f.b_id, &id, NOW_MS + 20_000)
            .expect("cosign ok");
        assert!(signed);
        let entry = &doc.requests[&id].signatures[&hex::encode(f.b_id)];
        assert!(entry.epoch_doc_sig_hex.is_none());
        // B's part verifies over the same pair payload.
        let payload = revocation_pair_payload(
            f.trust.owner_id,
            f.c_id,
            doc.requests[&id].issued_at,
            &harmony_owner::certs::RevocationReason::Lost,
            f.a_id,
            f.b_id,
        )
        .unwrap();
        verify_with_tag(
            &f.b_sk.verifying_key(),
            tags::REVOCATION,
            &payload,
            &hex::decode(&entry.primary_sig_hex).unwrap(),
            "Revocation-Quorum-Part",
        )
        .expect("cosigner part verifies");
        // Second call: idempotent no-op.
        let again = cosign_request_core(&mut doc, &f.trust, &f.b_sk, f.b_id, &id, NOW_MS + 21_000)
            .expect("idempotent");
        assert!(!again);
    }

    #[test]
    fn cosign_core_rejection_matrix() {
        let f = three_device_fleet();
        let (mut doc, id) = doc_with_request(&f);

        assert!(
            cosign_request_core(&mut doc, &f.trust, &f.b_sk, f.b_id, "ff00", NOW_MS)
                .unwrap_err()
                .starts_with("unknownRequest:")
        );
        // Expired.
        assert!(cosign_request_core(
            &mut doc,
            &f.trust,
            &f.b_sk,
            f.b_id,
            &id,
            NOW_MS + QUORUM_REVOCATION_TTL_MS + 20_000
        )
        .unwrap_err()
        .starts_with("expired:"));
        // Initiator self-cosign.
        assert!(
            cosign_request_core(&mut doc, &f.trust, &f.a_sk, f.a_id, &id, NOW_MS + 20_000)
                .unwrap_err()
                .starts_with("ownRequest:")
        );
        // Declined earlier (a real, verifiable veto by B).
        decline_request_core(&mut doc, &f.trust, &f.b_sk, f.b_id, &id).expect("decline");
        assert!(
            cosign_request_core(&mut doc, &f.trust, &f.b_sk, f.b_id, &id, NOW_MS + 20_000)
                .unwrap_err()
                .starts_with("declined:")
        );
        {
            let req = doc.requests.get_mut(&id).unwrap();
            req.declined_by.clear();
        }
        // Tampered initiator signature.
        {
            let req = doc.requests.get_mut(&id).unwrap();
            let slot = req.initiator_sigs.get_mut(&hex::encode(f.b_id)).unwrap();
            *slot = "00".repeat(64);
        }
        assert!(
            cosign_request_core(&mut doc, &f.trust, &f.b_sk, f.b_id, &id, NOW_MS + 20_000)
                .unwrap_err()
                .starts_with("badInitiatorSig:")
        );
        // Not addressed (no slot for this device).
        {
            let req = doc.requests.get_mut(&id).unwrap();
            req.initiator_sigs.clear();
        }
        assert!(
            cosign_request_core(&mut doc, &f.trust, &f.b_sk, f.b_id, &id, NOW_MS + 20_000)
                .unwrap_err()
                .starts_with("notAddressed:")
        );
    }

    #[test]
    fn decline_core_tombstones_and_rejects_initiator() {
        let f = three_device_fleet();
        let (mut doc, id) = doc_with_request(&f);
        assert!(
            decline_request_core(&mut doc, &f.trust, &f.b_sk, f.b_id, &id).expect("decline ok")
        );
        // The tombstone is a VERIFIED veto (signed by B over the decline
        // payload), not merely a raw map entry.
        assert!(
            crate::owner_quorum_sync::verified_decliners(&f.trust, &id, &doc.requests[&id])
                .contains(&hex::encode(f.b_id))
        );
        // Idempotent.
        assert!(
            !decline_request_core(&mut doc, &f.trust, &f.b_sk, f.b_id, &id).expect("idempotent")
        );
        // The initiator cannot decline its own request.
        assert!(
            decline_request_core(&mut doc, &f.trust, &f.a_sk, f.a_id, &id)
                .unwrap_err()
                .starts_with("ownRequest:")
        );
        assert!(
            decline_request_core(&mut doc, &f.trust, &f.b_sk, f.b_id, "ff00")
                .unwrap_err()
                .starts_with("unknownRequest:")
        );
    }
}
