# ZEB-666: DM Unread Badges Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Per-thread unread badges on DM / group-DM nav rows, plus the `list_owner_dm_spaces` IPC and boot rehydration that make them survive restarts.

**Architecture:** A new `DmUnreadService` (sibling of `ChannelUnreadService`, NOT a generalization — the channel service is untouched) tracks a capped Set of unread message CIDs per DM space over a persisted `receivedAt` wall-clock cursor stored as a pseudo-HLC in the existing `UnreadCursorStore` under the `'dm'` namespace. A new Rust `list_owner_dm_spaces` IPC (mirror of ZEB-393's `list_owner_communities`) lets App.svelte rehydrate DM nav rows at boot; a `NavService.onDmSpaceChange` hook triggers seeding, and a `MessageService.onDmReceived` post-dedup hook feeds live arrivals.

**Tech Stack:** Rust (Tauri IPC + ZEB-445 RPC seam), TypeScript/Svelte 5, vitest, cargo-nextest.

**Spec:** `docs/specs/2026-07-09-zeb-666-dm-unread-design.md` (Jake-approved 2026-07-09).

## Global Constraints

- Cursor = `receivedAt` wall-clock watermark, **strict `>`**, stored as pseudo-HLC `{ wallMs: receivedAt, logical: 0, deviceId: '' }` under store namespace `'dm'` (key `dm:<spaceId>`).
- Semantics parity with ZEB-665: open-clears-all; cap at `UNREAD_TRACK_CAP` (100, renders "99+"); start-clean stamp on first sight; unfocused-but-open counts; focused arrival uncounts only its own CID.
- **Init order (the ZEB-665 Qodo lesson):** `DmUnreadService` constructed and BOTH hooks assigned BEFORE `messageService.connectAdapter` (App.svelte line ~2099) — an optional-chained hook is a silent no-op and nothing replays it.
- **Shared cursor store:** `ChannelUnreadService` and `DmUnreadService` must share ONE `LocalStorageUnreadCursorStore` instance. Both persist into the same owner-scoped localStorage blob; two instances would each serialize only their own in-memory map and clobber the other's keys on every write.
- The channel service (`channel-unread-service.ts`) is not modified.
- Rust: `--locked` and `--features test-fixtures` on every cargo command; `--all-targets` on clippy.
- Frontend gates from repo root: `npx tsc --noEmit` + `npx vitest run`. Cargo gates from `src-tauri/`.
- Commit trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`.

---

### Task 1: Rust — `DmNavDto`, `dm_spaces_for_nav`, `list_owner_dm_spaces` IPC + RPC seam

**Files:**
- Modify: `src-tauri/src/lib.rs` (production block after `list_owner_communities_impl` ends at line ~19608; test mod after `zeb393_communities_for_nav_tests` closes at line ~19692; invoke_handler registration next to `list_owner_communities` at line ~52726)
- Modify: `src-tauri/src/api/rpc.rs` (rpc! block after `read_dm_thread` at line ~806; expected-command-list entry after `"read_dm_thread"` at line ~1551; parity test after `dm_invite_rpcs_dispatch_with_ipc_parity_pre_node` closes at line ~1295)

**Interfaces:**
- Consumes: `crate::owner_state_types::SpaceKind::{Dm, GroupDm}`, `Space { id, kind, name, custom_name, members, left_at }`, `OWNER_NOT_LOADED_MSG`, `NodeState.crdt_state`.
- Produces: IPC command `list_owner_dm_spaces` (no args) → `Vec<DmNavDto>` serialized camelCase as `{ spaceId: string, kind: "dm"|"group-dm", name: string, members: string[] }`. Task 5's rehydration loop invokes it.

- [ ] **Step 1: Write the failing unit tests**

In `src-tauri/src/lib.rs`, insert immediately after the closing `}` of `mod zeb393_communities_for_nav_tests` (line ~19692):

```rust
#[cfg(test)]
mod zeb666_dm_spaces_for_nav_tests {
    use super::*;
    use crate::owner_state_crdt::OwnerState;
    use crate::owner_state_types::{Hlc, OwnerAddr, Space, SpaceId, SpaceKind};

    fn hlc() -> Hlc {
        Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "test".into(),
        }
    }

    fn dm_space(id: u8, kind: SpaceKind, name: &str, custom: Option<&str>, left: bool) -> Space {
        Space {
            id: SpaceId([id; 16]),
            kind,
            parent: None,
            community_id: None,
            name: name.into(),
            transport: None,
            members: vec![OwnerAddr([1u8; 16]), OwnerAddr([2u8; 16])],
            custom_name: custom.map(Into::into),
            notification_pref: None,
            left_at: if left { Some(hlc()) } else { None },
            created_at: hlc(),
            updated_at: hlc(),
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        }
    }

    #[test]
    fn returns_live_dm_and_group_dm_with_kind_name_members() {
        let mut st = OwnerState::default();
        for s in [
            dm_space(1, SpaceKind::Dm, "DM with abcd", None, false),
            dm_space(2, SpaceKind::GroupDm, "Group chat", Some("Weekend crew"), false),
            dm_space(3, SpaceKind::Dm, "Left DM", None, true), // left → excluded
            dm_space(4, SpaceKind::Community, "Not a DM", None, false), // wrong kind → excluded
        ] {
            st.spaces.insert(s.id, s);
        }

        let mut got = dm_spaces_for_nav(&st);
        got.sort_by(|a, b| a.space_id.cmp(&b.space_id));

        assert_eq!(got.len(), 2, "only the two live DM-kind spaces");
        assert_eq!(got[0].space_id, hex::encode([1u8; 16]));
        assert_eq!(got[0].kind, "dm");
        assert_eq!(got[0].name, "DM with abcd", "no custom_name → original name");
        assert_eq!(
            got[0].members,
            vec![hex::encode([1u8; 16]), hex::encode([2u8; 16])]
        );
        assert_eq!(got[1].kind, "group-dm");
        assert_eq!(got[1].name, "Weekend crew", "custom_name (user rename) wins");
    }

    #[test]
    fn empty_state_yields_empty() {
        assert!(dm_spaces_for_nav(&OwnerState::default()).is_empty());
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zeb666)'`
Expected: compile FAIL — `dm_spaces_for_nav` not found.

- [ ] **Step 3: Write the production block**

In `src-tauri/src/lib.rs`, insert immediately after the closing `}` of `list_owner_dm_spaces`'s future neighbor — i.e., after `list_owner_communities_impl` ends (line ~19608), before `mod zeb393_communities_for_nav_tests`:

