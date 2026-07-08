<script lang="ts">
  import type { NetworkNode, NetworkLink } from '../network-types';
  import { heatToColor } from '../graph-utils';
  import { appliedTheme } from '../theme-service';
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
    void $appliedTheme; // re-resolve status colors on a theme flip (ZEB-645)
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
          color: heatToColor(0, status, false),
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
    background: var(--bg-secondary);
    border-left: 1px solid var(--bg-tertiary);
    overflow-y: auto;
  }

  .network-summary {
    padding: 16px;
  }

  .summary-heading {
    margin: 0 0 16px;
    font-size: 16px;
    font-weight: 600;
    color: var(--text-primary);
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
    background: var(--bg-primary);
    border-radius: 8px;
  }

  .stat-value {
    font-size: 24px;
    font-weight: 700;
    color: var(--text-primary);
  }

  .stat-label {
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-secondary);
  }

  .health-breakdown {
    border-top: 1px solid var(--bg-tertiary);
    padding-top: 12px;
  }

  .breakdown-heading {
    margin: 0 0 8px;
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-secondary);
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
    color: var(--text-primary);
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
    color: var(--text-secondary);
    text-transform: capitalize;
  }
</style>
