//! Type definitions for Mint Phase 2 sync.
//! No business logic — just data, errors, and (de)serialization.

use serde::{Deserialize, Serialize};

pub const MINT_SCHEMA_VERSION: u32 = 1;
pub const LOCAL_MAX_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MintSnapshot {
    pub schema_version: u32,
    pub accounts: Vec<AccountRow>,
    pub transactions: Vec<TransactionRow>,
    pub settings: Vec<SettingRow>,
    /// ISO 8601 snapshot timestamp. Debugging/observability only — NOT used in
    /// merge logic. Per-row LWW uses each row's own `updated_at`.
    pub captured_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRow {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionRow {
    pub id: String,
    pub transaction_date: String,
    pub amount: String,
    pub currency: String,
    pub account_id: String,
    pub description: String,
    pub metadata: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingRow {
    pub key: String,
    pub value: String,
    pub updated_at: String,
}

/// Wire envelope published on the Zenoh topic. Two-char serde rename keys
/// satisfy the same-length-keys precondition that `canonical_cbor_encode`
/// established in owner-state Phase 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MintRootPublishPayload {
    #[serde(rename = "rc")]
    pub root_cid: crate::owner_state_types::ContentId,
    #[serde(rename = "at")]
    pub at: crate::owner_state_types::Hlc,
}

/// On-disk persisted state for the mint sync engine.
/// Stored at `<app_data_dir>/mint/mint_sync_state.cbor`.
///
/// `replay_tracker` mirrors the `BTreeMap<String, Hlc>` representation used
/// by `owner_state_persist::save_replay`/`load_replay` — `RootReplayTracker`
/// itself is not serializable, so we persist its inner map directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MintSyncState {
    pub schema_version: u32,
    pub replay_tracker: std::collections::BTreeMap<String, crate::owner_state_types::Hlc>,
    pub account_deletion_floor: std::collections::HashMap<String, String>,
}

impl Default for MintSyncState {
    fn default() -> Self {
        Self {
            schema_version: MINT_SCHEMA_VERSION,
            replay_tracker: std::collections::BTreeMap::new(),
            account_deletion_floor: std::collections::HashMap::new(),
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum MintSyncError {
    #[error("mint sync IO: {0}")]
    Io(#[from] std::io::Error),
    #[error("mint sync cbor encode/decode: {0}")]
    Cbor(String),
    #[error("mint sync sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("mint sync crypto: {0}")]
    Crypto(String),
    #[error("mint sync content store: blob {0:?} missing")]
    MissingBlob(crate::owner_state_types::ContentId),
    #[error("mint sync schema version too new: remote={remote}, local_max={local_max}")]
    SchemaTooNew { remote: u32, local_max: u32 },
    #[error("mint sync other: {0}")]
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciborium::{from_reader, into_writer};

    #[test]
    fn mint_snapshot_round_trips_through_cbor() {
        let snap = MintSnapshot {
            schema_version: MINT_SCHEMA_VERSION,
            accounts: vec![AccountRow {
                id: "acct-1".into(),
                name: "Chase".into(),
                created_at: "2026-05-01T00:00:00Z".into(),
                updated_at: "2026-05-01T00:00:00Z".into(),
            }],
            transactions: vec![TransactionRow {
                id: "tx-1".into(),
                transaction_date: "2026-05-01".into(),
                amount: "-12.34".into(),
                currency: "USD".into(),
                account_id: "acct-1".into(),
                description: "Coffee".into(),
                metadata: Some(r#"{"tag":"travel"}"#.into()),
                created_at: "2026-05-01T00:00:00Z".into(),
                updated_at: "2026-05-01T00:00:00Z".into(),
                deleted_at: None,
            }],
            settings: vec![SettingRow {
                key: "default_currency".into(),
                value: "USD".into(),
                updated_at: "2026-05-01T00:00:00Z".into(),
            }],
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        let mut bytes = Vec::new();
        into_writer(&snap, &mut bytes).unwrap();
        let decoded: MintSnapshot = from_reader(&bytes[..]).unwrap();
        assert_eq!(snap, decoded);
    }

    #[test]
    fn tombstone_round_trips() {
        let row = TransactionRow {
            id: "tx-1".into(),
            transaction_date: "2026-05-01".into(),
            amount: "0".into(),
            currency: "USD".into(),
            account_id: "acct-1".into(),
            description: "x".into(),
            metadata: None,
            created_at: "2026-05-01T00:00:00Z".into(),
            updated_at: "2026-05-02T00:00:00Z".into(),
            deleted_at: Some("2026-05-02T00:00:00Z".into()),
        };
        let mut bytes = Vec::new();
        into_writer(&row, &mut bytes).unwrap();
        let decoded: TransactionRow = from_reader(&bytes[..]).unwrap();
        assert_eq!(row, decoded);
    }

    #[test]
    fn root_publish_payload_round_trips() {
        let payload = MintRootPublishPayload {
            root_cid: crate::owner_state_types::ContentId::from_bytes([1u8; 32]),
            at: crate::owner_state_types::Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 7,
                device_id: "aabbccdd-eeff-0011-2233-445566778899".into(),
            },
        };
        let mut bytes = Vec::new();
        into_writer(&payload, &mut bytes).unwrap();
        let decoded: MintRootPublishPayload = from_reader(&bytes[..]).unwrap();
        assert_eq!(payload, decoded);
    }
}
