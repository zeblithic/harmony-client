//! ZEB-677 §2: the ONE issuer-policy decision point for peer-presented
//! enrollment/revocation certs.
//!
//! Master-issued certs verify self-contained (as today). Quorum-issued certs
//! verify via depth-1 chain carriage: the presenting peer supplies the
//! Master-issued enrollment certs of the quorum signers, and the crate's
//! `verify_quorum_with_signers` checks the full chain (signer presence,
//! same-owner binding, Master-issued depth-1, signer-cert validity,
//! no backdating, part signatures). This module never re-implements those
//! checks — it routes, maps errors, and recovers the owner's master anchor
//! from the verified bundle (a Quorum cert carries no master pubkey of its
//! own; its signer certs all do, and must agree).
//!
//! Every network verifier seam maps `EnrollmentVerifyError` into its local
//! error type; none may pattern-match `EnrollmentIssuer` directly anymore.

use harmony_owner::certs::{
    EnrollmentCert, EnrollmentIssuer, RevocationCert, RevocationIssuer, RevocationReason,
};
use harmony_owner::OwnerError;

/// Chokepoint verification failure, classified for the three distinctions
/// callers actually make. `Invalid` keeps the crate error for tracing.
#[derive(Debug)]
pub enum EnrollmentVerifyError {
    /// The cert (or a quorum signer's cert) is expired at `now_secs`.
    Expired,
    /// `cert.owner_id` does not match the caller's expected owner.
    OwnerMismatch,
    /// Any other verification failure.
    Invalid(OwnerError),
}

impl std::fmt::Display for EnrollmentVerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Expired => write!(f, "enrollment cert expired"),
            Self::OwnerMismatch => write!(f, "enrollment cert owner mismatch"),
            Self::Invalid(e) => write!(f, "cert invalid: {e}"),
        }
    }
}

/// What a successful enrollment verification yields: the enrolled device
/// key (signature checks) and the owner's master anchor (friend-graph /
/// admission binding). For Master certs the anchor is the cert's own
/// issuer key; for Quorum certs it is the signer certs' common master key.
#[derive(Debug, Clone, Copy)]
pub struct VerifiedEnrollment {
    pub device_ed25519: [u8; 32],
    pub master_ed25519: [u8; 32],
}

fn map_owner_err(e: OwnerError) -> EnrollmentVerifyError {
    match e {
        OwnerError::EnrollmentCertExpired { .. } => EnrollmentVerifyError::Expired,
        other => EnrollmentVerifyError::Invalid(other),
    }
}

/// Verify a peer-presented `EnrollmentCert` of any issuer.
///
/// `signer_certs` is the presented bundle backing a Quorum-issued cert
/// (empty for Master-issued — extra certs are ignored either way).
/// `expected_owner` binds `cert.owner_id` to the caller's claimed owner
/// (checked first, so callers keep today's error split). `now_secs` is
/// Unix SECONDS — millisecond callers must convert (`/ 1000`).
pub fn verify_enrollment_any_issuer(
    cert: &EnrollmentCert,
    signer_certs: &[EnrollmentCert],
    expected_owner: Option<&[u8; 16]>,
    now_secs: u64,
) -> Result<VerifiedEnrollment, EnrollmentVerifyError> {
    if let Some(owner) = expected_owner {
        if cert.owner_id != *owner {
            return Err(EnrollmentVerifyError::OwnerMismatch);
        }
    }
    let master_ed25519 = match &cert.issuer {
        EnrollmentIssuer::Master { master_pubkey } => {
            cert.verify(now_secs).map_err(map_owner_err)?;
            master_pubkey.classical.ed25519_verify
        }
        EnrollmentIssuer::Quorum { signers, .. } => {
            cert.verify_quorum_with_signers(signer_certs, now_secs)
                .map_err(map_owner_err)?;
            // Recover the master anchor from the verified bundle. Each
            // signer cert independently passed verify() above (master
            // pubkey hashes to the shared owner_id), so any one of them
            // carries THE master key — the all-equal check is defense in
            // depth against a crate regression, not a trust decision.
            let mut anchor: Option<[u8; 32]> = None;
            for id in signers {
                let signer_cert = signer_certs.iter().find(|c| c.device_id == *id).ok_or(
                    EnrollmentVerifyError::Invalid(OwnerError::NotEnrolled {
                        owner: cert.owner_id,
                        device: *id,
                    }),
                )?;
                let EnrollmentIssuer::Master { master_pubkey } = &signer_cert.issuer else {
                    return Err(EnrollmentVerifyError::Invalid(
                        OwnerError::InvalidSignature {
                            cert_type: "Enrollment-Quorum-Signer-Not-Master",
                        },
                    ));
                };
                let m = master_pubkey.classical.ed25519_verify;
                if *anchor.get_or_insert(m) != m {
                    return Err(EnrollmentVerifyError::Invalid(
                        OwnerError::InvalidSignature {
                            cert_type: "Enrollment-Quorum-Master-Anchor-Mismatch",
                        },
                    ));
                }
            }
            anchor.ok_or(EnrollmentVerifyError::Invalid(
                OwnerError::InsufficientQuorum { min: 2, got: 0 },
            ))?
        }
    };
    Ok(VerifiedEnrollment {
        device_ed25519: cert.device_pubkeys.classical.ed25519_verify,
        master_ed25519,
    })
}

