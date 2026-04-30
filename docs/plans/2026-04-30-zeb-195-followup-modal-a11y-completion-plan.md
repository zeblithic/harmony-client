# ZEB-195 follow-up Implementation Plan: Modal a11y completion + ZEB-186

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate the 3 simple confirm dialogs deferred from ZEB-195 (`ConfirmDialog`, `TypeToConfirmDialog`, `DoubleConfirmDialog`) to consume the `<Modal>` primitive, document the pairing modals' permissive `canCancel` default, and add `role="alert"` to IdentityPanel's top-level error paragraphs (closes ZEB-186).

**Architecture:** Reuse the `<Modal>` component + `use:trapFocus` action shipped in ZEB-195. No new artifacts. Each migration is a near-mechanical wrapper swap.

**Tech Stack:** Svelte 5 runes/snippets/actions, TypeScript, vitest + jsdom + @testing-library/svelte.

**Spec:** `docs/specs/2026-04-30-zeb-195-followup-modal-a11y-completion-design.md`

---

## Task 1: Migrate `ConfirmDialog.svelte` to `<Modal>`

**Files:**
- Modify: `src/lib/components/ConfirmDialog.svelte`
- Modify: `src/lib/components/__tests__/ConfirmDialog.test.ts`

The simplest migration. ConfirmDialog has manual focus on first button via `onMount` and an Escape handler on the overlay. Both behaviors are now provided by `<Modal>` + `use:trapFocus`.

- [ ] **Step 1: Audit existing tests for `.dialog-overlay` / `.dialog` selectors**

```bash
grep -nE "\.dialog-overlay|\.dialog\b|getByRole\('dialog'\)" src/lib/components/__tests__/ConfirmDialog.test.ts
```

Note any selectors targeting the deleted classes — those need updates after the migration. `getByRole('dialog')` survives because `<Modal>` renders `role="dialog"` itself.

- [ ] **Step 2: Add the smoke test (will pass against existing markup)**

Append to `src/lib/components/__tests__/ConfirmDialog.test.ts`:

```ts
  it('renders inside <Modal> with correct a11y attributes', () => {
    const { getByRole } = render(ConfirmDialog, {
      props: {
        title: 'Confirm',
        message: 'Are you sure?',
        confirmLabel: 'Yes',
        onConfirm: () => {},
        onCancel: () => {},
      },
    });
    const dialog = getByRole('dialog');
    expect(dialog.getAttribute('aria-modal')).toBe('true');
    expect(dialog.getAttribute('aria-labelledby')).toMatch(/^dialog-title-/);
  });
```

This test passes against the existing markup AND the migrated markup — it's a regression net for the contract that survives the migration.

- [ ] **Step 3: Run tests — verify the new test passes pre-migration**

```bash
npx vitest run src/lib/components/__tests__/ConfirmDialog.test.ts
```

Expected: all existing tests + new smoke test PASS.

- [ ] **Step 4: Migrate `ConfirmDialog.svelte`**

Replace the entire file content with:

```svelte
<script lang="ts">
  import Modal from './Modal.svelte';

  let {
    title,
    message,
    confirmLabel,
    destructive = false,
    onConfirm,
    onCancel,
  }: {
    title: string;
    message: string;
    confirmLabel: string;
    destructive?: boolean;
    onConfirm: () => void;
    onCancel: () => void;
  } = $props();

  const titleId = `dialog-title-${Math.random().toString(36).slice(2)}`;
</script>

<Modal onCancel={onCancel} ariaLabelledby={titleId}>
  <h2 class="dialog-title" id={titleId}>{title}</h2>
  <p class="dialog-message">{message}</p>
  <div class="dialog-actions">
    <button class="cancel-btn" onclick={onCancel}>Cancel</button>
    <button
      class="confirm-btn"
      class:destructive
      onclick={onConfirm}
    >
      {confirmLabel}
    </button>
  </div>
</Modal>

<style>
  .dialog-title {
    color: var(--text-primary);
    font-size: 1.1rem;
    margin: 0 0 12px;
  }

  .dialog-message {
    color: var(--text-secondary);
    font-size: 0.9rem;
    line-height: 1.5;
    margin: 0 0 20px;
  }

  .dialog-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }

  .cancel-btn {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    border: none;
    padding: 8px 16px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }

  .confirm-btn {
    background: var(--accent);
    color: var(--text-primary);
    border: none;
    padding: 8px 16px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }

  .confirm-btn.destructive {
    background: #d83c3e;
  }

  .confirm-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .cancel-btn:focus-visible,
  .confirm-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
</style>
```

