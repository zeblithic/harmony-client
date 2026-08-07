import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import ChannelMessageFeed from '../ChannelMessageFeed.svelte';
import { ChannelMessageService } from '../../channel-message-service';
import type { TauriAdapter } from '../../zenoh-service';
import { VotingAdapter } from '../../voting-adapter';
import type { PollMeta } from '../../types/voting';

// vi.mock is hoisted; vi.hoisted makes the spies available at factory-call
// time (repo pattern — see WelcomeModal.test.ts).
const { openMock, saveMock } = vi.hoisted(() => ({ openMock: vi.fn(), saveMock: vi.fn() }));
vi.mock('@tauri-apps/plugin-dialog', () => ({
  open: openMock,
  save: saveMock,
}));

// ZEB-541: stub normalizeEmoji so the custom-emoji pick test doesn't need a real
// canvas/decode — we only assert the normalize→ingest→react wiring.
const { normalizeEmojiMock } = vi.hoisted(() => ({
  normalizeEmojiMock: vi.fn().mockResolvedValue(new Uint8Array([0x89, 0x50, 0x4e, 0x47])),
}));
vi.mock('../../emoji-normalize', () => ({
  EMOJI_EDGE: 128,
  normalizeEmoji: normalizeEmojiMock,
}));

function makeAdapter(): TauriAdapter & { listeners: Map<string, Function> } {
  const listeners = new Map<string, Function>();
  return {
    listeners,
    invoke: vi.fn(),
    listen: vi.fn(async (event: string, handler: Function) => {
      listeners.set(event, handler);
      return () => listeners.delete(event);
    }),
  } as any;
}

async function setup(propOverrides: Record<string, unknown> = {}) {
  const adapter = makeAdapter();
  (adapter.invoke as any).mockImplementation((cmd: string) => {
    if (cmd === 'list_channel_messages') return Promise.resolve([]);
    if (cmd === 'request_channel_backfill') return Promise.resolve(undefined);
    if (cmd === 'post_channel_message') return Promise.resolve('mid' + 'a'.repeat(29));
    return Promise.resolve(undefined);
  });
  const service = new ChannelMessageService();
  await service.connectAdapter(adapter);
  const props = {
    communityId: 'aa'.repeat(16),
    channelId: 'bb'.repeat(16),
    channelName: 'general',
    channelMessageService: service,
    ownAddress: 'cc'.repeat(20),
    myPower: 50,
    enableVirtualization: false, // tests render synchronously
    ...propOverrides,
  };
  const renderResult = render(ChannelMessageFeed, { props });
  return { adapter, service, props, ...renderResult };
}

