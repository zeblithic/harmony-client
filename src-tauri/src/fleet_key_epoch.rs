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
use harmony_owner::pubkey_bundle::PubKeyBundle;

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
/// master signature verifies against `owner_id`. Everything else (equal or
/// lower epoch, unsigned, wrong owner, bad signature) leaves `local`
/// untouched. Returns whether local changed.
pub fn merge_fleet_keys_remote(
    local: &mut FleetKeyEpochDoc,
    remote: FleetKeyEpochDoc,
    owner_id: &[u8; 16],
) -> bool {
    if remote.epoch <= local.epoch {
        return false;
    }
    if !remote.verify(owner_id) {
        tracing::warn!(
            remote_epoch = remote.epoch,
            "fleet-keys carrier: rejected remote doc with invalid master signature"
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
pub fn load_doc_or_recover(path: &std::path::Path) -> FleetKeyEpochDoc {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return FleetKeyEpochDoc::default(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e,
                "fleet-keys doc read failed; starting from default");
            return FleetKeyEpochDoc::default();
        }
    };
    match bytes.split_first() {
        Some((&FLEET_KEYS_SCHEMA_V1, rest)) => match ciborium::from_reader(rest) {
            Ok(doc) => doc,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e,
                    "fleet-keys doc decode failed; starting from default");
                FleetKeyEpochDoc::default()
            }
        },
        _ => {
            tracing::warn!(path = %path.display(),
                "fleet-keys doc has unknown schema byte; starting from default");
            FleetKeyEpochDoc::default()
        }
    }
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

        // Missing files recover to defaults.
        assert_eq!(load_doc_or_recover(&doc_path), FleetKeyEpochDoc::default());
        assert!(load_replay_or_recover(&replay_path).is_empty());

        let (doc, _owner) = signed_doc([3u8; 32], 2);
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
        assert_eq!(load_doc_or_recover(&doc_path), doc);
        assert_eq!(load_replay_or_recover(&replay_path), tracker);

        // Corrupt doc recovers to default rather than failing the boot.
        std::fs::write(&doc_path, [0xFF, 0x00]).expect("corrupt");
        assert_eq!(load_doc_or_recover(&doc_path), FleetKeyEpochDoc::default());
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
