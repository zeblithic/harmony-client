<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import type { CommunityService } from '../community-service';
  import type { CommunityMember, ModerationEvent } from '../types';
  import { POWER_THRESHOLDS } from '../types';
  import MemberRow from './MemberRow.svelte';
  import type { KebabAction, OpenCardPayload } from './MemberRow.svelte';
  import type { ResolvedCard } from '../member-card-service';
  // ZEB-972: three-state presence — stale rows are excluded from count/sort.
  import type { PresenceDisplay } from '../presence-service';
  import { resolveMentionLabel } from '../mention-render';
  import ModerationReasonDialog from './ModerationReasonDialog.svelte';
  import LastAdminWarningDialog from './LastAdminWarningDialog.svelte';
  import RecentActionsBadge from './RecentActionsBadge.svelte';

  let {
    communityId,
    communityName,
    communityService,
    ownAddress,
    resolveCard,
    resolveNickname,
    presence,
    selfInvisible = false,
    onOpenCard,
    thresholds = POWER_THRESHOLDS,
  }: {
    communityId: string;
    communityName: string;
    communityService: CommunityService;
    ownAddress: string;
    /** ZEB-341: optional card resolver for display names. The card subscription
     *  lifecycle is owned by CommunityView (the always-mounted-per-community
     *  container), not this transient overlay, so member/author names resolve in
     *  the channel view regardless of whether this panel is open. This panel is
     *  a pure consumer of the resolved map. */
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
    /** ZEB-432: optional local friend-nickname resolver (ZEB-419), preferred
     *  over the profile-card name. Same pure-consumer contract as resolveCard. */
    resolveNickname?: (ownerIdHex: string) => string | undefined;
    /** ZEB-537/ZEB-972: optional three-state presence resolver. Pure consumer
     *  of the parent's PresenceService (`presenceFor`) — same contract as
     *  resolveCard. Three states so stale rows are not counted or lit as
     *  online. */
    presence?: (ownerIdHex: string) => PresenceDisplay;
    /** ZEB-600: true when the viewer has "Appear offline" on — forwarded to
     *  MemberRow so only the self row shows the hollow "invisible" dot. */
    selfInvisible?: boolean;
    /** ZEB-341: open the owner_id card popover for a clicked member. */
    onOpenCard?: (payload: OpenCardPayload, ev: MouseEvent) => void;
    /** ZEB-942: per-community power thresholds, forwarded to each MemberRow so
     *  its kebab gates mirror the backend `verify_event`. Optional; defaults to
     *  the global POWER_THRESHOLDS. CommunityView supplies the live governance
     *  snapshot. */
    thresholds?: { kick: number; setPower: number };
  } = $props();

  let members: CommunityMember[] = $state([]);
  let recentEvents = $state<ModerationEvent[]>([]);
  let loading = $state(true);
  // Three error surfaces with distinct semantics:
  //   loadError    — initial-fetch failure ONLY; panel-blocking (no usable
  //                  data to show). Set when the very first `refresh()` call
  //                  rejects; never written by subsequent refreshes.
  //   actionError  — action IPC rejection (promote/demote/kick/unban). Shown
  //                  as a transient banner above the still-rendered list.
  //   refreshError — post-initial-load refresh failure (e.g., transient
  //                  backend hiccup during an onMembersChanged tick). Shown
  //                  the same way as actionError (banner) so the user keeps
  //                  the last-known-good member list and isn't dropped into
  //                  a blank panel by a momentary fetch failure.
  let loadError: string | null = $state(null);
  let actionError: string | null = $state(null);
  let refreshError: string | null = $state(null);
  // ZEB-250: non-error informational message shown when an admin action was
  // routed to an AdminProposal (admin_quorum > 1 AND admin-affecting).
  let pendingActionMsg: string | null = $state(null);
  // Tracks whether the initial fetch has ever succeeded — gates the
  // loadError-vs-refreshError routing in `refresh()`.
  let hasLoadedMembers = $state(false);
  let searchQuery = $state('');
  let bannedExpanded = $state(false);

  // Dialog state for kick / unban actions
  let dialogOpen = $state(false);
  let dialogAction = $state<'kick' | 'unban'>('kick');
  let dialogTarget = $state<CommunityMember | null>(null);

  // Last-admin warning dialog state
  let lastAdminDialogOpen = $state(false);
  let lastAdminDialogAction = $state<'demote' | 'leave'>('demote');
  let lastAdminDialogPendingPower = $state<number>(0);

  // Snapshot the prior onMembersChanged so we can restore it on destroy,
  // following the same chaining pattern used by CommunityView for
  // onChannelConfigChanged.
  let prevOnMembersChanged: typeof communityService.onMembersChanged;

  function matchesSearch(m: CommunityMember, q: string): boolean {
    if (q === '') return true;
    const lower = q.toLowerCase();
    return (
      (m.displayName?.toLowerCase().includes(lower) ?? false) ||
      m.address.toLowerCase().includes(lower)
    );
  }

  let viewerPower = $derived(
    members.find((m) => m.address === ownAddress)?.power ?? 0
  );
  let admins = $derived(
    members.filter((m) => m.power === 100 && m.status === 'joined')
  );
  let viewerIsLastAdmin = $derived(
    viewerPower === 100 && admins.length === 1 && admins[0].address === ownAddress
  );
  let viewer = $derived({
    addr: ownAddress,
    power: viewerPower,
    isLastAdmin: viewerIsLastAdmin,
  });
  let searchTrimmed = $derived(searchQuery.trim());
  // ZEB-600: self counts as online unless "Appear offline" is on — matching
  // MemberRow's self-dot exactly, so the header count and online-first sort never
  // disagree with what the row shows. zenoh never loops our own beacon back, so
  // the resolver can't answer for self; we decide it from `selfInvisible` here.
  function memberOnline(m: CommunityMember): boolean {
    if (m.address === ownAddress) return !selfInvisible;
    // ZEB-972: only a genuinely fresh row counts — `stale` sorts/counts as
    // not-online, matching the honest dot MemberRow renders for it.
    return presence?.(m.address).state === 'online';
  }
  // ZEB-600: sort online-first (stable — Array.sort keeps the backend order
  // within each group), so who's around floats to the top.
  let joined = $derived(
    members
      .filter((m) => m.status === 'joined' && matchesSearch(m, searchTrimmed))
      .sort((a, b) => Number(memberOnline(b)) - Number(memberOnline(a)))
  );
  // ZEB-600: count of online joined members (incl. yourself when visible),
  // independent of the search filter.
  let onlineCount = $derived(
    members.filter((m) => m.status === 'joined' && memberOnline(m)).length
  );
  let banned = $derived(
    members.filter(
      (m) => m.status === 'banned' && matchesSearch(m, searchTrimmed)
    )
  );

  async function refresh() {
    // Only show the loading skeleton on the very first fetch — subsequent
    // refreshes (triggered by `onMembersChanged`) keep the last-known-good
    // list visible to avoid flashing the panel back to a "Loading..." state.
    if (!hasLoadedMembers) loading = true;
    try {
      members = await communityService.listCommunityMembers(communityId);
      loadError = null;
      refreshError = null;
      // A successful refresh implies whatever world-state caused the prior
      // action error may have changed; clear it.
      actionError = null;
      hasLoadedMembers = true;
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      if (hasLoadedMembers) {
        // Initial load already succeeded — preserve the last-known-good
        // members list and surface the failure as a transient banner.
        refreshError = message;
      } else {
        // First-ever load failed — no usable list to render; panel-block.
        loadError = message;
      }
    } finally {
      loading = false;
    }
    // Moderation events are non-critical — fetch after members so a
    // failure here doesn't block the panel from loading.
    try {
      recentEvents = await communityService.listRecentModerationEvents(communityId, 10);
    } catch (e) {
      console.error('failed to load moderation events', e instanceof Error ? e.message : String(e));
    }
  }

  // ZEB-250: handle AdminActionResult returned by set_power_level / kick IPC.
  // Completed → no-op (success is silent; refresh will show updated state).
  // Pending   → show an informational banner so the admin knows a quorum
  //             proposal was submitted rather than a direct action.
  function handleAdminActionResult(result: import('../types').AdminActionResult) {
    if (result.kind === 'Pending') {
      pendingActionMsg =
        `Proposal submitted — ${result.signers_so_far} of ${result.quorum_required} signatures`;
    } else {
      pendingActionMsg = null;
    }
  }

  async function onMemberAction(detail: { action: KebabAction; member: CommunityMember }) {
    const { action, member } = detail;
    try {
      if (action === 'kick') {
        dialogAction = 'kick';
        dialogTarget = member;
        dialogOpen = true;
      } else if (action === 'unban') {
        dialogAction = 'unban';
        dialogTarget = member;
        dialogOpen = true;
      } else if (action === 'promote-mod') {
        const r = await communityService.setPowerLevel(communityId, member.address, 50);
        handleAdminActionResult(r);
      } else if (action === 'promote-admin') {
        const r = await communityService.setPowerLevel(communityId, member.address, 100);
        handleAdminActionResult(r);
      } else if (action === 'demote-mod' || action === 'demote-member') {
        const isSelf = member.address === viewer.addr;
        const newPower = action === 'demote-mod' ? 50 : 0;
        if (isSelf && viewer.isLastAdmin) {
          lastAdminDialogAction = 'demote';
          lastAdminDialogPendingPower = newPower;
          lastAdminDialogOpen = true;
          return;
        }
        const r = await communityService.setPowerLevel(communityId, member.address, newPower);
        handleAdminActionResult(r);
      }
    } catch (e) {
      // Surface IPC errors as a transient banner above the member list —
      // NOT in `loadError` which would hide the list entirely. Cleared
      // by the next successful refresh (see `refresh`).
      actionError = e instanceof Error ? e.message : String(e);
    }
  }

  async function onDialogConfirm(reason: string | null): Promise<void> {
    if (!dialogTarget) return;
    if (dialogAction === 'kick') {
      const r = await communityService.kickFromCommunity(communityId, dialogTarget.address, reason ?? undefined);
      handleAdminActionResult(r);
    } else {
      await communityService.unbanFromCommunity(communityId, dialogTarget.address, reason ?? undefined);
    }
    // Refresh is triggered by communityService.onMembersChanged callback automatically
  }

  function onDialogCancel() {
    dialogTarget = null;
  }

  async function onLastAdminDialogConfirm() {
    if (lastAdminDialogAction === 'demote' && ownAddress) {
      const r = await communityService.setPowerLevel(
        communityId,
        ownAddress,
        lastAdminDialogPendingPower,
      );
      handleAdminActionResult(r);
    }
  }

  function onLastAdminDialogCancel() {
    // no-op; dialog resets and closes itself
  }

  onMount(() => {
    void refresh();

    prevOnMembersChanged = communityService.onMembersChanged;
    communityService.onMembersChanged = (cid) => {
      prevOnMembersChanged?.(cid);
      if (cid !== communityId) return;
      void refresh();
    };
  });

  onDestroy(() => {
    communityService.onMembersChanged = prevOnMembersChanged;
  });
