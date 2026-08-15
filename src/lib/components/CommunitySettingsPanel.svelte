<script lang="ts">
  import { trapFocus } from '../actions/trap-focus';
  import {
    POWER_THRESHOLDS,
    powerToRole,
    type CommunityLineageDto,
    type CommunityMember,
    type ForkDescendantDto,
  } from '../types';
  import { nonEmpty } from '../display-label';
  import { resolveMentionLabel } from '../mention-render';
  import type { ResolvedCard } from '../member-card-service';
  import ConfirmationModal from './ConfirmationModal.svelte';
  import SetPowerDialog from './SetPowerDialog.svelte';
  import LastAdminWarningDialog from './LastAdminWarningDialog.svelte';
  import InviteLinkManager from './InviteLinkManager.svelte';
  import ForkConfirmDialog from './ForkConfirmDialog.svelte';
  import ForkLineageTree from './ForkLineageTree.svelte';
  import ForkGenealogyGraph from './ForkGenealogyGraph.svelte';
  import Modal from './Modal.svelte';
  import PendingJoinsPanel from './PendingJoinsPanel.svelte';
  import { invoke } from '@tauri-apps/api/core';
  import PendingAdminProposalsPanel from './PendingAdminProposalsPanel.svelte';
  import ChangeQuorumDialog from './ChangeQuorumDialog.svelte';
  import ChangeThresholdsDialog from './ChangeThresholdsDialog.svelte';
  import RoleBadge from './governance/RoleBadge.svelte';
  import PipMeter from './governance/PipMeter.svelte';
  import RecoveryConfigDialog from './RecoveryConfigDialog.svelte';
  import InitiateRecoveryDialog from './InitiateRecoveryDialog.svelte';
  import type { RecoveryStateDto } from '../recovery-types';
  import {
    RECOVERY_FLAGS_CHANGED_EVENT,
    dismissSoleAdminNudge,
    isSoleAdminNudgeDismissed,
  } from '../recovery-flags';

  let {
    communityId,
    communityName,
    communityKind,
    members,
    myAddress,
    myPower,
    isDegraded,
    sharedInProfile,
    onToggleSharedInProfile,
    onClose,
    onKick,
    onSetPower,
    onLeave,
    onGenerateInvite,
    onOpenMembersPanel,
    onFork,
    lineage,
    phase2Lineage = null,
    descendants = [],
    localNavIds = new Set<string>(),
    onForkLineageNavigate,
    resolveLocalCommunityName,
    adminQuorum = 1,
    thresholds = {
      invite: POWER_THRESHOLDS.invite,
      kick: POWER_THRESHOLDS.kick,
      setPower: POWER_THRESHOLDS.setPower,
    },
    thresholdsLoaded = false,
    resolveCard,
    resolveNickname,
  }: {
    communityId: string;
    communityName: string;
    communityKind: 'open' | 'invite-only' | 'unknown';
    members: CommunityMember[];
    myAddress: string;
    myPower: number;
    isDegraded: boolean;
    sharedInProfile: boolean;
    onToggleSharedInProfile: (shared: boolean) => Promise<void>;
    onClose: () => void;
    onKick: (targetAddr: string) => void;
    onSetPower: (targetAddr: string, newPower: number) => void;
    onLeave: () => void;
    onGenerateInvite: () => Promise<string>;
    /** ZEB-907: optional resolvers (same contracts as CommunityMembersPanel).
     *  Rows resolve through the shared 4-rung ladder (nickname → live card →
     *  roster displayName → hex) so the self row — whose roster displayName
     *  is always null (you never receive your own card) — renders the local
     *  card name instead of hex. */
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
    resolveNickname?: (ownerIdHex: string) => string | undefined;
    /** ZEB-250: current admin quorum threshold for this community.
     *  When not provided, defaults to 1 (no multi-sig required). */
    adminQuorum?: number;
    /** ZEB-251: per-community power thresholds (invite / kick / setPower).
     *  When not provided, defaults to the global POWER_THRESHOLDS consts so
     *  an un-customized community behaves exactly as before per-community
     *  thresholds existed. */
    thresholds?: { invite: number; kick: number; setPower: number };
    /** ZEB-251: true once `thresholds` reflects loaded governance data (vs the
     *  fallback defaults). The "Change thresholds…" affordance stays disabled
     *  until then, so an admin can't propose a change built from stale fallback
     *  values before getCommunityGovernance resolves. */
    thresholdsLoaded?: boolean;
    /** Optional: if provided, a "Manage members" button appears in the Members
     *  section that opens the full CommunityMembersPanel overlay (with recent
     *  moderation history). Callers that don't yet thread through communityService
     *  can omit this to keep the inline member list. */
    onOpenMembersPanel?: () => void;
    /** ZEB-285: if provided, the "Fork this community" button is wired to this
     *  callback which handles the actual IPC call + nav transition. */
    onFork?: (opts: { name: string; silent: boolean; alsoLeave: boolean; reason: string }) => Promise<void>;
    /** ZEB-285: fork lineage metadata — present when this community was forked
     *  from another. Populated by the caller via get_fork_snapshot_metadata IPC. */
    lineage?: {
      originalCommunityName: string | null;
      forkedAtMs: number;
      snapshotMessageCount: number;
    } | null;
    /** ZEB-287 Phase 2: full lineage DTO with multi-hop ancestor chain.
     *  Populated by the caller via getCommunityLineage IPC. When null,
     *  the Forks tree renders with self-only and no ancestors. */
    phase2Lineage?: CommunityLineageDto | null;
    /** ZEB-287 Phase 2: descendants list returned by listCommunityForks IPC. */
    descendants?: ForkDescendantDto[];
    /** ZEB-287 Phase 2: set of hex SpaceIds the user has locally
     *  (joined / known via OwnerState). Used by ForkLineageTree to gate
     *  clickability of ancestor and descendant rows. */
    localNavIds?: Set<string>;
    /** ZEB-287 Phase 2: callback fired when a clickable tree row is
     *  activated. Caller routes the spaceId to the community-navigation
     *  primitive (NavService.selectCommunity equivalent). */
    onForkLineageNavigate?: (spaceId: string) => void;
    /** ZEB-287 R3-1: resolver for a fork's display name from its hex SpaceId.
     *  Caller passes NavService.getCommunityNameBySpaceId so the Forks tree
     *  renders locally-known descendants by name (not raw hex). */
    resolveLocalCommunityName?: (spaceId: string) => string | null | undefined;
  } = $props();

  let kickTarget = $state<CommunityMember | null>(null);
  let setPowerTarget = $state<CommunityMember | null>(null);
  // ZEB-250: change-quorum dialog state
  let showChangeQuorumDialog = $state(false);
  // ZEB-251: change-thresholds dialog state
  let showChangeThresholdsDialog = $state(false);
  // ZEB-285: fork dialog state
  let forkDialogOpen = $state(false);
  // ZEB-649: the 2D genealogy graph modal (design Frame A / Frame D's
  // deferred "View full lineage tree" button).
  let genealogyOpen = $state(false);
  let forkError = $state<string | null>(null);
  // Holds an admin-threshold-crossing power change pending tier-2
  // confirmation. Populated by SetPowerDialog onSubmit when the new
  // power crosses the admin tier (ADMIN_TIER, power 100) in either direction; the
  // ConfirmationModal it drives commits the change on accept (or
  // discards on cancel). Promote-to-admin is effectively irreversible
  // through the current UI — once two users are both at power 100,
  // canSetPower's strict caller > target rule prevents either from
  // demoting the other — so this confirmation is load-bearing rather
  // than ceremonial.
  let pendingAdminChange = $state<{ target: CommunityMember; newPower: number } | null>(null);
  let leaveOpen = $state(false);
  let lastAdminLeaveDialogOpen = $state(false);
  const titleId = `community-settings-title-${Math.random().toString(36).slice(2)}`;

  // ZEB-251: the admin tier is FIXED at power 100 (= POWER_THRESHOLDS.max),
  // independent of the per-community `set_power` action threshold. Backend
  // admin governance (AdminProposal / quorum / recovery) authorizes on
  // power == 100, so admin-IDENTITY gates key off ADMIN_TIER — only the
  // per-action gates (canKick / canSetPower / invite) use per-community
  // thresholds. Lowering set_power must NOT make a non-admin look like an
  // admin in the UI while the backend still rejects their governance actions.
  const ADMIN_TIER = POWER_THRESHOLDS.max;

  function crossesAdminThreshold(currentPower: number, newPower: number): boolean {
    const threshold = ADMIN_TIER;
    return (currentPower < threshold && newPower >= threshold)
        || (currentPower >= threshold && newPower < threshold);
  }

  function handleSetPowerSubmit(newPower: number) {
    const target = setPowerTarget!;
    setPowerTarget = null;
    if (crossesAdminThreshold(target.power, newPower)) {
      pendingAdminChange = { target, newPower };
    } else {
      onSetPower(target.address, newPower);
    }
  }

  // ZEB-714: admin-recovery state (config + selfIsDesignate). Fetched on
  // mount; readable by any Joined member. null until loaded / on failure.
  let recoveryState = $state<RecoveryStateDto | null>(null);
  let showRecoveryConfigDialog = $state(false);
  let showInitiateRecoveryDialog = $state(false);
  // Bumped on recovery-flag writes so the localStorage-backed sole-admin
  // nudge re-derives without a remount (the BackupReminderBanner pattern).
  let recoveryFlagsTick = $state(0);
  let latestRecoveryCallId = 0;

  async function refreshRecoveryState() {
    const myCallId = ++latestRecoveryCallId;
    try {
      const result = await invoke<RecoveryStateDto>('get_recovery_state', { communityId });
      if (myCallId !== latestRecoveryCallId) return;
      recoveryState = result;
    } catch {
      if (myCallId !== latestRecoveryCallId) return;
      recoveryState = null;
    }
  }

  $effect(() => {
    void communityId;
    void refreshRecoveryState();
    const onFlags = () => {
      recoveryFlagsTick += 1;
    };
    window.addEventListener(RECOVERY_FLAGS_CHANGED_EVENT, onFlags);
    return () => {
      latestRecoveryCallId++;
      window.removeEventListener(RECOVERY_FLAGS_CHANGED_EVENT, onFlags);
    };
  });

  let joinedMembers = $derived(members.filter((m) => m.status === 'joined'));
  let adminCount = $derived(joinedMembers.filter((m) => m.power >= ADMIN_TIER).length);
  let amOnlyAdmin = $derived(
    myPower >= ADMIN_TIER &&
    adminCount === 1 &&
    joinedMembers.some((m) => m.address === myAddress && m.power >= ADMIN_TIER)
  );
  let myRole = $derived(powerToRole(myPower));
  let canModerate = $derived(myPower >= ADMIN_TIER);
  // ZEB-250: admin governance section
  let canAdmin = $derived(myPower >= ADMIN_TIER);
  let currentAdminCount = $derived(adminCount);
  let currentAdminQuorum = $derived(adminQuorum);

  // ZEB-714: sole-admin recovery nudge (spec §5.1) — exactly one
  // power-100 member, no designates configured, not dismissed.
  let showSoleAdminNudge = $derived.by(() => {
    void recoveryFlagsTick;
    return (
      canAdmin &&
      amOnlyAdmin &&
      recoveryState !== null &&
      recoveryState.config === null &&
      !isSoleAdminNudgeDismissed(myAddress, communityId)
    );
  });
  let recoveryDesignateNames = $derived(
    (recoveryState?.config?.designateAddrs ?? []).map(
      (addr) => members.find((m) => m.address === addr)?.displayName ?? addr.slice(0, 8),
    ),
  );
  let recoveryWindowDays = $derived.by(() => {
    const ms = recoveryState?.config?.vetoWindowMs ?? 0;
    const days = ms / 86_400_000;
    return Number.isInteger(days) ? `${days}` : `~${days.toFixed(1)}`;
  });

  // ZEB-250: pending-badge map — indexed by target_addr for O(1) member-row lookup.
  // Only fetched when caller is admin (IPC is admin-gated per spec §7.5).
  let pendingProposalsByTarget = $state<Map<string, PendingAdminProposalDto>>(new Map());
  let latestProposalsCallId = 0;

  $effect(() => {
    // Track reactive deps.
    void communityId;
    if (!canAdmin) {
      latestProposalsCallId++;
      pendingProposalsByTarget = new Map();
      return;
    }
    const myCallId = ++latestProposalsCallId;
    invoke<PendingAdminProposalDto[]>('list_pending_admin_proposals', { communityId })
      .then((proposals) => {
        if (myCallId !== latestProposalsCallId) return; // stale
        const m = new Map<string, PendingAdminProposalDto>();
        for (const p of proposals) {
          if (p.expired || p.effective) continue;
          const kind = p.proposal_kind;
          if (kind.kind === 'SetPower' || kind.kind === 'Kick') {
            m.set(kind.target_addr, p);
          }
        }
        pendingProposalsByTarget = m;
      })
      .catch(() => {
        // Badges are best-effort; silently skip on error.
      });
  });

  // ZEB-582: per-community relay opt-in. Semantically first-person — "my fleet
  // volunteers to store & forward this community's (ciphertext) messages for
  // members who are offline or behind strict networks." Invite-only communities
  // auto-enable this at creation (backend), so remote/CGNAT joiners get a
  // relay-backed state-root pull path instead of silently never receiving
  // channels; this toggle surfaces that state and lets the owner opt out (or any
  // member opt in). Reads/writes the existing get_community_relay_status /
  // set_community_relay_opt_in IPCs (note: both key on `communityIdHex`).
  let relayOptedIn = $state(false);
  let relayLoading = $state(true);
  let relayPending = $state(false);
  let relayError = $state<string | null>(null);
  // Monotonic guard: a communityId change must drop a stale in-flight status
  // read so it can't overwrite a newer community's value (mirrors the
  // latestProposalsCallId pattern above).
  let relayStatusSeq = 0;

  $effect(() => {
    void communityId;
    const mySeq = ++relayStatusSeq;
    relayLoading = true;
    // A new community starts fresh: clear the in-flight toggle state AND the
    // displayed value from the prior community so neither bleeds across. The
    // relayOptedIn reset is load-bearing: without it, a FAILED status read on
    // the new community would re-enable the checkbox (finally → relayLoading =
    // false) still showing the prior community's opted-in value (CodeRabbit
    // PR #357). The toggle handler also drops its own stale completions, below.
    relayOptedIn = false;
    relayPending = false;
    relayError = null;
    invoke<boolean>('get_community_relay_status', { communityIdHex: communityId })
      .then((v) => {
        if (mySeq !== relayStatusSeq) return; // superseded
        relayOptedIn = v === true;
        relayError = null;
      })
      .catch((e) => {
        if (mySeq !== relayStatusSeq) return;
        relayError = e instanceof Error ? e.message : String(e);
      })
      .finally(() => {
        if (mySeq !== relayStatusSeq) return;
        relayLoading = false;
      });
  });

  async function handleRelayToggle(e: Event) {
    if (relayPending) return;
    // Capture the checkbox BEFORE awaiting — `e.currentTarget` is nulled once
    // the synchronous event dispatch completes, so re-reading it in the catch
    // would throw and strand the optimistic state (same hazard as the
    // shared-in-profile toggle below).
    const target = e.currentTarget as HTMLInputElement;
    const next = target.checked;
    // Bind this completion to the community AND the load generation it was
    // clicked on. If the user switches communities while the set is in flight,
    // the stale completion must NOT touch the new community's toggle state
    // (CodeRabbit PR #357) — otherwise community A's failure could roll back
    // B's UI or leave B's toggle stuck disabled. Community id ALONE is not
    // enough: an A → B → A round-trip returns to the same id, so the original
    // A set would still pass `cid === communityId` and clobber the re-entered
    // A's fresh state. relayStatusSeq bumps on every switch (incl. A→B→A), so
    // pairing it with the id pins this completion to this exact visit.
    const cid = communityId;
    const mySeq = relayStatusSeq;
    relayOptedIn = next; // optimistic
    relayPending = true;
    try {
      await invoke('set_community_relay_opt_in', {
        communityIdHex: cid,
        optedIn: next,
      });
      if (cid !== communityId || mySeq !== relayStatusSeq) return; // switched mid-flight — drop
      relayError = null;
    } catch (err) {
      if (cid !== communityId || mySeq !== relayStatusSeq) return; // switched mid-flight — drop
      // Roll back both the model and the DOM checkbox on failure.
      relayOptedIn = !next;
      target.checked = !next;
      relayError = err instanceof Error ? err.message : String(err);
    } finally {
      if (cid === communityId && mySeq === relayStatusSeq) relayPending = false;
    }
  }

  let search = $state('');

  /** ZEB-907: ONE label per row via the shared ladder, used by the render
   *  AND the search filter so a rendered name is always findable. */
  function memberLabel(m: CommunityMember): string {
    return resolveMentionLabel(m.address, resolveNickname, resolveCard, () => m.displayName);
  }

  let filteredMembers = $derived(
    search.trim() === ''
      ? joinedMembers
      : (() => {
          // Trim before lowercasing so a leading space doesn't bypass
          // both the empty-check (which trims) AND the substring match
          // (which would compare against the raw string with spaces).
          const q = search.trim().toLowerCase();
          return joinedMembers.filter(
            (m) =>
              memberLabel(m).toLowerCase().includes(q) ||
              (m.displayName?.toLowerCase().includes(q) ?? false) ||
              m.address.toLowerCase().includes(q)
          );
        })()
  );

  function canKick(target: CommunityMember): boolean {
    return target.address !== myAddress
      && myPower >= thresholds.kick
      && myPower > target.power;
  }

  function canSetPower(target: CommunityMember): boolean {
    return target.address !== myAddress
      && myPower >= thresholds.setPower
      && myPower > target.power;
  }

  function handleOverlayClick(e: MouseEvent) {
    // Backdrop-click-to-dismiss for consistency with every other
    // modal in the PR. Guard against clicks bubbling from inside
    // the panel — those should NOT dismiss (we don't want a stray
    // click on a member row to close the whole panel).
    // Escape key handling lives on the .panel div via the trapFocus
    // action's onCancel; no need to duplicate it on the overlay
    // (focus is always trapped inside the panel anyway).
    if (e.target === e.currentTarget) onClose();
  }

  // ZEB-250: pending-badge helpers for member-list rows.
  type ProposalKindDto =
    | { kind: 'SetPower'; target_addr: string; target_display_name: string | null; level: number }
    | { kind: 'Kick'; target_addr: string; target_display_name: string | null; reason: string | null }
    | { kind: 'ChangeQuorum'; new_quorum: number };

  type PendingAdminProposalDto = {
    event_id: string;
    proposer_addr: string;
    proposer_display_name: string | null;
    proposal_kind: ProposalKindDto;
    proposed_at_wall_ms: number;
    signers_so_far: number;
    quorum_required: number;
    expired: boolean;
    effective: boolean;
    self_has_signed: boolean;
    signer_display_names: string[];
  };

  function pendingBadgeText(p: PendingAdminProposalDto): string {
    const kind = p.proposal_kind;
    if (kind.kind === 'SetPower' && kind.level === 100) return 'pending promotion to admin';
    if (kind.kind === 'SetPower' && kind.level === 0) return 'pending demotion';
    if (kind.kind === 'SetPower') return 'pending power change';
    if (kind.kind === 'Kick') return 'pending kick';
    return 'pending action';
  }