describe('ChannelMessageFeed', () => {
  beforeEach(() => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  // ZEB-776: a channel still converging after a fresh join shows a "still
  // syncing" banner so an empty feed doesn't read as broken.
  it('shows the syncing banner when channelSyncing is true', async () => {
    const { queryByTestId } = await setup({ channelSyncing: true });
    await waitFor(() => {
      expect(queryByTestId('channel-syncing-banner')).not.toBeNull();
    });
  });

  it('hides the syncing banner when channelSyncing is false', async () => {
    const { queryByTestId } = await setup({ channelSyncing: false });
    expect(queryByTestId('channel-syncing-banner')).toBeNull();
  });

  // ZEB-776: the banner is empty-feed-gated — once messages arrive (live, as the
  // channel converges) it clears even while channelSyncing is still true, so it
  // never sits contradictorily above a populated feed.
  it('clears the syncing banner once a message arrives, even while syncing', async () => {
    const { adapter, queryByTestId } = await setup({ channelSyncing: true });
    expect(queryByTestId('channel-syncing-banner')).not.toBeNull();
    const handler = adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'm1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'cc'.repeat(20),
          at: { wallMs: 1000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode('hi')),
        },
      },
    });
    await waitFor(() => {
      expect(queryByTestId('channel-syncing-banner')).toBeNull();
    });
  });

  it('mounts and calls listMessages on mount', async () => {
    const { adapter } = await setup();
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('list_channel_messages', {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        since: undefined,
        limit: 100,
      });
    });
  });

  it('renders the channel header with #channelName', async () => {
    const { container } = await setup({ channelName: 'announcements' });
    const header = container.querySelector('.channel-header');
    expect(header?.textContent).toContain('#');
    expect(header?.textContent).toContain('announcements');
    // Verify the .name class hook exists for Task 7's CommunityView test:
    expect(container.querySelector('.channel-header .name')?.textContent?.trim()).toBe('announcements');
  });

  it('renders a message when one arrives via channel-message-received', async () => {
    const { adapter, container } = await setup();
    const handler = adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'm1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'cc'.repeat(20),
          at: { wallMs: 1000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode('hello world')),
        },
      },
    });
    await waitFor(() => {
      const msgs = container.querySelectorAll('.channel-message');
      expect(msgs.length).toBe(1);
      expect(msgs[0].textContent).toContain('hello world');
    });
  });

  it('compose Enter posts via channelMessageService.postMessage', async () => {
    const { adapter, container } = await setup();
    const el = container.querySelector('[role="textbox"]') as HTMLElement;
    el.textContent = 'first message';
    await fireEvent.keyDown(el, { key: 'Enter' });
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('post_channel_message', expect.objectContaining({
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        body: Array.from(new TextEncoder().encode('first message')),
        replyTo: undefined,
      }));
    });
    // Compose box clears on successful send.
    await waitFor(() => expect(el.textContent).toBe(''));
  });

  it('Shift+Enter inserts a newline (does NOT send)', async () => {
    const { adapter, container } = await setup();
    const el = container.querySelector('[role="textbox"]') as HTMLElement;
    el.textContent = 'line one';
    await fireEvent.keyDown(el, { key: 'Enter', shiftKey: true });
    expect(adapter.invoke).not.toHaveBeenCalledWith(
      'post_channel_message',
      expect.anything(),
    );
    // Content retained (the browser would insert the newline; we just verify we
    // didn't send).
    expect(el.textContent).toBe('line one');
  });

  it('does not post empty/whitespace-only messages', async () => {
    const { adapter, container } = await setup();
    const el = container.querySelector('[role="textbox"]') as HTMLElement;
    el.textContent = '   ';
    await fireEvent.keyDown(el, { key: 'Enter' });
    expect(adapter.invoke).not.toHaveBeenCalledWith(
      'post_channel_message',
      expect.anything(),
    );
  });

  it('scroll-to-top stable for 250ms fires requestBackfill', async () => {
    const { adapter, container } = await setup();
    const scroll = container.querySelector('.messages-scroll') as HTMLDivElement;
    Object.defineProperty(scroll, 'scrollTop', { value: 10, writable: true, configurable: true });
    await fireEvent.scroll(scroll);

    // Before 250ms — no backfill yet.
    vi.advanceTimersByTime(200);
    expect(adapter.invoke).not.toHaveBeenCalledWith(
      'request_channel_backfill',
      expect.anything(),
    );

    // After 250ms total — backfill fires.
    vi.advanceTimersByTime(60);
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('request_channel_backfill', {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        since: undefined,
      });
    });
  });

  it('scroll-to-top single-in-flight gate: second trigger during in-flight is no-op', async () => {
    const { adapter, container } = await setup();
    let resolveBackfill: () => void = () => {};
    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      if (cmd === 'request_channel_backfill') return new Promise<void>((r) => { resolveBackfill = () => r(); });
      return Promise.resolve(undefined);
    });

    const scroll = container.querySelector('.messages-scroll') as HTMLDivElement;
    Object.defineProperty(scroll, 'scrollTop', { value: 10, writable: true, configurable: true });
    await fireEvent.scroll(scroll);
    vi.advanceTimersByTime(260);

    await waitFor(() => {
      expect((adapter.invoke as any).mock.calls.filter((c: any[]) => c[0] === 'request_channel_backfill').length).toBe(1);
    });

    // Second scroll-to-top stable: no-op (gate held by ChannelMessageService).
    await fireEvent.scroll(scroll);
    vi.advanceTimersByTime(260);
    expect((adapter.invoke as any).mock.calls.filter((c: any[]) => c[0] === 'request_channel_backfill').length).toBe(1);

    resolveBackfill();
  });

  it('shows skeleton while backfill in-flight; hides on terminal progress', async () => {
    const { adapter, container } = await setup();
    let resolveBackfill: () => void = () => {};
    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      if (cmd === 'request_channel_backfill') return new Promise<void>((r) => { resolveBackfill = () => r(); });
      return Promise.resolve(undefined);
    });

    const scroll = container.querySelector('.messages-scroll') as HTMLDivElement;
    Object.defineProperty(scroll, 'scrollTop', { value: 10, writable: true, configurable: true });
    await fireEvent.scroll(scroll);
    vi.advanceTimersByTime(260);

    await waitFor(() => {
      expect(container.querySelector('.backfill-skeleton')).toBeTruthy();
    });

    // Terminal progress event releases skeleton.
    const progressHandler = adapter.listeners.get('channel-backfill-progress')!;
    progressHandler({ payload: { communityId: 'aa'.repeat(16), channelId: 'bb'.repeat(16), fetched: 5, totalEstimate: 5 } });
    resolveBackfill();
    await waitFor(() => {
      expect(container.querySelector('.backfill-skeleton')).toBeNull();
    });
  });

  it('switching channelId resubscribes + re-lists', async () => {
    const { adapter, props, rerender } = await setup();
    expect(adapter.invoke).toHaveBeenCalledWith('list_channel_messages', expect.objectContaining({ channelId: 'bb'.repeat(16) }));

    await rerender({ ...props, channelId: 'dd'.repeat(16) });
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('list_channel_messages', expect.objectContaining({ channelId: 'dd'.repeat(16) }));
    });
  });

  // ── ZEB-291 Tasks 21-23: Phase 1.5 chat-native poll dispatch ────────
  //
  // The Rust IPC boundary tags `ChannelMessageDto.kind = 'poll'` and
  // `pollId = hex` when the body matches the convention
  // (`0x00` magic byte + 64 ASCII hex chars). The feed then renders
  // `<PollMessage>` inline when a matching `PollMeta` is in the
  // pre-fetched cache (from `listActivePolls`). Tests cover:
  //
  //   1. Happy path: poll-kind message + matching meta → PollMessage renders.
  //   2. Regression: text-kind message still renders as text.
  //   3. Race: poll-kind message but no matching meta → "Loading poll…".
  //
  // PollMessage's $effect calls `adapter.getPoll(pollId)` on mount;
  // we stub it to a never-resolving promise so the inner card stays
  // in its initial "Loading poll…" state and the OUTER feed-level
  // cache-miss placeholder vs OUTER cache-hit (PollMessage mount) are
  // distinguishable by component-presence rather than text content.

  const POLL_ID_HEX = 'ab'.repeat(32); // 64 hex chars = 32-byte poll_id.
  const POLL_ID_BYTES = new Array(32).fill(0xab);
  const COMMUNITY_ID_BYTES = new Array(16).fill(0xaa);
  const CHANNEL_ID_BYTES = new Array(16).fill(0xbb);
  const CREATOR_BYTES = new Array(16).fill(0xcc);

  function makePollMeta(overrides: Partial<PollMeta> = {}): PollMeta {
    return {
      poll_id: POLL_ID_BYTES,
      community_id: COMMUNITY_ID_BYTES,
      creator: CREATOR_BYTES,
      tier: 1,
      eligibility: { mp: 0 },
      lifecycle: 'Open',
      created_at: { w: 100, l: 0, d: 'dev' },
      opens_at: { w: 100, l: 0, d: 'dev' },
      closes_at: { w: 1000, l: 0, d: 'dev' },
      channel_id: CHANNEL_ID_BYTES,
      ...overrides,
    };
  }

  function makeVotingAdapter(
    polls: PollMeta[],
  ): { adapter: VotingAdapter; listActivePollsMock: ReturnType<typeof vi.fn> } {
    const adapter = new VotingAdapter();
    const listActivePollsMock = vi
      .fn<(communityId: string) => Promise<PollMeta[]>>()
      .mockResolvedValue(polls);
    // Patch directly so we don't need a real Tauri adapter wired in.
    adapter.listActivePolls = listActivePollsMock;
    // PollMessage's $effect calls adapter.getPoll on mount. Keep it
    // pending so we can distinguish "PollMessage mounted, loading"
    // from the feed-level "Loading poll…" miss placeholder.
    // VotingAdapter.getPoll signature takes a hex string; we just
    // keep the promise pending so we can distinguish the OUTER feed
    // miss-placeholder vs the PollMessage inner loading state.
    adapter.getPoll = vi
      .fn<VotingAdapter['getPoll']>()
      .mockImplementation(() => new Promise(() => {}));
    return { adapter, listActivePollsMock };
  }

  it('poll-kind message renders <PollMessage> when meta is cached', async () => {
    const { adapter: votingAdapter, listActivePollsMock } = makeVotingAdapter([makePollMeta()]);
    const { adapter, container } = await setup({ votingAdapter });

    // Pre-fetch was invoked with the feed's communityId.
    await waitFor(() => {
      expect(listActivePollsMock).toHaveBeenCalledWith('aa'.repeat(16));
    });

    // Inject a poll-kind message via the live event (body bytes
    // unused; the kind/pollId discriminator is the dispatch key).
    const handler = adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'pollmsg1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'cc'.repeat(20),
          at: { wallMs: 2000, logical: 0, deviceId: 'd' },
          body: [0x00, ...Array.from(POLL_ID_HEX).map((c) => c.charCodeAt(0))],
          kind: 'poll',
          pollId: POLL_ID_HEX,
        },
      },
    });

    await waitFor(() => {
      // PollMessage renders its `.poll-message` article wrapper.
      const card = container.querySelector('.poll-message');
      expect(card).toBeTruthy();
    });
    // The text-body element should NOT be rendered for poll-kind messages.
    const msg = container.querySelector('.channel-message');
    expect(msg?.querySelector('p.body')).toBeNull();
  });

  it('text-kind message still renders as text (regression)', async () => {
    const { adapter: votingAdapter } = makeVotingAdapter([makePollMeta()]);
    const { adapter, container } = await setup({ votingAdapter });

    const handler = adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'textmsg1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'cc'.repeat(20),
          at: { wallMs: 3000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode('plain text message')),
          // kind omitted = text (default)
        },
      },
    });

    await waitFor(() => {
      const body = container.querySelector('.channel-message p.body');
      expect(body?.textContent).toContain('plain text message');
    });
    expect(container.querySelector('.poll-message')).toBeNull();
  });

  // ── ZEB-588: @-mention rendering ──
  it('renders a <@id> body token as a resolved styled mention', async () => {
    const MID = 'a'.repeat(32);
    const { adapter, container } = await setup({
      resolveCard: (id: string) => (id === MID ? { displayName: 'Jake', statusText: '' } : undefined),
    });
    adapter.listeners.get('channel-message-received')!({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'mentionmsg1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'cc'.repeat(20),
          at: { wallMs: 4000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode(`hi <@${MID}>`)),
          mentions: [MID],
        },
      },
    });
    await waitFor(() => {
      const mention = container.querySelector('[data-testid="mention"]');
      expect(mention?.textContent).toBe('@Jake');
    });
    // not the viewer → no self emphasis, no row highlight
    expect(container.querySelector('[data-testid="mention"]')!.classList.contains('self')).toBe(false);
    expect(container.querySelector('.channel-message')!.classList.contains('mentions-me')).toBe(false);
    // surrounding text is preserved
    expect(container.querySelector('.channel-message p.body')?.textContent).toContain('hi @Jake');
  });

  it('highlights the row and the mention when the viewer is mentioned', async () => {
    const ME = 'a'.repeat(32);
    const { adapter, container } = await setup({
      ownAddress: ME,
      resolveCard: (id: string) => (id === ME ? { displayName: 'Me', statusText: '' } : undefined),
    });
    adapter.listeners.get('channel-message-received')!({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'mentionmsg2',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'cc'.repeat(20),
          at: { wallMs: 5000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode(`yo <@${ME}>`)),
          mentions: [ME],
        },
      },
    });
    await waitFor(() => {
      expect(container.querySelector('[data-testid="mention"]')?.textContent).toBe('@Me');
    });
    expect(container.querySelector('[data-testid="mention"]')!.classList.contains('self')).toBe(true);
    expect(container.querySelector('.channel-message')!.classList.contains('mentions-me')).toBe(true);
  });

  it('compose: @-autocomplete pick → Enter sends a body token + mentions array', async () => {
    // ZEB-594: drive the real trigger→autocomplete→pick flow via a jsdom
    // Selection, then Enter to send. The pick splices an atomic chip node whose
    // ownerId serializes to a <@id> token.
    const ID = 'a'.repeat(32);
    const { adapter, container } = await setup({ mentionCandidates: [{ ownerId: ID, label: 'Jake' }] });
    const el = container.querySelector('[role="textbox"]') as HTMLElement;
    // type "@Ja" with the caret at the end
    el.textContent = '@Ja';
    const range = document.createRange();
    range.setStart(el.firstChild!, 3);
    range.collapse(true);
    const sel = window.getSelection()!;
    sel.removeAllRanges();
    sel.addRange(range);
    await fireEvent.input(el);
    // the autocomplete opens
    await waitFor(() =>
      expect(container.querySelector('[data-testid="mention-autocomplete"]')).toBeTruthy(),
    );
    // pick "Jake" (mousedown, so the input keeps focus/selection)
    await fireEvent.mouseDown(container.querySelector('[data-testid="mention-option"] button')!);
    await waitFor(() => expect(el.querySelector('.mention-chip')).toBeTruthy());
    // send with Enter (dropdown is closed now → Enter sends, not picks)
    await fireEvent.keyDown(el, { key: 'Enter' });
    await waitFor(() => {
      const call = (adapter.invoke as ReturnType<typeof vi.fn>).mock.calls.find(
        (c: unknown[]) => c[0] === 'post_channel_message',
      );
      expect(call).toBeTruthy();
      const body = new TextDecoder().decode(new Uint8Array((call![1] as { body: number[] }).body));
      expect(body).toContain(`<@${ID}>`);
      expect((call![1] as { mentions?: string[] }).mentions).toEqual([ID]);
    });
  });

  it('compose: a plain message (no picks) sends no mentions', async () => {
    const { adapter, container } = await setup({
      mentionCandidates: [{ ownerId: 'a'.repeat(32), label: 'Jake' }],
    });
    const el = container.querySelector('[role="textbox"]') as HTMLElement;
    el.textContent = 'hello world';
    await fireEvent.keyDown(el, { key: 'Enter' });
    await waitFor(() => {
      const call = (adapter.invoke as ReturnType<typeof vi.fn>).mock.calls.find(
        (c: unknown[]) => c[0] === 'post_channel_message',
      );
      expect(call).toBeTruthy();
      expect(new TextDecoder().decode(new Uint8Array((call![1] as { body: number[] }).body))).toBe(
        'hello world',
      );
      expect((call![1] as { mentions?: string[] }).mentions).toBeUndefined();
    });
  });

  it('poll-kind message with no matching meta shows "Loading poll…" placeholder', async () => {
    // listActivePolls returns EMPTY → the incoming poll_id has no cache entry.
    const { adapter: votingAdapter, listActivePollsMock } = makeVotingAdapter([]);
    const { adapter, container } = await setup({ votingAdapter });

    await waitFor(() => {
      expect(listActivePollsMock).toHaveBeenCalled();
    });

    const handler = adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'pollmsg2',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'cc'.repeat(20),
          at: { wallMs: 4000, logical: 0, deviceId: 'd' },
          body: [0x00, ...Array.from(POLL_ID_HEX).map((c) => c.charCodeAt(0))],
          kind: 'poll',
          pollId: POLL_ID_HEX,
        },
      },
    });

    await waitFor(() => {
      // The feed-level placeholder appears (NOT the PollMessage card,
      // which would only render with a cache hit).
      const placeholder = container.querySelector('.channel-message .poll-loading');
      expect(placeholder).toBeTruthy();
      expect(placeholder?.textContent).toContain('Loading poll');
    });
    expect(container.querySelector('.poll-message')).toBeNull();
  });

  it('shows inline error when post fails', async () => {
    const { adapter, container } = await setup();
    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      if (cmd === 'post_channel_message') return Promise.reject(new Error('no engine for ...'));
      return Promise.resolve(undefined);
    });
    const el = container.querySelector('[role="textbox"]') as HTMLElement;
    el.textContent = 'will fail';
    await fireEvent.keyDown(el, { key: 'Enter' });
    await waitFor(() => {
      expect(container.querySelector('.compose-error')?.textContent).toMatch(/no engine/);
    });
    // Compose retains text on failure so user can retry.
    expect(el.textContent).toBe('will fail');
  });

  it('renders the Commons fork-divider band with the real carried count', async () => {
    const preFork1 = {
      messageId: 'pf1', communityId: 'aa'.repeat(16), channelId: 'bb'.repeat(16),
      author: 'ee'.repeat(20), at: { wallMs: 400, logical: 0, deviceId: 'd' },
      body: Array.from(new TextEncoder().encode('old 1')),
    };
    const preFork2 = {
      ...preFork1, messageId: 'pf2', at: { wallMs: 500, logical: 0, deviceId: 'd' },
      body: Array.from(new TextEncoder().encode('old 2')),
    };
    const { adapter, container } = await setup({
      snapshotMessages: [preFork1, preFork2],
      originalCommunityName: 'OldCommunity',
      forkedAtMs: 1000,
    });
    // A live post-fork message (wallMs after the snapshot) creates the boundary.
    const handler = adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16), channelId: 'bb'.repeat(16),
        message: {
          messageId: 'live1', communityId: 'aa'.repeat(16), channelId: 'bb'.repeat(16),
          author: 'cc'.repeat(20), at: { wallMs: 2000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode('new message')),
        },
      },
    });
    let divider: Element | null = null;
    await waitFor(() => {
      divider = container.querySelector('.fork-divider');
      expect(divider).toBeTruthy();
    });
    expect(divider!.getAttribute('role')).toBe('separator');
    expect(divider!.getAttribute('aria-label')).toBe('Forked from OldCommunity');
    expect(divider!.textContent).toContain('Forked from OldCommunity');
    expect(divider!.textContent).toContain('⑂');
    // Real carried count = snapshotMessages.length = 2.
    expect(divider!.textContent).toContain('2 messages carried');
  });
});

