//! Owner-state CRDT merge semantics (ZEB-215 Sub-A Phase 2).
//!
//! See specs:
//! - `docs/specs/2026-04-30-zeb-206-nav-tree-design.md` §"CRDT convergence
//!   semantics" — dedupe rules, tombstones-vs-leaves, dependent-record
//!   canonicalization, InboxEntry idempotency, OutboxEntry persistent log.
//!
//! Pure in-memory CRDT layer. Phase 3 wires this into harmony-content
//! CAS + Zenoh transport.

#![allow(dead_code)] // Skeleton; tasks 10-15 fill in the merge engine.
#![allow(unused_imports)]

use crate::owner_state_types as types;
