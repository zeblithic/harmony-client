# ZEB-209 mock-clear policy — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring `MessageService`, `VineService`, `NavService` into compliance with the ZEB-146 clear-on-connect contract by clearing mock-seeded state at the top of `connectAdapter()` before listener setup.

**Architecture:** Synchronous clear of mock-seeded fields immediately after the idempotency guard inside each service's `connectAdapter()`, followed by a single `onChange?.()` to re-render. JS event-loop semantics guarantee no listener callback fires before the clear completes.

**Tech Stack:** TypeScript / Svelte 5 frontend, vitest unit tests, no Rust or backend changes.

**Spec:** `docs/specs/2026-05-14-zeb-209-mock-clear-policy-design.md` (commit `0ea34da`).

**Branch:** `zeb-209-mock-clear-policy` cut from `origin/main` at `a8e88cc` (ZEB-103 lineage).

---

## Pre-flight notes

- Working directory: `/Users/zeblith/work/zeblithic/harmony-client`. No worktrees.
- Cargo commands run from `src-tauri/`; frontend commands from repo root.
- 5 CI gates: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `npx tsc --noEmit`, `npx vitest run`.
- Per `feedback_ci_disabled` memory: harmony-client's GitHub CI is currently disabled (ci.yml→ci.yml.disabled). Pretend CI is green; local gates are authoritative.

---

## Task 0: Pre-flight verification

**Goal:** Confirm green baseline before changing anything. No commit.

**Files:** None modified.

- [ ] **Step 1: Verify branch + clean working tree**

  ```bash
  git status
  git log --oneline -3
  ```

  Expected: `On branch zeb-209-mock-clear-policy`, clean working tree (besides the just-committed spec doc), HEAD at the spec commit on top of `a8e88cc`.

- [ ] **Step 2: Frontend baseline gates**

  ```bash
  npx tsc --noEmit
  npx vitest run
  ```

  Expected: both exit 0. Capture vitest test count for later comparison.

- [ ] **Step 3: Rust baseline gates**

  ```bash
  cd src-tauri
  cargo fmt --all -- --check
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
  cargo nextest run --locked --workspace --all-targets --features test-fixtures
  cd ..
  ```

  Expected: all three exit 0.

- [ ] **Step 4: No commit** — Task 0 is verification only.

---

## Task 1: MessageService clear-on-connect

**Files:**
- Modify: `src/lib/message-service.ts:71-75` (constructor comment) and `:77-92` (`connectAdapter` body — add clear).
- Modify: `src/lib/message-service.test.ts` (append new tests).

- [ ] **Step 1: Write the failing tests**

  Append the following block to `src/lib/message-service.test.ts` inside the existing top-level `describe('MessageService', ...)` (locate the existing close-of-describe `});` — insert before it):

  ```ts
    // ── ZEB-209: clear-on-connect ─────────────────────────────────

    it('clears mock-seeded messages on connectAdapter (ZEB-209)', async () => {
      const svc = new MessageService();
      // Sanity: constructor seeds from mockMessages so the UI is never empty
      // in browser/dev mode (no adapter connects).
      expect(svc.messages.length).toBeGreaterThan(0);
      const { adapter } = createMockAdapter();
      await svc.connectAdapter(adapter);
      expect(svc.messages).toEqual([]);
    });

    it('fires onChange once after clearing mocks (ZEB-209)', async () => {
      const svc = new MessageService();
      let calls = 0;
      svc.onChange = () => { calls++; };
      const { adapter } = createMockAdapter();
      await svc.connectAdapter(adapter);
      // At least one onChange — listener setup is async but the clear is
      // synchronous so the post-clear notification fires before any event.
      expect(calls).toBeGreaterThanOrEqual(1);
    });

    it('no longer dedupes events whose id collided with a former mock (ZEB-209)', async () => {
      const svc = new MessageService();
      // Pick any mock id — it was seeded into seenIds in the constructor.
      const collidingId = svc.messages[0]!.id;
      const { adapter, emit } = createMockAdapter();
      await svc.connectAdapter(adapter);
      // A real event with the same id should now be accepted (post-clear
      // seenIds no longer contains the mock id).
      emit('message-received', {
        id: collidingId,
        senderAddress: 'aa'.repeat(32),
        senderName: 'Real Sender',
        channel: 'channel-1',
        hub: 'hub-1',
        text: 'real',
        timestamp: 1700000000000,
        priority: 'standard',
      } satisfies ChannelMessageEvent);
      expect(svc.messages.find((m) => m.id === collidingId)?.text).toBe('real');
    });
  ```

  If `ChannelMessageEvent` is not yet imported into the test file, add `ChannelMessageEvent` to the existing import from `./message-service`:

  ```ts
  import { MessageService, type ChannelMessageEvent } from './message-service';
  ```

  (Inspect the current import line; if `ChannelMessageEvent` is already imported, do nothing.)

