<script lang="ts">
  /**
   * ZEB-714 — community admin-recovery banner (spec §5.4). Mounted in
   * CommunityView above the columns so EVERY member sees it, regardless
   * of the active view. Deliberately loud: social detection is a
   * first-class defense layer (spec §6 T2), starting from the moment a
   * proposal exists (before threshold).
   *
   * Phases advance with TIME, not events (spec §4.1), so the banner
   * refreshes on the `community-recovery-changed` Tauri event AND on a
   * 60 s poll — an idle community's proposal still flips
   * collecting → time-locked → executed on screen.
   */
  import { onDestroy } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import GovConfirmModal from './governance/GovConfirmModal.svelte';
  import type { RecoveryProposalDto, RecoveryStateDto } from '../recovery-types';
  import { isActiveRecoveryPhase } from '../recovery-types';
  import {
    RECOVERY_FLAGS_CHANGED_EVENT,
    dismissReconfigureNudge,
    dismissResolvedProposal,
    isReconfigureNudgeDismissed,
    isResolvedProposalDismissed,
  } from '../recovery-flags';
  // ZEB-946: recovery deadlines honor the owner's time-format prefs.
  import { formatFullTimestamp, type TimeFormatPrefs } from '../time-format';
  import { timeFormatPrefs } from '../time-format-service';

  const POLL_MS = 60_000;

  let {
    communityId,
    myAddress,
    resolveName,
    onOpenRecoverySettings,
  }: {
    communityId: string;
    myAddress: string;
    /** Roster-backed display-name resolver (address hex → name). */
    resolveName: (addr: string) => string;
    /** Opens Community Settings (the new-admin reconfigure nudge target). */
    onOpenRecoverySettings?: () => void;
  } = $props();

  let recovery: RecoveryStateDto | null = $state(null);
  let nowMs = $state(Date.now());
  let flagsTick = $state(0);
  let latestCallId = 0;
  let latestWatchId = 0;
  let unsubChanged: (() => void) | null = null;
  let pollHandle: ReturnType<typeof setInterval> | null = null;
  let vetoTarget: RecoveryProposalDto | null = $state(null);
  let vetoBusy = $state(false);
  let cosignBusy = $state<string | null>(null);
  let actionError: string | null = $state(null);

  async function refresh() {
    const myCallId = ++latestCallId;
    try {
      const result = await invoke<RecoveryStateDto>('get_recovery_state', { communityId });
      if (myCallId !== latestCallId) return; // stale
      recovery = result;
      nowMs = Date.now();
    } catch {
      if (myCallId !== latestCallId) return;
      // Not joined / engine not up — no banner. Errors are non-actionable here.
      recovery = null;
    }
  }

  $effect(() => {
    const myWatchId = ++latestWatchId;
    void communityId;
    recovery = null;
    actionError = null;
    void refresh();

    let cancelled = false;
    listen<{ communityId: string }>('community-recovery-changed', (event) => {
      if (myWatchId !== latestWatchId) return;
      if (event.payload?.communityId !== communityId) return;
      void refresh();
    })
      .then((unlisten) => {
        if (cancelled || myWatchId !== latestWatchId) {
          unlisten();
          return;
        }
        const prev = unsubChanged;
        unsubChanged = () => unlisten();
        prev?.();
      })
      .catch(() => {
        // Listener registration can fail in test environments — poll covers it.
      });

    pollHandle = setInterval(() => {
      if (myWatchId !== latestWatchId) return;
      void refresh();
    }, POLL_MS);

    const onFlags = () => {
      flagsTick += 1;
    };
    window.addEventListener(RECOVERY_FLAGS_CHANGED_EVENT, onFlags);

    return () => {
      cancelled = true;
      unsubChanged?.();
      unsubChanged = null;
      if (pollHandle) clearInterval(pollHandle);
      pollHandle = null;
      window.removeEventListener(RECOVERY_FLAGS_CHANGED_EVENT, onFlags);
    };
  });

  onDestroy(() => {
    unsubChanged?.();
    unsubChanged = null;
    if (pollHandle) clearInterval(pollHandle);
    pollHandle = null;
  });

  let activeProposals = $derived(
    (recovery?.proposals ?? []).filter((p) => isActiveRecoveryPhase(p.phase)),
  );
  // flagsTick makes the non-reactive localStorage flags re-derive on
  // every dismissal write (the BackupReminderBanner pattern).
  let resolvedProposals = $derived.by(() => {
    void flagsTick;
    return (recovery?.proposals ?? [])
      .filter(
        (p) =>
          (p.phase === 'executed' ||
            p.phase === 'vetoed' ||
            p.phase === 'expired' ||
            p.phase === 'stalled' ||
            p.phase === 'configChanged' ||
            p.phase === 'superseded') &&
          !isResolvedProposalDismissed(myAddress, communityId, p.proposalEventId),
      )
      .map((p) => ({
        ...p,
        showReconfigureNudge:
          p.phase === 'executed' &&
          p.newAdminAddr === myAddress &&
          !isReconfigureNudgeDismissed(myAddress, communityId, p.proposalEventId),
      }));
  });

  let canVeto = $derived((recovery?.selfPower ?? 0) >= 100);

  function name(addr: string): string {
    return resolveName(addr);
  }

  function fmtDate(ms: number, prefs: TimeFormatPrefs): string {
    return formatFullTimestamp(ms, prefs);
  }

  function collectingLine(p: RecoveryProposalDto): string {
    const remaining = Math.max(0, p.threshold - p.signersSoFar);
    return `Recovery of @${name(p.lostAdminAddr)} proposed by @${name(p.proposerAddr)} — ${remaining} more signature${remaining === 1 ? '' : 's'} needed`;
  }

  function timeLockedLine(p: RecoveryProposalDto, prefs: TimeFormatPrefs): string {
    const when = p.deadlineMs !== null ? fmtDate(p.deadlineMs, prefs) : 'the veto window closes';
    return `@${name(p.newAdminAddr)} becomes admin of this community on ${when} unless a current admin vetoes`;
  }

  function resolvedLine(p: RecoveryProposalDto): string {
    switch (p.phase) {
      case 'executed':
        return `Admin recovery completed — @${name(p.newAdminAddr)} replaced @${name(p.lostAdminAddr)} as admin`;
      case 'vetoed':
        return `Recovery of @${name(p.lostAdminAddr)} was vetoed${p.vetoedByAddr ? ` by @${name(p.vetoedByAddr)}` : ''}`;
      case 'stalled':
        return `Recovery of @${name(p.lostAdminAddr)} stalled — the proposed admin left before the veto window closed`;
      case 'configChanged':
        return `Recovery of @${name(p.lostAdminAddr)} was cancelled by a change to the recovery settings`;
      case 'superseded':
        return `Recovery of @${name(p.lostAdminAddr)} was superseded by another recovery proposal`;
      default:
        return `Recovery of @${name(p.lostAdminAddr)} expired without enough signatures`;
    }
  }

  async function cosign(p: RecoveryProposalDto) {
    cosignBusy = p.proposalEventId;
    actionError = null;
    try {
      await invoke('cosign_admin_recovery', {
        communityId,
        proposalEventId: p.proposalEventId,
      });
      await refresh();
    } catch (e) {
      actionError = e instanceof Error ? e.message : String(e);
    } finally {
      cosignBusy = null;
    }
  }

  async function confirmVeto() {
    if (!vetoTarget) return;
    vetoBusy = true;
    actionError = null;
    try {
      await invoke('veto_admin_recovery', {
        communityId,
        proposalEventId: vetoTarget.proposalEventId,
      });
      vetoTarget = null;
      await refresh();
    } catch (e) {
      actionError = e instanceof Error ? e.message : String(e);
      vetoTarget = null;
    } finally {
      vetoBusy = false;
    }
  }