Differences from original: drops `onMount` import + dialogEl binding + outer wrapper divs + `.dialog-overlay` and `.dialog` CSS rules. Adds `import Modal from './Modal.svelte';`.

- [ ] **Step 5: Run all gates**

```bash
npx vitest run src/lib/components/__tests__/ConfirmDialog.test.ts
npx tsc --noEmit
npx vitest run
```

All green. If any test fails on a removed CSS selector identified in Step 1, update the selector to `getByRole('dialog')` or query by class on inner content (`.dialog-actions`, `.dialog-title`, etc.).

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/ConfirmDialog.svelte src/lib/components/__tests__/ConfirmDialog.test.ts
git commit -m "feat(zeb-195-followup): migrate ConfirmDialog to <Modal>"
```

---

## Task 2: Migrate `TypeToConfirmDialog.svelte` to `<Modal>`

**Files:**
- Modify: `src/lib/components/TypeToConfirmDialog.svelte`
- Modify: `src/lib/components/__tests__/TypeToConfirmDialog.test.ts`

Same shape as Task 1, but with an input element. The input is first in DOM order inside the modal content, so trapFocus's first-focusable rule lands on it — preserves the existing UX where the user can immediately type the confirmation phrase.

- [ ] **Step 1: Audit existing tests**

```bash
grep -nE "\.dialog-overlay|\.dialog\b|getByRole\('dialog'\)" src/lib/components/__tests__/TypeToConfirmDialog.test.ts
```

Note any class-targeted selectors needing updates.

- [ ] **Step 2: Add the smoke test**

Append to `src/lib/components/__tests__/TypeToConfirmDialog.test.ts`:

```ts
  it('renders inside <Modal> with correct a11y attributes and focuses the input', async () => {
    const { getByRole, getByLabelText } = render(TypeToConfirmDialog, {
      props: {
        title: 'Confirm',
        message: 'Type to confirm',
        confirmText: 'DELETE',
        confirmLabel: 'Delete',
        onConfirm: () => {},
        onCancel: () => {},
      },
    });
    const dialog = getByRole('dialog');
    expect(dialog.getAttribute('aria-modal')).toBe('true');
    expect(dialog.getAttribute('aria-labelledby')).toMatch(/^dialog-title-/);
    // Verify input is first-focused (preserves the existing UX).
    const input = getByLabelText('Type to confirm');
    expect(document.activeElement).toBe(input);
  });
```

- [ ] **Step 3: Run tests pre-migration**

```bash
npx vitest run src/lib/components/__tests__/TypeToConfirmDialog.test.ts
```

The smoke test should pass against the existing markup (existing `onMount(() => inputEl?.focus())` does the same thing trapFocus will do).

- [ ] **Step 4: Migrate `TypeToConfirmDialog.svelte`**

Replace the entire file content with:

```svelte
<script lang="ts">
  import Modal from './Modal.svelte';

  let {
    title,
    message,
    confirmText,
    confirmLabel,
    destructive = false,
    onConfirm,
    onCancel,
  }: {
    title: string;
    message: string;
    confirmText: string;
    confirmLabel: string;
    destructive?: boolean;
    onConfirm: () => void;
    onCancel: () => void;
  } = $props();

  let typed = $state('');
  let matches = $derived(typed === confirmText);
  const titleId = `dialog-title-${Math.random().toString(36).slice(2)}`;
