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

/// Build the `listen/endpoints` JSON array that ADDS our own `iroh/<hex>` listener
/// locator (which forces the factory to run at `zenoh::open`, starting the inbound
/// forwarder even on inbound-only nodes) while PRESERVING Zenoh's default peer TCP
/// listener `tcp/[::]:0`.
///
/// `Config::insert_json5` OVERWRITES the path — it does not merge — so emitting an
/// iroh-only array here would silently drop the default TCP listener and kill the
/// existing LAN zenoh transport. Keep both. (CodeRabbit, PR #188.)
pub fn iroh_listen_endpoints_json(self_node_id: &[u8; 32]) -> String {
    format!("[\"tcp/[::]:0\", \"iroh/{}\"]", hex::encode(self_node_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_endpoints_preserve_default_tcp_listener() {
        let nid = [0xABu8; 32];
        let json = iroh_listen_endpoints_json(&nid);
        // Must keep Zenoh's default peer TCP listener so the LAN transport survives…
        assert!(
            json.contains("tcp/[::]:0"),
            "listen endpoints must preserve the default TCP peer listener: {json}"
        );
        // …and add our own iroh listener locator to trigger the factory at open.
        assert!(
            json.contains(&format!("iroh/{}", hex::encode(nid))),
            "listen endpoints must include the self iroh locator: {json}"
        );
        // Valid JSON array of exactly those two endpoints.
        let parsed: Vec<String> = serde_json::from_str(&json).expect("valid JSON array");
        assert_eq!(parsed.len(), 2, "exactly tcp + iroh: {json}");
    }
}
