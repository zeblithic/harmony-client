<script lang="ts">
  /**
   * ZEB-454 — owner-mnemonic restore wizard.
   *
   * Re-adopts the owner identity from its 24-word master-seed mnemonic. The
   * GUI analog of `harmony-app restore owner-mnemonic`. Self-contained modal,
   * reused by the fresh-install onboarding (WelcomeModal, `currentOwnerId=null`)
   * and the identity/devices settings flow (DevicesPanel, an existing owner).
   *
   * Tiers (decided in `owner-restore-logic`, enforced authoritatively by the
   * backend): fresh install → non-destructive restore; same-owner re-adoption →
   * typed-prefix confirm + force; different owner → refused before any write.
   * The recovery words are never logged; the host reloads after success so a
   * fresh `start_node` picks up the re-minted owner_state.
   */
  import { invoke } from '@tauri-apps/api/core';
  import { extractError } from '../owner-service';
  import Modal from './Modal.svelte';
  import {
    classifyRestore,
    restoreForceFlag,
    ownerIdPrefix,
    parseMnemonicWords,
    countMnemonicWords,
    friendlyPreviewError,
    type RestoreTier,
  } from '../owner-restore-logic';

  let {
    currentOwnerId,
    onRestored,
    onCancel,
  }: {
    /** The device's current owner-id (hex), or null on a fresh install. */
    currentOwnerId: string | null;
    /** Called with the restored owner-id hex after a successful re-mint. */
    onRestored: (ownerId: string) => void;
    onCancel: () => void;
  } = $props();

  type Phase =
    | { name: 'entry' }
    | {
        name: 'confirm';
        words: string[];
        restoredOwnerId: string;
        tier: RestoreTier;
        typedPrefix: string;
      }
    | { name: 'committing' }
    | { name: 'error'; message: string };

  let phase = $state<Phase>({ name: 'entry' });
  let input = $state('');
  let validationError = $state<string | null>(null);
  let previewing = $state(false);

  let wordCount = $derived(countMnemonicWords(input));
  const titleId = `owner-restore-title-${Math.random().toString(36).slice(2)}`;

  async function preview() {
    if (wordCount !== 24 || previewing) return;
    validationError = null;
    previewing = true;
    const words = parseMnemonicWords(input);
    try {
      const restoredOwnerId = await invoke<string>('preview_owner_mnemonic_identity', { words });
      const tier = classifyRestore(currentOwnerId, restoredOwnerId);
      if (tier.kind === 'different-owner') {
        validationError =
          `This recovery phrase belongs to a different identity (0x${restoredOwnerId.slice(0, 8)}…) ` +
          `than the one on this device (0x${(currentOwnerId ?? '').slice(0, 8)}…). Restoring would not ` +
          `re-adopt your current identity, so it is refused.`;
        return;
      }
      phase = { name: 'confirm', words, restoredOwnerId, tier, typedPrefix: '' };
    } catch (e) {
      validationError = friendlyPreviewError(extractError(e));
    } finally {
      previewing = false;
    }
  }

  // Same-owner re-adoption is destructive (replaces the device key / owner_state)
  // so it is gated behind a typed-prefix confirm; a fresh install is not.
  let confirmReady = $derived(
    phase.name === 'confirm' &&
      (phase.tier.kind !== 'readopt-same' ||
        phase.typedPrefix.trimEnd() === ownerIdPrefix(currentOwnerId ?? '')),
  );

  async function commit() {
    if (phase.name !== 'confirm' || !confirmReady) return;
    const { words, tier } = phase;
    phase = { name: 'committing' };
    try {
      const ownerId = await invoke<string>('restore_owner_mnemonic_from_words', {
        words,
        force: restoreForceFlag(tier),
      });
      onRestored(ownerId);
    } catch (e) {
      phase = { name: 'error', message: extractError(e) };
    }
  }

  function backToEntry() {
    phase = { name: 'entry' };
    validationError = null;
  }
</script>

