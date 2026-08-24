//! Client-side sidecar for self-ingested content metadata (ZEB-146).
//!
//! Persists a map of `sidecar_id -> ContentIndexEntry` as JSON under
//! `app_data_dir/content-index.json` so the File Manager UI can surface
//! filenames, ingest timestamps, and user-set flags (sensitivity,
//! replication tier, licensed, archived) for content that the runtime's
//! RAM-only cache doesn't know about.
//!
//! ZEB-164: the storage key flips from `[u8;32]` CID to `SidecarId` so
//! multiple user-visible entries (symlink-style) can share a CID. The
//! runtime pin_intent invariant now derives from the OR of every sidecar
//! entry's pinned flag for a given CID — see `is_cid_pinned_by_any`.
//!
//! Authority split:
//! - Sidecar is authoritative for membership, size_bytes, and pin *intent*
//!   (CIDs are immutable, so size never drifts from the ingest-time value;
//!   intent is what the user asked for and must survive restart).
//! - Runtime cache is authoritative for pin *effect* — the active eviction
//!   protection. `list_content` OR-joins the two sources at display time;
//!   see `ContentIndexEntry.pinned` for the full shape.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Opaque per-entry stable identity for a sidecar row.
///
/// The sidecar key was `[u8; 32]` CID prior to ZEB-164, which forced one
/// entry per CID. With multiple user-visible entries (folders or otherwise)
/// allowed to share a CID — symlink-style — we need a stable identity that
/// is independent of content. UUID v4 is opaque (callers can't conflate
/// identity with content), survives restart, and is unique across devices
/// in case sidecars ever sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SidecarId(Uuid);

#[allow(clippy::new_without_default)]
impl SidecarId {
    /// Mint a fresh random SidecarId. Backend is the source of truth for
    /// minting; the frontend never generates these.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Parse the hyphenated lowercase Display form back into a SidecarId.
    /// Used at the IPC boundary when commands receive sidecar_id strings.
    pub fn parse_str(s: &str) -> Result<Self, uuid::Error> {
        Uuid::parse_str(s).map(Self)
    }

    /// Underlying 16-byte UUID representation. Used for allocation-free
    /// deterministic ordering on disk; do not depend on this being a
    /// stable hash for cross-version comparisons.
    pub fn as_bytes(&self) -> [u8; 16] {
        self.0.into_bytes()
    }
}

impl std::fmt::Display for SidecarId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hyphenated lowercase, e.g. "8b4f7c2e-1a3d-4f5b-9c0e-1234567890ab".
        write!(f, "{}", self.0.as_hyphenated())
    }
}

const INDEX_FILE: &str = "content-index.json";
const FILE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sensitivity {
    Private,
    Confidential,
    Public,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplicationTier {
    Expendable,
    Light,
    Default,
    High,
    Ultra,
}

/// ZEB-158 slice 1: distinguishes user-visible content kinds at the sidecar
/// level. Leaves are ingested files (books or chunked-file bundles); folders
/// are bundles whose child-0 is a manifest book (see
/// `src-tauri/src/folders.rs` and `docs/specs/2026-04-24-folder-primitive-design.md`).
///
/// The default variant is `Leaf` so `#[serde(default)]` on the `kind` field
/// lets pre-ZEB-158 sidecar entries deserialize correctly (they were all
/// leaves at the time of their last save).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentKind {
    #[default]
    Leaf,
    Folder,
}

