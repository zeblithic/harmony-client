import type { NodeStatus } from './network-types';

export function nodeHealthColor(status: NodeStatus, isLocal: boolean): string {
  switch (status) {
    case 'online':
      return isLocal ? '#5865f2' : '#43b581';
    case 'degraded':
      return '#faa61a';
    case 'offline':
      return '#72767d';
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
