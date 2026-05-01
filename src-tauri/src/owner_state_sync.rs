//! Owner-state SyncEngine — debounced publishes + Zenoh-agnostic
//! channel surface + replay-protected subscriber merge path
//! (ZEB-215 Sub-A Phase 3a).
//!
//! See `docs/specs/2026-05-01-zeb-215-sub-a-phase3a-sync-design.md`
//! §"Architecture". Channel-based; the Zenoh adapter lives in
//! `event_loop.rs` (Task 19).
