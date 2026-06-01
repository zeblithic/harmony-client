//! ZEB-345 — long-form profile document (bio + links + fields), CAS-addressed.
//! Canonical CBOR, declaration-order keys; encode/validate authority lives here.
//! Spec: docs/specs/2026-05-31-zeb-345-profile-page-cas-design.md §4.
use crate::owner_state_crypto::{
    canonical_cbor_encode, sealed::CanonicalPayloadSealed, CanonicalPayload, CryptoError,
};
use serde::{Deserialize, Serialize};

pub const PROFILE_DOC_VERSION: u8 = 1;
pub const MAX_BIO_BYTES: usize = 4_096;
pub const MAX_LINKS: usize = 10;
pub const MAX_LINK_LABEL_BYTES: usize = 64;
pub const MAX_LINK_URL_BYTES: usize = 512;
pub const MAX_FIELDS: usize = 16;
pub const MAX_FIELD_KEY_BYTES: usize = 32;
pub const MAX_FIELD_VALUE_BYTES: usize = 256;
pub const MAX_PROFILE_DOC_BYTES: usize = 16_384;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilePageDoc {
    #[serde(rename = "vn")]
    pub version: u8,
    #[serde(rename = "bo")]
    pub bio: String,
    #[serde(rename = "ln")]
    pub links: Vec<ProfileLink>,
    #[serde(rename = "fl")]
    pub fields: Vec<ProfileField>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileLink {
    #[serde(rename = "lb")]
    pub label: String,
    #[serde(rename = "ur")]
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileField {
    #[serde(rename = "ky")]
    pub key: String,
    #[serde(rename = "vl")]
    pub value: String,
}

impl CanonicalPayloadSealed for ProfilePageDoc {}
impl CanonicalPayload for ProfilePageDoc {}

// NOTE: no `PartialEq`/`Eq` derive — the `Encode(CryptoError)` variant wraps
// `owner_state_crypto::CryptoError`, which intentionally does not implement them
// (it carries free-form `String` payloads). Tests assert variants via `matches!`,
// which needs no equality bound.
#[derive(Debug, thiserror::Error)]
pub enum ProfileDocError {
    #[error("bio exceeds {MAX_BIO_BYTES} bytes")]
    BioTooLong,
    #[error("more than {MAX_LINKS} links")]
    TooManyLinks,
    #[error("link label exceeds {MAX_LINK_LABEL_BYTES} bytes")]
    LinkLabelTooLong,
    #[error("link url exceeds {MAX_LINK_URL_BYTES} bytes")]
    LinkUrlTooLong,
    #[error("link scheme not in allowlist (https/harmony)")]
    LinkSchemeNotAllowed,
    #[error("more than {MAX_FIELDS} fields")]
    TooManyFields,
    #[error("field key exceeds {MAX_FIELD_KEY_BYTES} bytes")]
    FieldKeyTooLong,
    #[error("field value exceeds {MAX_FIELD_VALUE_BYTES} bytes")]
    FieldValueTooLong,
    #[error("encoded doc exceeds {MAX_PROFILE_DOC_BYTES} bytes")]
    DocTooLarge,
    #[error("unsupported profile doc version")]
    UnsupportedVersion,
    #[error("canonical CBOR encode failed: {0}")]
    Encode(#[from] CryptoError),
    #[error("CBOR decode failed")]
    Decode,
}

/// A link URL is allowed only if it is a non-empty `https://` URL or a
/// `harmony:` deep-link with a non-empty path that contains no further scheme
/// separator. The `:` guard is the load-bearing part: it rejects scheme
/// lookalikes like `harmony:javascript:alert(1)` and `harmony:data:...` (and
/// the bare `harmony:`), which a plain `starts_with("harmony:")` would accept.
fn link_scheme_allowed(url: &str) -> bool {
    if let Some(rest) = url.strip_prefix("https://") {
        return !rest.is_empty();
    }
    if let Some(rest) = url.strip_prefix("harmony:") {
        // Require a non-empty path with no embedded scheme separator, so
        // harmony:javascript:..., harmony:data:..., and bare harmony: are rejected.
        return !rest.is_empty() && !rest.contains(':');
    }
    false
}

/// Enforce every per-dimension cap: bio bytes, link count + per-link label/url
/// bytes + allowed scheme, field count + per-field key/value bytes, and the doc
/// version. Does NOT enforce the total-encoded-bytes cap (`DocTooLarge`): that
/// is checked on produced bytes in `encode_profile_doc` and on input bytes in
/// `decode_profile_doc`, so the canonical encode runs at most once per call.
pub fn validate_profile_doc(doc: &ProfilePageDoc) -> Result<(), ProfileDocError> {
    // Keep encode/decode symmetric: decode_profile_doc rejects an unknown
    // version, so validate (and therefore encode) must reject it too — otherwise
    // encode could emit a doc its own decode path refuses.
    if doc.version != PROFILE_DOC_VERSION {
        return Err(ProfileDocError::UnsupportedVersion);
    }
    if doc.bio.len() > MAX_BIO_BYTES {
        return Err(ProfileDocError::BioTooLong);
    }
    if doc.links.len() > MAX_LINKS {
        return Err(ProfileDocError::TooManyLinks);
    }
    for l in &doc.links {
        if l.label.len() > MAX_LINK_LABEL_BYTES {
            return Err(ProfileDocError::LinkLabelTooLong);
        }
        if l.url.len() > MAX_LINK_URL_BYTES {
            return Err(ProfileDocError::LinkUrlTooLong);
        }
        if !link_scheme_allowed(&l.url) {
            return Err(ProfileDocError::LinkSchemeNotAllowed);
        }
    }
    if doc.fields.len() > MAX_FIELDS {
        return Err(ProfileDocError::TooManyFields);
    }
    for f in &doc.fields {
        if f.key.len() > MAX_FIELD_KEY_BYTES {
            return Err(ProfileDocError::FieldKeyTooLong);
        }
        if f.value.len() > MAX_FIELD_VALUE_BYTES {
            return Err(ProfileDocError::FieldValueTooLong);
        }
    }
    Ok(())
}

/// Validate caps, canonical-CBOR encode (declaration-order keys), then reject if
/// the produced bytes exceed `MAX_PROFILE_DOC_BYTES`. The total-bytes cap is
/// enforced here (on the single produced encoding) rather than inside
/// `validate_profile_doc`, so the canonical encode runs only once.
pub fn encode_profile_doc(doc: &ProfilePageDoc) -> Result<Vec<u8>, ProfileDocError> {
    validate_profile_doc(doc)?;
    let bytes = canonical_cbor_encode(doc)?;
    if bytes.len() > MAX_PROFILE_DOC_BYTES {
        return Err(ProfileDocError::DocTooLarge);
    }
    Ok(bytes)
}

/// Reject oversize blobs up front, parse, reject unknown version, then run
/// `validate_profile_doc` so a fetched doc is bound by the same per-dimension
/// caps as an ingested one. The input bytes already bound the total size (the
/// up-front `DocTooLarge` check), so no re-encode is needed.
pub fn decode_profile_doc(bytes: &[u8]) -> Result<ProfilePageDoc, ProfileDocError> {
    if bytes.len() > MAX_PROFILE_DOC_BYTES {
        return Err(ProfileDocError::DocTooLarge);
    }
    let doc: ProfilePageDoc =
        ciborium::de::from_reader(bytes).map_err(|_| ProfileDocError::Decode)?;
    if doc.version != PROFILE_DOC_VERSION {
        return Err(ProfileDocError::UnsupportedVersion);
    }
    validate_profile_doc(&doc)?;
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_doc() -> ProfilePageDoc {
        ProfilePageDoc {
            version: PROFILE_DOC_VERSION,
            bio: "hi\nthere".into(),
            links: vec![ProfileLink {
                label: "Site".into(),
                url: "https://example.com".into(),
            }],
            fields: vec![ProfileField {
                key: "Pronouns".into(),
                value: "she/her".into(),
            }],
        }
    }

    fn doc_with_bio(bio: String) -> ProfilePageDoc {
        ProfilePageDoc {
            version: PROFILE_DOC_VERSION,
            bio,
            links: vec![],
            fields: vec![],
        }
    }

    fn doc_with_link(label: &str, url: &str) -> ProfilePageDoc {
        ProfilePageDoc {
            version: PROFILE_DOC_VERSION,
            bio: String::new(),
            links: vec![ProfileLink {
                label: label.into(),
                url: url.into(),
            }],
            fields: vec![],
        }
    }

    #[test]
    fn round_trip_encodes_and_decodes() {
        let doc = base_doc();
        let bytes = encode_profile_doc(&doc).unwrap();
        assert_eq!(decode_profile_doc(&bytes).unwrap(), doc);
    }

    #[test]
    fn rejects_oversize_bio() {
        let doc = doc_with_bio("x".repeat(MAX_BIO_BYTES + 1));
        assert!(matches!(
            validate_profile_doc(&doc),
            Err(ProfileDocError::BioTooLong)
        ));
    }

    #[test]
    fn rejects_too_many_links() {
        let doc = ProfilePageDoc {
            version: PROFILE_DOC_VERSION,
            bio: String::new(),
            links: (0..MAX_LINKS + 1)
                .map(|_| ProfileLink {
                    label: "L".into(),
                    url: "https://x.example".into(),
                })
                .collect(),
            fields: vec![],
        };
        assert!(matches!(
            validate_profile_doc(&doc),
            Err(ProfileDocError::TooManyLinks)
        ));
    }

    #[test]
    fn rejects_overlong_link_label_and_url() {
        let long_label = doc_with_link(&"L".repeat(MAX_LINK_LABEL_BYTES + 1), "https://x.example");
        assert!(matches!(
            validate_profile_doc(&long_label),
            Err(ProfileDocError::LinkLabelTooLong)
        ));

        let long_url = doc_with_link("L", &format!("https://{}", "y".repeat(MAX_LINK_URL_BYTES)));
        assert!(matches!(
            validate_profile_doc(&long_url),
            Err(ProfileDocError::LinkUrlTooLong)
        ));
    }

    #[test]
    fn rejects_too_many_fields_and_overlong_key_value() {
        let too_many = ProfilePageDoc {
            version: PROFILE_DOC_VERSION,
            bio: String::new(),
            links: vec![],
            fields: (0..MAX_FIELDS + 1)
                .map(|_| ProfileField {
                    key: "k".into(),
                    value: "v".into(),
                })
                .collect(),
        };
        assert!(matches!(
            validate_profile_doc(&too_many),
            Err(ProfileDocError::TooManyFields)
        ));

        let long_key = ProfilePageDoc {
            version: PROFILE_DOC_VERSION,
            bio: String::new(),
            links: vec![],
            fields: vec![ProfileField {
                key: "k".repeat(MAX_FIELD_KEY_BYTES + 1),
                value: "v".into(),
            }],
        };
        assert!(matches!(
            validate_profile_doc(&long_key),
            Err(ProfileDocError::FieldKeyTooLong)
        ));

        let long_value = ProfilePageDoc {
            version: PROFILE_DOC_VERSION,
            bio: String::new(),
            links: vec![],
            fields: vec![ProfileField {
                key: "k".into(),
                value: "v".repeat(MAX_FIELD_VALUE_BYTES + 1),
            }],
        };
        assert!(matches!(
            validate_profile_doc(&long_value),
            Err(ProfileDocError::FieldValueTooLong)
        ));
    }

    #[test]
    fn rejects_disallowed_link_scheme() {
        let doc = doc_with_link("L", "http://insecure.example");
        assert!(matches!(
            validate_profile_doc(&doc),
            Err(ProfileDocError::LinkSchemeNotAllowed)
        ));
    }

    #[test]
    fn accepts_https_and_harmony_schemes() {
        assert!(validate_profile_doc(&doc_with_link("a", "https://x.example")).is_ok());
        assert!(validate_profile_doc(&doc_with_link("b", "harmony:community/abc")).is_ok());
        // https with an explicit port is still a valid https URL (the embedded
        // ':' is part of the authority, not a second scheme).
        assert!(validate_profile_doc(&doc_with_link("c", "https://host:8443/p")).is_ok());
    }

    #[test]
    fn rejects_scheme_lookalikes() {
        // A harmony: prefix wrapping another scheme must NOT slip through the
        // allowlist — the link_scheme_allowed `:` guard rejects these.
        for url in [
            "harmony:javascript:alert(1)",
            "harmony:data:text/html,<script>1</script>",
            "harmony:", // bare scheme, empty path
            "https://", // empty https authority
        ] {
            assert!(
                matches!(
                    validate_profile_doc(&doc_with_link("L", url)),
                    Err(ProfileDocError::LinkSchemeNotAllowed)
                ),
                "expected LinkSchemeNotAllowed for {url:?}"
            );
        }
    }

    #[test]
    fn total_bytes_cap_is_a_correctly_sized_backstop() {
        // A fully-maxed doc (every per-dimension cap at its limit) must still
        // encode within MAX_PROFILE_DOC_BYTES — i.e. the total-bytes cap is a
        // backstop, never a spurious rejecter of otherwise-valid content. (The
        // per-dimension caps are deliberately sized so their sum + CBOR overhead
        // fits under the total cap.) If a future cap bump breaks this invariant,
        // this test catches it.
        let maxed = ProfilePageDoc {
            version: PROFILE_DOC_VERSION,
            bio: "x".repeat(MAX_BIO_BYTES),
            links: (0..MAX_LINKS)
                .map(|_| ProfileLink {
                    label: "L".repeat(MAX_LINK_LABEL_BYTES),
                    url: format!(
                        "https://{}",
                        "y".repeat(MAX_LINK_URL_BYTES - "https://".len())
                    ),
                })
                .collect(),
            fields: (0..MAX_FIELDS)
                .map(|_| ProfileField {
                    key: "k".repeat(MAX_FIELD_KEY_BYTES),
                    value: "v".repeat(MAX_FIELD_VALUE_BYTES),
                })
                .collect(),
        };
        let bytes = encode_profile_doc(&maxed).expect("maxed doc must validate + encode");
        assert!(
            bytes.len() <= MAX_PROFILE_DOC_BYTES,
            "maxed valid doc encodes to {} bytes, over the {MAX_PROFILE_DOC_BYTES}-byte cap",
            bytes.len()
        );
    }

    #[test]
    fn rejects_total_bytes_over_cap() {
        // Exercise the DocTooLarge branch directly: decode_profile_doc rejects any
        // blob larger than the total byte cap up front. The cap is enforced on
        // input bytes here (decode) and on produced bytes in encode_profile_doc;
        // validate_profile_doc no longer re-encodes to measure size (the
        // per-dimension caps make an over-cap valid doc unreachable by
        // construction — see total_bytes_cap_is_a_correctly_sized_backstop).
        assert!(matches!(
            decode_profile_doc(&vec![0u8; MAX_PROFILE_DOC_BYTES + 1]),
            Err(ProfileDocError::DocTooLarge)
        ));
    }

    #[test]
    fn decode_rejects_unknown_version() {
        // Hand-encode a doc with version=2 (bypass encode_profile_doc, which only
        // ever emits PROFILE_DOC_VERSION) and confirm decode rejects it.
        let doc = ProfilePageDoc {
            version: 2,
            bio: "hi".into(),
            links: vec![],
            fields: vec![],
        };
        let bytes = canonical_cbor_encode(&doc).unwrap();
        assert!(matches!(
            decode_profile_doc(&bytes),
            Err(ProfileDocError::UnsupportedVersion)
        ));
    }

    #[test]
    fn encode_rejects_unknown_version() {
        // Symmetry with decode_rejects_unknown_version: encode must refuse a
        // wrong-version doc rather than emitting bytes its own decode rejects.
        let doc = ProfilePageDoc {
            version: PROFILE_DOC_VERSION + 1,
            bio: "hi".into(),
            links: vec![],
            fields: vec![],
        };
        assert!(matches!(
            encode_profile_doc(&doc),
            Err(ProfileDocError::UnsupportedVersion)
        ));
        assert!(matches!(
            validate_profile_doc(&doc),
            Err(ProfileDocError::UnsupportedVersion)
        ));
    }

    #[test]
    fn decode_rejects_oversize_blob() {
        assert!(matches!(
            decode_profile_doc(&vec![0u8; MAX_PROFILE_DOC_BYTES + 1]),
            Err(ProfileDocError::DocTooLarge)
        ));
    }
}