</script>

<!-- Backdrop: click-to-dismiss only (Escape is handled on the focusable panel
     below); presentation role keeps it out of the a11y tree. -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<div
  class="panel-overlay"
  role="presentation"
  onclick={handleOverlayClick}
>
  <div
    class="panel"
    role="dialog"
    aria-modal="true"
    aria-labelledby={titleId}
    use:trapFocus={{ onCancel: onClose }}
  >
    <div class="header">
      <div>
        <h3 class="panel-title" id={titleId}>Manage community</h3>
        <div class="subtitle">{communityName}</div>
      </div>
      <button class="close-btn" onclick={onClose} aria-label="Close">✕</button>
    </div>

    <div class="section">
      <div class="section-label">Info</div>
      <div class="info-grid">
        <div class="key">Name</div><div>{communityName}</div>
        <div class="key">Type</div>
        <div>
          {#if communityKind === 'invite-only'}🔒 Invite-only
          {:else if communityKind === 'open'}🌐 Open
          {:else}<span class="muted">— (unknown)</span>{/if}
        </div>
        <div class="key">Members</div><div>{joinedMembers.length} joined</div>
        <div class="key">Your role</div>
        <div>
          <RoleBadge role={myRole} />
          (power {myPower})
        </div>
        <div class="key">Sync status</div>
        <div class={isDegraded ? 'degraded' : 'healthy'}>
          {isDegraded ? '⚠ Degraded — pending events not yet visible' : '● Healthy'}
        </div>
      </div>
    </div>

    <div class="section">
      <div class="section-label">Public profile</div>
      <label class="toggle-row">
        <input
          type="checkbox"
          checked={sharedInProfile}
          onchange={async (e) => {
            // Capture the checkbox reference BEFORE awaiting. Per the
            // DOM spec, `event.currentTarget` is nullified once the
            // synchronous event-dispatch completes, so re-reading
            // `e.currentTarget` in the catch branch would throw a
            // TypeError and leave the checkbox stuck in the
            // optimistic-true state.
            const target = e.currentTarget as HTMLInputElement;
            const checked = target.checked;
            try {
              await onToggleSharedInProfile(checked);
            } catch (err) {
              const msg = err instanceof Error ? err.message : String(err);
              // Roll back the UI to match server state on failure.
              target.checked = !checked;
              console.warn('toggle shared_in_profile failed:', msg);
            }
          }}
        />
        <span class="toggle-label">
          Share this community in my public profile
        </span>
      </label>
      <p class="toggle-help">
        When enabled, peers viewing your profile will see that you've
        joined <strong>{communityName}</strong>. Off by default.
      </p>
    </div>

    <!-- ZEB-582: per-community relay opt-in. Invite-only communities default
         this on at creation so remote members actually receive channels. -->
    <div class="section">
      <div class="section-label">Message relay</div>
      <label class="toggle-row">
        <input
          type="checkbox"
          checked={relayOptedIn}
          disabled={relayLoading || relayPending}
          onchange={handleRelayToggle}
        />
        <span class="toggle-label">
          Relay this community for offline members
        </span>
      </label>
      <p class="toggle-help">
        When on, your devices help store &amp; forward this community's messages
        so members who are offline or behind strict networks still receive them.
        The relay only ever sees encrypted data.{#if communityKind === 'invite-only'}
          Especially important if you invite members from other networks —
          without a relay, they may join but never receive channels.{/if}
      </p>
      {#if relayError}
        <p class="fork-error">{relayError}</p>
      {/if}
    </div>

    <div class="section">
      <div class="section-label">Members ({joinedMembers.length})</div>
      <div class="member-search">
        <input
          type="text"
          placeholder="Search members..."
          bind:value={search}
          class="search-input"
          aria-label="Search members"
        />
      </div>
      <div class="member-list">
        {#each filteredMembers as m (m.address)}
          {@const label = memberLabel(m)}
          <div class="member-row">
            <div class="avatar">{label.slice(0, 1).toUpperCase()}</div>
            <div class="member-name">
              <div class="name">{label}{m.address === myAddress ? ' (you)' : ''}</div>
              <div class="addr">{m.address}</div>
            </div>
            <RoleBadge role={powerToRole(m.power)} />
            {#if canSetPower(m)}
              <button class="set-role" onclick={() => (setPowerTarget = m)}>Set role</button>
            {/if}
            {#if canKick(m)}
              <button class="kick" onclick={() => (kickTarget = m)}>Kick</button>
            {/if}
            {#if canAdmin && pendingProposalsByTarget.has(m.address)}
              {@const pending = pendingProposalsByTarget.get(m.address)!}
              <span class="pending-badge" aria-label={`Pending: ${pendingBadgeText(pending)}`}>
                ⏳ {pendingBadgeText(pending)}
              </span>
            {/if}
          </div>
        {/each}
      </div>
      {#if onOpenMembersPanel && myPower >= thresholds.kick}
        <!-- ZEB-926: the manage/moderation overlay is a member-moderation
             surface, gated on the same power axis as Invites / Join requests
             (myPower >= thresholds.kick). The in-panel controls are already
             power-gated per row and the CRDT layer rejects under-powered
             events on every peer, so this gate is cosmetic — it just stops a
             plain member from opening a surface they can't act on. -->
        <button class="manage-members-btn" onclick={onOpenMembersPanel}>
          Manage members &amp; moderation history →
        </button>
      {/if}
    </div>

    {#if myPower >= thresholds.invite}
      <div class="section">
        <div class="section-label">Invites</div>
        <InviteLinkManager kind={communityKind} onGenerate={onGenerateInvite} />
      </div>
    {/if}

    {#if canModerate}
      <div class="section">
        <div class="section-label">Join requests</div>
        <PendingJoinsPanel {communityId} {canModerate} />
      </div>
    {/if}

    <!-- ZEB-250: Admin governance section — admin-only. -->
    {#if canAdmin}
      <div class="section admin-governance-section" aria-label="Admin governance">
        <div class="section-label">Admin governance</div>
        <p class="admin-quorum-info">
          Current admin quorum: {currentAdminQuorum} of {currentAdminCount} admins required for
          admin-affecting actions.
        </p>
        <PipMeter
          filled={currentAdminQuorum}
          total={currentAdminCount}
          label="Admin quorum meter"
        />
        <button class="change-quorum-btn" onclick={() => (showChangeQuorumDialog = true)}>
          Change quorum…
        </button>
        <button
          class="change-quorum-btn"
          disabled={!thresholdsLoaded}
          title={thresholdsLoaded ? undefined : 'Loading current thresholds…'}
          onclick={() => (showChangeThresholdsDialog = true)}
        >
          Change thresholds…
        </button>
        <PendingAdminProposalsPanel {communityId} {canAdmin} />
      </div>
    {/if}

    <!-- ZEB-714: Admin recovery section (spec §5.1 / §5.3). Visible to
         admins (configuration) and to recovery designates (initiation) —
         verbally distinct from fleet/device recovery (spec §8). -->
    {#if canAdmin || recoveryState?.selfIsDesignate}
      <div class="section admin-recovery-section" aria-label="Admin recovery">
        <div class="section-label">Admin recovery</div>

        {#if showSoleAdminNudge}
          <div class="recovery-nudge" role="status">
            <span class="recovery-nudge-text">
              If you lose your identity, this community cannot replace you.
              Configure recovery designates.
            </span>
            <button
              class="recovery-nudge-dismiss"
              aria-label="Dismiss recovery nudge"
              onclick={() => dismissSoleAdminNudge(myAddress, communityId)}
            >✕</button>
          </div>
        {/if}

        {#if recoveryState?.config}
          <p class="recovery-config-info">
            Recovery is configured: {recoveryState.config.threshold} of
            {recoveryState.config.designateAddrs.length} designates
            ({recoveryDesignateNames.map((n) => `@${n}`).join(', ')}) can propose a
            replacement admin after a {recoveryWindowDays}-day veto window.
          </p>
        {:else if recoveryState !== null}
          <p class="recovery-config-info">
            No recovery designates configured. If this community's only admin
            identity is lost, no one can replace them and the community cannot
            be administered again.
          </p>
        {/if}

        {#if canAdmin}
          <button class="recovery-config-btn" onclick={() => (showRecoveryConfigDialog = true)}>
            {recoveryState?.config ? 'Change recovery settings…' : 'Configure recovery…'}
          </button>
        {/if}

        {#if recoveryState?.selfIsDesignate}
          <p class="recovery-designate-info">
            You are a recovery designate for this community. If an admin's
            identity is lost, you can start the recovery process.
          </p>
          <button
            class="recovery-initiate-btn"
            onclick={() => (showInitiateRecoveryDialog = true)}
          >
            Initiate admin recovery…
          </button>
        {/if}
      </div>
    {/if}

    {#if showChangeQuorumDialog && canAdmin}
      <ChangeQuorumDialog
        {communityId}
        currentQuorum={currentAdminQuorum}
        currentAdminCount={currentAdminCount}
        onClose={() => (showChangeQuorumDialog = false)}
      />
    {/if}

    {#if showChangeThresholdsDialog && canAdmin && thresholdsLoaded}
      <ChangeThresholdsDialog
        {communityId}
        currentThresholds={{ invite: thresholds.invite, kick: thresholds.kick, setPower: thresholds.setPower }}
        onClose={() => (showChangeThresholdsDialog = false)}
      />
    {/if}

    {#if showRecoveryConfigDialog && canAdmin}
      <RecoveryConfigDialog
        {communityId}
        {joinedMembers}
        {myAddress}
        existing={recoveryState?.config ?? null}
        onClose={() => (showRecoveryConfigDialog = false)}
        onSaved={() => {
          void refreshRecoveryState();
        }}
      />
    {/if}

    {#if showInitiateRecoveryDialog && recoveryState?.selfIsDesignate}
      <InitiateRecoveryDialog
        {communityId}
        {members}
        {myAddress}
        onClose={() => (showInitiateRecoveryDialog = false)}
        onInitiated={() => {
          void refreshRecoveryState();
        }}
      />
    {/if}

    <!-- ZEB-287 Phase 2 spec §5.1: unified "Forks" section that always renders
         for every community. Replaces Phase 1's separate Lineage + Fork
         sections with a single coherent block: polycentric-framing
         explainer + ForkLineageTree + "Fork this community" button. -->
    <div class="section forks-section">
      <div class="section-label">Forks</div>
      <p class="forks-explainer">
        Any member of a community can fork it at any time, creating a new community with
        the snapshot of history they had access to. The fork is independent &mdash; it has
        its own membership, channels, and admin. Forks are how communities preserve
        continuity if members want to take their conversation elsewhere.
      </p>

      {#if phase2Lineage?.forkedFrom && phase2Lineage.parentLineage.length > 0}
        {@const parent = phase2Lineage.parentLineage[phase2Lineage.parentLineage.length - 1]}
        {@const parentName = nonEmpty(parent.name) ?? ('0x' + parent.spaceId.slice(0, 8) + '…')}
        <div class="fork-of-callout">
          <span class="fork-of-avatar" aria-hidden="true">{(parentName.trim().charAt(0) || '⑂').toUpperCase()}</span>
          <span class="fork-of-body">
            <span class="fork-of-label">This is a fork of</span>
            <span class="fork-of-name">{parentName}</span>
          </span>
          {#if localNavIds.has(parent.spaceId)}
            <button class="fork-of-open" onclick={() => onForkLineageNavigate?.(parent.spaceId)}>Open <span aria-hidden="true">↗</span></button>
          {/if}
        </div>
      {/if}

      {#if phase2Lineage}
        <ForkLineageTree
          lineage={phase2Lineage}
          {descendants}
          {localNavIds}
          resolveLocalName={resolveLocalCommunityName}
          onNavigate={(spaceId) => onForkLineageNavigate?.(spaceId)}
        />
        {#if phase2Lineage.forkedFrom || descendants.length > 0}
          <button class="view-genealogy-btn" onclick={() => (genealogyOpen = true)}>
            View genealogy <span aria-hidden="true">→</span>
          </button>
        {/if}
      {/if}

      {#if onFork}
        <button class="fork-btn fork-this-community" onclick={() => { forkDialogOpen = true; forkError = null; }}>
          Fork this community
        </button>
        {#if forkError}
          <p class="fork-error">{forkError}</p>
        {/if}
      {/if}
    </div>

    <div class="section danger-zone">
      <div class="section-label">Danger zone</div>
      <button class="leave-btn" onclick={() => {
        if (amOnlyAdmin) {
          lastAdminLeaveDialogOpen = true;
        } else {
          leaveOpen = true;
        }
      }}>Leave community</button>
      {#if amOnlyAdmin}
        <p class="hint">As the only admin, leaving will leave the community without an admin until another member is promoted.</p>
      {/if}
    </div>
  </div>
</div>

{#if kickTarget}
  <ConfirmationModal
    title={`Kick ${kickTarget.displayName ?? kickTarget.address.slice(0, 8)} from ${communityName}?`}
    description="They will be banned from rejoining. A future admin can re-invite them, but kick events can't be undone."
    confirmLabel={`Kick ${kickTarget.displayName ?? kickTarget.address.slice(0, 8)}`}
    danger={true}
    onConfirm={() => { onKick(kickTarget!.address); kickTarget = null; }}
    onCancel={() => (kickTarget = null)}
  />
{/if}

{#if setPowerTarget}
  <SetPowerDialog
    targetName={setPowerTarget.displayName ?? setPowerTarget.address.slice(0, 8)}
    targetAddress={setPowerTarget.address}
    currentPower={setPowerTarget.power}
    actorMaxPower={myPower}
    onSubmit={handleSetPowerSubmit}
    onCancel={() => (setPowerTarget = null)}
  />
{/if}

{#if pendingAdminChange}
  {@const promoting = pendingAdminChange.newPower >= ADMIN_TIER}
  {@const targetName = pendingAdminChange.target.displayName ?? pendingAdminChange.target.address.slice(0, 8)}
  <ConfirmationModal
    title={promoting
      ? `Promote ${targetName} to admin?`
      : `Demote ${targetName} from admin?`}
    description={promoting
      ? `${targetName} will gain admin powers — they'll be able to invite, kick, and set roles like you. Once two users are both admin, neither can demote the other through the UI; they would have to leave voluntarily.`
      : `${targetName} will lose admin powers — they'll keep their place in the community but won't be able to moderate anymore.`}
    confirmLabel={promoting ? `Promote to admin` : `Demote from admin`}
    danger={!promoting}
    onConfirm={() => {
      onSetPower(pendingAdminChange!.target.address, pendingAdminChange!.newPower);
      pendingAdminChange = null;
    }}
    onCancel={() => (pendingAdminChange = null)}
  />
{/if}

<LastAdminWarningDialog
  bind:open={lastAdminLeaveDialogOpen}
  action="leave"
  {communityName}
  onConfirm={async () => { onLeave(); }}
  onCancel={() => {}}
/>

{#if leaveOpen}
  <ConfirmationModal
    title={`Leave ${communityName}?`}
    description="You will lose access. You can rejoin via invite later if available."
    confirmLabel="Leave"
    danger={true}
    onConfirm={() => { onLeave(); leaveOpen = false; }}
    onCancel={() => (leaveOpen = false)}
  />
{/if}

{#if forkDialogOpen && onFork}
  <ForkConfirmDialog
    originalName={communityName}
    messageCount={lineage?.snapshotMessageCount ?? 0}
    onConfirm={async (opts) => {
      try {
        await onFork(opts);
        forkDialogOpen = false;
        forkError = null;
      } catch (e) {
        forkError = e instanceof Error ? e.message : String(e);
      }
    }}
    onCancel={() => { forkDialogOpen = false; forkError = null; }}
  />
{/if}

{#if genealogyOpen && phase2Lineage}
  <Modal onCancel={() => (genealogyOpen = false)} ariaLabelledby="genealogy-modal-title" canDismissOnBackdrop={true} wide={true}>
    <div class="genealogy-modal">
      <h3 class="genealogy-modal-title" id="genealogy-modal-title">
        <span aria-hidden="true">⑂</span> Lineage — {communityName}
      </h3>
      <ForkGenealogyGraph
        lineage={phase2Lineage}
        {descendants}
        {localNavIds}
        resolveLocalName={resolveLocalCommunityName}
        onNavigate={(spaceId) => {
          genealogyOpen = false;
          onForkLineageNavigate?.(spaceId);
        }}
      />
    </div>
  </Modal>
{/if}

<style>
  .panel-overlay {
    position: fixed;
    inset: 0;
    background: var(--overlay);
    display: flex;
    align-items: flex-start;
    justify-content: center;
    z-index: 900;
    overflow-y: auto;
    padding: 32px 16px;
  }
  .panel {
    background: var(--bg-secondary);
    border-radius: 8px;
    border: 1px solid var(--border);
    max-width: 640px;
    width: 100%;
    box-shadow: 0 8px 24px var(--shadow-heavy);
  }
  .header {
    padding: 16px 20px;
    border-bottom: 1px solid var(--border);
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  .panel-title {
    color: var(--text-primary);
    margin: 0;
    font-family: var(--font-display);
    font-weight: 500;
    font-size: 1.15rem;
  }
  .subtitle { color: var(--text-secondary); font-size: 0.75rem; }
  .close-btn {
    background: transparent;
    color: var(--text-secondary);
    border: none;
    font-size: 1.1rem;
    padding: 4px 10px;
    cursor: pointer;
  }
  .section {
    padding: 18px 20px;
    border-bottom: 1px solid var(--border);
  }
  .section:last-child { border-bottom: none; }
  .section-label {
    font-size: 10.5px;
    font-weight: 600;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.1em;
    margin-bottom: 12px;
  }
  .danger-zone .section-label {
    color: var(--vote-against);
  }
  .info-grid {
    display: grid;
    grid-template-columns: 120px 1fr;
    gap: 10px 16px;
    font-size: 0.8rem;
    color: var(--text-primary);
  }
  .info-grid .key { color: var(--text-secondary); }
  .healthy { color: var(--presence-online); }
  .degraded { color: var(--role-mod); }
  .muted { color: var(--text-secondary); }
  .member-list {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .member-row {
    display: flex;
    align-items: center;
    padding: 8px 10px;
    gap: 10px;
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 8px;
    box-shadow: var(--shadow-e1);
  }
  .avatar {
    width: 28px;
    height: 28px;
    border-radius: 50%;
    background: var(--accent);
    color: var(--on-accent);
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: 0.75rem;
    font-weight: bold;
  }
  .member-name { flex: 1; }
  .member-name .name { color: var(--text-primary); font-size: 0.8rem; }
  .member-name .addr { font-size: 0.65rem; color: var(--text-secondary); font-family: var(--font-mono); }
  .set-role,
  .kick {
    font-size: 11px;
    font-weight: 600;
    padding: 2px 4px;
    border: none;
    background: none;
    border-radius: 3px;
    cursor: pointer;
  }
  .set-role { color: var(--vote-for); }
  .kick { color: var(--vote-against); }
  .leave-btn {
    background: color-mix(in srgb, var(--vote-against) 8%, var(--surface-raised));
    color: var(--vote-against);
    border: 1px solid var(--danger-border-muted);
    padding: 6px 14px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 0.8rem;
    font-weight: 600;
  }
  .hint {
    font-size: 0.7rem;
    color: var(--text-secondary);
    margin: 8px 0 0 0;
  }
  .manage-members-btn {
    display: block;
    margin-top: 10px;
    padding: 6px 10px;
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 7px;
    color: var(--text-secondary);
    font-size: 0.75rem;
    cursor: pointer;
    text-align: left;
    width: 100%;
  }
  .manage-members-btn:hover {
    color: var(--text-primary);
    border-color: var(--accent);
  }
  .manage-members-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .member-search { margin-bottom: 12px; }
  .search-input {
    width: 100%;
    padding: 6px 10px;
    background: var(--input-bg);
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
  .close-btn:focus-visible,
  .leave-btn:focus-visible,
  .set-role:focus-visible,
  .kick:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .toggle-row {
    display: flex;
    gap: 8px;
    align-items: center;
    cursor: pointer;
    padding: 6px 0;
  }
  .toggle-label {
    font-size: 0.8rem;
    color: var(--text-primary);
  }
  .toggle-help {
    font-size: 0.7rem;
    color: var(--text-secondary);
    margin: 4px 0 0;
  }
  /* ZEB-287 Phase 2: explainer paragraph in the Forks section. */
  .forks-explainer {
    font-size: 0.8rem;
    color: var(--text-secondary);
    margin: 0 0 0.75rem;
    line-height: 1.4;
  }
  .fork-of-callout {
    display: flex;
    align-items: center;
    gap: 10px;
    background: var(--primary-soft);
    border: 1px solid var(--primary-border);
    border-radius: 9px;
    padding: 8px 12px;
    margin: 0 0 12px;
  }
  .fork-of-avatar {
    flex: 0 0 auto;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 30px;
    height: 30px;
    border-radius: 8px;
    background: var(--accent);
    color: var(--on-accent);
    font-family: var(--font-display);
    font-weight: 600;
  }
  .fork-of-body { display: flex; flex-direction: column; gap: 1px; min-width: 0; }
  .fork-of-label {
    font-size: 0.62rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--primary-deep);
  }
  .fork-of-name {
    font-family: var(--font-display);
    font-size: 0.95rem;
    color: var(--text-primary);
  }
  .fork-of-open {
    margin-left: auto;
    flex: 0 0 auto;
    background: var(--surface-raised);
    color: var(--primary-deep);
    border: 1px solid var(--primary-border);
    border-radius: 6px;
    padding: 4px 10px;
    font-size: 0.75rem;
    cursor: pointer;
  }
  .view-genealogy-btn {
    /* ZEB-649: Frame D's deferred "View full lineage tree" affordance. */
    align-self: flex-start;
    background: var(--surface-raised);
    color: var(--gov-clay-deep);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 30%, transparent);
    border-radius: 7px;
    padding: 6px 12px;
    font-size: 0.8rem;
    cursor: pointer;
    margin-top: 8px;
  }
  .view-genealogy-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .genealogy-modal {
    display: flex;
    flex-direction: column;
    gap: 12px;
    /* The graph needs more room than Modal's 480px default. */
    width: min(860px, 90vw);
  }
  .genealogy-modal-title {
    margin: 0;
    font-family: var(--font-display);
    font-size: 1.1rem;
    color: var(--text-primary);
  }
  .fork-btn {
    background: var(--gov-clay-soft);
    color: var(--gov-clay-deep);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised));
    padding: 6px 14px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 0.8rem;
  }
  .fork-btn:hover {
    border-color: var(--gov-clay);
  }
  .fork-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .fork-error {
    font-size: 0.7rem;
    color: var(--danger-text-muted);
    margin: 8px 0 0;
  }
  /* ZEB-250: admin governance section */
  .admin-quorum-info {
    font-size: 0.8rem;
    color: var(--text-secondary);
    margin: 0 0 8px;
  }
  .change-quorum-btn {
    background: var(--surface-raised);
    color: var(--text-secondary);
    border: 1px solid var(--border);
    padding: 4px 10px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 0.75rem;
    margin-bottom: 8px;
  }
  .change-quorum-btn:hover {
    color: var(--text-primary);
    border-color: var(--accent);
  }
  .change-quorum-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  /* ZEB-714: admin recovery section */
  .recovery-nudge {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.5rem 0.6rem;
    margin-bottom: 8px;
    border-radius: 7px;
    background: var(--gov-clay-soft);
    color: var(--gov-clay-deep);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised));
  }
  .recovery-nudge-text {
    flex: 1;
    font-size: 0.8rem;
  }
  .recovery-nudge-dismiss {
    border: none;
    background: transparent;
    color: var(--gov-clay-deep);
    font: inherit;
    cursor: pointer;
    padding: 2px 6px;
  }
  .recovery-config-info,
  .recovery-designate-info {
    font-size: 0.8rem;
    color: var(--text-secondary);
    margin: 0 0 8px;
  }
  .recovery-config-btn,
  .recovery-initiate-btn {
    background: var(--surface-raised);
    color: var(--text-secondary);
    border: 1px solid var(--border);
    padding: 4px 10px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 0.75rem;
    margin-bottom: 8px;
  }
  .recovery-config-btn:hover,
  .recovery-initiate-btn:hover {
    color: var(--text-primary);
    border-color: var(--accent);
  }
  .recovery-config-btn:focus-visible,
  .recovery-initiate-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  /* ZEB-250: member-row pending badge */
  .pending-badge {
    font-size: 0.65rem;
    color: var(--role-mod);
    white-space: nowrap;
  }
</style>
