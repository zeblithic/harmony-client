# Client Mail Receive Path (ZEB-114 Phase 2)

## Overview

Add a native mail receive path to harmony-client so users can read mail delivered through the gateway's Merkle bridge (Phase 1) without an IMAP client. The client subscribes to Zenoh mailbox root CID updates, walks the CAS Merkle tree, and renders an inbox with message body viewing.

This phase also includes two gateway-side prerequisites: extracting shared mailbox types into a standalone crate, and activating the deferred Zenoh publishing.

## Scope

- **Shared mailbox types:** New `harmony-mailbox` crate with MailRoot/MailFolder/MailPage/MessageEntry and HarmonyMessage wire format types, used by both harmony-mail and harmony-client.
- **Gateway Zenoh activation:** Wire up the deferred ZenohConfig so the gateway publishes root CID updates and serves catch-up queryables.
- **Client Zenoh subscription:** Subscribe to `harmony/messages/{addr_hex}/inbox` for real-time root CID updates, with catch-up query on startup.
- **CAS tree walking:** Fetch and deserialize Merkle tree blobs (root, folder, page) to produce inbox entry lists.
- **Local CAS cache:** Flat file cache at `~/.harmony/mail-cache/` for immutable block caching.
- **Inbox UI:** New `mail` app mode with two-panel layout (inbox list + message detail).

## Non-Scope

- Folder navigation (inbox/sent/drafts/trash switching) — follow-up issue.
- Mark-as-read / flag management — follow-up issue.
- Compose / send — Phase 3.
- Pagination beyond the head page (100 entries) — follow-up.
- Attachment viewing — follow-up.
- Offline-first sync or conflict resolution — CAS immutability makes caching trivial, no conflicts possible in read-only mode.

## Architecture

```
Gateway (harmony repo)                    Client (harmony-client repo)
========================                  ============================

SMTP in → Phase 5 →                      Zenoh subscription
  MailboxManager.insert_message()           "harmony/messages/{addr}/inbox"
    │                                         │
    ├─ CAS write (DiskBookStore)              │ root CID (32 bytes)
    │                                         ▼
    └─ mpsc → async task                    MailState
              session.put(root_cid)           │
                                              ├─ IPC: 'mail-root-updated'
Queryable (catch-up)                          ▼
  "harmony/messages/{addr}/inbox"           get_inbox() → tree walk
    → responds with current root CID          │
                                              ├─ cached_fetch(root)
CAS queryable (harmony-node)                  ├─ cached_fetch(folder)
  "harmony/content/{prefix}/**"               ├─ cached_fetch(page)
    → serves CAS blocks                      ▼
                                            Vec<InboxEntry> → UI
                                              │
                                            get_mail_message(cid)
                                              ├─ cached_fetch(message)
                                              ▼
                                            MailMessage → UI
```

## Design Decisions

- **Shared crate over duplication** — The mailbox wire format is the contract between gateway and client. A shared crate (`harmony-mailbox`) keeps it in one place. The crate has minimal dependencies (`thiserror`, `harmony-identity`).
- **Disk cache for CAS blocks** — CAS blocks are immutable (same CID = same content), so cached blocks never go stale. Only the root CID pointer is mutable. This means first-fetch-from-network, then-read-from-disk forever after.
- **Two-panel layout** — Classic desktop email pattern: inbox list on the left (38% width), message detail on the right. Matches the desktop form factor of the Tauri app. No navigation state needed — both panels visible simultaneously.
- **New `mail` app mode** — Mail is a fundamentally different communication pattern from real-time channel messaging. A dedicated mode keeps it clean and follows the existing AppMode architecture.
- **Head page only** — Phase 2 returns only the first page of inbox entries (up to 100). Sufficient for initial launch. Pagination is a follow-up.
- **mpsc channel for Zenoh puts** — MailboxManager runs in `spawn_blocking` (sync context). Rather than needing a tokio Handle, it sends `(addr_hex, root_cid)` through an mpsc channel to a background async task that does `session.put()`.
- **Gateway Zenoh included** — Activating the deferred ZenohConfig is small (~1 task) and enables end-to-end testing. Without it, the client would need mock data.

## Subsystem 1: `harmony-mailbox` Shared Crate

### Location

New crate at `crates/harmony-mailbox/` in the harmony repo.

### Contents

