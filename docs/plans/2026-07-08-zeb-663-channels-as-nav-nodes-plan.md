# Community Channels as First-Class NavNodes (ZEB-663) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make community channels first-class `NavService` NavNodes so the main nav renders one unified community→channel tree, delivering per-channel mention badges/clear and retiring the per-community `ChannelSubSidebar`.

**Architecture:** `NavService` gains one reconcile method (`setChannels`) and stays a pure tree store. A new injected `ChannelNavSync` service bridges the channel data-pipe (`communityService.listChannels` + the `channel-config-updated` event) into `NavService`. Channel selection and management move up to App level (the nav is App-scoped), shrinking `CommunityView` to feed + governance + members. The existing `NavTree` recursion already renders a community's children inline (since ZEB-263); this plan populates them in production.

**Tech Stack:** Svelte 5 (runes: `$state`/`$derived`/`$effect`/`$props`), TypeScript, Vitest, Tauri IPC. Frontend-only — no Rust/CRDT change (channels already exist server-side).

**Design spec:** `docs/specs/2026-07-08-zeb-663-channels-as-nav-nodes-design.md` (approved 2026-07-08).

## Global Constraints

Every task's requirements implicitly include this section.

- **Frontend gate (run before every commit):** `npx tsc --noEmit` (from repo root) AND `npx vitest run` (from repo root). Both must be clean. This is the CI `frontend` job.
- **Style-token guard:** all new/changed CSS colors, spacing tokens, etc. must use `var(--*)` design tokens only — never raw hex/named colors. A `style-token-guard` test in the vitest suite enforces this; a raw literal fails the suite.
- **Scope:** full unification, **mentions-only**, frontend-only. NON-GOALS (do not implement): general per-message unread (`NavNode.unreadCount` bolding), governance-in-nav (Constitutional/Charter stay CommunityView tabs; Proposals keeps its existing nav row).
- **Mention invariant:** a community node's `mentionCount` is exactly the sum of its descendant channels' `mentionCount`. `incMention`/`clearMention`/`applyMentionDelta`/`setChannels` are the only mutators; each must keep this invariant. Never zero a community node directly in a way that leaves child counts non-zero.
- **`listChannels` order:** channel children render in `communityService.listChannels` order (backend sorts `created_at` ascending, general-first). No activity re-sort for `type: 'channel'`.
- **Tauri IPC naming:** Rust params are `snake_case`, JS callers pass `camelCase` (auto-converted). Error extraction: `const msg = e instanceof Error ? e.message : String(e)`.
- **Power gating:** channel create/rename/delete require `myCommunityPower >= POWER_THRESHOLDS.kick` (= 50). Management affordances show only on the **selected** community's rows (App resolves power for the selected community only).
- **Commit trailers (every commit):**
  ```
  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc
  ```
- **One PR / one branch:** branch `zeb-663-channels-as-nav-nodes` (already created; the spec is committed on it as `adb7cf30`). Do NOT auto-merge.
- **Do not edit the working tree while `tauri dev` is running** (SIGKILL rebuild loses in-memory state).

---

## File Structure

| File | Responsibility | Slice |
|---|---|---|
| `src/lib/types.ts` | `NavNode.channelKind?: 'text' \| 'voice'` (channel nodes only). | 1 |
| `src/lib/nav-service.ts` | `setChannels` reconcile; community-remove drops channel children. | 1 |
| `src/lib/nav-service.test.ts` | `setChannels` + community-remove-children unit tests. | 1 |
| `src/lib/channel-nav-sync.ts` | **New.** `ChannelNavSyncService` — the injected bridge (`start`/`resync`). | 2 |
| `src/lib/channel-nav-sync.test.ts` | **New.** Bridge unit tests (all deps injected). | 2 |
| `src/lib/components/NavNodeRow.svelte` | Channel-row glyph (`#`/`🔊`); channel context-menu (rename/delete) + demotion guard. | 2, 4 |
| `src/lib/components/AddChannelNavRow.svelte` | **New.** Synthetic "＋ add channel" row (mirrors `ProposalsNavRow`). | 4 |
| `src/lib/components/NavTree.svelte` | Append `AddChannelNavRow`; thread channel-manage props. | 4 |
| `src/lib/components/NavPanel.svelte` | Thread channel-manage props App→NavTree. | 4 |
| `src/App.svelte` | Instantiate `ChannelNavSync`; wire it; `selectedChannelId` + `openCommunityChannel` + resolution effect; hoist channel dialogs; nav channel-row wiring; remove ZEB-662 community-open clear. | 2, 3, 4, 6 |
| `src/lib/components/CommunityView.svelte` | Consume `selectedChannelId`; drop internal selection/management; `.three-cols`→2-col; drop Channels tab. | 3, 5 |
| `src/lib/components/ChannelSubSidebar.svelte` | **Deleted.** | 5 |

---

## Task 1: Data core — `NavNode.channelKind` + `NavService.setChannels` + community-remove children

**Files:**
- Modify: `src/lib/types.ts` (NavNode interface, ~line 104–131)
- Modify: `src/lib/nav-service.ts` (add `setChannels`; edit community `removed` branch ~line 201–206)
- Test: `src/lib/nav-service.test.ts` (add a `setChannels` describe block; extend community-remove coverage)

**Interfaces:**
- Consumes: `ChannelInfo` from `./community-service` (`{ channelId, name, writePower, kind: 'text'|'voice', createdAt, deletedAt? }`) — **type-only import** (community-service already does the reciprocal `import type { NavUpdatedPayload }`, so this stays a compile-time cycle with no runtime cycle).
- Produces (later tasks rely on these exact signatures):
  - `NavNode.channelKind?: 'text' | 'voice'`
  - `NavService.setChannels(communityId: string, channels: ChannelInfo[]): void` — caller passes **already deleted-filtered** live channels; reconciles the community's channel children (add/update-name+kind/remove), preserves each survivor's `mentionCount`/`expanded`, keeps `listChannels` order, subtracts removed channels' mentions from the community bubble, fires `onChange()` once iff the tree changed.
  - `NavService.addOrUpdateNavSpace` community `removed` now also drops `parentId === spaceId` children.

- [ ] **Step 1: Add `channelKind` to `NavNode`**

In `src/lib/types.ts`, inside `interface NavNode` (after the `mentionCount` field, ~line 117), add:

```typescript
  /** ZEB-663: channel kind, set only on `type: 'channel'` nodes. Drives the
   *  nav row glyph (# vs 🔊). Absent on all other node types. */
  channelKind?: 'text' | 'voice';
```

- [ ] **Step 2: Write the failing `setChannels` tests**

In `src/lib/nav-service.test.ts`, append a new describe block (the file already imports `describe, it, expect` and `NavService`):