</script>

<Modal onCancel={onCancel} ariaLabelledby={titleId}>
  <h2 class="dialog-title" id={titleId}>{title}</h2>
  <p class="dialog-message">{message}</p>
  <p class="dialog-hint">Type <code>{confirmText}</code> to confirm</p>
  <input
    class="dialog-input"
    type="text"
    aria-label="Type to confirm"
    bind:value={typed}
  />
  <div class="dialog-actions">
    <button class="cancel-btn" onclick={onCancel}>Cancel</button>
    <button
      class="confirm-btn"
      class:destructive
      disabled={!matches}
      onclick={onConfirm}
    >
      {confirmLabel}
    </button>
  </div>
</Modal>

<style>
  .dialog-title {
    color: var(--text-primary);
    font-size: 1.1rem;
    margin: 0 0 12px;
  }

  .dialog-message {
    color: var(--text-secondary);
    font-size: 0.9rem;
    line-height: 1.5;
    margin: 0 0 12px;
  }

  .dialog-hint {
    color: var(--text-secondary);
    font-size: 0.85rem;
    margin: 0 0 8px;
  }

  .dialog-hint code {
    background: var(--bg-tertiary);
    padding: 2px 6px;
    border-radius: 3px;
    font-family: monospace;
    color: var(--text-primary);
  }

  .dialog-input {
    width: 100%;
    padding: 8px 12px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    font-size: 0.9rem;
    margin-bottom: 20px;
    box-sizing: border-box;
  }

  .dialog-input:focus {
    outline: 2px solid var(--accent);
    outline-offset: -1px;
  }

  .dialog-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }

  .cancel-btn {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    border: none;
    padding: 8px 16px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }

  .confirm-btn {
    background: var(--accent);
    color: var(--text-primary);
    border: none;
    padding: 8px 16px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }

  .confirm-btn.destructive {
    background: #d83c3e;
  }

  .confirm-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .cancel-btn:focus-visible,
  .confirm-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
</style>
```

Differences from original: drops `onMount` import + `inputEl` binding + outer wrapper divs + `.dialog-overlay` and `.dialog` CSS rules. Adds `import Modal from './Modal.svelte';`.

- [ ] **Step 5: Run all gates**

```bash
npx vitest run src/lib/components/__tests__/TypeToConfirmDialog.test.ts
npx tsc --noEmit
npx vitest run
```

All green.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/TypeToConfirmDialog.svelte src/lib/components/__tests__/TypeToConfirmDialog.test.ts
git commit -m "feat(zeb-195-followup): migrate TypeToConfirmDialog to <Modal>"
```

---

## Task 3: Migrate `DoubleConfirmDialog.svelte` with `$effect` rescoping

**Files:**
- Modify: `src/lib/components/DoubleConfirmDialog.svelte`
- Modify: `src/lib/components/__tests__/DoubleConfirmDialog.test.ts`

The non-mechanical migration. The `$effect` that re-focuses the first button on `gate` change must survive — but it needs to query a content ref instead of the now-removed `dialogEl`.

- [ ] **Step 1: Audit existing tests + identify gate-transition test**

```bash
grep -nE "\.dialog-overlay|\.dialog\b|gate|Continue|gate ?= ?2" src/lib/components/__tests__/DoubleConfirmDialog.test.ts
```

Note any class selectors AND identify the existing test that exercises the gate=1→2 transition. That test is the regression net for the `$effect` rescoping.

- [ ] **Step 2: Add the smoke test**

Append to `src/lib/components/__tests__/DoubleConfirmDialog.test.ts`:

```ts
  it('renders inside <Modal> with correct a11y attributes', () => {
    const { getByRole } = render(DoubleConfirmDialog, {
      props: {
        title: 'Double confirm',
        firstMessage: 'Are you sure?',
        secondMessage: 'Really really sure?',
        confirmLabel: 'Yes',
        onConfirm: () => {},
        onCancel: () => {},
      },
    });
    const dialog = getByRole('dialog');
    expect(dialog.getAttribute('aria-modal')).toBe('true');
    expect(dialog.getAttribute('aria-labelledby')).toMatch(/^dialog-title-/);
  });
```

- [ ] **Step 3: Run tests pre-migration**

```bash
npx vitest run src/lib/components/__tests__/DoubleConfirmDialog.test.ts
```

All existing + new smoke test PASS.

- [ ] **Step 4: Migrate `DoubleConfirmDialog.svelte` with `$effect` rescoping**

Replace the entire file content with:

```svelte
<script lang="ts">
  import Modal from './Modal.svelte';

  let {
    title,
    firstMessage,
    secondMessage,
    confirmLabel,
    destructive = false,
    onConfirm,
    onCancel,
  }: {
    title: string;
    firstMessage: string;
    secondMessage: string;
    confirmLabel: string;
    destructive?: boolean;
    onConfirm: () => void;
    onCancel: () => void;
  } = $props();

  let gate = $state(1);
  const titleId = `dialog-title-${Math.random().toString(36).slice(2)}`;
  let contentEl: HTMLElement;

  // Re-focus first button when gate transitions 1→2 — trapFocus only acts
  // on mount/unmount, not on internal state changes that swap the visible
  // button set. On mount this no-ops because trapFocus already focused the
  // first button.
  $effect(() => {
    void gate;
    contentEl?.querySelector<HTMLElement>('button')?.focus();
  });
</script>

<Modal onCancel={onCancel} ariaLabelledby={titleId}>
  <h2 class="dialog-title" id={titleId}>{title}</h2>
  <div bind:this={contentEl}>
    {#if gate === 1}
      <p class="dialog-message">{firstMessage}</p>
      <div class="dialog-actions">
        <button class="cancel-btn" onclick={onCancel}>Cancel</button>
        <button class="confirm-btn" onclick={() => gate = 2}>Continue</button>
      </div>
    {:else}
      <p class="dialog-message">{secondMessage}</p>
      <div class="dialog-actions">
        <button class="cancel-btn" onclick={onCancel}>Cancel</button>
        <button
          class="confirm-btn"
          class:destructive
          onclick={onConfirm}
        >
          {confirmLabel}
        </button>
      </div>
    {/if}
  </div>
</Modal>

<style>
  .dialog-title {
    color: var(--text-primary);
    font-size: 1.1rem;
    margin: 0 0 12px;
  }

  .dialog-message {
    color: var(--text-secondary);
    font-size: 0.9rem;
    line-height: 1.5;
    margin: 0 0 20px;
  }

  .dialog-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }

  .cancel-btn {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    border: none;
    padding: 8px 16px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }

  .confirm-btn {
    background: var(--accent);
    color: var(--text-primary);
    border: none;
    padding: 8px 16px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }

  .confirm-btn.destructive {
    background: #d83c3e;
  }

  .confirm-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .cancel-btn:focus-visible,
  .confirm-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
</style>
```

