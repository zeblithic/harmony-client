import type { NodeStatus, NodeCapability, TransportType } from './network-types';

export const CAPABILITY_COLORS: Record<NodeCapability, string> = {
  inference: '#5865f2',
  storage: '#57f287',
  routing: '#fee75c',
  compute: '#eb459e',
};

const CAPABILITY_INDEX: Record<NodeCapability, number> = {
  inference: 0,
  storage: 1,
  routing: 2,
  compute: 3,
};

function lerpColor(a: string, b: string, t: number): string {
  const ar = parseInt(a.slice(1, 3), 16);
  const ag = parseInt(a.slice(3, 5), 16);
  const ab = parseInt(a.slice(5, 7), 16);
  const br = parseInt(b.slice(1, 3), 16);
  const bg = parseInt(b.slice(3, 5), 16);
  const bb = parseInt(b.slice(5, 7), 16);
  const r = Math.round(ar + (br - ar) * t);
  const g = Math.round(ag + (bg - ag) * t);
  const bl = Math.round(ab + (bb - ab) * t);
  return `#${r.toString(16).padStart(2, '0')}${g.toString(16).padStart(2, '0')}${bl.toString(16).padStart(2, '0')}`;
}

export function heatToColor(percent: number, status: NodeStatus, isLocal: boolean): string {
  if (status === 'offline') return '#72767d';
  if (isLocal) return '#5865f2';
  const p = Math.max(0, Math.min(100, percent));
  if (p <= 50) {
    return lerpColor('#43b581', '#faa61a', p / 50);
  }
  return lerpColor('#faa61a', '#ed4245', (p - 50) / 50);
}

export function badgePosition(
  capability: NodeCapability,
  nr: number,
): { dx: number; dy: number } {
  const offset = Math.round(nr * 0.7);
  const index = CAPABILITY_INDEX[capability];
  switch (index) {
    case 0: return { dx: offset, dy: -offset };
    case 1: return { dx: offset, dy: offset };
    case 2: return { dx: -offset, dy: offset };
    case 3: return { dx: -offset, dy: -offset };
    default: return { dx: offset, dy: -offset };
  }
}

export function linkDashPattern(transportType: TransportType): number[] {
  switch (transportType) {
    case 'iroh': return [];
    case 'reticulum': return [8, 4];
    case 'zenoh': return [2, 4];
    case 'rawlink': return [4, 8];
    case 's3': return [1, 3];
  }
}

export function linkUtilizationColor(percent: number): string {
  const p = Math.max(0, Math.min(100, percent));
  if (p < 20) return '#4f545c';
  if (p < 60) return '#43b581';
  if (p < 85) return '#faa61a';
  return '#ed4245';
}

export function linkWidth(percent: number): number {
  const p = Math.max(0, Math.min(100, percent));
  return 1 + (p / 100) * 3;
}

export function nodeRadius(hopDistance: number): number {
  if (hopDistance === 0) return 20;
  if (hopDistance === 1) return 14;
  return 10;
}

interface PointNode {
  address: string;
  x: number;
  y: number;
  hopDistance: number;
}

export function findNodeAtPoint<T extends PointNode>(
  px: number,
  py: number,
  nodes: T[],
): T | null {
  let closest: T | null = null;
  let closestDist = Infinity;

  for (const node of nodes) {
    const dx = px - node.x;
    const dy = py - node.y;
    const dist = Math.sqrt(dx * dx + dy * dy);
    const radius = nodeRadius(node.hopDistance);
    if (dist <= radius && dist < closestDist) {
      closest = node;
      closestDist = dist;
    }
  }

  return closest;
}

interface PointLink {
  id: string;
  source: { x: number; y: number };
  target: { x: number; y: number };
  utilizationPercent: number;
}

export function findLinkAtPoint<T extends PointLink>(
  px: number,
  py: number,
  links: T[],
): T | null {
  let closest: T | null = null;
  let closestDist = Infinity;

  for (const link of links) {
    const sx = link.source.x;
    const sy = link.source.y;
    const tx = link.target.x;
    const ty = link.target.y;

    const dx = tx - sx;
    const dy = ty - sy;
    const lenSq = dx * dx + dy * dy;
    if (lenSq === 0) continue;

    // Project point onto line segment, clamped to [0,1]
    const t = Math.max(0, Math.min(1, ((px - sx) * dx + (py - sy) * dy) / lenSq));
    const projX = sx + t * dx;
    const projY = sy + t * dy;
    const dist = Math.sqrt((px - projX) ** 2 + (py - projY) ** 2);

    const threshold = linkWidth(link.utilizationPercent) / 2 + 4;
    if (dist <= threshold && dist < closestDist) {
      closest = link;
      closestDist = dist;
    }
  }

  return closest;
}

export function advanceParticle(position: number, speed: number, dt: number): number {
  const advancement = 2 * speed * dt;
  return (position + advancement) % 1;
}