/// Verify a peer-presented `RevocationCert` of any issuer.
///
/// `target_enrollment` is the revoked device's enrollment cert (needed for
/// the SelfDevice arm's verify key). `signer_certs` backs a Quorum-issued
/// revocation, same depth-1 rules as enrollment. Revocations carry no
/// expiry, so there is no `Expired` mapping here.
pub fn verify_revocation_any_issuer(
    cert: &RevocationCert,
    target_enrollment: &EnrollmentCert,
    signer_certs: &[EnrollmentCert],
    now_secs: u64,
) -> Result<(), EnrollmentVerifyError> {
    match &cert.issuer {
        RevocationIssuer::Master { .. } => {
            cert.verify(None).map_err(EnrollmentVerifyError::Invalid)
        }
        RevocationIssuer::SelfDevice => {
            let vk_bytes = target_enrollment.device_pubkeys.classical.ed25519_verify;
            let vk = ed25519_dalek::VerifyingKey::from_bytes(&vk_bytes).map_err(|_| {
                EnrollmentVerifyError::Invalid(OwnerError::InvalidSignature {
                    cert_type: "Revocation-Self-Target-Key",
                })
            })?;
            cert.verify(Some(&vk))
                .map_err(EnrollmentVerifyError::Invalid)
        }
        RevocationIssuer::Quorum { .. } => cert
            .verify_quorum_with_signers(signer_certs, now_secs)
            .map_err(EnrollmentVerifyError::Invalid),
    }
}

/// Presentation-side bundle assembly (spec §2): the signer certs a device
/// must attach when presenting its own Quorum-issued cert, pulled from the
/// local trust doc by the cert's `issuer.signers` ids. Master-issued certs
/// need no bundle. Missing signer enrollments are skipped — peers will
/// reject the short bundle, which is the honest outcome.
pub fn own_cert_bundle(
    state: &harmony_owner::state::OwnerState,
    cert: &EnrollmentCert,
) -> Vec<EnrollmentCert> {
    match &cert.issuer {
        EnrollmentIssuer::Master { .. } => Vec::new(),
        EnrollmentIssuer::Quorum { signers, .. } => signers
            .iter()
            .filter_map(|id| state.enrollments.get(id).cloned())
            .collect(),
    }
}

/// Depth-1 policy: quorum signers must hold Master-issued enrollments.
///
/// ZEB-548 Stage 2: lives here (the issuer-policy chokepoint) rather than in
/// `owner_quorum_commands`; the command surface re-exports it back (downward),
/// so the spine quorum-sync path no longer reaches up for it.
pub fn is_master_issued(cert: &EnrollmentCert) -> bool {
    matches!(cert.issuer, EnrollmentIssuer::Master { .. })
}

