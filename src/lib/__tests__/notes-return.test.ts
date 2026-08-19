import { describe, it, expect } from 'vitest';
import {
  captureNotesReturnStash,
  resolveMessagesReturn,
  type NotesReturnStash,
} from '../notes-return';

// ZEB-966: clicking the Messages footer button while the Notes space is open
// should return to the view the user was in before Notes — a community, or a
// bare DM/group-chat — instead of being a no-op. The stash capture and the
// restore decision are pure; App.svelte supplies state and applies outcomes.
describe('captureNotesReturnStash (ZEB-966)', () => {
  it('captures the selected community when Notes is entered from one', () => {
    expect(
      captureNotesReturnStash({
        selectedCommunityId: 'comm-1',
        notesSelected: false,
        previous: null,
      }),
    ).toEqual({ kind: 'community', communityId: 'comm-1' });
  });

  it('captures active-channel when Notes is entered from a bare DM view', () => {
    expect(
      captureNotesReturnStash({
        selectedCommunityId: null,
        notesSelected: false,
        previous: null,
      }),
    ).toEqual({ kind: 'active-channel' });
  });

  it('preserves the previous stash on a repeated Notes click (CodeRabbit PR #718)', () => {
    // Second Notes click: the community was already cleared by the first, so
    // an unconditional capture would clobber the stash with null/active-channel
    // and make the next Messages click unable to restore.
    const previous: NotesReturnStash = { kind: 'community', communityId: 'comm-1' };
    expect(
      captureNotesReturnStash({
        selectedCommunityId: null,
        notesSelected: true,
        previous,
      }),
    ).toBe(previous);
  });

  it('most recent real view wins: a bare DM overwrites an older community stash', () => {
    expect(
      captureNotesReturnStash({
        selectedCommunityId: null,
        notesSelected: false,
        previous: { kind: 'community', communityId: 'comm-old' },
      }),
    ).toEqual({ kind: 'active-channel' });
  });
});

describe('resolveMessagesReturn (ZEB-966)', () => {
  const exists = (present: string[]) => (id: string) => present.includes(id);

  it('restores the stashed community when clicked from Notes in messages mode', () => {
    expect(
      resolveMessagesReturn({
        appMode: 'messages',
        notesSelected: true,
        stash: { kind: 'community', communityId: 'comm-1' },
        communityExists: exists(['comm-1']),
        activeChannelIsLive: () => false,
      }),
    ).toEqual({ action: 'restore-community', communityId: 'comm-1' });
  });

  it('clears Notes to reveal the surviving DM view for an active-channel stash', () => {
    // selectNotes never touches activeChannel, so returning to a bare DM is
    // just leaving Notes — the DM feed renders from the intact activeChannel.
    expect(
      resolveMessagesReturn({
        appMode: 'messages',
        notesSelected: true,
        stash: { kind: 'active-channel' },
        communityExists: exists([]),
        activeChannelIsLive: () => true,
      }),
    ).toEqual({ action: 'clear-notes' });
  });

  it('no-ops on an active-channel stash whose channel is no longer live', () => {
    expect(
      resolveMessagesReturn({
        appMode: 'messages',
        notesSelected: true,
        stash: { kind: 'active-channel' },
        communityExists: exists([]),
        activeChannelIsLive: () => false,
      }),
    ).toEqual({ action: 'none' });
  });

  it('no-ops when there is no stash (fresh zero-community user)', () => {
    expect(
      resolveMessagesReturn({
        appMode: 'messages',
        notesSelected: true,
        stash: null,
        communityExists: exists([]),
        activeChannelIsLive: () => false,
      }),
    ).toEqual({ action: 'none' });
  });

  it('no-ops when the stashed community no longer exists (left or deleted)', () => {
    expect(
      resolveMessagesReturn({
        appMode: 'messages',
        notesSelected: true,
        stash: { kind: 'community', communityId: 'comm-gone' },
        communityExists: exists(['comm-other']),
        activeChannelIsLive: () => true,
      }),
    ).toEqual({ action: 'none' });
  });

  it('no-ops when Notes is not selected (normal messages-mode click)', () => {
    expect(
      resolveMessagesReturn({
        appMode: 'messages',
        notesSelected: false,
        stash: { kind: 'community', communityId: 'comm-1' },
        communityExists: exists(['comm-1']),
        activeChannelIsLive: () => true,
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
        stash: { kind: 'community', communityId: 'comm-1' },
        communityExists: exists(['comm-1']),
        activeChannelIsLive: () => true,
      }),
    ).toEqual({ action: 'none' });
  });
});

describe('notes-return end-to-end sequences (ZEB-966)', () => {
  it('community stash survives repeated Notes clicks and still restores', () => {
    // community A → Notes → Notes again → Messages: restore A.
    const exists = (id: string) => id === 'comm-a';
    let stash = captureNotesReturnStash({
      selectedCommunityId: 'comm-a',
      notesSelected: false,
      previous: null,
    });
    stash = captureNotesReturnStash({
      selectedCommunityId: null,
      notesSelected: true,
      previous: stash,
    });
    expect(
      resolveMessagesReturn({
        appMode: 'messages',
        notesSelected: true,
        stash,
        communityExists: exists,
        activeChannelIsLive: () => false,
      }),
    ).toEqual({ action: 'restore-community', communityId: 'comm-a' });
  });

  it('DM → Notes → Messages returns to the DM, not an older community stash', () => {
    let stash = captureNotesReturnStash({
      selectedCommunityId: 'comm-a',
      notesSelected: false,
      previous: null,
    });
    // Later: user restored to comm-a, opened a DM (community cleared), then Notes.
    stash = captureNotesReturnStash({
      selectedCommunityId: null,
      notesSelected: false,
      previous: stash,
    });
    expect(
      resolveMessagesReturn({
        appMode: 'messages',
        notesSelected: true,
        stash,
        communityExists: () => true,
        activeChannelIsLive: () => true,
      }),
    ).toEqual({ action: 'clear-notes' });
  });
});
