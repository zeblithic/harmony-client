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

  // Enter is scoped to the input only — so pressing Enter while the "Skip for
  // now" button is focused activates Skip (its native click), not Save.
  // (CodeRabbit, PR #180.)
  function handleInputKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter') { e.preventDefault(); handleSave(); }
  }

  // Escape anywhere in the dialog skips (this is not a hard gate).
  function handleDialogKeydown(e: KeyboardEvent) {
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
      onkeydown={handleDialogKeydown}
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
        onkeydown={handleInputKeydown}
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
    background: var(--overlay);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .modal-content {
    background: var(--surface-raised);
    color: var(--text-primary);
    font-family: var(--font-ui);
    padding: 1.5rem;
    border: 1px solid var(--border-default);
    border-radius: 10px;
    box-shadow: var(--shadow-e3);
    max-width: 460px;
    width: 90%;
  }
  .modal-content h2 {
    margin: 0 0 1rem;
    font-family: var(--font-display);
    font-size: 1.35rem;
    font-weight: 600;
    letter-spacing: -0.01em;
  }
  .muted { color: var(--text-secondary); font-size: 0.9rem; margin: 0 0 1rem; line-height: 1.5; }
  label { display: block; margin-bottom: 0.4rem; font-size: 0.9rem; }
  input {
    width: 100%;
    box-sizing: border-box;
    padding: 0.5rem 0.6rem;
    background: var(--bg-primary);
    color: var(--text-primary);
    border: 1px solid var(--border-default);
    border-radius: 6px;
    margin-bottom: 1rem;
    transition: border-color 0.15s ease, box-shadow 0.15s ease;
  }
  input:focus {
    /* Keep the focus ring visible under forced-colors / High Contrast,
       where box-shadow is dropped (Qodo #412; see ForkConfirmDialog #411). */
    outline: 2px solid transparent;
    border-color: var(--accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 12%, transparent);
  }
  .actions { display: flex; gap: 0.5rem; }
  .actions button {
    padding: 0.5rem 1rem;
    border: 1px solid var(--border-default);
    background: var(--bg-tertiary);
    color: var(--text-primary);
    border-radius: 6px;
    font-family: var(--font-ui);
    cursor: pointer;
  }
  .actions button.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--text-bright);
  }
  .actions button:disabled { opacity: 0.5; cursor: default; }
</style>