- [ ] **Step 2: Run tests to verify they fail**

  ```bash
  npx vitest run src/lib/message-service.test.ts
  ```

  Expected: 3 new failures. The first two assert empty state after `connectAdapter` (currently mocks remain). The third asserts the colliding-id event is processed (currently dedupe blocks it).

- [ ] **Step 3: Implement clear-on-connect in MessageService**

  In `src/lib/message-service.ts`, locate the existing `connectAdapter` method (line 77-92). Replace its body so the start looks like:

  ```ts
    /** Connect a Tauri adapter and start listening for network messages. */
    async connectAdapter(adapter: TauriAdapter): Promise<void> {
      if (this.adapter) return; // already wired; prevent duplicate listeners
      this.adapter = adapter;

      // ZEB-209: clear mock-seeded state before subscribing to real events.
      // The constructor seeds mockMessages for browser/dev mode (no adapter
      // connects). In production the adapter always wires in, so the mocks
      // must go to avoid hybrid real+fictional state.
      this.messages = [];
      this.seenIds = new Set();
      this.onChange?.();

      const unlisten = await adapter.listen(
        'message-received',
        (event) => {
  ```

  (Keep the rest of `connectAdapter` exactly as it was.)

- [ ] **Step 4: Update the constructor comment**

  In `src/lib/message-service.ts`, replace the existing `constructor() { ... }` body comment (line 72) from:

  ```ts
      // Seed with mock data — real messages append on top.
  ```

  to:

  ```ts
      // Seed with mock data for browser/dev mode — `connectAdapter()` clears
      // these before subscribing to real events (ZEB-209).
  ```

- [ ] **Step 5: Run the new tests to verify they pass**

  ```bash
  npx vitest run src/lib/message-service.test.ts
  ```

  Expected: all green.

- [ ] **Step 6: Run the full vitest suite to catch regressions**

  ```bash
  npx vitest run
  npx tsc --noEmit
  ```

  Expected: both green. If a pre-existing test relied on mock data persisting after connect, fix the test (the spec change is intentional — the test was asserting the pre-ZEB-209 bug).

- [ ] **Step 7: Commit**

  ```bash
  git add src/lib/message-service.ts src/lib/message-service.test.ts
  git commit -m "$(cat <<'EOF'
  feat(zeb-209): MessageService clears mock state on connectAdapter

  Brings MessageService into compliance with the ZEB-146 clear-on-connect
  contract. Drops mock-seeded messages + seenIds inside connectAdapter()
  before listener setup. Three new tests cover post-clear state, the
  onChange notification, and the formerly-blocked colliding-id case.

  Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
  EOF
  )"
  ```

---

## Task 2: VineService clear-on-connect

**Files:**
- Modify: `src/lib/vine-service.ts:54-61` (constructor comment) and `:64-83` (`connectAdapter` body — add clear).
- Modify: `src/lib/vine-service.test.ts` (append new tests).

- [ ] **Step 1: Write the failing tests**

  Append to `src/lib/vine-service.test.ts` inside the existing top-level `describe(...)`:

  ```ts
    // ── ZEB-209: clear-on-connect ─────────────────────────────────

    it('clears mock-seeded vines on connectAdapter (ZEB-209)', async () => {
      const svc = new VineService();
      // Sanity: constructor seeds from mockVines so the UI is never empty
      // in browser/dev mode (no adapter connects).
      expect(svc.discoverVines.length).toBeGreaterThan(0);
      const { adapter } = createMockAdapter();
      await svc.connectAdapter(adapter);
      expect(svc.discoverVines).toEqual([]);
      expect(svc.followedVines).toEqual([]);
      expect(svc.viewedIds.size).toBe(0);
    });

    it('fires onChange once after clearing mocks (ZEB-209)', async () => {
      const svc = new VineService();
      let calls = 0;
      svc.onChange = () => { calls++; };
      const { adapter } = createMockAdapter();
      await svc.connectAdapter(adapter);
      expect(calls).toBeGreaterThanOrEqual(1);
    });

    it('no longer dedupes events whose id collided with a former mock vine (ZEB-209)', async () => {
      const svc = new VineService();
      const collidingId = svc.discoverVines[0]!.id;
      const { adapter, emit } = createMockAdapter();
      await svc.connectAdapter(adapter);
      emit('vine-received', {
        id: collidingId,
        creatorAddress: 'bb'.repeat(32),
        creatorName: 'Real Creator',
        createdAt: 1700000000,
        videoCid: 'real-cid',
        source: 'discover',
      });
      // The mock vine is gone; a real event with the same id is now accepted.
      const found = svc.discoverVines.find((v) => v.id === collidingId);
      expect(found?.creatorName).toBe('Real Creator');
    });
  ```

