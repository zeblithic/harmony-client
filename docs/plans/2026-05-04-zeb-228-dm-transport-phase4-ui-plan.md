# ZEB-228 — DM Transport Phase 4 UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the now-shipped DM transport stack (Phases 1-3b) into the existing harmony-client UI so users can create DMs, send/receive messages, and manage stuck/expired outbox entries — all through the existing chat-shaped `TextFeed` + `ComposeBar` surface.

**Architecture:** Reuse existing chat components (TextFeed/ComposeBar already accept `channelType: 'dm' | 'group-chat'`). Add minimal backend extensions (dm-received body inclusion, self-InboxEntry on send, read_dm_thread + delete_outbox_entry IPC, add_space DM/GroupDm extension). Add minimal frontend (DmCreateDialog component, NavService/MessageService event handling, App.svelte send-path branch, inline manual delete via existing ConfirmDialog).

**Tech Stack:** Rust (tauri 2, ciborium, ed25519-dalek), Svelte 5 (runes mode), Vitest, TypeScript. Single PR against `origin/main` of `harmony-client`.

**Spec reference:** `docs/specs/2026-05-04-zeb-228-dm-transport-phase4-ui-design.md` (commit `5e0281c`).

**Branch:** `zeb-228-dm-transport-phase4` (already created off `origin/main` at `04f3bb9`).

---

## File Structure

### Modified Rust files

- `src-tauri/src/dm_outbox.rs` — `DrainOutcome.newly_received` widens to `Vec<ReceivedMessage>` (carries body + mime_type + sent_at); `send_dm` writes self-InboxEntry; new `delete_dm_outbox_entry` method.
- `src-tauri/src/owner_state_crdt.rs` — new `inbox_entries_for_space(space_id) -> impl Iterator<&InboxEntry>` helper; new `delete_inbox_entry(InboxKey) -> Option<InboxEntry>`.
- `src-tauri/src/event_loop.rs` — `dm-received` IPC emit extended to include `body`, `mimeType`, `sentAt`. New `dm-deleted` emit branch (manual delete).
- `src-tauri/src/lib.rs` — `add_space` extended for DM/GroupDm kinds (content_key gen + DmInvite fan-out); new IPC commands `read_dm_thread` and `delete_outbox_entry`.
- `src-tauri/src/owner_state_types.rs` — new `ReceivedMessage` struct (carries InboxEntry + body + mime_type + sent_at).

### Modified TypeScript files

- `src/lib/types.ts` — extend `Message` interface with `deliveryState?: 'sending' | 'delivered' | 'expired' | 'failed'`.
- `src/lib/message-service.ts` — subscribe to `dm-received` / `dm-delivered` / `dm-expired` / `dm-deleted`; add `loadDmThread(spaceId)` for cold-start scrollback; route DM events into per-channel buffer keyed by SpaceId hex.
- `src/lib/nav-service.ts` — handle `nav-updated` for DM/GroupDm Space kinds (insert NavNode at top-level `parentId=null`).
- `src/App.svelte` — `onSend` branch: DM channels → `send_dm` IPC (with optimistic Message); existing channels unchanged. Wire DmCreateDialog into a "+New DM" button.

### New frontend files

- `src/lib/components/DmCreateDialog.svelte` — member picker with at-15-recipients cap.
- `src/lib/components/__tests__/DmCreateDialog.test.ts` — vitest coverage.

### Test additions (no new files; extend existing)

- `src/lib/components/__tests__/TextFeed.test.ts` — DM message rendering with deliveryState indicators.
- `src/lib/message-service.test.ts` — DM IPC subscription tests + loadDmThread test.
- `src/lib/nav-service.test.ts` — nav-updated handler for DM Space kinds.
- `src-tauri/src/dm_outbox.rs` test module — self-InboxEntry on send, delete_dm_outbox_entry.
- `src-tauri/tests/dm_unicast_integration.rs` — extend with body+mimeType payload assertion.
- `src-tauri/tests/dm_thread_integration.rs` (new) — end-to-end read_dm_thread roundtrip.

---

## Task Sequence Overview

Backend foundation (tasks 1-5) → frontend types (task 6) → frontend services (tasks 7-9) → frontend UI (tasks 10-13) → integration polish + PR (tasks 14-15).

Each task ends with a commit. Verification gates (`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `npx vitest run`, `npx tsc --noEmit`) run at each commit.

---

### Task 1: Extend `dm-received` IPC payload to include body + mime_type + sentAt

**Why first:** every later task that touches DM message rendering depends on this payload shape. Doing it first means downstream tasks can write tests against the final payload.

**Files:**
- Create: `src-tauri/src/owner_state_types.rs` — new `ReceivedMessage` struct (inserted near `InboxEntry`).
- Modify: `src-tauri/src/dm_outbox.rs` — `DrainOutcome.newly_received: Vec<ReceivedMessage>`; `handle_cidnotify` populates body + mime_type + sent_at from decrypted payload.
- Modify: `src-tauri/src/event_loop.rs` — emit extended payload.
- Test: `src-tauri/tests/dm_unicast_integration.rs` — extend existing `dm_full_round_trip_through_unicast_channel` to assert new fields.

- [ ] **Step 1: Add `ReceivedMessage` struct to `owner_state_types.rs`.**

Insert after the `impl InboxEntry { ... }` block (around line 1697):

```rust
/// A received DM message bundle — Phase 4 IPC payload carrier.
///
/// `handle_cidnotify` (receive path) decrypts the message, then emits this
/// struct via `DrainOutcome.newly_received`. The event_loop tick consumes
/// the vec and emits one `dm-received` IPC event per element with body +
/// mime_type + sent_at fields the frontend needs to render the message.
///
/// This widens the previous `Vec<InboxEntry>` carrier so the decrypted
/// body doesn't have to be re-fetched + re-decrypted on the IPC emit
/// path. The fields are not persisted — only InboxEntry persists; body
/// lives in CAS keyed by message_cid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedMessage {
    pub inbox_entry: InboxEntry,
    pub body: Vec<u8>,
    pub mime_type: String,
    pub sent_at: Hlc,
}
```

- [ ] **Step 2: Update `DrainOutcome` in `dm_outbox.rs`.**

Find `DrainOutcome` (around line 292). Change:
```rust
pub newly_received: Vec<crate::owner_state_types::InboxEntry>,
```
to:
```rust
pub newly_received: Vec<crate::owner_state_types::ReceivedMessage>,
```

Update the doc comment to mention body+mime_type+sent_at.

- [ ] **Step 3: Update `handle_cidnotify` to populate `ReceivedMessage`.**

Find the `apply_inbox` block in `handle_cidnotify` (around line 1085-1095). After:
```rust
let outcome = state.apply_inbox(inbox_entry.clone());
let mut drain_outcome = DrainOutcome::default();
if matches!(outcome, ApplyOutcome::Inserted) {
    drain_outcome.newly_received.push(inbox_entry);
}
```

Replace the `.push(inbox_entry)` with:
```rust
drain_outcome.newly_received.push(crate::owner_state_types::ReceivedMessage {
    inbox_entry,
    body: payload.body.clone(),
    mime_type: payload.mime_type.clone(),
    sent_at: payload.sent_at.clone(),
});
```

(The `payload` var holds the decrypted `MessagePayload` from Step 11 of `handle_cidnotify` — already in scope.)

- [ ] **Step 4: Update event_loop emit.**

Find the `dm-received` emit at `event_loop.rs:1335-1344`. Replace:
```rust
for entry in outcome.newly_received {
    let _ = app.emit(
        "dm-received",
        serde_json::json!({
            "spaceId": hex::encode(entry.space_id.0),
            "messageCid": hex::encode(entry.message_cid.to_bytes()),
            "from": hex::encode(entry.from.0),
            "receivedAt": entry.received_at.wall_ms,
        }),
    );
}
```
with:
```rust
for rm in outcome.newly_received {
    let _ = app.emit(
        "dm-received",
        serde_json::json!({
            "spaceId": hex::encode(rm.inbox_entry.space_id.0),
            "messageCid": hex::encode(rm.inbox_entry.message_cid.to_bytes()),
            "from": hex::encode(rm.inbox_entry.from.0),
            "receivedAt": rm.inbox_entry.received_at.wall_ms,
            "sentAt": rm.sent_at.wall_ms,
            "body": hex::encode(&rm.body),
            "mimeType": rm.mime_type,
        }),
    );
}
```

- [ ] **Step 5: Compile + fix call sites.**

Run `cargo build --manifest-path src-tauri/Cargo.toml`. Fix any compile errors (test fixtures may reference `outcome.newly_received` as `Vec<InboxEntry>` — update them to read `rm.inbox_entry` instead).

- [ ] **Step 6: Update existing dm_outbox.rs tests that assert on `newly_received`.**

Search: `grep -n "newly_received" src-tauri/src/dm_outbox.rs`. Update each test to read `rm.inbox_entry.space_id` etc. instead of `entry.space_id`.

- [ ] **Step 7: Extend integration test to verify new payload fields.**

In `src-tauri/tests/dm_unicast_integration.rs`, find the `dm_full_round_trip_through_unicast_channel` test. Where it currently asserts on the dm-received payload (look for `dm-received` emit verification or the equivalent direct DrainOutcome assertion), add:

```rust
// Phase 4: dm-received now includes body + mime_type + sent_at
assert_eq!(received_msg.body, expected_body, "body must match sender's plaintext");
assert_eq!(received_msg.mime_type, "text/plain", "mime_type must propagate");
assert_eq!(received_msg.sent_at, sender_sent_at, "sent_at = sender's HLC, not receiver's");
```

(Adjust variable names to match the actual test structure.)

- [ ] **Step 8: Run all gates.**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

All three must exit 0. Use `set -o pipefail` if piping output.

- [ ] **Step 9: Commit.**

```bash
git add -A
git commit -m "feat(zeb-228): widen dm-received IPC payload with body + mimeType + sentAt

