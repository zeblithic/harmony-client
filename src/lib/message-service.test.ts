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

  // ZEB-560: like NavService/VineService, a shipped/alpha build must not seed
  // mock messages. The constructor gates seeding on `import.meta.env.DEV`
  // (true under vitest, false in a `vite build`); these pin both modes.
  it('does NOT seed mock messages when seedMockData is false (shipped/alpha build)', () => {
    const prod = new MessageService({ seedMockData: false });
    expect(prod.messages).toEqual([]);
  });

  it('seeds mock messages when seedMockData is true (dev/browser)', () => {
    const dev = new MessageService({ seedMockData: true });
    expect(dev.messages.length).toBe(mockMessages.length);
  });

  it('populates seenIds from incoming events (dedup)', async () => {
    // ZEB-209: mock IDs are cleared on connectAdapter, so post-connect
    // dedup must be tested with a real (network-arrived) ID, not a mock one.
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    const wire = { id: 'real-id-1', senderAddress: 'x', senderName: 'X', channel: 'c', hub: 'h', text: 'first', timestamp: 1, priority: 'standard' } satisfies ChannelMessageEvent;
    emit('message-received', wire);
    // Emit the same id again — must be deduped.
    emit('message-received', { ...wire, text: 'dup' });
    expect(svc.messages.length).toBe(1);
    expect(svc.messages[0].text).toBe('first');
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
    // ZEB-209: mock messages are cleared on connectAdapter; the first
    // real message becomes the only message in the list.
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    const wire: ChannelMessageEvent = {
      id: 'net-1', senderAddress: 'abc123', senderName: 'Peer',
      channel: 'general', hub: 'main', text: 'hello', timestamp: Date.now(), priority: 'standard',
    };
    emit('message-received', wire);
    expect(svc.messages.length).toBe(1);
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
    // ZEB-209: connectAdapter fires onChange once for the clear-on-connect,
    // then the incoming message fires it again. Set onChange before
    // connectAdapter so both calls are counted.
    const { adapter, emit } = createMockAdapter();
    svc.onChange = vi.fn();
    await svc.connectAdapter(adapter);
    emit('message-received', {
      id: 'notify-1', senderAddress: 'x', senderName: 'X',
      channel: 'c', hub: 'h', text: 'hi', timestamp: 1, priority: 'standard',
    } satisfies ChannelMessageEvent);
    // Called at least twice: once for the mock-clear, once for the new message.
    expect(svc.onChange).toHaveBeenCalledTimes(2);
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

  // ── ZEB-209: clear-on-connect ─────────────────────────────────

  it('clears mock-seeded messages on connectAdapter (ZEB-209)', async () => {
    const svc = new MessageService();
    // Sanity: constructor seeds from mockMessages so the UI is never empty
    // in browser/dev mode (no adapter connects).
    expect(svc.messages.length).toBeGreaterThan(0);
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    expect(svc.messages).toEqual([]);
  });

  it('fires onChange once after clearing mocks (ZEB-209)', async () => {
    const svc = new MessageService();
    let calls = 0;
    svc.onChange = () => { calls++; };
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    // At least one onChange — listener setup is async but the clear is
    // synchronous so the post-clear notification fires before any event.
    expect(calls).toBeGreaterThanOrEqual(1);
  });

  it('no longer dedupes events whose id collided with a former mock (ZEB-209)', async () => {
    const svc = new MessageService();
    // Pick any mock id — it was seeded into seenIds in the constructor.
    const collidingId = svc.messages[0]!.id;
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    // A real event with the same id should now be accepted (post-clear
    // seenIds no longer contains the mock id).
    emit('message-received', {
      id: collidingId,
      senderAddress: 'aa'.repeat(32),
      senderName: 'Real Sender',
      channel: 'channel-1',
      hub: 'hub-1',
      text: 'real',
      timestamp: 1700000000000,
      priority: 'standard',
    } satisfies ChannelMessageEvent);
    expect(svc.messages.find((m) => m.id === collidingId)?.text).toBe('real');
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
    // Fix A from PR #81 review: top-level DMs use activeHub='' in
    // App.svelte's feed filter; Message.hub must be '' (not undefined)
    // so the filter matches and the bubble renders.
    expect(incoming[0].hub).toBe('');
  });

  it('maps dm-received from self to the "self" sentinel sender', async () => {
    const { adapter, emit } = createMockAdapter();
    svc.ownAddress = 'my-own-hex';
    await svc.connectAdapter(adapter);

    emit('dm-received', {
      spaceId: 'aabbccdd',
      messageCid: 'self-cid',
      from: 'my-own-hex', // backend echoed our own DM back via the inbox path
      sentAt: 1,
      receivedAt: 2,
      body: hexEncode('echo'),
      mimeType: 'text/plain',
    });

    const msg = svc.messages.find((m) => m.id === 'self-cid')!;
    // Fix D from PR #81 review: self-sender uses the 'self' sentinel
    // (mirrors the channel-publish convention; downstream classification
    // and TextFeed.isSelf both key off 'self').
    expect(msg.sender.address).toBe('self');
    expect(msg.sender.displayName).toBe('You');
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

  it('onDmReceived hook fires post-dedup with the raw payload (ZEB-666)', async () => {
    const { adapter, emit } = createMockAdapter();
    const hook = vi.fn();
    svc.onDmReceived = hook;
    await svc.connectAdapter(adapter);

    const payload = {
      spaceId: 'aabbccdd',
      messageCid: 'cid-1',
      from: 'peer-hex',
      sentAt: 1,
      receivedAt: 2,
      body: hexEncode('hi'),
      mimeType: 'text/plain',
    };
    emit('dm-received', payload);
    emit('dm-received', payload); // duplicate delivery → deduped

    expect(hook).toHaveBeenCalledTimes(1);
    expect(hook).toHaveBeenCalledWith(expect.objectContaining({
      spaceId: 'aabbccdd',
      messageCid: 'cid-1',
      from: 'peer-hex',
      receivedAt: 2,
    }));
  });

  it('onDmReceived hook also fires for self-echoed DMs (service filters self itself)', async () => {
    const { adapter, emit } = createMockAdapter();
    const hook = vi.fn();
    svc.onDmReceived = hook;
    svc.ownAddress = 'my-own-hex';
    await svc.connectAdapter(adapter);

    emit('dm-received', {
      spaceId: 'aabbccdd',
      messageCid: 'self-cid-2',
      from: 'my-own-hex',
      sentAt: 1,
      receivedAt: 2,
      body: hexEncode('echo'),
      mimeType: 'text/plain',
    });

    expect(hook).toHaveBeenCalledTimes(1);
  });

  it('transitions self-Message to delivered on dm-delivered (ZEB-231)', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);

    svc.messages = [{
      id: 'cid-delivered',
      messageId: 'mid1',
      sender: { address: 'self', displayName: 'You' },
      text: 'pending',
      timestamp: Date.now(),
      media: [],
      priority: 'standard',
      channel: 'aabbccdd',
      deliveryState: 'sending',
    }];

    emit('dm-delivered', {
      spaceId: 'aabbccdd',
      messageCid: 'cid-delivered',
      recipientOwnerAddr: 'bob-hex-address',
    });

    expect(svc.messages[0].deliveryState).toBe('delivered');
  });

  it('transitions self-Message to expired on dm-expired (ZEB-231)', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);

    svc.messages = [{
      id: 'cid-expired',
      messageId: 'mid-exp',
      sender: { address: 'self', displayName: 'You' },
      text: 'pending',
      timestamp: Date.now(),
      media: [],
      priority: 'standard',
      channel: 'aabbccdd',
      deliveryState: 'sending',
    }];

    emit('dm-expired', {
      spaceId: 'aabbccdd',
      messageCid: 'cid-expired',
    });

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

  it('dm-deleted matches by messageId for pre-swap optimistic placeholder', async () => {
    // Fix C from PR #81 review: a delete that arrives before
    // replaceOptimisticId fires — or for a placeholder that failed to
    // swap — must still prune the bubble. Match by messageId in
    // addition to id (messageCid) so both shapes are covered.
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);

    svc.messages = [
      {
        id: 'optimistic-uuid', // not yet swapped to messageCid
        messageId: 'mid-pre-swap',
        sender: { address: 'self', displayName: 'You' },
        text: 'pending',
        timestamp: Date.now(),
        media: [],
        priority: 'standard',
        channel: 'aabbccdd',
        deliveryState: 'sending',
      },
    ];

    emit('dm-deleted', {
      messageId: 'mid-pre-swap',
      spaceId: 'aabbccdd',
      messageCid: 'cid-final', // doesn't match Message.id (still optimistic)
    });

    expect(svc.messages).toHaveLength(0);
  });

  it('dm-delivered with unknown (spaceId, messageCid) is a no-op (ZEB-231)', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);

    svc.messages = [{
      id: 'cid-known',
      messageId: 'mid1',
      sender: { address: 'self', displayName: 'You' },
      text: 'pending',
      timestamp: Date.now(),
      media: [],
      priority: 'standard',
      channel: 'aabbccdd',
      deliveryState: 'sending',
    }];

    emit('dm-delivered', {
      spaceId: 'aabbccdd',
      messageCid: 'cid-unknown',
      recipientOwnerAddr: 'bob',
    });

    expect(svc.messages[0].deliveryState).toBe('sending');
  });
});

