# ZEB-388 — "Share my key" affordance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose the local node's 64-byte transport identity pub as hex (read-only IPC) and give FriendsPanel a "My key" copy affordance, so a peer can add you via "Add friend by key" and the cross-WAN playbook's raw-key discovery is runnable.

**Architecture:** A read-only vertical slice with no new state — the value already lives in `NodeState.dm_identity_pub_64` (set in `start_node`). One Rust getter IPC (mirrors `connectivity_get_my_reachability_record`) → one thin `FriendService` TS wrapper (mirrors `addByKey`) → one FriendsPanel `action-block` (reuses the existing `handleCopy` clipboard pattern).

**Tech Stack:** Rust + Tauri IPC (`#[tauri::command]`), `hex` crate; TypeScript `FriendService`; Svelte 5 runes; vitest + `@testing-library/svelte`.

**Spec:** `docs/specs/2026-06-08-zeb-388-share-my-key-design.md`

**Gate commands (run from `src-tauri/` for Rust, repo root for frontend):**
- Rust fmt: `cd src-tauri && cargo fmt --all -- --check`
- Rust clippy: `cd src-tauri && cargo clippy --locked -p harmony-app --lib --bins --features test-fixtures --no-deps -- -D warnings`
- Rust test: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(get_my_identity_pub_hex)'`
- Frontend test: `npx vitest run src/lib/friend-service.test.ts` (Task 2) / `npx vitest run src/lib/components/FriendsPanel.test.ts` (Task 3)
- Frontend types: `npx tsc --noEmit`

**Discipline (per controller memory):** commit BEFORE running the heavy gate; 10-min wall-clock kill switch per cargo command; if a gate stalls, report `DONE_WITH_CONCERNS` rather than hanging. iroh/zenoh first-bind flakes are known-nonblocking — these tasks add NO iroh-binding tests, so any iroh/zenoh failure is unrelated.

---

### Task 1: Rust IPC — `connectivity_get_my_identity_pub_hex`

**Files:**
- Modify: `src-tauri/src/lib.rs` — add the command after `connectivity_get_my_reachability_record` (ends ~line 37103); register it in the `invoke_handler!` macro (~line 37721, next to `connectivity_get_my_reachability_record,`); add two tests in the connectivity-IPC `#[cfg(test)]` module after `get_my_reachability_returns_none_when_iroh_not_running_inner` (~line 43090).

- [ ] **Step 1: Write the two failing tests**

Add to the `#[cfg(test)]` module that already contains `get_my_reachability_returns_none_when_iroh_not_running` (right after that test's `_inner` fn, ~line 43090). This module already has `mock_app_with_default_node_state()`, `StdMutex`, and `NodeState` in scope.

```rust
    /// `connectivity_get_my_identity_pub_hex` returns `Ok(None)` before
    /// `start_node` captures an identity (no `dm_identity_pub_64`). Mirrors
    /// `get_my_reachability_returns_none_when_iroh_not_running`.
    #[tokio::test]
    async fn get_my_identity_pub_hex_returns_none_when_unset() {
        let app = mock_app_with_default_node_state();
        let state = app.state::<StdMutex<NodeState>>();
        let got = connectivity_get_my_identity_pub_hex(state)
            .await
            .expect("IPC must succeed");
        assert!(
            got.is_none(),
            "expected None when dm_identity_pub_64 is unset, got {got:?}"
        );
    }

    /// `connectivity_get_my_identity_pub_hex` returns the 128-char lowercase
    /// hex of `dm_identity_pub_64` — exactly what `add_friend_by_key` consumes.
    /// Sets the field directly, the same way `force_republish_wakes_publisher`
    /// installs `iroh_publisher_force`.
    #[tokio::test]
    async fn get_my_identity_pub_hex_encodes_the_64_byte_pub() {
        let app = mock_app_with_default_node_state();
        {
            let state_handle = app.state::<StdMutex<NodeState>>();
            let mut g = state_handle.lock().expect("NodeState lock");
            g.dm_identity_pub_64 = Some([0xAB; 64]);
        }
        let state = app.state::<StdMutex<NodeState>>();
        let got = connectivity_get_my_identity_pub_hex(state)
            .await
            .expect("IPC must succeed");
        assert_eq!(got, Some("ab".repeat(64)));
    }
```

- [ ] **Step 2: Run tests to verify they fail (compile error — fn undefined)**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(get_my_identity_pub_hex)'`
Expected: FAIL — `cannot find function connectivity_get_my_identity_pub_hex in this scope`.

- [ ] **Step 3: Add the IPC command**

Insert immediately after `connectivity_get_my_reachability_record`'s closing brace (~line 37103), before the `connectivity_list_peer_reachability` doc comment:

```rust
/// Returns the local node's 64-byte transport identity pub
/// (`X25519_pub(32) ‖ Ed25519_pub(32)`) as 128 lowercase hex chars — exactly
/// the value `add_friend_by_key` / `connectivity_discover_identity` consume, so
/// a peer can add you by key. `Ok(None)` before `start_node` captures an
/// identity (mirrors `connectivity_get_my_reachability_record`'s pre-start
/// `Ok(None)`). Read-only: the identity *pub* is public key material, not the
/// owner secret. (ZEB-388)
#[tauri::command(rename_all = "snake_case")]
async fn connectivity_get_my_identity_pub_hex(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Option<String>, String> {
    let g = state
        .lock()
        .map_err(|e| format!("NodeState poisoned: {e}"))?;
    Ok(g.dm_identity_pub_64.map(hex::encode))
}
```

- [ ] **Step 4: Register in the `invoke_handler!` macro**

At ~line 37721, add the line directly under `connectivity_get_my_reachability_record,`:

```rust
            connectivity_get_my_reachability_record,
            connectivity_get_my_identity_pub_hex,
            connectivity_list_peer_reachability,
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(get_my_identity_pub_hex)'`
Expected: PASS — 2 tests (`get_my_identity_pub_hex_returns_none_when_unset`, `get_my_identity_pub_hex_encodes_the_64_byte_pub`).

- [ ] **Step 6: Commit, then run the scoped gates**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-388): connectivity_get_my_identity_pub_hex IPC (hex of dm_identity_pub_64)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
cd src-tauri && cargo fmt --all -- --check \
  && cargo clippy --locked -p harmony-app --lib --bins --features test-fixtures --no-deps -- -D warnings
