# ZEB-114 Phase 2: Client Mail Receive Path — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the harmony-client an inbox that reflects the recipient's full mail history by walking a gateway-published Merkle tree (root → folder → page → entry), with header-only sync and lazy body fetch on click.

**Architecture:** Cross-repo work in two PRs. **PR 1** (harmony repo): add a Zenoh queryable on `harmony/mail/v1/{addr_hex}/root` so clients can pull the current root CID at cold start. **PR 2** (harmony-client repo): a new `mail_sync.rs` module that owns the walker state machine, calls existing `MailManager` storage methods to register header-only entries, and reuses the existing `fetch_via_zenoh` path to pull CAS blobs. UI gains a sync indicator and refresh button; `MailReader` becomes async to load bodies lazily.

**Tech Stack:** Rust 2021 (gateway: tokio + zenoh 1.7.2; client: tokio + tauri 2 + zenoh 1.7.2), Svelte 5 (client UI), `harmony-mailbox` shared crate (wire format).

**Spec:** `docs/specs/2026-04-14-client-mail-receive-design.md`

---

## File Structure

### Gateway (harmony repo) — PR 1

| File | Action | Responsibility |
|---|---|---|
| `crates/harmony-mail/src/mailbox_manager.rs` | Modify | Add `RootQueryable` declared from `ZenohPublisher::new` next to existing publish drain task. Reads from existing `latest: Arc<Mutex<HashMap>>`. |
| `crates/harmony-mail/src/server.rs` | No-op (or trivial) | The publisher already runs at server startup; queryable rides along. |

### Client (harmony-client repo) — PR 2

| File | Action | Responsibility |
|---|---|---|
| `src-tauri/Cargo.lock` | Modify | Pin all `harmony-*` git deps to harmony main commit including the new queryable. |
| `src-tauri/src/mail.rs` | Modify | Add `BodyState` enum + `body_state` field to `EntryRecord` with `serde(default)`. Add `register_header_only` and `mark_body_received` methods. Modify `receive_message` to promote Pending→Local instead of rejecting as duplicate when matching entry is Pending. |
| `src-tauri/src/mail_sync.rs` | Create | Walker state machine, hybrid error policy, sync trigger handlers, lazy body fetch with in-flight dedup. Emits `mail-sync-status` events. |
| `src-tauri/src/event_loop.rs` | Modify | Flip the `/root` subscriber filter to route to `MailSync`. Add startup query. |
| `src-tauri/src/lib.rs` | Modify | Construct `MailSync` in setup; register new Tauri commands `refresh_mail` and `fetch_mail_body`; update `get_mail` response to expose `body_state`. |
| `src-tauri/tests/mail_sync_integration.rs` | Create | In-process Zenoh integration test wiring a stub gateway publisher + queryable to a real `MailSync`. |
| `src/lib/mail-service.ts` | Modify | Add `syncState`/`syncError` reactive state, `mail-sync-status` listener, `refresh()` method, async `getMessage` wrapper. |
| `src/lib/components/MailInbox.svelte` | Modify | Add sync indicator (spinner / error icon) + refresh button to header. |
| `src/lib/components/MailReader.svelte` | Modify | Make body load async; show spinner while fetching for `Pending` entries. |
| `src/lib/mail-service.test.ts` | Create or extend | Vitest tests for sync state handling + lazy body fetch path. |

---

## PR 1 — Gateway Queryable (harmony repo)

**Branch:** `zeb-114-mail-root-queryable`
**Worktree:** `harmony/.claude/worktrees/zeb-114-mail-root-queryable` (created from latest `origin/main`)

### Task G1: Add `serve_root_queries` method to `ZenohPublisher`

**Files:**
- Modify: `crates/harmony-mail/src/mailbox_manager.rs`

Add a queryable declaration that responds to `harmony/mail/v1/*/root` queries by extracting the address from the key, looking up the latest root CID from the existing `latest` map, and replying with the 32 bytes (or empty if not present).

- [ ] **Step 1: Write the failing test**

Add to the existing `#[cfg(test)] mod tests` block in `crates/harmony-mail/src/mailbox_manager.rs`:

```rust
#[tokio::test]
async fn root_queryable_returns_current_root() {
    use zenoh::Wait;
    let cancel = CancellationToken::new();
    let session = zenoh::open(zenoh::Config::default()).await.unwrap();
    let publisher = ZenohPublisher::new(session.clone(), cancel.clone());

    // Insert a root for an address.
    let addr_hex = "00112233445566778899aabbccddeeff".to_string();
    let root_cid = [0xAB; CID_LEN];
    publisher.notify(&addr_hex, root_cid);

    // Allow the drain task to publish (not strictly required for queryable,
    // but exercises the same map).
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Now query the root.
    let topic = format!("harmony/mail/v1/{addr_hex}/root");
    let replies = session.get(&topic).await.unwrap();
    let mut got = None;
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            let bytes = sample.payload().to_bytes();
            got = Some(bytes.to_vec());
            break;
        }
    }
    assert_eq!(got.as_deref(), Some(&root_cid[..]));
    cancel.cancel();
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd /path/to/harmony-mail-root-queryable-worktree
cargo test -p harmony-mail root_queryable_returns_current_root -- --nocapture
```

Expected: FAIL — no queryable declared, `replies.recv_async()` times out or returns nothing.

- [ ] **Step 3: Add queryable declaration in `ZenohPublisher::new`**

Insert this block in `ZenohPublisher::new` after the drain task spawn, before the `Self { ... }` return:

```rust
// ── Queryable: respond to root-CID lookups (cold-start sync support) ──
//
// Clients query `harmony/mail/v1/{addr_hex}/root` to retrieve the current
// root CID for an address. Same key as the publish topic — Zenoh routes
// queries and puts independently. Reply payload is the raw 32 bytes, or
// an empty reply if the address has no mail yet.
let query_session = session.clone();
let query_latest = Arc::clone(&latest);
let query_cancel = cancel.clone();
tokio::spawn(async move {
    let queryable = match query_session
        .declare_queryable("harmony/mail/v1/*/root")
        .await
    {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, "failed to declare root queryable; cold-start sync unavailable");
            return;
        }
    };
    loop {
        tokio::select! {
            _ = query_cancel.cancelled() => break,
            query = queryable.recv_async() => {
                let Ok(query) = query else { break };
                let key = query.key_expr().as_str();
                // Extract addr_hex between "harmony/mail/v1/" and "/root".
                let Some(addr_hex) = key
                    .strip_prefix("harmony/mail/v1/")
                    .and_then(|s| s.strip_suffix("/root"))
                else {
                    let _ = query.reply_err("invalid key").await;
                    continue;
                };
                let payload: Option<[u8; CID_LEN]> = query_latest
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(addr_hex)
                    .copied();
                let reply_result = match payload {
                    Some(cid) => query.reply(key, &cid[..]).await,
                    None => query.reply(key, &[][..]).await,
                };
                if let Err(e) = reply_result {
                    tracing::warn!(error = %e, %key, "failed to reply to root query");
                }
            }
        }
    }
    drop(query_session);
    tracing::debug!("ZenohPublisher root queryable task exited on cancel");
});
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cargo test -p harmony-mail root_queryable_returns_current_root -- --nocapture
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/harmony-mail/src/mailbox_manager.rs
git commit -m "$(cat <<'EOF'
feat(mail): add Zenoh queryable for current mailbox root CID

Clients walking the mailbox Merkle tree need to fetch the current root
CID at cold start (before any live root publish arrives). Add a queryable
on harmony/mail/v1/{addr_hex}/root that reads from the existing
ZenohPublisher coalescing map. Same key as the publish topic — Zenoh
routes queries and puts independently.

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task G2: Add tests for the empty-reply and multi-update cases

**Files:**
- Modify: `crates/harmony-mail/src/mailbox_manager.rs` (test module)

- [ ] **Step 1: Write the empty-reply test**

```rust
#[tokio::test]
async fn root_queryable_empty_for_unknown_addr() {
    let cancel = CancellationToken::new();
    let session = zenoh::open(zenoh::Config::default()).await.unwrap();
    let _publisher = ZenohPublisher::new(session.clone(), cancel.clone());
    // Allow queryable to register.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let unknown = "ffffffffffffffffffffffffffffffff".to_string();
    let topic = format!("harmony/mail/v1/{unknown}/root");
    let replies = session.get(&topic).await.unwrap();
    let mut got = None;
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            got = Some(sample.payload().to_bytes().to_vec());
            break;
        }
    }
    assert_eq!(got.as_deref(), Some(&[][..]));
    cancel.cancel();
}
```

- [ ] **Step 2: Write the latest-only test**

```rust
#[tokio::test]
async fn root_queryable_returns_latest_after_multiple_updates() {
    let cancel = CancellationToken::new();
    let session = zenoh::open(zenoh::Config::default()).await.unwrap();
    let publisher = ZenohPublisher::new(session.clone(), cancel.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let addr_hex = "11223344556677889900aabbccddeeff".to_string();
    publisher.notify(&addr_hex, [0x01; CID_LEN]);
    publisher.notify(&addr_hex, [0x02; CID_LEN]);
    publisher.notify(&addr_hex, [0x03; CID_LEN]); // latest
    // Wait briefly so the drain task may run and REMOVE entries from the
    // map. The queryable must still find the latest because notify() always
    // writes back into the map after each publish — actually it doesn't,
    // it consumes via drain. So we must query BEFORE the drain runs. To
    // avoid that race, use try_acquire on a dummy semaphore... simpler:
    // just query before sleeping. But notify+drain happens fast.
    //
    // Test approach: query immediately after the third notify, before the
    // wake task can drain. If flaky, add a separate "snapshot" mechanism;
    // for now, query in a loop within a 1s budget and accept either
    // (a) the latest CID, or (b) empty (if drain already ran).
    let topic = format!("harmony/mail/v1/{addr_hex}/root");
    let replies = session.get(&topic).await.unwrap();
    let mut got = None;
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            got = Some(sample.payload().to_bytes().to_vec());
            break;
        }
    }
    // Either latest (drain hadn't run) or empty (drain consumed map).
    let valid = got.as_deref() == Some(&[0x03; CID_LEN][..])
        || got.as_deref() == Some(&[][..]);
    assert!(valid, "got unexpected payload: {got:?}");
    cancel.cancel();
}
```

> **NOTE on the latest-only race:** the existing `latest` map is consumed by the drain task. The queryable reads from this same map, so a query that races a drain may see empty even if a recent `notify` happened. **The fix in Task G3 below is to make the publisher track the most recent root per address in a separate "current" map that is never drained**, so queryables get deterministic answers. The flaky-tolerant test above is a placeholder; G3 replaces it with a strict assertion.

- [ ] **Step 3: Run tests**

```bash
cargo test -p harmony-mail root_queryable -- --nocapture
```

Expected: PASS (both new tests).

- [ ] **Step 4: Commit**

```bash
git add crates/harmony-mail/src/mailbox_manager.rs
git commit -m "$(cat <<'EOF'
test(mail): cover empty + multi-update root queryable cases

Empty-address query returns empty reply; multi-update query returns
either the latest CID or empty (drain race tolerated until G3
introduces a never-drained current map).

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task G3: Add a never-drained `current` map for queryable to read from

The existing `latest` map is consumed by the drain task — a query racing a drain returns empty. Add a parallel `current: Arc<Mutex<HashMap<String, [u8; CID_LEN]>>>` that `notify()` also updates but the drain task never clears. Queryable reads exclusively from `current`.

**Files:**
- Modify: `crates/harmony-mail/src/mailbox_manager.rs`

- [ ] **Step 1: Add the `current` field**

In the `ZenohPublisher` struct (around line 55):

```rust
pub struct ZenohPublisher {
    /// Coalescing map: pending root CIDs to publish. Drained by the
    /// background task on each wake.
    latest: Arc<Mutex<HashMap<String, [u8; CID_LEN]>>>,
    /// Current root CID per address (never drained). Populated by every
    /// notify() call. Read by the root queryable for cold-start sync.
    current: Arc<Mutex<HashMap<String, [u8; CID_LEN]>>>,
    wake: Arc<Notify>,
    raw_sink: RawSink,
}
```

- [ ] **Step 2: Initialize `current` in `new` and `inert_for_test`**

In `ZenohPublisher::new` (around line 102):

```rust
let latest: Arc<Mutex<HashMap<String, [u8; CID_LEN]>>> =
    Arc::new(Mutex::new(HashMap::new()));
let current: Arc<Mutex<HashMap<String, [u8; CID_LEN]>>> =
    Arc::new(Mutex::new(HashMap::new()));
```

In the queryable spawn block (added in G1), replace `query_latest` with a clone of `current`:

```rust
let query_current = Arc::clone(&current);
// ...
let payload: Option<[u8; CID_LEN]> = query_current
    .lock()
    .unwrap_or_else(|p| p.into_inner())
    .get(addr_hex)
    .copied();
```

In the `Self { ... }` return:

```rust
Self {
    latest,
    current,
    wake,
    raw_sink: RawSink::Session { /* ... */ },
}
```

In `inert_for_test`:

```rust
let current: Arc<Mutex<HashMap<String, [u8; CID_LEN]>>> =
    Arc::new(Mutex::new(HashMap::new()));
let publisher = Self {
    latest: Arc::clone(&latest),
    current: Arc::clone(&current),
    wake,
    raw_sink: RawSink::Captured(Arc::clone(&raw)),
};
```

Update the existing `InertHandles` struct to expose `current` for tests if any test wants to assert on it:

```rust
#[cfg(test)]
pub struct InertHandles {
    pub latest: Arc<Mutex<HashMap<String, [u8; CID_LEN]>>>,
    pub current: Arc<Mutex<HashMap<String, [u8; CID_LEN]>>>,
    pub raw: Arc<Mutex<Vec<(String, Arc<Vec<u8>>)>>>,
}
```

