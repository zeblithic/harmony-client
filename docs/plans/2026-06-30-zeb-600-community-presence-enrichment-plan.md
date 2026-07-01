# Community Presence Enrichment (ZEB-600) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the shipped ZEB-537 community presence useful at a glance (online count, sort-online-first, cross-community sidebar + DM dots) and add an "appear offline" (invisible) mode.

**Architecture:** Extend existing infrastructure — no new topics or transports. Backend gains a global `presence_visible: Arc<AtomicBool>` that the presence publisher checks each tick (skip `session.put` when invisible), persisted via the existing `PkarrSettings`/`connectivity-settings.json` with fail-closed-to-invisible semantics. Frontend drives `PresenceService.subscribe` for all joined communities (not just active), adds three roster accessors, and surfaces them in the member panel, the nav sidebar (`NavNodeRow`), and a settings toggle.

**Tech Stack:** Rust (Tauri backend), Svelte 5 (runes), TypeScript, `cargo nextest`, `vitest`.

## Global Constraints

- CI gates (all must pass): `cd src-tauri && cargo fmt --all -- --check`; `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`; `npx tsc --noEmit` (repo root); `npx vitest run` (repo root).
- Tauri IPC: Rust params `snake_case`; JS callers `camelCase`. Tauri auto-converts at the boundary.
- Tauri IPC error extraction (TS): `const msg = e instanceof Error ? e.message : String(e);`.
- Keychain isolation (ZEB-428): never construct `KeychainStore::new()` in test-reachable code; these tasks don't touch identity persistence, but any test that boots a node sets `HARMONY_DISABLE_KEYCHAIN=1` + `HARMONY_PASSPHRASE`.
- Per-task Rust scope: prefer `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(<name>)'` during dev; run `--all-targets` clippy + the integration test only as the task's final gate (a lib change relinks ~97 integration binaries under `--all-targets`, ~50 min cold — avoid it mid-loop).
- Presence is community-scoped; zenoh never loops a node's own beacon back, so "self" is always a frontend special-case (never in the local roster).
- Commit after each task. Do NOT put parent IDs (ZEB-533/ZEB-537) in any commit that will reach the PR body verbatim — reference only ZEB-600 (avoids the Linear auto-close cascade closing the parents).

---

## File Structure

| File | Responsibility | Task |
|---|---|---|
| `src-tauri/src/pkarr_settings.rs` | `presence_invisible` field + fail-closed-to-invisible | 1 |
| `src-tauri/src/community_presence.rs` | publisher visibility gate | 2 |
| `src-tauri/src/event_loop.rs` | thread `presence_visible` to publisher call | 2 |
| `src-tauri/src/lib.rs` | boot atomic from settings; store in NodeState; `set/get_presence_visibility` IPCs + registration | 2, 3 |
| `src/lib/presence-service.ts` | `onlineCount` / `hasOthersOnline` / `isOnlineAnywhere` accessors | 4 |
| `src/App.svelte` | subscribe-all-joined lifecycle; thread `presenceVersion` to NavPanel | 4, 6 |
| `src/lib/components/CommunityMembersPanel.svelte` | header online count + sort-online-first | 5 |
| `src/lib/components/NavNodeRow.svelte` (+ `NavTree.svelte`, `NavPanel.svelte`) | per-community + per-DM presence dot | 6 |
| `src/lib/connectivity-adapter.ts` | `getPresenceVisibility` / `setPresenceVisibility` bindings | 7 |
| `src/lib/components/NetworkDiscoverabilitySettings.svelte` | "Appear offline" toggle | 7 |
| `src/lib/components/MemberRow.svelte` | hollow self-dot when invisible | 7 |

---

## Task 1: Settings — `presence_invisible` field (fail-closed to invisible)

