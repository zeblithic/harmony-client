import { describe, it, expect } from 'vitest';
import { discoveredToNetworkNode, filterStaleNodes } from './zenoh-utils';
import type { DiscoveredNode } from './zenoh-service';

describe('discoveredToNetworkNode', () => {
  const sample: DiscoveredNode = {
    nodeAddr: 'deadbeef01020304aabbccdd',
    modelCid: 'aabbccdd11223344eeff0011',
    ready: true,
    lastSeen: 1711500000,
  };

  it('sets address from nodeAddr', () => {
    const node = discoveredToNetworkNode(sample);
    expect(node.address).toBe('deadbeef01020304aabbccdd');
  });

  it('truncates displayName and adds (live) suffix', () => {
    const node = discoveredToNetworkNode(sample);
    expect(node.displayName).toBe('dead (live)');
    expect(node.displayName.length).toBeLessThanOrEqual(12);
  });

  it('marks online when ready', () => {
    const node = discoveredToNetworkNode(sample);
    expect(node.status).toBe('online');
  });

  it('marks degraded when not ready', () => {
    const node = discoveredToNetworkNode({ ...sample, ready: false });
    expect(node.status).toBe('degraded');
  });

  it('has inference and routing capabilities', () => {
    const node = discoveredToNetworkNode(sample);
    expect(node.capabilities).toContain('inference');
    expect(node.capabilities).toContain('routing');
  });

  it('sets modelName from truncated CID', () => {
    const node = discoveredToNetworkNode(sample);
    expect(node.modelName).toBe('aabbccdd');
  });

  it('sets heatPercent to 0', () => {
    const node = discoveredToNetworkNode(sample);
    expect(node.heatPercent).toBe(0);
  });

  it('is not local', () => {
    const node = discoveredToNetworkNode(sample);
    expect(node.isLocal).toBe(false);
  });

  it('handles short address without truncation', () => {
    const node = discoveredToNetworkNode({ ...sample, nodeAddr: 'abcd' });
    expect(node.displayName).toBe('abcd (live)');
  });
});

describe('filterStaleNodes', () => {
  const now = 1711500000;

  function makeMap(...nodes: DiscoveredNode[]): Map<string, DiscoveredNode> {
    return new Map(nodes.map(n => [n.nodeAddr, n]));
  }

  it('keeps nodes within threshold', () => {
    const nodes = makeMap({
      nodeAddr: 'a', modelCid: 'cc', ready: true,
      lastSeen: now - 10_000, // 10s ago
    });
    const result = filterStaleNodes(nodes, now);
    expect(result).toHaveLength(1);
    expect(result[0].nodeAddr).toBe('a');
  });

  it('removes nodes older than threshold', () => {
    const nodes = makeMap({
      nodeAddr: 'old', modelCid: 'cc', ready: true,
      lastSeen: now - 60_000, // 60s ago
    });
    const result = filterStaleNodes(nodes, now);
    expect(result).toHaveLength(0);
  });

  it('keeps fresh and removes stale in mixed set', () => {
    const nodes = makeMap(
      { nodeAddr: 'fresh', modelCid: 'cc', ready: true, lastSeen: now - 5_000 },
      { nodeAddr: 'stale', modelCid: 'dd', ready: true, lastSeen: now - 45_000 },
    );
    const result = filterStaleNodes(nodes, now);
    expect(result).toHaveLength(1);
    expect(result[0].nodeAddr).toBe('fresh');
  });

  it('uses custom threshold', () => {
    const nodes = makeMap({
      nodeAddr: 'a', modelCid: 'cc', ready: true,
      lastSeen: now - 10_000,
    });
    // 5s threshold — 10s old node should be stale
    const result = filterStaleNodes(nodes, now, 5_000);
    expect(result).toHaveLength(0);
  });

  it('returns empty for empty map', () => {
    const result = filterStaleNodes(new Map(), now);
    expect(result).toHaveLength(0);
  });

  it('keeps nodes at exactly the threshold', () => {
    const nodes = makeMap({
      nodeAddr: 'edge', modelCid: 'cc', ready: true,
      lastSeen: now - 30_000, // exactly at threshold
    });
    const result = filterStaleNodes(nodes, now);
    expect(result).toHaveLength(1);
  });
});
