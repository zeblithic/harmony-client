import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi, afterEach } from 'vitest';
import { tick } from 'svelte';

vi.mock('@tauri-apps/api/webviewWindow', () => ({
  WebviewWindow: class {
    static getByLabel = vi.fn().mockResolvedValue(null);
    constructor() { /* no-op in tests */ }
  },
}));

import NavPanel from '../NavPanel.svelte';
import type { NavNode, ContentItem } from '../../types';
import { setNavModeOverride } from '../../feature-flags';

// ZEB-544: nav gating reads localStorage overrides — reset between tests so a
// re-enable in one test can't leak the rail state into another.
afterEach(() => {
  localStorage.clear();
});

const testNodes: NavNode[] = [
  {
    id: 'work',
    parentId: null,
    type: 'folder',
    name: 'Work',
    expanded: true,
    sortOrder: 'activity',
    unreadCount: 0,
    mentionCount: 0,
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
    mentionCount: 0,
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
    mentionCount: 0,
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
    mentionCount: 0,
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
    mentionCount: 0,
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
        replicationTier: 'default',
        replicaCount: 3,
        pinned: false,
        licensed: false,
        parentCid: null,
        isFolder: true,
      },
    ];

    it('shows FolderTree and QuickFilters when appMode is files', () => {
      render(NavPanel, {
        props: {
          nodes: testNodes,
          collapsed: false,
          appMode: 'files',
          contentItems: testContentItems,
        },
      });
      // FolderTree root
      expect(screen.getByText('All files')).toBeTruthy();
      // FolderTree folder
      expect(screen.getByText('Projects')).toBeTruthy();
      // QuickFilters sections
      expect(screen.getByText('Category')).toBeTruthy();
      expect(screen.getByText('Status')).toBeTruthy();
      expect(screen.getByText('Replication Tier')).toBeTruthy();
    });

    it('does not show NavTree when in files mode', () => {
      render(NavPanel, {
        props: {
          nodes: testNodes,
          collapsed: false,
          appMode: 'files',
          contentItems: testContentItems,
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
        },
      });
      // File components should not render
      expect(screen.queryByText('All files')).toBeNull();
      expect(screen.queryByText('Category')).toBeNull();
      expect(screen.queryByText(/buddies/)).toBeNull();
      // NavTree should render
      expect(screen.getByText('Work')).toBeTruthy();
    });
  });

  // ZEB-544: alpha surface gating — deferred/experimental modes are hidden from
  // the rail by default; the core Communities-first set stays.
  it('hides the deferred mode buttons (spellbook/mail/mint/network) by default', () => {
    render(NavPanel, { props: { nodes: testNodes, collapsed: false } });
    expect(screen.queryByRole('button', { name: /spellbook/i })).toBeNull();
    expect(screen.queryByRole('button', { name: /^mail$/i })).toBeNull();
    expect(screen.queryByRole('button', { name: /mint/i })).toBeNull();
    expect(screen.queryByRole('button', { name: /^network$/i })).toBeNull();
  });

  it('shows the core Communities-first rail buttons by default', () => {
    render(NavPanel, { props: { nodes: testNodes, collapsed: false } });
    expect(screen.getByRole('button', { name: /messages/i })).toBeTruthy();
    expect(screen.getByRole('button', { name: /vines/i })).toBeTruthy();
    expect(screen.getByRole('button', { name: /^files$/i })).toBeTruthy();
  });

  it('reveals a deferred mode inside the More menu when an override re-enables it, and routes on click', async () => {
    setNavModeOverride('spellbook', true);
    const onModeChange = vi.fn();
    render(NavPanel, { props: { nodes: testNodes, collapsed: false, onModeChange } });
    // ZEB-555: deferred/secondary modes now live in the "More ▾" overflow menu,
    // not the primary rail.
    await fireEvent.click(screen.getByTestId('more-menu-button'));
    await fireEvent.click(screen.getByRole('menuitem', { name: /spellbook/i }));
    expect(onModeChange).toHaveBeenCalledWith('spellbook');
  });

  describe('More menu (ZEB-555)', () => {
    it('renders a More button in the footer by default', () => {
      render(NavPanel, { props: { nodes: testNodes, collapsed: false } });
      expect(screen.getByTestId('more-menu-button')).toBeTruthy();
    });

    it('exposes the rehomed Help items when opened', async () => {
      render(NavPanel, { props: { nodes: testNodes, collapsed: false } });
      await fireEvent.click(screen.getByTestId('more-menu-button'));
      expect(screen.getByTestId('more-network-health')).toBeTruthy();
      expect(screen.getByTestId('more-feedback')).toBeTruthy();
      expect(screen.getByTestId('more-about')).toBeTruthy();
      expect(screen.getByTestId('more-docs')).toBeTruthy();
    });

    it('routes Network Health to the network mode via onModeChange', async () => {
      const onModeChange = vi.fn();
      render(NavPanel, { props: { nodes: testNodes, collapsed: false, onModeChange } });
      await fireEvent.click(screen.getByTestId('more-menu-button'));
      await fireEvent.click(screen.getByTestId('more-network-health'));
      expect(onModeChange).toHaveBeenCalledWith('network');
    });

    it('forwards the Submit Feedback callback', async () => {
      const onSubmitFeedback = vi.fn();
      render(NavPanel, { props: { nodes: testNodes, collapsed: false, onSubmitFeedback } });
      await fireEvent.click(screen.getByTestId('more-menu-button'));
      await fireEvent.click(screen.getByTestId('more-feedback'));
      expect(onSubmitFeedback).toHaveBeenCalled();
    });

    it('keeps Help reachable when the nav is collapsed (narrow screens)', async () => {
      // Regression (Qodo PR #334): collapsed (innerWidth <= 768) renders only
      // the icon rail. The old (?) overlay was always visible, so Help must stay
      // reachable here via a compact More trigger.
      render(NavPanel, { props: { nodes: testNodes, collapsed: true } });
      await fireEvent.click(screen.getByTestId('more-menu-button'));
      expect(screen.getByTestId('more-feedback')).toBeTruthy();
      expect(screen.getByTestId('more-network-health')).toBeTruthy();
    });
  });

  describe('Settings gear active state (ZEB-569)', () => {
    it('marks the gear active (aria-pressed + .active) when settingsActive is set', () => {
      render(NavPanel, { props: { nodes: testNodes, collapsed: false, settingsActive: true } });
      const gear = screen.getByRole('button', { name: 'Settings' });
      expect(gear.getAttribute('aria-pressed')).toBe('true');
      expect(gear.classList.contains('active')).toBe(true);
    });

    it('is inactive by default so click-again-to-close stays discoverable only while open', () => {
      render(NavPanel, { props: { nodes: testNodes, collapsed: false } });
      const gear = screen.getByRole('button', { name: 'Settings' });
      expect(gear.getAttribute('aria-pressed')).toBe('false');
      expect(gear.classList.contains('active')).toBe(false);
    });

    it('fires onSettingsClick when the gear is clicked', async () => {
      const onSettingsClick = vi.fn();
      render(NavPanel, { props: { nodes: testNodes, collapsed: false, onSettingsClick } });
      await fireEvent.click(screen.getByRole('button', { name: 'Settings' }));
      expect(onSettingsClick).toHaveBeenCalled();
    });
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
      // Scope to the menu item: with no community joined, the ZEB-553 item-13
      // empty-state CTA also carries "Redeem invite link" (a plain button, not a
      // menuitem), so a bare getByText would now be ambiguous.
      await fireEvent.click(screen.getByRole('menuitem', { name: /Redeem invite/i }));
      expect(onRedeemInvite).toHaveBeenCalled();
    });

    it('does not show "Post a vine" when onPostVine is not wired (ZEB-559)', async () => {
      render(NavPanel, { props: fabBaseProps });
      await fireEvent.click(screen.getByLabelText(/Create new/i));
      expect(screen.queryByText(/Post a vine/i)).toBeNull();
    });

    it('shows + wires "Post a vine" when onPostVine is provided (ZEB-559)', async () => {
      const onPostVine = vi.fn();
      render(NavPanel, { props: { ...fabBaseProps, onPostVine } });
      await fireEvent.click(screen.getByLabelText(/Create new/i));
      await fireEvent.click(screen.getByRole('menuitem', { name: /Post a vine/i }));
      expect(onPostVine).toHaveBeenCalled();
      expect(screen.queryByText(/Post a vine/i)).toBeNull();
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
        mentionCount: 0,
        unreadLevel: 'none',
        lastActivity: 1000,
      },
    ];

    it('renders a community-kind node with its name and a letter chip (ZEB-606)', () => {
      const { container } = render(NavPanel, { props: { nodes: communityNodes, collapsed: false } });
      expect(screen.getByText('IPFS Crew')).toBeTruthy();
      expect(container.textContent).not.toContain('🏛️');
      expect(container.querySelector('.community-chip')?.textContent?.trim()).toBe('I');
    });

    it('clicking a community node fires onNodeClick with the node id', async () => {
      const onNodeClick = vi.fn();
      render(NavPanel, { props: { nodes: communityNodes, collapsed: false, onNodeClick } });
      await fireEvent.click(screen.getByText('IPFS Crew'));
      expect(onNodeClick).toHaveBeenCalledWith('comm-1');
    });
  });

  describe('Section headers (ZEB-606)', () => {
    const base = { expanded: false, unreadCount: 0, mentionCount: 0, unreadLevel: 'none' as const };
    const mixedNodes: NavNode[] = [
      { id: 'work', parentId: null, type: 'folder', name: 'Work', ...base, expanded: true, lastActivity: 3 },
      { id: 'comm-1', parentId: null, type: 'community', name: 'IPFS Crew', ...base, lastActivity: 2 },
      { id: 'dm-1', parentId: null, type: 'dm', name: 'alice', ...base, lastActivity: 1 },
    ];

    it('shows Communities and Direct messages headers when those groups exist', () => {
      render(NavPanel, { props: { nodes: mixedNodes, collapsed: false } });
      expect(screen.getByText('Communities')).toBeTruthy();
      expect(screen.getByText('Direct messages')).toBeTruthy();
    });

    it('omits headers for empty groups', () => {
      render(NavPanel, { props: { nodes: [mixedNodes[0]], collapsed: false } });
      expect(screen.queryByText('Communities')).toBeNull();
      expect(screen.queryByText('Direct messages')).toBeNull();
    });

    it('renders un-headed folder trees before the Communities section', () => {
      const { container } = render(NavPanel, { props: { nodes: mixedNodes, collapsed: false } });
      const text = container.querySelector('.nav-tree-container')?.textContent ?? '';
      expect(text.indexOf('Work')).toBeGreaterThanOrEqual(0);
      expect(text.indexOf('Work')).toBeLessThan(text.indexOf('Communities'));
    });

    it('renders the Communities header before the Direct messages header', () => {
      const { container } = render(NavPanel, { props: { nodes: mixedNodes, collapsed: false } });
      const text = container.querySelector('.nav-tree-container')?.textContent ?? '';
      expect(text.indexOf('Communities')).toBeGreaterThanOrEqual(0);
      expect(text.indexOf('Communities')).toBeLessThan(text.indexOf('Direct messages'));
    });

    it('group-chat nodes land under Direct messages', () => {
      const nodes: NavNode[] = [
        { id: 'g1', parentId: null, type: 'group-chat', name: 'weekend crew', ...base, lastActivity: 1 },
      ];
      render(NavPanel, { props: { nodes, collapsed: false } });
      expect(screen.getByText('Direct messages')).toBeTruthy();
      expect(screen.queryByText('Communities')).toBeNull();
    });
  });

  describe('Zero-community empty-state redeem CTA (ZEB-553 item 13)', () => {
    // A fresh tester may have a DM but no community joined — the CTA targets the
    // absence of a *community* specifically (DMs don't count), since its purpose
    // is onboarding into a first community via an invite.
    const dmOnlyNodes: NavNode[] = [
      {
        id: 'eve-dm',
        parentId: null,
        type: 'dm',
        name: 'Eve',
        expanded: false,
        unreadCount: 0,
        mentionCount: 0,
        unreadLevel: 'none',
        lastActivity: 300,
      },
    ];
    const communityNodes: NavNode[] = [
      {
        id: 'comm-1',
        parentId: null,
        type: 'community',
        name: 'IPFS Crew',
        expanded: true,
        unreadCount: 0,
        mentionCount: 0,
        unreadLevel: 'none',
        lastActivity: 1000,
      },
    ];

    it('shows the empty-state CTA when no community is joined', () => {
      render(NavPanel, { props: { nodes: dmOnlyNodes, collapsed: false, onRedeemInvite: vi.fn() } });
      expect(screen.getByText(/No communities yet/i)).toBeTruthy();
      // Menu is closed, so the only "Redeem invite link" affordance is the CTA.
      expect(screen.getByRole('button', { name: /Redeem invite link/i })).toBeTruthy();
    });

    it('clicking the CTA invokes onRedeemInvite (same flow as the FAB menu item)', async () => {
      const onRedeemInvite = vi.fn();
      render(NavPanel, { props: { nodes: dmOnlyNodes, collapsed: false, onRedeemInvite } });
      await fireEvent.click(screen.getByRole('button', { name: /Redeem invite link/i }));
      expect(onRedeemInvite).toHaveBeenCalled();
    });

    it('hides the CTA once at least one community is present', () => {
      render(NavPanel, { props: { nodes: communityNodes, collapsed: false, onRedeemInvite: vi.fn() } });
      expect(screen.queryByText(/No communities yet/i)).toBeNull();
    });

    it('hides the CTA while a search query is active (no-results state, not a zero-community home)', async () => {
      render(NavPanel, { props: { nodes: dmOnlyNodes, collapsed: false, onRedeemInvite: vi.fn() } });
      expect(screen.getByText(/No communities yet/i)).toBeTruthy();
      await fireEvent.input(screen.getByPlaceholderText('Search'), { target: { value: 'zzz' } });
      expect(screen.queryByText(/No communities yet/i)).toBeNull();
    });

    it('does not show the CTA outside messages mode', () => {
      render(NavPanel, {
        props: { nodes: dmOnlyNodes, collapsed: false, appMode: 'vines', onRedeemInvite: vi.fn() },
      });
      expect(screen.queryByText(/No communities yet/i)).toBeNull();
    });

    it('does not show the CTA when no onRedeemInvite handler is wired', () => {
      render(NavPanel, { props: { nodes: dmOnlyNodes, collapsed: false } });
      expect(screen.queryByText(/No communities yet/i)).toBeNull();
    });
  });

  describe('Left nav is communities-flat (ZEB-965)', () => {
    const base = { unreadCount: 0, mentionCount: 0, unreadLevel: 'none' as const };
    const expandedCommunity: NavNode[] = [
      { id: 'comm-1', parentId: null, type: 'community', name: 'IPFS Crew', expanded: true, ...base, lastActivity: 2 },
      { id: 'chan-1', parentId: 'comm-1', type: 'channel', name: 'general', expanded: false, ...base, lastActivity: 1 },
    ];

    // ZEB-965: the proposals row (ZEB-606), channel rows, and the ＋ add-channel
    // row (ZEB-663) all moved to CommunityView's right-hand ChannelsPanel.
    it('renders the community row but neither its channels nor a proposals row', () => {
      const { container } = render(NavPanel, {
        props: { nodes: expandedCommunity, collapsed: false },
      });
      expect(screen.getByText('IPFS Crew')).toBeTruthy();
      expect(screen.queryByText('general')).toBeNull();
      expect(container.querySelector('[data-testid="proposals-row-comm-1"]')).toBeNull();
      expect(container.querySelector('[data-testid="add-channel-row-comm-1"]')).toBeNull();
    });

    it('community rows carry no expand/collapse chevron', () => {
      render(NavPanel, { props: { nodes: expandedCommunity, collapsed: false } });
      expect(screen.queryByLabelText(/Collapse community|Expand community/)).toBeNull();
    });
  });

  describe('Notes as a footer mode button (ZEB-965)', () => {
    it('renders Notes inside the App-mode button group, not as a top nav row', () => {
      const { container } = render(NavPanel, {
        props: { nodes: [], collapsed: false, onSelectNotes: vi.fn() },
      });
      const group = container.querySelector('[role="group"][aria-label="App mode"]')!;
      const notesBtn = screen.getByRole('button', { name: 'Notes' });
      expect(group.contains(notesBtn)).toBe(true);
      expect(container.querySelector('.notes-nav-row')).toBeNull();
    });

    it('clicking Notes fires onSelectNotes and active state follows notesActive', async () => {
      const onSelectNotes = vi.fn();
      render(NavPanel, {
        props: { nodes: [], collapsed: false, onSelectNotes, notesActive: true },
      });
      const notesBtn = screen.getByRole('button', { name: 'Notes' });
      expect(notesBtn.getAttribute('aria-pressed')).toBe('true');
      await fireEvent.click(notesBtn);
      expect(onSelectNotes).toHaveBeenCalled();
    });

    it('Messages does not read active while Notes is the selected view', () => {
      render(NavPanel, {
        props: { nodes: [], collapsed: false, appMode: 'messages', onSelectNotes: vi.fn(), notesActive: true },
      });
      expect(screen.getByRole('button', { name: 'Messages' }).getAttribute('aria-pressed')).toBe('false');
    });

    it('renders no Notes button when onSelectNotes is not wired', () => {
      render(NavPanel, { props: { nodes: [], collapsed: false } });
      expect(screen.queryByRole('button', { name: 'Notes' })).toBeNull();
    });
  });

  describe('Identity chip (ZEB-606)', () => {
    it('renders the chip when identity is provided', () => {
      const { container } = render(NavPanel, {
        props: {
          nodes: [],
          collapsed: false,
          identity: { displayName: 'Jake Englund', ownerIdHex: 'ab'.repeat(16), selfOnline: true, selfSovereign: true },
        },
      });
      expect(container.querySelector('[data-testid="identity-chip"]')).toBeTruthy();
      expect(screen.getByText('Jake Englund')).toBeTruthy();
      expect(screen.getByText('● self-sovereign')).toBeTruthy();
    });

    it('renders no chip without identity (bare construction)', () => {
      const { container } = render(NavPanel, { props: { nodes: [], collapsed: false } });
      expect(container.querySelector('[data-testid="identity-chip"]')).toBeNull();
    });
  });
});

