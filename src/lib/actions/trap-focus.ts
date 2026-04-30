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
  const previouslyFocused =
    document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const originalTabindex = node.getAttribute('tabindex');

  const focusables = focusableIn(node);
  const setTabindexFallback = focusables.length === 0;
  if (setTabindexFallback) {
    if (originalTabindex === null) node.setAttribute('tabindex', '-1');
    node.focus();
  } else {
    focusables[0].focus();
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
    if (items.length === 0) {
      e.preventDefault();
      node.focus();
      return;
    }
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
      if (setTabindexFallback && originalTabindex === null) {
        node.removeAttribute('tabindex');
      }
      try {
        previouslyFocused?.focus({ preventScroll: true });
      } catch {
        // Trigger removed; let focus fall back to body.
      }
    },
  };
}
