<script lang="ts">
  import type { CommunityMember } from '../types';
  import { powerToRole } from '../types';
  import Avatar from './Avatar.svelte';
  import type { TrustService } from '../trust-service';
  import type { ResolvedCard } from '../member-card-service';

  let {
    members,
    ownAddress,
    trustService,
    collapsed,
    onAvatarClick,
    resolveCard,
    resolveNickname,
  }: {
    members: CommunityMember[];
    ownAddress: string;
    trustService?: TrustService;
    collapsed: boolean;
    onAvatarClick?: (address: string, event: MouseEvent) => void;
    /** ZEB-341: optional profile-card resolver for member display names. */
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
    /** ZEB-432: optional local friend-nickname resolver (ZEB-419), preferred
     *  over the broadcast profile-card name — mirrors MemberRow / the message
     *  author label ladder. */
    resolveNickname?: (ownerIdHex: string) => string | undefined;
  } = $props();

  // ZEB-432 label ladder (mirrors MemberRow / ChannelMessageFeed): local friend
  // nickname (ZEB-419) ► broadcast profile-card name (ZEB-341) ► backend-provided
  // member.displayName ► truncated owner hex. Both resolvers read through App's
  // cardVersion-backed closures, so reading them here (in the template + the
  // ordering $derived) re-renders this panel automatically as cards fill in —
  // without this ladder the always-visible Members panel only ever showed hex.
  function memberLabel(m: CommunityMember): string {
    return (
      resolveNickname?.(m.address) ??
      resolveCard?.(m.address)?.displayName ??
      m.displayName ??
      m.address.slice(0, 8)
    );
  }

  // Filter to joined members only — left/kicked/invited members render
  // in the settings modal's member list, not the channel-context list.
  let visible = $derived(members.filter((m) => m.status === 'joined'));

  // Order: self first, then by power desc, then by display name asc.
  let ordered = $derived.by(() => {
    return [...visible].sort((a, b) => {
      if (a.address === ownAddress) return -1;
      if (b.address === ownAddress) return 1;
      if (a.power !== b.power) return b.power - a.power;
      // Tiebreak by the resolved display label so ordering matches what's shown.
      const an = memberLabel(a).toLowerCase();
      const bn = memberLabel(b).toLowerCase();
      return an.localeCompare(bn);
    });
  });
</script>

{#if !collapsed}
  <aside class="members-panel" aria-label="Community members">
    <header class="panel-header">
      <span class="title">Members</span>
      <span class="count">{visible.length}</span>
    </header>
    <ul class="member-list">
      {#each ordered as m (m.address)}
        <li class="member-row">
          <button
            class="avatar-trigger"
            type="button"
            aria-label="Open profile for {memberLabel(m)}"
            onclick={(e) => onAvatarClick?.(m.address, e)}
          >
            <Avatar address={m.address} {trustService} size={24} />
          </button>
          <div class="info">
            <span class="name" class:self={m.address === ownAddress}>
              {memberLabel(m)}
            </span>
            <span class="role" data-role={powerToRole(m.power)}>{powerToRole(m.power)}</span>
          </div>
        </li>
      {/each}
    </ul>
  </aside>
{/if}

<style>
  .members-panel {
    display: flex;
    flex-direction: column;
    background: var(--bg-secondary);
    border-left: 1px solid var(--border);
    width: 200px;
    min-width: 0;
    overflow: hidden;
  }
  .panel-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 12px 14px 6px;
    border-bottom: 1px solid var(--border);
    color: var(--text-secondary);
    font-size: 0.75rem;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  .count {
    background: var(--bg-tertiary);
    color: var(--text-primary);
    border-radius: 8px;
    padding: 0 6px;
    font-size: 0.7rem;
    text-transform: none;
    letter-spacing: 0;
  }
  .member-list {
    list-style: none;
    margin: 0;
    padding: 6px 0;
    overflow-y: auto;
    flex: 1;
  }
  .member-row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 14px;
    color: var(--text-primary);
    font-size: 0.875rem;
  }
  .member-row:hover { background: var(--bg-tertiary); }
  .avatar-trigger {
    background: none;
    border: none;
    padding: 0;
    cursor: pointer;
    display: flex;
  }
  .info { display: flex; flex-direction: column; min-width: 0; flex: 1; }
  .name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .name.self { color: var(--accent); }
  .role {
    font-size: 0.65rem;
    text-transform: uppercase;
    color: var(--text-secondary);
  }
  .role[data-role="admin"] { color: var(--accent); }
  .role[data-role="mod"] { color: #ffb84a; }
</style>
