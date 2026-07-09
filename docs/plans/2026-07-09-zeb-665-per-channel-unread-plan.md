# ZEB-665: Per-channel unread counts — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Drive the dead `NavNode.unreadCount` scaffold for community channels: per-channel numeric unread badges (capped "99+"), quiet dot on the owning community, precise clear on open, over a persisted per-channel HLC read cursor.

**Architecture:** New `ChannelUnreadService` (dep-injected, `mention-alert.ts` pattern) maintains a capped per-channel Set of unread message IDs over a persisted owner-scoped HLC cursor (`UnreadCursorStore`, localStorage). Seeds per channel via the existing `list_channel_messages(since)` IPC when channels materialize; counts live events gated by cursor comparison; ID-set dedupes backfill re-emission (live + backfill share one unflagged event). `NavService.setUnread` mirrors the mention rollup (community = Σ children).

**Tech Stack:** Svelte 5 (runes), TypeScript, vitest + @testing-library/svelte. Frontend-only — no Rust changes.

**Spec:** `docs/specs/2026-07-09-zeb-665-per-channel-unread-design.md` (approved 2026-07-09). Two deviations discovered during planning, both narrowing:
1. The spec's "three existing clearMention call sites" is TWO for community channels — `App.svelte:1147` (selection-resolution effect) and `App.svelte:1229` (`openCommunityChannel`). The third (`App.svelte:2977`) is the DM/legacy-node path, out of scope per §2.
2. `onCommunityRemoved` ships on the service (unit-tested) but has no App wiring: no community-removal hook exists in App today; stale session sets are bounded memory and badges vanish with the nav nodes. Noted for the cross-device follow-up.

## Global Constraints

- Badge display cap: exactly `count > 99 ? '99+' : count`. Internal set cap: 100 (`UNREAD_TRACK_CAP`).
- Cursor storage key: `harmony-unread:owner-<ownerId>`; map key inside the blob: `` `${communityId}:${channelId}` ``. Pre-owner: reads return `null`, writes no-op (ZEB-586 pattern).
- Unread predicate: `hlcNewer(msg.at, cursor)` AND `msg.author !== self` AND NOT (focused AND active channel).
- Start-clean stamp / clear stamp: `{ wallMs: now(), logical: 0, deviceId: '' }`; clear stamps `max(cursor, maxSeen, stamp)` by `compareHlc`.
- Error extraction idiom everywhere: `e instanceof Error ? e.message : String(e)`.
- Seed IPC: raw `invoke('list_channel_messages', { communityId, channelId, since, limit })` — NEVER `channelMessageService.listMessages` (it ingests into the feed cache and re-fires `onMessage`).
- Levels: channel `standard`/`none`; community rollup `quiet`/`none`. Mention plumbing untouched.
- Gates per task: scoped `npx vitest run <files>`; final: `npx tsc --noEmit && npx vitest run` (full).
- Commit per task on branch `zeb-665-per-channel-unread`; trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`.

---

### Task 1: Shared HLC helpers (`src/lib/hlc.ts`) + deduplicate the two private copies

**Files:**
- Create: `src/lib/hlc.ts`, `src/lib/hlc.test.ts`
- Modify: `src/lib/channel-message-service.ts:709-713` (delete private `compareHlc`, import), `src/lib/fork-timeline.ts:127-132` (same)

**Interfaces:**
- Consumes: `Hlc` type shape `{ wallMs: number; logical: number; deviceId: string }` (structural — `HlcDto` in both modules matches).
- Produces: `compareHlc(a: HlcLike, b: HlcLike): number` (negative/0/positive; wallMs → logical → deviceId lexical) and `hlcNewer(a: HlcLike, b: HlcLike): boolean` (strict `compareHlc(a,b) > 0`). Tasks 4-6 import these.

- [ ] **Step 1: Write the failing test** — `src/lib/hlc.test.ts`:

```ts
import { describe, it, expect } from 'vitest';
import { compareHlc, hlcNewer } from './hlc';

const h = (wallMs: number, logical = 0, deviceId = 'a') => ({ wallMs, logical, deviceId });

describe('compareHlc', () => {
  it('orders by wallMs first', () => {
    expect(compareHlc(h(1), h(2))).toBeLessThan(0);
    expect(compareHlc(h(2), h(1))).toBeGreaterThan(0);
  });
  it('breaks wallMs ties by logical', () => {
    expect(compareHlc(h(1, 0), h(1, 1))).toBeLessThan(0);
    expect(compareHlc(h(1, 2), h(1, 1))).toBeGreaterThan(0);
  });
  it('breaks (wallMs, logical) ties by deviceId lexical', () => {
    expect(compareHlc(h(1, 1, 'a'), h(1, 1, 'b'))).toBeLessThan(0);
    expect(compareHlc(h(1, 1, 'b'), h(1, 1, 'a'))).toBeGreaterThan(0);
  });
  it('returns 0 for identical HLCs', () => {
    expect(compareHlc(h(1, 1, 'a'), h(1, 1, 'a'))).toBe(0);
  });
});

