//! ZEB-368: bridges harmony's iroh `IrohZenohLinkManager` to the vendored
//! `zenoh-link` fork's process-global factory, so the running Zenoh session
//! owns iroh as a first-class unicast transport.
//!
//! Production model is one node per process: the factory + ctx are a global
//! singleton, set once and the ctx swapped on each start/stop (identity switch).
use std::sync::{Arc, Mutex, OnceLock};

use crate::zenoh_iroh_transport::IrohZenohLinkManager;

/// Per-session iroh context the factory reads. Holds harmony's manager (returned
/// to Zenoh for outbound `new_link`) and the accept-loop's receiver (drained by
/// the forwarder into Zenoh's real sender).
pub struct IrohSessionCtx {
    pub manager: Arc<IrohZenohLinkManager>,
    pub new_link_rx: flume::Receiver<zenoh_link::LinkUnicast>,
}

fn ctx_slot() -> &'static Arc<Mutex<Option<IrohSessionCtx>>> {
    static SLOT: OnceLock<Arc<Mutex<Option<IrohSessionCtx>>>> = OnceLock::new();
    SLOT.get_or_init(|| Arc::new(Mutex::new(None)))
}

/// Set by `start_node` before `zenoh::open`. Overwrites any prior session's ctx.
pub fn set_iroh_session_ctx(ctx: IrohSessionCtx) {
    *ctx_slot().lock().expect("iroh ctx slot poisoned") = Some(ctx);
}

/// Cleared by the stop path so a restart re-populates fresh.
pub fn clear_iroh_session_ctx() {
    *ctx_slot().lock().expect("iroh ctx slot poisoned") = None;
}

/// Forward accepted inbound iroh links into Zenoh's transport-accept queue.
/// Exits when Zenoh's receiver is dropped (session closed) — clean across restarts.
async fn forward_inbound_links(
    rx: flume::Receiver<zenoh_link::LinkUnicast>,
    zenoh_sender: zenoh_link::NewLinkChannelSender,
) {
    while let Ok(link) = rx.recv_async().await {
        if zenoh_sender.send_async(link).await.is_err() {
            tracing::debug!("ZEB-368: iroh inbound forwarder stopping (zenoh sender closed)");
            return;
        }
    }
    // rx errored → harmony's accept-loop sender was dropped (node stop). Log the
    // other shutdown edge too so a hung/early-exiting forwarder is diagnosable.
    tracing::debug!("ZEB-368: iroh inbound forwarder stopping (harmony sender closed)");
}

/// Register the global iroh link-manager factory exactly once per process.
/// Idempotent: a second call (node restart) is a no-op — the factory reads the
/// current ctx slot, so restarts just swap the ctx, not the factory.
pub fn ensure_iroh_factory_registered() {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| {
        let factory: zenoh_link::IrohLinkManagerFactory = Arc::new(|zenoh_sender| {
            let guard = ctx_slot().lock().expect("iroh ctx slot poisoned");
            let ctx = guard.as_ref().ok_or_else(|| {
                zenoh_result::zerror!(
                    "ZEB-368: iroh session ctx not set before zenoh::open \
                     (call set_iroh_session_ctx first)"
                )
            })?;
            let manager: zenoh_link::LinkManagerUnicast = ctx.manager.clone();
            let rx = ctx.new_link_rx.clone();
            drop(guard); // release the lock before spawning
            tokio::spawn(forward_inbound_links(rx, zenoh_sender));
            Ok(manager)
        });
        // Our local REGISTERED OnceLock guarantees this closure runs once per
        // process, so register() is expected to succeed. An Err means something
        // else already claimed the global factory slot — unexpected; surface it
        // rather than silently masking a double-registration bug.
        if let Err(e) = zenoh_link::register_iroh_link_manager_factory(factory) {
            tracing::warn!("ZEB-368: unexpected iroh factory registration failure: {e}");
        }
    });
}

