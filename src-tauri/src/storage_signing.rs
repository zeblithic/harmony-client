//! ZEB-669 slice 2: signed wire records for the storage-buddy domain.
//!
//! Three owner-published LWW records on `harmony/storage/{owner}/…`,
//! built on the exact `vine_signing` scheme (length-prefixed injective
//! canonical bytes with u32-LE count prefixes, 64-byte identity pub,
//! `verify_strict`, pubkey→address binding). Like follow lists these
//! record types are born strict — there is no unsigned legacy.
//!
//! Privacy posture (spec §0.2): content announcements stay anonymous;
//! owner identity appears ONLY in these consenting-parties records.
//! Record contents are addresses and byte totals — never file names.

use serde::{Deserialize, Serialize};

use crate::vine_signing::{push_str, push_u64, verify_signed};

/// Domain-separation prefix + version for pledge-list canonical bytes.
pub const PLEDGE_LIST_DOMAIN: &str = "harmony-storage-pledges-v1";
/// Domain-separation prefix + version for backup-set canonical bytes.
pub const BACKUP_SET_DOMAIN: &str = "harmony-storage-backup-set-v1";
/// Domain-separation prefix + version for hosting-report canonical bytes.
pub const HOSTING_REPORT_DOMAIN: &str = "harmony-storage-hosting-v1";

// ── ZEB-679: enrolled `#2` device-key signing (`-v2` domains) ────────────
//
// Same migration family as vines (ZEB-678 S2) and DM packets (ZEB-580):
// records stay dual-signed — the legacy `#3` `identity_pub`/`sig` keep old
// receivers working and prove address ownership; the additive v2 material
// attributes the record to an enrolled `#2` device so receivers can consult
// revocation state. Unlike vines there is no per-owner authority record and
// no session handshake, so each record self-anchors: it carries the
// enrollment (+ quorum bundle) AND a `binding_sig` — the `#3` key
// countersigning `(owner_address ‖ owner_id ‖ device key)`. Without that
// countersignature an attacker could attach their own valid enrollment +
// `device_sig` to a victim's legacy-signed record and hijack attribution
// (and squat the receiver-side first-write-wins signer pin).

/// `-v2` domain: pledge list signed by the enrolled `#2` device key.
pub const PLEDGE_LIST_DOMAIN_V2: &str = "harmony-storage-pledges-v2";
/// `-v2` domain: backup set signed by the enrolled `#2` device key.
pub const BACKUP_SET_DOMAIN_V2: &str = "harmony-storage-backup-set-v2";
/// `-v2` domain: hosting report signed by the enrolled `#2` device key.
pub const HOSTING_REPORT_DOMAIN_V2: &str = "harmony-storage-hosting-v2";
/// Domain for the `#3` address↔device binding countersignature.
pub const STORAGE_BINDING_DOMAIN: &str = "harmony-storage-binding-v1";

/// Additive `#2` signer material carried by every migrated record
/// (`#[serde(flatten)]`-inlined, so the wire keys stay flat camelCase and a
/// legacy record simply omits them all).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageSignerV2 {
    /// 16-byte master owner id, hex (32 chars).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    /// The signer's `EnrollmentCert`, CBOR-hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enrollment_cbor_hex: Option<String>,
    /// Quorum signer bundle, CBOR-hex (empty ⇒ omitted; master-issued).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub signer_certs_cbor_hex: String,
    /// `#2` signature over the record's `-v2` canonical bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_sig: Option<String>,
    /// `#3` signature over [`binding_canonical_bytes`] — the address holder
    /// countersigning its `(owner_id, #2 key)` binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_sig: Option<String>,
}

impl StorageSignerV2 {
    /// True when the record carries any v2 material (the dual-path ingest
    /// treats present-but-invalid as reject, never fallback).
    pub fn is_present(&self) -> bool {
        self.owner_id.is_some()
            || self.enrollment_cbor_hex.is_some()
            || !self.signer_certs_cbor_hex.is_empty()
            || self.device_sig.is_some()
            || self.binding_sig.is_some()
    }
}

/// One pledge: bytes this owner offers to host for `to`. A 0-byte pledge
/// is a valid accept (reciprocity is social, not enforced — spec §0.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PledgeEntry {
    pub to: String,
    pub bytes: u64,
}

/// `harmony/storage/{owner}/pledges` — whole-record LWW by `updated_at`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PledgeListPayload {
    pub owner_address: String,
    pub pledges: Vec<PledgeEntry>,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_pub: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
    #[serde(flatten)]
    pub v2: StorageSignerV2,
}

/// One backup-set entry: a public durable ContentId (64-hex) and the
/// owner's claimed total size — a budget-admission HINT; receivers
/// verify actual bytes after fetch and record actuals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupEntry {
    pub cid: String,
    pub size: u64,
}

