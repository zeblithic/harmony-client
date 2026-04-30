# ZEB-195 Implementation Plan: Modal a11y

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a reusable `<Modal>` component + `use:trapFocus` action that provides focus-trap, focus-restore, and Escape-to-cancel for keyboard-only modal operation. Migrate four existing modals (DevicesPanel mint+backup, PairingInviter, PairingJoiner) to consume the primitive.

**Architecture:** Custom Svelte 5 action implementing the focus-trap logic; `<Modal>` wraps `.modal-overlay > .modal[role=dialog]` with the action applied. Consumers keep `{#if openFlag}` mount/unmount control. Zero new dependencies.

**Tech Stack:** Svelte 5 (snippets, runes, `use:` actions), TypeScript, vitest + jsdom + @testing-library/svelte for tests.

**Spec:** `docs/specs/2026-04-30-zeb-195-modal-a11y-design.md`

---

## Task 1: Build `use:trapFocus` action with full test coverage

**Files:**
- Create: `src/lib/actions/trap-focus.ts`
- Create: `src/lib/actions/__tests__/trap-focus.test.ts`

The action is the load-bearing primitive — get this right and the rest is wiring. Build it test-first against the full behavior contract from the spec.

- [ ] **Step 1: Create the action source file with type-only stub**

Create `src/lib/actions/trap-focus.ts`:

```ts
export interface TrapFocusParams {
  onCancel?: () => void;
  canCancel?: boolean;
}

export function trapFocus(_node: HTMLElement, _params: TrapFocusParams) {
  return {
    update(_next: TrapFocusParams) {},
    destroy() {},
  };
}
```

Underscore prefixes prevent unused-arg lint warnings until we wire them up in step 5.

- [ ] **Step 2: Write the focus-on-mount + empty-fallback tests**

Create `src/lib/actions/__tests__/trap-focus.test.ts`:

```ts
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { trapFocus } from '../trap-focus';

describe('trap-focus action', () => {
  let cleanup: { destroy(): void } | undefined;

  beforeEach(() => {
    document.body.innerHTML = '';
  });

  afterEach(() => {
    cleanup?.destroy();
    cleanup = undefined;
    document.body.innerHTML = '';
  });

  it('focuses the first focusable element on mount', () => {
    document.body.innerHTML = `
      <button id="trigger">Open</button>
      <div id="modal">
        <button id="b1">B1</button>
        <button id="b2">B2</button>
      </div>
    `;
    const trigger = document.querySelector<HTMLButtonElement>('#trigger')!;
    trigger.focus();
    const modal = document.querySelector<HTMLElement>('#modal')!;
    cleanup = trapFocus(modal, {});
    expect(document.activeElement?.id).toBe('b1');
  });

  it('focuses the modal container itself when no focusables are present', () => {
    document.body.innerHTML = `
      <button id="trigger">Open</button>
      <div id="modal"><p>No focusables here.</p></div>
    `;
    const trigger = document.querySelector<HTMLButtonElement>('#trigger')!;
    trigger.focus();
    const modal = document.querySelector<HTMLElement>('#modal')!;
    cleanup = trapFocus(modal, {});
    expect(document.activeElement).toBe(modal);
    expect(modal.getAttribute('tabindex')).toBe('-1');
  });

  it('skips disabled buttons when picking the first focusable', () => {
    document.body.innerHTML = `
      <button id="trigger">Open</button>
      <div id="modal">
        <button id="b1" disabled>Disabled</button>
        <button id="b2">B2</button>
      </div>
    `;
    document.querySelector<HTMLButtonElement>('#trigger')!.focus();
    const modal = document.querySelector<HTMLElement>('#modal')!;
    cleanup = trapFocus(modal, {});
    expect(document.activeElement?.id).toBe('b2');
  });
});
```

- [ ] **Step 3: Run tests — verify they fail**

```bash
npx vitest run src/lib/actions/__tests__/trap-focus.test.ts
```

Expected: 3 FAIL — the stub doesn't focus anything, so `activeElement` stays as the trigger button.

- [ ] **Step 4: Implement the focusable selector + mount focus logic**

Replace `src/lib/actions/trap-focus.ts` with:

```ts
export interface TrapFocusParams {
  onCancel?: () => void;
  canCancel?: boolean;
}

const FOCUSABLE_SELECTOR = [
  'button:not(:disabled)',
  '[href]',
  'input:not(:disabled):not([type="hidden"])',
  'select:not(:disabled)',
  'textarea:not(:disabled)',
  '[tabindex]:not([tabindex="-1"])',
].join(', ');

function focusableIn(node: HTMLElement): HTMLElement[] {
  return Array.from(node.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR)).filter(
    (el) => !el.hasAttribute('hidden') && el.getAttribute('aria-hidden') !== 'true',
  );
}

export function trapFocus(node: HTMLElement, params: TrapFocusParams) {
  let current = params;

  const focusables = focusableIn(node);
  if (focusables.length > 0) {
    focusables[0].focus();
  } else {
    node.setAttribute('tabindex', '-1');
    node.focus();
  }

  return {
    update(next: TrapFocusParams) {
      current = next;
    },
    destroy() {},
  };
}
```

`current` is captured (referenced by closure) for use in steps 6 and 9; ESLint/TS may flag it as unused-but-assigned for now — that's fine, it's wired in those steps.

- [ ] **Step 5: Run tests — verify they pass**

```bash
npx vitest run src/lib/actions/__tests__/trap-focus.test.ts
```

Expected: 3 PASS.

- [ ] **Step 6: Add Tab/Shift-Tab cycling tests + dynamic re-query test**

Append to the test file (inside the same `describe`):

```ts
  function pressKey(target: HTMLElement, key: string, opts: { shift?: boolean } = {}) {
    const event = new KeyboardEvent('keydown', {
      key,
      shiftKey: opts.shift ?? false,
      bubbles: true,
      cancelable: true,
    });
    target.dispatchEvent(event);
    return event;
  }

  it('cycles Tab from last focusable to first', () => {
    document.body.innerHTML = `
      <div id="modal">
        <button id="b1">B1</button>
        <button id="b2">B2</button>
      </div>
    `;
    const modal = document.querySelector<HTMLElement>('#modal')!;
    cleanup = trapFocus(modal, {});
    const last = document.querySelector<HTMLButtonElement>('#b2')!;
    last.focus();
    const event = pressKey(last, 'Tab');
    expect(event.defaultPrevented).toBe(true);
    expect(document.activeElement?.id).toBe('b1');
  });

  it('cycles Shift+Tab from first focusable to last', () => {
    document.body.innerHTML = `
      <div id="modal">
        <button id="b1">B1</button>
        <button id="b2">B2</button>
      </div>
    `;
    const modal = document.querySelector<HTMLElement>('#modal')!;
    cleanup = trapFocus(modal, {});
    const first = document.querySelector<HTMLButtonElement>('#b1')!;
    first.focus();
    const event = pressKey(first, 'Tab', { shift: true });
    expect(event.defaultPrevented).toBe(true);
    expect(document.activeElement?.id).toBe('b2');
  });

  it('does not preventDefault on Tab from middle of focusable list', () => {
    document.body.innerHTML = `
      <div id="modal">
        <button id="b1">B1</button>
        <button id="b2">B2</button>
        <button id="b3">B3</button>
      </div>
    `;
    const modal = document.querySelector<HTMLElement>('#modal')!;
    cleanup = trapFocus(modal, {});
    const middle = document.querySelector<HTMLButtonElement>('#b2')!;
    middle.focus();
    const event = pressKey(middle, 'Tab');
    expect(event.defaultPrevented).toBe(false);
  });

  it('re-queries focusables on each Tab so dynamically-disabled buttons are skipped', () => {
    document.body.innerHTML = `
      <div id="modal">
        <button id="b1">B1</button>
        <button id="b2">B2</button>
        <button id="b3">B3</button>
      </div>
    `;
    const modal = document.querySelector<HTMLElement>('#modal')!;
    cleanup = trapFocus(modal, {});
    document.querySelector<HTMLButtonElement>('#b3')!.disabled = true;
    const last = document.querySelector<HTMLButtonElement>('#b2')!;
    last.focus();
    pressKey(last, 'Tab');
    expect(document.activeElement?.id).toBe('b1');
  });
```

- [ ] **Step 7: Run tests — verify the four new tests fail**

```bash
npx vitest run src/lib/actions/__tests__/trap-focus.test.ts
```

Expected: 3 PASS, 4 FAIL — keydown handling is not implemented yet.

- [ ] **Step 8: Implement the keydown handler with Tab/Shift+Tab logic**

Replace the `trapFocus` function body in `src/lib/actions/trap-focus.ts` with:

```ts
export function trapFocus(node: HTMLElement, params: TrapFocusParams) {
  let current = params;

  const focusables = focusableIn(node);
  if (focusables.length > 0) {
    focusables[0].focus();
  } else {
    node.setAttribute('tabindex', '-1');
    node.focus();
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key !== 'Tab') return;
    const items = focusableIn(node);
    if (items.length === 0) return;
    const first = items[0];
    const last = items[items.length - 1];
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  }

  node.addEventListener('keydown', onKeydown);

  return {
    update(next: TrapFocusParams) {
      current = next;
    },
    destroy() {
      node.removeEventListener('keydown', onKeydown);
    },
  };
}
```

- [ ] **Step 9: Run tests — verify all 7 pass**

```bash
npx vitest run src/lib/actions/__tests__/trap-focus.test.ts
```

Expected: 7 PASS.

- [ ] **Step 10: Add Escape behavior tests**

Append:

```ts
  it('calls onCancel on Escape when canCancel is true', () => {
    document.body.innerHTML = `
      <div id="modal"><button id="b1">B1</button></div>
    `;
    const modal = document.querySelector<HTMLElement>('#modal')!;
    const onCancel = vi.fn();
    cleanup = trapFocus(modal, { onCancel, canCancel: true });
    pressKey(modal, 'Escape');
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it('treats omitted canCancel as true', () => {
    document.body.innerHTML = `
      <div id="modal"><button id="b1">B1</button></div>
    `;
    const modal = document.querySelector<HTMLElement>('#modal')!;
    const onCancel = vi.fn();
    cleanup = trapFocus(modal, { onCancel });
    pressKey(modal, 'Escape');
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it('does not call onCancel on Escape when canCancel is false', () => {
    document.body.innerHTML = `
      <div id="modal"><button id="b1">B1</button></div>
    `;
    const modal = document.querySelector<HTMLElement>('#modal')!;
    const onCancel = vi.fn();
    cleanup = trapFocus(modal, { onCancel, canCancel: false });
    pressKey(modal, 'Escape');
    expect(onCancel).not.toHaveBeenCalled();
  });
```

- [ ] **Step 11: Run tests — verify the three Escape tests fail**

```bash
npx vitest run src/lib/actions/__tests__/trap-focus.test.ts
```

Expected: 7 PASS, 3 FAIL.

- [ ] **Step 12: Implement Escape handling**

In `src/lib/actions/trap-focus.ts`, modify the `onKeydown` function to handle Escape BEFORE the Tab early-return:

```ts
  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      if (current.canCancel !== false && current.onCancel) {
        current.onCancel();
      }
      return;
    }
    if (e.key !== 'Tab') return;
    const items = focusableIn(node);
    if (items.length === 0) return;
    const first = items[0];
    const last = items[items.length - 1];
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  }
```

`canCancel !== false` means `undefined` (default) and `true` both allow cancel; only explicit `false` blocks.

- [ ] **Step 13: Run tests — verify all 10 pass**

```bash
npx vitest run src/lib/actions/__tests__/trap-focus.test.ts
```

Expected: 10 PASS.

- [ ] **Step 14: Add focus-restore tests**

Append:

```ts
  it('restores focus to previouslyFocused on destroy', () => {
    document.body.innerHTML = `
      <button id="trigger">Open</button>
      <div id="modal"><button id="b1">B1</button></div>
    `;
    const trigger = document.querySelector<HTMLButtonElement>('#trigger')!;
    trigger.focus();
    expect(document.activeElement?.id).toBe('trigger');
    const modal = document.querySelector<HTMLElement>('#modal')!;
    const handle = trapFocus(modal, {});
    expect(document.activeElement?.id).toBe('b1');
    handle.destroy();
    expect(document.activeElement?.id).toBe('trigger');
  });

  it('does not throw when previouslyFocused was removed before destroy', () => {
    document.body.innerHTML = `
      <button id="trigger">Open</button>
      <div id="modal"><button id="b1">B1</button></div>
    `;
    const trigger = document.querySelector<HTMLButtonElement>('#trigger')!;
    trigger.focus();
    const modal = document.querySelector<HTMLElement>('#modal')!;
    const handle = trapFocus(modal, {});
    trigger.remove();
    expect(() => handle.destroy()).not.toThrow();
  });
```

- [ ] **Step 15: Run tests — verify the two restore tests fail**

```bash
npx vitest run src/lib/actions/__tests__/trap-focus.test.ts
```

Expected: 10 PASS, 2 FAIL — destroy() doesn't restore focus yet.

- [ ] **Step 16: Implement focus restore in destroy**

In `src/lib/actions/trap-focus.ts`, capture `previouslyFocused` at mount and restore on destroy. Replace the function body:

```ts
export function trapFocus(node: HTMLElement, params: TrapFocusParams) {
  let current = params;
  const previouslyFocused =
    document.activeElement instanceof HTMLElement ? document.activeElement : null;

  const focusables = focusableIn(node);
  if (focusables.length > 0) {
    focusables[0].focus();
  } else {
    node.setAttribute('tabindex', '-1');
    node.focus();
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      if (current.canCancel !== false && current.onCancel) {
        current.onCancel();
      }
      return;
    }
    if (e.key !== 'Tab') return;
    const items = focusableIn(node);
    if (items.length === 0) return;
    const first = items[0];
    const last = items[items.length - 1];
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  }

  node.addEventListener('keydown', onKeydown);

  return {
    update(next: TrapFocusParams) {
      current = next;
    },
    destroy() {
      node.removeEventListener('keydown', onKeydown);
      try {
        previouslyFocused?.focus({ preventScroll: true });
      } catch {
        // Trigger removed; let focus fall back to body.
      }
    },
  };
}
```

- [ ] **Step 17: Run tests — verify all 12 pass**

```bash
npx vitest run src/lib/actions/__tests__/trap-focus.test.ts
```

Expected: 12 PASS.

- [ ] **Step 18: Add update() test**

Append:

```ts
  it('honors canCancel changes via update()', () => {
    document.body.innerHTML = `
      <div id="modal"><button id="b1">B1</button></div>
    `;
    const modal = document.querySelector<HTMLElement>('#modal')!;
    const onCancel = vi.fn();
    const handle = trapFocus(modal, { onCancel, canCancel: true });
    cleanup = handle;
    pressKey(modal, 'Escape');
    expect(onCancel).toHaveBeenCalledTimes(1);
    handle.update({ onCancel, canCancel: false });
    pressKey(modal, 'Escape');
    expect(onCancel).toHaveBeenCalledTimes(1); // still 1 — second press blocked
  });
```

- [ ] **Step 19: Run test — should already pass**

```bash
npx vitest run src/lib/actions/__tests__/trap-focus.test.ts
```

Expected: 13 PASS. The implementation already calls `current.canCancel` (via the closure-mutated `current`), so `update()` works without further changes. If this test fails, the implementation's `current = next` assignment is missing.

- [ ] **Step 20: Run typecheck and full vitest, then commit**

```bash
npx tsc --noEmit
npx vitest run
```

Expected: tsc clean, all existing 1303 + new 13 tests pass.

```bash
git add src/lib/actions/trap-focus.ts src/lib/actions/__tests__/trap-focus.test.ts
git commit -m "feat(zeb-195): use:trapFocus action — focus mount, Tab cycling, Esc, restore"
```

---

## Task 2: Build `<Modal>` component with tests

**Files:**
- Create: `src/lib/components/Modal.svelte`
- Create: `src/lib/components/__tests__/Modal.test.ts`

The component is a thin wrapper over the action. Its job is to own the shared overlay/dialog markup and CSS, plumb props through to the action, and accept children.

- [ ] **Step 1: Write the failing tests**

Create `src/lib/components/__tests__/Modal.test.ts`:

```ts
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import { tick } from 'svelte';
import Modal from '../Modal.svelte';
import ModalHarness from './ModalHarness.svelte';

describe('Modal component', () => {
  it('renders with role=dialog and aria-modal=true', () => {
    const { getByRole } = render(Modal, {
      props: {
        onCancel: () => {},
        ariaLabelledby: 'heading',
      },
    });
    const dialog = getByRole('dialog');
    expect(dialog.getAttribute('aria-modal')).toBe('true');
    expect(dialog.getAttribute('aria-labelledby')).toBe('heading');
  });

  it('calls onCancel when Escape is pressed and canCancel is true', async () => {
    const onCancel = vi.fn();
    const { getByRole } = render(Modal, {
      props: { onCancel, canCancel: true, ariaLabelledby: 'h' },
    });
    const dialog = getByRole('dialog');
    await fireEvent.keyDown(dialog, { key: 'Escape' });
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it('does not call onCancel when canCancel is false', async () => {
    const onCancel = vi.fn();
    const { getByRole } = render(Modal, {
      props: { onCancel, canCancel: false, ariaLabelledby: 'h' },
    });
    const dialog = getByRole('dialog');
    await fireEvent.keyDown(dialog, { key: 'Escape' });
    expect(onCancel).not.toHaveBeenCalled();
  });

  it('renders provided children inside the dialog', () => {
    const { getByRole, getByText } = render(ModalHarness, {
      props: { ariaLabelledby: 'h', onCancel: () => {} },
    });
    const dialog = getByRole('dialog');
    expect(dialog.contains(getByText('child content'))).toBe(true);
  });
});
```