/// ZEB-620: recency-ordered node-ids for boot-time reconnect seeding. Every
/// distinct peer the resolver knows (minus self), newest routing record first
/// (by `effective_announced_at_ms`, ties broken by node-id for determinism).
/// Same-node-id records under different owners collapse to one entry keyed on
/// the freshest.
///
/// This replaces ZEB-368's `iroh_connect_locators` (which injected `iroh/<hex>`
/// strings into zenoh's static `connect/endpoints`): boot peers now enter the
/// reconnect supervisor as `NewPeer` kicks via
/// [`seed_boot_peers_into_supervisor`], so it returns raw node-ids (the kick
/// key), not locator strings.
pub fn boot_seed_node_ids_by_recency(
    resolver: &crate::reachability_resolver::ReachabilityResolver,
    self_node_id: &[u8; 32],
) -> Vec<[u8; 32]> {
    use std::collections::HashMap;
    // Keep the freshest effective_announced_at_ms per node-id (a peer may be
    // announced under multiple owners; recency is per-device, not per-owner).
    // ZEB-702: enumerate the DIAL view (`list_dialable_peers`, freshest across
    // durable/pkarr/fleet) rather than `list_active_peers` (durable/pkarr only)
    // so a fleet-slot-only sibling — a SAS-paired cert-only butler — is
    // re-dialed at every boot. The runtime kick gate already dials off the
    // freshest view; this aligns the boot seed with it. Recency uses the
    // future-skew-clamped `effective_announced_at_ms` so a bogus future-dated
    // record cannot permanently dominate the seed order.
    let mut newest: HashMap<[u8; 32], u64> = HashMap::new();
    for (_owner, entry) in resolver.list_dialable_peers() {
        let nid = entry.payload.iroh_node_id;
        if &nid == self_node_id {
            continue;
        }
        newest
            .entry(nid)
            .and_modify(|at| *at = (*at).max(entry.effective_announced_at_ms))
            .or_insert(entry.effective_announced_at_ms);
    }
    let mut ordered: Vec<([u8; 32], u64)> = newest.into_iter().collect();
    // Newest first; deterministic tie-break on node-id so the seed order is
    // stable across runs (the HashMap iteration order is not).
    ordered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ordered.into_iter().map(|(nid, _)| nid).collect()
}

/// ZEB-620: seed the reconnect supervisor with every known peer at boot. Each
/// peer (recency-ordered, newest first) is kicked [`ReconnectTrigger::NewPeer`],
/// so the supervisor arms its reconnect ladder for it — the successor to
/// ZEB-368's static `connect/endpoints` seed. Returns the seeded node-ids (in
/// kick order) for logging/tests.
///
/// The kicks land in the supervisor's coalescing dirty set, which drains
/// unordered; the recency ordering is therefore best-effort at the seed layer
/// (its realized effect on dial order under the concurrency cap is nil at fleet
/// scale — see the PR body's documented deviation). Ordering is verified at this
/// helper's boundary via [`boot_seed_node_ids_by_recency`].
pub fn seed_boot_peers_into_supervisor(
    resolver: &crate::reachability_resolver::ReachabilityResolver,
    self_node_id: &[u8; 32],
    handle: &crate::reconnect_supervisor::SupervisorHandle,
) -> Vec<[u8; 32]> {
    let ordered = boot_seed_node_ids_by_recency(resolver, self_node_id);
    for nid in &ordered {
        handle.kick(*nid, crate::reconnect_supervisor::ReconnectTrigger::NewPeer);
    }
    ordered
}

/// ZEB-931: bind a joined community's reachability bindings into the admission
/// oracle before the boot-seed kick. The oracle is installed in `event_loop::run`
/// *after* the resolver was populated in `start_node`, so those boot-time binds
/// were dropped (no-op against a `None` oracle); without this backfill a
/// router-mode node finds its boot-seeded peers unbound, [`admit`] fails open,
/// and it dials the full persisted roster instead of ~degree ring neighbors
/// (the R4 fan-out storm). Each item is one TTL-fresh reachability binding
/// `(community, actor, iroh_node_id, device)` so the boot-seed kicks classify
/// against real bindings. Returns the number of bindings applied (for the boot log).
///
/// Takes already-resolved bindings rather than the address book itself, so this
/// transport-layer helper names no community wire type (ZEB-990 spine cut): the
/// community-tier book-walk — reading each community's TTL-fresh rows and dropping
/// relay rows, which carry no dialable node-id — lives with its sole caller in
/// `event_loop`, which already depends on the community layer.
///
/// `is_enrolled(community, actor, device)` gates each row against **current**
/// materialized membership — the same `device_is_enrolled` check every other
/// routing-ingest path applies (BOOT-PROBE 10, live `ingest_verified_row`).
/// Rows were enrollment-gated when ingested, but membership can change afterward
/// and the book retains a row until its TTL, so re-checking here keeps the
/// backfill from re-binding a stale row (a departed member or a retired device).
/// Binding a stale row could never *admit* it (admission also requires the key
/// in the controller's current admitted set), but it could transiently mis-park
/// a current member mid device-rotation; the gate removes that edge and keeps
/// this path consistent with its siblings.
///
/// Peer mode never calls this — the caller gates on
/// [`crate::admission_oracle::AdmissionOracle::enabled`], keeping peer-mode boot
/// byte-identical to pre-R4.
///
/// [`admit`]: crate::admission_oracle::AdmissionOracle::admit
pub fn backfill_admission_oracle_from_reachability(
    oracle: &crate::admission_oracle::AdmissionOracle,
    bindings: impl IntoIterator<
        Item = (
            crate::owner_state_types::SpaceId,   // community id
            crate::owner_state_types::OwnerAddr, // actor
            [u8; 32],                            // iroh_node_id
            [u8; 32],                            // enrolled device key
        ),
    >,
    is_enrolled: impl Fn(
        &crate::owner_state_types::SpaceId,
        &crate::owner_state_types::OwnerAddr,
        &[u8; 32],
    ) -> bool,
) -> usize {
    let mut bound = 0usize;
    for (community, actor, iroh_node_id, device) in bindings {
        if is_enrolled(&community, &actor, &device) {
            oracle.bind(actor.0, iroh_node_id, device);
            bound += 1;
        }
    }
    bound
}

