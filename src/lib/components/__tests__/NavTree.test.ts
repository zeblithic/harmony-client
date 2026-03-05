import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import NavTree from '../NavTree.svelte';
import type { NavNode } from '../../types';

const baseNodes: NavNode[] = [
  {
    id: 'work',
    parentId: null,
    type: 'folder',
    name: 'Work',
    expanded: true,
    unreadCount: 0,
    unreadLevel: 'none',
    sortOrder: 'alphabetical',
    lastActivity: 1000,
  },
  {
    id: 'general',
    parentId: 'work',
    type: 'channel',
    name: 'general',
    expanded: false,
    unreadCount: 3,
    unreadLevel: 'standard',
    lastActivity: 900,
  },
  {
    id: 'crypto',
    parentId: 'work',
    type: 'channel',
    name: 'crypto',
    expanded: false,
    unreadCount: 0,
    unreadLevel: 'none',
    lastActivity: 800,
  },
  {
    id: 'friends',
    parentId: null,
    type: 'folder',
    name: 'Friends',
    expanded: false,
    unreadCount: 0,
    unreadLevel: 'none',
    lastActivity: 500,
  },
  {
    id: 'bob',
    parentId: 'friends',
    type: 'dm',
    name: 'Bob',
    expanded: false,
    unreadCount: 0,
    unreadLevel: 'none',
    lastActivity: 400,
  },
];

describe('NavTree', () => {
  it('renders top-level nodes', () => {
    render(NavTree, { props: { nodes: baseNodes, parentId: null } });
    expect(screen.getByText('Work')).toBeTruthy();
    expect(screen.getByText('Friends')).toBeTruthy();
  });

  it('renders children of expanded folder', () => {
    render(NavTree, { props: { nodes: baseNodes, parentId: null } });
    // Work is expanded, so general and crypto should appear
    expect(screen.getByText('general')).toBeTruthy();
    expect(screen.getByText('crypto')).toBeTruthy();
  });

  it('does not render children of collapsed folder', () => {
    render(NavTree, { props: { nodes: baseNodes, parentId: null } });
    // Friends is collapsed, so Bob should not appear
    expect(screen.queryByText('Bob')).toBeNull();
  });

  it('respects alphabetical sort order', () => {
    render(NavTree, { props: { nodes: baseNodes, parentId: null } });
    // Work has sortOrder 'alphabetical', children should be crypto before general
    const allText = document.body.textContent ?? '';
    const cryptoPos = allText.indexOf('crypto');
    const generalPos = allText.indexOf('general');
    expect(cryptoPos).toBeLessThan(generalPos);
  });
});
