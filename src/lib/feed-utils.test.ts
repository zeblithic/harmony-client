import { describe, it, expect } from 'vitest';
import { groupMessages } from './feed-utils';
import type { Message } from './types';

function msg(id: string, priority: 'quiet' | 'standard' | 'loud' = 'standard', sender = 'Alice'): Message {
  return {
    id,
    sender: { address: sender.toLowerCase(), displayName: sender },
    text: `Message ${id}`,
    timestamp: Date.now(),
    media: [],
    priority,
  };
}

describe('groupMessages', () => {
  it('returns empty array for empty input', () => {
    expect(groupMessages([])).toEqual([]);
  });

  it('wraps standard messages as individual items', () => {
    const result = groupMessages([msg('1'), msg('2')]);
    expect(result).toHaveLength(2);
    expect(result[0].kind).toBe('message');
    expect(result[1].kind).toBe('message');
  });

  it('groups consecutive quiet messages', () => {
    const result = groupMessages([
      msg('1', 'quiet', 'Alice'),
      msg('2', 'quiet', 'Bob'),
    ]);
    expect(result).toHaveLength(1);
    expect(result[0].kind).toBe('quiet-group');
    if (result[0].kind === 'quiet-group') {
      expect(result[0].messages).toHaveLength(2);
    }
  });

  it('breaks group on non-quiet message', () => {
    const result = groupMessages([
      msg('1', 'quiet'),
      msg('2', 'standard'),
      msg('3', 'quiet'),
    ]);
    expect(result).toHaveLength(3);
    expect(result[0].kind).toBe('quiet-group');
    expect(result[1].kind).toBe('message');
    expect(result[2].kind).toBe('quiet-group');
  });

  it('wraps loud messages as individual items', () => {
    const result = groupMessages([msg('1', 'loud')]);
    expect(result).toHaveLength(1);
    expect(result[0].kind).toBe('message');
  });

  it('handles single quiet message as a group of one', () => {
    const result = groupMessages([msg('1', 'quiet')]);
    expect(result).toHaveLength(1);
    expect(result[0].kind).toBe('quiet-group');
    if (result[0].kind === 'quiet-group') {
      expect(result[0].messages).toHaveLength(1);
    }
  });

  it('handles all-quiet input as single group', () => {
    const result = groupMessages([
      msg('1', 'quiet'),
      msg('2', 'quiet'),
      msg('3', 'quiet'),
    ]);
    expect(result).toHaveLength(1);
    expect(result[0].kind).toBe('quiet-group');
    if (result[0].kind === 'quiet-group') {
      expect(result[0].messages).toHaveLength(3);
    }
  });

  it('handles mixed priorities correctly', () => {
    const result = groupMessages([
      msg('1', 'standard'),
      msg('2', 'quiet'),
      msg('3', 'quiet'),
      msg('4', 'loud'),
      msg('5', 'quiet'),
      msg('6', 'standard'),
    ]);
    expect(result).toHaveLength(5);
    expect(result[0].kind).toBe('message');
    expect(result[1].kind).toBe('quiet-group');
    expect(result[2].kind).toBe('message');
    expect(result[3].kind).toBe('quiet-group');
    expect(result[4].kind).toBe('message');
  });
});
