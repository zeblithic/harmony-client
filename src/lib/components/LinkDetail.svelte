<script lang="ts">
  import type { NetworkLink, NetworkNode, LinkSnapshot } from '../network-types';
  import { linkUtilizationColor } from '../graph-utils';
  import Sparkline from './Sparkline.svelte';
  import { RingBuffer } from '../ring-buffer';

  let {
    link,
    nodes,
  }: {
    link: NetworkLink;
    nodes: NetworkNode[];
  } = $props();

  let sourceNode = $derived(nodes.find((n) => n.address === link.source));
  let targetNode = $derived(nodes.find((n) => n.address === link.target));
  let sourceName = $derived(sourceNode?.displayName ?? link.source);
  let targetName = $derived(targetNode?.displayName ?? link.target);

  let utilizationColor = $derived(linkUtilizationColor(link.utilizationPercent));

  // Extract sparkline data from utilization history
  let utilizationData = $derived.by(() => {
    const buf = new RingBuffer<number>(link.utilizationHistory.capacity);
    for (const s of link.utilizationHistory) {
      buf.push(s.utilizationPercent);
    }
    return buf;
  });

  let latencyData = $derived.by(() => {
    const buf = new RingBuffer<number>(link.utilizationHistory.capacity);
    for (const s of link.utilizationHistory) {
      buf.push(s.latencyMs);
    }
    return buf;
  });

  let lastSnapshot = $derived(link.utilizationHistory.peek() as LinkSnapshot | undefined);

  function formatBandwidth(bps: number): string {
    if (bps >= 1e9) return `${(bps / 1e9).toFixed(1)} Gbps`;
    if (bps >= 1e6) return `${(bps / 1e6).toFixed(0)} Mbps`;
    if (bps >= 1e3) return `${(bps / 1e3).toFixed(0)} Kbps`;
    return `${bps} bps`;
  }

  function formatThroughput(bytesPerSec: number): string {
    if (bytesPerSec >= 1e6) return `${(bytesPerSec / 1e6).toFixed(1)} MB/s`;
    if (bytesPerSec >= 1e3) return `${(bytesPerSec / 1e3).toFixed(1)} KB/s`;
    return `${Math.round(bytesPerSec)} B/s`;
  }
</script>

<section class="link-detail" aria-label="Link details between {sourceName} and {targetName}">
  <header class="detail-header">
    <h2 class="link-title">
      <span class="endpoint">{sourceName}</span>
      <span class="separator" aria-hidden="true">&harr;</span>
      <span class="endpoint">{targetName}</span>
    </h2>
  </header>

  <p class="meta-line">
    <span class="interface-type">{link.interfaceType.toUpperCase()}</span>
    · capacity: {formatBandwidth(link.capacityBps)}
  </p>

  <div class="metrics-section">
    <div class="metric">
      <div class="metric-header">
        <span class="metric-label">Utilization</span>
        <span class="metric-value" style="color: {utilizationColor}">{Math.round(link.utilizationPercent)}%</span>
      </div>
      <Sparkline data={utilizationData} label="Link utilization history" color={utilizationColor} />
    </div>

    <div class="metric">
      <div class="metric-header">
        <span class="metric-label">Latency</span>
        <span class="metric-value">{link.latencyMs} ms</span>
      </div>
      <Sparkline data={latencyData} label="Link latency history" color="#5865f2" />
    </div>

    {#if lastSnapshot}
      <div class="metric">
        <div class="metric-header">
          <span class="metric-label">Throughput</span>
        </div>
        <div class="throughput-values">
          <span class="throughput-up" aria-label="Upload throughput">&uarr; {formatThroughput(lastSnapshot.bytesSent)}</span>
          <span class="throughput-down" aria-label="Download throughput">&darr; {formatThroughput(lastSnapshot.bytesReceived)}</span>
        </div>
      </div>
    {/if}
  </div>
</section>

<style>
  .link-detail {
    padding: 16px;
    font-size: 13px;
    color: var(--text-primary, #dcddde);
  }

  .detail-header {
    margin-bottom: 8px;
  }

  .link-title {
    margin: 0;
    font-size: 16px;
    font-weight: 600;
    display: flex;
    align-items: center;
    gap: 8px;
    color: var(--text-primary, #dcddde);
  }

  .endpoint {
    color: var(--text-primary, #dcddde);
  }

  .separator {
    color: var(--text-secondary, #b9bbbe);
    font-size: 14px;
  }

  .meta-line {
    margin: 0 0 16px;
    color: var(--text-secondary, #b9bbbe);
    font-size: 12px;
  }

  .interface-type {
    font-weight: 600;
    letter-spacing: 0.05em;
  }

  .metrics-section {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  .metric {
    background: var(--bg-secondary, #2f3136);
    border-radius: 6px;
    padding: 8px 10px;
  }

  .metric-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 4px;
  }

  .metric-label {
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-secondary, #b9bbbe);
  }

  .metric-value {
    font-size: 13px;
    font-weight: 500;
    color: var(--text-primary, #dcddde);
  }

  .throughput-values {
    display: flex;
    gap: 16px;
    font-size: 13px;
    font-weight: 500;
  }

  .throughput-up {
    color: #43b581;
  }

  .throughput-down {
    color: #5865f2;
  }
</style>
