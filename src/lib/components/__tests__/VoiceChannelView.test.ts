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
      selfPower: 0,
      selfModMuted: false,
      selfKicked: false,
      ...state,
    }),
    join: vi.fn(async () => {}),
    leave: vi.fn(async () => {}),
    setMuted: vi.fn(async () => {}),
    setDeafened: vi.fn(async () => {}),
    setPttMode: vi.fn(async () => {}),
    setPttHeld: vi.fn(),
    clearChannelFull: vi.fn(),
    moderate: vi.fn(async () => {}),
  };
}

const base = { channelName: 'General', communityId: 'c', channelId: 'ch' };

function roster(n: number) {
  return Array.from({ length: n }, (_, i) => ({
    ownerHex: String(i).padStart(2, '0').repeat(16),
    deviceHex: String(i).repeat(16),
    muted: false,
    speaking: i === 0,
    displayName: { label: `User${i}`, source: 'card' },
    modMuted: false,
    power: 0,
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

  it('clears a stale channelFull banner on mount and when navigating channels', async () => {
    // channelFull lives on the app-wide singleton; the view clears it on mount
    // and whenever it switches to a different channel so a bounce on one channel
    // never leaks "voice channel full" onto another.
    const session = fakeSession({ phase: 'idle', channelFull: true });
    const { rerender } = render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(session.clearChannelFull).toHaveBeenCalled();
    (session.clearChannelFull as ReturnType<typeof vi.fn>).mockClear();
    await rerender({ session: session as never, ...base, channelId: 'ch-other' });
    expect(session.clearChannelFull).toHaveBeenCalled();
  });

  it('shows a persistent "listening only" note when micBlocked (ZEB-353)', () => {
    // Mic permission denied / no device → listen-only join; surface a persistent
    // informational note (role=status, not an error alert).
    const session = fakeSession({ phase: 'connected', micBlocked: true });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    const note = screen.getByTestId('voice-mic-blocked');
    expect(note).toBeInTheDocument();
    expect(note).toHaveTextContent(/listening only/i);
    expect(note).toHaveAttribute('role', 'status');
  });

  it('hides the "listening only" note when the mic is not blocked', () => {
    const session = fakeSession({ phase: 'connected', micBlocked: false });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.queryByTestId('voice-mic-blocked')).not.toBeInTheDocument();
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

  it('shows a Reconnecting… badge while reconnecting (ZEB-353)', () => {
    const session = fakeSession({ phase: 'connected', reconnecting: true });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.getByTestId('voice-reconnecting')).toBeInTheDocument();
    expect(screen.getByText(/Reconnecting/)).toBeInTheDocument();
  });

  it('hides the Reconnecting… badge when not reconnecting', () => {
    const session = fakeSession({ phase: 'connected', reconnecting: false });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.queryByTestId('voice-reconnecting')).not.toBeInTheDocument();
  });
});

describe('VoiceChannelView (ZEB-358): moderation', () => {
  it('shows mod controls only when self has power over the member', () => {
    const session = fakeSession({ phase: 'connected', selfPower: 60,
      roster: [{ ownerHex: 'b'.repeat(32), deviceHex: 'b'.repeat(64), muted: false, speaking: false, modMuted: false, power: 0 }] });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.getByTestId('mod-mute')).toBeInTheDocument();
    expect(screen.getByTestId('mod-remove')).toBeInTheDocument();
  });

  it('hides mod controls when self lacks power over the member (equal power)', () => {
    const session = fakeSession({ phase: 'connected', selfPower: 50,
      roster: [{ ownerHex: 'b'.repeat(32), deviceHex: 'b'.repeat(64), muted: false, speaking: false, modMuted: false, power: 50 }] });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.queryByTestId('mod-mute')).not.toBeInTheDocument();
  });

  it('mute control calls moderate("mute")', async () => {
    const session = fakeSession({ phase: 'connected', selfPower: 60,
      roster: [{ ownerHex: 'b'.repeat(32), deviceHex: 'b'.repeat(64), muted: false, speaking: false, modMuted: false, power: 0 }] });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    await fireEvent.click(screen.getByTestId('mod-mute'));
    expect(session.moderate).toHaveBeenCalledWith('b'.repeat(32), 'mute');
  });

  it('mute control calls moderate("unmute") when already mod-muted', async () => {
    const session = fakeSession({ phase: 'connected', selfPower: 60,
      roster: [{ ownerHex: 'b'.repeat(32), deviceHex: 'b'.repeat(64), muted: false, speaking: false, modMuted: true, power: 0 }] });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    await fireEvent.click(screen.getByTestId('mod-mute'));
    expect(session.moderate).toHaveBeenCalledWith('b'.repeat(32), 'unmute');
  });

  it('Remove requires a confirm click before kicking', async () => {
    const session = fakeSession({ phase: 'connected', selfPower: 60,
      roster: [{ ownerHex: 'b'.repeat(32), deviceHex: 'b'.repeat(64), muted: false, speaking: false, modMuted: false, power: 0 }] });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    await fireEvent.click(screen.getByTestId('mod-remove'));
    expect(session.moderate).not.toHaveBeenCalled();
    await fireEvent.click(screen.getByTestId('mod-remove-confirm'));
    expect(session.moderate).toHaveBeenCalledWith('b'.repeat(32), 'kick');
  });

  it('renders a mod-muted badge distinct from self-mute', () => {
    const session = fakeSession({ phase: 'connected', selfPower: 0,
      roster: [{ ownerHex: 'b'.repeat(32), deviceHex: 'b'.repeat(64), muted: false, speaking: false, modMuted: true, power: 0 }] });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.getByTestId('mod-muted-badge')).toBeInTheDocument();
  });

  it('shows the moderator banners', () => {
    const muted = fakeSession({ phase: 'connected', selfModMuted: true });
    const { unmount } = render(VoiceChannelView, { props: { session: muted as never, ...base } });
    expect(screen.getByTestId('self-mod-muted')).toHaveAttribute('role', 'status');
    unmount();
    const kicked = fakeSession({ phase: 'connected', selfKicked: true });
    render(VoiceChannelView, { props: { session: kicked as never, ...base } });
    expect(screen.getByTestId('self-kicked')).toHaveAttribute('role', 'alert');
  });

  it('disables the mute toggle while server-muted', () => {
    const session = fakeSession({ phase: 'connected', selfModMuted: true, muted: true });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    const muteBtn = screen.getByRole('button', { name: /unmute|muted/i });
    expect(muteBtn).toBeDisabled();
  });
});

