// ZEB-966: decisions for the Messages footer button while the Notes space is
// open. Pre-fix, `appMode` was already 'messages' so the click was inert; the
// desired behavior is returning to the view the user was in before selecting
// Notes — a community, or a bare DM/group-chat. App.svelte captures the stash
// in `selectNotes()` and applies the outcome (mirroring a left-nav click).

export type NotesReturnStash =
  // Notes was entered from a community view; restore = re-select it.
  | { kind: 'community'; communityId: string }
  // Notes was entered from a bare DM/group-chat view. `selectNotes()` never
  // touches `activeChannel`, so restoring is just leaving Notes — the DM feed
  // renders from the still-intact activeChannel state. No id is stashed:
  // while in Notes nothing can change activeChannel (any channel click clears
  // Notes first), so the live value at click time is the stash-time value.
  | { kind: 'active-channel' };

export type MessagesReturnOutcome =
  | { action: 'restore-community'; communityId: string }
  | { action: 'clear-notes' }
  | { action: 'none' };

// Called on every Notes selection, BEFORE the community is cleared. A repeated
// Notes click (already in Notes, community already cleared) must preserve the
// previous stash rather than clobber it (CodeRabbit finding on PR #718); a
// Notes entry from any real messages view overwrites — most recent view wins.
export function captureNotesReturnStash(opts: {
  selectedCommunityId: string | null;
  notesSelected: boolean;
  previous: NotesReturnStash | null;
}): NotesReturnStash | null {
  const { selectedCommunityId, notesSelected, previous } = opts;
  if (selectedCommunityId !== null) {
    return { kind: 'community', communityId: selectedCommunityId };
  }
  if (!notesSelected) return { kind: 'active-channel' };
  return previous;
}

export function resolveMessagesReturn(opts: {
  appMode: string;
  notesSelected: boolean;
  stash: NotesReturnStash | null;
  communityExists: (id: string) => boolean;
  // Whether the current activeChannel is a live left-nav DM/group-chat node —
  // checked at click time so a conversation removed while in Notes can't be
  // restored into a dead view.
  activeChannelIsLive: () => boolean;
}): MessagesReturnOutcome {
  const { appMode, notesSelected, stash, communityExists, activeChannelIsLive } = opts;
  // Only the already-in-messages no-op click restores. A cross-mode switch
  // (e.g. Vines → Messages) returns to whatever the messages view was —
  // Notes included — so it must not yank the user out of Notes.
  if (appMode !== 'messages' || !notesSelected) return { action: 'none' };
  // No stash (fresh zero-community user): stay in Notes rather than render a
  // void. Stale stashes (community left/deleted, DM removed) likewise no-op.
  if (stash === null) return { action: 'none' };
  if (stash.kind === 'community') {
    return communityExists(stash.communityId)
      ? { action: 'restore-community', communityId: stash.communityId }
      : { action: 'none' };
  }
  return activeChannelIsLive() ? { action: 'clear-notes' } : { action: 'none' };
}