// ── loadDmThread (Phase 4 — ZEB-228, Task 10) ────────────────────────
//
// Cold-start scrollback: when a user opens a DM Space for the first
// time after launch, the UI calls loadDmThread to fetch decrypted
// history via the read_dm_thread IPC. The IPC returns newest-first;
// MessageService prepends them in oldest-first order to match the UI's
// scroll-to-bottom display contract. Pagination uses a per-channel
// before_hlc cursor (oldest received_at from the prior page).

describe('MessageService loadDmThread', () => {
  let svc: MessageService;

  beforeEach(() => {
    svc = new MessageService();
    svc.messages = [];
  });

  function hexEncode(s: string): string {
    return Array.from(new TextEncoder().encode(s))
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');
  }

  it('fetches read_dm_thread IPC and populates messages oldest-first', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue([
      { messageCid: 'cid3', from: 'self-hex', sentAt: 3000, receivedAt: 3001, cursor: '3001:0:dev', body: hexEncode('newest'), mimeType: 'text/plain', isSelfOutbound: true, deliveryState: 'delivered' },
      { messageCid: 'cid2', from: 'bob-hex', sentAt: 2000, receivedAt: 2001, cursor: '2001:0:dev', body: hexEncode('mid'), mimeType: 'text/plain', isSelfOutbound: false },
      { messageCid: 'cid1', from: 'self-hex', sentAt: 1000, receivedAt: 1001, cursor: '1001:0:dev', body: hexEncode('oldest'), mimeType: 'text/plain', isSelfOutbound: true, deliveryState: 'delivered' },
    ]);
    await svc.connectAdapter(adapter);
    await svc.loadDmThread('aabbcc');

    const msgs = svc.messages.filter((m) => m.channel === 'aabbcc');
    expect(msgs).toHaveLength(3);
    // Display order: oldest first (UI scrolls to bottom).
    expect(msgs[0].text).toBe('oldest');
    expect(msgs[1].text).toBe('mid');
    expect(msgs[2].text).toBe('newest');
    // Self-sent historical messages reflect backend's per-entry delivery_status.
    expect(msgs[0].deliveryState).toBe('delivered');
    expect(msgs[2].deliveryState).toBe('delivered');
    // Received messages have no deliveryState.
    expect(msgs[1].deliveryState).toBeUndefined();
    // Fix A from PR #81 review: hub must be '' so the (channel && hub)
    // feed filter matches activeHub='' for top-level DMs.
    expect(msgs.every((m) => m.hub === '')).toBe(true);
  });

  it('honors backend per-entry deliveryState (Fix E from PR #81 review)', async () => {
    // Backend (post-b58a15b) ships outbox.delivery_status on each
    // self-outbound entry: 'sending' (still in-flight at restart),
    // 'expired' (30-day TTL ran out), 'failed', etc. The frontend used
    // to hardcode 'delivered' for all self-outbound history.
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue([
      { messageCid: 'cid-stuck', from: 'self', sentAt: 3000, receivedAt: 3001, body: hexEncode('stuck'), mimeType: 'text/plain', isSelfOutbound: true, deliveryState: 'sending' },
      { messageCid: 'cid-exp', from: 'self', sentAt: 2000, receivedAt: 2001, body: hexEncode('gone'), mimeType: 'text/plain', isSelfOutbound: true, deliveryState: 'expired' },
    ]);
    await svc.connectAdapter(adapter);
    await svc.loadDmThread('space-xyz');

    const msgs = svc.messages.filter((m) => m.channel === 'space-xyz');
    expect(msgs.find((m) => m.id === 'cid-stuck')!.deliveryState).toBe('sending');
    expect(msgs.find((m) => m.id === 'cid-exp')!.deliveryState).toBe('expired');
  });

  it('uses "self" sentinel for self-outbound scrollback entries (Fix D)', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue([
      { messageCid: 'cid-self', from: 'my-own-hex', sentAt: 1000, receivedAt: 1001, body: hexEncode('mine'), mimeType: 'text/plain', isSelfOutbound: true, deliveryState: 'delivered' },
    ]);
    await svc.connectAdapter(adapter);
    await svc.loadDmThread('space-self');

    const msg = svc.messages.find((m) => m.id === 'cid-self')!;
    expect(msg.sender.address).toBe('self');
    expect(msg.sender.displayName).toBe('You');
  });

  it('passes spaceId, limit, and undefined cursor on first call', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue([]);
    await svc.connectAdapter(adapter);
    await svc.loadDmThread('aabbcc');

    expect(adapter.invoke).toHaveBeenCalledWith('read_dm_thread', {
      spaceId: 'aabbcc',
      limit: 50,
      beforeHlc: undefined,
    });
  });

  it('tracks before_hlc cursor for pagination', async () => {
    // PR #81 round 3: loadDmThread now first-visit-guards. Repeated calls
    // for the same SpaceId are no-ops. The test asserts the cursor is
    // STORED (so a future explicit-backfill caller can use it) but NOT
    // re-invoked. Test renamed accordingly.
    // ZEB-244: the stored cursor is now the oldest entry's opaque `cursor`
    // token (full-HLC string), not its bare receivedAt wall_ms.
    const { adapter } = createMockAdapter();
    const invokeMock = adapter.invoke as ReturnType<typeof vi.fn>;
    invokeMock.mockResolvedValueOnce([
      { messageCid: 'cid3', from: 'self-hex', sentAt: 3000, receivedAt: 3001, cursor: '3001:0:dev', body: hexEncode('p1-newest'), mimeType: 'text/plain', isSelfOutbound: true },
      { messageCid: 'cid2', from: 'bob-hex', sentAt: 2000, receivedAt: 2001, cursor: '2001:0:dev', body: hexEncode('p1-oldest'), mimeType: 'text/plain', isSelfOutbound: false },
    ]);
    await svc.connectAdapter(adapter);

    await svc.loadDmThread('aabbcc', 2);
    await svc.loadDmThread('aabbcc', 2);
    await svc.loadDmThread('aabbcc', 2);

    // First call invokes read_dm_thread; subsequent calls are no-ops.
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith('read_dm_thread', {
      spaceId: 'aabbcc',
      limit: 2,
      beforeHlc: undefined, // first call has no cursor
    });
  });

  it('first-visit guards: empty-page response still marks SpaceId loaded', async () => {
    const { adapter } = createMockAdapter();
    const invokeMock = adapter.invoke as ReturnType<typeof vi.fn>;
    invokeMock.mockResolvedValueOnce([]); // start-of-thread / empty inbox
    await svc.connectAdapter(adapter);

    await svc.loadDmThread('coldspace', 50);
    await svc.loadDmThread('coldspace', 50); // should not re-fetch

    expect(invokeMock).toHaveBeenCalledTimes(1);
  });

  it('first-visit guard is per-spaceId: different spaces still load independently', async () => {
    const { adapter } = createMockAdapter();
    const invokeMock = adapter.invoke as ReturnType<typeof vi.fn>;
    invokeMock
      .mockResolvedValueOnce([
        { messageCid: 'a1', from: 'self-hex', sentAt: 1, receivedAt: 1, body: hexEncode('a'), mimeType: 'text/plain', isSelfOutbound: true },
      ])
      .mockResolvedValueOnce([
        { messageCid: 'b1', from: 'self-hex', sentAt: 2, receivedAt: 2, body: hexEncode('b'), mimeType: 'text/plain', isSelfOutbound: true },
      ]);
    await svc.connectAdapter(adapter);

    await svc.loadDmThread('space-a');
    await svc.loadDmThread('space-b');
    await svc.loadDmThread('space-a'); // re-visit space-a — guarded
    await svc.loadDmThread('space-b'); // re-visit space-b — guarded

    expect(invokeMock).toHaveBeenCalledTimes(2);
    expect(invokeMock).toHaveBeenNthCalledWith(1, 'read_dm_thread', expect.objectContaining({ spaceId: 'space-a' }));
    expect(invokeMock).toHaveBeenNthCalledWith(2, 'read_dm_thread', expect.objectContaining({ spaceId: 'space-b' }));
  });

  it('surfaces backend messageId on self scrollback entries (Cursor PR #81 round 2 fix)', async () => {
    const { adapter } = createMockAdapter();
    const invokeMock = adapter.invoke as ReturnType<typeof vi.fn>;
    invokeMock.mockResolvedValueOnce([
      // self entry with surviving outbox row → carries messageId
      { messageCid: 'cidA', from: 'self-hex', sentAt: 100, receivedAt: 101, body: hexEncode('hi'), mimeType: 'text/plain', isSelfOutbound: true, deliveryState: 'sending', messageId: 'mid-stuck' },
      // self entry post-Complete-GC → no messageId
      { messageCid: 'cidB', from: 'self-hex', sentAt: 50, receivedAt: 51, body: hexEncode('done'), mimeType: 'text/plain', isSelfOutbound: true, deliveryState: 'delivered' },
      // received entry → never has messageId
      { messageCid: 'cidC', from: 'bob-hex', sentAt: 10, receivedAt: 11, body: hexEncode('y'), mimeType: 'text/plain', isSelfOutbound: false },
    ]);
    await svc.connectAdapter(adapter);
    await svc.loadDmThread('s1');

    const stuck = svc.messages.find((m) => m.id === 'cidA');
    const delivered = svc.messages.find((m) => m.id === 'cidB');
    const received = svc.messages.find((m) => m.id === 'cidC');
    expect(stuck?.messageId).toBe('mid-stuck');
    expect(delivered?.messageId).toBeUndefined();
    expect(received?.messageId).toBeUndefined();
  });

  it('returns early when results are empty (no cursor update, no onChange)', async () => {
    // ZEB-209: set onChange AFTER connectAdapter so the clear-on-connect
    // notification doesn't pollute the loadDmThread onChange assertion.
    // The test intent is: loadDmThread with empty results must NOT call onChange.
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue([]);
    await svc.connectAdapter(adapter);
    svc.onChange = vi.fn();
    await svc.loadDmThread('aabbcc');

    expect(svc.messages).toHaveLength(0);
    expect(svc.onChange).not.toHaveBeenCalled();
  });

  it('skips messages already in seenIds (race with dm-received)', async () => {
    const { adapter, emit } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue([
      { messageCid: 'cid-shared', from: 'bob-hex', sentAt: 2000, receivedAt: 2001, body: hexEncode('via-thread'), mimeType: 'text/plain', isSelfOutbound: false },
    ]);
    await svc.connectAdapter(adapter);

    // Simulate dm-received arriving before loadDmThread completes:
    emit('dm-received', {
      spaceId: 'aabbcc',
      messageCid: 'cid-shared',
      from: 'bob-hex',
      sentAt: 2000,
      receivedAt: 2001,
      body: hexEncode('via-event'),
      mimeType: 'text/plain',
    });
    expect(svc.messages).toHaveLength(1);

    await svc.loadDmThread('aabbcc');

    // Still only one copy — loadDmThread saw it in seenIds and skipped.
    expect(svc.messages.filter((m) => m.id === 'cid-shared')).toHaveLength(1);
    expect(svc.messages[0].text).toBe('via-event');
  });

  it('prepends history before existing messages', async () => {
    const { adapter, emit } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue([
      { messageCid: 'old1', from: 'bob-hex', sentAt: 1000, receivedAt: 1001, body: hexEncode('history'), mimeType: 'text/plain', isSelfOutbound: false },
    ]);
    await svc.connectAdapter(adapter);

    emit('dm-received', {
      spaceId: 'aabbcc',
      messageCid: 'live1',
      from: 'bob-hex',
      sentAt: 5000,
      receivedAt: 5001,
      body: hexEncode('live'),
      mimeType: 'text/plain',
    });

    await svc.loadDmThread('aabbcc');

    expect(svc.messages.map((m) => m.id)).toEqual(['old1', 'live1']);
  });

  it('calls onChange after merging results', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue([
      { messageCid: 'cid1', from: 'bob-hex', sentAt: 1000, receivedAt: 1001, body: hexEncode('hi'), mimeType: 'text/plain', isSelfOutbound: false },
    ]);
    await svc.connectAdapter(adapter);
    svc.onChange = vi.fn();
    await svc.loadDmThread('aabbcc');
    expect(svc.onChange).toHaveBeenCalledOnce();
  });

  it('no-op when adapter is not connected', async () => {
    svc.onChange = vi.fn();
    await svc.loadDmThread('aabbcc');
    expect(svc.messages).toHaveLength(0);
    expect(svc.onChange).not.toHaveBeenCalled();
  });
});

