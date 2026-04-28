//! Mail manager: local persistence for the CAS Merkle mailbox.
//!
//! Phase 0 strategy: flatten the CAS tree into a JSON index + binary blobs.
//! The Merkle mailbox is the conceptual model and network wire format;
//! locally we use a pragmatic representation for fast reads/writes.
//!
//! Follows the `follows.rs` atomic-write pattern (write tmp → rename).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use harmony_mailbox::message::{HarmonyMessage, RecipientType, ADDRESS_HASH_LEN};
use serde::{Deserialize, Serialize};

// ── Public types (shared with Tauri commands) ────────────────────────

/// Whether a message body blob is locally cached.
///
/// `Local` — the HarmonyMessage blob exists at `{data_dir}/mail/blobs/{cid}.bin`.
///   Created by `receive_message` (live raw push) or `mark_body_received`
///   (lazy fetch).
/// `Pending` — a header-only entry registered by the Phase 2 walker. The
///   inbox entry exists but the body has not yet been fetched. Triggered to
///   fetch on first `MailReader` open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum BodyState {
    #[default]
    Local,
    Pending,
}

/// Outcome of a `register_header_only` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// Entry was new; caller should emit a `mail-received` event.
    Inserted { cid: String },
    /// A matching message_id already exists in inbox/trash/drafts; no change.
    Duplicate,
}

/// Outcome of a `receive_message` call.
///
/// Intentionally does NOT derive `PartialEq`/`Eq` — `EntryRecord` is not
/// an equality type, and call sites should match on the variant rather
/// than compare whole outcomes.
#[derive(Debug, Clone)]
pub enum ReceiveOutcome {
    /// A new entry was inserted; caller should emit `mail-received`.
    Inserted(EntryRecord),
    /// An existing Pending entry was promoted to Local; caller should
    /// NOT emit `mail-received` (the user already sees this row from
    /// the walker pass that registered it). Callers may still want the
    /// EntryRecord to update stale views.
    Promoted(EntryRecord),
}

/// A lightweight entry for inbox listing (mirrors MessageEntry semantics).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryRecord {
    pub message_cid: String,
    pub message_id: String,
    pub sender_address: String,
    pub timestamp: u64,
    pub subject_snippet: String,
    pub read: bool,
    #[serde(default)]
    pub body_state: BodyState,
}

/// Folder summary counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderCounts {
    pub total: u32,
    pub unread: u32,
}

