import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import ChannelMessageFeed from '../ChannelMessageFeed.svelte';
import { ChannelMessageService } from '../../channel-message-service';
import type { TauriAdapter } from '../../zenoh-service';
import { VotingAdapter } from '../../voting-adapter';
import type { PollMeta } from '../../types/voting';

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
    const textarea = container.querySelector('textarea.compose-input') as HTMLTextAreaElement;
    await fireEvent.input(textarea, { target: { value: 'first message' } });
    await fireEvent.keyDown(textarea, { key: 'Enter' });
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('post_channel_message', expect.objectContaining({
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        body: Array.from(new TextEncoder().encode('first message')),
        replyTo: undefined,
      }));
    });
    // Compose box clears on successful send.
    expect(textarea.value).toBe('');
  });

  it('Shift+Enter inserts a newline (does NOT send)', async () => {
    const { adapter, container } = await setup();
    const textarea = container.querySelector('textarea.compose-input') as HTMLTextAreaElement;
    await fireEvent.input(textarea, { target: { value: 'line one' } });
    await fireEvent.keyDown(textarea, { key: 'Enter', shiftKey: true });
    expect(adapter.invoke).not.toHaveBeenCalledWith(
      'post_channel_message',
      expect.anything(),
    );
    // Textarea retains the value (browser would handle the newline insertion;
    // we just verify we didn't send).
    expect(textarea.value).toBe('line one');
  });

  it('does not post empty/whitespace-only messages', async () => {
    const { adapter, container } = await setup();
    const textarea = container.querySelector('textarea.compose-input') as HTMLTextAreaElement;
    await fireEvent.input(textarea, { target: { value: '   ' } });
    await fireEvent.keyDown(textarea, { key: 'Enter' });
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
    const textarea = container.querySelector('textarea.compose-input') as HTMLTextAreaElement;
    await fireEvent.input(textarea, { target: { value: 'will fail' } });
    await fireEvent.keyDown(textarea, { key: 'Enter' });
    await waitFor(() => {
      expect(container.querySelector('.compose-error')?.textContent).toMatch(/no engine/);
    });
    // Compose retains text on failure so user can retry.
    expect(textarea.value).toBe('will fail');
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
});
