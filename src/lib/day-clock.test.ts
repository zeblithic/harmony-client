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

  it('re-emits a fresh instant on resubscription after a gap (no stale resume)', () => {
    // The store tears down (clears its timer) when the last subscriber leaves —
    // e.g. every message surface unmounts while the user is in Settings. If the
    // gap crosses midnight, a naive readable would hand the next subscriber its
    // retained pre-midnight value and only refresh at the FOLLOWING midnight.
    vi.useFakeTimers();
    vi.setSystemTime(new Date(2026, 7, 16, 23, 0, 0)); // 11pm, day 16
    const clock = createDayClock();

    const unsub1 = clock.subscribe(() => {});
    unsub1(); // teardown: timer cleared, value retained at day 16

    // Time advances across midnight while nothing is subscribed.
    vi.setSystemTime(new Date(2026, 7, 17, 9, 0, 0)); // 9am, day 17

    let firstSeen = 0;
    const unsub2 = clock.subscribe((v) => {
      if (firstSeen === 0) firstSeen = v;
    });
    // The value the returning subscriber sees first must be "today" (17),
    // not the stale retained day-16 value.
    expect(new Date(firstSeen).getDate()).toBe(17);
    unsub2();
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