/// ZEB-669 S4: provenance for the detail panel's "From" row. Recorded at
/// index-entry creation only — never inferred retroactively (honesty rule,
/// ZEB-610 §0). Serialized camelCase because it rides `ContentItemWire`
/// to the frontend verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OriginInfo {
    pub kind: OriginKind,
    /// 32-char lowercase hex owner address of whoever introduced the
    /// content; `None` for self-ingest.
    #[serde(default)]
    pub introducer: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OriginKind {
    SelfIngest,
    /// Reserved: channel downloads write plaintext to a user-chosen path
    /// and create no index entry today, so no seam emits this yet
    /// (ZEB-669 plan-time seam enumeration).
    ChannelDownload,
    /// Reserved: buddy pins are cache+pin only (no index entry); no seam
    /// emits this yet.
    BuddyPin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentIndexEntry {
    pub sidecar_id: SidecarId,
    #[serde(with = "hex_cid")]
    pub cid: [u8; 32],
    pub file_name: String,
    pub size_bytes: u64,
    pub stored_at_ms: u64,
    pub sensitivity: Sensitivity,
    pub replication_tier: ReplicationTier,
    pub licensed: bool,
    pub archived: bool,
    /// ZEB-155: persisted pin intent. With ZEB-164's symlink model, the
    /// runtime cache's `pin_intent` derives from the OR of every sidecar
    /// entry's pinned flag for a given CID — see `is_cid_pinned_by_any`.
    /// Per-row UI uses this entry's flag directly; cross-entry computation
    /// happens on mutation paths (pin/unpin/burn/rekey) to keep the
    /// runtime invariant in sync.
    ///
    /// `#[serde(default)]` makes pre-ZEB-155 sidecars readable: legacy
    /// entries deserialize with pinned=false (correct — they weren't
    /// pinned at their last save, since the field didn't exist).
    #[serde(default)]
    pub pinned: bool,
    /// ZEB-158 slice 1: distinguishes leaf files from folder bundles at the
    /// sidecar level. Default `Leaf` with `#[serde(default)]` keeps pre-slice-1
    /// sidecars readable — legacy entries were all leaves by construction,
    /// because folders didn't exist before slice 1.
    #[serde(default)]
    pub kind: ContentKind,
    /// ZEB-669 S2: "back up with buddies" flag. Flagged public-durable
    /// entries are published in this owner's signed BackupSet so active
    /// pacts auto-pin them. `#[serde(default)]` keeps pre-field sidecars
    /// readable (legacy entries were never flagged).
    #[serde(default)]
    pub backup: bool,
    /// ZEB-669 S4: provenance recorded at entry creation. `#[serde(default)]`
    /// keeps pre-field sidecars readable — legacy entries stay `None` and the
    /// UI renders no "From" row for them (never inferred retroactively).
    #[serde(default)]
    pub origin: Option<OriginInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    entries: Vec<ContentIndexEntry>,
}

pub struct ContentIndex {
    path: PathBuf,
    entries: HashMap<SidecarId, ContentIndexEntry>,
    /// ZEB-986: set when the index was unreadable OR content-corrupt at load. Because
    /// an empty index orphans every stored blob (and the sensitivity/provenance metadata
    /// cannot be rebuilt from a directory scan), a corrupt index uses FreezeInPlace: the
    /// file is left untouched and writes are frozen (degraded read-only) rather than
    /// overwritten with the empty default.
    disk_write_frozen: bool,
    /// Test-only one-shot hook armed via `arm_next_rekey_conflict`.
    /// When `Some((sid, fake_actual))`, the next `rekey` call targeting
    /// `sid` returns `Err(RekeyError::Conflict { actual: fake_actual })`
    /// without mutating any entry, then clears the hook.
    #[cfg(any(test, feature = "test-fixtures"))]
    test_rekey_conflict_hook: Option<(SidecarId, [u8; 32])>,
    /// Test-only one-shot hook armed via
    /// `arm_next_remove_if_cid_matches_conflict`. Symmetric counterpart
    /// of `test_rekey_conflict_hook` for the `remove_if_cid_matches`
    /// path that backs `move_content`'s Case C STAGE 2.
    #[cfg(any(test, feature = "test-fixtures"))]
    test_remove_conflict_hook: Option<(SidecarId, [u8; 32])>,
}

/// Error returned by [`ContentIndex::rekey`].
///
/// `Collision` was retired in ZEB-164: multiple entries can legally
/// share a CID under the symlink-style sidecar model, so the rekey target
/// already being present is no longer an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RekeyError {
    /// The given `sidecar_id` doesn't refer to any known entry.
    OldMissing,
    /// The entry exists but its current CID doesn't match the
    /// `expected_old_cid` the caller passed — a concurrent rekey
    /// landed first. The caller's view of the chain is stale; any
    /// retry would need to rebuild ancestors from `actual` rather
    /// than the CID it walked. Surfaces the CAS-style guard added
    /// after the create_folder_nested ingest reorder widened the
    /// verify→rekey window from "ancestor reads" to "drain
    /// pending_ingests."
    Conflict { actual: [u8; 32] },
}

