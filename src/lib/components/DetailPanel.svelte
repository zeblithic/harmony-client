<script lang="ts">
  import type { NetworkNode, NetworkLink } from '../network-types';
  import { nodeHealthColor } from '../graph-utils';
  import NodeDetail from './NodeDetail.svelte';
  import LinkDetail from './LinkDetail.svelte';

  let {
    selectedNode,
    selectedLink,
    nodes,
    links,
    onLinkClick,
  }: {
    selectedNode?: NetworkNode | null;
    selectedLink?: NetworkLink | null;
    nodes: NetworkNode[];
    links: NetworkLink[];
    onLinkClick?: (linkId: string) => void;
  } = $props();

  interface StatusCount {
    status: string;
    count: number;
    color: string;
  }

  let healthBreakdown = $derived.by((): StatusCount[] => {
    const counts: Record<string, number> = {};
    for (const node of nodes) {
      counts[node.status] = (counts[node.status] || 0) + 1;
    }
    const order = ['online', 'degraded', 'offline'] as const;
    const result: StatusCount[] = [];
    for (const status of order) {
      const count = counts[status];
      if (count) {
        result.push({
          status,
          count,
          color: nodeHealthColor(status, false),
        });
      }
    }
    return result;
  });
</script>

<aside class="detail-panel" aria-label="Detail panel">
  {#if selectedNode}
    <NodeDetail node={selectedNode} {links} {onLinkClick} />
  {:else if selectedLink}
    <LinkDetail link={selectedLink} {nodes} />
  {:else}
    <div class="network-summary">
      <h2 class="summary-heading">Network Overview</h2>

      <div class="summary-stats">
        <div class="stat">
          <span class="stat-value">{nodes.length}</span>
          <span class="stat-label">Nodes</span>
        </div>
        <div class="stat">
          <span class="stat-value">{links.length}</span>
          <span class="stat-label">Links</span>
        </div>
      </div>

      {#if healthBreakdown.length > 0}
        <div class="health-breakdown">
          <h3 class="breakdown-heading">Health</h3>
          <ul class="breakdown-list">
            {#each healthBreakdown as entry}
              <li class="breakdown-item">
                <span class="health-dot" style="background: {entry.color}"></span>
                <span class="breakdown-count">{entry.count}</span>
                <span class="breakdown-status">{entry.status}</span>
              </li>
            {/each}
          </ul>
        </div>
      {/if}
    </div>
  {/if}
</aside>

<style>
  .detail-panel {
    width: 320px;
    flex-shrink: 0;
    background: var(--bg-secondary, #2b2d31);
    border-left: 1px solid var(--bg-tertiary, #313338);
    overflow-y: auto;
  }

  .network-summary {
    padding: 16px;
  }

  .summary-heading {
    margin: 0 0 16px;
    font-size: 16px;
    font-weight: 600;
    color: var(--text-primary, #f2f3f5);
  }

  .summary-stats {
    display: flex;
    gap: 16px;
    margin-bottom: 20px;
  }

  .stat {
    display: flex;
    flex-direction: column;
    align-items: center;
    flex: 1;
    padding: 12px;
    background: var(--bg-primary, #1e1f22);
    border-radius: 8px;
  }

  .stat-value {
    font-size: 24px;
    font-weight: 700;
    color: var(--text-primary, #f2f3f5);
  }

  .stat-label {
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-secondary, #b5bac1);
  }

  .health-breakdown {
    border-top: 1px solid var(--bg-tertiary, #313338);
    padding-top: 12px;
  }

  .breakdown-heading {
    margin: 0 0 8px;
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-secondary, #b5bac1);
  }

  .breakdown-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .breakdown-item {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
    color: var(--text-primary, #f2f3f5);
  }

  .health-dot {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
  }

  .breakdown-count {
    font-weight: 600;
  }

  .breakdown-status {
    color: var(--text-secondary, #b5bac1);
    text-transform: capitalize;
  }
</style>