```rust
/// ZEB-666: a persisted DM / group-DM space shaped for the nav sidebar.
/// `space_id` is the 32-char lowercase hex of the 16-byte SpaceId; `kind`
/// uses the `nav-updated` vocabulary ("dm" | "group-dm") so the frontend can
/// feed rows straight into `addOrUpdateNavSpace`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DmNavDto {
    pub space_id: String,
    pub kind: String,
    /// `custom_name` (user rename) wins over the space's original name.
    pub name: String,
    /// Hex-encoded member OwnerAddrs (self included — the frontend derives
    /// the 1:1 peer as "whichever member isn't me", exactly as it does for
    /// the runtime `nav-updated` emit's `members`).
    pub members: Vec<String>,
}

/// ZEB-666: owner-state DM / group-DM spaces shaped for boot rehydration of
/// the nav sidebar (the ZEB-393 `communities_for_nav` pattern). Filters to
/// live (non-left) spaces. The frontend has no other way to learn "which DM
/// threads do I have" — DM nav rows are otherwise push-only (runtime
/// `nav-updated` / `handleDmCreate`) and vanish on every restart, which
/// would strand the persisted per-thread read cursors.
pub fn dm_spaces_for_nav(state: &crate::owner_state_crdt::OwnerState) -> Vec<DmNavDto> {
    use crate::owner_state_types::SpaceKind;
    state
        .spaces
        .values()
        .filter(|s| matches!(s.kind, SpaceKind::Dm | SpaceKind::GroupDm) && s.left_at.is_none())
        .map(|s| DmNavDto {
            space_id: hex::encode(s.id.0),
            kind: match s.kind {
                SpaceKind::GroupDm => "group-dm".to_string(),
                _ => "dm".to_string(),
            },
            name: s.custom_name.clone().unwrap_or_else(|| s.name.clone()),
            members: s.members.iter().map(|m| hex::encode(m.0)).collect(),
        })
        .collect()
}

/// ZEB-666: enumerate the viewer's live DM / group-DM spaces for boot
/// rehydration of the nav sidebar. Read-only over the in-memory owner-state
/// CRDT (populated at `start_node` by `load_crdt`). App.svelte calls this
/// right after community rehydration (after `navService.connectAdapter`)
/// and seeds via `addOrUpdateNavSpace`.
#[tauri::command]
async fn list_owner_dm_spaces(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
) -> Result<Vec<DmNavDto>, String> {
    list_owner_dm_spaces_impl(state_lock.inner()).await
}

/// ZEB-445: shared IPC/RPC seam.
pub(crate) async fn list_owner_dm_spaces_impl(
    state: &std::sync::Mutex<NodeState>,
) -> Result<Vec<DmNavDto>, String> {
    let crdt_state = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.crdt_state.clone().ok_or(OWNER_NOT_LOADED_MSG)?
    };
    let state = crdt_state.lock().await;
    Ok(dm_spaces_for_nav(&state))
}
```

Then register the command in the `invoke_handler` list (line ~52726), directly under `list_owner_communities,`:

```rust
            list_owner_communities,
            list_owner_dm_spaces,
```

- [ ] **Step 4: Run unit tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zeb666)'`
Expected: 2 PASS.

- [ ] **Step 5: Write the failing RPC tests**

In `src-tauri/src/api/rpc.rs`:

(a) Expected-command-list: after `"read_dm_thread",` (line ~1551) add:

```rust
            "read_dm_thread",
            // DM nav rehydration (ZEB-666)
            "list_owner_dm_spaces",
```

(b) Parity test: insert after the closing `}` of `dm_invite_rpcs_dispatch_with_ipc_parity_pre_node` (line ~1295):

```rust
    #[tokio::test]
    async fn list_owner_dm_spaces_dispatches_with_ipc_parity_pre_node() {
        // ZEB-666: must dispatch through the SAME `*_impl` seam the Tauri
        // IPC layer uses, observing the shared pre-node error string.
        let reg = build_registry();
        let err = reg
            .dispatch(
                "list_owner_dm_spaces",
                test_state(),
                test_sink(),
                serde_json::Value::Null,
            )
            .await
            .unwrap_err();
        match err {
            RpcError::Command(msg) => assert_eq!(
                msg,
                crate::OWNER_NOT_LOADED_MSG,
                "must share the IPC owner-not-loaded error string"
            ),
            other => panic!("expected Command, got {other:?}"),
        }
    }
```

- [ ] **Step 6: Run to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(list_owner_dm_spaces) or test(command_list)'`
Expected: FAIL — the command-list test reports `list_owner_dm_spaces` missing from the registry, the parity test gets `RpcError::UnknownCommand`.

- [ ] **Step 7: Register the RPC verb**

In `src-tauri/src/api/rpc.rs`, after the `read_dm_thread` rpc! block (line ~806):

```rust
    // DM nav rehydration (ZEB-666).
    rpc!(
        m,
        "list_owner_dm_spaces",
        EmptyArgs,
        |state, _sink, _a| async move { crate::list_owner_dm_spaces_impl(state).await }
    );
```

- [ ] **Step 8: Run all Task-1 tests + fmt**

