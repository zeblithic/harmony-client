<script lang="ts">
  import type { NetworkNode, NetworkLink } from '../network-types';
  import { generateIdenticon } from '../identicon';
  import { nodeHealthColor, linkUtilizationColor } from '../graph-utils';
  import Sparkline from './Sparkline.svelte';
  import { RingBuffer } from '../ring-buffer';

  let {
    node,
    links,
    onLinkClick,
  }: {
    node: NetworkNode;
    links: NetworkLink[];
    onLinkClick?: (linkId: string) => void;
  } = $props();

  let truncatedAddress = $derived(
    node.address.length > 8
      ? `${node.address.slice(0, 4)}...${node.address.slice(-4)}`
      : node.address,
  );

  let statusColor = $derived(nodeHealthColor(node.status, node.isLocal));

  let hopLabel = $derived(
    node.hopDistance === 1 ? '1 hop' : `${node.hopDistance} hops`,
  );

  let lastSeenSeconds = $derived(
    Math.round((Date.now() - node.lastSeen) / 1000),
  );

  let identiconSvg = $derived(generateIdenticon(node.address, 36));

  // Extract sparkline data from metrics history
  let cpuData = $derived.by(() => {
    const buf = new RingBuffer<number>(node.metricsHistory.capacity);
    for (const m of node.metricsHistory) {
      buf.push(m.cpuPercent);
    }
    return buf;
  });

  let memData = $derived.by(() => {
    const buf = new RingBuffer<number>(node.metricsHistory.capacity);
    for (const m of node.metricsHistory) {
      buf.push((m.memoryUsedBytes / m.memoryTotalBytes) * 100);
    }
    return buf;
  });

  let diskData = $derived.by(() => {
    const buf = new RingBuffer<number>(node.metricsHistory.capacity);
    for (const m of node.metricsHistory) {
      buf.push((m.diskUsedBytes / m.diskTotalBytes) * 100);
    }
    return buf;
  });

  let cpuColor = $derived(sparklineColor(node.metrics.cpuPercent));
  let memPercent = $derived(
    (node.metrics.memoryUsedBytes / node.metrics.memoryTotalBytes) * 100,
  );
  let memColor = $derived(sparklineColor(memPercent));
  let diskPercent = $derived(
    (node.metrics.diskUsedBytes / node.metrics.diskTotalBytes) * 100,
  );
  let diskColor = $derived(sparklineColor(diskPercent));

  let connectedLinks = $derived(
    links.filter(
      (l) => l.source === node.address || l.target === node.address,
    ),
  );

  function sparklineColor(value: number): string {
    if (value < 60) return '#43b581';
    if (value <= 85) return '#faa61a';
    return '#ed4245';
  }

  function formatPercent(value: number): string {
    return `${Math.round(value)}%`;
  }

  function formatBytes(bytes: number): string {
    const gb = bytes / (1024 * 1024 * 1024);
    if (gb >= 1) return `${gb.toFixed(1)} GB`;
    const mb = bytes / (1024 * 1024);
    return `${mb.toFixed(0)} MB`;
  }

  function handleCopyAddress() {
    navigator.clipboard?.writeText(node.address);
  }

  function handleCopyKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      handleCopyAddress();
    }
  }

  function handleLinkClick(linkId: string) {
    onLinkClick?.(linkId);
  }

  function handleLinkKeydown(e: KeyboardEvent, linkId: string) {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      onLinkClick?.(linkId);
    }
  }
</script>