</script>

<section class="community-members-panel">
  <header class="panel-header">
    <h2 class="panel-title">
      Community Members{#if onlineCount > 0} · {onlineCount} online{/if}
    </h2>
    <input
      type="search"
      placeholder="Filter members..."
      bind:value={searchQuery}
      aria-label="Filter members"
      class="search-input"
    />
  </header>

  {#if loading}
    <p class="loading">Loading members...</p>
  {:else if loadError}
    <p class="error" role="alert">{loadError}</p>
  {:else}
    {#if pendingActionMsg}
      <p class="info pending-action" role="status">
        {pendingActionMsg}
        <button
          type="button"
          class="dismiss-action-error"
          aria-label="Dismiss pending action notice"
          onclick={() => (pendingActionMsg = null)}
        >&times;</button>
      </p>
    {/if}
    {#if actionError}
      <p class="error action-error" role="alert">
        {actionError}
        <button
          type="button"
          class="dismiss-action-error"
          aria-label="Dismiss error"
          onclick={() => (actionError = null)}
        >&times;</button>
      </p>
    {/if}
    {#if refreshError}
      <p class="error refresh-error" role="alert">
        Refresh failed: {refreshError}
        <button
          type="button"
          class="dismiss-action-error"
          aria-label="Dismiss refresh error"
          onclick={() => (refreshError = null)}
        >&times;</button>
      </p>
    {/if}
    <RecentActionsBadge events={recentEvents} {resolveCard} {resolveNickname} />
    <ul class="member-list" role="list" aria-label="Active members">
      {#each joined as member (member.address)}
        <MemberRow
          {member}
          {viewer}
          {resolveCard}
          {resolveNickname}
          {presence}
          {selfInvisible}
          {onOpenCard}
          {thresholds}
          onaction={(detail) => onMemberAction(detail)}
        />
      {/each}
      {#if joined.length === 0}
        <li class="empty-row" role="listitem">No members match your filter.</li>
      {/if}
    </ul>

    {#if banned.length > 0}
      <details bind:open={bannedExpanded}>
        <summary class="banned-summary">Banned ({banned.length})</summary>
        <ul class="member-list banned-list" role="list" aria-label="Banned members">
          {#each banned as member (member.address)}
            <MemberRow
              {member}
              {viewer}
              {resolveCard}
              {resolveNickname}
              {presence}
              {selfInvisible}
              {onOpenCard}
              {thresholds}
              onaction={(detail) => onMemberAction(detail)}
            />
          {/each}
        </ul>
      </details>
    {/if}
  {/if}
</section>

<ModerationReasonDialog
  bind:open={dialogOpen}
  action={dialogAction}
  targetName={dialogTarget
    ? resolveMentionLabel(dialogTarget.address, resolveNickname, resolveCard, () => dialogTarget!.displayName)
    : ''}
  {communityName}
  onConfirm={onDialogConfirm}
  onCancel={onDialogCancel}
/>

<LastAdminWarningDialog
  bind:open={lastAdminDialogOpen}
  action={lastAdminDialogAction}
  {communityName}
  onConfirm={onLastAdminDialogConfirm}
  onCancel={onLastAdminDialogCancel}
/>

<style>
  .community-members-panel {
    display: flex;
    flex-direction: column;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 6px;
  }
  .panel-header {
    padding: 14px 16px 10px;
    border-bottom: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .panel-title {
    margin: 0;
    font-size: 0.9rem;
    color: var(--text-primary);
    font-weight: 600;
  }
  .search-input {
    width: 100%;
    padding: 6px 10px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    font-size: 0.8rem;
    box-sizing: border-box;
  }
  .search-input:focus {
    outline: 2px solid var(--accent);
    outline-offset: -1px;
  }
  .loading,
  .error {
    padding: 16px;
    font-size: 0.8rem;
    text-align: center;
  }
  .loading {
    color: var(--text-secondary);
  }
  .error {
    color: var(--danger-text-muted);
  }
  .member-list {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .empty-row {
    padding: 12px 16px;
    font-size: 0.8rem;
    color: var(--text-secondary);
    text-align: center;
  }
  details {
    border-top: 1px solid var(--border);
  }
  .banned-summary {
    padding: 10px 16px;
    font-size: 0.75rem;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.05em;
    cursor: pointer;
    user-select: none;
    list-style: none;
  }
  .banned-summary::-webkit-details-marker {
    display: none;
  }
  .banned-summary::before {
    content: '▶ ';
    font-size: 0.6rem;
    margin-right: 4px;
  }
  details[open] .banned-summary::before {
    content: '▼ ';
  }
  .banned-list {
    border-top: 1px solid var(--border);
  }
</style>