/// Wire → crate reason mapping (ZEB-668 S2 spec §3: three UI reasons; `Other`
/// unused by the UI).
///
/// ZEB-548 Stage 2: lives here (beside the revocation issuer policy) rather
/// than in `owner_commands`; the command surface re-exports it back
/// (downward), so the spine quorum-sync path no longer reaches up for it.
pub fn parse_revoke_reason(reason: &str) -> Result<RevocationReason, String> {
    match reason {
        "decommissioned" => Ok(RevocationReason::Decommissioned),
        "lost" => Ok(RevocationReason::Lost),
        "compromised" => Ok(RevocationReason::Compromised),
        other => Err(format!(
            "invalidReason: expected decommissioned|lost|compromised, got {other:?}"
        )),
    }
}

/// Deterministic quorum-cert worlds for seam tests (ZEB-677). One master,
/// two Master-enrolled signer devices (A, B), and a third device (C)
/// enrolled by A+B quorum — plus the signer-cert bundle peers need.
#[cfg(any(test, feature = "test-fixtures"))]
pub mod quorum_fixtures {
    use harmony_owner::certs::{EnrollmentCert, RevocationCert, RevocationReason};
    use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};

    pub const SIGNER_ISSUED_AT: u64 = 1_700_000_000;
    pub const QUORUM_ISSUED_AT: u64 = 1_700_000_100;
    /// A "now" at which every cert in a fresh world is valid.
    pub const WORLD_NOW: u64 = 1_700_000_200;

    pub struct QuorumWorld {
        pub owner_id: [u8; 16],
        pub master_sk: ed25519_dalek::SigningKey,
        pub master_bundle: PubKeyBundle,
        pub master_ed25519: [u8; 32],
        pub a_sk: ed25519_dalek::SigningKey,
        pub a_cert: EnrollmentCert,
        pub b_sk: ed25519_dalek::SigningKey,
        pub b_cert: EnrollmentCert,
        pub c_sk: ed25519_dalek::SigningKey,
        /// Device C's Quorum-issued enrollment cert (signers: A, B).
        pub c_quorum_cert: EnrollmentCert,
        /// The signer-cert bundle a peer needs to verify `c_quorum_cert`.
        pub bundle: Vec<EnrollmentCert>,
    }

    fn keypair(fill: u8) -> (ed25519_dalek::SigningKey, PubKeyBundle, [u8; 16]) {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[fill; 32]);
        let bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let id = bundle.identity_hash();
        (sk, bundle, id)
    }

    /// ZEB-339 test helper: a realistic owner with an enrolled device key.
    /// Produced by `mint_test_owner`; consumed by membership tests that need
    /// the new `actor = owner_id ≠ address_hash(device_key)` signing model.
    ///
    /// ZEB-548 Stage 2: homed here (beside the quorum worlds) rather than in
    /// `community_membership`, which re-exports it back (downward), so spine
    /// modules' tests no longer reach up for it.
    #[derive(Debug, Clone)]
    pub struct TestOwner {
        pub owner: crate::owner_state_types::OwnerAddr,
        pub device_key: ed25519_dalek::SigningKey,
        pub cert: EnrollmentCert,
    }

    /// ZEB-339 test helper: produce a realistic owner — owner_id (master),
    /// an enrolled device signing key, and a self-minted Master EnrollmentCert
    /// binding them. `seed` makes it deterministic.
    ///
    /// Note on seeds: the master key derives from `[seed; 32]` and the device
    /// key from `[seed ^ 0xFF; 32]`, so seeds `N` and `N ^ 0xFF` share raw key
    /// material (with master/device roles swapped). Use seeds in `0x01..=0xFE`
    /// and avoid pairing `N` with `N ^ 0xFF` in the same test.
    pub fn mint_test_owner(seed: u8) -> TestOwner {
        let master_sk = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        let master_bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: master_sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let owner_id = master_bundle.identity_hash();
        let device_sk = ed25519_dalek::SigningKey::from_bytes(&[seed ^ 0xFF; 32]);
        let device_bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: device_sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let device_id = device_bundle.identity_hash();
        let cert = EnrollmentCert::sign_master(
            &master_sk,
            master_bundle,
            device_id,
            device_bundle,
            1_700_000_000,
            None,
        )
        .expect("sign_master");
        cert.verify(0).expect("self-minted cert verifies");
        TestOwner {
            owner: crate::owner_state_types::OwnerAddr(owner_id),
            device_key: device_sk,
            cert,
        }
    }

    /// Deterministic per `seed`. Key fills are `seed`, `seed+1..3` — pick
    /// world seeds at least 4 apart (and away from other fixtures' seeds)
    /// to avoid key-material collisions.
    pub fn mint_quorum_world(seed: u8) -> QuorumWorld {
        let (master_sk, master_bundle, owner_id) = keypair(seed);
        let (a_sk, a_bundle, a_id) = keypair(seed.wrapping_add(1));
        let (b_sk, b_bundle, b_id) = keypair(seed.wrapping_add(2));
        let (c_sk, c_bundle, c_id) = keypair(seed.wrapping_add(3));

        let a_cert = EnrollmentCert::sign_master(
            &master_sk,
            master_bundle.clone(),
            a_id,
            a_bundle,
            SIGNER_ISSUED_AT,
            None,
        )
        .expect("sign A");
        let b_cert = EnrollmentCert::sign_master(
            &master_sk,
            master_bundle.clone(),
            b_id,
            b_bundle,
            SIGNER_ISSUED_AT,
            None,
        )
        .expect("sign B");

        let payload = EnrollmentCert::quorum_signing_payload_bytes(
            owner_id,
            c_id,
            &c_bundle,
            QUORUM_ISSUED_AT,
            None,
            &[a_id, b_id],
        )
        .expect("quorum payload");
        let sig_a = EnrollmentCert::sign_quorum_part(&a_sk, &payload);
        let sig_b = EnrollmentCert::sign_quorum_part(&b_sk, &payload);
        let c_quorum_cert = EnrollmentCert::assemble_quorum(
            owner_id,
            c_id,
            c_bundle,
            QUORUM_ISSUED_AT,
            None,
            vec![(a_id, sig_a), (b_id, sig_b)],
        )
        .expect("assemble quorum");
        c_quorum_cert
            .verify_quorum_with_signers(&[a_cert.clone(), b_cert.clone()], WORLD_NOW)
            .expect("fresh world verifies");

        QuorumWorld {
            owner_id,
            master_sk,
            master_ed25519: master_bundle.classical.ed25519_verify,
            master_bundle,
            a_sk,
            a_cert: a_cert.clone(),
            b_sk,
            b_cert: b_cert.clone(),
            c_sk,
            c_quorum_cert,
            bundle: vec![a_cert, b_cert],
        }
    }

    /// A+B quorum-revoke `target` (reason: Lost). Verifiable against
    /// `world.bundle`.
    pub fn mint_quorum_revocation(
        world: &QuorumWorld,
        target: [u8; 16],
        issued_at: u64,
    ) -> RevocationCert {
        let a_id = world.a_cert.device_id;
        let b_id = world.b_cert.device_id;
        let payload = RevocationCert::quorum_signing_payload_bytes(
            world.owner_id,
            target,
            issued_at,
            &RevocationReason::Lost,
            &[a_id, b_id],
        )
        .expect("quorum revocation payload");
        let sig_a = RevocationCert::sign_quorum_part(&world.a_sk, &payload);
        let sig_b = RevocationCert::sign_quorum_part(&world.b_sk, &payload);
        RevocationCert::assemble_quorum(
            world.owner_id,
            target,
            issued_at,
            RevocationReason::Lost,
            vec![(a_id, sig_a), (b_id, sig_b)],
        )
        .expect("assemble quorum revocation")
    }
}