<Modal {onCancel} ariaLabelledby={titleId}>
  <h3 class="title" id={titleId}>Restore from recovery phrase</h3>

  {#if phase.name === 'entry'}
    <p class="desc">
      Enter your 24-word recovery phrase to re-adopt your owner identity on this
      device. Your phrase is checked locally and never sent anywhere.
    </p>
    <label class="field-label" for="owner-restore-words-input">Recovery phrase (24 words)</label>
    <textarea
      id="owner-restore-words-input"
      class="words"
      data-testid="owner-restore-words"
      bind:value={input}
      placeholder="word1 word2 word3 … word24"
      rows="4"
      autocomplete="off"
      autocapitalize="off"
      spellcheck="false"
      aria-label="Recovery phrase (24 words)"
    ></textarea>
    <p class="count" class:ok={wordCount === 24}>{wordCount} / 24 words</p>
    {#if validationError}
      <p class="error" data-testid="owner-restore-error" role="alert">{validationError}</p>
    {/if}
    <div class="actions">
      <button
        class="primary"
        data-testid="owner-restore-continue"
        onclick={preview}
        disabled={wordCount !== 24 || previewing}
      >
        {previewing ? 'Checking…' : 'Continue'}
      </button>
      <button class="secondary" onclick={onCancel}>Cancel</button>
    </div>
  {:else if phase.name === 'confirm'}
    <p class="desc">
      This phrase restores owner identity
      <code class="owner-id">0x{phase.restoredOwnerId.slice(0, 8)}…</code>.
    </p>
    {#if phase.tier.kind === 'readopt-same'}
      <p class="warn" data-testid="owner-restore-overwrite-warn">
        ⚠ An identity already exists on this device. Restoring replaces it with a
        fresh device key for the <strong>same</strong> identity. This is
        irreversible.
      </p>
      <p class="prompt">
        Type <strong class="required">{ownerIdPrefix(currentOwnerId ?? '')}</strong>
        (the first 8 characters of your current owner-id) to confirm:
      </p>
      <input
        class="typed-input"
        data-testid="owner-restore-typed-confirm"
        type="text"
        bind:value={phase.typedPrefix}
        aria-label="Type to confirm restore"
        autocomplete="off"
      />
    {:else}
      <p class="desc">
        No identity exists on this device yet, so this restore is
        non-destructive.
      </p>
    {/if}
    <div class="actions">
      <button
        class="primary danger"
        data-testid="owner-restore-confirm"
        onclick={commit}
        disabled={!confirmReady}
      >
        Restore identity
      </button>
      <button class="secondary" onclick={backToEntry}>Back</button>
    </div>
  {:else if phase.name === 'committing'}
    <p class="desc" data-testid="owner-restore-committing">Restoring your identity…</p>
  {:else if phase.name === 'error'}
    <p class="error" data-testid="owner-restore-commit-error" role="alert">{phase.message}</p>
    <div class="actions">
      <button class="secondary" onclick={backToEntry}>Try again</button>
      <button class="secondary" onclick={onCancel}>Cancel</button>
    </div>
  {/if}
</Modal>

<style>
  .title {
    color: var(--text-primary);
    font-size: 1rem;
    margin: 0 0 12px;
  }
  .desc {
    color: var(--text-secondary);
    font-size: 0.875rem;
    line-height: 1.5;
    margin: 0 0 12px;
  }
  .words {
    width: 100%;
    box-sizing: border-box;
    padding: 8px 12px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    font-family: var(--font-mono);
    font-size: 0.875rem;
    resize: vertical;
  }
  .field-label {
    display: block;
    color: var(--text-secondary);
    font-size: 0.8rem;
    margin: 0 0 4px;
  }
  .count {
    color: var(--text-muted);
    font-size: 0.78rem;
    margin: 4px 0 8px;
  }
  .count.ok {
    color: var(--accent);
  }
  .owner-id,
  .required {
    font-family: var(--font-mono);
    background: var(--bg-tertiary);
    padding: 2px 6px;
    border-radius: 3px;
    color: var(--text-primary);
  }
  .warn {
    color: var(--text-primary);
    background: color-mix(in srgb, var(--danger-muted) 12%, transparent);
    border: 1px solid var(--danger-muted);
    border-radius: 4px;
    padding: 8px 12px;
    font-size: 0.85rem;
    margin: 0 0 12px;
  }
  .prompt {
    color: var(--text-secondary);
    font-size: 0.875rem;
    margin: 0 0 8px;
  }
  .typed-input {
    width: 100%;
    box-sizing: border-box;
    padding: 8px 12px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    font-family: var(--font-mono);
    font-size: 0.875rem;
    margin-bottom: 12px;
  }
  .error {
    color: var(--fg-error);
    font-size: 0.85rem;
    margin: 0 0 12px;
  }
  .actions {
    display: flex;
    gap: 8px;
  }
  .actions button {
    padding: 8px 16px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
    border: 1px solid var(--border);
  }
  .actions .primary {
    background: var(--accent);
    color: var(--text-primary);
    border-color: var(--accent);
  }
  .actions .primary.danger {
    background: var(--danger-muted);
    border-color: var(--danger-muted);
  }
  .actions .primary:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .actions .secondary {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
  }
</style>
