//! ZEB-669 slice 2: bounded LWW store for REMOTE storage-buddy records.
//!
//! Mirrors the follow-list half of `vine_feed_cache`: strict verify-first
//! ingest (byte cap → parse → signature → pubkey→address binding → topic
//! shape → caps → eligibility) before any state effect, whole-record LWW
//! replace by `updated_at` (strictly-greater wins), bounded owner maps
//! with newest-received-first overflow eviction (ZEB-851 flood-proofing: a
//! flood of throwaway rows evicts itself, never established honest rows —
//! see `evict_overflow`).
//!
//! Pledge lists and backup sets persist to `storage_records.json`
//! (verify-once-at-ingest — signatures are never written to disk, the
//! `TombstoneOnDisk` posture). Hosting reports are deliberately
//! in-memory only: they are staleness-pruned freshness signals, and a
//! stale report surviving a restart would claim liveness we cannot show.
//!
//! ZEB-679: ingest is additionally revocation-aware. A record carrying
//! `#2` v2 material self-anchors (enrollment + `#3` binding
//! countersignature), is checked against [`RevokedDeviceProjection`],
//! and pins its `(owner_id, device key)` binding first-write-wins per
//! owner address ([`StorageSignerPin`], persisted). Pinned owners'
//! legacy records are rejected (anti-downgrade ratchet).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::owner_state_types::OwnerAddr;
use crate::revoked_device_projection::RevokedDeviceProjection;
use crate::storage_signing::{
    self, BackupEntry, BackupSetPayload, HostingReportEntry, HostingReportPayload, PledgeEntry,
    PledgeListPayload, StorageSignerV2, VerifiedStorageSigner,
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
/// Bounded-store cap per record family; beyond it the NEWEST-received owner
/// is evicted first, so a flood of throwaway rows evicts itself rather than
/// established honest rows (ZEB-851 flood-proofing — see `evict_overflow`).
pub const MAX_TRACKED_OWNERS: usize = 1024;
/// Cap on first-write-wins signer pins (ZEB-679). Strictly above the
/// worst-case live-owner union (3 families × [`MAX_TRACKED_OWNERS`]), so
/// a pin backing a live record is never forced out — only pins whose
/// owner has no current record compete for eviction.
pub const MAX_SIGNER_PINS: usize = 4096;
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
    /// Local receipt clock (ms) — no trust meaning (mirrors
    /// `HostingReportRecord::received_at_ms` and
    /// `StorageSignerPin::pinned_at_ms`). Never the peer `updated_at`.
    /// Eviction ordering is `seq`, not this (ZEB-863).
    pub received_at_ms: u64,
    /// Local monotonic insert sequence (ZEB-863) — the SOLE eviction
    /// ordering key. Peer-independent and immune to wall-clock rollback,
    /// unlike `received_at_ms`. Local, never serialized.
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupSetRecord {
    pub entries: Vec<BackupEntry>,
    pub updated_at: u64,
    /// Local receipt clock (ms) — no trust meaning (mirrors
    /// `HostingReportRecord::received_at_ms` and
    /// `StorageSignerPin::pinned_at_ms`). Never the peer `updated_at`.
    /// Eviction ordering is `seq`, not this (ZEB-863).
    pub received_at_ms: u64,
    /// Local monotonic insert sequence (ZEB-863) — the SOLE eviction
    /// ordering key. Peer-independent and immune to wall-clock rollback,
    /// unlike `received_at_ms`. Local, never serialized.
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostingReportRecord {
    pub reports: Vec<HostingReportEntry>,
    pub updated_at: u64,
    /// Local receipt clock (ms) — drives staleness pruning and the
    /// "report age" surfaced to the UI. NOT the eviction key (that is
    /// `seq`, ZEB-863) — this stays dual-purpose for staleness/UI.
    pub received_at_ms: u64,
    /// Local monotonic insert sequence (ZEB-863) — the SOLE eviction
    /// ordering key. Peer-independent and immune to wall-clock rollback,
    /// unlike `received_at_ms`. Local, never serialized.
    pub seq: u64,
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
struct SignerPinOnDisk {
    owner: String,
    /// 16-byte master owner id, hex (32 chars).
    owner_id: String,
    /// 32-byte enrolled `#2` verify key, hex (64 chars).
    device_ed25519: String,
    pinned_at_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StorageRecordsDiskV1 {
    version: u32,
    #[serde(default)]
    pledge_lists: Vec<PledgeListOnDisk>,
    #[serde(default)]
    backup_sets: Vec<BackupSetOnDisk>,
    /// ZEB-679, additive (old files load with no pins; version unbumped).
    #[serde(default)]
    signer_pins: Vec<SignerPinOnDisk>,
}

/// ZEB-679: the first-verified `(owner_id, #2 key)` binding for an
/// owner address — first-write-wins, exactly the vine authority-cache
/// posture. PERSISTED (unlike the vine cache) because storage has no
/// wire-durable revoked-authority record to re-arm revocation after a
/// restart: a revoked device holds its `#3` address key forever and
/// would otherwise publish accepted legacy records after every receiver
/// restart. A pinned binding can never legitimately change — same-device
/// `#2` rotation is unrepresentable (`device_id == hash(device_pubkeys)`,
/// ZEB-683 finding), so a mismatch is always a rebind attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageSignerPin {
    pub owner_id: [u8; 16],
    pub device_ed25519: [u8; 32],
    /// Local pin clock (ms) — no trust meaning; round-trips to disk.
    /// Eviction ordering is `seq`, not this (ZEB-863).
    pub pinned_at_ms: u64,
    /// Local monotonic insert sequence (ZEB-863) — the SOLE eviction
    /// ordering key within a liveness class. Peer-independent and immune
    /// to wall-clock rollback. Local, never serialized.
    pub seq: u64,
}

/// Bounded store of verified remote records, keyed by owner address.
#[derive(Debug)]
pub struct StorageRecordStore {
    pledge_lists: HashMap<String, PledgeListRecord>,
    backup_sets: HashMap<String, BackupSetRecord>,
    hosting_reports: HashMap<String, HostingReportRecord>,
    signer_pins: HashMap<String, StorageSignerPin>,
    path: Option<PathBuf>,
    /// Strictly-increasing local counter stamped onto every inserted record
    /// as its `seq` (ZEB-863). One store-wide sequence across all record
    /// families; single-threaded `&mut self` access, so no atomics. Not
    /// persisted — re-derived on load in disk order.
    insert_seq: u64,
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
            signer_pins: HashMap::new(),
            path,
            insert_seq: 0,
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
            // Reloaded rows are "most-established": they get the lowest `seq`
            // values (stamped here in disk order, before any live ingest), so
            // a post-restart flood is the newest (highest seq) and self-evicts
            // first. `received_at_ms: 0` is kept for its receipt-clock meaning.
            let seq = store.next_insert_seq();
            store.pledge_lists.insert(
                row.owner,
                PledgeListRecord {
                    pledges: row.pledges,
                    updated_at: row.updated_at,
                    received_at_ms: 0,
                    seq,
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
            // Reloaded rows are "most-established" — lowest `seq` (disk order),
            // so a post-restart flood self-evicts first (see pledge reload).
            let seq = store.next_insert_seq();
            store.backup_sets.insert(
                row.owner,
                BackupSetRecord {
                    entries: row.entries,
                    updated_at: row.updated_at,
                    received_at_ms: 0,
                    seq,
                },
            );
        }
        for row in disk.signer_pins {
            let mut owner_id = [0u8; 16];
            let mut device = [0u8; 32];
            if row.owner_id.len() != 32
                || row.device_ed25519.len() != 64
                || hex::decode_to_slice(&row.owner_id, &mut owner_id).is_err()
                || hex::decode_to_slice(&row.device_ed25519, &mut device).is_err()
            {
                tracing::warn!(owner = %row.owner, "storage_records reload: dropping malformed signer pin");
                continue;
            }
            let seq = store.next_insert_seq();
            store.signer_pins.insert(
                row.owner,
                StorageSignerPin {
                    owner_id,
                    device_ed25519: device,
                    pinned_at_ms: row.pinned_at_ms,
                    seq,
                },
            );
        }
        evict_overflow(&mut store.pledge_lists, |r| r.seq);
        evict_overflow(&mut store.backup_sets, |r| r.seq);
        evict_pins(
            &mut store.signer_pins,
            &store.pledge_lists,
            &store.backup_sets,
            &store.hosting_reports,
        );
        store
    }

    /// Hand out the next local insert sequence and advance the counter.
    /// Strictly increasing ⇒ every live record's `seq` is unique, which is
    /// what lets eviction key on `seq` alone (no peer-controlled tiebreak).
    fn next_insert_seq(&mut self) -> u64 {
        let s = self.insert_seq;
        self.insert_seq += 1;
        s
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
        let mut signer_pins: Vec<SignerPinOnDisk> = self
            .signer_pins
            .iter()
            .map(|(owner, pin)| SignerPinOnDisk {
                owner: owner.clone(),
                owner_id: hex::encode(pin.owner_id),
                device_ed25519: hex::encode(pin.device_ed25519),
                pinned_at_ms: pin.pinned_at_ms,
            })
            .collect();
        signer_pins.sort_by(|a, b| a.owner.cmp(&b.owner));
        let disk = StorageRecordsDiskV1 {
            version: RECORDS_FILE_VERSION,
            pledge_lists,
            backup_sets,
            signer_pins,
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

    /// ZEB-679 dual-path admission, shared by all three families. Runs
    /// AFTER the legacy `#3` verify + topic + caps (so its errors never
    /// mask a plain signature failure) and BEFORE any state effect:
    ///
    /// * v2 material present → it must fully verify (present-but-invalid
    ///   is reject, never fallback), the signer must not be revoked
    ///   ([`RevokedDeviceProjection`], the same store the DM/friend/PEX
    ///   cutoffs consult), and the binding must match the pin
    ///   (first-write-wins; mismatch = rebind attempt).
    /// * v2 absent → rejected iff this address has a pin (anti-downgrade
    ///   ratchet: a migrated owner's legacy records are no longer
    ///   trusted); unpinned legacy records keep working (bootstrap).
    ///
    /// Returns whether a new pin was written (callers persist + bound it
    /// after the record itself lands, so a fresh pin counts as live for
    /// eviction).
    fn v2_admission(
        &mut self,
        owner_address: &str,
        v2: &StorageSignerV2,
        revoked: &RevokedDeviceProjection,
        now_ms: u64,
        verify: impl FnOnce() -> Result<VerifiedStorageSigner, String>,
    ) -> Result<bool, String> {
        if !v2.is_present() {
            if self.signer_pins.contains_key(owner_address) {
                return Err(format!(
                    "legacy record from migrated owner {owner_address} rejected (signer pin present)"
                ));
            }
            return Ok(false);
        }
        let signer = verify()?;
        if revoked.is_revoked(&OwnerAddr(signer.owner_id), &signer.device_ed25519) {
            return Err(format!(
                "record signer device revoked for owner {}",
                hex::encode(signer.owner_id)
            ));
        }
        match self.signer_pins.get(owner_address) {
            None => {
                let seq = self.next_insert_seq();
                self.signer_pins.insert(
                    owner_address.to_string(),
                    StorageSignerPin {
                        owner_id: signer.owner_id,
                        device_ed25519: signer.device_ed25519,
                        pinned_at_ms: now_ms,
                        seq,
                    },
                );
                Ok(true)
            }
            Some(pin)
                if pin.owner_id == signer.owner_id
                    && pin.device_ed25519 == signer.device_ed25519 =>
            {
                Ok(false)
            }
            Some(_) => Err(format!(
                "v2 binding for {owner_address} does not match pinned signer (rebind rejected)"
            )),
        }
    }

    /// Verify-first ingest of a `harmony/storage/{owner}/pledges` sample.
    /// `now_ms` is the verifier-controlled ingest clock (cert expiry +
    /// pin stamping); `revoked` is the shared revocation projection.
    pub fn on_pledge_list_sample(
        &mut self,
        key_expr: &str,
        payload: &[u8],
        revoked: &RevokedDeviceProjection,
        now_ms: u64,
    ) -> RecordOutcome {
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
        let pin_changed =
            match self.v2_admission(&list.owner_address, &list.v2, revoked, now_ms, || {
                storage_signing::verify_pledge_list_v2(&list, now_ms / 1000)
            }) {
                Ok(changed) => changed,
                Err(e) => return RecordOutcome::Rejected(e),
            };
        let seq = self.next_insert_seq();
        let outcome = lww_insert(
            &mut self.pledge_lists,
            list.owner_address,
            PledgeListRecord {
                pledges: list.pledges,
                updated_at: list.updated_at,
                received_at_ms: now_ms,
                seq,
            },
            |r| r.updated_at,
        );
        if outcome.changed() {
            evict_overflow(&mut self.pledge_lists, |r| r.seq);
        }
        if pin_changed {
            evict_pins(
                &mut self.signer_pins,
                &self.pledge_lists,
                &self.backup_sets,
                &self.hosting_reports,
            );
        }
        if outcome.changed() || pin_changed {
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
    pub fn on_backup_set_sample(
        &mut self,
        key_expr: &str,
        payload: &[u8],
        revoked: &RevokedDeviceProjection,
        now_ms: u64,
    ) -> RecordOutcome {
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
        let pin_changed =
            match self.v2_admission(&set.owner_address, &set.v2, revoked, now_ms, || {
                storage_signing::verify_backup_set_v2(&set, now_ms / 1000)
            }) {
                Ok(changed) => changed,
                Err(e) => return RecordOutcome::Rejected(e),
            };
        let seq = self.next_insert_seq();
        let outcome = lww_insert(
            &mut self.backup_sets,
            set.owner_address,
            BackupSetRecord {
                entries: set.entries,
                updated_at: set.updated_at,
                received_at_ms: now_ms,
                seq,
            },
            |r| r.updated_at,
        );
        if outcome.changed() {
            evict_overflow(&mut self.backup_sets, |r| r.seq);
        }
        if pin_changed {
            evict_pins(
                &mut self.signer_pins,
                &self.pledge_lists,
                &self.backup_sets,
                &self.hosting_reports,
            );
        }
        if outcome.changed() || pin_changed {
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
        revoked: &RevokedDeviceProjection,
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
        let pin_changed =
            match self.v2_admission(&report.owner_address, &report.v2, revoked, now_ms, || {
                storage_signing::verify_hosting_report_v2(&report, now_ms / 1000)
            }) {
                Ok(changed) => changed,
                Err(e) => return RecordOutcome::Rejected(e),
            };
        let seq = self.next_insert_seq();
        let outcome = lww_insert(
            &mut self.hosting_reports,
            report.owner_address,
            HostingReportRecord {
                reports: report.reports,
                updated_at: report.updated_at,
                received_at_ms: now_ms,
                seq,
            },
            |r| r.updated_at,
        );
        if outcome.changed() {
            evict_overflow(&mut self.hosting_reports, |r| r.seq);
            // Hosting reports are in-memory only — no save() for them…
        }
        if pin_changed {
            evict_pins(
                &mut self.signer_pins,
                &self.pledge_lists,
                &self.backup_sets,
                &self.hosting_reports,
            );
            // …but a pin minted from one IS durable revocation state.
            self.save();
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

    /// The first-write-wins signer pin for `owner`, if it has migrated.
    pub fn signer_pin(&self, owner: &str) -> Option<&StorageSignerPin> {
        self.signer_pins.get(owner)
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

    /// ZEB-679 R1 (Qodo): revocation is enforced RETROACTIVELY too.
    /// Records admitted BEFORE the projection learned their signer's
    /// revocation would otherwise keep driving buddy pin/fetch planning
    /// indefinitely (the revoked device has no reason to republish).
    /// Called from the auto-pin engine tick; drops the revoked owners'
    /// records across all three families while KEEPING their pins — the
    /// pin is the sticky ratchet that also blocks the legacy downgrade.
    /// Returns true when anything was dropped (caller re-plans/emits).
    pub fn purge_revoked(&mut self, revoked: &RevokedDeviceProjection) -> bool {
        let dead: Vec<String> = self
            .signer_pins
            .iter()
            .filter(|(_, pin)| revoked.is_revoked(&OwnerAddr(pin.owner_id), &pin.device_ed25519))
            .map(|(owner, _)| owner.clone())
            .collect();
        let mut changed = false;
        for owner in &dead {
            changed |= self.pledge_lists.remove(owner).is_some();
            changed |= self.backup_sets.remove(owner).is_some();
            changed |= self.hosting_reports.remove(owner).is_some();
        }
        if changed {
            self.save();
        }
        changed
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

/// Pin-cap overflow: evict pins whose owner has NO live record first
/// (dead weight), and among those the NEWEST (highest local `seq`) first
/// (ZEB-863 — peer-independent, was the `pinned_at_ms`+owner tiebreak).
/// Newest-first makes a pin flood self-limiting (R1,
/// CodeRabbit): an attacker minting throwaway addresses evicts their
/// OWN just-minted pins, while a long-established migrated address's
/// ratchet pin is never the newest and survives. Legit fresh pins are
/// live-backed at mint time (created during record admission), so they
/// don't compete in the dead pool. Because [`MAX_SIGNER_PINS`] strictly
/// exceeds the worst-case live-owner union (3 × [`MAX_TRACKED_OWNERS`]),
/// a pin backing a live record is never forced out — evicting one would
/// re-open the legacy-downgrade window for that owner.
fn evict_pins(
    pins: &mut HashMap<String, StorageSignerPin>,
    pledges: &HashMap<String, PledgeListRecord>,
    backups: &HashMap<String, BackupSetRecord>,
    hosting: &HashMap<String, HostingReportRecord>,
) {
    while pins.len() > MAX_SIGNER_PINS {
        // Evict dead-weight (no live record) first, then the newest (highest
        // `seq`) within a liveness class (ZEB-863). `!live` makes dead pins
        // compare greater so `max_by_key` picks them first; `seq` is a unique
        // local counter, so the max is deterministic with no peer-controlled
        // `owner` tiebreak and no wall-clock dependence.
        let victim = pins
            .iter()
            .max_by_key(|&(owner, pin)| {
                let live = pledges.contains_key(owner)
                    || backups.contains_key(owner)
                    || hosting.contains_key(owner);
                (!live, pin.seq)
            })
            .map(|(owner, _)| owner.clone());
        match victim {
            Some(owner) => {
                pins.remove(&owner);
            }
            None => break,
        }
    }
}

/// Bounded-store overflow: evict the record with the NEWEST local insert
/// sequence (highest `seq`) first until within [`MAX_TRACKED_OWNERS`].
/// Newest-first makes a flood self-limiting,
/// mirroring [`evict_pins`]: an attacker minting throwaway addresses and
/// publishing far-future `updated_at` evicts their OWN just-received rows,
/// while an established honest owner's row (earlier `seq`) is never the
/// newest and survives. Keying on the LOCAL insert sequence — never the
/// peer-supplied `updated_at`, and (ZEB-863) never the peer `owner` address
/// nor the non-monotonic wall clock — is what closes the ZEB-851 flood-evict
/// vector: a peer stamp can no longer choose the eviction victim, and a
/// same-millisecond collision or a local-clock rollback cannot invert the order.
///
/// Consequence: once the map is at [`MAX_TRACKED_OWNERS`] with established
/// rows, a genuinely NEW honest owner has the newest `seq` and
/// evicts itself — it is NOT admitted until a slot frees (an LWW-replace of
/// an existing owner, or a revocation purge). The established working set is
/// frozen. This is inherent to flood resistance: admitting newcomers over
/// established rows would re-open the flood this fn exists to close.
/// [`MAX_TRACKED_OWNERS`] (1024) is set well above realistic honest
/// storage-buddy counts, so honest saturation is not expected; if it ever
/// becomes real, the mitigation is a higher cap or a liveness distinction.
fn evict_overflow<R>(map: &mut HashMap<String, R>, seq: impl Fn(&R) -> u64) {
    while map.len() > MAX_TRACKED_OWNERS {
        // Evict the newest (highest `seq`) first. `seq` is a strictly-
        // increasing local counter (ZEB-863): unique by construction, so the
        // max is a single deterministic record — no peer-controlled `owner`
        // tiebreak in the decision, and immune to wall-clock rollback (unlike
        // the former `received_at_ms` key).
        let victim = map
            .iter()
            .max_by_key(|&(_, r)| seq(r))
            .map(|(owner, _)| owner.clone());
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

    /// Empty revocation projection — the default for tests that don't
    /// exercise the ZEB-679 revocation path.
    fn rvk() -> RevokedDeviceProjection {
        RevokedDeviceProjection::new()
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
            v2: Default::default(),
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
            v2: Default::default(),
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
            v2: Default::default(),
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
            store.on_pledge_list_sample(&topic, &bytes, &rvk(), 9_000),
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
            store.on_backup_set_sample(&topic, &bytes, &rvk(), 9_000),
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
            store.on_hosting_report_sample(&topic, &bytes, &rvk(), 999),
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
            v2: Default::default(),
        };
        let topic = format!("harmony/storage/{owner}/pledges");
        let outcome = store.on_pledge_list_sample(
            &topic,
            &serde_json::to_vec(&unsigned).unwrap(),
            &rvk(),
            9_000,
        );
        assert!(matches!(outcome, RecordOutcome::Rejected(ref e) if e.contains("unsigned")));

        let (topic, bytes) = signed_pledge_bytes(&id, vec![pledge("x", 1)], 2);
        let mut tampered: PledgeListPayload = serde_json::from_slice(&bytes).unwrap();
        tampered.pledges[0].bytes = 999;
        let outcome = store.on_pledge_list_sample(
            &topic,
            &serde_json::to_vec(&tampered).unwrap(),
            &rvk(),
            9_000,
        );
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
            let outcome = store.on_pledge_list_sample(&bad_topic, &bytes, &rvk(), 9_000);
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
            store.on_pledge_list_sample(&topic, &v2, &rvk(), 9_000),
            RecordOutcome::Inserted
        );

        let (_, v1) = signed_pledge_bytes(&id, vec![pledge("a", 1)], 10);
        assert_eq!(
            store.on_pledge_list_sample(&topic, &v1, &rvk(), 9_000),
            RecordOutcome::IgnoredOlder
        );

        let (_, v2_replay) = signed_pledge_bytes(&id, vec![pledge("a", 9)], 20);
        assert_eq!(
            store.on_pledge_list_sample(&topic, &v2_replay, &rvk(), 9_000),
            RecordOutcome::IgnoredOlder
        );
        assert_eq!(store.pledge_list(&owner).unwrap().pledges[0].bytes, 2);

        let (_, v3) = signed_pledge_bytes(&id, vec![pledge("a", 3)], 30);
        assert_eq!(
            store.on_pledge_list_sample(&topic, &v3, &rvk(), 9_000),
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
            store.on_pledge_list_sample(&topic, &bytes, &rvk(), 9_000),
            RecordOutcome::Rejected(ref e) if e.contains("cap")
        ));

        // Wire-byte cap, checked before parse (payload is not even JSON).
        let huge = vec![b'x'; MAX_PLEDGE_LIST_WIRE_BYTES + 1];
        assert!(matches!(
            store.on_pledge_list_sample(&topic, &huge, &rvk(), 9_000),
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
            let outcome = store.on_backup_set_sample(&topic, &bytes, &rvk(), 9_000);
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
            store.on_backup_set_sample(&topic, &bytes, &rvk(), 9_000),
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
            store.on_backup_set_sample(&topic, &bytes, &rvk(), 9_000),
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
            store.on_backup_set_sample(&topic, &bytes, &rvk(), 9_000),
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
            store.on_backup_set_sample(&topic, &bytes, &rvk(), 9_000),
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
            assert!(store
                .on_pledge_list_sample(&topic, &bytes, &rvk(), 9_000)
                .changed());
            let cid = public_durable_cid_hex(b"keep");
            let (topic, bytes) = signed_backup_bytes(&id, vec![BackupEntry { cid, size: 4 }], 5);
            assert!(store
                .on_backup_set_sample(&topic, &bytes, &rvk(), 9_000)
                .changed());
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
                .on_hosting_report_sample(&topic, &bytes, &rvk(), 100)
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
    fn owner_cap_evicts_overflow_newest_received_first() {
        let mut store = StorageRecordStore::new(None);
        // Directly exercise the bounded-map helper: driving 1025 signed
        // ingests would mostly re-test signing. Ingest→evict wiring is
        // covered by the changed()-then-evict call sites above.
        for i in 0..=MAX_TRACKED_OWNERS {
            store.pledge_lists.insert(
                format!("owner-{i:04}"),
                PledgeListRecord {
                    pledges: vec![],
                    updated_at: 1,
                    received_at_ms: i as u64 + 1,
                    seq: i as u64,
                },
            );
        }
        evict_overflow(&mut store.pledge_lists, |r| r.seq);
        assert_eq!(store.pledge_lists.len(), MAX_TRACKED_OWNERS);
        assert!(
            store
                .pledge_list(&format!("owner-{MAX_TRACKED_OWNERS:04}"))
                .is_none(),
            "newest (max seq) evicted first"
        );
        assert!(store.pledge_list("owner-0000").is_some());
    }

    #[test]
    fn evict_overflow_same_ms_is_peer_independent() {
        // ZEB-863 #3: a same-wall-ms flood must not let a peer-chosen owner
        // address decide the eviction victim. The honest row is received
        // FIRST but is given the SMALLEST owner address — precisely the
        // victim under the old `min(owner)` same-ms tiebreak. It must
        // survive: eviction is ordered by local receipt sequence, not the
        // peer address.
        let mut store = StorageRecordStore::new(None);
        let same_ms = 5_000u64;
        // Honest, received first (lowest seq), smallest owner address.
        store.pledge_lists.insert(
            "owner-0000".into(),
            PledgeListRecord {
                pledges: vec![],
                updated_at: 1,
                received_at_ms: same_ms,
                seq: 0,
            },
        );
        // Flood: larger owner addresses, all in the SAME wall ms (so the old
        // key would tiebreak on owner), received AFTER the honest row so they
        // carry higher seq. Over cap by one.
        for i in 1..=MAX_TRACKED_OWNERS {
            store.pledge_lists.insert(
                format!("owner-{i:04}"),
                PledgeListRecord {
                    pledges: vec![],
                    updated_at: 1,
                    received_at_ms: same_ms,
                    seq: i as u64,
                },
            );
        }
        evict_overflow(&mut store.pledge_lists, |r| r.seq);
        assert_eq!(store.pledge_lists.len(), MAX_TRACKED_OWNERS);
        assert!(
            store.pledge_list("owner-0000").is_some(),
            "honest first-received row must survive a same-ms flood regardless of its (smallest) address"
        );
    }

    #[test]
    fn hosting_sweep_drops_stale_reports() {
        let mut store = StorageRecordStore::new(None);
        let id = test_identity();
        let owner = addr_of(&id);
        let (topic, bytes) = signed_hosting_bytes(&id, vec![], 1);
        assert!(store
            .on_hosting_report_sample(&topic, &bytes, &rvk(), 1_000)
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
        assert!(store
            .on_pledge_list_sample(&topic, &bytes, &rvk(), 9_000)
            .changed());
        let (topic, bytes) = signed_pledge_bytes(&bob, vec![pledge("other", 50)], 1);
        assert!(store
            .on_pledge_list_sample(&topic, &bytes, &rvk(), 9_000)
            .changed());

        let pledgers = store.owners_pledging_to(me);
        assert_eq!(pledgers, vec![(addr_of(&alice), 100)]);

        assert_eq!(
            store.hosting_reported_for(&addr_of(&alice), me),
            None,
            "no hosting report yet"
        );
    }

    // ── ZEB-679: v2 admission (revocation + signer pin + ratchet) ────

    use crate::enrollment_verify::quorum_fixtures::{mint_quorum_world, QuorumWorld, WORLD_NOW};
    use crate::storage_signing::StorageSignerMaterial;

    /// Ingest clock (ms) at which every fixture world's certs are valid.
    const NOW_MS: u64 = WORLD_NOW * 1000;

    fn master_material(world: &QuorumWorld) -> StorageSignerMaterial {
        StorageSignerMaterial {
            sk: std::sync::Arc::new(world.a_sk.clone()),
            cert: world.a_cert.clone(),
            signer_certs: Vec::new(),
        }
    }

    fn dual_signed_pledge_bytes(
        signer: &harmony_identity::PrivateIdentity,
        material: &StorageSignerMaterial,
        pledges: Vec<PledgeEntry>,
        updated_at: u64,
    ) -> (String, Vec<u8>) {
        let mut p = PledgeListPayload {
            owner_address: addr_of(signer),
            pledges,
            updated_at,
            identity_pub: None,
            sig: None,
            v2: Default::default(),
        };
        storage_signing::sign_pledge_list_v2(signer, material, &mut p).expect("dual-sign");
        let topic = format!("harmony/storage/{}/pledges", p.owner_address);
        (topic, serde_json::to_vec(&p).unwrap())
    }

    fn dual_signed_hosting_bytes(
        signer: &harmony_identity::PrivateIdentity,
        material: &StorageSignerMaterial,
        updated_at: u64,
    ) -> (String, Vec<u8>) {
        let mut p = HostingReportPayload {
            owner_address: addr_of(signer),
            reports: vec![HostingReportEntry {
                beneficiary: "b".into(),
                bytes: 1,
                cids: 1,
            }],
            updated_at,
            identity_pub: None,
            sig: None,
            v2: Default::default(),
        };
        storage_signing::sign_hosting_report_v2(signer, material, &mut p).expect("dual-sign");
        let topic = format!("harmony/storage/{}/hosting", p.owner_address);
        (topic, serde_json::to_vec(&p).unwrap())
    }

    #[test]
    fn v2_record_pins_and_accepts_zeb679() {
        let world = mint_quorum_world(0x20);
        let mut store = StorageRecordStore::new(None);
        let id = test_identity();
        let (topic, bytes) = dual_signed_pledge_bytes(&id, &master_material(&world), vec![], 1);
        assert!(store
            .on_pledge_list_sample(&topic, &bytes, &rvk(), NOW_MS)
            .changed());
        let pin = store.signer_pin(&addr_of(&id)).expect("pinned");
        assert_eq!(pin.owner_id, world.owner_id);
        assert_eq!(
            pin.device_ed25519,
            world.a_cert.device_pubkeys.classical.ed25519_verify
        );
    }

    #[test]
    fn revoked_signer_rejected_zero_state_zeb679() {
        let world = mint_quorum_world(0x28);
        let mut store = StorageRecordStore::new(None);
        let id = test_identity();
        let revoked = rvk();
        let key = world.a_cert.device_pubkeys.classical.ed25519_verify;
        let keys: std::collections::BTreeSet<[u8; 32]> = std::iter::once(key).collect();
        revoked.union_from_members(std::iter::once((OwnerAddr(world.owner_id), &keys)));

        let (topic, bytes) = dual_signed_pledge_bytes(&id, &master_material(&world), vec![], 1);
        let outcome = store.on_pledge_list_sample(&topic, &bytes, &revoked, NOW_MS);
        assert!(
            matches!(outcome, RecordOutcome::Rejected(ref e) if e.contains("revoked")),
            "{outcome:?}"
        );
        assert!(store.pledge_list(&addr_of(&id)).is_none(), "zero state");
        assert!(store.signer_pin(&addr_of(&id)).is_none(), "no pin");
    }

    #[test]
    fn legacy_after_pin_rejected_cross_family_zeb679() {
        let world = mint_quorum_world(0x20);
        let mut store = StorageRecordStore::new(None);
        let id = test_identity();
        let (topic, bytes) = dual_signed_pledge_bytes(&id, &master_material(&world), vec![], 1);
        assert!(store
            .on_pledge_list_sample(&topic, &bytes, &rvk(), NOW_MS)
            .changed());

        // Legacy pledge from the (now migrated) owner: ratchet rejects,
        // even at a NEWER updated_at.
        let (topic, legacy) = signed_pledge_bytes(&id, vec![], 9);
        let outcome = store.on_pledge_list_sample(&topic, &legacy, &rvk(), NOW_MS);
        assert!(
            matches!(outcome, RecordOutcome::Rejected(ref e) if e.contains("signer pin")),
            "{outcome:?}"
        );
        // The ratchet is per-ADDRESS, so it spans record families: a
        // legacy hosting report from the same address is also rejected.
        let (topic, legacy_hosting) = signed_hosting_bytes(&id, vec![], 9);
        let outcome = store.on_hosting_report_sample(&topic, &legacy_hosting, &rvk(), NOW_MS);
        assert!(
            matches!(outcome, RecordOutcome::Rejected(ref e) if e.contains("signer pin")),
            "{outcome:?}"
        );
    }

    #[test]
    fn rebind_to_different_owner_rejected_zeb679() {
        let world = mint_quorum_world(0x20);
        let other = mint_quorum_world(0x24);
        let mut store = StorageRecordStore::new(None);
        let id = test_identity();
        let (topic, bytes) = dual_signed_pledge_bytes(&id, &master_material(&world), vec![], 1);
        assert!(store
            .on_pledge_list_sample(&topic, &bytes, &rvk(), NOW_MS)
            .changed());

        // Same #3 address re-anchoring to a DIFFERENT owner (the
        // throwaway-owner laundering move): first-write-wins refuses.
        let (topic, rebind) = dual_signed_pledge_bytes(&id, &master_material(&other), vec![], 9);
        let outcome = store.on_pledge_list_sample(&topic, &rebind, &rvk(), NOW_MS);
        assert!(
            matches!(outcome, RecordOutcome::Rejected(ref e) if e.contains("rebind")),
            "{outcome:?}"
        );
        let pin = store.signer_pin(&addr_of(&id)).expect("pin kept");
        assert_eq!(pin.owner_id, world.owner_id, "original binding kept");
    }

    #[test]
    fn pin_survives_disk_reload_and_keeps_ratchet_zeb679() {
        let world = mint_quorum_world(0x20);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("storage_records.json");
        let id = test_identity();
        {
            let mut store = StorageRecordStore::new(Some(path.clone()));
            // Pin via a HOSTING report — the record itself is in-memory
            // only, so this also pins the "pin persists even when its
            // record doesn't" property.
            let (topic, bytes) = dual_signed_hosting_bytes(&id, &master_material(&world), 1);
            assert!(store
                .on_hosting_report_sample(&topic, &bytes, &rvk(), NOW_MS)
                .changed());
        }
        let mut reloaded = StorageRecordStore::new(Some(path));
        assert!(
            reloaded.hosting_report(&addr_of(&id)).is_none(),
            "hosting record itself is not persisted"
        );
        let pin = reloaded.signer_pin(&addr_of(&id)).expect("pin reloaded");
        assert_eq!(pin.owner_id, world.owner_id);
        // The revoked device's post-restart legacy publish is still dead.
        let (topic, legacy) = signed_pledge_bytes(&id, vec![], 9);
        let outcome = reloaded.on_pledge_list_sample(&topic, &legacy, &rvk(), NOW_MS);
        assert!(
            matches!(outcome, RecordOutcome::Rejected(ref e) if e.contains("signer pin")),
            "{outcome:?}"
        );
    }

    #[test]
    fn invalid_v2_material_rejected_zero_state_zeb679() {
        let world = mint_quorum_world(0x20);
        let mut store = StorageRecordStore::new(None);
        let victim = test_identity();
        let attacker = test_identity();
        // Swap attack at the ingest boundary: victim's legacy record with
        // the attacker's (genuinely valid) v2 material attached.
        let mut p = PledgeListPayload {
            owner_address: addr_of(&victim),
            pledges: vec![],
            updated_at: 1,
            identity_pub: None,
            sig: None,
            v2: Default::default(),
        };
        storage_signing::sign_pledge_list(&victim, &mut p);
        let mut hijacked = p.clone();
        storage_signing::sign_pledge_list_v2(&attacker, &master_material(&world), &mut hijacked)
            .expect("attacker dual-sign");
        // Graft the attacker's v2 block onto the victim's record (the
        // legacy fields stay the victim's).
        p.v2 = hijacked.v2;
        let topic = format!("harmony/storage/{}/pledges", p.owner_address);
        let outcome =
            store.on_pledge_list_sample(&topic, &serde_json::to_vec(&p).unwrap(), &rvk(), NOW_MS);
        assert!(
            matches!(outcome, RecordOutcome::Rejected(ref e) if e.contains("binding")),
            "{outcome:?}"
        );
        assert!(store.pledge_list(&addr_of(&victim)).is_none(), "zero state");
        assert!(store.signer_pin(&addr_of(&victim)).is_none(), "no pin");
    }

    #[test]
    fn older_v2_replay_still_pins_zeb679() {
        let world = mint_quorum_world(0x20);
        let mut store = StorageRecordStore::new(None);
        let id = test_identity();
        // Legacy record lands first (unpinned bootstrap), newer clock.
        let (topic, legacy) = signed_pledge_bytes(&id, vec![], 10);
        assert!(store
            .on_pledge_list_sample(&topic, &legacy, &rvk(), NOW_MS)
            .changed());
        // An OLDER v2 record is LWW-ignored but its verified binding
        // still pins — the binding is time-invariant.
        let (topic, v2_bytes) = dual_signed_pledge_bytes(&id, &master_material(&world), vec![], 5);
        let outcome = store.on_pledge_list_sample(&topic, &v2_bytes, &rvk(), NOW_MS);
        assert_eq!(outcome, RecordOutcome::IgnoredOlder);
        assert!(store.signer_pin(&addr_of(&id)).is_some(), "pin set anyway");
        // …and the ratchet is live from that point on.
        let (topic, legacy2) = signed_pledge_bytes(&id, vec![], 20);
        let outcome = store.on_pledge_list_sample(&topic, &legacy2, &rvk(), NOW_MS);
        assert!(matches!(outcome, RecordOutcome::Rejected(_)), "{outcome:?}");
    }

    #[test]
    fn evict_pins_prefers_dead_then_newest_zeb679() {
        let mut pins: HashMap<String, StorageSignerPin> = HashMap::new();
        let mut pledges: HashMap<String, PledgeListRecord> = HashMap::new();
        let backups: HashMap<String, BackupSetRecord> = HashMap::new();
        let hosting: HashMap<String, HostingReportRecord> = HashMap::new();
        // seq mirrors the pin's arrival order (== pinned_at_ms here), which is
        // what eviction now orders on (ZEB-863).
        let pin = |t| StorageSignerPin {
            owner_id: [1; 16],
            device_ed25519: [2; 32],
            pinned_at_ms: t,
            seq: t,
        };
        // One live-backed pin + an OLD established dead ratchet pin +
        // a flood of fresh throwaway pins over the cap (the R1 attack).
        pins.insert("live".into(), pin(0));
        pledges.insert(
            "live".into(),
            PledgeListRecord {
                pledges: vec![],
                updated_at: 1,
                received_at_ms: 0,
                seq: 0,
            },
        );
        pins.insert("established-ratchet".into(), pin(5));
        for i in 0..MAX_SIGNER_PINS {
            pins.insert(format!("flood-{i:05}"), pin(1_000 + i as u64));
        }
        evict_pins(&mut pins, &pledges, &backups, &hosting);
        assert_eq!(pins.len(), MAX_SIGNER_PINS);
        assert!(
            pins.contains_key("live"),
            "live-backed pin survives even as the globally oldest"
        );
        assert!(
            pins.contains_key("established-ratchet"),
            "old dead ratchet pin survives a fresh-pin flood (newest-dead evicted first)"
        );
        assert!(
            !pins.contains_key(&format!("flood-{:05}", MAX_SIGNER_PINS - 1)),
            "the flood's newest pins are the ones evicted"
        );
    }

    #[test]
    fn evict_overflow_is_flood_proof_newest_received_first() {
        // MAX_TRACKED_OWNERS honest rows, all received long ago.
        let mut map: HashMap<String, PledgeListRecord> = HashMap::new();
        for i in 0..MAX_TRACKED_OWNERS {
            map.insert(
                format!("honest-{i:04}"),
                PledgeListRecord {
                    pledges: vec![],
                    updated_at: 1,
                    received_at_ms: 1,
                    seq: i as u64,
                },
            );
        }
        // A flood of throwaway owners: far-FUTURE peer updated_at, but received
        // LAST (highest seq = the newest). Under the OLD min(updated_at) evictor
        // these never lose; the honest rows were evicted instead.
        for i in 0..16 {
            map.insert(
                format!("flood-{i:04}"),
                PledgeListRecord {
                    pledges: vec![],
                    updated_at: u64::MAX,
                    received_at_ms: 10_000,
                    seq: MAX_TRACKED_OWNERS as u64 + i as u64,
                },
            );
        }
        evict_overflow(&mut map, |r| r.seq);
        assert_eq!(map.len(), MAX_TRACKED_OWNERS);
        for i in 0..MAX_TRACKED_OWNERS {
            assert!(
                map.contains_key(&format!("honest-{i:04}")),
                "honest-{i:04} must survive the flood"
            );
        }
        assert!(
            !map.keys().any(|k| k.starts_with("flood-")),
            "every flood row evicted itself"
        );
    }

    #[test]
    fn ingest_stamps_local_received_clock_not_peer_updated_at() {
        let mut store = StorageRecordStore::new(None);
        let id = test_identity();
        let owner = addr_of(&id);
        // Peer claims a far-future updated_at; local receipt is now_ms = 5_000.
        let (topic, bytes) = signed_pledge_bytes(&id, vec![pledge("x", 1)], u64::MAX);
        assert_eq!(
            store.on_pledge_list_sample(&topic, &bytes, &rvk(), 5_000),
            RecordOutcome::Inserted
        );
        assert_eq!(
            store.pledge_list(&owner).unwrap().received_at_ms,
            5_000,
            "received_at_ms is the LOCAL receipt clock, independent of the peer updated_at"
        );
    }

    #[test]
    fn purge_revoked_drops_records_keeps_ratchet_zeb679_r1() {
        let world = mint_quorum_world(0x20);
        let mut store = StorageRecordStore::new(None);
        let id = test_identity();
        let addr = addr_of(&id);
        // Records land while the signer is in good standing.
        let (topic, bytes) =
            dual_signed_pledge_bytes(&id, &master_material(&world), vec![pledge("someone", 5)], 1);
        assert!(store
            .on_pledge_list_sample(&topic, &bytes, &rvk(), NOW_MS)
            .changed());
        let (topic, bytes) = dual_signed_hosting_bytes(&id, &master_material(&world), 1);
        assert!(store
            .on_hosting_report_sample(&topic, &bytes, &rvk(), NOW_MS)
            .changed());

        // The projection later learns the revocation (community feed,
        // friend-link carry, …) — the already-admitted records must go.
        let revoked = rvk();
        let key = world.a_cert.device_pubkeys.classical.ed25519_verify;
        let keys: std::collections::BTreeSet<[u8; 32]> = std::iter::once(key).collect();
        revoked.union_from_members(std::iter::once((OwnerAddr(world.owner_id), &keys)));
        assert!(store.purge_revoked(&revoked), "records dropped");
        assert!(store.pledge_list(&addr).is_none());
        assert!(store.hosting_report(&addr).is_none());
        assert!(!store.purge_revoked(&revoked), "idempotent");
        // The pin STAYS: the ratchet still blocks the legacy downgrade…
        assert!(store.signer_pin(&addr).is_some(), "pin kept (sticky)");
        let (topic, legacy) = signed_pledge_bytes(&id, vec![], 9);
        assert!(matches!(
            store.on_pledge_list_sample(&topic, &legacy, &revoked, NOW_MS),
            RecordOutcome::Rejected(_)
        ));
        // …and re-admission of v2 records is blocked at ingest.
        let (topic, again) = dual_signed_pledge_bytes(&id, &master_material(&world), vec![], 20);
        assert!(matches!(
            store.on_pledge_list_sample(&topic, &again, &revoked, NOW_MS),
            RecordOutcome::Rejected(ref e) if e.contains("revoked")
        ));
    }
}
