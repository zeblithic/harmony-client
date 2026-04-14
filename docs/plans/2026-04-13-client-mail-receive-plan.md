# Client Mail Receive Path Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add native mail receive path to harmony-client: subscribe to Zenoh mailbox root CID updates, walk CAS Merkle tree, display inbox with message body viewing.

**Architecture:** Extract shared mailbox types into harmony-mailbox crate. Activate gateway Zenoh publishing. Client subscribes to root CID updates, walks tree via cached CAS block fetches, renders two-panel inbox UI.

**Tech Stack:** Rust (Tauri backend, harmony-mailbox), TypeScript/Svelte 5 (frontend), Zenoh (pub/sub), CAS (content-addressed storage)

---

## File Map

| File | Repo | Action | Responsibility |
|------|------|--------|---------------|
| `crates/harmony-mailbox/Cargo.toml` | harmony | Create | Shared crate manifest |
| `crates/harmony-mailbox/src/lib.rs` | harmony | Create | Public re-exports |
| `crates/harmony-mailbox/src/error.rs` | harmony | Create | `MailboxError` enum (wire-format variants only) |
| `crates/harmony-mailbox/src/mailbox.rs` | harmony | Create | `MailRoot`, `MailFolder`, `MailPage`, `MessageEntry`, `FolderKind` |
| `crates/harmony-mailbox/src/message.rs` | harmony | Create | `HarmonyMessage`, `MailMessageType`, `RecipientType`, `MessageFlags`, `Recipient`, `AttachmentRef` |
| `Cargo.toml` (workspace root) | harmony | Modify | Add `harmony-mailbox` to members + workspace deps |
| `crates/harmony-mail/Cargo.toml` | harmony | Modify | Add `harmony-mailbox` dependency |
| `crates/harmony-mail/src/error.rs` | harmony | Modify | Keep SMTP-only variants, add `#[from] MailboxError` |
| `crates/harmony-mail/src/mailbox.rs` | harmony | Modify | Replace with re-exports from `harmony-mailbox` |
| `crates/harmony-mail/src/message.rs` | harmony | Modify | Replace with re-exports, keep `unique_message_id()` locally |
| `crates/harmony-mail/src/server.rs` | harmony | Modify | Open Zenoh session, spawn publisher task, register queryables |
| `crates/harmony-mail/src/config.rs` | harmony | Modify | Add `ZenohConfig` section |
| `src-tauri/Cargo.toml` | harmony-client | Modify | Add `harmony-mailbox` dependency |
| `src-tauri/src/mail.rs` | harmony-client | Create | `MailState`, `cached_fetch`, `get_inbox`, `get_mail_message` |
| `src-tauri/src/lib.rs` | harmony-client | Modify | Add `mod mail`, register commands, add `MailState` |
| `src-tauri/src/event_loop.rs` | harmony-client | Modify | Add mail subscription + catch-up query |
| `src/lib/types.ts` | harmony-client | Modify | Add `InboxEntry`, `MailMessage` interfaces, extend `AppMode` |
| `src/lib/mail-service.ts` | harmony-client | Create | `MailService` class |
| `src/lib/components/MailMode.svelte` | harmony-client | Create | Two-panel layout container |
| `src/lib/components/InboxList.svelte` | harmony-client | Create | Inbox entry list |
| `src/lib/components/MailDetail.svelte` | harmony-client | Create | Message body display |
| `src/App.svelte` | harmony-client | Modify | Add mail mode, wire `MailService` |
| `src/lib/components/Layout.svelte` | harmony-client | Modify | Add mail mode layout slot |

---

## Task 1: Create `harmony-mailbox` shared crate (harmony repo)

**Files:** Create `crates/harmony-mailbox/Cargo.toml`, `crates/harmony-mailbox/src/lib.rs`, `crates/harmony-mailbox/src/error.rs`, `crates/harmony-mailbox/src/mailbox.rs`, `crates/harmony-mailbox/src/message.rs`. Modify workspace `Cargo.toml`.

- [ ] **Step 1: Create crate directory structure**

```bash
cd /Users/zeblith/work/zeblithic/harmony
mkdir -p crates/harmony-mailbox/src
```

- [ ] **Step 2: Create `crates/harmony-mailbox/Cargo.toml`**

```toml
[package]
name = "harmony-mailbox"
description = "Shared mailbox wire format types for Harmony mail"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
rust-version.workspace = true

[dependencies]
harmony-identity.workspace = true
thiserror.workspace = true

[dev-dependencies]
hex.workspace = true
```

- [ ] **Step 3: Create `crates/harmony-mailbox/src/error.rs`**

This contains ONLY the wire-format error variants extracted from `harmony-mail`'s `MailError`. The SMTP-only variants (`UnknownCommand`, `InvalidIdentity`) stay in `harmony-mail`.

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MailboxError {
    #[error("message too short: {len} bytes, minimum {min}")]
    MessageTooShort { len: usize, min: usize },

    #[error("unsupported message version: {0}")]
    UnsupportedVersion(u8),

    #[error("invalid message type: {0}")]
    InvalidMessageType(u8),

    #[error("invalid recipient type: {0}")]
    InvalidRecipientType(u8),

    #[error("subject too long: {len} bytes, maximum {max}")]
    SubjectTooLong { len: usize, max: usize },

    #[error("body too long: {len} bytes, maximum {max}")]
    BodyTooLong { len: usize, max: usize },

    #[error("too many recipients: {count}, maximum {max}")]
    TooManyRecipients { count: usize, max: usize },

    #[error("too many attachments: {count}, maximum {max}")]
    TooManyAttachments { count: usize, max: usize },

    #[error("truncated message: expected {expected} more bytes")]
    Truncated { expected: usize },

    #[error("invalid UTF-8 in {field}")]
    InvalidUtf8 { field: &'static str },

    #[error("invalid in_reply_to flag: {0}")]
    InvalidInReplyToFlag(u8),

    #[error("filename too long: {len} bytes, maximum 255")]
    FilenameTooLong { len: usize },

    #[error("mime type too long: {len} bytes, maximum 255")]
    MimeTypeTooLong { len: usize },

    #[error("trailing bytes after message: {count} extra bytes")]
    TrailingBytes { count: usize },

    #[error("{field} too long for u16 length prefix: {len} bytes (max 65535)")]
    StringTooLong { field: &'static str, len: usize },

    #[error("invalid magic bytes: expected {expected:?}, found {found:?}")]
    InvalidMagic { expected: [u8; 4], found: [u8; 4] },

    #[error("invalid flag value in {field}: {value:#04x}")]
    InvalidFlag { field: &'static str, value: u8 },

    #[error("too many entries: {count}, maximum {max}")]
    TooManyEntries { count: usize, max: usize },

    #[error("{field} too long: {len} bytes, maximum {max}")]
    FieldTooLong {
        field: &'static str,
        len: usize,
        max: usize,
    },
}

/// Validate that a string's length fits in a u16 for length-prefixed encoding.
pub fn check_u16_len(s: &str, field: &'static str) -> Result<u16, MailboxError> {
    u16::try_from(s.len()).map_err(|_| MailboxError::StringTooLong {
        field,
        len: s.len(),
    })
}
```

- [ ] **Step 4: Create `crates/harmony-mailbox/src/message.rs`**

Copy all types and constants from `harmony-mail/src/message.rs`, changing `MailError` to `MailboxError`. Exclude `unique_message_id()` (it depends on `blake3`). Include all deserialization helper functions (`read_u8`, `read_u16_be`, `read_u32_be`, `read_u64_be`, `read_fixed`, `read_utf8`) and all tests.

```rust
//! Harmony-native email message format.
//!
//! Binary wire format for HarmonyMessage -- the internal representation used
//! after SMTP ingress and before Reticulum/Zenoh transport.

use crate::error::MailboxError;

// ── Constants ──────────────────────────────────────────────────────────

/// Current wire format version.
pub const VERSION: u8 = 0x01;

/// Maximum subject length in bytes (RFC 2822 line limit).
pub const MAX_SUBJECT_LEN: usize = 998;

/// Maximum body length: 16 MiB.
pub const MAX_BODY_LEN: usize = 16 * 1024 * 1024;

/// Maximum number of recipients per message.
pub const MAX_RECIPIENTS: usize = 100;

/// Maximum number of attachment references per message.
pub const MAX_ATTACHMENTS: usize = 100;

/// Length of a Harmony address hash in bytes.
/// Re-exported from harmony-identity to avoid duplicate constants.
pub use harmony_identity::identity::ADDRESS_HASH_LENGTH as ADDRESS_HASH_LEN;

/// Length of a content identifier (CID) in bytes.
pub const CID_LEN: usize = 32;

/// Length of a message identifier in bytes.
pub const MESSAGE_ID_LEN: usize = 16;

// ── Minimum wire size ──────────────────────────────────────────────────
// version(1) + type(1) + flags(1) + timestamp(8) + message_id(16)
// + in_reply_to_flag(1) + sender_address(16) + recipient_count(1)
// + subject_len(2) + body_len(4) + attachment_count(1)
const MIN_HEADER_SIZE: usize = 1 + 1 + 1 + 8 + 16 + 1 + 16 + 1 + 2 + 4 + 1;

// ── Enums ──────────────────────────────────────────────────────────────

/// The type of mail message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailMessageType {
    /// Standard email message.
    Email = 0x00,
    /// Delivery/read receipt.
    Receipt = 0x01,
    /// Bounce notification.
    Bounce = 0x02,
}

impl MailMessageType {
    /// Decode from a single byte.
    pub fn from_u8(val: u8) -> Result<Self, MailboxError> {
        match val {
            0x00 => Ok(Self::Email),
            0x01 => Ok(Self::Receipt),
            0x02 => Ok(Self::Bounce),
            other => Err(MailboxError::InvalidMessageType(other)),
        }
    }
}

/// Recipient role within a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipientType {
    /// Primary recipient.
    To = 0x00,
    /// Carbon-copy recipient.
    Cc = 0x01,
    /// Blind carbon-copy recipient (stripped before delivery).
    Bcc = 0x02,
}

impl RecipientType {
    /// Decode from a single byte.
    pub fn from_u8(val: u8) -> Result<Self, MailboxError> {
        match val {
            0x00 => Ok(Self::To),
            0x01 => Ok(Self::Cc),
            0x02 => Ok(Self::Bcc),
            other => Err(MailboxError::InvalidRecipientType(other)),
        }
    }
}

// ── MessageFlags ───────────────────────────────────────────────────────

/// Bitfield flags for a message.
///
/// - Bit 0: has attachments
/// - Bit 1: is reply
/// - Bit 2: is forward
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageFlags(u8);

impl MessageFlags {
    /// Construct flags from individual booleans.
    pub fn new(has_attachments: bool, is_reply: bool, is_forward: bool) -> Self {
        let mut bits = 0u8;
        if has_attachments {
            bits |= 1 << 0;
        }
        if is_reply {
            bits |= 1 << 1;
        }
        if is_forward {
            bits |= 1 << 2;
        }
        Self(bits)
    }

    /// Construct from raw bits, masking off reserved bits 3-7.
    pub fn from_bits(bits: u8) -> Self {
        Self(bits & 0b0000_0111)
    }

    /// Return the raw bits.
    pub fn bits(self) -> u8 {
        self.0
    }

    /// Whether the message has attachments.
    pub fn has_attachments(self) -> bool {
        self.0 & (1 << 0) != 0
    }

    /// Whether the message is a reply.
    pub fn is_reply(self) -> bool {
        self.0 & (1 << 1) != 0
    }

    /// Whether the message is a forward.
    pub fn is_forward(self) -> bool {
        self.0 & (1 << 2) != 0
    }
}

// ── Recipient ──────────────────────────────────────────────────────────

/// A message recipient identified by address hash and role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipient {
    /// 128-bit Harmony address hash.
    pub address_hash: [u8; ADDRESS_HASH_LEN],
    /// Role of this recipient (To/Cc/Bcc).
    pub recipient_type: RecipientType,
}

// ── AttachmentRef ──────────────────────────────────────────────────────

/// Reference to an attachment stored in the content-addressed layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRef {
    /// Content identifier (32-byte hash).
    pub cid: [u8; CID_LEN],
    /// Original filename.
    pub filename: String,
    /// MIME type (e.g. "application/pdf").
    pub mime_type: String,
    /// Size in bytes.
    pub size: u64,
}

// ── HarmonyMessage ─────────────────────────────────────────────────────

