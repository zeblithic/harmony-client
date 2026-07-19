<script lang="ts">
  /**
   * ZEB-714 — Governance → Admin recovery configuration dialog (spec §3.1
   * / §5.1). Picks recovery designates from the Joined roster, a
   * threshold R, and a veto window W (slider paired with a typeable
   * number input, 7–365 days). Routed through the ZEB-250 quorum
   * proposal flow via `set_recovery_designates` — self-satisfies when
   * admin quorum is 1, otherwise lands in the pending-proposals panel.
   */
  import { untrack } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import type { AdminActionResult, CommunityMember } from '../types';
  import type { RecoveryConfigDto } from '../recovery-types';

  const DAY_MS = 86_400_000;
  const MIN_WINDOW_DAYS = 7;
  const MAX_WINDOW_DAYS = 365;

  let {
    communityId,
    joinedMembers,
    myAddress,
    existing,
    onClose,
    onSaved,
  }: {
    communityId: string;
    joinedMembers: CommunityMember[];
    myAddress: string;
    existing: RecoveryConfigDto | null;
    onClose: () => void;
    onSaved: (result: AdminActionResult) => void;
  } = $props();

  // ZEB-714 (PR #498 R1, CodeRabbit): seed only from designates still in
  // the Joined roster — a designate who left has no picker row, so a
  // stale address would be invisible, unremovable, and re-submitting it
  // trips the RD2 "not a Joined member" rejection with no way out.
  // Departed designates surface via `departedDesignates` below instead.
  let selected: string[] = $state(
    untrack(() => {
      const joinedAddrs = new Set(joinedMembers.map((m) => m.address));
      return (existing?.designateAddrs ?? []).filter((a) => joinedAddrs.has(a));
    }),
  );
  // Existing designates no longer Joined — shown as an informational row
  // so the admin sees WHY the designate count changed on re-save.
  let departedDesignates = $derived.by(() => {
    const joinedAddrs = new Set(joinedMembers.map((m) => m.address));
    return (existing?.designateAddrs ?? []).filter((a) => !joinedAddrs.has(a));
  });
  let threshold = $state(untrack(() => existing?.threshold ?? 1));
  let windowDays = $state(
    untrack(() => {
      const ms = existing?.vetoWindowMs;
      if (!ms) return 30;
      return Math.min(MAX_WINDOW_DAYS, Math.max(MIN_WINDOW_DAYS, Math.round(ms / DAY_MS)));
    }),
  );
  let submitting = $state(false);
  let errorMessage: string | null = $state(null);

  let dialogEl: HTMLDialogElement | undefined = $state();
  $effect(() => {
    if (dialogEl && !dialogEl.open) dialogEl.showModal();
  });

  // Keep the threshold inside 1..selected.length as the picker changes.
  $effect(() => {
    const cap = Math.max(1, selected.length);
    if (threshold > cap) threshold = cap;
    if (threshold < 1) threshold = 1;
  });

  function toggleSelect(addr: string) {
    selected = selected.includes(addr)
      ? selected.filter((a) => a !== addr)
      : [...selected, addr];
  }

  function memberName(m: CommunityMember): string {
    return m.displayName ?? m.address.slice(0, 8);
  }

  // Integrality guards (Qodo PR #498 R1): the backend takes `u8`/`u64`,
  // so a fractional typed value (e.g. "7.5" days) would serialize as a
  // JSON float and fail serde at the IPC boundary with an opaque error.
  let canSubmit = $derived(
    !submitting &&
      selected.length >= 1 &&
      Number.isInteger(threshold) &&
      threshold >= 1 &&
      threshold <= selected.length &&
      Number.isInteger(windowDays) &&
      windowDays >= MIN_WINDOW_DAYS &&
      windowDays <= MAX_WINDOW_DAYS,
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

  async function save() {
    if (!canSubmit) return;
    submitting = true;
    errorMessage = null;
    try {
      const result = await invoke<AdminActionResult>('set_recovery_designates', {
        communityId,
        designateAddrs: selected,
        threshold,
        vetoWindowMs: windowDays * DAY_MS,
      });
      onSaved(result);
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
  class="recovery-config-dialog"
  aria-label="Configure admin recovery"
>
  <h2>Configure admin recovery</h2>
  <p>
    If this community's admin identity is ever lost, the designates below can
    together propose a replacement admin. The proposal waits out a public veto
    window before taking effect — any current admin can veto with one click.
  </p>

  <div class="field-label" id="designate-list-label">
    Designates ({selected.length} selected)
  </div>
  <ul class="designate-list" role="listbox" aria-labelledby="designate-list-label" aria-multiselectable="true">
    {#each joinedMembers as m (m.address)}
      <li>
        <button
          type="button"
          class="designate-row"
          class:selected={selected.includes(m.address)}
          role="option"
          aria-selected={selected.includes(m.address)}
          onclick={() => toggleSelect(m.address)}
        >
          <span class="check">{selected.includes(m.address) ? '✓' : ''}</span>
          <span class="name">@{memberName(m)}</span>
          {#if m.power === 100}
            <span class="hint">admin — can already act; choose non-admins</span>
          {:else if m.address === myAddress}
            <span class="hint">you</span>
          {/if}
        </button>
      </li>
    {/each}
  </ul>
  {#if departedDesignates.length > 0}
    <p class="departed-note" role="status">
      {departedDesignates.length === 1 ? 'One existing designate has' : `${departedDesignates.length} existing designates have`}
      left the community and will be removed when you save.
    </p>
  {/if}

  <div class="field-label">Signatures required to propose recovery</div>
  <div class="control-row">
    <input
      type="range"
      min={1}
      max={Math.max(1, selected.length)}
      step={1}
      bind:value={threshold}
      aria-label="Recovery threshold slider"
      disabled={selected.length === 0}
    />
    <input
      type="number"
      min={1}
      max={Math.max(1, selected.length)}
      step={1}
      bind:value={threshold}
      aria-label="Recovery threshold"
      disabled={selected.length === 0}
    />
    <span class="of-label">of {selected.length} designates</span>
  </div>

  <div class="field-label">Veto window</div>
  <div class="control-row">
    <input
      type="range"
      min={MIN_WINDOW_DAYS}
      max={MAX_WINDOW_DAYS}
      step={1}
      bind:value={windowDays}
      aria-label="Veto window slider"
    />
    <input
      type="number"
      min={MIN_WINDOW_DAYS}
      max={MAX_WINDOW_DAYS}
      step={1}
      bind:value={windowDays}
      aria-label="Veto window in days"
    />
    <span class="of-label">days</span>
  </div>
  <p class="window-note">
    Every member sees a recovery proposal for the full window before it takes
    effect. Minimum {MIN_WINDOW_DAYS} days, maximum {MAX_WINDOW_DAYS}.
  </p>

  {#if errorMessage}
    <p class="error" role="alert">{errorMessage}</p>
  {/if}

  <div class="actions">
    <button type="button" onclick={() => handleUserCancel()} disabled={submitting}>Cancel</button>
    <button type="button" class="primary" onclick={save} disabled={!canSubmit}>
      {submitting ? 'Saving…' : 'Save recovery settings'}
    </button>
  </div>
</dialog>

<style>
  .recovery-config-dialog {
    max-width: 30rem;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--surface-raised);
    color: var(--text-primary);
    padding: 1.25rem;
  }
  .recovery-config-dialog::backdrop {
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
  .designate-list {
    list-style: none;
    margin: 0;
    padding: 0;
    max-height: 11rem;
    overflow-y: auto;
    border: 1px solid var(--border);
    border-radius: 7px;
  }
  .designate-row {
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
  .designate-row:hover {
    background: var(--surface-hover);
  }
  .designate-row.selected {
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
  .control-row {
    display: flex;
    align-items: center;
    gap: 0.75rem;
  }
  .control-row input[type='range'] {
    flex: 1;
  }
  .control-row input[type='number'] {
    width: 5rem;
    background: var(--input-bg);
    border: 1px solid var(--border);
    border-radius: 5px;
    color: var(--text-primary);
    font-family: var(--font-mono);
    padding: 0.25rem 0.4rem;
  }
  .of-label {
    font-size: 0.8rem;
    color: var(--muted);
    white-space: nowrap;
  }
  .window-note {
    font-size: 0.75rem;
    margin-block: 0.35rem 0;
  }
  .departed-note {
    font-size: 0.75rem;
    color: var(--gov-clay-deep);
    margin-block: 0.35rem 0;
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