**Files:**
- Modify: `src-tauri/src/pkarr_settings.rs` (struct l.9-27, `default_*` helpers, `Default` l.52-60, `fail_closed_defaults` l.202-208)
- Test: same file `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `PkarrSettings.presence_invisible: bool` (serde default `false` = visible). `fail_closed_defaults()` sets it `true`.

- [ ] **Step 1: Write the failing tests**

Add to `pkarr_settings.rs` tests module:

```rust
    #[test]
    fn presence_invisible_defaults_visible() {
        // First-run product default: presence broadcasts (invisible = false).
        assert!(!PkarrSettings::default().presence_invisible);
    }

    #[test]
    fn presence_invisible_missing_field_defaults_visible() {
        // A pre-ZEB-600 settings file has no `presence_invisible` key; serde's
        // field default must fill it FALSE so existing users keep broadcasting.
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("legacy.json");
        std::fs::write(&path, r#"{"identity_discoverable":true}"#).expect("write");
        assert!(!PkarrSettings::load_or_default(&path).presence_invisible);
    }

    #[test]
    fn presence_invisible_fails_closed_to_invisible() {
        // A corrupt settings file must fail CLOSED = INVISIBLE: never silently
        // re-broadcast a user who had opted to appear offline. NB this is the
        // INVERSE direction of identity_discoverable (whose closed value is false).
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("connectivity-settings.json");
        std::fs::write(&path, b"{ not valid json").expect("write");
        assert!(PkarrSettings::load_or_default(&path).presence_invisible);
    }

    #[test]
    fn presence_invisible_round_trips() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("connectivity-settings.json");
        let mut s = PkarrSettings::default();
        s.presence_invisible = true;
        s.save(&path).expect("save");
        assert!(PkarrSettings::load_or_default(&path).presence_invisible);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(presence_invisible)'`
Expected: FAIL — `no field presence_invisible on type PkarrSettings` (compile error).

- [ ] **Step 3: Add the field + fail-closed value**

In `pkarr_settings.rs`, add to the struct (after `relays`, l.26):

```rust
    /// ZEB-600: user "appear offline" toggle. When true, the node suppresses
    /// its community-presence beacons (others see it offline; it still receives
    /// their presence). Default OFF (visible) — presence is a product default.
    #[serde(default)]
    pub presence_invisible: bool,
```

In `Default::default()` (l.53-59), add `presence_invisible: false,`.

In `fail_closed_defaults()` (l.203-207), add `presence_invisible: true,` — with a comment:

```rust
            // ZEB-600: fail closed = INVISIBLE. A corrupt file must never
            // silently re-broadcast a hidden user. This is the INVERSE of
            // identity_discoverable's closed value (false); presence's
            // restrictive value is "don't broadcast" = invisible = true.
            presence_invisible: true,
```

Also update the two existing full-struct literals in tests that construct `PkarrSettings { .. }` (search for `identity_discoverable:` inside `round_trip_save_then_load` l.335 and `round_trips_custom_relays` l.384) to add `presence_invisible: false,` so they still compile.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(presence_invisible)'`
Expected: PASS (4 tests). Also run `-E 'test(pkarr) or test(connectivity)'` to confirm no regressions in the existing settings tests.

- [ ] **Step 5: fmt + commit**

```bash
cd src-tauri && cargo fmt --all
git add src-tauri/src/pkarr_settings.rs
git commit -m "feat(zeb-600): presence_invisible setting, fail-closed to invisible"
```

---

## Task 2: Backend — publisher visibility gate + boot atomic + NodeState

**Files:**
- Modify: `src-tauri/src/community_presence.rs` (fn `spawn_community_presence_publisher` l.403-445; tick loop l.420-443)
- Modify: `src-tauri/src/event_loop.rs` (l.2988 `closing_for_presence`; publisher call l.3045-3059)
- Modify: `src-tauri/src/lib.rs` (NodeState presence fields l.994-998 + init None l.1622-1623 + teardown l.2148-2149/3124-3125; boot l.3708-3713; settings load l.7006-7008; event-loop spawn l.8937-9057; NodeState populate l.9247-9248)
- Test: `src-tauri/tests/community_presence_two_engine_integration.rs`

**Interfaces:**
- Produces: `spawn_community_presence_publisher(..., presence_visible: Arc<AtomicBool>, closing: Arc<AtomicBool>)` — new param inserted **before** `closing` (last position stays `closing` to match the existing final-arg convention; see Step 3 for exact order). `NodeState.presence_visible: Option<Arc<std::sync::atomic::AtomicBool>>`. Boot creates the atomic as `AtomicBool::new(!pkarr_settings.presence_invisible)`.
- Consumes: `PkarrSettings.presence_invisible` (Task 1).

- [ ] **Step 1: Write the failing integration test**

Open `src-tauri/tests/community_presence_two_engine_integration.rs`, read an existing two-engine test to copy its harness (node A publishes presence, node B sees A in its roster). Add a test that flips A invisible and asserts B's roster for A stays empty:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn invisible_publisher_emits_no_beacon() {
    // Two engines in one community. Node A is created with presence_visible=false
    // (invisible). B subscribes. Within > STALE_MS, B's roster must NOT contain A,
    // because an invisible node publishes no beacon.
    // (Mirror the existing two-engine setup in this file; the only delta is passing
    //  an AtomicBool(false) as A's presence_visible into A's publisher spawn.)
    // ... harness setup identical to the sibling `two_engine` test ...
    // let a_visible = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // spawn A's publisher with a_visible; spawn B's subscriber;
    // tokio::time::sleep(Duration::from_millis(community_presence::BEACON_INTERVAL_MS * 2)).await;
    // let roster = b_map.lock().await.online_owners(&community);
    // assert!(roster.iter().all(|o| o.owner != a_owner.0), "invisible A must not appear");
}
```

If the existing test spawns the publisher through a helper that doesn't expose `presence_visible`, thread the new param through that helper too (default the sibling test to `AtomicBool::new(true)`).

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(invisible_publisher_emits_no_beacon)'`
Expected: FAIL — compile error (arity of `spawn_community_presence_publisher`) until Step 3.

