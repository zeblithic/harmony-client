//! ZEB-668 S5 — the `fleet-keys-v1` carrier dataset.
//!
//! On a master-issued device revoke the fleet rotates its KeyTree to epoch
//! N+1 so the revoked device (which retains sealed epoch-0
//! `FleetKeyMaterial`) stops decrypting fleet publishes. The new material
//! reaches the surviving devices through THIS dataset: one
//! [`FleetKeyEpochDoc`] carrying the epoch counter, the bump wall-clock, and
//! a per-surviving-device map of sealed material blobs.
//!
//! Two properties make the carrier trustworthy despite being readable by the
//! device it excludes (spec §6.1 amendments 1–3):
//!
//! * **Permanently keyed by the epoch-0 KeyTree.** Every enrolled device
//!   received epoch-0 at pairing and never prunes it, so any device — however
//!   far behind — can always read the carrier to LEARN of newer epochs.
//!   Publishing the blobs under the new epoch instead would deadlock
//!   bootstrap: survivors could never decrypt the very publish that carries
//!   their key material. The price is a bounded metadata leak to revoked
//!   devices (epoch counter, bump time, device-id list, unopenable blobs) —
//!   recorded in the spec §8 honesty ledger.
//! * **Master-signed and monotonic.** A revoked device still holds epoch-0
//!   and could otherwise forge carrier publishes. Receivers adopt a remote
//!   doc only when its epoch is STRICTLY higher than the local one AND its
//!   signature verifies against the owner master key (self-certifying
//!   embedded [`PubKeyBundle`], identity-hash-bound to `owner_id` — the same
//!   pattern as the crate's Master-issued certs). Rollback and forgery are
//!   both dead ends.
//!
//! The individual blobs are sealed to each device's enrollment x25519 via
//! [`crate::dm_signing::seal_to_owner_with_info`]; the carrier's encryption
//! only bounds WHO sees the metadata, never protects key material.

use std::collections::BTreeMap;

use ed25519_dalek::Signer;
use harmony_owner::certs::{EnrollmentCert, EnrollmentIssuer};
use harmony_owner::pubkey_bundle::PubKeyBundle;
use zeroize::Zeroizing;

use crate::owner_state_crypto::{canonical_cbor_encode, sealed::CanonicalPayloadSealed};

/// Dataset id on the fleet topic tree (`…/ds/fleet-keys-v1`).
pub const FLEET_KEYS_DATASET: &str = "fleet-keys-v1";
/// Lookup tag for the carrier's single root blob in CAS.
pub const FLEET_KEYS_LOOKUP_TAG: &[u8] = b"fleet-keys-v1";
/// Dual-epoch read window: the previous epoch stays in the data engines'
/// accept set until every active device's fleet-net `seen_at` postdates the
/// bump or this window elapses, whichever comes first (spec §6).
pub const FLEET_EPOCH_WINDOW_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// HKDF domain separator for sealing `FleetKeyMaterial` to a device x25519.
pub const FLEET_EPOCH_SEAL_INFO: &[u8] = b"harmony-fleet-epoch-key-seal-v1";
/// Domain prefix for the carrier doc's master signature.
const FLEET_KEYS_SIG_DOMAIN: &[u8] = b"harmony-fleet-keys-v1-sig";

/// The replicated carrier doc. Wholesale-replaced on every bump (no
/// per-entry merge): the sealed map is only meaningful as the atomic output
/// of one bump, signed as a unit.
#[derive(Clone, Debug, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct FleetKeyEpochDoc {
    /// Current fleet KeyTree epoch. 0 = never bumped (the default doc).
    #[serde(rename = "e", default)]
    pub epoch: u32,
    /// Wall-clock ms of the bump that produced this doc. The transition
    /// window measures from here.
    #[serde(rename = "b", default)]
    pub bump_wall_ms: u64,
    /// device_id hex → sealed CBOR(`FleetKeyMaterial`), sealed to that
    /// device's enrollment x25519. Revoked devices are absent by
    /// construction.
    #[serde(rename = "s", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sealed: BTreeMap<String, Vec<u8>>,
    /// The owner master key bundle that signed this doc. Self-certifying:
    /// receivers check `identity_hash() == owner_id` before trusting it
    /// (mirrors `EnrollmentIssuer::Master` / `RevocationIssuer::Master`).
    #[serde(rename = "k", default, skip_serializing_if = "Option::is_none")]
    pub master_pubkey: Option<PubKeyBundle>,
    /// ed25519 master signature over [`Self::signing_bytes`].
    #[serde(rename = "g", default, skip_serializing_if = "Vec::is_empty")]
    pub master_sig: Vec<u8>,
    /// ZEB-677 S5 — K=2 quorum co-signature over [`Self::signing_bytes`], for a
    /// master-less (lost-master) fleet epoch bump. Mutually exclusive with
    /// `master_sig`; [`Self::verify`] dispatches on presence. Absent (omitted)
    /// for master-signed docs, so a master doc's wire encoding is unchanged.
    #[serde(rename = "q", default, skip_serializing_if = "Option::is_none")]
    pub quorum_sig: Option<QuorumDocSig>,
    /// ZEB-677 S5 — depth-1 signer bundle: the Master-issued `EnrollmentCert`
    /// of each quorum signer, EMBEDDED so the SYNCHRONOUS carrier merger can
    /// verify a quorum doc self-containedly (it cannot lock the async trust
    /// doc). Mirrors ZEB-677 §2 chain carriage. Empty for master-signed docs.
    #[serde(rename = "c", default, skip_serializing_if = "Vec::is_empty")]
    pub signer_certs: Vec<EnrollmentCert>,
}

/// A K=2 quorum co-signature envelope over [`FleetKeyEpochDoc::signing_bytes`],
/// mirroring `EnrollmentIssuer::Quorum`. `signatures[i]` is by `signers[i]`'s
/// enrolled ed25519 key; `signers` are hex device-ids (the request-doc idiom).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QuorumDocSig {
    #[serde(rename = "n")]
    pub signers: Vec<String>,
    #[serde(rename = "g")]
    pub signatures: Vec<Vec<u8>>,
}

