import { describe, it, expect } from 'vitest';
import {
  createIncomingCallQueue,
  type IncomingCallEvent,
} from './incoming-call-queue';

function ev(n: number): IncomingCallEvent {
  return {
    callId: `call-${n}`,
    callerOwner: `${n}`.padStart(2, '0').repeat(32),
    spaceId: `space-${n}`,
  };
}

describe('incoming-call-queue (ZEB-364)', () => {
  it('drains buffered events in FIFO arrival order', () => {
    const q = createIncomingCallQueue();
    q.queue(ev(1));
    q.queue(ev(2));
    q.queue(ev(3));
    const drained = q.drain();
    expect(drained.map((e) => e.callId)).toEqual(['call-1', 'call-2', 'call-3']);
  });

  it('drain clears the buffer — a second drain returns nothing', () => {
    const q = createIncomingCallQueue();
    q.queue(ev(1));
    expect(q.drain()).toHaveLength(1);
    expect(q.drain()).toEqual([]);
    expect(q.size).toBe(0);
  });

  it('draining an empty queue returns an empty array (the no-early-events case)', () => {
    const q = createIncomingCallQueue();
    expect(q.drain()).toEqual([]);
  });

  it('size reflects the number of buffered events', () => {
    const q = createIncomingCallQueue();
    expect(q.size).toBe(0);
    q.queue(ev(1));
    q.queue(ev(2));
    expect(q.size).toBe(2);
    q.drain();
    expect(q.size).toBe(0);
  });

  it('two queues are independent (1:1 vs group buffers must not cross)', () => {
    const dm = createIncomingCallQueue();
    const group = createIncomingCallQueue();
    dm.queue(ev(1));
    group.queue(ev(2));
    group.queue(ev(3));
    expect(dm.drain().map((e) => e.callId)).toEqual(['call-1']);
    expect(group.drain().map((e) => e.callId)).toEqual(['call-2', 'call-3']);
  });

  it('suppresses a buffered ring whose call was canceled during the window (ZEB-364 phantom-ring)', () => {
    const q = createIncomingCallQueue();
    q.queue(ev(1));
    q.queue(ev(2));
    q.cancel('call-1'); // caller rang then canceled before the session built
    const drained = q.drain();
    expect(drained.map((e) => e.callId)).toEqual(['call-2']);
  });

  it('cancel is order-independent — recorded before the matching queue still suppresses', () => {
    const q = createIncomingCallQueue();
    q.cancel('call-1');
    q.queue(ev(1));
    expect(q.drain()).toEqual([]);
  });

  it('a cancel for an unrelated callId leaves the ring intact', () => {
    const q = createIncomingCallQueue();
    q.queue(ev(1));
    q.cancel('call-999');
    expect(q.drain().map((e) => e.callId)).toEqual(['call-1']);
  });

  it('drain clears cancellations too — a later identical callId is not suppressed', () => {
    const q = createIncomingCallQueue();
    q.queue(ev(1));
    q.cancel('call-1');
    expect(q.drain()).toEqual([]);
    // A fresh ring with the same id in a later window must NOT be suppressed by
    // a stale cancellation.
    q.queue(ev(1));
    expect(q.drain().map((e) => e.callId)).toEqual(['call-1']);
  });

  it('size excludes canceled events so it matches what drain returns (Greptile)', () => {
    const q = createIncomingCallQueue();
    q.queue(ev(1));
    q.queue(ev(2));
    expect(q.size).toBe(2);
    q.cancel('call-1');
    expect(q.size).toBe(1); // the canceled event no longer counts
    expect(q.drain().map((e) => e.callId)).toEqual(['call-2']);
  });

  it('caps at 32 buffered events and drops the newest beyond the cap', () => {
    const q = createIncomingCallQueue();
    for (let i = 0; i < 40; i++) q.queue(ev(i));
    expect(q.size).toBe(32);
    const drained = q.drain();
    // The first 32 (oldest) are kept; events 32..39 are dropped.
    expect(drained).toHaveLength(32);
    expect(drained[0].callId).toBe('call-0');
    expect(drained[31].callId).toBe('call-31');
  });
});
