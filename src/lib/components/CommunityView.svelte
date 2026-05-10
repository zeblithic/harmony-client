<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import type { CommunityService, ChannelInfo } from '../community-service';
  import type { ChannelMessageService } from '../channel-message-service';
  import type { CommunityMember } from '../types';
  import type { TrustService } from '../trust-service';
  import ChannelSubSidebar from './ChannelSubSidebar.svelte';
  import ChannelMessageFeed from './ChannelMessageFeed.svelte';
  import ChannelMembersPanel from './ChannelMembersPanel.svelte';
  import CreateChannelDialog from './CreateChannelDialog.svelte';
  import ModifyChannelDialog from './ModifyChannelDialog.svelte';
  import ConfirmDialog from './ConfirmDialog.svelte';
  import CommunitySettingsPanel from './CommunitySettingsPanel.svelte';

  let {
    communityId,
    communityName,
    communityKind,
    myPower,
    ownAddress,
    members,
    isDegraded,
    communityService,
    channelMessageService,
    trustService,
    onLeave,
    onKickMember,
    onSetPowerLevel,
    onGenerateInvite,
  }: {
    communityId: string;
    communityName: string;
    communityKind: 'open' | 'invite-only' | 'unknown';
    myPower: number;
    ownAddress: string;
    members: CommunityMember[];
    isDegraded: boolean;
    communityService: CommunityService;
    channelMessageService: ChannelMessageService;
    trustService?: TrustService;
    onLeave: () => Promise<void>;
    onKickMember: (addr: string) => Promise<void>;
    onSetPowerLevel: (addr: string, power: number) => Promise<void>;
    onGenerateInvite: () => Promise<string>;
  } = $props();

  let channels = $state<ChannelInfo[]>([]);
  let activeChannelId = $state<string | null>(null);
  let settingsModalOpen = $state(false);
  let showCreateDialog = $state(false);
  let modifyDialogChannel = $state<ChannelInfo | null>(null);
  let deleteConfirmChannel = $state<ChannelInfo | null>(null);
  let membersPanelCollapsed = $state(false);
  let prevOnChannelConfigChanged: typeof communityService.onChannelConfigChanged;

  let activeChannel = $derived(channels.find((c) => c.channelId === activeChannelId) ?? null);

  async function refreshChannels() {
    const list = await communityService.listChannels(communityId);
    channels = list.filter((c) => c.deletedAt === undefined);
  }

  /** Per spec §6.4: when active channel disappears, cascade to fallback.
   *  1. #general if it exists and is not the just-deleted channel.
   *  2. Next-oldest by created_at HLC.
   *  3. null (empty-state).
   *  Backend already sorts list_channels by created_at ascending so we
   *  just pick the first non-deleted entry. */
  function pickFallbackChannel(deletedChannelId: string): string | null {
    const general = channels.find((c) => c.name === 'general' && c.channelId !== deletedChannelId);
    if (general) return general.channelId;
    const next = channels.find((c) => c.channelId !== deletedChannelId);
    return next?.channelId ?? null;
  }

  function handleSelect(channelId: string) {
    activeChannelId = channelId;
    communityService.setSelectedChannel(communityId, channelId);
  }

  async function handleConfirmDelete() {
    if (!deleteConfirmChannel) return;
    const target = deleteConfirmChannel;
    deleteConfirmChannel = null;
    try {
      await communityService.deleteChannel(communityId, target.channelId);
      // The channel-config-updated event arrives shortly; the cascade
      // happens there. We don't optimistically remove from the local
      // `channels` list — keeps state-of-truth as the materialized
      // CRDT response.
    } catch (e) {
      // Could surface a toast here; for now log + leave channel in place
      // so user can retry.
      console.warn('deleteChannel failed', e);
    }
  }

  onMount(() => {
    // Hook channel-config callback ONCE per component lifetime. Chain prior
    // so we don't clobber App.svelte's listener if it had one.
    prevOnChannelConfigChanged = communityService.onChannelConfigChanged;
    communityService.onChannelConfigChanged = (cid, action, channelId, name, writePower) => {
      prevOnChannelConfigChanged?.(cid, action, channelId, name, writePower);
      if (cid !== communityId) return;
      void (async () => {
        await refreshChannels();
        if (action === 'deleted' && channelId === activeChannelId) {
          activeChannelId = pickFallbackChannel(channelId);
          if (activeChannelId) {
            communityService.setSelectedChannel(communityId, activeChannelId);
          }
        }
      })();
    };
  });

  // Per-community initialization runs whenever `communityId` changes (or on
  // first mount). Without this $effect, switching communities reuses the
  // component instance but leaves it pinned to the previous community's
  // channel list (Cursor Bugbot HIGH on PR #97 round 1).
  $effect(() => {
    const cid = communityId;
    let cancelled = false;
    // Reset activeChannelId so the persisted-channel logic re-runs for the
    // new community. The prior community's `setSelectedChannel` already
    // captured its last-viewed channel into the service map, so switching
    // back will restore from there.
    activeChannelId = null;

    void (async () => {
      // Capture persisted before refresh so the post-refresh validation
      // sees the most recent stored value.
      const persisted = communityService.getSelectedChannel(cid);
      if (cancelled) return;
      if (persisted) activeChannelId = persisted;
      await refreshChannels();
      if (cancelled) return;
      // Validate the persisted activeChannelId still resolves to a
      // non-deleted channel; if not (e.g., the channel was deleted while
      // user was elsewhere), default-select per §6.4.
      const stillExists = activeChannelId !== null
        && channels.some((c) => c.channelId === activeChannelId);
      if (!stillExists) {
        const general = channels.find((c) => c.name === 'general');
        activeChannelId = general?.channelId ?? channels[0]?.channelId ?? null;
        if (activeChannelId) {
          communityService.setSelectedChannel(cid, activeChannelId);
        }
      }
    })();

    return () => {
      cancelled = true;
    };
  });

  onDestroy(() => {
    communityService.onChannelConfigChanged = prevOnChannelConfigChanged;
  });
