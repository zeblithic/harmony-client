# ZEB-656 — confirm-dialog family + Modal.svelte shared elevation

**Goal:** Bring the confirm-dialog family to Commons anatomy and give the shared
`Modal.svelte` its canonical elevation, so ~20 Modal-based dialogs read as
warm-raised paper instead of the flat Discord-era `--bg-secondary`.

**Source:** `docs/design/commons/gap-fill-audit.md` §5 (ZEB-656). Reference
idioms (already aligned, excluded): `governance/GovConfirmModal.svelte`
(elevation), `ForkConfirmDialog.svelte` (button/cancel anatomy).

## Global constraints
- **Budget-0**: only `var(--*)` / color-mix of vars in `<style>`. No raw colors.
  `style-token-allowlist.json` stays **byte-identical** (no new literals, none removed).
- Reference-pinned target values only — no improvised design. Tokens verified
  present in `app.css` (light+dark): `--surface-raised`, `--shadow-e2`,
  `--border`, `--font-display`.
- Gates: `npx tsc --noEmit && npx vitest run` (repo root) + `style-token-guard`.
- Frontend-only; no Rust/IPC/behavior change.

## The changes

### 1. `Modal.svelte` (SHARED — 31 consumers, app-wide blast radius)
- `.modal` background `var(--bg-secondary)` → `var(--surface-raised)`.
- Add `box-shadow: var(--shadow-e2)`.
- Radius (8px) + border already correct — leave.
- ⚠️ Every Modal-based dialog gains elevation → static consumer audit + visual
  smoke-test before merge.

### 2. NEW `ConfirmDialogContent.svelte` (dedup of the byte-identical pair)
Shared inner: `<h2 title>` + `<p message>` + `<div actions>` (cancel + confirm).
Props: `title`, `titleId`, `message`, `confirmLabel`, `destructive?`, `onConfirm`,
`onCancel`. Carries the restyled-once `<style>` (title `--font-display`; buttons
7px; `.cancel-btn` → `--surface-raised` + `1px solid var(--border)`; confirm
bg/`.destructive` unchanged). Rendered DOM identical to today → existing tests
are the regression net.

### 3. `ConfirmDialog.svelte`
`<Modal>` wraps `<ConfirmDialogContent … />`. Drop own `<style>`.

### 4. `DoubleConfirmDialog.svelte`
`<Modal>` wraps `contentEl` → `{#if gate===1}<ConfirmDialogContent message=first
confirmLabel="Continue" onConfirm={()=>gate=2}/>{:else}<ConfirmDialogContent
message=second {confirmLabel} {destructive} {onConfirm}/>{/if}`. Keep the
gate-refocus `$effect`. Drop own `<style>`.

### 5–8. In-place restyle (distinct structures — not deduped)
`ConfirmationModal.svelte`, `TypeToConfirmDialog.svelte`,
`TypedConfirmationModal.svelte`, `ReshareConfirmDialog.svelte`:
- Title selector: add `font-family: var(--font-display)`.
- `.confirm-btn` / `.cancel-btn` `border-radius: 4px` → `7px`.
- Inputs (`.dialog-input`, `.typed-input`) `border-radius: 4px` → `5px`.
- `.cancel-btn` `background: var(--bg-tertiary)` + `border: none`
  → `background: var(--surface-raised)` + `border: 1px solid var(--border)`.
- Inline code chips (`.dialog-hint code`, `.required`) stay 3px (chip radius) — leave.
- Confirm-btn bg/color unchanged (on-accent contrast is ZEB-644's scope, not here).

## Out of scope (flagged for review)
- **`InviteLinkManager.svelte`** — the audit's component table buckets it under
  "confirm family" (url-row 6px, button 4px), but the ticket's explicit per-file
  list omits it and it is **not** a Modal-based confirm dialog (inline
  generate/copy widget inside CommunitySettingsPanel, on a non-raised surface).
  Its cancel/secondary treatment depends on that different context. Excluded to
  respect the ticket's scope; trivially folded into a follow-up if desired.

## Verification
- `npx tsc --noEmit` clean.
- `npx vitest run` green — all dialog tests pass unchanged (regression net for
  the dedup refactor).
- `style-token-guard` green; allowlist byte-identical.
- Static audit of all 31 Modal consumers: confirm none depends on the old
  `--bg-secondary` for inner contrast.
- Smoke-test checklist in the PR for the merge-time visual pass.
