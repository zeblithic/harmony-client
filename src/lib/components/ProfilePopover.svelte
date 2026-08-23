<script lang="ts">
  import { untrack } from 'svelte';
  import { nonEmpty } from '../display-label';
  import { listen } from '@tauri-apps/api/event';
  import type { Profile } from '../types';
  import type {
    ProfileBroadcastService,
    ProfileMembershipBroadcastInfo,
  } from '../profile-broadcast-service';
  import Avatar from './Avatar.svelte';
  import type { ContactsService } from '../contacts-service';
  import { detectCollision } from '../name-collision';
  import { knownPeersState } from '../known-peers-state.svelte';

  /**
   * ZEB-341: owner_id card payload for the click-to-view surface. Distinct
   * from the Reticulum {@link Profile} world — keyed by owner_id hex, resolved
   * via the MemberCardService.
   */
  export type OwnerCard = {
    ownerIdHex: string;
    displayName: string;
    statusText: string;
    /** Resolved avatar URL from CAS/MemberCardService. Undefined → identicon. */
    avatarUrl?: string;
    /** Community power level. 100=Admin, 50=Moderator, else Member. Omitted
     *  (undefined) for message authors not in the member list → no role line. */
    power?: number;
    /** Community membership state (e.g. 'joined' / 'banned'). Distinct from
     *  `statusText` (the freeform profile status message). Omitted for message
     *  authors whose membership isn't known. */
    membershipStatus?: string;
  };

  let {
    mode = 'reticulum',
    profile,
    card,
    x,
    y,
    onClose,
    ownAddress,
    ownerIdHex,
    profileBroadcastService,
    resolveCommunityName,
    onViewProfile,
    contactsService,
    selfOwnerIdHex,
  }: {
    /** 'reticulum' = existing avatar-click profile popover; 'owner-card' =
     *  ZEB-341 owner_id card popover. */
    mode?: 'reticulum' | 'owner-card';
    /** Required in 'reticulum' mode. */
    profile?: Profile;
    /** ZEB-567: reticulum mode only. Canonical owner_id hex for the SELF profile
     *  — seeds the avatar identicon to match members/chat instead of the random
     *  `profile.address`. Omitted (others) → falls back to `profile.address`. */
    ownerIdHex?: string;
    /** Required in 'owner-card' mode. */
    card?: OwnerCard;
    x: number;
    y: number;
    onClose: () => void;
    /** Required in 'reticulum' mode. */
    ownAddress?: string;
    /** Required in 'reticulum' mode. */
    profileBroadcastService?: ProfileBroadcastService;
    /** Required in 'reticulum' mode. */
    resolveCommunityName?: (communityIdHex: string) => string | null;
    /** ZEB-345: owner-card mode only. Opens the right-side ProfilePanel for the
     *  card's owner_id. App.svelte (T10) owns the openProfileOwnerId state this
     *  sets. Omitted → the "View full profile" action is hidden. */
    onViewProfile?: (ownerIdHex: string) => void;
    /** ZEB-977: owner-card mode only. When provided, the popover offers the
     *  petname + private-notes editor for ANY identity (the drill-down is
     *  reachable from every roster row / message author, which is what makes
     *  contacts editable "anywhere"). Omitted → read-only popover. */
    contactsService?: ContactsService;
    /** ZEB-977: suppresses the annotation editor on the user's own card. */
    selfOwnerIdHex?: string;
  } = $props();

  // ── ZEB-977: petname + notes editor state (owner-card mode) ──────────
  let petnameDraft = $state('');
  let notesDraft = $state('');
  let contactLoaded = $state(false);
  let contactBusy = $state(false);
  let contactStatus = $state<string | null>(null);
  let canAnnotate = $derived(
    mode === 'owner-card' &&
      !!card &&
      !!contactsService &&
      card.ownerIdHex !== selfOwnerIdHex
  );
  $effect(() => {
    // Re-load drafts when the popover retargets to a different identity.
    const target = canAnnotate ? card?.ownerIdHex : undefined;
    contactLoaded = false;
    contactStatus = null;
    petnameDraft = '';
    notesDraft = '';
    if (!target || !contactsService) return;
    let cancelled = false;
    void contactsService
      .list()
      .then((rows) => {
        if (cancelled) return;
        const entry = rows.find((r) => r.ownerIdHex === target.toLowerCase());
        petnameDraft = entry?.petname ?? '';
        notesDraft = entry?.notes ?? '';
        contactLoaded = true;
      })
      .catch((e: unknown) => {
        if (cancelled) return;
        // Pre-owner-load ("contacts dataset not loaded") → editor stays
        // usable with blank drafts; a save will surface the real error.
        console.debug('[harmony-client] contact drafts load skipped:', e instanceof Error ? e.message : String(e));
        contactLoaded = true;
      });
    return () => {
      cancelled = true;
    };
  });
  async function saveContact(): Promise<void> {
    // Snapshot the target + drafts BEFORE the first await: if the popover
    // retargets to a different identity mid-save, the second IPC must not
    // apply this form's notes to the new target — and the status line must
    // not report onto the wrong card.
    const target = card?.ownerIdHex;
    const service = contactsService;
    if (!target || !service || contactBusy) return;
    const petname = petnameDraft.trim() || null;
    const notes = notesDraft.trim() || null;
    contactBusy = true;
    contactStatus = null;
    // Two independent IPCs; report per-step so a notes failure after a
    // successful petname write is honest about what actually persisted.
    let status: string;
    try {
      await service.setPetname(target, petname);
      try {
        await service.setNotes(target, notes);
        status = 'Saved';
      } catch (e) {
        status = `Petname saved; notes failed: ${e instanceof Error ? e.message : String(e)}`;
      }
    } catch (e) {
      status = e instanceof Error ? e.message : String(e);
    }
    if (card?.ownerIdHex === target) contactStatus = status;
    contactBusy = false;
  }

  const SOUND_LABELS = { quiet: 'Quiet', standard: 'Standard', loud: 'Loud' } as const;

  function roleLabel(power: number): string {
    // Mirror MemberRow's tierLabel thresholds (>= 50 Moderator) so the popover
    // role line matches the roster badge for power levels between tiers.
    return power >= 100 ? 'Admin' : power >= 50 ? 'Moderator' : 'Member';
  }

  // ZEB-979: impersonation-risk drill-down. The popover is where a user
  // lands to investigate a collision-marked name, so it spells out the
  // situation with BOTH hexes. Source is 'card' — the popover renders the
  // signed card name. Known peers (and self, via App's extraKnownHexes)
  // are exempt inside detectCollision.
  const cardCollision = $derived(
    mode === 'owner-card' && card
      ? detectCollision(
          { label: card.displayName, source: 'card' },
          card.ownerIdHex,
          knownPeersState.index,
        )
      : undefined,
  );

  let copied = $state(false);
  let copyTimer: ReturnType<typeof setTimeout> | null = null;

  async function copyOwnerId() {
    const hex = card?.ownerIdHex;
    if (!hex) return;
    // Only surface "Copied" after the write actually succeeds — don't claim a
    // copy if the clipboard is unavailable or the write rejects.
    if (!navigator.clipboard) return;
    try {
      await navigator.clipboard.writeText(hex);
    } catch {
      return;
    }
    copied = true;
    if (copyTimer) clearTimeout(copyTimer);
    copyTimer = setTimeout(() => {
      copied = false;
      copyTimer = null;
    }, 1500);
  }

  function onCopyKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      void copyOwnerId();
    }
  }

  let subscriptionId = $state<number | null>(null);
  let memberships = $state<ProfileMembershipBroadcastInfo | null>(null);
  let isLoading = $state(true);
  // Switch loading→empty after 3s if no broadcast arrives. Spec §8.3.
  const LOAD_TIMEOUT_MS = 3000;

  $effect(() => {
    const close = untrack(() => onClose);
    function onKeyDown(e: KeyboardEvent) {
      if (e.key === 'Escape') close();
    }
    function onClickOutside(e: MouseEvent) {
      const target = e.target as HTMLElement;
      // Exclude the popover itself AND every element that OPENS a popover — the
      // avatar (Reticulum popover) and the member-name / message-author buttons
      // (owner-card popover). Without the latter two, clicking a different
      // member's name to SWITCH popovers would open the new one (button onclick)
      // and then immediately close it (this handler), so a switch would wrongly
      // require two clicks.
      if (
        !target.closest('.profile-popover') &&
        !target.closest('.avatar.interactive') &&
        !target.closest('.name-btn') &&
        !target.closest('.author-btn')
      ) {
        close();
      }
    }
    window.addEventListener('keydown', onKeyDown);
    const timer = setTimeout(() => {
      window.addEventListener('click', onClickOutside);
    }, 0);
    return () => {
      window.removeEventListener('keydown', onKeyDown);
      window.removeEventListener('click', onClickOutside);
      clearTimeout(timer);
    };
  });

  // Subscription lifecycle for peer profiles. Splits from the keyboard
  // / click-outside effect so subscribe + listener cleanup are
  // independent: re-running the listener effect should not tear down
  // the subscription, and vice versa.
  $effect(() => {
    // ZEB-341: owner-card mode does not subscribe to anything and must not
    // require profileBroadcastService. Early-return before touching any
    // Reticulum-profile dep.
    if (mode === 'owner-card') return;
    // Snapshot reactive deps so this effect tracks only the address
    // identity of the profile we're showing.
    const peerAddr = profile!.address;
    const ownAddr = untrack(() => ownAddress);
    const svc = untrack(() => profileBroadcastService)!;

    if (peerAddr === ownAddr) {
      // Don't subscribe to ourselves; the section isn't rendered.
      return;
    }
    // Reset to a clean loading state on every profile.address change.
    // Without this, a popover instance reused across peer switches
    // (e.g. closed for A, immediately re-opened for B before the prior
    // subscription cleanup runs) would briefly render peer A's
    // memberships under peer B's profile header.
    memberships = null;
    isLoading = true;
    let cancelled = false;
    let unlistenFn: (() => void) | null = null;
    let localSubId: number | null = null;

    (async () => {
      try {
        const id = await svc.subscribe(peerAddr);
        if (cancelled) {
          await svc.unsubscribe(id).catch(() => {});
          return;
        }
        localSubId = id;
        subscriptionId = id;
        // Attach the listener BEFORE reading the cache. Otherwise a
        // broadcast can land at the server between `getCached` and
        // `listen` and be silently dropped: the cache snapshot we read
        // predates the broadcast, and no listener is attached to
        // receive the corresponding `profile-broadcast-received` event.
        // Order: subscribe → listen → getCached. Any event that lands
        // between subscribe and getCached is queued for the listener;
        // any event that lands between listen and getCached either
        // overwrites the cache before we read it (and we render the
        // newer value) or fires through the listener (and we render
        // via the event path).
        // Event payload is flat per spec §7:
        // { subscriptionId, ownerAddr, communityIds, sharedAt }.
        const unlisten = await listen<{
          subscriptionId: number;
          ownerAddr: string;
          communityIds: string[];
          sharedAt: string;
        }>('profile-broadcast-received', (event) => {
          // subscriptionId filtering: a final event may race past
          // server-side abort() — silently drop events for other ids.
          if (event.payload.subscriptionId !== id) return;
          memberships = {
            ownerAddr: event.payload.ownerAddr,
            communityIds: event.payload.communityIds,
            sharedAt: event.payload.sharedAt,
          };
          isLoading = false;
        });
        if (cancelled) {
          unlisten();
          return;
        }
        unlistenFn = unlisten;
        // Render immediately if already cached.
        const cached = await svc.getCached(id);
        if (!cancelled && cached && isLoading) {
          // Only paint from cache if a live event hasn't already
          // painted newer data (isLoading guards against clobbering a
          // listener-delivered render with a staler cache snapshot).
          memberships = cached;
          isLoading = false;
        }
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        if (!cancelled) {
          console.warn('peer profile subscription setup failed:', msg);
          // Flip out of the loading state so the popover renders the
          // empty / "no shared communities" view instead of spinning
          // until the 3s timeout fires.
          isLoading = false;
        }
      }
    })();

    const timeout = setTimeout(() => {
      if (!cancelled && isLoading) {
        isLoading = false;
        // memberships stays null → renders empty state
      }
    }, LOAD_TIMEOUT_MS);

    return () => {
      cancelled = true;
      clearTimeout(timeout);
      // Unlisten first (so we don't process a final event we'd ignore),
      // then unsubscribe IPC (so the server-side subscription drops).
      if (unlistenFn) unlistenFn();
      if (localSubId !== null) {
        svc.unsubscribe(localSubId).catch(() => {});
      }
      subscriptionId = null;
    };
  });
