import { describe, it, expect } from 'vitest';
import { resolveMessagesReturn } from '../notes-return';

// ZEB-966: clicking the Messages footer button while the Notes space is open
// should return to the community the user was in before Notes — instead of
// being a no-op. The decision (restore vs. leave alone) is pure; App.svelte
// supplies the stash and applies the outcome.
describe('resolveMessagesReturn (ZEB-966)', () => {
  const exists = (present: string[]) => (id: string) => present.includes(id);

  it('restores the stashed community when clicked from Notes in messages mode', () => {
    expect(
      resolveMessagesReturn({
        appMode: 'messages',
        notesSelected: true,
        stashedCommunityId: 'comm-1',
        communityExists: exists(['comm-1']),
      }),
    ).toEqual({ action: 'restore-community', communityId: 'comm-1' });
  });

  it('no-ops when there is no stashed community (fresh zero-community user)', () => {
    expect(
      resolveMessagesReturn({
        appMode: 'messages',
        notesSelected: true,
        stashedCommunityId: null,
        communityExists: exists([]),
      }),
    ).toEqual({ action: 'none' });
  });

  it('no-ops when the stashed community no longer exists (left or deleted)', () => {
    expect(
      resolveMessagesReturn({
        appMode: 'messages',
        notesSelected: true,
        stashedCommunityId: 'comm-gone',
        communityExists: exists(['comm-other']),
      }),
    ).toEqual({ action: 'none' });
  });

  it('no-ops when Notes is not selected (normal messages-mode click)', () => {
    expect(
      resolveMessagesReturn({
        appMode: 'messages',
        notesSelected: false,
        stashedCommunityId: 'comm-1',
        communityExists: exists(['comm-1']),
      }),
    ).toEqual({ action: 'none' });
  });

  it('no-ops when arriving from another mode — Notes stays the messages view', () => {
    // From e.g. Vines, the Messages button returns to whatever the messages
    // view was (Notes included). Only the already-in-messages no-op case
    // restores; a cross-mode switch must not yank the user out of Notes.
    expect(
      resolveMessagesReturn({
        appMode: 'vines',
        notesSelected: true,
        stashedCommunityId: 'comm-1',
        communityExists: exists(['comm-1']),
      }),
    ).toEqual({ action: 'none' });
  });
});
