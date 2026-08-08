//! Tauri command surface for the Devices panel.
//!
//! Wraps `crate::owner_state` operations with async + state-injection
//! plumbing. Long-running ops go through `crate::identity_commands::run_blocking`.

use crate::identity::KeychainStore;
use crate::identity_commands::run_blocking;
use crate::owner_state::{
    insert_token, load_owner_state, refresh_self_liveness, save_owner_state_atomic,
    save_owner_state_cbor_only, take_token, DeviceView, LoadedOwnerState, OwnerStateView,
    TrustDecisionView, TrustKind,
};
use crate::recovery_policy::{MAX_RECOVERY_COMMENT_BYTES, MIN_RECOVERY_PASSPHRASE_LEN};
use harmony_owner::certs::{RevocationCert, RevocationReason};
use harmony_owner::lifecycle::{mint_owner, MintResult, RecoveryArtifact};
use harmony_owner::recovery::RecoveryMetadata;
use harmony_owner::trust;
use secrecy::SecretString;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;
use zeroize::Zeroizing;

/// Process-wide mutex for **all writers of `owner_state.cbor` and its
/// companion seed/key entries** — mint plus the pairing-persist drainer
/// that calls `install_inviter_state` / `install_joiner_state` after a
/// successful pair. Without this serialization (ZEB-199), two concurrent
/// pairing-Completes can both load the pre-mutation OwnerState, each add
/// their own enrollment, and one writer's enrollment is silently lost
/// when the second `save_owner_state_atomic` overwrites the first.
///
/// Originally introduced as `MINT_OWNER_LOCK` in PR #62 to guard the
/// mint check-and-write window against rapid double-click; renamed to
/// reflect its broader role during ZEB-199 review. Held across each
/// caller's entire load+save window. Recover from poisoning so a panic
/// in one handler doesn't brick future writes (mirrors PR-61's
/// preview_cache_lock policy).
///
/// Note: this lock does NOT cover the encrypted-file writers `rotate_passphrase`
/// / `write_seed_to_disk_with_keychain`, which write `identity.enc` but not
/// `owner_state.cbor`. Those are serialized by the sibling
/// `identity::IDENTITY_FILE_WRITE_LOCK` (ZEB-201). `save_owner_state_atomic`
/// acquires BOTH — this lock (held by its callers) as the OUTER and
/// `IDENTITY_FILE_WRITE_LOCK` as the INNER; the acquisition order never inverts,
/// so the two locks are deadlock-free together.
pub(crate) static OWNER_STATE_WRITE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MintIpcResult {
    pub state: OwnerStateView,
    pub recovery_token: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportInfo {
    pub identity_hash: String,
    pub byte_len: u64,
    pub path: String,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Epoch-checked sibling of [`now_unix`] (ZEB-721): `None` when the host clock is
/// before the Unix epoch. Liveness-refresh callers use this to SKIP signing a
/// bogus `timestamp = 0` cert (instantly stale to every peer) instead of
/// `unwrap_or(0)`-ing — mirroring the heartbeat's pre-epoch skip.
fn now_unix_checked() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .ok()
}

/// Wire → crate reason mapping (ZEB-668 S2 spec §3: three UI reasons; `Other`
/// unused by the UI).
pub(crate) fn parse_revoke_reason(reason: &str) -> Result<RevocationReason, String> {
    match reason {
        "decommissioned" => Ok(RevocationReason::Decommissioned),
        "lost" => Ok(RevocationReason::Lost),
        "compromised" => Ok(RevocationReason::Compromised),
        other => Err(format!(
            "invalidReason: expected decommissioned|lost|compromised, got {other:?}"
        )),
    }
}

/// Crate → wire label (for `DeviceView.revoked_reason`).
pub(crate) fn revoke_reason_label(reason: &RevocationReason) -> String {
    match reason {
        RevocationReason::Decommissioned => "decommissioned".to_string(),
        RevocationReason::Lost => "lost".to_string(),
        RevocationReason::Compromised => "compromised".to_string(),
        RevocationReason::Other(s) => s.clone(),
    }
}

#[derive(Debug)]
pub(crate) struct PlannedRevocation {
    pub cert: RevocationCert,
    pub is_self: bool,
}

/// Outcome of [`plan_revocation`].
#[derive(Debug)]
pub(crate) enum RevocationPlan {
    /// Target is already revoked. `is_self` lets the caller complete a
    /// PENDING terminal transition — a prior self-revoke that mutated the
    /// doc but failed at flush must converge on retry (CodeRabbit PR #452).
    /// `target` (the resolved device_id) lets that retry re-publish the feed
    /// cut-off from the stored `RevocationCert` (ZEB-678 S3).
    AlreadyRevoked {
        is_self: bool,
        target: [u8; 16],
    },
    Planned(Box<PlannedRevocation>),
}

/// Pure revocation planner: validates the request against a trust-state
/// snapshot and constructs the signed cert. No I/O, no locks — the whole
/// guard surface is unit-testable from mint fixtures.
pub(crate) fn plan_revocation(
    state: &harmony_owner::state::OwnerState,
    device_signing_key: &ed25519_dalek::SigningKey,
    master_seed: Option<&[u8; 32]>,
    device_vk_hex: &str,
    reason_str: &str,
    now: u64,
) -> Result<RevocationPlan, String> {
    let reason = parse_revoke_reason(reason_str)?;
    let vk_bytes: [u8; 32] = hex::decode(device_vk_hex)
        .map_err(|e| format!("badDeviceVk: {e}"))?
        .try_into()
        .map_err(|_| "badDeviceVk: expected 32 bytes".to_string())?;
    // Resolve the target through its enrollment — revocation of a device the
    // owner never enrolled is meaningless (and SelfDevice certs cannot verify
    // without the enrolled vk).
    let target = state
        .enrollments
        .values()
        .find(|c| c.device_pubkeys.classical.ed25519_verify == vk_bytes)
        .map(|c| c.device_id)
        .ok_or_else(|| "unknownDevice: no enrollment matches that key".to_string())?;
    let self_id = crate::owner_state::device_id_from_signing_key(device_signing_key);
    let is_self = target == self_id;
    if state.is_revoked(target) {
        return Ok(RevocationPlan::AlreadyRevoked { is_self, target });
    }
    // Last-device guard: the CALLER is demonstrably alive (it is making this
    // call), so revoking a sibling can never leave the account with zero
    // usable devices. Only a self-revoke with no other *active* device can —
    // refuse that (conservative: a stale-but-enrolled sibling does not count;
    // the user should revoke from that device or accept recovery-phrase-only).
    if is_self {
        let active = state.active_devices(now, trust::DEFAULT_ACTIVE_WINDOW_SECS);
        if active.iter().all(|d| *d == self_id) {
            return Err(
                "lastDevice: refusing to revoke the only active device on this account".to_string(),
            );
        }
    }
    let cert = if is_self {
        RevocationCert::sign_self(device_signing_key, state.owner_id, target, now, reason)
            .map_err(|e| format!("failed to sign self-revocation: {e}"))?
    } else {
        let seed = master_seed.ok_or_else(|| {
            "notMaster: this device does not hold the master key; only the \
             device with your master key can remove other devices"
                .to_string()
        })?;
        // Transient master reconstruct — same shape as pairing/cert.rs
        // sign_enrollment_for_joiner: derive, sign, drop (RecoveryArtifact
        // zeroizes its seed on drop).
        let artifact = RecoveryArtifact::from_seed(*seed);
        let master_pubkey = artifact.master_pubkey_bundle();
        if master_pubkey.identity_hash() != state.owner_id {
            return Err("master seed does not match this owner".to_string());
        }
        let master_sk = artifact.master_signing_key();
        let cert = RevocationCert::sign_master(&master_sk, master_pubkey, target, now, reason)
            .map_err(|e| format!("failed to sign revocation: {e}"))?;
        drop(master_sk);
        drop(artifact);
        cert
    };
    Ok(RevocationPlan::Planned(Box::new(PlannedRevocation {
        cert,
        is_self,
    })))
}

/// ZEB-668 S5: pure fleet-epoch-bump planner. Derives epoch
/// `max(carrier, current) + 1`, seals the new material to every surviving
/// (enrolled, non-revoked) device's enrollment x25519, and master-signs the
/// carrier doc. No I/O, no locks — unit-testable from mint fixtures.
///
/// Errors: `notMaster:` (seed absent is the CALLER's check — this one fires
/// when the seed doesn't match the owner), `sealFailed:<device_id_hex>`
/// (unusable x25519 even after recomputing from the ed25519 key — the bump
/// ABORTS rather than silently orphaning a surviving device).
pub(crate) fn plan_fleet_epoch_bump(
    trust: &harmony_owner::state::OwnerState,
    carrier: &crate::fleet_key_epoch::FleetKeyEpochDoc,
    current_data_epoch: u32,
    master_seed: &[u8; 32],
    now_ms: u64,
) -> Result<
    (
        crate::fleet_key_epoch::FleetKeyEpochDoc,
        crate::owner_state_crypto::KeyTree,
    ),
    String,
> {
    let artifact = RecoveryArtifact::from_seed(*master_seed);
    let master_pubkey = artifact.master_pubkey_bundle();
    if master_pubkey.identity_hash() != trust.owner_id {
        return Err("notMaster: master seed does not match this owner".to_string());
    }

    let new_epoch = carrier
        .epoch
        .max(current_data_epoch)
        .checked_add(1)
        .ok_or_else(|| "fleet epoch counter overflow".to_string())?;
    let new_kt = crate::owner_state_crypto::KeyTree::derive_at_epoch(master_seed, new_epoch)
        .map_err(|e| format!("derive_at_epoch({new_epoch}): {e}"))?;
    let material_cbor = {
        let mut buf = Zeroizing::new(Vec::new());
        ciborium::into_writer(&new_kt.to_fleet_material(), &mut *buf)
            .map_err(|e| format!("encode new material: {e}"))?;
        buf
    };

    let sealed = seal_material_to_survivors(trust, &material_cbor, None)?;

    let mut doc = crate::fleet_key_epoch::FleetKeyEpochDoc {
        epoch: new_epoch,
        bump_wall_ms: now_ms,
        sealed,
        master_pubkey: None,
        master_sig: Vec::new(),
        quorum_sig: None,
        signer_certs: Vec::new(),
    };
    doc.sign(&artifact.master_signing_key(), master_pubkey)?;
    Ok((doc, new_kt))
}

/// Seal `material_cbor` to every surviving device's enrollment x25519 →
/// `device_id_hex → blob`. Survivors = enrolled minus revoked (deliberately
/// NOT `active_devices`: a temporarily-offline, non-revoked device must still
/// get a blob or it is orphaned at window close). `exclude` additionally drops
/// one device — the quorum-revocation target, which may not yet be revoked in
/// this trust snapshot (ZEB-677 S5).
fn seal_material_to_survivors(
    trust: &harmony_owner::state::OwnerState,
    material_cbor: &[u8],
    exclude: Option<[u8; 16]>,
) -> Result<std::collections::BTreeMap<String, Vec<u8>>, String> {
    let mut sealed = std::collections::BTreeMap::new();
    for (device_id, cert) in trust.enrollments.iter() {
        if trust.is_revoked(*device_id) || exclude == Some(*device_id) {
            continue;
        }
        let id_hex = hex::encode(device_id);
        let mut x_pub = cert.device_pubkeys.classical.x25519_pub;
        if x_pub == [0u8; 32] {
            // `classical_only` zero-fills the x25519 slot when the ed25519
            // bytes don't map — retry the birational map explicitly so the
            // error names the device instead of sealing to a dead key.
            x_pub = crate::dm_signing::ed25519_pub_to_x25519(
                &cert.device_pubkeys.classical.ed25519_verify,
            )
            .map_err(|e| format!("sealFailed:{id_hex}: no usable x25519 ({e})"))?;
        }
        let blob = crate::dm_signing::seal_to_owner_with_info(
            &x_pub,
            material_cbor,
            crate::fleet_key_epoch::FLEET_EPOCH_SEAL_INFO,
        )
        .map_err(|e| format!("sealFailed:{id_hex}: {e}"))?;
        sealed.insert(id_hex, blob);
    }
    Ok(sealed)
}

/// ZEB-677 S5 — build the UNSIGNED next-epoch carrier doc for a master-less
/// (quorum) fleet bump. Generates a FRESH RANDOM `KeyTree` (no master seed to
/// derive from) and seals it to survivors minus `exclude_target`. The returned
/// doc carries no signature — the co-sign ceremony collects K=2 quorum parts
/// over its `signing_bytes` and calls `assemble_quorum`. The returned `KeyTree`
/// is discarded by the request planner (A recovers it by unsealing its own
/// blob at assembly time, like any survivor).
pub(crate) fn plan_fleet_epoch_bump_quorum(
    trust: &harmony_owner::state::OwnerState,
    current_data_epoch: u32,
    now_ms: u64,
    exclude_target: Option<[u8; 16]>,
) -> Result<
    (
        crate::fleet_key_epoch::FleetKeyEpochDoc,
        crate::owner_state_crypto::KeyTree,
    ),
    String,
> {
    let new_epoch = current_data_epoch
        .checked_add(1)
        .ok_or_else(|| "fleet epoch counter overflow".to_string())?;
    let new_kt = crate::owner_state_crypto::KeyTree::generate_at_epoch(new_epoch);
    let material_cbor = {
        let mut buf = Zeroizing::new(Vec::new());
        ciborium::into_writer(&new_kt.to_fleet_material(), &mut *buf)
            .map_err(|e| format!("encode new material: {e}"))?;
        buf
    };
    let sealed = seal_material_to_survivors(trust, &material_cbor, exclude_target)?;
    let doc = crate::fleet_key_epoch::FleetKeyEpochDoc {
        epoch: new_epoch,
        bump_wall_ms: now_ms,
        sealed,
        master_pubkey: None,
        master_sig: Vec::new(),
        quorum_sig: None,
        signer_certs: Vec::new(),
    };
    Ok((doc, new_kt))
}

/// ZEB-668 S5: pure window-close decision for the dual-epoch read window.
/// `survivor_seen_ms` carries each surviving device's fleet-net
/// `seen_at.wall_ms` (`None` = no row yet). Close when EVERY survivor has
/// been seen after the bump, or when the 7-day window has elapsed —
/// whichever comes first.
pub(crate) fn fleet_epoch_window_should_close(
    bump_wall_ms: u64,
    now_ms: u64,
    survivor_seen_ms: &[Option<u64>],
) -> bool {
    if now_ms >= bump_wall_ms.saturating_add(crate::fleet_key_epoch::FLEET_EPOCH_WINDOW_MS) {
        return true;
    }
    !survivor_seen_ms.is_empty()
        && survivor_seen_ms
            .iter()
            .all(|seen| seen.is_some_and(|ms| ms > bump_wall_ms))
}

/// ZEB-668 S4: everything `build_owner_state_view` joins from the fleet-net
/// doc + peer liveness, snapshotted in async context before the blocking
/// task. `Default` = fleet-net cold / node not running — every joined field
/// degrades honestly (no pin, no petnames, no last-seen, not connected).
#[derive(Default)]
pub(crate) struct FleetJoin {
    /// 64-hex fleet `pinned` value (butler pin).
    pub pinned: Option<String>,
    /// device_vk_hex → petname as stored (the view join trims). An
    /// empty-after-trim value is an explicitly CLEARED name (LWW tombstone
    /// entry); an absent key means the device was never named. The
    /// distinction flows into `DeviceView.pet_name` (`Some("")` vs `None`)
    /// and gates the panel's one-shot label migration — a cleared name must
    /// never be resurrected from a stale local label (PR #454 round 1).
    pub petnames: std::collections::BTreeMap<String, String>,
    /// device_vk_hex → (seen_at.wall_ms, iroh_endpoint_id).
    pub rows: std::collections::BTreeMap<String, (u64, [u8; 32])>,
    /// Endpoint ids with a live Connected liveness slot (Degraded excluded).
    pub connected_eps: std::collections::BTreeSet<[u8; 32]>,
    /// ZEB-668 S5: fleet-keys carrier snapshot — current epoch + the
    /// wall-clock of the bump that produced it (0/0 = never bumped, or the
    /// carrier is cold).
    pub carrier_epoch: u32,
    pub carrier_bump_wall_ms: u64,
}

/// ZEB-677 S3: snapshot of the resident quorum-request doc for the view
/// join. Default (empty doc, now 0) when the node is not running — no
/// resident doc means no ceremony surface, honestly.
#[derive(Default)]
pub(crate) struct QuorumJoin {
    pub doc: crate::owner_quorum_sync::QuorumReqDoc,
    pub now_ms: u64,
}

/// Build an `OwnerStateView` from a loaded state.
///
/// `fleet`: the fleet-net + liveness join snapshot (see [`FleetJoin`]). The
/// matching device row receives `butler_pinned` / `pet_name` /
/// `last_seen_ms` / `connected_now`; everything defaults to absent when the
/// fleet-net doc is cold or the node is not running.
///
/// `quorum`: the quorum-request doc snapshot (see [`QuorumJoin`]) feeding
/// the co-sign banner rows + per-device `quorum_removable`.
fn build_owner_state_view(
    loaded: &LoadedOwnerState,
    this_device_name: String,
    fleet: FleetJoin,
    quorum: QuorumJoin,
) -> OwnerStateView {
    // ZEB-721: keep the epoch-checked clock so a pre-epoch (`None`) clock does NOT
    // surface a false regression. `now` stays 0 for the other view computations
    // below, which already tolerate a broken clock.
    let now_checked = now_unix_checked();
    let now = now_checked.unwrap_or(0);
    let active_window = trust::DEFAULT_ACTIVE_WINDOW_SECS;
    let freshness = trust::DEFAULT_FRESHNESS_WINDOW_SECS;
    // ZEB-721: surface a regressed host clock (our own cert stamped in the future)
    // so the panel can warn; `None` when healthy OR when the clock is pre-epoch.
    // Same helper the refresh path uses.
    let self_clock_regressed_skew_secs = now_checked.and_then(|now| {
        crate::owner_state::self_liveness_future_skew_secs(
            &loaded.state,
            &loaded.device_signing_key,
            now,
        )
    });
    let this_device_id = derive_this_device_id(&loaded.device_signing_key);
    let this_device_hex = hex::encode(this_device_id);
    let self_is_master = loaded.master_seed.is_some();
    let self_master_certed = loaded
        .state
        .enrollments
        .get(&this_device_id)
        .is_some_and(crate::owner_quorum_commands::is_master_issued);
    // Active master-certed device ids — the co-signer candidate pool for
    // `quorum_removable` (spec §4.1 visibility rule).
    let active_master_certed: std::collections::BTreeSet<[u8; 16]> = loaded
        .state
        .active_devices(now, active_window)
        .into_iter()
        .filter(|id| {
            loaded
                .state
                .enrollments
                .get(id)
                .is_some_and(crate::owner_quorum_commands::is_master_issued)
        })
        .collect();
    // S4: can THIS device arm an enrollment window? Only a master-less fleet
    // uses quorum enrollment; it needs this device Master-certed plus ≥1
    // OTHER active Master-certed sibling to act as the inviter (spec §5.1).
    let can_arm_enrollment = !self_is_master
        && self_master_certed
        && active_master_certed.iter().any(|id| *id != this_device_id);

    let devices: Vec<DeviceView> = loaded
        .state
        .enrollments
        .values()
        .map(|cert| {
            let decision =
                trust::evaluate_trust(&loaded.state, cert.device_id, now, active_window, freshness);
            let (kind, reason) = match decision {
                trust::TrustDecision::Full => (TrustKind::Full, None),
                trust::TrustDecision::Provisional => (TrustKind::Provisional, None),
                trust::TrustDecision::Refused(r) => (TrustKind::Refused, Some(format!("{r:?}"))),
            };
            // The fleet-net doc keys on hex of the 32-byte ed25519 verify key;
            // enrollment certs carry the ed25519 verify key directly (first 32
            // bytes of the classical bundle). Compare device_id hex forms.
            let dev_id_hex = hex::encode(cert.device_pubkeys.classical.ed25519_verify);
            let butler_pinned = fleet
                .pinned
                .as_deref()
                .map(|p| p == dev_id_hex)
                .unwrap_or(false);
            // ZEB-668 S4: petname + last-seen + connected join on the same
            // 64-hex vk key the fleet-net doc uses. Trim on read (PR #454
            // round 1): the local writer trims, but a remote peer's entry is
            // only LWW-merged — normalize here so a whitespace-only name can
            // never surface as a visible petname. Some("") = explicitly
            // cleared (distinct from None = never named — see FleetJoin doc).
            let pet_name = fleet
                .petnames
                .get(&dev_id_hex)
                .map(|s| s.trim().to_string());
            let (last_seen_ms, connected_now) = match fleet.rows.get(&dev_id_hex) {
                Some((ms, ep)) => (Some(*ms), fleet.connected_eps.contains(ep)),
                None => (None, false),
            };
            let rev_cert = loaded.state.revocations.cert_for(cert.device_id);
            DeviceView {
                device_id: hex::encode(cert.device_id),
                display_name: if cert.device_id == this_device_id {
                    this_device_name.clone()
                } else {
                    format!("Device {}", &hex::encode(cert.device_id)[..8])
                },
                is_this_device: cert.device_id == this_device_id,
                trust_decision: TrustDecisionView { kind, reason },
                enrolled_at: cert.issued_at,
                fingerprint: format_fingerprint(&cert.device_id),
                butler_pinned,
                // Round-2 Greptile P1: the toggle must send THIS value to
                // `set_butler_pin` — `device_id` above is the 16-byte
                // identity hash, which the enrolled-set check rejects.
                device_vk_hex: dev_id_hex,
                revoked: rev_cert.is_some(),
                revoked_at: rev_cert.map(|c| c.issued_at),
                revoked_reason: rev_cert.map(|c| revoke_reason_label(&c.reason)),
                pet_name,
                last_seen_ms,
                connected_now,
                // ZEB-677 S3: sibling removable via the co-sign ceremony —
                // seed absent here, self master-certed, and some OTHER
                // active master-certed sibling (≠ self, ≠ this row) exists.
                quorum_removable: cert.device_id != this_device_id
                    && rev_cert.is_none()
                    && !self_is_master
                    && self_master_certed
                    && active_master_certed
                        .iter()
                        .any(|id| *id != this_device_id && *id != cert.device_id),
            }
        })
        .collect();

    // ZEB-668 S5: the fleet keys are stale when ANY revocation postdates
    // the last bump — that device still holds decryptable material. Cert
    // `issued_at` is SECONDS; the carrier bump stamp is MILLISECONDS.
    // Pre-S5 fleets (bump 0) with any revocation are honestly stale until
    // their first rotation.
    let fleet_epoch_stale = loaded
        .state
        .revocations
        .iter()
        .any(|c| c.issued_at.saturating_mul(1000) > fleet.carrier_bump_wall_ms);

    // ZEB-677 S3: pending co-sign requests (unexpired only), pre-joined so
    // the panel just renders `can_cosign` / `initiated_by_me`.
    let quorum_requests: Vec<crate::owner_state::QuorumRequestView> = quorum
        .doc
        .requests
        .iter()
        .filter(|(_, r)| quorum.now_ms <= r.expires_at_ms)
        .filter_map(|(id, r)| {
            // Only revocation requests surface as manual co-sign banners;
            // enrollment requests are auto-co-signed by an armed sibling.
            let crate::owner_quorum_sync::QuorumRequestKind::Revocation {
                reason, target_hex, ..
            } = &r.kind
            else {
                return None;
            };
            let initiated_by_me = r.initiator_hex == this_device_hex;
            let signed_by_me = r.signatures.contains_key(&this_device_hex);
            // Only VERIFIED declines surface (an unverifiable entry is
            // forgeable junk and must not render the request as dead).
            let decliners = crate::owner_quorum_sync::verified_decliners(&loaded.state, id, r);
            let declined_by_me = decliners.contains(&this_device_hex);
            let declined = !decliners.is_empty();
            let target_is_me = *target_hex == this_device_hex;
            let target_revoked = crate::owner_quorum_sync::parse_device_id_hex(target_hex)
                .map(|t| loaded.state.is_revoked(t))
                .unwrap_or(true);
            Some(crate::owner_state::QuorumRequestView {
                request_id: id.clone(),
                kind: "revocation".to_string(),
                target_device_id: target_hex.clone(),
                initiator_device_id: r.initiator_hex.clone(),
                reason: reason.clone(),
                expires_at_ms: r.expires_at_ms,
                initiated_by_me,
                signed_by_me,
                declined_by_me,
                declined,
                cosigner_signed: !r.signatures.is_empty(),
                can_cosign: !initiated_by_me
                    && !signed_by_me
                    && !declined
                    && !target_is_me
                    && !target_revoked
                    && self_master_certed
                    && r.initiator_sigs.contains_key(&this_device_hex),
            })
        })
        .collect();
    let quorum_armed_until_ms = quorum
        .doc
        .enroll_arms
        .get(&this_device_hex)
        .filter(|arm| quorum.now_ms <= arm.armed_until_ms)
        .map(|arm| arm.armed_until_ms);

    OwnerStateView {
        owner_id: hex::encode(loaded.state.owner_id),
        owner_display_name: this_device_name,
        devices,
        can_back_up: loaded.master_seed.is_some(),
        fleet_epoch: fleet.carrier_epoch,
        fleet_epoch_stale,
        self_is_master,
        can_arm_enrollment,
        quorum_requests,
        quorum_armed_until_ms,
        self_clock_regressed_skew_secs,
    }
}

fn derive_this_device_id(sk: &ed25519_dalek::SigningKey) -> [u8; 16] {
    // Delegates to the single source of truth so the Devices-panel view and the
    // liveness refresh can never derive the local device id differently.
    crate::owner_state::device_id_from_signing_key(sk)
}

/// Format the first 4 bytes of a 16-byte device_id as `xxxx·xxxx`
/// for display. The full id is internal plumbing — see the
/// "Two-address world" section of the design spec.
pub(crate) fn format_fingerprint(id: &[u8; 16]) -> String {
    let hex = hex::encode(id);
    format!("{}·{}", &hex[..4], &hex[4..8])
}

/// Resolve the directory where owner_state.cbor + companion files live.
///
/// Workaround: `crate::identity::identity_dir(AppHandle)` does not exist;
/// instead, take the parent of the per-device identity key path. Assumes
/// `identity.key` is never at the filesystem root — true on every Tauri-
/// supported OS (macOS / Linux / Windows).
pub(crate) fn resolve_identity_dir() -> Result<PathBuf, String> {
    let key_path = crate::identity::resolve_path(None)?;
    key_path
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| "identity key path has no parent directory".to_string())
}

