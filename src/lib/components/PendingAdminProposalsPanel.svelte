<script lang="ts">
  import { onDestroy } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';

  type ProposalKindDto =
    | { kind: 'SetPower'; target_addr: string; target_display_name: string | null; level: number }
    | { kind: 'Kick'; target_addr: string; target_display_name: string | null; reason: string | null }
    | { kind: 'ChangeQuorum'; new_quorum: number }
    | {
        kind: 'SetRecoveryDesignates';
        designate_addrs: string[];
        threshold: number;
        veto_window_ms: number;
      };

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

  type CountersignResult = {
    signers_after: number;
    quorum_required: number;
    reached_quorum: boolean;
  };

  let { communityId, canAdmin }: {
    communityId: string;
    canAdmin: boolean;
  } = $props();

  let proposals: PendingAdminProposalDto[] = $state([]);
  let loading = $state(false);
  let errorMessage: string | null = $state(null);
  let latestCallId = 0;
  let latestWatchId = 0;
  let unsubConverged: (() => void) | null = null;

  async function refresh() {
    if (!canAdmin) {
      // Bump latestCallId so any in-flight refresh from before canAdmin
      // flipped to false is discarded.
      latestCallId++;
      proposals = [];
      return;
    }
    const myCallId = ++latestCallId;
    loading = true;
    errorMessage = null;
    try {
      const result = await invoke<PendingAdminProposalDto[]>(
        'list_pending_admin_proposals',
        { communityId }
      );
      if (myCallId !== latestCallId) return; // stale
      proposals = result;
    } catch (e) {
      if (myCallId !== latestCallId) return;
      const msg = e instanceof Error ? e.message : String(e);
      errorMessage = msg;
    } finally {
      if (myCallId === latestCallId) loading = false;
    }
  }

  async function countersign(eventId: string) {
    try {
      const _result = await invoke<CountersignResult>(
        'countersign_admin_proposal',
        { communityId, proposalEventId: eventId }
      );
      // Optimistic refresh.
      await refresh();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      errorMessage = msg;
    }
  }

  $effect(() => {
    const myWatchId = ++latestWatchId;
    // Track reactive dependencies so the effect re-runs when they change.
    void communityId;
    void canAdmin;

    void refresh();

    if (canAdmin) {
      let cancelled = false;
      listen('community-state-sync-converged', () => {
        if (myWatchId !== latestWatchId) return;
        void refresh();
      }).then((unlisten) => {
        if (cancelled || myWatchId !== latestWatchId) {
          unlisten();
          return;
        }
        const prev = unsubConverged;
        unsubConverged = () => {
          unlisten();
        };
        prev?.();
      }).catch(() => {
        // Event listener registration may fail in some test environments — that's OK.
      });
      return () => {
        cancelled = true;
        unsubConverged?.();
        unsubConverged = null;
      };
    } else {
      unsubConverged?.();
      unsubConverged = null;
      return () => {};
    }
  });

  onDestroy(() => {
    unsubConverged?.();
    unsubConverged = null;
  });

  // Bucket sort: pending → effective → expired.
  let pendingProposals = $derived(
    proposals.filter((p) => !p.expired && !p.effective)
  );
  let effectiveProposals = $derived(proposals.filter((p) => p.effective));
  let expiredProposals = $derived(proposals.filter((p) => p.expired));

  function proposalSummary(p: PendingAdminProposalDto): string {
    const kind = p.proposal_kind;
    switch (kind.kind) {
      case 'SetPower': {
        const name = kind.target_display_name ?? kind.target_addr.slice(0, 8);
        if (kind.level === 100) return `Promote @${name} to admin`;
        if (kind.level === 0) return `Demote @${name} from admin`;
        return `Change @${name}'s power to ${kind.level}`;
      }
      case 'Kick': {
        const name = kind.target_display_name ?? kind.target_addr.slice(0, 8);
        return `Kick @${name}`;
      }
      case 'ChangeQuorum':
        return `Change quorum to ${kind.new_quorum}`;
      case 'SetRecoveryDesignates': {
        // Exact when whole days; one decimal with an explicit "~" when
        // fractional — never silently round 7.5 days up to "8".
        const days = kind.veto_window_ms / 86_400_000;
        const label = Number.isInteger(days) ? `${days}` : `~${days.toFixed(1)}`;
        return `Configure admin recovery (${kind.threshold} of ${kind.designate_addrs.length} designates, ${label}-day veto window)`;
      }
    }
  }

  function daysRemaining(wall_ms: number): number {
    const elapsed_ms = Date.now() - wall_ms;
    const remaining_ms = 30 * 24 * 60 * 60 * 1000 - elapsed_ms;
    return Math.max(0, Math.ceil(remaining_ms / (24 * 60 * 60 * 1000)));
  }
