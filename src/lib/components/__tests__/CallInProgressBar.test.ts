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
    peerDisplayName: null,
    muted: true,
    pttMode: false,
    pttHeld: false,
    deafened: false,
    startedAt: null,
    endReason: null,
    reconnecting: false,
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

  it('clicking Deafen calls session.setDeafened(true) when not deafened', async () => {
    const session = fakeSession({ phase: 'active', deafened: false });
    render(CallInProgressBar, { props: { session: session as never, onEnd: vi.fn() } });
    const btn = screen.getByTestId('deafen');
    expect(btn).toHaveTextContent('🔈 Deafen');
    expect(btn).toHaveAttribute('aria-pressed', 'false');
    await fireEvent.click(btn);
    expect(session.setDeafened).toHaveBeenCalledWith(true);
  });

  it('clicking Deafen calls session.setDeafened(false) when already deafened', async () => {
    const session = fakeSession({ phase: 'active', deafened: true });
    render(CallInProgressBar, { props: { session: session as never, onEnd: vi.fn() } });
    const btn = screen.getByTestId('deafen');
    expect(btn).toHaveTextContent('🔕 Deafened');
    expect(btn).toHaveAttribute('aria-pressed', 'true');
    await fireEvent.click(btn);
    expect(session.setDeafened).toHaveBeenCalledWith(false);
  });

  it('clicking End calls onEnd', async () => {
    const onEnd = vi.fn();
    const session = fakeSession({ phase: 'active' });
    render(CallInProgressBar, { props: { session: session as never, onEnd } });
    await fireEvent.click(screen.getByRole('button', { name: /end call/i }));
    expect(onEnd).toHaveBeenCalled();
  });

  // ZEB-958 — the in-call bar prefers the resolved peer name over raw hex.
  it('renders the resolved peer name when peerDisplayName is set', () => {
    const session = fakeSession({
      phase: 'active', startedAt: Date.now(),
      peerOwnerHex: 'abcdef'.repeat(5) + 'ab', peerDisplayName: { label: 'Ziggy', source: 'card' },
    });
    render(CallInProgressBar, { props: { session: session as never, onEnd: vi.fn() } });
    expect(screen.getByText('Ziggy')).toBeInTheDocument();
    // The hex short-id must NOT also show once a name resolved.
    expect(screen.queryByText(/abcdef…/)).toBeNull();
  });

  // ZEB-980 — the bar renders through PeerName, so a petname-sourced peer keeps
  // its provenance badge; a card-sourced peer (same label) does NOT get one.
  it('shows the petname badge for a petname-sourced peer, not a card-sourced one', () => {
    const petSession = fakeSession({
      phase: 'active', startedAt: Date.now(),
      peerOwnerHex: 'abcdef'.repeat(5) + 'ab', peerDisplayName: { label: 'Ziggy', source: 'petname' },
    });
    const { container, unmount } = render(CallInProgressBar, { props: { session: petSession as never, onEnd: vi.fn() } });
    expect(screen.getByText('Ziggy')).toBeInTheDocument();
    expect(container.querySelector('.petname-badge')).not.toBeNull();
    unmount();

    const cardSession = fakeSession({
      phase: 'active', startedAt: Date.now(),
      peerOwnerHex: 'abcdef'.repeat(5) + 'ab', peerDisplayName: { label: 'Ziggy', source: 'card' },
    });
    const { container: cardContainer } = render(CallInProgressBar, { props: { session: cardSession as never, onEnd: vi.fn() } });
    expect(cardContainer.querySelector('.petname-badge')).toBeNull();
  });

  it('falls back to the hex short-id when peerDisplayName is absent', () => {
    const session = fakeSession({
      phase: 'active', startedAt: Date.now(),
      peerOwnerHex: 'abcdef'.repeat(5) + 'ab', peerDisplayName: null,
    });
    render(CallInProgressBar, { props: { session: session as never, onEnd: vi.fn() } });
    expect(screen.getByText('abcdef…')).toBeInTheDocument();
  });

  it('shows a Reconnecting… badge while reconnecting (ZEB-353)', () => {
    const session = fakeSession({ phase: 'active', startedAt: Date.now(), reconnecting: true });
    render(CallInProgressBar, { props: { session: session as never, onEnd: vi.fn() } });
    expect(screen.getByTestId('call-reconnecting')).toBeInTheDocument();
    expect(screen.getByText(/Reconnecting/)).toBeInTheDocument();
  });

  it('hides the Reconnecting… badge when not reconnecting', () => {
    const session = fakeSession({ phase: 'active', startedAt: Date.now(), reconnecting: false });
    render(CallInProgressBar, { props: { session: session as never, onEnd: vi.fn() } });
    expect(screen.queryByTestId('call-reconnecting')).toBeNull();
  });
});
