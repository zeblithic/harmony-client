# ZEB-393 — Durable community membership + boot rehydration

**Status:** Design approved (scope decisions confirmed 2026-06-08)
**Ticket:** [ZEB-393](https://linear.app/zeblith/issue/ZEB-393) (Urgent)
**Author:** Claude (Opus 4.8), with Jake
**Verification machine:** Ildwyn (Windows, Playwright/CDP-drivable Tauri build)

---

## Problem — two independent bugs, not one

The ticket originally framed this as a single persistence bug: minted/joined
communities vanish *only* on a non-graceful exit. Investigation against the
current source (plus Jake's lived experience — communities have **always** been
session-only on his builds, every restart) shows that framing is wrong. There
are **two independent failure modes**, and the second one *masks* the first:

### Bug A — owner-state membership is not durable-on-commit

`create_community` and `redeem_invite` apply the new `Community` Space into the
in-memory owner-state CRDT via `apply_space_with_canonicalization`
(`lib.rs:16711` / `lib.rs:18587`), and they persist the *per-community* dir
(`communities/{id}/`). But neither calls `notify_dirty()` **or** `flush_now()`
on the owner-state `SyncEngine`. The only thing that subsequently writes
`owner_state_crdt.cbor` is one of four triggers in `owner_state_sync.rs`:

1. a **debounced** timer (~250ms) after a dirty signal,
2. an explicit `flush_now`,
3. an incoming remote publish that mutated state,
4. **graceful shutdown**.

So a Space row reaches `owner_state_crdt.cbor` only on the debounce/shutdown
path. Any non-graceful exit (crash / force-quit / OS kill / power loss / dev
SIGKILL-rebuild) before that flush loses it. Evidence in the ticket: on a dev
machine `owner_state_crdt.cbor` mtime was days stale while `communities/` held
~11 dirs minted since — Spaces that never reached owner-state.

### Bug B — boot never rehydrates the nav from persisted state

Even when `owner_state_crdt.cbor` *is* current (graceful shutdown), the UI
community sidebar still boots empty. The sidebar (`nav-service.ts`) is populated
**exclusively** by runtime `nav-updated` events emitted from the IPC handlers
during a session (`create_community` `lib.rs:16842`, `redeem_invite`
`lib.rs:18909`, `join_open_community` `lib.rs:19133`). At boot:

- `navService.connectAdapter` (`nav-service.ts:87`) only *registers* listeners
  — it never queries existing state.
- `communityService.connectAdapter` (`community-service.ts:133`) likewise only
  registers change-listeners.
- The IPC registry has **no enumerate-my-communities command** at all
  (`list_community_members` / `list_community_forks` both require a
  `communityId` you must already know).
- `App.svelte` boot (`1340-1449`) calls `start_node` → `get_owner_state` (reads
  only `ownerId`) → `listSharedSet` (hydrates a toggle mirror) → the
  `connectAdapter`s. Nothing replays persisted communities into the tree.

Result: the sidebar is push-only and session-scoped. On restart, the
`nav-updated` events are in the past and nothing replays them, so the tree boots
empty regardless of what's on disk. **This is the dominant, always-present bug.**

Because Bug B swallows communities on *every* boot, Bug A was never observable —
you can't notice persistence working or failing when the UI never reads it back.

---

## Scope (confirmed with Jake)

**In scope:**

- **Fix A — persist-on-commit, mint + join only.** Synchronously fence
  owner-state to disk via the existing `flush_now()` seam after
  `create_community` and `redeem_invite` apply their Space, before the IPC
  returns. (`flush_now` also republishes the state-root to the user's *other
  devices* — a desirable side effect for a mint/join.)
- **Fix B — boot rehydration, communities only.** A pull IPC
  `list_owner_communities` returns the persisted `Community` spaces from
  `crdt_state`; `App.svelte` calls it at boot and seeds the nav tree through the
  existing idempotent `addOrUpdateNavSpace` seam.

**Explicitly deferred (noted follow-ups, not this PR):**

- DM / folder boot rehydration. Almost certainly the same Bug-B class (same
  `crdt_state.spaces` map, other `SpaceKind`s; DM history loads via
  `read_dm_thread(spaceId)` which presupposes a known space id, and no boot
  "list DM threads → seed nav" path was found). To be confirmed and filed as a
  sibling ticket. The machinery added here extends to other kinds cleanly.
- Leave-path durability. `leave_community` mutates the *community-state* engine;
  the owner-state `left_at` Space mutation is projected later by a delta
  consumer, a different mechanism than the mint/join apply path.
- Crash-atomic per-community mint dir (temp-dir + rename + fsync). The current
  `communities/{id}/` write is temp+rename without fsync, but is peer-recoverable
  from the next state-root publish, so the durability payoff is low.
- Fork-lineage glyph (`forkedFrom`) on boot-rehydrated nodes. Nice-to-have; the
  boot sweep doesn't currently surface lineage. Keep the DTO minimal for v1.

---

## Design

### Fix A — persist-on-commit (Rust, `lib.rs`)

Both IPC handlers already snapshot their NodeState handles in a single std-guard
scope, then drop the guard before any `.await`. `NodeState.sync_engine:
Option<Arc<SyncEngine>>` exists at `lib.rs:549`.

1. **`create_community` (`lib.rs:16768`)** — add `g.sync_engine.clone()` to the
   snapshot tuple (`16793-16805`). After `create_community_inner(...).await?`
   (`16836`) and **before** the `nav-updated` emit (`16842`), fence:

   ```rust
   // ZEB-393 Bug A: durable-on-commit. The in-memory Space + per-community dir
   // are written, but owner_state_crdt.cbor only persists on debounce/shutdown.
   // Fence it to disk now so a non-graceful exit can't lose this membership.
   // Non-fatal: the mint already committed in-memory; a flush hiccup falls back
   // to the debounce path (still persists on the next dirty/shutdown).
   if let Some(engine) = sync_engine.as_ref() {
       if let Err(e) = engine.flush_now().await {
           tracing::warn!(error = %e, "create_community: owner-state flush_now failed");
       }
   }
   ```

2. **`redeem_invite` (`lib.rs:18797`)** — symmetric: add `g.sync_engine.clone()`
   to the snapshot tuple (`18820-18833`), and after
   `redeem_invite_inner(...).await?` (`18900`), before the emit (`18909`), the
   same `flush_now` block (warn label `redeem_invite:`).

`flush_now` serializes through the engine's internal task: locks `OwnerState`,
encrypts, content-store-puts, publishes, then `save_crdt` + `save_replay` via
`save_atomically` (temp + fsync + rename). It is `async`, returns
`Result<(), SyncError>`. The `Option` is `None` only pre-`start_node` (where a
mint can't happen anyway), so `None` → skip-and-warn is safe.

### Fix B — boot rehydration (Rust IPC + frontend seed)

**Rust — pure function (testable), DTO, IPC.** Place near the existing community
IPCs in `lib.rs`.

- **Pure fn** (mirrors the boot engine-spawn sweep at `lib.rs:3804`):

  ```rust
  /// ZEB-393 Bug B: owner-state Community spaces shaped for the nav sidebar.
  /// Filters to live (non-left) Community spaces; mirrors the boot
  /// engine-spawn sweep's predicate so the UI and the engine sweep agree on
  /// "which communities am I in."
  pub fn communities_for_nav(state: &OwnerState) -> Vec<CommunityNavDto> {
      state
          .spaces
          .iter()
          .filter(|(_, s)| s.kind == SpaceKind::Community && s.left_at.is_none())
          .map(|(id, s)| CommunityNavDto {
              space_id: hex::encode(id.0),
              name: s.name.clone(),
              is_invite_only: s.is_invite_only.unwrap_or(false),
              pending: s.pending_join_at.is_some(),
          })
          .collect()
  }
  ```

- **DTO:**

  ```rust
  #[derive(Debug, Clone, Serialize, PartialEq, Eq)]
  #[serde(rename_all = "camelCase")]
  pub struct CommunityNavDto {
      pub space_id: String,
      pub name: String,
      pub is_invite_only: bool,
      pub pending: bool,
  }
  ```

- **IPC** (register in the `invoke_handler!` registry):

  ```rust
  #[tauri::command]
  async fn list_owner_communities(
      state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
  ) -> Result<Vec<CommunityNavDto>, String> {
      let crdt_state = {
          let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
          g.crdt_state.clone().ok_or(OWNER_NOT_LOADED_MSG)?
      };
      let state = crdt_state.lock().await;
      Ok(communities_for_nav(&state))
  }
  ```

**Frontend — service method + boot seed.**

- `community-service.ts` — typed accessor:

  ```ts
  async listOwnerCommunities(): Promise<CommunityNavDto[]> {
    return this.invoke<CommunityNavDto[]>('list_owner_communities', {});
  }
  ```
  with `interface CommunityNavDto { spaceId: string; name: string; isInviteOnly: boolean; pending: boolean }`.

- `App.svelte` boot — after `navService.connectAdapter(adapter)` (`1449`), so the
  listener is live and the seed can't race a runtime emit. Non-fatal, wrapped
  like the other boot hydrations:

  ```ts
  // ZEB-393 Bug B: rehydrate persisted communities into the sidebar. The nav
  // tree is otherwise push-only/session-scoped and boots empty on restart.
  try {
    for (const c of await communityService.listOwnerCommunities()) {
      navService.addOrUpdateNavSpace({
        action: 'added',
        spaceId: c.spaceId,
        kind: 'community',
        name: c.name,
        pending: c.pending || undefined, // greyed if invite-only join not yet countersigned
      });
      communityService.noteKind(c.spaceId, c.isInviteOnly ? 'invite-only' : 'open');
    }
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    console.warn('[harmony-client] community rehydration failed:', msg);
  }
  ```
  `noteKind` is a tiny setter wrapping the existing private `knownKinds` map so
  rehydrated communities resolve `getKind()` correctly (else fork/settings
  affordances would read `unknown` until a runtime event). If a thin public
  setter is awkward, expose `knownKinds` population via the existing
  `createCommunity`/`redeemInvite` pattern equivalent.

`addOrUpdateNavSpace` for `kind:'community'`/`action:'added'` (`nav-service.ts:209`)
is cold-replay idempotent — a duplicate add preserves user-applied parentId /
expanded / unread state, and a later runtime `nav-updated` for the same
community updates in place. So the boot seed is safe against ordering.

### File / component map

| File | Change |
|---|---|
| `src-tauri/src/lib.rs` | Fix A: snapshot `sync_engine` + `flush_now` in `create_community` & `redeem_invite`. Fix B: `CommunityNavDto`, `communities_for_nav`, `list_owner_communities` IPC + registry entry. |
| `src/lib/community-service.ts` | `listOwnerCommunities()`, `CommunityNavDto` type, `noteKind()` setter. |
| `src/App.svelte` | Boot seed loop after `navService.connectAdapter`. |
| `src/lib/types.ts` (or service-local) | `CommunityNavDto` TS interface if shared. |

### Data flow (boot)

```
start_node  ──► owner_state_persist::load_crdt ──► crdt_state (in mem)
                                                       │
App.svelte boot: navService.connectAdapter (listener live)
   │
   └─► communityService.listOwnerCommunities()
          └─► IPC list_owner_communities ──► communities_for_nav(&crdt_state)
                 └─► [CommunityNavDto] ──► navService.addOrUpdateNavSpace(×N)
                        └─► sidebar shows persisted communities ✓
```

---

## Testing

**TDD units (red → green → commit):**

1. **Rust `communities_for_nav`** (pure, no Tauri): build an `OwnerState` with a
   live Community space, a *left* Community space (`left_at = Some`), a pending
   invite-only Community space, and a non-Community space. Assert output contains
   only the two live Community rows, with correct `name` / `is_invite_only` /
   `pending`, and excludes the left and non-Community rows. (Mirrors the
   `crdt_round_trip` test pattern in `owner_state_persist.rs` for building state.)
2. **Rust durability mechanism** (regression guard for Bug A): using the
   `SyncEngine` test harness (`owner_state_sync.rs` `flush_now_*` pattern —
   `InMemoryStub` content store, `KeyTree::derive(&[0u8;32])`, `tempdir` paths),
   apply a Community Space to the engine's `OwnerState`, call `flush_now().await`,
   then `load_crdt(&paths.crdt)` **without** a graceful shutdown and assert the
   Space is present. Proves `flush_now` yields durable-on-commit.
3. **Frontend `listOwnerCommunities`** (vitest, mock adapter): asserts it invokes
   `list_owner_communities` and returns typed rows.
4. **Frontend rehydration seed** (vitest): given a mocked `listOwnerCommunities`
   returning two communities (one pending), assert `navService` ends with two
   community nodes and the pending one carries `pending: true`. Test the smallest
   honest seam (extract a `seedCommunities(navService, rows)` helper if it makes
   the test clean).

**Live verification on Ildwyn (the integration gate for the handler wiring,
which is impractical to unit-test without a full Tauri `mock_app` + NodeState):**

- **Fix B:** mint a community → restart the app → community appears in the
  sidebar (was always empty before). Drive + screenshot via CDP.
- **Fix A:** mint a community → confirm the create IPC returned → hard-kill the
  process (`Stop-Process -Force`) → relaunch → community persists *and*
  rehydrates. Without Fix A, a kill shortly after mint loses it. (Launch via the
  `HARMONY_PASSPHRASE_FILE` throwaway-identity recipe, or coordinate a Jake-driven
  launch, per the keychain-passphrase gotcha.)

**Full suite:** `npx tsc --noEmit` + `npx vitest run` green; `cargo nextest run
--locked --all-targets --features test-fixtures` + `cargo clippy -D warnings` +
`cargo fmt --check` green.

---

## Risks / coordination

- **Boot-seed race:** mitigated by seeding *after* `navService.connectAdapter`
  (pull, listener already live) and by `addOrUpdateNavSpace` idempotency.
- **`flush_now` latency on mint:** bounded local work (encrypt + content-store
  put + atomic disk write); the publish is an mpsc send, not a network await.
  Acceptable for a deliberate user action.
- **`sync_engine == None`:** only pre-`start_node`; mint/join can't occur then.
  Skip-and-warn.
- **No open PRs / clean main** (both ZEB-396/404 merged), so no coordination
  collisions expected.

Closes ZEB-393 (Bug A + Bug B). Sibling follow-up (DM/folder rehydration) to be
filed after live confirmation.
