<script lang="ts">
  import './app.css';
  import Layout from './lib/components/Layout.svelte';
  import NavPanel from './lib/components/NavPanel.svelte';
  import TextFeed from './lib/components/TextFeed.svelte';
  import MediaFeed from './lib/components/MediaFeed.svelte';
  import VineFeed from './lib/components/VineFeed.svelte';
  import FileBrowser from './lib/components/FileBrowser.svelte';
  import FileDetailPanel from './lib/components/FileDetailPanel.svelte';
  import NotificationSettingsPanel from './lib/components/NotificationSettingsPanel.svelte';
  import ProfileEditor from './lib/components/ProfileEditor.svelte';
  import SpellbookMode from './lib/components/SpellbookMode.svelte';
  import FlashcardStats from './lib/components/FlashcardStats.svelte';
  import ProfilePopover from './lib/components/ProfilePopover.svelte';
  import { NotificationService } from './lib/notification-service';
  import { loadProfile, saveProfile } from './lib/profile-service';
  import { Stq8Service } from './lib/stq8-service';
  import { initialSessionStats } from './lib/flashcard-types';
  import { TrustService } from './lib/trust-service';
  import { FileManagerService } from './lib/file-manager-service';
  // TODO: Replace mock-data imports with real data sources once content transport is wired up
  import { messages, navNodes, profileStore, vineVideos } from './lib/mock-data';
  import type { AppMode, MessagePriority, Profile, ThreadDisplayMode, FileViewMode, ContentSection, ReplicationTier } from './lib/types';
  import { getThreadMeta } from './lib/feed-utils';

  let innerWidth = $state(window.innerWidth);
  let collapsed = $derived(innerWidth <= 768);
  let showSettings = $state(false);
  let appMode = $state<AppMode>('messages');

  let myProfile = $state(loadProfile());

  async function handleProfileSave(profile: Profile) {
    saveProfile(profile);
    myProfile = profile;
    // Publish to network if Tauri is available
    try {
      const { invoke } = await import('@tauri-apps/api/core');
      await invoke('publish_profile', {
        profile: {
          address: profile.address,
          displayName: profile.displayName,
          statusText: profile.statusText,
          avatarUrl: profile.avatarUrl,
        },
      });
    } catch {
      // Not in Tauri or not connected — profile saved locally only
    }
  }

  // Viewed vine IDs — lifted here so state survives VineFeed remounts on mode toggle
  let vineViewedIds = $state(new Set<string>(
    vineVideos.filter(v => v.viewed).map(v => v.id)
  ));

  function handleMarkVineViewed(id: string) {
    vineViewedIds = new Set([...vineViewedIds, id]);
    // invoke('mark_vine_viewed', { vineId: id }); // wire up when transport is ready
  }

  let popoverProfile = $state<Profile | null>(null);
  let popoverX = $state(0);
  let popoverY = $state(0);

  function handleAvatarClick(address: string, event: MouseEvent) {
    if (popoverProfile?.address === address) {
      popoverProfile = null;
      return;
    }
    const profile = profileStore.get(address);
    if (!profile) return;
    const el = (event.target as HTMLElement).closest('.avatar') as HTMLElement | null;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    const POPOVER_WIDTH = 300;
    const POPOVER_HEIGHT = 220;
    popoverX = Math.min(rect.right + 8, window.innerWidth - POPOVER_WIDTH - 8);
    popoverY = Math.min(rect.top, window.innerHeight - POPOVER_HEIGHT - 8);
    popoverProfile = profile;
  }

  function closePopover() {
    popoverProfile = null;
  }

  const notificationService = new NotificationService();
  const trustService = new TrustService();
  const fileManagerService = new FileManagerService();
  const stq8Service = new Stq8Service(null); // WASM loaded async later
  let flashcardStats = $state(initialSessionStats());
  let trustVersion = $state(0);

  function handleTrustChange() {
    trustVersion++;
  }

  // ── File manager state ──────────────────────────────────────────────
  let fileManagerVersion = $state(0);
  let selectedFileCid = $state<string | null>(null);
  let currentFolderCid = $state<string | null>(null);
  let fileViewMode = $state<FileViewMode>('list');
  let showCleanup = $state(false);
  let fileSection = $state<ContentSection>('private');
  let fileSearchQuery = $state('');
  let fileFilters = $state<Record<string, unknown>>({});

  // ── File manager derived data ───────────────────────────────────────
  let allFileContents = $derived.by(() => {
    void fileManagerVersion;
    return fileManagerService.getContents();
  });

  let selectedFileDetail = $derived.by(() => {
    void fileManagerVersion;
    if (!selectedFileCid) return undefined;
    return fileManagerService.getContentDetail(selectedFileCid);
  });

  let fileBuddies = $derived.by(() => {
    void fileManagerVersion;
    return fileManagerService.getStorageBuddies();
  });

  let availablePeers = $derived.by(() => {
    void fileManagerVersion;
    return fileManagerService.getAvailablePeers();
  });

  // ── File manager callbacks ──────────────────────────────────────────
  function handleFileItemClick(cid: string) {
    selectedFileCid = cid;
  }

  function handleNavigateFolder(cid: string | null) {
    currentFolderCid = cid;
    selectedFileCid = null;
    showCleanup = false;
  }

  function handleFileBurn() {
    if (!selectedFileCid) return;
    fileManagerService.burn([selectedFileCid]);
    fileManagerVersion++;
    selectedFileCid = null;
  }

  function handleFileArchive() {
    if (!selectedFileCid) return;
    fileManagerService.archive([selectedFileCid]);
    // archive is a no-op stub — don't bump version or clear selection
    // until the service actually moves items to cold storage
  }

  function handleFilePublish(cid: string) {
    fileManagerService.publish([cid]);
    fileManagerVersion++;
    selectedFileCid = null;
  }

  function handleFileRelease(cid: string) {
    fileManagerService.release([cid]);
    fileManagerVersion++;
    selectedFileCid = null;
  }

  function handleFilePin() {
    if (!selectedFileCid) return;
    fileManagerService.pin(selectedFileCid);
    fileManagerVersion++;
  }

  function handleFileUnpin() {
    if (!selectedFileCid) return;
    fileManagerService.unpin(selectedFileCid);
    fileManagerVersion++;
  }

  function handleFileExport() {
    if (!selectedFileCid) return;
    fileManagerService.exportToDevice([selectedFileCid]);
  }

  function handleFileTierChange(tier: ReplicationTier) {
    if (!selectedFileCid) return;
    fileManagerService.setReplicationTier([selectedFileCid], tier);
    fileManagerVersion++;
  }

  function handleFileUploadClick() {
    // Future: open file picker via Tauri dialog
  }

  function handleFileCleanupClick() {
    showCleanup = !showCleanup;
  }

  function handleCleanupAction(cid: string, action: string) {
    if (action === 'burn') fileManagerService.burn([cid]);
    else if (action === 'archive') { fileManagerService.archive([cid]); return; }
    else if (action === 'release') fileManagerService.release([cid]);
    else if (action === 'publish') fileManagerService.publish([cid]);
    else if (action === 'pin') fileManagerService.pin(cid);
    fileManagerVersion++;
    if (selectedFileCid === cid && (action === 'burn' || action === 'release' || action === 'publish')) {
      selectedFileCid = null;
    }
  }

  function handleBulkBurn(cids: string[]) {
    fileManagerService.burn(cids);
    fileManagerVersion++;
    if (selectedFileCid && cids.includes(selectedFileCid)) {
      selectedFileCid = null;
    }
  }

  function handleBulkArchive(cids: string[]) {
    fileManagerService.archive(cids);
    // archive is a no-op stub — don't bump version or clear selection
    // until the service actually moves items to cold storage
  }

  function handleBulkRelease(cids: string[]) {
    fileManagerService.release(cids);
    fileManagerVersion++;
    if (selectedFileCid && cids.includes(selectedFileCid)) {
      selectedFileCid = null;
    }
  }

  function handleBulkPublish(cids: string[]) {
    fileManagerService.publish(cids);
    fileManagerVersion++;
    if (selectedFileCid && cids.includes(selectedFileCid)) {
      selectedFileCid = null;
    }
  }

  // Mock per-peer override to demonstrate settings
  notificationService.setPeerPolicy('q7r8s9t0', { quiet: 'silent' });

  let allMessages = $state([...messages]);

  // Thread state
  let openThreadId = $state<string | null>(null);
  let threadModes = $state<Map<string, ThreadDisplayMode>>(new Map());
  let pinnedThreadIds = $state<Set<string>>(new Set());

  // Thread derivations
  let threadMeta = $derived(getThreadMeta(allMessages));

  let threadRoot = $derived(
    openThreadId
      ? allMessages.find(m => m.id === openThreadId) ?? null
      : null
  );

  let threadReplies = $derived(
    openThreadId
      ? allMessages.filter(m => m.replyTo === openThreadId)
      : []
  );

  let threadMessageIds = $derived(
    openThreadId
      ? new Set(threadReplies.map(m => m.id))
      : new Set<string>()
  );

  // Main feed: exclude replies for panel/muted threads, keep inline
  let mainFeedMessages = $derived(
    allMessages.filter(m => {
      if (!m.replyTo) return true;
      const mode = threadModes.get(m.replyTo) ?? 'panel';
      return mode === 'inline';
    })
  );

  // Media feed: main + open thread replies (exclude muted)
  let mediaMessages = $derived.by(() => {
    const base = allMessages.filter(m => {
      if (!m.replyTo) return true;
      const mode = threadModes.get(m.replyTo) ?? 'panel';
      if (mode === 'muted') return false;
      if (mode === 'inline') return true;
      // panel mode: only include if this thread is open
      return m.replyTo === openThreadId;
    });
    return base;
  });

  function scrollToMedia(mediaId: string) {
    document.getElementById(`media-${mediaId}`)?.scrollIntoView({
      behavior: 'smooth',
      block: 'center',
    });
  }

  function scrollToMessage(messageId: string) {
    let el = document.getElementById(`msg-${messageId}`);
    if (!el) {
      document.dispatchEvent(new CustomEvent('reveal-message', { detail: messageId }));
      requestAnimationFrame(() => {
        el = document.getElementById(`msg-${messageId}`);
        if (el) {
          el.scrollIntoView({ behavior: 'smooth', block: 'center' });
          el.classList.add('highlight');
          setTimeout(() => el.classList.remove('highlight'), 1500);
        }
      });
      return;
    }
    el.scrollIntoView({ behavior: 'smooth', block: 'center' });
    el.classList.add('highlight');
    setTimeout(() => el.classList.remove('highlight'), 1500);
  }

  function handleSend(text: string, priority: MessagePriority) {
    const newMsg = {
      id: `msg-${Date.now()}`,
      sender: { address: 'self', displayName: 'You' },
      text,
      timestamp: Date.now(),
      media: [],
      priority,
    };
    allMessages = [...allMessages, newMsg];
  }

  function handleThreadOpen(rootId: string) {
    openThreadId = rootId;
  }

  function handleThreadClose() {
    openThreadId = null;
  }

  function handleThreadSend(text: string, priority: MessagePriority) {
    if (!openThreadId) return;
    const newMsg = {
      id: `msg-${Date.now()}`,
      sender: { address: 'self', displayName: 'You' },
      text,
      timestamp: Date.now(),
      media: [],
      priority,
      replyTo: openThreadId,
    };
    allMessages = [...allMessages, newMsg];
  }

  // Extract community nodes (folders) for settings panel
  let communities = $derived(navNodes.filter((n) => n.type === 'folder'));

  // Collect known peers from messages and nav nodes (DMs)
  let knownPeers = $derived.by(() => {
    const peerMap = new Map(
      allMessages
        .filter((m) => m.sender.address !== 'self')
        .map((m) => [m.sender.address, m.sender])
    );
    for (const node of navNodes) {
      if (node.peer && !peerMap.has(node.peer.address)) {
        peerMap.set(node.peer.address, node.peer);
      }
    }
    return [...peerMap.values()];
  });
