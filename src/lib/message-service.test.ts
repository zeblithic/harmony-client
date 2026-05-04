import { describe, it, expect, vi, beforeEach } from 'vitest';
import { MessageService, type ChannelMessageEvent } from './message-service';
import { messages as mockMessages } from './mock-data';
import { createMockAdapter } from './test-utils';

describe('MessageService', () => {
  let svc: MessageService;

  beforeEach(() => {
    svc = new MessageService();
  });

  // ── Constructor ───────────────────────────────────────────────────

  it('seeds with mock messages', () => {
    expect(svc.messages.length).toBe(mockMessages.length);
  });

  it('populates seenIds from mock data', async () => {
    // A second message with an existing ID should be deduped
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
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
    // connectAdapter registers one listener per IPC channel
    // (message-received + 4 DM lifecycle channels = 5). The mock adapter
    // returns the same `unlisten` fn from each listen() call, so destroy
    // invokes it once per registered listener.
    expect(unlisten).toHaveBeenCalledTimes(5);
    expect(external).toHaveBeenCalledOnce();
  });

  it('destroy is safe to call twice', async () => {
    const { adapter, unlisten } = createMockAdapter();
    await svc.connectAdapter(adapter);
    svc.destroy();
    svc.destroy();
    // Second destroy is a no-op (unlisteners already cleared).
    expect(unlisten).toHaveBeenCalledTimes(5);
  });
});

// ── DM events (Phase 4 — ZEB-228) ────────────────────────────────────
//
// These cover the four DM lifecycle IPC events that the Rust event loop
// emits: `dm-received`, `dm-delivered`, `dm-expired`, `dm-deleted`. The
// MessageService merges them into the same `messages` buffer the channel
// path uses; `channel` is set to the SpaceId hex (matches NavNode.id), and
// `messageId` correlates self-Messages to lifecycle transitions.

describe('MessageService DM events', () => {
  let svc: MessageService;

  beforeEach(() => {
    svc = new MessageService();
    // Drop mock seed so DM-arriving messages are easy to assert on.
    svc.messages = [];
  });

  function hexEncode(s: string): string {
    return Array.from(new TextEncoder().encode(s))
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');
  }

  it('pushes a Message for dm-received with body decoded from hex', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);

    emit('dm-received', {
      spaceId: 'aabbccdd',
      messageCid: 'deadbeef',
      from: 'bob-hex-address',
      sentAt: 1_700_000_000_000,
      receivedAt: 1_700_000_000_500,
      body: hexEncode('hello world'),
      mimeType: 'text/plain',
    });

    const incoming = svc.messages.filter((m) => m.channel === 'aabbccdd');
    expect(incoming).toHaveLength(1);
    expect(incoming[0].text).toBe('hello world');
    expect(incoming[0].timestamp).toBe(1_700_000_000_000);
    expect(incoming[0].sender.address).toBe('bob-hex-address');
    expect(incoming[0].id).toBe('deadbeef');
  });

  it('calls onChange when dm-received fires', async () => {
    const { adapter, emit } = createMockAdapter();
    svc.onChange = vi.fn();
    await svc.connectAdapter(adapter);

    emit('dm-received', {
      spaceId: 'aabb',
      messageCid: 'cafebabe',
      from: 'peer-hex',
      sentAt: 1,
      receivedAt: 2,
      body: hexEncode('hi'),
      mimeType: 'text/plain',
    });

    expect(svc.onChange).toHaveBeenCalled();
  });

  it('transitions self-Message to delivered on dm-delivered', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);

    svc.messages = [{
      id: 'optimistic-id',
      messageId: 'mid1',
      sender: { address: 'self', displayName: 'You' },
      text: 'pending',
      timestamp: Date.now(),
      media: [],
      priority: 'standard',
      channel: 'aabbccdd',
      deliveryState: 'sending',
    }];

    emit('dm-delivered', { messageId: 'mid1', recipient: 'bob-hex-address' });

    expect(svc.messages[0].deliveryState).toBe('delivered');
  });

  it('transitions self-Message to expired on dm-expired', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);

    svc.messages = [{
      id: 'optimistic-id',
      messageId: 'mid-exp',
      sender: { address: 'self', displayName: 'You' },
      text: 'pending',
      timestamp: Date.now(),
      media: [],
      priority: 'standard',
      channel: 'aabbccdd',
      deliveryState: 'sending',
    }];

    emit('dm-expired', { messageId: 'mid-exp' });

    expect(svc.messages[0].deliveryState).toBe('expired');
  });

  it('removes Message on dm-deleted matching channel + id', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);

    svc.messages = [
      {
        id: 'cid-to-delete',
        messageId: 'mid-del',
        sender: { address: 'self', displayName: 'You' },
        text: 'goodbye',
        timestamp: Date.now(),
        media: [],
        priority: 'standard',
        channel: 'aabbccdd',
        deliveryState: 'expired',
      },
      {
        id: 'cid-keep',
        sender: { address: 'peer', displayName: 'P' },
        text: 'keep me',
        timestamp: Date.now(),
        media: [],
        priority: 'standard',
        channel: 'aabbccdd',
      },
    ];

    emit('dm-deleted', {
      messageId: 'mid-del',
      spaceId: 'aabbccdd',
      messageCid: 'cid-to-delete',
    });

    expect(svc.messages.map((m) => m.id)).toEqual(['cid-keep']);
  });

  it('dm-deleted only removes from the matching channel', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);

    // Same id in two different channels — only the matching channel
    // should drop the message (defensive: id collisions across channels
    // are theoretically possible since id is a content CID).
    svc.messages = [
      {
        id: 'shared-cid',
        sender: { address: 'p', displayName: 'P' },
        text: 'a',
        timestamp: 1,
        media: [],
        priority: 'standard',
        channel: 'space-A',
      },
      {
        id: 'shared-cid',
        sender: { address: 'p', displayName: 'P' },
        text: 'b',
        timestamp: 2,
        media: [],
        priority: 'standard',
        channel: 'space-B',
      },
    ];

    emit('dm-deleted', {
      messageId: 'mid-x',
      spaceId: 'space-A',
      messageCid: 'shared-cid',
    });

    expect(svc.messages).toHaveLength(1);
    expect(svc.messages[0].channel).toBe('space-B');
  });

  it('dm-delivered with unknown messageId is a no-op', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);

    svc.messages = [{
      id: 'optimistic-id',
      messageId: 'mid1',
      sender: { address: 'self', displayName: 'You' },
      text: 'pending',
      timestamp: Date.now(),
      media: [],
      priority: 'standard',
      channel: 'aabbccdd',
      deliveryState: 'sending',
    }];

    emit('dm-delivered', { messageId: 'unknown', recipient: 'bob' });

    expect(svc.messages[0].deliveryState).toBe('sending');
  });
});
