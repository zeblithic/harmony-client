import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';

vi.mock('@tauri-apps/api/webviewWindow', () => ({
  WebviewWindow: class {
    static getByLabel = vi.fn().mockResolvedValue(null);
    constructor() { /* no-op in tests */ }
  },
}));

import NavPanel from '../NavPanel.svelte';
import type { NavNode, ContentItem, StorageBuddy } from '../../types';

const testNodes: NavNode[] = [
  {
    id: 'work',
    parentId: null,
    type: 'folder',
    name: 'Work',
    expanded: true,
    sortOrder: 'activity',
    unreadCount: 0,
    unreadLevel: 'none',
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
    id: 'friends',
    parentId: null,
    type: 'folder',
    name: 'Friends',
    expanded: true,
    sortOrder: 'pinned',
    unreadCount: 0,
    unreadLevel: 'none',
    lastActivity: 500,
  },
  {
    id: 'bob-dm',
    parentId: 'friends',
    type: 'dm',
    name: 'Bob',
    expanded: false,
    unreadCount: 0,
    unreadLevel: 'none',
    lastActivity: 400,
  },
  {
    id: 'eve-dm',
    parentId: null,
    type: 'dm',
    name: 'Eve',
    expanded: false,
    unreadCount: 0,
    unreadLevel: 'none',
    lastActivity: 300,
  },
];