</script>

<svelte:window bind:innerWidth />

<Layout {collapsed} {showSettings} mode={appMode}>
  {#snippet nav()}
    <NavPanel
      nodes={navNodes}
      {collapsed}
      onSettingsClick={() => { showSettings = !showSettings; }}
      profileLookup={(addr) => profileStore.get(addr)?.statusText}
      onModeChange={(mode: AppMode) => { appMode = mode; showSettings = false; showCleanup = false; fileFilters = {}; fileSearchQuery = ''; selectedFileCid = null; currentFolderCid = null; }}
      {appMode}
      contentItems={allFileContents}
      storageBuddies={fileBuddies}
      {fileSection}
      {currentFolderCid}
      onFolderSelect={handleNavigateFolder}
      filters={fileFilters}
      onFilterChange={(filters) => { fileFilters = filters; }}
    />
  {/snippet}
  {#snippet textFeed()}
    <TextFeed
      messages={mainFeedMessages}
      {collapsed}
      onMediaClick={scrollToMedia}
      onSend={handleSend}
      onAvatarClick={handleAvatarClick}
      {trustService}
      {trustVersion}
      {threadRoot}
      {threadReplies}
      {threadMeta}
      {openThreadId}
      onThreadOpen={handleThreadOpen}
      onThreadClose={handleThreadClose}
      onThreadSend={handleThreadSend}
      onScrollToMessage={scrollToMessage}
      {pinnedThreadIds}
    />
  {/snippet}
  {#snippet mediaFeed()}
    <MediaFeed
      messages={mediaMessages}
      {trustService}
      {trustVersion}
      onLinkBack={scrollToMessage}
      onAvatarClick={handleAvatarClick}
      onTrustChange={handleTrustChange}
      {threadMessageIds}
    />
  {/snippet}
  {#snippet settingsPanel()}
    <ProfileEditor profile={myProfile} onSave={handleProfileSave} />
    <NotificationSettingsPanel
      service={notificationService}
      {trustService}
      peers={knownPeers}
      {communities}
      onClose={() => { showSettings = false; }}
      onTrustChange={handleTrustChange}
    />
  {/snippet}
  {#snippet vineFeed()}
    <VineFeed vines={vineVideos} viewedIds={vineViewedIds} onMarkViewed={handleMarkVineViewed} />
  {/snippet}
  {#snippet fileBrowser()}
    <FileBrowser
      service={fileManagerService}
      {currentFolderCid}
      selectedCid={selectedFileCid}
      viewMode={fileViewMode}
      section={fileSection}
      searchQuery={fileSearchQuery}
      filters={fileFilters}
      {showCleanup}
      onItemClick={handleFileItemClick}
      onNavigateFolder={handleNavigateFolder}
      onViewModeChange={(mode) => { fileViewMode = mode; }}
      onSearchChange={(query) => { fileSearchQuery = query; }}
      onSectionChange={(newSection) => { fileSection = newSection; selectedFileCid = null; showCleanup = false; fileFilters = {}; fileSearchQuery = ''; }}
      onUploadClick={handleFileUploadClick}
      onCleanupClick={handleFileCleanupClick}
      onCleanupAction={handleCleanupAction}
      onBulkBurn={handleBulkBurn}
      onBulkArchive={handleBulkArchive}
      onBulkRelease={handleBulkRelease}
      onBulkPublish={handleBulkPublish}
      serviceVersion={fileManagerVersion}
    />
  {/snippet}
  {#snippet fileDetailPanel()}
    {#if selectedFileDetail}
      <FileDetailPanel
        detail={selectedFileDetail}
        availablePeers={availablePeers}
        storageBuddyDetails={fileBuddies.filter(b => selectedFileDetail?.storageBuddies.some(sb => sb.address === b.address))}
        confirmationOverrides={fileManagerService.settings.confirmationOverrides}
        onTierChange={handleFileTierChange}
        onPublish={handleFilePublish}
        onRelease={handleFileRelease}
        onBurn={handleFileBurn}
        onArchive={handleFileArchive}
        onPin={handleFilePin}
        onUnpin={handleFileUnpin}
        onExport={handleFileExport}
      />
    {:else}
      <div class="file-detail-empty">
        <p>Select a file to view details</p>
      </div>
    {/if}
  {/snippet}
  {#snippet spellbookContent()}
    <SpellbookMode
      {stq8Service}
      onStatsUpdate={(stats) => { flashcardStats = stats; }}
    />
  {/snippet}
  {#snippet spellbookDetail()}
    <FlashcardStats stats={flashcardStats} />
  {/snippet}
</Layout>

{#if popoverProfile}
  <ProfilePopover
    profile={popoverProfile}
    x={popoverX}
    y={popoverY}
    onClose={closePopover}
  />
{/if}

<style>
  :global(.text-message) {
    transition: background 0.3s ease;
  }

  :global(.text-message.highlight) {
    background: rgba(88, 101, 242, 0.15) !important;
  }

  .file-detail-empty {
    display: flex;
    align-items: center;
    justify-content: center;
    height: 100%;
    color: var(--text-muted, #949ba4);
  }
</style>
