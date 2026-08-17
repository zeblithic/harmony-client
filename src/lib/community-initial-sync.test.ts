import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { createInitialSyncTracker } from './community-initial-sync';

describe('initial-sync tracker', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it('reports syncing after markJoined and stops after clear', () => {
    const t = createInitialSyncTracker();
    expect(t.isSyncing('c1')).toBe(false);
    t.markJoined('c1');
    expect(t.isSyncing('c1')).toBe(true);
    t.clear('c1');
    expect(t.isSyncing('c1')).toBe(false);
  });

  it('auto-clears after the timeout safety-valve', () => {
    const t = createInitialSyncTracker(10_000);
    t.markJoined('c1');
    expect(t.isSyncing('c1')).toBe(true);
    vi.advanceTimersByTime(10_000);
    expect(t.isSyncing('c1')).toBe(false);
  });

  it('tracks communities independently', () => {
    const t = createInitialSyncTracker();
    t.markJoined('c1');
    expect(t.isSyncing('c1')).toBe(true);
    expect(t.isSyncing('c2')).toBe(false);
  });

  it('fires onChange on mark, explicit clear, and timeout clear', () => {
    const onChange = vi.fn();
    const t = createInitialSyncTracker(10_000, onChange);
    t.markJoined('c1');
    expect(onChange).toHaveBeenCalledTimes(1); // mark
    t.clear('c1');
    expect(onChange).toHaveBeenCalledTimes(2); // explicit clear
    t.markJoined('c2');
    expect(onChange).toHaveBeenCalledTimes(3); // mark again
    vi.advanceTimersByTime(10_000);
    expect(onChange).toHaveBeenCalledTimes(4); // timeout clear
  });

  it('does not fire onChange clearing an unknown community', () => {
    const onChange = vi.fn();
    const t = createInitialSyncTracker(10_000, onChange);
    t.clear('never-marked');
    expect(onChange).not.toHaveBeenCalled();
  });
});
