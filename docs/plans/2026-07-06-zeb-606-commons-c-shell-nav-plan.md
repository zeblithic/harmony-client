# ZEB-606 Commons C: Shell & Nav Restyle + Assembly Rail — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restyle the left nav to the Commons design language (section headers, unified active states, community letter chips, proposals rows with live count badges), add a community-scoped Assembly rail as a third occupant of the messages-mode right column, and add an identity chip + live connection-status strip to the nav footer.

**Architecture:** Frontend-only (zero Rust changes; `src-tauri/` diff must be empty; `Layout.svelte` diff must be empty). A new `ProposalCountService` (mirrors `presence-service.ts`) feeds nav badges; a new `MessagesRail.svelte` host inside App's existing `mediaFeed` snippet swaps `AssemblyRail` vs the existing `MediaFeed`; `CommunityView.activeView` becomes `$bindable` so the nav row and rail can deep-link to the Proposals view. Spec: `docs/specs/2026-07-06-zeb-606-commons-c-shell-nav-design.md` (commit 0fa03c35).

**Tech Stack:** Svelte 5 (runes: `$state`/`$derived`/`$props`/`$bindable`/`$effect`), TypeScript, Vitest + @testing-library/svelte, Tauri IPC.

## Global Constraints

- Frontend gates run from the **repo root**: `npx tsc --noEmit && npx vitest run`. No cargo gates (no Rust changes — verify `git diff --stat main -- src-tauri/` stays empty).
- `src/style-token-guard.test.ts` forbids raw color literals in Svelte `<style>` blocks — **all styles below use `var(--…)` tokens only**. NEVER regenerate `src/style-token-allowlist.json`.
- `src/commons-hex-guard.test.ts` forbids the eight Discord hexes anywhere under `src/` — nothing below introduces hex at all.
- Tier-2 voting DTOs (`Tier2ProposalExport`) are **snake_case on the wire** (`proposal_id`, `community_id`, `lifecycle`, `total_conviction_ms` as decimal string → `BigInt`). Event payloads (`VotingTier2ProposalCreatedPayload` etc.) are **camelCase** (`communityId`). Do not "fix" either spelling.
- Tauri error extraction: `e instanceof Error ? e.message : String(e)`.
- Preserve pinned contracts: ZEB-569 gear `class:active` + `aria-pressed`; ZEB-600 `.nav-presence-dot`; `Show/Hide media panel` aria-labels (Layout untouched); NavPanel placeholder `"Search"`, FAB `Create new`, More-menu testids, mode-button names.
- ONE commit per task; commit from repo root; no worktrees. Branch: `zeb-606-commons-c-shell-nav`.

## File Structure

| File | Task | Responsibility |
| --- | --- | --- |
| `src/lib/proposal-count-service.ts` (new) | 1 | Lazy per-community active-Tier-2-proposal counts, event-refreshed |
| `src/lib/proposal-count-service.test.ts` (new) | 1 | Service unit tests (mock VotingAdapter with multi-handler arrays) |
| `src/lib/components/NavTree.svelte` | 2, 3 | `filterTop` prop (2); proposals-row injection + prop threading (3) |
| `src/lib/components/NavPanel.svelte` | 2, 3, 5 | Section headers + active tokens (2); proposal props (3); footer chips (5) |
| `src/lib/components/NavNodeRow.svelte` | 2 | Community letter chip; `.nav-row.active` tokens |
| `src/lib/components/ProposalsNavRow.svelte` (new) | 3 | Synthetic ⚖ proposals row + `--gov-clay` count badge |
| `src/lib/components/CommunityView.svelte` | 3 | `activeView` becomes `$bindable` prop |
| `src/lib/media-panel-prefs.ts` (+ its test) | 4 | `RailTab` pref (`harmony-rail-tab`) |
| `src/lib/components/AssemblyRail.svelte` (new) | 4 | Live proposal-card list for one community |
| `src/lib/components/MessagesRail.svelte` (new) | 4 | Tab host: Assembly ⚖ / Media inside the right-rail cell |
| `src/lib/components/ConnectionStatusChip.svelte` (new) | 5 | `● connected · N peers` via network-health-adapter |
| `src/lib/components/IdentityChip.svelte` (new) | 5 | Initials avatar + presence ring + `● self-sovereign` |
| `src/App.svelte` | 3, 4, 5 | Service singleton + deep-link + snippet swap + footer props |
| Tests: `__tests__/NavPanel.test.ts` (2,3,5), `__tests__/CommunityView.test.ts` (3), `__tests__/AssemblyRail.test.ts` + `__tests__/MessagesRail.test.ts` (4, new), `__tests__/ConnectionStatusChip.test.ts` + `__tests__/IdentityChip.test.ts` (5, new) | | |

---

### Task 1: ProposalCountService

**Files:**
- Create: `src/lib/proposal-count-service.ts`
- Create: `src/lib/proposal-count-service.test.ts`

**Interfaces:**
- Consumes: `VotingAdapter` (`src/lib/voting-adapter.ts`) — `listTier2Proposals(communityId): Promise<Tier2ProposalExport[]>`, `subscribeProposalCreated/subscribeThresholdReached/subscribeThresholdReverted/subscribeProposalFinalized(handler): () => void` (multi-subscriber; unsubscribe closures). `Tier2ProposalExport.lifecycle: 'Open' | 'ThresholdReached' | 'Finalized' | 'Archived'` (snake_case DTO, `src/lib/types/voting.ts:224`).
- Produces (Task 3 relies on these exact names): `class ProposalCountService` with `connectAdapter(adapter: VotingAdapter): void`, `ensure(communityId: string): void`, `countFor(communityId: string): number | undefined`, `disconnect(): void`, `version: number`, `onChange?: () => void`.

- [ ] **Step 1: Write the failing test**

Create `src/lib/proposal-count-service.test.ts`:

```typescript
import { describe, expect, it, vi } from 'vitest';
import { ProposalCountService } from './proposal-count-service';
import type { VotingAdapter } from './voting-adapter';
import type { Tier2ProposalExport } from './types/voting';

/** Snake_case Tier-2 fixture (wire-realistic — see types/voting.ts:224). */
function makeProposal(overrides: Partial<Tier2ProposalExport> = {}): Tier2ProposalExport {
  return {
    proposal_id: 'aa'.repeat(32),
    community_id: 'c1',
    proposal_text: 'Fix the fountain',
    lifecycle: 'Open',
    total_conviction_ms: '0',
    threshold_conviction_ms: '1000',
    half_life_seconds: 3600,
    auto_exec: { kk: 'n' },
    total_supply: 10,
    voter_count: 0,
    ...overrides,
  };
}

/** Multi-handler mock — VotingAdapter is multi-subscriber, so the shared
 *  createMockAdapter (single handler slot per event) is insufficient here.
 *  Mirrors the inline makeMockAdapter idiom in voting-adapter-tier3.test.ts. */
function makeVotingMock() {
  const created: Array<(p: { proposalId: string; communityId: string }) => void> = [];
  const reached: Array<(p: { communityId: string; proposalId: string; thresholdReachedAtMs: number }) => void> = [];
  const reverted: Array<(p: { communityId: string; proposalId: string; revertedAtMs: number }) => void> = [];
  const finalized: Array<(p: { communityId: string; proposalId: string }) => void> = [];
  const listTier2Proposals = vi.fn(async (_cid: string): Promise<Tier2ProposalExport[]> => []);
  const adapter = {
    listTier2Proposals,
    subscribeProposalCreated: (h: (typeof created)[number]) => {
      created.push(h);
      return () => created.splice(created.indexOf(h), 1);
    },
    subscribeThresholdReached: (h: (typeof reached)[number]) => {
      reached.push(h);
      return () => reached.splice(reached.indexOf(h), 1);
    },
    subscribeThresholdReverted: (h: (typeof reverted)[number]) => {
      reverted.push(h);
      return () => reverted.splice(reverted.indexOf(h), 1);
    },
    subscribeProposalFinalized: (h: (typeof finalized)[number]) => {
      finalized.push(h);
      return () => finalized.splice(finalized.indexOf(h), 1);
    },
  } as unknown as VotingAdapter;
  return {
    adapter,
    listTier2Proposals,
    emitCreated: (p: { proposalId: string; communityId: string }) => [...created].forEach((h) => h(p)),
    emitFinalized: (p: { communityId: string; proposalId: string }) => [...finalized].forEach((h) => h(p)),
    counts: { created, reached, reverted, finalized },
  };
}

/** Flush pending microtasks (service refetches are fire-and-forget). */
const flush = () => new Promise<void>((r) => setTimeout(r, 0));

describe('ProposalCountService', () => {
  it('ensure() lazily fetches and counts only Open + ThresholdReached', async () => {
    const { adapter, listTier2Proposals } = makeVotingMock();
    listTier2Proposals.mockResolvedValue([
      makeProposal({ proposal_id: 'p1', lifecycle: 'Open' }),
      makeProposal({ proposal_id: 'p2', lifecycle: 'ThresholdReached' }),
      makeProposal({ proposal_id: 'p3', lifecycle: 'Finalized' }),
      makeProposal({ proposal_id: 'p4', lifecycle: 'Archived' }),
    ]);
    const svc = new ProposalCountService();
    svc.connectAdapter(adapter);
    expect(svc.countFor('c1')).toBeUndefined();
    svc.ensure('c1');
    await flush();
    expect(svc.countFor('c1')).toBe(2);
    expect(listTier2Proposals).toHaveBeenCalledTimes(1);
  });

  it('ensure() is idempotent (no duplicate IPC while loaded or loading)', async () => {
    const { adapter, listTier2Proposals } = makeVotingMock();
    const svc = new ProposalCountService();
    svc.connectAdapter(adapter);
    svc.ensure('c1');
    svc.ensure('c1'); // in-flight
    await flush();
    svc.ensure('c1'); // loaded
    await flush();
    expect(listTier2Proposals).toHaveBeenCalledTimes(1);
  });

  it('lifecycle events refetch the affected community and fire onChange', async () => {
    const { adapter, listTier2Proposals, emitCreated } = makeVotingMock();
    listTier2Proposals.mockResolvedValue([makeProposal({ lifecycle: 'Open' })]);
    const svc = new ProposalCountService();
    svc.connectAdapter(adapter);
    const onChange = vi.fn();
    svc.onChange = onChange;
    svc.ensure('c1');
    await flush();
    expect(svc.countFor('c1')).toBe(1);
    const v0 = svc.version;
    listTier2Proposals.mockResolvedValue([
      makeProposal({ proposal_id: 'p1', lifecycle: 'Open' }),
      makeProposal({ proposal_id: 'p2', lifecycle: 'Open' }),
    ]);
    emitCreated({ proposalId: 'p2', communityId: 'c1' });
    await flush();
    expect(svc.countFor('c1')).toBe(2);
    expect(svc.version).toBeGreaterThan(v0);
    expect(onChange).toHaveBeenCalled();
  });

  it('a stale slow fetch cannot clobber a newer event-driven refetch', async () => {
    const { adapter, listTier2Proposals, emitFinalized } = makeVotingMock();
    let releaseFirst!: (v: Tier2ProposalExport[]) => void;
    const first = new Promise<Tier2ProposalExport[]>((r) => (releaseFirst = r));
    listTier2Proposals.mockReturnValueOnce(first); // slow initial load
    const svc = new ProposalCountService();
    svc.connectAdapter(adapter);
    svc.ensure('c1');
    // Event fires while the first fetch hangs; its refetch resolves first.
    listTier2Proposals.mockResolvedValue([makeProposal({ lifecycle: 'Open' })]);
    emitFinalized({ communityId: 'c1', proposalId: 'p9' });
    await flush();
    expect(svc.countFor('c1')).toBe(1);
    // Now the stale first fetch lands with 3 actives — must be dropped.
    releaseFirst([
      makeProposal({ proposal_id: 'p1' }),
      makeProposal({ proposal_id: 'p2' }),
      makeProposal({ proposal_id: 'p3' }),
    ]);
    await flush();
    expect(svc.countFor('c1')).toBe(1);
  });

  it('fetch errors leave the count undefined and allow a later ensure() retry', async () => {
    const { adapter, listTier2Proposals } = makeVotingMock();
    listTier2Proposals.mockRejectedValueOnce('boom');
    const svc = new ProposalCountService();
    svc.connectAdapter(adapter);
    svc.ensure('c1');
    await flush();
    expect(svc.countFor('c1')).toBeUndefined();
    listTier2Proposals.mockResolvedValue([makeProposal()]);
    svc.ensure('c1'); // retry allowed after a failed first load
    await flush();
    expect(svc.countFor('c1')).toBe(1);
  });

  it('disconnect() unsubscribes all four event handlers', () => {
    const mock = makeVotingMock();
    const svc = new ProposalCountService();
    svc.connectAdapter(mock.adapter);
    expect(mock.counts.created.length + mock.counts.reached.length + mock.counts.reverted.length + mock.counts.finalized.length).toBe(4);
    svc.disconnect();
    expect(mock.counts.created.length + mock.counts.reached.length + mock.counts.reverted.length + mock.counts.finalized.length).toBe(0);
  });
});
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `npx vitest run src/lib/proposal-count-service.test.ts`
Expected: FAIL — cannot resolve `./proposal-count-service`.

- [ ] **Step 3: Write the implementation**

Create `src/lib/proposal-count-service.ts`:

```typescript
import type { VotingAdapter } from './voting-adapter';

