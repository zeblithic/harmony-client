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
    });
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
