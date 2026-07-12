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
use crate::owner_state_types::OwnerAddr;
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
/// inviter's master owner address + an optional display hint + the inviter's
/// `EnrollmentCert` (so the redeemer can verify the inviter's owner->device
/// binding before it has synced any shared state).
///
/// Per the corrected identity model (spec §3), the inviter is identified by
/// their master `owner_id`, and the cert carries the master Ed25519 key — there
/// is no separate Reticulum combined-pub field. `inviter_addr` MUST equal
/// `inviter_enrollment.owner_id` (a structural check enforced in
/// [`decode_friend_token_url`]; the full crypto verification of the cert happens
/// later at handshake time).
///
/// Wire format: up to 5-key map; `dn` is skipped when `None`, `ib` when empty.
/// Field codes are 2 chars (3-byte encoded) to satisfy the canonical-CBOR
/// same-length-key rule at this nesting level (matching
/// `CommunityInvitePayload`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendTokenPayload {
    /// The inviter's 16-byte master owner address (their `owner_id`; matches
    /// `token.inviter` and `inviter_enrollment.owner_id`).
    #[serde(rename = "ia")]
    pub inviter_addr: OwnerAddr,

    /// The inviter's advertised display name at mint time (UX hint; refreshable
    /// on accept). `None`/absent on the wire when unset.
    ///
    /// Capped at `MAX_FRIEND_DISPLAY_LEN` at DECODE (oversized → hard decode
    /// error, not truncation), via the same strict deserializer
    /// `FriendEntry.display` and the handshake `display` fields use. Without this
    /// cap an oversized hint would flow into a local `FriendEntry` on redeem and
    /// then fail owner-state deserialize on the owner's other devices during
    /// sync. The cap does not alter the ENCODED wire bytes of valid values.
    #[serde(
        rename = "dn",
        skip_serializing_if = "Option::is_none",
        default,
        deserialize_with = "crate::friend_graph::deserialize_capped_display"
    )]
    pub display_hint: Option<String>,

    /// The device-#2-signed one-shot redemption token (`invitee_hint = None`).
    #[serde(rename = "tk")]
    pub token: InviteToken,

    /// The inviter's Master `EnrollmentCert`, so the redeemer can verify the
    /// inviter's owner->device binding (and thus the `InviteToken` signature)
    /// before syncing any shared state. REQUIRED — the redeemer authenticates
    /// the inviter solely from this cert under the device-#2 + `owner_id` model.
    #[serde(rename = "ie")]
    pub inviter_enrollment: EnrollmentCert,

    /// ZEB-677: Master-issued signer certs backing a Quorum-issued
    /// `inviter_enrollment`. Empty for Master-issued certs (the key is
    /// omitted on the wire; old redeemers ignore it and keep rejecting
    /// quorum-certed inviters at handshake time).
    #[serde(rename = "ib", default, skip_serializing_if = "Vec::is_empty")]
    pub inviter_signer_certs: Vec<EnrollmentCert>,
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
    /// `inviter_addr` does not equal `inviter_enrollment.owner_id`. A structural
    /// (field-equality) check only — the cert's full crypto verification happens
    /// later at handshake time. A mismatch means the payload's claimed inviter
    /// address is not the one the embedded cert attests, so the token is
    /// internally inconsistent and rejected before any further processing.
    #[error("inviter_addr does not match inviter_enrollment.owner_id")]
    AddrCertMismatch,
    /// The signed `token.inviter` field disagrees with the cert-backed
    /// `inviter_addr`. The `InviteToken.sig` is bound to `token.inviter`, so if
    /// it differs from `inviter_addr` (which the cert attests) the token is
    /// internally inconsistent — a friend token whose redemption proof is signed
    /// for a different inviter than the one the payload/cert claim. Rejected at
    /// decode before any further processing.
    #[error("token.inviter does not match inviter_addr")]
    AddrTokenMismatch,
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
    let payload = canonical_cbor_decode::<FriendTokenPayload>(&bytes)
        .map_err(|e| FriendTokenError::Cbor(e.to_string()))?;
    // Structural consistency: the claimed inviter address MUST be the one the
    // embedded cert attests. Field-equality only — full cert crypto verification
    // is deferred to handshake time (`verify_enrolled_device`).
    if payload.inviter_addr.0 != payload.inviter_enrollment.owner_id {
        return Err(FriendTokenError::AddrCertMismatch);
    }
    // Structural consistency #2: the signed `token.inviter` MUST equal the
    // cert-backed `inviter_addr`. The `InviteToken.sig` is bound to
    // `token.inviter`; a token signed for a DIFFERENT inviter than the payload's
    // claimed (cert-attested) `inviter_addr` is internally inconsistent. Reject
    // it here before any pkarr/handshake work.
    if payload.token.inviter != payload.inviter_addr {
        return Err(FriendTokenError::AddrTokenMismatch);
    }
    Ok(payload)
}

