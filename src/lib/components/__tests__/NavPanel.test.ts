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
});