**From `error.rs`** — New `MailboxError` enum with wire-format variants only:
- `MessageTooShort`, `UnsupportedVersion`, `Truncated`, `TrailingBytes`
- `InvalidMagic`, `InvalidFlag`, `TooManyEntries`, `FieldTooLong`
- `InvalidUtf8`, `InvalidMessageType`, `InvalidRecipientType`, `InvalidInReplyToFlag`
- `SubjectTooLong`, `BodyTooLong`, `TooManyRecipients`, `TooManyAttachments`
- `FilenameTooLong`, `MimeTypeTooLong`, `StringTooLong`

**From `mailbox.rs`** — All types and constants:
- `MailRoot`, `MailFolder`, `MailPage`, `MessageEntry`, `FolderKind`
- Constants: `MAILBOX_VERSION`, `ROOT_MAGIC`, `FOLDER_MAGIC`, `PAGE_MAGIC`, `PAGE_CAPACITY`, `MAX_SNIPPET_LEN`, `FOLDER_COUNT`, `FOLDER_NAMES`, `EMPTY_CID`
- Helper: `truncate_utf8`

**From `message.rs`** — All types and constants (excluding creation helpers):
- `HarmonyMessage`, `MailMessageType`, `RecipientType`, `MessageFlags`, `Recipient`, `AttachmentRef`
- Constants: `CID_LEN`, `MESSAGE_ID_LEN`, `ADDRESS_HASH_LEN`, `MAX_SUBJECT_LEN`, `MAX_BODY_LEN`, `MAX_RECIPIENTS`, `MAX_ATTACHMENTS`
- Excludes `unique_message_id()` (requires `blake3`, not needed for read-only Phase 2)

### Dependencies

- `thiserror` — error derive macro
- `harmony-identity` — re-exports `ADDRESS_HASH_LENGTH` as `ADDRESS_HASH_LEN`

### Impact on `harmony-mail`

- `mailbox.rs` and `message.rs` replaced with `pub use harmony_mailbox::*` re-exports
- `error.rs`: `MailError` keeps SMTP-specific variants (`UnknownCommand`, `InvalidIdentity`), gains `#[from] MailboxError` variant
- `unique_message_id()` stays in harmony-mail (depends on `blake3`, only used for message creation)
- All existing tests should pass unchanged

## Subsystem 2: Gateway Zenoh Activation

### Startup (`run()` in `server.rs`)

1. Parse ZenohConfig from config.toml (already done — `config.zenoh`)
2. If `zenoh.enabled`:
   - Open session: `zenoh::open(zenoh::Config::default())` or with explicit endpoint
   - Wrap in `Arc<zenoh::Session>`
3. Pass `Option<Arc<zenoh::Session>>` into MailboxManager constructor
4. Spawn background async task that drains an mpsc channel and calls `session.put()`
5. For each local user, register a queryable on `harmony/messages/{addr_hex}/inbox` that responds with current root CID (32 bytes)

### On Insert (`insert_message` in `mailbox_manager.rs`)

After updating root CID in HashMap + SQLite:
- If Zenoh mpsc sender is available, send `(addr_hex, new_root_cid)` through the channel
- Background async task receives it and calls `session.put("harmony/messages/{addr_hex}/inbox", &root_cid)`
- Errors logged and swallowed (non-critical path)

### mpsc Channel Design

```rust
pub struct ZenohPublisher {
    tx: mpsc::UnboundedSender<(String, [u8; 32])>,  // (addr_hex, root_cid)
}

impl ZenohPublisher {
    fn new(session: Arc<zenoh::Session>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some((addr_hex, root_cid)) = rx.recv().await {
                let topic = format!("harmony/messages/{addr_hex}/inbox");
                if let Err(e) = session.put(&topic, &root_cid[..]).await {
                    tracing::warn!(error = %e, "Zenoh root CID publish failed");
                }
            }
        });
        Self { tx }
    }
}
```

MailboxManager holds `Option<ZenohPublisher>` instead of `Option<Arc<zenoh::Session>>`.

## Subsystem 3: Client Tauri Backend

### New File: `src-tauri/src/mail.rs`

**MailState:**

```rust
pub struct MailState {
    /// Latest known root CID for our mailbox.
    pub root_cid: Option<[u8; 32]>,
    /// Local disk cache directory for fetched CAS blocks.
    pub cache_dir: PathBuf,
}
```

Cache path: `~/.harmony/mail-cache/`. Blocks stored as `{cid_hex}.bin` files. Atomic writes via tmp + rename.

**cached_fetch:**