/// Mint a friend token: sign an `InviteToken` with the enrolled device-#2 key
/// (reusing `invite_mint::mint_invite_token` verbatim, `invitee_hint = None` so
/// the link is an untargeted "controlled open" peer link per spec §5.3), then
/// assemble the `FriendTokenPayload`. The returned payload is ready for
/// [`encode_friend_token_url`] and for Case-A pkarr publication (the inviter's
/// reachability is keyed off `token.sig`).
///
/// Returns `Err` only if the underlying `InviteToken` signing fails (string
/// error, surfaced verbatim from `mint_invite_token`).
pub fn mint_friend_token(
    inviter_addr: OwnerAddr,
    display_hint: Option<String>,
    minted_at: crate::owner_state_types::Hlc,
    expires_at: Option<u64>,
    inviter_enrollment: EnrollmentCert,
    device2_signing_key: &ed25519_dalek::SigningKey,
) -> Result<FriendTokenPayload, String> {
    // invitee_hint = None: untargeted, "controlled open" friend link. The token
    // sig is bound to (inviter, minted_at, expires_at?) by mint_invite_token.
    let token = crate::invite_mint::mint_invite_token(
        inviter_addr,
        None,
        minted_at,
        expires_at,
        device2_signing_key,
    )?;
    Ok(FriendTokenPayload {
        inviter_addr,
        display_hint,
        token,
        inviter_enrollment,
        // ZEB-677: bundle threading for quorum-certed inviters lands with the
        // ceremony slices (S4); every self-cert today is Master-issued.
        inviter_signer_certs: Vec::new(),
    })
}