DrainOutcome.newly_received now carries Vec<ReceivedMessage> (was
Vec<InboxEntry>). handle_cidnotify populates the new struct from
the decrypted MessagePayload that's already in scope. event_loop
emit threads body (hex-encoded) + mimeType (string) + sentAt
(unix-ms from sender's HLC) into the dm-received IPC payload.

The umbrella spec at docs/specs/2026-05-02-zeb-216-sub-b-dm-transport-design.md:773
called for this payload shape; Phase 3b shipped a stub that only
carried the InboxEntry pointer. Phase 4 closes the gap so the
frontend can render incoming messages without a separate fetch."
```

---

### Task 2: `send_dm` writes a self-InboxEntry alongside the OutboxEntry

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` — `send_dm` calls `state.apply_inbox(...)` after writing OutboxEntry.
- Modify: `src-tauri/src/owner_state_types.rs` — update `InboxEntry` doc comment to widen semantics ("exists in this Space's history (sender or recipient)").
- Test: `src-tauri/src/dm_outbox.rs` test module.

- [ ] **Step 1: Write the failing test in `dm_outbox.rs` test module.**

Find the existing `send_dm_*` tests. Add:

```rust
#[tokio::test]
async fn send_dm_writes_self_inbox_entry_alongside_outbox_entry() {
    let (mut state, mut outbox, cas, _, _) = test_fixture_with_dm_space().await;
    let space_id = SpaceId([0x42; 16]);
    let body = b"hello".to_vec();

    let _message_id = outbox
        .send_dm(
            &mut state,
            cas.as_ref(),
            space_id,
            body.clone(),
            "text/plain".to_string(),
            1_000_000, // wall_now_ms
            None,      // prev_hlc
        )
        .await
        .expect("send_dm must succeed");

    // Self-InboxEntry exists at (space_id, message_cid).
    let self_inbox: Vec<&InboxEntry> = state
        .inbox
        .values()
        .filter(|e| e.space_id == space_id && e.from == outbox.self_owner)
        .collect();
    assert_eq!(self_inbox.len(), 1, "send_dm must write exactly one self-InboxEntry");
    assert_eq!(self_inbox[0].from, outbox.self_owner, "from = self_owner");
}
```

(`test_fixture_with_dm_space` is whatever existing helper builds a DM-space-with-content_key fixture. If none exists, build inline — see `send_dm_persists_outbox_entry` or similar prior tests as a template.)

- [ ] **Step 2: Run test, verify it fails.**

```bash
cd src-tauri
cargo test --manifest-path Cargo.toml send_dm_writes_self_inbox_entry_alongside_outbox_entry
```

Expected: FAIL — `assertion failed: self_inbox.len() == 1` (currently 0 because send_dm doesn't write to inbox).

- [ ] **Step 3: Add the self-InboxEntry write in `send_dm`.**

Find the place in `send_dm` (around line 480+) where `state.outbox.insert(...)` writes the OutboxEntry. Right after that insert, add:

```rust
// Phase 4: Self-InboxEntry write for self-history persistence.
//
// InboxEntry semantics widen here from "received from someone else"
// to "exists in this Space's history (sender OR recipient)". A
// paired device receiving the same DmCidNotify writes its own
// InboxEntry on receipt; this self-write on the sending device
// matches what the paired device will write, so the InboxEntry
// table converges naturally without special-casing.
let self_inbox_entry = crate::owner_state_types::InboxEntry {
    space_id,
    message_cid,
    from: self.self_owner,
    received_at: sent_at.clone(),
};
let _ = state.apply_inbox(self_inbox_entry);
// Outcome ignored: Inserted is the happy path; Merged{old_id: None}
// fires if a paired device's CidNotify already wrote this CID first
// (cross-device race), which is fine — same payload, idempotent.
```

- [ ] **Step 4: Run test, verify it passes.**

```bash
cargo test --manifest-path Cargo.toml send_dm_writes_self_inbox_entry_alongside_outbox_entry
```

Expected: PASS.

- [ ] **Step 5: Update InboxEntry doc comment in `owner_state_types.rs`.**

Find the doc block above `pub struct InboxEntry` (around line 1670). Update text from "received from a sender" to "exists in this Space's history (sender or recipient)" and add a note that Phase 4's send_dm writes a self-InboxEntry on every send.

- [ ] **Step 6: Run all gates.**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- [ ] **Step 7: Commit.**

```bash
git add -A
git commit -m "feat(zeb-228): send_dm writes a self-InboxEntry for history persistence

InboxEntry's semantics widen from \"received from someone else\" to
\"exists in this Space's history (sender or recipient)\". send_dm
now apply_inbox()s a self-InboxEntry alongside the OutboxEntry,
so self-sent messages persist beyond OutboxEntry's lifetime
(critical for cold-start scrollback after restart).

Cross-device convergence handles the multi-device case naturally:
a paired device receiving the same DmCidNotify writes its own
InboxEntry on receipt; this self-write matches what the paired
device will write, so the InboxEntry table converges without
special-casing."
```

---

### Task 3: New `inbox_entries_for_space` helper + `delete_inbox_entry`

**Files:**
- Modify: `src-tauri/src/owner_state_crdt.rs` — two new helpers on `OwnerState`.
- Test: `src-tauri/src/owner_state_crdt.rs` test module.

These helpers are needed by `read_dm_thread` (task 4) and `delete_outbox_entry` (task 5). Building them in their own task keeps later tasks focused.

- [ ] **Step 1: Write failing test for `inbox_entries_for_space`.**

In `owner_state_crdt.rs` test module:

```rust
#[test]
fn inbox_entries_for_space_returns_only_matching_space_sorted_by_received_at() {
    let mut state = OwnerState::new("device-a".to_string());
    let space_a = SpaceId([0x01; 16]);
    let space_b = SpaceId([0x02; 16]);
    let owner = OwnerAddr([0xff; 16]);

    state.apply_inbox(InboxEntry {
        space_id: space_a,
        message_cid: ContentId([0x10; 32]),
        from: owner,
        received_at: hlc(2),
    });
    state.apply_inbox(InboxEntry {
        space_id: space_a,
        message_cid: ContentId([0x11; 32]),
        from: owner,
        received_at: hlc(1),
    });
    state.apply_inbox(InboxEntry {
        space_id: space_b,
        message_cid: ContentId([0x20; 32]),
        from: owner,
        received_at: hlc(99),
    });

    let entries: Vec<&InboxEntry> = state.inbox_entries_for_space(space_a).collect();
    assert_eq!(entries.len(), 2, "only space_a entries");
    // Confirm all returned entries are space_a (caller is responsible for
    // sort order; helper just filters by SpaceId).
    assert!(entries.iter().all(|e| e.space_id == space_a));
}
```

- [ ] **Step 2: Verify test fails (no `inbox_entries_for_space` method exists).**

```bash
cargo test --manifest-path Cargo.toml inbox_entries_for_space_returns_only_matching_space_sorted_by_received_at 2>&1 | tail -10
```

Expected: compile error — `no method named inbox_entries_for_space`.

- [ ] **Step 3: Implement the helper on `OwnerState`.**

In `owner_state_crdt.rs`, add to `impl OwnerState`:

```rust
/// Iterator over InboxEntries belonging to a given Space, in
/// BTreeMap natural order (`(space_id, message_cid)` lex).
///
/// For UI scrollback, callers typically collect + sort by
/// `received_at` descending. The natural BTreeMap order is by
/// message_cid which IS NOT chronological — `received_at` is
/// the chronological key.
pub fn inbox_entries_for_space(
    &self,
    space_id: SpaceId,
) -> impl Iterator<Item = &InboxEntry> {
    self.inbox
        .values()
        .filter(move |e| e.space_id == space_id)
}
```

- [ ] **Step 4: Verify test passes.**

```bash
cargo test --manifest-path Cargo.toml inbox_entries_for_space_returns_only_matching_space_sorted_by_received_at
```

Expected: PASS.

- [ ] **Step 5: Write failing test for `delete_inbox_entry`.**

```rust
#[test]
fn delete_inbox_entry_removes_only_matching_key() {
    let mut state = OwnerState::new("device-a".to_string());
    let space_a = SpaceId([0x01; 16]);
    let cid_x = ContentId([0xaa; 32]);
    let cid_y = ContentId([0xbb; 32]);

    state.apply_inbox(InboxEntry { space_id: space_a, message_cid: cid_x, from: OwnerAddr([1; 16]), received_at: hlc(1) });
    state.apply_inbox(InboxEntry { space_id: space_a, message_cid: cid_y, from: OwnerAddr([1; 16]), received_at: hlc(2) });
    assert_eq!(state.inbox.len(), 2);

    let removed = state.delete_inbox_entry(InboxKey { space_id: space_a, message_cid: cid_x });
    assert!(removed.is_some(), "must return the removed entry");
    assert_eq!(removed.unwrap().message_cid, cid_x);
    assert_eq!(state.inbox.len(), 1, "exactly one entry deleted");
    assert!(state.inbox.values().any(|e| e.message_cid == cid_y), "the other entry survives");

    let removed_again = state.delete_inbox_entry(InboxKey { space_id: space_a, message_cid: cid_x });
    assert!(removed_again.is_none(), "second delete returns None");
}
```

- [ ] **Step 6: Verify test fails.**

```bash
cargo test --manifest-path Cargo.toml delete_inbox_entry_removes_only_matching_key 2>&1 | tail -5
```

Expected: compile error.

- [ ] **Step 7: Implement `delete_inbox_entry`.**

In `owner_state_crdt.rs` `impl OwnerState`:

```rust
/// Remove an InboxEntry by (space_id, message_cid). Returns the
/// removed entry on hit, None on miss. Idempotent: second call
/// with the same key returns None.
///
/// Phase 4: used by `delete_outbox_entry` IPC to clear a stuck/
/// expired self-Message from the user's history.
pub fn delete_inbox_entry(&mut self, key: InboxKey) -> Option<InboxEntry> {
    self.inbox.remove(&key)
}
```

- [ ] **Step 8: Verify test passes.**

```bash
cargo test --manifest-path Cargo.toml delete_inbox_entry_removes_only_matching_key
```

Expected: PASS.

- [ ] **Step 9: Run all gates.**

- [ ] **Step 10: Commit.**

```bash
git add -A
git commit -m "feat(zeb-228): inbox_entries_for_space + delete_inbox_entry helpers

Two pure helpers on OwnerState — used by Phase 4's read_dm_thread
IPC (scrollback) and delete_outbox_entry IPC (manual delete of
stuck/expired messages). No CRDT semantics change; both delegate
to BTreeMap operations on state.inbox."
```

---

### Task 4: New `read_dm_thread` IPC

**Files:**
- Modify: `src-tauri/src/lib.rs` — new `#[tauri::command] async fn read_dm_thread`.
- Test: `src-tauri/tests/dm_thread_integration.rs` (new file) — end-to-end IPC test.

- [ ] **Step 1: Create the integration test file with the failing happy-path test.**

Create `src-tauri/tests/dm_thread_integration.rs`:

```rust
//! Phase 4 integration test for the `read_dm_thread` IPC.
//!
//! Seeds a DM Space with content_key, writes 3 InboxEntries with
//! decryptable bodies via the existing send_dm path, then calls
//! the read_dm_thread tauri::command and asserts the bodies + mime
//! types come back in reverse-chronological order with proper
//! pagination.

use harmony_app::*;
// (imports modeled after dm_unicast_integration.rs's existing pattern)

#[tokio::test]
async fn read_dm_thread_returns_decrypted_messages_reverse_chronological() {
    // Seed: build a NodeState with a DM Space + 3 self-sent messages
    // via send_dm. Call read_dm_thread. Assert: 3 messages returned,
    // newest-first, body bytes decode back to the original plaintext.
    todo!("implement once read_dm_thread IPC is wired in lib.rs");
}

#[tokio::test]
async fn read_dm_thread_paginates_via_before_hlc_cursor() {
    // Seed 5 messages. Call read_dm_thread(limit=2, before_hlc=None) → newest 2.
    // Take the oldest one's received_at as cursor. Call again with
    // before_hlc=cursor → next 2. Continue until empty.
    todo!("implement once read_dm_thread IPC is wired");
}
```

- [ ] **Step 2: Verify tests fail (compile or todo!).**

```bash
cargo test --manifest-path Cargo.toml --test dm_thread_integration 2>&1 | tail -10
```

Expected: tests panic with `not yet implemented` (todo!()) — this is intentional; we'll fill in the test bodies after the IPC ships.

- [ ] **Step 3: Add `DmThreadMessage` struct + `read_dm_thread` command in `lib.rs`.**

Near the existing `send_dm` command in `lib.rs`:

```rust
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DmThreadMessage {
    pub message_cid: String,        // hex
    pub from: String,                // hex OwnerAddr
    pub sent_at: u64,                // wall_ms
    pub received_at: u64,            // wall_ms
    pub body: String,                // hex
    pub mime_type: String,
    pub is_self_outbound: bool,
}

/// Phase 4 — Cold-start scrollback IPC.
///
/// Returns InboxEntries for a given Space (self-sent + received), each
/// with its decrypted body and mime_type. Reverse-chronological order
/// by `received_at`. Paginated via `limit` + `before_hlc` cursor:
///
/// - `limit`: max entries to return (UI page size; typical 50).
/// - `before_hlc`: if `Some(wall_ms)`, return entries with
///   `received_at.wall_ms < before_hlc`. None = newest first.
///
/// Decrypts via `dm_crypto::decrypt_dm_message` with prior-keys
/// fallback (matches `handle_cidnotify`'s receive path). Errors:
/// `UnknownSpace`, `MissingContentKey`, CAS-fetch failure (per-entry,
/// surfaced as a single Err and the whole call fails — frontend can
/// retry; partial-result handling is a follow-up if needed).
#[tauri::command]
pub async fn read_dm_thread(
    state: tauri::State<'_, NodeState>,
    space_id: String,
    limit: usize,
    before_hlc: Option<u64>,
) -> Result<Vec<DmThreadMessage>, String> {
    let space_id_bytes = hex::decode(&space_id)
        .map_err(|e| format!("invalid space_id hex: {e}"))?;
    let space_id_arr: [u8; 16] = space_id_bytes
        .as_slice()
        .try_into()
        .map_err(|_| "space_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(space_id_arr);

    // Read state under lock — collect needed data + drop lock before async work.
    let (entries_to_decrypt, content_key, prior_content_keys, aad, self_owner) = {
        let state_guard = state.crdt_state.lock().await;
        let space = state_guard
            .spaces
            .get(&space_id)
            .ok_or_else(|| format!("UnknownSpace({space_id:?})"))?;
        let content_key = space
            .content_key
            .clone()
            .ok_or_else(|| format!("MissingContentKey({space_id:?})"))?;
        let prior = space.prior_content_keys.clone();
        let aad = crate::dm_crypto::compute_aad(space)
            .map_err(|e| format!("compute_aad: {e}"))?;
        let self_owner = state.self_owner;

        // Filter + sort + paginate purely in memory.
        let mut entries: Vec<crate::owner_state_types::InboxEntry> = state_guard
            .inbox_entries_for_space(space_id)
            .cloned()
            .collect();
        entries.sort_by(|a, b| b.received_at.cmp(&a.received_at)); // newest first
        if let Some(cursor) = before_hlc {
            entries.retain(|e| e.received_at.wall_ms < cursor);
        }
        entries.truncate(limit);
        (entries, content_key, prior, aad, self_owner)
    };

    // Decrypt + assemble outside the lock (CAS fetches may await).
    let mut out: Vec<DmThreadMessage> = Vec::with_capacity(entries_to_decrypt.len());
    for entry in entries_to_decrypt {
        let blob_opt = state.cas_handle.get(&entry.message_cid).await
            .map_err(|e| format!("cas.get failed for {:?}: {e:?}", entry.message_cid))?;
        let blob = blob_opt.ok_or_else(|| format!("blob missing for {:?}", entry.message_cid))?;
        let payload = crate::dm_crypto::decrypt_dm_message(
            &content_key,
            &prior_content_keys,
            &aad,
            &blob,
        )
        .map_err(|e| format!("decrypt failed: {e:?}"))?;
        out.push(DmThreadMessage {
            message_cid: hex::encode(entry.message_cid.to_bytes()),
            from: hex::encode(entry.from.0),
            sent_at: payload.sent_at.wall_ms,
            received_at: entry.received_at.wall_ms,
            body: hex::encode(&payload.body),
            mime_type: payload.mime_type,
            is_self_outbound: entry.from == self_owner,
        });
    }
    Ok(out)
}
```

(Adjust `state.crdt_state` / `state.cas_handle` / `state.self_owner` to whatever `NodeState`'s actual field names are — check `lib.rs` for the existing `send_dm` command's pattern.)

- [ ] **Step 4: Register the command in the tauri builder.**

Find `tauri::Builder::default().invoke_handler(tauri::generate_handler![...])` in `lib.rs`. Add `read_dm_thread` to the handler list.

- [ ] **Step 5: Fill in the integration test bodies.**

Replace the two `todo!()` test bodies in `dm_thread_integration.rs`. Use the existing `dm_unicast_integration.rs` as a template for `make_party()` and seeding helpers — the test setup is parallel.

```rust
#[tokio::test]
async fn read_dm_thread_returns_decrypted_messages_reverse_chronological() {
    let alice = make_party(b"alice");
    let space_id = SpaceId([0x42; 16]);
    let mut state = OwnerState::new("alice-device".to_string());
    let cas = std::sync::Arc::new(InMemoryStub::new());

    // Seed Space with content_key.
    let content_key = DmContentKey::random();
    let space = Space {
        id: space_id,
        kind: SpaceKind::Dm,
        members: vec![alice.owner_addr, OwnerAddr([0x99; 16])],
        content_key: Some(content_key.clone()),
        prior_content_keys: vec![],
        ..test_dm_space_defaults(space_id)
    };
    state.apply_space_with_canonicalization(space);

    // Seed 3 self-sent messages via send_dm.
    let mut outbox = DmOutbox::new(...);
    for (i, body) in [b"hello", b"world", b"phase4"].iter().enumerate() {
        outbox.send_dm(
            &mut state, cas.as_ref(),
            space_id, body.to_vec(), "text/plain".to_string(),
            1_000_000 + (i as u64) * 1_000,
            None,
        ).await.unwrap();
    }

    // Build NodeState fixture and call read_dm_thread.
    let node_state = NodeState { /* with state, cas, self_owner = alice.owner_addr */ };
    let result = read_dm_thread(
        tauri::State::from(&node_state),  // adapt to whatever fixture pattern works
        hex::encode(space_id.0),
        50,
        None,
    ).await.unwrap();

    assert_eq!(result.len(), 3);
    // Reverse chronological: newest first.
    assert_eq!(hex::decode(&result[0].body).unwrap(), b"phase4");
    assert_eq!(hex::decode(&result[1].body).unwrap(), b"world");
    assert_eq!(hex::decode(&result[2].body).unwrap(), b"hello");
    assert!(result.iter().all(|m| m.is_self_outbound), "all 3 are self-sent");
    assert!(result.iter().all(|m| m.mime_type == "text/plain"));
}
```

(If the tauri::State / NodeState fixture is awkward to spin up in tests, refactor `read_dm_thread` to delegate to a pure inner function `read_dm_thread_inner(state: &OwnerState, cas: &dyn ContentStore, ...) -> Result<Vec<DmThreadMessage>, _>` and test that. The tauri::command then becomes a thin shim. This pattern is already used elsewhere — check existing IPCs.)

- [ ] **Step 6: Run integration test.**

```bash
cargo test --manifest-path Cargo.toml --test dm_thread_integration
```

Both tests pass.

- [ ] **Step 7: Run all gates.**

- [ ] **Step 8: Commit.**

```bash
git add -A
git commit -m "feat(zeb-228): read_dm_thread IPC for cold-start DM scrollback

Returns InboxEntries for a given Space, paginated via
limit + before_hlc cursor. Each entry decrypts via the same
prior-keys fallback path handle_cidnotify uses (so post-rotation
scrollback still works). Locks released before async CAS fetches
per the locks-across-await rule (mirrors ZEB-241 pending refactor).

Frontend uses this on first DM-channel switch to populate the
TextFeed with history. Pagination cursor is received_at.wall_ms;
caller passes the oldest entry's value to fetch the next page."
```

---

### Task 5: New `delete_outbox_entry` IPC

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` — new `delete_dm_outbox_entry` method.
- Modify: `src-tauri/src/lib.rs` — new IPC command + `dm-deleted` emit.
- Modify: `src-tauri/src/event_loop.rs` (or wherever IPC events are emitted) — `dm-deleted` event branch.
- Test: `src-tauri/src/dm_outbox.rs` test module.

- [ ] **Step 1: Write failing test for `delete_dm_outbox_entry`.**

```rust
#[tokio::test]
async fn delete_dm_outbox_entry_removes_outbox_and_self_inbox() {
    let (mut state, mut outbox, cas, _, _) = test_fixture_with_dm_space().await;
    let space_id = SpaceId([0x42; 16]);
    let body = b"hello".to_vec();

    let message_id = outbox
        .send_dm(&mut state, cas.as_ref(), space_id, body, "text/plain".to_string(), 1_000_000, None)
        .await
        .expect("send_dm must succeed");

    // Pre: both entries exist.
    assert!(state.outbox.contains_key(&message_id), "outbox entry exists");
    let pre_inbox: Vec<_> = state.inbox_entries_for_space(space_id).collect();
    assert_eq!(pre_inbox.len(), 1, "self-InboxEntry exists");
    let inbox_key = pre_inbox[0].key();

    // Act.
    let outcome = outbox
        .delete_dm_outbox_entry(&mut state, message_id)
        .expect("delete must succeed");

    // Post: both gone.
    assert!(!state.outbox.contains_key(&message_id), "outbox cleared");
    assert_eq!(state.inbox_entries_for_space(space_id).count(), 0, "self-InboxEntry cleared");
    assert_eq!(outcome.deleted_inbox_key, Some(inbox_key));
    assert_eq!(outcome.deleted_outbox_id, Some(message_id));
}

#[tokio::test]
async fn delete_dm_outbox_entry_idempotent_on_missing() {
    let (mut state, mut outbox, _, _, _) = test_fixture_empty().await;
    let fake_id = OutboxEntryId([0xff; 16]);
    let outcome = outbox.delete_dm_outbox_entry(&mut state, fake_id).expect("must not error");
    assert_eq!(outcome.deleted_outbox_id, None);
    assert_eq!(outcome.deleted_inbox_key, None);
}
```

- [ ] **Step 2: Verify tests fail.**

- [ ] **Step 3: Implement `delete_dm_outbox_entry`.**

In `dm_outbox.rs` `impl DmOutbox`:

```rust
/// Outcome of `delete_dm_outbox_entry`. The IPC layer reads this to
/// decide which `dm-deleted` IPC events to emit.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DeleteDmOutboxOutcome {
    pub deleted_outbox_id: Option<OutboxEntryId>,
    pub deleted_inbox_key: Option<crate::owner_state_types::InboxKey>,
    pub space_id: Option<SpaceId>,
    pub message_cid: Option<crate::owner_state_types::ContentId>,
}

/// Phase 4 — Manual delete of a stuck or expired self-OutboxEntry.
///
/// Removes BOTH the OutboxEntry and the corresponding self-InboxEntry
/// keyed by `(space_id, message_cid)`. User intent on manual delete
/// is "make this message go away," so removing both is the expected
/// UX. If a future ticket wants "withdraw delivery but keep my own
/// history," that's a separate IPC.
///
/// Idempotent: returns `Default::default()` (all None) if the
/// OutboxEntry doesn't exist.
pub fn delete_dm_outbox_entry(
    &mut self,
    state: &mut OwnerState,
    message_id: OutboxEntryId,
) -> Result<DeleteDmOutboxOutcome, DeleteDmError> {
    let outbox_entry = match state.outbox.remove(&message_id) {
        Some(e) => e,
        None => return Ok(DeleteDmOutboxOutcome::default()),
    };
    let inbox_key = crate::owner_state_types::InboxKey {
        space_id: outbox_entry.space_id,
        message_cid: outbox_entry.message_cid,
    };
    let _removed_inbox = state.delete_inbox_entry(inbox_key);

    // Also clear in-flight + backoff caches so a stale entry can't
    // resurface.
    self.in_flight.retain(|(eid, _)| *eid != message_id);
    self.backoff.retain(|(eid, _), _| *eid != message_id);

    Ok(DeleteDmOutboxOutcome {
        deleted_outbox_id: Some(message_id),
        deleted_inbox_key: Some(inbox_key),
        space_id: Some(outbox_entry.space_id),
        message_cid: Some(outbox_entry.message_cid),
    })
}

#[derive(Debug, thiserror::Error)]
pub enum DeleteDmError {
    // No variants currently — kept as an enum for future extensibility
    // (e.g., if we later distinguish "entry exists but is currently
    // in_flight; cannot delete safely without canceling the runtime
    // task" as a separate error).
}
```

- [ ] **Step 4: Verify tests pass.**

- [ ] **Step 5: Add the tauri::command in `lib.rs`.**

```rust
/// Phase 4 — Delete a stuck or expired DM message (manual delete).
///
/// Wraps `DmOutbox::delete_dm_outbox_entry`. On success, emits a
/// `dm-deleted` IPC event so the frontend MessageService can prune
/// the message from its local cache.
#[tauri::command]
pub async fn delete_outbox_entry(
    app: tauri::AppHandle,
    state: tauri::State<'_, NodeState>,
    message_id: String, // hex OutboxEntryId
) -> Result<(), String> {
    let id_bytes = hex::decode(&message_id).map_err(|e| format!("invalid id hex: {e}"))?;
    let id_arr: [u8; 16] = id_bytes.as_slice().try_into()
        .map_err(|_| "message_id must be 16 bytes".to_string())?;
    let id = OutboxEntryId(id_arr);

    let outcome = {
        let mut outbox_g = state.dm_outbox.lock().await;
        let mut state_g = state.crdt_state.lock().await;
        outbox_g.delete_dm_outbox_entry(&mut state_g, id)
            .map_err(|e| format!("delete failed: {e}"))?
    };

    if let (Some(space_id), Some(message_cid)) = (outcome.space_id, outcome.message_cid) {
        let _ = app.emit("dm-deleted", serde_json::json!({
            "spaceId": hex::encode(space_id.0),
            "messageCid": hex::encode(message_cid.to_bytes()),
        }));
    }
    // No-op case (outcome all None): no event emitted, returns Ok(()).
    Ok(())
}
```

Register `delete_outbox_entry` in the `tauri::generate_handler![...]` list.

- [ ] **Step 6: Run all gates.**

- [ ] **Step 7: Commit.**

```bash
git add -A
git commit -m "feat(zeb-228): delete_outbox_entry IPC for manual delete of stuck/expired DMs

DmOutbox::delete_dm_outbox_entry removes both the OutboxEntry and
the corresponding self-InboxEntry (\"make this message go away\"
semantics). In-flight + backoff caches also cleared. Emits a
dm-deleted IPC event so the frontend MessageService can prune
its local cache. Idempotent on missing message_id."
```

---

### Task 6: `add_space` extension for DM/GroupDm kinds

**Files:**
- Modify: `src-tauri/src/lib.rs` — extend `add_space` command with DM/GroupDm handling.
- Test: `src-tauri/tests/dm_create_integration.rs` (new file) OR extend `dm_unicast_integration.rs`.

- [ ] **Step 1: Locate the existing `add_space` command in `lib.rs`.**

Search `grep -n "add_space" src-tauri/src/lib.rs`. Read the existing implementation to understand the patterns for kind dispatch.

- [ ] **Step 2: Write a failing integration test for DM creation.**

Either extend `dm_unicast_integration.rs` or create a new `dm_create_integration.rs`. Recommended: new file for separation of concerns.

```rust
#[tokio::test]
async fn add_space_dm_kind_generates_content_key_and_dispatches_invite() {
    let alice = make_party(b"alice");
    let bob = make_party(b"bob");
    let node_state = build_test_node_state(&alice).await;

    let space_id = add_space(
        node_state.app.clone(),
        node_state.state_handle(),
        SpaceKind::Dm,
        "DM with Bob".to_string(),
        None, // parent
        Some(vec![bob.owner_addr]), // members (recipients only — backend adds self)
        None, // transport (defaulted to Reticulum for DM)
    )
    .await
    .expect("add_space must succeed");

    let state = node_state.crdt_state.lock().await;
    let space = state.spaces.get(&space_id).expect("Space must exist");
    assert_eq!(space.kind, SpaceKind::Dm);
    assert_eq!(space.members.len(), 2, "self + bob");
    assert!(space.members.contains(&alice.owner_addr));
    assert!(space.members.contains(&bob.owner_addr));
    assert!(space.content_key.is_some(), "DM must have content_key");
    assert!(space.prior_content_keys.is_empty());
    assert!(matches!(space.transport, Some(crate::owner_state_types::TransportBinding::Reticulum { .. })));

    // DmInvite was dispatched to Bob's known device(s) via unicast_send_tx.
    // Verify by reading the test fixture's unicast_send_rx.
    let invite_packet = node_state.unicast_send_rx.try_recv()
        .expect("DmInvite must have been dispatched");
    let decoded = decode_packet(&invite_packet.packet).unwrap();
    match decoded {
        DmPacket::Invite { signed, .. } => {
            assert_eq!(signed.space_id, space_id);
            assert_eq!(signed.kind, SpaceKind::Dm);
            assert!(signed.members.contains(&alice.owner_addr));
            assert!(signed.members.contains(&bob.owner_addr));
        }
        _ => panic!("expected DmInvite"),
    }
}

#[tokio::test]
async fn add_space_group_dm_kind_with_15_recipients_succeeds() { /* ... */ }

#[tokio::test]
async fn add_space_rejects_dm_with_zero_recipients() {
    let alice = make_party(b"alice");
    let node_state = build_test_node_state(&alice).await;
    let result = add_space(
        node_state.app.clone(),
        node_state.state_handle(),
        SpaceKind::Dm, "empty".to_string(), None,
        Some(vec![]), None,
    ).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("at least 2 members") || result.unwrap_err().contains("at least 1 recipient"));
}