Run: `cd src-tauri && cargo fmt --all && cargo nextest run --locked --features test-fixtures -E 'test(zeb666) or test(list_owner_dm_spaces) or test(command_list)'`
Expected: all PASS.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/api/rpc.rs
git commit -m "ZEB-666: list_owner_dm_spaces IPC — enumerate live DM/group-DM spaces for nav rehydration"
```

---

### Task 2: `DmUnreadService` (frontend model) + tests

**Files:**
- Create: `src/lib/dm-unread-service.ts`
- Create: `src/lib/dm-unread-service.test.ts`

**Interfaces:**
- Consumes: `UNREAD_TRACK_CAP` from `./channel-unread-service`; `UnreadCursorStore` from `./unread-cursor-store`; `Hlc` from `./types`.
- Produces (used by Tasks 4/5): `class DmUnreadService` with `connectOwner(ownerId: string): void`, `onDmSpaceMaterialized(spaceId: string): Promise<void>`, `onDmReceived(p: DmArrival): void`, `markThreadRead(spaceId: string): void`, `onDmSpaceRemoved(spaceId: string): void`; types `DmUnreadDeps`, `DmThreadPageEntry { messageCid, from, receivedAt, isSelfOutbound }`, `DmArrival { spaceId, messageCid, from, receivedAt }`.

- [ ] **Step 1: Write the failing test suite**

Create `src/lib/dm-unread-service.test.ts`:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { DmUnreadService, type DmUnreadDeps, type DmThreadPageEntry } from './dm-unread-service';
import { UNREAD_TRACK_CAP } from './channel-unread-service';
import type { Hlc } from './types';
import type { UnreadCursorStore } from './unread-cursor-store';

const entry = (
  messageCid: string,
  receivedAt: number,
  isSelfOutbound = false,
): DmThreadPageEntry => ({ messageCid, from: isSelfOutbound ? 'me' : 'peer', receivedAt, isSelfOutbound });
const arrival = (messageCid: string, receivedAt: number, from = 'peer') => ({
  spaceId: 's1',
  messageCid,
  from,
  receivedAt,
});

class MemStore implements UnreadCursorStore {
  owner: string | null = null;
  map = new Map<string, Hlc>();
  connectOwner(o: string) {
    this.owner = o;
  }
  get(ns: string, id: string) {
    return this.owner ? (this.map.get(`${ns}:${id}`) ?? null) : null;
  }
  set(ns: string, id: string, h: Hlc) {
    if (this.owner) this.map.set(`${ns}:${id}`, h);
  }
}

function harness(over: Partial<DmUnreadDeps> = {}) {
  const store = new MemStore();
  store.connectOwner('me');
  const pushes: Array<[string, number]> = [];
  const deps: DmUnreadDeps = {
    listThreadPage: vi.fn(async () => []),
    setUnread: (id, n) => pushes.push([id, n]),
    isActiveThread: () => false,
    isFocused: () => true,
    selfOwnerId: () => 'me',
    storage: store,
    now: () => 5000,
    ...over,
  };
  return { svc: new DmUnreadService(deps), deps, store, pushes };
}
const lastCount = (pushes: Array<[string, number]>, id: string) =>
  [...pushes].reverse().find(([sid]) => sid === id)?.[1];
const cursorMs = (store: MemStore, id: string) => store.get('dm', id)?.wallMs;

describe('DmUnreadService (ZEB-666)', () => {
  it('start-clean: no stored cursor → stamps now() and pushes 0, no IPC', async () => {
    const { svc, deps, store, pushes } = harness();
    await svc.onDmSpaceMaterialized('s1');
    expect(store.get('dm', 's1')).toEqual({ wallMs: 5000, logical: 0, deviceId: '' });
    expect(deps.listThreadPage).not.toHaveBeenCalled();
    expect(lastCount(pushes, 's1')).toBe(0);
  });

  it('seed with stored cursor counts strictly-newer non-self entries', async () => {
    const { svc, store, pushes } = harness({
      listThreadPage: async () => [
        entry('m4', 400, true), // self-outbound → dropped
        entry('m3', 300),
        entry('m2', 200),
        entry('m1', 100), // == cursor → dropped (strict >)
        entry('m0', 50), // older → dropped
      ],
    });
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    expect(lastCount(pushes, 's1')).toBe(2);
  });

  it('seed overflow caps at UNREAD_TRACK_CAP', async () => {
    const many = Array.from({ length: UNREAD_TRACK_CAP + 20 }, (_, i) =>
      entry(`m${i}`, 5000 - i), // newest-first, all > cursor
    );
    const { svc, store, pushes } = harness({ listThreadPage: async () => many });
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    expect(lastCount(pushes, 's1')).toBe(UNREAD_TRACK_CAP);
  });

  it('seed failure un-marks seeded (retried on next materialize) and still pushes', async () => {
    let calls = 0;
    const { svc, pushes, store } = harness({
      listThreadPage: async () => {
        calls++;
        if (calls === 1) throw new Error('ipc down');
        return [entry('m1', 200)];
      },
    });
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    expect(lastCount(pushes, 's1')).toBe(0); // failed seed → empty set, still pushed
    await svc.onDmSpaceMaterialized('s1'); // retry succeeds
    expect(lastCount(pushes, 's1')).toBe(1);
  });

  it('live arrival for a non-active thread counts once (re-delivery dedupes)', async () => {
    const { svc, store, pushes } = harness();
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    svc.onDmReceived(arrival('m1', 200));
    svc.onDmReceived(arrival('m1', 200));
    expect(lastCount(pushes, 's1')).toBe(1);
  });

  it('arrivals at or before the cursor never count (strict >)', async () => {
    const { svc, store, pushes } = harness();
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    svc.onDmReceived(arrival('old', 50));
    svc.onDmReceived(arrival('at-cursor', 100));
    expect(lastCount(pushes, 's1')).toBe(0);
  });

  it('self arrivals (from === selfOwnerId) never count', async () => {
    const { svc, store, pushes } = harness();
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    svc.onDmReceived(arrival('mine', 200, 'me'));
    expect(lastCount(pushes, 's1')).toBe(0);
  });

  it('no-cursor arrival is ignored (start-clean at materialize covers it)', () => {
    const { svc, pushes } = harness();
    svc.onDmReceived(arrival('m1', 200));
    expect(pushes.length).toBe(0);
  });

  it('focused+active arrival advances cursor and uncounts only its own CID', async () => {
    const { svc, store, pushes } = harness({
      isActiveThread: (id) => id === 's1',
      isFocused: () => true,
    });
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    svc.onDmReceived(arrival('backlog', 150));
    expect(lastCount(pushes, 's1')).toBe(0); // focused+active → not counted, cursor advanced to 150
    // Now simulate the backlog arriving while unfocused:
    const { svc: svc2, store: store2, pushes: pushes2 } = harness({
      isActiveThread: (id) => id === 's1',
      isFocused: () => false,
    });
    store2.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc2.onDmSpaceMaterialized('s1');
    svc2.onDmReceived(arrival('b1', 150));
    svc2.onDmReceived(arrival('b2', 160));
    expect(lastCount(pushes2, 's1')).toBe(2);
  });

  it('focused+active re-delivery of a counted CID removes it, preserving the rest', async () => {
    let focused = false;
    const { svc, store, pushes } = harness({
      isActiveThread: (id) => id === 's1',
      isFocused: () => focused,
    });
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    svc.onDmReceived(arrival('b1', 150));
    svc.onDmReceived(arrival('b2', 160));
    expect(lastCount(pushes, 's1')).toBe(2);
    focused = true; // user focuses the window with the thread open
    svc.onDmReceived(arrival('b2', 160)); // re-delivery of a counted message
    expect(lastCount(pushes, 's1')).toBe(1); // b2 uncounted, b1 backlog preserved
    expect(cursorMs(store, 's1')).toBe(160);
  });

  it('markThreadRead stamps max(cursor, maxSeen, now) and clears the set', async () => {
    const { svc, store, pushes } = harness({ now: () => 5000 });
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    svc.onDmReceived(arrival('m1', 9000)); // receivedAt ahead of now()
    expect(lastCount(pushes, 's1')).toBe(1);
    svc.markThreadRead('s1');
    expect(lastCount(pushes, 's1')).toBe(0);
    expect(cursorMs(store, 's1')).toBe(9000); // maxSeen wins over now()
    svc.onDmReceived(arrival('m1', 9000)); // replay of the read message
    expect(lastCount(pushes, 's1')).toBe(0);
  });

  it('connectOwner wipes session state and replays materialized spaces', async () => {
    const { svc, store, pushes } = harness();
    await svc.onDmSpaceMaterialized('s1'); // start-clean under 'me'
    svc.onDmReceived(arrival('m1', 6000));
    expect(lastCount(pushes, 's1')).toBe(1);
    store.connectOwner('other'); // MemStore keeps map; real store reloads per owner
    svc.connectOwner('other');
    await new Promise((r) => setTimeout(r, 0)); // drain the replayed async seed
    expect(lastCount(pushes, 's1')).toBe(0); // fresh session state for the new owner
  });

  it('onDmSpaceRemoved drops session state (cursor kept)', async () => {
    const { svc, store, pushes } = harness();
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    svc.onDmReceived(arrival('m1', 200));
    expect(lastCount(pushes, 's1')).toBe(1);
    svc.onDmSpaceRemoved('s1');
    expect(store.get('dm', 's1')).not.toBeNull(); // cursor survives removal
    svc.onDmReceived(arrival('m2', 300)); // no longer tracked → still counts fresh?
    // After removal the space is unseeded; a later arrival with a cursor
    // still counts (channel parity: gate is the cursor, not seededness).
    expect(lastCount(pushes, 's1')).toBe(1);
  });
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `npx vitest run src/lib/dm-unread-service.test.ts`
Expected: FAIL — module `./dm-unread-service` not found.

- [ ] **Step 3: Implement the service**

Create `src/lib/dm-unread-service.ts`:

```typescript
/**
 * ZEB-666 — per-thread unread counts for DM / group-DM nav rows.
 *
 * Sibling of ChannelUnreadService (ZEB-665), NOT a generalization: it
 * consumes DM shapes (the raw `dm-received` payload and `read_dm_thread`
 * page entries), which carry bare wall-clock ms instead of full HLCs.
 * The cursor is a `receivedAt` watermark (strict >) stored as a pseudo-HLC
 * `{ wallMs, logical: 0, deviceId: '' }` in the shared UnreadCursorStore
 * under the 'dm' namespace (key `dm:<spaceId>`), so the ZEB-665 storage
 * machinery works unchanged. Local-arrival ordering is the right unread
 * semantic (immune to sender clock skew) and matches `read_dm_thread`'s own
 * pagination key.
 *
 * Known v1 caveat (spec §4.1): equal-millisecond ties at a restart boundary
 * are swallowed from the count (strict > on bare ms). Rare, self-heals on
 * open; the root fix is full HLC on the DM path (spec §6.1 follow-up).
 *
 * All side effects are injected → unit-testable. NavService renders the
 * counts (`setUnread`); this service owns the model.
 */
