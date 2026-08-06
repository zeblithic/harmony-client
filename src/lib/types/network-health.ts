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

// ZEB-804: per-peer staleness tier derived server-side at snapshot assembly
// from the freshest traffic evidence (see Rust `PeerStaleness`,
// network_health.rs; serde rename_all = "camelCase" wire values). The tier
// says "no evidence", not "down" — thresholds are deliberately generous
// (fresh < 5 min ≤ quiet ≤ 30 min < dark).
export type PeerStaleness = 'fresh' | 'quiet' | 'dark';

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
  /**
   * ZEB-804: freshest served-traffic evidence for this peer — max of the
   * liveness machine's rx app-frame stamp (≤30s coarse) and the acceptor
   * registry's served stamp. `null` when neither source has evidence.
   * Optional for forward-compat with pre-field snapshots (Rust
   * `#[serde(default)]`).
   */
  lastTrafficMs?: number | null;
  /**
   * ZEB-804: most recent successfully served community-relay pull from this
   * peer (success-only by design — the relay-pull cadence is the
   * staleness-tier signal). Optional for pre-field snapshots.
   */
  lastRelayPullServedMs?: number | null;
  /**
   * ZEB-804: when the current connection was established — the establishment
   * stamp under its honest name (NOT traffic evidence). `null` when no source
   * reports the peer connected. Optional for pre-field snapshots.
   */
  connectedSinceMs?: number | null;
  /**
   * ZEB-804: derived staleness tier, bucketed server-side from the final
   * merged `lastTrafficMs`. `null` when `connectionMode` is `noConnection`
   * (absence of a connection is already honest). A connected-looking peer
   * with NO traffic evidence ever reads `'dark'` — the ZEB-804 incident
   * shape. Optional for pre-field snapshots.
   */
  staleness?: PeerStaleness | null;
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
  /**
   * ZEB-804 (spec §8): lifetime count of Connected entries via registry swap
   * (inbound accepts / zenoh-initiated links the supervisor ladder never
   * dialed). Makes an inbound-only node's `dialStatus` legible: `attempts: 0`
   * beside `connectedViaRegistry > 0` reads "healthy listener", not "dialing
   * is broken". Optional for pre-field snapshots (Rust `#[serde(default)]`).
   */
  connectedViaRegistry?: number;
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
  /**
   * ZEB-803: ZEB-458 community-relay health, both directions.
   *
   * `null`/absent means this node runs no relay wiring at all — which is a
   * DIFFERENT statement from "wired and serving nothing". The latter arrives as
   * a present object with zeroed counters, and is exactly the incident state
   * this field exists to surface, so the two must not be rendered alike.
   */
  communityRelay?: CommunityRelayHealth | null;
  /**
   * ZEB-803: the self-healing relay-acceptor watchdog's own state. Present iff
   * the relay serving path is wired (parity with `communityRelay`). Absent on
   * pre-field snapshots (Rust `#[serde(default)]`).
   */
  relayAcceptorWatchdog?: RelayAcceptorWatchdogHealth | null;
  /**
   * ZEB-805: per-community sync-advance health, one row per live community
   * engine. Absent on pre-field snapshots (Rust `#[serde(default)]`); an empty
   * array means no engines are running, which is honest rather than a fault.
   */
  communitySync?: CommunitySyncHealth[];
}

/**
 * ZEB-803: the relay-acceptor watchdog's own state (camelCase mirror of Rust
 * `RelayAcceptorWatchdogHealth`, network_health.rs). Surfaced so a silent
 * serving stall — and the watchdog's remediation of it — is never invisible.
 *
 * `phase` is `"normal" | "cooldown" | "escalated"`; `lastActionTier` is
 * `"probe" | "restart"`. `escalated` true means the watchdog exhausted its
 * restart budget and stopped acting (needs a human) rather than looping.
 */
export interface RelayAcceptorWatchdogHealth {
  stalenessMs: number | null;
  connectedPeers: number;
  phase: 'normal' | 'cooldown' | 'escalated';
  consecutiveRestarts: number;
  lastActionMs: number | null;
  lastActionTier: 'probe' | 'restart' | null;
  escalated: boolean;
}

/**
 * ZEB-805: per-community sync-advance health (camelCase mirror of Rust
 * `CommunitySyncHealth`, network_health.rs).
 *
 * **Read `lastInboundMs` and `lastAdvanceMs` together.** Inbound advancing
 * while advance stays frozen is the receiving-and-discarding signature: the
 * node is taking state-root publishes and failing to apply any of them. That
 * state reported `reachable`, both peers `direct`, all members `online`, and
 * exchanged nothing for 90 minutes — indistinguishable, from its own vantage,
 * from a quiet community until these two numbers sat side by side.
 *
 * Neither field alone can say it: inbound alone cannot separate "applying
 * fine" from "dropping everything"; advance alone cannot separate "wedged"
 * from "genuinely quiet".
 */