describe('hlcNewer', () => {
  it('is strict: true only when a > b', () => {
    expect(hlcNewer(h(2), h(1))).toBe(true);
    expect(hlcNewer(h(1), h(1))).toBe(false);
    expect(hlcNewer(h(1), h(2))).toBe(false);
  });
});
```

- [ ] **Step 2: Run to verify it fails** — `npx vitest run src/lib/hlc.test.ts` → FAIL (module not found).

- [ ] **Step 3: Implement** — `src/lib/hlc.ts`:

```ts
/**
 * ZEB-665: shared HLC comparison. Extracted from the two identical private
 * copies in channel-message-service.ts and fork-timeline.ts (which now import
 * it). Matches the backend's canonical (wall_ms, logical, device_id) order —
 * wallMs alone is NOT a valid cursor (the ZEB-244 lesson).
 */
export interface HlcLike {
  wallMs: number;
  logical: number;
  deviceId: string;
}

/** wallMs → logical → deviceId lexical. Returns negative/0/positive. */
export function compareHlc(a: HlcLike, b: HlcLike): number {
  if (a.wallMs !== b.wallMs) return a.wallMs - b.wallMs;
  if (a.logical !== b.logical) return a.logical - b.logical;
  return a.deviceId < b.deviceId ? -1 : a.deviceId > b.deviceId ? 1 : 0;
}

/** Strictly newer: a > b. Mirrors the backend's `is_strictly_newer_than`. */
export function hlcNewer(a: HlcLike, b: HlcLike): boolean {
  return compareHlc(a, b) > 0;
}
```

- [ ] **Step 4: Refactor the duplicates.** In `src/lib/channel-message-service.ts`: add `import { compareHlc } from './hlc';` to the imports, delete the private `compareHlc` function (lines 709-713). In `src/lib/fork-timeline.ts`: add the same import, delete its private copy (lines 127-132, keep the doc comment context sensible — delete comment line 127 too).

- [ ] **Step 5: Run tests** — `npx vitest run src/lib/hlc.test.ts src/lib/channel-message-service.test.ts src/lib/fork-timeline.test.ts` → PASS (behavior identical). `npx tsc --noEmit` → clean.

- [ ] **Step 6: Commit** — `git add src/lib/hlc.ts src/lib/hlc.test.ts src/lib/channel-message-service.ts src/lib/fork-timeline.ts && git commit -m "ZEB-665: extract shared compareHlc/hlcNewer into src/lib/hlc.ts"` (+ trailers).

---

### Task 2: `NavService.setUnread` + community rollup (incl. `setChannels`)

**Files:**
- Modify: `src/lib/nav-service.ts` (new method after `clearMention` ~line 536; rollup line in `setChannels` after line 469)
- Test: `src/lib/nav-service.test.ts` (append describe block)

**Interfaces:**
- Consumes: existing `NavNode.unreadCount/unreadLevel`, `communityIdOf(node)` private helper, `onChange` callback.
- Produces: `setUnread(channelId: string, count: number): void` — absolute set (NOT delta); missing node → silent no-op; channel level `standard`/`none`; owning community `unreadCount` = Σ children, level `quiet`/`none`. Task 5's `deps.setUnread` and Task 6's wiring rely on this exact signature.

- [ ] **Step 1: Write failing tests** — append to `src/lib/nav-service.test.ts`:

```ts
describe('NavService — per-channel unread (ZEB-665)', () => {
  function seedCommunityWithChannels(svc: NavService) {
    svc.syncFromBackend([
      { id: 'c1', parent_id: null, kind: 'community', name: 'Crew' },
    ]);
    svc.setChannels('c1', [
      { channelId: 'ch1', name: 'general', writePower: 0, kind: 'text', createdAt: HLC },
      { channelId: 'ch2', name: 'random', writePower: 0, kind: 'text', createdAt: HLC },
    ]);
  }

  it('setUnread sets count + standard level and rolls up a quiet community dot', () => {
    const svc = new NavService();
    seedCommunityWithChannels(svc);
    svc.setUnread('ch1', 3);
    const ch1 = svc.nodes.find((n) => n.id === 'ch1')!;
    const c1 = svc.nodes.find((n) => n.id === 'c1')!;
    expect(ch1.unreadCount).toBe(3);
    expect(ch1.unreadLevel).toBe('standard');
    expect(c1.unreadCount).toBe(3);
    expect(c1.unreadLevel).toBe('quiet');
  });

  it('setUnread(0) clears the level and the rollup follows the sum', () => {
    const svc = new NavService();
    seedCommunityWithChannels(svc);
    svc.setUnread('ch1', 2);
    svc.setUnread('ch2', 5);
    svc.setUnread('ch1', 0);
    const ch1 = svc.nodes.find((n) => n.id === 'ch1')!;
    const c1 = svc.nodes.find((n) => n.id === 'c1')!;
    expect(ch1.unreadLevel).toBe('none');
    expect(c1.unreadCount).toBe(5);
    expect(c1.unreadLevel).toBe('quiet');
    svc.setUnread('ch2', 0);
    expect(c1.unreadCount).toBe(0);
    expect(c1.unreadLevel).toBe('none');
  });

  it('setUnread on a missing node is a silent no-op', () => {
    const svc = new NavService();
    seedCommunityWithChannels(svc);
    expect(() => svc.setUnread('nope', 4)).not.toThrow();
  });

  it('setUnread notifies onChange once per effective change and not on no-ops', () => {
    const svc = new NavService();
    seedCommunityWithChannels(svc);
    let calls = 0;
    svc.onChange = () => calls++;
    svc.setUnread('ch1', 3);
    expect(calls).toBe(1);
    svc.setUnread('ch1', 3); // same value — no-op
    expect(calls).toBe(1);
  });

  it('setChannels preserves per-channel unread and recomputes the rollup on removal', () => {
    const svc = new NavService();
    seedCommunityWithChannels(svc);
    svc.setUnread('ch1', 2);
    svc.setUnread('ch2', 7);
    svc.setChannels('c1', [
      { channelId: 'ch1', name: 'general', writePower: 0, kind: 'text', createdAt: HLC },
    ]); // ch2 removed
    const ch1 = svc.nodes.find((n) => n.id === 'ch1')!;
    const c1 = svc.nodes.find((n) => n.id === 'c1')!;
    expect(ch1.unreadCount).toBe(2);
    expect(c1.unreadCount).toBe(2); // rollup follows survivors, ch2's 7 gone
    expect(c1.unreadLevel).toBe('quiet');
  });
});
```

(Reuse the existing test file's `HLC` fixture constant and `syncFromBackend` seeding idiom — match the surrounding describe blocks' exact setup helpers if names differ; the file already builds community+channel trees for the mention tests.)

- [ ] **Step 2: Run to verify failure** — `npx vitest run src/lib/nav-service.test.ts` → FAIL (`setUnread is not a function`).

- [ ] **Step 3: Implement.** In `src/lib/nav-service.ts`, after `clearMention` (line ~536):

```ts
  /** ZEB-665: absolute per-channel unread count (from ChannelUnreadService's
   *  capped ID-set — a recomputable projection, so no boot-race queue like
   *  mentions: the service re-pushes when channels materialize). Rolls the
   *  owning community up to Σ(children) with a `quiet` dot (numbers on
   *  channels, dot on the community — deliberate, mirrors messenger idiom). */
  setUnread(channelId: string, count: number): void {
    const node = this.nodes.find((n) => n.id === channelId && n.type === 'channel');
    if (!node) return;
    const next = Math.max(0, count);
    const nextLevel = next > 0 ? 'standard' : 'none';
    if (node.unreadCount === next && node.unreadLevel === nextLevel) return;
    node.unreadCount = next;
    node.unreadLevel = nextLevel;
    const cid = this.communityIdOf(node);
    if (cid) this.rollUpCommunityUnread(cid);
    this.onChange?.();
  }

  /** ZEB-665: community unread = Σ(channel children); level quiet/none. */
  private rollUpCommunityUnread(communityId: string): void {
    const comm = this.nodes.find((n) => n.id === communityId && n.type === 'community');
    if (!comm) return;
    const sum = this.nodes
      .filter((n) => n.parentId === communityId && n.type === 'channel')
      .reduce((acc, c) => acc + c.unreadCount, 0);
    comm.unreadCount = sum;
    comm.unreadLevel = sum > 0 ? 'quiet' : 'none';
  }
