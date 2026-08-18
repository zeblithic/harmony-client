import { describe, it, expect } from 'vitest';
import { contactsFromFriends } from '../friend-service';
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
