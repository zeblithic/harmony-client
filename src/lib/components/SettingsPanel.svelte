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
  import type { DmInviteService } from '../dm-invite-service';
  import type { MemberCardService } from '../member-card-service';
  import type { CommunityService } from '../community-service';
  import type { OpenCardPayload } from './MemberRow.svelte';

  import ProfileEditor from './ProfileEditor.svelte';
  import AppearanceSettings from './AppearanceSettings.svelte';
  import IdentityPanel from './IdentityPanel.svelte';
  import DevicesPanel from './DevicesPanel.svelte';
  import NotificationSettingsPanel from './NotificationSettingsPanel.svelte';
  import NetworkDiscoverabilitySettings from './NetworkDiscoverabilitySettings.svelte';
  import IrohRelaySettings from './IrohRelaySettings.svelte';
  import LeftCommunitiesPanel from './LeftCommunitiesPanel.svelte';
  import FriendsPanel from './FriendsPanel.svelte';

  type SettingsTab =
    | 'profile'
    | 'appearance'
    | 'account'
    | 'notifications'
    | 'network'
    | 'communities'
    | 'friends';

  let {
    profile,
    onProfileSave,
    ownerIdHex,
    notificationService,
    trustService,
    peers,
    communities,
    onClose,
    onTrustChange,
    friendService,
    friendCardService,
    dmInviteService,
    communityService,
    onOpenCard,
    activeTab = $bindable('profile'),
  }: {
    profile: Profile;
    onProfileSave: (profile: Profile) => void;
    /** ZEB-567: canonical owner_id hex, threaded to ProfileEditor's self-avatar. */
    ownerIdHex?: string;
    notificationService: NotificationService;
    trustService?: TrustService;
    peers: Peer[];
    communities: NavNode[];
    onClose?: () => void;
    onTrustChange?: () => void;
    friendService: FriendService;
    friendCardService?: MemberCardService;
    /** ZEB-236 T7: shared DM-invite service, forwarded straight to FriendsPanel
     *  for its "DM invites" pending section. Optional (existing mounts/tests). */
    dmInviteService?: DmInviteService;
    /** ZEB-435: shared community service for the left-communities management
     *  tab. Optional (existing mounts/tests); the tab renders empty without it. */
    communityService?: CommunityService;
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
    { id: 'appearance', label: 'Appearance' },
    { id: 'account', label: 'Account' },
    { id: 'notifications', label: 'Notifications' },
    { id: 'network', label: 'Network' },
    { id: 'communities', label: 'Communities' },
    { id: 'friends', label: 'Friends' },
  ];

  function focusTab(id: SettingsTab): void {
    // Every tab always renders, so the element exists; focus() works regardless
    // of the roving tabindex value at this instant.
    document.getElementById(`settings-tab-${id}`)?.focus();
  }

  // WAI-ARIA tablist keyboard interaction (horizontal, automatic activation):
  // Left/Right move between tabs (wrapping), Home/End jump to the ends, and
  // selection follows focus. Paired with a roving tabindex (only the active tab
  // is in the tab order) so Tab enters the tablist once and arrows navigate it.
  function handleTabKey(e: KeyboardEvent, index: number): void {
    let target: number | null = null;
    if (e.key === 'ArrowRight') target = (index + 1) % TABS.length;
    else if (e.key === 'ArrowLeft') target = (index - 1 + TABS.length) % TABS.length;
    else if (e.key === 'Home') target = 0;
    else if (e.key === 'End') target = TABS.length - 1;
    if (target === null) return;
    e.preventDefault();
    activeTab = TABS[target].id;
    focusTab(activeTab);
  }
</script>

