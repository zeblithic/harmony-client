//! Mail module: CAS tree walking, flat file cache, Tauri commands.
//!
//! Provides the client-side mail receive path:
//! - MailState: holds the latest known root CID for our mailbox
//! - cached_fetch: disk-cached CAS block fetcher
//! - get_inbox: tree walk returning inbox entries
//! - get_mail_message: fetch and deserialize a full HarmonyMessage

use std::path::{Path, PathBuf};

use harmony_mailbox::{FolderKind, MailFolder, MailPage, MailRoot, EMPTY_CID};
use harmony_mailbox::message::HarmonyMessage;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};

use crate::event_loop::FetchRequest;

/// Client-side mail state.
pub struct MailState {
    /// Latest known root CID for our mailbox (set by Zenoh subscription).
    pub root_cid: Option<[u8; 32]>,
    /// Local disk cache directory for fetched CAS blocks.
    pub cache_dir: PathBuf,
}

impl MailState {
    /// Create a new MailState with cache at `~/.harmony/mail-cache/`.
    pub fn new() -> Self {
        let cache_dir = dirs_home()
            .join(".harmony")
            .join("mail-cache");
        Self {
            root_cid: None,
            cache_dir,
        }
    }
}

/// Resolve the user's home directory.
fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// Inbox entry for the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxEntry {
    /// Hex-encoded CID of the HarmonyMessage blob.
    pub message_cid: String,
    /// Hex-encoded sender address hash.
    pub sender_address: String,
    /// Unix timestamp in seconds.
    pub timestamp: u64,
    /// Subject snippet (up to 128 bytes).
    pub subject_snippet: String,
    /// Whether this message has been read.
    pub read: bool,
}

/// Full mail message for the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailMessage {
    pub subject: String,
    pub body: String,
    /// Hex-encoded sender address hash.
    pub sender_address: String,
    /// Unix timestamp in seconds.
    pub timestamp: u64,
    /// Hex-encoded recipient address hashes.
    pub recipients: Vec<String>,
    pub is_reply: bool,
    pub has_attachments: bool,
}

