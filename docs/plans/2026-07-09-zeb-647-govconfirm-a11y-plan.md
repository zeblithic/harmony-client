# ZEB-647: GovConfirmModal a11y Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the shared governance confirm modal real modal behavior — Escape-to-cancel, initial focus, focus trap, focus restore — and fix the role/severity inversion with `role="alertdialog"`.

**Architecture:** Reuse the existing `trapFocus` action (`src/lib/actions/trap-focus.ts`, already used by `Modal.svelte` and pinned by 17 tests). Move dialog semantics from the overlay onto the card, wire `aria-labelledby`/`aria-describedby` via Svelte 5's `$props.id()`. Zero consumer changes.

**Tech Stack:** Svelte 5 (runes), @testing-library/svelte, vitest.

**Spec:** `docs/specs/2026-07-09-zeb-647-govconfirm-a11y-design.md` (approved 2026-07-09).

## Global Constraints

- Frontend-only: no Rust surface touched; gates are `npx tsc --noEmit` + `npx vitest run` from repo root.
- The four consumers (Tier3ProposalPanel, StatementComposer, StarRatificationBallot, DelegationWidget) must not change.
- The four existing GovConfirmModal tests must keep passing unmodified.
- `.confirm-body` wrapper must mirror the card's `display:flex; flex-direction:column; gap:0.75rem` so consumer spacing is unchanged (spec §3.1).
- Commit trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`.

---

### Task 1: Failing tests — focus management + alertdialog wiring

**Files:**
- Modify: `src/lib/components/governance/__tests__/governance-primitives.test.ts` (extend the `GovConfirmModal` describe, after the `busy disables both buttons` test)

**Interfaces:**
- Consumes: `GovConfirmModal` props surface (unchanged), `createRawSnippet` (precedent: `src/lib/components/Layout.test.ts:3-8`), the Escape-on-dialog-node dispatch pattern (`ReshareConfirmDialog.test.ts:125-143`).
- Produces: 7 tests that pin the Task 2 behavior.

- [ ] **Step 1: Add the failing tests**

Add `createRawSnippet` to the imports at the top of the file:

```typescript
import { createRawSnippet } from 'svelte';
```

Append inside `describe('GovConfirmModal', ...)`:

```typescript
  // ZEB-647 — focus management + alertdialog semantics (via trapFocus).
  it('typed severity: autofocuses the typed input on open', () => {
    render(GovConfirmModal, {
      props: {
        title: 'T',
        severity: 'typed',
        typedMatch: 'revoke',
        onConfirm: vi.fn(),
        onCancel: vi.fn(),
      },
    });
    expect(document.activeElement).toBe(
      screen.getByLabelText('Type the word revoke to confirm'),
    );
  });
  it('click severity: autofocuses the Cancel button on open', () => {
    render(GovConfirmModal, { props: { title: 'T', onConfirm: vi.fn(), onCancel: vi.fn() } });
    expect(document.activeElement).toBe(screen.getByRole('button', { name: 'Cancel' }));
  });
  it('Escape fires onCancel', async () => {
    const onCancel = vi.fn();
    render(GovConfirmModal, { props: { title: 'T', onConfirm: vi.fn(), onCancel } });
    // trap-focus binds keydown on the dialog node (not window) — dispatch on
    // the alertdialog element, same pattern as ReshareConfirmDialog.test.ts.
    await fireEvent.keyDown(screen.getByRole('alertdialog'), { key: 'Escape' });
    expect(onCancel).toHaveBeenCalledTimes(1);
  });
  it('Escape during busy does not cancel', async () => {
    const onCancel = vi.fn();
    render(GovConfirmModal, {
      props: { title: 'T', busy: true, onConfirm: vi.fn(), onCancel },
    });
    await fireEvent.keyDown(screen.getByRole('alertdialog'), { key: 'Escape' });
    expect(onCancel).not.toHaveBeenCalled();
  });
  it('exposes alertdialog with title + body wiring', () => {
    const children = createRawSnippet(() => ({
      render: () => '<p>Irreversible warning</p>',
    }));
    render(GovConfirmModal, {
      props: { title: 'Confirm thing', onConfirm: vi.fn(), onCancel: vi.fn(), children },
    });
    const dialog = screen.getByRole('alertdialog');
    expect(dialog.getAttribute('aria-modal')).toBe('true');
    const titleEl = document.getElementById(dialog.getAttribute('aria-labelledby')!);
    expect(titleEl?.textContent).toBe('Confirm thing');
    const bodyEl = document.getElementById(dialog.getAttribute('aria-describedby')!);
    expect(bodyEl?.textContent).toContain('Irreversible warning');
  });
  it('omits aria-describedby without children', () => {
    render(GovConfirmModal, { props: { title: 'T', onConfirm: vi.fn(), onCancel: vi.fn() } });
    expect(screen.getByRole('alertdialog').hasAttribute('aria-describedby')).toBe(false);
  });
  it('restores focus to the opener on unmount', () => {
    const trigger = document.createElement('button');
    trigger.textContent = 'Open';
    document.body.appendChild(trigger);
    trigger.focus();
    const { unmount } = render(GovConfirmModal, {
      props: { title: 'T', onConfirm: vi.fn(), onCancel: vi.fn() },
    });
    expect(document.activeElement).not.toBe(trigger);
    unmount();
    expect(document.activeElement).toBe(trigger);
    trigger.remove();
  });
