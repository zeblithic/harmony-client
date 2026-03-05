import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import NotificationSettingsPanel from '../NotificationSettingsPanel.svelte';
import { NotificationService } from '../../notification-service';
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
});
