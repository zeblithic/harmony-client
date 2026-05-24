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
}
