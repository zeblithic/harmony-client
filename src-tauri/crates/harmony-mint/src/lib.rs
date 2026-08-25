//! harmony-mint — ZEB-548 Stage 1 (PR #6).
//!
//! The Mint personal-finance backend: the pure SQLite/ledger logic ([`mint`]) and
//! the owner-scoped sync engine ([`mint_sync`] + [`mint_sync_types`] /
//! [`mint_sync_persist`]), extracted from `harmony-app`. A leaf: it depends only
//! downward on `harmony-core-types` (`owner_state_crypto` / `owner_state_types`),
//! `harmony-foundation` (`clock_trust` / `hlc_adopt_floor` / `node_event_sink` /
//! `republish`), and `harmony-identity-crypto` (`content_store` /
//! `device_dataset_file`). No Tauri.
//!
//! The Mint *commands* — the 12 `#[tauri::command]` wrappers that need
//! `AppHandle` / `NodeState` / `mint_db_handle` — stay in `harmony-app` (its
//! `mint` module), calling the pure ledger functions re-exported from here. The
//! module bodies below are unchanged from their `harmony-app` form: their leaf
//! dependencies are re-exported under the same `crate::…` paths they use
//! internally, mirroring how `harmony-app` re-exports them. `harmony-app` in turn
//! re-exports these modules so its `crate::mint::*` / `crate::mint_sync::*` call
//! sites resolve unchanged.

pub use harmony_core_types::{owner_state_crypto, owner_state_types};
pub use harmony_foundation::{clock_trust, hlc_adopt_floor, node_event_sink};
pub use harmony_identity_crypto::{content_store, device_dataset_file};

/// `harmony-app` re-exports `RepublishDirty` from its `fleet_sync` module; mirror
/// that path here so `mint_sync`'s `crate::fleet_sync::RepublishDirty` impl
/// resolves to the foundation trait without editing its body.
pub mod fleet_sync {
    pub use harmony_foundation::republish::RepublishDirty;
}

pub mod mint;
pub mod mint_sync;
pub mod mint_sync_persist;
pub mod mint_sync_types;
