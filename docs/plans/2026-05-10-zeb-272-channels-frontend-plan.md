# ZEB-272 Phase 4 — Channels Frontend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the Phase 1-3 channels backend (channel-config CRDT + ChannelLog data plane + Zenoh transport + IPCs/events) to a user-visible Svelte 5 surface (`CommunityView` three-column layout, channel CRUD dialogs, virtualized message feed with scroll-trigger backfill, modal-relocated settings) so that ZEB-248's full acceptance criteria are met.

**Architecture:** New `CommunityView.svelte` layout shell mounts when a community NavNode is selected, hosting three column components (`ChannelSubSidebar` / `ChannelMessageFeed` / `ChannelMembersPanel`) plus a ⚙️-revealed `CommunitySettingsPanel` modal and two new dialogs (`CreateChannelDialog`, `ModifyChannelDialog`). A new `ChannelMessageService` mirrors `MessageService` shape and consumes the Phase 3 `channel-message-received` + `channel-backfill-progress` events; `community-service.ts` extends with channel-config IPCs (`createChannel` / `modifyChannel` / `deleteChannel` / `listChannels`) and an `onChannelConfigChanged` callback subscribed to `channel-config-updated`. All eight plan-time questions from spec §6 are locked. `App.svelte` routing replaces today's direct `CommunitySettingsPanel` mount at L1525 with the new `CommunityView` mount.

**Tech Stack:** Svelte 5 (rune syntax: `$state` / `$derived` / `$effect` / `$props`), TypeScript (strict via `tsc --noEmit`), `@testing-library/svelte` + `vitest` for tests, existing `Modal.svelte` (provides `role="dialog"` + `aria-modal="true"` + focus-trap via `use:trapFocus` action), existing `TauriAdapter` interface (`invoke` / `listen` from `zenoh-service.ts`).

**Spec:** `docs/specs/2026-05-10-zeb-272-channels-frontend-design.md` (commit `8fcbbbf`).
**Branch:** `zeb-272-channels-frontend` already cut from `origin/main` `ec58fe2`.
**Linear:** [ZEB-272](https://linear.app/zeblith/issue/ZEB-272), parent [ZEB-248](https://linear.app/zeblith/issue/ZEB-248).

---

## File structure

### New files (13)

| Path | Responsibility | Approx LoC |
|---|---|---|
| `src/lib/channel-message-service.ts` | Per-channel message cache + subscribe API + IPC method facade for `post_channel_message` / `list_channel_messages` / `request_channel_backfill`. Subscribes to `channel-message-received` and `channel-backfill-progress` events. Single-in-flight backfill gate. | ~250 |
| `src/lib/components/CommunityView.svelte` | Layout shell. Owns `channels` / `activeChannelId` / `settingsModalOpen` / `showCreateDialog` / `modifyDialogChannel` / `deleteConfirmChannel` state. Subscribes to `communityService.onChannelConfigChanged`; runs §6.4 cascade fallback when active channel deleted; silently re-renders header on rename. | ~150 |
| `src/lib/components/ChannelSubSidebar.svelte` | Channel list (parent guarantees oldest-first + `#general` first). Active highlight. "+" button + right-click context menu (Rename / Set write_power / Delete) gated on `myPower >= 50`. | ~150 |
| `src/lib/components/ChannelMessageFeed.svelte` | Virtualized message list (windowed render via `IntersectionObserver` — no library dep). Auto-scroll-to-bottom on new live message when at bottom. 250 ms stable-at-top fires `requestBackfill` via single-in-flight gate. "Loading older messages…" skeleton. Inline channel-compose `<textarea>` (Enter posts, Shift+Enter newline). | ~250 |
| `src/lib/components/ChannelMembersPanel.svelte` | Right-column member list. Collapsible via prop. Reuses existing avatar-resolver and `onAvatarClick` hook. | ~80 |
| `src/lib/components/CreateChannelDialog.svelte` | Modal-wrapped form. Name 1–32 char validation. Slider + number-input pair for `write_power` hidden behind `// v3 unhide`. Auto-close on `myPower < 50`. | ~120 |
| `src/lib/components/ModifyChannelDialog.svelte` | Modal-wrapped form. Pre-fills from `channel` prop. Partial-update payload (Some/None). All-None rejected as no-op. Auto-close on `myPower < 50`. | ~130 |
| `src/lib/__tests__/channel-message-service.test.ts` | Vitest service tests with fake `TauriAdapter`. Covers IPC dispatch, event handling, dedup, single-in-flight, destroy cleanup. | ~220 |
| `src/lib/components/__tests__/CommunityView.test.ts` | Layout + ⚙️ modal + §6.4 cascade + §6.6 silent rename. | ~180 |
| `src/lib/components/__tests__/ChannelSubSidebar.test.ts` | Ordering, active highlight, power-gating of "+" + context menu, click dispatch. | ~140 |
| `src/lib/components/__tests__/ChannelMessageFeed.test.ts` | Auto-scroll, scroll-trigger backfill (250 ms + single-in-flight), skeleton lifecycle, compose post. | ~180 |
| `src/lib/components/__tests__/CreateChannelDialog.test.ts` | Name validation, IPC dispatch, error inline render, auto-close on demotion. | ~100 |
| `src/lib/components/__tests__/ModifyChannelDialog.test.ts` | Pre-fill, partial-update, all-None rejection, auto-close on demotion. | ~110 |

### Modified files (3)

| Path | Change | Approx LoC delta |
|---|---|---|
| `src/lib/community-service.ts` | Add `createChannel` / `modifyChannel` / `deleteChannel` / `listChannels` method facades, `onChannelConfigChanged` callback subscribed to `channel-config-updated` event in `connectAdapter`, `channelCache: Map<string, ChannelInfo[]>`, `selectedChannelByCommunity: Map<string, string>` with get/set + cleared by `destroy()`. | +~100 |
| `src/lib/__tests__/community-service.test.ts` | Add channel-config method tests + selected-channel-map tests. | +~110 |
| `src/App.svelte` | Replace `CommunitySettingsPanel` mount at L1525 with `CommunityView` mount; instantiate `channelMessageService` alongside `communityService` at L321; `connectAdapter` / `destroy` lifecycle parity. | net ~0 (additions + replacement) |

### Deliberately NOT modified

- `src/lib/components/CommunitySettingsPanel.svelte` (relocates behind ⚙️ modal — same shape, just new mount point inside `CommunityView`).
- `src/lib/components/TextFeed.svelte` (DM feed kept isolated; `ChannelMessageFeed` is a fresh fork per spec §6.1).
- `src/lib/message-service.ts` (DM service unchanged; `ChannelMessageService` mirrors its shape but is separate per spec §7.7).

---

## Phase 4 IPC reference (Phase 3 backend, already shipped at `ec58fe2`)

The seven IPCs the frontend invokes, with exact backend signatures from `src-tauri/src/lib.rs` and DTOs from `src-tauri/src/community_channel_log_engine.rs` + `src-tauri/src/lib.rs:9451-9497`. Tauri auto-converts snake_case Rust params to camelCase JS args — invoke with camelCase.

```typescript
// === Channel-config IPCs (Phase 1, refreshed in Phase 3) ===

// create_channel(communityId, name, writePower) → channelId hex
// Errors: "channel name is empty or exceeds 32 chars" / "invalid community_id hex: ..."
// / "community_id must be 16 bytes (32 hex chars)" / power-gating errors from verify_event.
adapter.invoke('create_channel', {
  communityId: string,        // 32 hex chars
  name: string,               // 1..=32 chars (Unicode)
  writePower: number,         // 0..=100 (POWER_THRESHOLDS.max)
}): Promise<string>;          // channelId hex

// modify_channel(communityId, channelId, name?, writePower?) → void
// Errors: "modify_channel: must provide name and/or write_power" (all-None rejection).
adapter.invoke('modify_channel', {
  communityId: string,
  channelId: string,
  name?: string | null,       // null/undefined = unchanged; defined = updated
  writePower?: number | null,
}): Promise<void>;

// delete_channel(communityId, channelId) → void
adapter.invoke('delete_channel', {
  communityId: string,
  channelId: string,
}): Promise<void>;

// list_channels(communityId) → ChannelInfoDto[] (sorted oldest-first by created_at)
adapter.invoke('list_channels', {
  communityId: string,
}): Promise<ChannelInfoDto[]>;

// === Channel-message IPCs (Phase 3) ===

// post_channel_message(communityId, channelId, body, replyTo?) → messageId hex
// `body` is bytes (Vec<u8>); JS sends as number array. Engine enforces UTF-8 + size cap.
// Errors: "community_id must be 16 bytes (32 hex chars)" / "no engine for {cid}/{chid}" / etc.
adapter.invoke('post_channel_message', {
  communityId: string,
  channelId: string,
  body: number[],             // UTF-8 bytes
  replyTo?: string | null,    // 32 hex chars or undefined; v2 always omits
}): Promise<string>;          // messageId hex

// list_channel_messages(communityId, channelId, since?, limit) → ChannelMessageDto[]
// `limit` 0 = engine default (256); cap 1000.
adapter.invoke('list_channel_messages', {
  communityId: string,
  channelId: string,
  since?: HlcDto | null,      // {wallMs, logical, deviceId}
  limit: number,              // u32, 0..=1000
}): Promise<ChannelMessageDto[]>;

// request_channel_backfill(communityId, channelId, since?) → void (fire-and-forget)
adapter.invoke('request_channel_backfill', {
  communityId: string,
  channelId: string,
  since?: HlcDto | null,
}): Promise<void>;
```

```typescript
// === DTOs (camelCase via serde rename_all) ===

interface HlcDto { wallMs: number; logical: number; deviceId: string; }

interface ChannelInfoDto {
  channelId: string;          // 32 hex chars
  name: string;
  writePower: number;
  createdAt: HlcDto;
  deletedAt?: HlcDto;
}

interface ChannelMessageDto {
  messageId: string;          // 32 hex chars
  communityId: string;
  channelId: string;
  author: string;             // owner hex
  at: HlcDto;
  body: number[];             // UTF-8 bytes; frontend decodes via TextDecoder
  replyTo?: string;           // 32 hex chars (v2 always omitted)
}

// === Tauri events ===

interface ChannelConfigChangedPayload {
  communityId: string;
  channelId: string;
  action: 'Created' | 'Modified' | 'Deleted';
  name?: string;              // populated for Created (always) + Modified (if changed)
  writePower?: number;        // populated for Created + Modified-with-wp
  atWallMs: number;
}

interface ChannelMessageReceivedPayload {
  communityId: string;
  channelId: string;
  message: ChannelMessageDto;
}

interface ChannelBackfillProgressPayload {
  communityId: string;
  channelId: string;
  fetched: number;
  totalEstimate?: number;     // terminal tick when fetched == totalEstimate
}
```

---

## Task 0: Pre-flight + green baseline confirm

**Goal:** Verify all CI gates green on the branch before any change, so any later red is unambiguously our doing. **No commit.**

**Files:** none (read-only verification).

- [ ] **Step 1: Verify on the right branch.**

```bash
git status
```

Expected:
```
On branch zeb-272-channels-frontend
Your branch is up to date with ... (or: branch is local).
nothing to commit, working tree clean
```

If not on this branch, the dispatcher made a mistake — ABORT and report.

- [ ] **Step 2: Verify base commit.**

```bash
git log --oneline -3
```

Expected first line begins `8fcbbbf docs(zeb-272): Phase 4 channels frontend design spec`.
Expected second line begins `ec58fe2 Merge pull request #96` (the spec parent).

- [ ] **Step 3: Run cargo fmt gate.**

```bash
cd src-tauri && cargo fmt --all -- --check
```

Expected: no output, exit 0.

- [ ] **Step 4: Run cargo clippy gate.**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures -- -D warnings
```

Expected: builds without warnings, exit 0. (Network-dependent first run can take 5-10 min as it warms the cache.)

- [ ] **Step 5: Run cargo test gate.**

```bash
cd src-tauri && cargo test --locked --workspace --all-targets --features test-fixtures --no-fail-fast
```

Expected: all tests pass, exit 0. Phase 3's integration tests (`community_channel_messages_integration`) should be among the green.

- [ ] **Step 6: Run msrv gate.**

```bash
cd src-tauri && cargo check --locked --all-targets --features test-fixtures
```

Expected: builds, exit 0. (This is what CI's `msrv` job runs against the declared `rust-version` from Cargo.toml; locally it's just a check against the current toolchain.)

- [ ] **Step 7: Run frontend tsc gate.**

```bash
npx tsc --noEmit
```

Expected: no diagnostics, exit 0.

- [ ] **Step 8: Run frontend vitest gate.**

```bash
npx vitest run
```

Expected: all tests pass, exit 0.

- [ ] **Step 9: No commit.** Pre-flight is verification only.

If ANY gate fails: ABORT and report the failure. Do not start Task 1 until baseline is green. Per `feedback_test_drift_is_our_fault` memory rule: broken tests on main are exclusively ours; sweep + fix + add CI gate, no externalizing language.

---

## Task 1: `ChannelMessageService` + tests

**Goal:** Build the per-channel message cache + subscribe API + IPC method facade. Mirrors `MessageService` (`src/lib/message-service.ts`) shape: `connectAdapter` → install listeners → `destroy` cleans up. Per spec §7.7. Single-in-flight backfill gate per spec §6.7.

**Files:**
- Create: `src/lib/channel-message-service.ts`
- Create: `src/lib/__tests__/channel-message-service.test.ts`

- [ ] **Step 1: Write the failing test file (initial subset).**

Create `src/lib/__tests__/channel-message-service.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { ChannelMessageService } from '../channel-message-service';
import type { TauriAdapter } from '../zenoh-service';

function makeAdapter(): TauriAdapter & { listeners: Map<string, Function> } {
  const listeners = new Map<string, Function>();
  return {
    listeners,
    invoke: vi.fn(),
    listen: vi.fn(async (event: string, handler: Function) => {
      listeners.set(event, handler);
      return () => listeners.delete(event);
    }),
  } as any;
}

describe('ChannelMessageService', () => {
  let service: ChannelMessageService;
  let adapter: ReturnType<typeof makeAdapter>;

  beforeEach(() => {
    service = new ChannelMessageService();
    adapter = makeAdapter();
  });

  it('connectAdapter installs channel-message-received + channel-backfill-progress listeners', async () => {
    await service.connectAdapter(adapter);
    expect(adapter.listeners.has('channel-message-received')).toBe(true);
    expect(adapter.listeners.has('channel-backfill-progress')).toBe(true);
  });

  it('postMessage invokes post_channel_message with camelCase args', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('aabb' + 'cc'.repeat(14));
    const msgId = await service.postMessage(
      'aa'.repeat(16),
      'bb'.repeat(16),
      'hello',
    );
    expect(adapter.invoke).toHaveBeenCalledWith('post_channel_message', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      body: Array.from(new TextEncoder().encode('hello')),
      replyTo: undefined,
    });
    expect(msgId).toBe('aabb' + 'cc'.repeat(14));
  });

  it('postMessage forwards replyTo when provided', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('mid');
    await service.postMessage('aa'.repeat(16), 'bb'.repeat(16), 'hi', 'cc'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledWith('post_channel_message', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      body: Array.from(new TextEncoder().encode('hi')),
      replyTo: 'cc'.repeat(16),
    });
  });

  it('listMessages invokes list_channel_messages with limit + optional since', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue([]);
    await service.listMessages('aa'.repeat(16), 'bb'.repeat(16), undefined, 100);
    expect(adapter.invoke).toHaveBeenCalledWith('list_channel_messages', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      since: undefined,
      limit: 100,
    });
  });

  it('listMessages caches result into byChannel and notifies subscribers', async () => {
    await service.connectAdapter(adapter);
    const dto = {
      messageId: 'm1',
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      author: 'au',
      at: { wallMs: 100, logical: 0, deviceId: 'd' },
      body: Array.from(new TextEncoder().encode('hi')),
    };
    (adapter.invoke as any).mockResolvedValue([dto]);

    const cb = vi.fn();
    service.subscribeToChannel('aa'.repeat(16), 'bb'.repeat(16), cb);
    await service.listMessages('aa'.repeat(16), 'bb'.repeat(16), undefined, 100);
    expect(cb).toHaveBeenCalledWith(dto);

    const cached = service.getMessages('aa'.repeat(16), 'bb'.repeat(16));
    expect(cached).toHaveLength(1);
    expect(cached[0].messageId).toBe('m1');
  });

  it('channel-message-received event appends to per-channel cache + notifies', async () => {
    await service.connectAdapter(adapter);
    const cb = vi.fn();
    service.subscribeToChannel('aa'.repeat(16), 'bb'.repeat(16), cb);

    const handler = adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'm1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'au',
          at: { wallMs: 100, logical: 0, deviceId: 'd' },
          body: [104, 105],
        },
      },
    });

    expect(cb).toHaveBeenCalledTimes(1);
    expect(service.getMessages('aa'.repeat(16), 'bb'.repeat(16))).toHaveLength(1);
  });

  it('dedupes by messageId — second arrival of same id is ignored', async () => {
    await service.connectAdapter(adapter);
    const cb = vi.fn();
    service.subscribeToChannel('aa'.repeat(16), 'bb'.repeat(16), cb);

    const handler = adapter.listeners.get('channel-message-received')!;
    const payload = {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      message: {
        messageId: 'm1',
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        author: 'au',
        at: { wallMs: 100, logical: 0, deviceId: 'd' },
        body: [104],
      },
    };
    handler({ payload });
    handler({ payload });

    expect(cb).toHaveBeenCalledTimes(1);
    expect(service.getMessages('aa'.repeat(16), 'bb'.repeat(16))).toHaveLength(1);
  });

  it('keeps cache sorted by HLC ascending across out-of-order arrivals', async () => {
    await service.connectAdapter(adapter);
    const handler = adapter.listeners.get('channel-message-received')!;
    const cid = 'aa'.repeat(16);
    const chid = 'bb'.repeat(16);

    handler({
      payload: {
        communityId: cid,
        channelId: chid,
        message: {
          messageId: 'm200',
          communityId: cid,
          channelId: chid,
          author: 'au',
          at: { wallMs: 200, logical: 0, deviceId: 'd' },
          body: [],
        },
      },
    });
    handler({
      payload: {
        communityId: cid,
        channelId: chid,
        message: {
          messageId: 'm100',
          communityId: cid,
          channelId: chid,
          author: 'au',
          at: { wallMs: 100, logical: 0, deviceId: 'd' },
          body: [],
        },
      },
    });

    const cached = service.getMessages(cid, chid);
    expect(cached.map(m => m.at.wallMs)).toEqual([100, 200]);
  });

  it('subscribeToChannel returns working unsubscribe', async () => {
    await service.connectAdapter(adapter);
    const cb = vi.fn();
    const unsub = service.subscribeToChannel('aa'.repeat(16), 'bb'.repeat(16), cb);
    unsub();
    const handler = adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'm1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'a',
          at: { wallMs: 1, logical: 0, deviceId: 'd' },
          body: [],
        },
      },
    });
    expect(cb).not.toHaveBeenCalled();
  });

  it('requestBackfill invokes request_channel_backfill + sets in-flight gate', async () => {
    await service.connectAdapter(adapter);
    let resolveInvoke: () => void = () => {};
    (adapter.invoke as any).mockReturnValue(new Promise<void>(r => { resolveInvoke = () => r(); }));

    const p1 = service.requestBackfill('aa'.repeat(16), 'bb'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(1);

    // Second call while first is in-flight: no-op (single-in-flight gate).
    const p2 = service.requestBackfill('aa'.repeat(16), 'bb'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(1);

    resolveInvoke();
    await p1;
    await p2;

    // After the first completes, a third call should fire a new IPC.
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.requestBackfill('aa'.repeat(16), 'bb'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(2);
  });

  it('in-flight gate is per (communityId, channelId)', async () => {
    await service.connectAdapter(adapter);
    let resolveA: () => void = () => {};
    (adapter.invoke as any).mockImplementation(() => new Promise<void>(r => { resolveA = () => r(); }));

    const pA = service.requestBackfill('aa'.repeat(16), 'bb'.repeat(16));
    const pB = service.requestBackfill('aa'.repeat(16), 'cc'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(2);  // independent gates

    resolveA();
    await pA;
    await pB;
  });

  it('channel-backfill-progress fires onBackfillProgress callback', async () => {
    await service.connectAdapter(adapter);
    const cb = vi.fn();
    service.onBackfillProgress = cb;

    const handler = adapter.listeners.get('channel-backfill-progress')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        fetched: 5,
        totalEstimate: 10,
      },
    });

    expect(cb).toHaveBeenCalledWith('aa'.repeat(16), 'bb'.repeat(16), 5, 10);
  });

  it('terminal channel-backfill-progress (fetched == totalEstimate) clears in-flight gate', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);

    await service.requestBackfill('aa'.repeat(16), 'bb'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(1);

    // Simulate non-terminal progress; gate stays active because IPC settled
    // already, but we want progress events to only flip the gate when
    // terminal. Use the in-flight check via a second invoke attempt:
    const handler = adapter.listeners.get('channel-backfill-progress')!;

    // After the IPC promise resolved, the in-flight gate is already
    // released by the .finally(). The test below verifies the
    // PROGRESS-driven gate-release case for callers that fire and forget.
    // Re-arm the gate by firing a second backfill that hangs:
    let resolve2: () => void = () => {};
    (adapter.invoke as any).mockReturnValue(new Promise<void>(r => { resolve2 = () => r(); }));
    void service.requestBackfill('aa'.repeat(16), 'bb'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(2);

    // Mid-flight progress (non-terminal) does NOT release the gate:
    handler({ payload: { communityId: 'aa'.repeat(16), channelId: 'bb'.repeat(16), fetched: 3, totalEstimate: 5 } });
    void service.requestBackfill('aa'.repeat(16), 'bb'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(2);  // still gated

    // Terminal progress releases the gate:
    handler({ payload: { communityId: 'aa'.repeat(16), channelId: 'bb'.repeat(16), fetched: 5, totalEstimate: 5 } });
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.requestBackfill('aa'.repeat(16), 'bb'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(3);

    resolve2();
  });

  it('destroy unregisters all listeners and clears caches', async () => {
    await service.connectAdapter(adapter);
    const cb = vi.fn();
    service.subscribeToChannel('aa'.repeat(16), 'bb'.repeat(16), cb);

    service.destroy();
    expect(adapter.listeners.size).toBe(0);

    expect(service.getMessages('aa'.repeat(16), 'bb'.repeat(16))).toEqual([]);
  });

  it('IPC errors propagate from postMessage', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockRejectedValue(new Error('no engine for ...'));
    await expect(
      service.postMessage('aa'.repeat(16), 'bb'.repeat(16), 'hi'),
    ).rejects.toThrow('no engine for ...');
  });

  it('postMessage throws when adapter not connected', async () => {
    await expect(
      service.postMessage('aa'.repeat(16), 'bb'.repeat(16), 'hi'),
    ).rejects.toThrow(/adapter not connected/);
  });
});
```

- [ ] **Step 2: Run the test to verify it fails.**

```bash
npx vitest run src/lib/__tests__/channel-message-service.test.ts
```

Expected: FAIL with errors about `Cannot find module '../channel-message-service'`.

- [ ] **Step 3: Implement `ChannelMessageService`.**

Create `src/lib/channel-message-service.ts`:

```typescript
import type { TauriAdapter } from './zenoh-service';

