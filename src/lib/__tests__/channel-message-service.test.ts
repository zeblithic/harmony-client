import { describe, it, expect, vi, beforeEach } from 'vitest';
import { ChannelMessageService } from '../channel-message-service';
import type { TauriAdapter } from '../zenoh-service';

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

describe('ChannelMessageService', () => {
  let service: ChannelMessageService;
  let adapter: ReturnType<typeof makeAdapter>;

  beforeEach(() => {
    service = new ChannelMessageService();
    adapter = makeAdapter();
  });

  it('connectAdapter installs channel-message-received + channel-backfill-progress listeners', async () => {
    await service.connectAdapter(adapter);
    expect(adapter.listeners.has('channel-message-received')).toBe(true);
    expect(adapter.listeners.has('channel-backfill-progress')).toBe(true);
  });

  it('postMessage invokes post_channel_message with camelCase args', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('aabb' + 'cc'.repeat(14));
    const msgId = await service.postMessage(
      'aa'.repeat(16),
      'bb'.repeat(16),
      'hello',
    );
    expect(adapter.invoke).toHaveBeenCalledWith('post_channel_message', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      body: Array.from(new TextEncoder().encode('hello')),
      replyTo: undefined,
      mentions: undefined,
      attachments: undefined,
    });
    expect(msgId).toBe('aabb' + 'cc'.repeat(14));
  });

  it('postMessage forwards replyTo when provided', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('mid');
    await service.postMessage('aa'.repeat(16), 'bb'.repeat(16), 'hi', 'cc'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledWith('post_channel_message', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      body: Array.from(new TextEncoder().encode('hi')),
      replyTo: 'cc'.repeat(16),
      mentions: undefined,
      attachments: undefined,
    });
  });

  it('postMessage forwards mentions when provided', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('mid');
    await service.postMessage(
      'aa'.repeat(16),
      'bb'.repeat(16),
      'hi',
      undefined,
      ['cc'.repeat(16), 'dd'.repeat(16)],
    );
    expect(adapter.invoke).toHaveBeenCalledWith('post_channel_message', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      body: Array.from(new TextEncoder().encode('hi')),
      replyTo: undefined,
      mentions: ['cc'.repeat(16), 'dd'.repeat(16)],
      attachments: undefined,
    });
  });

  it('postMessage sends empty mentions as undefined', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('mid');
    await service.postMessage('aa'.repeat(16), 'bb'.repeat(16), 'hi', undefined, []);
    expect(adapter.invoke).toHaveBeenCalledWith('post_channel_message', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      body: Array.from(new TextEncoder().encode('hi')),
      replyTo: undefined,
      mentions: undefined,
      attachments: undefined,
    });
  });

  it('postMessage forwards attachments when provided', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('mid');
    const att = [
      { cid: 'ab'.repeat(32), mime: 'text/plain', name: 'a.txt', size: 12, encrypted: true },
      { cid: 'cd'.repeat(32), mime: 'image/png', name: 'b.png', size: 99, encrypted: false },
    ];
    await service.postMessage('aa'.repeat(16), 'bb'.repeat(16), 'hi', undefined, undefined, att);
    expect(adapter.invoke).toHaveBeenCalledWith('post_channel_message', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      body: Array.from(new TextEncoder().encode('hi')),
      replyTo: undefined,
      mentions: undefined,
      attachments: att,
    });
  });

  it('postMessage sends empty attachments as undefined', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('mid');
    await service.postMessage('aa'.repeat(16), 'bb'.repeat(16), 'hi', undefined, undefined, []);
    expect(adapter.invoke).toHaveBeenCalledWith('post_channel_message', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      body: Array.from(new TextEncoder().encode('hi')),
      replyTo: undefined,
      mentions: undefined,
      attachments: undefined,
    });
  });

  it('ingestArtifact forwards args + defaults encrypt=true', async () => {
    await service.connectAdapter(adapter);
    const dto = { cid: 'ab'.repeat(32), mime: 'text/plain', name: 'l.txt', size: 42, encrypted: true };
    (adapter.invoke as any).mockResolvedValue(dto);
    const out = await service.ingestArtifact('aa'.repeat(16), '/tmp/l.txt', { name: 'l.txt', mime: 'text/plain' });
    expect(out).toEqual(dto);
    expect(adapter.invoke).toHaveBeenCalledWith('ingest_channel_artifact', {
      communityId: 'aa'.repeat(16),
      sourcePath: '/tmp/l.txt',
      name: 'l.txt',
      mime: 'text/plain',
      encrypt: true,
    });
  });

  it('ingestArtifact respects encrypt=false', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue({ cid: 'ab'.repeat(32), mime: 'text/plain', name: 'l.txt', size: 42, encrypted: false });
    await service.ingestArtifact('aa'.repeat(16), '/tmp/l.txt', { encrypt: false });
    expect(adapter.invoke).toHaveBeenCalledWith('ingest_channel_artifact', {
      communityId: 'aa'.repeat(16),
      sourcePath: '/tmp/l.txt',
      name: undefined,
      mime: undefined,
      encrypt: false,
    });
  });

  it('ingestArtifact normalizes a raw-string rejection into an Error', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockRejectedValue('artifact too large: 5MB (max 4MB)');
    await expect(
      service.ingestArtifact('aa'.repeat(16), '/tmp/l.txt'),
    ).rejects.toThrow('artifact too large: 5MB (max 4MB)');
  });

  it('downloadArtifact forwards channelId + cid and omits expectedSize', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(42);
    const att = { cid: 'ab'.repeat(32), mime: 'text/plain', name: 'l.txt', size: 42, encrypted: true };
    const n = await service.downloadArtifact('aa'.repeat(16), 'cc'.repeat(16), att, '/tmp/l.txt');
    expect(n).toBe(42);
    expect(adapter.invoke).toHaveBeenCalledWith('download_channel_artifact', {
      communityId: 'aa'.repeat(16),
      channelId: 'cc'.repeat(16),
      cid: att.cid,
      destPath: '/tmp/l.txt',
      maxBytes: undefined,
    });
  });

  it('downloadArtifact forwards maxBytes when provided', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(7);
    const att = { cid: 'ab'.repeat(32), mime: 'text/plain', name: 'l.txt', size: 7, encrypted: true };
    await service.downloadArtifact('aa'.repeat(16), 'cc'.repeat(16), att, '/tmp/l.txt', 1024);
    expect(adapter.invoke).toHaveBeenCalledWith('download_channel_artifact', {
      communityId: 'aa'.repeat(16),
      channelId: 'cc'.repeat(16),
      cid: att.cid,
      destPath: '/tmp/l.txt',
      maxBytes: 1024,
    });
  });

  it('downloadArtifact throws when adapter not connected', async () => {
    const att = { cid: 'ab'.repeat(32), mime: 'text/plain', name: 'l.txt', size: 42, encrypted: true };
    await expect(
      service.downloadArtifact('aa'.repeat(16), 'cc'.repeat(16), att, '/tmp/l.txt'),
    ).rejects.toThrow(/adapter not connected/);
  });

  it('previewArtifact invokes preview_channel_artifact and returns Uint8Array', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue([1, 2, 3]);
    const att = { cid: 'cc', mime: 'image/png', name: 'p.png', size: 3, encrypted: true };
    const bytes = await service.previewArtifact('00', '11', att, 4096);
    expect(adapter.invoke).toHaveBeenCalledWith('preview_channel_artifact', {
      communityId: '00',
      channelId: '11',
      cid: 'cc',
      maxBytes: 4096,
    });
    expect(bytes).toBeInstanceOf(Uint8Array);
    expect(Array.from(bytes)).toEqual([1, 2, 3]);
  });

  it('previewArtifact omits maxBytes when not provided', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue([9]);
    const att = { cid: 'cc', mime: 'text/plain', name: 'l.txt', size: 1, encrypted: false };
    await service.previewArtifact('00', '11', att);
    expect(adapter.invoke).toHaveBeenCalledWith('preview_channel_artifact', {
      communityId: '00',
      channelId: '11',
      cid: 'cc',
      maxBytes: undefined,
    });
  });

  it('previewArtifact normalizes a rejection to an Error with the message', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockRejectedValue('peer offline');
    const att = { cid: 'cc', mime: 'image/png', name: 'p.png', size: 3, encrypted: true };
    await expect(service.previewArtifact('00', '11', att)).rejects.toThrow('peer offline');
  });

  it('previewArtifact throws when adapter not connected', async () => {
    const att = { cid: 'cc', mime: 'image/png', name: 'p.png', size: 3, encrypted: true };
    await expect(
      service.previewArtifact('00', '11', att),
    ).rejects.toThrow(/adapter not connected/);
  });

  it('ingestArtifact throws when adapter not connected', async () => {
    await expect(
      service.ingestArtifact('aa'.repeat(16), '/tmp/l.txt'),
    ).rejects.toThrow(/adapter not connected/);
  });

  it('listMessages invokes list_channel_messages with limit + optional since', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue([]);
    await service.listMessages('aa'.repeat(16), 'bb'.repeat(16), undefined, 100);
    expect(adapter.invoke).toHaveBeenCalledWith('list_channel_messages', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      since: undefined,
      limit: 100,
    });
  });

  it('listMessages caches result into byChannel and notifies subscribers', async () => {
    await service.connectAdapter(adapter);
    const dto = {
      messageId: 'm1',
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      author: 'au',
      at: { wallMs: 100, logical: 0, deviceId: 'd' },
      body: Array.from(new TextEncoder().encode('hi')),
    };
    (adapter.invoke as any).mockResolvedValue([dto]);

    const cb = vi.fn();
    service.subscribeToChannel('aa'.repeat(16), 'bb'.repeat(16), cb);
    await service.listMessages('aa'.repeat(16), 'bb'.repeat(16), undefined, 100);
    expect(cb).toHaveBeenCalledWith(dto);

    const cached = service.getMessages('aa'.repeat(16), 'bb'.repeat(16));
    expect(cached).toHaveLength(1);
    expect(cached[0].messageId).toBe('m1');
  });

  it('channel-message-received event appends to per-channel cache + notifies', async () => {
    await service.connectAdapter(adapter);
    const cb = vi.fn();
    service.subscribeToChannel('aa'.repeat(16), 'bb'.repeat(16), cb);

    const handler = adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'm1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'au',
          at: { wallMs: 100, logical: 0, deviceId: 'd' },
          body: [104, 105],
        },
      },
    });

    expect(cb).toHaveBeenCalledTimes(1);
    expect(service.getMessages('aa'.repeat(16), 'bb'.repeat(16))).toHaveLength(1);
  });

  it('dedupes by messageId — second arrival of same id is ignored', async () => {
    await service.connectAdapter(adapter);
    const cb = vi.fn();
    service.subscribeToChannel('aa'.repeat(16), 'bb'.repeat(16), cb);

    const handler = adapter.listeners.get('channel-message-received')!;
    const payload = {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      message: {
        messageId: 'm1',
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        author: 'au',
        at: { wallMs: 100, logical: 0, deviceId: 'd' },
        body: [104],
      },
    };
    handler({ payload });
    handler({ payload });

    expect(cb).toHaveBeenCalledTimes(1);
    expect(service.getMessages('aa'.repeat(16), 'bb'.repeat(16))).toHaveLength(1);
  });

  it('keeps cache sorted by HLC ascending across out-of-order arrivals', async () => {
    await service.connectAdapter(adapter);
    const handler = adapter.listeners.get('channel-message-received')!;
    const cid = 'aa'.repeat(16);
    const chid = 'bb'.repeat(16);

    handler({
      payload: {
        communityId: cid,
        channelId: chid,
        message: {
          messageId: 'm200',
          communityId: cid,
          channelId: chid,
          author: 'au',
          at: { wallMs: 200, logical: 0, deviceId: 'd' },
          body: [],
        },
      },
    });
    handler({
      payload: {
        communityId: cid,
        channelId: chid,
        message: {
          messageId: 'm100',
          communityId: cid,
          channelId: chid,
          author: 'au',
          at: { wallMs: 100, logical: 0, deviceId: 'd' },
          body: [],
        },
      },
    });

    const cached = service.getMessages(cid, chid);
    expect(cached.map(m => m.at.wallMs)).toEqual([100, 200]);
  });

  it('subscribeToChannel returns working unsubscribe', async () => {
    await service.connectAdapter(adapter);
    const cb = vi.fn();
    const unsub = service.subscribeToChannel('aa'.repeat(16), 'bb'.repeat(16), cb);
    unsub();
    const handler = adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'm1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'a',
          at: { wallMs: 1, logical: 0, deviceId: 'd' },
          body: [],
        },
      },
    });
    expect(cb).not.toHaveBeenCalled();
  });

  it('requestBackfill invokes request_channel_backfill + sets in-flight gate', async () => {
    await service.connectAdapter(adapter);
    let resolveInvoke: () => void = () => {};
    (adapter.invoke as any).mockReturnValue(new Promise<void>(r => { resolveInvoke = () => r(); }));

    const p1 = service.requestBackfill('aa'.repeat(16), 'bb'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(1);

    // Second call while first is in-flight: no-op (single-in-flight gate).
    const p2 = service.requestBackfill('aa'.repeat(16), 'bb'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(1);

    resolveInvoke();
    await p1;
    await p2;

    // Gate is held until terminal progress (IPC is fire-and-forget on backend).
    // Fire a terminal progress event to release the gate.
    const progressHandler = adapter.listeners.get('channel-backfill-progress')!;
    progressHandler({ payload: { communityId: 'aa'.repeat(16), channelId: 'bb'.repeat(16), fetched: 5, totalEstimate: 5 } });

    // After the gate is released, a third call should fire a new IPC.
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.requestBackfill('aa'.repeat(16), 'bb'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(2);
  });

  it('in-flight gate is per (communityId, channelId)', async () => {
    await service.connectAdapter(adapter);
    const resolvers: Array<() => void> = [];
    (adapter.invoke as any).mockImplementation(() => new Promise<void>((r) => { resolvers.push(() => r()); }));

    const pA = service.requestBackfill('aa'.repeat(16), 'bb'.repeat(16));
    const pB = service.requestBackfill('aa'.repeat(16), 'cc'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(2);  // independent gates

    // Resolve both invoke promises so both pA and pB can settle.
    for (const r of resolvers) r();
    await pA;
    await pB;
  });

  it('channel-backfill-progress fires onBackfillProgress callback', async () => {
    await service.connectAdapter(adapter);
    const cb = vi.fn();
    service.onBackfillProgress = cb;

    const handler = adapter.listeners.get('channel-backfill-progress')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        fetched: 5,
        totalEstimate: 10,
      },
    });

    expect(cb).toHaveBeenCalledWith('aa'.repeat(16), 'bb'.repeat(16), 5, 10);
  });

  it('terminal channel-backfill-progress (fetched == totalEstimate) clears in-flight gate', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);

    await service.requestBackfill('aa'.repeat(16), 'bb'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(1);

    // Simulate non-terminal progress; gate stays active because IPC settled
    // already, but we want progress events to only flip the gate when
    // terminal. Use the in-flight check via a second invoke attempt:
    const handler = adapter.listeners.get('channel-backfill-progress')!;

    // The gate is still held after IPC resolves (fire-and-forget backend).
    // Re-arm the gate by firing a second backfill that hangs — but first
    // release the gate from the first call via terminal progress:
    handler({ payload: { communityId: 'aa'.repeat(16), channelId: 'bb'.repeat(16), fetched: 5, totalEstimate: 5 } });

    // Now re-arm the gate with a backfill that hangs (IPC doesn't resolve):
    let resolve2: () => void = () => {};
    (adapter.invoke as any).mockReturnValue(new Promise<void>(r => { resolve2 = () => r(); }));
    void service.requestBackfill('aa'.repeat(16), 'bb'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(2);

    // Mid-flight progress (non-terminal) does NOT release the gate:
    handler({ payload: { communityId: 'aa'.repeat(16), channelId: 'bb'.repeat(16), fetched: 3, totalEstimate: 5 } });
    void service.requestBackfill('aa'.repeat(16), 'bb'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(2);  // still gated

    // Terminal progress releases the gate:
    handler({ payload: { communityId: 'aa'.repeat(16), channelId: 'bb'.repeat(16), fetched: 5, totalEstimate: 5 } });
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.requestBackfill('aa'.repeat(16), 'bb'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(3);

    resolve2();
  });

  it('destroy unregisters all listeners and clears caches', async () => {
    await service.connectAdapter(adapter);
    const cb = vi.fn();
    service.subscribeToChannel('aa'.repeat(16), 'bb'.repeat(16), cb);

    service.destroy();
    expect(adapter.listeners.size).toBe(0);

    expect(service.getMessages('aa'.repeat(16), 'bb'.repeat(16))).toEqual([]);
  });

  it('IPC errors propagate from postMessage', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockRejectedValue(new Error('no engine for ...'));
    await expect(
      service.postMessage('aa'.repeat(16), 'bb'.repeat(16), 'hi'),
    ).rejects.toThrow('no engine for ...');
  });

  it('postMessage normalizes a raw-string IPC rejection into an Error', async () => {
    // Production IPC rejections are raw strings (the new mentions path can
    // reject with "too many mentions: N (max 64)"). postMessage must wrap
    // them so callers reading `.message` keep the detail.
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockRejectedValue('too many mentions: 65 (max 64)');
    await expect(
      service.postMessage('aa'.repeat(16), 'bb'.repeat(16), 'hi'),
    ).rejects.toThrow('too many mentions: 65 (max 64)');
  });

  it('postMessage throws when adapter not connected', async () => {
    await expect(
      service.postMessage('aa'.repeat(16), 'bb'.repeat(16), 'hi'),
    ).rejects.toThrow(/adapter not connected/);
  });
});