```

- [ ] **Step 2: Run to verify all 7 fail**

Run: `npx vitest run src/lib/components/governance/__tests__/governance-primitives.test.ts`
Expected: the 4 pre-existing GovConfirmModal tests pass; the 7 new ones FAIL (`getByRole('alertdialog')` finds nothing; `document.activeElement` is `<body>`).

- [ ] **Step 3: Commit**

```bash
git add src/lib/components/governance/__tests__/governance-primitives.test.ts
git commit -m "test: ZEB-647 failing tests for GovConfirmModal focus + alertdialog"
```

---

### Task 2: Wire trapFocus + alertdialog into GovConfirmModal

**Files:**
- Modify: `src/lib/components/governance/GovConfirmModal.svelte`

**Interfaces:**
- Consumes: `trapFocus` from `../../actions/trap-focus` (params `{ onCancel, canCancel }`; `update()` handles reactive `canCancel` changes — pinned by `trap-focus.test.ts` "honors canCancel changes via update()").
- Produces: unchanged props surface; DOM contract `role="alertdialog"` on `.confirm-card`.

- [ ] **Step 1: Script changes**

Add the import (after the `Snippet` import) and the id constants (after the `$props()` destructure):

```typescript
  import { trapFocus } from '../../actions/trap-focus';
```

```typescript
  const uid = $props.id();
  const titleId = `${uid}-title`;
  const bodyId = `${uid}-body`;
```

- [ ] **Step 2: Markup changes**

Replace the container/card open tags and the title/children block:

```svelte
<div class="confirm-modal">
  <div
    class="confirm-card"
    role="alertdialog"
    aria-modal="true"
    aria-labelledby={titleId}
    aria-describedby={children ? bodyId : undefined}
    use:trapFocus={{ onCancel, canCancel: !busy }}
  >
    <p class="confirm-title" id={titleId}>{title}</p>
    {#if children}
      <div class="confirm-body" id={bodyId}>
        {@render children()}
      </div>
    {/if}
```

(The typed input, actions row, and closing tags are unchanged.)

- [ ] **Step 3: CSS — preserve consumer spacing**

Add after the `.confirm-title` rule:

```css
  .confirm-body {
    /* Mirrors .confirm-card's layout: consumers pass sibling elements
       (preview + caveat) that were direct flex children of the card before
       this wrapper existed — keep their 0.75rem rhythm. */
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
  }
```

- [ ] **Step 4: Run the test file — all pass**

Run: `npx vitest run src/lib/components/governance/__tests__/governance-primitives.test.ts`
Expected: PASS (11 GovConfirmModal tests + the other primitives).

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/governance/GovConfirmModal.svelte
git commit -m "feat: ZEB-647 GovConfirmModal focus management + alertdialog role"
```

---

### Task 3: Full gates + PR + converge

- [ ] **Step 1: Full frontend gates**

Run from repo root: `npx tsc --noEmit && npx vitest run`
Expected: clean. (Frontend-only change — the CI rust jobs are unaffected; no local cargo gate needed.)

- [ ] **Step 2: Push and open PR**

```bash
git push -u origin zeb-647-govconfirmmodal-a11y
gh pr create --repo zeblithic/harmony-client --title "ZEB-647: GovConfirmModal a11y — trap-focus wiring + alertdialog role" --body "..."
```

PR body: spec/plan links, the trapFocus-reuse story, role rationale, accepted limitation §3.5, magic words `Closes ZEB-647`.

- [ ] **Step 3: Fire `@coderabbitai review` once, converge bots + CI**

One pass at PR-open; scan all three comment buckets each round; one commit + one push per converge round.
