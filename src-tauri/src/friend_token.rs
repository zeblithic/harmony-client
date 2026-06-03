//! ZEB-370 Phase 1: friend-token payload + `harmony://friend/` URL codec.
//!
//! The friend-token is the out-of-band island-bridging path (spec §5.3): an
//! inviter mints a device-#2-signed `InviteToken` (reusing the ZEB-367 mint
//! machinery verbatim, `invitee_hint = None` so the link is an untargeted
//! "controlled open" peer link), wraps it in a `FriendTokenPayload`, and shares
//! a `harmony://friend/<b64>` URL. The redeemer decodes the URL, resolves the
//! inviter's reachability via the Case-A pkarr record, and completes the
//! `harmony/friend/v1` accept exchange (later phases).
//!
//! Wire format mirrors `community_invite`'s URL codec EXACTLY: canonical CBOR →
//! base64url-no-pad → `harmony://friend/` prefix. Decode strips the prefix,
//! size-caps FIRST (bounding work on untrusted input), base64url-decodes, then
//! canonical-CBOR-decodes. Map keys at this nesting level are all 2-char (3-byte
//! encoded) to satisfy the canonical-CBOR same-length-key rule.

use crate::community_invite::InviteToken;
use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use crate::owner_state_types::{deserialize_bytes_from_bstr, serialize_bytes_as_bstr, OwnerAddr};
use base64::Engine;
use harmony_owner::certs::EnrollmentCert;
use serde::{Deserialize, Serialize};

/// URL scheme for friend tokens. Distinct from `harmony://invite/` for UX
/// clarity (spec §11.4) so a friend link is never mistaken for a community
/// invite and vice-versa.
const URL_PREFIX: &str = "harmony://friend/";

/// Hard cap on the base64url body length (post-prefix-strip, in base64 chars)
/// handed to the base64 + CBOR decoders. Reuses the invite-side cap so the two
/// codecs bound untrusted input identically; the embedded `InviteToken` +
/// optional `EnrollmentCert` make a friend token comparable in size to an
/// invite. See [`crate::community_invite::MAX_INVITE_BODY_B64_CHARS`].
pub const MAX_FRIEND_BODY_B64_CHARS: usize = crate::community_invite::MAX_INVITE_BODY_B64_CHARS;

/// The payload carried by a `harmony://friend/<b64>` URL. Wraps a device-#2-
/// signed `InviteToken` (the cryptographic one-shot redemption proof) with the
/// inviter's owner address + 64-byte identity public bytes + an optional display
/// hint + optional `EnrollmentCert` (so the redeemer can verify the inviter's
/// owner->device binding before it has synced any shared state).
///
/// Wire format: up to 5-key map; `dn`/`ie` are skipped when `None`. Field codes
/// are 2 chars (3-byte encoded) to satisfy the canonical-CBOR same-length-key
/// rule at this nesting level (matching `CommunityInvitePayload`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendTokenPayload {
    /// The inviter's 16-byte owner address (matches `token.inviter`).
    #[serde(rename = "ia")]
    pub inviter_addr: OwnerAddr,

    /// The inviter's 64-byte owner identity: `X25519_pub(32) || Ed25519_pub(32)`.
    /// Stored as a CBOR bstr(64). The redeemer needs the full pubkey (not just
    /// the address) to verify accept-side signatures and derive the pairwise
    /// secret in later phases. Reuses the in-repo bstr serde helper.
    #[serde(
        rename = "ip",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub inviter_owner_pub: [u8; 64],

    /// The inviter's advertised display name at mint time (UX hint; refreshable
    /// on accept). `None`/absent on the wire when unset.
    #[serde(rename = "dn", skip_serializing_if = "Option::is_none", default)]
    pub display_hint: Option<String>,

    /// The device-#2-signed one-shot redemption token (`invitee_hint = None`).
    #[serde(rename = "tk")]
    pub token: InviteToken,

    /// The inviter's Master `EnrollmentCert`, so the redeemer can verify the
    /// inviter's owner->device binding (and thus the `InviteToken` signature)
    /// before syncing any shared state. `None`/absent on the wire when unset
    /// (e.g. in unit fixtures); populated by `mint_friend_token` in production.
    #[serde(rename = "ie", skip_serializing_if = "Option::is_none", default)]
    pub inviter_enrollment: Option<EnrollmentCert>,
}

