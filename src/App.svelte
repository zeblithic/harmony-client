<script lang="ts">
  // ZEB-605: self-hosted Commons fonts (bundled woff2 — offline; no CDN).
  import '@fontsource-variable/newsreader/opsz.css';
  import '@fontsource/public-sans/400.css';
  import '@fontsource/public-sans/500.css';
  import '@fontsource/public-sans/600.css';
  import '@fontsource/public-sans/700.css';
  import '@fontsource/ibm-plex-mono/400.css';
  import '@fontsource/ibm-plex-mono/500.css';
  import '@fontsource/ibm-plex-mono/600.css';
  import './app.css';
  import { connectOwnerTheme } from './lib/theme-service';
  import Layout from './lib/components/Layout.svelte';
  import { makeCloseRequestedHandler, makeTrayResidentNotifier } from './lib/window-close';
  import NavPanel from './lib/components/NavPanel.svelte';
  import TextFeed from './lib/components/TextFeed.svelte';
  import NotesView from './lib/components/NotesView.svelte';
  import MessagesRail from './lib/components/MessagesRail.svelte';
  import VineFeed from './lib/components/VineFeed.svelte';
  import FileBrowser from './lib/components/FileBrowser.svelte';
  import FileDetailPanel from './lib/components/FileDetailPanel.svelte';
  import SettingsPanel from './lib/components/SettingsPanel.svelte';
  import BackupStalenessWarning from './lib/components/BackupStalenessWarning.svelte';
  import { backupExportRequest } from './lib/backup-export-request.svelte';
  import SpellbookMode from './lib/components/SpellbookMode.svelte';
  import FlashcardStats from './lib/components/FlashcardStats.svelte';
  import MailInbox from './lib/components/MailInbox.svelte';
  import MailReader from './lib/components/MailReader.svelte';
  import MailCompose from './lib/components/MailCompose.svelte';
  import MintLedger from './lib/components/MintLedger.svelte';
  import NetworkHealthView from './lib/components/NetworkHealthView.svelte';
  import ProfilePopover from './lib/components/ProfilePopover.svelte';
  import ProfilePanel from './lib/components/ProfilePanel.svelte';
  import VinePublishDialog from './lib/components/VinePublishDialog.svelte';
  import DmCreateDialog from './lib/components/DmCreateDialog.svelte';
  import ConfirmDialog from './lib/components/ConfirmDialog.svelte';
  import CreateCommunityDialog from './lib/components/CreateCommunityDialog.svelte';
  import RedeemInviteDialog from './lib/components/RedeemInviteDialog.svelte';
  import CreateChannelDialog from './lib/components/CreateChannelDialog.svelte';
  import ModifyChannelDialog from './lib/components/ModifyChannelDialog.svelte';
  import TypedConfirmationModal from './lib/components/TypedConfirmationModal.svelte';
  import CommunityView from './lib/components/CommunityView.svelte';
  import LibraryDirectoryBrowser from './lib/components/LibraryDirectoryBrowser.svelte';
  import ToastHost from './lib/components/ToastHost.svelte';
  import IncomingCallToast from './lib/components/IncomingCallToast.svelte';
  import DmInviteToast from './lib/components/DmInviteToast.svelte';
  import CallInProgressBar from './lib/components/CallInProgressBar.svelte';
  import GroupCallBar from './lib/components/GroupCallBar.svelte';
  import GroupCallBanner from './lib/components/GroupCallBanner.svelte';
  import { VotingAdapter } from './lib/voting-adapter';
  import { ProposalCountService } from './lib/proposal-count-service';
  import { setupDelegateOnBehalfToast } from './lib/voting-toast-wiring';
  import { LibraryDirectoryService } from './lib/library-directory-service';
  import {
    openJoinIroh,
    getPresenceVisibility,
    onPresenceVisibilityChanged,
  } from './lib/connectivity-adapter';
  import { ProfileBroadcastService } from './lib/profile-broadcast-service';
  import type { TauriAdapter } from './lib/zenoh-service';
  import { CommunityService, rosterHasJoinedAuthor, toNavPayload } from './lib/community-service';
  import { FriendService, contactsFromFriends } from './lib/friend-service';
  import { DmInviteService, type PendingDmInviteDto } from './lib/dm-invite-service';
  import { ChannelMessageService } from './lib/channel-message-service';
  import type { CommunityMember } from './lib/types';
  import { POWER_THRESHOLDS } from './lib/types';
  import { NotificationService } from './lib/notification-service';
  import {
    loadNotificationSettings,
    attachNotificationSettingsPersistence,
  } from './lib/notification-settings-persistence';
  import { loadProfile, saveProfile } from './lib/profile-service';
  import { Stq8Service } from './lib/stq8-service';
  import * as stq8ProfileStorage from './lib/stq8-profile-storage';
  import { initialSessionStats } from './lib/flashcard-types';
  import { TrustService } from './lib/trust-service';
  import { FileManagerService } from './lib/file-manager-service';
  import { StorageBuddyService } from './lib/storage-buddy-service';
  import type { ContributionSummaryDto, StorageBuddyDto } from './lib/storage-buddy-service';
  import StorageBuddySheet from './lib/components/StorageBuddySheet.svelte';
  import { MessageService } from './lib/message-service';
  import { NotesService } from './lib/notes-service';
  import { migrateLocalNotes } from './lib/notes-migrate';
  import { MailService } from './lib/mail-service';
  import { VineService } from './lib/vine-service';
  import { resolveOriginalCreator } from './lib/vine-utils';
  import { NavService } from './lib/nav-service';
  import { ChannelNavSyncService } from './lib/channel-nav-sync';
  import { AvatarResolver } from './lib/avatar-resolver';
  import { ProfilePageResolver } from './lib/profile-page-resolver';
  import type { AppMode, Message, MessagePriority, Profile, ThreadDisplayMode, FileViewMode, ContentSection, ReplicationTier, MailFolderKind, MailMessageDetail, ContentItem, CleanupRecommendation } from './lib/types';
  import { getThreadMeta } from './lib/feed-utils';
  import { findNode, findNearestFolder, resolveChannelSelection } from './lib/nav-utils';
  import { isTauri } from './lib/tauri-env';
  import { onMount } from 'svelte';
  import type { Update } from '@tauri-apps/plugin-updater';
  import { checkForUpdate } from './lib/updater-adapter';
  import {
    extractHarmonyInviteUrl,
    queueInviteForPostMint,
    consumeQueuedInvite,
  } from './lib/deep-link-router';
  import UpdateAvailableToast from './lib/components/UpdateAvailableToast.svelte';
  import WelcomeModal from './lib/components/WelcomeModal.svelte';
  import NamePromptModal from './lib/components/NamePromptModal.svelte';
  import BackupReminderBanner from './lib/components/BackupReminderBanner.svelte';
  import type { MintIpcResult, OwnerStateView } from './lib/owner-service';
  import type { StartNodeResponse } from './lib/types/onboarding';
  import { MemberCardService } from './lib/member-card-service';
  import { PresenceService } from './lib/presence-service';
  import { selfCommunityPower } from './lib/community-self-power';
  import { shouldClearMembersLoading } from './lib/members-loading';
  import { getVoiceSession, type VoiceSession } from './lib/voice-session';
  import { getCallSession, type CallSession } from './lib/call-session';
  import { CALL_EVENT_MIME, encodeCallEvent, describeCallEvent, type CallEventPayload } from './lib/call-log';
  import { runQuitTeardown } from './lib/quit-teardown';
  import { getGroupCallSession, type GroupCallSession } from './lib/group-call-session';
  import { groupCallBanners } from './lib/group-call-banner-store';
  import { ensureGroupMembers, getCachedGroupMembers, invalidateGroupMembers } from './lib/group-dm-members-cache';
  import { createIncomingCallQueue, type IncomingCallEvent } from './lib/incoming-call-queue';
  import { toastStore } from './lib/stores/toast';
  import { get } from 'svelte/store';
  import { classifyOwnerIdentity, type OwnerIdentityState } from './lib/owner-gate';
  import { trapFocus } from './lib/focus-trap';
  import FeedbackModal from './lib/components/FeedbackModal.svelte';
  import AboutModal from './lib/components/AboutModal.svelte';
  import {
    loadMediaPanelOpen,
    saveMediaPanelOpen,
    loadMediaPanelWidth,
    saveMediaPanelWidth,
  } from './lib/media-panel-prefs';

  let innerWidth = $state(window.innerWidth);
  let collapsed = $derived(innerWidth <= 768);
  let showSettings = $state(false);
  // ZEB-545: which Settings tab is active. Held here (not just inside
  // SettingsPanel) so the app can route to a specific section — see
  // handleExportRequested, which opens Settings → Account for the backup wizard.
  let settingsTab = $state<
    'profile' | 'appearance' | 'account' | 'notifications' | 'network' | 'communities' | 'friends'
  >('profile');
  let appMode = $state<AppMode>('messages');

  // ZEB-405 (WS-C): user-controlled reveal + width of the messages-mode media
  // panel. Collapsed by default (opt-in). Bound into <Layout>, persisted here.
  let mediaPanelOpen = $state(loadMediaPanelOpen());
  let mediaPanelWidth = $state(loadMediaPanelWidth());
  $effect(() => { saveMediaPanelOpen(mediaPanelOpen); });
  $effect(() => { saveMediaPanelWidth(mediaPanelWidth); });

  let myProfile = $state(loadProfile());

  // ZEB-341 Task 1: self-first member-card resolution.
  // A single MemberCardService instance for the app lifetime. Task 8 will
  // convert the internal Map to Svelte 5 $state so peer cards re-render;
  // components read through resolveCard() inside $derived so the upgrade
  // is transparent — no snapshot, always a live read.
  const memberCardService = new MemberCardService();
  // ZEB-341 Task 8: reactivity seam. The service stays a plain class with a
  // plain Map; it calls onUpdate() after any poll mutates the card map. We
  // bump this $state counter so the resolveCard() reads inside MemberRow /
  // ChannelMessageFeed $derived re-run and peer names fill in live.
  let cardVersion = $state(0);
  memberCardService.onUpdate = () => {
    cardVersion++;
  };
  // ZEB-537: per-community online presence. Mirrors MemberCardService's
  // optional-adapter + reactivity-seam pattern: constructed early so it's
  // available at boot, the adapter is wired in Tauri-init, and a $state
  // version counter bumped on every presence update re-runs the inline
  // isOnline() resolver read in MemberRow's $derived so the online dot
  // repaints live.
  const presenceService = new PresenceService();
  let presenceVersion = $state(0);
  // ZEB-600: the viewer's own presence visibility ("Appear offline" inverse).
  // Seeded on boot and kept live via the presence-visibility-changed event so
  // the member-list self-dot reflects a toggle flip made in Settings. Defaults
  // to visible; a corrupt settings file fails closed to invisible backend-side.
  let presenceVisible = $state(true);
  // selfOwnerId is the OwnerAddr hex (32 chars) obtained from get_owner_state.
  // Set at startup (after start_node) and kept stable for the session.
  let selfOwnerId = $state<string | null>(null);
  // ZEB-417: one-time migration of legacy localStorage notes to the Rust
  // backend. Runs once the first time selfOwnerId becomes non-null; the
  // per-owner localStorage flag makes it idempotent across restarts.
  let _notesMigratedForOwner = $state<string | null>(null);
  // In-flight guard: prevents the effect from launching a second concurrent
  // import for the same owner. Reset on failure so a later re-trigger retries.
  let _notesMigrationInFlight: string | null = null;
  $effect(() => {
    const owner = selfOwnerId;
    if (!owner || _notesMigratedForOwner === owner || _notesMigrationInFlight === owner) return;
    _notesMigrationInFlight = owner;
    void import('@tauri-apps/api/core')
      .then(({ invoke }) =>
        migrateLocalNotes(owner, invoke as (cmd: string, args: Record<string, unknown>) => Promise<unknown>),
      )
      .then(() => {
        // Only mark migrated on success; idempotency (notes-migrate.ts) makes
        // any retry safe, so leaving the guard unset on failure lets a later
        // re-trigger retry instead of permanently skipping this session.
        _notesMigratedForOwner = owner;
      })
      .catch((e) => {
        console.error('notes migration failed; will retry on a later trigger:', e);
      })
      .finally(() => {
        if (_notesMigrationInFlight === owner) _notesMigrationInFlight = null;
      });
  });

  // ── ZEB-351 Voice V3: app-lifetime voice session ───────────────────
  // Built once after owner identity loads (the get_self_voice_identity IPC
  // supplies the device VK + senderHash the frontend can't derive itself).
  // null until that IPC resolves; CommunityView/VoiceChannelView only mount
  // the join UI behind an `{#if voiceSession}` guard, so the brief pre-ready
  // window degrades gracefully. Threaded down into <CommunityView>.
  let voiceSession = $state<VoiceSession | null>(null);
  // One-shot guard: the owner-present path can run from up to three triggers
  // (boot get_owner_state, zenoh-status reconnect, onMinted), but the session
  // must be built exactly once.
  let voiceSessionInit = false;

  // ── ZEB-352 Voice V4: 1:1 DM calls ────────────────────────────────
  // The app-lifetime singleton CallSession, built alongside voiceSession from
  // the same identity + adapter deps. Drives the global incoming-call toast and
  // the in-call bar. null until buildVoiceSession() resolves.
  let callSession = $state<CallSession | null>(null);
  // ── ZEB-360 Voice (group DM calls): app-lifetime singleton GroupCallSession,
  // built alongside callSession from the same identity + adapter deps. Exposed
  // module-level (sibling to callSession) so T13 can render the group in-call /
  // ring UI; THIS task only wires the session + its remote-event listeners.
  let groupCall = $state<GroupCallSession | null>(null);
  // T13: reactive aliases of the voice/call/group-call state stores so the
  // `$store` syntax auto-subscribes in the script + markup (the group-DM header's
  // busy/active/self model and the in-call bar's group-name lookup read these).
  // Each re-points if its singleton is rebuilt on an identity switch; undefined
  // until buildVoiceSession resolves.
  const groupCallState = $derived(groupCall?.state);
  const voiceState = $derived(voiceSession?.state);
  const callSessionState = $derived(callSession?.state);
  // Incoming-call banner model. Set when an `incoming-call` event lands AND the
  // session actually entered the 'incoming' phase (not busy-auto-declined);
  // cleared whenever the call leaves that phase (accepted / declined / canceled).
  let incomingCall = $state<{ callId: string; spaceId: string; callerName: string; callerAvatarUrl?: string } | null>(null);
  // ZEB-360 T13: incoming GROUP-call banner model, mirroring `incomingCall`. Set
  // when an `incoming-group-call` event lands AND the group session entered the
  // 'incoming' phase (not busy-ignored); cleared when the group phase leaves
  // 'incoming' (the groupCallStateUnsub subscription below). The toast body reads
  // "{caller} is calling {group name}".
  let groupIncomingCall = $state<{ callId: string; spaceId: string; callerName: string; groupName: string; callerAvatarUrl?: string } | null>(null);
  // ZEB-364: buffers for incoming-call signaling that lands during the startup
  // window — after the `incoming-call` / `incoming-group-call` listeners are
  // registered but before buildVoiceSession builds callSession / groupCall.
  // Drained in buildVoiceSession once the sessions exist; without this a ring
  // arriving in that window is silently dropped (callSession?.onIncoming no-ops
  // while null). One buffer per call kind.
  const dmIncomingQueue = createIncomingCallQueue();
  const groupIncomingQueue = createIncomingCallQueue();
  // Unsubscribe for the callSession.state subscription that clears the toast;
  // torn down on unmount via fileManagerService.addUnlisten.
  let callStateUnsub: (() => void) | null = null;
  // ZEB-360: unsubscribe for the groupCall.state subscription that clears the
  // incoming-group-call OS alert when the group phase leaves 'incoming'. Torn
  // down on unmount via fileManagerService.addUnlisten (alongside callStateUnsub).
  let groupCallStateUnsub: (() => void) | null = null;
  // ── ZEB-356: incoming-call OS notification + window attention ──────
  // Built in the Tauri-init IIFE below (real Tauri deps); null in web/dev.
  let incomingCallAlerter: import('./lib/incoming-call-alert').IncomingCallAlerter | null = null;
  // ZEB-662: mention-notification alerter (created in the Tauri-transport IIFE).
  let mentionAlerter: import('./lib/mention-alert').MentionAlertService | null = null;
  // ZEB-665: per-channel unread tracker (created in the Tauri-transport IIFE).
  let channelUnread: import('./lib/channel-unread-service').ChannelUnreadService | null = null;
  // ZEB-666: DM/group-DM unread tracker (created in the Tauri-transport IIFE,
  // BEFORE messageService/navService connect — see the init-order comment there).
  let dmUnread: import('./lib/dm-unread-service').DmUnreadService | null = null;

  // ── ZEB-353 Voice V5: SPA-unmount teardown ────────────────────────
  // Leave any active voice channel / end any in-progress DM call when the
  // component unmounts (hot-reload, SPA teardown). Mirrors the service
  // `$effect(() => () => service.destroy())` cleanup pattern above. The native
  // window-close path is handled separately via onCloseRequested in Tauri-init
  // (Svelte unmount does NOT fire on a native window close), so both paths are
  // covered. Errors are swallowed — teardown must never throw on the way out.
  $effect(() => () => {
    void voiceSession?.leave().catch(() => {});
    void callSession?.end().catch(() => {});
    void groupCall?.leave().catch(() => {});
  });

  /** Fire-and-forget: swallow a (possibly async) result so a click handler never
   *  surfaces an unhandled rejection. Mirrors VoiceChannelView's `swallow`. */
  const swallow = (p: unknown) => { void Promise.resolve(p).catch(() => {}); };

  /** D12 one-active-session coordinator: if a channel voice session is currently
   *  connected, leave it before entering a DM call, so the two media engines
   *  never run at once. */
  async function leaveOtherVoiceThen<T>(fn: () => Promise<T>): Promise<T> {
    if (voiceSession && get(voiceSession.state).phase === 'connected') {
      await voiceSession.leave().catch(() => {});
    }
    // D12: also end an in-progress DM call before starting/answering another, so
    // a DM→DM transition doesn't hit "a call is already in progress". 'incoming'
    // is excluded — the accept path runs in that phase and must NOT tear down
    // the very call it's about to answer.
    if (callSession) {
      const p = get(callSession.state).phase;
      if (p === 'ringingOut' || p === 'connecting' || p === 'active') {
        await callSession.end().catch(() => {});
      }
    }
    // ZEB-360 D6: also tear down an in-progress GROUP call before entering any
    // other voice session, so the group media engine never runs alongside a 1:1
    // call or community voice. 'incoming' is excluded for the same reason as
    // callSession above — the group accept path routes through this helper and
    // must NOT decline the very call it's about to answer. (An 'incoming' group
    // call has no media engine running yet — only a ring toast — so leaving it up
    // does not create two live engines.)
    if (groupCall) {
      const gp = get(groupCall.state).phase;
      if (gp !== 'idle' && gp !== 'incoming') {
        await groupCall.leave().catch(() => {});
      }
    }
    return fn();
  }

  /** ZEB-360 T13: stable Tauri-invoke wrapper threaded into the group-call banner
   *  (its self-contained Join path warms the members cache + calls joinActive).
   *  Rejects outside Tauri so the banner's Join no-ops gracefully. */
  const groupCallInvoke = (cmd: string, args?: Record<string, unknown>): Promise<unknown> =>
    import('@tauri-apps/api/core').then(({ invoke }) => invoke(cmd, args));

  /** ZEB-360 T13: place a new group-DM call. Warms the members cache first (so the
   *  ringing rows render on the first beacon), then drops into media via the
   *  one-engine coordinator. Tauri-only; the dynamic invoke import throws outside
   *  Tauri and is swallowed by the caller. */
  async function placeGroupCall(spaceId: string): Promise<void> {
    if (!groupCall) return;
    const { invoke } = await import('@tauri-apps/api/core');
    await ensureGroupMembers(invoke, spaceId);
    await leaveOtherVoiceThen(() => groupCall!.placeGroupCall(spaceId));
  }

  /** ZEB-360 T13: join the active group-DM call for a space. Reads the live callId
   *  from the banner store, warms the members cache, then joins via the one-engine
   *  coordinator. No-op if the banner entry has gone or we're already in it. */
  async function joinGroupCall(spaceId: string): Promise<void> {
    if (!groupCall) return;
    const entry = get(groupCallBanners)[spaceId];
    if (!entry) return;
    const gs = get(groupCall.state);
    if (gs.callId === entry.callId) return;
    // ZEB-360 (Cursor R6): if we're being RUNG for a *different* concurrent call
    // in this space, dismiss that ring before joining the one the user picked.
    // `leaveOtherVoiceThen` intentionally skips 'incoming' (it's the accept path),
    // so joinActive — which requires 'idle' — would otherwise throw and the Join
    // would silently no-op. Declining is the honest semantic: D6 allows only one
    // engine, so choosing this call means we're not answering the other, and the
    // decline signal lets that caller know. Best-effort; never blocks the join.
    if (gs.phase === 'incoming') {
      await groupCall.decline().catch(() => {});
    }
    const { invoke } = await import('@tauri-apps/api/core');
    await ensureGroupMembers(invoke, spaceId);
    await leaveOtherVoiceThen(() => groupCall!.joinActive(entry.callId, spaceId));
  }

  /** ZEB-351: typed wrapper for the get_self_voice_identity IPC.
   *  Returns the 64-hex device verifying key + the 16-byte sender hash the
   *  voice engine stamps into outbound packet headers. */
  async function getSelfVoiceIdentity(
    invoke: (cmd: string, args?: Record<string, unknown>) => Promise<unknown>,
  ): Promise<{ deviceVkHex: string; senderHash: number[] }> {
    return invoke('get_self_voice_identity') as Promise<{ deviceVkHex: string; senderHash: number[] }>;
  }

  /** ZEB-351: build the singleton voice session once owner identity is present.
   *  Idempotent (one-shot guard); fetches device identity via the new IPC, then
   *  wires identity + the Tauri adapter + member-card resolution into the session. */
  // ZEB-357: author the caller-side call-event DM message. Single writer —
  // only the caller's CallSession fires this, once per call at its terminal
  // transition. Mirrors handleSend's optimistic DM path so the system line
  // appears in the caller's own thread immediately; the callee receives it
  // like any DM (deposit + tunnel), which is what surfaces a missed call to
  // an offline callee on next boot (call signaling itself is live-only).
  // PR #494 R1 (CodeRabbit): in-flight call-event writes, drained by the
  // bounded quit teardown below — a detached send racing quit_app must not
  // lose the only durable record of a call. Entries self-remove on settle.
  const pendingCallRecordWrites = new Set<Promise<unknown>>();

  function recordCallOutcome(spaceId: string, payload: CallEventPayload): void {
    const optimisticId = crypto.randomUUID();
    const optimistic: Message = {
      id: optimisticId,
      sender: { address: 'self', displayName: 'You' },
      text: describeCallEvent(payload, 'author'),
      timestamp: Date.now(),
      media: [],
      priority: 'standard',
      channel: spaceId,
      hub: '',
      deliveryState: 'sending',
      callEvent: payload,
    };
    messageService.pushOptimistic(optimistic);
    const write = (async () => {
      try {
        const { invoke } = await import('@tauri-apps/api/core');
        const result = (await invoke('send_dm', {
          spaceId,
          content: Array.from(new TextEncoder().encode(encodeCallEvent(payload))),
          mimeType: CALL_EVENT_MIME,
        })) as { messageId: string; messageCid: string };
        messageService.replaceOptimisticId(optimisticId, result.messageCid, result.messageId);
      } catch (e) {
        // Keep the line visible in 'failed' state — losing it would silently
        // drop the only durable record of the call. (Qodo R1: log the derived
        // message, not the raw caught value.)
        const msg = e instanceof Error ? e.message : String(e);
        messageService.markFailed(optimisticId, msg);
        console.error('call-event record failed:', msg);
      }
    })();
    pendingCallRecordWrites.add(write);
    void write.finally(() => pendingCallRecordWrites.delete(write));
  }

  async function buildVoiceSession(
    invoke: (cmd: string, args?: Record<string, unknown>) => Promise<unknown>,
    listen: (event: string, handler: (e: { payload: unknown }) => void) => Promise<() => void>,
    selfOwnerHex: string,
  ): Promise<void> {
    if (voiceSessionInit) return;
    voiceSessionInit = true;
    try {
      const { deviceVkHex, senderHash } = await getSelfVoiceIdentity(invoke);
      voiceSession = getVoiceSession({
        invoke,
        listen,
        selfOwnerHex,
        selfDeviceHex: deviceVkHex,
        senderHash: new Uint8Array(senderHash),
        resolveCard: (ownerHex: string) => {
          const card = resolveCard(ownerHex);
          if (!card) return undefined;
          return {
            displayName: card.displayName,
            ...(card.avatarUrl ? { avatarUrl: card.avatarUrl } : {}),
          };
        },
        onRosterOwners: (ownerHexes: string[]) => {
          void memberCardService.subscribeVisible(ownerHexes);
        },
      });
      // ZEB-352: build the DM-call session from the same identity + adapter deps
      // (getCallSession is a singleton, so reconnect re-runs are no-ops). The
      // remote-event listeners are wired in the Tauri-init block once this is up.
      callSession = getCallSession({
        invoke,
        listen,
        selfOwnerHex,
        selfDeviceHex: deviceVkHex,
        senderHash: new Uint8Array(senderHash),
        resolveCard: (ownerHex: string) => {
          const card = resolveCard(ownerHex);
          if (!card) return undefined;
          return {
            displayName: card.displayName,
            ...(card.avatarUrl ? { avatarUrl: card.avatarUrl } : {}),
          };
        },
        // ZEB-357: caller-side terminal outcomes author a call-event DM.
        onCallOutcome: recordCallOutcome,
      });
      // ZEB-360: build the group-DM call session from the same identity + adapter
      // deps (getGroupCallSession is a singleton, so reconnect re-runs are no-ops).
      // resolveMembers reads the sync members cache warmed by the group listeners
      // below (await ensureGroupMembers before each forward). Its remote-event
      // listeners are wired in the Tauri-init block once this is up.
      groupCall = getGroupCallSession({
        invoke,
        listen,
        selfOwnerHex,
        selfDeviceHex: deviceVkHex,
        senderHash: new Uint8Array(senderHash),
        resolveCard: (ownerHex: string) => {
          const card = resolveCard(ownerHex);
          if (!card) return undefined;
          return {
            displayName: card.displayName,
            ...(card.avatarUrl ? { avatarUrl: card.avatarUrl } : {}),
          };
        },
        resolveMembers: (spaceId: string) => getCachedGroupMembers(spaceId),
        onRosterOwners: (ownerHexes: string[]) => {
          void memberCardService.subscribeVisible(ownerHexes);
        },
      });
      // Clear the incoming-call banner whenever the call leaves the 'incoming'
      // phase (accepted, declined, canceled, or busy-rejected). Subscribed once;
      // the unsubscribe is registered for unmount cleanup below.
      callStateUnsub?.();
      callStateUnsub = callSession.state.subscribe((s) => {
        if (s.phase !== 'incoming') {
          // ZEB-356: drop the OS escalation when the call leaves 'incoming'
          // (accepted / declined / canceled / timeout). Capture the id before
          // clearing the banner model.
          const id = incomingCall?.callId;
          if (id) void incomingCallAlerter?.clear(id);
          incomingCall = null;
        }
      });
      // ZEB-360: mirror the above for the group session — clear the incoming-
      // group-call OS alert when the group phase leaves 'incoming' (accepted /
      // declined / canceled / timeout). The group banner model (T13) lives
      // elsewhere; here we only drop the OS escalation, keyed by callId. Track the
      // last incoming callId so the clear fires exactly on the transition out.
      groupCallStateUnsub?.();
      let lastGroupIncomingId: string | null = null;
      groupCallStateUnsub = groupCall.state.subscribe((s) => {
        if (s.phase === 'incoming') {
          lastGroupIncomingId = s.callId;
        } else if (lastGroupIncomingId) {
          void incomingCallAlerter?.clear(lastGroupIncomingId);
          lastGroupIncomingId = null;
        }
        // T13: drop the in-app group ring toast whenever the group session leaves
        // 'incoming' (accepted / declined / canceled / timeout).
        if (s.phase !== 'incoming') groupIncomingCall = null;
      });
      // ZEB-364: replay incoming-call events that arrived before the sessions
      // existed (startup race). callSession/groupCall + their banner-clearing
      // subscriptions are now wired, so handleIncoming* behaves exactly as it
      // would for a live event. drain() already drops rings whose call was
      // canceled during the window. Group handlers are awaited sequentially so
      // they replay in FIFO order and a rejection surfaces to the catch below
      // (a bare `void` diverged from the live listener, which awaits).
      for (const p of dmIncomingQueue.drain()) handleIncomingDmCall(p);
      for (const p of groupIncomingQueue.drain()) await handleIncomingGroupCall(invoke, p);
      // ZEB-356: now that owner identity is present (buildVoiceSession only runs
      // then), request notification permission once so an incoming call's banner
      // isn't lost to a permission-prompt race. Deferred here (not app-init) so
      // first-run users aren't prompted before completing onboarding; notify() has
      // a lazy permission fallback regardless. Tauri-only; the import/calls throw
      // and are caught outside Tauri.
      void (async () => {
        try {
          const { isPermissionGranted, requestPermission } = await import('@tauri-apps/plugin-notification');
          if (!(await isPermissionGranted())) await requestPermission();
        } catch { /* non-Tauri / plugin absent — notify()'s lazy fallback covers it */ }
      })();
    } catch (err) {
      // Allow a retry on a later trigger (reconnect) if the IPC wasn't ready.
      voiceSessionInit = false;
      const msg = err instanceof Error ? err.message : String(err);
      console.warn('[harmony-client] get_self_voice_identity not yet available:', msg);
    }
  }

  // ZEB-364: 1:1 incoming-call routing, shared by the live `incoming-call`
  // listener and the startup-queue drain. Callers ensure callSession exists; the
  // `callSession &&` guards keep it correct even if it were torn down mid-replay.
  function handleIncomingDmCall(p: IncomingCallEvent): void {
    callSession?.onIncoming(p.callId, p.callerOwner, p.spaceId);
    // Only raise the banner if the session actually ADOPTED this invite — in
    // 'incoming' AND tracking this exact callId. onIncoming busy-declines a
    // second invite while already ringing (stays on the original call), so
    // without the callId check the banner + OS notification would overwrite to
    // the new caller while accept/decline still act on the original — a
    // shown-vs-acted mismatch (CodeRabbit). Mirrors the group handler's guard;
    // reachable here when the startup drain replays two buffered invites.
    const st = callSession ? get(callSession.state) : null;
    if (st && st.phase === 'incoming' && st.callId === p.callId) {
      const card = resolveCard(p.callerOwner);
      incomingCall = {
        callId: p.callId,
        spaceId: p.spaceId,
        callerName: card?.displayName ?? p.callerOwner.slice(0, 8),
        ...(card?.avatarUrl ? { callerAvatarUrl: card.avatarUrl } : {}),
      };
      // ZEB-356: escalate to the OS if the window is unfocused (no-op if focused).
      void incomingCallAlerter?.notify({
        id: p.callId,
        title: 'Incoming call',
        body: `${incomingCall.callerName} is calling`,
      });
    }
  }

  // ZEB-364: group incoming-call routing, shared by the `incoming-group-call`
  // listener and the startup-queue drain. Takes `invoke` explicitly because it
  // warms the group-member cache — the live listener passes its IIFE-scoped
  // adapter, the drain passes buildVoiceSession's.
  async function handleIncomingGroupCall(
    invoke: (cmd: string, args?: Record<string, unknown>) => Promise<unknown>,
    p: IncomingCallEvent,
  ): Promise<void> {
    // Best-effort cache warm: a transient get_group_dm_members failure must NOT
    // abort the handler and drop the ring (roster degrades to live beacons only).
    await ensureGroupMembers(invoke, p.spaceId).catch(() => {});
    groupCall?.onIncomingGroup(p.callId, p.callerOwner, p.spaceId);
    // Only escalate if the session actually ADOPTED this invite — in 'incoming'
    // AND tracking this exact callId (onIncomingGroup no-ops when not idle, so a
    // second invite while ringing leaves the session on the original call).
    const gst = groupCall ? get(groupCall.state) : null;
    if (gst && gst.phase === 'incoming' && gst.callId === p.callId) {
      const card = resolveCard(p.callerOwner);
      const name = card?.displayName ?? p.callerOwner.slice(0, 8);
      const groupName = navService.nodes.find((n) => n.id === p.spaceId)?.name ?? 'a group';
      // T13: raise the in-app ring toast (cleared when the group phase leaves
      // 'incoming' via the subscription in buildVoiceSession).
      groupIncomingCall = {
        callId: p.callId,
        spaceId: p.spaceId,
        callerName: name,
        groupName,
        ...(card?.avatarUrl ? { callerAvatarUrl: card.avatarUrl } : {}),
      };
      void incomingCallAlerter?.notify({
        id: p.callId,
        title: 'Incoming group call',
        body: `${name} is calling ${groupName}`,
      });
    }
  }

  // Expose a resolver function that components call inside $derived.
  // Reading cardVersion registers the reactive dependency so consumers
  // re-run when a peer card arrives (Task 8).
  function resolveCard(ownerIdHex: string) {
    cardVersion; // reactive dep: re-run derived consumers when cards change
    return memberCardService.resolve(ownerIdHex);
  }

  // ZEB-432: local friend nickname for an owner_id, preferred over the broadcast
  // profile-card name in community surfaces (members roster + message authors),
  // matching the Friends panel ladder. Reads the reactive `friendNicknames`
  // $state so a nickname edit (friend-list-changed → refreshDmContacts) repaints
  // the open community view. Case-normalized: community member addresses and
  // friend ownerIdHex are both lowercase owner_id, but normalize defensively.
  function resolveNickname(ownerIdHex: string): string | undefined {
    return friendNicknames.get(ownerIdHex.toLowerCase()) || undefined;
  }

  // ZEB-537: online-presence resolver threaded into the members roster, mirroring
  // resolveCard. Reading presenceVersion registers the reactive dependency so
  // consumers re-run when a presence-updated event mutates the service's map.
  function isOnline(ownerIdHex: string): boolean {
    presenceVersion; // reactive dep: re-run derived consumers when presence changes
    return presenceService.isOnline(ownerIdHex);
  }

  // ZEB-341 Task 8: lifecycle hooks threaded down to CommunityMembersPanel,
  // which knows the visible-member set. No-ops until the adapter wires up.
  function subscribeVisibleCards(ownerIdHexes: string[]) {
    void memberCardService.subscribeVisible(ownerIdHexes);
  }
  function unsubscribeCards() {
    void memberCardService.unsubscribeAll();
  }

  // ZEB-341: one-shot guard so the boot-time card re-publish fires at most
  // once per process (the 600s refresh re-emits cached bytes thereafter).
  let hasPublishedCardOnBoot = false;
  // ZEB-341: in-flight promise for the boot card publish. The boot path fires
  // tryBootPublishCard un-awaited while the zenoh-status 'connected' handler
  // awaits it, so two calls can be in flight at once. `hasPublishedCardOnBoot`
  // is a plain boolean checked before an await, so without joining the in-flight
  // attempt both calls would pass the check before either sets the guard and
  // publish a duplicate card (async TOCTOU). Concurrent callers join this promise
  // instead of starting a second publish; it clears once the attempt settles.
  let bootCardPublishInFlight: Promise<void> | null = null;

  // Publish the profile (and, backend-side, the owner_id card) to the network.
  // Uses direct invoke rather than ZenohService.publishProfile() because
  // ZenohService lives in NetworkApp (not accessible here). Both paths invoke
  // the same 'publish_profile' command. Swallows errors: not-in-Tauri /
  // not-connected leaves the profile saved locally only, and the backend
  // already best-effort-skips the card publish when the node isn't wired.
  // Returns true if the IPC actually landed (session connected). A successful
  // invoke means the Reticulum publish committed, which implies the node is
  // wired enough for the backend's best-effort card publish to have run too.
  async function publishProfileToNetwork(profile: Profile): Promise<boolean> {
    try {
      const { invoke } = await import('@tauri-apps/api/core');
      await invoke('publish_profile', {
        profile: {
          address: profile.address,
          displayName: profile.displayName,
          statusText: profile.statusText,
          avatarUrl: profile.avatarUrl,
          avatarCid: profile.avatarCid,
          // ZEB-345: carry the long-form profile-page root CID (hex). Unlike
          // avatarUrl it needs no blob: sanitization — it's a content CID, not
          // a session-local object URL — so it flows to peers verbatim.
          profilePageRoot: profile.profilePageRoot,
        },
      });
      return true;
    } catch {
      // Not in Tauri or not connected — profile saved locally only.
      return false;
    }
  }

  // ZEB-341: re-publish ONLY the owner_id card (not the full Reticulum profile)
  // via the dedicated card-only IPC. Returns true if it landed (node ready).
  async function republishOwnerCard(profile: Profile): Promise<boolean> {
    try {
      const { invoke } = await import('@tauri-apps/api/core');
      await invoke('republish_owner_card', {
        displayName: profile.displayName,
        statusText: profile.statusText ?? '',
        avatarCid: profile.avatarCid ?? null,
        // ZEB-345: card-only republish carries the profile-page root CID hex.
        profilePageRoot: profile.profilePageRoot ?? null,
      });
      return true;
    } catch {
      return false; // node not ready — caller retries on zenoh-status connect
    }
  }

  // ZEB-341: attempt the one-shot boot card re-publish, but only mark it done on
  // a SUCCESSFUL publish. The publish can race Zenoh session startup (start_node
  // may return before the session connects); a failed early attempt leaves the
  // guard unset so a later trigger (the zenoh-status 'connected' handler)
  // retries. Without this, the guard would burn on a no-op and a returning
  // user's card would never publish (the 600s refresh only re-emits cached
  // bytes, which stay empty until the card publish succeeds at least once).
  // Publishes the CARD ONLY — not the full profile — so boot doesn't re-emit
  // the unchanged Reticulum profile.
  function tryBootPublishCard(): Promise<void> {
    if (hasPublishedCardOnBoot || selfOwnerId === null || !myProfile?.displayName) {
      return Promise.resolve();
    }
    // Join an already-running attempt rather than starting a second publish.
    // The check-decide-assign below is synchronous (no await between the guard
    // read and the assignment), so it's atomic w.r.t. concurrent callers; the
    // first await happens only inside the memoized closure.
    if (bootCardPublishInFlight) {
      return bootCardPublishInFlight;
    }
    bootCardPublishInFlight = (async () => {
      try {
        if (await republishOwnerCard(myProfile)) {
          hasPublishedCardOnBoot = true;
        }
      } finally {
        // Clear so a FAILED attempt (guard still false) can retry on the next
        // trigger; a SUCCEEDED attempt is short-circuited by the guard above.
        bootCardPublishInFlight = null;
      }
    })();
    return bootCardPublishInFlight;
  }

  async function handleProfileSave(profile: Profile) {
    // Strip a `blob:`-scheme avatarUrl before it leaves this session — for BOTH
    // durable persistence AND the network publish. Blob URLs are session-local:
    // dead after a reload, and unusable by peers. The avatar re-resolves from
    // `avatarCid` (via the AvatarResolver) on reload and on peers. Only the
    // in-session `myProfile`/preview keeps the blob URL so the live self-preview
    // stays instant.
    const sanitized: Profile = profile.avatarUrl?.startsWith('blob:')
      ? { ...profile, avatarUrl: undefined }
      : profile;
    saveProfile(sanitized, selfOwnerId ?? undefined);
    myProfile = profile;
    // Re-seed the card whenever the profile is saved so the name updates
    // immediately without a network round-trip (self-first, ZEB-341 Task 1).
    if (selfOwnerId !== null) {
      memberCardService.seedSelf(selfOwnerId, {
        displayName: profile.displayName,
        statusText: profile.statusText ?? '',
        avatarUrl: profile.avatarUrl,
        avatarCid: profile.avatarCid,
        // ZEB-345: seed the self profile-page root so the owner's own panel
        // resolves locally without a network round-trip (mirrors avatarCid).
        profilePageRoot: profile.profilePageRoot,
      });
    }
    // messageService.ownDisplayName / vineService.ownDisplayName are kept
    // in sync by a `$effect` later in the script (single source of truth).
    // Publish to network if Tauri is available — use the sanitized profile so a
    // session-local blob: avatarUrl is never sent to peers (they resolve the
    // avatar from avatarCid).
    await publishProfileToNetwork(sanitized);
  }

  // ZEB-336: persist the first-run name through the normal profile-save path
  // (saves locally, re-seeds the self card, publishes to the network), then
  // close the prompt.
  async function handleNamePromptSave(name: string): Promise<void> {
    // Close the prompt even if the network publish inside handleProfileSave
    // rejects — the local profile save already persisted the name, so the modal
    // must never get stuck open behind a swallowed rejection. (Greptile PR #180.)
    try {
      await handleProfileSave({ ...myProfile, displayName: name });
    } finally {
      showNamePrompt = false;
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

  /**
   * ZEB-559: ingest a vine video via the backend-owned native picker (the
   * backend opens the dialog and returns the CID + filename, so the renderer
   * never supplies a filesystem path). Returns null if the user cancels; throws
   * on ingest failure so VinePublishDialog can surface the message.
   */
  function handlePickVineVideo(): Promise<{ cid: string; fileName: string } | null> {
    return vineService.ingestVideo();
  }

  async function handleVineReshare(vine: import('./lib/types').VineVideo) {
    // Self-reshare prevention (spec §Edge Cases → Self-reshare prevention):
    // silently no-op when the SOURCE vine is our OWN ORIGINAL
    // (creatorAddress identifies us AND `reshareOf` is unset). Resharing
    // someone else's reshare of our content is explicitly allowed — the
    // resolved `originalCreatorAddress` in that case still maps to us,
    // which is why this check must live at the caller (with full access
    // to the source vine's identity), not inside `vineService.publish()`.
    const isOwn = vine.creatorAddress === 'self'
      || (vineService.ownAddress != null && vine.creatorAddress === vineService.ownAddress);
    if (isOwn && !vine.reshareOf) {
      return;
    }
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
  // ── ZEB-328: in-app update notification ────────────────────────────
  let availableUpdate = $state<Update | null>(null);
  // ── ZEB-331: first-run welcome + feedback + about ─────────────────
  let showWelcomeModal = $state(false);
  // ZEB-336: first-run display-name prompt, shown after onboarding when the
  // profile name is still the "Anonymous" default.
  let showNamePrompt = $state(false);
  // ZEB-338 / PR #169: backend-authoritative owner-identity gate. Four states
  // (see owner-gate.ts) so a start_node *failure* is never mistaken for "no
  // owner identity" — that mistake trapped returning users in the mint gate.
  // 'unknown' until start_node resolves; 'present' after a successful mint.
  let ownerIdentityState = $state<OwnerIdentityState>('unknown');
  // ZEB-338 / PR #169: message from a failed start_node, shown in the startup
  // error overlay (the non-mint escape from the 'error' state).
  let startNodeError = $state<string | null>(null);
  // ZEB-338 / PR #169: bound to the startup-error dialog for its focus trap.
  let startupErrorModalEl = $state<HTMLElement | null>(null);
  // ZEB-668 S2: bound to the device-revoked terminal dialog for its focus trap.
  let revokedModalEl = $state<HTMLElement | null>(null);

  // ZEB-338 / PR #169: the startup-error overlay is a blocking dialog, so trap
  // focus inside it (same util as WelcomeModal) — keyboard users must not be
  // able to tab into background app controls behind it.
  $effect(() => {
    if (ownerIdentityState !== 'error' || startupErrorModalEl === null) return;
    return trapFocus(startupErrorModalEl);
  });
  // ZEB-668 S2: same treatment for the terminal "removed" overlay.
  $effect(() => {
    if (ownerIdentityState !== 'revoked' || revokedModalEl === null) return;
    return trapFocus(revokedModalEl);
  });
  let feedbackModalOpen = $state(false);
  let aboutModalOpen = $state(false);

  // ZEB-338: route an incoming harmony:// invite. Pre-mint (no owner identity)
  // → queue for the post-mint drain. Post-mint → open the redeem dialog.
  function routeInviteUrl(url: string): void {
    if (ownerIdentityState !== 'present') {
      // No loaded owner identity yet (first run, boot-in-flight, or a startup
      // error) → queue for the post-mint / post-recovery drain.
      queueInviteForPostMint(url);
      return;
    }
    redeemUrl = url;
    redeemError = null;
    showRedeemInvite = true;
  }

  // ZEB-345 Task 11: dispatch a `harmony:` link clicked inside ProfilePanel
  // through the same in-app deep-link routing the OS handoff uses (ZEB-338).
  // The panel already enforced the scheme allowlist and preventDefault'd the
  // raw navigation, so this only ever receives a `harmony:` url. Today the only
  // in-app harmony route is the invite flow; an invite url goes through
  // routeInviteUrl (queue pre-mint / redeem dialog post-mint), matching the OS
  // deep-link path. Other harmony: links have no in-app destination yet, so they
  // are intentionally a no-op (logged) rather than an external open — never a
  // raw navigation. When a general harmony: route lands, extend the dispatch
  // here.
  function routeHarmonyLink(url: string): void {
    const invite = extractHarmonyInviteUrl([url]);
    if (invite) {
      routeInviteUrl(invite);
      return;
    }
    console.warn(`[harmony-client] no in-app route for harmony link: ${url}`);
  }

  // ZEB-338: drain a queued invite into the redeem dialog (called post-mint and
  // post-boot-when-owner-already-present).
  function drainQueuedInvite(): void {
    const queued = consumeQueuedInvite();
    if (queued !== null) {
      redeemUrl = queued;
      redeemError = null;
      showRedeemInvite = true;
    }
  }

  // ZEB-338: WelcomeModal hard-gate completion. Flip owner-present, close the
  // gate, and drain any invite that was queued pre-mint (Flow 3).
  // eslint-disable-next-line @typescript-eslint/no-unused-vars
  async function onMinted(result: MintIpcResult): Promise<void> {
    ownerIdentityState = 'present';
    startNodeError = null;
    showWelcomeModal = false;
    // ZEB-341 Task 1: seed the card using the newly-minted owner identity.
    // MintIpcResult.state.ownerId is available from the mint response.
    selfOwnerId = result.state.ownerId;
    connectOwnerTheme(result.state.ownerId);
    // ZEB-586: re-load the profile under the freshly-minted owner so a prior
    // identity's cached name can't seed/broadcast from this one. A fresh owner
    // has no owner-scoped entry → 'Anonymous', which the name prompt below picks
    // up. (loadProfile is owner-scoped; the global key is no longer read.)
    myProfile = loadProfile(result.state.ownerId);
    memberCardService.seedSelf(result.state.ownerId, {
      displayName: myProfile.displayName,
      statusText: myProfile.statusText ?? '',
      avatarUrl: myProfile.avatarUrl,
      avatarCid: myProfile.avatarCid,
      profilePageRoot: myProfile.profilePageRoot,
    });
    // ZEB-351: build the voice session for a freshly-minted owner. The adapter
    // is wired up by the Tauri-init IIFE; if it hasn't landed yet (mint can
    // race init), the zenoh-status reconnect handler retries via the same guard.
    if (tauriAdapter) {
      void buildVoiceSession(tauriAdapter.invoke, tauriAdapter.listen, result.state.ownerId);
    }
    drainQueuedInvite();
    // ZEB-336: a freshly-minted owner has no name yet — prompt for one, but only
    // when a queued invite didn't just open the redeem dialog, so the two
    // first-run modals don't stack. (Cursor PR #180.) The name prompt is
    // skippable and editable later in Settings, so deferring it here is fine.
    if (!showRedeemInvite && (!myProfile.displayName || myProfile.displayName === 'Anonymous')) {
      showNamePrompt = true;
    }
  }

  let showCreateCommunity = $state(false);
  let showRedeemInvite = $state(false);
  let createPending = $state(false);
  let createError = $state<string | null>(null);
  let redeemPending = $state(false);
  let redeemError = $state<string | null>(null);
  let redeemUrl = $state('');
  /** ZEB-254: transient status message shown after a successful redeem.
   *  Empty string = no message displayed. Auto-cleared after 6 s. */
  let redeemStatusMsg = $state('');
  /** Timer ID for auto-clearing redeemStatusMsg. Tracked so back-to-back
   *  redeems cancel the previous timer before arming a new one. */
  let redeemStatusTimer: ReturnType<typeof setTimeout> | null = null;

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
  // ZEB-298 PR 2 Task 10 — singleton VotingAdapter shared by the
  // delegate-on-behalf toast wiring and any voting-UI components that
  // accept the `votingAdapter` prop (CommunityView et al.). Constructed
  // up-front so module-scope wiring code can reference the same
  // instance regardless of Tauri-init ordering; `connectAdapter` runs
  // exactly once below, after the TauriAdapter is established.
  const votingAdapter = new VotingAdapter();
  // ZEB-606: nav proposals-row badge counts. onChange bumps the $state
  // mirror so the NavPanel resolver re-reads (presenceVersion idiom).
  const proposalCountService = new ProposalCountService();
  let proposalCountVersion = $state(0);
  // ZEB-606 (PR #408 Qodo R1): true once votingAdapter.connectAdapter has
  // resolved — until then VotingAdapter rejects every invoke, so voting-backed
  // UI (Assembly tab) must not mount and the badge ensure() pass must re-run
  // on the flip. On connect failure this stays false and the rail degrades to
  // media-only instead of a permanent error state.
  let votingReady = $state(false);
  // ZEB-606: App-held mirror of CommunityView's middle-column view, bound
  // via bind:activeView so the nav proposals row / Assembly rail can
  // deep-link and show an active state.
  let communityActiveView = $state<'channels' | 'proposals' | 'tier3' | 'charter'>('channels');
  // ZEB-606: identity-chip signals. Ring = "you appear online to others"
  // (visibility toggle + identity present); presence rosters never contain
  // self (zenoh doesn't loop our own beacon), so presenceService can't
  // answer this. Microline = self-sovereign identity minted and loaded.
  let identityChipInfo = $derived({
    displayName: myProfile?.displayName ?? '',
    ownerIdHex: selfOwnerId,
    selfOnline: presenceVisible && ownerIdentityState === 'present',
    selfSovereign: ownerIdentityState === 'present',
  });
  // Captured unsubscribe handle from setupDelegateOnBehalfToast. Stored
  // so any future re-init / remount path can tear down the prior toast
  // subscription before registering a new one (preventing duplicate
  // toasts on a single delegate signal). Today connectAdapter runs once
  // at boot; this is future-proofing per CodeRabbit R1.
  let toastUnsubscribe: (() => void) | null = null;
  // Local mirror of per-community shared_in_profile state. The backend
  // is the source of truth; we hydrate this Map at startup via
  // `profileBroadcastService.listSharedSet()` (see ZEB-281 Sub-D Phase 4
  // R1 — required so the toggle reflects server state after restart and
  // the UI never claims "off / private" while the publisher broadcasts
  // "on / public"). The settings panel rolls back on toggle failure.
  let sharedInProfileByCommunity = $state<Map<string, boolean>>(new Map());
  let selectedCommunityId = $state<string | null>(null);
  // ZEB-663: the reactive selected channel for the currently-selected
  // community (single source of truth for channel selection; persistence
  // mirrors into communityService.setSelectedChannel). Null = none resolved
  // yet (the resolution effect below picks a default from the nav children).
  let selectedChannelId = $state<string | null>(null);
  // ZEB-663: hoisted channel-management dialog state (was CommunityView's).
  // Scoped to the selected community; power-gated by myCommunityPower.
  let showCreateChannelDialog = $state(false);
  // ZEB-663: rename/delete targets capture the community they were opened for,
  // so the op binds to the channel's OWN community — not the mutable
  // `selectedCommunityId`, which can change (user switches communities) while
  // the dialog is open and would otherwise send the op to the wrong community.
  let modifyChannelTarget = $state<{
    channel: import('./lib/community-service').ChannelInfo;
    communityId: string;
  } | null>(null);
  let deleteChannelTarget = $state<{
    channel: import('./lib/community-service').ChannelInfo;
    communityId: string;
  } | null>(null);
  let communityMembers = $state<CommunityMember[]>([]);
  // ZEB-553 item 11: true while an *initial* roster load is in flight after a
  // community switch (i.e. the roster is still empty). Owned entirely by
  // refreshCommunityMembers — set when it starts against an empty roster,
  // cleared in its finally — so it can never get stuck true and background
  // refreshes (message throttle, reconnect) never flash a loading state.
  // Forwarded into ChannelMembersPanel via CommunityView so a switch shows
  // "Loading members…" instead of a bare, misleading "0".
  let membersLoading = $state(false);
  // ZEB-404: timestamp throttle for the message-triggered roster refetch (see
  // the channelMessageService.onMessage wiring). Time-based — a failed or
  // too-early refresh self-heals on the next message rather than permanently
  // suppressing an author. Reset on community switch so each community starts
  // fresh.
  let lastMessageRosterRefetchAt = 0;
  let myAddress = $state('');
  // Local mirror of communityService.isDegraded(selectedCommunityId).
  // Direct method calls in the template aren't reactive — we need a
  // $state field that the listener can update so the settings panel
  // re-renders on degraded events.
  let isCurrentCommunityDegraded = $state(false);
  // Derived from the live roster — recomputes when fetchOwnAddress
  // resolves later than the first roster load (race fixed in PR #91
  // review). Never assign to this directly.
  //
  // ZEB-396: the roster is owner_id-keyed; self-power must match selfOwnerId
  // (owner_id), NOT myAddress (the node/transport address from get_node_addr).
  // The $derived recomputes when selfOwnerId resolves after start_node.
  let myCommunityPower = $derived(selfCommunityPower(communityMembers, selfOwnerId));
  // ZEB-663: resolve which channel is selected for the current community when
  // the current selection isn't a live channel of it (community switch,
  // first visit, or the active channel was just deleted). Restores the
  // persisted last-viewed channel, else #general, else the first channel.
  // Only runs on the Channels view so governance views don't force a channel
  // selection. Gated on populated nav children so the just-joined async
  // window resolves once ChannelNavSync fills them in.
  $effect(() => {
    const cid = selectedCommunityId;
    if (!cid || communityActiveView !== 'channels') return;
    const kids = navNodes.filter((n) => n.parentId === cid && n.type === 'channel');
    if (kids.length === 0) return; // not populated yet
    const target = resolveChannelSelection(
      kids.map((k) => ({ id: k.id, name: k.name })),
      selectedChannelId,
      communityService.getSelectedChannel(cid),
    );
    if (target === null || target === selectedChannelId) return;
    selectedChannelId = target;
    communityService.setSelectedChannel(cid, target);
    navService.clearMention(target); // landing on it marks its mentions seen
    channelUnread?.markChannelRead(cid, target); // ZEB-665: …and its unread read
  });
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
      // ZEB-606: a community switch always lands on Channels unless a
      // deep-link (openCommunityProposals) overrides it afterwards.
      communityActiveView = 'channels';
      // ZEB-663: drop the prior community's selected channel so the active-row
      // highlight falls back to the community row (not a stale cross-community
      // channel) until the resolution effect re-picks this community's channel.
      // openCommunityChannel sets the concrete channel right after this call.
      selectedChannelId = null;
      communityMembers = [];
      // ZEB-404: new community session → reset the refetch throttle.
      lastMessageRosterRefetchAt = 0;
      // ZEB-600: subscribe-all model — every joined community stays subscribed
      // for the session (the sidebar/DM dots need non-active rosters), so a
      // switch only re-points isOnline() at the newly-selected community. If it
      // isn't subscribed yet (selected before boot subscribe-all, or a
      // just-joined community), subscribe it now. onUpdate bumps presenceVersion
      // → App re-renders → resolvers re-read. Fire-and-forget; no-ops before the
      // adapter is wired.
      if (id != null) {
        if (presenceService.isSubscribed(id)) {
          presenceService.setActive(id);
        } else {
          void presenceService
            .subscribe(id, () => {
              presenceVersion++;
            })
            .catch((e) =>
              console.error('presence subscribe failed:', e instanceof Error ? e.message : String(e)),
            );
        }
      }
    }
    // ZEB-334 (Cursor PR #180): selecting a real community leaves the Notes
    // space, so the nav highlights the community and the feed shows it. Passing
    // null (e.g. from selectNotes/leave) does NOT touch notesSelected — those
    // callers manage it themselves.
    if (id !== null) notesSelected = false;
    selectedCommunityId = id;
    isCurrentCommunityDegraded = id != null ? communityService.isDegraded(id) : false;
    // ZEB-663: ensure this community's channels are populated in the nav tree
    // (covers communities joined after boot, which start()'s snapshot missed).
    // Idempotent + cache-hit for already-synced communities.
    if (id != null) void channelNavSync.resync(id);
  }

  /** ZEB-606: nav proposals-row / Assembly-rail deep link — select the
   *  community and land on its Proposals view. */
  function openCommunityProposals(communityId: string) {
    if (appMode !== 'messages') switchMode('messages');
    changeSelectedCommunity(communityId);
    void refreshCommunityMembers(communityId);
    showSettings = false;
    communityActiveView = 'proposals';
  }

  /** ZEB-663: nav channel-row click — select the community + channel and land
   *  on its Channels feed. Mirrors openCommunityProposals. */
  function openCommunityChannel(communityId: string, channelId: string) {
    if (appMode !== 'messages') switchMode('messages');
    changeSelectedCommunity(communityId);
    void refreshCommunityMembers(communityId);
    showSettings = false;
    communityService.setSelectedChannel(communityId, channelId);
    selectedChannelId = channelId;
    communityActiveView = 'channels';
    // ZEB-663: viewing a channel clears its unseen-mention indicator.
    navService.clearMention(channelId);
    // ZEB-665: open-clears-all — stamp the read cursor + zero the badge.
    channelUnread?.markChannelRead(communityId, channelId);
  }

  // ZEB-663: nav channel management — gated to the selected community at power
  // ≥ kick (App resolves power for the selected community only).
  function canManageSelectedCommunityChannels(communityId: string): boolean {
    return communityId === selectedCommunityId && myCommunityPower >= POWER_THRESHOLDS.kick;
  }

  // Wired as fire-and-forget nav-row callbacks (onRenameChannel/onDeleteChannel
  // aren't awaited), so an unguarded listChannels rejection would surface as an
  // unhandled promise rejection and the menu action would silently do nothing.
  async function openRenameChannel(communityId: string, channelId: string) {
    try {
      const list = await communityService.listChannels(communityId);
      const ch = list.find((c) => c.channelId === channelId);
      if (ch) modifyChannelTarget = { channel: ch, communityId };
    } catch (e) {
      console.warn('openRenameChannel: listChannels failed:', e instanceof Error ? e.message : String(e));
    }
  }

  async function openDeleteChannel(communityId: string, channelId: string) {
    try {
      const list = await communityService.listChannels(communityId);
      const ch = list.find((c) => c.channelId === channelId);
      if (ch) deleteChannelTarget = { channel: ch, communityId };
    } catch (e) {
      console.warn('openDeleteChannel: listChannels failed:', e instanceof Error ? e.message : String(e));
    }
  }

  async function confirmDeleteChannel() {
    const target = deleteChannelTarget;
    deleteChannelTarget = null;
    if (!target) return;
    try {
      // Bind to the captured community, not the mutable selectedCommunityId.
      await communityService.deleteChannel(target.communityId, target.channel.channelId);
      // The channel-config-updated event drives the nav reconcile + the App
      // resolution effect's fallback re-select if the active channel went away.
    } catch (e) {
      console.warn('deleteChannel failed:', e instanceof Error ? e.message : String(e));
    }
  }

  async function refreshCommunityMembers(id: string) {
    // ZEB-553 item 11: only an initial load (empty roster) drives the loading
    // affordance — a background refresh keeps the existing roster on screen and
    // must not flash "Loading members…". changeSelectedCommunity clears the
    // roster synchronously on a real switch, so this reads empty there.
    if (communityMembers.length === 0) membersLoading = true;
    try {
      // ZEB-404: force-bypass the per-community member cache — an explicit
      // refresh must return ground truth (the cache is invalidated only by the
      // `community-members-changed` event, the very signal a missed-delta
      // refresh compensates for). Concurrent same-community refreshes are
      // coalesced inside CommunityService (single-flight), so overlapping
      // triggers (message throttle, reconnect, community-open) can't race the
      // cache or the roster.
      const fresh = await communityService.listCommunityMembers(id, true);
      // Drop if the user switched communities while we were awaiting.
      if (selectedCommunityId !== id) return;
      communityMembers = fresh;
    } catch (e) {
      // listCommunityMembers throws when the adapter isn't connected
      // (mock-data mode) or the backend isn't ready. Surface the failure to
      // the console and keep the last known-good roster rather than wiping it
      // on a transient failure — the throttle/reconnect paths will retry.
      const msg = e instanceof Error ? e.message : String(e);
      console.warn('[harmony-client] listCommunityMembers failed:', msg);
    } finally {
      // ZEB-553 item 11 (Qodo PR #332): clear the loading flag when this fetch
      // is still relevant — same community, or none selected (user left to
      // Notes/DMs mid-fetch). A fetch superseded by a switch to a *different*
      // community must not clear it; that community's own refresh owns its
      // loading state. Without the null case the flag could stick true after a
      // mid-fetch leave until the next community open reset it.
      if (shouldClearMembersLoading(selectedCommunityId, id)) membersLoading = false;
    }
  }

  // ZEB-431: single entry point for the DM-create modal. The refresh
  // re-pulls the friend graph at open time so a friend added moments
  // earlier (or on another device, missed-event case) is listed without
  // waiting for a friend-list-changed emit.
  function openDmCreate(kind: 'dm' | 'group-dm') {
    dmCreateInitialKind = kind;
    dmCreateDialogOpen = true;
    void refreshDmContacts();
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

  // ── ZEB-345 Task 10: long-form profile-page resolver ───────────────
  // Lazy DTO resolver (twin of AvatarResolver, but NOT eagerly per-member —
  // resolve() only fires when a panel opens). connectAdapter runs in the
  // Tauri-init IIFE alongside avatarResolver.connectAdapter. The open panel
  // re-renders via profileDocVersion (bumped in onChange below) — a separate
  // counter from avatarResolver's nav/card refresh path so the two never
  // clobber each other's onChange.
  const profilePageResolver = new ProfilePageResolver();
  $effect(() => () => profilePageResolver.destroy());
  let profileDocVersion = $state(0);
  profilePageResolver.onChange = () => {
    profileDocVersion++;
  };
  // Owner_id (hex) whose long-form profile panel is open, or null. Set from
  // the owner-card popover's "View full profile" action (onViewProfile).
  let openProfileOwnerId = $state<string | null>(null);

  const navService = new NavService();
  navService.setAvatarResolver(avatarResolver);
  $effect(() => () => navService.destroy());

  // ── Community service (ZEB-263) ────────────────────────────────────
  // Mirrors the MessageService / NavService pattern: constructed eagerly,
  // adapter wired up inside the Tauri-init IIFE below, destroy() ran on
  // unmount via $effect cleanup.
  const communityService = new CommunityService();
  $effect(() => () => communityService.destroy());
  // ── ZEB-663: channel→nav bridge ────────────────────────────────────
  // Populates each community's channel children as first-class NavNodes so
  // the unified nav tree renders channels inline. Pure dep-injection — no
  // direct service refs beyond the three lambdas below.
  const channelNavSync = new ChannelNavSyncService({
    listChannels: (id) => communityService.listChannels(id),
    setChannels: (id, channels) => {
      navService.setChannels(id, channels);
      // ZEB-665: channels just materialized/re-synced — seed unseen channels'
      // unread state and re-push known counts onto the (possibly rebuilt) nodes.
      void channelUnread?.onChannelsMaterialized(id, channels);
    },
    listCommunityIds: () =>
      navService.nodes.filter((n) => n.type === 'community').map((n) => n.id),
  });
  // ZEB-665/ZEB-666: (re)connect the unread cursor stores when the owner
  // identity lands (or changes) — pre-identity the store no-ops all
  // reads/writes, and anything materialized before this point gets re-seeded
  // by connectOwner.
  $effect(() => {
    const oid = selfOwnerId;
    if (oid) {
      channelUnread?.connectOwner(oid);
      dmUnread?.connectOwner(oid);
    }
  });
  // ── Friend service (ZEB-370) ───────────────────────────────────────
  // Same eager-construct / adapter-wired-in-IIFE / destroy-on-unmount
  // pattern as CommunityService. Surfaced in the Settings panel via
  // FriendsPanel.
  const friendService = new FriendService();
  $effect(() => () => friendService.destroy());
  // ── ZEB-431: DM contact picker source ──────────────────────────────
  // The picker lists Active friends (keyed by master ownerIdHex — the
  // identifier class `add_space` members require), NOT zenoh presence
  // profiles (which are keyed by device identity hash and only exist
  // for peers whose broadcast traversed our mesh this session). The
  // mode split is structural, not temporal (Cursor PR #225 R1): in
  // Tauri the picker NEVER sees the presence/mock map — pre-hydration
  // it's just empty — so a device-hash entry can never be selected
  // even in the boot window before the first listFriends resolves.
  // Browser/mock demo mode keeps the legacy navService.profiles map.
  let dmContacts: Map<string, Profile> | null = $state(null);
  const EMPTY_DM_CONTACTS: Map<string, Profile> = new Map();
  // ZEB-432: owner_id(hex) → local friend nickname (ZEB-419), rebuilt from the
  // same friends list that feeds the DM picker. Consulted by community surfaces
  // (members roster + message authors) so a nicknamed friend renders by name
  // instead of raw hex. friend-list-changed (which fires on nickname edits too,
  // see the listener below) keeps this fresh.
  let friendNicknames: Map<string, string> = $state(new Map());
  let pickerContacts = $derived(
    isTauri() ? (dmContacts ?? EMPTY_DM_CONTACTS) : navService.profiles
  );
  // Monotonic sequencing so overlapping refreshes (connect + friend-list-
  // changed + dialog open can race) never let an older listFriends reply
  // overwrite a newer committed map (Cursor PR #225 R2). The guard compares
  // against the last COMMITTED sequence, not the last started one — a newer
  // call that FAILS carries no data and must not invalidate an older
  // in-flight success (Cursor R4: failures are inert).
  let dmContactsRefreshSeq = 0;
  let dmContactsCommittedSeq = 0;
  async function refreshDmContacts(): Promise<void> {
    const seq = ++dmContactsRefreshSeq;
    try {
      const friends = await friendService.listFriends();
      if (seq <= dmContactsCommittedSeq) return; // a newer success already committed
      dmContactsCommittedSeq = seq;
      dmContacts = contactsFromFriends(friends);
      // ZEB-432: rebuild the nickname map under the same commit guard so it
      // stays consistent with the DM-contact map (and never regresses to an
      // older friends snapshot).
      friendNicknames = new Map(
        friends
          .filter((f) => f.nickname)
          .map((f) => [f.ownerIdHex.toLowerCase(), f.nickname as string]),
      );
    } catch (e) {
      // Expected pre-owner-load ("owner not loaded") and in mock mode
      // (no adapter). Keep the last known-good map rather than wiping —
      // the friend-list-changed listener and dialog-open refresh retry.
      const msg = e instanceof Error ? e.message : String(e);
      console.debug('[harmony-client] refreshDmContacts skipped:', msg);
    }
  }
  // Covers add/remove/accept from any device, incl. ZEB-419 nickname
  // edits (those emit friend-list-changed too). Listener set is cleared
  // by friendService.destroy() on unmount.
  friendService.onFriendsChanged(() => void refreshDmContacts());
  // ── ZEB-236: DM-invite corner toast ─────────────────────────────────
  // Same eager-construct / adapter-wired-in-IIFE / destroy-on-unmount
  // pattern as FriendService. `dmInviteQueue` holds pending inbound DM
  // invites (oldest-first) not yet "Later"-dismissed this session; the
  // toast (mounted beside IncomingCallToast, below) shows only the head
  // of the queue. Accept/Decline call the service directly and do NOT
  // optimistically remove the row — the service's `dm-invite-list-changed`
  // event is the single source of truth, and it drives the same refresh
  // that hydrates the queue in the first place.
  const dmInviteService = new DmInviteService();
  $effect(() => () => dmInviteService.destroy());
  let dmInviteQueue: PendingDmInviteDto[] = $state([]);
  // Session-local dismissals from the toast's "Later" button. Deliberately
  // a plain (non-reactive) Set: it's never read by the template directly,
  // only consulted when filtering `dmInviteQueue` (which IS reactive), and
  // it resets on reload since it's not persisted anywhere.
  const laterDismissed = new Set<string>();
  async function refreshDmInviteQueue(): Promise<void> {
    try {
      const pending = await dmInviteService.listPending();
      // Prune dismissals whose invite is no longer pending (accepted, declined,
      // or store restart): "Later" suppresses the CURRENT staged invite only —
      // a genuinely new invite for the same Space later in the session must
      // toast again (spec: repeat invites re-prompt).
      const pendingIds = new Set(pending.map((invite) => invite.spaceIdHex));
      for (const id of laterDismissed) {
        if (!pendingIds.has(id)) laterDismissed.delete(id);
      }
      dmInviteQueue = pending
        .filter((invite) => !laterDismissed.has(invite.spaceIdHex))
        .sort((a, b) => a.receivedAtMs - b.receivedAtMs);
    } catch (e) {
      // Expected pre-owner-load and in mock mode (no adapter), same as
      // refreshDmContacts above. Keep the last known-good queue.
      const msg = e instanceof Error ? e.message : String(e);
      console.debug('[harmony-client] refreshDmInviteQueue skipped:', msg);
    }
  }
  // Fires on both new-invite-received and list-mutated (accept/decline,
  // possibly from another device). Listener set is cleared by
  // dmInviteService.destroy() on unmount.
  dmInviteService.onPendingChanged(() => void refreshDmInviteQueue());
  // T6 review fix: accept/decline can reject (backend error, stale invite
  // already actioned on another device, etc.) — surface it via the shared
  // toast store instead of letting it become an unhandled rejection with no
  // user feedback. The queue itself is refreshed via onPendingChanged above,
  // regardless of success/failure, so no optimistic-state cleanup is needed here.
  async function handleDmInviteAccept(spaceIdHex: string): Promise<void> {
    try {
      await dmInviteService.accept(spaceIdHex);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      toastStore.show(`DM invite accept failed: ${msg}`);
    }
  }
  async function handleDmInviteDecline(spaceIdHex: string): Promise<void> {
    try {
      await dmInviteService.decline(spaceIdHex);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      toastStore.show(`DM invite decline failed: ${msg}`);
    }
  }
  // ZEB-419: a SECOND MemberCardService dedicated to the Friends panel. It must
  // NOT share the roster instance: subscribeVisible(ids) reconciles to EXACTLY
  // the passed set, so friends + roster would unsubscribe each other. The panel
  // drives its subscriptions and owns its onUpdate; App only wires the adapter +
  // avatar resolver (below).
  const friendCardService = new MemberCardService();
  $effect(() => () => void friendCardService.unsubscribeAll());
  const channelMessageService = new ChannelMessageService();
  $effect(() => () => channelMessageService.destroy());
  // ZEB-600: on app teardown, drop ALL presence subscriptions (subscribe-all
  // keeps every joined community live for the session). No-ops if none / adapter
  // absent.
  $effect(() => () => {
    void presenceService.unsubscribeAll();
  });
  // ZEB-600: seed the viewer's own presence visibility once and stay live with
  // the backend so an "Appear offline" flip in Settings re-renders the member
  // self-dot without a restart. Empty reactive deps → runs once, torn down on
  // unmount. Both the initial read and the listen are individually error-guarded
  // (web/dev mode has no Tauri backend), so neither can crash boot.
  $effect(() => {
    let cancelled = false;
    // Subscribe BEFORE the initial read so a live "Appear offline" flip that
    // lands mid-boot can't be overwritten by a late-resolving getPresenceVisibility
    // seed (CodeRabbit #381). sawLiveUpdate wins over the initial read.
    let sawLiveUpdate = false;
    const stop = onPresenceVisibilityChanged((v) => {
      sawLiveUpdate = true;
      presenceVisible = v;
    });
    void getPresenceVisibility()
      .then((v) => {
        if (!cancelled && !sawLiveUpdate) presenceVisible = v;
      })
      .catch((e) => {
        console.warn(
          '[zeb-600] getPresenceVisibility failed:',
          e instanceof Error ? e.message : String(e),
        );
      });
    return () => {
      cancelled = true;
      stop();
    };
  });

  let navNodes = $state([...navService.nodes]);

  // ZEB-606: lazily load Tier-2 proposal counts for every community in the
  // nav (one IPC each on first sight; events keep them fresh afterwards).
  // Reading votingReady re-runs the pass once the adapter connects — without
  // it, nav nodes populated before connect would never get their initial
  // fetch (ensure() no-ops pre-connect; PR #408 Qodo R1 bug 3).
  $effect(() => {
    void votingReady;
    for (const n of navNodes) {
      if (n.type === 'community') proposalCountService.ensure(n.id);
    }
  });

  // Share the same resolver with the member-card service so peer-card avatars
  // (member rows, message feed) resolve through the identical fetch cache as
  // nav nodes. Task 11's setAvatarResolver does NOT touch resolver.onChange, so
  // setting the combined onChange immediately below is safe regardless of order.
  memberCardService.setAvatarResolver(avatarResolver);
  // ZEB-419: same shared resolver for the friends-panel card service.
  friendCardService.setAvatarResolver(avatarResolver);

  // When avatar CIDs finish resolving, push blob URLs into BOTH stored
  // nav profiles/nodes AND peer member cards so every avatar surface
  // re-renders after a late fetch completes.
  avatarResolver.onChange = () => {
    navService.refreshAvatars();
    memberCardService.onAvatarsRefreshed();
    // ZEB-419: the friends panel's card service shares this resolver — refresh it
    // too so resolved friend avatars repaint immediately, not only on its poll.
    friendCardService.onAvatarsRefreshed();
  };
  navService.onChange = () => {
    navNodes = [...navService.nodes];
  };

  let popoverProfile = $state<Profile | null>(null);
  let popoverX = $state(0);
  let popoverY = $state(0);

  // ── ZEB-341: owner_id card popover (click-to-view on members/authors) ──
  // Keyed by owner_id hex — a distinct world from the Reticulum `Profile`
  // popover above. Its own x/y avoids regressing the avatar-click path.
  let popoverCard = $state<{
    ownerIdHex: string;
    displayName: string;
    statusText: string;
    avatarUrl?: string;
    power?: number;
    membershipStatus?: string;
  } | null>(null);
  let popoverCardX = $state(0);
  let popoverCardY = $state(0);

  function openMemberCard(
    payload: {
      ownerIdHex: string;
      displayName: string;
      statusText: string;
      avatarUrl?: string;
      power?: number;
      membershipStatus?: string;
    },
    event: MouseEvent,
  ) {
    // Toggle-close if the same owner_id is re-clicked (mirrors the avatar path).
    if (popoverCard?.ownerIdHex === payload.ownerIdHex) {
      popoverCard = null;
      return;
    }
    const el =
      (event.currentTarget as HTMLElement | null) ??
      ((event.target as HTMLElement).closest('button') as HTMLElement | null);
    const rect = el?.getBoundingClientRect();
    const POPOVER_WIDTH = 300;
    const POPOVER_HEIGHT = 180;
    if (rect) {
      popoverCardX = Math.min(rect.right + 8, window.innerWidth - POPOVER_WIDTH - 8);
      popoverCardY = Math.min(rect.top, window.innerHeight - POPOVER_HEIGHT - 8);
    }
    // Mutually exclusive with the Reticulum avatar popover — only one shows.
    popoverProfile = null;
    popoverCard = payload;
  }

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
    // Mutually exclusive with the owner-card popover — only one shows.
    popoverCard = null;
    popoverProfile = profile;
  }

  function closePopover() {
    popoverProfile = null;
  }

  const notificationService = new NotificationService();

  // ZEB-662: persist notification settings owner-scoped (localStorage). Hydrate
  // once the owner id is known, then save on every settings change. Idempotent
  // across owner changes; load() runs before attach() so hydration is not re-saved.
  let notifPersistenceOwner: string | null = null;
  $effect(() => {
    const owner = selfOwnerId;
    if (owner && owner !== notifPersistenceOwner) {
      notifPersistenceOwner = owner;
      loadNotificationSettings(notificationService, owner);
      attachNotificationSettingsPersistence(notificationService, owner);
    }
  });
  const trustService = new TrustService();
  const fileManagerService = new FileManagerService();
  $effect(() => () => fileManagerService.destroy());
  // ZEB-669 S3: storage-buddy pacts + contribution meter state. Summary and
  // buddy rows stay null/empty until their IPCs succeed (honesty gate).
  const storageBuddyService = new StorageBuddyService();
  $effect(() => () => storageBuddyService.destroy());
  let contributionSummary = $state<ContributionSummaryDto | null>(null);
  let storageBuddies = $state<StorageBuddyDto[]>([]);
  let manageBuddiesOpen = $state(false);
  async function refreshContributionSummary(): Promise<void> {
    try {
      contributionSummary = await storageBuddyService.getContributionSummary();
    } catch (e) {
      // Meter renders nothing on failure — log once at warn, don't fabricate.
      const msg = e instanceof Error ? e.message : String(e);
      console.warn('[storage-buddy] contribution summary fetch failed:', msg);
      contributionSummary = null;
    }
  }
  async function refreshStorageBuddies(): Promise<void> {
    try {
      storageBuddies = await storageBuddyService.listBuddies();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      console.warn('[storage-buddy] buddy list fetch failed:', msg);
    }
  }
  // Declare fileManagerVersion before wiring onChange — same pattern as allMessages.
  let fileManagerVersion = $state(0);
  fileManagerService.onChange = () => { fileManagerVersion++; };
  const messageService = new MessageService();
  $effect(() => () => messageService.destroy());

  // ZEB-334: local-only self-notes store backing the private "Notes" space —
  // the always-present default shown when no community is joined.
  const notesService = new NotesService();

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

  // ZEB-404: a channel message from an author not in our roster means we
  // missed a live `community-members-changed` delta — the joiner announced
  // themselves by speaking. Re-fetch the roster (which also re-subscribes the
  // joiner's profile card, so their nickname resolves). Throttle so a backfill
  // (or a departed member's old posts, who never become 'joined') can't spin
  // refetches: at most one message-triggered refresh per
  // ROSTER_REFETCH_MIN_INTERVAL_MS. The throttle is time-based, not per-author,
  // so a failed or too-early refresh self-heals on the next message after the
  // window — it never permanently suppresses an author. We refetch the message's
  // own community (== selectedCommunityId here); refreshCommunityMembers' own
  // stale-response guard drops the result if the selection changed in-flight.
  const ROSTER_REFETCH_MIN_INTERVAL_MS = 3000;
  channelMessageService.onMessage = (communityId, channelId, message) => {
    // ZEB-662: drive mention notifications for EVERY community (including
    // background ones) — must run before the selected-community early-return
    // below, which only scopes the roster refetch. The alerter self-gates on
    // active channel / focus / policy, so this is safe to call unconditionally.
    void mentionAlerter
      ?.onMessage(communityId, channelId, message)
      ?.catch((e) => console.warn('[harmony-client] mention alert failed:', e));
    // ZEB-665: unread tracking, also for EVERY community. Self-gates on
    // cursor/author/focused-active (and ingest fires this for list/backfill
    // deliveries too — the service's ID set dedupes those by design).
    channelUnread?.onMessage(communityId, channelId, message);
    if (communityId !== selectedCommunityId) return;
    if (rosterHasJoinedAuthor(communityMembers, message.author)) return;
    const now = Date.now();
    if (now - lastMessageRosterRefetchAt < ROSTER_REFETCH_MIN_INTERVAL_MS) return;
    lastMessageRosterRefetchAt = now;
    void refreshCommunityMembers(communityId);
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
      // ZEB-341 Task 8: the MemberCardService is constructed early (so
      // seedSelf/resolve work at boot before Tauri-init); wire the adapter
      // now so cross-peer subscriptions can start.
      memberCardService.setAdapter(adapter);
      // ZEB-537: wire the same adapter into the community-presence service so
      // subscribe/get/unsubscribe can reach the backend IPCs.
      presenceService.setAdapter(adapter);
      // ZEB-419: wire the same adapter into the friends-panel card service.
      friendCardService.setAdapter(adapter);

      // ZEB-298 PR 2 Task 10 — wire the voting adapter so the
      // delegate-on-behalf Tauri event can fire toast notifications.
      // connectAdapter resolves once all voting-event listeners are
      // registered; only after that do we attach the toast handler so
      // we never miss an in-flight event during boot. Failure is
      // logged at warn level (matching other tryConnect callers below)
      // — toast notifications are non-critical UX; rest of the app
      // boots regardless.
      void votingAdapter
        .connectAdapter(adapter)
        .then(() => {
          // Tear down any prior toast subscription before registering a new
          // one — prevents duplicate toasts if connectAdapter is ever called
          // twice (e.g. a future reconnect path).
          toastUnsubscribe?.();
          toastUnsubscribe = setupDelegateOnBehalfToast(votingAdapter);
          // ZEB-606: badge counts subscribe to the same Tier-2 events.
          proposalCountService.onChange = () => {
            proposalCountVersion += 1;
          };
          proposalCountService.connectAdapter(votingAdapter);
          votingReady = true;
        })
        .catch((err) => {
          console.warn(
            '[harmony-client] votingAdapter connect failed:',
            err instanceof Error ? err.message : String(err),
          );
        });

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
      let startResp: StartNodeResponse | null = null;
      let startFailed = false;
      try {
        startResp = await invoke<StartNodeResponse>('start_node', { endpoint: null });
      } catch (err) {
        // feedback_tauri_error_extraction: production rejections are strings,
        // tests use Error objects — normalize either way.
        startFailed = true;
        startNodeError = err instanceof Error ? err.message : String(err);
        console.warn('[harmony-client] auto-start_node failed:', startNodeError);
      }

      // ZEB-338 / PR #169: gate on backend owner-identity presence, but keep
      // start_node *failure* distinct from "no owner identity". Forward-compat:
      // an older backend that omits hasOwnerIdentity classifies as 'missing'
      // (onboarding shown), never silently skipped. The backend keychain is the
      // authoritative source of "is this a new user".
      ownerIdentityState = classifyOwnerIdentity(startResp, startFailed);
      if (ownerIdentityState === 'present') {
        showWelcomeModal = false;
        // ZEB-341 Task 1: fetch the owner identity hex so we can seed
        // MemberCardService with the viewer's own display name immediately.
        // owner_id is the 32-char lowercase hex of the 16-byte OwnerAddr —
        // same format as MemberInfoDto.addr and ChannelMessageDto.author.
        try {
          const ownerState = await invoke<OwnerStateView | null>('get_owner_state');
          if (ownerState !== null) {
            selfOwnerId = ownerState.ownerId;
            connectOwnerTheme(ownerState.ownerId);
            // ZEB-586: load this returning owner's profile from its owner-scoped
            // cache before seeding/broadcasting the card, so no other identity's
            // name on this machine can leak into the boot republish below.
            myProfile = loadProfile(ownerState.ownerId);
            memberCardService.seedSelf(ownerState.ownerId, {
              displayName: myProfile.displayName,
              statusText: myProfile.statusText ?? '',
              avatarUrl: myProfile.avatarUrl,
              avatarCid: myProfile.avatarCid,
              profilePageRoot: myProfile.profilePageRoot,
            });
            // ZEB-341: re-publish this returning user's profile card on boot so
            // subscribing peers can resolve their name without a manual re-save.
            // (The 600s refresh only re-emits CACHED bytes, which are empty on a
            // fresh process.) Only marks done on success; if the Zenoh session
            // isn't up yet, the zenoh-status 'connected' handler retries.
            void tryBootPublishCard();
            // ZEB-351: build the voice session now owner identity is present.
            void buildVoiceSession(adapter.invoke, adapter.listen, ownerState.ownerId);
          }
        } catch (err) {
          const msg = err instanceof Error ? err.message : String(err);
          console.debug('[harmony-client] get_owner_state not yet available:', msg);
        }
        // Returning user who clicked an invite before start_node resolved:
        // drain it. (routeInviteUrl queued it because the owner wasn't loaded
        // yet at the time.)
        drainQueuedInvite();
      } else if (ownerIdentityState === 'missing') {
        // Genuine first run → hard gate. (A deep-link that already arrived was
        // queued by routeInviteUrl, not shown over the welcome.)
        showWelcomeModal = true;
      } else {
        // 'error': start_node threw. Do NOT show the mint gate — if an identity
        // already exists on disk but failed to load, the non-dismissible mint
        // gate would deadlock (mint refuses "already exists"). The startup-error
        // overlay offers a retry instead.
        // 'revoked' (ZEB-668 S2): same rule — owner_state.cbor exists, so the
        // mint gate would deadlock; the terminal "removed" overlay renders.
        showWelcomeModal = false;
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

      // ── ZEB-666: DM/group-DM unread counts. Constructed (and BOTH hooks
      // assigned) BEFORE messageService/navService connect so no dm-received
      // or nav-updated event can fire into a null hook (the ZEB-665 Qodo
      // lesson: an optional-chained hook is a silent no-op, and nothing
      // replays it). Seeds via read_dm_thread directly (newest-first page —
      // NOT messageService.loadDmThread, which ingests into the feed and
      // advances its own pagination cursor). Shares ONE cursor-store
      // instance with ChannelUnreadService below: both persist into the
      // same owner-scoped localStorage blob, so separate instances would
      // clobber each other's keys on every write.
      let unreadCursorStore:
        | import('./lib/unread-cursor-store').LocalStorageUnreadCursorStore
        | null = null;
      try {
        const { DmUnreadService } = await import('./lib/dm-unread-service');
        const { LocalStorageUnreadCursorStore } = await import('./lib/unread-cursor-store');
        unreadCursorStore = new LocalStorageUnreadCursorStore();
        const dm = new DmUnreadService({
          listThreadPage: (spaceId, limit) =>
            invoke('read_dm_thread', { spaceId, limit, beforeHlc: null }) as Promise<
              import('./lib/dm-unread-service').DmThreadPageEntry[]
            >,
          // ZEB-357: the third param is the missed-call subset → 📞 badge.
          setUnread: (spaceId, count, missedCalls) =>
            navService.setUnread(spaceId, count, missedCalls),
          // Same "looking right at it" contract as channelUnread's
          // isActiveChannel below, in DM terms.
          isActiveThread: (spaceId) =>
            appMode === 'messages' &&
            activeChannel === spaceId &&
            (activeChannelType === 'dm' || activeChannelType === 'group-chat'),
          isFocused: () => document.hasFocus(),
          selfOwnerId: () => selfOwnerId ?? null,
          storage: unreadCursorStore,
          now: () => Date.now(),
        });
        dmUnread = dm;
        navService.onDmSpaceChange = (action, spaceId) => {
          if (action === 'added') void dm.onDmSpaceMaterialized(spaceId);
          else dm.onDmSpaceRemoved(spaceId);
        };
        messageService.onDmReceived = (p) => dm.onDmReceived(p);
        // Owner may already be known (the $effect only re-fires on change).
        if (selfOwnerId) dm.connectOwner(selfOwnerId);
        fileManagerService.addUnlisten(() => {
          dmUnread = null;
          // Also clear the hook fields — they close over `dm`, so leaving
          // them installed would retain the dead service across a
          // teardown/remount boundary and risk duplicate callbacks (Qodo
          // PR #432).
          navService.onDmSpaceChange = undefined;
          messageService.onDmReceived = undefined;
        });
      } catch (e) {
        console.warn(
          '[harmony-client] dm-unread init failed:',
          e instanceof Error ? e.message : String(e),
        );
      }

      await tryConnect('message', messageService.connectAdapter(adapter));
      // Mail connect may fail if mail_mgr isn't ready yet (race with
      // start_node); non-blocking so other services proceed. The
      // zenoh-status handler below re-hydrates mail state on reconnect.
      tryConnect('mail', mailService.connectAdapter(adapter));
      await tryConnect('vine', vineService.connectAdapter(adapter));
      await tryConnect('vine.loadFollowed', vineService.loadFollowed());
      // (vine.hydrate moved below fetchOwnAddress — ZEB-672: own persisted
      // reaction rows must resolve likedByMe, which needs ownAddress.)
      await tryConnect('fileManager', fileManagerService.connectAdapter(adapter));
      // ZEB-669 S3: storage-buddy meter + manage sheet. The summary stays
      // null on failure (meter renders nothing — never fabricated), and
      // both backend events re-fetch it.
      await tryConnect('storageBuddy', storageBuddyService.connectAdapter(adapter));
      void refreshContributionSummary();
      storageBuddyService.onChange(() => {
        void refreshContributionSummary();
        // Backup-flag changes can originate outside this window (headless
        // RPC) and arrive as storage-buddies-updated — keep the file list
        // and detail panel honest too.
        void fileManagerService.refreshContents();
        if (manageBuddiesOpen) void refreshStorageBuddies();
      });
      await tryConnect('community', communityService.connectAdapter(adapter));
      // ZEB-663: base channel-config handler — keep the nav tree's channel
      // children fresh on create/rename/delete. CommunityView (when mounted)
      // chains its own feed-list refresh on top of this via the existing
      // prev-handler chain.
      communityService.onChannelConfigChanged = (cid) => {
        void channelNavSync.resync(cid);
      };
      await tryConnect('friend', friendService.connectAdapter(adapter));
      // ZEB-431: hydrate the DM contact picker from the friend graph.
      // Fire-and-forget: pre-owner-load failure is recovered by the
      // friend-list-changed listener and the dialog-open refresh.
      void refreshDmContacts();
      await tryConnect('dmInvite', dmInviteService.connectAdapter(adapter));
      // ZEB-236: hydrate the pending DM-invite queue. Fire-and-forget:
      // pre-owner-load failure is recovered by the onPendingChanged
      // listener once a real event fires.
      void refreshDmInviteQueue();
      await tryConnect('channelMessage', channelMessageService.connectAdapter(adapter));
      avatarResolver.connectAdapter(adapter);
      // ZEB-345 Task 10: wire the lazy profile-page resolver so panel opens can
      // fetch_profile_doc. No eager per-member resolution (unlike avatars).
      profilePageResolver.connectAdapter(adapter);
      resolveVideoFn = async (cid: string) => {
        const bytes = (await adapter.invoke('fetch_content', { cid })) as number[];
        const mime = detectVideoMime(bytes);
        const blob = new Blob([new Uint8Array(bytes)], { type: mime });
        return URL.createObjectURL(blob);
      };
      await tryConnect('nav', navService.connectAdapter(adapter));

      // ZEB-393 Bug B: rehydrate persisted communities into the sidebar. The
      // nav tree is otherwise push-only/session-scoped and boots empty on every
      // restart regardless of what's on disk. Pull (not a backend boot emit) so
      // it can't race the nav listener registered just above; addOrUpdateNavSpace
      // is cold-replay idempotent, so a later runtime nav-updated for the same
      // community is a no-op update. Non-fatal: failure leaves the sidebar empty
      // (today's behaviour), it doesn't block the rest of boot.
      try {
        for (const c of await communityService.listOwnerCommunities()) {
          navService.addOrUpdateNavSpace(toNavPayload(c));
        }
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        console.warn('[harmony-client] community rehydration failed:', msg);
      }

      // Fetch our node address so self-sent messages/vines echo back as
      // 'self'/'You'. Try immediately (node may already be connected after
      // hot reload / auto-start), and also retry on later zenoh-status
      // events. Runs BEFORE the DM rehydration below (CodeRabbit PR #432):
      // addOrUpdateNavSpace derives a 1:1 DM's peer as "whichever member
      // isn't me" and falls back to members[0] when ownAddress is null —
      // a wrong (self) peer attachment never self-corrects because profile
      // updates only refresh nodes whose peer.address matches.
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

      // ZEB-612 S2: pull the Rust-persisted feed cache (descriptors +
      // viewed-set + reactions, ZEB-672) — vine-received /
      // vine-reaction-received only fire on FIRST receipt, so a restarted
      // app never re-receives what the cache already holds. Must run after
      // loadFollowed (hydrate classifies rows by the follow set; the list
      // DTO carries no source field) and after fetchOwnAddress (own
      // reaction rows resolve likedByMe by matching ownAddress).
      await tryConnect('vine.hydrate', vineService.hydrate());

      // ZEB-666: rehydrate persisted DM / group-DM rows the same way — the
      // nav otherwise boots with NO DM rows (push-only; only runtime
      // nav-updated events re-create them), which would strand the persisted
      // read cursors. Pull (not a boot emit) for the same no-race reason;
      // addOrUpdateNavSpace is cold-replay idempotent, and each row fires
      // onDmSpaceChange → unread seeding. Non-fatal.
      try {
        const dms = (await invoke('list_owner_dm_spaces')) as Array<{
          spaceId: string;
          kind: 'dm' | 'group-dm';
          name: string;
          members: string[];
        }>;
        for (const d of dms) {
          navService.addOrUpdateNavSpace({
            action: 'added',
            spaceId: d.spaceId,
            kind: d.kind,
            name: d.name,
            members: d.members,
          });
        }
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        console.warn('[harmony-client] dm rehydration failed:', msg);
      }

      // ── ZEB-665: per-channel unread counts. Constructed BEFORE
      // channelNavSync.start() so the very first setChannels materialization
      // seeds deterministically (Qodo PR #430: the optional-chained hook is a
      // silent no-op while channelUnread is null, and nothing replays it).
      // Seeds via list_channel_messages directly (NOT
      // channelMessageService.listMessages — that would ingest into the feed
      // cache and re-fire onMessage), newest-first (ZEB-602) so a seed that
      // overflows the tracking cap holds the NEWEST cap-worth of unread.
      // Self-gates on cursor, author, and focused-active; NavService renders
      // the badges.
      try {
        const { ChannelUnreadService } = await import('./lib/channel-unread-service');
        const { LocalStorageUnreadCursorStore } = await import('./lib/unread-cursor-store');
        channelUnread = new ChannelUnreadService({
          listMessagesSince: (communityId, channelId, since, limit) =>
            invoke('list_channel_messages', {
              communityId,
              channelId,
              since,
              limit,
              order: 'desc',
            }) as Promise<import('./lib/channel-message-service').ChannelMessageDto[]>,
          setUnread: (channelId, count) => navService.setUnread(channelId, count),
          // Same "looking right at it" contract as the mention alerter below.
          isActiveChannel: (communityId, channelId) =>
            appMode === 'messages' &&
            selectedCommunityId === communityId &&
            communityActiveView === 'channels' &&
            communityService.getSelectedChannel(communityId) === channelId,
          isFocused: () => document.hasFocus(),
          selfOwnerId: () => selfOwnerId ?? null,
          // ZEB-666: shared with DmUnreadService — separate instances would
          // clobber each other's keys in the same owner-scoped blob.
          storage: unreadCursorStore ?? new LocalStorageUnreadCursorStore(),
          now: () => Date.now(),
        });
        // Owner may already be known (the $effect only re-fires on change).
        if (selfOwnerId) channelUnread.connectOwner(selfOwnerId);
        fileManagerService.addUnlisten(() => { channelUnread = null; });
      } catch (e) {
        console.warn(
          '[harmony-client] channel-unread init failed:',
          e instanceof Error ? e.message : String(e),
        );
      }

      // ZEB-663: now that nav communities are hydrated, populate their
      // channels as nav nodes. Fire-and-forget; per-community failures are
      // isolated inside ChannelNavSync and never block boot.
      void channelNavSync.start();

      // ZEB-600: subscribe presence for ALL joined communities (not just the
      // active one) so the sidebar + DM dots reflect every community. A shared
      // callback bumps presenceVersion; per-community failures are logged and
      // skipped. setActive:false so these background subscribes never clobber the
      // active roster isOnline() reads — only an explicit selection
      // (changeSelectedCommunity) points active at a community (CodeRabbit #381).
      for (const navNode of navService.nodes) {
        if (navNode.type === 'community') {
          void presenceService
            .subscribe(
              navNode.id,
              () => {
                presenceVersion++;
              },
              { setActive: false },
            )
            .catch((e) =>
              console.error(
                'presence subscribe-all failed for',
                navNode.id,
                e instanceof Error ? e.message : String(e),
              ),
            );
        }
      }

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
          // ZEB-672: hydrate is chained after loadFollowed (classification
          // needs the follow set) and replayed on reconnect so persisted own
          // reactions recalculate likedByMe when boot-time get_node_addr
          // raced (the handler awaits fetchOwnAddress before this). hydrate
          // is idempotent: seenIds / reactors-set / viewed-set all dedupe.
          [
            'vine.loadFollowed+hydrate',
            vineService.loadFollowed().then(() => vineService.hydrate()),
          ],
        ] as const;
        const results = await Promise.allSettled(tasks.map(([, p]) => p));
        for (const [i, result] of results.entries()) {
          if (result.status === 'rejected') {
            console.warn(`[harmony-client] ${tasks[i][0]} failed after reconnect:`, result.reason);
          }
        }
      }
      // ZEB-668 S2: a mid-session self-revocation (this device removed itself,
      // or a sibling's revocation replicated in via the trust engine) drops the
      // app into the terminal "removed" state. Boot-time revocation is handled
      // by classifyOwnerIdentity (StartNodeResponse.selfRevoked) — this event
      // covers the live transition. Local media is client-owned (the backend
      // only stops its own engines), so mirror the quit-requested teardown:
      // a revoked device must not keep holding the mic or an active call
      // (CodeRabbit PR #452).
      const unlistenRevokedSelf = await listen('device-revoked-self', () => {
        ownerIdentityState = 'revoked';
        void voiceSession?.leave().catch(() => {});
        void callSession?.end().catch(() => {});
        if (groupCall) {
          const gp = get(groupCall.state).phase;
          if (gp !== 'idle' && gp !== 'incoming') {
            void groupCall.leave().catch(() => {});
          }
        }
      });
      fileManagerService.addUnlisten(unlistenRevokedSelf);

      const unlistenStatus = await listen('zenoh-status', async (event) => {
        const status = (event as { payload: { status: string } }).payload;
        if (status.status === 'connected') {
          await fetchOwnAddress();
          await reloadBackendState();
          // ZEB-404: a reconnect may have missed live community-members-changed
          // deltas while the session was down; converge the active community's
          // roster (and, transitively, its members' profile cards).
          if (selectedCommunityId !== null) {
            void refreshCommunityMembers(selectedCommunityId);
          }
          // ZEB-341: the initial get_owner_state probe can return null when the
          // owner finishes loading AFTER start_node returns; selfOwnerId would
          // then stay null and tryBootPublishCard would be a permanent no-op.
          // Re-fetch here (only while still unknown) so the boot card publish
          // can proceed once the owner is available.
          if (selfOwnerId === null) {
            try {
              // `invoke` is already in scope from the enclosing Tauri-init IIFE
              // (imported once at boot) — no need to re-import it on every
              // reconnect.
              const ownerState = await invoke<OwnerStateView | null>('get_owner_state');
              if (ownerState !== null) {
                selfOwnerId = ownerState.ownerId;
                connectOwnerTheme(ownerState.ownerId);
                // ZEB-586: this is the third owner-resolution path (owner loads
                // after start_node). Like the other two, load the owner-scoped
                // profile before seeding/publishing so tryBootPublishCard can't
                // broadcast the pre-owner default for the resolved owner.
                myProfile = loadProfile(ownerState.ownerId);
                memberCardService.seedSelf(ownerState.ownerId, {
                  displayName: myProfile.displayName,
                  statusText: myProfile.statusText ?? '',
                  avatarUrl: myProfile.avatarUrl,
                  avatarCid: myProfile.avatarCid,
                  profilePageRoot: myProfile.profilePageRoot,
                });
              }
            } catch {
              // owner still unavailable — leave selfOwnerId null for a later retry
            }
          }
          // ZEB-431 (Qodo R3): the connect-time refreshDmContacts fails in
          // the same owner-loads-after-start_node window as get_owner_state
          // above, leaving the picker source null (empty picker in Tauri
          // mode, even for an already-open dialog). Retry once the session
          // is confirmed up, gated on the never-hydrated state so routine
          // reconnects don't re-IPC — after first hydration, friend-list-
          // changed and dialog-open refreshes keep the map fresh. dmContacts
          // is $state, so an open dialog converges live via pickerContacts.
          if (dmContacts === null) {
            void refreshDmContacts();
          }
          // ZEB-351: (re)attempt the voice-session build on reconnect — the
          // get_self_voice_identity IPC may not have been ready at boot. The
          // one-shot guard inside makes this a no-op once it has succeeded.
          if (selfOwnerId !== null) {
            void buildVoiceSession(adapter.invoke, adapter.listen, selfOwnerId);
          }
          // The session is now confirmed up — (re)attempt the one-shot boot card
          // publish in case an earlier attempt raced session startup.
          await tryBootPublishCard();
        }
      });
      // zenoh-status serves messages, vines, and nav (fetchOwnAddress sets
      // all ownAddress fields). All four services are destroyed on unmount;
      // cleanup order is irrelevant since no service depends on another's
      // teardown. Registered on fileManagerService arbitrarily.
      // listen() returns UnlistenFn (= () => void), matching addUnlisten's
      // signature directly — no cast needed.
      fileManagerService.addUnlisten(unlistenStatus);

      // ZEB-341: instant card resolution. The backend emits
      // `member-card-received` whenever a verified card is cached; apply it
      // immediately so names don't lag the 3s poll. The poll loop remains the
      // fallback (applyCard is idempotent with it). Registered here next to
      // the zenoh-status listener so the adapter/event system is confirmed
      // ready, and torn down via the same addUnlisten path on unmount.
      const unlistenMemberCard = await listen<{
        subscriptionId: number;
        ownerIdHex: string;
        displayName: string;
        statusText: string;
        avatarCid?: string;
        profilePageRoot?: string;
      }>('member-card-received', (event) => {
        const card = {
          displayName: event.payload.displayName,
          statusText: event.payload.statusText,
          avatarCid: event.payload.avatarCid,
          // ZEB-345: forward the profile-page root CID so an open panel for
          // this owner re-resolves once a fresh card lands.
          profilePageRoot: event.payload.profilePageRoot,
        };
        memberCardService.applyCard(event.payload.ownerIdHex, card);
        // ZEB-419: also feed the friends-panel card service so friend/request
        // rows update instantly on a pushed card, not only via its 3s poll.
        friendCardService.applyCard(event.payload.ownerIdHex, card);
      });
      fileManagerService.addUnlisten(unlistenMemberCard);

      // ── ZEB-356: build the incoming-call alerter (OS notification + window
      // attention). Constructed BEFORE the call-signaling listeners below so an
      // `incoming-call` arriving right after init still escalates (no startup
      // gap). The notification-permission prompt is deferred to buildVoiceSession
      // (owner ready) so first-run users aren't prompted before onboarding.
      try {
        const { createDefaultIncomingCallAlerter } = await import('./lib/incoming-call-alert');
        incomingCallAlerter = await createDefaultIncomingCallAlerter();
        fileManagerService.addUnlisten(() => { incomingCallAlerter?.dispose(); incomingCallAlerter = null; });
      } catch (e) {
        console.warn('[harmony-client] incoming-call alerter init failed:', e);
      }

      // ── ZEB-662: build the mention-notification alerter. Reuses the same
      // Tauri focus/notification rails as the call alerter; self-gates on
      // active channel, focus, and the NotificationService policy.
      try {
        const { createDefaultMentionAlerter } = await import('./lib/mention-alert');
        mentionAlerter = await createDefaultMentionAlerter({
          getSelfOwnerId: () => selfOwnerId ?? undefined,
          // Community channels live in CommunityView / CommunityService state,
          // NOT NavService — the viewer is "looking right at" a channel only in
          // messages mode, with that community selected, on its channels tab,
          // and that channel being its session-selected one. (App-level
          // `activeChannel` is a DM/messages channel and never a community one.)
          isActiveChannel: (communityId, channelId) =>
            appMode === 'messages' &&
            selectedCommunityId === communityId &&
            communityActiveView === 'channels' &&
            communityService.getSelectedChannel(communityId) === channelId,
          resolve: (p, peer, community) => notificationService.resolve(p, peer, community),
          incMention: (communityId, channelId) =>
            navService.incMention(communityId, channelId),
          // Community channels aren't nav nodes in production; resolve the name
          // from CommunityService's session cache (populated when the community
          // was opened), falling back to a generic label.
          getChannelName: (communityId, channelId) =>
            communityService.getCachedChannelName(communityId, channelId) ?? 'a channel',
          showToast: (m) => { toastStore.show(m); },
        });
        fileManagerService.addUnlisten(() => { mentionAlerter = null; });
      } catch (e) {
        console.warn('[harmony-client] mention alerter init failed:', e);
      }

      // ── ZEB-352 Voice V4: DM-call signaling listeners ───────────────
      // The backend emits these once per call-state transition; route each into
      // the CallSession state machine (built lazily in buildVoiceSession, so
      // guard with `callSession?.`). Each handler is callId-guarded inside the
      // session, so stale events for an old call can't disturb a current one.
      const unlistenIncomingCall = await listen('incoming-call', (event) => {
        const p = (event as { payload: IncomingCallEvent }).payload;
        // ZEB-364: callSession is built lazily in buildVoiceSession (fired via
        // `void`). A ring arriving before it exists must be buffered, not dropped
        // — the drain in buildVoiceSession replays it through the same handler.
        if (!callSession) { dmIncomingQueue.queue(p); return; }
        handleIncomingDmCall(p);
      });
      fileManagerService.addUnlisten(unlistenIncomingCall);

      const unlistenCallAccepted = await listen('call-accepted', (event) => {
        const p = (event as { payload: { callId: string } }).payload;
        void callSession?.onRemoteAccepted(p.callId);
      });
      fileManagerService.addUnlisten(unlistenCallAccepted);

      const unlistenCallDeclined = await listen('call-declined', (event) => {
        const p = (event as { payload: { callId: string; reason: string } }).payload;
        // Only surface a toast when the decline targets the call we're actually
        // ringing out on — a delayed decline for an old call must not pop "No
        // answer"/"Call declined" over an unrelated, later session. Capture the
        // match BEFORE onRemoteDeclined resets the state machine to idle.
        const isActiveCall = !!callSession && get(callSession.state).callId === p.callId;
        callSession?.onRemoteDeclined(p.callId, p.reason);
        if (!isActiveCall) return;
        const msg = p.reason === 'busy'
          ? 'User is on another call'
          : p.reason === 'timeout'
            ? 'No answer'
            : 'Call declined';
        toastStore.show(msg);
      });
      fileManagerService.addUnlisten(unlistenCallDeclined);

      const unlistenCallCanceled = await listen('call-canceled', (event) => {
        const p = (event as { payload: { callId: string } }).payload;
        // ZEB-364: if the caller cancels while callSession is still null (startup
        // window), record it so the drain suppresses the matching buffered ring
        // instead of replaying a call that's already gone. Once the session
        // exists, cancels route normally and clear an already-replayed ring.
        if (!callSession) { dmIncomingQueue.cancel(p.callId); return; }
        callSession.onRemoteCanceled(p.callId);
      });
      fileManagerService.addUnlisten(unlistenCallCanceled);

      const unlistenCallEnded = await listen('call-ended', (event) => {
        const p = (event as { payload: { callId: string } }).payload;
        void callSession?.onRemoteEnded(p.callId);
      });
      fileManagerService.addUnlisten(unlistenCallEnded);

      // ── ZEB-360 Voice (group DM): group-call signaling listeners ─────
      // Mirror the 1:1 wiring above. Each handler is callId-guarded inside the
      // GroupCallSession, so stale events for an old call can't disturb a current
      // one (guard with `groupCall?.` — built lazily in buildVoiceSession). The
      // presence/incoming handlers await ensureGroupMembers FIRST so the sync
      // members cache (resolveMembers) is warm before the roster merge — that's
      // what renders the ringing/declined rows on the very first beacon.
      fileManagerService.addUnlisten(await listen('incoming-group-call', async (event) => {
        const p = (event as { payload: IncomingCallEvent }).payload;
        // ZEB-364: groupCall is built lazily in buildVoiceSession; buffer a ring
        // that arrives before it exists so the drain can replay it (else dropped).
        if (!groupCall) { groupIncomingQueue.queue(p); return; }
        await handleIncomingGroupCall(invoke, p);
      }));

      fileManagerService.addUnlisten(await listen('group-call-presence-changed', async (event) => {
        const p = (event as { payload: { spaceId: string; callId: string; roster: { owner: string; device: string; muted: boolean }[] } }).payload;
        // Best-effort cache warm BEFORE the merge so ringing rows render on the
        // first beacon — but a transient failure must NOT drop the presence
        // update (banner count / roster), so swallow it and proceed.
        await ensureGroupMembers(invoke, p.spaceId).catch(() => {});
        groupCall?.onPresenceChanged(p.callId, p.roster);
        groupCallBanners.apply(p.spaceId, p.callId, p.roster);
      }));

      fileManagerService.addUnlisten(await listen('group-call-declined', (event) => {
        const p = (event as { payload: { callId: string; spaceId: string; owner: string } }).payload;
        groupCall?.onDeclined(p.callId, p.owner);
      }));

      // Tear down the callSession.state subscription (set in buildVoiceSession)
      // on unmount, alongside the listeners.
      fileManagerService.addUnlisten(() => { callStateUnsub?.(); callStateUnsub = null; });
      // ZEB-360: same for the group session's state subscription.
      fileManagerService.addUnlisten(() => { groupCallStateUnsub?.(); groupCallStateUnsub = null; });

      const { getCurrentWebviewWindow } = await import('@tauri-apps/api/webviewWindow');
      const appWin = getCurrentWebviewWindow();
      // ── ZEB-356/ZEB-433: conditional close-to-tray. ─────
      // Closing the window hides it to the tray and keeps the process, the
      // Zenoh node, and any ACTIVE CALL alive (hide-during-call = "minimize to
      // keep talking"). The real exit is the tray "Quit Harmony" item, handled
      // by the quit-requested listener below. No teardown here.
      //
      // ZEB-433: hide ONLY when the tray actually exists. If tray creation
      // failed (tray_active=false — or this query itself fails), hiding would
      // strand the node headless with no restore/quit affordance, so the
      // close proceeds and the process exits via last-window-closed.
      let trayActive = false;
      try {
        trayActive = (await invoke('tray_active')) as boolean;
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        console.warn('[harmony-client] tray_active query failed; window close will exit:', msg);
      }
      // The tray notice is best-effort: a notification-plugin import failure
      // must not abort the lifecycle wiring below (close handler +
      // quit-requested listener), so it gets its own try with a no-op
      // fallback notifier.
      let notifyTrayResident: () => Promise<boolean> = async () => false;
      try {
        const notif = await import('@tauri-apps/plugin-notification');
        notifyTrayResident = makeTrayResidentNotifier(notif);
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        console.warn('[harmony-client] notification plugin unavailable; tray notice disabled:', msg);
      }
      const unlistenClose = await appWin.onCloseRequested(
        makeCloseRequestedHandler(trayActive, {
          hide: () => appWin.hide(),
          notifyTrayResident,
          storage: localStorage,
        }),
      );
      fileManagerService.addUnlisten(unlistenClose);

      // ── ZEB-356: real quit path. The tray "Quit Harmony" item emits
      // `quit-requested`; run the (bounded, best-effort) V5 voice/call teardown
      // so we don't linger in peers' rosters or hold the mic, then invoke
      // quit_app to terminate the tray-resident process.
      const unlistenQuit = await listen('quit-requested', async () => {
        // ZEB-360 (Cursor R2): also leave an active GROUP call on quit, or we
        // linger in peers' rosters (no presence tombstone + no `leave_group_call`)
        // and hold the mic until the presence TTL expires. 'incoming' has no media
        // engine yet, so there's nothing to tear down for a ring-only state.
        let groupLeave: Promise<void> = Promise.resolve();
        if (groupCall) {
          const gp = get(groupCall.state).phase;
          if (gp !== 'idle' && gp !== 'incoming') {
            groupLeave = groupCall.leave().catch(() => {});
          }
        }
        // PR #494 R1 (CodeRabbit): the drain also awaits in-flight call-event
        // DM writes — callSession.end() registers the terminal record into
        // pendingCallRecordWrites, and runQuitTeardown snapshots that set
        // AFTER the teardowns settle so the record can't be orphaned by
        // quit_app. Same single 1.5s bound as before.
        await runQuitTeardown({
          teardowns: [
            voiceSession?.leave() ?? Promise.resolve(),
            callSession?.end() ?? Promise.resolve(),
            groupLeave,
          ],
          pendingWrites: () => [...pendingCallRecordWrites],
          timeoutMs: 1500,
          warn: (m) => console.warn(`[harmony-client] ${m}`),
        });
        await invoke('quit_app');
      });
      fileManagerService.addUnlisten(unlistenQuit);
    } catch (err) {
      console.warn('[harmony-client] Tauri init failed:', err);
    }
  })();

  // ── ZEB-328: startup update check + deep-link routing ─────────────
  // PR #160 R1: matches the rest of App.svelte's pattern — isTauri()
  // guard up front + dynamic imports of Tauri APIs only inside the
  // guarded block. Static imports of @tauri-apps/* were tree-shaken
  // into the dev bundle and would either throw or no-op outside
  // Tauri; the dynamic-import shape sidesteps both.
  //
  // Ordering matters: subscribe to `deep-link-received` BEFORE any
  // awaits that could span the gap where the OS hands us a URL.
  // The earlier ordering put `checkForUpdate()` first, leaving a
  // ~few-hundred-ms window where a warm-app deep-link could be
  // missed (cold-launch was covered by getCurrent(); warm wasn't).
  onMount(async () => {
    if (!isTauri()) {
      return;   // Web/dev mode: skip Tauri-plugin work entirely
    }
    let unlistenDeepLink: (() => void) | undefined;
    try {
      const { listen } = await import('@tauri-apps/api/event');
      const { getCurrent: getCurrentDeepLink } = await import('@tauri-apps/plugin-deep-link');

      // (1) Subscribe FIRST so warm-launch handoffs aren't dropped
      // during the subsequent awaits.
      unlistenDeepLink = await listen<string[]>('deep-link-received', (event) => {
        const url = extractHarmonyInviteUrl(event.payload);
        if (url) {
          routeInviteUrl(url);
        }
      });

      // (2) Drain URLs queued by the deep-link plugin from before the
      // listener was registered (cold-launch path: OS launches the
      // app with a harmony:// URL → plugin queues it → first
      // getCurrent() returns it).
      try {
        const queued = await getCurrentDeepLink();
        if (queued) {
          const url = extractHarmonyInviteUrl(queued);
          if (url) {
            routeInviteUrl(url);
          }
        }
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        console.warn(`[harmony-client] deep-link getCurrent() failed: ${msg}`);
      }

      // (3) Updater check LAST. `checkForUpdate()` is self-protecting
      // (try/catch internal, returns null on failure) but doing it
      // after the listener registration means a slow updater
      // endpoint can never cause us to drop a deep-link.
      availableUpdate = await checkForUpdate();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      console.warn(`[harmony-client] ZEB-328 startup setup failed: ${msg}`);
    }

    return () => {
      unlistenDeepLink?.();
    };
  });

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
    // Prefer the exact sidecar row: CID-only lookup can land on a
    // duplicate-CID sibling and show stale per-row state (ZEB-164 symlink
    // model; Greptile PR #450).
    return fileManagerService.getContentDetail(selectedFileCid, selectedFileSidecarId ?? undefined);
  });

  // ZEB-612 S3: "Used by N vines" — recompute when either vine feed
  // changes (the $state mirrors are the reactive signal; the service's
  // internal arrays are not reactive on their own).
  let selectedFileVineCount = $derived.by(() => {
    void followedVines;
    void discoverVines;
    if (!selectedFileDetail) return 0;
    return vineService.countByVideoCid(selectedFileDetail.cid);
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

  // Thread state
  let openThreadId = $state<string | null>(null);
  let threadModes = $state<Map<string, ThreadDisplayMode>>(new Map());
  let pinnedThreadIds = $state<Set<string>>(new Set());

  let activeChannel = $state('general');
  let activeHub = $state('harmony-dev');
  let activeChannelName = $state('general');
  let activeChannelType = $state<'channel' | 'dm' | 'group-chat'>('channel');

  // ── ZEB-360 T13: group-DM call presence watch/unwatch ──────────────
  // The currently-selected group-DM space (null when the active view isn't a
  // group DM). Drives the read-only presence subscription (watch_group_call)
  // that feeds the group-call banner store, plus the header Call/Join button.
  let activeGroupDmSpaceId = $derived(
    activeChannelType === 'group-chat' && !selectedCommunityId ? activeChannel : null,
  );
  // Header-button model for the active group DM. `groupCallActive` = a call is in
  // progress in this space (banner store has an entry); `groupCallSelf` = the
  // local user is in *that* call (any non-idle phase whose callId matches this
  // space's banner entry); `groupCallBusy` = any voice session is active, so a new
  // place/join must be blocked (one media engine at a time).
  let groupCallActive = $derived(
    !!activeGroupDmSpaceId && !!$groupCallBanners[activeGroupDmSpaceId],
  );
  let groupCallSelf = $derived(
    !!activeGroupDmSpaceId
    && !!$groupCallState
    && $groupCallState.phase !== 'idle'
    && !!$groupCallState.callId
    && $groupCallState.callId === $groupCallBanners[activeGroupDmSpaceId]?.callId,
  );
  let groupCallBusy = $derived(
    (!!$voiceState && $voiceState.phase === 'connected')
    || (!!$callSessionState && $callSessionState.phase !== 'idle')
    || (!!$groupCallState && $groupCallState.phase !== 'idle'),
  );

  // Subscribe to group-call presence for the viewed group DM; unsubscribe when
  // the selection changes away (or the view unmounts). The $effect cleanup runs
  // before each re-run AND on teardown, so a stable space id re-runs the effect
  // only when it actually changes — no double-watch. Tauri-only; the dynamic
  // import / invoke throw outside Tauri and are swallowed.
  $effect(() => {
    const spaceId = activeGroupDmSpaceId;
    if (!spaceId || !isTauri()) return;
    let active = true;
    void (async () => {
      // Bounded retry/backoff: a transient "node not ready" at startup would
      // otherwise swallow the watch permanently (the effect only re-runs on a
      // space change), leaving the banner subscription off for this space. Retry
      // a few times so the watch establishes once the node is up. `active` is
      // cleared by the cleanup below, so a space change / unmount aborts the loop.
      const { invoke } = await import('@tauri-apps/api/core');
      for (let attempt = 0; attempt < 5 && active; attempt++) {
        try {
          await invoke('watch_group_call', { spaceId });
          return; // watch established
        } catch {
          // not in Tauri / node not ready — back off and retry.
          if (!active) return;
          await new Promise((r) => setTimeout(r, 500 * (attempt + 1)));
        }
      }
    })();
    return () => {
      active = false;
      // Stop driving a join into a call that may end while we're unwatched: once
      // unwatched we stop receiving presence updates, so clear the stale banner
      // entry for this space.
      groupCallBanners.clear(spaceId);
      // ZEB-360 (Cursor R5): the members cache (warmed by the presence/incoming
      // handlers) is otherwise write-once-per-space for the session. Drop this
      // space's entry on the watch boundary so re-opening the group DM re-fetches
      // membership from the CRDT — picking up any add/remove. (Identity switch is
      // handled separately: it goes through location.reload(), which wipes the
      // module-global cache entirely.)
      invalidateGroupMembers(spaceId);
      void (async () => {
        try {
          const { invoke } = await import('@tauri-apps/api/core');
          await invoke('unwatch_group_call', { spaceId });
        } catch { /* best-effort */ }
      })();
    };
  });
  // ZEB-334: when true (and no community is selected) the main feed renders the
  // private self-notes space instead of the legacy "#general" void. Defaults
  // true so a fresh, community-less user lands in Notes, not the void.
  let notesSelected = $state(true);
  // The nav row to render with active styling. When a community is
  // selected, highlight the community node; otherwise fall back to
  // the active channel/DM. Keeping these in separate $state fields
  // avoids reusing activeChannel for community ids — activeChannel
  // is consumed by message-send paths that only make sense for
  // channels/DMs.
  // ZEB-334 (Cursor PR #180): when the Notes space is selected, no real nav
  // node is active — the Notes row carries its own active state — so don't let
  // navActiveNodeId keep highlighting the last channel/community.
  let navActiveNodeId = $derived(
    notesSelected
      ? null
      : selectedCommunityId
        // ZEB-663: on the Channels view, highlight the active channel row;
        // otherwise (proposals/charter/tier3, or no channel yet) the community.
        ? (communityActiveView === 'channels' ? (selectedChannelId ?? selectedCommunityId) : selectedCommunityId)
        : activeChannel,
  );

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

  // ZEB-334: select the private self-notes space — clears any community so the
  // feed pane renders NotesView (the zero-community default).
  function selectNotes() {
    notesSelected = true;
    changeSelectedCommunity(null);
    if (appMode !== 'messages') switchMode('messages');
  }

  function handleNodeClick(id: string) {
    const node = findNode(navNodes, id);
    if (!node || node.type === 'folder') return;
    // ZEB-663: a channel row routes to its community + channel feed.
    if (node.type === 'channel') {
      if (node.parentId) {
        openCommunityChannel(node.parentId, node.id);
      } else {
        // Unreachable: setChannels always assigns parentId. A warning makes a
        // future regression (a click that visibly no-ops) discoverable.
        console.warn(`handleNodeClick: channel node ${node.id} has no parentId; ignoring click`);
      }
      return;
    }
    // ZEB-334: selecting any real space leaves the self-notes view.
    notesSelected = false;
    // ZEB-263: community nodes route to the right-pane overview placeholder
    // instead of the message feed (no channels yet — that's a later phase).
    if (node.type === 'community') {
      changeSelectedCommunity(id);
      void refreshCommunityMembers(id);
      // ZEB-663: clearing is per-channel now (openCommunityChannel + the
      // selection-resolution effect clear the channel you land on). The
      // community rollup decrements naturally as its channels clear — zeroing
      // the community node directly here would break the sum invariant.
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
    // ZEB-662: opening a channel clears its unseen-mention indicator.
    navService.clearMention(node.id);
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
      // ZEB-666: opening the thread clears its unread badge (open-clears-all;
      // stamps the cursor past maxSeen so replays don't resurrect it).
      dmUnread?.markThreadRead(node.id);
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

  function handleExportRequested() {
    // The backup wizard lives in IdentityPanel (Settings → Account). Mark the
    // request reactively (not a fire-and-forget event) so it survives until that
    // panel observes it — robust to the Account tab not being open, or to
    // collapsed/mobile layout where Settings only renders once the window is
    // widened — then route there so the wizard is visible when it opens.
    backupExportRequest.pending = true;
    appMode = 'messages';
    showSettings = true;
    settingsTab = 'account';
  }
</script>

<svelte:window bind:innerWidth />

<!--
  ZEB-298 PR 2 Task 10 — ToastHost mounts unconditionally at the top of
  the template so toast notifications (e.g., delegate-on-behalf events)
  appear regardless of which view is active. The host is `position:
  fixed` so it floats above whatever Layout renders.
-->
<ToastHost />

<!--
  ZEB-352 Voice V4: the global DM-call surfaces. The incoming-call banner and
  the in-call bar live at the app root (above Layout) so a call is reachable
  regardless of which view is active. Both are gated on the lazily-built
  callSession. `accept()` takes the spaceId carried in the incoming-call event.
-->
{#if callSession}
  <IncomingCallToast
    {incomingCall}
    onAccept={() => {
      // accept() uses the spaceId pinned by onIncoming() — guard only on there
      // being an incoming call to acknowledge.
      if (incomingCall) swallow(leaveOtherVoiceThen(() => callSession!.accept()));
    }}
    onDecline={() => swallow(callSession!.decline('user'))}
  />
  <CallInProgressBar session={callSession} onEnd={() => swallow(callSession!.end())} />
{/if}

<!--
  ZEB-360 T13: the global GROUP-DM call surfaces, mirroring the 1:1 pair above.
  The incoming-group-call ring toast reuses IncomingCallToast (body extended with
  the group name via the model below); the in-call bar is a sibling GroupCallBar
  iterating the participant roster. Both live at the app root so a group call is
  reachable regardless of which view is active.
-->
{#if groupCall}
  <IncomingCallToast
    incomingCall={groupIncomingCall
      ? {
          callId: groupIncomingCall.callId,
          callerName: `${groupIncomingCall.callerName} · ${groupIncomingCall.groupName}`,
          ...(groupIncomingCall.callerAvatarUrl ? { callerAvatarUrl: groupIncomingCall.callerAvatarUrl } : {}),
        }
      : null}
    onAccept={() => { if (groupIncomingCall) swallow(leaveOtherVoiceThen(() => groupCall!.accept())); }}
    onDecline={() => swallow(groupCall!.decline())}
  />
  <GroupCallBar
    session={groupCall}
    groupName={navService.nodes.find((n) => n.id === $groupCallState?.spaceId)?.name}
  />
{/if}

<!--
  ZEB-236: DM-invite corner toast, mirroring the IncomingCallToast pair
  above — lives at the app root so a pending invite is reachable regardless
  of which view is active. Shows only the oldest not-yet-"Later"-dismissed
  pending invite; App owns the queue (dmInviteQueue), the component is pure
  presentational.
-->
{#if dmInviteQueue.length > 0}
  {#key dmInviteQueue[0].spaceIdHex}
    <!-- Keyed remount per invite: a fresh component instance resets the
         internal `busy` flag (so "Later" during an in-flight accept/decline
         can't disable the NEXT invite's buttons) and replays the entrance
         transition when the queue head changes. -->
    <DmInviteToast
      invite={dmInviteQueue[0]}
      onAccept={() => handleDmInviteAccept(dmInviteQueue[0].spaceIdHex)}
      onDecline={() => handleDmInviteDecline(dmInviteQueue[0].spaceIdHex)}
      onLater={() => {
        laterDismissed.add(dmInviteQueue[0].spaceIdHex);
        dmInviteQueue = dmInviteQueue.filter((i) => !laterDismissed.has(i.spaceIdHex));
      }}
    />
  {/key}
{/if}

<BackupStalenessWarning ownerId={selfOwnerId} onExportRequested={handleExportRequested} />

<div class="app-shell">
{#if ownerIdentityState === 'present'}
  <BackupReminderBanner ownerId={selfOwnerId} />
{/if}
<Layout {collapsed} {showSettings} mode={appMode} mailSelected={selectedMailCid !== null} bind:mediaPanelOpen bind:mediaPanelWidth>
  {#snippet nav()}
    <div class="nav-with-dm-create">
      <NavPanel
        nodes={navNodes}
        {collapsed}
        activeNodeId={navActiveNodeId}
        onNodeClick={handleNodeClick}
        onSettingsClick={() => { showSettings = !showSettings; }}
        settingsActive={showSettings}
        profileLookup={(addr) => navService.profileLookup(addr)}
        presenceOnline={(node) => {
          // ZEB-600: reading presenceVersion registers the reactive dependency
          // so every NavNodeRow dot recomputes when a beacon lands.
          void presenceVersion;
          if (node.type === 'community') {
            return presenceService.hasOthersOnline(node.id, selfOwnerId ?? '');
          }
          if ((node.type === 'dm' || node.type === 'group-chat') && node.peer) {
            return presenceService.isOnlineAnywhere(node.peer.address);
          }
          return false;
        }}
        proposalCount={(node) => {
          // Reading proposalCountVersion registers the reactive dependency.
          void proposalCountVersion;
          return proposalCountService.countFor(node.id);
        }}
        onSelectProposals={openCommunityProposals}
        proposalsActiveFor={communityActiveView === 'proposals' ? selectedCommunityId : null}
        canManageChannels={canManageSelectedCommunityChannels}
        onAddChannel={() => { showCreateChannelDialog = true; }}
        onRenameChannel={openRenameChannel}
        onDeleteChannel={openDeleteChannel}
        identity={identityChipInfo}
        showConnectionStatus={true}
        onModeChange={switchMode}
        {appMode}
        contentItems={allFileContents}
        {contributionSummary}
        onManageBuddies={() => {
          manageBuddiesOpen = true;
          void refreshStorageBuddies();
        }}
        {fileSection}
        {currentFolderCid}
        onFolderSelect={handleNavigateFolder}
        filters={fileFilters}
        onFilterChange={(filters) => { fileFilters = filters; }}
        onNewDm={() => openDmCreate('dm')}
        onNewGroupDm={() => openDmCreate('group-dm')}
        onNewCommunity={() => { showCreateCommunity = true; createError = null; }}
        onRedeemInvite={() => { showRedeemInvite = true; redeemError = null; redeemUrl = ''; }}
        onBrowseLibraries={() => { libraryDirectoryOpen = true; }}
        onPostVine={() => { switchMode('vines'); showVinePublish = true; }}
        onSelectNotes={selectNotes}
        notesActive={notesSelected && !selectedCommunityNode}
        onSubmitFeedback={() => (feedbackModalOpen = true)}
        onShowAbout={() => (aboutModalOpen = true)}
        onOpenDocs={async () => {
          try {
            const { open: shellOpen } = await import('@tauri-apps/plugin-shell');
            await shellOpen('https://github.com/zeblithic/harmony-client/blob/main/README.md');
          } catch (e) {
            const msg = e instanceof Error ? e.message : String(e);
            console.warn('[zeb-555] failed to open docs:', msg);
          }
        }}
      />
      {#if !collapsed && appMode === 'messages'}
        <button
          type="button"
          class="new-dm-button"
          onclick={() => openDmCreate('dm')}
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
        membersLoading={membersLoading}
        ownAddress={selfOwnerId ?? ''}
        myPower={myCommunityPower}
        isDegraded={isCurrentCommunityDegraded}
        sharedInProfile={sharedInProfileByCommunity.get(selectedCommunityNode.id) ?? false}
        {communityService}
        {channelMessageService}
        {trustService}
        {navService}
        {votingAdapter}
        {selectedChannelId}
        bind:activeView={communityActiveView}
        onForkSuccess={(forkSpaceId) => {
          // ZEB-285: navigate to the newly created fork community and
          // refresh its member roster (matching create/join/redeem flows).
          changeSelectedCommunity(forkSpaceId);
          void refreshCommunityMembers(forkSpaceId);
        }}
        onSelectCommunity={(spaceId) => {
          // ZEB-287 Phase 2: route the lineage-tree click into the
          // changeSelectedCommunity primitive.
          changeSelectedCommunity(spaceId);
          void refreshCommunityMembers(spaceId);
        }}
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
            // ZEB-665: drop unread session state so a same-session rejoin
            // re-seeds instead of reusing stale seeded marks/counts.
            channelUnread?.onCommunityRemoved(leavingId);
            // ZEB-334 (Cursor PR #180): after leaving, fall back to the private
            // Notes default rather than the legacy #general void — clearing the
            // community alone would drop through to TextFeed on 'general'.
            selectNotes();
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
        {resolveCard}
        {resolveNickname}
        {isOnline}
        selfInvisible={!presenceVisible}
        {subscribeVisibleCards}
        {unsubscribeCards}
        onOpenCard={openMemberCard}
        {voiceSession}
        onBeforeVoiceJoin={async () => {
          // ZEB-352 D12 (reverse): tear down an active DM call before joining
          // channel voice, so the two media engines never run concurrently.
          if (callSession && get(callSession.state).phase !== 'idle') {
            await callSession.end().catch(() => {});
          }
          // ZEB-360 D6: also tear down an in-progress GROUP call before joining
          // community voice. 'incoming' is excluded — it's only a ring toast with
          // no media engine yet, so it can't run alongside community voice.
          if (groupCall) {
            const gp = get(groupCall.state).phase;
            if (gp !== 'idle' && gp !== 'incoming') {
              await groupCall.leave().catch(() => {});
            }
          }
        }}
      />
    {:else if notesSelected}
      <NotesView
        {notesService}
        ownerId={selfOwnerId ?? ''}
        displayName={myProfile.displayName}
      />
    {:else}
      <TextFeed
        messages={mainFeedMessages}
        {collapsed}
        channelName={activeChannelName}
        channelType={activeChannelType}
        channelId={activeChannel}
        onStartCall={(spaceId) => { if (callSession) swallow(leaveOtherVoiceThen(() => callSession!.placeCall(spaceId))); }}
        onStartGroupCall={(spaceId) => swallow(placeGroupCall(spaceId))}
        onJoinGroupCall={(spaceId) => swallow(joinGroupCall(spaceId))}
        {groupCallActive}
        {groupCallSelf}
        {groupCallBusy}
        {groupCall}
        groupCallInvoke={groupCallInvoke}
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
    <MessagesRail
      communityId={selectedCommunityNode?.id ?? null}
      votingAdapter={votingReady ? votingAdapter : undefined}
      myAddr={selfOwnerId ?? ''}
      {communityMembers}
      onViewAllProposals={openCommunityProposals}
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
    <SettingsPanel
      profile={myProfile}
      onProfileSave={handleProfileSave}
      ownerIdHex={selfOwnerId ?? undefined}
      {notificationService}
      {trustService}
      peers={knownPeers}
      {communities}
      onClose={() => { showSettings = false; }}
      onTrustChange={handleTrustChange}
      {friendService}
      {friendCardService}
      {dmInviteService}
      {communityService}
      onOpenCard={openMemberCard}
      bind:activeTab={settingsTab}
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
      onDelete={(vine) => vineService.deleteVine(vine)}
      onFollow={handleVineFollow}
      onUnfollow={handleVineUnfollow}
      getReaction={vineGetReaction}
      onToggleLike={handleVineToggleLike}
      onViewOriginal={handleViewOriginal}
      playTarget={viewOriginalTarget}
      onPlayTargetConsumed={() => { viewOriginalTarget = null; }}
      resolveVideo={resolveVideoFn}
      ownAddress={myAddress || undefined}
      getShareFollows={tauriAdapter ? async () => {
        const s = await tauriAdapter!.invoke('get_vine_settings', {}) as { shareFollows: boolean };
        return s.shareFollows;
      } : undefined}
      onSetShareFollows={tauriAdapter ? async (on: boolean) => {
        await tauriAdapter!.invoke('set_vine_settings', { shareFollows: on });
      } : undefined}
    />
    {#if showVinePublish}
      <VinePublishDialog onPublish={handleVinePublish} onPickVideo={isTauri() ? handlePickVineVideo : undefined} resolveVideo={resolveVideoFn} onClose={() => showVinePublish = false} />
    {/if}
  {/snippet}
  {#snippet fileBrowser()}
    <FileBrowser
      service={fileManagerService}
      adapter={tauriAdapter}
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
        usedByVines={selectedFileVineCount}
        confirmationOverrides={fileManagerService.settings.confirmationOverrides}
        onTierChange={handleFileTierChange}
        onPublish={handleFilePublish}
        onRelease={handleFileRelease}
        onBurn={handleFileBurn}
        onArchive={handleFileArchive}
        onPin={handleFilePin}
        onUnpin={handleFileUnpin}
        onExport={handleFileExport}
        onSetBackup={(sidecarId, backup) => fileManagerService.setBackupFlag(sidecarId, backup)}
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
  {#snippet mintLedger()}
    {#if tauriAdapter}
      <MintLedger adapter={tauriAdapter} />
    {:else}
      <p style="padding: 1rem;">Mint requires Tauri — run via <code>npm run tauri dev</code>.</p>
    {/if}
  {/snippet}
  {#snippet networkPanel()}
    <NetworkHealthView />
  {/snippet}
</Layout>
</div>

{#if popoverProfile && profileBroadcastService}
  <ProfilePopover
    profile={popoverProfile}
    x={popoverX}
    y={popoverY}
    onClose={closePopover}
    ownAddress={myAddress}
    ownerIdHex={popoverProfile?.address === myProfile.address
      ? (selfOwnerId ?? undefined)
      : undefined}
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

{#if popoverCard}
  <ProfilePopover
    mode="owner-card"
    card={popoverCard}
    x={popoverCardX}
    y={popoverCardY}
    onClose={() => (popoverCard = null)}
    onViewProfile={(ownerIdHex) => {
      // ZEB-345 Task 10: open the right-side long-form profile panel and
      // dismiss the popover so the two surfaces don't overlap.
      openProfileOwnerId = ownerIdHex;
      popoverCard = null;
    }}
  />
{/if}

{#if openProfileOwnerId}
  <!-- ZEB-345 Task 10: right-side long-form profile panel. Rendered as a
       fixed-position sibling of Layout (like the popovers) so it floats over
       the right column regardless of the active mode. The resolved card comes
       from the same MemberCardService resolution the popover used; reading
       cardVersion (via resolveCard) keeps the header live as the card fills
       in, and docVersion re-resolves the doc once fetch_profile_doc lands. -->
  <div class="profile-panel-host">
    <ProfilePanel
      ownerIdHex={openProfileOwnerId}
      card={resolveCard(openProfileOwnerId) ?? { displayName: '' }}
      resolver={profilePageResolver}
      docVersion={profileDocVersion}
      onClose={() => (openProfileOwnerId = null)}
      onHarmonyLink={routeHarmonyLink}
    />
  </div>
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
        profiles={pickerContacts}
        friendSourced={isTauri()}
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

{#if manageBuddiesOpen}
  <!-- ZEB-669 S3: storage-buddy manage sheet. Buddy rows refresh via the
       service onChange subscription while open; actions surface their own
       errors inline (backend stays the authority). -->
  <StorageBuddySheet
    buddies={storageBuddies}
    summary={contributionSummary}
    friendContacts={pickerContacts}
    onClose={() => { manageBuddiesOpen = false; }}
    onSetPledge={(addr, bytes) => storageBuddyService.setPledge(addr, bytes)}
    onRemove={(addr) => storageBuddyService.removeBuddy(addr)}
    onSetBudget={(bytes) => storageBuddyService.setSharedBudget(bytes)}
  />
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

{#if selectedCommunityId}
  <!-- ZEB-663: hoisted channel-management dialogs (were CommunityView's).
       Scoped to the selected community; power-gated by myCommunityPower. -->
  <CreateChannelDialog
    communityId={selectedCommunityId}
    {communityService}
    open={showCreateChannelDialog}
    myPower={myCommunityPower}
    onClose={() => { showCreateChannelDialog = false; }}
    onCreated={(channelId) => {
      showCreateChannelDialog = false;
      if (selectedCommunityId) openCommunityChannel(selectedCommunityId, channelId);
    }}
  />
  {#if modifyChannelTarget}
    <ModifyChannelDialog
      communityId={modifyChannelTarget.communityId}
      channel={modifyChannelTarget.channel}
      {communityService}
      open={true}
      myPower={myCommunityPower}
      onClose={() => { modifyChannelTarget = null; }}
    />
  {/if}
  {#if deleteChannelTarget}
    <TypedConfirmationModal
      title={`Delete #${deleteChannelTarget.channel.name}?`}
      description="Channel deletion is permanent. The message log persists on existing devices, but no new messages can be posted and the channel will disappear from the sidebar for everyone."
      requiredText={deleteChannelTarget.channel.name}
      confirmLabel="Delete channel"
      onConfirm={confirmDeleteChannel}
      onCancel={() => { deleteChannelTarget = null; }}
    />
  {/if}
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
        // ZEB-254: carry the pending flag so the nav node starts greyed
        // when the invite-only join countersign hasn't arrived yet.
        navService.addOrUpdateNavSpace({
          action: 'added',
          spaceId: dto.communityId,
          kind: 'community',
          name: dto.communityName,
          members: [],
          parentId: null,
          pending: dto.pending ? true : undefined,
        });
        showRedeemInvite = false;
        redeemUrl = '';
        // ZEB-254: show a transient status message; auto-clear after 6 s.
        // Cancel any in-flight timer so back-to-back redeems don't clear
        // the second message early.
        redeemStatusMsg = dto.pending
          ? `Join request sent. "${dto.communityName}" will unlock once an admin approves.`
          : `You're in "${dto.communityName}"!`;
        if (redeemStatusTimer !== null) clearTimeout(redeemStatusTimer);
        redeemStatusTimer = setTimeout(() => {
          redeemStatusMsg = '';
          redeemStatusTimer = null;
        }, 6000);
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

{#if redeemStatusMsg}
  <!-- ZEB-254: transient join-status banner; auto-clears after 6 s. -->
  <div class="redeem-status-banner" role="status" aria-live="polite">
    {redeemStatusMsg}
  </div>
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
        onOpenJoinIroh={(inviteUrl) => openJoinIroh(inviteUrl)}
        onJoinedUi={async (dto) => {
          // Open-community cross-WAN first-contact "joined" follow-up: the
          // `connectivity_open_join_iroh` IPC already committed membership
          // backend-side, so this runs ONLY the UI side-effects (nav-update,
          // community switch, member refresh) — it must NOT re-invoke
          // `join_open_community`, which would be a redundant second backend
          // join. Mirrors the side-effects in `onJoin` below, minus the
          // backend `joinOpenCommunity` call.
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

{#if availableUpdate}
  <!-- ZEB-328: update notification toast. Non-blocking; shown after startup
       check. Dismissed via Later (session-only) or Skip (persisted to
       localStorage so the same version is suppressed on next launch). -->
  <UpdateAvailableToast
    update={availableUpdate}
    onDismiss={() => (availableUpdate = null)}
  />
{/if}

<!-- ZEB-338: first-run welcome hard-gate. Visibility is owned by
     ownerIdentityState === 'missing' (backend keychain authoritative): shown
     only when start_node succeeded and reported no owner identity, not
     dismissable, closed by onMinted on successful mint. A start_node *failure*
     shows the startup-error overlay below instead — never this mint gate. -->
<WelcomeModal open={showWelcomeModal} {onMinted} />
<NamePromptModal
  open={showNamePrompt}
  ownerIdHex={selfOwnerId}
  onSave={handleNamePromptSave}
  onSkip={() => { showNamePrompt = false; }}
/>

<!-- ZEB-338 / PR #169: startup-error overlay. When start_node fails we must not
     show the mint gate (an existing-but-unloaded identity would deadlock it),
     so surface an honest error + retry. Reload re-runs the whole boot, which
     re-invokes start_node and re-classifies the gate. -->
{#if ownerIdentityState === 'error'}
  <div class="modal-overlay" data-testid="startup-error-backdrop" role="presentation">
    <div
      bind:this={startupErrorModalEl}
      class="modal-content startup-error-modal"
      data-testid="startup-error-modal"
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="startup-error-title"
      tabindex="-1"
    >
      <h2 id="startup-error-title">Couldn't start Harmony</h2>
      <p>
        The Harmony node failed to start, so the app can't load your identity
        yet. Your data is untouched — this is usually a temporary problem (for
        example, another copy of Harmony is still running).
      </p>
      {#if startNodeError}
        <p class="error" data-testid="startup-error-detail">{startNodeError}</p>
      {/if}
      <div class="startup-error-actions">
        <button
          class="primary"
          data-testid="startup-error-retry"
          onclick={() => location.reload()}
        >
          Retry
        </button>
      </div>
    </div>
  </div>
{/if}

<!-- ZEB-668 S2: terminal state for a revoked device. No retry — the
     revocation is monotonic (remove-wins CRDT). Honesty rule: the copy says
     exactly what is and is not affected (local data is NOT wiped — §1
     non-goal), and how to use Harmony on this hardware again. -->
{#if ownerIdentityState === 'revoked'}
  <div class="modal-overlay" data-testid="device-revoked-backdrop" role="presentation">
    <div
      bind:this={revokedModalEl}
      class="modal-content startup-error-modal"
      data-testid="device-revoked-modal"
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="device-revoked-title"
      tabindex="-1"
    >
      <h2 id="device-revoked-title">This device was removed from your account</h2>
      <p>
        This device's enrollment was revoked — by you here, or from another of
        your devices. It no longer syncs with your other devices or posts to
        your communities as you.
      </p>
      <p>
        Your local data on this device has not been deleted. To use Harmony
        here again, pair this device from another of your devices, or restore
        an identity from a recovery phrase.
      </p>
    </div>
  </div>
{/if}


<FeedbackModal
  open={feedbackModalOpen}
  onDismiss={() => (feedbackModalOpen = false)}
/>

<AboutModal
  open={aboutModalOpen}
  onDismiss={() => (aboutModalOpen = false)}
/>

<style>
  :global(.text-message) {
    transition: background 0.3s ease;
  }

  :global(.text-message.highlight) {
    background: color-mix(in srgb, var(--accent) 15%, transparent) !important;
  }

  .file-detail-empty {
    display: flex;
    align-items: center;
    justify-content: center;
    height: 100%;
    color: var(--text-muted);
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
    background: color-mix(in srgb, var(--library-accent) 15%, transparent);
    color: var(--text-primary);
    border: none;
    border-top: 1px solid var(--border);
    cursor: pointer;
    font-size: 13px;
    text-align: center;
  }
  .new-dm-button:hover {
    background: color-mix(in srgb, var(--library-accent) 30%, transparent);
  }
  .modal-overlay {
    position: fixed;
    inset: 0;
    background: var(--overlay);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 100;
  }
  .modal-content {
    background: var(--bg-secondary);
    border-radius: 8px;
    box-shadow: 0 4px 24px var(--shadow-strong);
  }

  /* ZEB-345 Task 10: floating host for the long-form profile panel. Pins the
     320px panel to the right edge, full viewport height, above the layout. The
     panel supplies its own width/background/border-left. */
  .profile-panel-host {
    position: fixed;
    top: 0;
    right: 0;
    height: 100vh;
    z-index: 90;
    box-shadow: -4px 0 16px var(--shadow-mid);
  }

  /* ZEB-254: transient join-status banner (pending vs joined).
     ZEB-610 (Commons G): sage / --primary-soft tint with e2 elevation. */
  .redeem-status-banner {
    position: fixed;
    bottom: 20px;
    left: 50%;
    transform: translateX(-50%);
    background: var(--primary-soft);
    border: 1px solid var(--primary-border);
    border-radius: 8px;
    padding: 10px 18px;
    color: var(--primary-deep);
    font-size: 0.875rem;
    box-shadow: var(--shadow-e2);
    z-index: 1000;
    pointer-events: none;
    /* R3 (M2): allow wrapping so long community names don't overflow the
       viewport. `white-space: nowrap` collapses to a single line and would
       push the banner past the screen edge for any community name longer
       than ~30 chars. `overflow-wrap: anywhere` breaks at any character if
       a single token is wider than the box (unbroken URLs, etc.). The
       max-width keeps the banner constrained to ~viewport width minus
       gutters even when wrapping kicks in. */
    white-space: normal;
    overflow-wrap: anywhere;
    max-width: calc(100% - 40px);
  }

  /* ZEB-338: backup-reminder banner overlay. Below modal (1000) + help
     overlay; above app chrome. */
  /* ZEB-406: app-shell hosts the optional backup banner in normal flow above the
     main layout, so the banner reserves height instead of overlaying + intercepting
     the top toolbar. */
  .app-shell {
    display: flex;
    flex-direction: column;
    height: 100vh;
  }

  /* ZEB-338 / PR #169: startup-error overlay (start_node failed).
     ZEB-610 (Commons G): Commons chrome — overrides the shared .modal-content
     base (surface-raised card, border, radius 10, e3 elevation) without
     touching that shared rule; sage retry, danger detail line preserved. */
  .startup-error-modal {
    background: var(--surface-raised);
    color: var(--text-primary);
    font-family: var(--font-ui);
    padding: 1.5rem;
    border: 1px solid var(--border-default);
    border-radius: 10px;
    box-shadow: var(--shadow-e3);
    max-width: 520px;
    width: 90%;
  }
  .startup-error-modal h2 {
    margin: 0 0 1rem;
    font-family: var(--font-display);
    font-size: 1.35rem;
    font-weight: 600;
    letter-spacing: -0.01em;
  }
  .startup-error-modal p {
    margin: 0 0 1rem;
    line-height: 1.5;
  }
  .startup-error-modal .error {
    color: var(--danger);
    font-family: var(--font-mono);
    font-size: 0.85rem;
    word-break: break-word;
  }
  .startup-error-actions {
    display: flex;
    gap: 0.5rem;
    margin-top: 0.5rem;
  }
  .startup-error-actions button {
    padding: 0.5rem 1rem;
    border: 1px solid var(--border-default);
    background: var(--bg-tertiary);
    color: var(--text-primary);
    border-radius: 6px;
    font-family: var(--font-ui);
    cursor: pointer;
  }
  .startup-error-actions button.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--on-accent);
  }

</style>