</script>

<div class="profile-popover" style="left: {x}px; top: {y}px;">
{#if mode === 'owner-card' && card}
  <div class="popover-header">
    <Avatar address={card.ownerIdHex} displayName={card.displayName} avatarUrl={card.avatarUrl} size={64} />
    <div class="popover-identity">
      <div class="popover-name">{nonEmpty(card.displayName) ?? 'Name unavailable'}</div>
      {#if card.statusText}
        <div class="popover-status">{card.statusText}</div>
      {/if}
      <button
        type="button"
        class="popover-ownerid"
        title="Copy owner ID"
        aria-label="Copy owner ID"
        onclick={copyOwnerId}
        onkeydown={onCopyKeydown}
      >
        <span class="ownerid-hex">{card.ownerIdHex}</span>
        <span class="ownerid-copy-hint">{copied ? 'Copied' : 'Copy'}</span>
      </button>
    </div>
  </div>
  {#if cardCollision}
    <!-- ZEB-979: same-name/different-identity warning. Full hexes on
         purpose — prefix truncation would let a chosen-prefix identity
         defeat the very comparison this block exists to enable. -->
    <div class="popover-collision" role="alert" data-testid="collision-warning">
      <div class="collision-headline">
        ⚠ This is a <strong>different identity</strong> from the
        “{cardCollision.knownLabel}” you know.
      </div>
      <div class="collision-hexes">
        <span>The {cardCollision.knownLabel} you know: <code>{cardCollision.knownHex}</code></span>
        <span>This identity: <code>{card.ownerIdHex}</code></span>
      </div>
    </div>
  {/if}
  {#if card.power !== undefined}
    <div class="popover-role">{roleLabel(card.power)}</div>
  {/if}
  {#if card.membershipStatus === 'banned'}
    <div class="popover-status-flag">Banned</div>
  {/if}
  {#if canAnnotate}
    <!-- ZEB-977: local annotations. Deliberately SEPARATE from the identity
         section above — the signed card name + full hex stay the untouched
         drill-down truth; these fields are labeled as yours alone. -->
    <div class="popover-contact" data-testid="contact-editor">
      <div class="contact-label">Your petname &amp; notes <span class="contact-privacy">(only you see these)</span></div>
      <input
        type="text"
        class="contact-petname"
        placeholder="Petname"
        aria-label="Your petname for this person"
        maxlength="64"
        bind:value={petnameDraft}
        disabled={!contactLoaded || contactBusy}
      />
      <textarea
        class="contact-notes"
        placeholder="Private notes — how you know them, why you trust them…"
        aria-label="Your private notes about this person"
        maxlength="4096"
        rows="2"
        bind:value={notesDraft}
        disabled={!contactLoaded || contactBusy}
      ></textarea>
      <div class="contact-actions">
        <button
          type="button"
          class="contact-save"
          onclick={() => void saveContact()}
          disabled={!contactLoaded || contactBusy}
        >
          {contactBusy ? 'Saving…' : 'Save'}
        </button>
        {#if contactStatus}
          <span class="contact-status" role="status">{contactStatus}</span>
        {/if}
      </div>
    </div>
  {/if}
  {#if onViewProfile}
    <button
      type="button"
      class="popover-view-profile"
      onclick={() => onViewProfile?.(card.ownerIdHex)}
    >
      View full profile
    </button>
  {/if}
{:else if profile}
  <div class="popover-header">
    <Avatar address={ownerIdHex || profile.address} displayName={profile.displayName} avatarUrl={profile.avatarUrl} size={64} />
    <div class="popover-identity">
      <div class="popover-name">{nonEmpty(profile.displayName) ?? 'Name unavailable'}</div>
      {#if profile.statusText}
        <div class="popover-status">{profile.statusText}</div>
      {/if}
      <div class="popover-address">{profile.address}</div>
    </div>
  </div>
  <div class="popover-sounds">
    <div class="sounds-label">Notification sounds</div>
    {#each (['quiet', 'standard', 'loud'] as const) as slot}
      <div class="sound-row">
        <span class="sound-slot">{SOUND_LABELS[slot]}</span>
        <span class="sound-value">
          {profile.notificationSounds?.[slot] || 'System default'}
        </span>
      </div>
    {/each}
  </div>
  {#if profile.address !== ownAddress}
    <div class="popover-memberships">
      <div class="memberships-label">Public memberships</div>
      {#if isLoading}
        <div class="memberships-loading">Looking up public memberships…</div>
      {:else if memberships === null || memberships.communityIds.length === 0}
        <div class="memberships-empty">No public memberships shared.</div>
      {:else}
        <ul class="memberships-list">
          {#each memberships.communityIds as communityId}
            <li>{resolveCommunityName?.(communityId) ?? communityId.slice(0, 8) + '…'}</li>
          {/each}
        </ul>
      {/if}
    </div>
  {/if}
{/if}
</div>

<style>
  .profile-popover {
    position: fixed;
    z-index: 100;
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 16px;
    min-width: 240px;
    max-width: 300px;
    box-shadow: var(--shadow-e2);
  }

  .popover-header {
    display: flex;
    gap: 12px;
    align-items: flex-start;
    margin-bottom: 12px;
  }

  .popover-identity {
    flex: 1;
    min-width: 0;
  }

  .popover-name {
    font-size: 16px;
    font-weight: 700;
    color: var(--text-primary);
    font-family: var(--font-display);
  }

  .popover-status {
    font-size: 12px;
    color: var(--text-muted);
    margin-top: 2px;
  }

  /* ZEB-979: impersonation-risk drill-down block. */
  .popover-collision {
    margin-bottom: 12px;
    padding: 8px 10px;
    border: 1px solid var(--warning);
    border-radius: 6px;
    font-size: 12px;
    color: var(--text-primary);
    background: color-mix(in srgb, var(--warning) 12%, transparent);
  }

  .collision-headline {
    margin-bottom: 6px;
  }

  .collision-hexes {
    display: flex;
    flex-direction: column;
    gap: 2px;
    color: var(--text-muted);
  }

  .collision-hexes code {
    font-family: var(--font-mono, monospace);
    font-size: 11px;
    word-break: break-all;
  }

  .popover-address {
    font-size: 11px;
    color: var(--text-muted);
    font-family: var(--font-mono);
    margin-top: 4px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .popover-ownerid {
    display: flex;
    align-items: center;
    gap: 6px;
    width: 100%;
    margin-top: 4px;
    padding: 2px 4px;
    background: transparent;
    border: 1px solid transparent;
    border-radius: 4px;
    cursor: pointer;
    text-align: left;
  }
  .popover-ownerid:hover {
    background: var(--bg-secondary);
    border-color: var(--border);
  }
  .popover-ownerid:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .ownerid-hex {
    font-size: 11px;
    color: var(--text-muted);
    font-family: var(--font-mono);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    flex: 1;
    min-width: 0;
  }
  .ownerid-copy-hint {
    font-size: 10px;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    flex-shrink: 0;
  }
  .popover-role {
    border-top: 1px solid var(--border);
    padding-top: 10px;
    font-size: 12px;
    font-weight: 600;
    color: var(--text-secondary);
  }

  .popover-status-flag {
    margin-top: 6px;
    font-size: 12px;
    font-weight: 600;
    color: var(--danger);
  }

  .popover-view-profile {
    display: block;
    width: 100%;
    margin-top: 10px;
    padding: 6px 8px;
    font-size: 12px;
    font-weight: 600;
    color: var(--accent);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 4px;
    cursor: pointer;
    text-align: center;
  }
  .popover-view-profile:hover {
    background: var(--bg-secondary);
  }
  .popover-view-profile:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .popover-sounds {
    border-top: 1px solid var(--border);
    padding-top: 10px;
  }

  .sounds-label {
    font-size: 11px;
    font-weight: 600;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    margin-bottom: 6px;
  }

  .sound-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 3px 0;
  }

  .sound-slot {
    font-size: 12px;
    color: var(--text-secondary);
  }

  .sound-value {
    font-size: 12px;
    color: var(--text-muted);
  }

  .popover-memberships {
    border-top: 1px solid var(--border);
    padding-top: 10px;
    margin-top: 10px;
  }
  .memberships-label {
    font-size: 11px;
    font-weight: 600;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    margin-bottom: 6px;
  }
  .memberships-loading,
  .memberships-empty {
    font-size: 12px;
    color: var(--text-muted);
    padding: 3px 0;
  }
  .memberships-list {
    list-style: none;
    padding: 0;
    margin: 0;
  }
  .memberships-list li {
    font-size: 12px;
    color: var(--text-secondary);
    padding: 3px 0;
  }

  /* ZEB-977: petname + notes editor */
  .popover-contact {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-top: 8px;
    padding-top: 8px;
    border-top: 1px solid var(--border);
  }
  .contact-label {
    font-size: 11px;
    font-weight: 600;
    color: var(--text-muted);
  }
  .contact-privacy {
    font-weight: 400;
  }
  .contact-petname,
  .contact-notes {
    font: inherit;
    font-size: 12px;
    padding: 4px 6px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg-secondary);
    color: inherit;
    resize: vertical;
  }
  .contact-actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .contact-save {
    font-size: 12px;
    padding: 3px 10px;
    border: none;
    border-radius: 4px;
    background: var(--accent);
    color: var(--on-accent);
    cursor: pointer;
  }
  .contact-save:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .contact-status {
    font-size: 11px;
    color: var(--text-muted);
  }
</style>
