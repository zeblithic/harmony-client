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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PkarrFallbackHit {
    pub peer_addr_short: String,
    pub community_id_short: String,
    pub hit: bool,
    pub captured_at_ms: u64,
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
            schema_version: 1,
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
            },
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

/// Pkarr-side data the snapshot needs. Trait-extracted for testability;
/// production impl reads from `pkarr_publisher.active_handles()` + the
/// fallback ring buffer.
pub trait PkarrSnapshot: Send + Sync {
    fn identity_published(&self) -> bool;
    fn identity_last_publish_ms(&self) -> Option<u64>;
    fn community_publish_count(&self) -> u32;
    fn recent_fallback_events(&self) -> Vec<PkarrFallbackHit>;
}

/// Resolver-side data. Trait-extracted so the snapshot can be tested
/// without the full ReachabilityResolver. Production impl reads from
/// `ReachabilityResolver::list_active_peers()` + iroh-side
/// connection-mode lookups.
pub trait ReachabilitySnapshot: Send + Sync {
    fn list_records(&self) -> Vec<ResolverPeerRecord>;
}

/// Spec §5.5: state coupling summary. NetworkHealthService owns the
/// rate-limiter task handle + cached last self-test report; the iroh /
/// resolver / pkarr handles come from AppState (already constructed).
pub struct NetworkHealthService {
    iroh: std::sync::Arc<dyn IrohSnapshot>,
    pkarr: std::sync::Arc<dyn PkarrSnapshot>,
    resolver: std::sync::Arc<dyn ReachabilitySnapshot>,
    membership: std::sync::Arc<dyn MyMembershipSet + Send + Sync>,
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
    ) -> Self {
        Self {
            iroh,
            pkarr,
            resolver,
            membership,
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
            schema_version: 1,
            captured_at_ms: now,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            platform: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
            my_network,
            peers,
            pkarr_status: PkarrHealthSummary {
                identity_published: self.pkarr.identity_published(),
                identity_last_publish_ms: self.pkarr.identity_last_publish_ms(),
                community_publish_count: self.pkarr.community_publish_count(),
                recent_fallback_events: self.pkarr.recent_fallback_events(),
            },
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
        let _ = writeln!(
            out,
            "fallback {} in {} -> {}",
            hit.peer_addr_short,
            hit.community_id_short,
            if hit.hit { "hit" } else { "miss" }
        );
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
        assert_eq!(s.schema_version, 1);
        assert!(s.my_network.is_none());
        assert!(s.peers.is_empty());
        assert_eq!(s.pkarr_status.community_publish_count, 0);
        assert!(s.pkarr_status.recent_fallback_events.is_empty());
        assert!(!s.app_version.is_empty());
    }

    fn fixture_snapshot_with_full_ids() -> NetworkHealthSnapshot {
        NetworkHealthSnapshot {
            schema_version: 1,
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
            },
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
            md.contains("schemaVersion: 1"),
            "schema version token must appear verbatim; output was:\n{}",
            md
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
        fn identity_published(&self) -> bool {
            true
        }
        fn identity_last_publish_ms(&self) -> Option<u64> {
            Some(1_700_000_000_000)
        }
        fn community_publish_count(&self) -> u32 {
            1
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
        );
        let snap = svc.snapshot().await;
        assert!(snap.my_network.is_none());
        assert!(snap.peers.is_empty());
        assert_eq!(snap.schema_version, 1);
    }

    #[tokio::test]
    async fn snapshot_with_iroh_ready_empty_resolver() {
        let svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
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
}
