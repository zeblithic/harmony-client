import { describe, it, expect } from 'vitest';
import { groupMessages, getThreadReplies, getThreadRoots, getThreadMeta } from './feed-utils';
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

function threadMsg(id: string, replyTo: string, sender = 'Alice', priority: 'quiet' | 'standard' | 'loud' = 'standard'): Message {
  return {
    id,
    sender: { address: sender.toLowerCase(), displayName: sender },
    text: `Reply ${id}`,
    timestamp: Date.now(),
    media: [],
    priority,
    replyTo,
  };
}

describe('getThreadReplies', () => {
  it('returns replies for a given root', () => {
    const messages = [msg('1'), threadMsg('2', '1'), threadMsg('3', '1'), msg('4')];
    const replies = getThreadReplies(messages, '1');
    expect(replies).toHaveLength(2);
    expect(replies[0].id).toBe('2');
    expect(replies[1].id).toBe('3');
  });

  it('returns empty array when no replies exist', () => {
    const messages = [msg('1'), msg('2')];
    expect(getThreadReplies(messages, '1')).toEqual([]);
  });
});

describe('getThreadRoots', () => {
  it('returns message IDs that have at least one reply', () => {
    const messages = [msg('1'), threadMsg('2', '1'), msg('3'), threadMsg('4', '3')];
    const roots = getThreadRoots(messages);
    expect(roots).toEqual(new Set(['1', '3']));
  });

  it('returns empty set when no threads exist', () => {
    const messages = [msg('1'), msg('2')];
    expect(getThreadRoots(messages)).toEqual(new Set());
  });
});

describe('getThreadMeta', () => {
  it('returns count and participants for each thread root', () => {
    const messages = [
      msg('1', 'standard', 'Alice'),
      threadMsg('2', '1', 'Bob'),
      threadMsg('3', '1', 'Carol'),
      threadMsg('4', '1', 'Bob'),
    ];
    const meta = getThreadMeta(messages);
    expect(meta.size).toBe(1);
    const entry = meta.get('1')!;
    expect(entry.count).toBe(3);
    expect(entry.participants).toHaveLength(2);
    expect(entry.participants.map(p => p.displayName)).toContain('Bob');
    expect(entry.participants.map(p => p.displayName)).toContain('Carol');
  });

  it('returns empty map when no threads', () => {
    expect(getThreadMeta([msg('1')])).toEqual(new Map());
  });
});

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
