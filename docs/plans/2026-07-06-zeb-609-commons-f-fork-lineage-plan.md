# Commons F — Fork & Lineage Restyle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restyle the four fork & lineage surfaces to the Commons design system, rendering only data the app genuinely has.

**Architecture:** Pure frontend restyle (Svelte 5 runes). Four independent tasks, one per surface — `ForkLineageTree` (card rows), the `ChannelMessageFeed` fork-divider band, `ForkConfirmDialog` (Commons chrome), and the `CommunitySettingsPanel` Forks section. No data-model, IPC, or Rust changes. Every unbacked mock element (fork "why", amicable/dispute coding, member counts, "signed by N", the 2D graph) is deferred to a follow-up ticket.

**Tech Stack:** Svelte 5 (runes), TypeScript, Vitest + @testing-library/svelte, CSS custom-property tokens from `src/app.css`.

**Spec:** `docs/specs/2026-07-06-zeb-609-commons-f-fork-lineage-design.md` (committed `b2caa77f`).

## Global Constraints

- **Frontend gates (both must pass):** `npx tsc --noEmit && npx vitest run` from the repo root.
- **Zero new raw color literals** in any `<style>` block. Use `var(--…)` tokens or the guard-whitelisted idiom `color-mix(in srgb, var(--x) N%, …)` (and `transparent`, which the guard ignores). None of these files are in `src/style-token-allowlist.json`, so their budget is 0 — one raw hex fails `src/style-token-guard.test.ts`.
- **`src/commons-hex-guard.test.ts` stays empty:** introduce none of the 8 forbidden Discord hex anywhere.
- **Byte-identical pinned anchors:** all copy, roles, `aria-*`, class selectors, and input ids that existing tests assert on are preserved exactly (each task lists its pins).
- **Do not rename** `ForkDivider`/`TimelineMessage` fields, the `onConfirm({name,silent,alsoLeave})` contract, or the `ForkLineageTree` prop interface.
- **Sage↔clay is structural, never a dispute classifier.** Clay = the "fork" accent (`⑂`, divider, fork CTA); sage (`--accent`/`--primary-*`) = the "you/member" accent.
- **Svelte 5 runes** (`$props`, `$state`, `$derived`). **Commit per task. No worktrees** (`git checkout -b` in the main repo, already on branch `zeb-609-commons-f-fork-lineage`).
- **No cross-repo / harmony-core changes.** ZEB-605/606/607/608 surfaces untouched.

**Exact Commons tokens used (all already in `src/app.css`, light + dark):**
`--accent` (sage), `--primary-deep`/`--primary-soft`/`--primary-border` (sage tints), `--gov-clay`/`--gov-clay-soft`/`--gov-clay-deep` (clay), `--surface-raised`, `--bg-tertiary`, `--border`, `--line-soft`, `--text-primary`/`--text-secondary`/`--text-muted`/`--faint`/`--text-bright`, `--font-display`/`--font-ui`/`--font-mono`, `--shadow-e1`, `--surface-highlight`, `--overlay`, `--vote-against`, `--danger-text-muted`.

---

## File Structure

| File | Responsibility | Task |
|---|---|---|
| `src/lib/components/ForkLineageTree.svelte` | Card-row lineage tree (was flat list) | T1 |
| `src/lib/components/__tests__/ForkLineageTree.test.ts` | + badge assertions | T1 |
| `src/lib/components/ChannelMessageFeed.svelte` | Fork-divider band (markup ~901-908, styles ~1474-1488) | T2 |
| `src/lib/components/__tests__/ChannelMessageFeed.test.ts` | + divider-render test | T2 |
| `src/lib/components/ForkConfirmDialog.svelte` | Commons dialog chrome | T3 |
| `src/lib/components/__tests__/ForkConfirmDialog.test.ts` | + snapshot-note/consent test | T3 |
| `src/lib/components/CommunitySettingsPanel.svelte` | Forks section chrome + "fork of" callout (~532-559, styles ~848-869) | T4 |
| `src/lib/components/__tests__/CommunitySettingsPanel.test.ts` | + callout tests | T4 |

`src/lib/fork-timeline.ts` and `src/lib/types.ts` are **read-only references** — do not modify.

---

## Task 1: `ForkLineageTree` card rows

**Files:**
- Modify: `src/lib/components/ForkLineageTree.svelte` (full markup + `<style>` rewrite; `<script>` logic mostly preserved)
- Test: `src/lib/components/__tests__/ForkLineageTree.test.ts`

**Interfaces:**
- Consumes: `CommunityLineageDto`, `ForkDescendantDto` (from `../types`) — **unchanged**. Props unchanged: `lineage`, `descendants`, `localNavIds`, `resolveLocalName`, `onNavigate`.
- Produces: same component contract. Adds `.lineage-badge` spans (`you-here` / `member` / `not-joined`).

