<script lang="ts">
  /**
   * ZEB-714 — designate-side "Initiate admin recovery" dialog (spec
   * §5.3). Select WHICH admin is lost and the proposed replacement from
   * the Joined roster, then sign. Signatures from other designates
   * accumulate asynchronously as CRDT events — designates never need to
   * be online together. Click-confirm tier: initiating is loud, public,
   * vetoable, and expires — reversible in effect.
   */
  import { invoke } from '@tauri-apps/api/core';
  import type { CommunityMember } from '../types';
  import type { InitiateRecoveryResult } from '../recovery-types';

  let {
    communityId,
    members,
    myAddress,
    onClose,
    onInitiated,
  }: {
    communityId: string;
    members: CommunityMember[];
    myAddress: string;
    onClose: () => void;
    onInitiated: (result: InitiateRecoveryResult) => void;
  } = $props();

  let lostAdminAddr: string | null = $state(null);
  let newAdminAddr: string | null = $state(null);
  let submitting = $state(false);
  let errorMessage: string | null = $state(null);

  let dialogEl: HTMLDialogElement | undefined = $state();
  $effect(() => {
    if (dialogEl && !dialogEl.open) dialogEl.showModal();
  });

  // RP4: only current power-100 identities can be declared lost.
  let admins = $derived(members.filter((m) => m.status === 'joined' && m.power === 100));
  // RP3: replacement must be Joined and not already an admin.
  let candidates = $derived(
    members.filter(
      (m) => m.status === 'joined' && m.power !== 100 && m.address !== lostAdminAddr,
    ),
  );

  function memberName(addr: string): string {
    const m = members.find((x) => x.address === addr);
    return m?.displayName ?? addr.slice(0, 8);
  }

  let canSubmit = $derived(
    !submitting && lostAdminAddr !== null && newAdminAddr !== null && lostAdminAddr !== newAdminAddr,
  );

  function handleClose() {
    dialogEl?.close();
    onClose();
  }

  function handleUserCancel(e?: Event) {
    if (submitting) {
      e?.preventDefault();
      return;
    }
    handleClose();
  }

  async function initiate() {
    if (!canSubmit || !lostAdminAddr || !newAdminAddr) return;
    submitting = true;
    errorMessage = null;
    try {
      const result = await invoke<InitiateRecoveryResult>('initiate_admin_recovery', {
        communityId,
        lostAdminAddr,
        newAdminAddr,
      });
      onInitiated(result);
      handleClose();
    } catch (e) {
      errorMessage = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }
</script>

<dialog
  bind:this={dialogEl}
  oncancel={handleUserCancel}
  class="initiate-recovery-dialog"
  aria-label="Initiate admin recovery"
>
  <h2>Initiate admin recovery</h2>
  <p>
    You are a recovery designate for this community. If an admin's identity is
    lost, you and the other designates can propose transferring their admin
    role to a replacement. The proposal is visible to every member and waits
    out a public veto window — a current admin can cancel it with one click.
  </p>

  <div class="field-label">Which admin is lost?</div>
  <ul class="pick-list" role="listbox" aria-label="Lost admin">
    {#each admins as m (m.address)}
      <li>
        <button
          type="button"
          class="pick-row"
          class:selected={lostAdminAddr === m.address}
          role="option"
          aria-selected={lostAdminAddr === m.address}
          onclick={() => (lostAdminAddr = m.address)}
        >
          <span class="check">{lostAdminAddr === m.address ? '✓' : ''}</span>
          <span class="name">@{memberName(m.address)}</span>
        </button>
      </li>
    {/each}
    {#if admins.length === 0}
      <li class="empty">No admins found in the roster.</li>
    {/if}
  </ul>

  <div class="field-label">Proposed replacement admin</div>
  <ul class="pick-list" role="listbox" aria-label="Replacement admin">
    {#each candidates as m (m.address)}
      <li>
        <button
          type="button"
          class="pick-row"
          class:selected={newAdminAddr === m.address}
          role="option"
          aria-selected={newAdminAddr === m.address}
          onclick={() => (newAdminAddr = m.address)}
        >
          <span class="check">{newAdminAddr === m.address ? '✓' : ''}</span>
          <span class="name">@{memberName(m.address)}</span>
          {#if m.address === myAddress}
            <span class="hint">you</span>
          {/if}
        </button>
      </li>
    {/each}
    {#if candidates.length === 0}
      <li class="empty">
        No eligible members — the replacement must already have re-joined the
        community as a regular member.
      </li>
    {/if}
  </ul>

  {#if errorMessage}
    <p class="error" role="alert">{errorMessage}</p>
  {/if}

  <div class="actions">
    <button type="button" onclick={() => handleUserCancel()} disabled={submitting}>Cancel</button>
    <button type="button" class="primary" onclick={initiate} disabled={!canSubmit}>
      {submitting ? 'Signing…' : 'Sign recovery proposal'}
    </button>
  </div>
</dialog>

<style>
  .initiate-recovery-dialog {
    max-width: 30rem;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--surface-raised);
    color: var(--text-primary);
    padding: 1.25rem;
  }
  .initiate-recovery-dialog::backdrop {
    background: var(--overlay);
  }
  h2 {
    margin-block: 0 0.5rem;
    font-size: 1.05rem;
  }
  p {
    font-size: 0.85rem;
    color: var(--text-muted);
  }
  .field-label {
    font-size: 10.5px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--muted);
    margin-block: 0.9rem 0.35rem;
  }
  .pick-list {
    list-style: none;
    margin: 0;
    padding: 0;
    max-height: 9rem;
    overflow-y: auto;
    border: 1px solid var(--border);
    border-radius: 7px;
  }
  .pick-row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    width: 100%;
    padding: 0.4rem 0.6rem;
    background: transparent;
    border: none;
    color: var(--text-primary);
    font: inherit;
    text-align: left;
    cursor: pointer;
  }
  .pick-row:hover {
    background: var(--surface-hover);
  }
  .pick-row.selected {
    background: var(--primary-soft);
  }
  .check {
    width: 1rem;
    color: var(--vote-for);
  }
  .hint {
    margin-left: auto;
    font-size: 0.7rem;
    color: var(--muted);
    font-style: italic;
  }
  .empty {
    padding: 0.5rem 0.6rem;
    font-size: 0.8rem;
    color: var(--muted);
    font-style: italic;
  }
  .error {
    color: var(--danger-deep);
    font-size: 0.8rem;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 0.5rem;
    margin-top: 1rem;
  }
  .actions button {
    padding: 6px 14px;
    border: 1px solid var(--border);
    border-radius: 7px;
    background: var(--surface-raised);
    color: var(--text-primary);
    font: inherit;
    cursor: pointer;
  }
  .actions button.primary {
    background: var(--gov-clay);
    border-color: var(--gov-clay);
    color: var(--text-bright);
    font-weight: 600;
  }
  .actions button:disabled {
    cursor: not-allowed;
    opacity: 0.6;
  }
</style>