/// `harmony/storage/{owner}/backup-set` — the CIDs this owner asks
/// buddies to pin, in priority (list) order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupSetPayload {
    pub owner_address: String,
    pub entries: Vec<BackupEntry>,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_pub: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
    #[serde(flatten)]
    pub v2: StorageSignerV2,
}

/// One hosting report line: AGGREGATE bytes + CID count this owner holds
/// for `beneficiary` — never per-CID (spec §3: keeps the record tiny at
/// scale; the drawn UI never shows per-file buddy status).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostingReportEntry {
    pub beneficiary: String,
    pub bytes: u64,
    pub cids: u32,
}

/// `harmony/storage/{owner}/hosting` — what this owner reports holding
/// for its beneficiaries. Receiver-side staleness-pruned, not persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostingReportPayload {
    pub owner_address: String,
    pub reports: Vec<HostingReportEntry>,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_pub: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
    #[serde(flatten)]
    pub v2: StorageSignerV2,
}

/// Canonical bytes a pledge-list signature covers: domain ‖ owner ‖
/// updated_at ‖ u32-LE entry count ‖ each (to, bytes). The count prefix
/// pins the list boundary (same argument as follow lists).
pub fn pledge_list_canonical_bytes(p: &PledgeListPayload) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + p.pledges.len() * 48);
    push_str(&mut out, PLEDGE_LIST_DOMAIN);
    push_str(&mut out, &p.owner_address);
    push_u64(&mut out, p.updated_at);
    out.extend_from_slice(&(p.pledges.len() as u32).to_le_bytes());
    for e in &p.pledges {
        push_str(&mut out, &e.to);
        push_u64(&mut out, e.bytes);
    }
    out
}

/// Canonical bytes a backup-set signature covers.
pub fn backup_set_canonical_bytes(p: &BackupSetPayload) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + p.entries.len() * 80);
    push_str(&mut out, BACKUP_SET_DOMAIN);
    push_str(&mut out, &p.owner_address);
    push_u64(&mut out, p.updated_at);
    out.extend_from_slice(&(p.entries.len() as u32).to_le_bytes());
    for e in &p.entries {
        push_str(&mut out, &e.cid);
        push_u64(&mut out, e.size);
    }
    out
}

/// Canonical bytes a hosting-report signature covers. The u32 `cids`
/// count is widened to u64 so every numeric field shares one encoding.
pub fn hosting_report_canonical_bytes(p: &HostingReportPayload) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + p.reports.len() * 56);
    push_str(&mut out, HOSTING_REPORT_DOMAIN);
    push_str(&mut out, &p.owner_address);
    push_u64(&mut out, p.updated_at);
    out.extend_from_slice(&(p.reports.len() as u32).to_le_bytes());
    for r in &p.reports {
        push_str(&mut out, &r.beneficiary);
        push_u64(&mut out, r.bytes);
        push_u64(&mut out, u64::from(r.cids));
    }
    out
}

/// Sign a pledge list in place; `owner_address` must match `private`
/// (publish paths guard via `vine_signing::signer_address`).
pub fn sign_pledge_list(private: &harmony_identity::PrivateIdentity, p: &mut PledgeListPayload) {
    let bytes = pledge_list_canonical_bytes(p);
    p.sig = Some(hex::encode(private.sign(&bytes)));
    p.identity_pub = Some(hex::encode(private.public_identity().to_public_bytes()));
}

/// Sign a backup set in place; `owner_address` must match `private`.
pub fn sign_backup_set(private: &harmony_identity::PrivateIdentity, p: &mut BackupSetPayload) {
    let bytes = backup_set_canonical_bytes(p);
    p.sig = Some(hex::encode(private.sign(&bytes)));
    p.identity_pub = Some(hex::encode(private.public_identity().to_public_bytes()));
}

/// Sign a hosting report in place; `owner_address` must match `private`.
pub fn sign_hosting_report(
    private: &harmony_identity::PrivateIdentity,
    p: &mut HostingReportPayload,
) {
    let bytes = hosting_report_canonical_bytes(p);
    p.sig = Some(hex::encode(private.sign(&bytes)));
    p.identity_pub = Some(hex::encode(private.public_identity().to_public_bytes()));
}

/// Verify a received pledge list: signer must derive `owner_address`.
pub fn verify_pledge_list(p: &PledgeListPayload) -> Result<(), String> {
    verify_signed(
        p.identity_pub.as_deref(),
        p.sig.as_deref(),
        &p.owner_address,
        &pledge_list_canonical_bytes(p),
        "pledge list",
    )
}

/// Verify a received backup set: signer must derive `owner_address`.
pub fn verify_backup_set(p: &BackupSetPayload) -> Result<(), String> {
    verify_signed(
        p.identity_pub.as_deref(),
        p.sig.as_deref(),
        &p.owner_address,
        &backup_set_canonical_bytes(p),
        "backup set",
    )
}