// `FriendTokenPayload`'s `CanonicalPayload`/`CanonicalPayloadSealed` impls are
// registered via `impl_canonical!` in `owner_state_types.rs` (alongside
// `FriendGraph`/`FriendEntry`), so `canonical_cbor_encode`/`decode` apply here.

/// Errors encoding/decoding a `harmony://friend/...` URL. Mirrors
/// `InviteUrlError`'s failure classes (distinct variants per class so the IPC
/// layer can surface a precise diagnostic).
#[derive(thiserror::Error, Debug)]
pub enum FriendTokenError {
    #[error("friend URL scheme must be `harmony://friend/`, got `{0}`")]
    WrongScheme(String),
    #[error("base64url decode failed: {0}")]
    Base64(String),
    #[error("CBOR decode failed: {0}")]
    Cbor(String),
    /// The post-prefix body exceeds [`MAX_FRIEND_BODY_B64_CHARS`] base64 chars.
    /// Bounds allocator + decode work on a hostile multi-MB paste.
    #[error("friend payload exceeds base64-char limit (got {0} chars)")]
    TooLarge(usize),
}

/// Canonical-CBOR-encode the payload, base64url-no-pad the result, and prefix
/// `harmony://friend/`. Output is copy-paste-safe across chat/email clients.
pub fn encode_friend_token_url(payload: &FriendTokenPayload) -> Result<String, FriendTokenError> {
    let cbor = canonical_cbor_encode(payload).map_err(|e| FriendTokenError::Cbor(e.to_string()))?;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&cbor);
    // Encode-time size check: fail fast rather than producing a URL that
    // decode_friend_token_url would immediately reject with TooLarge.
    if b64.len() > MAX_FRIEND_BODY_B64_CHARS {
        return Err(FriendTokenError::TooLarge(b64.len()));
    }
    Ok(format!("{URL_PREFIX}{b64}"))
}

/// Strip the `harmony://friend/` prefix, size-cap the body, base64url-decode,
/// then canonical-CBOR-decode into a [`FriendTokenPayload`].
///
/// Trims surrounding whitespace before scheme inspection — paste flows routinely
/// add leading/trailing whitespace. Caps the post-prefix body length (in base64
/// chars, not decoded bytes) BEFORE decoding to bound work on untrusted input.
pub fn decode_friend_token_url(url: &str) -> Result<FriendTokenPayload, FriendTokenError> {
    let url = url.trim();
    let body = url.strip_prefix(URL_PREFIX).ok_or_else(|| {
        FriendTokenError::WrongScheme(url.chars().take(URL_PREFIX.len()).collect())
    })?;
    if body.len() > MAX_FRIEND_BODY_B64_CHARS {
        return Err(FriendTokenError::TooLarge(body.len()));
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body)
        .map_err(|e| FriendTokenError::Base64(e.to_string()))?;
    canonical_cbor_decode::<FriendTokenPayload>(&bytes)
        .map_err(|e| FriendTokenError::Cbor(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_invite::InviteToken;
    use crate::owner_state_types::{Hlc, OwnerAddr};

    fn sample() -> FriendTokenPayload {
        FriendTokenPayload {
            inviter_addr: OwnerAddr([1u8; 16]),
            inviter_owner_pub: [2u8; 64],
            display_hint: Some("bob".into()),
            token: InviteToken {
                inviter: OwnerAddr([1u8; 16]),
                invitee_hint: None,
                minted_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "d".into(),
                },
                expires_at: None,
                sig: [3u8; 64],
            },
            inviter_enrollment: None, // Some(cert) in real mint; codec must handle None
        }
    }

    #[test]
    fn friend_token_url_round_trips() {
        let p = sample();
        let url = encode_friend_token_url(&p).expect("encode");
        assert!(url.starts_with("harmony://friend/"));
        assert_eq!(decode_friend_token_url(&url).expect("decode"), p);
    }

    #[test]
    fn decode_rejects_wrong_prefix() {
        assert!(decode_friend_token_url("harmony://invite/AAAA").is_err());
    }

    #[test]
    fn decode_rejects_oversized_body() {
        let url = format!(
            "harmony://friend/{}",
            "A".repeat(MAX_FRIEND_BODY_B64_CHARS + 1)
        );
        assert!(decode_friend_token_url(&url).is_err());
    }
}
