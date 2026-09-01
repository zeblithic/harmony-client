/**
 * Integration test for the TextFeed subsystem.
 *
 * Tests end-to-end: render messages with grouping, compose + send,
 * thread indicators, thread panel open/close, priority handling, and
 * channel-aware placeholders.
 */
import { render, screen, fireEvent, cleanup } from '@testing-library/svelte';
import { describe, it, expect, vi, afterEach, beforeEach } from 'vitest';
import TextFeed from '../TextFeed.svelte';
import type { Message, Peer } from '../../types';
import type { ThreadMetaEntry } from '../../feed-utils';

// jsdom lacks IntersectionObserver — provide a minimal mock. The
// callback is accepted to match the constructor signature but never
// invoked in tests; observe/unobserve/disconnect are no-ops.
class MockIntersectionObserver {
  constructor(_cb: IntersectionObserverCallback) {}
  observe() {}
  unobserve() {}
  disconnect() {}
}

beforeEach(() => {
  vi.stubGlobal('IntersectionObserver', MockIntersectionObserver);
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

// ── Shared fixtures ────────────────────────────────────────────────

const alice: Peer = { address: 'a1b2c3d4', displayName: 'Alice' };
const bob: Peer = { address: 'e5f6g7h8', displayName: 'Bob' };
const carol: Peer = { address: 'i9j0k1l2', displayName: 'Carol' };

const base = Date.now() - 3600_000;

function msg(overrides: Partial<Message> & Pick<Message, 'id' | 'text'>): Message {
  return {
    sender: alice,
    timestamp: base,
    priority: 'standard',
    ...overrides,
  };
}

const MESSAGES: Message[] = [
  msg({ id: 'm1', sender: alice, text: 'Hello everyone', timestamp: base }),
  msg({ id: 'm2', sender: bob, text: 'Hey Alice!', timestamp: base + 60_000 }),
  msg({ id: 'm3', sender: carol, text: 'quiet ack', timestamp: base + 120_000, priority: 'quiet' }),
  msg({ id: 'm4', sender: alice, text: 'another quiet', timestamp: base + 180_000, priority: 'quiet' }),
  msg({ id: 'm5', sender: bob, text: 'Important update!', timestamp: base + 240_000, priority: 'loud' }),
];

// Thread meta: m2 has 2 replies from Carol and Alice
const THREAD_META: Map<string, ThreadMetaEntry> = new Map([
  ['m2', { count: 2, participants: [carol, alice] }],
]);

const THREAD_ROOT = msg({ id: 'm2', sender: bob, text: 'Hey Alice!', timestamp: base + 60_000 });
const THREAD_REPLIES: Message[] = [
  msg({ id: 'm2-r1', sender: carol, text: 'Thread reply from Carol', timestamp: base + 90_000, replyTo: 'm2' }),
  msg({ id: 'm2-r2', sender: alice, text: 'Thread reply from Alice', timestamp: base + 100_000, replyTo: 'm2' }),
];

// ── Helper ──────────────────────────────────────────────────────────

function renderFeed(overrides: Record<string, unknown> = {}) {
  const callbacks = {
    onSend: vi.fn(),
    onAvatarClick: vi.fn(),
    onThreadOpen: vi.fn(),
    onThreadClose: vi.fn(),
    onThreadSend: vi.fn(),
    onScrollToMessage: vi.fn(),
  };

  const result = render(TextFeed, {
    props: {
      messages: MESSAGES,
      channelName: 'general',
      channelType: 'channel' as const,
      threadMeta: THREAD_META,
      threadRoot: null as Message | null,
      threadReplies: [] as Message[],
      openThreadId: null as string | null,
      pinnedThreadIds: new Set<string>(),
      ...callbacks,
      ...overrides,
    },
  });

  return { ...result, callbacks };
}

describe('TextFeed Integration', () => {
  // ── 1. Rendering ──────────────────────────────────────────────────

  it('renders standard and loud messages as individual items', () => {
    renderFeed();
    expect(screen.getByText('Hello everyone')).toBeTruthy();
    // "Hey Alice!" appears in both FloatingThreadBar and message list
    expect(screen.getAllByText('Hey Alice!').length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText('Important update!')).toBeTruthy();
  });

  it('renders sender display names', () => {
    renderFeed();
    expect(screen.getAllByText('Alice').length).toBeGreaterThan(0);
    expect(screen.getAllByText('Bob').length).toBeGreaterThan(0);
  });

  it('groups consecutive quiet messages into a collapsed summary', () => {
    renderFeed();
    // Quiet messages are collapsed — shows summary instead of individual text
    expect(screen.getByText(/2 quiet messages/)).toBeTruthy();
  });

  it('expands quiet group to show individual messages', async () => {
    renderFeed();
    const toggle = screen.getByText(/2 quiet messages/).closest('button')!;
    await fireEvent.click(toggle);

    expect(screen.getByText('quiet ack')).toBeTruthy();
    expect(screen.getByText('another quiet')).toBeTruthy();
  });

  // ── 2. ComposeBar ─────────────────────────────────────────────────

  it('shows channel-aware placeholder for channels', () => {
    renderFeed({ channelName: 'crypto', channelType: 'channel' });
    expect(screen.getByPlaceholderText('Message #crypto')).toBeTruthy();
  });

  it('shows DM-style placeholder for DMs', () => {
    renderFeed({ channelName: 'Alice', channelType: 'dm' });
    expect(screen.getByPlaceholderText('Message Alice')).toBeTruthy();
  });

  it('fires onSend with text and priority when Enter is pressed', async () => {
    const { callbacks } = renderFeed();
    const textarea = screen.getByPlaceholderText('Message #general');

    await fireEvent.input(textarea, { target: { value: 'test message' } });
    await fireEvent.keyDown(textarea, { key: 'Enter' });

    expect(callbacks.onSend).toHaveBeenCalledWith('test message', 'standard');
  });

  it('fires onSend with quiet priority on Ctrl+Enter', async () => {
    const { callbacks } = renderFeed();
    const textarea = screen.getByPlaceholderText('Message #general');

    await fireEvent.input(textarea, { target: { value: 'quiet msg' } });
    await fireEvent.keyDown(textarea, { key: 'Enter', ctrlKey: true });

    expect(callbacks.onSend).toHaveBeenCalledWith('quiet msg', 'quiet');
  });

  it('does not send empty messages', async () => {
    const { callbacks } = renderFeed();
    const textarea = screen.getByPlaceholderText('Message #general');

    await fireEvent.keyDown(textarea, { key: 'Enter' });

    expect(callbacks.onSend).not.toHaveBeenCalled();
  });

  // ── 3. Priority controls ──────────────────────────────────────────

  it('allows selecting priority before sending', async () => {
    const { callbacks } = renderFeed();
    const loudBtn = screen.getByTitle('loud');
    await fireEvent.click(loudBtn);

    const textarea = screen.getByPlaceholderText('Message #general');
    await fireEvent.input(textarea, { target: { value: 'loud msg' } });
    await fireEvent.keyDown(textarea, { key: 'Enter' });

    expect(callbacks.onSend).toHaveBeenCalledWith('loud msg', 'loud');
  });

  // ── 4. Thread indicators ──────────────────────────────────────────

  it('shows thread indicator under messages with replies', () => {
    renderFeed();
    // Thread indicator shows "2 replies" for m2
    expect(screen.getByText(/2 replies/)).toBeTruthy();
  });

  it('shows participant names in thread indicator', () => {
    renderFeed();
    // Thread indicator has specific class; quiet group also mentions "Carol, Alice"
    const indicator = screen.getByText(/2 replies · Carol, Alice/);
    expect(indicator).toBeTruthy();
  });

  it('fires onThreadOpen when thread indicator is clicked', async () => {
    const { callbacks } = renderFeed();
    const indicator = screen.getByText(/2 replies/);
    await fireEvent.click(indicator.closest('button')!);

    expect(callbacks.onThreadOpen).toHaveBeenCalledWith('m2');
  });

  // ── 5. Thread panel ───────────────────────────────────────────────

  it('shows thread panel when openThreadId and threadRoot are set', () => {
    renderFeed({
      openThreadId: 'm2',
      threadRoot: THREAD_ROOT,
      threadReplies: THREAD_REPLIES,
    });

    // Thread panel renders root + replies
    expect(screen.getByText('Thread reply from Carol')).toBeTruthy();
    expect(screen.getByText('Thread reply from Alice')).toBeTruthy();
    // Thread header
    expect(screen.getByText('Thread')).toBeTruthy();
  });

  it('fires onThreadClose when close button is clicked', async () => {
    const { callbacks } = renderFeed({
      openThreadId: 'm2',
      threadRoot: THREAD_ROOT,
      threadReplies: THREAD_REPLIES,
    });

    const closeBtn = screen.getByLabelText('Close thread');
    await fireEvent.click(closeBtn);

    expect(callbacks.onThreadClose).toHaveBeenCalled();
  });

  it('fires onThreadClose on Escape key', async () => {
    const { callbacks } = renderFeed({
      openThreadId: 'm2',
      threadRoot: THREAD_ROOT,
      threadReplies: THREAD_REPLIES,
    });

    await fireEvent.keyDown(window, { key: 'Escape' });

    expect(callbacks.onThreadClose).toHaveBeenCalled();
  });

  it('shows drag handle between main and thread sections', () => {
    renderFeed({
      openThreadId: 'm2',
      threadRoot: THREAD_ROOT,
      threadReplies: THREAD_REPLIES,
    });

    expect(screen.getByLabelText('Resize thread panel')).toBeTruthy();
  });

  // ── 8. DM header Call button (ZEB-352) ────────────────────────────

  it('renders a "Start call" button when channelType is dm', () => {
    renderFeed({ channelType: 'dm', channelId: 'space-abc', channelName: 'Alice' });
    expect(screen.getByRole('button', { name: 'Start call' })).toBeTruthy();
  });

  it('does not render a "Start call" button when channelType is channel', () => {
    renderFeed({ channelType: 'channel', channelId: 'chan-1', channelName: 'general' });
    expect(screen.queryByRole('button', { name: 'Start call' })).toBeNull();
  });

  it('calls onStartCall with channelId when the Call button is clicked', async () => {
    const onStartCall = vi.fn();
    renderFeed({ channelType: 'dm', channelId: 'space-xyz', onStartCall });
    await fireEvent.click(screen.getByRole('button', { name: 'Start call' }));
    expect(onStartCall).toHaveBeenCalledWith('space-xyz');
  });

  // ── 9. Call-event system lines (ZEB-357) ──────────────────────────

  it('renders a call-event message as a system line, not a text bubble', () => {
    const callMsg = msg({
      id: 'call-1',
      sender: bob,
      text: 'Missed call',
      timestamp: base + 400_000,
      callEvent: { v: 1, callId: 'ab'.repeat(16), outcome: 'no_answer' },
    });
    renderFeed({ channelType: 'dm', messages: [...MESSAGES, callMsg] });
    const line = screen.getByTestId('call-event-line');
    expect(line.textContent).toContain('Missed call');
    // The system line replaces the bubble — the fallback text must not ALSO
    // render as a normal message body.
    expect(screen.queryAllByText('Missed call')).toHaveLength(1);
  });

  it('a self-authored call-event renders the author-side label', () => {
    const callMsg = msg({
      id: 'call-2',
      sender: { address: 'self', displayName: 'You' },
      text: 'Call — no answer',
      timestamp: base + 500_000,
      callEvent: { v: 1, callId: 'cd'.repeat(16), outcome: 'no_answer' },
    });
    renderFeed({ channelType: 'dm', messages: [callMsg] });
    expect(screen.getByTestId('call-event-line').textContent).toContain('Call — no answer');
  });
});

// ── 10. DM author resolution (ZEB-839) ──────────────────────────────
//
// `message-service` used to bake `from.slice(0, 8)` into a DM sender's
// displayName once, at message-arrival time, so a DM bubble showed raw hex
// forever — even when the peer's name was already known in the community view.
// DM senders now carry the RAW owner_id and an empty name, and the feed runs
// the shared ladder (friend nickname ► profile card ► wire name ► short hex)
// on every render. Mirrors ChannelMessageFeed's ZEB-432 author tests.
describe('TextFeed DM author resolution (ZEB-839)', () => {
  const PEER = '2e9a2151'.repeat(4); // 32-char lowercase owner_id hex

  /** A DM as message-service now produces it: raw owner_id, no baked name. */
  const dmFromPeer = msg({
    id: 'dm-1',
    sender: { address: PEER, displayName: '' },
    text: 'hey there',
    timestamp: base,
  });

  function renderDm(overrides: Record<string, unknown> = {}) {
    return renderFeed({
      channelType: 'dm',
      channelName: 'DM',
      messages: [dmFromPeer],
      threadMeta: new Map(),
      ...overrides,
    });
  }

  it('renders the profile-card name for a DM author', () => {
    renderDm({
      resolveCard: (id: string) =>
        id === PEER ? { displayName: 'Alice', statusText: '' } : undefined,
    });
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.queryByText(PEER.slice(0, 8))).toBeNull();
  });

  it('prefers the local friend nickname over the profile-card name', () => {
    renderDm({
      resolveCard: (id: string) =>
        id === PEER ? { displayName: 'Alice', statusText: '' } : undefined,
      resolveNickname: (id: string) => (id === PEER ? 'Ali' : undefined),
    });
    expect(screen.getByText('Ali')).toBeTruthy();
    expect(screen.queryByText('Alice')).toBeNull();
  });

  it('falls back to truncated hex only when nothing resolves', () => {
    renderDm({ resolveCard: () => undefined, resolveNickname: () => undefined });
    expect(screen.getByText(PEER.slice(0, 8))).toBeTruthy();
  });

  it('falls back to truncated hex when the card resolves to a blank name', () => {
    renderDm({
      resolveCard: (id: string) =>
        id === PEER ? { displayName: '   ', statusText: '' } : undefined,
    });
    expect(screen.getByText(PEER.slice(0, 8))).toBeTruthy();
  });

  // The regression this ticket exists for: resolution must happen at RENDER
  // time, so a card arriving after the message repaints the label. With the
  // old baked name the bubble stayed hex no matter what arrived later.
  it('repaints the author label when the peer card arrives later', async () => {
    const { rerender } = renderDm({ resolveCard: () => undefined });
    expect(screen.getByText(PEER.slice(0, 8))).toBeTruthy();

    await rerender({
      resolveCard: (id: string) =>
        id === PEER ? { displayName: 'Alice', statusText: '' } : undefined,
    });

    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.queryByText(PEER.slice(0, 8))).toBeNull();
  });

  it('leaves a self-authored DM on the locally-known label', () => {
    renderDm({
      messages: [
        msg({
          id: 'dm-self',
          sender: { address: 'self', displayName: 'Jake Englund' },
          text: 'mine',
          timestamp: base,
        }),
      ],
      resolveCard: () => ({ displayName: 'should-not-be-used', statusText: '' }),
      resolveNickname: () => 'nope',
    });
    expect(screen.getByText('Jake Englund')).toBeTruthy();
  });

  // A channel message carries a self-declared `senderName` on the wire; it sits
  // BELOW the card so a verified card name wins, but above the hex fallback.
  it('uses the wire senderName when no nickname or card resolves', () => {
    renderDm({
      messages: [
        msg({
          id: 'wire-1',
          sender: { address: PEER, displayName: 'WireName' },
          text: 'from a channel',
          timestamp: base,
        }),
      ],
    });
    expect(screen.getByText('WireName')).toBeTruthy();
  });

  it('a verified card name outranks the wire senderName', () => {
    renderDm({
      messages: [
        msg({
          id: 'wire-2',
          sender: { address: PEER, displayName: 'WireName' },
          text: 'from a channel',
          timestamp: base,
        }),
      ],
      resolveCard: (id: string) =>
        id === PEER ? { displayName: 'Carded', statusText: '' } : undefined,
    });
    expect(screen.getByText('Carded')).toBeTruthy();
    expect(screen.queryByText('WireName')).toBeNull();
  });
});