#[tauri::command]
pub async fn get_owner_state(
    _app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<Option<OwnerStateView>, String> {
    get_owner_state_impl(state.inner()).await
}

/// ZEB-428: keychain construction is injected as a factory so tests pass
/// `|| None` explicitly instead of relying on the constructor's test-build
/// refusal. A fn pointer (not a closure type) keeps the blocking-task move
/// `'static` while letting construction happen inside the blocking closure.
pub(crate) type KeychainFactory = fn() -> Option<KeychainStore>;

pub(crate) fn prod_keychain() -> Option<KeychainStore> {
    KeychainStore::new().ok()
}

/// ZEB-445: shared IPC/RPC seam.
pub(crate) async fn get_owner_state_impl(
    state: &std::sync::Mutex<crate::NodeState>,
) -> Result<Option<OwnerStateView>, String> {
    get_owner_state_inner(state, prod_keychain).await
}

/// ZEB-668 S2: keychain-injectable body (see `KeychainFactory`).
pub(crate) async fn get_owner_state_inner(
    state: &std::sync::Mutex<crate::NodeState>,
    keychain: KeychainFactory,
) -> Result<Option<OwnerStateView>, String> {
    // ZEB-418 P2 D17 + ZEB-668 S4: snapshot the whole fleet-net join (pin,
    // petnames, per-row seen_at/endpoint) plus the Connected liveness set
    // before entering the blocking task. Reads under the NodeState lock; the
    // Arc clones are cheap and the tokio Mutex lock is async — we do it here
    // (async context) and pass the resolved `FleetJoin` into the blocking
    // closure (no async in there).
    let fleet: FleetJoin = {
        let (fleet_net_doc_arc, resolver, carrier_doc_arc) = {
            let g = state
                .lock()
                .map_err(|e| format!("NodeState poisoned: {e}"))?;
            (
                g.fleet_net_doc.clone(),
                g.reachability_resolver.clone(),
                g.fleet_key_epoch_doc.clone(),
            )
        };
        let mut fleet = FleetJoin::default();
        // ZEB-668 S5: carrier snapshot for the epoch/staleness pair.
        if let Some(arc) = carrier_doc_arc {
            let doc = arc.lock().await;
            fleet.carrier_epoch = doc.epoch;
            fleet.carrier_bump_wall_ms = doc.bump_wall_ms;
        }
        if let Some(arc) = fleet_net_doc_arc {
            let doc = arc.lock().await;
            fleet.pinned = doc.pinned.clone();
            for (id, row) in &doc.devices {
                fleet
                    .rows
                    .insert(id.clone(), (row.seen_at.wall_ms, row.iroh_endpoint_id));
            }
            for (id, pn) in &doc.petnames {
                fleet.petnames.insert(id.clone(), pn.name.clone());
            }
        }
        if let Some(h) = resolver.and_then(|r| r.liveness()) {
            for (ep, st) in h.states_snapshot() {
                if matches!(
                    st,
                    crate::peer_liveness::LivenessStateWire::Connected { .. }
                ) {
                    fleet.connected_eps.insert(ep);
                }
            }
        }
        fleet
    };
    // ZEB-668 S1: snapshot the resident trust handles (Some while the node
    // runs with an owner loaded). When resident, the view renders from the
    // replicated trust doc and a liveness refresh reaches siblings through
    // the trust engine instead of only a silent local file write.
    let (trust_resident, quorum_doc_arc) = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        let trust = match (g.owner_trust_doc.clone(), g.owner_trust_sync.clone()) {
            (Some(doc), Some(engine)) => Some((doc, engine)),
            _ => None,
        };
        (trust, g.owner_quorum_doc.clone())
    };
    // ZEB-677 S3: quorum-request snapshot for the co-sign surfaces. Empty
    // join when the node is down — no resident doc, no ceremony surface.
    let quorum: QuorumJoin = match quorum_doc_arc {
        Some(arc) => QuorumJoin {
            doc: arc.lock().await.clone(),
            now_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        },
        None => QuorumJoin::default(),
    };
    let identity_dir = resolve_identity_dir()?;
    let display_name = "this device".to_string();
    if let Some((doc, engine)) = trust_resident {
        // Keys still come from disk/keychain (they are not part of the
        // replicated doc); the trust state itself comes from the resident
        // doc, which was seeded from disk at start_node and is at least as
        // new as owner_state.cbor.
        let dir = identity_dir.clone();
        let loaded_opt = run_blocking(move || {
            let _guard = OWNER_STATE_WRITE_LOCK
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            load_owner_state(&dir, keychain())
        })
        .await?;
        let mut loaded = match loaded_opt {
            Some(l) => l,
            None => return Ok(None),
        };
        let (snapshot, refreshed) = {
            let mut g = doc.lock().await;
            // ZEB-721: skip on a pre-epoch clock rather than stamping a 0-ts cert.
            let refreshed = match now_unix_checked() {
                Some(now) => refresh_self_liveness(&mut g, &loaded.device_signing_key, now).wrote(),
                None => {
                    tracing::warn!(
                        target: "harmony_liveness",
                        "get_owner_state: host clock before Unix epoch; skipping self-liveness refresh"
                    );
                    false
                }
            };
            (g.clone(), refreshed)
        };
        // Only nudge the engine when the refresh actually wrote — a panel
        // open must not cause a pointless publish round.
        if refreshed {
            engine.notify_dirty();
        }
        loaded.state = snapshot;
        return Ok(Some(build_owner_state_view(
            &loaded,
            display_name,
            fleet,
            quorum,
        )));
    }
    run_blocking(move || {
        // ZEB-342: hold the write lock only across load+refresh+save, so the cbor
        // write stays serialized with mint / pairing-install (loading inside the
        // lock closes the read-modify-write race). The lock is released at the end
        // of this block — build_owner_state_view below only reads the already-local
        // `loaded` snapshot (trust eval + formatting), which needs no serialization.
        let loaded = {
            let _guard = OWNER_STATE_WRITE_LOCK
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let mut loaded = match load_owner_state(&identity_dir, keychain())? {
                Some(l) => l,
                None => return Ok(None),
            };
            // ZEB-721: skip on a pre-epoch clock rather than stamping a 0-ts cert.
            let did_write = match now_unix_checked() {
                Some(now) => {
                    refresh_self_liveness(&mut loaded.state, &loaded.device_signing_key, now).wrote()
                }
                None => {
                    tracing::warn!(
                        target: "harmony_liveness",
                        "get_owner_state: host clock before Unix epoch; skipping self-liveness refresh"
                    );
                    false
                }
            };
            if did_write {
                // Fail open: the in-memory state already carries the fresh liveness, so
                // the panel renders correctly even if persistence fails. A persist error
                // must NOT block the Devices panel (it didn't before this change); the
                // next load retries the refresh + write.
                if let Err(e) = save_owner_state_cbor_only(&identity_dir, &loaded.state) {
                    tracing::warn!(
                        error = %e,
                        "get_owner_state: failed to persist refreshed liveness; rendering from in-memory state"
                    );
                }
            }
            loaded
        };
        Ok(Some(build_owner_state_view(
            &loaded,
            display_name,
            fleet,
            quorum,
        )))
    })
    .await
}

/// ZEB-678 S3: find the stamped `feed_binding` for the device whose harmony-owner
/// `device_id` (16-byte, hex) equals `target_device_id_hex`, by scanning fleet-net
/// rows and parsing each row's authority record. `None` ⇒ that device never
/// migrated a feed (honest residual §8 — nothing to cut). Keyed on the *parsed*
/// `device_id`, not the row's SP1 key, so no owner↔SP1 id mapping is needed.
fn feed_binding_for_device(
    doc: &crate::fleet_net::FleetNetDoc,
    target_device_id_hex: &str,
) -> Option<String> {
    doc.devices.values().find_map(|row| {
        let fb = row.feed_binding.as_ref()?;
        let rec: crate::feed_authority::FeedAuthorityRecord = serde_json::from_str(fb).ok()?;
        (rec.device_id == target_device_id_hex).then(|| fb.clone())
    })
}

/// ZEB-678 S3: publish a revoked device's feed cut-off. Reads its stamped active
/// binding from the replicated fleet-net doc, appends `revocation`, and republishes
/// to `harmony/vines/{N}/authority`. Idempotent (sticky-revoked on the follower).
/// `Ok(true)` ⇒ published; `Ok(false)` ⇒ no migrated feed. Never fatal to revoke.
async fn publish_feed_revocation(
    publish_tx: &tokio::sync::mpsc::Sender<crate::event_loop::PublishRequest>,
    fleet_net_doc: &std::sync::Arc<tokio::sync::Mutex<crate::fleet_net::FleetNetDoc>>,
    revocation: &RevocationCert,
    now_ms: u64,
) -> Result<bool, String> {
    let target_hex = hex::encode(revocation.target);
    let feed_binding = {
        let doc = fleet_net_doc.lock().await;
        feed_binding_for_device(&doc, &target_hex)
    };
    let Some(fb) = feed_binding else {
        return Ok(false);
    };
    let (feed_id, rec_json) =
        crate::feed_authority::build_revoked_authority(&fb, revocation, now_ms)?;
    let key_expr = format!("harmony/vines/{feed_id}/authority");
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    publish_tx
        .send(crate::event_loop::PublishRequest {
            key_expr,
            payload: rec_json.into_bytes(),
            reply: reply_tx,
        })
        .await
        .map_err(|_| "vine authority: event loop not running".to_string())?;
    reply_rx
        .await
        .map_err(|_| "vine authority: event loop dropped publish".to_string())?
        .map_err(|e| format!("vine authority: publish rejected: {e}"))?;
    Ok(true)
}

/// ZEB-678 S3: best-effort, non-fatal feed cut-off publish shared by the main
/// revoke path and the idempotent/retry arms. Logs the outcome and never fails
/// the revoke — the trust revocation has already landed regardless. `context`
/// distinguishes the call site in logs (main / self-retry / sibling-retry).
async fn try_publish_feed_cutoff(
    publish_tx: &Option<tokio::sync::mpsc::Sender<crate::event_loop::PublishRequest>>,
    fleet_net_doc: &Option<std::sync::Arc<tokio::sync::Mutex<crate::fleet_net::FleetNetDoc>>>,
    revocation: &RevocationCert,
    now_ms: u64,
    context: &str,
) {
    let (Some(publish_tx), Some(fleet_net_doc)) = (publish_tx, fleet_net_doc) else {
        return;
    };
    match publish_feed_revocation(publish_tx, fleet_net_doc, revocation, now_ms).await {
        Ok(true) => tracing::info!(context, "revoke_device: published vine feed cut-off"),
        Ok(false) => {
            tracing::debug!(
                context,
                "revoke_device: revoked device has no migrated vine feed to cut"
            )
        }
        Err(e) => {
            tracing::warn!(error = %e, context, "revoke_device: vine feed cut-off publish failed (non-fatal)")
        }
    }
}

/// ZEB-668 S2. Self-revoke ordering is load-bearing (spec §3): sign → add →
/// persist → publish+flush → terminal state + engine halt. The initiating
/// device must not wait for its own merge callback.
pub(crate) async fn revoke_device_inner(
    state: &std::sync::Mutex<crate::NodeState>,
    keychain: KeychainFactory,
    emit: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
    device_vk_hex: String,
    reason: String,
) -> Result<(), String> {
    // Snapshot handles under the std lock; drop before any await.
    #[allow(clippy::type_complexity)]
    let (
        trust_doc,
        trust_engine,
        identity_dir,
        revoked_flag,
        owner_sync,
        fleet_net,
        retire_nudge,
        publish_tx,
        fleet_net_doc,
    ) = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.owner_trust_doc.clone(),
            g.owner_trust_sync.clone(),
            g.identity_dir.clone(),
            std::sync::Arc::clone(&g.owner_trust_revoked_self),
            g.sync_engine.clone(),
            g.fleet_net_sync.clone(),
            g.community_device_retire_nudge.clone(),
            g.publish_tx.clone(),
            g.fleet_net_doc.clone(),
        )
    };
    // ZEB-668 S5: carrier handles for the post-revoke epoch bump (master
    // path only). Snapshotted separately to keep the tuple readable.
    let (carrier_doc, carrier_engine, fleet_keys) = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.fleet_key_epoch_doc.clone(),
            g.fleet_key_epoch_sync.clone(),
            g.fleet_keys.clone(),
        )
    };
    // ZEB-685 (S3): handles for the friend-DM RevocationPush fan-out (master
    // revoke only; see the hook after the feed cut-off). Both are `None` when
    // the node is down or iroh is unbound — the push is best-effort and simply
    // skipped then (the revocation still lands in trust state).
    let (crdt_state_for_push, tunnel_manager_for_push, butler_client_for_push) = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state.clone(),
            g.tunnel_manager.clone(),
            g.butler_deposit_client.clone(),
        )
    };
    // Fall back to the resolved default identity dir when the node has not
    // populated NodeState (same source get_owner_state uses).
    let dir = match identity_dir {
        Some(d) => d,
        None => resolve_identity_dir()?,
    };

    // Keys always come from disk/keychain (device sk + optional master seed).
    let dir_for_load = dir.clone();
    let loaded = run_blocking(move || {
        let _guard = OWNER_STATE_WRITE_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        load_owner_state(&dir_for_load, keychain())
    })
    .await?
    .ok_or_else(|| "noOwner: no owner identity on this device".to_string())?;

    // Guard against the freshest trust state we have: resident doc when the
    // node is running, disk snapshot otherwise.
    let trust_snapshot = match &trust_doc {
        Some(doc) => doc.lock().await.clone(),
        None => loaded.state.clone(),
    };
    // ZEB-678 S3: LWW clock (HLC wall_ms) for the revoked authority republish,
    // shared by the retry arm and the main path below.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let planned = match plan_revocation(
        &trust_snapshot,
        &loaded.device_signing_key,
        loaded.master_seed.as_deref(),
        &device_vk_hex,
        &reason,
        now_unix(),
    )? {
        RevocationPlan::Planned(p) => p,
        RevocationPlan::AlreadyRevoked {
            is_self: false,
            target,
        } => {
            // Idempotent for trust state, but still re-drive the feed cut-off:
            // the main-path publish is best-effort, so a transient failure there
            // must be retried on a subsequent revoke of the same (already-revoked)
            // sibling. Any fleet device holding the replicated revocation can
            // republish — the cert is self-proving, sticky-revoked on the follower.
            // (ZEB-678 S3 review — CodeRabbit.)
            if let Some(rev) = trust_snapshot.revocations.cert_for(target).cloned() {
                try_publish_feed_cutoff(&publish_tx, &fleet_net_doc, &rev, now_ms, "sibling-retry")
                    .await;
                // ZEB-685 (S3): re-drive the best-effort DM friend-push on the
                // same retry seam. The live-tunnel half is still best-effort —
                // ZEB-691 now ALSO deposits the same wire into each friend's
                // butler for durable offline recovery (the recipient's inbox
                // sweeper re-verifies + applies it), but re-running the revoke
                // remains the manual retry for the tunnel half, e.g. a friend
                // who was offline at push time. Master-issued only, exactly
                // like the main path — the stored sibling cert is
                // `sign_master` (this arm is `!is_self`); the receiver + push
                // helper are idempotent. (Qodo #471.)
                if let (Some(crdt), Some(mgr)) = (&crdt_state_for_push, &tunnel_manager_for_push) {
                    push_revocation_to_friends(
                        crdt,
                        mgr,
                        butler_client_for_push.as_ref(),
                        &trust_snapshot,
                        &rev,
                    )
                    .await;
                }
            }
            return Ok(());
        }
        RevocationPlan::AlreadyRevoked {
            is_self: true,
            target,
        } => {
            // The doc already carries our own revocation but the terminal
            // transition may be PENDING — a prior self-revoke that failed
            // between mutation and flush would otherwise strand this device
            // running revoked forever (CodeRabbit PR #452). Idempotent when
            // terminal already completed (flag latched → plain success).
            if revoked_flag.load(std::sync::atomic::Ordering::Acquire) {
                return Ok(());
            }
            if let Some(engine) = &trust_engine {
                engine.flush_now().await.map_err(|e| {
                    format!("self-revocation staged but not yet published (will retry): {e}")
                })?;
            }
            // ZEB-678 S3: a self-revoke that added the revocation but died before
            // publishing its feed cut-off must re-publish it on retry — followers
            // key on the authority cache, not the trust doc. Idempotent (sticky-
            // revoked on the follower); reuses the stored cert, no re-signing.
            if let Some(rev) = trust_snapshot.revocations.cert_for(target).cloned() {
                try_publish_feed_cutoff(&publish_tx, &fleet_net_doc, &rev, now_ms, "self-retry")
                    .await;
            }
            complete_self_revoke_terminal(
                &revoked_flag,
                &emit,
                owner_sync,
                fleet_net,
                trust_engine,
            )
            .await;
            return Ok(());
        }
    };
    let is_self = planned.is_self;
    let cert = planned.cert;
    // ZEB-678 S3: keep a copy for the feed cut-off — `cert` is moved into the
    // add_revocation closure below.
    let cert_for_feed = cert.clone();

    // Apply through the S1 substrate: resident doc + notify_dirty, or the
    // locked load→mutate→save path when the node is down.
    let access = match (&trust_doc, &trust_engine) {
        (Some(doc), Some(engine)) => crate::owner_trust_sync::TrustStateAccess::Resident {
            doc: std::sync::Arc::clone(doc),
            engine: std::sync::Arc::clone(engine),
        },
        _ => crate::owner_trust_sync::TrustStateAccess::FileOnly {
            identity_dir: dir.clone(),
        },
    };
    // Re-check revoked INSIDE the mutation closure: the planning snapshot is
    // taken outside the doc lock, so a concurrent revoke of the same target
    // must degrade to the documented idempotent no-op, atomically with the
    // insert (CodeRabbit PR #452). The crate's insert is itself a monotonic
    // earliest-wins merge, so this guard is belt-and-suspenders.
    let now = now_unix();
    crate::owner_trust_sync::mutate_trust_state(access, move |s| {
        if s.is_revoked(cert.target) {
            return Ok(());
        }
        s.add_revocation(cert, now, trust::DEFAULT_ACTIVE_WINDOW_SECS)
    })
    .await?
    .map_err(|e| format!("revocation rejected: {e}"))?;

    // Durability + propagation. Resident: force the publish+persist now.
    // Sibling revokes tolerate a flush failure (dirty latch retries); a SELF
    // revoke must not reach the terminal state unpublished — no sibling
    // would ever learn of it. (The retry that converges a failed self flush
    // is the AlreadyRevoked{is_self} arm above.)
    if let Some(engine) = &trust_engine {
        if let Err(e) = engine.flush_now().await {
            if is_self {
                return Err(format!(
                    "self-revocation staged but not yet published (will retry): {e}"
                ));
            }
            tracing::warn!(error = %e, "revoke_device: trust flush failed; dirty latch will retry");
        }
    }

    // ZEB-678 S3: cut off the revoked device's migrated vine feed by republishing
    // its stamped authority binding with the RevocationCert appended. Self-revoke
    // (own feed, before the terminal engine-halt below) and master-revoke (the
    // sibling's replicated feed_binding) share this one path. Best-effort +
    // non-fatal: a device that never migrated a feed has nothing to cut, and a
    // publish failure never fails the revoke (the trust revocation still landed).
    try_publish_feed_cutoff(&publish_tx, &fleet_net_doc, &cert_for_feed, now_ms, "main").await;

    // ZEB-685 (S3): push this revocation to DM-only friends over the friend-DM
    // tunnel so their §5.2 cutoff rejects device D's DMs. MASTER revoke only —
    // a SelfDevice-issued revocation is not a master attestation and the
    // receiver rejects it (design §3.3 / line 60). The live-tunnel half is
    // best-effort fire-and-forget (an offline friend simply misses it, and
    // re-running the revoke is the manual retry — see the AlreadyRevoked
    // sibling-retry arm above); ZEB-691 now ALSO deposits the same wire into
    // each friend's butler for durable offline recovery, recovered by the
    // recipient's inbox sweeper on reconnect. A friend reached later also
    // still learns of the revocation via community retire-announce where a
    // shared community exists. `trust_snapshot` predates the mutation but
    // still carries device D's enrollment (revocation does not prune it).
    if !is_self {
        if let (Some(crdt), Some(mgr)) = (&crdt_state_for_push, &tunnel_manager_for_push) {
            push_revocation_to_friends(
                crdt,
                mgr,
                butler_client_for_push.as_ref(),
                &trust_snapshot,
                &cert_for_feed,
            )
            .await;
        }
    }

    // ZEB-668 S5: a master-issued revoke immediately rotates the fleet
    // KeyTree so the revoked device's retained epoch material goes stale
    // (spec §6: master-revoke always bumps; self-revoke never does — no
    // seed on cert-only devices). Failure does NOT roll back the
    // revocation: the panel's `fleetEpochStale` banner is the retry
    // surface, which is the same UX the self-revoke case lands on.
    if !is_self {
        if let (Some(seed), Some(c_doc), Some(c_engine), Some(keys)) = (
            loaded.master_seed.as_deref(),
            &carrier_doc,
            &carrier_engine,
            &fleet_keys,
        ) {
            // Re-snapshot the RESIDENT trust doc — the revocation just
            // landed in it, and the survivor enumeration must exclude the
            // freshly-revoked device. (`trust_snapshot` above predates the
            // mutation.)
            let bump = async {
                let trust_now = match &trust_doc {
                    Some(doc) => doc.lock().await.clone(),
                    None => return Err("carrier running without trust doc".to_string()),
                };
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let carrier_snapshot = { c_doc.lock().await.clone() };
                let (new_doc, new_kt) = plan_fleet_epoch_bump(
                    &trust_now,
                    &carrier_snapshot,
                    keys.newest().epoch,
                    seed,
                    now_ms,
                )?;
                let new_epoch = new_doc.epoch;
                {
                    let mut doc = c_doc.lock().await;
                    if new_doc.epoch <= doc.epoch {
                        return Err(format!(
                            "bump raced a newer epoch ({} <= {})",
                            new_doc.epoch, doc.epoch
                        ));
                    }
                    *doc = new_doc;
                }
                keys.install(std::sync::Arc::new(new_kt));
                c_engine.notify_dirty();
                if let Err(e) = c_engine.flush_now().await {
                    tracing::warn!(
                        error = %e,
                        "post-revoke epoch bump: carrier flush failed; dirty latch will retry"
                    );
                }
                Ok::<u32, String>(new_epoch)
            };
            match bump.await {
                Ok(epoch) => tracing::info!(epoch, "fleet KeyTree epoch bumped after revoke"),
                Err(e) => tracing::warn!(
                    error = %e,
                    "post-revoke fleet epoch bump failed — panel staleness banner is the retry surface"
                ),
            }
        } else if loaded.master_seed.is_some() {
            tracing::warn!(
                "post-revoke fleet epoch bump skipped: node not running — \
                 panel staleness banner offers the manual rotate"
            );
        }
    }

    emit("owner-devices-updated");

    // ZEB-668 S3: nudge the retire-deposit sweeper so the community
    // retire-announce goes out immediately (the trust engine's on_applied
    // fires on REMOTE merges only — a local revocation would otherwise wait
    // for the next restart's startup pass). Best-effort: the sweeper is
    // level-triggered, so a dropped nudge only costs latency.
    if let Some(tx) = &retire_nudge {
        let _ = tx.try_send(());
    }

    if is_self {
        complete_self_revoke_terminal(&revoked_flag, &emit, owner_sync, fleet_net, trust_engine)
            .await;
    }
    Ok(())
}