- [ ] **Step 2: Run tests to verify they fail**

  ```bash
  npx vitest run src/lib/vine-service.test.ts
  ```

  Expected: 3 new failures.

- [ ] **Step 3: Implement clear-on-connect in VineService**

  In `src/lib/vine-service.ts`, locate `connectAdapter` (line 64). Replace the start so it reads:

  ```ts
    /** Connect a Tauri adapter and start listening for vine descriptors. */
    async connectAdapter(adapter: TauriAdapter): Promise<void> {
      if (this.adapter) return; // already wired; prevent duplicate listeners
      this.adapter = adapter;

      // ZEB-209: clear mock-seeded state before subscribing to real events.
      // The constructor seeds mockVines for browser/dev mode (no adapter
      // connects). In production the adapter always wires in, so the mocks
      // must go to avoid hybrid real+fictional state (fictional CIDs lead
      // to dead-end UI clicks).
      this.discoverVines = [];
      this.followedVines = [];
      this.seenIds = new Set();
      this.viewedIds = new Set();
      this.reactionMap = new Map();
      this.likePending = new Set();
      this.onChange?.();

      const unlisten = await adapter.listen(
        'vine-received',
        (event) => {
  ```

  (Keep the rest of `connectAdapter` exactly as it was.)

- [ ] **Step 4: Update the constructor comment**

  In `src/lib/vine-service.ts`, replace the existing `constructor()` body comment (line 55) from:

  ```ts
      // Seed with mock data — real vines append on top.
  ```

  to:

  ```ts
      // Seed with mock data for browser/dev mode — `connectAdapter()` clears
      // these before subscribing to real events (ZEB-209).
  ```

- [ ] **Step 5: Run the new tests to verify they pass**

  ```bash
  npx vitest run src/lib/vine-service.test.ts
  ```

  Expected: all green.

- [ ] **Step 6: Run the full vitest suite to catch regressions**

  ```bash
  npx vitest run
  npx tsc --noEmit
  ```

  Expected: both green. The VineFeed component tests (`src/lib/components/__tests__/VineFeed.test.ts` etc.) construct their own VineService instances with explicit fixtures and don't call `connectAdapter`, so they should be unaffected — but verify.

- [ ] **Step 7: Commit**

  ```bash
  git add src/lib/vine-service.ts src/lib/vine-service.test.ts
  git commit -m "$(cat <<'EOF'
  feat(zeb-209): VineService clears mock state on connectAdapter

  Brings VineService into compliance with the ZEB-146 clear-on-connect
  contract. Drops mock-seeded vines, seenIds, viewedIds, reactionMap,
  and likePending inside connectAdapter() before listener setup. Three
  new tests cover post-clear state, the onChange notification, and the
  formerly-blocked colliding-id case.

  Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
  EOF
  )"
  ```

---

## Task 3: NavService clear-on-connect

**Files:**
- Modify: `src/lib/nav-service.ts:43-46` (constructor comment) and `:63-105` (`connectAdapter` body — add clear).
- Modify: `src/lib/nav-service.test.ts` (append new tests).

**Note on existing tests:** `nav-service.test.ts` has a `describe('NavService DM handling', ...)` block whose `beforeEach` calls `connectAdapter`. After this change, `nav.profiles` will be empty post-connect (was previously seeded with `mockProfileStore`). Tests in that block that depend on a mock profile being present must be inspected during Step 6. Most existing tests emit their own `profile-update` events to set up state, so they should pass — but verify.