```
Expected: fmt clean (0 diff), clippy clean (0 warnings). If fmt reports a diff, run `cargo fmt --all` and `git commit --amend --no-edit`.

---

### Task 2: TS wrapper — `FriendService.getMyIdentityPubHex`

**Files:**
- Modify: `src/lib/friend-service.ts` — add a method near `addByKey` (~line 226).
- Test: `src/lib/friend-service.test.ts` — add two cases near the `addByKey` block (~line 173).

- [ ] **Step 1: Write the two failing tests**

Add inside the `describe('FriendService', ...)` block (the harness exposes `service` and `adapter`, connected via `await service.connectAdapter(adapter)`; `adapter.invoke` is a vitest mock — see the existing `addByKey` test):

```typescript
  it('getMyIdentityPubHex invokes connectivity_get_my_identity_pub_hex and returns the hex', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('ab'.repeat(64));
    const result = await service.getMyIdentityPubHex();
    expect(adapter.invoke).toHaveBeenCalledWith('connectivity_get_my_identity_pub_hex', {});
    expect(result).toBe('ab'.repeat(64));
  });

  it('getMyIdentityPubHex returns null when the node is not started', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(null);
    const result = await service.getMyIdentityPubHex();
    expect(result).toBeNull();
  });
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/friend-service.test.ts -t getMyIdentityPubHex`
Expected: FAIL — `service.getMyIdentityPubHex is not a function`.

- [ ] **Step 3: Add the wrapper method**

In `src/lib/friend-service.ts`, immediately after the `addByKey(...)` method (~line 250, before `setAutoAccept`), add:

```typescript
  /**
   * Returns the local node's 64-byte transport identity pub as 128 lowercase
   * hex chars — the value a peer pastes into "Add friend by key". `null` when
   * the node isn't started yet (owner identity not loaded). ZEB-388.
   */
  async getMyIdentityPubHex(): Promise<string | null> {
    return this.invoke<string | null>('connectivity_get_my_identity_pub_hex', {});
  }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/friend-service.test.ts -t getMyIdentityPubHex`
Expected: PASS — 2 cases.

- [ ] **Step 5: Type-check, commit**

```bash
npx tsc --noEmit
git add src/lib/friend-service.ts src/lib/friend-service.test.ts
git commit -m "feat(zeb-388): FriendService.getMyIdentityPubHex wrapper

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```
Expected: `tsc` clean (no output).

---

### Task 3: FriendsPanel "My key" copy affordance

**Files:**
- Modify: `src/lib/components/FriendsPanel.svelte` — add state, a `loadMyKey()` loader called from `onMount`, a `handleCopyMyKey()` handler, and a "My key" `action-block` immediately before the "Add friend by key" block (~line 528).
- Create: `src/lib/components/FriendsPanel.test.ts` — net-new component test (mirror `src/lib/components/Layout.test.ts`'s `@testing-library/svelte` harness).

- [ ] **Step 1: Write the failing component test**

Create `src/lib/components/FriendsPanel.test.ts`. The panel calls `listFriends`, `listPendingRequests`, `getAutoAccept`, `onFriendsChanged`, `onPendingRequestsChanged`, and (new) `getMyIdentityPubHex` on mount — the mock stubs all of them. `findByTestId` awaits the async onMount render.

```typescript
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import FriendsPanel from './FriendsPanel.svelte';
import type { FriendService } from '../friend-service';

