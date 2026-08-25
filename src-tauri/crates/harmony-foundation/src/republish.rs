//! ZEB-702 (Component B): object-safe dataset-republish seam.
//!
//! A transport-epoch listener holds a `Vec<Arc<dyn RepublishDirty>>` over every
//! owner-scoped dataset engine and nudges each on the transport up-edge, closing
//! the late-joiner hole where a link that forms after the last publish would
//! otherwise carry nothing until the next local mutation. `republish_dirty`
//! delegates to each engine's own debounced `notify_dirty()`, so the re-offer
//! rides the existing debounce + publish path — byte-identical content,
//! idempotent on receivers (LWW/HLC merge).
//!
//! The trait lives here (a leaf crate) so both `harmony-app`'s engines
//! (`FleetSyncEngine<S>`, `owner_state_sync::SyncEngine`) and the extracted
//! feature crates (`harmony-mint`'s `MintSyncEngine`) can implement it without a
//! back-dependency on the binary. `harmony-app` re-exports it from `fleet_sync`
//! so its `crate::fleet_sync::RepublishDirty` call sites resolve unchanged.

/// Object-safe seam for re-offering the current dataset root when a transport
/// link forms.
pub trait RepublishDirty: Send + Sync {
    /// Schedule a debounced re-publish of the current dataset root.
    /// Non-blocking; coalesces with any pending dirty state.
    fn republish_dirty(&self);
}
