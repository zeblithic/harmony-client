# ZEB-652 Commons H: FriendsPanel restyle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring `FriendsPanel.svelte` control/badge anatomy onto the Commons idiom — 5px control radii, explicit `--font-ui` on action buttons, and the `.already-friend-badge` reshaped to the Commons neutral status-pill.

**Architecture:** A single-file, `<style>`-block-only restyle. No template, logic, testid, label, or token-value changes. The three edits (radii, button font-family, badge pill) are one reviewable deliverable, so this is one SDD task.

**Tech Stack:** Svelte 5, CSS custom-property token layer (`src/app.css`), Vitest + @testing-library/svelte, `style-token-guard` budget test.

## Global Constraints

- **Budget-0 color tokens.** Introduce **no** new hex/rgb/hsl/named-color literals. Every color stays a `var(--*)` already defined in `src/app.css`. `src/style-token-allowlist.json` must remain **byte-identical** (the guard ratchets down only, never up).
- **Preserve all testids, labels, roles.** `data-testid="already-friend-badge"` and its `already friends` text are unchanged. The full existing FriendsPanel test suite (`src/lib/components/FriendsPanel.test.ts`, 573 lines) must stay green.
- **Radii target = 5px** for controls/inputs (the Commons control radius), applied only to the 6 audited `4px` selectors. The `2px` on `.identity-btn:focus-visible` (a focus-ring corner) and the `8px`→`20px` badge change are handled separately below.
- **Badge radius = 20px**, matching the Commons status-pill anatomy already used by `NetworkStatusPill.svelte` and governance `StatusPill.svelte` (not an ad-hoc `999px`).
- **Badge tone = neutral** (approved 2026-07-07): keep `background: var(--bg-tertiary)` / `color: var(--text-secondary)` / `1px solid var(--border)`. Only the radius and font-family change.
- **`.identity-btn` stays mono** — it is the short-hex identity drill-down trigger and inherits `--font-mono` via `.friend-addr`; do **not** add `--font-ui` to it. `.link-btn` already uses `font: inherit`; leave it.
- **Gates (from repo root):** `npx tsc --noEmit && npx vitest run` both clean; `style-token-guard` test green.

**Scope note (audit correction):** The ZEB-611 audit said "7× `border-radius: 4px`". The actual code has **6** `4px` control selectors (`.nickname-input`, `.unfriend-btn`, `.primary-btn`, `.secondary-btn`, `.url-input`, `.accept-btn`). This plan restyles those 6; there is no 7th.

---

### Task 1: FriendsPanel Commons control + badge restyle

**Files:**
- Modify: `src/lib/components/FriendsPanel.svelte` (`<style>` block only, lines ~1233–1457)
- Verify (no edit): `src/lib/components/FriendsPanel.test.ts`, `src/style-token-guard.test.ts`, `src/style-token-allowlist.json`

**Interfaces:**
- Consumes: existing tokens `--font-ui`, `--bg-tertiary`, `--text-secondary`, `--border`, `--accent`, `--text-bright`, `--danger-muted` (all already defined in `src/app.css`).
- Produces: nothing new (no new component/export/testid). Purely visual.

**Testing note (deliberate — no new test):** This is a CSS-only change to a `<style>` block; no template, testid, label, or logic is touched, so there is no new behavior to test. jsdom does not compute `<style>`-block values, so a `border-radius: 5px` assertion would test nothing real. The meaningful contract — the badge testid + label survive — is already guarded by the existing suite staying green. Verification is: existing suite green + tsc clean + token-guard green + allowlist byte-identical. Do **not** add a brittle CSS-value test.

- [ ] **Step 1: Baseline the gates (must be green before editing)**

Run (from repo root):
```bash
npx vitest run src/lib/components/FriendsPanel.test.ts src/style-token-guard.test.ts
```
Expected: PASS (establishes the green baseline you must preserve).

- [ ] **Step 2: Bump the 6 control radii 4px → 5px**

In `src/lib/components/FriendsPanel.svelte`, change `border-radius: 4px;` → `border-radius: 5px;` in each of these rules (and ONLY these — do not touch the `2px` focus-ring or the badge's radius here):

- `.nickname-input` (~line 1238)
- `.unfriend-btn` (~line 1318)
- `.primary-btn` (~line 1342)
- `.secondary-btn` (~line 1357)
- `.url-input` (~line 1376)
- `.accept-btn` (~line 1452)

- [ ] **Step 3: Add `font-family: var(--font-ui)` to the action buttons**

HTML `<button>`s do not inherit the body font. Add `font-family: var(--font-ui);` to each of these button rules:

- `.small-btn` (~line 1227)
- `.unfriend-btn` (~line 1314)
- `.primary-btn` (~line 1339)
- `.secondary-btn` (~line 1354)
- `.accept-btn` (~line 1448)

Do **not** add it to `.identity-btn` (stays mono) or `.link-btn` (already `font: inherit`).

- [ ] **Step 4: Reshape `.already-friend-badge` to the Commons neutral status-pill**

Replace the `.already-friend-badge` rule (~lines 1271–1279) with:

```css
  .already-friend-badge {
    flex-shrink: 0;
    font-size: 10px;
    padding: 1px 8px;
    border-radius: 20px;
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    border: 1px solid var(--border);
    font-family: var(--font-ui);
  }
```

Changes vs. current: `border-radius` 8px → **20px** (pill), horizontal padding 6px → **8px** (pill breathing room), add `font-family: var(--font-ui)`. Tone tokens (`--bg-tertiary`/`--text-secondary`/`--border`) unchanged. Template/testid/label untouched.

- [ ] **Step 5: Run the gates**

Run (from repo root):
```bash
npx tsc --noEmit && npx vitest run
```
Expected: tsc clean; full suite PASS (272 files / 3245+ tests), including `FriendsPanel.test.ts` and `style-token-guard`.

- [ ] **Step 6: Confirm the allowlist is byte-identical**

Run:
```bash
git diff --stat src/style-token-allowlist.json
```
Expected: **no output** (file unchanged). If it changed, you added a color literal — revert and use an existing `var(--*)` token.

- [ ] **Step 7: Commit**

```bash
git add src/lib/components/FriendsPanel.svelte
git commit -m "ZEB-652: Commons H — FriendsPanel control radii, button font-ui, badge pill"
```

---

## Self-Review

**1. Spec coverage:** All three ticket bullets are covered — radii (Step 2), button font-family (Step 3), `.already-friend-badge` chip idiom (Step 4). The audit's "7×" miscount is reconciled to the real 6 selectors in Global Constraints. ✅

**2. Placeholder scan:** No TBD/TODO/vague steps; every code step shows exact selectors and the full replacement rule. ✅

**3. Type consistency:** No types/signatures — CSS-only. Token names (`--font-ui`, `--bg-tertiary`, `--text-secondary`, `--border`) verified present in `src/app.css`. Badge radius `20px` matches the sibling-pill anatomy asserted in Global Constraints. ✅