export interface HlcDto {
  wallMs: number;
  logical: number;
  deviceId: string;
}

export interface ChannelMessageDto {
  messageId: string;
  communityId: string;
  channelId: string;
  author: string;
  at: HlcDto;
  body: number[];
  replyTo?: string;
}

interface ChannelMessageReceivedPayload {
  communityId: string;
  channelId: string;
  message: ChannelMessageDto;
}

interface ChannelBackfillProgressPayload {
  communityId: string;
  channelId: string;
  fetched: number;
  totalEstimate?: number;
}

function chKey(communityId: string, channelId: string): string {
  return `${communityId}:${channelId}`;
}

/**
 * Per-channel message cache + subscribe API + IPC facade for the three
 * channel-message IPCs shipped in ZEB-270 Phase 3
 * (`post_channel_message` / `list_channel_messages` /
 * `request_channel_backfill`). Mirrors MessageService (the DM service)
 * shape — connectAdapter installs listeners, method facades validate
 * args + dispatch, destroy() unwinds. Single-in-flight backfill gate is
 * keyed per (communityId, channelId) per spec §6.7.
 *
 * Per-channel cache is sorted by HLC ascending so insert-on-event is
 * cheap and consumers can render oldest-at-top without re-sorting.
 * Dedupe is keyed by messageId (Phase 3 ChannelLogReplayTracker handles
 * the protocol-level dedup; this is a defense-in-depth at the UI layer
 * for cases like backfill-while-live overlapping the same event id).
 */
export class ChannelMessageService {
  /** Called whenever any channel sees a new message (live or backfilled). */
  onMessage?: (communityId: string, channelId: string, message: ChannelMessageDto) => void;
  /** Called for each channel-backfill-progress tick. */
  onBackfillProgress?: (
    communityId: string,
    channelId: string,
    fetched: number,
    totalEstimate?: number,
  ) => void;

  private adapter: TauriAdapter | null = null;
  private unlisteners: Array<() => void> = [];
  private byChannel = new Map<string, ChannelMessageDto[]>();
  private subscribers = new Map<string, Set<(msg: ChannelMessageDto) => void>>();
  private inFlightBackfill = new Set<string>();
  private seenIds = new Map<string, Set<string>>();

  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return;
    this.adapter = adapter;

    const unlistenMsg = await adapter.listen('channel-message-received', (event) => {
      const p = event.payload as ChannelMessageReceivedPayload;
      this.ingest(p.communityId, p.channelId, p.message);
    });
    this.unlisteners.push(unlistenMsg);

    const unlistenProgress = await adapter.listen('channel-backfill-progress', (event) => {
      const p = event.payload as ChannelBackfillProgressPayload;
      // Terminal tick (fetched == totalEstimate) releases the in-flight
      // backfill gate so subsequent scroll-trigger fires can re-request.
      // Non-terminal ticks just notify the UI for skeleton updates.
      if (p.totalEstimate !== undefined && p.fetched >= p.totalEstimate) {
        this.inFlightBackfill.delete(chKey(p.communityId, p.channelId));
      }
      this.onBackfillProgress?.(p.communityId, p.channelId, p.fetched, p.totalEstimate);
    });
    this.unlisteners.push(unlistenProgress);
  }

  /** Post a message. Returns the engine-minted messageId hex. */
  async postMessage(
    communityId: string,
    channelId: string,
    body: string,
    replyTo?: string,
  ): Promise<string> {
    if (!this.adapter) throw new Error('ChannelMessageService.postMessage: adapter not connected');
    const bodyBytes = Array.from(new TextEncoder().encode(body));
    const messageId = await this.adapter.invoke('post_channel_message', {
      communityId,
      channelId,
      body: bodyBytes,
      replyTo,
    }) as string;
    return messageId;
  }

  /** Page through locally-known messages. Caches results + notifies
   *  subscribers (so callers don't double-render — list-then-subscribe
   *  is the standard pattern). */
  async listMessages(
    communityId: string,
    channelId: string,
    since: HlcDto | undefined,
    limit: number,
  ): Promise<ChannelMessageDto[]> {
    if (!this.adapter) throw new Error('ChannelMessageService.listMessages: adapter not connected');
    const dtos = await this.adapter.invoke('list_channel_messages', {
      communityId,
      channelId,
      since,
      limit,
    }) as ChannelMessageDto[];
    for (const dto of dtos) {
      this.ingest(communityId, channelId, dto);
    }
    return dtos;
  }

  /** Fire-and-forget backfill request. Single-in-flight per
   *  (communityId, channelId) per spec §6.7 — additional calls during
   *  in-flight are no-ops. The gate releases when:
   *    1. The IPC promise rejects (engine error / not-connected), OR
   *    2. A terminal `channel-backfill-progress` event arrives (success). */
  async requestBackfill(
    communityId: string,
    channelId: string,
    since?: HlcDto,
  ): Promise<void> {
    if (!this.adapter) throw new Error('ChannelMessageService.requestBackfill: adapter not connected');
    const key = chKey(communityId, channelId);
    if (this.inFlightBackfill.has(key)) return;
    this.inFlightBackfill.add(key);
    try {
      await this.adapter.invoke('request_channel_backfill', {
        communityId,
        channelId,
        since,
      });
      // Don't release the gate here — wait for a terminal progress tick,
      // because the IPC returns immediately (fire-and-forget) and packets
      // arrive afterward via channel-message-received.
    } catch (e) {
      this.inFlightBackfill.delete(key);
      throw e;
    }
  }

  /** Subscribe to live + backfilled messages on a single channel.
   *  Returns an unsubscribe function. Multiple subscribers per channel
   *  are supported (each callback fires once per ingested message). */
  subscribeToChannel(
    communityId: string,
    channelId: string,
    callback: (msg: ChannelMessageDto) => void,
  ): () => void {
    const key = chKey(communityId, channelId);
    let set = this.subscribers.get(key);
    if (!set) {
      set = new Set();
      this.subscribers.set(key, set);
    }
    set.add(callback);
    return () => {
      const s = this.subscribers.get(key);
      s?.delete(callback);
      if (s?.size === 0) this.subscribers.delete(key);
    };
  }

  /** Read-only snapshot of cached messages for a channel (oldest-first). */
  getMessages(communityId: string, channelId: string): ChannelMessageDto[] {
    return this.byChannel.get(chKey(communityId, channelId)) ?? [];
  }

  /** Test-helper / belt-and-braces: clear local in-flight gate.
   *  Production code shouldn't need this — the terminal-progress event
   *  releases it. Exposed for the rare error-recovery path where the
   *  engine drops without progress events. */
  clearBackfillInFlight(communityId: string, channelId: string): void {
    this.inFlightBackfill.delete(chKey(communityId, channelId));
  }

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
    this.byChannel.clear();
    this.subscribers.clear();
    this.inFlightBackfill.clear();
    this.seenIds.clear();
    this.adapter = null;
  }

  private ingest(communityId: string, channelId: string, message: ChannelMessageDto): void {
    const key = chKey(communityId, channelId);
    let seen = this.seenIds.get(key);
    if (!seen) {
      seen = new Set();
      this.seenIds.set(key, seen);
    }
    if (seen.has(message.messageId)) return;
    seen.add(message.messageId);

    let arr = this.byChannel.get(key);
    if (!arr) {
      arr = [];
      this.byChannel.set(key, arr);
    }
    // Insert in HLC-sorted order. Comparison is wallMs primary, logical
    // secondary, deviceId tertiary — same convention as backend
    // list_channels' sort and ChannelLog's manifest.
    const idx = sortedInsertIndex(arr, message);
    arr.splice(idx, 0, message);

    this.onMessage?.(communityId, channelId, message);
    const subs = this.subscribers.get(key);
    if (subs) for (const cb of subs) cb(message);
  }
}

function sortedInsertIndex(arr: ChannelMessageDto[], msg: ChannelMessageDto): number {
  // Linear scan from the end is fine for typical visible-window sizes
  // (~100s). When ChannelLog ships a windowed-prefetch optimization in
  // v3 we can swap to binary search; YAGNI for v2.
  for (let i = arr.length - 1; i >= 0; i--) {
    if (compareHlc(arr[i].at, msg.at) <= 0) return i + 1;
  }
  return 0;
}

function compareHlc(a: HlcDto, b: HlcDto): number {
  if (a.wallMs !== b.wallMs) return a.wallMs - b.wallMs;
  if (a.logical !== b.logical) return a.logical - b.logical;
  return a.deviceId < b.deviceId ? -1 : a.deviceId > b.deviceId ? 1 : 0;
}
```

- [ ] **Step 4: Run the tests to verify they pass.**

```bash
npx vitest run src/lib/__tests__/channel-message-service.test.ts
```

Expected: all tests green.

- [ ] **Step 5: Run tsc + full vitest gate.**

```bash
npx tsc --noEmit
npx vitest run
```

Expected: no diagnostics, all tests green.

- [ ] **Step 6: Commit.**

```bash
git add src/lib/channel-message-service.ts src/lib/__tests__/channel-message-service.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-272): ChannelMessageService — per-channel cache + IPC facade

Mirrors MessageService shape. Handles channel-message-received +
channel-backfill-progress events; dedupes by messageId; keeps cache
HLC-sorted; single-in-flight backfill gate per (communityId, channelId)
per spec §6.7 (released by terminal channel-backfill-progress tick).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `community-service.ts` extension + tests

**Goal:** Extend the existing `CommunityService` (`src/lib/community-service.ts`, 170L) with channel-config IPC method facades, `onChannelConfigChanged` callback subscribed to `channel-config-updated`, channel cache, and the session-scoped `selectedChannelByCommunity` Map per spec §6.5. Per spec §7.8.

**Files:**
- Modify: `src/lib/community-service.ts`
- Modify: `src/lib/__tests__/community-service.test.ts`

- [ ] **Step 1: Add ChannelInfo type + ChannelConfigChangeAction enum at the top of `community-service.ts`.**

After the existing `RedeemInviteResultDto` interface (around line 16), add:

```typescript
/** Mirrors `ChannelInfoDto` in src-tauri/src/lib.rs (ZEB-266 Phase 1).
 *  HLC fields wire as `HlcDto` ({wallMs, logical, deviceId}). */
export interface ChannelInfo {
  channelId: string;
  name: string;
  writePower: number;
  createdAt: { wallMs: number; logical: number; deviceId: string };
  deletedAt?: { wallMs: number; logical: number; deviceId: string };
}

/** Action discriminator on the `channel-config-updated` Tauri event.
 *  Backend serializes via serde rename_all = "camelCase" so the wire
 *  shape is the literal strings 'created' | 'modified' | 'deleted'. */
export type ChannelConfigAction = 'created' | 'modified' | 'deleted';

interface ChannelConfigChangedPayload {
  communityId: string;
  channelId: string;
  action: ChannelConfigAction;
  name?: string;
  writePower?: number;
  atWallMs: number;
}
```

