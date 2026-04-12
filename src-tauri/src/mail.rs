//! Mail manager: local persistence for the CAS Merkle mailbox.
//!
//! Phase 0 strategy: flatten the CAS tree into a JSON index + binary blobs.
//! The Merkle mailbox is the conceptual model and network wire format;
//! locally we use a pragmatic representation for fast reads/writes.
//!
//! Follows the `follows.rs` atomic-write pattern (write tmp → rename).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use harmony_mail::message::{
    HarmonyMessage, RecipientType, ADDRESS_HASH_LEN,
};
use serde::{Deserialize, Serialize};

// ── Public types (shared with Tauri commands) ────────────────────────

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
        let index = if index_path.exists() {
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

        Self {
            data_dir: data_dir.to_path_buf(),
            owner_address,
            index,
        }
    }

    /// Process an inbound message (raw bytes from Zenoh subscription).
    /// Returns the entry record on success (for frontend notification).
    pub fn receive_message(&mut self, msg_bytes: &[u8]) -> Result<EntryRecord, String> {
        let msg = HarmonyMessage::from_bytes(msg_bytes).map_err(|e| format!("parse: {e}"))?;

        // Compute CID (BLAKE3 hash of the raw bytes)
        let hash = blake3::hash(msg_bytes);
        let cid_hex = hex::encode(hash.as_bytes());

        // Dedup by message_id in receive-side folders (inbox, trash, drafts).
        // Excludes "sent" so self-addressed messages can land in inbox after
        // store_sent already recorded the same message_id in sent.
        let msg_id_hex = hex::encode(msg.message_id);
        let already_received = ["inbox", "trash", "drafts"]
            .into_iter()
            .filter_map(|name| self.index.folders.get(name))
            .any(|folder| folder.entries.iter().any(|e| e.message_id == msg_id_hex));
        if already_received {
            return Err("duplicate message".to_string());
        }

        // Build entry record
        let snippet = truncate_snippet(&msg.subject);
        let entry = EntryRecord {
            message_cid: cid_hex.clone(),
            message_id: msg_id_hex,
            sender_address: hex::encode(msg.sender_address),
            timestamp: msg.timestamp,
            subject_snippet: snippet,
            read: false,
        };

        // Store blob (atomic: write tmp then rename, matching save_index pattern)
        let blob_path = self.data_dir.join("blobs").join(format!("{cid_hex}.bin"));
        let tmp_blob = self.data_dir.join("blobs").join(format!("{cid_hex}.bin.tmp"));
        std::fs::write(&tmp_blob, msg_bytes)
            .map_err(|e| format!("write blob: {e}"))?;
        std::fs::rename(&tmp_blob, &blob_path)
            .map_err(|e| format!("rename blob: {e}"))?;

        // Prepend to inbox (newest first)
        let inbox = self.index.folders.get_mut("inbox").unwrap();
        inbox.entries.insert(0, entry.clone());

        self.save_index()?;
        Ok(entry)
    }

    /// Store a sent message (already serialized).
    pub fn store_sent(
        &mut self,
        msg_bytes: &[u8],
        msg: &HarmonyMessage,
    ) -> Result<String, String> {
        let hash = blake3::hash(msg_bytes);
        let cid_hex = hex::encode(hash.as_bytes());

        // Store blob (atomic: write tmp then rename)
        let blob_path = self.data_dir.join("blobs").join(format!("{cid_hex}.bin"));
        let tmp_blob = self.data_dir.join("blobs").join(format!("{cid_hex}.bin.tmp"));
        std::fs::write(&tmp_blob, msg_bytes)
            .map_err(|e| format!("write blob: {e}"))?;
        std::fs::rename(&tmp_blob, &blob_path)
            .map_err(|e| format!("rename blob: {e}"))?;

        // Add to sent folder
        let snippet = truncate_snippet(&msg.subject);
        let entry = EntryRecord {
            message_cid: cid_hex.clone(),
            message_id: hex::encode(msg.message_id),
            sender_address: hex::encode(msg.sender_address),
            timestamp: msg.timestamp,
            subject_snippet: snippet,
            read: true, // Sent messages are always "read"
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
        })
    }

    /// Mark a message as read/unread.
    /// When `folder` is provided, targets that specific folder (deterministic
    /// even when the same CID exists in multiple folders, e.g. self-send).
    pub fn mark_read(&mut self, cid_hex: &str, read: bool, folder: Option<&str>) -> Result<(), String> {
        validate_hex(cid_hex)?;
        if let Some(folder_name) = folder {
            let state = self.index.folders.get_mut(folder_name)
                .ok_or_else(|| format!("unknown folder: {folder_name}"))?;
            let entry = state.entries.iter_mut().find(|e| e.message_cid == cid_hex)
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
    pub fn move_message(&mut self, cid_hex: &str, from_folder: Option<&str>, to_folder: &str) -> Result<(), String> {
        validate_hex(cid_hex)?;
        if !self.index.folders.contains_key(to_folder) {
            return Err(format!("unknown folder: {to_folder}"));
        }

        // Find and remove from source folder
        let entry = if let Some(folder_name) = from_folder {
            let state = self.index.folders.get_mut(folder_name)
                .ok_or_else(|| format!("unknown folder: {folder_name}"))?;
            let pos = state.entries.iter().position(|e| e.message_cid == cid_hex)
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
            let state = self.index.folders.get_mut(folder_name)
                .ok_or_else(|| format!("unknown folder: {folder_name}"))?;
            let pos = state.entries.iter().position(|e| e.message_cid == cid_hex)
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
        let json = serde_json::to_vec_pretty(&self.index)
            .map_err(|e| format!("serialize index: {e}"))?;
        std::fs::write(&tmp_path, &json)
            .map_err(|e| format!("write index: {e}"))?;
        std::fs::rename(&tmp_path, &index_path)
            .map_err(|e| format!("replace index: {e}"))?;
        Ok(())
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Reject non-hex CIDs to prevent path traversal via IPC input.
fn validate_hex(s: &str) -> Result<(), String> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid hex CID".to_string());
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
    use harmony_mail::message::{
        unique_message_id, MailMessageType, MessageFlags, Recipient, RecipientType,
    };

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
        let entry = mgr.receive_message(&bytes).unwrap();

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
        let entry = mgr.receive_message(&bytes).unwrap();

        assert_eq!(mgr.folder_counts()["inbox"].unread, 1);
        mgr.mark_read(&entry.message_cid, true, Some("inbox")).unwrap();
        assert_eq!(mgr.folder_counts()["inbox"].unread, 0);
    }

    #[test]
    fn move_message_between_folders() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = MailManager::load(dir.path(), [0xBB; 16]);

        let msg = make_test_message("Move Me", [0xAA; 16]);
        let bytes = msg.to_bytes().unwrap();
        let entry = mgr.receive_message(&bytes).unwrap();

        mgr.move_message(&entry.message_cid, Some("inbox"), "trash").unwrap();
        assert_eq!(mgr.folder_counts()["inbox"].total, 0);
        assert_eq!(mgr.folder_counts()["trash"].total, 1);
    }

    #[test]
    fn get_message_returns_detail() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = MailManager::load(dir.path(), [0xBB; 16]);

        let msg = make_test_message("Detail Test", [0xAA; 16]);
        let bytes = msg.to_bytes().unwrap();
        let entry = mgr.receive_message(&bytes).unwrap();

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
        let entry = mgr.receive_message(&bytes).unwrap();

        let blob_path = dir.path().join("blobs").join(format!("{}.bin", entry.message_cid));
        assert!(blob_path.exists());

        mgr.delete_message(&entry.message_cid, Some("inbox")).unwrap();
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
            let entry = mgr.receive_message(&bytes).unwrap();
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
        let entry = mgr.receive_message(&bytes).unwrap();
        assert_eq!(entry.message_cid, sent_cid); // same CID

        assert_eq!(mgr.folder_counts()["sent"].total, 1);
        assert_eq!(mgr.folder_counts()["inbox"].total, 1);
        assert_eq!(mgr.folder_counts()["inbox"].unread, 1);
    }

    #[test]
    fn invalid_hex_cid_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = MailManager::load(dir.path(), [0xAA; 16]);

        assert!(mgr.get_message("../../../etc/passwd").is_err());
        assert!(mgr.get_message("not-hex!").is_err());
        assert!(mgr.get_message("").is_err());
        assert!(mgr.mark_read("zzzz", true, None).is_err());
        assert!(mgr.move_message("a/b", None, "inbox").is_err());
        assert!(mgr.delete_message("..", None).is_err());
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
        let entry = mgr.receive_message(&bytes).unwrap();

        // Inbox copy is unread, sent copy is read
        assert_eq!(mgr.folder_counts()["inbox"].unread, 1);
        assert_eq!(mgr.folder_counts()["sent"].unread, 0);

        // Mark inbox copy as read — must not affect sent
        mgr.mark_read(&entry.message_cid, true, Some("inbox")).unwrap();
        assert_eq!(mgr.folder_counts()["inbox"].unread, 0);
        assert_eq!(mgr.folder_counts()["sent"].unread, 0);

        // Move inbox copy to trash — sent copy must remain
        mgr.move_message(&entry.message_cid, Some("inbox"), "trash").unwrap();
        assert_eq!(mgr.folder_counts()["inbox"].total, 0);
        assert_eq!(mgr.folder_counts()["sent"].total, 1);
        assert_eq!(mgr.folder_counts()["trash"].total, 1);

        // Delete from trash — blob kept because sent still references it
        mgr.delete_message(&entry.message_cid, Some("trash")).unwrap();
        assert_eq!(mgr.folder_counts()["trash"].total, 0);
        // Blob still on disk (sent copy references it)
        let blob_path = dir.path().join("blobs").join(format!("{}.bin", entry.message_cid));
        assert!(blob_path.exists());

        // Delete from sent — now blob is removed
        mgr.delete_message(&entry.message_cid, Some("sent")).unwrap();
        assert!(!blob_path.exists());
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
}