We need a small harness because `@testing-library/svelte` doesn't directly accept Snippet props. Create `src/lib/components/__tests__/ModalHarness.svelte`:

```svelte
<script lang="ts">
  import Modal from '../Modal.svelte';

  let { ariaLabelledby, onCancel }: { ariaLabelledby: string; onCancel: () => void } = $props();
</script>

<Modal {onCancel} {ariaLabelledby}>
  <h2 id={ariaLabelledby}>Heading</h2>
  <p>child content</p>
</Modal>
```

- [ ] **Step 2: Run tests — verify they fail**

```bash
npx vitest run src/lib/components/__tests__/Modal.test.ts
```

Expected: FAIL — Modal.svelte doesn't exist yet.

- [ ] **Step 3: Implement `Modal.svelte`**

Create `src/lib/components/Modal.svelte`:

```svelte
<script lang="ts">
  import type { Snippet } from 'svelte';
  import { trapFocus } from '../actions/trap-focus';

  let {
    onCancel,
    canCancel = true,
    ariaLabelledby,
    children,
  }: {
    onCancel: () => void;
    canCancel?: boolean;
    ariaLabelledby: string;
    children?: Snippet;
  } = $props();
</script>

<div class="modal-overlay">
  <div
    class="modal"
    role="dialog"
    aria-modal="true"
    aria-labelledby={ariaLabelledby}
    use:trapFocus={{ onCancel, canCancel }}
  >
    {@render children?.()}
  </div>
</div>

<style>
  .modal-overlay {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .modal {
    background: var(--bg-secondary);
    padding: 24px;
    border-radius: 8px;
    max-width: 480px;
    border: 1px solid var(--border);
  }
</style>
```

These styles are byte-identical to the rules in `DevicesPanel.svelte` lines 537–552 (the existing `.modal-overlay` and `.modal` rules), so visual output is preserved.

- [ ] **Step 4: Run tests — verify they pass**

```bash
npx vitest run src/lib/components/__tests__/Modal.test.ts
```

Expected: 4 PASS.

- [ ] **Step 5: Run full vitest and tsc, then commit**

```bash
npx tsc --noEmit
npx vitest run
```

Expected: clean.

```bash
git add src/lib/components/Modal.svelte \
        src/lib/components/__tests__/Modal.test.ts \
        src/lib/components/__tests__/ModalHarness.svelte
git commit -m "feat(zeb-195): <Modal> component wrapping use:trapFocus"
```

---

## Task 3: Migrate DevicesPanel mint-confirm modal

**Files:**
- Modify: `src/lib/components/DevicesPanel.svelte` (lines 474–495)
- Modify: `src/lib/components/__tests__/DevicesPanel.test.ts`

The mint modal is the simpler of the two (no form fields, just a confirm/cancel pair). Migrate it first.

- [ ] **Step 1: Write the failing integration tests**

Append to `src/lib/components/__tests__/DevicesPanel.test.ts` (inside the existing top-level `describe`):

```ts
  describe('mint modal a11y (ZEB-195)', () => {
    it('moves focus into the modal when opened, restores it on close', async () => {
      // Setup mirrors the existing "opens confirm modal when bind CTA is
      // clicked" test (line 39 in this file): mock OwnerService.refresh +
      // get_owner_state returning null (empty state), render DevicesPanel,
      // findByRole('button', { name: /Create owner identity/i }), click it.
      // Then:
      const trigger = screen.getByRole('button', { name: /Create owner identity/i });
      // (already focused by user click)
      expect(document.activeElement).toBe(trigger);
      await fireEvent.click(trigger);
      const dialog = await screen.findByRole('dialog');
      expect(dialog.contains(document.activeElement)).toBe(true);
      // Press Escape on the dialog itself.
      await fireEvent.keyDown(dialog, { key: 'Escape' });
      expect(screen.queryByRole('dialog')).toBeNull();
      expect(document.activeElement).toBe(trigger);
    });

    it('Escape closes the mint modal', async () => {
      // Same setup; open modal; press Escape on the dialog; assert it unmounts.
      // (This is the second half of the prior test — keep separate so a focus
      // assertion failure doesn't mask the Escape-closes-modal regression.)
      // Implementation: copy setup from prior test, omit the focus-restore assertion.
    });

    it('Escape is no-op while mintInFlight', async () => {
      // Setup as above. Open modal. Click "Create owner identity" inside the
      // modal to start mint flow (do NOT await — leaves mintInFlight = true).
      // Press Escape on the dialog. Assert dialog still in DOM.
      // (Then resolve the pending promise and tick to clean up.)
    });
  });
```

