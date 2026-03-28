import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { ZenohService, type TauriAdapter, type CapacityUpdate, type ZenohStatusEvent, type ProfilePayload } from './zenoh-service';

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

  afterEach(() => {
    service.destroy();
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

  it('updates status on zenoh-status connected event', async () => {
    // Must be in 'connecting' state for 'connected' event to be accepted
    const promise = service.connect('tcp/127.0.0.1:7447');
    mock.emit('zenoh-status', {
      status: 'connected',
      endpoint: 'tcp/127.0.0.1:7447',
    } satisfies ZenohStatusEvent);
    await promise;
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

  it('upserts discovered node on capacity-update', async () => {
    // Must be connected for capacity updates to be accepted
    const promise = service.connect('tcp/127.0.0.1:7447');
    mock.emit('zenoh-status', { status: 'connected' } satisfies ZenohStatusEvent);
    await promise;

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

  it('updates lastSeen on duplicate capacity-update', async () => {
    const promise = service.connect('tcp/127.0.0.1:7447');
    mock.emit('zenoh-status', { status: 'connected' } satisfies ZenohStatusEvent);
    await promise;

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
    const promise = service.connect('tcp/127.0.0.1:7447');
    mock.emit('zenoh-status', { status: 'connected' } satisfies ZenohStatusEvent);
    await promise;

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

  it('schedules reconnect on invoke failure', async () => {
    (mock.adapter.invoke as ReturnType<typeof vi.fn>).mockRejectedValueOnce('timeout');
    await service.connect('tcp/bad:9999');
    // After invoke failure, auto-reconnect kicks in immediately → 'reconnecting'
    expect(service.connectionStatus).toBe('reconnecting');
    expect(service.errorMessage).toContain('Reconnecting');
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

  it('upserts peer profile on profile-update', async () => {
    const promise = service.connect('tcp/127.0.0.1:7447');
    mock.emit('zenoh-status', { status: 'connected' } satisfies ZenohStatusEvent);
    await promise;

    mock.emit('profile-update', {
      address: 'peer1',
      displayName: 'Alice',
      statusText: 'Hello',
    } satisfies ProfilePayload);
    expect(service.peerProfiles.size).toBe(1);
    const profile = service.peerProfiles.get('peer1')!;
    expect(profile.displayName).toBe('Alice');
    expect(profile.statusText).toBe('Hello');
  });

  it('clears peer profiles on disconnect', async () => {
    const promise = service.connect('tcp/127.0.0.1:7447');
    mock.emit('zenoh-status', { status: 'connected' } satisfies ZenohStatusEvent);
    await promise;

    mock.emit('profile-update', {
      address: 'peer1',
      displayName: 'Bob',
    } satisfies ProfilePayload);
    expect(service.peerProfiles.size).toBe(1);

    await service.disconnect();
    expect(service.peerProfiles.size).toBe(0);
  });

  it('publishProfile invokes publish_profile command', async () => {
    await service.publishProfile({
      address: 'local',
      displayName: 'Me',
    });
    expect(mock.adapter.invoke).toHaveBeenCalledWith('publish_profile', {
      profile: { address: 'local', displayName: 'Me' },
    });
  });
});

describe('ZenohService auto-reconnect', () => {
  let service: ZenohService;
  let mock: ReturnType<typeof createMockAdapter>;

  beforeEach(async () => {
    vi.useFakeTimers();
    mock = createMockAdapter();
    service = new ZenohService(mock.adapter);
    await service.init();
  });

  afterEach(() => {
    service.destroy();
    vi.useRealTimers();
  });

  it('schedules reconnect on error after connect', async () => {
    await service.connect('tcp/127.0.0.1:7447');
    mock.emit('zenoh-status', { status: 'connected' } satisfies ZenohStatusEvent);
    mock.emit('zenoh-status', { status: 'error', error: 'session lost' } satisfies ZenohStatusEvent);
    expect(service.connectionStatus).toBe('reconnecting');
    expect(service.errorMessage).toContain('Reconnecting');
  });

  it('reconnects after delay', async () => {
    await service.connect('tcp/127.0.0.1:7447');
    mock.emit('zenoh-status', { status: 'connected' } satisfies ZenohStatusEvent);
    mock.emit('zenoh-status', { status: 'error', error: 'lost' } satisfies ZenohStatusEvent);

    vi.advanceTimersByTime(2000);
    // Should have called connect_zenoh again
    expect(mock.adapter.invoke).toHaveBeenCalledTimes(2); // original + reconnect
  });

  it('ignores stale error after user disconnect', async () => {
    await service.connect('tcp/127.0.0.1:7447');
    mock.emit('zenoh-status', { status: 'connected' } satisfies ZenohStatusEvent);
    await service.disconnect();

    // Simulate a stale error arriving after disconnect — should be ignored
    mock.emit('zenoh-status', { status: 'error', error: 'stale' } satisfies ZenohStatusEvent);
    // Status stays 'disconnected', not overwritten to 'error'
    expect(service.connectionStatus).toBe('disconnected');
    vi.advanceTimersByTime(30_000);
    expect(mock.adapter.invoke).toHaveBeenCalledTimes(2);
  });

  it('cancels reconnect on explicit disconnect', async () => {
    await service.connect('tcp/127.0.0.1:7447');
    mock.emit('zenoh-status', { status: 'connected' } satisfies ZenohStatusEvent);
    mock.emit('zenoh-status', { status: 'error', error: 'lost' } satisfies ZenohStatusEvent);
    expect(service.connectionStatus).toBe('reconnecting');

    await service.disconnect();
    expect(service.connectionStatus).toBe('disconnected');

    vi.advanceTimersByTime(30_000);
    // No reconnect attempted
    expect(mock.adapter.invoke).toHaveBeenCalledTimes(2); // connect + disconnect
  });

  it('uses exponential backoff', async () => {
    await service.connect('tcp/127.0.0.1:7447');
    mock.emit('zenoh-status', { status: 'connected' } satisfies ZenohStatusEvent);

    // First error → 2s delay
    mock.emit('zenoh-status', { status: 'error', error: 'lost' } satisfies ZenohStatusEvent);
    expect(service.errorMessage).toContain('2s');
  });

  it('backoff delay grows on repeated failures', async () => {
    await service.connect('tcp/127.0.0.1:7447');
    mock.emit('zenoh-status', { status: 'connected' } satisfies ZenohStatusEvent);

    // First error → 2s
    mock.emit('zenoh-status', { status: 'error', error: 'lost' } satisfies ZenohStatusEvent);
    expect(service.errorMessage).toContain('2s');

    // First reconnect fires after 2s
    vi.advanceTimersByTime(2000);
    // connect is called again (isReconnect=true, counter NOT reset)
    // Simulate it failing again
    mock.emit('zenoh-status', { status: 'error', error: 'lost again' } satisfies ZenohStatusEvent);
    // Second delay should be 4s (2000 * 2^1)
    expect(service.errorMessage).toContain('4s');

    // Third failure after 4s
    vi.advanceTimersByTime(4000);
    mock.emit('zenoh-status', { status: 'error', error: 'still lost' } satisfies ZenohStatusEvent);
    // Third delay should be 8s (2000 * 2^2)
    expect(service.errorMessage).toContain('8s');
  });
});
