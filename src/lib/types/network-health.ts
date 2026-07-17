// ZEB-329 — frontend types mirroring src-tauri/src/network_health.rs.
// All fields in camelCase per Tauri serde rename_all = "camelCase".

export type ReachabilityStatus = 'reachable' | 'degraded' | 'unreachable';

export type NatClass =
  | 'fullCone'
  | 'restrictedCone'
  | 'portRestricted'
  | 'symmetric'
  | 'unknown';

// ZEB-622: `degraded` — the transport link is up but no selected path is known
// yet (an up-edge before the first path report, or a lost-path report on a
// still-live conn). Mirrors Rust `ConnectionMode::Degraded` (wire `"degraded"`).
export type ConnectionMode = 'direct' | 'relay' | 'noConnection' | 'degraded';

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
  // ZEB-623: set when the tunnel-v2 hello negotiation recorded this peer as
  // protocol-incompatible; carries the reason the panel shows in a loud badge.
  // `null` when compatible. Rust serializes it unconditionally (present as
  // `null` when None), so it is always on the wire for a schema-v4 snapshot.
  protocolIncompatReason: string | null;
}

// ZEB-595: three-state so the panel can distinguish a clean "not published
// here" (miss) from a probe that couldn't produce a trustworthy answer
// (error) — they must not be conflated during incident triage.
export type PkarrFallbackOutcome = 'hit' | 'miss' | 'error';

export interface PkarrFallbackHit {
  peerAddrShort: string;
  communityIdShort: string;
  outcome: PkarrFallbackOutcome;
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

// ZEB-624: iroh transport relay configuration. Distinct from the pkarr relay
// pool (per-relay RelayHealth) — the iroh wire carries no per-relay health.
// `custom === false` means the node is following iroh's recommended defaults
// (the returned `relays` list shows them); `true` means a materialized custom
// list. Mirrors the Rust `IrohRelayInfo` DTO (serde rename_all = "camelCase").
export interface IrohRelayInfo {
  relays: string[];
  custom: boolean;
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
  // ZEB-620/622 dial-ring markers. Dial outcomes: "succeeded" | "failed";
  // reconnect-supervisor state transitions: "reconnected" | "retrying" |
  // "dormant".
  outcome: string;
  capturedAtMs: number;
}

export interface DialHealthSummary {
  attempts: number;
  succeeded: number;
  failed: number;
  skippedDuplicate: number;
  // ZEB-620: live per-peer-state counts from the reconnect supervisor, folded
  // into the dial summary. Rust `#[serde(default)]`, so a pre-field snapshot
  // (or old cached WS data) may omit them — optional here; coalesce with `?? 0`
  // at the render site.
  retrying?: number;
  dormant?: number;
  connected?: number;
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
  /**
   * ZEB-702: butler-deposit accept/reject decision counts from this node's
   * deposit acceptor. `null`/absent on nodes running no acceptor (no owner
   * identity loaded) and on pre-field snapshots (Rust `#[serde(default)]`).
   * Lets an always-rejecting butler (sibling whose roster never converged)
   * be told apart from transport failure without a debug-level log session.
   */
  butlerDeposits?: ButlerDepositHealth | null;
}

/** ZEB-702: process-lifetime butler-deposit decision counters (camelCase
 * mirror of Rust `ButlerDepositHealth`, network_health.rs). */
export interface ButlerDepositHealth {
  accepted: number;
  rejectedUnauthorized: number;
  rejectedOther: number;
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
