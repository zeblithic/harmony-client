import { describe, it, expect, vi, beforeEach } from 'vitest';
import { get } from 'svelte/store';
import { toastStore } from '../stores/toast';

describe('toastStore', () => {
  beforeEach(() => {
    vi.useRealTimers();
    // Clear all toasts.
    const current = get(toastStore.toasts);
    current.forEach((t) => toastStore.dismiss(t.id));
  });

  it('show() adds a toast with the message', () => {
    const id = toastStore.show('hello');
    expect(typeof id).toBe('string');
    const toasts = get(toastStore.toasts);
    expect(toasts).toHaveLength(1);
    expect(toasts[0].message).toBe('hello');
    expect(toasts[0].id).toBe(id);
  });

  it('auto-dismisses after duration', () => {
    vi.useFakeTimers();
    toastStore.show('hi', 100);
    expect(get(toastStore.toasts)).toHaveLength(1);
    vi.advanceTimersByTime(150);
    expect(get(toastStore.toasts)).toHaveLength(0);
  });

  it('manual dismiss removes the targeted toast', () => {
    const a = toastStore.show('a');
    const b = toastStore.show('b');
    expect(get(toastStore.toasts)).toHaveLength(2);
    toastStore.dismiss(a);
    const remaining = get(toastStore.toasts);
    expect(remaining).toHaveLength(1);
    expect(remaining[0].id).toBe(b);
  });

  it('uses default 5000ms duration when omitted', () => {
    vi.useFakeTimers();
    toastStore.show('default-duration');
    expect(get(toastStore.toasts)).toHaveLength(1);
    vi.advanceTimersByTime(4999);
    expect(get(toastStore.toasts)).toHaveLength(1);
    vi.advanceTimersByTime(2); // advance past 5000
    expect(get(toastStore.toasts)).toHaveLength(0);
  });
});