```typescript
describe('NavService.setChannels reconcile (ZEB-663)', () => {
  const HLC = { wallMs: 0, logical: 0, deviceId: 'd' };
  const ch = (channelId: string, name: string, kind: 'text' | 'voice' = 'text') => ({
    channelId,
    name,
    writePower: 0,
    kind,
    createdAt: HLC,
  });

  function withCommunity(): NavService {
    const s = new NavService({ seedMockData: false });
    s.nodes = [
      { id: 'c1', parentId: null, type: 'community', name: 'C', expanded: true, unreadCount: 0, mentionCount: 0, unreadLevel: 'none' },
    ];
    return s;
  }

  it('adds channel children in listChannels order with channelKind', () => {
    const s = withCommunity();
    s.setChannels('c1', [ch('a', 'general'), ch('b', 'voice-room', 'voice')]);
    const kids = s.nodes.filter((n) => n.parentId === 'c1' && n.type === 'channel');
    expect(kids.map((n) => n.id)).toEqual(['a', 'b']);
    expect(kids.map((n) => n.name)).toEqual(['general', 'voice-room']);
    expect(kids.map((n) => n.channelKind)).toEqual(['text', 'voice']);
  });

  it('updates name and kind on survivors', () => {
    const s = withCommunity();
    s.setChannels('c1', [ch('a', 'general')]);
    s.setChannels('c1', [ch('a', 'lobby', 'voice')]);
    const a = s.nodes.find((n) => n.id === 'a')!;
    expect(a.name).toBe('lobby');
    expect(a.channelKind).toBe('voice');
  });

  it('preserves mentionCount + expanded on survivors across a reconcile', () => {
    const s = withCommunity();
    s.setChannels('c1', [ch('a', 'general'), ch('b', 'random')]);
    s.incMention('c1', 'a');
    s.incMention('c1', 'a');
    s.nodes.find((n) => n.id === 'a')!.expanded = true;
    s.setChannels('c1', [ch('a', 'general'), ch('b', 'random')]); // idempotent re-sync
    const a = s.nodes.find((n) => n.id === 'a')!;
    expect(a.mentionCount).toBe(2);
    expect(a.expanded).toBe(true);
    expect(s.nodes.find((n) => n.id === 'c1')!.mentionCount).toBe(2); // bubble intact
  });

  it('removes absent channels and subtracts their mentions from the community bubble', () => {
    const s = withCommunity();
    s.setChannels('c1', [ch('a', 'general'), ch('b', 'random')]);
    s.incMention('c1', 'a'); // community bubble = 1
    s.incMention('c1', 'b'); // community bubble = 2
    s.setChannels('c1', [ch('a', 'general')]); // b deleted
    expect(s.nodes.some((n) => n.id === 'b')).toBe(false);
    expect(s.nodes.find((n) => n.id === 'a')!.mentionCount).toBe(1);
    expect(s.nodes.find((n) => n.id === 'c1')!.mentionCount).toBe(1); // b's 1 subtracted
  });

  it('fires onChange only when the reconcile changes the tree', () => {
    const s = withCommunity();
    let changed = 0;
    s.onChange = () => { changed++; };
    s.setChannels('c1', [ch('a', 'general')]); // add → 1
    s.setChannels('c1', [ch('a', 'general')]); // no-op → still 1
    expect(changed).toBe(1);
  });

  it('is a no-op when the community node is absent', () => {
    const s = new NavService({ seedMockData: false });
    s.nodes = [];
    let changed = 0;
    s.onChange = () => { changed++; };
    s.setChannels('nope', [ch('a', 'general')]);
    expect(s.nodes.length).toBe(0);
    expect(changed).toBe(0);
  });
});

describe('NavService community removal drops channel children (ZEB-663)', () => {
  it('removing a community also removes its channel nodes', () => {
    const s = new NavService({ seedMockData: false });
    s.nodes = [
      { id: 'c1', parentId: null, type: 'community', name: 'C', expanded: true, unreadCount: 0, mentionCount: 0, unreadLevel: 'none' },
    ];
    s.setChannels('c1', [
      { channelId: 'a', name: 'general', writePower: 0, kind: 'text', createdAt: { wallMs: 0, logical: 0, deviceId: 'd' } },
    ]);
    expect(s.nodes.some((n) => n.id === 'a')).toBe(true);
    s.addOrUpdateNavSpace({ action: 'removed', spaceId: 'c1', kind: 'community', name: 'C' });
    expect(s.nodes.some((n) => n.id === 'c1')).toBe(false);
    expect(s.nodes.some((n) => n.id === 'a')).toBe(false); // child dropped too
  });
});
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `npx vitest run src/lib/nav-service.test.ts`
Expected: FAIL — `setChannels is not a function` (and the removal test's child still present).

- [ ] **Step 4: Implement `setChannels` and the type-only import**

In `src/lib/nav-service.ts`, at the top with the other imports, add:

```typescript
import type { ChannelInfo } from './community-service';
```

Then add this method to the `NavService` class (place it just after `addOrUpdateNavSpace`, before `communityIdOf`):

```typescript
  /**
   * ZEB-663: reconcile a community's channel children against a live
   * (deleted-filtered) `ChannelInfo[]`. Rebuilds the child block in
   * `listChannels` order (backend sorts created_at asc, general-first),
   * preserving each survivor's `mentionCount`/`expanded`. Removed channels'
   * mentions are subtracted from the community bubble so the sum invariant
   * holds. Fires `onChange()` once iff the tree actually changed (an
   * idempotent re-sync is silent, so a community switch doesn't re-render).
   */
  setChannels(communityId: string, channels: ChannelInfo[]): void {
    const community = this.nodes.find(
      (n) => n.id === communityId && n.type === 'community',
    );
    if (!community) return; // no community node to attach to → nothing to do

    const prev = new Map(
      this.nodes
        .filter((n) => n.parentId === communityId && n.type === 'channel')
        .map((n) => [n.id, n] as const),
    );

    // Detect structural change (id set, names, or kinds differ).
    let changed = prev.size !== channels.length;
    if (!changed) {
      for (const info of channels) {
        const old = prev.get(info.channelId);
        if (!old || old.name !== info.name || old.channelKind !== info.kind) {
          changed = true;
          break;
        }
      }
    }

    // Mentions on channels absent from the incoming set must leave the bubble.
    const nextIds = new Set(channels.map((c) => c.channelId));
    let removedMentions = 0;
    for (const [id, node] of prev) {
      if (!nextIds.has(id)) removedMentions += node.mentionCount;
    }

    if (!changed) return; // idempotent re-sync — stay silent

    // Rebuild the child block in listChannels order, preserving live state.
    const nextChildren: NavNode[] = channels.map((info) => {
      const old = prev.get(info.channelId);
      return {
        id: info.channelId,
        parentId: communityId,
        type: 'channel',
        channelKind: info.kind,
        name: info.name,
        expanded: old?.expanded ?? false,
        unreadCount: old?.unreadCount ?? 0,
        mentionCount: old?.mentionCount ?? 0,
        unreadLevel: old?.unreadLevel ?? 'none',
      };
    });

    // Replace only this community's channel children; keep every other node's
    // order. Insert the block right after the community node so children stay
    // grouped (sibling render order is by array order among same-parent nodes).
    const others = this.nodes.filter(
      (n) => !(n.parentId === communityId && n.type === 'channel'),
    );
    const communityIdx = others.findIndex((n) => n.id === communityId);
    this.nodes = [
      ...others.slice(0, communityIdx + 1),
      ...nextChildren,
      ...others.slice(communityIdx + 1),
    ];

    if (removedMentions > 0) {
      community.mentionCount = Math.max(0, community.mentionCount - removedMentions);
    }

    this.onChange?.();
  }
```

- [ ] **Step 5: Drop channel children on community removal**

In `src/lib/nav-service.ts`, in `addOrUpdateNavSpace`, the community `removed` branch (currently ~line 201–205):

```typescript
      if (action === 'removed') {
        const before = this.nodes.length;
        this.nodes = this.nodes.filter((n) => n.id !== spaceId);
        if (this.nodes.length !== before) this.onChange?.();
        return;
      }
```

Change the filter to also drop channel children:

```typescript
      if (action === 'removed') {
        const before = this.nodes.length;
        // ZEB-663: a community's channel children are removed with it.
        this.nodes = this.nodes.filter((n) => n.id !== spaceId && n.parentId !== spaceId);
        if (this.nodes.length !== before) this.onChange?.();
        return;
      }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `npx vitest run src/lib/nav-service.test.ts`
