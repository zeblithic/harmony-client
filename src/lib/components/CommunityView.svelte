<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { get } from 'svelte/store';
  import type { CommunityService, ChannelInfo, PreForkSnapshotDto } from '../community-service';
  import type { ChannelMessageService } from '../channel-message-service';
  import { POWER_THRESHOLDS, type CommunityMember, type CommunityGovernance } from '../types';
  import { resolveMentionLabel } from '../mention-render';
  import type { TrustService } from '../trust-service';
  import type { NavService } from '../nav-service';
  import ChannelMessageFeed from './ChannelMessageFeed.svelte';
  import VoiceChannelView from './VoiceChannelView.svelte';
  import TownHallView from './TownHallView.svelte';
  import ChannelMembersPanel from './ChannelMembersPanel.svelte';
  import CommunityMembersPanel from './CommunityMembersPanel.svelte';
  import CommunitySettingsPanel from './CommunitySettingsPanel.svelte';
  import RecoveryBanner from './RecoveryBanner.svelte';
  import CommunityProposalsPanel from './CommunityProposalsPanel.svelte';
  import Tier3ProposalPanel from './Tier3ProposalPanel.svelte';
  import CharterView from './CharterView.svelte';
  import type { VotingAdapter } from '../voting-adapter';
  import type { ResolvedCard } from '../member-card-service';
  import type { VoiceSession } from '../voice-session';

  let {
    communityId,
    communityName,
    communityKind,
    myPower,
    ownAddress,
    members,
    membersLoading = false,
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
    onSelectCommunity,
    votingAdapter,
    resolveCard,
    resolveNickname,
    isOnline,
    selfInvisible = false,
    subscribeVisibleCards,
    unsubscribeCards,
    onOpenCard,
    voiceSession,
    onBeforeVoiceJoin,
    selectedChannelId,
    activeView = $bindable('channels'),
  }: {
    communityId: string;
    communityName: string;
    communityKind: 'open' | 'invite-only' | 'unknown';
    myPower: number;
    ownAddress: string;
    members: CommunityMember[];
    /** ZEB-553 item 11: forwarded to ChannelMembersPanel so a community switch
     *  shows a loading affordance instead of a bare "0 members" while the
     *  roster is being fetched. */
    membersLoading?: boolean;
    isDegraded: boolean;
    sharedInProfile: boolean;
    communityService: CommunityService;
    channelMessageService: ChannelMessageService;
    trustService?: TrustService;
    /** ZEB-341: optional card resolver — undefined until owner identity loads. */
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
    /** ZEB-432: optional local friend-nickname resolver (ZEB-419), preferred
     *  over the profile-card name in the roster and on message authors. */
    resolveNickname?: (ownerIdHex: string) => string | undefined;
    /** ZEB-537: optional online-presence resolver for the members roster.
     *  Pure consumer of the parent's PresenceService (same contract as
     *  resolveCard). Undefined until the presence subscription is wired. */
    isOnline?: (ownerIdHex: string) => boolean;
    /** ZEB-600: true when the viewer has "Appear offline" on — forwarded to the
     *  members panel so the self row shows the hollow "invisible" dot. */
    selfInvisible?: boolean;
    /** ZEB-341 Task 8: subscribe to cross-peer cards for the visible member set. */
    subscribeVisibleCards?: (ownerIdHexes: string[]) => void;
    /** ZEB-341 Task 8: tear down all card subscriptions when the panel unmounts. */
    unsubscribeCards?: () => void;
    /** ZEB-341: open the owner_id card popover for a clicked member/author. */
    onOpenCard?: (
      payload: { ownerIdHex: string; displayName: string; statusText: string; power?: number; membershipStatus?: string },
      ev: MouseEvent,
    ) => void;
    /** ZEB-291 Phase 2: connected VotingAdapter. When present, a
     *  Proposals tab appears next to Channels — switches the middle
     *  column to the Tier 2 governance panel. Optional so existing
     *  CommunityView consumers (App.svelte mounts that haven't been
     *  updated yet) keep working with the channels-only view. */
    votingAdapter?: VotingAdapter;
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
    /** ZEB-287 Phase 2: called when the user clicks a clickable row in the
     *  fork lineage tree. Routes the target SpaceId through the parent's
     *  changeSelectedCommunity primitive. */
    onSelectCommunity?: (spaceId: string) => void;
    /** ZEB-351 Voice V3: the app-lifetime singleton voice session. Threaded
     *  into VoiceChannelView for voice channels. Null until App.svelte's
     *  get_self_voice_identity IPC resolves — the voice routing is guarded so
     *  the brief pre-ready window simply shows nothing. */
    voiceSession?: VoiceSession | null;
    /** ZEB-352 D12: forwarded to VoiceChannelView — the app tears down any
     *  active DM call before this community channel's voice join proceeds. */
    onBeforeVoiceJoin?: () => Promise<void>;
    /** ZEB-663: the App-owned selected channel id (single source of truth).
     *  Drives which channel's feed renders. Selection is driven by the nav
     *  channel rows (App.openCommunityChannel). */
    selectedChannelId: string | null;
    /** ZEB-606: which middle-column view is active. Bindable so App can
     *  deep-link (nav proposals row / Assembly rail "View all"). Default
     *  'channels' preserves the ZEB-291 behavior for non-binding parents.
     *  ZEB-608 adds 'charter'. */
    activeView?: 'channels' | 'proposals' | 'tier3' | 'charter';
  } = $props();

  let channels = $state<ChannelInfo[]>([]);
  let settingsModalOpen = $state(false);
  let communityMembersPanelOpen = $state(false);
  let membersPanelCollapsed = $state(false);
  let prevOnChannelConfigChanged: typeof communityService.onChannelConfigChanged;
  // ZEB-285: fork lineage — loaded lazily when the settings modal first opens.
  let lineage = $state<{
    originalCommunityName: string | null;
    forkedAtMs: number;
    snapshotMessageCount: number;
  } | null | undefined>(undefined); // undefined = not yet fetched

  // ZEB-287 Phase 2: multi-hop lineage + descendants — loaded lazily.
  let phase2Lineage = $state<import('../types').CommunityLineageDto | null>(null);
  let descendants = $state<import('../types').ForkDescendantDto[]>([]);

  // ZEB-287 Phase 2: derive locally-known community SpaceIds from the
  // NavService snapshot. Used by ForkLineageTree to gate clickability
  // of ancestor + descendant rows. Recomputes when navService.nodes changes.
  let localCommunityIds = $derived(
    new Set<string>(navService.nodes.filter((n) => n.type === 'community').map((n) => n.id)),
  );

  // ZEB-774: roster-DTO displayName resolver (list_community_members' displayName,
  // ZEB-777) threaded into the shared mention ladder as the rung below the live
  // card — so a peer the roster already named degrades to that name rather than
  // raw hex while their profile card is still propagating. Same source the member
  // panel uses (MemberRow's `member.displayName`). Recomputes with the roster;
  // callers read it inside their own reactive contexts, so labels re-resolve as
  // names arrive.
  let rosterNameByOwner = $derived(new Map(members.map((m) => [m.address, m.displayName])));
  function resolveRosterName(ownerId: string): string | undefined {
    return rosterNameByOwner.get(ownerId) ?? undefined;
  }

  // ZEB-612 S5 (CodeRabbit #443): memoized mention-candidate list shared by
  // the text feed and the townhall backchannel — recomputes only when the
  // member roster (or a resolver) changes, not on every re-render.
  let joinedMentionCandidates = $derived(
    members
      .filter((m) => m.status === 'joined')
      .map((m) => ({
        ownerId: m.address,
        label: resolveMentionLabel(m.address, resolveNickname, resolveCard, resolveRosterName),
      })),
  );

  // ZEB-285 Task 11: pre-fork snapshot for unified timeline rendering.
  // Loaded once per community view. null = non-fork community; undefined = not yet loaded.
  let preForkSnapshot = $state<PreForkSnapshotDto | null | undefined>(undefined);

  // ZEB-608 D1 / ZEB-251: per-community governance snapshot (admin quorum +
  // per-community power thresholds). Loaded on every community switch,
  // stale-guarded like the other per-community loads. null until the IPC
  // resolves or on failure. The charter treats null as "not loaded" (shows
  // '…', staying honest about live state); the settings panel keeps its
  // operational `?? 1` / `?? POWER_THRESHOLDS.*` fallback (its long-standing
  // contract).
  let governance = $state<CommunityGovernance | null>(null);

  // ZEB-251: per-community power thresholds derived from the governance
  // snapshot, falling back field-by-field to the global POWER_THRESHOLDS
  // consts (both when governance hasn't loaded yet and — defensively — if a
  // caller ever supplies a partial governance object). An un-customized
  // community's thresholds always equal these consts, so this never changes
  // gate behavior before ZEB-251 governance data is available.
  let thresholds = $derived({
    invite: governance?.invite ?? POWER_THRESHOLDS.invite,
    kick: governance?.kick ?? POWER_THRESHOLDS.kick,
    setPower: governance?.setPower ?? POWER_THRESHOLDS.setPower,
  });

  $effect(() => {
    const cid = communityId;
    // Per-run cancellation flag (matches the preForkSnapshot effect below). A
    // bare `cid !== communityId` compare is NOT enough: an A→B→A switch returns
    // to the same id, so a late-resolving first-A fetch would pass the compare
    // and clobber the re-entered A's fresh value. The cleanup fires on every
    // re-run, so only the latest visit's completion is ever applied (PR #410
    // Qodo; same guard class as PR #97/#144/#357).
    let cancelled = false;
    governance = null;
    void communityService
      .getCommunityGovernance(cid)
      .then((g) => {
        if (cancelled) return;
        governance = g ?? null;
      })
      .catch(() => {
        if (cancelled) return;
        governance = null;
      });
    return () => {
      cancelled = true;
    };
  });

  // ZEB-663: the active channel is derived from the App-owned selection.
  let activeChannel = $derived(channels.find((c) => c.channelId === selectedChannelId) ?? null);

  async function refreshChannels() {
    const list = await communityService.listChannels(communityId);
    channels = list.filter((c) => c.deletedAt === undefined);
  }

  onMount(() => {
    // Hook channel-config callback ONCE per component lifetime. Chain prior
    // so we don't clobber App.svelte's listener if it had one.
    prevOnChannelConfigChanged = communityService.onChannelConfigChanged;
    communityService.onChannelConfigChanged = (cid, action, channelId, name, writePower) => {
      prevOnChannelConfigChanged?.(cid, action, channelId, name, writePower);
      if (cid !== communityId) return;
      // ZEB-663: keep only the feed-list refresh here — App owns the
      // selected-channel fallback (its resolution effect re-picks when the
      // active channel is removed from the nav children).
      void refreshChannels().catch((e) => {
        const msg = e instanceof Error ? e.message : String(e);
        console.warn('CommunityView: refreshChannels failed in onChannelConfigChanged:', msg);
      });
    };
  });

  // Per-community initialization runs whenever `communityId` changes (or on
  // first mount). Without this $effect, switching communities reuses the
  // component instance but leaves it pinned to the previous community's
  // channel list (Cursor Bugbot HIGH on PR #97 round 1).
  $effect(() => {
    const cid = communityId;
    let cancelled = false;
    // Reset snapshot and lineage state on community switch so stale data from
    // the previous community never briefly shows for the new one.
    // (Fix: PR #122 round-4, CodeRabbit inline — lineage was not cleared.)
    preForkSnapshot = undefined;
    lineage = undefined; // undefined = not yet fetched for this community
    // ZEB-287 Phase 2: reset multi-hop lineage state on community switch.
    phase2Lineage = null;
    descendants = [];

    void (async () => {
      try {
        // ZEB-285 Task 11: load pre-fork snapshot for unified timeline.
        // Fire-and-forget; result flows into snapshotMessages prop on ChannelMessageFeed.
        communityService.getPreForkSnapshot(cid).then((snapshot) => {
          if (!cancelled) preForkSnapshot = snapshot;
        }).catch(() => {
          if (!cancelled) preForkSnapshot = null;
        });

        // ZEB-663: refresh the feed channel list (selection is App-owned —
        // its resolution effect picks the persisted/default channel).
        await refreshChannels();
      } catch (e) {
        console.warn('CommunityView: refreshChannels failed in community $effect:', e);
      }
    })();

    return () => {
      cancelled = true;
    };
  });

  // ZEB-341: drive cross-peer card subscriptions for the whole community view,
  // not just the (transient) members-panel overlay. CommunityView stays mounted
  // for as long as a community is selected, so anchoring the subscription
  // lifecycle here makes message-author names in ChannelMessageFeed resolve in
  // the channel view itself — and keep updating — regardless of whether the
  // members overlay is open. Previously this lived only in the overlay panel, so
  // author names never resolved unless the user opened it (Cursor Bugbot,
  // PR #171).
  //
  // Scope: currently-JOINED members only. They are the channel-view-visible set
  // (message authors + the channel members list); banned members surface only in
  // the members-overlay's collapsible section and are intentionally not
  // subscribed here. This bounds the active subscription count by *live*
  // membership rather than lifetime ban accumulation (spec §7 "bound the active
  // subscription count"; CodeRabbit "subscribe only to visible rows"). The
  // service diffs internally (subscribes new owner_ids, unsubscribes departed)
  // and excludes self, so re-running on every `members` change is idempotent;
  // `member.address` is the lowercase owner_id hex. Teardown is in onDestroy
  // (fires on view change / leaving the community).
  $effect(() => {
    const joinedOwnerIds = members
      .filter((m) => m.status === 'joined')
      .map((m) => m.address);
    subscribeVisibleCards?.(joinedOwnerIds);
  });

  // ZEB-351 Voice V3: leave the voice session when the user navigates away from
  // the channel it's connected to. The session is a single app-lifetime
  // instance, so switching to a different channel (text or another voice
  // channel) — or switching communities — must tear down the live mic/transport.
  // We compare the newly-active channel against the session's CONNECTED channel
  // (read from its state store); if they differ while connected, leave.
  // Reading `selectedChannelId` registers the reactive dependency so this re-runs
  // on every channel switch. We deliberately do NOT depend on the store value
  // reactively (it isn't a rune) — a one-shot `get()` at switch time is the
  // intent: "on navigation, if we're parked on a now-unselected voice channel,
  // disconnect."
  $effect(() => {
    const nowActive = selectedChannelId;
    if (!voiceSession) return;
    const vs = get(voiceSession.state);
    if (vs.phase !== 'idle' && vs.channel !== null && vs.channel !== nowActive) {
      void voiceSession.leave().catch(() => {});
    }
  });

  onDestroy(() => {
    communityService.onChannelConfigChanged = prevOnChannelConfigChanged;
    // ZEB-341: tear down all card subscriptions + the poll loop when the
    // community view goes away (switching to a non-community space / closing).
    unsubscribeCards?.();
    // ZEB-351: best-effort leave on unmount (switching to a non-community
    // space / closing the community view) so a live voice session doesn't
    // outlive the view that owns its UI.
    if (voiceSession && get(voiceSession.state).phase !== 'idle') {
      void voiceSession.leave().catch(() => {});
    }
  });
