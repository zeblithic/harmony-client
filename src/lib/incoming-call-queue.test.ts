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