The setup pattern (OwnerService mock + render + click trigger) is established by the existing tests at lines 39 and 75 in this file. Copy that scaffold — only the assertions differ.

- [ ] **Step 2: Run tests — verify the three new tests fail**

```bash
npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts
```

Expected: 26 existing PASS, 3 new FAIL.

- [ ] **Step 3: Migrate the mint-confirm modal markup**

In `src/lib/components/DevicesPanel.svelte`, replace lines 474–495 (the `{#if modalOpen}` block):

```svelte
{#if modalOpen}
  <div class="modal-overlay" role="dialog" aria-modal="true" aria-labelledby="modal-heading">
    <div class="modal">
      <h3 id="modal-heading">Create your owner identity</h3>
      <p>
        This will create your owner identity. This device will be bound as the first device.
        You'll receive a recovery file to back up — you can do this immediately or later.
      </p>
      {#if mintError}
        <p class="error" role="alert">{mintError}</p>
      {/if}
      <div class="modal-actions">
        <button class="secondary" onclick={() => { modalOpen = false; }} disabled={mintInFlight}>
          Cancel
        </button>
        <button class="primary" onclick={handleConfirmMint} disabled={mintInFlight}>
          {mintInFlight ? 'Creating…' : 'Create owner identity'}
        </button>
      </div>
    </div>
  </div>
{/if}
```

with:

```svelte
{#if modalOpen}
  <Modal
    onCancel={() => { modalOpen = false; }}
    canCancel={!mintInFlight}
    ariaLabelledby="modal-heading"
  >
    <h3 id="modal-heading">Create your owner identity</h3>
    <p>
      This will create your owner identity. This device will be bound as the first device.
      You'll receive a recovery file to back up — you can do this immediately or later.
    </p>
    {#if mintError}
      <p class="error" role="alert">{mintError}</p>
    {/if}
    <div class="modal-actions">
      <button class="secondary" onclick={() => { modalOpen = false; }} disabled={mintInFlight}>
        Cancel
      </button>
      <button class="primary" onclick={handleConfirmMint} disabled={mintInFlight}>
        {mintInFlight ? 'Creating…' : 'Create owner identity'}
      </button>
    </div>
  </Modal>
{/if}
```

Add the import at the top of the script block:

```ts
  import Modal from './Modal.svelte';
```

`canCancel={!mintInFlight}` mirrors the Cancel button's `disabled` attribute exactly.

- [ ] **Step 4: Run tests — verify all 29 pass**

```bash
npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts
```

Expected: 29 PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/DevicesPanel.svelte src/lib/components/__tests__/DevicesPanel.test.ts
git commit -m "feat(zeb-195): migrate DevicesPanel mint-confirm modal to <Modal>"
```

---

## Task 4: Migrate DevicesPanel backup modal + delete duplicated CSS

**Files:**
- Modify: `src/lib/components/DevicesPanel.svelte` (lines 407–459 + delete CSS at lines 537–552)
- Modify: `src/lib/components/__tests__/DevicesPanel.test.ts`

The backup modal is more complex — it has form fields, multiple in-flight states (`backupDialogInFlight`, `backupInFlight`), and a success-state branch. The migration is mechanical but needs care on the `canCancel` predicate.

- [ ] **Step 1: Write the failing integration tests**

Append to `src/lib/components/__tests__/DevicesPanel.test.ts`:

```ts
  describe('backup modal a11y (ZEB-195)', () => {
    it('moves focus into the modal when opened, restores it on close', async () => {
      // Setup mirrors "clicking Back up opens the backup modal and issues a
      // token if needed" (line 194): mock OwnerService.refresh returning
      // populated state with canBackUp=true, mock issue_owner_recovery_token,
      // render, find trigger button, click.
      const trigger = screen.getByRole('button', { name: /Back up owner identity/i });
      expect(document.activeElement).toBe(trigger);
      await fireEvent.click(trigger);
      const dialog = await screen.findByRole('dialog');
      expect(dialog.contains(document.activeElement)).toBe(true);
      await fireEvent.keyDown(dialog, { key: 'Escape' });
      expect(screen.queryByRole('dialog')).toBeNull();
      expect(document.activeElement).toBe(trigger);
    });

    it('Escape closes the backup modal', async () => {
      // Same setup; open modal; press Escape; assert closed.
    });

    it('Escape is no-op while backupDialogInFlight or backupInFlight', async () => {
      // Setup as above. Open modal. Fill in matching passphrase fields with
      // ≥12 codepoints. Click "Save backup" — backupDialogInFlight becomes
      // true while the save dialog promise is pending. Press Escape; assert
      // dialog still in DOM. Resolve the pending dialog promise and tick.
    });
  });
