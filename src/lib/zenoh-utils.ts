import type { DiscoveredNode } from './zenoh-service';
import type { NetworkNode } from './network-types';
import { RingBuffer } from './ring-buffer';

const METRICS_HISTORY_CAPACITY = 300;

/** Convert a DiscoveredNode from Zenoh capacity into a NetworkNode for the graph. */
export function discoveredToNetworkNode(node: DiscoveredNode): NetworkNode {
  const shortAddr = node.nodeAddr.length > 8
    ? node.nodeAddr.slice(0, 8)
    : node.nodeAddr;
  const shortCid = node.modelCid.length > 8
    ? node.modelCid.slice(0, 8)
    : node.modelCid;

  return {
    address: node.nodeAddr,
    displayName: `${shortAddr} (live)`,
    isLocal: false,
    hopDistance: 2,
    status: node.ready ? 'online' : 'degraded',
    metrics: {
      timestamp: node.lastSeen,
      cpuPercent: 0,
      memoryUsedBytes: 0,
      memoryTotalBytes: 1,
      diskUsedBytes: 0,
      diskTotalBytes: 1,
    },
    metricsHistory: new RingBuffer(METRICS_HISTORY_CAPACITY),
    lastSeen: node.lastSeen,
    capabilities: ['inference', 'routing'],
    heatPercent: 0,
    modelName: shortCid,
  };
}
