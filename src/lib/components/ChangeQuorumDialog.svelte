<script lang="ts">
  import { untrack } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';

  type AdminActionResult =
    | { kind: 'Completed' }
    | { kind: 'Pending'; proposal_event_id: string; signers_so_far: number; quorum_required: number };

  let {
    communityId,
    currentQuorum,
    currentAdminCount,
    onClose,
  }: {
    communityId: string;
    currentQuorum: number;
    currentAdminCount: number;
    onClose: () => void;
  } = $props();

  let proposedQuorum = $state(untrack(() => currentQuorum));
  let submitting = $state(false);
  let errorMessage: string | null = $state(null);

  // Bidirectional sync: slider + number input share the same $state.
  // Both bind to proposedQuorum — changing either updates the other automatically.

  async function propose() {
    if (proposedQuorum < 1 || proposedQuorum > currentAdminCount) {
      errorMessage = `Quorum must be between 1 and ${currentAdminCount}.`;
      return;
    }
    submitting = true;
    errorMessage = null;
    try {
      const result = await invoke<AdminActionResult>('propose_change_quorum', {
        communityId,
        newQuorum: proposedQuorum,
      });
      if (result.kind === 'Completed') {
        // Quorum=1 self-satisfied; close.
        onClose();
      } else {
        // Pending; close — pending will appear in PendingAdminProposalsPanel.
        onClose();
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      errorMessage = msg;
    } finally {
      submitting = false;
    }
  }
</script>

<dialog open class="change-quorum-dialog" aria-label="Change admin quorum">
  <h2>Change admin quorum</h2>
  <p>
    Currently {currentQuorum} of {currentAdminCount} admin signatures are required to approve
    actions. We recommend at least N+1 admins for survivability so that no single admin
    departure blocks future proposals.
  </p>

  <div class="control-row">
    <input
      type="range"
      min={1}
      max={currentAdminCount}
      bind:value={proposedQuorum}
      aria-label="Quorum slider"
    />
    <input
      type="number"
      min={1}
      max={currentAdminCount}
      bind:value={proposedQuorum}
      aria-label="Quorum number"
    />
    <span class="of-label">of {currentAdminCount} admins</span>
  </div>

  {#if errorMessage}
    <p class="error">{errorMessage}</p>
  {/if}

  <div class="actions">
    <button onclick={onClose} disabled={submitting}>Cancel</button>
    <button
      onclick={propose}
      disabled={submitting || proposedQuorum < 1 || proposedQuorum > currentAdminCount || proposedQuorum === currentQuorum}
    >
      Propose
    </button>
  </div>
</dialog>

<style>
  .change-quorum-dialog { padding: 1.5rem; min-width: 24rem; }
  .control-row { display: flex; align-items: center; gap: 0.75rem; margin-block: 1rem; }
  .control-row input[type="range"] { flex: 1; }
  .control-row input[type="number"] { width: 5rem; }
  .of-label { white-space: nowrap; font-size: 0.9rem; color: var(--muted, #666); }
  .actions { display: flex; gap: 0.5rem; justify-content: flex-end; margin-top: 1rem; }
  .error { color: var(--error, #c00); }
</style>
