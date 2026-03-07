import { describe, it, expect } from 'vitest';
import {
  nodeHealthColor,
  linkUtilizationColor,
  linkWidth,
  findNodeAtPoint,
  advanceParticle,
  nodeRadius,
} from './graph-utils';

describe('nodeHealthColor', () => {
  it('returns green for healthy online nodes', () => {
    expect(nodeHealthColor('online', false)).toBe('#43b581');
  });

  it('returns accent blue for local node', () => {
    expect(nodeHealthColor('online', true)).toBe('#5865f2');
  });

  it('returns amber for degraded', () => {
    expect(nodeHealthColor('degraded', false)).toBe('#faa61a');
  });

  it('returns gray for offline', () => {
    expect(nodeHealthColor('offline', false)).toBe('#72767d');
  });
});

describe('linkUtilizationColor', () => {
  it('returns dim gray for idle (0-20%)', () => {
    expect(linkUtilizationColor(10)).toBe('#4f545c');
  });

  it('returns green for moderate (20-60%)', () => {
    expect(linkUtilizationColor(40)).toBe('#43b581');
  });

  it('returns amber for busy (60-85%)', () => {
    expect(linkUtilizationColor(70)).toBe('#faa61a');
  });

  it('returns red for saturated (85%+)', () => {
    expect(linkUtilizationColor(90)).toBe('#ed4245');
  });

  it('clamps to 0-100 range', () => {
    expect(linkUtilizationColor(-5)).toBe('#4f545c');
    expect(linkUtilizationColor(110)).toBe('#ed4245');
  });
});

describe('linkWidth', () => {
  it('returns 1 for idle', () => {
    expect(linkWidth(0)).toBe(1);
  });

  it('returns 4 for saturated', () => {
    expect(linkWidth(100)).toBe(4);
  });

  it('scales linearly between 1 and 4', () => {
    const w = linkWidth(50);
    expect(w).toBeGreaterThan(1);
    expect(w).toBeLessThan(4);
  });
});

describe('nodeRadius', () => {
  it('returns 20 for local node (hop 0)', () => {
    expect(nodeRadius(0)).toBe(20);
  });

  it('returns 14 for direct peer (hop 1)', () => {
    expect(nodeRadius(1)).toBe(14);
  });

  it('returns 10 for peers of peers (hop 2+)', () => {
    expect(nodeRadius(2)).toBe(10);
    expect(nodeRadius(5)).toBe(10);
  });
});

interface SimpleNode {
  address: string;
  x: number;
  y: number;
  hopDistance: number;
}

describe('findNodeAtPoint', () => {
  const nodes: SimpleNode[] = [
    { address: 'a', x: 100, y: 100, hopDistance: 0 },
    { address: 'b', x: 200, y: 200, hopDistance: 1 },
    { address: 'c', x: 300, y: 300, hopDistance: 2 },
  ];

  it('finds a node when clicking within its radius', () => {
    const found = findNodeAtPoint(105, 105, nodes);
    expect(found?.address).toBe('a');
  });

  it('returns null when clicking empty space', () => {
    const found = findNodeAtPoint(500, 500, nodes);
    expect(found).toBeNull();
  });

  it('picks the closest node when overlapping', () => {
    const close: SimpleNode[] = [
      { address: 'a', x: 100, y: 100, hopDistance: 1 },
      { address: 'b', x: 110, y: 100, hopDistance: 1 },
    ];
    const found = findNodeAtPoint(108, 100, close);
    expect(found?.address).toBe('b');
  });
});

describe('advanceParticle', () => {
  it('moves particle along the link', () => {
    const pos = advanceParticle(0, 0.5, 0.1);
    expect(pos).toBeCloseTo(0.1, 5);
  });

  it('wraps around past 1.0', () => {
    const pos = advanceParticle(0.95, 0.5, 0.1);
    expect(pos).toBeCloseTo(0.05, 5);
  });

  it('speed affects advancement', () => {
    const slow = advanceParticle(0, 0.2, 0.1);
    const fast = advanceParticle(0, 0.8, 0.1);
    expect(fast).toBeGreaterThan(slow);
  });
});
