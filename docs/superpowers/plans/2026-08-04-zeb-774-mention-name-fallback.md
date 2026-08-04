# ZEB-774 mention name-fallback — Implementation Plan

> **For agentic workers:** design at
> `docs/superpowers/specs/2026-08-04-zeb-774-mention-name-fallback-design.md`.
> Steps use checkbox syntax for tracking.

**Goal:** Make the GUI @-mention path (autocomplete labels, message-author
labels, inline mention tokens) fall back to the roster-DTO display name before
raw hex, and make the autocomplete matchable — and self-documenting — by
owner-id hex prefix.

**Architecture:** One shared ladder `resolveMentionLabel` gains an optional
`resolveRosterName` rung (between profile-card name and short hex); a single
`resolveRosterName` built in `CommunityView` from `members` is threaded to every
channel/townhall `resolveMentionLabel` call site. `filterCandidates` gains an
additive owner-id-hex-prefix match. `MentionAutocomplete` shows a muted hex hint
on named rows.

**Tech Stack:** Svelte 5 + TypeScript; vitest.

## Global Constraints

- Frontend only — no Rust changes. Gates: `npx tsc --noEmit` + `npx vitest run`
  from the repo root.
- Fallback order is fixed and must match `MemberRow.svelte:127`:
  `nickname ?? card.displayName ?? rosterName ?? ownerId.slice(0,8)`.
- Hex match is **prefix** on `ownerId`, case-insensitive, and **additive**
  (never reorders or removes name matches). Ranking: label-prefix ►
  label-substring ► hex-prefix.
- Row hint = `ownerId.slice(0,8)`, shown only when `label !== ownerId.slice(0,8)`.
- TDD: failing test → minimal impl → green → commit, per task.

---

### Task 1: 4th rung in the shared ladder

**Files:** Modify `src/lib/mention-render.ts:43-53`; Test
`src/lib/mention-render.test.ts`.

- [ ] **Step 1 — failing tests.** Add cases to `mention-render.test.ts`:
  `resolveMentionLabel(id, undefined, undefined, () => 'Roster')` → `'Roster'`;
  nickname/card still win over the roster resolver; with all three absent →
  `id.slice(0,8)`.
- [ ] **Step 2 — run, expect fail** (4th param not yet accepted / rung missing).
- [ ] **Step 3 — implement.** Add optional param
  `resolveRosterName?: (id: string) => string | undefined` and the
  `?? nonEmpty(resolveRosterName?.(ownerId))` rung before the hex fallback.
  Update the doc-comment ladder description.
- [ ] **Step 4 — run, expect pass.**
- [ ] **Step 5 — commit.**

### Task 2: hex-prefix matching in the autocomplete matcher

**Files:** Modify `src/lib/mention-compose.ts:105-118`; Test
`src/lib/mention-compose.test.ts`.

- [ ] **Step 1 — failing tests.** Add cases: a candidate with
  `ownerId: '2e9a2151' + …` is returned for query `'2e9a'`; a query matching a
  label ranks that candidate ahead of a hex-only match; a label match is not
  also emitted as a hex match (no duplicate); empty-query and `limit` unchanged.
- [ ] **Step 2 — run, expect fail** (hex query returns nothing today).
- [ ] **Step 3 — implement** the 3-way exclusive partition (label-prefix ►
  label-substring ► hex-prefix), preserving the `q === ''` early return and
  `limit`. Update the doc-comment.
- [ ] **Step 4 — run, expect pass.**
- [ ] **Step 5 — commit.**

### Task 3: build + thread `resolveRosterName` (Gaps A + C wiring)

**Files:** Modify `src/lib/components/CommunityView.svelte` (build resolver;
use in `joinedMentionCandidates:167`; pass prop to `ChannelMessageFeed:513-529`
and `TownHallView:479-500`); `src/lib/components/ChannelMessageFeed.svelte`
(add prop + type; use at `authorLabel:547` and inline mention `:972`);
`src/lib/components/TownHallView.svelte` (add prop + type; forward at `:466`).

- [ ] **Step 1 — CommunityView.** Add the `rosterNameByOwner` `$derived` Map and
  `resolveRosterName` fn; pass it as the 4th arg in the `joinedMentionCandidates`
  label build; add `{resolveRosterName}` to both the `ChannelMessageFeed` and
  `TownHallView` prop lists.
- [ ] **Step 2 — ChannelMessageFeed.** Add `resolveRosterName` to the `$props`
  destructure and the props type
  (`resolveRosterName?: (ownerIdHex: string) => string | undefined;`); pass it
  as the 4th arg at `authorLabel` (`:547`) and the inline mention render
  (`:972`).
- [ ] **Step 3 — TownHallView.** Add `resolveRosterName` to its `$props` +
  type; forward `{resolveRosterName}` to the nested `ChannelMessageFeed`.
- [ ] **Step 4 — `npx tsc --noEmit`.** Expect clean (prop wiring type-checks).
- [ ] **Step 5 — commit.**

### Task 4: autocomplete row hint

**Files:** Modify `src/lib/components/MentionAutocomplete.svelte:33-42` + style;
Test `src/lib/components/__tests__/MentionAutocomplete.test.ts`.

- [ ] **Step 1 — failing tests.** Add: a candidate with a name label + distinct
  `ownerId` renders the 8-char hex hint; a candidate whose label already equals
  `ownerId.slice(0,8)` renders no hint.
- [ ] **Step 2 — run, expect fail** (no hint element yet).
- [ ] **Step 3 — implement.** Wrap the label in a `<span class="label">`; add a
  `{#if c.label !== c.ownerId.slice(0,8)}<span class="hex-hint">{c.ownerId.slice(0,8)}</span>{/if}`;
  add muted `.hex-hint` style; make the button a flex row so the hint floats
  trailing.
- [ ] **Step 4 — run, expect pass.**
- [ ] **Step 5 — commit.**

### Task 5: full frontend gate

- [ ] `npx tsc --noEmit` (repo root) — clean.
- [ ] `npx vitest run` (repo root) — all green.
- [ ] Manual reasoning pass: confirm no other `resolveMentionLabel` call site in
  a community/townhall surface was missed (grep `resolveMentionLabel`).
- [ ] Commit any residual, then push + open PR (Closes ZEB-774).