/// Verify a received hosting report: signer must derive `owner_address`.
pub fn verify_hosting_report(p: &HostingReportPayload) -> Result<(), String> {
    verify_signed(
        p.identity_pub.as_deref(),
        p.sig.as_deref(),
        &p.owner_address,
        &hosting_report_canonical_bytes(p),
        "hosting report",
    )
}

// ── ZEB-679 v2: canonical bytes, dual-sign, self-anchored verify ─────────

/// Same field set as [`pledge_list_canonical_bytes`], under the `-v2` domain.
pub fn pledge_list_canonical_bytes_v2(p: &PledgeListPayload) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + p.pledges.len() * 48);
    push_str(&mut out, PLEDGE_LIST_DOMAIN_V2);
    push_str(&mut out, &p.owner_address);
    push_u64(&mut out, p.updated_at);
    out.extend_from_slice(&(p.pledges.len() as u32).to_le_bytes());
    for e in &p.pledges {
        push_str(&mut out, &e.to);
        push_u64(&mut out, e.bytes);
    }
    out
}

/// Same field set as [`backup_set_canonical_bytes`], under the `-v2` domain.
pub fn backup_set_canonical_bytes_v2(p: &BackupSetPayload) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + p.entries.len() * 80);
    push_str(&mut out, BACKUP_SET_DOMAIN_V2);
    push_str(&mut out, &p.owner_address);
    push_u64(&mut out, p.updated_at);
    out.extend_from_slice(&(p.entries.len() as u32).to_le_bytes());
    for e in &p.entries {
        push_str(&mut out, &e.cid);
        push_u64(&mut out, e.size);
    }
    out
}

/// Same field set as [`hosting_report_canonical_bytes`], under the `-v2`
/// domain.
pub fn hosting_report_canonical_bytes_v2(p: &HostingReportPayload) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + p.reports.len() * 56);
    push_str(&mut out, HOSTING_REPORT_DOMAIN_V2);
    push_str(&mut out, &p.owner_address);
    push_u64(&mut out, p.updated_at);
    out.extend_from_slice(&(p.reports.len() as u32).to_le_bytes());
    for r in &p.reports {
        push_str(&mut out, &r.beneficiary);
        push_u64(&mut out, r.bytes);
        push_u64(&mut out, u64::from(r.cids));
    }
    out
}

/// Length-prefixed bytes the `binding_sig` covers: the `#3` address holder
/// countersigning "my address is operated by device `device_ed25519` of
/// owner `owner_id`". The per-record inlining of the vine
/// `FeedAuthorityRecord.n_sig` binding (storage has no authority record).
pub fn binding_canonical_bytes(
    owner_address: &str,
    owner_id: &[u8; 16],
    device_ed25519: &[u8; 32],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(160);
    push_str(&mut out, STORAGE_BINDING_DOMAIN);
    push_str(&mut out, owner_address);
    push_str(&mut out, &hex::encode(owner_id));
    push_str(&mut out, &hex::encode(device_ed25519));
    out
}

/// The enrolled-device material a publish path supplies to dual-sign
/// (snapshotted from `DmOutbox` + trust doc; publisher-side self-checked
/// BEFORE use so a record receivers would drop is never published).
#[derive(Clone)]
pub struct StorageSignerMaterial {
    /// The enrolled `#2` device signing key.
    pub sk: std::sync::Arc<ed25519_dalek::SigningKey>,
    /// This device's own enrollment cert.
    pub cert: harmony_owner::certs::EnrollmentCert,
    /// Quorum signer bundle (empty for master-issued).
    pub signer_certs: Vec<harmony_owner::certs::EnrollmentCert>,
}

/// Build the v2 field block for one record: encodes the enrollment +
/// bundle, mints `binding_sig` with the `#3` identity and `device_sig`
/// with the `#2` key over `canonical_v2`. Errors (cbor encode) leave the
/// caller free to publish legacy-only.
fn build_v2(
    private: &harmony_identity::PrivateIdentity,
    material: &StorageSignerMaterial,
    owner_address: &str,
    canonical_v2: &[u8],
) -> Result<StorageSignerV2, String> {
    let device_ed25519 = material.sk.verifying_key().to_bytes();
    let binding = binding_canonical_bytes(owner_address, &material.cert.owner_id, &device_ed25519);
    use ed25519_dalek::Signer as _;
    Ok(StorageSignerV2 {
        owner_id: Some(hex::encode(material.cert.owner_id)),
        enrollment_cbor_hex: Some(crate::feed_authority::encode_cert(&material.cert)?),
        signer_certs_cbor_hex: crate::feed_authority::encode_certs(&material.signer_certs)?,
        device_sig: Some(hex::encode(material.sk.sign(canonical_v2).to_bytes())),
        binding_sig: Some(hex::encode(private.sign(&binding))),
    })
}