/// Full message detail returned to the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailDetail {
    pub message_cid: String,
    pub message_id: String,
    pub subject: String,
    pub body: String,
    pub sender_address: String,
    pub recipients: Vec<RecipientDto>,
    pub timestamp: u64,
    pub attachments: Vec<AttachmentDto>,
    pub is_reply: bool,
    pub is_forward: bool,
    pub in_reply_to: Option<String>,
    pub body_state: BodyState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipientDto {
    pub address: String,
    pub recipient_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentDto {
    pub cid: String,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
}

// ── Internal persistence types ───────────────────────────────────────

const INDEX_VERSION: u32 = 1;
const MAX_SNIPPET_LEN: usize = 128;

/// The complete local mail state, persisted as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MailIndex {
    version: u32,
    folders: HashMap<String, FolderState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FolderState {
    entries: Vec<EntryRecord>,
}

impl Default for MailIndex {
    fn default() -> Self {
        let mut folders = HashMap::new();
        for name in &["inbox", "sent", "drafts", "trash"] {
            folders.insert(
                name.to_string(),
                FolderState {
                    entries: Vec::new(),
                },
            );
        }
        Self {
            version: INDEX_VERSION,
            folders,
        }
    }
}

// ── MailManager ──────────────────────────────────────────────────────

/// Manages local mail persistence. Thread-safe when wrapped in Arc<Mutex<>>.
pub struct MailManager {
    data_dir: PathBuf,
    owner_address: [u8; ADDRESS_HASH_LEN],
    index: MailIndex,
}

impl MailManager {
    /// Load existing mail state from disk, or create empty.
    pub fn load(data_dir: &Path, owner_address: [u8; ADDRESS_HASH_LEN]) -> Self {
        if let Err(e) = std::fs::create_dir_all(data_dir) {
            tracing::warn!(path = %data_dir.display(), error = %e, "failed to create mail dir");
        }
        if let Err(e) = std::fs::create_dir_all(data_dir.join("blobs")) {
            tracing::warn!(path = %data_dir.display(), error = %e, "failed to create mail blobs dir");
        }

        let index_path = data_dir.join("index.json");
        let mut index: MailIndex = if index_path.exists() {
            match std::fs::read(&index_path) {
                Ok(bytes) => match serde_json::from_slice(&bytes) {
                    Ok(idx) => idx,
                    Err(e) => {
                        tracing::warn!(path = %index_path.display(), error = %e, "corrupt mail index, starting fresh");
                        MailIndex::default()
                    }
                },
                Err(_) => MailIndex::default(),
            }
        } else {
            MailIndex::default()
        };

        // Ensure required folders exist even if the persisted index was
        // edited or written by an older version that omitted some.
        for name in &["inbox", "sent", "drafts", "trash"] {
            index
                .folders
                .entry(name.to_string())
                .or_insert_with(|| FolderState {
                    entries: Vec::new(),
                });
        }

        Self {
            data_dir: data_dir.to_path_buf(),
            owner_address,
            index,
        }
    }

    /// Process an inbound message (raw bytes from Zenoh subscription).
    ///
    /// Returns a [`ReceiveOutcome`] distinguishing a fresh insert from a
    /// Pending→Local promotion. Callers that drive UI notifications should
    /// emit on `Inserted` only — `Promoted` means the walker already
    /// surfaced the row to the user via `register_header_only`, so a
    /// second notification would be spurious.
    pub fn receive_message(&mut self, msg_bytes: &[u8]) -> Result<ReceiveOutcome, String> {
        let msg = HarmonyMessage::from_bytes(msg_bytes).map_err(|e| format!("parse: {e}"))?;

        // Compute CID (BLAKE3 hash of the raw bytes)
        let hash = blake3::hash(msg_bytes);
        let cid_hex = hex::encode(hash.as_bytes());

        // Scan for matching message_id across receive-side folders.
        // Classify the match (if any):
        //   - Local existing entry → duplicate, reject.
        //   - Pending existing entry → promote (write blob, flip state to Local,
        //     preserve current folder placement — handles user-moved-to-trash).
        //   - No match → fall through to the normal insert path below.
        //
        // Coexistence note: if a Local match exists anywhere, we reject even
        // if other folders contain stale Pending matches for the same
        // message_id. That indicates a prior bug (register_header_only also
        // dedups against Local) and is surfaced via the warning log below —
        // not silently healed.
        let msg_id_hex = hex::encode(msg.message_id);
        let mut has_pending_match = false;
        let mut has_local_match = false;
        for folder_name in ["inbox", "trash", "drafts"] {
            let Some(folder) = self.index.folders.get(folder_name) else {
                continue;
            };
            for entry in &folder.entries {
                if entry.message_id == msg_id_hex {
                    if entry.body_state == BodyState::Local {
                        has_local_match = true;
                    } else {
                        has_pending_match = true;
                        // Guard against silent bug-hiding: if a Pending entry
                        // carries a different CID than the just-hashed bytes,
                        // refuse the promotion rather than silently overwrite.
                        // Possible causes: walker bug, wire format skew, or
                        // (cosmically unlikely) BLAKE3 collision — all worth
                        // surfacing.
                        if entry.message_cid != cid_hex {
                            return Err(format!(
                                "cid mismatch on promotion: pending has {}, computed {}",
                                entry.message_cid, cid_hex
                            ));
                        }
                    }
                }
            }
        }
        if has_local_match {
            if has_pending_match {
                tracing::warn!(
                    %msg_id_hex,
                    "receive_message: message_id has both Local and Pending entries — \
                     rejecting as duplicate; Pending entries left stale (prior bug)"
                );
            }
            return Err("duplicate message".to_string());
        }

        if has_pending_match {
            // Write the blob FIRST so the `Local ⇒ blob exists` invariant holds
            // even if I/O fails mid-promotion. (Atomic: tmp + rename.)
            let blob_path = self.data_dir.join("blobs").join(format!("{cid_hex}.bin"));
            let tmp_blob = self
                .data_dir
                .join("blobs")
                .join(format!("{cid_hex}.bin.tmp"));
            std::fs::write(&tmp_blob, msg_bytes).map_err(|e| format!("write blob: {e}"))?;
            std::fs::rename(&tmp_blob, &blob_path).map_err(|e| format!("rename blob: {e}"))?;

            // Promote all matching Pending entries. Preserve folder placement.
            // CID equality already verified in the scan — no need to re-check.
            let mut promoted_entry: Option<EntryRecord> = None;
            for folder_name in ["inbox", "trash", "drafts"] {
                let Some(folder) = self.index.folders.get_mut(folder_name) else {
                    continue;
                };
                for entry in folder.entries.iter_mut() {
                    if entry.message_id == msg_id_hex && entry.body_state == BodyState::Pending {
                        entry.body_state = BodyState::Local;
                        if promoted_entry.is_none() {
                            promoted_entry = Some(entry.clone());
                        }
                    }
                }
            }

            self.save_index()?;
            // Safe unwrap: has_pending_match guaranteed at least one matching
            // Pending entry exists at scan time; no other mutator runs between
            // scan and promote (single-threaded &mut self borrow), and the
            // promote predicate matches the scan predicate exactly.
            let promoted =
                promoted_entry.expect("has_pending_match implies at least one promotion");
            return Ok(ReceiveOutcome::Promoted(promoted));
        }

        // No existing match — fall through to normal insert path.

        // Build entry record
        let snippet = truncate_snippet(&msg.subject);
        let entry = EntryRecord {
            message_cid: cid_hex.clone(),
            message_id: msg_id_hex,
            sender_address: hex::encode(msg.sender_address),
            timestamp: msg.timestamp,
            subject_snippet: snippet,
            read: false,
            body_state: BodyState::Local,
        };

        // Store blob (atomic: write tmp then rename, matching save_index pattern)
        let blob_path = self.data_dir.join("blobs").join(format!("{cid_hex}.bin"));
        let tmp_blob = self
            .data_dir
            .join("blobs")
            .join(format!("{cid_hex}.bin.tmp"));
        std::fs::write(&tmp_blob, msg_bytes).map_err(|e| format!("write blob: {e}"))?;
        std::fs::rename(&tmp_blob, &blob_path).map_err(|e| format!("rename blob: {e}"))?;

        // Prepend to inbox (newest first)
        let inbox = self.index.folders.get_mut("inbox").unwrap();
        inbox.entries.insert(0, entry.clone());

        self.save_index()?;
        Ok(ReceiveOutcome::Inserted(entry))
    }

    /// Register a header-only inbox entry from a walker-discovered MessageEntry.
    ///
    /// Inserts a `body_state: Pending` entry at position 0 of Inbox (the Phase 2
    /// walker only descends Inbox). Dedup scope: returns `Duplicate` if
    /// message_id is already present in inbox/trash/drafts (matches existing
    /// receive_message dedup window — deliberately excludes sent).
    pub fn register_header_only(
        &mut self,
        entry: harmony_mailbox::mailbox::MessageEntry,
    ) -> Result<RegisterOutcome, String> {
        let outcome = self.register_header_only_no_persist(entry)?;
        if matches!(outcome, RegisterOutcome::Inserted { .. }) {
            self.save_index()?;
        }
        Ok(outcome)
    }

    /// Like `register_header_only` but skips persistence — callers MUST invoke
    /// [`flush_index`] after a batch to durably commit the entries. Walkers
    /// use this on cold-start backfill so one disk write covers a full page
    /// (or full walk) instead of one per entry.
    pub fn register_header_only_no_persist(
        &mut self,
        entry: harmony_mailbox::mailbox::MessageEntry,
    ) -> Result<RegisterOutcome, String> {
        let cid_hex = hex::encode(entry.message_cid);
        let msg_id_hex = hex::encode(entry.message_id);

        let already_known = ["inbox", "trash", "drafts"]
            .into_iter()
            .filter_map(|name| self.index.folders.get(name))
            .any(|folder| folder.entries.iter().any(|e| e.message_id == msg_id_hex));
        if already_known {
            return Ok(RegisterOutcome::Duplicate);
        }

        // Defense-in-depth: harmony-mailbox enforces its own snippet cap on the
        // wire, but that constant can drift across version skew. Re-clamp here
        // so the client-side MAX_SNIPPET_LEN invariant holds regardless.
        let snippet = truncate_snippet(&entry.subject_snippet);

        let record = EntryRecord {
            message_cid: cid_hex.clone(),
            message_id: msg_id_hex,
            sender_address: hex::encode(entry.sender_address),
            timestamp: entry.timestamp,
            subject_snippet: snippet,
            read: entry.read,
            body_state: BodyState::Pending,
        };

        let inbox = self.index.folders.get_mut("inbox").unwrap();
        inbox.entries.insert(0, record);
        Ok(RegisterOutcome::Inserted { cid: cid_hex })
    }

    /// Persist the in-memory index. Pair with `register_header_only_no_persist`
    /// after a batch of header inserts.
    pub fn flush_index(&self) -> Result<(), String> {
        self.save_index()
    }

    /// Find the first entry across all folders whose `message_cid` matches.
    /// Borrows from the in-memory index (no clone), so callers that only
    /// need a single field (e.g., `body_state` to decide a routing branch)
    /// can avoid the O(N) folder copy that `list_folder(..usize::MAX)`
    /// would do for the same lookup.
    pub fn entry_by_cid(&self, cid_hex: &str) -> Option<&EntryRecord> {
        ["inbox", "trash", "drafts", "sent"]
            .iter()
            .filter_map(|name| self.index.folders.get(*name))
            .flat_map(|folder| folder.entries.iter())
            .find(|e| e.message_cid == cid_hex)
    }

    /// Verify bytes hash to cid_hex, write blob, transition matching
    /// Pending entries to Local. No-op (returns Ok) if no Pending entry
    /// matches (e.g., entry already Local from a racing live push).
    pub fn mark_body_received(&mut self, cid_hex: &str, bytes: &[u8]) -> Result<(), String> {
        validate_hex(cid_hex)?;

        // Verify bytes hash to the claimed CID.
        let computed = hex::encode(blake3::hash(bytes).as_bytes());
        if computed != cid_hex {
            return Err(format!(
                "hash mismatch: claimed {cid_hex}, computed {computed}"
            ));
        }

        // Pre-scan immutably: is there anything to promote? If not, return
        // before any filesystem side effects — live receive_message already
        // handled it, writing a stale blob would be wasted I/O.
        //
        // Note: multiple matches across folders are possible (e.g., a stale
        // Pending in trash and a fresh Pending in inbox referencing the same
        // body). All matches will be promoted — they all reference the same
        // blob, so making every reference resolvable is intentional.
        let has_pending = ["inbox", "trash", "drafts"]
            .iter()
            .filter_map(|name| self.index.folders.get(*name))
            .flat_map(|folder| folder.entries.iter())
            .any(|e| e.message_cid == cid_hex && e.body_state == BodyState::Pending);

        if !has_pending {
            return Ok(());
        }

        // Write the blob BEFORE mutating in-memory state, so the invariant
        // `state == Local ⇒ blob exists on disk` holds even if I/O fails
        // mid-way. (Atomic: tmp + rename.)
        let blob_path = self.data_dir.join("blobs").join(format!("{cid_hex}.bin"));
        let tmp_blob = self
            .data_dir
            .join("blobs")
            .join(format!("{cid_hex}.bin.tmp"));
        std::fs::write(&tmp_blob, bytes).map_err(|e| format!("write blob: {e}"))?;
        std::fs::rename(&tmp_blob, &blob_path).map_err(|e| format!("rename blob: {e}"))?;

        // Blob is durable; now flip every matching Pending entry to Local.
        for folder_name in ["inbox", "trash", "drafts"] {
            let Some(folder) = self.index.folders.get_mut(folder_name) else {
                continue;
            };
            for entry in folder.entries.iter_mut() {
                if entry.message_cid == cid_hex && entry.body_state == BodyState::Pending {
                    entry.body_state = BodyState::Local;
                }
            }
        }

        self.save_index()?;
        Ok(())
    }

    /// Store a sent message (already serialized).
    pub fn store_sent(&mut self, msg_bytes: &[u8], msg: &HarmonyMessage) -> Result<String, String> {
        let hash = blake3::hash(msg_bytes);
        let cid_hex = hex::encode(hash.as_bytes());

        // Store blob (atomic: write tmp then rename)
        let blob_path = self.data_dir.join("blobs").join(format!("{cid_hex}.bin"));
        let tmp_blob = self
            .data_dir
            .join("blobs")
            .join(format!("{cid_hex}.bin.tmp"));
        std::fs::write(&tmp_blob, msg_bytes).map_err(|e| format!("write blob: {e}"))?;
        std::fs::rename(&tmp_blob, &blob_path).map_err(|e| format!("rename blob: {e}"))?;

        // Add to sent folder
        let snippet = truncate_snippet(&msg.subject);
        let entry = EntryRecord {
            message_cid: cid_hex.clone(),
            message_id: hex::encode(msg.message_id),
            sender_address: hex::encode(msg.sender_address),
            timestamp: msg.timestamp,
            subject_snippet: snippet,
            read: true, // Sent messages are always "read"
            body_state: BodyState::Local,
        };

        let sent = self.index.folders.get_mut("sent").unwrap();
        sent.entries.insert(0, entry);

        self.save_index()?;
        Ok(cid_hex)
    }

    /// List entries in a folder with pagination.
    pub fn list_folder(&self, folder: &str, page: usize, per_page: usize) -> Vec<EntryRecord> {
        let Some(state) = self.index.folders.get(folder) else {
            return Vec::new();
        };
        let start = page * per_page;
        if start >= state.entries.len() {
            return Vec::new();
        }
        let end = (start + per_page).min(state.entries.len());
        state.entries[start..end].to_vec()
    }

    /// Get the full message detail by CID.
    pub fn get_message(&self, cid_hex: &str) -> Result<MailDetail, String> {
        validate_hex(cid_hex)?;
        let blob_path = self.data_dir.join("blobs").join(format!("{cid_hex}.bin"));
        let bytes = std::fs::read(&blob_path).map_err(|e| format!("read blob: {e}"))?;
        let msg = HarmonyMessage::from_bytes(&bytes).map_err(|e| format!("parse: {e}"))?;

        let recipients: Vec<RecipientDto> = msg
            .recipients
            .iter()
            .map(|r| RecipientDto {
                address: hex::encode(r.address_hash),
                recipient_type: match r.recipient_type {
                    RecipientType::To => "to".to_string(),
                    RecipientType::Cc => "cc".to_string(),
                    RecipientType::Bcc => "bcc".to_string(),
                },
            })
            .collect();

        let attachments: Vec<AttachmentDto> = msg
            .attachments
            .iter()
            .map(|a| AttachmentDto {
                cid: hex::encode(a.cid),
                filename: a.filename.clone(),
                mime_type: a.mime_type.clone(),
                size: a.size,
            })
            .collect();

        Ok(MailDetail {
            message_cid: cid_hex.to_string(),
            message_id: hex::encode(msg.message_id),
            subject: msg.subject,
            body: msg.body,
            sender_address: hex::encode(msg.sender_address),
            recipients,
            timestamp: msg.timestamp,
            attachments,
            is_reply: msg.flags.is_reply(),
            is_forward: msg.flags.is_forward(),
            in_reply_to: msg.in_reply_to.map(|id| hex::encode(id)),
            body_state: BodyState::Local,
        })
    }

    /// Mark a message as read/unread.
    /// When `folder` is provided, targets that specific folder (deterministic
    /// even when the same CID exists in multiple folders, e.g. self-send).
    pub fn mark_read(
        &mut self,
        cid_hex: &str,
        read: bool,
        folder: Option<&str>,
    ) -> Result<(), String> {
        validate_hex(cid_hex)?;
        if let Some(folder_name) = folder {
            let state = self
                .index
                .folders
                .get_mut(folder_name)
                .ok_or_else(|| format!("unknown folder: {folder_name}"))?;
            let entry = state
                .entries
                .iter_mut()
                .find(|e| e.message_cid == cid_hex)
                .ok_or("message not found in folder")?;
            if entry.read != read {
                entry.read = read;
                self.save_index()?;
            }
            return Ok(());
        }
        // Fallback: search all folders (non-deterministic for duplicate CIDs)
        for state in self.index.folders.values_mut() {
            if let Some(entry) = state.entries.iter_mut().find(|e| e.message_cid == cid_hex) {
                if entry.read != read {
                    entry.read = read;
                    self.save_index()?;
                }
                return Ok(());
            }
        }
        Err("message not found".to_string())
    }

    /// Move a message between folders.
    /// When `from_folder` is provided, only searches that folder (deterministic
    /// for duplicate CIDs across folders, e.g. self-send).
    pub fn move_message(
        &mut self,
        cid_hex: &str,
        from_folder: Option<&str>,
        to_folder: &str,
    ) -> Result<(), String> {
        validate_hex(cid_hex)?;
        if !self.index.folders.contains_key(to_folder) {
            return Err(format!("unknown folder: {to_folder}"));
        }

        // Find and remove from source folder
        let entry = if let Some(folder_name) = from_folder {
            let state = self
                .index
                .folders
                .get_mut(folder_name)
                .ok_or_else(|| format!("unknown folder: {folder_name}"))?;
            let pos = state
                .entries
                .iter()
                .position(|e| e.message_cid == cid_hex)
                .ok_or("message not found in folder")?;
            state.entries.remove(pos)
        } else {
            // Fallback: search all folders
            let mut found = None;
            for state in self.index.folders.values_mut() {
                if let Some(pos) = state.entries.iter().position(|e| e.message_cid == cid_hex) {
                    found = Some(state.entries.remove(pos));
                    break;
                }
            }
            found.ok_or("message not found")?
        };

        // Add to destination
        let dest = self.index.folders.get_mut(to_folder).unwrap();
        dest.entries.insert(0, entry);

        self.save_index()?;
        Ok(())
    }

    /// Permanently delete a message (removes blob + entry).
    /// When `folder` is provided, only searches that folder (deterministic
    /// for duplicate CIDs across folders).
    pub fn delete_message(&mut self, cid_hex: &str, folder: Option<&str>) -> Result<(), String> {
        validate_hex(cid_hex)?;

        // Remove the entry from its folder
        if let Some(folder_name) = folder {
            let state = self
                .index
                .folders
                .get_mut(folder_name)
                .ok_or_else(|| format!("unknown folder: {folder_name}"))?;
            let pos = state
                .entries
                .iter()
                .position(|e| e.message_cid == cid_hex)
                .ok_or("message not found in folder")?;
            state.entries.remove(pos);
        } else {
            // Fallback: search all folders
            let mut found = false;
            for state in self.index.folders.values_mut() {
                if let Some(pos) = state.entries.iter().position(|e| e.message_cid == cid_hex) {
                    state.entries.remove(pos);
                    found = true;
                    break;
                }
            }
            if !found {
                return Err("message not found".to_string());
            }
        }

        // Only remove blob if no remaining entry references this CID
        let still_referenced = self
            .index
            .folders
            .values()
            .any(|f| f.entries.iter().any(|e| e.message_cid == cid_hex));
        if !still_referenced {
            let blob_path = self.data_dir.join("blobs").join(format!("{cid_hex}.bin"));
            let _ = std::fs::remove_file(blob_path);
        }

        self.save_index()?;
        Ok(())
    }

    /// Get folder counts for all folders (derived from entries — always consistent).
    pub fn folder_counts(&self) -> HashMap<String, FolderCounts> {
        self.index
            .folders
            .iter()
            .map(|(name, state)| {
                (
                    name.clone(),
                    FolderCounts {
                        total: state.entries.len() as u32,
                        unread: state.entries.iter().filter(|e| !e.read).count() as u32,
                    },
                )
            })
            .collect()
    }

    /// Get the hex-encoded owner address.
    pub fn owner_address_hex(&self) -> String {
        hex::encode(self.owner_address)
    }

    // ── Private ──────────────────────────────────────────────────────

    /// Atomic save: write to tmp, then rename. Errors propagate to callers.
    fn save_index(&self) -> Result<(), String> {
        let index_path = self.data_dir.join("index.json");
        let tmp_path = self.data_dir.join("index.json.tmp");
        let json =
            serde_json::to_vec_pretty(&self.index).map_err(|e| format!("serialize index: {e}"))?;
        std::fs::write(&tmp_path, &json).map_err(|e| format!("write index: {e}"))?;
        std::fs::rename(&tmp_path, &index_path).map_err(|e| format!("replace index: {e}"))?;
        Ok(())
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Expected hex length of a BLAKE3 CID (32 bytes = 64 hex characters).
const CID_HEX_LEN: usize = 64;

/// Reject CIDs that aren't exactly 64 hex characters.
/// Prevents path traversal (non-hex), and DoS via oversized strings
/// that would cause large allocations and filesystem path bloat.
fn validate_hex(s: &str) -> Result<(), String> {
    if s.len() != CID_HEX_LEN || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid CID: expected 64 hex characters".to_string());
    }
    Ok(())
}

/// Truncate subject to MAX_SNIPPET_LEN bytes without splitting UTF-8.
fn truncate_snippet(subject: &str) -> String {
    if subject.len() <= MAX_SNIPPET_LEN {
        return subject.to_string();
    }
    let mut end = MAX_SNIPPET_LEN;
    while end > 0 && !subject.is_char_boundary(end) {
        end -= 1;
    }
    subject[..end].to_string()
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_mailbox::mailbox::MessageEntry;
    use harmony_mailbox::message::{
        unique_message_id, MailMessageType, MessageFlags, Recipient, RecipientType,
    };

    fn make_message_entry(message_id: [u8; 16], snippet: &str) -> MessageEntry {
        MessageEntry {
            message_cid: [0xAA; 32],
            message_id,
            sender_address: [0xBB; 16],
            timestamp: 1700000000,
            read: false,
            subject_snippet: snippet.to_string(),
        }
    }

    fn make_test_message(subject: &str, sender: [u8; 16]) -> HarmonyMessage {
        HarmonyMessage {
            version: 0x01,
            message_type: MailMessageType::Email,
            flags: MessageFlags::new(false, false, false),
            timestamp: 1744403200,
            message_id: unique_message_id(),
            in_reply_to: None,
            sender_address: sender,
            recipients: vec![Recipient {
                address_hash: [0xBB; ADDRESS_HASH_LEN],
                recipient_type: RecipientType::To,
            }],
            subject: subject.to_string(),
            body: "Hello, world!".to_string(),
            attachments: vec![],
        }
    }

    #[test]
    fn mail_manager_load_creates_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MailManager::load(dir.path(), [0xAA; 16]);
        assert_eq!(mgr.index.folders.len(), 4);
        assert_eq!(mgr.folder_counts()["inbox"].total, 0);
    }

    #[test]
    fn receive_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = MailManager::load(dir.path(), [0xBB; 16]);

        let msg = make_test_message("Test Subject", [0xAA; 16]);
        let bytes = msg.to_bytes().unwrap();
        let ReceiveOutcome::Inserted(entry) = mgr.receive_message(&bytes).unwrap() else {
            panic!("expected Inserted outcome on fresh receive");
        };

        assert_eq!(entry.subject_snippet, "Test Subject");
        assert!(!entry.read);

        let listed = mgr.list_folder("inbox", 0, 50);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].message_cid, entry.message_cid);

        let counts = mgr.folder_counts();
        assert_eq!(counts["inbox"].total, 1);
        assert_eq!(counts["inbox"].unread, 1);
    }

    #[test]
    fn dedup_by_message_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = MailManager::load(dir.path(), [0xBB; 16]);

        let msg = make_test_message("Dupe", [0xAA; 16]);
        let bytes = msg.to_bytes().unwrap();
        mgr.receive_message(&bytes).unwrap();
        let result = mgr.receive_message(&bytes);
        assert!(result.is_err());
        assert_eq!(mgr.folder_counts()["inbox"].total, 1);
    }

    #[test]
    fn mark_read_updates_counts() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = MailManager::load(dir.path(), [0xBB; 16]);

        let msg = make_test_message("Read Me", [0xAA; 16]);
        let bytes = msg.to_bytes().unwrap();
        let ReceiveOutcome::Inserted(entry) = mgr.receive_message(&bytes).unwrap() else {
            panic!("expected Inserted outcome on fresh receive");
        };

        assert_eq!(mgr.folder_counts()["inbox"].unread, 1);
        mgr.mark_read(&entry.message_cid, true, Some("inbox"))
            .unwrap();
        assert_eq!(mgr.folder_counts()["inbox"].unread, 0);
    }

    #[test]
    fn move_message_between_folders() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = MailManager::load(dir.path(), [0xBB; 16]);

        let msg = make_test_message("Move Me", [0xAA; 16]);
        let bytes = msg.to_bytes().unwrap();
        let ReceiveOutcome::Inserted(entry) = mgr.receive_message(&bytes).unwrap() else {
            panic!("expected Inserted outcome on fresh receive");
        };

        mgr.move_message(&entry.message_cid, Some("inbox"), "trash")
            .unwrap();
        assert_eq!(mgr.folder_counts()["inbox"].total, 0);
        assert_eq!(mgr.folder_counts()["trash"].total, 1);
    }

    #[test]
    fn get_message_returns_detail() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = MailManager::load(dir.path(), [0xBB; 16]);

        let msg = make_test_message("Detail Test", [0xAA; 16]);
        let bytes = msg.to_bytes().unwrap();
        let ReceiveOutcome::Inserted(entry) = mgr.receive_message(&bytes).unwrap() else {
            panic!("expected Inserted outcome on fresh receive");
        };

        let detail = mgr.get_message(&entry.message_cid).unwrap();
        assert_eq!(detail.subject, "Detail Test");
        assert_eq!(detail.body, "Hello, world!");
        assert_eq!(detail.sender_address, hex::encode([0xAA; 16]));
    }

    #[test]
    fn delete_message_removes_blob() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = MailManager::load(dir.path(), [0xBB; 16]);

        let msg = make_test_message("Delete Me", [0xAA; 16]);
        let bytes = msg.to_bytes().unwrap();
        let ReceiveOutcome::Inserted(entry) = mgr.receive_message(&bytes).unwrap() else {
            panic!("expected Inserted outcome on fresh receive");
        };

        let blob_path = dir
            .path()
            .join("blobs")
            .join(format!("{}.bin", entry.message_cid));
        assert!(blob_path.exists());

        mgr.delete_message(&entry.message_cid, Some("inbox"))
            .unwrap();
        assert!(!blob_path.exists());
        assert_eq!(mgr.folder_counts()["inbox"].total, 0);
    }

    #[test]
    fn persistence_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let cid;
        {
            let mut mgr = MailManager::load(dir.path(), [0xBB; 16]);
            let msg = make_test_message("Persist", [0xAA; 16]);
            let bytes = msg.to_bytes().unwrap();
            let ReceiveOutcome::Inserted(entry) = mgr.receive_message(&bytes).unwrap() else {
                panic!("expected Inserted outcome on fresh receive");
            };
            cid = entry.message_cid;
        }
        // Reload from disk
        let mgr = MailManager::load(dir.path(), [0xBB; 16]);
        assert_eq!(mgr.folder_counts()["inbox"].total, 1);
        let listed = mgr.list_folder("inbox", 0, 50);
        assert_eq!(listed[0].message_cid, cid);
    }

    #[test]
    fn store_sent() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = MailManager::load(dir.path(), [0xAA; 16]);

        let msg = make_test_message("Outbound", [0xAA; 16]);
        let bytes = msg.to_bytes().unwrap();
        let cid = mgr.store_sent(&bytes, &msg).unwrap();

        assert_eq!(mgr.folder_counts()["sent"].total, 1);
        let detail = mgr.get_message(&cid).unwrap();
        assert_eq!(detail.subject, "Outbound");
    }

    #[test]
    fn self_send_lands_in_both_folders() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = MailManager::load(dir.path(), [0xAA; 16]);

        let msg = make_test_message("Self Send", [0xAA; 16]);
        let bytes = msg.to_bytes().unwrap();

        // Store in sent first (mimics send_mail flow)
        let sent_cid = mgr.store_sent(&bytes, &msg).unwrap();

        // Then receive the same message (mimics Zenoh delivery to self)
        let ReceiveOutcome::Inserted(entry) = mgr.receive_message(&bytes).unwrap() else {
            panic!("expected Inserted outcome on fresh receive");
        };
        assert_eq!(entry.message_cid, sent_cid); // same CID

        assert_eq!(mgr.folder_counts()["sent"].total, 1);
        assert_eq!(mgr.folder_counts()["inbox"].total, 1);
        assert_eq!(mgr.folder_counts()["inbox"].unread, 1);
    }

    #[test]
    fn invalid_hex_cid_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = MailManager::load(dir.path(), [0xAA; 16]);

        // Path traversal
        assert!(mgr.get_message("../../../etc/passwd").is_err());
        // Non-hex characters
        assert!(mgr.get_message("not-hex!").is_err());
        // Empty
        assert!(mgr.get_message("").is_err());
        // Wrong length (too short)
        assert!(mgr.mark_read("aabbccdd", true, None).is_err());
        assert!(mgr.move_message("aabb", None, "inbox").is_err());
        assert!(mgr.delete_message("ff", None).is_err());
        // Wrong length (too long — DoS vector)
        let oversized = "aa".repeat(64); // 128 hex chars instead of 64
        assert!(mgr.get_message(&oversized).is_err());
    }

    #[test]
    fn list_folder_pagination() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = MailManager::load(dir.path(), [0xBB; 16]);

        // Insert 5 messages
        for i in 0..5u8 {
            let mut msg = make_test_message(&format!("Msg {i}"), [i + 1; 16]);
            msg.message_id = [i + 10; 16]; // unique IDs
            let bytes = msg.to_bytes().unwrap();
            mgr.receive_message(&bytes).unwrap();
        }

        // Page of 2: first page has 2 entries, second has 2, third has 1
        assert_eq!(mgr.list_folder("inbox", 0, 2).len(), 2);
        assert_eq!(mgr.list_folder("inbox", 1, 2).len(), 2);
        assert_eq!(mgr.list_folder("inbox", 2, 2).len(), 1);
        // Past the end returns empty
        assert_eq!(mgr.list_folder("inbox", 3, 2).len(), 0);
        // Empty folder
        assert_eq!(mgr.list_folder("drafts", 0, 50).len(), 0);
        // Unknown folder
        assert_eq!(mgr.list_folder("nonexistent", 0, 50).len(), 0);
    }

    #[test]
    fn folder_targeted_mark_read_self_send() {
        // When same CID exists in inbox + sent (self-send), mark_read with
        // a folder parameter deterministically targets the correct copy.
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = MailManager::load(dir.path(), [0xAA; 16]);

        let msg = make_test_message("Self Send", [0xAA; 16]);
        let bytes = msg.to_bytes().unwrap();

        // Store in sent first, then receive (mimics send_mail flow)
        mgr.store_sent(&bytes, &msg).unwrap();
        let ReceiveOutcome::Inserted(entry) = mgr.receive_message(&bytes).unwrap() else {
            panic!("expected Inserted outcome on fresh receive");
        };

        // Inbox copy is unread, sent copy is read
        assert_eq!(mgr.folder_counts()["inbox"].unread, 1);
        assert_eq!(mgr.folder_counts()["sent"].unread, 0);

        // Mark inbox copy as read — must not affect sent
        mgr.mark_read(&entry.message_cid, true, Some("inbox"))
            .unwrap();
        assert_eq!(mgr.folder_counts()["inbox"].unread, 0);
        assert_eq!(mgr.folder_counts()["sent"].unread, 0);

        // Move inbox copy to trash — sent copy must remain
        mgr.move_message(&entry.message_cid, Some("inbox"), "trash")
            .unwrap();
        assert_eq!(mgr.folder_counts()["inbox"].total, 0);
        assert_eq!(mgr.folder_counts()["sent"].total, 1);
        assert_eq!(mgr.folder_counts()["trash"].total, 1);

        // Delete from trash — blob kept because sent still references it
        mgr.delete_message(&entry.message_cid, Some("trash"))
            .unwrap();
        assert_eq!(mgr.folder_counts()["trash"].total, 0);
        // Blob still on disk (sent copy references it)
        let blob_path = dir
            .path()
            .join("blobs")
            .join(format!("{}.bin", entry.message_cid));
        assert!(blob_path.exists());

        // Delete from sent — now blob is removed
        mgr.delete_message(&entry.message_cid, Some("sent"))
            .unwrap();
        assert!(!blob_path.exists());
    }

    #[test]
    fn index_loads_old_format_with_local_default() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_dir = tmp.path().join("mail");
        std::fs::create_dir_all(&mail_dir).unwrap();
        std::fs::create_dir_all(mail_dir.join("blobs")).unwrap();

        // Old-format index: no body_state field on entries.
        let old_json = r#"{
            "version": 1,
            "folders": {
                "inbox": {
                    "entries": [{
                        "messageCid": "0011223344556677889900aabbccddeeff00112233445566778899aabbccddee",
                        "messageId": "00112233445566778899aabbccddeeff",
                        "senderAddress": "00112233445566778899aabbccddeeff",
                        "timestamp": 1700000000,
                        "subjectSnippet": "old entry",
                        "read": false
                    }]
                },
                "sent": { "entries": [] },
                "drafts": { "entries": [] },
                "trash": { "entries": [] }
            }
        }"#;
        std::fs::write(mail_dir.join("index.json"), old_json).unwrap();

        let mgr = MailManager::load(&mail_dir, [0u8; ADDRESS_HASH_LEN]);
        let inbox = mgr.list_folder("inbox", 0, 100);
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].body_state, BodyState::Local);
    }

    #[test]
    fn register_header_only_inserts_pending_inbox_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mgr = MailManager::load(&tmp.path().join("mail"), [0u8; ADDRESS_HASH_LEN]);

        let entry = make_message_entry([0x11; 16], "first message");
        let outcome = mgr.register_header_only(entry).unwrap();

        match outcome {
            RegisterOutcome::Inserted { ref cid } => {
                let inbox = mgr.list_folder("inbox", 0, 100);
                assert_eq!(inbox.len(), 1);
                assert_eq!(inbox[0].message_cid, *cid);
                assert_eq!(inbox[0].body_state, BodyState::Pending);
                assert_eq!(inbox[0].subject_snippet, "first message");
            }
            RegisterOutcome::Duplicate => panic!("expected Inserted, got Duplicate"),
        }
    }

    #[test]
    fn register_header_only_returns_duplicate_for_existing_inbox_message_id() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mgr = MailManager::load(&tmp.path().join("mail"), [0u8; ADDRESS_HASH_LEN]);

        // First, register via header-only (creates Pending in inbox).
        let entry1 = make_message_entry([0x22; 16], "first");
        mgr.register_header_only(entry1).unwrap();

        // Try to register again with the same message_id.
        let entry2 = make_message_entry([0x22; 16], "second-attempt");
        let outcome = mgr.register_header_only(entry2).unwrap();
        assert!(matches!(outcome, RegisterOutcome::Duplicate));

        // Inbox still has one entry, original snippet preserved.
        let inbox = mgr.list_folder("inbox", 0, 100);
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].subject_snippet, "first");
    }

    #[test]
    fn register_header_only_dedups_across_inbox_trash_drafts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mgr = MailManager::load(&tmp.path().join("mail"), [0u8; ADDRESS_HASH_LEN]);

        // Register, then move to trash.
        let entry = make_message_entry([0x33; 16], "msg");
        let outcome = mgr.register_header_only(entry).unwrap();
        let cid = match outcome {
            RegisterOutcome::Inserted { cid } => cid,
            _ => panic!(),
        };
        mgr.move_message(&cid, Some("inbox"), "trash").unwrap();

        // Re-attempting the same message_id should return Duplicate (not reappear in inbox).
        let entry2 = make_message_entry([0x33; 16], "msg");
        let outcome2 = mgr.register_header_only(entry2).unwrap();
        assert!(matches!(outcome2, RegisterOutcome::Duplicate));
        let inbox = mgr.list_folder("inbox", 0, 100);
        assert_eq!(inbox.len(), 0);
        let trash = mgr.list_folder("trash", 0, 100);
        assert_eq!(trash.len(), 1);
    }

    #[test]
    fn mark_body_received_promotes_pending_to_local() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_dir = tmp.path().join("mail");
        let mut mgr = MailManager::load(&mail_dir, [0u8; ADDRESS_HASH_LEN]);

        // Create a real HarmonyMessage so the bytes parse cleanly.
        let msg = make_test_message("subject", [0xCC; 16]);
        let bytes = msg.to_bytes().unwrap();
        let real_cid = blake3::hash(&bytes);
        let real_cid_hex = hex::encode(real_cid.as_bytes());

        // Register a pending entry whose message_cid matches the real bytes.
        let entry = MessageEntry {
            message_cid: *real_cid.as_bytes(),
            message_id: msg.message_id,
            sender_address: [0xCC; 16],
            timestamp: msg.timestamp,
            read: false,
            subject_snippet: "subject".to_string(),
        };
        mgr.register_header_only(entry).unwrap();

        // Promote it.
        mgr.mark_body_received(&real_cid_hex, &bytes).unwrap();

        // Inbox entry is now Local.
        let inbox = mgr.list_folder("inbox", 0, 100);
        assert_eq!(inbox[0].body_state, BodyState::Local);

        // Blob exists on disk.
        let blob_path = mail_dir.join("blobs").join(format!("{real_cid_hex}.bin"));
        assert!(blob_path.exists(), "blob should be written");
        assert_eq!(std::fs::read(&blob_path).unwrap(), bytes);
    }

    #[test]
    fn mark_body_received_rejects_hash_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_dir = tmp.path().join("mail");
        let mut mgr = MailManager::load(&mail_dir, [0u8; ADDRESS_HASH_LEN]);

        let claimed_cid_hex = hex::encode([0xDD; 32]);
        let wrong_bytes = b"not a harmony message";

        let result = mgr.mark_body_received(&claimed_cid_hex, wrong_bytes);
        assert!(
            result.is_err(),
            "should reject bytes that don't hash to the claimed CID"
        );

        // Rejection must not leak any filesystem side effects.
        let blob_path = mail_dir
            .join("blobs")
            .join(format!("{claimed_cid_hex}.bin"));
        assert!(
            !blob_path.exists(),
            "rejected bytes must not produce a blob file"
        );
        let tmp_blob = mail_dir
            .join("blobs")
            .join(format!("{claimed_cid_hex}.bin.tmp"));
        assert!(
            !tmp_blob.exists(),
            "rejected bytes must not leave a tmp file"
        );
    }

    #[test]
    fn mark_body_received_is_idempotent_for_local_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_dir = tmp.path().join("mail");
        let mut mgr = MailManager::load(&mail_dir, [0u8; ADDRESS_HASH_LEN]);

        let msg = make_test_message("s", [0xEE; 16]);
        let bytes = msg.to_bytes().unwrap();
        let cid_hex = hex::encode(blake3::hash(&bytes).as_bytes());

        // Receive once via the live raw path → entry is Local.
        mgr.receive_message(&bytes).unwrap();

        // Delete the blob the no-op path must not re-write it to prove that
        // it's truly a no-op (not silently doing work).
        let blob_path = mail_dir.join("blobs").join(format!("{cid_hex}.bin"));
        std::fs::remove_file(&blob_path).unwrap();

        // mark_body_received should be a no-op (returns Ok).
        mgr.mark_body_received(&cid_hex, &bytes).unwrap();

        // Blob was NOT re-written — this is the contract: don't touch the
        // filesystem when there's no Pending entry to promote.
        assert!(!blob_path.exists(), "no-op path must not re-write the blob");

        let inbox = mgr.list_folder("inbox", 0, 100);
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].body_state, BodyState::Local);
    }

    #[test]
    fn folder_counts_derived_from_entries() {
        // Verify folder_counts() reflects actual entry state, not stored fields.
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = MailManager::load(dir.path(), [0xBB; 16]);

        let msg = make_test_message("Derived", [0xAA; 16]);
        let bytes = msg.to_bytes().unwrap();
        mgr.receive_message(&bytes).unwrap();

        let counts = mgr.folder_counts();
        assert_eq!(counts["inbox"].total, 1);
        assert_eq!(counts["inbox"].unread, 1);
        assert_eq!(counts["sent"].total, 0);
        assert_eq!(counts["sent"].unread, 0);
    }

    #[test]
    fn receive_message_promotes_pending_to_local_preserving_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_dir = tmp.path().join("mail");
        let mut mgr = MailManager::load(&mail_dir, [0u8; ADDRESS_HASH_LEN]);

        // Build a real message and compute its real CID from serialized bytes.
        let msg = make_test_message("race", [0xFF; 16]);
        let bytes = msg.to_bytes().unwrap();
        let cid = blake3::hash(&bytes);
        let cid_hex = hex::encode(cid.as_bytes());

        // Walker registered a Pending entry first.
        let entry = MessageEntry {
            message_cid: *cid.as_bytes(),
            message_id: msg.message_id,
            sender_address: [0xFF; 16],
            timestamp: msg.timestamp,
            read: false,
            subject_snippet: "race".to_string(),
        };
        mgr.register_header_only(entry).unwrap();

        // User moved it to trash before the live push arrived.
        mgr.move_message(&cid_hex, Some("inbox"), "trash").unwrap();
        assert_eq!(
            mgr.list_folder("trash", 0, 100)[0].body_state,
            BodyState::Pending
        );

        // NOW the live raw push arrives.
        let result = mgr.receive_message(&bytes);

        // Should NOT error as duplicate; should promote in-place.
        assert!(
            matches!(result, Ok(ReceiveOutcome::Promoted(_))),
            "expected Promoted, got {result:?}"
        );

        // Entry stays in trash (folder preserved), body_state now Local.
        let inbox = mgr.list_folder("inbox", 0, 100);
        let trash = mgr.list_folder("trash", 0, 100);
        assert_eq!(inbox.len(), 0, "should not appear in inbox");
        assert_eq!(trash.len(), 1);
        assert_eq!(trash[0].body_state, BodyState::Local);

        // Blob written.
        let blob_path = mail_dir.join("blobs").join(format!("{cid_hex}.bin"));
        assert!(blob_path.exists());
    }

    #[test]
    fn receive_message_still_dedups_when_already_local() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mgr = MailManager::load(&tmp.path().join("mail"), [0u8; ADDRESS_HASH_LEN]);

        let msg = make_test_message("s", [0xAA; 16]);
        let bytes = msg.to_bytes().unwrap();
        mgr.receive_message(&bytes).unwrap();

        // Receiving the same message again should still be rejected as duplicate.
        let result = mgr.receive_message(&bytes);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("duplicate"));
    }
}
