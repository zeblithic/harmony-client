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
}
