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
  import IdentityPanel from './lib/components/IdentityPanel.svelte';
  import DevicesPanel from './lib/components/DevicesPanel.svelte';
  import SpellbookMode from './lib/components/SpellbookMode.svelte';
  import FlashcardStats from './lib/components/FlashcardStats.svelte';
  import MailInbox from './lib/components/MailInbox.svelte';
  import MailReader from './lib/components/MailReader.svelte';
  import MailCompose from './lib/components/MailCompose.svelte';
  import ProfilePopover from './lib/components/ProfilePopover.svelte';
  import VinePublishDialog from './lib/components/VinePublishDialog.svelte';
  import DmCreateDialog from './lib/components/DmCreateDialog.svelte';
  import ConfirmDialog from './lib/components/ConfirmDialog.svelte';
  import CreateCommunityDialog from './lib/components/CreateCommunityDialog.svelte';
  import RedeemInviteDialog from './lib/components/RedeemInviteDialog.svelte';
  import CommunityView from './lib/components/CommunityView.svelte';
  import LibraryDirectoryBrowser from './lib/components/LibraryDirectoryBrowser.svelte';
  import { LibraryDirectoryService } from './lib/library-directory-service';
  import { ProfileBroadcastService } from './lib/profile-broadcast-service';
  import type { TauriAdapter } from './lib/zenoh-service';
  import { CommunityService } from './lib/community-service';
  import { ChannelMessageService } from './lib/channel-message-service';
  import type { CommunityMember } from './lib/types';
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
  import { resolveOriginalCreator } from './lib/vine-utils';
  import { NavService } from './lib/nav-service';
  import { AvatarResolver } from './lib/avatar-resolver';
  import type { AppMode, Message, MessagePriority, Profile, ThreadDisplayMode, FileViewMode, ContentSection, ReplicationTier, MailFolderKind, MailMessageDetail, ContentItem, CleanupRecommendation } from './lib/types';
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
  // Mirrors `vineGetReaction` but for reshare counts (computed on demand
  // from both vine arrays — see `VineService.getReshareCount`). Re-bound
  // on every `onChange` so the VineFeed re-renders when a new reshare
  // arrives. The wrapper layer (closure-over-`vineService`) is what makes
  // it reactive — Svelte tracks the `$state` slot, not the method call.
  let vineGetReshareCount = $state<(vineId: string) => number>(
    (vineId: string) => vineService.getReshareCount(vineId)
  );
  // Target vine to open in VineFeed's internal player. Set by
  // `handleViewOriginal` after resolving via `vineService.findVine`;
  // VineFeed watches this via `$effect` and clears the state slot back
  // to null once it has opened the player. The slot-clear handler lives
  // here so we keep the "VineFeed owns the player" contract intact (we
  // don't have an App-level player state to reuse).
  let viewOriginalTarget = $state<import('./lib/types').VineVideo | null>(null);

  vineService.onChange = () => {
    followedVines = [...vineService.followedVines];
    discoverVines = [...vineService.discoverVines];
    vineViewedIds = new Set(vineService.viewedIds);
    followedAddresses = new Set(vineService.followedAddresses);
    vineGetReaction = (vineId: string) => vineService.getReaction(vineId);
    vineGetReshareCount = (vineId: string) => vineService.getReshareCount(vineId);
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
      // Resolve the true origin. If `vine` is itself a reshare, the
      // helper propagates its `originalCreator*` fields (transitive —
      // points at the true creator, not the intermediate resharer).
      // Otherwise `vine` IS the origin, so the helper falls back to
      // its own creator. See `vine-utils.ts` and §Edge Cases →
      // Resharing a reshare.
      const { originalCreatorAddress, originalCreatorName } =
        resolveOriginalCreator(vine);
      await vineService.publish(
        vine.videoCid,
        vine.title,
        vine.id,
        originalCreatorAddress,
        originalCreatorName,
      );
    } catch (err) {
      console.error('Vine reshare failed', err);
      throw err;
    }
  }

  function handleViewOriginal(vineId: string) {
    // Resolve the original vine from the local feed (followed or
    // discover). If it's not in the local feed — e.g., creator isn't
    // followed and the original wasn't surfaced via Discover — we
    // silently no-op per spec §Edge Cases → Original not in feed. No
    // toast: a network-discovery probe is the right long-term path,
    // but emitting "the original isn't on your network" every time
    // would be a noisy UX for what's a common state.
    const original = vineService.findVine(vineId);
    if (!original) return;
    // VineFeed owns the player; toggling its `playTarget` prop is how
    // we trigger its internal `openPlayer`. We re-assign on every
    // click (even to the same vine) by clearing first so VineFeed's
    // `$effect` re-runs — otherwise clicking the attribution link
    // twice in a row would be a no-op on the second click.
    viewOriginalTarget = null;
    queueMicrotask(() => { viewOriginalTarget = original; });
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

  // ── DM creation modal (ZEB-228 Phase 4 Task 13) ─────────────────────
  // The "+ New DM" button at the bottom of the nav sidebar opens this
  // modal. Submit invokes `add_space` (DM/GroupDm wire codes), which
  // dispatches DmInvites; the backend's apply_space + nav-updated emit
  // will trigger NavService to insert the new NavNode. We switch to it
  // after a short tick so NavService has time to receive the event.
  let dmCreateDialogOpen = $state(false);
  // Tracks which FAB menu item opened the DM dialog so the dialog
  // can change its framing (title, search placeholder, hint) — the
  // actual kind is still derived from selected.length inside the
  // dialog, so this is purely UX, not load-bearing.
  let dmCreateInitialKind = $state<'dm' | 'group-dm'>('dm');

  // ── Community dialogs / panel state (ZEB-263 Phase 5 Task 7) ───────
  // Three modals + a right-pane overview gate on the selected community
  // id. The dialog `pending`/`error` state stays in App.svelte rather
  // than the dialog component so a re-open after an error gets a fresh
  // state without remounting (and so multiple dialogs can be cycled).
  let showCreateCommunity = $state(false);
  let showRedeemInvite = $state(false);
  let createPending = $state(false);
  let createError = $state<string | null>(null);
  let redeemPending = $state(false);
  let redeemError = $state<string | null>(null);
  let redeemUrl = $state('');

  // ── ZEB-218 Sub-D Phase 1: library directory browser modal ─────────
  // The adapter is constructed inside the Tauri-init IIFE below; we
  // hoist a reference here so the LibraryDirectoryService can be
  // lazy-created when the user opens the browser. The service itself
  // is `null` until the adapter wires up — opening the browser before
  // Tauri-init has completed will leave the button effectively inert.
  let tauriAdapter = $state<TauriAdapter | null>(null);
  let libraryDirectoryService = $state<LibraryDirectoryService | null>(null);
  let libraryDirectoryOpen = $state(false);
  // ── ZEB-281 Sub-D Phase 4: profile-membership broadcast service ────
  // Shared singleton wrapping the four set/subscribe/unsubscribe/cached
  // IPCs. Lazy-created alongside libraryDirectoryService once the Tauri
  // adapter wires up. The CommunitySettingsPanel toggle and the
  // ProfilePopover memberships section both route through this instance.
  let profileBroadcastService = $state<ProfileBroadcastService | null>(null);
  // Local mirror of per-community shared_in_profile state. The backend
  // is the source of truth; we hydrate this Map at startup via
  // `profileBroadcastService.listSharedSet()` (see ZEB-281 Sub-D Phase 4
  // R1 — required so the toggle reflects server state after restart and
  // the UI never claims "off / private" while the publisher broadcasts
  // "on / public"). The settings panel rolls back on toggle failure.
  let sharedInProfileByCommunity = $state<Map<string, boolean>>(new Map());
  let selectedCommunityId = $state<string | null>(null);
  let communityMembers = $state<CommunityMember[]>([]);
  let myAddress = $state('');
  // Local mirror of communityService.isDegraded(selectedCommunityId).
  // Direct method calls in the template aren't reactive — we need a
  // $state field that the listener can update so the settings panel
  // re-renders on degraded events.
  let isCurrentCommunityDegraded = $state(false);
  // Derived from the live roster — recomputes when fetchOwnAddress
  // resolves later than the first roster load (race fixed in PR #91
  // review). Never assign to this directly.
  let myCommunityPower = $derived(
    communityMembers.find((m) => m.address === myAddress)?.power ?? 0,
  );
  // Count only currently-joined members so the overview matches the
  // "X joined" line in CommunitySettingsPanel — invited/banned/left
  // entries shouldn't be counted as members in either place.
  let joinedCommunityCount = $derived(
    communityMembers.filter((m) => m.status === 'joined').length,
  );

  // Centralized switch helper. Clears the visible roster on a real
  // community change so the settings panel doesn't flash the previous
  // community's members during the in-flight fetch (greptile P1).
  // Also captures the current degraded flag so the UI doesn't lag
  // behind the backend signal until the next members-changed event.
  function changeSelectedCommunity(id: string | null) {
    if (selectedCommunityId !== id) {
      communityMembers = [];
    }
    selectedCommunityId = id;
    isCurrentCommunityDegraded = id != null ? communityService.isDegraded(id) : false;
  }

  async function refreshCommunityMembers(id: string) {
    try {
      const fresh = await communityService.listCommunityMembers(id);
      // Stale-response guard: if the user switched communities while
      // we were awaiting, drop the result rather than overwriting the
      // newly-selected community's roster with the previous one's.
      if (selectedCommunityId !== id) return;
      communityMembers = fresh;
    } catch (e) {
      // listCommunityMembers throws when the adapter isn't connected
      // (mock-data mode) or when the backend isn't ready. Surface the
      // failure to the console rather than crash the app — the panel
      // simply renders with an empty member list.
      if (selectedCommunityId !== id) return;
      const msg = e instanceof Error ? e.message : String(e);
      console.warn('[harmony-client] listCommunityMembers failed:', msg);
      communityMembers = [];
    }
  }

  async function handleDmCreate(args: { kind: 'dm' | 'group-dm'; members: string[]; name: string }) {
    try {
      const { invoke } = await import('@tauri-apps/api/core');
      const spaceId = (await invoke('add_space', {
        kind: args.kind, // backend accepts the wire codes "dm" / "group-dm"
        name: args.name,
        members: args.members,
      })) as string;
      dmCreateDialogOpen = false;
      // Fix B from PR #81 review: there's no Rust-side `nav-updated`
      // emit yet, so waiting on the IPC event meant the new DM never
      // appeared in the nav tree. Synthesize the NavNode directly via
      // navService — same logic the (still-wired) listener uses, so a
      // future backend emit won't double-insert (the duplicate-added
      // path preserves UI state via Fix G).
      navService.addOrUpdateNavSpace({
        action: 'added',
        spaceId,
        kind: args.kind,
        name: args.name,
        members: args.members,
        parentId: null,
      });
      // Switch synchronously — the node is in navService.nodes now, no
      // need for the old setTimeout delay.
      handleNodeClick(spaceId);
    } catch (e) {
      // Phase 4 v1: log to console. The dialog's client-side recipient cap
      // catches the most common failure (16+ members); other failures
      // (backend not ready, decoding errors) are rare and currently shown
      // only in the dev console. Toast UX is a polish follow-up.
      console.error('add_space failed:', e);
    }
  }

  // ── Inline manual delete on stuck/expired DMs (ZEB-228 Phase 4 Task 14) ─
  // TextMessage surfaces an inline ⓧ when a self-Message has been stuck in
  // 'sending' for >60s, or has reached terminal 'expired'/'failed' state.
  // The click drops here through TextFeed.onMessageDelete; we open a
  // ConfirmDialog with state-appropriate copy. Confirm dispatches the
  // delete_outbox_entry IPC; the backend's `dm-deleted` event arrives
  // and MessageService prunes the message from the per-channel buffer.
  let pendingDeleteMessageId: string | null = $state(null);
  let pendingDeleteState: string | null = $state(null);

  function requestDeleteMessage(messageId: string) {
    const msg = messageService.messages.find((m) => m.messageId === messageId);
    pendingDeleteMessageId = messageId;
    pendingDeleteState = msg?.deliveryState ?? null;
  }

  async function confirmDeleteMessage() {
    if (!pendingDeleteMessageId) return;
    const id = pendingDeleteMessageId;
    pendingDeleteMessageId = null;
    pendingDeleteState = null;
    try {
      const { invoke } = await import('@tauri-apps/api/core');
      await invoke('delete_outbox_entry', { messageId: id });
      // The `dm-deleted` IPC event will land via MessageService's
      // subscription (Task 5) and prune the message from the buffer.
    } catch (e) {
      // Production rejections are strings; tests may surface Error objects.
      // Per `feedback_tauri_error_extraction`: always normalize.
      const msg = e instanceof Error ? e.message : String(e);
      console.error('delete_outbox_entry failed:', msg);
    }
  }

  function cancelDeleteMessage() {
    pendingDeleteMessageId = null;
    pendingDeleteState = null;
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

  // ── Community service (ZEB-263) ────────────────────────────────────
  // Mirrors the MessageService / NavService pattern: constructed eagerly,
  // adapter wired up inside the Tauri-init IIFE below, destroy() ran on
  // unmount via $effect cleanup.
  const communityService = new CommunityService();
  $effect(() => () => communityService.destroy());
  const channelMessageService = new ChannelMessageService();
  $effect(() => () => channelMessageService.destroy());

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

  // ZEB-263: Wire communityService.onChange BEFORE the init IIFE so that
  // the first roster-changed event (which may fire as soon as connectAdapter
  // resolves) is never dropped silently. Svelte 5 $state reads inside the
  // closure are always current at call time — no TDZ risk.
  communityService.onMembersChanged = (changedId: string) => {
    if (selectedCommunityId && changedId === selectedCommunityId) {
      // Route through refreshCommunityMembers so we get the same
      // try/catch + stale-response guard as the imperative caller.
      // Don't await — onMembersChanged is fire-and-forget at the
      // listener boundary; awaiting here would leak unhandled
      // rejections.
      void refreshCommunityMembers(selectedCommunityId);
    }
  };

  communityService.onDegradedChanged = (changedId: string) => {
    // Degraded transitions don't affect the member roster — only
    // mirror the flag into local $state so the settings panel
    // re-renders. Avoiding the roster fetch here was a deliberate
    // split (cursor flagged the unnecessary reactive cascade).
    if (selectedCommunityId && changedId === selectedCommunityId) {
      isCurrentCommunityDegraded = communityService.isDegraded(changedId);
    }
  };

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
      // ZEB-218 Sub-D Phase 1: expose adapter + lazy-init library
      // directory service so the NavPanel "Browse libraries" button
      // can mount the browser modal once Tauri-init has wired up.
      tauriAdapter = adapter;
      libraryDirectoryService = new LibraryDirectoryService(adapter);
      profileBroadcastService = new ProfileBroadcastService(adapter);

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

      // ZEB-281 Sub-D Phase 4 R1: hydrate the per-community
      // shared_in_profile mirror from the backend so the toggle in
      // CommunitySettingsPanel reflects the server-side state on the
      // very first render after restart. Without this, the toggle would
      // default to OFF for every community even when the publisher is
      // still broadcasting opted-in Communities (the publisher reads
      // from CRDT, not from this Map). Requires crdt_state to be
      // populated — runs after start_node above. Failure falls back to
      // an empty Map (all toggles OFF), the safe default for the
      // privacy invariant.
      try {
        const sharedIds = await profileBroadcastService.listSharedSet();
        const next = new Map<string, boolean>();
        for (const cid of sharedIds) {
          next.set(cid, true);
        }
        // Re-assign to trigger Svelte 5 reactivity on the $state Map
        // (same idiom as the toggle handler below).
        sharedInProfileByCommunity = next;
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        console.warn('[harmony-client] listSharedSet hydration failed:', msg);
      }

      await tryConnect('message', messageService.connectAdapter(adapter));
      // Mail connect may fail if mail_mgr isn't ready yet (race with
      // start_node); non-blocking so other services proceed. The
      // zenoh-status handler below re-hydrates mail state on reconnect.
      tryConnect('mail', mailService.connectAdapter(adapter));
      await tryConnect('vine', vineService.connectAdapter(adapter));
      await tryConnect('vine.loadFollowed', vineService.loadFollowed());
      await tryConnect('fileManager', fileManagerService.connectAdapter(adapter));
      await tryConnect('community', communityService.connectAdapter(adapter));
      await tryConnect('channelMessage', channelMessageService.connectAdapter(adapter));
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
          // ZEB-263: keep our reactive copy in sync so CommunitySettingsPanel
          // can identify "you" + gate kick/setPower self-actions.
          myAddress = addr;
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
  function handleFileItemClick(item: ContentItem) {
    selectedFileCid = item.cid;
    // ZEB-164: sidecarId is the stable per-entry identity. For manifest-derived
    // rows (sidecarId === '') selection is informational only — pin/burn/archive
    // are gated downstream.
    selectedFileSidecarId = item.sidecarId || null;
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

  async function handleCleanupAction(rec: CleanupRecommendation, action: string) {
    try {
      if (action === 'burn') {
        await fileManagerService.burn([rec.sidecarId]);
      } else if (action === 'archive') {
        await fileManagerService.archive([rec.sidecarId]);
      } else if (action === 'release') {
        fileManagerService.release([rec.cid]);
      } else if (action === 'publish') {
        fileManagerService.publish([rec.cid]);
      } else if (action === 'pin') {
        await fileManagerService.pin(rec.sidecarId);
      }
      fileManagerVersion++;
      if (selectedFileCid === rec.cid && (action === 'burn' || action === 'archive' || action === 'release' || action === 'publish')) {
        selectedFileCid = null;
        selectedFileSidecarId = null;
      }
    } catch (err) {
      console.error(`Cleanup ${action} failed:`, err);
    }
  }

  async function handleBulkBurn(recs: CleanupRecommendation[]) {
    try {
      const sidecarIds = recs.map((r) => r.sidecarId).filter(Boolean);
      if (sidecarIds.length === 0) return;
      await fileManagerService.burn(sidecarIds);
      fileManagerVersion++;
      if (selectedFileCid && recs.some((r) => r.cid === selectedFileCid)) {
        selectedFileCid = null;
        selectedFileSidecarId = null;
      }
    } catch (err) {
      console.error('Bulk burn failed:', err);
    }
  }

  async function handleBulkArchive(recs: CleanupRecommendation[]) {
    try {
      const sidecarIds = recs.map((r) => r.sidecarId).filter(Boolean);
      if (sidecarIds.length === 0) return;
      await fileManagerService.archive(sidecarIds);
      fileManagerVersion++;
      if (selectedFileCid && recs.some((r) => r.cid === selectedFileCid)) {
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
  // The nav row to render with active styling. When a community is
  // selected, highlight the community node; otherwise fall back to
  // the active channel/DM. Keeping these in separate $state fields
  // avoids reusing activeChannel for community ids — activeChannel
  // is consumed by message-send paths that only make sense for
  // channels/DMs.
  let navActiveNodeId = $derived(selectedCommunityId ?? activeChannel);

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
    // ZEB-263: community nodes route to the right-pane overview placeholder
    // instead of the message feed (no channels yet — that's a later phase).
    if (node.type === 'community') {
      changeSelectedCommunity(id);
      void refreshCommunityMembers(id);
      if (appMode !== 'messages') {
        switchMode('messages');
      }
      return;
    }
    // Clicking any non-community navigable node clears the community
    // overview so the message feed shows through.
    changeSelectedCommunity(null);
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
    // Phase 4 (ZEB-228) — fetch decrypted DM scrollback on switch so
    // cold-start (no live dm-received yet) renders history. Per-channel
    // pagination cursor in MessageService prevents repeat fetches of
    // the same page on rapid switches; no-op when offline / pre-adapter.
    if (node.type === 'dm' || node.type === 'group-chat') {
      messageService.loadDmThread(node.id).catch((e) => {
        console.error('loadDmThread failed:', e);
      });
    }
  }

  // Filter to messages in the active channel (mock messages without
  // channel/hub pass through so pre-existing seed data still shows).
  //
  // PR #81 round 4 (Greptile P1): for DM/group-chat channels, skip the
  // hub equality check entirely. DM Messages always carry `hub: ''`,
  // but `activeHub` is computed via `findNearestFolder(node.id)` —
  // which returns the folder's id when a DM is dragged into a folder.
  // The folder placement is a NavService UI-state concept, not a DM
  // message-routing key; channels live in hubs, DMs do not. Without
  // this special-case the moment a user organizes a DM into a folder
  // every message in that DM disappears from the feed.
  let channelMessages = $derived(
    allMessages.filter(m => {
      if (!m.channel) return true; // mock seed pass-through
      if (m.channel !== activeChannel) return false;
      if (activeChannelType === 'dm' || activeChannelType === 'group-chat') {
        return true; // DMs ignore hub — folder placement is UI-only
      }
      return m.hub === activeHub;
    })
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
    // Phase 4 (ZEB-228) — DM/GroupDm channels route through the
    // send_dm IPC instead of the channel publish path. Optimistic UI:
    // a placeholder Message is pushed in 'sending' state immediately;
    // on success the placeholder's id is swapped for the real
    // OutboxEntryId returned by send_dm so dm-delivered / dm-expired
    // can correlate via `messageId`. On failure the placeholder is
    // marked 'failed' (kept visible — losing the bubble would hide
    // the user's intent).
    if (activeChannelType === 'dm' || activeChannelType === 'group-chat') {
      const optimisticId = crypto.randomUUID();
      const optimistic: Message = {
        id: optimisticId,
        // Fix D from PR #81 review: self-sender uses the 'self' sentinel
        // (matches the channel-publish convention). Using the real
        // ownAddress here confused downstream classification (e.g.
        // knownPeers derivation, isSelf computation in TextFeed).
        sender: { address: 'self', displayName: 'You' },
        text,
        timestamp: Date.now(),
        media: [],
        priority,
        channel: activeChannel,
        // Fix A from PR #81 review: top-level DMs have activeHub=''; the
        // feed filter compares Message.hub against activeHub — without
        // hub:'' here, the optimistic bubble fails the filter and never
        // renders in the active feed.
        hub: '',
        deliveryState: 'sending',
      };
      messageService.pushOptimistic(optimistic);

      try {
        const { invoke } = await import('@tauri-apps/api/core');
        // Fix C from PR #81 review: send_dm now returns
        // { messageId, messageCid } (post-b58a15b). messageCid is the
        // stable id (matches the dm-received echo / scrollback fetch);
        // messageId is the OutboxEntryId used only for lifecycle
        // correlation in dm-delivered / dm-expired / dm-deleted.
        const result = (await invoke('send_dm', {
          spaceId: activeChannel,
          content: Array.from(new TextEncoder().encode(text)),
          mimeType: 'text/plain',
        })) as { messageId: string; messageCid: string };
        messageService.replaceOptimisticId(
          optimisticId,
          result.messageCid,
          result.messageId,
        );
      } catch (e) {
        messageService.markFailed(optimisticId, String(e));
        console.error('DM send failed:', e);
      }
      return;
    }

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

  // ZEB-263: derive the selected community NavNode (if any) so the
  // right-pane overview placeholder + the settings panel can read its
  // name without the redundant `find` lookup at every render site.
  let selectedCommunityNode = $derived(
    selectedCommunityId
      ? navNodes.find((n) => n.id === selectedCommunityId && n.type === 'community') ?? null
      : null
  );

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
    <div class="nav-with-dm-create">
      <NavPanel
        nodes={navNodes}
        {collapsed}
        activeNodeId={navActiveNodeId}
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
        onNewDm={() => { dmCreateInitialKind = 'dm'; dmCreateDialogOpen = true; }}
        onNewGroupDm={() => { dmCreateInitialKind = 'group-dm'; dmCreateDialogOpen = true; }}
        onNewCommunity={() => { showCreateCommunity = true; createError = null; }}
        onRedeemInvite={() => { showRedeemInvite = true; redeemError = null; redeemUrl = ''; }}
        onBrowseLibraries={() => { libraryDirectoryOpen = true; }}
      />
      {#if !collapsed && appMode === 'messages'}
        <button
          type="button"
          class="new-dm-button"
          onclick={() => { dmCreateInitialKind = 'dm'; dmCreateDialogOpen = true; }}
          title="New direct message"
        >
          <span aria-hidden="true">+</span> New DM
        </button>
      {/if}
    </div>
  {/snippet}
  {#snippet textFeed()}
    {#if selectedCommunityNode}
      <CommunityView
        communityId={selectedCommunityNode.id}
        communityName={selectedCommunityNode.name}
        communityKind={communityService.getKind(selectedCommunityNode.id)}
        members={communityMembers}
        ownAddress={myAddress}
        myPower={myCommunityPower}
        isDegraded={isCurrentCommunityDegraded}
        sharedInProfile={sharedInProfileByCommunity.get(selectedCommunityNode.id) ?? false}
        {communityService}
        {channelMessageService}
        {trustService}
        onKickMember={async (target) => {
          if (!selectedCommunityId) return;
          try {
            await communityService.kickFromCommunity(selectedCommunityId, target);
          } catch (e) {
            const msg = e instanceof Error ? e.message : String(e);
            console.error('kickFromCommunity failed:', msg);
          }
        }}
        onSetPowerLevel={async (target, power) => {
          if (!selectedCommunityId) return;
          try {
            await communityService.setPowerLevel(selectedCommunityId, target, power);
          } catch (e) {
            const msg = e instanceof Error ? e.message : String(e);
            console.error('setPowerLevel failed:', msg);
          }
        }}
        onLeave={async () => {
          if (!selectedCommunityId) return;
          const leavingId = selectedCommunityId;
          try {
            await communityService.leaveCommunity(leavingId);
            // ZEB-265: backend emits nav-updated { action: "removed" }, but
            // events aren't buffered for late listeners. Mirror locally so
            // the node disappears even if the listener missed the emit.
            navService.addOrUpdateNavSpace({
              action: 'removed',
              spaceId: leavingId,
              kind: 'community',
              name: '',
              members: [],
              parentId: null,
            });
            changeSelectedCommunity(null);
          } catch (e) {
            const msg = e instanceof Error ? e.message : String(e);
            console.error('leaveCommunity failed:', msg);
          }
        }}
        onGenerateInvite={async () => {
          if (!selectedCommunityId) throw new Error('no community selected');
          return communityService.generateInvite(selectedCommunityId);
        }}
        onToggleSharedInProfile={async (shared) => {
          if (!selectedCommunityId) return;
          const cid = selectedCommunityId;
          if (!profileBroadcastService) {
            // Tauri adapter not wired yet — surface failure so the
            // settings panel rolls back the checkbox UI.
            throw new Error('profile broadcast service not connected');
          }
          await profileBroadcastService.setShared(cid, shared);
          // Local mirror — backend is source of truth (hydrated at
          // startup via listSharedSet). Reassign the map so the
          // reactive read in the parent picks up the change.
          const next = new Map(sharedInProfileByCommunity);
          next.set(cid, shared);
          sharedInProfileByCommunity = next;
        }}
      />
    {:else}
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
        onMessageDelete={requestDeleteMessage}
        ownAddress={messageService.ownAddress ?? ''}
      />
    {/if}
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
    <IdentityPanel />
    <DevicesPanel />
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
      getReshareCount={vineGetReshareCount}
      onToggleLike={handleVineToggleLike}
      onViewOriginal={handleViewOriginal}
      playTarget={viewOriginalTarget}
      onPlayTargetConsumed={() => { viewOriginalTarget = null; }}
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
      selectedSidecarId={selectedFileSidecarId}
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

{#if popoverProfile && profileBroadcastService}
  <ProfilePopover
    profile={popoverProfile}
    x={popoverX}
    y={popoverY}
    onClose={closePopover}
    ownAddress={myAddress}
    profileBroadcastService={profileBroadcastService}
    resolveCommunityName={(communityIdHex) => {
      // Look up the viewer's own NavNodes (subset of OwnerState.spaces);
      // returns null for communities the viewer isn't a member of, so
      // the popover falls back to the truncated hex id.
      const node = navNodes.find(
        (n) => n.type === 'community' && n.id === communityIdHex,
      );
      return node?.name ?? null;
    }}
  />
{/if}

{#if dmCreateDialogOpen}
  <!-- ZEB-228 Phase 4: DM creation modal. Overlay-click and Esc dismiss;
       inner content stops click propagation so dialog clicks don't
       dismiss. Fix H from PR #81 review: keydown propagation is NOT
       stopped — the overlay's `onkeydown={Escape}` needs to receive
       events whose target is inside .modal-content (e.g. when focus is
       in the search input). Stopping keydown blocked Esc-to-dismiss. -->
  <div
    class="modal-overlay"
    role="presentation"
    onclick={() => { dmCreateDialogOpen = false; }}
    onkeydown={(e) => { if (e.key === 'Escape') dmCreateDialogOpen = false; }}
  >
    <div
      class="modal-content"
      role="dialog"
      aria-modal="true"
      onclick={(e) => e.stopPropagation()}
    >
      <DmCreateDialog
        profiles={navService.profiles}
        initialKind={dmCreateInitialKind}
        onSubmit={handleDmCreate}
        onCancel={() => { dmCreateDialogOpen = false; }}
        onConvertToCommunity={() => {
          dmCreateDialogOpen = false;
          createError = null;
          showCreateCommunity = true;
        }}
      />
    </div>
  </div>
{/if}

{#if pendingDeleteMessageId}
  <!-- ZEB-228 Phase 4 Task 14: confirmation for inline delete on stuck/
       expired DM messages. Copy switches by lifecycle state — expired
       implies the 30-day TTL ran out, anything else implies the recipient
       hasn't seen it yet. -->
  <ConfirmDialog
    title="Delete message?"
    message={pendingDeleteState === 'expired'
      ? "Delete this expired message? It's been undeliverable for 30 days."
      : "Delete this message? It hasn't been delivered yet. Recipients who haven't received it won't see it."}
    confirmLabel="Delete"
    destructive={true}
    onConfirm={confirmDeleteMessage}
    onCancel={cancelDeleteMessage}
  />
{/if}

{#if showCreateCommunity}
  <!-- ZEB-263 Phase 5 Task 7: Create-community modal. Successful create
       returns the new community_id which we adopt as the selection so
       the user lands on the new community's overview placeholder. -->
  <CreateCommunityDialog
    pending={createPending}
    error={createError}
    onSubmit={async (name, kind) => {
      createPending = true;
      createError = null;
      try {
        const id = await communityService.createCommunity(name, kind);
        // ZEB-265: backend emits nav-updated, but Tauri events are not
        // buffered for not-yet-connected listeners and dispatch
        // ordering vs IPC response is timing-sensitive. Mirror the
        // listener payload locally as defense-in-depth — if the
        // listener also fires, addOrUpdateNavSpace's Fix G dedupes.
        navService.addOrUpdateNavSpace({
          action: 'added',
          spaceId: id,
          kind: 'community',
          name,
          members: [],
          parentId: null,
        });
        showCreateCommunity = false;
        changeSelectedCommunity(id);
        await refreshCommunityMembers(id);
      } catch (e) {
        createError = e instanceof Error ? e.message : String(e);
      } finally {
        createPending = false;
      }
    }}
    onCancel={() => {
      showCreateCommunity = false;
      createPending = false;
      createError = null;
    }}
  />
{/if}

{#if showRedeemInvite}
  <!-- ZEB-263 Phase 5 Task 7: Redeem-invite modal. The dialog keeps the
       URL value across re-opens via initialUrl so a transient backend
       error doesn't force re-paste; we clear it explicitly on cancel /
       success. -->
  <RedeemInviteDialog
    pending={redeemPending}
    error={redeemError}
    initialUrl={redeemUrl}
    onSubmit={async (url) => {
      redeemPending = true;
      redeemError = null;
      redeemUrl = url;
      try {
        const dto = await communityService.redeemInvite(url);
        // ZEB-265: same defense-in-depth as create_community —
        // backend emits nav-updated but events aren't buffered for
        // late listeners. dto.communityName carries the real name so
        // there's no placeholder regression vs the listener path.
        navService.addOrUpdateNavSpace({
          action: 'added',
          spaceId: dto.communityId,
          kind: 'community',
          name: dto.communityName,
          members: [],
          parentId: null,
        });
        showRedeemInvite = false;
        redeemUrl = '';
        changeSelectedCommunity(dto.communityId);
        await refreshCommunityMembers(dto.communityId);
      } catch (e) {
        redeemError = e instanceof Error ? e.message : String(e);
      } finally {
        redeemPending = false;
      }
    }}
    onCancel={() => {
      showRedeemInvite = false;
      redeemUrl = '';
      redeemError = null;
    }}
  />
{/if}

{#if libraryDirectoryOpen && libraryDirectoryService && tauriAdapter}
  <!-- ZEB-218 Sub-D Phase 1 + Phase 6 (ZEB-252): library directory
       browser modal. Click-to-join calls `join_open_community(community_id)`
       which re-resolves the entry server-side and delegates to the
       same `redeem_invite_inner` codepath RedeemInviteDialog uses
       (full side-effects: nav-updated synth, kind tracking, selected-
       community switch, member refresh). Stale URLs handled by ZEB-249
       §4.6 EpochCatchup self-healing; no app-level retry needed here. -->
  <div
    class="modal-overlay"
    role="presentation"
    onclick={() => (libraryDirectoryOpen = false)}
    onkeydown={(e) => { if (e.key === 'Escape') libraryDirectoryOpen = false; }}
  >
    <div
      class="modal-content"
      role="dialog"
      aria-modal="true"
      onclick={(e) => e.stopPropagation()}
      onkeydown={(e) => e.stopPropagation()}
    >
      <LibraryDirectoryBrowser
        service={libraryDirectoryService}
        adapter={tauriAdapter}
        onJoin={async (communityId) => {
          // ZEB-252 Sub-D Phase 6: typed direct-join. Backend re-resolves
          // the matching directory entry server-side and delegates to
          // the same redeem_invite_inner codepath RedeemInviteDialog uses.
          // Side-effects (nav-updated synthesis, selected-community switch,
          // member refresh) mirror the dialog handler at line ~1620+ —
          // EXCEPT modal visibility, which the browser owns: its
          // `handleJoin` calls `onClose()` itself on success (and surfaces
          // failures via its `joinError` state). Closing the modal here
          // would unmount the component before `refreshCommunityMembers`
          // resolves, suppressing any late error display. So this handler
          // intentionally has no try/catch and no `libraryDirectoryOpen
          // = false` — both responsibilities live in the browser.
          const dto = await communityService.joinOpenCommunity(communityId);
          navService.addOrUpdateNavSpace({
            action: 'added',
            spaceId: dto.communityId,
            kind: 'community',
            name: dto.communityName,
            members: [],
            parentId: null,
          });
          changeSelectedCommunity(dto.communityId);
          await refreshCommunityMembers(dto.communityId);
        }}
        onClose={() => (libraryDirectoryOpen = false)}
      />
    </div>
  </div>
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

  /* ── DM creation: nav sidebar wrapper + button + modal ────────────── */
  /* Wraps NavPanel + the "+ New DM" button in a flex column so the
     button sits at the bottom of the nav sidebar without scrolling
     out of view. NavPanel's outer .nav-panel is height:100% — we
     override with flex:1 + min-height:0 so it shares space with the
     button instead of forcing a vertical overflow. */
  .nav-with-dm-create {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
  }
  :global(.nav-with-dm-create > .nav-panel) {
    flex: 1;
    min-height: 0;
    height: auto;
  }
  .new-dm-button {
    flex-shrink: 0;
    width: 100%;
    padding: 8px 12px;
    background: rgba(120, 140, 200, 0.15);
    color: var(--text-primary, #e8eaed);
    border: none;
    border-top: 1px solid var(--border, rgba(255, 255, 255, 0.08));
    cursor: pointer;
    font-size: 13px;
    text-align: center;
  }
  .new-dm-button:hover {
    background: rgba(120, 140, 200, 0.3);
  }
  .modal-overlay {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 100;
  }
  .modal-content {
    background: var(--bg-secondary, #222);
    border-radius: 8px;
    box-shadow: 0 4px 24px rgba(0, 0, 0, 0.4);
  }

</style>