impl ContentIndex {
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(INDEX_FILE);
        // Symmetric with save(): if data_dir is empty, `path` is the bare
        // filename "content-index.json" and would resolve to CWD. Don't read
        // a stray CWD sidecar into the default/uninitialised state.
        let path_is_bare = path.parent().is_none_or(|p| p.as_os_str().is_empty());
        let (entries, disk_write_frozen) = if path_is_bare {
            (HashMap::new(), false)
        } else {
            // FreezeInPlace: a corrupt index must NOT be quarantined-and-healed —
            // overwriting it with an empty index orphans every stored blob. Preserve
            // the file and freeze writes so a transient error self-recovers next boot
            // and genuine corruption stays available for a future rebuild feature.
            // `now_ms` is unused under FreezeInPlace (no timestamped quarantine), so the
            // load signature stays clock-free and its many call sites are untouched.
            let recovered = crate::recoverable_load::load_or_recover(
                &path,
                crate::recoverable_load::now_ms(),
                crate::recoverable_load::CorruptPolicy::FreezeInPlace,
                Self::parse_index,
            );
            (recovered.value, recovered.disk_write_frozen)
        };
        ContentIndex {
            path,
            entries,
            disk_write_frozen,
            #[cfg(any(test, feature = "test-fixtures"))]
            test_rekey_conflict_hook: None,
            #[cfg(any(test, feature = "test-fixtures"))]
            test_remove_conflict_hook: None,
        }
    }

    fn parse_index(data: &[u8]) -> Result<HashMap<SidecarId, ContentIndexEntry>, String> {
        let file: IndexFile = serde_json::from_slice(data).map_err(|e| e.to_string())?;
        if file.version != FILE_VERSION {
            return Err(format!("unexpected version {}", file.version));
        }
        let mut map = HashMap::with_capacity(file.entries.len());
        for entry in file.entries {
            if map.insert(entry.sidecar_id, entry).is_some() {
                tracing::warn!("duplicate sidecar_id in content-index.json; last-write-wins");
            }
        }
        Ok(map)
    }

    fn save(&self) {
        if self.disk_write_frozen {
            tracing::warn!(
                path = ?self.path,
                "content-index save skipped — file unreadable/corrupt at load; \
                 preserving existing index (degraded read-only, blobs not orphaned)"
            );
            return;
        }
        // Guard against the default/uninitialised state: NodeState::default()
        // constructs a ContentIndex with an empty data_dir before start_node
        // loads the real one. In that state `self.path` resolves to the
        // bare filename "content-index.json", which would land in the
        // process's current working directory. A properly-initialised path
        // always has a non-empty parent; we use that as the liveness check.
        let path_is_bare = self.path.parent().is_none_or(|p| p.as_os_str().is_empty());
        if path_is_bare {
            tracing::warn!(
                "content-index save called before start_node initialised the sidecar path; \
                 dropping write (mutation lost)"
            );
            return;
        }
        // Sort by sidecar_id for deterministic on-disk ordering; HashMap
        // iteration order would otherwise churn the file on every save and
        // make diffs (and future snapshotting) noisy.
        let mut sorted: Vec<ContentIndexEntry> = self.entries.values().cloned().collect();
        sorted.sort_unstable_by_key(|e| e.sidecar_id.as_bytes());
        let file = IndexFile {
            version: FILE_VERSION,
            entries: sorted,
        };
        let json = match serde_json::to_vec_pretty(&file) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(err = %e, "content-index serialize failed; changes not persisted");
                return;
            }
        };

        let tmp_path = {
            let mut name = self.path.file_name().unwrap_or_default().to_os_string();
            name.push(".tmp");
            self.path.with_file_name(name)
        };
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&tmp_path, &json) {
            tracing::warn!(err = %e, "content-index write failed; changes not persisted");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp_path, &self.path) {
            tracing::warn!(err = %e, "content-index rename failed; tmp file may be stale");
        }
    }

    /// Insert a new entry. Returns `true` if added, `false` if the
    /// `sidecar_id` was already present (no mutation in that case).
    pub fn insert(&mut self, entry: ContentIndexEntry) -> bool {
        if self.entries.contains_key(&entry.sidecar_id) {
            return false;
        }
        self.entries.insert(entry.sidecar_id, entry);
        self.save();
        true
    }

    /// Remove an entry by sidecar_id. Returns `true` if present before the call.
    pub fn remove(&mut self, id: &SidecarId) -> bool {
        let removed = self.entries.remove(id).is_some();
        if removed {
            self.save();
        }
        removed
    }

    /// Flip the `archived` flag. Returns `true` if the flag changed;
    /// `false` if already at the target state or the sidecar_id is unknown.
    pub fn set_archived(&mut self, id: &SidecarId, archived: bool) -> bool {
        let Some(entry) = self.entries.get_mut(id) else {
            return false;
        };
        if entry.archived == archived {
            return false;
        }
        entry.archived = archived;
        self.save();
        true
    }

    /// Update the `file_name` of an existing sidecar entry. Returns
    /// `true` if the name changed, `false` if the sidecar_id is unknown OR
    /// the name was already at the target value. Not CAS-protected on the
    /// name field — consistent with set_archived / set_pinned /
    /// set_replication_tier (all non-CID field mutations on the sidecar
    /// are non-CAS).
    pub fn set_file_name(&mut self, id: &SidecarId, new_name: String) -> bool {
        let Some(entry) = self.entries.get_mut(id) else {
            return false;
        };
        if entry.file_name == new_name {
            return false;
        }
        entry.file_name = new_name;
        self.save();
        true
    }

    /// Atomically replace an entry's CID while preserving user-state
    /// (file_name, sensitivity, replication_tier, licensed, archived,
    /// pinned, kind). Used when a folder mutation produces a new top-level
    /// root CID (nested `create_folder`, future move/rename). One save()
    /// for the whole replacement.
    ///
    /// CAS-style: caller passes `expected_old_cid`; rekey fails with
    /// `Conflict { actual }` if the entry's current CID doesn't match,
    /// preventing lost-update when two concurrent mutations rebuild
    /// from the same old tree but the second one's verify→rekey
    /// gap straddles the first one's commit.
    ///
    /// ZEB-164 retired `RekeyError::Collision`: under the symlink-style
    /// model, the new CID already being used by another entry is not an
    /// error — entries are identified by `sidecar_id`, not CID.
    pub fn rekey(
        &mut self,
        id: &SidecarId,
        expected_old_cid: [u8; 32],
        new_cid: [u8; 32],
        new_size_bytes: u64,
        new_stored_at_ms: u64,
    ) -> Result<(), RekeyError> {
        #[cfg(any(test, feature = "test-fixtures"))]
        if let Some((armed_sid, fake_actual)) = self.test_rekey_conflict_hook {
            if armed_sid == *id {
                self.test_rekey_conflict_hook = None;
                return Err(RekeyError::Conflict {
                    actual: fake_actual,
                });
            }
        }
        let Some(entry) = self.entries.get_mut(id) else {
            return Err(RekeyError::OldMissing);
        };
        if entry.cid != expected_old_cid {
            return Err(RekeyError::Conflict { actual: entry.cid });
        }
        entry.cid = new_cid;
        entry.size_bytes = new_size_bytes;
        entry.stored_at_ms = new_stored_at_ms;
        self.save();
        Ok(())
    }

    /// Remove the sidecar entry whose `id` is `id` and whose current CID
    /// is `expected_cid`. CAS-protected counterpart of [`Self::remove`]:
    /// returns `Err(RekeyError::OldMissing)` if the entry is absent and
    /// `Err(RekeyError::Conflict { actual })` if it exists but its CID
    /// differs. Used by `move_content`'s Case C STAGE 2 so a concurrent
    /// rekey doesn't get its top-level entry deleted.
    pub fn remove_if_cid_matches(
        &mut self,
        id: &SidecarId,
        expected_cid: [u8; 32],
    ) -> Result<(), RekeyError> {
        #[cfg(any(test, feature = "test-fixtures"))]
        if let Some((armed_sid, fake_actual)) = self.test_remove_conflict_hook {
            if armed_sid == *id {
                self.test_remove_conflict_hook = None;
                return Err(RekeyError::Conflict {
                    actual: fake_actual,
                });
            }
        }
        let actual = match self.entries.get(id) {
            None => return Err(RekeyError::OldMissing),
            Some(entry) => entry.cid,
        };
        if actual != expected_cid {
            return Err(RekeyError::Conflict { actual });
        }
        self.entries.remove(id);
        self.save();
        Ok(())
    }

    /// Flip the `pinned` flag. Returns `true` if the flag changed;
    /// `false` if already at the target state or the sidecar_id is unknown.
    pub fn set_pinned(&mut self, id: &SidecarId, pinned: bool) -> bool {
        let Some(entry) = self.entries.get_mut(id) else {
            return false;
        };
        if entry.pinned == pinned {
            return false;
        }
        entry.pinned = pinned;
        self.save();
        true
    }

    /// ZEB-669 S2: toggle the "back up with buddies" flag. Returns true
    /// only when the flag actually changed (unknown id or same value ⇒
    /// false), so callers republish the BackupSet only on real change.
    /// Eligibility (public-durable class) is the IPC layer's check —
    /// the index stores whatever intent survived it.
    pub fn set_backup(&mut self, id: &SidecarId, backup: bool) -> bool {
        let Some(entry) = self.entries.get_mut(id) else {
            return false;
        };
        if entry.backup == backup {
            return false;
        }
        entry.backup = backup;
        self.save();
        true
    }

    /// Set replication tier on a batch of sidecar_ids. Returns the count
    /// of entries whose tier actually changed.
    pub fn set_replication_tier(&mut self, ids: &[SidecarId], tier: ReplicationTier) -> usize {
        let mut changed = 0;
        for id in ids {
            if let Some(entry) = self.entries.get_mut(id) {
                if entry.replication_tier != tier {
                    entry.replication_tier = tier;
                    changed += 1;
                }
            }
        }
        if changed > 0 {
            self.save();
        }
        changed
    }

    /// Look up a single entry by sidecar_id.
    pub fn get(&self, id: &SidecarId) -> Option<&ContentIndexEntry> {
        self.entries.get(id)
    }

    /// Iterate over every sidecar entry referencing this CID.
    ///
    /// With multiple entries allowed per CID (symlink-style), this is the
    /// natural shape for the OR-join logic that maintains the runtime
    /// `pin_intent` invariant. Linear scan; sidecar size is bounded by
    /// user library scale (hundreds to low thousands of entries) and
    /// scans run only on mutation paths, not on `list_content`.
    pub fn entries_for_cid<'a>(
        &'a self,
        cid: &'a [u8; 32],
    ) -> impl Iterator<Item = &'a ContentIndexEntry> {
        self.entries.values().filter(move |e| &e.cid == cid)
    }

    /// True iff some sidecar entry references this CID with `pinned == true`.
    /// Backs the runtime pin_intent invariant: runtime should hold this CID
    /// in `pin_intent` iff this returns true (assuming nothing else outside
    /// the sidecar pins it).
    pub fn is_cid_pinned_by_any(&self, cid: &[u8; 32]) -> bool {
        self.entries_for_cid(cid).any(|e| e.pinned)
    }

    /// Iterate over all entries. **Order is not guaranteed** (HashMap-backed).
    /// Callers that surface results to users must sort.
    pub fn entries(&self) -> impl Iterator<Item = &ContentIndexEntry> {
        self.entries.values()
    }

    /// Test-only: arm a one-shot hook that forces the NEXT `rekey` call
    /// targeting `target` to return `Conflict { actual: fake_actual }`
    /// without mutating the entry. Consumes itself on use.
    ///
    /// Lets tests deterministically exercise STAGE-2 CAS-conflict paths
    /// in `move_content`'s Case B/C/D that would otherwise require a
    /// timing race via wall-clock sleep.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn arm_next_rekey_conflict(&mut self, target: SidecarId, fake_actual: [u8; 32]) {
        self.test_rekey_conflict_hook = Some((target, fake_actual));
    }

    /// Test-only: symmetric counterpart of `arm_next_rekey_conflict` for
    /// the `remove_if_cid_matches` path. Forces the NEXT
    /// `remove_if_cid_matches` call targeting `target` to return
    /// `Conflict { actual: fake_actual }` without mutating the entry.
    ///
    /// Lets the Case C STAGE 2 test exercise the post-STAGE-1 CAS
    /// conflict + compensating-undo flow without the boundary verify
    /// pre-empting it.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn arm_next_remove_if_cid_matches_conflict(
        &mut self,
        target: SidecarId,
        fake_actual: [u8; 32],
    ) {
        self.test_remove_conflict_hook = Some((target, fake_actual));
    }
}