**Pinned anchors (preserve byte-identical):** `<ul role="tree" aria-label="Fork lineage tree">`; one `<li role="treeitem">` per ancestor + self + descendant; `aria-current="page"` + `aria-selected={true}` on the self `<li>`; copy "You are here", "(no forks yet)", "…and {N} earlier ancestors", "an unknown member"; `truncSpaceId` format `0x{first8}…`; locally-known rows render a `<button>` firing `onNavigate(spaceId)`, unknown rows render a `<span>` (zero buttons); `resolveLocalName` invoked only for locally-known descendants.

**Key testing-library fact:** `getByText` matches an element by its *direct* text nodes only (not descendants). Putting the name in a `<span class="node-name">` inside a `<button>` means `getByText(/name/)` returns the span; a click on it still bubbles to the button's `onclick`. This is why nesting the name inside the card does not break the click test or produce multiple matches.

- [ ] **Step 1: Add the failing badge tests**

Append these tests inside the `describe('ForkLineageTree', …)` block in `src/lib/components/__tests__/ForkLineageTree.test.ts`:

```ts
  it('shows the sage "You are here" badge on the self row', () => {
    const { container } = render(ForkLineageTree, {
      props: { lineage: emptyLineage('Root'), descendants: [], localNavIds: new Set() },
    });
    const selfRow = container.querySelector('[aria-current="page"]')!;
    const badge = selfRow.querySelector('.lineage-badge.you-here');
    expect(badge).toBeTruthy();
    expect(badge!.textContent).toMatch(/You are here/);
  });

  it('badges a locally-known descendant "Member" and an unknown one "not joined"', () => {
    const descendants: ForkDescendantDto[] = [
      { forkSpaceId: '33'.repeat(16), forkerAddr: 'ab'.repeat(16), forkerDisplayName: 'Maya', forkedAtWallMs: 1_715_000_000_000, locallyKnown: true },
      { forkSpaceId: '44'.repeat(16), forkerAddr: 'cd'.repeat(16), forkerDisplayName: null, forkedAtWallMs: 1_716_000_000_000, locallyKnown: false },
    ];
    const { container } = render(ForkLineageTree, {
      props: {
        lineage: emptyLineage('Root'),
        descendants,
        localNavIds: new Set(['33'.repeat(16)]),
        resolveLocalName: (hex: string) => (hex === '33'.repeat(16) ? 'Maya Fork' : null),
      },
    });
    const rows = container.querySelectorAll('.lineage-descendant');
    expect(rows.length).toBe(2);
    // Locally-known → sage Member badge, and it is a button.
    expect(rows[0].querySelector('.lineage-badge.member')?.textContent).toMatch(/Member/);
    expect(rows[0].querySelector('button')).toBeTruthy();
    // Unknown → muted "not joined", and zero buttons.
    expect(rows[1].querySelector('.lineage-badge.not-joined')?.textContent).toMatch(/not joined/);
    expect(rows[1].querySelector('button')).toBeNull();
  });
```

- [ ] **Step 2: Run the new tests to confirm they fail**

Run: `npx vitest run src/lib/components/__tests__/ForkLineageTree.test.ts`
Expected: the two new tests FAIL (no `.lineage-badge` elements yet); the pre-existing tests still PASS.

- [ ] **Step 3: Rewrite the component**

Replace the entire contents of `src/lib/components/ForkLineageTree.svelte` with:

```svelte
<script lang="ts">
  import type {
    CommunityLineageDto,
    ForkDescendantDto,
  } from '../types';

  let {
    lineage,
    descendants = [],
    localNavIds = new Set<string>(),
    resolveLocalName,
    onNavigate,
  }: {
    lineage: CommunityLineageDto;
    descendants?: ForkDescendantDto[];
    /** Set of locally-known SpaceIds (hex) — used to gate clickability of
     *  ancestor / descendant rows. Caller typically passes the OwnerState
     *  Space-id set (e.g., the current NavService snapshot). */
    localNavIds?: Set<string>;
    /** ZEB-287 R3-1: resolves a hex SpaceId to its display name from the
     *  caller's local nav state (typically NavService.getCommunityNameBySpaceId).
     *  Returns null/undefined if the caller can't resolve, in which case we
     *  fall back to truncated hex (defense-in-depth). */
    resolveLocalName?: (spaceId: string) => string | null | undefined;
    /** Callback fired when a clickable row is activated. */
    onNavigate?: (spaceId: string) => void;
  } = $props();

  const MAX_DISPLAYED_LINEAGE = 16;

  let ancestorRows = $derived.by(() => {
    const rows = lineage.parentLineage;
    if (rows.length <= MAX_DISPLAYED_LINEAGE) return { rows, truncated: 0 };
    const truncated = rows.length - MAX_DISPLAYED_LINEAGE;
    return { rows: rows.slice(truncated), truncated };
  });

  let hasAnyForks = $derived(
    lineage.parentLineage.length > 0 || descendants.length > 0,
  );

  let selfDepth = $derived(
    ancestorRows.rows.length + 1 + (ancestorRows.truncated > 0 ? 1 : 0),
  );

  // Self sub-line: honest — "root" when this community has no parent, else
  // the fork date when we have it. No invented "founded" date.
  let selfSub = $derived(
    lineage.forkedFrom == null
      ? 'root'
      : lineage.forkedAtWallMs != null
        ? 'forked ' + formatDate(lineage.forkedAtWallMs)
        : 'forked',
  );

  function formatDate(wallMs: number | null | undefined): string {
    if (wallMs == null) return '';
    return new Date(wallMs).toISOString().slice(0, 10);
  }

  function truncSpaceId(hex: string): string {
    return '0x' + hex.slice(0, 8) + '…';
  }

  function initial(text: string): string {
    return (text.trim().charAt(0) || '⑂').toUpperCase();
  }

  function handleClick(spaceId: string): void {
    onNavigate?.(spaceId);
  }
</script>

<ul role="tree" class="fork-lineage-tree" aria-label="Fork lineage tree">
  {#if ancestorRows.truncated > 0}
    <li
      role="treeitem"
      aria-level={1}
      aria-selected={false}
      class="lineage-row lineage-truncation"
    >
      &hellip;and {ancestorRows.truncated} earlier ancestors
    </li>
  {/if}

  {#each ancestorRows.rows as entry, i (entry.spaceId)}
    {@const depth = i + 1 + (ancestorRows.truncated > 0 ? 1 : 0)}
    {@const known = localNavIds.has(entry.spaceId)}
    <li
      role="treeitem"
      aria-level={depth}
      aria-selected={false}
      class="lineage-row lineage-ancestor"
      style="padding-left: calc({depth} * 1.5rem);"
    >
      {#if known}
        <button class="lineage-card lineage-clickable" onclick={() => handleClick(entry.spaceId)}>
          <span class="node-avatar" aria-hidden="true">{initial(entry.name)}</span>
          <span class="node-body">
            <span class="node-name">&#x21B3; {entry.name}</span>
            {#if entry.forkedAtWallMs != null}<span class="node-sub">forked {formatDate(entry.forkedAtWallMs)}</span>{/if}
          </span>
        </button>
      {:else}
        <span class="lineage-card lineage-unknown" title="You're not a member of this community.">
          <span class="node-avatar" aria-hidden="true">{initial(entry.name)}</span>
          <span class="node-body">
            <span class="node-name">&#x21B3; {entry.name}</span>
            {#if entry.forkedAtWallMs != null}<span class="node-sub">forked {formatDate(entry.forkedAtWallMs)}</span>{/if}
          </span>
        </span>
      {/if}
    </li>
  {/each}

  <li
    role="treeitem"
    aria-level={selfDepth}
    aria-current="page"
    aria-selected={true}
    class="lineage-row lineage-self"
    style="padding-left: calc({selfDepth} * 1.5rem);"
  >
    <span class="lineage-card self-card">
      <span class="node-avatar node-avatar-self" aria-hidden="true">{initial(lineage.selfName)}</span>
      <span class="node-body">
        <span class="node-name">{lineage.selfName}</span>
        <span class="node-sub">{selfSub}</span>
      </span>
      <span class="lineage-badge you-here">&#x25CF; You are here</span>
    </span>
  </li>

  {#each descendants as desc (desc.forkSpaceId)}
    {@const known = desc.locallyKnown && localNavIds.has(desc.forkSpaceId)}
    {@const resolvedName = known ? resolveLocalName?.(desc.forkSpaceId) : null}
    {@const display = resolvedName ?? truncSpaceId(desc.forkSpaceId)}
    {@const forker = desc.forkerDisplayName ?? 'an unknown member'}
    <li
      role="treeitem"
      aria-level={selfDepth + 1}
      aria-selected={false}
      class="lineage-row lineage-descendant"
      style="padding-left: calc({selfDepth + 1} * 1.5rem);"
    >
      {#if known}
        <button class="lineage-card lineage-clickable card-member" onclick={() => handleClick(desc.forkSpaceId)}>
          <span class="node-avatar" aria-hidden="true">{initial(display)}</span>
          <span class="node-body">
            <span class="node-name">&#x21B3; {display}</span>
            <span class="node-sub">forked {formatDate(desc.forkedAtWallMs)} · by {forker}</span>
          </span>
          <span class="lineage-badge member">&#x2713; Member</span>
        </button>
      {:else}
        <span class="lineage-card lineage-unknown" title="You're not a member of this fork.">
          <span class="node-avatar" aria-hidden="true">{initial(display)}</span>
          <span class="node-body">
            <span class="node-name">&#x21B3; {display}</span>
            <span class="node-sub">forked {formatDate(desc.forkedAtWallMs)} · by {forker}</span>
          </span>
          <span class="lineage-badge not-joined">not joined</span>
        </span>
      {/if}
    </li>
  {/each}

  {#if !hasAnyForks}
    <li class="lineage-empty-hint" aria-hidden="true">(no forks yet)</li>
  {/if}
</ul>

<style>
  .fork-lineage-tree {
    position: relative;
    list-style: none;
    padding: 0;
    margin: 0.5rem 0;
    font-size: 0.9rem;
  }
  /* Lineage spine — a faint clay rail that reads the rows as a genealogy. */
  .fork-lineage-tree::before {
    content: '';
    position: absolute;
    top: 0.75rem;
    bottom: 0.75rem;
    left: 0.7rem;
    width: 1px;
    background: color-mix(in srgb, var(--gov-clay) 28%, transparent);
  }
  .lineage-row {
    padding: 0.2rem 0;
  }
  .lineage-card {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    box-sizing: border-box;
    padding: 8px 10px;
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 11px;
    box-shadow: var(--shadow-e1);
    text-align: left;
    font: inherit;
    color: var(--text-primary);
  }
  .self-card {
    border: 2px solid var(--accent);
    background: var(--surface-highlight);
  }
  .card-member {
    border-color: var(--primary-border);
  }
  .node-avatar {
    flex: 0 0 auto;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 30px;
    height: 30px;
    border-radius: 9px;
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    font-family: var(--font-display);
    font-weight: 600;
    font-size: 0.9rem;
  }
  .node-avatar-self {
    background: var(--accent);
    color: var(--text-bright);
  }
  .node-body {
    display: flex;
    flex-direction: column;
    gap: 1px;
    min-width: 0;
  }
  .node-name {
    font-family: var(--font-display);
    font-size: 0.95rem;
    color: var(--text-primary);
  }
  .node-sub {
    font-family: var(--font-mono);
    font-size: 0.68rem;
    color: var(--faint);
  }
  .lineage-badge {
    margin-left: auto;
    flex: 0 0 auto;
    font-family: var(--font-mono);
    font-size: 0.62rem;
    font-weight: 600;
    letter-spacing: 0.03em;
    white-space: nowrap;
  }
  .lineage-badge.you-here,
  .lineage-badge.member {
    color: var(--primary-deep);
    background: var(--primary-soft);
    padding: 2px 8px;
    border-radius: 20px;
  }
  .lineage-badge.not-joined {
    color: var(--faint);
  }
  button.lineage-clickable {
    cursor: pointer;
  }
  button.lineage-clickable:hover {
    border-color: var(--accent);
  }
  .lineage-unknown {
    cursor: default;
  }
  .lineage-empty-hint {
    color: var(--text-muted);
    font-style: italic;
    padding-left: 1.5rem;
  }
  .lineage-truncation {
    color: var(--text-muted);
    font-style: italic;
    padding-left: 0;
  }
</style>
```