</script>

<section class="community-view" aria-label={`Community: ${communityName}`}>
  <header class="community-header">
    <h2 class="community-name">{communityName}</h2>
    {#if votingAdapter}
      <nav class="view-tabs" aria-label="Community view">
        <button
          type="button"
          class="view-tab"
          class:active={activeView === 'proposals'}
          aria-pressed={activeView === 'proposals'}
          onclick={() => { activeView = 'proposals'; }}
        >Proposals</button>
        <button
          type="button"
          class="view-tab"
          class:active={activeView === 'tier3'}
          aria-pressed={activeView === 'tier3'}
          onclick={() => { activeView = 'tier3'; }}
        >Constitutional</button>
        <button
          type="button"
          class="view-tab"
          class:active={activeView === 'charter'}
          aria-pressed={activeView === 'charter'}
          onclick={() => { activeView = 'charter'; }}
        >Charter</button>
      </nav>
    {/if}
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
            void communityService.getForkSnapshotMetadata(requestedCommunityId).then((dto) => {
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
          // ZEB-287 Phase 2: load multi-hop lineage + descendants for the Forks tree.
          // Same race-guard pattern as the Phase 1 lineage load above.
          if (phase2Lineage === null) {
            const requestedCommunityId = communityId;
            void Promise.allSettled([
              communityService.getCommunityLineage(requestedCommunityId),
              communityService.listCommunityForks(requestedCommunityId),
            ]).then(([lineageResult, descendantsResult]) => {
              if (communityId !== requestedCommunityId) return;
              if (lineageResult.status === 'fulfilled') {
                phase2Lineage = lineageResult.value;
              }
              if (descendantsResult.status === 'fulfilled') {
                descendants = descendantsResult.value;
              }
            });
          }
        }}
      >⚙️</button>
    </div>
  </header>

  <!-- ZEB-714: community admin-recovery banner (spec §5.4) — above the
       columns so EVERY member sees it regardless of the active view. -->
  <RecoveryBanner
    {communityId}
    myAddress={ownAddress}
    resolveName={(addr) =>
      members.find((m) => m.address === addr)?.displayName ??
      resolveMentionLabel(addr, resolveNickname, resolveCard)}
    onOpenRecoverySettings={() => (settingsModalOpen = true)}
  />

  <div class="two-cols">
    {#if activeView === 'charter' && votingAdapter}
      <CharterView
        {communityId}
        {communityName}
        {members}
        adminQuorum={governance?.adminQuorum ?? null}
        adapter={votingAdapter}
        onProposeAmendment={() => { activeView = 'tier3'; }}
      />
    {:else if activeView === 'tier3' && votingAdapter}
      <Tier3ProposalPanel
        {communityId}
        adapter={votingAdapter}
        myAddr={ownAddress}
      />
    {:else if activeView === 'proposals' && votingAdapter}
      <CommunityProposalsPanel
        {communityId}
        adapter={votingAdapter}
        {myPower}
        myAddr={ownAddress}
        communityMembers={members}
      />
    {:else if activeView !== 'channels'}
      <!-- A governance view (charter / tier3 / proposals) is selected but the
           voting adapter isn't available (pre-connect, or connect failed), so
           the guarded branches above fell through. Render an explicit
           unavailable state instead of silently showing the channel feed while
           the tab still reads as the governance view (PR #410 Greptile P1). -->
      <div class="empty-channels" role="status">
        <p>This view needs a live connection to community governance.</p>
        <p>It’ll appear here once the connection is ready.</p>
      </div>
    {:else if activeChannel}
      {#if activeChannel.kind === 'townhall'}
        <!-- ZEB-612 S5: townhall channels render the assembly view. -->
        {#if voiceSession}
          <TownHallView
            session={voiceSession}
            channelName={activeChannel.name}
            {communityId}
            channelId={activeChannel.channelId}
            channelSyncing={activeChannel.syncing ?? false}
            onBeforeJoin={onBeforeVoiceJoin}
            {ownAddress}
            {myPower}
            adminQuorum={governance?.adminQuorum ?? null}
            {votingAdapter}
            {channelMessageService}
            {resolveCard}
            {resolveNickname}
            {resolveRosterName}
            {onOpenCard}
            snapshotMessages={preForkSnapshot?.channelLog?.[activeChannel.channelId] ?? []}
            originalCommunityName={preForkSnapshot?.originalCommunityName ?? ''}
            forkedAtMs={preForkSnapshot?.forkedAtMs ?? 0}
            forkReason={preForkSnapshot?.forkReason ?? null}
            mentionCandidates={joinedMentionCandidates}
            onOpenProposals={() => { activeView = 'proposals'; }}
          />
        {/if}
      {:else if activeChannel.kind === 'voice'}
        {#if voiceSession}
          <VoiceChannelView
            session={voiceSession}
            channelName={activeChannel.name}
            {communityId}
            channelId={activeChannel.channelId}
            onBeforeJoin={onBeforeVoiceJoin}
          />
        {/if}
      {:else}
        <ChannelMessageFeed
          {communityId}
          channelId={activeChannel.channelId}
          channelName={activeChannel.name}
          channelSyncing={activeChannel.syncing ?? false}
          {channelMessageService}
          {votingAdapter}
          {ownAddress}
          {myPower}
          snapshotMessages={preForkSnapshot?.channelLog?.[activeChannel.channelId] ?? []}
          originalCommunityName={preForkSnapshot?.originalCommunityName ?? ''}
          forkedAtMs={preForkSnapshot?.forkedAtMs ?? 0}
          forkReason={preForkSnapshot?.forkReason ?? null}
          {resolveCard}
          {resolveNickname}
          {resolveRosterName}
          {onOpenCard}
          mentionCandidates={joinedMentionCandidates}
        />
      {/if}
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
      loading={membersLoading}
      {ownAddress}
      {trustService}
      {resolveCard}
      {resolveNickname}
      {isOnline}
      {onOpenCard}
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
    adminQuorum={governance?.adminQuorum ?? 1}
    {thresholds}
    thresholdsLoaded={governance != null}
    {onToggleSharedInProfile}
    onClose={() => { settingsModalOpen = false; }}
    onKick={onKickMember}
    onSetPower={onSetPowerLevel}
    onLeave={onLeave}
    onGenerateInvite={onGenerateInvite}
    onOpenMembersPanel={() => { communityMembersPanelOpen = true; }}
    lineage={lineage ?? null}
    {phase2Lineage}
    {descendants}
    localNavIds={localCommunityIds}
    resolveLocalCommunityName={(spaceId) => navService.getCommunityNameBySpaceId(spaceId)}
    onForkLineageNavigate={(spaceId) => {
      settingsModalOpen = false;
      onSelectCommunity?.(spaceId);
    }}
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
        {resolveCard}
        {resolveNickname}
        {isOnline}
        {selfInvisible}
        {onOpenCard}
      />
    </div>
  </div>
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
  .view-tabs {
    display: flex;
    gap: 4px;
    margin-left: 16px;
  }
  .view-tab {
    background: none;
    border: 1px solid transparent;
    color: var(--text-secondary);
    padding: 4px 12px;
    border-radius: 4px;
    cursor: pointer;
    font: inherit;
    font-size: 0.85rem;
  }
  .view-tab:hover {
    background: var(--bg-tertiary);
    color: var(--text-primary);
  }
  .view-tab.active {
    background: var(--bg-tertiary);
    color: var(--text-primary);
    border-color: var(--border);
  }
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
  .two-cols {
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
    background: var(--overlay);
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
    box-shadow: 0 8px 24px var(--shadow-heavy);
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
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
</style>
