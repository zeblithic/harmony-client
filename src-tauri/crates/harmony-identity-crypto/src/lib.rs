//! harmony-identity-crypto — ZEB-548 Stage 1 (PR #3).
//!
//! The identity/vault crypto + at-rest sealing + content-addressing leaf tier,
//! extracted from `harmony-app` so edits here recompile only this crate:
//!
//! - [`identity`] — owner/device identity, vault sealing, keychain + encrypted
//!   file stores, seed persistence.
//! - [`device_dataset_file`] — the ZEB-982 device-dataset at-rest envelope
//!   (sentinel 0x03), keyed off `identity::read_seed_from_disk`.
//! - [`content_store`] — content-addressed (CID) blob store + state-root fetch.
//! - [`avatar_blob_store`] — the on-disk avatar blob cache (CID-verified).
//!
//! Depends only downward: `harmony-core-types` (owner wire types) +
//! `harmony-foundation` (`save_atomically` / `wall_clock_ms` / `profile`), plus
//! the `harmony-*` crypto/content crates and third-party primitives. No Tauri,
//! no back-reference to `harmony-app`; `harmony-app` re-exports these modules so
//! its `crate::identity::*` / `crate::content_store::*` /
//! `crate::device_dataset_file::*` / `crate::avatar_blob_store::*` call sites
//! resolve unchanged.

pub mod avatar_blob_store;
pub mod content_store;
pub mod device_dataset_file;
pub mod identity;
