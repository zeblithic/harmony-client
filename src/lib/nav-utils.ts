import type { NavNode, DisplayMode, SortOrder } from './types';

export const NAV_PALETTE = ['#43b581', '#5865f2', '#9b59b6', '#e67e22'];

/** Return direct children of a given parent (null for top-level). */
export function getChildNodes(nodes: NavNode[], parentId: string | null): NavNode[] {
  return nodes.filter((n) => n.parentId === parentId);
}

/** Find a node by ID, or undefined. */
export function findNode(nodes: NavNode[], id: string): NavNode | undefined {
  return nodes.find((n) => n.id === id);
}

/** Count ancestors to root (top-level = 0). */
export function getNodeDepth(nodes: NavNode[], nodeId: string): number {
  let depth = 0;
  let current = findNode(nodes, nodeId);
  while (current && current.parentId !== null) {
    depth++;
    current = findNode(nodes, current.parentId);
  }
  return depth;
}

/**
 * Color index for folders:
 * - Top-level: sibling position % 4
 * - Nested: avoid parent's color, cycle through remaining 3
 * - Non-folders: returns -1
 */
export function getNodeColor(nodes: NavNode[], nodeId: string): number {
  const node = findNode(nodes, nodeId);
  if (!node || node.type !== 'folder') return -1;

  const depth = getNodeDepth(nodes, nodeId);

  if (depth === 0) {
    // Top-level: position among top-level siblings
    const siblings = getChildNodes(nodes, null);
    const index = siblings.findIndex((n) => n.id === nodeId);
    return index % NAV_PALETTE.length;
  }

  // Nested: avoid parent's color
  const parentColor = getNodeColor(nodes, node.parentId!);
  const siblings = getChildNodes(nodes, node.parentId);
  const siblingIndex = siblings.filter((n) => n.type === 'folder').findIndex((n) => n.id === nodeId);
  // Build palette excluding parent color
  const available = [0, 1, 2, 3].filter((i) => i !== parentColor);
  return available[siblingIndex % available.length];
}

/**
 * Array of color indices from outermost ancestor folder down to parent folder.
 * Used for rendering stacked border strips.
 */
export function getColorAncestry(nodes: NavNode[], nodeId: string): number[] {
  const ancestry: number[] = [];
  let current = findNode(nodes, nodeId);
  // Walk up to collect ancestor folder colors (excluding self)
  while (current && current.parentId !== null) {
    const parent = findNode(nodes, current.parentId);
    if (parent && parent.type === 'folder') {
      ancestry.unshift(getNodeColor(nodes, parent.id));
    }
    current = parent;
  }
  return ancestry;
}

/** Sort nodes by the given order. Returns a new array. */
export function sortNodes(nodes: NavNode[], order: SortOrder): NavNode[] {
  const copy = [...nodes];
  switch (order) {
    case 'activity':
      return copy.sort((a, b) => (b.lastActivity ?? 0) - (a.lastActivity ?? 0));
    case 'alphabetical':
      return copy.sort((a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: 'base' }));
    case 'pinned':
      return copy; // preserve order
  }
}

/** Walk up ancestors for displayMode, fallback 'text'. */
export function getInheritedDisplayMode(nodes: NavNode[], nodeId: string): DisplayMode {
  let current = findNode(nodes, nodeId);
  while (current) {
    if (current.displayMode) return current.displayMode;
    if (current.parentId === null) break;
    current = findNode(nodes, current.parentId);
  }
  return 'text';
}

/** Walk up ancestors for sortOrder, fallback 'activity'. */
export function getInheritedSortOrder(nodes: NavNode[], nodeId: string): SortOrder {
  let current = findNode(nodes, nodeId);
  while (current) {
    if (current.sortOrder) return current.sortOrder;
    if (current.parentId === null) break;
    current = findNode(nodes, current.parentId);
  }
  return 'activity';
}