/**
 * ZEB-606: per-community count of ACTIVE Tier-2 conviction proposals
 * (`lifecycle ∈ {Open, ThresholdReached}`) for the nav proposals-row badge.
 *
 * Mirrors the PresenceService shape: a plain class the App owns, with a
 * `version` counter + `onChange` callback for Svelte invalidation (App bumps
 * a $state mirror in onChange; the nav resolver reads that mirror). Counts
 * load lazily via {@link ensure} — one IPC per community, communities are
 * few — and stay fresh via the four Tier-2 lifecycle events, each of which
 * refetches only the affected community (payloads carry `communityId`).
 *
 * NOT used by the Assembly rail: the rail holds full proposal lists itself;
 * the two stay consistent because both refetch on the same events.
 */
export class ProposalCountService {
  private counts = new Map<string, number>();
  /** communityId → monotonically increasing load token (stale-fetch guard). */
  private tokens = new Map<string, number>();
  private adapter: VotingAdapter | null = null;
  private unsubs: Array<() => void> = [];
  /** Bumped on every count change (presence-service version idiom). */
  version = 0;
  /** App-installed notifier — bumps a $state counter for reactivity. */
  onChange?: () => void;

  /** Wire the (possibly still-connecting) VotingAdapter. Idempotent. */
  connectAdapter(adapter: VotingAdapter): void {
    if (this.adapter) return;
    this.adapter = adapter;
    const refresh = (p: { communityId: string }) => {
      void this.refetch(p.communityId);
    };
    this.unsubs.push(
      adapter.subscribeProposalCreated(refresh),
      adapter.subscribeThresholdReached(refresh),
      adapter.subscribeThresholdReverted(refresh),
      adapter.subscribeProposalFinalized(refresh),
    );
  }

  /** Lazily fetch the count for `communityId`. No-op while a load is in
   *  flight or after one has succeeded (events keep it fresh from there). */
  ensure(communityId: string): void {
    if (!this.adapter) return;
    if (this.counts.has(communityId) || this.tokens.has(communityId)) return;
    void this.refetch(communityId);
  }

  /** Current active-proposal count, or undefined before the first
   *  successful fetch (callers render no badge for undefined). */
  countFor(communityId: string): number | undefined {
    return this.counts.get(communityId);
  }

  /** Tear down all event subscriptions (App unmount). */
  disconnect(): void {
    for (const u of this.unsubs) u();
    this.unsubs = [];
    this.adapter = null;
  }

