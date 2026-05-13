<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import type { CommunityService } from '../community-service';
  import type { CommunityMember } from '../types';
  import MemberRow from './MemberRow.svelte';
  import type { KebabAction } from './MemberRow.svelte';
  import ModerationReasonDialog from './ModerationReasonDialog.svelte';

  let {
    communityId,
    communityName,
    communityService,
    ownAddress,
  }: {
    communityId: string;
    communityName: string;
    communityService: CommunityService;
    ownAddress: string;
  } = $props();

  let members: CommunityMember[] = $state([]);
  let loading = $state(true);
  let error: string | null = $state(null);
  let searchQuery = $state('');
  let bannedExpanded = $state(false);

  // Dialog state for kick / unban actions
  let dialogOpen = $state(false);
  let dialogAction = $state<'kick' | 'unban'>('kick');
  let dialogTarget = $state<CommunityMember | null>(null);

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
  let joined = $derived(
    members.filter(
      (m) => m.status === 'joined' && matchesSearch(m, searchTrimmed)
    )
  );
  let banned = $derived(
    members.filter(
      (m) => m.status === 'banned' && matchesSearch(m, searchTrimmed)
    )
  );

  async function refresh() {
    loading = true;
    try {
      members = await communityService.listCommunityMembers(communityId);
      error = null;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
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
        await communityService.setPowerLevel(communityId, member.address, 50);
      } else if (action === 'promote-admin') {
        await communityService.setPowerLevel(communityId, member.address, 100);
      } else if (action === 'demote-mod') {
        // Last-admin guard wires in Task 6
        await communityService.setPowerLevel(communityId, member.address, 50);
      } else if (action === 'demote-member') {
        await communityService.setPowerLevel(communityId, member.address, 0);
      }
    } catch (e) {
      // Surface IPC errors inline on the panel. Toast service is not
      // present in this project; use local error state.
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function onDialogConfirm(reason: string | null): Promise<void> {
    if (!dialogTarget) return;
    if (dialogAction === 'kick') {
      await communityService.kickFromCommunity(communityId, dialogTarget.address, reason ?? undefined);
    } else {
      await communityService.unbanFromCommunity(communityId, dialogTarget.address, reason ?? undefined);
    }
    // Refresh is triggered by communityService.onMembersChanged callback automatically
  }

  function onDialogCancel() {
    dialogTarget = null;
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
    <h2 class="panel-title">Community Members</h2>
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
  {:else if error}
    <p class="error" role="alert">{error}</p>
  {:else}
    <ul class="member-list" aria-label="Active members">
      {#each joined as member (member.address)}
        <MemberRow
          {member}
          {viewer}
          onaction={(detail) => onMemberAction(detail)}
        />
      {/each}
      {#if joined.length === 0}
        <li class="empty-row">No members match your filter.</li>
      {/if}
    </ul>

    {#if banned.length > 0}
      <details bind:open={bannedExpanded}>
        <summary class="banned-summary">Banned ({banned.length})</summary>
        <ul class="member-list banned-list" aria-label="Banned members">
          {#each banned as member (member.address)}
            <MemberRow
              {member}
              {viewer}
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
  targetName={dialogTarget?.displayName ?? dialogTarget?.address.slice(0, 8) ?? ''}
  {communityName}
  onConfirm={onDialogConfirm}
  onCancel={onDialogCancel}
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
    outline: 2px solid var(--accent, #5865f2);
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
    color: #cc7a7a;
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