pub(crate) mod hex_cid {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(cid: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(cid))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
        <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
            serde::de::Error::custom(format!("expected 32-byte hex CID, got {}", s.len() / 2))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_entry(cid: [u8; 32]) -> ContentIndexEntry {
        ContentIndexEntry {
            sidecar_id: SidecarId::new(),
            cid,
            file_name: "hello.txt".into(),
            size_bytes: 42,
            stored_at_ms: 1_700_000_000_000,
            sensitivity: Sensitivity::Private,
            replication_tier: ReplicationTier::Default,
            licensed: false,
            archived: false,
            pinned: false,
            backup: false,
            origin: None,
            kind: ContentKind::Leaf,
        }
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let dir = tempdir().unwrap();
        let idx = ContentIndex::load(dir.path());
        assert!(idx.entries.is_empty());
    }

    #[test]
    fn save_then_load_roundtrips_entries() {
        let dir = tempdir().unwrap();
        let entry = sample_entry([0xAA; 32]);

        let mut idx = ContentIndex::load(dir.path());
        idx.entries.insert(entry.sidecar_id, entry.clone());
        idx.save();

        let reloaded = ContentIndex::load(dir.path());
        assert_eq!(reloaded.entries.len(), 1);
        assert_eq!(reloaded.entries.get(&entry.sidecar_id), Some(&entry));
    }

