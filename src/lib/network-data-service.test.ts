import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { MockNetworkDataService } from './network-data-service';

describe('MockNetworkDataService', () => {
  let service: MockNetworkDataService;

  beforeEach(() => {
    vi.useFakeTimers();
    service = new MockNetworkDataService();
  });

  afterEach(() => {
    service.stop();
    vi.useRealTimers();
  });

  it('starts with nodes and links', () => {
    expect(service.nodes.length).toBeGreaterThan(0);
    expect(service.links.length).toBeGreaterThan(0);
  });

  it('has exactly one local node', () => {
    const local = service.nodes.filter((n) => n.isLocal);
    expect(local).toHaveLength(1);
    expect(local[0].hopDistance).toBe(0);
  });

  it('all nodes have valid initial metrics', () => {
    for (const node of service.nodes) {
      expect(node.metrics.cpuPercent).toBeGreaterThanOrEqual(0);
      expect(node.metrics.cpuPercent).toBeLessThanOrEqual(100);
      expect(node.metrics.memoryUsedBytes).toBeLessThanOrEqual(
        node.metrics.memoryTotalBytes,
      );
      expect(node.metrics.diskUsedBytes).toBeLessThanOrEqual(
        node.metrics.diskTotalBytes,
      );
    }
  });

  it('all links reference valid node addresses', () => {
    const addresses = new Set(service.nodes.map((n) => n.address));
    for (const link of service.links) {
      expect(addresses.has(link.source)).toBe(true);
      expect(addresses.has(link.target)).toBe(true);
    }
  });

  it('updates metrics after ticking', () => {
    service.start();
    vi.advanceTimersByTime(5000);
    expect(service.nodes[0].metricsHistory.length).toBeGreaterThan(0);
  });

  it('link utilization stays in valid range after ticking', () => {
    service.start();
    vi.advanceTimersByTime(10000);
    for (const link of service.links) {
      expect(link.utilizationPercent).toBeGreaterThanOrEqual(0);
      expect(link.utilizationPercent).toBeLessThanOrEqual(100);
    }
  });

  it('requestPeerData adds new nodes after delay', () => {
    service.start();
    const initialCount = service.nodes.length;
    service.requestPeerData(service.nodes[1].address);
    vi.advanceTimersByTime(2000);
    expect(service.nodes.length).toBeGreaterThan(initialCount);
  });

  it('stop() halts metric updates', () => {
    service.start();
    vi.advanceTimersByTime(3000);
    const historyLen = service.nodes[0].metricsHistory.length;
    service.stop();
    vi.advanceTimersByTime(5000);
    expect(service.nodes[0].metricsHistory.length).toBe(historyLen);
  });

  it('calls onAlert when node status changes', () => {
    const alerts: string[] = [];
    service.onAlert = (msg) => alerts.push(msg);
    service.start();
    vi.advanceTimersByTime(60000);
    expect(alerts.length).toBeGreaterThan(0);
  });
});
