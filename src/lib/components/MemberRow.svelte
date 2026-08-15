<script lang="ts">
  import type { CommunityMember } from '../types';
  import { POWER_THRESHOLDS } from '../types';
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
    selfInvisible = false,
    onOpenCard,
    thresholds = POWER_THRESHOLDS,
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
    /** ZEB-600: true when the viewer has "Appear offline" on. Only affects the
     *  self row — flips its always-online dot to a hollow "invisible" state so
     *  the viewer can confirm their own hidden status at a glance. */
    selfInvisible?: boolean;
    /** ZEB-341: open the owner_id card popover for this member. */
    onOpenCard?: (payload: OpenCardPayload, ev: MouseEvent) => void;
    /** ZEB-942: per-community power thresholds (kick / setPower). The kebab
     *  gates mirror the backend `verify_event`, so a community that customizes
     *  these away from the defaults gets consistent controls. Optional; defaults
     *  to the global POWER_THRESHOLDS so callers that don't customize (and every
     *  existing test) behave exactly as before. */
    thresholds?: { kick: number; setPower: number };
  } = $props();

  let menuOpen = $state(false);

  function tierLabel(power: number, status: CommunityMember['status']): string {
    if (status === 'banned') return 'Banned';
    if (power === 100) return 'Admin';
    if (power >= 50) return 'Moderator';
    return 'Member';
  }

  // ZEB-942: FIXED role-ladder tiers — the same boundaries the tier badge uses
  // (member < 50 ≤ Moderator, Admin = 100). These are the immovable role tiers,
  // distinct from the customizable authorization `thresholds` below: only the
  // auth gates move per community, the role ladder never does.
  const MOD_TIER = 50;
  const ADMIN_TIER = POWER_THRESHOLDS.max; // 100 — the immovable `max` the
  // backend keys admin-affecting SetPower on (community_membership.rs).

  function kebabActions(
    viewerPower: number,
    targetPower: number,
    targetStatus: CommunityMember['status'],
    isSelf: boolean,
    _isLastAdmin: boolean,
    thresholds: { kick: number; setPower: number },
  ): KebabAction[] {
    if (targetStatus === 'banned') {
      // Unban: backend requires actor_power >= set_power threshold
      // (verify_event for MembershipEventKind::Unban).
      return viewerPower >= thresholds.setPower ? ['unban'] : [];
    }
    if (isSelf) {
      // Self-demote is a SetPower on self → gated on set_power. (An admin
      // self-demoting is admin-affecting but always holds max, so the set_power
      // gate alone never surfaces a control the backend rejects.) A member below
      // set_power cannot self-demote here — they should use community-leave.
      const actions: KebabAction[] = [];
      if (viewerPower >= thresholds.setPower) {
        actions.push('demote-mod');
        actions.push('demote-member');
      }
      return actions;
    }
    // Acting on another member — mirror `verify_event` (community_membership.rs):
    //  - Kick: actor_power >= thresholds.kick AND strictly-greater than target.
    //  - SetPower: actor_power >= thresholds.setPower, EXCEPT admin-affecting
    //    SetPower (granting the max tier, or touching a current max-holder)
    //    requires actor_power >= max (100) regardless of a lowered set_power —
    //    else a lowered set_power would delegate admin grant/removal to
    //    non-admins (see the ZEB-734 guard). So promote-to-admin and demoting an
    //    admin stay gated at ADMIN_TIER; promote-to-mod and demoting a mod use
    //    set_power.
    const actions: KebabAction[] = [];
    const canSetPower = viewerPower >= thresholds.setPower; // non-admin-affecting
    const canAdminAffect = viewerPower >= ADMIN_TIER; // grants/removes max
    const canKick = viewerPower >= thresholds.kick && viewerPower > targetPower;
    if (canSetPower && targetPower < MOD_TIER) actions.push('promote-mod');
    if (canAdminAffect && targetPower < ADMIN_TIER) actions.push('promote-admin');
    if (canAdminAffect && targetPower >= ADMIN_TIER) actions.push('demote-mod');
    // Demote-to-Member is admin-affecting only when the target currently holds
    // the max tier; demoting a mod (50..99) to member is not.
    if (
      targetPower >= ADMIN_TIER
        ? canAdminAffect
        : canSetPower && targetPower >= MOD_TIER
    ) {
      actions.push('demote-member');
    }
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
    kebabActions(viewer.power, member.power, member.status, isSelf, viewer.isLastAdmin, thresholds)
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
  // ZEB-600: when the viewer has "Appear offline" on, invert exactly that
  // hard-coded self branch so the one row we author reflects the choice.
  let online = $derived(
    isSelf ? !selfInvisible : isOnline ? isOnline(member.address) : false,
  );
  // ZEB-600: the self row while invisible gets a distinct hollow look + label so
  // "you appear offline" is unmistakable (vs a peer who is merely offline).
  let selfHollow = $derived(isSelf && selfInvisible);
  let dotTitle = $derived(
    selfHollow ? 'Appearing offline' : online ? 'Online' : 'Offline',
  );

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

  // ZEB-386: close the kebab menu on Escape regardless of where focus sits in
  // the row. A document-level listener registered only WHILE the menu is open
  // keeps the Escape handler off the noninteractive <li> (clearing
  // a11y_no_noninteractive_element_interactions) without narrowing the close
  // scope to a single wrapper — and, being gated on `menuOpen`, it adds at most
  // one listener per open menu rather than one per rendered row.
  $effect(() => {
    if (!menuOpen) return;
    document.addEventListener('keydown', handleKeydown);
    return () => document.removeEventListener('keydown', handleKeydown);
  });

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

<!-- `list-style: none` on the parent strips implicit list semantics in Safari +
     VoiceOver, so the listitem role is explicit. Escape-to-close-kebab is handled
     by a `menuOpen`-gated document listener (see the $effect above), so this row
     stays a pure listitem with no listener on a noninteractive element. -->
<li class="member-row" role="listitem">
  <span
    class="presence-dot"
    class:online
    class:self-invisible={selfHollow}
    role="img"
    title={dotTitle}
    aria-label={dotTitle}
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
     var(--presence-online) when online. Sized to align with the row's avatar/text. */
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
  /* ZEB-600: self row while "Appear offline" is on — hollow with a dashed ring
     so it reads as a deliberate state, not an offline peer. */
  .presence-dot.self-invisible {
    background: transparent;
    border-style: dashed;
    border-color: var(--text-muted);
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
    outline: 2px solid var(--accent);
    outline-offset: 1px;
    border-radius: 2px;
  }
  .addr {
    font-size: 0.65rem;
    color: var(--text-secondary);
    font-family: var(--font-mono);
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
    background: var(--danger-border-muted);
    color: var(--danger-text-muted);
  }
  .tier-badge[data-power="100"] {
    background: var(--accent);
    color: var(--on-accent);
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
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .menu {
    position: absolute;
    right: 0;
    top: calc(100% + 2px);
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 4px;
    box-shadow: 0 4px 12px var(--overlay);
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
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
  .menu-item.danger {
    color: var(--danger-text-muted);
  }
</style>