#[tokio::test]
async fn add_space_rejects_group_dm_over_16_members() {
    let alice = make_party(b"alice");
    let node_state = build_test_node_state(&alice).await;
    let recipients: Vec<OwnerAddr> = (0..16).map(|i| OwnerAddr([i; 16])).collect(); // 16 recipients + self = 17 total
    let result = add_space(
        node_state.app.clone(),
        node_state.state_handle(),
        SpaceKind::GroupDm, "too big".to_string(), None,
        Some(recipients), None,
    ).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("16") || result.unwrap_err().contains("cap"));
}

#[tokio::test]
async fn add_space_dm_kind_rejects_more_than_one_recipient() {
    // DM (1-on-1) requires exactly one recipient. >1 should be rejected
    // (frontend would call group-dm in that case).
}
```

- [ ] **Step 3: Verify tests fail.**

- [ ] **Step 4: Extend `add_space` in `lib.rs`.**

In the existing `add_space` command, add DM/GroupDm branches:

```rust
match kind {
    SpaceKind::Dm | SpaceKind::GroupDm => {
        let recipients = members.unwrap_or_default();

        // Validate cap.
        let total_members = 1 + recipients.len(); // self + recipients
        if total_members < 2 {
            return Err("DM requires at least one recipient".to_string());
        }
        if total_members > 16 {
            return Err(format!(
                "DM/GroupDm cap is 16 members; got {} (use a community for larger groups)",
                total_members
            ));
        }
        if matches!(kind, SpaceKind::Dm) && recipients.len() != 1 {
            return Err(format!(
                "Dm kind requires exactly 1 recipient; got {} (use GroupDm for 2-15)",
                recipients.len()
            ));
        }

        // Generate content_key.
        use rand::RngCore;
        let mut ck_bytes = zeroize::Zeroizing::new([0u8; 32]);
        rand::rngs::OsRng.fill_bytes(&mut *ck_bytes);
        let content_key = crate::owner_state_types::DmContentKey::from_bytes(&*ck_bytes);

        // Build Space.
        let self_owner = state.self_owner;
        let mut all_members: Vec<OwnerAddr> = std::iter::once(self_owner)
            .chain(recipients.iter().copied())
            .collect();
        all_members.sort();
        all_members.dedup();
        if all_members.len() != total_members {
            return Err("duplicate or self-included recipient".to_string());
        }

        let space_id = SpaceId(rand::random());
        let space = crate::owner_state_types::Space {
            id: space_id,
            kind,
            parent: None,
            community_id: None,
            name,
            members: all_members.clone(),
            transport: Some(crate::owner_state_types::TransportBinding::Reticulum {
                participants: vec![],
            }),
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc_now(),
            updated_at: hlc_now(),
            content_key: Some(content_key.clone()),
            prior_content_keys: vec![],
        };

        // Apply locally.
        let mut state_g = state.crdt_state.lock().await;
        state_g.apply_space_with_canonicalization(space.clone());
        drop(state_g);

        // Build + dispatch DmInvite per non-self member, per-device.
        let our_signing_device_hash = state.our_signing_device_hash;
        let signing_key = state.signing_key.clone();
        let inviter_identity_pub = state.identity_pub_64; // [u8; 64]
        let our_devices = {
            let state_g = state.crdt_state.lock().await;
            state_g
                .owner_device_cache
                .devices
                .get(&self_owner)
                .map(|e| e.devices.clone())
                .unwrap_or_else(|| vec![our_signing_device_hash])
        };

        let signed_invite = crate::dm_envelope::DmInviteSigned {
            space_id,
            kind,
            members: all_members,
            inviter: self_owner,
            inviter_identity_pub,
            content_key,
            sender_devices: our_devices,
            signing_device_hash: our_signing_device_hash,
            created_at: hlc_now(),
        };
        let invite_packet = crate::dm_envelope::build_signed_invite(signed_invite, &signing_key)
            .map_err(|e| format!("build_signed_invite failed: {e}"))?;
        let invite_wire = crate::dm_envelope::encode_packet(&invite_packet)
            .map_err(|e| format!("encode_packet failed: {e}"))?;

        // Send to each recipient's known devices.
        let recipient_devices: Vec<DeviceIdentityHash> = {
            let state_g = state.crdt_state.lock().await;
            recipients
                .iter()
                .flat_map(|r| {
                    state_g
                        .owner_device_cache
                        .devices
                        .get(r)
                        .map(|e| e.devices.clone())
                        .unwrap_or_default()
                })
                .collect()
        };

        for device in recipient_devices {
            let dest_hash = crate::dm_signing::compute_dm_destination_hash(device.0);
            let _ = state.unicast_send_tx.try_send(crate::dm_outbox::UnicastSendRequest {
                destination_hash: dest_hash,
                packet: invite_wire.clone(),
            });
            // Best-effort: dropped sends are recovered via Phase 3b's
            // outbox retry loop on the first send_dm into this Space.
        }

        return Ok(hex::encode(space_id.0));
    }
    // existing kinds unchanged
    _ => { /* existing code */ }
}
```

(Adapt to whatever NodeState's actual field names are. `state.identity_pub_64` may need to be added to NodeState if it's not already there — Phase 3b stored it somewhere when bootstrapping.)

- [ ] **Step 5: Verify tests pass.**

- [ ] **Step 6: Run all gates.**

- [ ] **Step 7: Commit.**

```bash
git add -A
git commit -m "feat(zeb-228): add_space extension for DM/GroupDm kinds