/// A fully-decoded Harmony mail message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarmonyMessage {
    /// Wire format version (currently 0x01).
    pub version: u8,
    /// Message type (Email, Receipt, Bounce).
    pub message_type: MailMessageType,
    /// Bitfield flags.
    pub flags: MessageFlags,
    /// Unix timestamp in seconds.
    pub timestamp: u64,
    /// Unique message identifier.
    pub message_id: [u8; MESSAGE_ID_LEN],
    /// If this is a reply, the ID of the original message.
    pub in_reply_to: Option<[u8; MESSAGE_ID_LEN]>,
    /// Sender's Harmony address hash.
    pub sender_address: [u8; ADDRESS_HASH_LEN],
    /// List of recipients.
    pub recipients: Vec<Recipient>,
    /// Subject line (UTF-8).
    pub subject: String,
    /// Message body (UTF-8).
    pub body: String,
    /// Attachment references.
    pub attachments: Vec<AttachmentRef>,
}

impl HarmonyMessage {
    /// Serialize the message to its binary wire format.
    ///
    /// BCC recipients are **stripped** from the serialized output -- only To and
    /// Cc recipients appear in the wire format.
    pub fn to_bytes(&self) -> Result<Vec<u8>, MailboxError> {
        // Validate limits
        if self.subject.len() > MAX_SUBJECT_LEN {
            return Err(MailboxError::SubjectTooLong {
                len: self.subject.len(),
                max: MAX_SUBJECT_LEN,
            });
        }
        if self.body.len() > MAX_BODY_LEN {
            return Err(MailboxError::BodyTooLong {
                len: self.body.len(),
                max: MAX_BODY_LEN,
            });
        }
        if self.recipients.len() > MAX_RECIPIENTS {
            return Err(MailboxError::TooManyRecipients {
                count: self.recipients.len(),
                max: MAX_RECIPIENTS,
            });
        }
        if self.attachments.len() > MAX_ATTACHMENTS {
            return Err(MailboxError::TooManyAttachments {
                count: self.attachments.len(),
                max: MAX_ATTACHMENTS,
            });
        }

        for att in &self.attachments {
            if att.filename.len() > 255 {
                return Err(MailboxError::FilenameTooLong {
                    len: att.filename.len(),
                });
            }
            if att.mime_type.len() > 255 {
                return Err(MailboxError::MimeTypeTooLong {
                    len: att.mime_type.len(),
                });
            }
        }

        let mut buf = Vec::with_capacity(MIN_HEADER_SIZE);

        buf.push(self.version);
        buf.push(self.message_type as u8);
        buf.push(self.flags.bits());
        buf.extend_from_slice(&self.timestamp.to_be_bytes());
        buf.extend_from_slice(&self.message_id);
        match &self.in_reply_to {
            None => buf.push(0x00),
            Some(id) => {
                buf.push(0x01);
                buf.extend_from_slice(id);
            }
        }
        buf.extend_from_slice(&self.sender_address);
        let non_bcc: Vec<_> = self
            .recipients
            .iter()
            .filter(|r| r.recipient_type != RecipientType::Bcc)
            .collect();
        buf.push(non_bcc.len() as u8);
        for r in &non_bcc {
            buf.extend_from_slice(&r.address_hash);
            buf.push(r.recipient_type as u8);
        }
        buf.extend_from_slice(&(self.subject.len() as u16).to_be_bytes());
        buf.extend_from_slice(self.subject.as_bytes());
        buf.extend_from_slice(&(self.body.len() as u32).to_be_bytes());
        buf.extend_from_slice(self.body.as_bytes());
        buf.push(self.attachments.len() as u8);
        for att in &self.attachments {
            buf.extend_from_slice(&att.cid);
            buf.push(att.filename.len() as u8);
            buf.extend_from_slice(att.filename.as_bytes());
            buf.push(att.mime_type.len() as u8);
            buf.extend_from_slice(att.mime_type.as_bytes());
            buf.extend_from_slice(&att.size.to_be_bytes());
        }

        Ok(buf)
    }

    /// Deserialize a message from its binary wire format.
    pub fn from_bytes(data: &[u8]) -> Result<Self, MailboxError> {
        if data.len() < MIN_HEADER_SIZE {
            return Err(MailboxError::MessageTooShort {
                len: data.len(),
                min: MIN_HEADER_SIZE,
            });
        }

        let mut pos = 0;

        let version = data[pos];
        if version != VERSION {
            return Err(MailboxError::UnsupportedVersion(version));
        }
        pos += 1;

        let message_type = MailMessageType::from_u8(data[pos])?;
        pos += 1;

        let flags = MessageFlags::from_bits(data[pos]);
        pos += 1;

        let timestamp = read_u64_be(data, &mut pos)?;
        let message_id = read_fixed::<MESSAGE_ID_LEN>(data, &mut pos)?;

        let in_reply_to_flag = read_u8(data, &mut pos)?;
        let in_reply_to = match in_reply_to_flag {
            0x00 => None,
            0x01 => Some(read_fixed::<MESSAGE_ID_LEN>(data, &mut pos)?),
            other => return Err(MailboxError::InvalidInReplyToFlag(other)),
        };

        let sender_address = read_fixed::<ADDRESS_HASH_LEN>(data, &mut pos)?;

        let recipient_count = read_u8(data, &mut pos)? as usize;
        if recipient_count > MAX_RECIPIENTS {
            return Err(MailboxError::TooManyRecipients {
                count: recipient_count,
                max: MAX_RECIPIENTS,
            });
        }

        let mut recipients = Vec::with_capacity(recipient_count);
        for _ in 0..recipient_count {
            let address_hash = read_fixed::<ADDRESS_HASH_LEN>(data, &mut pos)?;
            let rtype = RecipientType::from_u8(read_u8(data, &mut pos)?)?;
            recipients.push(Recipient {
                address_hash,
                recipient_type: rtype,
            });
        }

        let subject_len = read_u16_be(data, &mut pos)? as usize;
        if subject_len > MAX_SUBJECT_LEN {
            return Err(MailboxError::SubjectTooLong {
                len: subject_len,
                max: MAX_SUBJECT_LEN,
            });
        }

        let subject = read_utf8(data, &mut pos, subject_len, "subject")?;

        let body_len = read_u32_be(data, &mut pos)? as usize;
        if body_len > MAX_BODY_LEN {
            return Err(MailboxError::BodyTooLong {
                len: body_len,
                max: MAX_BODY_LEN,
            });
        }

        let body = read_utf8(data, &mut pos, body_len, "body")?;

        let attachment_count = read_u8(data, &mut pos)? as usize;
        if attachment_count > MAX_ATTACHMENTS {
            return Err(MailboxError::TooManyAttachments {
                count: attachment_count,
                max: MAX_ATTACHMENTS,
            });
        }

        let mut attachments = Vec::with_capacity(attachment_count);
        for _ in 0..attachment_count {
            let cid = read_fixed::<CID_LEN>(data, &mut pos)?;
            let filename_len = read_u8(data, &mut pos)? as usize;
            let filename = read_utf8(data, &mut pos, filename_len, "filename")?;
            let mime_len = read_u8(data, &mut pos)? as usize;
            let mime_type = read_utf8(data, &mut pos, mime_len, "mime_type")?;
            let size = read_u64_be(data, &mut pos)?;
            attachments.push(AttachmentRef {
                cid,
                filename,
                mime_type,
                size,
            });
        }

        if pos != data.len() {
            return Err(MailboxError::TrailingBytes {
                count: data.len() - pos,
            });
        }

        Ok(HarmonyMessage {
            version,
            message_type,
            flags,
            timestamp,
            message_id,
            in_reply_to,
            sender_address,
            recipients,
            subject,
            body,
            attachments,
        })
    }
}

// ── Deserialization helpers ────────────────────────────────────────────

fn read_u8(data: &[u8], pos: &mut usize) -> Result<u8, MailboxError> {
    if *pos >= data.len() {
        return Err(MailboxError::Truncated { expected: 1 });
    }
    let val = data[*pos];
    *pos += 1;
    Ok(val)
}

fn read_u16_be(data: &[u8], pos: &mut usize) -> Result<u16, MailboxError> {
    let end = *pos + 2;
    if end > data.len() {
        return Err(MailboxError::Truncated {
            expected: end - data.len(),
        });
    }
    let val = u16::from_be_bytes(data[*pos..end].try_into().unwrap());
    *pos = end;
    Ok(val)
}

fn read_u32_be(data: &[u8], pos: &mut usize) -> Result<u32, MailboxError> {
    let end = *pos + 4;
    if end > data.len() {
        return Err(MailboxError::Truncated {
            expected: end - data.len(),
        });
    }
    let val = u32::from_be_bytes(data[*pos..end].try_into().unwrap());
    *pos = end;
    Ok(val)
}

fn read_u64_be(data: &[u8], pos: &mut usize) -> Result<u64, MailboxError> {
    let end = *pos + 8;
    if end > data.len() {
        return Err(MailboxError::Truncated {
            expected: end - data.len(),
        });
    }
    let val = u64::from_be_bytes(data[*pos..end].try_into().unwrap());
    *pos = end;
    Ok(val)
}

fn read_fixed<const N: usize>(data: &[u8], pos: &mut usize) -> Result<[u8; N], MailboxError> {
    let end = *pos + N;
    if end > data.len() {
        return Err(MailboxError::Truncated {
            expected: end - data.len(),
        });
    }
    let mut arr = [0u8; N];
    arr.copy_from_slice(&data[*pos..end]);
    *pos = end;
    Ok(arr)
}

