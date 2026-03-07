import { RingBuffer } from './ring-buffer';
import { randomInt } from './random-utils';
import type {
  NetworkNode,
  NetworkLink,
  NodeMetrics,
  LinkSnapshot,
  InterfaceType,
  NodeStatus,
  NetworkDataService,
} from './network-types';

const NATO_NAMES = [
  'Alpha', 'Bravo', 'Charlie', 'Delta', 'Echo', 'Foxtrot',
  'Golf', 'Hotel', 'India', 'Juliet', 'Kilo', 'Lima',
  'Mike', 'November', 'Oscar', 'Papa', 'Quebec', 'Romeo',
  'Sierra', 'Tango', 'Uniform', 'Victor', 'Whiskey', 'Xray',
  'Yankee', 'Zulu',
];

const INTERFACE_TYPES: InterfaceType[] = ['tcp', 'udp', 'serial', 'i2p', 'lora', 'pipe'];

const METRICS_HISTORY_CAPACITY = 300;

function randomHexAddress(): string {
  const bytes = new Array(16);
  for (let i = 0; i < 16; i++) {
    bytes[i] = Math.floor(Math.random() * 256).toString(16).padStart(2, '0');
  }
  return bytes.join('');
}

function randomBetween(min: number, max: number): number {
  return min + Math.random() * (max - min);
}


function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function pickRandom<T>(arr: T[]): T {
  return arr[Math.floor(Math.random() * arr.length)];
}

function createInitialMetrics(): NodeMetrics {
  const memoryTotalBytes = randomInt(4, 32) * 1024 * 1024 * 1024;
  const memoryUsedFraction = randomBetween(0.2, 0.6);
  const diskTotalBytes = randomInt(64, 512) * 1024 * 1024 * 1024;
  const diskUsedFraction = randomBetween(0.1, 0.3);

  return {
    timestamp: Date.now(),
    cpuPercent: randomBetween(10, 50),
    memoryUsedBytes: Math.floor(memoryTotalBytes * memoryUsedFraction),
    memoryTotalBytes,
    diskUsedBytes: Math.floor(diskTotalBytes * diskUsedFraction),
    diskTotalBytes,
  };
}

function createNode(
  name: string,
  isLocal: boolean,
  hopDistance: number,
): NetworkNode {
  return {
    address: randomHexAddress(),
    displayName: name,
    isLocal,
    hopDistance,
    status: 'online',
    metrics: createInitialMetrics(),
    metricsHistory: new RingBuffer<NodeMetrics>(METRICS_HISTORY_CAPACITY),
    lastSeen: Date.now(),
  };
}

function createLink(source: string, target: string): NetworkLink {
  const interfaceType = pickRandom(INTERFACE_TYPES);
  const capacityMap: Record<InterfaceType, number> = {
    tcp: 100_000_000,
    udp: 100_000_000,
    serial: 115_200,
    i2p: 1_000_000,
    lora: 37_500,
    pipe: 1_000_000_000,
  };

  return {
    id: `${source.slice(0, 8)}-${target.slice(0, 8)}`,
    source,
    target,
    interfaceType,
    capacityBps: capacityMap[interfaceType],
    utilizationPercent: randomBetween(5, 40),
    latencyMs: randomBetween(1, 200),
    utilizationHistory: new RingBuffer<LinkSnapshot>(METRICS_HISTORY_CAPACITY),
  };
}

interface PendingDiscovery {
  address: string;
  readyAtTick: number;
}

export class MockNetworkDataService implements NetworkDataService {
  nodes: NetworkNode[];
  links: NetworkLink[];
  onAlert?: (message: string) => void;
  onTick?: () => void;

  private intervalId: ReturnType<typeof setInterval> | null = null;
  private tickCount = 0;
  private usedNames: Set<string> = new Set();
  private pendingDiscoveries: PendingDiscovery[] = [];

  constructor() {
    const nodeCount = randomInt(8, 12);
    this.nodes = [];
    this.links = [];

    // Create local node (hop 0)
    const localName = this.pickUniqueName();
    this.nodes.push(createNode(localName, true, 0));

    // Create 3 hop-1 nodes
    for (let i = 0; i < 3; i++) {
      this.nodes.push(createNode(this.pickUniqueName(), false, 1));
    }

    // Create remaining hop-2 nodes
    for (let i = 4; i < nodeCount; i++) {
      this.nodes.push(createNode(this.pickUniqueName(), false, 2));
    }

    // Create a connected graph of links
    // Connect each hop-1 node to the local node
    const localAddr = this.nodes[0].address;
    for (let i = 1; i <= 3; i++) {
      this.links.push(createLink(localAddr, this.nodes[i].address));
    }

    // Connect hop-2 nodes to random hop-1 nodes
    for (let i = 4; i < this.nodes.length; i++) {
      const hop1Index = randomInt(1, 3);
      this.links.push(createLink(this.nodes[hop1Index].address, this.nodes[i].address));
    }

    // Add a few extra links for mesh redundancy
    if (this.nodes.length > 5) {
      const extraLinks = randomInt(1, 3);
      for (let i = 0; i < extraLinks; i++) {
        const a = randomInt(1, this.nodes.length - 1);
        let b = randomInt(1, this.nodes.length - 1);
        if (b === a) b = (b + 1) % (this.nodes.length - 1) + 1;
        const existingLink = this.links.find(
          (l) =>
            (l.source === this.nodes[a].address && l.target === this.nodes[b].address) ||
            (l.source === this.nodes[b].address && l.target === this.nodes[a].address),
        );
        if (!existingLink) {
          this.links.push(createLink(this.nodes[a].address, this.nodes[b].address));
        }
      }
    }
  }

