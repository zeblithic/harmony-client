import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import NotificationSettingsPanel from '../NotificationSettingsPanel.svelte';
import { NotificationService } from '../../notification-service';
import { shortId } from '../../short-addr';
import type { NavNode, Peer } from '../../types';

const mockPeers: Peer[] = [
  { address: 'alice-addr', displayName: 'Alice' },
  { address: 'bob-addr', displayName: 'Bob' },
];

const mockCommunities: NavNode[] = [
  {
    id: 'work',
    parentId: null,
    type: 'folder',
    name: 'Work',
    expanded: true,
    unreadCount: 0,
    mentionCount: 0,
    unreadLevel: 'none',
  },
];

describe('NotificationSettingsPanel', () => {
  it('renders with Global tab active by default', () => {
    const svc = new NotificationService();
    render(NotificationSettingsPanel, {
      props: { service: svc, peers: mockPeers, communities: mockCommunities },
    });
    expect(screen.getByText('Global')).toBeTruthy();
    expect(screen.getByText('Quiet messages')).toBeTruthy();
    expect(screen.getByText('Standard messages')).toBeTruthy();
    expect(screen.getByText('Loud messages')).toBeTruthy();
  });

  it('displays current global policy values', () => {
    const svc = new NotificationService();
    render(NotificationSettingsPanel, {
      props: { service: svc, peers: mockPeers, communities: mockCommunities },
    });
    const selects = screen.getAllByRole('combobox');
    expect(selects).toHaveLength(3);
  });

  it('switches to Peers tab and shows peer list', async () => {
    const svc = new NotificationService();
    render(NotificationSettingsPanel, {
      props: { service: svc, peers: mockPeers, communities: mockCommunities },
    });
    const peersTab = screen.getByText('Peers');
    await fireEvent.click(peersTab);
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('Bob')).toBeTruthy();
  });

  it('switches to Communities tab and shows community list', async () => {
    const svc = new NotificationService();
    render(NotificationSettingsPanel, {
      props: { service: svc, peers: mockPeers, communities: mockCommunities },
    });
    const commTab = screen.getByText('Communities');
    await fireEvent.click(commTab);
    expect(screen.getByText('Work')).toBeTruthy();
  });

  it('exposes the sub-tabs as a WAI-ARIA tablist with roving tabindex + arrow-key nav (finding 15)', async () => {
    const svc = new NotificationService();
    render(NotificationSettingsPanel, {
      props: { service: svc, peers: mockPeers, communities: mockCommunities },
    });
    expect(screen.getByRole('tablist')).toBeTruthy();
    const tabs = screen.getAllByRole('tab');
    expect(tabs).toHaveLength(3);
    // Roving tabindex: only the active (Global) tab is in the tab order.
    expect(tabs[0].getAttribute('aria-selected')).toBe('true');
    expect(tabs[0].getAttribute('tabindex')).toBe('0');
    expect(tabs[1].getAttribute('tabindex')).toBe('-1');
    // ArrowRight moves selection to Communities (selection follows focus).
    await fireEvent.keyDown(tabs[0], { key: 'ArrowRight' });
    expect(tabs[1].getAttribute('aria-selected')).toBe('true');
    expect(tabs[1].getAttribute('tabindex')).toBe('0');
    expect(screen.getByText('Work')).toBeTruthy(); // Communities panel content
    // The content is a labelled tabpanel pointing at the active tab.
    expect(screen.getByRole('tabpanel').getAttribute('aria-labelledby')).toBe('notif-tab-communities');
    // Home jumps back to the first tab.
    await fireEvent.keyDown(tabs[1], { key: 'Home' });
    expect(tabs[0].getAttribute('aria-selected')).toBe('true');
  });

  it('calls onClose when close button is clicked', async () => {
    const svc = new NotificationService();
    const onClose = vi.fn();
    render(NotificationSettingsPanel, {
      props: { service: svc, peers: mockPeers, communities: mockCommunities, onClose },
    });
    const closeBtn = screen.getByLabelText('Close settings');
    await fireEvent.click(closeBtn);
    expect(onClose).toHaveBeenCalled();
  });

  // ZEB-962: the peer override row rendered `peer.displayName` raw — a
  // whitespace-only broadcast name showed a blank row. Floor to short hex.
  it('floors a whitespace-only peer name to short hex, not blank', async () => {
    const BLANK = 'ee'.repeat(16);
    const svc = new NotificationService();
    render(NotificationSettingsPanel, {
      props: {
        service: svc,
        peers: [{ address: BLANK, displayName: '   ' }],
        communities: mockCommunities,
      },
    });
    await fireEvent.click(screen.getByText('Peers'));
    expect(screen.getByText(shortId(BLANK))).toBeTruthy();
  });
});
