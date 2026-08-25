import { describe, it, expect, vi, afterEach } from 'vitest';
import { render, screen, fireEvent, cleanup } from '@testing-library/svelte';
import { writable } from 'svelte/store';
import GroupCallBar from '../GroupCallBar.svelte';
import type { GroupCallSessionState, Participant } from '../../group-call-session';

afterEach(() => { cleanup(); });

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
    setMuted: vi.fn(async () => {}),
    setPttMode: vi.fn(async () => {}),
    setPttHeld: vi.fn(),
    setDeafened: vi.fn(async () => {}),
    leave: vi.fn(async () => {}),
  };
}

function participant(p: Partial<Participant> & { ownerHex: string }): Participant {
  return {
    deviceHex: p.ownerHex + 'dev',
    muted: false,
    speaking: false,
    state: 'in-call',
    ...p,
  };
}

describe('GroupCallBar', () => {
  it('renders nothing when session is null', () => {
    render(GroupCallBar, { props: { session: null } });
    expect(screen.queryByTestId('group-call-bar')).toBeNull();
  });

  it('renders nothing when phase is idle', () => {
    const session = fakeSession({ phase: 'idle' });
    render(GroupCallBar, { props: { session: session as never } });
    expect(screen.queryByTestId('group-call-bar')).toBeNull();
  });

  it('shows the bar when phase is active / connecting / leaving', () => {
    for (const phase of ['active', 'connecting', 'leaving'] as const) {
      const session = fakeSession({ phase, startedAt: Date.now() });
      render(GroupCallBar, { props: { session: session as never } });
      expect(screen.getByTestId('group-call-bar')).toBeInTheDocument();
      cleanup();
    }
  });

  it('renders the group name in the bar head', () => {
    const session = fakeSession({ phase: 'active', startedAt: Date.now() });
    render(GroupCallBar, { props: { session: session as never, groupName: 'Crew' } });
    expect(screen.getByText('Crew')).toBeInTheDocument();
  });

  it('renders a tile per participant', () => {
    const session = fakeSession({
      phase: 'active',
      startedAt: Date.now(),
      participants: [
        participant({ ownerHex: 'aaaa', displayName: { label: 'Alice', source: 'card' }, state: 'in-call' }),
        participant({ ownerHex: 'bbbb', displayName: { label: 'Bob', source: 'card' }, state: 'ringing' }),
        participant({ ownerHex: 'cccc', displayName: { label: 'Carol', source: 'card' }, state: 'declined' }),
      ],
    });
    render(GroupCallBar, { props: { session: session as never } });
    const tiles = screen.getAllByTestId('group-call-tile');
    expect(tiles).toHaveLength(3);
    expect(screen.getByText('Alice')).toBeInTheDocument();
    expect(screen.getByText('Ringing…')).toBeInTheDocument();
    expect(screen.getByText('Declined')).toBeInTheDocument();
  });

  // ZEB-980 — tiles render through PeerName: a petname-sourced participant keeps
  // the provenance badge; a card-sourced one (same label) does not.
  it('shows the petname badge only for a petname-sourced participant', () => {
    const session = fakeSession({
      phase: 'active',
      startedAt: Date.now(),
      participants: [
        participant({ ownerHex: 'aaaa', displayName: { label: 'Alice', source: 'petname' } }),
        participant({ ownerHex: 'bbbb', displayName: { label: 'Bob', source: 'card' } }),
      ],
    });
    const { container } = render(GroupCallBar, { props: { session: session as never } });
    const tiles = screen.getAllByTestId('group-call-tile');
    // Alice (petname) carries a badge inside her tile; Bob (card) does not.
    expect(tiles[0].querySelector('.petname-badge')).not.toBeNull();
    expect(tiles[1].querySelector('.petname-badge')).toBeNull();
    // Sanity: exactly one badge in the whole bar.
    expect(container.querySelectorAll('.petname-badge')).toHaveLength(1);
  });

  it('falls back to a hex short-id (no badge) when a participant has no resolved name', () => {
    const session = fakeSession({
      phase: 'active',
      startedAt: Date.now(),
      participants: [participant({ ownerHex: 'abcdef0123', state: 'in-call' })],
    });
    const { container } = render(GroupCallBar, { props: { session: session as never } });
    expect(screen.getByText('abcdef…')).toBeInTheDocument();
    expect(container.querySelector('.petname-badge')).toBeNull();
  });

  it('marks ringing/declined tiles with a data-state attribute', () => {
    const session = fakeSession({
      phase: 'active',
      startedAt: Date.now(),
      participants: [
        participant({ ownerHex: 'bbbb', state: 'ringing' }),
        participant({ ownerHex: 'cccc', state: 'declined' }),
      ],
    });
    render(GroupCallBar, { props: { session: session as never } });
    const states = screen.getAllByTestId('group-call-tile').map((t) => t.getAttribute('data-state'));
    expect(states).toEqual(['ringing', 'declined']);
  });

  it('clicking Mute calls session.setMuted', async () => {
    const session = fakeSession({ phase: 'active', muted: true });
    render(GroupCallBar, { props: { session: session as never } });
    await fireEvent.click(screen.getByRole('button', { name: /unmute|muted/i }));
    expect(session.setMuted).toHaveBeenCalledWith(false);
  });

  it('clicking PTT calls session.setPttMode', async () => {
    const session = fakeSession({ phase: 'active', pttMode: false });
    render(GroupCallBar, { props: { session: session as never } });
    await fireEvent.click(screen.getByRole('button', { name: /push to talk/i }));
    expect(session.setPttMode).toHaveBeenCalledWith(true);
  });

  it('clicking Deafen calls session.setDeafened', async () => {
    const session = fakeSession({ phase: 'active', deafened: false });
    render(GroupCallBar, { props: { session: session as never } });
    await fireEvent.click(screen.getByTestId('group-deafen'));
    expect(session.setDeafened).toHaveBeenCalledWith(true);
  });

  it('clicking Leave calls session.leave', async () => {
    const session = fakeSession({ phase: 'active' });
    render(GroupCallBar, { props: { session: session as never } });
    await fireEvent.click(screen.getByRole('button', { name: /leave call/i }));
    expect(session.leave).toHaveBeenCalled();
  });

  it('shows a Reconnecting… badge while reconnecting', () => {
    const session = fakeSession({ phase: 'active', startedAt: Date.now(), reconnecting: true });
    render(GroupCallBar, { props: { session: session as never } });
    expect(screen.getByTestId('group-call-reconnecting')).toBeInTheDocument();
  });

  it('PTT-mode swaps the talk control to a hold button', () => {
    const session = fakeSession({ phase: 'active', pttMode: true, pttHeld: false });
    render(GroupCallBar, { props: { session: session as never } });
    const hold = screen.getByTestId('group-ptt-hold');
    expect(hold).toBeInTheDocument();
    fireEvent.pointerDown(hold);
    expect(session.setPttHeld).toHaveBeenCalledWith(true);
    fireEvent.pointerUp(hold);
    expect(session.setPttHeld).toHaveBeenCalledWith(false);
  });

  it('PTT hold is keyboard-operable (Space/Enter hold; blur releases)', () => {
    const session = fakeSession({ phase: 'active', pttMode: true, pttHeld: false });
    render(GroupCallBar, { props: { session: session as never } });
    const hold = screen.getByTestId('group-ptt-hold');
    // Space holds, keyup releases.
    fireEvent.keyDown(hold, { key: ' ' });
    expect(session.setPttHeld).toHaveBeenCalledWith(true);
    fireEvent.keyUp(hold, { key: ' ' });
    expect(session.setPttHeld).toHaveBeenCalledWith(false);
    // OS key-repeat must not re-fire the hold.
    session.setPttHeld.mockClear();
    fireEvent.keyDown(hold, { key: 'Enter', repeat: true });
    expect(session.setPttHeld).not.toHaveBeenCalled();
    // Losing focus while held releases the gate.
    fireEvent.keyDown(hold, { key: 'Enter' });
    expect(session.setPttHeld).toHaveBeenCalledWith(true);
    fireEvent.blur(hold);
    expect(session.setPttHeld).toHaveBeenCalledWith(false);
  });
});
