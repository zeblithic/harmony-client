import type { DiscoveredNode } from './zenoh-service';
import type { NetworkNode } from './network-types';
import { RingBuffer } from './ring-buffer';
import type { NodeMetrics } from './network-types';

const METRICS_HISTORY_CAPACITY = 300;

/** Cache of RingBuffers keyed by nodeAddr — reused across mergeNodes() calls
 *  so history isn't discarded on every tick. */
const ringBufferCache = new Map<string, RingBuffer<NodeMetrics>>();

function getCachedRingBuffer(nodeAddr: string): RingBuffer<NodeMetrics> {
  let buf = ringBufferCache.get(nodeAddr);
  if (!buf) {
    buf = new RingBuffer(METRICS_HISTORY_CAPACITY);
    ringBufferCache.set(nodeAddr, buf);
  }
  return buf;
}

/** Remove cached RingBuffers for nodes no longer discovered. */
export function pruneRingBufferCache(activeAddresses: Set<string>): void {
  for (const addr of ringBufferCache.keys()) {
    if (!activeAddresses.has(addr)) {
      ringBufferCache.delete(addr);
    }
  }
}

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
    hopDistance: 2, // TODO: derive from Zenoh routing info; placeholder for now
    status: node.ready ? 'online' : 'degraded',
    metrics: {
      timestamp: node.lastSeen,
      cpuPercent: 0,
      memoryUsedBytes: 0,
      memoryTotalBytes: 1, // sentinel — avoids NaN in usage% until real metrics arrive
      diskUsedBytes: 0,
      diskTotalBytes: 1,   // sentinel — avoids NaN in usage% until real metrics arrive
    },
    metricsHistory: getCachedRingBuffer(node.nodeAddr),
    lastSeen: node.lastSeen,
    capabilities: ['inference', 'routing'],
    heatPercent: 0,
    modelName: shortCid,
  };
}