- [ ] **Step 3: Add the gate to the publisher**

In `community_presence.rs`, add the parameter to `spawn_community_presence_publisher` (insert before `closing: Arc<AtomicBool>`, keeping `closing` last):

```rust
    presence_visible: Arc<AtomicBool>,
    closing: Arc<AtomicBool>,
```

Inside the tick loop, right after the existing `closing` check (l.422-424), add:

```rust
            // ZEB-600: invisible mode — skip emitting our beacon (still cheap;
            // the subscriber keeps running so we keep seeing others). Peers
            // evict us within STALE_MS once we stop publishing.
            if !presence_visible.load(Ordering::SeqCst) {
                continue;
            }
```

- [ ] **Step 4: Thread the atomic through the event loop**

In `event_loop.rs`, the event-loop entry fn currently receives `community_presence_request_rx` (passed from `lib.rs:9057`). Add a parameter `presence_visible: Arc<std::sync::atomic::AtomicBool>` to that fn signature. At l.2988 (next to `let closing_for_presence = Arc::clone(&closing);`) add:

```rust
            let presence_visible_for_presence = Arc::clone(&presence_visible);
```

At the publisher call (l.3045-3059), pass `Arc::clone(&presence_visible_for_presence)` as the new second-to-last arg (before `Arc::clone(&closing_for_presence)`).

- [ ] **Step 5: Create the atomic at boot + store in NodeState + populate**

In `lib.rs`:
- Add to the `NodeState` struct (near l.994-998, beside `community_presence_map`):
  ```rust
      /// ZEB-600: presence-visibility gate shared with every presence publisher.
      /// Some(false) => invisible. None until a node is started.
      community_presence_visible: Option<Arc<std::sync::atomic::AtomicBool>>,
  ```
- Initialize `community_presence_visible: None,` at the two `NodeState { .. }` construction / reset sites (l.1622-1623 area and the teardown resets at l.2148-2149 / l.3124-3125 set it back to `None`).
- At the settings-load site (l.7006-7008), after `let pkarr_settings = ...`, create:
  ```rust
      let presence_visible = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
          !pkarr_settings.presence_invisible,
      ));
      let presence_visible_for_state = std::sync::Arc::clone(&presence_visible);
  ```
- Pass `std::sync::Arc::clone(&presence_visible)` into the event-loop spawn (the call around l.9057 that passes `community_presence_request_rx_for_loop`) as the new `presence_visible` arg.
- Where NodeState is populated post-spawn (l.9247-9248, beside `guard.community_presence_map = Some(...)`), add:
  ```rust
                          guard.community_presence_visible = Some(presence_visible_for_state);
  ```

(If `pkarr_settings` at l.7006 is scoped inside an `if let Some(seed)` that also contains the event-loop spawn, the clones flow naturally; if the spawn is outside that block, hoist the `presence_visible` creation to just before the spawn, still initialized from `pkarr_settings.presence_invisible`.)

- [ ] **Step 6: Run the gate test + fmt + clippy**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(invisible_publisher_emits_no_beacon) or test(community_presence)'`
Expected: PASS (new test + existing presence tests).
Then: `cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` (final gate; verify real exit via `${pipestatus[1]}` if piping).
Expected: clippy exit 0. If `spawn_community_presence_publisher` now exceeds 7 args, keep the existing `#[allow(clippy::too_many_arguments)]` (already present at l.402).

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/community_presence.rs src-tauri/src/event_loop.rs src-tauri/src/lib.rs src-tauri/tests/community_presence_two_engine_integration.rs
git commit -m "feat(zeb-600): presence publisher visibility gate + boot atomic from settings"
```

---

## Task 3: Backend — `set_presence_visibility` / `get_presence_visibility` IPCs

**Files:**
- Modify: `src-tauri/src/lib.rs` (define the two commands near the presence IPC trio l.31010-31093; mirror the `apply_pkarr_relays` load→mutate→save pattern l.45907-45937; register in `generate_handler!` l.50624-50626)
- Test: `src-tauri/src/lib.rs` inline `#[cfg(test)]` or a small integration test asserting persist round-trip through `PkarrSettings`.