Key differences from original:
- `dialogEl` binding becomes `contentEl` bound to a wrapper `<div>` around the conditional content (NOT the entire dialog — wrapping the whole modal would include the `<h2>` heading which isn't a button).
- `$effect` queries `contentEl` instead of `dialogEl`.
- Outer wrapper divs and `.dialog-overlay`/`.dialog` CSS removed.
- `import Modal from './Modal.svelte';` added.

- [ ] **Step 5: Run all gates with attention to the gate-transition test**

```bash
npx vitest run src/lib/components/__tests__/DoubleConfirmDialog.test.ts
npx tsc --noEmit
npx vitest run
```

All green. If the gate-transition test fails, the `contentEl` wrapper is likely too narrow or too wide — re-read the migration shape above.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/DoubleConfirmDialog.svelte src/lib/components/__tests__/DoubleConfirmDialog.test.ts
git commit -m "feat(zeb-195-followup): migrate DoubleConfirmDialog to <Modal>; rescope \$effect"
```

---

## Task 4: Add clarifying comments to pairing modals

**Files:**
- Modify: `src/lib/components/PairingInviter.svelte`
- Modify: `src/lib/components/PairingJoiner.svelte`

Pure documentation. Addresses PR #68 round-2 feedback that asked for either gating or a comment.

- [ ] **Step 1: Add comment in `PairingInviter.svelte`**

Find the `<Modal onCancel={handleCancel} ariaLabelledby="invite-heading">` element (currently at line 39). Insert a comment block immediately above it:

```svelte
<!--
  canCancel intentionally omitted (defaults to true). Pairing's existing
  Cancel button is enabled in every non-terminal state — even during
  active operations like enroll — so Esc mirrors that always-available
  dismissal. The terminal-state IPC skip lives in handleCancel above;
  this Modal stays permissive on Esc.
-->
<Modal onCancel={handleCancel} ariaLabelledby="invite-heading">
```

- [ ] **Step 2: Add the same-shape comment in `PairingJoiner.svelte`**

Find the `<Modal onCancel={handleCancel} ariaLabelledby="join-heading">` element (currently at line 56). Insert the comment block above it:

```svelte
<!--
  canCancel intentionally omitted (defaults to true). Pairing's existing
  Cancel button is enabled in every non-terminal state — even during
  active operations like enroll/start — so Esc mirrors that always-
  available dismissal. The terminal-state IPC skip lives in handleCancel
  above; this Modal stays permissive on Esc.
-->
<Modal onCancel={handleCancel} ariaLabelledby="join-heading">
```

(Note: PairingJoiner says "enroll/start" because it has the additional `starting` state that PairingInviter doesn't.)

- [ ] **Step 3: Run all gates**

```bash
npx tsc --noEmit
npx vitest run
```

All green. No test changes needed — comments don't affect rendered DOM.

- [ ] **Step 4: Commit**

```bash
git add src/lib/components/PairingInviter.svelte src/lib/components/PairingJoiner.svelte
git commit -m "docs(zeb-195-followup): clarify why pairing modals omit canCancel (PR #68 round-2 feedback)"
```

---

## Task 5: ZEB-186 — `role="alert"` on IdentityPanel top-level errors

**Files:**
- Modify: `src/lib/components/IdentityPanel.svelte`
- Modify: `src/lib/components/__tests__/IdentityPanel.test.ts`

Closes ZEB-186. Four single-line attribute additions + one new test assertion.

- [ ] **Step 1: Identify the 4 target lines**

```bash
grep -nE 'class="error"' src/lib/components/IdentityPanel.svelte
```

Should find exactly 4 matches (the inline-error displays at lines 795/839/907 use `class="inline-error"` and already have `role="alert"`). The 4 top-level error lines are 549, 603, 734, and 920.

If the line numbers have drifted since this plan was written, adapt — what matters is the 4 `<p class="error">{...}</p>` patterns that lack `role="alert"`.

- [ ] **Step 2: Add `role="alert"` to all 4 occurrences**

For each of the 4 lines, replace `<p class="error">` with `<p class="error" role="alert">`. The exact replacements (using Edit's `replace_all=false`, matching enough surrounding context to make each unique):

Line 549:
```
<p class="error">{loadError}</p>
```
→
```
<p class="error" role="alert">{loadError}</p>
```

Line 603:
```
<p class="error">{wizardState.step.loadError}</p>
```
→
```
<p class="error" role="alert">{wizardState.step.loadError}</p>
```

Lines 734 and 920 (BOTH are `<p class="error">{wizardState.step.error}</p>` — they are not unique without surrounding context):

For line 734, use surrounding context to disambiguate. Look at the `{:else if wizardState.step.phase === ...}` branch immediately above each. The implementer should use Edit twice with sufficient surrounding context (e.g., 2-3 lines before and after) to make each match unique.

Verify after each edit:
```bash
grep -cE 'role="alert"' src/lib/components/IdentityPanel.svelte
```

Should grow by 1 each time. After all 4 edits, the count should be 7 (3 pre-existing inline-error + 4 new top-level).

- [ ] **Step 3: Extend the existing load-error test**

The existing test at `src/lib/components/__tests__/IdentityPanel.test.ts:174` ("shows error message when identity hash cannot be loaded" inside the `describe('IdentityPanel — error state', ...)` block at line 169) sets up exactly the load-error path that exercises the line-549 `<p class="error">{loadError}</p>`. Extend it with a role-attribute assertion.

Find this in `IdentityPanel.test.ts`:

```ts
  it('shows error message when identity hash cannot be loaded', async () => {
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') throw new Error('identity store locked');
      throw new Error(`unexpected invoke: ${cmd}`);
    });

    render(IdentityPanel);

    await screen.findByText(/could not read identity store/i);
    // Buttons should not be present in error state
    expect(screen.queryByRole('button', { name: /backup/i })).toBeNull();
    expect(screen.queryByRole('button', { name: /restore/i })).toBeNull();
  });
