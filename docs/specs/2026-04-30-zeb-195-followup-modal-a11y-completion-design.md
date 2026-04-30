# ZEB-195 follow-up: Modal a11y completion + ZEB-186

**Date:** 2026-04-30
**Linear:**
- Closes [ZEB-186](https://linear.app/zeblith/issue/ZEB-186) — `role="alert"` on IdentityPanel top-level errors.
- Completes the deferred modal migrations from [ZEB-195](https://linear.app/zeblith/issue/ZEB-195) (3 of the 5 deferred modals; the other 2 are tracked as ZEB-204 / ZEB-205).
- Addresses the late round-2 PR #68 feedback asking for documentation of the pairing modals' permissive `canCancel` default.

**Origin:** PR #68 (ZEB-195) merged with 5 deferred modal migrations + a late documentation request. The 3 simple confirm dialogs are mechanical migrations using the just-shipped primitive; the 2 Vine modals warrant their own scope.

## Goal

Drain the modal a11y debt for the 3 simple confirm dialogs, document the pairing modal design choice, and patch the IdentityPanel's top-level error displays so screen readers actually announce them.

## Scope

**In:**

- Migrate `ConfirmDialog.svelte`, `DoubleConfirmDialog.svelte`, `TypeToConfirmDialog.svelte` to consume the `<Modal>` primitive shipped in ZEB-195. Drop manual focus management, manual Escape handling, and the duplicated `.dialog-overlay` / `.dialog` CSS rules in each file.
- Add a clarifying comment to `PairingInviter.svelte` and `PairingJoiner.svelte` explaining why `canCancel` is intentionally omitted from their `<Modal>` consumers.
- Add `role="alert"` to the 4 top-level error paragraphs in `IdentityPanel.svelte` (closes ZEB-186).

**Out (filed as separate tickets):**

- `VinePublishDialog.svelte` migration → [ZEB-204](https://linear.app/zeblith/issue/ZEB-204). The `✕` close button is first focusable (would regress input-first-focus UX) and the dialog has overlay-click-to-close semantics that `<Modal>` deliberately doesn't support. Three viable paths documented in the ticket.
- `VinePlayer.svelte` migration → [ZEB-205](https://linear.app/zeblith/issue/ZEB-205). Already has manual focus management; needs an audit before migrating.

**Not changing:**

- The `<Modal>` API. No new props. The 3 confirm dialogs fit cleanly with the existing `onCancel` + `canCancel` + `ariaLabelledby` shape.
- The `use:trapFocus` action. The behavior contract stays as-is.

## Architecture

No new artifacts. Each migration is a near-mechanical wrapper swap of the same shape that worked for `PairingInviter` and `PairingJoiner` in ZEB-195. Net code reduction across the three dialogs (drop manual a11y plumbing).

The `<Modal>` primitive at `src/lib/components/Modal.svelte` and the `use:trapFocus` action at `src/lib/actions/trap-focus.ts` are reused unchanged.

## Per-file changes

### `ConfirmDialog.svelte`

**Drop:**
- `import { onMount } from 'svelte'`
- The `dialogEl: HTMLElement` binding
- The `onMount(() => dialogEl?.querySelector<HTMLElement>('button')?.focus())` block
- The outer `<div class="dialog-overlay" onkeydown={(e) => { if (e.key === 'Escape') onCancel(); }}>` wrapper
- The inner `<div class="dialog" role="dialog" aria-modal="true" aria-labelledby={titleId} bind:this={dialogEl}>`
- The `.dialog-overlay` and `.dialog` CSS rules

**Replace with:** `<Modal onCancel={onCancel} ariaLabelledby={titleId}>` wrapping the existing inner content (h2, p, dialog-actions). `canCancel` defaults to true — these are simple synchronous confirm dialogs with no in-flight state to gate against.

**Add:** `import Modal from './Modal.svelte';` to the script block.

**Preserve:** `.dialog-title`, `.dialog-message`, `.dialog-actions`, `.cancel-btn`, `.confirm-btn`, `.confirm-btn.destructive`, `.confirm-btn:disabled`, `:focus-visible` rules.

### `TypeToConfirmDialog.svelte`

**Drop:**
- `import { onMount }`
- The `inputEl: HTMLInputElement` binding (only used for focus)
- The `onMount(() => inputEl?.focus())` block
- The outer wrapper divs and the `.dialog-overlay` / `.dialog` CSS

**Replace with:** `<Modal>` wrapper. The `<input class="dialog-input" ...>` is first in DOM order inside the wrapper, so trapFocus's first-focusable rule lands on it — preserves the existing UX where the user can immediately type the confirmation phrase.

**Add:** `import Modal from './Modal.svelte';`

**Preserve:** `.dialog-title`, `.dialog-message`, `.dialog-hint`, `.dialog-input`, `.dialog-actions`, `.cancel-btn`, `.confirm-btn`, `.confirm-btn.destructive`, `.confirm-btn:disabled`, `:focus-visible` rules.

### `DoubleConfirmDialog.svelte`

**Drop:**
- The `dialogEl: HTMLElement` binding
- The outer wrapper divs and the `.dialog-overlay` / `.dialog` CSS

**Replace with:** `<Modal>` wrapper. **Keep with rescoping:** the `$effect(() => { void gate; ... })` block needs to survive the migration to handle the `gate` 1→2 transition (when the user clicks Continue, the Cancel/Continue pair unmounts and a new Cancel/Confirm pair mounts — focus must move to the new first button or it falls back to body). Replace the `dialogEl` ref with a new `contentEl: HTMLElement` ref bound to a wrapper div around the conditional content:

```svelte
<Modal onCancel={onCancel} ariaLabelledby={titleId}>
  <h2 class="dialog-title" id={titleId}>{title}</h2>
  <div bind:this={contentEl}>
    {#if gate === 1}
      ...
    {:else}
      ...
    {/if}
  </div>
</Modal>
```

```ts
$effect(() => {
  void gate;
  contentEl?.querySelector<HTMLElement>('button')?.focus();
});
```

The effect runs on mount AND on every `gate` change. On mount, both trapFocus and the effect want to focus the first button — they end up focusing the same element, so no conflict. On gate transition, trapFocus doesn't re-fire (it only acts on mount/unmount), so the effect is the load-bearing focus mover.

**Add:** `import Modal from './Modal.svelte';`

**Preserve:** all other styles and the existing two-page state machine logic.

### `PairingInviter.svelte` and `PairingJoiner.svelte`

**No code behavior change.** Add a comment block above the `<Modal>` consumer in each file:

```svelte
<!--
  canCancel intentionally omitted (defaults to true). Pairing's existing
  Cancel button is enabled in every non-terminal state — even during
  active operations like enroll/start — so Esc mirrors that always-
  available dismissal. The terminal-state IPC skip lives in handleCancel
  (see comment on that function); this Modal stays permissive on Esc.
-->
<Modal onCancel={handleCancel} ariaLabelledby="invite-heading">
```

(Same comment shape in PairingJoiner with `ariaLabelledby="join-heading"`.)

This addresses the round-2 PR #68 feedback that asked for either gating against in-flight operations OR a comment justifying the always-cancellable behavior.

### `IdentityPanel.svelte` (closes ZEB-186)

Four single-line changes — add `role="alert"` to the top-level error paragraphs:

| Line | Current | Replace with |
|------|---------|--------------|
| 549 | `<p class="error">{loadError}</p>` | `<p class="error" role="alert">{loadError}</p>` |
| 603 | `<p class="error">{wizardState.step.loadError}</p>` | `<p class="error" role="alert">{wizardState.step.loadError}</p>` |
| 734 | `<p class="error">{wizardState.step.error}</p>` | `<p class="error" role="alert">{wizardState.step.error}</p>` |
| 920 | `<p class="error">{wizardState.step.error}</p>` | `<p class="error" role="alert">{wizardState.step.error}</p>` |

The 3 inline-error displays at lines 795, 839, 907 already have `role="alert"` and stay unchanged.

`role="alert"` implies `aria-live="assertive"` and `aria-atomic="true"` — no separate `aria-live` attribute needed.

## Test strategy

### Per-dialog migration

Each of the 3 confirm dialogs has an existing test file under `src/lib/components/__tests__/`:

- `ConfirmDialog.test.ts`
- `DoubleConfirmDialog.test.ts`
- `TypeToConfirmDialog.test.ts`

**Pass-through assumption:** assertions using `getByRole('dialog')`, `aria-modal`, `aria-labelledby`, focus-on-first-focusable, and Escape→onCancel survive the migration because `<Modal>` and `use:trapFocus` provide all of these. Tests that target removed CSS class names like `.dialog-overlay` need their selectors updated; the implementer scans each test file and adjusts as needed.

**One new smoke test per migrated dialog** under the same test file: assert `role="dialog"` + `aria-modal="true"` + correct `aria-labelledby` after migration. Same shape as the PairingInviter/PairingJoiner smoke tests in ZEB-195.

**For DoubleConfirmDialog:** verify both gate=1 (Continue button focused after mount) and gate=2 (Confirm button focused after the 1→2 transition) work. The existing test for gate transition behavior is the regression net for the `$effect` rescoping.

### IdentityPanel ZEB-186 fix

Existing tests at `src/lib/components/__tests__/IdentityPanel.test.ts` cover the 4 error paths. Add one new assertion (extend an existing test or add a fresh one) checking that a top-level error paragraph carries `role="alert"`. A single-flow assertion (e.g., the load-error path) is sufficient — the 4 fixes are identical and a regression in any one would fail the same kind of check on any path.

### Pairing modal comments

No new tests needed — pure documentation change.

## Migration order

Each step lands a green-tested commit:

1. Migrate `ConfirmDialog.svelte`. Smoke test added. Existing tests pass.
2. Migrate `TypeToConfirmDialog.svelte`. Smoke test added. Existing tests pass.
3. Migrate `DoubleConfirmDialog.svelte` with the `$effect` rescoping. Smoke test added. Existing gate-transition test still passes.
4. Add the clarifying comments to `PairingInviter.svelte` and `PairingJoiner.svelte`.
5. Add `role="alert"` to the 4 IdentityPanel error paragraphs. Add the new IdentityPanel test assertion.
6. Final verification gates (vitest, tsc, defensive cargo gates per memory rule).

The pairing comment + IdentityPanel fix could ship in a single commit since they're both small and unrelated to each other; the implementer can choose. Each migration is its own commit because each has its own test scaffolding and risk surface.

## Risk surface

- **`DoubleConfirmDialog` `$effect` rescoping** — the only non-mechanical change. The new `contentEl` must wrap the conditional block, not the entire dialog. If the wrapper is too narrow (e.g., wraps only one branch), focus shift breaks; if too wide (wraps the heading too), it still works but the heading isn't the focus target. Existing test asserting Continue→Confirm flow catches the obvious failures.
- **Test selectors targeting deleted CSS classes** — each test file needs a quick scan for `.dialog-overlay` / `.dialog` selectors before committing the migration. Likely none — existing tests prefer `getByRole('dialog')` — but worth confirming.
- **CSS visual regression** — the original dialogs use `rgba(0, 0, 0, 0.6)` overlay; `Modal.svelte` uses `rgba(0, 0, 0, 0.5)`. 0.1 opacity delta. Visually nearly identical; not worth a CSS extension API to expose. Manual smoke test confirms acceptable.
- **The `IdentityPanel` test file is large** — the new `role="alert"` assertion should attach to an existing relevant test (e.g., one that already exercises the load-error path) rather than create a new flow setup. Reduces test surface bloat.

## Acceptance criteria

- All 3 simple confirm dialogs migrated to `<Modal>`. Existing tests pass; one smoke test added per dialog.
- `DoubleConfirmDialog` gate=1→2 focus transition still works (handled by the rescoped `$effect`).
- Pairing modals carry a clarifying comment explaining the permissive `canCancel` default.
- All 4 IdentityPanel top-level error paragraphs carry `role="alert"` (closes ZEB-186).
- No regressions to existing tests.
- No changes to `<Modal>` or `use:trapFocus`.

## Verification gates

Before requesting PR review:

- `npx vitest run` — full suite passes (1332 + new smoke tests).
- `npx tsc --noEmit` — clean.
- `cargo test --manifest-path src-tauri/Cargo.toml` — green (defensive — no Rust changes).
- `cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings` — clean.
- `cargo fmt --all -- --check` — clean (per memory rule).
- Manual smoke: launch GUI → open each migrated dialog (where reachable) → keyboard-only operation works (Tab, Esc, focus restore).
