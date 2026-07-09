# ZEB-647: GovConfirmModal a11y — focus management + role severity

**Status:** DRAFT — awaiting review
**Ticket:** [ZEB-647](https://linear.app/zeblith/issue/ZEB-647/govconfirmmodal-a11y-focus-management-role-severity)
**Origin:** ZEB-607 final whole-branch review (Commons D governance restyle)

## 1. Goal

`GovConfirmModal.svelte` is the single shared confirm modal for four governance
consumers (Tier3ProposalPanel, StatementComposer, StarRatificationBallot,
DelegationWidget typed-revoke). It declares `aria-modal="true"` but delivers
none of the behavior that attribute promises: no Escape-to-cancel, no initial
focus, no focus trap, no focus restore on close, and a role/severity inversion
(the *low*-severity inline click bar in DelegationWidget is `alertdialog` while
the *high*-severity typed modal is plain `dialog`). Fix all of it once, here.

## 2. Key discovery: `trapFocus` already exists

`src/lib/actions/trap-focus.ts` (used by the generic `Modal.svelte`) already
implements everything the ticket's "suggested shape" asks for, with 17 tests
pinning its behavior:

* **Initial focus** — the typed input under `severity="typed"`, the Cancel
  button under `severity="click"` (Cancel-first is the safe default for
  destructive confirms). *Converge amendment (Qodo, PR #433 round 1):*
  originally this relied on the intended control being first in DOM order,
  but the `children` snippet renders before the controls, so a focusable
  element in a future consumer's body copy would steal it. `trapFocus` now
  takes an explicit `initialFocus?: () => HTMLElement | null` param (falls
  back to first-focusable when the target isn't currently focusable, e.g.
  disabled while `busy`), and GovConfirmModal passes the severity-appropriate
  ref.
* **Escape → `onCancel()`** — gated by `canCancel`; we pass `canCancel: !busy`,
  which is precisely the ticket's "unless `busy`" guard.
* **Tab/Shift+Tab trap** — cycles within the node, re-querying focusables per
  keypress (dynamically-disabled buttons are skipped).
* **Focus restore** — `destroy()` returns focus to the previously-focused
  element (the button that opened the modal), fixing "focus falls to `<body>`
  when the modal unmounts."

So the implementation is *wiring*, not new machinery — and it makes
GovConfirmModal behave identically to every `Modal.svelte`-based dialog
(ReshareConfirmDialog et al.), one consistent keyboard contract app-wide.

## 3. Design

### 3.1 Markup restructure (match `Modal.svelte`'s pattern)

Move the dialog semantics from the outer overlay onto the inner card, and
attach the action there — the same overlay/dialog split `Modal.svelte` uses:

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
      <div class="confirm-body" id={bodyId}>{@render children()}</div>
    {/if}
    …existing typed input + actions unchanged…
  </div>
</div>
```

No backdrop-click dismissal (parity with `Modal.svelte`'s default and with
current GovConfirmModal behavior — Escape and the Cancel button are the
dismissal surfaces).

The `.confirm-body` wrapper demotes what were direct flex children of the
card (StatementComposer and StarRatificationBallot both pass two siblings —
preview + caveat, list + caveat) into one flex item, which would collapse the
0.75rem gaps between them. The wrapper therefore mirrors the card's layout so
the visual rhythm is unchanged:

```css
.confirm-body {
  display: flex;
  flex-direction: column;
  gap: 0.75rem;
}
```

### 3.2 Role: `alertdialog`, unconditionally

Every GovConfirmModal use is an interrupting confirmation of a consequential
governance action — the exact WAI-ARIA definition of `alertdialog` (and the
reason `alertdialog` announces its `aria-describedby` content on focus). Both
severity tiers get it; gating on `severity === 'typed'` would re-introduce a
role split with no user-facing meaning, since the click tier is *also* an
interrupting confirm. This resolves the ZEB-607 inversion: DelegationWidget's
inline click bar (`alertdialog`) and the shared modal now agree.

### 3.3 ARIA naming: `aria-labelledby` + `aria-describedby`

`alertdialog` wants a label *and* a described-by message — the message is
where the actual warning copy lives ("Your delegate is carrying significant
weight… Type **revoke** to confirm"), and today a screen reader announces
only the title. Generate ids with Svelte 5's `$props.id()` (first use in the
codebase; available since 5.20, we're on ^5.53):

```svelte
const uid = $props.id();
const titleId = `${uid}-title`;
const bodyId = `${uid}-body`;
```

`aria-label={title}` is replaced by `aria-labelledby={titleId}` (same
announced text, now also visible-text-linked). `aria-describedby` is set only
when `children` exist.

### 3.4 What does NOT change

* **The four consumers** — zero call-site changes; the props surface is
  untouched.
* **DelegationWidget's inline click bar** (`.dw-confirm-bar`) — stays as-is.
  Its `role="alertdialog"` on a non-modal in-flow bar is itself imperfect
  ARIA, but it's a different pattern (in-flow, non-blocking) and out of this
  ticket's scope; noted in §6.
* **Confirm-enablement logic** (`confirmEnabled`, empty-`typedMatch` guard,
  `busy` disabling) — untouched.

### 3.5 Accepted limitation (Modal.svelte parity)

While `busy` is true, *all* controls in the card are disabled; if the user
confirmed via click, focus drops to `<body>` for the duration of the in-flight
request, and Tab can temporarily reach behind the overlay. Every
`Modal.svelte` consumer with a busy state shares this today. The eventual fix
(refocus the card via a `busy` `$effect`, or `inert` on the app root) applies
to both components and is deliberately not folded in — modal unmount still
restores focus correctly regardless, because `trapFocus.destroy()` captured
the opener at mount.

## 4. Tests

Extend the existing `GovConfirmModal` describe in
`src/lib/components/governance/__tests__/governance-primitives.test.ts`
(integration points only — `trapFocus` internals are already pinned by its own
17-test suite):

1. **Initial focus, typed severity** — typed input has focus after render.
2. **Initial focus, click severity** — Cancel button has focus after render.
3. **Escape fires `onCancel`** — dispatch `keyDown(card, { key: 'Escape' })`
   on the `[role="alertdialog"]` element (the ReshareConfirmDialog.test.ts
   pattern: trap-focus binds keydown on the node, not window).
4. **Escape during `busy` does not cancel** — same dispatch with
   `busy: true`, expect `onCancel` not called.
5. **Roles + naming** — `getByRole('alertdialog')` resolves; its
   `aria-labelledby` target is the title node; with children,
   `aria-describedby` target contains the body copy; without children, no
   `aria-describedby`.
6. **Focus restore on unmount** — focus a trigger button, render, `unmount()`,
   expect focus back on the trigger.

Existing four GovConfirmModal tests keep passing unmodified (they select by
button role/label, not by container role).

## 5. Gates

Frontend-only change: `npx tsc --noEmit` + `npx vitest run` locally; full CI
(4 jobs) on the PR. No Rust surface touched.

## 6. Follow-ups (not this ticket)

* **Busy-window focus gap** (§3.5) — shared fix for Modal.svelte +
  GovConfirmModal if it ever bites in practice.
* **DelegationWidget inline bar role** — `role="alertdialog"` on a non-modal
  in-flow bar; arguably should be `role="group"` + `aria-live` announce. Only
  worth a ticket if a11y sweep work continues (ZEB-646 is the sibling).
