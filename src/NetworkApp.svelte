<script lang="ts">
  import './app.css';
  import { MockNetworkDataService } from './lib/network-data-service';
  import NetworkToolbar from './lib/components/NetworkToolbar.svelte';
  import NetworkGraph from './lib/components/NetworkGraph.svelte';
  import DetailPanel from './lib/components/DetailPanel.svelte';
  import DataTable from './lib/components/DataTable.svelte';
  import AriaAnnouncer from './lib/components/AriaAnnouncer.svelte';
  import NetworkStatusBar from './lib/components/NetworkStatusBar.svelte';

  let service = new MockNetworkDataService();
  let selectedAddress = $state<string | null>(null);
  let selectedLinkId = $state<string | null>(null);
  let showTable = $state(false);
  let announcement = $state('');
  let graphComponent: NetworkGraph;

  // Load table preference from localStorage
  if (typeof localStorage !== 'undefined') {
    showTable = localStorage.getItem('network-viz-show-table') === 'true';
  }

  let selectedNode = $derived(
    selectedAddress ? service.nodes.find((n) => n.address === selectedAddress) ?? null : null,
  );

  let selectedLink = $derived(
    selectedLinkId ? service.links.find((l) => l.id === selectedLinkId) ?? null : null,
  );

  let healthySummary = $derived.by(() => {
    const counts = { online: 0, degraded: 0, offline: 0 };
    for (const n of service.nodes) counts[n.status]++;
    const parts: string[] = [];
    if (counts.online > 0) parts.push(`${counts.online} healthy`);
    if (counts.degraded > 0) parts.push(`${counts.degraded} degraded`);
    if (counts.offline > 0) parts.push(`${counts.offline} offline`);
    return parts.join(', ');
  });

  service.onAlert = (msg) => {
    announcement = msg;
  };

  function handleNodeClick(address: string) {
    selectedAddress = address;
    selectedLinkId = null;
  }

  function handleLinkClick(linkId: string) {
    selectedLinkId = linkId;
    selectedAddress = null;
  }

  function toggleView() {
    showTable = !showTable;
    if (typeof localStorage !== 'undefined') {
      localStorage.setItem('network-viz-show-table', String(showTable));
    }
  }

  $effect(() => {
    service.start();
    return () => service.stop();
  });
</script>

<main class="network-app">
  <NetworkToolbar
    {showTable}
    onToggleView={toggleView}
    onRecenter={() => graphComponent?.recenter()}
    onZoomFit={() => graphComponent?.zoomToFit()}
  />

  <div class="content">
    {#if showTable}
      <div class="table-area">
        <DataTable
          nodes={service.nodes}
          {selectedAddress}
          onNodeSelect={handleNodeClick}
        />
      </div>
    {:else}
      <NetworkGraph
        bind:this={graphComponent}
        nodes={service.nodes}
        links={service.links}
        {selectedAddress}
        onNodeClick={handleNodeClick}
        onLinkClick={handleLinkClick}
      />
    {/if}

    <DetailPanel
      {selectedNode}
      {selectedLink}
      nodes={service.nodes}
      links={service.links}
      onLinkClick={handleLinkClick}
    />
  </div>

  <NetworkStatusBar
    nodeCount={service.nodes.length}
    linkCount={service.links.length}
    {healthySummary}
  />

  <AriaAnnouncer message={announcement} />
</main>

<style>
  .network-app {
    width: 100vw;
    height: 100vh;
    display: flex;
    flex-direction: column;
    background: var(--bg-primary, #1e1f22);
    color: var(--text-primary, #f2f3f5);
    overflow: hidden;
  }

  .content {
    flex: 1;
    display: flex;
    overflow: hidden;
  }

  .table-area {
    flex: 1;
    overflow: auto;
  }
</style>
