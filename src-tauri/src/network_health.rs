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

// ZEB-620 Task 6: the reconnect-supervisor's per-peer state projection feeds the
// dial-state counts and the PeerHealth last-seen fallback.
use crate::reconnect_supervisor::PeerStateWire;
// ZEB-622: the peer-liveness state machine's per-peer transport projection is
// joined into PeerHealth (live mode/rtt) and folds into the last-seen freshness.
use crate::peer_liveness::{LivenessMode, LivenessStateWire};

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
    /// ZEB-702 (Component D): butler-deposit accept/reject decision counts.
    /// `None` when this node runs no butler-deposit acceptor (no owner
    /// identity loaded — headless / owner-less), serialized as `null` per the
    /// `Option` convention above. Lets an always-rejecting butler (a sibling
    /// whose `friend_graph` never converged) be told apart from transport
    /// failure at the panel / e2e layer. `#[serde(default)]` keeps a pre-field
    /// snapshot forward-compatible.
    #[serde(default)]
    pub butler_deposits: Option<ButlerDepositHealth>,
    /// ZEB-710: drain-fence degraded-mode counters. `None` until the boot
    /// wiring installs the source (`#[serde(default)]` keeps a pre-field
    /// snapshot forward-compatible, matching `butler_deposits`).
    #[serde(default)]
    pub dm_fence: Option<DmFenceHealth>,
    /// ZEB-803: ZEB-458 community-relay serving/pulling health. `None` when this
    /// node runs no relay wiring at all (no owner identity, relay opt-in off),
    /// which is a different statement from "wired but serving nothing" — the
    /// latter is `Some` with zeroed counters, and is exactly the incident state.
    /// Conflating the two would hide the bug this field exists to surface.
    /// `#[serde(default)]` keeps a pre-field snapshot forward-compatible.
    ///
    /// Named `community_relay`, not `relay`, because `pkarr_status.relays`
    /// already means the pkarr relay pool — an unqualified `relay` on the same
    /// wire type would read as that.
    #[serde(default)]
    pub community_relay: Option<CommunityRelayHealth>,
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
    /// ZEB-623: set when the tunnel-v2 hello negotiation recorded this peer as
    /// protocol-incompatible (its `protocol_version` below our
    /// `MIN_SUPPORTED_TUNNEL_PROTOCOL_VERSION`); carries the human reason the
    /// panel shows in a loud badge. `None` when the peer is compatible. Additive
    /// wire field — `#[serde(default)]`, always serialized (present as `null`
    /// when `None`), so a pre-field cached snapshot still deserializes.
    #[serde(default)]
    pub protocol_incompat_reason: Option<String>,
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

/// ZEB-595: outcome of a single Case-C in-community pkarr fallback probe.
/// Three-state (not a bare bool) so the panel can distinguish a clean
/// "peer hasn't published a current record here" from a probe that could not
/// produce a trustworthy answer — the two must not be conflated during
/// incident triage (Qodo #377).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PkarrFallbackOutcome {
    /// A fresh, verified, decodable routing record was found.
    Hit,
    /// The relay answered cleanly but had no usable current record — `Ok(None)`,
    /// or a record that was stale/expired or for a different identity. A
    /// legitimate "not (currently) published here".
    Miss,
    /// The probe could not produce a trustworthy answer: a resolver/transport
    /// `Err`, or a record that was present but failed signature/identity
    /// verification or CBOR decode (corrupt/anomalous). Distinct from `Miss`.
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PkarrFallbackHit {
    pub peer_addr_short: String,
    pub community_id_short: String,
    pub outcome: PkarrFallbackOutcome,
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
    // Dial outcomes ("succeeded" | "failed") plus ZEB-620 supervisor
    // state-transition markers ("reconnected" | "retrying" | "dormant").
    pub outcome: String,
    pub captured_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct DialHealthSummary {
    pub attempts: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub skipped_duplicate: u64,
    // ZEB-620: live per-peer-state counts from the reconnect supervisor's
    // `states_snapshot` (folded in by `NetworkHealthService::snapshot`, not the
    // dial ring). `#[serde(default)]` keeps a pre-field snapshot forward-compatible.
    #[serde(default)]
    pub retrying: u32,
    #[serde(default)]
    pub dormant: u32,
    #[serde(default)]
    pub connected: u32,
    pub recent: Vec<DynamicDialHit>,
}

/// ZEB-702 (Component D): process-lifetime butler-deposit decision counts,
/// mirrored from the acceptor's `ButlerDepositStats`
/// (`iroh_butler_acceptor.rs`). Serde camelCase — the e2e assertions read the
/// exact `accepted` / `rejectedUnauthorized` / `rejectedOther` keys. Field
/// order matches the acceptor's `ButlerDepositCounts`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ButlerDepositHealth {
    pub accepted: u64,
    pub rejected_unauthorized: u64,
    pub rejected_other: u64,
}

/// ZEB-710: the ZEB-703 drain-fence's two degraded modes, mirrored from the
/// process-lived `dm_outbox::DM_FENCE_STATS`. Both were WARN-log-only; a
/// non-zero value here means either Phase C wedging (saturated fence made a
/// drain tick skip Phase C) or a stop that ran without the drain-path fence
/// (contended outbox lock). Serde camelCase like the sibling sections.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct DmFenceHealth {
    pub phase_c_saturated_skips: u64,
    pub stop_fence_skipped_contended: u64,
}

/// ZEB-803: the ZEB-458 community-relay path, both directions.
///
/// Motivated by a live incident: a relay acceptor served **zero pulls for 46
/// minutes** while the process stayed alive and logged normally, and separately
/// a channel message took **≥33 minutes** to reach a third node. Every existing
/// surface read green throughout. The relay path is how channel history reaches
/// a peer, so while it is down a node looks healthy from the inside and silent
/// from the outside — indistinguishable from "nobody is talking".
///
/// Both directions are carried because the two ends cannot see each other:
/// the incident needed a third node precisely because a serving fault and a
/// pulling fault present identically to the peer observing them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CommunityRelayHealth {
    /// Outbound: are we serving pulls to peers who relay through us?
    pub serving: CommunityRelayServingHealth,
    /// Inbound: are we successfully pulling our own held blobs from relays?
    pub pulling: CommunityRelayPullingHealth,
}

/// ZEB-803: acceptor side — the `harmony/community-relay-pull/v1` shell.
///
/// ## Reading this to tell a dead acceptor from an unreachable node
///
/// The incident left two hypotheses the logs could not separate: the acceptor
/// task stopped accepting, or inbound reachability changed so peers could no
/// longer reach a live acceptor. Both zero these counters, so this section
/// alone does not decide it — but the snapshot already carries the tiebreak.
///
/// The iroh accept loop is **shared** across ALPNs (butler, friend, invite,
/// pex, tunnel, relay). So compare against [`ButlerDepositHealth`]:
///
/// | relay counters | butler counters | reading |
/// | -- | -- | -- |
/// | flat | still moving | accept loop alive ⇒ **relay-specific fault** |
/// | flat | flat | nothing is arriving ⇒ **reachability** |
///
/// `rejected` and `failed` are the other half: a connection that arrives and
/// fails proves reachability was fine, which rules out hypothesis 2 outright.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CommunityRelayServingHealth {
    pub pulls_served: u64,
    pub pulls_rejected: u64,
    pub pulls_failed: u64,
    /// Wall ms of the most recent successfully served pull, any peer. `None`
    /// until this node serves its first. **This is the field the incident
    /// wanted**: cadence is ~7m30s per peer, so a value older than ~3 cadences
    /// while peers are believed connected is the signature.
    pub last_served_ms: Option<u64>,
    /// Per-peer, so one stuck peer is distinguishable from a dead acceptor.
    /// Sorted by `last_served_ms` desc. Bounded — see `COMMUNITY_RELAY_PEER_CAP`.
    pub peers: Vec<CommunityRelayPeerServed>,
}

/// ZEB-803: one peer's served-pull record. `peer_short` is 8 hex chars,
/// truncated **at the writer** per the ZEB-329 redaction invariant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommunityRelayPeerServed {
    pub peer_short: String,
    pub last_served_ms: u64,
    pub served_count: u64,
}

/// ZEB-803: puller side — [`crate::community_relay_pull_driver`].
///
/// ## Why `passes_run` matters more than the success counters
///
/// `passes_run` is a **liveness proof for the pull loop itself**, independent of
/// whether any relay answered. A driver whose task died and a driver finding
/// nothing to do are the same observation from the success counters alone; they
/// differ here. That is the receiver-side mirror of the acceptor tiebreak above.
///
/// ## The silent paths this exists to expose
///
/// `run_one_pass` had three ways to do nothing and say nothing:
///
/// 1. a failed pull session — logged at `debug!`, filtered out in production;
/// 2. a session that succeeded and ingested `0` blobs — logged nothing at all;
/// 3. a joined community whose resolver returned **no fresh relay** — never
///    entered the inner loop, so not even a debug line.
///
/// Path 3 is the dangerous one: it is indistinguishable from a healthy quiet
/// channel, and it is a leading candidate for the ≥33-minute delivery lag.
/// `passes_no_relay` counts it explicitly rather than leaving it inferable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CommunityRelayPullingHealth {
    /// Pull passes started. Climbs on the idle backstop even with zero joined
    /// communities, so a flat value means the loop is gone, not merely idle.
    pub passes_run: u64,
    pub last_pass_ms: Option<u64>,
    pub sessions_ok: u64,
    pub sessions_failed: u64,
    pub blobs_ingested: u64,
    /// Wall ms of the last pass that actually ingested at least one blob.
    pub last_ingest_ms: Option<u64>,
    /// Silent path 3: a joined community examined with no fresh relay available.
    /// Counted per (pass, community).
    pub passes_no_relay: u64,
    /// Bounded ring of recent per-relay session outcomes.
    pub recent: Vec<CommunityRelayPullHit>,
}

/// ZEB-803: one pull-session outcome. Short-form ids only (ZEB-329).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommunityRelayPullHit {
    pub community_short: String,
    pub relay_device_short: String,
    /// `"ok"` | `"failed"` | `"noRelay"`.
    pub outcome: String,
    /// Blobs ingested by this session (`0` for a failure or a no-op success).
    pub ingested: u32,
    pub captured_at_ms: u64,
}

/// ZEB-620: live per-peer-state tally derived from a supervisor
/// [`states_snapshot`](crate::reconnect_supervisor::SupervisorHandle::states_snapshot).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PeerStateCounts {
    pub retrying: u32,
    pub dormant: u32,
    pub connected: u32,
}