Expected: PASS (all new + existing nav-service tests green).

- [ ] **Step 7: Run the full frontend gate**

Run: `npx tsc --noEmit` then `npx vitest run`
Expected: both clean.

- [ ] **Step 8: Commit**

```bash
git add src/lib/types.ts src/lib/nav-service.ts src/lib/nav-service.test.ts
git commit -m "feat(nav): NavService.setChannels reconcile + channelKind (ZEB-663)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc"
```

---

## Task 2: `ChannelNavSync` bridge + App wiring + eager boot + channel glyph

**Files:**
- Create: `src/lib/channel-nav-sync.ts`
- Test: `src/lib/channel-nav-sync.test.ts`
- Modify: `src/App.svelte` (construct + wire the bridge; 4 small edits)
- Modify: `src/lib/components/NavNodeRow.svelte` (`typeIcon` voice glyph)

**Interfaces:**
- Consumes: `NavService.setChannels` (Task 1); `communityService.listChannels(communityId): Promise<ChannelInfo[]>`; `communityService.onChannelConfigChanged` callback slot; `ChannelInfo.deletedAt?`.
- Produces:
  - `ChannelNavSyncDeps { listChannels(communityId): Promise<ChannelInfo[]>; setChannels(communityId, channels: ChannelInfo[]): void; listCommunityIds(): string[]; }`
  - `ChannelNavSyncService` with `start(): Promise<void>` (eager-populate every joined community) and `resync(communityId): Promise<void>` (re-fetch + reconcile; per-community try/catch, never throws).

**Design note (deviation from spec, deliberate):** The spec listed a separate `onCommunityAdded(communityId)` method. It is realized here as **`resync` wired into `changeSelectedCommunity`** — every join path (create/redeem/join/fork) calls `changeSelectedCommunity(id)` immediately after adding the community node, so that single choke-point covers post-boot joins without editing 4 scattered synth sites. `resync` is idempotent (cache-hit `listChannels` + silent no-op `setChannels`), so firing it on every community switch is cheap.

- [ ] **Step 1: Write the failing bridge tests**

Create `src/lib/channel-nav-sync.test.ts`:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { ChannelNavSyncService, type ChannelNavSyncDeps } from './channel-nav-sync';
import type { ChannelInfo } from './community-service';

const HLC = { wallMs: 0, logical: 0, deviceId: 'd' };
const ch = (id: string, name: string, deleted = false): ChannelInfo => ({
  channelId: id,
  name,
  writePower: 0,
  kind: 'text',
  createdAt: HLC,
  ...(deleted ? { deletedAt: HLC } : {}),
});

function harness(
  channelsByCommunity: Record<string, ChannelInfo[] | Error>,
  communityIds: string[],
) {
  const setCalls: Array<[string, ChannelInfo[]]> = [];
  const deps: ChannelNavSyncDeps = {
    listChannels: vi.fn(async (id: string) => {
      const v = channelsByCommunity[id];
      if (v instanceof Error) throw v;
      return v ?? [];
    }),
    setChannels: (id, channels) => setCalls.push([id, channels]),
    listCommunityIds: () => communityIds,
  };
  return { svc: new ChannelNavSyncService(deps), setCalls, deps };
}

describe('ChannelNavSyncService (ZEB-663)', () => {
  it('start() eager-populates every joined community', async () => {
    const { svc, setCalls } = harness(
      { c1: [ch('a', 'general')], c2: [ch('b', 'lobby')] },
      ['c1', 'c2'],
    );
    await svc.start();
    expect(setCalls.map(([id]) => id).sort()).toEqual(['c1', 'c2']);
  });

  it('resync filters deletedAt before setChannels', async () => {
    const { svc, setCalls } = harness(
      { c1: [ch('a', 'general'), ch('b', 'gone', true)] },
      ['c1'],
    );
    await svc.resync('c1');
    expect(setCalls).toHaveLength(1);
    expect(setCalls[0][1].map((c) => c.channelId)).toEqual(['a']); // 'b' filtered
  });

  it('a listChannels rejection is swallowed and does not block other communities', async () => {
    const { svc, setCalls } = harness(
      { c1: new Error('boom'), c2: [ch('b', 'lobby')] },
      ['c1', 'c2'],
    );
    await svc.start(); // must not reject
    expect(setCalls.map(([id]) => id)).toEqual(['c2']); // c1 skipped, c2 populated
  });

  it('resync never rejects even if listChannels throws', async () => {
    const { svc, setCalls } = harness({ c1: new Error('boom') }, ['c1']);
    await expect(svc.resync('c1')).resolves.toBeUndefined();
    expect(setCalls).toHaveLength(0);
  });
});
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `npx vitest run src/lib/channel-nav-sync.test.ts`
Expected: FAIL — module `./channel-nav-sync` not found.

- [ ] **Step 3: Implement the bridge**

Create `src/lib/channel-nav-sync.ts`:

```typescript
import type { ChannelInfo } from './community-service';

/**
 * ZEB-663: the bridge from the channel data-pipe (CommunityService +
 * the `channel-config-updated` event) into NavService's tree. All side
 * effects are injected → deterministic + unit-testable (mirrors the
 * dep-injection pattern of mention-alert.ts / incoming-call-alert.ts).
 */
export interface ChannelNavSyncDeps {
  /** communityService.listChannels — cached; may reject pre-connect. */
  listChannels(communityId: string): Promise<ChannelInfo[]>;
  /** navService.setChannels — reconciles the community's channel children. */
  setChannels(communityId: string, channels: ChannelInfo[]): void;
  /** Current nav community node ids (navService.nodes, community-kind). */
  listCommunityIds(): string[];
}

export class ChannelNavSyncService {
  constructor(private deps: ChannelNavSyncDeps) {}

  /** Eager boot: populate channels for every joined community. Per-community
   *  failures are isolated (a stalled/erroring community renders childless and
   *  self-heals on its next resync); start() never rejects. */
  async start(): Promise<void> {
    await Promise.allSettled(
      this.deps.listCommunityIds().map((id) => this.resync(id)),
    );
  }

  /** Re-fetch a community's channels and reconcile them into the nav tree.
   *  The `channel-config-updated` event already invalidated CommunityService's
   *  cache, so listChannels re-fetches. Swallows failures (never throws into
   *  boot / event handlers). */
  async resync(communityId: string): Promise<void> {
    try {
      const channels = await this.deps.listChannels(communityId);
      const live = channels.filter((c) => c.deletedAt === undefined);
      this.deps.setChannels(communityId, live);
    } catch (e) {
      console.warn(
        `[channel-nav-sync] resync failed for ${communityId}:`,
        e instanceof Error ? e.message : String(e),
      );
    }
  }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `npx vitest run src/lib/channel-nav-sync.test.ts`
Expected: PASS.

- [ ] **Step 5: Construct `ChannelNavSync` in App**

In `src/App.svelte`, add the import near the other `./lib` service imports (top of the `<script>`):

```typescript
  import { ChannelNavSyncService } from './lib/channel-nav-sync';
```

Then, immediately after the `communityService` construction + destroy effect (currently ~line 1324–1325: `const communityService = new CommunityService();` / `$effect(() => () => communityService.destroy());`), add:

```typescript
  // ── ZEB-663: channel→nav bridge ────────────────────────────────────
  // Populates each community's channel children as first-class NavNodes so
  // the unified nav tree renders channels inline. Pure dep-injection — no
  // direct service refs beyond the three lambdas below.
  const channelNavSync = new ChannelNavSyncService({
    listChannels: (id) => communityService.listChannels(id),
    setChannels: (id, channels) => navService.setChannels(id, channels),
    listCommunityIds: () =>
      navService.nodes.filter((n) => n.type === 'community').map((n) => n.id),
  });