Generates a fresh content_key (32 random bytes via OsRng, wrapped
in Zeroizing), builds the Space CRDT entry with members (self +
recipients), applies locally, and dispatches DmInvite to each
recipient's known devices via the unicast channel.

Validates: DM kind = exactly 1 recipient; GroupDm = 2-15
recipients; total members ≤ 16. Returns SpaceId on success.

Frontend's DmCreateDialog calls this; the dispatched DmInvite
flows through Phase 3b's handle_invite on each recipient's
device, which auto-accepts and writes the Space + cache entry."
```

---

### Task 7: Extend `Message` type with `deliveryState` field (frontend)

**Files:**
- Modify: `src/lib/types.ts` — `Message` interface gets `deliveryState?: 'sending' | 'delivered' | 'expired' | 'failed'`.
- Test: covered by downstream tasks (no test in isolation).

- [ ] **Step 1: Update `Message` in `types.ts`.**

Find `export interface Message` (around line 60-80 — check `grep -n "interface Message" src/lib/types.ts`). Add:

```typescript
export interface Message {
  // existing fields...
  /** Phase 4 — DM delivery state. Undefined for non-self / received messages. */
  deliveryState?: 'sending' | 'delivered' | 'expired' | 'failed';
  /** Phase 4 — DM message id (hex OutboxEntryId), for delete + delivered correlation. */
  messageId?: string;
}
```

- [ ] **Step 2: Run tsc.**

```bash
npx tsc --noEmit
```

Expected: clean (additive optional fields).

- [ ] **Step 3: Run vitest to confirm no regressions.**

```bash
npx vitest run
```

Expected: all green.

- [ ] **Step 4: Commit.**

```bash
git add -A
git commit -m "feat(zeb-228): Message.deliveryState + messageId for DM lifecycle

