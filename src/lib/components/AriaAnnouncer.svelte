<script lang="ts">
  import { onDestroy } from 'svelte';

  let { message }: { message: string } = $props();
  let displayedMessage = $state('');
  let throttleTimer: ReturnType<typeof setTimeout> | null = null;
  let lastDropped: string | null = null;

  function startCooldown() {
    throttleTimer = setTimeout(() => {
      throttleTimer = null;
      if (lastDropped) {
        displayedMessage = lastDropped;
        lastDropped = null;
        startCooldown();
      }
    }, 3000);
  }

  $effect(() => {
    const msg = message;
    if (!msg) return;

    if (!throttleTimer) {
      displayedMessage = msg;
      lastDropped = null;
      startCooldown();
    } else {
      lastDropped = msg;
    }
  });

  onDestroy(() => {
    if (throttleTimer) {
      clearTimeout(throttleTimer);
      throttleTimer = null;
    }
  });
</script>

<div role="status" aria-live="polite" aria-atomic="true" class="sr-only">
  {displayedMessage}
</div>

<style>
  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
    white-space: nowrap;
    border: 0;
  }
</style>
