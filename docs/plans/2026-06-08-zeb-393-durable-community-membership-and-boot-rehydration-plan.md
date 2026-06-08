# ZEB-393 — Durable community membership + boot rehydration — Implementation Plan

> **For agentic workers:** Execute task-by-task. Steps use checkbox (`- [ ]`) syntax. TDD: red → green → commit. Run Rust from `src-tauri/`, frontend from repo root.

**Goal:** Communities a user mints/joins survive an app restart — both by persisting owner-state on commit (Fix A) and by rehydrating the nav sidebar from persisted state at boot (Fix B).

**Architecture:** Two coordinated fixes. Fix A: `flush_now()` the owner-state `SyncEngine` after `create_community`/`redeem_invite` apply their Space. Fix B: a new `list_owner_communities` pull IPC over a pure `communities_for_nav` filter, seeded into the nav tree by `App.svelte` at boot via the idempotent `addOrUpdateNavSpace`.

**Tech Stack:** Rust (Tauri IPC, tokio), Svelte 5 + TypeScript (vitest), CBOR owner-state CRDT.

**Test commands:**
- Rust single test: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(NAME)'`
- Rust full: `cd src-tauri && cargo nextest run --locked --all-targets --features test-fixtures`
- Rust lint/fmt: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` / `cargo fmt --all`
- Frontend: `npx vitest run path` / `npx tsc --noEmit`

---

## Task 1: Rust — `communities_for_nav` pure fn + `CommunityNavDto` (TDD)

**Files:**
- Modify: `src-tauri/src/lib.rs` — add DTO + fn near the other community IPCs (just before `list_community_members`, ~lib.rs:13900), and a `#[cfg(test)] mod zeb393_communities_for_nav_tests`.

- [ ] **Step 1: Write the failing test**

Add at the end of `lib.rs` (or near the community IPC tests):

```rust
#[cfg(test)]
mod zeb393_communities_for_nav_tests {
    use super::*;
    use crate::owner_state_crdt::OwnerState;
    use crate::owner_state_types::{EpochKey, Hlc, OwnerAddr, Space, SpaceId, SpaceKind};

    fn hlc() -> Hlc {
        Hlc { wall_ms: 100, logical: 0, device_id: "test".into() }
    }

    fn community_space(id: u8, name: &str, invite_only: bool, pending: bool, left: bool) -> Space {
        Space {
            id: SpaceId([id; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: name.into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: if left { Some(hlc()) } else { None },
            created_at: hlc(),
            updated_at: hlc(),
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: Some(EpochKey::new([7u8; 32])),
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([9u8; 16])),
            is_invite_only: Some(invite_only),
            shared_in_profile: false,
            pending_join_at: if pending { Some(hlc()) } else { None },
        }
    }

    fn folder_space(id: u8, name: &str) -> Space {
        let mut s = community_space(id, name, false, false, false);
        s.kind = SpaceKind::Folder;
        s.current_epoch = None;
        s.current_epoch_key = None;
        s.admin_addr = None;
        s.is_invite_only = None;
        s
    }

    #[test]
    fn returns_only_live_communities_with_correct_fields() {
        let mut st = OwnerState::default();
        for s in [
            community_space(1, "Open Town", false, false, false),
            community_space(2, "Secret Club", true, true, false), // pending invite-only
            community_space(3, "Left Behind", false, false, true), // left → excluded
            folder_space(4, "Root"),                               // non-community → excluded
        ] {
            st.spaces.insert(s.id, s);
        }

        let mut got = communities_for_nav(&st);
        got.sort_by(|a, b| a.space_id.cmp(&b.space_id));

        assert_eq!(got.len(), 2, "only the two live communities");
        assert_eq!(got[0].space_id, hex::encode([1u8; 16]));
        assert_eq!(got[0].name, "Open Town");
        assert!(!got[0].is_invite_only);
        assert!(!got[0].pending);
        assert_eq!(got[1].name, "Secret Club");
        assert!(got[1].is_invite_only);
        assert!(got[1].pending, "invite-only pending join stays greyed at boot");
    }

    #[test]
    fn empty_state_yields_empty() {
        assert!(communities_for_nav(&OwnerState::default()).is_empty());
    }
}
```

- [ ] **Step 2: Run — verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zeb393_communities_for_nav)'`
Expected: FAIL to compile — `cannot find function communities_for_nav` / `cannot find type CommunityNavDto`.

- [ ] **Step 3: Implement DTO + fn**

Add near the community IPCs in `lib.rs` (just above `async fn list_community_members`):