describe('ChannelMessageFeed author display-name resolution (ZEB-432)', () => {
  beforeEach(() => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  const AUTHOR = 'ee'.repeat(20);

  function deliver(adapter: { listeners: Map<string, Function> }) {
    adapter.listeners.get('channel-message-received')!({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'm1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: AUTHOR,
          at: { wallMs: 1000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode('hello')),
        },
      },
    });
  }

  it('renders the local friend nickname for an author OVER the profile-card name', async () => {
    const { adapter, container } = await setup({
      resolveCard: (id: string) =>
        id === AUTHOR ? { displayName: 'ZEBbot', statusText: '' } : undefined,
      resolveNickname: (id: string) => (id === AUTHOR ? 'Jake-nick' : undefined),
    });
    deliver(adapter);
    await waitFor(() => {
      const author = container.querySelector('.channel-message .author');
      expect(author?.textContent).toContain('Jake-nick');
      expect(author?.textContent).not.toContain('ZEBbot');
    });
  });

  it('falls back to the profile-card name when the author has no nickname', async () => {
    const { adapter, container } = await setup({
      resolveCard: (id: string) =>
        id === AUTHOR ? { displayName: 'ZEBbot', statusText: '' } : undefined,
      resolveNickname: () => undefined,
    });
    deliver(adapter);
    await waitFor(() => {
      const author = container.querySelector('.channel-message .author');
      expect(author?.textContent).toContain('ZEBbot');
    });
  });

  it('falls back to truncated hex when neither nickname nor card resolves', async () => {
    const { adapter, container } = await setup({
      resolveCard: () => undefined,
      resolveNickname: () => undefined,
    });
    deliver(adapter);
    await waitFor(() => {
      const author = container.querySelector('.channel-message .author');
      expect(author?.textContent).toContain(AUTHOR.slice(0, 8));
    });
  });

  it('the owner-card popover carries the SIGNED card name, not the nickname (PR #240 review)', async () => {
    const onOpenCard = vi.fn();
    const { adapter, container } = await setup({
      onOpenCard,
      resolveCard: (id: string) =>
        id === AUTHOR ? { displayName: 'ZEBbot', statusText: 's' } : undefined,
      resolveNickname: (id: string) => (id === AUTHOR ? 'Jake-nick' : undefined),
    });
    deliver(adapter);
    let btn: Element | null = null;
    await waitFor(() => {
      btn = container.querySelector('.channel-message .author.author-btn');
      expect(btn).toBeTruthy();
    });
    // Inline author label shows the nickname…
    expect(btn!.textContent).toContain('Jake-nick');
    await fireEvent.click(btn!);
    // …but the identity drill-down popover gets the signed card name.
    expect(onOpenCard).toHaveBeenCalled();
    expect(onOpenCard.mock.calls[0][0].displayName).toBe('ZEBbot');
  });

  it('renders MessageAttachments for a message carrying attachments', async () => {
    const { adapter, container } = await setup();
    const handler = adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'm1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'cc'.repeat(20),
          at: { wallMs: 1000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode('see attached')),
          attachments: [{ cid: 'k1', mime: 'text/plain', name: 'ci.log', size: 1234, encrypted: true }],
        },
      },
    });
    await waitFor(() => {
      expect(container.querySelector('.attachment-chip')).not.toBeNull();
      expect(container.textContent).toContain('ci.log');
    });
  });

  function withIngest(adapter: any, opts: { reject?: Error } = {}) {
    let n = 0;
    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      if (cmd === 'request_channel_backfill') return Promise.resolve(undefined);
      if (cmd === 'ingest_channel_artifact') {
        if (opts.reject) return Promise.reject(opts.reject);
        return Promise.resolve({ cid: 'cid' + n++, mime: 'text/plain', name: 'f.txt', size: 5, encrypted: true });
      }
      if (cmd === 'post_channel_message') return Promise.resolve('mid' + 'a'.repeat(29));
      return Promise.resolve(undefined);
    });
  }

  it('attach button ingests picked files into pending chips', async () => {
    openMock.mockResolvedValue(['/tmp/a.txt', '/tmp/b.txt']);
    const { adapter, container } = await setup();
    withIngest(adapter);
    await fireEvent.click(container.querySelector('.attach-btn')!);
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('ingest_channel_artifact', expect.objectContaining({ sourcePath: '/tmp/a.txt' }));
      expect(container.querySelectorAll('.pending-chip').length).toBe(2);
    });
  });

  it('dedupes a pending attachment with a duplicate cid', async () => {
    openMock.mockResolvedValue(['/tmp/a.txt', '/tmp/a.txt']);
    const { adapter, container } = await setup();
    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      if (cmd === 'request_channel_backfill') return Promise.resolve(undefined);
      if (cmd === 'ingest_channel_artifact')
        return Promise.resolve({ cid: 'samecid', mime: 'text/plain', name: 'f.txt', size: 5, encrypted: true });
      if (cmd === 'post_channel_message') return Promise.resolve('mid' + 'a'.repeat(29));
      return Promise.resolve(undefined);
    });
    await fireEvent.click(container.querySelector('.attach-btn')!);
    await waitFor(() => expect(container.querySelectorAll('.pending-chip').length).toBe(1));
  });

  it('removing a pending attachment drops its chip', async () => {
    openMock.mockResolvedValue('/tmp/a.txt');
    const { adapter, container } = await setup();
    withIngest(adapter);
    await fireEvent.click(container.querySelector('.attach-btn')!);
    await waitFor(() => expect(container.querySelectorAll('.pending-chip').length).toBe(1));
    await fireEvent.click(container.querySelector('.pending-remove')!);
    await waitFor(() => expect(container.querySelectorAll('.pending-chip').length).toBe(0));
  });

  it('send includes pendingAttachments and clears them', async () => {
    openMock.mockResolvedValue('/tmp/a.txt');
    const { adapter, container } = await setup();
    withIngest(adapter);
    await fireEvent.click(container.querySelector('.attach-btn')!);
    await waitFor(() => expect(container.querySelectorAll('.pending-chip').length).toBe(1));
    const el = container.querySelector('[role="textbox"]') as HTMLElement;
    el.textContent = 'here it is';
    await fireEvent.keyDown(el, { key: 'Enter' });
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('post_channel_message', expect.objectContaining({
        body: Array.from(new TextEncoder().encode('here it is')),
        attachments: [{ cid: 'cid0', mime: 'text/plain', name: 'f.txt', size: 5, encrypted: true }],
      }));
    });
    expect(container.querySelectorAll('.pending-chip').length).toBe(0);
  });

  it('allows sending with empty body when an attachment is pending', async () => {
    openMock.mockResolvedValue('/tmp/a.txt');
    const { adapter, container } = await setup();
    withIngest(adapter);
    await fireEvent.click(container.querySelector('.attach-btn')!);
    await waitFor(() => expect(container.querySelectorAll('.pending-chip').length).toBe(1));
    const el = container.querySelector('[role="textbox"]') as HTMLElement;
    await fireEvent.keyDown(el, { key: 'Enter' });
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('post_channel_message', expect.objectContaining({
        body: [],
        attachments: [{ cid: 'cid0', mime: 'text/plain', name: 'f.txt', size: 5, encrypted: true }],
      }));
    });
  });

  it('surfaces an ingest error on the compose error line', async () => {
    openMock.mockResolvedValue('/tmp/a.txt');
    const { adapter, container } = await setup();
    withIngest(adapter, { reject: new Error('artifact too large') });
    await fireEvent.click(container.querySelector('.attach-btn')!);
    await waitFor(() => {
      expect(container.querySelector('.compose-error')?.textContent).toContain('artifact too large');
      expect(container.querySelectorAll('.pending-chip').length).toBe(0);
    });
  });

  it('shows a "finishing upload" hint while ingesting and Enter does not post (finding 9)', async () => {
    // Pressing Enter while a file is still being ingested no-ops; surface the
    // in-flight state as "Finishing upload…" rather than a dead key.
    openMock.mockResolvedValue('/tmp/a.txt');
    const { adapter, container } = await setup();
    // Hold the ingest open so the component stays in the `ingesting` state.
    let resolveIngest!: (v: unknown) => void;
    const ingestGate = new Promise<unknown>((r) => {
      resolveIngest = r;
    });
    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      if (cmd === 'request_channel_backfill') return Promise.resolve(undefined);
      if (cmd === 'ingest_channel_artifact') return ingestGate;
      if (cmd === 'post_channel_message') return Promise.resolve('mid' + 'a'.repeat(29));
      return Promise.resolve(undefined);
    });
    await fireEvent.click(container.querySelector('.attach-btn')!);
    // Hint is visible while ingesting.
    await waitFor(() => {
      expect(container.querySelector('[data-testid="compose-ingest-hint"]')).toBeTruthy();
    });
    // Enter during ingest must not post.
    const el = container.querySelector('[role="textbox"]') as HTMLElement;
    el.textContent = 'too soon';
    await fireEvent.keyDown(el, { key: 'Enter' });
    expect(adapter.invoke).not.toHaveBeenCalledWith('post_channel_message', expect.anything());
    // Let the ingest finish; the hint clears.
    resolveIngest({ cid: 'cid0', mime: 'text/plain', name: 'f.txt', size: 5, encrypted: true });
    await waitFor(() => {
      expect(container.querySelector('[data-testid="compose-ingest-hint"]')).toBeNull();
    });
  });

  it('clears pending attachments when the channel changes', async () => {
    openMock.mockResolvedValue('/tmp/a.txt');
    const { adapter, container, props, rerender } = await setup();
    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      if (cmd === 'request_channel_backfill') return Promise.resolve(undefined);
      if (cmd === 'ingest_channel_artifact')
        return Promise.resolve({ cid: 'c0', mime: 'text/plain', name: 'f.txt', size: 5, encrypted: true });
      if (cmd === 'post_channel_message') return Promise.resolve('mid' + 'a'.repeat(29));
      return Promise.resolve(undefined);
    });
    await fireEvent.click(container.querySelector('.attach-btn')!);
    await waitFor(() => expect(container.querySelectorAll('.pending-chip').length).toBe(1));
    // Re-render the SAME instance with a changed channelId; the switch $effect
    // resets pendingAttachments so the chip must disappear.
    await rerender({ ...props, channelId: 'dd'.repeat(16) });
    await waitFor(() => expect(container.querySelectorAll('.pending-chip').length).toBe(0));
  });

  it('does not open a second file picker while one attach is in flight', async () => {
    let resolveOpen: (v: unknown) => void = () => {};
    // Reset call history so the count below measures only this test's clicks
    // (openMock is module-scoped and exercised by other tests in this suite).
    openMock.mockReset();
    openMock.mockImplementation(() => new Promise((r) => { resolveOpen = r; }));
    const { container } = await setup();
    const btn = container.querySelector('.attach-btn')!;
    await fireEvent.click(btn);
    await fireEvent.click(btn);
    expect(openMock).toHaveBeenCalledTimes(1);
    resolveOpen(null); // let the first flow finish/cancel cleanly
  });
});