```rust
pub async fn cached_fetch(
    cid: &[u8; 32],
    cache_dir: &Path,
    fetch_tx: &mpsc::Sender<FetchRequest>,
) -> Result<Vec<u8>, String> {
    let hex = hex::encode(cid);
    let path = cache_dir.join(format!("{hex}.bin"));
    // 1. Check local disk cache
    if let Ok(bytes) = std::fs::read(&path) {
        return Ok(bytes);
    }
    // 2. Fetch via Zenoh (reuses existing FetchRequest channel)
    let (reply_tx, reply_rx) = oneshot::channel();
    fetch_tx.send(FetchRequest { cid_hex: hex.clone(), reply: reply_tx }).await...;
    let bytes = reply_rx.await...?;
    // 3. Write to cache (atomic: tmp + rename)
    let tmp = cache_dir.join(format!("{hex}.bin.tmp"));
    std::fs::write(&tmp, &bytes).ok();
    std::fs::rename(&tmp, &path).ok();
    Ok(bytes)
}
```

**Tauri commands:**

`get_inbox()` — Tree walk returning structured inbox data:
1. Read current root CID from MailState (error if None)
2. `cached_fetch(root_cid)` → `MailRoot::from_bytes()`
3. `cached_fetch(inbox_folder_cid)` → `MailFolder::from_bytes()`
4. If folder has pages: `cached_fetch(head_page_cid)` → `MailPage::from_bytes()`
5. Return entries mapped to `InboxEntry`:

```rust
#[derive(Serialize)]
pub struct InboxEntry {
    pub message_cid: String,     // hex-encoded
    pub sender_address: String,  // hex-encoded
    pub timestamp: u64,
    pub subject_snippet: String,
    pub read: bool,
}
```

`get_mail_message(message_cid: String)` — Fetch and deserialize full message:
1. `cached_fetch(message_cid_bytes)` → `HarmonyMessage::from_bytes()`
2. Return:

```rust
#[derive(Serialize)]
pub struct MailMessage {
    pub subject: String,
    pub body: String,
    pub sender_address: String,  // hex-encoded
    pub timestamp: u64,
    pub recipients: Vec<String>, // hex-encoded addresses
    pub is_reply: bool,
    pub has_attachments: bool,
}
```

### Event Loop Changes (`event_loop.rs`)

At startup, after Zenoh session is established:
- Subscribe to `harmony/messages/{node_addr_hex}/inbox`
- Also do an initial `session.get()` on the same topic for catch-up
- On receiving payload (32 bytes): store in MailState, emit `mail-root-updated` IPC event with `{ rootCid: "<hex>" }`

## Subsystem 4: Client Svelte Frontend

### AppMode Extension

Add `'mail'` to the `AppMode` union type in `App.svelte`. Add mail icon to sidebar mode switcher.

### MailService (`src/lib/mail-service.ts`)

Follows the established service pattern (MessageService, VineService):

```typescript
export class MailService {
    entries: InboxEntry[] = [];
    selectedCid: string | null = null;
    selectedMessage: MailMessage | null = null;
    loading: boolean = false;
    onChange: (() => void) | null = null;

    connectAdapter(adapter: TauriAdapter): void
    // Listens to 'mail-root-updated' IPC → calls refreshInbox()

    async refreshInbox(): Promise<void>
    // invoke('get_inbox') → update entries, call onChange

    async openMessage(cid: string): Promise<void>
    // invoke('get_mail_message', { messageCid: cid }) → set selected*, call onChange

    closeMessage(): void
    // clear selected*, call onChange
}
```

**Types** (added to `src/lib/types.ts`):

```typescript
interface InboxEntry {
    message_cid: string;
    sender_address: string;
    timestamp: number;
    subject_snippet: string;
    read: boolean;
}

interface MailMessage {
    subject: string;
    body: string;
    sender_address: string;
    timestamp: number;
    recipients: string[];
    is_reply: boolean;
    has_attachments: boolean;
}
```

### Components

**`MailMode.svelte`** — Top-level view, two-panel layout:
- Left panel (38% width): `<InboxList>` with entries from MailService
- Right panel (flex): `<MailDetail>` with selected message, or empty state

**`InboxList.svelte`** — Scrollable entry list:
- Each row shows: unread dot (indigo), sender address, relative timestamp, subject snippet
- Selected entry has accent left border + highlighted background
- Click handler calls `mailService.openMessage(cid)`

**`MailDetail.svelte`** — Message body display:
- Header: subject (large), sender address, timestamp
- Body: scrollable plaintext content
- Empty state: "Select a message to read" centered text

## Data Flow

### New Mail (Push)

