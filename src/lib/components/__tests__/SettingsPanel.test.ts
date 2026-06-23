import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';

// The container's only job is tab routing. Mock the six inner panels to a no-op
// stub (they each fire Tauri IPC / construct services on mount) so this test
// isolates which section is shown per tab, not the panels' internals.
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
vi.mock('../ProfileEditor.svelte', () => import('./settings-panel-stub.svelte'));
vi.mock('../IdentityPanel.svelte', () => import('./settings-panel-stub.svelte'));
vi.mock('../DevicesPanel.svelte', () => import('./settings-panel-stub.svelte'));
vi.mock('../NotificationSettingsPanel.svelte', () => import('./settings-panel-stub.svelte'));
vi.mock('../NetworkDiscoverabilitySettings.svelte', () => import('./settings-panel-stub.svelte'));
vi.mock('../FriendsPanel.svelte', () => import('./settings-panel-stub.svelte'));

import SettingsPanel from '../SettingsPanel.svelte';
import type { Profile } from '../../types';
import type { NotificationService } from '../../notification-service';
import type { FriendService } from '../../friend-service';

const baseProps = {
  profile: { address: 'me', displayName: 'Me' } as Profile,
  onProfileSave: () => {},
  notificationService: {} as unknown as NotificationService,
  peers: [],
  communities: [],
  onClose: () => {},
  friendService: {} as unknown as FriendService,
};

const TAB_LABELS = ['Profile', 'Account', 'Notifications', 'Network', 'Friends'];

describe('SettingsPanel tab routing', () => {
  it('renders the five tabs in order with Profile active by default', () => {
    render(SettingsPanel, { props: baseProps });
    const tabs = screen.getAllByRole('tab');
    expect(tabs.map((t) => t.textContent?.trim())).toEqual(TAB_LABELS);
    expect(screen.getByRole('tab', { name: 'Profile' }).getAttribute('aria-selected')).toBe('true');
    // Exactly one tabpanel is rendered (the active one), labelled by its tab.
    expect(screen.getByRole('tabpanel').getAttribute('aria-labelledby')).toBe(
      'settings-tab-profile',
    );
  });

  it('shows only the Profile section content by default (single panel mounted)', () => {
    render(SettingsPanel, { props: baseProps });
    expect(screen.getAllByTestId('panel-stub')).toHaveLength(1);
  });

  for (const label of TAB_LABELS) {
    const tabId = label.toLowerCase();
    it(`activates the ${label} tab and routes to its tabpanel on click`, async () => {
      render(SettingsPanel, { props: baseProps });
      await fireEvent.click(screen.getByRole('tab', { name: label }));

      expect(screen.getByRole('tab', { name: label }).getAttribute('aria-selected')).toBe('true');
      // The other tabs are deselected.
      for (const other of TAB_LABELS.filter((l) => l !== label)) {
        expect(screen.getByRole('tab', { name: other }).getAttribute('aria-selected')).toBe('false');
      }
      expect(screen.getByRole('tabpanel').getAttribute('aria-labelledby')).toBe(
        `settings-tab-${tabId}`,
      );
    });
  }

  it('groups Identity + Devices under the Account tab (two panels mounted)', async () => {
    render(SettingsPanel, { props: baseProps });
    await fireEvent.click(screen.getByRole('tab', { name: 'Account' }));
    expect(screen.getAllByTestId('panel-stub')).toHaveLength(2);
  });

  it('opens on a caller-supplied active tab (the backup-export → Account route)', () => {
    // The backup-staleness banner drives Settings to Account via this prop so
    // IdentityPanel mounts and catches the export event; opening there directly
    // must select that tab without a click.
    render(SettingsPanel, { props: { ...baseProps, activeTab: 'account' as const } });
    expect(screen.getByRole('tab', { name: 'Account' }).getAttribute('aria-selected')).toBe('true');
    expect(screen.getByRole('tab', { name: 'Profile' }).getAttribute('aria-selected')).toBe(
      'false',
    );
    expect(screen.getByRole('tabpanel').getAttribute('aria-labelledby')).toBe('settings-tab-account');
    expect(screen.getAllByTestId('panel-stub')).toHaveLength(2);
  });
});
