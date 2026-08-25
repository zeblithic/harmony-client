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

pub mod owner_state_crypto;
pub mod owner_state_types;
