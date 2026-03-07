import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import LinkDetail from '../LinkDetail.svelte';
import { RingBuffer } from '../../ring-buffer';
import type { NetworkLink, NetworkNode, NodeMetrics, LinkSnapshot } from '../../network-types';

function makeTestLink(): NetworkLink {
  const history = new RingBuffer<LinkSnapshot>(10);
  history.push({ timestamp: Date.now(), utilizationPercent: 67, latencyMs: 23, bytesSent: 12.3e6, bytesReceived: 4.1e6 });
  return {
    id: 'l1',
    source: 'aaa',
    target: 'bbb',
    interfaceType: 'tcp',
    capacityBps: 100_000_000,
    utilizationPercent: 67,
    latencyMs: 23,
    utilizationHistory: history,
  };
}

function makeNodes(): NetworkNode[] {
  const metrics: NodeMetrics = { timestamp: 0, cpuPercent: 50, memoryUsedBytes: 4e9, memoryTotalBytes: 8e9, diskUsedBytes: 50e9, diskTotalBytes: 256e9 };
  return [
    { address: 'aaa', displayName: 'alpha', isLocal: true, hopDistance: 0, status: 'online', metrics, metricsHistory: new RingBuffer(10), lastSeen: Date.now() },
    { address: 'bbb', displayName: 'bravo', isLocal: false, hopDistance: 1, status: 'online', metrics, metricsHistory: new RingBuffer(10), lastSeen: Date.now() },
  ];
}

describe('LinkDetail', () => {
  it('displays endpoint names', () => {
    render(LinkDetail, { props: { link: makeTestLink(), nodes: makeNodes() } });
    expect(screen.getByText(/alpha/)).toBeTruthy();
    expect(screen.getByText(/bravo/)).toBeTruthy();
  });

  it('shows interface type', () => {
    render(LinkDetail, { props: { link: makeTestLink(), nodes: makeNodes() } });
    expect(screen.getByText(/TCP/i)).toBeTruthy();
  });

  it('displays utilization percentage', () => {
    render(LinkDetail, { props: { link: makeTestLink(), nodes: makeNodes() } });
    expect(screen.getByText(/67%/)).toBeTruthy();
  });

  it('displays latency', () => {
    render(LinkDetail, { props: { link: makeTestLink(), nodes: makeNodes() } });
    expect(screen.getByText(/23/)).toBeTruthy();
  });

  it('renders sparklines', () => {
    render(LinkDetail, { props: { link: makeTestLink(), nodes: makeNodes() } });
    const sparklines = screen.getAllByRole('img');
    expect(sparklines.length).toBeGreaterThanOrEqual(2); // utilization, latency
  });
});