- [ ] **Step 4: Run the full test file to confirm all pass**

Run: `npx vitest run src/lib/components/__tests__/ForkLineageTree.test.ts`
Expected: PASS (all pre-existing tests + the two new badge tests).

- [ ] **Step 5: Type-check**

Run: `npx tsc --noEmit`
Expected: no errors.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/ForkLineageTree.svelte src/lib/components/__tests__/ForkLineageTree.test.ts
git commit -m "feat(zeb-609): ForkLineageTree Commons card rows + relationship badges"
```

---

## Task 2: Fork-divider band in `ChannelMessageFeed`

**Files:**
- Modify: `src/lib/components/ChannelMessageFeed.svelte` (divider markup ~901-908; `.fork-divider` style block ~1474-1483)
- Test: `src/lib/components/__tests__/ChannelMessageFeed.test.ts`

**Interfaces:**
- Consumes: the `ForkDivider` timeline row from `buildUnifiedTimeline()` (`{ kind, originalCommunityName, forkedAtMs }` — **do not rename**), and the in-scope `snapshotMessages` prop (`ChannelMessageDto[]`).
- Produces: the same `.fork-divider` element (`role="separator"`, `aria-label="Forked from {name}"`), restyled as a clay card band with a real carried-count.

**Pinned anchors:** `class="fork-divider"`, `role="separator"`, `aria-label="Forked from {row.originalCommunityName}"`. Untouched: `.channel-message.pre-fork` opacity, `.pre-fork-badge`, the disabled-reactions gating.

**Timeline fact:** `buildUnifiedTimeline` inserts a divider only when snapshot AND live are both non-empty and a live row sorts (by HLC wallMs) after the last pre-fork row. The divider-render test therefore seeds snapshot messages via the `snapshotMessages` prop AND a live message via the `channel-message-received` listener.

- [ ] **Step 1: Add the failing divider-render test**

Append inside `describe('ChannelMessageFeed', …)` in `src/lib/components/__tests__/ChannelMessageFeed.test.ts`:

```ts
  it('renders the Commons fork-divider band with the real carried count', async () => {
    const preFork1 = {
      messageId: 'pf1', communityId: 'aa'.repeat(16), channelId: 'bb'.repeat(16),
      author: 'ee'.repeat(20), at: { wallMs: 400, logical: 0, deviceId: 'd' },
      body: Array.from(new TextEncoder().encode('old 1')),
    };
    const preFork2 = {
      ...preFork1, messageId: 'pf2', at: { wallMs: 500, logical: 0, deviceId: 'd' },
      body: Array.from(new TextEncoder().encode('old 2')),
    };
    const { adapter, container } = await setup({
      snapshotMessages: [preFork1, preFork2],
      originalCommunityName: 'OldCommunity',
      forkedAtMs: 1000,
    });
    // A live post-fork message (wallMs after the snapshot) creates the boundary.
    const handler = adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16), channelId: 'bb'.repeat(16),
        message: {
          messageId: 'live1', communityId: 'aa'.repeat(16), channelId: 'bb'.repeat(16),
          author: 'cc'.repeat(20), at: { wallMs: 2000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode('new message')),
        },
      },
    });
    let divider: Element | null = null;
    await waitFor(() => {
      divider = container.querySelector('.fork-divider');
      expect(divider).toBeTruthy();
    });
    expect(divider!.getAttribute('role')).toBe('separator');
    expect(divider!.getAttribute('aria-label')).toBe('Forked from OldCommunity');
    expect(divider!.textContent).toContain('Forked from OldCommunity');
    expect(divider!.textContent).toContain('⑂');
    // Real carried count = snapshotMessages.length = 2.
    expect(divider!.textContent).toContain('2 messages carried');
  });