- [ ] **Step 1: Write the failing tests**

  Append to `src/lib/nav-service.test.ts` as a **new top-level `describe`** block (after the existing `describe`s, before the file ends):

  ```ts
  describe('NavService mock-clear policy (ZEB-209)', () => {
    it('clears mock-seeded nodes and profiles on connectAdapter', async () => {
      const nav = new NavService();
      // Sanity: constructor seeds from mockNavNodes + mockProfileStore
      expect(nav.nodes.length).toBeGreaterThan(0);
      expect(nav.profiles.size).toBeGreaterThan(0);
      const { adapter } = createMockAdapter();
      await nav.connectAdapter(adapter);
      expect(nav.nodes).toEqual([]);
      expect(nav.profiles.size).toBe(0);
      nav.destroy();
    });

    it('fires onChange once after clearing mocks', async () => {
      const nav = new NavService();
      let calls = 0;
      nav.onChange = () => { calls++; };
      const { adapter } = createMockAdapter();
      await nav.connectAdapter(adapter);
      expect(calls).toBeGreaterThanOrEqual(1);
      nav.destroy();
    });
  });
  ```

- [ ] **Step 2: Run tests to verify they fail**

  ```bash
  npx vitest run src/lib/nav-service.test.ts
  ```

  Expected: 2 new failures.

- [ ] **Step 3: Implement clear-on-connect in NavService**

  In `src/lib/nav-service.ts`, locate `connectAdapter` (line 63). Replace the start so it reads:

  ```ts
    /** Connect a Tauri adapter and start listening for profile + nav updates. */
    async connectAdapter(adapter: TauriAdapter): Promise<void> {
      if (this.adapter) return;
      this.adapter = adapter;

      // ZEB-209: clear mock-seeded state before subscribing to real events.
      // The constructor seeds mockNavNodes + mockProfileStore for browser/
      // dev mode (no adapter connects). In production the adapter always
      // wires in, so the mocks must go to avoid mock channels/DMs that are
      // uninhabitable (no real Zenoh state behind them).
      this.nodes = [];
      this.profiles = new Map();
      this.onChange?.();

      const unlistenProfile = await adapter.listen(
        'profile-update',
        (event) => {
  ```

  (Keep the rest of `connectAdapter` exactly as it was.)

- [ ] **Step 4: Update the constructor comment**

  In `src/lib/nav-service.ts`, find the constructor (line 43) and add a brief preceding comment so it reads:

  ```ts
    constructor() {
      // Seed with mock data for browser/dev mode — `connectAdapter()` clears
      // these before subscribing to real events (ZEB-209).
      this.nodes = [...mockNavNodes];
      this.profiles = new Map(mockProfileStore);
    }
  ```

- [ ] **Step 5: Run the new tests to verify they pass**

  ```bash
  npx vitest run src/lib/nav-service.test.ts
  ```

  Expected: 2 new tests green. Note: if existing tests in this file fail because they relied on mock profile data persisting after connect, fix them by emitting the needed `profile-update` events in their setup. Per `feedback_test_drift_is_our_fault` — those are our tests to fix.

- [ ] **Step 6: Run the full vitest suite to catch regressions**

  ```bash
  npx vitest run
  npx tsc --noEmit
  ```

  Expected: both green. App.svelte and other consumers don't access `nav.profiles` directly until after real events arrive, so no UI regressions expected.

- [ ] **Step 7: Commit**

  ```bash
  git add src/lib/nav-service.ts src/lib/nav-service.test.ts
  git commit -m "$(cat <<'EOF'
  feat(zeb-209): NavService clears mock state on connectAdapter

  Brings NavService into compliance with the ZEB-146 clear-on-connect
  contract. Drops mock-seeded nodes + profiles inside connectAdapter()
  before listener setup. Two new tests cover post-clear state and the
  onChange notification.

  Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
  EOF
  )"
  ```

---

## Task 4: Final gate sweep + push + PR

**Goal:** Run all five CI gates, push the branch, open the PR with markdown-linked Linear refs and a test plan.

- [ ] **Step 1: Frontend gates**

  ```bash
  npx tsc --noEmit
  npx vitest run
  ```

  Expected: both green. New test count should be +8 (3 MessageService + 3 VineService + 2 NavService).

- [ ] **Step 2: Rust gates**

  ```bash
  cd src-tauri
  cargo fmt --all -- --check
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
  cargo nextest run --locked --workspace --all-targets --features test-fixtures
  cd ..
  ```

  Expected: all green. No Rust code changed, so any failure here indicates pre-existing drift on main (file a follow-up ticket per `feedback_unrelated_test_failures` if so).