    #[test]
    fn load_malformed_json_returns_empty() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(INDEX_FILE), b"{ not valid json").unwrap();
        let idx = ContentIndex::load(dir.path());
        assert!(idx.entries.is_empty());
    }

    #[test]
    fn load_wrong_version_returns_empty() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join(INDEX_FILE),
            br#"{"version": 99, "entries": []}"#,
        )
        .unwrap();
        let idx = ContentIndex::load(dir.path());
        assert!(idx.entries.is_empty());
    }

    #[test]
    fn insert_adds_entry_and_returns_true() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xBB; 32]);
        let id = entry.sidecar_id;
        assert!(idx.insert(entry.clone()));
        assert_eq!(idx.get(&id), Some(&entry));
    }

    #[test]
    fn corrupt_content_index_freezes_in_place_no_orphan() {
        // ZEB-986: a corrupt content-index must NOT be quarantined-and-healed —
        // overwriting it with an empty index would orphan every stored blob. It is
        // preserved untouched and writes are frozen (degraded read-only).
        let dir = tempdir().unwrap();
        let p = dir.path().join(INDEX_FILE);
        std::fs::write(&p, b"{ corrupt").unwrap();

        let mut idx = ContentIndex::load(dir.path());
        assert!(
            idx.entries.is_empty(),
            "corrupt index starts empty in-memory"
        );
        assert_eq!(
            std::fs::read(&p).unwrap(),
            b"{ corrupt",
            "file preserved untouched (no quarantine under FreezeInPlace)"
        );
        assert!(
            !dir.path().join(format!("{INDEX_FILE}.corrupt-0")).exists(),
            "no sidecar created"
        );

        // The next mutation's save() must be a frozen no-op: the file stays the
        // original corrupt bytes, so nothing is orphaned.
        idx.insert(sample_entry([0xAB; 32]));
        assert_eq!(
            std::fs::read(&p).unwrap(),
            b"{ corrupt",
            "frozen: save did not overwrite, blobs not orphaned"
        );
    }

    #[test]
    fn insert_two_entries_with_same_cid_coexist() {
        // Pre-ZEB-164 this case was rejected via duplicate-CID guard. Now
        // each entry's identity is its sidecar_id, so two entries pointing
        // at the same CID are legal (symlink-style).
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());

        let mut alpha = sample_entry([0xCC; 32]);
        alpha.file_name = "Alpha".into();
        let mut beta = sample_entry([0xCC; 32]);
        beta.file_name = "Beta".into();

        assert_ne!(alpha.sidecar_id, beta.sidecar_id, "fresh ids must differ");
        assert!(idx.insert(alpha.clone()));
        assert!(idx.insert(beta.clone()));

        // Both entries are retrievable by their own sidecar_id.
        assert_eq!(idx.get(&alpha.sidecar_id).unwrap().file_name, "Alpha");
        assert_eq!(idx.get(&beta.sidecar_id).unwrap().file_name, "Beta");
    }

    #[test]
    fn insert_duplicate_sidecar_id_returns_false() {
        // Defense-in-depth: SidecarId collisions are practically impossible
        // for UUID v4, but the API still has to behave sensibly if a caller
        // re-uses one (e.g. tests cloning an entry verbatim).
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xCD; 32]);
        assert!(idx.insert(entry.clone()));
        assert!(!idx.insert(entry), "duplicate sidecar_id is rejected");
    }

    #[test]
    fn remove_returns_true_when_present_false_otherwise() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xDD; 32]);
        let id = entry.sidecar_id;
        idx.insert(entry);
        assert!(idx.remove(&id));
        assert!(!idx.remove(&id));
    }

    #[test]
    fn set_archived_flips_flag_and_reports_change() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xEE; 32]);
        let id = entry.sidecar_id;
        idx.insert(entry);

        assert!(idx.set_archived(&id, true)); // flipped
        assert!(idx.get(&id).unwrap().archived);
        assert!(!idx.set_archived(&id, true)); // idempotent
    }

    #[test]
    fn set_archived_missing_id_returns_false() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let bogus = SidecarId::new();
        assert!(!idx.set_archived(&bogus, true));
    }

    #[test]
    fn set_file_name_updates_name_and_reports_change() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xF1; 32]);
        let id = entry.sidecar_id;
        idx.insert(entry);

        assert!(idx.set_file_name(&id, "renamed.txt".into()));
        assert_eq!(idx.get(&id).unwrap().file_name, "renamed.txt");
        assert!(!idx.set_file_name(&id, "renamed.txt".into())); // idempotent
    }

    #[test]
    fn set_file_name_missing_id_returns_false() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let bogus = SidecarId::new();
        assert!(!idx.set_file_name(&bogus, "new.txt".into()));
    }

    #[test]
    fn set_file_name_persists_change() {
        let dir = tempdir().unwrap();
        let saved_id;
        {
            let mut idx = ContentIndex::load(dir.path());
            let entry = sample_entry([0xF2; 32]);
            saved_id = entry.sidecar_id;
            idx.insert(entry);
            assert!(idx.set_file_name(&saved_id, "persisted.txt".into()));
        }
        let reloaded = ContentIndex::load(dir.path());
        assert_eq!(
            reloaded.get(&saved_id).expect("entry persisted").file_name,
            "persisted.txt",
        );
    }

    #[test]
    fn set_replication_tier_counts_updated_entries() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let a = sample_entry([0x01; 32]);
        let b = sample_entry([0x02; 32]);
        let a_id = a.sidecar_id;
        let b_id = b.sidecar_id;
        idx.insert(a);
        idx.insert(b);

        let updated = idx.set_replication_tier(&[a_id, b_id], ReplicationTier::Ultra);
        assert_eq!(updated, 2);

        let again = idx.set_replication_tier(&[a_id, b_id], ReplicationTier::Ultra);
        assert_eq!(again, 0);

        let bogus = SidecarId::new();
        let with_missing = idx.set_replication_tier(&[a_id, bogus], ReplicationTier::Expendable);
        assert_eq!(with_missing, 1);
    }

    #[test]
    fn save_is_noop_on_empty_path() {
        // NodeState::default() constructs a ContentIndex with an empty
        // path. Ensure mutations on that degenerate state don't
        // accidentally write content-index.json into CWD.
        let mut idx = ContentIndex::load(Path::new(""));
        assert!(idx.insert(sample_entry([0xFE; 32])));
        // No file should have been created in CWD.
        assert!(!Path::new("content-index.json").exists());
    }

    #[test]
    fn save_persists_mutations() {
        let dir = tempdir().unwrap();
        let saved_id;
        {
            let mut idx = ContentIndex::load(dir.path());
            let a1 = sample_entry([0xA1; 32]);
            let a2 = sample_entry([0xA2; 32]);
            let a1_id = a1.sidecar_id;
            saved_id = a2.sidecar_id;
            idx.insert(a1);
            idx.insert(a2);
            idx.remove(&a1_id);
            assert!(idx.set_archived(&saved_id, true));
            assert_eq!(
                idx.set_replication_tier(&[saved_id], ReplicationTier::Ultra),
                1
            );
        }
        let reloaded = ContentIndex::load(dir.path());
        assert_eq!(reloaded.entries.len(), 1);
        let entry = reloaded.get(&saved_id).expect("saved entry persisted");
        assert!(entry.archived);
        assert_eq!(entry.replication_tier, ReplicationTier::Ultra);
    }

    #[test]
    fn set_pinned_flips_flag_and_reports_change() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xB1; 32]);
        let id = entry.sidecar_id;
        idx.insert(entry);

        assert!(idx.set_pinned(&id, true));
        assert!(idx.get(&id).unwrap().pinned);
        assert!(!idx.set_pinned(&id, true));
        assert!(idx.set_pinned(&id, false));
        assert!(!idx.get(&id).unwrap().pinned);
    }

    #[test]
    fn set_backup_flips_flag_persists_and_reports_change() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let id = {
            let mut idx = ContentIndex::load(&path);
            let entry = sample_entry([0xB2; 32]);
            let id = entry.sidecar_id;
            idx.insert(entry);
            assert!(idx.set_backup(&id, true));
            assert!(!idx.set_backup(&id, true), "same value is a no-op");
            id
        };
        let mut idx = ContentIndex::load(&path);
        assert!(idx.get(&id).unwrap().backup, "flag survives reload");
        assert!(
            !idx.set_backup(&SidecarId::new(), true),
            "unknown id is a no-op"
        );
    }

    /// Pre-ZEB-669 sidecars have no `backup` key — they must deserialize
    /// with the flag off (they were never flagged).
    #[test]
    fn backup_flag_defaults_false_on_legacy_entries() {
        let json = format!(
            r#"{{"sidecar_id":"{}","cid":"{}","file_name":"old.txt","size_bytes":1,
                "stored_at_ms":1,"sensitivity":"private","replication_tier":"default",
                "licensed":false,"archived":false}}"#,
            SidecarId::new(),
            "ab".repeat(32),
        );
        let entry: ContentIndexEntry = serde_json::from_str(&json).unwrap();
        assert!(!entry.backup);
        assert!(!entry.pinned);
    }

    /// Pre-ZEB-669-S4 sidecars have no `origin` key — they must deserialize
    /// as `None` (provenance is recorded at creation, never inferred).
    #[test]
    fn origin_defaults_none_on_legacy_entries() {
        let json = format!(
            r#"{{"sidecar_id":"{}","cid":"{}","file_name":"old.txt","size_bytes":1,
                "stored_at_ms":1,"sensitivity":"private","replication_tier":"default",
                "licensed":false,"archived":false}}"#,
            SidecarId::new(),
            "ab".repeat(32),
        );
        let entry: ContentIndexEntry = serde_json::from_str(&json).unwrap();
        assert!(entry.origin.is_none());
    }

    #[test]
    fn origin_round_trips_through_save_and_reload() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let mut entry = sample_entry([0xC4; 32]);
        entry.origin = Some(OriginInfo {
            kind: OriginKind::SelfIngest,
            introducer: None,
        });
        let id = entry.sidecar_id;
        idx.insert(entry);

        let reloaded = ContentIndex::load(dir.path());
        let got = reloaded.get(&id).expect("round-trips");
        assert_eq!(
            got.origin,
            Some(OriginInfo {
                kind: OriginKind::SelfIngest,
                introducer: None,
            })
        );
        // The wire spelling is camelCase — pin it so the frontend contract
        // can't drift silently.
        let json = serde_json::to_string(&got.origin).unwrap();
        assert_eq!(json, r#"{"kind":"selfIngest","introducer":null}"#);
    }

    #[test]
    fn set_pinned_missing_id_returns_false() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let bogus = SidecarId::new();
        assert!(!idx.set_pinned(&bogus, true));
    }

    #[test]
    fn save_persists_pin_mutations() {
        let dir = tempdir().unwrap();
        let entry_id;
        {
            let mut idx = ContentIndex::load(dir.path());
            let entry = sample_entry([0xB3; 32]);
            entry_id = entry.sidecar_id;
            idx.insert(entry);
            assert!(idx.set_pinned(&entry_id, true));
        }
        let reloaded = ContentIndex::load(dir.path());
        assert!(
            reloaded.get(&entry_id).expect("persisted").pinned,
            "pinned flag must survive save/load"
        );
    }

    #[test]
    fn save_persists_kind_field() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let mut entry = sample_entry([0xF0; 32]);
        entry.file_name = "Photos".into();
        entry.kind = ContentKind::Folder;
        let id = entry.sidecar_id;
        idx.insert(entry);

        let reloaded = ContentIndex::load(dir.path());
        let got = reloaded.get(&id).expect("round-trips");
        assert_eq!(got.kind, ContentKind::Folder);
        assert_eq!(got.file_name, "Photos");
    }

    #[test]
    fn rekey_atomically_replaces_cid_and_preserves_user_state() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());

        let mut entry = sample_entry([0x01; 32]);
        entry.file_name = "Folder".into();
        entry.kind = ContentKind::Folder;
        entry.pinned = true;
        entry.sensitivity = Sensitivity::Confidential;
        entry.replication_tier = ReplicationTier::Ultra;
        entry.licensed = true;
        entry.archived = true;
        let id = entry.sidecar_id;
        idx.insert(entry);

        let result = idx.rekey(&id, [0x01; 32], [0x02; 32], 999, 1234);
        assert!(result.is_ok());

        let after = idx
            .get(&id)
            .expect("entry still present under same sidecar_id");
        assert_eq!(after.cid, [0x02; 32], "cid updated");
        assert_eq!(after.size_bytes, 999, "size_bytes updated");
        assert_eq!(after.stored_at_ms, 1234, "stored_at_ms updated");
        // All seven user-state fields preserved.
        assert_eq!(after.file_name, "Folder");
        assert_eq!(after.kind, ContentKind::Folder);
        assert!(after.pinned);
        assert_eq!(after.sensitivity, Sensitivity::Confidential);
        assert_eq!(after.replication_tier, ReplicationTier::Ultra);
        assert!(after.licensed);
        assert!(after.archived);
    }

    #[test]
    fn rekey_missing_id_returns_old_missing() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let bogus = SidecarId::new();
        // OldMissing fires before the expected_old_cid check.
        assert_eq!(
            idx.rekey(&bogus, [0x00; 32], [0xEE; 32], 0, 0),
            Err(RekeyError::OldMissing)
        );
    }

    #[test]
    fn rekey_self_update_refreshes_size_and_stored_at() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xCC; 32]);
        let id = entry.sidecar_id;
        idx.insert(entry);

        // Self-update: expected_old_cid == new_cid == current cid.
        let result = idx.rekey(&id, [0xCC; 32], [0xCC; 32], 12345, 67890);
        assert!(result.is_ok());
        let after = idx.get(&id).expect("entry still present");
        assert_eq!(after.size_bytes, 12345);
        assert_eq!(after.stored_at_ms, 67890);
    }

    #[test]
    fn sidecar_id_new_produces_unique_values() {
        let a = SidecarId::new();
        let b = SidecarId::new();
        assert_ne!(
            a, b,
            "two SidecarId::new() calls must produce distinct values"
        );
    }

    #[test]
    fn sidecar_id_round_trips_through_display_and_parse() {
        let original = SidecarId::new();
        let s = original.to_string();
        let parsed = SidecarId::parse_str(&s).expect("must parse own Display output");
        assert_eq!(parsed, original);
    }

    #[test]
    fn sidecar_id_parse_str_rejects_garbage() {
        assert!(SidecarId::parse_str("").is_err());
        assert!(SidecarId::parse_str("not-a-uuid").is_err());
        assert!(SidecarId::parse_str("8b4f7c2e-1a3d-4f5b-9c0e-XXXXXXXXXXXX").is_err());
    }

    #[test]
    fn sidecar_id_serializes_as_hyphenated_string() {
        let id = SidecarId::new();
        let json = serde_json::to_string(&id).expect("serialize");
        // Hyphenated UUID is 38 chars wrapped in quotes: "<36 chars>"
        assert_eq!(json.len(), 38, "got {json}");
        assert!(json.starts_with('"') && json.ends_with('"'));
        // Round-trip via deserialization too.
        let back: SidecarId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);
    }

    #[test]
    fn load_v1_without_sidecar_id_returns_empty() {
        // After ZEB-164, sidecar_id is a required field. v1 fixtures from
        // before this change fail deserialization → load() falls back to
        // empty (the existing malformed-JSON path). The two test sidecars
        // in dev are re-uploaded post-deploy.
        let dir = tempdir().unwrap();
        let pre_zeb164 = br#"{
            "version": 1,
            "entries": [{
                "cid": "aa11bb22cc33dd44ee55ff6677889900112233445566778899aabbccddeeff00",
                "file_name": "old.txt",
                "size_bytes": 42,
                "stored_at_ms": 1700000000000,
                "sensitivity": "private",
                "replication_tier": "default",
                "licensed": false,
                "archived": false,
                "pinned": false,
                "kind": "leaf"
            }]
        }"#;
        std::fs::write(dir.path().join(INDEX_FILE), pre_zeb164).unwrap();
        let idx = ContentIndex::load(dir.path());
        assert!(
            idx.entries.is_empty(),
            "legacy entry without sidecar_id must not load"
        );
    }

    #[test]
    fn rekey_target_cid_already_used_succeeds() {
        // Pre-ZEB-164 this would have returned RekeyError::Collision. Now
        // multiple entries can share a CID — the rekey target collision
        // case is no longer an error.
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());

        let keeper = sample_entry([0xAA; 32]);
        let other = sample_entry([0xBB; 32]);
        let keeper_id = keeper.sidecar_id;
        let other_id = other.sidecar_id;
        idx.insert(keeper);
        idx.insert(other);

        // other's current cid is [0xBB; 32]; rekey to [0xAA; 32] (which keeper already has).
        let result = idx.rekey(&other_id, [0xBB; 32], [0xAA; 32], 0, 0);
        assert!(result.is_ok());

        assert_eq!(idx.get(&keeper_id).unwrap().cid, [0xAA; 32]);
        assert_eq!(idx.get(&other_id).unwrap().cid, [0xAA; 32]);
    }

    #[test]
    fn rekey_with_stale_expected_cid_returns_conflict() {
        // CAS guard: if a concurrent rekey landed first, the entry's
        // current CID won't match what the caller walked from. Surface
        // the actual CID so a future retry path could rebuild from it.
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());

        let entry = sample_entry([0xAA; 32]);
        let id = entry.sidecar_id;
        idx.insert(entry);

        // Caller thinks current cid is [0xBB; 32], but it's actually [0xAA; 32].
        let result = idx.rekey(&id, [0xBB; 32], [0xCC; 32], 0, 0);
        assert_eq!(result, Err(RekeyError::Conflict { actual: [0xAA; 32] }));

        // Entry must be unchanged.
        let after = idx.get(&id).expect("entry still present");
        assert_eq!(after.cid, [0xAA; 32], "Conflict must not mutate the entry");
    }

    #[test]
    fn entries_for_cid_returns_all_matching() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());

        let a = sample_entry([0x10; 32]);
        let b = sample_entry([0x10; 32]);
        let c = sample_entry([0x20; 32]);
        idx.insert(a.clone());
        idx.insert(b.clone());
        idx.insert(c.clone());

        let mut matched: Vec<SidecarId> = idx
            .entries_for_cid(&[0x10; 32])
            .map(|e| e.sidecar_id)
            .collect();
        matched.sort_by_key(|id| id.to_string());
        let mut expected = vec![a.sidecar_id, b.sidecar_id];
        expected.sort_by_key(|id| id.to_string());
        assert_eq!(matched, expected);

        let lone: Vec<SidecarId> = idx
            .entries_for_cid(&[0x20; 32])
            .map(|e| e.sidecar_id)
            .collect();
        assert_eq!(lone, vec![c.sidecar_id]);

        let none: Vec<SidecarId> = idx
            .entries_for_cid(&[0x99; 32])
            .map(|e| e.sidecar_id)
            .collect();
        assert!(none.is_empty());
    }

    #[test]
    fn is_cid_pinned_by_any_or_joins_entries() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());

        let mut a = sample_entry([0x30; 32]);
        let mut b = sample_entry([0x30; 32]);
        let a_id = a.sidecar_id;
        let b_id = b.sidecar_id;
        a.pinned = false;
        b.pinned = false;
        idx.insert(a);
        idx.insert(b);

        assert!(!idx.is_cid_pinned_by_any(&[0x30; 32]), "neither pinned");

        idx.set_pinned(&a_id, true);
        assert!(idx.is_cid_pinned_by_any(&[0x30; 32]), "one pinned");

        idx.set_pinned(&b_id, true);
        assert!(idx.is_cid_pinned_by_any(&[0x30; 32]), "both pinned");

        idx.set_pinned(&a_id, false);
        assert!(idx.is_cid_pinned_by_any(&[0x30; 32]), "still one pinned");

        idx.set_pinned(&b_id, false);
        assert!(!idx.is_cid_pinned_by_any(&[0x30; 32]), "all unpinned");

        assert!(!idx.is_cid_pinned_by_any(&[0x99; 32]));
    }
}
