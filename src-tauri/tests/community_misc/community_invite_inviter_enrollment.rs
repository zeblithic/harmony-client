//! ZEB-497: unit coverage for `verify_inviter_enrollment` — the inviter's
//! EnrollmentCert is cryptographically bound to the InviteToken on the
//! community redeem path. Fixtures use `mint_test_owner` (matched device_key +
//! cert); see pkarr_iroh_redeem_full_integration.rs for the same pattern.

use ed25519_dalek::Signer;
use harmony_app::community_invite::{
    canonical_invite_token_bytes, verify_inviter_enrollment, CommunityInvitePayload,
    CommunityInviteVerifyError, InviteEpochSnapshot, InviteToken, MaterializedCommunityState,
    RedeemInviteError, RedeemInviteErrorCode,
};
use harmony_app::community_membership::mint_test_owner;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

const NOW_SECS: u64 = 1_700_000_500; // within mint_test_owner's cert validity

/// Build an invite-only payload whose InviteToken is signed by `signer` and
/// whose `inviter` field is `inviter_addr`. `cert` rides in inviter_enrollment.
fn invite_only_payload(
    inviter_addr: OwnerAddr,
    signer: &ed25519_dalek::SigningKey,
    cert: harmony_owner::certs::EnrollmentCert,
) -> CommunityInvitePayload {
    let unsigned = InviteToken {
        inviter: inviter_addr,
        invitee_hint: None,
        minted_at: Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "d".into(),
        },
        expires_at: None,
        sig: [0u8; 64],
    };
    let bytes = canonical_invite_token_bytes(&unsigned).expect("canonical bytes");
    let sig: [u8; 64] = signer.sign(&bytes).to_bytes();
    let token = InviteToken { sig, ..unsigned };
    CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id: SpaceId([0x11; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: Vec::new(),
            sealed_epoch_keys: Vec::new(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: inviter_addr,
        community_name: "T".into(),
        is_invite_only: true,
        expires_at: None,
        invite_token: Some(token),
        admin_bootstrap: None,
        inviter_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: Some(cert),
        untargeted_decrypt_key: None,
    }
}

#[test]
fn valid_inviter_enrollment_passes() {
    let inviter = mint_test_owner(0x42);
    let payload = invite_only_payload(inviter.owner, &inviter.device_key, inviter.cert.clone());
    assert!(verify_inviter_enrollment(&payload, NOW_SECS).is_ok());
}

#[test]
fn forged_token_sig_rejected() {
    let inviter = mint_test_owner(0x42);
    let wrong = mint_test_owner(0x07); // different device key signs the token
    let payload = invite_only_payload(inviter.owner, &wrong.device_key, inviter.cert.clone());
    assert_eq!(
        verify_inviter_enrollment(&payload, NOW_SECS),
        Err(CommunityInviteVerifyError::InviteTokenSigInvalid)
    );
}

/// ZEB-892 (C2): the redeem path maps `verify_inviter_enrollment` failures
/// per-variant via `From<CommunityInviteVerifyError>`. A forged/tampered token
/// (`InviteTokenSigInvalid`) must surface as `invite_verify_failed`, NOT the
/// misleading `inviter_enrollment_invalid` — #641 hardcoded the latter at the
/// production site, leaving the per-variant `From` dead. This drives the REAL
/// verify function through the exact production mapping.
#[test]
fn forged_token_maps_to_invite_verify_failed_not_enrollment() {
    let inviter = mint_test_owner(0x42);
    let wrong = mint_test_owner(0x07);
    let payload = invite_only_payload(inviter.owner, &wrong.device_key, inviter.cert.clone());
    let err = verify_inviter_enrollment(&payload, NOW_SECS).expect_err("forged token must reject");
    let mapped = RedeemInviteError::from(err);
    assert_eq!(
        mapped.code,
        RedeemInviteErrorCode::InviteVerifyFailed,
        "a forged token is a verify failure, not an enrollment-cert problem"
    );
}

#[test]
fn owner_mismatch_rejected() {
    let inviter = mint_test_owner(0x42);
    // `other` is a cert for a different owner. The token says inviter=inviter.owner
    // and is signed by inviter.device_key, but the cert in inviter_enrollment
    // belongs to `other`, so the owner-binding check must reject.
    let other = mint_test_owner(0x07);
    let payload = invite_only_payload(inviter.owner, &inviter.device_key, other.cert.clone());
    assert_eq!(
        verify_inviter_enrollment(&payload, NOW_SECS),
        Err(CommunityInviteVerifyError::InviterEnrollmentOwnerMismatch)
    );
}

#[test]
fn tampered_cert_rejected() {
    let inviter = mint_test_owner(0x42);
    let mut cert = inviter.cert.clone();
    cert.signature[0] ^= 0x01; // break the master signature
    let payload = invite_only_payload(inviter.owner, &inviter.device_key, cert);
    assert_eq!(
        verify_inviter_enrollment(&payload, NOW_SECS),
        Err(CommunityInviteVerifyError::InviterEnrollmentCertInvalid)
    );
}

#[test]
fn expired_cert_rejected() {
    // mint_test_owner sets expires_at: None (never expires). Mint a fresh cert
    // for the SAME owner/device with a PAST expiry, re-signing via the same
    // sign_master path mint_test_owner uses — so the cert carries a VALID master
    // signature and is rejected purely on expiry (cert.verify's now_secs > exp
    // branch), not on a broken signature (distinct from tampered_cert_rejected).
    use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};
    let seed: u8 = 0x42;
    let master_sk = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
    let master_bundle = PubKeyBundle {
        classical: ClassicalKeys {
            ed25519_verify: master_sk.verifying_key().to_bytes(),
            x25519_pub: [0u8; 32],
        },
        post_quantum: None,
    };
    let device_sk = ed25519_dalek::SigningKey::from_bytes(&[seed ^ 0xFF; 32]);
    let device_bundle = PubKeyBundle {
        classical: ClassicalKeys {
            ed25519_verify: device_sk.verifying_key().to_bytes(),
            x25519_pub: [0u8; 32],
        },
        post_quantum: None,
    };
    let device_id = device_bundle.identity_hash();
    let expired_cert = harmony_owner::certs::EnrollmentCert::sign_master(
        &master_sk,
        master_bundle,
        device_id,
        device_bundle,
        1_700_000_000,
        Some(NOW_SECS - 1), // already expired at NOW_SECS
    )
    .expect("sign_master expired cert");
    // Sanity: the cert is well-formed at an earlier time, so the only thing
    // failing at NOW_SECS is the expiry, not the signature.
    assert!(expired_cert.verify(1_700_000_000).is_ok());

    let owner = OwnerAddr(expired_cert.owner_id);
    let payload = invite_only_payload(owner, &device_sk, expired_cert);
    assert_eq!(
        verify_inviter_enrollment(&payload, NOW_SECS),
        Err(CommunityInviteVerifyError::InviterEnrollmentCertInvalid)
    );
}

#[test]
fn non_invite_only_short_circuits() {
    let inviter = mint_test_owner(0x42);
    let mut payload = invite_only_payload(inviter.owner, &inviter.device_key, inviter.cert.clone());
    payload.is_invite_only = false;
    payload.inviter_enrollment = None;
    payload.invite_token = None;
    assert!(verify_inviter_enrollment(&payload, NOW_SECS).is_ok());
}

#[test]
fn missing_inviter_enrollment_rejected() {
    let inviter = mint_test_owner(0x42);
    let mut payload = invite_only_payload(inviter.owner, &inviter.device_key, inviter.cert.clone());
    payload.inviter_enrollment = None;
    assert_eq!(
        verify_inviter_enrollment(&payload, NOW_SECS),
        Err(CommunityInviteVerifyError::InviterEnrollmentCertInvalid)
    );
}
