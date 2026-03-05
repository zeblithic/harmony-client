import { describe, it, expect } from 'vitest';
import type { NavNode } from './types';
import {
  getChildNodes,
  getNodeColor,
  sortNodes,
  getInheritedDisplayMode,
  getInheritedSortOrder,
} from './nav-utils';

/** Helper to build a minimal NavNode. */
function node(overrides: Partial<NavNode> & Pick<NavNode, 'id' | 'parentId' | 'type' | 'name'>): NavNode {
  return {
    expanded: false,
    unreadCount: 0,
    unreadLevel: 'none',
    ...overrides,
  };
}

// Shared fixture: a tree with 2 top-level folders, one nested folder, and leaf channels.
const TREE: NavNode[] = [
  node({ id: 'f1', parentId: null, type: 'folder', name: 'General' }),
  node({ id: 'f2', parentId: null, type: 'folder', name: 'Projects' }),
  node({ id: 'f3', parentId: 'f1', type: 'folder', name: 'Sub-General' }),
  node({ id: 'c1', parentId: 'f1', type: 'channel', name: 'chat' }),
  node({ id: 'c2', parentId: 'f3', type: 'channel', name: 'deep-chat' }),
  node({ id: 'c3', parentId: null, type: 'channel', name: 'announcements' }),
];

describe('getChildNodes', () => {
  it('returns top-level nodes when parentId is null', () => {
    const result = getChildNodes(TREE, null);
    expect(result.map((n) => n.id)).toEqual(['f1', 'f2', 'c3']);
  });

  it('returns nested children of a folder', () => {
    const result = getChildNodes(TREE, 'f1');
    expect(result.map((n) => n.id)).toEqual(['f3', 'c1']);
  });

  it('returns empty array for a leaf node', () => {
    const result = getChildNodes(TREE, 'c1');
    expect(result).toEqual([]);
  });
});

describe('getNodeColor', () => {
  it('assigns top-level folders by sibling position mod 4', () => {
    expect(getNodeColor(TREE, 'f1')).toBe(0);
    expect(getNodeColor(TREE, 'f2')).toBe(1);
  });

  it('assigns nested folder avoiding parent color', () => {
    // f1 has color 0, so f3 should pick from [1,2,3] -> index 0 -> 1
    expect(getNodeColor(TREE, 'f3')).toBe(1);
  });

  it('returns -1 for non-folder nodes', () => {
    expect(getNodeColor(TREE, 'c1')).toBe(-1);
  });
});

describe('sortNodes', () => {
  const unsorted: NavNode[] = [
    node({ id: 'a', parentId: null, type: 'channel', name: 'Zebra', lastActivity: 100 }),
    node({ id: 'b', parentId: null, type: 'channel', name: 'apple', lastActivity: 300 }),
    node({ id: 'c', parentId: null, type: 'channel', name: 'Mango', lastActivity: 200 }),
  ];

  it('sorts by activity descending', () => {
    const result = sortNodes(unsorted, 'activity');
    expect(result.map((n) => n.id)).toEqual(['b', 'c', 'a']);
  });

  it('sorts alphabetically case-insensitive', () => {
    const result = sortNodes(unsorted, 'alphabetical');
    expect(result.map((n) => n.name)).toEqual(['apple', 'Mango', 'Zebra']);
  });

  it('preserves order for pinned', () => {
    const result = sortNodes(unsorted, 'pinned');
    expect(result.map((n) => n.id)).toEqual(['a', 'b', 'c']);
  });
});

describe('getInheritedDisplayMode', () => {
  it('returns the node own displayMode when set', () => {
    const nodes: NavNode[] = [
      node({ id: 'f1', parentId: null, type: 'folder', name: 'F1', displayMode: 'icon' }),
    ];
    expect(getInheritedDisplayMode(nodes, 'f1')).toBe('icon');
  });

  it('inherits displayMode from ancestor', () => {
    const nodes: NavNode[] = [
      node({ id: 'f1', parentId: null, type: 'folder', name: 'F1', displayMode: 'both' }),
      node({ id: 'c1', parentId: 'f1', type: 'channel', name: 'C1' }),
    ];
    expect(getInheritedDisplayMode(nodes, 'c1')).toBe('both');
  });

  it('falls back to text when no ancestor sets displayMode', () => {
    expect(getInheritedDisplayMode(TREE, 'c2')).toBe('text');
  });
});

describe('getInheritedSortOrder', () => {
  it('returns own sortOrder when set', () => {
    const nodes: NavNode[] = [
      node({ id: 'f1', parentId: null, type: 'folder', name: 'F1', sortOrder: 'alphabetical' }),
    ];
    expect(getInheritedSortOrder(nodes, 'f1')).toBe('alphabetical');
  });

  it('falls back to activity when nothing set', () => {
    expect(getInheritedSortOrder(TREE, 'c1')).toBe('activity');
  });
});