fn read_utf8(
    data: &[u8],
    pos: &mut usize,
    len: usize,
    field: &'static str,
) -> Result<String, MailboxError> {
    let end = *pos + len;
    if end > data.len() {
        return Err(MailboxError::Truncated {
            expected: end - data.len(),
        });
    }
    let s =
        std::str::from_utf8(&data[*pos..end]).map_err(|_| MailboxError::InvalidUtf8 { field })?;
    *pos = end;
    Ok(s.to_owned())
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_type_roundtrip() {
        assert_eq!(
            MailMessageType::from_u8(0x00).unwrap(),
            MailMessageType::Email
        );
        assert_eq!(
            MailMessageType::from_u8(0x01).unwrap(),
            MailMessageType::Receipt
        );
        assert_eq!(
            MailMessageType::from_u8(0x02).unwrap(),
            MailMessageType::Bounce
        );
        assert!(MailMessageType::from_u8(0x03).is_err());
        assert!(MailMessageType::from_u8(0xFF).is_err());
    }

    #[test]
    fn recipient_type_roundtrip() {
        assert_eq!(RecipientType::from_u8(0x00).unwrap(), RecipientType::To);
        assert_eq!(RecipientType::from_u8(0x01).unwrap(), RecipientType::Cc);
        assert_eq!(RecipientType::from_u8(0x02).unwrap(), RecipientType::Bcc);
        assert!(RecipientType::from_u8(0x03).is_err());
        assert!(RecipientType::from_u8(0xFF).is_err());
    }

    #[test]
    fn flags_bitfield() {
        let f = MessageFlags::new(false, false, false);
        assert_eq!(f.bits(), 0);
        assert!(!f.has_attachments());
        assert!(!f.is_reply());
        assert!(!f.is_forward());

        let f = MessageFlags::new(true, false, false);
        assert_eq!(f.bits(), 0b001);
        assert!(f.has_attachments());

        let f = MessageFlags::new(false, true, false);
        assert_eq!(f.bits(), 0b010);
        assert!(f.is_reply());

        let f = MessageFlags::new(false, false, true);
        assert_eq!(f.bits(), 0b100);
        assert!(f.is_forward());

        let f = MessageFlags::new(true, true, true);
        assert_eq!(f.bits(), 0b111);

        for bits in 0..=0b111u8 {
            let f = MessageFlags::from_bits(bits);
            assert_eq!(f.bits(), bits);
        }

        let f = MessageFlags::from_bits(0xFF);
        assert_eq!(f.bits(), 0b111);
    }

    fn simple_message() -> HarmonyMessage {
        HarmonyMessage {
            version: VERSION,
            message_type: MailMessageType::Email,
            flags: MessageFlags::new(false, false, false),
            timestamp: 1_709_654_400,
            message_id: [0x01; MESSAGE_ID_LEN],
            in_reply_to: None,
            sender_address: [0xAA; ADDRESS_HASH_LEN],
            recipients: vec![Recipient {
                address_hash: [0xBB; ADDRESS_HASH_LEN],
                recipient_type: RecipientType::To,
            }],
            subject: "Hello".to_string(),
            body: "Hi there".to_string(),
            attachments: vec![],
        }
    }

    #[test]
    fn simple_message_roundtrip() {
        let msg = simple_message();
        let bytes = msg.to_bytes().unwrap();
        let decoded = HarmonyMessage::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn message_with_reply_and_attachments() {
        let msg = HarmonyMessage {
            version: VERSION,
            message_type: MailMessageType::Email,
            flags: MessageFlags::new(true, true, false),
            timestamp: 1_709_654_400,
            message_id: [0x02; MESSAGE_ID_LEN],
            in_reply_to: Some([0x01; MESSAGE_ID_LEN]),
            sender_address: [0xAA; ADDRESS_HASH_LEN],
            recipients: vec![
                Recipient {
                    address_hash: [0xBB; ADDRESS_HASH_LEN],
                    recipient_type: RecipientType::To,
                },
                Recipient {
                    address_hash: [0xCC; ADDRESS_HASH_LEN],
                    recipient_type: RecipientType::Cc,
                },
            ],
            subject: "Re: Hello".to_string(),
            body: "Thanks for the message".to_string(),
            attachments: vec![AttachmentRef {
                cid: [0xDD; CID_LEN],
                filename: "doc.pdf".to_string(),
                mime_type: "application/pdf".to_string(),
                size: 1024,
            }],
        };

        let bytes = msg.to_bytes().unwrap();
        let decoded = HarmonyMessage::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, msg);
        assert!(decoded.flags.has_attachments());
        assert!(decoded.flags.is_reply());
        assert!(!decoded.flags.is_forward());
        assert_eq!(decoded.in_reply_to, Some([0x01; MESSAGE_ID_LEN]));
        assert_eq!(decoded.recipients.len(), 2);
        assert_eq!(decoded.attachments.len(), 1);
        assert_eq!(decoded.attachments[0].filename, "doc.pdf");
        assert_eq!(decoded.attachments[0].size, 1024);
    }

    #[test]
    fn receipt_message_roundtrip() {
        let msg = HarmonyMessage {
            version: VERSION,
            message_type: MailMessageType::Receipt,
            flags: MessageFlags::new(false, false, false),
            timestamp: 1_709_654_400,
            message_id: [0x03; MESSAGE_ID_LEN],
            in_reply_to: Some([0x01; MESSAGE_ID_LEN]),
            sender_address: [0xBB; ADDRESS_HASH_LEN],
            recipients: vec![Recipient {
                address_hash: [0xAA; ADDRESS_HASH_LEN],
                recipient_type: RecipientType::To,
            }],
            subject: String::new(),
            body: String::new(),
            attachments: vec![],
        };

        let bytes = msg.to_bytes().unwrap();
        let decoded = HarmonyMessage::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, msg);
        assert_eq!(decoded.message_type, MailMessageType::Receipt);
    }

    #[test]
    fn bounce_message_roundtrip() {
        let msg = HarmonyMessage {
            version: VERSION,
            message_type: MailMessageType::Bounce,
            flags: MessageFlags::new(false, false, false),
            timestamp: 1_709_654_400,
            message_id: [0x04; MESSAGE_ID_LEN],
            in_reply_to: Some([0x01; MESSAGE_ID_LEN]),
            sender_address: [0x00; ADDRESS_HASH_LEN],
            recipients: vec![Recipient {
                address_hash: [0xAA; ADDRESS_HASH_LEN],
                recipient_type: RecipientType::To,
            }],
            subject: "Undeliverable".to_string(),
            body: "Recipient not found on any reachable node".to_string(),
            attachments: vec![],
        };

        let bytes = msg.to_bytes().unwrap();
        let decoded = HarmonyMessage::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, msg);
        assert_eq!(decoded.message_type, MailMessageType::Bounce);
    }

    #[test]
    fn bcc_stripped_from_wire_format() {
        let msg = HarmonyMessage {
            version: VERSION,
            message_type: MailMessageType::Email,
            flags: MessageFlags::new(false, false, false),
            timestamp: 1_709_654_400,
            message_id: [0x01; MESSAGE_ID_LEN],
            in_reply_to: None,
            sender_address: [0xAA; ADDRESS_HASH_LEN],
            recipients: vec![
                Recipient {
                    address_hash: [0xBB; ADDRESS_HASH_LEN],
                    recipient_type: RecipientType::To,
                },
                Recipient {
                    address_hash: [0xCC; ADDRESS_HASH_LEN],
                    recipient_type: RecipientType::Bcc,
                },
            ],
            subject: "Secret".to_string(),
            body: "BCC test".to_string(),
            attachments: vec![],
        };

        let bytes = msg.to_bytes().unwrap();
        let decoded = HarmonyMessage::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.recipients.len(), 1);
        assert_eq!(decoded.recipients[0].recipient_type, RecipientType::To);
        assert_eq!(decoded.recipients[0].address_hash, [0xBB; ADDRESS_HASH_LEN]);
    }

    #[test]
    fn simple_message_size() {
        let msg = simple_message();
        let bytes = msg.to_bytes().unwrap();
        assert!(
            bytes.len() < 150,
            "simple message should be under 150 bytes, got {}",
            bytes.len()
        );
    }
}
```

- [ ] **Step 5: Create `crates/harmony-mailbox/src/mailbox.rs`**

Copy all code from `harmony-mail/src/mailbox.rs`, changing `use crate::error::MailError` to `use crate::error::MailboxError` throughout. Change `fn truncate_utf8` from `fn` (private) to `pub fn` so harmony-mail can access it if needed. Include ALL tests.

```rust
//! Merkle mailbox -- CAS-backed email storage with folder structure.

use crate::error::MailboxError;
use crate::message::{ADDRESS_HASH_LEN, CID_LEN, MESSAGE_ID_LEN};

// ── Wire format v1 size guards ───────────────────────────────────────
const _: () = assert!(CID_LEN == 32, "mailbox v1 wire format requires CID_LEN == 32");
const _: () = assert!(
    MESSAGE_ID_LEN == 16,
    "mailbox v1 wire format requires MESSAGE_ID_LEN == 16"
);
const _: () = assert!(
    ADDRESS_HASH_LEN == 16,
    "mailbox v1 wire format requires ADDRESS_HASH_LEN == 16"
);

// ── Constants ──────────────────────────────────────────────────────────

pub const MAILBOX_VERSION: u8 = 0x01;
pub const ROOT_MAGIC: [u8; 4] = *b"MBOX";
pub const FOLDER_MAGIC: [u8; 4] = *b"MFLD";
pub const PAGE_MAGIC: [u8; 4] = *b"MPAG";
pub const PAGE_CAPACITY: usize = 100;
pub const MAX_SNIPPET_LEN: usize = 128;
pub const FOLDER_COUNT: usize = 4;
pub const FOLDER_NAMES: [&str; FOLDER_COUNT] = ["inbox", "sent", "drafts", "trash"];
pub const EMPTY_CID: [u8; CID_LEN] = [0u8; CID_LEN];

// ── Helpers ────────────────────────────────────────────────────────────

/// Truncate a UTF-8 string to at most `max_bytes` without splitting
/// multi-byte characters. Returns the longest valid prefix.
pub fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ── Folder kind ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FolderKind {
    Inbox = 0,
    Sent = 1,
    Drafts = 2,
    Trash = 3,
}

impl FolderKind {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(Self::Inbox),
            1 => Some(Self::Sent),
            2 => Some(Self::Drafts),
            3 => Some(Self::Trash),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        FOLDER_NAMES[self as usize]
    }
}

// ── MailRoot ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailRoot {
    pub version: u8,
    pub owner_address: [u8; ADDRESS_HASH_LEN],
    pub updated_at: u64,
    pub folders: [[u8; CID_LEN]; FOLDER_COUNT],
}

impl MailRoot {
    pub const WIRE_SIZE: usize = 4 + 1 + ADDRESS_HASH_LEN + 8 + (CID_LEN * FOLDER_COUNT);

    pub fn new_empty(owner_address: [u8; ADDRESS_HASH_LEN], now: u64) -> Self {
        Self {
            version: MAILBOX_VERSION,
            owner_address,
            updated_at: now,
            folders: [EMPTY_CID; FOLDER_COUNT],
        }
    }

    pub fn folder_cid(&self, kind: FolderKind) -> &[u8; CID_LEN] {
        &self.folders[kind as usize]
    }

    pub fn with_folder(mut self, kind: FolderKind, cid: [u8; CID_LEN], now: u64) -> Self {
        self.folders[kind as usize] = cid;
        self.updated_at = now;
        self
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(Self::WIRE_SIZE);
        buf.extend_from_slice(&ROOT_MAGIC);
        buf.push(MAILBOX_VERSION);
        buf.extend_from_slice(&self.owner_address);
        buf.extend_from_slice(&self.updated_at.to_be_bytes());
        for folder_cid in &self.folders {
            buf.extend_from_slice(folder_cid);
        }
        buf
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, MailboxError> {
        if data.len() < Self::WIRE_SIZE {
            return Err(MailboxError::MessageTooShort {
                len: data.len(),
                min: Self::WIRE_SIZE,
            });
        }
        if data.len() > Self::WIRE_SIZE {
            return Err(MailboxError::TrailingBytes {
                count: data.len() - Self::WIRE_SIZE,
            });
        }

        let mut found_magic = [0u8; 4];
        found_magic.copy_from_slice(&data[0..4]);
        if found_magic != ROOT_MAGIC {
            return Err(MailboxError::InvalidMagic {
                expected: ROOT_MAGIC,
                found: found_magic,
            });
        }
        let version = data[4];
        if version != MAILBOX_VERSION {
            return Err(MailboxError::UnsupportedVersion(version));
        }

        let mut pos = 5;
        let mut owner_address = [0u8; ADDRESS_HASH_LEN];
        owner_address.copy_from_slice(&data[pos..pos + ADDRESS_HASH_LEN]);
        pos += ADDRESS_HASH_LEN;

        let updated_at = u64::from_be_bytes(data[pos..pos + 8].try_into().unwrap());
        pos += 8;

        let mut folders = [EMPTY_CID; FOLDER_COUNT];
        for folder in &mut folders {
            folder.copy_from_slice(&data[pos..pos + CID_LEN]);
            pos += CID_LEN;
        }

        Ok(Self {
            version,
            owner_address,
            updated_at,
            folders,
        })
    }
}

// ── MailFolder ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailFolder {
    pub version: u8,
    pub message_count: u32,
    pub unread_count: u32,
    pub page_cids: Vec<[u8; CID_LEN]>,
}

impl MailFolder {
    const MIN_SIZE: usize = 4 + 1 + 4 + 4 + 2;

    pub fn new_empty() -> Self {
        Self {
            version: MAILBOX_VERSION,
            message_count: 0,
            unread_count: 0,
            page_cids: Vec::new(),
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, MailboxError> {
        if self.unread_count > self.message_count {
            return Err(MailboxError::TooManyEntries {
                count: self.unread_count as usize,
                max: self.message_count as usize,
            });
        }
        let count = u16::try_from(self.page_cids.len()).map_err(|_| {
            MailboxError::TooManyEntries {
                count: self.page_cids.len(),
                max: u16::MAX as usize,
            }
        })?;
        let mut buf = Vec::with_capacity(Self::MIN_SIZE + self.page_cids.len() * CID_LEN);
        buf.extend_from_slice(&FOLDER_MAGIC);
        buf.push(MAILBOX_VERSION);
        buf.extend_from_slice(&self.message_count.to_be_bytes());
        buf.extend_from_slice(&self.unread_count.to_be_bytes());
        buf.extend_from_slice(&count.to_be_bytes());
        for cid in &self.page_cids {
            buf.extend_from_slice(cid);
        }
        Ok(buf)
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, MailboxError> {
        if data.len() < Self::MIN_SIZE {
            return Err(MailboxError::MessageTooShort {
                len: data.len(),
                min: Self::MIN_SIZE,
            });
        }

        let mut found_magic = [0u8; 4];
        found_magic.copy_from_slice(&data[0..4]);
        if found_magic != FOLDER_MAGIC {
            return Err(MailboxError::InvalidMagic {
                expected: FOLDER_MAGIC,
                found: found_magic,
            });
        }
        let version = data[4];
        if version != MAILBOX_VERSION {
            return Err(MailboxError::UnsupportedVersion(version));
        }

        let mut pos = 5;
        let message_count = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap());
        pos += 4;
        let unread_count = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap());
        pos += 4;

        if unread_count > message_count {
            return Err(MailboxError::TooManyEntries {
                count: unread_count as usize,
                max: message_count as usize,
            });
        }

        let page_count = u16::from_be_bytes(data[pos..pos + 2].try_into().unwrap()) as usize;
        pos += 2;

        let expected = pos + page_count * CID_LEN;
        if data.len() < expected {
            return Err(MailboxError::Truncated {
                expected: expected - data.len(),
            });
        }
        if data.len() > expected {
            return Err(MailboxError::TrailingBytes {
                count: data.len() - expected,
            });
        }

        let mut page_cids = Vec::with_capacity(page_count);
        for _ in 0..page_count {
            let mut cid = [0u8; CID_LEN];
            cid.copy_from_slice(&data[pos..pos + CID_LEN]);
            pos += CID_LEN;
            page_cids.push(cid);
        }

        Ok(Self {
            version,
            message_count,
            unread_count,
            page_cids,
        })
    }
}

// ── MessageEntry ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageEntry {
    pub message_cid: [u8; CID_LEN],
    pub message_id: [u8; MESSAGE_ID_LEN],
    pub sender_address: [u8; ADDRESS_HASH_LEN],
    pub timestamp: u64,
    pub subject_snippet: String,
    pub read: bool,
}

