/**
 * ZEB-338 / PR #169 — trap keyboard focus inside a modal dialog.
 *
 * `aria-modal="true"` alone does not stop Tab from reaching background controls
 * (e.g. the help button), so for hard-gate dialogs — the WelcomeModal and the
 * startup-error overlay — we additionally: mark sibling overlays `inert`, move
 * initial focus into the dialog, cycle Tab/Shift+Tab within it, and restore
 * focus to the previously-focused element on teardown.
 *
 * Call this from a Svelte `$effect` once the dialog element is mounted and the
 * gate is open; the returned cleanup runs on close/destroy. The `dialog` is
 * expected to be the dialog element whose `parentElement` is the backdrop
 * (its siblings under the backdrop's parent are the background to inert).
 *
 * @returns a teardown function that removes the listener, clears `inert`, and
 *          restores focus.
 */
export function trapFocus(dialog: HTMLElement): () => void {
  const backdrop = dialog.parentElement;
  const root = backdrop?.parentElement ?? null;
  const previouslyFocused = document.activeElement as HTMLElement | null;
  const inerted: HTMLElement[] = [];

  if (root && backdrop) {
    for (const child of Array.from(root.children)) {
      if (child !== backdrop && child instanceof HTMLElement && !child.hasAttribute('inert')) {
        child.setAttribute('inert', '');
        inerted.push(child);
      }
    }
  }

  const focusables = (): HTMLElement[] =>
    Array.from(
      dialog.querySelectorAll<HTMLElement>(
        'a[href], button:not([disabled]), input:not([disabled]), [tabindex]:not([tabindex="-1"])',
      ),
    );

  (focusables()[0] ?? dialog).focus();

  function onKeydown(ev: KeyboardEvent) {
    if (ev.key !== 'Tab') return;
    const items = focusables();
    if (items.length === 0) {
      ev.preventDefault();
      return;
    }
    const first = items[0];
    const last = items[items.length - 1];
    const active = document.activeElement;
    if (ev.shiftKey && active === first) {
      ev.preventDefault();
      last.focus();
    } else if (!ev.shiftKey && active === last) {
      ev.preventDefault();
      first.focus();
    }
  }

  dialog.addEventListener('keydown', onKeydown);

  return () => {
    dialog.removeEventListener('keydown', onKeydown);
    for (const el of inerted) el.removeAttribute('inert');
    previouslyFocused?.focus?.();
  };
}
