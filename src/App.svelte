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
  import MailInbox from './lib/components/MailInbox.svelte';
  import MailReader from './lib/components/MailReader.svelte';
  import MailCompose from './lib/components/MailCompose.svelte';
  import ProfilePopover from './lib/components/ProfilePopover.svelte';
  import VinePublishDialog from './lib/components/VinePublishDialog.svelte';
  import { NotificationService } from './lib/notification-service';
  import { loadProfile, saveProfile } from './lib/profile-service';
  import { Stq8Service } from './lib/stq8-service';
  import * as stq8ProfileStorage from './lib/stq8-profile-storage';
  import { initialSessionStats } from './lib/flashcard-types';
  import { TrustService } from './lib/trust-service';
  import { FileManagerService } from './lib/file-manager-service';
  import { MessageService } from './lib/message-service';
  import { MailService } from './lib/mail-service';
  import { VineService } from './lib/vine-service';
  import { NavService } from './lib/nav-service';
  import { AvatarResolver } from './lib/avatar-resolver';
  import type { AppMode, MessagePriority, Profile, ThreadDisplayMode, FileViewMode, ContentSection, ReplicationTier, MailFolderKind, MailMessageDetail } from './lib/types';
  import { getThreadMeta } from './lib/feed-utils';
  import { findNode, findNearestFolder } from './lib/nav-utils';
  import { isTauri } from './lib/tauri-env';

  let innerWidth = $state(window.innerWidth);
  let collapsed = $derived(innerWidth <= 768);
  let showSettings = $state(false);
  let appMode = $state<AppMode>('messages');

  let myProfile = $state(loadProfile());

  async function handleProfileSave(profile: Profile) {
    saveProfile(profile);
    myProfile = profile;
    // messageService.ownDisplayName / vineService.ownDisplayName are kept
    // in sync by a `$effect` later in the script (single source of truth).
    // Publish to network if Tauri is available.
    // Uses direct invoke rather than ZenohService.publishProfile() because
    // ZenohService lives in NetworkApp (not accessible here). Both paths
    // invoke the same 'publish_profile' command.
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

  const vineService = new VineService();
  $effect(() => () => vineService.destroy());

  let followedVines = $state([...vineService.followedVines]);
  let discoverVines = $state([...vineService.discoverVines]);
  let vineViewedIds = $state(new Set(vineService.viewedIds));
  let vineTab = $state<'following' | 'discover'>('following');
  let followedAddresses = $state(new Set(vineService.followedAddresses));
  let vineGetReaction = $state<(vineId: string) => { count: number; likedByMe: boolean }>(
    (vineId: string) => vineService.getReaction(vineId)
  );

  vineService.onChange = () => {
    followedVines = [...vineService.followedVines];
    discoverVines = [...vineService.discoverVines];
    vineViewedIds = new Set(vineService.viewedIds);
    followedAddresses = new Set(vineService.followedAddresses);
    vineGetReaction = (vineId: string) => vineService.getReaction(vineId);
  };

  function handleMarkVineViewed(id: string) {
    vineService.markViewed(id);
  }

  let showVinePublish = $state(false);

  async function handleVinePublish(videoCid: string, title?: string) {
    try {
      await vineService.publish(videoCid, title);
    } catch (err) {
      console.error('Vine publish failed', err);
      throw err;
    }
  }

  async function handleVineReshare(vine: import('./lib/types').VineVideo) {
    try {
      await vineService.publish(vine.videoCid, vine.title, vine.id);
    } catch (err) {
      console.error('Vine reshare failed', err);
      throw err;
    }
  }

  async function handleVineFollow(address: string, name: string) {
    try {
      await vineService.follow(address, name);
    } catch (err) {
      console.error('Follow failed', err);
    }
  }

  async function handleVineUnfollow(address: string) {
    try {
      await vineService.unfollow(address);
    } catch (err) {
      console.error('Unfollow failed', err);
    }
  }

  function handleVineToggleLike(vine: import('./lib/types').VineVideo) {
    vineService.toggleLike(vine).catch((err) => {
      console.error('Toggle like failed', err);
    });
  }

  /** Detect video MIME type from magic bytes. */
  function detectVideoMime(bytes: number[]): string {
    // MP4/M4V: byte 4-7 = 'ftyp'
    if (bytes[4] === 0x66 && bytes[5] === 0x74 && bytes[6] === 0x79 && bytes[7] === 0x70) return 'video/mp4';
    // WebM/Matroska: 0x1A 0x45 0xDF 0xA3
    if (bytes[0] === 0x1A && bytes[1] === 0x45 && bytes[2] === 0xDF && bytes[3] === 0xA3) return 'video/webm';
    // AVI: 'RIFF' header + 'AVI ' subtype at bytes 8-11
    if (bytes[0] === 0x52 && bytes[1] === 0x49 && bytes[2] === 0x46 && bytes[3] === 0x46
        && bytes[8] === 0x41 && bytes[9] === 0x56 && bytes[10] === 0x49 && bytes[11] === 0x20) return 'video/avi';
    return 'video/mp4';
  }

  let resolveVideoFn = $state<((cid: string) => Promise<string>) | undefined>(undefined);

  const avatarResolver = new AvatarResolver();
  $effect(() => () => avatarResolver.destroy());

  const navService = new NavService();
  navService.setAvatarResolver(avatarResolver);
  $effect(() => () => navService.destroy());

  let navNodes = $state([...navService.nodes]);

  // When avatar CIDs finish resolving, push blob URLs into stored profiles/nodes.
  avatarResolver.onChange = () => navService.refreshAvatars();
  navService.onChange = () => {
    navNodes = [...navService.nodes];
  };

  let popoverProfile = $state<Profile | null>(null);
  let popoverX = $state(0);
  let popoverY = $state(0);

  function handleAvatarClick(address: string, event: MouseEvent) {
    if (popoverProfile?.address === address) {
      popoverProfile = null;
      return;
    }
    const profile = navService.getProfile(address);
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
  $effect(() => () => fileManagerService.destroy());
  // Declare fileManagerVersion before wiring onChange — same pattern as allMessages.
  let fileManagerVersion = $state(0);
  fileManagerService.onChange = () => { fileManagerVersion++; };
  const messageService = new MessageService();
  $effect(() => () => messageService.destroy());

  const mailService = new MailService();
  $effect(() => () => mailService.destroy());
  let mailEntries = $state([...mailService.entries]);
  let mailCounts = $state({ ...mailService.counts });
  let mailSyncState = $state<'idle' | 'syncing' | 'error'>(mailService.syncState);
  let mailSyncError = $state<string | null>(mailService.syncError);
  let activeMailFolder = $state<MailFolderKind>('inbox');
  let selectedMailCid = $state<string | null>(null);
  let selectedMailDetail = $state<MailMessageDetail | null>(null);
  let mailDetailLoading = $state(false);
  let mailDetailError = $state<string | null>(null);
  let showCompose = $state(false);
  let composeReplyTo = $state<string | null>(null);
  let composeInitialTo = $state('');
  let composeInitialSubject = $state('');
  mailService.onChange = () => {
    mailEntries = [...mailService.entries];
    mailCounts = { ...mailService.counts };
    mailSyncState = mailService.syncState;
    mailSyncError = mailService.syncError;
  };

  // Start with the null-backed service so FlashcardView shows its "Loading
  // Q8 engine..." placeholder. initStq8Service() swaps in a real instance
  // once the wasm-bindgen module resolves; reassigning a $state-tracked
  // reference is what propagates through SpellbookMode's props down to
  // FlashcardView's `{#if !stq8Service.isReady()}` guard.
  //
  // `harmony-stq8` is a Vite alias to the sibling `../harmony-stq8/stq8-web/pkg/`
  // directory, produced by `scripts/build-wasm.sh` in that repo. Missing
  // alias target (fresh clone without the sibling, or before first build)
  // logs a friendly message and leaves Spellbook in placeholder mode —
  // everything else still works. See vite.config.ts for the alias setup.
  let stq8Service = $state(new Stq8Service(null));
  (async () => {
    try {
      const wasm = await import('harmony-stq8');
      await wasm.default();
      const pipeline = new wasm.WasmPipeline();
      // wasm-bindgen emits static class methods for Rust fns without
      // `&self` (generate_challenge, validate_row, level_info, format_*)
      // and prototype methods for fns with `&self` / `&mut self`
      // (process + calibration/profile). Stq8Service's WasmPipelineApi
      // flattens both onto one adapter so the service stays unaware of
      // the split.
      stq8Service = new Stq8Service({
        generate_challenge: wasm.WasmPipeline.generate_challenge,
        validate_row: wasm.WasmPipeline.validate_row,
        format_box_q8: wasm.WasmPipeline.format_box_q8,
        format_flat_q8: wasm.WasmPipeline.format_flat_q8,
        level_info: wasm.WasmPipeline.level_info,
        process: (pcm) => pipeline.process(pcm),
        add_calibration_sample: (idx, pcm) => pipeline.add_calibration_sample(idx, pcm),
        finalize_calibration: () => pipeline.finalize_calibration(),
        is_calibrated: () => pipeline.is_calibrated(),
        export_profile: () => pipeline.export_profile(),
        import_profile: (json) => pipeline.import_profile(json),
        set_created_epoch_secs: (secs) => pipeline.set_created_epoch_secs(secs),
      });
      // Restore a previously-saved voice profile so the user doesn't
      // have to re-run the 16-syllable calibration on every reload.
      // import_profile throws on corrupted / schema-mismatch JSON; in
      // that case we clear and drop back to the uncalibrated state
      // rather than leave poisoned storage around forever.
      const savedProfile = stq8ProfileStorage.loadProfile();
      if (savedProfile !== null) {
        try {
          stq8Service.importProfile(savedProfile);
        } catch (err) {
          console.warn('[harmony-client] stq8 saved profile rejected, clearing:', err);
          stq8ProfileStorage.clearProfile();
        }
      }
    } catch (err) {
      console.info('[harmony-client] stq8 WASM not loaded — Spellbook stays in placeholder mode. Build it with `scripts/build-wasm.sh` in the sibling harmony-stq8 clone.', err);
    }
  })();

  // Declare allMessages before wiring onChange — avoids a temporal dead zone
  // if onChange were ever triggered synchronously during init.
  let allMessages = $state([...messageService.messages]);

  // Wire onChange so both online (Zenoh echo) and offline (local append)
  // paths update the reactive allMessages state.
  messageService.onChange = () => { allMessages = [...messageService.messages]; };

  // Keep the display name on both services in sync with profile edits.
  $effect(() => {
    const name = myProfile.displayName || 'You';
    vineService.ownDisplayName = name;
    messageService.ownDisplayName = name;
  });

  // Wire up real Tauri transport (messages, vines, file manager, mail, nav).
  //
  // Environment check first: if we're not inside Tauri, mock data stays.
  // Past that check, every failure is a real bug (backend command rejected,
  // runtime not ready, malformed response) — those get logged loudly so
  // they don't silently hide behind mock data like they used to.
  (async () => {
    if (!isTauri()) {
      console.info('[harmony-client] Tauri not detected — services using mock data');
      return;
    }
    // Everything past the env check is inside Tauri, so any failure here
    // (failed dynamic import, rejected listen() registration, anything
    // not already guarded by tryConnect) is a real bug. Surface it with
    // the standard tag instead of letting it become an unhandled
    // rejection that stops init silently.
    try {
      const { invoke } = await import('@tauri-apps/api/core');
      const { listen } = await import('@tauri-apps/api/event');
      const adapter = {
        invoke: (cmd: string, args?: Record<string, unknown>) => invoke(cmd, args),
        listen: (event: string, handler: (e: { payload: unknown }) => void) => listen(event, handler),
      };

      // Per-service connect wrapper: logs failures with the adapter name but
      // doesn't cascade — a broken MessageService shouldn't kill VineService.
      // zenoh-status re-hydration (below) covers later-connect recovery.
      async function tryConnect(name: string, p: Promise<unknown>): Promise<void> {
        try {
          await p;
        } catch (err) {
          console.warn(`[harmony-client] ${name} adapter connect failed:`, err);
        }
      }

      // Boot the harmony node in standalone mode (no upstream endpoint) so
      // identity loads and mail_mgr is ready before adapters wire up. The
      // Network view's Connect flow can later re-invoke start_node with an
      // endpoint to join a gateway.
      try {
        await invoke('start_node', { endpoint: null });
      } catch (err) {
        console.warn('[harmony-client] auto-start_node failed:', err);
      }

      await tryConnect('message', messageService.connectAdapter(adapter));
      // Mail connect may fail if mail_mgr isn't ready yet (race with
      // start_node); non-blocking so other services proceed. The
      // zenoh-status handler below re-hydrates mail state on reconnect.
      tryConnect('mail', mailService.connectAdapter(adapter));
      await tryConnect('vine', vineService.connectAdapter(adapter));
      await tryConnect('vine.loadFollowed', vineService.loadFollowed());
      await tryConnect('fileManager', fileManagerService.connectAdapter(adapter));
      avatarResolver.connectAdapter(adapter);
      resolveVideoFn = async (cid: string) => {
        const bytes = (await adapter.invoke('fetch_content', { cid })) as number[];
        const mime = detectVideoMime(bytes);
        const blob = new Blob([new Uint8Array(bytes)], { type: mime });
        return URL.createObjectURL(blob);
      };
      await tryConnect('nav', navService.connectAdapter(adapter));

      // Fetch our node address so self-sent messages/vines echo back as
      // 'self'/'You'. Try immediately (node may already be connected after
      // hot reload / auto-start), and also retry on later zenoh-status
      // events.
      async function fetchOwnAddress() {
        try {
          const addr = await invoke('get_node_addr') as string;
          messageService.ownAddress = addr;
          vineService.ownAddress = addr;
          navService.ownAddress = addr;
        } catch (err) {
          // Expected while start_node is still racing with boot; the
          // zenoh-status listener below retries on 'connected'. Logged at
          // debug so it's discoverable but doesn't pollute the normal log.
          console.debug('[harmony-client] get_node_addr not yet available:', err);
        }
      }
      await fetchOwnAddress();

      // Re-hydrate backend-dependent state when Zenoh reports connected.
      // On initial boot, mail_mgr / follow list may not be ready yet (e.g.
      // if auto-start_node failed or raced), so the first round of
      // refreshCounts / loadFolder / loadFollowed returns empty. When a
      // later Connect (from the Network view) succeeds and fires
      // `zenoh-status: connected`, we re-read so the UI catches up.
      // MailService.connectAdapter has already registered event listeners;
      // we only re-run the idempotent data-fetch calls here — nothing
      // double-registers.
      //
      // Each service internally swallows *expected* errors (missing
      // adapter, "not connected", "mail not initialized"), so rejections
      // that bubble up here are unexpected — log them at warn level with
      // the service tag, matching the ZEB-148 convention. Rejections
      // don't block the other refreshes (allSettled) since the UI should
      // catch up whatever it can.
      async function reloadBackendState() {
        const tasks = [
          ['mail.refreshCounts', mailService.refreshCounts()],
          ['mail.loadFolder', mailService.loadFolder(mailService.activeFolder)],
          ['vine.loadFollowed', vineService.loadFollowed()],
        ] as const;
        const results = await Promise.allSettled(tasks.map(([, p]) => p));
        for (const [i, result] of results.entries()) {
          if (result.status === 'rejected') {
            console.warn(`[harmony-client] ${tasks[i][0]} failed after reconnect:`, result.reason);
          }
        }
      }
      const unlistenStatus = await listen('zenoh-status', async (event) => {
        const status = (event as { payload: { status: string } }).payload;
        if (status.status === 'connected') {
          await fetchOwnAddress();
          await reloadBackendState();
        }
      });
      // zenoh-status serves messages, vines, and nav (fetchOwnAddress sets
      // all ownAddress fields). All four services are destroyed on unmount;
      // cleanup order is irrelevant since no service depends on another's
      // teardown. Registered on fileManagerService arbitrarily.
      // listen() returns UnlistenFn (= () => void), matching addUnlisten's
      // signature directly — no cast needed.
      fileManagerService.addUnlisten(unlistenStatus);
    } catch (err) {
      console.warn('[harmony-client] Tauri init failed:', err);
    }
  })();
  let flashcardStats = $state(initialSessionStats());
  let trustVersion = $state(0);

  function handleTrustChange() {
    trustVersion++;
  }

  // ── File manager state ──────────────────────────────────────────────
  let selectedFileCid = $state<string | null>(null);
  let selectedFileSidecarId = $state<string | null>(null);
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
    // Lookup sidecarId from the active list. The FileBrowser only emits
    // cids on click (its callback contract is unchanged), so we resolve
    // sidecarId here. Top-level entries always have a sidecarId; for
    // manifest-derived rows (sidecarId === '' from the wire) selection
    // is informational only — pin/burn/archive are gated downstream.
    const item = fileManagerService.getContents().find((i) => i.cid === cid);
    selectedFileSidecarId = item?.sidecarId || null;
  }

  function handleNavigateFolder(cid: string | null) {
    currentFolderCid = cid;
    selectedFileCid = null;
    selectedFileSidecarId = null;
    showCleanup = false;
  }

  async function handleFileBurn() {
    if (!selectedFileSidecarId) return;
    try {
      await fileManagerService.burn([selectedFileSidecarId]);
      fileManagerVersion++;
      selectedFileCid = null;
      selectedFileSidecarId = null;
    } catch (err) {
      console.error('File burn failed:', err);
    }
  }

  async function handleFileArchive() {
    if (!selectedFileSidecarId) return;
    try {
      await fileManagerService.archive([selectedFileSidecarId]);
      fileManagerVersion++;
      selectedFileCid = null;
      selectedFileSidecarId = null;
    } catch (err) {
      console.error('File archive failed:', err);
    }
  }

  function handleFilePublish(cid: string) {
    fileManagerService.publish([cid]);
    fileManagerVersion++;
    selectedFileCid = null;
    selectedFileSidecarId = null;
  }

  function handleFileRelease(cid: string) {
    fileManagerService.release([cid]);
    fileManagerVersion++;
    selectedFileCid = null;
    selectedFileSidecarId = null;
  }

  async function handleFilePin() {
    if (!selectedFileSidecarId) return;
    try {
      await fileManagerService.pin(selectedFileSidecarId);
      fileManagerVersion++;
    } catch (err) {
      // Most common failure: pin quota exhausted. Surface via console for now;
      // a proper toast/error channel is tracked separately.
      console.error('File pin failed:', err);
    }
  }

  async function handleFileUnpin() {
    if (!selectedFileSidecarId) return;
    try {
      await fileManagerService.unpin(selectedFileSidecarId);
      fileManagerVersion++;
    } catch (err) {
      console.error('File unpin failed:', err);
    }
  }

  async function handleFileExport() {
    if (!selectedFileCid) return;
    try {
      await fileManagerService.exportToDevice([selectedFileCid]);
    } catch {
      // Export can fail if user cancels the save dialog or node is disconnected.
    }
  }

  async function handleFileTierChange(tier: ReplicationTier) {
    if (!selectedFileSidecarId) return;
    try {
      await fileManagerService.setReplicationTier([selectedFileSidecarId], tier);
      fileManagerVersion++;
    } catch (err) {
      console.error('Replication-tier update failed:', err);
    }
  }

  async function handleFileUploadClick() {
    try {
      const item = await fileManagerService.ingest(currentFolderCid);
      if (item) fileManagerVersion++;
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      if (!msg.includes('upload cancelled')) {
        console.error('File upload failed:', msg);
      }
    }
  }

  function handleFileCleanupClick() {
    showCleanup = !showCleanup;
  }

  async function handleCleanupAction(cid: string, action: string) {
    const item = fileManagerService.getContents().find((i) => i.cid === cid);
    try {
      if (action === 'burn') {
        if (!item) return;
        await fileManagerService.burn([item.sidecarId]);
      } else if (action === 'archive') {
        if (!item) return;
        await fileManagerService.archive([item.sidecarId]);
      } else if (action === 'release') {
        fileManagerService.release([cid]);
      } else if (action === 'publish') {
        fileManagerService.publish([cid]);
      } else if (action === 'pin') {
        if (!item) return;
        await fileManagerService.pin(item.sidecarId);
      }
      fileManagerVersion++;
      if (selectedFileCid === cid && (action === 'burn' || action === 'archive' || action === 'release' || action === 'publish')) {
        selectedFileCid = null;
        selectedFileSidecarId = null;
      }
    } catch (err) {
      console.error(`Cleanup ${action} failed:`, err);
    }
  }

  async function handleBulkBurn(cids: string[]) {
    try {
      const sidecarIds = cids
        .map((cid) => fileManagerService.getContents().find((i) => i.cid === cid)?.sidecarId)
        .filter((id): id is string => !!id);
      if (sidecarIds.length === 0) return;
      await fileManagerService.burn(sidecarIds);
      fileManagerVersion++;
      if (selectedFileCid && cids.includes(selectedFileCid)) {
        selectedFileCid = null;
        selectedFileSidecarId = null;
      }
    } catch (err) {
      console.error('Bulk burn failed:', err);
    }
  }

  async function handleBulkArchive(cids: string[]) {
    try {
      const sidecarIds = cids
        .map((cid) => fileManagerService.getContents().find((i) => i.cid === cid)?.sidecarId)
        .filter((id): id is string => !!id);
      if (sidecarIds.length === 0) return;
      await fileManagerService.archive(sidecarIds);
      fileManagerVersion++;
      if (selectedFileCid && cids.includes(selectedFileCid)) {
        selectedFileCid = null;
        selectedFileSidecarId = null;
      }
    } catch (err) {
      console.error('Bulk archive failed:', err);
    }
  }

  function handleBulkRelease(cids: string[]) {
    fileManagerService.release(cids);
    fileManagerVersion++;
    if (selectedFileCid && cids.includes(selectedFileCid)) {
      selectedFileCid = null;
      selectedFileSidecarId = null;
    }
  }

  function handleBulkPublish(cids: string[]) {
    fileManagerService.publish(cids);
    fileManagerVersion++;
    if (selectedFileCid && cids.includes(selectedFileCid)) {
      selectedFileCid = null;
      selectedFileSidecarId = null;
    }
  }

  // Mock per-peer override to demonstrate settings
  notificationService.setPeerPolicy('q7r8s9t0', { quiet: 'silent' });

  // Thread state
  let openThreadId = $state<string | null>(null);
  let threadModes = $state<Map<string, ThreadDisplayMode>>(new Map());
  let pinnedThreadIds = $state<Set<string>>(new Set());

  let activeChannel = $state('general');
  let activeHub = $state('harmony-dev');
  let activeChannelName = $state('general');
  let activeChannelType = $state<'channel' | 'dm' | 'group-chat'>('channel');

  function switchMode(mode: AppMode) {
    appMode = mode;
    showSettings = false;
    showCleanup = false;
    fileFilters = {};
    fileSearchQuery = '';
    selectedFileCid = null;
    selectedFileSidecarId = null;
    currentFolderCid = null;
  }

  function handleNodeClick(id: string) {
    const node = findNode(navNodes, id);
    if (!node || node.type === 'folder') return;
    const switched = id !== activeChannel;
    activeChannel = node.id;
    activeHub = findNearestFolder(navNodes, node.id) ?? '';
    activeChannelName = node.name;
    activeChannelType = node.type as 'channel' | 'dm' | 'group-chat';
    if (appMode !== 'messages') {
      switchMode('messages');
    }
    // Close any open thread when switching channels (but not when
    // re-clicking the already-active channel).
    if (switched) {
      openThreadId = null;
    }
  }

  // Filter to messages in the active channel (mock messages without
  // channel/hub pass through so pre-existing seed data still shows).
  let channelMessages = $derived(
    allMessages.filter(m =>
      !m.channel || (m.channel === activeChannel && m.hub === activeHub)
    )
  );

  // Thread derivations — scoped to the active channel so thread
  // indicators and panel contents don't leak cross-channel messages.
  let threadMeta = $derived(getThreadMeta(channelMessages));

  let threadRoot = $derived(
    openThreadId
      ? channelMessages.find(m => m.id === openThreadId) ?? null
      : null
  );

  let threadReplies = $derived(
    openThreadId
      ? channelMessages.filter(m => m.replyTo === openThreadId)
      : []
  );

  let threadMessageIds = $derived(
    openThreadId
      ? new Set(threadReplies.map(m => m.id))
      : new Set<string>()
  );

  // Main feed: exclude replies for panel/muted threads, keep inline
  let mainFeedMessages = $derived(
    channelMessages.filter(m => {
      if (!m.replyTo) return true;
      const mode = threadModes.get(m.replyTo) ?? 'panel';
      return mode === 'inline';
    })
  );

  // Media feed: main + open thread replies (exclude muted)
  let mediaMessages = $derived.by(() => {
    const base = channelMessages.filter(m => {
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

  async function handleSend(text: string, priority: MessagePriority) {
    try {
      await messageService.send(text, priority, activeChannel, activeHub);
    } catch (err) {
      console.error('Failed to send message:', err);
    }
  }

  function handleThreadOpen(rootId: string) {
    openThreadId = rootId;
  }

  function handleThreadClose() {
    openThreadId = null;
  }

  async function handleThreadSend(text: string, priority: MessagePriority) {
    if (!openThreadId) return;
    try {
      await messageService.send(text, priority, activeChannel, activeHub, openThreadId);
    } catch (err) {
      console.error('Failed to send thread reply:', err);
    }
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

<Layout {collapsed} {showSettings} mode={appMode} mailSelected={selectedMailCid !== null}>
  {#snippet nav()}
    <NavPanel
      nodes={navNodes}
      {collapsed}
      activeNodeId={activeChannel}
      onNodeClick={handleNodeClick}
      onSettingsClick={() => { showSettings = !showSettings; }}
      profileLookup={(addr) => navService.profileLookup(addr)}
      onModeChange={switchMode}
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
      channelName={activeChannelName}
      channelType={activeChannelType}
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
    <VineFeed
      {followedVines}
      {discoverVines}
      viewedIds={vineViewedIds}
      activeTab={vineTab}
      {followedAddresses}
      onTabChange={(tab) => { vineTab = tab; }}
      onMarkViewed={handleMarkVineViewed}
      onPublish={() => showVinePublish = true}
      onReshare={handleVineReshare}
      onFollow={handleVineFollow}
      onUnfollow={handleVineUnfollow}
      getReaction={vineGetReaction}
      onToggleLike={handleVineToggleLike}
      resolveVideo={resolveVideoFn}
    />
    {#if showVinePublish}
      <VinePublishDialog onPublish={handleVinePublish} onClose={() => showVinePublish = false} />
    {/if}
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
  {#snippet mailInbox()}
    {#if showCompose}
      {#key composeReplyTo}
        <MailCompose
          replyTo={composeReplyTo}
          initialTo={composeInitialTo}
          initialSubject={composeInitialSubject}
          onSend={async (to, subject, body, replyTo) => {
            await mailService.send(to, subject, body, replyTo ?? undefined);
            showCompose = false;
            composeReplyTo = null;
          }}
          onCancel={() => { showCompose = false; composeReplyTo = null; }}
        />
      {/key}
    {:else}
      <MailInbox
        entries={mailEntries}
        activeFolder={activeMailFolder}
        counts={mailCounts}
        selectedCid={selectedMailCid}
        syncState={mailSyncState}
        syncError={mailSyncError}
        onRefresh={() => { mailService.refresh().catch(() => {}); }}
        onSelectEmail={async (cid) => {
          selectedMailCid = cid;
          const folder = activeMailFolder;
          mailDetailLoading = true;
          mailDetailError = null;
          selectedMailDetail = null;
          try {
            const detail = await mailService.getMessage(cid);
            if (selectedMailCid !== cid) return; // stale selection
            selectedMailDetail = detail;
            mailDetailLoading = false;
            if (!detail) {
              mailDetailError = 'Message not found or fetch failed';
            }
          } catch (e) {
            if (selectedMailCid !== cid) return;
            mailDetailLoading = false;
            mailDetailError = e instanceof Error ? e.message : String(e);
            return;
          }
          // markRead is fire-and-forget: a read-status update failure must
          // not replace a successfully loaded message with an error view.
          if (selectedMailDetail) {
            mailService.markRead(cid, folder).catch((err) => {
              console.warn('markRead failed:', err);
            });
          }
        }}
        onFolderChange={async (folder) => {
          activeMailFolder = folder;
          selectedMailCid = null;
          selectedMailDetail = null;
          mailDetailLoading = false;
          mailDetailError = null;
          await mailService.loadFolder(folder);
        }}
        onCompose={() => { showCompose = true; composeReplyTo = null; composeInitialTo = ''; composeInitialSubject = ''; }}
        onMarkRead={(cid) => { mailService.markRead(cid).catch(() => {}); }}
        onMoveTrash={(cid) => { mailService.moveToTrash(cid).catch(() => {}); }}
      />
    {/if}
  {/snippet}
  {#snippet mailDetail()}
    <MailReader
      message={selectedMailDetail}
      loading={mailDetailLoading}
      error={mailDetailError}
      onReply={(_cid, msgId) => {
        composeReplyTo = msgId;
        composeInitialTo = selectedMailDetail?.senderAddress ?? '';
        const subj = selectedMailDetail?.subject ?? '';
        composeInitialSubject = subj.startsWith('Re: ') ? subj : `Re: ${subj}`;
        showCompose = true;
      }}
      onBack={() => {
        selectedMailCid = null;
        selectedMailDetail = null;
        mailDetailLoading = false;
        mailDetailError = null;
      }}
    />
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