describe('VoiceChannelView (ZEB-612 slice 1): Commons restyle — banners + join pane', () => {
  it('channel-full banner is its own clay note (not the danger error class), still an alert', () => {
    const session = fakeSession({ phase: 'idle', channelFull: true });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    const alert = screen.getByRole('alert');
    expect(alert).toHaveTextContent(/voice channel full — try again later/i);
    expect(alert.className).toMatch(/voice-full-note/);
    expect(alert.className).not.toMatch(/voice-error/);
  });

  it('join errors still use the danger error class', async () => {
    // Pin that only channel-full moved off .voice-error — real errors keep it.
    const session = fakeSession({ phase: 'idle' });
    (session.join as ReturnType<typeof vi.fn>).mockRejectedValueOnce(new Error('boom'));
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    await fireEvent.click(screen.getByRole('button', { name: /join/i }));
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent(/boom/);
    expect(alert.className).toMatch(/voice-error/);
  });

  it('mod-silenced note carries the full Commons copy', () => {
    const session = fakeSession({ phase: 'connected', selfModMuted: true });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    const note = screen.getByTestId('self-mod-muted');
    expect(note).toHaveTextContent(/You've been muted by a moderator/);
    expect(note).toHaveTextContent(/talk controls are disabled until they unmute you/i);
  });

  it('join pane shows the room glyph and keeps the join-muted hint verbatim', () => {
    const session = fakeSession({ phase: 'idle' });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.getByTestId('join-glyph')).toBeInTheDocument();
    expect(
      screen.getByText("You'll join muted — unmute when you're ready.")
    ).toBeInTheDocument();
  });
});

describe('VoiceChannelView (ZEB-612 slice 1): Commons restyle — roster', () => {
  const modMutedRoster = [{
    ownerHex: 'b'.repeat(32), deviceHex: 'b'.repeat(64),
    muted: false, speaking: false, modMuted: true, power: 0,
  }];

  it('mod-muted tile shows the "mod-muted" sub-label', () => {
    const session = fakeSession({ phase: 'connected', roster: modMutedRoster });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.getByTestId('mod-sub')).toHaveTextContent('mod-muted');
  });

  it('mod-muted list row also shows the sub-label past the grid cap', () => {
    const big = [...roster(13)];
    big[3] = { ...big[3], modMuted: true };
    const session = fakeSession({ phase: 'connected', roster: big });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.getByTestId('voice-list')).toBeInTheDocument();
    expect(screen.getByTestId('mod-sub')).toHaveTextContent('mod-muted');
  });

  it('mod controls remain clickable under the hover-reveal treatment', async () => {
    // Reveal is CSS-only (opacity); handlers must be unaffected in jsdom.
    const session = fakeSession({ phase: 'connected', selfPower: 60,
      roster: [{ ownerHex: 'b'.repeat(32), deviceHex: 'b'.repeat(64), muted: false, speaking: false, modMuted: false, power: 0 }] });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    await fireEvent.click(screen.getByTestId('mod-mute'));
    expect(session.moderate).toHaveBeenCalledWith('b'.repeat(32), 'mute');
  });
});

describe('VoiceChannelView (ZEB-612 slice 1): Commons restyle — control bar', () => {
  it('held PTT shows the transmitting label with the Space hint', () => {
    const session = fakeSession({ phase: 'connected', pttMode: true, pttHeld: true, muted: false });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.getByTestId('ptt-hold')).toHaveTextContent('🎙 Transmitting… (hold Space)');
  });

  it('unheld PTT keeps the hold-to-talk label and explains release behavior via title', () => {
    const session = fakeSession({ phase: 'connected', pttMode: true, pttHeld: false, muted: false });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    const hold = screen.getByTestId('ptt-hold');
    expect(hold).toHaveTextContent('🎙 Hold to Talk');
    expect(hold).toHaveAttribute('title', 'Release to go quiet. Replaces the mute toggle while PTT mode is on.');
  });

  it('deafen control uses the headphones glyph when not deafened', () => {
    const session = fakeSession({ phase: 'connected', deafened: false });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.getByRole('button', { name: 'Deafen' })).toHaveTextContent('🎧 Deafen');
  });
});
