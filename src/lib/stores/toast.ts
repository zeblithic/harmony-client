import { writable, type Readable, type Writable } from 'svelte/store';

export type Toast = {
  id: string;
  message: string;
  durationMs: number;
};

// Internal writable; consumers see a Readable view via `toastStore.toasts`.
const toastsWritable: Writable<Toast[]> = writable([]);

function nextId(): string {
  if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
    return crypto.randomUUID();
  }
  // Vitest's node environment may not have crypto.randomUUID in older runners.
  return `toast-${Date.now()}-${Math.random().toString(36).slice(2)}`;
}

function show(message: string, durationMs = 5000): string {
  const id = nextId();
  toastsWritable.update((arr) => [...arr, { id, message, durationMs }]);
  setTimeout(() => dismiss(id), durationMs);
  return id;
}

function dismiss(id: string): void {
  toastsWritable.update((arr) => arr.filter((t) => t.id !== id));
}

/**
 * Singleton toast store. Consumers subscribe to `toasts` for the
 * current list (read-only) and call `show()` / `dismiss()` to mutate.
 *
 * ZEB-298: minimum-viable toast infra. First consumer is the
 * `voting-delegate-signaled-on-your-behalf` event subscription
 * (wired in Task 10). Future toast use cases (channel invites,
 * sortition draw notifications, etc.) reuse the same `show()`.
 */
export const toastStore = {
  toasts: toastsWritable as unknown as Readable<Toast[]>,
  show,
  dismiss,
};

/**
 * Direct handle on the inner writable for components that need to
 * auto-subscribe in a Svelte template (e.g. `$toastsStore` in
 * ToastHost.svelte). Treat as read-only at call sites — mutate only
 * through `toastStore.show()` / `toastStore.dismiss()` to keep the
 * auto-dismiss timer paired with each entry.
 */
export const toastsStore: Readable<Toast[]> = toastsWritable;