impl CanonicalPayloadSealed for FleetKeyEpochDoc {}
impl crate::owner_state_crypto::CanonicalPayload for FleetKeyEpochDoc {}

impl FleetKeyEpochDoc {
    /// Domain-separated canonical bytes the master signature covers: every
    /// field except the signature itself.
    fn signing_bytes(&self) -> Result<Vec<u8>, String> {
        #[derive(serde::Serialize)]
        struct SigView<'a> {
            epoch: u32,
            bump_wall_ms: u64,
            sealed: &'a BTreeMap<String, Vec<u8>>,
            master_pubkey: &'a Option<PubKeyBundle>,
        }
        let body = canonical_cbor_encode(&CanonicalTuple(SigView {
            epoch: self.epoch,
            bump_wall_ms: self.bump_wall_ms,
            sealed: &self.sealed,
            master_pubkey: &self.master_pubkey,
        }))
        .map_err(|e| format!("carrier signing bytes: {e}"))?;
        let mut out = Vec::with_capacity(FLEET_KEYS_SIG_DOMAIN.len() + body.len());
        out.extend_from_slice(FLEET_KEYS_SIG_DOMAIN);
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Sign with the transiently-reconstructed owner master key. Sets both
    /// `master_pubkey` and `master_sig`.
    pub fn sign(
        &mut self,
        master_sk: &ed25519_dalek::SigningKey,
        master_pubkey: PubKeyBundle,
    ) -> Result<(), String> {
        self.master_pubkey = Some(master_pubkey);
        self.master_sig = Vec::new();
        let bytes = self.signing_bytes()?;
        self.master_sig = master_sk.sign(&bytes).to_bytes().to_vec();
        Ok(())
    }

    /// Verify the master signature and its identity binding to `owner_id`.
    /// `false` for the unsigned default doc, a wrong owner, a tampered
    /// field, or a signature by any key other than the owner master.
    pub fn verify(&self, owner_id: &[u8; 16]) -> bool {
        let Some(bundle) = self.master_pubkey.as_ref() else {
            return false;
        };
        if bundle.identity_hash() != *owner_id {
            return false;
        }
        let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&bundle.classical.ed25519_verify)
        else {
            return false;
        };
        let Ok(sig_bytes) = <[u8; 64]>::try_from(self.master_sig.as_slice()) else {
            return false;
        };
        let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
        let Ok(bytes) = self.signing_bytes() else {
            return false;
        };
        vk.verify_strict(&bytes, &sig).is_ok()
    }

    /// ZEB-677 S5 — verify a carrier doc by whichever issuer signed it: a
    /// quorum doc (`quorum_sig` present) via [`Self::verify_quorum`], else the
    /// master path via [`Self::verify`]. The single reader entry point.
    /// Fully self-contained (no external clock — quorum signer certs verify at
    /// the doc's own signed `bump_wall_ms`).
    pub fn verify_any(&self, owner_id: &[u8; 16]) -> bool {
        if self.quorum_sig.is_some() {
            self.verify_quorum(owner_id)
        } else {
            self.verify(owner_id)
        }
    }

    /// ZEB-677 S5 — one quorum signer's detached ed25519 signature over the
    /// doc's [`Self::signing_bytes`]. The initiator and each co-signer produce
    /// one of these; [`Self::assemble_quorum`] collects them.
    pub fn sign_quorum_part(sk: &ed25519_dalek::SigningKey, signing_bytes: &[u8]) -> Vec<u8> {
        sk.sign(signing_bytes).to_bytes().to_vec()
    }

    /// ZEB-677 S5 — sign THIS (unsigned) doc's own signing bytes as one quorum
    /// part. The co-sign ceremony calls this on the request-carried unsigned
    /// doc so B and A cover byte-identical content (canonical CBOR is
    /// deterministic).
    pub fn quorum_part_over(&self, sk: &ed25519_dalek::SigningKey) -> Result<Vec<u8>, String> {
        let bytes = self.signing_bytes()?;
        Ok(Self::sign_quorum_part(sk, &bytes))
    }

    /// ZEB-677 S5 — verify one quorum part (a detached signature) over this
    /// doc's signing bytes, against `vk`. Used A-side to validate a co-signer's
    /// epoch-doc part before assembling.
    pub fn verify_quorum_part(&self, vk: &ed25519_dalek::VerifyingKey, sig: &[u8]) -> bool {
        let Ok(bytes) = self.signing_bytes() else {
            return false;
        };
        let Ok(arr) = <[u8; 64]>::try_from(sig) else {
            return false;
        };
        vk.verify_strict(&bytes, &ed25519_dalek::Signature::from_bytes(&arr))
            .is_ok()
    }

    /// ZEB-677 S5 — stamp a K=2 quorum signature (+ its depth-1 signer bundle)
    /// onto an otherwise-unsigned doc. Clears the master-signature fields:
    /// a quorum doc is verified against its embedded signer certs, never a
    /// master key. `signers` are raw device-ids, stored hex.
    pub fn assemble_quorum(
        &mut self,
        signers: Vec<[u8; 16]>,
        signatures: Vec<Vec<u8>>,
        signer_certs: Vec<EnrollmentCert>,
    ) {
        self.master_pubkey = None;
        self.master_sig = Vec::new();
        self.quorum_sig = Some(QuorumDocSig {
            signers: signers.iter().map(hex::encode).collect(),
            signatures,
        });
        self.signer_certs = signer_certs;
    }

    /// ZEB-677 S5 — verify a quorum-signed doc against its EMBEDDED signer
    /// bundle (self-contained: the synchronous carrier merger cannot lock the
    /// async trust doc). Mirrors the crate's `verify_quorum_with_signers`:
    /// ≥2 DISTINCT signers (deduplicated on the decoded 16-byte device id, so
    /// hex-casing variants of one id cannot masquerade as a quorum — Qodo PR
    /// #461); parity signers/signatures; every signer id has a matching
    /// embedded cert; each cert is Master-issued, `owner_id`-bound, and valid
    /// **at the doc's own signed `bump_wall_ms`** (not the reader's clock — a
    /// signer cert that expires after the bump must not make an
    /// already-signed carrier unverifiable on a later boot/merge, Qodo PR
    /// #461); each signature verifies against that cert's enrolled ed25519 key
    /// over [`Self::signing_bytes`]. Live-revocation is intentionally NOT
    /// checked here — it mirrors the master path (no revocation check) and is
    /// gated in the co-sign ceremony instead.
    pub fn verify_quorum(&self, owner_id: &[u8; 16]) -> bool {
        let Some(q) = self.quorum_sig.as_ref() else {
            return false;
        };
        if q.signers.len() < 2 || q.signers.len() != q.signatures.len() {
            return false;
        }
        let Ok(bytes) = self.signing_bytes() else {
            return false;
        };
        // Verify signer certs as of the doc's signed bump time, not "now".
        let bump_secs = self.bump_wall_ms / 1000;
        let mut seen = std::collections::BTreeSet::new();
        for (signer_hex, sig_bytes) in q.signers.iter().zip(q.signatures.iter()) {
            let Ok(signer_id_vec) = hex::decode(signer_hex) else {
                return false;
            };
            let Ok(signer_id) = <[u8; 16]>::try_from(signer_id_vec.as_slice()) else {
                return false;
            };
            // Distinct signers — on the DECODED id, so "ab"/"AB" can't double.
            if !seen.insert(signer_id) {
                return false;
            }
            // Depth-1: the signer's own cert must be present, Master-issued,
            // this owner, and valid at bump time.
            let Some(cert) = self.signer_certs.iter().find(|c| c.device_id == signer_id) else {
                return false;
            };
            if cert.owner_id != *owner_id {
                return false;
            }
            if !matches!(cert.issuer, EnrollmentIssuer::Master { .. }) {
                return false;
            }
            if cert.verify(bump_secs).is_err() {
                return false;
            }
            let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(
                &cert.device_pubkeys.classical.ed25519_verify,
            ) else {
                return false;
            };
            let Ok(sig_arr) = <[u8; 64]>::try_from(sig_bytes.as_slice()) else {
                return false;
            };
            if vk
                .verify_strict(&bytes, &ed25519_dalek::Signature::from_bytes(&sig_arr))
                .is_err()
            {
                return false;
            }
        }
        true
    }
}