```

- [ ] **Step 6: Wire resync-on-switch, the config-event base handler, and eager boot**

Three edits in `src/App.svelte`:

**(a) resync on community switch.** In `changeSelectedCommunity`, immediately after `selectedCommunityId = id;` (currently ~line 1148), add:

```typescript
    // ZEB-663: ensure this community's channels are populated in the nav tree
    // (covers communities joined after boot, which start()'s snapshot missed).
    // Idempotent + cache-hit for already-synced communities.
    if (id != null) void channelNavSync.resync(id);
```

**(b) base `channel-config-updated` handler.** Inside the Tauri-init IIFE, immediately after `await tryConnect('community', communityService.connectAdapter(adapter));` (currently ~line 1963), add:

```typescript
      // ZEB-663: base channel-config handler — keep the nav tree's channel
      // children fresh on create/rename/delete. CommunityView (when mounted)
      // chains its own feed-list refresh on top of this via the existing
      // prev-handler chain.
      communityService.onChannelConfigChanged = (cid) => {
        void channelNavSync.resync(cid);
      };
```

**(c) eager boot.** Immediately after the `listOwnerCommunities` rehydration `try/catch` block (the one that loops `navService.addOrUpdateNavSpace(toNavPayload(c))`, ending ~line 2001), add:

```typescript
      // ZEB-663: now that nav communities are hydrated, populate their
      // channels as nav nodes. Fire-and-forget; per-community failures are
      // isolated inside ChannelNavSync and never block boot.
      void channelNavSync.start();
```

- [ ] **Step 7: Voice-aware channel glyph in `NavNodeRow`**

In `src/lib/components/NavNodeRow.svelte`, the `typeIcon` function (currently ~line 84–89):

```typescript
  function typeIcon(n: NavNode): string {
    if (n.type === 'channel') return '#';
    if (n.type === 'dm' || n.type === 'group-chat') return '@';
    if (n.type === 'folder') return n.expanded ? '▾' : '▸';
    return '';
  }
```

Change the channel branch to distinguish voice:

```typescript
  function typeIcon(n: NavNode): string {
    // ZEB-663: voice channels get the speaker glyph (matches ChannelSubSidebar).
    if (n.type === 'channel') return n.channelKind === 'voice' ? '🔊' : '#';
    if (n.type === 'dm' || n.type === 'group-chat') return '@';
    if (n.type === 'folder') return n.expanded ? '▾' : '▸';
    return '';
  }
```

- [ ] **Step 8: Run the full frontend gate**

Run: `npx tsc --noEmit` then `npx vitest run`
Expected: both clean. (Channel nodes now populate in production; clicking one still misroutes through the DM branch — Task 3 fixes routing. That is an acceptable mid-branch state; no test asserts channel-click routing yet.)

- [ ] **Step 9: Commit**

```bash
git add src/lib/channel-nav-sync.ts src/lib/channel-nav-sync.test.ts src/App.svelte src/lib/components/NavNodeRow.svelte
git commit -m "feat(nav): ChannelNavSync bridge populates channel nav nodes (ZEB-663)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc"
```

---

## Task 3: Selection routing — `openCommunityChannel` + active highlight + CommunityView consumes `selectedChannelId`

**Files:**
- Modify: `src/App.svelte` (add `selectedChannelId` state, `openCommunityChannel`, selection-resolution `$effect`, `handleNodeClick` channel branch, `navActiveNodeId`; pass `selectedChannelId`/`onSelectChannel` to CommunityView)
- Modify: `src/lib/components/CommunityView.svelte` (consume `selectedChannelId`; remove internal `activeChannelId`, `handleSelect`, `pickFallbackChannel`, the channel-selection part of the per-community effect and the config-event fallback; keep feed-list refresh)

**Interfaces:**
- Consumes: `communityService.getSelectedChannel(cid)` / `setSelectedChannel(cid, id)`; `navService.clearMention(id)`; App's reactive `navNodes` ($state, updated by `navService.onChange`); `communityActiveView` ('channels'|'proposals'|'tier3'|'charter'); `changeSelectedCommunity(id)`; `switchMode`; `refreshCommunityMembers`.
- Produces:
  - `App.openCommunityChannel(communityId: string, channelId: string): void` — nav channel-row onClick target.
  - `CommunityView` prop `selectedChannelId: string | null` (drives `activeChannel`), prop `onSelectChannel: (channelId: string) => void` (temporary — ChannelSubSidebar select; removed in Task 5).

- [ ] **Step 1: Add `selectedChannelId` state + `openCommunityChannel` in App**

In `src/App.svelte`, near `selectedCommunityId` (~line 1072), add:

```typescript
  // ZEB-663: the reactive selected channel for the currently-selected
  // community (single source of truth for channel selection; persistence
  // mirrors into communityService.setSelectedChannel). Null = none resolved
  // yet (the resolution effect below picks a default from the nav children).
  let selectedChannelId = $state<string | null>(null);
```

Then add `openCommunityChannel` immediately after `openCommunityProposals` (~line 1160):

```typescript
  /** ZEB-663: nav channel-row click — select the community + channel and land
   *  on its Channels feed. Mirrors openCommunityProposals. */
  function openCommunityChannel(communityId: string, channelId: string) {
    if (appMode !== 'messages') switchMode('messages');
    changeSelectedCommunity(communityId);
    void refreshCommunityMembers(communityId);
    showSettings = false;
    communityService.setSelectedChannel(communityId, channelId);
    selectedChannelId = channelId;
    communityActiveView = 'channels';
    // ZEB-663: viewing a channel clears its unseen-mention indicator.
    navService.clearMention(channelId);
  }
```

- [ ] **Step 2: Add the selection-resolution `$effect` in App**

In `src/App.svelte`, after the `myCommunityPower` derived (~line 1101) — anywhere in the reactive-declarations region is fine — add:

```typescript
  // ZEB-663: resolve which channel is selected for the current community when
  // the current selection isn't a live channel of it (community switch,
  // first visit, or the active channel was just deleted). Restores the
  // persisted last-viewed channel, else #general, else the first channel.
  // Only runs on the Channels view so governance views don't force a channel
  // selection. Gated on populated nav children so the just-joined async
  // window resolves once ChannelNavSync fills them in.
  $effect(() => {
    const cid = selectedCommunityId;
    if (!cid || communityActiveView !== 'channels') return;
    const kids = navNodes.filter((n) => n.parentId === cid && n.type === 'channel');
    if (kids.length === 0) return; // not populated yet
    const stillValid = selectedChannelId !== null && kids.some((k) => k.id === selectedChannelId);
    if (stillValid) return;
    const persisted = communityService.getSelectedChannel(cid);
    const target =
      persisted && kids.some((k) => k.id === persisted)
        ? persisted
        : (kids.find((k) => k.name === 'general') ?? kids[0]).id;
    selectedChannelId = target;
    communityService.setSelectedChannel(cid, target);
    navService.clearMention(target); // landing on it marks its mentions seen
  });
```

- [ ] **Step 3: Route channel-row clicks in `handleNodeClick`**

In `src/App.svelte`, in `handleNodeClick`, immediately after the folder guard (`if (!node || node.type === 'folder') return;`, ~line 2802) and before the community branch, add:

```typescript
    // ZEB-663: a channel row routes to its community + channel feed.
    if (node.type === 'channel') {
      if (node.parentId) openCommunityChannel(node.parentId, node.id);
      return;
    }
