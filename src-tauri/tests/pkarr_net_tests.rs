//! ZEB-442: consolidated pkarr_net test harness (ZEB-440 lever 3 — test-binary
//! consolidation).
//!
//! Scope: the **pkarr / iroh / network-transport** domain — pkarr publish/resolve + community fallback, identity discovery, invite redemption, iroh key-file fallback + zenoh registration, and two-endpoint network health. (The real-iroh dynamic-dial test stays standalone in `zeb_373_dynamic_dial_integration.rs`; its ZEB-402 `#[ignore]` was removed once ZEB-626 fixed the first-bind stall behind the flake.)
//!
//! Each former `tests/*.rs` here compiled as its own integration-test binary,
//! statically re-linking the whole `harmony-app` lib (the link-time multiplier
//! this ticket targets). They are now `#[path]`-included submodules of this one
//! harness binary. Files under a `tests/` subdirectory are NOT auto-compiled as
//! separate binaries, so each builds only via its `mod` declaration below.
//!
//! Pure move: original basenames are preserved (subdir moved, not renamed) so
//! cross-references from other tests and from production `src/` doc-comments stay
//! resolvable by name. nextest still runs every `#[test]` in its own process, so
//! per-test isolation (and any `#[serial]`/per-test stack-size) is unchanged.
//!
//! Run just this group: `cargo nextest run -E 'binary(pkarr_net_tests)'`.

#[path = "pkarr_net/iroh_key_file_fallback.rs"]
mod iroh_key_file_fallback;
#[path = "pkarr_net/iroh_zenoh_registration_integration.rs"]
mod iroh_zenoh_registration_integration;
#[path = "pkarr_net/network_health_two_endpoint.rs"]
mod network_health_two_endpoint;
#[path = "pkarr_net/pkarr_community_fallback_integration.rs"]
mod pkarr_community_fallback_integration;
#[path = "pkarr_net/pkarr_contexts_fn_integration.rs"]
mod pkarr_contexts_fn_integration;
#[path = "pkarr_net/pkarr_identity_discovery_integration.rs"]
mod pkarr_identity_discovery_integration;
#[path = "pkarr_net/pkarr_invite_redemption_integration.rs"]
mod pkarr_invite_redemption_integration;
#[path = "pkarr_net/pkarr_iroh_redeem_full_integration.rs"]
mod pkarr_iroh_redeem_full_integration;
// ZEB-880 round 2: AVALON-shaped record must survive the REAL pkarr packet
// build (the round-1 CBOR budget was derived against the wrong cap).
#[path = "pkarr_net/zeb880_record_size.rs"]
mod zeb880_record_size;
#[path = "pkarr_net/zeb910_all_slots.rs"]
mod zeb910_all_slots;
#[path = "pkarr_net/zeb918_epoch_rotation.rs"]
mod zeb918_epoch_rotation;
// ZEB-911 slice 2: the witness discovery ladder. Reuses the two-party harness
// above (Alice = admin, Bob = joiner, mock pkarr relay) and adds a third
// party — the witness node — on top of it.
#[path = "pkarr_net/zeb911_witness_redeem.rs"]
mod zeb911_witness_redeem;

// ZEB-1021: shared mock-relay visibility barrier (epoch tolerance window +
// per-attempt fresh resolver). Shared across umbrella binaries via #[path].
#[path = "support/pkarr_visibility.rs"]
mod pkarr_visibility;
