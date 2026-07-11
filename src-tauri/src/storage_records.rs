//! ZEB-669 slice 2: bounded LWW store for REMOTE storage-buddy records.
//!
//! Mirrors the follow-list half of `vine_feed_cache`: strict verify-first
//! ingest (byte cap → parse → signature → pubkey→address binding → topic
//! shape → caps → eligibility) before any state effect, whole-record LWW
//! replace by `updated_at` (strictly-greater wins), bounded owner maps
//! with stalest-evicted overflow.
//!
//! Pledge lists and backup sets persist to `storage_records.json`
//! (verify-once-at-ingest — signatures are never written to disk, the
//! `TombstoneOnDisk` posture). Hosting reports are deliberately
//! in-memory only: they are staleness-pruned freshness signals, and a
//! stale report surviving a restart would claim liveness we cannot show.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::storage_signing::{
    self, BackupEntry, BackupSetPayload, HostingReportEntry, HostingReportPayload, PledgeEntry,
    PledgeListPayload,
};

/// Spec §3 cap: pledges per list.
pub const MAX_PLEDGES_PER_LIST: usize = 64;
/// Wire cap for pledge lists, checked before serde.
pub const MAX_PLEDGE_LIST_WIRE_BYTES: usize = 16 * 1024;
/// Spec §3 cap: backup-set entries.
pub const MAX_BACKUP_ENTRIES: usize = 1000;
/// Spec §3 wire cap for backup sets, checked before serde.
pub const MAX_BACKUP_SET_WIRE_BYTES: usize = 96 * 1024;
/// Spec §3 cap: hosting-report lines (aggregate per beneficiary).
pub const MAX_HOSTING_REPORTS: usize = 64;
/// Wire cap for hosting reports, checked before serde.
pub const MAX_HOSTING_REPORT_WIRE_BYTES: usize = 16 * 1024;
/// Bounded-store cap per record family; stalest owner evicted beyond it.
pub const MAX_TRACKED_OWNERS: usize = 1024;
/// Cadence at which the local node republishes its hosting report.
pub const HOSTING_REFRESH_INTERVAL_MS: u64 = 300_000;
/// Receiver-side staleness bound for hosting reports (spec §3: ≥ 3
/// refresh intervals).
pub const HOSTING_REPORT_STALE_MS: u64 = 3 * HOSTING_REFRESH_INTERVAL_MS;

const RECORDS_FILE_VERSION: u32 = 1;

/// Ingest outcome, shared by all three record families. `Rejected`
/// carries a reason for debug logging; it implies ZERO state effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordOutcome {
    Inserted,
    UpdatedNewer,
    IgnoredOlder,
    Rejected(String),
}