```

- [ ] **Step 2: Run it to confirm it fails**

Run: `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts -t "fork-divider band"`
Expected: FAIL — current divider has no `⑂` and no "messages carried" text.

- [ ] **Step 3: Replace the divider markup**

In `src/lib/components/ChannelMessageFeed.svelte`, replace the divider block (currently ~901-908):

```svelte
      {#if 'kind' in row && row.kind === 'fork-divider'}
        <div
          class="fork-divider"
          role="separator"
          aria-label="Forked from {row.originalCommunityName}"
        >
          ───── Forked from {row.originalCommunityName} on {new Date(row.forkedAtMs).toLocaleDateString()} ─────
        </div>
```

with:

```svelte
      {#if 'kind' in row && row.kind === 'fork-divider'}
        <div
          class="fork-divider"
          role="separator"
          aria-label="Forked from {row.originalCommunityName}"
        >
          <span class="fork-divider-glyph" aria-hidden="true">⑂</span>
          <span class="fork-divider-text">
            <span class="fork-divider-title">Forked from {row.originalCommunityName}</span>
            <span class="fork-divider-meta">{new Date(row.forkedAtMs).toLocaleDateString()} · {snapshotMessages.length} message{snapshotMessages.length === 1 ? '' : 's'} carried</span>
          </span>
        </div>
```

- [ ] **Step 4: Replace the `.fork-divider` style block**

Replace the `.fork-divider` rule (currently ~1474-1483, the `text-align:center` version) with:

```css
  /* ZEB-609: Commons fork-divider band (clay card). */
  .fork-divider {
    display: flex;
    align-items: center;
    gap: 10px;
    margin: 4px 16px;
    padding: 10px 14px;
    background: var(--gov-clay-soft);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised));
    border-radius: 10px;
    box-shadow: var(--shadow-e1);
    user-select: none;
  }
  .fork-divider-glyph {
    flex: 0 0 auto;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 26px;
    height: 26px;
    border-radius: 7px;
    background: var(--gov-clay);
    color: var(--text-bright);
    font-size: 0.9rem;
  }
  .fork-divider-text {
    display: flex;
    flex-direction: column;
    gap: 1px;
    min-width: 0;
  }
  .fork-divider-title {
    font-family: var(--font-ui);
    font-weight: 600;
    font-size: 0.8rem;
    color: var(--gov-clay-deep);
  }
  .fork-divider-meta {
    font-family: var(--font-mono);
    font-size: 0.7rem;
    color: var(--text-muted);
  }
