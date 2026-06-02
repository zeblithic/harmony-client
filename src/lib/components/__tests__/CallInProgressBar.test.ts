import { describe, it, expect, vi, afterEach } from 'vitest';
import { render, screen, fireEvent, cleanup } from '@testing-library/svelte';
import { writable } from 'svelte/store';
import CallInProgressBar from '../CallInProgressBar.svelte';
import type { CallSessionState } from '../../call-session';

afterEach(() => { cleanup(); });

function fakeSession(stateOverrides: Partial<CallSessionState> = {}) {
  const state = writable<CallSessionState>({
    phase: 'idle',
    callId: null,
    peerOwnerHex: null,
    muted: true,
    pttMode: false,
    pttHeld: false,
    deafened: false,
    startedAt: null,
    endReason: null,
    ...stateOverrides,
  });
  return {
    state,
    setMuted: vi.fn(async () => {}),
    setPttMode: vi.fn(async () => {}),
    setPttHeld: vi.fn(),
    setDeafened: vi.fn(async () => {}),
    placeCall: vi.fn(async () => 'call-id'),
    cancel: vi.fn(async () => {}),
    accept: vi.fn(async () => {}),
    decline: vi.fn(async () => {}),
    end: vi.fn(async () => {}),
  };
}

describe('CallInProgressBar', () => {
  it('renders nothing when session is null', () => {
    render(CallInProgressBar, { props: { session: null, onEnd: vi.fn() } });
    expect(screen.queryByTestId('call-bar')).toBeNull();
  });

  it('renders nothing when phase is idle', () => {
    const session = fakeSession({ phase: 'idle' });
    render(CallInProgressBar, { props: { session: session as never, onEnd: vi.fn() } });
    expect(screen.queryByTestId('call-bar')).toBeNull();
  });

  it('shows call-bar when phase is active', () => {
    const session = fakeSession({ phase: 'active', startedAt: Date.now() });
    render(CallInProgressBar, { props: { session: session as never, onEnd: vi.fn() } });
    expect(screen.getByTestId('call-bar')).toBeInTheDocument();
  });

  it('shows call-bar when phase is connecting', () => {
    const session = fakeSession({ phase: 'connecting' });
    render(CallInProgressBar, { props: { session: session as never, onEnd: vi.fn() } });
    expect(screen.getByTestId('call-bar')).toBeInTheDocument();
  });

  it('renders a time element', () => {
    const session = fakeSession({ phase: 'active', startedAt: Date.now() });
    render(CallInProgressBar, { props: { session: session as never, onEnd: vi.fn() } });
    expect(screen.getByTestId('call-bar').querySelector('time')).toBeTruthy();
  });

  it('clicking Mute calls session.setMuted', async () => {
    const session = fakeSession({ phase: 'active', muted: true });
    render(CallInProgressBar, { props: { session: session as never, onEnd: vi.fn() } });
    await fireEvent.click(screen.getByRole('button', { name: /unmute|muted/i }));
    expect(session.setMuted).toHaveBeenCalledWith(false);
  });

  it('clicking PTT toggle calls session.setPttMode', async () => {
    const session = fakeSession({ phase: 'active', pttMode: false });
    render(CallInProgressBar, { props: { session: session as never, onEnd: vi.fn() } });
    await fireEvent.click(screen.getByRole('button', { name: /push to talk/i }));
    expect(session.setPttMode).toHaveBeenCalledWith(true);
  });

  it('clicking End calls onEnd', async () => {
    const onEnd = vi.fn();
    const session = fakeSession({ phase: 'active' });
    render(CallInProgressBar, { props: { session: session as never, onEnd } });
    await fireEvent.click(screen.getByRole('button', { name: /end call/i }));
    expect(onEnd).toHaveBeenCalled();
  });
});