/// ZEB-685 (S3): best-effort fan-out of a master `RevocationCert` (+ its paired
/// `EnrollmentCert`) to every `Active` friend's DM tunnel, so their §5.2 cutoff
/// rejects device D's DMs even with no shared community. Skips silently when the
/// revoked device has no enrollment on record (nothing to pair) or the wire
/// build fails — the revoke already landed; this is the additive DM-only signal.
async fn push_revocation_to_friends(
    crdt_state: &std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    mgr: &std::sync::Arc<crate::tunnel_manager::TunnelManager>,
    butler: Option<&std::sync::Arc<dyn crate::butler_deposit::ButlerDepositClient>>,
    trust_snapshot: &harmony_owner::state::OwnerState,
    revocation: &RevocationCert,
) {
    let Some(enrollment) = trust_snapshot.enrollments.get(&revocation.target).cloned() else {
        tracing::warn!(
            target = %hex::encode(revocation.target),
            "ZEB-685: no enrollment for revoked device; skipping friend RevocationPush"
        );
        return;
    };
    let packet = crate::dm_envelope::build_revocation_push_packet(revocation.clone(), enrollment);
    let wire = match crate::dm_envelope::encode_packet(&packet) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = ?e, "ZEB-685: encode RevocationPush failed; skipping push");
            return;
        }
    };
    let targets = {
        let s = crdt_state.lock().await;
        s.active_friend_owners()
    };
    if targets.is_empty() {
        return;
    }
    tracing::info!(
        friends = targets.len(),
        "ZEB-685: pushing device revocation to DM friends"
    );
    for owner in targets {
        crate::iroh_tunnel_dm_transport::send_packet_to_owner_tunnels(
            crdt_state, mgr, owner, &wire,
        )
        .await;
        // ZEB-691: also deposit to the friend's own butler set (their always-on
        // fleet) so an OFFLINE DM-only friend recovers the revocation on
        // reconnect: butler acceptor pre-validates + persists under `revoke_key`,
        // and the recipient's inbox sweeper re-verifies + applies + notify_dirty.
        // Best-effort — a `None` butler (iroh unbound) or a failing deposit
        // simply skips; the live-tunnel path above is untouched. The zero
        // `entry_id`/`space_id` are inert for a revocation (the butler keys by
        // inner content via `revoke_key`; this direct deposit does not ride the
        // outbox retry loop).
        if let Some(butler) = butler {
            let req = crate::butler_deposit::ButlerDepositRequest {
                entry_id: crate::owner_state_types::OutboxEntryId([0u8; 16]),
                recipient_owner: owner,
                space_id: crate::owner_state_types::SpaceId([0u8; 16]),
                message_cid: None,
                cidnotify_packet: None,
                invite_packet: None,
                revocation_push: Some(wire.clone()),
                grant_push: None,
                grant_revoke: None,
                now_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            };
            let _ = butler.deposit(&req).await;
        }
    }
}

/// Terminal state for a self-revoked device: latch once, tell the UI, then
/// stop fleet engines (hygiene — enforcement is receiver-side; matches the
/// S1 detector's halt set). Every step is idempotent, so the pending-retry
/// path can re-run it safely.
async fn complete_self_revoke_terminal(
    revoked_flag: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    emit: &std::sync::Arc<dyn Fn(&str) + Send + Sync>,
    owner_sync: Option<std::sync::Arc<crate::owner_state_sync::SyncEngine>>,
    fleet_net: Option<
        std::sync::Arc<crate::fleet_sync::FleetSyncEngine<crate::fleet_net::FleetNetDoc>>,
    >,
    trust_engine: Option<
        std::sync::Arc<crate::fleet_sync::FleetSyncEngine<harmony_owner::state::OwnerState>>,
    >,
) {
    if !revoked_flag.swap(true, std::sync::atomic::Ordering::AcqRel) {
        emit("device-revoked-self");
    }
    if let Some(engine) = owner_sync {
        if let Err(e) = engine.shutdown().await {
            tracing::error!(error = %e, "revoke_device: owner-state engine shutdown failed");
        }
    }
    if let Some(engine) = fleet_net {
        if let Err(e) = engine.shutdown().await {
            tracing::error!(error = %e, "revoke_device: fleet-net engine shutdown failed");
        }
    }
    if let Some(engine) = trust_engine {
        if let Err(e) = engine.shutdown().await {
            tracing::error!(error = %e, "revoke_device: trust engine shutdown failed");
        }
    }
}

/// ZEB-445-style shared IPC/RPC seam for `revoke_device`.
pub(crate) async fn revoke_device_impl(
    state: &std::sync::Mutex<crate::NodeState>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    device_vk_hex: String,
    reason: String,
) -> Result<(), String> {
    let emit: std::sync::Arc<dyn Fn(&str) + Send + Sync> =
        std::sync::Arc::new(move |name: &str| {
            crate::node_event_sink::emit_ser(&*sink, name, &serde_json::Value::Null);
        });
    revoke_device_inner(state, prod_keychain, emit, device_vk_hex, reason).await
}

#[tauri::command]
pub async fn revoke_device(
    app: tauri::AppHandle,
    device_vk_hex: String,
    reason: String,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<(), String> {
    revoke_device_impl(
        state.inner(),
        std::sync::Arc::new(app),
        device_vk_hex,
        reason,
    )
    .await
}

#[tauri::command]
pub async fn mint_owner_identity(
    app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<MintIpcResult, String> {
    // ZEB-445: wrap the AppHandle as the event sink (same shape as the
    // `start_node` command wrapper) and delegate to the shared IPC/RPC seam.
    let sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink> =
        std::sync::Arc::new(app.clone());
    mint_owner_identity_impl(state.inner(), sink, Some(app), None).await
}

/// ZEB-445: shared IPC/RPC seam. `wry_handle` is Some in the GUI (keeps
/// voting/dfrost registries alive across the restart) and None headless.
///
/// Forwards to the testable inner fn (symmetric with `start_node_inner`).
/// The restart step is injected as a closure so the inner fn can be
/// driven from a headless integration test (where a real `AppHandle<Wry>`
/// cannot be constructed). Production (this seam) passes the real node
/// restart.
///
/// ZEB-428: the real keychain is acquired HERE (production seam) and
/// injected, mirroring pairing/persist.rs's install_joiner_state — the
/// inner fn must never construct it internally, so test drivers can't
/// reach the developer's real credential store. (In test-fixtures builds
/// `KeychainStore::new()` itself refuses, so RPC-driven test mints fall
/// back to the encrypted-file store.)
pub(crate) async fn mint_owner_identity_impl(
    state: &Mutex<crate::NodeState>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    wry_handle: Option<tauri::AppHandle<tauri::Wry>>,
    // ZEB-719: owned NodeState handle threaded into the mint's node RESTART so a
    // headless node that mints (every agent-testing node mints on first run) keeps
    // Tier-2 auto-exec wired across that restart. `None` in the GUI (uses wry_handle).
    owned_state: Option<std::sync::Arc<Mutex<crate::NodeState>>>,
) -> Result<MintIpcResult, String> {
    mint_owner_identity_inner(state, KeychainStore::new().ok(), || async {
        crate::start_node_inner(
            None,
            sink.clone(),
            wry_handle.clone(),
            state,
            owned_state.clone(),
        )
        .await
        .map(|_| ())
    })
    .await
}

/// Core of `mint_owner_identity`, extracted for testability (mirrors
/// `start_node_inner`). Flow: stop node → mint+persist (under the
/// owner-state write lock) → restart node.
///
/// `restart` performs the node restart given the already-persisted owner
/// state on disk. Production supplies a closure that calls
/// `crate::start_node_inner`; tests supply a closure that records invocation
/// (so they can assert "restart happens after mint, with cbor on disk") or
/// deliberately fails (to lock the no-rollback invariant below).
///
/// `keychain` is injected by the caller (ZEB-428): production passes
/// `KeychainStore::new().ok()`, the test shim passes `None`. The inner fn
/// must never construct the real store itself — the OS keychain is a
/// process-global resource that a test's HOME-to-tempdir redirect cannot
/// scope, and an internal `new()` here once let a full-suite run overwrite
/// a developer's real owner identity.
pub(crate) async fn mint_owner_identity_inner<F, Fut>(
    state: &Mutex<crate::NodeState>,
    keychain: Option<KeychainStore>,
    restart: F,
) -> Result<MintIpcResult, String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let identity_dir = resolve_identity_dir()?;
    let display_name = "this device".to_string();

    // Idempotent failure if already minted — existing guard, kept. The hard
    // gate (frontend) means this is normally unreachable, but a race or a
    // direct DevicesPanel call could hit it.
    if identity_dir.join("owner_state.cbor").exists() {
        return Err(
            "Owner identity already exists on this device. Wipe via Settings to re-mint."
                .to_string(),
        );
    }

    // ── Phase 1: stop the node ──────────────────────────────────────────
    // ZEB-338: mint takes responsibility for the node lifecycle so the user
    // never has to "stop the node" by hand (the old require_node_stopped
    // dead-end). `stop_inner` is async-context-safe — it drives its async
    // shutdown on an ephemeral runtime inside std::thread::scope, so calling
    // it from this async fn does NOT panic with a nested runtime.
    // `None` = stop unconditionally (no generation check).
    crate::stop_inner(state, None);

    // ── Phase 2: mint + persist ─────────────────────────────────────────
    // Held under OWNER_STATE_WRITE_LOCK to serialize concurrent mints.
    // metadata-before-irreversible-write note (feedback_metadata_before_
    // irreversible_write): the cbor + keychain write here IS the desired
    // irreversible write. If Phase 3 (restart) fails afterward we do NOT roll
    // it back — rolling back would lose the user's freshly minted identity
    // (spec §7.1). The cost of a failed restart is a manual relaunch, which
    // is strictly better than identity loss.
    // ZEB-796: a freshly-minted identity must not inherit the previous
    // identity's privacy posture (discoverability etc.) — `connectivity-settings.json`
    // is keyed to the app-data dir, not the identity, so it outlives any single
    // identity. Resolve its path up front so the mint's blocking closure can
    // reset it right after the new identity is persisted. Resolution failure is
    // non-fatal: the reset is best-effort and must never block a mint.
    let connectivity_settings_path = match crate::resolve_app_data_dir() {
        Ok(dir) => Some(dir.join("connectivity-settings.json")),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "mint: could not resolve app data dir for the ZEB-796 privacy-posture reset; skipping (a new identity may inherit the prior posture until the next settings change)"
            );
            None
        }
    };
    let mint_dir = identity_dir.clone();
    let mint_result = run_blocking(move || {
        // Hold the process-wide owner-state write mutex for the entire
        // check-and-write window. Without this, concurrent mints could both
        // observe an absent owner_state.cbor and race to write competing
        // OwnerStates; pairing-persist callers (ZEB-199) take the same lock
        // for the same reason on the load+save side. Recover from
        // poisoning so a panic in one handler doesn't brick future writes
        // (mirrors PR-61's preview_cache_lock policy).
        let _owner_write_guard = OWNER_STATE_WRITE_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // Re-check under the lock (TOCTOU: another caller could have minted
        // between the outer check and acquiring the lock).
        if mint_dir.join("owner_state.cbor").exists() {
            return Err(
                "Owner identity already exists on this device. Wipe via Settings to re-mint."
                    .to_string(),
            );
        }
        let MintResult {
            state: owner_state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(now_unix()).map_err(|e| format!("mint_owner: {e}"))?;
        let master_seed: Zeroizing<[u8; 32]> = Zeroizing::new(*recovery_artifact.as_bytes());
        save_owner_state_atomic(
            &mint_dir,
            &owner_state,
            &device_signing_key,
            Some(&*master_seed),
            keychain,
        )?;
        // ZEB-796: now that the new identity is persisted, reset the
        // identity-scoped privacy/trust posture to product defaults (preserving
        // the machine's relay infra). Serialize this whole-file read-modify-write
        // behind the process-global settings write lock (ZEB-629): a mint is a new
        // writer, so a concurrent settings mutator (relay setter,
        // presence/discoverable toggle, boot reconcile) must not interleave and
        // resurrect a stale toggle. `blocking_lock` is correct here — this closure
        // runs on the blocking pool (`run_blocking` = `spawn_blocking`). Lock order
        // is OWNER_STATE_WRITE_LOCK → settings lock; no settings-lock holder ever
        // acquires OWNER_STATE_WRITE_LOCK, so nesting introduces no cycle.
        //
        // Best-effort: the identity is already written (no rollback — spec §7.1),
        // so a reset failure must NOT fail the mint.
        if let Some(ref cs_path) = connectivity_settings_path {
            let _settings_guard = crate::connectivity_settings_write_lock().blocking_lock();
            if let Err(reset_err) =
                crate::connectivity_settings::ConnectivitySettings::reset_privacy_posture_for_new_identity(cs_path)
            {
                // The reset write failed; the inherited file (possibly the prior
                // identity's `identity_discoverable=true`) may still be on disk.
                // ZEB-881: post-flip a MISSING settings file loads Default =
                // discoverable ON, so we can NO LONGER "fail safe" by deleting the
                // file — deletion would leave the degraded mint discoverable.
                // Restore the deliberate fail-closed-on-error intent by writing an
                // EXPLICIT invisible posture (discoverable OFF, relays preserved),
                // so a mint whose settings write failed never broadcasts the new
                // identity. The happy path keeps the ON product default; only this
                // error path fails closed. Report the outcome — a swallowed error
                // here would hide a privacy-fail-open.
                match crate::connectivity_settings::ConnectivitySettings::persist_fail_closed(cs_path) {
                    Ok(()) => tracing::error!(
                        path = %cs_path.display(),
                        error = %reset_err,
                        "mint: could not reset privacy posture for the new identity; wrote a fail-closed (non-discoverable) settings file so it stays invisible until the user opts in via Settings → Network"
                    ),
                    // The fail-closed write ALSO failed (the disk fault that failed
                    // the reset likely persists). Remove the inherited file as a
                    // last resort so we at least don't load the PRIOR identity's
                    // posture, and surface LOUDLY — a missing file now loads Default
                    // (discoverable ON), so this doubly-degraded path can leave the
                    // identity discoverable (mirrors `load_or_default`'s loud fail).
                    Err(fail_closed_err) => match std::fs::remove_file(cs_path) {
                        Err(remove_err) if remove_err.kind() == std::io::ErrorKind::NotFound => {
                            tracing::warn!(
                                path = %cs_path.display(),
                                reset_error = %reset_err,
                                fail_closed_error = %fail_closed_err,
                                "mint: privacy-posture reset and fail-closed write both failed, but no settings file exists to inherit — the new identity boots on defaults (discoverable ON)"
                            )
                        }
                        Ok(()) => tracing::error!(
                            path = %cs_path.display(),
                            reset_error = %reset_err,
                            fail_closed_error = %fail_closed_err,
                            "mint: reset AND fail-closed write failed; removed the inherited settings file as a last resort — the new identity now loads Default (discoverable ON). Verify the intended posture in Settings → Network"
                        ),
                        Err(remove_err) => tracing::error!(
                            path = %cs_path.display(),
                            reset_error = %reset_err,
                            fail_closed_error = %fail_closed_err,
                            remove_error = %remove_err,
                            "mint: FAILED to reset privacy posture, to write a fail-closed fallback, AND to remove the inherited settings file — the new identity may load the prior posture; connectivity-settings.json needs manual cleanup"
                        ),
                    },
                }
            }
        }
        let token = insert_token(master_seed.clone());
        let loaded = LoadedOwnerState {
            state: owner_state,
            device_signing_key,
            master_seed: Some(master_seed),
            fleet_keytree: None,
        };
        Ok(MintIpcResult {
            // Mint happens before the node restarts — fleet-net is not yet
            // running so every fleet-joined field is absent (fresh identity).
            state: build_owner_state_view(
                &loaded,
                display_name,
                FleetJoin::default(),
                QuorumJoin::default(),
            ),
            recovery_token: token.to_string(),
        })
    })
    .await?;

    // ── Phase 3: restart the node — now loads owner_state.cbor ──────────
    // NO ROLLBACK on failure: the mint above already wrote the identity to
    // disk. If the restart errors we surface it but leave the minted
    // identity in place (see Phase 2 note + spec §7.1).
    restart()
        .await
        .map_err(|e| format!("Node restart failed after mint: {e}"))?;

    Ok(mint_result)
}

/// Test-only public shim over [`mint_owner_identity_inner`] so headless
/// integration tests (a separate crate, no `pub(crate)` visibility) can
/// drive the mint lifecycle with an injected restart closure. Never
/// compiled into production (gated behind `test-fixtures`).
///
/// ZEB-428: the shim hard-codes `keychain: None` — the mint persists
/// through the encrypted-file fallback inside the test's tempdir HOME,
/// never the developer's real OS keychain. (Defense-in-depth: even if a
/// future caller bypassed this shim, `KeychainStore::new()` refuses in
/// test-fixtures builds.)
#[cfg(feature = "test-fixtures")]
pub async fn mint_owner_identity_inner_for_test<F, Fut>(
    state: &Mutex<crate::NodeState>,
    restart: F,
) -> Result<MintIpcResult, String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    mint_owner_identity_inner(state, None, restart).await
}

#[tauri::command]
pub async fn export_owner_recovery_file_to_path(
    recovery_token: String,
    path_token: String,
    passphrase: String,
    comment: Option<String>,
) -> Result<ExportInfo, String> {
    // Validate passphrase length BEFORE consuming any token (existing).
    // Use Unicode codepoint count (not byte count) so the "characters"
    // error wording matches the check, and so multibyte passphrases
    // (emoji, CJK) round-trip identically with the JS frontend's
    // [...str].length check.
    if passphrase.chars().count() < MIN_RECOVERY_PASSPHRASE_LEN {
        return Err(format!(
            "Recovery passphrase must be at least {MIN_RECOVERY_PASSPHRASE_LEN} characters."
        ));
    }
    // Validate comment length BEFORE consuming any token (existing).
    // 256-BYTE cap matches harmony-owner's hard limit on the underlying
    // field. Frontend mirrors with a TextEncoder byte count before submit.
    let comment_validated = match comment {
        Some(c) if c.len() > MAX_RECOVERY_COMMENT_BYTES => {
            return Err(format!(
                "Recovery comment must be at most {MAX_RECOVERY_COMMENT_BYTES} bytes."
            ));
        }
        c => c,
    };
    let recovery_uuid: Uuid = recovery_token
        .parse()
        .map_err(|e| format!("invalid recovery token: {e}"))?;
    let path_uuid: Uuid = path_token
        .parse()
        .map_err(|e| format!("invalid path token: {e}"))?;
    run_blocking(move || {
        // Consume path_token FIRST so a downstream seed-token consumption
        // failure does not leave a path token live in the cache pointing
        // at the user's chosen file (ZEB-194 ordering invariant — see test
        // `export_consumes_path_token_even_when_seed_token_invalid`).
        let out = crate::owner_state::take_path_token(&path_uuid).ok_or_else(|| {
            "Save path token expired or invalid. Please re-trigger backup.".to_string()
        })?;
        let seed = take_token(&recovery_uuid).ok_or_else(|| {
            "Recovery token expired or invalid. Please re-trigger backup from the Devices panel."
                .to_string()
        })?;
        let secret = SecretString::from(passphrase);
        let artifact = RecoveryArtifact::from_seed(*seed);
        let id_hash = artifact.master_pubkey_bundle().identity_hash();
        let metadata = RecoveryMetadata {
            // ZEB-180: stamp the export time on the GUI backup path too (the
            // CLI exporters already do), so a later GUI restore can surface it
            // via RestoreInfo.minted_at. Same source-of-truth helper.
            mint_at: Some(crate::recovery_cli::mint_timestamp_secs()),
            comment: comment_validated,
        };
        let bytes = artifact
            .to_encrypted_file(&secret, &metadata)
            .map_err(|e| format!("encrypt recovery file: {e}"))?;
        crate::identity::write_atomic_0600(&out, &bytes)
            .map_err(|e| format!("write {}: {e}", out.display()))?;
        Ok(ExportInfo {
            identity_hash: hex::encode(id_hash),
            byte_len: bytes.len() as u64,
            path: out.display().to_string(),
        })
    })
    .await
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueTokenResult {
    pub recovery_token: String,
}

#[tauri::command]
pub async fn issue_owner_recovery_token(
    _app: tauri::AppHandle,
) -> Result<IssueTokenResult, String> {
    let identity_dir = resolve_identity_dir()?;
    run_blocking(move || {
        let loaded = load_owner_state(&identity_dir, KeychainStore::new().ok())?
            .ok_or_else(|| "Owner identity has not been minted on this device.".to_string())?;
        let seed = loaded.master_seed.ok_or_else(|| {
            "Master seed has been wiped from this device — backup is no longer possible."
                .to_string()
        })?;
        let token = insert_token(seed);
        Ok(IssueTokenResult {
            recovery_token: token.to_string(),
        })
    })
    .await
}

/// ZEB-196: drop an issued-but-unconsumed recovery token from the server-side
/// cache.
///
/// `closeBackup` in the Devices panel calls this (fire-and-forget) when the
/// user cancels the backup modal. Without it, a token that was issued but never
/// consumed lingers in `TOKEN_CACHE` for its full 5-minute TTL and can
/// LRU-evict a *legitimate* live token once the 8-slot cache fills — e.g. a
/// token freshly minted for the mint→backup happy path that the user has not
/// yet consumed (the eviction concern from PR #62 round-5 review).
///
/// Idempotent: revoking an already-consumed, expired, or never-existed token is
/// a no-op success. `take_token` removes the entry if present and returns
/// `None` otherwise, so a double-revoke (or a revoke racing the export that
/// consumed the same token) is inherently safe. The client fires this
/// best-effort and never blocks the modal close on the result. A malformed
/// (non-UUID) token string is the one error case — it can only arise from a
/// caller bug, never from normal token aging.
#[tauri::command]
pub async fn revoke_owner_recovery_token(recovery_token: String) -> Result<(), String> {
    let recovery_uuid: Uuid = recovery_token
        .parse()
        .map_err(|e| format!("invalid recovery token: {e}"))?;
    // Consume-and-discard. No disk I/O (in-memory cache mutex only), so this
    // needs neither `run_blocking` nor `AppHandle`.
    let _ = take_token(&recovery_uuid);
    Ok(())
}

/// Wire DTO for the owner recovery-phrase reveal (ZEB-650 slice 2).
///
/// `owner_id` exists ONLY so the webview can cross-check the words against
/// the owner it is currently displaying. It must never be rendered: it is a
/// 32-hex-char run, which the WelcomeModal redaction invariant
/// (`/[0-9a-f]{32,}/` never in `innerHTML`) forbids in the DOM.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerMnemonicDto {
    pub words: Vec<String>,
    pub owner_id: String,
}

