<script lang="ts">
  import type { CommunityMember } from '../types';
  import { powerToRole } from '../types';
  import Avatar from './Avatar.svelte';
  import type { TrustService } from '../trust-service';
  import type { ResolvedCard } from '../member-card-service';
  import type { OpenCardPayload } from './MemberRow.svelte';
  import { nonEmpty } from '../display-label';

  let {
    members,
    ownAddress,
    trustService,
    collapsed,
    loading = false,
    initialSyncing = false,
    onAvatarClick,
    resolveCard,
    resolveNickname,
    isOnline,
    onOpenCard,
  }: {
    members: CommunityMember[];
    ownAddress: string;
    trustService?: TrustService;
    collapsed: boolean;
    /** ZEB-553 item 11: true while an initial roster load is in flight after a
     *  community switch. Drives a "Loading members…" affordance instead of a
     *  bare "0", which otherwise reads as "this community has no members" during
     *  the fetch. Only meaningful when the roster is still empty. */
    loading?: boolean;
    /** ZEB-949 Phase 2: true while a freshly-joined community is doing its first
     *  roster sync (slim invites no longer inline the roster). Shows "Syncing
     *  members…" instead of a bare empty list. Distinct from `loading`, which
     *  covers only the local list_community_members IPC round-trip. */
    initialSyncing?: boolean;
    onAvatarClick?: (address: string, event: MouseEvent) => void;
    /** ZEB-341: optional profile-card resolver for member display names. */
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
    /** ZEB-432: optional local friend-nickname resolver (ZEB-419), preferred
     *  over the broadcast profile-card name — mirrors MemberRow / the message
     *  author label ladder. */
    resolveNickname?: (ownerIdHex: string) => string | undefined;
    /** ZEB-537: optional online-presence resolver, mirroring MemberRow. The
     *  green dot is the headline presence affordance — without this prop the
     *  always-visible roster never showed it. Undefined → offline. */
    isOnline?: (ownerIdHex: string) => boolean;
    /** ZEB-341: open the owner_id card popover for a member. Mirrors the
     *  Settings roster / message-author path so clicking a member in the
     *  always-visible roster opens their card instead of a dead no-op. */
    onOpenCard?: (payload: OpenCardPayload, ev: MouseEvent) => void;
  } = $props();

  // ZEB-432 label ladder (mirrors MemberRow / ChannelMessageFeed): local friend
  // nickname (ZEB-419) ► broadcast profile-card name (ZEB-341) ► backend-provided
  // member.displayName ► truncated owner hex. Both resolvers read through App's
  // cardVersion-backed closures, so reading them here (in the template + the
  // ordering $derived) re-renders this panel automatically as cards fill in —
  // without this ladder the always-visible Members panel only ever showed hex.
  function memberLabel(m: CommunityMember): string {
    return (
      nonEmpty(resolveNickname?.(m.address)) ??
      nonEmpty(resolveCard?.(m.address)?.displayName) ??
      nonEmpty(m.displayName) ??
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

  // Online status, mirroring MemberRow. Self is always shown online: zenoh does
  // not loop our own presence beacon back within a session, so `isOnline(self)`
  // would otherwise read false even though we're clearly online.
  function presenceOnline(m: CommunityMember): boolean {
    return m.address === ownAddress || (isOnline ? isOnline(m.address) : false);
  }

  // The owner-card popover is the identity drill-down, so it shows the SIGNED
  // profile-card name — never the local nickname (mirrors MemberRow's
  // cardDisplayName ladder). A private label must not masquerade as identity.
  function cardDisplayName(m: CommunityMember): string {
    return (
      nonEmpty(resolveCard?.(m.address)?.displayName) ??
      nonEmpty(m.displayName) ??
      m.address.slice(0, 8)
    );
  }

  function handleOpenCard(m: CommunityMember, ev: MouseEvent) {
    onOpenCard?.(
      {
        ownerIdHex: m.address,
        displayName: cardDisplayName(m),
        statusText: resolveCard?.(m.address)?.statusText ?? '',
        avatarUrl: resolveCard?.(m.address)?.avatarUrl,
        power: m.power,
        membershipStatus: m.status,
      },
      ev,
    );
  }
</script>

{#if !collapsed}
  <aside class="members-panel" aria-label="Community members">
    <header class="panel-header">
      <span class="title">Members</span>
      <!-- During the initial post-switch fetch the true count is unknown, so
           "0" would be a lie — show an ellipsis until the roster resolves. -->
      <span class="count"
        >{(loading || initialSyncing) && visible.length === 0 ? '…' : visible.length}</span
      >
    </header>
    {#if loading && visible.length === 0}
      <!-- ZEB-553 item 11: kept a sibling of the <ul> (not an <li>) so the list
           stays a pure list of listitems for screen readers, and the loading
           message is a clean status live-region instead. -->
      <p class="member-loading" role="status">Loading members…</p>
    {:else if initialSyncing && visible.length === 0}
      <!-- ZEB-949 Phase 2: freshly joined; the roster is syncing in, not empty.
           Only when genuinely empty (self hasn't materialized yet) — once any
           member is visible the real list renders and this clears. -->
      <p class="member-loading" role="status" data-testid="members-syncing-banner">
        Syncing members…
      </p>
    {:else}
      <!-- Explicit list roles: `list-style: none` strips the implicit
           list/listitem semantics in Safari + VoiceOver, so a screen reader
           wouldn't announce this as a list of N members without them. -->
      <ul class="member-list" role="list">
        {#each ordered as m (m.address)}
        <li class="member-row" role="listitem">
          <span
            class="presence-dot"
            class:online={presenceOnline(m)}
            role="img"
            title={presenceOnline(m) ? 'Online' : 'Offline'}
            aria-label={presenceOnline(m) ? 'Online' : 'Offline'}
          ></span>
          <button
            class="avatar-trigger"
            type="button"
            aria-label="Open profile for {memberLabel(m)}"
            onclick={(e) => (onOpenCard ? handleOpenCard(m, e) : onAvatarClick?.(m.address, e))}
          >
            <Avatar address={m.address} {trustService} size={24} />
          </button>
          <div class="info">
            {#if onOpenCard}
              <button
                type="button"
                class="name name-btn"
                class:self={m.address === ownAddress}
                onclick={(e) => handleOpenCard(m, e)}
              >
                {memberLabel(m)}
              </button>
            {:else}
              <span class="name" class:self={m.address === ownAddress}>
                {memberLabel(m)}
              </span>
            {/if}
            <span class="role" data-role={powerToRole(m.power)}>{powerToRole(m.power)}</span>
          </div>
        </li>
      {/each}
      </ul>
    {/if}
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
  /* ZEB-553 item 11: post-switch loading affordance for the roster. */
  .member-loading {
    margin: 0;
    padding: 10px 14px;
    color: var(--text-secondary);
    font-size: 0.8rem;
    font-style: italic;
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
  /* ZEB-537: online-presence indicator, mirroring MemberRow. */
  .presence-dot {
    flex-shrink: 0;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    box-sizing: border-box;
  }
  .presence-dot.online {
    background: var(--presence-online);
    border-color: var(--presence-online);
  }
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
  /* Clickable member name → owner-card popover (ZEB-341), mirroring MemberRow. */
  .name-btn {
    background: transparent;
    border: none;
    padding: 0;
    margin: 0;
    text-align: left;
    cursor: pointer;
    font: inherit;
    color: var(--text-primary);
  }
  .name-btn:hover { text-decoration: underline; }
  .name-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
    border-radius: 2px;
  }
  .role {
    font-size: 0.65rem;
    text-transform: uppercase;
    color: var(--text-secondary);
  }
  .role[data-role="admin"] { color: var(--accent); }
  .role[data-role="mod"] { color: var(--role-mod); }
</style>