impl MessageEntry {
    const MIN_SIZE: usize = CID_LEN + MESSAGE_ID_LEN + ADDRESS_HASH_LEN + 8 + 1 + 2;

    pub fn to_bytes(&self) -> Vec<u8> {
        let snippet = truncate_utf8(&self.subject_snippet, MAX_SNIPPET_LEN);
        let snippet_bytes = snippet.as_bytes();
        let mut buf = Vec::with_capacity(Self::MIN_SIZE + snippet_bytes.len());

        buf.extend_from_slice(&self.message_cid);
        buf.extend_from_slice(&self.message_id);
        buf.extend_from_slice(&self.sender_address);
        buf.extend_from_slice(&self.timestamp.to_be_bytes());

        let flags: u8 = if self.read { 0x01 } else { 0x00 };
        buf.push(flags);

        buf.extend_from_slice(&(snippet_bytes.len() as u16).to_be_bytes());
        buf.extend_from_slice(snippet_bytes);
        buf
    }

    pub fn from_bytes(data: &[u8]) -> Result<(Self, usize), MailboxError> {
        if data.len() < Self::MIN_SIZE {
            return Err(MailboxError::MessageTooShort {
                len: data.len(),
                min: Self::MIN_SIZE,
            });
        }

        let mut pos = 0;

        let mut message_cid = [0u8; CID_LEN];
        message_cid.copy_from_slice(&data[pos..pos + CID_LEN]);
        pos += CID_LEN;

        let mut message_id = [0u8; MESSAGE_ID_LEN];
        message_id.copy_from_slice(&data[pos..pos + MESSAGE_ID_LEN]);
        pos += MESSAGE_ID_LEN;

        let mut sender_address = [0u8; ADDRESS_HASH_LEN];
        sender_address.copy_from_slice(&data[pos..pos + ADDRESS_HASH_LEN]);
        pos += ADDRESS_HASH_LEN;

        let timestamp = u64::from_be_bytes(data[pos..pos + 8].try_into().unwrap());
        pos += 8;

        let read_flag = data[pos];
        if read_flag != 0x00 && read_flag != 0x01 {
            return Err(MailboxError::InvalidFlag {
                field: "read",
                value: read_flag,
            });
        }
        let read = read_flag == 0x01;
        pos += 1;

        let snippet_len = u16::from_be_bytes(data[pos..pos + 2].try_into().unwrap()) as usize;
        pos += 2;

        if snippet_len > MAX_SNIPPET_LEN {
            return Err(MailboxError::FieldTooLong {
                field: "subject_snippet",
                len: snippet_len,
                max: MAX_SNIPPET_LEN,
            });
        }

        if data.len() < pos + snippet_len {
            return Err(MailboxError::Truncated {
                expected: (pos + snippet_len) - data.len(),
            });
        }

        let subject_snippet = core::str::from_utf8(&data[pos..pos + snippet_len])
            .map_err(|_| MailboxError::InvalidUtf8 {
                field: "subject_snippet",
            })?
            .to_string();
        pos += snippet_len;

        Ok((
            Self {
                message_cid,
                message_id,
                sender_address,
                timestamp,
                subject_snippet,
                read,
            },
            pos,
        ))
    }
}

// ── MailPage ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailPage {
    pub version: u8,
    pub next_page: Option<[u8; CID_LEN]>,
    pub entries: Vec<MessageEntry>,
}

impl MailPage {
    const MIN_SIZE: usize = 4 + 1 + 1 + 2;

    pub fn new_empty() -> Self {
        Self {
            version: MAILBOX_VERSION,
            next_page: None,
            entries: Vec::new(),
        }
    }