/// Testable core of [`export_owner_mnemonic_words`] (ZEB-428 seam: tests
/// inject `keychain: None` + `HARMONY_PASSPHRASE`; only the command wrapper
/// constructs the real keychain).
pub(crate) fn export_owner_mnemonic_dto(
    identity_dir: &Path,
    keychain: Option<KeychainStore>,
) -> Result<OwnerMnemonicDto, String> {
    let (words, owner_id) =
        crate::recovery_cli::export_owner_mnemonic_words_with_keychain(identity_dir, keychain)?;
    Ok(OwnerMnemonicDto {
        words,
        owner_id: hex::encode(owner_id),
    })
}

/// Return the 24 BIP39 owner-mnemonic words + owner id for the GUI reveal
/// (ZEB-650 slice 2). The first and only command returning owner seed
/// material to the webview; the renderer shows the words only behind an
/// explicit user reveal action (`OwnerPhraseReveal.svelte`), and the IPC
/// fires only on that action — never on mount.
///
/// Inherits all three gates from
/// [`crate::recovery_cli::export_owner_mnemonic_words_with_keychain`]:
/// owner minted; master seed still on device (the `canBackUp` condition);
/// seed↔owner-id invariant.
#[tauri::command]
pub async fn export_owner_mnemonic_words(
    _app: tauri::AppHandle,
) -> Result<OwnerMnemonicDto, String> {
    let identity_dir = resolve_identity_dir()?;
    run_blocking(move || export_owner_mnemonic_dto(&identity_dir, KeychainStore::new().ok())).await
}

/// Derive the owner-id (hex) that restoring the given 24-word mnemonic would
/// re-adopt, WITHOUT writing anything. The GUI restore wizard shows this for
/// confirmation and compares it against the device's current owner-id (ZEB-454).
#[tauri::command]
pub async fn preview_owner_mnemonic_identity(words: Vec<String>) -> Result<String, String> {
    // Wipe the renderer-supplied plaintext words on drop (Vec<String>: Zeroize).
    let words = Zeroizing::new(words);
    run_blocking(move || crate::recovery_cli::preview_owner_mnemonic_owner_id(&words)).await
}

/// Re-adopt the owner identity from its 24-word master-seed mnemonic (the GUI
/// analog of `harmony-app restore owner-mnemonic`). Same guards as the headless
/// path: refuses to overwrite an existing owner unless `force`, and refuses even
/// with `force` if the mnemonic derives a DIFFERENT owner-id. Returns the
/// restored owner-id hex. The renderer reloads afterwards so a fresh `start_node`
/// loads the re-minted owner_state (ZEB-454).
#[tauri::command]
pub async fn restore_owner_mnemonic_from_words(
    words: Vec<String>,
    force: bool,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<String, String> {
    // Wipe the renderer-supplied plaintext words on drop (Vec<String>: Zeroize).
    let words = Zeroizing::new(words);
    let identity_dir = resolve_identity_dir()?;

    // Preflight FIRST (CodeRabbit #339): a read-only validate + overwrite-guard
    // check, so a doomed restore (bad words / refused identity / corrupt marker)
    // returns WITHOUT needlessly stopping the running node. The `pre_words` clone
    // is also `Zeroizing` (wiped after the check).
    let pre_dir = identity_dir.clone();
    let pre_words = words.clone();
    run_blocking(move || {
        crate::recovery_cli::preflight_owner_mnemonic_restore(&pre_dir, &pre_words, force)
    })
    .await?;

    // Preflight passed → committed to writing. Enforce node lifecycle before the
    // irreversible rewrite (CodeAnt #339): stop the running node FIRST so the OLD
    // identity's engines (notably the ZEB-342 liveness refresher) cannot write a
    // competing owner_state into the gap and clobber the restore. Mirrors
    // mint_owner_identity's Phase-1 stop-before-persist. `stop_inner` is
    // async-context-safe (drives shutdown on an ephemeral runtime inside
    // std::thread::scope), and `None` stops unconditionally. The renderer reloads
    // on success, re-running `start_node` (which itself does stop ->
    // reload-identity -> respawn) to come up on the restored identity.
    crate::stop_inner(state.inner(), None);
    run_blocking(move || {
        crate::recovery_cli::restore_owner_mnemonic_from_words_with_keychain(
            &identity_dir,
            &words,
            force,
            KeychainStore::new().ok(),
        )
    })
    .await
}

/// Files under the active identity dir that make up **this device's** owner
/// identity + fleet state. [`reset_local_identity`] snapshots then moves these
/// aside so the next boot classifies as `missing` (fresh onboarding), mirroring
/// the manual operator workaround in ZEB-835 / ZEB-836.
///
/// Deliberately NOT included: `identity.key` (the network/device address — a
/// fresh mint re-adopts it harmlessly) and the `profiles/` subtree (other
/// named identities are isolated and must never be touched).
///
/// Filenames that have a single canonical constant reference it, so a rename
/// there breaks *this* build (real compile-time coupling, PR #571 review). The
/// CRDT/replay pair (`owner_state_crdt.cbor` / `state_root_replay.cbor`, joined
/// as literals in `lib.rs`) and the `*_secret` encrypted-file names
/// (`device_sk.enc` / `master_seed.enc`, literal args to `load_secret`) have no
/// single owning constant, so they stay literals here.
const OWNER_RESET_FILES: &[&str] = &[
    crate::owner_state::OWNER_STATE_FILENAME,
    "owner_state_crdt.cbor",
    "state_root_replay.cbor",
    "device_sk.enc",
    "master_seed.enc",
    crate::owner_state::FLEET_KEYTREE_FILENAME,
    crate::fleet_net_persist::FLEET_NET_FILENAME,
    crate::fleet_net_persist::FLEET_NET_REPLAY_FILENAME,
];

/// Pick a backup dir that does not already exist, so two resets that land in the
/// same wall-clock second never target the same directory and clobber each
/// other's snapshot (PR #571 review). Race-free: the sole caller holds
/// [`OWNER_STATE_WRITE_LOCK`] across the pick-then-create window.
fn unique_reset_backup_dir(identity_dir: &Path) -> Result<PathBuf, String> {
    let base = format!("_reset-backup-{}", now_unix());
    let mut candidate = identity_dir.join(&base);
    for n in 1..=1000 {
        if !candidate.exists() {
            return Ok(candidate);
        }
        candidate = identity_dir.join(format!("{base}-{n}"));
    }
    Err(format!(
        "could not allocate a unique reset backup dir under {}",
        identity_dir.display()
    ))
}

/// Move `src` → `dst`. A plain rename within the same identity dir (the reset
/// backup lives inside it) never crosses a filesystem, but fall back to
/// copy+remove defensively so an unusual mount can't fail the reset.
fn move_file(src: &Path, dst: &Path) -> Result<(), String> {
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    std::fs::copy(src, dst)
        .map_err(|e| format!("copy {} -> {}: {e}", src.display(), dst.display()))?;
    std::fs::remove_file(src).map_err(|e| format!("remove {} after copy: {e}", src.display()))?;
    Ok(())
}

/// ZEB-835 / ZEB-836: self-serve escape from a *terminal* owner-boot failure.
///
/// Both failure modes (device key in neither store; loaded device key not in
/// `enrollments`) leave the app stuck on the "Couldn't start Harmony" / Retry
/// modal with no in-app recovery. This is the "Reset this device & start fresh"
/// action behind that modal's "Still stuck?" disclosure: it snapshots this
/// device's owner identity to a timestamped backup dir, moves it aside so the
/// next boot onboards fresh (`load_owner_state` → `Ok(None)` → `missing`), and
/// best-effort clears the OS keychain vault. Returns `Some(backup dir path)` so
/// the UI can tell the user where their old identity went, or `None` when there
/// was nothing to move (no owner files present — so no backup was created and
/// the "backed up to a folder" copy would be untrue).
///
/// Snapshot-then-wipe posture (approved 2026-07-30): a mistaken reset is
/// dev-recoverable from the backup dir. Keychain-only secrets are NOT snapshotted
/// (the OS keychain holds the vault as plaintext CBOR — copying it to a file
/// would leak the seed); their recovery path is the recovery phrase, which is
/// why Restore-from-phrase is offered alongside Reset.
#[tauri::command]
pub async fn reset_local_identity(
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<Option<String>, String> {
    let identity_dir = resolve_identity_dir()?;
    // Stop the node FIRST so the old identity's engines (the ZEB-342 liveness
    // refresher, fleet sync) cannot rewrite owner_state.cbor into the gap and
    // resurrect the broken state. Mirrors mint_owner_identity's Phase-1 and
    // restore_owner_mnemonic_from_words. `None` = stop unconditionally.
    crate::stop_inner(state.inner(), None);
    let backup =
        run_blocking(move || reset_local_identity_inner(&identity_dir, prod_keychain())).await?;
    Ok(backup.map(|p| p.to_string_lossy().into_owned()))
}

/// Core of [`reset_local_identity`], extracted for testability. `keychain` is
/// injected (ZEB-428): production passes [`prod_keychain`], tests pass `None` or
/// a mock. Held under [`OWNER_STATE_WRITE_LOCK`] so a straggler write can't race
/// the wipe. Idempotent and safe on partial state — a missing file is skipped;
/// an already-clean identity dir + empty keychain is a successful no-op that
/// returns `Ok(None)` (the backup dir is created lazily, only if there is
/// something to move, so no empty dir is left behind).
pub(crate) fn reset_local_identity_inner(
    identity_dir: &Path,
    keychain: Option<KeychainStore>,
) -> Result<Option<PathBuf>, String> {
    let _guard = OWNER_STATE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());

    // Cross-process exclusion (Greptile #571): hold the same identity write-lock
    // the identity writers take, so a reset does not race another harmony process
    // sharing this identity dir. If one is mid-write, this fails fast with
    // "another harmony-app process is writing…" rather than moving files out from
    // under it. (The GUI is single-instance; this covers a mixed headless case.)
    let backup_dir = crate::identity::with_identity_dir_write_guard(identity_dir, || {
        let backup_dir = unique_reset_backup_dir(identity_dir)?;
        let mut created_backup = false;
        for name in OWNER_RESET_FILES {
            let src = identity_dir.join(name);
            if !src.exists() {
                continue;
            }
            if !created_backup {
                std::fs::create_dir_all(&backup_dir).map_err(|e| {
                    format!("create reset backup dir {}: {e}", backup_dir.display())
                })?;
                created_backup = true;
            }
            // On a mid-list failure, owner_state.cbor (first) may already be moved
            // — the live dir is now "missing" — so surface the backup location, or
            // the already-moved files are undiscoverable (this is a last resort).
            move_file(&src, &backup_dir.join(name)).map_err(|e| {
                format!(
                    "{e} (partial reset: files already moved are in {})",
                    backup_dir.display()
                )
            })?;
        }
        Ok(created_backup.then_some(backup_dir))
    })?;

    // Best-effort clear the OS keychain owner secrets. `prod_keychain` returns
    // `None` for named profiles (file-vault, no keychain) and in test builds, so
    // those correctly skip this. The on-disk owner_state.cbor removal above is
    // the authoritative onboarding gate — a keychain-clear failure must NOT fail
    // the reset (mirrors delete_stale_keychain_after_restore's posture).
    if let Some(kc) = keychain {
        for (item, err) in kc.delete_all() {
            tracing::warn!(
                keychain_item = item,
                error = %err,
                "reset_local_identity: could not clear keychain item — manual cleanup may be needed"
            );
        }
    }

    Ok(backup_dir)
}

/// Remove every direct child of `dir` whose file name is not in `excluded`,
/// recursively for subdirectories. Attempts EVERY child even when some fail, so
/// one un-removable entry can't abort the wipe — then returns the collected
/// failures (joined) so the caller can report a truthful partial result instead
/// of a false "clean" (ZEB-842; CWE-459). A missing `dir` is a clean no-op.
fn remove_dir_children_except(dir: &Path, excluded: &[&str]) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("read dir {}: {e}", dir.display())),
    };
    let mut failures: Vec<String> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                failures.push(format!("read entry in {}: {e}", dir.display()));
                continue;
            }
        };
        if excluded
            .iter()
            .any(|x| std::ffi::OsStr::new(x) == entry.file_name())
        {
            continue;
        }
        let path = entry.path();
        let removed = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(e) = removed {
            failures.push(format!("{}: {e}", path.display()));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// Names kept when wiping the **identity** dir: `profiles/` isolates sibling
/// identities (ZEB-586); `identity.enc.lock` is the cross-process lock the wipe
/// itself holds via [`crate::identity::with_identity_dir_write_guard`] — deleting
/// the held path lets another process lock a *replacement* file mid-wipe on Unix,
/// silently breaking the exclusion (CodeRabbit). It is a zero-byte lock,
/// recreated on next boot.
const IDENTITY_ERASE_EXCLUDED: &[&str] = &["profiles", "identity.enc.lock"];

/// Names kept when wiping the **app-data** dir: `profiles/` isolates sibling
/// profiles (ZEB-586); `logs/` is the live tracing sink the webview-only reload
/// keeps open, so deleting it just races the appender (diagnostic, not user
/// content); `api/` holds the cross-process profile lock a running `serve` /
/// GUI-API server holds (`api::lock::acquire(app_data_dir/api)`) — deleting it
/// mid-hold breaks the lock on Unix and fails on Windows (Qodo). None of the
/// three carry the active profile's private user content.
const APP_DATA_ERASE_EXCLUDED: &[&str] = &["profiles", "logs", "api"];

/// ZEB-842 clean-slate: hard-delete the active profile's identity dir and
/// app-data dir children (minus the exclusions above) and best-effort clear the
/// keychain. No snapshot — the recovery phrase is the identity backup (contrast
/// [`reset_local_identity_inner`], which snapshots-then-moves to recover a
/// bricked boot). Held under [`OWNER_STATE_WRITE_LOCK`] + the identity
/// write-guard like the reset, so a straggler write can't race the wipe.
///
/// Returns `Err` if any child could not be removed: a partial wipe must not read
/// as success, or the frontend would reload into a false clean slate while
/// private content remains (CodeRabbit, CWE-459). It still attempts BOTH dirs and
/// the keychain before reporting — a failure in one does not skip the rest. A
/// failure to acquire the identity write-guard aborts before anything is deleted
/// (another process is mid-identity-write). `keychain` is injected (ZEB-428):
/// production passes [`prod_keychain`], tests pass `None`.
pub(crate) fn erase_all_local_data_inner(
    identity_dir: &Path,
    app_data_dir: &Path,
    keychain: Option<KeychainStore>,
) -> Result<(), String> {
    let _guard = OWNER_STATE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());

    let mut failures: Vec<String> = Vec::new();

    // Identity dir under the same cross-process write guard the identity writers
    // take (Greptile #571). A guard-acquisition failure aborts the whole wipe —
    // deleting nothing beats racing another process's identity write. Removal
    // failures inside are collected (not fatal here) so app-data + keychain
    // cleanup still run and the final result is truthful.
    crate::identity::with_identity_dir_write_guard(identity_dir, || {
        if let Err(e) = remove_dir_children_except(identity_dir, IDENTITY_ERASE_EXCLUDED) {
            failures.push(e);
        }
        Ok::<(), String>(())
    })?;

    // App-data dir: no cross-process guard exists; the caller's stop_inner
    // quiesced THIS process's engines. `api/` is excluded so a separately running
    // serve process's held profile lock is not yanked.
    if let Err(e) = remove_dir_children_except(app_data_dir, APP_DATA_ERASE_EXCLUDED) {
        failures.push(e);
    }

    // Best-effort clear the OS keychain owner secrets. A failure warns but must
    // NOT fail the wipe — the on-disk owner_state.cbor removal is the
    // authoritative onboarding gate (mirrors reset_local_identity_inner).
    if let Some(kc) = keychain {
        for (item, err) in kc.delete_all() {
            tracing::warn!(
                keychain_item = item,
                error = %err,
                "erase_all_local_data: could not clear keychain item — manual cleanup may be needed"
            );
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "erase incomplete — {} item(s) could not be removed: {}",
            failures.len(),
            failures.join("; ")
        ))
    }
}

/// Backend confirmation gate for [`erase_all_local_data`]: the typed-confirm is
/// re-checked server-side so a stray or compromised IPC invoke can't trigger an
/// irreversible wipe without the literal confirmation — the frontend gate alone
/// is not a security boundary (Qodo, defense-in-depth).
fn erase_all_confirmed(confirm: &str) -> bool {
    confirm == "ERASE"
}

