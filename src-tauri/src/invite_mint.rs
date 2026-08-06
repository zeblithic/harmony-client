//! ZEB-367 Phase 4: invite-only invite mint primitives.
//!
//! Pure functions (no Tauri/NodeState) that produce the signed + sealed pieces
//! of an invite-only `CommunityInvitePayload`. The verify/redeem counterparts
//! already live in `community_invite`; this is the mint side.

use crate::community_invite::{canonical_invite_token_bytes, InviteToken};
use crate::owner_state_types::{Hlc, OwnerAddr};

/// Mint + sign an `InviteToken` with the enrolled device-#2 key (ZEB-339).
/// The sig commits to (inviter, invitee_hint?, minted_at, expires_at?) via
/// `canonical_invite_token_bytes`.
pub fn mint_invite_token(
    inviter: OwnerAddr,
    invitee_hint: Option<OwnerAddr>,
    minted_at: Hlc,
    expires_at: Option<u64>,
    device2_signing_key: &ed25519_dalek::SigningKey,
) -> Result<InviteToken, String> {
    let mut token = InviteToken {
        inviter,
        invitee_hint,
        minted_at,
        expires_at,
        sig: [0u8; 64],
    };
    let bytes = canonical_invite_token_bytes(&token)
        .map_err(|e| format!("canonical_invite_token_bytes: {e}"))?;
    use ed25519_dalek::Signer;
    token.sig = device2_signing_key.sign(&bytes).to_bytes();
    Ok(token)
}

use crate::dm_signing::seal_to_owner;

/// Who the epoch key is sealed to.
pub enum SealRecipient {
    /// Sealed to a specific invitee's device-#2 X25519 public key (confidential).
    Targeted([u8; 32]),
    /// Sealed to a fresh ephemeral key whose private half ships in the URL
    /// (single-use "controlled open" link).
    Untargeted,
}

pub struct SealedEpochKey {
    /// 92-byte X25519 envelope (32 ephemeral_pub || 12 nonce || 32 ct || 16 tag).
    pub sealed: Vec<u8>,
    /// Untargeted only: the ephemeral X25519 private key the redeemer uses.
    pub untargeted_decrypt_key: Option<[u8; 32]>,
}

pub fn seal_epoch_key(
    epoch_key: &[u8; 32],
    recipient: SealRecipient,
) -> Result<SealedEpochKey, String> {
    match recipient {
        SealRecipient::Targeted(pub_) => {
            let sealed =
                seal_to_owner(&pub_, epoch_key).map_err(|e| format!("seal_to_owner: {e}"))?;
            Ok(SealedEpochKey {
                sealed,
                untargeted_decrypt_key: None,
            })
        }
        SealRecipient::Untargeted => {
            let ephemeral_priv = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
            let ephemeral_pub = x25519_dalek::PublicKey::from(&ephemeral_priv);
            let sealed = seal_to_owner(ephemeral_pub.as_bytes(), epoch_key)
                .map_err(|e| format!("seal_to_owner: {e}"))?;
            Ok(SealedEpochKey {
                sealed,
                untargeted_decrypt_key: Some(ephemeral_priv.to_bytes()),
            })
        }
    }
}

use crate::community_membership::{MembershipEventKind, SignedMembershipEvent};
use crate::owner_state_types::SpaceId;

#[derive(Debug)]
pub enum InviteMintError {
    NoAdminBootstrap,
    /// ZEB-833: the admin bootstrap self-Join selected from the community log
    /// does not pass the joiner's own verify chain (`enrolled_key_from_cert` +
    /// `verify_membership_signer`). Refusing to embed it means the host never
    /// publishes an invite whose `admin_bootstrap` the joiner would
    /// deterministically reject — turning a silent, host-invisible poisoned
    /// invite into a fail-fast host-side error at mint time.
    AdminBootstrapUnverifiable(crate::community_membership::VerifyError),
}
impl std::fmt::Display for InviteMintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAdminBootstrap => write!(f, "admin bootstrap Join not found in community log"),
            Self::AdminBootstrapUnverifiable(e) => write!(
                f,
                "admin bootstrap self-Join failed the joiner's verify chain ({e}) — \
                 refusing to publish an invite the joiner would reject"
            ),
        }
    }
}

