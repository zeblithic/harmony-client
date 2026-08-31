<script lang="ts">
  /**
   * ZEB-1031 §9 — D-FROST committee-reset admin panel. Mounted inside
   * CommunitySettingsPanel alongside the ZEB-714 admin-recovery section
   * (same file, same mount pattern) — but reading is NOT admin-gated:
   * `get_dfrost_reset_state` is "readable by any Joined member" (spec §9
   * doc comment on the impl), because the committee whose liveness is at
   * stake, and whose endorse/veto response actually matters, may not
   * overlap with the community's admin set at all. Only the "Propose a
   * reset" form and the admin-quorum Cosign button are power-gated
   * (mirrors `cosign_dfrost_reset`'s RS-C1 backend gate) — Endorse/Veto
   * are left visible to every viewer since the DTO carries no
   * "am I a committee member" signal; a non-member's attempt fails
   * cleanly with a readable IPC error rather than being hidden.
   *
   * Vocabulary (spec §1): this replaces a community's D-FROST
   * *committee* — the threshold-signing group backing Tier-3 secret
   * ballots + VRF sortition — NOT community admin identity (ZEB-714,
   * `RecoveryBanner`/`CommunitySettingsPanel`'s "Admin recovery"
   * section) and not fleet/device recovery. Copy below stays verbally
   * distinct from both.
   *
   * ZEB-1042: `get_dfrost_committee_summary` exposes the active
   * committee's vk/epoch/shape, closing the ZEB-1031 Task 9 gap where
   * the propose form's `target_vk_hex` / `target_epoch` had to be typed
   * in from out-of-band knowledge. When the summary loads with an
   * active committee the two fields are prefilled and locked
   * (read-only); if the read fails or no committee exists yet, the
   * manual inputs remain as the fallback — the backend validates either
   * way (RS-P mirror) and returns a readable error on mismatch.
   */
  import { onDestroy } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import type { CommunityMember } from '../types';
  import type {
    CosignDfrostResetResult,
    DfrostCommitteeSummaryDto,
    ProposeDfrostResetResult,
    ResetPhase,
    ResetProposalDto,
  } from '../dfrost-reset-types';
  import {
    RESET_VETO_WINDOW_CEILING_MS,
    RESET_VETO_WINDOW_DEFAULT_MS,
    RESET_VETO_WINDOW_FLOOR_MS,
    isActiveResetPhase,
  } from '../dfrost-reset-types';
  import GovConfirmModal from './governance/GovConfirmModal.svelte';

  const POLL_MS = 60_000;
  const TICK_MS = 1_000;
  const HOUR_MS = 3_600_000;
  const MIN_WINDOW_HOURS = RESET_VETO_WINDOW_FLOOR_MS / HOUR_MS; // 24
  const MAX_WINDOW_HOURS = RESET_VETO_WINDOW_CEILING_MS / HOUR_MS; // 720

  let {
    communityId,
    joinedMembers,
    canAdmin,
    adminQuorum = 1,
  }: {
    communityId: string;
    /** Joined-member roster — source for the successor multi-select and
     *  for resolving addresses to display names. */
    joinedMembers: CommunityMember[];
    /** Gates the "Propose a reset" form + the Cosign button (mirrors
     *  `cosign_dfrost_reset`'s RS-C1: caller must be a Joined power-100
     *  admin). Endorse/Veto stay visible to everyone — see file doc. */
    canAdmin: boolean;
    /** Community admin-quorum threshold (same value CommunitySettingsPanel
     *  already computes for the "Admin governance" section) — used to
     *  show "N of Q admins signed" during the Collecting phase. */
    adminQuorum?: number;
  } = $props();

  let proposals = $state<ResetProposalDto[]>([]);
  let loadError = $state<string | null>(null);
  // ZEB-1042: active-committee identity — backs the "Current committee"
  // line and prefills/locks the propose form's target vk/epoch.
  let committee = $state<DfrostCommitteeSummaryDto | null>(null);
  let committeeError = $state<string | null>(null);
  let nowMs = $state(Date.now());
  let tick = $state(Date.now());
  let latestCallId = 0;
  let latestCommitteeCallId = 0;
  let pollHandle: ReturnType<typeof setInterval> | null = null;
  let tickHandle: ReturnType<typeof setInterval> | null = null;

  let cosignBusy = $state<string | null>(null);
  let respondBusy = $state<string | null>(null);
  let actionError = $state<string | null>(null);

  // Propose-form state.
  let showProposeForm = $state(false);
  let targetVkHex = $state('');
  let targetEpoch = $state('');
  let selectedMembers = $state<Set<string>>(new Set());
  let newThreshold = $state(2);
  let vetoWindowHours = $state(RESET_VETO_WINDOW_DEFAULT_MS / HOUR_MS);
  let confirmingPropose = $state(false);
  let proposing = $state(false);
  let proposeError = $state<string | null>(null);

  async function refresh() {
    const myCallId = ++latestCallId;
    try {
      const result = await invoke<ResetProposalDto[]>('get_dfrost_reset_state', {
        communityId,
        nowMs: Date.now(),
      });
      if (myCallId !== latestCallId) return; // stale
      proposals = result;
      loadError = null;
      nowMs = Date.now();
    } catch (e) {
      if (myCallId !== latestCallId) return;
      loadError = e instanceof Error ? e.message : String(e);
    }
  }

  /** ZEB-1042: fetch the active committee summary. On success with an
   *  active committee, prefill the propose form's target fields — they
   *  render read-only in that state (`prefillLocked`), so this never
   *  clobbers manual input; when the summary is missing or pre-DKG the
   *  fields stay editable and untouched. */
  async function refreshCommittee() {
    const myCallId = ++latestCommitteeCallId;
    try {
      const summary = await invoke<DfrostCommitteeSummaryDto>('get_dfrost_committee_summary', {
        communityId,
      });
      if (myCallId !== latestCommitteeCallId) return; // stale
      committee = summary;
      committeeError = null;
      if (summary.active && summary.jointVk !== null) {
        targetVkHex = summary.jointVk;
        targetEpoch = String(summary.currentEpoch);
      }
    } catch (e) {
      if (myCallId !== latestCommitteeCallId) return;
      committeeError = e instanceof Error ? e.message : String(e);
      committee = null;
    }
  }

  $effect(() => {
    void communityId;
    proposals = [];
    loadError = null;
    actionError = null;
    // Clear cross-community carryover: a prefilled vk/epoch from the
    // previous community must never survive into this one's form.
    committee = null;
    committeeError = null;
    targetVkHex = '';
    targetEpoch = '';
    void refresh();
    void refreshCommittee();

    pollHandle = setInterval(() => {
      void refresh();
      void refreshCommittee();
    }, POLL_MS);
    tickHandle = setInterval(() => {
      tick = Date.now();
    }, TICK_MS);

    return () => {
      if (pollHandle) clearInterval(pollHandle);
      pollHandle = null;
      if (tickHandle) clearInterval(tickHandle);
      tickHandle = null;
    };
  });

  onDestroy(() => {
    if (pollHandle) clearInterval(pollHandle);
    pollHandle = null;
    if (tickHandle) clearInterval(tickHandle);
    tickHandle = null;
  });

  function nameFor(addr: string): string {
    return joinedMembers.find((m) => m.address === addr)?.displayName ?? addr.slice(0, 8);
  }

  function shortHex(hex: string): string {
    return hex.length <= 12 ? hex : `${hex.slice(0, 8)}…${hex.slice(-4)}`;
  }

  function phaseLabel(phase: ResetPhase): string {
    switch (phase) {
      case 'collecting':
        return 'Collecting signatures';
      case 'window':
        return 'Veto window open';
      case 'authorized':
        return 'Authorized';
      case 'consumed':
        return 'Consumed';
      case 'vetoed':
        return 'Vetoed';
      case 'expired':
        return 'Expired';
      case 'lapsed':
        return 'Lapsed';
    }
  }

  function formatCountdown(deadlineMs: number, asOfMs: number): string {
    const remaining = deadlineMs - asOfMs;
    if (remaining <= 0) return 'closing any moment';
    const totalMinutes = Math.floor(remaining / 60_000);
    const days = Math.floor(totalMinutes / 1440);
    const hours = Math.floor((totalMinutes % 1440) / 60);
    const minutes = totalMinutes % 60;
    if (days > 0) return `${days}d ${hours}h remaining`;
    if (hours > 0) return `${hours}h ${minutes}m remaining`;
    return `${Math.max(minutes, 1)}m remaining`;
  }

  async function cosign(p: ResetProposalDto) {
    cosignBusy = p.proposalEventId;
    actionError = null;
    try {
      await invoke<CosignDfrostResetResult>('cosign_dfrost_reset', {
        communityId,
        targetEventId: p.proposalEventId,
      });
      await refresh();
    } catch (e) {
      actionError = e instanceof Error ? e.message : String(e);
    } finally {
      if (cosignBusy === p.proposalEventId) cosignBusy = null;
    }
  }

  async function respond(p: ResetProposalDto, verdict: 'endorse' | 'veto') {
    const busyKey = `${p.proposalEventId}:${verdict}`;
    respondBusy = busyKey;
    actionError = null;
    try {
      await invoke('respond_dfrost_reset', {
        communityId,
        targetEventId: p.proposalEventId,
        verdict,
      });
      await refresh();
    } catch (e) {
      actionError = e instanceof Error ? e.message : String(e);
    } finally {
      if (respondBusy === busyKey) respondBusy = null;
    }
  }

  function toggleMember(addr: string) {
    const next = new Set(selectedMembers);
    if (next.has(addr)) next.delete(addr);
    else next.add(addr);
    selectedMembers = next;
  }

  function clampWindowHours(h: number): number {
    if (Number.isNaN(h)) return MIN_WINDOW_HOURS;
    return Math.min(MAX_WINDOW_HOURS, Math.max(MIN_WINDOW_HOURS, Math.round(h)));
  }

  let vetoWindowDaysLabel = $derived(
    vetoWindowHours % 24 === 0
      ? `${vetoWindowHours / 24}d`
      : `${(vetoWindowHours / 24).toFixed(1)}d`,
  );

  // CodeAnt majors 1+2 (review round 1): the backend requires at least
  // two successor members and a threshold of at least two — a 1-member
  // or threshold-1 submission always fails server-side. Match the
  // bounds here so the form never advertises a config it will reject.
  let canSubmitPropose = $derived(
    !proposing &&
      targetVkHex.trim().length > 0 &&
      targetEpoch.trim().length > 0 &&
      selectedMembers.size >= 2 &&
      newThreshold >= 2 &&
      newThreshold <= selectedMembers.size,
  );

  // ZEB-1042: with an active committee loaded, the target fields are
  // authoritative — render them read-only so the admin can't submit a
  // mistyped vk/epoch against a known committee.
  let prefillLocked = $derived(
    committee !== null && committee.active && committee.jointVk !== null,
  );

  function toggleProposeForm() {
    showProposeForm = !showProposeForm;
    // Re-fetch on open so a committee refresh (epoch bump) that landed
    // since the last poll can't leave a stale epoch in the form. The
    // backend's RS-P mirror still validates as the backstop.
    if (showProposeForm) void refreshCommittee();
  }

  function openProposeConfirm() {
    if (!canSubmitPropose) return;
    proposeError = null;
    confirmingPropose = true;
  }

  async function submitPropose() {
    if (!canSubmitPropose) return;
    proposing = true;
    proposeError = null;
    try {
      const result = await invoke<ProposeDfrostResetResult>('propose_dfrost_reset', {
        communityId,
        targetVkHex: targetVkHex.trim(),
        targetEpoch: Number(targetEpoch),
        newMembers: Array.from(selectedMembers),
        newThreshold,
        vetoWindowMs: clampWindowHours(vetoWindowHours) * HOUR_MS,
      });
      void result;
      // Reset the form on success.
      targetVkHex = '';
      targetEpoch = '';
      selectedMembers = new Set();
      newThreshold = 2;
      vetoWindowHours = RESET_VETO_WINDOW_DEFAULT_MS / HOUR_MS;
      showProposeForm = false;
      confirmingPropose = false;
      await refresh();
    } catch (e) {
      proposeError = e instanceof Error ? e.message : String(e);
      confirmingPropose = false;
    } finally {
      proposing = false;
    }
  }