impl RecordOutcome {
    /// True when the store changed — the caller's emit-on-real-change
    /// signal.
    pub fn changed(&self) -> bool {
        matches!(self, RecordOutcome::Inserted | RecordOutcome::UpdatedNewer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PledgeListRecord {
    pub pledges: Vec<PledgeEntry>,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupSetRecord {
    pub entries: Vec<BackupEntry>,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostingReportRecord {
    pub reports: Vec<HostingReportEntry>,
    pub updated_at: u64,
    /// Local receipt clock (ms) — drives staleness pruning and the
    /// "report age" surfaced to the UI.
    pub received_at_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PledgeListOnDisk {
    owner: String,
    pledges: Vec<PledgeEntry>,
    updated_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackupSetOnDisk {
    owner: String,
    entries: Vec<BackupEntry>,
    updated_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StorageRecordsDiskV1 {
    version: u32,
    #[serde(default)]
    pledge_lists: Vec<PledgeListOnDisk>,
    #[serde(default)]
    backup_sets: Vec<BackupSetOnDisk>,
}

/// Bounded store of verified remote records, keyed by owner address.
#[derive(Debug)]
pub struct StorageRecordStore {
    pledge_lists: HashMap<String, PledgeListRecord>,
    backup_sets: HashMap<String, BackupSetRecord>,
    hosting_reports: HashMap<String, HostingReportRecord>,
    path: Option<PathBuf>,
}

impl StorageRecordStore {
    /// Load from `path` (tolerant: missing/corrupt/foreign-version ⇒
    /// empty store) and re-apply every bound the ingest path enforces —
    /// a tampered disk file must not smuggle an over-cap record in.
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut store = Self {
            pledge_lists: HashMap::new(),
            backup_sets: HashMap::new(),
            hosting_reports: HashMap::new(),
            path,
        };
        let Some(p) = store.path.clone() else {
            return store;
        };
        let Ok(bytes) = std::fs::read(&p) else {
            return store;
        };
        let disk: StorageRecordsDiskV1 = match serde_json::from_slice(&bytes) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("storage_records load failed, starting empty: {e}");
                return store;
            }
        };
        if disk.version != RECORDS_FILE_VERSION {
            tracing::warn!(
                "storage_records version {} unsupported, starting empty",
                disk.version
            );
            return store;
        }
        for row in disk.pledge_lists {
            if row.pledges.len() > MAX_PLEDGES_PER_LIST {
                continue;
            }
            store.pledge_lists.insert(
                row.owner,
                PledgeListRecord {
                    pledges: row.pledges,
                    updated_at: row.updated_at,
                },
            );
        }
        for row in disk.backup_sets {
            if row.entries.len() > MAX_BACKUP_ENTRIES {
                continue;
            }
            if let Err(reason) = validate_backup_entries(&row.entries) {
                tracing::warn!(owner = %row.owner, %reason, "storage_records reload: dropping ineligible backup set");
                continue;
            }
            store.backup_sets.insert(
                row.owner,
                BackupSetRecord {
                    entries: row.entries,
                    updated_at: row.updated_at,
                },
            );
        }
        evict_stalest(&mut store.pledge_lists, |r| r.updated_at);
        evict_stalest(&mut store.backup_sets, |r| r.updated_at);
        store
    }

    fn save(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let mut pledge_lists: Vec<PledgeListOnDisk> = self
            .pledge_lists
            .iter()
            .map(|(owner, r)| PledgeListOnDisk {
                owner: owner.clone(),
                pledges: r.pledges.clone(),
                updated_at: r.updated_at,
            })
            .collect();
        pledge_lists.sort_by(|a, b| a.owner.cmp(&b.owner));
        let mut backup_sets: Vec<BackupSetOnDisk> = self
            .backup_sets
            .iter()
            .map(|(owner, r)| BackupSetOnDisk {
                owner: owner.clone(),
                entries: r.entries.clone(),
                updated_at: r.updated_at,
            })
            .collect();
        backup_sets.sort_by(|a, b| a.owner.cmp(&b.owner));
        let disk = StorageRecordsDiskV1 {
            version: RECORDS_FILE_VERSION,
            pledge_lists,
            backup_sets,
        };
        match serde_json::to_vec_pretty(&disk) {
            Ok(bytes) => {
                if let Err(e) = crate::identity::write_atomic_0600(path, &bytes) {
                    tracing::error!("storage_records save failed: {e}");
                }
            }
            Err(e) => tracing::error!("storage_records serialize failed: {e}"),
        }
    }

    /// Verify-first ingest of a `harmony/storage/{owner}/pledges` sample.
    pub fn on_pledge_list_sample(&mut self, key_expr: &str, payload: &[u8]) -> RecordOutcome {
        if payload.len() > MAX_PLEDGE_LIST_WIRE_BYTES {
            return RecordOutcome::Rejected(format!(
                "pledge list {} bytes exceeds wire cap {MAX_PLEDGE_LIST_WIRE_BYTES}",
                payload.len()
            ));
        }
        let list: PledgeListPayload = match serde_json::from_slice(payload) {
            Ok(l) => l,
            Err(e) => return RecordOutcome::Rejected(format!("pledge list parse failed: {e}")),
        };
        if let Err(e) = storage_signing::verify_pledge_list(&list) {
            return RecordOutcome::Rejected(e);
        }
        if let Err(e) = check_topic(key_expr, "pledges", &list.owner_address) {
            return RecordOutcome::Rejected(e);
        }
        if list.pledges.len() > MAX_PLEDGES_PER_LIST {
            return RecordOutcome::Rejected(format!(
                "pledge list has {} entries, cap {MAX_PLEDGES_PER_LIST}",
                list.pledges.len()
            ));
        }
        let outcome = lww_insert(
            &mut self.pledge_lists,
            list.owner_address,
            PledgeListRecord {
                pledges: list.pledges,
                updated_at: list.updated_at,
            },
            |r| r.updated_at,
        );
        if outcome.changed() {
            evict_stalest(&mut self.pledge_lists, |r| r.updated_at);
            self.save();
        }
        outcome
    }

    /// Verify-first ingest of a `harmony/storage/{owner}/backup-set`
    /// sample. Eligibility is enforced HERE, not just at local flag time
    /// (spec §3, PR #448 review): a signature authenticates the sender,
    /// not policy compliance — a hostile record listing encrypted or
    /// ephemeral CIDs must never induce fetches of never-announced
    /// content classes.
    pub fn on_backup_set_sample(&mut self, key_expr: &str, payload: &[u8]) -> RecordOutcome {
        if payload.len() > MAX_BACKUP_SET_WIRE_BYTES {
            return RecordOutcome::Rejected(format!(
                "backup set {} bytes exceeds wire cap {MAX_BACKUP_SET_WIRE_BYTES}",
                payload.len()
            ));
        }
        let set: BackupSetPayload = match serde_json::from_slice(payload) {
            Ok(s) => s,
            Err(e) => return RecordOutcome::Rejected(format!("backup set parse failed: {e}")),
        };
        if let Err(e) = storage_signing::verify_backup_set(&set) {
            return RecordOutcome::Rejected(e);
        }
        if let Err(e) = check_topic(key_expr, "backup-set", &set.owner_address) {
            return RecordOutcome::Rejected(e);
        }
        if set.entries.len() > MAX_BACKUP_ENTRIES {
            return RecordOutcome::Rejected(format!(
                "backup set has {} entries, cap {MAX_BACKUP_ENTRIES}",
                set.entries.len()
            ));
        }
        if let Err(reason) = validate_backup_entries(&set.entries) {
            return RecordOutcome::Rejected(reason);
        }
        let outcome = lww_insert(
            &mut self.backup_sets,
            set.owner_address,
            BackupSetRecord {
                entries: set.entries,
                updated_at: set.updated_at,
            },
            |r| r.updated_at,
        );
        if outcome.changed() {
            evict_stalest(&mut self.backup_sets, |r| r.updated_at);
            self.save();
        }
        outcome
    }

    /// Verify-first ingest of a `harmony/storage/{owner}/hosting`
    /// sample. `now_ms` stamps receipt for staleness pruning.
    pub fn on_hosting_report_sample(
        &mut self,
        key_expr: &str,
        payload: &[u8],
        now_ms: u64,
    ) -> RecordOutcome {
        if payload.len() > MAX_HOSTING_REPORT_WIRE_BYTES {
            return RecordOutcome::Rejected(format!(
                "hosting report {} bytes exceeds wire cap {MAX_HOSTING_REPORT_WIRE_BYTES}",
                payload.len()
            ));
        }
        let report: HostingReportPayload = match serde_json::from_slice(payload) {
            Ok(r) => r,
            Err(e) => {
                return RecordOutcome::Rejected(format!("hosting report parse failed: {e}"));
            }
        };
        if let Err(e) = storage_signing::verify_hosting_report(&report) {
            return RecordOutcome::Rejected(e);
        }
        if let Err(e) = check_topic(key_expr, "hosting", &report.owner_address) {
            return RecordOutcome::Rejected(e);
        }
        if report.reports.len() > MAX_HOSTING_REPORTS {
            return RecordOutcome::Rejected(format!(
                "hosting report has {} entries, cap {MAX_HOSTING_REPORTS}",
                report.reports.len()
            ));
        }
        let outcome = lww_insert(
            &mut self.hosting_reports,
            report.owner_address,
            HostingReportRecord {
                reports: report.reports,
                updated_at: report.updated_at,
                received_at_ms: now_ms,
            },
            |r| r.updated_at,
        );
        if outcome.changed() {
            evict_stalest(&mut self.hosting_reports, |r| r.updated_at);
            // Hosting reports are in-memory only — no save().
        }
        outcome
    }

    pub fn pledge_list(&self, owner: &str) -> Option<&PledgeListRecord> {
        self.pledge_lists.get(owner)
    }

    pub fn backup_set(&self, owner: &str) -> Option<&BackupSetRecord> {
        self.backup_sets.get(owner)
    }

    pub fn hosting_report(&self, owner: &str) -> Option<&HostingReportRecord> {
        self.hosting_reports.get(owner)
    }

    /// Owners whose pledge list names `me`, with the pledged bytes —
    /// the remote half of pact derivation. Sorted by owner for
    /// deterministic iteration.
    pub fn owners_pledging_to(&self, me: &str) -> Vec<(String, u64)> {
        let mut out: Vec<(String, u64)> = self
            .pledge_lists
            .iter()
            .filter_map(|(owner, r)| {
                r.pledges
                    .iter()
                    .find(|p| p.to == me)
                    .map(|p| (owner.clone(), p.bytes))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Bytes `reporter` claims to hold for `beneficiary`, plus the
    /// report's local receipt clock (for age display).
    pub fn hosting_reported_for(&self, reporter: &str, beneficiary: &str) -> Option<(u64, u64)> {
        let record = self.hosting_reports.get(reporter)?;
        let line = record
            .reports
            .iter()
            .find(|r| r.beneficiary == beneficiary)?;
        Some((line.bytes, record.received_at_ms))
    }

    /// Drop hosting reports older than [`HOSTING_REPORT_STALE_MS`].
    pub fn sweep_hosting(&mut self, now_ms: u64) {
        self.hosting_reports
            .retain(|_, r| now_ms.saturating_sub(r.received_at_ms) < HOSTING_REPORT_STALE_MS);
    }
}

/// Per-entry BackupSet eligibility, shared by wire ingest AND disk
/// reload (PR #449 review, Qodo): a tampered `storage_records.json`
/// must not smuggle in entries the wire path would reject — encrypted/
/// ephemeral classes are never announced, so the planner must never see
/// them regardless of how the record arrived.
fn validate_backup_entries(entries: &[BackupEntry]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for e in entries {
        let raw = hex::decode(&e.cid).map_err(|_| format!("backup set cid not hex: {}", e.cid))?;
        let bytes32: [u8; 32] = raw
            .try_into()
            .map_err(|_| "backup set cid is not 32 bytes".to_string())?;
        let cid = harmony_content::cid::ContentId::from_bytes(bytes32);
        if cid.verify_checksum().is_err() {
            return Err("backup set cid checksum invalid".into());
        }
        if cid.content_class() != harmony_content::cid::ContentClass::PublicDurable {
            return Err("backup set entry is not public durable content".into());
        }
        if !seen.insert(bytes32) {
            return Err("backup set contains duplicate cid".into());
        }
    }
    Ok(())
}

/// Owner-bound topic-shape check: `harmony/storage/{owner}/{kind}`,
/// exactly, with the owner segment equal to the payload's claimed owner
/// (a valid record replayed onto a foreign topic is rejected).
fn check_topic(key_expr: &str, kind: &str, claimed_owner: &str) -> Result<(), String> {
    let rest = key_expr
        .strip_prefix(crate::STORAGE_RECORD_PREFIX)
        .ok_or_else(|| format!("topic {key_expr} is not a storage record topic"))?;
    let mut segments = rest.split('/');
    let owner = segments
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("topic {key_expr} is missing the owner segment"))?;
    let got_kind = segments
        .next()
        .ok_or_else(|| format!("topic {key_expr} is missing the record-kind segment"))?;
    if segments.next().is_some() {
        return Err(format!("topic {key_expr} has trailing segments"));
    }
    if got_kind != kind {
        return Err(format!("topic {key_expr} kind {got_kind} is not {kind}"));
    }
    if owner != claimed_owner {
        return Err(format!(
            "topic owner {owner} does not match payload owner {claimed_owner}"
        ));
    }
    Ok(())
}

/// Whole-record LWW: strictly-greater `updated_at` replaces; equal or
/// older is a no-op (`IgnoredOlder`), so replays cannot churn state.
fn lww_insert<R>(
    map: &mut HashMap<String, R>,
    owner: String,
    record: R,
    updated_at: impl Fn(&R) -> u64,
) -> RecordOutcome {
    match map.get(&owner) {
        Some(existing) if updated_at(existing) >= updated_at(&record) => {
            RecordOutcome::IgnoredOlder
        }
        Some(_) => {
            map.insert(owner, record);
            RecordOutcome::UpdatedNewer
        }
        None => {
            map.insert(owner, record);
            RecordOutcome::Inserted
        }
    }
}

/// Bounded-store overflow: evict the stalest record (min `updated_at`,
/// ties broken by owner) until within [`MAX_TRACKED_OWNERS`].
fn evict_stalest<R>(map: &mut HashMap<String, R>, updated_at: impl Fn(&R) -> u64) {
    while map.len() > MAX_TRACKED_OWNERS {
        let victim = map
            .iter()
            .map(|(owner, r)| (updated_at(r), owner.clone()))
            .min()
            .map(|(_, owner)| owner);
        match victim {
            Some(owner) => {
                map.remove(&owner);
            }
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_content::cid::{ContentFlags, ContentId};

    fn test_identity() -> harmony_identity::PrivateIdentity {
        harmony_identity::PrivateIdentity::generate(&mut rand::rngs::OsRng)
    }

    fn addr_of(private: &harmony_identity::PrivateIdentity) -> String {
        hex::encode(private.public_identity().address_hash)
    }

    fn public_durable_cid_hex(data: &[u8]) -> String {
        let cid = ContentId::for_book(data, ContentFlags::default()).expect("cid");
        hex::encode(cid.to_bytes())
    }

    fn signed_pledge_bytes(
        signer: &harmony_identity::PrivateIdentity,
        pledges: Vec<PledgeEntry>,
        updated_at: u64,
    ) -> (String, Vec<u8>) {
        let mut p = PledgeListPayload {
            owner_address: addr_of(signer),
            pledges,
            updated_at,
            identity_pub: None,
            sig: None,
        };
        storage_signing::sign_pledge_list(signer, &mut p);
        let topic = format!("harmony/storage/{}/pledges", p.owner_address);
        (topic, serde_json::to_vec(&p).unwrap())
    }

    fn signed_backup_bytes(
        signer: &harmony_identity::PrivateIdentity,
        entries: Vec<BackupEntry>,
        updated_at: u64,
    ) -> (String, Vec<u8>) {
        let mut p = BackupSetPayload {
            owner_address: addr_of(signer),
            entries,
            updated_at,
            identity_pub: None,
            sig: None,
        };
        storage_signing::sign_backup_set(signer, &mut p);
        let topic = format!("harmony/storage/{}/backup-set", p.owner_address);
        (topic, serde_json::to_vec(&p).unwrap())
    }

    fn signed_hosting_bytes(
        signer: &harmony_identity::PrivateIdentity,
        reports: Vec<HostingReportEntry>,
        updated_at: u64,
    ) -> (String, Vec<u8>) {
        let mut p = HostingReportPayload {
            owner_address: addr_of(signer),
            reports,
            updated_at,
            identity_pub: None,
            sig: None,
        };
        storage_signing::sign_hosting_report(signer, &mut p);
        let topic = format!("harmony/storage/{}/hosting", p.owner_address);
        (topic, serde_json::to_vec(&p).unwrap())
    }

    fn pledge(to: &str, bytes: u64) -> PledgeEntry {
        PledgeEntry {
            to: to.into(),
            bytes,
        }
    }

    #[test]
    fn signed_records_insert_and_read_back() {
        let mut store = StorageRecordStore::new(None);
        let id = test_identity();
        let owner = addr_of(&id);

        let (topic, bytes) = signed_pledge_bytes(&id, vec![pledge("someone", 5)], 10);
        assert_eq!(
            store.on_pledge_list_sample(&topic, &bytes),
            RecordOutcome::Inserted
        );
        assert_eq!(store.pledge_list(&owner).unwrap().pledges[0].bytes, 5);

        let cid = public_durable_cid_hex(b"blob");
        let (topic, bytes) = signed_backup_bytes(
            &id,
            vec![BackupEntry {
                cid: cid.clone(),
                size: 4,
            }],
            10,
        );
        assert_eq!(
            store.on_backup_set_sample(&topic, &bytes),
            RecordOutcome::Inserted
        );
        assert_eq!(store.backup_set(&owner).unwrap().entries[0].cid, cid);

        let (topic, bytes) = signed_hosting_bytes(
            &id,
            vec![HostingReportEntry {
                beneficiary: "b".into(),
                bytes: 4,
                cids: 1,
            }],
            10,
        );
        assert_eq!(
            store.on_hosting_report_sample(&topic, &bytes, 999),
            RecordOutcome::Inserted
        );
        assert_eq!(store.hosting_report(&owner).unwrap().received_at_ms, 999);
    }

    #[test]
    fn unsigned_and_tampered_records_rejected() {
        let mut store = StorageRecordStore::new(None);
        let id = test_identity();
        let owner = addr_of(&id);

        let unsigned = PledgeListPayload {
            owner_address: owner.clone(),
            pledges: vec![],
            updated_at: 1,
            identity_pub: None,
            sig: None,
        };
        let topic = format!("harmony/storage/{owner}/pledges");
        let outcome = store.on_pledge_list_sample(&topic, &serde_json::to_vec(&unsigned).unwrap());
        assert!(matches!(outcome, RecordOutcome::Rejected(ref e) if e.contains("unsigned")));

        let (topic, bytes) = signed_pledge_bytes(&id, vec![pledge("x", 1)], 2);
        let mut tampered: PledgeListPayload = serde_json::from_slice(&bytes).unwrap();
        tampered.pledges[0].bytes = 999;
        let outcome = store.on_pledge_list_sample(&topic, &serde_json::to_vec(&tampered).unwrap());
        assert!(
            matches!(outcome, RecordOutcome::Rejected(ref e) if e.contains("signature invalid"))
        );
        assert!(
            store.pledge_list(&owner).is_none(),
            "no state effect on rejection"
        );
    }

    #[test]
    fn record_on_foreign_or_misshapen_topic_rejected() {
        let mut store = StorageRecordStore::new(None);
        let id = test_identity();
        let (_, bytes) = signed_pledge_bytes(&id, vec![], 1);

        for bad_topic in [
            "harmony/storage/somebody-else/pledges".to_string(),
            format!("harmony/storage/{}/pledges/extra", addr_of(&id)),
            format!("harmony/storage/{}/backup-set", addr_of(&id)),
            format!("harmony/vines/{}/pledges", addr_of(&id)),
        ] {
            let outcome = store.on_pledge_list_sample(&bad_topic, &bytes);
            assert!(
                matches!(outcome, RecordOutcome::Rejected(_)),
                "topic {bad_topic} should reject"
            );
        }
    }

    #[test]
    fn lww_keeps_newest_ignores_equal_and_older() {
        let mut store = StorageRecordStore::new(None);
        let id = test_identity();
        let owner = addr_of(&id);

        let (topic, v2) = signed_pledge_bytes(&id, vec![pledge("a", 2)], 20);
        assert_eq!(
            store.on_pledge_list_sample(&topic, &v2),
            RecordOutcome::Inserted
        );

        let (_, v1) = signed_pledge_bytes(&id, vec![pledge("a", 1)], 10);
        assert_eq!(
            store.on_pledge_list_sample(&topic, &v1),
            RecordOutcome::IgnoredOlder
        );

        let (_, v2_replay) = signed_pledge_bytes(&id, vec![pledge("a", 9)], 20);
        assert_eq!(
            store.on_pledge_list_sample(&topic, &v2_replay),
            RecordOutcome::IgnoredOlder
        );
        assert_eq!(store.pledge_list(&owner).unwrap().pledges[0].bytes, 2);

        let (_, v3) = signed_pledge_bytes(&id, vec![pledge("a", 3)], 30);
        assert_eq!(
            store.on_pledge_list_sample(&topic, &v3),
            RecordOutcome::UpdatedNewer
        );
        assert_eq!(store.pledge_list(&owner).unwrap().pledges[0].bytes, 3);
    }

    #[test]
    fn oversized_records_rejected() {
        let mut store = StorageRecordStore::new(None);
        let id = test_identity();

        // Entry-count cap.
        let too_many = (0..=MAX_PLEDGES_PER_LIST)
            .map(|i| pledge(&format!("p{i}"), 1))
            .collect();
        let (topic, bytes) = signed_pledge_bytes(&id, too_many, 1);
        assert!(matches!(
            store.on_pledge_list_sample(&topic, &bytes),
            RecordOutcome::Rejected(ref e) if e.contains("cap")
        ));

        // Wire-byte cap, checked before parse (payload is not even JSON).
        let huge = vec![b'x'; MAX_PLEDGE_LIST_WIRE_BYTES + 1];
        assert!(matches!(
            store.on_pledge_list_sample(&topic, &huge),
            RecordOutcome::Rejected(ref e) if e.contains("wire cap")
        ));
    }

    #[test]
    fn backup_set_ineligible_cids_rejected_at_ingest() {
        let mut store = StorageRecordStore::new(None);
        let id = test_identity();
        let owner = addr_of(&id);

        let encrypted = ContentId::for_book(
            b"secret",
            ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .unwrap();
        let ephemeral = ContentId::for_book(
            b"fleeting",
            ContentFlags {
                ephemeral: true,
                ..Default::default()
            },
        )
        .unwrap();
        for bad in [encrypted, ephemeral] {
            let (topic, bytes) = signed_backup_bytes(
                &id,
                vec![BackupEntry {
                    cid: hex::encode(bad.to_bytes()),
                    size: 1,
                }],
                1,
            );
            let outcome = store.on_backup_set_sample(&topic, &bytes);
            assert!(
                matches!(outcome, RecordOutcome::Rejected(ref e) if e.contains("public durable")),
                "class {:?} must reject",
                bad.content_class()
            );
        }
        assert!(store.backup_set(&owner).is_none(), "no state effect");
    }

    #[test]
    fn backup_set_malformed_or_bad_checksum_or_duplicate_cid_rejected() {
        let mut store = StorageRecordStore::new(None);
        let id = test_identity();

        let (topic, bytes) = signed_backup_bytes(
            &id,
            vec![BackupEntry {
                cid: "zz".into(),
                size: 1,
            }],
            1,
        );
        assert!(matches!(
            store.on_backup_set_sample(&topic, &bytes),
            RecordOutcome::Rejected(ref e) if e.contains("not hex")
        ));

        let (_, bytes) = signed_backup_bytes(
            &id,
            vec![BackupEntry {
                cid: "abcd".into(),
                size: 1,
            }],
            1,
        );
        assert!(matches!(
            store.on_backup_set_sample(&topic, &bytes),
            RecordOutcome::Rejected(ref e) if e.contains("32 bytes")
        ));

        let mut corrupt = ContentId::for_book(b"ok", ContentFlags::default())
            .unwrap()
            .to_bytes();
        corrupt[3] ^= 0x01; // flip a checksum bit
        let (_, bytes) = signed_backup_bytes(
            &id,
            vec![BackupEntry {
                cid: hex::encode(corrupt),
                size: 1,
            }],
            1,
        );
        assert!(matches!(
            store.on_backup_set_sample(&topic, &bytes),
            RecordOutcome::Rejected(ref e) if e.contains("checksum")
        ));

        let good = public_durable_cid_hex(b"dup");
        let (_, bytes) = signed_backup_bytes(
            &id,
            vec![
                BackupEntry {
                    cid: good.clone(),
                    size: 1,
                },
                BackupEntry { cid: good, size: 1 },
            ],
            1,
        );
        assert!(matches!(
            store.on_backup_set_sample(&topic, &bytes),
            RecordOutcome::Rejected(ref e) if e.contains("duplicate")
        ));
    }

    #[test]
    fn pledges_and_backup_sets_survive_disk_reload_hosting_does_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("storage_records.json");
        let id = test_identity();
        let owner = addr_of(&id);

        {
            let mut store = StorageRecordStore::new(Some(path.clone()));
            let (topic, bytes) = signed_pledge_bytes(&id, vec![pledge("a", 7)], 5);
            assert!(store.on_pledge_list_sample(&topic, &bytes).changed());
            let cid = public_durable_cid_hex(b"keep");
            let (topic, bytes) = signed_backup_bytes(&id, vec![BackupEntry { cid, size: 4 }], 5);
            assert!(store.on_backup_set_sample(&topic, &bytes).changed());
            let (topic, bytes) = signed_hosting_bytes(
                &id,
                vec![HostingReportEntry {
                    beneficiary: "b".into(),
                    bytes: 4,
                    cids: 1,
                }],
                5,
            );
            assert!(store
                .on_hosting_report_sample(&topic, &bytes, 100)
                .changed());
        }

        let reloaded = StorageRecordStore::new(Some(path));
        assert_eq!(reloaded.pledge_list(&owner).unwrap().pledges[0].bytes, 7);
        assert_eq!(reloaded.backup_set(&owner).unwrap().entries.len(), 1);
        assert!(
            reloaded.hosting_report(&owner).is_none(),
            "hosting reports are freshness signals and must not persist"
        );
    }

    #[test]
    fn corrupt_or_foreign_version_disk_file_yields_empty_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("storage_records.json");

        std::fs::write(&path, b"{not json").unwrap();
        let store = StorageRecordStore::new(Some(path.clone()));
        assert!(store.pledge_lists.is_empty());

        std::fs::write(&path, br#"{"version":99,"pledgeLists":[],"backupSets":[]}"#).unwrap();
        let store = StorageRecordStore::new(Some(path));
        assert!(store.pledge_lists.is_empty());
    }

    /// PR #449 review (Qodo): the disk file is not a trusted channel —
    /// reload re-runs the same eligibility validation as wire ingest.
    #[test]
    fn tampered_disk_backup_set_with_ineligible_cid_is_dropped_on_reload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("storage_records.json");
        let encrypted = ContentId::for_book(
            b"secret",
            ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .unwrap();
        let tampered = format!(
            r#"{{"version":1,"pledgeLists":[],"backupSets":[
                {{"owner":"mallory","entries":[{{"cid":"{}","size":1}}],"updatedAt":9}}
            ]}}"#,
            hex::encode(encrypted.to_bytes()),
        );
        std::fs::write(&path, tampered).unwrap();
        let store = StorageRecordStore::new(Some(path));
        assert!(
            store.backup_set("mallory").is_none(),
            "ineligible entries must not survive the reload path"
        );
    }

    #[test]
    fn owner_cap_evicts_stalest() {
        let mut store = StorageRecordStore::new(None);
        // Directly exercise the bounded-map helper: driving 1025 signed
        // ingests would mostly re-test signing. Ingest→evict wiring is
        // covered by the changed()-then-evict call sites above.
        for i in 0..=MAX_TRACKED_OWNERS {
            store.pledge_lists.insert(
                format!("owner-{i:04}"),
                PledgeListRecord {
                    pledges: vec![],
                    updated_at: i as u64 + 1,
                },
            );
        }
        evict_stalest(&mut store.pledge_lists, |r| r.updated_at);
        assert_eq!(store.pledge_lists.len(), MAX_TRACKED_OWNERS);
        assert!(
            store.pledge_list("owner-0000").is_none(),
            "stalest (min updated_at) evicted first"
        );
        assert!(store
            .pledge_list(&format!("owner-{MAX_TRACKED_OWNERS:04}"))
            .is_some());
    }

    #[test]
    fn hosting_sweep_drops_stale_reports() {
        let mut store = StorageRecordStore::new(None);
        let id = test_identity();
        let owner = addr_of(&id);
        let (topic, bytes) = signed_hosting_bytes(&id, vec![], 1);
        assert!(store
            .on_hosting_report_sample(&topic, &bytes, 1_000)
            .changed());

        store.sweep_hosting(1_000 + HOSTING_REPORT_STALE_MS - 1);
        assert!(store.hosting_report(&owner).is_some(), "fresh report kept");

        store.sweep_hosting(1_000 + HOSTING_REPORT_STALE_MS);
        assert!(
            store.hosting_report(&owner).is_none(),
            "stale report dropped"
        );
    }

    #[test]
    fn owners_pledging_to_filters_by_beneficiary() {
        let mut store = StorageRecordStore::new(None);
        let alice = test_identity();
        let bob = test_identity();
        let me = "me-address";

        let (topic, bytes) = signed_pledge_bytes(&alice, vec![pledge(me, 100)], 1);
        assert!(store.on_pledge_list_sample(&topic, &bytes).changed());
        let (topic, bytes) = signed_pledge_bytes(&bob, vec![pledge("other", 50)], 1);
        assert!(store.on_pledge_list_sample(&topic, &bytes).changed());

        let pledgers = store.owners_pledging_to(me);
        assert_eq!(pledgers, vec![(addr_of(&alice), 100)]);

        assert_eq!(
            store.hosting_reported_for(&addr_of(&alice), me),
            None,
            "no hosting report yet"
        );
    }
}
