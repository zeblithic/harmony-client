//! ZEB-262 Phase 4 Task 9: discriminant-based dispatch for inbound
//! Reticulum unicast packets. Peeks `packet[0]` and routes:
//!   `0x10` → [`crate::community_invite::handle_unicast`]
//!   else  → fall through to the existing DM dispatch (caller decides)
//!
//! Adding the new branch in a tight wrapper avoids refactoring DM
//! dispatch in this PR. The DM path's existing 0x01-0x03 handling +
//! unknown-discriminant logging are preserved unchanged.
//!
//! Discriminant assignment per spec §"Wire format":
//!   - `0x01-0x03` — DM packets (Path B)
//!   - `0x10-0x1F` — community packets (this module's surface)
//!   - `0x20+`     — reserved for Sub-D directory packets

/// Returns `true` if the packet was claimed by this dispatcher (and
/// dispatched into `community_invite::handle_unicast`), `false` if the
/// caller should fall through to DM dispatch.
///
/// The caller (event_loop's `UnicastReceived` arm) wires this in
/// front of the existing DM dispatch. On `false`, the existing DM
/// `try_lock` chain runs unchanged.
///
/// `community_registry` is `Option` so this dispatcher composes
/// gracefully with the no-owner-identity startup phase: if the
/// registry isn't constructed yet (no owner loaded), we drop the
/// 0x10 packet and warn — same shape as the dm_outbox-missing branch
/// in event_loop.
pub async fn try_dispatch_community<H: crate::community_invite::AppHandleEmit>(
    community_registry: Option<&std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>>,
    dm_outbox: &std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>,
    crdt_state: &std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    packet_bytes: &[u8],
    app: Option<&H>,
) -> bool {
    let disc = match packet_bytes.first() {
        Some(b) => *b,
        None => return false, // empty packet — let DM dispatch decide.
    };
    if disc != 0x10 {
        return false;
    }
    let registry = match community_registry {
        Some(r) => r,
        None => {
            // No community runtime — drop. (Owner identity not loaded
            // yet, or registry torn down during shutdown.)
            tracing::warn!(
                "received community_invite packet (disc 0x10) but community_registry is unset; dropping"
            );
            return true; // claimed (and dropped — don't fall through).
        }
    };
    // Errors are handled inside handle_unicast (warn-log + emit
    // degraded event); we deliberately discard the Result here so the
    // event loop's drain doesn't propagate per-packet rejections.
    let _ = crate::community_invite::handle_unicast(
        registry,
        dm_outbox,
        crdt_state,
        packet_bytes.to_vec(),
        app,
    )
    .await;
    true
}

#[cfg(test)]
mod tests {
    //! Discriminant pre-dispatch coverage for `try_dispatch_community`.
    //!
    //! Asserts the contract of the receive-side wrapper independent of
    //! `community_invite::handle_unicast`'s internal verification logic:
    //!   - empty packet — `false` (caller falls through).
    //!   - 0x01 (DM) — `false` (caller falls through).
    //!   - 0xFF (unknown) — `false` (caller falls through; unknown-disc
    //!     logging is the DM dispatch's responsibility).
    //!   - 0x10 + `None` — `true` (claimed + dropped silently).
    //!   - 0x10 + `Some` — `true` (claimed; underlying decode failure
    //!     surfaces only as a warn-log inside handle_unicast and is
    //!     intentionally swallowed so the event loop drain doesn't
    //!     propagate per-packet rejections — Task 9 contract).
    //!
    //! The 0x10+`Some` test reaches into `handle_unicast`, which will
    //! fail at `decode_packet` (the `[0x10, 0xff]` payload is far
    //! shorter than the 64-byte sig trailer) — but
    //! `try_dispatch_community` MUST still return `true` because the
    //! discriminant matched.
    use super::*;
    use crate::community_state_sync::{
        CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver, DEFAULT_DEBOUNCE_MS,
    };
    use crate::content_store::{ContentStore, RuntimeContentStore};
    use crate::dm_outbox::DmOutbox;
    use crate::owner_state_crdt::OwnerState;
    use crate::owner_state_types::{DeviceIdentityHash, OwnerAddr};
    use std::sync::Arc;
    use tokio::sync::{mpsc, Mutex as TokioMutex};

    struct NopResolver;
    #[async_trait::async_trait]
    impl IdentityResolver for NopResolver {
        async fn resolve(&self, _: &OwnerAddr) -> Option<[u8; 64]> {
            None
        }
    }