import type { Hlc } from './types';
import type { UnreadCursorStore } from './unread-cursor-store';
import { UNREAD_TRACK_CAP } from './channel-unread-service';

/** Store namespace: cursor keys are `dm:<spaceId>`. Channel keys are
 *  `<communityId>:<channelId>` with 32-hex community ids — no collision. */
const NS = 'dm';

/** The subset of a `read_dm_thread` DmThreadMessage this service reads.
 *  The page is newest-first (backend-hardcoded), so an over-cap seed
 *  naturally keeps the newest cap-worth — no ZEB-602-style ordering work. */
export interface DmThreadPageEntry {
  messageCid: string;
  from: string;
  receivedAt: number;
  isSelfOutbound: boolean;
}

/** The subset of a `dm-received` payload this service reads (the full
 *  MessageService payload is structurally assignable). */
export interface DmArrival {
  spaceId: string;
  messageCid: string;
  from: string;
  receivedAt: number;
}

export interface DmUnreadDeps {
  /** Raw `read_dm_thread` invoke (newest-first page, `beforeHlc: null`).
   *  NOT MessageService.loadDmThread — that ingests into the feed cache and
   *  advances its own pagination cursor. */
  listThreadPage(spaceId: string, limit: number): Promise<DmThreadPageEntry[]>;
  setUnread(spaceId: string, count: number): void;
  /** True iff the viewer is looking right at this DM thread's feed. */
  isActiveThread(spaceId: string): boolean;
  isFocused(): boolean;
  selfOwnerId(): string | null;
  storage: UnreadCursorStore;
  /** Wall clock (injectable for tests) — start-clean / clear stamps only. */
  now(): number;
}

const pseudoHlc = (wallMs: number): Hlc => ({ wallMs, logical: 0, deviceId: '' });

export class DmUnreadService {
  /** Unread message CIDs per space, capped at UNREAD_TRACK_CAP. */
  private sets = new Map<string, Set<string>>();
  /** Newest receivedAt ever seen per space (seeds AND arrivals), for clear stamps. */
  private maxSeen = new Map<string, number>();
  /** Spaces seeded this session (cleared by connectOwner / removal). */
  private seeded = new Set<string>();
  /** Spaces materialized in the nav, for owner-connect re-seed. */
  private materialized = new Set<string>();

  constructor(private deps: DmUnreadDeps) {}

  /** (Re)connect the cursor store when the owner identity lands. Spaces
   *  materialized pre-identity were start-clean no-ops (the store refuses
   *  pre-owner writes), so clear session state and re-seed everything. */
  connectOwner(ownerId: string): void {
    this.deps.storage.connectOwner(ownerId);
    this.sets.clear();
    this.maxSeen.clear();
    this.seeded.clear();
    for (const spaceId of this.materialized) {
      void this.onDmSpaceMaterialized(spaceId);
    }
  }

