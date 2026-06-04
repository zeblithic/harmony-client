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
            break;
        }
    }
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
        // Ignore "already registered" — within one process this runs once.
        let _ = zenoh_link::register_iroh_link_manager_factory(factory);
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
