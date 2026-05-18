<script lang="ts">
  /**
   * ZEB-292 Phase 3: Tier 2 delegation widget.
   *
   * Surfaces the caller's current delegate (or "Voting directly"),
   * offers a member-picker dropdown to delegate/change, and a Revoke
   * button with severity-tiered confirmation per
   * `feedback_severe_action_confirmation` memory:
   *   - No-confirm: initial Delegate / change-delegate (reversible — a
   *     subsequent Delegate / Undelegate overrides)
   *   - Click-confirm at different screen position: revoke when the
   *     delegate has no signaled-on-active-proposal weight
   *   - Typed-confirm "revoke": revoke when the delegate is signaling
   *     on at least one Open/ThresholdReached Tier 2 proposal AND the
   *     accumulated delegated weight crosses SEVERE_REVOKE_WEIGHT_RATIO
   *     (yanking the rug on active votes)
   *
   * Refreshes on voting-delegation-changed events for the active
   * community where the local user is either the delegator or the
   * delegate (other users' changes don't affect this widget's state).
   */

  import type { CommunityMember } from '../types';
  import type { VotingAdapter } from '../voting-adapter';
  import type { Tier2ProposalExport } from '../types/voting';

  // Threshold above which revoking the delegate requires typed-confirm.
  // Calibrated so revoking a delegate carrying ≥25% of the effective
  // supply on at least one live proposal is treated as irreversible-by-
  // consequence.
  const SEVERE_REVOKE_WEIGHT_RATIO = 0.25;

  let {
    communityId,
    adapter,
    myAddr,
    communityMembers,
  }: {
    /** Hex SpaceId of the community. */
    communityId: string;
    /** Voting IPC adapter — must be connected. */
    adapter: VotingAdapter;
    /** Caller's 32-char hex OwnerAddr. */
    myAddr: string;
    /** Full community member roster for the picker + name resolution. */
    communityMembers: CommunityMember[];
  } = $props();

  let currentDelegate = $state<string | null>(null);
  let pendingDelegate = $state<string>('');
  let busy = $state(false);
  let error = $state<string | null>(null);
  /** UI state machine: 'none' = show widget, 'click' = single-click
   *  confirm bar, 'typed' = type-the-word confirm modal. */
  let confirmState = $state<'none' | 'click' | 'typed'>('none');
  let typedInput = $state('');

  let delegateName = $derived(
    currentDelegate
      ? (communityMembers.find((m) => m.address === currentDelegate)?.displayName ??
        `${currentDelegate.slice(0, 8)}…`)
      : null,
  );

  let eligibleMembers = $derived(
    communityMembers
      .filter((m) => m.address !== myAddr && m.power >= 1 && m.status === 'joined')
      .slice()
      .sort((a, b) => (a.displayName ?? a.address).localeCompare(b.displayName ?? b.address)),
  );

  async function loadDelegate() {
    try {
      currentDelegate = await adapter.getMyDelegate(communityId);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  $effect(() => {
    const cid = communityId;
    let cancelled = false;
    void (async () => {
      if (cancelled) return;
      await loadDelegate();
    })();

    // Refresh on delegation-changed events that touch the local user.
    // Other community members' changes don't impact this widget's
    // visible state, so we ignore them to avoid spurious IPC.
    const unsub = adapter.subscribeDelegationChanged((p) => {
      if (cancelled || p.communityId !== cid) return;
      if (p.delegator !== myAddr && p.delegate !== myAddr) return;
      void loadDelegate();
    });

    return () => {
      cancelled = true;
      unsub();
    };
  });

  async function setDelegate(target: string) {
    if (busy || !target) return;
    busy = true;
    error = null;
    try {
      await adapter.delegateTier2(communityId, target);
      pendingDelegate = '';
      // currentDelegate refreshes via voting-delegation-changed subscriber.
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  /**
   * Inspects active Tier 2 proposals to decide whether revocation is
   * "severe" (delegate likely carrying significant weight on a live
   * proposal, so revoking changes the tally) or "click" (no active
   * proposal has meaningful participation, so revoke is low-consequence).
   *
   * Phase 3 heuristic: if any Open or ThresholdReached proposal has
   * `voter_count / total_supply >= SEVERE_REVOKE_WEIGHT_RATIO`, we trigger
   * typed-confirm — the participation ratio is the closest signal the
   * Tier2ProposalExport DTO exposes for "is this tally meaningful right
   * now." Tighter "this specific delegate's weight share" would require
   * a richer per-voter export; deferred to a follow-up when delegation
   * shows operational pain.
   *
   * On error: defaults to 'typed' — better to over-warn than to silently
   * rug-pull a live vote.
   */
  async function decideRevokeSeverity(): Promise<'click' | 'typed'> {
    if (!currentDelegate) return 'click';
    try {
      const proposals = await adapter.listTier2Proposals(communityId);
      for (const p of proposals) {
        if (p.lifecycle !== 'Open' && p.lifecycle !== 'ThresholdReached') continue;
        const totalSupply = p.total_supply ?? 0;
        if (totalSupply <= 0) continue;
        const participationShare = (p.voter_count ?? 0) / totalSupply;
        if (participationShare >= SEVERE_REVOKE_WEIGHT_RATIO) {
          return 'typed';
        }
      }
      return 'click';
    } catch {
      return 'typed';
    }
  }

  async function beginRevoke() {
    if (busy) return;
    const severity = await decideRevokeSeverity();
    confirmState = severity;
  }

  async function confirmRevoke() {
    if (busy) return;
    if (confirmState === 'typed' && typedInput.trim().toLowerCase() !== 'revoke') return;
    busy = true;
    error = null;
    try {
      await adapter.undelegateTier2(communityId);
      confirmState = 'none';
      typedInput = '';
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  function cancelRevoke() {
    confirmState = 'none';
    typedInput = '';
  }
</script>

<section class="delegation-widget" aria-label="Delegation">
  {#if currentDelegate}
    <p class="dw-status">
      Delegated to <strong>{delegateName}</strong>
      <span class="dw-addr-tail" aria-hidden="true">({currentDelegate.slice(0, 8)}…)</span>
    </p>

    {#if confirmState === 'none'}
      <div class="dw-actions">
        <button
          type="button"
          class="dw-revoke"
          onclick={() => void beginRevoke()}
          disabled={busy}
        >
          Revoke delegation
        </button>
        <label class="dw-change-label" for="dw-change">Change delegate</label>
        <select
          id="dw-change"
          class="dw-select"
          bind:value={pendingDelegate}
          disabled={busy || eligibleMembers.length === 0}
        >
          <option value="" disabled>Select a member…</option>
          {#each eligibleMembers as m (m.address)}
            <option value={m.address}>{m.displayName ?? m.address.slice(0, 8)}</option>
          {/each}
        </select>
        <button
          type="button"
          class="dw-apply"
          onclick={() => void setDelegate(pendingDelegate)}
          disabled={busy || !pendingDelegate}
        >
          {busy ? 'Applying…' : 'Apply'}
        </button>
      </div>
    {:else if confirmState === 'click'}
      <div class="dw-confirm-bar" role="alertdialog" aria-label="Confirm revoke">
        <span>Revoke delegation to <strong>{delegateName}</strong>?</span>
        <div class="dw-confirm-actions">
          <button
            type="button"
            class="dw-confirm"
            onclick={() => void confirmRevoke()}
            disabled={busy}
          >
            Confirm revoke
          </button>
          <button type="button" class="dw-cancel" onclick={cancelRevoke}>Cancel</button>
        </div>
      </div>
    {:else if confirmState === 'typed'}
      <div class="dw-confirm-typed" role="alertdialog" aria-label="Type-to-confirm revoke">
        <p>
          Your delegate is carrying significant weight on at least one active proposal.
          Revoking now will change those tallies. Type <strong>revoke</strong> to confirm.
        </p>
        <input
          class="dw-typed-input"
          type="text"
          bind:value={typedInput}
          placeholder="revoke"
          aria-label="Type the word revoke to confirm"
          disabled={busy}
        />
        <div class="dw-confirm-actions">
          <button
            type="button"
            class="dw-confirm"
            onclick={() => void confirmRevoke()}
            disabled={busy || typedInput.trim().toLowerCase() !== 'revoke'}
          >
            Confirm revoke
          </button>
          <button type="button" class="dw-cancel" onclick={cancelRevoke}>Cancel</button>
        </div>
      </div>
    {/if}
  {:else}
    <p class="dw-status">Voting directly (no delegate)</p>
    <div class="dw-actions">
      <label class="dw-change-label" for="dw-set">Delegate to</label>
      <select
        id="dw-set"
        class="dw-select"
        bind:value={pendingDelegate}
        disabled={busy || eligibleMembers.length === 0}
      >
        <option value="" disabled>Select a member…</option>
        {#each eligibleMembers as m (m.address)}
          <option value={m.address}>{m.displayName ?? m.address.slice(0, 8)}</option>
        {/each}
      </select>
      <button
        type="button"
        class="dw-apply"
        onclick={() => void setDelegate(pendingDelegate)}
        disabled={busy || !pendingDelegate}
      >
        {busy ? 'Applying…' : 'Delegate'}
      </button>
    </div>
  {/if}
  {#if error}
    <p class="dw-error" role="alert">{error}</p>
  {/if}
</section>

<style>
  .delegation-widget {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 12px 14px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-secondary);
  }
  .dw-status {
    margin: 0;
    font-size: 0.92rem;
    color: var(--text-primary);
  }
  .dw-addr-tail {
    font-family: var(--font-mono, ui-monospace, SFMono-Regular, monospace);
    font-size: 0.78rem;
    color: var(--text-secondary);
    margin-left: 4px;
  }
  .dw-actions {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .dw-change-label {
    font-size: 0.78rem;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  .dw-select {
    padding: 4px 8px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg-primary);
    color: var(--text-primary);
    font: inherit;
  }
  .dw-revoke {
    padding: 4px 12px;
    border: 1px solid var(--danger, #f87171);
    background: transparent;
    color: var(--danger, #f87171);
    border-radius: 4px;
    cursor: pointer;
    font: inherit;
  }
  .dw-revoke:disabled {
    cursor: not-allowed;
    opacity: 0.5;
  }
  .dw-apply,
  .dw-confirm {
    padding: 4px 12px;
    border: 1px solid var(--accent, #4ade80);
    background: var(--accent, #4ade80);
    color: var(--bg-primary);
    border-radius: 4px;
    cursor: pointer;
    font: inherit;
  }
  .dw-apply:disabled,
  .dw-confirm:disabled {
    cursor: not-allowed;
    opacity: 0.5;
  }
  .dw-cancel {
    padding: 4px 12px;
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
    border-radius: 4px;
    cursor: pointer;
    font: inherit;
  }
  .dw-confirm-bar,
  .dw-confirm-typed {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 10px 12px;
    border: 1px solid var(--danger, #f87171);
    border-radius: 4px;
    background: var(--bg-primary);
  }
  .dw-confirm-actions {
    display: flex;
    gap: 8px;
  }
  .dw-typed-input {
    padding: 4px 8px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg-secondary);
    color: var(--text-primary);
    font: inherit;
    max-width: 160px;
  }
  .dw-typed-input:focus {
    outline: 1px solid var(--accent, #4ade80);
    outline-offset: 1px;
  }
  .dw-error {
    margin: 0;
    color: var(--danger, #f87171);
    font-size: 0.85rem;
  }
</style>
