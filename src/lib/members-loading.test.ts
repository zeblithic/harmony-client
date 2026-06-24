import { describe, it, expect } from 'vitest';
import { shouldClearMembersLoading } from './members-loading';

describe('shouldClearMembersLoading (ZEB-553 item 11)', () => {
  it('clears when the fetch is still for the active community', () => {
    expect(shouldClearMembersLoading('comm-a', 'comm-a')).toBe(true);
  });

  it('clears when the user has left to a non-community view (active === null)', () => {
    // Qodo PR #332: leaving to Notes/DMs mid-fetch must not leave the flag stuck
    // true — nothing community-scoped is rendered, so the fetch may clear it.
    expect(shouldClearMembersLoading(null, 'comm-a')).toBe(true);
  });

  it('does NOT clear when a different community is now active (the newer switch owns it)', () => {
    // A fetch superseded by a switch to another community must not wipe the
    // loading row that community is still showing.
    expect(shouldClearMembersLoading('comm-b', 'comm-a')).toBe(false);
  });
});