// ── Optimistic-UI helpers (Phase 4 — ZEB-228, PR #81 review) ─────────
//
// Fix C from PR #81 review: send_dm IPC now returns
// { messageId, messageCid }. replaceOptimisticId takes 3 args — the
// optimisticId, the messageCid (becomes Message.id, stable across
// scrollback / dm-received), and the messageId (kept as
// Message.messageId for lifecycle correlation only).

describe('MessageService optimistic helpers', () => {
  let svc: MessageService;

  beforeEach(() => {
    svc = new MessageService();
    svc.messages = [];
  });

  it('pushOptimistic appends a Message and tracks its id', () => {
    svc.onChange = vi.fn();
    svc.pushOptimistic({
      id: 'opt-1',
      sender: { address: 'self', displayName: 'You' },
      text: 'hi',
      timestamp: 1,
      media: [],
      priority: 'standard',
      channel: 'space-1',
      hub: '',
      deliveryState: 'sending',
    });

    expect(svc.messages).toHaveLength(1);
    expect(svc.onChange).toHaveBeenCalledOnce();
  });

  it('replaceOptimisticId swaps id to messageCid and attaches messageId', () => {
    svc.pushOptimistic({
      id: 'opt-1',
      sender: { address: 'self', displayName: 'You' },
      text: 'hi',
      timestamp: 1,
      media: [],
      priority: 'standard',
      channel: 'space-1',
      hub: '',
      deliveryState: 'sending',
    });

    svc.replaceOptimisticId('opt-1', 'cid-final', 'mid-outbox');

    const msg = svc.messages[0];
    expect(msg.id).toBe('cid-final');
    expect(msg.messageId).toBe('mid-outbox');
  });

  it('replaceOptimisticId drops optimistic id from seenIds, adds messageCid', async () => {
    // Verify dedup behavior: after the swap, a dm-received echo with
    // the messageCid must be deduped (we already have the bubble).
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    svc.pushOptimistic({
      id: 'opt-1',
      sender: { address: 'self', displayName: 'You' },
      text: 'hi',
      timestamp: 1,
      media: [],
      priority: 'standard',
      channel: 'space-1',
      hub: '',
      deliveryState: 'sending',
    });
    svc.replaceOptimisticId('opt-1', 'cid-final', 'mid-outbox');

    function hexEncode(s: string): string {
      return Array.from(new TextEncoder().encode(s))
        .map((b) => b.toString(16).padStart(2, '0'))
        .join('');
    }
    emit('dm-received', {
      spaceId: 'space-1',
      messageCid: 'cid-final',
      from: 'self-hex',
      sentAt: 1,
      receivedAt: 2,
      body: hexEncode('hi'),
      mimeType: 'text/plain',
    });

    // Echo deduped — still only one Message with id 'cid-final'.
    expect(svc.messages.filter((m) => m.id === 'cid-final')).toHaveLength(1);
  });

  it('markFailed flags the optimistic Message without removing it', () => {
    svc.pushOptimistic({
      id: 'opt-fail',
      sender: { address: 'self', displayName: 'You' },
      text: 'doomed',
      timestamp: 1,
      media: [],
      priority: 'standard',
      channel: 'space-1',
      hub: '',
      deliveryState: 'sending',
    });

    svc.markFailed('opt-fail', 'send_dm rejected');

    expect(svc.messages).toHaveLength(1);
    expect(svc.messages[0].deliveryState).toBe('failed');
  });
});