  /** Called whenever a DM / group-DM nav row is (re)pushed into the nav —
   *  boot rehydration, runtime nav-updated, or handleDmCreate. Seeds once
   *  per session; always re-pushes the count (the nav node may have just
   *  been rebuilt with unreadCount preserved-or-zero). */
  async onDmSpaceMaterialized(spaceId: string): Promise<void> {
    this.materialized.add(spaceId);
    if (!this.seeded.has(spaceId)) {
      this.seeded.add(spaceId); // pre-mark so a concurrent re-materialize can't double-seed
      try {
        await this.seedSpace(spaceId);
      } catch (e) {
        this.seeded.delete(spaceId); // retried on the next materialize
        const msg = e instanceof Error ? e.message : String(e);
        console.warn(`[dm-unread] seed failed for ${spaceId}:`, msg);
      }
    }
    this.push(spaceId);
  }

  /** Live per-message hook (fires once per CID — `apply_inbox` is idempotent
   *  and MessageService dedups by messageCid before calling this; the capped
   *  CID-set still dedupes defensively). Synchronous by design. */
  onDmReceived(p: DmArrival): void {
    this.bumpMaxSeen(p.spaceId, p.receivedAt);
    if (p.from === this.deps.selfOwnerId()) return;
    const cursor = this.deps.storage.get(NS, p.spaceId);
    // No cursor yet → first-sight start-clean at materialize covers this
    // space; counting here would violate the start-clean decision.
    if (cursor === null) return;
    if (this.deps.isFocused() && this.deps.isActiveThread(p.spaceId)) {
      // Looking right at it → THIS message is read the moment it lands. Do
      // NOT wipe the whole set: an unfocused-but-open backlog stays badged
      // until markThreadRead. Remove only this message's CID (it can be
      // present via re-delivery of a previously-counted message).
      if (p.receivedAt > cursor.wallMs) {
        this.deps.storage.set(NS, p.spaceId, pseudoHlc(p.receivedAt));
      }
      const set = this.sets.get(p.spaceId);
      if (set?.delete(p.messageCid)) this.push(p.spaceId);
      return;
    }
    if (p.receivedAt <= cursor.wallMs) return; // history replay of read messages
    const set = this.setFor(p.spaceId);
    const before = set.size;
    if (set.size < UNREAD_TRACK_CAP || set.has(p.messageCid)) {
      set.add(p.messageCid);
    }
    if (set.size !== before) this.push(p.spaceId);
  }

  /** Open-clears-all: stamp the cursor past everything we know about and
   *  wipe the set. maxSeen reflects the true newest even under overflow
   *  (the newest-first page bumps it); the wall-clock stamp stays as
   *  belt-and-braces (e.g. a failed seed) so opening always clears. */
  markThreadRead(spaceId: string): void {
    const candidates = [this.deps.now()];
    const cursor = this.deps.storage.get(NS, spaceId);
    if (cursor) candidates.push(cursor.wallMs);
    const seen = this.maxSeen.get(spaceId);
    if (seen !== undefined) candidates.push(seen);
    this.deps.storage.set(NS, spaceId, pseudoHlc(Math.max(...candidates)));
    this.sets.get(spaceId)?.clear();
    this.push(spaceId);
  }

  /** Drop session state for a removed DM row (cursor is kept — a re-added
   *  thread preserving read state is the better behavior, and it's tiny). */
  onDmSpaceRemoved(spaceId: string): void {
    this.sets.delete(spaceId);
    this.maxSeen.delete(spaceId);
    this.seeded.delete(spaceId);
    this.materialized.delete(spaceId);
  }

  private async seedSpace(spaceId: string): Promise<void> {
    const cursor = this.deps.storage.get(NS, spaceId);
    if (cursor === null) {
      // First sight: start clean — stamp "now", count from here.
      this.deps.storage.set(NS, spaceId, pseudoHlc(this.deps.now()));
      return;
    }
    const page = await this.deps.listThreadPage(spaceId, UNREAD_TRACK_CAP);
    const set = this.setFor(spaceId); // union with event-race arrivals
    for (const m of page) {
      this.bumpMaxSeen(spaceId, m.receivedAt);
      if (m.isSelfOutbound) continue;
      if (m.receivedAt <= cursor.wallMs) continue; // page is uncursored; filter client-side
      if (set.size < UNREAD_TRACK_CAP || set.has(m.messageCid)) {
        set.add(m.messageCid);
      }
    }
  }

  private push(spaceId: string): void {
    const set = this.sets.get(spaceId);
    this.deps.setUnread(spaceId, set?.size ?? 0);
  }

  private setFor(spaceId: string): Set<string> {
    let set = this.sets.get(spaceId);
    if (!set) {
      set = new Set();
      this.sets.set(spaceId, set);
    }
    return set;
  }

