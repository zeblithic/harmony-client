//! ZEB-442: consolidated identity test harness (ZEB-440 lever 3 — test-binary
//! consolidation).
//!
//! Scope: the **identity / recovery / owner** domain — identity-state recovery, recovery + passphrase-rotate CLIs, pairing, owner lifecycle, keychain isolation, and referral-catalog roundtrip.
//!
//! Each former `tests/*.rs` here compiled as its own integration-test binary,
//! statically re-linking the whole `harmony-app` lib (the link-time multiplier
//! this ticket targets). They are now `#[path]`-included submodules of this one
//! harness binary. Files under a `tests/` subdirectory are NOT auto-compiled as
//! separate binaries, so each builds only via its `mod` declaration below.
//!
//! The shared `tests/common/` helper is declared **once** here as `mod common`;
//! the former per-file `mod common;` becomes `use crate::common;` (loading the
//! same file as a module more than once per binary is a rustc error). Original
//! basenames are preserved so cross-references stay resolvable by name. nextest
//! runs every `#[test]` in its own process, so `#[serial]` isolation is unchanged.
//!
//! Run just this group: `cargo nextest run -E 'binary(identity_tests)'`.
//!
//! ZEB-442/ZEB-428: this harness bundles fixtures-only modules (and, for
//! identity, the keychain-refusal guard test), so it requires `--features
//! test-fixtures`. The whole harness is gated at the crate root: feature-off it
//! compiles out to an empty binary, so `cargo test` without the feature can
//! never run these tests against the real OS keychain. Every CI job sets the
//! feature, so this is a no-op there.
#![cfg(feature = "test-fixtures")]

#[path = "common/mod.rs"]
mod common;

#[path = "identity/identity_state_recovery_integration.rs"]
mod identity_state_recovery_integration;
#[path = "identity/keychain_isolation.rs"]
mod keychain_isolation;
#[path = "identity/owner_integration.rs"]
mod owner_integration;
#[path = "identity/pairing_integration.rs"]
mod pairing_integration;
#[path = "identity/recovery_cli_integration.rs"]
mod recovery_cli_integration;
#[path = "identity/referral_catalog_roundtrip_integration.rs"]
mod referral_catalog_roundtrip_integration;
#[path = "identity/rotate_passphrase_cli.rs"]
mod rotate_passphrase_cli;