// ── ZEB-337: self-authored messages must render the configured display
// name, not the hardcoded literal "You". `ownDisplayName` is kept in sync
// with the user's profile by App.svelte; the self-echo mapping sites must
// honour it instead of stamping "You". Each test sets a distinctive name
// so a regression to the literal "You" fails loudly.
describe('MessageService — ZEB-337: self messages use configured ownDisplayName', () => {
  let svc: MessageService;

  beforeEach(() => {
    svc = new MessageService();
  });

  function hexEncode(s: string): string {
    return Array.from(new TextEncoder().encode(s))
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');
  }

  it('labels a self-echoed channel message with ownDisplayName, not "You"', async () => {
    const { adapter, emit } = createMockAdapter();
    svc.ownAddress = 'myaddr';
    svc.ownDisplayName = 'Jake Englund';
    await svc.connectAdapter(adapter);
    emit('message-received', {
      id: 'self-echo-1', senderAddress: 'myaddr', senderName: 'whatever',
      channel: 'c', hub: 'h', text: 'echo', timestamp: 1, priority: 'standard',
    } satisfies ChannelMessageEvent);
    const msg = svc.messages.find((m) => m.id === 'self-echo-1')!;
    expect(msg.sender.address).toBe('self');
    expect(msg.sender.displayName).toBe('Jake Englund');
  });

  it('labels a self-sent DM (dm-received echo) with ownDisplayName', async () => {
    const { adapter, emit } = createMockAdapter();
    svc.ownAddress = 'myaddr';
    svc.ownDisplayName = 'Jake Englund';
    await svc.connectAdapter(adapter);
    emit('dm-received', {
      spaceId: 'space-1', messageCid: 'dm-self-1', from: 'myaddr',
      sentAt: 1, receivedAt: 2, body: hexEncode('hi'), mimeType: 'text/plain',
    });
    const msg = svc.messages.find((m) => m.id === 'dm-self-1')!;
    expect(msg.sender.address).toBe('self');
    expect(msg.sender.displayName).toBe('Jake Englund');
  });

  it('labels the offline-fallback self message with ownDisplayName', async () => {
    svc.ownDisplayName = 'Jake Englund';
    await svc.send('offline', 'standard', 'general', 'main');
    const msg = svc.messages.at(-1)!;
    expect(msg.sender.address).toBe('self');
    expect(msg.sender.displayName).toBe('Jake Englund');
  });

  it('labels self-outbound scrollback (loadDmThread) with ownDisplayName', async () => {
    const { adapter } = createMockAdapter();
    svc.ownDisplayName = 'Jake Englund';
    await svc.connectAdapter(adapter);
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue([
      {
        messageCid: 'sb-self-1', from: 'myaddr', sentAt: 1, receivedAt: 2,
        body: hexEncode('history'), mimeType: 'text/plain', isSelfOutbound: true,
      },
    ]);
    await svc.loadDmThread('space-1');
    const msg = svc.messages.find((m) => m.id === 'sb-self-1')!;
    expect(msg.sender.address).toBe('self');
    expect(msg.sender.displayName).toBe('Jake Englund');
  });

  // CodeAnt (PR #180): a whitespace-only display name (e.g. a legacy untrimmed
  // profile) must not render a blank self author label — fall back to "You".
  it('falls back to "You" when ownDisplayName is whitespace-only', async () => {
    const { adapter, emit } = createMockAdapter();
    svc.ownAddress = 'myaddr';
    svc.ownDisplayName = '   ';
    await svc.connectAdapter(adapter);
    emit('message-received', {
      id: 'self-echo-ws', senderAddress: 'myaddr', senderName: 'whatever',
      channel: 'c', hub: 'h', text: 'echo', timestamp: 1, priority: 'standard',
    } satisfies ChannelMessageEvent);
    const msg = svc.messages.find((m) => m.id === 'self-echo-ws')!;
    expect(msg.sender.address).toBe('self');
    expect(msg.sender.displayName).toBe('You');
  });
});

