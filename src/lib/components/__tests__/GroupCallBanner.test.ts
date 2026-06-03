import { describe, it, expect, vi, afterEach, beforeEach } from 'vitest';
import { render, screen, fireEvent, cleanup, waitFor } from '@testing-library/svelte';
import { writable } from 'svelte/store';
import GroupCallBanner from '../GroupCallBanner.svelte';
import { groupCallBanners } from '../../group-call-banner-store';
import { invalidateGroupMembers } from '../../group-dm-members-cache';
import type { GroupCallSessionState } from '../../group-call-session';

afterEach(() => {
  cleanup();
  // Reset the singleton banner store between tests (clear any entry we set).
  groupCallBanners.apply('space-1', 'call-1', []);
  groupCallBanners.apply('space-1', 'call-2', []);
});

beforeEach(() => {
  invalidateGroupMembers();
});

function fakeSession(stateOverrides: Partial<GroupCallSessionState> = {}) {
  const state = writable<GroupCallSessionState>({
    phase: 'idle',
    callId: null,
    spaceId: null,
    participants: [],
    muted: true,
    pttMode: false,
    pttHeld: false,
    deafened: false,
    startedAt: null,
    reconnecting: false,
    callerOwnerHex: null,
    ...stateOverrides,
  });
  return {
    state,
    joinActive: vi.fn(async () => {}),
  };
}

const roster = [{ owner: 'aaaa', device: 'dev-a', muted: false }];

describe('GroupCallBanner', () => {
  it('renders nothing when there is no active call for the space', () => {
    const session = fakeSession();
    render(GroupCallBanner, {
      props: { spaceId: 'space-1', invoke: vi.fn(async () => []), groupCall: session as never },
    });
    expect(screen.queryByTestId('group-call-banner')).toBeNull();
  });

  it('shows the banner with the in-call count when a call is active and the user is not in it', async () => {
    groupCallBanners.apply('space-1', 'call-1', roster);
    const session = fakeSession({ phase: 'idle' });
    render(GroupCallBanner, {
      props: { spaceId: 'space-1', invoke: vi.fn(async () => []), groupCall: session as never },
    });
    expect(await screen.findByTestId('group-call-banner')).toBeInTheDocument();
    expect(screen.getByText(/1 in call/)).toBeInTheDocument();
  });

  it('hides the banner when the local user is already in that call', async () => {
    groupCallBanners.apply('space-1', 'call-1', roster);
    // Session is in the SAME call → banner suppressed (in-call bar takes over).
    const session = fakeSession({ phase: 'active', callId: 'call-1', spaceId: 'space-1' });
    render(GroupCallBanner, {
      props: { spaceId: 'space-1', invoke: vi.fn(async () => []), groupCall: session as never },
    });
    await waitFor(() => {
      expect(screen.queryByTestId('group-call-banner')).toBeNull();
    });
  });

  it('still shows the banner when the user is in a DIFFERENT call', async () => {
    groupCallBanners.apply('space-1', 'call-2', roster);
    const session = fakeSession({ phase: 'active', callId: 'call-other', spaceId: 'space-1' });
    render(GroupCallBanner, {
      props: { spaceId: 'space-1', invoke: vi.fn(async () => []), groupCall: session as never },
    });
    expect(await screen.findByTestId('group-call-banner')).toBeInTheDocument();
  });

  it('Join warms members then calls joinActive with the active callId', async () => {
    groupCallBanners.apply('space-1', 'call-1', roster);
    const session = fakeSession({ phase: 'idle' });
    const invoke = vi.fn(async (cmd: string) => (cmd === 'get_group_dm_members' ? ['aaaa', 'bbbb'] : undefined));
    render(GroupCallBanner, {
      props: { spaceId: 'space-1', invoke: invoke as never, groupCall: session as never },
    });
    await fireEvent.click(await screen.findByTestId('group-call-join'));
    await waitFor(() => {
      expect(session.joinActive).toHaveBeenCalledWith('call-1', 'space-1');
    });
    expect(invoke).toHaveBeenCalledWith('get_group_dm_members', { spaceId: 'space-1' });
  });
});
