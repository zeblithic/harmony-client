//! ZEB-548 Stage 2: `CanonicalPayload` registrations for the spine's wire
//! types whose certification moved here with their definitions (from
//! harmony-app's `canonical_impls` — the sealed trait's orphan rule requires
//! the impl in the crate defining the type). Kept together in one module so
//! ZEB-220's "audit the certified types in one place" intent survives
//! per-crate. (`friend_token::FriendTokenPayload` stays app-side, registered
//! in harmony-app's `canonical_impls`.)

// Certified via the exported macro — the only supported path (never
// hand-write the sealed-trait impls).
harmony_core_types::impl_canonical!(
    crate::friend_graph::FriendGraph,
    crate::friend_graph::FriendEntry,
    crate::owner_state_crdt::OwnerState,
);