  private bumpMaxSeen(spaceId: string, receivedAt: number): void {
    const cur = this.maxSeen.get(spaceId);
    if (cur === undefined || receivedAt > cur) this.maxSeen.set(spaceId, receivedAt);
  }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/dm-unread-service.test.ts`
Expected: all PASS. Also run `npx tsc --noEmit` — clean.

- [ ] **Step 5: Commit**

```bash
git add src/lib/dm-unread-service.ts src/lib/dm-unread-service.test.ts
git commit -m "ZEB-666: DmUnreadService — per-thread unread model over receivedAt cursors"
```

---

### Task 3: NavService — `onDmSpaceChange` hook + `setUnread` widened to DM rows

**Files:**
- Modify: `src/lib/nav-service.ts` (hook field near line 46; dm-path firing in `addOrUpdateNavSpace` lines ~281-368; `setUnread` lines ~540-566)
- Modify: `src/lib/nav-service.test.ts` (new describe block at end)

**Interfaces:**
- Produces (used by Task 5): `NavService.onDmSpaceChange?: (action: 'added' | 'removed', spaceId: string) => void`; `setUnread(channelId, count)` now also drives nodes with `type: 'dm' | 'group-chat'` (community rollup remains channel-only).

- [ ] **Step 1: Write the failing tests**

Append to `src/lib/nav-service.test.ts` (uses the same `NavService` import as the existing suites):

```typescript
describe('NavService — DM unread + space-change hook (ZEB-666)', () => {
  function withDm(): NavService {
    const s = new NavService({ seedMockData: false });
    s.addOrUpdateNavSpace({
      action: 'added', spaceId: 'dm1', kind: 'dm', name: 'DM with abcd',
      members: ['aaaa', 'bbbb'],
    });
    s.addOrUpdateNavSpace({
      action: 'added', spaceId: 'g1', kind: 'group-dm', name: 'Weekend crew',
      members: ['aaaa', 'bbbb', 'cccc'],
    });
    return s;
  }

  it('setUnread drives a dm node (count + standard level) and clears at 0', () => {
    const s = withDm();
    s.setUnread('dm1', 4);
    const dm = s.nodes.find((n) => n.id === 'dm1')!;
    expect(dm.unreadCount).toBe(4);
    expect(dm.unreadLevel).toBe('standard');
    s.setUnread('dm1', 0);
    expect(s.nodes.find((n) => n.id === 'dm1')!.unreadLevel).toBe('none');
  });

  it('setUnread drives a group-chat node', () => {
    const s = withDm();
    s.setUnread('g1', 2);
    const g = s.nodes.find((n) => n.id === 'g1')!;
    expect(g.unreadCount).toBe(2);
    expect(g.unreadLevel).toBe('standard');
  });

  it('setUnread on a DM node never touches community rollup', () => {
    const s = withDm();
    s.nodes = [
      ...s.nodes,
      { id: 'c1', parentId: null, type: 'community', name: 'C', expanded: true, unreadCount: 0, mentionCount: 0, unreadLevel: 'none' },
    ];
    s.setUnread('dm1', 7);
    const c1 = s.nodes.find((n) => n.id === 'c1')!;
    expect(c1.unreadCount).toBe(0);
    expect(c1.unreadLevel).toBe('none');
  });

  it('onDmSpaceChange fires added for dm/group-dm adds and modifies', () => {
    const s = new NavService({ seedMockData: false });
    const events: Array<[string, string]> = [];
    s.onDmSpaceChange = (action, spaceId) => events.push([action, spaceId]);
    s.addOrUpdateNavSpace({ action: 'added', spaceId: 'dm1', kind: 'dm', name: 'D' });
    s.addOrUpdateNavSpace({ action: 'modified', spaceId: 'dm1', kind: 'dm', name: 'D2' });
    s.addOrUpdateNavSpace({ action: 'added', spaceId: 'g1', kind: 'group-dm', name: 'G' });
    expect(events).toEqual([
      ['added', 'dm1'],
      ['added', 'dm1'], // modified self-heals as added; seeding is idempotent
      ['added', 'g1'],
    ]);
  });

  it('onDmSpaceChange fires removed on dm removal', () => {
    const s = new NavService({ seedMockData: false });
    const events: Array<[string, string]> = [];
    s.addOrUpdateNavSpace({ action: 'added', spaceId: 'dm1', kind: 'dm', name: 'D' });
    s.onDmSpaceChange = (action, spaceId) => events.push([action, spaceId]);
    s.addOrUpdateNavSpace({ action: 'removed', spaceId: 'dm1', kind: 'dm', name: 'D' });
    expect(events).toEqual([['removed', 'dm1']]);
  });

  it('onDmSpaceChange does NOT fire for community payloads', () => {
    const s = new NavService({ seedMockData: false });
    const events: Array<[string, string]> = [];
    s.onDmSpaceChange = (action, spaceId) => events.push([action, spaceId]);
    s.addOrUpdateNavSpace({ action: 'added', spaceId: 'c1', kind: 'community', name: 'C' });
    s.addOrUpdateNavSpace({ action: 'removed', spaceId: 'c1', kind: 'community', name: 'C' });
    expect(events).toEqual([]);
  });
});
```

- [ ] **Step 2: Run to verify they fail**

Run: `npx vitest run src/lib/nav-service.test.ts`
Expected: FAIL — `onDmSpaceChange` unknown / `setUnread` no-ops on dm nodes.

- [ ] **Step 3: Implement**

(a) Add the hook field in `NavService`, directly under `ownAddress` (line ~46):

```typescript
  /** Hex-encoded own address — profile updates matching this are filtered. */
  ownAddress: string | null = null;
  /** ZEB-666: fired from `addOrUpdateNavSpace`'s dm/group-dm path so
   *  DmUnreadService can seed ('added' — also fired for modified, which
   *  self-heals as added; seeding is idempotent) or drop ('removed')
   *  per-thread unread state. Assign BEFORE connectAdapter (an
   *  optional-chained hook is a silent no-op; nothing replays it). */
  onDmSpaceChange?: (action: 'added' | 'removed', spaceId: string) => void;
```

(b) Fire it from the dm/group-dm path of `addOrUpdateNavSpace`. The removed branch (line ~281) becomes:

```typescript
    if (action === 'removed') {
      const before = this.nodes.length;
      this.nodes = this.nodes.filter((n) => n.id !== spaceId);
      if (this.nodes.length !== before) this.onChange?.();
      this.onDmSpaceChange?.('removed', spaceId);
      return;
    }
```

And the end of the method (line ~367) becomes:

```typescript
    this.onChange?.();
    this.onDmSpaceChange?.('added', spaceId);
  }
