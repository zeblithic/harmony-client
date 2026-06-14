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

// ZEB-380: per-relay health types. Rust uses serde internally-tagged enums
// with rename_all = "camelCase", so wire shapes use `kind` discriminant.
export type RelayState =
  | { kind: 'healthy' }
  | { kind: 'coolingDown'; untilMs: number };

export type RelayOutcome =
  | { kind: 'success' }
  | { kind: 'timeout' }
  | { kind: 'transport' }
  | { kind: 'http'; status: number };

export interface RelayHealth {
  url: string;
  state: RelayState;
  lastOutcome: RelayOutcome | null;
  lastSuccessMs: number | null;
}

export interface PkarrHealthSummary {
  identityPublished: boolean;
  identityLastPublishMs: number | null;
  communityPublishCount: number;
  recentFallbackEvents: PkarrFallbackHit[];
  /** ZEB-380: per-relay health. Always present in schema v3+. */
  relays: RelayHealth[];
}

export interface DynamicDialHit {
  nodeIdShort: string;
  ownerShort: string;
  outcome: string; // "succeeded" | "failed"
  capturedAtMs: number;
}

export interface DialHealthSummary {
  attempts: number;
  succeeded: number;
  failed: number;
  skippedDuplicate: number;
  recent: DynamicDialHit[];
}

export interface NetworkHealthSnapshot {
  schemaVersion: number;
  capturedAtMs: number;
  appVersion: string;
  platform: string;
  myNetwork: MyNetworkSummary | null;
  peers: PeerHealth[];
  pkarrStatus: PkarrHealthSummary;
  /**
   * ZEB-373: dial telemetry. Always present in schema v2+ responses (Rust
   * serializes `dial_status` unconditionally). Kept optional only for
   * forward-compat with any pre-v2 (schema v1) snapshot that predates the field;
   * live `network_health_snapshot` responses always include it. (Greptile, PR #190.)
   */
  dialStatus?: DialHealthSummary;
  /**
   * ZEB-450: set when the iroh transport could not be brought up this session
   * (key load/create failed — e.g. HARMONY_DISABLE_KEYCHAIN with no passphrase —
   * or endpoint bind failed at boot). `null`/absent when transport is up or
   * still initializing. Drives a persistent "this node can't network" banner so
   * the failure is loud in the UI instead of buried in a boot log line. Optional
   * for forward-compat with pre-field snapshots (Rust `#[serde(default)]`).
   */
  transportDisabledReason?: string | null;
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
