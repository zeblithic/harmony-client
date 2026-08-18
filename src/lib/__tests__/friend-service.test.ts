import { describe, it, expect } from 'vitest';
import { contactsFromFriends, nicknameMapFromFriends } from '../friend-service';
import type { FriendDto } from '../friend-service';

// ZEB-962: `contactsFromFriends` bakes the DM contact-cache display name. Its
// precedence is nickname → published display → short-hex, and "empty strings
// are treated as absent" — but the original `||` chain only drops `""`, letting
// a whitespace-only nickname/display through as a truthy blank label. `nonEmpty`
// closes that at the ingest boundary so no blank name enters the contacts cache.
const OWNER = 'ab'.repeat(16); // 32-char owner_id hex
const SHORT = OWNER.slice(0, 8) + '…';

function friend(over: Partial<FriendDto>): FriendDto {
  return { ownerIdHex: OWNER, status: 'active', ...over } as FriendDto;
}

describe('contactsFromFriends display-name ladder (ZEB-962)', () => {
  it('prefers a non-blank nickname', () => {
    const c = contactsFromFriends([friend({ nickname: 'Al', display: 'Alice' })]);
    expect(c.get(OWNER)?.displayName).toBe('Al');
  });

  it('falls through a whitespace-only nickname to the published display name', () => {
    const c = contactsFromFriends([friend({ nickname: '   ', display: 'Alice' })]);
    expect(c.get(OWNER)?.displayName).toBe('Alice');
  });

  it('falls through whitespace nickname AND whitespace display to short hex', () => {
    const c = contactsFromFriends([friend({ nickname: '  ', display: '   ' })]);
    expect(c.get(OWNER)?.displayName).toBe(SHORT);
  });

  it('falls through null nickname/display to short hex', () => {
    const c = contactsFromFriends([friend({ nickname: null, display: null })]);
    expect(c.get(OWNER)?.displayName).toBe(SHORT);
  });
});

// ZEB-962 (CodeRabbit #709): `resolveNickname` reads a nickname map App.svelte
// built with a plain truthiness filter, so a whitespace-only nickname stayed in
// state. This shared builder keeps only non-blank nicknames (nonEmpty), keyed by
// lowercased owner hex — so `resolveNickname` can never return a blank.
describe('nicknameMapFromFriends (ZEB-962)', () => {
  it('keeps a non-blank nickname under the lowercased owner key', () => {
    const m = nicknameMapFromFriends([friend({ nickname: 'Al' })]);
    expect(m.get(OWNER.toLowerCase())).toBe('Al');
  });

  it('omits a whitespace-only nickname', () => {
    const m = nicknameMapFromFriends([friend({ nickname: '   ' })]);
    expect(m.has(OWNER.toLowerCase())).toBe(false);
  });

  it('omits a null nickname', () => {
    const m = nicknameMapFromFriends([friend({ nickname: null })]);
    expect(m.has(OWNER.toLowerCase())).toBe(false);
  });
});
