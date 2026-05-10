import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import ChannelMessageFeed from '../ChannelMessageFeed.svelte';
import { ChannelMessageService } from '../../channel-message-service';
import type { TauriAdapter } from '../../zenoh-service';

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
