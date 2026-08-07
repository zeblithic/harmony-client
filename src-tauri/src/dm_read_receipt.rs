//! ZEB-214 — opt-in per-DM read receipts (ephemeral, live-only). This module
//! owns the `dm-read-receipt` UI event shape and (in later tasks) the emit
//! decision + packet build. Receipts never touch the outbox/deposit rung.

use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

/// The `dm-read-receipt` UI event name — a peer told us they've read our DM up
/// to a watermark. Emitted from the tunnel ingest path only (receipts are
/// live-only, so there is no deposit/sweeper path to duplicate).
pub(crate) const DM_READ_RECEIPT_EVENT: &str = "dm-read-receipt";

/// Shared payload builder — single source of truth for the event shape.
/// `readUpTo` and `at` are exposed as wall-ms (like `sentAt` on `dm-received`)
/// so the frontend compares them against `Message.timestamp` directly.
/// `readUpTo` = the watermark (which of the viewer's sent messages are seen);
/// `at` = the receipt's send time (the "Seen HH:MM" clock).
pub(crate) fn dm_read_receipt_event_payload(
    space_id: SpaceId,
    from: OwnerAddr,
    read_up_to: &Hlc,
    at_ms: u64,
) -> serde_json::Value {
    serde_json::json!({
        "spaceId": hex::encode(space_id.0),
        "from": hex::encode(from.0),
        "readUpTo": read_up_to.wall_ms,
        "at": at_ms,
    })
}