#[cfg(test)]
mod tests {
    use super::quorum_fixtures::*;
    use super::*;
    use harmony_owner::certs::RevocationReason;

    #[test]
    fn master_cert_verifies_without_bundle() {
        let world = mint_quorum_world(0x40);
        let v = verify_enrollment_any_issuer(&world.a_cert, &[], Some(&world.owner_id), WORLD_NOW)
            .expect("master cert verifies");
        assert_eq!(v.master_ed25519, world.master_ed25519);
        assert_eq!(
            v.device_ed25519,
            world.a_cert.device_pubkeys.classical.ed25519_verify
        );
    }

    #[test]
    fn quorum_cert_verifies_with_bundle() {
        let world = mint_quorum_world(0x44);
        let v = verify_enrollment_any_issuer(
            &world.c_quorum_cert,
            &world.bundle,
            Some(&world.owner_id),
            WORLD_NOW,
        )
        .expect("quorum cert verifies with bundle");
        assert_eq!(
            v.device_ed25519,
            world.c_quorum_cert.device_pubkeys.classical.ed25519_verify
        );
        // The master anchor is recovered from the signer certs.
        assert_eq!(v.master_ed25519, world.master_ed25519);
    }

    #[test]
    fn quorum_cert_missing_bundle_rejected() {
        let world = mint_quorum_world(0x48);
        assert!(matches!(
            verify_enrollment_any_issuer(&world.c_quorum_cert, &[], None, WORLD_NOW),
            Err(EnrollmentVerifyError::Invalid(_))
        ));
    }

