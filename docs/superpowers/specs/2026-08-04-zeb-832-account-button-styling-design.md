# ZEB-832 — Theme the Account action buttons (IdentityPanel.svelte) — Design

**Ticket:** [ZEB-832](https://linear.app/zeblith/issue/ZEB-832) (Medium, Bug, harmony-client)
**Type:** Frontend / CSS-only. No markup, no logic, no backend.
**Follows:** ZEB-773 / PR #553 (which themed the relay buttons but skipped `IdentityPanel.svelte` — this is the unshipped half of ZEB-773's scope).

## Problem (verified against current source, post-ZEB-842)

`src/lib/components/IdentityPanel.svelte`'s idle `.actions` row holds three
buttons — `Backup…`, `Restore…`, and (as of ZEB-842) `Erase all data…`. The
component's `<style>` has **no** base `button` or `.actions button` rule; the
only button-targeting rule is:

```css
button.danger { border-color: var(--danger); color: var(--danger); }
```

Because that rule sets `border-color`/`color` but never `border-style`, and no
base rule supplies one, every button in the file inherits the user-agent
default chrome — a 2px black `outset` bevel, `rgb(240,240,240)` fill, `0`
radius (measured over CDP on AVALON, Windows 11):

- `Backup…` / `Restore…` — full UA-default grey bevels (the reported symptom).
- `Erase all data…` — a UA-default bevel *with* a red border/text. A latent
  visual defect shipped in ZEB-842; this change corrects it as a side effect.
- The 37 wizard-step `.actions` buttons (`Cancel`, `Continue`, `Done`,
  `Back to settings`, the `.sidecar-choice` toggles, …) — also UA-default,
  unreported because the AVALON run only opened the idle Account view.

Root cause is the *absence* of a base treatment, not a wrong one.

## Approach

Three options were considered:

- **(A) Reuse ZEB-773's global `.relay-*` classes.** Rejected — semantically
  wrong (relay-named classes on identity controls); misleads future readers.
- **(B) Add a new *global* shared button class in `src/app.css`.** Rejected —
  ZEB-773 went global *specifically* because its treatment was duplicated
  across two components and had already drifted (`.relay-input` was `3px` in
  one file, `5px` in the other). That dedup rationale does not apply here:
  these buttons live only in `IdentityPanel.svelte`, so a global class adds
  namespace surface for no benefit.
- **(C) Add a local `.actions button` base rule** in the component's own
  `<style>`. **Chosen.** It is exactly the fix the ticket suggests ("add an
  `.actions button { … }` rule"), scoped to the single component that owns
  these buttons, and it fixes all three problems (reported bug + Erase bevel +
  wizard buttons) with one rule. Since every affected button already sits
  inside an `.actions` div, **no markup changes are required.**

## The change (IdentityPanel.svelte `<style>` only)

Add a base treatment mirroring the ZEB-773 reference (`src/app.css:364`)
token-for-token, and raise the danger rule's specificity so it still wins:

```css
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
.actions button.danger {   /* was `button.danger` — raised so it beats `.actions button` */
  border-color: var(--danger);
  color: var(--danger);
}
```

### Why this composes cleanly

- **Tokens** all exist in both light and dark themes: `--border`,
  `--bg-tertiary`, `--text-muted`, `--accent`, `--on-accent`, `--danger`.
- **Specificity:** `.actions button` is `(0,1,1)`. The overrides
  `.actions button.danger` and the pre-existing
  `.sidecar-choice button[aria-pressed='true']` (which adds
  `border: 1px solid var(--accent)`) are both `(0,2,1)`, so they continue to
  win their respective properties. The base rule supplies the resting chrome;
  the specific rules layer intent on top.
- **Disabled state is load-bearing here:** `Backup…`/`Restore…` carry
  `disabled={!hashLoaded}` and render disabled during the brief identity-hash
  load, so the `:disabled` rule keeps that state legible rather than letting it
  inherit a hover.

## Testing

`IdentityPanel.svelte` markup is unchanged, so the existing 89 behavioral
tests (testids, handlers, wizard flow) must stay green — that is the
regression guard for "didn't break anything."

A computed-style assertion (`border-style !== 'outset'`) is **not** feasible in
the vitest/jsdom harness: jsdom does not compute user-agent button chrome, so
the very property that distinguishes a themed button from a UA-default one is
unobservable there. This is the same reason ZEB-773 / PR #553 was verified
visually over CDP on a running app, not by a unit test.

The enforceable guardrails:

1. **`src/style-token-guard.test.ts`** — every color must be a `var(--…)`
   token, no raw literals. The change uses only tokens, so it passes.
2. **`npx tsc --noEmit`** — clean (no TS surface touched).
3. **`npx vitest run`** — the full frontend suite stays green.

Visual correctness is guaranteed structurally: the treatment is copied
token-for-token from the already-verified ZEB-773 reference, so matching that
reference *is* the visual spec.

## Out of scope

- No change to `src/app.css` or any other component.
- No new global button class or design-system refactor.
- No markup/logic/behavior change in `IdentityPanel.svelte`.