```

Leave `.channel-message.pre-fork` and `.pre-fork-badge` (immediately below) unchanged.

- [ ] **Step 5: Run the divider test + the pre-fork test**

Run: `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts`
Expected: PASS — the new divider test and the existing "does not render the reaction toolbar on pre-fork snapshot messages" test both green.

- [ ] **Step 6: Type-check + commit**

Run: `npx tsc --noEmit` → no errors.

```bash
git add src/lib/components/ChannelMessageFeed.svelte src/lib/components/__tests__/ChannelMessageFeed.test.ts
git commit -m "feat(zeb-609): Commons fork-divider band with real carried count"
```

---

## Task 3: `ForkConfirmDialog` Commons chrome

**Files:**
- Modify: `src/lib/components/ForkConfirmDialog.svelte` (Modal body markup + `<style>`; `<script>` unchanged)
- Test: `src/lib/components/__tests__/ForkConfirmDialog.test.ts`

**Interfaces:**
- Consumes: `Modal.svelte` (owns `.modal-overlay`, `role="dialog"`) and `TypedConfirmationModal.svelte` (also-leave path) — both untouched.
- Produces: same props/behavior. `onConfirm({name,silent,alsoLeave})` unchanged.

**Pinned anchors:** heading "Fork this community"; `<label for="fork-name">Name:` + `id="fork-name"`; checkbox labels "Fork silently (don't tell other members)" and "Also leave the original community"; snapshot copy `Snapshot will include ~{messageCount} messages.` / `…accessible message history (up to 5000 messages).`; buttons "Create fork" and "Cancel"; the `TypedConfirmationModal` props ("leave" required text, "Type to confirm", "Confirm"). No "why" field.

- [ ] **Step 1: Add the failing chrome test**

Append inside `describe('ForkConfirmDialog', …)` in `src/lib/components/__tests__/ForkConfirmDialog.test.ts`:

```ts
  it('shows the always-included snapshot note and the sage consent callout', () => {
    render(ForkConfirmDialog, { props: { ...baseProps } });
    expect(screen.getByText(/a frozen snapshot of every channel is always included/i)).toBeTruthy();
    expect(screen.getByText(/you become its first admin/i)).toBeTruthy();
  });