<section class="node-detail" aria-label="Node details for {node.displayName}">
  <header class="detail-header">
    <div class="identicon" aria-hidden="true">
      {@html identiconSvg}
    </div>
    <div class="header-info">
      <h2 class="node-name">
        <span class="status-dot" style="background: {statusColor}"></span>
        {node.displayName}
      </h2>
      <button
        class="address-chip"
        onclick={handleCopyAddress}
        onkeydown={handleCopyKeydown}
        title="Copy full address"
        aria-label="Copy address {node.address}"
      >
        {truncatedAddress}
      </button>
    </div>
  </header>

  <p class="meta-line">
    {hopLabel} · last seen: {lastSeenSeconds}s
  </p>

  <div class="metrics-section">
    <div class="metric">
      <div class="metric-header">
        <span class="metric-label">CPU</span>
        <span class="metric-value">{formatPercent(node.metrics.cpuPercent)}</span>
      </div>
      <Sparkline data={cpuData} label="CPU usage history" color={cpuColor} />
    </div>

    <div class="metric">
      <div class="metric-header">
        <span class="metric-label">Memory</span>
        <span class="metric-value">{formatBytes(node.metrics.memoryUsedBytes)} / {formatBytes(node.metrics.memoryTotalBytes)}</span>
      </div>
      <Sparkline data={memData} label="Memory usage history" color={memColor} />
    </div>

    <div class="metric">
      <div class="metric-header">
        <span class="metric-label">Disk</span>
        <span class="metric-value">{formatBytes(node.metrics.diskUsedBytes)} / {formatBytes(node.metrics.diskTotalBytes)}</span>
      </div>
      <Sparkline data={diskData} label="Disk usage history" color={diskColor} />
    </div>
  </div>

  <div class="links-section">
    <h3 class="links-heading">Links ({connectedLinks.length})</h3>
    <ul class="links-list">
      {#each connectedLinks as link}
        <li>
          <button
            class="link-item"
            onclick={() => handleLinkClick(link.id)}
            onkeydown={(e) => handleLinkKeydown(e, link.id)}
          >
            <span class="link-type">{link.interfaceType.toUpperCase()}</span>
            <span class="link-stats">
              <span style="color: {linkUtilizationColor(link.utilizationPercent)}">{formatPercent(link.utilizationPercent)}</span>
              · {link.latencyMs}ms
            </span>
          </button>
        </li>
      {/each}
    </ul>
  </div>
</section>

<style>
  .node-detail {
    padding: 16px;
    font-size: 13px;
    color: var(--text-primary, #dcddde);
  }

  .detail-header {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-bottom: 8px;
  }

  .identicon {
    flex-shrink: 0;
    width: 36px;
    height: 36px;
    border-radius: 50%;
    overflow: hidden;
    background: var(--bg-tertiary, #40444b);
  }

  .header-info {
    min-width: 0;
  }

  .node-name {
    margin: 0;
    font-size: 16px;
    font-weight: 600;
    display: flex;
    align-items: center;
    gap: 6px;
    color: var(--text-primary, #dcddde);
  }

  .status-dot {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
  }

  .address-chip {
    display: inline-block;
    margin-top: 2px;
    padding: 2px 8px;
    border: 1px solid var(--bg-tertiary, #40444b);
    border-radius: 10px;
    background: var(--bg-secondary, #2f3136);
    color: var(--text-secondary, #b9bbbe);
    font-family: monospace;
    font-size: 11px;
    cursor: pointer;
  }

  .address-chip:hover {
    background: var(--bg-tertiary, #40444b);
  }

  .address-chip:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }

  .meta-line {
    margin: 0 0 16px;
    color: var(--text-secondary, #b9bbbe);
    font-size: 12px;
  }

  .metrics-section {
    display: flex;
    flex-direction: column;
    gap: 12px;
    margin-bottom: 16px;
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

  .links-section {
    border-top: 1px solid var(--bg-tertiary, #40444b);
    padding-top: 12px;
  }

  .links-heading {
    margin: 0 0 8px;
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-secondary, #b9bbbe);
  }

  .links-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .link-item {
    display: flex;
    align-items: center;
    justify-content: space-between;
    width: 100%;
    padding: 6px 10px;
    border: none;
    border-radius: 4px;
    background: var(--bg-secondary, #2f3136);
    color: var(--text-primary, #dcddde);
    font: inherit;
    font-size: 12px;
    cursor: pointer;
  }

  .link-item:hover {
    background: var(--bg-tertiary, #40444b);
  }

  .link-item:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: -2px;
  }

  .link-type {
    font-weight: 600;
    font-size: 11px;
    letter-spacing: 0.05em;
  }

  .link-stats {
    color: var(--text-secondary, #b9bbbe);
    font-size: 12px;
  }
</style>