    pub fn is_full(&self) -> bool {
        self.entries.len() >= PAGE_CAPACITY
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, MailboxError> {
        if self.entries.len() > PAGE_CAPACITY {
            return Err(MailboxError::TooManyEntries {
                count: self.entries.len(),
                max: PAGE_CAPACITY,
            });
        }
        let count = u16::try_from(self.entries.len()).map_err(|_| {
            MailboxError::TooManyEntries {
                count: self.entries.len(),
                max: u16::MAX as usize,
            }
        })?;

        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(&PAGE_MAGIC);
        buf.push(MAILBOX_VERSION);

        match &self.next_page {
            Some(cid) => {
                buf.push(0x01);
                buf.extend_from_slice(cid);
            }
            None => buf.push(0x00),
        }

        buf.extend_from_slice(&count.to_be_bytes());
        for entry in &self.entries {
            buf.extend_from_slice(&entry.to_bytes());
        }
        Ok(buf)
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, MailboxError> {
        if data.len() < Self::MIN_SIZE {
            return Err(MailboxError::MessageTooShort {
                len: data.len(),
                min: Self::MIN_SIZE,
            });
        }

        let mut found_magic = [0u8; 4];
        found_magic.copy_from_slice(&data[0..4]);
        if found_magic != PAGE_MAGIC {
            return Err(MailboxError::InvalidMagic {
                expected: PAGE_MAGIC,
                found: found_magic,
            });
        }
        let version = data[4];
        if version != MAILBOX_VERSION {
            return Err(MailboxError::UnsupportedVersion(version));
        }

        let mut pos = 5;
        let has_next = data[pos];
        pos += 1;

        let next_page = match has_next {
            0x00 => None,
            0x01 => {
                if data.len() < pos + CID_LEN {
                    return Err(MailboxError::Truncated {
                        expected: (pos + CID_LEN) - data.len(),
                    });
                }
                let mut cid = [0u8; CID_LEN];
                cid.copy_from_slice(&data[pos..pos + CID_LEN]);
                pos += CID_LEN;
                Some(cid)
            }
            _ => {
                return Err(MailboxError::InvalidFlag {
                    field: "has_next",
                    value: has_next,
                });
            }
        };

        if data.len() < pos + 2 {
            return Err(MailboxError::Truncated {
                expected: (pos + 2) - data.len(),
            });
        }
        let entry_count = u16::from_be_bytes(data[pos..pos + 2].try_into().unwrap()) as usize;
        pos += 2;

        if entry_count > PAGE_CAPACITY {
            return Err(MailboxError::TooManyEntries {
                count: entry_count,
                max: PAGE_CAPACITY,
            });
        }

        let mut entries = Vec::with_capacity(entry_count);
        for _ in 0..entry_count {
            let (entry, consumed) = MessageEntry::from_bytes(&data[pos..])?;
            pos += consumed;
            entries.push(entry);
        }

        if pos != data.len() {
            return Err(MailboxError::TrailingBytes {
                count: data.len() - pos,
            });
        }

        Ok(Self {
            version,
            next_page,
            entries,
        })
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_address() -> [u8; ADDRESS_HASH_LEN] {
        let mut addr = [0u8; ADDRESS_HASH_LEN];
        addr[0] = 0xAB;
        addr[15] = 0xCD;
        addr
    }

    fn dummy_cid(tag: u8) -> [u8; CID_LEN] {
        let mut cid = [0u8; CID_LEN];
        cid[0] = tag;
        cid[31] = tag;
        cid
    }

    fn dummy_entry(tag: u8) -> MessageEntry {
        MessageEntry {
            message_cid: dummy_cid(tag),
            message_id: [tag; MESSAGE_ID_LEN],
            sender_address: dummy_address(),
            timestamp: 1744403200 + tag as u64,
            subject_snippet: format!("Test email #{tag}"),
            read: tag % 2 == 0,
        }
    }

    #[test]
    fn mail_root_roundtrip() {
        let root = MailRoot {
            version: MAILBOX_VERSION,
            owner_address: dummy_address(),
            updated_at: 1744403200,
            folders: [dummy_cid(1), dummy_cid(2), dummy_cid(3), dummy_cid(4)],
        };
        let bytes = root.to_bytes();
        assert_eq!(bytes.len(), MailRoot::WIRE_SIZE);
        let decoded = MailRoot::from_bytes(&bytes).unwrap();
        assert_eq!(root, decoded);
    }

    #[test]
    fn mail_root_rejects_trailing_bytes() {
        let root = MailRoot::new_empty(dummy_address(), 100);
        let mut bytes = root.to_bytes();
        bytes.push(0xFF);
        assert!(matches!(
            MailRoot::from_bytes(&bytes),
            Err(MailboxError::TrailingBytes { count: 1 })
        ));
    }

    #[test]
    fn mail_root_rejects_bad_magic() {
        let mut bytes = MailRoot::new_empty(dummy_address(), 100).to_bytes();
        bytes[0] = b'X';
        assert!(matches!(
            MailRoot::from_bytes(&bytes),
            Err(MailboxError::InvalidMagic { .. })
        ));
    }

    #[test]
    fn mail_root_empty() {
        let root = MailRoot::new_empty(dummy_address(), 1744403200);
        let bytes = root.to_bytes();
        let decoded = MailRoot::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.folders, [EMPTY_CID; FOLDER_COUNT]);
    }

    #[test]
    fn mail_root_with_folder() {
        let root = MailRoot::new_empty(dummy_address(), 100);
        let updated = root.with_folder(FolderKind::Inbox, dummy_cid(0xFF), 200);
        assert_eq!(updated.folders[0], dummy_cid(0xFF));
        assert_eq!(updated.updated_at, 200);
        assert_eq!(updated.folders[1], EMPTY_CID);
    }

    #[test]
    fn mail_folder_roundtrip() {
        let folder = MailFolder {
            version: MAILBOX_VERSION,
            message_count: 42,
            unread_count: 3,
            page_cids: vec![dummy_cid(1), dummy_cid(2)],
        };
        let bytes = folder.to_bytes().unwrap();
        let decoded = MailFolder::from_bytes(&bytes).unwrap();
        assert_eq!(folder, decoded);
    }

    #[test]
    fn mail_folder_rejects_trailing_bytes() {
        let folder = MailFolder::new_empty();
        let mut bytes = folder.to_bytes().unwrap();
        bytes.push(0xFF);
        assert!(matches!(
            MailFolder::from_bytes(&bytes),
            Err(MailboxError::TrailingBytes { count: 1 })
        ));
    }

    #[test]
    fn mail_folder_empty() {
        let folder = MailFolder::new_empty();
        let bytes = folder.to_bytes().unwrap();
        let decoded = MailFolder::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.message_count, 0);
        assert_eq!(decoded.unread_count, 0);
        assert!(decoded.page_cids.is_empty());
    }

    #[test]
    fn message_entry_roundtrip() {
        let entry = dummy_entry(7);
        let bytes = entry.to_bytes();
        let (decoded, consumed) = MessageEntry::from_bytes(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(entry, decoded);
    }

    #[test]
    fn message_entry_utf8_truncation() {
        let entry = MessageEntry {
            message_cid: dummy_cid(1),
            message_id: [1; MESSAGE_ID_LEN],
            sender_address: dummy_address(),
            timestamp: 100,
            subject_snippet: "x".repeat(MAX_SNIPPET_LEN - 1) + "\u{1F600}",
            read: false,
        };
        let bytes = entry.to_bytes();
        let (decoded, _) = MessageEntry::from_bytes(&bytes).unwrap();
        assert!(decoded.subject_snippet.len() <= MAX_SNIPPET_LEN);
        assert!(decoded
            .subject_snippet
            .is_char_boundary(decoded.subject_snippet.len()));
    }

    #[test]
    fn mail_page_roundtrip() {
        let page = MailPage {
            version: MAILBOX_VERSION,
            next_page: Some(dummy_cid(0xAA)),
            entries: vec![dummy_entry(1), dummy_entry(2), dummy_entry(3)],
        };
        let bytes = page.to_bytes().unwrap();
        let decoded = MailPage::from_bytes(&bytes).unwrap();
        assert_eq!(page, decoded);
    }

    #[test]
    fn mail_page_no_next() {
        let page = MailPage {
            version: MAILBOX_VERSION,
            next_page: None,
            entries: vec![dummy_entry(5)],
        };
        let bytes = page.to_bytes().unwrap();
        let decoded = MailPage::from_bytes(&bytes).unwrap();
        assert!(decoded.next_page.is_none());
        assert_eq!(decoded.entries.len(), 1);
    }

    #[test]
    fn mail_page_rejects_invalid_has_next() {
        let page = MailPage::new_empty();
        let mut bytes = page.to_bytes().unwrap();
        bytes[5] = 0x02;
        assert!(matches!(
            MailPage::from_bytes(&bytes),
            Err(MailboxError::InvalidFlag {
                field: "has_next",
                value: 0x02
            })
        ));
    }

    #[test]
    fn mail_page_rejects_trailing_bytes() {
        let page = MailPage {
            version: MAILBOX_VERSION,
            next_page: None,
            entries: vec![dummy_entry(1)],
        };
        let mut bytes = page.to_bytes().unwrap();
        bytes.push(0xFF);
        assert!(matches!(
            MailPage::from_bytes(&bytes),
            Err(MailboxError::TrailingBytes { .. })
        ));
    }

    #[test]
    fn mail_page_is_full() {
        let mut page = MailPage::new_empty();
        assert!(!page.is_full());
        for i in 0..PAGE_CAPACITY {
            page.entries.push(dummy_entry(i as u8));
        }
        assert!(page.is_full());
    }

    #[test]
    fn folder_kind_roundtrip() {
        for i in 0..FOLDER_COUNT {
            let kind = FolderKind::from_u8(i as u8).unwrap();
            assert_eq!(kind as u8, i as u8);
            assert_eq!(kind.name(), FOLDER_NAMES[i]);
        }
        assert!(FolderKind::from_u8(4).is_none());
    }

    #[test]
    fn message_entry_rejects_invalid_read_flag() {
        let entry = dummy_entry(1);
        let mut bytes = entry.to_bytes();
        let flag_offset = CID_LEN + MESSAGE_ID_LEN + ADDRESS_HASH_LEN + 8;
        bytes[flag_offset] = 0x02;
        assert!(matches!(
            MessageEntry::from_bytes(&bytes),
            Err(MailboxError::InvalidFlag {
                field: "read",
                value: 0x02
            })
        ));
    }

    #[test]
    fn mail_page_rejects_entry_count_over_capacity() {
        let page = MailPage {
            version: MAILBOX_VERSION,
            next_page: None,
            entries: vec![dummy_entry(1)],
        };
        let mut bytes = page.to_bytes().unwrap();
        let count_offset = 6;
        let bad_count = (PAGE_CAPACITY as u16) + 1;
        bytes[count_offset..count_offset + 2].copy_from_slice(&bad_count.to_be_bytes());
        assert!(matches!(
            MailPage::from_bytes(&bytes),
            Err(MailboxError::TooManyEntries { .. })
        ));
    }

    #[test]
    fn message_entry_rejects_oversized_snippet() {
        let entry = dummy_entry(1);
        let mut bytes = entry.to_bytes();
        let len_offset = CID_LEN + MESSAGE_ID_LEN + ADDRESS_HASH_LEN + 8 + 1;
        let bad_len = (MAX_SNIPPET_LEN as u16) + 1;
        bytes[len_offset..len_offset + 2].copy_from_slice(&bad_len.to_be_bytes());
        assert!(matches!(
            MessageEntry::from_bytes(&bytes),
            Err(MailboxError::FieldTooLong {
                field: "subject_snippet",
                max: MAX_SNIPPET_LEN,
                ..
            })
        ));
    }

    #[test]
    fn mail_folder_rejects_unread_exceeding_total_on_deserialize() {
        let mut folder = MailFolder::new_empty();
        folder.message_count = 5;
        folder.unread_count = 3;
        let mut bytes = folder.to_bytes().unwrap();
        bytes[9..13].copy_from_slice(&10u32.to_be_bytes());
        assert!(matches!(
            MailFolder::from_bytes(&bytes),
            Err(MailboxError::TooManyEntries { .. })
        ));
    }

    #[test]
    fn mail_folder_to_bytes_rejects_unread_exceeding_total() {
        let mut folder = MailFolder::new_empty();
        folder.message_count = 5;
        folder.unread_count = 10;
        assert!(matches!(
            folder.to_bytes(),
            Err(MailboxError::TooManyEntries { .. })
        ));
    }

    #[test]
    fn truncate_utf8_respects_boundaries() {
        assert_eq!(truncate_utf8("hello", 10), "hello");
        assert_eq!(truncate_utf8("hello", 3), "hel");
        assert_eq!(truncate_utf8("cafe\u{0301}", 3), "caf");
        assert_eq!(truncate_utf8("cafe\u{0301}", 4), "cafe");
        assert_eq!(truncate_utf8("a\u{1F600}b", 2), "a");
        assert_eq!(truncate_utf8("a\u{1F600}b", 5), "a\u{1F600}");
    }
}
```

Note: The `truncate_utf8` test for "cafe" uses the combining acute accent (\u{0301}) rather than the precomposed "e" (\u{00E9}) because the original test in harmony-mail uses that pattern. Verify the actual test expectation matches the source. The key behavior tested is that truncation respects char boundaries.

- [ ] **Step 6: Create `crates/harmony-mailbox/src/lib.rs`**

```rust
pub mod error;
pub mod mailbox;
pub mod message;

// Re-export primary types for convenient access.
pub use error::MailboxError;
pub use mailbox::{
    FolderKind, MailFolder, MailPage, MailRoot, MessageEntry, EMPTY_CID, FOLDER_COUNT,
    FOLDER_MAGIC, FOLDER_NAMES, MAILBOX_VERSION, MAX_SNIPPET_LEN, PAGE_CAPACITY, PAGE_MAGIC,
    ROOT_MAGIC,
};
pub use message::{
    AttachmentRef, HarmonyMessage, MailMessageType, MessageFlags, Recipient, RecipientType,
    ADDRESS_HASH_LEN, CID_LEN, MAX_ATTACHMENTS, MAX_BODY_LEN, MAX_RECIPIENTS, MAX_SUBJECT_LEN,
    MESSAGE_ID_LEN, VERSION,
};
```

- [ ] **Step 7: Add `harmony-mailbox` to workspace `Cargo.toml`**

In `/Users/zeblith/work/zeblithic/harmony/Cargo.toml`, add to the `members` array:

```toml
    "crates/harmony-mailbox",
```

Add after the `"crates/harmony-mail",` line.

Also add to `[workspace.dependencies]`:

```toml
harmony-mailbox = { path = "crates/harmony-mailbox", default-features = false }
```

Add this line after the `harmony-mail` line if one exists, or near the other `harmony-*` workspace deps (around line 139-171).

- [ ] **Step 8: Run tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony
cargo test -p harmony-mailbox
```

Expected output: all tests pass (same count as the mailbox + message tests from harmony-mail: ~30 tests).

- [ ] **Step 9: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony
git add crates/harmony-mailbox/ Cargo.toml
git commit -m "feat: create harmony-mailbox shared crate with wire format types

Extract MailRoot/MailFolder/MailPage/MessageEntry and HarmonyMessage
types from harmony-mail into a standalone crate for shared use by
harmony-client. Includes MailboxError (wire-format variants only),
all serialization/deserialization, and all existing tests."
```

---

## Task 2: Update `harmony-mail` to use `harmony-mailbox` (harmony repo)

**Files:** Modify `crates/harmony-mail/Cargo.toml`, `crates/harmony-mail/src/error.rs`, `crates/harmony-mail/src/mailbox.rs`, `crates/harmony-mail/src/message.rs`.

The goal: `harmony-mail` re-exports everything from `harmony-mailbox` so downstream code (translate.rs, outbound.rs, imap.rs, etc.) sees no API change. `MailError` keeps its SMTP-only variants and gains a `#[from] MailboxError` variant.

- [ ] **Step 1: Add `harmony-mailbox` to `crates/harmony-mail/Cargo.toml`**

Add to `[dependencies]`:

```toml
harmony-mailbox.workspace = true
```

- [ ] **Step 2: Update `crates/harmony-mail/src/error.rs`**

Replace the entire file. `MailError` now has only SMTP-specific variants plus a blanket `#[from] MailboxError` that wraps all wire-format errors. The `check_u16_len` helper delegates to `harmony_mailbox::error::check_u16_len`.

```rust
use harmony_mailbox::MailboxError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MailError {
    #[error(transparent)]
    Mailbox(#[from] MailboxError),

    #[error("unknown SMTP command: {0}")]
    UnknownCommand(String),

    #[error("invalid identity bytes in registration")]
    InvalidIdentity,
}

/// Validate that a string's length fits in a u16 for length-prefixed encoding.
pub(crate) fn check_u16_len(s: &str, field: &'static str) -> Result<u16, MailError> {
    harmony_mailbox::error::check_u16_len(s, field).map_err(MailError::from)
}
```

- [ ] **Step 3: Update `crates/harmony-mail/src/mailbox.rs`**

Replace the entire file with re-exports from `harmony-mailbox`. This preserves the public API for all downstream code within `harmony-mail`.

```rust
//! Merkle mailbox -- CAS-backed email storage with folder structure.
//!
//! Types and logic are defined in the `harmony-mailbox` crate.
//! This module re-exports them for backward compatibility.

pub use harmony_mailbox::mailbox::*;
```

- [ ] **Step 4: Update `crates/harmony-mail/src/message.rs`**

Replace the file with re-exports plus the `unique_message_id()` function, which depends on `blake3` (not available in `harmony-mailbox`).

```rust
//! Harmony-native email message format.
//!
//! Wire format types are defined in the `harmony-mailbox` crate.
//! This module re-exports them and adds `unique_message_id()`.

pub use harmony_mailbox::message::*;

/// Generate a unique 16-byte message ID using timestamp + atomic counter.
/// Safe to call from multiple threads -- uses a process-global atomic counter
/// to guarantee uniqueness even within the same nanosecond.
pub fn unique_message_id() -> [u8; MESSAGE_ID_LEN] {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);

    let mut hasher = blake3::Hasher::new();
    hasher.update(&now.to_le_bytes());
    hasher.update(&seq.to_le_bytes());
    let hash = hasher.finalize();
    let mut id = [0u8; MESSAGE_ID_LEN];
    id.copy_from_slice(&hash.as_bytes()[..MESSAGE_ID_LEN]);
    id
}
```

- [ ] **Step 5: Verify downstream code compiles**

The key consumers within `harmony-mail` that use these types:
- `translate.rs` — imports `crate::message::{HarmonyMessage, MailMessageType, ...}` (unchanged due to re-exports)
- `outbound.rs` — imports `crate::message::{HarmonyMessage, MailMessageType, RecipientType, ADDRESS_HASH_LEN}` (unchanged)
- `imap.rs` / `imap_store.rs` — may reference `MailError` (now wraps `MailboxError`)

Check if any code does `match` on `MailError` variants that moved to `MailboxError`. If so, pattern matches like `Err(MailError::MessageTooShort { .. })` need to become `Err(MailError::Mailbox(MailboxError::MessageTooShort { .. }))` or use the `?` operator with `From`.

Scan for explicit `MailError::` variant matches in files other than error.rs/mailbox.rs/message.rs:

```bash
cd /Users/zeblith/work/zeblithic/harmony
grep -rn "MailError::" crates/harmony-mail/src/ --include="*.rs" | grep -v "error.rs\|mailbox.rs\|message.rs"
```

For any matches found: if they reference wire-format variants (e.g., `MailError::MessageTooShort`), update to `MailError::Mailbox(MailboxError::MessageTooShort { .. })` or restructure the match. Most code uses `?` and `Result<_, MailError>` which auto-converts via `#[from]`.

- [ ] **Step 6: Run all harmony-mail tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony
cargo test -p harmony-mail
```

Expected: all existing tests pass. The tests in the original `mailbox.rs` and `message.rs` now live in `harmony-mailbox`. Tests in other files (translate.rs, outbound.rs, imap.rs, imap_store.rs, etc.) should still compile and pass because re-exports preserve the API surface.

The test count for `harmony-mail` will decrease (the moved tests now run under `harmony-mailbox`), but the sum across both crates should match the original total.

- [ ] **Step 7: Run both crates together**

```bash
cd /Users/zeblith/work/zeblithic/harmony
cargo test -p harmony-mailbox -p harmony-mail
```

Expected: all tests pass across both crates.

- [ ] **Step 8: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony
git add crates/harmony-mail/
git commit -m "refactor: update harmony-mail to re-export from harmony-mailbox

Replace inline type definitions with pub use re-exports from the new
harmony-mailbox crate. MailError gains #[from] MailboxError for seamless
error conversion. unique_message_id() stays local (depends on blake3)."
```

---

## Task 3: Gateway Zenoh activation (harmony repo)

**Files:** Modify `crates/harmony-mail/src/config.rs`, create `crates/harmony-mail/src/mailbox_manager.rs`, modify `crates/harmony-mail/src/lib.rs`, modify `crates/harmony-mail/src/server.rs`.

- [ ] **Step 1: Add `ZenohConfig` to `crates/harmony-mail/src/config.rs`**

Add the following struct and default functions, and add the field to `Config`:

```rust
/// Zenoh session configuration for real-time mailbox notifications.
///
/// When enabled, the gateway publishes root CID updates on message delivery
/// and serves catch-up queryables for client startup.
#[derive(Debug, Deserialize)]
pub struct ZenohConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Zenoh endpoint to connect to (e.g. "tcp/127.0.0.1:7447").
    /// If not set, uses Zenoh's default scouting.
    pub endpoint: Option<String>,
}

impl Default for ZenohConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
        }
    }
}
```

Add to the `Config` struct:

```rust
    #[serde(default)]
    pub zenoh: ZenohConfig,
```

- [ ] **Step 2: Add `zenoh` dependency to `crates/harmony-mail/Cargo.toml`**

Add to `[dependencies]`:

```toml
zenoh.workspace = true
```

- [ ] **Step 3: Create `crates/harmony-mail/src/mailbox_manager.rs`**

This module manages per-user mailbox state and Zenoh root CID notifications.

```rust
//! Mailbox manager: per-user mailbox state + Zenoh root CID publishing.
//!
//! Manages the in-memory root CID cache and sends notifications through
//! an mpsc channel to a background async task that publishes to Zenoh.

use std::collections::HashMap;
use tokio::sync::mpsc;

/// Notification channel for Zenoh root CID publications.
///
/// MailboxManager runs in sync context (spawn_blocking). This struct holds
/// the send side of an unbounded mpsc channel. A background async task
/// drains the receiver and calls session.put().
pub struct ZenohPublisher {
    tx: mpsc::UnboundedSender<(String, [u8; 32])>,
}

impl ZenohPublisher {
    /// Create a new publisher backed by a Zenoh session.
    ///
    /// Spawns a background task that drains the channel and publishes
    /// root CID updates to `harmony/messages/{addr_hex}/inbox`.
    pub fn new(session: zenoh::Session) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, [u8; 32])>();
        tokio::spawn(async move {
            while let Some((addr_hex, root_cid)) = rx.recv().await {
                let topic = format!("harmony/messages/{addr_hex}/inbox");
                if let Err(e) = session.put(&topic, &root_cid[..]).await {
                    tracing::warn!(error = %e, %topic, "Zenoh root CID publish failed");
                }
            }
        });
        Self { tx }
    }

    /// Create a publisher from a raw sender (for testing).
    #[cfg(test)]
    pub fn from_sender(tx: mpsc::UnboundedSender<(String, [u8; 32])>) -> Self {
        Self { tx }
    }

    /// Send a root CID update notification.
    pub fn notify(&self, addr_hex: String, root_cid: [u8; 32]) {
        if let Err(e) = self.tx.send((addr_hex, root_cid)) {
            tracing::warn!(error = %e, "ZenohPublisher channel closed");
        }
    }
}

/// Per-user mailbox state.
pub struct MailboxManager {
    /// Current root CID per user address (hex-encoded address -> 32-byte CID).
    root_cids: HashMap<String, [u8; 32]>,
    /// Optional Zenoh publisher for root CID notifications.
    publisher: Option<ZenohPublisher>,
}

impl MailboxManager {
    /// Create a new MailboxManager with an optional Zenoh publisher.
    pub fn new(publisher: Option<ZenohPublisher>) -> Self {
        Self {
            root_cids: HashMap::new(),
            publisher,
        }
    }

    /// Update the root CID for a user and publish the notification.
    pub fn update_root_cid(&mut self, addr_hex: &str, root_cid: [u8; 32]) {
        self.root_cids.insert(addr_hex.to_string(), root_cid);
        if let Some(ref publisher) = self.publisher {
            publisher.notify(addr_hex.to_string(), root_cid);
        }
    }

    /// Get the current root CID for a user, if known.
    pub fn get_root_cid(&self, addr_hex: &str) -> Option<&[u8; 32]> {
        self.root_cids.get(addr_hex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mailbox_manager_publishes_root_cid() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let publisher = ZenohPublisher::from_sender(tx);
        let mut manager = MailboxManager::new(Some(publisher));

        let addr = "deadbeef01020304";
        let root_cid = [0xAA; 32];
        manager.update_root_cid(addr, root_cid);

        // Verify the publisher received the notification.
        let (received_addr, received_cid) = rx.recv().await.unwrap();
        assert_eq!(received_addr, addr);
        assert_eq!(received_cid, root_cid);

        // Verify in-memory state.
        assert_eq!(manager.get_root_cid(addr), Some(&root_cid));
    }

    #[tokio::test]
    async fn mailbox_manager_works_without_publisher() {
        let mut manager = MailboxManager::new(None);
        let addr = "abcd1234";
        let root_cid = [0xBB; 32];
        manager.update_root_cid(addr, root_cid);
        assert_eq!(manager.get_root_cid(addr), Some(&root_cid));
    }

    #[tokio::test]
    async fn mailbox_manager_updates_overwrite() {
        let mut manager = MailboxManager::new(None);
        let addr = "node1";
        manager.update_root_cid(addr, [0x01; 32]);
        manager.update_root_cid(addr, [0x02; 32]);
        assert_eq!(manager.get_root_cid(addr), Some(&[0x02; 32]));
    }
}
```

- [ ] **Step 4: Add `pub mod mailbox_manager;` to `crates/harmony-mail/src/lib.rs`**

Add the line:

```rust
pub mod mailbox_manager;
```

- [ ] **Step 5: Wire Zenoh session into `server.rs`**

In `crates/harmony-mail/src/server.rs`, add Zenoh initialization after the TLS setup (around line 196, after the cancel token creation) but before the SMTP listener loops. Add inside the `run()` function:

```rust
    // ── Zenoh session (mailbox root CID notifications) ──────────────
    use crate::mailbox_manager::{MailboxManager, ZenohPublisher};

    let _mailbox_manager = if config.zenoh.enabled {
        let mut zenoh_config = zenoh::Config::default();
        if let Some(ref ep) = config.zenoh.endpoint {
            if let Ok(ep_json) = serde_json::to_string(ep) {
                let _ = zenoh_config.insert_json5("connect/endpoints", &format!("[{ep_json}]"));
            }
        }

        match zenoh::open(zenoh_config).await {
            Ok(session) => {
                tracing::info!("Zenoh session opened for mailbox notifications");
                let publisher = ZenohPublisher::new(session.clone());
                let manager = MailboxManager::new(Some(publisher));

                // Register catch-up queryables for known users (future: loaded from registry).
                // For now, queryables will be registered dynamically as users are registered.

                Some(manager)
            }
            Err(e) => {
                tracing::warn!(error = %e, "Zenoh open failed, mailbox notifications disabled");
                None
            }
        }
    } else {
        tracing::info!("Zenoh mailbox notifications disabled");
        None
    };
```

Note: The `_mailbox_manager` is unused in this task -- it will be wired into message delivery in a future task. The important part is that the Zenoh session opens and the `ZenohPublisher` is ready.

- [ ] **Step 6: Run tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony
cargo test -p harmony-mail
```

Expected: all existing tests pass, plus the new mailbox_manager tests.

- [ ] **Step 7: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony
git add crates/harmony-mail/
git commit -m "feat: add Zenoh publisher for mailbox root CID notifications

Add ZenohConfig to config.toml, MailboxManager with ZenohPublisher
that sends (addr_hex, root_cid) through mpsc to a background async
task. Wire Zenoh session opening into server::run()."
```

---

## Task 4: Client mail module (harmony-client repo)

**Files:** Create `src-tauri/src/mail.rs`. Modify `src-tauri/Cargo.toml`, `src-tauri/src/lib.rs`.

- [ ] **Step 1: Add `harmony-mailbox` to `src-tauri/Cargo.toml`**

Add to `[dependencies]`:

```toml
harmony-mailbox = { git = "https://github.com/zeblithic/harmony.git", branch = "main" }
```

Note: During development before the harmony-mailbox crate is merged to `main`, use a feature branch name or a `[patch]` override in `Cargo.toml`:

```toml
[patch.'https://github.com/zeblithic/harmony.git']
harmony-mailbox = { path = "../../harmony/crates/harmony-mailbox" }
harmony-runtime = { path = "../../harmony/crates/harmony-runtime" }
harmony-identity = { path = "../../harmony/crates/harmony-identity" }
harmony-content = { path = "../../harmony/crates/harmony-content" }
harmony-compute = { path = "../../harmony/crates/harmony-compute" }
harmony-telemetry = { path = "../../harmony/crates/harmony-telemetry" }
```

The patch block ensures all harmony crates resolve to local paths during development.

- [ ] **Step 2: Create `src-tauri/src/mail.rs`**

```rust
//! Mail module: CAS tree walking, flat file cache, Tauri commands.
//!
//! Provides the client-side mail receive path:
//! - MailState: holds the latest known root CID for our mailbox
//! - cached_fetch: disk-cached CAS block fetcher
//! - get_inbox: tree walk returning inbox entries
//! - get_mail_message: fetch and deserialize a full HarmonyMessage

use std::path::{Path, PathBuf};

use harmony_mailbox::mailbox::{FolderKind, MailFolder, MailPage, MailRoot, EMPTY_CID};
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
    use harmony_mailbox::mailbox::{
        MailFolder, MailPage, MailRoot, MessageEntry, MAILBOX_VERSION,
    };
    use harmony_mailbox::message::{
        HarmonyMessage, MailMessageType, MessageFlags, Recipient, RecipientType,
        ADDRESS_HASH_LEN, CID_LEN, MESSAGE_ID_LEN, VERSION,
    };

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

    /// Compute a BLAKE3 hash as a 32-byte CID (matches harmony-content).
    fn blake3_cid(data: &[u8]) -> [u8; 32] {
        let hash = blake3::hash(data);
        *hash.as_bytes()
    }
}
```

Note: Tests use `blake3` for CID computation. Add `blake3` as a dev-dependency if not already present:

Add to `[dev-dependencies]` in `src-tauri/Cargo.toml`:

```toml
[dev-dependencies]
blake3 = "1"
```

- [ ] **Step 3: Add `mod mail` and Tauri commands to `src-tauri/src/lib.rs`**

Add module declaration near the top with other mod declarations:

```rust
mod mail;
```

Add `MailState` to `NodeState`:

```rust
#[derive(Default)]
struct NodeState {
    thread: Option<thread::JoinHandle<()>>,
    shutdown_tx: Option<tokio::sync::watch::Sender<bool>>,
    publish_tx: Option<tokio::sync::mpsc::Sender<event_loop::PublishRequest>>,
    fetch_tx: Option<tokio::sync::mpsc::Sender<event_loop::FetchRequest>>,
    generation: u64,
    node_addr: String,
    /// Mail state for the client mail receive path.
    mail: mail::MailState,
}
```

Update the `Default` impl (since `MailState` has a custom constructor):

```rust
impl Default for NodeState {
    fn default() -> Self {
        Self {
            thread: None,
            shutdown_tx: None,
            publish_tx: None,
            fetch_tx: None,
            generation: 0,
            node_addr: String::new(),
            mail: mail::MailState::new(),
        }
    }
}
```

Note: Remove the `#[derive(Default)]` from `NodeState` since we now have a manual `impl Default`.

Add the Tauri commands:

```rust
/// Get inbox entries by walking the CAS Merkle tree.
#[tauri::command]
async fn get_inbox(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<mail::InboxEntry>, String> {
    let (root_cid, cache_dir, fetch_tx) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        let root_cid = guard
            .mail
            .root_cid
            .ok_or_else(|| "no mailbox root CID available".to_string())?;
        let cache_dir = guard.mail.cache_dir.clone();
        let fetch_tx = guard
            .fetch_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?;
        (root_cid, cache_dir, fetch_tx)
    };

    mail::get_inbox_inner(root_cid, &cache_dir, &fetch_tx).await
}

/// Fetch and deserialize a full HarmonyMessage by hex-encoded CID.
#[tauri::command]
async fn get_mail_message(
    message_cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<mail::MailMessage, String> {
    let (cache_dir, fetch_tx) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        let cache_dir = guard.mail.cache_dir.clone();
        let fetch_tx = guard
            .fetch_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?;
        (cache_dir, fetch_tx)
    };

    mail::get_mail_message_inner(&message_cid, &cache_dir, &fetch_tx).await
}
```

Register both commands in the `tauri::generate_handler!` macro:

```rust
        .invoke_handler(tauri::generate_handler![
            list_vine_videos,
            follow_vine_creator,
            unfollow_vine_creator,
            mark_vine_viewed,
            publish_vine,
            start_node,
            stop_node,
            connect_zenoh,
            disconnect_zenoh,
            publish_profile,
            send_message,
            get_node_addr,
            list_content,
            pin_content,
            unpin_content,
            burn_content,
            fetch_content,
            get_inbox,
            get_mail_message,
        ])
```

- [ ] **Step 4: Run tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo test
```

Expected: all existing tests pass plus the 3 new mail tests.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/
git commit -m "feat: add client mail module with CAS tree walking and disk cache

Add MailState, cached_fetch (flat file cache at ~/.harmony/mail-cache/),
get_inbox (root->folder->page tree walk), and get_mail_message (full
HarmonyMessage deserialization). Register as Tauri commands."
```

---

## Task 5: Client event loop mail subscription (harmony-client repo)

**Files:** Modify `src-tauri/src/event_loop.rs`, `src-tauri/src/lib.rs`.

- [ ] **Step 1: Add mail root CID event payload type to `src-tauri/src/lib.rs`**

Add near the other payload structs:

```rust
/// Mail root CID update emitted to the frontend via IPC.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailRootUpdate {
    /// Hex-encoded 32-byte root CID.
    pub root_cid: String,
}
```

- [ ] **Step 2: Add mail subscription to `src-tauri/src/event_loop.rs`**

After the existing `dispatch_action` calls for `harmony/announce/*` (around line 207), add a mail subscription. The node address is needed for the topic, so pass it into the event loop.

First, update the `run()` function signature to accept the node address:

```rust
pub async fn run(
    mut runtime: NodeRuntime<MemoryBookStore>,
    startup_actions: Vec<RuntimeAction>,
    app: AppHandle,
    endpoint: Option<String>,
    node_addr: String,            // <-- NEW
    ready_tx: oneshot::Sender<Result<(), String>>,
    mut shutdown: watch::Receiver<bool>,
    mut publish_rx: mpsc::Receiver<PublishRequest>,
    mut fetch_rx: mpsc::Receiver<FetchRequest>,
) {
```

After the `harmony/announce/*` subscription (around line 207, before `ready_tx.send(Ok(()))`), add:

```rust
    // Subscribe to mailbox root CID updates for this node's address.
    if !node_addr.is_empty() {
        let mail_topic = format!("harmony/messages/{node_addr}/inbox");
        dispatch_action(
            RuntimeAction::Subscribe {
                key_expr: mail_topic.clone(),
            },
            &session,
            &zenoh_tx,
            &udp,
            &broadcast_addr,
            &app,
            &closing,
            &own_zid,
        )
        .await;

        // Catch-up query: fetch current root CID from gateway queryable.
        let session_clone = session.clone();
        let app_clone = app.clone();
        tokio::spawn(async move {
            match session_clone.get(&mail_topic).await {
                Ok(replies) => {
                    let deadline = std::time::Duration::from_secs(5);
                    let _ = tokio::time::timeout(deadline, async {
                        while let Ok(reply) = replies.recv_async().await {
                            if let Ok(sample) = reply.result() {
                                let payload = sample.payload().to_bytes().to_vec();
                                if payload.len() == 32 {
                                    let root_hex = hex::encode(&payload);
                                    let _ = app_clone.emit(
                                        "mail-root-updated",
                                        &crate::MailRootUpdate {
                                            root_cid: root_hex,
                                        },
                                    );
                                }
                            }
                        }
                    })
                    .await;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "mail catch-up query failed (gateway may not be running)");
                }
            }
        });
    }
```

- [ ] **Step 3: Handle mail topic in `emit_frontend_event`**

Add a new branch to `emit_frontend_event()` in `event_loop.rs`:

```rust
    } else if key_expr.starts_with("harmony/messages/") && key_expr.ends_with("/inbox") {
        // Mail root CID update: payload is 32 bytes (raw CID).
        if payload.len() == 32 {
            let root_hex = hex::encode(payload);
            let _ = app.emit(
                "mail-root-updated",
                &crate::MailRootUpdate {
                    root_cid: root_hex,
                },
            );
        }
```

Add this after the `harmony/announce/` branch and before the telemetry branch.

- [ ] **Step 4: Update `start_node` to pass `node_addr` to event loop**

In `src-tauri/src/lib.rs`, in the `start_node` function where `event_loop::run` is called (around line 298), add `node_addr` to the call:

Change:

```rust
                    event_loop::run(
                        runtime,
                        startup_actions,
                        app_clone,
                        ep_clone,
                        ready_tx,
                        shutdown_rx,
                        publish_rx,
                        fetch_rx,
                    )
```

To:

```rust
                    event_loop::run(
                        runtime,
                        startup_actions,
                        app_clone,
                        ep_clone,
                        node_addr,       // <-- NEW
                        ready_tx,
                        shutdown_rx,
                        publish_rx,
                        fetch_rx,
                    )
```

The `node_addr` variable is already in scope (line 236: `let node_addr = hex::encode(our_addr_bytes);`).

- [ ] **Step 5: Store root CID in MailState on IPC event**

Add a listener inside `start_node` (after the `ready_rx` is awaited, inside the `Ok(Ok(()))` branch) that updates `MailState` when a `mail-root-updated` event fires:

Actually, this is simpler if done in the event loop itself. Since the event loop already emits to the frontend, add a second side effect: update MailState via a channel.

Alternative (simpler): Listen for the `mail-root-updated` event in the frontend and let the frontend call `get_inbox` when it fires. The root CID doesn't need to be stored in `NodeState.mail.root_cid` at all -- the frontend can pass it back with each call.

Revised approach: Store the root CID in MailState from the event loop's emit_frontend_event path. Since emit_frontend_event doesn't have access to NodeState, use a watch channel instead.

Simplest approach: When the event loop emits `mail-root-updated`, also update `MailState`. Since the event loop runs on a separate thread, use an `Arc<Mutex<Option<[u8; 32]>>>` shared between the event loop and NodeState. OR: just store it in `NodeState` from the Tauri command side -- let the frontend listen for `mail-root-updated`, decode the hex CID, and pass it to `get_inbox` as a parameter.

Final design (cleanest): Make `get_inbox` take an optional `root_cid` hex parameter. If provided, use it. If not, use `MailState.root_cid`. The frontend always passes the latest root CID it received from the IPC event. This avoids needing shared state between event loop and commands.

Update `get_inbox` command to accept root CID:

```rust
#[tauri::command]
async fn get_inbox(
    root_cid: Option<String>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<mail::InboxEntry>, String> {
    let (cid_bytes, cache_dir, fetch_tx) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;

        let cid_bytes: [u8; 32] = if let Some(ref hex_cid) = root_cid {
            hex::decode(hex_cid)
                .map_err(|e| format!("invalid root CID hex: {e}"))?
                .try_into()
                .map_err(|v: Vec<u8>| format!("root CID wrong length: {} bytes", v.len()))?
        } else {
            guard
                .mail
                .root_cid
                .ok_or_else(|| "no mailbox root CID available".to_string())?
        };

        let cache_dir = guard.mail.cache_dir.clone();
        let fetch_tx = guard
            .fetch_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?;
        (cid_bytes, cache_dir, fetch_tx)
    };

    mail::get_inbox_inner(cid_bytes, &cache_dir, &fetch_tx).await
}
```

- [ ] **Step 6: Run tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo check && cargo test
```

Expected: all tests pass.

- [ ] **Step 7: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/
git commit -m "feat: add mail Zenoh subscription and catch-up query

Subscribe to harmony/messages/{addr}/inbox for root CID updates.
Handle in emit_frontend_event, emit mail-root-updated IPC event.
Add catch-up session.get() on startup for initial root CID."
```

---

## Task 6: Client frontend (harmony-client repo)

**Files:** Modify `src/lib/types.ts`. Create `src/lib/mail-service.ts`, `src/lib/components/MailMode.svelte`, `src/lib/components/InboxList.svelte`, `src/lib/components/MailDetail.svelte`. Modify `src/App.svelte`, `src/lib/components/Layout.svelte`.

- [ ] **Step 1: Add types to `src/lib/types.ts`**

Add at the end of the file:

```typescript
// ── Mail Types ──────────────────────────────────────────────────────

export interface InboxEntry {
  messageCid: string;
  senderAddress: string;
  timestamp: number;
  subjectSnippet: string;
  read: boolean;
}

export interface MailMessage {
  subject: string;
  body: string;
  senderAddress: string;
  timestamp: number;
  recipients: string[];
  isReply: boolean;
  hasAttachments: boolean;
}
```

Update the `AppMode` type:

Change:

```typescript
export type AppMode = 'messages' | 'vines' | 'files' | 'spellbook';
```

To:

```typescript
export type AppMode = 'messages' | 'vines' | 'files' | 'spellbook' | 'mail';
```

- [ ] **Step 2: Create `src/lib/mail-service.ts`**

```typescript
import type { TauriAdapter } from './zenoh-service';
import type { InboxEntry, MailMessage } from './types';

/**
 * Manages mail inbox state and message viewing.
 *
 * Listens for 'mail-root-updated' IPC events to refresh the inbox.
 * Follows the same service pattern as MessageService/VineService.
 */
export class MailService {
  entries: InboxEntry[] = [];
  selectedCid: string | null = null;
  selectedMessage: MailMessage | null = null;
  loading = false;
  /** Called whenever service state changes so the UI can re-render. */
  onChange: (() => void) | null = null;

  private adapter: TauriAdapter | null = null;
  private unlisteners: Array<() => void> = [];
  private latestRootCid: string | null = null;

  /** Connect a Tauri adapter and start listening for mail events. */
  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return;
    this.adapter = adapter;

    const unlisten = await adapter.listen(
      'mail-root-updated',
      (event) => {
        const payload = event.payload as { rootCid: string };
        if (payload.rootCid) {
          this.latestRootCid = payload.rootCid;
          this.refreshInbox();
        }
      },
    );
    this.unlisteners.push(unlisten);
  }

  /** Refresh the inbox entries from the backend. */
  async refreshInbox(): Promise<void> {
    if (!this.adapter) return;
    this.loading = true;
    this.onChange?.();
    try {
      const entries = await this.adapter.invoke('get_inbox', {
        rootCid: this.latestRootCid,
      }) as InboxEntry[];
      this.entries = entries;
    } catch (err) {
      console.error('Failed to refresh inbox:', err);
    } finally {
      this.loading = false;
      this.onChange?.();
    }
  }

  /** Open a message by CID. */
  async openMessage(cid: string): Promise<void> {
    if (!this.adapter) return;
    this.selectedCid = cid;
    this.loading = true;
    this.onChange?.();
    try {
      const message = await this.adapter.invoke('get_mail_message', {
        messageCid: cid,
      }) as MailMessage;
      this.selectedMessage = message;
    } catch (err) {
      console.error('Failed to open message:', err);
      this.selectedMessage = null;
    } finally {
      this.loading = false;
      this.onChange?.();
    }
  }

  /** Close the selected message. */
  closeMessage(): void {
    this.selectedCid = null;
    this.selectedMessage = null;
    this.onChange?.();
  }

  /** Register an external unlisten handle for cleanup. */
  addUnlisten(fn: () => void): void {
    this.unlisteners.push(fn);
  }

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
  }
}
```

- [ ] **Step 3: Create `src/lib/components/InboxList.svelte`**

```svelte
<script lang="ts">
  import type { InboxEntry } from '../types';

  let {
    entries = [],
    selectedCid = null,
    onSelect,
  }: {
    entries: InboxEntry[];
    selectedCid: string | null;
    onSelect: (cid: string) => void;
  } = $props();

  function formatTime(timestamp: number): string {
    const date = new Date(timestamp * 1000);
    const now = new Date();
    const diffMs = now.getTime() - date.getTime();
    const diffDays = Math.floor(diffMs / (1000 * 60 * 60 * 24));

    if (diffDays === 0) {
      return date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
    } else if (diffDays === 1) {
      return 'Yesterday';
    } else if (diffDays < 7) {
      return date.toLocaleDateString([], { weekday: 'short' });
    } else {
      return date.toLocaleDateString([], { month: 'short', day: 'numeric' });
    }
  }

  function truncateAddress(addr: string): string {
    if (addr.length <= 12) return addr;
    return addr.slice(0, 6) + '...' + addr.slice(-4);
  }
</script>

<div class="inbox-list">
  {#if entries.length === 0}
    <div class="empty-state">
      <p>No messages yet</p>
    </div>
  {:else}
    {#each entries as entry (entry.messageCid)}
      <button
        class="inbox-entry"
        class:selected={selectedCid === entry.messageCid}
        class:unread={!entry.read}
        onclick={() => onSelect(entry.messageCid)}
      >
        <div class="entry-left">
          {#if !entry.read}
            <span class="unread-dot"></span>
          {:else}
            <span class="read-spacer"></span>
          {/if}
        </div>
        <div class="entry-content">
          <div class="entry-header">
            <span class="sender">{truncateAddress(entry.senderAddress)}</span>
            <span class="timestamp">{formatTime(entry.timestamp)}</span>
          </div>
          <div class="subject">{entry.subjectSnippet || '(no subject)'}</div>
        </div>
      </button>
    {/each}
  {/if}
</div>

<style>
  .inbox-list {
    display: flex;
    flex-direction: column;
    overflow-y: auto;
    height: 100%;
  }

  .empty-state {
    display: flex;
    align-items: center;
    justify-content: center;
    height: 100%;
    color: var(--text-muted, #949ba4);
  }

  .inbox-entry {
    display: flex;
    align-items: flex-start;
    gap: 8px;
    padding: 10px 12px;
    border: none;
    border-bottom: 1px solid var(--border, #2a2d31);
    background: transparent;
    cursor: pointer;
    text-align: left;
    width: 100%;
    color: var(--text-primary, #dbdee1);
    transition: background 0.15s ease;
  }

  .inbox-entry:hover {
    background: var(--bg-hover, rgba(255, 255, 255, 0.04));
  }

  .inbox-entry.selected {
    background: var(--bg-active, rgba(88, 101, 242, 0.1));
    border-left: 3px solid var(--accent, #5865f2);
    padding-left: 9px;
  }

  .inbox-entry.unread .sender {
    font-weight: 600;
  }

  .entry-left {
    flex-shrink: 0;
    width: 10px;
    padding-top: 4px;
  }

  .unread-dot {
    display: block;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--accent, #5865f2);
  }

  .read-spacer {
    display: block;
    width: 8px;
    height: 8px;
  }

  .entry-content {
    flex: 1;
    min-width: 0;
  }

  .entry-header {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    gap: 8px;
    margin-bottom: 2px;
  }

  .sender {
    font-size: 13px;
    color: var(--text-primary, #dbdee1);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .timestamp {
    font-size: 11px;
    color: var(--text-muted, #949ba4);
    flex-shrink: 0;
  }

  .subject {
    font-size: 12px;
    color: var(--text-secondary, #b5bac1);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
</style>
```

- [ ] **Step 4: Create `src/lib/components/MailDetail.svelte`**

```svelte
<script lang="ts">
  import type { MailMessage } from '../types';

  let {
    message = null,
    loading = false,
  }: {
    message: MailMessage | null;
    loading?: boolean;
  } = $props();

  function formatTimestamp(timestamp: number): string {
    const date = new Date(timestamp * 1000);
    return date.toLocaleString([], {
      year: 'numeric',
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit',
    });
  }

  function truncateAddress(addr: string): string {
    if (addr.length <= 12) return addr;
    return addr.slice(0, 6) + '...' + addr.slice(-4);
  }
</script>

<div class="mail-detail">
  {#if loading}
    <div class="empty-state">
      <p>Loading...</p>
    </div>
  {:else if message}
    <header class="message-header">
      <h2 class="subject">{message.subject || '(no subject)'}</h2>
      <div class="meta">
        <span class="from">From: {truncateAddress(message.senderAddress)}</span>
        <span class="date">{formatTimestamp(message.timestamp)}</span>
      </div>
      {#if message.recipients.length > 0}
        <div class="recipients">
          To: {message.recipients.map(truncateAddress).join(', ')}
        </div>
      {/if}
      <div class="badges">
        {#if message.isReply}
          <span class="badge reply">Reply</span>
        {/if}
        {#if message.hasAttachments}
          <span class="badge attachment">Attachments</span>
        {/if}
      </div>
    </header>
    <div class="message-body">
      <pre class="body-text">{message.body}</pre>
    </div>
  {:else}
    <div class="empty-state">
      <p>Select a message to read</p>
    </div>
  {/if}
</div>

<style>
  .mail-detail {
    display: flex;
    flex-direction: column;
    height: 100%;
    overflow: hidden;
  }

  .empty-state {
    display: flex;
    align-items: center;
    justify-content: center;
    height: 100%;
    color: var(--text-muted, #949ba4);
  }

  .message-header {
    padding: 16px 20px;
    border-bottom: 1px solid var(--border, #2a2d31);
    flex-shrink: 0;
  }

  .subject {
    font-size: 18px;
    font-weight: 600;
    color: var(--text-primary, #dbdee1);
    margin: 0 0 8px 0;
  }

  .meta {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    gap: 12px;
    margin-bottom: 4px;
  }

  .from {
    font-size: 13px;
    color: var(--text-secondary, #b5bac1);
  }

  .date {
    font-size: 12px;
    color: var(--text-muted, #949ba4);
    flex-shrink: 0;
  }

  .recipients {
    font-size: 12px;
    color: var(--text-muted, #949ba4);
    margin-bottom: 4px;
  }

  .badges {
    display: flex;
    gap: 6px;
    margin-top: 6px;
  }

  .badge {
    font-size: 11px;
    padding: 2px 6px;
    border-radius: 3px;
    color: var(--text-primary, #dbdee1);
  }

  .badge.reply {
    background: rgba(88, 101, 242, 0.2);
  }

  .badge.attachment {
    background: rgba(87, 242, 135, 0.2);
  }

  .message-body {
    flex: 1;
    overflow-y: auto;
    padding: 16px 20px;
  }

  .body-text {
    font-family: inherit;
    font-size: 14px;
    line-height: 1.6;
    color: var(--text-primary, #dbdee1);
    white-space: pre-wrap;
    word-wrap: break-word;
    margin: 0;
  }
</style>
```

- [ ] **Step 5: Create `src/lib/components/MailMode.svelte`**

```svelte
<script lang="ts">
  import type { InboxEntry, MailMessage } from '../types';
  import InboxList from './InboxList.svelte';
  import MailDetail from './MailDetail.svelte';

  let {
    entries = [],
    selectedCid = null,
    selectedMessage = null,
    loading = false,
    onSelect,
  }: {
    entries: InboxEntry[];
    selectedCid: string | null;
    selectedMessage: MailMessage | null;
    loading?: boolean;
    onSelect: (cid: string) => void;
  } = $props();
</script>

<div class="mail-mode">
  <div class="inbox-panel">
    <div class="inbox-header">
      <h3>Inbox</h3>
    </div>
    <InboxList {entries} {selectedCid} {onSelect} />
  </div>
  <div class="detail-panel">
    <MailDetail message={selectedMessage} {loading} />
  </div>
</div>

<style>
  .mail-mode {
    display: flex;
    height: 100%;
    overflow: hidden;
  }

  .inbox-panel {
    width: 38%;
    min-width: 280px;
    max-width: 450px;
    display: flex;
    flex-direction: column;
    border-right: 1px solid var(--border, #2a2d31);
    background: var(--bg-secondary, #2b2d31);
  }

  .inbox-header {
    padding: 12px 16px;
    border-bottom: 1px solid var(--border, #2a2d31);
    flex-shrink: 0;
  }

  .inbox-header h3 {
    margin: 0;
    font-size: 15px;
    font-weight: 600;
    color: var(--text-primary, #dbdee1);
  }

  .detail-panel {
    flex: 1;
    background: var(--bg-primary, #313338);
    overflow: hidden;
  }
</style>
```

- [ ] **Step 6: Update `src/lib/components/Layout.svelte`**

Add `mailContent` snippet prop and mail mode rendering:

```svelte
<script lang="ts">
  import type { Snippet } from 'svelte';
  import type { AppMode } from '../types';

  let { nav, textFeed, mediaFeed, vineFeed, fileBrowser, fileDetailPanel, spellbookContent, spellbookDetail, mailContent, settingsPanel, collapsed = false, showSettings = false, mode = 'messages' }: {
    nav: Snippet;
    textFeed: Snippet;
    mediaFeed: Snippet;
    vineFeed?: Snippet;
    fileBrowser?: Snippet;
    fileDetailPanel?: Snippet;
    spellbookContent?: Snippet;
    spellbookDetail?: Snippet;
    mailContent?: Snippet;
    settingsPanel?: Snippet;
    collapsed?: boolean;
    showSettings?: boolean;
    mode?: AppMode;
  } = $props();
</script>
```

Add a new conditional branch in the template, before the `{:else}` (messages) branch:

```svelte
  {:else if mode === 'mail' && mailContent}
    <main class="mail-area">
      {@render mailContent()}
    </main>
```

Add the CSS:

```css
  .layout.mail-mode {
    grid-template-columns: var(--nav-width) 1fr;
    grid-template-areas: "nav mail";
  }
  .layout.mail-mode.collapsed {
    grid-template-columns: var(--nav-width-collapsed) 1fr;
    grid-template-areas: "nav mail";
  }
  .mail-area {
    grid-area: mail;
    background: var(--bg-primary);
    overflow: hidden;
    display: flex;
    flex-direction: column;
  }
```

Add `mail-mode` class to the layout div:

```svelte
<div class="layout" class:collapsed class:files-mode={mode === 'files' && fileBrowser} class:vine-mode={mode === 'vines' && vineFeed} class:spellbook-mode={mode === 'spellbook' && spellbookContent} class:mail-mode={mode === 'mail' && mailContent}>
```

- [ ] **Step 7: Wire into `src/App.svelte`**

Add import:

```typescript
import { MailService } from './lib/mail-service';
import MailMode from './lib/components/MailMode.svelte';
```

Create service instance (near other service instantiations):

```typescript
const mailService = new MailService();
$effect(() => () => mailService.destroy());

let mailEntries = $state<import('./lib/types').InboxEntry[]>([]);
let mailSelectedCid = $state<string | null>(null);
let mailSelectedMessage = $state<import('./lib/types').MailMessage | null>(null);
let mailLoading = $state(false);

mailService.onChange = () => {
  mailEntries = [...mailService.entries];
  mailSelectedCid = mailService.selectedCid;
  mailSelectedMessage = mailService.selectedMessage;
  mailLoading = mailService.loading;
};
```

In the Tauri adapter wiring block (the `async () => { try { ... } }` IIFE), add after `await navService.connectAdapter(adapter);`:

```typescript
      await mailService.connectAdapter(adapter);
```

Add mail handler:

```typescript
function handleMailSelect(cid: string) {
  mailService.openMessage(cid);
}
```

Add the mail snippet to the Layout component:

```svelte
  {#snippet mailContent()}
    <MailMode
      entries={mailEntries}
      selectedCid={mailSelectedCid}
      selectedMessage={mailSelectedMessage}
      loading={mailLoading}
      onSelect={handleMailSelect}
    />
  {/snippet}
```

Add this snippet after `{#snippet spellbookDetail()}...{/snippet}` and before the closing `</Layout>`.

- [ ] **Step 8: Add mail to NavPanel mode switcher**

The NavPanel already has a mode switcher. Find where `onModeChange` is called for other modes and add a mail button. In `src/lib/components/NavPanel.svelte`, find the mode buttons and add:

```svelte
<button
  class="mode-btn"
  class:active={appMode === 'mail'}
  onclick={() => onModeChange('mail')}
  title="Mail"
>
  <!-- Mail icon (envelope) -->
  <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
    <rect x="2" y="4" width="20" height="16" rx="2" />
    <polyline points="22,4 12,13 2,4" />
  </svg>
</button>
```

- [ ] **Step 9: Run frontend dev server to verify**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npm run dev
```

Verify:
1. The mail icon appears in the NavPanel mode switcher
2. Clicking it switches to mail mode
3. The two-panel layout renders (empty inbox + "Select a message to read")
4. No console errors

- [ ] **Step 10: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src/
git commit -m "feat: add mail inbox UI with two-panel layout

Add MailService (listens for mail-root-updated IPC), InboxList (unread
dot, sender, subject, timestamp), MailDetail (subject, body, badges),
and MailMode (38%/62% two-panel split). Wire into App.svelte and add
mail icon to NavPanel mode switcher."
```
