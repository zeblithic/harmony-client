import { describe, it, expect } from 'vitest';
import { feedAuthorOwnerIds } from '../feed-utils';
import type { Message } from '../types';

function msg(id: string, addr: string): Message {
  return {
    id,
    sender: { address: addr, displayName: '' },
    text: '',
    timestamp: 1,
    priority: 'standard',
  };
}

// ZEB-962 (CodeRabbit #709): the feed / media / thread render sites resolve
// author cards, which only resolve owners the MemberCardService has subscribed.
// App.svelte drives a `feedAuthors` bucket from this helper so every visible
// non-self author (incl. group-chat + thread participants) is subscribed.
describe('feedAuthorOwnerIds (ZEB-962)', () => {
  it('collects unique non-self sender addresses', () => {
    const ids = feedAuthorOwnerIds([msg('1', 'aa'), msg('2', 'bb'), msg('3', 'aa')]);
    expect([...ids].sort()).toEqual(['aa', 'bb']);
  });

  it('excludes the self sentinel', () => {
    expect(feedAuthorOwnerIds([msg('1', 'self'), msg('2', 'aa')])).toEqual(['aa']);
  });

  it('returns empty for no messages', () => {
    expect(feedAuthorOwnerIds([])).toEqual([]);
  });
});
