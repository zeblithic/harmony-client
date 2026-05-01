//! Owner-state CRDT typed CBOR shapes (ZEB-215 Sub-A Phase 2).
//!
//! See specs:
//! - `docs/specs/2026-04-30-zeb-206-nav-tree-design.md` — data model
//! - `docs/specs/2026-04-30-zeb-211-owner-state-encryption-design.md` — canonical CBOR
//!
//! Every type in this module exists on the wire — changes here are
//! wire-format breaking. Field name renames are chosen so all keys at
//! a single nesting level have the same encoded CBOR length, satisfying
//! the precondition documented on `crate::owner_state_crypto::canonical_cbor_encode`.
//!
//! Phase 3 (Zenoh sync) and Phase 4 (IPC) consume these types; this
//! module has no I/O of its own.

#![allow(dead_code)] // Skeleton; tasks 2-9 fill in the public surface.