describe('ChannelMessageFeed reactions — chips (ZEB-536)', () => {
  beforeEach(() => { vi.useFakeTimers({ shouldAdvanceTime: true }); });
  afterEach(() => { vi.useRealTimers(); });

  // Seed one message carrying `reactions` straight from the message event.
  async function seedMessageWithReactions(
    reactions: Array<{ emoji: string; count: number; mine: boolean; reactors: string[] }>,
    propOverrides: Record<string, unknown> = {},
  ) {
    const ctx = await setup(propOverrides);
    const handler = ctx.adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'm1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'ee'.repeat(20),
          at: { wallMs: 1000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode('hi')),
          reactions,
        },
      },
    });
    return ctx;
  }

  it('renders a chip per reaction with emoji + count', async () => {
    const { container } = await seedMessageWithReactions([
      { emoji: '👍', count: 2, mine: false, reactors: ['ee'.repeat(20), 'ff'.repeat(20)] },
    ]);
    await waitFor(() => {
      const chip = container.querySelector('.reaction-chip');
      expect(chip).toBeTruthy();
      expect(chip?.textContent).toContain('👍');
      expect(chip?.textContent).toContain('2');
    });
  });

  it('adds the .mine class when reaction.mine is true', async () => {
    const { container } = await seedMessageWithReactions([
      { emoji: '👍', count: 1, mine: true, reactors: ['cc'.repeat(20)] },
    ]);
    await waitFor(() => {
      expect(container.querySelector('.reaction-chip.mine')).toBeTruthy();
    });
  });

  it('clicking a mine chip toggles it off (add:false)', async () => {
    const { adapter, container } = await seedMessageWithReactions([
      { emoji: '👍', count: 1, mine: true, reactors: ['cc'.repeat(20)] },
    ]);
    let chip: Element | null = null;
    await waitFor(() => {
      chip = container.querySelector('.reaction-chip');
      expect(chip).toBeTruthy();
    });
    await fireEvent.click(chip!);
    expect(adapter.invoke).toHaveBeenCalledWith('set_message_reaction', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      messageId: 'm1',
      emoji: '👍',
      add: false,
    });
  });

  it('clicking a not-mine chip adds my reaction (add:true)', async () => {
    const { adapter, container } = await seedMessageWithReactions([
      { emoji: '👍', count: 1, mine: false, reactors: ['ee'.repeat(20)] },
    ]);
    let chip: Element | null = null;
    await waitFor(() => {
      chip = container.querySelector('.reaction-chip');
      expect(chip).toBeTruthy();
    });
    await fireEvent.click(chip!);
    expect(adapter.invoke).toHaveBeenCalledWith('set_message_reaction', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      messageId: 'm1',
      emoji: '👍',
      add: true,
    });
  });

  it('reaction-error banner can be dismissed (finding 14)', async () => {
    const { adapter, container } = await seedMessageWithReactions([
      { emoji: '👍', count: 1, mine: false, reactors: ['ee'.repeat(20)] },
    ]);
    // Make the toggle fail so the error banner surfaces.
    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'set_message_reaction') return Promise.reject(new Error('reaction rejected'));
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      if (cmd === 'request_channel_backfill') return Promise.resolve(undefined);
      return Promise.resolve(undefined);
    });
    let chip!: Element;
    await waitFor(() => {
      chip = container.querySelector('.reaction-chip')!;
      expect(chip).toBeTruthy();
    });
    await fireEvent.click(chip);
    await waitFor(() => expect(container.querySelector('.reaction-error')).not.toBeNull());
    // The dismiss button clears the banner without requiring a new action.
    await fireEvent.click(container.querySelector('.reaction-error-dismiss')!);
    await waitFor(() => expect(container.querySelector('.reaction-error')).toBeNull());
  });

  it('reaction picker supports arrow-key roving focus across menuitems (finding 15)', async () => {
    const { container } = await seedMessageWithReactions([]);
    let toggle!: Element;
    await waitFor(() => {
      toggle = container.querySelector('.picker-toggle')!;
      expect(toggle).toBeTruthy();
    });
    await fireEvent.click(toggle);
    const menu = container.querySelector('.reaction-picker') as HTMLElement;
    expect(menu).toBeTruthy();
    expect(menu.getAttribute('role')).toBe('menu');
    const items = menu.querySelectorAll<HTMLElement>('[role="menuitem"]');
    expect(items.length).toBeGreaterThan(1);
    // Opening the picker moves focus to the first reaction.
    expect(document.activeElement).toBe(items[0]);
    // ArrowRight advances; ArrowLeft returns (wrapping roving focus). Fire from
    // the focused item so the event bubbles to the menu's handler as it would
    // in the browser.
    await fireEvent.keyDown(items[0], { key: 'ArrowRight' });
    expect(document.activeElement).toBe(items[1]);
    await fireEvent.keyDown(items[1], { key: 'ArrowLeft' });
    expect(document.activeElement).toBe(items[0]);
  });

  it('a live channel-reaction-received event updates the chip count', async () => {
    const { adapter, container } = await seedMessageWithReactions([
      { emoji: '👍', count: 1, mine: false, reactors: ['ee'.repeat(20)] },
    ]);
    await waitFor(() =>
      expect(container.querySelector('.reaction-chip')?.textContent).toContain('1'),
    );
    const rh = adapter.listeners.get('channel-reaction-received')!;
    rh({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        messageId: 'm1',
        reactor: 'ff'.repeat(20),
        emoji: '👍',
        add: true,
        at: { wallMs: 2000, logical: 0, deviceId: 'd' },
      },
    });
    await waitFor(() =>
      expect(container.querySelector('.reaction-chip')?.textContent).toContain('2'),
    );
  });

  it('chip title lists reactor display names via resolveCard', async () => {
    const resolveCard = (hex: string) =>
      hex === 'ee'.repeat(20) ? ({ displayName: 'Ildwyn' } as any) : undefined;
    const { container } = await seedMessageWithReactions(
      [{ emoji: '👍', count: 1, mine: false, reactors: ['ee'.repeat(20)] }],
      { resolveCard },
    );
    await waitFor(() => {
      const chip = container.querySelector('.reaction-chip');
      expect(chip?.getAttribute('title')).toContain('Ildwyn');
    });
  });
});

