import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import DataTable from '../DataTable.svelte';
import { RingBuffer } from '../../ring-buffer';
import type { NetworkNode, NodeMetrics } from '../../network-types';

function makeNode(overrides: Partial<NetworkNode> & { address: string; displayName: string }): NetworkNode {
  const metrics: NodeMetrics = {
    timestamp: Date.now(),
    cpuPercent: 25,
    memoryUsedBytes: 2 * 1024 * 1024 * 1024,
    memoryTotalBytes: 8 * 1024 * 1024 * 1024,
    diskUsedBytes: 50 * 1024 * 1024 * 1024,
    diskTotalBytes: 256 * 1024 * 1024 * 1024,
  };
  return {
    isLocal: false,
    hopDistance: 1,
    status: 'online',
    metrics,
    metricsHistory: new RingBuffer<NodeMetrics>(10),
    lastSeen: Date.now(),
    ...overrides,
  };
}

const testNodes: NetworkNode[] = [
  makeNode({ address: 'aaa', displayName: 'alpha', isLocal: true, hopDistance: 0, metrics: { timestamp: 0, cpuPercent: 23, memoryUsedBytes: 2e9, memoryTotalBytes: 8e9, diskUsedBytes: 30e9, diskTotalBytes: 256e9 } }),
  makeNode({ address: 'bbb', displayName: 'bravo', status: 'degraded', metrics: { timestamp: 0, cpuPercent: 91, memoryUsedBytes: 6e9, memoryTotalBytes: 8e9, diskUsedBytes: 100e9, diskTotalBytes: 256e9 } }),
  makeNode({ address: 'ccc', displayName: 'charlie', status: 'offline', metrics: { timestamp: 0, cpuPercent: 0, memoryUsedBytes: 0, memoryTotalBytes: 8e9, diskUsedBytes: 0, diskTotalBytes: 256e9 } }),
];

describe('DataTable', () => {
  it('renders a table element', () => {
    render(DataTable, { props: { nodes: testNodes } });
    const table = screen.getByRole('table');
    expect(table).toBeTruthy();
  });

  it('renders a row for each node', () => {
    render(DataTable, { props: { nodes: testNodes } });
    const rows = screen.getAllByRole('row');
    // header + 3 data rows
    expect(rows.length).toBe(4);
  });

  it('displays node names', () => {
    render(DataTable, { props: { nodes: testNodes } });
    expect(screen.getByText('alpha')).toBeTruthy();
    expect(screen.getByText('bravo')).toBeTruthy();
    expect(screen.getByText('charlie')).toBeTruthy();
  });

  it('shows dashes for offline node metrics', () => {
    render(DataTable, { props: { nodes: testNodes } });
    const rows = screen.getAllByRole('row');
    // charlie is offline (row index 3 — 0 is header)
    const cells = rows[3].querySelectorAll('td');
    // CPU, Mem, Disk columns should show em-dash for offline
    const dashCells = Array.from(cells).filter((c) => c.textContent?.trim() === '\u2014');
    expect(dashCells.length).toBeGreaterThanOrEqual(3);
  });

  it('has sortable column headers', () => {
    render(DataTable, { props: { nodes: testNodes } });
    const headers = screen.getAllByRole('columnheader');
    expect(headers.length).toBeGreaterThan(0);
    const sortableHeaders = headers.filter((h) => h.querySelector('button'));
    expect(sortableHeaders.length).toBeGreaterThan(0);
  });

  it('emits onNodeSelect when a row is clicked', async () => {
    const onNodeSelect = vi.fn();
    render(DataTable, { props: { nodes: testNodes, onNodeSelect } });
    const rows = screen.getAllByRole('row');
    await fireEvent.click(rows[1]); // click first data row
    expect(onNodeSelect).toHaveBeenCalledWith('aaa');
  });

  it('marks selected row with aria-selected', () => {
    render(DataTable, { props: { nodes: testNodes, selectedAddress: 'bbb' } });
    const rows = screen.getAllByRole('row');
    const selected = rows.find((r) => r.getAttribute('aria-selected') === 'true');
    expect(selected).toBeTruthy();
    expect(selected?.textContent).toContain('bravo');
  });

  it('sorts memory by utilization ratio, not absolute bytes', async () => {
    const nodesWithDifferentRatios: NetworkNode[] = [
      makeNode({
        address: 'x1',
        displayName: 'highRatio',
        metrics: { timestamp: 0, cpuPercent: 50, memoryUsedBytes: 6e9, memoryTotalBytes: 8e9, diskUsedBytes: 50e9, diskTotalBytes: 256e9 },
      }),
      makeNode({
        address: 'x2',
        displayName: 'lowRatio',
        metrics: { timestamp: 0, cpuPercent: 50, memoryUsedBytes: 10e9, memoryTotalBytes: 64e9, diskUsedBytes: 50e9, diskTotalBytes: 256e9 },
      }),
    ];
    render(DataTable, { props: { nodes: nodesWithDifferentRatios } });
    const memBtn = screen.getByRole('button', { name: /sort by memory/i });
    await fireEvent.click(memBtn);
    const rows = screen.getAllByRole('row');
    // lowRatio (10/64 = 15.6%) should sort before highRatio (6/8 = 75%)
    expect(rows[1].textContent).toContain('lowRatio');
    expect(rows[2].textContent).toContain('highRatio');
  });
});