```

- [ ] **Step 4: Extend `navActiveNodeId` to highlight the active channel row**

In `src/App.svelte`, replace `navActiveNodeId` (~line 2777–2779):

```typescript
  let navActiveNodeId = $derived(
    notesSelected ? null : (selectedCommunityId ?? activeChannel),
  );
```

with:

```typescript
  let navActiveNodeId = $derived(
    notesSelected
      ? null
      : selectedCommunityId
        // ZEB-663: on the Channels view, highlight the active channel row;
        // otherwise (proposals/charter/tier3, or no channel yet) the community.
        ? (communityActiveView === 'channels' ? (selectedChannelId ?? selectedCommunityId) : selectedCommunityId)
        : activeChannel,
  );
```

- [ ] **Step 5: Pass `selectedChannelId` + `onSelectChannel` into CommunityView**

In `src/App.svelte`, in the `<CommunityView ... />` mount (~line 3220), add these props (anywhere in the prop list):

```svelte
        {selectedChannelId}
        onSelectChannel={(channelId) => openCommunityChannel(selectedCommunityNode.id, channelId)}
```

- [ ] **Step 6: Make CommunityView consume `selectedChannelId` (remove its internal selection)**

In `src/lib/components/CommunityView.svelte`:

**(a)** Add the two props. In the `$props()` destructure (~line 57) add `selectedChannelId` and `onSelectChannel`, and in the props type block add:

```typescript
    /** ZEB-663: the App-owned selected channel id (single source of truth).
     *  Drives which channel's feed renders. */
    selectedChannelId: string | null;
    /** ZEB-663: select a channel (App routes through openCommunityChannel).
     *  Temporary — the ChannelSubSidebar select path; the nav rows are the
     *  real selector. Removed with ChannelSubSidebar (Task 5). */
    onSelectChannel: (channelId: string) => void;
```

**(b)** Remove the internal selection state `let activeChannelId = $state<string | null>(null);` (~line 134) and redefine `activeChannel` (~line 196) to derive from the prop:

```typescript
  let activeChannel = $derived(channels.find((c) => c.channelId === selectedChannelId) ?? null);
```

**(c)** Delete `handleSelect` (~line 216–219) and `pickFallbackChannel` (~line 209–214) — App owns selection + fallback now.

**(d)** In the `onMount` `onChannelConfigChanged` chain (~line 242–258), remove the selection/fallback block; keep the feed-list refresh. The handler body becomes:

```typescript
    communityService.onChannelConfigChanged = (cid, action, channelId, name, writePower) => {
      prevOnChannelConfigChanged?.(cid, action, channelId, name, writePower);
      if (cid !== communityId) return;
      // ZEB-663: keep only the feed-list refresh here — App owns the
      // selected-channel fallback (its resolution effect re-picks when the
      // active channel is removed from the nav children).
      void refreshChannels().catch((e) => {
        console.warn('CommunityView: refreshChannels failed in onChannelConfigChanged:', e);
      });
    };
```

**(e)** In the per-community `$effect` (~line 265–319), remove the channel-selection block (the `persisted`/`refreshChannels`/`stillExists`/default-pick logic in the async IIFE, and the `activeChannelId = null` reset ~line 272). Keep the snapshot/lineage/phase2 resets and the `getPreForkSnapshot` load. Keep a `refreshChannels()` call so `channels` (the feed list) is populated on community switch. The IIFE becomes:

```typescript
    void (async () => {
      try {
        // ZEB-285 Task 11: load pre-fork snapshot for unified timeline.
        communityService.getPreForkSnapshot(cid).then((snapshot) => {
          if (!cancelled) preForkSnapshot = snapshot;
        }).catch(() => {
          if (!cancelled) preForkSnapshot = null;
        });
        // ZEB-663: refresh the feed channel list (selection is App-owned).
        await refreshChannels();
      } catch (e) {
        console.warn('CommunityView: refreshChannels failed in community $effect:', e);
      }
    })();
```

**(f)** The voice-teardown `$effect` (~line 358–365): change `const nowActive = activeChannelId;` to `const nowActive = selectedChannelId;`.

**(g)** Point ChannelSubSidebar at the prop-driven selection (still mounted until Task 5). In the `<ChannelSubSidebar ... />` (~line 475), change `activeChannelId={activeChannelId}` to `activeChannelId={selectedChannelId}` and `onSelect={handleSelect}` to `onSelect={onSelectChannel}`.

**(h)** `CreateChannelDialog`'s `onCreated` (~line 660–663) currently calls `handleSelect(channelId)`. Change it to `onSelectChannel(channelId)` (until the dialog moves to App in Task 4).

- [ ] **Step 7: Run the full frontend gate**

Run: `npx tsc --noEmit` then `npx vitest run`
Expected: both clean. tsc catches any missed `activeChannelId` reference.

- [ ] **Step 8: Manual smoke check (dev build)**

> Do NOT run this while a separate `tauri dev` is editing the tree. Build/run per your normal dev flow, then verify:
> - Clicking a channel row in the nav selects it; the row highlights (`--primary-soft`/`--primary-deep`).
> - Clicking a community row lands on its last-viewed (or #general) channel; that row highlights.
> - The ChannelSubSidebar's active channel matches the nav highlight (both reflect `selectedChannelId`).

- [ ] **Step 9: Commit**

```bash
git add src/App.svelte src/lib/components/CommunityView.svelte
git commit -m "feat(nav): route channel selection through App (openCommunityChannel) (ZEB-663)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc"
```

---

## Task 4: Management migration — hoist channel dialogs to App + nav triggers (power-gated)

**Files:**
- Create: `src/lib/components/AddChannelNavRow.svelte`
- Modify: `src/lib/components/NavNodeRow.svelte` (channel context-menu: rename/delete + demotion guard)
- Modify: `src/lib/components/NavTree.svelte` (append `AddChannelNavRow`; thread channel-manage props)
- Modify: `src/lib/components/NavPanel.svelte` (thread channel-manage props)
- Modify: `src/App.svelte` (channel-dialog state + handlers; mount dialogs; provide nav-manage callbacks)
- Modify: `src/lib/components/CommunityView.svelte` (strip management from ChannelSubSidebar wiring + remove hoisted dialogs)

**Interfaces:**
- Consumes: `CreateChannelDialog` props `{ communityId, communityService, open, myPower, onClose, onCreated(channelId) }`; `ModifyChannelDialog` props `{ communityId, channel: ChannelInfo, communityService, open, myPower, onClose }`; `TypedConfirmationModal` props `{ title, description, requiredText, confirmLabel, onConfirm, onCancel }`; `POWER_THRESHOLDS.kick`; `communityService.deleteChannel(cid, channelId)` / `listChannels(cid)`.
- Produces (threaded App→NavPanel→NavTree→NavNodeRow):
  - `canManageChannels?: (communityId: string) => boolean` — true iff `communityId === selectedCommunityId && myCommunityPower >= POWER_THRESHOLDS.kick`.
  - `onAddChannel?: (communityId: string) => void`
  - `onRenameChannel?: (communityId: string, channelId: string) => void`
  - `onDeleteChannel?: (communityId: string, channelId: string) => void`

- [ ] **Step 1: Create `AddChannelNavRow.svelte`**

Create `src/lib/components/AddChannelNavRow.svelte` (mirrors `ProposalsNavRow` anatomy so it aligns with channel rows):

```svelte
<script lang="ts">
  /**
   * ZEB-663: synthetic "＋ add channel" row rendered by NavTree inside an
   * expanded community when the viewer can manage its channels — NOT a
   * NavNode. Mirrors ProposalsNavRow's row anatomy + keyboard model.
   */
  let {
    communityId,
    indent = 0,
    onAdd,
  }: {
    communityId: string;
    indent?: number;
    onAdd?: (communityId: string) => void;
  } = $props();

  let paddingLeft = $derived(indent * 4 + 8);

  function activate(e: MouseEvent | KeyboardEvent) {
    e.stopPropagation();
    onAdd?.(communityId);
  }
