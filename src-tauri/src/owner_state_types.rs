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

use serde::{Deserialize, Serialize};

/// Hybrid Logical Clock.
///
/// Wire format (locked): CBOR map with single-char field names `w` / `l`
/// / `d` so all three keys encode to the same length (CBOR text(1) =
/// 2 bytes per key). Without this, `wall_ms` (7) / `logical` (7) /
/// `device_id` (9) would mix encoded lengths 8/8/10 and silently
/// violate the canonical-CBOR precondition. See PR #72 round 3 for
/// the rationale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hlc {
    #[serde(rename = "w")]
    pub wall_ms: u64,
    #[serde(rename = "l")]
    pub logical: u32,
    #[serde(rename = "d")]
    pub device_id: String,
}

impl Hlc {
    /// Lexicographic ordering on `(wall_ms, logical, device_id)`. See
    /// ZEB-211 spec §"Definition of strictly newer".
    pub fn is_strictly_newer_than(&self, other: &Hlc) -> bool {
        (self.wall_ms, self.logical, self.device_id.as_str())
            > (other.wall_ms, other.logical, other.device_id.as_str())
    }
}

#[cfg(test)]
mod hlc_tests {
    use super::*;

    #[test]
    fn hlc_strictly_newer_lexicographic() {
        let a = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "alice".into(),
        };
        let b = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "alice".into(),
        };
        assert!(!a.is_strictly_newer_than(&b));
        assert!(!b.is_strictly_newer_than(&a));

        let later_wall = Hlc {
            wall_ms: 101,
            ..a.clone()
        };
        assert!(later_wall.is_strictly_newer_than(&a));

        let later_logical = Hlc {
            logical: 1,
            ..a.clone()
        };
        assert!(later_logical.is_strictly_newer_than(&a));

        let later_device = Hlc {
            device_id: "bob".into(),
            ..a.clone()
        };
        assert!(later_device.is_strictly_newer_than(&a));
    }
}