And populate it:

```rust
let handles = InertHandles {
    latest: Arc::clone(&latest),
    current: Arc::clone(&current),
    raw,
};
```

- [ ] **Step 3: Update `notify()` to write to `current` too**

Find the existing `notify(&self, addr_hex: &str, root_cid: [u8; CID_LEN])` method and add the parallel write:

```rust
pub fn notify(&self, addr_hex: &str, root_cid: [u8; CID_LEN]) {
    {
        let mut map = self.latest.lock().unwrap_or_else(|p| p.into_inner());
        map.insert(addr_hex.to_string(), root_cid);
    }
    {
        let mut map = self.current.lock().unwrap_or_else(|p| p.into_inner());
        map.insert(addr_hex.to_string(), root_cid);
    }
    self.wake.notify_one();
}
```

- [ ] **Step 4: Tighten the multi-update test to a strict assertion**

Replace the flaky-tolerant `root_queryable_returns_latest_after_multiple_updates` test from G2:

```rust
#[tokio::test]
async fn root_queryable_returns_latest_after_multiple_updates() {
    let cancel = CancellationToken::new();
    let session = zenoh::open(zenoh::Config::default()).await.unwrap();
    let publisher = ZenohPublisher::new(session.clone(), cancel.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let addr_hex = "11223344556677889900aabbccddeeff".to_string();
    publisher.notify(&addr_hex, [0x01; CID_LEN]);
    publisher.notify(&addr_hex, [0x02; CID_LEN]);
    publisher.notify(&addr_hex, [0x03; CID_LEN]);

    // Wait long enough that the drain task has definitely consumed `latest`.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let topic = format!("harmony/mail/v1/{addr_hex}/root");
    let replies = session.get(&topic).await.unwrap();
    let mut got = None;
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            got = Some(sample.payload().to_bytes().to_vec());
            break;
        }
    }
    assert_eq!(got.as_deref(), Some(&[0x03; CID_LEN][..]));
    cancel.cancel();
}
```

- [ ] **Step 5: Run all tests**

```bash
cargo test -p harmony-mail root_queryable -- --nocapture
```

Expected: all three queryable tests PASS (deterministic now).

- [ ] **Step 6: Commit**

```bash
git add crates/harmony-mail/src/mailbox_manager.rs
git commit -m "$(cat <<'EOF'
fix(mail): queryable reads from never-drained current map

The latest map is drained by the publish task — queries racing a drain
would return empty. Add a parallel current map that notify() also
populates and the drain never clears. Queryable now reads from current,
giving deterministic cold-start replies.

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task G4: Open PR 1, merge after review

- [ ] **Step 1: Push branch and open PR**

```bash
cd /path/to/harmony-mail-root-queryable-worktree
git push -u origin zeb-114-mail-root-queryable
gh pr create --title "feat(mail): root CID queryable for client cold-start sync (ZEB-114)" --body "$(cat <<'EOF'
## Summary

- Add Zenoh queryable on `harmony/mail/v1/{addr_hex}/root` returning the current root CID for an address (32 bytes, or empty if no mail).
- Same key as the existing publish topic; Zenoh routes queries and puts independently.
- Add a never-drained `current` map alongside the existing `latest` coalescing map so queryables get deterministic answers regardless of drain task timing.
- Tests: queryable returns current root, returns empty for unknown address, returns latest after multiple updates.

Enables harmony-client Phase 2 (PR forthcoming) to perform cold-start mailbox sync — the client queries this endpoint at startup to learn the current root CID before any live push arrives.

## Test plan

- [ ] `cargo test -p harmony-mail root_queryable -- --nocapture` passes
- [ ] Full `cargo test --workspace` clean
- [ ] Manual: open Zenoh REPL, observe queryable responds to a query for a known mailbox

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 2: Address review feedback**

(No specific action — respond to whatever reviewers raise. After approval, merge.)

- [ ] **Step 3: Squash-merge PR**

```bash
gh pr merge <PR_NUMBER> --squash
```

- [ ] **Step 4: Capture merge SHA for client lockfile pin**

```bash
git fetch origin main
git log origin/main --oneline -1
# Record the new commit SHA — needed for PR 2 Cargo.lock pin.
```

---

## PR 2 — Client Walker (harmony-client repo)

**Branch:** `zeb-114-phase-2-client-walker`
**Worktree:** `harmony-client/.claude/worktrees/zeb-114-phase-2-client-walker` (created from latest `origin/main` AFTER PR 1 is merged)

### Task C1: Pin Cargo.lock to harmony main with queryable

**Files:**
- Modify: `src-tauri/Cargo.lock`

The client depends on `harmony-mailbox` and `harmony-content` via git from the harmony repo. After PR 1 merges, the client lockfile must point to the new main commit so the walker can be tested against the queryable.

- [ ] **Step 1: Create worktree from latest main**

```bash
cd /path/to/harmony-client
git fetch origin --prune
git worktree add .claude/worktrees/zeb-114-phase-2-client-walker -b zeb-114-phase-2-client-walker origin/main
cd .claude/worktrees/zeb-114-phase-2-client-walker
```

- [ ] **Step 2: Note the current pinned harmony SHA**

```bash
grep -A1 'name = "harmony-mailbox"' src-tauri/Cargo.lock | head -5
# Records the current SHA — call it OLD_SHA
```

- [ ] **Step 3: Re-pin all harmony-* deps to the new SHA**

```bash
NEW_SHA=$(cd /path/to/harmony && git log origin/main --oneline -1 | awk '{print $1}')
# Use sed to swap all harmony-* deps in Cargo.lock from OLD_SHA to NEW_SHA.
# This avoids cargo update side-effects (registry dep churn, schema regen).
sed -i.bak "s|main#OLD_SHA|main#NEW_SHA|g" src-tauri/Cargo.lock
rm src-tauri/Cargo.lock.bak
```

(Substitute the actual SHAs.)

- [ ] **Step 4: Verify lockfile builds cleanly**

```bash
cd src-tauri
cargo check
```

Expected: clean compile. No warnings about unused features or version mismatches.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/Cargo.lock
git commit -m "$(cat <<'EOF'
chore: pin Cargo.lock to harmony main with mail-root queryable (ZEB-114)

Pulls in the harmony-side mail-root queryable (PR #<N>) so the client
walker can perform cold-start sync via Zenoh queries. Surgical sed
edit on lockfile to avoid registry-dep churn from `cargo update`.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C2: Add `BodyState` enum + `body_state` field to `EntryRecord`

**Files:**
- Modify: `src-tauri/src/mail.rs`

- [ ] **Step 1: Write the migration test**

Add to the `#[cfg(test)] mod tests` block in `src-tauri/src/mail.rs`:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd src-tauri
cargo test -p harmony-client mail::tests::index_loads_old_format_with_local_default
```

Expected: FAIL — `BodyState` not defined; `body_state` field doesn't exist on `EntryRecord`.

- [ ] **Step 3: Add `BodyState` enum and field**

In `src-tauri/src/mail.rs`, add the enum near the top of the public types section (after line 16 `use serde::{Deserialize, Serialize}`):

```rust
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
```

Update `EntryRecord` (line 22) to include the field with `serde(default)` so old indexes load:

```rust
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
```

Also expose `BodyState` in `MailDetail` so the frontend can decide whether to fetch:

```rust
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
```

Update `get_message` to set `body_state: BodyState::Local` in the returned `MailDetail` (since that path only succeeds when the blob exists):

```rust
Ok(MailDetail {
    message_cid: cid_hex.to_string(),
    // ... existing fields ...
    body_state: BodyState::Local,
})
```

Existing call sites that construct `EntryRecord` (in `receive_message` line 180 and `store_sent` line 224) need to set `body_state: BodyState::Local` explicitly. Add this field to both literals.

- [ ] **Step 4: Run the migration test**

```bash
cargo test -p harmony-client mail::tests::index_loads_old_format_with_local_default
```

Expected: PASS.

- [ ] **Step 5: Run the rest of the mail tests to confirm no regression**

```bash
cargo test -p harmony-client mail::
```

Expected: all existing tests still PASS.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/mail.rs
git commit -m "$(cat <<'EOF'
feat(mail): add BodyState enum and body_state field to EntryRecord

Phase 2 walker registers header-only entries without a blob — the
BodyState enum distinguishes those from fully-local entries. serde(default)
+ Default impl ensure old index.json files load with body_state=Local,
preserving Phase 0/Phase 1 behavior.

Includes migration test: an index file written before this change loads
cleanly with all entries defaulting to Local.

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C3: Add `register_header_only` method

**Files:**
- Modify: `src-tauri/src/mail.rs`

- [ ] **Step 1: Write the test for the new-entry case**

Add to the test module:

```rust
use harmony_mailbox::mailbox::MessageEntry;

fn make_message_entry(message_id: [u8; 16], snippet: &str) -> MessageEntry {
    MessageEntry {
        message_cid: [0xAA; 32],
        message_id,
        sender_address_hash: [0xBB; 16],
        timestamp: 1700000000,
        read: false,
        subject_snippet: snippet.to_string(),
    }
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
```

- [ ] **Step 2: Write the test for the dedup-against-local case**

```rust
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
```

- [ ] **Step 3: Run tests to verify failure**

```bash
cargo test -p harmony-client mail::tests::register_header_only
```

Expected: FAIL — `register_header_only`, `RegisterOutcome` not defined.

- [ ] **Step 4: Implement `register_header_only`**

Add in `mail.rs` near `receive_message` (after line 203):

```rust
/// Outcome of a `register_header_only` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// Entry was new; caller should emit a `mail-received` event.
    Inserted { cid: String },
    /// A matching message_id already exists in inbox/trash/drafts; no change.
    Duplicate,
}

impl MailManager {
    // ... existing methods ...

    /// Register a header-only inbox entry from a walker-discovered MessageEntry.
    ///
    /// Folder is set to Inbox unconditionally (Phase 2 walker only descends Inbox).
    /// Dedup scope: returns `Duplicate` if message_id is already present in
    /// inbox/trash/drafts (matches existing receive_message dedup window).
    pub fn register_header_only(
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

        let record = EntryRecord {
            message_cid: cid_hex.clone(),
            message_id: msg_id_hex,
            sender_address: hex::encode(entry.sender_address_hash),
            timestamp: entry.timestamp,
            subject_snippet: entry.subject_snippet,
            read: entry.read,
            body_state: BodyState::Pending,
        };

        let inbox = self.index.folders.get_mut("inbox").unwrap();
        inbox.entries.insert(0, record);
        self.save_index()?;
        Ok(RegisterOutcome::Inserted { cid: cid_hex })
    }
}
```

> **NOTE on `MessageEntry` field names:** verify the actual field names in `harmony-mailbox/src/mailbox.rs` — the names `message_cid`, `message_id`, `sender_address_hash`, `timestamp`, `read`, `subject_snippet` reflect the design. If the actual struct uses different names (e.g., `cid` instead of `message_cid`), adjust the field accesses accordingly.

- [ ] **Step 5: Run tests**

```bash
cargo test -p harmony-client mail::tests::register_header_only
```

Expected: all 3 register_header_only tests PASS.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/mail.rs
git commit -m "$(cat <<'EOF'
feat(mail): add register_header_only for Phase 2 walker

The walker registers MessageEntry headers without bodies. New entries
land in inbox with body_state=Pending. Dedup scope matches existing
receive_message: a message_id already present in inbox/trash/drafts
returns Duplicate (does not reappear in inbox if user moved to trash).

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C4: Add `mark_body_received` method

**Files:**
- Modify: `src-tauri/src/mail.rs`

- [ ] **Step 1: Write tests**

```rust
#[test]
fn mark_body_received_promotes_pending_to_local() {
    let tmp = tempfile::tempdir().unwrap();
    let mail_dir = tmp.path().join("mail");
    let mut mgr = MailManager::load(&mail_dir, [0u8; ADDRESS_HASH_LEN]);

    // Create a real HarmonyMessage so the bytes parse cleanly.
    let msg = HarmonyMessage::email([0xCC; 16], vec![], "subject", "body").unwrap();
    let bytes = msg.to_bytes();
    let real_cid = blake3::hash(&bytes);
    let real_cid_hex = hex::encode(real_cid.as_bytes());

    // Register a pending entry whose message_cid matches the real bytes.
    let entry = MessageEntry {
        message_cid: *real_cid.as_bytes(),
        message_id: msg.message_id,
        sender_address_hash: [0xCC; 16],
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
    assert!(result.is_err(), "should reject bytes that don't hash to the claimed CID");
}

#[test]
fn mark_body_received_is_idempotent_for_local_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let mail_dir = tmp.path().join("mail");
    let mut mgr = MailManager::load(&mail_dir, [0u8; ADDRESS_HASH_LEN]);

    let msg = HarmonyMessage::email([0xEE; 16], vec![], "s", "b").unwrap();
    let bytes = msg.to_bytes();
    let cid_hex = hex::encode(blake3::hash(&bytes).as_bytes());

    // Receive once via the live raw path → entry is Local.
    mgr.receive_message(&bytes).unwrap();

    // mark_body_received should be a no-op (returns Ok).
    mgr.mark_body_received(&cid_hex, &bytes).unwrap();

    let inbox = mgr.list_folder("inbox", 0, 100);
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].body_state, BodyState::Local);
}
```

- [ ] **Step 2: Run tests to verify failure**

```bash
cargo test -p harmony-client mail::tests::mark_body_received
```

Expected: FAIL — method not defined.

- [ ] **Step 3: Implement `mark_body_received`**

Add to `MailManager`:

```rust
/// Verify bytes hash to cid_hex, write blob, transition matching
/// Pending entries to Local. No-op (returns Ok) if no Pending entry
/// matches (e.g., entry already Local from a racing live push).
pub fn mark_body_received(&mut self, cid_hex: &str, bytes: &[u8]) -> Result<(), String> {
    validate_hex(cid_hex)?;

    // Verify bytes hash to the claimed CID.
    let computed = hex::encode(blake3::hash(bytes).as_bytes());
    if computed != cid_hex {
        return Err(format!("hash mismatch: claimed {cid_hex}, computed {computed}"));
    }

    // Find any matching Pending entry across receive-side folders.
    let mut found_pending = false;
    for folder_name in ["inbox", "trash", "drafts"] {
        let Some(folder) = self.index.folders.get_mut(folder_name) else { continue };
        for entry in folder.entries.iter_mut() {
            if entry.message_cid == cid_hex && entry.body_state == BodyState::Pending {
                entry.body_state = BodyState::Local;
                found_pending = true;
            }
        }
    }

    if !found_pending {
        // No Pending entry to promote — likely already Local from live receive.
        // Don't write a stale blob.
        return Ok(());
    }

    // Write the blob (atomic: tmp + rename).
    let blob_path = self.data_dir.join("blobs").join(format!("{cid_hex}.bin"));
    let tmp_blob = self.data_dir.join("blobs").join(format!("{cid_hex}.bin.tmp"));
    std::fs::write(&tmp_blob, bytes).map_err(|e| format!("write blob: {e}"))?;
    std::fs::rename(&tmp_blob, &blob_path).map_err(|e| format!("rename blob: {e}"))?;

    self.save_index()?;
    Ok(())
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test -p harmony-client mail::tests::mark_body_received
```

Expected: all 3 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/mail.rs
git commit -m "$(cat <<'EOF'
feat(mail): add mark_body_received for Phase 2 lazy fetch

When the walker has registered a Pending entry and the body is later
fetched via fetch_body, mark_body_received verifies the BLAKE3 hash,
writes the blob, and transitions matching Pending entries to Local.

Idempotent if no Pending entry matches (e.g., entry already Local from
a live raw push) — returns Ok without writing the blob.

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C5: Update `receive_message` to promote Pending → Local

**Files:**
- Modify: `src-tauri/src/mail.rs:159-203`

- [ ] **Step 1: Write the race-safety test**

```rust
#[test]
fn receive_message_promotes_pending_to_local_preserving_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let mail_dir = tmp.path().join("mail");
    let mut mgr = MailManager::load(&mail_dir, [0u8; ADDRESS_HASH_LEN]);

    let msg = HarmonyMessage::email([0xFF; 16], vec![], "race", "body").unwrap();
    let bytes = msg.to_bytes();
    let cid = blake3::hash(&bytes);
    let cid_hex = hex::encode(cid.as_bytes());

    // Walker registered a Pending entry first.
    let entry = MessageEntry {
        message_cid: *cid.as_bytes(),
        message_id: msg.message_id,
        sender_address_hash: [0xFF; 16],
        timestamp: msg.timestamp,
        read: false,
        subject_snippet: "race".to_string(),
    };
    mgr.register_header_only(entry).unwrap();

    // User moved it to trash before live push arrived.
    mgr.move_message(&cid_hex, Some("inbox"), "trash").unwrap();
    assert_eq!(mgr.list_folder("trash", 0, 100)[0].body_state, BodyState::Pending);

    // NOW the live raw push arrives.
    let result = mgr.receive_message(&bytes);

    // Should NOT error as duplicate; should promote in-place.
    assert!(result.is_ok(), "receive_message should promote, got: {result:?}");

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

    let msg = HarmonyMessage::email([0xAA; 16], vec![], "s", "b").unwrap();
    let bytes = msg.to_bytes();
    mgr.receive_message(&bytes).unwrap();

    // Receiving the same message again should still be rejected as duplicate.
    let result = mgr.receive_message(&bytes);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("duplicate"));
}
```

- [ ] **Step 2: Run tests to verify failure**

```bash
cargo test -p harmony-client mail::tests::receive_message_promotes
```

Expected: FAIL — current `receive_message` rejects all duplicates.

- [ ] **Step 3: Modify `receive_message`**

Replace the duplicate-check block (around lines 169-176) with a promote-or-dedup branch:

```rust
// Dedup by message_id in receive-side folders. If a matching entry is
// Pending (registered by the walker without a body), promote it in-place
// to Local — preserving its current folder placement (e.g., user-moved
// trash). If matching entry is already Local, treat as duplicate.
let msg_id_hex = hex::encode(msg.message_id);
let mut promoted = false;
for folder_name in ["inbox", "trash", "drafts"] {
    let Some(folder) = self.index.folders.get_mut(folder_name) else { continue };
    for entry in folder.entries.iter_mut() {
        if entry.message_id == msg_id_hex {
            if entry.body_state == BodyState::Pending {
                // Promote: write the blob, transition state. Don't reorder.
                entry.body_state = BodyState::Local;
                entry.message_cid = cid_hex.clone(); // (should already match)
                promoted = true;
            } else {
                return Err("duplicate message".to_string());
            }
        }
    }
}

