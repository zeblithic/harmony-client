<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import type { CommunityService, ChannelInfo, PreForkSnapshotDto } from '../community-service';
  import type { ChannelMessageService } from '../channel-message-service';
  import type { CommunityMember } from '../types';
  import type { TrustService } from '../trust-service';
  import type { NavService } from '../nav-service';
  import ChannelSubSidebar from './ChannelSubSidebar.svelte';
  import ChannelMessageFeed from './ChannelMessageFeed.svelte';
  import ChannelMembersPanel from './ChannelMembersPanel.svelte';
  import CommunityMembersPanel from './CommunityMembersPanel.svelte';
  import CreateChannelDialog from './CreateChannelDialog.svelte';
  import ModifyChannelDialog from './ModifyChannelDialog.svelte';
  import TypedConfirmationModal from './TypedConfirmationModal.svelte';
  import CommunitySettingsPanel from './CommunitySettingsPanel.svelte';

  let {
    communityId,
    communityName,
    communityKind,
    myPower,
    ownAddress,
    members,
    isDegraded,
    sharedInProfile,
    communityService,
    channelMessageService,
    trustService,
    navService,
    onLeave,
    onKickMember,
    onSetPowerLevel,
    onGenerateInvite,
    onToggleSharedInProfile,
    onForkSuccess,
  }: {
    communityId: string;
    communityName: string;
    communityKind: 'open' | 'invite-only' | 'unknown';
    myPower: number;
    ownAddress: string;
    members: CommunityMember[];
    isDegraded: boolean;
    sharedInProfile: boolean;
    communityService: CommunityService;
    channelMessageService: ChannelMessageService;
    trustService?: TrustService;
    /** ZEB-285: NavService ref for fork-parent name resolution in the Lineage block and
     *  for adding the new fork to the sidebar after fork_community succeeds. Required —
     *  the fork nav-visibility path (navService.addOrUpdateNavSpace) silently no-ops if
     *  navService is absent, leaving the fork invisible in the sidebar. Making it required
     *  surfaces the dependency at the type level. (Fix: PR #122 round-3, Greptile P2.) */
    navService: NavService;
    onLeave: () => Promise<void>;
    onKickMember: (addr: string) => Promise<void>;
    onSetPowerLevel: (addr: string, power: number) => Promise<void>;
    onGenerateInvite: () => Promise<string>;
    onToggleSharedInProfile: (shared: boolean) => Promise<void>;
    /** ZEB-285: called after a successful fork with the new fork community ID. */
    onForkSuccess?: (forkSpaceId: string) => void;
  } = $props();

  let channels = $state<ChannelInfo[]>([]);
  let activeChannelId = $state<string | null>(null);
  let settingsModalOpen = $state(false);
  let communityMembersPanelOpen = $state(false);
  let showCreateDialog = $state(false);
  let modifyDialogChannel = $state<ChannelInfo | null>(null);
  let deleteConfirmChannel = $state<ChannelInfo | null>(null);
  let membersPanelCollapsed = $state(false);
  let prevOnChannelConfigChanged: typeof communityService.onChannelConfigChanged;
  // ZEB-285: fork lineage — loaded lazily when the settings modal first opens.
  let lineage = $state<{
    originalCommunityName: string | null;
    forkedAtMs: number;
    snapshotMessageCount: number;
  } | null | undefined>(undefined); // undefined = not yet fetched

  // ZEB-285 Task 11: pre-fork snapshot for unified timeline rendering.
  // Loaded once per community view. null = non-fork community; undefined = not yet loaded.
  let preForkSnapshot = $state<PreForkSnapshotDto | null | undefined>(undefined);

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
        try {
          await refreshChannels();
          if (action === 'deleted' && channelId === activeChannelId) {
            activeChannelId = pickFallbackChannel(channelId);
            if (activeChannelId) {
              communityService.setSelectedChannel(communityId, activeChannelId);
            }
          }
        } catch (e) {
          console.warn('CommunityView: refreshChannels failed in onChannelConfigChanged:', e);
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
    // Reset snapshot and lineage state on community switch so stale data from
    // the previous community never briefly shows for the new one.
    // (Fix: PR #122 round-4, CodeRabbit inline — lineage was not cleared.)
    preForkSnapshot = undefined;
    lineage = undefined; // undefined = not yet fetched for this community

    void (async () => {
      try {
        // ZEB-285 Task 11: load pre-fork snapshot for unified timeline.
        // Fire-and-forget; result flows into snapshotMessages prop on ChannelMessageFeed.
        communityService.getPreForkSnapshot(cid).then((snapshot) => {
          if (!cancelled) preForkSnapshot = snapshot;
        }).catch(() => {
          if (!cancelled) preForkSnapshot = null;
        });

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
      } catch (e) {
        console.warn('CommunityView: refreshChannels failed in community $effect:', e);
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
    <div class="header-actions">
      <button
        type="button"
        class="members-toggle-btn"
        aria-label={membersPanelCollapsed ? 'Show members panel' : 'Hide members panel'}
        aria-pressed={!membersPanelCollapsed}
        onclick={() => { membersPanelCollapsed = !membersPanelCollapsed; }}
      >👥</button>
      <button
        type="button"
        class="settings-btn"
        aria-label="Open community settings"
        onclick={() => {
          settingsModalOpen = true;
          // ZEB-285: lazily load lineage metadata on first open (or when community changes).
          if (lineage === undefined) {
            const requestedCommunityId = communityId;
            void communityService.getCommunityLineage(requestedCommunityId).then((dto) => {
              // Guard: only apply if we're still on the same community; a late-arriving
              // response from a previous community must not overwrite the new one's state.
              // (Fix: PR #122 round-4, CodeRabbit inline.)
              if (communityId !== requestedCommunityId) return;
              if (dto === null) {
                lineage = null; // not a fork
              } else {
                lineage = {
                  originalCommunityName: dto.originalCommunityName,
                  forkedAtMs: dto.forkedAtMs,
                  snapshotMessageCount: dto.snapshotMessageCount,
                };
              }
            }).catch(() => {
              if (communityId !== requestedCommunityId) return;
              lineage = null; // on error, hide lineage block
            });
          }
        }}
      >⚙️</button>
    </div>
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
        snapshotMessages={preForkSnapshot?.channelLog?.[activeChannel.channelId] ?? []}
        originalCommunityName={preForkSnapshot?.originalCommunityName ?? ''}
        forkedAtMs={preForkSnapshot?.forkedAtMs ?? 0}
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
    {sharedInProfile}
    {onToggleSharedInProfile}
    onClose={() => { settingsModalOpen = false; }}
    onKick={onKickMember}
    onSetPower={onSetPowerLevel}
    onLeave={onLeave}
    onGenerateInvite={onGenerateInvite}
    onOpenMembersPanel={() => { communityMembersPanelOpen = true; }}
    lineage={lineage ?? null}
    onFork={async (opts) => {
      const result = await communityService.forkCommunity(communityId, opts);
      // Add the fork to the nav tree with forkedFrom lineage.
      navService.addOrUpdateNavSpace({
        action: 'added',
        spaceId: result.forkSpaceId,
        kind: 'community',
        name: opts.name,
        members: [],
        parentId: null,
        forkedFrom: communityId,
      });
      settingsModalOpen = false;
      onForkSuccess?.(result.forkSpaceId);
    }}
  />
{/if}

{#if communityMembersPanelOpen}
  <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
  <div
    class="community-members-overlay"
    role="dialog"
    aria-modal="true"
    aria-label="Community members"
    onclick={(e) => { if (e.target === e.currentTarget) communityMembersPanelOpen = false; }}
  >
    <div class="community-members-overlay-inner">
      <div class="community-members-overlay-header">
        <span class="community-members-overlay-title">Members — {communityName}</span>
        <button
          class="community-members-overlay-close"
          onclick={() => { communityMembersPanelOpen = false; }}
          aria-label="Close members panel"
        >✕</button>
      </div>
      <CommunityMembersPanel
        {communityId}
        {communityName}
        {communityService}
        ownAddress={ownAddress}
      />
    </div>
  </div>
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
  <TypedConfirmationModal
    title={`Delete #${deleteConfirmChannel.name}?`}
    description="Channel deletion is permanent. The message log persists on existing devices, but no new messages can be posted and the channel will disappear from the sidebar for everyone."
    requiredText={deleteConfirmChannel.name}
    confirmLabel="Delete channel"
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
  .header-actions {
    display: flex;
    align-items: center;
    gap: 4px;
  }
  .settings-btn {
    background: none;
    border: none;
    cursor: pointer;
    font-size: 1.1rem;
    padding: 4px 8px;
    border-radius: 4px;
  }
  .settings-btn:hover { background: var(--bg-tertiary); }
  .members-toggle-btn {
    background: none;
    border: none;
    cursor: pointer;
    font-size: 1.1rem;
    padding: 4px 8px;
    border-radius: 4px;
  }
  .members-toggle-btn:hover { background: var(--bg-tertiary); }
  .members-toggle-btn[aria-pressed="false"] { opacity: 0.5; }
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
  .community-members-overlay {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: flex-start;
    justify-content: center;
    z-index: 950;
    overflow-y: auto;
    padding: 32px 16px;
  }
  .community-members-overlay-inner {
    background: var(--bg-secondary);
    border-radius: 8px;
    border: 1px solid var(--border);
    max-width: 640px;
    width: 100%;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.6);
    overflow: hidden;
  }
  .community-members-overlay-header {
    padding: 14px 20px;
    border-bottom: 1px solid var(--border);
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  .community-members-overlay-title {
    font-size: 0.9rem;
    color: var(--text-primary);
    font-weight: 600;
  }
  .community-members-overlay-close {
    background: transparent;
    color: var(--text-secondary);
    border: none;
    font-size: 1.1rem;
    padding: 4px 10px;
    cursor: pointer;
  }
  .community-members-overlay-close:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
</style>
