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
    let mut token = InviteToken { inviter, invitee_hint, minted_at, expires_at, sig: [0u8; 64] };
    let bytes = canonical_invite_token_bytes(&token)
        .map_err(|e| format!("canonical_invite_token_bytes: {e}"))?;
    use ed25519_dalek::Signer;
    token.sig = device2_signing_key.sign(&bytes).to_bytes();
    Ok(token)
}

use crate::dm_signing::{open_from_owner, seal_to_owner};

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
            let sealed = seal_to_owner(&pub_, epoch_key).map_err(|e| format!("seal_to_owner: {e}"))?;
            Ok(SealedEpochKey { sealed, untargeted_decrypt_key: None })
        }
        SealRecipient::Untargeted => {
            let ephemeral_priv = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
            let ephemeral_pub = x25519_dalek::PublicKey::from(&ephemeral_priv);
            let sealed = seal_to_owner(ephemeral_pub.as_bytes(), epoch_key)
                .map_err(|e| format!("seal_to_owner: {e}"))?;
            Ok(SealedEpochKey { sealed, untargeted_decrypt_key: Some(ephemeral_priv.to_bytes()) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_invite::verify_invite_token_sig_device_key;

    fn hlc() -> Hlc { Hlc { wall_ms: 1_000, logical: 0, device_id: "dev2".to_string() } }

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
        assert!(verify_invite_token_sig_device_key(&token, &sk.verifying_key().to_bytes()).is_err());
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
        let key = out.untargeted_decrypt_key.expect("untargeted returns a key");
        let opened = open_from_owner(&key, &out.sealed).unwrap();
        assert_eq!(opened.as_slice(), &epoch);
    }
}
