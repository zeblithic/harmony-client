# ZEB-723 — Grantee "Shared with me" browse + read surface (design)

**Ticket:** [ZEB-723](https://linear.app/zeblith/issue/ZEB-723) — ZEB-674 follow-up.
**Depends on:** ZEB-674 (PR #512, merged `649fbb44`) — the grantee RECEIVE + decrypt
backend (`received_file_grants`, `ingest_grant_push`, decrypt-on-read) is already shipped.

## Goal

Give a grantee a first-class surface to **see** the encrypted files others have
shared with them and to **download** (decrypt-to-disk) those files. ZEB-674
shipped the owner side end-to-end (create encrypted → share → owner reads) and
the *entire* grantee backend, but left the grantee no UI. This closes that loop.

## What already exists (do NOT rebuild)

- `OwnerState.received_file_grants: BTreeMap<[u8;32], ReceivedFileGrant>` — one
  entry per file shared *to* this owner, keyed by the encrypted root CID bytes.
  `ReceivedFileGrant` carries `{granter_owner, cid, file_name, file_size, mime,
  sealed_dek, received_at}` and replicates across the owner's own devices (Flow A).
- `file_sharing::ingest_grant_push` — the sweeper applies an inbound `grant_push`
  deposit into `received_file_grants` (re-sealing the DEK under the grantee's own
  shared KeyTree so *any* bound device can open it). Call site:
  `dm_inbox_ingest.rs:1282`.
- **The read path is complete.** `fetch_content` / `export_content` fetch the
  ciphertext from the network via the event-loop `FetchRequest` (the granter
  allowlisted the CID's subtree for member serve), then
  `maybe_decrypt_personal_file` → `decrypt_personal_file_if_held` consults
  `received_file_grants` for the sealed DEK and returns plaintext
  (`lib.rs:19798-19822`). A grantee who knows a CID + filename can already
  download-and-decrypt today; the only missing piece is *surfacing* the CID + name.

So the "small fetch a granted CID" the ticket anticipated is **already covered**.
The remaining backend work is one read-only list IPC + one arrival event.

## Architecture

The Files view (`appMode === 'files'`, rendered by `FileBrowser`) already has a
**section tab bar** — `ContentSection = 'private' | 'published'`, rendered in
`BrowserToolbar` as `Private | Published`. "Shared with me" slots in as a **third
section**, mirroring the existing pattern. When the section is active, the browser
renders a dedicated, simpler list (received files are not `ContentItem`s — no
sidecarId, no tier/pin/burn/folder actions — so reusing `FileGrid` would be a
type and affordance mismatch).

### Data flow

```
grant deposit arrives → sweeper → ingest_grant_push (received_file_grants.insert)
        └─► emit "shared-with-me-updated"  ─────────────────────────────┐
                                                                        ▼
App.svelte: on event → refresh + bump unread badge on the tab      badge/dot
        │
        │ user opens "Shared with me" section
        ▼
FileManagerService.listReceivedGrants() ──► IPC list_received_grants()
        │                                         └─ projects received_file_grants
        ▼                                            → Vec<ReceivedGrantDto>
SharedWithMeList.svelte renders rows (name, granter, size, received)
        │  Download action
        ▼
export_content(cid, fileName)  ──► fetch ciphertext + decrypt (already works)
```

## Backend changes (small)

### 1. `list_received_grants()` IPC — read-only projection

New Tauri command + `list_received_grants_impl`, a direct mirror of the existing
`list_grants_impl` (`lib.rs:20240`) but over `received_file_grants`:

```rust
pub(crate) async fn list_received_grants_impl(
    state: &Mutex<NodeState>,
) -> Result<Vec<ReceivedGrantDto>, String>
```

- Snapshot `crdt_state` under the std lock; drop it; take the async crdt lock;
  call the pure projection helper.
- **Granter display name** is friend-resolved exactly as `list_grants_inner`
  resolves grantee names: `state.friend_graph.friends.get(&granter_owner)
  .and_then(|f| f.display.clone())` → `display_name: Option<String>` (`None` when
  the granter is not a currently-known friend). The frontend falls back to the
  hex address when `display_name` is `None` — same convention as `FileGrantDto`.
- `Err("no owner loaded")` when pre-mint (same as `list_grants_impl`). The
  frontend treats *any* error as "unresolved", never as proven-empty.
- Rows sorted by `received_at` descending (newest first).

New DTO (serde camelCase for the IPC boundary — matches `FileGrantDto`), plus a
pure `list_received_grants_inner(&OwnerState) -> Vec<ReceivedGrantDto>` helper
next to `list_grants_inner` (unit-testable without a live node; the friend graph
lives inside `OwnerState`, so no separate arg):

```rust
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceivedGrantDto {
    pub cid: String,                   // hex encrypted root CID
    pub granter_address: String,       // hex OwnerAddr
    pub display_name: Option<String>,  // friend display; None → frontend shows hex
    pub file_name: String,
    pub file_size: u64,
    pub mime: String,
    pub received_at: u64,              // wall ms
}
```

### 2. Arrival event

At the `ingest_grant_push` call site (`dm_inbox_ingest.rs:1282`), after a
successful ingest, emit `shared-with-me-updated` (payload: `{ cid }`) through the
same node-event-sink path the DM sweeper already uses. One-line addition next to
the existing post-ingest handling. The event is a refresh trigger; the frontend
re-queries `list_received_grants` rather than trusting the payload to carry state.

## Frontend changes

### 3. `ContentSection` + tab

- `types.ts`: `ContentSection = 'private' | 'published' | 'sharedWithMe'`.
- `BrowserToolbar.svelte`: third `section-btn` "Shared with me", same markup as
  Private/Published.
- `FileBrowser.svelte`: `{#if section === 'sharedWithMe'}` renders
  `SharedWithMeList`; the private-only toolbar actions (new folder, upload,
  cleanup) already gate on `section === 'private'`, so they stay hidden.

### 4. `FileManagerService.listReceivedGrants()`

```ts
async listReceivedGrants(): Promise<ReceivedFile[]>  // maps DTO → view type
```

Invokes `list_received_grants`; maps `ReceivedGrantDto` → a `ReceivedFile`
(`{ cid, granterAddress, granterDisplay, fileName, fileSize, mime, receivedAt }`),
where `granterDisplay = displayName ?? granterAddress` (mirrors how the owner-side
ShareList falls back to the address when `displayName` is null). Errors propagate
(caller distinguishes unresolved from empty).

### 5. `SharedWithMeList.svelte`

- Prop `files: ReceivedFile[] | null` — the **honesty contract**, identical to
  `ShareList.grants`:
  - `null` → unresolved: a neutral loading/placeholder state (NOT "empty").
  - `[]` → proven-empty: "Nothing has been shared with you yet."
  - populated → one row per file: filename, granter display, size, relative
    received-time, and a **Download** button.
- Download → `onDownload(file)` → `fileManagerService`'s existing export path
  (`export_content(cid, fileName)`), which already fetches + decrypts. Reuse the
  same error normalization the owner-side export uses (`e instanceof Error`).
- A load *failure* in the caller leaves `files = null` (unresolved) — never `[]`.
  Mirrors ZEB-674 Gap G-3: a catch must not fabricate proven-empty.

### 6. App.svelte wiring

- State: `receivedFiles: ReceivedFile[] | null = null`, resolved when the
  `sharedWithMe` section is opened (and re-resolved on the arrival event).
- Load guard: mirror the ZEB-674 `refreshFileGrants` race guard — if the user
  leaves the section before the async resolves, don't commit stale results.
- **Unread badge:** a dedicated localStorage key `sharedWithMeLastSeenMs`
  (settings are in-memory only, so this is a standalone persisted value that
  survives restart). Badge count = number of received grants with
  `received_at > lastSeenMs`. Opening the section sets `lastSeenMs = now` (via a
  small helper) and clears the badge. On the `shared-with-me-updated` event:
  refresh the list and recompute the badge.

## Honesty model

Tri-valued fetch state everywhere (`null` unresolved / `[]` proven-empty /
populated). Rendering "nothing shared with you" when the list actually failed to
load would be an active misstatement about another person's action toward the
user — worse than an empty owner-side list — so the failure path stays `null`.
This is the same invariant enforced for the ZEB-674 ShareList.

## Testing

**Backend (Rust, inline + integration):**
- `list_received_grants_inner`: unit test — projects entries, sorts by
  `received_at` desc, resolves a friend granter to its display name, falls back
  to hex for a non-friend, returns `[]` for an empty map.
- `list_received_grants_impl`: `Err("no owner loaded")` pre-mint.
- Arrival event: assert `shared-with-me-updated` is emitted after a successful
  `ingest_grant_push` at the sweeper (extend an existing dm-inbox ingest test if
  one covers the grant_push arm; else a focused integration test).

**Frontend (vitest):**
- `SharedWithMeList.test.ts`: `null` → placeholder (no "empty" copy); `[]` →
  empty copy; populated → rows + Download wired; Download click calls the handler
  with the right cid/fileName.
- `FileManagerService.listReceivedGrants`: maps DTO fields; propagates an IPC
  error (does not swallow to `[]`).
- Badge logic: count of received newer than last-seen; opening clears it.

**Gates:** full CI-parity sweep — `cargo fmt` · `clippy --locked --all-targets
--features test-fixtures -D warnings` · `cargo nextest --locked --workspace
--all-targets --features test-fixtures` · `tsc --noEmit` · `vitest`.

## Scope / non-goals (deferred, per ZEB-674 deferral list)

- **In-app preview/open** — MVP is download-to-disk only (mirrors owner export).
- **Toast on arrival** — MVP is the tab badge only.
- **Rotate-on-revoke / true crypto withdrawal** — unchanged fundamental limit.
- **New-device automatic re-seal, PQ-hybrid seal, folder-ingest encryption,
  `received_file_grants` `(granter,cid)` keying** — separate ZEB-674 follow-ups.
- **Removing / hiding a received grant from the grantee's own list** — no
  grantee-side dismiss in MVP (the list reflects what was granted; a dismiss/hide
  affordance is a future nicety, not required to close the loop).

## Client-only

No `harmony` crate change → no rev-bump → single PR.