</script>

{#if activeProposals.length > 0 || resolvedProposals.length > 0}
  <div class="recovery-banner" role="status" data-testid="recovery-banner">
    {#each activeProposals as p (p.proposalEventId)}
      <div class="row active-row">
        <span class="icon" aria-hidden="true">🛟</span>
        <span class="text">
          {#if p.phase === 'collecting'}
            {collectingLine(p)}
          {:else}
            {timeLockedLine(p, $timeFormatPrefs)}
          {/if}
        </span>
        <span class="row-actions">
          {#if p.phase === 'collecting' && recovery?.selfIsDesignate && !p.selfHasCosigned}
            <button
              class="act"
              disabled={cosignBusy === p.proposalEventId}
              onclick={() => cosign(p)}
              aria-label={`Co-sign recovery of ${name(p.lostAdminAddr)}`}
            >
              {cosignBusy === p.proposalEventId ? 'Signing…' : 'Co-sign'}
            </button>
          {/if}
          {#if canVeto}
            <button
              class="act veto"
              onclick={() => (vetoTarget = p)}
              aria-label={`Veto recovery of ${name(p.lostAdminAddr)}`}
            >
              Veto
            </button>
          {/if}
        </span>
      </div>
    {/each}

    {#each resolvedProposals as p (p.proposalEventId)}
      <div class="row resolved-row" class:executed={p.phase === 'executed'}>
        <span class="icon" aria-hidden="true">{p.phase === 'executed' ? '✅' : 'ℹ️'}</span>
        <span class="text">
          {resolvedLine(p)}
          {#if p.phase === 'executed' && p.rotationEligibleAtMs !== null && nowMs <= p.rotationEligibleAtMs}
            · membership key rotation pending finality (completes {fmtDate(p.rotationEligibleAtMs, $timeFormatPrefs)})
          {/if}
        </span>
        <span class="row-actions">
          {#if p.showReconfigureNudge}
            <button
              class="act"
              onclick={() => {
                dismissReconfigureNudge(myAddress, communityId, p.proposalEventId);
                onOpenRecoverySettings?.();
              }}
            >
              Review recovery settings
            </button>
          {/if}
          <button
            class="dismiss"
            aria-label="Dismiss resolved recovery notice"
            onclick={() => dismissResolvedProposal(myAddress, communityId, p.proposalEventId)}
          >
            ✕
          </button>
        </span>
      </div>
    {/each}

    {#if actionError}
      <p class="error" role="alert">{actionError}</p>
    {/if}
  </div>
{/if}

{#if vetoTarget}
  <GovConfirmModal
    title="Veto this admin recovery?"
    confirmLabel="Veto recovery"
    busy={vetoBusy}
    onConfirm={confirmVeto}
    onCancel={() => {
      if (!vetoBusy) vetoTarget = null;
    }}
  >
    <p>
      This cancels the proposal to make @{name(vetoTarget.newAdminAddr)} an admin
      in place of @{name(vetoTarget.lostAdminAddr)}. Vetoing restores the status
      quo — the designates can always propose again.
    </p>
  </GovConfirmModal>
{/if}

<style>
  .recovery-banner {
    display: flex;
    flex-direction: column;
    border-bottom: 1px solid color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised));
    background: var(--gov-clay-soft);
    color: var(--gov-clay-deep);
  }
  .row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.5rem 0.75rem;
  }
  .resolved-row {
    opacity: 0.85;
  }
  .text {
    flex: 1;
    font-size: 0.85rem;
  }
  .row-actions {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  .act {
    padding: 4px 12px;
    border: 1px solid var(--gov-clay);
    border-radius: 7px;
    background: var(--gov-clay);
    color: var(--text-bright);
    font: inherit;
    font-size: 0.8rem;
    font-weight: 600;
    cursor: pointer;
  }
  .act.veto {
    background: transparent;
    color: var(--gov-clay-deep);
  }
  .act:disabled {
    cursor: not-allowed;
    opacity: 0.6;
  }
  .dismiss {
    border: none;
    background: transparent;
    color: var(--gov-clay-deep);
    font: inherit;
    cursor: pointer;
    padding: 2px 6px;
  }
  .error {
    margin: 0;
    padding: 0.25rem 0.75rem 0.5rem;
    font-size: 0.75rem;
    color: var(--danger-deep);
  }
</style>
