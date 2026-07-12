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
/// Note: this lock does NOT cover `rotate_passphrase` /
/// `restore_recovery_from_preview_token`, which write the encrypted-file
/// fallback (`identity.key.enc`) but not `owner_state.cbor`. See ZEB-201
/// for the parallel race on those paths.
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
    AlreadyRevoked {
        is_self: bool,
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
        return Ok(RevocationPlan::AlreadyRevoked { is_self });
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

    // Survivors = enrolled minus revoked. Deliberately NOT `active_devices`
    // (liveness-windowed): a temporarily-offline, non-revoked device must
    // still get a blob or it is orphaned at window close.
    let mut sealed = std::collections::BTreeMap::new();
    for (device_id, cert) in trust.enrollments.iter() {
        if trust.is_revoked(*device_id) {
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
            &material_cbor,
            crate::fleet_key_epoch::FLEET_EPOCH_SEAL_INFO,
        )
        .map_err(|e| format!("sealFailed:{id_hex}: {e}"))?;
        sealed.insert(id_hex, blob);
    }

    let mut doc = crate::fleet_key_epoch::FleetKeyEpochDoc {
        epoch: new_epoch,
        bump_wall_ms: now_ms,
        sealed,
        master_pubkey: None,
        master_sig: Vec::new(),
    };
    doc.sign(&artifact.master_signing_key(), master_pubkey)?;
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

/// Build an `OwnerStateView` from a loaded state.
///
/// `fleet`: the fleet-net + liveness join snapshot (see [`FleetJoin`]). The
/// matching device row receives `butler_pinned` / `pet_name` /
/// `last_seen_ms` / `connected_now`; everything defaults to absent when the
/// fleet-net doc is cold or the node is not running.
fn build_owner_state_view(
    loaded: &LoadedOwnerState,
    this_device_name: String,
    fleet: FleetJoin,
) -> OwnerStateView {
    let now = now_unix();
    let active_window = trust::DEFAULT_ACTIVE_WINDOW_SECS;
    let freshness = trust::DEFAULT_FRESHNESS_WINDOW_SECS;
    let this_device_id = derive_this_device_id(&loaded.device_signing_key);

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

    OwnerStateView {
        owner_id: hex::encode(loaded.state.owner_id),
        owner_display_name: this_device_name,
        devices,
        can_back_up: loaded.master_seed.is_some(),
        fleet_epoch: fleet.carrier_epoch,
        fleet_epoch_stale,
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
    let trust_resident = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        match (g.owner_trust_doc.clone(), g.owner_trust_sync.clone()) {
            (Some(doc), Some(engine)) => Some((doc, engine)),
            _ => None,
        }
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
            let refreshed = refresh_self_liveness(&mut g, &loaded.device_signing_key, now_unix());
            (g.clone(), refreshed)
        };
        // Only nudge the engine when the refresh actually wrote — a panel
        // open must not cause a pointless publish round.
        if refreshed {
            engine.notify_dirty();
        }
        loaded.state = snapshot;
        return Ok(Some(build_owner_state_view(&loaded, display_name, fleet)));
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
            if refresh_self_liveness(&mut loaded.state, &loaded.device_signing_key, now_unix()) {
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
        Ok(Some(build_owner_state_view(&loaded, display_name, fleet)))
    })
    .await
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
    let (trust_doc, trust_engine, identity_dir, revoked_flag, owner_sync, fleet_net, retire_nudge) = {
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
    let planned = match plan_revocation(
        &trust_snapshot,
        &loaded.device_signing_key,
        loaded.master_seed.as_deref(),
        &device_vk_hex,
        &reason,
        now_unix(),
    )? {
        RevocationPlan::Planned(p) => p,
        RevocationPlan::AlreadyRevoked { is_self: false } => return Ok(()), // idempotent
        RevocationPlan::AlreadyRevoked { is_self: true } => {
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
    mint_owner_identity_impl(state.inner(), sink, Some(app)).await
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
) -> Result<MintIpcResult, String> {
    mint_owner_identity_inner(state, KeychainStore::new().ok(), || async {
        crate::start_node_inner(None, sink.clone(), wry_handle.clone(), state)
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
            state: build_owner_state_view(&loaded, display_name, FleetJoin::default()),
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
            mint_at: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state::{
        clear_path_token_cache, clear_token_cache, insert_path_token, take_path_token,
    };
    use harmony_owner::state::OwnerState;
    use serial_test::serial;

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
        let view = build_owner_state_view(&loaded, "this device".into(), FleetJoin::default());
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

        let view = build_owner_state_view(&loaded, "this device".into(), fleet);
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
        let view = build_owner_state_view(&loaded, "this device".into(), FleetJoin::default());
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
        let view = build_owner_state_view(&loaded, "this device".into(), fleet);
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
        let view = build_owner_state_view(&loaded, "this device".into(), fleet);
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
        let view = build_owner_state_view(&loaded, "this device".into(), fleet);
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
        s2.add_revocation(planned.cert, 1_700_000_100, trust::DEFAULT_ACTIVE_WINDOW_SECS)
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
                replay_tracker: std::sync::Arc::new(tokio::sync::Mutex::new(Default::default())),
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
                replay_tracker: std::sync::Arc::new(tokio::sync::Mutex::new(Default::default())),
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
        let view = build_owner_state_view(&loaded, "d".into(), FleetJoin::default());
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
        let view = build_owner_state_view(&loaded, "d".into(), FleetJoin::default());
        assert!(view.fleet_epoch_stale, "pre-bump fleet with a revocation");

        // Bump BEFORE the revocation (ms): still stale.
        let stale_join = FleetJoin {
            carrier_epoch: 1,
            carrier_bump_wall_ms: (now - 10) * 1000,
            ..Default::default()
        };
        let view = build_owner_state_view(&loaded, "d".into(), stale_join);
        assert!(view.fleet_epoch_stale, "revocation postdates the bump");
        assert_eq!(view.fleet_epoch, 1);

        // Bump AFTER the revocation: fresh.
        let fresh_join = FleetJoin {
            carrier_epoch: 2,
            carrier_bump_wall_ms: (now + 10) * 1000,
            ..Default::default()
        };
        let view = build_owner_state_view(&loaded, "d".into(), fresh_join);
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
        let view = build_owner_state_view(&loaded, "Test Device".to_string(), FleetJoin::default());
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
            matches!(second, RevocationPlan::AlreadyRevoked { is_self: false }),
            "idempotent no-op: {second:?}"
        );
    }
}
