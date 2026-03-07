<script lang="ts">
  let { message }: { message: string } = $props();
  let debouncedMessage = $state('');
  let debounceTimer: ReturnType<typeof setTimeout> | null = null;

  $effect(() => {
    const msg = message;
    if (!msg) return;
    if (debounceTimer) clearTimeout(debounceTimer);
    debounceTimer = setTimeout(() => {
      debouncedMessage = msg;
    }, 3000);
    return () => {
      if (debounceTimer) clearTimeout(debounceTimer);
    };
  });
</script>

<div role="status" aria-live="polite" aria-atomic="true" class="sr-only">
  {debouncedMessage}
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