describe('ChannelMessageFeed reactions — toolbar + picker (ZEB-536)', () => {
  beforeEach(() => { vi.useFakeTimers({ shouldAdvanceTime: true }); });
  afterEach(() => { vi.useRealTimers(); });

  async function seedPlainMessage() {
    const ctx = await setup();
    const handler = ctx.adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'm1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'ee'.repeat(20),
          at: { wallMs: 1000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode('hi')),
        },
      },
    });
    await waitFor(() => expect(ctx.container.querySelector('.channel-message')).toBeTruthy());
    return ctx;
  }

  it('renders the quick-react buttons 👍 👎 in the toolbar', async () => {
    const { container } = await seedPlainMessage();
    const quick = container.querySelectorAll('.reaction-toolbar .quick-react');
    expect(quick.length).toBe(2);
    expect(quick[0].textContent).toContain('👍');
    expect(quick[1].textContent).toContain('👎');
  });

  it('does not render the reaction toolbar on pre-fork snapshot messages', async () => {
    // Pre-fork messages come from the original community's log, not the live
    // channel — reacting would mis-target the current (community, channel),
    // so the hover toolbar must be suppressed for them (CodeAnt PR #316).
    const preForkMsg = {
      messageId: 'pf1',
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      author: 'ee'.repeat(20),
      at: { wallMs: 500, logical: 0, deviceId: 'd' },
      body: Array.from(new TextEncoder().encode('old message')),
    };
    const { container } = await setup({
      snapshotMessages: [preForkMsg],
      originalCommunityName: 'OldCommunity',
      forkedAtMs: 1000,
    });
    let preForkArticle: Element | null = null;
    await waitFor(() => {
      preForkArticle = container.querySelector('article.channel-message.pre-fork');
      expect(preForkArticle).toBeTruthy();
    });
    expect(preForkArticle!.querySelector('.reaction-toolbar')).toBeNull();
  });

  it('clicking the 👍 quick-react adds the reaction (add:true)', async () => {
    const { adapter, container } = await seedPlainMessage();
    const thumb = container.querySelector('.reaction-toolbar .quick-react') as HTMLButtonElement;
    await fireEvent.click(thumb);
    expect(adapter.invoke).toHaveBeenCalledWith('set_message_reaction', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      messageId: 'm1',
      emoji: '👍',
      add: true,
    });
  });

  it('rapid double-click on a quick-react sends only one set_message_reaction (CodeAnt #318)', async () => {
    const { adapter, container } = await seedPlainMessage();
    // Hold the reaction IPC in flight so the second click lands within the
    // in-flight window — the realistic rapid double-click. Without a guard both
    // clicks compute add:true from the still-stale `mine` and double-send.
    let resolveReact: () => void = () => {};
    (adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'set_message_reaction') return new Promise<void>((r) => { resolveReact = () => r(); });
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    const thumb = container.querySelector('.reaction-toolbar .quick-react') as HTMLButtonElement;
    await fireEvent.click(thumb);
    await fireEvent.click(thumb);
    const reactCalls = (adapter.invoke as any).mock.calls.filter(
      (c: any[]) => c[0] === 'set_message_reaction',
    );
    expect(reactCalls.length).toBe(1);
    resolveReact();
  });

  it('the picker toggle opens a grid of 10 emoji', async () => {
    const { container } = await seedPlainMessage();
    await fireEvent.click(container.querySelector('.picker-toggle') as HTMLButtonElement);
    await waitFor(() => {
      const picker = container.querySelector('.reaction-picker');
      expect(picker).toBeTruthy();
      expect(picker!.querySelectorAll('.picker-emoji').length).toBe(10);
    });
  });

  it('selecting a picker emoji adds it (add:true) and closes the picker', async () => {
    const { adapter, container } = await seedPlainMessage();
    await fireEvent.click(container.querySelector('.picker-toggle') as HTMLButtonElement);
    let party: HTMLButtonElement | null = null;
    await waitFor(() => {
      const btns = Array.from(container.querySelectorAll('.picker-emoji')) as HTMLButtonElement[];
      party = btns.find((b) => b.textContent?.includes('🎉')) ?? null;
      expect(party).toBeTruthy();
    });
    await fireEvent.click(party!);
    expect(adapter.invoke).toHaveBeenCalledWith('set_message_reaction', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      messageId: 'm1',
      emoji: '🎉',
      add: true,
    });
    await waitFor(() => expect(container.querySelector('.reaction-picker')).toBeNull());
  });

  it('Escape closes the picker', async () => {
    const { container } = await seedPlainMessage();
    await fireEvent.click(container.querySelector('.picker-toggle') as HTMLButtonElement);
    await waitFor(() => expect(container.querySelector('.reaction-picker')).toBeTruthy());
    await fireEvent.keyDown(window, { key: 'Escape' });
    await waitFor(() => expect(container.querySelector('.reaction-picker')).toBeNull());
  });

  it('closing the picker resets namedPickerFor so the named popover does not auto-reopen', async () => {
    // CodeRabbit (ZEB-541): `namedPickerFor` could linger after the main picker
    // closed, so reopening the picker on the same message auto-reopened the
    // named popover. The $effect that aligns the two must drop it on close.
    const { adapter, container } = await seedPlainMessage();
    (adapter.invoke as any).mockImplementation((cmd: string) =>
      cmd === 'list_emoji_names' ? Promise.resolve([]) : Promise.resolve(undefined),
    );
    // Open picker → open the named popover.
    await fireEvent.click(container.querySelector('.picker-toggle') as HTMLButtonElement);
    await waitFor(() => expect(container.querySelector('.reaction-picker')).toBeTruthy());
    await fireEvent.click(container.querySelector('.picker-named') as HTMLButtonElement);
    await waitFor(() => expect(container.querySelector('.named-popover')).toBeTruthy());
    // Close the picker.
    await fireEvent.keyDown(window, { key: 'Escape' });
    await waitFor(() => expect(container.querySelector('.reaction-picker')).toBeNull());
    // Reopen the picker on the SAME message — the named popover must be gone.
    await fireEvent.click(container.querySelector('.picker-toggle') as HTMLButtonElement);
    await waitFor(() => expect(container.querySelector('.reaction-picker')).toBeTruthy());
    expect(container.querySelector('.named-popover')).toBeNull();
  });

  it('clicking outside closes the picker', async () => {
    const { container } = await seedPlainMessage();
    await fireEvent.click(container.querySelector('.picker-toggle') as HTMLButtonElement);
    await waitFor(() => expect(container.querySelector('.reaction-picker')).toBeTruthy());
    await fireEvent.click(document.body);
    await waitFor(() => expect(container.querySelector('.reaction-picker')).toBeNull());
  });

  it("clicking a DIFFERENT message's toolbar closes the open picker (Greptile #316 P1)", async () => {
    // Every message renders its own .reaction-toolbar; the outside-click guard
    // must close the picker when the click lands in another message's toolbar,
    // not just leave it open because *some* toolbar was hit.
    const ctx = await setup();
    const handler = ctx.adapter.listeners.get('channel-message-received')!;
    for (const [id, w] of [['m1', 1000], ['m2', 2000]] as const) {
      handler({
        payload: {
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          message: {
            messageId: id,
            communityId: 'aa'.repeat(16),
            channelId: 'bb'.repeat(16),
            author: 'ee'.repeat(20),
            at: { wallMs: w, logical: 0, deviceId: 'd' },
            body: Array.from(new TextEncoder().encode(`hi ${id}`)),
          },
        },
      });
    }
    await waitFor(() =>
      expect(ctx.container.querySelectorAll('.channel-message').length).toBe(2),
    );
    const toolbars = Array.from(ctx.container.querySelectorAll('.reaction-toolbar'));
    expect(toolbars.length).toBe(2);
    // Open the picker on the first message's toolbar.
    await fireEvent.click(toolbars[0].querySelector('.picker-toggle') as HTMLButtonElement);
    await waitFor(() => expect(ctx.container.querySelector('.reaction-picker')).toBeTruthy());
    // The OTHER toolbar is the one that does NOT contain the open picker.
    const otherToolbar = toolbars.find((tb) => !tb.querySelector('.reaction-picker'))!;
    expect(otherToolbar).toBeTruthy();
    // Clicking a control in a different message's toolbar must close the picker.
    await fireEvent.click(otherToolbar.querySelector('.quick-react') as HTMLButtonElement);
    await waitFor(() => expect(ctx.container.querySelector('.reaction-picker')).toBeNull());
  });
});