/// Wrapper granting the private `SigView` access to `canonical_cbor_encode`
/// (which requires `CanonicalPayload`, a sealed trait).
struct CanonicalTuple<T: serde::Serialize>(T);
impl<T: serde::Serialize> serde::Serialize for CanonicalTuple<T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}
impl<T: serde::Serialize> CanonicalPayloadSealed for CanonicalTuple<T> {}
impl<T: serde::Serialize> crate::owner_state_crypto::CanonicalPayload for CanonicalTuple<T> {}

/// Monotonic + authenticated merge for the carrier engine: adopt `remote`
/// wholesale iff its epoch is STRICTLY higher than the local doc's AND its
/// signature verifies against `owner_id` (master OR quorum — ZEB-677 S5).
/// Everything else (equal or lower epoch, unsigned, wrong owner, bad
/// signature) leaves `local` untouched. Fully self-contained — a quorum doc's
/// signer certs verify at its own signed `bump_wall_ms`, so no clock is
/// threaded in. Returns whether local changed.
pub fn merge_fleet_keys_remote(
    local: &mut FleetKeyEpochDoc,
    remote: FleetKeyEpochDoc,
    owner_id: &[u8; 16],
) -> bool {
    if remote.epoch <= local.epoch {
        return false;
    }
    if !remote.verify_any(owner_id) {
        tracing::warn!(
            remote_epoch = remote.epoch,
            quorum = remote.quorum_sig.is_some(),
            "fleet-keys carrier: rejected remote doc with invalid signature"
        );
        return false;
    }
    *local = remote;
    true
}

// ── Persistence (mirrors fleet_net_persist) ─────────────────────────────────

/// Carrier doc file in the identity dir. Content is sealed blobs + signed
/// metadata — nothing secret — so a plain file matches `fleet_net.cbor`.
pub const FLEET_KEYS_FILENAME: &str = "fleet_keys.cbor";
/// Replay-tracker sidecar for the carrier engine.
pub const FLEET_KEYS_REPLAY_FILENAME: &str = "fleet_keys_replay.cbor";
/// Zenoh payload cap for the carrier dataset (mirrors owner-trust's).
pub const FLEET_KEYS_DATASET_MAX_BYTES: usize = 256 * 1024;

const FLEET_KEYS_SCHEMA_V1: u8 = 0x01;

fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<(), crate::fleet_sync::SyncError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            crate::fleet_sync::SyncError::Persist(format!("create_dir_all {}: {e}", path.display()))
        })?;
    }
    crate::owner_state_persist::save_atomically(path, bytes)
        .map_err(|e| crate::fleet_sync::SyncError::Persist(e.to_string()))
}

