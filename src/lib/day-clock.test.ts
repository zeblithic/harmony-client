// ZEB-943: app-wide day clock that advances at local midnight so date-aware
// timestamp labels reclassify without remounting any component.
import { describe, it, expect, vi, afterEach } from 'vitest';
import { msUntilNextLocalMidnight, createDayClock } from './day-clock';

afterEach(() => {
  vi.useRealTimers();
});

describe('msUntilNextLocalMidnight', () => {
  it('is the gap to the upcoming local midnight', () => {
    const now = new Date(2026, 7, 16, 23, 55, 0, 0).getTime();
    expect(msUntilNextLocalMidnight(now)).toBe(5 * 60_000);
  });

  it('is a full day at exactly local midnight', () => {
    const now = new Date(2026, 7, 16, 0, 0, 0, 0).getTime();
    expect(msUntilNextLocalMidnight(now)).toBe(24 * 60 * 60_000);
  });

  it('is always strictly positive (never fires immediately or in the past)', () => {
    const now = new Date(2026, 7, 16, 12, 30, 15, 123).getTime();
    expect(msUntilNextLocalMidnight(now)).toBeGreaterThan(0);
  });
});

describe('createDayClock', () => {
  it('emits a new day after crossing local midnight, without re-subscribing', () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date(2026, 7, 16, 23, 59, 30));
    const clock = createDayClock();
    const seen: number[] = [];
    const unsub = clock.subscribe((v) => seen.push(v));

    // Initial value is "today".
    expect(new Date(seen[0]).getDate()).toBe(16);

    // Cross midnight (30s to boundary + 1s margin) with the SAME subscription.
    vi.advanceTimersByTime(31_000);

    expect(seen.length).toBeGreaterThan(1);
    expect(new Date(seen[seen.length - 1]).getDate()).toBe(17);
    unsub();
  });

  it('stops its timer once the last subscriber leaves', () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date(2026, 7, 16, 12, 0, 0));
    const clock = createDayClock();
    const seen: number[] = [];
    const unsub = clock.subscribe((v) => seen.push(v));
    unsub();
    const countAfterUnsub = seen.length;
    // Advancing well past several midnights must not push more values.
    vi.advanceTimersByTime(3 * 24 * 60 * 60_000);
    expect(seen.length).toBe(countAfterUnsub);
  });
});