```rust
/// ZEB-393 Bug B: a persisted Community space shaped for the nav sidebar.
/// `space_id` is the 32-char lowercase hex of the 16-byte SpaceId (same
/// format the runtime `nav-updated` emit uses). Mirrors the frontend
/// `CommunityNavDto` in `community-service.ts`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommunityNavDto {
    pub space_id: String,
    pub name: String,
    pub is_invite_only: bool,
    pub pending: bool,
}

/// ZEB-393 Bug B: owner-state Community spaces shaped for boot rehydration
/// of the nav sidebar. Filters to live (non-left) Community spaces —
/// mirrors the boot engine-spawn sweep's predicate (`lib.rs` start_node)
/// so the UI and the engine sweep agree on "which communities am I in."
pub fn communities_for_nav(state: &crate::owner_state_crdt::OwnerState) -> Vec<CommunityNavDto> {
    state
        .spaces
        .values()
        .filter(|s| {
            s.kind == crate::owner_state_types::SpaceKind::Community && s.left_at.is_none()
        })
        .map(|s| CommunityNavDto {
            space_id: hex::encode(s.id.0),
            name: s.name.clone(),
            is_invite_only: s.is_invite_only.unwrap_or(false),
            pending: s.pending_join_at.is_some(),
        })
        .collect()
}
```

- [ ] **Step 4: Run — verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zeb393_communities_for_nav)'`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "ZEB-393: communities_for_nav pure fn + CommunityNavDto (Bug B core)"
```

---

## Task 2: Rust — durability contract guard for `flush_now` (persist side)

**Why:** Fix A depends on `flush_now()` writing `owner_state_crdt.cbor` synchronously. The existing `flush_now_fires_immediately` only asserts the *publish* side; the *persist* side has no test. This guard locks it. (Characterization test — expected green on first run; it documents the contract Fix A relies on.)

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs` — add a test in the same `#[cfg(test)] mod` as `flush_now_fires_immediately` (~line 842).

- [ ] **Step 1: Write the test**

```rust
#[tokio::test]
async fn flush_now_persists_owner_state_to_disk_without_shutdown() {
    use crate::owner_state_persist::load_crdt;
    use crate::owner_state_types::{Space, SpaceId, SpaceKind, Hlc, EpochKey, OwnerAddr};

    let (pub_tx, _pub_rx) = mpsc::channel(16);
    let (_sub_tx, sub_rx) = mpsc::channel(16);
    let (_dir, paths) = paths();
    let crdt_path = paths.crdt.clone(); // capture before `paths` is moved into new()
    let state = Arc::new(Mutex::new(OwnerState::default()));
    let engine = SyncEngine::new(
        make_kt(),
        "test-device".into(),
        Arc::clone(&state),
        Arc::new(Mutex::new(BTreeMap::new())),
        Arc::new(InMemoryStub::default()),
        pub_tx,
        sub_rx,
        paths,
        5000, // long debounce — only flush_now can persist within the test
    );

    // Mutate owner-state in memory: insert a Community Space.
    {
        let h = Hlc { wall_ms: 1, logical: 0, device_id: "d".into() };
        let space = Space {
            id: SpaceId([42; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "Durable".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: h.clone(),
            updated_at: h,
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: Some(EpochKey::new([1u8; 32])),
            old_epoch_keys: BTreeMap::new(),
            admin_addr: Some(OwnerAddr([2u8; 16])),
            is_invite_only: Some(false),
            shared_in_profile: false,
            pending_join_at: None,
        };
        state.lock().await.spaces.insert(space.id, space);
    }

    // Fence to disk WITHOUT a graceful shutdown.
    engine.flush_now().await.unwrap();

    // Reload from disk as boot would — the Space must be present.
    let reloaded = load_crdt(&crdt_path).unwrap();
    assert!(
        reloaded.spaces.contains_key(&SpaceId([42; 16])),
        "flush_now must persist owner-state so a crash after mint can't lose it"
    );

    let _ = engine.shutdown().await;
}
```

