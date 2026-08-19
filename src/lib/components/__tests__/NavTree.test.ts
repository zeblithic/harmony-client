import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
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
    mentionCount: 0,
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
    mentionCount: 0,
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
    mentionCount: 0,
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
    mentionCount: 0,
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
    mentionCount: 0,
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

// ZEB-965: communities render FLAT in the left nav — their channel rows (and
// the synthetic proposals / ＋ add-channel rows, formerly ZEB-663/606) moved to
// CommunityView's right-hand ChannelsPanel. NavTree still renders channel rows
// when mounted AT a community (parentId=communityId) — that is the ChannelsPanel
// mount path. The add-channel/proposals gating tests live in ChannelsPanel.test.ts.
describe('NavTree — communities are flat in the left nav (ZEB-965)', () => {
  const commNodes: NavNode[] = [
    {
      id: 'c1', parentId: null, type: 'community', name: 'Crew',
      expanded: true, unreadCount: 0, mentionCount: 0, unreadLevel: 'none',
    },
    {
      id: 'ch1', parentId: 'c1', type: 'channel', name: 'general', channelKind: 'text',
      expanded: false, unreadCount: 0, mentionCount: 0, unreadLevel: 'none',
    },
  ];

  it('does not render channel children under a community at the top level (even expanded)', () => {
    render(NavTree, { props: { nodes: commNodes, parentId: null } });
    expect(screen.getByText('Crew')).toBeTruthy();
    expect(screen.queryByText('general')).toBeNull();
  });

  it('renders no proposals or add-channel rows in the left nav', () => {
    const { container } = render(NavTree, {
      props: { nodes: commNodes, parentId: null, canManageChannels: () => true },
    });
    expect(container.querySelector('[data-testid="proposals-row-c1"]')).toBeNull();
    expect(container.querySelector('[data-testid="add-channel-row-c1"]')).toBeNull();
  });

  it('community rows have no expand/collapse chevron', () => {
    render(NavTree, { props: { nodes: commNodes, parentId: null, onToggle: vi.fn() } });
    expect(screen.queryByLabelText(/Collapse community|Expand community/)).toBeNull();
  });

  it('still renders channel rows when mounted at a community (ChannelsPanel path)', async () => {
    const onClick = vi.fn();
    const { getByTestId } = render(NavTree, {
      props: { nodes: commNodes, parentId: 'c1', onClick },
    });
    expect(screen.getByText('general')).toBeTruthy();
    await fireEvent.click(getByTestId('nav-row-ch1'));
    expect(onClick).toHaveBeenCalledWith('ch1');
  });
});
