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

/** Default age threshold in milliseconds (30 seconds). */
export const NODE_AGE_THRESHOLD_MS = 30_000;

/** Filter out discovered nodes whose lastSeen is older than the threshold. */
export function filterStaleNodes(
  nodes: Map<string, DiscoveredNode>,
  now: number = Date.now(),
  thresholdMs: number = NODE_AGE_THRESHOLD_MS,
): DiscoveredNode[] {
  const fresh: DiscoveredNode[] = [];
  for (const node of nodes.values()) {
    if (now - node.lastSeen <= thresholdMs) {
      fresh.push(node);
    }
  }
  return fresh;
}

/** Convert a DiscoveredNode from Zenoh capacity into a NetworkNode for the graph. */
export function discoveredToNetworkNode(node: DiscoveredNode): NetworkNode {
  // Truncate to 4 chars so displayName "abcd (live)" = 11 chars,
  // fitting within NetworkGraph's 12-char label limit.
  const shortAddr = node.nodeAddr.length > 4
    ? node.nodeAddr.slice(0, 4)
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
      cpuPercent: node.cpuPercent ?? 0,
      memoryUsedBytes: (node.memMb ?? 0) * 1024 * 1024,
      memoryTotalBytes: 1, // sentinel — total memory not available in current health payload
      diskUsedBytes: 0,
      diskTotalBytes: 1,   // sentinel — avoids NaN in usage% until real metrics arrive
    },
    metricsHistory: getCachedRingBuffer(node.nodeAddr),
    lastSeen: node.lastSeen,
    capabilities: ['inference', 'routing'],
    heatPercent: node.cpuPercent ?? 0,
    modelName: shortCid,
  };
}