```

(c) Widen `setUnread` (lines ~540-566) — node lookup accepts DM types; rollup stays channel-only:

```typescript
  /** ZEB-665/ZEB-666: absolute per-node unread count (from the unread
   *  services' capped ID-sets — recomputable projections, so no boot-race
   *  queue like mentions: the services re-push when nodes materialize).
   *  Drives channel, dm, and group-chat nodes; only channels roll the
   *  owning community up to Σ(children) with a `quiet` dot (DM rows have
   *  no aggregation target — spec ZEB-666 §1.4). */
  setUnread(channelId: string, count: number): void {
    const node = this.nodes.find(
      (n) =>
        n.id === channelId &&
        (n.type === 'channel' || n.type === 'dm' || n.type === 'group-chat'),
    );
    if (!node) return;
    const next = Math.max(0, count);
    const nextLevel: NavNode['unreadLevel'] = next > 0 ? 'standard' : 'none';
    if (node.unreadCount === next && node.unreadLevel === nextLevel) return;
    const delta = next - node.unreadCount;
    node.unreadCount = next;
    node.unreadLevel = nextLevel;
    // Incremental community rollup (the applyMentionDelta idiom) — setUnread
    // sits on the per-message hot path, so no full-node scan here. setChannels
    // still full-recomputes via rollUpCommunityUnread when structure changes.
    // Channel nodes only: a DM's parentId is a user folder, not a community.
    if (node.type === 'channel') {
      const cid = this.communityIdOf(node);
      if (cid) {
        const comm = this.nodes.find((n) => n.id === cid);
        if (comm) {
          comm.unreadCount = Math.max(0, comm.unreadCount + delta);
          comm.unreadLevel = comm.unreadCount > 0 ? 'quiet' : 'none';
        }
      }
    }
    this.onChange?.();
  }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/nav-service.test.ts && npx tsc --noEmit`
Expected: all PASS (including the pre-existing ZEB-665 suite — channel behavior unchanged).

- [ ] **Step 5: Commit**

```bash
git add src/lib/nav-service.ts src/lib/nav-service.test.ts
git commit -m "ZEB-666: NavService — setUnread drives DM rows; onDmSpaceChange hook"
```

---

### Task 4: MessageService — `onDmReceived` post-dedup hook

**Files:**
- Modify: `src/lib/message-service.ts` (field near line 50; `dm-received` listener at lines ~154-166)
- Modify: `src/lib/message-service.test.ts` (new tests in the `MessageService DM events` describe)

**Interfaces:**
- Produces (used by Task 5): `MessageService.onDmReceived?: (payload: { spaceId: string; messageCid: string; from: string; sentAt: number; receivedAt: number; body: string; mimeType: string }) => void` — fires once per unique messageCid (post-`seenIds` dedup), before the Message is appended.

- [ ] **Step 1: Write the failing tests**

Add inside `describe('MessageService DM events', ...)` in `src/lib/message-service.test.ts`:

```typescript
  it('onDmReceived hook fires post-dedup with the raw payload (ZEB-666)', async () => {
    const { adapter, emit } = createMockAdapter();
    const hook = vi.fn();
    svc.onDmReceived = hook;
    await svc.connectAdapter(adapter);

    const payload = {
      spaceId: 'aabbccdd',
      messageCid: 'cid-1',
      from: 'peer-hex',
      sentAt: 1,
      receivedAt: 2,
      body: hexEncode('hi'),
      mimeType: 'text/plain',
    };
    emit('dm-received', payload);
    emit('dm-received', payload); // duplicate delivery → deduped

    expect(hook).toHaveBeenCalledTimes(1);
    expect(hook).toHaveBeenCalledWith(expect.objectContaining({
      spaceId: 'aabbccdd',
      messageCid: 'cid-1',
      from: 'peer-hex',
      receivedAt: 2,
    }));
  });

  it('onDmReceived hook also fires for self-echoed DMs (service filters self itself)', async () => {
    const { adapter, emit } = createMockAdapter();
    const hook = vi.fn();
    svc.onDmReceived = hook;
    svc.ownAddress = 'my-own-hex';
    await svc.connectAdapter(adapter);

    emit('dm-received', {
      spaceId: 'aabbccdd',
      messageCid: 'self-cid-2',
      from: 'my-own-hex',
      sentAt: 1,
      receivedAt: 2,
      body: hexEncode('echo'),
      mimeType: 'text/plain',
    });

    expect(hook).toHaveBeenCalledTimes(1);
  });
```

- [ ] **Step 2: Run to verify they fail**

Run: `npx vitest run src/lib/message-service.test.ts`
Expected: FAIL — `onDmReceived` is not a property / hook never called.

- [ ] **Step 3: Implement**

(a) Add the field in `MessageService`, under `onChange` (line ~50):

```typescript
  /** Called whenever the message list changes so the UI can re-render. */
  onChange?: () => void;
  /** ZEB-666: post-dedup hook for `dm-received` — fires once per unique
   *  messageCid with the raw wire payload (DmUnreadService consumes
   *  spaceId/messageCid/from/receivedAt; it does its own self-filtering
   *  via selfOwnerId). Assign BEFORE connectAdapter (an optional-chained
   *  hook is a silent no-op; nothing replays it). */
  onDmReceived?: (payload: {
    spaceId: string;
    messageCid: string;
    from: string;
    sentAt: number;
    receivedAt: number;
    body: string;
    mimeType: string;
  }) => void;
```

(b) Fire it in the `dm-received` listener, immediately after the dedup lines (~165-166):

```typescript
        // Dedupe across reconnect/cold-start replay (messageCid is content-addressed).
        if (this.seenIds.has(payload.messageCid)) return;
        this.seenIds.add(payload.messageCid);
        // ZEB-666: unread tracking sees exactly one event per CID.
        this.onDmReceived?.(payload);
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/message-service.test.ts && npx tsc --noEmit`
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib/message-service.ts src/lib/message-service.test.ts
git commit -m "ZEB-666: MessageService — post-dedup onDmReceived hook"
```

---

### Task 5: App.svelte wiring — construction order, rehydration, clear site, shared store

App.svelte has no unit tests; this task's verification is `npx tsc --noEmit` + the full vitest suite + careful anchor placement. **Every anchor below is inside the Tauri-init IIFE unless noted.**

**Files:**
- Modify: `src/App.svelte` — declaration (~line 269), owner `$effect` (~line 1462), IIFE before `tryConnect('message', ...)` (~line 2099), DM rehydration after the community loop (~line 2152), `channelUnread` storage (~line 2185), `handleNodeClick` DM branch (~line 3058)

**Interfaces:**
- Consumes: `DmUnreadService` / `DmThreadPageEntry` (Task 2), `navService.onDmSpaceChange` + widened `setUnread` (Task 3), `messageService.onDmReceived` (Task 4), `list_owner_dm_spaces` IPC (Task 1), plus existing `invoke`, `adapter`, `navService`, `messageService`, `fileManagerService`, `selfOwnerId`, `appMode`, `activeChannel`, `activeChannelType`.

- [ ] **Step 1: Declare the service handle**

Next to the `channelUnread` declaration (~line 269):

```typescript
  // ZEB-665: per-channel unread tracker (created in the Tauri-transport IIFE).
  let channelUnread: import('./lib/channel-unread-service').ChannelUnreadService | null = null;
  // ZEB-666: DM/group-DM unread tracker (created in the Tauri-transport IIFE,
  // BEFORE messageService/navService connect — see the init-order comment there).
  let dmUnread: import('./lib/dm-unread-service').DmUnreadService | null = null;
```

- [ ] **Step 2: Extend the owner-connect `$effect`** (~line 1462)

