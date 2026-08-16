// ZEB-943 (PR #692 review): a message rendered as "today" must gain its date
// label when the local day advances past midnight — WITHOUT the component
// remounting. Feeds persist across channel switches and long-open sessions, so
// this is the real-world path. We drive a controllable stand-in for the
// app-wide day clock and assert the visible label reclassifies on the same
// mounted instance. Assertions compare against the formatter itself, so they
// stay locale-robust (no hardcoded separators).
import { describe, it, expect, afterEach, vi } from 'vitest';
import { render, screen, cleanup } from '@testing-library/svelte';
import { tick } from 'svelte';
import type { Writable } from 'svelte/store';

vi.mock('../../day-clock', async () => {
  const { writable } = await import('svelte/store');
  return { dayClock: writable(0) };
});

import { dayClock } from '../../day-clock';
import CallEventLine from '../CallEventLine.svelte';
import { formatMessageTimestamp } from '../../time-format';
import type { Message } from '../../types';

const clock = dayClock as unknown as Writable<number>;

afterEach(cleanup);

function callMessageAt(ts: number): { message: Message; isSelf: boolean } {
  const message: Message = {
    id: 'cid-rollover',
    sender: { address: 'peer', displayName: 'Peer' },
    text: 'fallback',
    timestamp: ts,
    media: [],
    priority: 'standard',
    channel: 'space-1',
    hub: '',
    callEvent: { v: 1, callId: 'ab'.repeat(16), outcome: 'declined' },
  };
  return { message, isSelf: false };
}

describe('message timestamp reclassifies across local midnight without remount', () => {
  it('a same-day label gains its date when the day clock crosses midnight', async () => {
    const msgTs = new Date(2026, 7, 16, 23, 0, 0).getTime(); // 11:00 PM — "today"
    const beforeMidnight = new Date(2026, 7, 16, 23, 30, 0).getTime();
    const afterMidnight = new Date(2026, 7, 17, 0, 1, 0).getTime();

    clock.set(beforeMidnight);
    const { message, isSelf } = callMessageAt(msgTs);
    render(CallEventLine, { props: { message, isSelf } });
    const label = () =>
      screen.getByTestId('call-event-line').querySelector('.time')!.textContent;

    // Same local day → bare time.
    expect(label()).toBe(formatMessageTimestamp(msgTs, beforeMidnight));

    // Advance past midnight on the SAME mounted component — no re-render/remount.
    clock.set(afterMidnight);
    await tick();

    // Now a different local day → dated label; and it actually changed.
    expect(label()).toBe(formatMessageTimestamp(msgTs, afterMidnight));
    expect(label()).not.toBe(formatMessageTimestamp(msgTs, beforeMidnight));
  });
});