/// Build the `iroh/<hex>` listener locator for this node — adding it to
/// `listen/endpoints` forces Zenoh to invoke the factory at `zenoh::open`, which
/// starts the inbound forwarder even on inbound-only / no-known-peer nodes.
pub fn iroh_listen_locator(self_node_id: &[u8; 32]) -> String {
    format!("iroh/{}", hex::encode(self_node_id))
}

/// MERGE `self_loc` into Zenoh's existing `listen/endpoints`, preserving every
/// listener already configured (e.g. the default peer `tcp/[::]:0`) — never
/// overwriting them. `Config::insert_json5` replaces the path with no merge, so we
/// read the current value back (`Config::get_json("listen/endpoints")`), append our
/// locator, and write the union.
///
/// `current_json` is that read-back value (`None` if unreadable). `listen/endpoints`
/// may be the flat array form (`["tcp/[::]:0"]`) or the per-mode object form
/// (`{"router": [...], "peer": [...]}`); for the object form we append under `mode`
/// — the session's own mode (ZEB-912: endpoints under a non-matching key are
/// silently ignored by zenoh's ModeDependentValue resolution, so a router-mode
/// session with the locator under "peer" would never instantiate the iroh
/// factory) — or to the flat list for the array form, and dedupe. Falls back to
/// `["tcp/[::]:0", self_loc]` if the value can't be parsed, so the default LAN
/// listener is preserved even on the error path. (CodeRabbit + Qodo, PR #188.)
pub fn merge_iroh_listen_endpoints(
    current_json: Option<&str>,
    self_loc: &str,
    mode: &str,
) -> String {
    use serde_json::Value;
    let fallback = || format!("[\"tcp/[::]:0\", \"{self_loc}\"]");
    let append_if_missing = |arr: &mut Vec<Value>| {
        if !arr.iter().any(|e| e.as_str() == Some(self_loc)) {
            arr.push(Value::String(self_loc.to_string()));
        }
    };
    let Some(cur) = current_json else {
        return fallback();
    };
    let Ok(mut v) = serde_json::from_str::<Value>(cur) else {
        return fallback();
    };
    match &mut v {
        // Flat array form: append to the single shared listener list.
        Value::Array(arr) => append_if_missing(arr),
        // Per-mode object form: append under the session's own mode; create it
        // if absent.
        Value::Object(map) => match map
            .entry(mode.to_string())
            .or_insert_with(|| Value::Array(vec![]))
        {
            Value::Array(arr) => append_if_missing(arr),
            _ => return fallback(),
        },
        _ => return fallback(),
    }
    serde_json::to_string(&v).unwrap_or_else(|_| fallback())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn self_loc() -> String {
        iroh_listen_locator(&[0xABu8; 32])
    }

    #[test]
    fn merge_into_default_object_preserves_all_and_appends_peer() {
        let loc = self_loc();
        // Zenoh's Config::default() shape for listen/endpoints.
        let cur = r#"{"router":["tcp/[::]:7447"],"peer":["tcp/[::]:0"]}"#;
        let merged = merge_iroh_listen_endpoints(Some(cur), &loc, "peer");
        let v: serde_json::Value = serde_json::from_str(&merged).expect("valid JSON");
        let peer = v["peer"].as_array().expect("peer array");
        // Default peer TCP listener survives (LAN transport intact)…
        assert!(
            peer.iter().any(|e| e == "tcp/[::]:0"),
            "peer keeps tcp: {merged}"
        );
        // …router listener untouched…
        assert_eq!(
            v["router"][0], "tcp/[::]:7447",
            "router preserved: {merged}"
        );
        // …and the iroh locator is appended to peer.
        assert!(peer.iter().any(|e| e == &loc), "peer gains iroh: {merged}");
    }

    #[test]
    fn merge_into_flat_array_appends_and_preserves() {
        let loc = self_loc();
        let merged = merge_iroh_listen_endpoints(Some(r#"["tcp/[::]:0"]"#), &loc, "peer");
        let arr: Vec<String> = serde_json::from_str(&merged).expect("valid JSON array");
        assert!(arr.iter().any(|e| e == "tcp/[::]:0"), "keeps tcp: {merged}");
        assert!(arr.iter().any(|e| e == &loc), "adds iroh: {merged}");
    }

    #[test]
    fn merge_is_idempotent_no_duplicate_iroh() {
        let loc = self_loc();
        let cur = format!(r#"{{"peer":["tcp/[::]:0","{loc}"]}}"#);
        let merged = merge_iroh_listen_endpoints(Some(&cur), &loc, "peer");
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        let count = v["peer"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| *e == &loc)
            .count();
        assert_eq!(count, 1, "no duplicate iroh locator: {merged}");
    }

    /// ZEB-912: under a router-mode session (HARMONY_ZENOH_MODE=router), the
    /// object form must gain the locator under "router" — endpoints appended
    /// under a key that doesn't match the session's own mode are silently
    /// ignored by zenoh's ModeDependentValue resolution.
    #[test]
    fn merge_object_form_router_mode_appends_router_key() {
        let loc = self_loc();
        let cur = r#"{"router":["tcp/[::]:7447"],"peer":["tcp/[::]:0"]}"#;
        let merged = merge_iroh_listen_endpoints(Some(cur), &loc, "router");
        let v: serde_json::Value = serde_json::from_str(&merged).expect("valid JSON");
        let router = v["router"].as_array().expect("router array");
        assert!(
            router.iter().any(|e| e == &loc),
            "router gains iroh: {merged}"
        );
        assert!(
            router.iter().any(|e| e == "tcp/[::]:7447"),
            "router keeps tcp: {merged}"
        );
        let peer = v["peer"].as_array().expect("peer array");
        assert!(
            !peer.iter().any(|e| e == &loc),
            "peer list must NOT gain the locator in router mode: {merged}"
        );
    }

    #[test]
    fn merge_unreadable_or_garbage_falls_back_to_tcp_plus_iroh() {
        let loc = self_loc();
        for cur in [None, Some("not json"), Some("42")] {
            let merged = merge_iroh_listen_endpoints(cur, &loc, "peer");
            assert!(
                merged.contains("tcp/[::]:0"),
                "fallback keeps tcp: {merged}"
            );
            assert!(merged.contains(&loc), "fallback adds iroh: {merged}");
        }
    }

    // ZEB-702: the boot-seed enumeration must include fleet-slot-only siblings so
    // a SAS-paired cert-only butler is re-dialed at every boot. Before the fix it
    // enumerated `list_active_peers()` (durable_preferred → durable/pkarr only),
    // dropping FleetSibling entries; now it enumerates `list_dialable_peers()`
    // (freshest across all three slots). Recency ordering + self-exclusion
    // preserved.
    #[test]
    fn boot_seed_includes_fleet_only_recency_ordered_self_excluded() {
        use crate::owner_state_types::{Hlc, OwnerAddr};
        use crate::reachability_record::ReachabilityAnnouncePayload;
        use crate::reachability_resolver::{ReachabilityResolver, ReachabilitySource};

        fn payload(node: u8, announced_at_ms: u64) -> ReachabilityAnnouncePayload {
            ReachabilityAnnouncePayload {
                iroh_node_id: [node; 32],
                home_relay_url: "https://derp.example/".into(),
                direct_addresses: vec![],
                announced_at_ms,
                identity_signature: [0; 64],
                butler_set: Vec::new(),
                bs_at: 0,
            }
        }
        fn hlc(wall_ms: u64) -> Hlc {
            Hlc {
                wall_ms,
                logical: 0,
                device_id: String::new(),
            }
        }

        let r = ReachabilityResolver::new();
        let owner = OwnerAddr([0x51; 16]);
        let self_node = [0xEE; 32];

        // Two fleet-slot-only siblings (older then fresher). durable_preferred
        // would drop both, so the pre-ZEB-702 boot seed never re-dialed them.
        r.update_with_source(
            owner,
            payload(0xB2, 3_000),
            hlc(3_000),
            ReachabilitySource::FleetSibling,
        );
        r.update_with_source(
            owner,
            payload(0xC3, 5_000),
            hlc(5_000),
            ReachabilitySource::FleetSibling,
        );
        // A self-node fleet entry — must be filtered out of the seed.
        let mut self_payload = payload(0, 9_000);
        self_payload.iroh_node_id = self_node;
        r.update_with_source(
            owner,
            self_payload,
            hlc(9_000),
            ReachabilitySource::FleetSibling,
        );

        let seeded = boot_seed_node_ids_by_recency(&r, &self_node);

        assert!(!seeded.contains(&self_node), "self-node excluded from seed");
        assert_eq!(
            seeded,
            vec![[0xC3; 32], [0xB2; 32]],
            "fleet-only siblings seeded, newest effective_announced_at_ms first"
        );
    }

    /// ZEB-931: the boot backfill binds every joined community's reachability
    /// rows into the admission oracle BEFORE the boot-seed, so a router-mode
    /// node classifies its kicks against real bindings instead of failing open
    /// and over-dialing the full roster. Mirrors
    /// `seed_from_pkarr_some_binds_none_fails_open`: an unbound node fails open
    /// (dialed); after the backfill a ring neighbor is admitted and a
    /// non-neighbor is denied (parked Dormant at the dial-dispatch point).
    ///
    /// Feeds resolved reachability bindings directly — the community book-walk
    /// that produces them lives with the caller in `event_loop` (ZEB-990 spine
    /// cut). Bindings: A1 an enrolled ring neighbor, B2 an enrolled non-neighbor,
    /// C3 a departed member whose still-TTL-fresh row must NOT re-bind.
    #[test]
    fn backfill_binds_reachability_rows_so_router_mode_classifies() {
        use crate::admission_oracle::AdmissionOracle;
        use crate::owner_state_types::{OwnerAddr, SpaceId};
        use std::collections::BTreeSet;

        // (community, actor, iroh_node_id, enrolled device key)
        let community = SpaceId([0x77; 16]);
        let bindings = vec![
            (community, OwnerAddr([0x01; 16]), [0x0A; 32], [0xA1; 32]), // ring neighbor (enrolled)
            (community, OwnerAddr([0x02; 16]), [0x0B; 32], [0xB2; 32]), // non-neighbor (enrolled)
            (community, OwnerAddr([0x03; 16]), [0x0C; 32], [0xC3; 32]), // departed member (NOT enrolled)
        ];

        let oracle = AdmissionOracle::new(true); // router mode

        // Pre-backfill: no node-id is bound -> unknown -> fail open (dialed).
        for n in [0x0Au8, 0x0B, 0x0C] {
            assert!(oracle.admit(&[n; 32]), "pre-backfill unknown fails open");
        }

        // The same `device_is_enrolled` gate every sibling ingest path applies:
        // C3's actor has departed (or its device was retired), so its stale row
        // must NOT be re-bound even though it is still TTL-fresh in the book.
        let enrolled: BTreeSet<[u8; 32]> = BTreeSet::from([[0xA1; 32], [0xB2; 32]]);
        let bound = backfill_admission_oracle_from_reachability(
            &oracle,
            bindings,
            |_cid, _actor, device| enrolled.contains(device),
        );
        assert_eq!(
            bound, 2,
            "only the two enrolled rows bound; departed row skipped"
        );

        // The controller publishes the ring-neighbor union — here just device A1.
        oracle.publish_admitted(BTreeSet::from([[0xA1u8; 32]]));

        // Node A is bound to an admitted key -> admitted; node B is bound to a
        // non-admitted key -> denied (parked Dormant at dispatch, not dialed).
        assert!(oracle.admit(&[0x0A; 32]), "backfilled neighbor admitted");
        assert!(
            !oracle.admit(&[0x0B; 32]),
            "backfilled non-neighbor denied -> parked, not over-dialed"
        );
        // Node C was never bound (gate skipped the departed row) -> unknown ->
        // fail open: the stale row cannot mis-park it, and it certainly cannot
        // be admitted (its key is not in the admitted set either way).
        assert!(
            oracle.admit(&[0x0C; 32]),
            "departed row skipped -> unbound -> fail open, not mis-parked"
        );
    }
}