- [ ] **Step 2: Run — verify it passes (characterization)**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(flush_now_persists_owner_state_to_disk)'`
Expected: PASS. (If it FAILS, `flush_now`'s persist path regressed — investigate before proceeding; Fix A would be built on sand.)

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/owner_state_sync.rs
git commit -m "ZEB-393: guard test — flush_now persists owner-state to disk (Fix A contract)"
```

---

## Task 3: Rust — `list_owner_communities` IPC + registry (wiring)

**Why no unit test:** the logic is `communities_for_nav` (tested in Task 1); this is a thin `#[tauri::command]` shell over a `NodeState` snapshot. Verified by compile + the frontend integration (Task 5) + the live gate (Task 7).

**Files:**
- Modify: `src-tauri/src/lib.rs` — add the IPC next to `communities_for_nav`; register it in the `invoke_handler!` list after `list_community_members` (~lib.rs:37423).

- [ ] **Step 1: Add the IPC**

```rust
/// ZEB-393 Bug B: enumerate the viewer's live (non-left) communities for
/// boot rehydration of the nav sidebar. The frontend has no other way to
/// learn "which communities am I in" — `list_community_members` /
/// `list_community_forks` both require a `communityId` you must already
/// have. Read-only over the in-memory owner-state CRDT (populated at
/// start_node by `load_crdt`).
#[tauri::command]
async fn list_owner_communities(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
) -> Result<Vec<CommunityNavDto>, String> {
    let crdt_state = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.crdt_state.clone().ok_or(OWNER_NOT_LOADED_MSG)?
    };
    let state = crdt_state.lock().await;
    Ok(communities_for_nav(&state))
}
```

- [ ] **Step 2: Register in the invoke_handler! macro**

In `lib.rs` ~37423, after `list_community_members,`, add:

```rust
            list_community_members,
            list_owner_communities,
```

- [ ] **Step 3: Verify it compiles + full Rust suite green**

Run: `cd src-tauri && cargo nextest run --locked --all-targets --features test-fixtures`
Then: `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` and `cargo fmt --all`.
Expected: builds, all tests pass, no clippy warnings.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "ZEB-393: list_owner_communities IPC + registry (Bug B pull seam)"
```

---

## Task 4: Rust — Fix A: flush_now on mint + join (wiring)

**Why no unit test:** the handler integration needs a full Tauri `mock_app` + `NodeState`; covered by code review + the live gate (Task 7). The persist contract it relies on is guarded by Task 2.

**Files:**
- Modify: `src-tauri/src/lib.rs` — `create_community` (16768) and `redeem_invite` (18797).

- [ ] **Step 1: `create_community` — snapshot the sync engine**

In the snapshot tuple (`16779-16806`), add `g.sync_engine.clone()` as a new binding. Change the destructure to include `sync_engine` and add the field read (it's `Option`, so no `.ok_or`):

```rust
    let (
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        community_registry,
        community_adapter_tx,
        channel_log_registry,
        dm_outbox,
        snapshot_generation,
        sync_engine,
    ) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state.clone().ok_or(OWNER_NOT_LOADED_MSG)?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry.clone().ok_or(OWNER_NOT_LOADED_MSG)?,
            g.community_adapter_request_tx
                .clone()
                .ok_or("community_adapter_request_tx missing")?,
            g.channel_log_registry.clone().ok_or(OWNER_NOT_LOADED_MSG)?,
            g.dm_outbox.clone().ok_or(OWNER_NOT_LOADED_MSG)?,
            g.generation,
            g.sync_engine.clone(),
        )
    };
```

- [ ] **Step 2: `create_community` — fence after the inner returns**

Immediately after `let community_id = create_community_inner(...).await?;` (16836) and **before** the `nav-updated` emit (16842), insert:

```rust
    // ZEB-393 Bug A: durable-on-commit. The in-memory Space + per-community
    // dir are written, but owner_state_crdt.cbor otherwise persists only on
    // the ~250ms debounce or graceful shutdown. Fence it to disk now so a
    // non-graceful exit can't lose this membership. flush_now also republishes
    // the state-root to the user's other devices (desirable for a mint).
    // Non-fatal: the mint already committed in-memory; a flush hiccup falls
    // back to the debounce path. None only pre-start_node, where mint can't run.
    if let Some(engine) = sync_engine.as_ref() {
        if let Err(e) = engine.flush_now().await {
            tracing::warn!(error = %e, "create_community: owner-state flush_now failed");
        }
    }
```

- [ ] **Step 3: `redeem_invite` — snapshot the sync engine**

In the snapshot tuple (`18805-18834`), add `sync_engine` the same way (append `sync_engine` to the binding list and `g.sync_engine.clone()` as the last tuple element).

- [ ] **Step 4: `redeem_invite` — fence after the inner returns**

After `let dto = redeem_invite_inner(...).await?;` (18900) and before the `nav-updated` emit (18909), insert the same block with the warn label `redeem_invite: owner-state flush_now failed`.

- [ ] **Step 5: Verify compile + full Rust suite green**

Run: `cd src-tauri && cargo nextest run --locked --all-targets --features test-fixtures`
Then clippy + fmt as in Task 3 Step 3.
Expected: builds, all green, no warnings. (Watch for any existing `create_community` / `redeem_invite` tests that destructure differently — they call the *inner* fns, which are unchanged, so they should be unaffected.)

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "ZEB-393: flush_now owner-state after create_community + redeem_invite (Fix A)"
```