```typescript
  // ZEB-665/ZEB-666: (re)connect the unread cursor stores when the owner
  // identity lands (or changes) — pre-identity the store no-ops all
  // reads/writes, and anything materialized before this point gets
  // re-seeded by connectOwner.
  $effect(() => {
    const oid = selfOwnerId;
    if (oid) {
      channelUnread?.connectOwner(oid);
      dmUnread?.connectOwner(oid);
    }
  });
```

- [ ] **Step 3: Construct the service + assign both hooks BEFORE `tryConnect('message', ...)`** (~line 2099)

Insert immediately above `await tryConnect('message', messageService.connectAdapter(adapter));`:

```typescript
      // ── ZEB-666: DM/group-DM unread counts. Constructed (and BOTH hooks
      // assigned) BEFORE messageService/navService connect so no dm-received
      // or nav-updated event can fire into a null hook (the ZEB-665 Qodo
      // lesson: an optional-chained hook is a silent no-op, and nothing
      // replays it). Seeds via read_dm_thread directly (newest-first page —
      // NOT messageService.loadDmThread, which ingests into the feed and
      // advances its own pagination cursor). Shares ONE cursor-store
      // instance with ChannelUnreadService below: both persist into the
      // same owner-scoped localStorage blob, so separate instances would
      // clobber each other's keys on every write.
      let unreadCursorStore:
        | import('./lib/unread-cursor-store').LocalStorageUnreadCursorStore
        | null = null;
      try {
        const { DmUnreadService } = await import('./lib/dm-unread-service');
        const { LocalStorageUnreadCursorStore } = await import('./lib/unread-cursor-store');
        unreadCursorStore = new LocalStorageUnreadCursorStore();
        const dm = new DmUnreadService({
          listThreadPage: (spaceId, limit) =>
            invoke('read_dm_thread', { spaceId, limit, beforeHlc: null }) as Promise<
              import('./lib/dm-unread-service').DmThreadPageEntry[]
            >,
          setUnread: (spaceId, count) => navService.setUnread(spaceId, count),
          // Same "looking right at it" contract as channelUnread's
          // isActiveChannel, in DM terms.
          isActiveThread: (spaceId) =>
            appMode === 'messages' &&
            activeChannel === spaceId &&
            (activeChannelType === 'dm' || activeChannelType === 'group-chat'),
          isFocused: () => document.hasFocus(),
          selfOwnerId: () => selfOwnerId ?? null,
          storage: unreadCursorStore,
          now: () => Date.now(),
        });
        dmUnread = dm;
        navService.onDmSpaceChange = (action, spaceId) => {
          if (action === 'added') void dm.onDmSpaceMaterialized(spaceId);
          else dm.onDmSpaceRemoved(spaceId);
        };
        messageService.onDmReceived = (p) => dm.onDmReceived(p);
        // Owner may already be known (the $effect only re-fires on change).
        if (selfOwnerId) dm.connectOwner(selfOwnerId);
        fileManagerService.addUnlisten(() => { dmUnread = null; });
      } catch (e) {
        console.warn(
          '[harmony-client] dm-unread init failed:',
          e instanceof Error ? e.message : String(e),
        );
      }
```

- [ ] **Step 4: DM rehydration loop after the community rehydration** (~line 2152, right after the `communityService.listOwnerCommunities()` try/catch)

```typescript
      // ZEB-666: rehydrate persisted DM / group-DM rows the same way — the
      // nav otherwise boots with NO DM rows (push-only; only runtime
      // nav-updated events re-create them), which would strand the persisted
      // read cursors. Pull (not a boot emit) for the same no-race reason;
      // addOrUpdateNavSpace is cold-replay idempotent, and each row fires
      // onDmSpaceChange → unread seeding. Non-fatal.
      try {
        const dms = (await invoke('list_owner_dm_spaces')) as Array<{
          spaceId: string;
          kind: 'dm' | 'group-dm';
          name: string;
          members: string[];
        }>;
        for (const d of dms) {
          navService.addOrUpdateNavSpace({
            action: 'added',
            spaceId: d.spaceId,
            kind: d.kind,
            name: d.name,
            members: d.members,
          });
        }
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        console.warn('[harmony-client] dm rehydration failed:', msg);
      }
```

- [ ] **Step 5: Share the cursor store with `channelUnread`** (~line 2185)

Replace `storage: new LocalStorageUnreadCursorStore(),` in the `channelUnread` construction with:

```typescript
          // ZEB-666: shared with DmUnreadService — separate instances would
          // clobber each other's keys in the same owner-scoped blob.
          storage: unreadCursorStore ?? new LocalStorageUnreadCursorStore(),
```

(The `new` fallback keeps channel unread alive if the dm-unread init failed; in that case there is no second writer, so the single instance is still safe.)

- [ ] **Step 6: Clear site in `handleNodeClick`'s DM branch** (~line 3058)

```typescript
    if (node.type === 'dm' || node.type === 'group-chat') {
      // ZEB-666: opening the thread clears its unread badge (open-clears-all;
      // stamps the cursor past maxSeen so replays don't resurrect it).
      dmUnread?.markThreadRead(node.id);
      messageService.loadDmThread(node.id).catch((e) => {
        console.error('loadDmThread failed:', e);
      });
    }
```

- [ ] **Step 7: Verify**

Run: `npx tsc --noEmit && npx vitest run`
Expected: clean / all PASS.

- [ ] **Step 8: Commit**

```bash
git add src/App.svelte
git commit -m "ZEB-666: wire DM unread — boot rehydration, hooks before connect, open-clears-all"
```

---

### Task 6: Full gates, spec/plan docs, PR

- [ ] **Step 1: Commit the spec + plan** (kept uncommitted until the branch existed)

```bash
git add docs/specs/2026-07-09-zeb-666-dm-unread-design.md docs/plans/2026-07-09-zeb-666-dm-unread-plan.md
git commit -m "ZEB-666: design spec + implementation plan"
```

- [ ] **Step 2: Rust gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
scripts/test-select --context task   # from repo root; paste round=… bucket=… into the report
```

Expected: fmt clean, clippy clean, selected tests PASS.

- [ ] **Step 3: Frontend gates**

```bash
npx tsc --noEmit && npx vitest run
```

Expected: clean / all PASS.

- [ ] **Step 4: Push and open the PR**

```bash
git push -u origin zeb-666-dm-unread-badges
gh pr create --repo zeblithic/harmony-client --title "ZEB-666: DM unread badges — list_owner_dm_spaces rehydration + per-thread read cursors" --body "<summary per repo convention>"
```

Then fire `@coderabbitai review` once, and converge bot/CI reviews.
