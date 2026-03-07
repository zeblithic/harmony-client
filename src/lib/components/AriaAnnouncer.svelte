<script lang="ts">
  let { message }: { message: string } = $props();
</script>

<div role="status" aria-live="polite" aria-atomic="true" class="sr-only">
  {message}
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