  private async refetch(communityId: string): Promise<void> {
    if (!this.adapter) return;
    const token = (this.tokens.get(communityId) ?? 0) + 1;
    this.tokens.set(communityId, token);
    try {
      const list = await this.adapter.listTier2Proposals(communityId);
      if (this.tokens.get(communityId) !== token) return; // superseded
      const count = list.filter(
        (p) => p.lifecycle === 'Open' || p.lifecycle === 'ThresholdReached',
      ).length;
      if (this.counts.get(communityId) !== count) {
        this.counts.set(communityId, count);
        this.version += 1;
        this.onChange?.();
      }
    } catch (e) {
      if (this.tokens.get(communityId) !== token) return;
      // Badge is best-effort; log and leave the count unset/stale. If this
      // was the FIRST load (no count yet), clear the token so a later
      // ensure() may retry — otherwise a boot-window failure would pin the
      // badge to "unknown" forever.
      if (!this.counts.has(communityId)) this.tokens.delete(communityId);
      console.warn(
        `[zeb-606] proposal-count fetch failed for ${communityId}:`,
        e instanceof Error ? e.message : String(e),
      );
    }
  }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `npx vitest run src/lib/proposal-count-service.test.ts`
Expected: PASS (6 tests).

- [ ] **Step 5: Full gates + commit**

```bash
npx tsc --noEmit && npx vitest run
git add src/lib/proposal-count-service.ts src/lib/proposal-count-service.test.ts
git commit -m "ZEB-606 T1: ProposalCountService — lazy per-community active Tier-2 counts, event-refreshed"
```

---

### Task 2: Nav Commons restyle — section headers, active tokens, community letter chip

**Files:**
- Modify: `src/lib/components/NavTree.svelte` (add root-only `filterTop` prop)
- Modify: `src/lib/components/NavPanel.svelte` (partitioned tree render + section headers + active-state CSS)
- Modify: `src/lib/components/NavNodeRow.svelte` (community chip; `.nav-row.active` tokens)
- Modify: `src/lib/components/__tests__/NavPanel.test.ts` (🏛️ assertion → chip; new section-header tests)

**Interfaces:**
- Produces: NavTree prop `filterTop?: (n: NavNode) => boolean` (applied ONLY at the level it is passed — recursive calls don't thread it, so it is root-only by construction). CSS classes `.nav-section-header` (NavPanel), `.community-chip` (NavNodeRow). Task 3 renders `ProposalsNavRow` next to these rows and reuses the `--primary-soft`/`--primary-deep` active idiom.
- Behavioral invariants preserved: ZEB-569 gear `class:active`+`aria-pressed` (colors change, mechanism doesn't); ZEB-600 presence dots; search filtering (`filteredNodes`) runs BEFORE partitioning; `.color-band` ancestry stripes; empty-state CTA; collapsed icon rail.

- [ ] **Step 1: Write the failing tests (lockstep updates)**

In `src/lib/components/__tests__/NavPanel.test.ts`, replace the 🏛️ test (currently ~line 401, `it('renders a community-kind node with its name and 🏛️ icon', …)`):

```typescript
    it('renders a community-kind node with its name and a letter chip (ZEB-606)', () => {
      const { container } = render(NavPanel, { props: { nodes: communityNodes, collapsed: false } });
      expect(screen.getByText('IPFS Crew')).toBeTruthy();
      expect(container.textContent).not.toContain('🏛️');
      expect(container.querySelector('.community-chip')?.textContent?.trim()).toBe('I');
    });
```

Add a new describe block (top level, after the community-node describe):

```typescript
describe('Section headers (ZEB-606)', () => {
  const base = { expanded: false, unreadCount: 0, unreadLevel: 'none' as const };
  const mixedNodes: NavNode[] = [
    { id: 'work', parentId: null, type: 'folder', name: 'Work', ...base, expanded: true, lastActivity: 3 },
    { id: 'comm-1', parentId: null, type: 'community', name: 'IPFS Crew', ...base, lastActivity: 2 },
    { id: 'dm-1', parentId: null, type: 'dm', name: 'alice', ...base, lastActivity: 1 },
  ];

  it('shows Communities and Direct messages headers when those groups exist', () => {
    render(NavPanel, { props: { nodes: mixedNodes, collapsed: false } });
    expect(screen.getByText('Communities')).toBeTruthy();
    expect(screen.getByText('Direct messages')).toBeTruthy();
  });

  it('omits headers for empty groups', () => {
    render(NavPanel, { props: { nodes: [mixedNodes[0]], collapsed: false } });
    expect(screen.queryByText('Communities')).toBeNull();
    expect(screen.queryByText('Direct messages')).toBeNull();
  });

  it('renders un-headed folder trees before the Communities section', () => {
    const { container } = render(NavPanel, { props: { nodes: mixedNodes, collapsed: false } });
    const text = container.querySelector('.nav-tree-container')?.textContent ?? '';
    expect(text.indexOf('Work')).toBeGreaterThanOrEqual(0);
    expect(text.indexOf('Work')).toBeLessThan(text.indexOf('Communities'));
  });

  it('group-chat nodes land under Direct messages', () => {
    const nodes: NavNode[] = [
      { id: 'g1', parentId: null, type: 'group-chat', name: 'weekend crew', ...base, lastActivity: 1 },
    ];
    render(NavPanel, { props: { nodes, collapsed: false } });
    expect(screen.getByText('Direct messages')).toBeTruthy();
    expect(screen.queryByText('Communities')).toBeNull();
  });
});
```

- [ ] **Step 2: Run to verify the new assertions fail**

Run: `npx vitest run src/lib/components/__tests__/NavPanel.test.ts`
Expected: FAIL — 🏛️ still rendered; no section headers.

- [ ] **Step 3: NavTree — add root-only `filterTop`**

In `src/lib/components/NavTree.svelte`, extend the props destructure + type (after `presenceOnline`):

```typescript
    presenceOnline,
    filterTop,
  }: {
    …
    presenceOnline?: (node: NavNode) => boolean;
    /** ZEB-606: keep only matching nodes at THIS level. Passed by NavPanel at
     *  the root to partition top-level nodes into headed sections; recursive
     *  calls below do not thread it, so descendants are never filtered. */
    filterTop?: (n: NavNode) => boolean;
  } = $props();
```

And apply it in `sortedChildren`:

```typescript
  let sortedChildren = $derived.by(() => {
    const children = getChildNodes(nodes, parentId);
    const kept = filterTop ? children.filter(filterTop) : children;
    const order = parentId ? getInheritedSortOrder(nodes, parentId) : 'activity';
    return sortNodes(kept, order);
  });
```

(The recursive `<NavTree …>` call at the bottom of the file is left exactly as-is — it does not pass `filterTop`.)

- [ ] **Step 4: NavPanel — partitioned render + section headers + active-state tokens**

In `src/lib/components/NavPanel.svelte` script, after the `hasCommunities` derived (~line 239), add:

```typescript
  // ZEB-606: render-time partition of TOP-LEVEL nodes into Commons sections.
  // NavService data and the NavNode DTO are untouched; search filtering
  // (filteredNodes) runs before partitioning so hits stay sectioned.
  const isDmNode = (n: NavNode) => n.type === 'dm' || n.type === 'group-chat';
  const isCommunityNode = (n: NavNode) => n.type === 'community';
  const isUnheadedTop = (n: NavNode) => !isCommunityNode(n) && !isDmNode(n);
  let filteredTop = $derived(getChildNodes(filteredNodes, null));
  let hasUnheadedTop = $derived(filteredTop.some(isUnheadedTop));
  let hasCommunitiesTop = $derived(filteredTop.some(isCommunityNode));
  let hasDmsTop = $derived(filteredTop.some(isDmNode));
```

Replace the single `<NavTree …/>` render (lines 300–310) with three sectioned renders:

```svelte
        {#if hasUnheadedTop}
          <NavTree
            nodes={filteredNodes}
            parentId={null}
            filterTop={isUnheadedTop}
            {activeNodeId}
            onToggle={toggleFolder}
            onClick={onNodeClick}
            onDisplayModeChange={changeDisplayMode}
            onSortOrderChange={changeSortOrder}
            {profileLookup}
            {presenceOnline}
          />
        {/if}
        {#if hasCommunitiesTop}
          <div class="nav-section-header">Communities</div>
          <NavTree
            nodes={filteredNodes}
            parentId={null}
            filterTop={isCommunityNode}
            {activeNodeId}
            onToggle={toggleFolder}
            onClick={onNodeClick}
            onDisplayModeChange={changeDisplayMode}
            onSortOrderChange={changeSortOrder}
            {profileLookup}
            {presenceOnline}
          />
        {/if}
        {#if hasDmsTop}
          <div class="nav-section-header">Direct messages</div>
          <NavTree
            nodes={filteredNodes}
            parentId={null}
            filterTop={isDmNode}
            {activeNodeId}
            onToggle={toggleFolder}
            onClick={onNodeClick}
            onDisplayModeChange={changeDisplayMode}
            onSortOrderChange={changeSortOrder}
            {profileLookup}
            {presenceOnline}
          />
        {/if}
```

In the `<style>` block, add (near `.nav-tree-container`):

```css
  /* ZEB-606: Commons section headers — uppercase micro-labels. */
  .nav-section-header {
    padding: 10px 12px 4px;
    font-size: 10.5px;
    font-weight: 600;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--text-faint);
    user-select: none;
  }
```

Active-state unification (same `<style>` block — value-only changes):

```css
  /* was: background: var(--accent); color: var(--text-primary); */
  .settings-btn.active {
    background: var(--primary-soft);
    color: var(--primary-deep);
    border-radius: 4px;
  }
```

```css
  /* was: background: var(--accent); color: var(--text-primary); */
  .notes-nav-row.active { background: var(--primary-soft); color: var(--primary-deep); }
```

```css
  /* was: background: var(--accent); color: var(--text-primary); */
  .mode-toggle.active {
    background: var(--primary-soft);
    color: var(--primary-deep);
  }
```

And the header search input picks up the Commons surface (value-only):

```css
  .search-input {
    width: 100%;
    padding: 6px 10px;
    border: none;
    border-radius: 4px;
    background: var(--surface);
    color: var(--text-primary);
    font-size: 13px;
    outline: none;
  }
```

(FAB, empty-state CTA, and unread badges intentionally KEEP `--accent` — action buttons/badges, not active states. Spec §1.3.)

- [ ] **Step 5: NavNodeRow — community letter chip + active tokens**

In `src/lib/components/NavNodeRow.svelte`:

1. Remove the community case from `typeIcon` (it becomes unreachable):

```typescript
  function typeIcon(n: NavNode): string {
    if (n.type === 'channel') return '#';
    if (n.type === 'dm' || n.type === 'group-chat') return '@';
    if (n.type === 'folder') return n.expanded ? '▾' : '▸';
    return '';
  }
```

2. In the text/both-mode markup, replace the single type-icon span (line 155, `<span class="type-icon">{typeIcon(node)}</span>`) with a community-conditional chip (the chevron button block above it stays exactly as-is):

```svelte
      {#if node.type === 'community'}
        <span class="community-chip" aria-hidden="true">{node.name.charAt(0).toUpperCase()}</span>
      {:else}
        <span class="type-icon">{typeIcon(node)}</span>
      {/if}
```

3. In `<style>`, change `.nav-row.active` (value-only) and add `.community-chip`:

```css
  .nav-row.active {
    background: var(--primary-soft);
    color: var(--primary-deep);
  }

  /* ZEB-606: Commons community letter chip — replaces the 🏛️ type icon. */
  .community-chip {
    flex-shrink: 0;
    width: 20px;
    height: 20px;
    border-radius: 6px;
    background: var(--accent);
    color: var(--text-bright);
    font-size: 11px;
    font-weight: 700;
    display: flex;
    align-items: center;
    justify-content: center;
  }
```

- [ ] **Step 6: Run the nav test files**

Run: `npx vitest run src/lib/components/__tests__/NavPanel.test.ts src/lib/components/__tests__/NavNodeRow.test.ts src/lib/components/__tests__/NavTree.test.ts`
Expected: PASS (NavNodeRow/NavTree tests don't pin 🏛️ or the changed colors; NavTree order tests exercise a single homogeneous level and are unaffected by root partitioning).

- [ ] **Step 7: Full gates + commit**

```bash
npx tsc --noEmit && npx vitest run
git add src/lib/components/NavTree.svelte src/lib/components/NavPanel.svelte src/lib/components/NavNodeRow.svelte src/lib/components/__tests__/NavPanel.test.ts
git commit -m "ZEB-606 T2: nav Commons restyle — section headers, primary-soft actives, community letter chip"
```

---

### Task 3: Proposals nav row + deep-link into CommunityView

**Files:**
- Create: `src/lib/components/ProposalsNavRow.svelte`
- Modify: `src/lib/components/NavTree.svelte` (thread 3 props; inject row after expanded community subtree)
- Modify: `src/lib/components/NavPanel.svelte` (thread 3 props)
- Modify: `src/lib/components/CommunityView.svelte` (`activeView` → `$bindable` prop)
- Modify: `src/App.svelte` (service singleton, ensure-effect, `openCommunityProposals`, `bind:activeView`, NavPanel props, view reset on community switch)
- Modify: `src/lib/components/__tests__/NavPanel.test.ts`, `src/lib/components/__tests__/CommunityView.test.ts`

**Interfaces:**
- Consumes: `ProposalCountService` (Task 1 — exact API above); NavTree `filterTop`/threading pattern (Task 2); `VotingAdapter` at `src/App.svelte:1027` (`const votingAdapter = new VotingAdapter()`); its connect block at `src/App.svelte:1749-1761`; `changeSelectedCommunity(id: string | null)` at `src/App.svelte:1083`; `switchMode(mode: AppMode)` at ~`src/App.svelte:2667`; `let navNodes = $state([...navService.nodes])` at `src/App.svelte:1453`.
- Produces (Task 4 relies on): `openCommunityProposals(communityId: string): void` in App; `let communityActiveView = $state<'channels' | 'proposals' | 'tier3'>('channels')` in App; CommunityView prop `activeView?: 'channels' | 'proposals' | 'tier3'` ($bindable, default `'channels'`). NavPanel props `proposalCount?: (node: NavNode) => number | undefined`, `onSelectProposals?: (communityId: string) => void`, `proposalsActiveFor?: string | null`.

- [ ] **Step 1: Write the failing NavPanel tests**

Add to `src/lib/components/__tests__/NavPanel.test.ts` (new top-level describe):

```typescript
describe('Proposals nav row (ZEB-606)', () => {
  const base = { unreadCount: 0, unreadLevel: 'none' as const };
  const expandedCommunity: NavNode[] = [
    { id: 'comm-1', parentId: null, type: 'community', name: 'IPFS Crew', expanded: true, ...base, lastActivity: 2 },
    { id: 'chan-1', parentId: 'comm-1', type: 'channel', name: 'general', expanded: false, ...base, lastActivity: 1 },
  ];

  it('renders the row with a mono count badge inside an expanded community', () => {
    const { container } = render(NavPanel, {
      props: {
        nodes: expandedCommunity,
        collapsed: false,
        proposalCount: () => 3,
        onSelectProposals: vi.fn(),
      },
    });
    const row = container.querySelector('[data-testid="proposals-row-comm-1"]');
    expect(row).toBeTruthy();
    expect(row?.textContent).toContain('proposals');
    expect(row?.querySelector('.count-badge')?.textContent).toBe('3');
  });

  it('shows no badge for zero or unknown counts (row still renders)', () => {
    const { container } = render(NavPanel, {
      props: {
        nodes: expandedCommunity,
        collapsed: false,
        proposalCount: () => 0,
        onSelectProposals: vi.fn(),
      },
    });
    const row = container.querySelector('[data-testid="proposals-row-comm-1"]');
    expect(row).toBeTruthy();
    expect(row?.querySelector('.count-badge')).toBeNull();
  });

  it('clicking the row fires onSelectProposals with the community id', async () => {
    const onSelectProposals = vi.fn();
    const { container } = render(NavPanel, {
      props: { nodes: expandedCommunity, collapsed: false, proposalCount: () => 1, onSelectProposals },
    });
    await fireEvent.click(container.querySelector('[data-testid="proposals-row-comm-1"]')!);
    expect(onSelectProposals).toHaveBeenCalledWith('comm-1');
  });

  it('is active when proposalsActiveFor matches the community', () => {
    const { container } = render(NavPanel, {
      props: {
        nodes: expandedCommunity,
        collapsed: false,
        proposalCount: () => 1,
        onSelectProposals: vi.fn(),
        proposalsActiveFor: 'comm-1',
      },
    });
    expect(container.querySelector('[data-testid="proposals-row-comm-1"]')?.classList.contains('active')).toBe(true);
  });

  it('renders no row without the resolver (no votingAdapter contexts)', () => {
    const { container } = render(NavPanel, { props: { nodes: expandedCommunity, collapsed: false } });
    expect(container.querySelector('[data-testid="proposals-row-comm-1"]')).toBeNull();
  });

  it('renders no row for a collapsed community', () => {
    const collapsedNodes = [{ ...expandedCommunity[0], expanded: false }];
    const { container } = render(NavPanel, {
      props: { nodes: collapsedNodes, collapsed: false, proposalCount: () => 1, onSelectProposals: vi.fn() },
    });
    expect(container.querySelector('[data-testid="proposals-row-comm-1"]')).toBeNull();
  });
});
```

- [ ] **Step 2: Run to verify failure**

Run: `npx vitest run src/lib/components/__tests__/NavPanel.test.ts`
Expected: FAIL — no proposals row rendered.

- [ ] **Step 3: Create `src/lib/components/ProposalsNavRow.svelte`**

```svelte
<script lang="ts">
  /**
   * ZEB-606: synthetic "proposals" nav row rendered by NavTree inside each
   * expanded community — NOT a NavNode (NavService never sees it). Mirrors
   * NavNodeRow's row anatomy (16px icon cell + 6px gap, 4px-per-ancestor
   * indent) so it aligns with sibling channel rows, and its keyboard model
   * (role="button", Enter/Space activate).
   */
  let {
    communityId,
    indent = 0,
    count,
    active = false,
    onSelect,
  }: {
    communityId: string;
    /** Folder-ancestry depth of sibling channel rows (community rows are not
     *  folders, so children share the community's own ancestry length). */
    indent?: number;
    /** Active Tier-2 proposal count; undefined = not yet known (no badge). */
    count: number | undefined;
    active?: boolean;
    onSelect?: () => void;
  } = $props();

  let paddingLeft = $derived(indent * 4 + 8);

  function activate(e: MouseEvent | KeyboardEvent) {
    e.stopPropagation();
    onSelect?.();
  }
</script>

<div
  class="proposals-row"
  class:active
  role="button"
  tabindex="0"
  data-testid="proposals-row-{communityId}"
  onclick={activate}
  onkeydown={(e) => { if (e.key === 'Enter' || e.key === ' ') activate(e); }}
>
  <span class="row-content" style="padding-left: {paddingLeft}px">
    <span class="gov-glyph" aria-hidden="true">⚖</span>
    <span class="row-label">proposals</span>
    {#if count !== undefined && count > 0}
      <span class="count-badge" aria-label="{count} open proposals">{count}</span>
    {/if}
  </span>
</div>

<style>
  .proposals-row {
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
  .proposals-row:hover {
    background: var(--bg-tertiary);
    color: var(--text-primary);
  }
  .proposals-row.active {
    background: var(--primary-soft);
    color: var(--primary-deep);
  }
  .row-content {
    display: flex;
    align-items: center;
    gap: 6px;
    flex: 1;
    min-width: 0;
  }
  .gov-glyph {
    flex-shrink: 0;
    width: 16px;
    text-align: center;
    color: var(--gov-clay);
  }
  .row-label {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .count-badge {
    background: var(--gov-clay);
    color: var(--text-bright);
    font-family: var(--font-mono);
    font-size: 10px;
    font-weight: 700;
    padding: 1px 6px;
    border-radius: 9px;
    flex-shrink: 0;
    margin-right: 8px;
  }
</style>
```

- [ ] **Step 4: NavTree — thread props + inject the row**

In `src/lib/components/NavTree.svelte`:

1. Import: `import ProposalsNavRow from './ProposalsNavRow.svelte';`
2. Extend props (after `filterTop` from Task 2):

```typescript
    filterTop,
    proposalCount,
    onSelectProposals,
    proposalsActiveFor,
  }: {
    …
    filterTop?: (n: NavNode) => boolean;
    /** ZEB-606: active Tier-2 count resolver (undefined = feature absent). */
    proposalCount?: (node: NavNode) => number | undefined;
    /** ZEB-606: open the community's Proposals view. */
    onSelectProposals?: (communityId: string) => void;
    /** ZEB-606: community id whose Proposals view is currently open. */
    proposalsActiveFor?: string | null;
  } = $props();
```

3. Replace the recursion block (lines 58–60) with:

```svelte
  {#if (child.type === 'folder' || child.type === 'community') && child.expanded}
    <NavTree nodes={nodes} parentId={child.id} {activeNodeId} {onToggle} {onClick} {onDisplayModeChange} {onSortOrderChange} {profileLookup} {presenceOnline} {proposalCount} {onSelectProposals} {proposalsActiveFor} />
    {#if child.type === 'community' && proposalCount && onSelectProposals}
      <ProposalsNavRow
        communityId={child.id}
        indent={ancestry.length}
        count={proposalCount(child)}
        active={proposalsActiveFor === child.id}
        onSelect={() => onSelectProposals(child.id)}
      />
    {/if}
  {/if}
```

- [ ] **Step 5: NavPanel — thread the three props**

In `src/lib/components/NavPanel.svelte`, add to the props destructure + type (after `onOpenDocs`):

```typescript
    onOpenDocs,
    proposalCount,
    onSelectProposals,
    proposalsActiveFor = null,
  }: {
    …
    onOpenDocs?: () => void;
    /** ZEB-606: active Tier-2 proposal count resolver for community rows. */
    proposalCount?: (node: NavNode) => number | undefined;
    /** ZEB-606: open a community's Proposals view (nav proposals row). */
    onSelectProposals?: (communityId: string) => void;
    /** ZEB-606: community id whose Proposals view is open (row active state). */
    proposalsActiveFor?: string | null;
  } = $props();
```

Pass them into ALL THREE sectioned `<NavTree …/>` renders from Task 2 (add `{proposalCount} {onSelectProposals} {proposalsActiveFor}` alongside `{presenceOnline}` in each).

- [ ] **Step 6: CommunityView — `activeView` becomes `$bindable`**

In `src/lib/components/CommunityView.svelte`:

1. Add to the props destructure (after `onBeforeVoiceJoin`) and type:

```typescript
    onBeforeVoiceJoin,
    activeView = $bindable('channels'),
  }: {
    …
    onBeforeVoiceJoin?: () => Promise<void>;
    /** ZEB-606: which middle-column view is active. Bindable so App can
     *  deep-link (nav proposals row / Assembly rail "View all"). Default
     *  'channels' preserves the ZEB-291 behavior for non-binding parents. */
    activeView?: 'channels' | 'proposals' | 'tier3';
  } = $props();
```

2. DELETE the internal state (currently ~line 132):

```typescript
  /** ZEB-291 Phase 2: which middle-column view is active. Default is
   *  'channels' (chat-native, current behavior); 'proposals' switches
   *  to the Tier 2 governance panel. Only togglable when a
   *  votingAdapter is provided. */
  let activeView = $state<'channels' | 'proposals' | 'tier3'>('channels');
```

(The tab buttons' `onclick={() => { activeView = 'proposals'; }}` assignments work unchanged against the bindable prop.)

- [ ] **Step 7: CommunityView test — external drive**

Add to `src/lib/components/__tests__/CommunityView.test.ts` (inside the main describe, using the existing `setup`/`makeAdapter` helpers):

```typescript
  it('activeView is externally drivable to proposals (ZEB-606 deep-link)', async () => {
    const votingHost = makeAdapter();
    (votingHost.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'voting_list_tier2_proposals') return Promise.resolve([]);
      if (cmd === 'voting_get_my_delegate') return Promise.resolve(null);
      return Promise.resolve(undefined);
    });
    const votingAdapter = new VotingAdapter();
    await votingAdapter.connectAdapter(votingHost);
    const { container } = await setup([general, announcements], {
      votingAdapter,
      activeView: 'proposals',
    });
    await waitFor(() => {
      expect(container.querySelector('.community-proposals')).toBeTruthy();
    });
  });
```

- [ ] **Step 8: App wiring**

In `src/App.svelte`:

1. Import (with the other lib imports): `import { ProposalCountService } from './lib/proposal-count-service';`
2. Next to `const votingAdapter = new VotingAdapter();` (line 1027), add:

```typescript
  // ZEB-606: nav proposals-row badge counts. onChange bumps the $state
  // mirror so the NavPanel resolver re-reads (presenceVersion idiom).
  const proposalCountService = new ProposalCountService();
  let proposalCountVersion = $state(0);
  // ZEB-606: App-held mirror of CommunityView's middle-column view, bound
  // via bind:activeView so the nav proposals row / Assembly rail can
  // deep-link and show an active state.
  let communityActiveView = $state<'channels' | 'proposals' | 'tier3'>('channels');
```

3. In the votingAdapter connect `.then` block (lines 1749–1758), wire the service after the toast handler:

```typescript
      void votingAdapter
        .connectAdapter(adapter)
        .then(() => {
          // Tear down any prior toast subscription before registering a new
          // one — prevents duplicate toasts if connectAdapter is ever called
          // twice (e.g. a future reconnect path).
          toastUnsubscribe?.();
          toastUnsubscribe = setupDelegateOnBehalfToast(votingAdapter);
          // ZEB-606: badge counts subscribe to the same Tier-2 events.
          proposalCountService.onChange = () => {
            proposalCountVersion += 1;
          };
          proposalCountService.connectAdapter(votingAdapter);
        })
        .catch((err) => {
          // Amended in PR #408 R1 (Qodo rule 571712): normalize the IPC
          // rejection instead of logging the raw value.
          console.warn(
            '[harmony-client] votingAdapter connect failed:',
            err instanceof Error ? err.message : String(err),
          );
        });
```

4. Near the other top-level `$effect`s (e.g. directly after the `let navNodes = $state([...navService.nodes]);` wiring at line 1453), add:

```typescript
  // ZEB-606: lazily load Tier-2 proposal counts for every community in the
  // nav (one IPC each on first sight; events keep them fresh afterwards).
  $effect(() => {
    for (const n of navNodes) {
      if (n.type === 'community') proposalCountService.ensure(n.id);
    }
  });
```

5. In `changeSelectedCommunity` (line 1083), inside the `if (selectedCommunityId !== id) {` block (first line), add:

```typescript
      // ZEB-606: a community switch always lands on Channels unless a
      // deep-link (openCommunityProposals) overrides it afterwards.
      communityActiveView = 'channels';
```

6. Below `changeSelectedCommunity`, add:

```typescript
  /** ZEB-606: nav proposals-row / Assembly-rail deep link — select the
   *  community and land on its Proposals view. */
  function openCommunityProposals(communityId: string) {
    if (appMode !== 'messages') switchMode('messages');
    changeSelectedCommunity(communityId);
    void refreshCommunityMembers(communityId);
    showSettings = false;
    communityActiveView = 'proposals';
  }
```

7. NavPanel mount (~line 3028): add after `presenceOnline={…}`:

```svelte
        proposalCount={(node) => {
          // Reading proposalCountVersion registers the reactive dependency.
          void proposalCountVersion;
          return proposalCountService.countFor(node.id);
        }}
        onSelectProposals={openCommunityProposals}
        proposalsActiveFor={communityActiveView === 'proposals' ? selectedCommunityId : null}
```

8. CommunityView mount (~line 3092): add `bind:activeView={communityActiveView}` after `{votingAdapter}`.

- [ ] **Step 9: Run the touched test files**

Run: `npx vitest run src/lib/components/__tests__/NavPanel.test.ts src/lib/components/__tests__/CommunityView.test.ts src/lib/components/__tests__/NavTree.test.ts`
Expected: PASS.

- [ ] **Step 10: Full gates + commit**

```bash
npx tsc --noEmit && npx vitest run
git add src/lib/components/ProposalsNavRow.svelte src/lib/components/NavTree.svelte src/lib/components/NavPanel.svelte src/lib/components/CommunityView.svelte src/App.svelte src/lib/components/__tests__/NavPanel.test.ts src/lib/components/__tests__/CommunityView.test.ts
git commit -m "ZEB-606 T3: proposals nav row + count badges + bindable CommunityView deep-link"
```

---

### Task 4: Assembly rail — MessagesRail host + AssemblyRail + rail-tab pref

**Files:**
- Modify: `src/lib/media-panel-prefs.ts` + `src/lib/media-panel-prefs.test.ts`
- Create: `src/lib/components/AssemblyRail.svelte` + `src/lib/components/__tests__/AssemblyRail.test.ts`
- Create: `src/lib/components/MessagesRail.svelte` + `src/lib/components/__tests__/MessagesRail.test.ts`
- Modify: `src/App.svelte` (mediaFeed snippet swaps `MediaFeed` → `MessagesRail`)

**Interfaces:**
- Consumes: `openCommunityProposals(communityId)` (Task 3); `ConvictionProposalCard` props `{ communityId, proposal, adapter, myDelegate?, delegateName? }`; `MediaFeed` props `{ messages, trustService, trustVersion?, threadMessageIds?, onLinkBack?, onAvatarClick?, onTrustChange? }`; App's existing snippet-local values `mediaMessages`, `trustService`, `trustVersion`, `threadMessageIds`, `scrollToMessage`, `handleAvatarClick`, `handleTrustChange`, `selectedCommunityNode`, `votingAdapter`.
- Produces: `type RailTab = 'assembly' | 'media'`, `loadRailTab(): RailTab`, `saveRailTab(tab: RailTab): void` in `media-panel-prefs.ts`. Components `AssemblyRail` (`{ communityId, adapter, onViewAllProposals? }`) and `MessagesRail` (props below).
- **`Layout.svelte` must show zero diff** — the rail cell's content is App's `mediaFeed` snippet; tabs live inside it. Pinned `Show/Hide media panel` aria-labels and `Layout.test.ts` stay untouched.

- [ ] **Step 1: Rail-tab pref — failing test**

Append to `src/lib/media-panel-prefs.test.ts` (inside the top-level describe):

```typescript
  describe('rail tab preference (ZEB-606)', () => {
    it('defaults to assembly when nothing is stored', () => {
      expect(loadRailTab()).toBe('assembly');
    });

    it('round-trips media', () => {
      saveRailTab('media');
      expect(loadRailTab()).toBe('media');
      saveRailTab('assembly');
      expect(loadRailTab()).toBe('assembly');
    });

    it('treats garbage as the assembly default', () => {
      localStorage.setItem('harmony-rail-tab', 'blurple');
      expect(loadRailTab()).toBe('assembly');
    });
  });
```

And extend the import at the top of the test file with `loadRailTab, saveRailTab`.

- [ ] **Step 2: Implement the pref**

Append to `src/lib/media-panel-prefs.ts`:

```typescript
const RAIL_TAB_KEY = 'harmony-rail-tab';

/** ZEB-606: which right-rail tab is selected in messages mode. */
export type RailTab = 'assembly' | 'media';

/** Last-selected rail tab. Defaults to 'assembly' (the design's "always one
 *  glance away"); any non-'media' stored value degrades to the default. */
export function loadRailTab(): RailTab {
  try {
    return localStorage.getItem(RAIL_TAB_KEY) === 'media' ? 'media' : 'assembly';
  } catch {
    return 'assembly';
  }
}

/** Persist the rail-tab choice. No-op if localStorage is unavailable. */
export function saveRailTab(tab: RailTab): void {
  try {
    localStorage.setItem(RAIL_TAB_KEY, tab);
  } catch {
    // localStorage unavailable — non-fatal.
  }
}
```

Run: `npx vitest run src/lib/media-panel-prefs.test.ts` — expected PASS.

- [ ] **Step 3: AssemblyRail — failing test**

Create `src/lib/components/__tests__/AssemblyRail.test.ts`:

```typescript
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import AssemblyRail from '../AssemblyRail.svelte';
import type { VotingAdapter } from '../../voting-adapter';
import type { Tier2ProposalExport } from '../../types/voting';

function makeProposal(overrides: Partial<Tier2ProposalExport> = {}): Tier2ProposalExport {
  return {
    proposal_id: 'aa'.repeat(32),
    community_id: 'c1',
    proposal_text: 'Fix the fountain',
    lifecycle: 'Open',
    total_conviction_ms: '100',
    threshold_conviction_ms: '1000',
    half_life_seconds: 3600,
    auto_exec: { kk: 'n' },
    total_supply: 10,
    voter_count: 1,
    ...overrides,
  };
}

/** Multi-handler voting mock (VotingAdapter is multi-subscriber). */
function makeVotingMock(initial: Tier2ProposalExport[]) {
  let listResult = initial;
  type H<T> = Array<(p: T) => void>;
  const created: H<{ proposalId: string; communityId: string }> = [];
  const reached: H<{ communityId: string; proposalId: string; thresholdReachedAtMs: number }> = [];
  const reverted: H<{ communityId: string; proposalId: string; revertedAtMs: number }> = [];
  const finalized: H<{ communityId: string; proposalId: string }> = [];
  const listTier2Proposals = vi.fn(async (_cid: string) => listResult);
  const adapter = {
    listTier2Proposals,
    subscribeProposalCreated: (h: (typeof created)[number]) => { created.push(h); return () => created.splice(created.indexOf(h), 1); },
    subscribeThresholdReached: (h: (typeof reached)[number]) => { reached.push(h); return () => reached.splice(reached.indexOf(h), 1); },
    subscribeThresholdReverted: (h: (typeof reverted)[number]) => { reverted.push(h); return () => reverted.splice(reverted.indexOf(h), 1); },
    subscribeProposalFinalized: (h: (typeof finalized)[number]) => { finalized.push(h); return () => finalized.splice(finalized.indexOf(h), 1); },
  } as unknown as VotingAdapter;
  return {
    adapter,
    listTier2Proposals,
    setList: (l: Tier2ProposalExport[]) => { listResult = l; },
    emitCreated: (p: { proposalId: string; communityId: string }) => [...created].forEach((h) => h(p)),
    handlerCount: () => created.length + reached.length + reverted.length + finalized.length,
  };
}

describe('AssemblyRail (ZEB-606)', () => {
  it('renders active proposals, ThresholdReached first then conviction desc', async () => {
    const { adapter } = makeVotingMock([
      makeProposal({ proposal_id: 'p-low', proposal_text: 'Low conviction', lifecycle: 'Open', total_conviction_ms: '10' }),
      makeProposal({ proposal_id: 'p-arch', proposal_text: 'Archived one', lifecycle: 'Archived' }),
      makeProposal({ proposal_id: 'p-thresh', proposal_text: 'Crossed threshold', lifecycle: 'ThresholdReached', total_conviction_ms: '5' }),
      makeProposal({ proposal_id: 'p-high', proposal_text: 'High conviction', lifecycle: 'Open', total_conviction_ms: '900' }),
    ]);
    const { container } = render(AssemblyRail, { props: { communityId: 'c1', adapter } });
    await waitFor(() => expect(screen.getByText('Crossed threshold')).toBeTruthy());
    expect(screen.queryByText('Archived one')).toBeNull();
    const text = container.textContent ?? '';
    expect(text.indexOf('Crossed threshold')).toBeLessThan(text.indexOf('High conviction'));
    expect(text.indexOf('High conviction')).toBeLessThan(text.indexOf('Low conviction'));
  });

  it('shows the empty state when no proposals are active', async () => {
    const { adapter } = makeVotingMock([makeProposal({ lifecycle: 'Finalized' })]);
    render(AssemblyRail, { props: { communityId: 'c1', adapter } });
    await waitFor(() => expect(screen.getByText('No open proposals')).toBeTruthy());
  });

  it('fires onViewAllProposals from the footer link', async () => {
    const { adapter } = makeVotingMock([]);
    const onViewAllProposals = vi.fn();
    render(AssemblyRail, { props: { communityId: 'c1', adapter, onViewAllProposals } });
    await waitFor(() => expect(screen.getByText('View all proposals →')).toBeTruthy());
    await fireEvent.click(screen.getByText('View all proposals →'));
    expect(onViewAllProposals).toHaveBeenCalledTimes(1);
  });

  it('refetches on a matching lifecycle event and ignores other communities', async () => {
    const mock = makeVotingMock([]);
    render(AssemblyRail, { props: { communityId: 'c1', adapter: mock.adapter } });
    await waitFor(() => expect(mock.listTier2Proposals).toHaveBeenCalledTimes(1));
    mock.setList([makeProposal({ proposal_text: 'Fresh proposal' })]);
    mock.emitCreated({ proposalId: 'px', communityId: 'other' });
    await new Promise((r) => setTimeout(r, 0));
    expect(mock.listTier2Proposals).toHaveBeenCalledTimes(1);
    mock.emitCreated({ proposalId: 'px', communityId: 'c1' });
    await waitFor(() => expect(screen.getByText('Fresh proposal')).toBeTruthy());
  });

  it('unsubscribes all handlers on destroy', async () => {
    const mock = makeVotingMock([]);
    const { unmount } = render(AssemblyRail, { props: { communityId: 'c1', adapter: mock.adapter } });
    await waitFor(() => expect(mock.handlerCount()).toBe(4));
    unmount();
    expect(mock.handlerCount()).toBe(0);
  });
});
```

Run: `npx vitest run src/lib/components/__tests__/AssemblyRail.test.ts` — expected FAIL (component missing).

- [ ] **Step 4: Create `src/lib/components/AssemblyRail.svelte`**

```svelte
<script lang="ts">
  /**
   * ZEB-606: the Assembly rail — a compact, live list of ACTIVE Tier-2
   * proposals for one community, mounted in the messages-mode right rail.
   *
   * Lifecycle copies the proven CommunityProposalsPanel pattern: an $effect
   * keyed on communityId resets state, fetches, subscribes the four Tier-2
   * lifecycle events (filtered by communityId), and cleans up with a
   * cancelled flag + unsubscribes. A monotonic load token drops superseded
   * fetch results (community-switch race). Signal-cast events deliberately
   * do NOT refetch — ConvictionProposalCard handles its own optimistic
   * state and a refetch here would race it and flicker (ZEB-291 tradeoff).
   */
  import type { Tier2ProposalExport } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';
  import ConvictionProposalCard from './ConvictionProposalCard.svelte';

  let {
    communityId,
    adapter,
    onViewAllProposals,
  }: {
    /** Hex SpaceId of the community whose assembly this shows. */
    communityId: string;
    /** Voting IPC adapter (connected or connecting — a pre-connect fetch
     *  rejects and surfaces as the error state; the next lifecycle event
     *  refetches). */
    adapter: VotingAdapter;
    /** "View all proposals →" — App routes this to the Proposals view. */
    onViewAllProposals?: () => void;
  } = $props();

  let proposals = $state<Tier2ProposalExport[] | null>(null);
  let loadError = $state<string | null>(null);
  /** Monotonic; superseded loads drop their results (community switch). */
  let latestLoadToken = 0;

  async function refetch(cid: string) {
    const token = ++latestLoadToken;
    try {
      const list = await adapter.listTier2Proposals(cid);
      if (token !== latestLoadToken) return;
      proposals = list;
      loadError = null;
    } catch (e) {
      if (token !== latestLoadToken) return;
      loadError = e instanceof Error ? e.message : String(e);
    }
  }

  $effect(() => {
    const cid = communityId;
    let cancelled = false;
    proposals = null;
    loadError = null;
    void refetch(cid);
    const unsubs = [
      adapter.subscribeProposalCreated((p) => {
        if (!cancelled && p.communityId === cid) void refetch(cid);
      }),
      adapter.subscribeThresholdReached((p) => {
        if (!cancelled && p.communityId === cid) void refetch(cid);
      }),
      adapter.subscribeThresholdReverted((p) => {
        if (!cancelled && p.communityId === cid) void refetch(cid);
      }),
      adapter.subscribeProposalFinalized((p) => {
        if (!cancelled && p.communityId === cid) void refetch(cid);
      }),
    ];
    return () => {
      cancelled = true;
      for (const u of unsubs) u();
    };
  });

  /** Active proposals only: ThresholdReached first (closest to execution),
   *  then by total conviction descending (BigInt — Q96.32 decimal strings
   *  routinely exceed Number.MAX_SAFE_INTEGER). */
  let activeProposals = $derived.by(() => {
    if (proposals === null) return null;
    return proposals
      .filter((p) => p.lifecycle === 'Open' || p.lifecycle === 'ThresholdReached')
      .slice()
      .sort((a, b) => {
        if (a.lifecycle !== b.lifecycle) {
          return a.lifecycle === 'ThresholdReached' ? -1 : 1;
        }
        const d = BigInt(b.total_conviction_ms) - BigInt(a.total_conviction_ms);
        return d > 0n ? 1 : d < 0n ? -1 : 0;
      });
  });
</script>

<div class="assembly-rail" aria-label="Assembly">
  <h3 class="assembly-title">Assembly</h3>
  {#if loadError}
    <p class="assembly-error">{loadError}</p>
  {:else if activeProposals === null}
    <p class="assembly-empty">Loading proposals…</p>
  {:else if activeProposals.length === 0}
    <p class="assembly-empty">No open proposals</p>
  {:else}
    <div class="assembly-cards">
      {#each activeProposals as proposal (proposal.proposal_id)}
        <ConvictionProposalCard {communityId} {proposal} {adapter} />
      {/each}
    </div>
  {/if}
  <button type="button" class="view-all" onclick={() => onViewAllProposals?.()}>
    View all proposals →
  </button>
</div>

<style>
  .assembly-rail {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .assembly-title {
    margin: 0;
    font-family: var(--font-display);
    font-size: 16px;
    font-weight: 600;
    color: var(--text-primary);
  }
  .assembly-cards {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .assembly-empty {
    margin: 0;
    color: var(--text-muted);
    font-size: 13px;
    text-align: center;
    padding: 24px 8px;
  }
  .assembly-error {
    margin: 0;
    color: var(--danger);
    font-size: 12px;
  }
  .view-all {
    border: none;
    background: none;
    color: var(--gov-clay);
    font-size: 13px;
    cursor: pointer;
    text-align: left;
    padding: 4px 0;
  }
  .view-all:hover {
    text-decoration: underline;
  }
</style>
```

Run: `npx vitest run src/lib/components/__tests__/AssemblyRail.test.ts` — expected PASS.

- [ ] **Step 5: MessagesRail — failing test**

Create `src/lib/components/__tests__/MessagesRail.test.ts`:

```typescript
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import MessagesRail from '../MessagesRail.svelte';
import { TrustService } from '../../trust-service';
import type { VotingAdapter } from '../../voting-adapter';
import type { Message } from '../../types';

beforeEach(() => {
  localStorage.clear();
});

const noMessages: Message[] = [];

/** Minimal voting stub — the rail only needs list + the 4 subscribes. */
function makeVotingStub(): VotingAdapter {
  return {
    listTier2Proposals: vi.fn(async () => []),
    subscribeProposalCreated: () => () => {},
    subscribeThresholdReached: () => () => {},
    subscribeThresholdReverted: () => () => {},
    subscribeProposalFinalized: () => () => {},
  } as unknown as VotingAdapter;
}

function baseProps(overrides: Record<string, unknown> = {}) {
  return {
    communityId: 'c1',
    votingAdapter: makeVotingStub(),
    onViewAllProposals: vi.fn(),
    messages: noMessages,
    trustService: new TrustService(),
    ...overrides,
  };
}

describe('MessagesRail (ZEB-606)', () => {
  it('defaults to the Assembly tab when a community is active', async () => {
    render(MessagesRail, { props: baseProps() });
    expect(screen.getByRole('tab', { name: '⚖ Assembly' })).toBeTruthy();
    await waitFor(() => expect(screen.getByText('No open proposals')).toBeTruthy());
    expect(screen.queryByText('No media yet')).toBeNull();
  });

  it('switches to Media and persists the choice', async () => {
    render(MessagesRail, { props: baseProps() });
    await fireEvent.click(screen.getByRole('tab', { name: 'Media' }));
    expect(screen.getByText('No media yet')).toBeTruthy();
    expect(localStorage.getItem('harmony-rail-tab')).toBe('media');
  });

  it('honors a persisted media preference on mount', () => {
    localStorage.setItem('harmony-rail-tab', 'media');
    render(MessagesRail, { props: baseProps() });
    expect(screen.getByText('No media yet')).toBeTruthy();
  });

  it('renders media-only (no tabs) without a community', () => {
    render(MessagesRail, { props: baseProps({ communityId: null }) });
    expect(screen.queryByRole('tab', { name: '⚖ Assembly' })).toBeNull();
    expect(screen.getByText('No media yet')).toBeTruthy();
  });

  it('renders media-only (no tabs) without a votingAdapter', () => {
    render(MessagesRail, { props: baseProps({ votingAdapter: undefined }) });
    expect(screen.queryByRole('tab', { name: '⚖ Assembly' })).toBeNull();
    expect(screen.getByText('No media yet')).toBeTruthy();
  });

  it('routes View-all through onViewAllProposals with the community id', async () => {
    const onViewAllProposals = vi.fn();
    render(MessagesRail, { props: baseProps({ onViewAllProposals }) });
    await waitFor(() => expect(screen.getByText('View all proposals →')).toBeTruthy());
    await fireEvent.click(screen.getByText('View all proposals →'));
    expect(onViewAllProposals).toHaveBeenCalledWith('c1');
  });
});
```

Run: `npx vitest run src/lib/components/__tests__/MessagesRail.test.ts` — expected FAIL.

- [ ] **Step 6: Create `src/lib/components/MessagesRail.svelte`**

```svelte
<script lang="ts">
  /**
   * ZEB-606: right-rail host for messages mode — a third occupant of the
   * existing Layout media cell, implemented entirely App-side so
   * Layout.svelte (resize/collapse/prefs/aria contract) is untouched.
   *
   * When a community is active AND a votingAdapter exists, a two-tab header
   * swaps AssemblyRail vs the existing MediaFeed; the last choice persists
   * device-scoped (harmony-rail-tab). Outside community contexts (DMs, no
   * selection) the rail is media-only with no tab chrome — pixel-identical
   * to the pre-ZEB-606 experience.
   */
  import type { Message } from '../types';
  import type { TrustService } from '../trust-service';
  import type { VotingAdapter } from '../voting-adapter';
  import { loadRailTab, saveRailTab, type RailTab } from '../media-panel-prefs';
  import AssemblyRail from './AssemblyRail.svelte';
  import MediaFeed from './MediaFeed.svelte';

  let {
    communityId = null,
    votingAdapter,
    onViewAllProposals,
    messages,
    trustService,
    trustVersion = 0,
    threadMessageIds = new Set<string>(),
    onLinkBack,
    onAvatarClick,
    onTrustChange,
  }: {
    /** Active community, or null outside community contexts (media-only). */
    communityId?: string | null;
    votingAdapter?: VotingAdapter;
    /** Deep-link to the community's Proposals view ("View all"). */
    onViewAllProposals?: (communityId: string) => void;
    /* MediaFeed pass-through (same contract as before ZEB-606): */
    messages: Message[];
    trustService: TrustService;
    trustVersion?: number;
    threadMessageIds?: Set<string>;
    onLinkBack?: (messageId: string) => void;
    onAvatarClick?: (address: string, event: MouseEvent) => void;
    onTrustChange?: () => void;
  } = $props();

  let railTab = $state<RailTab>(loadRailTab());
  let assemblyAvailable = $derived(communityId != null && votingAdapter != null);
  let showAssembly = $derived(assemblyAvailable && railTab === 'assembly');

  function selectTab(tab: RailTab) {
    railTab = tab;
    saveRailTab(tab);
  }
</script>

{#if assemblyAvailable}
  <div class="rail-tabs" role="tablist" aria-label="Right rail content">
    <button
      type="button"
      role="tab"
      class="rail-tab"
      class:active={railTab === 'assembly'}
      aria-selected={railTab === 'assembly'}
      onclick={() => selectTab('assembly')}
    >⚖ Assembly</button>
    <button
      type="button"
      role="tab"
      class="rail-tab"
      class:active={railTab === 'media'}
      aria-selected={railTab === 'media'}
      onclick={() => selectTab('media')}
    >Media</button>
  </div>
{/if}
{#if showAssembly && communityId != null && votingAdapter}
  <AssemblyRail
    {communityId}
    adapter={votingAdapter}
    onViewAllProposals={() => onViewAllProposals?.(communityId)}
  />
{:else}
  <MediaFeed {messages} {trustService} {trustVersion} {threadMessageIds} {onLinkBack} {onAvatarClick} {onTrustChange} />
{/if}

<style>
  .rail-tabs {
    display: inline-flex;
    border: 1px solid var(--border);
    border-radius: 6px;
    overflow: hidden;
    font-size: 0.75rem;
    margin-bottom: 10px;
    align-self: flex-start;
  }
  .rail-tab {
    padding: 4px 12px;
    border: none;
    background: none;
    cursor: pointer;
    color: var(--text-secondary);
    user-select: none;
  }
  .rail-tab:not(:last-child) {
    border-right: 1px solid var(--border);
  }
  .rail-tab.active {
    background: var(--primary-soft);
    color: var(--primary-deep);
  }
</style>
```

Run: `npx vitest run src/lib/components/__tests__/MessagesRail.test.ts` — expected PASS.

- [ ] **Step 7: App snippet swap**

In `src/App.svelte`:

1. Replace the import `import MediaFeed from './lib/components/MediaFeed.svelte';` (line 18) with `import MessagesRail from './lib/components/MessagesRail.svelte';`.
2. Replace the `mediaFeed` snippet body (lines ~3246–3256):

```svelte
  {#snippet mediaFeed()}
    <MessagesRail
      communityId={selectedCommunityNode?.id ?? null}
      {votingAdapter}
      onViewAllProposals={openCommunityProposals}
      messages={mediaMessages}
      {trustService}
      {trustVersion}
      onLinkBack={scrollToMessage}
      onAvatarClick={handleAvatarClick}
      onTrustChange={handleTrustChange}
      {threadMessageIds}
    />
  {/snippet}
```

3. Verify Layout is untouched: `git diff --stat main -- src/lib/components/Layout.svelte` prints nothing.

- [ ] **Step 8: Full gates + commit**

```bash
npx tsc --noEmit && npx vitest run
git add src/lib/media-panel-prefs.ts src/lib/media-panel-prefs.test.ts src/lib/components/AssemblyRail.svelte src/lib/components/MessagesRail.svelte src/lib/components/__tests__/AssemblyRail.test.ts src/lib/components/__tests__/MessagesRail.test.ts src/App.svelte
git commit -m "ZEB-606 T4: Assembly rail — MessagesRail tab host + AssemblyRail + rail-tab pref (Layout untouched)"
```

---

### Task 5: Nav footer — IdentityChip + ConnectionStatusChip

**Files:**
- Create: `src/lib/components/IdentityChip.svelte` + `src/lib/components/__tests__/IdentityChip.test.ts`
- Create: `src/lib/components/ConnectionStatusChip.svelte` + `src/lib/components/__tests__/ConnectionStatusChip.test.ts`
- Modify: `src/lib/components/NavPanel.svelte` (footer mount + 2 props)
- Modify: `src/App.svelte` (identity prop wiring)
- Modify: `src/lib/components/__tests__/NavPanel.test.ts` (chip presence/absence)

**Interfaces:**
- Consumes: `network-health-adapter.ts` — `snapshot(): Promise<NetworkHealthSnapshot>`, `onNetworkHealthChanged(cb): Promise<UnlistenFn>`; `NetworkHealthSnapshot` (`myNetwork: { reachability: 'reachable'|'degraded'|'unreachable' } | null`, `peers: PeerHealth[]` with `connectionMode: 'direct'|'relay'|'noConnection'|'degraded'`, `transportDisabledReason?: string | null`). App signals: `myProfile` (has `displayName`), `selfOwnerId: string | null`, `presenceVisible: boolean` (App:165), `ownerIdentityState` (`'present'` = self-sovereign, owner-gate.ts:21). Tokens `--net-ok-fg/-bg`, `--net-warn-fg/-bg`, `--net-danger-fg/-bg` (app.css:108–114, both themes).
- Produces: NavPanel props `identity?: { displayName: string; ownerIdHex: string | null; selfOnline: boolean; selfSovereign: boolean }` and `showConnectionStatus?: boolean` (both optional/default-off so bare NavPanel test construction renders no chips and fires no IPC).

- [ ] **Step 1: IdentityChip — failing test**

Create `src/lib/components/__tests__/IdentityChip.test.ts`:

```typescript
import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import IdentityChip from '../IdentityChip.svelte';

describe('IdentityChip (ZEB-606)', () => {
  it('renders two-word initials, name, and the self-sovereign microline', () => {
    const { container } = render(IdentityChip, {
      props: { displayName: 'Jake Englund', ownerIdHex: 'ab'.repeat(16), selfOnline: true, selfSovereign: true },
    });
    expect(container.querySelector('.chip-avatar')?.textContent).toContain('JE');
    expect(screen.getByText('Jake Englund')).toBeTruthy();
    expect(screen.getByText('● self-sovereign')).toBeTruthy();
    expect(container.querySelector('.presence-ring')).toBeTruthy();
  });

  it('single-word names use their first two characters', () => {
    const { container } = render(IdentityChip, {
      props: { displayName: 'zeblith', ownerIdHex: null },
    });
    expect(container.querySelector('.chip-avatar')?.textContent).toContain('ZE');
  });

  it('empty name falls back to the owner id prefix for initials and name', () => {
    const { container } = render(IdentityChip, {
      props: { displayName: '', ownerIdHex: 'deadbeef' + 'ab'.repeat(12) },
    });
    expect(container.querySelector('.chip-avatar')?.textContent).toContain('DE');
    expect(screen.getByText('deadbeef…')).toBeTruthy();
  });

  it('hides the ring and microline when offline / not self-sovereign', () => {
    const { container } = render(IdentityChip, {
      props: { displayName: 'Jake', ownerIdHex: null, selfOnline: false, selfSovereign: false },
    });
    expect(container.querySelector('.presence-ring')).toBeNull();
    expect(screen.queryByText('● self-sovereign')).toBeNull();
  });
});
```

- [ ] **Step 2: Create `src/lib/components/IdentityChip.svelte`**

```svelte
<script lang="ts">
  /**
   * ZEB-606: nav-footer identity chip — initials avatar with a presence
   * ring, display name, and a mono "● self-sovereign" microline when the
   * owner identity is minted and loaded. Purely presentational; App
   * computes every signal (spec §6). The settings gear stays in the nav
   * header (ZEB-569) — this chip carries no actions.
   */
  let {
    displayName,
    ownerIdHex,
    selfOnline = false,
    selfSovereign = false,
  }: {
    displayName: string;
    ownerIdHex: string | null;
    /** Presence ring — App derives this from visibility + identity state. */
    selfOnline?: boolean;
    /** True when ownerIdentityState === 'present'. */
    selfSovereign?: boolean;
  } = $props();

  let initials = $derived.by(() => {
    const parts = displayName.trim().split(/\s+/).filter(Boolean);
    if (parts.length === 0) return (ownerIdHex ?? '??').slice(0, 2).toUpperCase();
    if (parts.length === 1) return parts[0].slice(0, 2).toUpperCase();
    return (parts[0][0] + parts[parts.length - 1][0]).toUpperCase();
  });

  let shownName = $derived(
    displayName.trim() !== ''
      ? displayName
      : ownerIdHex
        ? `${ownerIdHex.slice(0, 8)}…`
        : 'Anonymous',
  );
</script>

<div class="identity-chip" data-testid="identity-chip">
  <span class="chip-avatar" aria-hidden="true">
    {initials}
    {#if selfOnline}
      <span class="presence-ring" role="img" aria-label="Online" title="Online"></span>
    {/if}
  </span>
  <span class="chip-text">
    <span class="chip-name">{shownName}</span>
    {#if selfSovereign}
      <span class="chip-status">● self-sovereign</span>
    {/if}
  </span>
</div>

<style>
  .identity-chip {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 0;
    min-width: 0;
  }
  .chip-avatar {
    position: relative;
    flex-shrink: 0;
    width: 30px;
    height: 30px;
    border-radius: 8px;
    background: var(--accent);
    color: var(--text-bright);
    font-size: 12px;
    font-weight: 700;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .presence-ring {
    position: absolute;
    right: -2px;
    bottom: -2px;
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: var(--presence-online);
    border: 2px solid var(--bg-secondary);
  }
  .chip-text {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .chip-name {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .chip-status {
    font-family: var(--font-mono);
    font-size: 10px;
    color: var(--presence-online);
  }
</style>
```

Run: `npx vitest run src/lib/components/__tests__/IdentityChip.test.ts` — expected PASS.

- [ ] **Step 3: ConnectionStatusChip — failing test**

Create `src/lib/components/__tests__/ConnectionStatusChip.test.ts`:

```typescript
import { render, screen, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('../../network-health-adapter', () => ({
  snapshot: vi.fn(),
  onNetworkHealthChanged: vi.fn(async () => () => {}),
}));

import ConnectionStatusChip from '../ConnectionStatusChip.svelte';
import { snapshot } from '../../network-health-adapter';
import type { NetworkHealthSnapshot, PeerHealth } from '../../types/network-health';

function makePeer(connectionMode: PeerHealth['connectionMode']): PeerHealth {
  return {
    ownerAddr: 'aa'.repeat(16),
    displayName: null,
    sharedCommunities: [],
    connectionMode,
    rttMs: null,
    lastSeenMs: null,
    reachabilityRecordAgeMs: null,
    protocolIncompatReason: null,
  };
}

function makeSnap(overrides: Partial<NetworkHealthSnapshot>): NetworkHealthSnapshot {
  return {
    schemaVersion: 4,
    capturedAtMs: 0,
    appVersion: 'test',
    platform: 'test',
    myNetwork: {
      irohNodeId: 'node',
      reachability: 'reachable',
      natClassification: 'unknown',
      homeRelayUrl: null,
      relayRttMs: null,
      directAddresses: [],
    },
    peers: [],
    pkarrStatus: {} as NetworkHealthSnapshot['pkarrStatus'],
    ...overrides,
  };
}

beforeEach(() => {
  vi.mocked(snapshot).mockReset();
});

describe('ConnectionStatusChip (ZEB-606)', () => {
  it('shows connected with the count of connected peers only', async () => {
    vi.mocked(snapshot).mockResolvedValue(
      makeSnap({ peers: [makePeer('direct'), makePeer('relay'), makePeer('noConnection')] }),
    );
    render(ConnectionStatusChip);
    await waitFor(() => expect(screen.getByText('● connected · 2 peers')).toBeTruthy());
  });

  it('singularizes one peer', async () => {
    vi.mocked(snapshot).mockResolvedValue(makeSnap({ peers: [makePeer('direct')] }));
    render(ConnectionStatusChip);
    await waitFor(() => expect(screen.getByText('● connected · 1 peer')).toBeTruthy());
  });

  it('shows degraded when reachability is degraded', async () => {
    vi.mocked(snapshot).mockResolvedValue(
      makeSnap({
        myNetwork: {
          irohNodeId: 'node',
          reachability: 'degraded',
          natClassification: 'unknown',
          homeRelayUrl: null,
          relayRttMs: null,
          directAddresses: [],
        },
        peers: [makePeer('degraded')],
      }),
    );
    render(ConnectionStatusChip);
    await waitFor(() => expect(screen.getByText('● degraded · 1 peer')).toBeTruthy());
  });

  it('shows offline with a tooltip when the transport is disabled', async () => {
    vi.mocked(snapshot).mockResolvedValue(
      makeSnap({ transportDisabledReason: 'keychain unavailable' }),
    );
    render(ConnectionStatusChip);
    await waitFor(() => expect(screen.getByText('● offline')).toBeTruthy());
    expect(screen.getByText('● offline').getAttribute('title')).toBe('keychain unavailable');
  });

  it('renders nothing while initializing (myNetwork null, transport up)', async () => {
    vi.mocked(snapshot).mockResolvedValue(makeSnap({ myNetwork: null }));
    const { container } = render(ConnectionStatusChip);
    await new Promise((r) => setTimeout(r, 0));
    expect(container.querySelector('.status-chip')).toBeNull();
  });

  it('renders nothing when the snapshot IPC rejects (boot window)', async () => {
    vi.mocked(snapshot).mockRejectedValue('ipc not ready');
    const { container } = render(ConnectionStatusChip);
    await new Promise((r) => setTimeout(r, 0));
    expect(container.querySelector('.status-chip')).toBeNull();
  });
});
```

- [ ] **Step 4: Create `src/lib/components/ConnectionStatusChip.svelte`**

```svelte
<script lang="ts">
  /**
   * ZEB-606: slim mono connection-status strip for the nav footer —
   * "● connected · N peers". The design placed this in the window chrome,
   * but the app uses native decorations (no titlebar exists), so it lives
   * at the bottom of the nav column (spec §0.2/§5).
   *
   * Self-contained: owns its network-health subscription (snapshot +
   * network-health-changed), mirroring NetworkHealthView's race-safe
   * destroyed-flag teardown. Renders nothing until a snapshot with a
   * resolved network arrives (no "offline" flash during boot).
   */
  import { onDestroy, onMount } from 'svelte';
  import { onNetworkHealthChanged, snapshot } from '../network-health-adapter';
  import type { NetworkHealthSnapshot } from '../types/network-health';
  import type { UnlistenFn } from '@tauri-apps/api/event';

  let snap = $state<NetworkHealthSnapshot | null>(null);
  let destroyed = false;
  let unlisten: UnlistenFn | null = null;

  async function refresh() {
    try {
      const s = await snapshot();
      if (!destroyed) snap = s;
    } catch {
      // IPC unavailable (boot window) — keep whatever we had; the next
      // network-health-changed event retries.
    }
  }

  onMount(async () => {
    await refresh();
    try {
      const resolved = await onNetworkHealthChanged(() => {
        void refresh();
      });
      if (destroyed) {
        resolved();
      } else {
        unlisten = resolved;
      }
    } catch (e) {
      console.warn(
        '[zeb-606] status chip subscribe failed:',
        e instanceof Error ? e.message : String(e),
      );
    }
  });

  onDestroy(() => {
    destroyed = true;
    if (unlisten) unlisten();
  });

  let chip = $derived.by((): { kind: 'ok' | 'warn' | 'danger'; text: string; title?: string } | null => {
    if (!snap) return null;
    if (snap.transportDisabledReason) {
      return { kind: 'danger', text: '● offline', title: snap.transportDisabledReason };
    }
    if (!snap.myNetwork) return null; // still initializing — no flash
    const n = snap.peers.filter((p) => p.connectionMode !== 'noConnection').length;
    const peers = `${n} ${n === 1 ? 'peer' : 'peers'}`;
    if (snap.myNetwork.reachability === 'unreachable') {
      return { kind: 'danger', text: '● offline' };
    }
    if (snap.myNetwork.reachability === 'degraded') {
      return { kind: 'warn', text: `● degraded · ${peers}` };
    }
    return { kind: 'ok', text: `● connected · ${peers}` };
  });
</script>

{#if chip}
  <div class="status-chip status-{chip.kind}" title={chip.title}>{chip.text}</div>
{/if}

<style>
  .status-chip {
    font-family: var(--font-mono);
    font-size: 10px;
    line-height: 1;
    padding: 4px 6px;
    border-radius: 4px;
    user-select: none;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .status-ok {
    color: var(--net-ok-fg);
    background: var(--net-ok-bg);
  }
  .status-warn {
    color: var(--net-warn-fg);
    background: var(--net-warn-bg);
  }
  .status-danger {
    color: var(--net-danger-fg);
    background: var(--net-danger-bg);
  }
</style>
```

Run: `npx vitest run src/lib/components/__tests__/ConnectionStatusChip.test.ts` — expected PASS.

- [ ] **Step 5: NavPanel footer mount + NavPanel tests**

In `src/lib/components/NavPanel.svelte`:

1. Imports: `import IdentityChip from './IdentityChip.svelte';` and `import ConnectionStatusChip from './ConnectionStatusChip.svelte';`
2. Props (after `proposalsActiveFor` from Task 3):

```typescript
    proposalsActiveFor = null,
    identity,
    showConnectionStatus = false,
  }: {
    …
    proposalsActiveFor?: string | null;
    /** ZEB-606: identity-chip signals (App-computed). Chip renders only when
     *  provided, so bare test construction stays chip-free. */
    identity?: { displayName: string; ownerIdHex: string | null; selfOnline: boolean; selfSovereign: boolean };
    /** ZEB-606: mount the connection-status strip (fires network-health IPC
     *  on mount — default off so bare construction makes no IPC calls). */
    showConnectionStatus?: boolean;
  } = $props();
```

3. Footer markup — after the `Network Viz` button (line ~365), still inside `.nav-footer`:

```svelte
      {#if identity}
        <IdentityChip
          displayName={identity.displayName}
          ownerIdHex={identity.ownerIdHex}
          selfOnline={identity.selfOnline}
          selfSovereign={identity.selfSovereign}
        />
      {/if}
      {#if showConnectionStatus}
        <ConnectionStatusChip />
      {/if}
```

4. Add to `src/lib/components/__tests__/NavPanel.test.ts` (new describe; NOTE: do not pass `showConnectionStatus` anywhere in this file — it would fire unmocked IPC):

```typescript
describe('Identity chip (ZEB-606)', () => {
  it('renders the chip when identity is provided', () => {
    const { container } = render(NavPanel, {
      props: {
        nodes: [],
        collapsed: false,
        identity: { displayName: 'Jake Englund', ownerIdHex: 'ab'.repeat(16), selfOnline: true, selfSovereign: true },
      },
    });
    expect(container.querySelector('[data-testid="identity-chip"]')).toBeTruthy();
    expect(screen.getByText('Jake Englund')).toBeTruthy();
    expect(screen.getByText('● self-sovereign')).toBeTruthy();
  });

  it('renders no chip without identity (bare construction)', () => {
    const { container } = render(NavPanel, { props: { nodes: [], collapsed: false } });
    expect(container.querySelector('[data-testid="identity-chip"]')).toBeNull();
  });
});
```

- [ ] **Step 6: App wiring**

In `src/App.svelte`:

1. Near `communityActiveView` (Task 3 additions), add:

```typescript
  // ZEB-606: identity-chip signals. Ring = "you appear online to others"
  // (visibility toggle + identity present); presence rosters never contain
  // self (zenoh doesn't loop our own beacon), so presenceService can't
  // answer this. Microline = self-sovereign identity minted and loaded.
  let identityChipInfo = $derived({
    displayName: myProfile?.displayName ?? '',
    ownerIdHex: selfOwnerId,
    selfOnline: presenceVisible && ownerIdentityState === 'present',
    selfSovereign: ownerIdentityState === 'present',
  });
```

2. NavPanel mount: after `proposalsActiveFor={…}` add:

```svelte
        identity={identityChipInfo}
        showConnectionStatus={true}
```

- [ ] **Step 7: Full gates + commit**

```bash
npx tsc --noEmit && npx vitest run
git add src/lib/components/IdentityChip.svelte src/lib/components/ConnectionStatusChip.svelte src/lib/components/__tests__/IdentityChip.test.ts src/lib/components/__tests__/ConnectionStatusChip.test.ts src/lib/components/NavPanel.svelte src/App.svelte src/lib/components/__tests__/NavPanel.test.ts
git commit -m "ZEB-606 T5: nav footer identity chip + live connection-status strip"
```

---

## Final verification (before the whole-branch review)

```bash
npx tsc --noEmit && npx vitest run          # full frontend gates
git diff --stat main -- src-tauri/           # MUST print nothing
git diff --stat main -- src/lib/components/Layout.svelte  # MUST print nothing
```
