//! ZEB-262 Phase 4 Task 9: discriminant-based dispatch for inbound
//! Reticulum unicast packets. Peeks `packet[0]` and routes:
//!   `0x10` → [`crate::community_invite::handle_unicast`]
//!   else  → fall through to the existing DM dispatch (caller decides)
//!
//! Adding the new branch in a tight wrapper avoids refactoring DM
//! dispatch in this PR. The DM path's existing 0x01-0x03 handling +
//! unknown-discriminant logging are preserved unchanged.
//!
//! Discriminant assignment per spec §"Wire format":
//!   - `0x01-0x03` — DM packets (Path B)
//!   - `0x10-0x1F` — community packets (this module's surface)
//!   - `0x20+`     — reserved for Sub-D directory packets

/// Returns `true` if the packet was claimed by this dispatcher (and
/// dispatched into `community_invite::handle_unicast`), `false` if the
/// caller should fall through to DM dispatch.
///
/// The caller (event_loop's `UnicastReceived` arm) wires this in
/// front of the existing DM dispatch. On `false`, the existing DM
/// `try_lock` chain runs unchanged.
///
/// `community_registry` is `Option` so this dispatcher composes
/// gracefully with the no-owner-identity startup phase: if the
/// registry isn't constructed yet (no owner loaded), we drop the
/// 0x10 packet and warn — same shape as the dm_outbox-missing branch
/// in event_loop.
pub async fn try_dispatch_community<H: crate::community_invite::AppHandleEmit>(
    community_registry: Option<&std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>>,
    dm_outbox: &std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>,
    crdt_state: &std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    packet_bytes: &[u8],
    app: Option<&H>,
) -> bool {
    let disc = match packet_bytes.first() {
        Some(b) => *b,
        None => return false, // empty packet — let DM dispatch decide.
    };
    if disc != 0x10 {
        return false;
    }
    let registry = match community_registry {
        Some(r) => r,
        None => {
            // No community runtime — drop. (Owner identity not loaded
            // yet, or registry torn down during shutdown.)
            tracing::warn!(
                "received community_invite packet (disc 0x10) but community_registry is unset; dropping"
            );
            return true; // claimed (and dropped — don't fall through).
        }
    };
    // Errors are handled inside handle_unicast (warn-log + emit
    // degraded event); we deliberately discard the Result here so the
    // event loop's drain doesn't propagate per-packet rejections.
    let _ = crate::community_invite::handle_unicast(
        registry,
        dm_outbox,
        crdt_state,
        packet_bytes.to_vec(),
        app,
    )
    .await;
    true
}
