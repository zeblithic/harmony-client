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

#[cfg(test)]
mod tests {
    use super::*;

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
        let tampers: Vec<Box<dyn Fn(&mut PledgeListPayload)>> = vec![
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
        let tampers: Vec<Box<dyn Fn(&mut BackupSetPayload)>> = vec![
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
        let tampers: Vec<Box<dyn Fn(&mut HostingReportPayload)>> = vec![
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
        assert!(err.contains("pubkey does not match claimed address"), "{err}");
    }

    #[test]
    fn serde_camel_case_pins() {
        let id = test_identity();
        let mut p = pledge_list_for(&id);
        sign_pledge_list(&id, &mut p);
        let json = serde_json::to_string(&p).unwrap();
        for key in ["ownerAddress", "pledges", "updatedAt", "identityPub", "sig", "\"to\"", "\"bytes\""] {
            assert!(json.contains(key), "pledge json missing {key}: {json}");
        }

        let mut b = backup_set_for(&id);
        sign_backup_set(&id, &mut b);
        let json = serde_json::to_string(&b).unwrap();
        for key in ["ownerAddress", "entries", "updatedAt", "\"cid\"", "\"size\""] {
            assert!(json.contains(key), "backup json missing {key}: {json}");
        }

        let mut h = hosting_report_for(&id);
        sign_hosting_report(&id, &mut h);
        let json = serde_json::to_string(&h).unwrap();
        for key in ["ownerAddress", "reports", "updatedAt", "beneficiary", "\"cids\""] {
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
            pledges: vec![PledgeEntry { to: "ab".into(), bytes: 1 }],
            updated_at: 1,
            identity_pub: None,
            sig: None,
        };
        let two = PledgeListPayload {
            owner_address: "o".into(),
            pledges: vec![
                PledgeEntry { to: "a".into(), bytes: 1 },
                PledgeEntry { to: "b".into(), bytes: 1 },
            ],
            updated_at: 1,
            identity_pub: None,
            sig: None,
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
            pledges: vec![PledgeEntry { to: "bb".into(), bytes: 7 }],
            updated_at: 42,
            identity_pub: None,
            sig: None,
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
            entries: vec![BackupEntry { cid: "cc".into(), size: 7 }],
            updated_at: 42,
            identity_pub: None,
            sig: None,
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
        };
        let b = BackupSetPayload {
            owner_address: "o".into(),
            entries: vec![],
            updated_at: 1,
            identity_pub: None,
            sig: None,
        };
        let h = HostingReportPayload {
            owner_address: "o".into(),
            reports: vec![],
            updated_at: 1,
            identity_pub: None,
            sig: None,
        };
        let pb = pledge_list_canonical_bytes(&p);
        let bb = backup_set_canonical_bytes(&b);
        let hb = hosting_report_canonical_bytes(&h);
        assert_ne!(pb, bb);
        assert_ne!(pb, hb);
        assert_ne!(bb, hb);
    }
}
