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

/// Build the `"iroh/<hex>"` connect-locator strings for every distinct peer the
/// resolver knows (minus self). Used for static outbound seeding (Task 3, later).
pub fn iroh_connect_locators(
    resolver: &crate::reachability_resolver::ReachabilityResolver,
    self_node_id: &[u8; 32],
) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (_owner, payload) in resolver.list_active_peers() {
        let nid = payload.iroh_node_id;
        if &nid == self_node_id {
            continue;
        }
        if seen.insert(nid) {
            out.push(format!("iroh/{}", hex::encode(nid)));
        }
    }
    out
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
/// (`{"router": [...], "peer": [...]}`); we append to the `peer` list (we always run
/// in peer mode) for the object form, or to the flat list for the array form, and
/// dedupe. Falls back to `["tcp/[::]:0", self_loc]` if the value can't be parsed, so
/// the default LAN listener is preserved even on the error path. (CodeRabbit + Qodo,
/// PR #188.)
pub fn merge_iroh_listen_endpoints(current_json: Option<&str>, self_loc: &str) -> String {
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
        // Per-mode object form: append to `peer` (our mode); create it if absent.
        Value::Object(map) => match map.entry("peer").or_insert_with(|| Value::Array(vec![])) {
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
        let merged = merge_iroh_listen_endpoints(Some(cur), &loc);
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
        let merged = merge_iroh_listen_endpoints(Some(r#"["tcp/[::]:0"]"#), &loc);
        let arr: Vec<String> = serde_json::from_str(&merged).expect("valid JSON array");
        assert!(arr.iter().any(|e| e == "tcp/[::]:0"), "keeps tcp: {merged}");
        assert!(arr.iter().any(|e| e == &loc), "adds iroh: {merged}");
    }

    #[test]
    fn merge_is_idempotent_no_duplicate_iroh() {
        let loc = self_loc();
        let cur = format!(r#"{{"peer":["tcp/[::]:0","{loc}"]}}"#);
        let merged = merge_iroh_listen_endpoints(Some(&cur), &loc);
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        let count = v["peer"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| *e == &loc)
            .count();
        assert_eq!(count, 1, "no duplicate iroh locator: {merged}");
    }

    #[test]
    fn merge_unreadable_or_garbage_falls_back_to_tcp_plus_iroh() {
        let loc = self_loc();
        for cur in [None, Some("not json"), Some("42")] {
            let merged = merge_iroh_listen_endpoints(cur, &loc);
            assert!(
                merged.contains("tcp/[::]:0"),
                "fallback keeps tcp: {merged}"
            );
            assert!(merged.contains(&loc), "fallback adds iroh: {merged}");
        }
    }
}
