import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import NodeDetail from '../NodeDetail.svelte';
import { RingBuffer } from '../../ring-buffer';
import type { NetworkNode, NetworkLink, NodeMetrics } from '../../network-types';

function makeTestNode(): NetworkNode {
  const metrics: NodeMetrics = {
    timestamp: Date.now(),
    cpuPercent: 47,
    memoryUsedBytes: 2.1 * 1024 * 1024 * 1024,
    memoryTotalBytes: 8 * 1024 * 1024 * 1024,
    diskUsedBytes: 34 * 1024 * 1024 * 1024,
    diskTotalBytes: 256 * 1024 * 1024 * 1024,
  };
  const history = new RingBuffer<NodeMetrics>(10);
  history.push(metrics);
  return {
    address: 'a7f3c219deadbeef',
    displayName: 'bravo',
    isLocal: false,
    hopDistance: 2,
    status: 'online',
    metrics,
    metricsHistory: history,
    lastSeen: Date.now(),
  };
}

const testLinks: NetworkLink[] = [
  { id: 'l1', source: 'a7f3c219deadbeef', target: 'aaa', interfaceType: 'tcp', capacityBps: 1e8, utilizationPercent: 12, latencyMs: 8, utilizationHistory: new RingBuffer(10) },
  { id: 'l2', source: 'a7f3c219deadbeef', target: 'bbb', interfaceType: 'udp', capacityBps: 1e8, utilizationPercent: 67, latencyMs: 23, utilizationHistory: new RingBuffer(10) },
];

describe('NodeDetail', () => {
  it('displays node name', () => {
    render(NodeDetail, { props: { node: makeTestNode(), links: testLinks } });
    expect(screen.getByText('bravo')).toBeTruthy();
  });

  it('displays truncated address', () => {
    render(NodeDetail, { props: { node: makeTestNode(), links: testLinks } });
    expect(screen.getByText('a7f3...beef')).toBeTruthy();
  });

  it('shows hop distance', () => {
    render(NodeDetail, { props: { node: makeTestNode(), links: testLinks } });
    expect(screen.getByText(/2 hops/)).toBeTruthy();
  });

  it('shows "local" for hop distance 0', () => {
    const localNode = { ...makeTestNode(), hopDistance: 0, isLocal: true };
    render(NodeDetail, { props: { node: localNode, links: testLinks } });
    expect(screen.getByText(/local/)).toBeTruthy();
  });

  it('displays CPU metric', () => {
    render(NodeDetail, { props: { node: makeTestNode(), links: testLinks } });
    expect(screen.getByText(/47%/)).toBeTruthy();
  });

  it('renders sparklines with role="img"', () => {
    render(NodeDetail, { props: { node: makeTestNode(), links: testLinks } });
    const sparklines = screen.getAllByRole('img');
    expect(sparklines.length).toBeGreaterThanOrEqual(3); // CPU, mem, disk
  });

  it('lists connected links', () => {
    render(NodeDetail, { props: { node: makeTestNode(), links: testLinks } });
    expect(screen.getByText(/TCP/i)).toBeTruthy();
    expect(screen.getByText(/UDP/i)).toBeTruthy();
  });
});