    #[test]
    fn quorum_cert_partial_bundle_rejected() {
        let world = mint_quorum_world(0x4C);
        assert!(matches!(
            verify_enrollment_any_issuer(
                &world.c_quorum_cert,
                std::slice::from_ref(&world.a_cert),
                None,
                WORLD_NOW
            ),
            Err(EnrollmentVerifyError::Invalid(_))
        ));
    }

    #[test]
    fn quorum_cert_wrong_owner_signer_rejected() {
        let world = mint_quorum_world(0x50);
        // Same device key as B, but enrolled under a DIFFERENT owner's
        // master: lookup by device_id succeeds, owner binding must fail.
        let foreign_master = ed25519_dalek::SigningKey::from_bytes(&[0x70; 32]);
        let foreign_bundle = harmony_owner::pubkey_bundle::PubKeyBundle {
            classical: harmony_owner::pubkey_bundle::ClassicalKeys {
                ed25519_verify: foreign_master.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let foreign_b = EnrollmentCert::sign_master(
            &foreign_master,
            foreign_bundle,
            world.b_cert.device_id,
            world.b_cert.device_pubkeys.clone(),
            SIGNER_ISSUED_AT,
            None,
        )
        .expect("foreign B cert");
        assert!(matches!(
            verify_enrollment_any_issuer(
                &world.c_quorum_cert,
                &[world.a_cert.clone(), foreign_b],
                None,
                WORLD_NOW
            ),
            Err(EnrollmentVerifyError::Invalid(_))
        ));
    }

    #[test]
    fn quorum_cert_bad_part_signature_rejected() {
        let world = mint_quorum_world(0x54);
        let a_id = world.a_cert.device_id;
        let b_id = world.b_cert.device_id;
        let payload = EnrollmentCert::quorum_signing_payload_bytes(
            world.owner_id,
            world.c_quorum_cert.device_id,
            &world.c_quorum_cert.device_pubkeys,
            QUORUM_ISSUED_AT,
            None,
            &[a_id, b_id],
        )
        .unwrap();
        let sig_a = EnrollmentCert::sign_quorum_part(&world.a_sk, &payload);
        // B's slot carries A's signature — assembles structurally, must
        // fail full verification.
        let tampered = EnrollmentCert::assemble_quorum(
            world.owner_id,
            world.c_quorum_cert.device_id,
            world.c_quorum_cert.device_pubkeys.clone(),
            QUORUM_ISSUED_AT,
            None,
            vec![(a_id, sig_a.clone()), (b_id, sig_a)],
        )
        .expect("structural assembly succeeds");
        assert!(matches!(
            verify_enrollment_any_issuer(&tampered, &world.bundle, None, WORLD_NOW),
            Err(EnrollmentVerifyError::Invalid(_))
        ));
    }

    #[test]
    fn quorum_signer_cert_not_master_rejected() {
        let world = mint_quorum_world(0x58);
        // Depth-1: a signer cert that is itself Quorum-issued is refused.
        let mut quorum_signer = world.a_cert.clone();
        quorum_signer.issuer = EnrollmentIssuer::Quorum {
            signers: vec![[9u8; 16], [8u8; 16]],
            signatures: vec![vec![0u8; 64], vec![0u8; 64]],
        };
        quorum_signer.signature = Vec::new();
        assert!(matches!(
            verify_enrollment_any_issuer(
                &world.c_quorum_cert,
                &[quorum_signer, world.b_cert.clone()],
                None,
                WORLD_NOW
            ),
            Err(EnrollmentVerifyError::Invalid(_))
        ));
    }

    #[test]
    fn expected_owner_mismatch_rejected() {
        let world = mint_quorum_world(0x5C);
        assert!(matches!(
            verify_enrollment_any_issuer(
                &world.c_quorum_cert,
                &world.bundle,
                Some(&[0xEE; 16]),
                WORLD_NOW
            ),
            Err(EnrollmentVerifyError::OwnerMismatch)
        ));
    }

    #[test]
    fn expired_master_cert_maps_to_expired() {
        let world = mint_quorum_world(0x60);
        let d_sk = ed25519_dalek::SigningKey::from_bytes(&[0x74; 32]);
        let d_bundle = harmony_owner::pubkey_bundle::PubKeyBundle {
            classical: harmony_owner::pubkey_bundle::ClassicalKeys {
                ed25519_verify: d_sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let d_id = d_bundle.identity_hash();
        let expiring = EnrollmentCert::sign_master(
            &world.master_sk,
            world.master_bundle.clone(),
            d_id,
            d_bundle,
            SIGNER_ISSUED_AT,
            Some(SIGNER_ISSUED_AT + 50),
        )
        .expect("sign expiring cert");
        assert!(matches!(
            verify_enrollment_any_issuer(&expiring, &[], None, WORLD_NOW),
            Err(EnrollmentVerifyError::Expired)
        ));
    }

    #[test]
    fn revocation_selfdevice_and_master_verify() {
        let world = mint_quorum_world(0x64);
        let c_id = world.c_quorum_cert.device_id;
        // C self-revokes.
        let self_rev = harmony_owner::certs::RevocationCert::sign_self(
            &world.c_sk,
            world.owner_id,
            c_id,
            WORLD_NOW,
            RevocationReason::Decommissioned,
        )
        .expect("self revocation");
        verify_revocation_any_issuer(&self_rev, &world.c_quorum_cert, &[], WORLD_NOW)
            .expect("self revocation verifies");
        // Master revokes B.
        let master_rev = harmony_owner::certs::RevocationCert::sign_master(
            &world.master_sk,
            world.master_bundle.clone(),
            world.b_cert.device_id,
            WORLD_NOW,
            RevocationReason::Lost,
        )
        .expect("master revocation");
        verify_revocation_any_issuer(&master_rev, &world.b_cert, &[], WORLD_NOW)
            .expect("master revocation verifies");
    }

    #[test]
    fn revocation_quorum_verifies_with_bundle() {
        let world = mint_quorum_world(0x68);
        let c_id = world.c_quorum_cert.device_id;
        let rev = mint_quorum_revocation(&world, c_id, WORLD_NOW);
        verify_revocation_any_issuer(&rev, &world.c_quorum_cert, &world.bundle, WORLD_NOW)
            .expect("quorum revocation verifies with bundle");
        assert!(matches!(
            verify_revocation_any_issuer(&rev, &world.c_quorum_cert, &[], WORLD_NOW),
            Err(EnrollmentVerifyError::Invalid(_))
        ));
    }

    #[test]
    fn own_cert_bundle_collects_signers() {
        let world = mint_quorum_world(0x6C);
        let mut state = harmony_owner::state::OwnerState::new(world.owner_id);
        state
            .add_enrollment(
                world.a_cert.clone(),
                WORLD_NOW,
                harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS,
            )
            .expect("enroll A");
        state
            .add_enrollment(
                world.b_cert.clone(),
                WORLD_NOW,
                harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS,
            )
            .expect("enroll B");
        let bundle = own_cert_bundle(&state, &world.c_quorum_cert);
        assert_eq!(bundle, vec![world.a_cert.clone(), world.b_cert.clone()]);
        assert!(own_cert_bundle(&state, &world.a_cert).is_empty());
    }
}
