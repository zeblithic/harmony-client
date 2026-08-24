//! ZEB-669 slice 2: the hosting ledger — local source of truth for the
//! contribution meter's numerator and for HostingReport publishes.
//!
//! Physical-CID refcounting across pacts (spec §3, PR #448 review): the
//! same CID may appear in several buddies' backup sets. Bytes are stored
//! once — [`StorageLedger::distinct_pinned_bytes`] counts each distinct
//! pinned CID once — while per-pact attribution keeps an entry per
//! (buddy, cid) so each pledging pact's budget slice is charged. A CID
//! is unpinned only when the LAST referencing pact releases it: a
//! revoke/backup-set change for one pact never evicts content another
//! pact still requires.
//!
//! Entries are recorded ONLY after a successful fetch+pin (actual bytes,
//! never claimed) — the honesty rule. v1 is device-local; fleet-unified
//! hosting across an owner's devices is future work.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const LEDGER_FILE_VERSION: u32 = 1;

/// What a release did to the underlying physical pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// The buddy did not hold this cid — no-op.
    NotHeld,
    /// Released this pact's attribution; another pact still pins the cid.
    StillReferenced,
    /// That was the final reference — the caller must unpin the cid.
    LastReference,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerEntry {
    /// 64-hex ContentId.
    pub cid: String,
    /// ACTUAL fetched bytes (never the backup-set claim).
    pub size: u64,
    pub pinned_at_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuddyOnDisk {
    owner: String,
    entries: Vec<LedgerEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LedgerDiskV1 {
    version: u32,
    #[serde(default)]
    buddies: Vec<BuddyOnDisk>,
}

/// Canonical filename, bound as the device-envelope AAD (ZEB-986 PR-3).
const LEDGER_FILE: &str = "storage_ledger.json";

/// Refcounted (buddy → pinned entries) ledger, persisted to
/// `storage_ledger.json`, sealed under the device envelope (ZEB-986 PR-3).
#[derive(Debug)]
pub struct StorageLedger {
    per_buddy: BTreeMap<String, Vec<LedgerEntry>>,
    path: Option<PathBuf>,
    /// ZEB-986 PR-3: device cipher for at-rest sealing. `None` for the path-less
    /// placeholder and on a pre-identity boot.
    cipher: Option<crate::device_dataset_file::DeviceCipher>,
    /// ZEB-986 PR-3: fail-closed latch. Set when the sealed file could not be read as a
    /// trustworthy ledger at load (unreadable, undecryptable, corrupt, or an unsupported
    /// version). While set, `save()` is a no-op so the still-good on-disk ledger is never
    /// overwritten with an empty one. RAM mutations still occur but do not persist until
    /// the fault clears (the local hosting ledger re-derives). A clean load or a clean
    /// absence leaves it clear.
    sealed_fault: bool,
}

impl StorageLedger {
    /// Load from `path`, sealed under the device envelope (ZEB-986 PR-3). Missing ⇒ empty
    /// (first run). Any load failure — unreadable, undecryptable, corrupt, or an
    /// unsupported version — FAILS CLOSED: empty + `sealed_fault` (freeze `save()`), so the
    /// preserved on-disk ledger is never overwritten with an empty one. A legacy plaintext
    /// file that parses is re-sealed in place.
    pub fn new(
        cipher: Option<crate::device_dataset_file::DeviceCipher>,
        path: Option<PathBuf>,
    ) -> Self {
        let mut ledger = Self {
            per_buddy: BTreeMap::new(),
            path,
            cipher,
            sealed_fault: false,
        };
        let Some(p) = ledger.path.clone() else {
            return ledger;
        };
        let cipher = ledger.cipher.clone();
        let image = match &cipher {
            Some(c) => match crate::device_dataset_file::read_image(c, &p, LEDGER_FILE) {
                Ok(Some(img)) => img,
                Ok(None) => return ledger, // first run
                Err(e) => {
                    tracing::warn!(path = ?p, error = %e, "storage_ledger: sealed load failed — FAIL-CLOSED (freezing writes, preserving file)");
                    ledger.sealed_fault = true;
                    return ledger;
                }
            },
            None => {
                if p.exists() {
                    tracing::warn!(path = ?p, "storage_ledger: no device cipher — FAIL-CLOSED (freezing writes, preserving file)");
                    ledger.sealed_fault = true;
                }
                return ledger;
            }
        };
        match serde_json::from_slice::<LedgerDiskV1>(&image.bytes) {
            Ok(disk) if disk.version == LEDGER_FILE_VERSION => {
                for b in disk.buddies {
                    ledger.per_buddy.insert(b.owner, b.entries);
                }
                if let Some(c) = &cipher {
                    crate::device_dataset_file::reseal_if_legacy(c, &p, LEDGER_FILE, &image);
                }
            }
            Ok(disk) => {
                tracing::warn!(
                    "storage_ledger version {} unsupported — FAIL-CLOSED (freezing writes)",
                    disk.version
                );
                ledger.sealed_fault = true;
            }
            Err(e) => {
                tracing::warn!("storage_ledger parse failed — FAIL-CLOSED (freezing writes): {e}");
                ledger.sealed_fault = true;
            }
        }
        ledger
    }

    /// ZEB-986 PR-3: arm the device cipher after a fresh-profile boot (identity.key is
    /// created partway through `start_node_inner`, after this ledger first loads). On a
    /// fresh profile the file is absent, so the ledger loaded empty and un-faulted; arming
    /// the cipher lets later saves seal. Deliberately does NOT clear `sealed_fault`. No-op
    /// if already armed.
    pub fn arm_cipher(&mut self, cipher: crate::device_dataset_file::DeviceCipher) {
        if self.cipher.is_none() {
            self.cipher = Some(cipher);
        }
    }

    /// Persist. Public so callers batching several mutations via the
    /// non-saving internals can flush once — the standard mutators below
    /// already save.
    pub fn save(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        if self.sealed_fault {
            tracing::warn!(
                path = ?path,
                "storage_ledger save skipped — sealed-fault (preserving the on-disk ledger)"
            );
            return;
        }
        let Some(cipher) = &self.cipher else {
            tracing::warn!(
                path = ?path,
                "storage_ledger save skipped — no device cipher (pre-identity boot)"
            );
            return;
        };
        let disk = LedgerDiskV1 {
            version: LEDGER_FILE_VERSION,
            buddies: self
                .per_buddy
                .iter()
                .map(|(owner, entries)| BuddyOnDisk {
                    owner: owner.clone(),
                    entries: entries.clone(),
                })
                .collect(),
        };
        match serde_json::to_vec_pretty(&disk) {
            Ok(bytes) => {
                // ZEB-986 PR-3: seal + atomic write + fsync + 0600.
                if let Err(e) =
                    crate::device_dataset_file::write_image(cipher, path, LEDGER_FILE, &bytes)
                {
                    tracing::error!("storage_ledger sealed save failed: {e}");
                }
            }
            Err(e) => tracing::error!("storage_ledger serialize failed: {e}"),
        }
    }

    /// Record that `cid` (of ACTUAL `size` bytes) is pinned on behalf of
    /// `buddy`. Returns false (no-op) when the buddy already holds it.
    pub fn record_pin(&mut self, buddy: &str, cid: &str, size: u64, now_ms: u64) -> bool {
        let entries = self.per_buddy.entry(buddy.to_string()).or_default();
        if entries.iter().any(|e| e.cid == cid) {
            return false;
        }
        entries.push(LedgerEntry {
            cid: cid.to_string(),
            size,
            pinned_at_ms: now_ms,
        });
        self.save();
        true
    }

    /// Release `buddy`'s attribution of `cid`.
    pub fn release(&mut self, buddy: &str, cid: &str) -> ReleaseOutcome {
        let outcome = self.release_inner(buddy, cid);
        if outcome != ReleaseOutcome::NotHeld {
            self.save();
        }
        outcome
    }

    /// Release without persisting — batch callers (`evict_newest_first`)
    /// save once per sweep instead of once per cid (PR #449 review,
    /// CodeRabbit: a large budget shrink must not fsync per eviction).
    fn release_inner(&mut self, buddy: &str, cid: &str) -> ReleaseOutcome {
        let Some(entries) = self.per_buddy.get_mut(buddy) else {
            return ReleaseOutcome::NotHeld;
        };
        let before = entries.len();
        entries.retain(|e| e.cid != cid);
        if entries.len() == before {
            return ReleaseOutcome::NotHeld;
        }
        if entries.is_empty() {
            self.per_buddy.remove(buddy);
        }
        if self.refcount(cid) == 0 {
            ReleaseOutcome::LastReference
        } else {
            ReleaseOutcome::StillReferenced
        }
    }

    /// Release every attribution held for `buddy` (pact revoked/removed).
    /// Returns the cids that hit last-reference — the caller unpins them.
    pub fn release_buddy(&mut self, buddy: &str) -> Vec<String> {
        let Some(entries) = self.per_buddy.remove(buddy) else {
            return Vec::new();
        };
        let freed: Vec<String> = entries
            .into_iter()
            .filter(|e| self.refcount(&e.cid) == 0)
            .map(|e| e.cid)
            .collect();
        self.save();
        freed
    }

    /// Budget shrunk: evict newest-pinned-first (spec §3) until the
    /// distinct-bytes numerator fits `target_bytes`. Returns the cids
    /// that hit last-reference (to unpin). Attribution-only removals of
    /// still-referenced cids free no bytes but honor eviction order.
    pub fn evict_newest_first(&mut self, target_bytes: u64) -> Vec<String> {
        let mut freed = Vec::new();
        let mut removed_any = false;
        while self.distinct_pinned_bytes() > target_bytes {
            let newest = self
                .per_buddy
                .iter()
                .flat_map(|(owner, entries)| {
                    entries
                        .iter()
                        .map(move |e| (e.pinned_at_ms, owner.clone(), e.cid.clone()))
                })
                .max();
            let Some((_, owner, cid)) = newest else {
                break;
            };
            if self.release_inner(&owner, &cid) == ReleaseOutcome::LastReference {
                freed.push(cid);
            }
            removed_any = true;
        }
        if removed_any {
            self.save();
        }
        freed
    }

    /// Boot honesty sweep: the RAM-only cache lost `cid` across restart,
    /// so every attribution of it is dropped — hosting reports must only
    /// ever claim actually-held bytes. The cid re-enters via the normal
    /// fetch path when pacts still want it.
    pub fn drop_cid_everywhere(&mut self, cid: &str) {
        for entries in self.per_buddy.values_mut() {
            entries.retain(|e| e.cid != cid);
        }
        self.per_buddy.retain(|_, entries| !entries.is_empty());
        self.save();
    }

    pub fn holds(&self, buddy: &str, cid: &str) -> bool {
        self.per_buddy
            .get(buddy)
            .is_some_and(|entries| entries.iter().any(|e| e.cid == cid))
    }

    /// Some buddy (any pact) already pins this cid — with its recorded
    /// actual size. The attribute-without-refetch signal.
    pub fn held_anywhere(&self, cid: &str) -> Option<u64> {
        self.per_buddy
            .values()
            .flat_map(|entries| entries.iter())
            .find(|e| e.cid == cid)
            .map(|e| e.size)
    }

    /// Per-pact attribution: bytes charged against `buddy`'s slice.
    pub fn bytes_for_buddy(&self, buddy: &str) -> u64 {
        self.per_buddy
            .get(buddy)
            .map(|entries| entries.iter().map(|e| e.size).sum())
            .unwrap_or(0)
    }

    pub fn cid_count_for_buddy(&self, buddy: &str) -> u32 {
        self.per_buddy
            .get(buddy)
            .map(|entries| entries.len() as u32)
            .unwrap_or(0)
    }

    /// The contribution meter's numerator: each distinct pinned CID
    /// counted once (bytes are stored once regardless of pact fan-in).
    pub fn distinct_pinned_bytes(&self) -> u64 {
        let mut seen = HashSet::new();
        self.per_buddy
            .values()
            .flat_map(|entries| entries.iter())
            .filter(|e| seen.insert(e.cid.as_str()))
            .map(|e| e.size)
            .sum()
    }

    pub fn distinct_cids(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.per_buddy
            .values()
            .flat_map(|entries| entries.iter())
            .filter(|e| seen.insert(e.cid.as_str()))
            .map(|e| e.cid.clone())
            .collect()
    }

    /// Buddies with at least one attribution, sorted (BTreeMap order).
    pub fn buddies(&self) -> Vec<String> {
        self.per_buddy.keys().cloned().collect()
    }

    /// Cids attributed to `buddy` (ledger truth for release diffing).
    pub fn cids_for_buddy(&self, buddy: &str) -> Vec<String> {
        self.per_buddy
            .get(buddy)
            .map(|entries| entries.iter().map(|e| e.cid.clone()).collect())
            .unwrap_or_default()
    }

    fn refcount(&self, cid: &str) -> usize {
        self.per_buddy
            .values()
            .filter(|entries| entries.iter().any(|e| e.cid == cid))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic device cipher for the sealing tests (ZEB-986 PR-3).
    fn tc() -> crate::device_dataset_file::DeviceCipher {
        crate::device_dataset_file::test_cipher()
    }

    #[test]
    fn record_and_release_roundtrip() {
        let mut ledger = StorageLedger::new(None, None);
        assert!(ledger.record_pin("alice", "cid1", 100, 1));
        assert!(
            !ledger.record_pin("alice", "cid1", 100, 2),
            "duplicate is a no-op"
        );
        assert!(ledger.holds("alice", "cid1"));
        assert_eq!(ledger.bytes_for_buddy("alice"), 100);
        assert_eq!(
            ledger.release("alice", "cid1"),
            ReleaseOutcome::LastReference
        );
        assert_eq!(ledger.release("alice", "cid1"), ReleaseOutcome::NotHeld);
        assert_eq!(ledger.distinct_pinned_bytes(), 0);
    }

    #[test]
    fn shared_cid_counted_once_globally_but_attributed_to_both() {
        let mut ledger = StorageLedger::new(None, None);
        ledger.record_pin("alice", "cid1", 100, 1);
        ledger.record_pin("bob", "cid1", 100, 2);
        ledger.record_pin("bob", "cid2", 40, 3);

        assert_eq!(ledger.distinct_pinned_bytes(), 140, "cid1 stored once");
        assert_eq!(
            ledger.bytes_for_buddy("alice"),
            100,
            "slice attribution per pact"
        );
        assert_eq!(ledger.bytes_for_buddy("bob"), 140);
        assert_eq!(ledger.cid_count_for_buddy("bob"), 2);
        assert_eq!(ledger.held_anywhere("cid1"), Some(100));

        assert_eq!(
            ledger.release("alice", "cid1"),
            ReleaseOutcome::StillReferenced,
            "bob still pins cid1"
        );
        assert_eq!(ledger.distinct_pinned_bytes(), 140, "no bytes freed");
        assert_eq!(ledger.release("bob", "cid1"), ReleaseOutcome::LastReference);
        assert_eq!(ledger.distinct_pinned_bytes(), 40);
    }

    #[test]
    fn release_buddy_returns_only_last_ref_cids() {
        let mut ledger = StorageLedger::new(None, None);
        ledger.record_pin("alice", "shared", 10, 1);
        ledger.record_pin("bob", "shared", 10, 2);
        ledger.record_pin("bob", "solo", 20, 3);

        let freed = ledger.release_buddy("bob");
        assert_eq!(freed, vec!["solo".to_string()], "shared survives via alice");
        assert!(ledger.holds("alice", "shared"));
        assert!(
            ledger.release_buddy("bob").is_empty(),
            "second removal is a no-op"
        );
    }

    #[test]
    fn evict_newest_first_reaches_target_without_double_freeing_shared_cids() {
        let mut ledger = StorageLedger::new(None, None);
        ledger.record_pin("alice", "old", 100, 10);
        ledger.record_pin("alice", "shared", 50, 20);
        ledger.record_pin("bob", "shared", 50, 30); // newest entry, refcounted
        ledger.record_pin("bob", "new", 100, 40);

        // distinct = 250. Target 100: evict "new" (t=40, frees 100),
        // then bob's "shared" (t=30, still referenced → frees nothing),
        // then alice's "shared" (t=20, last ref → frees 50) ⇒ 100.
        let freed = ledger.evict_newest_first(100);
        assert_eq!(ledger.distinct_pinned_bytes(), 100);
        assert_eq!(freed, vec!["new".to_string(), "shared".to_string()]);
        assert!(ledger.holds("alice", "old"), "oldest survives");
    }

    #[test]
    fn drop_cid_everywhere_removes_all_attributions() {
        let mut ledger = StorageLedger::new(None, None);
        ledger.record_pin("alice", "gone", 10, 1);
        ledger.record_pin("bob", "gone", 10, 2);
        ledger.record_pin("bob", "kept", 5, 3);

        ledger.drop_cid_everywhere("gone");
        assert_eq!(ledger.distinct_pinned_bytes(), 5);
        assert!(!ledger.holds("alice", "gone"));
        assert_eq!(
            ledger.buddies(),
            vec!["bob".to_string()],
            "empty buddies pruned"
        );
    }

    #[test]
    fn ledger_survives_disk_reload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("storage_ledger.json");
        {
            let mut ledger = StorageLedger::new(Some(tc()), Some(path.clone()));
            ledger.record_pin("alice", "cid1", 100, 7);
        }
        let reloaded = StorageLedger::new(Some(tc()), Some(path));
        assert!(reloaded.holds("alice", "cid1"));
        assert_eq!(reloaded.distinct_pinned_bytes(), 100);
        assert_eq!(
            reloaded.per_buddy.get("alice").unwrap()[0].pinned_at_ms,
            7,
            "eviction ordering survives reload"
        );
    }

    #[test]
    fn corrupt_disk_file_yields_empty_ledger() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("storage_ledger.json");
        std::fs::write(&path, b"nope").unwrap();
        let ledger = StorageLedger::new(Some(tc()), Some(path));
        assert!(ledger.buddies().is_empty());
    }

    // ── ZEB-986 PR-3: at-rest sealing + fail-closed ──────────────────────────

    #[test]
    fn save_seals_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("storage_ledger.json");
        let mut ledger = StorageLedger::new(Some(tc()), Some(path.clone()));
        ledger.record_pin("alice", "cid1", 100, 7);
        assert_eq!(
            std::fs::read(&path).unwrap()[0],
            crate::device_dataset_file::SEALED_DEVICE_SCHEMA_V3,
            "storage_ledger sealed on disk"
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_mode_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("storage_ledger.json");
        let mut ledger = StorageLedger::new(Some(tc()), Some(path.clone()));
        ledger.record_pin("alice", "cid1", 100, 7);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "sealed storage_ledger is owner-only");
    }

    #[test]
    fn foreign_cipher_fails_closed_and_preserves_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("storage_ledger.json");
        {
            let mut ledger = StorageLedger::new(Some(tc()), Some(path.clone()));
            ledger.record_pin("alice", "cid1", 100, 7);
        }
        let sealed = std::fs::read(&path).unwrap();
        let foreign = crate::device_dataset_file::DeviceCipher::derive(&[9u8; 32]).unwrap();
        let mut ledger = StorageLedger::new(Some(foreign), Some(path.clone()));
        assert!(ledger.buddies().is_empty(), "undecryptable → empty");
        ledger.record_pin("mallory", "cid2", 5, 8); // RAM mutation; save() is a no-op
        ledger.save();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            sealed,
            "faulted ledger preserves the on-disk file byte-identical"
        );
    }

    #[test]
    fn legacy_plaintext_migrates_to_sealed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("storage_ledger.json");
        std::fs::write(
            &path,
            br#"{"version":1,"buddies":[{"owner":"alice","entries":[{"cid":"c","size":9,"pinnedAtMs":1}]}]}"#,
        )
        .unwrap();
        let ledger = StorageLedger::new(Some(tc()), Some(path.clone()));
        assert!(ledger.holds("alice", "c"), "legacy data recovered");
        assert_eq!(
            std::fs::read(&path).unwrap()[0],
            crate::device_dataset_file::SEALED_DEVICE_SCHEMA_V3,
            "migrated to sealed on load"
        );
    }
}
