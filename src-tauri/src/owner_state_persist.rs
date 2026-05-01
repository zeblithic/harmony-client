//! On-disk persistence for the Phase-2 OwnerState CRDT and the
//! RootReplayTracker (ZEB-215 Sub-A Phase 3a).
//!
//! See `docs/specs/2026-05-01-zeb-215-sub-a-phase3a-sync-design.md`
//! §"Persistence layer". Two files written via atomic-rename + fsync,
//! each prefixed with a 1-byte schema version.