/// Load the carrier doc; missing file → default; corrupt → warn + default
/// (the doc re-replicates from any sibling, so recovery is safe).
///
/// Verifies the master signature against `owner_id` for any non-default doc
/// (PR #455 round 1, Qodo bug 2): the monotonic merge rejects remotes with
/// `epoch <= local` BEFORE verifying them, so a locally-corrupted (or
/// tampered) doc with a spuriously high epoch would otherwise stall
/// adoption of every legitimate carrier doc until a strictly-higher epoch
/// appeared. An unverifiable persisted doc degrades to the default and
/// re-replicates.
pub fn load_doc_or_recover(path: &std::path::Path, owner_id: &[u8; 16]) -> FleetKeyEpochDoc {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return FleetKeyEpochDoc::default(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e,
                "fleet-keys doc read failed; starting from default");
            return FleetKeyEpochDoc::default();
        }
    };
    let doc: FleetKeyEpochDoc = match bytes.split_first() {
        Some((&FLEET_KEYS_SCHEMA_V1, rest)) => match ciborium::from_reader(rest) {
            Ok(doc) => doc,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e,
                    "fleet-keys doc decode failed; starting from default");
                return FleetKeyEpochDoc::default();
            }
        },
        _ => {
            tracing::warn!(path = %path.display(),
                "fleet-keys doc has unknown schema byte; starting from default");
            return FleetKeyEpochDoc::default();
        }
    };
    // Epoch 0 is the never-bumped default (unsigned by construction); any
    // bumped doc must carry a valid master signature.
    if doc.epoch > 0 && !doc.verify_any(owner_id) {
        tracing::warn!(path = %path.display(), epoch = doc.epoch,
            "fleet-keys doc failed signature verification; starting from default");
        return FleetKeyEpochDoc::default();
    }
    doc
}

/// Load the carrier replay tracker; any failure → empty (replay protection
/// re-arms from the next publish).
pub fn load_replay_or_recover(
    path: &std::path::Path,
) -> std::collections::BTreeMap<String, crate::owner_state_types::Hlc> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Default::default(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e,
                "fleet-keys replay read failed; starting with empty tracker");
            return Default::default();
        }
    };
    match bytes.split_first() {
        Some((&FLEET_KEYS_SCHEMA_V1, rest)) => ciborium::from_reader(rest).unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), error = %e,
                "fleet-keys replay decode failed; starting with empty tracker");
            Default::default()
        }),
        _ => Default::default(),
    }
}

/// Durability sink for the carrier engine.
pub struct FleetKeyEpochPersist {
    pub doc_path: std::path::PathBuf,
    pub replay_path: std::path::PathBuf,
}

impl crate::fleet_sync::FleetPersist<FleetKeyEpochDoc> for FleetKeyEpochPersist {
    fn persist(
        &self,
        state: &FleetKeyEpochDoc,
        tracker: &std::collections::BTreeMap<String, crate::owner_state_types::Hlc>,
    ) -> Result<(), crate::fleet_sync::SyncError> {
        let mut bytes = vec![FLEET_KEYS_SCHEMA_V1];
        ciborium::into_writer(state, &mut bytes).map_err(|e| {
            crate::fleet_sync::SyncError::CborEncode(format!("fleet-keys doc: {e}"))
        })?;
        atomic_write(&self.doc_path, &bytes)?;
        let mut rbytes = vec![FLEET_KEYS_SCHEMA_V1];
        ciborium::into_writer(tracker, &mut rbytes).map_err(|e| {
            crate::fleet_sync::SyncError::CborEncode(format!("fleet-keys replay: {e}"))
        })?;
        atomic_write(&self.replay_path, &rbytes)
    }
}

/// Persist the vault `fleet_keytree` slot from the CURRENT data accept set,
/// guaranteeing the epoch-0 material rides along (PR #455 round 1, Qodo
/// bug 1): the data set deliberately excludes epoch-0 once a higher epoch
/// exists, but epoch-0 keys the carrier forever — a slot write without it
/// would brick carrier access on the next boot. When the accept set lacks
/// epoch-0, it is re-read from the existing slot; if it cannot be found
/// anywhere, the write is REFUSED (the new epoch re-installs from the
/// carrier replay on the next merge, whereas a slot without epoch-0 is
/// unrecoverable).
pub fn persist_vault_material_set(
    keychain: &Option<crate::identity::KeychainStore>,
    identity_dir: &std::path::Path,
    keys: &crate::owner_state_crypto::FleetKeySet,
    min_epoch: u32,
) -> Result<(), String> {
    // `min_epoch` lets the window-close path persist the PRUNED set BEFORE
    // narrowing memory (PR #455 round 2, Greptile P1: durability first — a
    // failed write must leave the in-memory set wide so the next tick
    // retries). Epoch-0 rides along regardless of the floor: it is the
    // carrier key, not a data epoch.
    let mut materials: Vec<crate::owner_state_crypto::FleetKeyMaterial> = keys
        .accept_set()
        .iter()
        .filter(|k| k.epoch() >= min_epoch)
        .map(|k| k.to_fleet_material())
        .collect();
    if !materials.iter().any(|m| m.epoch == 0) {
        if let Ok(Some(bytes)) = crate::owner_state::load_fleet_keytree(keychain, identity_dir) {
            if let Ok(old_set) = crate::owner_state_crypto::decode_fleet_material_set(&bytes) {
                materials.extend(old_set.into_iter().filter(|m| m.epoch == 0));
            }
        }
    }
    if !materials.iter().any(|m| m.epoch == 0) {
        return Err(
            "refusing to persist fleet_keytree without epoch-0 material (the carrier key); \
             the pending epochs re-install from the carrier replay"
                .to_string(),
        );
    }
    let bytes = crate::owner_state_crypto::encode_fleet_material_set(&materials)?;
    crate::owner_state::save_fleet_keytree(keychain, identity_dir, &bytes)
}

// ── Receiver-side install (the on_applied consumer) ─────────────────────────

/// Open this device's sealed blob from an applied carrier doc and decode the
/// material. Pure — unit-testable without the engine. Errors are diagnostic
/// strings (the caller logs and waits for a corrected doc).
pub fn unseal_own_material(
    doc: &FleetKeyEpochDoc,
    self_device_id_hex: &str,
    device_signing_key: &ed25519_dalek::SigningKey,
) -> Result<crate::owner_state_crypto::FleetKeyMaterial, String> {
    let blob = doc.sealed.get(self_device_id_hex).ok_or_else(|| {
        format!(
            "no sealed blob for this device ({self_device_id_hex}) — revoked, \
             or enrolled after the bump (pairing delivers current material)"
        )
    })?;
    let x_priv = crate::dm_signing::ed25519_priv_to_x25519(device_signing_key);
    let opened = crate::dm_signing::open_from_owner_with_info(&x_priv, blob, FLEET_EPOCH_SEAL_INFO)
        .map_err(|e| format!("unseal fleet material: {e}"))?;
    let material: crate::owner_state_crypto::FleetKeyMaterial =
        ciborium::from_reader(opened.as_slice()).map_err(|e| format!("decode material: {e}"))?;
    if material.epoch != doc.epoch {
        return Err(format!(
            "sealed material epoch {} != carrier doc epoch {}",
            material.epoch, doc.epoch
        ));
    }
    Ok(material)
}

