//! harmony-core-types — the shared foundation crate for the harmony-client
//! workspace (ZEB-548 Stage 0).
//!
//! Contains the pure-data owner-state wire vocabulary (`owner_state_types`)
//! and the canonical-CBOR crypto primitives (`owner_state_crypto`), extracted
//! verbatim from `harmony-app` so every other cluster can depend on them
//! without pulling in the monolith. No I/O, no Tauri, no back-reference to
//! `harmony-app`.
//!
//! `CanonicalPayload` (ZEB-220) stays sealed-by-macro: the `impl_canonical!`
//! macro is the only supported way to certify a type, and it is re-exported at
//! the crate root so feature crates register their own wire types against the
//! trait defined here (each crate's impls audit locally, next to their types).

// ZEB-548 Stage 2 (PR #1): two owner-cert/owner-state leaves pulled down here as
// spine-extraction prep. Both are pure derivations over the owner vocabulary
// already homed in this crate — `enrollment_verify` routes `harmony_owner`
// cert-issuer policy, `revoked_device_projection` aggregates revoked device keys
// by `OwnerAddr` — so core-types is their natural floor. They are depended on
// across the (still-monolithic) transport/dm/owner-fleet spine; landing them
// below it now means those edges already point downward when the spine extracts.
pub mod enrollment_verify;
pub mod owner_state_crypto;
pub mod owner_state_types;
pub mod revoked_device_projection;
