<script lang="ts">
  import './app.css';
  import { MockNetworkDataService } from './lib/network-data-service';
  import type { NetworkNode, NetworkLink } from './lib/network-types';
  import { ZenohService, type TauriAdapter, type ConnectionStatus } from './lib/zenoh-service';
  import NetworkToolbar from './lib/components/NetworkToolbar.svelte';
  import NetworkGraph from './lib/components/NetworkGraph.svelte';
  import DetailPanel from './lib/components/DetailPanel.svelte';
  import DataTable from './lib/components/DataTable.svelte';
  import AriaAnnouncer from './lib/components/AriaAnnouncer.svelte';
  import NetworkStatusBar from './lib/components/NetworkStatusBar.svelte';
  import ConnectionBar from './lib/components/ConnectionBar.svelte';
  import { discoveredToNetworkNode, pruneRingBufferCache, filterStaleNodes } from './lib/zenoh-utils';
  import { loadProfile } from './lib/profile-service';

  let service = new MockNetworkDataService();
  let nodes = $state<NetworkNode[]>([...service.nodes]);
  let links = $state<NetworkLink[]>([...service.links]);
  let selectedAddress = $state<string | null>(null);
  let selectedLinkId = $state<string | null>(null);
  let showTable = $state(false);
  let announcement = $state('');
  let graphComponent: NetworkGraph;

  // Zenoh connection state — owned by NetworkApp as $state for reactivity.
  // ZenohService updates its internal fields; we sync them on each tick.
  let zenohStatus = $state<ConnectionStatus>('disconnected');
  let discoveredCount = $state(0);
  let zenohError = $state<string | undefined>(undefined);
  let zenohService: ZenohService | null = null;
  let destroyed = false;

  // Detect Tauri environment and create real ZenohService.
  // Uses a `destroyed` flag to handle the race where the component
  // unmounts before the async init resolves — prevents listener leaks.
  async function initZenohService() {
    try {
      const { invoke } = await import('@tauri-apps/api/core');
      const { listen } = await import('@tauri-apps/api/event');
      if (destroyed) return; // Component unmounted during await
      const adapter: TauriAdapter = {
        invoke: (cmd, args) => invoke(cmd, args),
        listen: (event, handler) => listen(event, (e) => handler({ payload: e.payload })),
      };
      const svc = new ZenohService(adapter);
      svc.ownAddress = loadProfile().address;
      svc.onChange = () => {
        syncZenohState();
        // Re-merge nodes so new discoveries appear immediately
        nodes = mergeNodes();
      };
      await svc.init();
      // Only assign after successful init — if init() throws,
      // zenohService stays null and stubs are used.
      if (destroyed) {
        svc.destroy();
      } else {
        zenohService = svc;
      }
    } catch {
      // Not in Tauri or init failed — zenohService stays null, stubs used
      zenohService = null;
      console.log('Tauri not available — Zenoh connection disabled');
    }
  }

  function syncZenohState() {
    if (zenohService) {
      zenohStatus = zenohService.connectionStatus;
      // Use filtered count so the badge matches the graph (excludes stale nodes)
      discoveredCount = filterStaleNodes(zenohService.discoveredNodes).length;
      zenohError = zenohService.errorMessage;
    }
  }

  /** Merge mock nodes with real discovered nodes for the graph. */
  function mergeNodes(): NetworkNode[] {
    const mockNodes = service.nodes.map((n) => ({ ...n }));
    if (!zenohService || zenohService.connectionStatus !== 'connected') {
      pruneRingBufferCache(new Set()); // Clear all cached buffers
      return mockNodes;
    }
    const freshDiscovered = filterStaleNodes(zenohService.discoveredNodes);
    const realAddresses = new Set<string>();
    const realNodes: NetworkNode[] = [];
    for (const discovered of freshDiscovered) {
      realNodes.push(discoveredToNetworkNode(discovered));
      realAddresses.add(discovered.nodeAddr);
    }
    pruneRingBufferCache(realAddresses); // Remove buffers for departed nodes
    // Mock nodes first (excluding any with same address), real nodes appended
    return [...mockNodes.filter((n) => !realAddresses.has(n.address)), ...realNodes];
  }

  function handleConnect(endpoint: string) {
    if (zenohService) {
      // ZenohService.connect() calls onChange → syncZenohState immediately
      zenohService.connect(endpoint).catch(() => {});
    } else {
      zenohStatus = 'connecting';
      console.log('Zenoh connect requested (no Tauri):', endpoint);
      setTimeout(() => {
        if (zenohStatus === 'connecting') zenohStatus = 'disconnected';
      }, 2000);
    }
  }

  function handleDisconnect() {
    // Update UI immediately — don't wait for async invoke
    zenohStatus = 'disconnected';
    discoveredCount = 0;
    zenohError = undefined;
    if (zenohService) {
      zenohService.disconnect().catch(() => {});
    }
  }

  // Load table preference from localStorage
  if (typeof localStorage !== 'undefined') {
    showTable = localStorage.getItem('network-viz-show-table') === 'true';
  }

  let selectedNode = $derived(
    selectedAddress ? nodes.find((n) => n.address === selectedAddress) ?? null : null,
  );

  let selectedLink = $derived(
    selectedLinkId ? links.find((l) => l.id === selectedLinkId) ?? null : null,
  );

  let healthySummary = $derived.by(() => {
    const counts = { online: 0, degraded: 0, offline: 0 };
    for (const n of nodes) counts[n.status]++;
    const parts: string[] = [];
    if (counts.online > 0) parts.push(`${counts.online} healthy`);
    if (counts.degraded > 0) parts.push(`${counts.degraded} degraded`);
    if (counts.offline > 0) parts.push(`${counts.offline} offline`);
    return parts.join(', ');
  });

  service.onAlert = (msg) => {
    announcement = msg;
  };

  service.onTick = () => {
    nodes = mergeNodes();
    links = service.links.map((l) => ({ ...l }));
    syncZenohState();
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
    initZenohService();
    return () => {
      destroyed = true;
      service.stop();
      // Disconnect the backend session before removing listeners.
      // Without this, the Zenoh session stays open (subscriber task
      // keeps running) for the lifetime of the app process.
      zenohService?.disconnect().catch(() => {});
      zenohService?.destroy();
    };
  });
</script>

<main class="network-app">
  <NetworkToolbar
    {showTable}
    onToggleView={toggleView}
    onRecenter={() => graphComponent?.recenter()}
    onZoomFit={() => graphComponent?.zoomToFit()}
  />

  <ConnectionBar
    connectionStatus={zenohStatus}
    {discoveredCount}
    errorMessage={zenohError}
    onConnect={handleConnect}
    onDisconnect={handleDisconnect}
  />

  <div class="content">
    {#if showTable}
      <div class="table-area">
        <DataTable
          {nodes}
          {selectedAddress}
          onNodeSelect={handleNodeClick}
        />
      </div>
    {:else}
      <NetworkGraph
        bind:this={graphComponent}
        {nodes}
        {links}
        {selectedAddress}
        onNodeClick={handleNodeClick}
        onLinkClick={handleLinkClick}
      />
    {/if}

    <DetailPanel
      {selectedNode}
      {selectedLink}
      {nodes}
      {links}
      onLinkClick={handleLinkClick}
    />
  </div>

  <NetworkStatusBar
    nodeCount={nodes.length}
    linkCount={links.length}
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
