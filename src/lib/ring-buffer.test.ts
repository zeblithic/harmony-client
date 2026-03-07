import { describe, it, expect } from 'vitest';
import { RingBuffer } from './ring-buffer';

describe('RingBuffer', () => {
  it('starts empty', () => {
    const buf = new RingBuffer<number>(5);
    expect(buf.length).toBe(0);
    expect([...buf]).toEqual([]);
  });

  it('stores items up to capacity', () => {
    const buf = new RingBuffer<number>(3);
    buf.push(1);
    buf.push(2);
    buf.push(3);
    expect(buf.length).toBe(3);
    expect([...buf]).toEqual([1, 2, 3]);
  });

  it('overwrites oldest item when full', () => {
    const buf = new RingBuffer<number>(3);
    buf.push(1);
    buf.push(2);
    buf.push(3);
    buf.push(4);
    expect(buf.length).toBe(3);
    expect([...buf]).toEqual([2, 3, 4]);
  });

  it('overwrites multiple items correctly', () => {
    const buf = new RingBuffer<number>(3);
    for (let i = 1; i <= 7; i++) buf.push(i);
    expect([...buf]).toEqual([5, 6, 7]);
  });

  it('returns last item with peek()', () => {
    const buf = new RingBuffer<number>(3);
    buf.push(10);
    buf.push(20);
    expect(buf.peek()).toBe(20);
  });

  it('peek() returns undefined when empty', () => {
    const buf = new RingBuffer<number>(3);
    expect(buf.peek()).toBeUndefined();
  });

  it('clear() empties the buffer', () => {
    const buf = new RingBuffer<number>(3);
    buf.push(1);
    buf.push(2);
    buf.clear();
    expect(buf.length).toBe(0);
    expect([...buf]).toEqual([]);
  });

  it('handles capacity of 1', () => {
    const buf = new RingBuffer<number>(1);
    buf.push(1);
    expect([...buf]).toEqual([1]);
    buf.push(2);
    expect([...buf]).toEqual([2]);
  });

  it('is iterable with for-of', () => {
    const buf = new RingBuffer<string>(3);
    buf.push('a');
    buf.push('b');
    const result: string[] = [];
    for (const item of buf) result.push(item);
    expect(result).toEqual(['a', 'b']);
  });

  it('toArray() returns a copy', () => {
    const buf = new RingBuffer<number>(3);
    buf.push(1);
    buf.push(2);
    const arr = buf.toArray();
    arr.push(99);
    expect([...buf]).toEqual([1, 2]);
  });
});