/// Resolve a caller-supplied **TTL duration in milliseconds** into the absolute
/// wall-clock-ms `expires_at` the token contract requires (`InviteToken.expires_at`,
/// serde `xa`: "wall-clock ms past which the receiver MUST reject this token").
///
/// ZEB-507: the API used to take `expires_at` as an *absolute epoch-ms* and stamp
/// it verbatim, so a caller who passed a seconds value (~10 digits) minted a
/// dead-on-arrival token — every verify site compares against now-in-ms (~13
/// digits), making a seconds value ~1000× too small and thus always-expired. By
/// deriving `expires_at = now_ms + ttl_ms` server-side (the same `now_ms` already
/// used for `minted_at`), the caller never supplies an epoch, so the wrong-unit
/// class is structurally impossible.
///
/// `None` → no app-level expiry (the `Some(_)` guard is skipped at both verify
/// sites). `saturating_add` keeps a pathological TTL from wrapping to a tiny
/// (already-expired) value, which would silently reintroduce the footgun.
pub(crate) fn resolve_expiry_ms(ttl_ms: Option<u64>, now_ms: u64) -> Option<u64> {
    ttl_ms.map(|ttl| now_ms.saturating_add(ttl))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_invite::InviteToken;
    use crate::community_membership::mint_test_owner;
    use crate::owner_state_types::Hlc;

    /// A realistic payload whose `inviter_addr` is the master `owner_id` the
    /// embedded Master `EnrollmentCert` attests (the structural invariant
    /// `decode_friend_token_url` enforces). The token's `inviter` matches too.
    fn sample() -> FriendTokenPayload {
        let owner = mint_test_owner(0x42);
        FriendTokenPayload {
            inviter_addr: owner.owner,
            display_hint: Some("bob".into()),
            inviter_signer_certs: Vec::new(),
            token: InviteToken {
                inviter: owner.owner,
                invitee_hint: None,
                minted_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "d".into(),
                },
                expires_at: None,
                sig: [3u8; 64],
            },
            inviter_enrollment: owner.cert,
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

    // ── resolve_expiry_ms (ZEB-507): TTL-duration → absolute wall-clock ms ──────

    #[test]
    fn resolve_expiry_ms_none_is_no_expiry() {
        // Omitted TTL → no app-level expiry (the `if let Some(_)` guard is skipped
        // at both verify sites), exactly as the pre-ZEB-507 `expiresAt: null` path.
        assert_eq!(resolve_expiry_ms(None, 1_700_000_000_000), None);
    }

    #[test]
    fn resolve_expiry_ms_adds_ttl_to_now() {
        // A 4-hour TTL resolves to now + 4h in ms — server-derived from the same
        // `now` used for `minted_at`, so the caller never supplies an epoch.
        let now = 1_700_000_000_000u64;
        let four_hours = 4 * 60 * 60 * 1000u64;
        assert_eq!(
            resolve_expiry_ms(Some(four_hours), now),
            Some(now + four_hours)
        );
    }

    #[test]
    fn resolve_expiry_ms_saturates_on_overflow() {
        // Overflow-safe: a pathological TTL clamps to u64::MAX rather than wrapping
        // to a tiny (already-expired) value — which would reintroduce the footgun.
        assert_eq!(resolve_expiry_ms(Some(u64::MAX), 5), Some(u64::MAX));
    }

    #[test]
    fn resolve_expiry_ms_zero_ttl_is_immediate() {
        // A zero TTL yields expires_at == now → immediately expired at redeem.
        // That is the caller's explicit (if odd) choice, NOT the silent unit
        // mistake ZEB-507 fixed: you can no longer pass a seconds-valued *epoch*.
        let now = 1_700_000_000_000u64;
        assert_eq!(resolve_expiry_ms(Some(0), now), Some(now));
    }

    #[test]
    fn decode_rejects_oversized_body() {
        let url = format!(
            "harmony://friend/{}",
            "A".repeat(MAX_FRIEND_BODY_B64_CHARS + 1)
        );
        assert!(decode_friend_token_url(&url).is_err());
    }

    #[test]
    fn decode_rejects_addr_cert_mismatch() {
        // A payload whose inviter_addr does NOT equal the embedded cert's
        // owner_id must be rejected at decode (structural consistency).
        let mut p = sample();
        // Re-point inviter_addr at a different owner while keeping the original
        // cert — now inviter_addr != inviter_enrollment.owner_id.
        let other = mint_test_owner(0x43);
        assert_ne!(other.owner.0, p.inviter_enrollment.owner_id);
        p.inviter_addr = other.owner;
        let url = encode_friend_token_url(&p).expect("encode (no check on encode)");
        match decode_friend_token_url(&url) {
            Err(FriendTokenError::AddrCertMismatch) => {}
            other => panic!("expected AddrCertMismatch, got {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_addr_token_mismatch() {
        // A payload whose signed `token.inviter` disagrees with the cert-backed
        // `inviter_addr` must be rejected at decode (structural consistency #2).
        // We keep `inviter_addr == cert.owner_id` (so the AddrCertMismatch check
        // passes) but re-point `token.inviter` at a different owner.
        let mut p = sample();
        let other = mint_test_owner(0x44);
        assert_ne!(other.owner, p.inviter_addr);
        p.token.inviter = other.owner;
        let url = encode_friend_token_url(&p).expect("encode (no check on encode)");
        match decode_friend_token_url(&url) {
            Err(FriendTokenError::AddrTokenMismatch) => {}
            other => panic!("expected AddrTokenMismatch, got {other:?}"),
        }

        // Sanity: the consistent sample (token.inviter == inviter_addr) still
        // decodes cleanly.
        let consistent = sample();
        let url = encode_friend_token_url(&consistent).expect("encode");
        assert_eq!(
            decode_friend_token_url(&url).expect("consistent token decodes"),
            consistent
        );
    }

    #[test]
    fn decode_rejects_oversized_display_hint() {
        // FIX 3: an oversized `display_hint` must be REJECTED at
        // decode_friend_token_url (wire ingress), mirroring FriendEntry.display /
        // the handshake display fields. Otherwise it would flow into a local
        // FriendEntry on redeem and fail owner-state deserialize on other devices.
        use crate::friend_graph::MAX_FRIEND_DISPLAY_LEN;
        let mut p = sample();

        // 257-byte hint → must FAIL to decode (serialize is uncapped).
        p.display_hint = Some("x".repeat(MAX_FRIEND_DISPLAY_LEN + 1));
        let url = encode_friend_token_url(&p).expect("encode (serialize is uncapped)");
        match decode_friend_token_url(&url) {
            Err(FriendTokenError::Cbor(_)) => {}
            other => panic!("expected Cbor decode error for oversized hint, got {other:?}"),
        }

        // 256-byte hint (exactly at the cap) → still decodes round-trip.
        p.display_hint = Some("y".repeat(MAX_FRIEND_DISPLAY_LEN));
        let url = encode_friend_token_url(&p).expect("encode");
        assert_eq!(
            decode_friend_token_url(&url).expect("at-cap hint decodes"),
            p
        );
    }

    #[test]
    fn minted_friend_token_verifies_and_encodes() {
        use crate::community_invite::verify_invite_token_sig_device_key;
        // mint_friend_token signs the InviteToken with the OWNER's enrolled
        // device-#2 key (the same key the cert attests), so the token sig must
        // verify against `cert.device_pubkeys.classical.ed25519_verify`.
        let owner = mint_test_owner(0x55);
        let device2 = owner.device_key.clone();
        let p = mint_friend_token(
            owner.owner,        // inviter_addr (= cert.owner_id)
            Some("bob".into()), // display_hint
            Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "d".into(),
            }, // minted_at
            Some(200),          // expires_at
            owner.cert.clone(), // inviter_enrollment (required)
            &device2,
        )
        .expect("mint");
        verify_invite_token_sig_device_key(
            &p.token,
            &owner.cert.device_pubkeys.classical.ed25519_verify,
        )
        .expect("sig ok against the cert's enrolled device key");
        // Round-trips through the URL codec (passes the addr↔cert check).
        let url = encode_friend_token_url(&p).expect("encode");
        assert_eq!(decode_friend_token_url(&url).expect("decode"), p);
    }
}