<div class="settings-panel">
  <!-- ZEB-569: explicit close control (matches NotificationSettingsPanel /
       CommunitySettingsPanel) so Settings isn't dismiss-only via the nav gear. -->
  <div class="settings-header">
    <h3>Settings</h3>
    <button class="close-btn" aria-label="Close settings" onclick={() => onClose?.()}>&#x2715;</button>
  </div>
  <div class="tabs" role="tablist" aria-label="Settings sections">
    {#each TABS as tab, i (tab.id)}
      <button
        class="tab"
        class:active={activeTab === tab.id}
        role="tab"
        id="settings-tab-{tab.id}"
        aria-selected={activeTab === tab.id}
        aria-controls="settings-tabpanel-{tab.id}"
        tabindex={activeTab === tab.id ? 0 : -1}
        onclick={() => {
          activeTab = tab.id;
        }}
        onkeydown={(e) => handleTabKey(e, i)}
      >
        {tab.label}
      </button>
    {/each}
  </div>

  <!--
    Every section stays mounted; inactive panels are `hidden` rather than removed
    (note: NOT an `{#if activeTab}` swap). Unmounting on tab switch would reset
    each panel's component-local `$state` — wiping ProfileEditor drafts, the
    IdentityPanel backup wizard mid-flow, or FriendsPanel add-friend/nickname
    edits. This also restores the pre-tabs mount behavior (all sections mounted
    while Settings is open) and keeps `aria-controls` targets present.
  -->
  <div
    class="tab-content"
    role="tabpanel"
    id="settings-tabpanel-profile"
    aria-labelledby="settings-tab-profile"
    hidden={activeTab !== 'profile'}
  >
    <ProfileEditor {profile} {ownerIdHex} onSave={onProfileSave} />
  </div>
  <div
    class="tab-content"
    role="tabpanel"
    id="settings-tabpanel-appearance"
    aria-labelledby="settings-tab-appearance"
    hidden={activeTab !== 'appearance'}
  >
    <AppearanceSettings />
  </div>
  <div
    class="tab-content"
    role="tabpanel"
    id="settings-tabpanel-account"
    aria-labelledby="settings-tab-account"
    hidden={activeTab !== 'account'}
  >
    <IdentityPanel />
    <DevicesPanel />
  </div>
  <div
    class="tab-content"
    role="tabpanel"
    id="settings-tabpanel-notifications"
    aria-labelledby="settings-tab-notifications"
    hidden={activeTab !== 'notifications'}
  >
    <NotificationSettingsPanel
      service={notificationService}
      {trustService}
      {peers}
      {communities}
      {onClose}
      {onTrustChange}
    />
  </div>
  <div
    class="tab-content"
    role="tabpanel"
    id="settings-tabpanel-network"
    aria-labelledby="settings-tab-network"
    hidden={activeTab !== 'network'}
  >
    <NetworkDiscoverabilitySettings />
    <IrohRelaySettings />
  </div>
  <div
    class="tab-content"
    role="tabpanel"
    id="settings-tabpanel-communities"
    aria-labelledby="settings-tab-communities"
    hidden={activeTab !== 'communities'}
  >
    {#if communityService}
      <LeftCommunitiesPanel service={communityService} />
    {/if}
  </div>
  <div
    class="tab-content"
    role="tabpanel"
    id="settings-tabpanel-friends"
    aria-labelledby="settings-tab-friends"
    hidden={activeTab !== 'friends'}
  >
    <FriendsPanel service={friendService} cardService={friendCardService} {dmInviteService} {onOpenCard} />
  </div>
</div>

<style>
  /* ZEB-569: header + close button mirror NotificationSettingsPanel so the two
     right-panel surfaces dismiss the same way. */
  .settings-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 12px 16px;
    border-bottom: 1px solid var(--border);
  }

  .settings-header h3 {
    margin: 0;
    font-size: 14px;
    font-weight: 600;
    color: var(--text-primary);
    font-family: var(--font-display);
  }

  .close-btn {
    border: none;
    background: none;
    color: var(--text-muted);
    font-size: 16px;
    cursor: pointer;
    padding: 4px;
  }

  .close-btn:hover {
    color: var(--text-primary);
  }

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
