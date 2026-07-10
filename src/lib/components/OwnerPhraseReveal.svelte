<script lang="ts">
  /**
   * Owner recovery-phrase reveal (ZEB-650 slice 2, Option A).
   *
   * Invariant (spec §3.3): owner seed material may exist in the webview
   * only as BIP39 words, only inside this component, only after an explicit
   * user reveal action, and never past the component's visible lifetime.
   * The export IPC fires only on the confirm click — never on mount. Word
   * state is dropped on collapse/unmount (best-effort: JS strings cannot be
   * zeroized; the invariant is about DOM exposure and lifetime).
   *
   * dto.ownerId (a 32-hex-char run) exists only for the cross-check below
   * and must NEVER be rendered — the WelcomeModal redaction invariant
   * forbids any /[0-9a-f]{32,}/ run in innerHTML.
   */
  import { invoke } from '@tauri-apps/api/core';
  import { markRecoveryBackedUp } from '../onboarding-backup-flags';

  interface OwnerMnemonicDto {
    words: string[];
    ownerId: string;
  }

  interface Props {
    /** Hex owner id the host surface is displaying (reveal cross-check). */
    ownerId: string;
  }
  let { ownerId }: Props = $props();

  type Phase =
    | { kind: 'collapsed' }
    | { kind: 'confirm'; inFlight: boolean; error: string | null }
    | { kind: 'revealed'; words: string[]; unblurred: boolean; writtenDown: boolean };

  let phase = $state<Phase>({ kind: 'collapsed' });
  let copied = $state(false);

  function collapse() {
    // Drops the words with the state object — they leave the DOM now.
    phase = { kind: 'collapsed' };
    copied = false;
  }

  async function fetchWords() {
    if (phase.kind !== 'confirm' || phase.inFlight) return;
    phase = { kind: 'confirm', inFlight: true, error: null };
    let dto: OwnerMnemonicDto;
    try {
      dto = await invoke<OwnerMnemonicDto>('export_owner_mnemonic_words');
    } catch (e) {
      phase = {
        kind: 'confirm',
        inFlight: false,
        error: e instanceof Error ? e.message : String(e),
      };
      return;
    }
    if (dto.ownerId !== ownerId) {
      // Words belong to a different identity than the one on screen —
      // discard them and render nothing.
      phase = {
        kind: 'confirm',
        inFlight: false,
        error: 'Recovery phrase does not match the identity on screen — not displaying it.',
      };
      return;
    }
    phase = { kind: 'revealed', words: dto.words, unblurred: false, writtenDown: false };
  }

  function toggleWrittenDown() {
    if (phase.kind !== 'revealed') return;
    const next = !phase.writtenDown;
    phase = { ...phase, writtenDown: next };
    // Marking is one-way: unchecking doesn't unmark (there is no honest
    // "un-back-up" — the words were seen and may be on paper).
    if (next) markRecoveryBackedUp(ownerId);
  }

  async function copyWords() {
    if (phase.kind !== 'revealed' || !phase.unblurred) return;
    try {
      await navigator.clipboard.writeText(phase.words.join(' '));
      copied = true;
    } catch {
      copied = false;
    }
  }
</script>

{#if phase.kind === 'collapsed'}
  <button
    type="button"
    class="linklike"
    data-testid="phrase-reveal-open"
    onclick={() => {
      phase = { kind: 'confirm', inFlight: false, error: null };
    }}
  >
    Or write down your 24-word recovery phrase instead
  </button>
{:else if phase.kind === 'confirm'}
  <div class="phrase-warning" role="note" data-testid="phrase-reveal-warning">
    <p class="warning-copy">
      Anyone who sees these 24 words controls your identity. Make sure no one
      is watching your screen.
    </p>
    {#if phase.error}
      <p class="error" role="alert" data-testid="phrase-reveal-error">{phase.error}</p>
    {/if}
    <div class="phrase-actions">
      <button
        type="button"
        class="secondary"
        data-testid="phrase-reveal-cancel"
        onclick={collapse}
        disabled={phase.inFlight}
      >
        Cancel
      </button>
      <button
        type="button"
        class="primary"
        data-testid="phrase-reveal-confirm"
        onclick={fetchWords}
        disabled={phase.inFlight}
      >
        {phase.inFlight ? 'Loading…' : 'Show recovery phrase'}
      </button>
    </div>
  </div>
{:else}
  <div class="phrase-revealed">
    <ol data-testid="phrase-grid" class="mnemonic-grid" class:blurred={!phase.unblurred}>
      {#each phase.words as w, i (i)}
        <li class="word">{w}</li>
      {/each}
    </ol>
    {#if !phase.unblurred}
      <button
        type="button"
        class="secondary"
        data-testid="phrase-reveal-unblur"
        onclick={() => {
          if (phase.kind === 'revealed') phase = { ...phase, unblurred: true };
        }}
      >
        Reveal
      </button>
    {:else}
      <div class="phrase-actions">
        <button type="button" class="secondary" data-testid="phrase-copy" onclick={copyWords}>
          {copied ? 'Copied' : 'Copy'}
        </button>
        <button type="button" class="secondary" data-testid="phrase-reveal-hide" onclick={collapse}>
          Hide
        </button>
      </div>
      <label class="confirm-label">
        <input
          type="checkbox"
          data-testid="phrase-written-down"
          checked={phase.writtenDown}
          onchange={toggleWrittenDown}
        />
        I've written these words down
      </label>
    {/if}
  </div>
{/if}

<style>
  .mnemonic-grid {
    display: grid;
    grid-template-columns: repeat(4, 1fr);
    gap: 6px;
    background: var(--bg-tertiary);
    border-radius: 6px;
    padding: 12px 12px 12px 36px; /* left padding leaves room for the list marker */
    font-family: var(--font-mono);
    font-size: 0.85em;
    margin: 12px 0;
    list-style: decimal;
  }
  .mnemonic-grid.blurred {
    filter: blur(6px);
    user-select: none;
  }
  .word {
    padding: 2px 0;
  }
  .confirm-label {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 12px 0;
    cursor: pointer;
    color: var(--text-primary);
  }
  .phrase-warning {
    margin: 12px 0;
    padding: 12px;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--bg-tertiary);
  }
  .warning-copy {
    margin: 0 0 10px;
    color: var(--text-primary);
  }
  .phrase-actions {
    display: flex;
    gap: 8px;
    margin: 8px 0;
  }
  .error {
    color: var(--danger);
    margin: 8px 0;
  }
  .linklike {
    background: none;
    border: none;
    padding: 0;
    color: var(--accent);
    cursor: pointer;
    text-decoration: underline;
    font-size: 0.9em;
  }
  .primary,
  .secondary {
    padding: 6px 14px;
    border-radius: 6px;
    cursor: pointer;
  }
  .primary {
    background: var(--accent);
    color: var(--on-accent);
    border: 1px solid var(--accent);
  }
  .secondary {
    background: transparent;
    color: var(--text-primary);
    border: 1px solid var(--border);
  }
  button:disabled {
    opacity: 0.6;
    cursor: default;
  }
</style>
