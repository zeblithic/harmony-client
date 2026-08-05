# ZEB-832 — Theme Account action buttons — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (inline — this is a single-file, single-task change). Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the `.actions` buttons in `IdentityPanel.svelte` the standard themed treatment so `Backup…` / `Restore…` (and the ZEB-842 `Erase all data…` and the wizard-step buttons) stop rendering with user-agent-default chrome.

**Architecture:** CSS-only. Add a base `.actions button` rule (plus hover/focus/disabled) to the component's scoped `<style>`, mirroring the ZEB-773 reference in `src/app.css`, and raise the existing `button.danger` rule's specificity to `.actions button.danger` so it keeps winning. No markup, logic, or backend change.

**Tech Stack:** Svelte 5 scoped styles, CSS custom-property design tokens (`src/app.css`), vitest + jsdom, `tsc`.

## Global Constraints

- **Design tokens only** — every color is a `var(--…)` token, no raw literals (`src/style-token-guard.test.ts` / ZEB-605). Tokens used: `--border`, `--bg-tertiary`, `--text-muted`, `--accent`, `--on-accent`, `--danger` (all defined in both light and dark themes).
- **No markup change** — all target buttons already live inside `.actions` divs.
- **Existing behavioral tests stay green** — the 89 IdentityPanel tests assert testids/handlers/flow, untouched by a style-only change.
- **Frontend gates from repo root:** `npx tsc --noEmit`, `npx vitest run`.

---

### Task 1: Add the `.actions button` themed treatment

**Files:**
- Modify: `src/lib/components/IdentityPanel.svelte` (`<style>` block, around the existing `.actions` / `button.danger` rules at lines 1212–1214)
- Test (regression only): `src/lib/components/__tests__/IdentityPanel.test.ts` (unchanged — must stay green), `src/style-token-guard.test.ts` (unchanged — must stay green)

**Interfaces:**
- Consumes: design tokens from `src/app.css`; the pre-existing `.actions` layout rule and the `.sidecar-choice button[aria-pressed='true']` override.
- Produces: no new public interface. The rendered buttons gain themed chrome; DOM structure, classes on markup, testids, and handlers are all unchanged.

- [ ] **Step 1: Note the "failing" state**

No new unit test — jsdom cannot compute UA button chrome (`border-style: outset`), so the defect is not observable in vitest. The pre-change baseline is captured by the ticket's CDP measurement (UA-default `outset`/2px/black/`rgb(240,240,240)`/`0`). Verification is: existing suites stay green + style-token-guard passes + treatment matches the ZEB-773 reference token-for-token.

- [ ] **Step 2: Replace the `button.danger` rule with the base treatment + raised-specificity danger**

In `src/lib/components/IdentityPanel.svelte`, replace:

```css
  /* ZEB-842: destructive-action affordance (Erase all local data). */
  button.danger { border-color: var(--danger); color: var(--danger); }
```

with:

```css
  /* ZEB-832: base treatment for every button in an .actions row (idle Backup…/
     Restore…/Erase all data… and the wizard step actions). Without this the UA
     default (2px black outset bevel) showed through — mirrors the ZEB-773
     reference in src/app.css. */
  .actions button {
    font: inherit;
    padding: 6px 12px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg-tertiary);
    color: var(--text-muted);
    cursor: pointer;
  }
  .actions button:hover:not(:disabled) {
    background: var(--accent);
    color: var(--on-accent);
    border-color: var(--accent);
  }
  .actions button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .actions button:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
  /* ZEB-842 destructive affordance — raised to .actions button.danger (0,2,1)
     so it still beats the .actions button base (0,1,1). */
  .actions button.danger { border-color: var(--danger); color: var(--danger); }
```

- [ ] **Step 3: Type-check**

Run: `npx tsc --noEmit`
Expected: clean (no TS surface changed).

- [ ] **Step 4: Run the style-token guard + IdentityPanel tests**

Run: `npx vitest run src/style-token-guard.test.ts src/lib/components/__tests__/IdentityPanel.test.ts src/lib/components/__tests__/StartupRecoveryOptions.test.ts`
Expected: PASS (no raw literals; markup/handlers unchanged).

- [ ] **Step 5: Run the full frontend suite**

Run: `npx vitest run`
Expected: all green (parity with the pre-change baseline).

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/IdentityPanel.svelte
git commit -m "fix(zeb-832): theme the Account .actions buttons (IdentityPanel)"
```

## Self-Review

- **Spec coverage:** the sole spec requirement (base `.actions button` treatment + raised danger specificity, tokens only, no markup change) is implemented in Task 1, Step 2. ✓
- **Placeholder scan:** none. ✓
- **Type consistency:** no types involved (CSS-only); selector names (`.actions`, `.actions button.danger`, `.sidecar-choice button[aria-pressed='true']`) match the existing DOM and style block. ✓