describe('NavPanel', () => {
  it('renders tree when not collapsed', () => {
    render(NavPanel, { props: { nodes: testNodes, collapsed: false } });
    expect(screen.getByText('Work')).toBeTruthy();
    expect(screen.getByText('general')).toBeTruthy();
    expect(screen.getByText('Friends')).toBeTruthy();
    expect(screen.getByText('Bob')).toBeTruthy();
    expect(screen.getByText('Eve')).toBeTruthy();
  });

  it('shows only top-level icons when collapsed', () => {
    render(NavPanel, { props: { nodes: testNodes, collapsed: true } });
    // Top-level: Work (W), Friends (F), Eve (E)
    expect(screen.getByText('W')).toBeTruthy();
    expect(screen.getByText('F')).toBeTruthy();
    expect(screen.getByText('E')).toBeTruthy();
    // Children should NOT render
    expect(screen.queryByText('general')).toBeNull();
    expect(screen.queryByText('Bob')).toBeNull();
  });

  it('filters nodes by search query', async () => {
    render(NavPanel, { props: { nodes: testNodes, collapsed: false } });
    const input = screen.getByPlaceholderText('Search');
    await fireEvent.input(input, { target: { value: 'gen' } });
    // 'general' matches, and its ancestor 'Work' is shown
    expect(screen.getByText('general')).toBeTruthy();
    expect(screen.getByText('Work')).toBeTruthy();
    // Non-matching nodes should be filtered out
    expect(screen.queryByText('Friends')).toBeNull();
    expect(screen.queryByText('Eve')).toBeNull();
  });

  describe('files mode', () => {
    const testContentItems: ContentItem[] = [
      {
        sidecarId: 'mock-sidecar-navpanel-1',
        cid: 'cid-folder-projects',
        name: 'Projects',
        category: 'bundle',
        sensitivity: 'private',
        sizeBytes: 0,
        storedAt: Date.now(),
        lastAccessed: Date.now(),
        accessCount: 1,
        stalenessScore: 0,
        replicationTier: 'default',
        replicaCount: 3,
        pinned: false,
        licensed: false,
        parentCid: null,
        isFolder: true,
      },
    ];

    const testBuddies: StorageBuddy[] = [
      { address: 'addr1', displayName: 'Alice', storageUsedBytes: 100, online: true },
      { address: 'addr2', displayName: 'Bob', storageUsedBytes: 200, online: false },
    ];

    it('shows FolderTree and QuickFilters when appMode is files', () => {
      render(NavPanel, {
        props: {
          nodes: testNodes,
          collapsed: false,
          appMode: 'files',
          contentItems: testContentItems,
          storageBuddies: testBuddies,
        },
      });
      // FolderTree root
      expect(screen.getByText('All Files')).toBeTruthy();
      // FolderTree folder
      expect(screen.getByText('Projects')).toBeTruthy();
      // QuickFilters sections
      expect(screen.getByText('Category')).toBeTruthy();
      expect(screen.getByText('Status')).toBeTruthy();
      expect(screen.getByText('Replication Tier')).toBeTruthy();
    });

    it('shows StorageBuddySummary when appMode is files', () => {
      render(NavPanel, {
        props: {
          nodes: testNodes,
          collapsed: false,
          appMode: 'files',
          contentItems: testContentItems,
          storageBuddies: testBuddies,
        },
      });
      expect(screen.getByText(/2 buddies/)).toBeTruthy();
      expect(screen.getByText(/1 online/)).toBeTruthy();
      expect(screen.getByRole('button', { name: /Manage/ })).toBeTruthy();
    });

    it('does not show NavTree when in files mode', () => {
      render(NavPanel, {
        props: {
          nodes: testNodes,
          collapsed: false,
          appMode: 'files',
          contentItems: testContentItems,
          storageBuddies: testBuddies,
        },
      });
      // NavTree items should not be rendered
      expect(screen.queryByText('Work')).toBeNull();
      expect(screen.queryByText('general')).toBeNull();
    });

    it('does not show file components when in messages mode', () => {
      render(NavPanel, {
        props: {
          nodes: testNodes,
          collapsed: false,
          appMode: 'messages',
          contentItems: testContentItems,
          storageBuddies: testBuddies,
        },
      });
      // File components should not render
      expect(screen.queryByText('All Files')).toBeNull();
      expect(screen.queryByText('Category')).toBeNull();
      expect(screen.queryByText(/buddies/)).toBeNull();
      // NavTree should render
      expect(screen.getByText('Work')).toBeTruthy();
    });
  });

  it('renders Spellbook mode button', () => {
    render(NavPanel, { props: { nodes: testNodes, collapsed: false } });
    expect(screen.getByRole('button', { name: /spellbook/i })).toBeTruthy();
  });

  it('calls onModeChange with spellbook when clicked', async () => {
    const onModeChange = vi.fn();
    render(NavPanel, { props: { nodes: testNodes, collapsed: false, onModeChange } });
    await fireEvent.click(screen.getByRole('button', { name: /spellbook/i }));
    expect(onModeChange).toHaveBeenCalledWith('spellbook');
  });

  describe('FAB + fan-out menu (ZEB-263)', () => {
    const fabBaseProps = {
      nodes: testNodes,
      collapsed: false,
    };

    it('renders the "+" FAB button', () => {
      render(NavPanel, { props: fabBaseProps });
      expect(screen.getByLabelText(/Create new/i)).toBeTruthy();
    });

    it('clicking "+" opens the fan-out menu with 4 items', async () => {
      render(NavPanel, { props: fabBaseProps });
      await fireEvent.click(screen.getByLabelText(/Create new/i));
      expect(screen.getByText(/New direct message/i)).toBeTruthy();
      expect(screen.getByText(/New group DM/i)).toBeTruthy();
      expect(screen.getByText(/New community/i)).toBeTruthy();
      expect(screen.getByText(/Redeem invite/i)).toBeTruthy();
    });

    it('clicking "New direct message" calls onNewDm and closes the popover', async () => {
      const onNewDm = vi.fn();
      render(NavPanel, { props: { ...fabBaseProps, onNewDm } });
      await fireEvent.click(screen.getByLabelText(/Create new/i));
      await fireEvent.click(screen.getByText(/New direct message/i));
      expect(onNewDm).toHaveBeenCalled();
      expect(screen.queryByText(/New direct message/i)).toBeNull();
    });

    it('clicking "New group DM" calls onNewGroupDm', async () => {
      const onNewGroupDm = vi.fn();
      render(NavPanel, { props: { ...fabBaseProps, onNewGroupDm } });
      await fireEvent.click(screen.getByLabelText(/Create new/i));
      await fireEvent.click(screen.getByText(/New group DM/i));
      expect(onNewGroupDm).toHaveBeenCalled();
    });

    it('clicking "New community" calls onNewCommunity', async () => {
      const onNewCommunity = vi.fn();
      render(NavPanel, { props: { ...fabBaseProps, onNewCommunity } });
      await fireEvent.click(screen.getByLabelText(/Create new/i));
      await fireEvent.click(screen.getByText(/New community/i));
      expect(onNewCommunity).toHaveBeenCalled();
    });

    it('clicking "Redeem invite" calls onRedeemInvite', async () => {
      const onRedeemInvite = vi.fn();
      render(NavPanel, { props: { ...fabBaseProps, onRedeemInvite } });
      await fireEvent.click(screen.getByLabelText(/Create new/i));
      await fireEvent.click(screen.getByText(/Redeem invite/i));
      expect(onRedeemInvite).toHaveBeenCalled();
    });

    it('Escape closes the popover', async () => {
      render(NavPanel, { props: fabBaseProps });
      await fireEvent.click(screen.getByLabelText(/Create new/i));
      expect(screen.queryByText(/New community/i)).toBeTruthy();
      await fireEvent.keyDown(window, { key: 'Escape' });
      expect(screen.queryByText(/New community/i)).toBeNull();
    });

    it('clicking outside the popover closes it', async () => {
      render(NavPanel, { props: fabBaseProps });
      await fireEvent.click(screen.getByLabelText(/Create new/i));
      expect(screen.queryByText(/New community/i)).toBeTruthy();
      // Click on document.body — outside the popover and outside the FAB
      await fireEvent.mouseDown(document.body);
      expect(screen.queryByText(/New community/i)).toBeNull();
    });
  });

  describe('Community node rendering (ZEB-263)', () => {
    const communityNodes: NavNode[] = [
      {
        id: 'comm-1',
        parentId: null,
        type: 'community',
        name: 'IPFS Crew',
        expanded: true,
        unreadCount: 0,
        unreadLevel: 'none',
        lastActivity: 1000,
      },
    ];

    it('renders a community-kind node with its name', () => {
      render(NavPanel, { props: { nodes: communityNodes, collapsed: false } });
      expect(screen.getByText('IPFS Crew')).toBeTruthy();
    });

    it('clicking a community node fires onNodeClick with the node id', async () => {
      const onNodeClick = vi.fn();
      render(NavPanel, { props: { nodes: communityNodes, collapsed: false, onNodeClick } });
      await fireEvent.click(screen.getByText('IPFS Crew'));
      expect(onNodeClick).toHaveBeenCalledWith('comm-1');
    });
  });
});
