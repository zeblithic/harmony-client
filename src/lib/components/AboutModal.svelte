<script lang="ts">
  /**
   * ZEB-331 — Simple About modal (spec §4.3 / Task 7).
   *
   * Shows the app version (read from Tauri's app.getVersion), license
   * line, and a link to the GitHub repo. Reached via HelpMenuButton's
   * "About" item.
   */
  import { onMount } from 'svelte';
  import { getVersion } from '@tauri-apps/api/app';

  interface Props {
    open: boolean;
    onDismiss: () => void;
  }
  const { open, onDismiss }: Props = $props();

  let appVersion = $state<string>('unknown');

  onMount(async () => {
    try {
      appVersion = await getVersion();
    } catch {
      // Outside Tauri (dev/browser) — leave as 'unknown'.
    }
  });

  function handleBackdropClick(e: MouseEvent) {
    if (e.target === e.currentTarget) {
      onDismiss();
    }
  }

  $effect(() => {
    if (!open) return;
    function onKey(e: KeyboardEvent) {
      if (e.key === 'Escape') onDismiss();
    }
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });
</script>

{#if open}
  <div
    class="modal-backdrop"
    data-testid="about-modal-backdrop"
    onclick={handleBackdropClick}
    role="presentation"
  >
    <div
      class="modal-content"
      data-testid="about-modal"
      role="dialog"
      aria-labelledby="about-title"
      aria-modal="true"
    >
      <h2 id="about-title">Harmony</h2>
      <p class="version" data-testid="about-version">
        Version <code>{appVersion}</code>
      </p>
      <p>
        A federated chat with self-governing communities.
      </p>
      <p class="license">
        Licensed under MIT.
      </p>
      <p>
        <a
          href="https://github.com/zeblithic/harmony-client"
          target="_blank"
          rel="noopener noreferrer"
        >
          github.com/zeblithic/harmony-client
        </a>
      </p>
      <div class="actions">
        <button type="button" onclick={onDismiss} data-testid="about-close">
          Close
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
    max-width: 420px;
    width: 90%;
  }
  .modal-content h2 {
    margin: 0 0 1rem;
    font-size: 1.25rem;
  }
  .modal-content p {
    margin: 0 0 0.75rem;
    line-height: 1.5;
  }
  .version code {
    background: var(--bg-tertiary, #1f1f1f);
    padding: 0.1rem 0.3rem;
    border-radius: 3px;
  }
  .license {
    font-size: 0.85rem;
    color: var(--text-secondary, #aaa);
  }
  .modal-content a {
    color: var(--accent, #5865f2);
    text-decoration: none;
  }
  .modal-content a:hover {
    text-decoration: underline;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    margin-top: 1rem;
  }
  .actions button {
    padding: 0.5rem 1rem;
    border: 1px solid var(--border, #444);
    background: var(--bg-tertiary, #1f1f1f);
    color: var(--text-primary, #fff);
    border-radius: 4px;
    cursor: pointer;
  }
</style>