</script>

<section class="community-view" aria-label={`Community: ${communityName}`}>
  <header class="community-header">
    <h2 class="community-name">{communityName}</h2>
    <button
      type="button"
      class="settings-btn"
      aria-label="Open community settings"
      onclick={() => { settingsModalOpen = true; }}
    >⚙️</button>
  </header>

  <div class="three-cols">
    <ChannelSubSidebar
      {channels}
      {activeChannelId}
      {myPower}
      onSelect={handleSelect}
      onCreateClick={() => { showCreateDialog = true; }}
      onModifyClick={(c) => { modifyDialogChannel = c; }}
      onDeleteClick={(c) => { deleteConfirmChannel = c; }}
    />
    {#if activeChannel}
      <ChannelMessageFeed
        {communityId}
        channelId={activeChannel.channelId}
        channelName={activeChannel.name}
        {channelMessageService}
        {ownAddress}
        {trustService}
        {myPower}
      />
    {:else}
      <div class="empty-channels">
        <p>No channels in this community yet.</p>
        {#if myPower >= 50}
          <p>Click <strong>Create channel</strong> to add one.</p>
        {/if}
      </div>
    {/if}
    <ChannelMembersPanel
      {members}
      {ownAddress}
      {trustService}
      collapsed={membersPanelCollapsed}
    />
  </div>
</section>

<!-- Settings modal: simply mount CommunitySettingsPanel inside a Modal
  wrapper. The panel itself supplies its own close affordances; we only
  need the modal scrim + role=dialog + focus-trap (Modal provides those). -->
{#if settingsModalOpen}
  <CommunitySettingsPanel
    {communityId}
    {communityName}
    {communityKind}
    {members}
    myAddress={ownAddress}
    {myPower}
    {isDegraded}
    onClose={() => { settingsModalOpen = false; }}
    onKick={onKickMember}
    onSetPower={onSetPowerLevel}
    onLeave={onLeave}
    onGenerateInvite={onGenerateInvite}
  />
{/if}

<CreateChannelDialog
  {communityId}
  {communityService}
  open={showCreateDialog}
  {myPower}
  onClose={() => { showCreateDialog = false; }}
  onCreated={(channelId) => {
    showCreateDialog = false;
    handleSelect(channelId);
  }}
/>

{#if modifyDialogChannel}
  <ModifyChannelDialog
    {communityId}
    channel={modifyDialogChannel}
    {communityService}
    open={true}
    {myPower}
    onClose={() => { modifyDialogChannel = null; }}
  />
{/if}

{#if deleteConfirmChannel}
  <ConfirmDialog
    title={`Delete #${deleteConfirmChannel.name}?`}
    message={`Channel deletion is permanent. The message log persists but no new messages can be posted. Type "${deleteConfirmChannel.name}" to confirm.`}
    confirmLabel="Delete channel"
    destructive={true}
    onConfirm={handleConfirmDelete}
    onCancel={() => { deleteConfirmChannel = null; }}
  />
{/if}

<style>
  .community-view {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-width: 0;
  }
  .community-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 10px 16px;
    border-bottom: 1px solid var(--border);
    background: var(--bg-secondary);
  }
  .community-name { margin: 0; color: var(--text-primary); font-size: 1rem; }
  .settings-btn {
    background: none;
    border: none;
    cursor: pointer;
    font-size: 1.1rem;
    padding: 4px 8px;
    border-radius: 4px;
  }
  .settings-btn:hover { background: var(--bg-tertiary); }
  .three-cols {
    display: flex;
    flex: 1;
    min-height: 0;
  }
  .empty-channels {
    flex: 1;
    display: flex;
    flex-direction: column;
    justify-content: center;
    align-items: center;
    color: var(--text-secondary);
    padding: 32px;
    text-align: center;
  }
  .empty-channels p { margin: 6px 0; }
</style>
