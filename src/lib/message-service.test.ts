import { describe, it, expect, vi, beforeEach } from 'vitest';
import { MessageService, type ChannelMessageEvent } from './message-service';
import type { TauriAdapter } from './zenoh-service';
import { messages as mockMessages } from './mock-data';

function createMockAdapter() {
  const listeners = new Map<string, (event: { payload: unknown }) => void>();
  const unlisten = vi.fn();
  const adapter: TauriAdapter = {
    invoke: vi.fn().mockResolvedValue(undefined),
    listen: vi.fn().mockImplementation((event: string, handler: (event: { payload: unknown }) => void) => {
      listeners.set(event, handler);
      return Promise.resolve(unlisten);
    }),
  };
  function emit(event: string, payload: unknown) {
    listeners.get(event)?.({ payload });
  }
  return { adapter, emit, unlisten };
}

describe('MessageService', () => {
  let svc: MessageService;

  beforeEach(() => {
    svc = new MessageService();
  });

  // ── Constructor ───────────────────────────────────────────────────

  it('seeds with mock messages', () => {
    expect(svc.messages.length).toBe(mockMessages.length);
  });

  it('populates seenIds from mock data', () => {
    // A second message with an existing ID should be deduped
    const { adapter, emit } = createMockAdapter();
    svc.connectAdapter(adapter);
    const existingId = mockMessages[0].id;
    emit('message-received', { id: existingId, senderAddress: 'x', senderName: 'X', channel: 'c', hub: 'h', text: 'dup', timestamp: 1, priority: 'standard' } satisfies ChannelMessageEvent);
    expect(svc.messages.length).toBe(mockMessages.length);
  });

  // ── connectAdapter ────────────────────────────────────────────────

  it('registers a message-received listener', async () => {
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    expect(adapter.listen).toHaveBeenCalledWith('message-received', expect.any(Function));
  });

  it('idempotent — second call is a no-op', async () => {
    const { adapter: a1 } = createMockAdapter();
    const { adapter: a2 } = createMockAdapter();
    await svc.connectAdapter(a1);
    await svc.connectAdapter(a2);
    expect(a2.listen).not.toHaveBeenCalled();
  });

  it('appends incoming wire messages', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    const wire: ChannelMessageEvent = {
      id: 'net-1', senderAddress: 'abc123', senderName: 'Peer',
      channel: 'general', hub: 'main', text: 'hello', timestamp: Date.now(), priority: 'standard',
    };
    emit('message-received', wire);
    expect(svc.messages.length).toBe(mockMessages.length + 1);
    expect(svc.messages.at(-1)!.text).toBe('hello');
  });

  it('deduplicates by id', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    const wire: ChannelMessageEvent = {
      id: 'dup-1', senderAddress: 'x', senderName: 'X',
      channel: 'c', hub: 'h', text: 'first', timestamp: 1, priority: 'standard',
    };
    emit('message-received', wire);
    emit('message-received', wire);
    expect(svc.messages.filter(m => m.id === 'dup-1').length).toBe(1);
  });

  it('calls onChange when a new message arrives', async () => {
    const { adapter, emit } = createMockAdapter();
    svc.onChange = vi.fn();
    await svc.connectAdapter(adapter);
    emit('message-received', {
      id: 'notify-1', senderAddress: 'x', senderName: 'X',
      channel: 'c', hub: 'h', text: 'hi', timestamp: 1, priority: 'standard',
    } satisfies ChannelMessageEvent);
    expect(svc.onChange).toHaveBeenCalledOnce();
  });

  // ── wireToMessage ─────────────────────────────────────────────────

  it('maps self-sent messages to address "self"', async () => {
    const { adapter, emit } = createMockAdapter();
    svc.ownAddress = 'myaddr';
    await svc.connectAdapter(adapter);
    emit('message-received', {
      id: 'self-1', senderAddress: 'myaddr', senderName: 'Me',
      channel: 'c', hub: 'h', text: 'echo', timestamp: 1, priority: 'standard',
    } satisfies ChannelMessageEvent);
    const msg = svc.messages.find(m => m.id === 'self-1')!;
    expect(msg.sender.address).toBe('self');
    expect(msg.sender.displayName).toBe('You');
  });

  it('falls back to truncated address when senderName is empty', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    emit('message-received', {
      id: 'noname-1', senderAddress: 'abcdef1234567890', senderName: '',
      channel: 'c', hub: 'h', text: 'hi', timestamp: 1, priority: 'standard',
    } satisfies ChannelMessageEvent);
    const msg = svc.messages.find(m => m.id === 'noname-1')!;
    expect(msg.sender.displayName).toBe('abcdef12');
  });

  it('defaults priority to standard when wire priority is empty', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    emit('message-received', {
      id: 'prio-1', senderAddress: 'x', senderName: 'X',
      channel: 'c', hub: 'h', text: 'hi', timestamp: 1, priority: '',
    } satisfies ChannelMessageEvent);
    const msg = svc.messages.find(m => m.id === 'prio-1')!;
    expect(msg.priority).toBe('standard');
  });

  // ── send ───────────────────────────────────────────────────────────

  it('invokes send_message on the adapter', async () => {
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    await svc.send('hi', 'standard', 'general', 'main');
    expect(adapter.invoke).toHaveBeenCalledWith('send_message', {
      message: { channel: 'general', hub: 'main', text: 'hi', priority: 'standard', replyTo: undefined, senderName: 'You' },
    });
  });

  it('falls back to local message when no adapter', async () => {
    const before = svc.messages.length;
    await svc.send('offline msg', 'quiet', 'general', 'main');
    expect(svc.messages.length).toBe(before + 1);
    expect(svc.messages.at(-1)!.text).toBe('offline msg');
    expect(svc.messages.at(-1)!.sender.address).toBe('self');
  });

  it('falls back locally on "not connected" error', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockRejectedValue(new Error('not connected'));
    await svc.connectAdapter(adapter);
    const before = svc.messages.length;
    await svc.send('fallback', 'standard', 'c', 'h');
    expect(svc.messages.length).toBe(before + 1);
  });

  it('re-throws non-connectivity errors', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockRejectedValue(new Error('permission denied'));
    await svc.connectAdapter(adapter);
    await expect(svc.send('boom', 'standard', 'c', 'h')).rejects.toThrow('permission denied');
  });

  it('includes replyTo in send payload', async () => {
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    await svc.send('reply', 'standard', 'c', 'h', 'msg-01');
    expect(adapter.invoke).toHaveBeenCalledWith('send_message', expect.objectContaining({
      message: expect.objectContaining({ replyTo: 'msg-01' }),
    }));
  });

  it('calls onChange on offline send', async () => {
    svc.onChange = vi.fn();
    await svc.send('local', 'standard', 'c', 'h');
    expect(svc.onChange).toHaveBeenCalledOnce();
  });

  // ── destroy / addUnlisten ─────────────────────────────────────────

  it('destroy calls all registered unlisteners', async () => {
    const { adapter, unlisten } = createMockAdapter();
    await svc.connectAdapter(adapter);
    const external = vi.fn();
    svc.addUnlisten(external);
    svc.destroy();
    expect(unlisten).toHaveBeenCalledOnce();
    expect(external).toHaveBeenCalledOnce();
  });

  it('destroy is safe to call twice', async () => {
    const { adapter, unlisten } = createMockAdapter();
    await svc.connectAdapter(adapter);
    svc.destroy();
    svc.destroy();
    expect(unlisten).toHaveBeenCalledOnce();
  });
});
