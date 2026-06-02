import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import { writable } from 'svelte/store';
import VoiceChannelView from '../VoiceChannelView.svelte';

function fakeSession(state: object) {
  return {
    state: writable({
      phase: 'idle',
      community: null,
      channel: null,
      muted: true,
      deafened: false,
      pttMode: false,
      pttHeld: false,
      roster: [],
      ...state,
    }),
    join: vi.fn(async () => {}),
    leave: vi.fn(async () => {}),
    setMuted: vi.fn(async () => {}),
    setDeafened: vi.fn(async () => {}),
    setPttMode: vi.fn(async () => {}),
    setPttHeld: vi.fn(),
  };
}

const base = { channelName: 'General', communityId: 'c', channelId: 'ch' };

function roster(n: number) {
  return Array.from({ length: n }, (_, i) => ({
    ownerHex: String(i).padStart(2, '0').repeat(16),
    deviceHex: String(i).repeat(16),
    muted: false,
    speaking: i === 0,
    displayName: `User${i}`,
  }));
}

describe('VoiceChannelView (V3): join flow + control bar', () => {
  it('renders header with participant count', () => {
    const session = fakeSession({
      phase: 'connected',
      roster: [
        { ownerHex: 'a', deviceHex: 'a', muted: false, speaking: false },
        { ownerHex: 'b', deviceHex: 'b', muted: true, speaking: false },
      ],
    });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.getByText(/General/)).toBeInTheDocument();
    expect(screen.getByText(/2 here/)).toBeInTheDocument();
  });

  it('Join triggers session.join (connects muted)', async () => {
    const session = fakeSession({ phase: 'idle' });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    await fireEvent.click(screen.getByRole('button', { name: /join/i }));
    expect(session.join).toHaveBeenCalledWith('c', 'ch');
  });

  it('shows an unmute control when connected & muted', () => {
    const session = fakeSession({ phase: 'connected', muted: true });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    const btn = screen.getByRole('button', { name: /unmute|muted/i });
    expect(btn).toBeInTheDocument();
  });

  it('toggles mute via session.setMuted', async () => {
    const session = fakeSession({ phase: 'connected', muted: true });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    await fireEvent.click(screen.getByRole('button', { name: /unmute|muted/i }));
    expect(session.setMuted).toHaveBeenCalledWith(false);
  });

  it('Leave triggers session.leave', async () => {
    const session = fakeSession({ phase: 'connected' });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    await fireEvent.click(screen.getByRole('button', { name: /leave/i }));
    expect(session.leave).toHaveBeenCalled();
  });

  it('shows a "voice channel full" alert when the session bounced (channelFull)', () => {
    // The soft cap bounces an over-cap join back to idle with channelFull:true;
    // the join pane must surface the reason so the user knows why.
    const session = fakeSession({ phase: 'idle', channelFull: true });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.getByRole('alert')).toHaveTextContent(/voice channel full/i);
  });
});

describe('VoiceChannelView (V3): hybrid grid<->list roster', () => {
  it('renders a grid at/below 12 participants', () => {
    const session = fakeSession({ phase: 'connected', roster: roster(12) });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.getByTestId('voice-grid')).toBeInTheDocument();
    expect(screen.queryByTestId('voice-list')).not.toBeInTheDocument();
    expect(screen.getAllByTestId('voice-tile')).toHaveLength(12);
  });

  it('collapses to a compact list past 12 participants', () => {
    const session = fakeSession({ phase: 'connected', roster: roster(13) });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.getByTestId('voice-list')).toBeInTheDocument();
    expect(screen.queryByTestId('voice-grid')).not.toBeInTheDocument();
    expect(screen.getAllByTestId('voice-list-row')).toHaveLength(13);
  });

  it('shows a speaking ring for speaking members', () => {
    const session = fakeSession({ phase: 'connected', roster: roster(2) });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    const tiles = screen.getAllByTestId('voice-tile');
    expect(tiles[0].className).toMatch(/speaking/);
    expect(tiles[1].className).not.toMatch(/speaking/);
  });
});

describe('VoiceChannelView (V3): push-to-talk hold', () => {
  it('toggles PTT mode via session.setPttMode', async () => {
    const session = fakeSession({ phase: 'connected' });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    await fireEvent.click(screen.getByRole('button', { name: /push to talk/i }));
    expect(session.setPttMode).toHaveBeenCalledWith(true);
  });

  it('in PTT mode shows a Hold-to-Talk control instead of the mute toggle', () => {
    const session = fakeSession({ phase: 'connected', pttMode: true, muted: false });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.getByTestId('ptt-hold')).toBeInTheDocument();
    // The mute toggle is replaced while in PTT mode.
    expect(screen.queryByRole('button', { name: /unmute|^mute$/i })).not.toBeInTheDocument();
  });

  it('pointer press/release on the hold control drives setPttHeld', async () => {
    const session = fakeSession({ phase: 'connected', pttMode: true, muted: false });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    const hold = screen.getByTestId('ptt-hold');
    await fireEvent.pointerDown(hold);
    expect(session.setPttHeld).toHaveBeenCalledWith(true);
    await fireEvent.pointerUp(hold);
    expect(session.setPttHeld).toHaveBeenCalledWith(false);
  });

  it('Space holds and releases PTT while in PTT mode', async () => {
    const session = fakeSession({ phase: 'connected', pttMode: true, muted: false });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    await fireEvent.keyDown(window, { code: 'Space' });
    expect(session.setPttHeld).toHaveBeenCalledWith(true);
    await fireEvent.keyUp(window, { code: 'Space' });
    expect(session.setPttHeld).toHaveBeenCalledWith(false);
  });

  it('Space is ignored when not in PTT mode', async () => {
    const session = fakeSession({ phase: 'connected', pttMode: false });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    await fireEvent.keyDown(window, { code: 'Space' });
    expect(session.setPttHeld).not.toHaveBeenCalled();
  });
});
