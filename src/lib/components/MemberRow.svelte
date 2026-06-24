<script lang="ts">
  import type { CommunityMember } from '../types';
  import type { ResolvedCard } from '../member-card-service';
  import Avatar from './Avatar.svelte';
  import { nonEmpty } from '../display-label';

  export type KebabAction =
    | 'kick'
    | 'unban'
    | 'promote-mod'
    | 'promote-admin'
    | 'demote-mod'
    | 'demote-member';

  /** ZEB-341: payload the leaf assembles for the owner_id card popover. */
  export type OpenCardPayload = {
    ownerIdHex: string;
    displayName: string;
    statusText: string;
    /** Resolved avatar URL from CAS/MemberCardService. Undefined → identicon. */
    avatarUrl?: string;
    power?: number;
    /** Community membership state ('joined'/'banned'). Distinct from the
     *  freeform `statusText` profile message. */
    membershipStatus?: string;
  };

  let {
    member,
    viewer,
    onaction,
    resolveCard,
    resolveNickname,
    isOnline,
    onOpenCard,
  }: {
    member: CommunityMember;
    viewer: { addr: string; power: number; isLastAdmin: boolean };
    onaction?: (detail: { action: KebabAction; member: CommunityMember }) => void;
    /** ZEB-341: optional card resolver for member display names. */
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
    /** ZEB-432: optional local friend-nickname resolver (ZEB-419). Takes
     *  precedence over the broadcast profile-card name, matching the Friends
     *  panel's label ladder. */
    resolveNickname?: (ownerIdHex: string) => string | undefined;
    /** ZEB-537: optional online-presence resolver. Reads through a parent
     *  PresenceService; undefined → no dot (treated as offline). */
    isOnline?: (ownerIdHex: string) => boolean;
    /** ZEB-341: open the owner_id card popover for this member. */
    onOpenCard?: (payload: OpenCardPayload, ev: MouseEvent) => void;
  } = $props();

  let menuOpen = $state(false);

  function tierLabel(power: number, status: CommunityMember['status']): string {
    if (status === 'banned') return 'Banned';
    if (power === 100) return 'Admin';
    if (power >= 50) return 'Moderator';
    return 'Member';
  }

  function kebabActions(
    viewerPower: number,
    targetPower: number,
    targetStatus: CommunityMember['status'],
    isSelf: boolean,
    _isLastAdmin: boolean,
  ): KebabAction[] {
    if (targetStatus === 'banned') {
      return viewerPower >= 100 ? ['unban'] : [];
    }
    if (isSelf) {
      // Self-demote requires SetPower privilege (backend check:
      // actor_power >= POWER_THRESHOLDS.set_power == 100). A moderator
      // cannot self-demote via this UI — the backend would reject the
      // setPowerLevel call with "insufficient power". Mods who want to
      // step down should use the community-leave flow instead.
      const actions: KebabAction[] = [];
      if (viewerPower >= 100) {
        actions.push('demote-mod');
        actions.push('demote-member');
      }
      return actions;
    }
    // Acting on another member.
    //
    // SetPower (promote/demote) requires only that the actor has admin-tier
    // (>= 100). The backend does NOT compare actor power to target power
    // for SetPower — see `verify_event` for `MembershipEventKind::SetPower`
    // in community_membership.rs. So an admin can demote a peer admin.
    // Kick, by contrast, requires strictly-greater (`actor_power > target_power`).
    const actions: KebabAction[] = [];
    const canSetPower = viewerPower >= 100;
    const canKick = viewerPower >= 50 && viewerPower > targetPower;
    if (canSetPower && targetPower < 50) actions.push('promote-mod');
    if (canSetPower && targetPower < 100) actions.push('promote-admin');
    if (canSetPower && targetPower >= 100) actions.push('demote-mod');
    if (canSetPower && targetPower >= 50) actions.push('demote-member');
    if (canKick) actions.push('kick');
    return actions;
  }

  const ACTION_LABELS: Record<KebabAction, string> = {
    kick: 'Kick',
    unban: 'Unban',
    'promote-mod': 'Promote to Moderator',
    'promote-admin': 'Promote to Admin',
    'demote-mod': 'Demote to Moderator',
    'demote-member': 'Demote to Member',
  };

  let isSelf = $derived(member.address === viewer.addr);
  let actions = $derived(
    kebabActions(viewer.power, member.power, member.status, isSelf, viewer.isLastAdmin)
  );
  let label = $derived(tierLabel(member.power, member.status));
  // ZEB-432 label ladder (mirrors FriendsPanel): local friend nickname
  // (ZEB-419) ► broadcast profile-card name (ZEB-341) ► backend-provided
  // member.displayName ► truncated owner hex. Read through both resolvers inside
  // $derived so the reactive nickname map / card Map upgrades re-render
  // automatically — no one-time snapshot.
  let displayName = $derived(
    nonEmpty(resolveNickname?.(member.address)) ??
      nonEmpty(resolveCard?.(member.address)?.displayName) ??
      nonEmpty(member.displayName) ??
      member.address.slice(0, 8)
  );
  // ZEB-432 (PR #240 review): the owner-card popover is the identity drill-down,
  // so it shows the SIGNED profile-card name — never the local nickname. A
  // private label must not masquerade as the cryptographic identity (mirrors
  // FriendsPanel, whose popover uses the resolved card name while its row label
  // is nickname-first). Same card-first ladder the row used before ZEB-432.
  let cardDisplayName = $derived(
    nonEmpty(resolveCard?.(member.address)?.displayName) ??
      nonEmpty(member.displayName) ??
      member.address.slice(0, 8)
  );
  let joinedDate = $derived(
    member.joinedAt != null
      ? new Date(member.joinedAt).toLocaleDateString()
      : '—'
  );
  // ZEB-537: online status. Read through the resolver inside $derived so a
  // presence-updated counter bump (in App.svelte) re-evaluates the dot live —
  // mirrors the displayName/label ladder. Undefined resolver → offline.
  // Self is always shown online: zenoh does not loop our own presence beacon
  // back within a session, so `isOnline(self)` reads false even though we're
  // clearly online — showing yourself "offline" in a community you're using
  // is confusing and undermines trust in the indicator.
  let online = $derived(isSelf || (isOnline ? isOnline(member.address) : false));

  function handleMenuItemClick(action: KebabAction) {
    menuOpen = false;
    onaction?.({ action, member });
  }

  function handleKebabClick() {
    menuOpen = !menuOpen;
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') menuOpen = false;
  }

  function handleNameClick(ev: MouseEvent) {
    onOpenCard?.(
      {
        ownerIdHex: member.address,
        displayName: cardDisplayName,
        statusText: resolveCard?.(member.address)?.statusText ?? '',
        avatarUrl: resolveCard?.(member.address)?.avatarUrl,
        power: member.power,
        membershipStatus: member.status,
      },
      ev,
    );
  }
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<li class="member-row" onkeydown={handleKeydown}>
  <span
    class="presence-dot"
    class:online
    role="img"
    title={online ? 'Online' : 'Offline'}
    aria-label={online ? 'Online' : 'Offline'}
  ></span>
  <Avatar
    address={member.address}
    displayName={displayName}
    avatarUrl={resolveCard?.(member.address)?.avatarUrl}
    size={28}
  />
  <div class="member-info">
    {#if onOpenCard}
      <button type="button" class="name name-btn" onclick={handleNameClick}>
        {displayName}{isSelf ? ' (you)' : ''}
      </button>
    {:else}
      <span class="name">{displayName}{isSelf ? ' (you)' : ''}</span>
    {/if}
    <span class="addr">{member.address}</span>
  </div>
  <span class="tier-badge" data-status={member.status} data-power={member.power}>
    {label}
  </span>
  <span class="joined-date">{joinedDate}</span>

  {#if actions.length > 0}
    <div class="kebab-wrapper">
      <button
        type="button"
        class="kebab-btn"
        aria-label="Member actions"
        aria-haspopup="menu"
        aria-expanded={menuOpen}
        onclick={handleKebabClick}
      >
        ⋮
      </button>
      {#if menuOpen}
        <ul class="menu" role="menu">
          {#each actions as action (action)}
            <li role="none">
              <button
                type="button"
                role="menuitem"
                class="menu-item"
                class:danger={action === 'kick'}
                onclick={() => handleMenuItemClick(action)}
              >
                {ACTION_LABELS[action]}
              </button>
            </li>
          {/each}
        </ul>
      {/if}
    </div>
  {/if}
</li>

<style>
  .member-row {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 6px 6px;
    border-bottom: 1px solid var(--border);
    position: relative;
  }
  .member-row:last-child {
    border-bottom: none;
  }
  /* ZEB-537: online-presence indicator. Muted/hollow when offline, solid
     green when online. Sized to align with the row's avatar/text. */
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
    background: #3ba55d;
    border-color: #3ba55d;
  }
  .member-info {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
  }
  .name {
    color: var(--text-primary);
    font-size: 0.8rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .name-btn {
    background: transparent;
    border: none;
    padding: 0;
    margin: 0;
    text-align: left;
    cursor: pointer;
    font: inherit;
    max-width: 100%;
  }
  .name-btn:hover {
    text-decoration: underline;
  }
  .name-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
    border-radius: 2px;
  }
  .addr {
    font-size: 0.65rem;
    color: var(--text-secondary);
    font-family: monospace;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .tier-badge {
    padding: 1px 7px;
    border-radius: 8px;
    font-size: 0.6rem;
    font-weight: bold;
    white-space: nowrap;
    flex-shrink: 0;
  }
  .tier-badge[data-status="banned"] {
    background: #553333;
    color: #cc7a7a;
  }
  .tier-badge[data-power="100"] {
    background: var(--accent);
    color: var(--text-primary);
  }
  /* Moderator: power >= 50 but not 100 — handled via data attribute fallback */
  .tier-badge:not([data-status="banned"]):not([data-power="100"]) {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
  }
  .joined-date {
    font-size: 0.65rem;
    color: var(--text-secondary);
    white-space: nowrap;
    flex-shrink: 0;
  }
  .kebab-wrapper {
    position: relative;
    flex-shrink: 0;
  }
  .kebab-btn {
    background: transparent;
    border: 1px solid transparent;
    border-radius: 4px;
    color: var(--text-secondary);
    cursor: pointer;
    font-size: 1.1rem;
    min-width: 44px;
    min-height: 44px;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0;
    line-height: 1;
  }
  .kebab-btn:hover {
    background: var(--bg-tertiary);
    border-color: var(--border);
  }
  .kebab-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
  .menu {
    position: absolute;
    right: 0;
    top: calc(100% + 2px);
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 4px;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.5);
    list-style: none;
    margin: 0;
    padding: 4px 0;
    z-index: 100;
    min-width: 180px;
  }
  .menu-item {
    display: block;
    width: 100%;
    text-align: left;
    background: transparent;
    border: none;
    color: var(--text-primary);
    cursor: pointer;
    font-size: 0.8rem;
    padding: 7px 14px;
  }
  .menu-item:hover {
    background: var(--bg-tertiary);
  }
  .menu-item:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: -2px;
  }
  .menu-item.danger {
    color: #cc7a7a;
  }
</style>