/// Dual-sign a pledge list: legacy `#3` (unchanged) + v2 device block.
pub fn sign_pledge_list_v2(
    private: &harmony_identity::PrivateIdentity,
    material: &StorageSignerMaterial,
    p: &mut PledgeListPayload,
) -> Result<(), String> {
    sign_pledge_list(private, p);
    p.v2 = build_v2(
        private,
        material,
        &p.owner_address,
        &pledge_list_canonical_bytes_v2(p),
    )?;
    Ok(())
}

/// Dual-sign a backup set: legacy `#3` (unchanged) + v2 device block.
pub fn sign_backup_set_v2(
    private: &harmony_identity::PrivateIdentity,
    material: &StorageSignerMaterial,
    p: &mut BackupSetPayload,
) -> Result<(), String> {
    sign_backup_set(private, p);
    p.v2 = build_v2(
        private,
        material,
        &p.owner_address,
        &backup_set_canonical_bytes_v2(p),
    )?;
    Ok(())
}

/// Dual-sign a hosting report: legacy `#3` (unchanged) + v2 device block.
pub fn sign_hosting_report_v2(
    private: &harmony_identity::PrivateIdentity,
    material: &StorageSignerMaterial,
    p: &mut HostingReportPayload,
) -> Result<(), String> {
    sign_hosting_report(private, p);
    p.v2 = build_v2(
        private,
        material,
        &p.owner_address,
        &hosting_report_canonical_bytes_v2(p),
    )?;
    Ok(())
}

/// What a successful v2 verification yields: the revocation-store key pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedStorageSigner {
    pub owner_id: [u8; 16],
    pub device_ed25519: [u8; 32],
}

/// Shared v2 verification core. Ingest order (all bounded before decode,
/// same posture as the vine `-v2` path):
/// 1. `owner_id` present + 32-hex.
/// 2. Enrollment (+ bundle) decode via the capped cbor-hex decoders, then
///    the [`crate::enrollment_verify`] chokepoint bound to `owner_id`.
/// 3. `binding_sig`: the record's legacy `identity_pub` (whose address
///    binding the legacy verify already proved) countersigns
///    `(owner_address ‖ owner_id ‖ device key)` — kills the material-swap
///    attack; only the `#3` holder can mint it.
/// 4. `device_sig` over `canonical_v2` against the enrolled `#2` key.
///
/// Revocation is NOT consulted here (chokepoint convention, ZEB-677): the
/// ingest layer checks `RevokedDeviceProjection::is_revoked` on the
/// returned pair.
fn verify_v2_common(
    owner_address: &str,
    identity_pub: Option<&str>,
    v2: &StorageSignerV2,
    canonical_v2: &[u8],
    now_secs: u64,
    what: &str,
) -> Result<VerifiedStorageSigner, String> {
    let owner_hex = v2
        .owner_id
        .as_deref()
        .ok_or_else(|| format!("{what} v2 missing owner_id"))?;
    if owner_hex.len() != 32 {
        return Err(format!("{what} owner_id must be 32 hex chars (16 bytes)"));
    }
    let mut owner_id = [0u8; 16];
    hex::decode_to_slice(owner_hex, &mut owner_id)
        .map_err(|e| format!("{what} owner_id is not hex: {e}"))?;
    let enrollment = crate::feed_authority::decode_cert(
        v2.enrollment_cbor_hex
            .as_deref()
            .ok_or_else(|| format!("{what} v2 missing enrollment"))?,
    )?;
    let signer_certs = crate::feed_authority::decode_certs(&v2.signer_certs_cbor_hex)?;
    let verified = crate::enrollment_verify::verify_enrollment_any_issuer(
        &enrollment,
        &signer_certs,
        Some(&owner_id),
        now_secs,
    )
    .map_err(|e| format!("{what} enrollment invalid: {e}"))?;
    let binding = binding_canonical_bytes(owner_address, &owner_id, &verified.device_ed25519);
    verify_signed(
        identity_pub,
        v2.binding_sig.as_deref(),
        owner_address,
        &binding,
        "storage binding",
    )
    .map_err(|e| format!("{what} binding: {e}"))?;
    crate::vine_signing::verify_device_sig(
        v2.device_sig.as_deref(),
        &verified.device_ed25519,
        canonical_v2,
        what,
    )?;
    Ok(VerifiedStorageSigner {
        owner_id,
        device_ed25519: verified.device_ed25519,
    })
}

/// Verify a pledge list's v2 device block. `now_secs` is verifier-
/// controlled (supplied by the ingest boundary), never a record field.
pub fn verify_pledge_list_v2(
    p: &PledgeListPayload,
    now_secs: u64,
) -> Result<VerifiedStorageSigner, String> {
    verify_v2_common(
        &p.owner_address,
        p.identity_pub.as_deref(),
        &p.v2,
        &pledge_list_canonical_bytes_v2(p),
        now_secs,
        "pledge list",
    )
}

