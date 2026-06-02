<script lang="ts">
  /**
   * ZEB-336 — first-run "what should we call you?" prompt.
   *
   * Shown by App.svelte after onboarding (post-WelcomeModal) when the profile
   * display name is still the "Anonymous" default. This is NOT a hard gate —
   * it is skippable (Skip or Escape leaves "Anonymous", editable later in
   * Settings → Profile). Save hands the trimmed name to the parent, which owns
   * persistence + card re-seed + network publish.
   */
  import { trapFocus } from '../focus-trap';

  interface Props {
    open: boolean;
    onSave: (name: string) => void | Promise<void>;
    onSkip: () => void;
  }
  const { open, onSave, onSkip }: Props = $props();

  let name = $state('');
  let modalEl = $state<HTMLElement | null>(null);

  // Mirror WelcomeModal's focus trap so keyboard users stay within the dialog.
  $effect(() => {
    if (!open || modalEl === null) return;
    return trapFocus(modalEl);
  });

  function handleSave() {
    const trimmed = name.trim();
    if (!trimmed) return;
    void onSave(trimmed);
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter') { e.preventDefault(); handleSave(); }
    if (e.key === 'Escape') { e.preventDefault(); onSkip(); }
  }
</script>

{#if open}
  <div class="modal-backdrop" data-testid="name-prompt-backdrop" role="presentation">
    <div
      bind:this={modalEl}
      class="modal-content"
      data-testid="name-prompt-modal"
      role="dialog"
      aria-modal="true"
      aria-labelledby="name-prompt-title"
      tabindex="-1"
      onkeydown={handleKeydown}
    >
      <h2 id="name-prompt-title">What should we call you?</h2>
      <p class="muted">
        This is the name people see on your messages. You can change it anytime
        in your profile.
      </p>
      <label for="name-prompt-input">Display name</label>
      <input
        id="name-prompt-input"
        data-testid="name-prompt-input"
        type="text"
        bind:value={name}
        placeholder="Anonymous"
        aria-label="Display name"
      />
      <div class="actions">
        <button
          class="primary"
          data-testid="name-prompt-save"
          onclick={handleSave}
          disabled={name.trim().length === 0}
        >
          Save
        </button>
        <button data-testid="name-prompt-skip" onclick={onSkip}>
          Skip for now
        </button>
      </div>
    </div>
  </div>
{/if}

<style>
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .modal-content {
    background: var(--bg-secondary, #2a2a2a);
    color: var(--text-primary, #fff);
    padding: 1.5rem;
    border-radius: 8px;
    max-width: 460px;
    width: 90%;
  }
  .modal-content h2 { margin: 0 0 1rem; font-size: 1.25rem; }
  .muted { color: var(--text-secondary, #aaa); font-size: 0.9rem; margin: 0 0 1rem; line-height: 1.5; }
  label { display: block; margin-bottom: 0.4rem; font-size: 0.9rem; }
  input {
    width: 100%;
    box-sizing: border-box;
    padding: 0.5rem;
    background: var(--bg-primary, #111);
    color: var(--text-primary, #fff);
    border: 1px solid var(--border, #444);
    border-radius: 4px;
    margin-bottom: 1rem;
  }
  .actions { display: flex; gap: 0.5rem; }
  .actions button {
    padding: 0.5rem 1rem;
    border: 1px solid var(--border, #444);
    background: var(--bg-tertiary, #1f1f1f);
    color: var(--text-primary, #fff);
    border-radius: 4px;
    cursor: pointer;
  }
  .actions button.primary { background: var(--accent, #5865f2); border-color: var(--accent, #5865f2); }
  .actions button:disabled { opacity: 0.5; cursor: default; }
</style>
