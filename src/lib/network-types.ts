import type { RingBuffer } from './ring-buffer';

export interface NodeMetrics {
  timestamp: number;
  cpuPercent: number;
  memoryUsedBytes: number;
  memoryTotalBytes: number;
  diskUsedBytes: number;
  diskTotalBytes: number;
}

export interface LinkSnapshot {
  timestamp: number;
  utilizationPercent: number;
  latencyMs: number;
  bytesSent: number;
  bytesReceived: number;
}

export type NodeStatus = 'online' | 'degraded' | 'offline';
export type InterfaceType = 'tcp' | 'udp' | 'serial' | 'i2p' | 'lora' | 'pipe';
export type NodeCapability = 'inference' | 'storage' | 'routing' | 'compute';
export type TransportType = 'iroh' | 'reticulum' | 'zenoh' | 'rawlink' | 's3';

export interface NetworkNode {
  address: string;
  displayName: string;
  isLocal: boolean;
  hopDistance: number;
  status: NodeStatus;
  metrics: NodeMetrics;
  metricsHistory: RingBuffer<NodeMetrics>;
  lastSeen: number;
  capabilities: NodeCapability[];
  heatPercent: number;
  modelName?: string;
}

export interface NetworkLink {
  id: string;
  source: string;
  target: string;
  interfaceType: InterfaceType;
  capacityBps: number;
  utilizationPercent: number;
  latencyMs: number;
  utilizationHistory: RingBuffer<LinkSnapshot>;
  transportType: TransportType;
  encrypted: boolean;
}

export interface NetworkDataService {
  nodes: NetworkNode[];
  links: NetworkLink[];
  start(): void;
  stop(): void;
  requestPeerData(address: string): void;
  onAlert?: (message: string) => void;
  onTick?: () => void;
}
