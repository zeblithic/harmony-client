//! ZEB-329 — Network Health: cross-WAN validation surface.
//!
//! See `docs/specs/2026-05-24-zeb-329-network-health-design.md` for the
//! full design. This module is **synthesis only** — it reads from
//! existing sources (iroh::Endpoint, ReachabilityResolver, pkarr
//! publishers, my-membership set) and never mutates them. Pure
//! functions (classify_nat, derive_reachability_status,
//! filter_peers_by_shared_membership, format_export_markdown) are
//! decomposed for direct unit testing without iroh / network.
//!
//! ## Cache vs commit token (memory rule feedback_two_ipc_toctou)
//!
//! `network_health_run_self_test` writes to a cached
//! `Arc<RwLock<Option<SelfTestReport>>>` that `network_health_export_payload`
//! later reads. This is NOT a write/commit token pair — the cache is a
//! memo of the most recent test result, not a binding identifier. A
//! TOCTOU race here only means an export sees a stale report (or no
//! report); the export's correctness does not depend on a contract
//! between the two IPCs.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

// ── Public data types (wire shape for IPC) ──────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkHealthSnapshot {
    /// Defaults to 1; bump on breaking export-format changes per spec §4.4.
    pub schema_version: u32,
    pub captured_at_ms: u64,
    pub app_version: String,
    pub platform: String,
    /// `None` when iroh isn't yet bound (early boot, sandbox).
    pub my_network: Option<MyNetworkSummary>,
    /// Sorted by `last_seen_ms` desc, `None` values last.
    pub peers: Vec<PeerHealth>,
    pub pkarr_status: PkarrHealthSummary,
    pub dial_status: DialHealthSummary,
    /// ZEB-450: set when the iroh transport could not be brought up this
    /// session (key load/create failed, or endpoint bind failed, at boot).
    /// `None` = transport is up or still initializing normally; `Some(reason)`
    /// carries the loud, actionable explanation (the same class of string
    /// `load_or_create_secret_key` returns) so the UI can show a persistent
    /// "this node can't network" banner instead of the failure living only in a
    /// boot log line. Stamped by the `network_health_snapshot` IPC from
    /// `NodeState` — the disabled case has no `NetworkHealthService` to read
    /// from, so this is intentionally orthogonal to the health sources.
    /// `#[serde(default)]` keeps a pre-field snapshot forward-compatible.
    #[serde(default)]
    pub transport_disabled_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MyNetworkSummary {
    /// Hex-encoded iroh EndpointId (64 lowercase hex chars).
    pub iroh_node_id: String,
    pub reachability: ReachabilityStatus,
    pub nat_classification: NatClass,
    pub home_relay_url: Option<String>,
    pub relay_rtt_ms: Option<u32>,
    pub direct_addresses: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PeerHealth {
    pub owner_addr: String,
    pub display_name: Option<String>,
    pub shared_communities: Vec<String>,
    pub connection_mode: ConnectionMode,
    pub rtt_ms: Option<u32>,
    pub last_seen_ms: Option<u64>,
    pub reachability_record_age_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PkarrHealthSummary {
    pub identity_published: bool,
    pub identity_last_publish_ms: Option<u64>,
    pub community_publish_count: u32,
    pub recent_fallback_events: Vec<PkarrFallbackHit>,
    /// ZEB-380: per-relay health for the configured pool. Empty pre-wiring.
    pub relays: Vec<RelayHealthWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PkarrFallbackHit {
    pub peer_addr_short: String,
    pub community_id_short: String,
    pub hit: bool,
    pub captured_at_ms: u64,
}

/// ZEB-380: camelCase wire shape of one relay's health (maps from
/// `harmony_pkarr::RelayHealth`, whose core type stays idiomatic snake_case).
/// Owned client-side so the IPC contract lives in the consumer repo, same as
/// `DialHealthSummary`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RelayHealthWire {
    pub url: String,
    pub state: RelayStateWire,
    pub last_outcome: Option<RelayOutcomeWire>,
    pub last_success_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RelayStateWire {
    Healthy,
    // `rename_all` on the enum renames the VARIANT (`coolingDown`) but NOT the
    // struct-variant field. Without this per-variant attr the field serializes
    // as snake_case `until_ms`, while the TS DTO reads `untilMs` — yielding a
    // `NaN` cooldown countdown in the UI. ZEB-384.
    #[serde(rename_all = "camelCase")]
    CoolingDown {
        until_ms: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RelayOutcomeWire {
    Success,
    Timeout,
    Transport,
    Http { status: u16 },
}

impl From<harmony_pkarr::RelayHealth> for RelayHealthWire {
    fn from(h: harmony_pkarr::RelayHealth) -> Self {
        RelayHealthWire {
            url: h.url,
            state: match h.state {
                harmony_pkarr::RelayState::Healthy => RelayStateWire::Healthy,
                harmony_pkarr::RelayState::CoolingDown { until_ms } => {
                    RelayStateWire::CoolingDown { until_ms }
                }
            },
            last_outcome: h.last_outcome.map(|o| match o {
                harmony_pkarr::RelayOutcome::Success => RelayOutcomeWire::Success,
                harmony_pkarr::RelayOutcome::Timeout => RelayOutcomeWire::Timeout,
                harmony_pkarr::RelayOutcome::Transport => RelayOutcomeWire::Transport,
                harmony_pkarr::RelayOutcome::Http(status) => RelayOutcomeWire::Http { status },
            }),
            last_success_ms: h.last_success_ms,
        }
    }
}

/// One recorded dial outcome for the Network Health panel (ZEB-373).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DynamicDialHit {
    pub node_id_short: String,
    pub owner_short: String,
    pub outcome: String, // "succeeded" | "failed"
    pub captured_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct DialHealthSummary {
    pub attempts: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub skipped_duplicate: u64,
    pub recent: Vec<DynamicDialHit>,
}

const DIAL_RING_CAP: usize = 32;

/// Process-lifetime counters + a bounded ring of recent dial outcomes. Shared
/// (`Arc`) between the dial driver (writer) and `network_health_snapshot` (reader).
#[derive(Debug, Default)]
pub struct DialTelemetry {
    attempts: AtomicU64,
    succeeded: AtomicU64,
    failed: AtomicU64,
    skipped_duplicate: AtomicU64,
    recent: Mutex<VecDeque<DynamicDialHit>>,
}

impl DialTelemetry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn record_attempt(&self) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_skipped_duplicate(&self) {
        self.skipped_duplicate.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_succeeded(&self, node_id: [u8; 32], owner: [u8; 16]) {
        self.succeeded.fetch_add(1, Ordering::Relaxed);
        self.push(node_id, owner, "succeeded");
    }
    pub fn record_failed(&self, node_id: [u8; 32], owner: [u8; 16]) {
        self.failed.fetch_add(1, Ordering::Relaxed);
        self.push(node_id, owner, "failed");
    }
    fn push(&self, node_id: [u8; 32], owner: [u8; 16], outcome: &str) {
        let hit = DynamicDialHit {
            node_id_short: hex::encode(&node_id[..4]),
            owner_short: hex::encode(&owner[..4]),
            outcome: outcome.to_string(),
            captured_at_ms: now_ms(),
        };
        let mut ring = self.recent.lock().expect("dial ring lock");
        if ring.len() == DIAL_RING_CAP {
            ring.pop_front();
        }
        ring.push_back(hit);
    }
    pub fn summary(&self) -> DialHealthSummary {
        DialHealthSummary {
            attempts: self.attempts.load(Ordering::Relaxed),
            succeeded: self.succeeded.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            skipped_duplicate: self.skipped_duplicate.load(Ordering::Relaxed),
            recent: self
                .recent
                .lock()
                .expect("dial ring lock")
                .iter()
                .cloned()
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ReachabilityStatus {
    Reachable,
    Degraded,
    Unreachable,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum NatClass {
    FullCone,
    RestrictedCone,
    PortRestricted,
    Symmetric,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ConnectionMode {
    Direct,
    Relay,
    NoConnection,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SelfTestReport {
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub steps: Vec<SelfTestStep>,
    pub peer_results: Vec<PeerPingResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SelfTestStep {
    pub name: String,
    pub outcome: StepOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PeerPingResult {
    pub owner_addr: String,
    pub outcome: StepOutcome,
    pub mode: Option<ConnectionMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum StepOutcome {
    Pass { duration_ms: u32 },
    Fail { reason: String },
    Skipped { reason: String },
}

impl NetworkHealthSnapshot {
    /// Empty-but-well-formed snapshot for the "iroh not ready" path
    /// (spec §6.1: snapshot never throws). All renders gracefully:
    /// `my_network: None` → "starting up…" placeholder in UI;
    /// `peers: []` → "no peers yet"; `pkarr_status` zeroed.
    pub fn empty() -> Self {
        Self {
            schema_version: 3,
            captured_at_ms: now_ms(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            platform: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
            my_network: None,
            peers: Vec::new(),
            pkarr_status: PkarrHealthSummary {
                identity_published: false,
                identity_last_publish_ms: None,
                community_publish_count: 0,
                recent_fallback_events: Vec::new(),
                relays: Vec::new(),
            },
            dial_status: DialHealthSummary::default(),
            // ZEB-450: the empty snapshot is the "iroh not ready / no service"
            // path. The reason (if transport is disabled this session) is
            // stamped on by the IPC from NodeState; default None here.
            transport_disabled_reason: None,
        }
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// ZEB-450: stamp the boot-time transport-disabled reason onto a snapshot.
///
/// Pure so the `network_health_snapshot` IPC's stamping is unit-testable
/// without constructing a full `NodeState`. Applied uniformly to BOTH the
/// service snapshot and the `empty()` (service-absent) snapshot, so the single
/// caller covers the disabled case (no `NetworkHealthService`) and any future
/// degraded-but-running case. Overwrites whatever the constructor defaulted.
pub(crate) fn stamp_transport_status(
    mut snap: NetworkHealthSnapshot,
    reason: Option<String>,
) -> NetworkHealthSnapshot {
    snap.transport_disabled_reason = reason;
    snap
}

// ── Pure synthesis functions (no iroh, no network) ──────────────────

/// Spec §4.1: derive top-level reachability from my own state +
/// peer set. Reachable: my_network present + at least one peer is
/// Direct-connected. Degraded: my_network present but all peer
/// connections are Relay or NoConnection. Unreachable: my_network
/// absent OR all peers NoConnection AND none Direct/Relay.
pub fn derive_reachability_status(
    _my: &MyNetworkSummary,
    peers: &[PeerHealth],
) -> ReachabilityStatus {
    if peers
        .iter()
        .any(|p| p.connection_mode == ConnectionMode::Direct)
    {
        ReachabilityStatus::Reachable
    } else if peers
        .iter()
        .any(|p| p.connection_mode == ConnectionMode::Relay)
    {
        ReachabilityStatus::Degraded
    } else if peers.is_empty() {
        // No peers yet ≠ unreachable. Report Reachable because *we* have
        // working endpoint state; reachability of others is unknown,
        // not failing.
        ReachabilityStatus::Reachable
    } else {
        ReachabilityStatus::Unreachable
    }
}

/// Iroh 0.98 may or may not expose NAT classification directly via
/// `ConnectionInfo`. This function wraps whatever iroh provides into
/// our `NatClass` enum. If iroh exposes nothing useful, returns
/// `NatClass::Unknown` (spec §6.1 — snapshot never throws).
///
/// TODO(zeb-329-followup): when iroh ships a stable NAT classifier
/// hook, replace the `Unknown` fallback with real classification.
/// The function signature takes a generic stand-in to keep the
/// interface stable across iroh versions.
pub fn classify_nat<T>(_connection_info: &T) -> NatClass {
    // Phase 1: no iroh-side NAT classification API we can rely on
    // across versions. Render as Unknown; the snapshot still carries
    // home_relay_url + relay_rtt_ms + direct_addresses so testers can
    // self-diagnose without the classifier.
    NatClass::Unknown
}

/// Spec §4.1: peer list scoped to peers we share community membership
/// with. Resolver records are `Vec<(OwnerAddr, ReachabilityPayload)>`;
/// my_memberships is `Vec<(OwnerAddr, Vec<CommunityIdHex>)>` — the
/// existing membership store enumerates communities per owner. Output
/// is sorted by `last_seen_ms` desc with `None` last.
///
/// Pass `now_ms_fn` for testable time (production uses `now_ms`).
pub fn filter_peers_by_shared_membership(
    resolver_records: Vec<ResolverPeerRecord>,
    my_memberships: &dyn MyMembershipSet,
    now_ms: u64,
) -> Vec<PeerHealth> {
    let mut out: Vec<PeerHealth> = Vec::new();
    for r in resolver_records {
        let shared = my_memberships.communities_shared_with(&r.owner_addr);
        if shared.is_empty() {
            continue;
        }
        out.push(PeerHealth {
            owner_addr: r.owner_addr_hex(),
            display_name: r.display_name,
            shared_communities: shared,
            connection_mode: r.connection_mode,
            rtt_ms: r.rtt_ms,
            last_seen_ms: r.last_seen_ms,
            reachability_record_age_ms: r.last_seen_ms.map(|ls| now_ms.saturating_sub(ls)),
        });
    }
    // Sort by last_seen_ms desc; None values last.
    out.sort_by(|a, b| match (b.last_seen_ms, a.last_seen_ms) {
        (Some(bv), Some(av)) => bv.cmp(&av),
        // a has a value, b is None → a should come BEFORE b (None last).
        (None, Some(_)) => std::cmp::Ordering::Less,
        // a is None, b has a value → a should come AFTER b (None last).
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    out
}

/// Plain-data input to `filter_peers_by_shared_membership`. Constructed
/// by `NetworkHealthService::snapshot` (Task 3) from the resolver +
/// connection-info read. Decoupled so the filter is testable without
/// iroh.
#[derive(Debug, Clone)]
pub struct ResolverPeerRecord {
    pub owner_addr: [u8; 16],
    pub display_name: Option<String>,
    pub connection_mode: ConnectionMode,
    pub rtt_ms: Option<u32>,
    pub last_seen_ms: Option<u64>,
}

impl ResolverPeerRecord {
    pub fn owner_addr_hex(&self) -> String {
        hex::encode(self.owner_addr)
    }
}

/// Membership lookup interface — implemented by the production
/// membership store and by test fakes.
pub trait MyMembershipSet {
    /// Return community ids (lowercase hex) that I share with `peer`.
    /// Empty Vec = no shared community → peer is excluded from the
    /// Network Health panel.
    fn communities_shared_with(&self, peer: &[u8; 16]) -> Vec<String>;
}

/// Iroh-side data the snapshot needs. Trait-extracted so unit tests
/// can substitute a fake without running real iroh. Production impl
/// in lib.rs boot wiring delegates to `IrohEndpoint`.
pub trait IrohSnapshot: Send + Sync {
    fn iroh_node_id_hex(&self) -> Option<String>;
    fn home_relay_url(&self) -> Option<String>;
    fn relay_rtt_ms(&self) -> Option<u32>;
    fn direct_addresses(&self) -> Vec<String>;
    fn nat_classification(&self) -> NatClass;
}

/// Identity + community publish state, derived from a *single* read of the
/// publisher's handle set so the two fields can never disagree within one
/// snapshot due to lock-contention timing between separate reads (ZEB-511).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PkarrPublishState {
    pub identity_published: bool,
    pub community_publish_count: u32,
}

/// Pkarr-side data the snapshot needs. Trait-extracted for testability;
/// production impl reads from `pkarr_publisher.try_active_handles()` + the
/// fallback ring buffer.
pub trait PkarrSnapshot: Send + Sync {
    /// Identity + community publish state, read atomically (a single
    /// handle-set read) so one snapshot can't report contradictory fields.
    fn publish_state(&self) -> PkarrPublishState;
    fn recent_fallback_events(&self) -> Vec<PkarrFallbackHit>;
}

/// Resolver-side data. Trait-extracted so the snapshot can be tested
/// without the full ReachabilityResolver. Production impl reads from
/// `ReachabilityResolver::list_active_peers()` + iroh-side
/// connection-mode lookups.
pub trait ReachabilitySnapshot: Send + Sync {
    fn list_records(&self) -> Vec<ResolverPeerRecord>;
}

/// ZEB-373: source of dynamic-dial telemetry for the snapshot. Mirrors the
/// existing `PkarrSnapshot`/`IrohSnapshot` source-trait pattern.
pub trait DialSnapshot: Send + Sync {
    fn dial_summary(&self) -> DialHealthSummary;
}

/// Production source: reads the shared `DialTelemetry` written by the dial
/// driver (`crate::iroh_dial_driver::run_dial_driver`).
pub struct ProdDialSnapshot {
    pub telemetry: std::sync::Arc<DialTelemetry>,
}
impl DialSnapshot for ProdDialSnapshot {
    fn dial_summary(&self) -> DialHealthSummary {
        self.telemetry.summary()
    }
}

/// ZEB-373: trivial `DialSnapshot` double for unit tests that don't exercise
/// dial telemetry — always returns a zeroed summary.
#[cfg(test)]
pub struct EmptyDialSnapshot;
#[cfg(test)]
impl DialSnapshot for EmptyDialSnapshot {
    fn dial_summary(&self) -> DialHealthSummary {
        DialHealthSummary::default()
    }
}

/// ZEB-380: per-relay health source for the snapshot. Mirrors `DialSnapshot`.
/// Returns the core `harmony_pkarr::RelayHealth`; `snapshot()` maps it to the
/// camelCase wire DTO.
pub trait RelaySnapshot: Send + Sync {
    fn relay_health(&self) -> Vec<harmony_pkarr::RelayHealth>;
}

/// Production source: reads the live `Arc<RelayClient>` retained in NodeState.
pub struct ProdRelaySnapshot(pub std::sync::Arc<harmony_pkarr::RelayClient>);
impl RelaySnapshot for ProdRelaySnapshot {
    fn relay_health(&self) -> Vec<harmony_pkarr::RelayHealth> {
        self.0.relay_health()
    }
}

#[cfg(test)]
pub struct EmptyRelaySnapshot;
#[cfg(test)]
impl RelaySnapshot for EmptyRelaySnapshot {
    fn relay_health(&self) -> Vec<harmony_pkarr::RelayHealth> {
        Vec::new()
    }
}

/// Spec §5.5: state coupling summary. NetworkHealthService owns the
/// rate-limiter task handle + cached last self-test report; the iroh /
/// resolver / pkarr handles come from AppState (already constructed).
pub struct NetworkHealthService {
    iroh: std::sync::Arc<dyn IrohSnapshot>,
    pkarr: std::sync::Arc<dyn PkarrSnapshot>,
    resolver: std::sync::Arc<dyn ReachabilitySnapshot>,
    membership: std::sync::Arc<dyn MyMembershipSet + Send + Sync>,
    /// ZEB-373: dynamic-dial telemetry source.
    dial: std::sync::Arc<dyn DialSnapshot>,
    /// ZEB-380: per-relay health source.
    relay: std::sync::Arc<dyn RelaySnapshot>,
    last_self_test: std::sync::Arc<tokio::sync::RwLock<Option<SelfTestReport>>>,
    /// Channel into the rate-limiter task. `None` until `spawn_rate_limiter`
    /// is called at boot; `notify()` is a no-op while None so unit tests
    /// that don't exercise event emission can construct the service freely.
    notify_tx: Option<tokio::sync::mpsc::UnboundedSender<()>>,
}

impl NetworkHealthService {
    pub fn new(
        iroh: std::sync::Arc<dyn IrohSnapshot>,
        pkarr: std::sync::Arc<dyn PkarrSnapshot>,
        resolver: std::sync::Arc<dyn ReachabilitySnapshot>,
        membership: std::sync::Arc<dyn MyMembershipSet + Send + Sync>,
        dial: std::sync::Arc<dyn DialSnapshot>,
        relay: std::sync::Arc<dyn RelaySnapshot>,
    ) -> Self {
        Self {
            iroh,
            pkarr,
            resolver,
            membership,
            dial,
            relay,
            last_self_test: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
            notify_tx: None,
        }
    }

    /// Spec §5.1: read from all sources, synthesize a snapshot. Never
    /// fails — empty/None fields render gracefully in the UI.
    pub async fn snapshot(&self) -> NetworkHealthSnapshot {
        let now = now_ms();
        // Build MyNetworkSummary with a placeholder reachability so we
        // can pass it through derive_reachability_status once peers are
        // known. The two-pass shape keeps derive_reachability_status'
        // signature peers-first without forcing the iroh read to know
        // about peers.
        let my_network = self
            .iroh
            .iroh_node_id_hex()
            .map(|node_id| MyNetworkSummary {
                iroh_node_id: node_id,
                reachability: ReachabilityStatus::Reachable, // patched below
                nat_classification: self.iroh.nat_classification(),
                home_relay_url: self.iroh.home_relay_url(),
                relay_rtt_ms: self.iroh.relay_rtt_ms(),
                direct_addresses: self.iroh.direct_addresses(),
            });

        let records = self.resolver.list_records();
        let peers = filter_peers_by_shared_membership(records, &*self.membership, now);

        // Patch reachability status now that we have peers.
        let my_network = my_network.map(|mut my| {
            my.reachability = derive_reachability_status(&my, &peers);
            my
        });

        NetworkHealthSnapshot {
            schema_version: 3,
            captured_at_ms: now,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            platform: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
            my_network,
            peers,
            pkarr_status: {
                let relays: Vec<RelayHealthWire> = self
                    .relay
                    .relay_health()
                    .into_iter()
                    .map(Into::into)
                    .collect();
                // Single atomic read so identity + community can't disagree
                // within one snapshot (ZEB-511).
                let publish = self.pkarr.publish_state();
                // ZEB-511: the publisher records no last-publish wall-clock, but
                // relay health does (last_success_ms, a *confirmed* PUT success).
                // Surface the most-recent confirmed success — but only while we
                // are actually publishing identity, so a community/friend PUT's
                // timestamp is never attributed to an identity that isn't being
                // published. A not-publishing node reports None (no stale
                // timestamp); the value is relay-derived only.
                let identity_last_publish_ms = if publish.identity_published {
                    relays.iter().filter_map(|r| r.last_success_ms).max()
                } else {
                    None
                };
                PkarrHealthSummary {
                    identity_published: publish.identity_published,
                    identity_last_publish_ms,
                    community_publish_count: publish.community_publish_count,
                    recent_fallback_events: self.pkarr.recent_fallback_events(),
                    relays,
                }
            },
            // ZEB-373 Task 5: real dynamic-dial telemetry, read from the
            // shared DialTelemetry via the DialSnapshot source.
            dial_status: self.dial.dial_summary(),
            // ZEB-450: a live service means transport is up — never disabled.
            // The disabled case has no service and goes through the IPC's
            // empty()-path stamp instead.
            transport_disabled_reason: None,
        }
    }

    /// Read the cached last self-test report (Task 5 + 6 populate this).
    #[allow(dead_code)]
    pub async fn cached_last_self_test(&self) -> Option<SelfTestReport> {
        self.last_self_test.read().await.clone()
    }
}

/// Spec §5.4: server-side redaction is the only path that emits
/// identifier prefixes. With `include_full_ids=false`, all owner
/// addresses + community ids + iroh node ids are reduced to 8-char
/// prefixes followed by `…`. Self-test section is fully omitted if
/// `last_report` is `None`. Schema version is always present.
pub fn format_export_markdown(
    snapshot: &NetworkHealthSnapshot,
    last_report: Option<&SelfTestReport>,
    include_full_ids: bool,
) -> String {
    let r = |s: &str| -> String {
        // Redaction off OR string already short enough → emit verbatim.
        // Otherwise truncate to 8-char prefix + ellipsis. Combined into
        // one branch to silence clippy::if_same_then_else.
        if include_full_ids || s.len() <= 8 {
            s.to_string()
        } else {
            format!("{}…", &s[..8])
        }
    };

    let mut out = String::new();
    use std::fmt::Write;

    let _ = writeln!(
        out,
        "## Harmony v{} ({})",
        snapshot.app_version, snapshot.platform
    );
    let _ = writeln!(out, "schemaVersion: {}", snapshot.schema_version);
    let _ = writeln!(out, "capturedAtMs: {}", snapshot.captured_at_ms);
    let _ = writeln!(out);

    let _ = writeln!(out, "## Network");
    match &snapshot.my_network {
        Some(my) => {
            let _ = writeln!(out, "irohNodeId: {}", r(&my.iroh_node_id));
            let _ = writeln!(out, "reachability: {:?}", my.reachability);
            let _ = writeln!(out, "nat: {:?}", my.nat_classification);
            if let Some(url) = &my.home_relay_url {
                let _ = writeln!(out, "homeRelayUrl: {}", url);
            }
            if let Some(rtt) = my.relay_rtt_ms {
                let _ = writeln!(out, "relayRttMs: {}", rtt);
            }
            if !my.direct_addresses.is_empty() {
                let _ = writeln!(out, "directAddresses: {}", my.direct_addresses.join(", "));
            }
        }
        None => {
            let _ = writeln!(out, "(iroh endpoint not yet bound)");
        }
    }
    let _ = writeln!(out);

    if let Some(report) = last_report {
        let _ = writeln!(out, "## Self-test");
        let _ = writeln!(out, "startedAtMs: {}", report.started_at_ms);
        let _ = writeln!(out, "finishedAtMs: {}", report.finished_at_ms);
        for step in &report.steps {
            let marker = match &step.outcome {
                StepOutcome::Pass { duration_ms } => format!("✓ ({}ms)", duration_ms),
                StepOutcome::Fail { reason } => format!("✗ {}", reason),
                StepOutcome::Skipped { reason } => format!("⊘ {}", reason),
            };
            let _ = writeln!(out, "{}: {}", step.name, marker);
        }
        if !report.peer_results.is_empty() {
            let _ = writeln!(out, "peerPings:");
            for pr in &report.peer_results {
                let marker = match &pr.outcome {
                    StepOutcome::Pass { duration_ms } => format!("✓ ({}ms)", duration_ms),
                    StepOutcome::Fail { reason } => format!("✗ {}", reason),
                    StepOutcome::Skipped { reason } => format!("⊘ {}", reason),
                };
                let mode = pr.mode.map(|m| format!(" [{:?}]", m)).unwrap_or_default();
                let _ = writeln!(out, "  {} {}{}", r(&pr.owner_addr), marker, mode);
            }
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "## Peers");
    if snapshot.peers.is_empty() {
        let _ = writeln!(out, "(no peers in shared communities)");
    } else {
        for p in &snapshot.peers {
            let mode_marker = match p.connection_mode {
                ConnectionMode::Direct => "direct",
                ConnectionMode::Relay => "relay",
                ConnectionMode::NoConnection => "none",
            };
            let rtt = p.rtt_ms.map(|v| format!(" {}ms", v)).unwrap_or_default();
            let age = p
                .reachability_record_age_ms
                .map(|ms| format!(" ({}s ago)", ms / 1000))
                .unwrap_or_default();
            let comms: Vec<String> = p.shared_communities.iter().map(|c| r(c)).collect();
            let _ = writeln!(
                out,
                "{} {}{}{} [{}]",
                r(&p.owner_addr),
                mode_marker,
                rtt,
                age,
                comms.join(",")
            );
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "## Discovery (pkarr)");
    let _ = writeln!(
        out,
        "identityPublished: {}",
        snapshot.pkarr_status.identity_published
    );
    if let Some(t) = snapshot.pkarr_status.identity_last_publish_ms {
        let _ = writeln!(out, "identityLastPublishMs: {}", t);
    }
    let _ = writeln!(
        out,
        "communityPublishCount: {}",
        snapshot.pkarr_status.community_publish_count
    );
    for hit in &snapshot.pkarr_status.recent_fallback_events {
        // Defense-in-depth: route through `r()` even though the field
        // names imply upstream pre-redaction. A future bug populating
        // these with full hex must not slip past the [0-9a-f]{32,}
        // regex guard exercised by the redaction tests.
        let _ = writeln!(
            out,
            "fallback {} in {} -> {}",
            r(&hit.peer_addr_short),
            r(&hit.community_id_short),
            if hit.hit { "hit" } else { "miss" }
        );
    }
    for relay in &snapshot.pkarr_status.relays {
        // Redact loopback/private/link-local relay hosts — public relays are
        // fine verbatim, but a shared export shouldn't leak a user's LAN relay.
        let display_url = match url::Url::parse(&relay.url) {
            Ok(u) if crate::pkarr_settings::is_local_host(u.host_str().unwrap_or("")) => {
                format!("{}://<local-relay>", u.scheme())
            }
            _ => relay.url.clone(),
        };
        let state = match &relay.state {
            RelayStateWire::Healthy => "healthy".to_string(),
            RelayStateWire::CoolingDown { until_ms } => format!("coolingDown(until={until_ms})"),
        };
        let last = match &relay.last_outcome {
            None => String::new(),
            Some(RelayOutcomeWire::Success) => " lastOutcome=success".to_string(),
            Some(RelayOutcomeWire::Timeout) => " lastOutcome=timeout".to_string(),
            Some(RelayOutcomeWire::Transport) => " lastOutcome=transport".to_string(),
            Some(RelayOutcomeWire::Http { status }) => format!(" lastOutcome=http:{status}"),
        };
        let _ = writeln!(out, "relay {} [{}]{}", display_url, state, last);
    }

    out
}

// ── Rate-limiter task + notify() API (spec §5.2) ────────────────────

const RATE_LIMIT_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// Event name emitted to the frontend when the rate-limiter fires.
pub const NETWORK_HEALTH_CHANGED_EVENT: &str = "network-health-changed";

/// Indirection over Tauri's `app_handle.emit(...)` so the rate-limiter
/// task can be tested without a real app. Production impl is a thin
/// wrapper around `tauri::AppHandle`.
pub trait NotifyEmitter: Send + Sync {
    fn emit_change(&self);
}

impl NetworkHealthService {
    /// Spawn the rate-limiter task and wire `self.notify_tx`. Call once
    /// at boot, AFTER iroh + resolver are constructed (spec §5.5).
    ///
    /// Idempotent: a second call replaces the channel + spawns a new
    /// task; the old task drains its channel and exits when its sender
    /// is dropped.
    pub fn spawn_rate_limiter<E: NotifyEmitter + Send + 'static>(&mut self, emitter: E) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        self.notify_tx = Some(tx);
        tokio::spawn(async move {
            let mut last_emit: Option<std::time::Instant> = None;
            while rx.recv().await.is_some() {
                // Drain any other queued notifies that arrived since the
                // last poll — we only need to know "something happened".
                while rx.try_recv().is_ok() {}
                let now = std::time::Instant::now();
                let due = last_emit
                    .map(|t| now.duration_since(t) >= RATE_LIMIT_WINDOW)
                    .unwrap_or(true);
                if due {
                    emitter.emit_change();
                    last_emit = Some(std::time::Instant::now());
                } else {
                    // Sleep until the window edge so subsequent notifies
                    // in this window collapse into one delayed emit.
                    let last = last_emit.expect("else branch implies last_emit is Some");
                    let remaining = RATE_LIMIT_WINDOW
                        .checked_sub(now.duration_since(last))
                        .unwrap_or_default();
                    tokio::time::sleep(remaining).await;
                    // Drain any notifies queued during the sleep.
                    while rx.try_recv().is_ok() {}
                    emitter.emit_change();
                    last_emit = Some(std::time::Instant::now());
                }
            }
        });
    }

    /// Send a notify into the rate-limiter. Safe to call from any
    /// task. No-op when the rate-limiter hasn't been spawned (e.g. in
    /// unit tests that don't exercise event emission).
    pub fn notify(&self) {
        if let Some(tx) = self.notify_tx.as_ref() {
            // Ignore send errors: the only way send fails is the receiver
            // dropped, which means the rate-limiter task exited. That's
            // a boot-shutdown race; nothing to do.
            let _ = tx.send(());
        }
    }
}

// ── HARMONY_PING_V1 accept-side handler + connect-side helper ───────

/// Spec §5.3 + §7.3: handle one inbound HARMONY_PING_V1 connection —
/// accept one bi-stream, read one byte, echo it back, finish.
/// Self-test only — produces no app-level state.
///
/// Dispatched from [`crate::zenoh_iroh_transport::IrohZenohLinkManager::spawn_accept_loop`]'s
/// ALPN switch (NOT a separate accept loop). The zenoh-over-iroh
/// accept loop owns the single consumer of `Endpoint::accept()` and
/// fans out by negotiated ALPN. This avoids the iroh 0.98 dual-loop
/// hazard: `Endpoint::accept()` is backed by a shared mutex-protected
/// queue, so two concurrent callers would round-robin and silently
/// consume each other's connections.
///
/// Takes an already-accepted [`iroh::endpoint::Connection`] because
/// the zenoh accept loop awaits `incoming` before matching on ALPN
/// (see `zenoh_iroh_transport.rs` line ~292).
pub async fn handle_ping_accept(conn: iroh::endpoint::Connection) {
    // The zenoh accept loop has already checked ALPN; this is defensive
    // in case `handle_ping_accept` is ever called from a different
    // dispatch path. iroh 0.98: Connection::alpn() returns &[u8].
    if conn.alpn() != crate::iroh_endpoint::alpn::HARMONY_PING_V1 {
        return;
    }
    // PR #161 R1 (Qodo Security): bound each await at 5s. Without
    // these, a hostile peer can open a HARMONY_PING_V1 connection
    // and never send a byte, pinning this spawned task until iroh's
    // idle timeout fires. Mirrors the `conn.closed()` bound below
    // and the production precedent in `iroh_invite_acceptor.rs`
    // (PR #159 F2/F4).
    let Ok(Ok((mut send, mut recv))) =
        tokio::time::timeout(std::time::Duration::from_secs(5), conn.accept_bi()).await
    else {
        return;
    };
    let mut buf = [0u8; 1];
    let Ok(Ok(())) =
        tokio::time::timeout(std::time::Duration::from_secs(5), recv.read_exact(&mut buf)).await
    else {
        return;
    };
    let Ok(Ok(())) =
        tokio::time::timeout(std::time::Duration::from_secs(5), send.write_all(&buf)).await
    else {
        return;
    };
    let _ = send.finish();
    // Hold the connection open until the client drives the close.
    // QUIC: `send.finish()` only marks the stream finished locally —
    // the echoed byte may still be in-flight. If we drop `conn` here
    // the server-side teardown can wipe the in-flight bytes and the
    // client's `read_exact` resolves with "connection lost".
    // Mirrors the close-handshake pattern documented in
    // `zenoh_iroh_link.rs::paired_stream_roundtrip` (ZEB-321 Task 5,
    // 2026-05-22: same race cost 6 hours of debug).
    //
    // Bounded at 5s: a peer that successfully reads the echo byte but
    // never tears the connection down (hostile, crashed, or
    // network-partitioned) must not pin this accept-task until iroh's
    // idle timeout fires. Mirrors the production precedent in
    // `iroh_invite_acceptor.rs` (PR #159 F2/F4 added the same bound
    // for exactly this reason).
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), conn.closed()).await;
}

/// Connect-side: open a HARMONY_PING_V1 bi-stream to `node_id`, write
/// one byte, read one byte echo, return RTT. Failure reasons are
/// bounded strings per spec §6.2.
pub async fn ping_peer(
    endpoint: &crate::iroh_endpoint::IrohEndpoint,
    node_id: iroh::EndpointId,
    timeout: std::time::Duration,
) -> Result<std::time::Duration, String> {
    // PR #161 R1 (CodeRabbit): spec §6.2 requires bounded canonical
    // reason strings on user-facing self-test output. Raw transport
    // error chains (`{e}`) leak internal addresses / cert details /
    // peer IDs into the exported diagnostic and the Network Health
    // panel. Use a fixed label per failure site; the underlying
    // error is still observable via tracing logs at the call sites.
    let start = std::time::Instant::now();
    let result = tokio::time::timeout(timeout, async {
        let conn = endpoint
            .inner()
            .connect(node_id, crate::iroh_endpoint::alpn::HARMONY_PING_V1)
            .await
            .map_err(|_| "connect failed".to_string())?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|_| "open_bi failed".to_string())?;
        send.write_all(&[0x42])
            .await
            .map_err(|_| "write_all failed".to_string())?;
        send.finish().map_err(|_| "finish failed".to_string())?;
        let mut buf = [0u8; 1];
        recv.read_exact(&mut buf)
            .await
            .map_err(|_| "read_exact failed".to_string())?;
        if buf[0] != 0x42 {
            return Err("unexpected echo byte".to_string());
        }
        Ok::<(), String>(())
    })
    .await;
    match result {
        Ok(Ok(())) => Ok(start.elapsed()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("timeout".to_string()),
    }
}

// ── Self-test traits + run_self_test (Task 6, spec §5.3) ───────────

/// Trait extension for self-test operations (spec §5.3). Production
/// impl lives in lib.rs boot wiring; tests use fakes.
pub trait IrohSelfTest: Send + Sync {
    /// True if the iroh endpoint is bound (Phase 1: any endpoint present).
    fn endpoint_bound(&self) -> bool;
    /// Round-trip reachability probe to the pkarr relay. Returns a
    /// `StepOutcome` directly so the probe owns its duration / reason.
    fn relay_round_trip(&self) -> futures::future::BoxFuture<'_, StepOutcome>;
}

/// Pkarr self-test surface. Production impl lives in lib.rs boot
/// wiring; tests use fakes.
pub trait PkarrSelfTest: Send + Sync {
    /// Read-only state-check (never publishes): is the identity publication
    /// active? Returns `Skipped` when discoverability is off (not a failure).
    /// `resolve_self` carries the real DHT round-trip that proves publish.
    fn publish_identity(&self) -> futures::future::BoxFuture<'_, StepOutcome>;
    /// Resolve own identity from pkarr and verify the returned record.
    fn resolve_self(&self) -> futures::future::BoxFuture<'_, StepOutcome>;
}

/// Trait for ping side. Production impl wraps `ping_peer` (Task 5);
/// tests substitute a fake that yields scripted results.
pub trait PingDispatcher: Send + Sync {
    /// Returns (RTT, mode) on success, error string on failure. Mode
    /// is approximate — implementer maps iroh connection-mode bytes
    /// to `ConnectionMode::{Direct,Relay}`.
    fn ping(
        &self,
        peer_node_id_bytes: [u8; 32],
        timeout: std::time::Duration,
    ) -> futures::future::BoxFuture<'_, Result<(std::time::Duration, ConnectionMode), String>>;
}

/// Per-peer ping wall-clock budget. Spec §5.3.
pub const PEER_PING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Semaphore cap on concurrent peer pings. Spec §5.3.
pub const PEER_PING_CONCURRENCY: usize = 32;

impl NetworkHealthService {
    /// Spec §5.3: self-test runs 4 ordered local steps + per-peer
    /// parallel pings (semaphore cap 32, 5s timeout each). Result is
    /// cached for `network_health_export_payload`.
    ///
    /// Per spec §6.2: step outcomes are Pass / Fail / Skipped. If an
    /// upstream step Fails, downstream steps are Skipped (not Failed)
    /// to avoid "4 things failed!" UI from one root cause.
    pub async fn run_self_test(
        &self,
        iroh_test: &dyn IrohSelfTest,
        pkarr_test: &dyn PkarrSelfTest,
        ping: &dyn PingDispatcher,
    ) -> SelfTestReport {
        let started_at_ms = now_ms();
        let mut steps = Vec::new();
        let mut peer_results = Vec::new();

        // Step 1: endpoint (binary precondition).
        let endpoint_ok = iroh_test.endpoint_bound();
        steps.push(SelfTestStep {
            name: "endpoint".into(),
            outcome: if endpoint_ok {
                StepOutcome::Pass { duration_ms: 0 }
            } else {
                StepOutcome::Fail {
                    reason: "endpoint not bound".into(),
                }
            },
        });

        // Step 2: relay (gated on endpoint). The probe owns its outcome.
        let relay_outcome = if endpoint_ok {
            iroh_test.relay_round_trip().await
        } else {
            StepOutcome::Skipped {
                reason: "skipped: endpoint not bound".into(),
            }
        };
        let relay_ok = matches!(relay_outcome, StepOutcome::Pass { .. });
        steps.push(SelfTestStep {
            name: "relay".into(),
            outcome: relay_outcome,
        });

        // Step 3: pkarr_publish (gated on relay). The probe may itself
        // return Skipped (e.g. discoverability off) — that gates resolve.
        let publish_outcome = if relay_ok {
            pkarr_test.publish_identity().await
        } else {
            // Accurate for every non-Pass relay outcome (Fail OR Skipped);
            // the root cause is already shown on the relay step itself.
            StepOutcome::Skipped {
                reason: "skipped: relay did not pass".into(),
            }
        };
        let publish_ok = matches!(publish_outcome, StepOutcome::Pass { .. });
        steps.push(SelfTestStep {
            name: "pkarr_publish".into(),
            outcome: publish_outcome,
        });

        // Step 4: pkarr_resolve (gated on publish) — the real round-trip.
        let resolve_outcome = if publish_ok {
            pkarr_test.resolve_self().await
        } else {
            StepOutcome::Skipped {
                reason: "skipped: publish not completed".into(),
            }
        };
        steps.push(SelfTestStep {
            name: "pkarr_resolve".into(),
            outcome: resolve_outcome,
        });

        // Per-peer pings: only attempt if endpoint is bound. Otherwise
        // all peer pings are Skipped.
        let records = self.resolver.list_records();
        let now = now_ms();
        let scoped = filter_peers_by_shared_membership(records, &*self.membership, now);
        if endpoint_ok {
            // Semaphore-bounded parallel ping. The ping dispatcher itself
            // is the production wiring extension point — for Phase 1 the
            // boot site passes a stub that emits Skipped, so the spawned
            // tasks borrow nothing from `ping` and the per-peer fanout
            // is iroh-free at unit-test time. Task 7's lib.rs boot
            // wiring replaces the stub with a resolver→iroh_node_id
            // lookup that calls `ping_peer`.
            //
            // NOTE: `ping` arg is currently unused on the unwired path;
            // it's kept in the signature so Task 7 can flip the body to
            // call `ping.ping(...)` without touching the trait. Suppress
            // the unused-warning here rather than at the function level.
            let _ = ping;
            let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(PEER_PING_CONCURRENCY));
            let mut handles = Vec::with_capacity(scoped.len());
            for peer in &scoped {
                // Permit acquired BEFORE spawn to provide N-of-32 spawn-rate
                // gating (back-pressure). If Task 7 reflexively moves this
                // into the spawned task to avoid the parent-loop wait, the
                // bound semantics break — all permits acquire immediately
                // and all tasks fan out concurrently.
                let permit = std::sync::Arc::clone(&semaphore)
                    .acquire_owned()
                    .await
                    .expect("semaphore not closed");
                let owner_addr = peer.owner_addr.clone();
                let mode_hint = peer.connection_mode;
                let last_seen = peer.last_seen_ms;
                // STAGE: Task 7's production NetworkHealthService
                // construction site replaces this stub closure with a
                // dispatcher.ping(...) call. For now: emit Skipped so the
                // peer-results vector is well-formed without iroh.
                handles.push(tokio::spawn(async move {
                    // TASK 7 NOTE: move this drop to AFTER the production
                    // `dispatcher.ping(...).await` — releasing the permit
                    // here in Phase 1 is safe ONLY because the stub does
                    // no work. Dropping early in production would defeat
                    // the semaphore cap and allow unbounded concurrency.
                    drop(permit);
                    PeerPingResult {
                        owner_addr,
                        outcome: StepOutcome::Skipped {
                            reason: format!(
                                "phase-1: dispatcher not wired (last_seen={:?}, hint={:?})",
                                last_seen, mode_hint
                            ),
                        },
                        mode: None,
                    }
                }));
            }
            for h in handles {
                if let Ok(r) = h.await {
                    peer_results.push(r);
                }
            }
        } else {
            for peer in &scoped {
                peer_results.push(PeerPingResult {
                    owner_addr: peer.owner_addr.clone(),
                    outcome: StepOutcome::Skipped {
                        reason: "skipped: endpoint not bound".into(),
                    },
                    mode: None,
                });
            }
        }

        let report = SelfTestReport {
            started_at_ms,
            finished_at_ms: now_ms(),
            steps,
            peer_results,
        };

        // Cache for export_payload (spec §5.4 / §5.5).
        *self.last_self_test.write().await = Some(report.clone());

        report
    }
}

/// Stub used during the wiring stage; replaced by the production
/// `PingDispatcher` built around `ping_peer` in lib.rs Task 7. Kept
/// `pub` so the production boot site can fall back to it before the
/// resolver is ready (and tests can construct it).
pub struct NullDispatcher;

impl PingDispatcher for NullDispatcher {
    fn ping(
        &self,
        _peer_node_id_bytes: [u8; 32],
        _timeout: std::time::Duration,
    ) -> futures::future::BoxFuture<'_, Result<(std::time::Duration, ConnectionMode), String>> {
        Box::pin(async { Err("dispatcher not wired".into()) })
    }
}

// ── ZEB-385: production self-test probes ────────────────────────────
//
// Built at IPC-call time from the locked `NodeState`; holds cheap
// `Arc`/copy handles. Both pkarr probes build a FRESH `PkarrResolver`
// from the relay client each call so the self-test reflects current
// reachability (no shared-cache hits / stale positives or negatives).
//
// `relay_round_trip` is declared on `IrohSelfTest` but probes the
// **pkarr** relay (the precondition the pkarr publish/resolve steps
// depend on): iroh 0.98 exposes no relay-RTT API, and iroh home-relay
// assignment is surfaced separately on the snapshot panel.

/// Fixed deterministic throwaway key for the relay-reachability probe.
/// A fresh resolver resolves it each run (empty cache), so a reachable
/// relay returns `Ok(None)` and the timed window is pure network RTT; the
/// determinism keeps the probe stable and excludes keygen-entropy jitter.
const RELAY_PROBE_SEED: [u8; 32] = [0x5a; 32];

/// Self-test-owned latency budget for each pkarr probe (mirrors
/// `PEER_PING_TIMEOUT`): a misbehaving relay must not hang "Run self-test".
const SELF_TEST_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub struct ProdSelfTest {
    pub iroh_endpoint: Option<std::sync::Arc<crate::iroh_endpoint::IrohEndpoint>>,
    pub pkarr_relay_client: Option<std::sync::Arc<harmony_pkarr::RelayClient>>,
    pub identity_pub_64: Option<[u8; 64]>,
    pub discoverable: bool,
    pub identity_publishing: bool,
}

impl IrohSelfTest for ProdSelfTest {
    fn endpoint_bound(&self) -> bool {
        // A present endpoint is a bound endpoint (node_id() is infallible).
        self.iroh_endpoint.is_some()
    }

    fn relay_round_trip(&self) -> futures::future::BoxFuture<'_, StepOutcome> {
        Box::pin(async move {
            let Some(relay) = self.pkarr_relay_client.as_ref() else {
                return StepOutcome::Fail {
                    reason: "pkarr relay client not initialized".into(),
                };
            };
            // Fresh resolver -> empty cache -> a real round-trip every run,
            // even with the fixed deterministic probe key (it is almost
            // certainly absent, so a reachable relay returns Ok(None); a
            // transport failure returns Err).
            //
            // The resolver wraps the LIVE RelayClient shared with the background
            // publisher: if every relay is on cooldown from a recent publish
            // failure, the GET short-circuits to Err without touching the
            // network. The resulting Fail is still accurate (the relay is
            // currently unavailable to this node), just with ~0ms duration.
            let resolver = harmony_pkarr::PkarrResolver::new(std::sync::Arc::clone(relay));
            let probe_vk = ed25519_dalek::SigningKey::from_bytes(&RELAY_PROBE_SEED).verifying_key();
            // Time only the network round-trip (keygen excluded), bounded by a
            // self-test-owned budget so a stalled relay can't hang the probe.
            let start = std::time::Instant::now();
            match tokio::time::timeout(SELF_TEST_PROBE_TIMEOUT, resolver.resolve(&probe_vk)).await {
                Ok(Ok(_)) => StepOutcome::Pass {
                    duration_ms: start.elapsed().as_millis() as u32,
                },
                Ok(Err(_)) => StepOutcome::Fail {
                    reason: "pkarr relay unreachable".into(),
                },
                Err(_) => StepOutcome::Fail {
                    reason: "pkarr relay timed out".into(),
                },
            }
        })
    }
}

impl PkarrSelfTest for ProdSelfTest {
    fn publish_identity(&self) -> futures::future::BoxFuture<'_, StepOutcome> {
        let discoverable = self.discoverable;
        let publishing = self.identity_publishing;
        Box::pin(async move {
            if !discoverable {
                StepOutcome::Skipped {
                    reason: "enable 'Make me discoverable' to test discovery".into(),
                }
            } else if publishing {
                StepOutcome::Pass { duration_ms: 0 }
            } else {
                StepOutcome::Fail {
                    reason: "identity publication not active".into(),
                }
            }
        })
    }

    fn resolve_self(&self) -> futures::future::BoxFuture<'_, StepOutcome> {
        Box::pin(async move {
            let Some(relay) = self.pkarr_relay_client.as_ref() else {
                return StepOutcome::Fail {
                    reason: "pkarr relay client not initialized".into(),
                };
            };
            let Some(id_pub) = self.identity_pub_64 else {
                return StepOutcome::Fail {
                    reason: "identity not loaded".into(),
                };
            };
            // Single clock sample for both key derivation and skew verification.
            let now_ms = now_ms();
            let verifying_keys: Vec<_> = harmony_pkarr::epoch_tolerance_window(now_ms)
                .iter()
                .map(|&epoch| {
                    harmony_pkarr::derive_ephemeral_key(
                        harmony_pkarr::PkarrCase::Identity,
                        &id_pub,
                        &epoch.to_be_bytes(),
                    )
                    .verifying_key()
                })
                .collect();
            let resolver = harmony_pkarr::PkarrResolver::new(std::sync::Arc::clone(relay));
            let start = std::time::Instant::now();
            match tokio::time::timeout(
                SELF_TEST_PROBE_TIMEOUT,
                resolver.resolve_window(&verifying_keys),
            )
            .await
            {
                Ok(Ok(Some(rec))) => {
                    if rec.verify_inner_sig().is_err()
                        || rec.verify_identity_match(&id_pub).is_err()
                        || rec.verify_freshness(now_ms).is_err()
                    {
                        StepOutcome::Fail {
                            reason: "resolved record failed verification".into(),
                        }
                    } else {
                        StepOutcome::Pass {
                            duration_ms: start.elapsed().as_millis() as u32,
                        }
                    }
                }
                Ok(Ok(None)) => StepOutcome::Fail {
                    reason: "identity not resolvable from pkarr".into(),
                },
                Ok(Err(_)) => StepOutcome::Fail {
                    reason: "pkarr resolve failed".into(),
                },
                Err(_) => StepOutcome::Fail {
                    reason: "pkarr resolve timed out".into(),
                },
            }
        })
    }
}

// ── Production trait impls (boot-wired in lib.rs) ───────────────────
//
// These adapters wire the synthesis-only traits above against the
// concrete sources (iroh Endpoint, ReachabilityResolver,
// PkarrPublisher, tauri::AppHandle). Lifetimes are short — each impl
// holds an `Arc`-shaped or `Clone`-cheap handle and is consulted on
// demand by `NetworkHealthService::snapshot`.

/// Production `IrohSnapshot` wrapping `Arc<IrohEndpoint>`.
pub struct ProdIrohSnapshot(pub std::sync::Arc<crate::iroh_endpoint::IrohEndpoint>);

impl IrohSnapshot for ProdIrohSnapshot {
    fn iroh_node_id_hex(&self) -> Option<String> {
        Some(hex::encode(self.0.node_id().as_bytes()))
    }
    fn home_relay_url(&self) -> Option<String> {
        self.0.home_relay().map(|r| r.to_string())
    }
    fn relay_rtt_ms(&self) -> Option<u32> {
        // Phase 1: iroh 0.98 does not expose a stable relay-RTT API
        // suitable for synthesis. Returning `None` keeps the snapshot
        // safe; testers still get `home_relay_url` to interpret.
        None
    }
    fn direct_addresses(&self) -> Vec<String> {
        self.0
            .direct_addresses()
            .into_iter()
            .map(|sa| sa.to_string())
            .collect()
    }
    fn nat_classification(&self) -> NatClass {
        // Phase 1: see `classify_nat` docs — no stable iroh hook yet.
        NatClass::Unknown
    }
}

/// Production `ReachabilitySnapshot` wrapping `ReachabilityResolver`
/// (cheap to clone — internally `Arc`-shared).
pub struct ProdReachabilitySnapshot(pub crate::reachability_resolver::ReachabilityResolver);

impl ReachabilitySnapshot for ProdReachabilitySnapshot {
    fn list_records(&self) -> Vec<ResolverPeerRecord> {
        self.0
            .list_active_peers()
            .into_iter()
            .map(|(owner, payload)| ResolverPeerRecord {
                owner_addr: owner.0,
                // Phase 1: no profile-cache lookup wired here. Follow-up
                // pulls display names out of the profile-broadcast cache.
                display_name: None,
                // Phase 1: no live iroh connection-mode inspection. The
                // field defaults to NoConnection so the UI shows the
                // peer without a misleading "Direct/Relay" badge.
                connection_mode: ConnectionMode::NoConnection,
                rtt_ms: None,
                last_seen_ms: Some(payload.announced_at_ms),
            })
            .collect()
    }
}

/// Production `NotifyEmitter` wrapping the mode-agnostic event sink
/// (ZEB-445; formerly `tauri::AppHandle`). The emit is fire-and-forget —
/// the rate-limiter task cannot meaningfully react to a closed window.
/// Tauri serialized `()` as JSON `null`, so `Value::Null` is wire-identical.
pub struct ProdNotifyEmitter(pub std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>);

impl NotifyEmitter for ProdNotifyEmitter {
    fn emit_change(&self) {
        self.0
            .emit(NETWORK_HEALTH_CHANGED_EVENT, serde_json::Value::Null);
    }
}

/// Production `PkarrSnapshot` wrapping the shared `PkarrPublisher`.
///
/// The `PkarrSnapshot` trait is synchronous (so it can fan through
/// `NetworkHealthService::snapshot` without imposing `async` recursion at
/// every call site), so this impl reads publish state via the publisher's
/// non-blocking [`try_active_handles`][harmony_pkarr::PkarrPublisher::try_active_handles]
/// accessor (ZEB-511). The registered handle set is the single source of
/// truth:
///
///   * `identity_published` — the fixed `"identity"` handle is registered
///     (case B; see `pkarr_identity_publisher::HANDLE`).
///   * `community_publish_count` — number of `"community:<hex>"` handles
///     registered (case C; see `pkarr_community_publisher`).
///
/// `try_active_handles()` returns `None` only during the sub-millisecond
/// window the background driver holds the state lock; that maps to the
/// conservative default (the driver never holds the lock across a network
/// PUT). `identity_last_publish_ms` is derived in the synthesis from the
/// confirmed relay `last_success_ms` — the publisher records no
/// last-publish wall-clock of its own.
pub struct ProdPkarrSnapshot {
    publisher: std::sync::Arc<harmony_pkarr::PkarrPublisher>,
}

impl ProdPkarrSnapshot {
    pub fn new(publisher: std::sync::Arc<harmony_pkarr::PkarrPublisher>) -> Self {
        Self { publisher }
    }
}

impl PkarrSnapshot for ProdPkarrSnapshot {
    fn publish_state(&self) -> PkarrPublishState {
        // Single non-blocking read of the publisher's registered handles
        // (ZEB-511), so identity + community fields always reflect the same
        // handle set. Case-B identity registers under the fixed "identity"
        // handle (pkarr_identity_publisher::HANDLE); case-C communities under
        // "community:<hex>" (pkarr_community_publisher). `None` (the sub-ms
        // driver lock window) maps both fields to the conservative default
        // together — never a contradictory pair.
        match self.publisher.try_active_handles() {
            Some(handles) => PkarrPublishState {
                identity_published: handles.iter().any(|k| k == "identity"),
                community_publish_count: handles
                    .iter()
                    .filter(|k| k.starts_with("community:"))
                    .count() as u32,
            },
            None => PkarrPublishState {
                identity_published: false,
                community_publish_count: 0,
            },
        }
    }
    fn recent_fallback_events(&self) -> Vec<PkarrFallbackHit> {
        // TODO(zeb-329-followup): wire a ring buffer of recent
        // `PkarrFallback` invocations (see `pkarr_resolver_adapter`).
        // Phase 1 returns an empty Vec — the panel hides the section.
        Vec::new()
    }
}

/// Production `MyMembershipSet`.
///
/// **Phase 1 status (per plan §Task 7 Step 2):** the implementer
/// search for a clean membership accessor (`grep -nE
/// "list_my_communities|membership.*list|joined_communities"
/// src-tauri/src/`) turned up no synchronous, non-NodeState-locking
/// accessor. The community membership lives behind the per-community
/// CRDT (`registry.engine_arc(cid).await.materialized(...)`), which
/// requires:
///
///   * an async hop into the community registry
///   * one async lock per community we know about
///   * a per-community materialisation pass to filter by
///     `MemberStatus::Joined`
///
/// Wiring that here would force `communities_shared_with` to grow an
/// async signature, which cascades into making `snapshot()` block on
/// per-peer async lookups inside its synthesis. That contradicts the
/// "synthesis only" design of `network_health` (top-of-file doc).
///
/// This is now satisfied by [`MembershipProjection`] — a synchronous
/// `SpaceId → joined-member set` cache fed off the `on_epoch_event`
/// hook in lib.rs boot wiring. `communities_shared_with` reads it under
/// a `std::sync::RwLock` with no `.await`, preserving the "synthesis
/// only" design.
///
/// A community is inserted ONLY while the local node is `Joined` in it
/// (the updater's gate), so every stored entry already implies local
/// membership — and `communities_shared_with(peer)` is exactly the set
/// of communities BOTH the local node and `peer` are `Joined` in.
#[derive(Clone, Default)]
pub struct MembershipProjection {
    inner: std::sync::Arc<
        std::sync::RwLock<
            std::collections::BTreeMap<
                crate::owner_state_types::SpaceId,
                std::collections::BTreeSet<crate::owner_state_types::OwnerAddr>,
            >,
        >,
    >,
}

impl MembershipProjection {
    pub fn new() -> Self {
        Self::default()
    }

    /// The local node IS `Joined` in `community`; replace its recorded
    /// joined-member set. Callers must drop any async lock guard before
    /// calling — writers never hold this guard across an `.await`.
    pub fn set_community_members(
        &self,
        community: crate::owner_state_types::SpaceId,
        joined: std::collections::BTreeSet<crate::owner_state_types::OwnerAddr>,
    ) {
        if let Ok(mut g) = self.inner.write() {
            g.insert(community, joined);
        }
    }

    /// The local node is NOT (or no longer) `Joined` in `community`;
    /// drop it entirely so no peer matches through it.
    pub fn remove_community(&self, community: &crate::owner_state_types::SpaceId) {
        if let Ok(mut g) = self.inner.write() {
            g.remove(community);
        }
    }

    /// Communities (lowercase hex, ascending) the local node shares
    /// with `peer`. Synchronous: no `.await` on the read path.
    pub fn communities_shared_with(&self, peer: &[u8; 16]) -> Vec<String> {
        let needle = crate::owner_state_types::OwnerAddr(*peer);
        let Ok(g) = self.inner.read() else {
            return Vec::new();
        };
        g.iter()
            .filter(|(_, members)| members.contains(&needle))
            .map(|(cid, _)| hex::encode(cid.0))
            .collect()
    }
}

/// Production membership lookup backed by [`MembershipProjection`].
pub struct ProdMembership {
    projection: MembershipProjection,
}

impl ProdMembership {
    pub fn new(projection: MembershipProjection) -> Self {
        Self { projection }
    }
}

impl MyMembershipSet for ProdMembership {
    fn communities_shared_with(&self, peer: &[u8; 16]) -> Vec<String> {
        self.projection.communities_shared_with(peer)
    }
}

/// IPC-internal accessor for the private `now_ms()` clock. Exposed via
/// `#[doc(hidden)]` so the Tauri command in `lib.rs` (which constructs
/// the synthetic Phase 1 `SelfTestReport`) can stamp its timestamps
/// from the same monotonic-wall-clock source as production snapshots.
#[doc(hidden)]
pub fn __now_ms_for_ipc() -> u64 {
    now_ms()
}

// ── Unit tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeMembership {
        // peer hex addr → list of shared community ids
        table: std::collections::HashMap<[u8; 16], Vec<String>>,
    }

    impl MyMembershipSet for FakeMembership {
        fn communities_shared_with(&self, peer: &[u8; 16]) -> Vec<String> {
            self.table.get(peer).cloned().unwrap_or_default()
        }
    }

    fn make_record(byte: u8, mode: ConnectionMode, last_seen: Option<u64>) -> ResolverPeerRecord {
        ResolverPeerRecord {
            owner_addr: [byte; 16],
            display_name: None,
            connection_mode: mode,
            rtt_ms: None,
            last_seen_ms: last_seen,
        }
    }

    fn sid(b: u8) -> crate::owner_state_types::SpaceId {
        crate::owner_state_types::SpaceId([b; 16])
    }
    fn oaddr(b: u8) -> crate::owner_state_types::OwnerAddr {
        crate::owner_state_types::OwnerAddr([b; 16])
    }

    #[test]
    fn membership_projection_lists_shared_community_for_member() {
        let proj = MembershipProjection::new();
        proj.set_community_members(sid(0xC1), [oaddr(0xAA), oaddr(0xBB)].into_iter().collect());
        assert_eq!(
            proj.communities_shared_with(&[0xAA; 16]),
            vec![hex::encode([0xC1u8; 16])]
        );
    }

    #[test]
    fn membership_projection_excludes_non_member_peer() {
        let proj = MembershipProjection::new();
        proj.set_community_members(sid(0xC1), [oaddr(0xAA)].into_iter().collect());
        assert!(proj.communities_shared_with(&[0xBB; 16]).is_empty());
    }

    #[test]
    fn membership_projection_empty_before_any_set() {
        let proj = MembershipProjection::new();
        assert!(proj.communities_shared_with(&[0xAA; 16]).is_empty());
    }

    #[test]
    fn membership_projection_remove_community_clears_match() {
        let proj = MembershipProjection::new();
        let community = sid(0xC1);
        proj.set_community_members(community, [oaddr(0xAA)].into_iter().collect());
        assert!(!proj.communities_shared_with(&[0xAA; 16]).is_empty());
        proj.remove_community(&community);
        assert!(proj.communities_shared_with(&[0xAA; 16]).is_empty());
    }

    #[test]
    fn membership_projection_returns_both_shared_communities_ascending() {
        let proj = MembershipProjection::new();
        // 0xC1 < 0xC2 → BTreeMap iteration yields them in ascending order.
        proj.set_community_members(sid(0xC2), [oaddr(0xAA)].into_iter().collect());
        proj.set_community_members(sid(0xC1), [oaddr(0xAA)].into_iter().collect());
        assert_eq!(
            proj.communities_shared_with(&[0xAA; 16]),
            vec![hex::encode([0xC1u8; 16]), hex::encode([0xC2u8; 16])]
        );
    }

    #[test]
    fn prod_membership_filter_keeps_only_shared_peers() {
        // Phase-A keystone, end-to-end through the real ProdMembership +
        // filter (not the FakeMembership): a hand-seeded projection makes
        // exactly the shared peer survive filter_peers_by_shared_membership,
        // proving the empty-peer-list blocker is lifted.
        let proj = MembershipProjection::new();
        proj.set_community_members(sid(0xC1), [oaddr(0xAA)].into_iter().collect());
        let membership = ProdMembership::new(proj);
        let records = vec![
            make_record(0xAA, ConnectionMode::NoConnection, Some(1_000)),
            make_record(0xBB, ConnectionMode::NoConnection, Some(1_000)),
        ];
        let kept = filter_peers_by_shared_membership(records, &membership, 2_000);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].owner_addr, hex::encode([0xAA; 16]));
        assert_eq!(kept[0].shared_communities, vec![hex::encode([0xC1u8; 16])]);
    }

    #[test]
    fn classify_nat_returns_unknown_for_any_input() {
        // Phase 1: classify_nat always returns Unknown until iroh
        // exposes a stable hook (TODO above).
        let dummy: u8 = 0;
        assert_eq!(classify_nat(&dummy), NatClass::Unknown);
    }

    #[test]
    fn derive_reachability_status_reachable_when_any_direct() {
        let my = MyNetworkSummary {
            iroh_node_id: "deadbeef".into(),
            reachability: ReachabilityStatus::Unreachable, // ignored
            nat_classification: NatClass::Unknown,
            home_relay_url: None,
            relay_rtt_ms: None,
            direct_addresses: vec![],
        };
        let peers = vec![PeerHealth {
            owner_addr: "abcd".into(),
            display_name: None,
            shared_communities: vec![],
            connection_mode: ConnectionMode::Direct,
            rtt_ms: None,
            last_seen_ms: None,
            reachability_record_age_ms: None,
        }];
        assert_eq!(
            derive_reachability_status(&my, &peers),
            ReachabilityStatus::Reachable
        );
    }

    #[test]
    fn derive_reachability_status_degraded_when_only_relay() {
        let my = MyNetworkSummary {
            iroh_node_id: "deadbeef".into(),
            reachability: ReachabilityStatus::Unreachable,
            nat_classification: NatClass::Unknown,
            home_relay_url: None,
            relay_rtt_ms: None,
            direct_addresses: vec![],
        };
        let peers = vec![PeerHealth {
            owner_addr: "abcd".into(),
            display_name: None,
            shared_communities: vec![],
            connection_mode: ConnectionMode::Relay,
            rtt_ms: None,
            last_seen_ms: None,
            reachability_record_age_ms: None,
        }];
        assert_eq!(
            derive_reachability_status(&my, &peers),
            ReachabilityStatus::Degraded
        );
    }

    #[test]
    fn derive_reachability_status_unreachable_when_all_no_connection() {
        let my = MyNetworkSummary {
            iroh_node_id: "deadbeef".into(),
            reachability: ReachabilityStatus::Reachable,
            nat_classification: NatClass::Unknown,
            home_relay_url: None,
            relay_rtt_ms: None,
            direct_addresses: vec![],
        };
        let peers = vec![PeerHealth {
            owner_addr: "abcd".into(),
            display_name: None,
            shared_communities: vec![],
            connection_mode: ConnectionMode::NoConnection,
            rtt_ms: None,
            last_seen_ms: None,
            reachability_record_age_ms: None,
        }];
        assert_eq!(
            derive_reachability_status(&my, &peers),
            ReachabilityStatus::Unreachable
        );
    }

    #[test]
    fn derive_reachability_status_reachable_when_no_peers_yet() {
        // Spec rationale: no peers known yet ≠ unreachable. We have
        // working endpoint state; peer reachability is just unknown.
        let my = MyNetworkSummary {
            iroh_node_id: "deadbeef".into(),
            reachability: ReachabilityStatus::Unreachable,
            nat_classification: NatClass::Unknown,
            home_relay_url: None,
            relay_rtt_ms: None,
            direct_addresses: vec![],
        };
        let peers: Vec<PeerHealth> = vec![];
        assert_eq!(
            derive_reachability_status(&my, &peers),
            ReachabilityStatus::Reachable
        );
    }

    #[test]
    fn filter_peers_empty_membership_yields_empty_list() {
        let records = vec![
            make_record(0x11, ConnectionMode::Direct, Some(1000)),
            make_record(0x22, ConnectionMode::Relay, Some(2000)),
        ];
        let memb = FakeMembership {
            table: std::collections::HashMap::new(),
        };
        let out = filter_peers_by_shared_membership(records, &memb, 5000);
        assert!(out.is_empty());
    }

    #[test]
    fn filter_peers_excludes_peers_with_no_shared_community() {
        let records = vec![
            make_record(0x11, ConnectionMode::Direct, Some(1000)),
            make_record(0x22, ConnectionMode::Relay, Some(2000)),
        ];
        let mut table = std::collections::HashMap::new();
        table.insert([0x11u8; 16], vec!["comm-a".to_string()]);
        // 0x22 has NO entry → excluded
        let memb = FakeMembership { table };
        let out = filter_peers_by_shared_membership(records, &memb, 5000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].owner_addr, hex::encode([0x11u8; 16]));
    }

    #[test]
    fn filter_peers_records_all_shared_communities() {
        let records = vec![make_record(0x11, ConnectionMode::Direct, Some(1000))];
        let mut table = std::collections::HashMap::new();
        table.insert(
            [0x11u8; 16],
            vec!["comm-a".to_string(), "comm-b".to_string()],
        );
        let memb = FakeMembership { table };
        let out = filter_peers_by_shared_membership(records, &memb, 5000);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].shared_communities,
            vec!["comm-a".to_string(), "comm-b".to_string()]
        );
    }

    #[test]
    fn filter_peers_sorts_by_last_seen_desc_none_last() {
        let records = vec![
            make_record(0x11, ConnectionMode::Direct, Some(1000)),
            make_record(0x22, ConnectionMode::Direct, Some(3000)),
            make_record(0x33, ConnectionMode::Direct, None),
            make_record(0x44, ConnectionMode::Direct, Some(2000)),
        ];
        let mut table = std::collections::HashMap::new();
        for b in [0x11, 0x22, 0x33, 0x44] {
            table.insert([b as u8; 16], vec!["c".to_string()]);
        }
        let memb = FakeMembership { table };
        let out = filter_peers_by_shared_membership(records, &memb, 10_000);
        assert_eq!(out.len(), 4);
        // Order: 3000, 2000, 1000, None
        assert_eq!(out[0].last_seen_ms, Some(3000));
        assert_eq!(out[1].last_seen_ms, Some(2000));
        assert_eq!(out[2].last_seen_ms, Some(1000));
        assert_eq!(out[3].last_seen_ms, None);
    }

    #[test]
    fn filter_peers_computes_record_age() {
        let records = vec![make_record(0x11, ConnectionMode::Direct, Some(1000))];
        let mut table = std::collections::HashMap::new();
        table.insert([0x11u8; 16], vec!["c".to_string()]);
        let memb = FakeMembership { table };
        let out = filter_peers_by_shared_membership(records, &memb, 5000);
        assert_eq!(out[0].reachability_record_age_ms, Some(4000));
    }

    #[test]
    fn network_health_snapshot_empty_is_well_formed() {
        let s = NetworkHealthSnapshot::empty();
        assert_eq!(s.schema_version, 3);
        assert!(s.my_network.is_none());
        assert!(s.peers.is_empty());
        assert_eq!(s.pkarr_status.community_publish_count, 0);
        assert!(s.pkarr_status.recent_fallback_events.is_empty());
        assert!(!s.app_version.is_empty());
        // ZEB-450: the empty snapshot must NOT spuriously claim transport is
        // disabled — `my_network: None` already covers "still starting up". A
        // reason is only ever set by the IPC stamp from NodeState.
        assert!(s.transport_disabled_reason.is_none());
    }

    #[test]
    fn zeb_450_stamp_transport_status_sets_overwrites_and_clears() {
        // None passes through (transport up / still booting).
        let stamped = stamp_transport_status(NetworkHealthSnapshot::empty(), None);
        assert!(stamped.transport_disabled_reason.is_none());

        // Some carries the actionable reason verbatim.
        let reason = "iroh transport unavailable this session: no keychain \
                      available and HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set";
        let stamped =
            stamp_transport_status(NetworkHealthSnapshot::empty(), Some(reason.to_string()));
        assert_eq!(stamped.transport_disabled_reason.as_deref(), Some(reason));

        // Overwrites any prior value (the constructor default must not stick).
        let mut seeded = NetworkHealthSnapshot::empty();
        seeded.transport_disabled_reason = Some("stale".to_string());
        let cleared = stamp_transport_status(seeded, None);
        assert!(cleared.transport_disabled_reason.is_none());
    }

    #[test]
    fn zeb_450_transport_disabled_reason_serializes_camel_case() {
        // Pins the wire contract the frontend type/banner depends on.
        let mut snap = NetworkHealthSnapshot::empty();
        snap.transport_disabled_reason = Some("transport off".to_string());
        let json = serde_json::to_string(&snap).expect("serialize");
        assert!(
            json.contains("\"transportDisabledReason\":\"transport off\""),
            "expected camelCase key in {json}"
        );
        // Round-trips, and a payload predating the field deserializes to None.
        let back: NetworkHealthSnapshot = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(
            back.transport_disabled_reason.as_deref(),
            Some("transport off")
        );
        let legacy = json.replace(",\"transportDisabledReason\":\"transport off\"", "");
        let legacy_snap: NetworkHealthSnapshot =
            serde_json::from_str(&legacy).expect("forward-compat deserialize");
        assert!(legacy_snap.transport_disabled_reason.is_none());
    }

    fn fixture_snapshot_with_full_ids() -> NetworkHealthSnapshot {
        NetworkHealthSnapshot {
            schema_version: 3,
            captured_at_ms: 1_700_000_000_000,
            app_version: "0.1.0-alpha.1".into(),
            platform: "darwin/aarch64".into(),
            my_network: Some(MyNetworkSummary {
                // 64 hex chars = a real Ed25519/iroh node id
                iroh_node_id: "a3f9e1c2".repeat(8),
                reachability: ReachabilityStatus::Reachable,
                nat_classification: NatClass::FullCone,
                home_relay_url: Some("https://use1.derp.iroh.network/".into()),
                relay_rtt_ms: Some(24),
                direct_addresses: vec!["192.0.2.1:11204".into()],
            }),
            peers: vec![PeerHealth {
                // 32-char lowercase hex owner addr
                owner_addr: "deadbeef".repeat(4),
                display_name: Some("alice".into()),
                shared_communities: vec!["beefcafe".repeat(4)],
                connection_mode: ConnectionMode::Direct,
                rtt_ms: Some(18),
                last_seen_ms: Some(1_700_000_000_000 - 3_000),
                reachability_record_age_ms: Some(3_000),
            }],
            pkarr_status: PkarrHealthSummary {
                identity_published: true,
                identity_last_publish_ms: Some(1_700_000_000_000 - 60_000),
                community_publish_count: 1,
                recent_fallback_events: vec![],
                relays: Vec::new(),
            },
            dial_status: DialHealthSummary::default(),
            transport_disabled_reason: None,
        }
    }

    #[test]
    fn format_export_redacted_leaks_no_full_ids() {
        let snap = fixture_snapshot_with_full_ids();
        let md = format_export_markdown(&snap, None, false);
        // Reject any 32+ lowercase hex run anywhere in the output.
        // 32 chars is the minimum length of an owner addr or community
        // id; 64 for iroh node id. Both should be redacted to 8-char
        // prefixes by the redacted formatter.
        let re = regex::Regex::new(r"[0-9a-f]{32,}").unwrap();
        if let Some(m) = re.find(&md) {
            panic!(
                "redacted export leaks full id at byte {}: {}\n--- full output ---\n{}",
                m.start(),
                m.as_str(),
                md
            );
        }
    }

    #[test]
    fn format_export_redacted_handles_full_hex_in_pkarr_fallback_fields() {
        // Greptile PR #161 R2 P1: defense-in-depth — if a future bug
        // populates pkarr fallback short-fields with full hex strings,
        // the format_export_markdown redactor must still strip them.
        let mut snap = fixture_snapshot_with_full_ids();
        snap.pkarr_status
            .recent_fallback_events
            .push(PkarrFallbackHit {
                peer_addr_short: "deadbeef".repeat(4),    // 32 chars hex
                community_id_short: "cafef00d".repeat(4), // 32 chars hex
                hit: true,
                captured_at_ms: 1_700_000_000_000,
            });
        let md = format_export_markdown(&snap, None, false);
        let re = regex::Regex::new(r"[0-9a-f]{32,}").unwrap();
        assert!(
            re.find(&md).is_none(),
            "redacted export leaked full hex from pkarr fallback fields: {}",
            md
        );
    }

    #[test]
    fn format_export_full_ids_includes_them() {
        let snap = fixture_snapshot_with_full_ids();
        let md = format_export_markdown(&snap, None, true);
        // Owner addr "deadbeef" * 4 = "deadbeefdeadbeefdeadbeefdeadbeef" (32 chars)
        assert!(
            md.contains("deadbeefdeadbeefdeadbeefdeadbeef"),
            "full owner addr must appear"
        );
        // iroh node id "a3f9e1c2" * 8 = 64 char hex
        assert!(
            md.contains(&"a3f9e1c2".repeat(8)),
            "full iroh node id must appear"
        );
    }

    #[test]
    fn format_export_omits_self_test_section_when_none() {
        let snap = fixture_snapshot_with_full_ids();
        let md = format_export_markdown(&snap, None, false);
        // No header for self-test, no boilerplate "not run"
        assert!(
            !md.contains("Self-test"),
            "no self-test header when report=None"
        );
        assert!(!md.contains("not run"), "no boilerplate placeholder");
    }

    #[test]
    fn format_export_includes_self_test_section_when_some() {
        let snap = fixture_snapshot_with_full_ids();
        let report = SelfTestReport {
            started_at_ms: 1_700_000_000_000,
            finished_at_ms: 1_700_000_001_500,
            steps: vec![
                SelfTestStep {
                    name: "endpoint".into(),
                    outcome: StepOutcome::Pass { duration_ms: 12 },
                },
                SelfTestStep {
                    name: "relay".into(),
                    outcome: StepOutcome::Pass { duration_ms: 24 },
                },
            ],
            peer_results: vec![],
        };
        let md = format_export_markdown(&snap, Some(&report), false);
        assert!(md.contains("Self-test"), "self-test header present");
        assert!(md.contains("endpoint"), "step name present");
    }

    #[test]
    fn format_export_empty_peer_list_emits_no_peers_line() {
        let mut snap = fixture_snapshot_with_full_ids();
        snap.peers.clear();
        let md = format_export_markdown(&snap, None, false);
        // The Peers section exists but the body is a single line, not
        // a header followed by empty content.
        assert!(
            md.contains("no peers"),
            "empty peer list emits 'no peers' line"
        );
    }

    #[test]
    fn format_export_includes_schema_version() {
        let snap = fixture_snapshot_with_full_ids();
        let md = format_export_markdown(&snap, None, false);
        // Tighten per code-review feedback: the previous loose match
        // accepted "1" anywhere in output, which would pass even if
        // schemaVersion were omitted (captured_at_ms contains "1").
        // Bind to the literal emitted token.
        assert!(
            md.contains("schemaVersion: 3"),
            "schema version token must appear verbatim; output was:\n{}",
            md
        );
    }

    #[test]
    fn format_export_redacts_local_relay_url() {
        // A user may configure a loopback or LAN relay; a shared export must
        // not leak the host — replace it with `scheme://<local-relay>`.
        let mut snap = fixture_snapshot_with_full_ids();
        snap.pkarr_status.relays = vec![
            RelayHealthWire {
                url: "http://192.168.1.5:6881".into(),
                state: RelayStateWire::Healthy,
                last_outcome: None,
                last_success_ms: None,
            },
            RelayHealthWire {
                url: "https://relay.pkarr.org".into(),
                state: RelayStateWire::Healthy,
                last_outcome: None,
                last_success_ms: None,
            },
        ];
        let md = format_export_markdown(&snap, None, false);
        // Local relay must be redacted.
        assert!(
            md.contains("http://<local-relay>"),
            "local relay host must be redacted; output:\n{md}"
        );
        assert!(
            !md.contains("192.168.1.5"),
            "raw private IP must not appear; output:\n{md}"
        );
        // Public relay must appear verbatim.
        assert!(
            md.contains("relay.pkarr.org"),
            "public relay must not be redacted; output:\n{md}"
        );
    }

    // ── Task 3: NetworkHealthService snapshot() tests ──────────────

    struct FakeIroh {
        ready: bool,
    }
    impl IrohSnapshot for FakeIroh {
        fn iroh_node_id_hex(&self) -> Option<String> {
            if self.ready {
                Some("a3f9e1c2".repeat(8))
            } else {
                None
            }
        }
        fn home_relay_url(&self) -> Option<String> {
            if self.ready {
                Some("https://derp.example/".into())
            } else {
                None
            }
        }
        fn relay_rtt_ms(&self) -> Option<u32> {
            if self.ready {
                Some(24)
            } else {
                None
            }
        }
        fn direct_addresses(&self) -> Vec<String> {
            if self.ready {
                vec!["192.0.2.1:11204".into()]
            } else {
                vec![]
            }
        }
        fn nat_classification(&self) -> NatClass {
            NatClass::Unknown
        }
    }

    struct FakePkarr;
    impl PkarrSnapshot for FakePkarr {
        fn publish_state(&self) -> PkarrPublishState {
            PkarrPublishState {
                identity_published: true,
                community_publish_count: 1,
            }
        }
        fn recent_fallback_events(&self) -> Vec<PkarrFallbackHit> {
            vec![]
        }
    }

    struct FakeResolver {
        records: Vec<ResolverPeerRecord>,
    }
    impl ReachabilitySnapshot for FakeResolver {
        fn list_records(&self) -> Vec<ResolverPeerRecord> {
            self.records.clone()
        }
    }

    fn empty_membership() -> std::sync::Arc<FakeMembership> {
        std::sync::Arc::new(FakeMembership {
            table: std::collections::HashMap::new(),
        })
    }

    #[tokio::test]
    async fn snapshot_with_iroh_not_ready_returns_my_network_none() {
        let svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: false }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        let snap = svc.snapshot().await;
        assert!(snap.my_network.is_none());
        assert!(snap.peers.is_empty());
        assert_eq!(snap.schema_version, 3);
    }

    #[tokio::test]
    async fn snapshot_with_iroh_ready_empty_resolver() {
        let svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        let snap = svc.snapshot().await;
        assert!(snap.my_network.is_some());
        assert_eq!(snap.peers, vec![]);
        assert_eq!(
            snap.my_network.as_ref().unwrap().home_relay_url,
            Some("https://derp.example/".into())
        );
    }

    #[tokio::test]
    async fn snapshot_with_three_peers_sorted_by_last_seen_desc() {
        let mut table = std::collections::HashMap::new();
        for b in [0x11u8, 0x22, 0x33] {
            table.insert([b; 16], vec!["c1".to_string()]);
        }
        let svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver {
                records: vec![
                    make_record(0x11, ConnectionMode::Direct, Some(1000)),
                    make_record(0x22, ConnectionMode::Direct, Some(3000)),
                    make_record(0x33, ConnectionMode::Direct, Some(2000)),
                ],
            }),
            std::sync::Arc::new(FakeMembership { table }),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        let snap = svc.snapshot().await;
        assert_eq!(snap.peers.len(), 3);
        assert_eq!(snap.peers[0].last_seen_ms, Some(3000));
        assert_eq!(snap.peers[1].last_seen_ms, Some(2000));
        assert_eq!(snap.peers[2].last_seen_ms, Some(1000));
        // With at least one Direct peer, reachability is Reachable.
        assert_eq!(
            snap.my_network.unwrap().reachability,
            ReachabilityStatus::Reachable
        );
    }

    // ── Rate-limiter tests (Task 4) ──────────────────────────────────

    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct CountingEmitter {
        n: std::sync::Arc<AtomicUsize>,
    }
    impl NotifyEmitter for CountingEmitter {
        fn emit_change(&self) {
            self.n.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn build_svc_with_rate_limiter() -> (NetworkHealthService, std::sync::Arc<AtomicUsize>) {
        let mut svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        let emitter = CountingEmitter { n: counter.clone() };
        svc.spawn_rate_limiter(emitter);
        (svc, counter)
    }

    #[tokio::test]
    async fn rate_limiter_collapses_30_rapid_notifies_to_one_emit() {
        let (svc, counter) = build_svc_with_rate_limiter();
        for _ in 0..30 {
            svc.notify();
        }
        // Wait past the rate-limit window plus a small grace.
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        let n = counter.load(Ordering::SeqCst);
        // The first notify emits immediately (last_emit was None); any
        // further notifies in the 2s window collapse into ONE delayed
        // emit at the window edge. Total = 2 (one immediate + one delayed).
        // If notifies stop after the burst, NO further emits fire.
        assert!(
            n == 1 || n == 2,
            "expected 1-2 emits for 30 rapid notifies, got {}",
            n
        );
    }

    #[tokio::test]
    async fn rate_limiter_no_emit_when_no_notifies() {
        let (_svc, counter) = build_svc_with_rate_limiter();
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rate_limiter_emits_every_window_when_continuously_notified() {
        // Notify once per 500ms for 5s (10 notifies); expect ~3 emits
        // (one per 2s window). Use a loose bound because tokio timer
        // resolution + test runner jitter make exact counts brittle.
        let (svc, counter) = build_svc_with_rate_limiter();
        for _ in 0..10 {
            svc.notify();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        let n = counter.load(Ordering::SeqCst);
        assert!(
            (2..=5).contains(&n),
            "expected 2-5 emits for 10 notifies spaced 500ms over 5s, got {}",
            n
        );
    }

    // ── Self-test tests (Task 6) ────────────────────────────────────

    use futures::FutureExt;

    struct ScriptedIrohTest {
        bound: bool,
        relay: StepOutcome,
    }
    impl IrohSelfTest for ScriptedIrohTest {
        fn endpoint_bound(&self) -> bool {
            self.bound
        }
        fn relay_round_trip(&self) -> futures::future::BoxFuture<'_, StepOutcome> {
            let r = self.relay.clone();
            async move { r }.boxed()
        }
    }

    struct ScriptedPkarrTest {
        publish: StepOutcome,
        resolve: StepOutcome,
    }
    impl PkarrSelfTest for ScriptedPkarrTest {
        fn publish_identity(&self) -> futures::future::BoxFuture<'_, StepOutcome> {
            let r = self.publish.clone();
            async move { r }.boxed()
        }
        fn resolve_self(&self) -> futures::future::BoxFuture<'_, StepOutcome> {
            let r = self.resolve.clone();
            async move { r }.boxed()
        }
    }

    fn build_svc_for_self_test() -> NetworkHealthService {
        NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        )
    }

    #[tokio::test]
    async fn self_test_all_pass_path() {
        let svc = build_svc_for_self_test();
        let iroh_t = ScriptedIrohTest {
            bound: true,
            relay: StepOutcome::Pass { duration_ms: 24 },
        };
        let pkarr_t = ScriptedPkarrTest {
            publish: StepOutcome::Pass { duration_ms: 380 },
            resolve: StepOutcome::Pass { duration_ms: 210 },
        };
        let report = svc.run_self_test(&iroh_t, &pkarr_t, &NullDispatcher).await;
        assert_eq!(report.steps.len(), 4);
        assert_eq!(report.steps[0].name, "endpoint");
        assert_eq!(report.steps[1].name, "relay");
        assert_eq!(report.steps[2].name, "pkarr_publish");
        assert_eq!(report.steps[3].name, "pkarr_resolve");
        assert!(
            matches!(report.steps[0].outcome, StepOutcome::Pass { .. }),
            "endpoint pass"
        );
        assert!(
            matches!(report.steps[1].outcome, StepOutcome::Pass { .. }),
            "relay pass"
        );
        assert!(
            matches!(report.steps[2].outcome, StepOutcome::Pass { .. }),
            "pkarr_publish pass"
        );
        assert!(
            matches!(report.steps[3].outcome, StepOutcome::Pass { .. }),
            "pkarr_resolve pass"
        );
    }

    #[tokio::test]
    async fn self_test_relay_fail_cascades_downstream_to_skipped() {
        let svc = build_svc_for_self_test();
        let iroh_t = ScriptedIrohTest {
            bound: true,
            relay: StepOutcome::Fail {
                reason: "relay timeout after 5s".into(),
            },
        };
        let pkarr_t = ScriptedPkarrTest {
            publish: StepOutcome::Pass { duration_ms: 380 },
            resolve: StepOutcome::Pass { duration_ms: 210 },
        };
        let report = svc.run_self_test(&iroh_t, &pkarr_t, &NullDispatcher).await;
        assert!(matches!(report.steps[0].outcome, StepOutcome::Pass { .. }));
        assert!(matches!(report.steps[1].outcome, StepOutcome::Fail { .. }));
        assert!(
            matches!(report.steps[2].outcome, StepOutcome::Skipped { .. }),
            "pkarr_publish skipped"
        );
        assert!(
            matches!(report.steps[3].outcome, StepOutcome::Skipped { .. }),
            "pkarr_resolve skipped"
        );
    }

    #[tokio::test]
    async fn self_test_endpoint_unbound_all_steps_skipped() {
        let svc = build_svc_for_self_test();
        let iroh_t = ScriptedIrohTest {
            bound: false,
            relay: StepOutcome::Pass { duration_ms: 0 },
        };
        let pkarr_t = ScriptedPkarrTest {
            publish: StepOutcome::Pass { duration_ms: 0 },
            resolve: StepOutcome::Pass { duration_ms: 0 },
        };
        let report = svc.run_self_test(&iroh_t, &pkarr_t, &NullDispatcher).await;
        assert!(
            matches!(report.steps[0].outcome, StepOutcome::Fail { .. }),
            "endpoint fail"
        );
        for i in 1..4 {
            assert!(
                matches!(report.steps[i].outcome, StepOutcome::Skipped { .. }),
                "step {} skipped",
                i
            );
        }
    }

    #[tokio::test]
    async fn self_test_pkarr_resolve_mismatch_reported_as_fail() {
        let svc = build_svc_for_self_test();
        let iroh_t = ScriptedIrohTest {
            bound: true,
            relay: StepOutcome::Pass { duration_ms: 24 },
        };
        let pkarr_t = ScriptedPkarrTest {
            publish: StepOutcome::Pass { duration_ms: 380 },
            resolve: StepOutcome::Fail {
                reason: "pkarr resolved unexpected payload".into(),
            },
        };
        let report = svc.run_self_test(&iroh_t, &pkarr_t, &NullDispatcher).await;
        match &report.steps[3].outcome {
            StepOutcome::Fail { reason } => assert_eq!(reason, "pkarr resolved unexpected payload"),
            other => panic!("expected Fail, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn self_test_result_is_cached_for_export() {
        let svc = build_svc_for_self_test();
        let iroh_t = ScriptedIrohTest {
            bound: true,
            relay: StepOutcome::Pass { duration_ms: 24 },
        };
        let pkarr_t = ScriptedPkarrTest {
            publish: StepOutcome::Pass { duration_ms: 380 },
            resolve: StepOutcome::Pass { duration_ms: 210 },
        };
        assert!(
            svc.cached_last_self_test().await.is_none(),
            "empty cache before run"
        );
        let _ = svc.run_self_test(&iroh_t, &pkarr_t, &NullDispatcher).await;
        let cached = svc.cached_last_self_test().await;
        assert!(cached.is_some(), "cache populated after run");
    }

    #[tokio::test]
    async fn self_test_publish_self_skip_cascades_resolve_to_skipped() {
        // Probe returns Skipped on publish (e.g. discoverability off); the
        // orchestrator must mark pkarr_resolve Skipped, NOT run it.
        let svc = build_svc_for_self_test();
        let iroh_t = ScriptedIrohTest {
            bound: true,
            relay: StepOutcome::Pass { duration_ms: 12 },
        };
        let pkarr_t = ScriptedPkarrTest {
            publish: StepOutcome::Skipped {
                reason: "enable 'Make me discoverable' to test discovery".into(),
            },
            resolve: StepOutcome::Pass { duration_ms: 99 },
        };
        let report = svc.run_self_test(&iroh_t, &pkarr_t, &NullDispatcher).await;
        assert!(matches!(report.steps[1].outcome, StepOutcome::Pass { .. }));
        assert!(
            matches!(report.steps[2].outcome, StepOutcome::Skipped { .. }),
            "publish self-skipped"
        );
        assert!(
            matches!(report.steps[3].outcome, StepOutcome::Skipped { .. }),
            "resolve skipped because publish did not pass"
        );
    }

    #[tokio::test]
    async fn self_test_relay_self_skip_cascades_downstream_to_skipped() {
        // The relay probe may itself return Skipped (not just Fail/Pass); the
        // orchestrator must gate publish + resolve to Skipped, not run them.
        let svc = build_svc_for_self_test();
        let iroh_t = ScriptedIrohTest {
            bound: true,
            relay: StepOutcome::Skipped {
                reason: "no relay configured".into(),
            },
        };
        let pkarr_t = ScriptedPkarrTest {
            publish: StepOutcome::Pass { duration_ms: 0 },
            resolve: StepOutcome::Pass { duration_ms: 0 },
        };
        let report = svc.run_self_test(&iroh_t, &pkarr_t, &NullDispatcher).await;
        assert!(matches!(report.steps[0].outcome, StepOutcome::Pass { .. }));
        assert!(matches!(
            report.steps[1].outcome,
            StepOutcome::Skipped { .. }
        ));
        assert!(
            matches!(report.steps[2].outcome, StepOutcome::Skipped { .. }),
            "publish skipped"
        );
        assert!(
            matches!(report.steps[3].outcome, StepOutcome::Skipped { .. }),
            "resolve skipped"
        );
    }

    // ── ZEB-385: ProdSelfTest probe tests (real RelayClient + mock relay) ──

    #[tokio::test]
    async fn prod_relay_round_trip_reachable_relay_passes() {
        use harmony_pkarr::{testing::MockPkarrRelay, RelayClient, RelayPool};
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = std::sync::Arc::new(RelayClient::new(pool));
        let probes = ProdSelfTest {
            iroh_endpoint: None,
            pkarr_relay_client: Some(client),
            identity_pub_64: None,
            discoverable: false,
            identity_publishing: false,
        };
        assert!(
            matches!(probes.relay_round_trip().await, StepOutcome::Pass { .. }),
            "reachable mock relay -> Pass"
        );
    }

    #[tokio::test]
    async fn prod_relay_round_trip_dead_relay_fails() {
        use harmony_pkarr::{RelayClient, RelayPool};
        // Port 1 is unbindable / unreachable; the GET round-trip errors.
        let pool = RelayPool::new(vec!["http://127.0.0.1:1".to_string()]);
        let client = std::sync::Arc::new(RelayClient::new(pool));
        let probes = ProdSelfTest {
            iroh_endpoint: None,
            pkarr_relay_client: Some(client),
            identity_pub_64: None,
            discoverable: false,
            identity_publishing: false,
        };
        assert!(
            matches!(probes.relay_round_trip().await, StepOutcome::Fail { .. }),
            "dead relay -> Fail"
        );
    }

    #[tokio::test]
    async fn prod_publish_identity_state_check_three_ways() {
        let mk = |discoverable, identity_publishing| ProdSelfTest {
            iroh_endpoint: None,
            pkarr_relay_client: None,
            identity_pub_64: None,
            discoverable,
            identity_publishing,
        };
        assert!(
            matches!(
                mk(false, false).publish_identity().await,
                StepOutcome::Skipped { .. }
            ),
            "not discoverable -> Skipped"
        );
        assert!(
            matches!(
                mk(true, true).publish_identity().await,
                StepOutcome::Pass { .. }
            ),
            "discoverable + registered -> Pass"
        );
        assert!(
            matches!(
                mk(true, false).publish_identity().await,
                StepOutcome::Fail { .. }
            ),
            "discoverable but not registered -> Fail"
        );
    }

    #[tokio::test]
    async fn prod_resolve_self_absent_identity_fails() {
        use harmony_pkarr::{testing::MockPkarrRelay, RelayClient, RelayPool};
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = std::sync::Arc::new(RelayClient::new(pool));
        let id_sk = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let mut id_pub = [0u8; 64];
        id_pub[32..].copy_from_slice(&id_sk.verifying_key().to_bytes());
        let probes = ProdSelfTest {
            iroh_endpoint: None,
            pkarr_relay_client: Some(client),
            identity_pub_64: Some(id_pub),
            discoverable: true,
            identity_publishing: true,
        };
        // Nothing published for this identity -> not resolvable -> Fail.
        assert!(matches!(
            probes.resolve_self().await,
            StepOutcome::Fail { .. }
        ));
    }

    #[tokio::test]
    async fn prod_resolve_self_finds_published_identity() {
        use harmony_pkarr::{
            current_epoch_id, derive_ephemeral_key, testing::MockPkarrRelay, EphemeralKeyBuilder,
            PkarrCase, PkarrPublisher, PkarrRoutingRecord, RecordBuilder, RelayClient, RelayPool,
        };
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = std::sync::Arc::new(RelayClient::new(pool));
        let publisher = std::sync::Arc::new(PkarrPublisher::new(std::sync::Arc::clone(&client)));
        let _ph = std::sync::Arc::clone(&publisher).spawn();

        let id_sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let mut id_pub = [0u8; 64];
        id_pub[32..].copy_from_slice(&id_sk.verifying_key().to_bytes());

        // Register the identity publication (mirrors PkarrIdentityPublisher::enable).
        let id_pub_for_key = id_pub;
        let key_builder: EphemeralKeyBuilder = std::sync::Arc::new(move |at_ms| {
            let epoch_id = current_epoch_id(at_ms);
            derive_ephemeral_key(
                PkarrCase::Identity,
                &id_pub_for_key,
                &epoch_id.to_be_bytes(),
            )
        });
        let id_sk2 = id_sk.clone();
        let builder: RecordBuilder = std::sync::Arc::new(move |at_ms| {
            PkarrRoutingRecord::sign_new(
                b"routing".to_vec(),
                id_pub,
                at_ms,
                at_ms + crate::reachability_record::REACHABILITY_RECORD_TTL_MS,
                &id_sk2,
            )
            .expect("sign")
        });
        publisher
            .register("identity".to_string(), key_builder, builder)
            .await;

        let probes = ProdSelfTest {
            iroh_endpoint: None,
            pkarr_relay_client: Some(std::sync::Arc::clone(&client)),
            identity_pub_64: Some(id_pub),
            discoverable: true,
            identity_publishing: true,
        };
        // resolve_self builds a FRESH resolver each call (no stale cache), so
        // polling works: wait for the background publish to land on the relay.
        let mut found = false;
        for _ in 0..40 {
            if matches!(probes.resolve_self().await, StepOutcome::Pass { .. }) {
                found = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(found, "published identity became resolvable -> Pass");
    }

    /// Build a `ProdPkarrSnapshot` whose publisher has the given handles
    /// registered. No driver is spawned — `register` inserts into the state
    /// map, which `try_active_handles` reads synchronously (ZEB-511).
    async fn prod_pkarr_with_handles(handles: &[&str]) -> ProdPkarrSnapshot {
        use harmony_pkarr::{
            current_epoch_id, derive_ephemeral_key, testing::MockPkarrRelay, EphemeralKeyBuilder,
            PkarrCase, PkarrPublisher, PkarrRoutingRecord, RecordBuilder, RelayClient, RelayPool,
        };
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = std::sync::Arc::new(RelayClient::new(pool));
        let publisher = std::sync::Arc::new(PkarrPublisher::new(client));
        let id_sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let mut id_pub = [0u8; 64];
        id_pub[32..].copy_from_slice(&id_sk.verifying_key().to_bytes());
        for h in handles {
            let id_pub_k = id_pub;
            let kb: EphemeralKeyBuilder = std::sync::Arc::new(move |at_ms| {
                derive_ephemeral_key(
                    PkarrCase::Identity,
                    &id_pub_k,
                    &current_epoch_id(at_ms).to_be_bytes(),
                )
            });
            let sk = id_sk.clone();
            let b: RecordBuilder = std::sync::Arc::new(move |at_ms| {
                PkarrRoutingRecord::sign_new(
                    b"x".to_vec(),
                    id_pub,
                    at_ms,
                    at_ms + crate::reachability_record::REACHABILITY_RECORD_TTL_MS,
                    &sk,
                )
                .expect("sign")
            });
            publisher.register((*h).to_string(), kb, b).await;
        }
        ProdPkarrSnapshot::new(publisher)
    }

    #[tokio::test]
    async fn prod_pkarr_identity_published_reflects_registered_handle() {
        let snap = prod_pkarr_with_handles(&["identity"]).await;
        let st = snap.publish_state();
        assert!(st.identity_published);
        assert_eq!(st.community_publish_count, 0);
    }

    #[tokio::test]
    async fn prod_pkarr_community_count_counts_community_handles() {
        let snap = prod_pkarr_with_handles(&["identity", "community:aa", "community:bb"]).await;
        let st = snap.publish_state();
        assert!(st.identity_published);
        assert_eq!(st.community_publish_count, 2);
    }

    #[tokio::test]
    async fn prod_pkarr_identity_unpublished_when_no_identity_handle() {
        let snap = prod_pkarr_with_handles(&["community:aa"]).await;
        let st = snap.publish_state();
        assert!(!st.identity_published);
        assert_eq!(st.community_publish_count, 1);
    }

    #[tokio::test]
    async fn prod_endpoint_bound_false_when_no_endpoint() {
        let probes = ProdSelfTest {
            iroh_endpoint: None,
            pkarr_relay_client: None,
            identity_pub_64: None,
            discoverable: false,
            identity_publishing: false,
        };
        assert!(!probes.endpoint_bound());
    }

    #[tokio::test]
    async fn prod_relay_and_resolve_fail_when_no_relay_client() {
        // Defensive None-guards: with no relay client, both pkarr round-trips
        // Fail (rather than panic) — covers the early-return branches.
        let probes = ProdSelfTest {
            iroh_endpoint: None,
            pkarr_relay_client: None,
            identity_pub_64: Some([0u8; 64]),
            discoverable: true,
            identity_publishing: true,
        };
        assert!(matches!(
            probes.relay_round_trip().await,
            StepOutcome::Fail { .. }
        ));
        assert!(matches!(
            probes.resolve_self().await,
            StepOutcome::Fail { .. }
        ));
    }

    // ── ZEB-373 Task 3: DialTelemetry tests ─────────────────────────

    #[test]
    fn dial_telemetry_counts_and_rings() {
        let t = DialTelemetry::new();
        t.record_attempt();
        t.record_succeeded([0x11; 32], [0xAA; 16]);
        t.record_attempt();
        t.record_failed([0x22; 32], [0xBB; 16]);
        t.record_skipped_duplicate();
        let s = t.summary();
        assert_eq!(s.attempts, 2);
        assert_eq!(s.succeeded, 1);
        assert_eq!(s.failed, 1);
        assert_eq!(s.skipped_duplicate, 1);
        assert_eq!(s.recent.len(), 2);
        assert!(s.recent.iter().any(|h| h.outcome == "succeeded"));
        assert!(s.recent.iter().any(|h| h.outcome == "failed"));
    }

    #[test]
    fn dial_ring_evicts_oldest_past_cap() {
        let t = DialTelemetry::new();
        // Push one more than the cap; the oldest must be evicted (FIFO).
        for i in 0..(DIAL_RING_CAP + 1) {
            let mut id = [0u8; 32];
            id[0] = i as u8;
            t.record_succeeded(id, [0xAA; 16]);
        }
        let s = t.summary();
        assert_eq!(s.recent.len(), DIAL_RING_CAP, "ring stays at cap");
        // The first entry (node_id_short of byte 0x00) was evicted; the newest
        // (byte = DIAL_RING_CAP) is present.
        let newest_short = hex::encode([DIAL_RING_CAP as u8, 0, 0, 0]);
        assert_eq!(
            s.recent.last().map(|h| h.node_id_short.clone()),
            Some(newest_short),
            "newest entry retained at the back"
        );
        assert!(
            s.recent
                .iter()
                .all(|h| h.node_id_short != hex::encode([0u8, 0, 0, 0])),
            "oldest entry evicted"
        );
        assert_eq!(
            s.succeeded,
            (DIAL_RING_CAP + 1) as u64,
            "counter not capped"
        );
    }

    #[test]
    fn empty_snapshot_has_zeroed_dial_status() {
        let snap = NetworkHealthSnapshot::empty();
        assert_eq!(snap.dial_status.attempts, 0);
        assert!(snap.dial_status.recent.is_empty());
    }

    // ── ZEB-380: RelaySnapshot tests ────────────────────────────────

    struct FakeRelaySnapshot(Vec<harmony_pkarr::RelayHealth>);
    impl RelaySnapshot for FakeRelaySnapshot {
        fn relay_health(&self) -> Vec<harmony_pkarr::RelayHealth> {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn snapshot_populates_relay_health() {
        let relay = harmony_pkarr::RelayHealth {
            url: "https://relay.pkarr.org".to_string(),
            state: harmony_pkarr::RelayState::CoolingDown { until_ms: 123 },
            last_outcome: Some(harmony_pkarr::RelayOutcome::Http(503)),
            last_success_ms: Some(42),
        };
        let svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(FakeRelaySnapshot(vec![relay.clone()])),
        );
        let snap = svc.snapshot().await;
        assert_eq!(snap.schema_version, 3);
        assert_eq!(snap.pkarr_status.relays.len(), 1);
        assert_eq!(snap.pkarr_status.relays[0].url, "https://relay.pkarr.org");
        assert_eq!(
            snap.pkarr_status.relays[0].state,
            RelayStateWire::CoolingDown { until_ms: 123 }
        );
        assert_eq!(
            snap.pkarr_status.relays[0].last_outcome,
            Some(RelayOutcomeWire::Http { status: 503 })
        );
    }

    // ── ZEB-511: identity_last_publish_ms derived from relay success ──

    /// Pkarr source with a configurable publish flag — the production impl
    /// derives `identity_last_publish_ms` from relay health, not the pkarr
    /// source, so this fake only needs to control `identity_published`.
    struct ConfigurablePkarr {
        identity_published: bool,
    }
    impl PkarrSnapshot for ConfigurablePkarr {
        fn publish_state(&self) -> PkarrPublishState {
            PkarrPublishState {
                identity_published: self.identity_published,
                community_publish_count: 0,
            }
        }
        fn recent_fallback_events(&self) -> Vec<PkarrFallbackHit> {
            vec![]
        }
    }

    fn relay_with_success(success_ms: Option<u64>) -> harmony_pkarr::RelayHealth {
        harmony_pkarr::RelayHealth {
            url: format!("https://r{}.example", success_ms.unwrap_or(0)),
            state: harmony_pkarr::RelayState::CoolingDown { until_ms: 0 },
            last_outcome: None,
            last_success_ms: success_ms,
        }
    }

    #[tokio::test]
    async fn snapshot_identity_last_publish_ms_from_relay_success_when_publishing() {
        // Publishing identity + the impl itself has no timestamp → the
        // synthesis surfaces the most-recent confirmed relay success.
        let svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(ConfigurablePkarr {
                identity_published: true,
            }),
            std::sync::Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(FakeRelaySnapshot(vec![
                relay_with_success(Some(1000)),
                relay_with_success(Some(3000)),
                relay_with_success(None),
            ])),
        );
        let snap = svc.snapshot().await;
        assert_eq!(snap.pkarr_status.identity_last_publish_ms, Some(3000));
    }

    #[tokio::test]
    async fn snapshot_identity_last_publish_ms_null_when_not_publishing() {
        // Not publishing identity → never attribute a relay success to it.
        let svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(ConfigurablePkarr {
                identity_published: false,
            }),
            std::sync::Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(FakeRelaySnapshot(vec![relay_with_success(Some(3000))])),
        );
        let snap = svc.snapshot().await;
        assert_eq!(snap.pkarr_status.identity_last_publish_ms, None);
    }

    #[test]
    fn relay_state_wire_cooling_down_serializes_camelcase_field() {
        // ZEB-384 regression guard: `rename_all` on the enum renames the
        // variant but NOT the struct-variant field, so the field must carry its
        // own `rename_all` to emit `untilMs`. The TS DTO reads `untilMs`; a
        // snake_case `until_ms` regression silently breaks the cooldown
        // countdown (renders `NaN`). Pin the serialized JSON so it can't return.
        let json = serde_json::to_value(RelayStateWire::CoolingDown { until_ms: 123 }).unwrap();
        assert_eq!(json["kind"], "coolingDown");
        assert_eq!(json["untilMs"], 123);
        assert!(
            json.get("until_ms").is_none(),
            "must not emit snake_case until_ms: {json}"
        );
    }

    #[test]
    fn relay_outcome_wire_http_serializes_camelcase() {
        // Sibling tagged enum audited in ZEB-384: `status` is single-word so it
        // is already correct, but pin the contract explicitly.
        let json = serde_json::to_value(RelayOutcomeWire::Http { status: 503 }).unwrap();
        assert_eq!(json["kind"], "http");
        assert_eq!(json["status"], 503);
    }
}