</script>

<div class="dfrost-reset-panel" data-testid="dfrost-reset-panel">
  <p class="explainer">
    A committee reset deactivates this community's current D-FROST committee
    (the threshold-signing group backing Tier-3 secret ballots and sortition)
    and authorizes a successor committee. Any current committee member can
    veto a reset by proving the committee is still alive.
  </p>

  {#if committee}
    <p class="committee-summary" data-testid="dfrost-committee-summary">
      {#if committee.active && committee.jointVk !== null}
        Current committee: epoch {committee.currentEpoch},
        {committee.threshold}-of-{committee.maxSigners},
        key <code>{shortHex(committee.jointVk)}</code>
        {#if committee.pendingReset}
          <span class="reset-in-progress">— reset in progress, successor ceremony pending</span>
        {/if}
      {:else}
        No active D-FROST committee yet — there is nothing to reset until the
        first key ceremony completes.
      {/if}
    </p>
  {/if}

  {#if loadError}
    <p class="error" role="alert">{loadError}</p>
  {/if}

  {#if proposals.length === 0 && !loadError}
    <p class="empty">No committee reset proposals in this community.</p>
  {:else}
    <ul class="proposal-list">
      {#each proposals as p (p.proposalEventId)}
        <li class="proposal-row">
          <div class="proposal-header">
            <span class={`phase-chip ${p.phase}`}>{phaseLabel(p.phase)}</span>
            {#if p.phase === 'window' && p.deadlineMs !== null}
              <span class="countdown">{formatCountdown(p.deadlineMs, tick)}</span>
            {/if}
          </div>

          <dl class="proposal-detail">
            <dt>Proposed by</dt>
            <dd>@{nameFor(p.proposerAddr)}</dd>
            <dt>Target epoch</dt>
            <dd>{p.targetEpoch}</dd>
            <dt>Successor committee</dt>
            <dd>
              {p.newThreshold} of {p.newMemberAddrs.length}
              ({p.newMemberAddrs.map((a) => `@${nameFor(a)}`).join(', ')})
            </dd>
            {#if p.phase === 'collecting' || p.phase === 'window'}
              <dt>Admin signatures</dt>
              <dd>
                {p.signerAddrs.length} of {p.effectiveQuorum ?? adminQuorum} required
              </dd>
            {/if}
            {#if p.phase === 'authorized'}
              <dt>Since authorization</dt>
              <dd>
                {p.endorsed
                  ? 'the committee endorsed this reset'
                  : 'the veto window elapsed without an effective veto'}
                — awaiting the automatic deactivation marker and a
                successor D-FROST key ceremony (DKG). DKG initiation is a
                separate manual step.
              </dd>
            {/if}
            {#if p.phase === 'consumed'}
              <dt>New committee key</dt>
              <dd>
                {p.consumedNewVk ? shortHex(p.consumedNewVk) : '—'}
                {#if p.consumptionSuperseded}
                  <span class="superseded-note">
                    — superseded: the old committee later proved it is still
                    alive
                  </span>
                {/if}
              </dd>
            {/if}
          </dl>

          <div class="proposal-actions">
            {#if canAdmin && isActiveResetPhase(p.phase)}
              <button
                type="button"
                class="act"
                disabled={p.selfHasCosigned || cosignBusy === p.proposalEventId}
                title={p.selfHasCosigned ? 'You already co-signed this proposal' : undefined}
                onclick={() => cosign(p)}
              >
                {p.selfHasCosigned
                  ? 'Co-signed'
                  : cosignBusy === p.proposalEventId
                    ? 'Signing…'
                    : 'Co-sign'}
              </button>
            {/if}
            {#if isActiveResetPhase(p.phase)}
              <span class="committee-response-label">Committee response:</span>
              <button
                type="button"
                class="act endorse"
                disabled={respondBusy === `${p.proposalEventId}:endorse`}
                title="Only current committee members can sign this"
                onclick={() => respond(p, 'endorse')}
              >
                {respondBusy === `${p.proposalEventId}:endorse` ? 'Signing…' : 'Endorse'}
              </button>
              <button
                type="button"
                class="act veto"
                disabled={respondBusy === `${p.proposalEventId}:veto`}
                title="Only current committee members can sign this"
                onclick={() => respond(p, 'veto')}
              >
                {respondBusy === `${p.proposalEventId}:veto` ? 'Signing…' : 'Veto'}
              </button>
            {/if}
          </div>
        </li>
      {/each}
    </ul>
  {/if}

  {#if actionError}
    <p class="error" role="alert">{actionError}</p>
  {/if}

  {#if canAdmin}
    <button type="button" class="propose-toggle" onclick={toggleProposeForm}>
      {showProposeForm ? 'Cancel proposal' : 'Propose a committee reset…'}
    </button>

    {#if showProposeForm}
      <form
        class="propose-form"
        onsubmit={(e) => {
          e.preventDefault();
          openProposeConfirm();
        }}
      >
        {#if prefillLocked}
          <p class="help-text">
            Target key and epoch are filled from the active committee.
          </p>
        {:else if committeeError !== null}
          <p class="help-text">
            Couldn't load the active committee ({committeeError}) — enter the
            target key and epoch manually.
          </p>
        {/if}
        <label>
          <span>Target committee verifying key (hex)</span>
          <input
            type="text"
            bind:value={targetVkHex}
            placeholder="64-char hex — the committee being replaced"
            readonly={prefillLocked}
            class:prefilled={prefillLocked}
            required
          />
        </label>
        <label>
          <span>Target epoch</span>
          <input
            type="number"
            min="0"
            value={targetEpoch}
            oninput={(e) => {
              targetEpoch = (e.target as HTMLInputElement).value;
            }}
            readonly={prefillLocked}
            class:prefilled={prefillLocked}
            required
          />
        </label>

        <div class="field-label">Successor committee members</div>
        <ul class="pick-list" role="listbox" aria-label="Successor committee members" aria-multiselectable="true">
          {#each joinedMembers as m (m.address)}
            <li role="presentation">
              <button
                type="button"
                class="pick-row"
                class:selected={selectedMembers.has(m.address)}
                role="option"
                aria-selected={selectedMembers.has(m.address)}
                onclick={() => toggleMember(m.address)}
              >
                <span class="check">{selectedMembers.has(m.address) ? '✓' : ''}</span>
                <span class="name">@{m.displayName ?? m.address.slice(0, 8)}</span>
              </button>
            </li>
          {/each}
          {#if joinedMembers.length === 0}
            <li class="empty" role="presentation">No joined members found.</li>
          {/if}
        </ul>

        <label>
          <span>Successor threshold</span>
          <input
            type="number"
            class="threshold-input"
            min="2"
            max={Math.max(2, selectedMembers.size)}
            bind:value={newThreshold}
          />
        </label>

        <div class="paired-input">
          <label for="veto-window-hours">Veto window (hours)</label>
          <input
            id="veto-window-hours"
            type="range"
            min={MIN_WINDOW_HOURS}
            max={MAX_WINDOW_HOURS}
            step="1"
            value={vetoWindowHours}
            oninput={(e) => {
              vetoWindowHours = clampWindowHours(Number((e.target as HTMLInputElement).value));
            }}
          />
          <input
            type="number"
            min={MIN_WINDOW_HOURS}
            max={MAX_WINDOW_HOURS}
            value={vetoWindowHours}
            oninput={(e) => {
              vetoWindowHours = clampWindowHours(Number((e.target as HTMLInputElement).value));
            }}
          />
        </div>
        <p class="help-text">≈ {vetoWindowDaysLabel} (clamped 24h–30d; default 72h).</p>

        {#if proposeError}
          <p class="error" role="alert">{proposeError}</p>
        {/if}

        <button type="submit" disabled={!canSubmitPropose}>Review proposal…</button>
      </form>
    {/if}
  {/if}
</div>

{#if confirmingPropose}
  <GovConfirmModal
    title="Propose a D-FROST committee reset?"
    confirmLabel={proposing ? 'Proposing…' : 'Propose reset'}
    busy={proposing}
    onConfirm={submitPropose}
    onCancel={() => {
      if (!proposing) confirmingPropose = false;
    }}
  >
    <p>
      This proposes deactivating the current committee at epoch {targetEpoch} and
      authorizing a {newThreshold}-of-{selectedMembers.size} successor committee.
      Once {adminQuorum} admin{adminQuorum === 1 ? '' : 's'} co-sign, a
      {vetoWindowDaysLabel} veto window opens — the current committee can stop
      it at any time by proving it is still alive. If nothing vetoes, the
      committee is deactivated and a successor D-FROST key ceremony becomes
      possible.
    </p>
  </GovConfirmModal>
{/if}

<style>
  .dfrost-reset-panel {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
  }
  .explainer {
    font-size: 0.8rem;
    color: var(--text-secondary);
    margin: 0;
  }
  .committee-summary {
    font-size: 0.8rem;
    color: var(--text-primary);
    margin: 0;
    padding: 0.45rem 0.6rem;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--surface-raised);
  }
  .committee-summary code {
    font-size: 0.75rem;
  }
  .reset-in-progress {
    color: var(--gov-clay-deep, var(--text-secondary));
    font-weight: 600;
  }
  .propose-form input.prefilled {
    color: var(--text-secondary);
    background: var(--surface-raised);
    cursor: default;
  }
  .empty {
    color: var(--text-secondary);
    font-size: 0.8rem;
    margin: 0;
  }
  .error {
    color: var(--danger-deep, var(--danger));
    font-size: 0.75rem;
    margin: 0;
  }
  .proposal-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.6rem;
  }
  .proposal-row {
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 0.6rem 0.75rem;
    background: var(--surface-raised);
  }
  .proposal-header {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin-bottom: 0.4rem;
  }
  .phase-chip {
    display: inline-block;
    font-size: 0.7rem;
    font-weight: 600;
    padding: 0.15rem 0.5rem;
    border-radius: 999px;
    background: var(--chip-bg, var(--surface-raised));
    color: var(--text-secondary);
  }
  .phase-chip.collecting {
    background: var(--sortition-bg, var(--surface-raised));
    color: var(--gov-purple, var(--text-primary));
  }
  .phase-chip.window {
    background: var(--gov-clay-soft);
    color: var(--gov-clay-deep);
  }
  .phase-chip.authorized {
    background: var(--status-drafting-bg);
    color: var(--status-drafting-fg);
  }
  .phase-chip.consumed {
    background: var(--status-passed-bg);
    color: var(--status-passed-fg);
  }
  .phase-chip.vetoed,
  .phase-chip.expired,
  .phase-chip.lapsed {
    background: color-mix(in srgb, var(--vote-against) 15%, var(--surface-raised));
    color: var(--vote-against);
  }
  .countdown {
    font-size: 0.75rem;
    color: var(--text-secondary);
  }
  .proposal-detail {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 0.2rem 0.6rem;
    font-size: 0.78rem;
    margin: 0 0 0.5rem;
  }
  .proposal-detail dt {
    color: var(--text-secondary);
  }
  .proposal-detail dd {
    margin: 0;
    color: var(--text-primary);
  }
  .superseded-note {
    color: var(--text-secondary);
    font-style: italic;
  }
  .proposal-actions {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    flex-wrap: wrap;
  }
  .committee-response-label {
    font-size: 0.7rem;
    color: var(--text-secondary);
    margin-right: 0.1rem;
  }
  .act {
    padding: 3px 10px;
    border: 1px solid var(--border);
    border-radius: 7px;
    background: var(--surface-raised);
    color: var(--text-primary);
    font: inherit;
    font-size: 0.78rem;
    cursor: pointer;
  }
  .act:disabled {
    cursor: not-allowed;
    opacity: 0.6;
  }
  .act.endorse {
    border-color: var(--vote-for);
    color: var(--vote-for);
  }
  .act.veto {
    border-color: var(--vote-against);
    color: var(--vote-against);
  }
  .propose-toggle {
    align-self: flex-start;
    background: var(--surface-raised);
    color: var(--text-secondary);
    border: 1px solid var(--border);
    padding: 4px 10px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 0.75rem;
  }
  .propose-toggle:hover {
    color: var(--text-primary);
    border-color: var(--accent);
  }
  .propose-form {
    display: flex;
    flex-direction: column;
    gap: 0.6rem;
    padding: 0.75rem;
    background: var(--panel-bg, var(--surface-raised));
    border-radius: 8px;
    border: 1px solid var(--border);
  }
  .propose-form label {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    font-size: 0.78rem;
    color: var(--text-secondary);
  }
  .propose-form input[type='text'],
  .propose-form input[type='number'] {
    background: var(--input-bg);
    color: var(--text-primary);
    border: 1px solid var(--border);
    border-radius: 4px;
    padding: 0.35rem 0.5rem;
    font: inherit;
  }
  .field-label {
    font-size: 10.5px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--text-secondary);
    margin-top: 0.3rem;
  }
  .pick-list {
    list-style: none;
    margin: 0;
    padding: 0;
    max-height: 8rem;
    overflow-y: auto;
    border: 1px solid var(--border);
    border-radius: 7px;
  }
  .pick-row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    width: 100%;
    padding: 0.35rem 0.6rem;
    background: transparent;
    border: none;
    color: var(--text-primary);
    font: inherit;
    text-align: left;
    cursor: pointer;
  }
  .pick-row:hover {
    background: var(--surface-hover, var(--surface-raised));
  }
  .pick-row.selected {
    background: var(--primary-soft);
  }
  .check {
    width: 1rem;
    color: var(--vote-for);
  }
  .paired-input {
    display: grid;
    grid-template-columns: 1fr 3fr 80px;
    gap: 0.5rem;
    align-items: center;
    font-size: 0.78rem;
    color: var(--text-secondary);
  }
  .help-text {
    color: var(--text-secondary);
    font-size: 0.72rem;
    margin: -0.2rem 0 0;
  }
  .propose-form button[type='submit'] {
    align-self: flex-start;
    background: var(--accent);
    color: var(--on-accent);
    border: 0;
    padding: 0.4rem 0.9rem;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.8rem;
  }
  .propose-form button[type='submit']:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
</style>
