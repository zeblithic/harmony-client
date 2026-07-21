# ZEB-723 — Grantee "Shared with me" surface — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give a grantee a Files section that lists the encrypted files others have shared with them, each with a Download (decrypt-to-disk) action, plus an unread badge when new shares arrive.

**Architecture:** ZEB-674 (PR #512) already shipped the entire grantee backend — `received_file_grants`, `ingest_grant_push`, and decrypt-on-read (`fetch_content`/`export_content` fetch ciphertext from the network then decrypt via `decrypt_personal_file_if_held`). This plan adds one read-only list IPC (`list_received_grants`), one arrival event (`shared-with-me-updated`), and the frontend surface: a third `ContentSection` tab rendering a new `SharedWithMeList.svelte`.

**Tech Stack:** Rust (Tauri IPC, tokio, serde), Svelte 5 (runes), TypeScript, vitest, cargo-nextest.

## Global Constraints

- **Client-only.** No `harmony` crate change → no rev-bump → single PR.
- **Honesty (tri-valued fetch state), everywhere the received list is rendered:** `null` = unresolved (loading / not yet queried — render a neutral placeholder, NEVER an empty-state message); `[]` = proven-empty ("Nothing has been shared with you yet"); populated = rows. A load **failure** stays `null` — a `catch` must never fabricate `[]`. (Mirrors the ZEB-674 ShareList `grants: FileGrant[] | null` contract and Gap G-3.)
- **Tauri IPC naming:** Rust params `snake_case`; JS callers `camelCase`. DTOs use `#[serde(rename_all = "camelCase")]` (matches `FileGrantDto`).
- **Tauri IPC error extraction:** `const msg = e instanceof Error ? e.message : String(e);` (CLAUDE.md).
- **Register the new IPC command ONLY in the production handler** (`lib.rs` `tauri::generate_handler![` at ~line 64057, alongside `list_grants,` at ~64102). Do NOT add it to `add_dm_ipc_handlers` (the `#[cfg(any(test, feature = "test-fixtures"))]` test-only helper at ~64463) — `list_grants` isn't there either.
- **Keychain isolation (ZEB-428):** all new Rust tests build `OwnerState` / `NodeState` directly (no mint, no `KeychainStore::new()`) — safe by avoidance, exactly like `tests/file_sharing_grants.rs` and the existing `file_share_ipc_tests`.
- **`ReceivedGrantDto` is NOT a canonical-CBOR wire type** — it is a frontend projection (serde JSON camelCase), never `canonical_cbor_encode`d — so the same-length-key rule that governs `ReceivedFileGrant`/`GrantEntry` does NOT apply to it.
- **Scope:** download-only (no in-app preview), badge-only (no toast). No grantee-side dismiss/hide.
- **Gates (all must pass before PR):** from `src-tauri/`: `cargo fmt --all -- --check` · `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` · `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. From repo root: `npx tsc --noEmit` · `npx vitest run`. During iterative dev use `scripts/test-select --context task`, and paste its printed `round=… bucket=…` summary line into the task report so the selection is auditable; the final pre-PR sweep is the full commands above.

---

## File Structure

**Backend:**
- Modify `src-tauri/src/file_sharing.rs` — add `ReceivedGrantDto` + pure `list_received_grants_inner`.
- Modify `src-tauri/src/lib.rs` — add `list_received_grants_impl` + `#[tauri::command] list_received_grants` + register in the production handler.
- Modify `src-tauri/src/dm_inbox_ingest.rs` — emit `shared-with-me-updated` from `apply_grant_push` on a genuine new record.

**Frontend:**
- Modify `src/lib/types.ts` — `ContentSection += 'sharedWithMe'`; add `ReceivedFile` + `ReceivedGrantWire`.
- Modify `src/lib/file-manager-service.ts` — `listReceivedGrants()`, `exportReceived()`, pure `unreadReceivedCount()`.
- Create `src/lib/components/SharedWithMeList.svelte` + `src/lib/components/SharedWithMeList.test.ts`.
- Modify `src/lib/components/BrowserToolbar.svelte` — third "Shared with me" tab + unread badge.
- Modify `src/lib/components/FileBrowser.svelte` — third section branch + two new props.
- Modify `src/App.svelte` — received-files state, load-on-section-open (race-guarded), event listener, unread badge (localStorage), download handler, prop wiring.

---

### Task 1: Backend — `ReceivedGrantDto` + pure `list_received_grants_inner`

**Files:**
- Modify: `src-tauri/src/file_sharing.rs` (add DTO + helper next to `FileGrantDto`/`list_grants_inner`, ~lines 338–490; add tests to the existing `#[cfg(test)] mod` at the bottom of the file)

**Interfaces:**
- Consumes: `OwnerState` (`state.received_file_grants: BTreeMap<[u8;32], ReceivedFileGrant>`, `state.friend_graph.friends`); `ReceivedFileGrant { granter_owner, cid, file_name, file_size, mime, sealed_dek, received_at }`.
- Produces: `pub struct ReceivedGrantDto` and `pub fn list_received_grants_inner(state: &OwnerState) -> Vec<ReceivedGrantDto>` — consumed by Task 2.

- [ ] **Step 1: Write the failing test**

Add to the existing `#[cfg(test)] mod tests` in `file_sharing.rs` (the module that already imports `OwnerState`, `OwnerAddr`, `ReceivedFileGrant`; add any missing imports). This test builds an `OwnerState` directly (no mint — keychain-safe):

```rust
#[test]
fn list_received_grants_inner_projects_sorts_and_resolves_names() {
    use crate::friend_graph::{FriendEntry, FriendOrigin, FriendStatus};

    let granter_friend = OwnerAddr([0x11; 16]);
    let granter_stranger = OwnerAddr([0x22; 16]);
    let cid_a = [0xAA; 32];
    let cid_b = [0xBB; 32];

    let mut state = OwnerState::default();
    // A friend granter → display name resolves.
    state.friend_graph.friends.insert(
        granter_friend,
        FriendEntry {
            status: FriendStatus::Active,
            display: Some("Alice".to_string()),
            origin: FriendOrigin::Direct,
            ..FriendEntry::default()
        },
    );
    // Older grant from a friend.
    state.received_file_grants.insert(
        cid_a,
        ReceivedFileGrant {
            granter_owner: granter_friend,
            cid: cid_a,
            file_name: "a.pdf".into(),
            file_size: 100,
            mime: "application/pdf".into(),
            sealed_dek: vec![1, 2, 3],
            received_at: 1_000,
        },
    );
    // Newer grant from a non-friend stranger.
    state.received_file_grants.insert(
        cid_b,
        ReceivedFileGrant {
            granter_owner: granter_stranger,
            cid: cid_b,
            file_name: "b.png".into(),
            file_size: 200,
            mime: "image/png".into(),
            sealed_dek: vec![4, 5],
            received_at: 2_000,
        },
    );

    let rows = list_received_grants_inner(&state);

    // Sorted by received_at DESC → newest (cid_b) first.
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].cid, hex::encode(cid_b));
    assert_eq!(rows[0].granter_address, hex::encode(granter_stranger.0));
    assert_eq!(rows[0].display_name, None, "stranger has no friend display");
    assert_eq!(rows[0].file_name, "b.png");
    assert_eq!(rows[0].file_size, 200);
    assert_eq!(rows[0].mime, "image/png");
    assert_eq!(rows[0].received_at, 2_000);

    assert_eq!(rows[1].cid, hex::encode(cid_a));
    assert_eq!(rows[1].display_name.as_deref(), Some("Alice"));

    // Empty map → empty vec (proven-empty).
    assert!(list_received_grants_inner(&OwnerState::default()).is_empty());
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(list_received_grants_inner_projects)'`
Expected: FAIL to compile — `ReceivedGrantDto` / `list_received_grants_inner` not found. (Confirm `FriendEntry` has the fields used; if `FriendEntry::default()` or a field name differs, adjust the test to the real shape — check `friend_graph.rs` — before implementing.)

- [ ] **Step 3: Write the DTO + helper**

In `file_sharing.rs`, immediately after the `FileGrantDto` definition (~line 350), add:

```rust
/// One row of the grantee's "Shared with me" list, projected for the frontend
/// from [`crate::owner_state_types::ReceivedFileGrant`]. Serde camelCase:
/// `cid` / `granterAddress` / `displayName` / `fileName` / `fileSize` / `mime` /
/// `receivedAt`. NOT a canonical-CBOR wire type — never `canonical_cbor_encode`d,
/// so the same-length-key rule does not apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceivedGrantDto {
    /// The shared file's encrypted root CID, hex-encoded.
    pub cid: String,
    /// The granting owner's 16-byte master `owner_id`, hex-encoded.
    pub granter_address: String,
    /// The granter's friend-graph display name; `None` when the granter is not a
    /// currently-known friend (frontend falls back to `granter_address`).
    pub display_name: Option<String>,
    /// Display file name.
    pub file_name: String,
    /// Stored (CAS) byte length — for encrypted content this includes the AEAD
    /// nonce+tag overhead (plaintext length + 28); the UI shows it as-is.
    pub file_size: u64,
    /// MIME type string.
    pub mime: String,
    /// Wall-clock ms when this grant was ingested.
    pub received_at: u64,
}

/// Project `state.received_file_grants` into DTO rows for the grantee's
/// "Shared with me" surface, newest first. The granter's `display_name` is
/// resolved from the friend graph exactly as [`list_grants_inner`] resolves the
/// grantee's (`None` when the granter is not a currently-known friend). Pure;
/// unit-testable without a live node.
pub fn list_received_grants_inner(state: &OwnerState) -> Vec<ReceivedGrantDto> {
    let mut rows: Vec<ReceivedGrantDto> = state
        .received_file_grants
        .values()
        .map(|g| ReceivedGrantDto {
            cid: hex::encode(g.cid),
            granter_address: hex::encode(g.granter_owner.0),
            display_name: state
                .friend_graph
                .friends
                .get(&g.granter_owner)
                .and_then(|f| f.display.clone()),
            file_name: g.file_name.clone(),
            file_size: g.file_size,
            mime: g.mime.clone(),
            received_at: g.received_at,
        })
        .collect();
    // Newest first; tie-break on cid for a deterministic order.
    rows.sort_by(|a, b| {
        b.received_at
            .cmp(&a.received_at)
            .then_with(|| a.cid.cmp(&b.cid))
    });
    rows
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(list_received_grants_inner_projects)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/file_sharing.rs
git commit -m "ZEB-723: ReceivedGrantDto + pure list_received_grants_inner projection"
```

---

### Task 2: Backend — `list_received_grants` IPC command + registration

**Files:**
- Modify: `src-tauri/src/lib.rs` (add `list_received_grants_impl` next to `list_grants_impl` ~20240; add `#[tauri::command] list_received_grants` next to `list_grants` ~20455; register in the production handler ~64102; add tests to the existing `#[cfg(test)] mod file_share_ipc_tests` ~20484)

**Interfaces:**
- Consumes: `list_received_grants_inner` (Task 1); `NodeState.crdt_state`; the `file_share_ipc_tests::grantable_state(store)` helper.
- Produces: IPC command `list_received_grants` → `Vec<ReceivedGrantDto>` (JSON camelCase over the Tauri boundary), consumed by the frontend (Task 4).

- [ ] **Step 1: Write the failing tests**

Add to `mod file_share_ipc_tests` in `lib.rs`:

```rust
#[tokio::test]
async fn list_received_grants_impl_errors_pre_mint() {
    let state = Mutex::new(NodeState::default());
    let err = list_received_grants_impl(&state)
        .await
        .expect_err("no owner loaded pre-mint");
    assert_eq!(err, "no owner loaded");
}

#[tokio::test]
async fn list_received_grants_impl_projects_received_map() {
    use crate::owner_state_types::ReceivedFileGrant;
    let store = std::sync::Arc::new(RecordingStore::default());
    let state = grantable_state(store);
    let cid = [0x5C; 32];
    {
        let crdt = state.lock().unwrap().crdt_state.clone().unwrap();
        crdt.lock().await.received_file_grants.insert(
            cid,
            ReceivedFileGrant {
                granter_owner: crate::owner_state_types::OwnerAddr([0x77; 16]),
                cid,
                file_name: "shared.txt".into(),
                file_size: 42,
                mime: "text/plain".into(),
                sealed_dek: vec![9, 9, 9],
                received_at: 1_234,
            },
        );
    }
    let rows = list_received_grants_impl(&state).await.expect("list ok");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].cid, hex::encode(cid));
    assert_eq!(rows[0].file_name, "shared.txt");
    assert_eq!(rows[0].received_at, 1_234);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(list_received_grants_impl)'`
Expected: FAIL — `list_received_grants_impl` not found.

- [ ] **Step 3: Add the impl + command + registration**

In `lib.rs`, after `list_grants_impl` (~20254), add:

```rust
/// `list_received_grants()` core: project `received_file_grants` into DTO rows
/// (granter address + friend-resolved display + file meta + received_at) for the
/// grantee's "Shared with me" surface. Read-only; newest first. `Err("no owner
/// loaded")` pre-mint — the frontend treats any error as "unresolved", never as
/// proven-empty.
pub(crate) async fn list_received_grants_impl(
    state: &Mutex<NodeState>,
) -> Result<Vec<crate::file_sharing::ReceivedGrantDto>, String> {
    let crdt_state = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .crdt_state
            .clone()
            .ok_or_else(|| "no owner loaded".to_string())?
    };
    let st = crdt_state.lock().await;
    Ok(crate::file_sharing::list_received_grants_inner(&st))
}
```

After the `#[tauri::command] async fn list_grants` (~20460), add:

```rust
/// ZEB-723: project the grantee's "Shared with me" list.
#[tauri::command]
async fn list_received_grants(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<crate::file_sharing::ReceivedGrantDto>, String> {
    list_received_grants_impl(state.inner()).await
}
```

In the production `tauri::generate_handler![` block, add `list_received_grants,` immediately after `list_grants,` (~64102).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(list_received_grants_impl)'`
Expected: PASS (both).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "ZEB-723: list_received_grants IPC command + registration"
```

---

### Task 3: Backend — emit `shared-with-me-updated` on grant arrival

**Files:**
- Modify: `src-tauri/src/dm_inbox_ingest.rs` (`apply_grant_push` ~1272–1303; add a focused test in the file's existing `#[cfg(test)] mod` — reuse the `RecordingSink` idiom already used there ~2129)

**Interfaces:**
- Consumes: `self.sink: Arc<dyn NodeEventSink>` (struct field ~925); `crate::node_event_sink::emit_ser`; `ingest_grant_push` returns `Ok(Some(ContentId))` on a genuine new record.
- Produces: a `shared-with-me-updated` event (`{ "cid": <hex> }`) the frontend listens for (Task 6).

- [ ] **Step 1: Write the failing test**

The exact anchor exists: `sweep_ingests_real_grant_push_via_prod_ctx_device_agnostic` (~line 2190) already drives a **real** per-device-sealed `grant_push` through the production `apply_grant_push` via `ingest_pending(&mut doc, &ctx)`, using the `prod_ctx_with_dirty()` fixture, and asserts `received_file_grants` is populated (cid `[0xC1; 32]`). The only thing missing is a handle to the `RecordingSink` — the fixture builds one (`sink_handle`) but drops it.

First expose the sink handle with a **non-invasive** refactor (do NOT change the existing 4-tuple call sites): add a sibling fixture that returns the handle and have the existing one delegate to it:

```rust
// New: returns the RecordingSink handle as a 5th element.
fn prod_ctx_with_dirty_and_sink() -> (
    ProdDmInboxIngestCtx,
    Arc<Mutex<crate::owner_state_crdt::OwnerState>>,
    Arc<AtomicUsize>,
    crate::revoked_device_projection::RevokedDeviceProjection,
    Arc<crate::node_event_sink::RecordingSink>,
) {
    // ... identical body to prod_ctx_with_dirty, but also return `sink_handle`.
}

// Existing signature preserved — delegate and drop the handle so the ~5 current
// callers are untouched:
fn prod_ctx_with_dirty() -> ( /* unchanged 4-tuple */ ) {
    let (ctx, crdt, dirty, revoked, _sink) = prod_ctx_with_dirty_and_sink();
    (ctx, crdt, dirty, revoked)
}
```

Then add the emit test (sibling to the anchor, reusing its exact grant-push construction — **do not fabricate crypto**, copy the `seal_grant_for_devices` + `ciborium` encode block verbatim):

```rust
#[tokio::test]
async fn sweep_ingested_grant_emits_shared_with_me_updated() {
    use crate::file_sharing::{seal_grant_for_devices, FileGrantInner};
    let (ctx, _crdt_state, _dirty, _revoked, sink_handle) = prod_ctx_with_dirty_and_sink();

    let cid_bytes = [0xC1u8; 32];
    let inner = FileGrantInner {
        cid: cid_bytes,
        file_name: "shared.md".into(),
        file_size: 42,
        mime: "text/markdown".into(),
        dek: [0x5Au8; 32],
    };
    let sealed = seal_grant_for_devices(&inner, &[test_device_x25519_pub()]).expect("seal");
    let list: Vec<serde_bytes::ByteBuf> =
        sealed.into_iter().map(serde_bytes::ByteBuf::from).collect();
    let mut grant_push = Vec::new();
    ciborium::into_writer(&list, &mut grant_push).expect("encode grant_push");

    let granter = OwnerAddr([0xB0; 16]);
    let key = DmInboxDoc::grant_key(&granter.0, &grant_push);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    let entry = DmInboxEntry {
        sender_owner: granter.0,
        cidnotify_packet: None,
        storage_blob: Vec::new(),
        invite_packet: None,
        revocation_push: None,
        grant_push: Some(grant_push),
        deposited_at: hlc(now),
        deposited_by: "butler-device".into(),
        ingested_by: Default::default(),
    };
    let mut doc = DmInboxDoc::default();
    doc.entries.insert(key, entry);

    let changed = ingest_pending(&mut doc, &ctx).await;
    assert!(changed, "grant sweep mutated the doc");

    let frames = sink_handle.frames();
    assert!(
        frames.iter().any(|(name, payload)| name == "shared-with-me-updated"
            && payload["cid"] == serde_json::json!(hex::encode(cid_bytes))),
        "a newly recorded grant emits shared-with-me-updated with its cid; got {frames:?}"
    );
}
```

(Confirm `RecordingSink::frames()` returns `Vec<(String, serde_json::Value)>` — it's used that way in the `file_share_ipc_tests` and elsewhere in this module.)

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(grant_push)' -p harmony-app`
Expected: FAIL — no `shared-with-me-updated` frame emitted.

- [ ] **Step 3: Emit on a genuine new record**

In `apply_grant_push`, the existing code binds `recorded` (the `Option<ContentId>` from `ingest_grant_push`) and calls `notify_owner_state_dirty` when `recorded.is_some()`. Extend that same `is_some()` branch to also emit — the emit must be OUTSIDE the `crdt_state` lock (the block that produced `recorded` already dropped the guard), matching the "sink must not nest inside the held lock" rule used elsewhere in this file:

```rust
        if let Some(cid) = recorded {
            if let Some(mark) = &self.notify_owner_state_dirty {
                mark();
            }
            // ZEB-723: nudge the grantee UI to refresh "Shared with me" + bump
            // its unread badge. Gated on a genuine new record (Some) exactly like
            // notify_dirty — an idempotent re-apply (None) mutated nothing and
            // must not re-emit. Payload mirrors `grants-updated` ({ cid }); the
            // frontend re-queries `list_received_grants` rather than trusting it.
            crate::node_event_sink::emit_ser(
                self.sink.as_ref(),
                "shared-with-me-updated",
                &serde_json::json!({ "cid": hex::encode(cid.to_bytes()) }),
            );
        }
```

(Replace the existing `if recorded.is_some() { ... notify ... }` block with this `if let Some(cid) = recorded` form. Confirm `ContentId::to_bytes()` is the right accessor — it is used throughout this codebase.)

- [ ] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(grant_push)' -p harmony-app`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/dm_inbox_ingest.rs
git commit -m "ZEB-723: emit shared-with-me-updated when a grant is recorded"
```

---

### Task 4: Frontend — types, service method, badge helper

**Files:**
- Modify: `src/lib/types.ts` (`ContentSection`; add `ReceivedFile`, `ReceivedGrantWire`)
- Modify: `src/lib/file-manager-service.ts` (`listReceivedGrants`, `exportReceived`, pure `unreadReceivedCount`)
- Test: `src/lib/file-manager-service.test.ts`

**Interfaces:**
- Consumes: IPC `list_received_grants` (Task 2) → `ReceivedGrantWire[]`; IPC `export_content`.
- Produces: `ReceivedFile` type; `ContentSection` with `'sharedWithMe'`; `service.listReceivedGrants(): Promise<ReceivedFile[]>`; `service.exportReceived(cid, fileName): Promise<void>`; `unreadReceivedCount(files, lastSeenMs): number` — consumed by Tasks 5 & 6.

- [ ] **Step 1: Write the failing tests**

In `src/lib/file-manager-service.test.ts` (follow the file's existing adapter-mock idiom — a fake `TauriAdapter` with a stubbed `invoke`):

```ts
import { unreadReceivedCount } from './file-manager-service';
import type { ReceivedFile } from './types';

// --- unreadReceivedCount (pure) ---
const mk = (cid: string, receivedAt: number): ReceivedFile => ({
  cid, granterAddress: 'aa', granterDisplay: 'aa',
  fileName: 'f', fileSize: 1, mime: 'text/plain', receivedAt,
});

it('unreadReceivedCount counts files newer than lastSeen', () => {
  const files = [mk('a', 100), mk('b', 200), mk('c', 300)];
  expect(unreadReceivedCount(files, 150)).toBe(2);   // b, c
  expect(unreadReceivedCount(files, 0)).toBe(3);
  expect(unreadReceivedCount(files, 300)).toBe(0);   // strictly-newer
  expect(unreadReceivedCount([], 0)).toBe(0);
  expect(unreadReceivedCount(null, 0)).toBe(0);      // unresolved → no badge
});

// --- listReceivedGrants (maps wire → view; propagates errors) ---
it('listReceivedGrants maps the wire DTO', async () => {
  const svc = new FileManagerService();
  await svc.connectAdapter(fakeAdapter({
    list_received_grants: [{
      cid: 'ab', granterAddress: 'cd', displayName: 'Alice',
      fileName: 'q.pdf', fileSize: 9, mime: 'application/pdf', receivedAt: 5,
    }],
  }));
  const rows = await svc.listReceivedGrants();
  expect(rows).toEqual([{
    cid: 'ab', granterAddress: 'cd', granterDisplay: 'Alice',
    fileName: 'q.pdf', fileSize: 9, mime: 'application/pdf', receivedAt: 5,
  }]);
});

it('listReceivedGrants falls back to the address when displayName is null', async () => {
  const svc = new FileManagerService();
  await svc.connectAdapter(fakeAdapter({
    list_received_grants: [{
      cid: 'ab', granterAddress: 'cd', displayName: null,
      fileName: 'q.pdf', fileSize: 9, mime: 'application/pdf', receivedAt: 5,
    }],
  }));
  const rows = await svc.listReceivedGrants();
  expect(rows[0].granterDisplay).toBe('cd');
});

it('listReceivedGrants propagates an IPC error (never swallows to [])', async () => {
  const svc = new FileManagerService();
  await svc.connectAdapter(fakeAdapter({
    list_received_grants: () => { throw new Error('boom'); },
  }));
  await expect(svc.listReceivedGrants()).rejects.toThrow('boom');
});
```

Use / extend whatever adapter-mock helper the test file already defines (`fakeAdapter` above is illustrative — match the existing pattern; the file already stubs `list_content` for `connectAdapter`, so include a `list_content: []` default so connect succeeds).

- [ ] **Step 2: Run to verify they fail**

Run: `npx vitest run src/lib/file-manager-service.test.ts`
Expected: FAIL — `unreadReceivedCount` / `listReceivedGrants` / `ReceivedFile` not found.

- [ ] **Step 3: Implement**

In `src/lib/types.ts`:

```ts
export type ContentSection = 'private' | 'published' | 'sharedWithMe';

/** Wire shape of one `list_received_grants` row (serde camelCase). */
export interface ReceivedGrantWire {
  cid: string;
  granterAddress: string;
  displayName: string | null;
  fileName: string;
  fileSize: number;
  mime: string;
  receivedAt: number;
}

/** One file another owner has shared with this user (view model). */
export interface ReceivedFile {
  cid: string;
  granterAddress: string;
  /** Friend display name, or the hex address when the granter isn't a friend. */
  granterDisplay: string;
  fileName: string;
  fileSize: number;
  mime: string;
  receivedAt: number;
}
```

In `src/lib/file-manager-service.ts` — add a top-level exported pure helper (outside the class):

```ts
/** Count received files strictly newer than `lastSeenMs` (the unread badge).
 *  `null` (unresolved) → 0, so an in-flight/failed load never shows a badge. */
export function unreadReceivedCount(
  files: ReceivedFile[] | null,
  lastSeenMs: number,
): number {
  if (!files) return 0;
  return files.filter((f) => f.receivedAt > lastSeenMs).length;
}
```

Add two methods to `FileManagerService` (near `listGrants`, ~455). `listReceivedGrants` must NOT swallow a backend error to `[]` — only the no-adapter demo case returns `[]`; a real IPC rejection propagates so the App-level caller can hold `null` (unresolved):

```ts
  /** ZEB-723: lists files others have shared with this user. Backend-only —
   *  returns [] without a connected adapter (demo/test). A real IPC rejection
   *  PROPAGATES (never swallowed to []) so the caller keeps the honest
   *  unresolved (null) state on failure. */
  async listReceivedGrants(): Promise<ReceivedFile[]> {
    if (!this.adapter) return [];
    const rows = (await this.adapter.invoke('list_received_grants')) as ReceivedGrantWire[];
    return rows.map((r) => ({
      cid: r.cid,
      granterAddress: r.granterAddress,
      granterDisplay: r.displayName ?? r.granterAddress,
      fileName: r.fileName,
      fileSize: r.fileSize,
      mime: r.mime,
      receivedAt: r.receivedAt,
    }));
  }

  /** ZEB-723: download a shared file to disk. `export_content` fetches the
   *  ciphertext from the network and decrypts via `received_file_grants`
   *  (ZEB-674 T12) — the grantee read path is already complete. */
  async exportReceived(cid: string, fileName: string): Promise<void> {
    if (!this.adapter) return;
    await this.adapter.invoke('export_content', { cid, fileName });
  }
```

Add `ReceivedFile`, `ReceivedGrantWire` to the file's `import type { ... } from './types'`.

- [ ] **Step 4: Run to verify they pass**

Run: `npx vitest run src/lib/file-manager-service.test.ts` — Expected: PASS. Then `npx tsc --noEmit` — Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/lib/types.ts src/lib/file-manager-service.ts src/lib/file-manager-service.test.ts
git commit -m "ZEB-723: ReceivedFile types + listReceivedGrants/exportReceived + unread badge helper"
```

---

### Task 5: Frontend — `SharedWithMeList.svelte` component

**Files:**
- Create: `src/lib/components/SharedWithMeList.svelte`
- Create: `src/lib/components/SharedWithMeList.test.ts`

**Interfaces:**
- Consumes: `ReceivedFile` (Task 4); mirrors `ShareList.svelte`'s honesty prop idiom.
- Produces: `<SharedWithMeList files={...} onDownload={...} />` — consumed by Task 6.

- [ ] **Step 1: Write the failing test**

`src/lib/components/SharedWithMeList.test.ts` (mirror `ShareList.test.ts`'s render setup — `@testing-library/svelte`):

```ts
import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import SharedWithMeList from './SharedWithMeList.svelte';
import type { ReceivedFile } from '../types';

const file: ReceivedFile = {
  cid: 'abc', granterAddress: 'dead', granterDisplay: 'Alice',
  fileName: 'quarterly.pdf', fileSize: 4096, mime: 'application/pdf', receivedAt: 1,
};

describe('SharedWithMeList', () => {
  it('renders a neutral placeholder (NOT the empty message) while unresolved (null)', () => {
    render(SharedWithMeList, { files: null, onDownload: vi.fn() });
    expect(screen.queryByText(/nothing has been shared/i)).toBeNull();
  });

  it('renders the proven-empty message for []', () => {
    render(SharedWithMeList, { files: [], onDownload: vi.fn() });
    expect(screen.getByText(/nothing has been shared/i)).toBeTruthy();
  });

  it('renders a row per file with granter + download, and wires onDownload', async () => {
    const onDownload = vi.fn();
    render(SharedWithMeList, { files: [file], onDownload });
    expect(screen.getByText('quarterly.pdf')).toBeTruthy();
    expect(screen.getByText(/Alice/)).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: /download/i }));
    expect(onDownload).toHaveBeenCalledWith(file);
  });
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `npx vitest run src/lib/components/SharedWithMeList.test.ts`
Expected: FAIL — component does not exist.

- [ ] **Step 3: Implement the component**

`src/lib/components/SharedWithMeList.svelte` (Svelte 5 runes; `$props`; mirror `ShareList.svelte` structure + the file's existing size/relative-time formatting if a shared util exists — otherwise a minimal inline formatter):

```svelte
<script lang="ts">
  import type { ReceivedFile } from '../types';

  let {
    files,
    onDownload,
  }: {
    /** Files shared with this user. `null` until `list_received_grants`
     *  resolves — render a neutral placeholder, NOT the proven-empty message
     *  (null-until-resolved honesty; mirrors ShareList's `grants`). A load
     *  FAILURE must also leave this `null`, never `[]`. */
    files: ReceivedFile[] | null;
    onDownload: (file: ReceivedFile) => void;
  } = $props();

  function fmtSize(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  }
</script>

<section class="shared-with-me">
  {#if files === null}
    <p class="swm-placeholder" aria-busy="true">Loading…</p>
  {:else if files.length === 0}
    <p class="swm-empty">Nothing has been shared with you yet.</p>
  {:else}
    <ul class="swm-list">
      {#each files as file (file.cid)}
        <li class="swm-row">
          <div class="swm-meta">
            <span class="swm-name">{file.fileName}</span>
            <span class="swm-sub">Shared by {file.granterDisplay} · {fmtSize(file.fileSize)}</span>
          </div>
          <button class="swm-download" onclick={() => onDownload(file)}>Download</button>
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .shared-with-me { padding: 0.5rem 0; }
  .swm-placeholder, .swm-empty { color: var(--text-muted, #888); padding: 1rem; }
  .swm-list { list-style: none; margin: 0; padding: 0; }
  .swm-row {
    display: flex; align-items: center; justify-content: space-between;
    gap: 1rem; padding: 0.5rem 0.75rem; border-bottom: 1px solid var(--border, #2a2a2a);
  }
  .swm-meta { display: flex; flex-direction: column; min-width: 0; }
  .swm-name { font-weight: 600; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .swm-sub { font-size: 0.85em; color: var(--text-muted, #888); }
  .swm-download { flex: none; }
</style>
```

(Match the repo's actual CSS variable names by glancing at `ShareList.svelte`'s `<style>`; use the same tokens so it themes correctly.)

- [ ] **Step 4: Run to verify it passes**

Run: `npx vitest run src/lib/components/SharedWithMeList.test.ts` — Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/SharedWithMeList.svelte src/lib/components/SharedWithMeList.test.ts
git commit -m "ZEB-723: SharedWithMeList component (null/[]/populated honesty + Download)"
```

---

### Task 6: Frontend — tab, browser branch, and App.svelte wiring

**Files:**
- Modify: `src/lib/components/BrowserToolbar.svelte` (third section tab + unread badge)
- Modify: `src/lib/components/FileBrowser.svelte` (third section branch + two new props + pass badge to toolbar)
- Modify: `src/App.svelte` (received-files state, load-on-open effect w/ race guard, event listener, unread badge via localStorage, download handler, prop wiring)
- Test: extend `src/lib/components/BrowserToolbar` test if one exists; otherwise add a focused render test asserting the third tab + badge render and fire `onSectionChange('sharedWithMe')`.

**Interfaces:**
- Consumes: `SharedWithMeList` (Task 5); `service.listReceivedGrants`/`exportReceived`/`unreadReceivedCount` (Task 4); the `shared-with-me-updated` event (Task 3); the existing `listen(...)` + `fileManagerService.addUnlisten(...)` idiom (App.svelte ~2717) and the `refreshFileGrants` race-guard idiom (~3009).

- [ ] **Step 1: Write the failing test (toolbar tab + badge)**

Add (or extend) a BrowserToolbar render test:

```ts
import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import BrowserToolbar from './BrowserToolbar.svelte';

it('renders a Shared-with-me tab with an unread badge and fires onSectionChange', async () => {
  const onSectionChange = vi.fn();
  render(BrowserToolbar, {
    section: 'private', onSectionChange,
    // ...other required props with minimal stubs (viewMode, searchQuery, etc.);
    // copy the required set from the existing toolbar test or the prop list.
    sharedUnreadCount: 3,
  });
  const tab = screen.getByRole('button', { name: /shared with me/i });
  expect(tab).toBeTruthy();
  expect(screen.getByText('3')).toBeTruthy();      // badge
  await fireEvent.click(tab);
  expect(onSectionChange).toHaveBeenCalledWith('sharedWithMe');
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `npx vitest run src/lib/components/` (the toolbar test)
Expected: FAIL — no "Shared with me" tab / `sharedUnreadCount` prop.

- [ ] **Step 3: Implement the wiring**

**`BrowserToolbar.svelte`** — add a `sharedUnreadCount = 0` prop (typed `number`), and a third `section-btn` after "Published" (~line 53), mirroring the existing two, with a badge when `sharedUnreadCount > 0`:

```svelte
    <button
      class="section-btn"
      class:active={section === 'sharedWithMe'}
      aria-pressed={section === 'sharedWithMe'}
      onclick={() => onSectionChange('sharedWithMe')}
    >Shared with me{#if sharedUnreadCount > 0}<span class="section-badge">{sharedUnreadCount}</span>{/if}</button>
```

Add a `.section-badge` style (small pill: inline-block, rounded, accent bg, `margin-left: 0.4em`, matches the app's badge tokens if one exists elsewhere — grep for an existing badge class first and reuse it).

**`FileBrowser.svelte`** — add two props: `receivedFiles: ReceivedFile[] | null = null` and `onDownloadReceived: (file: ReceivedFile) => void` (and `sharedUnreadCount = 0`); import `SharedWithMeList` and `ReceivedFile`. Pass `sharedUnreadCount` into `<BrowserToolbar>`. Restructure the section render (~1018–1119) from the current binary `{#if section === 'private'} … {:else} <PublishedView/> {/if}` into three branches:

```svelte
  {#if section === 'private'}
    ... (unchanged private block) ...
  {:else if section === 'published'}
    <PublishedView items={publishedItems} />
  {:else}
    <SharedWithMeList files={receivedFiles} onDownload={onDownloadReceived} />
  {/if}
```

**`App.svelte`:**

State + helpers (near the ZEB-674 C5 block ~2999), importing `unreadReceivedCount` and `ReceivedFile`:

```ts
  // ── ZEB-723: "Shared with me" state ─────────────────────────────────
  let receivedFiles = $state<ReceivedFile[] | null>(null);
  let receivedFilesReq = 0;                 // monotonic staleness guard
  const SWM_LAST_SEEN_KEY = 'sharedWithMeLastSeenMs';
  let sharedUnreadCount = $state(0);

  function swmLastSeen(): number {
    return Number(localStorage.getItem(SWM_LAST_SEEN_KEY) ?? '0');
  }
  function recomputeSharedUnread() {
    sharedUnreadCount = unreadReceivedCount(receivedFiles, swmLastSeen());
  }

  async function refreshReceivedFiles(): Promise<void> {
    const req = ++receivedFilesReq;
    try {
      const rows = await fileManagerService.listReceivedGrants();
      if (req !== receivedFilesReq) return;         // stale
      receivedFiles = rows;
    } catch (err) {
      if (req !== receivedFilesReq) return;
      console.error('listReceivedGrants failed:', err);
      receivedFiles = null;                          // unresolved, NOT [] (honesty)
    }
    recomputeSharedUnread();
  }

  function markSharedSeen() {
    // Clear the badge: last-seen = the newest received_at we know about (or now).
    const newest = (receivedFiles ?? []).reduce((m, f) => Math.max(m, f.receivedAt), 0);
    localStorage.setItem(SWM_LAST_SEEN_KEY, String(Math.max(newest, Date.now())));
    recomputeSharedUnread();
  }

  async function handleDownloadReceived(file: ReceivedFile): Promise<void> {
    try {
      await fileManagerService.exportReceived(file.cid, file.fileName);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      console.error('exportReceived failed:', msg);
    }
  }
```

Load-on-open effect — when the Files section becomes `sharedWithMe`, refresh and clear the badge:

```ts
  $effect(() => {
    if (fileSection === 'sharedWithMe') {
      void refreshReceivedFiles().then(markSharedSeen);
    }
  });
```

Event listener — register alongside the existing `grants-updated` listener (~2717). Refresh the list; if the user is NOT currently viewing the section, the badge stays lit (do not mark seen):

```ts
  const unlistenSharedWithMe = await listen('shared-with-me-updated', () => {
    void refreshReceivedFiles().then(() => {
      if (fileSection === 'sharedWithMe') markSharedSeen();
    });
  });
  fileManagerService.addUnlisten(unlistenSharedWithMe);
```

Also do an initial `void refreshReceivedFiles()` once after the adapter connects (near where other post-connect state loads), so the badge can light on startup for grants that arrived while offline.

Thread the props into the `<FileBrowser>` mount (the `fileBrowser` snippet ~4055):

```svelte
      receivedFiles={receivedFiles}
      sharedUnreadCount={sharedUnreadCount}
      onDownloadReceived={handleDownloadReceived}
```

- [ ] **Step 4: Run tests + type check**

Run: `npx vitest run src/lib/components/` then `npx tsc --noEmit`
Expected: toolbar test PASS; tsc clean. (`ContentSection` now has three variants — fix any non-exhaustive `switch`/`if` the compiler flags.)

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/BrowserToolbar.svelte src/lib/components/FileBrowser.svelte src/App.svelte src/lib/components/*.test.ts
git commit -m "ZEB-723: Shared-with-me tab + browser branch + App wiring (load, event, badge, download)"
```

---

## Final gate (before PR)

Run the full CI-parity sweep (not `test-select`):

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd ..
npx tsc --noEmit
npx vitest run
```

All green → open the PR (`Closes ZEB-723.`), trigger CodeRabbit once, converge.

## Manual smoke (post-merge or in dev, not a gate)

Two nodes, friends: node A ingests an encrypted file, `grant_read`s it to B. On B: the "Shared with me" tab shows an unread badge; opening it lists the file (granter = A's display name); Download saves the decrypted plaintext. Revoke on A → the row leaves B's list after B's next sync/refresh (lazy; ZEB-725 convergence).