- [ ] **Step 3: Inspect commit lineage**

  ```bash
  git log --oneline origin/main..HEAD
  ```

  Expected (4 commits on top of `a8e88cc`):

  ```
  feat(zeb-209): NavService clears mock state on connectAdapter
  feat(zeb-209): VineService clears mock state on connectAdapter
  feat(zeb-209): MessageService clears mock state on connectAdapter
  docs(zeb-209): mock-clear policy design spec
  ```

- [ ] **Step 4: Push the branch**

  ```bash
  git push -u origin zeb-209-mock-clear-policy
  ```

- [ ] **Step 5: Open the PR**

  ```bash
  gh pr create --title "ZEB-209: mock-clear policy across MessageService / VineService / NavService" --body "$(cat <<'EOF'
  ## Summary

  Brings `MessageService`, `VineService`, and `NavService` into compliance with the [ZEB-146](https://linear.app/zeblith/issue/ZEB-146) clear-on-connect contract already followed by `FileManagerService`. Each `connectAdapter()` now synchronously discards mock-seeded state at the top before subscribing to real Tauri IPC events, eliminating the hybrid real+fictional UI state in production builds.

  Closes [ZEB-209](https://linear.app/zeblith/issue/ZEB-209).

  Design spec: `docs/specs/2026-05-14-zeb-209-mock-clear-policy-design.md` (commit `0ea34da`).

  ## What changed

  - `MessageService.connectAdapter` clears `messages` + `seenIds` before listener setup.
  - `VineService.connectAdapter` clears `discoverVines`, `followedVines`, `seenIds`, `viewedIds`, `reactionMap`, `likePending` before listener setup.
  - `NavService.connectAdapter` clears `nodes` + `profiles` before listener setup.
  - All three services fire `onChange?.()` once after the clear so subscribed UI re-renders against the empty state.
  - Constructor comments updated to describe mocks as a browser/dev-mode demo aid only.
  - 8 new vitest cases (3 + 3 + 2) covering post-clear state, the `onChange` notification, and the formerly-blocked colliding-id event case.

  ## What did NOT change

  - Dev/browser mode behavior — mocks remain visible when no adapter connects (the only path where `connectAdapter` is never called).
  - `mockMessages` / `mockVines` / `mockNavNodes` / `mockProfileStore` themselves — still used for dev mode and unit-test fixtures.
  - Identity state (`ownAddress`, `ownDisplayName`) — orthogonal to mock-clear.
  - Rust / Tauri backend — pure frontend change.
  - `FileManagerService` — already compliant.

  Related but out-of-scope (separate tickets):

  - [ZEB-207](https://linear.app/zeblith/issue/ZEB-207) — `FileManagerService.getContentDetail` hardcodes `mockPeers[0]/[1]`.
  - [ZEB-208](https://linear.app/zeblith/issue/ZEB-208) — `mockCleanupRecommendations.reason` staleness.

  Context references:

  - [ZEB-146](https://linear.app/zeblith/issue/ZEB-146) — original clear-on-connect contract (FileManager).
  - [ZEB-148](https://linear.app/zeblith/issue/ZEB-148) — silent adapter-failure observability precedent.
  - [ZEB-32](https://linear.app/zeblith/issue/ZEB-32) — original "offline-first fallback" rationale, now obsoleted by [ZEB-215](https://linear.app/zeblith/issue/ZEB-215) / [ZEB-228](https://linear.app/zeblith/issue/ZEB-228) / [ZEB-286](https://linear.app/zeblith/issue/ZEB-286) shipping real data sources.

  ## Test plan

  - [x] `npx tsc --noEmit` — green
  - [x] `npx vitest run` — green, +8 new tests
  - [x] `cd src-tauri && cargo fmt --all -- --check` — green
  - [x] `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` — green
  - [x] `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` — green
  - [ ] Manual: launch the Tauri shell with a paired identity; confirm channel feed, vine feed, and nav tree all start empty after Zenoh connects (no Alice/Bob/IPFS-Crew/discover-vines leftovers).

  🤖 Generated with [Claude Code](https://claude.com/claude-code)
  EOF
  )"
  ```

- [ ] **Step 6: Capture the PR URL** and report it to the user. After the PR opens, enter the autonomous bot-review monitoring loop (CodeRabbit / Cursor / CodeAnt / Qodo — not Greptile, not CI). Address findings via fixup commits. Send pushover when the PR converges + becomes mergeable; wait for the user's merge decision.
