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

describe('NavTree — AddChannelNavRow gating (ZEB-663)', () => {
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

  it('renders the ＋ add-channel row when the viewer can manage the community', () => {
    const { getByTestId } = render(NavTree, {
      props: {
        nodes: commNodes,
        parentId: null,
        canManageChannels: () => true,
        onAddChannel: () => {},
      },
    });
    expect(getByTestId('add-channel-row-c1')).toBeTruthy();
  });

  it('hides the ＋ add-channel row when the viewer cannot manage', () => {
    const { queryByTestId } = render(NavTree, {
      props: {
        nodes: commNodes,
        parentId: null,
        canManageChannels: () => false,
        onAddChannel: () => {},
      },
    });
    expect(queryByTestId('add-channel-row-c1')).toBeNull();
  });

  it('hides the ＋ add-channel row when no canManageChannels resolver is provided', () => {
    const { queryByTestId } = render(NavTree, {
      props: { nodes: commNodes, parentId: null },
    });
    expect(queryByTestId('add-channel-row-c1')).toBeNull();
  });

  it('clicking the ＋ row dispatches onAddChannel with the community id', async () => {
    const onAddChannel = vi.fn();
    const { getByTestId } = render(NavTree, {
      props: {
        nodes: commNodes,
        parentId: null,
        canManageChannels: () => true,
        onAddChannel,
      },
    });
    await fireEvent.click(getByTestId('add-channel-row-c1'));
    expect(onAddChannel).toHaveBeenCalledWith('c1');
  });

  it('Space activation suppresses the browser default (page scroll) and adds a channel', () => {
    const onAddChannel = vi.fn();
    const { getByTestId } = render(NavTree, {
      props: {
        nodes: commNodes,
        parentId: null,
        canManageChannels: () => true,
        onAddChannel,
      },
    });
    const row = getByTestId('add-channel-row-c1');
    const evt = new KeyboardEvent('keydown', { key: ' ', bubbles: true, cancelable: true });
    row.dispatchEvent(evt);
    expect(evt.defaultPrevented).toBe(true); // Space would otherwise scroll the page
    expect(onAddChannel).toHaveBeenCalledWith('c1');
  });
});
