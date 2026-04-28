<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onMount } from 'svelte';

  let fullHash = $state('');
  let displayHash = $derived(fullHash ? `0x${fullHash.slice(0, 8)}…` : '…');
  let loadError = $state<string | null>(null);

  interface RestoreCandidate {
    identity_hash: string;
    minted_at?: number;
    comment?: string;
  }

  type BackupStep =
    | { phase: 'pickType' }                                                                                               // step 1
    | { phase: 'mnemonicReveal'; words: string[]; revealed: boolean; storedSafely: boolean; loadError: string | null }   // step 2a
    | { phase: 'fileEntry'; passphrase: string; passphraseConfirm: string; comment: string; showPass: boolean }          // step 2b
    | { phase: 'fileSaved'; savedPath: string }                                                                           // step 3b success
    | { phase: 'fileSaveError'; error: string };                                                                          // step 3b error

  type RestoreStep =
    | { phase: 'pickSource' }                                                                                             // step 1
    | { phase: 'mnemonicEntry'; input: string; validationError: string | null }                                          // step 2a
    | { phase: 'fileEntry'; pendingFilePath: string; passphrase: string; showPass: boolean; restoreError: string | null }  // step 2b before decrypt
    | { phase: 'fileDecrypted'; pendingFilePath: string; passphrase: string; restoreCandidate: RestoreCandidate }        // step 2b after decrypt
    | { phase: 'confirm'; restoreSource: 'mnemonic' | 'file'; pendingWords: string[]; pendingFilePath?: string; passphrase?: string; restoreCandidate: RestoreCandidate; typedPrefix: string }  // step 3
    | { phase: 'commitError'; error: string }                                                                             // step 3 error
    | { phase: 'done'; postRestoreHash: string };                                                                         // step 4

  type WizardState =
    | { kind: 'idle' }
    | { kind: 'backup'; step: BackupStep }
    | { kind: 'restore'; step: RestoreStep };

  let wizardState = $state<WizardState>({ kind: 'idle' });

  onMount(async () => {
    try {
      fullHash = await invoke<string>('current_identity_hash');
    } catch (e) {
      loadError = `Could not read identity store: ${e}. The wizard cannot continue.`;
    }
  });

  async function copyHash() {
    if (!fullHash || !navigator.clipboard) return;
    try {
      await navigator.clipboard.writeText(fullHash);
    } catch {
      // Some browsers reject when document is unfocused. User can retry.
    }
  }
</script>

{#if loadError}
  <section class="identity-panel" aria-label="Identity">
    <h3 class="section-title">Your Identity</h3>
    <p class="error">{loadError}</p>
  </section>
{:else if wizardState.kind === 'idle'}
  <section class="identity-panel" aria-label="Identity">
    <h3 class="section-title">Your Identity</h3>
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
      <button onclick={() => (wizardState = { kind: 'backup', step: { phase: 'pickType' } })}>Backup…</button>
      <button onclick={() => (wizardState = { kind: 'restore', step: { phase: 'pickSource' } })}>Restore…</button>
    </div>
    <p class="explainer">
      Back up your identity to a 24-word phrase or an encrypted file.
      Restore replaces your current identity — the current one becomes unrecoverable.
    </p>
  </section>
{:else if wizardState.kind === 'backup'}
  <!-- TODO Task 5/6: backup wizard flows -->
  <section class="identity-panel" aria-label="Identity">
    <button onclick={() => (wizardState = { kind: 'idle' })}>← Back</button>
    <p>Backup wizard placeholder.</p>
  </section>
{:else}
  <!-- TODO Task 7/8/9: restore wizard flows -->
  <section class="identity-panel" aria-label="Identity">
    <button onclick={() => (wizardState = { kind: 'idle' })}>← Back</button>
    <p>Restore wizard placeholder.</p>
  </section>
{/if}

<style>
  .identity-panel { padding: 16px; }
  .section-title {
    margin: 0 0 12px;
    font-size: 14px;
    font-weight: 600;
    color: var(--text-primary);
  }
  .hash-row { display: flex; align-items: center; gap: 8px; margin: 8px 0; }
  .hash-display {
    font-family: ui-monospace, monospace;
    background: var(--bg-tertiary);
    padding: 6px 10px;
    border-radius: 4px;
    border: none;
    color: inherit;
    cursor: pointer;
  }
  .hash-display:hover { background: var(--border); }
  .actions { display: flex; gap: 8px; margin: 16px 0; }
  .explainer { color: var(--text-secondary); font-size: 0.85em; margin-top: 14px; }
  /* TODO: add --danger token to app.css for semantic error coloring */
  .error { color: var(--text-secondary); }
</style>
