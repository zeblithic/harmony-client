<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onMount } from 'svelte';

  function assertNever(x: never): never {
    throw new Error(`unhandled wizard variant: ${JSON.stringify(x)}`);
  }

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

  // Compile-time exhaustiveness check: the compiler proves this switch is
  // exhaustive over BackupStep / RestoreStep. If a new variant is added in
  // Tasks 6-9 without a matching case here, tsc will error at the
  // assertNever(step) call — catching forgotten-variant bugs before runtime.
  function checkExhaustive(state: WizardState): void {
    if (state.kind === 'backup') {
      const step = state.step;
      switch (step.phase) {
        case 'pickType':
        case 'mnemonicReveal':
        case 'fileEntry':
        case 'fileSaved':
        case 'fileSaveError':
          break;
        default:
          assertNever(step);
      }
    } else if (state.kind === 'restore') {
      const step = state.step;
      switch (step.phase) {
        case 'pickSource':
        case 'mnemonicEntry':
        case 'fileEntry':
        case 'fileDecrypted':
        case 'confirm':
        case 'commitError':
        case 'done':
          break;
        default:
          assertNever(step);
      }
    }
  }
  // Silence the unused-variable warning — this function is intentionally
  // called only for its compile-time type narrowing.
  void checkExhaustive;

  // Transient UI state for pickType step (not yet committed to wizardState)
  let selectedBackupType = $state<'mnemonic' | 'file' | null>(null);

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

  function resetToIdle() {
    wizardState = { kind: 'idle' };
    selectedBackupType = null;
  }

  async function advanceFromPickType() {
    if (!selectedBackupType) return;
    if (selectedBackupType === 'mnemonic') {
      // Capture the current state so we can detect if the user cancelled
      // (or otherwise transitioned away) while the invoke was pending.
      const epoch = wizardState;
      let words: string[];
      try {
        words = await invoke<string[]>('export_mnemonic_words');
      } catch (e) {
        // Same guard for the error path: don't resurrect a cancelled wizard.
        if (wizardState !== epoch) return;
        wizardState = {
          kind: 'backup',
          step: { phase: 'mnemonicReveal', words: [], revealed: false, storedSafely: false, loadError: `Could not load recovery phrase: ${e}` },
        };
        return;
      }
      if (wizardState !== epoch) return;
      wizardState = {
        kind: 'backup',
        step: { phase: 'mnemonicReveal', words, revealed: false, storedSafely: false, loadError: null },
      };
    } else {
      // No await on this path — direct transition is safe.
      wizardState = {
        kind: 'backup',
        step: { phase: 'fileEntry', passphrase: '', passphraseConfirm: '', comment: '', showPass: false },
      };
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
  {#if wizardState.step.phase === 'pickType'}
    <section class="identity-panel" aria-label="Identity">
      <h3 class="section-title">Choose backup type</h3>
      <fieldset class="backup-type-picker">
        <legend class="visually-hidden">Backup type</legend>
        <label>
          <input type="radio" bind:group={selectedBackupType} value="mnemonic" />
          24-word recovery phrase
        </label>
        <label>
          <input type="radio" bind:group={selectedBackupType} value="file" />
          Encrypted recovery file
        </label>
      </fieldset>
      <div class="actions">
        <button onclick={resetToIdle}>Cancel</button>
        <button disabled={!selectedBackupType} onclick={advanceFromPickType}>Continue</button>
      </div>
    </section>
  {:else if wizardState.step.phase === 'mnemonicReveal'}
    <section class="identity-panel" aria-label="Identity">
      {#if wizardState.step.loadError}
        <h3 class="section-title">Backup identity</h3>
        <p class="error">{wizardState.step.loadError}</p>
        <div class="actions">
          <button onclick={resetToIdle}>Back to settings</button>
        </div>
      {:else}
        <h3 class="section-title">Your recovery phrase</h3>
        <p class="hash-anchor">Backing up identity {displayHash}</p>
        <p class="explainer">
          Write these 24 words down. Anyone with them can recover your identity.
          There is no way to retrieve them later.
        </p>
        <ol
          data-testid="mnemonic-grid"
          class="mnemonic-grid"
          class:blurred={!wizardState.step.revealed}
        >
          {#each wizardState.step.words as w, i (i)}
            <li class="word">{w}</li>
          {/each}
        </ol>
        {#if !wizardState.step.revealed}
          <button onclick={() => {
            if (wizardState.kind === 'backup' && wizardState.step.phase === 'mnemonicReveal') {
              wizardState = { kind: 'backup', step: { ...wizardState.step, revealed: true } };
            }
          }}>Reveal</button>
        {:else}
          <label class="confirm-label">
            <input
              type="checkbox"
              checked={wizardState.step.storedSafely}
              onchange={() => {
                if (wizardState.kind === 'backup' && wizardState.step.phase === 'mnemonicReveal') {
                  wizardState = { kind: 'backup', step: { ...wizardState.step, storedSafely: !wizardState.step.storedSafely } };
                }
              }}
            />
            I've stored this safely
          </label>
        {/if}
        <div class="actions">
          <button onclick={resetToIdle}>Cancel</button>
          <button
            disabled={!wizardState.step.revealed || !wizardState.step.storedSafely}
            onclick={resetToIdle}
          >Done</button>
        </div>
      {/if}
    </section>
  {:else}
    <!-- fileEntry / fileSaved / fileSaveError — Task 6 placeholder -->
    <section class="identity-panel" aria-label="Identity">
      <button onclick={resetToIdle}>← Back</button>
      <p>Backup wizard placeholder.</p>
    </section>
  {/if}
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
  .visually-hidden {
    position: absolute; width: 1px; height: 1px; padding: 0;
    margin: -1px; overflow: hidden; clip: rect(0, 0, 0, 0);
    white-space: nowrap; border: 0;
  }
  .backup-type-picker {
    border: none; padding: 0; margin: 8px 0 16px;
    display: flex; flex-direction: column; gap: 8px;
  }
  .backup-type-picker label {
    display: flex; align-items: center; gap: 8px;
    cursor: pointer; color: var(--text-primary);
  }
  .mnemonic-grid {
    display: grid;
    grid-template-columns: repeat(4, 1fr);
    gap: 6px;
    background: var(--bg-tertiary);
    border-radius: 6px;
    padding: 12px 12px 12px 36px; /* left padding leaves room for the list marker */
    font-family: ui-monospace, monospace;
    font-size: 0.85em;
    margin: 12px 0;
    list-style: decimal;
  }
  .mnemonic-grid.blurred { filter: blur(6px); user-select: none; }
  .word { padding: 2px 0; }
  .confirm-label {
    display: flex; align-items: center; gap: 8px;
    margin: 12px 0; cursor: pointer; color: var(--text-primary);
  }
  .hash-anchor { color: var(--text-secondary); font-size: 0.85em; margin: 4px 0 8px; }
</style>