/// Fetch a CAS block by CID, using disk cache first.
///
/// 1. Check `cache_dir/{hex}.bin` on disk
/// 2. If miss, fetch via Zenoh through the event loop's FetchRequest channel
/// 3. Write to disk cache (atomic: tmp + rename)
pub async fn cached_fetch(
    cid: &[u8; 32],
    cache_dir: &Path,
    fetch_tx: &mpsc::Sender<FetchRequest>,
) -> Result<Vec<u8>, String> {
    let hex = hex::encode(cid);
    let path = cache_dir.join(format!("{hex}.bin"));

    // 1. Check local disk cache
    if let Ok(bytes) = tokio::fs::read(&path).await {
        return Ok(bytes);
    }

    // 2. Fetch via Zenoh
    let (reply_tx, reply_rx) = oneshot::channel();
    fetch_tx
        .send(FetchRequest {
            cid_hex: hex.clone(),
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;

    let bytes = reply_rx
        .await
        .map_err(|_| "event loop dropped fetch request".to_string())??;

    // 3. Write to cache (atomic: tmp + rename)
    if let Err(e) = tokio::fs::create_dir_all(cache_dir).await {
        tracing::warn!(error = %e, "failed to create mail cache dir");
    }
    let tmp = cache_dir.join(format!("{hex}.bin.tmp"));
    if tokio::fs::write(&tmp, &bytes).await.is_ok() {
        let _ = tokio::fs::rename(&tmp, &path).await;
    }

    Ok(bytes)
}

/// Walk the CAS Merkle tree and return inbox entries.
///
/// Tree walk: root CID -> MailRoot -> inbox folder CID -> MailFolder ->
/// head page CID -> MailPage -> entries.
pub async fn get_inbox_inner(
    root_cid: [u8; 32],
    cache_dir: &Path,
    fetch_tx: &mpsc::Sender<FetchRequest>,
) -> Result<Vec<InboxEntry>, String> {
    // Fetch and parse MailRoot
    let root_bytes = cached_fetch(&root_cid, cache_dir, fetch_tx).await?;
    let root = MailRoot::from_bytes(&root_bytes)
        .map_err(|e| format!("MailRoot parse error: {e}"))?;

    // Get inbox folder CID (index 0 = FolderKind::Inbox)
    let inbox_cid = root.folder_cid(FolderKind::Inbox);
    if *inbox_cid == EMPTY_CID {
        return Ok(Vec::new()); // Empty inbox
    }

    // Fetch and parse MailFolder
    let folder_bytes = cached_fetch(inbox_cid, cache_dir, fetch_tx).await?;
    let folder = MailFolder::from_bytes(&folder_bytes)
        .map_err(|e| format!("MailFolder parse error: {e}"))?;

    if folder.page_cids.is_empty() {
        return Ok(Vec::new());
    }

    // Fetch head page only (index 0 = most recent)
    let head_page_cid = &folder.page_cids[0];
    let page_bytes = cached_fetch(head_page_cid, cache_dir, fetch_tx).await?;
    let page = MailPage::from_bytes(&page_bytes)
        .map_err(|e| format!("MailPage parse error: {e}"))?;

    // Map entries to InboxEntry
    let entries = page
        .entries
        .iter()
        .map(|e| InboxEntry {
            message_cid: hex::encode(e.message_cid),
            sender_address: hex::encode(e.sender_address),
            timestamp: e.timestamp,
            subject_snippet: e.subject_snippet.clone(),
            read: e.read,
        })
        .collect();

    Ok(entries)
}

/// Fetch and deserialize a full HarmonyMessage by CID.
pub async fn get_mail_message_inner(
    message_cid_hex: &str,
    cache_dir: &Path,
    fetch_tx: &mpsc::Sender<FetchRequest>,
) -> Result<MailMessage, String> {
    // Parse hex CID
    let cid_bytes: [u8; 32] = hex::decode(message_cid_hex)
        .map_err(|e| format!("invalid CID hex: {e}"))?
        .try_into()
        .map_err(|v: Vec<u8>| format!("CID wrong length: {} bytes", v.len()))?;

    let msg_bytes = cached_fetch(&cid_bytes, cache_dir, fetch_tx).await?;
    let msg = HarmonyMessage::from_bytes(&msg_bytes)
        .map_err(|e| format!("HarmonyMessage parse error: {e}"))?;

    Ok(MailMessage {
        subject: msg.subject,
        body: msg.body,
        sender_address: hex::encode(msg.sender_address),
        timestamp: msg.timestamp,
        recipients: msg
            .recipients
            .iter()
            .map(|r| hex::encode(r.address_hash))
            .collect(),
        is_reply: msg.flags.is_reply(),
        has_attachments: msg.flags.has_attachments(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_mailbox::{
        MailFolder, MailPage, MailRoot, MessageEntry, MAILBOX_VERSION,
        message::{
            HarmonyMessage, MailMessageType, MessageFlags, Recipient, RecipientType,
            ADDRESS_HASH_LEN, CID_LEN, MESSAGE_ID_LEN, VERSION,
        },
    };

    /// Compute a BLAKE3 hash as a 32-byte CID (matches harmony-content).
    fn blake3_cid(data: &[u8]) -> [u8; 32] {
        let hash = blake3::hash(data);
        *hash.as_bytes()
    }

    /// Seed the cache directory with pre-built blobs and verify tree walk.
    #[tokio::test]
    async fn get_inbox_walks_tree() {
        let cache_dir = std::env::temp_dir().join(format!(
            "harmony-mail-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&cache_dir).unwrap();

        // Build a MessageEntry
        let entry = MessageEntry {
            message_cid: [0xDD; CID_LEN],
            message_id: [0x01; MESSAGE_ID_LEN],
            sender_address: [0xAA; ADDRESS_HASH_LEN],
            timestamp: 1744403200,
            subject_snippet: "Test subject".to_string(),
            read: false,
        };

        // Build MailPage
        let page = MailPage {
            version: MAILBOX_VERSION,
            next_page: None,
            entries: vec![entry],
        };
        let page_bytes = page.to_bytes().unwrap();
        let page_cid = blake3_cid(&page_bytes);

        // Build MailFolder
        let folder = MailFolder {
            version: MAILBOX_VERSION,
            message_count: 1,
            unread_count: 1,
            page_cids: vec![page_cid],
        };
        let folder_bytes = folder.to_bytes().unwrap();
        let folder_cid = blake3_cid(&folder_bytes);

        // Build MailRoot
        let mut root = MailRoot::new_empty([0xBB; ADDRESS_HASH_LEN], 1744403200);
        root.folders[0] = folder_cid; // inbox
        let root_bytes = root.to_bytes();
        let root_cid = blake3_cid(&root_bytes);

        // Write all blobs to cache
        std::fs::write(
            cache_dir.join(format!("{}.bin", hex::encode(root_cid))),
            &root_bytes,
        )
        .unwrap();
        std::fs::write(
            cache_dir.join(format!("{}.bin", hex::encode(folder_cid))),
            &folder_bytes,
        )
        .unwrap();
        std::fs::write(
            cache_dir.join(format!("{}.bin", hex::encode(page_cid))),
            &page_bytes,
        )
        .unwrap();

        // Create a dummy fetch_tx (should never be used since everything is cached)
        let (fetch_tx, _fetch_rx) = mpsc::channel::<FetchRequest>(1);

        let entries = get_inbox_inner(root_cid, &cache_dir, &fetch_tx)
            .await
            .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].subject_snippet, "Test subject");
        assert_eq!(entries[0].sender_address, hex::encode([0xAA; ADDRESS_HASH_LEN]));
        assert!(!entries[0].read);

        // Cleanup
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[tokio::test]
    async fn get_mail_message_deserializes() {
        let cache_dir = std::env::temp_dir().join(format!(
            "harmony-mail-msg-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&cache_dir).unwrap();

        let msg = HarmonyMessage {
            version: VERSION,
            message_type: MailMessageType::Email,
            flags: MessageFlags::new(true, true, false),
            timestamp: 1744403200,
            message_id: [0x01; MESSAGE_ID_LEN],
            in_reply_to: Some([0x02; MESSAGE_ID_LEN]),
            sender_address: [0xAA; ADDRESS_HASH_LEN],
            recipients: vec![Recipient {
                address_hash: [0xBB; ADDRESS_HASH_LEN],
                recipient_type: RecipientType::To,
            }],
            subject: "Test email".to_string(),
            body: "Hello from the test".to_string(),
            attachments: vec![],
        };

        let msg_bytes = msg.to_bytes().unwrap();
        let msg_cid = blake3_cid(&msg_bytes);
        let msg_cid_hex = hex::encode(msg_cid);

        std::fs::write(
            cache_dir.join(format!("{msg_cid_hex}.bin")),
            &msg_bytes,
        )
        .unwrap();

        let (fetch_tx, _fetch_rx) = mpsc::channel::<FetchRequest>(1);

        let mail_msg = get_mail_message_inner(&msg_cid_hex, &cache_dir, &fetch_tx)
            .await
            .unwrap();

        assert_eq!(mail_msg.subject, "Test email");
        assert_eq!(mail_msg.body, "Hello from the test");
        assert!(mail_msg.is_reply);
        assert!(mail_msg.has_attachments);
        assert_eq!(mail_msg.recipients.len(), 1);

        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[tokio::test]
    async fn cached_fetch_stores_and_retrieves() {
        let cache_dir = std::env::temp_dir().join(format!(
            "harmony-cache-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&cache_dir).unwrap();

        let data = b"test content for caching";
        let cid = blake3_cid(data);

        // Pre-seed the cache
        std::fs::write(
            cache_dir.join(format!("{}.bin", hex::encode(cid))),
            data,
        )
        .unwrap();

        // Create a dummy fetch_tx (should never be used)
        let (fetch_tx, _fetch_rx) = mpsc::channel::<FetchRequest>(1);

        let result = cached_fetch(&cid, &cache_dir, &fetch_tx).await.unwrap();
        assert_eq!(result, data);

        let _ = std::fs::remove_dir_all(&cache_dir);
    }
}