```

Replace the body so the `findByText` result is bound and asserted on:

```ts
  it('shows error message when identity hash cannot be loaded', async () => {
    mockInvoke.mockImplementation(async (cmd: string) => {
      if (cmd === 'current_identity_hash') throw new Error('identity store locked');
      throw new Error(`unexpected invoke: ${cmd}`);
    });

    render(IdentityPanel);

    const errorEl = await screen.findByText(/could not read identity store/i);
    // ZEB-186: top-level error displays carry role="alert" so screen
    // readers announce them.
    expect(errorEl.getAttribute('role')).toBe('alert');
    // Buttons should not be present in error state
    expect(screen.queryByRole('button', { name: /backup/i })).toBeNull();
    expect(screen.queryByRole('button', { name: /restore/i })).toBeNull();
  });
```

A single-flow assertion is sufficient — the 4 fixes are byte-identical (just adding `role="alert"` to the same kind of `<p class="error">` paragraph), so a regression in any one would fail the same kind of check on any path.

- [ ] **Step 4: Run all gates**

```bash
npx vitest run src/lib/components/__tests__/IdentityPanel.test.ts
npx tsc --noEmit
npx vitest run
```

All green.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/IdentityPanel.svelte src/lib/components/__tests__/IdentityPanel.test.ts
git commit -m "$(cat <<'EOF'
fix(zeb-186): role=alert on IdentityPanel top-level error displays

Top-level error paragraphs at lines 549/603/734/920 used class="error"
without role="alert" or aria-live, so screen readers didn't announce
them. The inline-error displays at 795/839/907 already had role="alert".

Now all top-level errors carry role="alert" (which implies aria-live=
assertive + aria-atomic=true), matching the inline-error pattern.

New test pins the role=alert attribute on the load-error flow.

Closes ZEB-186.
EOF
)"
```

---

## Task 6: Final verification gates

After all 5 tasks land green-tested commits, run the full set of gates one more time before opening the PR:

- [ ] **Frontend:**
  ```bash
  npx vitest run
  npx tsc --noEmit
  ```

- [ ] **Backend** (defensive — no Rust changes but CI runs them per memory rule):
  ```bash
  cargo test --manifest-path src-tauri/Cargo.toml
  cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings
  cargo fmt --all --manifest-path src-tauri/Cargo.toml -- --check
  ```

- [ ] **Manual smoke test** (cannot run from subagent context — flag for the user to run before PR merge): launch `npx tauri dev`, exercise the flows that surface the migrated dialogs and verify keyboard-only operation.

- [ ] **Use `superpowers:finishing-a-development-branch`** to push + create PR.