/// ZEB-842: user-confirmed clean-slate wipe (typed-confirm on the GUI). Removes
/// this device's identity AND every per-profile app-data cache (mail, avatars,
/// follows, card store, …) for the active profile — the "Erase all local data"
/// action in Settings and the boot-failure modal. `confirm` must equal the
/// literal `"ERASE"` (re-validated here, not only in the frontend). Stops the
/// node FIRST so its engines cannot rewrite a cache into the gap (mirrors
/// `reset_local_identity`), then wipes. `None` = stop unconditionally.
#[tauri::command]
pub async fn erase_all_local_data(
    confirm: String,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<(), String> {
    if !erase_all_confirmed(&confirm) {
        return Err("erase_all_local_data requires confirm = \"ERASE\"".to_string());
    }
    let identity_dir = resolve_identity_dir()?;
    let app_data_dir = crate::resolve_app_data_dir()?;
    crate::stop_inner(state.inner(), None);
    run_blocking(move || erase_all_local_data_inner(&identity_dir, &app_data_dir, prod_keychain()))
        .await
}

/// Read this device's persisted owner-id (hex), or `null` if no `owner_state.cbor`
/// exists. Read-only and key-free (delegates to the same
/// [`crate::owner_state::read_persisted_owner_id`] the restore overwrite-guard
/// uses), so it works even when `start_node` failed to load the identity
/// (ZEB-835/836). The startup-error "Still stuck?" restore path calls this to
/// classify a pasted recovery phrase as a same-owner re-adoption (→ `force`
/// overwrite of the broken state).
///
/// A corrupt/unreadable marker returns `Err` (NOT `null`): the marker exists, so
/// a fresh (`force=false`) restore would be *refused* by the overwrite guard.
/// The UI catches the error and steers the user to Reset, which handles a corrupt
/// marker. (Swallowing it to `null` would silently break the restore path.)
#[tauri::command]
pub async fn owner_id_on_disk() -> Result<Option<String>, String> {
    let identity_dir = resolve_identity_dir()?;
    run_blocking(move || {
        crate::owner_state::read_persisted_owner_id(&identity_dir).map(|id| id.map(hex::encode))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state::{
        clear_path_token_cache, clear_token_cache, insert_path_token, take_path_token,
    };
    use harmony_owner::state::OwnerState;
    use serial_test::serial;

    #[test]
    fn now_unix_checked_is_some_for_a_normal_clock() {
        // ZEB-721: `now_unix_checked` returns `None` ONLY for a pre-epoch host
        // clock (before 1970) — exactly when the panel path must SKIP the liveness
        // refresh rather than stamp a `timestamp = 0` cert. On any normal clock it
        // is `Some`. We deliberately assert nothing about the absolute value: the
        // test must not depend on the runner's wall clock being set to any era
        // (that would be flaky in exactly the clock-skew scenarios this PR is about).
        assert!(
            now_unix_checked().is_some(),
            "a normal (post-epoch) host clock must yield Some"
        );
    }

    #[test]
    fn reset_local_identity_inner_snapshots_then_wipes_and_is_idempotent() {
        // ZEB-835/836: the reset must (1) move every owner/fleet file into a
        // timestamped backup dir (snapshot-then-wipe — a mistaken reset is
        // dev-recoverable), (2) leave the live identity dir with no owner_state
        // gate so the next boot onboards fresh, (3) NOT touch the network
        // identity (identity.key) or the profiles/ subtree, and (4) be a clean
        // no-op on a second run. Keychain is injected as `None` (ZEB-428) so the
        // file behavior is exercised in isolation.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Seed every owner/fleet file with distinguishable contents.
        for name in OWNER_RESET_FILES {
            std::fs::write(root.join(name), format!("contents-of-{name}")).unwrap();
        }
        // Files that must survive: the network identity and another profile.
        std::fs::write(root.join("identity.key"), b"network-identity").unwrap();
        std::fs::create_dir_all(root.join("profiles").join("other")).unwrap();
        std::fs::write(
            root.join("profiles").join("other").join("owner_state.cbor"),
            b"other-profile-owner",
        )
        .unwrap();

        // Reset returns Some(backup_dir) because there were files to move.
        let backup = reset_local_identity_inner(root, None)
            .unwrap()
            .expect("a reset that moved files must report its backup dir");

        // 1+2: every owner/fleet file moved out of the live dir into the backup,
        // contents preserved byte-for-byte.
        for name in OWNER_RESET_FILES {
            assert!(
                !root.join(name).exists(),
                "{name} must be removed from the live identity dir"
            );
            let backed_up = backup.join(name);
            assert!(
                backed_up.exists(),
                "{name} must be snapshotted into the backup dir"
            );
            assert_eq!(
                std::fs::read(&backed_up).unwrap(),
                format!("contents-of-{name}").into_bytes(),
                "{name} contents must survive the snapshot intact"
            );
        }
        // 3: untouched survivors.
        assert!(
            root.join("identity.key").exists(),
            "identity.key (network identity) must NOT be reset"
        );
        assert!(
            root.join("profiles")
                .join("other")
                .join("owner_state.cbor")
                .exists(),
            "another profile's identity must NOT be touched"
        );

        // 4: idempotent — a second reset over the now-clean dir succeeds, moves
        // nothing (→ Ok(None), no backup dir created), and leaves the survivors.
        assert_eq!(
            reset_local_identity_inner(root, None).expect("second reset must be Ok"),
            None,
            "a no-op reset must report no backup dir"
        );
        for name in OWNER_RESET_FILES {
            assert!(
                !root.join(name).exists(),
                "{name} must stay absent after an idempotent second reset"
            );
        }
        assert!(root.join("identity.key").exists());
        assert!(root
            .join("profiles")
            .join("other")
            .join("owner_state.cbor")
            .exists());
    }

    #[test]
    fn erase_all_local_data_inner_wipes_identity_and_app_data() {
        // ZEB-842: erase-all is wholesale (unlike reset's curated snapshot) — it
        // hard-deletes EVERY child of the identity dir and app-data dir,
        // including identity.key and any prior `_reset-backup-*` snapshot, minus
        // the per-dir exclusions (covered by the preservation test below).
        // Keychain injected `None` (ZEB-428).
        let id_tmp = tempfile::tempdir().unwrap();
        let ad_tmp = tempfile::tempdir().unwrap();
        let id = id_tmp.path();
        let ad = ad_tmp.path();

        // identity dir: owner files + network identity + a prior reset snapshot.
        std::fs::write(id.join("owner_state.cbor"), b"owner").unwrap();
        std::fs::write(id.join("master_seed.enc"), b"seed").unwrap();
        std::fs::write(id.join("identity.key"), b"network").unwrap();
        std::fs::create_dir_all(id.join("_reset-backup-1700")).unwrap();
        std::fs::write(
            id.join("_reset-backup-1700").join("owner_state.cbor"),
            b"old",
        )
        .unwrap();

        // app-data dir: the full per-profile cache spread (design Appendix A).
        std::fs::create_dir_all(ad.join("mail")).unwrap();
        std::fs::write(ad.join("mail").join("blob"), b"dm").unwrap();
        std::fs::create_dir_all(ad.join("avatars")).unwrap();
        std::fs::write(ad.join("avatars").join("a.png"), b"img").unwrap();
        std::fs::write(ad.join("follows.json"), b"[]").unwrap();
        std::fs::write(ad.join("content-index.json"), b"{}").unwrap();
        std::fs::write(ad.join("profile_cards.deadbeef.cbor"), b"cards").unwrap();
        std::fs::create_dir_all(ad.join("mint")).unwrap();
        std::fs::write(ad.join("mint").join("x"), b"m").unwrap();
        std::fs::write(ad.join("storage_records.json"), b"{}").unwrap();
        std::fs::write(ad.join("storage_ledger.json"), b"{}").unwrap();
        std::fs::write(ad.join("connectivity-settings.json"), b"{}").unwrap();
        std::fs::write(ad.join("vine_pull.cbor"), b"v").unwrap();
        std::fs::write(ad.join("last_backup.json"), b"{}").unwrap();

        erase_all_local_data_inner(id, ad, None).expect("erase must succeed");

        for p in [
            id.join("owner_state.cbor"),
            id.join("master_seed.enc"),
            id.join("identity.key"),
            id.join("_reset-backup-1700"),
            ad.join("mail"),
            ad.join("avatars"),
            ad.join("follows.json"),
            ad.join("content-index.json"),
            ad.join("profile_cards.deadbeef.cbor"),
            ad.join("mint"),
            ad.join("storage_records.json"),
            ad.join("storage_ledger.json"),
            ad.join("connectivity-settings.json"),
            ad.join("vine_pull.cbor"),
            ad.join("last_backup.json"),
        ] {
            assert!(!p.exists(), "{} must be erased", p.display());
        }
        // The dir roots themselves remain (we delete children, not the dirs).
        assert!(id.exists() && ad.exists(), "the dir roots remain");
    }

    #[test]
    fn erase_all_local_data_inner_preserves_isolation_and_infra() {
        // The per-dir exclusions: the identity wipe keeps `profiles/` (ZEB-586)
        // and the held `identity.enc.lock` (CodeRabbit — deleting the lock the
        // wipe holds breaks cross-process exclusion). The app-data wipe keeps
        // `profiles/`, the live `logs/` sink, and the `api/` profile-lock dir a
        // running serve/GUI-API holds (Qodo). Everything else goes.
        let id_tmp = tempfile::tempdir().unwrap();
        let ad_tmp = tempfile::tempdir().unwrap();
        let id = id_tmp.path();
        let ad = ad_tmp.path();

        // Common to both roots: a sibling profile (survives) + a removable sibling.
        for root in [id, ad] {
            std::fs::create_dir_all(root.join("profiles").join("other")).unwrap();
            std::fs::write(
                root.join("profiles").join("other").join("owner_state.cbor"),
                b"sibling",
            )
            .unwrap();
            std::fs::write(root.join("removable"), b"x").unwrap();
        }
        // Identity dir: the held cross-process lock must survive.
        std::fs::write(id.join("identity.enc.lock"), b"").unwrap();
        // App-data dir: the live tracing sink and the api/ profile-lock dir survive.
        std::fs::create_dir_all(ad.join("logs")).unwrap();
        std::fs::write(ad.join("logs").join("app.log"), b"log").unwrap();
        std::fs::create_dir_all(ad.join("api")).unwrap();
        std::fs::write(ad.join("api").join("serve.lock"), b"").unwrap();

        erase_all_local_data_inner(id, ad, None).expect("erase must succeed");

        for root in [id, ad] {
            assert!(
                root.join("profiles")
                    .join("other")
                    .join("owner_state.cbor")
                    .exists(),
                "sibling profile under {} must survive",
                root.display()
            );
            assert!(
                !root.join("removable").exists(),
                "removable sibling under {} must be erased",
                root.display()
            );
        }
        assert!(
            id.join("identity.enc.lock").exists(),
            "identity.enc.lock must survive so the cross-process guard isn't broken"
        );
        assert!(
            ad.join("logs").join("app.log").exists(),
            "logs/ must survive (live tracing sink)"
        );
        assert!(
            ad.join("api").join("serve.lock").exists(),
            "api/ (profile-lock dir) must survive"
        );
    }

    #[test]
    fn erase_all_local_data_inner_is_a_clean_noop_on_empty_dirs() {
        // A wipe on already-clean dirs is a successful no-op (idempotent) and
        // creates no user data (CodeRabbit: assert the roots end clean). The
        // identity write-guard leaves its own (excluded) `identity.enc.lock`
        // behind — that lone zero-byte lock is the only permitted residue.
        let id_tmp = tempfile::tempdir().unwrap();
        let ad_tmp = tempfile::tempdir().unwrap();
        erase_all_local_data_inner(id_tmp.path(), ad_tmp.path(), None)
            .expect("no-op erase must be Ok");
        assert_eq!(
            std::fs::read_dir(ad_tmp.path()).unwrap().count(),
            0,
            "app-data root must end empty"
        );
        for entry in std::fs::read_dir(id_tmp.path()).unwrap() {
            assert_eq!(
                entry.unwrap().file_name(),
                std::ffi::OsStr::new("identity.enc.lock"),
                "identity root may retain only the guard's lock file"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn erase_all_local_data_inner_reports_err_on_partial_failure() {
        // CWE-459: a child that cannot be removed must make the wipe return Err,
        // so the frontend surfaces it instead of reloading into a false clean
        // slate — while still attempting the other entries (best-effort).
        use std::os::unix::fs::PermissionsExt;
        let id_tmp = tempfile::tempdir().unwrap();
        let ad_tmp = tempfile::tempdir().unwrap();
        let ad = ad_tmp.path();

        // A read-only subdir: its child can't be unlinked, so remove_dir_all of
        // the subdir fails and the failure must propagate.
        let locked = ad.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("blob"), b"stuck").unwrap();
        std::fs::write(ad.join("removable"), b"x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();

        // Root bypasses DAC, so detect whether the perms actually bite here; only
        // then can we assert the failure path (CI runners are non-root).
        let perms_enforced = std::fs::write(locked.join("probe"), b"p").is_err();

        let result = erase_all_local_data_inner(id_tmp.path(), ad, None);

        // Restore write perms so tempdir Drop can clean up regardless of outcome.
        let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));

        // The removable sibling is still attempted even though `locked` fails.
        assert!(
            !ad.join("removable").exists(),
            "best-effort: other entries are still removed after a failure"
        );
        if perms_enforced {
            assert!(result.is_err(), "a partial wipe must not report success");
        }
    }

    #[test]
    fn erase_all_confirmed_requires_the_exact_literal() {
        // Backend defense-in-depth gate: only the exact literal passes.
        assert!(erase_all_confirmed("ERASE"));
        assert!(!erase_all_confirmed("erase"));
        assert!(!erase_all_confirmed("ERASE "));
        assert!(!erase_all_confirmed(""));
        assert!(!erase_all_confirmed("ERASE ALL"));
    }

    #[test]
    fn unique_reset_backup_dir_never_collides_within_the_same_second() {
        // PR #571 review (Qodo): two resets in the same wall-clock second must
        // land in DIFFERENT backup dirs, or the second overwrites the first
        // snapshot — defeating snapshot-then-wipe. The uniqueness check runs
        // under OWNER_STATE_WRITE_LOCK, so simulating "the first dir already
        // exists" is the exact race the lock serializes.
        let dir = tempfile::tempdir().unwrap();
        let first = unique_reset_backup_dir(dir.path()).unwrap();
        std::fs::create_dir_all(&first).unwrap(); // first reset created its dir
        let second = unique_reset_backup_dir(dir.path()).unwrap();
        assert_ne!(
            first, second,
            "same-second backup dirs must differ so the earlier snapshot survives"
        );
        assert!(
            !second.exists(),
            "the fresh candidate must not already exist"
        );
    }

    /// RAII guard: sets an env var on construction, removes it on drop (even on panic).
    /// Prevents a panicking test from leaking HARMONY_PASSPHRASE into the next
    /// `#[serial]` test.
    struct EnvVarGuard {
        name: &'static str,
    }

    impl EnvVarGuard {
        fn set(name: &'static str, value: &str) -> Self {
            std::env::set_var(name, value);
            Self { name }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            std::env::remove_var(self.name);
        }
    }

    #[test]
    #[serial]
    fn export_with_too_short_passphrase_errors_without_consuming_token() {
        clear_token_cache();
        clear_path_token_cache();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
        let recovery_uuid = insert_token(Zeroizing::new([0xCDu8; 32]));
        let path_uuid = insert_path_token(PathBuf::from("/tmp/should-not-write"));
        // Use a too-short passphrase.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(export_owner_recovery_file_to_path(
            recovery_uuid.to_string(),
            path_uuid.to_string(),
            "short".into(),
            None,
        ));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("at least"),
            "error must mention passphrase length; got: {err}"
        );
        // Token must NOT have been consumed (validation precedes take).
        assert!(
            take_token(&recovery_uuid).is_some(),
            "weak-passphrase rejection must not consume token"
        );
        // Path token must ALSO survive: validation runs before any cache
        // consumption, so neither token must be consumed on this path.
        assert!(
            take_path_token(&path_uuid).is_some(),
            "weak-passphrase rejection must not consume path token"
        );
    }

    #[test]
    #[serial]
    fn export_with_invalid_token_errors() {
        clear_token_cache();
        clear_path_token_cache();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
        let bogus = Uuid::new_v4();
        let path_uuid = insert_path_token(PathBuf::from("/tmp/should-not-write"));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(export_owner_recovery_file_to_path(
            bogus.to_string(),
            path_uuid.to_string(),
            "passphrase-12+".into(),
            None,
        ));
        assert!(result.is_err());
        let err = result.unwrap_err();
        // Path token consumes first and succeeds, so this error originates
        // from the recovery_token consumption.
        assert!(
            err.contains("expired") || err.contains("invalid"),
            "actual: {err}"
        );
    }

    #[test]
    #[serial]
    fn comment_over_cap_rejected() {
        clear_token_cache();
        clear_path_token_cache();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
        let recovery_uuid = insert_token(Zeroizing::new([0xEEu8; 32]));
        let path_uuid = insert_path_token(PathBuf::from("/tmp/should-not-write"));
        let comment = "x".repeat(257);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(export_owner_recovery_file_to_path(
            recovery_uuid.to_string(),
            path_uuid.to_string(),
            "passphrase-12+".into(),
            Some(comment),
        ));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("256") || err.contains("at most"),
            "error must mention comment cap; got: {err}"
        );
        // Token must NOT have been consumed.
        assert!(
            take_token(&recovery_uuid).is_some(),
            "comment-over-cap rejection must not consume token"
        );
        // Path token must ALSO survive: validation runs before any cache
        // consumption, so neither token must be consumed on this path.
        assert!(
            take_path_token(&path_uuid).is_some(),
            "comment-over-cap rejection must not consume path token"
        );
    }

    #[test]
    #[serial]
    fn export_with_invalid_path_token_errors() {
        clear_token_cache();
        clear_path_token_cache();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
        let recovery_uuid = insert_token(Zeroizing::new([0xAAu8; 32]));
        let bogus_path_uuid = Uuid::new_v4();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(export_owner_recovery_file_to_path(
            recovery_uuid.to_string(),
            bogus_path_uuid.to_string(),
            "passphrase-12+".into(),
            None,
        ));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_lowercase().contains("path token")
                && (err.contains("expired") || err.contains("invalid")),
            "error must mention path token expired/invalid; got: {err}"
        );
        // Recovery token MUST survive: path-token consumption happens first
        // and fails, so seed-token consumption never runs.
        assert!(
            take_token(&recovery_uuid).is_some(),
            "invalid path-token must not consume recovery token"
        );
    }

    #[test]
    #[serial]
    fn export_consumes_path_token_even_when_seed_token_invalid() {
        // Pins the consumption ORDER: path_token taken first; if that succeeds
        // and seed-token consumption fails, the path token is still gone (so a
        // later replay of either token is impossible). This documents the
        // invariant against future refactors that might reorder consumption.
        clear_token_cache();
        clear_path_token_cache();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
        let bogus_recovery_uuid = Uuid::new_v4();
        let path_uuid = insert_path_token(PathBuf::from("/tmp/zeb194-ordering-test.bin"));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(export_owner_recovery_file_to_path(
            bogus_recovery_uuid.to_string(),
            path_uuid.to_string(),
            "passphrase-12+".into(),
            None,
        ));
        assert!(result.is_err());
        // Path token MUST have been consumed (taken first) even though the
        // overall command failed.
        assert!(
            take_path_token(&path_uuid).is_none(),
            "path token must be consumed even when subsequent seed-token consumption fails"
        );
    }

    #[test]
    #[serial]
    fn export_consumes_both_tokens_on_success() {
        // Drives a real write_atomic_0600 into a tempdir to verify the
        // happy path end-to-end: both tokens consumed AND ExportInfo.path
        // echoes the user-confirmed save location. The tempdir Drop
        // cleans up at scope exit.
        clear_token_cache();
        clear_path_token_cache();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("recovery.bin");
        let recovery_uuid = insert_token(Zeroizing::new([0xBBu8; 32]));
        let path_uuid = insert_path_token(out.clone());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(export_owner_recovery_file_to_path(
            recovery_uuid.to_string(),
            path_uuid.to_string(),
            "passphrase-12+".into(),
            None,
        ));
        assert!(result.is_ok(), "export must succeed; got: {result:?}");
        // Both caches must no longer hold the consumed UUIDs.
        assert!(
            take_token(&recovery_uuid).is_none(),
            "recovery token must be consumed"
        );
        assert!(
            take_path_token(&path_uuid).is_none(),
            "path token must be consumed"
        );
        // ExportInfo.path must echo the chosen path.
        let info = result.unwrap();
        assert_eq!(info.path, out.display().to_string());
    }

    #[test]
    #[serial]
    fn export_stamps_mint_at_on_gui_path() {
        // ZEB-180: the GUI export command must stamp mint_at (not leave it
        // None), so a later GUI restore surfaces the backup date. Decode the
        // written file and assert the metadata carries a recent timestamp.
        clear_token_cache();
        clear_path_token_cache();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("recovery.bin");
        let recovery_uuid = insert_token(Zeroizing::new([0xC1u8; 32]));
        let path_uuid = insert_path_token(out.clone());
        let before = crate::recovery_cli::mint_timestamp_secs();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(export_owner_recovery_file_to_path(
            recovery_uuid.to_string(),
            path_uuid.to_string(),
            "passphrase-12+".into(),
            Some("gui-backup".into()),
        ))
        .expect("export must succeed");

        let bytes = std::fs::read(&out).unwrap();
        let restored = RecoveryArtifact::from_encrypted_file(
            &bytes,
            &SecretString::from("passphrase-12+".to_string()),
        )
        .expect("decode");
        let minted = restored
            .metadata
            .mint_at
            .expect("GUI export must stamp mint_at");
        assert!(
            minted >= before,
            "mint_at ({minted}) must be >= the pre-export timestamp ({before})"
        );
        assert_eq!(restored.metadata.comment.as_deref(), Some("gui-backup"));
    }

    // ── ZEB-196: revoke_owner_recovery_token ─────────────────────────────

    #[test]
    #[serial]
    fn revoke_consumes_live_token() {
        clear_token_cache();
        let recovery_uuid = insert_token(Zeroizing::new([0x11u8; 32]));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(revoke_owner_recovery_token(recovery_uuid.to_string()));
        assert!(
            result.is_ok(),
            "revoke of a live token must succeed; got: {result:?}"
        );
        // Token is gone: a subsequent take finds nothing.
        assert!(
            take_token(&recovery_uuid).is_none(),
            "revoke must consume the token so it can no longer be redeemed"
        );
    }

    #[test]
    #[serial]
    fn revoke_is_idempotent() {
        clear_token_cache();
        let recovery_uuid = insert_token(Zeroizing::new([0x22u8; 32]));
        let rt = tokio::runtime::Runtime::new().unwrap();
        // First revoke consumes it; a second revoke of the same UUID must ALSO
        // be Ok (a no-op), not an error — closeBackup can fire more than once.
        assert!(rt
            .block_on(revoke_owner_recovery_token(recovery_uuid.to_string()))
            .is_ok());
        assert!(
            rt.block_on(revoke_owner_recovery_token(recovery_uuid.to_string()))
                .is_ok(),
            "second revoke of an already-consumed token must be a no-op success"
        );
    }

    #[test]
    #[serial]
    fn revoke_absent_token_is_ok() {
        clear_token_cache();
        let never_issued = Uuid::new_v4();
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(
            rt.block_on(revoke_owner_recovery_token(never_issued.to_string()))
                .is_ok(),
            "revoking a token that was never issued must succeed (idempotent)"
        );
    }

    #[test]
    #[serial]
    fn revoke_rejects_malformed_token() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(revoke_owner_recovery_token("not-a-uuid".to_string()));
        assert!(result.is_err(), "a non-UUID token string must be rejected");
        assert!(
            result.unwrap_err().contains("invalid recovery token"),
            "error must name the malformed token"
        );
    }

    #[test]
    #[serial]
    fn export_after_revoke_fails() {
        // Acceptance criterion (PR #62 round 5): once closeBackup revokes, the
        // previously-issued token cannot be redeemed by a later export.
        clear_token_cache();
        clear_path_token_cache();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
        let recovery_uuid = insert_token(Zeroizing::new([0x33u8; 32]));
        let path_uuid = insert_path_token(PathBuf::from("/tmp/should-not-write-zeb196.bin"));
        let rt = tokio::runtime::Runtime::new().unwrap();
        // Revoke first (simulates modal cancel).
        assert!(rt
            .block_on(revoke_owner_recovery_token(recovery_uuid.to_string()))
            .is_ok());
        // A later export with the same recovery token must now fail. The path
        // token is consumed first (ZEB-194 ordering) and succeeds, so the error
        // originates from the now-absent recovery token — nothing is written.
        let result = rt.block_on(export_owner_recovery_file_to_path(
            recovery_uuid.to_string(),
            path_uuid.to_string(),
            "passphrase-12+".into(),
            None,
        ));
        assert!(result.is_err(), "export with a revoked token must fail");
        let err = result.unwrap_err();
        assert!(
            err.contains("expired") || err.contains("invalid"),
            "revoked-token export must report expired/invalid; got: {err}"
        );
    }

    /// ZEB-418 P2 round-2 (Greptile P1): cross-layer contract test pinning the
    /// device-ID FORMAT across the toggle round trip. The view's
    /// `device_vk_hex` must be the exact string that `set_butler_pin_inner`
    /// validates against (64-hex ed25519 verify key, the SP1 form the
    /// fleet-net doc keys on) AND the value that, fed back as
    /// `pinned_device_id_hex`, lights up `butler_pinned` on the same row.
    /// The pre-fix bug shipped `device_id` (the 16-byte identity hash) to the
    /// toggle — rejected for every device — and an opaque-ID test would miss
    /// it again, so this test derives everything from ONE enrollment-cert
    /// fixture.
    #[tokio::test]
    async fn device_vk_hex_round_trips_through_set_butler_pin() {
        use crate::fleet_net::FleetNetDoc;
        use harmony_owner::pubkey_bundle::PubKeyBundle;
        use std::collections::BTreeSet;

        // ── One fixture: mint an owner (1 enrollment) + enroll a 2nd device ─
        let MintResult {
            mut state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(1_700_000_000).expect("mint");
        let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());

        let joiner_sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let joiner_pubkey = PubKeyBundle::classical_only(joiner_sk.verifying_key().to_bytes());
        let joiner_cert = crate::pairing::cert::sign_enrollment_for_joiner(
            &master_seed,
            &state,
            joiner_pubkey,
            1_700_000_001,
        )
        .expect("sign joiner enrollment");
        let joiner_identity_hash_hex = hex::encode(joiner_cert.device_id);
        state.enrollments.insert(joiner_cert.device_id, joiner_cert);

        let loaded = LoadedOwnerState {
            state,
            device_signing_key,
            master_seed: Some(master_seed),
            fleet_keytree: None,
        };

        // ── (a) Build the view with no pin; take the joiner's device_vk_hex ─
        let view = build_owner_state_view(
            &loaded,
            "this device".into(),
            FleetJoin::default(),
            QuorumJoin::default(),
        );
        assert_eq!(view.devices.len(), 2, "mint device + joiner");
        let joiner_row = view
            .devices
            .iter()
            .find(|d| d.device_id == joiner_identity_hash_hex)
            .expect("joiner row present in view");
        let vk_hex = joiner_row.device_vk_hex.clone();
        // The two ID forms must be distinct: 64-hex VK vs 32-hex identity hash.
        assert_eq!(vk_hex.len(), 64, "device_vk_hex is the 64-hex VK form");
        assert_eq!(
            joiner_row.device_id.len(),
            32,
            "device_id is the 32-hex identity-hash form"
        );
        assert!(view.devices.iter().all(|d| !d.butler_pinned), "no pin yet");

        // ── (b) Derive the enrolled set EXACTLY as start_node does ──────────
        let enrolled: BTreeSet<String> = loaded
            .state
            .enrollments
            .values()
            .map(|cert| hex::encode(cert.device_pubkeys.classical.ed25519_verify))
            .collect();
        // Sanity: the identity-hash form (the pre-fix toggle payload) is NOT
        // in the enrolled set — that is the bug this test pins against.
        assert!(
            !enrolled.contains(&joiner_identity_hash_hex),
            "identity-hash form must not be a valid pin id"
        );

        // ── (c) set_butler_pin_inner must ACCEPT the view's device_vk_hex ───
        let doc = tokio::sync::Mutex::new(FleetNetDoc::default());
        crate::set_butler_pin_inner(&doc, &enrolled, Some(vk_hex.clone()), "self-dev", 1_000)
            .await
            .expect("set_butler_pin_inner must accept DeviceView.device_vk_hex");
        let pinned = doc.lock().await.pinned.clone();
        assert_eq!(pinned.as_deref(), Some(vk_hex.as_str()));

        // ── (d) Feed the doc's pinned value back; exactly the joiner pins ───
        let view2 = build_owner_state_view(
            &loaded,
            "this device".into(),
            FleetJoin {
                pinned,
                ..Default::default()
            },
            QuorumJoin::default(),
        );
        for d in &view2.devices {
            assert_eq!(
                d.butler_pinned,
                d.device_id == joiner_identity_hash_hex,
                "butler_pinned must be true for the pinned joiner only (row {})",
                d.device_id
            );
        }
    }

    /// ZEB-668 S4: single-enrollment loaded-state fixture for the fleet-join
    /// view tests (mirrors the round-trip test's mint recipe above).
    fn minted_loaded_state() -> LoadedOwnerState {
        let MintResult {
            state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(1_700_000_000).expect("mint");
        LoadedOwnerState {
            state,
            device_signing_key,
            master_seed: Some(Zeroizing::new(*recovery_artifact.as_bytes())),
            fleet_keytree: None,
        }
    }

    fn fixture_vk_hex(loaded: &LoadedOwnerState) -> String {
        let cert = loaded
            .state
            .enrollments
            .values()
            .next()
            .expect("mint enrollment present");
        hex::encode(cert.device_pubkeys.classical.ed25519_verify)
    }

    #[test]
    fn view_joins_petname_last_seen_and_connected() {
        let loaded = minted_loaded_state();
        let dev_vk_hex = fixture_vk_hex(&loaded);
        let ep = [0x42u8; 32];
        let mut fleet = FleetJoin::default();
        fleet.petnames.insert(dev_vk_hex.clone(), "KRILE".into());
        fleet.rows.insert(dev_vk_hex.clone(), (123_456, ep));
        fleet.connected_eps.insert(ep);

        let view =
            build_owner_state_view(&loaded, "this device".into(), fleet, QuorumJoin::default());
        let d = view
            .devices
            .iter()
            .find(|d| d.device_vk_hex == dev_vk_hex)
            .expect("fixture device row");
        assert_eq!(d.pet_name.as_deref(), Some("KRILE"));
        assert_eq!(d.last_seen_ms, Some(123_456));
        assert!(d.connected_now);
    }

    #[test]
    fn view_absent_fleet_row_yields_honest_nulls() {
        let loaded = minted_loaded_state();
        let view = build_owner_state_view(
            &loaded,
            "this device".into(),
            FleetJoin::default(),
            QuorumJoin::default(),
        );
        let d = &view.devices[0];
        assert_eq!(d.pet_name, None);
        assert_eq!(d.last_seen_ms, None);
        assert!(!d.connected_now);
    }

    #[test]
    fn view_cleared_petname_surfaces_as_some_empty_not_none() {
        // PR #454 round 1: a cleared petname (LWW tombstone, name: "") must
        // surface as Some("") — DISTINCT from None (never named). The panel's
        // one-shot local-label migration keys on exactly this distinction: a
        // cleared name must never be resurrected from a stale local label.
        let loaded = minted_loaded_state();
        let dev_vk_hex = fixture_vk_hex(&loaded);
        let mut fleet = FleetJoin::default();
        fleet.petnames.insert(dev_vk_hex.clone(), String::new());
        let view =
            build_owner_state_view(&loaded, "this device".into(), fleet, QuorumJoin::default());
        let d = view
            .devices
            .iter()
            .find(|d| d.device_vk_hex == dev_vk_hex)
            .expect("fixture device row");
        assert_eq!(d.pet_name.as_deref(), Some(""));
    }

    #[test]
    fn view_trims_whitespace_petname_from_remote_writers() {
        // PR #454 round 1 (Qodo): the local writer trims, but a remote
        // peer's LWW entry lands as stored. A whitespace-only name must
        // normalize to the cleared form, and padding must be stripped.
        let loaded = minted_loaded_state();
        let dev_vk_hex = fixture_vk_hex(&loaded);

        let mut fleet = FleetJoin::default();
        fleet.petnames.insert(dev_vk_hex.clone(), "   ".into());
        let view =
            build_owner_state_view(&loaded, "this device".into(), fleet, QuorumJoin::default());
        let d = view
            .devices
            .iter()
            .find(|d| d.device_vk_hex == dev_vk_hex)
            .expect("fixture device row");
        assert_eq!(
            d.pet_name.as_deref(),
            Some(""),
            "whitespace-only → cleared form"
        );

        let mut fleet = FleetJoin::default();
        fleet
            .petnames
            .insert(dev_vk_hex.clone(), "  KRILE  ".into());
        let view =
            build_owner_state_view(&loaded, "this device".into(), fleet, QuorumJoin::default());
        let d = view
            .devices
            .iter()
            .find(|d| d.device_vk_hex == dev_vk_hex)
            .expect("fixture device row");
        assert_eq!(d.pet_name.as_deref(), Some("KRILE"), "padding stripped");
    }

    #[test]
    #[serial]
    fn issue_token_errors_when_owner_state_does_not_exist() {
        clear_token_cache();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "issue-test-pp");
        let _dir = tempfile::tempdir().unwrap();
        // Note: this test cannot easily call issue_owner_recovery_token directly
        // because that command resolves identity_dir from real OS paths. Instead,
        // we test the underlying invariant: load_owner_state on an empty dir
        // returns Ok(None), and the command errors when None.
        let result = crate::owner_state::load_owner_state(_dir.path(), None);
        assert!(matches!(result, Ok(None)), "empty dir → Ok(None)");
    }

    // ── ZEB-650 slice 2: owner-mnemonic DTO seam ──

    /// Plant a minted owner (with master seed) in `dir`. Mirrors
    /// recovery_cli.rs::plant_owner_and_export_words minus the export.
    fn plant_owner(dir: &Path) -> (OwnerState, [u8; 32]) {
        let MintResult {
            state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(1_700_000_000).unwrap();
        let master_seed = *recovery_artifact.as_bytes();
        save_owner_state_atomic(dir, &state, &device_signing_key, Some(&master_seed), None)
            .unwrap();
        (state, master_seed)
    }

    #[test]
    #[serial]
    fn export_owner_mnemonic_dto_round_trips_words_and_owner_id() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-mnemonic-dto-test");
        let (state, master_seed) = plant_owner(dir.path());

        let dto = export_owner_mnemonic_dto(dir.path(), None).expect("export must succeed");
        assert_eq!(dto.words.len(), 24, "owner mnemonic is 24 words");
        assert_eq!(dto.owner_id, hex::encode(state.owner_id));
        // Words must round-trip to the same master seed (same invariant the
        // recovery_cli tests pin; kept here so the DTO layer cannot drift).
        let parsed = RecoveryArtifact::from_mnemonic(&dto.words.join(" ")).expect("words parse");
        assert_eq!(
            *parsed.as_bytes(),
            master_seed,
            "DTO words must encode the owner master seed"
        );
    }

    #[test]
    #[serial]
    fn export_owner_mnemonic_dto_errors_when_seed_wiped() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-mnemonic-dto-wiped");
        let MintResult {
            state,
            device_signing_key,
            ..
        } = mint_owner(1_700_000_000).unwrap();
        // Persist WITHOUT the master seed — the wiped/joiner model.
        save_owner_state_atomic(dir.path(), &state, &device_signing_key, None, None).unwrap();
        let err = export_owner_mnemonic_dto(dir.path(), None).unwrap_err();
        assert!(
            err.contains("wiped"),
            "wiped-seed error must surface: {err}"
        );
    }

    #[test]
    fn owner_mnemonic_dto_serializes_camel_case() {
        let dto = OwnerMnemonicDto {
            words: vec!["abandon".into()],
            owner_id: "ab".into(),
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert!(
            json.contains("\"ownerId\""),
            "camelCase key required: {json}"
        );
        assert!(json.contains("\"words\""));
    }
}

