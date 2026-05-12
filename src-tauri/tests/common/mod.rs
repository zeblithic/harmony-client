//! Common test helpers for harmony-app integration tests.
//!
//! Each integration test that imports from this module must `mod common;`
//! at its top — Cargo links the file per binary.

#[cfg(feature = "test-fixtures")]
pub mod library_fixtures;

#[cfg(feature = "test-fixtures")]
pub mod profile_fixtures;
