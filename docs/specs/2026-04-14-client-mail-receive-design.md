# ZEB-114 Phase 2: Client Mail Receive Path — Design

**Status:** Draft for review
**Date:** 2026-04-14
**Repos affected:** `harmony-client` (primary), `harmony` (one Zenoh queryable addition)
**Builds on:**
- Phase 1: harmony PR #229/#230 (gateway Merkle bridge → root CID publish)
- Phase 1.5: harmony PR #233 (gateway raw-mail publish for live receive)
- Phase B: harmony-client PR #41 (migrate dep to `harmony-mailbox`)

## Goal

Give the harmony-client an inbox that reflects the recipient's full mail history — not just live messages received while the app was open. The client subscribes to the gateway-published mailbox root CID, walks the CAS Merkle tree, and renders inbox entries from the discovered `MessageEntry` headers. Bodies are fetched lazily on click.

## Non-Goals (Phase 2)

- **Bidirectional sync.** No mechanism for pushing client-side state (read/unread, folder moves, deletes) back to the gateway. Multi-device users see local-only state until a future phase.
- **Sent / Drafts / Trash sync.** Gateway only populates Inbox (it's an SMTP receiver). Sent stays purely local via existing `store_sent()`. Drafts and Trash are local-only by design.
- **Body prefetch.** Tracked separately as ZEB-118.
- **Full-text search across body content.** Searchable via subject snippets only in Phase 2.
- **Encryption at rest beyond what already exists.**

## Constraints recap (decisions from brainstorm)

| Q | Decision |
|---|---|
| Sync scope on cold start | Header-only walk of full Inbox tree; bodies lazy on click |
| Trust model | Local is authoritative for state (read/unread/folder); gateway tree is additive only |
| Cold-start root retrieval | New Zenoh queryable on gateway at `harmony/mail/v1/{addr_hex}/root` |
| UX feedback | Subtle spinner during sync, error icon on failure (with tooltip) |
| Body fetch | Lazy on click for Phase 2 (background prefetch deferred to ZEB-118) |
| Sync triggers | Root push (live) + manual refresh button. No periodic poll. |
| Per-node error policy | Hybrid: strict for root/folder (abort), skip for page/entry (continue) |
| Architecture | Dedicated `mail_sync.rs` module owns walker; clean boundary from `mail.rs` and `event_loop.rs` |

## Architecture

```
┌─────────────────── harmony (gateway) ──────────────────────────┐
│                                                                │
│  mailbox_manager.rs                                            │
│  ┌──────────────┐   ┌─────────────┐   ┌────────────────────┐  │
│  │ ZenohPub:    │   │ NEW:        │   │ Existing storage:  │  │
│  │  /v1/{a}     │   │ Queryable   │   │ DiskBookStore      │  │
│  │  /v1/{a}/root│←─→│ on /root    │←─►│ (CAS, sled-backed) │  │
│  └──────────────┘   │ → 32B CID   │   └────────────────────┘  │
│                     └─────────────┘                            │
└────────────────────────┬───────────────────────────────────────┘
                         │ Zenoh
                         ▼
┌─────────────── harmony-client (Tauri) ─────────────────────────┐
│                                                                │
│  event_loop.rs             mail_sync.rs (NEW)      mail.rs     │
│  ┌──────────────┐         ┌────────────────┐    ┌──────────┐  │
│  │ /root sub    │ ──────► │ Walker state   │ ─► │ register_│  │
│  │ /root query  │ ──────► │ machine        │    │   header │  │
│  │ fetch_via_   │ ◄────── │ Error policy   │    │ receive_ │  │
│  │   zenoh      │ ─────►  │ Sync triggers  │    │   message│  │
│  └──────────────┘ replies └────────┬───────┘    └────┬─────┘  │
│                                    │                 │        │
│                                    │ mail-sync-status│        │
│                                    │ mail-received   │        │
│                                    ▼                 ▼        │
│                            ┌─────────────────────────────┐    │
│                            │ Tauri events → frontend     │    │
│                            └─────────────────────────────┘    │
│                                          │                    │
│                                          ▼                    │
│  src/lib/mail-service.ts ──► MailInbox.svelte (sync indicator,│
│                              refresh button, list)            │
│                              MailReader.svelte (lazy body)    │
└────────────────────────────────────────────────────────────────┘
```

## Wire & CAS contracts

### `harmony/mail/v1/{addr_hex}/root`

Same key serves two operations:

| Operation | Direction | Payload | Semantics |
|---|---|---|---|
| `put` (existing) | gateway → world | 32 raw bytes (root CID) | Coalesced latest-wins. Emitted on each successful SMTP delivery for `addr_hex`. |
| `query` (NEW in this phase) | client → gateway | empty | Gateway replies with current root CID (32 raw bytes), or empty reply if no mail yet for this address. |

Co-locating put and query on one key matches the Zenoh "querying subscriber" pattern: the client uses query for cold-start state, subscribe for live updates. Splitting into two keys would only invent a new convention.

### `harmony/content/{prefix}/{cid_hex}` (existing, unchanged)

Tier 2 storage already serves CAS blobs over Zenoh. The walker fetches all four node types via this path:

1. `MailRoot` blob (CID from `/root`)
2. `MailFolder` blob for Inbox (CID from `MailRoot.folder_cids[0]`)
3. Each `MailPage` blob (CIDs from `MailFolder.page_cids`)
4. Each `HarmonyMessage` body (CID from `MessageEntry.message_cid`, lazy on click)

The client's `event_loop.rs:407` already implements `fetch_via_zenoh()` against this exact path.

### Wire format dependence

All parsers already exist in `harmony-mailbox` (added as a client dep in PR #41):

- `MailRoot::from_bytes` → `[folder_cids: [[u8; 32]; 4], owner_hash, timestamp, ...]`
- `MailFolder::from_bytes` → `[page_cids: Vec<[u8; 32]>, message_count, unread_count]` (newest-first)
- `MailPage::from_bytes` → `[entries: Vec<MessageEntry>]` (≤100 per page)
- `MessageEntry::from_bytes` → `[message_cid, message_id, sender_hash, timestamp, read, subject_snippet]`
- `HarmonyMessage::from_bytes` → already used by Phase 0 raw-publish path

**No new wire types.** Phase 2 is purely consumption side, except for the gateway queryable.

### Single-folder scope

The walker descends into only `folder_cids[0]` (Inbox). Slots 1/2/3 (Sent/Drafts/Trash) are unpopulated by the gateway — they exist in the wire format for symmetry but aren't sourced from incoming SMTP. Outbound mail continues to be recorded by `store_sent()` directly to local state.

## `mail_sync.rs` — new module

### Public API

```rust
pub struct MailSync<R: Runtime = tauri::Wry> {
    state: Arc<Mutex<SyncState>>,
    fetch_tx: mpsc::Sender<FetchRequest>,
    refresh_tx: mpsc::Sender<RefreshRequest>,
    mail_mgr: Arc<Mutex<MailManager>>,
    own_addr_hex: String,
    app: AppHandle<R>,
    in_flight_bodies: Arc<Mutex<HashMap<[u8; 32], watch::Receiver<Option<Result<Vec<u8>, String>>>>>>,
}

impl<R: Runtime> MailSync<R> {
    pub fn new(
        fetch_tx: mpsc::Sender<FetchRequest>,
        refresh_tx: mpsc::Sender<RefreshRequest>,
        mail_mgr: Arc<Mutex<MailManager>>,
        own_addr_hex: String,
        app: AppHandle<R>,
    ) -> Self;

    pub async fn handle_root_push(self: Arc<Self>, payload: &[u8]);
    pub async fn handle_startup_query_reply(self: Arc<Self>, payload: Option<&[u8]>);
    pub async fn refresh_now(self: Arc<Self>);
    pub async fn fetch_body(self: Arc<Self>, cid: [u8; 32]) -> Result<Vec<u8>, String>;
}
```

The `MailSync<R>` type parameter exists so tests can instantiate against
`tauri::test::MockRuntime`; production callers use the default `Wry`. The
in-flight body map shares results via `watch::Receiver` rather than
`Shared` so the primary fetcher's `tx` can publish a single `Some(result)`
to all subscribers atomically.

### State machine

```rust
enum SyncState {
    Idle { last_walked_root: Option<[u8; 32]> },
    Walking {
        root: [u8; 32],
        started_at: Instant,
        pending_root: Option<[u8; 32]>,
        // The last_walked_root carried into Walking — preserved so a strict
        // failure can transition to Error without losing the previous
        // successful root.
        prev_last_walked_root: Option<[u8; 32]>,
    },
    Error { last_error: String, last_walked_root: Option<[u8; 32]> },
}
```

**Single-flight semantics:** when a new root push arrives during an active walk, the walker stores it as `pending_root` and the current pass's `finish_walk`/`finish_walk_error` transitions directly to a new `Walking` for the queued root under the same lock — the surrounding `run_walk_loop` then continues iterating. Inline dispatch avoids the `tokio::spawn` race where a newer root push could overtake an older queued one.

**Duplicate suppression:** `start_or_queue_walk` skips no-op walks when the incoming root equals the active `Walking::root`, the queued `pending_root`, or (for `Idle` only) the most recent `last_walked_root` — so the cold-start `get` reply plus the first live `/root` push for the same CID don't double-walk.

The `std::sync::Mutex` is held only for state transitions, never across `.await`. Walk loop runs as a single spawned tokio task that drives back-to-back passes inline.

### Walk algorithm (one pass)

```
1. Emit mail-sync-status { state: 'syncing' }.
2. Fetch root CID → MailRoot bytes.
   On failure: → Error state, abort, emit error event. (Strict per Q7.)
3. Parse MailRoot, take folder_cids[0] (Inbox). Skip slots 1/2/3.
4. Fetch Inbox MailFolder bytes.
   On failure: → Error state, abort. (Strict.)
5. Parse MailFolder, get page_cids list (newest-first).
6. For each page_cid (parallelize up to 8 concurrent fetches):
     a. Fetch MailPage bytes.
        On failure: log + record skipped page, continue. (Skip.)
     b. Parse MailPage, iterate MessageEntries.
     c. For each entry:
          On parse error: log + skip entry, continue. (Skip.)
          On success: call mail_mgr.register_header_only(entry).
            (MailManager dedups by message_id.)
7. For each newly-Inserted entry (not Duplicate), emit mail-received event.
8. Compute final state: if any skipped pages/entries → Error with summary;
   else → Idle, set last_walked_root = root.
9. Emit terminal mail-sync-status event.
10. If pending_root exists, re-enter at step 1 with that root.
```

Concurrency cap of 8 in-flight page fetches uses a `tokio::sync::Semaphore`. The cap is intentionally small — page sequence is short (typically <10 pages even for large mailboxes) and we don't want to monopolize the Zenoh fetch channel.

### Body fetch (lazy)

```
1. fetch_body(cid) called via Tauri command.
2. If cid is in_flight_bodies, await its Shared future. (Dedup.)
3. Otherwise: insert a Shared placeholder; send FetchRequest to fetch_tx;
   await reply.
4. Validate: BLAKE3(reply_bytes) == cid.
5. Parse as HarmonyMessage to validate structure.
6. Call mail_mgr.mark_body_received(cid_hex, &bytes).
7. Resolve the Shared future. Remove from in_flight_bodies.
8. Return body to caller.
```

### Testability

- Mock `fetch_tx` with a test channel feeding pre-canned bytes — tests walker logic without Zenoh.
- Real `MailManager` with `tempdir` storage — tests dedup and persistence.
- Each error policy branch has a deterministic test (page 404 → continue; folder 404 → abort).

## `mail.rs` (MailManager) extensions

### New entry state

```rust
pub struct MailEntry {
    pub cid: String,
    pub message_id: [u8; 16],
    pub sender_hash: [u8; 16],
    pub timestamp_secs: u64,
    pub subject_snippet: String,
    pub read: bool,
    pub folder: FolderKind,
    #[serde(default)]
    pub body_state: BodyState,
}

#[derive(Default)]
pub enum BodyState {
    #[default]
    Local,    // blob exists in {data_dir}/mail/blobs/{cid}.bin
    Pending,  // header-only, body not yet fetched
}
```

`#[serde(default)]` ensures existing `index.json` files load without migration: missing field defaults to `Local`, preserving Phase 0/Phase 1 behavior.

### New methods

```rust
impl MailManager {
    /// Insert a header-only inbox entry from a walker-discovered MessageEntry.
    /// Folder is set to Inbox unconditionally (Phase 2 walker only descends Inbox).
    /// Dedup scope: returns Duplicate if the message_id is already present in
    /// inbox/trash/drafts (matches existing receive_message dedup window — a
    /// message previously moved to trash should not reappear in inbox).
    pub fn register_header_only(&self, entry: MessageEntry) -> Result<RegisterOutcome, MailError>;

    /// Verify bytes hash to cid, write blob, transition matching index entry
    /// from Pending → Local. No-op if entry already Local.
    pub fn mark_body_received(&self, cid_hex: &str, bytes: &[u8]) -> Result<(), MailError>;
}

pub enum RegisterOutcome {
    Inserted { cid: String },
    Duplicate,
}
```

### Modified behavior

- **`receive_message(bytes)`** (existing): if a `Pending` entry for the same `message_id` exists in inbox/trash/drafts, transitions it to `Local` (writes blob, clears pending flag) — preserving its current folder. The previous behavior of "duplicate message_id → no-op" continues to apply when the existing entry is already `Local`. This is the **race-safety property**: the live raw push and the walker can register the same entry in any order; net result is one `Local` entry, and a user's prior trash/drafts placement is not reset to Inbox by the live push.

- **`get_message(cid)`** (existing): unchanged signature. Returns the index entry; for `Pending` entries the returned `MailDetail` has empty body fields. Frontend uses `body_state` to decide whether to call `fetch_mail_body`.

- **`delete_message` / `move_message` / `mark_read`**: unchanged. Operate equally on `Pending` and `Local` entries. Deleting a `Pending` entry just removes the index row (no blob to delete).

### Storage cost

`Pending` entries cost only the index row (~150 bytes). A 10k-message backfill is ~1.5 MB of index, with bodies fetched on demand.

## `event_loop.rs` additions

### Subscriber filter

Today (line 797): `if key_expr.starts_with("harmony/mail/v1/") && !key_expr.ends_with("/root")`

Phase 2:

```rust
if key_expr.starts_with("harmony/mail/v1/") {
    if key_expr == own_root_key {
        // Spawn task to call mail_sync.handle_root_push(payload).
    } else if key_expr == own_mail_key {
        // Existing: route to MailManager::receive_message.
    }
    // Other keys silently ignored (defensive — current sub scope is exact).
}
```

`own_root_key = format!("harmony/mail/v1/{own_hex}/root")` and `own_mail_key = format!("harmony/mail/v1/{own_hex}")` are computed once at startup.

### Startup query

After Zenoh session is up and `MailSync` is constructed, a one-shot Zenoh `get` against `own_root_key` is fired in a spawned task. Reply (or empty / timeout) feeds `MailSync::handle_startup_query_reply`. 10-second timeout; on timeout, MailSync stays Idle and the manual refresh button (or the next live push) recovers.

### MailSync wiring

`MailSync::new` is called once during Tauri setup, alongside `MailManager`. It receives:
- A clone of the existing `fetch_tx: mpsc::Sender<FetchRequest>` (from lib.rs:1010)
- An `Arc<MailManager>` (Tauri-managed state today)
- The `own_addr_hex` string
- The Tauri `AppHandle` for emitting events

Stored alongside `MailManager` in Tauri-managed state so commands and event-loop dispatch can both call into it.

### New Tauri commands

```rust
#[tauri::command]
async fn refresh_mail(state: State<'_, AppState>) -> Result<(), String>;

#[tauri::command]
async fn fetch_mail_body(state: State<'_, AppState>, cid: String) -> Result<MailDetail, String>;
```

`get_mail` is updated to expose `body_state` in its `MailDetail` response.

### Untouched

- `fetch_via_zenoh()` and the `fetch_rx` channel.
- The `RuntimeAction` enum.
- All other event-loop arms.

## Frontend changes

### `mail-service.ts`

```ts
// New reactive state
syncState: 'idle' | 'syncing' | 'error' = $state('idle');
syncError: string | null = $state(null);

// New listener
listen<{state: string, error?: string}>('mail-sync-status', (e) => {
    this.syncState = e.payload.state as any;
    this.syncError = e.payload.error ?? null;
});

// New / wrapped methods
async refresh(): Promise<void> {
    await invoke('refresh_mail');
}

async getMessage(cid: string): Promise<MailDetail> {
    const detail = await invoke<MailDetail>('get_mail', { cid });
    if (detail.body_state === 'Pending') {
        return await invoke<MailDetail>('fetch_mail_body', { cid });
    }
    return detail;
}
```

### `MailInbox.svelte`

In the header (alongside existing folder tabs):

```svelte
<div class="sync-controls">
    {#if mailService.syncState === 'syncing'}
        <Spinner size="sm" title="Syncing mailbox…" />
    {:else if mailService.syncState === 'error'}
        <ErrorIcon
            title={mailService.syncError ?? 'Sync error'}
            onclick={() => alert(mailService.syncError)}
        />
    {/if}
    <button onclick={() => mailService.refresh()} title="Refresh">⟳</button>
</div>
```

Final visual treatment can be polished during implementation. Spec requirement: spinner during walk, error icon with tooltip on failure, refresh button always available.

### `MailReader.svelte`

```svelte
<script>
    let { cid } = $props();
    let detail = $state<MailDetail | null>(null);
    let loading = $state(true);
    let error = $state<string | null>(null);

    $effect(() => {
        loading = true;
        mailService.getMessage(cid)
            .then((d) => { detail = d; loading = false; })
            .catch((e) => { error = String(e); loading = false; });
    });
</script>

{#if loading}
    <Spinner /> Loading message…
{:else if error}
    <ErrorBanner message={error} />
{:else if detail}
    <!-- existing render of detail.subject / detail.body / etc. -->
{/if}
```

### Untouched

- `MailCompose.svelte` — pure send path.
- `App.svelte` routing/layout.
- Folder-switching behavior.

## Gateway change (harmony repo)

Single addition to `crates/harmony-mail/src/mailbox_manager.rs`: a Zenoh queryable handler that responds to queries on `harmony/mail/v1/{addr_hex}/root` with the current root CID for that address.

The publisher already maintains the latest root per address in its internal `latest: HashMap<[u8; 16], [u8; 32]>`. The queryable handler:

1. Extracts `addr_hex` from the query's key expression.
2. Looks up the address in the existing `latest` map.
3. Replies with the 32-byte CID if present, or empty payload if not.

Implementation lives alongside the existing publisher in `ZenohPublisher`. Cancellation token gates the queryable task the same way it gates the drain task. Tests verify (a) queryable returns the current root after a delivery, (b) returns empty for unknown address, (c) returns latest after multiple deliveries.

## Testing strategy

### Rust unit tests in `mail_sync.rs`

| Test | Setup | Assert |
|---|---|---|
| `walk_empty_root` | Mock returns valid MailRoot with empty Inbox | No entries registered, status → Idle |
| `walk_single_page` | Mock serves Root + Folder + 1 Page with 3 entries | 3 Pending entries, 3 mail-received events, status → Idle |
| `walk_multi_page` | Mock serves Root + Folder + 5 Pages | All entries registered, page fetches issued in parallel, status → Idle |
| `dedup_against_local` | Pre-populate inbox with one message_id; mock serves a Page containing it | `register_header_only` returns Duplicate, no overwrite of existing Local entry |
| `root_fetch_404` | Mock returns NotFound for root CID | Status → Error, no entries |
| `folder_fetch_404` | Root OK; Folder NotFound | Status → Error, no entries (strict) |
| `page_fetch_404` | Root + Folder OK; one of three pages NotFound | Other two pages registered; status → Error with skip summary |
| `entry_parse_error` | Page bytes contain malformed entry | Other entries in same page registered; that one skipped; status → Error |
| `pending_root_during_walk` | Walk in progress; second push arrives | Walk completes, second walk runs immediately after, no thrashing |
| `body_fetch_dedup` | Two concurrent `fetch_body(cid)` calls | One Zenoh fetch issued, both callers receive same bytes |
| `body_fetch_invalid_hash` | Mock returns bytes that don't hash to requested CID | Returns error, entry stays Pending |

### Rust unit tests in `mail.rs`

| Test | Assert |
|---|---|
| `register_header_only_new` | Inserts Pending; `list_folder` includes it |
| `register_header_only_dedup` | Returns Duplicate when message_id already exists |
| `mark_body_received_pending_to_local` | Pending → Local; blob written |
| `mark_body_received_already_local` | No-op, returns Ok |
| `mark_body_received_hash_mismatch` | Returns error, entry stays Pending |
| `index_migration_old_format` | Loads index without `body_state`; defaults all Local |
| `receive_message_promotes_pending` | Pending entry with same message_id → Local with body |

### Gateway-side tests in `harmony/crates/harmony-mail`

| Test | Assert |
|---|---|
| `root_queryable_returns_current_root` | After SMTP delivery, Zenoh `get` returns same 32 bytes that were `put` |
| `root_queryable_empty_for_unknown_addr` | Query for address with no mail returns empty reply |
| `root_queryable_after_multiple_deliveries` | Returns latest root only |

### Integration test (in `harmony-client`)

One test using an in-process Zenoh session with both MailSync (client side) and a stub gateway publisher serving a hand-crafted Merkle tree:

```
1. Bring up Zenoh session.
2. Stub gateway: publish root CID + register CAS queryable serving 1 root + 1 folder + 1 page + 3 message bodies.
3. Bring up MailSync; wait for startup query.
4. Assert: inbox has 3 Pending entries within 1s.
5. Call MailSync::fetch_body for one of them.
6. Assert: entry transitions to Local; blob on disk; HarmonyMessage parseable.
```

### Frontend tests (Vitest, Svelte component tests)

| Test | Assert |
|---|---|
| `MailInbox shows spinner during sync` | When `syncState='syncing'`, Spinner rendered |
| `MailInbox shows error icon on sync error` | When `syncState='error'`, ErrorIcon visible with tooltip |
| `Refresh button calls invoke('refresh_mail')` | Click triggers Tauri command |
| `MailReader fetches body for Pending entry` | `getMessage` resolves with body after async fetch |
| `MailReader uses cached body for Local entry` | No `fetch_mail_body` call when entry is Local |

### Out of test scope

- Real Zenoh transport behavior (covered by Zenoh's own tests + Phase 1 PRs).
- harmony-mailbox parsers (already tested in their crate).
- `fetch_via_zenoh` (already tested in event_loop).
- Multi-device sync conflicts — explicitly out of scope per Q2 trust model.

## Risks & open questions

### Resolved during brainstorm

- ~~How does the client fetch CAS blobs?~~ — Existing `harmony/content/{prefix}/{cid_hex}` Zenoh queryable served by Tier 2 storage; client already has `fetch_via_zenoh`.
- ~~How does the client know the current root on cold start?~~ — New gateway queryable on the same `/root` key.
- ~~What happens when a body fetch races with the live raw publish?~~ — `receive_message` promotes any matching Pending entry to Local, idempotent in either order.

### Accepted limitations (documented, not addressed)

- **Multi-device divergence.** Per Q2, local state is authoritative. Reading a message on device A doesn't mark it read on device B. Fix in a future bidirectional sync phase.
- **Sync drops if Zenoh dropped the publish AND user never clicks refresh.** Known. Acceptable because (a) Zenoh's local-network reliability is high, (b) every new inbound message triggers a fresh root push.
- **Body fetch latency** dominates user-perceived "open message" time. Tracked as ZEB-118 (background prefetch) for a follow-up optimization.

### Will discover during implementation

- **Exact concurrency cap** for parallel page fetches (proposed 8). May tune up or down based on integration test latency measurements.
- **Spinner visual treatment** — coordinate with existing UI components rather than introducing new patterns.
- **Error tooltip detail level** — list every skipped CID, or summary count? Decide when first skip-error scenario is wired up.

## Implementation order (sketch — full plan to follow)

1. Gateway: add Zenoh queryable on `/root` (harmony repo, small PR).
2. Client: `BodyState` enum + `register_header_only` + `mark_body_received` in `mail.rs` (with tests).
3. Client: `mail_sync.rs` skeleton + walker state machine + unit tests with mocked fetch.
4. Client: `event_loop.rs` filter flip + startup query + MailSync wiring.
5. Client: Tauri commands `refresh_mail` and `fetch_mail_body`; update `get_mail` to expose `body_state`.
6. Client: `mail-service.ts` extensions; `MailInbox.svelte` indicator + refresh button.
7. Client: `MailReader.svelte` async body load.
8. Integration test (cross-component, in-process Zenoh).
9. Manual end-to-end QA against a real gateway.

The detailed step-by-step plan will be drafted via the writing-plans skill once this design is approved.