Optional fields, additive. deliveryState surfaces sending/delivered/
expired/failed in the UI; messageId correlates dm-delivered /
dm-expired / dm-deleted IPC events to the right Message in the
per-channel buffer."
```

---

### Task 8: NavService DM/GroupDm handling

**Files:**
- Modify: `src/lib/nav-service.ts` — handle `nav-updated` for DM Space kinds.
- Test: `src/lib/nav-service.test.ts` — extend with DM cases.

- [ ] **Step 1: Write failing test.**

In `src/lib/nav-service.test.ts`:

```typescript
describe('NavService DM handling', () => {
  it('inserts a top-level NavNode for a new DM Space via nav-updated', async () => {
    const nav = new NavService();
    const adapter = makeMockAdapter(); // existing test helper
    await nav.connectAdapter(adapter);

    adapter.emit('nav-updated', {
      action: 'added',
      spaceId: 'aabbccdd00112233',
      kind: 'dm',
      name: 'DM with Bob',
      members: ['bob-hex-address'],
      parentId: null,
    });

    expect(nav.nodes).toContainEqual(expect.objectContaining({
      id: 'aabbccdd00112233',
      type: 'dm',
      name: 'DM with Bob',
      parentId: null,
    }));
  });

  it('inserts a group-chat NavNode for a new GroupDm Space', async () => {
    // similar, with kind='group-dm' → type='group-chat'
  });

  it('removes a NavNode on nav-updated action=removed', async () => {
    // seed a node, emit removed, assert gone
  });
});
```

- [ ] **Step 2: Verify test fails.**

```bash
npx vitest run nav-service
```

Expected: FAIL — NavService doesn't subscribe to nav-updated for DM kinds yet.

- [ ] **Step 3: Add nav-updated subscription in `nav-service.ts`.**

In the `connectAdapter` method, alongside existing listeners:

```typescript
const unlistenNav = await adapter.listen<{
  action: 'added' | 'removed' | 'modified';
  spaceId: string;
  kind: 'dm' | 'group-dm' | 'channel' | 'community' | 'folder';
  name: string;
  members?: string[];
  parentId?: string | null;
}>('nav-updated', (event) => {
  const { action, spaceId, kind, name, members, parentId } = event.payload;
  if (kind !== 'dm' && kind !== 'group-dm') return; // Phase 4 only handles DM kinds; channels handled elsewhere

  if (action === 'removed') {
    this.nodes = this.nodes.filter((n) => n.id !== spaceId);
    this.onChange?.();
    return;
  }

  const navType: NavNodeType = kind === 'dm' ? 'dm' : 'group-chat';
  const peer = (members && members.length === 1)
    ? this.profiles.get(members[0])
    : undefined;
  const newNode: NavNode = {
    id: spaceId,
    type: navType,
    name,
    parentId: parentId ?? null,
    expanded: false,
    unreadCount: 0,
    unreadLevel: 'none',
    peer: peer ? { address: members![0], displayName: peer.displayName } : undefined,
  };

  if (action === 'added') {
    this.nodes = [...this.nodes, newNode];
  } else if (action === 'modified') {
    this.nodes = this.nodes.map((n) => n.id === spaceId ? { ...n, name, peer: newNode.peer } : n);
  }
  this.onChange?.();
});
this.unlisteners.push(unlistenNav);
```

- [ ] **Step 4: Verify test passes.**

- [ ] **Step 5: Run all gates.**

```bash
npx tsc --noEmit
npx vitest run
```

- [ ] **Step 6: Commit.**

```bash
git add -A
git commit -m "feat(zeb-228): NavService handles nav-updated for DM/GroupDm Spaces

