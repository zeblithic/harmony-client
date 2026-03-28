import { describe, it, expect } from 'vitest';
import { discoveredToNetworkNode } from './zenoh-utils';
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
    expect(node.displayName).toBe('deadbeef (live)');
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
