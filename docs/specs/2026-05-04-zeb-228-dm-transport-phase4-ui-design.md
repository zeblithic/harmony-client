# ZEB-228 — DM Transport Phase 4: NavService + UI design

> Companion to [`docs/specs/2026-05-02-zeb-216-sub-b-dm-transport-design.md`](2026-05-02-zeb-216-sub-b-dm-transport-design.md). The umbrella spec covers all four phases at architecture level; this doc fills in the implementation-detail decisions for Phase 4 (final phase of ZEB-216).

## Goal

Wire the now-shipped DM transport stack (Phases 1-3b) into the existing harmony-client UI so users can create DMs, send/receive messages, and manage stuck/expired outbox entries — all through the existing chat-shaped `TextFeed` + `ComposeBar` surface.

After this lands, ZEB-216's umbrella acceptance criteria are met and the parent ticket closes.

## Scope

In-scope (Phase 4 deliverables per ZEB-228):

- DmCreateDialog component for picking 1-15 recipients (DM = 1 picked / 2 members; GroupDm = 2-15 picked / 3-16 members) with at-15-recipients inline hint and at-16-recipients hard block
- NavService DM/GroupDm rendering — new DM Spaces emit NavNodes at top-level (`parentId=null`); user drags into folders
- MessageService DM routing — subscribe to `dm-received`/`dm-delivered`; route into the existing channel-keyed message buffer using SpaceId as the channel key
- Cold-start scrollback — frontend reads InboxEntries via new `read_dm_thread` IPC on DM-channel switch
- Self-outbound persistence — `send_dm` writes a self-InboxEntry alongside the OutboxEntry so self-history persists beyond OutboxEntry's lifetime
- App.svelte send-path branch — `onSend` for DM channels routes through `send_dm` IPC; channels stay on existing publish path
- Inline manual-delete on stuck/expired messages via the existing ConfirmDialog component
- Vitest UI tests covering golden path, at-17 cap, manual delete, IPC event handling
- `dm-received` IPC payload extended to include `body` + `mimeType` (was promised in umbrella spec at line 773 but Phase 3b's emit only carried the InboxEntry pointer)

Out of scope (filed as follow-ups or deferred per umbrella spec):

- DmInvite decline UX (modal + accept/decline IPC) — [ZEB-236](https://linear.app/zeblith/issue/ZEB-236), separate sub-issue
- Manual two-device LAN smoke testing — [ZEB-239](https://linear.app/zeblith/issue/ZEB-239), final shipping verification
- Communities / channels-within-DMs — Sub-C, separate top-level work
- Reactions / threading on DMs — channel reactions ship via ZEB-32; DM reactions deferred
- Voice/video in DMs — separate transport design

## Architecture

The frontend is mostly already chat-shaped — `TextFeed.svelte` (241 LOC) accepts `channelType: 'channel' | 'dm' | 'group-chat'`, and `ComposeBar.svelte` (144 LOC) is general-purpose. NavNode types already include `'dm'` and `'group-chat'`. NavService mock data already places DMs at top-level and inside user-organized folders.

Phase 4 is therefore predominantly a wiring exercise: route DM-shaped data through existing components, extend a few backend IPC surfaces to support cold-start scrollback and self-history persistence, and add one new component (DmCreateDialog).

```text
                    ┌──────────────────────────────────────────────┐
                    │  App.svelte                                  │
                    │  ─────────                                   │
                    │  activeChannel: SpaceId                      │
                    │  activeChannelType: channel | dm | group-chat│
                    └────────┬─────────────────────────────────────┘
                             │ onSend(text)
                             │  ├─ channelType=channel → publish (existing)
                             │  └─ channelType=dm/group-chat → send_dm IPC (NEW branch)
                             ▼
                    ┌─────────────────────┐
                    │  ComposeBar.svelte  │ (existing, no changes)
                    └─────────────────────┘
                             │
                    ┌────────┴────────┐
                    │  TextFeed       │ (existing, no changes — already chat-shaped)
                    │   ↑ messages    │
                    └────────┬────────┘
                             │
        ┌────────────────────┼─────────────────────────┐
        │                    │                         │
        ▼                    ▼                         ▼
┌────────────────┐  ┌────────────────────┐  ┌────────────────────────┐
│ MessageService │  │ NavService         │  │ DmCreateDialog (NEW)   │
│ ────────────── │  │ ─────────          │  │ ────────────────       │
│ Per-channel    │  │ NavNodes by type   │  │ Member picker          │
│ message buffer │  │ {channel, dm,      │  │ at-16 inline hint      │
│ + IPC subs:    │  │  group-chat,       │  │ at-17 hard block       │
│  - dm-received │  │  folder}           │  │ → calls add_space IPC  │
│  - dm-delivered│  │ + IPC subs:        │  └────────────────────────┘
│  - dm-expired  │  │  - nav-updated     │
└────────┬───────┘  └────────────────────┘
         │
         ▼ on DM channel switch
┌────────────────────────────────┐
│ read_dm_thread IPC (NEW)       │
│ ─────────────────────────      │
│ Returns InboxEntry list        │
│ (incl. self-sent) + decrypted  │
│ bodies, paginated by HLC.      │
└────────────────────────────────┘
```

## Backend changes

### 1. Extend `dm-received` IPC payload to include body + mime_type

The umbrella spec (`docs/specs/2026-05-02-zeb-216-sub-b-dm-transport-design.md:773`) defined the payload as `{ space_id, message_cid, from, sent_at, body, mime_type }` — but the merged Phase 3b emit at `src-tauri/src/event_loop.rs:1336-1344` only carries `{ spaceId, messageCid, from, receivedAt }`. The body is decrypted in `handle_cidnotify` (Step 11) and currently dropped after `apply_inbox`.

**Change:** thread the decrypted `MessagePayload` through `DrainOutcome.newly_received` so the event_loop emit can include `body` (hex-encoded) and `mimeType` (string) in the IPC payload. Also include `sentAt: number` (unix-ms from the decrypted payload's `sent_at.wall_ms`) — the receiver's `received_at` is local-clock, but the UI cares about the sender's send time.

```rust
// owner_state_types.rs (or dm_outbox.rs)
pub struct ReceivedMessage {
    pub inbox_entry: InboxEntry,
    pub body: Vec<u8>,
    pub mime_type: String,
    pub sent_at: Hlc, // from decrypted MessagePayload
}

// dm_outbox.rs DrainOutcome
pub newly_received: Vec<ReceivedMessage>, // was Vec<InboxEntry>

// event_loop.rs emit
serde_json::json!({
    "spaceId": hex::encode(rm.inbox_entry.space_id.0),
    "messageCid": hex::encode(rm.inbox_entry.message_cid.to_bytes()),
    "from": hex::encode(rm.inbox_entry.from.0),
    "receivedAt": rm.inbox_entry.received_at.wall_ms,
    "sentAt": rm.sent_at.wall_ms,
    "body": hex::encode(&rm.body),
    "mimeType": rm.mime_type,
})
```

Migration safety: this is an additive payload change; existing frontend listeners ignore unknown fields. No version-bump needed.

### 2. Self-InboxEntry on `send_dm`

Currently `send_dm` (`src-tauri/src/dm_outbox.rs:376`) creates an OutboxEntry but does not write a self-InboxEntry. Self-outbound history is therefore only visible while the OutboxEntry is alive (pre-delivery, in-flight, or expired-but-not-yet-cleaned-up). After delivery sweeps the OutboxEntry, the user's own messages would vanish from the UI.

**Change:** in `send_dm`, after computing `message_cid` and writing the encrypted blob to CAS, call `state.apply_inbox(InboxEntry { space_id, message_cid, from: self_owner, received_at: sent_at.clone() })`. Use `sent_at` (the message's HLC) as `received_at` so the timeline ordering is consistent. The `apply_inbox` returns `ApplyOutcome::Inserted` for first-write, `Merged{old_id: None}` if a concurrent same-CID write happened (cross-device dedup) — both are fine, no error to propagate.

This means InboxEntry's semantics widen from "received from someone else" to "exists in this Space's history (sender or recipient)". Update the doc comment on `InboxEntry` accordingly.

Side effect: cross-device convergence already handles this — a user's other paired device receives the same DmCidNotify (Phase 3b multi-device fan-out) and writes its own InboxEntry. The self-InboxEntry on the sending device matches what a paired device would write on receipt, so the InboxEntry table converges naturally without special-casing.

### 3. New `read_dm_thread` IPC

For cold-start scrollback (user opens app → switches to a DM → frontend needs message history), add an IPC that reads InboxEntries for a given Space and returns decrypted bodies.

```rust
// lib.rs
#[derive(Serialize)]
pub struct DmThreadMessage {
    pub message_cid: String,    // hex
    pub from: String,           // hex OwnerAddr
    pub sent_at: u64,           // wall_ms from MessagePayload.sent_at
    pub received_at: u64,       // wall_ms from InboxEntry.received_at
    pub body: String,           // hex-encoded bytes
    pub mime_type: String,
    pub is_self_outbound: bool, // from == self_owner
}

#[tauri::command]
async fn read_dm_thread(
    app: AppHandle,
    space_id: String, // hex SpaceId
    limit: usize,     // pagination — first call typically passes a UI-page limit (~50)
    before_hlc: Option<u64>, // pagination cursor — wall_ms; None = newest
) -> Result<Vec<DmThreadMessage>, String>;
```

Implementation:
1. Resolve Space, get `content_key` + `prior_content_keys`.
2. Iterate `state.inbox.entries_for_space(space_id)` (need to add a helper if it doesn't exist) sorted by `received_at` descending.
3. Apply `before_hlc` cursor + `limit` to slice.
4. For each entry: `cas.get(&message_cid)` → `decrypt_dm_message` (with prior-keys fallback) → extract body + mime_type from MessagePayload.
5. Return reverse-chronological list. Frontend reverses for display.

CAS fetches go through the same `cas_op` channel + 500ms timeout pattern as `handle_cidnotify`; locally-cached blobs return synchronously. For Phase 4 scope, accept that paginating through 1000+ messages is slow (sequential CAS reads) — performance optimization is a follow-up if it matters.

Per ZEB-227 review feedback rule (locks-across-await): release `OwnerState` lock before each CAS fetch, re-acquire only for state lookups. Mirrors the pending [ZEB-241](https://linear.app/zeblith/issue/ZEB-241) refactor for `handle_cidnotify`.

### 4. Extend `add_space` for DM/GroupDm kinds

Currently `add_space` (`src-tauri/src/lib.rs`) handles Folder/Channel/Community kinds. Phase 4 adds DM/GroupDm handling:

1. Validate `kind ∈ {Dm, GroupDm}` and `members.len() ∈ [2, 16]` (creator + 1 to 15 recipients; at-17 blocked at the frontend).
2. Generate a fresh `content_key` (32 random bytes via `OsRng`, wrap in `Zeroizing`).
3. Build the Space CRDT entry: `{kind, members, content_key: Some(ck), prior_content_keys: vec![], transport: Some(Reticulum{participants: vec![]}), ...}`.
4. Apply locally via `state.apply_space_with_canonicalization(space)`.
5. For each non-self member, look up their devices via `OwnerDeviceCache` (best-effort — bootstrap-incomplete cases are fine; the invite carries the inviter's identity_pub so the receiver can establish trust).
6. Build `DmInviteSigned` per-device and dispatch via the same `unicast_send_tx` channel `RuntimeUnicastTransport` uses.

Return the new SpaceId.

Error cases:
- `members.len() < 2` → `Err("DM requires at least 2 members (creator + 1)")`.
- `members.len() > 16` → `Err("DM/GroupDm capped at 16 members; use a community for larger groups")` (defense-in-depth — frontend should already block).
- `kind == Dm && members.len() != 2` → `Err("Dm kind requires exactly 2 members; use GroupDm for 3+")`.
- Self not in members → automatically inserted (caller may pass `members: [Bob]` for a 1-on-1; backend adds self).

### 5. New `delete_outbox_entry` IPC

```rust
#[tauri::command]
async fn delete_outbox_entry(
    app: AppHandle,
    message_id: String, // hex OutboxEntryId
) -> Result<(), String>;
```

Implementation:
1. Look up OutboxEntry by id.
2. Remove from `state.outbox`.
3. Optionally also remove the corresponding self-InboxEntry (per `(space_id, message_cid)` lookup). User intent on manual delete is "make this message go away," so removing both is the expected UX. Mark with a doc comment — if we later want "withdraw delivery but keep my own history" that's a separate follow-up.
4. Persist via the existing save_crdt path.
5. Emit `dm-deleted` IPC event with `{spaceId, messageCid}` so the frontend updates its local cache.

This handles both stuck (still in retry backoff) and expired (past 30-day threshold) entries — the user-facing flow is the same.

## Frontend changes

### 1. New `DmCreateDialog.svelte`

Single-screen dialog (option A from the visual brainstorm). Member picker with multi-select from contacts/profiles, search box, selected-count counter "X of 15 recipients" (15 + self = 16, the GroupDm cap), at-15 disable on "Add more" with inline hint "Group DMs cap at 16 members (you + 15). Communities (coming soon) work better for larger groups." Plus a "← Cancel" button.

Calls `add_space(kind: selectedRecipients.length === 1 ? 'dm' : 'group-dm', name: <auto-generated from member list>, members: selectedRecipients)` IPC. The `selectedRecipients` count is the user's picked recipients (excluding self); the backend adds self to the Space's `members` field, so a 1-recipient pick yields a 2-member Space (Dm kind), and a 15-recipient pick yields a 16-member Space (GroupDm kind, at the cap). On success, switches the active channel to the new SpaceId. On error, displays inline.

Auto-generated names follow the existing pattern from `handle_invite`'s receiver-side name: `"DM with {hex(members[0])[0:8]}…"` for 1-on-1, `"DM: {member1Name}, {member2Name}, …"` for groups (truncated to fit).

### 2. `nav-service.ts` extension

Add a `nav-updated` IPC listener that:
1. Receives a NavUpdate payload `{spaceId, kind, name, members, parentId, action: 'added' | 'removed' | 'modified'}`.
2. For DM/GroupDm Spaces with `action='added'`: construct a NavNode `{type: kind === 'dm' ? 'dm' : 'group-chat', name, peer: members[0] (for 1-on-1), parentId: null}` and insert into `nodes`.
3. For `action='modified'`: update the existing NavNode in place.
4. For `action='removed'`: drop from `nodes` (deferred — Phase 4 doesn't ship Space deletion; placeholder branch).

The `parentId: null` default puts new DMs at top-level. The user can drag them into folders via existing nav-tree drag-drop (assumed already wired; verify and file a follow-up if not).

### 3. `message-service.ts` extension

Subscribe to:
- `dm-received` → push a Message into the per-channel buffer keyed by SpaceId. Decode `body` from hex, derive `senderName` from `from` via NavService's profile lookup, set `priority: 'normal'`.
- `dm-delivered` → mark the corresponding self-Message as delivered. The existing `Message` type doesn't have a delivery-state field; add `deliveryState: 'sending' | 'delivered' | 'expired' | 'failed'` (with `'sending'` as the default for self-Messages, undefined / not-present for received Messages).
- `dm-expired` → transition the corresponding self-Message to `deliveryState: 'expired'`. Already emitted by `event_loop.rs` from Phase 3b's `outcome.newly_expired` — no backend change needed.
- `dm-deleted` (NEW) → remove the corresponding Message from the buffer.

Add a `loadDmThread(spaceId)` method that calls the new `read_dm_thread` IPC on first DM-channel switch + caches results in the per-channel buffer. Pagination: track `oldestLoadedHlc` per channel; on scroll-to-top, fetch next page via `read_dm_thread(spaceId, 50, before_hlc=oldestLoadedHlc)`.

### 4. `App.svelte` send-path branch

Around line 654 (`switchMode`/`activeChannel` handling), modify the existing `onSend` callback path:

```ts
async function handleSend(text: string, priority: MessagePriority) {
  if (activeChannelType === 'dm' || activeChannelType === 'group-chat') {
    await tauriAdapter.invoke('send_dm', {
      spaceId: activeChannel,
      content: Array.from(new TextEncoder().encode(text)), // bytes for Vec<u8>
      mimeType: 'text/plain',
    });
    // dm-received fires for our self-InboxEntry write → MessageService picks it up
    // (or directly push optimistically for instant UI feedback; pick one)
  } else {
    // existing channel publish path unchanged
    ...
  }
}
```

**Optimistic UI question:** for DMs, should the message appear in the timeline immediately (before `send_dm` returns), or wait for the IPC roundtrip + the self-InboxEntry write event? Discord/iMessage convention is optimistic-with-fallback. Phase 4 default: optimistic — push a placeholder Message with `id: messageId from send_dm response, deliveryState: 'sending'`, then when `dm-delivered` arrives transition to `'delivered'`, on `dm-expired` transition to `'expired'`. If `send_dm` itself errors, mark `'failed'` immediately.

### 5. Inline manual-delete on stuck/expired messages

In `TextMessage.svelte` (or wherever individual message rendering lives), add an inline `ⓍDelete` button visible only when:
- `deliveryState === 'expired'` (past 30-day threshold), OR
- `deliveryState === 'sending'` AND `now - sentAt > 60_000` (stuck longer than 1 minute)

Click flow:
1. Open `ConfirmDialog.svelte` with `"Delete this message? It hasn't been delivered yet. Recipients who haven't received it won't see it."` (for stuck) or `"Delete this expired message? It's been undeliverable for 30 days."` (for expired).
2. On confirm: call `delete_outbox_entry(messageId)` IPC.
3. On success: MessageService's `dm-deleted` listener removes the Message from the buffer; UI re-renders.

Use the existing ConfirmDialog component (per user's DRY preference) unless the existing component proves too inflexible — fall back to a lightweight `confirm()`-style modal only if needed.

### 6. App.svelte wiring

- Wire `DmCreateDialog` onto a "+ New DM" button at the bottom of the nav sidebar (small "+" icon, tooltip "New direct message"). No existing "+New Channel" / "+New Folder" affordance ships today; the bottom-of-sidebar location matches Slack/Discord conventions and stays out of the way of the existing nav tree. If we add other "+New X" affordances later, they can group into a single popover; Phase 4 just needs the DM entry point.
- Subscribe `MessageService` and `NavService` to the new IPC events on connect.
- Call `messageService.loadDmThread(spaceId)` when `switchChannel` activates a DM/GroupDm channel for the first time in this session.

## Flow walkthroughs

### Flow A: Create a DM

1. User clicks "+ New DM" → `DmCreateDialog` opens.
2. User searches/selects Bob → counter shows "1 of 16."
3. User clicks "Start DM" → frontend invokes `add_space(kind: 'dm', name: 'DM with Bob', members: [Alice, Bob])`.
4. Backend generates content_key, builds Space CRDT, applies locally, fans out DmInviteSigned to Bob's known devices.
5. Backend returns SpaceId.
6. Frontend switches active channel to the new SpaceId → empty `TextFeed` rendered.
7. `nav-updated` IPC fires (CRDT change emitted same as any other Space change) → NavService inserts the new NavNode at top-level.

### Flow B: Send a message

1. User types in `ComposeBar`, presses Enter.
2. App.svelte's onSend detects `activeChannelType ∈ {dm, group-chat}` → optimistically pushes Message with `deliveryState: 'sending'` to MessageService → invokes `send_dm` IPC.
3. Backend encrypts, writes to CAS, creates OutboxEntry, **writes self-InboxEntry**, returns MessageId.
4. Backend's drain loop sends DmCidNotify to recipient(s) on next tick.
5. Recipient acks; `dm-delivered` IPC fires → MessageService transitions Message to `'delivered'`.

### Flow C: Receive a message

1. `dm-received` IPC fires (after `handle_cidnotify` + decrypt).
2. MessageService listener decodes `body` from hex, looks up sender profile via NavService, builds Message.
3. If active channel matches `spaceId` → push to TextFeed; auto-scroll if user is near bottom.
4. If active channel doesn't match → increment NavNode unread count.

### Flow D: Cold-start scrollback

1. App starts, IPC connects, NavService loads existing Spaces from initial state sync.
2. User clicks an existing DM NavNode → `switchChannel(spaceId)` fires.
3. MessageService checks per-channel buffer; if empty for this SpaceId, calls `loadDmThread(spaceId)`.
4. Backend reads InboxEntries (self + received), decrypts each, returns reverse-chrono list of 50.
5. Frontend reverses, populates buffer, TextFeed renders.
6. User scrolls to top → MessageService fetches next page via `before_hlc` cursor.

### Flow E: Manual delete stuck message

1. User sees a "sending..." indicator that's been stuck >60s on their own message.
2. User clicks the inline ⓧ → `ConfirmDialog` opens with the stuck-message copy.
3. User confirms → frontend invokes `delete_outbox_entry(messageId)`.
4. Backend removes OutboxEntry + self-InboxEntry, persists, emits `dm-deleted`.
5. MessageService removes the Message from the buffer; TextFeed re-renders without it.

## Tests

### Vitest (frontend)

- `DmCreateDialog.test.ts`:
  - Selecting 1 recipient, clicking Start DM, calls `add_space` with `kind='dm'` and `members.length===1`.
  - Selecting 2-14 recipients, calling Start DM, calls `add_space` with `kind='group-dm'`.
  - Selecting 15 recipients (the at-cap) shows the inline cap hint; "Add more" disables.
  - Attempting to select 16th recipient is blocked (no-op + tooltip).
  - Cancel button closes dialog without IPC call.
- `nav-service.test.ts` extension:
  - `nav-updated` for `kind='dm'` adds a NavNode at top-level.
  - `nav-updated` for `kind='group-dm'` adds a NavNode with `type: 'group-chat'`.
  - `nav-updated` `action='modified'` updates an existing NavNode's name/members.
- `message-service.test.ts` extension:
  - `dm-received` IPC pushes a Message into the per-spaceId buffer with body decoded.
  - `dm-delivered` transitions a self-Message to `'delivered'`.
  - `dm-deleted` removes a Message from the buffer.
  - `loadDmThread` calls `read_dm_thread` IPC and populates the buffer reverse-chronologically.
- Integration-style: simulate full Flow B (send) and Flow C (receive) via mock TauriAdapter, assert UI reflects each state transition.

### Cargo (backend)

- `lib.rs` integration test: `read_dm_thread` returns InboxEntries with decrypted bodies for a seeded Space.
- `lib.rs` integration test: `add_space` with `kind='dm'` generates content_key, applies Space, dispatches DmInvite via mock unicast channel.
- `dm_outbox.rs` test: `send_dm` writes both OutboxEntry AND self-InboxEntry; subsequent `read_dm_thread` finds the self entry.
- `dm_outbox.rs` test: `dm-received` IPC payload includes body + mimeType + sentAt (extend the existing `dm_unicast_integration.rs` test).
- `lib.rs` test: `delete_outbox_entry` removes both OutboxEntry and self-InboxEntry, emits `dm-deleted`.

## Verification gates

Per `cargo fmt + cargo clippy gates required at every task verification` user memory rule:

- `cargo fmt --all -- --check` — clean
- `cargo clippy --workspace --all-targets -- -D warnings` — clean
- `cargo test --workspace` — all green
- `npx vitest run` — all green
- `npx tsc --noEmit` — clean

Per `Pipe exit codes lie` user memory rule: any local pipe-based verification uses `set -o pipefail` or `${PIPESTATUS[0]}`.

## Acceptance criteria

Per ZEB-228 (and umbrella ZEB-216):

- [ ] Create a DM (1 recipient) and a GroupDm (2-15 recipients picked = 3-16 members including self) via DmCreateDialog
- [ ] Attempting to pick a 16th recipient (which would yield 17 members including self) is blocked with the inline cap hint pointing at communities (which are coming-soon)
- [ ] Send a DM to an online recipient on another paired device → message arrives in their UI
- [ ] Send a DM to an offline recipient → outbox queues; recipient comes online via any bound device → message delivers
- [ ] Self-sent messages persist across app restart (verified via cold-start scrollback)
- [ ] Receive a DM while UI is on a different channel → unread count increments on the DM's NavNode
- [ ] Receive a DM while UI is on that channel → message appears with auto-scroll
- [ ] 30-day expired messages surface to UI; user can manually delete via inline ⓧ
- [ ] Stuck (>1min sending) messages also offer manual delete
- [ ] Reticulum link failures don't surface to the UI; only persistent failures past threshold do (delivery state stays `'sending'`)
- [ ] All gates green (cargo fmt + clippy + test, vitest, tsc)
- [ ] Manual two-device LAN smoke test deferred to ZEB-239 per umbrella spec precedent

## Open questions / known risks

- **Pagination performance for very long DM threads:** sequential CAS reads in `read_dm_thread` could be slow for threads with thousands of messages. Phase 4 ships with naïve sequential reads; if the manual smoke test reveals UI latency, file a follow-up to batch CAS reads or maintain a side-cache of decrypted bodies.
- **Optimistic-vs-event-sourced UI update timing:** the design defaults to optimistic (push placeholder Message immediately, transition on events). If this produces flicker or off-by-one ordering bugs in practice, fall back to event-sourced (wait for self-InboxEntry's `dm-received` event before showing). Decide based on smoke-test feel.
- **Drag-drop into folders:** assumed the existing nav-tree drag-drop already handles arbitrary NavNode types. If not, verify and file a follow-up — Phase 4 defaults DMs to top-level which is usable even if drag-drop doesn't work yet.
- **Profile lookup for sender names:** depends on NavService's profile map being populated for DM senders. Bootstrap edge case: receive a DM from someone whose profile we haven't seen yet → render with hex address fallback. NavService's `nav-updated` should eventually deliver the profile.
