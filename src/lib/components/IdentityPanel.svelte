<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onMount } from 'svelte';

  let fullHash = $state('');
  let displayHash = $derived(fullHash ? `0x${fullHash.slice(0, 8)}…` : '…');
  let loadError = $state<string | null>(null);

  type WizardMode = 'idle' | 'backup' | 'restore';
  let mode = $state<WizardMode>('idle');

  onMount(async () => {
    try {
      fullHash = await invoke<string>('current_identity_hash');
    } catch (e) {
      loadError = `Could not read identity store: ${e}. The wizard cannot continue.`;
    }
  });

  async function copyHash() {
    if (fullHash) {
      navigator.clipboard?.writeText(fullHash);
    }
  }
</script>

{#if loadError}
  <div class="identity-panel">
    <h2>Identity</h2>
    <p class="error">{loadError}</p>
  </div>
{:else if mode === 'idle'}
  <div class="identity-panel">
    <h2>Identity</h2>
    <div class="hash-row">
      <span class="label">Identity hash</span>
      <button
        class="hash-display"
        title="Click to copy full identity hash"
        onclick={copyHash}
      >
        {displayHash}
      </button>
    </div>
    <div class="actions">
      <button onclick={() => (mode = 'backup')}>Backup…</button>
      <button onclick={() => (mode = 'restore')}>Restore…</button>
    </div>
    <p class="explainer">
      Back up your identity to a 24-word phrase or an encrypted file.
      Restore replaces your current identity — the current one becomes unrecoverable.
    </p>
  </div>
{:else if mode === 'backup'}
  <!-- TODO Task 5/6: backup wizard flows -->
  <div class="identity-panel">
    <button onclick={() => (mode = 'idle')}>← Back</button>
    <p>Backup wizard placeholder.</p>
  </div>
{:else}
  <!-- TODO Task 7/8/9: restore wizard flows -->
  <div class="identity-panel">
    <button onclick={() => (mode = 'idle')}>← Back</button>
    <p>Restore wizard placeholder.</p>
  </div>
{/if}

<style>
  .identity-panel { padding: 16px; }
  .hash-row { display: flex; align-items: center; gap: 8px; margin: 8px 0; }
  .hash-display {
    font-family: ui-monospace, monospace;
    background: #2a2c33;
    padding: 6px 10px;
    border-radius: 4px;
    border: none;
    color: inherit;
    cursor: pointer;
  }
  .hash-display:hover { background: #36393f; }
  .actions { display: flex; gap: 8px; margin: 16px 0; }
  .explainer { color: #b9bbbe; font-size: 0.85em; margin-top: 14px; }
  .error { color: #ed4245; }
</style>
