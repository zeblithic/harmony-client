<script lang="ts">
  import { fly } from 'svelte/transition';
  import { toastStore, type Toast } from '../stores/toast';

  let { toast }: { toast: Toast } = $props();
</script>

<!-- ZEB-298 R1: live-region semantics live on the parent ToastHost
     (which sets aria-live="polite" on the container). Adding them
     per-toast would cause assistive tech to announce each toast twice. -->
<div class="toast" transition:fly={{ y: 20, duration: 200 }}>
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
    background: var(--toast-bg);
    color: var(--toast-fg);
    border-radius: 6px;
    box-shadow: 0 4px 12px var(--shadow-soft);
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
    background: var(--bg-hover);
  }
  .dismiss:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
</style>
