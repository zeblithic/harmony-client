import { describe, it, expect } from 'vitest';
import { selfCommunityPower } from './community-self-power';

const member = (address: string, power: number) => ({ address, power });

describe('selfCommunityPower (ZEB-396)', () => {
  it('returns the matching owner_id row power', () => {
    const members = [member('cb7026bb877c6e580a5a35e5a4e1f857', 100), member('aa11', 0)];
    expect(selfCommunityPower(members, 'cb7026bb877c6e580a5a35e5a4e1f857')).toBe(100);
  });

  it('returns 0 when selfOwnerId is null (owner identity not loaded yet)', () => {
    expect(selfCommunityPower([member('cb7026bb877c6e580a5a35e5a4e1f857', 100)], null)).toBe(0);
  });

  it('returns 0 when matched against the wrong identity (e.g. the node address)', () => {
    // a node address (get_node_addr) never matches the owner_id-keyed roster — the original bug.
    expect(
      selfCommunityPower(
        [member('cb7026bb877c6e580a5a35e5a4e1f857', 100)],
        'a888ba9ecd0635acea2af590a70f02a8',
      ),
    ).toBe(0);
  });

  it('returns 0 for an empty roster', () => {
    expect(selfCommunityPower([], 'cb7026bb877c6e580a5a35e5a4e1f857')).toBe(0);
  });
});