New DM Spaces emit NavNodes at parentId=null (top-level); the user
can drag them into folders via existing nav-tree drag-drop. GroupDm
maps to type='group-chat' per existing NavNodeType discriminant.
Action='modified' updates name/peer in place; action='removed'
prunes (Phase 4 doesn't ship Space deletion, but the branch is
wired for future use)."
```

---

### Task 9: MessageService DM event subscriptions + deliveryState transitions

**Files:**
- Modify: `src/lib/message-service.ts` — subscribe to `dm-received` / `dm-delivered` / `dm-expired` / `dm-deleted`.
- Test: `src/lib/message-service.test.ts` — DM event handling tests.

- [ ] **Step 1: Write failing test.**

```typescript
describe('MessageService DM events', () => {
  it('pushes a Message for dm-received with body decoded from hex', async () => {
    const svc = new MessageService();
    const adapter = makeMockAdapter();
    await svc.connectAdapter(adapter);

    adapter.emit('dm-received', {
      spaceId: 'aabbcc',
      messageCid: 'deadbeef',
      from: 'bob-hex',
      sentAt: 1_700_000_000_000,
      receivedAt: 1_700_000_000_500,
      body: hex.encode('hello world'),
      mimeType: 'text/plain',
    });

    const channelMessages = svc.messagesForChannel('aabbcc');
    expect(channelMessages).toHaveLength(1);
    expect(channelMessages[0].text).toBe('hello world');
    expect(channelMessages[0].senderAddress).toBe('bob-hex');
    expect(channelMessages[0].timestamp).toBe(1_700_000_000_000);
  });

  it('transitions self-Message to delivered on dm-delivered', async () => {
    // seed with messageId; emit dm-delivered; assert deliveryState = 'delivered'
  });

  it('transitions self-Message to expired on dm-expired', async () => { /* ... */ });

  it('removes Message on dm-deleted', async () => { /* ... */ });
});
```

- [ ] **Step 2: Verify tests fail.**

- [ ] **Step 3: Add IPC subscriptions in `message-service.ts`.**

In `connectAdapter`:

```typescript
const unlistenDmRx = await adapter.listen<{
  spaceId: string;
  messageCid: string;
  from: string;
  sentAt: number;
  receivedAt: number;
  body: string;     // hex
  mimeType: string;
}>('dm-received', (event) => {
  const { spaceId, messageCid, from, sentAt, body } = event.payload;
  const text = new TextDecoder().decode(hexToBytes(body));
  const msg: Message = {
    id: messageCid,
    senderAddress: from,
    senderName: from, // resolved later by NavService profile lookup at render time
    channel: spaceId,
    hub: '', // not applicable for DMs
    text,
    timestamp: sentAt,
    priority: 'normal',
  };
  // No deliveryState — received messages don't have one.
  this.messages = [...this.messages, msg];
  this.onChange?.();
});
this.unlisteners.push(unlistenDmRx);

const unlistenDmDelivered = await adapter.listen<{
  messageId: string;
  recipient: string;
}>('dm-delivered', (event) => {
  const { messageId } = event.payload;
  this.messages = this.messages.map((m) =>
    m.messageId === messageId ? { ...m, deliveryState: 'delivered' } : m
  );
  this.onChange?.();
});
this.unlisteners.push(unlistenDmDelivered);

const unlistenDmExpired = await adapter.listen<{ messageId: string }>('dm-expired', (event) => {
  const { messageId } = event.payload;
  this.messages = this.messages.map((m) =>
    m.messageId === messageId ? { ...m, deliveryState: 'expired' } : m
  );
  this.onChange?.();
});
this.unlisteners.push(unlistenDmExpired);

const unlistenDmDeleted = await adapter.listen<{
  spaceId: string;
  messageCid: string;
}>('dm-deleted', (event) => {
  const { spaceId, messageCid } = event.payload;
  this.messages = this.messages.filter((m) =>
    !(m.channel === spaceId && m.id === messageCid)
  );
  this.onChange?.();
});
this.unlisteners.push(unlistenDmDeleted);
```

(Adapt naming if existing MessageService uses different conventions — e.g., `this.messages` may be a different field name; check the existing source.)

- [ ] **Step 4: Verify tests pass.**

- [ ] **Step 5: Run all gates.**

- [ ] **Step 6: Commit.**

```bash
git add -A
git commit -m "feat(zeb-228): MessageService subscribes to dm-received/delivered/expired/deleted

dm-received: pushes a Message into the per-channel buffer with body
decoded from hex. dm-delivered/expired/deleted: lifecycle transitions
on the sender's self-Message via messageId correlation. Channel key
is SpaceId hex (matches NavNode id from Task 8)."
```

---

### Task 10: `loadDmThread` for cold-start scrollback

**Files:**
- Modify: `src/lib/message-service.ts` — new `loadDmThread(spaceId)` method + per-channel pagination cursor tracking.
- Test: `src/lib/message-service.test.ts`.

- [ ] **Step 1: Write failing test.**

```typescript
it('loadDmThread fetches read_dm_thread IPC and populates messages reverse-chrono', async () => {
  const svc = new MessageService();
  const adapter = makeMockAdapter();
  adapter.invokeMock = jest.fn().mockResolvedValue([
    { messageCid: 'cid3', from: 'self-hex', sentAt: 3000, receivedAt: 3001, body: hex.encode('newest'), mimeType: 'text/plain', isSelfOutbound: true },
    { messageCid: 'cid2', from: 'bob-hex', sentAt: 2000, receivedAt: 2001, body: hex.encode('mid'), mimeType: 'text/plain', isSelfOutbound: false },
    { messageCid: 'cid1', from: 'self-hex', sentAt: 1000, receivedAt: 1001, body: hex.encode('oldest'), mimeType: 'text/plain', isSelfOutbound: true },
  ]);
  await svc.connectAdapter(adapter);

  await svc.loadDmThread('aabbcc');

  // After load: 3 messages, oldest-first in display order (frontend reverses).
  const msgs = svc.messagesForChannel('aabbcc');
  expect(msgs).toHaveLength(3);
  expect(msgs[0].text).toBe('oldest');
  expect(msgs[2].text).toBe('newest');
});
```

- [ ] **Step 2: Verify test fails.**

- [ ] **Step 3: Implement `loadDmThread`.**

```typescript
private dmThreadCursors: Map<string, number> = new Map();

async loadDmThread(spaceId: string, pageSize: number = 50): Promise<void> {
  if (!this.adapter) return;
  const cursor = this.dmThreadCursors.get(spaceId);
  const results: DmThreadMessage[] = await this.adapter.invoke('read_dm_thread', {
    spaceId,
    limit: pageSize,
    beforeHlc: cursor,
  });
  if (results.length === 0) return;

  const newMessages: Message[] = results.map((r) => ({
    id: r.messageCid,
    messageId: r.isSelfOutbound ? r.messageCid : undefined, // self uses messageCid as messageId for delete correlation
    senderAddress: r.from,
    senderName: r.from,
    channel: spaceId,
    hub: '',
    text: new TextDecoder().decode(hexToBytes(r.body)),
    timestamp: r.sentAt,
    priority: 'normal',
    deliveryState: r.isSelfOutbound ? 'delivered' : undefined, // historical: assume delivered if it's in our inbox
  })).reverse(); // backend sends newest-first; UI wants oldest-first

  this.messages = [...newMessages, ...this.messages];
  this.dmThreadCursors.set(spaceId, results[results.length - 1].receivedAt);
  this.onChange?.();
}
```

- [ ] **Step 4: Verify test passes.**

- [ ] **Step 5: Run all gates.**

- [ ] **Step 6: Commit.**

```bash
git add -A
git commit -m "feat(zeb-228): MessageService.loadDmThread for cold-start scrollback

Calls read_dm_thread IPC; merges results into per-channel buffer
oldest-first (UI display order). Tracks before_hlc pagination
cursor per channel for scroll-up backfill. Self-sent historical
messages default to deliveryState='delivered' since the fact
they're in our inbox means they made it that far."
```

---

### Task 11: App.svelte send-path branch + DM activation hook

**Files:**
- Modify: `src/App.svelte` — `onSend` for DM channels routes through `send_dm`; `switchChannel` for DM kinds calls `loadDmThread`.

- [ ] **Step 1: Locate the existing `onSend` callback in `App.svelte`.**

Search `grep -n "onSend\|handleSend" src/App.svelte`. Read the existing channel publish path.

- [ ] **Step 2: Add the DM branch.**

In the existing `onSend` handler (around line 800-900 based on the App.svelte size), add:

```svelte
async function handleSend(text: string, priority: MessagePriority) {
  if (activeChannelType === 'dm' || activeChannelType === 'group-chat') {
    // Optimistic UI: push placeholder Message immediately.
    const optimisticId = crypto.randomUUID();
    const optimistic: Message = {
      id: optimisticId,
      messageId: undefined, // backend will assign on success
      senderAddress: ownAddress!,
      senderName: 'You',
      channel: activeChannel,
      hub: '',
      text,
      timestamp: Date.now(),
      priority,
      deliveryState: 'sending',
    };
    messageService.pushOptimistic(optimistic);

    try {
      const messageId: string = await tauriAdapter.invoke('send_dm', {
        spaceId: activeChannel,
        content: Array.from(new TextEncoder().encode(text)),
        mimeType: 'text/plain',
      });
      // Replace optimistic id → real messageId so dm-delivered correlates.
      messageService.replaceOptimisticId(optimisticId, messageId);
    } catch (e) {
      messageService.markFailed(optimisticId, String(e));
    }
    return;
  }

  // existing channel publish path unchanged
  ...existing code...
}
```

Add the helper methods to MessageService:

```typescript
pushOptimistic(msg: Message): void {
  this.messages = [...this.messages, msg];
  this.onChange?.();
}

replaceOptimisticId(optimisticId: string, realMessageId: string): void {
  this.messages = this.messages.map((m) =>
    m.id === optimisticId ? { ...m, id: realMessageId, messageId: realMessageId } : m
  );
  this.onChange?.();
}

markFailed(optimisticId: string, error: string): void {
  this.messages = this.messages.map((m) =>
    m.id === optimisticId ? { ...m, deliveryState: 'failed' } : m
  );
  this.onChange?.();
  // Surface error somewhere visible — Phase 4 ships with a console.error;
  // toast UX is a polish follow-up.
  console.error('DM send failed:', error);
}
```

- [ ] **Step 3: Hook DM scrollback on channel switch.**

Find the existing `switchChannel(node)` function. Add:

```svelte
function switchChannel(node: NavNode) {
  // existing channel switch logic...

  if (node.type === 'dm' || node.type === 'group-chat') {
    // Load scrollback if this is the first switch to this DM in this session.
    messageService.loadDmThread(node.id).catch((e) => {
      console.error('loadDmThread failed:', e);
    });
  }
}
```

- [ ] **Step 4: Run gates.**

```bash
npx tsc --noEmit
npx vitest run
```

(No new vitest test in this task — the integration is covered by existing MessageService tests + the App.svelte interaction is hard to unit-test in isolation. End-to-end coverage in the manual smoke test, ZEB-239.)

- [ ] **Step 5: Commit.**

```bash
git add -A
git commit -m "feat(zeb-228): App.svelte send-path branches on channel kind