/// Tally a supervisor state snapshot by kind. Pure — unit-tested directly.
pub fn count_peer_states(states: &[([u8; 32], PeerStateWire)]) -> PeerStateCounts {
    let mut counts = PeerStateCounts::default();
    for (_peer, state) in states {
        match state {
            PeerStateWire::Connected { .. } => counts.connected += 1,
            PeerStateWire::Retrying { .. } => counts.retrying += 1,
            PeerStateWire::Dormant { .. } => counts.dormant += 1,
        }
    }
    counts
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
    /// ZEB-620: record a (re)connection established by a supervisor dial. Appends
    /// a `"reconnected"` marker to the recent ring WITHOUT touching the
    /// dial-outcome counters — those track dial results (`record_succeeded`
    /// already counted this dial); this marks the resulting state transition for
    /// the panel's recent-events feed.
    pub fn record_reconnected(&self, node_id: [u8; 32], owner: [u8; 16]) {
        self.push(node_id, owner, "reconnected");
    }
    /// ZEB-620: record a peer entering the retry ladder (ring marker only). The
    /// live retry COUNT the panel shows is derived from the supervisor state
    /// snapshot (`DialHealthSummary::retrying`), not this ring.
    pub fn record_retrying(&self, node_id: [u8; 32], owner: [u8; 16]) {
        self.push(node_id, owner, "retrying");
    }
    /// ZEB-620: record a peer going dormant (ring marker only — see
    /// [`record_retrying`](Self::record_retrying)).
    pub fn record_dormant(&self, node_id: [u8; 32], owner: [u8; 16]) {
        self.push(node_id, owner, "dormant");
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
            // Live per-peer-state counts are folded in by
            // `NetworkHealthService::snapshot` from the supervisor snapshot; the
            // telemetry ring itself knows nothing about current peer states.
            retrying: 0,
            dormant: 0,
            connected: 0,
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

const PKARR_FALLBACK_RING_CAP: usize = 32;

/// ZEB-595: process-lifetime bounded ring of recent Case-C in-community pkarr
/// fallback probe outcomes. Shared (`Arc`) between `PkarrResolverAdapter`
/// (writer) and `network_health_snapshot` via `ProdPkarrSnapshot` (reader) —
/// the exact mirror of `DialTelemetry` (ZEB-373).
///
/// One entry per (peer, community) probe: a single `resolve()` over N community
/// contexts records N entries (hit or miss each). Truncation to the 8-hex
/// `*_short` form happens here, at the writer, so the panel's short-only
/// redaction invariant (ZEB-329) is enforced where data enters — never relying
/// on a downstream caller to redact.
#[derive(Debug, Default)]
pub struct PkarrFallbackTelemetry {
    recent: Mutex<VecDeque<PkarrFallbackHit>>,
}

impl PkarrFallbackTelemetry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one community-context probe outcome. `peer`/`community` are the
    /// full 16-byte ids; only the first 4 bytes (8 hex chars) are retained.
    pub fn record(&self, peer: &[u8; 16], community: &[u8; 16], outcome: PkarrFallbackOutcome) {
        let entry = PkarrFallbackHit {
            peer_addr_short: hex::encode(&peer[..4]),
            community_id_short: hex::encode(&community[..4]),
            outcome,
            captured_at_ms: now_ms(),
        };
        let mut ring = self.recent.lock().expect("pkarr fallback ring lock");
        if ring.len() == PKARR_FALLBACK_RING_CAP {
            ring.pop_front();
        }
        ring.push_back(entry);
    }

    /// Snapshot of the ring, oldest-first.
    pub fn recent(&self) -> Vec<PkarrFallbackHit> {
        self.recent
            .lock()
            .expect("pkarr fallback ring lock")
            .iter()
            .cloned()
            .collect()
    }
}

const COMMUNITY_RELAY_PULL_RING_CAP: usize = 32;
/// Peers retained in [`CommunityRelayServingHealth::peers`]. Bounded because a public
/// relay serves an unbounded peer set; eviction is least-recently-served, so the
/// entry that matters during an incident (the one that stopped) is the last to
/// go, not the first.
const COMMUNITY_RELAY_PEER_CAP: usize = 64;

/// ZEB-803: process-lifetime relay-serving counters, shared (`Arc`) between
/// [`crate::iroh_community_relay_acceptor::IrohCommunityRelayPullAcceptor`]
/// (writer) and `network_health_snapshot` (reader). Mirrors [`DialTelemetry`].
#[derive(Debug, Default)]
pub struct CommunityRelayServingTelemetry {
    served: AtomicU64,
    rejected: AtomicU64,
    failed: AtomicU64,
    last_served_ms: AtomicU64,
    /// `peer_short → (last_served_ms, served_count)`.
    peers: Mutex<std::collections::HashMap<String, (u64, u64)>>,
}

impl CommunityRelayServingTelemetry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one successfully served pull. `peer` is the remote iroh node id;
    /// only the first 4 bytes (8 hex chars) are retained (ZEB-329).
    pub fn record_served(&self, peer: &[u8; 32]) {
        self.served.fetch_add(1, Ordering::Relaxed);
        let now = now_ms();
        self.last_served_ms.store(now, Ordering::Relaxed);
        let key = hex::encode(&peer[..4]);
        let mut peers = self.peers.lock().expect("relay serving peer map lock");
        let entry = peers.entry(key).or_insert((0, 0));
        entry.0 = now;
        entry.1 += 1;
        if peers.len() > COMMUNITY_RELAY_PEER_CAP {
            // Evict least-recently-served. Cheap at this cap and only on growth.
            if let Some(oldest) = peers
                .iter()
                .min_by_key(|(_, (last, _))| *last)
                .map(|(k, _)| k.clone())
            {
                peers.remove(&oldest);
            }
        }
    }

    pub fn record_rejected(&self) {
        self.rejected.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_failed(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn summary(&self) -> CommunityRelayServingHealth {
        let mut peers: Vec<CommunityRelayPeerServed> = self
            .peers
            .lock()
            .expect("relay serving peer map lock")
            .iter()
            .map(|(k, (last, count))| CommunityRelayPeerServed {
                peer_short: k.clone(),
                last_served_ms: *last,
                served_count: *count,
            })
            .collect();
        peers.sort_by(|a, b| {
            b.last_served_ms
                .cmp(&a.last_served_ms)
                .then_with(|| a.peer_short.cmp(&b.peer_short))
        });
        let last = self.last_served_ms.load(Ordering::Relaxed);
        CommunityRelayServingHealth {
            pulls_served: self.served.load(Ordering::Relaxed),
            pulls_rejected: self.rejected.load(Ordering::Relaxed),
            pulls_failed: self.failed.load(Ordering::Relaxed),
            // 0 is the "never served" sentinel: a real wall-clock stamp is never
            // 0, and `Option<AtomicU64>` does not exist. Mapped back to `None`
            // here so the wire type carries the honest absence rather than the
            // epoch, which a UI would render as 1970.
            last_served_ms: (last != 0).then_some(last),
            peers,
        }
    }
}

/// ZEB-803: process-lifetime relay-pulling counters, shared (`Arc`) between
/// [`crate::community_relay_pull_driver::CommunityRelayPullDriver`] (writer) and
/// `network_health_snapshot` (reader).
#[derive(Debug, Default)]
pub struct CommunityRelayPullTelemetry {
    passes_run: AtomicU64,
    last_pass_ms: AtomicU64,
    sessions_ok: AtomicU64,
    sessions_failed: AtomicU64,
    blobs_ingested: AtomicU64,
    last_ingest_ms: AtomicU64,
    passes_no_relay: AtomicU64,
    recent: Mutex<VecDeque<CommunityRelayPullHit>>,
}

impl CommunityRelayPullTelemetry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the START of a pull pass. Deliberately recorded before any work,
    /// and unconditionally — including when the node has joined no communities —
    /// because this counter's job is to prove the loop is alive, not that it
    /// found something to do.
    pub fn record_pass_start(&self) {
        self.passes_run.fetch_add(1, Ordering::Relaxed);
        self.last_pass_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// Silent path 3: a joined community with no fresh relay to pull from.
    pub fn record_no_relay(&self, community: &[u8; 16]) {
        self.passes_no_relay.fetch_add(1, Ordering::Relaxed);
        self.push(community, &[0u8; 16], "noRelay", 0);
    }

    /// A completed pull session. `ingested == 0` is a success, not a failure —
    /// it means the relay held nothing for us.
    pub fn record_session_ok(&self, community: &[u8; 16], relay_device: &[u8; 16], ingested: u32) {
        self.sessions_ok.fetch_add(1, Ordering::Relaxed);
        if ingested > 0 {
            self.blobs_ingested
                .fetch_add(u64::from(ingested), Ordering::Relaxed);
            self.last_ingest_ms.store(now_ms(), Ordering::Relaxed);
        }
        self.push(community, relay_device, "ok", ingested);
    }

    pub fn record_session_failed(&self, community: &[u8; 16], relay_device: &[u8; 16]) {
        self.sessions_failed.fetch_add(1, Ordering::Relaxed);
        self.push(community, relay_device, "failed", 0);
    }

    fn push(&self, community: &[u8; 16], relay_device: &[u8; 16], outcome: &str, ingested: u32) {
        let hit = CommunityRelayPullHit {
            community_short: hex::encode(&community[..4]),
            relay_device_short: hex::encode(&relay_device[..4]),
            outcome: outcome.to_string(),
            ingested,
            captured_at_ms: now_ms(),
        };
        let mut ring = self.recent.lock().expect("relay pull ring lock");
        if ring.len() == COMMUNITY_RELAY_PULL_RING_CAP {
            ring.pop_front();
        }
        ring.push_back(hit);
    }

    pub fn summary(&self) -> CommunityRelayPullingHealth {
        let last_pass = self.last_pass_ms.load(Ordering::Relaxed);
        let last_ingest = self.last_ingest_ms.load(Ordering::Relaxed);
        CommunityRelayPullingHealth {
            passes_run: self.passes_run.load(Ordering::Relaxed),
            // Same 0-as-never sentinel as `CommunityRelayServingTelemetry::summary`.
            last_pass_ms: (last_pass != 0).then_some(last_pass),
            sessions_ok: self.sessions_ok.load(Ordering::Relaxed),
            sessions_failed: self.sessions_failed.load(Ordering::Relaxed),
            blobs_ingested: self.blobs_ingested.load(Ordering::Relaxed),
            last_ingest_ms: (last_ingest != 0).then_some(last_ingest),
            passes_no_relay: self.passes_no_relay.load(Ordering::Relaxed),
            recent: self
                .recent
                .lock()
                .expect("relay pull ring lock")
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
    /// ZEB-622: the transport link is up but no selected path is known yet (a
    /// liveness `Degraded` state — e.g. an up-edge before the first path report,
    /// or a lost-path report on a still-live conn). Wire tag `"degraded"`.
    Degraded,
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
            schema_version: 4,
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
            // ZEB-702: no service ⇒ no acceptor to read counts from.
            butler_deposits: None,
            // ZEB-710: no service ⇒ no fence source installed.
            dm_fence: None,
            // ZEB-803: no service ⇒ no relay telemetry source wired.
            community_relay: None,
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

/// Spec §4.1 + ZEB-628: derive top-level reachability from my own state +
/// peer set. Reachable: at least one peer is Direct-connected (or no peers
/// yet — our own endpoint works; others' reachability is unknown, not
/// failing). Degraded: best peer signal is Relay OR liveness-Degraded (a
/// live link without a selected path is degraded-reachable, not down).
/// Unreachable: peers exist and every one is NoConnection. (`_my` presence
/// is enforced by the caller — this only runs inside `my_network.map(..)`.)
pub fn derive_reachability_status(
    _my: &MyNetworkSummary,
    peers: &[PeerHealth],
) -> ReachabilityStatus {
    if peers
        .iter()
        .any(|p| p.connection_mode == ConnectionMode::Direct)
    {
        ReachabilityStatus::Reachable
    } else if peers.iter().any(|p| {
        matches!(
            p.connection_mode,
            ConnectionMode::Relay | ConnectionMode::Degraded
        )
    }) {
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
    self_owner: Option<&[u8; 16]>,
    now_ms: u64,
) -> Vec<PeerHealth> {
    let mut out: Vec<PeerHealth> = Vec::new();
    for r in resolver_records {
        // ZEB-637: the node's own announce lands in its own resolver (the
        // membership consumer is self-blind by design) and the projection
        // "shares" every community with self — so without this skip the
        // snapshot grows a permanent self row at noConnection (no
        // connection source ever keys on self). Filter it here so every
        // peers[] consumer (panel, e2e asserts, GCE suite) sees peers only.
        if self_owner.is_some_and(|s| r.owner_addr == *s) {
            continue;
        }
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
            protocol_incompat_reason: r.protocol_incompat_reason,
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
    /// Iroh endpoint id from the peer's reachability announce — used by
    /// the self-test ping dispatcher to dial the peer. Zero if unknown.
    pub iroh_node_id: [u8; 32],
    pub display_name: Option<String>,
    pub connection_mode: ConnectionMode,
    pub rtt_ms: Option<u32>,
    pub last_seen_ms: Option<u64>,
    /// ZEB-623: the protocol-incompatibility reason recorded for this peer's
    /// iroh node id, joined from the `ProtocolCompatRegistry` by
    /// `NetworkHealthService::snapshot`. `None` for a compatible (or
    /// never-dialed) peer. Copied straight onto the emitted `PeerHealth`.
    pub protocol_incompat_reason: Option<String>,
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

/// Production source: reads the shared `DialTelemetry` written by the reconnect
/// supervisor's dials (`crate::reconnect_supervisor::run_reconnect_supervisor`).
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

/// ZEB-620 Task 6: source of the reconnect-supervisor's per-peer state snapshot.
/// Read once per network-health snapshot to (a) tally the live
/// `retrying`/`dormant`/`connected` counts and (b) back-fill a peer's
/// `last_seen_ms` from `Connected.since_ms` when its resolver record has none.
/// Mirrors the `DialSnapshot`/`PkarrSnapshot` source-trait pattern.
pub trait SupervisorSnapshot: Send + Sync {
    fn peer_states(&self) -> Vec<([u8; 32], PeerStateWire)>;
}

/// Production source: reads the live [`SupervisorHandle`] the resolver holds
/// (installed at boot by `event_loop`'s `set_supervisor`). Lazy — the handle is
/// `None` until the supervisor spawns, so a pre-boot snapshot reports empty
/// states (zero counts, no fallback). The resolver is a cheap `Arc`-backed
/// clone; all clones share the same handle cell.
///
/// [`SupervisorHandle`]: crate::reconnect_supervisor::SupervisorHandle
pub struct ProdSupervisorSnapshot {
    resolver: crate::reachability_resolver::ReachabilityResolver,
}
impl ProdSupervisorSnapshot {
    pub fn new(resolver: crate::reachability_resolver::ReachabilityResolver) -> Self {
        Self { resolver }
    }
}
impl SupervisorSnapshot for ProdSupervisorSnapshot {
    fn peer_states(&self) -> Vec<([u8; 32], PeerStateWire)> {
        self.resolver
            .supervisor()
            .map(|h| h.states_snapshot())
            .unwrap_or_default()
    }
}

/// ZEB-622: source of the peer-liveness state machine's per-peer transport
/// projection, read once per network-health snapshot to (a) join live
/// `connection_mode`/`rtt_ms` onto each `PeerHealth` (by `iroh_node_id`) and
/// (b) fold `Connected.since_ms` into the peer's `last_seen_ms` freshness, plus
/// (c) supply `MyNetworkSummary.relay_rtt_ms` when iroh exposes none. Mirrors
/// the [`SupervisorSnapshot`] source-trait pattern.
pub trait LivenessSnapshot: Send + Sync {
    fn peer_states(&self) -> Vec<([u8; 32], LivenessStateWire)>;
    fn min_relay_rtt_ms(&self) -> Option<u32>;
}

/// Production source: reads the live [`LivenessHandle`] the resolver holds
/// (installed at boot by `event_loop`'s `set_liveness`, before the supervisor
/// block). Lazy — the handle is `None` until the liveness machine is wired, so a
/// pre-boot snapshot reports empty states + no relay RTT. The resolver is a
/// cheap `Arc`-backed clone; all clones share the same handle cell — the exact
/// [`ProdSupervisorSnapshot`] pattern.
///
/// [`LivenessHandle`]: crate::peer_liveness::LivenessHandle
pub struct ProdLivenessSnapshot {
    resolver: crate::reachability_resolver::ReachabilityResolver,
}
impl ProdLivenessSnapshot {
    pub fn new(resolver: crate::reachability_resolver::ReachabilityResolver) -> Self {
        Self { resolver }
    }
}
impl LivenessSnapshot for ProdLivenessSnapshot {
    fn peer_states(&self) -> Vec<([u8; 32], LivenessStateWire)> {
        self.resolver
            .liveness()
            .map(|h| h.states_snapshot())
            .unwrap_or_default()
    }
    fn min_relay_rtt_ms(&self) -> Option<u32> {
        self.resolver.liveness().and_then(|h| h.min_relay_rtt_ms())
    }
}

/// ZEB-622: presence-beacon last-seen cache. Fed by `CommunityPresenceMap::apply`
/// on EVERY verified, member-gated beacon that reaches it (even a stale/duplicate
/// refresh that leaves the roster unchanged — still fresh evidence we just heard
/// from that owner), and read by [`NetworkHealthService::snapshot`] to max-merge
/// each peer's `last_seen_ms`. Keyed by owner addr (`[u8; 16]`); values are
/// wall-clock ms. A `std::sync::RwLock` (not the presence map's `tokio::Mutex`)
/// so the synchronous snapshot read path stays `.await`-free.
#[derive(Debug, Default)]
pub struct PresenceLastSeenCache {
    inner: std::sync::RwLock<std::collections::HashMap<[u8; 16], u64>>,
}

impl PresenceLastSeenCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `owner`'s presence beacon was observed at `last_seen_ms`.
    /// Max-merge: a stale (lower) timestamp never regresses a fresher recorded
    /// one. A poisoned lock is RECOVERED rather than treated as a no-op (the
    /// critical section is a panic-free map op) — matches `MembershipProjection`'s
    /// ZEB-495 recovery.
    pub fn note_seen(&self, owner: [u8; 16], last_seen_ms: u64) {
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let slot = g.entry(owner).or_insert(last_seen_ms);
        *slot = (*slot).max(last_seen_ms);
    }

    /// Freshest recorded presence-beacon wall-clock for `owner`, if any.
    /// Poisoned lock recovered (see [`note_seen`](Self::note_seen)).
    pub fn last_seen(&self, owner: &[u8; 16]) -> Option<u64> {
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        g.get(owner).copied()
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
    /// ZEB-620: reconnect-supervisor state source. `None` in unit tests that
    /// don't exercise dial-state telemetry (then `peer_states()` reads empty).
    /// Installed at boot via [`set_supervisor_source`](Self::set_supervisor_source),
    /// mirroring how `notify_tx` is wired by `spawn_rate_limiter`.
    supervisor: Option<std::sync::Arc<dyn SupervisorSnapshot>>,
    /// ZEB-622: peer-liveness state source. `None` in unit tests that don't
    /// exercise transport telemetry (then the liveness join + relay-RTT fallback
    /// are inert). Installed at boot via
    /// [`set_liveness_source`](Self::set_liveness_source).
    liveness: Option<std::sync::Arc<dyn LivenessSnapshot>>,
    /// ZEB-622: presence last-seen cache. `None` in unit tests that don't exercise
    /// presence freshness. Installed at boot via
    /// [`set_presence_source`](Self::set_presence_source).
    presence: Option<std::sync::Arc<PresenceLastSeenCache>>,
    /// ZEB-623: per-peer protocol-compatibility registry, shared with the
    /// TunnelManager that writes it from the tunnel-v2 hello negotiation. Reads
    /// only; defaults to an empty registry (inert — every lookup is `None`) so
    /// unit tests and pre-boot construction need no wiring. Installed at boot via
    /// [`set_protocol_compat_source`](Self::set_protocol_compat_source),
    /// mirroring the liveness/presence handles.
    protocol_compat: std::sync::Arc<crate::protocol_versioning::ProtocolCompatRegistry>,
    last_self_test: std::sync::Arc<tokio::sync::RwLock<Option<SelfTestReport>>>,
    /// Channel into the rate-limiter task. `None` until `spawn_rate_limiter`
    /// is called at boot; `notify()` is a no-op while None so unit tests
    /// that don't exercise event emission can construct the service freely.
    notify_tx: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    /// ZEB-637: the local node's own OwnerAddr, installed at boot when an
    /// identity is loaded. `None` in unit tests and pre-identity boot — then
    /// no self-row filtering happens (additive like the other `set_*`
    /// sources). Consumed by `snapshot` to drop the self row from `peers[]`.
    self_owner: Option<[u8; 16]>,
    /// ZEB-702 (Component D): butler-deposit decision-counter source — the SAME
    /// `Arc` the acceptor shell increments (`iroh_butler_acceptor.rs`). `None`
    /// on nodes with no acceptor installed (no owner identity); then
    /// `snapshot().butler_deposits` is `None`. Installed at boot via
    /// [`set_butler_deposit_source`](Self::set_butler_deposit_source),
    /// additive like the other `set_*` sources.
    butler_deposits: Option<std::sync::Arc<crate::iroh_butler_acceptor::ButlerDepositStats>>,
    /// ZEB-710: drain-fence degraded-mode counter source — the process-lived
    /// `dm_outbox::DM_FENCE_STATS` Arc. Installed at boot via
    /// [`set_dm_fence_source`](Self::set_dm_fence_source), additive like the
    /// other `set_*` sources.
    dm_fence: Option<std::sync::Arc<crate::dm_outbox::DmFenceStats>>,
    /// ZEB-803: community-relay ACCEPTOR telemetry — the SAME `Arc` the pull
    /// shell increments. `None` on nodes with no relay acceptor installed.
    /// Installed at boot via
    /// [`set_community_relay_serving_source`](Self::set_community_relay_serving_source).
    community_relay_serving: Option<std::sync::Arc<CommunityRelayServingTelemetry>>,
    /// ZEB-803: community-relay PULL-DRIVER telemetry — the SAME `Arc` the
    /// driver loop increments. Held separately from the serving source because
    /// a node can run one without the other, and the incident turned on being
    /// able to say which side was dark. Installed at boot via
    /// [`set_community_relay_pull_source`](Self::set_community_relay_pull_source).
    community_relay_pulling: Option<std::sync::Arc<CommunityRelayPullTelemetry>>,
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
            supervisor: None,
            liveness: None,
            presence: None,
            protocol_compat: std::sync::Arc::new(
                crate::protocol_versioning::ProtocolCompatRegistry::default(),
            ),
            last_self_test: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
            notify_tx: None,
            self_owner: None,
            butler_deposits: None,
            dm_fence: None,
            community_relay_serving: None,
            community_relay_pulling: None,
        }
    }

    /// ZEB-620 Task 6: install the reconnect-supervisor state source. Called once
    /// at boot after the supervisor's handle reaches the resolver. Additive —
    /// when unset, dial-state counts read zero and the PeerHealth last-seen
    /// fallback is inert (existing behavior).
    pub fn set_supervisor_source(&mut self, src: std::sync::Arc<dyn SupervisorSnapshot>) {
        self.supervisor = Some(src);
    }

    /// ZEB-622: install the peer-liveness state source. Called once at boot after
    /// the liveness handle reaches the resolver (event_loop). Additive — when
    /// unset, the liveness join is a no-op and `relay_rtt_ms` keeps the iroh
    /// value (existing behavior).
    pub fn set_liveness_source(&mut self, src: std::sync::Arc<dyn LivenessSnapshot>) {
        self.liveness = Some(src);
    }

    /// ZEB-622: install the presence last-seen cache. Called once at boot. When
    /// unset, the presence contribution to `last_seen_ms` is inert.
    pub fn set_presence_source(&mut self, src: std::sync::Arc<PresenceLastSeenCache>) {
        self.presence = Some(src);
    }

    /// ZEB-623: install the per-peer protocol-compat registry (the SAME Arc the
    /// TunnelManager writes from the hello negotiation). Called once at boot;
    /// until then the default empty registry keeps the compat join inert.
    pub fn set_protocol_compat_source(
        &mut self,
        src: std::sync::Arc<crate::protocol_versioning::ProtocolCompatRegistry>,
    ) {
        self.protocol_compat = src;
    }

    /// ZEB-637: install the local node's own OwnerAddr so `snapshot` can
    /// filter the self row out of `peers[]`. Called once at boot when an
    /// identity is loaded; when unset (no identity yet) no filtering
    /// happens — additive like the other `set_*` sources.
    pub fn set_self_owner(&mut self, owner: [u8; 16]) {
        self.self_owner = Some(owner);
    }

    /// ZEB-702 (Component D): install the butler-deposit stats source — the
    /// SAME `Arc` the acceptor shell increments. Called once at boot alongside
    /// the acceptor install; when unset (no acceptor — no owner), `snapshot`'s
    /// `butler_deposits` stays `None`, matching the other additive sources.
    pub fn set_butler_deposit_source(
        &mut self,
        src: std::sync::Arc<crate::iroh_butler_acceptor::ButlerDepositStats>,
    ) {
        self.butler_deposits = Some(src);
    }

    /// ZEB-710: install the drain-fence degraded-mode counter source — the
    /// process-lived `dm_outbox::DM_FENCE_STATS` Arc. Called once at boot;
    /// when unset, `snapshot`'s `dm_fence` stays `None`. `pub(crate)`
    /// because `DmFenceStats` is crate-private (a `pub fn` would leak it —
    /// same `private_interfaces` rationale as `root_serve_tx`).
    pub(crate) fn set_dm_fence_source(
        &mut self,
        src: std::sync::Arc<crate::dm_outbox::DmFenceStats>,
    ) {
        self.dm_fence = Some(src);
    }

    /// ZEB-803: install the community-relay ACCEPTOR telemetry source (the same
    /// `Arc` the pull shell writes). Additive — when unset,
    /// `snapshot().community_relay` reports `None` for a node with no acceptor.
    pub(crate) fn set_community_relay_serving_source(
        &mut self,
        src: std::sync::Arc<CommunityRelayServingTelemetry>,
    ) {
        self.community_relay_serving = Some(src);
    }

    /// ZEB-803: install the community-relay PULL-DRIVER telemetry source.
    pub(crate) fn set_community_relay_pull_source(
        &mut self,
        src: std::sync::Arc<CommunityRelayPullTelemetry>,
    ) {
        self.community_relay_pulling = Some(src);
    }

    /// Spec §5.1: read from all sources, synthesize a snapshot. Never
    /// fails — empty/None fields render gracefully in the UI.
    pub async fn snapshot(&self) -> NetworkHealthSnapshot {
        let now = now_ms();

        // ZEB-622: one read of the peer-liveness projection, reused for the
        // per-peer connection-mode/rtt join, the last-seen freshness fold, and
        // the `MyNetworkSummary.relay_rtt_ms` fallback. Empty when no liveness
        // source is installed (unit tests, pre-boot) → every use is inert.
        let liveness_states: std::collections::HashMap<[u8; 32], LivenessStateWire> = self
            .liveness
            .as_ref()
            .map(|s| s.peer_states().into_iter().collect())
            .unwrap_or_default();
        let liveness_min_relay = self.liveness.as_ref().and_then(|s| s.min_relay_rtt_ms());

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
                // ZEB-622: iroh exposes no stable relay-RTT hook today (always
                // None) — fall back to the liveness machine's min relay RTT
                // across live peers so the panel still surfaces a number.
                relay_rtt_ms: self.iroh.relay_rtt_ms().or(liveness_min_relay),
                direct_addresses: self.iroh.direct_addresses(),
            });

        let mut records = self.resolver.list_records();

        // ZEB-620 Task 6: one read of the reconnect-supervisor's per-peer states,
        // used for BOTH the dial-state counts (below) and the PeerHealth
        // last-seen fallback (here). A single read keeps the two consistent.
        let peer_states = self
            .supervisor
            .as_ref()
            .map(|s| s.peer_states())
            .unwrap_or_default();

        // A resolver record that carries no `last_seen_ms` falls back to when the
        // supervisor last saw the peer connect (`Connected.since_ms`), joined by
        // iroh node id. Applied at the record level so the filter's sort and
        // record-age derivation both see the resolved value; never overrides a
        // record that already has a `last_seen_ms`.
        if !peer_states.is_empty() {
            let connected_since: std::collections::HashMap<[u8; 32], u64> = peer_states
                .iter()
                .filter_map(|(node_id, state)| match state {
                    PeerStateWire::Connected { since_ms } => Some((*node_id, *since_ms)),
                    _ => None,
                })
                .collect();
            for record in &mut records {
                if record.last_seen_ms.is_none() {
                    if let Some(&since_ms) = connected_since.get(&record.iroh_node_id) {
                        record.last_seen_ms = Some(since_ms);
                    }
                }
            }
        }

        // ZEB-595: enrich each record's connection_mode + rtt_ms from the most
        // recent self-test's per-peer ping results (NO new network call — the
        // self-test already measured them; matched by owner-addr hex). Only a
        // Pass updates the record; Fail/Skipped leave the NoConnection/None
        // defaults. No-op until a self-test has run.
        //
        // ZEB-622: this overlay runs at the RECORD level and BEFORE the liveness
        // join below, so live transport data (`liveness_states`) wins over a
        // stale cached self-test for the same peer.
        {
            let last = self.last_self_test.read().await;
            if let Some(report) = last.as_ref() {
                let by_owner: std::collections::HashMap<&str, &PeerPingResult> = report
                    .peer_results
                    .iter()
                    .map(|p| (p.owner_addr.as_str(), p))
                    .collect();
                for record in &mut records {
                    let owner_hex = hex::encode(record.owner_addr);
                    if let Some(ping) = by_owner.get(owner_hex.as_str()) {
                        if let StepOutcome::Pass { duration_ms } = &ping.outcome {
                            record.rtt_ms = Some(*duration_ms);
                            if let Some(mode) = ping.mode {
                                record.connection_mode = mode;
                            }
                        }
                    }
                }
            }
        }

        // ZEB-622: join the live peer-liveness transport state onto each record
        // (by `iroh_node_id`). Runs AFTER the self-test overlay so live data
        // wins. `Connected` sets Direct/Relay + the live rtt; `Degraded` marks
        // the new Degraded mode AND clears rtt (the wire `Degraded` carries no
        // RTT by design, so a lingering self-test rtt beside `degraded` would be
        // stale honest-data-violating noise); `Disconnected` clears any stale
        // self-test overlay back to NoConnection/None (liveness KNOWS the peer is
        // down, so a lingering Direct+rtt from a cached self-test would be a lie);
        // an absent peer leaves the record's current mode (NoConnection default,
        // or a self-test value liveness has no opinion on).
        if !liveness_states.is_empty() {
            for record in &mut records {
                match liveness_states.get(&record.iroh_node_id) {
                    Some(LivenessStateWire::Connected { mode, rtt_ms, .. }) => {
                        record.connection_mode = match mode {
                            LivenessMode::Direct => ConnectionMode::Direct,
                            LivenessMode::Relay => ConnectionMode::Relay,
                        };
                        record.rtt_ms = *rtt_ms;
                    }
                    Some(LivenessStateWire::Degraded { .. }) => {
                        record.connection_mode = ConnectionMode::Degraded;
                        // `LivenessStateWire::Degraded` carries no RTT by design;
                        // clear any prior self-test overlay so `degraded` never
                        // ships alongside a stale rtt.
                        record.rtt_ms = None;
                    }
                    Some(LivenessStateWire::Disconnected { .. }) => {
                        record.connection_mode = ConnectionMode::NoConnection;
                        record.rtt_ms = None;
                    }
                    None => {}
                }
            }
        }

        // ZEB-622: fold the freshest last-seen evidence into each record. The
        // supervisor `Connected.since_ms` fallback (above) fills a record that
        // had none; here we additionally max-merge the liveness `Connected
        // .since_ms` and the presence-beacon cache so a fresher signal from
        // either advances the record's own value (never regresses it).
        for record in &mut records {
            let mut best = record.last_seen_ms;
            if let Some(LivenessStateWire::Connected { since_ms, .. }) =
                liveness_states.get(&record.iroh_node_id)
            {
                best = Some(best.map_or(*since_ms, |b| b.max(*since_ms)));
            }
            if let Some(cache) = self.presence.as_ref() {
                if let Some(seen) = cache.last_seen(&record.owner_addr) {
                    best = Some(best.map_or(seen, |b| b.max(seen)));
                }
            }
            record.last_seen_ms = best;
        }

        // ZEB-623: join the per-peer protocol-compat registry onto each record
        // by iroh node id. An entry means the tunnel-v2 hello negotiation could
        // not agree a compatible protocol with this peer; the reason rides
        // through the filter onto PeerHealth so the panel shows it loudly
        // instead of the failure being a silent connect drop. Inert when the
        // registry is empty (unit tests, deposit-only node with no manager).
        for record in &mut records {
            record.protocol_incompat_reason =
                self.protocol_compat.incompat_reason(&record.iroh_node_id);
        }

        let peers = filter_peers_by_shared_membership(
            records,
            &*self.membership,
            self.self_owner.as_ref(),
            now,
        );

        // Patch reachability status now that we have peers.
        let my_network = my_network.map(|mut my| {
            my.reachability = derive_reachability_status(&my, &peers);
            my
        });

        NetworkHealthSnapshot {
            schema_version: 4,
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
            // ZEB-620 Task 6: fold in the live per-peer-state counts from the
            // supervisor snapshot (the ring telemetry knows nothing of states).
            dial_status: {
                let mut summary = self.dial.dial_summary();
                let counts = count_peer_states(&peer_states);
                summary.retrying = counts.retrying;
                summary.dormant = counts.dormant;
                summary.connected = counts.connected;
                summary
            },
            // ZEB-450: a live service means transport is up — never disabled.
            // The disabled case has no service and goes through the IPC's
            // empty()-path stamp instead.
            transport_disabled_reason: None,
            // ZEB-702 (Component D): butler-deposit decision counts, read from
            // the acceptor's shared stats. `None` (absent section) when no
            // acceptor is installed on this node (no owner identity).
            butler_deposits: self.butler_deposits.as_ref().map(|s| {
                let c = s.snapshot();
                ButlerDepositHealth {
                    accepted: c.accepted,
                    rejected_unauthorized: c.rejected_unauthorized,
                    rejected_other: c.rejected_other,
                }
            }),
            dm_fence: self.dm_fence.as_ref().map(|s| DmFenceHealth {
                phase_c_saturated_skips: s.phase_c_saturated_skips(),
                stop_fence_skipped_contended: s.stop_fence_skipped_contended(),
            }),
            // ZEB-803: `Some` when EITHER side is wired — a node can serve
            // without pulling (relay-only) or pull without serving (opt-in off),
            // and the whole point of the section is saying which side is dark.
            // The unwired side reports its zeroed default rather than suppressing
            // the section, so "not running" is never dressed up as "running and
            // idle". Only a node with neither reports `None`.
            community_relay: match (
                self.community_relay_serving.as_ref(),
                self.community_relay_pulling.as_ref(),
            ) {
                (None, None) => None,
                (serving, pulling) => Some(CommunityRelayHealth {
                    serving: serving.map(|s| s.summary()).unwrap_or_default(),
                    pulling: pulling.map(|p| p.summary()).unwrap_or_default(),
                }),
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
                ConnectionMode::Degraded => "degraded",
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
    for ev in &snapshot.pkarr_status.recent_fallback_events {
        // Defense-in-depth: route through `r()` even though the field
        // names imply upstream pre-redaction. A future bug populating
        // these with full hex must not slip past the [0-9a-f]{32,}
        // regex guard exercised by the redaction tests.
        let outcome = match ev.outcome {
            PkarrFallbackOutcome::Hit => "hit",
            PkarrFallbackOutcome::Miss => "miss",
            PkarrFallbackOutcome::Error => "error",
        };
        let _ = writeln!(
            out,
            "fallback {} in {} -> {}",
            r(&ev.peer_addr_short),
            r(&ev.community_id_short),
            outcome
        );
    }
    for relay in &snapshot.pkarr_status.relays {
        // Redact loopback/private/link-local relay hosts — public relays are
        // fine verbatim, but a shared export shouldn't leak a user's LAN relay.
        let display_url = match url::Url::parse(&relay.url) {
            Ok(u) if crate::connectivity_settings::is_local_host(u.host_str().unwrap_or("")) => {
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
    // Discard the live connection — callers that want the selected-path mode
    // (ZEB-622) use `ping_peer_conn` directly and inspect it via
    // `mode_from_conn`.
    ping_peer_conn(endpoint, node_id, timeout)
        .await
        .map(|(rtt, _conn)| rtt)
}

/// Connect + echo core of [`ping_peer`], returning the measured RTT AND the
/// live [`iroh::endpoint::Connection`] so the caller can read its selected path
/// (ZEB-622: honest Direct-vs-Relay mode via [`mode_from_conn`]). The
/// connection stays open until the returned value is dropped.
async fn ping_peer_conn(
    endpoint: &crate::iroh_endpoint::IrohEndpoint,
    node_id: iroh::EndpointId,
    timeout: std::time::Duration,
) -> Result<(std::time::Duration, iroh::endpoint::Connection), String> {
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
        // Hand the connection back so the caller can inspect its paths.
        Ok::<iroh::endpoint::Connection, String>(conn)
    })
    .await;
    match result {
        Ok(Ok(conn)) => Ok((start.elapsed(), conn)),
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
    ) -> futures::future::BoxFuture<'static, Result<(std::time::Duration, ConnectionMode), String>>;
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
        // ZEB-407: owner-hex → iroh node id, built before the membership
        // filter consumes `records`. The per-peer ping loop below looks up
        // each scoped peer's node id here (PeerHealth carries only the hex
        // owner addr, not the node id). Records whose node id is unknown
        // (all-zero) are excluded, so the loop reports them Skipped ("no node
        // id") rather than dialing a bogus zero id and reporting Fail.
        let node_id_by_owner: std::collections::HashMap<String, [u8; 32]> = records
            .iter()
            .filter(|r| r.iroh_node_id != [0u8; 32])
            .map(|r| (r.owner_addr_hex(), r.iroh_node_id))
            .collect();
        // ZEB-637: keep self out of the ping-candidate list too (same root cause as the snapshot peers[] row).
        let scoped = filter_peers_by_shared_membership(
            records,
            &*self.membership,
            self.self_owner.as_ref(),
            now,
        );
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
            let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(PEER_PING_CONCURRENCY));
            let mut handles = Vec::with_capacity(scoped.len());
            for peer in &scoped {
                // Permit acquired BEFORE spawn to provide N-of-32 spawn-rate
                // gating (back-pressure) — do NOT move this into the spawned
                // task: that would let all permits acquire immediately and
                // fan out unboundedly.
                let permit = std::sync::Arc::clone(&semaphore)
                    .acquire_owned()
                    .await
                    .expect("semaphore not closed");
                let owner_addr = peer.owner_addr.clone();
                match node_id_by_owner.get(&peer.owner_addr).copied() {
                    Some(node_id) => {
                        // `ping.ping` returns a 'static future (the trait was
                        // widened from '_ to 'static precisely so the future can
                        // be moved into a spawned task — a '_ future bound to
                        // this &self frame cannot). Build it in the parent loop,
                        // then move it into the task with the permit.
                        let fut = ping.ping(node_id, PEER_PING_TIMEOUT);
                        handles.push(tokio::spawn(async move {
                            // Hold the permit until the ping completes, then
                            // drop — preserves the semaphore cap (the Phase-1
                            // stub dropped early only because it did no work).
                            let _permit = permit;
                            let (outcome, mode) = match fut.await {
                                Ok((rtt, mode)) => (
                                    StepOutcome::Pass {
                                        duration_ms: rtt.as_millis() as u32,
                                    },
                                    Some(mode),
                                ),
                                Err(reason) => (StepOutcome::Fail { reason }, None),
                            };
                            PeerPingResult {
                                owner_addr,
                                outcome,
                                mode,
                            }
                        }));
                    }
                    None => {
                        // No reachability record carrying an iroh node id for
                        // this peer — nothing to dial. Honest Skipped.
                        drop(permit);
                        peer_results.push(PeerPingResult {
                            owner_addr,
                            outcome: StepOutcome::Skipped {
                                reason: "no node id for peer".into(),
                            },
                            mode: None,
                        });
                    }
                }
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
    ) -> futures::future::BoxFuture<'static, Result<(std::time::Duration, ConnectionMode), String>>
    {
        Box::pin(async { Err("dispatcher not wired".into()) })
    }
}

/// ZEB-622: derive the honest [`ConnectionMode`] from a live connection's
/// selected path, mirroring `peer_liveness::run_conn_path_watcher`'s idiom
/// (`is_selected()` → the active path; `is_relay()` → `Relay`, else `Direct`).
/// If no path is marked selected yet, keep `Direct` as the documented
/// fallback: the echo just succeeded so a path exists — the `paths()` snapshot
/// merely raced its selection flag.
fn mode_from_conn(conn: &iroh::endpoint::Connection) -> ConnectionMode {
    conn.paths()
        .iter()
        .find(|p| p.is_selected())
        .map(|p| {
            if p.is_relay() {
                ConnectionMode::Relay
            } else {
                ConnectionMode::Direct
            }
        })
        .unwrap_or(ConnectionMode::Direct)
}

/// Production [`PingDispatcher`]: dials each peer's iroh node id on the
/// `harmony/ping/v1` ALPN via [`ping_peer_conn`], reports the measured RTT,
/// and derives the real [`ConnectionMode`] from the connection's selected path.
pub struct ProdPingDispatcher {
    endpoint: std::sync::Arc<crate::iroh_endpoint::IrohEndpoint>,
}

impl ProdPingDispatcher {
    pub fn new(endpoint: std::sync::Arc<crate::iroh_endpoint::IrohEndpoint>) -> Self {
        Self { endpoint }
    }
}

impl PingDispatcher for ProdPingDispatcher {
    fn ping(
        &self,
        peer_node_id_bytes: [u8; 32],
        timeout: std::time::Duration,
    ) -> futures::future::BoxFuture<'static, Result<(std::time::Duration, ConnectionMode), String>>
    {
        // Clone the endpoint Arc into the 'static future so it owns
        // everything it needs (no borrow of &self).
        let endpoint = std::sync::Arc::clone(&self.endpoint);
        Box::pin(async move {
            let node_id = iroh::EndpointId::from_bytes(&peer_node_id_bytes)
                .map_err(|_| "invalid node id".to_string())?;
            let (rtt, conn) = ping_peer_conn(&endpoint, node_id, timeout).await?;
            // ZEB-622: report the REAL transport mode. The echo just succeeded
            // over `conn`, so its selected path tells us Direct vs Relay
            // honestly (with a documented Direct fallback if the snapshot
            // raced the selection flag — see `mode_from_conn`).
            Ok((rtt, mode_from_conn(&conn)))
        })
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
                iroh_node_id: payload.iroh_node_id,
                // Phase 1: no profile-cache lookup wired here. Follow-up
                // pulls display names out of the profile-broadcast cache.
                display_name: None,
                // Phase 1: no live iroh connection-mode inspection. The
                // field defaults to NoConnection so the UI shows the
                // peer without a misleading "Direct/Relay" badge.
                connection_mode: ConnectionMode::NoConnection,
                rtt_ms: None,
                last_seen_ms: Some(payload.announced_at_ms),
                // ZEB-623: filled by `NetworkHealthService::snapshot`'s compat
                // join (the resolver has no view of the registry).
                protocol_incompat_reason: None,
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
    /// ZEB-595: the SAME ring the `PkarrResolverAdapter` writes Case-C fallback
    /// probe outcomes into. Read-only here.
    fallback_telemetry: std::sync::Arc<PkarrFallbackTelemetry>,
}

impl ProdPkarrSnapshot {
    pub fn new(
        publisher: std::sync::Arc<harmony_pkarr::PkarrPublisher>,
        fallback_telemetry: std::sync::Arc<PkarrFallbackTelemetry>,
    ) -> Self {
        Self {
            publisher,
            fallback_telemetry,
        }
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
        // ZEB-595: read the bounded ring written by `PkarrResolverAdapter`
        // (Case-C in-community pkarr fallback). Empty until the resolver's
        // `contexts_fn` is implemented (ZEB-323 §4.4) and the fallback
        // actually probes — at which point the panel lights up with no
        // further Network Health changes.
        self.fallback_telemetry.recent()
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
    /// joined-member set. Callers should drop any async lock guard before
    /// calling (the on-epoch and boot-replay paths both do) to keep the
    /// engine critical section short. A poisoned lock is RECOVERED rather
    /// than treated as a no-op: the critical sections are panic-free map
    /// ops, so one panic-while-holding elsewhere must not permanently
    /// disable peer scoping (matches the ZEB-495 dedupe-lock recovery).
    pub fn set_community_members(
        &self,
        community: crate::owner_state_types::SpaceId,
        joined: std::collections::BTreeSet<crate::owner_state_types::OwnerAddr>,
    ) {
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        g.insert(community, joined);
    }

    /// The local node is NOT (or no longer) `Joined` in `community`;
    /// drop it entirely so no peer matches through it. Poisoned lock
    /// recovered (see `set_community_members`).
    pub fn remove_community(&self, community: &crate::owner_state_types::SpaceId) {
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        g.remove(community);
    }

    /// Communities (lowercase hex, ascending) the local node shares with
    /// `peer`. Synchronous: no `.await` on the read path. A poisoned lock
    /// is recovered rather than read as an (incorrect) empty set.
    pub fn communities_shared_with(&self, peer: &[u8; 16]) -> Vec<String> {
        let needle = crate::owner_state_types::OwnerAddr(*peer);
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        g.iter()
            .filter(|(_, members)| members.contains(&needle))
            .map(|(cid, _)| hex::encode(cid.0))
            .collect()
    }

    /// True if `peer` is a Joined member of any community OTHER than
    /// `excluding` that the local node is Joined in. The lib.rs Leave/Kick
    /// eviction arms consult this (ZEB-634 item 2) so a departure from ONE
    /// shared community doesn't evict the reachability records and
    /// reconnect slot of a peer who is still a co-member elsewhere.
    /// `excluding` must be passed explicitly: for the SAME delta the
    /// consumer updates this projection AFTER the eviction arm runs, so the
    /// departing community's stored set is still pre-Leave (it would
    /// otherwise always match). Synchronous, poisoned lock recovered (see
    /// `set_community_members`).
    pub fn is_joined_elsewhere(
        &self,
        peer: &[u8; 16],
        excluding: &crate::owner_state_types::SpaceId,
    ) -> bool {
        let needle = crate::owner_state_types::OwnerAddr(*peer);
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        g.iter()
            .any(|(cid, members)| cid != excluding && members.contains(&needle))
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
            iroh_node_id: [byte; 32],
            display_name: None,
            connection_mode: mode,
            rtt_ms: None,
            last_seen_ms: last_seen,
            protocol_incompat_reason: None,
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

    /// ZEB-634 item 2: the Leave/Kick consult. Peer in A+B excluding A →
    /// true (skip eviction); only-A excluding A → false (last shared
    /// community: evict); unknown peer / empty projection → false.
    #[test]
    fn is_joined_elsewhere_matrix() {
        use crate::owner_state_types::{OwnerAddr, SpaceId};
        let proj = MembershipProjection::new();
        let a = SpaceId([0xA1; 16]);
        let b = SpaceId([0xB2; 16]);
        let peer = [0x77u8; 16];
        let loner = [0x88u8; 16];

        // Empty projection: nobody is joined anywhere.
        assert!(!proj.is_joined_elsewhere(&peer, &a), "empty projection");

        let mut a_members = std::collections::BTreeSet::new();
        a_members.insert(OwnerAddr(peer));
        a_members.insert(OwnerAddr(loner));
        proj.set_community_members(a, a_members);
        let mut b_members = std::collections::BTreeSet::new();
        b_members.insert(OwnerAddr(peer));
        proj.set_community_members(b, b_members);

        assert!(
            proj.is_joined_elsewhere(&peer, &a),
            "peer shares B: leaving A must not evict"
        );
        assert!(
            proj.is_joined_elsewhere(&peer, &b),
            "symmetric: leaving B, still in A"
        );
        assert!(
            !proj.is_joined_elsewhere(&loner, &a),
            "A is loner's LAST shared community: evict"
        );
        assert!(
            !proj.is_joined_elsewhere(&[0x99u8; 16], &a),
            "unknown peer matches nothing"
        );
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
        let kept = filter_peers_by_shared_membership(records, &membership, None, 2_000);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].owner_addr, hex::encode([0xAA; 16]));
        assert_eq!(kept[0].shared_communities, vec![hex::encode([0xC1u8; 16])]);
    }

    // ── ZEB-623 Task 3: per-peer protocol incompatibility surfacing ──────

    #[test]
    fn incompatible_peer_reason_flows_to_peer_health() {
        // A resolver record flagged incompatible (as the tunnel-v2 hello
        // negotiation records via ProtocolCompatRegistry) carries its reason
        // through the membership filter onto the PeerHealth the panel renders.
        let mut record = make_record(0xAA, ConnectionMode::NoConnection, Some(1_000));
        record.protocol_incompat_reason = Some("tunnel hello v0 < min 1".to_string());
        let out = filter_peers_by_shared_membership(
            vec![record],
            &*membership_sharing(&[0xAA]),
            None,
            2_000,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].protocol_incompat_reason.as_deref(),
            Some("tunnel hello v0 < min 1")
        );
    }

    #[test]
    fn peer_health_serializes_protocol_incompat_reason_camel_case() {
        let ph = PeerHealth {
            owner_addr: "abcd".into(),
            display_name: None,
            shared_communities: vec![],
            connection_mode: ConnectionMode::NoConnection,
            rtt_ms: None,
            last_seen_ms: None,
            reachability_record_age_ms: None,
            protocol_incompat_reason: Some("tunnel hello v0 < min 1".into()),
        };
        let v = serde_json::to_value(&ph).expect("serialize");
        assert_eq!(v["protocolIncompatReason"], "tunnel hello v0 < min 1");

        // A `None` still serializes as an explicit null with the field PRESENT
        // (additive-with-default: `#[serde(default)]`, never skip_serializing_if).
        let none = PeerHealth {
            protocol_incompat_reason: None,
            ..ph
        };
        let v2 = serde_json::to_value(&none).expect("serialize");
        assert!(
            v2.get("protocolIncompatReason").is_some(),
            "field must be present even when None"
        );
        assert!(v2["protocolIncompatReason"].is_null());
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
            protocol_incompat_reason: None,
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
            protocol_incompat_reason: None,
        }];
        assert_eq!(
            derive_reachability_status(&my, &peers),
            ReachabilityStatus::Degraded
        );
    }

    #[test]
    fn derive_reachability_status_degraded_when_only_peer_signal_degraded() {
        // ZEB-628: a peer whose only signal is liveness `Degraded` (live link,
        // no selected path yet) is degraded-reachable, NOT unreachable.
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
            connection_mode: ConnectionMode::Degraded,
            rtt_ms: None,
            last_seen_ms: None,
            reachability_record_age_ms: None,
            protocol_incompat_reason: None,
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
            protocol_incompat_reason: None,
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
        let out = filter_peers_by_shared_membership(records, &memb, None, 5000);
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
        let out = filter_peers_by_shared_membership(records, &memb, None, 5000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].owner_addr, hex::encode([0x11u8; 16]));
    }

    /// ZEB-637: the self owner's record is dropped from peers[]; `None`
    /// (no identity loaded) keeps the unfiltered behavior.
    #[test]
    fn filter_peers_drops_self_owner_row() {
        let records = vec![
            make_record(0x11, ConnectionMode::NoConnection, Some(1000)),
            make_record(0x22, ConnectionMode::Direct, Some(2000)),
        ];
        let mut table = std::collections::HashMap::new();
        table.insert([0x11u8; 16], vec!["comm-a".to_string()]);
        table.insert([0x22u8; 16], vec!["comm-a".to_string()]);
        let memb = FakeMembership { table };

        let self_owner = [0x11u8; 16];
        let out =
            filter_peers_by_shared_membership(records.clone(), &memb, Some(&self_owner), 5000);
        assert_eq!(out.len(), 1, "self row dropped");
        assert_eq!(out[0].owner_addr, hex::encode([0x22u8; 16]));

        let out = filter_peers_by_shared_membership(records, &memb, None, 5000);
        assert_eq!(out.len(), 2, "no identity → no filtering");
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
        let out = filter_peers_by_shared_membership(records, &memb, None, 5000);
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
        let out = filter_peers_by_shared_membership(records, &memb, None, 10_000);
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
        let out = filter_peers_by_shared_membership(records, &memb, None, 5000);
        assert_eq!(out[0].reachability_record_age_ms, Some(4000));
    }

    #[test]
    fn network_health_snapshot_empty_is_well_formed() {
        let s = NetworkHealthSnapshot::empty();
        assert_eq!(s.schema_version, 4);
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
            schema_version: 4,
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
                protocol_incompat_reason: None,
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
            butler_deposits: None,
            dm_fence: None,
            community_relay: None,
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
                outcome: PkarrFallbackOutcome::Hit,
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
            md.contains("schemaVersion: 4"),
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
        assert_eq!(snap.schema_version, 4);
    }

    // ── ZEB-702 Task 5: butler-deposit counters in the snapshot ────
    #[tokio::test]
    async fn snapshot_butler_deposits_section_serializes_camelcase() {
        use crate::iroh_butler_acceptor::{ButlerDepositStats, DepositReject};
        let stats = std::sync::Arc::new(ButlerDepositStats::new());
        stats.record_accepted(); // accepted = 1
        stats.record_rejected(&DepositReject::NotAuthorized); // rejected_unauthorized = 1
        stats.record_rejected(&DepositReject::NotAuthorized); // = 2
        stats.record_rejected(&DepositReject::WrongRecipient); // rejected_other = 1
        stats.record_rejected(&DepositReject::WrongRecipient); // = 2
        stats.record_rejected(&DepositReject::WrongRecipient); // = 3

        let mut svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        svc.set_butler_deposit_source(std::sync::Arc::clone(&stats));

        let snap = svc.snapshot().await;
        // Also assert the typed field, so the test pins BOTH the wire keys and
        // the Rust-side mapping.
        assert_eq!(
            snap.butler_deposits,
            Some(ButlerDepositHealth {
                accepted: 1,
                rejected_unauthorized: 2,
                rejected_other: 3,
            })
        );
        let v = serde_json::to_value(&snap).expect("snapshot serializes");
        let bd = &v["butlerDeposits"];
        assert_eq!(bd["accepted"], serde_json::json!(1));
        assert_eq!(bd["rejectedUnauthorized"], serde_json::json!(2));
        assert_eq!(bd["rejectedOther"], serde_json::json!(3));
        // No snake_case key leakage past the camelCase rename.
        assert!(bd.get("rejected_unauthorized").is_none());
        assert!(bd.get("rejected_other").is_none());
    }

    // ── ZEB-710: drain-fence degraded-mode counters in the snapshot ────
    #[tokio::test]
    async fn snapshot_dm_fence_section_serializes_camelcase() {
        // A private local instance (not the process-global) so the assertions
        // are exact, not delta-based.
        let stats = std::sync::Arc::new(crate::dm_outbox::DmFenceStats::new_for_source());
        stats.record_phase_c_saturated_skip();
        stats.record_stop_fence_skipped_contended();
        stats.record_stop_fence_skipped_contended();

        let mut svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        svc.set_dm_fence_source(std::sync::Arc::clone(&stats));

        let snap = svc.snapshot().await;
        assert_eq!(
            snap.dm_fence,
            Some(DmFenceHealth {
                phase_c_saturated_skips: 1,
                stop_fence_skipped_contended: 2,
            })
        );
        let v = serde_json::to_value(&snap).expect("snapshot serializes");
        let df = &v["dmFence"];
        assert_eq!(df["phaseCSaturatedSkips"], serde_json::json!(1));
        assert_eq!(df["stopFenceSkippedContended"], serde_json::json!(2));
        assert!(df.get("phase_c_saturated_skips").is_none());

        // Unset source ⇒ null section (the butlerDeposits convention).
        let svc2 = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        let snap2 = svc2.snapshot().await;
        assert!(snap2.dm_fence.is_none());
    }

    #[tokio::test]
    async fn snapshot_without_acceptor_omits_butler_deposits() {
        let svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        let snap = svc.snapshot().await;
        assert!(snap.butler_deposits.is_none());
        let v = serde_json::to_value(&snap).expect("snapshot serializes");
        // Present-as-null per the DTO's `Option` convention (the `myNetwork`
        // pattern) — the no-acceptor case is a null section, not a fabricated
        // zeroed one.
        assert!(v["butlerDeposits"].is_null());
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

    #[tokio::test]
    async fn snapshot_filters_self_owner_from_peers() {
        // ZEB-637: with a self owner installed, snapshot() drops the self row
        // from peers[]. Same three-peer fixture as the sort test above; the
        // pin is peer count (one fewer) + absence of the self owner_addr.
        let mut table = std::collections::HashMap::new();
        for b in [0x11u8, 0x22, 0x33] {
            table.insert([b; 16], vec!["c1".to_string()]);
        }
        let mut svc = NetworkHealthService::new(
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
        svc.set_self_owner([0x22u8; 16]);
        let snap = svc.snapshot().await;
        assert_eq!(snap.peers.len(), 2, "self row filtered out");
        let self_hex = hex::encode([0x22u8; 16]);
        assert!(
            snap.peers.iter().all(|p| p.owner_addr != self_hex),
            "no remaining row is the self owner"
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

    /// Scripted [`PingDispatcher`] keyed by node id — returns `Err` for any
    /// peer not in the table, so an "unscripted" dial surfaces loudly.
    struct ScriptedPingDispatcher {
        results: std::collections::HashMap<
            [u8; 32],
            Result<(std::time::Duration, ConnectionMode), String>,
        >,
    }
    impl PingDispatcher for ScriptedPingDispatcher {
        fn ping(
            &self,
            peer_node_id_bytes: [u8; 32],
            _timeout: std::time::Duration,
        ) -> futures::future::BoxFuture<
            'static,
            Result<(std::time::Duration, ConnectionMode), String>,
        > {
            let r = self
                .results
                .get(&peer_node_id_bytes)
                .cloned()
                .unwrap_or_else(|| Err("unscripted peer".into()));
            Box::pin(async move { r })
        }
    }

    fn membership_sharing(peers: &[u8]) -> std::sync::Arc<FakeMembership> {
        let mut table = std::collections::HashMap::new();
        for &b in peers {
            table.insert([b; 16], vec!["c1".to_string()]);
        }
        std::sync::Arc::new(FakeMembership { table })
    }

    #[tokio::test]
    async fn self_test_per_peer_ping_reports_pass_fail_and_skip() {
        // Three peers share a community and survive the membership filter:
        //   0xAA — scripted Ok  → Pass + Direct mode
        //   0xBB — scripted Err → Fail (reason surfaced)
        //   0xCC — zeroed node id → Skipped ("no node id"), never dialed.
        let mut records = vec![
            make_record(0xAA, ConnectionMode::NoConnection, Some(3_000)),
            make_record(0xBB, ConnectionMode::NoConnection, Some(2_000)),
            make_record(0xCC, ConnectionMode::NoConnection, Some(1_000)),
        ];
        records[2].iroh_node_id = [0u8; 32];

        let svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver { records }),
            membership_sharing(&[0xAA, 0xBB, 0xCC]),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );

        let mut scripted = std::collections::HashMap::new();
        scripted.insert(
            [0xAAu8; 32],
            Ok((std::time::Duration::from_millis(42), ConnectionMode::Direct)),
        );
        scripted.insert([0xBBu8; 32], Err("connect failed".to_string()));
        let dispatcher = ScriptedPingDispatcher { results: scripted };

        let iroh_t = ScriptedIrohTest {
            bound: true,
            relay: StepOutcome::Pass { duration_ms: 1 },
        };
        let pkarr_t = ScriptedPkarrTest {
            publish: StepOutcome::Pass { duration_ms: 1 },
            resolve: StepOutcome::Pass { duration_ms: 1 },
        };

        let report = svc.run_self_test(&iroh_t, &pkarr_t, &dispatcher).await;

        // Spawned pings complete in arbitrary order — index by owner hex.
        assert_eq!(report.peer_results.len(), 3);
        let by_owner: std::collections::HashMap<_, _> = report
            .peer_results
            .iter()
            .map(|p| (p.owner_addr.clone(), p))
            .collect();

        let aa = by_owner
            .get(&hex::encode([0xAAu8; 16]))
            .expect("aa result present");
        assert!(
            matches!(aa.outcome, StepOutcome::Pass { duration_ms: 42 }),
            "aa should Pass at 42ms"
        );
        assert_eq!(aa.mode, Some(ConnectionMode::Direct));

        let bb = by_owner
            .get(&hex::encode([0xBBu8; 16]))
            .expect("bb result present");
        assert!(
            matches!(&bb.outcome, StepOutcome::Fail { reason } if reason == "connect failed"),
            "bb should Fail with the dispatcher's reason"
        );
        assert_eq!(bb.mode, None);

        let cc = by_owner
            .get(&hex::encode([0xCCu8; 16]))
            .expect("cc result present");
        assert!(
            matches!(&cc.outcome, StepOutcome::Skipped { reason } if reason == "no node id for peer"),
            "cc should Skip (no node id)"
        );
        assert_eq!(cc.mode, None);
    }

    #[tokio::test]
    async fn self_test_omits_self_owner_from_ping_candidates() {
        // ZEB-637: with a self owner wired, the self record (0xAA) is dropped
        // from the ping-candidate list — only the real peer (0xBB) is dialed
        // and reported. Without the filter, self would surface an "unscripted
        // peer" Fail row in the user-visible self-test peer_results.
        let records = vec![
            make_record(0xAA, ConnectionMode::NoConnection, Some(2_000)),
            make_record(0xBB, ConnectionMode::NoConnection, Some(1_000)),
        ];
        let mut svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver { records }),
            membership_sharing(&[0xAA, 0xBB]),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        svc.set_self_owner([0xAAu8; 16]);

        let mut scripted = std::collections::HashMap::new();
        scripted.insert(
            [0xBBu8; 32],
            Ok((std::time::Duration::from_millis(7), ConnectionMode::Direct)),
        );
        let dispatcher = ScriptedPingDispatcher { results: scripted };
        let iroh_t = ScriptedIrohTest {
            bound: true,
            relay: StepOutcome::Pass { duration_ms: 1 },
        };
        let pkarr_t = ScriptedPkarrTest {
            publish: StepOutcome::Pass { duration_ms: 1 },
            resolve: StepOutcome::Pass { duration_ms: 1 },
        };

        let report = svc.run_self_test(&iroh_t, &pkarr_t, &dispatcher).await;

        assert_eq!(
            report.peer_results.len(),
            1,
            "only the non-self peer is pinged"
        );
        assert_eq!(report.peer_results[0].owner_addr, hex::encode([0xBBu8; 16]));
        let self_hex = hex::encode([0xAAu8; 16]);
        assert!(
            report.peer_results.iter().all(|p| p.owner_addr != self_hex),
            "self owner absent from ping results"
        );
    }

    #[tokio::test]
    async fn snapshot_enriches_peer_mode_rtt_from_last_self_test() {
        // A peer shares a community (survives the filter); the last self-test
        // pinged it Pass(Direct, 37ms). snapshot() should reflect that on the
        // peer instead of the NoConnection/None defaults.
        let svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver {
                records: vec![make_record(0xAA, ConnectionMode::NoConnection, Some(1_000))],
            }),
            membership_sharing(&[0xAA]),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        *svc.last_self_test.write().await = Some(SelfTestReport {
            started_at_ms: 0,
            finished_at_ms: 100,
            steps: vec![],
            peer_results: vec![PeerPingResult {
                owner_addr: hex::encode([0xAA; 16]),
                outcome: StepOutcome::Pass { duration_ms: 37 },
                mode: Some(ConnectionMode::Direct),
            }],
        });

        let snap = svc.snapshot().await;
        assert_eq!(snap.peers.len(), 1);
        assert_eq!(snap.peers[0].rtt_ms, Some(37));
        assert_eq!(snap.peers[0].connection_mode, ConnectionMode::Direct);
    }

    #[tokio::test]
    async fn snapshot_peer_mode_rtt_default_without_self_test() {
        // No cached self-test → the peer keeps the NoConnection/None defaults.
        let svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver {
                records: vec![make_record(0xAA, ConnectionMode::NoConnection, Some(1_000))],
            }),
            membership_sharing(&[0xAA]),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        let snap = svc.snapshot().await;
        assert_eq!(snap.peers.len(), 1);
        assert_eq!(snap.peers[0].rtt_ms, None);
        assert_eq!(snap.peers[0].connection_mode, ConnectionMode::NoConnection);
    }

    // ── ZEB-622 Task 5: liveness fusion + Degraded + presence last-seen ──

    /// Test `LivenessSnapshot` double: replays a scripted per-peer state list +
    /// a fixed min relay RTT.
    struct FakeLiveness {
        states: Vec<([u8; 32], LivenessStateWire)>,
        min_relay: Option<u32>,
    }
    impl LivenessSnapshot for FakeLiveness {
        fn peer_states(&self) -> Vec<([u8; 32], LivenessStateWire)> {
            self.states.clone()
        }
        fn min_relay_rtt_ms(&self) -> Option<u32> {
            self.min_relay
        }
    }

    /// Iroh double that is READY (has a node id) but reports no relay RTT — the
    /// production reality today (`ProdIrohSnapshot::relay_rtt_ms` is hardcoded
    /// None), so the liveness fallback is what actually fills the field.
    struct FakeIrohNoRelayRtt;
    impl IrohSnapshot for FakeIrohNoRelayRtt {
        fn iroh_node_id_hex(&self) -> Option<String> {
            Some("a3f9e1c2".repeat(8))
        }
        fn home_relay_url(&self) -> Option<String> {
            Some("https://derp.example/".into())
        }
        fn relay_rtt_ms(&self) -> Option<u32> {
            None
        }
        fn direct_addresses(&self) -> Vec<String> {
            vec![]
        }
        fn nat_classification(&self) -> NatClass {
            NatClass::Unknown
        }
    }

    /// (a) One Connected(Relay, 42), one Degraded, one absent → the three fused
    /// connection modes + rtts land on the right peers.
    #[tokio::test]
    async fn snapshot_fuses_liveness_states_into_peer_health() {
        let mut table = std::collections::HashMap::new();
        for b in [0x11u8, 0x22, 0x33] {
            table.insert([b; 16], vec!["c1".to_string()]);
        }
        let mut svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver {
                records: vec![
                    make_record(0x11, ConnectionMode::NoConnection, Some(1)),
                    make_record(0x22, ConnectionMode::NoConnection, Some(1)),
                    make_record(0x33, ConnectionMode::NoConnection, Some(1)),
                ],
            }),
            std::sync::Arc::new(FakeMembership { table }),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        svc.set_liveness_source(std::sync::Arc::new(FakeLiveness {
            states: vec![
                (
                    [0x11u8; 32],
                    LivenessStateWire::Connected {
                        mode: LivenessMode::Relay,
                        rtt_ms: Some(42),
                        since_ms: 5,
                    },
                ),
                ([0x22u8; 32], LivenessStateWire::Degraded { since_ms: 5 }),
                // 0x33 absent from the liveness projection.
            ],
            min_relay: None,
        }));
        let snap = svc.snapshot().await;
        let get = |b: u8| {
            snap.peers
                .iter()
                .find(|p| p.owner_addr == hex::encode([b; 16]))
                .expect("peer present")
        };
        let p11 = get(0x11);
        assert_eq!(p11.connection_mode, ConnectionMode::Relay);
        assert_eq!(p11.rtt_ms, Some(42));
        let p22 = get(0x22);
        assert_eq!(p22.connection_mode, ConnectionMode::Degraded);
        assert_eq!(p22.rtt_ms, None, "Degraded carries no rtt");
        let p33 = get(0x33);
        assert_eq!(p33.connection_mode, ConnectionMode::NoConnection);
        assert_eq!(p33.rtt_ms, None);
    }

    /// ZEB-623 Task 3: the snapshot joins the ProtocolCompatRegistry onto each
    /// peer by iroh node id, so a peer the tunnel handshake flagged
    /// incompatible surfaces the reason in its PeerHealth; a compatible peer
    /// carries `None`. `make_record(byte)` uses `[byte; 32]` as the node id.
    #[tokio::test]
    async fn snapshot_surfaces_protocol_incompat_reason_from_registry() {
        let registry =
            std::sync::Arc::new(crate::protocol_versioning::ProtocolCompatRegistry::default());
        registry.note_incompatible([0xAA; 32], "tunnel hello v0 < min 1".to_string());
        let mut svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver {
                records: vec![
                    make_record(0xAA, ConnectionMode::NoConnection, Some(1_000)),
                    make_record(0xBB, ConnectionMode::NoConnection, Some(1_000)),
                ],
            }),
            membership_sharing(&[0xAA, 0xBB]),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        svc.set_protocol_compat_source(std::sync::Arc::clone(&registry));
        let snap = svc.snapshot().await;
        let get = |b: u8| {
            snap.peers
                .iter()
                .find(|p| p.owner_addr == hex::encode([b; 16]))
                .expect("peer present")
        };
        assert_eq!(
            get(0xAA).protocol_incompat_reason.as_deref(),
            Some("tunnel hello v0 < min 1")
        );
        assert_eq!(get(0xBB).protocol_incompat_reason, None);
    }

    /// (b) Live liveness data wins over a stale cached self-test for the same
    /// peer — the self-test says Direct/37, liveness says Relay/99 → Relay/99.
    #[tokio::test]
    async fn liveness_overrides_stale_self_test_mode() {
        let mut svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver {
                records: vec![make_record(0xAA, ConnectionMode::NoConnection, Some(1_000))],
            }),
            membership_sharing(&[0xAA]),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        *svc.last_self_test.write().await = Some(SelfTestReport {
            started_at_ms: 0,
            finished_at_ms: 100,
            steps: vec![],
            peer_results: vec![PeerPingResult {
                owner_addr: hex::encode([0xAA; 16]),
                outcome: StepOutcome::Pass { duration_ms: 37 },
                mode: Some(ConnectionMode::Direct),
            }],
        });
        svc.set_liveness_source(std::sync::Arc::new(FakeLiveness {
            states: vec![(
                [0xAAu8; 32],
                LivenessStateWire::Connected {
                    mode: LivenessMode::Relay,
                    rtt_ms: Some(99),
                    since_ms: 5,
                },
            )],
            min_relay: None,
        }));
        let snap = svc.snapshot().await;
        assert_eq!(snap.peers.len(), 1);
        assert_eq!(
            snap.peers[0].connection_mode,
            ConnectionMode::Relay,
            "live liveness mode wins over stale self-test Direct"
        );
        assert_eq!(snap.peers[0].rtt_ms, Some(99), "live rtt wins too");
    }

    /// (b2) A stale cached self-test says the peer is Direct/37, but liveness
    /// KNOWS the transport is `Disconnected` → the overlay is cleared back to
    /// NoConnection/None (rather than leaving the lie standing).
    #[tokio::test]
    async fn liveness_disconnected_clears_stale_self_test_overlay() {
        let mut svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver {
                records: vec![make_record(0xAA, ConnectionMode::NoConnection, Some(1_000))],
            }),
            membership_sharing(&[0xAA]),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        // Self-test overlay set Direct + rtt (the stale value we must not trust).
        *svc.last_self_test.write().await = Some(SelfTestReport {
            started_at_ms: 0,
            finished_at_ms: 100,
            steps: vec![],
            peer_results: vec![PeerPingResult {
                owner_addr: hex::encode([0xAA; 16]),
                outcome: StepOutcome::Pass { duration_ms: 37 },
                mode: Some(ConnectionMode::Direct),
            }],
        });
        svc.set_liveness_source(std::sync::Arc::new(FakeLiveness {
            states: vec![(
                [0xAAu8; 32],
                LivenessStateWire::Disconnected { since_ms: 5 },
            )],
            min_relay: None,
        }));
        let snap = svc.snapshot().await;
        assert_eq!(snap.peers.len(), 1);
        assert_eq!(
            snap.peers[0].connection_mode,
            ConnectionMode::NoConnection,
            "liveness Disconnected clears the stale self-test Direct overlay"
        );
        assert_eq!(
            snap.peers[0].rtt_ms, None,
            "liveness Disconnected clears the stale self-test rtt too"
        );
    }

    /// (b3) A stale cached self-test says Direct/37, but liveness reports the
    /// link `Degraded` (up, no selected path) → the mode becomes Degraded AND the
    /// stale self-test rtt is cleared (the wire `Degraded` carries no RTT, so a
    /// lingering value would ship `degraded` beside a phantom rtt).
    #[tokio::test]
    async fn liveness_degraded_clears_stale_self_test_rtt() {
        let mut svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver {
                records: vec![make_record(0xAA, ConnectionMode::NoConnection, Some(1_000))],
            }),
            membership_sharing(&[0xAA]),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        // Self-test overlay set Direct + rtt (the stale value that must be cleared).
        *svc.last_self_test.write().await = Some(SelfTestReport {
            started_at_ms: 0,
            finished_at_ms: 100,
            steps: vec![],
            peer_results: vec![PeerPingResult {
                owner_addr: hex::encode([0xAA; 16]),
                outcome: StepOutcome::Pass { duration_ms: 37 },
                mode: Some(ConnectionMode::Direct),
            }],
        });
        svc.set_liveness_source(std::sync::Arc::new(FakeLiveness {
            states: vec![([0xAAu8; 32], LivenessStateWire::Degraded { since_ms: 5 })],
            min_relay: None,
        }));
        let snap = svc.snapshot().await;
        assert_eq!(snap.peers.len(), 1);
        assert_eq!(
            snap.peers[0].connection_mode,
            ConnectionMode::Degraded,
            "liveness Degraded wins over the stale self-test Direct"
        );
        assert_eq!(
            snap.peers[0].rtt_ms, None,
            "liveness Degraded clears the stale self-test rtt (Degraded carries no RTT)"
        );
        assert_eq!(
            snap.my_network.expect("my_network present").reachability,
            ReachabilityStatus::Degraded,
            "ZEB-628: peer-signal-only Degraded rolls up Degraded, not Unreachable"
        );
    }

    /// (c) `MyNetworkSummary.relay_rtt_ms` falls back to the liveness min when
    /// iroh reports None; when iroh HAS a value it wins.
    #[tokio::test]
    async fn relay_rtt_falls_back_to_liveness_min() {
        // iroh None → liveness min fills it.
        let mut svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIrohNoRelayRtt),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        svc.set_liveness_source(std::sync::Arc::new(FakeLiveness {
            states: vec![],
            min_relay: Some(77),
        }));
        let snap = svc.snapshot().await;
        assert_eq!(snap.my_network.expect("ready").relay_rtt_ms, Some(77));

        // iroh Some(24) → iroh wins, liveness min ignored.
        let mut svc2 = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        svc2.set_liveness_source(std::sync::Arc::new(FakeLiveness {
            states: vec![],
            min_relay: Some(77),
        }));
        let snap2 = svc2.snapshot().await;
        assert_eq!(snap2.my_network.expect("ready").relay_rtt_ms, Some(24));
    }

    /// (d) `last_seen_ms` prefers the freshest of {record ts, presence cache,
    /// liveness Connected.since_ms}. Record < presence cache → cache wins; both
    /// < since_ms → since_ms wins.
    #[tokio::test]
    async fn last_seen_prefers_freshest_source() {
        // Case 1: presence cache (5_000) fresher than record (1_000), no liveness.
        let mut svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver {
                records: vec![make_record(0x77, ConnectionMode::NoConnection, Some(1_000))],
            }),
            membership_sharing(&[0x77]),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        let cache = std::sync::Arc::new(PresenceLastSeenCache::new());
        cache.note_seen([0x77; 16], 5_000);
        svc.set_presence_source(std::sync::Arc::clone(&cache));
        let snap = svc.snapshot().await;
        assert_eq!(
            snap.peers[0].last_seen_ms,
            Some(5_000),
            "presence cache is fresher than the record → cache wins"
        );

        // Case 2: add liveness Connected.since_ms (9_000) — fresher than both.
        svc.set_liveness_source(std::sync::Arc::new(FakeLiveness {
            states: vec![(
                [0x77u8; 32],
                LivenessStateWire::Connected {
                    mode: LivenessMode::Direct,
                    rtt_ms: Some(3),
                    since_ms: 9_000,
                },
            )],
            min_relay: None,
        }));
        let snap2 = svc.snapshot().await;
        assert_eq!(
            snap2.peers[0].last_seen_ms,
            Some(9_000),
            "liveness Connected.since_ms is freshest → since_ms wins"
        );
    }

    /// (e) Serde pin: the new `ConnectionMode::Degraded` variant serializes to
    /// the wire tag `"degraded"` (the TS DTO in Task 6 reads this).
    #[test]
    fn connection_mode_degraded_serde_pin() {
        let v = serde_json::to_value(ConnectionMode::Degraded).expect("serialize");
        assert_eq!(v, serde_json::json!("degraded"));
    }

    /// (g) `PresenceLastSeenCache` max-merges: a stale (lower) note never
    /// regresses a fresher recorded value; a fresher note advances it.
    #[test]
    fn presence_last_seen_cache_max_merges() {
        let c = PresenceLastSeenCache::new();
        assert_eq!(c.last_seen(&[1; 16]), None);
        c.note_seen([1; 16], 100);
        assert_eq!(c.last_seen(&[1; 16]), Some(100));
        c.note_seen([1; 16], 50); // stale → must not regress
        assert_eq!(c.last_seen(&[1; 16]), Some(100));
        c.note_seen([1; 16], 200); // fresher → advances
        assert_eq!(c.last_seen(&[1; 16]), Some(200));
        // A different owner is independent.
        assert_eq!(c.last_seen(&[2; 16]), None);
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
        ProdPkarrSnapshot::new(
            publisher,
            std::sync::Arc::new(PkarrFallbackTelemetry::new()),
        )
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

    // ── ZEB-803: community-relay serving / pulling telemetry ──

    #[test]
    fn never_served_relay_reports_none_not_epoch() {
        // The 0-sentinel must surface as absence. If this regresses, the panel
        // renders 1970 and "never served" reads as "served 56 years ago" —
        // which during the incident would look like data rather than a gap.
        let t = CommunityRelayServingTelemetry::new();
        let s = t.summary();
        assert_eq!(s.last_served_ms, None, "never-served must be None, not 0");
        assert_eq!(s.pulls_served, 0);
        assert!(s.peers.is_empty());

        let p = CommunityRelayPullTelemetry::new();
        let ps = p.summary();
        assert_eq!(ps.last_pass_ms, None);
        assert_eq!(ps.last_ingest_ms, None);
    }

    #[test]
    fn serving_telemetry_tracks_per_peer_last_served() {
        let t = CommunityRelayServingTelemetry::new();
        let a = [0xAAu8; 32];
        let b = [0xBBu8; 32];
        t.record_served(&a);
        t.record_served(&b);
        t.record_served(&b);
        t.record_rejected();
        t.record_failed();

        let s = t.summary();
        assert_eq!(s.pulls_served, 3);
        assert_eq!(s.pulls_rejected, 1);
        assert_eq!(s.pulls_failed, 1);
        assert!(s.last_served_ms.is_some());
        assert_eq!(s.peers.len(), 2, "one row per peer");

        let bb = s
            .peers
            .iter()
            .find(|p| p.peer_short == "bbbbbbbb")
            .expect("peer b present");
        assert_eq!(bb.served_count, 2);
        // Short-form only — the full 32-byte id must never reach the wire.
        assert_eq!(bb.peer_short.len(), 8, "ZEB-329 redaction: 8 hex chars");
        for p in &s.peers {
            assert!(
                !p.peer_short.contains("aaaaaaaaaa"),
                "must not carry more than 4 bytes of the node id"
            );
        }
    }

    #[test]
    fn serving_peer_map_is_bounded_and_evicts_least_recently_served() {
        // A public relay serves an unbounded peer set; the map must not grow
        // without bound. Eviction is least-recently-served so the peer that
        // STOPPED being served is the last to be dropped — during an incident
        // that row is the evidence.
        let t = CommunityRelayServingTelemetry::new();
        for i in 0..(COMMUNITY_RELAY_PEER_CAP + 10) {
            let mut peer = [0u8; 32];
            peer[0] = (i / 256) as u8;
            peer[1] = (i % 256) as u8;
            peer[2] = 0xCC;
            peer[3] = 0xDD;
            t.record_served(&peer);
        }
        let s = t.summary();
        assert!(
            s.peers.len() <= COMMUNITY_RELAY_PEER_CAP,
            "peer map must stay bounded, got {}",
            s.peers.len()
        );
        assert_eq!(
            s.pulls_served,
            (COMMUNITY_RELAY_PEER_CAP + 10) as u64,
            "the counter is NOT capped even though the map is"
        );
    }

    #[test]
    fn serving_peers_sorted_newest_first() {
        let t = CommunityRelayServingTelemetry::new();
        t.record_served(&[0x11u8; 32]);
        t.record_served(&[0x22u8; 32]);
        let s = t.summary();
        let stamps: Vec<u64> = s.peers.iter().map(|p| p.last_served_ms).collect();
        let mut sorted = stamps.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(stamps, sorted, "peers must be newest-served first");
    }

    #[test]
    fn pull_pass_counter_climbs_even_when_nothing_to_do() {
        // This is the loop-liveness proof. A driver whose task died and a driver
        // with no joined communities are identical in the success counters; they
        // MUST differ here, or the receiver side keeps the blind spot that let a
        // >=33-minute delivery lag look like a quiet channel.
        let p = CommunityRelayPullTelemetry::new();
        p.record_pass_start();
        p.record_pass_start();
        let s = p.summary();
        assert_eq!(s.passes_run, 2);
        assert!(s.last_pass_ms.is_some());
        assert_eq!(s.sessions_ok, 0, "no session ran");
        assert_eq!(s.blobs_ingested, 0);
    }

    #[test]
    fn zero_ingest_session_is_success_not_failure() {
        // A relay holding nothing for us is a healthy answer. Counting it as a
        // failure would cry wolf on every idle pass; counting it as nothing at
        // all is silent path 2 from the module docs.
        let p = CommunityRelayPullTelemetry::new();
        p.record_session_ok(&[7u8; 16], &[9u8; 16], 0);
        let s = p.summary();
        assert_eq!(s.sessions_ok, 1);
        assert_eq!(s.sessions_failed, 0);
        assert_eq!(s.blobs_ingested, 0);
        assert_eq!(
            s.last_ingest_ms, None,
            "a 0-blob success must not stamp last_ingest_ms"
        );
        assert_eq!(s.recent.len(), 1);
        assert_eq!(s.recent[0].outcome, "ok");
    }

    #[test]
    fn no_relay_path_is_counted_distinctly_from_failure() {
        // Silent path 3: a joined community with no fresh relay never entered
        // the inner loop and produced no log line at all. It must be
        // distinguishable from a relay that was tried and failed, because the
        // remedies differ (stale/absent announce vs transport fault).
        let p = CommunityRelayPullTelemetry::new();
        p.record_no_relay(&[3u8; 16]);
        p.record_session_failed(&[3u8; 16], &[4u8; 16]);
        let s = p.summary();
        assert_eq!(s.passes_no_relay, 1);
        assert_eq!(s.sessions_failed, 1);
        assert_eq!(
            s.sessions_ok, 0,
            "neither path may be mistaken for a success"
        );
        let outcomes: Vec<&str> = s.recent.iter().map(|h| h.outcome.as_str()).collect();
        assert_eq!(outcomes, vec!["noRelay", "failed"]);
    }

    #[test]
    fn pull_ring_is_bounded() {
        let p = CommunityRelayPullTelemetry::new();
        for _ in 0..(COMMUNITY_RELAY_PULL_RING_CAP + 5) {
            p.record_session_failed(&[1u8; 16], &[2u8; 16]);
        }
        let s = p.summary();
        assert_eq!(
            s.recent.len(),
            COMMUNITY_RELAY_PULL_RING_CAP,
            "ring must be capped"
        );
        assert_eq!(
            s.sessions_failed,
            (COMMUNITY_RELAY_PULL_RING_CAP + 5) as u64,
            "counter is not capped"
        );
    }

    #[test]
    fn relay_health_wire_keys_are_camel_case() {
        // NetworkHealthSnapshot is serde-wire and read by the TS adapter; the
        // exact key spellings are the contract.
        let t = CommunityRelayServingTelemetry::new();
        t.record_served(&[0xEEu8; 32]);
        let p = CommunityRelayPullTelemetry::new();
        p.record_pass_start();
        p.record_no_relay(&[5u8; 16]);
        let health = CommunityRelayHealth {
            serving: t.summary(),
            pulling: p.summary(),
        };
        let v = serde_json::to_value(&health).expect("serialize");
        let serving = &v["serving"];
        for k in [
            "pullsServed",
            "pullsRejected",
            "pullsFailed",
            "lastServedMs",
            "peers",
        ] {
            assert!(serving.get(k).is_some(), "missing serving key {k}");
        }
        assert!(serving["peers"][0].get("peerShort").is_some());
        assert!(serving["peers"][0].get("lastServedMs").is_some());
        assert!(serving["peers"][0].get("servedCount").is_some());
        let pulling = &v["pulling"];
        for k in [
            "passesRun",
            "lastPassMs",
            "sessionsOk",
            "sessionsFailed",
            "blobsIngested",
            "lastIngestMs",
            "passesNoRelay",
            "recent",
        ] {
            assert!(pulling.get(k).is_some(), "missing pulling key {k}");
        }
        assert!(pulling["recent"][0].get("communityShort").is_some());
        assert!(pulling["recent"][0].get("relayDeviceShort").is_some());
        assert!(pulling["recent"][0].get("capturedAtMs").is_some());
    }

    #[test]
    fn snapshot_without_relay_field_still_deserializes() {
        // Forward-compat: a cached/exported snapshot written before ZEB-803 must
        // still load. Same guarantee butlerDeposits and dmFence carry.
        let mut v = serde_json::to_value(NetworkHealthSnapshot::empty()).expect("serialize");
        v.as_object_mut().expect("object").remove("communityRelay");
        let back: NetworkHealthSnapshot =
            serde_json::from_value(v).expect("pre-ZEB-803 snapshot must still deserialize");
        assert_eq!(back.community_relay, None);
    }

    #[test]
    fn empty_snapshot_relay_is_none_not_zeroed() {
        // `None` means "no relay wiring on this node"; `Some` with zeroed
        // counters means "wired and serving nothing" — the incident state.
        // Collapsing them would hide exactly what this field is for.
        let snap = NetworkHealthSnapshot::empty();
        assert_eq!(snap.community_relay, None);
    }

    // ── ZEB-620 Task 6: supervisor-state telemetry + PeerHealth feeds ──

    /// Test `SupervisorSnapshot` double: replays a scripted per-peer state list.
    struct FakeSupervisor(Vec<([u8; 32], crate::reconnect_supervisor::PeerStateWire)>);
    impl SupervisorSnapshot for FakeSupervisor {
        fn peer_states(&self) -> Vec<([u8; 32], crate::reconnect_supervisor::PeerStateWire)> {
            self.0.clone()
        }
    }

    #[test]
    fn count_peer_states_tallies_by_kind() {
        use crate::reconnect_supervisor::PeerStateWire;
        let states = vec![
            ([1u8; 32], PeerStateWire::Connected { since_ms: 10 }),
            ([2u8; 32], PeerStateWire::Connected { since_ms: 20 }),
            (
                [3u8; 32],
                PeerStateWire::Retrying {
                    attempt: 1,
                    retry_in_ms: 500,
                },
            ),
            ([4u8; 32], PeerStateWire::Dormant { since_ms: 5 }),
            (
                [5u8; 32],
                PeerStateWire::Retrying {
                    attempt: 3,
                    retry_in_ms: 900,
                },
            ),
        ];
        let c = count_peer_states(&states);
        assert_eq!(c.connected, 2);
        assert_eq!(c.retrying, 2);
        assert_eq!(c.dormant, 1);
    }

    #[test]
    fn count_peer_states_empty_is_zero() {
        let c = count_peer_states(&[]);
        assert_eq!((c.connected, c.retrying, c.dormant), (0, 0, 0));
    }

    #[test]
    fn dial_health_summary_serializes_new_camelcase_fields() {
        let s = DialHealthSummary {
            attempts: 3,
            succeeded: 1,
            failed: 1,
            skipped_duplicate: 0,
            retrying: 4,
            dormant: 2,
            connected: 5,
            recent: vec![],
        };
        let v = serde_json::to_value(&s).expect("serialize");
        // The panel reads these keys off the wire — assert the ACTUAL serialized
        // spelling (camelCase), not the Rust field identifier.
        let obj = v.as_object().expect("object");
        assert!(obj.contains_key("retrying"), "missing `retrying` key: {v}");
        assert!(obj.contains_key("dormant"), "missing `dormant` key: {v}");
        assert!(
            obj.contains_key("connected"),
            "missing `connected` key: {v}"
        );
        assert_eq!(v["retrying"], 4);
        assert_eq!(v["dormant"], 2);
        assert_eq!(v["connected"], 5);
    }

    #[tokio::test]
    async fn snapshot_folds_supervisor_state_counts_into_dial_status() {
        use crate::reconnect_supervisor::PeerStateWire;
        let mut svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        svc.set_supervisor_source(std::sync::Arc::new(FakeSupervisor(vec![
            ([1u8; 32], PeerStateWire::Connected { since_ms: 1 }),
            (
                [2u8; 32],
                PeerStateWire::Retrying {
                    attempt: 0,
                    retry_in_ms: 100,
                },
            ),
            (
                [3u8; 32],
                PeerStateWire::Retrying {
                    attempt: 1,
                    retry_in_ms: 200,
                },
            ),
            ([4u8; 32], PeerStateWire::Dormant { since_ms: 2 }),
        ])));
        let snap = svc.snapshot().await;
        assert_eq!(snap.dial_status.connected, 1);
        assert_eq!(snap.dial_status.retrying, 2);
        assert_eq!(snap.dial_status.dormant, 1);
    }

    #[tokio::test]
    async fn snapshot_without_supervisor_source_reports_zero_counts() {
        // No supervisor source installed → zero state counts, no panic.
        let svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver { records: vec![] }),
            empty_membership(),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        let snap = svc.snapshot().await;
        assert_eq!(snap.dial_status.connected, 0);
        assert_eq!(snap.dial_status.retrying, 0);
        assert_eq!(snap.dial_status.dormant, 0);
    }

    #[tokio::test]
    async fn peer_last_seen_falls_back_to_connected_since() {
        use crate::reconnect_supervisor::PeerStateWire;
        let mut table = std::collections::HashMap::new();
        table.insert([0x55u8; 16], vec!["c1".to_string()]);
        let mut svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver {
                // Record carries NO last_seen_ms.
                records: vec![make_record(0x55, ConnectionMode::NoConnection, None)],
            }),
            std::sync::Arc::new(FakeMembership { table }),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        svc.set_supervisor_source(std::sync::Arc::new(FakeSupervisor(vec![(
            [0x55u8; 32],
            PeerStateWire::Connected { since_ms: 7_000 },
        )])));
        let snap = svc.snapshot().await;
        assert_eq!(snap.peers.len(), 1);
        assert_eq!(
            snap.peers[0].last_seen_ms,
            Some(7_000),
            "record lacked last_seen; must fall back to Connected.since_ms"
        );
    }

    #[tokio::test]
    async fn peer_last_seen_prefers_record_over_connected_since() {
        use crate::reconnect_supervisor::PeerStateWire;
        let mut table = std::collections::HashMap::new();
        table.insert([0x66u8; 16], vec!["c1".to_string()]);
        let mut svc = NetworkHealthService::new(
            std::sync::Arc::new(FakeIroh { ready: true }),
            std::sync::Arc::new(FakePkarr),
            std::sync::Arc::new(FakeResolver {
                // Record already HAS a last_seen_ms — the fallback must not override it.
                records: vec![make_record(0x66, ConnectionMode::NoConnection, Some(3_000))],
            }),
            std::sync::Arc::new(FakeMembership { table }),
            std::sync::Arc::new(EmptyDialSnapshot),
            std::sync::Arc::new(EmptyRelaySnapshot),
        );
        svc.set_supervisor_source(std::sync::Arc::new(FakeSupervisor(vec![(
            [0x66u8; 32],
            PeerStateWire::Connected { since_ms: 9_000 },
        )])));
        let snap = svc.snapshot().await;
        assert_eq!(snap.peers.len(), 1);
        assert_eq!(
            snap.peers[0].last_seen_ms,
            Some(3_000),
            "record's own last_seen must win over the supervisor fallback"
        );
    }

    #[test]
    fn dial_telemetry_records_transition_outcomes() {
        let t = DialTelemetry::new();
        t.record_reconnected([0x11; 32], [0xAA; 16]);
        t.record_retrying([0x22; 32], [0xBB; 16]);
        t.record_dormant([0x33; 32], [0xCC; 16]);
        let s = t.summary();
        assert!(s.recent.iter().any(|h| h.outcome == "reconnected"));
        assert!(s.recent.iter().any(|h| h.outcome == "retrying"));
        assert!(s.recent.iter().any(|h| h.outcome == "dormant"));
        // Transition markers are ring-only: they never touch the dial-outcome
        // counters (attempts/succeeded/failed).
        assert_eq!(s.attempts, 0);
        assert_eq!(s.succeeded, 0);
        assert_eq!(s.failed, 0);
    }

    // ── ZEB-595: PkarrFallbackTelemetry tests ───────────────────────

    #[test]
    fn pkarr_fallback_telemetry_records_short_form_and_order() {
        let t = PkarrFallbackTelemetry::new();
        t.record(&[0x22; 16], &[0x33; 16], PkarrFallbackOutcome::Hit);
        t.record(&[0x44; 16], &[0x55; 16], PkarrFallbackOutcome::Error);
        let events = t.recent();
        assert_eq!(events.len(), 2);
        // Oldest-first, and only the first 4 bytes (8 hex chars) are retained —
        // never the full 16-byte id (short-only redaction invariant).
        assert_eq!(events[0].peer_addr_short, "22222222");
        assert_eq!(events[0].community_id_short, "33333333");
        assert_eq!(events[0].outcome, PkarrFallbackOutcome::Hit);
        assert_eq!(events[1].peer_addr_short, "44444444");
        assert_eq!(events[1].community_id_short, "55555555");
        // Error must stay distinct from Miss — that's the whole point.
        assert_eq!(events[1].outcome, PkarrFallbackOutcome::Error);
    }

    #[test]
    fn pkarr_fallback_ring_evicts_oldest_past_cap() {
        let t = PkarrFallbackTelemetry::new();
        // Push one more than the cap; the oldest must be evicted (FIFO).
        for i in 0..(PKARR_FALLBACK_RING_CAP + 1) {
            let mut peer = [0u8; 16];
            peer[0] = i as u8;
            t.record(&peer, &[0x33; 16], PkarrFallbackOutcome::Hit);
        }
        let events = t.recent();
        assert_eq!(events.len(), PKARR_FALLBACK_RING_CAP, "ring stays at cap");
        let newest_short = hex::encode([PKARR_FALLBACK_RING_CAP as u8, 0, 0, 0]);
        assert_eq!(
            events.last().map(|h| h.peer_addr_short.clone()),
            Some(newest_short),
            "newest entry retained at the back"
        );
        assert!(
            events
                .iter()
                .all(|h| h.peer_addr_short != hex::encode([0u8, 0, 0, 0])),
            "oldest entry evicted"
        );
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
        assert_eq!(snap.schema_version, 4);
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