  start(): void {
    if (this.intervalId !== null) return;

    this.intervalId = setInterval(() => {
      this.tick();
    }, 1000);
  }

  stop(): void {
    if (this.intervalId !== null) {
      clearInterval(this.intervalId);
      this.intervalId = null;
    }
  }

  requestPeerData(address: string): void {
    // Stub for future backend integration — not called by the mock UI,
    // but required by the NetworkDataService interface.
    // Queue a discovery that resolves 1-2 seconds later
    const delayTicks = randomInt(1, 2);
    this.pendingDiscoveries.push({
      address,
      readyAtTick: this.tickCount + delayTicks,
    });
  }

  private tick(): void {
    this.tickCount++;
    const now = Date.now();

    // Update node metrics
    for (const node of this.nodes) {
      // Status transitions and alerts happen for all nodes, but
      // metrics only update for non-offline nodes
      if (node.status === 'offline') {
        // Skip metrics update — preserve last-known values
      } else {
        const prevCpu = node.metrics.cpuPercent;

        // Sine-wave drift + random noise for CPU
        const sineComponent = Math.sin(this.tickCount * 0.05) * 10;
        const noise = (Math.random() - 0.5) * 8;
        const newCpu = clamp(prevCpu + sineComponent * 0.1 + noise, 2, 98);

        // Memory drifts slightly
        const memDrift = (Math.random() - 0.5) * 0.01 * node.metrics.memoryTotalBytes;
        const newMemUsed = clamp(
          node.metrics.memoryUsedBytes + memDrift,
          0,
          node.metrics.memoryTotalBytes,
        );

        const newMetrics: NodeMetrics = {
          timestamp: now,
          cpuPercent: newCpu,
          memoryUsedBytes: Math.floor(newMemUsed),
          memoryTotalBytes: node.metrics.memoryTotalBytes,
          diskUsedBytes: node.metrics.diskUsedBytes,
          diskTotalBytes: node.metrics.diskTotalBytes,
        };

        node.metrics = newMetrics;
        node.metricsHistory.push(newMetrics);
        node.lastSeen = now;
      }

      // Status transitions based on CPU
      const prevStatus = node.status;
      const currentCpu = node.metrics.cpuPercent;
      if (node.status === 'online' && currentCpu > 85) {
        node.status = 'degraded';
      } else if (node.status === 'degraded' && currentCpu < 70) {
        node.status = 'online';
      }

      if (node.status !== prevStatus) {
        this.onAlert?.(
          `Node ${node.displayName} (${node.address.slice(0, 8)}) changed from ${prevStatus} to ${node.status}`,
        );
      }

      // Occasional offline/recovery for non-local nodes (~15% chance every 60 ticks)
      const preOfflineStatus = node.status;
      if (!node.isLocal && this.tickCount % 60 === 0) {
        if (Math.random() < 0.15) {
          if (node.status === 'online' || node.status === 'degraded') {
            node.status = 'offline';
          } else {
            node.status = 'online';
          }
        }
      }

      if (node.status !== preOfflineStatus) {
        this.onAlert?.(
          `Node ${node.displayName} (${node.address.slice(0, 8)}) changed from ${preOfflineStatus} to ${node.status}`,
        );
      }
    }

    // Update link utilization
    for (const link of this.links) {
      const sineComponent = Math.sin(this.tickCount * 0.03) * 15;
      const noise = (Math.random() - 0.5) * 10;
      link.utilizationPercent = clamp(
        link.utilizationPercent + sineComponent * 0.1 + noise,
        0,
        100,
      );
      link.latencyMs = clamp(link.latencyMs + (Math.random() - 0.5) * 5, 0.5, 500);

      const snapshot: LinkSnapshot = {
        timestamp: now,
        utilizationPercent: link.utilizationPercent,
        latencyMs: link.latencyMs,
        bytesSent: Math.floor(Math.random() * link.capacityBps * 0.01),
        bytesReceived: Math.floor(Math.random() * link.capacityBps * 0.01),
      };
      link.utilizationHistory.push(snapshot);
    }

    // Process pending peer discoveries
    const ready = this.pendingDiscoveries.filter((d) => d.readyAtTick <= this.tickCount);
    this.pendingDiscoveries = this.pendingDiscoveries.filter(
      (d) => d.readyAtTick > this.tickCount,
    );

    for (const discovery of ready) {
      this.addDiscoveredNodes(discovery.address);
    }

    this.onTick?.();
  }

  private addDiscoveredNodes(parentAddress: string): void {
    const count = randomInt(2, 3);
    const parentNode = this.nodes.find((n) => n.address === parentAddress);
    const parentHop = parentNode ? parentNode.hopDistance : 2;

    for (let i = 0; i < count; i++) {
      const newNode = createNode(this.pickUniqueName(), false, parentHop + 1);
      this.nodes.push(newNode);
      this.links.push(createLink(parentAddress, newNode.address));
    }
  }

  private pickUniqueName(): string {
    for (const name of NATO_NAMES) {
      if (!this.usedNames.has(name)) {
        this.usedNames.add(name);
        return name;
      }
    }
    // Fallback if all NATO names used: generate a numbered name
    const fallback = `Node-${this.usedNames.size}`;
    this.usedNames.add(fallback);
    return fallback;
  }
}