**Interfaces:**
- Consumes: `NodeState.community_presence_visible` (Task 2); `PkarrSettings.presence_invisible` (Task 1).
- Produces IPCs: `set_presence_visibility(visible: bool) -> Result<(), String>`; `get_presence_visibility() -> Result<bool, String>`.

- [ ] **Step 1: Write the failing test**

Add a Rust test (inline `#[cfg(test)]` in `lib.rs` near other IPC tests, or a new `tests/presence_visibility_ipc.rs`) that exercises the persist helper directly (factor the load→mutate→save into a testable free fn, mirroring how relay-set is structured):

```rust
    #[test]
    fn set_visibility_persists_invisible_to_settings() {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("connectivity-settings.json");
        // helper: persist_presence_visibility(&path, false) => presence_invisible=true
        persist_presence_visibility(&path, false).expect("persist");
        assert!(pkarr_settings::PkarrSettings::load_or_default(&path).presence_invisible);
        persist_presence_visibility(&path, true).expect("persist");
        assert!(!pkarr_settings::PkarrSettings::load_or_default(&path).presence_invisible);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(set_visibility_persists)'`
Expected: FAIL — `persist_presence_visibility` not found.

- [ ] **Step 3: Implement helper + IPC commands + registration**

Add a free helper (near `apply_pkarr_relays`, mirroring its load→mutate→save):

```rust
fn persist_presence_visibility(path: &std::path::Path, visible: bool) -> Result<(), String> {
    let mut settings = pkarr_settings::PkarrSettings::load_or_default(&path.to_path_buf());
    settings.presence_invisible = !visible;
    settings
        .save(&path.to_path_buf())
        .map_err(|e| format!("save connectivity-settings: {e}"))
}
```

Add the two commands (near the presence trio at l.31010-31093), following the existing command style in this file (they read `tauri::State<'_, Mutex<NodeState>>`):

```rust
#[tauri::command]
async fn set_presence_visibility(
    state: tauri::State<'_, Mutex<NodeState>>,
    visible: bool,
) -> Result<(), String> {
    let (path, atomic) = {
        let g = state.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        (g.pkarr_settings_path.clone(), g.community_presence_visible.clone())
    };
    // Live effect: flip the shared atomic so publishers act immediately.
    if let Some(a) = atomic {
        a.store(visible, std::sync::atomic::Ordering::SeqCst);
    }
    // Durable: persist so an invisible user stays hidden across restarts.
    let path = connectivity_settings_path(path)?;
    persist_presence_visibility(&path, visible)
}

#[tauri::command]
async fn get_presence_visibility(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let g = state.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
    // Prefer the live atomic; fall back to persisted settings if not yet started.
    if let Some(a) = &g.community_presence_visible {
        return Ok(a.load(std::sync::atomic::Ordering::SeqCst));
    }
    let path = connectivity_settings_path(g.pkarr_settings_path.clone())?;
    Ok(!pkarr_settings::PkarrSettings::load_or_default(&path).presence_invisible)
}
```

(Use the same `connectivity_settings_path(...)` helper `apply_pkarr_relays` uses at l.45925. Confirm `NodeState.pkarr_settings_path` exists — it's read at l.45917.)

Register in the `generate_handler!` macro (l.50624-50626, right after the presence trio):

```rust
        set_presence_visibility,
        get_presence_visibility,
```

- [ ] **Step 4: Run test + fmt + clippy**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(set_visibility_persists)'`
Expected: PASS.
Then final gate: `cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` → exit 0.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-600): set/get_presence_visibility IPCs (live atomic + persist)"
```

---

## Task 4: Frontend — PresenceService accessors + subscribe-all-joined

**Files:**
- Modify: `src/lib/presence-service.ts` (add 3 accessors after `isOnline` l.176-181)
- Modify: `src/App.svelte` (subscribe-all at adapter-connect ~l.1793; keep switch working but stop unsubscribing others l.1080-1100; leave teardown l.1348-1350)
- Test: `src/lib/__tests__/presence-service.test.ts` (create if absent; check for an existing presence-service test first)

**Interfaces:**
- Produces: `PresenceService.onlineCount(communityId: string): number`; `PresenceService.hasOthersOnline(communityId: string, selfOwnerIdHex: string): boolean`; `PresenceService.isOnlineAnywhere(ownerIdHex: string): boolean`.
- Consumes (Task 6): the above accessors.

- [ ] **Step 1: Write the failing tests**

Create/extend `src/lib/__tests__/presence-service.test.ts`. Use a fake adapter (mirror the existing service tests in `src/lib/**/__tests__`). Seed two communities via `applyMembers` (exercise through `subscribe` + a fake `presence-updated`, or expose a test seam). Assert:

```ts
import { describe, it, expect } from 'vitest';
import { PresenceService } from '../presence-service';