```

And in `setChannels`, right after the mention-sum line (469):

```ts
    // ZEB-665: same sum invariant for unread (removal drops a child's count).
    this.rollUpCommunityUnread(communityId);
```

- [ ] **Step 4: Run tests** — `npx vitest run src/lib/nav-service.test.ts` → PASS (all, including existing mention suites).

- [ ] **Step 5: Commit** — `git add src/lib/nav-service.ts src/lib/nav-service.test.ts && git commit -m "ZEB-665: NavService.setUnread + community quiet-dot rollup"` (+ trailers).

---

### Task 3: NavNodeRow "99+" display cap

**Files:**
- Modify: `src/lib/components/NavNodeRow.svelte:246-252` (unread badge block)
- Test: `src/lib/components/__tests__/NavNodeRow.test.ts` (append)

**Interfaces:** consumes `NavNode.unreadCount/unreadLevel` only. No new props.

- [ ] **Step 1: Failing tests** — append to `NavNodeRow.test.ts`:

```ts
describe('NavNodeRow — unread badge display cap (ZEB-665)', () => {
  it('renders "99+" when unreadCount exceeds 99', () => {
    render(NavNodeRow, {
      props: {
        node: makeNode({ unreadCount: 100, unreadLevel: 'standard' }),
        colorAncestry: [], displayMode: 'text', isLastChild: false,
      },
    });
    expect(screen.getByText('99+')).toBeTruthy();
  });
  it('renders the exact count at or below 99', () => {
    render(NavNodeRow, {
      props: {
        node: makeNode({ unreadCount: 99, unreadLevel: 'standard' }),
        colorAncestry: [], displayMode: 'text', isLastChild: false,
      },
    });
    expect(screen.getByText('99')).toBeTruthy();
  });
  it('community quiet level renders the dot, not a number', () => {
    const { container } = render(NavNodeRow, {
      props: {
        node: makeNode({ type: 'community', name: 'Crew', unreadCount: 12, unreadLevel: 'quiet' }),
        colorAncestry: [], displayMode: 'text', isLastChild: false,
      },
    });
    expect(container.querySelector('.unread-dot')).toBeTruthy();
    expect(container.querySelector('.unread-badge')).toBeNull();
  });
});
```

- [ ] **Step 2: Verify failure** — `npx vitest run src/lib/components/__tests__/NavNodeRow.test.ts` → the "99+" test FAILS (renders `100`); quiet-dot test may already pass (existing render path) — keep it as a pin.

- [ ] **Step 3: Implement.** In `NavNodeRow.svelte`, replace the two numeric badge interpolations:

```svelte
    {#if node.unreadLevel === 'standard' && node.unreadCount > 0}
      <span class="unread-badge">{node.unreadCount > 99 ? '99+' : node.unreadCount}</span>
    {:else if node.unreadLevel === 'loud' && node.unreadCount > 0}
      <span class="unread-badge loud">{node.unreadCount > 99 ? '99+' : node.unreadCount}</span>
    {:else if node.unreadLevel === 'quiet' && node.unreadCount > 0}
      <span class="unread-dot"></span>
    {/if}
```

- [ ] **Step 4: Run** — `npx vitest run src/lib/components/__tests__/NavNodeRow.test.ts` → PASS.

- [ ] **Step 5: Commit** — `git add src/lib/components/NavNodeRow.svelte src/lib/components/__tests__/NavNodeRow.test.ts && git commit -m "ZEB-665: cap unread badge display at 99+"` (+ trailers).

---

### Task 4: `UnreadCursorStore` (owner-scoped localStorage)

**Files:**
- Create: `src/lib/unread-cursor-store.ts`, `src/lib/unread-cursor-store.test.ts`

**Interfaces:**
- Consumes: `Hlc` from `./types`; `localStorage` (jsdom in tests).
- Produces (Task 5 + 6 depend on these exact names):

```ts
export interface UnreadCursorStore {
  connectOwner(ownerId: string): void;
  get(communityId: string, channelId: string): Hlc | null;
  set(communityId: string, channelId: string, hlc: Hlc): void;
}
export class LocalStorageUnreadCursorStore implements UnreadCursorStore { ... }
```

- [ ] **Step 1: Failing tests** — `src/lib/unread-cursor-store.test.ts`:

```ts
import { describe, it, expect, beforeEach } from 'vitest';
import { LocalStorageUnreadCursorStore } from './unread-cursor-store';

const HLC = { wallMs: 100, logical: 1, deviceId: 'd1' };

describe('LocalStorageUnreadCursorStore (ZEB-665)', () => {
  beforeEach(() => localStorage.clear());

  it('pre-owner: get returns null and set is a no-op (ZEB-586 guard)', () => {
    const s = new LocalStorageUnreadCursorStore();
    s.set('c1', 'ch1', HLC);
    expect(s.get('c1', 'ch1')).toBeNull();
    expect(localStorage.length).toBe(0); // nothing leaked to a shared key
  });

  it('round-trips a cursor after connectOwner, keyed per owner', () => {
    const s = new LocalStorageUnreadCursorStore();
    s.connectOwner('owner-a');
    s.set('c1', 'ch1', HLC);
    expect(s.get('c1', 'ch1')).toEqual(HLC);
    expect(localStorage.getItem('harmony-unread:owner-owner-a')).toContain('"c1:ch1"');
  });

  it('isolates owners: owner B does not see owner A cursors', () => {
    const s = new LocalStorageUnreadCursorStore();
    s.connectOwner('owner-a');
    s.set('c1', 'ch1', HLC);
    s.connectOwner('owner-b');
    expect(s.get('c1', 'ch1')).toBeNull();
  });

  it('persists across instances for the same owner', () => {
    const a = new LocalStorageUnreadCursorStore();
    a.connectOwner('owner-a');
    a.set('c1', 'ch1', HLC);
    const b = new LocalStorageUnreadCursorStore();
    b.connectOwner('owner-a');
    expect(b.get('c1', 'ch1')).toEqual(HLC);
  });

  it('degrades a corrupt blob to an empty map instead of throwing', () => {
    localStorage.setItem('harmony-unread:owner-owner-a', '{not json');
    const s = new LocalStorageUnreadCursorStore();
    expect(() => s.connectOwner('owner-a')).not.toThrow();
    expect(s.get('c1', 'ch1')).toBeNull();
    s.set('c1', 'ch1', HLC); // and recovers on next write
    expect(s.get('c1', 'ch1')).toEqual(HLC);
  });
});
```

- [ ] **Step 2: Verify failure** — `npx vitest run src/lib/unread-cursor-store.test.ts` → FAIL (module not found).

- [ ] **Step 3: Implement** — `src/lib/unread-cursor-store.ts`:

```ts
/**
 * ZEB-665: owner-scoped persistence for per-channel read cursors ("newest-seen
 * message HLC"). localStorage in v1; the interface exists so the cross-device
 * OwnerState-CRDT swap (spec §8.1) is a storage-layer change only.
 *
 * Owner scoping follows the theme/profile-service pattern (the ZEB-586/589
 * fix): WebView localStorage is bundle-scoped, not identity-scoped, so a fixed
 * key would leak read state across owners on one machine. Before connectOwner,
 * reads return null and writes no-op.
 */
import type { Hlc } from './types';

const KEY_PREFIX = 'harmony-unread';

export interface UnreadCursorStore {
  connectOwner(ownerId: string): void;
  get(communityId: string, channelId: string): Hlc | null;
  set(communityId: string, channelId: string, hlc: Hlc): void;
}

export class LocalStorageUnreadCursorStore implements UnreadCursorStore {
  private ownerId: string | null = null;
  private map = new Map<string, Hlc>();

  connectOwner(ownerId: string): void {
    this.ownerId = ownerId;
    this.map = new Map();
    try {
      const raw = localStorage.getItem(this.key());
      if (raw) {
        const parsed = JSON.parse(raw) as Record<string, Hlc>;
        for (const [k, v] of Object.entries(parsed)) this.map.set(k, v);
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      console.warn('[unread-cursor-store] corrupt blob, starting clean:', msg);
      this.map = new Map();
    }
  }

  get(communityId: string, channelId: string): Hlc | null {
    if (this.ownerId === null) return null;
    return this.map.get(`${communityId}:${channelId}`) ?? null;
  }

  set(communityId: string, channelId: string, hlc: Hlc): void {
    if (this.ownerId === null) return; // pre-identity: never write a shared key
    this.map.set(`${communityId}:${channelId}`, hlc);
    try {
      localStorage.setItem(this.key(), JSON.stringify(Object.fromEntries(this.map)));
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      console.warn('[unread-cursor-store] persist failed:', msg);
    }
  }

  private key(): string {
    return `${KEY_PREFIX}:owner-${this.ownerId}`;
  }
}
```

- [ ] **Step 4: Run** — `npx vitest run src/lib/unread-cursor-store.test.ts` → PASS.

- [ ] **Step 5: Commit** — `git add src/lib/unread-cursor-store.ts src/lib/unread-cursor-store.test.ts && git commit -m "ZEB-665: owner-scoped UnreadCursorStore (localStorage v1)"` (+ trailers).

---

### Task 5: `ChannelUnreadService`

**Files:**
- Create: `src/lib/channel-unread-service.ts`, `src/lib/channel-unread-service.test.ts`

**Interfaces:**
- Consumes: `compareHlc`, `hlcNewer` (Task 1); `UnreadCursorStore` (Task 4); `ChannelMessageDto` type from `./channel-message-service`; `ChannelInfo` type from `./community-service`; `Hlc` from `./types`.
- Produces (Task 6 wiring depends on these exact names):

```ts
export const UNREAD_TRACK_CAP = 100;
export interface ChannelUnreadDeps {
  listMessagesSince(communityId: string, channelId: string,
                    since: Hlc, limit: number): Promise<ChannelMessageDto[]>;
  setUnread(channelId: string, count: number): void;
  isActiveChannel(communityId: string, channelId: string): boolean;
  isFocused(): boolean;
  selfOwnerId(): string | null;
  storage: UnreadCursorStore;
  now(): number;
}
export class ChannelUnreadService {
  constructor(deps: ChannelUnreadDeps);
  connectOwner(ownerId: string): void;                 // store + full re-seed
  onChannelsMaterialized(communityId: string, channels: ChannelInfo[]): Promise<void>;
  onMessage(communityId: string, channelId: string, message: ChannelMessageDto): void;
  markChannelRead(communityId: string, channelId: string): void;
  onCommunityRemoved(communityId: string): void;
}
```

Behavior contract (from spec §5, implement exactly):

- **`onChannelsMaterialized`** remembers `channels` per community (for owner-connect re-seed). For each channel not yet seeded this session: mark seeded, then — cursor exists → `listMessagesSince(cursor, UNREAD_TRACK_CAP)`, drop self-authored, union IDs into the channel's set (event-race union), update `maxSeen` from every returned message; no cursor → `storage.set(startClean())` only. Push the count for every channel in the list (nav nodes may have just been rebuilt). Seed failure: un-mark seeded, `console.warn('[channel-unread] seed failed for <communityId>:<channelId>:', msg)`, count stays as-is.
- **`onMessage`** (synchronous): always update `maxSeen` (max by `compareHlc`). Self-authored → return. Focused AND active → cursor ← max(cursor, msg.at) persisted, wipe set, push 0. Else: cursor missing → return (start-clean covers at materialize); `hlcNewer(msg.at, cursor)` → add ID to set iff `set.size < UNREAD_TRACK_CAP || set.has(id)`, push on size change.
- **`markChannelRead`**: cursor ← max by `compareHlc` of (existing cursor, `maxSeen`, `startClean()` stamp); `storage.set`; wipe set; push 0. The stamp component enforces open-clears-all under seed overflow (seed saw only the OLDEST 100).
- **`connectOwner`**: `storage.connectOwner(ownerId)`; clear all seeded marks + sets + maxSeen; re-run `onChannelsMaterialized` for every remembered community (fire-and-forget with catch-warn).
- **`onCommunityRemoved`**: drop remembered channels, sets, maxSeen, seeded marks for that community's channels.
- `startClean()` = `{ wallMs: this.deps.now(), logical: 0, deviceId: '' }`.
- push = `deps.setUnread(channelId, set?.size ?? 0)`.

- [ ] **Step 1: Failing tests** — `src/lib/channel-unread-service.test.ts`:

```ts
import { describe, it, expect, vi } from 'vitest';
import { ChannelUnreadService, UNREAD_TRACK_CAP, type ChannelUnreadDeps } from './channel-unread-service';
import type { ChannelMessageDto } from './channel-message-service';
import type { ChannelInfo } from './community-service';
import type { Hlc } from './types';
import type { UnreadCursorStore } from './unread-cursor-store';

const hlc = (wallMs: number, logical = 0, deviceId = 'peer'): Hlc => ({ wallMs, logical, deviceId });
const msg = (id: string, at: Hlc, author = 'peer-1'): ChannelMessageDto =>
  ({ messageId: id, communityId: 'c1', channelId: 'ch1', author, at, body: [] }) as ChannelMessageDto;
const ch = (id: string, name = id): ChannelInfo =>
  ({ channelId: id, name, writePower: 0, kind: 'text', createdAt: hlc(0) }) as ChannelInfo;

class MemStore implements UnreadCursorStore {
  owner: string | null = null;
  map = new Map<string, Hlc>();
  connectOwner(o: string) { this.owner = o; }
  get(c: string, chId: string) { return this.owner ? (this.map.get(`${c}:${chId}`) ?? null) : null; }
  set(c: string, chId: string, h: Hlc) { if (this.owner) this.map.set(`${c}:${chId}`, h); }
}

function harness(over: Partial<ChannelUnreadDeps> = {}) {
  const store = new MemStore();
  store.connectOwner('me');
  const pushes: Array<[string, number]> = [];
  const deps: ChannelUnreadDeps = {
    listMessagesSince: vi.fn(async () => []),
    setUnread: (chId, n) => pushes.push([chId, n]),
    isActiveChannel: () => false,
    isFocused: () => true,
    selfOwnerId: () => 'me',
    storage: store,
    now: () => 5000,
    ...over,
  };
  return { svc: new ChannelUnreadService(deps), deps, store, pushes };
}
const lastCount = (pushes: Array<[string, number]>, chId: string) =>
  [...pushes].reverse().find(([id]) => id === chId)?.[1];

describe('ChannelUnreadService (ZEB-665)', () => {
  it('start-clean: no stored cursor → stamps now() and pushes 0, no IPC', async () => {
    const { svc, deps, store, pushes } = harness();
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    expect(store.get('c1', 'ch1')).toEqual({ wallMs: 5000, logical: 0, deviceId: '' });
    expect(deps.listMessagesSince).not.toHaveBeenCalled();
    expect(lastCount(pushes, 'ch1')).toBe(0);
  });

  it('seed with stored cursor counts strictly-newer non-self messages', async () => {
    const { svc, store, pushes } = harness({
      listMessagesSince: async () => [msg('m1', hlc(200)), msg('m2', hlc(300)), msg('mine', hlc(400), 'me')],
    });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    expect(lastCount(pushes, 'ch1')).toBe(2); // self-authored dropped
  });

  it('seed overflow caps at UNREAD_TRACK_CAP', async () => {
    const many = Array.from({ length: UNREAD_TRACK_CAP }, (_, i) => msg(`m${i}`, hlc(200 + i)));
    const { svc, store, pushes } = harness({ listMessagesSince: async () => many });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    expect(lastCount(pushes, 'ch1')).toBe(UNREAD_TRACK_CAP);
  });

  it('live message for a non-active channel counts once (backfill re-emission dedupes)', async () => {
    const { svc, store, pushes } = harness();
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.onMessage('c1', 'ch1', msg('m1', hlc(200)));
    svc.onMessage('c1', 'ch1', msg('m1', hlc(200))); // re-emitted by backfill
    expect(lastCount(pushes, 'ch1')).toBe(1);
  });

  it('messages at or before the cursor never count (history replay)', async () => {
    const { svc, store, pushes } = harness();
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.onMessage('c1', 'ch1', msg('old', hlc(50)));
    svc.onMessage('c1', 'ch1', msg('at-cursor', hlc(100)));
    expect(lastCount(pushes, 'ch1')).toBe(0);
  });

  it('self-authored messages never count', async () => {
    const { svc, store, pushes } = harness();
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.onMessage('c1', 'ch1', msg('mine', hlc(200), 'me'));
    expect(lastCount(pushes, 'ch1')).toBe(0);
  });

  it('focused + active channel advances the cursor instead of counting', async () => {
    const { svc, store, pushes } = harness({ isActiveChannel: () => true, isFocused: () => true });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.onMessage('c1', 'ch1', msg('m1', hlc(200)));
    expect(lastCount(pushes, 'ch1')).toBe(0);
    expect(store.get('c1', 'ch1')).toEqual(hlc(200));
  });

  it('unfocused + active channel still counts (mirrors mention semantics)', async () => {
    const { svc, store, pushes } = harness({ isActiveChannel: () => true, isFocused: () => false });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.onMessage('c1', 'ch1', msg('m1', hlc(200)));
    expect(lastCount(pushes, 'ch1')).toBe(1);
  });

  it('markChannelRead wipes the set, pushes 0, and stamps past maxSeen', async () => {
    const { svc, store, pushes } = harness();
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.onMessage('c1', 'ch1', msg('m1', hlc(9000))); // beyond the now() stamp
    svc.markChannelRead('c1', 'ch1');
    expect(lastCount(pushes, 'ch1')).toBe(0);
    expect(store.get('c1', 'ch1')).toEqual(hlc(9000)); // maxSeen wins over now()=5000
    svc.onMessage('c1', 'ch1', msg('m1', hlc(9000))); // replayed after read
    expect(lastCount(pushes, 'ch1')).toBe(0);
  });

  it('markChannelRead under seed-overflow stamps at least now() (open-clears-all)', async () => {
    const many = Array.from({ length: UNREAD_TRACK_CAP }, (_, i) => msg(`m${i}`, hlc(200 + i)));
    const { svc, store } = harness({ listMessagesSince: async () => many });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.markChannelRead('c1', 'ch1');
    const cur = store.get('c1', 'ch1')!;
    expect(cur.wallMs).toBeGreaterThanOrEqual(5000); // ≥ now(), not the oldest-100 tail
  });

  it('event racing the seed unions into one count (no double-count)', async () => {
    let resolveList!: (v: ChannelMessageDto[]) => void;
    const { svc, store, pushes } = harness({
      listMessagesSince: () => new Promise((r) => { resolveList = r; }),
    });
    store.set('c1', 'ch1', hlc(100));
    const seeding = svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.onMessage('c1', 'ch1', msg('m1', hlc(200))); // arrives mid-seed
    resolveList([msg('m1', hlc(200)), msg('m2', hlc(300))]);
    await seeding;
    expect(lastCount(pushes, 'ch1')).toBe(2); // m1 counted once
  });

  it('unseeded channel ignores events (start-clean will cover it)', () => {
    const { svc, pushes } = harness();
    svc.onMessage('c1', 'ch-unknown', msg('m1', hlc(200)));
    expect(lastCount(pushes, 'ch-unknown')).toBeUndefined();
  });

  it('seed failure warns, stays at 0, and is retried on next materialize', async () => {
    const listMessagesSince = vi.fn()
      .mockRejectedValueOnce(new Error('boom'))
      .mockResolvedValueOnce([msg('m1', hlc(200))]);
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const { svc, store, pushes } = harness({ listMessagesSince });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    expect(warn).toHaveBeenCalled();
    await svc.onChannelsMaterialized('c1', [ch('ch1')]); // retry succeeds
    expect(lastCount(pushes, 'ch1')).toBe(1);
    warn.mockRestore();
  });

  it('re-materialize does not re-seed but re-pushes known counts', async () => {
    const listMessagesSince = vi.fn(async () => [msg('m1', hlc(200))]);
    const { svc, store, pushes } = harness({ listMessagesSince });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    pushes.length = 0;
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    expect(listMessagesSince).toHaveBeenCalledTimes(1); // no second IPC
    expect(lastCount(pushes, 'ch1')).toBe(1);           // but count re-pushed
  });

  it('connectOwner re-seeds remembered communities under the new owner', async () => {
    const listMessagesSince = vi.fn(async () => [msg('m1', hlc(200))]);
    const { svc, store } = harness({ listMessagesSince });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    expect(listMessagesSince).toHaveBeenCalledTimes(1);
    svc.connectOwner('me');
    await vi.waitFor(() => expect(listMessagesSince).toHaveBeenCalledTimes(2));
  });

  it('onCommunityRemoved drops session state so a later re-add re-seeds', async () => {
    const listMessagesSince = vi.fn(async () => [msg('m1', hlc(200))]);
    const { svc, store } = harness({ listMessagesSince });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.onCommunityRemoved('c1');
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    expect(listMessagesSince).toHaveBeenCalledTimes(2);
  });

  it('per-community isolation: counts on c1 do not leak to c2 channels', async () => {
    const { svc, store, pushes } = harness();
    store.set('c1', 'ch1', hlc(100));
    store.set('c2', 'chX', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    await svc.onChannelsMaterialized('c2', [ch('chX')]);
    svc.onMessage('c1', 'ch1', msg('m1', hlc(200)));
    expect(lastCount(pushes, 'ch1')).toBe(1);
    expect(lastCount(pushes, 'chX')).toBe(0);
  });
});
```

- [ ] **Step 2: Verify failure** — `npx vitest run src/lib/channel-unread-service.test.ts` → FAIL (module not found).

- [ ] **Step 3: Implement** — `src/lib/channel-unread-service.ts` per the behavior contract above (single class, ~170 lines; private helpers `seedChannel`, `push`, `startClean`, `advanceCursor(communityId, channelId, hlc)` = set-if-newer via `compareHlc`; state maps `sets/maxSeen/seeded` keyed `` `${communityId}:${channelId}` `` to match store keys, `channelsByCommunity: Map<string, ChannelInfo[]>`). Every await wrapped: `catch (e) { const msg = e instanceof Error ? e.message : String(e); console.warn('[channel-unread] seed failed for ...', msg); }`.

- [ ] **Step 4: Run** — `npx vitest run src/lib/channel-unread-service.test.ts` → PASS (17 tests).

- [ ] **Step 5: Commit** — `git add src/lib/channel-unread-service.ts src/lib/channel-unread-service.test.ts && git commit -m "ZEB-665: ChannelUnreadService — capped ID-set unread over persisted HLC cursor"` (+ trailers).

---

### Task 6: App wiring + full gates + PR

**Files:**
- Modify: `src/App.svelte` — five surgical touches (below).

**Interfaces:** consumes everything above; produces no new exports.

- [ ] **Step 1: Declare + construct.** Near the `mentionAlerter` declaration (`App.svelte:267`):

```ts
  let channelUnread: import('./lib/channel-unread-service').ChannelUnreadService | null = null;
```

Inside the Tauri-init IIFE, next to the mention-alerter block (~line 2332), construct it (dynamic imports match the file's lazy-service idiom):

```ts
      // ── ZEB-665: per-channel unread counts. Seeds via list_channel_messages
      // directly (NOT channelMessageService.listMessages — that would ingest
      // into the feed cache and re-fire onMessage). Self-gates on cursor,
      // author, and focused-active; NavService renders the badges.
      try {
        const { ChannelUnreadService } = await import('./lib/channel-unread-service');
        const { LocalStorageUnreadCursorStore } = await import('./lib/unread-cursor-store');
        channelUnread = new ChannelUnreadService({
          listMessagesSince: (communityId, channelId, since, limit) =>
            invoke('list_channel_messages', { communityId, channelId, since, limit }) as Promise<
              import('./lib/channel-message-service').ChannelMessageDto[]
            >,
          setUnread: (channelId, count) => navService.setUnread(channelId, count),
          isActiveChannel: (communityId, channelId) =>
            appMode === 'messages' &&
            selectedCommunityId === communityId &&
            communityActiveView === 'channels' &&
            communityService.getSelectedChannel(communityId) === channelId,
          isFocused: () => document.hasFocus(),
          selfOwnerId: () => selfOwnerId ?? null,
          storage: new LocalStorageUnreadCursorStore(),
          now: () => Date.now(),
        });
        if (selfOwnerId) channelUnread.connectOwner(selfOwnerId);
        fileManagerService.addUnlisten(() => { channelUnread = null; });
      } catch (e) {
        console.warn('[harmony-client] channel-unread init failed:', e instanceof Error ? e.message : String(e));
      }
```

- [ ] **Step 2: Owner connect.** Next to the existing reactive owner plumbing (near `myProfile` sync effects), add:

```ts
  // ZEB-665: (re)connect the unread cursor store when the owner identity lands
  // (or changes) — pre-identity the store no-ops, and channels materialized
  // before this point get re-seeded by connectOwner.
  $effect(() => {
    const oid = selfOwnerId;
    if (oid) channelUnread?.connectOwner(oid);
  });
```

- [ ] **Step 3: Channels-materialize chain.** In the `ChannelNavSyncService` construction (`App.svelte:1443-1448`), chain the unread hook:

```ts
    setChannels: (id, channels) => {
      navService.setChannels(id, channels);
      void channelUnread?.onChannelsMaterialized(id, channels);
    },
```

- [ ] **Step 4: Live hook + read marks.** In `channelMessageService.onMessage` (~1878), after the mention-alerter call:

```ts
    channelUnread?.onMessage(communityId, channelId, message);
```

At the two community-channel clear sites, beside `navService.clearMention`:
- `App.svelte:1147` (resolution `$effect`): `channelUnread?.markChannelRead(cid, target);`
- `App.svelte:1229` (`openCommunityChannel`): `channelUnread?.markChannelRead(communityId, channelId);`

(Do NOT touch `App.svelte:2977` — that's the DM/legacy path, out of scope.)

- [ ] **Step 5: Full gates** — `npx tsc --noEmit && npx vitest run` → clean + all tests pass.

- [ ] **Step 6: Commit + push + PR** —

```bash
git add src/App.svelte
git commit -m "ZEB-665: wire ChannelUnreadService (seed on materialize, live hook, read marks)"
git push -u origin zeb-665-per-channel-unread
gh pr create --repo zeblithic/harmony-client --title "ZEB-665: per-channel unread counts (local-first read cursors + nav badges)" --body "<summary per convention>"
```

Then fire `@coderabbitai review` once, converge with bots + CI.

---

## Self-review (writing-plans checklist)

1. **Spec coverage:** §4 data model → Tasks 4+5; §5 hlc.ts → Task 1, service → Task 5, NavService → Task 2, NavNodeRow → Task 3, App → Task 6; §6 caveats encoded in Task 5 tests (overflow stamp, skew accepted); §7 test list → all present. Spec's "three clearMention sites" and "onCommunityRemoved beside existing removal handling" corrected in header (narrowing deviations, documented).
2. **Placeholder scan:** Task 5 Step 3 describes the class by behavior contract + exact state shapes rather than full listing — the contract block at the task top carries exact signatures, state keys, and every branch; acceptable because the tests in Step 1 pin all behavior. No TBDs.
3. **Type consistency:** `setUnread(channelId, count)` consistent across Tasks 2/5/6; `UnreadCursorStore` names consistent across 4/5/6; `listMessagesSince(communityId, channelId, since, limit)` consistent across 5/6 (`since` non-optional — seeds only run with a cursor; start-clean never calls it).
