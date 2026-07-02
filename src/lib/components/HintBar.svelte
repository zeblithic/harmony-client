<script lang="ts">
  let {
    flatText,
    visible,
  }: {
    flatText: string;
    visible: boolean;
  } = $props();
</script>

{#if visible}
  <div class="hint-bar" aria-label="Phonetic hint">
    {#if flatText}
      <span class="hint-text">{flatText}</span>
    {:else}
      <span class="hint-placeholder">No active row</span>
    {/if}
  </div>
{/if}

<style>
  .hint-bar {
    text-align: center;
    padding: 6px 12px;
    font-family: 'Courier New', Courier, monospace;
    font-size: 1.1rem;
    color: var(--text-secondary);
    border-top: 1px solid var(--border);
    background: var(--bg-secondary);
  }

  .hint-placeholder {
    color: var(--text-muted);
    font-style: italic;
  }
</style>
