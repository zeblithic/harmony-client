<script lang="ts">
  let {
    nodeCount,
    linkCount,
    healthySummary,
  }: {
    nodeCount: number;
    linkCount: number;
    healthySummary: string;
  } = $props();
</script>

<footer class="network-status-bar" role="status">
  <span class="status-counts">{nodeCount} nodes &middot; {linkCount} links</span>
  {#if healthySummary}
    <span class="health-summary">{healthySummary}</span>
  {/if}
</footer>

<style>
  .network-status-bar {
    display: flex;
    align-items: center;
    padding: 4px 12px;
    background: var(--bg-secondary);
    border-top: 1px solid var(--bg-tertiary);
    font-size: 12px;
    color: var(--text-muted);
  }

  .health-summary {
    margin-left: auto;
  }
</style>