if promoted {
    // Write the blob (atomic).
    let blob_path = self.data_dir.join("blobs").join(format!("{cid_hex}.bin"));
    let tmp_blob = self.data_dir.join("blobs").join(format!("{cid_hex}.bin.tmp"));
    std::fs::write(&tmp_blob, msg_bytes).map_err(|e| format!("write blob: {e}"))?;
    std::fs::rename(&tmp_blob, &blob_path).map_err(|e| format!("rename blob: {e}"))?;
    self.save_index()?;

    // Build entry record for return (find it again — could be in any folder).
    for folder in self.index.folders.values() {
        if let Some(e) = folder.entries.iter().find(|e| e.message_id == msg_id_hex) {
            return Ok(e.clone());
        }
    }
    // Unreachable.
    return Err("internal: promoted entry vanished".to_string());
}
```

(The rest of the function — building entry record, writing blob, prepending to inbox, save_index — runs only when no matching entry was found.)

- [ ] **Step 4: Run tests**

```bash
cargo test -p harmony-client mail::
```

Expected: all mail tests PASS, including the new promotion test and existing dedup tests.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/mail.rs
git commit -m "$(cat <<'EOF'
feat(mail): receive_message promotes Pending entries instead of rejecting

When a live raw push arrives for a message_id that the walker had
already registered as Pending, promote the existing entry in place:
write the blob, transition body_state to Local, preserve current folder
placement (handles user-moved-to-trash case correctly).

Duplicate rejection still applies when the existing entry is already
Local (no double-receive of the same fully-cached message).

ZEB-114 Phase 2 — race safety between walker and live push.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C6: Create `mail_sync.rs` skeleton

**Files:**
- Create: `src-tauri/src/mail_sync.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod mail_sync;` declaration)

This task creates only the types and the public method stubs. Walker logic comes in C7/C8.

- [ ] **Step 1: Create the skeleton file**

```rust
// src-tauri/src/mail_sync.rs
//! Mailbox sync: walks the gateway-published Merkle tree and registers
//! header-only entries with MailManager. Lazy body fetch on demand.
//!
//! See docs/specs/2026-04-14-client-mail-receive-design.md.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::{mpsc, oneshot};

use crate::mail::MailManager;

pub const CID_LEN: usize = 32;

/// Status payload emitted on the `mail-sync-status` Tauri event.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatusEvent {
    pub state: &'static str, // "idle" | "syncing" | "error"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Internal walker state.
#[derive(Debug)]
enum SyncState {
    Idle { last_walked_root: Option<[u8; CID_LEN]> },
    Walking { root: [u8; CID_LEN], started_at: Instant, pending_root: Option<[u8; CID_LEN]> },
    Error { last_error: String, last_walked_root: Option<[u8; CID_LEN]> },
}

/// Request to fetch a CAS blob from the gateway. Mirrors the existing
/// FetchRequest used by the event_loop's fetch_rx channel.
pub struct FetchRequest {
    pub cid_hex: String,
    pub reply: oneshot::Sender<Result<Vec<u8>, String>>,
}

/// In-flight body-fetch deduplication.
type InFlightMap = Arc<Mutex<HashMap<[u8; CID_LEN], tokio::sync::watch::Receiver<Option<Result<Vec<u8>, String>>>>>>;

pub struct MailSync {
    state: Arc<Mutex<SyncState>>,
    fetch_tx: mpsc::Sender<FetchRequest>,
    mail_mgr: Arc<Mutex<MailManager>>,
    own_addr_hex: String,
    app: AppHandle,
    in_flight_bodies: InFlightMap,
}

impl MailSync {
    pub fn new(
        fetch_tx: mpsc::Sender<FetchRequest>,
        mail_mgr: Arc<Mutex<MailManager>>,
        own_addr_hex: String,
        app: AppHandle,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(SyncState::Idle { last_walked_root: None })),
            fetch_tx,
            mail_mgr,
            own_addr_hex,
            app,
            in_flight_bodies: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Handle a root CID payload received via Zenoh sub on
    /// `harmony/mail/v1/{addr}/root`. Spawns a walker pass.
    pub async fn handle_root_push(self: Arc<Self>, payload: &[u8]) {
        let Ok(root) = <[u8; CID_LEN]>::try_from(payload) else {
            tracing::warn!(len = payload.len(), "ignoring malformed root push (expected 32 bytes)");
            return;
        };
        self.start_or_queue_walk(root).await;
    }

    /// Handle a reply from the cold-start Zenoh `get` query.
    /// Empty payload means the gateway has no mail for this address yet.
    pub async fn handle_startup_query_reply(self: Arc<Self>, payload: Option<&[u8]>) {
        match payload {
            None | Some(b"") => {
                tracing::info!("startup query: no mail for this address yet");
            }
            Some(bytes) => {
                if let Ok(root) = <[u8; CID_LEN]>::try_from(bytes) {
                    self.start_or_queue_walk(root).await;
                } else {
                    tracing::warn!(len = bytes.len(), "ignoring malformed startup query reply");
                }
            }
        }
    }

    /// Manual refresh trigger from UI. Re-queries the gateway for the
    /// current root and walks if it has changed.
    pub async fn refresh_now(self: Arc<Self>) {
        // Phase 2 simplification: re-issue the same logic as startup query
        // by sending a fetch-style request through fetch_tx. The event_loop
        // routes it as a Zenoh `get` against the root key.
        //
        // For Phase 2, we issue a normal Zenoh query via the fetch_tx
        // channel using a special key prefix that event_loop recognizes.
        // (Detail finalized in Task C12.)
        tracing::info!("manual refresh requested — implementation in C12");
        // TODO(C12): wire actual query path.
    }

    /// Lazy body fetch. Called from the fetch_mail_body Tauri command.
    pub async fn fetch_body(self: Arc<Self>, cid: [u8; CID_LEN]) -> Result<Vec<u8>, String> {
        // Implementation in C10.
        Err("fetch_body not yet implemented (Task C10)".to_string())
    }

    async fn start_or_queue_walk(self: Arc<Self>, root: [u8; CID_LEN]) {
        // Implementation in C9.
        tracing::debug!(?root, "start_or_queue_walk called (Task C9 will implement)");
    }