```

- [ ] **Step 2: Run it to confirm it fails**

Run: `npx vitest run src/lib/components/__tests__/ForkConfirmDialog.test.ts -t "consent callout"`
Expected: FAIL (copy not present yet).

- [ ] **Step 3: Replace the Modal body markup**

In `src/lib/components/ForkConfirmDialog.svelte`, replace the `{:else}` Modal block (currently lines ~57-105, from `<Modal …>` through `</Modal>`) with:

```svelte
{:else}
  <Modal {onCancel} ariaLabelledby={titleId} canDismissOnBackdrop={true}>
    <div class="fork-header">
      <span class="fork-glyph" aria-hidden="true">⑂</span>
      <h3 class="modal-title" id={titleId}>Fork this community</h3>
    </div>

    <p class="modal-description">
      This creates a new community with a frozen copy of the history you can see
      in this one. Anyone you invite to the fork will see that history.
    </p>

    <div class="field">
      <label for="fork-name">Name:</label>
      <input
        id="fork-name"
        type="text"
        bind:value={name}
        class="name-input"
      />
    </div>

    <div class="checkbox-field">
      <label>
        <input type="checkbox" bind:checked={silent} />
        Fork silently (don't tell other members)
      </label>
    </div>

    <div class="checkbox-field">
      <label>
        <input type="checkbox" bind:checked={alsoLeave} />
        Also leave the original community
      </label>
    </div>

    <p class="snapshot-count">
      {#if messageCount > 0}
        Snapshot will include ~{messageCount} messages.
      {:else}
        Snapshot will include your accessible message history (up to 5000 messages).
      {/if}
    </p>
    <p class="snapshot-note">A frozen snapshot of every channel is always included.</p>

    <p class="consent-note">
      Forking writes a permanent divider into the new community's history. You
      become its first admin, and the original community is never affected.
    </p>

    <div class="action-row">
      <button class="confirm-btn" disabled={!nameValid} onclick={handleCreateFork}>
        Create fork
      </button>
      <div class="spacer"></div>
      <button class="cancel-btn" onclick={onCancel}>Cancel</button>
    </div>
  </Modal>
{/if}
```

- [ ] **Step 4: Update the styles**

In the `<style>` block, replace the `.modal-title` rule and add the new rules; change `.name-input:focus`, `.confirm-btn`, and `.cancel-btn`. The full set of changed/added rules:

```css
  .fork-header {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 12px;
  }
  .fork-glyph {
    flex: 0 0 auto;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 32px;
    height: 32px;
    border-radius: 8px;
    background: var(--gov-clay-soft);
    color: var(--gov-clay);
    font-size: 1rem;
  }
  .modal-title {
    color: var(--text-primary);
    font-family: var(--font-display);
    font-size: 1.15rem;
    margin: 0;
  }
```

Change `.name-input:focus` from the `outline` version to:

```css
  .name-input:focus {
    outline: none;
    border-color: var(--accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 12%, transparent);
  }
```

Add after `.snapshot-count`:

```css
  .snapshot-note {
    color: var(--text-muted);
    font-size: 0.8rem;
    margin: 0 0 12px;
  }
  .consent-note {
    background: var(--primary-soft);
    border: 1px solid var(--primary-border);
    color: var(--primary-deep);
    border-radius: 8px;
    padding: 10px 12px;
    font-size: 0.8rem;
    line-height: 1.45;
    margin: 0 0 16px;
  }
```

Replace `.confirm-btn` and `.cancel-btn` (keep the `:disabled` and `:focus-visible` rules that follow):

```css
  .confirm-btn {
    background: var(--gov-clay);
    color: var(--text-bright);
    border: none;
    padding: 8px 16px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 0.875rem;
    font-weight: 600;
  }
```

```css
  .cancel-btn {
    background: var(--surface-raised);
    color: var(--text-secondary);
    border: 1px solid var(--border);
    padding: 8px 16px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 0.875rem;
  }
```

- [ ] **Step 5: Run the full test file**

Run: `npx vitest run src/lib/components/__tests__/ForkConfirmDialog.test.ts`
Expected: PASS — all pre-existing tests (heading, checkboxes, snapshot copy, `onConfirm` payload, typed-confirm second stage, backdrop/Escape) plus the new consent-callout test.

- [ ] **Step 6: Type-check + commit**

Run: `npx tsc --noEmit` → no errors.

```bash
git add src/lib/components/ForkConfirmDialog.svelte src/lib/components/__tests__/ForkConfirmDialog.test.ts
git commit -m "feat(zeb-609): ForkConfirmDialog Commons chrome (clay header, sage consent)"
```

---

## Task 4: Settings Forks section + "fork of" callout

**Files:**
- Modify: `src/lib/components/CommunitySettingsPanel.svelte` (Forks section markup ~532-559; `.fork-btn` styles ~848-864; add callout styles)
- Test: `src/lib/components/__tests__/CommunitySettingsPanel.test.ts`

**Interfaces:**
- Consumes (all existing props, unchanged): `onFork`, `phase2Lineage: CommunityLineageDto | null`, `descendants`, `localNavIds`, `onForkLineageNavigate`, `resolveLocalCommunityName`.
- Produces: same section. Adds a `.fork-of-callout` block, gated on real `phase2Lineage.forkedFrom`.

**Pinned anchors:** section label "Forks" (`getByText('Forks')`); `.forks-explainer` copy substrings `/Any member of a community can fork it at any time/` and `/communities preserve continuity if members want to take/`; `.forks-section`; `button.fork-this-community` text "Fork this community"; the `ForkLineageTree` mount with `resolveLocalName={resolveLocalCommunityName}`.

**Parent-resolution fact:** when `forkedFrom` is set the backend guarantees ≥1 `parentLineage` entry; the immediate parent is the last element (its `spaceId === forkedFrom`). The `ForkLineageTree` already renders that parent as an ancestor row, so the callout's parent name will appear twice in the DOM — scope callout assertions to `.fork-of-callout` rather than `getByText`.

- [ ] **Step 1: Add the failing callout tests**

Append inside the main `describe` in `src/lib/components/__tests__/CommunitySettingsPanel.test.ts` (the file already imports `CommunityLineageDto`):

```ts
  it('shows the "This is a fork of {parent}" callout for a forked community', async () => {
    const parentHex = '22'.repeat(16);
    const lineage: CommunityLineageDto = {
      forkedFrom: parentHex,
      forkedAtWallMs: 1_700_000_000_000,
      parentLineage: [{ spaceId: parentHex, name: 'Origin Community', forkedAtWallMs: null }],
      selfSpaceId: '33'.repeat(16),
      selfName: 'The Fork',
    };
    const onNav = vi.fn();
    const { container } = render(CommunitySettingsPanel, {
      props: {
        ...baseProps,
        phase2Lineage: lineage,
        localNavIds: new Set([parentHex]),
        onForkLineageNavigate: onNav,
      },
    });
    const callout = container.querySelector('.fork-of-callout');
    expect(callout).toBeTruthy();
    expect(callout!.textContent).toContain('This is a fork of');
    expect(callout!.textContent).toContain('Origin Community');
    // "Open ↗" is present because the parent is locally known, and it navigates.
    const openBtn = callout!.querySelector('.fork-of-open') as HTMLButtonElement;
    expect(openBtn).toBeTruthy();
    await fireEvent.click(openBtn);
    expect(onNav).toHaveBeenCalledWith(parentHex);
  });

  it('omits the "fork of" callout for a root community', () => {
    const lineage: CommunityLineageDto = {
      forkedFrom: null, forkedAtWallMs: null, parentLineage: [],
      selfSpaceId: '11'.repeat(16), selfName: 'Root',
    };
    const { container } = render(CommunitySettingsPanel, {
      props: { ...baseProps, phase2Lineage: lineage },
    });
    expect(container.querySelector('.fork-of-callout')).toBeNull();
  });
```

- [ ] **Step 2: Run to confirm failure**

Run: `npx vitest run src/lib/components/__tests__/CommunitySettingsPanel.test.ts -t "fork of"`
Expected: the callout test FAILS (no `.fork-of-callout`); the root test PASSES (nothing rendered) — that's fine.

- [ ] **Step 3: Add the callout markup**

In `src/lib/components/CommunitySettingsPanel.svelte`, inside `<div class="section forks-section">`, insert the callout between the `.forks-explainer` `<p>` and the `{#if phase2Lineage}` tree block:

```svelte
      {#if phase2Lineage?.forkedFrom && phase2Lineage.parentLineage.length > 0}
        {@const parent = phase2Lineage.parentLineage[phase2Lineage.parentLineage.length - 1]}
        <div class="fork-of-callout">
          <span class="fork-of-avatar" aria-hidden="true">{(parent.name.trim().charAt(0) || '⑂').toUpperCase()}</span>
          <span class="fork-of-body">
            <span class="fork-of-label">This is a fork of</span>
            <span class="fork-of-name">{parent.name}</span>
          </span>
          {#if localNavIds.has(parent.spaceId)}
            <button class="fork-of-open" onclick={() => onForkLineageNavigate?.(parent.spaceId)}>Open ↗</button>
          {/if}
        </div>
      {/if}
```

- [ ] **Step 4: Add/adjust styles**

Add the callout styles and restyle `.fork-btn` to clay. Replace the existing `.fork-btn` and `.fork-btn:hover` rules (~848-860) with the clay versions and append the callout rules:

```css
  .fork-of-callout {
    display: flex;
    align-items: center;
    gap: 10px;
    background: var(--primary-soft);
    border: 1px solid var(--primary-border);
    border-radius: 9px;
    padding: 8px 12px;
    margin: 0 0 12px;
  }
  .fork-of-avatar {
    flex: 0 0 auto;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 30px;
    height: 30px;
    border-radius: 8px;
    background: var(--accent);
    color: var(--text-bright);
    font-family: var(--font-display);
    font-weight: 600;
  }
  .fork-of-body { display: flex; flex-direction: column; gap: 1px; min-width: 0; }
  .fork-of-label {
    font-size: 0.62rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--primary-deep);
  }
  .fork-of-name {
    font-family: var(--font-display);
    font-size: 0.95rem;
    color: var(--text-primary);
  }
  .fork-of-open {
    margin-left: auto;
    flex: 0 0 auto;
    background: var(--surface-raised);
    color: var(--primary-deep);
    border: 1px solid var(--primary-border);
    border-radius: 6px;
    padding: 4px 10px;
    font-size: 0.75rem;
    cursor: pointer;
  }
  .fork-btn {
    background: var(--gov-clay-soft);
    color: var(--gov-clay-deep);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised));
    padding: 6px 14px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 0.8rem;
  }
  .fork-btn:hover {
    border-color: var(--gov-clay);
  }
```

(Keep the existing `.fork-btn:focus-visible` and `.fork-error` rules unchanged.)

- [ ] **Step 5: Run the full test file**

Run: `npx vitest run src/lib/components/__tests__/CommunitySettingsPanel.test.ts`
Expected: PASS — the two new callout tests plus every pre-existing Forks test (`forks_section_always_renders…`, `…explainer_text_present`, `fork_this_community_button…`, `resolveLocalCommunityName_prop_flows_through…`).

- [ ] **Step 6: Type-check + commit**

Run: `npx tsc --noEmit` → no errors.

```bash
git add src/lib/components/CommunitySettingsPanel.svelte src/lib/components/__tests__/CommunitySettingsPanel.test.ts
git commit -m "feat(zeb-609): Settings Forks section Commons chrome + 'fork of' callout"
```

---

## Final verification (after all four tasks)

- [ ] **Full frontend gate:** `npx tsc --noEmit && npx vitest run` (repo root) — all green (~3200 tests).
- [ ] **Guard confirmation:** `npx vitest run src/style-token-guard.test.ts src/commons-hex-guard.test.ts` — both pass with no allowlist change (these four files stay out of the allowlist; if any is somehow flagged, a raw literal slipped in — replace it with a token/`color-mix`, do NOT add it to the allowlist).
- [ ] **File the follow-up ticket** (spec §0): *"Fork reason & richer lineage — capture mandatory 'why' in the fork dialog → `forkCommunity` IPC → harmony-core Fork event → persist a `reason` on the lineage; then the 2D genealogy graph + inspect panel + per-fork reason surfacing."* Reference `docs/specs/2026-07-06-zeb-609-commons-f-fork-lineage-design.md` §0, parent epic ZEB-603.

---

## Self-Review (checked against the spec)

**1. Spec coverage:** §0 honesty ledger → each surface renders only real fields (T1 badges keyed to `locallyKnown`/`localNavIds`; T2 carried-count = `snapshotMessages.length`; T3 no "why"; T4 callout gated on real `forkedFrom`). §2 Surface 1 → T1. Surface 2 → T2. Surface 3 → T3. Surface 4 → T4. §3 guards/constraints → Global Constraints + Final verification. §4 follow-up ticket → Final verification step. No gaps.

**2. Placeholder scan:** No TBD/TODO; every code step shows complete code; every command has an expected result. Radii/opacity values are concrete.

**3. Type consistency:** Prop interfaces for `ForkLineageTree`, `ForkConfirmDialog`, and the `CommunitySettingsPanel` Forks props are quoted verbatim from source and unchanged. `ForkDivider` fields (`kind`/`originalCommunityName`/`forkedAtMs`) and `snapshotMessages` are used exactly as defined in `fork-timeline.ts`/the component. `CommunityLineageDto.parentLineage[last].spaceId === forkedFrom` is the resolved parent, consistent across markup and tests.
