import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import ChannelsPanel from '../ChannelsPanel.svelte';
import type { NavNode } from '../../types';

// ZEB-965: the per-community channel list lives in CommunityView's right-hand
// column (toggled with the Members list), replacing the left-nav channel rows.
// Fixed order: proposals row on top, channels in nav order, ＋ add-channel last.

const base = { unreadCount: 0, mentionCount: 0, unreadLevel: 'none' as const };
const nodes: NavNode[] = [
  { id: 'c1', parentId: null, type: 'community', name: 'Crew', expanded: true, ...base, lastActivity: 2 },
  { id: 'ch1', parentId: 'c1', type: 'channel', name: 'general', channelKind: 'text', expanded: false, ...base },
  { id: 'ch2', parentId: 'c1', type: 'channel', name: 'voice-hall', channelKind: 'voice', expanded: false, ...base },
  // A different community's channel must never leak into c1's panel.
  { id: 'other', parentId: 'c2', type: 'channel', name: 'other-chan', channelKind: 'text', expanded: false, ...base },
];

function renderPanel(props: Record<string, unknown> = {}) {
  return render(ChannelsPanel, {
    props: { nodes, communityId: 'c1', selectedChannelId: null, ...props },
  });
}

describe('ChannelsPanel (ZEB-965)', () => {
  it('renders the panel with proposals on top, channels in order, and no leakage', () => {
    const { container } = renderPanel();
    expect(screen.getByRole('complementary', { name: 'Community channels' })).toBeTruthy();
    const text = container.textContent ?? '';
    const proposalsPos = text.indexOf('proposals');
    const generalPos = text.indexOf('general');
    const voicePos = text.indexOf('voice-hall');
    expect(proposalsPos).toBeGreaterThanOrEqual(0);
    expect(proposalsPos).toBeLessThan(generalPos);
    expect(generalPos).toBeLessThan(voicePos);
    expect(screen.queryByText('other-chan')).toBeNull();
  });

  it('marks the selected channel row active', () => {
    const { container } = renderPanel({ selectedChannelId: 'ch1' });
    expect(container.querySelector('[data-testid="nav-row-ch1"]')?.classList.contains('active')).toBe(true);
    expect(container.querySelector('[data-testid="nav-row-ch2"]')?.classList.contains('active')).toBe(false);
  });

  it('clicking a channel row fires onSelectChannel with the channel id', async () => {
    const onSelectChannel = vi.fn();
    const { container } = renderPanel({ onSelectChannel });
    await fireEvent.click(container.querySelector('[data-testid="nav-row-ch2"]')!);
    expect(onSelectChannel).toHaveBeenCalledWith('ch2');
  });

  describe('proposals row', () => {
    it('shows a count badge for a positive count and none for zero/unknown', () => {
      const { container, unmount } = renderPanel({ proposalCount: 3 });
      expect(container.querySelector('.count-badge')?.textContent).toBe('3');
      unmount();
      const { container: c2 } = renderPanel({ proposalCount: 0 });
      expect(c2.querySelector('[data-testid="proposals-row-c1"]')).toBeTruthy();
      expect(c2.querySelector('.count-badge')).toBeNull();
    });

    it('is active when proposalsActive and clicking fires onSelectProposals', async () => {
      const onSelectProposals = vi.fn();
      const { container } = renderPanel({ proposalsActive: true, onSelectProposals });
      const row = container.querySelector('[data-testid="proposals-row-c1"]')!;
      expect(row.classList.contains('active')).toBe(true);
      await fireEvent.click(row);
      expect(onSelectProposals).toHaveBeenCalled();
    });
  });

  describe('＋ add-channel row gating (moved from the left nav, ZEB-663 semantics)', () => {
    it('renders last when the viewer can manage, and fires onAddChannel', async () => {
      const onAddChannel = vi.fn();
      const { container } = renderPanel({ canManage: true, onAddChannel });
      const row = container.querySelector('[data-testid="add-channel-row-c1"]')!;
      expect(row).toBeTruthy();
      const text = container.textContent ?? '';
      expect(text.indexOf('add channel')).toBeGreaterThan(text.indexOf('voice-hall'));
      await fireEvent.click(row);
      expect(onAddChannel).toHaveBeenCalled();
    });

    it('is hidden when the viewer cannot manage', () => {
      const { container } = renderPanel({ canManage: false, onAddChannel: vi.fn() });
      expect(container.querySelector('[data-testid="add-channel-row-c1"]')).toBeNull();
    });

    it('Space activation suppresses the browser default and adds a channel', () => {
      const onAddChannel = vi.fn();
      const { container } = renderPanel({ canManage: true, onAddChannel });
      const row = container.querySelector('[data-testid="add-channel-row-c1"]')!;
      const evt = new KeyboardEvent('keydown', { key: ' ', bubbles: true, cancelable: true });
      row.dispatchEvent(evt);
      expect(evt.defaultPrevented).toBe(true);
      expect(onAddChannel).toHaveBeenCalled();
    });
  });

  describe('channel management trigger (⋯) gating', () => {
    it('shows the per-channel menu trigger only when the viewer can manage', () => {
      const { container, unmount } = renderPanel({ canManage: true });
      expect(container.querySelector('[data-testid="channel-menu-trigger-ch1"]')).toBeTruthy();
      unmount();
      const { container: c2 } = renderPanel({ canManage: false });
      expect(c2.querySelector('[data-testid="channel-menu-trigger-ch1"]')).toBeNull();
    });
  });

  describe('initial-sync affordance (ZEB-949 parity)', () => {
    it('shows "Syncing channels…" while syncing with no channels yet', () => {
      const bare: NavNode[] = [nodes[0]];
      render(ChannelsPanel, {
        props: { nodes: bare, communityId: 'c1', selectedChannelId: null, initialSyncing: true },
      });
      expect(screen.getByTestId('channels-panel-syncing')).toBeTruthy();
    });

    it('does not show the syncing banner once channels are present', () => {
      renderPanel({ initialSyncing: true });
      expect(screen.queryByTestId('channels-panel-syncing')).toBeNull();
      expect(screen.getByText('general')).toBeTruthy();
    });
  });
});
