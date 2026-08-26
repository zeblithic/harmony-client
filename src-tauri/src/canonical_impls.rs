//! ZEB-548 Stage 0: `CanonicalPayload` registrations for harmony-app wire types
//! that `harmony_core_types::owner_state_types` used to certify on their behalf,
//! before the sealed trait crossed the crate boundary. Kept together in one
//! module so ZEB-220's "audit the certified types in one place" intent survives
//! per-crate, next to where these types live.

// friend-graph / friend-token sub-CRDTs (ZEB-370 Phase 1) and the top-level
// OwnerState CRDT. Certified via the exported macro — the only supported path
// (never hand-write the sealed-trait impls).
// ZEB-548 Stage 2 (PR #10): friend_graph::{FriendGraph, FriendEntry} and
// owner_state_crdt::OwnerState moved to harmony-transport, and the orphan rule
// moved their registrations with them (harmony-transport's canonical_impls).
// friend_token stays app-side, so its registration stays here. The compile-time
// asserts below still cover ALL the types (the re-export paths resolve).
harmony_core_types::impl_canonical!(crate::friend_token::FriendTokenPayload);

#[cfg(test)]
mod tests {
    use harmony_core_types::owner_state_crypto::CanonicalPayload;

    /// Compile-time guard: the harmony-app wire types certified across the crate
    /// boundary implement `CanonicalPayload`. Moved here — together with the
    /// dm_envelope assertions — from harmony-core-types when the sealed trait was
    /// split out (ZEB-548 Stage 0). If a type's registration is dropped, this
    /// fails to compile with "the trait bound `T: CanonicalPayload` is not
    /// satisfied".
    #[test]
    fn harmony_app_types_implement_canonical_payload() {
        fn assert_canonical<T: CanonicalPayload>() {}
        assert_canonical::<crate::friend_graph::FriendGraph>();
        assert_canonical::<crate::friend_graph::FriendEntry>();
        assert_canonical::<crate::friend_token::FriendTokenPayload>();
        assert_canonical::<crate::owner_state_crdt::OwnerState>();
        // dm_envelope wire types (ZEB-216 Sub-B Phase 1).
        assert_canonical::<crate::dm_envelope::MessagePayload>();
        assert_canonical::<crate::dm_envelope::DmInviteSigned>();
        assert_canonical::<crate::dm_envelope::DmCidNotifySigned>();
        assert_canonical::<crate::dm_envelope::DmAckSigned>();
    }
}