</script>

<div
  class="add-channel-row"
  role="button"
  tabindex="0"
  data-testid="add-channel-row-{communityId}"
  onclick={activate}
  onkeydown={(e) => { if (e.key === 'Enter' || e.key === ' ') activate(e); }}
>
  <span class="row-content" style="padding-left: {paddingLeft}px">
    <span class="add-glyph" aria-hidden="true">＋</span>
    <span class="row-label">add channel</span>
  </span>
</div>

<style>
  .add-channel-row {
    position: relative;
    display: flex;
    align-items: center;
    width: 100%;
    min-height: 32px;
    border: none;
    background: none;
    color: var(--text-muted);
    font-size: 14px;
    cursor: pointer;
    text-align: left;
  }
  .add-channel-row:hover {
    background: var(--bg-tertiary);
    color: var(--text-primary);
  }
  .row-content {
    display: flex;
    align-items: center;
    gap: 6px;
    flex: 1;
    min-width: 0;
  }
  .add-glyph {
    flex-shrink: 0;
    width: 16px;
    text-align: center;
    color: var(--text-muted);
  }
  .row-label {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
```

- [ ] **Step 2: Add channel context-menu to `NavNodeRow`**

In `src/lib/components/NavNodeRow.svelte`:

**(a)** Add props. In the `$props()` destructure (~line 20–33) add `canManageChannel`, `onRenameChannel`, `onDeleteChannel`, and in the props type block add:

```typescript
    /** ZEB-663: may the viewer manage THIS channel node (rename/delete)?
     *  Resolves true only for the selected community's channels at power ≥ kick. */
    canManageChannel?: (node: NavNode) => boolean;
    /** ZEB-663: open rename dialog for a channel node. */
    onRenameChannel?: (communityId: string, channelId: string) => void;
    /** ZEB-663: open delete-confirm for a channel node. */
    onDeleteChannel?: (communityId: string, channelId: string) => void;
```

**(b)** Add the context-menu state + demotion guard + outside-click dismiss (mirrors ChannelSubSidebar §6.8). After the existing `let showSortMenu = $state(false);` (~line 55):

```typescript
  // ZEB-663: channel-row moderation context menu (rename/delete).
  let canManage = $derived(node.type === 'channel' && (canManageChannel?.(node) ?? false));
  let channelMenu = $state<{ x: number; y: number } | null>(null);
  let channelMenuEl: HTMLElement | undefined = $state();

  // §6.8: close a stale moderation menu when the viewer is demoted.
  $effect(() => {
    if (!canManage) channelMenu = null;
  });

  $effect(() => {
    if (!channelMenu) return;
    function onDocClick(e: MouseEvent) {
      const target = e.target as Node | null;
      if (channelMenuEl && target && channelMenuEl.contains(target)) return;
      channelMenu = null;
    }
    document.addEventListener('click', onDocClick, true);
    return () => document.removeEventListener('click', onDocClick, true);
  });

  function onChannelContextMenu(e: MouseEvent) {
    if (!canManage) return;
    e.preventDefault();
    e.stopPropagation();
    channelMenu = { x: e.clientX, y: e.clientY };
  }

  function channelRename() {
    channelMenu = null;
    if (node.parentId) onRenameChannel?.(node.parentId, node.id);
  }
  function channelDelete() {
    channelMenu = null;
    if (node.parentId) onDeleteChannel?.(node.parentId, node.id);
  }
```

**(c)** Wire `oncontextmenu` on the row `<div class="nav-row" ...>` (~line 110–119). Add the attribute:

```svelte
  oncontextmenu={onChannelContextMenu}
```

**(d)** Render the menu. After the closing `</div>` of `.nav-row` (~line 229), add:

```svelte
{#if channelMenu}
  <div
    bind:this={channelMenuEl}
    class="channel-context-menu"
    role="menu"
    style="left: {channelMenu.x}px; top: {channelMenu.y}px"
  >
    <button type="button" role="menuitem" onclick={channelRename}>Rename</button>
    <button type="button" role="menuitem" class="destructive" onclick={channelDelete}>Delete</button>
  </div>
{/if}
```

**(e)** Add the menu styles inside `<style>` (reuse ChannelSubSidebar's token palette):

```css
  .channel-context-menu {
    position: fixed;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 4px;
    box-shadow: 0 2px 8px var(--shadow-mid);
    z-index: 1000;
    min-width: 140px;
    padding: 4px 0;
  }
  .channel-context-menu button {
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
  .channel-context-menu button:hover { background: var(--bg-tertiary); }
  .channel-context-menu button.destructive { color: var(--danger-muted); }
```

- [ ] **Step 3: Append `AddChannelNavRow` + thread props in `NavTree`**

In `src/lib/components/NavTree.svelte`:

**(a)** Import (~line 6): `import AddChannelNavRow from './AddChannelNavRow.svelte';`

**(b)** Add the four channel-manage props to the `$props()` destructure and type block (~line 8–43):

```typescript
    canManageChannels,
    onAddChannel,
    onRenameChannel,
    onDeleteChannel,
```
```typescript
    canManageChannels?: (communityId: string) => boolean;
    onAddChannel?: (communityId: string) => void;
    onRenameChannel?: (communityId: string, channelId: string) => void;
    onDeleteChannel?: (communityId: string, channelId: string) => void;
```

**(c)** Pass `canManageChannel`/`onRenameChannel`/`onDeleteChannel` to `<NavNodeRow>` (~line 59–72):

```svelte
    canManageChannel={(n) => (canManageChannels && n.parentId ? canManageChannels(n.parentId) : false)}
    {onRenameChannel}
    {onDeleteChannel}
```

**(d)** Thread all four props into the recursive `<NavTree ... />` self-call (~line 75):

```svelte
{canManageChannels} {onAddChannel} {onRenameChannel} {onDeleteChannel}
```

**(e)** After the `ProposalsNavRow` block (~line 76–84), inside the same `{#if child.type === 'community' && ...}` region, append the add-channel row when the viewer can manage. Add a sibling block right after the ProposalsNavRow `{#if}`:

```svelte
    {#if child.type === 'community' && canManageChannels && canManageChannels(child.id)}
      <AddChannelNavRow communityId={child.id} indent={ancestry.length} onAdd={onAddChannel} />
    {/if}
```

- [ ] **Step 4: Thread channel-manage props through `NavPanel`**

In `src/lib/components/NavPanel.svelte`, add the four props to the `$props()` destructure + type block (mirror the existing `proposalCount`/`onSelectProposals` threading), and pass them to each `<NavTree ... />` instance (there are three, ~line 331/349/367). For each NavTree add:

```svelte
            {canManageChannels}
            {onAddChannel}
            {onRenameChannel}
            {onDeleteChannel}
```

Props type additions:

```typescript
    canManageChannels?: (communityId: string) => boolean;
    onAddChannel?: (communityId: string) => void;
    onRenameChannel?: (communityId: string, channelId: string) => void;
    onDeleteChannel?: (communityId: string, channelId: string) => void;
```

- [ ] **Step 5: Hoist channel dialogs + handlers into App**

In `src/App.svelte`:

**(a)** Import the dialogs near the other component imports:

```typescript
  import CreateChannelDialog from './lib/components/CreateChannelDialog.svelte';
  import ModifyChannelDialog from './lib/components/ModifyChannelDialog.svelte';
  import TypedConfirmationModal from './lib/components/TypedConfirmationModal.svelte';
```

> If any of these is already imported, keep the single existing import.

**(b)** Add dialog state near `selectedChannelId` (~line 1072):

```typescript
  // ZEB-663: hoisted channel-management dialog state (was CommunityView's).
  // Scoped to the selected community; power-gated by myCommunityPower.
  let showCreateChannelDialog = $state(false);
  let modifyChannelTarget = $state<import('./lib/community-service').ChannelInfo | null>(null);
  let deleteChannelTarget = $state<import('./lib/community-service').ChannelInfo | null>(null);
```

**(c)** Add the nav-manage callbacks + delete handler (place near `openCommunityChannel`):

```typescript
  // ZEB-663: nav channel management — gated to the selected community at power
  // ≥ kick (App resolves power for the selected community only).
  function canManageSelectedCommunityChannels(communityId: string): boolean {
    return communityId === selectedCommunityId && myCommunityPower >= POWER_THRESHOLDS.kick;
  }

  async function openRenameChannel(communityId: string, channelId: string) {
    const list = await communityService.listChannels(communityId);
    const ch = list.find((c) => c.channelId === channelId);
    if (ch) modifyChannelTarget = ch;
  }

  async function openDeleteChannel(communityId: string, channelId: string) {
    const list = await communityService.listChannels(communityId);
    const ch = list.find((c) => c.channelId === channelId);
    if (ch) deleteChannelTarget = ch;
  }

  async function confirmDeleteChannel() {
    const target = deleteChannelTarget;
    deleteChannelTarget = null;
    if (!target || !selectedCommunityId) return;
    try {
      await communityService.deleteChannel(selectedCommunityId, target.channelId);
      // The channel-config-updated event drives the nav reconcile + the App
      // resolution effect's fallback re-select if the active channel went away.
    } catch (e) {
      console.warn('deleteChannel failed:', e instanceof Error ? e.message : String(e));
    }
  }
```

> Ensure `POWER_THRESHOLDS` is imported in App (check the existing `./lib/types` import; add it if missing).

**(d)** Pass the manage callbacks to `<NavPanel>` (~line 3148). Add:

```svelte
        canManageChannels={canManageSelectedCommunityChannels}
        onAddChannel={() => { showCreateChannelDialog = true; }}
        onRenameChannel={openRenameChannel}
        onDeleteChannel={openDeleteChannel}
```

**(e)** Mount the dialogs (near the other App-level modals, e.g. after the create-community modal ~line 3742). Only meaningful when a community is selected:

```svelte
{#if selectedCommunityId}
  <CreateChannelDialog
    communityId={selectedCommunityId}
    {communityService}
    open={showCreateChannelDialog}
    myPower={myCommunityPower}
    onClose={() => { showCreateChannelDialog = false; }}
    onCreated={(channelId) => {
      showCreateChannelDialog = false;
      openCommunityChannel(selectedCommunityId, channelId);
    }}
  />
  {#if modifyChannelTarget}
    <ModifyChannelDialog
      communityId={selectedCommunityId}
      channel={modifyChannelTarget}
      {communityService}
      open={true}
      myPower={myCommunityPower}
      onClose={() => { modifyChannelTarget = null; }}
    />
  {/if}
  {#if deleteChannelTarget}
    <TypedConfirmationModal
      title={`Delete #${deleteChannelTarget.name}?`}
      description="Channel deletion is permanent. The message log persists on existing devices, but no new messages can be posted and the channel will disappear from the sidebar for everyone."
      requiredText={deleteChannelTarget.name}
      confirmLabel="Delete channel"
      onConfirm={confirmDeleteChannel}
      onCancel={() => { deleteChannelTarget = null; }}
    />
  {/if}
{/if}
```

- [ ] **Step 6: Strip management from CommunityView / ChannelSubSidebar wiring**

In `src/lib/components/CommunityView.svelte`:

**(a)** Remove the management-trigger props from the `<ChannelSubSidebar ... />` (~line 475–483): delete `onCreateClick`, `onModifyClick`, `onDeleteClick`. (ChannelSubSidebar keeps only `channels`, `activeChannelId`, `myPower`, `onSelect` — it is deleted entirely in Task 5.)

**(b)** Remove the now-unused dialog mounts + state from CommunityView: delete the `<CreateChannelDialog>` (~line 654–664), the `{#if modifyDialogChannel}<ModifyChannelDialog>` (~line 666–675), the `{#if deleteConfirmChannel}<TypedConfirmationModal>` (~line 677–686), their imports (~line 15–17), the state `showCreateDialog`/`modifyDialogChannel`/`deleteConfirmChannel` (~line 137–139), and `handleConfirmDelete` (~line 221–236).

> `ChannelSubSidebar` also needs its now-unused management props removed from its own `$props` (`onCreateClick`/`onModifyClick`/`onDeleteClick`) and the create button + context-menu removed — OR leave ChannelSubSidebar untouched since Task 5 deletes it. **Chosen: leave ChannelSubSidebar's file as-is** (its management props become optional-unused; but they're currently required). To avoid a tsc error from dropping required props, make `onCreateClick`/`onModifyClick`/`onDeleteClick` optional in ChannelSubSidebar's props type (`onCreateClick?: () => void;` etc.) in this step. Task 5 deletes the whole file.

- [ ] **Step 7: Run the full frontend gate**

Run: `npx tsc --noEmit` then `npx vitest run`
Expected: both clean.

- [ ] **Step 8: Manual smoke check (dev build, moderator identity)**

> - Right-click a channel row in the selected community (power ≥ 50) → Rename / Delete menu appears; both open the hoisted dialogs.
> - Right-click a channel row in a NON-selected community → no menu.
> - The "＋ add channel" row appears under the selected community only (power ≥ 50); clicking opens the create dialog; creating selects the new channel.
> - Demote yourself (or view as member, power < 50) → menu + ＋ row disappear; an open menu closes.

- [ ] **Step 9: Commit**

```bash
git add src/lib/components/AddChannelNavRow.svelte src/lib/components/NavNodeRow.svelte src/lib/components/NavTree.svelte src/lib/components/NavPanel.svelte src/App.svelte src/lib/components/CommunityView.svelte src/lib/components/ChannelSubSidebar.svelte
git commit -m "feat(nav): hoist channel management to nav rows + App dialogs (ZEB-663)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc"
```

---

## Task 5: Retire `ChannelSubSidebar` — CommunityView 2-col, drop Channels tab

**Files:**
- Delete: `src/lib/components/ChannelSubSidebar.svelte`
- Modify: `src/lib/components/CommunityView.svelte` (remove ChannelSubSidebar; `.three-cols`→2-col; drop Channels view-tab + `onSelectChannel` prop)

**Interfaces:**
- Consumes: nothing new. Selection is fully via nav channel rows (`openCommunityChannel`).
- Produces: `CommunityView` no longer takes `onSelectChannel` (removed); keeps `selectedChannelId`.

- [ ] **Step 1: Remove ChannelSubSidebar from CommunityView**

In `src/lib/components/CommunityView.svelte`:

**(a)** Delete the import `import ChannelSubSidebar from './ChannelSubSidebar.svelte';` (~line 10).

**(b)** Delete the `<ChannelSubSidebar ... />` element (~line 475–483).

**(c)** Remove the `onSelectChannel` prop (from `$props()` destructure and type block) — it's now unused (selection is nav-driven). Keep `selectedChannelId`.

- [ ] **Step 2: Collapse `.three-cols` to 2 columns**

The `.three-cols` container (~line 474) now holds only the feed branch + `<ChannelMembersPanel>`. Rename the class to `.two-cols` (or keep the name) and confirm the CSS still lays out feed | members. Update the CSS rule (~line 752):

```css
  .two-cols {
    display: flex;
    flex: 1;
    min-height: 0;
  }
```

Update the `<div class="three-cols">` to `<div class="two-cols">`.

- [ ] **Step 3: Drop the "Channels" view-tab**

In the `view-tabs` nav (~line 385–414), delete the Channels `<button>` (~line 386–392). The Proposals / Constitutional / Charter tabs remain. Channels are reached by clicking a nav channel row (which sets `communityActiveView = 'channels'`).

> Verify the feed still renders: the `{:else if activeChannel}` branch (~line 517) fires when `activeView === 'channels'` and a channel is selected — unchanged. The Channels tab was only a way to switch back to `activeView === 'channels'`; nav channel-row clicks now do that via `openCommunityChannel`.

- [ ] **Step 4: Delete the ChannelSubSidebar file**

```bash
git rm src/lib/components/ChannelSubSidebar.svelte
```

> There is no `ChannelSubSidebar.test.ts` to delete (confirmed absent).

- [ ] **Step 5: Run the full frontend gate**

Run: `npx tsc --noEmit` then `npx vitest run`
Expected: both clean. tsc confirms no lingering ChannelSubSidebar / `onSelectChannel` references.

- [ ] **Step 6: Manual smoke check**

> - The per-community channel rail is gone; channels appear only in the unified nav tree.
> - CommunityView shows feed | members (2 columns).
> - No "Channels" tab; clicking a nav channel row shows its feed; Proposals/Constitutional/Charter tabs still work.

- [ ] **Step 7: Commit**

```bash
git add src/lib/components/CommunityView.svelte
git commit -m "feat(nav): retire ChannelSubSidebar; CommunityView 2-col (ZEB-663)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc"
```

---

## Task 6: Mentions revert — per-channel clear only + final sweep

**Files:**
- Modify: `src/App.svelte` (remove the ZEB-662 community-open `clearMention`)
- Modify: `src/lib/nav-service.test.ts` (update the ZEB-662 production-aggregate test comment/expectation if needed)

**Interfaces:**
- Consumes: nothing new. `incMention` keeps its channel-else-community fallback (boot-race safety net — retained).
- Produces: no new API. Behavior: clearing is per-channel (on channel-row select, done in Task 3's `openCommunityChannel` + resolution effect); the community rollup decrements naturally.

- [ ] **Step 1: Remove the community-open aggregate clear**

In `src/App.svelte`, in `handleNodeClick`'s community branch (~line 2807–2817), delete the `navService.clearMention(id);` call and its comment (~line 2810–2813). The branch becomes:

```typescript
    if (node.type === 'community') {
      changeSelectedCommunity(id);
      void refreshCommunityMembers(id);
      if (appMode !== 'messages') {
        switchMode('messages');
      }
      return;
    }
```

> Why: clearing the community node directly zeros its aggregate without touching child channel counts → breaks the sum invariant. Per-channel clear (Task 3) is the correct model; the resolution effect clears the channel you land on when opening a community, so opening a community still clears the viewed channel's mentions.

- [ ] **Step 2: Reconcile the ZEB-662 unit test**

In `src/lib/nav-service.test.ts`, the existing test `'production: channel is not a nav node → the community node carries the badge'` (~line 825) calls `s.clearMention('c1')` **directly on the service** — that tests the `clearMention` method (still valid; the method is unchanged). No change is required to keep it green. If its narrative comment ("opening the community clears its aggregate") now reads as stale product behavior, update the comment to: `// clearMention on a community node still zeroes its aggregate (method-level; the community-open CALL was removed in ZEB-663).` Leave the assertions as-is.

- [ ] **Step 3: Run the full frontend gate**

Run: `npx tsc --noEmit` then `npx vitest run`
Expected: both clean.

- [ ] **Step 4: Full CI-parity sweep**

Run the complete frontend gate one final time from a clean state, plus confirm no stray console usage regressions:

Run: `npx tsc --noEmit && npx vitest run`
Expected: both clean (this is the CI `frontend` job in full).

> Note: this is a frontend-only change; the Rust `rust-check`/`rust-test`/`msrv` CI jobs are unaffected. No `cargo` run is required locally for this branch, but CI runs all four in parallel.

- [ ] **Step 5: Manual smoke check — mentions**

> With two identities in one community:
> - B @-mentions A in #general while A is viewing #random (focused): A's nav shows `@1` on the #general row and the rollup on the collapsed community row.
> - A clicks #general → its badge clears; the community rollup decrements.
> - B @-mentions A in a channel of a community A hasn't opened this session (channels not yet in nav for a just-joined community): the badge lands on the community node (boot-race fallback) and is not lost.

- [ ] **Step 6: Commit**

```bash
git add src/App.svelte src/lib/nav-service.test.ts
git commit -m "feat(nav): per-channel mention clear; drop community-open aggregate clear (ZEB-663)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc"
```

---

## Self-Review (author checklist — completed)

**1. Spec coverage:**
- Data core `NavService.setChannels` + `channelKind` + community-remove children → Task 1 ✓
- `ChannelNavSync` bridge + App wiring + eager boot → Task 2 ✓
- Selection `openCommunityChannel` + active highlight + CommunityView consumes selection → Task 3 ✓
- Management migration (hoist dialogs, nav context-menu + ＋, power-gating, §6.8 demotion guard) → Task 4 ✓
- CommunityView retirement (ChannelSubSidebar deleted, 2-col, drop Channels tab) → Task 5 ✓
- Mentions reconciliation (per-channel clear; remove community-open clear; incMention fallback retained) → Task 6 ✓
- Edge cases: delete-active fallback (Task 3 resolution effect), `listChannels` failure (Task 2 per-community try/catch), community removed (Task 1), demotion guard (Task 4), boot-race residual (Task 6 fallback retained) ✓
- Non-goals honored: no general unread, no governance-in-nav ✓

**2. Placeholder scan:** No TBD/TODO/"handle edge cases"; every code step shows complete code.

**3. Type consistency:** `setChannels(communityId, channels)`, `channelKind?: 'text'|'voice'`, `openCommunityChannel(communityId, channelId)`, `canManageChannels(communityId)`, `onRenameChannel(communityId, channelId)`, `onDeleteChannel(communityId, channelId)`, `onAddChannel(communityId)` — used identically across App/NavPanel/NavTree/NavNodeRow.

**4. Deliberate deviations from the spec (flagged for the reviewer):**
- Spec's `ChannelNavSync.onCommunityAdded(communityId)` is realized as `resync` wired into `changeSelectedCommunity` (single choke-point covering all join paths) — Task 2 design note.
- `setChannels` fires `onChange()` **only when the reconcile changes the tree** (spec said "once") — avoids redundant re-renders on idempotent resync-on-switch.
- CommunityView keeps its own `listChannels` feed list (per spec) refreshed via the existing chained `onChannelConfigChanged`; App owns selection state and drives the active-channel fallback from the nav children.