/// Verify a backup set's v2 device block.
pub fn verify_backup_set_v2(
    p: &BackupSetPayload,
    now_secs: u64,
) -> Result<VerifiedStorageSigner, String> {
    verify_v2_common(
        &p.owner_address,
        p.identity_pub.as_deref(),
        &p.v2,
        &backup_set_canonical_bytes_v2(p),
        now_secs,
        "backup set",
    )
}

/// Verify a hosting report's v2 device block.
pub fn verify_hosting_report_v2(
    p: &HostingReportPayload,
    now_secs: u64,
) -> Result<VerifiedStorageSigner, String> {
    verify_v2_common(
        &p.owner_address,
        p.identity_pub.as_deref(),
        &p.v2,
        &hosting_report_canonical_bytes_v2(p),
        now_secs,
        "hosting report",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mutation applied to a signed payload to prove the signature
    /// covers the mutated field.
    type Tamper<T> = Box<dyn Fn(&mut T)>;

    fn test_identity() -> harmony_identity::PrivateIdentity {
        harmony_identity::PrivateIdentity::generate(&mut rand::rngs::OsRng)
    }

    fn addr_of(private: &harmony_identity::PrivateIdentity) -> String {
        hex::encode(private.public_identity().address_hash)
    }

    fn pledge_list_for(private: &harmony_identity::PrivateIdentity) -> PledgeListPayload {
        PledgeListPayload {
            owner_address: addr_of(private),
            pledges: vec![PledgeEntry {
                to: "buddy-address".into(),
                bytes: 1_000_000,
            }],
            updated_at: 1_700_000_000,
            identity_pub: None,
            sig: None,
            v2: Default::default(),
        }
    }

    fn backup_set_for(private: &harmony_identity::PrivateIdentity) -> BackupSetPayload {
        BackupSetPayload {
            owner_address: addr_of(private),
            entries: vec![BackupEntry {
                cid: "cafe01".into(),
                size: 4096,
            }],
            updated_at: 1_700_000_000,
            identity_pub: None,
            sig: None,
            v2: Default::default(),
        }
    }

    fn hosting_report_for(private: &harmony_identity::PrivateIdentity) -> HostingReportPayload {
        HostingReportPayload {
            owner_address: addr_of(private),
            reports: vec![HostingReportEntry {
                beneficiary: "buddy-address".into(),
                bytes: 4096,
                cids: 1,
            }],
            updated_at: 1_700_000_000,
            identity_pub: None,
            sig: None,
            v2: Default::default(),
        }
    }

    #[test]
    fn pledge_list_sign_verify_roundtrip() {
        let id = test_identity();
        let mut p = pledge_list_for(&id);
        sign_pledge_list(&id, &mut p);
        assert!(verify_pledge_list(&p).is_ok());
    }

    #[test]
    fn backup_set_sign_verify_roundtrip() {
        let id = test_identity();
        let mut p = backup_set_for(&id);
        sign_backup_set(&id, &mut p);
        assert!(verify_backup_set(&p).is_ok());
    }

    #[test]
    fn hosting_report_sign_verify_roundtrip() {
        let id = test_identity();
        let mut p = hosting_report_for(&id);
        sign_hosting_report(&id, &mut p);
        assert!(verify_hosting_report(&p).is_ok());
    }

    #[test]
    fn unsigned_records_rejected_with_unsigned_message() {
        let id = test_identity();
        let err = verify_pledge_list(&pledge_list_for(&id)).unwrap_err();
        assert!(err.contains("is unsigned"), "{err}");
        let err = verify_backup_set(&backup_set_for(&id)).unwrap_err();
        assert!(err.contains("is unsigned"), "{err}");
        let err = verify_hosting_report(&hosting_report_for(&id)).unwrap_err();
        assert!(err.contains("is unsigned"), "{err}");
    }

    #[test]
    fn tampered_pledge_fields_invalidate_signature() {
        let id = test_identity();
        let tampers: Vec<Tamper<PledgeListPayload>> = vec![
            Box::new(|p| p.updated_at += 1),
            Box::new(|p| p.pledges[0].bytes += 1),
            Box::new(|p| p.pledges[0].to.push('x')),
            Box::new(|p| p.pledges.clear()),
        ];
        for tamper in tampers {
            let mut p = pledge_list_for(&id);
            sign_pledge_list(&id, &mut p);
            tamper(&mut p);
            let err = verify_pledge_list(&p).unwrap_err();
            assert!(err.contains("signature invalid"), "{err}");
        }
    }

    #[test]
    fn tampered_backup_set_fields_invalidate_signature() {
        let id = test_identity();
        let tampers: Vec<Tamper<BackupSetPayload>> = vec![
            Box::new(|p| p.updated_at += 1),
            Box::new(|p| p.entries[0].size += 1),
            Box::new(|p| p.entries[0].cid.push('f')),
        ];
        for tamper in tampers {
            let mut p = backup_set_for(&id);
            sign_backup_set(&id, &mut p);
            tamper(&mut p);
            let err = verify_backup_set(&p).unwrap_err();
            assert!(err.contains("signature invalid"), "{err}");
        }
    }

    #[test]
    fn tampered_hosting_report_fields_invalidate_signature() {
        let id = test_identity();
        let tampers: Vec<Tamper<HostingReportPayload>> = vec![
            Box::new(|p| p.updated_at += 1),
            Box::new(|p| p.reports[0].bytes += 1),
            Box::new(|p| p.reports[0].cids += 1),
            Box::new(|p| p.reports[0].beneficiary.push('x')),
        ];
        for tamper in tampers {
            let mut p = hosting_report_for(&id);
            sign_hosting_report(&id, &mut p);
            tamper(&mut p);
            let err = verify_hosting_report(&p).unwrap_err();
            assert!(err.contains("signature invalid"), "{err}");
        }
    }

    #[test]
    fn forged_signer_pubkey_address_mismatch() {
        let attacker = test_identity();
        let victim = test_identity();
        let mut p = pledge_list_for(&victim); // claims victim's address
        sign_pledge_list(&attacker, &mut p); // signed by attacker
        let err = verify_pledge_list(&p).unwrap_err();
        assert!(
            err.contains("pubkey does not match claimed address"),
            "{err}"
        );
    }

    #[test]
    fn serde_camel_case_pins() {
        let id = test_identity();
        let mut p = pledge_list_for(&id);
        sign_pledge_list(&id, &mut p);
        let json = serde_json::to_string(&p).unwrap();
        for key in [
            "ownerAddress",
            "pledges",
            "updatedAt",
            "identityPub",
            "sig",
            "\"to\"",
            "\"bytes\"",
        ] {
            assert!(json.contains(key), "pledge json missing {key}: {json}");
        }

        let mut b = backup_set_for(&id);
        sign_backup_set(&id, &mut b);
        let json = serde_json::to_string(&b).unwrap();
        for key in [
            "ownerAddress",
            "entries",
            "updatedAt",
            "\"cid\"",
            "\"size\"",
        ] {
            assert!(json.contains(key), "backup json missing {key}: {json}");
        }

        let mut h = hosting_report_for(&id);
        sign_hosting_report(&id, &mut h);
        let json = serde_json::to_string(&h).unwrap();
        for key in [
            "ownerAddress",
            "reports",
            "updatedAt",
            "beneficiary",
            "\"cids\"",
        ] {
            assert!(json.contains(key), "hosting json missing {key}: {json}");
        }
    }

    /// Decode-old pin: a record written without signature fields parses
    /// (serde default) but verification calls it unsigned.
    #[test]
    fn unsigned_wire_json_parses_but_fails_verification() {
        let json = r#"{"ownerAddress":"abc","pledges":[{"to":"def","bytes":5}],"updatedAt":9}"#;
        let p: PledgeListPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.pledges.len(), 1);
        assert!(verify_pledge_list(&p).unwrap_err().contains("is unsigned"));
    }

    /// The count prefix pins entry boundaries: two single-char pledges
    /// cannot collide with one two-char pledge.
    #[test]
    fn canonical_entry_boundaries_pinned() {
        let one = PledgeListPayload {
            owner_address: "o".into(),
            pledges: vec![PledgeEntry {
                to: "ab".into(),
                bytes: 1,
            }],
            updated_at: 1,
            identity_pub: None,
            sig: None,
            v2: Default::default(),
        };
        let two = PledgeListPayload {
            owner_address: "o".into(),
            pledges: vec![
                PledgeEntry {
                    to: "a".into(),
                    bytes: 1,
                },
                PledgeEntry {
                    to: "b".into(),
                    bytes: 1,
                },
            ],
            updated_at: 1,
            identity_pub: None,
            sig: None,
            v2: Default::default(),
        };
        assert_ne!(
            pledge_list_canonical_bytes(&one),
            pledge_list_canonical_bytes(&two)
        );
    }

    /// Golden wire-format pins (spec §8): the canonical byte encoding is
    /// frozen. If one of these fails, the wire format changed and every
    /// deployed signature breaks — bump the domain version instead.
    #[test]
    fn canonical_bytes_golden_pins() {
        let p = PledgeListPayload {
            owner_address: "aa".into(),
            pledges: vec![PledgeEntry {
                to: "bb".into(),
                bytes: 7,
            }],
            updated_at: 42,
            identity_pub: None,
            sig: None,
            v2: Default::default(),
        };
        assert_eq!(
            hex::encode(pledge_list_canonical_bytes(&p)),
            "1a0000006861726d6f6e792d73746f726167652d706c65646765732d7631\
             020000006161\
             2a00000000000000\
             01000000\
             020000006262\
             0700000000000000"
                .replace([' ', '\n'], "")
        );

        let b = BackupSetPayload {
            owner_address: "aa".into(),
            entries: vec![BackupEntry {
                cid: "cc".into(),
                size: 7,
            }],
            updated_at: 42,
            identity_pub: None,
            sig: None,
            v2: Default::default(),
        };
        assert_eq!(
            hex::encode(backup_set_canonical_bytes(&b)),
            "1d0000006861726d6f6e792d73746f726167652d6261636b75702d7365742d7631\
             020000006161\
             2a00000000000000\
             01000000\
             020000006363\
             0700000000000000"
                .replace([' ', '\n'], "")
        );

        let h = HostingReportPayload {
            owner_address: "aa".into(),
            reports: vec![HostingReportEntry {
                beneficiary: "bb".into(),
                bytes: 7,
                cids: 3,
            }],
            updated_at: 42,
            identity_pub: None,
            sig: None,
            v2: Default::default(),
        };
        assert_eq!(
            hex::encode(hosting_report_canonical_bytes(&h)),
            "1a0000006861726d6f6e792d73746f726167652d686f7374696e672d7631\
             020000006161\
             2a00000000000000\
             01000000\
             020000006262\
             0700000000000000\
             0300000000000000"
                .replace([' ', '\n'], "")
        );
    }

    // ── ZEB-679 v2 tests ─────────────────────────────────────────────

    use crate::enrollment_verify::quorum_fixtures::{mint_quorum_world, QuorumWorld, WORLD_NOW};

    fn master_material(world: &QuorumWorld) -> StorageSignerMaterial {
        StorageSignerMaterial {
            sk: std::sync::Arc::new(world.a_sk.clone()),
            cert: world.a_cert.clone(),
            signer_certs: Vec::new(),
        }
    }

    fn quorum_material(world: &QuorumWorld) -> StorageSignerMaterial {
        StorageSignerMaterial {
            sk: std::sync::Arc::new(world.c_sk.clone()),
            cert: world.c_quorum_cert.clone(),
            signer_certs: world.bundle.clone(),
        }
    }

    #[test]
    fn pledge_v2_dual_sign_verify_roundtrip_zeb679() {
        let world = mint_quorum_world(0xE0);
        let id = test_identity();
        let mut p = pledge_list_for(&id);
        sign_pledge_list_v2(&id, &master_material(&world), &mut p).expect("dual-sign");
        // Legacy path untouched: old receivers still verify.
        verify_pledge_list(&p).expect("legacy verifies");
        let v = verify_pledge_list_v2(&p, WORLD_NOW).expect("v2 verifies");
        assert_eq!(v.owner_id, world.owner_id);
        assert_eq!(
            v.device_ed25519,
            world.a_cert.device_pubkeys.classical.ed25519_verify
        );
    }

    #[test]
    fn backup_and_hosting_v2_roundtrips_zeb679() {
        let world = mint_quorum_world(0xE4);
        let id = test_identity();
        let mut b = backup_set_for(&id);
        sign_backup_set_v2(&id, &master_material(&world), &mut b).expect("dual-sign backup");
        verify_backup_set(&b).expect("legacy verifies");
        verify_backup_set_v2(&b, WORLD_NOW).expect("v2 verifies");

        let mut h = hosting_report_for(&id);
        sign_hosting_report_v2(&id, &master_material(&world), &mut h).expect("dual-sign hosting");
        verify_hosting_report(&h).expect("legacy verifies");
        verify_hosting_report_v2(&h, WORLD_NOW).expect("v2 verifies");
    }

    #[test]
    fn quorum_material_verifies_with_bundle_only_zeb679() {
        let world = mint_quorum_world(0xE8);
        let id = test_identity();
        let mut p = pledge_list_for(&id);
        sign_pledge_list_v2(&id, &quorum_material(&world), &mut p).expect("dual-sign");
        let v = verify_pledge_list_v2(&p, WORLD_NOW).expect("quorum v2 verifies with bundle");
        assert_eq!(v.owner_id, world.owner_id);
        // Stripping the bundle must fail the enrollment chokepoint.
        let mut stripped = p.clone();
        stripped.v2.signer_certs_cbor_hex = String::new();
        let err = verify_pledge_list_v2(&stripped, WORLD_NOW).unwrap_err();
        assert!(err.contains("enrollment invalid"), "{err}");
    }

    /// The load-bearing binding test: an attacker attaches their OWN valid
    /// enrollment + device_sig to a victim's legacy-signed record. Both
    /// attacker credentials are genuine — only `binding_sig` (the victim's
    /// `#3` countersignature over the binding) can reject the swap.
    #[test]
    fn swap_attack_foreign_v2_material_rejected_zeb679() {
        let attacker_world = mint_quorum_world(0xF0);
        let victim = test_identity();
        let attacker = test_identity();
        let mut p = pledge_list_for(&victim);
        sign_pledge_list(&victim, &mut p); // victim's genuine legacy record
        p.v2 = build_v2(
            &attacker,
            &master_material(&attacker_world),
            &p.owner_address,
            &pledge_list_canonical_bytes_v2(&p),
        )
        .expect("attacker builds v2 block");
        // Legacy still verifies (the victim's record content is untouched)…
        verify_pledge_list(&p).expect("legacy untouched");
        // …but the v2 layer must reject: the attacker cannot mint the
        // victim-address binding countersignature.
        let err = verify_pledge_list_v2(&p, WORLD_NOW).unwrap_err();
        assert!(err.contains("binding"), "{err}");
    }

    #[test]
    fn tampered_content_invalidates_device_sig_zeb679() {
        let world = mint_quorum_world(0xE0);
        let id = test_identity();
        let mut p = pledge_list_for(&id);
        sign_pledge_list_v2(&id, &master_material(&world), &mut p).expect("dual-sign");
        p.updated_at += 1;
        sign_pledge_list(&id, &mut p); // re-sign legacy only, keep v2 stale
        verify_pledge_list(&p).expect("legacy verifies after re-sign");
        let err = verify_pledge_list_v2(&p, WORLD_NOW).unwrap_err();
        assert!(err.contains("device signature invalid"), "{err}");
    }

    #[test]
    fn owner_id_mismatch_rejected_zeb679() {
        let world = mint_quorum_world(0xE0);
        let other_world = mint_quorum_world(0xF4);
        let id = test_identity();
        let mut p = pledge_list_for(&id);
        sign_pledge_list_v2(&id, &master_material(&world), &mut p).expect("dual-sign");
        p.v2.owner_id = Some(hex::encode(other_world.owner_id));
        let err = verify_pledge_list_v2(&p, WORLD_NOW).unwrap_err();
        assert!(err.contains("enrollment invalid"), "{err}");
    }

    #[test]
    fn v2_serde_camel_case_pins_and_legacy_omission_zeb679() {
        let world = mint_quorum_world(0xE4);
        let id = test_identity();
        let mut p = pledge_list_for(&id);
        sign_pledge_list_v2(&id, &quorum_material(&world), &mut p).expect("dual-sign");
        let json = serde_json::to_string(&p).unwrap();
        for key in [
            "ownerId",
            "enrollmentCborHex",
            "signerCertsCborHex",
            "deviceSig",
            "bindingSig",
        ] {
            assert!(json.contains(key), "v2 json missing {key}: {json}");
        }
        // Round-trips through the flatten layer.
        let back: PledgeListPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back.v2, p.v2);
        verify_pledge_list_v2(&back, WORLD_NOW).expect("round-tripped v2 verifies");

        // A legacy-only record serializes NONE of the v2 keys.
        let mut legacy = pledge_list_for(&id);
        sign_pledge_list(&id, &mut legacy);
        let legacy_json = serde_json::to_string(&legacy).unwrap();
        for key in ["ownerId", "enrollmentCborHex", "deviceSig", "bindingSig"] {
            assert!(!legacy_json.contains(key), "legacy json leaked {key}");
        }
        assert!(!legacy.v2.is_present());
    }

    #[test]
    fn v2_domains_distinct_from_v1_zeb679() {
        let id = test_identity();
        let p = pledge_list_for(&id);
        assert_ne!(
            pledge_list_canonical_bytes(&p),
            pledge_list_canonical_bytes_v2(&p)
        );
        let b = backup_set_for(&id);
        assert_ne!(
            backup_set_canonical_bytes(&b),
            backup_set_canonical_bytes_v2(&b)
        );
        let h = hosting_report_for(&id);
        assert_ne!(
            hosting_report_canonical_bytes(&h),
            hosting_report_canonical_bytes_v2(&h)
        );
    }

    /// Same owner/updated_at with empty entry lists must still differ
    /// across record types — the domain constant separates them.
    #[test]
    fn domains_are_distinct_across_record_types() {
        let p = PledgeListPayload {
            owner_address: "o".into(),
            pledges: vec![],
            updated_at: 1,
            identity_pub: None,
            sig: None,
            v2: Default::default(),
        };
        let b = BackupSetPayload {
            owner_address: "o".into(),
            entries: vec![],
            updated_at: 1,
            identity_pub: None,
            sig: None,
            v2: Default::default(),
        };
        let h = HostingReportPayload {
            owner_address: "o".into(),
            reports: vec![],
            updated_at: 1,
            identity_pub: None,
            sig: None,
            v2: Default::default(),
        };
        let pb = pledge_list_canonical_bytes(&p);
        let bb = backup_set_canonical_bytes(&b);
        let hb = hosting_report_canonical_bytes(&h);
        assert_ne!(pb, bb);
        assert_ne!(pb, hb);
        assert_ne!(bb, hb);
    }
}