---

## Task 5: Frontend — `listOwnerCommunities` + `CommunityNavDto` type (TDD)

**Files:**
- Modify: `src/lib/community-service.ts` — add the `CommunityNavDto` interface, `listOwnerCommunities()`, and a public `noteKind()` (or fold knownKinds population into the fetch).
- Test: `src/lib/community-service.test.ts` (existing).

- [ ] **Step 1: Write the failing test**

Add to `community-service.test.ts` (mirror the existing mock-adapter pattern in that file):

```ts
describe('listOwnerCommunities (ZEB-393 boot rehydration)', () => {
  it('fetches list_owner_communities and records each kind', async () => {
    const rows = [
      { spaceId: 'aa', name: 'Open Town', isInviteOnly: false, pending: false },
      { spaceId: 'bb', name: 'Secret Club', isInviteOnly: true, pending: true },
    ];
    const adapter = makeMockAdapter(); // existing helper in this test file
    (adapter.invoke as Mock).mockResolvedValueOnce(rows);
    const svc = new CommunityService();
    await svc.connectAdapter(adapter);

    const got = await svc.listOwnerCommunities();

    expect(adapter.invoke).toHaveBeenCalledWith('list_owner_communities', {});
    expect(got).toEqual(rows);
    expect(svc.getKind('aa')).toBe('open');
    expect(svc.getKind('bb')).toBe('invite-only');
  });
});
```

> If `makeMockAdapter` / the exact harness differ in this file, match the file's existing pattern (it already mocks `adapter.invoke` for `list_community_members`). Keep the asserted IPC name (`list_owner_communities`) and the `getKind` expectations.

- [ ] **Step 2: Run — verify it fails**

Run: `npx vitest run src/lib/community-service.test.ts -t "listOwnerCommunities"`
Expected: FAIL — `svc.listOwnerCommunities is not a function`.

- [ ] **Step 3: Implement**

In `community-service.ts`:

```ts
export interface CommunityNavDto {
  spaceId: string;
  name: string;
  isInviteOnly: boolean;
  pending: boolean;
}
```

```ts
  /**
   * ZEB-393 Bug B: enumerate the viewer's persisted communities so the nav
   * sidebar can be rehydrated at boot. Also records each community's kind so
   * getKind() resolves correctly for rehydrated nodes (otherwise 'unknown'
   * until a runtime event). The nav-tree seeding happens in App.svelte via
   * navService.addOrUpdateNavSpace(toNavPayload(row)).
   */
  async listOwnerCommunities(): Promise<CommunityNavDto[]> {
    const rows = await this.invoke<CommunityNavDto[]>('list_owner_communities', {});
    for (const r of rows) {
      this.knownKinds.set(r.spaceId, r.isInviteOnly ? 'invite-only' : 'open');
    }
    return rows;
  }
```

- [ ] **Step 4: Run — verify it passes**

Run: `npx vitest run src/lib/community-service.test.ts -t "listOwnerCommunities"`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib/community-service.ts src/lib/community-service.test.ts
git commit -m "ZEB-393: community-service.listOwnerCommunities + kind recording (Bug B)"
```

---

## Task 6: Frontend — `toNavPayload` mapper + App.svelte boot seed (TDD + thin wiring)

**Files:**
- Modify: `src/lib/community-service.ts` — export pure `toNavPayload(c)`.
- Modify: `src/App.svelte` — boot seed loop after `navService.connectAdapter` (~1449).
- Test: `src/lib/community-service.test.ts`.

- [ ] **Step 1: Write the failing test for the mapper**

```ts
describe('toNavPayload (ZEB-393)', () => {
  it('maps a community DTO to an added community nav payload', () => {
    expect(toNavPayload({ spaceId: 'aa', name: 'Open Town', isInviteOnly: false, pending: false }))
      .toEqual({ action: 'added', spaceId: 'aa', kind: 'community', name: 'Open Town', pending: undefined });
  });
  it('carries pending=true so an invite-only join renders greyed at boot', () => {
    expect(toNavPayload({ spaceId: 'bb', name: 'Secret Club', isInviteOnly: true, pending: true }).pending)
      .toBe(true);
  });
});
```

- [ ] **Step 2: Run — verify it fails**

Run: `npx vitest run src/lib/community-service.test.ts -t "toNavPayload"`
Expected: FAIL — `toNavPayload is not defined`.

- [ ] **Step 3: Implement the mapper**

In `community-service.ts` (import the `NavUpdatedPayload` type from `./nav-service` — type-only import; if it lives in `./types`, import from there):

```ts
import type { NavUpdatedPayload } from './nav-service';