1. SMTP → gateway Phase 5 → `MailboxManager.insert_message()`
2. MailboxManager updates Merkle tree in CAS, sends `(addr, root_cid)` via mpsc
3. Background task: `session.put("harmony/messages/{addr}/inbox", root_cid)`
4. Client event_loop: Zenoh subscription fires → store root CID → emit `mail-root-updated`
5. MailService: receives event → `get_inbox()` → tree walk with caching → update entries
6. UI: InboxList re-renders reactively

### Open Message (Pull)

1. User clicks InboxList entry
2. `MailService.openMessage(cid)` → `invoke('get_mail_message', {cid})`
3. Rust: `cached_fetch(cid)` → `HarmonyMessage::from_bytes()` → return MailMessage
4. UI: MailDetail displays subject, sender, body

### Client Startup (Catch-Up)

1. Node starts → subscribe to `harmony/messages/{addr}/inbox`
2. Initial `session.get()` on same topic → gateway queryable responds with current root CID
3. Same flow as push from step 4 onward

### Cache Behavior

All CAS blocks are immutable. Once fetched, they're valid forever. On a new root CID, only the changed path is fetched (new root → new folder → new head page). Older pages and message bodies are already cached.

## Files Changed

### New (harmony repo)

- **`crates/harmony-mailbox/`** — Shared crate: `Cargo.toml`, `src/lib.rs` (re-exports), `src/error.rs` (MailboxError), `src/mailbox.rs` (tree types), `src/message.rs` (HarmonyMessage types)

### Modified (harmony repo)

- **`crates/harmony-mail/Cargo.toml`** — Add `harmony-mailbox` dependency
- **`crates/harmony-mail/src/mailbox.rs`** — Replace with `pub use harmony_mailbox::mailbox::*` re-exports
- **`crates/harmony-mail/src/message.rs`** — Replace with re-exports + keep `unique_message_id()` locally
- **`crates/harmony-mail/src/error.rs`** — Keep SMTP variants, add `#[from] MailboxError`
- **`crates/harmony-mail/src/server.rs`** — Open Zenoh session, spawn publisher task, pass to MailboxManager, register queryables
- **`crates/harmony-mail/src/mailbox_manager.rs`** — Accept `Option<ZenohPublisher>`, send root CID updates through mpsc on insert
- **`Cargo.toml` (workspace)** — Add `harmony-mailbox` to workspace members

### New (harmony-client repo)

- **`src-tauri/src/mail.rs`** — MailState, flat file cache (cached_fetch), get_inbox tree walk, get_mail_message deserialization
- **`src/lib/mail-service.ts`** — MailService class
- **`src/lib/components/MailMode.svelte`** — Two-panel layout container
- **`src/lib/components/InboxList.svelte`** — Inbox entry list
- **`src/lib/components/MailDetail.svelte`** — Message body display

### Modified (harmony-client repo)

- **`src-tauri/Cargo.toml`** — Add `harmony-mailbox` dependency (for mailbox/message type deserialization)
- **`src-tauri/src/lib.rs`** — Add `mod mail`, register new Tauri commands, add MailState to NodeState
- **`src-tauri/src/event_loop.rs`** — Add mail subscription + catch-up query, emit `mail-root-updated` IPC
- **`src/lib/types.ts`** — Add InboxEntry, MailMessage interfaces
- **`src/App.svelte`** — Add `'mail'` to AppMode, add mail icon to sidebar, wire MailService, render MailMode

## Testing

### harmony-mailbox

- Existing unit tests from `mailbox.rs` and `message.rs` move with the code unchanged (roundtrip, validation, edge cases)
- Verify harmony-mail compiles and all tests pass after re-export refactoring

### Gateway Zenoh Activation

- `mailbox_manager_publishes_root_cid` — Insert a message with a ZenohPublisher backed by a test mpsc receiver, verify the receiver gets `(addr_hex, new_root_cid)`
- `queryable_responds_with_current_root` — Register queryable, verify it responds with latest root CID for a known user

### Client Tauri Backend

- `cached_fetch_hits_disk_on_second_call` — Fetch a CID via mock Zenoh, verify second fetch reads from disk cache without Zenoh get()
- `get_inbox_walks_tree` — Seed cache directory with pre-built MailRoot/MailFolder/MailPage blob files, call `get_inbox()`, verify returned entries match
- `get_mail_message_deserializes` — Seed cache directory with a HarmonyMessage blob file, call `get_mail_message()`, verify returned fields

### Client Svelte Frontend

- `mail_service_refreshes_on_event` — Mock adapter, emit `mail-root-updated`, verify entries update
- `inbox_list_renders_entries` — Pass mock entries, verify rendering (sender, subject, unread dot)
- `mail_detail_shows_message` — Pass mock MailMessage, verify subject/body/sender displayed
