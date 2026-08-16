// ZEB-943: an app-wide "current day" clock.
//
// Date-aware message timestamps (see time-format.ts) decide "is this message
// from today?" against a reference instant. If that reference is captured once
// at component mount, a client left open across local midnight keeps comparing
// against yesterday — so a message sent last night stays a bare time and a
// message sent this morning can wrongly gain a date. Harmony clients stay open
// for days, and switching channels/threads updates props in place rather than
// remounting the feed, so a mount-time capture never refreshes on its own.
//
// This module is the single source of truth: ONE timer for the whole app that
// fires at each local midnight (not per-component, not per-message — ZEB-242),
// re-emitting the current instant so every subscribed timestamp reclassifies at
// once. The value is only guaranteed fresh at day granularity, which is all the
// "today?" decision needs.

import { readable, type Readable } from 'svelte/store';

/** Milliseconds from `now` until the next local midnight (00:00:00.000). */
export function msUntilNextLocalMidnight(now: number): number {
  const d = new Date(now);
  const nextMidnight = new Date(
    d.getFullYear(),
    d.getMonth(),
    d.getDate() + 1,
    0,
    0,
    0,
    0,
  );
  return nextMidnight.getTime() - now;
}

/**
 * Build a day clock: a readable store of the current instant that re-emits at
 * each local midnight. The timer is owned by the store's subscription
 * lifecycle, so it runs only while something is subscribed and is torn down
 * when the last subscriber leaves. Exposed as a factory for deterministic
 * testing under fake timers; the app uses the shared `dayClock` singleton.
 */
export function createDayClock(): Readable<number> {
  return readable(Date.now(), (set) => {
    let timer: ReturnType<typeof setTimeout>;
    const scheduleNext = () => {
      timer = setTimeout(() => {
        set(Date.now());
        scheduleNext();
      }, msUntilNextLocalMidnight(Date.now()));
    };
    // Refresh immediately on (re)subscription: the store retains its last value
    // across a 0-subscriber gap and the timer is cleared on teardown, so a gap
    // that crossed midnight (e.g. the user sat in a non-message view) would
    // otherwise hand the returning subscriber a stale pre-midnight instant until
    // the following midnight. Greptile P1, PR #692.
    set(Date.now());
    scheduleNext();
    return () => clearTimeout(timer);
  });
}

/** Shared app-wide day clock. Read it as `$dayClock` and pass it as the `now`
 *  argument to the time-format helpers. */
export const dayClock: Readable<number> = createDayClock();