</script>

{#if canAdmin}
  <section aria-label="Admin actions" class="admin-proposals-panel">
    <h3>Admin actions</h3>
    {#if loading}
      <p>Loading...</p>
    {/if}
    {#if errorMessage}
      <p class="error">{errorMessage}</p>
    {/if}

    {#if pendingProposals.length > 0}
      <h4>Pending — {pendingProposals.length} awaiting signatures</h4>
      <ul role="list">
        {#each pendingProposals as p (p.event_id)}
          <li aria-label={`Pending admin proposal: ${proposalSummary(p)}`}>
            <div class="proposal-card">
              <div class="summary">{proposalSummary(p)}</div>
              <div class="meta">
                Proposed by @{p.proposer_display_name ?? p.proposer_addr.slice(0, 8)}
                · Signed {p.signers_so_far} of {p.quorum_required}
                · {daysRemaining(p.proposed_at_wall_ms)} days remaining
              </div>
              {#if p.proposal_kind.kind === 'Kick' && p.proposal_kind.reason}
                <div class="reason">Reason: {p.proposal_kind.reason}</div>
              {/if}
              <button
                disabled={p.self_has_signed || p.expired || p.effective}
                aria-label={`Countersign: ${proposalSummary(p)}`}
                onclick={() => countersign(p.event_id)}
              >
                {p.self_has_signed ? 'Already signed ✓' : 'Countersign'}
              </button>
            </div>
          </li>
        {/each}
      </ul>
    {/if}

    {#if effectiveProposals.length > 0}
      <details>
        <summary>Recently approved ({effectiveProposals.length})</summary>
        <ul role="list">
          {#each effectiveProposals as p (p.event_id)}
            <li><div class="proposal-card effective">{proposalSummary(p)}</div></li>
          {/each}
        </ul>
      </details>
    {/if}

    {#if expiredProposals.length > 0}
      <details>
        <summary>Expired without quorum ({expiredProposals.length})</summary>
        <ul role="list">
          {#each expiredProposals as p (p.event_id)}
            <li><div class="proposal-card expired">{proposalSummary(p)}</div></li>
          {/each}
        </ul>
      </details>
    {/if}

    {#if pendingProposals.length === 0 && effectiveProposals.length === 0 && expiredProposals.length === 0 && !loading}
      <p>No admin proposals yet.</p>
    {/if}
  </section>
{/if}

<style>
  .admin-proposals-panel { margin-block: 1rem; }
  .proposal-card {
    border: 1px solid var(--border);
    border-left: 3px solid var(--gov-clay);
    border-radius: 8px;
    background: var(--surface-raised);
    box-shadow: var(--shadow-e1);
    padding: 0.75rem;
    margin-block: 0.5rem;
  }
  .summary { font-weight: 600; }
  .meta { font-family: var(--font-mono); font-size: 0.75rem; color: var(--muted); margin-block: 0.25rem; }
  .reason { font-style: italic; margin-block: 0.25rem; }
  .error { color: var(--danger-deep); }
  .effective { opacity: 0.7; border-left-color: var(--vote-for); }
  .expired { opacity: 0.5; border-left-color: var(--vote-abstain); }
  button {
    padding: 6px 14px;
    border: 1px solid var(--vote-for);
    border-radius: 7px;
    background: var(--vote-for);
    color: var(--status-passed-fg);
    font: inherit;
    font-weight: 600;
    cursor: pointer;
  }
  button:disabled { cursor: not-allowed; opacity: 0.6; background: var(--surface-raised); color: var(--text-muted); border-color: var(--border); }
</style>
