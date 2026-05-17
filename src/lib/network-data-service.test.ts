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
    // ZEB-278: the original test relied on the 60-tick offline-flip branch
    // (15% per non-local node) which has a real 17-32% per-run flake rate
    // depending on Math.random() draws. Force a known-degraded CPU on a
    // non-local node so the FIRST tick deterministically fires an
    // online→degraded transition: with prevCpu=95, the per-tick drift of
    // ±~5 keeps the new CPU above the > 85 threshold, guaranteeing the
    // alert path runs exactly once.
    const alerts: string[] = [];
    service.onAlert = (msg) => alerts.push(msg);
    // Find a non-local node by isLocal rather than fixed array index
    // (ZEB-288 R1 CodeRabbit) — robust to any future constructor
    // reordering. The status-flip branches apply to all nodes; we
    // pick non-local just to match the original spec's intent.
    const target = service.nodes.find((n) => !n.isLocal);
    if (!target) throw new Error('no non-local node — constructor invariant broken');
    target.metrics.cpuPercent = 95;
    service.start();
    vi.advanceTimersByTime(1000);
    expect(alerts.length).toBeGreaterThan(0);
  });

  it('generates nodes with capabilities', () => {
    const service = new MockNetworkDataService();
    for (const node of service.nodes) {
      expect(node.capabilities.length).toBeGreaterThan(0);
      for (const cap of node.capabilities) {
        expect(['inference', 'storage', 'routing', 'compute']).toContain(cap);
      }
    }
  });

  it('assigns modelName only to inference nodes', () => {
    const service = new MockNetworkDataService();
    for (const node of service.nodes) {
      if (node.capabilities.includes('inference')) {
        expect(node.modelName).toBeDefined();
      } else {
        expect(node.modelName).toBeUndefined();
      }
    }
  });

  it('generates links with valid transport types', () => {
    const service = new MockNetworkDataService();
    for (const link of service.links) {
      expect(['iroh', 'reticulum', 'zenoh', 'rawlink', 's3']).toContain(link.transportType);
      expect(typeof link.encrypted).toBe('boolean');
    }
  });

  it('computes heatPercent after tick', () => {
    const service = new MockNetworkDataService();
    service.start();
    vi.advanceTimersByTime(1000);
    service.stop();
    const onlineNode = service.nodes.find(n => n.status !== 'offline');
    if (onlineNode) {
      expect(onlineNode.heatPercent).toBeGreaterThanOrEqual(0);
      expect(onlineNode.heatPercent).toBeLessThanOrEqual(100);
    }
  });
});