/// Seal `material_cbor` to every surviving device's enrollment x25519 →
/// `device_id_hex → blob`. Survivors = enrolled minus revoked (deliberately
/// NOT `active_devices`: a temporarily-offline, non-revoked device must still
/// get a blob or it is orphaned at window close). `exclude` additionally drops
/// one device — the quorum-revocation target, which may not yet be revoked in
/// this trust snapshot (ZEB-677 S5).
pub fn seal_material_to_survivors(
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

// ZEB-548 Stage 2: lives here (beside the `FleetKeyEpochDoc` it builds) rather
// than in `owner_commands`, which re-exports it back (downward), so the spine
// quorum planners no longer reach up for it.
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

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_owner::lifecycle::RecoveryArtifact;

    fn signed_doc(seed: [u8; 32], epoch: u32) -> (FleetKeyEpochDoc, [u8; 16]) {
        let artifact = RecoveryArtifact::from_seed(seed);
        let bundle = artifact.master_pubkey_bundle();
        let owner_id = bundle.identity_hash();
        let mut doc = FleetKeyEpochDoc {
            epoch,
            bump_wall_ms: 1_700_000_000_000 + u64::from(epoch),
            sealed: BTreeMap::from([(format!("device-{epoch}"), vec![0xAB; 40])]),
            master_pubkey: None,
            master_sig: Vec::new(),
            quorum_sig: None,
            signer_certs: Vec::new(),
        };
        doc.sign(&artifact.master_signing_key(), bundle)
            .expect("sign");
        (doc, owner_id)
    }

    #[test]
    fn sign_verify_round_trip_and_tamper_detection() {
        let (doc, owner_id) = signed_doc([3u8; 32], 1);
        assert!(doc.verify(&owner_id));

        // Tampered sealed map fails.
        let mut tampered = doc.clone();
        tampered
            .sealed
            .insert("intruder".to_string(), vec![0xCD; 40]);
        assert!(!tampered.verify(&owner_id));

        // Tampered epoch fails.
        let mut tampered = doc.clone();
        tampered.epoch = 2;
        assert!(!tampered.verify(&owner_id));

        // Wrong owner fails.
        assert!(!doc.verify(&[0x99; 16]));

        // The unsigned default doc never verifies.
        assert!(!FleetKeyEpochDoc::default().verify(&owner_id));
    }

    /// Mint a Master-issued device cert (+ its signing key) under `owner_sk`.
    fn mint_master_device(
        owner_sk: &ed25519_dalek::SigningKey,
        master_bundle: &PubKeyBundle,
        seed_byte: u8,
        now_secs: u64,
    ) -> (ed25519_dalek::SigningKey, EnrollmentCert) {
        let dev_sk = ed25519_dalek::SigningKey::from_bytes(&[seed_byte; 32]);
        let pkb = PubKeyBundle::classical_only(dev_sk.verifying_key().to_bytes());
        let device_id = pkb.identity_hash();
        let cert = EnrollmentCert::sign_master(
            owner_sk,
            master_bundle.clone(),
            device_id,
            pkb,
            now_secs,
            None,
        )
        .expect("sign_master");
        (dev_sk, cert)
    }

    /// Build an unsigned quorum doc at `epoch` and co-sign it with `devs`.
    fn quorum_doc(
        epoch: u32,
        devs: &[(ed25519_dalek::SigningKey, EnrollmentCert)],
    ) -> FleetKeyEpochDoc {
        let mut doc = FleetKeyEpochDoc {
            epoch,
            bump_wall_ms: 1_700_000_000_000 + u64::from(epoch),
            sealed: BTreeMap::from([("dev".to_string(), vec![0xAB; 40])]),
            ..Default::default()
        };
        let bytes = doc.signing_bytes().expect("bytes");
        let signers: Vec<[u8; 16]> = devs.iter().map(|(_, c)| c.device_id).collect();
        let signatures: Vec<Vec<u8>> = devs
            .iter()
            .map(|(sk, _)| FleetKeyEpochDoc::sign_quorum_part(sk, &bytes))
            .collect();
        let certs: Vec<EnrollmentCert> = devs.iter().map(|(_, c)| c.clone()).collect();
        doc.assemble_quorum(signers, signatures, certs);
        doc
    }

    #[test]
    fn quorum_doc_signs_verifies_and_rejects_bad_bundles() {
        let now = 1_700_000_000; // seconds
        let artifact = RecoveryArtifact::from_seed([5u8; 32]);
        let master = artifact.master_pubkey_bundle();
        let owner_sk = artifact.master_signing_key();
        let owner_id = master.identity_hash();
        let a = mint_master_device(&owner_sk, &master, 0x11, now);
        let b = mint_master_device(&owner_sk, &master, 0x22, now);

        // Happy path: two Master-issued signers.
        let doc = quorum_doc(1, &[a.clone(), b.clone()]);
        assert!(doc.verify_quorum(&owner_id), "valid quorum doc verifies");
        // A quorum doc is NOT a master doc.
        assert!(!doc.verify(&owner_id), "quorum doc has no master signature");

        // <2 signers rejected.
        let one = quorum_doc(1, std::slice::from_ref(&a));
        assert!(!one.verify_quorum(&owner_id), "single signer rejected");

        // Tampered `sealed` breaks every signature.
        let mut tampered = doc.clone();
        tampered
            .sealed
            .insert("intruder".to_string(), vec![0xCD; 8]);
        assert!(
            !tampered.verify_quorum(&owner_id),
            "tampered sealed rejected"
        );

        // Wrong owner: a signer cert from a DIFFERENT owner.
        let other = RecoveryArtifact::from_seed([9u8; 32]);
        let other_master = other.master_pubkey_bundle();
        let c = mint_master_device(&other.master_signing_key(), &other_master, 0x33, now);
        let cross = quorum_doc(1, &[a.clone(), c]);
        assert!(
            !cross.verify_quorum(&owner_id),
            "wrong-owner signer rejected"
        );

        // Depth-1: a non-Master (quorum-issued) signer cert is rejected.
        let mut depth = doc.clone();
        if let Some(cert) = depth.signer_certs.get_mut(0) {
            cert.issuer = EnrollmentIssuer::Quorum {
                signers: vec![],
                signatures: vec![],
            };
        }
        assert!(
            !depth.verify_quorum(&owner_id),
            "non-Master signer rejected (depth-1)"
        );

        // A signature by a key other than the claimed signer's.
        let mut forged = doc.clone();
        if let Some(q) = forged.quorum_sig.as_mut() {
            let bytes = FleetKeyEpochDoc {
                epoch: forged.epoch,
                bump_wall_ms: forged.bump_wall_ms,
                sealed: forged.sealed.clone(),
                ..Default::default()
            }
            .signing_bytes()
            .unwrap();
            // Sign with b's key but leave signers[0] claiming a.
            q.signatures[0] = FleetKeyEpochDoc::sign_quorum_part(&b.0, &bytes);
        }
        assert!(
            !forged.verify_quorum(&owner_id),
            "mismatched signer/key rejected"
        );
    }

    #[test]
    fn quorum_doc_wire_round_trips() {
        let now = 1_700_000_000;
        let artifact = RecoveryArtifact::from_seed([5u8; 32]);
        let master = artifact.master_pubkey_bundle();
        let owner_sk = artifact.master_signing_key();
        let a = mint_master_device(&owner_sk, &master, 0x11, now);
        let b = mint_master_device(&owner_sk, &master, 0x22, now);
        let doc = quorum_doc(2, &[a, b]);
        let bytes = canonical_cbor_encode(&doc).expect("encode");
        let back: FleetKeyEpochDoc = ciborium::from_reader(bytes.as_slice()).expect("decode");
        assert_eq!(back, doc);
        assert!(back.verify_quorum(&master.identity_hash()));
    }

    #[test]
    fn verify_quorum_rejects_hex_casing_duplicate_signer() {
        // Qodo PR #461: dedup must be on the DECODED device id, not the raw hex
        // string — else one device's id in two hex casings passes "≥2 distinct".
        let now = 1_700_000_000;
        let artifact = RecoveryArtifact::from_seed([5u8; 32]);
        let master = artifact.master_pubkey_bundle();
        let owner_id = master.identity_hash();
        let a = mint_master_device(&artifact.master_signing_key(), &master, 0x11, now);
        let doc = quorum_doc(1, &[a.clone(), a.clone()]); // same device twice
                                                          // Force an upper-case hex variant on the second signer entry so the raw
                                                          // strings differ but decode to the same id.
        let mut forged = doc.clone();
        if let Some(q) = forged.quorum_sig.as_mut() {
            q.signers[1] = q.signers[1].to_uppercase();
        }
        assert!(
            !forged.verify_quorum(&owner_id),
            "one device in two hex casings must not satisfy K=2"
        );
    }

    #[test]
    fn verify_quorum_uses_bump_time_not_readers_clock() {
        // Qodo PR #461: signer certs verify at the doc's own signed bump time,
        // so a cert that expires AFTER the bump keeps the carrier verifiable on
        // a later boot/merge (verification is time-independent of the reader).
        let issued = 1_700_000_000u64;
        let artifact = RecoveryArtifact::from_seed([5u8; 32]);
        let master = artifact.master_pubkey_bundle();
        let owner_id = master.identity_hash();
        let owner_sk = artifact.master_signing_key();
        // Certs expire 1 hour after issue; the doc's bump time is within that.
        let mk = |seed: u8| {
            let dev_sk = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
            let pkb = PubKeyBundle::classical_only(dev_sk.verifying_key().to_bytes());
            let cert = EnrollmentCert::sign_master(
                &owner_sk,
                master.clone(),
                pkb.identity_hash(),
                pkb,
                issued,
                Some(issued + 3600),
            )
            .expect("sign_master");
            (dev_sk, cert)
        };
        let mut doc = FleetKeyEpochDoc {
            epoch: 1,
            bump_wall_ms: (issued + 60) * 1000, // within the cert window
            sealed: BTreeMap::from([("dev".to_string(), vec![0xAB; 40])]),
            ..Default::default()
        };
        let bytes = doc.signing_bytes().expect("bytes");
        let devs = [mk(0x11), mk(0x22)];
        let signers: Vec<[u8; 16]> = devs.iter().map(|(_, c)| c.device_id).collect();
        let signatures: Vec<Vec<u8>> = devs
            .iter()
            .map(|(sk, _)| FleetKeyEpochDoc::sign_quorum_part(sk, &bytes))
            .collect();
        let certs: Vec<EnrollmentCert> = devs.iter().map(|(_, c)| c.clone()).collect();
        doc.assemble_quorum(signers, signatures, certs);
        // Verifies (bump time is inside the cert window) regardless of "now".
        assert!(doc.verify_quorum(&owner_id));

        // A doc bumped AFTER the certs expired does not verify.
        let mut late = doc.clone();
        late.bump_wall_ms = (issued + 7200) * 1000; // past expiry
                                                    // Re-sign so signatures still match the new bump time.
        let late_bytes = late.signing_bytes().expect("bytes");
        if let Some(q) = late.quorum_sig.as_mut() {
            q.signatures = devs
                .iter()
                .map(|(sk, _)| FleetKeyEpochDoc::sign_quorum_part(sk, &late_bytes))
                .collect();
        }
        assert!(
            !late.verify_quorum(&owner_id),
            "a bump past the signer certs' expiry must not verify"
        );
    }

    #[test]
    fn merge_adopts_strictly_higher_signed_remote_only() {
        let (remote1, owner_id) = signed_doc([3u8; 32], 1);
        let mut local = FleetKeyEpochDoc::default();

        assert!(merge_fleet_keys_remote(
            &mut local,
            remote1.clone(),
            &owner_id
        ));
        assert_eq!(local.epoch, 1);

        // Equal epoch: kept local.
        assert!(!merge_fleet_keys_remote(
            &mut local,
            remote1.clone(),
            &owner_id
        ));

        // Lower epoch after a higher one: kept local.
        let (remote2, _) = signed_doc([3u8; 32], 2);
        assert!(merge_fleet_keys_remote(&mut local, remote2, &owner_id));
        assert!(!merge_fleet_keys_remote(&mut local, remote1, &owner_id));
        assert_eq!(local.epoch, 2);
    }

    #[test]
    fn merge_rejects_bad_signature_and_wrong_signer() {
        let (good, owner_id) = signed_doc([3u8; 32], 1);

        // Stripped signature.
        let mut unsigned = good.clone();
        unsigned.master_sig = Vec::new();
        let mut local = FleetKeyEpochDoc::default();
        assert!(!merge_fleet_keys_remote(&mut local, unsigned, &owner_id));
        assert_eq!(local, FleetKeyEpochDoc::default());

        // Signed by a DIFFERENT master (a revoked device forging with its
        // own key): identity hash doesn't match this owner.
        let (forged, _other_owner) = signed_doc([9u8; 32], 5);
        assert!(!merge_fleet_keys_remote(&mut local, forged, &owner_id));
        assert_eq!(local.epoch, 0);
    }

    #[test]
    fn merge_adopts_valid_quorum_doc_and_rejects_bad_bundle() {
        let now = 1_700_000_000;
        let artifact = RecoveryArtifact::from_seed([5u8; 32]);
        let master = artifact.master_pubkey_bundle();
        let owner_id = master.identity_hash();
        let owner_sk = artifact.master_signing_key();
        let a = mint_master_device(&owner_sk, &master, 0x11, now);
        let b = mint_master_device(&owner_sk, &master, 0x22, now);

        // A valid quorum doc at a higher epoch is adopted.
        let good = quorum_doc(1, &[a.clone(), b.clone()]);
        let mut local = FleetKeyEpochDoc::default();
        assert!(merge_fleet_keys_remote(&mut local, good.clone(), &owner_id));
        assert_eq!(local.epoch, 1);
        assert!(local.quorum_sig.is_some());

        // A tampered quorum doc (extra sealed entry) at a higher epoch is rejected.
        let mut tampered = quorum_doc(2, &[a, b]);
        tampered
            .sealed
            .insert("intruder".to_string(), vec![0xEE; 8]);
        assert!(!merge_fleet_keys_remote(&mut local, tampered, &owner_id));
        assert_eq!(local.epoch, 1, "tampered quorum doc did not clobber local");
    }

    #[test]
    fn unseal_own_material_round_trips_and_rejects_epoch_mismatch() {
        let device_sk = ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32]);
        let device_x_pub =
            crate::dm_signing::ed25519_pub_to_x25519(&device_sk.verifying_key().to_bytes())
                .expect("x25519 pub");
        let kt = crate::owner_state_crypto::KeyTree::derive_at_epoch(&[8u8; 32], 4).expect("kt");
        let material = kt.to_fleet_material();
        let mut cbor = Vec::new();
        ciborium::into_writer(&material, &mut cbor).expect("encode material");
        let blob =
            crate::dm_signing::seal_to_owner_with_info(&device_x_pub, &cbor, FLEET_EPOCH_SEAL_INFO)
                .expect("seal");

        let mut doc = FleetKeyEpochDoc {
            epoch: 4,
            ..Default::default()
        };
        doc.sealed.insert("dev-a".to_string(), blob);

        let opened = unseal_own_material(&doc, "dev-a", &device_sk).expect("unseal");
        assert_eq!(opened.epoch, 4);

        // Missing blob → diagnostic error.
        assert!(unseal_own_material(&doc, "dev-b", &device_sk).is_err());

        // Wrong device key cannot open it.
        let other_sk = ed25519_dalek::SigningKey::from_bytes(&[0x43u8; 32]);
        assert!(unseal_own_material(&doc, "dev-a", &other_sk).is_err());

        // Epoch mismatch between blob and doc is rejected.
        doc.epoch = 5;
        assert!(unseal_own_material(&doc, "dev-a", &device_sk).is_err());
    }

    #[test]
    fn doc_and_replay_persist_round_trip_and_recover() {
        let dir = tempfile::tempdir().expect("tempdir");
        let doc_path = dir.path().join(FLEET_KEYS_FILENAME);
        let replay_path = dir.path().join(FLEET_KEYS_REPLAY_FILENAME);

        let (doc, owner_id) = signed_doc([3u8; 32], 2);

        // Missing files recover to defaults.
        assert_eq!(
            load_doc_or_recover(&doc_path, &owner_id),
            FleetKeyEpochDoc::default()
        );
        assert!(load_replay_or_recover(&replay_path).is_empty());
        let tracker = std::collections::BTreeMap::from([(
            "dev-a".to_string(),
            crate::owner_state_types::Hlc {
                wall_ms: 5,
                logical: 0,
                device_id: "dev-a".into(),
            },
        )]);
        let persist = FleetKeyEpochPersist {
            doc_path: doc_path.clone(),
            replay_path: replay_path.clone(),
        };
        crate::fleet_sync::FleetPersist::persist(&persist, &doc, &tracker).expect("persist");
        assert_eq!(load_doc_or_recover(&doc_path, &owner_id), doc);
        assert_eq!(load_replay_or_recover(&replay_path), tracker);

        // Corrupt doc recovers to default rather than failing the boot.
        std::fs::write(&doc_path, [0xFF, 0x00]).expect("corrupt");
        assert_eq!(
            load_doc_or_recover(&doc_path, &owner_id),
            FleetKeyEpochDoc::default()
        );

        // PR #455 round 1 (Qodo bug 2): a decodable-but-UNVERIFIED doc with
        // a spuriously high epoch must NOT load — it would stall adoption of
        // every legitimate carrier doc below its epoch.
        let mut forged = doc.clone();
        forged.epoch = 99;
        crate::fleet_sync::FleetPersist::persist(&persist, &forged, &tracker).expect("persist");
        assert_eq!(
            load_doc_or_recover(&doc_path, &owner_id),
            FleetKeyEpochDoc::default(),
            "tampered persisted doc must degrade to default"
        );
    }

    /// PR #455 round 1 (Qodo bug 1): the vault rewrite must never drop the
    /// epoch-0 carrier key, even when the data accept set excludes it.
    #[test]
    fn persist_vault_material_set_guarantees_epoch0() {
        std::env::set_var("HARMONY_PASSPHRASE", "test-passphrase");
        let dir = tempfile::tempdir().expect("tempdir");
        let seed = [6u8; 32];
        let kt0 = std::sync::Arc::new(
            crate::owner_state_crypto::KeyTree::derive_at_epoch(&seed, 0).expect("kt0"),
        );
        let kt2 = std::sync::Arc::new(
            crate::owner_state_crypto::KeyTree::derive_at_epoch(&seed, 2).expect("kt2"),
        );

        // Seed the slot with a set that INCLUDES epoch-0.
        let with_zero = crate::owner_state_crypto::FleetKeySet::new(std::sync::Arc::clone(&kt0));
        with_zero.install(std::sync::Arc::clone(&kt2));
        persist_vault_material_set(&None, dir.path(), &with_zero, 0).expect("persist with 0");

        // Now persist from a set WITHOUT epoch-0 (post-window data set):
        // epoch-0 must be re-read from the existing slot and survive.
        let without_zero = crate::owner_state_crypto::FleetKeySet::new(kt2);
        persist_vault_material_set(&None, dir.path(), &without_zero, 0).expect("persist without 0");
        let bytes = crate::owner_state::load_fleet_keytree(&None, dir.path())
            .expect("load ok")
            .expect("slot present");
        let set = crate::owner_state_crypto::decode_fleet_material_set(&bytes).expect("decode");
        let mut epochs: Vec<u32> = set.iter().map(|m| m.epoch).collect();
        epochs.sort_unstable();
        assert_eq!(epochs, vec![0, 2], "epoch-0 must ride along");

        // Empty slot + no epoch-0 in the set → REFUSE rather than brick.
        let dir2 = tempfile::tempdir().expect("tempdir2");
        let kt3 = std::sync::Arc::new(
            crate::owner_state_crypto::KeyTree::derive_at_epoch(&seed, 3).expect("kt3"),
        );
        let err = persist_vault_material_set(
            &None,
            dir2.path(),
            &crate::owner_state_crypto::FleetKeySet::new(kt3),
            0,
        )
        .expect_err("must refuse without epoch-0 anywhere");
        assert!(err.contains("epoch-0"), "{err}");

        // PR #455 round 2 (Greptile P1): the min_epoch floor persists the
        // PRUNED set (old data epoch dropped, epoch-0 carrier key kept) so
        // the close path can write durability-first.
        let kt4 = std::sync::Arc::new(
            crate::owner_state_crypto::KeyTree::derive_at_epoch(&seed, 4).expect("kt4"),
        );
        with_zero.install(kt4); // {4, 2, 0}
        persist_vault_material_set(&None, dir.path(), &with_zero, 4).expect("pruned persist");
        let bytes = crate::owner_state::load_fleet_keytree(&None, dir.path())
            .expect("load ok")
            .expect("slot present");
        let set = crate::owner_state_crypto::decode_fleet_material_set(&bytes).expect("decode");
        let mut epochs: Vec<u32> = set.iter().map(|m| m.epoch).collect();
        epochs.sort_unstable();
        assert_eq!(
            epochs,
            vec![0, 4],
            "old data epoch pruned, carrier key kept"
        );
    }

    #[test]
    fn wire_encoding_is_pinned_and_default_omits_empty_fields() {
        // Canonical CBOR of a fixture doc. NEVER regenerate this hex: a
        // mismatch means the carrier wire format changed and pre-change
        // fleets can no longer read the doc.
        let doc = FleetKeyEpochDoc {
            epoch: 2,
            bump_wall_ms: 1_700_000_000_000,
            sealed: BTreeMap::from([("aa".to_string(), vec![0x01, 0x02])]),
            master_pubkey: None,
            master_sig: vec![0x0F; 4],
            quorum_sig: None,
            signer_certs: Vec::new(),
        };
        let bytes = canonical_cbor_encode(&doc).expect("encode");
        assert_eq!(
            hex::encode(&bytes),
            "a461650261621b0000018bcfe568006173a16261618201026167840f0f0f0f",
            "carrier wire encoding drifted"
        );

        // Default doc omits `s`, `k`, `g` entirely (additive-forward shape).
        let empty = canonical_cbor_encode(&FleetKeyEpochDoc::default()).expect("encode default");
        let val: ciborium::Value =
            ciborium::from_reader(empty.as_slice()).expect("decode as value");
        let map = val.as_map().expect("cbor map");
        let keys: Vec<String> = map
            .iter()
            .map(|(k, _)| k.as_text().expect("text key").to_string())
            .collect();
        assert_eq!(keys, vec!["e", "b"]);

        // Old bytes (no s/k/g) decode with empty defaults.
        let back: FleetKeyEpochDoc = ciborium::from_reader(empty.as_slice()).expect("decode");
        assert_eq!(back, FleetKeyEpochDoc::default());
    }
}