// ZEB-357 — call-event DM messages: both ingestion points (live dm-received
// and read_dm_thread scrollback) must parse the payload onto Message.callEvent
// and replace `text` with the direction-aware system-line label, so previews /
// search / old render paths degrade to something human-readable.
describe('MessageService call-event ingestion (ZEB-357)', () => {
  let svc: MessageService;

  beforeEach(() => {
    svc = new MessageService();
    svc.messages = [];
  });

  function hexEncode(s: string): string {
    return Array.from(new TextEncoder().encode(s))
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');
  }

  const CALL_MIME = 'application/x-harmony-call-event+json';
  const missedBody = JSON.stringify({ v: 1, callId: 'ab'.repeat(16), outcome: 'no_answer' });

  it('dm-received from the peer parses callEvent and labels it as the recipient', async () => {
    const { adapter, emit } = createMockAdapter();
    svc.ownAddress = 'my-own-hex';
    await svc.connectAdapter(adapter);
    emit('dm-received', {
      spaceId: 'aabbccdd', messageCid: 'call-cid-1', from: 'bob-hex',
      sentAt: 1, receivedAt: 2, body: hexEncode(missedBody), mimeType: CALL_MIME,
    });
    const msg = svc.messages.find((m) => m.id === 'call-cid-1')!;
    expect(msg.callEvent).toEqual({ v: 1, callId: 'ab'.repeat(16), outcome: 'no_answer' });
    expect(msg.text).toBe('Missed call');
  });

  it('dm-received authored by self labels it as the author', async () => {
    const { adapter, emit } = createMockAdapter();
    svc.ownAddress = 'my-own-hex';
    await svc.connectAdapter(adapter);
    emit('dm-received', {
      spaceId: 'aabbccdd', messageCid: 'call-cid-2', from: 'my-own-hex',
      sentAt: 1, receivedAt: 2, body: hexEncode(missedBody), mimeType: CALL_MIME,
    });
    const msg = svc.messages.find((m) => m.id === 'call-cid-2')!;
    expect(msg.callEvent?.outcome).toBe('no_answer');
    expect(msg.text).toBe('Call — no answer');
  });

  it('a malformed call-event body falls back to plain text (no callEvent)', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    emit('dm-received', {
      spaceId: 'aabbccdd', messageCid: 'call-cid-3', from: 'bob-hex',
      sentAt: 1, receivedAt: 2, body: hexEncode('{broken'), mimeType: CALL_MIME,
    });
    const msg = svc.messages.find((m) => m.id === 'call-cid-3')!;
    expect(msg.callEvent).toBeUndefined();
    expect(msg.text).toBe('{broken');
  });

  it('scrollback parses callEvent with direction from isSelfOutbound', async () => {
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue([
      {
        messageCid: 'sb-call-peer', from: 'bob-hex', sentAt: 3, receivedAt: 4,
        body: hexEncode(missedBody), mimeType: CALL_MIME, isSelfOutbound: false,
      },
      {
        messageCid: 'sb-call-self', from: 'me', sentAt: 1, receivedAt: 2,
        body: hexEncode(missedBody), mimeType: CALL_MIME, isSelfOutbound: true,
      },
    ]);
    await svc.loadDmThread('space-1');
    const peer = svc.messages.find((m) => m.id === 'sb-call-peer')!;
    expect(peer.callEvent?.outcome).toBe('no_answer');
    expect(peer.text).toBe('Missed call');
    const self = svc.messages.find((m) => m.id === 'sb-call-self')!;
    expect(self.callEvent?.outcome).toBe('no_answer');
    expect(self.text).toBe('Call — no answer');
  });
});