#[cfg(test)]
mod revoke_tests {
    use super::*;
    use harmony_owner::lifecycle::enroll_via_master;
    use harmony_owner::pubkey_bundle::PubKeyBundle;
    use harmony_owner::state::OwnerState;

    // Mint an owner (device A holds the seed) and enroll a second device B.
    // Returns (state, a_sk, seed, b_sk, b_vk_hex).
    fn two_device_fixture(
        now: u64,
    ) -> (
        OwnerState,
        ed25519_dalek::SigningKey,
        [u8; 32],
        ed25519_dalek::SigningKey,
        String,
    ) {
        let MintResult {
            mut state,
            recovery_artifact,
            device_signing_key: a_sk,
        } = mint_owner(now).expect("mint");
        let seed = *recovery_artifact.as_bytes();
        let b_sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let b_vk = b_sk.verifying_key().to_bytes();
        let enroll = enroll_via_master(
            &state,
            &recovery_artifact,
            &b_sk,
            PubKeyBundle::classical_only(b_vk),
            now,
            trust::DEFAULT_ACTIVE_WINDOW_SECS,
        )
        .expect("enroll");
        state
            .add_enrollment(
                enroll.enrollment_cert,
                now,
                trust::DEFAULT_ACTIVE_WINDOW_SECS,
            )
            .expect("add enrollment");
        for c in enroll.auto_vouch_certs {
            let _ = state.add_vouching(c);
        }
        (state, a_sk, seed, b_sk, hex::encode(b_vk))
    }

