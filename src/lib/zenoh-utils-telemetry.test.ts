import { describe, it, expect } from 'vitest';
import { discoveredToNetworkNode } from './zenoh-utils';

describe('discoveredToNetworkNode with telemetry', () => {
  it('uses real CPU metrics when available', () => {
    const node = discoveredToNetworkNode({
      nodeAddr: 'abcd1234',
      modelCid: 'aa'.repeat(32),
      ready: true,
      lastSeen: Date.now(),
      cpuPercent: 42.5,
      memMb: 512,
    });
    expect(node.metrics.cpuPercent).toBe(42.5);
    expect(node.metrics.memoryUsedBytes).toBe(512 * 1024 * 1024);
    // memoryTotalBytes mirrors used so percentage = 100%, not billions%
    expect(node.metrics.memoryTotalBytes).toBe(512 * 1024 * 1024);
  });

  it('uses zero sentinels when no telemetry', () => {
    const node = discoveredToNetworkNode({
      nodeAddr: 'abcd1234',
      modelCid: 'aa'.repeat(32),
      ready: true,
      lastSeen: Date.now(),
    });
    expect(node.metrics.cpuPercent).toBe(0);
    expect(node.metrics.memoryUsedBytes).toBe(0);
  });
});
