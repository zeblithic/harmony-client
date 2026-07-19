import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent, within } from '@testing-library/svelte';

// The container's only job is tab routing. Mock the inner panels to a no-op
// stub (they each fire Tauri IPC / construct services on mount) so this test
// isolates which section is shown per tab, not the panels' internals.
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
vi.mock('../ProfileEditor.svelte', () => import('./settings-panel-stub.svelte'));
vi.mock('../AppearanceSettings.svelte', () => import('./settings-panel-stub.svelte'));
vi.mock('../IdentityPanel.svelte', () => import('./settings-panel-stub.svelte'));
vi.mock('../DevicesPanel.svelte', () => import('./settings-panel-stub.svelte'));
vi.mock('../NotificationSettingsPanel.svelte', () => import('./settings-panel-stub.svelte'));
vi.mock('../NetworkDiscoverabilitySettings.svelte', () => import('./settings-panel-stub.svelte'));
vi.mock('../LeftCommunitiesPanel.svelte', () => import('./settings-panel-stub.svelte'));
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

const TAB_LABELS = [
  'Profile',
  'Appearance',
  'Account',
  'Notifications',
  'Voice',
  'Network',
  'Communities',
  'Friends',
];

describe('SettingsPanel tab routing', () => {
  it('renders the tabs in order with Profile active by default', () => {
    render(SettingsPanel, { props: baseProps });
    const tabs = screen.getAllByRole('tab');
    expect(tabs.map((t) => t.textContent?.trim())).toEqual(TAB_LABELS);
    expect(screen.getByRole('tab', { name: 'Profile' }).getAttribute('aria-selected')).toBe('true');
    // Exactly one tabpanel is rendered (the active one), labelled by its tab.
    expect(screen.getByRole('tabpanel').getAttribute('aria-labelledby')).toBe(
      'settings-tab-profile',
    );
  });

  it('shows only the Profile section in the visible panel by default', () => {
    render(SettingsPanel, { props: baseProps });
    // All sections stay mounted (inactive ones are `hidden`), so scope to the
    // single visible tabpanel — getByRole excludes `hidden` elements.
    expect(within(screen.getByRole('tabpanel')).getAllByTestId('panel-stub')).toHaveLength(1);
  });

  it('keeps inactive sections mounted across tab switches (no state-resetting unmount)', async () => {
    const { container } = render(SettingsPanel, { props: baseProps });
    await fireEvent.click(screen.getByRole('tab', { name: 'Notifications' }));
    // The Profile section must remain in the DOM (just hidden), not be unmounted
    // — unmounting would reset ProfileEditor drafts / wizard progress on return.
    const profilePanel = container.querySelector('#settings-tabpanel-profile');
    expect(profilePanel).not.toBeNull();
    expect(profilePanel?.hasAttribute('hidden')).toBe(true);
    // And the now-active panel is visible (not hidden).
    const notifPanel = container.querySelector('#settings-tabpanel-notifications');
    expect(notifPanel?.hasAttribute('hidden')).toBe(false);
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

  it('navigates tabs with Arrow/Home/End keys and keeps a roving tabindex', async () => {
    render(SettingsPanel, { props: baseProps });
    const tab = (name: string) => screen.getByRole('tab', { name });
    // Roving tabindex: only the active tab is in the tab order.
    expect(tab('Profile').getAttribute('tabindex')).toBe('0');
    expect(tab('Account').getAttribute('tabindex')).toBe('-1');

    await fireEvent.keyDown(tab('Profile'), { key: 'ArrowRight' });
    expect(tab('Appearance').getAttribute('aria-selected')).toBe('true');
    expect(tab('Appearance').getAttribute('tabindex')).toBe('0');
    expect(tab('Profile').getAttribute('tabindex')).toBe('-1');

    await fireEvent.keyDown(tab('Appearance'), { key: 'End' });
    expect(tab('Friends').getAttribute('aria-selected')).toBe('true');

    await fireEvent.keyDown(tab('Friends'), { key: 'Home' });
    expect(tab('Profile').getAttribute('aria-selected')).toBe('true');

    // ArrowLeft from the first tab wraps to the last.
    await fireEvent.keyDown(tab('Profile'), { key: 'ArrowLeft' });
    expect(tab('Friends').getAttribute('aria-selected')).toBe('true');
  });

  it('groups Identity + Devices under the Account tab (two panels in the visible section)', async () => {
    render(SettingsPanel, { props: baseProps });
    await fireEvent.click(screen.getByRole('tab', { name: 'Account' }));
    expect(within(screen.getByRole('tabpanel')).getAllByTestId('panel-stub')).toHaveLength(2);
  });

  it('renders a close control in the header that fires onClose (ZEB-569)', async () => {
    const onClose = vi.fn();
    render(SettingsPanel, { props: { ...baseProps, onClose } });
    await fireEvent.click(screen.getByRole('button', { name: 'Close settings' }));
    expect(onClose).toHaveBeenCalledTimes(1);
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
    expect(within(screen.getByRole('tabpanel')).getAllByTestId('panel-stub')).toHaveLength(2);
  });
});