describe('ChannelMessageService reactions (ZEB-536 Spec 2)', () => {
  let service: ChannelMessageService;
  let adapter: ReturnType<typeof makeAdapter>;
  const CID = 'aa'.repeat(16);
  const CHID = 'bb'.repeat(16);
  const OWN = 'cc'.repeat(16);
  const OTHER = 'dd'.repeat(16);

  beforeEach(() => {
    service = new ChannelMessageService();
    adapter = makeAdapter();
  });

  // Seed one message ('m1') into the per-channel cache via listMessages.
  async function seedMessage(): Promise<void> {
    (adapter.invoke as any).mockResolvedValue([
      {
        messageId: 'm1',
        communityId: CID,
        channelId: CHID,
        author: OTHER,
        at: { wallMs: 100, logical: 0, deviceId: 'd' },
        body: [],
      },
    ]);
    await service.listMessages(CID, CHID, undefined, 100);
  }

  function fireReaction(over: Record<string, unknown> = {}): void {
    const handler = adapter.listeners.get('channel-reaction-received')!;
    handler({
      payload: {
        communityId: CID,
        channelId: CHID,
        messageId: 'm1',
        reactor: OTHER,
        emoji: '👍',
        add: true,
        at: { wallMs: 200, logical: 0, deviceId: 'd' },
        ...over,
      },
    });
  }

  it('connectAdapter installs the channel-reaction-received listener', async () => {
    await service.connectAdapter(adapter);
    expect(adapter.listeners.has('channel-reaction-received')).toBe(true);
  });

  it('applyReaction add creates a chip entry with count 1', async () => {
    await service.connectAdapter(adapter);
    await seedMessage();
    fireReaction();
    const msg = service.getMessages(CID, CHID)[0];
    expect(msg.reactions).toEqual([
      { emoji: '👍', count: 1, mine: false, reactors: [OTHER] },
    ]);
  });

  it('a second distinct reactor increments count to 2', async () => {
    await service.connectAdapter(adapter);
    await seedMessage();
    fireReaction({ reactor: OTHER });
    fireReaction({ reactor: OWN });
    const msg = service.getMessages(CID, CHID)[0];
    expect(msg.reactions?.[0].count).toBe(2);
    expect(msg.reactions?.[0].reactors).toEqual([OTHER, OWN]);
  });

  it('mine reflects selfOwnerId membership in reactors', async () => {
    service.selfOwnerId = OWN;
    await service.connectAdapter(adapter);
    await seedMessage();
    fireReaction({ reactor: OWN });
    const msg = service.getMessages(CID, CHID)[0];
    expect(msg.reactions?.[0].mine).toBe(true);
  });

  it('remove decrements and drops the entry at zero reactors', async () => {
    await service.connectAdapter(adapter);
    await seedMessage();
    fireReaction({ reactor: OTHER, add: true });
    fireReaction({ reactor: OTHER, add: false });
    const msg = service.getMessages(CID, CHID)[0];
    expect(msg.reactions).toEqual([]);
  });

  it('duplicate add (idempotent redelivery) does not double-count', async () => {
    await service.connectAdapter(adapter);
    await seedMessage();
    fireReaction({ reactor: OTHER, add: true });
    fireReaction({ reactor: OTHER, add: true });
    const msg = service.getMessages(CID, CHID)[0];
    expect(msg.reactions?.[0].count).toBe(1);
  });

  it('a reaction for an unloaded message is a no-op (no throw, no cache entry)', async () => {
    await service.connectAdapter(adapter);
    expect(() => fireReaction({ messageId: 'ghost' })).not.toThrow();
    expect(service.getMessages(CID, CHID)).toEqual([]);
  });

  it('applyReaction notifies channel subscribers (feed re-render hook)', async () => {
    await service.connectAdapter(adapter);
    await seedMessage();
    const cb = vi.fn();
    service.subscribeToChannel(CID, CHID, cb);
    fireReaction();
    expect(cb).toHaveBeenCalledTimes(1);
  });

  it('applyReaction does NOT fire onMessage (no spurious roster refetch)', async () => {
    await service.connectAdapter(adapter);
    await seedMessage();
    const onMessage = vi.fn();
    service.onMessage = onMessage;
    fireReaction();
    expect(onMessage).not.toHaveBeenCalled();
  });

  it('reactToMessage invokes set_message_reaction with camelCase args', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.reactToMessage(CID, CHID, 'm1', '👍', true);
    expect(adapter.invoke).toHaveBeenCalledWith('set_message_reaction', {
      communityId: CID,
      channelId: CHID,
      messageId: 'm1',
      emoji: '👍',
      add: true,
    });
  });

  it('reactToMessage omits customEmoji for a unicode reaction (byte-identical to before)', async () => {
    // Regression guard: the unicode path must NOT carry a customEmoji key at all,
    // so the backend's signed bytes are unchanged from pre-custom-emoji behavior.
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.reactToMessage(CID, CHID, 'm1', '👍', true);
    const [, args] = (adapter.invoke as any).mock.calls[0];
    expect(args).not.toHaveProperty('customEmoji');
    expect(args.customEmoji).toBeUndefined();
  });

  it('reactToMessage forwards a customEmoji descriptor for a custom reaction', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    const custom = { cid: 'ab'.repeat(32), mime: 'image/png', size: 1234 };
    await service.reactToMessage(CID, CHID, 'm1', '', true, custom);
    expect(adapter.invoke).toHaveBeenCalledWith('set_message_reaction', {
      communityId: CID,
      channelId: CHID,
      messageId: 'm1',
      emoji: '',
      add: true,
      customEmoji: custom,
    });
  });

  it('ingestEmojiBytes invokes ingest_channel_artifact_bytes with a number[] body + returns cid/size', async () => {
    await service.connectAdapter(adapter);
    const dto = { cid: 'cd'.repeat(32), mime: 'image/png', name: '', size: 256, encrypted: true };
    (adapter.invoke as any).mockResolvedValue(dto);
    const bytes = new Uint8Array([0x89, 0x50, 0x4e, 0x47]);
    const out = await service.ingestEmojiBytes(CID, bytes);
    expect(out).toEqual({ cid: dto.cid, size: dto.size });
    expect(adapter.invoke).toHaveBeenCalledWith('ingest_channel_artifact_bytes', {
      communityId: CID,
      bytes: [0x89, 0x50, 0x4e, 0x47],
      name: '',
      mime: 'image/png',
      encrypt: false,
    });
    // The bytes arg must be a plain Array (not a typed array) for IPC.
    const [, args] = (adapter.invoke as any).mock.calls[0];
    expect(Array.isArray(args.bytes)).toBe(true);
  });

  it('ingestEmojiBytes passes encrypt=true when caller opts into private', async () => {
    await service.connectAdapter(adapter);
    const bytes = new Uint8Array([1, 2, 3]);
    const dto = { cid: 'cd'.repeat(32), mime: 'image/png', name: '', size: 256, encrypted: true };
    (adapter.invoke as any).mockResolvedValue(dto);
    await service.ingestEmojiBytes(CID, bytes, true);
    expect(adapter.invoke).toHaveBeenCalledWith('ingest_channel_artifact_bytes', {
      communityId: CID,
      bytes: Array.from(bytes),
      name: '',
      mime: 'image/png',
      encrypt: true,
    });
  });

  it('ingestEmojiBytes normalizes a raw-string rejection into an Error', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockRejectedValue('artifact too large: 300KiB (max 256KiB)');
    await expect(
      service.ingestEmojiBytes(CID, new Uint8Array([1, 2, 3])),
    ).rejects.toThrow('artifact too large: 300KiB (max 256KiB)');
  });

  it('ingestEmojiBytes throws when the adapter is not connected', async () => {
    await expect(
      service.ingestEmojiBytes(CID, new Uint8Array([1])),
    ).rejects.toThrow(/adapter not connected/);
  });

  it('previewReactionEmoji invokes preview_reaction_emoji and returns a Uint8Array', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue([1, 2, 3]);
    const bytes = await service.previewReactionEmoji(CID, CHID, 'cc');
    expect(adapter.invoke).toHaveBeenCalledWith('preview_reaction_emoji', {
      communityId: CID,
      channelId: CHID,
      cid: 'cc',
    });
    expect(bytes).toBeInstanceOf(Uint8Array);
    expect(Array.from(bytes)).toEqual([1, 2, 3]);
  });

  it('previewReactionEmoji normalizes a raw-string rejection into an Error', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockRejectedValue('reaction emoji not authorized for this channel');
    await expect(
      service.previewReactionEmoji(CID, CHID, 'cc'),
    ).rejects.toThrow('reaction emoji not authorized for this channel');
  });

  it('previewReactionEmoji throws when the adapter is not connected', async () => {
    await expect(
      service.previewReactionEmoji(CID, CHID, 'cc'),
    ).rejects.toThrow(/adapter not connected/);
  });

  it('a custom-emoji reaction-received event carries emojiCid/emojiSize into the chip entry', async () => {
    await service.connectAdapter(adapter);
    await seedMessage();
    const cid = 'ef'.repeat(32);
    fireReaction({ reactor: OTHER, emoji: '', add: true, emojiCid: cid, emojiSize: 256 });
    const msg = service.getMessages(CID, CHID)[0];
    expect(msg.reactions?.[0]).toMatchObject({
      emoji: '',
      count: 1,
      reactors: [OTHER],
      emojiCid: cid,
      emojiSize: 256,
    });
  });

  it('reactToMessage throws when the adapter is not connected', async () => {
    await expect(
      service.reactToMessage(CID, CHID, 'm1', '👍', true),
    ).rejects.toThrow(/adapter not connected/);
  });

  it('reactToMessage normalizes a raw-string IPC rejection into an Error', async () => {
    // Production IPC rejections are raw strings (Error objects only in tests);
    // reactToMessage must wrap them so callers reading `.message` keep the
    // detail. Mirrors postMessage / ingestArtifact.
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockRejectedValue('message_id must be 16 bytes (32 hex chars)');
    await expect(
      service.reactToMessage(CID, CHID, 'm1', '👍', true),
    ).rejects.toThrow('message_id must be 16 bytes (32 hex chars)');
  });

  it('destroy removes the reaction listener', async () => {
    await service.connectAdapter(adapter);
    service.destroy();
    expect(adapter.listeners.has('channel-reaction-received')).toBe(false);
  });

  it('mine goes false when the local owner removes their own reaction (non-zero remainder)', async () => {
    service.selfOwnerId = OWN;
    await service.connectAdapter(adapter);
    await seedMessage();
    fireReaction({ reactor: OTHER, add: true });
    fireReaction({ reactor: OWN, add: true });
    let msg = service.getMessages(CID, CHID)[0];
    expect(msg.reactions?.[0].mine).toBe(true);
    fireReaction({ reactor: OWN, add: false });
    msg = service.getMessages(CID, CHID)[0];
    expect(msg.reactions?.[0].count).toBe(1);
    expect(msg.reactions?.[0].mine).toBe(false);
    expect(msg.reactions?.[0].reactors).toEqual([OTHER]);
  });

  // Seed m1 carrying an authoritative reactions[] (as list_channel_messages does).
  async function seedMessageWithReaction(
    reaction: { emoji: string; count: number; mine: boolean; reactors: string[] },
  ): Promise<void> {
    (adapter.invoke as any).mockResolvedValue([
      {
        messageId: 'm1',
        communityId: CID,
        channelId: CHID,
        author: OTHER,
        at: { wallMs: 100, logical: 0, deviceId: 'd' },
        body: [],
        reactions: [reaction],
      },
    ]);
    await service.listMessages(CID, CHID, undefined, 100);
  }

  it('does NOT clobber authoritative mine=true when selfOwnerId is empty (Qodo #318)', async () => {
    // App.svelte passes ownAddress='' while identity is still loading, so the
    // feed may set selfOwnerId=''. A live reaction event must NOT recompute and
    // clobber the authoritative mine=true carried by list_channel_messages.
    service.selfOwnerId = '';
    await service.connectAdapter(adapter);
    await seedMessageWithReaction({ emoji: '👍', count: 1, mine: true, reactors: [OWN] });
    fireReaction({ reactor: OTHER, emoji: '👍', add: true });
    const msg = service.getMessages(CID, CHID)[0];
    expect(msg.reactions?.[0].mine).toBe(true); // preserved, not clobbered
    expect(msg.reactions?.[0].reactors).toEqual([OWN, OTHER]);
  });

  it('self-heals cached mine when selfOwnerId transitions empty -> set (Qodo #318)', async () => {
    service.selfOwnerId = '';
    await service.connectAdapter(adapter);
    // mine was computed false during the empty-id window even though OWN reacted.
    await seedMessageWithReaction({ emoji: '👍', count: 1, mine: false, reactors: [OWN] });
    const cb = vi.fn();
    service.subscribeToChannel(CID, CHID, cb);
    service.selfOwnerId = OWN; // identity finishes loading
    const msg = service.getMessages(CID, CHID)[0];
    expect(msg.reactions?.[0].mine).toBe(true); // self-healed without a reload
    expect(cb).toHaveBeenCalled(); // subscribers notified so the feed re-renders
  });
});