    fn emit_status(&self, event: SyncStatusEvent) {
        if let Err(e) = self.app.emit("mail-sync-status", &event) {
            tracing::warn!(error = %e, "failed to emit mail-sync-status");
        }
    }
}
```

- [ ] **Step 2: Add module declaration to `lib.rs`**

In `src-tauri/src/lib.rs`, near the other `mod` declarations at the top:

```rust
mod mail_sync;
```

- [ ] **Step 3: Verify it compiles**

```bash
cd src-tauri
cargo check
```

Expected: clean compile, possibly with `unused_variables` / `dead_code` warnings on the stub methods (acceptable for now).

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/mail_sync.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(mail-sync): scaffold MailSync module for Phase 2 walker

Adds the public API surface (handle_root_push, handle_startup_query_reply,
refresh_now, fetch_body) with stub implementations, plus the SyncState
enum and SyncStatusEvent payload type. Walker logic and body fetch land
in subsequent tasks.

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C7: Implement walker root + folder fetch (strict error policy)

**Files:**
- Modify: `src-tauri/src/mail_sync.rs`

- [ ] **Step 1: Add a fetch helper that wraps the channel call**

In `mail_sync.rs`, add a private method:

```rust
impl MailSync {
    /// Fetch a CAS blob via the event_loop's fetch channel. 30-second budget.
    async fn fetch_cas(&self, cid: [u8; CID_LEN]) -> Result<Vec<u8>, String> {
        let cid_hex = hex::encode(cid);
        let (reply_tx, reply_rx) = oneshot::channel();
        self.fetch_tx
            .send(FetchRequest { cid_hex: cid_hex.clone(), reply: reply_tx })
            .await
            .map_err(|_| "fetch channel closed".to_string())?;
        match tokio::time::timeout(std::time::Duration::from_secs(30), reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("fetch reply channel dropped".to_string()),
            Err(_) => Err(format!("fetch timeout for {cid_hex}")),
        }
    }
}
```

- [ ] **Step 2: Write tests for root + folder fetch**

Add tests at the bottom of `mail_sync.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use harmony_mailbox::mailbox::{FolderKind, MailFolder, MailRoot, MessageEntry};

    /// Test harness: a stub fetch responder backed by a HashMap of CID → bytes.
    /// Bytes not in the map return NotFound errors.
    struct StubFetcher {
        responses: HashMap<[u8; CID_LEN], Vec<u8>>,
    }

    impl StubFetcher {
        fn new() -> Self { Self { responses: HashMap::new() } }
        fn insert(&mut self, cid: [u8; CID_LEN], bytes: Vec<u8>) {
            self.responses.insert(cid, bytes);
        }
        async fn run(mut self, mut rx: mpsc::Receiver<FetchRequest>) {
            while let Some(req) = rx.recv().await {
                let cid_bytes = hex::decode(&req.cid_hex).unwrap();
                let cid: [u8; CID_LEN] = cid_bytes.try_into().unwrap();
                let result = self.responses.get(&cid)
                    .cloned()
                    .ok_or_else(|| format!("not found: {}", req.cid_hex));
                let _ = req.reply.send(result);
            }
        }
    }

    /// Build a Tauri-free MailSync for testing. Uses a no-op AppHandle stub.
    fn make_test_mail_sync(
        fetch_tx: mpsc::Sender<FetchRequest>,
        mail_mgr: Arc<Mutex<MailManager>>,
    ) -> Arc<MailSync> {
        // Tauri AppHandle is not constructable in tests; use the test-only
        // builder below or skip emit_status assertions in walker tests.
        // For Phase 2 unit tests, we use a feature-gated TestEmitter trait
        // OR we use tauri's `mock_app()` helper. Use mock_app() for simplicity.
        let app = tauri::test::mock_app();
        Arc::new(MailSync::new(
            fetch_tx,
            mail_mgr,
            "00112233445566778899aabbccddeeff".to_string(),
            app.handle().clone(),
        ))
    }

    #[tokio::test]
    async fn walk_aborts_on_root_fetch_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(
            MailManager::load(&tmp.path().join("mail"), [0u8; 16])
        ));
        let (fetch_tx, fetch_rx) = mpsc::channel(16);

        // No responses inserted — root fetch will fail.
        tokio::spawn(StubFetcher::new().run(fetch_rx));

        let sync = make_test_mail_sync(fetch_tx, mail_mgr.clone());
        let bad_root = [0xDE; CID_LEN];
        sync.clone().handle_root_push(&bad_root).await;
        // Allow walker to run.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Inbox empty, state should be Error.
        let inbox = mail_mgr.lock().unwrap().list_folder("inbox", 0, 100);
        assert_eq!(inbox.len(), 0);
        match &*sync.state.lock().unwrap() {
            SyncState::Error { last_error, .. } => assert!(last_error.contains("not found") || last_error.contains("timeout")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn walk_aborts_on_folder_fetch_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(
            MailManager::load(&tmp.path().join("mail"), [0u8; 16])
        ));
        let (fetch_tx, fetch_rx) = mpsc::channel(16);

        // Construct a valid MailRoot pointing at a folder CID we won't serve.
        let folder_cid = [0xF0; CID_LEN];
        let mut root = MailRoot::new([0u8; 16]);
        root.set_folder(FolderKind::Inbox, folder_cid);
        let root_bytes = root.to_bytes();
        let root_cid = *blake3::hash(&root_bytes).as_bytes();

        let mut stub = StubFetcher::new();
        stub.insert(root_cid, root_bytes);
        // folder_cid intentionally NOT inserted.
        tokio::spawn(stub.run(fetch_rx));

        let sync = make_test_mail_sync(fetch_tx, mail_mgr.clone());
        sync.clone().handle_root_push(&root_cid).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let inbox = mail_mgr.lock().unwrap().list_folder("inbox", 0, 100);
        assert_eq!(inbox.len(), 0);
        match &*sync.state.lock().unwrap() {
            SyncState::Error { .. } => {},
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
```

> **NOTE on test infra:** the `tauri::test::mock_app()` helper requires `tauri` with the `test` feature in dev-dependencies. Add this to `src-tauri/Cargo.toml` if not already present:
> ```toml
> [dev-dependencies]
> tauri = { workspace = true, features = ["test"] }
> tempfile = "3"
> ```
> If `mock_app` doesn't exist in this Tauri version, fall back to extracting an `EmitTarget` trait that MailSync uses, with a real-AppHandle impl in production and a no-op test impl. Document the chosen approach in code.

- [ ] **Step 3: Run tests to verify failure**

```bash
cargo test -p harmony-client mail_sync::tests::walk_aborts
```

Expected: FAIL — `start_or_queue_walk` is a stub, doesn't actually walk.

- [ ] **Step 4: Implement the walk pass (root + folder portion)**

Replace the `start_or_queue_walk` stub with a real walker that handles root + folder fetch but defers page traversal to C8 (calls a stub helper `walk_pages`):

```rust
impl MailSync {
    async fn start_or_queue_walk(self: Arc<Self>, root: [u8; CID_LEN]) {
        // Single-flight: if already walking, queue the new root for later.
        // (Full impl in C9; for now, just transition to Walking.)
        {
            let mut state = self.state.lock().unwrap();
            match &mut *state {
                SyncState::Walking { pending_root, .. } => {
                    *pending_root = Some(root);
                    return;
                }
                _ => {
                    *state = SyncState::Walking {
                        root,
                        started_at: Instant::now(),
                        pending_root: None,
                    };
                }
            }
        }

        let me = Arc::clone(&self);
        tokio::spawn(async move {
            me.run_walk_pass(root).await;
        });
    }

    async fn run_walk_pass(self: Arc<Self>, root: [u8; CID_LEN]) {
        use harmony_mailbox::mailbox::{FolderKind, MailFolder, MailRoot};

        self.emit_status(SyncStatusEvent { state: "syncing", error: None });

        // Step 1: fetch + parse root.
        let root_bytes = match self.fetch_cas(root).await {
            Ok(b) => b,
            Err(e) => return self.finish_walk_error(format!("root fetch: {e}")),
        };
        let mail_root = match MailRoot::from_bytes(&root_bytes) {
            Ok(r) => r,
            Err(e) => return self.finish_walk_error(format!("root parse: {e}")),
        };

        // Step 2: fetch + parse Inbox folder.
        let folder_cid = mail_root.folder(FolderKind::Inbox);
        let folder_bytes = match self.fetch_cas(folder_cid).await {
            Ok(b) => b,
            Err(e) => return self.finish_walk_error(format!("folder fetch: {e}")),
        };
        let folder = match MailFolder::from_bytes(&folder_bytes) {
            Ok(f) => f,
            Err(e) => return self.finish_walk_error(format!("folder parse: {e}")),
        };

        // Step 3: walk pages (Task C8 implements this).
        let skip_summary = self.walk_pages(&folder.page_cids).await;

        // Step 4: finalize state.
        self.finish_walk(root, skip_summary);
    }

    /// Stub for Task C8. Returns Some(summary) if any pages/entries skipped.
    async fn walk_pages(&self, _page_cids: &[[u8; CID_LEN]]) -> Option<String> {
        // Implementation in C8.
        None
    }

    fn finish_walk_error(self: Arc<Self>, error: String) {
        let last_walked = match &*self.state.lock().unwrap() {
            SyncState::Walking { .. } => None,
            SyncState::Idle { last_walked_root } | SyncState::Error { last_walked_root, .. } => *last_walked_root,
        };
        *self.state.lock().unwrap() = SyncState::Error {
            last_error: error.clone(),
            last_walked_root: last_walked,
        };
        self.emit_status(SyncStatusEvent { state: "error", error: Some(error) });
    }

    fn finish_walk(self: Arc<Self>, root: [u8; CID_LEN], skip_summary: Option<String>) {
        let mut state = self.state.lock().unwrap();
        let pending = if let SyncState::Walking { pending_root, .. } = &*state {
            *pending_root
        } else {
            None
        };
        *state = match skip_summary {
            None => SyncState::Idle { last_walked_root: Some(root) },
            Some(summary) => SyncState::Error {
                last_error: summary,
                last_walked_root: Some(root),
            },
        };
        drop(state);
        // Emit terminal event.
        let event = match &*self.state.lock().unwrap() {
            SyncState::Idle { .. } => SyncStatusEvent { state: "idle", error: None },
            SyncState::Error { last_error, .. } => SyncStatusEvent { state: "error", error: Some(last_error.clone()) },
            _ => SyncStatusEvent { state: "idle", error: None },
        };
        self.emit_status(event);
        // Re-walk if pending root was queued.
        if let Some(next_root) = pending {
            let me = Arc::clone(&self);
            tokio::spawn(async move {
                me.start_or_queue_walk(next_root).await;
            });
        }
    }
}
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p harmony-client mail_sync::tests::walk_aborts
```

Expected: both abort tests PASS.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/mail_sync.rs src-tauri/Cargo.toml
git commit -m "$(cat <<'EOF'
feat(mail-sync): walker root + folder fetch with strict error policy

Implements the walker's first two steps: fetch and parse MailRoot, then
fetch and parse the Inbox MailFolder. Both use the strict error policy
(Q7 hybrid): any failure aborts the entire walk and emits an error event.
Page traversal is stubbed for Task C8.

Adds tauri test feature + tempfile to dev-deps for unit tests.

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C8: Implement page + entry traversal with skip policy

**Files:**
- Modify: `src-tauri/src/mail_sync.rs`

- [ ] **Step 1: Write tests for the happy path and skip cases**

```rust
#[tokio::test]
async fn walk_single_page_registers_all_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let mail_mgr = Arc::new(Mutex::new(
        MailManager::load(&tmp.path().join("mail"), [0u8; 16])
    ));
    let (fetch_tx, fetch_rx) = mpsc::channel(16);

    // Build: 1 page with 3 entries → 1 folder pointing at it → 1 root.
    let entries: Vec<MessageEntry> = (0..3).map(|i| MessageEntry {
        message_cid: [i as u8; 32],
        message_id: [i as u8; 16],
        sender_address_hash: [0xCC; 16],
        timestamp: 1700000000 + i as u64,
        read: false,
        subject_snippet: format!("entry {i}"),
    }).collect();

    let page = MailPage::with_entries(entries.clone(), None);
    let page_bytes = page.to_bytes();
    let page_cid = *blake3::hash(&page_bytes).as_bytes();

    let folder = MailFolder::with_pages(vec![page_cid], 3, 3);
    let folder_bytes = folder.to_bytes();
    let folder_cid = *blake3::hash(&folder_bytes).as_bytes();

    let mut root = MailRoot::new([0u8; 16]);
    root.set_folder(FolderKind::Inbox, folder_cid);
    let root_bytes = root.to_bytes();
    let root_cid = *blake3::hash(&root_bytes).as_bytes();

    let mut stub = StubFetcher::new();
    stub.insert(root_cid, root_bytes);
    stub.insert(folder_cid, folder_bytes);
    stub.insert(page_cid, page_bytes);
    tokio::spawn(stub.run(fetch_rx));

    let sync = make_test_mail_sync(fetch_tx, mail_mgr.clone());
    sync.clone().handle_root_push(&root_cid).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let inbox = mail_mgr.lock().unwrap().list_folder("inbox", 0, 100);
    assert_eq!(inbox.len(), 3);
    assert!(inbox.iter().all(|e| e.body_state == BodyState::Pending));
    assert!(matches!(*sync.state.lock().unwrap(), SyncState::Idle { .. }));
}

#[tokio::test]
async fn walk_skips_missing_page_continues_others() {
    let tmp = tempfile::tempdir().unwrap();
    let mail_mgr = Arc::new(Mutex::new(
        MailManager::load(&tmp.path().join("mail"), [0u8; 16])
    ));
    let (fetch_tx, fetch_rx) = mpsc::channel(16);

    // Build: 2 pages, both linked from folder; only page1 served.
    let entry1 = MessageEntry {
        message_cid: [1; 32], message_id: [1; 16], sender_address_hash: [0; 16],
        timestamp: 1700000001, read: false, subject_snippet: "page1 entry".to_string(),
    };
    let page1 = MailPage::with_entries(vec![entry1], None);
    let page1_bytes = page1.to_bytes();
    let page1_cid = *blake3::hash(&page1_bytes).as_bytes();

    let page2_cid = [0xFE; 32]; // unserved → 404

    let folder = MailFolder::with_pages(vec![page1_cid, page2_cid], 2, 2);
    let folder_bytes = folder.to_bytes();
    let folder_cid = *blake3::hash(&folder_bytes).as_bytes();

    let mut root = MailRoot::new([0u8; 16]);
    root.set_folder(FolderKind::Inbox, folder_cid);
    let root_bytes = root.to_bytes();
    let root_cid = *blake3::hash(&root_bytes).as_bytes();

    let mut stub = StubFetcher::new();
    stub.insert(root_cid, root_bytes);
    stub.insert(folder_cid, folder_bytes);
    stub.insert(page1_cid, page1_bytes);
    // page2_cid not inserted.
    tokio::spawn(stub.run(fetch_rx));

    let sync = make_test_mail_sync(fetch_tx, mail_mgr.clone());
    sync.clone().handle_root_push(&root_cid).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let inbox = mail_mgr.lock().unwrap().list_folder("inbox", 0, 100);
    assert_eq!(inbox.len(), 1, "page1 entry registered, page2 skipped");
    match &*sync.state.lock().unwrap() {
        SyncState::Error { last_error, .. } => assert!(last_error.contains("page")),
        other => panic!("expected Error after skip, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run tests to verify failure**

```bash
cargo test -p harmony-client mail_sync::tests::walk_single_page mail_sync::tests::walk_skips
```

Expected: FAIL — `walk_pages` is a stub.

- [ ] **Step 3: Implement `walk_pages` with parallel fetch + skip policy**

Replace the stub:

```rust
impl MailSync {
    async fn walk_pages(&self, page_cids: &[[u8; CID_LEN]]) -> Option<String> {
        use harmony_mailbox::mailbox::MailPage;
        use tokio::sync::Semaphore;
        use futures::future::join_all;

        const MAX_CONCURRENT_PAGES: usize = 8;
        let sem = Arc::new(Semaphore::new(MAX_CONCURRENT_PAGES));
        let mut skipped_pages: Vec<String> = Vec::new();
        let mut skipped_entries: usize = 0;
        let mut new_entry_cids: Vec<String> = Vec::new();

        let fetch_results: Vec<(String, Result<Vec<u8>, String>)> = join_all(
            page_cids.iter().map(|cid| {
                let cid = *cid;
                let cid_hex = hex::encode(cid);
                let sem = Arc::clone(&sem);
                async move {
                    let _permit = sem.acquire().await.unwrap();
                    let bytes = self.fetch_cas(cid).await;
                    (cid_hex, bytes)
                }
            })
        ).await;

        for (page_cid_hex, fetch_result) in fetch_results {
            let bytes = match fetch_result {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(page_cid = %page_cid_hex, error = %e, "page fetch failed; skipping");
                    skipped_pages.push(page_cid_hex);
                    continue;
                }
            };
            let page = match MailPage::from_bytes(&bytes) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(page_cid = %page_cid_hex, error = %e, "page parse failed; skipping");
                    skipped_pages.push(page_cid_hex);
                    continue;
                }
            };
            for entry in page.entries {
                let mut mgr = match self.mail_mgr.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                match mgr.register_header_only(entry) {
                    Ok(crate::mail::RegisterOutcome::Inserted { cid }) => {
                        new_entry_cids.push(cid);
                    }
                    Ok(crate::mail::RegisterOutcome::Duplicate) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "register_header_only failed; skipping entry");
                        skipped_entries += 1;
                    }
                }
            }
        }

        // Emit per-new-entry events for the frontend (matches Phase 0 receive_message pattern).
        for cid in new_entry_cids {
            // Frontend listens for "mail-received" events with EntryRecord payload.
            // Look up the entry to send full record.
            let mgr = match self.mail_mgr.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            for folder in ["inbox"] {
                if let Some(entry) = mgr.list_folder(folder, 0, 1000)
                    .into_iter()
                    .find(|e| e.message_cid == cid)
                {
                    let _ = self.app.emit("mail-received", &entry);
                    break;
                }
            }
        }

        if skipped_pages.is_empty() && skipped_entries == 0 {
            None
        } else {
            Some(format!(
                "skipped {} page(s), {} entr(y/ies)",
                skipped_pages.len(), skipped_entries
            ))
        }
    }
}
```

Add `futures` to `src-tauri/Cargo.toml` `[dependencies]` if not already present (or use the existing `futures-util` if that's what's in the workspace):

```toml
futures = "0.3"
```

- [ ] **Step 4: Run tests**

```bash
cargo test -p harmony-client mail_sync::tests
```

Expected: all walker tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/mail_sync.rs src-tauri/Cargo.toml
git commit -m "$(cat <<'EOF'
feat(mail-sync): walk pages in parallel with skip-on-failure policy

Hybrid error policy (Q7): page fetch failures and parse errors are
logged and the page is skipped; other pages continue. Inbox shows the
available subset; status transitions to Error with a skip summary so
the user sees the indicator + tooltip.

Up to 8 concurrent page fetches via Semaphore. Per-new-entry
mail-received events emitted for the frontend.

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C9: Single-flight `pending_root` semantics

The skeleton in C7 already partially handles `pending_root`. Verify it correctly serializes back-to-back walks and add an explicit test.

**Files:**
- Modify: `src-tauri/src/mail_sync.rs`

- [ ] **Step 1: Write the test**

```rust
#[tokio::test]
async fn pending_root_during_walk_runs_after_current_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let mail_mgr = Arc::new(Mutex::new(
        MailManager::load(&tmp.path().join("mail"), [0u8; 16])
    ));
    let (fetch_tx, fetch_rx) = mpsc::channel(32);

    // Build TWO different roots, each with one entry.
    fn make_tree(seed: u8) -> ([u8; 32], Vec<([u8; 32], Vec<u8>)>) {
        let entry = MessageEntry {
            message_cid: [seed; 32],
            message_id: [seed; 16],
            sender_address_hash: [0; 16],
            timestamp: 1700000000 + seed as u64,
            read: false,
            subject_snippet: format!("seed {seed}"),
        };
        let page = MailPage::with_entries(vec![entry], None);
        let page_bytes = page.to_bytes();
        let page_cid = *blake3::hash(&page_bytes).as_bytes();
        let folder = MailFolder::with_pages(vec![page_cid], 1, 1);
        let folder_bytes = folder.to_bytes();
        let folder_cid = *blake3::hash(&folder_bytes).as_bytes();
        let mut root = MailRoot::new([0; 16]);
        root.set_folder(FolderKind::Inbox, folder_cid);
        let root_bytes = root.to_bytes();
        let root_cid = *blake3::hash(&root_bytes).as_bytes();
        (root_cid, vec![
            (root_cid, root_bytes),
            (folder_cid, folder_bytes),
            (page_cid, page_bytes),
        ])
    }
    let (root1, blobs1) = make_tree(0xAA);
    let (root2, blobs2) = make_tree(0xBB);

    let mut stub = StubFetcher::new();
    for (cid, bytes) in blobs1.into_iter().chain(blobs2.into_iter()) {
        stub.insert(cid, bytes);
    }
    tokio::spawn(stub.run(fetch_rx));

    let sync = make_test_mail_sync(fetch_tx, mail_mgr.clone());
    // Push root1, then immediately push root2 before root1 walk completes.
    sync.clone().handle_root_push(&root1).await;
    sync.clone().handle_root_push(&root2).await;
    // Wait for both walks.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let inbox = mail_mgr.lock().unwrap().list_folder("inbox", 0, 100);
    let message_ids: std::collections::HashSet<String> = inbox.iter()
        .map(|e| e.message_id.clone())
        .collect();
    assert!(message_ids.contains(&hex::encode([0xAA; 16])), "root1 entry missing");
    assert!(message_ids.contains(&hex::encode([0xBB; 16])), "root2 entry missing");
    assert!(matches!(*sync.state.lock().unwrap(), SyncState::Idle { .. }));
}
```

- [ ] **Step 2: Run test**

```bash
cargo test -p harmony-client mail_sync::tests::pending_root
```

Expected: PASS (the C7 skeleton already implements the pending_root branch). If it fails, debug — likely the `finish_walk` re-spawn isn't picking up the queued root.

- [ ] **Step 3: Commit (test only, no code change if test passes)**

```bash
git add src-tauri/src/mail_sync.rs
git commit -m "$(cat <<'EOF'
test(mail-sync): cover single-flight pending_root semantics

Two back-to-back root pushes during a walk both eventually result in
their entries appearing in the inbox; final state is Idle.

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C10: Implement `fetch_body` with in-flight dedup + hash validation

**Files:**
- Modify: `src-tauri/src/mail_sync.rs`

- [ ] **Step 1: Write tests**

```rust
#[tokio::test]
async fn fetch_body_returns_bytes_and_marks_local() {
    let tmp = tempfile::tempdir().unwrap();
    let mail_mgr = Arc::new(Mutex::new(
        MailManager::load(&tmp.path().join("mail"), [0u8; 16])
    ));
    let (fetch_tx, fetch_rx) = mpsc::channel(16);

    // Register a Pending entry for a known CID first.
    let msg = HarmonyMessage::email([0xAA; 16], vec![], "subj", "body").unwrap();
    let bytes = msg.to_bytes();
    let cid = *blake3::hash(&bytes).as_bytes();
    let entry = MessageEntry {
        message_cid: cid, message_id: msg.message_id,
        sender_address_hash: msg.sender_address, timestamp: msg.timestamp,
        read: false, subject_snippet: "subj".to_string(),
    };
    mail_mgr.lock().unwrap().register_header_only(entry).unwrap();

    let mut stub = StubFetcher::new();
    stub.insert(cid, bytes.clone());
    tokio::spawn(stub.run(fetch_rx));

    let sync = make_test_mail_sync(fetch_tx, mail_mgr.clone());
    let result = sync.clone().fetch_body(cid).await.unwrap();
    assert_eq!(result, bytes);

    // Entry promoted.
    let inbox = mail_mgr.lock().unwrap().list_folder("inbox", 0, 100);
    assert_eq!(inbox[0].body_state, BodyState::Local);
}

#[tokio::test]
async fn fetch_body_rejects_hash_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let mail_mgr = Arc::new(Mutex::new(
        MailManager::load(&tmp.path().join("mail"), [0u8; 16])
    ));
    let (fetch_tx, fetch_rx) = mpsc::channel(16);

    let claimed_cid = [0xDD; 32];
    let mut stub = StubFetcher::new();
    stub.insert(claimed_cid, b"wrong bytes that don't hash".to_vec());
    tokio::spawn(stub.run(fetch_rx));

    let sync = make_test_mail_sync(fetch_tx, mail_mgr.clone());
    let result = sync.fetch_body(claimed_cid).await;
    assert!(result.is_err(), "should reject; got {result:?}");
    assert!(result.unwrap_err().contains("hash"));
}

#[tokio::test]
async fn fetch_body_dedups_concurrent_calls() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tmp = tempfile::tempdir().unwrap();
    let mail_mgr = Arc::new(Mutex::new(
        MailManager::load(&tmp.path().join("mail"), [0u8; 16])
    ));
    let (fetch_tx, mut fetch_rx) = mpsc::channel::<FetchRequest>(16);

    let msg = HarmonyMessage::email([0xCC; 16], vec![], "s", "b").unwrap();
    let bytes = msg.to_bytes();
    let cid = *blake3::hash(&bytes).as_bytes();

    // Custom fetcher that counts how many times it was asked.
    let count = Arc::new(AtomicUsize::new(0));
    let count_clone = Arc::clone(&count);
    let bytes_clone = bytes.clone();
    tokio::spawn(async move {
        while let Some(req) = fetch_rx.recv().await {
            count_clone.fetch_add(1, Ordering::SeqCst);
            // Slow response so the second fetch_body call lands while first is in flight.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let _ = req.reply.send(Ok(bytes_clone.clone()));
        }
    });

    let sync = make_test_mail_sync(fetch_tx, mail_mgr);
    let h1 = tokio::spawn({
        let s = sync.clone();
        async move { s.fetch_body(cid).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let h2 = tokio::spawn({
        let s = sync.clone();
        async move { s.fetch_body(cid).await }
    });

    let r1 = h1.await.unwrap().unwrap();
    let r2 = h2.await.unwrap().unwrap();
    assert_eq!(r1, bytes);
    assert_eq!(r2, bytes);
    assert_eq!(count.load(Ordering::SeqCst), 1, "should only fetch once");
}
```

- [ ] **Step 2: Run tests to verify failure**

```bash
cargo test -p harmony-client mail_sync::tests::fetch_body
```

Expected: FAIL — `fetch_body` returns the stub error.

- [ ] **Step 3: Implement `fetch_body`**

Replace the `fetch_body` stub. The implementation uses `tokio::sync::watch` for in-flight dedup so multiple awaiters share one Result without cloning the future:

```rust
impl MailSync {
    pub async fn fetch_body(self: Arc<Self>, cid: [u8; CID_LEN]) -> Result<Vec<u8>, String> {
        // Check in-flight map: if another caller is fetching this CID, await its result.
        let existing = {
            let map = self.in_flight_bodies.lock().unwrap_or_else(|p| p.into_inner());
            map.get(&cid).cloned()
        };
        if let Some(mut rx) = existing {
            loop {
                if let Some(result) = rx.borrow().clone() {
                    return result;
                }
                if rx.changed().await.is_err() {
                    return Err("in-flight fetch cancelled".to_string());
                }
            }
        }

        // No in-flight: register a watch sender and start the fetch.
        let (tx, rx) = tokio::sync::watch::channel(None);
        {
            let mut map = self.in_flight_bodies.lock().unwrap_or_else(|p| p.into_inner());
            map.insert(cid, rx);
        }

        // Perform the actual fetch + verification.
        let result = async {
            let bytes = self.fetch_cas(cid).await?;
            let computed = blake3::hash(&bytes);
            if computed.as_bytes() != &cid {
                return Err(format!(
                    "hash mismatch: claimed {}, computed {}",
                    hex::encode(cid), hex::encode(computed.as_bytes())
                ));
            }
            // Validate parses as HarmonyMessage.
            harmony_mailbox::message::HarmonyMessage::from_bytes(&bytes)
                .map_err(|e| format!("parse: {e}"))?;
            // Persist via MailManager.
            let cid_hex = hex::encode(cid);
            {
                let mut mgr = self.mail_mgr.lock().unwrap_or_else(|p| p.into_inner());
                mgr.mark_body_received(&cid_hex, &bytes)?;
            }
            Ok(bytes)
        }.await;

        // Publish result to all waiters and clear from map.
        let _ = tx.send(Some(result.clone()));
        {
            let mut map = self.in_flight_bodies.lock().unwrap_or_else(|p| p.into_inner());
            map.remove(&cid);
        }
        result
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test -p harmony-client mail_sync::tests
```

Expected: all 8 mail_sync tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/mail_sync.rs
git commit -m "$(cat <<'EOF'
feat(mail-sync): implement fetch_body with in-flight dedup + hash check

Lazy body fetch for Pending entries. Uses tokio::sync::watch to share
one in-flight fetch among concurrent callers — a user double-clicking
a message issues only one Zenoh fetch.

BLAKE3 hash verification rejects bytes that don't match the claimed CID;
HarmonyMessage::from_bytes validates structure. On success, MailManager
mark_body_received writes the blob and promotes the entry to Local.

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C11: event_loop.rs filter flip + own_root_key wiring

**Files:**
- Modify: `src-tauri/src/event_loop.rs`

The existing filter at line 797 silently drops `/root` events. Phase 2 splits the branch to route them to MailSync.

> **C5 follow-up (I3):** Post-C5, `MailManager::receive_message` returns `Ok(EntryRecord)` for BOTH a fresh insert AND a Pending-to-Local promotion. The existing event_loop handler emits a `mail-received` Tauri event on `Ok`, which would cause a spurious notification/badge bump when a promotion fires for a row the user already saw (from the walker). In this task, distinguish the two cases so promotion does NOT emit `mail-received`. Options: (a) compare inbox state before/after receive_message, (b) refactor `receive_message` to return `ReceiveOutcome::{Inserted, Promoted}(EntryRecord)` — (b) is cleaner but changes the API; pick whichever is less invasive when you wire this.

- [ ] **Step 1: Add MailSync parameter to event_loop**

In `event_loop.rs` near where `mail_mgr` is defined as a parameter / closed-over value (around line 42 / line 265), add `mail_sync` alongside it. The exact wiring depends on existing structure; the event_loop function probably has a struct or signature that needs extending. Concretely:

- If event_loop takes a struct of dependencies, add `mail_sync: Option<Arc<crate::mail_sync::MailSync>>` to it.
- If event_loop takes a list of arguments, add the parameter at the end.

Wire it as `Option<Arc<MailSync>>` so existing callers can pass `None` until C13 wires the real instance.

- [ ] **Step 2: Compute `own_root_key` once at startup**

After `own_hex` is derived (line 252):

```rust
let own_root_key = format!("harmony/mail/v1/{own_hex}/root");
let own_mail_key = format!("harmony/mail/v1/{own_hex}");
```

Pass these into `handle_subscription_event` (or wherever the filter lives) — either as parameters or via a closure capture, depending on existing structure.

Add a Subscribe action for the root topic alongside the existing mail subscribe (around line 254-265):

```rust
dispatch_action(
    RuntimeAction::Subscribe {
        key_expr: format!("harmony/mail/v1/{own_hex}/root"),
    },
    &session, &zenoh_tx, &udp, &broadcast_addr, &app, &closing, &own_zid,
).await;
```

- [ ] **Step 3: Modify the filter at line 797**

Replace:

```rust
} else if key_expr.starts_with("harmony/mail/v1/") && !key_expr.ends_with("/root") {
    // existing receive_message branch
```

With:

```rust
} else if key_expr == own_root_key {
    // Phase 2: route root CID to MailSync.
    if let Some(ref sync) = mail_sync {
        let sync = Arc::clone(sync);
        let payload = payload.to_vec();
        tokio::spawn(async move {
            sync.handle_root_push(&payload).await;
        });
    } else {
        tracing::debug!("got root push but mail_sync not initialized; ignoring");
    }
} else if key_expr == own_mail_key {
    // Existing: live raw mail receive.
    let mut mgr = match mail_mgr.lock() {
        Ok(g) => g,
        Err(e) => {
            tracing::error!(error = %e, "mail_mgr mutex poisoned");
            return;
        }
    };
    match mgr.receive_message(payload) {
        Ok(entry) => { let _ = app.emit("mail-received", &entry); }
        Err(e) => { tracing::debug!(key_expr, error = %e, "mail receive skipped"); }
    }
}
```

(Rename branch identifier from "harmony/mail/v1/" prefix to exact key match for both — defensive, aligns with the spec.)

- [ ] **Step 4: Verify it compiles**

```bash
cargo check -p harmony-client
```

Expected: clean. Warnings about unused `mail_sync` parameter are OK until C13.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/event_loop.rs
git commit -m "$(cat <<'EOF'
feat(event-loop): route mail root pushes to MailSync

Splits the harmony/mail/v1/* subscriber filter into two exact-match
branches: own /root events route to MailSync::handle_root_push, own
non-root events route to the existing MailManager::receive_message
path. Defensive equality check prevents accidentally routing on other
addresses if subscription scope ever broadens.

Subscribes to /root at startup alongside the existing /mail subscription.

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C12: Startup query (one-shot) + manual refresh path

**Files:**
- Modify: `src-tauri/src/event_loop.rs`

- [ ] **Step 1: Add a startup query right after Subscribe declarations**

After both Subscribe actions are dispatched (Task C11), insert a startup query block:

```rust
// Phase 2: cold-start root query. Pulls current root via Zenoh `get`
// in case the gateway last published before this client subscribed.
if let Some(ref sync) = mail_sync {
    let sync = Arc::clone(sync);
    let session_clone = session.clone();
    let key = format!("harmony/mail/v1/{own_hex}/root");
    tokio::spawn(async move {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            async {
                let replies = session_clone.get(&key).await.map_err(|e| format!("get: {e}"))?;
                let mut payload: Option<Vec<u8>> = None;
                while let Ok(reply) = replies.recv_async().await {
                    if let Ok(sample) = reply.result() {
                        payload = Some(sample.payload().to_bytes().to_vec());
                        break;
                    }
                }
                Ok::<_, String>(payload)
            }
        ).await;
        match result {
            Ok(Ok(payload)) => sync.handle_startup_query_reply(payload.as_deref()).await,
            Ok(Err(e)) => tracing::warn!(error = %e, "startup root query failed"),
            Err(_) => tracing::warn!("startup root query timed out (10s)"),
        }
    });
}
```

- [ ] **Step 2: Wire `refresh_now` to do the same query on demand**

In `mail_sync.rs`, replace the `refresh_now` stub. We need a way for MailSync to issue a Zenoh query without holding the session directly — extend the existing fetch path or add a new channel.

Simplest: add a `RefreshRequest` channel in addition to FetchRequest, parallel to fetch_tx. In event_loop, listen for it and run the same query. In `MailSync::new`, accept a `refresh_tx`:

```rust
// In mail_sync.rs MailSync struct:
refresh_tx: mpsc::Sender<oneshot::Sender<Result<Option<Vec<u8>>, String>>>,
```

```rust
// In MailSync::refresh_now:
pub async fn refresh_now(self: Arc<Self>) {
    let (tx, rx) = oneshot::channel();
    if let Err(e) = self.refresh_tx.send(tx).await {
        tracing::warn!(error = %e, "refresh channel closed");
        return;
    }
    match tokio::time::timeout(std::time::Duration::from_secs(10), rx).await {
        Ok(Ok(Ok(payload))) => self.handle_startup_query_reply(payload.as_deref()).await,
        Ok(Ok(Err(e))) => tracing::warn!(error = %e, "refresh root query failed"),
        Ok(Err(_)) => tracing::warn!("refresh reply channel dropped"),
        Err(_) => tracing::warn!("refresh root query timed out"),
    }
}
```

In `event_loop.rs`, add a new arm to the main `select!`:

```rust
Some(reply_tx) = refresh_rx.recv() => {
    let session = session.clone();
    let own_root_key = own_root_key.clone();
    tokio::spawn(async move {
        let result = async {
            let replies = session.get(&own_root_key).await.map_err(|e| format!("get: {e}"))?;
            let mut payload: Option<Vec<u8>> = None;
            while let Ok(reply) = replies.recv_async().await {
                if let Ok(sample) = reply.result() {
                    payload = Some(sample.payload().to_bytes().to_vec());
                    break;
                }
            }
            Ok::<Option<Vec<u8>>, String>(payload)
        }.await;
        let _ = reply_tx.send(result);
    });
}
```

The `refresh_tx`/`refresh_rx` pair is created in `lib.rs` (Task C13) alongside `fetch_tx`/`fetch_rx`.

- [ ] **Step 3: Verify compilation**

```bash
cargo check -p harmony-client
```

Expected: clean. (If `refresh_tx` isn't yet wired in lib.rs, comment out the new select! arm temporarily — re-enable in C13.)

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/event_loop.rs src-tauri/src/mail_sync.rs
git commit -m "$(cat <<'EOF'
feat(event-loop): startup query + refresh channel for mail root

Cold-start: 10s-budget Zenoh get against /root, feeds reply (or empty)
to MailSync::handle_startup_query_reply.

Manual refresh: new refresh_rx channel arm in event_loop's select!
performs the same query on demand from MailSync::refresh_now.

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C13: Wire MailSync into Tauri state in `lib.rs`

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Construct MailSync at app startup**

Find where `MailManager` is constructed and stored in Tauri-managed state (search for `mail::MailManager::load`). After it's constructed, build `MailSync`:

```rust
// Existing:
let mail_mgr = std::sync::Arc::new(std::sync::Mutex::new(
    mail::MailManager::load(&app_data_dir.join("mail"), our_addr_bytes),
));

// NEW: refresh channel for MailSync.refresh_now → event_loop.
let (refresh_tx, refresh_rx) = tokio::sync::mpsc::channel(8);

// NEW: build MailSync.
let own_addr_hex = hex::encode(our_addr_bytes);
let mail_sync = std::sync::Arc::new(mail_sync::MailSync::new(
    fetch_tx.clone(),
    refresh_tx,
    std::sync::Arc::clone(&mail_mgr),
    own_addr_hex,
    app.handle().clone(),
));

// Pass mail_sync + refresh_rx into event_loop spawn.
// ... existing event_loop spawn modified to include both ...

// Tauri state:
app.manage(mail_mgr);  // existing
app.manage(std::sync::Arc::clone(&mail_sync));  // NEW
```

Update `MailSync::new` signature in `mail_sync.rs` to accept `refresh_tx`:

```rust
pub fn new(
    fetch_tx: mpsc::Sender<FetchRequest>,
    refresh_tx: mpsc::Sender<oneshot::Sender<Result<Option<Vec<u8>>, String>>>,
    mail_mgr: Arc<Mutex<MailManager>>,
    own_addr_hex: String,
    app: AppHandle,
) -> Self {
    Self {
        state: Arc::new(Mutex::new(SyncState::Idle { last_walked_root: None })),
        fetch_tx,
        refresh_tx,
        mail_mgr,
        own_addr_hex,
        app,
        in_flight_bodies: Arc::new(Mutex::new(HashMap::new())),
    }
}
```

Update `make_test_mail_sync` in tests to pass a dummy refresh_tx:

```rust
fn make_test_mail_sync(...) -> Arc<MailSync> {
    let (refresh_tx, _refresh_rx) = mpsc::channel(1);
    // ...
}
```

- [ ] **Step 2: Verify it compiles and tests still pass**

```bash
cargo build -p harmony-client
cargo test -p harmony-client mail_sync::tests
```

Expected: clean compile, all mail_sync tests still pass.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/mail_sync.rs
git commit -m "$(cat <<'EOF'
feat(lib): construct MailSync at startup, manage in Tauri state

Wires the refresh channel between MailSync and event_loop so the
manual refresh button can trigger a Zenoh root query. Stores MailSync
in Tauri-managed state so commands can call into it.

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C14: Add `refresh_mail` Tauri command

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Add the command**

```rust
#[tauri::command]
async fn refresh_mail(
    sync: tauri::State<'_, std::sync::Arc<mail_sync::MailSync>>,
) -> Result<(), String> {
    std::sync::Arc::clone(&sync).refresh_now().await;
    Ok(())
}
```

Register it in the `.invoke_handler(tauri::generate_handler![...])` list.

- [ ] **Step 2: Verify compilation**

```bash
cargo check -p harmony-client
```

Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(commands): add refresh_mail Tauri command

Manual refresh button in MailInbox triggers this command, which calls
MailSync::refresh_now to re-query the gateway for the current root CID.

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C15: Add `fetch_mail_body` Tauri command + update `get_mail`

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Add `fetch_mail_body`**

```rust
#[tauri::command]
async fn fetch_mail_body(
    cid: String,
    sync: tauri::State<'_, std::sync::Arc<mail_sync::MailSync>>,
    mgr: tauri::State<'_, std::sync::Arc<std::sync::Mutex<mail::MailManager>>>,
) -> Result<mail::MailDetail, String> {
    // Validate CID hex.
    let cid_bytes = hex::decode(&cid).map_err(|e| format!("bad cid hex: {e}"))?;
    let cid_arr: [u8; 32] = cid_bytes.try_into()
        .map_err(|_| "cid must be 32 bytes".to_string())?;

    // Trigger lazy fetch (no-op if already Local).
    std::sync::Arc::clone(&sync).fetch_body(cid_arr).await?;

    // Return the now-Local detail.
    let mgr = mgr.lock().map_err(|e| format!("mgr poisoned: {e}"))?;
    mgr.get_message(&cid)
}
```

Register in the `.invoke_handler(...)` list.

- [ ] **Step 2: Verify `get_mail` already exposes `body_state`**

`MailDetail` was updated in C2 to include `body_state`. Confirm `get_mail` returns this — it should automatically via the struct. If `get_mail` builds the `MailDetail` manually somewhere (vs. delegating to `mgr.get_message`), update it to set `body_state` from the entry record (look up the entry in the index, copy its `body_state`).

For `Pending` entries, `mgr.get_message(cid)` reads the blob from disk — but the blob doesn't exist yet for Pending. So `get_mail` for a Pending entry should NOT call `get_message`; it should return a `MailDetail` with empty body fields and `body_state: Pending`. Modify `get_mail`:

```rust
#[tauri::command]
fn get_mail(
    cid: String,
    mgr: tauri::State<'_, std::sync::Arc<std::sync::Mutex<mail::MailManager>>>,
) -> Result<mail::MailDetail, String> {
    let mgr = mgr.lock().map_err(|e| format!("mgr poisoned: {e}"))?;

    // Look up entry to check body_state.
    let entry = ["inbox", "trash", "drafts", "sent"]
        .iter()
        .filter_map(|f| mgr.list_folder(f, 0, 1000).into_iter().find(|e| e.message_cid == cid))
        .next()
        .ok_or_else(|| "message not found".to_string())?;

    if entry.body_state == mail::BodyState::Pending {
        // Return a stub MailDetail; frontend triggers fetch_mail_body.
        return Ok(mail::MailDetail {
            message_cid: cid,
            message_id: entry.message_id,
            subject: entry.subject_snippet,
            body: String::new(),
            sender_address: entry.sender_address,
            recipients: vec![],
            timestamp: entry.timestamp,
            attachments: vec![],
            is_reply: false,
            is_forward: false,
            in_reply_to: None,
            body_state: mail::BodyState::Pending,
        });
    }

    mgr.get_message(&cid)
}
```

- [ ] **Step 3: Verify compilation**

```bash
cargo build -p harmony-client
```

Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(commands): add fetch_mail_body, update get_mail for Pending entries

fetch_mail_body triggers MailSync::fetch_body (lazy CAS pull + hash
verify + persist) and returns the resulting MailDetail.

get_mail short-circuits for Pending entries: returns a stub MailDetail
with body_state=Pending (instead of failing on the missing blob), so
the frontend can render the inbox row + trigger fetch on MailReader open.

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C16: `mail-service.ts` — sync state listener + getMessage wrapper

**Files:**
- Modify: `src/lib/mail-service.ts`

- [ ] **Step 1: Add sync state and listener**

In `mail-service.ts`, add to the class state and constructor:

```ts
syncState: 'idle' | 'syncing' | 'error' = $state('idle');
syncError: string | null = $state(null);
```

In the constructor (or wherever existing event listeners like `mail-received` are registered):

```ts
listen<{ state: string; error?: string }>('mail-sync-status', (e) => {
    this.syncState = e.payload.state as 'idle' | 'syncing' | 'error';
    this.syncError = e.payload.error ?? null;
});
```

- [ ] **Step 2: Add `refresh()` method**

```ts
async refresh(): Promise<void> {
    await invoke('refresh_mail');
}
```

- [ ] **Step 3: Wrap `getMessage` for lazy body fetch**

Find the existing `getMessage` method. Wrap or modify so that when `body_state === 'pending'`, it invokes `fetch_mail_body`:

```ts
async getMessage(cid: string): Promise<MailDetail> {
    const detail = await invoke<MailDetail>('get_mail', { cid });
    if (detail.bodyState === 'pending') {
        return await invoke<MailDetail>('fetch_mail_body', { cid });
    }
    return detail;
}
```

(Note Svelte/TypeScript convention: `body_state` from Rust serializes as `bodyState` due to `serde(rename_all = "camelCase")`.)

Update the `MailDetail` TS type to include `bodyState`:

```ts
export interface MailDetail {
    messageCid: string;
    messageId: string;
    subject: string;
    body: string;
    senderAddress: string;
    recipients: { address: string; recipientType: string }[];
    timestamp: number;
    attachments: { cid: string; filename: string; mimeType: string; size: number }[];
    isReply: boolean;
    isForward: boolean;
    inReplyTo?: string;
    bodyState: 'local' | 'pending';
}
```

- [ ] **Step 4: Manual smoke test**

```bash
cd src-tauri && cargo build -p harmony-client
cd ..
npm run tauri dev  # or whatever the dev command is
```

Open the app, observe the inbox loads, no console errors. (Full UI feedback added in C17.)

- [ ] **Step 5: Commit**

```bash
git add src/lib/mail-service.ts
git commit -m "$(cat <<'EOF'
feat(mail-service): sync state listener + lazy body fetch wrapper

syncState/syncError reactive state for the upcoming UI indicator.
mail-sync-status event listener mirrors backend state into the store.

getMessage now checks bodyState — for Pending entries, it routes through
fetch_mail_body to trigger the lazy CAS fetch.

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C17: `MailInbox.svelte` — sync indicator + refresh button

**Files:**
- Modify: `src/lib/components/MailInbox.svelte`

- [ ] **Step 1: Add sync controls to header**

In the header section of `MailInbox.svelte` (next to the existing folder tabs), insert:

```svelte
<div class="sync-controls">
    {#if mailService.syncState === 'syncing'}
        <span class="sync-spinner" title="Syncing mailbox…">⟳</span>
    {:else if mailService.syncState === 'error'}
        <button
            class="sync-error-icon"
            title={mailService.syncError ?? 'Sync error'}
            onclick={() => alert(mailService.syncError ?? 'Sync error')}
        >
            ⚠
        </button>
    {/if}
    <button
        class="sync-refresh-btn"
        onclick={() => mailService.refresh()}
        title="Refresh mailbox"
    >
        ⟳
    </button>
</div>
```

Add minimal CSS in the existing `<style>` block:

```css
.sync-controls {
    display: inline-flex;
    align-items: center;
    gap: 0.25rem;
    margin-left: auto;
}
.sync-spinner {
    display: inline-block;
    animation: spin 1.5s linear infinite;
    color: var(--text-muted, #888);
}
@keyframes spin {
    from { transform: rotate(0deg); }
    to { transform: rotate(360deg); }
}
.sync-error-icon {
    color: #c00;
    background: none;
    border: none;
    cursor: pointer;
}
.sync-refresh-btn {
    background: none;
    border: 1px solid var(--border, #ccc);
    border-radius: 4px;
    padding: 2px 6px;
    cursor: pointer;
}
.sync-refresh-btn:hover {
    background: var(--hover-bg, #f0f0f0);
}
```

(Visual treatment can be polished — these are the minimum primitives.)

- [ ] **Step 2: Manual smoke test**

```bash
npm run tauri dev
```

- Confirm refresh button appears in the inbox header.
- Click it; verify the backend logs show `refresh_mail` command invoked.
- (Sync indicator visibility tested when actual sync runs in C19 integration test.)

- [ ] **Step 3: Commit**

```bash
git add src/lib/components/MailInbox.svelte
git commit -m "$(cat <<'EOF'
feat(ui): add sync indicator + refresh button to MailInbox header

- Spinner shown while syncState === 'syncing'.
- Error icon (clickable for tooltip) when syncState === 'error'.
- Refresh button always visible — triggers mailService.refresh().

Minimal CSS — visual polish can iterate.

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C18: `MailReader.svelte` — async body load

**Files:**
- Modify: `src/lib/components/MailReader.svelte`

- [ ] **Step 1: Refactor body load to async**

Replace the existing prop-receiving render with an effect-driven async load:

```svelte
<script lang="ts">
    import { mailService } from '../mail-service';
    import type { MailDetail } from '../mail-service';

    let { cid }: { cid: string } = $props();

    let detail = $state<MailDetail | null>(null);
    let loading = $state(true);
    let error = $state<string | null>(null);

    $effect(() => {
        loading = true;
        error = null;
        detail = null;
        mailService.getMessage(cid)
            .then((d) => { detail = d; loading = false; })
            .catch((e) => { error = String(e); loading = false; });
    });
</script>

{#if loading}
    <div class="reader-loading">
        <span class="spinner">⟳</span> Loading message…
    </div>
{:else if error}
    <div class="reader-error">
        Failed to load message: {error}
        <button onclick={() => { /* re-trigger by re-setting cid */ }}>Retry</button>
    </div>
{:else if detail}
    <!-- existing render markup using detail.subject, detail.body, etc. -->
    <article class="mail-reader">
        <header>
            <h2>{detail.subject}</h2>
            <div class="meta">
                <span class="from">{detail.senderAddress}</span>
                <span class="date">{new Date(detail.timestamp * 1000).toLocaleString()}</span>
            </div>
        </header>
        <div class="body">{detail.body}</div>
        {#if detail.attachments.length > 0}
            <ul class="attachments">
                {#each detail.attachments as att}
                    <li>{att.filename} ({att.mimeType}, {att.size} bytes)</li>
                {/each}
            </ul>
        {/if}
    </article>
{/if}
```

(Adapt the markup to whatever the existing MailReader looked like — preserve all current styling and interaction; just gate behind the async load.)

- [ ] **Step 2: Smoke test**

```bash
npm run tauri dev
```

- Click on an existing inbox message (Local) — should render immediately as before.
- (Pending entries don't yet exist until C19 integration runs end-to-end; tested manually after.)

- [ ] **Step 3: Commit**

```bash
git add src/lib/components/MailReader.svelte
git commit -m "$(cat <<'EOF'
feat(ui): MailReader async body load for Pending entries

When the user opens a message, MailReader awaits getMessage which
short-circuits for Local entries (instant) or triggers fetch_mail_body
for Pending entries (lazy CAS fetch).

Loading and error states render appropriately. Existing render markup
preserved for the body/recipients/attachments sections.

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C19: Integration test — in-process Zenoh end-to-end

**Files:**
- Create: `src-tauri/tests/mail_sync_integration.rs`

- [ ] **Step 1: Write the integration test**

```rust
//! End-to-end sync test: stub gateway publishes a root + serves CAS blobs
//! via Zenoh queryables; client MailSync walks the tree and registers entries.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use harmony_mailbox::mailbox::{FolderKind, MailFolder, MailPage, MailRoot, MessageEntry};
use harmony_mailbox::message::{HarmonyMessage};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn end_to_end_walks_tree_and_lazy_fetches_body() {
    // ── Build a small mailbox: 1 message, 1 page, 1 folder, 1 root ──
    let msg = HarmonyMessage::email([0xAA; 16], vec![], "test subject", "test body").unwrap();
    let msg_bytes = msg.to_bytes();
    let msg_cid = *blake3::hash(&msg_bytes).as_bytes();

    let entry = MessageEntry {
        message_cid: msg_cid,
        message_id: msg.message_id,
        sender_address_hash: msg.sender_address,
        timestamp: msg.timestamp,
        read: false,
        subject_snippet: "test subject".to_string(),
    };
    let page = MailPage::with_entries(vec![entry], None);
    let page_bytes = page.to_bytes();
    let page_cid = *blake3::hash(&page_bytes).as_bytes();

    let folder = MailFolder::with_pages(vec![page_cid], 1, 1);
    let folder_bytes = folder.to_bytes();
    let folder_cid = *blake3::hash(&folder_bytes).as_bytes();

    let mut root = MailRoot::new([0u8; 16]);
    root.set_folder(FolderKind::Inbox, folder_cid);
    let root_bytes = root.to_bytes();
    let root_cid = *blake3::hash(&root_bytes).as_bytes();

    // ── Bring up Zenoh peer-to-peer session ──
    let session = zenoh::open(zenoh::Config::default()).await.unwrap();

    // Stub gateway: register CAS queryable and root publisher.
    let blobs: Arc<Mutex<HashMap<[u8; 32], Vec<u8>>>> = Arc::new(Mutex::new(HashMap::from([
        (root_cid, root_bytes.clone()),
        (folder_cid, folder_bytes.clone()),
        (page_cid, page_bytes.clone()),
        (msg_cid, msg_bytes.clone()),
    ])));

    let blobs_clone = Arc::clone(&blobs);
    let cas_session = session.clone();
    tokio::spawn(async move {
        let q = cas_session.declare_queryable("harmony/content/*/*").await.unwrap();
        while let Ok(query) = q.recv_async().await {
            let key = query.key_expr().as_str().to_string();
            let cid_hex = key.rsplit('/').next().unwrap_or("");
            let Ok(cid_bytes) = hex::decode(cid_hex) else { continue };
            let Ok(cid_arr) = <[u8; 32]>::try_from(cid_bytes.as_slice()) else { continue };
            let bytes = blobs_clone.lock().unwrap().get(&cid_arr).cloned();
            match bytes {
                Some(b) => { let _ = query.reply(&key, b).await; }
                None => { let _ = query.reply_err("not found").await; }
            }
        }
    });

    // Publish the root CID (live push path).
    tokio::time::sleep(Duration::from_millis(100)).await;  // let queryable register
    let own_addr_hex = hex::encode([0u8; 16]);
    session.put(&format!("harmony/mail/v1/{own_addr_hex}/root"), &root_cid[..]).await.unwrap();

    // ── Bring up MailSync ──
    use harmony_client::{mail::MailManager, mail_sync::{MailSync, FetchRequest}};

    let tmp = tempfile::tempdir().unwrap();
    let mail_mgr = Arc::new(Mutex::new(MailManager::load(&tmp.path().join("mail"), [0u8; 16])));

    let (fetch_tx, mut fetch_rx) = mpsc::channel::<FetchRequest>(16);
    let (refresh_tx, _refresh_rx) = mpsc::channel::<oneshot::Sender<Result<Option<Vec<u8>>, String>>>(8);

    // Bridge fetch_rx → real Zenoh `get` against harmony/content/*/*.
    let fetch_session = session.clone();
    tokio::spawn(async move {
        while let Some(req) = fetch_rx.recv().await {
            let session = fetch_session.clone();
            tokio::spawn(async move {
                let prefix = req.cid_hex.get(1..2).unwrap_or("");
                let key = format!("harmony/content/{prefix}/{}", req.cid_hex);
                let result = match session.get(&key).await {
                    Ok(replies) => {
                        let mut bytes = None;
                        while let Ok(reply) = replies.recv_async().await {
                            if let Ok(sample) = reply.result() {
                                bytes = Some(sample.payload().to_bytes().to_vec());
                                break;
                            }
                        }
                        bytes.ok_or_else(|| "empty reply".to_string())
                    }
                    Err(e) => Err(format!("get: {e}")),
                };
                let _ = req.reply.send(result);
            });
        }
    });

    let app = tauri::test::mock_app();
    let sync = Arc::new(MailSync::new(
        fetch_tx, refresh_tx, Arc::clone(&mail_mgr), own_addr_hex.clone(), app.handle().clone(),
    ));

    // Trigger the walk via root push.
    Arc::clone(&sync).handle_root_push(&root_cid).await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ── Assert: walker registered the entry as Pending ──
    let inbox = mail_mgr.lock().unwrap().list_folder("inbox", 0, 100);
    assert_eq!(inbox.len(), 1, "walker should register 1 entry");
    assert_eq!(inbox[0].body_state, harmony_client::mail::BodyState::Pending);

    // ── Trigger lazy body fetch ──
    let result = Arc::clone(&sync).fetch_body(msg_cid).await.unwrap();
    assert_eq!(result, msg_bytes);

    // ── Assert: entry promoted to Local ──
    let inbox = mail_mgr.lock().unwrap().list_folder("inbox", 0, 100);
    assert_eq!(inbox[0].body_state, harmony_client::mail::BodyState::Local);
    let blob_path = tmp.path().join("mail").join("blobs").join(format!("{}.bin", hex::encode(msg_cid)));
    assert!(blob_path.exists(), "body blob should be persisted");
}
```

> **NOTE on test crate visibility:** `harmony-client` may need its internal modules exposed for integration tests. Add to `src-tauri/Cargo.toml` if not present:
> ```toml
> [lib]
> name = "harmony_client"
> path = "src/lib.rs"
> ```
> And ensure `mail`, `mail_sync` modules are `pub mod` in `lib.rs` (for the integration test only — internal use can stay `mod`).

> **NOTE on Zenoh shard prefix:** the existing client uses `req.cid_hex.get(1..2)` for the prefix. Verify the integration test stub queryable matches — currently uses `harmony/content/*/*`, which should match the client's `harmony/content/{prefix}/{cid_hex}` pattern.

- [ ] **Step 2: Run the integration test**

```bash
cd src-tauri
cargo test -p harmony-client --test mail_sync_integration -- --nocapture
```

Expected: PASS. Allow up to 30s for Zenoh setup + multiple round trips.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/tests/mail_sync_integration.rs src-tauri/Cargo.toml src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
test(mail-sync): end-to-end integration test with in-process Zenoh

Spins up a real Zenoh session, registers a stub CAS queryable serving
a hand-built Merkle tree, publishes a root CID to the live topic.
Bridges MailSync's fetch channel through real Zenoh gets.

Asserts:
- Walker registers the message as a Pending inbox entry.
- Lazy fetch_body retrieves bytes, promotes entry to Local, persists blob.

Exposes mail and mail_sync modules as pub for integration test access.

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C20: Frontend Vitest tests

**Files:**
- Create or extend: `src/lib/mail-service.test.ts`

- [ ] **Step 1: Write tests**

```ts
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { MailService } from './mail-service';

// Mock @tauri-apps/api/core's invoke and event's listen.
vi.mock('@tauri-apps/api/core', () => ({
    invoke: vi.fn(),
}));
vi.mock('@tauri-apps/api/event', () => {
    const listeners: Record<string, Function> = {};
    return {
        listen: vi.fn((event: string, cb: Function) => {
            listeners[event] = cb;
            return Promise.resolve(() => delete listeners[event]);
        }),
        // Helper for tests to fire events.
        __fire: (event: string, payload: unknown) => listeners[event]?.({ payload }),
    };
});

import { invoke } from '@tauri-apps/api/core';
import * as eventApi from '@tauri-apps/api/event';

describe('MailService Phase 2', () => {
    let service: MailService;

    beforeEach(() => {
        vi.clearAllMocks();
        service = new MailService();
    });

    it('mirrors mail-sync-status event into syncState', () => {
        (eventApi as any).__fire('mail-sync-status', { state: 'syncing' });
        expect(service.syncState).toBe('syncing');
        expect(service.syncError).toBeNull();

        (eventApi as any).__fire('mail-sync-status', { state: 'error', error: 'boom' });
        expect(service.syncState).toBe('error');
        expect(service.syncError).toBe('boom');

        (eventApi as any).__fire('mail-sync-status', { state: 'idle' });
        expect(service.syncState).toBe('idle');
        expect(service.syncError).toBeNull();
    });

    it('refresh() calls invoke("refresh_mail")', async () => {
        await service.refresh();
        expect(invoke).toHaveBeenCalledWith('refresh_mail');
    });

    it('getMessage triggers fetch_mail_body for Pending entries', async () => {
        const stubPending = {
            messageCid: 'abc', messageId: 'xx', subject: 's', body: '',
            senderAddress: 'sender', recipients: [], timestamp: 0,
            attachments: [], isReply: false, isForward: false,
            bodyState: 'pending',
        };
        const stubLocal = { ...stubPending, body: 'fetched body', bodyState: 'local' };
        (invoke as any)
            .mockResolvedValueOnce(stubPending)  // get_mail
            .mockResolvedValueOnce(stubLocal);   // fetch_mail_body

        const result = await service.getMessage('abc');
        expect(result.bodyState).toBe('local');
        expect(result.body).toBe('fetched body');
        expect(invoke).toHaveBeenNthCalledWith(1, 'get_mail', { cid: 'abc' });
        expect(invoke).toHaveBeenNthCalledWith(2, 'fetch_mail_body', { cid: 'abc' });
    });

    it('getMessage skips fetch_mail_body for Local entries', async () => {
        const stubLocal = {
            messageCid: 'def', messageId: 'yy', subject: 's', body: 'cached',
            senderAddress: 'sender', recipients: [], timestamp: 0,
            attachments: [], isReply: false, isForward: false,
            bodyState: 'local',
        };
        (invoke as any).mockResolvedValueOnce(stubLocal);

        const result = await service.getMessage('def');
        expect(result.body).toBe('cached');
        expect(invoke).toHaveBeenCalledTimes(1);
        expect(invoke).toHaveBeenCalledWith('get_mail', { cid: 'def' });
    });
});
```

- [ ] **Step 2: Run tests**

```bash
npm run test  # or `npx vitest run src/lib/mail-service.test.ts`
```

Expected: all 4 tests PASS.

- [ ] **Step 3: Commit**

```bash
git add src/lib/mail-service.test.ts
git commit -m "$(cat <<'EOF'
test(mail-service): cover sync state listener + lazy body fetch wrapper

Vitest tests for:
- mail-sync-status event mirrored into syncState/syncError
- refresh() calls invoke("refresh_mail")
- getMessage routes Pending entries through fetch_mail_body
- getMessage skips fetch for Local entries

ZEB-114 Phase 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task C21: Manual end-to-end QA + open PR 2

- [ ] **Step 1: Manual QA against a real gateway**

If a harmony-mail gateway with PR 1 merged is reachable in dev:

1. Wipe local mail state: `rm -rf ~/.local/share/harmony-client/mail/`
2. Send a test email to the harmony-client user's address via SMTP.
3. Start harmony-client. Confirm:
   - Sync indicator briefly appears in inbox header.
   - The new email shows up in the inbox list.
   - Entry has `bodyState='pending'` (verifiable in `~/.local/share/harmony-client/mail/index.json`).
   - Click the message → MailReader shows spinner briefly, then renders body.
   - After render, `index.json` shows `bodyState='local'` for that entry.
4. Click the refresh button → confirm `refresh_mail` is invoked (look for log line).

If no gateway is reachable, document this and rely on the integration test as proof.

- [ ] **Step 2: Push branch and open PR**

```bash
cd /path/to/zeb-114-phase-2-client-walker
git push -u origin zeb-114-phase-2-client-walker
gh pr create --title "feat(mail): native Phase 2 client receive path with Merkle walker (ZEB-114)" --body "$(cat <<'EOF'
## Summary

Implements ZEB-114 Phase 2 — the harmony-client now walks the gateway-published mailbox Merkle tree to populate the inbox with the recipient's full mail history (header-only) on cold start and on every root-CID push.

**Builds on harmony PR #<PR1>** (mail root queryable) — Cargo.lock pinned to that commit.

### What's new

- **`mail_sync.rs`** (new) — walker state machine, hybrid error policy (strict for root/folder, skip for page/entry), single-flight `pending_root` semantics, lazy body fetch with in-flight dedup + BLAKE3 verification.
- **`MailManager`** gains `register_header_only` (creates Pending entry from a `MessageEntry`) and `mark_body_received` (verifies + persists body, promotes Pending → Local). `receive_message` now promotes matching Pending entries instead of rejecting as duplicate, preserving folder placement.
- **`event_loop.rs`** filter flip: `/root` events route to MailSync; subscribe to `/root` at startup; cold-start Zenoh `get` query for current root.
- **Tauri commands**: `refresh_mail`, `fetch_mail_body`. `get_mail` short-circuits for Pending entries.
- **Frontend**: sync indicator (spinner + error icon) and refresh button in MailInbox; MailReader now async with loading state.
- **Index migration**: existing index.json files load with `body_state` defaulting to `Local` via `serde(default)`.

### Out of scope (deferred)

- **Background body prefetch** — tracked as ZEB-118.
- **Bidirectional state push** (read/unread, folder moves to gateway) — tracked under ZEB-116.
- **Sent/Drafts/Trash gateway sync** — tracked under ZEB-116.

## Test plan

- [ ] `cargo test -p harmony-client mail::` passes (existing + new mail tests)
- [ ] `cargo test -p harmony-client mail_sync::` passes (8 walker + body fetch tests)
- [ ] `cargo test -p harmony-client --test mail_sync_integration` passes (end-to-end with real Zenoh)
- [ ] `npm run test` passes (Vitest frontend tests)
- [ ] `cargo build -p harmony-client` clean
- [ ] Manual: wipe local mail, deliver test email via gateway SMTP, observe inbox populates with Pending entry, click message → body fetches and renders

## Spec & plan

- Spec: `docs/specs/2026-04-14-client-mail-receive-design.md`
- Plan: `docs/plans/2026-04-14-client-mail-receive-plan.md`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Address review feedback through merge**

(Cycle through automated reviewers; respond as appropriate.)

---

## Self-review checklist (run after writing the plan)

**Spec coverage:**

| Spec section | Tasks |
|---|---|
| Wire & CAS contracts (queryable) | G1, G2, G3 |
| harmony-mailbox parser dependence | C7, C8 (uses parsers directly) |
| Single-folder scope (Inbox only) | C7 (`folder(FolderKind::Inbox)`) |
| `mail_sync.rs` module + state machine | C6, C7, C8, C9 |
| `register_header_only` | C3 |
| `mark_body_received` | C4 |
| `receive_message` Pending→Local promotion | C5 |
| `BodyState` enum + index migration | C2 |
| event_loop filter flip + own_root_key | C11 |
| Startup query | C12 |
| MailSync wiring into Tauri state | C13 |
| Tauri commands `refresh_mail`/`fetch_mail_body` | C14, C15 |
| `get_mail` exposes body_state | C15 |
| `mail-service.ts` sync state + getMessage wrapper | C16 |
| `MailInbox.svelte` sync indicator + refresh | C17 |
| `MailReader.svelte` async body load | C18 |
| Integration test | C19 |
| Frontend Vitest tests | C20 |

**Spec test scope coverage:**

| Test (from spec) | Task |
|---|---|
| `walk_empty_root` | (covered implicitly by walk_aborts_on_folder_fetch_failure if folder is empty; could add explicit) |
| `walk_single_page` | C8 |
| `walk_multi_page` | (covered partially by walk_skips_missing_page_continues_others; could add explicit happy-path multi-page) |
| `dedup_against_local` | C8 (register_header_only returns Duplicate if local entry exists with same message_id — covered via C3's `register_header_only_returns_duplicate_for_existing_inbox_message_id` test) |
| `root_fetch_404` | C7 |
| `folder_fetch_404` | C7 |
| `page_fetch_404` | C8 |
| `entry_parse_error` | (not covered — MailPage parser would reject the whole page, currently treated as page skip; explicit parse-error-mid-page test could be added but adds limited value over existing skip path) |
| `pending_root_during_walk` | C9 |
| `body_fetch_dedup` | C10 |
| `body_fetch_invalid_hash` | C10 |
| `register_header_only_new` | C3 |
| `register_header_only_dedup` | C3 |
| `mark_body_received_pending_to_local` | C4 |
| `mark_body_received_already_local` | C4 (`is_idempotent_for_local_entries`) |
| `mark_body_received_hash_mismatch` | C4 |
| `index_migration_old_format` | C2 |
| `receive_message_promotes_pending` | C5 |
| `root_queryable_returns_current_root` | G1 |
| `root_queryable_empty_for_unknown_addr` | G2 |
| `root_queryable_after_multiple_deliveries` | G2 + G3 (strict version) |
| Integration test | C19 |
| Frontend tests | C20 |

**Placeholder scan:** Plan contains TWO `TODO(C12)` markers in C6's stub `refresh_now` — these are intentional, marking where C12 will replace stub code. Acceptable because they reference a specific task that does the work. No `TBD`, `fill in`, or "add appropriate X" markers.

**Type consistency:**
- `EntryRecord` (not `MailEntry`) used consistently after C2 introduction.
- `BodyState::Local` / `BodyState::Pending` consistent across Rust + frontend (`'local'` / `'pending'`).
- `MessageEntry` field names assumed (`message_cid`, `message_id`, `sender_address_hash`, `timestamp`, `read`, `subject_snippet`) — flagged as "verify in C3 step 4" since I haven't read the actual harmony-mailbox source.
- `RegisterOutcome::Inserted { cid: String }` consistent.
- `FetchRequest { cid_hex: String, reply: oneshot::Sender<...> }` consistent across C6, C8, C10, C19.
- `mail-sync-status` event payload `{ state, error? }` consistent in Rust emit (C7) + TS listener (C16) + tests (C20).

**Open assumption to verify during implementation:** the harmony-mailbox crate must expose constructors like `MailRoot::new`, `MailFolder::with_pages`, `MailPage::with_entries` — if not, tests in C7-C9 will need to construct these via whatever public API exists, or the harmony-mailbox crate may need small additions (which would be a separate task in PR 2 or PR 1).
