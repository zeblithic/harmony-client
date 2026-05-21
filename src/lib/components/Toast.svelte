<script lang="ts">
  import { fly } from 'svelte/transition';
  import { toastStore, type Toast } from '../stores/toast';

  let { toast }: { toast: Toast } = $props();
</script>

<div class="toast" role="status" aria-live="polite" transition:fly={{ y: 20, duration: 200 }}>
  <span class="message">{toast.message}</span>
  <button
    type="button"
    class="dismiss"
    aria-label="Dismiss notification"
    onclick={() => toastStore.dismiss(toast.id)}
  >
    ×
  </button>
</div>

<style>
  .toast {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.75rem 1rem;
    background: var(--toast-bg, rgba(20, 22, 30, 0.95));
    color: var(--toast-fg, #fff);
    border-radius: 6px;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.2);
    max-width: 360px;
    font-size: 0.9rem;
    line-height: 1.4;
  }
  .message {
    flex: 1;
  }
  .dismiss {
    background: transparent;
    border: 0;
    color: inherit;
    font-size: 1.25rem;
    line-height: 1;
    cursor: pointer;
    padding: 0 0.25rem;
    border-radius: 4px;
  }
  .dismiss:hover {
    background: rgba(255, 255, 255, 0.1);
  }
  .dismiss:focus-visible {
    outline: 2px solid var(--accent, #4a9eff);
    outline-offset: 1px;
  }
</style>
