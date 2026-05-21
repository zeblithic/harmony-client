<script lang="ts">
  // Option B: import the inner writable directly so Svelte's `$store`
  // template auto-subscription applies. The codebase doesn't otherwise
  // use `svelte/store` — this is the minimal idiom that interops with
  // Svelte 5 runes (the `$prefix` on a store identifier in a template
  // or `$derived` expression auto-subscribes and tracks reactivity).
  import { toastsStore } from '../stores/toast';
  import Toast from './Toast.svelte';

  let toasts = $derived($toastsStore);
</script>

<div class="toast-host" aria-live="polite" aria-atomic="false">
  {#each toasts as t (t.id)}
    <Toast toast={t} />
  {/each}
</div>

<style>
  .toast-host {
    position: fixed;
    bottom: 1rem;
    right: 1rem;
    display: flex;
    flex-direction: column-reverse;
    gap: 0.5rem;
    z-index: 9999;
    pointer-events: none;
  }
  .toast-host > :global(*) {
    pointer-events: auto;
  }
</style>