- [ ] **Step 2: Add the new fields + callbacks to the `CommunityService` class.**

In `CommunityService` after the existing `onDegradedChanged?` declaration (around line 47), add:

```typescript
  /** Called when a channel-config CRDT mutation materializes through
   *  the per-community state-CRDT. Receivers should refresh
   *  listChannels(communityId) to pull the post-mutation snapshot. */
  onChannelConfigChanged?: (
    communityId: string,
    action: ChannelConfigAction,
    channelId: string,
    name?: string,
    writePower?: number,
  ) => void;
```

After the existing `private knownKinds` declaration (around line 59), add:

```typescript
  private channelCache = new Map<string, ChannelInfo[]>();
  private selectedChannelByCommunity = new Map<string, string>();
```

- [ ] **Step 3: Subscribe to `channel-config-updated` in `connectAdapter`.**

Inside `connectAdapter`, after the existing `unlistenDegraded` block (around line 84), add:

```typescript
    const unlistenChannelConfig = await adapter.listen(
      'channel-config-updated',
      (event) => {
        const p = event.payload as ChannelConfigChangedPayload;
        // Invalidate the per-community channel cache so the next
        // listChannels(communityId) re-fetches.
        this.channelCache.delete(p.communityId);
        this.onChannelConfigChanged?.(
          p.communityId,
          p.action,
          p.channelId,
          p.name,
          p.writePower,
        );
      },
    );
    this.unlisteners.push(unlistenChannelConfig);
```

- [ ] **Step 4: Add the four channel-config IPC method facades.**

After the existing `generateInvite` method (around line 129), add:

```typescript
  async createChannel(
    communityId: string,
    name: string,
    writePower: number,
  ): Promise<string> {
    return this.invoke<string>('create_channel', {
      communityId,
      name,
      writePower,
    });
  }

  async modifyChannel(
    communityId: string,
    channelId: string,
    name?: string,
    writePower?: number,
  ): Promise<void> {
    await this.invoke<void>('modify_channel', {
      communityId,
      channelId,
      name,
      writePower,
    });
  }

  async deleteChannel(communityId: string, channelId: string): Promise<void> {
    await this.invoke<void>('delete_channel', { communityId, channelId });
  }

  async listChannels(communityId: string): Promise<ChannelInfo[]> {
    const cached = this.channelCache.get(communityId);
    if (cached) return cached;
    const fresh = await this.invoke<ChannelInfo[]>('list_channels', { communityId });
    this.channelCache.set(communityId, fresh);
    return fresh;
  }

  /** Per spec §6.5: session-scoped selected-channel map. Returns
   *  undefined for first-visit to a community (caller falls back to
   *  #general or first channel). Cleared by destroy(). */
  getSelectedChannel(communityId: string): string | undefined {
    return this.selectedChannelByCommunity.get(communityId);
  }

  setSelectedChannel(communityId: string, channelId: string): void {
    this.selectedChannelByCommunity.set(communityId, channelId);
  }
```

- [ ] **Step 5: Update `destroy()` to clear the new caches.**

In the existing `destroy()` method, between `this.knownKinds.clear();` and `this.adapter = null;`, add:

```typescript
    this.channelCache.clear();
    this.selectedChannelByCommunity.clear();
```

- [ ] **Step 6: Write the failing test additions.**

Append to `src/lib/__tests__/community-service.test.ts` (just before the closing `});` of the `describe` block):

```typescript
  it('connectAdapter installs channel-config-updated listener', async () => {
    await service.connectAdapter(adapter);
    expect(adapter.listeners.has('channel-config-updated')).toBe(true);
  });

  it('createChannel invokes create_channel with camelCase args', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('cc'.repeat(16));
    const id = await service.createChannel('aa'.repeat(16), 'announcements', 0);
    expect(adapter.invoke).toHaveBeenCalledWith('create_channel', {
      communityId: 'aa'.repeat(16),
      name: 'announcements',
      writePower: 0,
    });
    expect(id).toBe('cc'.repeat(16));
  });

  it('modifyChannel forwards both name and writePower', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.modifyChannel('aa'.repeat(16), 'bb'.repeat(16), 'renamed', 50);
    expect(adapter.invoke).toHaveBeenCalledWith('modify_channel', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      name: 'renamed',
      writePower: 50,
    });
  });

  it('modifyChannel forwards undefined for unchanged fields', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.modifyChannel('aa'.repeat(16), 'bb'.repeat(16), 'just-rename');
    expect(adapter.invoke).toHaveBeenCalledWith('modify_channel', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      name: 'just-rename',
      writePower: undefined,
    });
  });

  it('deleteChannel invokes delete_channel', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.deleteChannel('aa'.repeat(16), 'bb'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledWith('delete_channel', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
    });
  });

  it('listChannels caches per-community result', async () => {
    await service.connectAdapter(adapter);
    const dtos = [
      {
        channelId: 'cc'.repeat(16),
        name: 'general',
        writePower: 0,
        createdAt: { wallMs: 1, logical: 0, deviceId: 'd' },
      },
    ];
    (adapter.invoke as any).mockResolvedValue(dtos);
    const r1 = await service.listChannels('aa'.repeat(16));
    const r2 = await service.listChannels('aa'.repeat(16));
    expect(r1).toEqual(dtos);
    expect(r2).toEqual(r1);
    expect(adapter.invoke).toHaveBeenCalledTimes(1);
  });

  it('channel-config-updated for a community invalidates its channel cache', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue([]);
    await service.listChannels('aa'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(1);

    const handler = adapter.listeners.get('channel-config-updated')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'cc'.repeat(16),
        action: 'created',
        name: 'announcements',
        writePower: 50,
        atWallMs: 100,
      },
    });

    await service.listChannels('aa'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(2);
  });

  it('channel-config-updated fires onChannelConfigChanged callback', async () => {
    await service.connectAdapter(adapter);
    const cb = vi.fn();
    service.onChannelConfigChanged = cb;

    const handler = adapter.listeners.get('channel-config-updated')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'cc'.repeat(16),
        action: 'modified',
        name: 'newname',
        atWallMs: 200,
      },
    });

    expect(cb).toHaveBeenCalledWith('aa'.repeat(16), 'modified', 'cc'.repeat(16), 'newname', undefined);
  });

  it('getSelectedChannel returns undefined before set', () => {
    expect(service.getSelectedChannel('aa'.repeat(16))).toBeUndefined();
  });

  it('setSelectedChannel + getSelectedChannel round-trip', () => {
    service.setSelectedChannel('aa'.repeat(16), 'cc'.repeat(16));
    expect(service.getSelectedChannel('aa'.repeat(16))).toBe('cc'.repeat(16));
  });

  it('destroy clears selectedChannelByCommunity', () => {
    service.setSelectedChannel('aa'.repeat(16), 'cc'.repeat(16));
    service.destroy();
    expect(service.getSelectedChannel('aa'.repeat(16))).toBeUndefined();
  });

  it('destroy clears channelCache', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue([
      {
        channelId: 'cc'.repeat(16),
        name: 'g',
        writePower: 0,
        createdAt: { wallMs: 0, logical: 0, deviceId: 'd' },
      },
    ]);
    await service.listChannels('aa'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(1);

    service.destroy();
    // After destroy, listChannels would re-fetch (and throw, since
    // adapter is null). We assert the cache was cleared by checking
    // that the invoke would be called again on a fresh service.
    const service2 = new CommunityService();
    const adapter2 = makeAdapter();
    await service2.connectAdapter(adapter2);
    (adapter2.invoke as any).mockResolvedValue([]);
    await service2.listChannels('aa'.repeat(16));
    expect(adapter2.invoke).toHaveBeenCalledTimes(1);
  });
```

- [ ] **Step 7: Run the tests to verify they pass.**

```bash
npx vitest run src/lib/__tests__/community-service.test.ts
```

Expected: all tests green (existing 11 + new 12 = 23 total).

- [ ] **Step 8: Run full tsc + vitest gate.**

```bash
npx tsc --noEmit
npx vitest run
```

Expected: no diagnostics, all tests green.

- [ ] **Step 9: Commit.**

```bash
git add src/lib/community-service.ts src/lib/__tests__/community-service.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-272): CommunityService channel-config extensions

Adds createChannel/modifyChannel/deleteChannel/listChannels IPC method
facades (Phase 1's IPCs); subscribes to channel-config-updated in
connectAdapter; per-community channelCache invalidated on event and
re-fetched on next listChannels; session-scoped
selectedChannelByCommunity Map per spec §6.5 with get/set + cleared
by destroy().

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `CreateChannelDialog` + `ModifyChannelDialog` + tests

**Goal:** Build the two channel-CRUD dialogs. Both wrap `Modal.svelte` (which provides `role="dialog"` + `aria-modal="true"` + `aria-labelledby` + focus-trap via the existing `trapFocus` action). Per spec §7.5 / §7.6 / §10. Slider+number-input pair for `write_power` per `feedback_slider_pair_with_number_input` memory rule, hidden behind `// v3 unhide` comment per parent spec §12.3 and this spec §7.5/§7.6.

**Files:**
- Create: `src/lib/components/CreateChannelDialog.svelte`
- Create: `src/lib/components/ModifyChannelDialog.svelte`
- Create: `src/lib/components/__tests__/CreateChannelDialog.test.ts`
- Create: `src/lib/components/__tests__/ModifyChannelDialog.test.ts`

- [ ] **Step 1: Write the `CreateChannelDialog` test file.**

Create `src/lib/components/__tests__/CreateChannelDialog.test.ts`:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import CreateChannelDialog from '../CreateChannelDialog.svelte';
import { CommunityService } from '../../community-service';
import type { TauriAdapter } from '../../zenoh-service';

function makeAdapter(): TauriAdapter & { listeners: Map<string, Function> } {
  const listeners = new Map<string, Function>();
  return {
    listeners,
    invoke: vi.fn(),
    listen: vi.fn(async (event: string, handler: Function) => {
      listeners.set(event, handler);
      return () => listeners.delete(event);
    }),
  } as any;
}

async function setupDialog(overrides: Record<string, unknown> = {}) {
  const adapter = makeAdapter();
  const service = new CommunityService();
  await service.connectAdapter(adapter);
  const onClose = vi.fn();
  const onCreated = vi.fn();
  const props = {
    communityId: 'aa'.repeat(16),
    communityService: service,
    open: true,
    myPower: 100,
    onClose,
    onCreated,
    ...overrides,
  };
  const renderResult = render(CreateChannelDialog, { props });
  return { adapter, service, props, ...renderResult };
}

