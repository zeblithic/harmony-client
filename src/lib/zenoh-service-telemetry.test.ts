import { describe, it, expect, vi, beforeEach } from 'vitest';
import { ZenohService, type TauriAdapter, type DiscoveredNode } from './zenoh-service';

function mockAdapter(): TauriAdapter & { handlers: Record<string, (e: { payload: unknown }) => void> } {
  const handlers: Record<string, (e: { payload: unknown }) => void> = {};
  return {
    handlers,
    invoke: vi.fn().mockResolvedValue(undefined),
    listen: vi.fn().mockImplementation((event: string, handler: (e: { payload: unknown }) => void) => {
      handlers[event] = handler;
      return Promise.resolve(() => {});
    }),
  };
}

describe('ZenohService telemetry', () => {
  let service: ZenohService;
  let adapter: ReturnType<typeof mockAdapter>;

  beforeEach(async () => {
    adapter = mockAdapter();
    service = new ZenohService(adapter);
    await service.init();
    service.connectionStatus = 'connected';
    service.discoveredNodes.set('abcd1234', {
      nodeAddr: 'abcd1234',
      modelCid: 'aa'.repeat(32),
      ready: true,
      lastSeen: Date.now(),
    });
  });

  it('registers telemetry-event listener on init', () => {
    expect(adapter.listen).toHaveBeenCalledWith('telemetry-event', expect.any(Function));
  });

  it('updates node metrics on health telemetry', () => {
    const onChange = vi.fn();
    service.onChange = onChange;
    adapter.handlers['telemetry-event']({
      payload: {
        nodeAddr: 'abcd1234',
        intent: 'health',
        sequence: 1,
        timestamp: 1711600000,
        payload: { cpu_percent: 42.5, mem_mb: 512 },
      },
    });
    const node = service.discoveredNodes.get('abcd1234')!;
    expect(node.cpuPercent).toBe(42.5);
    expect(node.memMb).toBe(512);
    expect(onChange).toHaveBeenCalled();
  });

  it('updates node ready status on capacity_changed telemetry', () => {
    adapter.handlers['telemetry-event']({
      payload: {
        nodeAddr: 'abcd1234',
        intent: 'capacity_changed',
        sequence: 2,
        timestamp: 1711600100,
        payload: { ready: false },
      },
    });
    const node = service.discoveredNodes.get('abcd1234')!;
    expect(node.ready).toBe(false);
  });

  it('ignores telemetry for unknown nodes', () => {
    const onChange = vi.fn();
    service.onChange = onChange;
    adapter.handlers['telemetry-event']({
      payload: {
        nodeAddr: 'unknown_node',
        intent: 'health',
        sequence: 1,
        timestamp: 1711600000,
        payload: { cpu_percent: 10 },
      },
    });
    expect(onChange).not.toHaveBeenCalled();
  });

  it('ignores unknown intents without error', () => {
    adapter.handlers['telemetry-event']({
      payload: {
        nodeAddr: 'abcd1234',
        intent: 'object_detected',
        sequence: 3,
        timestamp: 1711600200,
        payload: { class: 'person' },
      },
    });
    const node = service.discoveredNodes.get('abcd1234')!;
    expect(node.cpuPercent).toBeUndefined();
  });

  it('ignores telemetry when not connected', () => {
    service.connectionStatus = 'disconnected';
    const onChange = vi.fn();
    service.onChange = onChange;
    adapter.handlers['telemetry-event']({
      payload: {
        nodeAddr: 'abcd1234',
        intent: 'health',
        sequence: 1,
        timestamp: 1711600000,
        payload: { cpu_percent: 50 },
      },
    });
    expect(onChange).not.toHaveBeenCalled();
  });
});