describe('ChannelMessageFeed reactions — custom (CAS) emoji (ZEB-541)', () => {
  beforeEach(() => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    normalizeEmojiMock.mockClear();
    // ReactionEmojiImage decodes the previewed bytes — stub the browser APIs
    // jsdom lacks so a custom chip can resolve its <img> blob URL.
    vi.stubGlobal('createImageBitmap', vi.fn().mockResolvedValue({ width: 128, height: 128, close: vi.fn() }));
    vi.stubGlobal('URL', {
      ...URL,
      createObjectURL: vi.fn(() => 'blob:emoji'),
      revokeObjectURL: vi.fn(),
    });
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  // Seed a message carrying a CUSTOM reaction (emoji === '', emojiCid set).
  async function seedCustomReaction(mine: boolean) {
    const ctx = await setup();
    (ctx.adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      if (cmd === 'preview_reaction_emoji')
        // A full PNG header (signature + IHDR declaring 64x64). The render path
        // now parses dims BEFORE decode and rejects unparseable headers, so an
        // 8-byte signature alone is (correctly) refused — must carry the IHDR.
        return Promise.resolve([
          0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, // signature
          0x00, 0x00, 0x00, 0x0d, // IHDR length = 13
          0x49, 0x48, 0x44, 0x52, // "IHDR"
          0x00, 0x00, 0x00, 0x40, // width = 64
          0x00, 0x00, 0x00, 0x40, // height = 64
        ]);
      if (cmd === 'set_message_reaction') return Promise.resolve(undefined);
      if (cmd === 'ingest_channel_artifact_bytes') {
        return Promise.resolve({ cid: 'newcid', mime: 'image/png', name: '', size: 321, encrypted: true });
      }
      return Promise.resolve(undefined);
    });
    const handler = ctx.adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'm1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'ee'.repeat(20),
          at: { wallMs: 1000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode('hi')),
          reactions: [
            { emoji: '', count: 1, mine, reactors: ['ee'.repeat(20)], emojiCid: 'abc123', emojiSize: 321 },
          ],
        },
      },
    });
    return ctx;
  }

  it('renders a ReactionEmojiImage <img> for a reaction with emojiCid', async () => {
    const { container, adapter } = await seedCustomReaction(false);
    await waitFor(() => {
      const img = container.querySelector('.reaction-chip img.reaction-emoji-img');
      expect(img).toBeTruthy();
    });
    expect(adapter.invoke).toHaveBeenCalledWith('preview_reaction_emoji', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      cid: 'abc123',
    });
  });

  it('clicking a not-mine custom chip adds my reaction with the CID descriptor (add:true)', async () => {
    const { container, adapter } = await seedCustomReaction(false);
    let chip: Element | null = null;
    await waitFor(() => {
      chip = container.querySelector('.reaction-chip');
      expect(chip).toBeTruthy();
    });
    await fireEvent.click(chip!);
    expect(adapter.invoke).toHaveBeenCalledWith('set_message_reaction', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      messageId: 'm1',
      emoji: '',
      add: true,
      customEmoji: { cid: 'abc123', mime: 'image/png', size: 321 },
    });
  });

  it('clicking a mine custom chip toggles it off (add:false)', async () => {
    const { container, adapter } = await seedCustomReaction(true);
    let chip: Element | null = null;
    await waitFor(() => {
      chip = container.querySelector('.reaction-chip.mine');
      expect(chip).toBeTruthy();
    });
    await fireEvent.click(chip!);
    expect(adapter.invoke).toHaveBeenCalledWith('set_message_reaction', expect.objectContaining({
      messageId: 'm1',
      emoji: '',
      add: false,
      customEmoji: { cid: 'abc123', mime: 'image/png', size: 321 },
    }));
  });

  it('the picker custom button runs normalize → ingest → react', async () => {
    const ctx = await setup();
    (ctx.adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      if (cmd === 'ingest_channel_artifact_bytes') {
        return Promise.resolve({ cid: 'newcid', mime: 'image/png', name: '', size: 321, encrypted: true });
      }
      return Promise.resolve(undefined);
    });
    const handler = ctx.adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'm1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'ee'.repeat(20),
          at: { wallMs: 1000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode('hi')),
        },
      },
    });
    await waitFor(() => expect(ctx.container.querySelector('.channel-message')).toBeTruthy());

    // Open the picker, click the custom (+) affordance — this sets the in-flight
    // target and clicks the hidden file input.
    await fireEvent.click(ctx.container.querySelector('.picker-toggle') as HTMLButtonElement);
    await waitFor(() => expect(ctx.container.querySelector('.picker-custom')).toBeTruthy());
    await fireEvent.click(ctx.container.querySelector('.picker-custom') as HTMLButtonElement);

    // Simulate the user choosing a file: set files on the hidden input and fire change.
    const input = ctx.container.querySelector('.custom-emoji-input') as HTMLInputElement;
    const file = new File([new Uint8Array([1, 2, 3])], 'pepe.png', { type: 'image/png' });
    Object.defineProperty(input, 'files', { value: [file], configurable: true });
    await fireEvent.change(input);

    await waitFor(() => {
      expect(normalizeEmojiMock).toHaveBeenCalledWith(file);
      expect(ctx.adapter.invoke).toHaveBeenCalledWith(
        'ingest_channel_artifact_bytes',
        expect.objectContaining({ communityId: 'aa'.repeat(16), mime: 'image/png', encrypt: false }),
      );
      expect(ctx.adapter.invoke).toHaveBeenCalledWith('set_message_reaction', {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        messageId: 'm1',
        emoji: '',
        add: true,
        customEmoji: { cid: 'newcid', mime: 'image/png', size: 321 },
      });
    });
  });

  it('custom-emoji pick with "keep private" checked ingests encrypted', async () => {
    const ctx = await setup();
    (ctx.adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      if (cmd === 'ingest_channel_artifact_bytes') {
        return Promise.resolve({ cid: 'newcid', mime: 'image/png', name: '', size: 321, encrypted: true });
      }
      return Promise.resolve(undefined);
    });
    const handler = ctx.adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'm1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'ee'.repeat(20),
          at: { wallMs: 1000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode('hi')),
        },
      },
    });
    await waitFor(() => expect(ctx.container.querySelector('.channel-message')).toBeTruthy());

    await fireEvent.click(ctx.container.querySelector('.picker-toggle') as HTMLButtonElement);
    const priv = await waitFor(() => {
      const el = ctx.container.querySelector(
        '[aria-label="Keep custom emoji private to this community"]',
      );
      if (!el) throw new Error('private checkbox not rendered');
      return el as HTMLInputElement;
    });
    await fireEvent.click(priv);
    await fireEvent.click(ctx.container.querySelector('.picker-custom') as HTMLButtonElement);

    const input = ctx.container.querySelector('.custom-emoji-input') as HTMLInputElement;
    const file = new File([new Uint8Array([1, 2, 3])], 'pepe.png', { type: 'image/png' });
    Object.defineProperty(input, 'files', { value: [file], configurable: true });
    await fireEvent.change(input);

    await waitFor(() => {
      expect(ctx.adapter.invoke).toHaveBeenCalledWith(
        'ingest_channel_artifact_bytes',
        expect.objectContaining({ encrypt: true }),
      );
    });
  });

  // Task 10: seed a plain (no reactions) message + an emoji-name map; the feed
  // renders <NamedEmojiPicker>, which lists from `list_emoji_names`.
  async function seedWithNamedEmoji() {
    const ctx = await setup();
    (ctx.adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      if (cmd === 'list_emoji_names')
        return Promise.resolve([{ cid: 'aa', name: 'catjam', mime: 'image/png', size: 7 }]);
      if (cmd === 'preview_named_emoji') return Promise.resolve([0x89, 0x50, 0x4e, 0x47]);
      if (cmd === 'set_emoji_name') return Promise.resolve(undefined);
      if (cmd === 'set_message_reaction') return Promise.resolve(undefined);
      return Promise.resolve(undefined);
    });
    const handler = ctx.adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'm1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'ee'.repeat(20),
          at: { wallMs: 1000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode('hi')),
        },
      },
    });
    await waitFor(() => expect(ctx.container.querySelector('.channel-message')).toBeTruthy());
    return ctx;
  }

  it('picking from the named-emoji popover reacts with the stored descriptor', async () => {
    const ctx = await seedWithNamedEmoji();
    const reactSpy = vi.spyOn(ctx.service, 'reactToMessage');

    // Open the reaction picker, then the named-emoji popover.
    await fireEvent.click(ctx.container.querySelector('.picker-toggle') as HTMLButtonElement);
    const namedBtn = await ctx.findByLabelText('Named emoji');
    await fireEvent.click(namedBtn);

    // The popover lists the named emoji; click its tile (title = the name).
    const tile = await ctx.findByTitle('catjam');
    await fireEvent.click(tile);

    expect(reactSpy).toHaveBeenCalledWith(
      'aa'.repeat(16),
      'bb'.repeat(16),
      'm1',
      '',
      true,
      { cid: 'aa', mime: 'image/png', size: 7 },
    );
  });

  it('reaction-picker roving focus excludes the nested named-emoji popover (Qodo #331)', async () => {
    const ctx = await seedWithNamedEmoji();
    const container = ctx.container;
    await fireEvent.click(container.querySelector('.picker-toggle') as HTMLButtonElement);
    await fireEvent.click(await ctx.findByLabelText('Named emoji'));
    // The popover renders its own role="menuitem" tile (the seeded "catjam").
    await waitFor(() =>
      expect(container.querySelector('.named-popover [role="menuitem"]')).toBeTruthy(),
    );
    const menu = container.querySelector('.reaction-picker') as HTMLElement;
    const outer = Array.from(menu.querySelectorAll<HTMLElement>('[role="menuitem"]')).filter(
      (el) => !el.closest('.named-popover'),
    );
    const popoverItem = menu.querySelector('.named-popover [role="menuitem"]') as HTMLElement;
    expect(outer.length).toBeGreaterThan(1);
    expect(popoverItem).toBeTruthy();
    // Roving from the last OUTER menuitem wraps to the first OUTER one — never
    // into the popover's tile.
    const last = outer[outer.length - 1];
    last.focus();
    await fireEvent.keyDown(last, { key: 'ArrowRight' });
    expect(document.activeElement).toBe(outer[0]);
    expect(document.activeElement).not.toBe(popoverItem);
    // From inside the popover, the outer grid does NOT hijack arrow keys.
    popoverItem.focus();
    await fireEvent.keyDown(popoverItem, { key: 'ArrowRight' });
    expect(document.activeElement).toBe(popoverItem);
  });

  // Seed a message with a single PUBLIC custom reaction chip (encrypted unset).
  async function seedPublicChip(encrypted = false) {
    const ctx = await setup();
    (ctx.adapter.invoke as any).mockImplementation((cmd: string) => {
      if (cmd === 'list_channel_messages') return Promise.resolve([]);
      if (cmd === 'list_emoji_names') return Promise.resolve([]);
      if (cmd === 'preview_reaction_emoji')
        return Promise.resolve([
          0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a,
          0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
          0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x40,
        ]);
      if (cmd === 'set_emoji_name') return Promise.resolve(undefined);
      return Promise.resolve(undefined);
    });
    const handler = ctx.adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'm1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'ee'.repeat(20),
          at: { wallMs: 1000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode('hi')),
          reactions: [
            { emoji: '', count: 1, mine: false, reactors: ['ee'.repeat(20)], emojiCid: 'aa', emojiSize: 7, encrypted },
          ],
        },
      },
    });
    await waitFor(() => expect(ctx.container.querySelector('.reaction-chip')).toBeTruthy());
    return ctx;
  }

  it('name-this on a public custom chip calls setEmojiName with the chip descriptor', async () => {
    const ctx = await seedPublicChip(false);
    const nameSpy = vi.spyOn(ctx.service, 'setEmojiName');

    await fireEvent.click(await ctx.findByLabelText('Name this emoji'));
    const input = (await ctx.findByLabelText('Emoji name')) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'catjam' } });
    await fireEvent.click(await ctx.findByLabelText('Save emoji name'));

    expect(nameSpy).toHaveBeenCalledWith('aa', 'catjam', 'image/png', 7);
  });

  it('name-this affordance is absent on an encrypted custom chip', async () => {
    const ctx = await seedPublicChip(true);
    expect(ctx.queryByLabelText('Name this emoji')).toBeNull();
  });
});

describe('ChannelMessageFeed: composerPlaceholder (ZEB-612 S5)', () => {
  beforeEach(() => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it('composerPlaceholder overrides the default composer placeholder', async () => {
    const { container } = await setup({ composerPlaceholder: 'Message the room…' });
    await waitFor(() => {
      expect(container.querySelector('[data-placeholder="Message the room…"]')).toBeTruthy();
    });
    expect(container.querySelector('[data-placeholder="Message #general"]')).toBeNull();
  });

  it('absent composerPlaceholder keeps the long-standing Message #name default', async () => {
    const { container } = await setup();
    await waitFor(() => {
      expect(container.querySelector('[data-placeholder="Message #general"]')).toBeTruthy();
    });
  });
});
