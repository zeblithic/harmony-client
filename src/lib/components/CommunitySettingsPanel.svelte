<script lang="ts">
  import { trapFocus } from '../actions/trap-focus';
  import { POWER_THRESHOLDS, powerToRole, type CommunityMember } from '../types';
  import ConfirmationModal from './ConfirmationModal.svelte';
  import SetPowerDialog from './SetPowerDialog.svelte';
  import LastAdminWarningDialog from './LastAdminWarningDialog.svelte';
  import InviteLinkManager from './InviteLinkManager.svelte';
  import ForkConfirmDialog from './ForkConfirmDialog.svelte';

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
    /** Optional: if provided, a "Manage members" button appears in the Members
     *  section that opens the full CommunityMembersPanel overlay (with recent
     *  moderation history). Callers that don't yet thread through communityService
     *  can omit this to keep the inline member list. */
    onOpenMembersPanel?: () => void;
    /** ZEB-285: if provided, the "Fork this community" button is wired to this
     *  callback which handles the actual IPC call + nav transition. */
    onFork?: (opts: { name: string; silent: boolean; alsoLeave: boolean }) => Promise<void>;
    /** ZEB-285: fork lineage metadata — present when this community was forked
     *  from another. Populated by the caller via get_community_lineage IPC. */
    lineage?: {
      originalCommunityName: string | null;
      forkedAtMs: number;
      snapshotMessageCount: number;
    } | null;
  } = $props();

  let kickTarget = $state<CommunityMember | null>(null);
  let setPowerTarget = $state<CommunityMember | null>(null);
  // ZEB-285: fork dialog state
  let forkDialogOpen = $state(false);
  let forkError = $state<string | null>(null);
  // Holds an admin-threshold-crossing power change pending tier-2
  // confirmation. Populated by SetPowerDialog onSubmit when the new
  // power crosses POWER_THRESHOLDS.setPower in either direction; the
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

  function crossesAdminThreshold(currentPower: number, newPower: number): boolean {
    const threshold = POWER_THRESHOLDS.setPower;
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

  let joinedMembers = $derived(members.filter((m) => m.status === 'joined'));
  let adminCount = $derived(joinedMembers.filter((m) => m.power >= POWER_THRESHOLDS.setPower).length);
  let amOnlyAdmin = $derived(
    myPower >= POWER_THRESHOLDS.setPower &&
    adminCount === 1 &&
    joinedMembers.some((m) => m.address === myAddress && m.power >= POWER_THRESHOLDS.setPower)
  );
  let myRole = $derived(powerToRole(myPower));

  let search = $state('');
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
              (m.displayName?.toLowerCase().includes(q) ?? false) ||
              m.address.toLowerCase().includes(q)
          );
        })()
  );

  function canKick(target: CommunityMember): boolean {
    return target.address !== myAddress
      && myPower >= POWER_THRESHOLDS.kick
      && myPower > target.power;
  }

  function canSetPower(target: CommunityMember): boolean {
    return target.address !== myAddress
      && myPower >= POWER_THRESHOLDS.setPower
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
</script>

<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
<div
  class="panel-overlay"
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
          <span class="role-badge" data-role={myRole}>{myRole.toUpperCase()}</span>
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
          <div class="member-row">
            <div class="avatar">{(m.displayName ?? m.address).slice(0, 1).toUpperCase()}</div>
            <div class="member-name">
              <div class="name">{m.displayName ?? m.address.slice(0, 8)}{m.address === myAddress ? ' (you)' : ''}</div>
              <div class="addr">{m.address}</div>
            </div>
            <span class="role-badge" data-role={powerToRole(m.power)}>{powerToRole(m.power).toUpperCase()}</span>
            {#if canSetPower(m)}
              <button class="set-role" onclick={() => (setPowerTarget = m)}>Set role</button>
            {/if}
            {#if canKick(m)}
              <button class="kick" onclick={() => (kickTarget = m)}>Kick</button>
            {/if}
          </div>
        {/each}
      </div>
      {#if onOpenMembersPanel}
        <button class="manage-members-btn" onclick={onOpenMembersPanel}>
          Manage members &amp; moderation history →
        </button>
      {/if}
    </div>

    {#if myPower >= POWER_THRESHOLDS.invite}
      <div class="section">
        <div class="section-label">Invites</div>
        <InviteLinkManager kind={communityKind} onGenerate={onGenerateInvite} />
      </div>
    {/if}

    {#if lineage}
      <div class="section">
        <div class="section-label">Lineage</div>
        <dl class="lineage-grid">
          <dt>Forked from</dt>
          <dd>{lineage.originalCommunityName ?? 'another community'}</dd>
          <dt>Forked at</dt>
          <dd>{new Date(lineage.forkedAtMs).toUTCString()}</dd>
          <dt>Snapshot</dt>
          <dd>{lineage.snapshotMessageCount} messages bundled</dd>
        </dl>
      </div>
    {/if}

    {#if onFork}
      <div class="section">
        <div class="section-label">Fork</div>
        <button class="fork-btn" onclick={() => { forkDialogOpen = true; forkError = null; }}>
          Fork this community
        </button>
        <p class="toggle-help">
          Creates a new community with a frozen copy of the history you can see here.
        </p>
        {#if forkError}
          <p class="fork-error">{forkError}</p>
        {/if}
      </div>
    {/if}

    <div class="section">
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
  {@const promoting = pendingAdminChange.newPower >= POWER_THRESHOLDS.setPower}
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

<style>
  .panel-overlay {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
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
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.6);
  }
  .header {
    padding: 16px 20px;
    border-bottom: 1px solid var(--border);
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  .panel-title { color: var(--text-primary); margin: 0; font-size: 1.1rem; }
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
    font-size: 0.7rem;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    margin-bottom: 12px;
  }
  .info-grid {
    display: grid;
    grid-template-columns: 120px 1fr;
    gap: 10px 16px;
    font-size: 0.8rem;
    color: var(--text-primary);
  }
  .info-grid .key { color: var(--text-secondary); }
  .role-badge {
    padding: 1px 7px;
    border-radius: 8px;
    font-size: 0.6rem;
    font-weight: bold;
  }
  .role-badge[data-role="member"] { background: var(--bg-tertiary); color: var(--text-secondary); }
  .role-badge[data-role="mod"] { background: #ffb84a; color: #1a1a1a; }
  .role-badge[data-role="admin"] { background: var(--accent); color: var(--text-primary); }
  .healthy { color: #7acc7a; }
  .degraded { color: #ffb84a; }
  .muted { color: var(--text-secondary); }
  .member-list { display: flex; flex-direction: column; }
  .member-row {
    display: flex;
    align-items: center;
    padding: 6px 6px;
    gap: 10px;
    border-bottom: 1px solid var(--border);
  }
  .member-row:last-child { border-bottom: none; }
  .avatar {
    width: 28px;
    height: 28px;
    border-radius: 50%;
    background: var(--accent);
    color: var(--text-primary);
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: 0.75rem;
    font-weight: bold;
  }
  .member-name { flex: 1; }
  .member-name .name { color: var(--text-primary); font-size: 0.8rem; }
  .member-name .addr { font-size: 0.65rem; color: var(--text-secondary); font-family: monospace; }
  .set-role,
  .kick {
    font-size: 0.65rem;
    padding: 2px 7px;
    border-radius: 3px;
    cursor: pointer;
  }
  .set-role {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    border: 1px solid var(--border);
  }
  .kick {
    background: var(--bg-tertiary);
    color: #cc7a7a;
    border: 1px solid #553333;
  }
  .leave-btn {
    background: var(--bg-tertiary);
    color: #cc7a7a;
    border: 1px solid #553333;
    padding: 6px 14px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.8rem;
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
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-secondary);
    font-size: 0.75rem;
    cursor: pointer;
    text-align: left;
    width: 100%;
  }
  .manage-members-btn:hover {
    color: var(--text-primary);
    border-color: var(--accent, #5865f2);
  }
  .manage-members-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
  .member-search { margin-bottom: 12px; }
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
  .close-btn:focus-visible,
  .leave-btn:focus-visible,
  .set-role:focus-visible,
  .kick:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
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
  .lineage-grid {
    display: grid;
    grid-template-columns: 120px 1fr;
    gap: 8px 16px;
    font-size: 0.8rem;
    margin: 0;
  }
  .lineage-grid dt {
    color: var(--text-secondary);
  }
  .lineage-grid dd {
    color: var(--text-primary);
    margin: 0;
  }
  .fork-btn {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    border: 1px solid var(--border);
    padding: 6px 14px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.8rem;
  }
  .fork-btn:hover {
    color: var(--text-primary);
    border-color: var(--accent, #5865f2);
  }
  .fork-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
  .fork-error {
    font-size: 0.7rem;
    color: #cc7a7a;
    margin: 8px 0 0;
  }
</style>
