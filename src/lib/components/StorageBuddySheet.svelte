<script lang="ts">
  import { untrack } from 'svelte';
  import Modal from './Modal.svelte';
  import Avatar from './Avatar.svelte';
  import type { Profile } from '../types';
  import type { ContributionSummaryDto, StorageBuddyDto } from '../storage-buddy-service';
  import { formatBytes } from '../file-utils';
  import { nonEmpty } from '../display-label';

  let {
    buddies,
    summary,
    friendContacts,
    onClose,
    onSetPledge,
    onRemove,
    onSetBudget,
  }: {
    buddies: StorageBuddyDto[];
    /** `null` only if the summary IPC failed — budget controls disable. */
    summary: ContributionSummaryDto | null;
    /** Active friends (addr → Profile) for the invite picker. */
    friendContacts: Map<string, Profile>;
    onClose: () => void;
    /** Pledge bytes to an owner (invite, accept, or adjustment). */
    onSetPledge: (ownerAddress: string, bytes: number) => Promise<void>;
    /** Remove a pact / cancel an outgoing invite / decline an incoming one. */
    onRemove: (ownerAddress: string) => Promise<void>;
    onSetBudget: (bytes: number) => Promise<void>;
  } = $props();

  const GB = 1_000_000_000; // decimal GB — matches the backend's 10 GB default

  let active = $derived(buddies.filter((b) => b.status === 'active'));
  let incoming = $derived(buddies.filter((b) => b.status === 'pendingIncoming'));
  let outgoing = $derived(buddies.filter((b) => b.status === 'pendingOutgoing'));

  let sheetError = $state<string | null>(null);

  /** Wrap an action promise: surface rejection copy inline (prod rejections
   *  are strings — normalized to Error by the services). */
  async function run(p: Promise<void>): Promise<void> {
    sheetError = null;
    try {
      await p;
    } catch (e) {
      sheetError = e instanceof Error ? e.message : String(e);
    }
  }

  function label(b: StorageBuddyDto): string {
    // A local petName and a peer's published card displayName both lack a
    // non-blank constraint — guard each with nonEmpty() so a whitespace value
    // falls through to the short address instead of rendering a blank name.
    return (
      nonEmpty(b.petName) ??
      nonEmpty(friendContacts.get(b.ownerAddress)?.displayName) ??
      shortAddr(b.ownerAddress)
    );
  }

  function shortAddr(addr: string): string {
    return addr.length > 12 ? `${addr.slice(0, 6)}…${addr.slice(-4)}` : addr;
  }

  function reportAge(ms: number): string {
    if (ms < 60_000) return 'just now';
    if (ms < 3_600_000) return `${Math.round(ms / 60_000)}m ago`;
    return `${Math.round(ms / 3_600_000)}h ago`;
  }

  // ── Shared budget (slider + number pair, bidirectionally synced) ────────
  // Bidirectional sync: both inputs share the same $state (ChangeQuorumDialog
  // idiom). Late-arriving or event-refreshed summaries re-seed the pair
  // unless the user is mid-edit (Qodo PR #450 — a once-only untrack seed
  // went stale when `summary` started null or changed while open).
  let budgetGb = $state(untrack(() => (summary ? summary.budgetBytes / GB : 0)));
  let budgetDirty = $state(false);
  $effect(() => {
    const bytes = summary?.budgetBytes;
    if (bytes != null && !budgetDirty) budgetGb = bytes / GB;
  });
  function clampBudgetOnBlur() {
    if (Number.isNaN(budgetGb) || !Number.isFinite(budgetGb)) budgetGb = 0;
    if (budgetGb < 0) budgetGb = 0;
  }
  async function commitBudget() {
    clampBudgetOnBlur();
    await run(onSetBudget(Math.round(budgetGb * GB)));
    // Commit round-trips through the backend event → refreshed summary is
    // authoritative again.
    budgetDirty = false;
  }

  // ── Per-buddy pledge editing ─────────────────────────────────────────────
  /** Local in-progress pledge edits (GB), keyed by owner address; the shown
   *  value falls back to the wire pledge until the user touches a control. */
  let pledgeEdits = $state<Record<string, number>>({});
  function pledgeGb(b: StorageBuddyDto): number {
    return pledgeEdits[b.ownerAddress] ?? b.myPledgeBytes / GB;
  }
  function setPledgeEdit(addr: string, raw: number) {
    let v = Number.isFinite(raw) ? raw : 0;
    if (v < 0) v = 0;
    pledgeEdits[addr] = v;
  }
  function commitPledge(b: StorageBuddyDto) {
    void run(onSetPledge(b.ownerAddress, Math.round(pledgeGb(b) * GB)));
  }
  /** Slider max: the shared budget (you can't honestly pledge past it). */
  let pledgeMaxGb = $derived(Math.max(1, summary ? Math.ceil(summary.budgetBytes / GB) : 1));
  /** Per-row max: an already-over-budget pledge must still be representable
   *  (browser range inputs silently clamp value to max — Qodo PR #450). */
  function sliderMaxGb(b: StorageBuddyDto): number {
    return Math.max(pledgeMaxGb, Math.ceil(pledgeGb(b)));
  }

  // ── Tier-2 remove confirm (VoiceChannelView arm-token idiom) ────────────
  let confirmingRemove = $state<string | null>(null);
  function askRemove(addr: string) {
    confirmingRemove = addr;
  }
  function doRemove(addr: string) {
    confirmingRemove = null;
    void run(onRemove(addr));
  }
  function onWindowClick(e: MouseEvent) {
    if (!confirmingRemove) return;
    const t = e.target as HTMLElement | null;
    if (!t?.closest?.('.buddy-actions')) confirmingRemove = null;
  }

  // ── Invite-a-friend picker ───────────────────────────────────────────────
  let inviteQuery = $state('');
  let selectedInvite = $state<string | null>(null);
  let invitePledgeGb = $state(0);
  let buddyAddrs = $derived(new Set(buddies.map((b) => b.ownerAddress)));
  let inviteCandidates = $derived.by(() => {
    const q = inviteQuery.trim().toLowerCase();
    return [...friendContacts.entries()]
      .filter(([addr]) => !buddyAddrs.has(addr))
      .filter(([addr, p]) => {
        if (!q) return true;
        return (p.displayName ?? '').toLowerCase().includes(q) || addr.startsWith(q);
      })
      .slice(0, 50);
  });
  function clampInviteOnBlur() {
    if (Number.isNaN(invitePledgeGb) || !Number.isFinite(invitePledgeGb)) invitePledgeGb = 0;
    if (invitePledgeGb < 0) invitePledgeGb = 0;
  }
  function sendInvite() {
    if (!selectedInvite) return;
    clampInviteOnBlur();
    const addr = selectedInvite;
    void run(
      onSetPledge(addr, Math.round(invitePledgeGb * GB)).then(() => {
        selectedInvite = null;
        invitePledgeGb = 0;
        inviteQuery = '';
      }),
    );
  }
