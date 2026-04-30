# ZEB-195: Modal a11y — focus trap, focus restore, Esc-to-close

**Date:** 2026-04-30
**Linear:** [ZEB-195](https://linear.app/zeblith/issue/ZEB-195)
**Origin:** CodeRabbit round-4 review on PR #62 (ZEB-170 Devices panel) flagged the four standard modal a11y gaps as Major-severity.

## Goal

Make harmony-client modals fully keyboard-operable: focus moves into the modal on open, cycles within it on Tab/Shift+Tab, restores to the trigger on close, and Escape invokes the same cancel/close path the Cancel button does (including the disabled-during-inflight semantics).

## Scope

**In:**

- New `<Modal>` component + `use:trapFocus` action (the reusable primitive).
- Migrate four modals to consume the primitive:
  - `DevicesPanel.svelte` mint-confirm modal
  - `DevicesPanel.svelte` backup modal
  - `PairingInviter.svelte`
  - `PairingJoiner.svelte`
- Tests for the primitive and for the migrated modals.

**Out (deferred to follow-up tickets):**

- `ConfirmDialog.svelte`, `DoubleConfirmDialog.svelte`, `TypeToConfirmDialog.svelte`, `VinePublishDialog.svelte`, `VinePlayer.svelte` — all share the same gaps but aren't part of the DevicesPanel keyboard flow this ticket originates from.
- `IdentityPanel.svelte` — the ticket speculated this needed the same treatment; on inspection it's an in-page wizard, not a modal overlay (no `role="dialog"` markup, no `.modal-overlay`). No change needed.
- `inert` attribute on background content — would require portal/restructure to apply correctly; Tab-loop focus trap is sufficient for the four modals in scope.
- Overlay-click-to-close — none of the existing modals do this; adding it would be a destructive UX change worth its own discussion.

## Non-goals

- Migrating away from the in-place `.modal-overlay` pattern to native `<dialog>`. The native element handles focus trap and Esc natively, but jsdom can't fully test the focus-trap behavior, and CSS overlay → `::backdrop` would be a larger visual migration. Custom action keeps test coverage honest and zero-deps.
- Adding a runtime focus-trap library (e.g., `focus-trap`). Our four targets are simple — no contenteditable, no dynamic focusable lists beyond Cancel toggling disabled. The custom action's surface fits the bounded need.

## Architecture

Two new artifacts, both small:

- `src/lib/actions/trap-focus.ts` — Svelte 5 action implementing the four behaviors (mount-focus, Tab/Shift+Tab cycling, Escape, focus restore).
- `src/lib/components/Modal.svelte` — wrapper component that renders `.modal-overlay > .modal[role=dialog][aria-modal=true]`, applies `use:trapFocus`, and forwards children. Owns the shared modal CSS that's currently duplicated across files.

Consumers keep their existing `{#if openFlag}` mount/unmount control. `<Modal>` does not own open/close state; it owns a11y.

### Consumer API

```svelte
{#if backupOpen}
  <Modal
    onCancel={closeBackup}
    canCancel={!backupInFlight && !backupDialogInFlight}
    ariaLabelledby="backup-modal-heading"
  >
    <h3 id="backup-modal-heading">Back up owner identity</h3>
    <!-- existing form markup unchanged -->
  </Modal>
{/if}
```

**Props:**

- `onCancel: () => void` — called when Escape is pressed and `canCancel` is true. Consumer is responsible for the actual close action (typically the same handler the Cancel button calls).
- `canCancel: boolean` — gates Escape. False during in-flight operations to mirror the Cancel button's `disabled` attribute. Defaults to `true`.
- `ariaLabelledby: string` — id of the heading inside the modal. Required so screen readers announce the modal title on focus.

### Action API

```ts
export interface TrapFocusParams {
  onCancel?: () => void;
  canCancel?: boolean;
}

export function trapFocus(
  node: HTMLElement,
  params: TrapFocusParams,
): { update(p: TrapFocusParams): void; destroy(): void };
```

The action handles all four behaviors internally. The Modal component is the only intended consumer in this PR, but the action lives separately so it's reusable for future modal patterns that don't fit the `<Modal>` shape (e.g., a future drawer or popover).

## Behavior contract

### Focusable element selector

```
button:not(:disabled),
[href],
input:not(:disabled):not([type="hidden"]),
select:not(:disabled),
textarea:not(:disabled),
[tabindex]:not([tabindex="-1"])
```

Filtered to exclude `[hidden]` and `[aria-hidden="true"]`. Re-queried on each Tab keydown (not cached) — modals can have buttons that toggle disabled mid-flight.

### Mount sequence

1. Capture `document.activeElement` as `previouslyFocused` (the button that opened the modal).
2. Query focusable elements within the modal node.
3. If non-empty → focus the first one.
4. If empty → set `tabindex="-1"` on the modal node itself and focus that, so screen readers anchor to the dialog.

### Keydown handler

- **Tab** (no shift): query focusables fresh; if `document.activeElement === last`, `preventDefault()` and focus first. Otherwise let the browser advance focus normally.
- **Shift+Tab**: mirror — if `activeElement === first`, `preventDefault()` and focus last.
- **Escape**: if `canCancel`, call `onCancel()`. Otherwise no-op.

### Unmount sequence

Restore focus via `previouslyFocused?.focus({ preventScroll: true })`. Wrapped in try/catch — if the trigger element was removed from the DOM during the modal's lifetime, silently fall through (focus falls back to `<body>`). `preventScroll` avoids snapping the viewport during state transitions.

### Edge cases handled

| Case | Behavior |
|---|---|
| No focusable elements in modal | Focus the modal container (after `tabindex="-1"`). Screen reader still anchors to the dialog title. |
| Focusable list changes mid-modal | Re-query on every Tab keydown — never cached. |
| Trigger element removed during modal | try/catch around restore; focus falls back to body. |
| Modal opens with no prior focus (programmatic) | `previouslyFocused` is null/body; skip restore. |
| `update()` called with new params | Replace the kept `onCancel` / `canCancel` references; keep the `previouslyFocused` capture. |

## Test strategy

jsdom supports `.focus()`, `document.activeElement`, and synthetic keydown events. Our trap manages focus manually (it doesn't rely on jsdom's tab-order computation), so tests are accurate.

### `__tests__/trap-focus.test.ts` (new, unit)

Test the action in isolation:

1. Mount node with three buttons → first button is `document.activeElement`.
2. Mount node with no focusables → modal container is `activeElement`, and node has `tabindex="-1"`.
3. Focus last button, dispatch Tab → first button is `activeElement`.
4. Focus first button, dispatch Shift+Tab → last button is `activeElement`.
5. Dispatch Tab from middle → no preventDefault, browser advances normally.
6. Dispatch Escape with `canCancel: true` → `onCancel` called once.
7. Dispatch Escape with `canCancel: false` → `onCancel` not called.
8. Unmount → previously-focused element is `activeElement`.
9. Update `canCancel` from true to false via action `update()` → next Escape no-ops.
10. Disable a button mid-modal → next Tab cycle skips it (proves we re-query).

### `__tests__/Modal.test.ts` (new)

Render `<Modal>` with stub children. Assert:

- `role="dialog"` and `aria-modal="true"` are present.
- `aria-labelledby` is wired to the prop.
- Escape inside the modal calls the `onCancel` prop when `canCancel` is true.
- The shared `.modal-overlay` and `.modal` classes are applied.

### `__tests__/DevicesPanel.test.ts` (extend, integration)

Two new tests, one per migrated modal:

- Click trigger button → modal opens → press Escape → modal closes → trigger is `activeElement`.
- Same flow with Tab cycling: focus moves into modal on open, Tab from last focusable wraps to first.

Existing 26 DevicesPanel tests must still pass.

### Pairing modals

`PairingInviter` and `PairingJoiner` already have minimal test coverage. We won't add full integration tests for the focus flow there (out of scope) — but we'll add one smoke test per file asserting the migrated modal still has `role="dialog"` + `aria-modal="true"` after refactor.

## Migration order

1. Build action + Modal + their unit tests (no consumer changes yet).
2. Migrate DevicesPanel mint-confirm modal. Add the two integration tests for it.
3. Migrate DevicesPanel backup modal. Add the two integration tests for it.
4. Migrate `PairingInviter`. Add smoke test.
5. Migrate `PairingJoiner`. Add smoke test.
6. Delete the now-unused `.modal-overlay` and `.modal` CSS rules from `DevicesPanel.svelte`, `PairingInviter.svelte`, `PairingJoiner.svelte` (they live in `Modal.svelte` now).

Each step lands a green-tested commit. The Modal + action are usable from step 1; consumers join one at a time.

## Risk surface

- **Focus restoration in tests.** jsdom's focus state is reliable for `.focus()` calls but the `activeElement` after DOM removal can be subtle. If a test fails on focus assertion, prefer assertion via `toHaveFocus()` from `@testing-library/jest-dom` (already in devDependencies).
- **CSS regression.** The shared modal styles must visually match the existing `.modal-overlay` / `.modal` rules byte-for-byte. Manual smoke test (launch GUI, open both DevicesPanel modals + both pairing modals) before requesting review.
- **Existing test breakage.** The 26 DevicesPanel tests currently rely on the `.modal-overlay[role="dialog"]` selector path. Test fixtures using `getByRole('dialog')` will keep working — the role is preserved. Tests using `.modal-overlay` as a CSS class selector will keep working — the class moves into the Modal component but is still applied to the rendered overlay div.

## Acceptance criteria

From the Linear ticket, all four required:

- Opening either DevicesPanel modal moves focus to the first focusable element.
- Tab cycles forward; Shift+Tab cycles backward; cycle wraps at boundaries.
- Escape triggers cancel/close with disabled-during-inflight semantics (via `canCancel`).
- Closing modal restores focus to the triggering button.

Plus:

- Existing tests still pass; new tests assert the four behaviors.
- Pattern documented for reuse — `<Modal>` + `use:trapFocus` are reusable primitives, follow-up tickets can migrate the remaining 5 modals.

## Verification gates

Before requesting PR review:

- `npx vitest run` — all tests green (1303 + new tests).
- `npx tsc --noEmit` — clean.
- `cargo test --manifest-path src-tauri/Cargo.toml` — green (no Rust changes, but CI runs it; defensive).
- `cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings` — clean (no Rust changes, but CI runs it).
- `cargo fmt --all -- --check` — clean (per memory rule).
- Manual smoke: launch GUI → keyboard-only operate both DevicesPanel modals and both pairing modals. Verify all four a11y behaviors in real browser.
