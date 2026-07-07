# ZEB-661 Commons H: social-forms pass — Implementation Plan

> **For agentic workers:** execution of the four ZEB-657 §3 form/anatomy decisions
> (`docs/design/commons/h-deferred-decisions.md` #3/#4/#6/#8). Design is settled; this is
> transcription + tests. Sibling of the ZEB-660 valence pass (#417, merged).

**Goal:** Bring Commons chip/pill/section anatomy to four social surfaces — DmCreateDialog,
TrustBadge, ProfileEditor, and the reaction-chip — with zero new color tokens.

**Architecture:** Frontend-only Svelte 5 restyle. Four component edits + one test update. No Rust,
no IPC, no behavior change.

## Global Constraints

- **Budget-0 color tokens.** No new hex/rgb/hsl/named-color literals in any `<style>` block; every
  color a `var(--*)` already in `src/app.css` (or a `color-mix` of those). `style-token-allowlist.json`
  stays byte-identical.
- **Radius scale** (Commons rubric): chips/ID-badges 3px; inputs/rows/buttons 5px; cards 8px;
  round affordances (pill, count badge, reaction) 20px.
- **Gates:** `npx tsc --noEmit && npx vitest run` clean; `style-token-guard` green.

---

### Task 1: DmCreateDialog — sage identity + recipient pills (#3)

**File:** `src/lib/components/DmCreateDialog.svelte`

- `.chip`: background `color-mix(… var(--library-accent) 20% …)` → `… var(--accent) 20% …`;
  `border-radius: 12px` → `20px` (removable recipient person-token = round pill).
- `.primary`: background `color-mix(… var(--library-accent) 40% …)` → `… var(--accent) 40% …`.
- `.actions button`: add `border-radius: 5px` (covers Start DM + Cancel — rubric button radius; the
  primary rendered at the global 4px default, so this is the specified 4px→5px and keeps the adjacent
  pair consistent).

Tests: `DmCreateDialog.test.ts` asserts chip-remove behavior + button-disabled state only (no
radius/color) — no update needed.

### Task 2: TrustBadge — dot → labeled chip (#4)

**File:** `src/lib/components/TrustBadge.svelte` (+ `__tests__/TrustBadge.test.ts`)

- Render the derived `label` as **visible text** inside `span.trust-badge`.
- Style: `display:inline-block; font-family:var(--font-ui); font-weight:600; font-size:11px;
  padding:4px 11px; border-radius:20px; white-space:nowrap` (StatusPill anatomy; keep
  `flex-shrink`/`vertical-align`). Drop the 8px dot dims + `border-radius:50%`.
- Tone from `trustScoreColor`: inline `background: color-mix(in srgb, {color} 16%, transparent);
  color: {color}` — full color as text matches `TrustOverview`'s existing score cells; tinted bg is
  the pill fill. Keep `role="img"` + `aria-label`.
- **Test update:** retarget the 5 color assertions from `badge.style.background` → `badge.style.color`
  (the tone now lives on the foreground); add a `textContent` test locking the visible label. This is
  the color-blind-accessibility fix (label was previously screen-reader-only).

### Task 3: ProfileEditor — flat inline section (#6)

**File:** `src/lib/components/ProfileEditor.svelte`

- `.profile-editor`: drop `background: var(--bg-secondary)` + `border-radius: 8px` (keep
  `display/flex/gap/padding`) — it renders inside the Settings tabbed panel, not a modal; flat matches
  FriendsPanel's `.friends-section`.
- `.section-title` (h3): add `font-family: var(--font-display)`.

Tests: no ProfileEditor test asserts these styles — no update needed.

### Task 4: reaction-chip — 20px pill (#8)

**File:** `src/lib/components/ChannelMessageFeed.svelte`

- `.reaction-chip`: `border-radius: 10px` (magic) → `20px` (round affordance).

Tests: `ChannelMessageFeed.test.ts` queries `.reaction-chip`/textContent only — no update needed.

### Task 5: Gate + commit

- `npx tsc --noEmit && npx vitest run` (+ style-token-guard) green.
- Confirm `git diff src/style-token-allowlist.json` is empty.
- One commit; open PR; trigger CodeRabbit once.