describe('Network Viz dev-flag gate (ZEB-659)', () => {
  it('hides the Network Viz button when showNetworkViz is false', () => {
    render(NavPanel, { props: { nodes: testNodes, collapsed: false, showNetworkViz: false } });
    expect(screen.queryByRole('button', { name: /open network visualization/i })).toBeNull();
  });

  it('shows the Network Viz button when showNetworkViz is true', () => {
    render(NavPanel, { props: { nodes: testNodes, collapsed: false, showNetworkViz: true } });
    expect(screen.getByRole('button', { name: /open network visualization/i })).toBeTruthy();
  });
});

describe('NavPanel — collapse persistence (ZEB-838)', () => {
  const community: NavNode = {
    id: 'c1',
    parentId: null,
    type: 'community',
    name: 'Alpha',
    expanded: true,
    unreadCount: 0,
    mentionCount: 0,
    unreadLevel: 'none',
  };
  const channel: NavNode = {
    id: 'c1-general',
    parentId: 'c1',
    type: 'channel',
    channelKind: 'text',
    name: 'chatroom',
    expanded: false,
    unreadCount: 0,
    mentionCount: 0,
    unreadLevel: 'none',
  };

  // ZEB-965: community expand/collapse is retired — the left nav renders
  // communities flat and channels live in CommunityView's ChannelsPanel. The
  // ZEB-838 override machinery remains for FOLDER state (tests below); the two
  // community-collapse persistence tests it carried are superseded by this
  // flatness regression check.
  it('never renders channel rows in the left nav, before or after a nodes-prop rebuild', async () => {
    const { rerender } = render(NavPanel, {
      props: { nodes: [community, channel], collapsed: false },
    });
    expect(screen.getByText('Alpha')).toBeTruthy();
    expect(screen.queryByText('chatroom')).toBeNull();

    // A backend update rebuilds the nodes prop (unread bump) — still flat.
    await rerender({
      nodes: [
        { ...community, unreadCount: 3, unreadLevel: 'quiet' },
        { ...channel },
      ],
      collapsed: false,
    });
    await tick();
    expect(screen.queryByText('chatroom')).toBeNull();
  });

  it('keeps a folder display-mode change after a nodes-prop rebuild', async () => {
    // CodeRabbit (#573): display-mode rides the same override path as expand —
    // it must survive a backend re-sync too.
    const folder: NavNode = {
      id: 'f1',
      parentId: null,
      type: 'folder',
      name: 'Projects',
      expanded: true,
      unreadCount: 0,
      mentionCount: 0,
      unreadLevel: 'none',
    };
    const { container, rerender } = render(NavPanel, {
      props: { nodes: [folder], collapsed: false },
    });
    const modeToggle = () => container.querySelector('.mode-toggle') as HTMLButtonElement;
    const before = modeToggle().textContent;

    await fireEvent.click(modeToggle()); // cycle the display mode to a new value
    const after = modeToggle().textContent;
    expect(after).not.toBe(before);

    // A backend update rebuilds the prop (folder carries no display-mode of its own).
    await rerender({ nodes: [{ ...folder }], collapsed: false });
    expect(modeToggle().textContent).toBe(after);
  });

  it('keeps a folder sort-order change after a nodes-prop rebuild', async () => {
    // CodeRabbit (#573): sort-order too.
    const folder: NavNode = {
      id: 'f2',
      parentId: null,
      type: 'folder',
      name: 'Teams',
      expanded: true,
      unreadCount: 0,
      mentionCount: 0,
      unreadLevel: 'none',
    };
    const { container, rerender } = render(NavPanel, {
      props: { nodes: [folder], collapsed: false },
    });
    const sortTrigger = () => container.querySelector('.sort-trigger') as HTMLButtonElement;

    await fireEvent.click(sortTrigger()); // open the sort menu
    const alpha = Array.from(container.querySelectorAll('.sort-option')).find((o) =>
      /A-Z/.test(o.textContent ?? ''),
    ) as HTMLButtonElement;
    await fireEvent.click(alpha); // select alphabetical (a non-default) → menu closes

    // Rebuild the prop (folder carries no sort-order of its own).
    await rerender({ nodes: [{ ...folder }], collapsed: false });

    // Reopen the menu; the alphabetical option must still be the active one.
    await fireEvent.click(sortTrigger());
    const active = container.querySelector('.sort-option.active') as HTMLButtonElement;
    expect(active?.textContent).toMatch(/A-Z/);
  });
});