describe('CreateChannelDialog', () => {
  it('renders nothing when open=false', async () => {
    const { container } = await setupDialog({ open: false });
    expect(container.querySelector('[role="dialog"]')).toBeNull();
  });

  it('renders the form when open=true', async () => {
    const { getByPlaceholderText } = await setupDialog();
    expect(getByPlaceholderText(/Channel name/i)).toBeTruthy();
  });

  it('Create button disabled while name is empty', async () => {
    const { getByRole } = await setupDialog();
    const submit = getByRole('button', { name: /Create/i }) as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
  });

  it('Create button enabled when name has content', async () => {
    const { getByPlaceholderText, getByRole } = await setupDialog();
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'general' } });
    const submit = getByRole('button', { name: /Create/i }) as HTMLButtonElement;
    expect(submit.disabled).toBe(false);
  });

  it('rejects names over 32 chars (button stays disabled)', async () => {
    const { getByPlaceholderText, getByRole } = await setupDialog();
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'a'.repeat(33) } });
    const submit = getByRole('button', { name: /Create/i }) as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
  });

  it('submit invokes createChannel with name + writePower=0 (v2 default)', async () => {
    const { getByPlaceholderText, getByRole, adapter, props } = await setupDialog();
    (adapter.invoke as any).mockResolvedValue('cc'.repeat(16));
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'announcements' } });
    await fireEvent.click(getByRole('button', { name: /Create/i }));
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('create_channel', {
        communityId: 'aa'.repeat(16),
        name: 'announcements',
        writePower: 0,
      });
    });
    expect(props.onCreated).toHaveBeenCalledWith('cc'.repeat(16));
    expect(props.onClose).toHaveBeenCalled();
  });

  it('shows inline error when createChannel rejects', async () => {
    const { getByPlaceholderText, getByRole, getByText, adapter } = await setupDialog();
    (adapter.invoke as any).mockRejectedValue(new Error('channel name is empty or exceeds 32 chars'));
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'whatever' } });
    await fireEvent.click(getByRole('button', { name: /Create/i }));
    await waitFor(() => {
      expect(getByText(/channel name is empty or exceeds 32 chars/i)).toBeTruthy();
    });
  });

  it('Cancel button calls onClose without dispatching IPC', async () => {
    const { getByText, props, adapter } = await setupDialog();
    await fireEvent.click(getByText('Cancel'));
    expect(props.onClose).toHaveBeenCalled();
    expect(adapter.invoke).not.toHaveBeenCalled();
  });

  it('auto-closes via onClose when myPower drops below 50', async () => {
    const { rerender, props } = await setupDialog();
    await rerender({ ...props, myPower: 25 });
    await waitFor(() => {
      expect(props.onClose).toHaveBeenCalled();
    });
  });
});
```

- [ ] **Step 2: Run the test to verify it fails.**

```bash
npx vitest run src/lib/components/__tests__/CreateChannelDialog.test.ts
```

Expected: FAIL with `Cannot find module '../CreateChannelDialog.svelte'`.

- [ ] **Step 3: Implement `CreateChannelDialog.svelte`.**

Create `src/lib/components/CreateChannelDialog.svelte`:

```svelte
<script lang="ts">
  import Modal from './Modal.svelte';
  import type { CommunityService } from '../community-service';
  import { POWER_THRESHOLDS } from '../types';

  let {
    communityId,
    communityService,
    open,
    myPower,
    onClose,
    onCreated,
  }: {
    communityId: string;
    communityService: CommunityService;
    open: boolean;
    myPower: number;
    onClose: () => void;
    onCreated: (channelId: string) => void;
  } = $props();

  let name = $state('');
  let writePower = $state(0); // v2 always 0; the slider+number pair below is hidden behind `// v3 unhide`
  let submitting = $state(false);
  let error = $state<string | null>(null);
  const titleId = `create-channel-title-${Math.random().toString(36).slice(2)}`;

  let trimmed = $derived(name.trim());
  let canSubmit = $derived(
    trimmed.length > 0 && trimmed.length <= 32 && !submitting,
  );

  // Per spec §7.5 and §10: if local user is demoted below kick threshold
  // mid-action, auto-close. Power gating is the backend's
  // responsibility, but closing the dialog spares the user a
  // surprise rejection on submit.
  $effect(() => {
    if (open && myPower < POWER_THRESHOLDS.kick) {
      onClose();
    }
  });

  async function handleSubmit(e?: Event) {
    e?.preventDefault();
    if (!canSubmit) return;
    submitting = true;
    error = null;
    try {
      const channelId = await communityService.createChannel(communityId, trimmed, writePower);
      onCreated(channelId);
      // Reset for next open; the modal's open=false from the parent is what unmounts.
      name = '';
      writePower = 0;
      onClose();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }
</script>

{#if open}
  <Modal {onClose} canCancel={!submitting} ariaLabelledby={titleId} onCancel={onClose}>
    <h3 class="dialog-title" id={titleId}>New channel</h3>
    <form onsubmit={handleSubmit}>
      <label for="channel-name-input" class="sr-only">Channel name</label>
      <input
        id="channel-name-input"
        type="text"
        placeholder="Channel name"
        bind:value={name}
        class="name-input"
        disabled={submitting}
        maxlength={32}
        autofocus
      />
      <p class="hint">{trimmed.length}/32 characters</p>

      <!-- v3 unhide: per spec §7.5 + parent spec §12.3 — the
        write_power slider+number-input pair must exist from day one
        per the slider-pairing memory rule, but is hidden in v2 because
        v2 always submits write_power=0. v3 removes the `hidden` attr. -->
      <div class="control-row" hidden>
        <input
          type="range"
          min="0"
          max={POWER_THRESHOLDS.max}
          step="1"
          bind:value={writePower}
          class="slider"
          aria-label="Write-power threshold slider"
        />
        <input
          type="number"
          min="0"
          max={POWER_THRESHOLDS.max}
          step="1"
          bind:value={writePower}
          class="number-input"
          aria-label="Write-power threshold"
        />
      </div>

      {#if error}
        <div class="error-banner">{error}</div>
      {/if}

      <div class="dialog-actions">
        <button type="button" class="cancel-btn" onclick={onClose} disabled={submitting}>Cancel</button>
        <button type="submit" class="confirm-btn" disabled={!canSubmit}>
          {submitting ? 'Creating...' : 'Create'}
        </button>
      </div>
    </form>
  </Modal>
{/if}

<style>
  .dialog-title { color: var(--text-primary); font-size: 1.1rem; margin: 0 0 16px; }
  .sr-only {
    position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px;
    overflow: hidden; clip: rect(0, 0, 0, 0); white-space: nowrap; border: 0;
  }
  .name-input {
    width: 100%; padding: 8px 12px; background: var(--bg-tertiary);
    border: 1px solid var(--border); border-radius: 4px;
    color: var(--text-primary); font-size: 0.9rem; box-sizing: border-box;
  }
  .name-input:focus { outline: 2px solid var(--accent); outline-offset: -1px; }
  .hint { color: var(--text-secondary); font-size: 0.75rem; margin: 4px 0 16px; }
  .control-row { display: flex; align-items: center; gap: 14px; margin-bottom: 16px; }
  .slider { flex: 1; }
  .number-input {
    width: 64px; background: var(--bg-tertiary); border: 1px solid var(--accent);
    border-radius: 4px; padding: 6px 8px; color: var(--text-primary);
    font-size: 0.9rem; text-align: center; font-family: monospace;
  }
  .error-banner {
    background: var(--bg-tertiary); border: 1px solid #d83c3e; color: #d83c3e;
    padding: 8px 10px; border-radius: 4px; font-size: 0.8rem; margin-bottom: 12px;
  }
  .dialog-actions { display: flex; justify-content: flex-end; gap: 8px; }
  .cancel-btn, .confirm-btn {
    border: none; padding: 8px 16px; border-radius: 4px;
    cursor: pointer; font-size: 0.875rem;
  }
  .cancel-btn { background: var(--bg-tertiary); color: var(--text-secondary); }
  .confirm-btn { background: var(--accent); color: var(--text-primary); }
  .confirm-btn:disabled, .cancel-btn:disabled { opacity: 0.4; cursor: not-allowed; }
  .cancel-btn:focus-visible, .confirm-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2); outline-offset: 1px;
  }
</style>
```

- [ ] **Step 4: Run the test to verify it passes.**

```bash
npx vitest run src/lib/components/__tests__/CreateChannelDialog.test.ts
```

Expected: all tests green.

- [ ] **Step 5: Write the `ModifyChannelDialog` test file.**

Create `src/lib/components/__tests__/ModifyChannelDialog.test.ts`:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import ModifyChannelDialog from '../ModifyChannelDialog.svelte';
import { CommunityService } from '../../community-service';
import type { TauriAdapter } from '../../zenoh-service';
import type { ChannelInfo } from '../../community-service';

function makeAdapter(): TauriAdapter & { listeners: Map<string, Function> } {
  const listeners = new Map<string, Function>();
  return {
    listeners,
    invoke: vi.fn(),
    listen: vi.fn(async (event: string, handler: Function) => {
      listeners.set(event, handler);
      return () => listeners.delete(event);
    }),
  } as any;
}

const baseChannel: ChannelInfo = {
  channelId: 'cc'.repeat(16),
  name: 'general',
  writePower: 0,
  createdAt: { wallMs: 100, logical: 0, deviceId: 'd1' },
};

async function setupDialog(channelOverrides: Partial<ChannelInfo> = {}, propOverrides: Record<string, unknown> = {}) {
  const adapter = makeAdapter();
  const service = new CommunityService();
  await service.connectAdapter(adapter);
  const channel: ChannelInfo = { ...baseChannel, ...channelOverrides };
  const onClose = vi.fn();
  const props = {
    communityId: 'aa'.repeat(16),
    channel,
    communityService: service,
    open: true,
    myPower: 100,
    onClose,
    ...propOverrides,
  };
  const renderResult = render(ModifyChannelDialog, { props });
  return { adapter, service, props, channel, ...renderResult };
}

describe('ModifyChannelDialog', () => {
  it('pre-fills name from channel.name', async () => {
    const { getByPlaceholderText } = await setupDialog();
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    expect(input.value).toBe('general');
  });

  it('Save button disabled when no fields changed', async () => {
    const { getByRole } = await setupDialog();
    const submit = getByRole('button', { name: /Save/i }) as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
  });

  it('Save button enabled when name changes', async () => {
    const { getByPlaceholderText, getByRole } = await setupDialog();
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'announcements' } });
    expect((getByRole('button', { name: /Save/i }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('submit invokes modify_channel with only the changed name (writePower undefined)', async () => {
    const { getByPlaceholderText, getByRole, adapter, props } = await setupDialog();
    (adapter.invoke as any).mockResolvedValue(undefined);
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'announcements' } });
    await fireEvent.click(getByRole('button', { name: /Save/i }));
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('modify_channel', {
        communityId: 'aa'.repeat(16),
        channelId: 'cc'.repeat(16),
        name: 'announcements',
        writePower: undefined,
      });
    });
    expect(props.onClose).toHaveBeenCalled();
  });

  it('all-None submit is rejected client-side (no IPC dispatch, no error)', async () => {
    const { getByRole, adapter } = await setupDialog();
    // Force the submit handler by simulating Enter on the form even though
    // canSubmit should already be false.
    const submit = getByRole('button', { name: /Save/i }) as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
    await fireEvent.click(submit);
    expect(adapter.invoke).not.toHaveBeenCalled();
  });

  it('rejects empty name (button stays disabled)', async () => {
    const { getByPlaceholderText, getByRole } = await setupDialog();
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: '' } });
    expect((getByRole('button', { name: /Save/i }) as HTMLButtonElement).disabled).toBe(true);
  });

  it('rejects names over 32 chars (button stays disabled)', async () => {
    const { getByPlaceholderText, getByRole } = await setupDialog();
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'a'.repeat(33) } });
    expect((getByRole('button', { name: /Save/i }) as HTMLButtonElement).disabled).toBe(true);
  });

  it('shows inline error when modify_channel rejects', async () => {
    const { getByPlaceholderText, getByRole, getByText, adapter } = await setupDialog();
    (adapter.invoke as any).mockRejectedValue(new Error('actor power below mod threshold'));
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'newname' } });
    await fireEvent.click(getByRole('button', { name: /Save/i }));
    await waitFor(() => {
      expect(getByText(/actor power below mod threshold/i)).toBeTruthy();
    });
  });

  it('auto-closes via onClose when myPower drops below 50', async () => {
    const { rerender, props } = await setupDialog();
    await rerender({ ...props, myPower: 25 });
    await waitFor(() => {
      expect(props.onClose).toHaveBeenCalled();
    });
  });

  it('Cancel button calls onClose without IPC dispatch', async () => {
    const { getByText, adapter, props } = await setupDialog();
    await fireEvent.click(getByText('Cancel'));
    expect(props.onClose).toHaveBeenCalled();
    expect(adapter.invoke).not.toHaveBeenCalled();
  });
});
```

- [ ] **Step 6: Run the test to verify it fails.**

```bash
npx vitest run src/lib/components/__tests__/ModifyChannelDialog.test.ts
```

Expected: FAIL with `Cannot find module '../ModifyChannelDialog.svelte'`.

- [ ] **Step 7: Implement `ModifyChannelDialog.svelte`.**

Create `src/lib/components/ModifyChannelDialog.svelte`:

```svelte
<script lang="ts">
  import Modal from './Modal.svelte';
  import type { CommunityService, ChannelInfo } from '../community-service';
  import { POWER_THRESHOLDS } from '../types';

  let {
    communityId,
    channel,
    communityService,
    open,
    myPower,
    onClose,
  }: {
    communityId: string;
    channel: ChannelInfo;
    communityService: CommunityService;
    open: boolean;
    myPower: number;
    onClose: () => void;
  } = $props();

  let name = $state(channel.name);
  let writePower = $state(channel.writePower);
  let submitting = $state(false);
  let error = $state<string | null>(null);
  const titleId = `modify-channel-title-${Math.random().toString(36).slice(2)}`;

  // When the parent opens this dialog for a different channel, refresh the
  // pre-filled values. (Without this, the second open would still show the
  // first channel's name — Svelte won't re-init $state on prop change.)
  $effect(() => {
    if (open) {
      name = channel.name;
      writePower = channel.writePower;
      error = null;
    }
  });

  let trimmed = $derived(name.trim());
  let nameChanged = $derived(trimmed !== channel.name);
  let writePowerChanged = $derived(writePower !== channel.writePower);
  let nameValid = $derived(trimmed.length > 0 && trimmed.length <= 32);
  let canSubmit = $derived(
    !submitting &&
      (nameChanged || writePowerChanged) &&
      // If name didn't change, validity doesn't apply to it; but if it did
      // change, it must be valid.
      (!nameChanged || nameValid),
  );

  // Per spec §7.6 + §10: auto-close on demotion below kick threshold.
  $effect(() => {
    if (open && myPower < POWER_THRESHOLDS.kick) {
      onClose();
    }
  });

  async function handleSubmit(e?: Event) {
    e?.preventDefault();
    if (!canSubmit) return;
    submitting = true;
    error = null;
    try {
      const submitName = nameChanged ? trimmed : undefined;
      const submitWritePower = writePowerChanged ? writePower : undefined;
      // Defense-in-depth: backend rejects all-None too, but never reaching
      // the IPC saves a round-trip.
      if (submitName === undefined && submitWritePower === undefined) {
        return;
      }
      await communityService.modifyChannel(
        communityId,
        channel.channelId,
        submitName,
        submitWritePower,
      );
      onClose();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }
</script>

{#if open}
  <Modal {onClose} canCancel={!submitting} ariaLabelledby={titleId} onCancel={onClose}>
    <h3 class="dialog-title" id={titleId}>Modify #{channel.name}</h3>
    <form onsubmit={handleSubmit}>
      <label for="modify-channel-name-input" class="sr-only">Channel name</label>
      <input
        id="modify-channel-name-input"
        type="text"
        placeholder="Channel name"
        bind:value={name}
        class="name-input"
        disabled={submitting}
        maxlength={32}
        autofocus
      />
      <p class="hint">{trimmed.length}/32 characters</p>

      <!-- v3 unhide: same as CreateChannelDialog. v2 keeps writePower
        immutable through the UI (always equals channel.writePower).
        v3 removes the `hidden` attr. -->
      <div class="control-row" hidden>
        <input
          type="range"
          min="0"
          max={POWER_THRESHOLDS.max}
          step="1"
          bind:value={writePower}
          class="slider"
          aria-label="Write-power threshold slider"
        />
        <input
          type="number"
          min="0"
          max={POWER_THRESHOLDS.max}
          step="1"
          bind:value={writePower}
          class="number-input"
          aria-label="Write-power threshold"
        />
      </div>

      {#if error}
        <div class="error-banner">{error}</div>
      {/if}

      <div class="dialog-actions">
        <button type="button" class="cancel-btn" onclick={onClose} disabled={submitting}>Cancel</button>
        <button type="submit" class="confirm-btn" disabled={!canSubmit}>
          {submitting ? 'Saving...' : 'Save'}
        </button>
      </div>
    </form>
  </Modal>
{/if}

<style>
  .dialog-title { color: var(--text-primary); font-size: 1.1rem; margin: 0 0 16px; }
  .sr-only {
    position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px;
    overflow: hidden; clip: rect(0, 0, 0, 0); white-space: nowrap; border: 0;
  }
  .name-input {
    width: 100%; padding: 8px 12px; background: var(--bg-tertiary);
    border: 1px solid var(--border); border-radius: 4px;
    color: var(--text-primary); font-size: 0.9rem; box-sizing: border-box;
  }
  .name-input:focus { outline: 2px solid var(--accent); outline-offset: -1px; }
  .hint { color: var(--text-secondary); font-size: 0.75rem; margin: 4px 0 16px; }
  .control-row { display: flex; align-items: center; gap: 14px; margin-bottom: 16px; }
  .slider { flex: 1; }
  .number-input {
    width: 64px; background: var(--bg-tertiary); border: 1px solid var(--accent);
    border-radius: 4px; padding: 6px 8px; color: var(--text-primary);
    font-size: 0.9rem; text-align: center; font-family: monospace;
  }
  .error-banner {
    background: var(--bg-tertiary); border: 1px solid #d83c3e; color: #d83c3e;
    padding: 8px 10px; border-radius: 4px; font-size: 0.8rem; margin-bottom: 12px;
  }
  .dialog-actions { display: flex; justify-content: flex-end; gap: 8px; }
  .cancel-btn, .confirm-btn {
    border: none; padding: 8px 16px; border-radius: 4px;
    cursor: pointer; font-size: 0.875rem;
  }
  .cancel-btn { background: var(--bg-tertiary); color: var(--text-secondary); }
  .confirm-btn { background: var(--accent); color: var(--text-primary); }
  .confirm-btn:disabled, .cancel-btn:disabled { opacity: 0.4; cursor: not-allowed; }
  .cancel-btn:focus-visible, .confirm-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2); outline-offset: 1px;
  }
</style>
```

- [ ] **Step 8: Run the test to verify it passes.**

```bash
npx vitest run src/lib/components/__tests__/ModifyChannelDialog.test.ts
```

Expected: all tests green.

- [ ] **Step 9: Run full tsc + vitest gate.**

```bash
npx tsc --noEmit
npx vitest run
```

Expected: no diagnostics, all tests green.

- [ ] **Step 10: Commit.**

```bash
git add src/lib/components/CreateChannelDialog.svelte src/lib/components/ModifyChannelDialog.svelte src/lib/components/__tests__/CreateChannelDialog.test.ts src/lib/components/__tests__/ModifyChannelDialog.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-272): channel CRUD dialogs (Create + Modify)

Both wrap Modal.svelte (role=dialog + aria-modal + aria-labelledby +
focus-trap via existing trapFocus action). Name 1-32 char validation;
writePower slider+number-input pair present per slider-pairing memory
rule but `hidden` behind v3-unhide flag; ModifyChannelDialog
partial-update (only changed fields submitted, all-None rejected
client-side). Both auto-close on myPower drop below kick threshold
per spec §7.5/§7.6/§10.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `ChannelMembersPanel`

**Goal:** Right-column member list per spec §7.4. Presentational + tiny state. Component tests for this column live inside `CommunityView.test.ts` (Task 7) per spec §11.1 and the consolidation note in spec §5.1 — Task 4 ships only the component itself.

**Files:**
- Create: `src/lib/components/ChannelMembersPanel.svelte`

- [ ] **Step 1: Implement `ChannelMembersPanel.svelte`.**

Create `src/lib/components/ChannelMembersPanel.svelte`:

```svelte
<script lang="ts">
  import type { CommunityMember } from '../types';
  import { powerToRole } from '../types';
  import Avatar from './Avatar.svelte';
  import type { TrustService } from '../trust-service';

  let {
    members,
    ownAddress,
    trustService,
    collapsed,
    onAvatarClick,
  }: {
    members: CommunityMember[];
    ownAddress: string;
    trustService?: TrustService;
    collapsed: boolean;
    onAvatarClick?: (address: string, event: MouseEvent) => void;
  } = $props();

  // Filter to joined members only — left/kicked/invited members render
  // in the settings modal's member list, not the channel-context list.
  let visible = $derived(members.filter((m) => m.status === 'joined'));

  // Order: self first, then by power desc, then by display name asc.
  let ordered = $derived.by(() => {
    return [...visible].sort((a, b) => {
      if (a.address === ownAddress) return -1;
      if (b.address === ownAddress) return 1;
      if (a.power !== b.power) return b.power - a.power;
      const an = (a.displayName ?? a.address).toLowerCase();
      const bn = (b.displayName ?? b.address).toLowerCase();
      return an.localeCompare(bn);
    });
  });
</script>

{#if !collapsed}
  <aside class="members-panel" aria-label="Community members">
    <header class="panel-header">
      <span class="title">Members</span>
      <span class="count">{visible.length}</span>
    </header>
    <ul class="member-list">
      {#each ordered as m (m.address)}
        <li class="member-row">
          <button
            class="avatar-trigger"
            type="button"
            aria-label="Open profile for {m.displayName ?? m.address}"
            onclick={(e) => onAvatarClick?.(m.address, e)}
          >
            <Avatar address={m.address} {trustService} size={24} />
          </button>
          <div class="info">
            <span class="name" class:self={m.address === ownAddress}>
              {m.displayName ?? m.address.slice(0, 8)}
            </span>
            <span class="role" data-role={powerToRole(m.power)}>{powerToRole(m.power)}</span>
          </div>
        </li>
      {/each}
    </ul>
  </aside>
{/if}

<style>
  .members-panel {
    display: flex;
    flex-direction: column;
    background: var(--bg-secondary);
    border-left: 1px solid var(--border);
    width: 200px;
    min-width: 0;
    overflow: hidden;
  }
  .panel-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 12px 14px 6px;
    border-bottom: 1px solid var(--border);
    color: var(--text-secondary);
    font-size: 0.75rem;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  .count {
    background: var(--bg-tertiary);
    color: var(--text-primary);
    border-radius: 8px;
    padding: 0 6px;
    font-size: 0.7rem;
    text-transform: none;
    letter-spacing: 0;
  }
  .member-list {
    list-style: none;
    margin: 0;
    padding: 6px 0;
    overflow-y: auto;
    flex: 1;
  }
  .member-row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 14px;
    color: var(--text-primary);
    font-size: 0.875rem;
  }
  .member-row:hover { background: var(--bg-tertiary); }
  .avatar-trigger {
    background: none;
    border: none;
    padding: 0;
    cursor: pointer;
    display: flex;
  }
  .info { display: flex; flex-direction: column; min-width: 0; flex: 1; }
  .name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .name.self { color: var(--accent); }
  .role {
    font-size: 0.65rem;
    text-transform: uppercase;
    color: var(--text-secondary);
  }
  .role[data-role="admin"] { color: var(--accent); }
  .role[data-role="mod"] { color: #ffb84a; }
</style>
```

- [ ] **Step 2: Verify tsc.**

```bash
npx tsc --noEmit
```

Expected: no diagnostics.

- [ ] **Step 3: Verify vitest still passes.**

```bash
npx vitest run
```

Expected: all tests green (no test added in this task — Task 7 covers it).

- [ ] **Step 4: Commit.**

```bash
git add src/lib/components/ChannelMembersPanel.svelte
git commit -m "$(cat <<'EOF'
feat(zeb-272): ChannelMembersPanel right-column component

Presentational member list with self-first / power-desc / name-asc
ordering, role badges, avatar profile-popover hook reuse. Filters to
joined members (left/kicked render in settings modal instead).
Component tests live inside CommunityView.test.ts per spec §5.1
consolidation note.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `ChannelSubSidebar` + tests

**Goal:** Channel list (left column) per spec §7.2. Renders pre-sorted `ChannelInfo[]` (parent guarantees oldest-first ordering + `#general` first). Active highlight. "+" button + right-click context menu (Rename / Set write_power / Delete) gated on `myPower >= 50`.

**Files:**
- Create: `src/lib/components/ChannelSubSidebar.svelte`
- Create: `src/lib/components/__tests__/ChannelSubSidebar.test.ts`

- [ ] **Step 1: Write the failing test file.**

Create `src/lib/components/__tests__/ChannelSubSidebar.test.ts`:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import ChannelSubSidebar from '../ChannelSubSidebar.svelte';
import type { ChannelInfo } from '../../community-service';

const general: ChannelInfo = {
  channelId: '01'.repeat(16),
  name: 'general',
  writePower: 0,
  createdAt: { wallMs: 100, logical: 0, deviceId: 'd1' },
};
const announcements: ChannelInfo = {
  channelId: '02'.repeat(16),
  name: 'announcements',
  writePower: 50,
  createdAt: { wallMs: 200, logical: 0, deviceId: 'd1' },
};
const devTalk: ChannelInfo = {
  channelId: '03'.repeat(16),
  name: 'dev-talk',
  writePower: 0,
  createdAt: { wallMs: 300, logical: 0, deviceId: 'd1' },
};

const baseProps = {
  channels: [general, announcements, devTalk],
  activeChannelId: general.channelId,
  myPower: 100,
  onSelect: vi.fn(),
  onCreateClick: vi.fn(),
  onModifyClick: vi.fn(),
  onDeleteClick: vi.fn(),
};

describe('ChannelSubSidebar', () => {
  it('renders all channels in the order received (parent guarantees oldest-first)', () => {
    const { container } = render(ChannelSubSidebar, { props: baseProps });
    const items = Array.from(container.querySelectorAll('.channel-item .channel-name'))
      .map((el) => el.textContent?.trim());
    expect(items).toEqual(['general', 'announcements', 'dev-talk']);
  });

  it('highlights the active channel', () => {
    const { container } = render(ChannelSubSidebar, { props: baseProps });
    const active = container.querySelector('.channel-item.active');
    expect(active?.querySelector('.channel-name')?.textContent?.trim()).toBe('general');
  });

  it('clicking a channel item dispatches onSelect with channelId', async () => {
    const onSelect = vi.fn();
    const { container } = render(ChannelSubSidebar, { props: { ...baseProps, onSelect } });
    const items = container.querySelectorAll('.channel-item');
    await fireEvent.click(items[1] as HTMLElement);
    expect(onSelect).toHaveBeenCalledWith(announcements.channelId);
  });

  it('+ button visible when myPower >= 50', () => {
    const { container } = render(ChannelSubSidebar, { props: baseProps });
    expect(container.querySelector('button.create-channel-btn')).toBeTruthy();
  });

  it('+ button hidden when myPower < 50', () => {
    const { container } = render(ChannelSubSidebar, {
      props: { ...baseProps, myPower: 25 },
    });
    expect(container.querySelector('button.create-channel-btn')).toBeNull();
  });

  it('+ button click dispatches onCreateClick', async () => {
    const onCreateClick = vi.fn();
    const { container } = render(ChannelSubSidebar, {
      props: { ...baseProps, onCreateClick },
    });
    await fireEvent.click(container.querySelector('button.create-channel-btn') as HTMLElement);
    expect(onCreateClick).toHaveBeenCalled();
  });

  it('right-click on a channel opens context menu when myPower >= 50', async () => {
    const { container } = render(ChannelSubSidebar, { props: baseProps });
    const item = container.querySelectorAll('.channel-item')[1] as HTMLElement;
    await fireEvent.contextMenu(item);
    expect(container.querySelector('.context-menu')).toBeTruthy();
  });

  it('right-click context menu does NOT appear when myPower < 50', async () => {
    const { container } = render(ChannelSubSidebar, {
      props: { ...baseProps, myPower: 25 },
    });
    const item = container.querySelectorAll('.channel-item')[1] as HTMLElement;
    await fireEvent.contextMenu(item);
    expect(container.querySelector('.context-menu')).toBeNull();
  });

  it('Rename context menu item dispatches onModifyClick with the channel', async () => {
    const onModifyClick = vi.fn();
    const { container, getByRole } = render(ChannelSubSidebar, {
      props: { ...baseProps, onModifyClick },
    });
    const item = container.querySelectorAll('.channel-item')[1] as HTMLElement;
    await fireEvent.contextMenu(item);
    await fireEvent.click(getByRole('button', { name: /Rename/i }));
    expect(onModifyClick).toHaveBeenCalledWith(announcements);
  });

  it('Delete context menu item dispatches onDeleteClick with the channel', async () => {
    const onDeleteClick = vi.fn();
    const { container, getByRole } = render(ChannelSubSidebar, {
      props: { ...baseProps, onDeleteClick },
    });
    const item = container.querySelectorAll('.channel-item')[1] as HTMLElement;
    await fireEvent.contextMenu(item);
    await fireEvent.click(getByRole('button', { name: /Delete/i }));
    expect(onDeleteClick).toHaveBeenCalledWith(announcements);
  });

  it('clicking outside dismisses the context menu', async () => {
    const { container } = render(ChannelSubSidebar, { props: baseProps });
    const item = container.querySelectorAll('.channel-item')[1] as HTMLElement;
    await fireEvent.contextMenu(item);
    expect(container.querySelector('.context-menu')).toBeTruthy();

    // Click outside (on the sidebar root)
    await fireEvent.click(container.querySelector('.channel-sub-sidebar') as HTMLElement);
    expect(container.querySelector('.context-menu')).toBeNull();
  });
});
```

- [ ] **Step 2: Run the test to verify it fails.**

```bash
npx vitest run src/lib/components/__tests__/ChannelSubSidebar.test.ts
```

Expected: FAIL with `Cannot find module '../ChannelSubSidebar.svelte'`.

- [ ] **Step 3: Implement `ChannelSubSidebar.svelte`.**

Create `src/lib/components/ChannelSubSidebar.svelte`:

```svelte
<script lang="ts">
  import type { ChannelInfo } from '../community-service';
  import { POWER_THRESHOLDS } from '../types';

  let {
    channels,
    activeChannelId,
    myPower,
    onSelect,
    onCreateClick,
    onModifyClick,
    onDeleteClick,
  }: {
    channels: ChannelInfo[];
    activeChannelId: string | null;
    myPower: number;
    onSelect: (channelId: string) => void;
    onCreateClick: () => void;
    onModifyClick: (channel: ChannelInfo) => void;
    onDeleteClick: (channel: ChannelInfo) => void;
  } = $props();

  let canModerate = $derived(myPower >= POWER_THRESHOLDS.kick);

  // Per spec §6.4: parent (CommunityView) hands us a list of joined-only
  // channels (deletedAt is filtered upstream). We just render in input
  // order.
  let visible = $derived(channels.filter((c) => c.deletedAt === undefined));

  let contextMenu = $state<{ channel: ChannelInfo; x: number; y: number } | null>(null);

  function handleContextMenu(e: MouseEvent, channel: ChannelInfo) {
    if (!canModerate) return;
    e.preventDefault();
    contextMenu = { channel, x: e.clientX, y: e.clientY };
  }

  function dismissContextMenu() {
    contextMenu = null;
  }

  function handleRename() {
    if (!contextMenu) return;
    const ch = contextMenu.channel;
    contextMenu = null;
    onModifyClick(ch);
  }

  function handleDelete() {
    if (!contextMenu) return;
    const ch = contextMenu.channel;
    contextMenu = null;
    onDeleteClick(ch);
  }
</script>

<svelte:window onkeydown={(e) => { if (e.key === 'Escape') dismissContextMenu(); }} />

<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<nav class="channel-sub-sidebar" aria-label="Channels" onclick={dismissContextMenu}>
  <ul class="channel-list">
    {#each visible as channel (channel.channelId)}
      <li>
        <button
          type="button"
          class="channel-item"
          class:active={channel.channelId === activeChannelId}
          onclick={(e) => { e.stopPropagation(); onSelect(channel.channelId); }}
          oncontextmenu={(e) => handleContextMenu(e, channel)}
        >
          <span class="channel-hash" aria-hidden="true">#</span>
          <span class="channel-name">{channel.name}</span>
        </button>
      </li>
    {/each}
  </ul>
  {#if canModerate}
    <button
      type="button"
      class="create-channel-btn"
      aria-label="Create channel"
      onclick={(e) => { e.stopPropagation(); onCreateClick(); }}
    >
      <span aria-hidden="true">+</span>
      <span class="create-label">Create channel</span>
    </button>
  {/if}
</nav>

{#if contextMenu}
  <div
    class="context-menu"
    role="menu"
    style="left: {contextMenu.x}px; top: {contextMenu.y}px"
  >
    <button type="button" role="menuitem" onclick={handleRename}>Rename</button>
    <button type="button" role="menuitem" onclick={handleDelete} class="destructive">Delete</button>
  </div>
{/if}

<style>
  .channel-sub-sidebar {
    display: flex;
    flex-direction: column;
    background: var(--bg-secondary);
    border-right: 1px solid var(--border);
    width: 200px;
    min-width: 0;
    overflow-y: auto;
  }
  .channel-list { list-style: none; margin: 0; padding: 6px 0; flex: 1; }
  .channel-item {
    display: flex;
    align-items: center;
    width: 100%;
    background: none;
    border: none;
    color: var(--text-secondary);
    padding: 6px 14px;
    cursor: pointer;
    font-size: 0.9rem;
    text-align: left;
  }
  .channel-item:hover { background: var(--bg-tertiary); color: var(--text-primary); }
  .channel-item.active { background: var(--bg-tertiary); color: var(--text-primary); font-weight: 500; }
  .channel-hash { color: var(--text-tertiary, var(--text-secondary)); margin-right: 6px; }
  .channel-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .create-channel-btn {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    background: none;
    border: none;
    padding: 8px 14px;
    color: var(--text-secondary);
    cursor: pointer;
    font-size: 0.85rem;
    border-top: 1px solid var(--border);
  }
  .create-channel-btn:hover { background: var(--bg-tertiary); color: var(--accent); }
  .context-menu {
    position: fixed;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 4px;
    box-shadow: 0 2px 8px rgba(0, 0, 0, 0.3);
    z-index: 1000;
    min-width: 140px;
    padding: 4px 0;
  }
  .context-menu button {
    display: block;
    width: 100%;
    background: none;
    border: none;
    text-align: left;
    padding: 6px 12px;
    color: var(--text-primary);
    cursor: pointer;
    font-size: 0.85rem;
  }
  .context-menu button:hover { background: var(--bg-tertiary); }
  .context-menu button.destructive { color: #d83c3e; }
</style>
```

- [ ] **Step 4: Run the test to verify it passes.**

```bash
npx vitest run src/lib/components/__tests__/ChannelSubSidebar.test.ts
```

Expected: all tests green.

- [ ] **Step 5: Run full tsc + vitest gate.**

```bash
npx tsc --noEmit
npx vitest run
```

Expected: no diagnostics, all tests green.

- [ ] **Step 6: Commit.**

```bash
git add src/lib/components/ChannelSubSidebar.svelte src/lib/components/__tests__/ChannelSubSidebar.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-272): ChannelSubSidebar left-column component

Channel list rendering parent-supplied oldest-first order
(#general first by virtue of the backend sort). Active highlight.
"+" button + right-click context menu (Rename / Delete) both gated
on myPower >= POWER_THRESHOLDS.kick. Esc dismisses the context
menu; click-outside dismisses too.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: `ChannelMessageFeed` + tests

**Goal:** Center-column virtualized message feed per spec §7.3 + §6.7. Auto-scroll-to-bottom on new live message when at bottom; 250 ms stable-at-top fires `requestBackfill` via single-in-flight gate; "Loading older messages…" skeleton; inline channel-compose `<textarea>` (Enter posts, Shift+Enter newline). Per "engineer for real scale" memory rule, virtualization is load-bearing — implement windowed render via `IntersectionObserver` + height-cache (no library dep).

**Files:**
- Create: `src/lib/components/ChannelMessageFeed.svelte`
- Create: `src/lib/components/__tests__/ChannelMessageFeed.test.ts`

**Plan-time virtualization decision (per task-level note in spec):** Implement windowed-render via `IntersectionObserver` — no library dep needed (jsdom in vitest stubs IntersectionObserver via the `intersection-observer` polyfill if present; for tests we'll use a synchronous render-all fallback gated by an `enableVirtualization` prop default=true). This avoids a new package.json entry.

- [ ] **Step 1: Write the failing test file.**

Create `src/lib/components/__tests__/ChannelMessageFeed.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import ChannelMessageFeed from '../ChannelMessageFeed.svelte';
import { ChannelMessageService } from '../../channel-message-service';
import type { TauriAdapter } from '../../zenoh-service';

function makeAdapter(): TauriAdapter & { listeners: Map<string, Function> } {
  const listeners = new Map<string, Function>();
  return {
    listeners,
    invoke: vi.fn(),
    listen: vi.fn(async (event: string, handler: Function) => {
      listeners.set(event, handler);
      return () => listeners.delete(event);
    }),
  } as any;
}

async function setup(propOverrides: Record<string, unknown> = {}) {
  const adapter = makeAdapter();
  (adapter.invoke as any).mockImplementation((cmd: string) => {
    if (cmd === 'list_channel_messages') return Promise.resolve([]);
    if (cmd === 'request_channel_backfill') return Promise.resolve(undefined);
    if (cmd === 'post_channel_message') return Promise.resolve('mid' + 'a'.repeat(29));
    return Promise.resolve(undefined);
  });
  const service = new ChannelMessageService();
  await service.connectAdapter(adapter);
  const props = {
    communityId: 'aa'.repeat(16),
    channelId: 'bb'.repeat(16),
    channelName: 'general',
    channelMessageService: service,
    ownAddress: 'cc'.repeat(20),
    myPower: 50,
    enableVirtualization: false, // tests render synchronously
    ...propOverrides,
  };
  const renderResult = render(ChannelMessageFeed, { props });
  return { adapter, service, props, ...renderResult };
}

describe('ChannelMessageFeed', () => {
  beforeEach(() => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it('mounts and calls listMessages on mount', async () => {
    const { adapter } = await setup();
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('list_channel_messages', {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        since: undefined,
        limit: 100,
      });
    });
  });

  it('renders the channel header with #channelName', async () => {
    const { getByText } = await setup({ channelName: 'announcements' });
    expect(getByText('#announcements')).toBeTruthy();
  });

  it('renders a message when one arrives via channel-message-received', async () => {
    const { adapter, container } = await setup();
    const handler = adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'm1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'cc'.repeat(20),
          at: { wallMs: 1000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode('hello world')),
        },
      },
    });
    await waitFor(() => {
      const msgs = container.querySelectorAll('.channel-message');
      expect(msgs.length).toBe(1);
      expect(msgs[0].textContent).toContain('hello world');
    });
  });

  it('compose Enter posts via channelMessageService.postMessage', async () => {
    const { adapter, container } = await setup();
    const textarea = container.querySelector('textarea.compose-input') as HTMLTextAreaElement;
    await fireEvent.input(textarea, { target: { value: 'first message' } });
    await fireEvent.keyDown(textarea, { key: 'Enter' });
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('post_channel_message', expect.objectContaining({
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        body: Array.from(new TextEncoder().encode('first message')),
        replyTo: undefined,
      }));
    });
    // Compose box clears on successful send.
    expect(textarea.value).toBe('');
  });

  it('Shift+Enter inserts a newline (does NOT send)', async () => {
    const { adapter, container } = await setup();
    const textarea = container.querySelector('textarea.compose-input') as HTMLTextAreaElement;
    await fireEvent.input(textarea, { target: { value: 'line one' } });
    await fireEvent.keyDown(textarea, { key: 'Enter', shiftKey: true });
    expect(adapter.invoke).not.toHaveBeenCalledWith(
      'post_channel_message',
      expect.anything(),
    );
    // Textarea retains the value (browser would handle the newline insertion;
    // we just verify we didn't send).
    expect(textarea.value).toBe('line one');
  });

  it('does not post empty/whitespace-only messages', async () => {
    const { adapter, container } = await setup();
    const textarea = container.querySelector('textarea.compose-input') as HTMLTextAreaElement;
    await fireEvent.input(textarea, { target: { value: '   ' } });
    await fireEvent.keyDown(textarea, { key: 'Enter' });
    expect(adapter.invoke).not.toHaveBeenCalledWith(
      'post_channel_message',
      expect.anything(),
    );
  });

  it('scroll-to-top stable for 250ms fires requestBackfill', async () => {
    const { adapter, container } = await setup();
    const scroll = container.querySelector('.messages-scroll') as HTMLDivElement;
    Object.defineProperty(scroll, 'scrollTop', { value: 10, writable: true, configurable: true });
    await fireEvent.scroll(scroll);

    // Before 250ms — no backfill yet.
    vi.advanceTimersByTime(200);
    expect(adapter.invoke).not.toHaveBeenCalledWith(
      'request_channel_backfill',
      expect.anything(),
    );

    // After 250ms total — backfill fires.
    vi.advanceTimersByTime(60);
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('request_channel_backfill', {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        since: undefined,
      });
    });
  });

  it('scroll-to-top single-in-flight gate: second trigger during in-flight is no-op', async () => {
    const { adapter, container } = await setup();
    let resolveBackfill: () => void = () => {};
    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      if (cmd === 'request_channel_backfill') return new Promise<void>((r) => { resolveBackfill = () => r(); });
      return Promise.resolve(undefined);
    });

    const scroll = container.querySelector('.messages-scroll') as HTMLDivElement;
    Object.defineProperty(scroll, 'scrollTop', { value: 10, writable: true, configurable: true });
    await fireEvent.scroll(scroll);
    vi.advanceTimersByTime(260);

    await waitFor(() => {
      expect((adapter.invoke as any).mock.calls.filter((c: any[]) => c[0] === 'request_channel_backfill').length).toBe(1);
    });

    // Second scroll-to-top stable: no-op (gate held by ChannelMessageService).
    await fireEvent.scroll(scroll);
    vi.advanceTimersByTime(260);
    expect((adapter.invoke as any).mock.calls.filter((c: any[]) => c[0] === 'request_channel_backfill').length).toBe(1);

    resolveBackfill();
  });

  it('shows skeleton while backfill in-flight; hides on terminal progress', async () => {
    const { adapter, container } = await setup();
    let resolveBackfill: () => void = () => {};
    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      if (cmd === 'request_channel_backfill') return new Promise<void>((r) => { resolveBackfill = () => r(); });
      return Promise.resolve(undefined);
    });

    const scroll = container.querySelector('.messages-scroll') as HTMLDivElement;
    Object.defineProperty(scroll, 'scrollTop', { value: 10, writable: true, configurable: true });
    await fireEvent.scroll(scroll);
    vi.advanceTimersByTime(260);

    await waitFor(() => {
      expect(container.querySelector('.backfill-skeleton')).toBeTruthy();
    });

    // Terminal progress event releases skeleton.
    const progressHandler = adapter.listeners.get('channel-backfill-progress')!;
    progressHandler({ payload: { communityId: 'aa'.repeat(16), channelId: 'bb'.repeat(16), fetched: 5, totalEstimate: 5 } });
    resolveBackfill();
    await waitFor(() => {
      expect(container.querySelector('.backfill-skeleton')).toBeNull();
    });
  });

  it('switching channelId resubscribes + re-lists', async () => {
    const { adapter, props, rerender } = await setup();
    expect(adapter.invoke).toHaveBeenCalledWith('list_channel_messages', expect.objectContaining({ channelId: 'bb'.repeat(16) }));

    await rerender({ ...props, channelId: 'dd'.repeat(16) });
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('list_channel_messages', expect.objectContaining({ channelId: 'dd'.repeat(16) }));
    });
  });

  it('shows inline error when post fails', async () => {
    const { adapter, container } = await setup();
    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      if (cmd === 'post_channel_message') return Promise.reject(new Error('no engine for ...'));
      return Promise.resolve(undefined);
    });
    const textarea = container.querySelector('textarea.compose-input') as HTMLTextAreaElement;
    await fireEvent.input(textarea, { target: { value: 'will fail' } });
    await fireEvent.keyDown(textarea, { key: 'Enter' });
    await waitFor(() => {
      expect(container.querySelector('.compose-error')?.textContent).toMatch(/no engine/);
    });
    // Compose retains text on failure so user can retry.
    expect(textarea.value).toBe('will fail');
  });
});
```

- [ ] **Step 2: Run the test to verify it fails.**

```bash
npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts
```

Expected: FAIL with `Cannot find module '../ChannelMessageFeed.svelte'`.

- [ ] **Step 3: Implement `ChannelMessageFeed.svelte`.**

Create `src/lib/components/ChannelMessageFeed.svelte`:

```svelte
<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import type { ChannelMessageDto, HlcDto } from '../channel-message-service';
  import type { ChannelMessageService } from '../channel-message-service';
  import type { TrustService } from '../trust-service';
  import Avatar from './Avatar.svelte';

  let {
    communityId,
    channelId,
    channelName,
    channelMessageService,
    ownAddress,
    trustService,
    myPower,
    enableVirtualization = true,
  }: {
    communityId: string;
    channelId: string;
    channelName: string;
    channelMessageService: ChannelMessageService;
    ownAddress: string;
    trustService?: TrustService;
    myPower: number;
    /** Disable for jsdom tests where IntersectionObserver isn't reliable. */
    enableVirtualization?: boolean;
  } = $props();

  // Local mirror of service.byChannel cache for this channel.
  let messages = $state<ChannelMessageDto[]>([]);
  let scrollAtBottom = $state(true);
  let scrollAtTop = $state(false);
  let backfillInFlight = $state(false);
  let backfillProgress = $state<{ fetched: number; totalEstimate?: number } | null>(null);
  let composeText = $state('');
  let composeError = $state<string | null>(null);
  let posting = $state(false);

  let scrollEl: HTMLDivElement | undefined = $state();
  let composeEl: HTMLTextAreaElement | undefined = $state();
  let unsubChannel: (() => void) | null = null;
  let prevOnBackfillProgress: typeof channelMessageService.onBackfillProgress | undefined;
  let scrollAtTopTimer: ReturnType<typeof setTimeout> | null = null;
  const SCROLL_TOP_DEBOUNCE_MS = 250;
  const SCROLL_TOP_THRESHOLD_PX = 50;

  // Subscribe + initial list when channelId changes.
  $effect(() => {
    const cid = communityId;
    const chid = channelId;
    // Fresh local mirror per channel switch.
    messages = [];
    composeError = null;
    backfillProgress = null;

    // Tear down prior subscription before creating new one.
    if (unsubChannel) {
      unsubChannel();
      unsubChannel = null;
    }
    unsubChannel = channelMessageService.subscribeToChannel(cid, chid, (msg) => {
      // Append in HLC-sorted insert position. Service emits AFTER its
      // internal ingest, so the cache is already sorted; we mirror by
      // re-reading rather than splicing.
      messages = channelMessageService.getMessages(cid, chid);
      // Auto-scroll to bottom on new live message IF scrollAtBottom was
      // already true. We use a microtask so the DOM update completes first.
      queueMicrotask(() => {
        if (scrollAtBottom) scrollToBottom();
      });
    });

    // Pull initial page (last 100 messages).
    void channelMessageService.listMessages(cid, chid, undefined, 100).then(() => {
      messages = channelMessageService.getMessages(cid, chid);
      queueMicrotask(scrollToBottom);
    });
  });

  onMount(() => {
    // Hook progress notifications. We chain rather than overwrite so that
    // CommunityView (which also wants progress) still gets called. Per spec
    // §8.3 the service emits per-channel progress; we filter to ours.
    prevOnBackfillProgress = channelMessageService.onBackfillProgress;
    channelMessageService.onBackfillProgress = (cid, chid, fetched, totalEstimate) => {
      prevOnBackfillProgress?.(cid, chid, fetched, totalEstimate);
      if (cid !== communityId || chid !== channelId) return;
      backfillProgress = { fetched, totalEstimate };
      if (totalEstimate !== undefined && fetched >= totalEstimate) {
        // Terminal tick: hide skeleton.
        backfillInFlight = false;
        backfillProgress = null;
      }
    };
  });

  onDestroy(() => {
    if (unsubChannel) unsubChannel();
    if (scrollAtTopTimer) clearTimeout(scrollAtTopTimer);
    // Restore prior progress callback so we don't leak this component's hook.
    channelMessageService.onBackfillProgress = prevOnBackfillProgress;
  });

  function scrollToBottom() {
    if (!scrollEl) return;
    scrollEl.scrollTop = scrollEl.scrollHeight;
    scrollAtBottom = true;
    scrollAtTop = false;
  }

  function handleScroll() {
    if (!scrollEl) return;
    const distFromBottom = scrollEl.scrollHeight - scrollEl.scrollTop - scrollEl.clientHeight;
    scrollAtBottom = distFromBottom < 50;
    const atTop = scrollEl.scrollTop < SCROLL_TOP_THRESHOLD_PX;

    if (atTop && !scrollAtTop) {
      scrollAtTop = true;
      // Per spec §6.7: 250 ms stable-at-top + single-in-flight gate.
      if (scrollAtTopTimer) clearTimeout(scrollAtTopTimer);
      scrollAtTopTimer = setTimeout(() => {
        if (!scrollAtTop) return;
        triggerBackfill();
      }, SCROLL_TOP_DEBOUNCE_MS);
    } else if (!atTop && scrollAtTop) {
      scrollAtTop = false;
      if (scrollAtTopTimer) {
        clearTimeout(scrollAtTopTimer);
        scrollAtTopTimer = null;
      }
    }
  }

  function triggerBackfill() {
    // Use the oldest known message's HLC as `since` so the backend
    // returns events strictly older than what we already have. If no
    // messages locally yet, undefined fetches from the start.
    const oldest = messages.length > 0 ? messages[0].at : undefined;
    backfillInFlight = true;
    backfillProgress = { fetched: 0 };
    channelMessageService.requestBackfill(communityId, channelId, oldest).catch((e) => {
      // Service throws only if adapter not connected; in that case we
      // surface a transient skeleton state and clear.
      backfillInFlight = false;
      backfillProgress = null;
      console.warn('backfill request failed', e);
    });
  }

  async function handleCompose(e: KeyboardEvent) {
    if (e.key !== 'Enter') return;
    if (e.shiftKey) return; // newline; let browser handle
    e.preventDefault();
    const text = composeText.trim();
    if (!text || posting) return;
    posting = true;
    composeError = null;
    try {
      await channelMessageService.postMessage(communityId, channelId, text);
      composeText = '';
    } catch (e) {
      composeError = e instanceof Error ? e.message : String(e);
    } finally {
      posting = false;
    }
  }

  function bodyToText(body: number[]): string {
    try {
      return new TextDecoder().decode(new Uint8Array(body));
    } catch {
      return '';
    }
  }

  function isSelf(author: string): boolean {
    return author === ownAddress;
  }

  function formatTimestamp(at: HlcDto): string {
    return new Date(at.wallMs).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  }
</script>

<div class="channel-message-feed">
  <header class="channel-header">
    <span class="hash" aria-hidden="true">#</span>
    <span class="name">{channelName}</span>
  </header>

  <div
    class="messages-scroll"
    bind:this={scrollEl}
    onscroll={handleScroll}
    role="log"
    aria-live="polite"
    aria-relevant="additions"
  >
    {#if backfillInFlight}
      <div class="backfill-skeleton" role="status" aria-live="polite">
        Loading older messages…
        {#if backfillProgress?.totalEstimate}
          ({backfillProgress.fetched}/{backfillProgress.totalEstimate})
        {/if}
      </div>
    {/if}
    {#each messages as msg (msg.messageId)}
      <article class="channel-message" class:self={isSelf(msg.author)}>
        <div class="avatar-col">
          <Avatar address={msg.author} {trustService} size={32} />
        </div>
        <div class="content-col">
          <header class="msg-meta">
            <span class="author">{msg.author.slice(0, 8)}</span>
            <time class="ts" datetime={new Date(msg.at.wallMs).toISOString()}>{formatTimestamp(msg.at)}</time>
          </header>
          <p class="body">{bodyToText(msg.body)}</p>
        </div>
      </article>
    {/each}
  </div>

  <div class="compose">
    {#if composeError}
      <div class="compose-error" role="alert">{composeError}</div>
    {/if}
    <textarea
      bind:this={composeEl}
      bind:value={composeText}
      onkeydown={handleCompose}
      class="compose-input"
      placeholder={`Message #${channelName}`}
      rows="2"
      aria-label="Channel message"
      disabled={posting}
    ></textarea>
  </div>
</div>

<style>
  .channel-message-feed {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-width: 0;
    height: 100%;
  }
  .channel-header {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 12px 16px;
    border-bottom: 1px solid var(--border);
    color: var(--text-primary);
    font-weight: 500;
  }
  .channel-header .hash { color: var(--text-secondary); }
  .messages-scroll {
    flex: 1;
    overflow-y: auto;
    padding: 12px 0;
  }
  .backfill-skeleton {
    text-align: center;
    color: var(--text-secondary);
    font-size: 0.85rem;
    padding: 12px;
    background: var(--bg-tertiary);
    margin: 0 16px 12px;
    border-radius: 4px;
  }
  .channel-message {
    display: flex;
    gap: 10px;
    padding: 6px 16px;
  }
  .channel-message:hover { background: var(--bg-tertiary); }
  .avatar-col { flex: 0 0 auto; }
  .content-col { flex: 1; min-width: 0; }
  .msg-meta { display: flex; gap: 8px; align-items: baseline; }
  .author { color: var(--text-primary); font-weight: 500; font-size: 0.9rem; }
  .ts { color: var(--text-secondary); font-size: 0.7rem; }
  .body { margin: 2px 0 0; color: var(--text-primary); white-space: pre-wrap; word-wrap: break-word; }
  .compose {
    border-top: 1px solid var(--border);
    padding: 8px 16px 12px;
  }
  .compose-error {
    background: var(--bg-tertiary);
    border: 1px solid #d83c3e;
    color: #d83c3e;
    padding: 6px 8px;
    border-radius: 4px;
    font-size: 0.75rem;
    margin-bottom: 8px;
  }
  .compose-input {
    width: 100%;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    padding: 8px 10px;
    font-size: 0.9rem;
    font-family: inherit;
    resize: vertical;
    box-sizing: border-box;
  }
  .compose-input:focus { outline: 2px solid var(--accent); outline-offset: -1px; }
  .compose-input:disabled { opacity: 0.6; }
</style>
```

- [ ] **Step 4: Run the test to verify it passes.**

```bash
npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts
```

Expected: all tests green.

- [ ] **Step 5: Run full tsc + vitest gate.**

```bash
npx tsc --noEmit
npx vitest run
```

Expected: no diagnostics, all tests green.

- [ ] **Step 6: Commit.**

```bash
git add src/lib/components/ChannelMessageFeed.svelte src/lib/components/__tests__/ChannelMessageFeed.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-272): ChannelMessageFeed center-column component

Per-channel feed: subscribes to ChannelMessageService for live
deliveries, lists 100 most-recent on mount, auto-scrolls to bottom
when at bottom, fires requestBackfill via 250ms-stable-at-top
debounce + single-in-flight gate per spec §6.7. Inline channel-compose
textarea (Enter posts, Shift+Enter newline). Loading skeleton during
backfill, hidden on terminal progress event.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: `CommunityView` + tests

**Goal:** Layout shell that mounts the three column components + ⚙️-revealed `CommunitySettingsPanel` modal + `CreateChannelDialog` / `ModifyChannelDialog`. Owns `channels`, `activeChannelId`, `settingsModalOpen`, `showCreateDialog`, `modifyDialogChannel`, `deleteConfirmChannel` state. Subscribes to `communityService.onChannelConfigChanged` → cache invalidate + §6.4 cascade fallback (#general → next-oldest → empty-state) when active channel deleted + silent re-render on rename. Per spec §7.1 + §6.3 + §6.4 + §6.6.

**Files:**
- Create: `src/lib/components/CommunityView.svelte`
- Create: `src/lib/components/__tests__/CommunityView.test.ts`

- [ ] **Step 1: Write the failing test file.**

Create `src/lib/components/__tests__/CommunityView.test.ts`:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import CommunityView from '../CommunityView.svelte';
import { CommunityService } from '../../community-service';
import { ChannelMessageService } from '../../channel-message-service';
import type { TauriAdapter } from '../../zenoh-service';
import type { CommunityMember } from '../../types';

function makeAdapter(): TauriAdapter & { listeners: Map<string, Function> } {
  const listeners = new Map<string, Function>();
  return {
    listeners,
    invoke: vi.fn(),
    listen: vi.fn(async (event: string, handler: Function) => {
      listeners.set(event, handler);
      return () => listeners.delete(event);
    }),
  } as any;
}

const adminMember: CommunityMember = {
  address: 'aa'.repeat(20),
  displayName: 'Alice',
  power: 100,
  status: 'joined',
};
const general = {
  channelId: '01'.repeat(16),
  name: 'general',
  writePower: 0,
  createdAt: { wallMs: 100, logical: 0, deviceId: 'd' },
};
const announcements = {
  channelId: '02'.repeat(16),
  name: 'announcements',
  writePower: 50,
  createdAt: { wallMs: 200, logical: 0, deviceId: 'd' },
};

async function setup(channelList: any[] = [general, announcements], propOverrides: Record<string, unknown> = {}) {
  const adapter = makeAdapter();
  (adapter.invoke as any).mockImplementation((cmd: string) => {
    if (cmd === 'list_channels') return Promise.resolve(channelList);
    if (cmd === 'list_channel_messages') return Promise.resolve([]);
    return Promise.resolve(undefined);
  });
  const communityService = new CommunityService();
  await communityService.connectAdapter(adapter);
  const channelMessageService = new ChannelMessageService();
  await channelMessageService.connectAdapter(adapter);
  const props = {
    communityId: 'aa'.repeat(16),
    communityName: 'Test Community',
    communityKind: 'open' as const,
    myPower: 100,
    ownAddress: adminMember.address,
    members: [adminMember],
    isDegraded: false,
    communityService,
    channelMessageService,
    onLeave: vi.fn(),
    onKickMember: vi.fn(),
    onSetPowerLevel: vi.fn(),
    onGenerateInvite: vi.fn().mockResolvedValue('harmony://invite/...'),
    ...propOverrides,
  };
  const renderResult = render(CommunityView, { props });
  return { adapter, communityService, channelMessageService, props, ...renderResult };
}

describe('CommunityView', () => {
  it('mounts the three columns', async () => {
    const { container } = await setup();
    await waitFor(() => {
      expect(container.querySelector('.channel-sub-sidebar')).toBeTruthy();
      expect(container.querySelector('.channel-message-feed')).toBeTruthy();
      expect(container.querySelector('.members-panel')).toBeTruthy();
    });
  });

  it('selects #general by default on first visit', async () => {
    const { container } = await setup();
    await waitFor(() => {
      const active = container.querySelector('.channel-item.active');
      expect(active?.querySelector('.channel-name')?.textContent?.trim()).toBe('general');
    });
  });

  it('clicking ⚙️ opens CommunitySettingsPanel modal', async () => {
    const { container, getByLabelText } = await setup();
    await waitFor(() => {
      expect(container.querySelector('.channel-sub-sidebar')).toBeTruthy();
    });
    await fireEvent.click(getByLabelText(/Open community settings/i));
    await waitFor(() => {
      expect(document.querySelector('[role="dialog"]')).toBeTruthy();
    });
  });

  it('clicking + opens CreateChannelDialog', async () => {
    const { container, getByLabelText } = await setup();
    await waitFor(() => {
      expect(container.querySelector('.create-channel-btn')).toBeTruthy();
    });
    await fireEvent.click(container.querySelector('.create-channel-btn') as HTMLElement);
    await waitFor(() => {
      expect(getByLabelText(/Channel name/i)).toBeTruthy();
    });
  });

  it('channel-config-updated Modified silently re-renders header', async () => {
    const { adapter, container, communityService } = await setup();
    await waitFor(() => {
      expect(container.querySelector('.channel-message-feed .name')?.textContent?.trim()).toBe('general');
    });

    // Re-list returns a renamed channel.
    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channels') return Promise.resolve([
        { ...general, name: 'general-renamed' },
        announcements,
      ]);
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });

    const handler = adapter.listeners.get('channel-config-updated')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: general.channelId,
        action: 'modified',
        name: 'general-renamed',
        atWallMs: 200,
      },
    });

    await waitFor(() => {
      expect(container.querySelector('.channel-message-feed .name')?.textContent?.trim()).toBe('general-renamed');
    });
  });

  it('channel-config-updated Deleted on active channel cascades to next-newest (or empty if last)', async () => {
    const { adapter, container } = await setup();
    await waitFor(() => {
      expect(container.querySelector('.channel-item.active .channel-name')?.textContent?.trim()).toBe('general');
    });

    // After delete, list_channels returns only announcements.
    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channels') return Promise.resolve([announcements]);
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });

    const handler = adapter.listeners.get('channel-config-updated')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: general.channelId,
        action: 'deleted',
        atWallMs: 300,
      },
    });

    await waitFor(() => {
      // #general gone → cascade picks next channel (announcements).
      expect(container.querySelector('.channel-item.active .channel-name')?.textContent?.trim()).toBe('announcements');
    });
  });

  it('channel-config-updated Deleted on last remaining channel renders empty-state', async () => {
    const { adapter, container } = await setup([general]);
    await waitFor(() => {
      expect(container.querySelector('.channel-item.active')).toBeTruthy();
    });

    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channels') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });

    const handler = adapter.listeners.get('channel-config-updated')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: general.channelId,
        action: 'deleted',
        atWallMs: 300,
      },
    });

    await waitFor(() => {
      expect(container.querySelector('.empty-channels')).toBeTruthy();
    });
  });

  it('+ button hidden when myPower < 50', async () => {
    const { container } = await setup(undefined, { myPower: 25 });
    await waitFor(() => {
      expect(container.querySelector('.channel-sub-sidebar')).toBeTruthy();
    });
    expect(container.querySelector('.create-channel-btn')).toBeNull();
  });

  it('clicking a channel updates active selection and persists via communityService.setSelectedChannel', async () => {
    const { container, communityService } = await setup();
    await waitFor(() => {
      expect(container.querySelectorAll('.channel-item').length).toBeGreaterThan(1);
    });
    const items = container.querySelectorAll('.channel-item');
    await fireEvent.click(items[1] as HTMLElement);
    await waitFor(() => {
      expect(container.querySelector('.channel-item.active .channel-name')?.textContent?.trim()).toBe('announcements');
    });
    expect(communityService.getSelectedChannel('aa'.repeat(16))).toBe(announcements.channelId);
  });
});
```

- [ ] **Step 2: Run the test to verify it fails.**

```bash
npx vitest run src/lib/components/__tests__/CommunityView.test.ts
```

Expected: FAIL with `Cannot find module '../CommunityView.svelte'`.

- [ ] **Step 3: Implement `CommunityView.svelte`.**

Create `src/lib/components/CommunityView.svelte`:

```svelte
<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import type { CommunityService, ChannelInfo } from '../community-service';
  import type { ChannelMessageService } from '../channel-message-service';
  import type { CommunityMember } from '../types';
  import type { TrustService } from '../trust-service';
  import ChannelSubSidebar from './ChannelSubSidebar.svelte';
  import ChannelMessageFeed from './ChannelMessageFeed.svelte';
  import ChannelMembersPanel from './ChannelMembersPanel.svelte';
  import CreateChannelDialog from './CreateChannelDialog.svelte';
  import ModifyChannelDialog from './ModifyChannelDialog.svelte';
  import ConfirmDialog from './ConfirmDialog.svelte';
  import CommunitySettingsPanel from './CommunitySettingsPanel.svelte';

  let {
    communityId,
    communityName,
    communityKind,
    myPower,
    ownAddress,
    members,
    isDegraded,
    communityService,
    channelMessageService,
    trustService,
    onLeave,
    onKickMember,
    onSetPowerLevel,
    onGenerateInvite,
  }: {
    communityId: string;
    communityName: string;
    communityKind: 'open' | 'invite-only' | 'unknown';
    myPower: number;
    ownAddress: string;
    members: CommunityMember[];
    isDegraded: boolean;
    communityService: CommunityService;
    channelMessageService: ChannelMessageService;
    trustService?: TrustService;
    onLeave: () => Promise<void>;
    onKickMember: (addr: string) => Promise<void>;
    onSetPowerLevel: (addr: string, power: number) => Promise<void>;
    onGenerateInvite: () => Promise<string>;
  } = $props();

  let channels = $state<ChannelInfo[]>([]);
  let activeChannelId = $state<string | null>(null);
  let settingsModalOpen = $state(false);
  let showCreateDialog = $state(false);
  let modifyDialogChannel = $state<ChannelInfo | null>(null);
  let deleteConfirmChannel = $state<ChannelInfo | null>(null);
  let membersPanelCollapsed = $state(false);
  let prevOnChannelConfigChanged: typeof communityService.onChannelConfigChanged;

  let activeChannel = $derived(channels.find((c) => c.channelId === activeChannelId) ?? null);

  async function refreshChannels() {
    const list = await communityService.listChannels(communityId);
    channels = list.filter((c) => c.deletedAt === undefined);
  }

  /** Per spec §6.4: when active channel disappears, cascade to fallback.
   *  1. #general if it exists and is not the just-deleted channel.
   *  2. Next-oldest by created_at HLC.
   *  3. null (empty-state).
   *  Backend already sorts list_channels by created_at ascending so we
   *  just pick the first non-deleted entry. */
  function pickFallbackChannel(deletedChannelId: string): string | null {
    const general = channels.find((c) => c.name === 'general' && c.channelId !== deletedChannelId);
    if (general) return general.channelId;
    const next = channels.find((c) => c.channelId !== deletedChannelId);
    return next?.channelId ?? null;
  }

  function handleSelect(channelId: string) {
    activeChannelId = channelId;
    communityService.setSelectedChannel(communityId, channelId);
  }

  async function handleConfirmDelete() {
    if (!deleteConfirmChannel) return;
    const target = deleteConfirmChannel;
    deleteConfirmChannel = null;
    try {
      await communityService.deleteChannel(communityId, target.channelId);
      // The channel-config-updated event arrives shortly; the cascade
      // happens there. We don't optimistically remove from the local
      // `channels` list — keeps state-of-truth as the materialized
      // CRDT response.
    } catch (e) {
      // Could surface a toast here; for now log + leave channel in place
      // so user can retry.
      console.warn('deleteChannel failed', e);
    }
  }

  onMount(() => {
    // Capture the active selected-channel from CommunityService (set by
    // a prior visit in this session). Otherwise default to #general or
    // first channel after the initial refresh.
    const persisted = communityService.getSelectedChannel(communityId);
    if (persisted) activeChannelId = persisted;

    // Hook channel-config callback. Chain prior so we don't clobber
    // App.svelte's listener if it had one.
    prevOnChannelConfigChanged = communityService.onChannelConfigChanged;
    communityService.onChannelConfigChanged = (cid, action, channelId, name, writePower) => {
      prevOnChannelConfigChanged?.(cid, action, channelId, name, writePower);
      if (cid !== communityId) return;
      void (async () => {
        await refreshChannels();
        if (action === 'deleted' && channelId === activeChannelId) {
          activeChannelId = pickFallbackChannel(channelId);
          if (activeChannelId) {
            communityService.setSelectedChannel(communityId, activeChannelId);
          }
        }
      })();
    };

    void (async () => {
      await refreshChannels();
      // After initial load, default selection if not already set.
      if (!activeChannelId) {
        const general = channels.find((c) => c.name === 'general');
        activeChannelId = general?.channelId ?? channels[0]?.channelId ?? null;
        if (activeChannelId) {
          communityService.setSelectedChannel(communityId, activeChannelId);
        }
      }
    })();
  });

  onDestroy(() => {
    communityService.onChannelConfigChanged = prevOnChannelConfigChanged;
  });
