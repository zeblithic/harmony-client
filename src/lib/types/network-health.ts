// ZEB-329 — frontend types mirroring src-tauri/src/network_health.rs.
// All fields in camelCase per Tauri serde rename_all = "camelCase".

export type ReachabilityStatus = 'reachable' | 'degraded' | 'unreachable';

export type NatClass =
  | 'fullCone'
  | 'restrictedCone'
  | 'portRestricted'
  | 'symmetric'
  | 'unknown';

export type ConnectionMode = 'direct' | 'relay' | 'noConnection';

export interface MyNetworkSummary {
  irohNodeId: string;
  reachability: ReachabilityStatus;
  natClassification: NatClass;
  homeRelayUrl: string | null;
  relayRttMs: number | null;
  directAddresses: string[];
}

export interface PeerHealth {
  ownerAddr: string;
  displayName: string | null;
  sharedCommunities: string[];
  connectionMode: ConnectionMode;
  rttMs: number | null;
  lastSeenMs: number | null;
  reachabilityRecordAgeMs: number | null;
}

export interface PkarrFallbackHit {
  peerAddrShort: string;
  communityIdShort: string;
  hit: boolean;
  capturedAtMs: number;
}

export interface PkarrHealthSummary {
  identityPublished: boolean;
  identityLastPublishMs: number | null;
  communityPublishCount: number;
  recentFallbackEvents: PkarrFallbackHit[];
}

export interface NetworkHealthSnapshot {
  schemaVersion: number;
  capturedAtMs: number;
  appVersion: string;
  platform: string;
  myNetwork: MyNetworkSummary | null;
  peers: PeerHealth[];
  pkarrStatus: PkarrHealthSummary;
}

export type StepOutcome =
  | { type: 'pass'; durationMs: number }
  | { type: 'fail'; reason: string }
  | { type: 'skipped'; reason: string };

export interface SelfTestStep {
  name: string;
  outcome: StepOutcome;
}

export interface PeerPingResult {
  ownerAddr: string;
  outcome: StepOutcome;
  mode: ConnectionMode | null;
}

export interface SelfTestReport {
  startedAtMs: number;
  finishedAtMs: number;
  steps: SelfTestStep[];
  peerResults: PeerPingResult[];
}