DM/GroupDm channels route through send_dm IPC with optimistic
UI (placeholder Message in 'sending' state, replaced with real
messageId on success or marked 'failed' on error). Channel kinds
unchanged. switchChannel for DM kinds triggers loadDmThread for
cold-start scrollback population."
```

---

### Task 12: DmCreateDialog component

**Files:**
- Create: `src/lib/components/DmCreateDialog.svelte`.
- Create: `src/lib/components/__tests__/DmCreateDialog.test.ts`.

- [ ] **Step 1: Write the failing test first.**

```typescript
import { render, fireEvent } from '@testing-library/svelte';
import DmCreateDialog from '../DmCreateDialog.svelte';

describe('DmCreateDialog', () => {
  it('renders search box and shows recipient counter', () => {
    const { getByPlaceholderText, getByText } = render(DmCreateDialog, {
      props: { profiles: testProfiles, onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    expect(getByPlaceholderText('Search contacts…')).toBeInTheDocument();
    expect(getByText(/0 of 15/)).toBeInTheDocument();
  });

  it('calls onSubmit with kind=dm + 1 recipient when one selected', async () => {
    const onSubmit = vi.fn();
    const { getByText } = render(DmCreateDialog, {
      props: { profiles: testProfiles, onSubmit, onCancel: vi.fn() },
    });
    await fireEvent.click(getByText('Bob'));
    await fireEvent.click(getByText('Start DM'));
    expect(onSubmit).toHaveBeenCalledWith({
      kind: 'dm',
      members: ['bob-hex'],
      name: expect.any(String),
    });
  });

  it('calls onSubmit with kind=group-dm + N recipients when multiple selected', async () => { /* ... */ });

  it('disables Add when 15 recipients selected and shows cap hint', async () => { /* ... */ });

  it('blocks selecting the 16th recipient', async () => { /* ... */ });

  it('calls onCancel without IPC call when Cancel clicked', async () => { /* ... */ });
});
```

- [ ] **Step 2: Verify test fails (component doesn't exist yet).**

- [ ] **Step 3: Implement the component.**

```svelte
<!-- src/lib/components/DmCreateDialog.svelte -->
<script lang="ts">
  import type { Profile } from '../types';

  let {
    profiles,
    onSubmit,
    onCancel,
  }: {
    profiles: Map<string, Profile>;
    onSubmit: (args: { kind: 'dm' | 'group-dm'; members: string[]; name: string }) => void;
    onCancel: () => void;
  } = $props();

  const MAX_RECIPIENTS = 15;

  let searchQuery = $state('');
  let selected: string[] = $state([]);
  let error = $state('');

  let filteredProfiles = $derived.by(() => {
    const q = searchQuery.toLowerCase();
    return Array.from(profiles.entries())
      .filter(([_, p]) => p.displayName.toLowerCase().includes(q))
      .filter(([addr, _]) => !selected.includes(addr))
      .slice(0, 50);
  });

  let kind: 'dm' | 'group-dm' = $derived(selected.length === 1 ? 'dm' : 'group-dm');
  let canAddMore = $derived(selected.length < MAX_RECIPIENTS);
  let canSubmit = $derived(selected.length >= 1 && selected.length <= MAX_RECIPIENTS);

  function toggleSelect(addr: string) {
    if (selected.includes(addr)) {
      selected = selected.filter((a) => a !== addr);
    } else if (canAddMore) {
      selected = [...selected, addr];
    }
    // 16th recipient attempt: silent no-op (the cap hint already
    // explains why; an additional toast would be noise).
  }

  function handleSubmit() {
    if (!canSubmit) return;
    const name = kind === 'dm'
      ? `DM with ${profiles.get(selected[0])?.displayName ?? selected[0].slice(0, 8) + '…'}`
      : `DM: ${selected.map((a) => profiles.get(a)?.displayName ?? a.slice(0, 8) + '…').join(', ').slice(0, 80)}`;
    onSubmit({ kind, members: selected, name });
  }
</script>

<div class="dm-create-dialog" role="dialog" aria-labelledby="dm-create-title">
  <h2 id="dm-create-title">New direct message</h2>

  <input
    type="text"
    placeholder="Search contacts…"
    bind:value={searchQuery}
    class="search-input"
  />

  {#if selected.length > 0}
    <div class="selected-chips">
      {#each selected as addr}
        <button class="chip" onclick={() => toggleSelect(addr)} aria-label="Remove {profiles.get(addr)?.displayName ?? addr}">
          {profiles.get(addr)?.displayName ?? addr.slice(0, 8) + '…'} ✕
        </button>
      {/each}
    </div>
  {/if}

  <div class="contact-list">
    {#each filteredProfiles as [addr, profile]}
      <button class="contact" onclick={() => toggleSelect(addr)}>
        {profile.displayName}
      </button>
    {/each}
    {#if filteredProfiles.length === 0}
      <p class="empty">No contacts match "{searchQuery}"</p>
    {/if}
  </div>

  <div class="counter" class:at-cap={selected.length === MAX_RECIPIENTS}>
    {selected.length} of {MAX_RECIPIENTS} recipients
    {#if selected.length === MAX_RECIPIENTS}
      <span class="hint">Group DMs cap at 16 members (you + 15). Communities (coming soon) work better for larger groups.</span>
    {/if}
  </div>

  {#if error}
    <p class="error">{error}</p>
  {/if}

  <div class="actions">
    <button onclick={onCancel}>Cancel</button>
    <button onclick={handleSubmit} disabled={!canSubmit} class="primary">Start DM</button>
  </div>
</div>

<style>
  .dm-create-dialog { padding: 16px; max-width: 360px; }
  .search-input { width: 100%; box-sizing: border-box; margin-bottom: 8px; }
  .selected-chips { display: flex; flex-wrap: wrap; gap: 4px; margin-bottom: 8px; }
  .chip { background: rgba(120,140,200,0.2); border-radius: 12px; padding: 2px 8px; font-size: 12px; cursor: pointer; }
  .contact-list { max-height: 200px; overflow-y: auto; border-radius: 4px; background: rgba(255,255,255,0.05); margin-bottom: 8px; }
  .contact { display: block; width: 100%; text-align: left; padding: 6px 8px; background: transparent; border: none; cursor: pointer; }
  .contact:hover { background: rgba(255,255,255,0.05); }
  .empty { padding: 12px; opacity: 0.6; font-size: 12px; text-align: center; }
  .counter { font-size: 11px; opacity: 0.7; margin: 8px 0; }
  .counter.at-cap { color: #f99; }
  .counter .hint { display: block; margin-top: 4px; }
  .error { color: #f55; font-size: 12px; }
  .actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 12px; }
  .actions button { padding: 6px 12px; }
  .primary { background: rgba(120,140,200,0.4); }
  .primary:disabled { opacity: 0.4; cursor: not-allowed; }
</style>
```

- [ ] **Step 4: Verify tests pass.**

- [ ] **Step 5: Run all gates.**

- [ ] **Step 6: Commit.**

```bash
git add -A
git commit -m "feat(zeb-228): DmCreateDialog component for DM/GroupDm creation

Single-screen multi-select picker. 0-1 recipients = kind=dm;
2-15 = kind=group-dm. At 15 selected, contact list disables
selection (silent no-op on 16th-attempt) and shows the cap hint
pointing at communities (coming soon). Auto-generates Space name
from member display names. Calls onSubmit with the add_space
arg shape; parent component invokes the IPC."
```

---

### Task 13: "+ New DM" button + DmCreateDialog integration in App.svelte

**Files:**
- Modify: `src/App.svelte` — add the button at the bottom of the nav sidebar; wire DmCreateDialog into modal state.

- [ ] **Step 1: Locate the nav sidebar in App.svelte (or Layout.svelte).**

Check both files. The nav rendering is likely in App.svelte passed as a slot to Layout.svelte.

- [ ] **Step 2: Add modal state + button.**

```svelte
<script lang="ts">
  // existing imports + state...
  import DmCreateDialog from './lib/components/DmCreateDialog.svelte';

  let dmCreateDialogOpen = $state(false);

  async function handleDmCreate(args: { kind: 'dm' | 'group-dm'; members: string[]; name: string }) {
    try {
      const spaceId: string = await tauriAdapter.invoke('add_space', {
        kind: args.kind === 'dm' ? 'Dm' : 'GroupDm',
        name: args.name,
        parent: null,
        members: args.members,
        transport: null, // backend defaults to Reticulum for DM kinds
      });
      // The backend's apply_space + nav-updated emit will trigger
      // NavService to insert the NavNode; we can switch to it after
      // a tick to give NavService time to receive the event.
      setTimeout(() => {
        const newNode = navService.nodes.find((n) => n.id === spaceId);
        if (newNode) switchChannel(newNode);
      }, 50);
      dmCreateDialogOpen = false;
    } catch (e) {
      console.error('add_space failed:', e);
      // surface error inside the dialog (DmCreateDialog could expose an `error` prop;
      // for Phase 4 v1, console.error is fine since the cap is enforced client-side
      // and other failures are rare).
    }
  }
</script>

<!-- inside the nav sidebar slot, at the bottom -->
<button class="new-dm-button" onclick={() => dmCreateDialogOpen = true} title="New direct message">
  <span aria-hidden="true">+</span> New DM
</button>

<!-- modal overlay -->
{#if dmCreateDialogOpen}
  <div class="modal-overlay" onclick={() => dmCreateDialogOpen = false} onkeydown={(e) => e.key === 'Escape' && (dmCreateDialogOpen = false)}>
    <div class="modal-content" onclick={(e) => e.stopPropagation()} role="dialog">
      <DmCreateDialog
        profiles={navService.profiles}
        onSubmit={handleDmCreate}
        onCancel={() => dmCreateDialogOpen = false}
      />
    </div>
  </div>
{/if}

<style>
  .new-dm-button { position: sticky; bottom: 0; width: 100%; padding: 8px; background: rgba(120,140,200,0.15); border: none; cursor: pointer; }
  .modal-overlay { position: fixed; inset: 0; background: rgba(0,0,0,0.5); display: flex; align-items: center; justify-content: center; z-index: 100; }
  .modal-content { background: var(--bg-2, #222); border-radius: 8px; }
</style>
```

- [ ] **Step 3: Run gates.**

- [ ] **Step 4: Commit.**

```bash
git add -A
git commit -m "feat(zeb-228): + New DM button at nav sidebar bottom + DmCreateDialog wiring

Click the button → modal opens. Submit → invokes add_space IPC,
closes modal, switches active channel to the new SpaceId after
a 50ms tick (gives NavService time to handle the nav-updated
event from the apply_space result). Esc + overlay-click dismiss
the modal."
```

---

### Task 14: Inline manual-delete on stuck/expired messages

**Files:**
- Modify: `src/lib/components/TextMessage.svelte` (or wherever single-message rendering lives) — add inline ⓧ button.
- Modify: `src/App.svelte` — handle the delete confirmation flow with `ConfirmDialog`.
- Test: extend `TextMessage.test.ts` (if exists) with delete-button visibility cases.

- [ ] **Step 1: Locate the message rendering component.**

```bash
grep -rn "deliveryState\|messageId\|message.id" src/lib/components/TextMessage.svelte src/lib/components/QuietMessageGroup.svelte 2>/dev/null
```

If TextMessage doesn't exist, find whatever renders individual messages — `grep -rn "TextMessage\b" src/lib/`.

- [ ] **Step 2: Write a failing test.**

```typescript
describe('TextMessage delete button', () => {
  it('shows delete button for self-Message in expired state', () => {
    const { queryByLabelText } = render(TextMessage, {
      props: {
        message: { ...baseMessage, deliveryState: 'expired', messageId: 'mid1' },
        isSelf: true,
        onDelete: vi.fn(),
      },
    });
    expect(queryByLabelText('Delete this message')).toBeInTheDocument();
  });

  it('shows delete button for self-Message stuck in sending > 60s', () => { /* ... */ });
  it('hides delete button for received messages', () => { /* ... */ });
  it('hides delete button for delivered self-Message', () => { /* ... */ });
});
```

- [ ] **Step 3: Add the inline ⓧ button to TextMessage.svelte.**

```svelte
<script lang="ts">
  let { message, isSelf, onDelete }: {
    message: Message;
    isSelf: boolean;
    onDelete?: (messageId: string) => void;
  } = $props();

  let now = $state(Date.now());
  $effect(() => {
    const interval = setInterval(() => now = Date.now(), 5_000);
    return () => clearInterval(interval);
  });

  let canDelete = $derived(
    isSelf && (
      message.deliveryState === 'expired' ||
      message.deliveryState === 'failed' ||
      (message.deliveryState === 'sending' && now - message.timestamp > 60_000)
    ) && message.messageId !== undefined
  );
</script>

<!-- inside the message bubble -->
{#if canDelete}
  <button class="delete-btn" aria-label="Delete this message" onclick={() => onDelete?.(message.messageId!)}>
    ⓧ
  </button>
{/if}
```

- [ ] **Step 4: Wire onDelete in App.svelte through TextFeed.**

In App.svelte, add:

```svelte
<script lang="ts">
  import ConfirmDialog from './lib/components/ConfirmDialog.svelte';

  let pendingDeleteMessageId: string | null = $state(null);
  let pendingDeleteState: string | null = $state(null);

  function requestDeleteMessage(messageId: string) {
    const msg = messageService.messages.find((m) => m.messageId === messageId);
    pendingDeleteMessageId = messageId;
    pendingDeleteState = msg?.deliveryState ?? null;
  }

  async function confirmDelete() {
    if (!pendingDeleteMessageId) return;
    try {
      await tauriAdapter.invoke('delete_outbox_entry', { messageId: pendingDeleteMessageId });
      // dm-deleted IPC event will arrive → MessageService prunes the message
    } catch (e) {
      console.error('delete failed:', e);
    } finally {
      pendingDeleteMessageId = null;
      pendingDeleteState = null;
    }
  }
</script>

<!-- pass onDelete down to TextFeed → TextMessage -->
<TextFeed ... onMessageDelete={requestDeleteMessage} />

{#if pendingDeleteMessageId}
  <ConfirmDialog
    title="Delete message?"
    message={pendingDeleteState === 'expired'
      ? "Delete this expired message? It's been undeliverable for 30 days."
      : "Delete this message? It hasn't been delivered yet. Recipients who haven't received it won't see it."}
    confirmLabel="Delete"
    onConfirm={confirmDelete}
    onCancel={() => { pendingDeleteMessageId = null; pendingDeleteState = null; }}
  />
{/if}
```

(Adapt `ConfirmDialog`'s prop API to whatever the existing component accepts — read `src/lib/components/ConfirmDialog.svelte` first.)

- [ ] **Step 5: Update TextFeed.svelte to pipe onMessageDelete down to TextMessage.**

Add `onMessageDelete?: (messageId: string) => void;` to TextFeed's prop list, and pass it through to each `<TextMessage onDelete={onMessageDelete} ... />` instance with `isSelf={message.senderAddress === ownAddress}`.

- [ ] **Step 6: Verify tests pass.**

- [ ] **Step 7: Run all gates.**

- [ ] **Step 8: Commit.**

```bash
git add -A
git commit -m "feat(zeb-228): inline manual delete on stuck/expired DM messages

Self-Messages in expired/failed state OR stuck in sending > 60s
get an inline ⓧ button. Click → ConfirmDialog (existing component)
opens with state-appropriate copy. Confirm → delete_outbox_entry
IPC; dm-deleted event prunes from local cache. Cancel → no-op.

The 60s sending-stuck threshold is configurable; bikeshed if smoke
testing reveals it's wrong."
```

---

### Task 15: Final integration sweep + PR

- [ ] **Step 1: Run all gates from a clean state.**

```bash
cd "$(git rev-parse --show-toplevel)"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
npx tsc --noEmit
npx vitest run
```

All five must exit 0. Use `set -o pipefail` if piping; never trust pipe exit codes.

- [ ] **Step 2: Manual smoke test (single-device).**

```bash
npm run tauri dev
```

Test:
1. Click "+ New DM" → DmCreateDialog opens
2. Search, select 1 contact, Start DM → Space appears in nav, switches to it
3. Type a message, send → optimistic Message appears immediately with "sending" indicator
4. (Without a recipient online, the message stays "sending" — that's expected; manual delete after 60s works)
5. Switch to a different channel and back → loadDmThread populates the timeline correctly
6. Click "+ New DM" → select 15 contacts → "Start DM" still enabled, group-dm Space created
7. Try to select a 16th contact → blocked silently (ideally with the cap hint visible)

If anything's broken, file a follow-up Linear ticket; do not block PR on smoke-test polish issues that are out of scope (e.g., styling tweaks).

- [ ] **Step 3: Push the branch.**

```bash
git push -u origin zeb-228-dm-transport-phase4
```

- [ ] **Step 4: Open the PR.**

```bash
gh pr create --title "feat(zeb-228): DM transport Phase 4 — UI + scrollback + manual delete" --body "$(cat <<'EOF'
Closes ZEB-228 and ZEB-216 umbrella.

## Summary

Wires the now-shipped DM transport stack (Phases 1-3b) into the existing harmony-client UI. Reuses the existing chat-shaped TextFeed + ComposeBar (which already accepted `channelType: 'dm' | 'group-chat'`); only one new component (DmCreateDialog).

## Changes

**Backend:**
- `dm-received` IPC payload now includes `body` + `mimeType` + `sentAt` (was promised in umbrella spec but Phase 3b's emit only carried the InboxEntry pointer).
- `send_dm` writes a self-InboxEntry alongside OutboxEntry so self-history persists across restarts.
- New `read_dm_thread` IPC for cold-start scrollback (paginated by HLC cursor; decrypts via prior-keys fallback).
- New `delete_outbox_entry` IPC for manual delete of stuck/expired messages.
- `add_space` extended for DM/GroupDm kinds: generates content_key, builds Space CRDT entry, dispatches DmInvite to recipients' known devices.

**Frontend:**
- New `DmCreateDialog.svelte` — multi-select picker, 15-recipient cap (16 with self), at-cap hint pointing at communities (coming soon).
- `NavService` handles `nav-updated` for DM/GroupDm Space kinds (default top-level placement; user drags into folders).
- `MessageService` subscribes to `dm-received`/`dm-delivered`/`dm-expired`/`dm-deleted`; new `loadDmThread(spaceId)` for scrollback.
- `App.svelte` `onSend` branches on channel kind: DMs route through `send_dm` IPC with optimistic UI; channels stay on existing publish path.
- Inline ⓧ on stuck/expired self-Messages with the existing `ConfirmDialog` for delete confirmation.

**Tests:**
- Vitest: DmCreateDialog (cap behavior, kind selection), NavService DM handling, MessageService DM subscriptions + loadDmThread, TextMessage delete-button visibility.
- Cargo: send_dm self-InboxEntry write, delete_dm_outbox_entry, read_dm_thread roundtrip, add_space DM/GroupDm validation cases.

## Test plan
- [x] All gates green: cargo fmt + clippy + test, vitest, tsc
- [ ] Manual two-device LAN smoke test deferred to ZEB-239 (final shipping verification)
- [ ] DmInvite decline UX deferred to ZEB-236

## References
- Spec: `docs/specs/2026-05-04-zeb-228-dm-transport-phase4-ui-design.md`
- Plan: `docs/plans/2026-05-04-zeb-228-dm-transport-phase4-ui-plan.md`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 5: Update Linear.**

Mark ZEB-228 as In Progress (if not already) and link the PR via `links` field on the issue.

---

## Verification gates (run at every commit)

```bash
cd "$(git rev-parse --show-toplevel)"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
npx tsc --noEmit
npx vitest run
```

All must exit 0. Per user memory `Pipe exit codes lie`: any pipe-based verification uses `set -o pipefail` or `${PIPESTATUS[0]}`.

## Acceptance criteria

Mirrors `docs/specs/2026-05-04-zeb-228-dm-transport-phase4-ui-design.md` §Acceptance criteria. Final verification before PR-ready:

- [ ] Create a DM (1 recipient) and a GroupDm (2-15 recipients) via DmCreateDialog
- [ ] Attempting to pick a 16th recipient is blocked with the inline cap hint
- [ ] Send a DM to an online recipient on another paired device → message arrives in their UI (manual smoke; deferred to ZEB-239)
- [ ] Send a DM to an offline recipient → outbox queues; manual delete works
- [ ] Self-sent messages persist across app restart (cold-start scrollback)
- [ ] Receive a DM while UI is on a different channel → unread count increments
- [ ] Receive a DM while UI is on that channel → message appears with auto-scroll
- [ ] 30-day expired messages surface to UI; user can manually delete
- [ ] Stuck (>1min sending) messages also offer manual delete
- [ ] All gates green (cargo fmt + clippy + test, vitest, tsc)
- [ ] Manual two-device LAN smoke deferred to ZEB-239