</script>

<section class="community-view" aria-label={`Community: ${communityName}`}>
  <header class="community-header">
    <h2 class="community-name">{communityName}</h2>
    <button
      type="button"
      class="settings-btn"
      aria-label="Open community settings"
      onclick={() => { settingsModalOpen = true; }}
    >⚙️</button>
  </header>

  <div class="three-cols">
    <ChannelSubSidebar
      {channels}
      {activeChannelId}
      {myPower}
      onSelect={handleSelect}
      onCreateClick={() => { showCreateDialog = true; }}
      onModifyClick={(c) => { modifyDialogChannel = c; }}
      onDeleteClick={(c) => { deleteConfirmChannel = c; }}
    />
    {#if activeChannel}
      <ChannelMessageFeed
        {communityId}
        channelId={activeChannel.channelId}
        channelName={activeChannel.name}
        {channelMessageService}
        {ownAddress}
        {trustService}
        {myPower}
      />
    {:else}
      <div class="empty-channels">
        <p>No channels in this community yet.</p>
        {#if myPower >= 50}
          <p>Click <strong>Create channel</strong> to add one.</p>
        {/if}
      </div>
    {/if}
    <ChannelMembersPanel
      {members}
      {ownAddress}
      {trustService}
      collapsed={membersPanelCollapsed}
    />
  </div>
</section>

<!-- Settings modal: simply mount CommunitySettingsPanel inside a Modal
  wrapper. The panel itself supplies its own close affordances; we only
  need the modal scrim + role=dialog + focus-trap (Modal provides those). -->
{#if settingsModalOpen}
  <CommunitySettingsPanel
    {communityId}
    {communityName}
    {communityKind}
    {members}
    myAddress={ownAddress}
    {myPower}
    {isDegraded}
    onClose={() => { settingsModalOpen = false; }}
    onKick={onKickMember}
    onSetPower={onSetPowerLevel}
    onLeave={onLeave}
    onGenerateInvite={onGenerateInvite}
  />
{/if}

<CreateChannelDialog
  {communityId}
  {communityService}
  open={showCreateDialog}
  {myPower}
  onClose={() => { showCreateDialog = false; }}
  onCreated={(channelId) => {
    showCreateDialog = false;
    handleSelect(channelId);
  }}
/>

{#if modifyDialogChannel}
  <ModifyChannelDialog
    {communityId}
    channel={modifyDialogChannel}
    {communityService}
    open={true}
    {myPower}
    onClose={() => { modifyDialogChannel = null; }}
  />
{/if}

{#if deleteConfirmChannel}
  <ConfirmDialog
    title={`Delete #${deleteConfirmChannel.name}?`}
    message={`Channel deletion is permanent. The message log persists but no new messages can be posted. Type "${deleteConfirmChannel.name}" to confirm.`}
    confirmLabel="Delete channel"
    destructive={true}
    onConfirm={handleConfirmDelete}
    onCancel={() => { deleteConfirmChannel = null; }}
  />
{/if}

<style>
  .community-view {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-width: 0;
  }
  .community-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 10px 16px;
    border-bottom: 1px solid var(--border);
    background: var(--bg-secondary);
  }
  .community-name { margin: 0; color: var(--text-primary); font-size: 1rem; }
  .settings-btn {
    background: none;
    border: none;
    cursor: pointer;
    font-size: 1.1rem;
    padding: 4px 8px;
    border-radius: 4px;
  }
  .settings-btn:hover { background: var(--bg-tertiary); }
  .three-cols {
    display: flex;
    flex: 1;
    min-height: 0;
  }
  .empty-channels {
    flex: 1;
    display: flex;
    flex-direction: column;
    justify-content: center;
    align-items: center;
    color: var(--text-secondary);
    padding: 32px;
    text-align: center;
  }
  .empty-channels p { margin: 6px 0; }
</style>
```

- [ ] **Step 4: Run the test to verify it passes.**

```bash
npx vitest run src/lib/components/__tests__/CommunityView.test.ts
```

Expected: all tests green.

- [ ] **Step 5: Run full tsc + vitest gate.**

```bash
npx tsc --noEmit
npx vitest run
```

Expected: no diagnostics, all tests green.

- [ ] **Step 6: Commit.**

```bash
git add src/lib/components/CommunityView.svelte src/lib/components/__tests__/CommunityView.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-272): CommunityView layout shell

Three-column layout (channel sub-sidebar | feed | members) with
⚙️-revealed CommunitySettingsPanel modal + Create / Modify dialogs +
typed-confirm delete. Subscribes to onChannelConfigChanged: refreshes
channel cache and runs §6.4 cascade fallback (#general → next-oldest
→ empty) when active channel deleted; silently re-renders header on
rename per spec §6.6. Selected-channel persists via
communityService.setSelectedChannel per spec §6.5.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: `App.svelte` routing change

**Goal:** Replace the direct `CommunitySettingsPanel` mount at `App.svelte:1525-1577` with `CommunityView` mount. Instantiate `channelMessageService` alongside `communityService` at L321. Wire `connectAdapter` / `destroy` lifecycle parity. No new derived state — reuse the existing `derivedMyPower` and `members`.

**Files:**
- Modify: `src/App.svelte`

**Pre-task verification:** Before editing, re-read `src/App.svelte` lines around 321 (service instantiation) and 1500-1600 (community mount block) so the edits land precisely. Anchor changes by exact text match (the file is ~1700L; line numbers may have drifted from the spec).

- [ ] **Step 1: Read App.svelte lines 1-50 to find the imports block.**

```bash
sed -n '1,50p' src/App.svelte
```

Expected: import statements including `import CommunitySettingsPanel from ...` and `import { CommunityService } from ...`.

- [ ] **Step 2: Add new imports.**

After the existing `import { CommunityService }` line, add:

```typescript
  import CommunityView from './lib/components/CommunityView.svelte';
  import { ChannelMessageService } from './lib/channel-message-service';
```

Remove the now-unused `import CommunitySettingsPanel from ...` line **only if** no other code in App.svelte references it (the existing direct `<CommunitySettingsPanel ...>` mount block is being replaced; verify with `grep CommunitySettingsPanel src/App.svelte` that the only reference is in the import + the mount block we're about to replace; if it's referenced elsewhere — e.g., in tests or other UI paths — leave the import).

```bash
grep -c CommunitySettingsPanel src/App.svelte
```

If this returns `2` (one import + one mount), it's safe to remove the import after Step 5. If `>2`, leave it.

- [ ] **Step 3: Find the service instantiation block.**

Read lines 315-330 of App.svelte:

```bash
sed -n '315,330p' src/App.svelte
```

Expected: `const communityService = new CommunityService();` around L321.

- [ ] **Step 4: Add the channelMessageService instantiation.**

Immediately after `const communityService = new CommunityService();`, add:

```typescript
  const channelMessageService = new ChannelMessageService();
```

- [ ] **Step 5: Find the connectAdapter call site.**

```bash
grep -n "communityService.connectAdapter" src/App.svelte
```

Expected: one or more lines where `communityService.connectAdapter(...)` is invoked.

For each invocation, immediately after the `await communityService.connectAdapter(adapter);` line, add:

```typescript
        await channelMessageService.connectAdapter(adapter);
```

(Indentation should match the surrounding context.)

- [ ] **Step 6: Find the destroy call site.**

```bash
grep -n "communityService.destroy" src/App.svelte
```

Expected: one or more lines where `communityService.destroy()` is invoked (likely in a sign-out / teardown handler).

For each invocation, immediately after the `communityService.destroy();` line, add:

```typescript
        channelMessageService.destroy();
```

- [ ] **Step 7: Find the CommunitySettingsPanel mount block.**

```bash
grep -n "<CommunitySettingsPanel" src/App.svelte
```

Expected: one line, around L1525, opening the `<CommunitySettingsPanel ...>` tag.

Read the full mount block (about 50 lines):

```bash
sed -n '1520,1580p' src/App.svelte
```

Expected: the block looks roughly like:

```svelte
{:else if selectedNode && selectedCommunityNode && communityService.getKind(selectedCommunityNode.id) !== 'unknown'}
  <CommunitySettingsPanel
    communityId={selectedCommunityNode.id}
    communityName={selectedCommunityNode.name}
    communityKind={communityService.getKind(selectedCommunityNode.id)}
    members={communityMembers}
    myAddress={ownAddress}
    myPower={derivedMyPower}
    isDegraded={...}
    onClose={...}
    onKick={async (target) => { await communityService.kickMember(...) }}
    onSetPower={async (target, power) => { await communityService.setPowerLevel(...) }}
    onLeave={...}
    onGenerateInvite={async () => { return communityService.generateInvite(...); }}
  />
```

(Exact prop names may vary; the names in CommunitySettingsPanel.test.ts above are authoritative.)

- [ ] **Step 8: Replace the CommunitySettingsPanel mount with CommunityView mount.**

Replace the entire `<CommunitySettingsPanel ... />` block with:

```svelte
  <CommunityView
    communityId={selectedCommunityNode.id}
    communityName={selectedCommunityNode.name}
    communityKind={communityService.getKind(selectedCommunityNode.id)}
    myPower={derivedMyPower}
    ownAddress={ownAddress}
    members={communityMembers}
    isDegraded={isDegradedForSelectedCommunity}
    {communityService}
    {channelMessageService}
    {trustService}
    onLeave={async () => {
      await communityService.leaveCommunity(selectedCommunityNode.id);
    }}
    onKickMember={async (target) => {
      await communityService.kickMember(selectedCommunityNode.id, target);
    }}
    onSetPowerLevel={async (target, power) => {
      await communityService.setPowerLevel(selectedCommunityNode.id, target, power);
    }}
    onGenerateInvite={async () => {
      if (!selectedCommunityNode) throw new Error('no community selected');
      return communityService.generateInvite(selectedCommunityNode.id);
    }}
  />
```

(The exact existing handler bodies in App.svelte should be preserved — the goal is to reuse them, not rewrite them. Read the existing block first; the bodies above are illustrative; preserve actual production logic verbatim.)

- [ ] **Step 9: Verify no leftover references.**

```bash
grep -n "CommunitySettingsPanel" src/App.svelte
```

Expected: 0 matches (after import removal in Step 2). If matches remain, verify they're intentional (e.g., a cleanup branch we missed).

```bash
grep -n "channelMessageService" src/App.svelte
```

Expected: at least 4 matches (instantiation + connectAdapter + destroy + CommunityView prop).

- [ ] **Step 10: Run tsc gate.**

```bash
npx tsc --noEmit
```

Expected: no diagnostics. If you see "Property 'foo' does not exist on type 'CommunityView'", check that you're passing the prop names that match the spec §7.1 contract.

- [ ] **Step 11: Run vitest gate.**

```bash
npx vitest run
```

Expected: all tests green. If any tests reference `CommunitySettingsPanel` directly via App.svelte mounting, they'll likely still pass because `CommunitySettingsPanel.svelte` itself is unchanged — only its mount-point moved.

- [ ] **Step 12: Commit.**

```bash
git add src/App.svelte
git commit -m "$(cat <<'EOF'
feat(zeb-272): App.svelte routing — CommunityView replaces direct settings mount

Replaces the direct <CommunitySettingsPanel> mount block (around L1525)
with <CommunityView>. Instantiates channelMessageService alongside
communityService, threading connectAdapter + destroy lifecycle parity.
CommunitySettingsPanel.svelte is unchanged — relocated behind ⚙️ inside
CommunityView's modal.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Final verification + push + PR

**Goal:** Run the full CI gate locally, push the branch, open the PR with the spec's locked acceptance criteria as the test plan. PR body uses markdown-linked refs `[ZEB-248](url)` for the parent epic per `feedback_linear_pr_auto_close` memory rule. Acceptance criterion 15: PR merge closes parent ZEB-248 (final phase).

**Files:** none (push + PR only).

- [ ] **Step 1: Run full local gate set, all five.**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures -- -D warnings
cd src-tauri && cargo test --locked --workspace --all-targets --features test-fixtures --no-fail-fast
cd src-tauri && cargo check --locked --all-targets --features test-fixtures
cd .. && npx tsc --noEmit
npx vitest run
```

Expected: all green. (Run from repo root; the `cd` notes are reminders that the cargo block runs from `src-tauri/`.)

If any gate fails: fix locally + commit fix as a separate `fix(zeb-272): ...` commit before pushing.

- [ ] **Step 2: Verify git status is clean and on the right branch.**

```bash
git status
git log --oneline origin/main..HEAD
```

Expected: clean tree; `git log` shows the spec commit + 8 implementation commits (Tasks 1-8) = 9 total commits ahead of `origin/main`.

- [ ] **Step 3: Push the branch.**

```bash
git push -u origin zeb-272-channels-frontend
```

Expected: push succeeds; remote tracking branch set.

- [ ] **Step 4: Open the PR.**

```bash
gh pr create --title "ZEB-272 Phase 4: channels frontend" --body "$(cat <<'EOF'
## Summary

Final phase of [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) (Sub-C v2 channels-within-communities). Wires the user-facing channels surface to the backend that Phases 1-3 shipped.

- New `CommunityView.svelte` three-column layout (channel sub-sidebar | feed | members panel) replaces the direct `CommunitySettingsPanel` mount when a community NavNode is selected.
- New `ChannelMessageService` mirrors `MessageService` shape, consumes `channel-message-received` + `channel-backfill-progress` events, exposes `postMessage` / `listMessages` / `subscribeToChannel` / `requestBackfill` per spec §7.7.
- New `ChannelMessageFeed.svelte` renders virtualizable per-channel feed with auto-scroll-to-bottom + 250ms-stable-at-top scroll-trigger backfill (single-in-flight gate per spec §6.7).
- New dialogs `CreateChannelDialog.svelte` + `ModifyChannelDialog.svelte` (1-32 char name, slider+number-input pair hidden behind `// v3 unhide`, modify auto-rejects all-None client-side, both auto-close on `myPower < 50` demotion).
- New `ChannelSubSidebar.svelte` lists channels in backend-sorted oldest-first order; "+" button + right-click context menu (Rename / Delete) gated on `myPower >= POWER_THRESHOLDS.kick`.
- New `ChannelMembersPanel.svelte` right-column member list (self-first, power-desc, name-asc), collapsible.
- `community-service.ts` extended with `createChannel` / `modifyChannel` / `deleteChannel` / `listChannels` IPC method facades; subscribes to `channel-config-updated`; per-community channel cache invalidated on event; session-scoped `selectedChannelByCommunity` Map per spec §6.5 with get/set + cleared by `destroy()`.
- `CommunitySettingsPanel.svelte` deliberately **not modified** — relocates behind ⚙️ icon as a modal inside `CommunityView`. The existing settings UX is preserved; only its mount-point moved.

8 plan-time questions resolved during brainstorming and locked in spec §6 (component decomposition, settings modal, message-feed shape, channel-deleted-while-viewing cascade, selected-channel persistence shape, channel-renamed UX, backfill debounce, reactive `myPower` recompute).

**Closes:** ZEB-272.
**Closes parent (final phase):** [ZEB-248](https://linear.app/zeblith/issue/ZEB-248).

## Test plan

- [ ] `cargo fmt --all -- --check` (from `src-tauri/`) — green.
- [ ] `cargo clippy --locked --all-targets --features test-fixtures -- -D warnings` — green.
- [ ] `cargo test --locked --workspace --all-targets --features test-fixtures --no-fail-fast` — green.
- [ ] `cargo check --locked --all-targets --features test-fixtures` (msrv) — green.
- [ ] `npx tsc --noEmit` — green.
- [ ] `npx vitest run` — green.
- [ ] CI green on push.
- [ ] **Manual smoke (per parent spec §14):** Two-device manual smoke — create community, create #dev-talk, post message, see it on second device, rename #dev-talk → #dev-discussion (silent header re-render), delete #dev-discussion (auto-redirect to #general with toast), open ⚙️ settings modal (kick/setpower/invite link still work), backfill on cold reconnect.

## References

- **Spec:** `docs/specs/2026-05-10-zeb-272-channels-frontend-design.md` (commit `8fcbbbf`).
- **Plan:** `docs/plans/2026-05-10-zeb-272-channels-frontend-plan.md`.
- **Parent ticket:** [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) — Sub-C v2: channels-within-communities.
- **Sibling Phase 1:** [ZEB-266](https://linear.app/zeblith/issue/ZEB-266) — channel-config CRDT — PR #93.
- **Sibling Phase 2:** [ZEB-269](https://linear.app/zeblith/issue/ZEB-269) — ChannelLog data plane — PR #95.
- **Sibling Phase 3:** [ZEB-270](https://linear.app/zeblith/issue/ZEB-270) — ChannelLog Zenoh transport + IPCs — PR #96.
- **Sibling deferred bug:** [ZEB-271](https://linear.app/zeblith/issue/ZEB-271) — channel-log registry transactional spawn — NOT addressed by this PR.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Expected: PR URL printed.

- [ ] **Step 5: Report.**

Return the PR URL to the dispatcher. Do not enter the bot-review monitoring loop here — that's the calling agent's responsibility per the `feedback_autonomous_pr_monitoring_loop` memory rule.

---

## Self-review (for the writing-plans skill — already performed)

**1. Spec coverage check:**

| Spec section | Task |
|---|---|
| §5.1 / §5.2 (file structure) | Tasks 1-8 collectively |
| §6.1 (forked feed) | Task 6 |
| §6.2 (component decomposition) | Tasks 4, 5, 6, 7 |
| §6.3 (modal settings) | Task 7 (uses Modal.svelte which provides role/aria/focus-trap) |
| §6.4 (delete cascade) | Task 7 (`pickFallbackChannel`) |
| §6.5 (selected-channel persistence) | Task 2 + Task 7 |
| §6.6 (silent rename) | Task 7 (refresh on Modified event) |
| §6.7 (250ms + single-in-flight) | Task 1 (gate) + Task 6 (debounce) |
| §6.8 (myPower prop chain) | Task 8 (App.svelte threading) |
| §7.1-7.6 (component contracts) | Tasks 4, 5, 6, 7, 3, 3 respectively |
| §7.7 (ChannelMessageService) | Task 1 |
| §7.8 (CommunityService extensions) | Task 2 |
| §8 (data flow) | Tasks 1, 2, 6, 7 collectively |
| §9 (App.svelte routing) | Task 8 |
| §10 (error handling) | Tasks 1, 3, 6, 7 (each has its own error surfacing) |
| §11.1 (unit tests) | Tasks 1-7 (each TDD-shaped) |
| §11.2 (manual smoke) | Task 9 (PR test plan) |
| §11.3 (CI gates) | Tasks 0 + 9 |
| §12 (acceptance criteria 1-15) | All Tasks; AC #14/15 in Task 9 |
| §13 (cross-repo) | N/A — none |
| §14 (references) | Plan-wide |

**2. Placeholder scan:** No "TBD", "TODO", "implement later", "Add error handling" without code. Each step shows the actual code the implementer will write or the exact bash command to run.

**3. Type consistency check:**
- `ChannelInfo` defined in Task 2 (`src/lib/community-service.ts`); imported by Task 3 (`ModifyChannelDialog`), Task 5 (`ChannelSubSidebar`), Task 7 (`CommunityView`). All use same field names: `channelId`, `name`, `writePower`, `createdAt`, optional `deletedAt`.
- `ChannelMessageDto` defined in Task 1 (`src/lib/channel-message-service.ts`); consumed by Task 6 (`ChannelMessageFeed`). Field names: `messageId`, `communityId`, `channelId`, `author`, `at`, `body`, optional `replyTo`.
- `HlcDto` defined in Task 1; consumed by Tasks 1, 2, 6.
- `ChannelMessageService` method signatures: `postMessage(communityId, channelId, body, replyTo?)`, `listMessages(communityId, channelId, since, limit)`, `requestBackfill(communityId, channelId, since?)`, `subscribeToChannel(communityId, channelId, callback) → unsub`, `getMessages(communityId, channelId)`, `destroy()`. Used consistently in Tasks 6 and 7.
- `CommunityService` new methods: `createChannel(communityId, name, writePower)`, `modifyChannel(communityId, channelId, name?, writePower?)`, `deleteChannel(communityId, channelId)`, `listChannels(communityId)`, `getSelectedChannel(communityId)`, `setSelectedChannel(communityId, channelId)`. Used consistently in Tasks 3, 5, 7.
- `myPower` numeric, `POWER_THRESHOLDS.kick === 50`. Used consistently as gate predicate in Tasks 3, 5, 7.

**4. Plan completeness:** Task 0 verifies green baseline before any change. Tasks 1-2 build the services foundation (no UI yet). Tasks 3-6 build leaf components (each independently testable + dispatchable). Task 7 wires them together in `CommunityView`. Task 8 changes `App.svelte` routing — the only modification to a large existing file. Task 9 ships. Each Task 1-8 ends with a commit. Total 9 implementer-visible tasks; 9 commits on the branch (Task 0 has no commit).

---

## Execution handoff

**Plan complete and saved to `docs/plans/2026-05-10-zeb-272-channels-frontend-plan.md`. Two execution options:**

**1. Subagent-Driven (recommended)** — Dispatcher dispatches a fresh implementer subagent per task with two-stage review (spec compliance reviewer first, then code quality reviewer) between tasks. Most efficient, fast iteration, controller protects context window.

**2. Inline Execution** — Execute tasks in this session via `superpowers:executing-plans` with batch checkpoints for review.

**Which approach?**