</script>

<svelte:window onclick={onWindowClick} />

<Modal onCancel={onClose} ariaLabelledby="storage-buddy-title">
  <div class="sheet-body">
    <h3 id="storage-buddy-title">Storage buddies</h3>
    <p class="sheet-hint">
      Buddies auto-pin each other's flagged files inside a shared budget. Reports are aggregate —
      what a buddy claims to hold for you, signed by them.
    </p>

    {#if sheetError}
      <p class="sheet-error" role="alert">{sheetError}</p>
    {/if}

    <section class="sheet-section" aria-label="Shared budget">
      <h4>Shared budget</h4>
      <div class="control-row">
        <input
          type="range"
          min="0"
          max="100"
          step="1"
          bind:value={budgetGb}
          disabled={summary == null}
          oninput={() => (budgetDirty = true)}
          onchange={() => void commitBudget()}
          aria-label="Shared budget slider (GB)"
          data-testid="budget-slider"
        />
        <input
          type="number"
          min="0"
          step="0.1"
          bind:value={budgetGb}
          disabled={summary == null}
          oninput={() => (budgetDirty = true)}
          onblur={() => void commitBudget()}
          aria-label="Shared budget (GB)"
          data-testid="budget-number"
        />
        <span class="unit-label">GB</span>
      </div>
      {#if summary}
        <p class="section-note">
          {formatBytes(summary.hostedBytes)} of {formatBytes(summary.budgetBytes)} currently used.
        </p>
      {/if}
    </section>

    <section class="sheet-section" aria-label="Active buddies">
      <h4>Active</h4>
      {#if active.length === 0}
        <p class="section-note">No active pacts yet.</p>
      {/if}
      {#each active as b (b.ownerAddress)}
        <div class="buddy-row" data-testid="buddy-row-{b.ownerAddress}">
          <div class="buddy-id">
            <Avatar address={b.ownerAddress} displayName={label(b)} size={24} />
            <div class="buddy-names">
              <span class="buddy-name">{label(b)}</span>
              <span class="buddy-report">
                {#if b.theyReportHoldingBytes != null}
                  They hold {formatBytes(b.theyReportHoldingBytes)} for you
                  {#if b.reportAgeMs != null}
                    · {reportAge(b.reportAgeMs)}
                  {/if}
                {:else}
                  No report yet
                {/if}
              </span>
            </div>
          </div>
          <div class="pledge-controls">
            <span class="pledge-label">My pledge</span>
            <div class="control-row">
              <input
                type="range"
                min="0"
                max={sliderMaxGb(b)}
                step="0.5"
                value={pledgeGb(b)}
                oninput={(e) => setPledgeEdit(b.ownerAddress, Number(e.currentTarget.value))}
                onchange={() => commitPledge(b)}
                aria-label="Pledge to {label(b)} (GB slider)"
                data-testid="pledge-slider"
              />
              <input
                type="number"
                min="0"
                step="0.1"
                value={pledgeGb(b)}
                oninput={(e) => setPledgeEdit(b.ownerAddress, Number(e.currentTarget.value))}
                onblur={() => commitPledge(b)}
                aria-label="Pledge to {label(b)} (GB)"
                data-testid="pledge-number"
              />
              <span class="unit-label">GB</span>
            </div>
            <span class="section-note">Hosting {formatBytes(b.hostedForThemBytes)} for them</span>
          </div>
          <div class="buddy-actions">
            {#if confirmingRemove === b.ownerAddress}
              <button
                type="button"
                class="row-btn danger"
                data-testid="buddy-remove-confirm"
                onclick={() => doRemove(b.ownerAddress)}
                aria-label="Confirm remove buddy"
              >Confirm</button>
            {:else}
              <button
                type="button"
                class="row-btn"
                data-testid="buddy-remove"
                onclick={() => askRemove(b.ownerAddress)}
                aria-label="Remove buddy"
              >Remove</button>
            {/if}
          </div>
        </div>
      {/each}
    </section>

    {#if incoming.length > 0}
      <section class="sheet-section" aria-label="Invites for you">
        <h4>Invites for you</h4>
        {#each incoming as b (b.ownerAddress)}
          <div class="buddy-row" data-testid="buddy-row-{b.ownerAddress}">
            <div class="buddy-id">
              <Avatar address={b.ownerAddress} displayName={label(b)} size={24} />
              <div class="buddy-names">
                <span class="buddy-name">{label(b)}</span>
                <span class="buddy-report">
                  Offers {formatBytes(b.theirPledgeBytes ?? 0)}
                </span>
              </div>
            </div>
            <div class="buddy-actions">
              <button
                type="button"
                class="row-btn primary"
                data-testid="buddy-accept"
                onclick={() => void run(onSetPledge(b.ownerAddress, 0))}
              >Accept</button>
              <button
                type="button"
                class="row-btn"
                data-testid="buddy-decline"
                onclick={() => void run(onRemove(b.ownerAddress))}
              >Decline</button>
            </div>
          </div>
        {/each}
      </section>
    {/if}

    {#if outgoing.length > 0}
      <section class="sheet-section" aria-label="Sent invites">
        <h4>Sent invites</h4>
        {#each outgoing as b (b.ownerAddress)}
          <div class="buddy-row" data-testid="buddy-row-{b.ownerAddress}">
            <div class="buddy-id">
              <Avatar address={b.ownerAddress} displayName={label(b)} size={24} />
              <div class="buddy-names">
                <span class="buddy-name">{label(b)}</span>
                <span class="buddy-report">
                  You pledged {formatBytes(b.myPledgeBytes)} · awaiting their pledge
                </span>
              </div>
            </div>
            <div class="buddy-actions">
              {#if confirmingRemove === b.ownerAddress}
                <button
                  type="button"
                  class="row-btn danger"
                  data-testid="invite-cancel-confirm"
                  onclick={() => doRemove(b.ownerAddress)}
                >Confirm</button>
              {:else}
                <button
                  type="button"
                  class="row-btn"
                  data-testid="invite-cancel"
                  onclick={() => askRemove(b.ownerAddress)}
                >Cancel</button>
              {/if}
            </div>
          </div>
        {/each}
      </section>
    {/if}

    <section class="sheet-section" aria-label="Invite a friend">
      <h4>Invite a friend</h4>
      {#if friendContacts.size === 0}
        <p class="section-note">Add friends first — buddies come from your friend list.</p>
      {:else}
        <input
          type="search"
          class="invite-search"
          placeholder="Search friends…"
          bind:value={inviteQuery}
          aria-label="Search friends"
          data-testid="invite-search"
        />
        {#if inviteCandidates.length === 0}
          <p class="section-note">No eligible friends match.</p>
        {/if}
        <div class="invite-list">
          {#each inviteCandidates as [addr, profile] (addr)}
            <button
              type="button"
              class="invite-candidate"
              class:selected={selectedInvite === addr}
              data-testid="invite-candidate-{addr}"
              onclick={() => (selectedInvite = selectedInvite === addr ? null : addr)}
            >
              <Avatar address={addr} displayName={nonEmpty(profile.displayName) ?? shortAddr(addr)} size={24} />
              {nonEmpty(profile.displayName) ?? shortAddr(addr)}
            </button>
          {/each}
        </div>
        {#if selectedInvite}
          <div class="control-row">
            <input
              type="range"
              min="0"
              max={pledgeMaxGb}
              step="0.5"
              bind:value={invitePledgeGb}
              aria-label="Pledge to invite (GB slider)"
              data-testid="invite-pledge-slider"
            />
            <input
              type="number"
              min="0"
              step="0.1"
              bind:value={invitePledgeGb}
              onblur={clampInviteOnBlur}
              aria-label="Pledge to invite (GB)"
              data-testid="invite-pledge-number"
            />
            <span class="unit-label">GB</span>
            <button type="button" class="row-btn primary" data-testid="invite-send" onclick={sendInvite}>
              Send invite · pledge {formatBytes(Math.round(Math.max(0, invitePledgeGb || 0) * GB))}
            </button>
          </div>
        {/if}
      {/if}
    </section>

    <div class="sheet-footer">
      <button type="button" class="row-btn" data-testid="sheet-done" onclick={onClose}>Done</button>
    </div>
  </div>
</Modal>

<style>
  .sheet-body {
    display: flex;
    flex-direction: column;
    gap: 12px;
    max-height: 70vh;
    overflow-y: auto;
  }

  h3 {
    margin: 0;
    font-size: 1rem;
  }

  h4 {
    margin: 0 0 6px;
    font-size: 0.8rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--text-muted);
  }

  .sheet-hint,
  .section-note {
    margin: 0;
    font-size: 0.75rem;
    color: var(--text-muted);
  }

  .sheet-error {
    margin: 0;
    font-size: 0.8rem;
    color: var(--danger);
  }

  .sheet-section {
    border-top: 1px solid var(--border);
    padding-top: 8px;
  }

  .buddy-row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
    padding: 6px 0;
  }

  .buddy-id {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
    flex: 1;
  }

  .buddy-names {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }

  .buddy-name {
    font-size: 0.85rem;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .buddy-report {
    font-size: 0.7rem;
    color: var(--text-muted);
    font-variant-numeric: tabular-nums;
  }

  .pledge-controls {
    display: flex;
    flex-direction: column;
    gap: 2px;
    flex-basis: 100%;
  }

  .pledge-label {
    font-size: 0.7rem;
    color: var(--text-secondary);
  }

  .control-row {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .control-row input[type='range'] {
    flex: 1;
  }

  .control-row input[type='number'] {
    width: 5rem;
  }

  .unit-label {
    font-size: 0.75rem;
    color: var(--text-muted);
  }

  .buddy-actions {
    display: flex;
    gap: 6px;
  }

  .row-btn {
    padding: 3px 10px;
    font-size: 0.75rem;
    color: var(--text-secondary);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 6px;
    cursor: pointer;
  }

  .row-btn:hover {
    background: var(--bg-secondary);
    color: var(--text-primary);
  }

  .row-btn.primary {
    color: var(--on-accent);
    background: var(--accent);
    border-color: var(--accent);
  }

  .row-btn.primary:hover {
    background: var(--accent-hover);
  }

  .row-btn.danger {
    color: var(--danger);
    border-color: var(--danger);
  }

  .invite-search {
    width: 100%;
    padding: 4px 8px;
    font-size: 0.8rem;
    color: var(--text-primary);
    background: var(--input-bg);
    border: 1px solid var(--border);
    border-radius: 6px;
    margin-bottom: 6px;
  }

  .invite-list {
    display: flex;
    flex-direction: column;
    gap: 2px;
    max-height: 160px;
    overflow-y: auto;
  }

  .invite-candidate {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 6px;
    font-size: 0.8rem;
    color: var(--text-primary);
    background: transparent;
    border: 1px solid transparent;
    border-radius: 6px;
    cursor: pointer;
    text-align: left;
  }

  .invite-candidate:hover {
    background: var(--bg-secondary);
  }

  .invite-candidate.selected {
    border-color: var(--accent);
    background: var(--bg-secondary);
  }

  .sheet-footer {
    display: flex;
    justify-content: flex-end;
    border-top: 1px solid var(--border);
    padding-top: 8px;
  }
</style>
