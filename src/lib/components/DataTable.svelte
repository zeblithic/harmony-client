<script lang="ts">
  import type { NetworkNode } from '../network-types';
  import { nodeHealthColor } from '../graph-utils';

  type SortKey = 'displayName' | 'status' | 'hopDistance' | 'cpuPercent' | 'memory' | 'disk';

  let {
    nodes,
    selectedAddress = null,
    onNodeSelect,
  }: {
    nodes: NetworkNode[];
    selectedAddress?: string | null;
    onNodeSelect?: (address: string) => void;
  } = $props();

  let sortKey: SortKey = $state('displayName');
  let sortAsc: boolean = $state(true);

  const statusOrder: Record<string, number> = { online: 0, degraded: 1, offline: 2 };

  let sortedNodes = $derived.by(() => {
    const copy = [...nodes];
    copy.sort((a, b) => {
      let cmp = 0;
      switch (sortKey) {
        case 'displayName':
          cmp = a.displayName.localeCompare(b.displayName);
          break;
        case 'status':
          cmp = (statusOrder[a.status] ?? 3) - (statusOrder[b.status] ?? 3);
          break;
        case 'hopDistance':
          cmp = a.hopDistance - b.hopDistance;
          break;
        case 'cpuPercent':
          cmp = a.metrics.cpuPercent - b.metrics.cpuPercent;
          break;
        case 'memory':
          cmp =
            a.metrics.memoryUsedBytes / a.metrics.memoryTotalBytes -
            b.metrics.memoryUsedBytes / b.metrics.memoryTotalBytes;
          break;
        case 'disk':
          cmp =
            a.metrics.diskUsedBytes / a.metrics.diskTotalBytes -
            b.metrics.diskUsedBytes / b.metrics.diskTotalBytes;
          break;
      }
      return sortAsc ? cmp : -cmp;
    });
    return copy;
  });

  function toggleSort(key: SortKey) {
    if (sortKey === key) {
      sortAsc = !sortAsc;
    } else {
      sortKey = key;
      sortAsc = true;
    }
  }

  function sortIndicator(key: SortKey): string {
    if (sortKey !== key) return '';
    return sortAsc ? ' \u25B2' : ' \u25BC';
  }

  function formatPercent(value: number): string {
    return `${Math.round(value)}%`;
  }

  function formatBytes(used: number, total: number): string {
    const GiB = 1024 * 1024 * 1024;
    const usedGb = used / GiB;
    const totalGb = total / GiB;
    return `${usedGb.toFixed(1)}/${totalGb.toFixed(0)} GB`;
  }

  function handleRowClick(address: string) {
    onNodeSelect?.(address);
  }

  function handleRowKeydown(e: KeyboardEvent, address: string) {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      onNodeSelect?.(address);
    }
  }

  const columns: { key: SortKey; label: string }[] = [
    { key: 'displayName', label: 'Name' },
    { key: 'status', label: 'Status' },
    { key: 'hopDistance', label: 'Hops' },
    { key: 'cpuPercent', label: 'CPU' },
    { key: 'memory', label: 'Memory' },
    { key: 'disk', label: 'Disk' },
  ];
</script>

<table class="data-table">
  <thead>
    <tr>
      {#each columns as col}
        <th scope="col">
          <button
            class="sort-button"
            onclick={() => toggleSort(col.key)}
            aria-label="Sort by {col.label}"
          >
            {col.label}{sortIndicator(col.key)}
          </button>
        </th>
      {/each}
    </tr>
  </thead>
  <tbody>
    {#each sortedNodes as node}
      <tr
        class="data-row"
        class:selected={selectedAddress === node.address}
        aria-selected={selectedAddress === node.address}
        tabindex="0"
        onclick={() => handleRowClick(node.address)}
        onkeydown={(e) => handleRowKeydown(e, node.address)}
      >
        <td class="name-cell">{node.displayName}</td>
        <td>
          <span class="status-dot" style="background: {nodeHealthColor(node.status, node.isLocal)}"></span>
          {node.status}
        </td>
        <td>{node.hopDistance}</td>
        {#if node.status === 'offline'}
          <td>{'\u2014'}</td>
          <td>{'\u2014'}</td>
          <td>{'\u2014'}</td>
        {:else}
          <td>{formatPercent(node.metrics.cpuPercent)}</td>
          <td>{formatBytes(node.metrics.memoryUsedBytes, node.metrics.memoryTotalBytes)}</td>
          <td>{formatBytes(node.metrics.diskUsedBytes, node.metrics.diskTotalBytes)}</td>
        {/if}
      </tr>
    {/each}
  </tbody>
</table>

<style>
  .data-table {
    width: 100%;
    border-collapse: collapse;
    font-size: 13px;
    color: var(--text-primary);
    background: var(--bg-primary);
  }

  thead {
    background: var(--bg-secondary);
    position: sticky;
    top: 0;
    z-index: 1;
  }

  th {
    text-align: left;
    padding: 0;
    border-bottom: 1px solid var(--bg-tertiary);
    font-weight: 600;
    color: var(--text-secondary);
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }

  .sort-button {
    display: flex;
    align-items: center;
    gap: 4px;
    width: 100%;
    padding: 8px 12px;
    border: none;
    background: none;
    color: inherit;
    font: inherit;
    font-weight: 600;
    cursor: pointer;
    text-align: left;
    white-space: nowrap;
  }

  .sort-button:hover {
    color: var(--text-primary);
    background: var(--bg-tertiary);
  }

  .sort-button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
    border-radius: 2px;
  }

  .data-row {
    cursor: pointer;
    border-bottom: 1px solid var(--bg-secondary);
    transition: background 0.1s ease;
  }

  .data-row:hover {
    background: var(--bg-secondary);
  }

  .data-row:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }

  .data-row.selected {
    background: var(--bg-tertiary);
  }

  td {
    padding: 6px 12px;
    white-space: nowrap;
    color: var(--text-primary);
  }

  .name-cell {
    font-weight: 500;
  }

  .status-dot {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    margin-right: 6px;
    vertical-align: middle;
  }
</style>