/** ZEB-393 Bug B: CommunityNavDto → an 'added' community nav payload. Pure;
 *  `pending: undefined` when not pending so addOrUpdateNavSpace leaves the
 *  greyed state off (it reads `pending ?? undefined`). */
export function toNavPayload(c: CommunityNavDto): NavUpdatedPayload {
  return {
    action: 'added',
    spaceId: c.spaceId,
    kind: 'community',
    name: c.name,
    pending: c.pending || undefined,
  };
}
```

- [ ] **Step 4: Run — verify it passes**

Run: `npx vitest run src/lib/community-service.test.ts -t "toNavPayload"`
Expected: PASS.

- [ ] **Step 5: Wire the boot seed into App.svelte**

In `src/App.svelte`, immediately after `await tryConnect('nav', navService.connectAdapter(adapter));` (~1449), add (import `toNavPayload` from `./lib/community-service`):

```ts
      // ZEB-393 Bug B: rehydrate persisted communities into the sidebar. The
      // nav tree is otherwise push-only/session-scoped and boots empty on
      // every restart regardless of what's on disk. Pull (not a backend boot
      // emit) so it can't race the listener; addOrUpdateNavSpace is cold-replay
      // idempotent, so a later runtime nav-updated for the same community is a
      // no-op update. Non-fatal: failure leaves the sidebar as today.
      try {
        for (const c of await communityService.listOwnerCommunities()) {
          navService.addOrUpdateNavSpace(toNavPayload(c));
        }
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        console.warn('[harmony-client] community rehydration failed:', msg);
      }
```

- [ ] **Step 6: Frontend suite + types green**

Run: `npx tsc --noEmit` and `npx vitest run`
Expected: PASS (incl. the two new describe blocks); no type errors.

- [ ] **Step 7: Commit**

```bash
git add src/lib/community-service.ts src/lib/community-service.test.ts src/App.svelte
git commit -m "ZEB-393: boot-seed nav from persisted communities (Bug B wiring)"
```

---

## Task 7: Full verification, live Ildwyn gate, ticket, PR

- [ ] **Step 1: Full local gates green**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo nextest run --locked --all-targets --features test-fixtures
cd .. && npx tsc --noEmit && npx vitest run
```
All four must be green (mirrors CI: rust-check, rust-test, msrv, frontend).

- [ ] **Step 2: Live verification on Ildwyn (CDP) — the integration gate**

Build/run the app (throwaway identity via `HARMONY_PASSPHRASE_FILE`, or coordinate a Jake-launched build), then via the `.playwright-scratch` CDP pattern (select the main `localhost:5173/` page, never `network.html`, never `browser.close()`):
- **Fix B (rehydration):** mint a community → restart the app (or relaunch the dev build) → assert the community is present in `.nav-row` / the community list at boot (was always empty before).
- **Fix A (durability):** mint a community → confirm the create returned → `Stop-Process -Force` the app process → relaunch → assert the community persists *and* rehydrates. (Without Fix A a kill shortly after mint loses it.)
Capture a screenshot + the DOM assertion for the PR body.

- [ ] **Step 3: Correct the ZEB-393 ticket**

Update the Linear ticket description to the verified two-bug reality (persistence gap + no-boot-rehydration, the latter dominant/masking), the confirmed scope, and the deferrals. File a sibling ticket for DM/folder rehydration if the live test confirms they're session-only too.

- [ ] **Step 4: Push + open PR**

Push the branch; open the PR with the two-bug explanation, the live evidence, the test summary, and the explicit deferrals. Then run the autonomous bot/CI review loop (CodeRabbit / Cursor / Qodo / CodeAnt; Greptile excluded as PR author) as on prior PRs — address or refute each, re-verify, until CI green and feedback settled. Jake merges.

---

## Self-review notes

- **Spec coverage:** Fix A (Tasks 4 + guard 2), Fix B pure logic (1), IPC (3), frontend fetch (5), mapper + boot seed (6), verification + PR (7). All spec sections mapped.
- **Type consistency:** Rust `CommunityNavDto` `#[serde(rename_all="camelCase")]` ↔ TS `{ spaceId, name, isInviteOnly, pending }`. `communities_for_nav` reads `s.id.0` / `s.is_invite_only` / `s.pending_join_at` / `s.left_at` — all confirmed `Space` fields. `toNavPayload` returns the exact subset `addOrUpdateNavSpace` destructures.
- **Deferred (not this plan):** DM/folder rehydration, leave-path durability, atomic mint dir, fork-lineage glyph on boot.
