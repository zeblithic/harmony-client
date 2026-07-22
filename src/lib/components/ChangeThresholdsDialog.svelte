<script lang="ts">
  import { untrack } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import type { AdminActionResult } from '../types';

  let {
    communityId,
    currentThresholds,
    onClose,
  }: {
    communityId: string;
    currentThresholds: { invite: number; kick: number; setPower: number };
    onClose: () => void;
  } = $props();

  let invite = $state(untrack(() => currentThresholds.invite));
  let kick = $state(untrack(() => currentThresholds.kick));
  let setPower = $state(untrack(() => currentThresholds.setPower));
  let submitting = $state(false);
  let errorMessage: string | null = $state(null);

  // Bidirectional sync: each slider + number pair shares the same $state.

  // ZEB-251: the number inputs let a user type negatives or decimals that
  // satisfy the ordering check but fail at the u8 IPC boundary. Reject them
  // here so submit only enables for whole numbers in [0, 100].
  let allWholeInRange = $derived(
    [invite, kick, setPower].every((v) => Number.isInteger(v) && v >= 0 && v <= 100)
  );
  let orderingOk = $derived(
    allWholeInRange && invite <= kick && kick <= setPower && setPower <= 100
  );

  // ZEB-250 R2 Fix 4: bind to <dialog> element so we can call showModal().
  // showModal() enables the browser's native focus trap and Escape-to-close
  // (via the 'cancel' event), unlike the declarative `open` attribute which
  // leaves focus management to the page.
  let dialogEl: HTMLDialogElement | undefined = $state();

  $effect(() => {
    if (dialogEl && !dialogEl.open) {
      dialogEl.showModal();
    }
  });

  function handleClose() {
    // Internal close — used by propose() after a Completed/Pending IPC result.
    // Always closes regardless of submitting state.
    dialogEl?.close();
    onClose();
  }

  function handleUserCancel(e?: Event) {
    // User-initiated close (Escape via dialog `cancel`, or Cancel button).
    // Don't close mid-submission — the IPC is in flight; closing would
    // leave the optimistic UI in a confusing state. Native `cancel`
    // requires preventDefault to keep the dialog open.
    if (submitting) {
      e?.preventDefault();
      return;
    }
    handleClose();
  }

  async function propose() {
    if (!orderingOk) {
      errorMessage = 'Thresholds must be whole numbers with 0 ≤ invite ≤ kick ≤ set power ≤ 100.';
      return;
    }
    submitting = true;
    errorMessage = null;
    try {
      const result = await invoke<AdminActionResult>('propose_change_thresholds', {
        communityId,
        invite,
        kick,
        setPower,
      });
      if (result.kind === 'Completed') {
        // Quorum=1 self-satisfied; close.
        handleClose();
      } else {
        // Pending; close — pending will appear in PendingAdminProposalsPanel.
        handleClose();
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      errorMessage = msg;
    } finally {
      submitting = false;
    }
  }
</script>

<dialog bind:this={dialogEl} oncancel={handleUserCancel} class="change-quorum-dialog" aria-label="Change power thresholds">
  <h2>Change power thresholds</h2>
  <p>
    Set the minimum power a member needs to invite others, to be kicked, and to have their
    power changed by an admin. Values must satisfy invite ≤ kick ≤ set power ≤ 100.
  </p>

  <div class="control-row">
    <span class="of-label">Invite</span>
    <input
      type="range"
      min={0}
      max={100}
      bind:value={invite}
      aria-label="Invite threshold slider"
    />
    <input
      type="number"
      min={0}
      max={100}
      bind:value={invite}
      aria-label="Invite threshold number"
    />
  </div>

  <div class="control-row">
    <span class="of-label">Kick</span>
    <input
      type="range"
      min={0}
      max={100}
      bind:value={kick}
      aria-label="Kick threshold slider"
    />
    <input
      type="number"
      min={0}
      max={100}
      bind:value={kick}
      aria-label="Kick threshold number"
    />
  </div>

  <div class="control-row">
    <span class="of-label">Set power</span>
    <input
      type="range"
      min={0}
      max={100}
      bind:value={setPower}
      aria-label="Set-power threshold slider"
    />
    <input
      type="number"
      min={0}
      max={100}
      bind:value={setPower}
      aria-label="Set-power threshold number"
    />
  </div>

  <!-- Copy on ONE line: the test matches raw textContent (no whitespace
       normalization), so a line break inside the sentence would break it. -->
  <div class="quorum-warning">
    ⚖ This change is itself an admin action — it needs the current quorum to take effect.
  </div>

  {#if errorMessage}
    <p class="error">{errorMessage}</p>
  {/if}

  <div class="actions">
    <button onclick={handleUserCancel} disabled={submitting}>Cancel</button>
    <button
      onclick={propose}
      disabled={submitting || !orderingOk || (invite === currentThresholds.invite && kick === currentThresholds.kick && setPower === currentThresholds.setPower)}
    >
      Propose change
    </button>
  </div>
</dialog>

<style>
  .change-quorum-dialog {
    padding: 1.5rem;
    min-width: 24rem;
    max-width: 30rem;
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 10px;
    box-shadow: var(--shadow-e2);
    color: var(--text-primary);
  }
  .change-quorum-dialog::backdrop { background: var(--overlay); }
  h2 {
    margin: 0 0 0.5rem;
    font-family: var(--font-display);
    font-weight: 500;
    font-size: 1.2rem;
  }
  .control-row { display: flex; align-items: center; gap: 0.75rem; margin-block: 1rem; }
  .control-row input[type="range"] { flex: 1; }
  .control-row input[type="number"] {
    width: 5rem;
    background: var(--input-bg);
    border: 1px solid var(--border);
    border-radius: 5px;
    padding: 4px 8px;
    color: var(--text-primary);
    font-family: var(--font-mono);
  }
  .of-label { white-space: nowrap; font-size: 0.9rem; color: var(--text-muted); }
  .quorum-warning {
    background: var(--gov-clay-soft);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised));
    color: var(--gov-clay-deep);
    border-radius: 7px;
    padding: 0.6rem 0.8rem;
    font-size: 0.8rem;
    line-height: 1.45;
    margin-block: 1rem;
  }
  .actions { display: flex; gap: 0.5rem; justify-content: flex-end; margin-top: 1rem; }
  .actions button {
    padding: 6px 14px;
    border-radius: 7px;
    font: inherit;
    cursor: pointer;
  }
  .actions button:disabled { cursor: not-allowed; opacity: 0.5; }
  .actions button:first-child {
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
  }
  .actions button:last-child {
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--on-accent);
    font-weight: 600;
  }
  .error { color: var(--danger-deep); }
</style>