const FULL_KEY = 'ab'.repeat(64); // 128 hex chars

function mockService(overrides: Partial<FriendService> = {}): FriendService {
  return {
    listFriends: vi.fn().mockResolvedValue([]),
    listPendingRequests: vi.fn().mockResolvedValue([]),
    getAutoAccept: vi.fn().mockResolvedValue(false),
    onFriendsChanged: vi.fn().mockReturnValue(() => {}),
    onPendingRequestsChanged: vi.fn().mockReturnValue(() => {}),
    getMyIdentityPubHex: vi.fn().mockResolvedValue(null),
    ...overrides,
  } as unknown as FriendService;
}

const writeText = vi.fn().mockResolvedValue(undefined);

beforeEach(() => {
  writeText.mockClear();
  Object.defineProperty(navigator, 'clipboard', {
    value: { writeText },
    configurable: true,
  });
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe('FriendsPanel — My key (ZEB-388)', () => {
  it('renders the key and copies the full hex to the clipboard', async () => {
    const service = mockService({
      getMyIdentityPubHex: vi.fn().mockResolvedValue(FULL_KEY),
    });
    const { findByTestId } = render(FriendsPanel, { props: { service } });

    const input = (await findByTestId('my-key-input')) as HTMLInputElement;
    expect(input.value).toBe(FULL_KEY);

    const btn = await findByTestId('my-key-copy-btn');
    await fireEvent.click(btn);
    expect(writeText).toHaveBeenCalledWith(FULL_KEY);
  });

  it('shows a neutral "start your node" message when no key is available', async () => {
    const service = mockService({
      getMyIdentityPubHex: vi.fn().mockResolvedValue(null),
    });
    const { findByTestId, queryByTestId } = render(FriendsPanel, { props: { service } });

    expect(await findByTestId('my-key-empty')).toBeTruthy();
    expect(queryByTestId('my-key-copy-btn')).toBeNull();
  });
});
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `npx vitest run src/lib/components/FriendsPanel.test.ts`
Expected: FAIL — `Unable to find an element by: [data-testid="my-key-input"]` (affordance not yet rendered).

- [ ] **Step 3: Add state + loader + handler to the `<script>`**

In `src/lib/components/FriendsPanel.svelte`, add state next to the add-by-key state (after line 73, `let addByKeyStatus = $state<string | null>(null);`):

```typescript
  // ── ZEB-388: my own identity pub hex (share for add-by-key) ───────────────
  let myKeyHex = $state<string | null>(null);
  let myKeyCopied = $state(false);
```

Add a loader function (place it next to `loadAutoAccept`, after line 130):

```typescript
  async function loadMyKey(): Promise<void> {
    try {
      myKeyHex = await service.getMyIdentityPubHex();
    } catch {
      // Non-fatal: leave myKeyHex null → neutral "start your node" state.
      myKeyHex = null;
    }
  }
```

Call it from the existing `onMount` (after `void loadAutoAccept();`, line 143):

```typescript
    void loadAutoAccept();
    void loadMyKey();
```

Add the copy handler next to `handleCopy` (after line 174):

```typescript
  async function handleCopyMyKey(): Promise<void> {
    if (!myKeyHex) return;
    try {
      await navigator.clipboard.writeText(myKeyHex);
      myKeyCopied = true;
      setTimeout(() => {
        myKeyCopied = false;
      }, 1500);
    } catch {
      // Clipboard unavailable (headless / permission); the hex stays visible
      // in the readonly input for manual copy. Mirrors handleCopy.
    }
  }
```

- [ ] **Step 4: Add the "My key" markup**

In the template, immediately before the `<!-- ── Phase 1b: Add friend by public key -->` block (~line 528), insert:

```svelte
  <!-- ── ZEB-388: My key (share so a peer can add you by key) ───────────── -->
  <div class="action-block" data-testid="my-key-section">
    <label class="add-label" for="my-key-input">My key</label>
    {#if myKeyHex}
      <div class="add-row">
        <input
          id="my-key-input"
          type="text"
          class="url-input"
          readonly
          value={myKeyHex}
          data-testid="my-key-input"
        />
        <button
          type="button"
          class="secondary-btn"
          onclick={handleCopyMyKey}
          data-testid="my-key-copy-btn"
        >
          {myKeyCopied ? 'Copied!' : 'Copy'}
        </button>
      </div>
      <p class="muted">Share this so a friend can add you with "Add friend by key".</p>
    {:else}
      <p class="muted" data-testid="my-key-empty">Start your node to view your key.</p>
    {/if}
  </div>
```

(`action-block`, `add-label`, `add-row`, `url-input`, `secondary-btn`, and `muted` classes all already exist in this component — the add-by-key block and the friend-URL copy button use them.)

- [ ] **Step 5: Run the test to verify it passes**

Run: `npx vitest run src/lib/components/FriendsPanel.test.ts`
Expected: PASS — 2 cases.

- [ ] **Step 6: Type-check, commit**

```bash
npx tsc --noEmit
git add src/lib/components/FriendsPanel.svelte src/lib/components/FriendsPanel.test.ts
git commit -m "feat(zeb-388): FriendsPanel 'My key' copy affordance

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```
Expected: `tsc` clean.

---

### Task 4: Docs — note the affordance in the cross-WAN playbook

**Files:**
- Modify: `docs/cross-wan-validation.md` — Step 2/3 reference the "My key" affordance so a tester knows where to obtain the hex.

- [ ] **Step 1: Update the playbook**

In `docs/cross-wan-validation.md`, the friend/DM path now has a real "share my key" surface. Add a short note in the "Add friend by key" context. Locate the **Step 3: Exchange** section and add this sentence at the end of its intro (before the numbered list), or wherever raw-key exchange is described:

```markdown
> To exchange raw identity keys directly (instead of a community invite),
> each tester opens **Friends → My key**, clicks **Copy**, and sends the 128-char
> hex to the other out-of-band; the receiver pastes it into **Add friend by key**.
> ("My key" shows "Start your node to view your key" until the node is up.)
```

If the playbook has no raw-key exchange paragraph, add the note under Step 3's heading as a blockquote. Keep it to the one blockquote above — do not restructure the playbook.

- [ ] **Step 2: Commit**

```bash
git add docs/cross-wan-validation.md
git commit -m "docs(zeb-388): point testers to Friends → My key for raw-key exchange

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Final verification (after all tasks)

Run the full scoped gates once more (single pass, all four commits in place):

```bash
cd src-tauri && cargo fmt --all -- --check \
  && cargo clippy --locked -p harmony-app --lib --bins --features test-fixtures --no-deps -- -D warnings \
  && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(get_my_identity_pub_hex)'
cd .. && npx tsc --noEmit \
  && npx vitest run src/lib/friend-service.test.ts src/lib/components/FriendsPanel.test.ts
```

`--all-targets` is deliberately NOT run locally (relink cost; CI is the authoritative `--all-targets` gate). None of these changes touch integration-test symbols, so the lib-scoped run is sufficient for local confidence.

---

## Self-Review

**1. Spec coverage:**
- IPC `connectivity_get_my_identity_pub_hex` returning `Ok(None)` / `Ok(Some(hex))` → Task 1. ✓
- Registered in `invoke_handler!` → Task 1 Step 4. ✓
- `FriendService.getMyIdentityPubHex` wrapper → Task 2. ✓
- FriendsPanel "My key" affordance: present-state (readonly hex + Copy reusing clipboard pattern) and null-state (neutral message) → Task 3. ✓
- Privacy (read-only getter, public key material) → enforced by design (no write path); documented in the IPC doc-comment. ✓
- Testing: Rust None + Some IPC tests (Task 1), wrapper command-name/args/passthrough + null (Task 2), component present/null (Task 3). ✓
- Out-of-scope items (QR, chunked hex, auto-refresh, other surfaces) — correctly omitted. ✓
- Playbook doc note (a spec consequence: "makes the cross-WAN playbook runnable") → Task 4. ✓

**2. Placeholder scan:** No TBD/TODO/"handle edge cases"/"similar to". Every code step shows complete code. Task 4 Step 1 has a small conditional ("if no raw-key paragraph…") but provides the exact text to insert either way — acceptable.

**3. Type consistency:** IPC name `connectivity_get_my_identity_pub_hex` is identical across Task 1 (Rust fn + registry), Task 2 (wrapper invoke string + test), and the spec. Return shape `Option<String>` (Rust) ↔ `string | null` (TS). `data-testid`s (`my-key-input`, `my-key-copy-btn`, `my-key-empty`, `my-key-section`) match between Task 3 markup and its test. State names (`myKeyHex`, `myKeyCopied`) and fn names (`loadMyKey`, `handleCopyMyKey`) are consistent. ✓