    /// Return the enrollment cert whose classical ed25519 matches `sk`.
    fn cert_for_sk<'a>(
        state: &'a OwnerState,
        sk: &ed25519_dalek::SigningKey,
    ) -> &'a harmony_owner::certs::EnrollmentCert {
        let vk = sk.verifying_key().to_bytes();
        state
            .enrollments
            .values()
            .find(|c| c.device_pubkeys.classical.ed25519_verify == vk)
            .expect("enrollment for signing key")
    }

    /// Stamp a device's active feed_binding into a fleet-net row so the revoke
    /// path can find + cut its feed. Returns the feed_id (N).
    fn stamp_test_feed_binding(
        doc: &mut crate::fleet_net::FleetNetDoc,
        sp1_key: &str,
        device_cert: &harmony_owner::certs::EnrollmentCert,
        device_sk: &ed25519_dalek::SigningKey,
    ) -> String {
        let n = harmony_identity::PrivateIdentity::generate(&mut rand::rngs::OsRng);
        let rec =
            crate::feed_authority::build_active_authority(&n, device_sk, device_cert, &[], 1_000)
                .expect("build active authority");
        let feed_id = rec.feed_id.clone();
        let json = serde_json::to_string(&rec).unwrap();
        doc.devices.insert(
            sp1_key.to_string(),
            crate::fleet_net::FleetNetRow {
                iroh_endpoint_id: [0u8; 32],
                home_relay: String::new(),
                seen_at: crate::owner_state_types::Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: String::new(),
                },
                feed_binding: Some(json),
            },
        );
        feed_id
    }

    /// Shared log of `(key_expr, payload)` pairs a publish-drain task records.
    type PublishLog = std::sync::Arc<std::sync::Mutex<Vec<(String, Vec<u8>)>>>;

    /// Drain `publish_rx`, recording `(key_expr, payload)` and acking each
    /// request. Returns `(join_handle, shared_log)`.
    fn spawn_publish_drain(
        mut publish_rx: tokio::sync::mpsc::Receiver<crate::event_loop::PublishRequest>,
    ) -> (tokio::task::JoinHandle<()>, PublishLog) {
        let log: PublishLog = Default::default();
        let log_c = log.clone();
        let h = tokio::spawn(async move {
            while let Some(req) = publish_rx.recv().await {
                log_c
                    .lock()
                    .unwrap()
                    .push((req.key_expr.clone(), req.payload.clone()));
                let _ = req.reply.send(Ok(()));
            }
        });
        (h, log)
    }

    #[test]
    fn feed_binding_for_device_finds_matching_row_and_skips_others() {
        use crate::fleet_net::{FleetNetDoc, FleetNetRow};
        use crate::owner_state_types::Hlc;
        let make_binding = |device_id_hex: &str| {
            serde_json::json!({
                "feedId": "feed-abcd",
                "ownerId": "00".repeat(16),
                "deviceId": device_id_hex,
                "publisherKey": "11".repeat(32),
                "nIdentityPub": "22".repeat(64),
                "enrollmentCborHex": "aa",
                "updatedAt": 1u64,
                "nSig": "33".repeat(64),
            })
            .to_string()
        };
        let row = |fb: Option<String>| FleetNetRow {
            iroh_endpoint_id: [0u8; 32],
            home_relay: String::new(),
            seen_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: String::new(),
            },
            feed_binding: fb,
        };
        let mut doc = FleetNetDoc::default();
        doc.devices.insert("sp1-a".into(), row(None));
        let match_id = "ab".repeat(16);
        doc.devices
            .insert("sp1-b".into(), row(Some(make_binding(&match_id))));
        doc.devices
            .insert("sp1-c".into(), row(Some("garbage".into())));

        assert!(feed_binding_for_device(&doc, &match_id).is_some());
        assert!(feed_binding_for_device(&doc, &"cd".repeat(16)).is_none());
    }

    #[tokio::test]
    async fn revoke_device_inner_master_revoke_publishes_feed_cutoff() {
        std::env::set_var("HARMONY_PASSPHRASE", "test-passphrase");
        let now = 1_700_000_000;
        let dir = tempfile::tempdir().unwrap();
        let (state, a_sk, seed, b_sk, b_vk_hex) = two_device_fixture(now);
        let b_cert = cert_for_sk(&state, &b_sk).clone();
        save_owner_state_atomic(dir.path(), &state, &a_sk, Some(&seed), None).unwrap();

        let mut fleet_doc = crate::fleet_net::FleetNetDoc::default();
        let feed_id = stamp_test_feed_binding(&mut fleet_doc, "sp1-b", &b_cert, &b_sk);
        let fleet_net_doc = std::sync::Arc::new(tokio::sync::Mutex::new(fleet_doc));

        let (publish_tx, publish_rx) =
            tokio::sync::mpsc::channel::<crate::event_loop::PublishRequest>(8);
        let (drain, published) = spawn_publish_drain(publish_rx);

        let node = std::sync::Mutex::new(crate::NodeState {
            identity_dir: Some(dir.path().to_path_buf()),
            publish_tx: Some(publish_tx),
            fleet_net_doc: Some(fleet_net_doc),
            ..crate::NodeState::default()
        });

        revoke_device_inner(
            &node,
            || None,
            std::sync::Arc::new(|_: &str| {}),
            b_vk_hex,
            "lost".into(),
        )
        .await
        .unwrap();

        drop(node);
        drain.await.unwrap();

        let pubs = published.lock().unwrap();
        let want_key = format!("harmony/vines/{feed_id}/authority");
        let hit = pubs
            .iter()
            .find(|(k, _)| *k == want_key)
            .expect("feed cut-off published");
        let rec: crate::feed_authority::FeedAuthorityRecord =
            serde_json::from_slice(&hit.1).unwrap();
        let v = crate::feed_authority::verify_authority(&rec, now).expect("cut-off verifies");
        assert!(v.revoked, "published authority marks B revoked");
    }

    #[tokio::test]
    async fn revoke_device_inner_no_feed_binding_publishes_nothing() {
        std::env::set_var("HARMONY_PASSPHRASE", "test-passphrase");
        let now = 1_700_000_000;
        let dir = tempfile::tempdir().unwrap();
        let (state, a_sk, seed, _b_sk, b_vk_hex) = two_device_fixture(now);
        save_owner_state_atomic(dir.path(), &state, &a_sk, Some(&seed), None).unwrap();

        let fleet_net_doc = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::fleet_net::FleetNetDoc::default(),
        ));
        let (publish_tx, publish_rx) =
            tokio::sync::mpsc::channel::<crate::event_loop::PublishRequest>(8);
        let (drain, published) = spawn_publish_drain(publish_rx);
        let node = std::sync::Mutex::new(crate::NodeState {
            identity_dir: Some(dir.path().to_path_buf()),
            publish_tx: Some(publish_tx),
            fleet_net_doc: Some(fleet_net_doc),
            ..crate::NodeState::default()
        });
        revoke_device_inner(
            &node,
            || None,
            std::sync::Arc::new(|_: &str| {}),
            b_vk_hex,
            "lost".into(),
        )
        .await
        .unwrap();
        drop(node);
        drain.await.unwrap();
        assert!(
            published.lock().unwrap().is_empty(),
            "no publish when no feed"
        );
    }

    #[tokio::test]
    async fn revoke_device_inner_self_revoke_publishes_cutoff_before_terminal() {
        std::env::set_var("HARMONY_PASSPHRASE", "test-passphrase");
        // Enroll recently so both devices are active at wall-clock now — the
        // self-revoke `lastDevice` guard is evaluated against the real clock.
        let now = now_unix() - 60;
        let dir = tempfile::tempdir().unwrap();
        let (state, _a_sk, _seed, b_sk, b_vk_hex) = two_device_fixture(now);
        let b_cert = cert_for_sk(&state, &b_sk).clone();
        // Persist as B, cert-only (no seed) → self-revoke path.
        save_owner_state_atomic(dir.path(), &state, &b_sk, None, None).unwrap();

        let mut fleet_doc = crate::fleet_net::FleetNetDoc::default();
        let feed_id = stamp_test_feed_binding(&mut fleet_doc, "sp1-b", &b_cert, &b_sk);
        let fleet_net_doc = std::sync::Arc::new(tokio::sync::Mutex::new(fleet_doc));

        let (publish_tx, mut publish_rx) =
            tokio::sync::mpsc::channel::<crate::event_loop::PublishRequest>(8);
        let log: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let log_pub = log.clone();
        let drain = tokio::spawn(async move {
            while let Some(req) = publish_rx.recv().await {
                log_pub
                    .lock()
                    .unwrap()
                    .push(format!("publish:{}", req.key_expr));
                let _ = req.reply.send(Ok(()));
            }
        });
        let log_emit = log.clone();
        let emit = std::sync::Arc::new(move |name: &str| {
            log_emit.lock().unwrap().push(format!("emit:{name}"));
        });
        let node = std::sync::Mutex::new(crate::NodeState {
            identity_dir: Some(dir.path().to_path_buf()),
            publish_tx: Some(publish_tx),
            fleet_net_doc: Some(fleet_net_doc),
            ..crate::NodeState::default()
        });
        revoke_device_inner(&node, || None, emit, b_vk_hex, "decommissioned".into())
            .await
            .unwrap();
        drop(node);
        drain.await.unwrap();

        let events = log.lock().unwrap();
        let pub_idx = events
            .iter()
            .position(|e| e == &format!("publish:harmony/vines/{feed_id}/authority"))
            .expect("feed cut-off published");
        let term_idx = events
            .iter()
            .position(|e| e == "emit:device-revoked-self")
            .expect("terminal emitted");
        assert!(
            pub_idx < term_idx,
            "feed cut-off must publish before terminal halt"
        );
    }

    #[tokio::test]
    async fn revoke_device_inner_retry_republishes_self_feed_cutoff() {
        std::env::set_var("HARMONY_PASSPHRASE", "test-passphrase");
        let now = 1_700_000_000;
        let dir = tempfile::tempdir().unwrap();
        let (mut state, _a_sk, _seed, b_sk, b_vk_hex) = two_device_fixture(now);
        let b_cert = cert_for_sk(&state, &b_sk).clone();
        let b_target = crate::owner_state::device_id_from_signing_key(&b_sk);

        // Strand B's self-revocation into the doc (added, terminal not latched).
        let cert = RevocationCert::sign_self(
            &b_sk,
            state.owner_id,
            b_target,
            now,
            RevocationReason::Decommissioned,
        )
        .unwrap();
        state
            .add_revocation(cert, now, trust::DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        save_owner_state_atomic(dir.path(), &state, &b_sk, None, None).unwrap();

        let trust_doc = std::sync::Arc::new(tokio::sync::Mutex::new(state));

        let mut fleet_doc = crate::fleet_net::FleetNetDoc::default();
        let feed_id = stamp_test_feed_binding(&mut fleet_doc, "sp1-b", &b_cert, &b_sk);
        let fleet_net_doc = std::sync::Arc::new(tokio::sync::Mutex::new(fleet_doc));

        let (publish_tx, publish_rx) =
            tokio::sync::mpsc::channel::<crate::event_loop::PublishRequest>(8);
        let (drain, published) = spawn_publish_drain(publish_rx);
        let node = std::sync::Mutex::new(crate::NodeState {
            identity_dir: Some(dir.path().to_path_buf()),
            owner_trust_doc: Some(trust_doc),
            publish_tx: Some(publish_tx),
            fleet_net_doc: Some(fleet_net_doc),
            ..crate::NodeState::default()
        });
        revoke_device_inner(
            &node,
            || None,
            std::sync::Arc::new(|_: &str| {}),
            b_vk_hex,
            "decommissioned".into(),
        )
        .await
        .unwrap();
        drop(node);
        drain.await.unwrap();

        let want_key = format!("harmony/vines/{feed_id}/authority");
        assert!(
            published
                .lock()
                .unwrap()
                .iter()
                .any(|(k, _)| *k == want_key),
            "retry arm republishes the self feed cut-off"
        );
    }

    #[tokio::test]
    async fn revoke_device_inner_sibling_already_revoked_republishes_feed_cutoff() {
        std::env::set_var("HARMONY_PASSPHRASE", "test-passphrase");
        let now = 1_700_000_000;
        let dir = tempfile::tempdir().unwrap();
        let (mut state, a_sk, seed, b_sk, b_vk_hex) = two_device_fixture(now);
        let b_cert = cert_for_sk(&state, &b_sk).clone();
        let b_target = crate::owner_state::device_id_from_signing_key(&b_sk);

        // Pre-revoke B (master-signed), as if a prior master-revoke landed the
        // trust state but its best-effort feed cut-off publish failed.
        let artifact = RecoveryArtifact::from_seed(seed);
        let cert = RevocationCert::sign_master(
            &artifact.master_signing_key(),
            artifact.master_pubkey_bundle(),
            b_target,
            now,
            RevocationReason::Lost,
        )
        .unwrap();
        state
            .add_revocation(cert, now, trust::DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        save_owner_state_atomic(dir.path(), &state, &a_sk, Some(&seed), None).unwrap();
        let trust_doc = std::sync::Arc::new(tokio::sync::Mutex::new(state));

        let mut fleet_doc = crate::fleet_net::FleetNetDoc::default();
        let feed_id = stamp_test_feed_binding(&mut fleet_doc, "sp1-b", &b_cert, &b_sk);
        let fleet_net_doc = std::sync::Arc::new(tokio::sync::Mutex::new(fleet_doc));

        let (publish_tx, publish_rx) =
            tokio::sync::mpsc::channel::<crate::event_loop::PublishRequest>(8);
        let (drain, published) = spawn_publish_drain(publish_rx);
        let node = std::sync::Mutex::new(crate::NodeState {
            identity_dir: Some(dir.path().to_path_buf()),
            owner_trust_doc: Some(trust_doc),
            publish_tx: Some(publish_tx),
            fleet_net_doc: Some(fleet_net_doc),
            ..crate::NodeState::default()
        });
        // A (seed-holder) re-revokes the already-revoked sibling B → the
        // idempotent arm must still re-drive the feed cut-off.
        revoke_device_inner(
            &node,
            || None,
            std::sync::Arc::new(|_: &str| {}),
            b_vk_hex,
            "lost".into(),
        )
        .await
        .unwrap();
        drop(node);
        drain.await.unwrap();

        let want_key = format!("harmony/vines/{feed_id}/authority");
        assert!(
            published
                .lock()
                .unwrap()
                .iter()
                .any(|(k, _)| *k == want_key),
            "sibling idempotent arm republishes the feed cut-off"
        );
    }

    #[test]
    fn parse_revoke_reason_maps_wire_values() {
        assert_eq!(
            parse_revoke_reason("decommissioned").unwrap(),
            RevocationReason::Decommissioned
        );
        assert_eq!(parse_revoke_reason("lost").unwrap(), RevocationReason::Lost);
        assert_eq!(
            parse_revoke_reason("compromised").unwrap(),
            RevocationReason::Compromised
        );
        let err = parse_revoke_reason("banana").unwrap_err();
        assert!(err.starts_with("invalidReason:"), "{err}");
    }

    #[test]
    fn plan_bump_seals_to_survivors_only_and_signs() {
        let (mut state, a_sk, seed, b_sk, b_vk_hex) = two_device_fixture(1_700_000_000);
        // Third device C, enrolled then revoked — must get NO blob.
        let c_sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let c_vk = c_sk.verifying_key().to_bytes();
        let artifact = RecoveryArtifact::from_seed(seed);
        let enroll = harmony_owner::lifecycle::enroll_via_master(
            &state,
            &artifact,
            &c_sk,
            PubKeyBundle::classical_only(c_vk),
            1_700_000_010,
            trust::DEFAULT_ACTIVE_WINDOW_SECS,
        )
        .expect("enroll c");
        state
            .add_enrollment(
                enroll.enrollment_cert,
                1_700_000_010,
                trust::DEFAULT_ACTIVE_WINDOW_SECS,
            )
            .expect("add c");
        let c_id = PubKeyBundle::classical_only(c_vk).identity_hash();
        let cert = RevocationCert::sign_master(
            &artifact.master_signing_key(),
            artifact.master_pubkey_bundle(),
            c_id,
            1_700_000_020,
            RevocationReason::Lost,
        )
        .expect("revoke c");
        state
            .add_revocation(cert, 1_700_000_020, trust::DEFAULT_ACTIVE_WINDOW_SECS)
            .expect("add revocation");

        let carrier = crate::fleet_key_epoch::FleetKeyEpochDoc::default();
        let now_ms = 1_700_000_100_000u64;
        let (doc, new_kt) =
            plan_fleet_epoch_bump(&state, &carrier, 0, &seed, now_ms).expect("bump");

        assert_eq!(doc.epoch, 1);
        assert_eq!(new_kt.epoch, 1);
        assert_eq!(doc.bump_wall_ms, now_ms);
        // Survivors = A (seed-holder) + B; revoked C absent.
        let a_id_hex = hex::encode(
            PubKeyBundle::classical_only(a_sk.verifying_key().to_bytes()).identity_hash(),
        );
        let b_id_hex = hex::encode(
            PubKeyBundle::classical_only(
                <[u8; 32]>::try_from(hex::decode(&b_vk_hex).unwrap().as_slice()).unwrap(),
            )
            .identity_hash(),
        );
        let c_id_hex = hex::encode(c_id);
        assert!(doc.sealed.contains_key(&a_id_hex), "seed-holder sealed");
        assert!(doc.sealed.contains_key(&b_id_hex), "sibling sealed");
        assert!(!doc.sealed.contains_key(&c_id_hex), "revoked device absent");
        assert_eq!(doc.sealed.len(), 2);

        // The doc verifies against this owner.
        assert!(doc.verify(&state.owner_id));

        // B can open its blob and gets material at the new epoch that
        // reconstructs the same tree the planner derived.
        let opened =
            crate::fleet_key_epoch::unseal_own_material(&doc, &b_id_hex, &b_sk).expect("unseal");
        assert_eq!(opened.epoch, 1);
        let back = crate::owner_state_crypto::KeyTree::from_fleet_material(&opened).unwrap();
        assert_eq!(back.epoch, new_kt.epoch);

        // Chained bump: next epoch is max+1 even if data epoch lags.
        let (doc2, _) = plan_fleet_epoch_bump(&state, &doc, 0, &seed, now_ms + 1).expect("bump2");
        assert_eq!(doc2.epoch, 2);
    }

    #[test]
    fn plan_bump_rejects_foreign_seed() {
        let (state, _a_sk, _seed, _b_sk, _b_vk_hex) = two_device_fixture(1_700_000_000);
        let carrier = crate::fleet_key_epoch::FleetKeyEpochDoc::default();
        // `KeyTree` has no `Debug` (key material), so `expect_err` (which
        // formats the Ok value) won't compile — match on the result.
        match plan_fleet_epoch_bump(&state, &carrier, 0, &[0x77u8; 32], 1_000) {
            Err(err) => assert!(err.starts_with("notMaster:"), "{err}"),
            Ok(_) => panic!("foreign seed must fail"),
        }
    }

    #[test]
    fn fleet_epoch_window_close_decision() {
        let bump = 1_000_000u64;
        let week = crate::fleet_key_epoch::FLEET_EPOCH_WINDOW_MS;
        // All survivors postdate the bump → close.
        assert!(fleet_epoch_window_should_close(
            bump,
            bump + 10,
            &[Some(bump + 5), Some(bump + 7)]
        ));
        // One stale survivor holds the window open.
        assert!(!fleet_epoch_window_should_close(
            bump,
            bump + 10,
            &[Some(bump + 5), Some(bump - 1)]
        ));
        // A missing row holds it open.
        assert!(!fleet_epoch_window_should_close(
            bump,
            bump + 10,
            &[Some(bump + 5), None]
        ));
        // No survivors listed: hold (never close on an empty read).
        assert!(!fleet_epoch_window_should_close(bump, bump + 10, &[]));
        // 7-day timeout closes regardless of stragglers.
        assert!(fleet_epoch_window_should_close(
            bump,
            bump + week,
            &[Some(bump - 1), None]
        ));
    }

    #[test]
    fn plan_master_revoke_of_sibling_produces_master_cert() {
        let (state, a_sk, seed, _b_sk, b_vk_hex) = two_device_fixture(1_700_000_000);
        let now = 1_700_000_100u64;
        let RevocationPlan::Planned(planned) =
            plan_revocation(&state, &a_sk, Some(&seed), &b_vk_hex, "lost", now).expect("plan ok")
        else {
            panic!("expected a planned revocation");
        };
        assert!(!planned.is_self);
        assert!(matches!(
            planned.cert.issuer,
            harmony_owner::certs::RevocationIssuer::Master { .. }
        ));
        assert_eq!(planned.cert.reason, RevocationReason::Lost);
        // The cert must be acceptable to the CRDT.
        let mut s2 = state.clone();
        s2.add_revocation(planned.cert.clone(), now, trust::DEFAULT_ACTIVE_WINDOW_SECS)
            .expect("cert verifies");
        assert!(s2.is_revoked(planned.cert.target));
    }

    #[test]
    fn plan_self_revoke_produces_self_cert_without_seed() {
        let (state, _a_sk, _seed, b_sk, b_vk_hex) = two_device_fixture(1_700_000_000);
        let RevocationPlan::Planned(planned) = plan_revocation(
            &state,
            &b_sk,
            None,
            &b_vk_hex,
            "decommissioned",
            1_700_000_100,
        )
        .expect("plan ok") else {
            panic!("expected a planned revocation");
        };
        assert!(planned.is_self);
        assert!(matches!(
            planned.cert.issuer,
            harmony_owner::certs::RevocationIssuer::SelfDevice
        ));
        let mut s2 = state.clone();
        s2.add_revocation(
            planned.cert,
            1_700_000_100,
            trust::DEFAULT_ACTIVE_WINDOW_SECS,
        )
        .expect("self cert verifies");
    }

    #[test]
    fn plan_sibling_revoke_without_seed_is_not_master() {
        let (state, _a, _seed, b_sk, _bhex) = two_device_fixture(1_700_000_000);
        // Device B (no seed) targets device A: find A's vk from its enrollment.
        let b_id = crate::owner_state::device_id_from_signing_key(&b_sk);
        let a_vk_hex = state
            .enrollments
            .values()
            .find(|c| c.device_id != b_id)
            .map(|c| hex::encode(c.device_pubkeys.classical.ed25519_verify))
            .expect("A enrolled");
        let err =
            plan_revocation(&state, &b_sk, None, &a_vk_hex, "lost", 1_700_000_100).unwrap_err();
        assert!(err.starts_with("notMaster:"), "{err}");
    }

    #[test]
    fn plan_refuses_revoking_last_active_device() {
        let now = 1_700_000_000u64;
        let MintResult {
            state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(now).expect("mint");
        let seed = *recovery_artifact.as_bytes();
        let self_vk_hex = hex::encode(device_signing_key.verifying_key().to_bytes());
        let err = plan_revocation(
            &state,
            &device_signing_key,
            Some(&seed),
            &self_vk_hex,
            "decommissioned",
            now + 10,
        )
        .unwrap_err();
        assert!(err.starts_with("lastDevice:"), "{err}");
    }

    #[test]
    fn plan_unknown_target_and_bad_hex_error() {
        let (state, a_sk, seed, _b, _bhex) = two_device_fixture(1_700_000_000);
        let unknown_vk = hex::encode([9u8; 32]);
        let err = plan_revocation(
            &state,
            &a_sk,
            Some(&seed),
            &unknown_vk,
            "lost",
            1_700_000_100,
        )
        .unwrap_err();
        assert!(err.starts_with("unknownDevice:"), "{err}");
        let err =
            plan_revocation(&state, &a_sk, Some(&seed), "zz", "lost", 1_700_000_100).unwrap_err();
        assert!(err.starts_with("badDeviceVk:"), "{err}");
    }

    /// ZEB-668 S5: a master revoke with a RESIDENT carrier bumps the fleet
    /// epoch — signed doc at epoch 1 whose sealed map excludes the revoked
    /// device, and the shared key set publishes on the new epoch. A SELF
    /// revoke leaves the carrier untouched.
    #[tokio::test]
    async fn revoke_device_inner_master_path_bumps_fleet_epoch() {
        std::env::set_var("HARMONY_PASSPHRASE", "test-passphrase");
        let (state, a_sk, seed, _b, b_vk_hex) = two_device_fixture(now_unix() - 60);
        let dir = tempfile::tempdir().unwrap();
        save_owner_state_atomic(dir.path(), &state, &a_sk, Some(&seed), None)
            .expect("persist identity");

        // Resident carrier: real engine over an in-memory CAS, plus the
        // resident trust doc the hook re-snapshots after the mutation.
        let kt0 =
            std::sync::Arc::new(crate::owner_state_crypto::KeyTree::derive(&seed).expect("kt0"));
        let keys = crate::owner_state_crypto::FleetKeySet::new(std::sync::Arc::clone(&kt0));
        let carrier_doc = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::fleet_key_epoch::FleetKeyEpochDoc::default(),
        ));
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let drain = tokio::spawn(async move { while out_rx.recv().await.is_some() {} });
        let persist_dir = tempfile::tempdir().unwrap();
        let owner_id = state.owner_id;
        // ZEB-790: the carrier and trust engines model ONE node, so they must
        // share ONE adoption floor (the invariant: all HLC-minting/feeding
        // contexts within a node hold the same `HlcAdoptFloor` Arc).
        let adopt_floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
        let carrier_engine = std::sync::Arc::new(crate::fleet_sync::FleetSyncEngine::new(
            crate::fleet_sync::FleetSyncConfig {
                keys: crate::owner_state_crypto::FleetKeySet::new(kt0),
                device_id: "dev-a".to_string(),
                state: std::sync::Arc::clone(&carrier_doc),
                merger: std::sync::Arc::new(
                    move |l: &mut crate::fleet_key_epoch::FleetKeyEpochDoc, r| {
                        crate::fleet_sync::MergeOutcome {
                            changed: crate::fleet_key_epoch::merge_fleet_keys_remote(
                                l, r, &owner_id,
                            ),
                        }
                    },
                ),
                replay_tracker: std::sync::Arc::new(tokio::sync::Mutex::new(
                    harmony_crdt_sync::ReplayTracker::new("dev-a".to_string()),
                )),
                content_store: std::sync::Arc::new(crate::content_store::InMemoryStub::default()),
                publisher_tx: out_tx,
                subscriber_rx: in_rx,
                persist: std::sync::Arc::new(crate::fleet_key_epoch::FleetKeyEpochPersist {
                    doc_path: persist_dir.path().join("fleet_keys.cbor"),
                    replay_path: persist_dir.path().join("fleet_keys_replay.cbor"),
                }),
                lookup_key_tag: crate::fleet_key_epoch::FLEET_KEYS_LOOKUP_TAG,
                debounce_ms: 25,
                publish_seen: false,
                on_applied: None,
                sibling_acks: std::sync::Arc::new(tokio::sync::Mutex::new(Default::default())),
                adopt_floor: adopt_floor.clone(),
            },
        ));
        let trust_doc_arc = std::sync::Arc::new(tokio::sync::Mutex::new(state.clone()));
        // Resident trust engine too — prod invariant: doc + engine are
        // Some/None together, and the mutation path picks Resident only
        // when both exist (FileOnly would leave the resident doc stale and
        // the hook's survivor enumeration wrong).
        let (t_out_tx, mut t_out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let (_t_in_tx, t_in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let t_drain = tokio::spawn(async move { while t_out_rx.recv().await.is_some() {} });
        let trust_engine = std::sync::Arc::new(crate::fleet_sync::FleetSyncEngine::new(
            crate::fleet_sync::FleetSyncConfig {
                keys: keys.clone(),
                device_id: "dev-a".to_string(),
                state: std::sync::Arc::clone(&trust_doc_arc),
                merger: crate::owner_trust_sync::trust_merger(),
                replay_tracker: std::sync::Arc::new(tokio::sync::Mutex::new(
                    harmony_crdt_sync::ReplayTracker::new("dev-a".to_string()),
                )),
                content_store: std::sync::Arc::new(crate::content_store::InMemoryStub::default()),
                publisher_tx: t_out_tx,
                subscriber_rx: t_in_rx,
                persist: std::sync::Arc::new(crate::owner_trust_sync::TrustPersist {
                    identity_dir: dir.path().to_path_buf(),
                    replay_path: persist_dir.path().join("trust_replay.cbor"),
                }),
                lookup_key_tag: crate::owner_trust_sync::OWNER_TRUST_LOOKUP_TAG,
                debounce_ms: 25,
                publish_seen: true,
                on_applied: None,
                sibling_acks: std::sync::Arc::new(tokio::sync::Mutex::new(Default::default())),
                adopt_floor: adopt_floor.clone(),
            },
        ));
        let node = std::sync::Mutex::new(crate::NodeState {
            identity_dir: Some(dir.path().to_path_buf()),
            owner_trust_doc: Some(std::sync::Arc::clone(&trust_doc_arc)),
            owner_trust_sync: Some(std::sync::Arc::clone(&trust_engine)),
            fleet_key_epoch_doc: Some(std::sync::Arc::clone(&carrier_doc)),
            fleet_key_epoch_sync: Some(std::sync::Arc::clone(&carrier_engine)),
            fleet_keys: Some(keys.clone()),
            ..crate::NodeState::default()
        });
        let emit: std::sync::Arc<dyn Fn(&str) + Send + Sync> = std::sync::Arc::new(|_| {});

        revoke_device_inner(&node, || None, emit, b_vk_hex.clone(), "lost".into())
            .await
            .expect("revoke ok");

        let doc = carrier_doc.lock().await.clone();
        assert_eq!(doc.epoch, 1, "master revoke bumps to epoch 1");
        assert!(doc.verify(&owner_id), "bumped doc is master-signed");
        let b_id_hex = {
            let g = trust_doc_arc.lock().await;
            hex::encode(
                g.enrollments
                    .values()
                    .find(|c| hex::encode(c.device_pubkeys.classical.ed25519_verify) == b_vk_hex)
                    .map(|c| c.device_id)
                    .unwrap(),
            )
        };
        assert!(
            !doc.sealed.contains_key(&b_id_hex),
            "revoked device gets no blob"
        );
        assert_eq!(doc.sealed.len(), 1, "only the seed-holder survives");
        assert_eq!(keys.newest().epoch, 1, "key set publishes on the new epoch");

        let _ = carrier_engine.shutdown().await;
        let _ = trust_engine.shutdown().await;
        drain.abort();
        t_drain.abort();
    }

    /// ZEB-668 S5: self-revoke never bumps (no seed reachable on the
    /// cert-only device; spec §6). Carrier stays at the default doc.
    #[tokio::test]
    async fn revoke_device_inner_self_revoke_does_not_bump_epoch() {
        std::env::set_var("HARMONY_PASSPHRASE", "test-passphrase");
        let (state, _a_sk, _seed, b_sk, b_vk_hex) = two_device_fixture(now_unix() - 60);
        let dir = tempfile::tempdir().unwrap();
        // Persist as device B: cert-only (no master seed on disk).
        save_owner_state_atomic(dir.path(), &state, &b_sk, None, None).expect("persist identity");
        let carrier_doc = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::fleet_key_epoch::FleetKeyEpochDoc::default(),
        ));
        let node = std::sync::Mutex::new(crate::NodeState {
            identity_dir: Some(dir.path().to_path_buf()),
            fleet_key_epoch_doc: Some(std::sync::Arc::clone(&carrier_doc)),
            ..crate::NodeState::default()
        });
        let emit: std::sync::Arc<dyn Fn(&str) + Send + Sync> = std::sync::Arc::new(|_| {});
        revoke_device_inner(&node, || None, emit, b_vk_hex, "decommissioned".into())
            .await
            .expect("self revoke ok");
        assert_eq!(
            carrier_doc.lock().await.epoch,
            0,
            "self-revoke must not bump the fleet epoch"
        );
    }

    #[tokio::test]
    async fn revoke_device_inner_master_revokes_sibling_file_only() {
        std::env::set_var("HARMONY_PASSPHRASE", "test-passphrase");
        let (state, a_sk, seed, _b, b_vk_hex) = two_device_fixture(now_unix() - 60);
        let dir = tempfile::tempdir().unwrap();
        save_owner_state_atomic(dir.path(), &state, &a_sk, Some(&seed), None)
            .expect("persist identity");
        let node = std::sync::Mutex::new(crate::NodeState {
            identity_dir: Some(dir.path().to_path_buf()),
            ..crate::NodeState::default()
        });
        let events: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let ev = events.clone();
        let emit: std::sync::Arc<dyn Fn(&str) + Send + Sync> =
            std::sync::Arc::new(move |name: &str| ev.lock().unwrap().push(name.to_string()));

        revoke_device_inner(
            &node,
            || None,
            emit.clone(),
            b_vk_hex.clone(),
            "lost".into(),
        )
        .await
        .expect("revoke ok");

        // Durable: the revocation is on disk.
        let disk = crate::owner_state::load_owner_state_cbor(dir.path()).expect("disk state");
        let b_id = disk
            .enrollments
            .values()
            .find(|c| hex::encode(c.device_pubkeys.classical.ed25519_verify) == b_vk_hex)
            .map(|c| c.device_id)
            .unwrap();
        assert!(disk.is_revoked(b_id));
        assert_eq!(events.lock().unwrap().as_slice(), ["owner-devices-updated"]);
        // Sibling revoke must NOT latch the self-revoked flag.
        assert!(!node
            .lock()
            .unwrap()
            .owner_trust_revoked_self
            .load(std::sync::atomic::Ordering::Acquire));

        // Idempotent second call: no error, no duplicate event.
        revoke_device_inner(&node, || None, emit, b_vk_hex, "lost".into())
            .await
            .expect("noop ok");
        assert_eq!(events.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn revoke_device_inner_self_revoke_latches_and_emits_terminal_event() {
        std::env::set_var("HARMONY_PASSPHRASE", "test-passphrase");
        let (state, _a_sk, _seed, b_sk, _b_vk_hex) = two_device_fixture(now_unix() - 60);
        // Persist device B's identity (no seed) — B removes itself.
        let dir = tempfile::tempdir().unwrap();
        save_owner_state_atomic(dir.path(), &state, &b_sk, None, None).expect("persist identity");
        let self_vk_hex = hex::encode(b_sk.verifying_key().to_bytes());
        let node = std::sync::Mutex::new(crate::NodeState {
            identity_dir: Some(dir.path().to_path_buf()),
            ..crate::NodeState::default()
        });
        let events: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let ev = events.clone();
        let emit: std::sync::Arc<dyn Fn(&str) + Send + Sync> =
            std::sync::Arc::new(move |name: &str| ev.lock().unwrap().push(name.to_string()));

        revoke_device_inner(&node, || None, emit, self_vk_hex, "decommissioned".into())
            .await
            .expect("self-revoke ok");

        let disk = crate::owner_state::load_owner_state_cbor(dir.path()).unwrap();
        assert!(disk.is_revoked(crate::owner_state::device_id_from_signing_key(&b_sk)));
        assert!(node
            .lock()
            .unwrap()
            .owner_trust_revoked_self
            .load(std::sync::atomic::Ordering::Acquire));
        let got = events.lock().unwrap().clone();
        assert_eq!(got, ["owner-devices-updated", "device-revoked-self"]);
    }

    /// CodeRabbit PR #452: a self-revoke that mutated the doc but failed at
    /// flush must CONVERGE on retry — the already-revoked path completes the
    /// pending terminal transition instead of silently succeeding.
    #[tokio::test]
    async fn revoke_device_inner_retry_completes_pending_self_terminal() {
        std::env::set_var("HARMONY_PASSPHRASE", "test-passphrase");
        let (mut state, a_sk, seed, b_sk, _b_vk_hex) = two_device_fixture(now_unix() - 60);
        // Simulate the stranded state: B's self-revocation is already in the
        // persisted doc (as if a prior call failed between mutation-persist
        // and terminal), but the revoked flag was never latched.
        let RevocationPlan::Planned(planned) = plan_revocation(
            &state,
            &b_sk,
            None,
            &hex::encode(b_sk.verifying_key().to_bytes()),
            "decommissioned",
            now_unix() - 30,
        )
        .unwrap() else {
            panic!("expected a planned revocation");
        };
        state
            .add_revocation(planned.cert, now_unix(), trust::DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        save_owner_state_atomic(dir.path(), &state, &b_sk, None, None).expect("persist identity");
        let _ = (a_sk, seed);
        let node = std::sync::Mutex::new(crate::NodeState {
            identity_dir: Some(dir.path().to_path_buf()),
            ..crate::NodeState::default()
        });
        let events: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let ev = events.clone();
        let emit: std::sync::Arc<dyn Fn(&str) + Send + Sync> =
            std::sync::Arc::new(move |name: &str| ev.lock().unwrap().push(name.to_string()));

        // Retry: already revoked on disk, flag unlatched -> terminal completes.
        revoke_device_inner(
            &node,
            || None,
            emit.clone(),
            hex::encode(b_sk.verifying_key().to_bytes()),
            "decommissioned".into(),
        )
        .await
        .expect("retry ok");
        assert!(node
            .lock()
            .unwrap()
            .owner_trust_revoked_self
            .load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(events.lock().unwrap().as_slice(), ["device-revoked-self"]);

        // Second retry after terminal completed: plain idempotent success.
        revoke_device_inner(
            &node,
            || None,
            emit,
            hex::encode(b_sk.verifying_key().to_bytes()),
            "decommissioned".into(),
        )
        .await
        .expect("idempotent ok");
        assert_eq!(events.lock().unwrap().len(), 1, "no duplicate emission");
    }

    /// ZEB-677 S3: three master-enrolled devices with fresh liveness, keyed
    /// for the quorum-view tests. Returns (state, a_sk, b_id, c_id, c_vk_hex).
    fn quorum_view_fixture(
        now: u64,
    ) -> (
        OwnerState,
        ed25519_dalek::SigningKey,
        [u8; 16],
        [u8; 16],
        String,
    ) {
        let MintResult {
            mut state,
            recovery_artifact,
            device_signing_key: a_sk,
        } = mint_owner(now).expect("mint");
        let owner_id = state.owner_id;
        let mut enroll = |t: u64| {
            let sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
            let res = enroll_via_master(
                &state,
                &recovery_artifact,
                &sk,
                PubKeyBundle::classical_only(sk.verifying_key().to_bytes()),
                t,
                trust::DEFAULT_ACTIVE_WINDOW_SECS,
            )
            .expect("enroll");
            let id = res.enrollment_cert.device_id;
            state
                .add_enrollment(res.enrollment_cert, t, trust::DEFAULT_ACTIVE_WINDOW_SECS)
                .expect("add");
            (sk, id)
        };
        let (b_sk, b_id) = enroll(now + 1);
        let (c_sk, c_id) = enroll(now + 2);
        let c_vk_hex = hex::encode(c_sk.verifying_key().to_bytes());
        for sk in [&a_sk, &b_sk, &c_sk] {
            state
                .add_liveness(
                    harmony_owner::certs::LivenessCert::sign(sk, owner_id, now + 3).unwrap(),
                )
                .expect("liveness");
        }
        (state, a_sk, b_id, c_id, c_vk_hex)
    }

    fn masterless_loaded(state: &OwnerState, sk: &ed25519_dalek::SigningKey) -> LoadedOwnerState {
        LoadedOwnerState {
            state: state.clone(),
            device_signing_key: sk.clone(),
            master_seed: None,
            fleet_keytree: None,
        }
    }

    #[test]
    fn view_self_is_master_mirrors_seed_presence() {
        let now = now_unix() - 60;
        let (state, a_sk, _b, _c, _cvk) = quorum_view_fixture(now);
        let with_seed = LoadedOwnerState {
            state: state.clone(),
            device_signing_key: a_sk.clone(),
            master_seed: Some(Zeroizing::new([0x11; 32])),
            fleet_keytree: None,
        };
        let view = build_owner_state_view(
            &with_seed,
            "d".into(),
            FleetJoin::default(),
            QuorumJoin::default(),
        );
        assert!(view.self_is_master);
        let view2 = build_owner_state_view(
            &masterless_loaded(&state, &a_sk),
            "d".into(),
            FleetJoin::default(),
            QuorumJoin::default(),
        );
        assert!(!view2.self_is_master);
    }

    #[test]
    fn view_can_arm_enrollment_matrix() {
        let now = now_unix() - 60;
        let (state, a_sk, b_id, c_id, _cvk) = quorum_view_fixture(now);

        // Master-less A with active master-certed siblings → can arm.
        let view = build_owner_state_view(
            &masterless_loaded(&state, &a_sk),
            "d".into(),
            FleetJoin::default(),
            QuorumJoin::default(),
        );
        assert!(
            view.can_arm_enrollment,
            "master-less device with an active master-certed sibling can arm"
        );

        // Seed present → normal pairing; the arm affordance is off.
        let with_seed = LoadedOwnerState {
            state: state.clone(),
            device_signing_key: a_sk.clone(),
            master_seed: Some(Zeroizing::new([0x11; 32])),
            fleet_keytree: None,
        };
        let view_seed = build_owner_state_view(
            &with_seed,
            "d".into(),
            FleetJoin::default(),
            QuorumJoin::default(),
        );
        assert!(
            !view_seed.can_arm_enrollment,
            "a master holder adds devices through normal pairing"
        );

        // No active sibling: strip B and C liveness so A is the only active
        // master-certed device → no inviter/co-signer → cannot arm.
        let mut lonely = state.clone();
        lonely.liveness.remove(&b_id);
        lonely.liveness.remove(&c_id);
        let view_lonely = build_owner_state_view(
            &masterless_loaded(&lonely, &a_sk),
            "d".into(),
            FleetJoin::default(),
            QuorumJoin::default(),
        );
        assert!(
            !view_lonely.can_arm_enrollment,
            "no active master-certed sibling → cannot arm"
        );
    }

    #[test]
    fn view_quorum_removable_matrix() {
        let now = now_unix() - 60;
        let (state, a_sk, b_id, c_id, _cvk) = quorum_view_fixture(now);
        let a_id = crate::owner_state::device_id_from_signing_key(&a_sk);

        // Master-less A in a 3-device fleet: sibling rows removable via
        // quorum; the self row never is.
        let view = build_owner_state_view(
            &masterless_loaded(&state, &a_sk),
            "d".into(),
            FleetJoin::default(),
            QuorumJoin::default(),
        );
        let row = |id: [u8; 16]| {
            view.devices
                .iter()
                .find(|d| d.device_id == hex::encode(id))
                .expect("row")
                .clone()
        };
        assert!(row(b_id).quorum_removable);
        assert!(row(c_id).quorum_removable);
        assert!(
            !row(a_id).quorum_removable,
            "self row is never quorum-removable"
        );

        // Seed present → the direct Remove path; quorum affordance off.
        let with_seed = LoadedOwnerState {
            state: state.clone(),
            device_signing_key: a_sk.clone(),
            master_seed: Some(Zeroizing::new([0x11; 32])),
            fleet_keytree: None,
        };
        let view_seed = build_owner_state_view(
            &with_seed,
            "d".into(),
            FleetJoin::default(),
            QuorumJoin::default(),
        );
        assert!(view_seed.devices.iter().all(|d| !d.quorum_removable));

        // No liveness for C: B's row loses its second co-signer candidate
        // (only C could co-sign a removal of B — it is inactive), while
        // C's own row keeps B as the candidate.
        let mut stale_c = state.clone();
        stale_c.liveness.remove(&c_id);
        let view_stale = build_owner_state_view(
            &masterless_loaded(&stale_c, &a_sk),
            "d".into(),
            FleetJoin::default(),
            QuorumJoin::default(),
        );
        let row_s = |id: [u8; 16]| {
            view_stale
                .devices
                .iter()
                .find(|d| d.device_id == hex::encode(id))
                .expect("row")
                .clone()
        };
        assert!(
            !row_s(b_id).quorum_removable,
            "no active co-signer besides the target"
        );
        assert!(row_s(c_id).quorum_removable, "B can co-sign C's removal");
    }

    #[test]
    fn view_quorum_requests_flags_and_arm_window() {
        let now = now_unix() - 60;
        let now_ms = now * 1000;
        let (state, a_sk, b_id, c_id, _cvk) = quorum_view_fixture(now);
        let a_id = crate::owner_state::device_id_from_signing_key(&a_sk);
        let a_hex = hex::encode(a_id);

        let mk_request = |initiator: [u8; 16], addressed_to: &[[u8; 16]], expires_at_ms: u64| {
            crate::owner_quorum_sync::QuorumRequest {
                created_at: crate::owner_state_types::Hlc {
                    wall_ms: now_ms,
                    logical: 0,
                    device_id: hex::encode(initiator),
                },
                declined_by: Default::default(),
                initiator_hex: hex::encode(initiator),
                kind: crate::owner_quorum_sync::QuorumRequestKind::Revocation {
                    reason: "lost".into(),
                    target_hex: hex::encode(c_id),
                    epoch_doc_cbor_hex: None,
                    epoch_doc_initiator_sig_hex: None,
                },
                initiator_sigs: addressed_to
                    .iter()
                    .map(|id| (hex::encode(id), "00".repeat(64)))
                    .collect(),
                signatures: Default::default(),
                issued_at: now,
                expires_at_ms,
            }
        };

        let mut quorum = QuorumJoin {
            doc: Default::default(),
            now_ms,
        };
        // B asks A to co-sign removing C → canCosign.
        quorum
            .doc
            .requests
            .insert("01".repeat(16), mk_request(b_id, &[a_id], now_ms + 1000));
        // A's own request → initiatedByMe, never canCosign.
        quorum
            .doc
            .requests
            .insert("02".repeat(16), mk_request(a_id, &[b_id], now_ms + 1000));
        // Declined request → dead for co-sign. A (this device) was asked to
        // co-sign and vetoed instead; the decline is a VERIFIED signature (an
        // unverifiable entry would not surface as declined).
        let mut declined = mk_request(b_id, &[a_id], now_ms + 1000);
        let decline_payload =
            crate::owner_quorum_sync::decline_signing_payload(state.owner_id, &"03".repeat(16));
        let decline_sig = harmony_owner::signing::sign_with_tag(
            &a_sk,
            harmony_owner::signing::tags::REVOCATION,
            &decline_payload,
        );
        declined
            .declined_by
            .insert(a_hex.clone(), hex::encode(decline_sig));
        quorum.doc.requests.insert("03".repeat(16), declined);
        // Already signed by A → cosignerSigned, not canCosign.
        let mut signed = mk_request(b_id, &[a_id], now_ms + 1000);
        signed.signatures.insert(
            a_hex.clone(),
            crate::owner_quorum_sync::QuorumRequestSigs {
                epoch_doc_sig_hex: None,
                primary_sig_hex: "00".repeat(64),
            },
        );
        quorum.doc.requests.insert("04".repeat(16), signed);
        // Expired → filtered out entirely.
        quorum
            .doc
            .requests
            .insert("05".repeat(16), mk_request(b_id, &[a_id], now_ms - 1));
        // Not addressed to A (no co-sign slot) → not canCosign.
        quorum
            .doc
            .requests
            .insert("06".repeat(16), mk_request(b_id, &[], now_ms + 1000));
        // Own arm cell, unexpired → surfaces.
        quorum.doc.enroll_arms.insert(
            a_hex.clone(),
            crate::owner_quorum_sync::EnrollArm {
                set_at: crate::owner_state_types::Hlc {
                    wall_ms: now_ms,
                    logical: 0,
                    device_id: a_hex.clone(),
                },
                armed_until_ms: now_ms + 900_000,
            },
        );

        let view = build_owner_state_view(
            &masterless_loaded(&state, &a_sk),
            "d".into(),
            FleetJoin::default(),
            quorum,
        );
        assert_eq!(view.quorum_requests.len(), 5, "expired request filtered");
        let by_id = |id: &str| {
            view.quorum_requests
                .iter()
                .find(|r| r.request_id == id.repeat(16))
                .expect("request view")
        };
        let r1 = by_id("01");
        assert!(r1.can_cosign && !r1.initiated_by_me && !r1.signed_by_me && !r1.declined);
        assert_eq!(r1.kind, "revocation");
        assert_eq!(r1.reason, "lost");
        assert_eq!(r1.target_device_id, hex::encode(c_id));
        assert_eq!(r1.initiator_device_id, hex::encode(b_id));
        let r2 = by_id("02");
        assert!(r2.initiated_by_me && !r2.can_cosign);
        let r3 = by_id("03");
        assert!(r3.declined && !r3.can_cosign);
        let r4 = by_id("04");
        assert!(r4.signed_by_me && r4.cosigner_signed && !r4.can_cosign);
        let r6 = by_id("06");
        assert!(!r6.can_cosign, "no co-sign slot for this device");
        assert_eq!(view.quorum_armed_until_ms, Some(now_ms + 900_000));

        // Expired arm cell → None.
        let mut expired_arm = QuorumJoin {
            doc: Default::default(),
            now_ms,
        };
        expired_arm.doc.enroll_arms.insert(
            a_hex.clone(),
            crate::owner_quorum_sync::EnrollArm {
                set_at: crate::owner_state_types::Hlc {
                    wall_ms: now_ms,
                    logical: 0,
                    device_id: a_hex,
                },
                armed_until_ms: now_ms - 1,
            },
        );
        let view2 = build_owner_state_view(
            &masterless_loaded(&state, &a_sk),
            "d".into(),
            FleetJoin::default(),
            expired_arm,
        );
        assert_eq!(view2.quorum_armed_until_ms, None);
    }

    /// ZEB-721: `build_owner_state_view` surfaces a regressed host clock (this
    /// device's own liveness cert stamped in the future) via
    /// `self_clock_regressed_skew_secs`, and reports `None` when healthy.
    #[test]
    fn view_surfaces_self_clock_regressed_skew() {
        let (mut state, a_sk, ..) = quorum_view_fixture(now_unix());
        let a_id = crate::owner_state::device_id_from_signing_key(&a_sk);
        let owner_id = state.owner_id;
        // Healthy: force A's own cert to a clearly-past timestamp. Direct insert
        // bypasses the add_liveness LWW (which would keep the fixture's newer cert).
        let past = harmony_owner::certs::LivenessCert::sign(&a_sk, owner_id, now_unix() - 100_000)
            .unwrap();
        state.liveness.insert(a_id, past);
        let healthy = build_owner_state_view(
            &masterless_loaded(&state, &a_sk),
            "d".into(),
            FleetJoin::default(),
            QuorumJoin::default(),
        );
        assert_eq!(
            healthy.self_clock_regressed_skew_secs, None,
            "a past-stamped self-cert must surface no skew"
        );
        // Regress: stamp A's own liveness cert an hour into the future (wins the
        // LWW merge), simulating a host clock that moved backwards after signing.
        let future = now_unix() + 3600;
        let cert = harmony_owner::certs::LivenessCert::sign(&a_sk, owner_id, future).unwrap();
        state.add_liveness(cert).unwrap();
        let regressed = build_owner_state_view(
            &masterless_loaded(&state, &a_sk),
            "d".into(),
            FleetJoin::default(),
            QuorumJoin::default(),
        );
        let skew = regressed
            .self_clock_regressed_skew_secs
            .expect("a future-stamped self-cert must surface a skew");
        assert!(
            skew > 3000 && skew <= 3600,
            "skew ≈ future − now (~1h), got {skew}"
        );
    }

    /// ZEB-668 S5: `fleetEpochStale` = any revocation newer than the last
    /// bump. Cert `issued_at` is seconds; the bump stamp is milliseconds.
    #[test]
    fn view_fleet_epoch_staleness_tracks_revocations_vs_bump() {
        let (mut state, a_sk, seed, _b, b_vk_hex) = two_device_fixture(1_700_000_000);

        // No revocations → never stale, even pre-bump.
        let loaded = LoadedOwnerState {
            state: state.clone(),
            device_signing_key: a_sk.clone(),
            master_seed: Some(Zeroizing::new(seed)),
            fleet_keytree: None,
        };
        let view = build_owner_state_view(
            &loaded,
            "d".into(),
            FleetJoin::default(),
            QuorumJoin::default(),
        );
        assert!(!view.fleet_epoch_stale);
        assert_eq!(view.fleet_epoch, 0);

        // Revoke B at t=1_700_000_100 (seconds).
        let now = 1_700_000_100u64;
        let RevocationPlan::Planned(planned) =
            plan_revocation(&state, &a_sk, Some(&seed), &b_vk_hex, "lost", now).unwrap()
        else {
            panic!("expected planned");
        };
        state
            .add_revocation(planned.cert, now, trust::DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        let loaded = LoadedOwnerState {
            state,
            device_signing_key: a_sk,
            master_seed: Some(Zeroizing::new(seed)),
            fleet_keytree: None,
        };

        // Pre-S5 carrier (0/0): the revocation makes the fleet honestly stale.
        let view = build_owner_state_view(
            &loaded,
            "d".into(),
            FleetJoin::default(),
            QuorumJoin::default(),
        );
        assert!(view.fleet_epoch_stale, "pre-bump fleet with a revocation");

        // Bump BEFORE the revocation (ms): still stale.
        let stale_join = FleetJoin {
            carrier_epoch: 1,
            carrier_bump_wall_ms: (now - 10) * 1000,
            ..Default::default()
        };
        let view = build_owner_state_view(&loaded, "d".into(), stale_join, QuorumJoin::default());
        assert!(view.fleet_epoch_stale, "revocation postdates the bump");
        assert_eq!(view.fleet_epoch, 1);

        // Bump AFTER the revocation: fresh.
        let fresh_join = FleetJoin {
            carrier_epoch: 2,
            carrier_bump_wall_ms: (now + 10) * 1000,
            ..Default::default()
        };
        let view = build_owner_state_view(&loaded, "d".into(), fresh_join, QuorumJoin::default());
        assert!(!view.fleet_epoch_stale, "bump postdates every revocation");
        assert_eq!(view.fleet_epoch, 2);
    }

    #[test]
    fn view_marks_revoked_device_with_reason_and_date() {
        let (mut state, a_sk, seed, _b, b_vk_hex) = two_device_fixture(1_700_000_000);
        let now = 1_700_000_100u64;
        let RevocationPlan::Planned(planned) =
            plan_revocation(&state, &a_sk, Some(&seed), &b_vk_hex, "lost", now).unwrap()
        else {
            panic!("expected a planned revocation");
        };
        let target = planned.cert.target;
        state
            .add_revocation(planned.cert, now, trust::DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        let loaded = LoadedOwnerState {
            state,
            device_signing_key: a_sk,
            master_seed: Some(Zeroizing::new(seed)),
            fleet_keytree: None,
        };
        let view = build_owner_state_view(
            &loaded,
            "Test Device".to_string(),
            FleetJoin::default(),
            QuorumJoin::default(),
        );
        let revoked_row = view
            .devices
            .iter()
            .find(|d| d.device_id == hex::encode(target))
            .expect("revoked device still in view");
        assert!(revoked_row.revoked);
        assert_eq!(revoked_row.revoked_at, Some(now));
        assert_eq!(revoked_row.revoked_reason.as_deref(), Some("lost"));
        let self_row = view.devices.iter().find(|d| d.is_this_device).unwrap();
        assert!(!self_row.revoked);
        assert_eq!(self_row.revoked_at, None);
        // camelCase pin for the three new fields.
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"revoked\""));
        assert!(json.contains("\"revokedAt\""));
        assert!(json.contains("\"revokedReason\""));
    }

    #[test]
    fn plan_already_revoked_target_is_noop() {
        let (mut state, a_sk, seed, _b, b_vk_hex) = two_device_fixture(1_700_000_000);
        let now = 1_700_000_100u64;
        let RevocationPlan::Planned(planned) =
            plan_revocation(&state, &a_sk, Some(&seed), &b_vk_hex, "lost", now).unwrap()
        else {
            panic!("expected a planned revocation");
        };
        state
            .add_revocation(planned.cert, now, trust::DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        let second =
            plan_revocation(&state, &a_sk, Some(&seed), &b_vk_hex, "lost", now + 1).unwrap();
        assert!(
            matches!(
                second,
                RevocationPlan::AlreadyRevoked { is_self: false, .. }
            ),
            "idempotent no-op: {second:?}"
        );
    }

    // ── ZEB-691 (Task B7): send-side butler deposit of the friend RevocationPush ──

    /// A mock `ButlerDepositClient` that records every `ButlerDepositRequest`
    /// handed to it (mirrors `dm_outbox::tests::MockDepositClient`). Always
    /// reports `Acked`; the fan-out ignores the outcome (best-effort).
    struct RecordingButler {
        calls: std::sync::Mutex<Vec<crate::butler_deposit::ButlerDepositRequest>>,
    }

    #[async_trait::async_trait]
    impl crate::butler_deposit::ButlerDepositClient for RecordingButler {
        async fn deposit(
            &self,
            req: &crate::butler_deposit::ButlerDepositRequest,
        ) -> crate::butler_deposit::DepositRungOutcome {
            self.calls.lock().unwrap().push(req.clone());
            crate::butler_deposit::DepositRungOutcome::Acked
        }
    }

    /// Build an `Arc<TunnelManager>` over a real hermetic loopback iroh endpoint
    /// (cheap; this test never completes a dial and the friend targets carry no
    /// tunnel contacts, so no `send_dm` ever runs). Mirrors
    /// `iroh_tunnel_dm_transport::tests::test_manager`.
    async fn test_tunnel_manager() -> std::sync::Arc<crate::tunnel_manager::TunnelManager> {
        let endpoint = {
            let sk = iroh::SecretKey::generate();
            crate::iroh_endpoint::new_with_secret_and_relays_hermetic_dns(sk, None)
                .await
                .expect("bind loopback iroh endpoint")
        };
        let local_pq = std::sync::Arc::new(harmony_identity::PqPrivateIdentity::generate(
            &mut rand::rngs::OsRng,
        ));
        let (ingest_tx, _ingest_rx) = tokio::sync::mpsc::channel(16);
        std::sync::Arc::new(crate::tunnel_manager::TunnelManager::new(
            std::sync::Arc::new(endpoint),
            local_pq,
            ingest_tx,
            std::sync::Arc::new(crate::protocol_versioning::ProtocolCompatRegistry::default())
                as std::sync::Arc<dyn crate::tunnel_manager::CompatSink>,
        ))
    }

    /// Insert a friend of `status` into `crdt`, keyed by the addr derived from a
    /// seeded master key (so `apply_friend_update`'s key↔master invariant holds).
    /// Returns the friend's `OwnerAddr`.
    fn add_friend(
        crdt: &mut crate::owner_state_crdt::OwnerState,
        seed: u8,
        status: crate::friend_graph::FriendStatus,
    ) -> crate::owner_state_types::OwnerAddr {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        let master_ed25519 = sk.verifying_key().to_bytes();
        let addr = crate::friend_graph::owner_id_from_master_ed25519(&master_ed25519);
        crdt.apply_friend_update(
            addr,
            crate::friend_graph::FriendEntry {
                master_ed25519,
                display: None,
                status,
                established_via: crate::friend_graph::FriendOrigin::Token,
                referrable: false,
                learned_at: crate::owner_state_types::Hlc {
                    wall_ms: 10,
                    logical: 0,
                    device_id: "d".into(),
                },
                sealed_secret: None,
            },
        );
        addr
    }

    /// ZEB-691 (B7): the fan-out deposits the SAME `RevocationPush` wire to each
    /// ACTIVE friend's butler set — exactly one deposit per Active friend, none
    /// for a non-Active (Pending) friend — with only the `revocation_push` half
    /// set (no message / invite / CID).
    #[tokio::test]
    async fn push_revocation_to_friends_deposits_to_each_active_friend_butler() {
        let now = 1_700_000_000;
        // Real trust state: device B is enrolled (its EnrollmentCert is on
        // record for the RevocationPush pairing) and master-revoked.
        let (state, _a_sk, seed, b_sk, _b_vk_hex) = two_device_fixture(now);
        let b_target = crate::owner_state::device_id_from_signing_key(&b_sk);
        let artifact = RecoveryArtifact::from_seed(seed);
        let revocation = RevocationCert::sign_master(
            &artifact.master_signing_key(),
            artifact.master_pubkey_bundle(),
            b_target,
            now,
            RevocationReason::Lost,
        )
        .unwrap();

        // The wire the send side builds + deposits: the RevocationPush the
        // fan-out encodes from B's (pairing) enrollment.
        let enrollment = state
            .enrollments
            .get(&b_target)
            .cloned()
            .expect("B enrollment on record");
        let expected_wire = crate::dm_envelope::encode_packet(
            &crate::dm_envelope::build_revocation_push_packet(revocation.clone(), enrollment),
        )
        .expect("encode expected wire");

        // CRDT: TWO Active friends + one Pending (non-Active) → only the two
        // Active friends are deposit targets.
        let mut crdt = crate::owner_state_crdt::OwnerState::default();
        let active1 = add_friend(&mut crdt, 0x31, crate::friend_graph::FriendStatus::Active);
        let active2 = add_friend(&mut crdt, 0x32, crate::friend_graph::FriendStatus::Active);
        let _pending = add_friend(&mut crdt, 0x33, crate::friend_graph::FriendStatus::Pending);
        let crdt_state = std::sync::Arc::new(tokio::sync::Mutex::new(crdt));

        let mgr = test_tunnel_manager().await;
        let rec = std::sync::Arc::new(RecordingButler {
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let butler: std::sync::Arc<dyn crate::butler_deposit::ButlerDepositClient> = rec.clone();

        push_revocation_to_friends(&crdt_state, &mgr, Some(&butler), &state, &revocation).await;

        let calls = rec.calls.lock().unwrap().clone();
        assert_eq!(
            calls.len(),
            2,
            "exactly one deposit per ACTIVE friend (Pending excluded)"
        );
        let owners: std::collections::HashSet<_> =
            calls.iter().map(|r| r.recipient_owner).collect();
        assert_eq!(owners.len(), 2, "two distinct recipients");
        assert!(owners.contains(&active1), "active friend 1 got a deposit");
        assert!(owners.contains(&active2), "active friend 2 got a deposit");
        for req in &calls {
            assert_eq!(
                req.revocation_push.as_deref(),
                Some(expected_wire.as_slice()),
                "each deposit carries the RevocationPush wire"
            );
            assert_eq!(req.cidnotify_packet, None, "no message half");
            assert_eq!(req.invite_packet, None, "no invite half");
            assert_eq!(req.message_cid, None, "no message CID");
            // Inert-for-revocation zero keys (butler keys by inner content via
            // revoke_key; this direct deposit does not ride the outbox loop).
            assert_eq!(req.entry_id.0, [0u8; 16], "inert entry_id");
            assert_eq!(req.space_id.0, [0u8; 16], "inert space_id");
        }
    }
}