export interface CommunitySyncHealth {
  /** First 8 hex chars of the community id. */
  communityShort: string;
  /** Wall ms of the last state-root publish RECEIVED, whatever became of it. */
  lastInboundMs?: number | null;
  /** Wall ms of the last publish that actually MERGED. */
  lastAdvanceMs?: number | null;
  /**
   * Tier derived server-side from `lastAdvanceMs` — NOT from `lastInboundMs`.
   * Shares the peer-row vocabulary and thresholds. `null` when nothing has ever
   * arrived (a solo community has no evidence to age).
   */
  staleness?: PeerStaleness | null;
  /** Bounded-retry counters. `exhausted` climbing means publishes are being lost. */
  fetchRetriesScheduled: number;
  fetchRetriesDropped: number;
  fetchRetriesExhausted: number;
  /**
   * ZEB-762: publish-side RetryBackoff health — the OUTBOUND-replication
   * complement to `lastInboundMs`/`lastAdvanceMs` (which answer "am I
   * receiving?"; this answers "am I successfully publishing MY state out?").
   * Always present for a live engine; `owed: false` with zero counters is the
   * healthy state. Optional here only for forward-compat with a pre-field
   * cached snapshot (Rust `#[serde(default)]`).
   */
  publishRetry?: CommunityPublishRetryHealth;
}

/**
 * ZEB-762: per-community publish-side retry state (camelCase mirror of Rust
 * `CommunityPublishRetryHealth`, network_health.rs). `owed: true` with
 * `backoffMs` at/near the 600 s cap (ZEB-761) is the sustained-stall incident:
 * a node durably holding local state it can no longer replicate outward.
 */
export interface CommunityPublishRetryHealth {
  /** Whether an autonomous publish retry is currently owed. */
  owed: boolean;
  /** Consecutive failed publish attempts since the last success (unbounded). */
  consecutiveFailures: number;
  /** Current backoff interval (ms); `0` when nothing is owed. */
  backoffMs: number;
  /** Wall ms of the most recent failed publish. `null` if none ever. */
  lastFailureMs?: number | null;
  /**
   * Coarse class of the most recent publish failure (`transport_closed` /
   * `content_store` / `crypto` / `encode` / `other`). `null` if none ever.
   */
  lastError?: string | null;
}

/** ZEB-702: process-lifetime butler-deposit decision counters (camelCase
 * mirror of Rust `ButlerDepositHealth`, network_health.rs). */
export interface ButlerDepositHealth {
  accepted: number;
  rejectedUnauthorized: number;
  rejectedOther: number;
}

/**
 * ZEB-803: community-relay health (camelCase mirror of Rust
 * `CommunityRelayHealth`, network_health.rs).
 *
 * Both directions are carried because the two ends cannot see each other: a
 * serving fault and a pulling fault present identically to the peer observing
 * them, which is why the originating incident needed a third node to resolve.
 */
export interface CommunityRelayHealth {
  serving: CommunityRelayServingHealth;
  pulling: CommunityRelayPullingHealth;
}

/** ZEB-803: acceptor side — are we serving pulls to peers relaying through us? */
export interface CommunityRelayServingHealth {
  pullsServed: number;
  pullsRejected: number;
  pullsFailed: number;
  /**
   * Wall ms of the most recent successfully served pull, any peer. `null` until
   * this node serves its first — deliberately not `0`, so "never served" cannot
   * render as the epoch.
   *
   * Serving cadence is ~7m30s per peer, so a value older than ~3 cadences while
   * peers are believed connected is the incident signature.
   */
  lastServedMs: number | null;
  /** Per-peer, newest-served first. Bounded server-side. */
  peers: CommunityRelayPeerServed[];
}

/** ZEB-803: one peer's served-pull record. `peerShort` is 8 hex chars —
 * truncated at the writer per the ZEB-329 redaction invariant. */
export interface CommunityRelayPeerServed {
  peerShort: string;
  lastServedMs: number;
  servedCount: number;
}

/** ZEB-803: puller side — are we successfully pulling our held blobs? */
export interface CommunityRelayPullingHealth {
  /**
   * Pull passes started. This is the LIVENESS PROOF: it climbs on the idle
   * backstop even with zero joined communities, so a flat value means the loop
   * is gone rather than merely idle. The success counters below cannot make
   * that distinction, which is what let a silent stall look healthy.
   */
  passesRun: number;
  lastPassMs: number | null;
  sessionsOk: number;
  sessionsFailed: number;
  blobsIngested: number;
  lastIngestMs: number | null;
  /**
   * A joined community examined with NO fresh relay advertised — nothing was
   * tried, so it is not a failure, but it is also not health. Previously this
   * path emitted nothing at all and was indistinguishable from a quiet channel.
   */
  passesNoRelay: number;
  recent: CommunityRelayPullHit[];
}

/** ZEB-803: one pull-session outcome. Short-form ids only (ZEB-329). */
export interface CommunityRelayPullHit {
  communityShort: string;
  relayDeviceShort: string;
  outcome: 'ok' | 'failed' | 'noRelay';
  /** Blobs ingested by this session; `0` for a failure or a no-op success. */
  ingested: number;
  capturedAtMs: number;
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
