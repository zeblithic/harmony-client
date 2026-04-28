//! Persistence and IPC types for the owner-binding registry.
//!
//! Layered alongside `crate::identity` (per-device transport identity); does
//! not modify it. See `docs/specs/2026-04-28-zeb-170-track-b-devices-panel-v1-design.md`.

use serde::{Deserialize, Serialize};

/// Wire-format view of the owner identity + bound devices, mirrored to JS.
///
/// `canBackUp` reflects whether the master seed is still on this device:
/// `true` after a fresh mint, `false` after a future "Wipe master from
/// device" action. v1 does not ship the wipe; the field is here so the
/// panel renders the degraded state correctly when it does land.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OwnerStateView {
    pub owner_id: String,
    pub owner_display_name: String,
    pub devices: Vec<DeviceView>,
    pub can_back_up: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DeviceView {
    pub device_id: String,
    pub display_name: String,
    pub is_this_device: bool,
    pub trust_decision: TrustDecisionView,
    pub enrolled_at: u64,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TrustDecisionView {
    pub kind: TrustKind,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
// `camelCase` does NOT lowercase single-word PascalCase variants (e.g. "Full" stays "Full").
// Use `lowercase` to produce the conventional JSON discriminant form ("full" / "provisional" /
// "refused") that the TypeScript consumer does strict equality against.
#[serde(rename_all = "lowercase")]
pub enum TrustKind {
    Full,
    Provisional,
    Refused,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_serialize_with_camelcase() {
        let view = OwnerStateView {
            owner_id: "owner-hex".into(),
            owner_display_name: "zeblith".into(),
            devices: vec![DeviceView {
                device_id: "device-hex".into(),
                display_name: "KRILE".into(),
                is_this_device: true,
                trust_decision: TrustDecisionView { kind: TrustKind::Full, reason: None },
                enrolled_at: 1_700_000_000,
                fingerprint: "3e2f·7a91".into(),
            }],
            can_back_up: true,
        };
        let json = serde_json::to_string(&view).unwrap();
        // The wire format MUST be camelCase — JS depends on this.
        assert!(json.contains("\"ownerId\""), "expected ownerId, got {json}");
        assert!(json.contains("\"canBackUp\""), "expected canBackUp, got {json}");
        assert!(json.contains("\"isThisDevice\""), "expected isThisDevice, got {json}");
        assert!(json.contains("\"trustDecision\""), "expected trustDecision, got {json}");
        assert!(!json.contains("owner_id"), "snake_case must not leak: {json}");
        // TrustKind must serialize as lowercase — camelCase does NOT lowercase single-word variants.
        assert!(json.contains("\"full\""), "expected lowercase \"full\" on wire, got {json}");
        assert!(!json.contains("\"Full\""), "PascalCase \"Full\" must not appear on wire: {json}");
    }
}