// Minimal fake adapter: invoke resolves get_community_presence with seeded rows;
// listen returns a no-op unlisten. (Copy the fake-adapter shape from a sibling
// service test.)

describe('PresenceService accessors', () => {
  it('onlineCount counts online members in a community', async () => {
    const svc = makeSeeded({ c1: [{ ownerIdHex: 'aa', online: true }, { ownerIdHex: 'bb', online: true }] });
    expect(svc.onlineCount('c1')).toBe(2);
    expect(svc.onlineCount('unknown')).toBe(0);
  });

  it('hasOthersOnline excludes self', async () => {
    const svc = makeSeeded({ c1: [{ ownerIdHex: 'aa', online: true }] });
    expect(svc.hasOthersOnline('c1', 'aa')).toBe(false); // only self online
    expect(svc.hasOthersOnline('c1', 'zz')).toBe(true);  // aa is someone else
  });

  it('isOnlineAnywhere is true if online in any subscribed community', async () => {
    const svc = makeSeeded({ c1: [{ ownerIdHex: 'aa', online: true }], c2: [] });
    expect(svc.isOnlineAnywhere('AA')).toBe(true);  // case-insensitive
    expect(svc.isOnlineAnywhere('bb')).toBe(false);
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/__tests__/presence-service.test.ts`
Expected: FAIL — `svc.onlineCount is not a function`.

- [ ] **Step 3: Add the accessors**

In `presence-service.ts`, after `isOnline` (l.181), add:

```ts
  /** Count of online members in `communityId` (0 if unsubscribed/unknown). */
  onlineCount(communityId: string): number {
    const map = this.byCommunity.get(communityId);
    if (!map) return 0;
    let n = 0;
    for (const m of map.values()) if (m.online) n++;
    return n;
  }

  /**
   * True iff at least one member OTHER THAN `selfOwnerIdHex` is online in
   * `communityId`. Used for the sidebar dot ("someone besides you is around"),
   * so a community where only you are present does not light up. Self is never
   * in the roster (zenoh doesn't loop our own beacon), but we exclude defensively.
   */
  hasOthersOnline(communityId: string, selfOwnerIdHex: string): boolean {
    const map = this.byCommunity.get(communityId);
    if (!map) return false;
    const self = selfOwnerIdHex.toLowerCase();
    for (const m of map.values()) if (m.online && m.ownerIdHex.toLowerCase() !== self) return true;
    return false;
  }

  /** True iff `ownerIdHex` is online in ANY subscribed community (DM-list dot). */
  isOnlineAnywhere(ownerIdHex: string): boolean {
    const key = ownerIdHex.toLowerCase();
    for (const map of this.byCommunity.values()) {
      if (map.get(key)?.online) return true;
    }
    return false;
  }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/__tests__/presence-service.test.ts`
Expected: PASS (3 tests).

- [ ] **Step 5: Drive subscribe-all in App.svelte**

At the adapter-connect block (~l.1793, after `navService.connectAdapter(...)` and `presenceService.setAdapter(adapter)`), subscribe every joined community with a shared callback that bumps `presenceVersion`:

```ts
// ZEB-600: subscribe presence for ALL joined communities (not just active) so
// the sidebar/DM dots reflect every community. A single shared callback bumps
// presenceVersion; per-community failures are logged and skipped.
for (const n of navService.nodes.filter((n) => n.type === 'community')) {
  presenceService.subscribe(n.id, () => { presenceVersion++; })
    .catch((e) => console.error('presence subscribe-all failed for', n.id, e instanceof Error ? e.message : String(e)));
}
```

In the community-switch effect (l.1080-1100): **remove** the `unsubscribe(prevPresenceId)` call (all joined communities stay subscribed for the session). Keep a `subscribe(id, ...)` for the newly-selected community only if it may not already be subscribed (idempotent per the service's contract — a second subscribe replaces the callback; safe). On join of a NEW community (wherever `navService` gains a node), call `presenceService.subscribe(newId, () => presenceVersion++)`; on leave, `presenceService.unsubscribe(leftId)`. Leave the teardown-all at unmount (l.1348-1350) but broaden it to unsubscribe every subscribed community (iterate `navService.nodes`), or leave per-active — either is correct at app teardown.

- [ ] **Step 6: tsc + full frontend suite + commit**

Run: `npx tsc --noEmit && npx vitest run`
Expected: PASS.

```bash
git add src/lib/presence-service.ts src/App.svelte src/lib/__tests__/presence-service.test.ts
git commit -m "feat(zeb-600): presence accessors + subscribe-all-joined communities"
```

---

## Task 5: Frontend — member-panel online count + sort-online-first

**Files:**
- Modify: `src/lib/components/CommunityMembersPanel.svelte` (`joined` derived l.107-111; header l.250-259; the panel receives `isOnline` prop l.37)
- Test: `src/lib/components/__tests__/CommunityMembersPanel.test.ts` (create if absent; a sibling `ChannelMembersPanel.test.ts` exists to copy harness from)

**Interfaces:**
- Consumes: existing `isOnline?: (ownerIdHex: string) => boolean` prop (l.37).

- [ ] **Step 1: Write the failing test**

Copy the render harness from `src/lib/components/__tests__/ChannelMembersPanel.test.ts`. Provide a stub `communityService.listCommunityMembers` returning members `[A(offline), B(online), C(offline)]` and an `isOnline` that returns true for B. Assert (a) the header shows "2 online" (B + self, if self is a member) — pin the exact expected count to your self-handling; if the test's `ownAddress` is not among members, expect "1 online"; (b) the first rendered member row is B (online-first).

```ts
it('shows online count and sorts online-first', async () => {
  // members: A(offline), B(online), C(offline); ownAddress not a member.
  const { getByText, getAllByRole } = render(CommunityMembersPanel, { props });
  await tick();
  expect(getByText(/1 online/)).toBeTruthy();
  const rows = getAllByRole('listitem');
  // B (online) sorts before A/C
  expect(rows[0].textContent).toContain('B');
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `npx vitest run src/lib/components/__tests__/CommunityMembersPanel.test.ts`
Expected: FAIL — no "online" count text / order unchanged.

- [ ] **Step 3: Implement count + sort**

In `CommunityMembersPanel.svelte`, change the `joined` derived (l.107-111) to sort online-first (stable). Add an `onlineCount` derived. Self is online unless the caller's `isOnline` says otherwise — the panel only knows `isOnline`, so compute per-row online = `isOnline?.(m.address) ?? false`, and add `ownAddress` as online (matching `MemberRow`'s self-always-online) unless invisible is surfaced (invisible self-styling is Task 7; for the count, treat self as online here — Task 7 will refine if needed):

```ts
  function memberOnline(m: CommunityMember): boolean {
    return m.address === ownAddress || (isOnline?.(m.address) ?? false);
  }
  let joined = $derived(
    members
      .filter((m) => m.status === 'joined' && matchesSearch(m, searchTrimmed))
      // stable online-first: partition preserves original relative order.
      .slice()
      .sort((a, b) => Number(memberOnline(b)) - Number(memberOnline(a)))
  );
  let onlineCount = $derived(
    members.filter((m) => m.status === 'joined' && memberOnline(m)).length
  );
```

Note: `Array.prototype.sort` is stable in modern JS engines (V8/WebKit), so equal-key members keep the backend order. Update the header (l.251) to include the count:

```svelte
    <h2 class="panel-title">Community Members{#if onlineCount > 0} · {onlineCount} online{/if}</h2>
```

- [ ] **Step 4: Run test to verify it passes**

Run: `npx vitest run src/lib/components/__tests__/CommunityMembersPanel.test.ts`
Expected: PASS.

- [ ] **Step 5: tsc + commit**

Run: `npx tsc --noEmit`

```bash
git add src/lib/components/CommunityMembersPanel.svelte src/lib/components/__tests__/CommunityMembersPanel.test.ts
git commit -m "feat(zeb-600): member panel online count + sort-online-first"
```

---

## Task 6: Frontend — sidebar + DM presence dots (`NavNodeRow`)

**Files:**
- Modify: `src/lib/components/NavNodeRow.svelte` (add a presence dot for community + dm node types)
- Modify: `src/lib/components/NavTree.svelte` (l.34-56, pass a presence resolver down) and `src/lib/components/NavPanel.svelte` (l.297/365, pass resolver + `presenceVersion` in)
- Modify: `src/App.svelte` (pass `presenceService`-backed resolvers + `presenceVersion` to `NavPanel`)
- Test: `src/lib/components/__tests__/NavNodeRow.test.ts` (create if absent)

**Interfaces:**
- Consumes: `PresenceService.hasOthersOnline(communityId, selfOwnerIdHex)`, `PresenceService.isOnlineAnywhere(ownerIdHex)` (Task 4); `presenceVersion` reactive counter (App.svelte).

- [ ] **Step 1: Write the failing test**

Create `NavNodeRow.test.ts`. Render a community node with a `presenceDot` prop true → assert a `.nav-presence-dot` element with `aria-label="Online"` exists; render with false → the dot is absent (or `.online` class absent). Render a `dm` node likewise.

```ts
it('renders a presence dot when someone is online', async () => {
  const { container } = render(NavNodeRow, { props: { node: communityNode, presenceOnline: true, /* ...required props */ } });
  expect(container.querySelector('.nav-presence-dot.online')).toBeTruthy();
});
it('no online dot when nobody is around', async () => {
  const { container } = render(NavNodeRow, { props: { node: communityNode, presenceOnline: false, /* ... */ } });
  expect(container.querySelector('.nav-presence-dot.online')).toBeNull();
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `npx vitest run src/lib/components/__tests__/NavNodeRow.test.ts`
Expected: FAIL — no `.nav-presence-dot`.

- [ ] **Step 3: Add a `presenceOnline` prop + dot to NavNodeRow**

In `NavNodeRow.svelte`, add an optional prop `presenceOnline?: boolean` to the `$props()` destructure. Near the node's icon/avatar, render:

```svelte
{#if presenceOnline}
  <span class="nav-presence-dot online" role="img" aria-label="Online" title="Online"></span>
{/if}
```

Add CSS mirroring `MemberRow`'s dot (8px, `#3ba55d` when online):

```css
  .nav-presence-dot { width: 7px; height: 7px; border-radius: 50%; flex-shrink: 0; }
  .nav-presence-dot.online { background: #3ba55d; }
```

- [ ] **Step 4: Thread the resolver from App.svelte → NavPanel → NavTree → NavNodeRow**

Add a prop `presenceResolver?: (node: NavNode) => boolean` to `NavPanel` and `NavTree`; in `NavTree` (l.34-56) compute `presenceOnline={presenceResolver?.(child) ?? false}` and pass it into `NavNodeRow`. In `App.svelte`, where `NavPanel` is rendered, pass:

```ts
presenceResolver={(node) => {
  void presenceVersion; // reactive dependency so dots update live
  if (node.type === 'community') return presenceService.hasOthersOnline(node.id, ownAddressHex);
  if ((node.type === 'dm' || node.type === 'group-chat') && node.peer) return presenceService.isOnlineAnywhere(node.peer.address);
  return false;
}}
```

(Confirm the self owner-id hex variable name in App.svelte — search for the value passed as `ownAddress` to `CommunityMembersPanel`; reuse it as `ownAddressHex`.)

- [ ] **Step 5: Run test + tsc + commit**

Run: `npx vitest run src/lib/components/__tests__/NavNodeRow.test.ts && npx tsc --noEmit`
Expected: PASS.

```bash
git add src/lib/components/NavNodeRow.svelte src/lib/components/NavTree.svelte src/lib/components/NavPanel.svelte src/App.svelte src/lib/components/__tests__/NavNodeRow.test.ts
git commit -m "feat(zeb-600): per-community + per-DM presence dots in nav sidebar"
```

---

## Task 7: Frontend — "Appear offline" toggle + hollow self-dot

**Files:**
- Modify: `src/lib/connectivity-adapter.ts` (add `getPresenceVisibility` / `setPresenceVisibility`, mirroring `getIdentityDiscoverable` / `setIdentityDiscoverable`)
- Modify: `src/lib/components/NetworkDiscoverabilitySettings.svelte` (add the toggle, mirroring the `enabled`/`pending`/`error` pattern)
- Modify: `src/lib/components/MemberRow.svelte` (`online` derived l.150; add `selfInvisible` prop; dot CSS l.261-273)
- Modify: `src/lib/components/CommunityMembersPanel.svelte` + `src/App.svelte` (thread `selfInvisible` down so self renders hollow)
- Test: `src/lib/components/__tests__/MemberRow.test.ts` (exists) + extend `NetworkDiscoverabilitySettings` test if present.

**Interfaces:**
- Consumes: `set_presence_visibility` / `get_presence_visibility` IPCs (Task 3).
- Produces: `MemberRow` prop `selfInvisible?: boolean`.

- [ ] **Step 1: Write the failing MemberRow test**

In `src/lib/components/__tests__/MemberRow.test.ts`, add:

```ts
it('renders self dot hollow when invisible', async () => {
  const { container } = render(MemberRow, { props: { member: selfMember, viewer, isOnline: () => false, selfInvisible: true } });
  const dot = container.querySelector('.presence-dot');
  expect(dot?.classList.contains('online')).toBe(false); // hollow, not solid green
  expect(dot?.getAttribute('aria-label')).toBe('Appear offline');
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `npx vitest run src/lib/components/__tests__/MemberRow.test.ts`
Expected: FAIL — self still renders online (solid).

- [ ] **Step 3: MemberRow — honor `selfInvisible`**

In `MemberRow.svelte`, add `selfInvisible` to `$props()`. Change the `online` derived (l.150) so an invisible self is not shown solid-green:

```ts
  let online = $derived(
    isSelf ? !selfInvisible : (isOnline ? isOnline(member.address) : false)
  );
```

Update the dot's labels for the invisible-self case (l.190-191):

```svelte
    title={isSelf && selfInvisible ? 'Appear offline' : (online ? 'Online' : 'Offline')}
    aria-label={isSelf && selfInvisible ? 'Appear offline' : (online ? 'Online' : 'Offline')}
```

(The offline `.presence-dot` style is already the hollow/muted style, so an invisible self reuses it — good.)

- [ ] **Step 4: Adapter bindings + settings toggle**

In `connectivity-adapter.ts`, add (mirroring `getIdentityDiscoverable`/`setIdentityDiscoverable`):

```ts
export async function getPresenceVisibility(): Promise<boolean> {
  return (await adapter.invoke('get_presence_visibility', {})) as boolean;
}
export async function setPresenceVisibility(visible: boolean): Promise<void> {
  await adapter.invoke('set_presence_visibility', { visible });
}
```

In `NetworkDiscoverabilitySettings.svelte`, add an "Appear offline" toggle mirroring the existing discoverability toggle's `enabled`/`pending`/`error` state and load-on-mount pattern. Note the inversion: the UI toggle is "Appear offline" = invisible, so `checked = !visible`; on change call `setPresenceVisibility(!checked)`; seed with `visible = await getPresenceVisibility()` then `checked = !visible`. Label copy: "Appear offline" with helper text "Others won't see you as online. You'll still see them."

- [ ] **Step 5: Thread `selfInvisible` to the member panel**

In `App.svelte`, hold `presenceInvisible` state seeded from `getPresenceVisibility()` (and updated when the toggle flips — simplest: re-read on settings close, or expose a shared store). Pass `selfInvisible={presenceInvisible}` to `CommunityView` → `CommunityMembersPanel` → each `MemberRow` for the self row. (`CommunityMembersPanel` passes it through to `MemberRow` alongside `isOnline`.)

- [ ] **Step 6: Run tests + tsc + full gate + commit**

Run: `npx vitest run && npx tsc --noEmit`
Expected: PASS.

```bash
git add src/lib/connectivity-adapter.ts src/lib/components/NetworkDiscoverabilitySettings.svelte src/lib/components/MemberRow.svelte src/lib/components/CommunityMembersPanel.svelte src/App.svelte src/lib/components/__tests__/MemberRow.test.ts
git commit -m "feat(zeb-600): appear-offline toggle + hollow self-dot when invisible"
```

---

## Final gate (before PR)

- [ ] `cd src-tauri && cargo fmt --all -- --check` → clean
- [ ] `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` → exit 0
- [ ] `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` → all pass (watch for the ~50min cold relink; budget a long wall-clock, run foreground with a timeout or ScheduleWakeup)
- [ ] `npx tsc --noEmit` (repo root) → clean
- [ ] `npx vitest run` (repo root) → all pass
- [ ] Push branch; open PR referencing **only ZEB-600** in the body (no parent IDs); PR title `ZEB-600: community presence enrichment (count, sort, cross-community dots, invisible mode)`.

## Self-review notes (spec coverage)

- Spec Piece 1 (count) → Task 5. Piece 2 (sort) → Task 5. Piece 3 (subscribe-all + accessors + sidebar + DM) → Tasks 4 & 6. Piece 4 (invisible: gate + persist + IPC + toggle + self-styling) → Tasks 1, 2, 3, 7.
- Testing rows in spec → Task 1 (settings fail-closed/round-trip), Task 2 (publisher gate integration), Task 4 (accessors + subscribe-all), Task 5 (count/sort), Task 6 (dots), Task 7 (toggle + self-styling).
- Non-goals (idle/away, per-channel presence, last-seen) → not implemented, by design.
