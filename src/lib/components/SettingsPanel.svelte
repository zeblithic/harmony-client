<script lang="ts">
  /**
   * ZEB-545 — Settings container. Decomposes the former long-scroll settings
   * stack into five tabs so users aren't scrolling one flat column. Each section
   * is an already-isolated panel component; this container owns only the
   * active-tab state and forwards props straight through — no inner-panel logic
   * lives here. Grouping (confirmed with Jake 2026-06-23):
   *   Profile · Account (Identity + Devices) · Notifications · Network · Friends.
   * Default tab = Profile (lightest, most-visited).
   *
   * It sits inside Layout's `.media-area` scrolling column, so the container
   * adds no scroll of its own (avoids a double scrollbar); the tab bar is
   * sticky so it stays visible as a tall tab (e.g. Account/Identity) scrolls.
   */
  import type { Profile, Peer, NavNode } from '../types';
  import type { NotificationService } from '../notification-service';
  import type { TrustService } from '../trust-service';
  import type { FriendService } from '../friend-service';
  import type { MemberCardService } from '../member-card-service';
  import type { OpenCardPayload } from './MemberRow.svelte';

  import ProfileEditor from './ProfileEditor.svelte';
  import IdentityPanel from './IdentityPanel.svelte';
  import DevicesPanel from './DevicesPanel.svelte';
  import NotificationSettingsPanel from './NotificationSettingsPanel.svelte';
  import NetworkDiscoverabilitySettings from './NetworkDiscoverabilitySettings.svelte';
  import FriendsPanel from './FriendsPanel.svelte';

  type SettingsTab = 'profile' | 'account' | 'notifications' | 'network' | 'friends';

  let {
    profile,
    onProfileSave,
    notificationService,
    trustService,
    peers,
    communities,
    onClose,
    onTrustChange,
    friendService,
    friendCardService,
    onOpenCard,
    activeTab = $bindable('profile'),
  }: {
    profile: Profile;
    onProfileSave: (profile: Profile) => void;
    notificationService: NotificationService;
    trustService?: TrustService;
    peers: Peer[];
    communities: NavNode[];
    onClose?: () => void;
    onTrustChange?: () => void;
    friendService: FriendService;
    friendCardService?: MemberCardService;
    onOpenCard?: (payload: OpenCardPayload, ev: MouseEvent) => void;
    /**
     * Active tab. Bindable so the app can route to a specific section — e.g. the
     * backup-staleness banner opens Settings on `account` (IdentityPanel) to
     * surface the backup wizard. Defaults to the lightest, most-visited section.
     */
    activeTab?: SettingsTab;
  } = $props();

  const TABS: { id: SettingsTab; label: string }[] = [
    { id: 'profile', label: 'Profile' },
    { id: 'account', label: 'Account' },
    { id: 'notifications', label: 'Notifications' },
    { id: 'network', label: 'Network' },
    { id: 'friends', label: 'Friends' },
  ];
</script>

<div class="settings-panel">
  <div class="tabs" role="tablist" aria-label="Settings sections">
    {#each TABS as tab (tab.id)}
      <button
        class="tab"
        class:active={activeTab === tab.id}
        role="tab"
        id="settings-tab-{tab.id}"
        aria-selected={activeTab === tab.id}
        onclick={() => {
          activeTab = tab.id;
        }}
      >
        {tab.label}
      </button>
    {/each}
  </div>

  <div
    class="tab-content"
    role="tabpanel"
    id="settings-tabpanel-{activeTab}"
    aria-labelledby="settings-tab-{activeTab}"
  >
    {#if activeTab === 'profile'}
      <ProfileEditor {profile} onSave={onProfileSave} />
    {:else if activeTab === 'account'}
      <IdentityPanel />
      <DevicesPanel />
    {:else if activeTab === 'notifications'}
      <NotificationSettingsPanel
        service={notificationService}
        {trustService}
        {peers}
        {communities}
        {onClose}
        {onTrustChange}
      />
    {:else if activeTab === 'network'}
      <NetworkDiscoverabilitySettings />
    {:else if activeTab === 'friends'}
      <FriendsPanel service={friendService} cardService={friendCardService} {onOpenCard} />
    {/if}
  </div>
</div>

<style>
  /* Mirrors NotificationSettingsPanel's internal tab styling so the outer
     settings tabs read as the same control. */
  .tabs {
    display: flex;
    border-bottom: 1px solid var(--border);
    position: sticky;
    top: 0;
    background: var(--bg-secondary);
    z-index: 1;
  }

  .tab {
    flex: 1;
    padding: 8px 12px;
    border: none;
    background: none;
    color: var(--text-muted);
    font-size: 13px;
    cursor: pointer;
    border-bottom: 2px solid transparent;
    white-space: nowrap;
  }

  .tab.active {
    color: var(--text-primary);
    border-bottom-color: var(--accent);
  }

  .tab:hover {
    color: var(--text-secondary);
  }

  .tab-content {
    padding-top: 12px;
  }
</style>
