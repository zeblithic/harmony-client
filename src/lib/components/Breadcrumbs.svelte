<script lang="ts">
  let {
    path,
    onNavigate,
  }: {
    path: Array<{ cid: string | null; name: string }>;
    onNavigate: (cid: string | null) => void;
  } = $props();
</script>

<nav class="breadcrumbs" aria-label="File navigation">
  {#each path as segment, i}
    {#if i > 0}
      <span class="separator" aria-hidden="true">/</span>
    {/if}
    <button
      class="breadcrumb-segment"
      onclick={() => onNavigate(segment.cid)}
    >{segment.name}</button>
  {/each}
</nav>

<style>
  .breadcrumbs {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 4px;
    padding: 8px 12px;
    color: var(--text-secondary, #b5bac1);
  }

  .separator {
    color: var(--text-muted, #949ba4);
    font-size: 0.85rem;
    user-select: none;
  }

  .breadcrumb-segment {
    background: none;
    border: none;
    padding: 2px 4px;
    font: inherit;
    font-size: 0.85rem;
    color: var(--text-secondary, #b5bac1);
    cursor: pointer;
    border-radius: 2px;
  }

  .breadcrumb-segment:hover {
    color: var(--text-primary, #f2f3f5);
    text-decoration: underline;
  }

  .breadcrumb-segment:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }

  .breadcrumb-segment:last-child {
    color: var(--text-primary, #f2f3f5);
    font-weight: 600;
  }
</style>
