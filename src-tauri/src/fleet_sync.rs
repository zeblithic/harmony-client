//! Generic per-owner replicated-dataset sync engine (ZEB-417 SP1).
//! (module-level doc — full engine arrives in later tasks)

use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::Hlc;
use harmony_content::cid::ContentId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Engine-local root-publish envelope. Superset of legacy RootPublishPayload:
/// `seen` is skip-if-empty so an empty-seen envelope encodes byte-identically.
/// All three keys (rc/at/sn) are 2 chars (same-length-keys canonical precondition).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetRootPublish {
    #[serde(rename = "rc")]
    pub root_cid: ContentId,
    #[serde(rename = "at")]
    pub at: Hlc,
    #[serde(rename = "sn", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub seen: BTreeMap<String, Hlc>,
}

// ZEB-220 sealed CanonicalPayload registration. `RootPublishPayload` and the
// rest of the Phase 2 wire types are registered via the `impl_canonical!`
// macro in owner_state_types.rs; that macro is module-private, so we register
// `FleetRootPublish` with the same two impls the macro expands to (matching
// the manual `OwnerState` registration at the foot of owner_state_types.rs).
impl CanonicalPayloadSealed for FleetRootPublish {}
impl CanonicalPayload for FleetRootPublish {}

pub const MAX_DEVICES_PER_OWNER: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MergeOutcome {
    pub changed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
    use crate::owner_state_types::{Hlc, RootPublishPayload};
    use harmony_content::cid::{ContentFlags, ContentId};

    fn fixed_cid() -> ContentId {
        ContentId::for_book(
            b"fleet-sync-pin-fixture",
            ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .expect("cid")
    }
    fn fixed_hlc() -> Hlc {
        Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "dev-A".into(),
        }
    }

    #[test]
    fn fleet_root_publish_with_empty_seen_is_byte_identical_to_legacy() {
        let cid = fixed_cid();
        let at = fixed_hlc();
        let legacy_bytes = canonical_cbor_encode(&RootPublishPayload {
            root_cid: cid,
            at: at.clone(),
        })
        .expect("legacy");
        let fleet_bytes = canonical_cbor_encode(&FleetRootPublish {
            root_cid: cid,
            at,
            seen: BTreeMap::new(),
        })
        .expect("fleet");
        assert_eq!(
            fleet_bytes, legacy_bytes,
            "empty-seen FleetRootPublish must equal legacy RootPublishPayload bytes"
        );
    }

    #[test]
    fn fleet_root_publish_with_seen_round_trips() {
        let mut seen = BTreeMap::new();
        seen.insert("dev-B".to_string(), fixed_hlc());
        let env = FleetRootPublish {
            root_cid: fixed_cid(),
            at: fixed_hlc(),
            seen,
        };
        let bytes = canonical_cbor_encode(&env).expect("encode");
        let back: FleetRootPublish = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(back, env);
    }
}
