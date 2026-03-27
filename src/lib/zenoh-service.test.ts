import { describe, it, expect, vi, beforeEach } from 'vitest';
import { ZenohService, type TauriAdapter, type CapacityUpdate, type ZenohStatusEvent } from './zenoh-service';

function createMockAdapter() {
  const listeners: Record<string, Array<(event: { payload: unknown }) => void>> = {};
  const adapter: TauriAdapter = {
    invoke: vi.fn().mockResolvedValue(undefined),
    listen: vi.fn().mockImplementation(async (event: string, handler: (event: { payload: unknown }) => void) => {
      if (!listeners[event]) listeners[event] = [];
      listeners[event].push(handler);
      return () => {
        listeners[event] = listeners[event].filter((h) => h !== handler);
      };
    }),
  };
  function emit(event: string, payload: unknown) {
    for (const handler of listeners[event] ?? []) {
      handler({ payload });
    }
  }
  return { adapter, emit, listeners };
}

describe('ZenohService', () => {
  let service: ZenohService;
  let mock: ReturnType<typeof createMockAdapter>;

  beforeEach(async () => {
    mock = createMockAdapter();
    service = new ZenohService(mock.adapter);
    await service.init();
  });

  it('starts disconnected', () => {
    expect(service.connectionStatus).toBe('disconnected');
    expect(service.discoveredNodes.size).toBe(0);
  });

  it('connect invokes connect_zenoh with endpoint', async () => {
    await service.connect('tcp/127.0.0.1:7447');
    expect(mock.adapter.invoke).toHaveBeenCalledWith('connect_zenoh', {
      endpoint: 'tcp/127.0.0.1:7447',
    });
  });

  it('sets connecting status during connect', async () => {
    const promise = service.connect('tcp/127.0.0.1:7447');
    expect(service.connectionStatus).toBe('connecting');
    await promise;
  });

  it('updates status on zenoh-status connected event', () => {
    mock.emit('zenoh-status', {
      status: 'connected',
      endpoint: 'tcp/127.0.0.1:7447',
    } satisfies ZenohStatusEvent);
    expect(service.connectionStatus).toBe('connected');
  });

  it('updates status on zenoh-status error event', () => {
    mock.emit('zenoh-status', {
      status: 'error',
      error: 'connection refused',
    } satisfies ZenohStatusEvent);
    expect(service.connectionStatus).toBe('error');
    expect(service.errorMessage).toBe('connection refused');
  });

  it('upserts discovered node on capacity-update', () => {
    mock.emit('capacity-update', {
      nodeAddr: 'deadbeef',
      modelCid: 'aabb',
      ready: true,
    } satisfies CapacityUpdate);
    expect(service.discoveredNodes.size).toBe(1);
    const node = service.discoveredNodes.get('deadbeef')!;
    expect(node.modelCid).toBe('aabb');
    expect(node.ready).toBe(true);
    expect(node.lastSeen).toBeGreaterThan(0);
  });

  it('updates lastSeen on duplicate capacity-update', () => {
    mock.emit('capacity-update', {
      nodeAddr: 'node1',
      modelCid: 'cc',
      ready: true,
    } satisfies CapacityUpdate);
    const first = service.discoveredNodes.get('node1')!.lastSeen;

    mock.emit('capacity-update', {
      nodeAddr: 'node1',
      modelCid: 'cc',
      ready: false,
    } satisfies CapacityUpdate);
    const second = service.discoveredNodes.get('node1')!;
    expect(second.lastSeen).toBeGreaterThanOrEqual(first);
    expect(second.ready).toBe(false);
  });

  it('disconnect clears discovered nodes', async () => {
    mock.emit('capacity-update', {
      nodeAddr: 'node1',
      modelCid: 'cc',
      ready: true,
    } satisfies CapacityUpdate);
    expect(service.discoveredNodes.size).toBe(1);

    await service.disconnect();
    expect(service.discoveredNodes.size).toBe(0);
    expect(mock.adapter.invoke).toHaveBeenCalledWith('disconnect_zenoh');
  });

  it('sets error on invoke failure', async () => {
    (mock.adapter.invoke as ReturnType<typeof vi.fn>).mockRejectedValueOnce('timeout');
    await service.connect('tcp/bad:9999');
    expect(service.connectionStatus).toBe('error');
    expect(service.errorMessage).toBe('timeout');
  });

  it('destroy removes listeners', () => {
    service.destroy();
    mock.emit('capacity-update', {
      nodeAddr: 'after-destroy',
      modelCid: 'xx',
      ready: true,
    } satisfies CapacityUpdate);
    expect(service.discoveredNodes.size).toBe(0);
  });
});