```

The setup pattern is established by the existing tests at lines 194 and 210. Copy that scaffold.

- [ ] **Step 2: Run tests — verify the three new tests fail**

```bash
npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts
```

Expected: 29 existing PASS, 3 new FAIL.

- [ ] **Step 3: Migrate the backup modal markup**

In `src/lib/components/DevicesPanel.svelte`, replace lines 407–459 (the `{#if backupOpen}` block — the outer `<div class="modal-overlay">...<div class="modal">...</div></div>` wrapper).

The exact replacement pattern: take the entire existing `{#if backupOpen}` block, remove the outer two `<div>` lines and their closing tags, wrap with `<Modal ...>` and `</Modal>`. Use the same `canCancel` derivation as the existing Cancel button's `disabled` attribute:

```svelte
{#if backupOpen}
  <Modal
    onCancel={closeBackup}
    canCancel={!backupDialogInFlight && !backupInFlight}
    ariaLabelledby="backup-modal-heading"
  >
    <h3 id="backup-modal-heading">Back up owner identity</h3>
    <!-- existing inner content unchanged: success branch, form, error, actions -->
  </Modal>
{/if}
```

Note: the existing Cancel button at line 436 uses `disabled={backupDialogInFlight || backupInFlight}` — so `canCancel` is the negation of that predicate.

- [ ] **Step 4: Run tests — verify all 32 pass**

```bash
npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts
```

Expected: 32 PASS.

- [ ] **Step 5: Delete the now-unused `.modal-overlay` and `.modal` CSS rules**

Both DevicesPanel modals now route through `<Modal>`, which owns the styles. In `src/lib/components/DevicesPanel.svelte`, delete the two style blocks at lines 537–552 (the `.modal-overlay` and `.modal` rules — exact line numbers will have shifted after the markup migration; identify by selector, not line). Keep `.modal-actions`, `.error`, etc. — those are used elsewhere in the file.

- [ ] **Step 6: Run tests + tsc — verify nothing broke**

```bash
npx tsc --noEmit
npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts
```

Expected: clean and 32 PASS. Visual styles still apply (they live in `Modal.svelte` now).

- [ ] **Step 7: Commit**

```bash
git add src/lib/components/DevicesPanel.svelte src/lib/components/__tests__/DevicesPanel.test.ts
git commit -m "feat(zeb-195): migrate DevicesPanel backup modal to <Modal>; consolidate CSS"
```

---

## Task 5: Migrate PairingInviter

**Files:**
- Modify: `src/lib/components/PairingInviter.svelte` (lines 38–[end-of-modal])
- Modify: `src/lib/components/__tests__/PairingInviter.test.ts`

PairingInviter has multiple state branches inside a single modal — but the modal wrapper is the same single overlay/dialog pair. The migration only swaps the outer wrapper.

- [ ] **Step 1: Add a smoke test**

Append to `src/lib/components/__tests__/PairingInviter.test.ts`:

```ts
  it('renders inside <Modal> with role=dialog after migration', () => {
    // Use existing test setup
    const { getByRole } = render(PairingInviter, {
      props: { hostname: 'test', onClose: () => {} },
    });
    const dialog = getByRole('dialog');
    expect(dialog.getAttribute('aria-modal')).toBe('true');
    expect(dialog.getAttribute('aria-labelledby')).toBe('invite-heading');
  });
```

This passes against the current code (existing markup already has these attributes), so it's a regression net for the migration. Run it now to confirm it passes pre-migration:

```bash
npx vitest run src/lib/components/__tests__/PairingInviter.test.ts
```

Expected: PASS (test passes against existing markup; this is a regression-prevention test, not a TDD red→green test).

- [ ] **Step 2: Migrate the modal wrapper**

In `src/lib/components/PairingInviter.svelte`, replace the outer modal wrapper at lines 38–39:

```svelte
<div class="modal-overlay" role="dialog" aria-modal="true" aria-labelledby="invite-heading">
  <div class="modal">
```

with:

```svelte
<Modal
  onCancel={handleCancel}
  canCancel={state.kind !== 'complete' && state.kind !== 'failed'}
  ariaLabelledby="invite-heading"
>
```

And the matching closing `</div></div>` at the end of the modal (just before `<style>`) becomes `</Modal>`.

Add the import at the top of the script block:

```ts
  import Modal from './Modal.svelte';
```

**Predicate rationale:** the existing markup shows a Cancel button in every `state.kind` branch *except* `complete` and `failed`, where only a `Close → onClose` button is rendered (lines 84–92 of `PairingInviter.svelte`). `canCancel` mirrors Cancel-button presence — in terminal states, Esc no-ops, and the user closes via the focused Close button instead.

- [ ] **Step 3: Delete the now-unused `.modal-overlay` and `.modal` CSS rules from PairingInviter.svelte**

Find the `.modal-overlay` and `.modal` rules in the `<style>` block of `PairingInviter.svelte` and delete them. Keep `.modal-actions`, `.error`, etc.

- [ ] **Step 4: Run tests + tsc**

```bash
npx tsc --noEmit
npx vitest run src/lib/components/__tests__/PairingInviter.test.ts
```

Expected: clean, all PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/PairingInviter.svelte src/lib/components/__tests__/PairingInviter.test.ts
git commit -m "feat(zeb-195): migrate PairingInviter to <Modal>"
```

---

## Task 6: Migrate PairingJoiner + final gates

**Files:**
- Modify: `src/lib/components/PairingJoiner.svelte` (lines 51 + matching closing tags)
- Modify: `src/lib/components/__tests__/PairingJoiner.test.ts`

Mirror of Task 5. Same shape, different file.

- [ ] **Step 1: Add the smoke test**

Append to `src/lib/components/__tests__/PairingJoiner.test.ts`:

```ts
  it('renders inside <Modal> with role=dialog after migration', () => {
    const { getByRole } = render(PairingJoiner, {
      props: { onClose: () => {} },
    });
    const dialog = getByRole('dialog');
    expect(dialog.getAttribute('aria-modal')).toBe('true');
    expect(dialog.getAttribute('aria-labelledby')).toBe('join-heading');
  });
```

Run it pre-migration:

```bash
npx vitest run src/lib/components/__tests__/PairingJoiner.test.ts
```

Expected: PASS against existing markup.

- [ ] **Step 2: Migrate the modal wrapper**

In `src/lib/components/PairingJoiner.svelte`, replace lines 51–52:

```svelte
<div class="modal-overlay" role="dialog" aria-modal="true" aria-labelledby="join-heading">
  <div class="modal">
```

with:

```svelte
<Modal
  onCancel={handleCancel}
  canCancel={state.kind !== 'complete' && state.kind !== 'failed'}
  ariaLabelledby="join-heading"
>
```

The matching closing `</div></div>` at lines 123–124 becomes `</Modal>`.

Add the import:

```ts
  import Modal from './Modal.svelte';
```

**Predicate rationale:** identical to PairingInviter — Cancel button is present in every state branch except `complete` (line 112–116) and `failed` (line 117–121), which only show a Close button.

- [ ] **Step 3: Delete the now-unused `.modal-overlay` and `.modal` CSS rules from PairingJoiner.svelte**

- [ ] **Step 4: Run all gates — full vitest, tsc, Rust gates**

```bash
npx tsc --noEmit
npx vitest run
cargo test --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings
cargo fmt --all -- --check
```

All must be clean. The Rust gates have no Rust changes to validate, but CI runs them — defensive verification per memory rule.

- [ ] **Step 5: Manual smoke test**

Launch the GUI:

```bash
npx tauri dev
```

Verify all four migrated modals keyboard-only:

1. Open DevicesPanel mint modal (empty state → "Create owner identity"). Tab through → wraps. Esc closes → focus returns to trigger.
2. Open DevicesPanel backup modal ("Back up owner identity"). Tab through → wraps. Esc closes → focus returns to trigger.
3. Open PairingInviter ("Add another device"). Esc cancels → focus returns to trigger.
4. Open PairingJoiner ("Join existing identity"). Esc cancels → focus returns to trigger.

Also visually verify all four still render correctly (overlay shade, dialog box, actions row).

- [ ] **Step 6: Commit and finish**

```bash
git add src/lib/components/PairingJoiner.svelte src/lib/components/__tests__/PairingJoiner.test.ts
git commit -m "feat(zeb-195): migrate PairingJoiner to <Modal>; finish 4-modal sweep"
```

Use `superpowers:finishing-a-development-branch` to complete (push + PR).