    /// Build a synthetic `DmOutbox` mirroring `dm_outbox::tests`
    /// patterns. Uses a fresh `PrivateIdentity::from_seed` (NOT clone —
    /// `PrivateIdentity` is `ZeroizeOnDrop` and deliberately non-Clone).
    fn make_outbox() -> Arc<TokioMutex<DmOutbox>> {
        // Derive signing_key + device hash from a SINGLE PrivateIdentity so the
        // synthetic transport identity is self-consistent (matches the same-
        // identity invariant DmOutbox enforces between signing_key and
        // private_identity; mismatched fixtures could fail future ack/counter-
        // sign paths for fixture reasons rather than routing behavior).
        let private_identity = harmony_identity::PrivateIdentity::from_seed(&[0x55; 32]);
        let priv_bytes = private_identity.to_private_bytes();
        let mut ed_seed = [0u8; 32];
        ed_seed.copy_from_slice(&priv_bytes[32..64]);
        let signing_key = Arc::new(ed25519_dalek::SigningKey::from_bytes(&ed_seed));
        let device_hash = DeviceIdentityHash(private_identity.identity.address_hash);
        let private_identity = Arc::new(private_identity);
        // ZEB-339: use new_synthetic because OwnerAddr([0x01;16]) is an
        // arbitrary address not matching any mint_test_owner cert. These
        // tests exercise inbound packet routing, not community-signing.
        let test_owner = crate::community_membership::mint_test_owner(0xDE);
        let community_signing_key = Arc::new(ed25519_dalek::SigningKey::from_bytes(
            &test_owner.device_key.to_bytes(),
        ));
        let enrollment_cert = test_owner.cert;
        Arc::new(TokioMutex::new(DmOutbox::new_synthetic(
            "dev".into(),
            OwnerAddr([0x01; 16]),
            device_hash,
            signing_key,
            private_identity,
            community_signing_key,
            enrollment_cert,
        )))
    }

    fn make_crdt() -> Arc<TokioMutex<OwnerState>> {
        Arc::new(TokioMutex::new(OwnerState::default()))
    }

    /// Minimal registry with `NopResolver`, no engines spawned.
    /// Mirrors the fixture pattern in
    /// `tests/community_sync_registry_unit.rs`.
    fn make_registry() -> Arc<CommunitySyncRegistry> {
        let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);
        let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
            cas_op_tx,
            std::time::Duration::from_millis(1000),
        ));
        let dir = tempfile::tempdir().expect("tempdir");
        Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
            device_id: "dev".into(),
            content_store: cs,
            identity_resolver: Arc::new(NopResolver),
            identity_dir: dir.path().to_path_buf(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            error_tx: None,
            delta_tx: None,
            self_owner: OwnerAddr([0x01; 16]),
            signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])),
            crdt_state: None,
            nav_emitter: None,
        }))
    }

    #[tokio::test]
    async fn empty_packet_returns_false() {
        let dm_outbox = make_outbox();
        let crdt_state = make_crdt();
        let claimed = try_dispatch_community::<()>(None, &dm_outbox, &crdt_state, &[], None).await;
        assert!(
            !claimed,
            "empty packet must fall through to DM dispatch (returns false)"
        );
    }

    #[tokio::test]
    async fn dm_discriminant_0x01_returns_false() {
        let dm_outbox = make_outbox();
        let crdt_state = make_crdt();
        let claimed = try_dispatch_community::<()>(
            None,
            &dm_outbox,
            &crdt_state,
            &[0x01, 0xde, 0xad, 0xbe, 0xef],
            None,
        )
        .await;
        assert!(
            !claimed,
            "DM discriminant 0x01 must fall through to DM dispatch (returns false)"
        );
    }

    #[tokio::test]
    async fn unknown_discriminant_returns_false() {
        let dm_outbox = make_outbox();
        let crdt_state = make_crdt();
        let claimed =
            try_dispatch_community::<()>(None, &dm_outbox, &crdt_state, &[0xff, 0x00], None).await;
        assert!(
            !claimed,
            "unknown discriminant 0xFF must fall through (returns false); unknown-disc logging is the DM dispatch's responsibility"
        );
    }

    #[tokio::test]
    async fn community_disc_with_no_registry_claims_and_drops() {
        let dm_outbox = make_outbox();
        let crdt_state = make_crdt();
        let claimed =
            try_dispatch_community::<()>(None, &dm_outbox, &crdt_state, &[0x10, 0xaa, 0xbb], None)
                .await;
        assert!(
            claimed,
            "0x10 with community_registry=None must be claimed (returns true) and dropped silently"
        );
    }

    #[tokio::test]
    async fn community_disc_with_registry_invokes_handle_unicast_and_claims() {
        let registry = make_registry();
        let dm_outbox = make_outbox();
        let crdt_state = make_crdt();
        // `[0x10, 0xff]` is far too short for the 64-byte sig trailer
        // that `decode_packet` requires; `handle_unicast` will fail
        // with `TooShortForSignature` and warn-log internally. The
        // wrapper deliberately swallows that Err — what we're asserting
        // here is the contract: discriminant matched + registry
        // present → claimed (true), regardless of inner success/fail.
        let claimed = try_dispatch_community::<()>(
            Some(&registry),
            &dm_outbox,
            &crdt_state,
            &[0x10, 0xff],
            None,
        )
        .await;
        assert!(
            claimed,
            "0x10 with Some(registry) must be claimed (returns true) even when handle_unicast's decode fails — per Task 9 contract"
        );
    }
}