/// Find the admin's bootstrap self-Join (kind=Join, actor=admin, no countersig,
/// carries an enrollment cert) in a community's event set. This is what the
/// redeemer pre-inserts so its empty CRDT can verify the admin's publish-back.
///
/// ZEB-833: before returning, the selected event is run through the joiner's
/// EXACT verify chain (`enrolled_key_from_cert` + `verify_membership_signer` —
/// the same checks `community_invite::verify_admin_bootstrap` step 5 runs).
/// Structural selection alone (actor/community/kind/countersig/enrollment-
/// present) trusts that a self-Join sitting in the host's engine was validly
/// signed; nothing re-checked the signature at mint time, so a structurally-
/// qualifying but cryptographically-bad self-Join was silently baked into a
/// poisoned invite that the joiner deterministically rejects with
/// `admin_bootstrap signature verify failed` (after the host has already
/// committed a phantom member + burned the single-use invite — that acceptor-
/// side fallout is ZEB-874). Verifying here turns that silent, host-invisible
/// failure into a fail-fast host-side error at mint time.
pub fn extract_admin_bootstrap(
    events: &[SignedMembershipEvent],
    community_id: SpaceId,
    admin_addr: OwnerAddr,
) -> Result<SignedMembershipEvent, InviteMintError> {
    let selected = events
        .iter()
        .filter(|e| {
            e.actor == admin_addr
                && e.community_id == community_id
                && matches!(e.kind, MembershipEventKind::Join)
                && e.countersig.is_none()
                && e.enrollment.is_some()
        })
        // Deterministic selection: pick the EARLIEST qualifying bootstrap Join by
        // canonical HLC order (wall_ms, logical, device_id), with the event id as a
        // final total-order tiebreaker. A plain `.find()` would surface whichever
        // match happens to come first in iterator order — non-deterministic if the
        // slice ever holds more than one admin bootstrap Join, which would let a
        // non-canonical record get embedded in the minted invite.
        .min_by(|a, b| {
            (a.at.wall_ms, a.at.logical, &a.at.device_id, &a.id).cmp(&(
                b.at.wall_ms,
                b.at.logical,
                &b.at.device_id,
                &b.id,
            ))
        })
        .cloned()
        .ok_or(InviteMintError::NoAdminBootstrap)?;

    // ZEB-833: verify the selected bootstrap through the joiner's exact chain
    // before returning it. Both calls are pure crypto (no I/O) — the same
    // functions the redeemer runs in `verify_admin_bootstrap`.
    let signer = crate::community_membership::enrolled_key_from_cert(&selected)
        .map_err(InviteMintError::AdminBootstrapUnverifiable)?;
    crate::community_membership::verify_membership_signer(&selected, &signer)
        .map_err(InviteMintError::AdminBootstrapUnverifiable)?;

    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_invite::verify_invite_token_sig_device_key;
    use crate::dm_signing::open_from_owner;

    fn hlc() -> Hlc {
        Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "dev2".to_string(),
        }
    }

    /// A validly-signed admin bootstrap self-Join for `owner`: signed by the
    /// owner's enrolled device key and carrying the owner's Master cert, so it
    /// passes the joiner's verify chain (enrolled_key_from_cert +
    /// verify_membership_signer). `wall_ms` uses the cert-era timestamp so the
    /// enrollment cert verifies at the event's own time.
    fn signed_admin_join(
        owner: &crate::community_membership::TestOwner,
        community_id: SpaceId,
        id: [u8; 16],
        wall_ms: u64,
    ) -> SignedMembershipEvent {
        let payload = crate::community_membership::EventPayload {
            id,
            community_id,
            kind: MembershipEventKind::Join,
            actor: owner.owner,
            at: Hlc {
                wall_ms,
                logical: 0,
                device_id: "d".into(),
            },
        };
        let mut ev =
            crate::community_membership::sign_event(&payload, &owner.device_key).expect("sign");
        ev.enrollment = Some(owner.cert.clone());
        ev
    }

    #[test]
    fn minted_token_verifies_against_device_key() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let token = mint_invite_token(OwnerAddr([1u8; 16]), None, hlc(), Some(2_000), &sk).unwrap();
        verify_invite_token_sig_device_key(&token, &sk.verifying_key().to_bytes()).unwrap();
    }

    #[test]
    fn tampered_token_fails_verification() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let mut token = mint_invite_token(OwnerAddr([1u8; 16]), None, hlc(), None, &sk).unwrap();
        token.expires_at = Some(999_999); // not covered by the now-stale sig
        assert!(
            verify_invite_token_sig_device_key(&token, &sk.verifying_key().to_bytes()).is_err()
        );
    }

    #[test]
    fn targeted_seal_round_trips() {
        let priv_ = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
        let pub_ = x25519_dalek::PublicKey::from(&priv_);
        let epoch = [9u8; 32];
        let out = seal_epoch_key(&epoch, SealRecipient::Targeted(*pub_.as_bytes())).unwrap();
        assert!(out.untargeted_decrypt_key.is_none());
        let opened = open_from_owner(&priv_.to_bytes(), &out.sealed).unwrap();
        assert_eq!(opened.as_slice(), &epoch);
    }

    #[test]
    fn untargeted_seal_round_trips_via_url_key() {
        let epoch = [3u8; 32];
        let out = seal_epoch_key(&epoch, SealRecipient::Untargeted).unwrap();
        let key = out
            .untargeted_decrypt_key
            .expect("untargeted returns a key");
        let opened = open_from_owner(&key, &out.sealed).unwrap();
        assert_eq!(opened.as_slice(), &epoch);
    }

    #[test]
    fn extracts_admin_join_with_enrollment() {
        let owner = crate::community_membership::mint_test_owner(0x6E);
        let admin = owner.owner;
        let cid = SpaceId([0x11u8; 16]);
        let admin_join = signed_admin_join(&owner, cid, [1u8; 16], 1_700_000_000_000);
        // A different actor carrying no enrollment must be filtered out before
        // selection (never reaches the ZEB-833 verify guard).
        let other = SignedMembershipEvent {
            actor: OwnerAddr([2u8; 16]),
            enrollment: None,
            ..admin_join.clone()
        };
        let got = extract_admin_bootstrap(&[other, admin_join.clone()], cid, admin).unwrap();
        assert_eq!(got.actor, admin);
        assert!(got.enrollment.is_some());
    }

    #[test]
    fn missing_admin_bootstrap_errors() {
        let r = extract_admin_bootstrap(&[], SpaceId([0u8; 16]), OwnerAddr([0u8; 16]));
        assert!(matches!(r, Err(InviteMintError::NoAdminBootstrap)));
    }

    #[test]
    fn picks_earliest_bootstrap_when_multiple_admin_joins() {
        // Two qualifying admin bootstrap Joins differing only by HLC. Selection
        // must be deterministic (earliest by canonical HLC order), independent of
        // the order they appear in the slice.
        let owner = crate::community_membership::mint_test_owner(0x6E);
        let admin = owner.owner;
        let cid = SpaceId([0x11u8; 16]);
        // Each event is individually signed (the sig covers id + at), so both
        // are valid bootstraps differing only by HLC. extract only verifies the
        // selected (earliest) one, which must still pass the ZEB-833 guard.
        let earlier = signed_admin_join(&owner, cid, [1u8; 16], 1_700_000_010_000);
        let later = signed_admin_join(&owner, cid, [2u8; 16], 1_700_000_020_000);
        // Present in reverse (later first) so a naive `.find()` would pick `later`.
        let got = extract_admin_bootstrap(&[later.clone(), earlier.clone()], cid, admin).unwrap();
        assert_eq!(got.id, earlier.id, "must pick the earliest-HLC bootstrap");
        assert_eq!(got.at.wall_ms, 1_700_000_010_000);
    }

    #[test]
    fn rejects_unverifiable_admin_bootstrap() {
        // ZEB-833: a structurally-qualifying admin bootstrap (right
        // actor/community/kind, no countersig, carries an enrollment cert) but
        // with a TAMPERED signature must be rejected at mint time. The host
        // must never embed a bootstrap the joiner's verify_admin_bootstrap
        // would deterministically reject.
        let owner = crate::community_membership::mint_test_owner(0x6E);
        let cid = SpaceId([0x11u8; 16]);
        let mut ev = signed_admin_join(&owner, cid, [1u8; 16], 1_700_000_000_000);
        ev.sig[0] ^= 0x01; // flip one bit: valid structure, invalid signature
        let r = extract_admin_bootstrap(&[ev], cid, owner.owner);
        assert!(
            matches!(r, Err(InviteMintError::AdminBootstrapUnverifiable(_))),
            "tampered-sig bootstrap must be rejected at mint, got {r:?}"
        );
    }
}
