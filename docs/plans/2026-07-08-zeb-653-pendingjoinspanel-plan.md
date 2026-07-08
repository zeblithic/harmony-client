# ZEB-653 Commons H: PendingJoinsPanel — Implementation Plan

> Svelte 4→5 migration + Commons card/chip anatomy for the most Discord-era surface in the
> ZEB-611 audit. Reference sibling = `PendingAdminProposalsPanel` (already runes + Commons).

**Goal:** Migrate `PendingJoinsPanel.svelte` to Svelte-5 runes and bring Commons card/pill/mono
anatomy to it, preserving all existing async-race behavior and passing tests.

**Architecture:** One component rewrite + one added test. Frontend-only; no Rust/IPC/behavior change.

## Global Constraints
- **Budget-0**: every colour a `var(--*)` already in `app.css`; `style-token-allowlist.json`
  byte-identical (PendingJoinsPanel already has 0 raw literals — keep it 0).
- **Radius/type rubric**: cards 8px; buttons 5px; IDs/timestamps → `--font-mono`; round pills 20px.
- **Gates**: `tsc --noEmit` + `vitest run` + `style-token-guard` green.

---

### Task 1: Svelte 4 → 5 runes migration (behavior-preserving)

**File:** `src/lib/components/PendingJoinsPanel.svelte`

- `export let communityId/canModerate` → `let { communityId, canModerate } = $props()`.
- Rendered state → `$state`: `pending`, `recent`, `errorMessage`, `loading`.
- Non-reactive stale-guard tokens stay plain `let`: `latestCallId`, `latestWatchId`,
  `convergedUnlisten`.
- `refresh()` and `kickJoiner()` bodies unchanged (they already carry the `latestCallId`
  out-of-order guard).
- Replace the `$: void watchDeps(...)` reactive statement + its `lastWatched*` manual dedup with
  a single `$effect` modeled on the reference sibling: capture `myWatchId = ++latestWatchId`, read
  `communityId`/`canModerate` (dep tracking); on `!canModerate` bump `latestCallId` and clear
  pending/recent/errorMessage/loading (preserves the R4-7 discard); else `refresh()` + register the
  `community-state-sync-converged` listener guarded by `myWatchId`/`cancelled`, returning a cleanup
  that unlistens. Svelte-5's native dep-tracking replaces the manual `lastWatched*` dedup.
- `on:click` → `onclick`. Keep `onDestroy` as defensive unlisten.

**Behavior invariants** (must not regress): out-of-order refresh results discarded via
`latestCallId`; stale listener registration discarded via `latestWatchId`/`cancelled`;
`!canModerate` clears data and registers no listener; community switch tears down the old listener.

### Task 2: Commons anatomy (card chrome, CountChip, mono, destructive button, px grid)

Same file, `<style>` + markup:

- **Panel flat, rows carded.** Keep `.pending-joins-panel` a flat flex column (the parent
  `CommunitySettingsPanel` already frames it in a `.section` with a "Join requests" label, and the
  reference keeps its panel flat) — do **not** card-wrap the panel (avoids nesting a card in the
  Settings section, per the ProfileEditor #6 precedent). Each `<li class="join-row">` becomes a card:
  `var(--surface-raised)` + `1px var(--border)` + `border-radius: 8px` + `var(--shadow-e1)`.
- **Counts → CountChip.** Keep both `<details>` (tests pin `details:last-of-type li`); each
  `<summary>` renders a neutral CountChip carrying the section name + count:
  `<CountChip label="Awaiting counter-sign" value={String(pending.length)} tone="neutral" />` and
  `label="Recent joins" value={String(recent.length)}`. (Neutral per the ZEB-657 §3 #9 note.)
- **Mono.** `.joiner` (truncated ID) + `.time` (HLC timestamp) → `font-family: var(--font-mono)`.
- **Destructive Reject button.** `<button class="reject-btn" type="button" onclick=…>Reject (kick)</button>`
  styled `background: var(--danger-muted); color: var(--text-bright); border-radius: 5px` — matching
  the `LastAdminWarningDialog` destructive-button convention; keep the "Reject" text (test 2).
- **px grid.** Convert all `em` spacing to the 4/8/12/16 px grid (`margin: 1em 0` → `16px 0`, row
  `padding: 12px`, gaps `8px`/`12px`).

### Task 3: Tests

**File:** `src/lib/components/__tests__/PendingJoinsPanel.test.ts`

- The 4 existing tests pass unchanged (structure preserved: `li` rows, "Reject" button,
  `.pending-joins-panel` gate, `details:last-of-type li`).
- **Add** one test: with 2 pending / 0 recent, assert the pending summary renders the CountChip
  (label "Awaiting counter-sign" + value "2"), locking the count→chip integration.

### Task 4: Gate + PR

- `tsc --noEmit` + `vitest run` (+ style-token-guard) green; allowlist diff empty.
- Commit; the branch also carries the preserved ZEB-651 plan (chore, already committed). Open PR;
  trigger CodeRabbit once.
