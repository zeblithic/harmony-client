//! ZEB-248 Phase 2: per-channel data plane.
//!
//! Ships:
//! - `SignedChannelEvent` (Post variant; v3-reserved variants commented).
//! - `ChannelKey` + `derive_channel_key` (HKDF-SHA256 over EpochKey).
//! - `encrypt_channel_packet` / `decrypt_channel_packet` (ChaCha20-Poly1305 with
//!   12-byte random nonce + static AAD).
//! - `ChannelLogReplayTracker` (per-(channel, author, device) HLC monotonicity).
//! - `verify_channel_event` (§7 chain steps 3-7 against a pre-decrypted event).
//! - `ChannelLog` + `ChannelLogManifest` + `SegmentDescriptor` + segmented
//!   persistence (manifest + tail + sealed segments).
//!
//! Out of scope (Phase 3): `ChannelLogEngine`, Zenoh transport, debounced flush
//! task, IPC surface, frontend.
//!
//! Parent spec: docs/specs/2026-05-09-zeb-248-channels-within-communities-design.md
//! (commit 5145484), sections §5.2, §6, §7, §8, §13.1.

use crate::community_membership::ChannelId;
use crate::community_membership::ChannelInfo;
use crate::owner_state_types::DmContentKey;
use crate::owner_state_types::EpochKey;
use crate::owner_state_types::Hlc;
use crate::owner_state_types::OwnerAddr;
use crate::owner_state_types::SpaceId;
use chacha20poly1305::aead::{Aead, OsRng, Payload};
use chacha20poly1305::{AeadCore, ChaCha20Poly1305, KeyInit};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::channel_chunk_index::ChunkIndex;
use crate::channel_rbsr::{RangeFingerprint, RangeReconcileSource, ReconcileKey};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Symmetric key for one channel's wire encryption. Derived
/// deterministically from `(EpochKey, community_id, channel_id)`
/// via HKDF-SHA256, so any Joined member can derive every channel's
/// key without out-of-band coordination. v3 will use this seam to
/// add private channels (distribute the ChannelKey to a subset of
/// members) without a wire-format break.
#[derive(Clone, zeroize::ZeroizeOnDrop)]
pub struct ChannelKey([u8; 32]);

impl ChannelKey {
    /// Borrow the raw 32 bytes for AEAD initialization. Not `pub` —
    /// callers go through `encrypt_channel_packet` / `decrypt_channel_packet`.
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Wrap already-derived key bytes. `pub(crate)` so a sibling module's own
    /// HKDF derivation (e.g. `address_book_sync::derive_addrbook_key`) can
    /// produce a `ChannelKey` without this module needing to host every
    /// derivation function.
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl std::fmt::Debug for ChannelKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ChannelKey(<32 bytes redacted>)")
    }
}

/// HKDF-SHA256 derivation of a per-channel symmetric key.
///
/// - IKM: `EpochKey` raw bytes (32 B).
/// - Salt: `community_id` raw bytes (16 B). Community-scoped so the same
///   channel-id collision across two communities yields different keys.
/// - Info: `b"channel:" || channel_id` (8 + 16 = 24 B). Channel-scoped so
///   distinct channels in the same community yield different keys.
/// - Output: 32 B → ChannelKey.
///
/// Per spec §6.
pub fn derive_channel_key(
    mk: &EpochKey,
    community_id: &SpaceId,
    channel_id: &ChannelId,
) -> ChannelKey {
    let salt = community_id.0;
    let mut info = Vec::with_capacity(8 + 16);
    info.extend_from_slice(b"channel:");
    info.extend_from_slice(&channel_id.0[..]);
    let mut out = zeroize::Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(&salt), mk.as_bytes())
        .expand(&info, out.as_mut())
        .expect("32 ≤ 8160");
    ChannelKey(*out)
}

/// ZEB-537: derive the per-community presence key from the community epoch
/// (membership) key. Mirrors `derive_channel_key` but binds only the community
/// (presence is community-scoped, not per-channel) with a distinct `info` label
/// so the presence key is independent of every channel key.
pub fn derive_presence_key(mk: &EpochKey, community_id: &SpaceId) -> ChannelKey {
    let salt = community_id.0;
    let info = b"presence:";
    let mut out = zeroize::Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(&salt), mk.as_bytes())
        .expand(info, out.as_mut())
        .expect("32 <= 8160");
    ChannelKey(*out)
}

/// HKDF-SHA256 derivation of a per-call DM voice key from the DM space's
/// `DmContentKey`. Mirrors `derive_channel_key`: any party holding the DM
/// content key derives the same per-call subkey from the (caller-generated)
/// `call_id`, with no out-of-band coordination and no per-call rekey (D3 /
/// V4 non-goals). Salt = `call_id` (per-call scope); Info = `b"voice-dm:"`.
pub fn derive_dm_voice_key(dm_key: &DmContentKey, call_id: &[u8; 16]) -> ChannelKey {
    let mut out = zeroize::Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(&call_id[..]), dm_key.as_bytes())
        .expand(b"voice-dm:", out.as_mut())
        .expect("32 ≤ 8160");
    ChannelKey(*out)
}

/// HKDF-SHA256 derivation of the group-DM **presence** key from the group's
/// `DmContentKey`. Unlike `derive_dm_voice_key`, this is **call-independent**
/// (no `call_id` salt) so every member can derive it and decrypt presence
/// beacons BEFORE joining any call — the basis for the join-in-progress banner
/// (ZEB-360 D2). Domain-separated from the media key by a distinct `info`
/// string; the same group content key yields a presence key unrelated to any
/// per-call `derive_dm_voice_key` output, and it survives across successive
/// calls in the group.
pub fn derive_groupdm_presence_key(dm_key: &DmContentKey) -> ChannelKey {
    let mut out = zeroize::Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(None, dm_key.as_bytes())
        .expand(b"voice-presence-groupdm:", out.as_mut())
        .expect("32 ≤ 8160");
    ChannelKey(*out)
}

/// 16-byte ULID identifying a single message within a channel.
/// Generated client-side at post time. Stable identity for v3
/// references (Edit/Delete/React variants will target this id).
///
/// Tuple-struct newtype (not type alias) so the type system catches
/// accidental substitution between message-IDs / event-IDs / channel-IDs
/// at IPC boundaries; bstr serde keeps wire encoding compact (17 bytes
/// vs CBOR array-of-u8 33 bytes for ULIDs with timestamp bytes ≥ 0x18).
/// Mirrors the shape of `community_membership::ChannelId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MessageId(
    #[serde(
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub [u8; 16],
);

/// Static AAD bytes for ChaCha20-Poly1305 wrapping of channel events.
/// v3 may extend with per-event AAD; for now this is a constant across
/// every packet on every channel.
pub const CHANNEL_PACKET_AAD: &[u8] = b"harmony-channel-msg-v1";

/// ChaCha20-Poly1305 nonce length per packet. Per spec §5.3.
const NONCE_LEN: usize = 12;
/// Poly1305 authentication-tag length appended by ChaCha20-Poly1305.
const TAG_LEN: usize = 16;
/// Minimum valid wire-packet length: nonce + (empty plaintext) + tag.
/// Anything shorter cannot structurally contain both, and we reject
/// before invoking the AEAD layer for a cleaner error split.
const MIN_PACKET_LEN: usize = NONCE_LEN + TAG_LEN;

/// ZEB-536: max byte length of a reaction emoji string. Room for a ZWJ
/// emoji sequence plus a short custom shortcode (Spec 3). Over-long
/// reactions fail `react()` locally and `verify_channel_event` inbound.
pub const MAX_REACTION_EMOJI_BYTES: usize = 32;

/// ZEB-534: hard cap on mentions per channel post. Enforced at every
/// entry point — local mint (`ChannelLogEngine::publish`), the IPC
/// boundary (`post_channel_message_impl`), AND inbound verification
/// (`verify_channel_event`) — so a remote peer can't bypass the cap by
/// crafting a signed event with a huge `mn` array. Bounds the signed-set
/// size and each recipient's "mentions me" scan.
pub(crate) const MAX_MENTIONS: usize = 64;

/// ZEB-535: hard cap on attachments per channel post. Bounds the signed-set
/// size and the per-message fetch fan-out. Enforced at mint (`publish`) AND
/// inbound verification (`verify_channel_event`) in PR1; the IPC-boundary
/// enforcement point (`post_channel_message_impl`) lands with the IPC surface
/// in PR2.
pub(crate) const MAX_ATTACHMENTS: usize = 16;

/// ZEB-535: max bytes for an attachment's `name`/`mime` string fields (each).
pub(crate) const MAX_ATTACHMENT_FIELD_BYTES: usize = 255;

/// ZEB-539: hard cap on a single attachment's signed `size` (1 GiB). An
/// attachment whose `size` exceeds the artifact download/ingest cap could
/// never be downloaded (the download path rejects `size > cap`), so a peer
/// could otherwise sign permanently-undownloadable attachments. Reject at
/// verify time and at the IPC mint path.
///
/// Defined as an alias of `crate::MAX_ARTIFACT_BYTES` (the download/ingest
/// plaintext cap in lib.rs, 1 GiB) so the two are a single source of truth and
/// can never drift. If they did — e.g. `MAX_ARTIFACT_BYTES` is tightened for a
/// v2 cap but this isn't — a peer could sign an attachment in the gap that
/// passes verify-time checks yet is permanently un-downloadable.
pub(crate) const MAX_ATTACHMENT_SIZE: u64 = crate::MAX_ARTIFACT_BYTES;

/// ZEB-535: a CAS artifact referenced by a channel post. `cid` is the root
/// (Book or Bundle) of the stored bytes; `cid.flags().encrypted` tells the
/// receiver whether to decrypt with the community epoch key. `name`/`mime`/
/// `size` are signed (tamper-evident) and packet-encrypted (confidential).
/// `size` is the PLAINTEXT length, cross-checked on fetch.
///
/// Nested CBOR keys sort `cd`(cid) < `mi`(mime) < `nm`(name) < `sz`(size)
/// per RFC 8949 §4.2.1 — declare fields in that exact order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelAttachment {
    #[serde(
        rename = "cd",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub cid: [u8; 32],
    #[serde(rename = "mi")]
    pub mime: String,
    #[serde(rename = "nm")]
    pub name: String,
    #[serde(rename = "sz")]
    pub size: u64,
}

/// One signed channel event. `Post` (phase 2) and `React` (ZEB-536) are
/// live variants; `Post` also carries optional `mentions` (ZEB-534) and
/// `attachments` (ZEB-535). `Edit`/`Delete` remain reserved for a future
/// release.
/// Wire format: 2-key adjacently-tagged outer (`tg` + `vl`); inner
/// fields all 2-char keys to satisfy the same-length-keys invariant.
///
/// at, content_kind, body, reply_to, mentions, attachments)` — every field
/// minus the signature itself, so the `mentions`/`mn` and `attachments`/`pa`
/// lists are tamper-evident like every other field. The `React` variant
/// signs its own `ChannelReactPayload`; future Edit/Delete variants will
/// likewise sign their own typed payloads with no field reuse across variants.
///
/// Per spec §5.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tg", content = "vl")]
pub enum SignedChannelEvent {
    #[serde(rename = "p")]
    Post {
        // Field order matches RFC 8949 §4.2.1 canonical CBOR ordering
        // for our 2-char keys: bytewise lexicographic sort of
        // at, au, bd, ch, ci, id, kd, mn, pa, rt, sg. ciborium emits map keys
        // in declaration order, so this declaration is what a strict
        // RFC 8949 reader would produce. See ChannelPostSignedSet's
        // doc comment for the full rationale.
        #[serde(rename = "at")]
        at: Hlc,
        #[serde(rename = "au")]
        author: OwnerAddr,
        #[serde(rename = "bd")]
        body: String,
        #[serde(rename = "ch")]
        channel_id: ChannelId,
        #[serde(rename = "ci")]
        community_id: SpaceId,
        #[serde(rename = "id")]
        id: MessageId,
        #[serde(rename = "kd")]
        content_kind: u8,
        #[serde(rename = "mn", skip_serializing_if = "Option::is_none", default)]
        mentions: Option<Vec<OwnerAddr>>,
        #[serde(rename = "pa", skip_serializing_if = "Option::is_none", default)]
        attachments: Option<Vec<ChannelAttachment>>,
        #[serde(rename = "rt", skip_serializing_if = "Option::is_none", default)]
        reply_to: Option<MessageId>,
        #[serde(
            rename = "sg",
            serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
            deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
        )]
        sig: [u8; 64],
    },
    // v3 reserved (additive — no v2 wire-format break):
    // Edit { id, ci, ch, au, at, kd, bd, sg }
    // Delete { id, ci, ch, au, at, sg }
    /// ZEB-536: a reaction/ack targeting a prior message in this channel.
    /// Append-only — un-reacting is a fresh React with `add=false`, never
    /// a mutation. `id` is the TARGET message id (reactions sharing a
    /// target are deduped by the per-(channel,author,device) HLC lane,
    /// not by id). Convergence is LWW per (target, author, emoji) by HLC.
    #[serde(rename = "r")]
    React {
        #[serde(rename = "ad")]
        add: bool,
        #[serde(rename = "at")]
        at: Hlc,
        #[serde(rename = "au")]
        author: OwnerAddr,
        #[serde(rename = "ch")]
        channel_id: ChannelId,
        #[serde(rename = "ci")]
        community_id: SpaceId,
        /// ZEB-541: optional custom-emoji CAS descriptor. Key `ea` sorts
        /// between `ci` and `em` in RFC 8949 bytewise key order. The
        /// `skip_serializing_if` plus `default` keeps a unicode reaction
        /// (`None`) byte-identical to a pre-feature React, so old and new
        /// peers interop and the existing `react_packet_is_byte_stable`
        /// fixture stays green.
        #[serde(rename = "ea", skip_serializing_if = "Option::is_none", default)]
        emoji_attachment: Option<ChannelAttachment>,
        #[serde(rename = "em")]
        emoji: String,
        #[serde(rename = "id")]
        target: MessageId,
        #[serde(
            rename = "sg",
            serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
            deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
        )]
        sig: [u8; 64],
    },
}

impl SignedChannelEvent {
    /// Community id (both variants).
    pub fn community_id(&self) -> &SpaceId {
        match self {
            SignedChannelEvent::Post { community_id, .. }
            | SignedChannelEvent::React { community_id, .. } => community_id,
        }
    }
    pub fn channel_id(&self) -> &ChannelId {
        match self {
            SignedChannelEvent::Post { channel_id, .. }
            | SignedChannelEvent::React { channel_id, .. } => channel_id,
        }
    }
    pub fn author(&self) -> &OwnerAddr {
        match self {
            SignedChannelEvent::Post { author, .. } | SignedChannelEvent::React { author, .. } => {
                author
            }
        }
    }
    pub fn at(&self) -> &Hlc {
        match self {
            SignedChannelEvent::Post { at, .. } | SignedChannelEvent::React { at, .. } => at,
        }
    }
    /// Post → message id; React → target message id.
    pub fn id(&self) -> &MessageId {
        match self {
            SignedChannelEvent::Post { id, .. } => id,
            SignedChannelEvent::React { target, .. } => target,
        }
    }
    pub fn sig(&self) -> &[u8; 64] {
        match self {
            SignedChannelEvent::Post { sig, .. } | SignedChannelEvent::React { sig, .. } => sig,
        }
    }
}

/// Pre-signature payload used to derive `event_id` and the signed-set
/// canonical-CBOR digest. Caller fills these fields, hands to
/// `sign_channel_event`, gets back a `SignedChannelEvent::Post`.
pub struct ChannelPostPayload<'a> {
    pub id: MessageId,
    pub community_id: SpaceId,
    pub channel_id: ChannelId,
    pub author: OwnerAddr,
    pub at: Hlc,
    pub content_kind: u8,
    pub body: &'a str,
    pub reply_to: Option<MessageId>,
    /// ZEB-534: owner-ids this post addresses. `None` is wire-identical
    /// to a pre-feature post. Carried into the signed set (tamper-
    /// evident), mirroring `reply_to`. Owned `Vec` (not a borrow) because
    /// `sign_channel_event` moves it into the owned event variant.
    pub mentions: Option<Vec<OwnerAddr>>,
    /// ZEB-535: CAS artifacts this post references. `None` is wire-identical
    /// to a pre-feature post. Carried into the signed set (tamper-evident).
    pub attachments: Option<Vec<ChannelAttachment>>,
}

/// Pre-signature payload (everything except the signature itself).
/// Canonical CBOR of this is what `sg` covers AND what the SHA-256
/// (event_id derivation) hashes.
///
/// Same-length-keys invariant: all field renames are 2-char codes
/// matching the corresponding wire codes on `SignedChannelEvent::Post`.
///
/// Field order matches RFC 8949 §4.2.1 canonical CBOR ordering
/// (length-first, then bytewise lexicographic — for same-length keys
/// this reduces to bytewise sort): at, au, bd, ch, ci, id, kd, mn, pa, rt.
/// ciborium emits map keys in declaration order, so this declaration
/// matches what a strict RFC 8949 reader would produce, ensuring
/// cross-language signature compatibility.
///
/// The same field order is mirrored in `SignedChannelEvent::Post`
/// (plus `sg` last; sg sorts after rt because 0x73 > 0x72), so
/// canonical CBOR of the wire-format event minus `sg` is byte-
/// identical to canonical CBOR of this signed-set.
#[derive(Serialize)]
struct ChannelPostSignedSet<'a> {
    #[serde(rename = "at")]
    at: &'a Hlc,
    #[serde(rename = "au")]
    author: &'a OwnerAddr,
    #[serde(rename = "bd")]
    body: &'a str,
    #[serde(rename = "ch")]
    channel_id: &'a ChannelId,
    #[serde(rename = "ci")]
    community_id: &'a SpaceId,
    #[serde(rename = "id")]
    id: &'a MessageId,
    #[serde(rename = "kd")]
    content_kind: u8,
    #[serde(rename = "mn", skip_serializing_if = "Option::is_none")]
    mentions: &'a Option<Vec<OwnerAddr>>,
    #[serde(rename = "pa", skip_serializing_if = "Option::is_none")]
    attachments: &'a Option<Vec<ChannelAttachment>>,
    #[serde(rename = "rt", skip_serializing_if = "Option::is_none")]
    reply_to: &'a Option<MessageId>,
}

/// Caller-filled pre-signature payload for a reaction. Hand to
/// `sign_channel_react` to get a wire-ready `SignedChannelEvent::React`.
pub struct ChannelReactPayload {
    pub target: MessageId,
    pub community_id: SpaceId,
    pub channel_id: ChannelId,
    pub author: OwnerAddr,
    pub at: Hlc,
    /// ZEB-541: optional custom-emoji CAS descriptor. Carried into the signed
    /// set (tamper-evident reaction→emoji binding). `None` for unicode
    /// reactions, which stay wire-identical to a pre-feature React.
    pub emoji_attachment: Option<ChannelAttachment>,
    pub emoji: String,
    pub add: bool,
}

/// Pre-signature signed-set for a React (everything except `sg`).
/// 2-char keys in RFC-8949 bytewise order: ad, at, au, ch, ci, ea, em, id.
/// `ea` (ZEB-541 custom-emoji descriptor) sorts between `ci` and `em`; it is
/// skipped when `None` so a unicode reaction's signed bytes are unchanged.
#[derive(Serialize)]
struct ChannelReactSignedSet<'a> {
    #[serde(rename = "ad")]
    add: bool,
    #[serde(rename = "at")]
    at: &'a Hlc,
    #[serde(rename = "au")]
    author: &'a OwnerAddr,
    #[serde(rename = "ch")]
    channel_id: &'a ChannelId,
    #[serde(rename = "ci")]
    community_id: &'a SpaceId,
    #[serde(rename = "ea", skip_serializing_if = "Option::is_none")]
    emoji_attachment: &'a Option<ChannelAttachment>,
    #[serde(rename = "em")]
    emoji: &'a str,
    #[serde(rename = "id")]
    target: &'a MessageId,
}

/// Errors produced by the channel-event chain (sign, encrypt/decrypt,
/// verify, replay-tracker check). Each variant maps to a distinct
/// failure step so callers can distinguish wire-truncation from
/// auth-failure from membership-rejection without parsing strings.
#[derive(thiserror::Error, Debug)]
pub enum ChannelEventError {
    #[error("CBOR encode: {0}")]
    CborEncode(String),
    #[error("CBOR decode: {0}")]
    CborDecode(String),
    #[error("AEAD encrypt: {0}")]
    AeadEncrypt(String),
    #[error("AEAD decrypt: {0}")]
    AeadDecrypt(String),
    #[error(
        "malformed packet (length {0} bytes — need at least 28: NONCE_LEN=12 + TAG_LEN=16 for an empty-plaintext AEAD packet)"
    )]
    MalformedPacket(usize),
    #[error("identity-pubkey-to-author binding mismatch")]
    AuthorPubkeyMismatch,
    #[error("signature verify failed")]
    BadSignature,
    #[error("misroute: expected community {expected_community:?} channel {expected_channel:?}, got {got_community:?}/{got_channel:?}")]
    Misroute {
        expected_community: SpaceId,
        expected_channel: ChannelId,
        got_community: SpaceId,
        got_channel: ChannelId,
    },
    #[error("identity not resolvable for author {0:?}")]
    UnknownAuthor(OwnerAddr),
    #[error("replay: event {event_id:?} from author {author:?} on device {device_id} at {at:?} not strictly greater than last seen")]
    Replay {
        event_id: MessageId,
        author: OwnerAddr,
        device_id: String,
        at: Hlc,
    },
    #[error("not authorized: {0}")]
    NotAuthorized(String),
    #[error("too many mentions: {count} (max {max})")]
    TooManyMentions { count: usize, max: usize },
    #[error("too many attachments: {count} (max {max})")]
    TooManyAttachments { count: usize, max: usize },
    #[error("reaction emoji too large: {len} bytes (max {max})")]
    EmojiTooLarge { len: usize, max: usize },
    #[error("reaction emoji must not contain a NUL byte")]
    EmojiContainsNul,
    #[error("custom emoji exceeds cap: {size} bytes (max {max})")]
    CustomEmojiTooLarge { size: u64, max: u64 },
    #[error("custom emoji must be an image (mime: {mime})")]
    CustomEmojiNotImage { mime: String },
    #[error("custom emoji react must not also carry a unicode emoji")]
    CustomEmojiWithUnicode,
    #[error("attachment name/mime too long (max {max} bytes)")]
    AttachmentFieldTooLong { max: usize },
    #[error("attachment too large: {size} bytes (max {max})")]
    AttachmentTooLarge { size: u64, max: u64 },
}

/// Sign a channel post payload with the author's identity key. Returns
/// the wire-ready `SignedChannelEvent::Post`. Pure / sync / no I/O.
///
/// `event_id` is supplied by the caller (typically a freshly-generated
/// ULID); same-length-keys invariant means we can't derive event_id
/// from the canonical CBOR digest the way community membership events
/// do, because the digest would include `at` (which contains a String
/// device_id of variable length).
///
/// Per spec §5.2. The signed-set tuple is `(id, community_id, channel_id,
/// author, at, content_kind, body, reply_to)` — every field minus the
/// signature itself.
pub fn sign_channel_event(
    payload: &ChannelPostPayload,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<SignedChannelEvent, ChannelEventError> {
    use ed25519_dalek::Signer;
    // Build the event with a placeholder sig so we can route the
    // canonical-CBOR computation through `signed_set_canonical_cbor` —
    // the single source of truth for what bytes the signature covers.
    // `signed_set_canonical_cbor`'s destructure uses `sig: _,` so the
    // placeholder is excluded from the signed-set bytes.
    //
    // Field order in the construction matches the RFC 8949 §4.2.1
    // canonical-CBOR declaration order on `ChannelPostSignedSet`:
    // at, au, bd, ch, ci, id, kd, rt (and sg last on Post). Named-
    // field syntax doesn't affect CBOR output order — only the type
    // definition's declaration order does — but we keep construction
    // order aligned for grep-ability against the wire format.
    let mut event = SignedChannelEvent::Post {
        at: payload.at.clone(),
        author: payload.author,
        body: payload.body.to_string(),
        channel_id: payload.channel_id,
        community_id: payload.community_id,
        id: payload.id,
        content_kind: payload.content_kind,
        mentions: payload.mentions.clone(),
        attachments: payload.attachments.clone(),
        reply_to: payload.reply_to,
        sig: [0u8; 64], // placeholder — overwritten below
    };
    let canon = signed_set_canonical_cbor(&event)?;
    let new_sig = signing_key.sign(&canon).to_bytes();
    if let SignedChannelEvent::Post { sig, .. } = &mut event {
        *sig = new_sig;
    }
    Ok(event)
}

/// Sign a reaction payload. Mirrors `sign_channel_event`. Pure / sync.
pub fn sign_channel_react(
    payload: &ChannelReactPayload,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<SignedChannelEvent, ChannelEventError> {
    use ed25519_dalek::Signer;
    let mut event = SignedChannelEvent::React {
        add: payload.add,
        at: payload.at.clone(),
        author: payload.author,
        channel_id: payload.channel_id,
        community_id: payload.community_id,
        emoji_attachment: payload.emoji_attachment.clone(),
        emoji: payload.emoji.clone(),
        target: payload.target,
        sig: [0u8; 64],
    };
    let canon = signed_set_canonical_cbor(&event)?;
    let new_sig = signing_key.sign(&canon).to_bytes();
    if let SignedChannelEvent::React { sig, .. } = &mut event {
        *sig = new_sig;
    }
    Ok(event)
}

/// Recompute the signed-set canonical CBOR for a SignedChannelEvent.
/// The single source of truth for what bytes the signature covers — used
/// by both sign functions (via a placeholder-sig event) and
/// `verify_channel_event` (on the deserialized event).
/// ZEB-592: stable 32-byte RBSR set-element identity for an event — the
/// SHA-256 of its canonical signed-set CBOR (the exact bytes the signature
/// covers). Content+id-derived, so two peers compute the identical hash for
/// the same event regardless of how its HLC sorts or when it arrived.
pub(crate) fn event_element_hash(event: &SignedChannelEvent) -> [u8; 32] {
    use sha2::Digest;
    // A validated in-memory event always canonically serializes (the `Vec`
    // writer is infallible), so the `Result` here is ceremonial.
    let canon = signed_set_canonical_cbor(event)
        .expect("validated channel event must canonically serialize");
    sha2::Sha256::digest(canon).into()
}

pub(crate) fn signed_set_canonical_cbor(
    event: &SignedChannelEvent,
) -> Result<Vec<u8>, ChannelEventError> {
    let mut canon = Vec::with_capacity(256);
    match event {
        SignedChannelEvent::Post {
            at,
            author,
            body,
            channel_id,
            community_id,
            id,
            content_kind,
            mentions,
            attachments,
            reply_to,
            sig: _,
        } => {
            let signed_set = ChannelPostSignedSet {
                at,
                author,
                body,
                channel_id,
                community_id,
                id,
                content_kind: *content_kind,
                mentions,
                attachments,
                reply_to,
            };
            ciborium::into_writer(&signed_set, &mut canon)
                .map_err(|e| ChannelEventError::CborEncode(e.to_string()))?;
        }
        SignedChannelEvent::React {
            add,
            at,
            author,
            channel_id,
            community_id,
            emoji_attachment,
            emoji,
            target,
            sig: _,
        } => {
            let signed_set = ChannelReactSignedSet {
                add: *add,
                at,
                author,
                channel_id,
                community_id,
                emoji_attachment,
                emoji,
                target,
            };
            ciborium::into_writer(&signed_set, &mut canon)
                .map_err(|e| ChannelEventError::CborEncode(e.to_string()))?;
        }
    }
    Ok(canon)
}

/// Encrypt a SignedChannelEvent into the wire-format packet:
///   [12B random nonce][ChaCha20-Poly1305(key=ChannelKey,
///                                        plaintext=canonical_cbor(event),
///                                        AAD=CHANNEL_PACKET_AAD)]
///
/// Per spec §5.3. Random per-packet nonce is correct here — every packet
/// is distinct on the wire. Replay protection is at the ChannelLogReplayTracker
/// layer, not at the AEAD layer.
pub fn encrypt_channel_packet(
    key: &ChannelKey,
    event: &SignedChannelEvent,
) -> Result<Vec<u8>, ChannelEventError> {
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    encrypt_channel_packet_with_nonce_inner(key, event, nonce.into())
}

/// Deterministic-nonce variant of `encrypt_channel_packet`. Used by
/// the wire-format pin tests in `tests/wire_format_channel_log_fixtures.rs`
/// to assert backfill replies (and live broadcasts) are byte-stable
/// under fixed inputs — the random-nonce variant above is the
/// production path; this is a test-only helper.
///
/// SECURITY: caller must supply a unique nonce per (key, plaintext)
/// pair. Reusing a nonce under the same key is catastrophic for
/// ChaCha20-Poly1305 confidentiality + integrity. Production code MUST
/// use `encrypt_channel_packet`; this helper exists only because pin
/// tests need byte-determinism.
///
/// Gated behind `cfg(any(test, feature = "test-fixtures"))` so it is
/// physically excluded from release builds. The `test-fixtures` feature
/// is what lets integration tests in `tests/*.rs` (which compile against
/// the crate's public API and therefore can't see `#[cfg(test)]`-only
/// items) reach this helper without re-exposing the nonce-reuse footgun
/// to production callers. CI runs `cargo test --workspace --features
/// test-fixtures` to keep these tests compilable; release builds never
/// enable the feature.
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn encrypt_channel_packet_with_nonce(
    key: &ChannelKey,
    event: &SignedChannelEvent,
    nonce: [u8; 12],
) -> Result<Vec<u8>, ChannelEventError> {
    encrypt_channel_packet_with_nonce_inner(key, event, nonce)
}

/// Internal encrypt helper — both `encrypt_channel_packet` and the
/// pin-test variant `encrypt_channel_packet_with_nonce` route through
/// this single AEAD-call site so the wire format stays in sync.
fn encrypt_channel_packet_with_nonce_inner(
    key: &ChannelKey,
    event: &SignedChannelEvent,
    nonce: [u8; 12],
) -> Result<Vec<u8>, ChannelEventError> {
    let mut plaintext = Vec::with_capacity(256);
    ciborium::into_writer(event, &mut plaintext)
        .map_err(|e| ChannelEventError::CborEncode(e.to_string()))?;
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let ciphertext = cipher
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: &plaintext,
                aad: CHANNEL_PACKET_AAD,
            },
        )
        .map_err(|e| ChannelEventError::AeadEncrypt(e.to_string()))?;
    let mut packet = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    packet.extend_from_slice(&nonce);
    packet.extend_from_slice(&ciphertext);
    Ok(packet)
}

/// Decrypt a wire packet back to a SignedChannelEvent. Splits off the
/// 12-byte nonce, AEAD-decrypts under ChannelKey + CHANNEL_PACKET_AAD,
/// canonical-CBOR decodes the result.
///
/// Caller is responsible for the §7 chain steps 3-7 (verify_channel_event)
/// once a SignedChannelEvent is in hand.
pub fn decrypt_channel_packet(
    key: &ChannelKey,
    packet: &[u8],
) -> Result<SignedChannelEvent, ChannelEventError> {
    if packet.len() < MIN_PACKET_LEN {
        return Err(ChannelEventError::MalformedPacket(packet.len()));
    }
    let (nonce_bytes, ciphertext) = packet.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let plaintext = cipher
        .decrypt(
            nonce_bytes.into(),
            Payload {
                msg: ciphertext,
                aad: CHANNEL_PACKET_AAD,
            },
        )
        .map_err(|e| ChannelEventError::AeadDecrypt(e.to_string()))?;
    ciborium::from_reader(plaintext.as_slice())
        .map_err(|e| ChannelEventError::CborDecode(e.to_string()))
}

/// ZEB-585: per-author-lane catch-up watermark. Keyed by the
/// `(author, device_id)` lane — the SAME lane identity the replay tracker
/// uses (see `replay_tracker_independent_lanes_per_author`): two authors
/// may legitimately share a `device_id`, so keying by device alone would
/// collapse their lanes and let one author's high watermark suppress the
/// other's events — re-creating the very cross-author gap this closes.
/// Maps each lane to its max `(wall_ms, logical)` in the local log; within
/// one lane `device_id` is constant, so the HLC sort-order collapses to
/// that pair.
pub type WatermarkVector = BTreeMap<(OwnerAddr, String), (u64, u32)>;

/// Static AAD for sealed watermark vectors. Domain-separated from
/// `CHANNEL_PACKET_AAD` so a reply packet can never be opened as a vector
/// (or vice-versa) even under the same `ChannelKey`.
pub const WATERMARK_VECTOR_AAD: &[u8] = b"harmony-channel-wmv-v1";

/// Hard cap on a sealed watermark-vector payload, checked on the bytes
/// view BEFORE decrypt/decode (cap-before-alloc; mirrors the pairing-scope
/// `MAX_PAIRING_WIRE_BYTES` guard). 64 KiB ≈ 1000+ device entries — far
/// above any real early-scale community; a safety valve against a
/// pathological or malicious vector. Over cap → responder ignores the
/// payload and serves via the key-expr scalar `since`.
pub const MAX_WATERMARK_VECTOR_BYTES: usize = 64 * 1024;

/// Coarse pre-seal guard on the number of `(author, device)` lanes, checked
/// BEFORE the requester materializes the CBOR + AEAD (the byte cap above
/// only guards `open`, on the responder). A channel's real lane count is
/// bounded by membership × enrolled devices; this only fires on a
/// pathological local log. The byte cap stays the authoritative on-wire
/// bound.
pub const MAX_WATERMARK_VECTOR_ENTRIES: usize = 4096;

/// Deterministic-nonce variant of [`seal_watermark_vector`] for the
/// wire-format pin tests. Same nonce-reuse footgun rationale +
/// `test-fixtures` gating as [`encrypt_channel_packet_with_nonce`];
/// production code MUST use the random-nonce [`seal_watermark_vector`].
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn seal_watermark_vector_with_nonce(
    key: &ChannelKey,
    vector: &WatermarkVector,
    nonce: [u8; 12],
) -> Result<Vec<u8>, ChannelEventError> {
    seal_watermark_vector_inner(key, vector, nonce)
}

/// Internal seal helper — both `seal_watermark_vector` and the pin-test
/// variant route through this single AEAD-call site so the wire format
/// stays in sync.
fn seal_watermark_vector_inner(
    key: &ChannelKey,
    vector: &WatermarkVector,
    nonce: [u8; 12],
) -> Result<Vec<u8>, ChannelEventError> {
    let mut plaintext = Vec::with_capacity(64);
    ciborium::into_writer(vector, &mut plaintext)
        .map_err(|e| ChannelEventError::CborEncode(e.to_string()))?;
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let ciphertext = cipher
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: &plaintext,
                aad: WATERMARK_VECTOR_AAD,
            },
        )
        .map_err(|e| ChannelEventError::AeadEncrypt(e.to_string()))?;
    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// AEAD-seal a watermark vector with a random nonce (production path).
/// Wire: `[12B nonce][ChaCha20-Poly1305(key, cbor(vector), WATERMARK_VECTOR_AAD)]`.
pub fn seal_watermark_vector(
    key: &ChannelKey,
    vector: &WatermarkVector,
) -> Result<Vec<u8>, ChannelEventError> {
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    seal_watermark_vector_inner(key, vector, nonce.into())
}

/// Open a sealed watermark vector. Rejects an oversize (or structurally
/// too-short) payload on the bytes view BEFORE any AEAD work or
/// allocation (cap-before-alloc), then AEAD-decrypts under
/// `WATERMARK_VECTOR_AAD` and canonical-CBOR decodes.
pub fn open_watermark_vector(
    key: &ChannelKey,
    packet: &[u8],
) -> Result<WatermarkVector, ChannelEventError> {
    if packet.len() > MAX_WATERMARK_VECTOR_BYTES || packet.len() < MIN_PACKET_LEN {
        return Err(ChannelEventError::MalformedPacket(packet.len()));
    }
    let (nonce_bytes, ciphertext) = packet.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let plaintext = cipher
        .decrypt(
            nonce_bytes.into(),
            Payload {
                msg: ciphertext,
                aad: WATERMARK_VECTOR_AAD,
            },
        )
        .map_err(|e| ChannelEventError::AeadDecrypt(e.to_string()))?;
    ciborium::from_reader(plaintext.as_slice())
        .map_err(|e| ChannelEventError::CborDecode(e.to_string()))
}

/// Static AAD for sealed RBSR messages (ZEB-592). Domain-separated from
/// `WATERMARK_VECTOR_AAD` and `CHANNEL_PACKET_AAD` so a message of one kind can
/// never be opened as another even under the same `ChannelKey`.
pub const RBSR_AAD: &[u8] = b"harmony-channel-rbsr-v1";

/// Hard cap on a sealed RBSR message, checked on the bytes view BEFORE
/// decrypt/decode (cap-before-alloc; mirrors `MAX_WATERMARK_VECTOR_BYTES`).
/// Over cap → the responder ignores the payload / the caller falls back.
pub const MAX_RBSR_MESSAGE_BYTES: usize = 64 * 1024;

/// Deterministic-nonce variant of [`seal_rbsr_message`] for the wire-format pin
/// tests. Same nonce-reuse footgun + `test-fixtures` gating as the other
/// `*_with_nonce` helpers; production MUST use [`seal_rbsr_message`].
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn seal_rbsr_message_with_nonce(
    key: &ChannelKey,
    msg: &crate::channel_rbsr::RbsrMessage,
    nonce: [u8; 12],
) -> Result<Vec<u8>, ChannelEventError> {
    seal_rbsr_message_inner(key, msg, nonce)
}

/// Internal seal helper — both `seal_rbsr_message` and the pin-test variant
/// route through this single AEAD-call site so the wire format stays in sync.
fn seal_rbsr_message_inner(
    key: &ChannelKey,
    msg: &crate::channel_rbsr::RbsrMessage,
    nonce: [u8; 12],
) -> Result<Vec<u8>, ChannelEventError> {
    let plaintext = crate::channel_rbsr::encode_message(msg);
    // True cap-before-alloc on the seal path (parity with `open_rbsr_message`'s
    // pre-decrypt cap): the sealed packet is exactly NONCE_LEN + plaintext.len()
    // + TAG_LEN bytes (ChaCha20-Poly1305 appends a 16-byte tag), so reject an
    // oversize message on the plaintext length BEFORE spending the AEAD encrypt
    // and ciphertext allocation. Failing locally also keeps the fallback path
    // predictable instead of surfacing as a peer-side decode failure on `open`.
    let sealed_len = NONCE_LEN + plaintext.len() + TAG_LEN;
    if sealed_len > MAX_RBSR_MESSAGE_BYTES {
        return Err(ChannelEventError::MalformedPacket(sealed_len));
    }
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let ciphertext = cipher
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: &plaintext,
                aad: RBSR_AAD,
            },
        )
        .map_err(|e| ChannelEventError::AeadEncrypt(e.to_string()))?;
    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    debug_assert_eq!(
        out.len(),
        sealed_len,
        "AEAD output length must match the pre-checked cap (nonce + plaintext + tag)"
    );
    Ok(out)
}

/// AEAD-seal an RBSR message with a random nonce (production path).
/// Wire: `[12B nonce][ChaCha20-Poly1305(key, cbor(msg), RBSR_AAD)]`.
pub fn seal_rbsr_message(
    key: &ChannelKey,
    msg: &crate::channel_rbsr::RbsrMessage,
) -> Result<Vec<u8>, ChannelEventError> {
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    seal_rbsr_message_inner(key, msg, nonce.into())
}

/// Open a sealed RBSR message. Rejects an oversize (or structurally too-short)
/// payload on the bytes view BEFORE any AEAD work or allocation (cap-before-
/// alloc), then AEAD-decrypts under `RBSR_AAD` and canonical-CBOR decodes.
pub fn open_rbsr_message(
    key: &ChannelKey,
    packet: &[u8],
) -> Result<crate::channel_rbsr::RbsrMessage, ChannelEventError> {
    if packet.len() > MAX_RBSR_MESSAGE_BYTES || packet.len() < MIN_PACKET_LEN {
        return Err(ChannelEventError::MalformedPacket(packet.len()));
    }
    let (nonce_bytes, ciphertext) = packet.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let plaintext = cipher
        .decrypt(
            nonce_bytes.into(),
            Payload {
                msg: ciphertext,
                aad: RBSR_AAD,
            },
        )
        .map_err(|e| ChannelEventError::AeadDecrypt(e.to_string()))?;
    let msg = crate::channel_rbsr::decode_message(&plaintext)
        .map_err(|_| ChannelEventError::CborDecode("rbsr message".into()))?;
    // Validate the partition invariant at the trust boundary BEFORE the state
    // machine runs — a malformed (but correctly-sealed) message must not be able
    // to drive `lo > hi` slicing or a falsely-converged catch-up.
    crate::channel_rbsr::validate_message(&msg)
        .map_err(|_| ChannelEventError::CborDecode("rbsr message: invalid partition".into()))?;
    Ok(msg)
}

/// ZEB-585: raise a watermark-vector lane entry to cover `at` (no-op when
/// the existing entry already dominates). Shared by `ChannelLog::append`
/// and `ChannelLog::rebuild_device_watermarks` so maintenance and rebuild
/// use one rule. The lane is `(author, device_id)` — see
/// [`WatermarkVector`]. Within a lane the HLC order is just
/// `(wall_ms, logical)`.
fn raise_watermark(idx: &mut WatermarkVector, author: &OwnerAddr, at: &Hlc) {
    let entry = idx.entry((*author, at.device_id.clone())).or_insert((0, 0));
    let cand = (at.wall_ms, at.logical);
    if cand > *entry {
        *entry = cand;
    }
}

/// Per-(channel, author, device) HLC monotonicity check. Records the
/// highest `Hlc` seen for each triple; rejects any new event whose
/// HLC is not strictly greater (by sort-key).
///
/// Keys: `(ChannelId, OwnerAddr, String /* device_id */)`. Mirrors the
/// shape of `CommunityRootHlcTracker` (per-device tracking, not
/// per-author). Storage grows linearly with the number of distinct
/// authoring devices that have ever posted in each channel.
///
/// Per spec §7 step 6.
#[derive(Default, Debug, Clone)]
pub struct ChannelLogReplayTracker {
    last_seen: BTreeMap<(ChannelId, OwnerAddr, String), Hlc>,
}

impl ChannelLogReplayTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read-only check: would this event be accepted by the replay
    /// tracker without mutating state? Use BEFORE running expensive
    /// authorization checks so failed-auth events don't bump
    /// `last_seen` and block valid future events on the same lane.
    /// Pair with `record` after authorization succeeds.
    ///
    /// Mirrors `community_state_sync::CommunityRootHlcTracker`'s
    /// `would_accept` / `record` split — same rationale (advance only
    /// after the full chain succeeds).
    pub fn would_accept(&self, event: &SignedChannelEvent) -> Result<(), ChannelEventError> {
        let channel_id = event.channel_id();
        let author = event.author();
        let at = event.at();
        let key = (*channel_id, *author, at.device_id.clone());
        if let Some(prev) = self.last_seen.get(&key) {
            // Use the canonical Hlc sort-key comparator from
            // `owner_state_types::Hlc::is_strictly_newer_than` — same
            // definition `CommunityRootHlcTracker::would_accept` uses.
            // device_id is constant within this key, so the comparator
            // behaves as if comparing (wall_ms, logical) only.
            if !at.is_strictly_newer_than(prev) {
                return Err(ChannelEventError::Replay {
                    event_id: *event.id(),
                    author: *author,
                    device_id: at.device_id.clone(),
                    at: at.clone(),
                });
            }
        }
        Ok(())
    }

    /// Advance the tracker to record an accepted event. Caller must
    /// have already validated via `would_accept` (and authorization
    /// checks). Idempotent for the same (key, hlc) pair only — calling
    /// twice with the same event will overwrite with an identical
    /// value but doesn't error.
    pub fn record(&mut self, event: &SignedChannelEvent) {
        let key = (
            *event.channel_id(),
            *event.author(),
            event.at().device_id.clone(),
        );
        self.last_seen.insert(key, event.at().clone());
    }

    /// Combined check + advance for callers that already serialize
    /// the two operations (e.g., the replay-tracker unit tests where
    /// no authorization gate runs between them). Production callers
    /// inside `verify_channel_event` use the split form so failed-
    /// auth events don't poison the lane.
    ///
    /// On Ok, the tracker is bumped to this event's HLC. Concurrent
    /// callers must serialize externally — the tracker holds
    /// `&mut self` and is not internally locked.
    pub fn check_and_advance(
        &mut self,
        event: &SignedChannelEvent,
    ) -> Result<(), ChannelEventError> {
        self.would_accept(event)?;
        self.record(event);
        Ok(())
    }

    /// Read-only snapshot of the current tracker state. Used by tests
    /// to assert tracker fidelity. (Phase 3's engine, when it loads a
    /// persisted log, will rebuild the tracker by walking events
    /// through `check_and_advance` rather than restoring this map
    /// directly — there's no public write seam.)
    pub fn last_seen(&self) -> &BTreeMap<(ChannelId, OwnerAddr, String), Hlc> {
        &self.last_seen
    }
}

/// IPC-facing materialized reaction summary for one emoji on one message.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReactionDto {
    /// Unicode grouping key. Empty string for a custom (CAS-backed) emoji,
    /// whose identity is carried by `emoji_cid` instead.
    pub emoji: String,
    pub count: u32,
    /// True iff the local owner currently reacts with this emoji.
    pub mine: bool,
    /// Hex `OwnerAddr` of every member currently reacting with this emoji.
    pub reactors: Vec<String>,
    /// ZEB-541: hex CID of the custom emoji blob, when this chip is a
    /// CAS-backed custom emoji. `None` for unicode reactions. Serializes
    /// as `emojiCid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji_cid: Option<String>,
    /// ZEB-541: stored blob size (bytes) of the custom emoji. `None` for
    /// unicode reactions. Serializes as `emojiSize`. Advisory render hint only,
    /// NOT a trust boundary — it is the signer-asserted descriptor size (first
    /// seen wins per key). The serve path hard-caps the fetch server-side
    /// regardless, so a wrong/hostile value cannot enlarge the render.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji_size: Option<u64>,
    /// True/false for a custom (CAS-backed) emoji: whether its CID is encrypted.
    /// `None` for unicode reactions. Serializes as `encrypted`. Lets the UI hide
    /// the "name this emoji" affordance on encrypted chips (naming is public-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted: Option<bool>,
}

/// Per-author LWW cell for a reaction: (latest HLC, currently-present).
type ReactionAuthorCell = (Hlc, bool);

/// Per-grouping-key reaction state: the retained custom-emoji descriptor
/// (`None` for unicode keys; identical across reactors of a custom key by
/// CID identity) plus the author→cell LWW map.
#[derive(Debug, Default, Clone)]
struct ReactionKeyState {
    /// The custom-emoji descriptor for this key. Set the first time a
    /// custom reaction under this key is recorded; subsequent reactors
    /// carry the same descriptor (same CID → same bytes → same size).
    descriptor: Option<ChannelAttachment>,
    authors: BTreeMap<OwnerAddr, ReactionAuthorCell>,
}

/// Inner map type for `ReactionIndex`: grouping-key → per-key state.
/// The grouping key is the unicode `emoji` string for unicode reactions and
/// a sentinel-prefixed CID-derived key (`custom_emoji_key`) for customs, so
/// the two namespaces can never collide.
type ReactionEmojiMap = BTreeMap<String, ReactionKeyState>;

/// Derive the (non-unicode-collidable) grouping key for a custom emoji from
/// its CID. The leading NUL byte cannot appear in any well-formed unicode
/// emoji grapheme string, so this key can never collide with a unicode key.
fn custom_emoji_key(cid: &[u8; 32]) -> String {
    format!("\u{0}cid:{}", hex::encode(cid))
}

/// In-memory LWW materialization of reactions over a channel's events.
/// Keyed target → key → author → (latest HLC, present), where `key` is the
/// unicode emoji string or a CID-derived sentinel key for custom emoji. Derived
/// view — always reconstructable by folding the log through `apply`.
#[derive(Debug, Default, Clone)]
pub struct ReactionIndex {
    by_target: BTreeMap<MessageId, ReactionEmojiMap>,
}

impl ReactionIndex {
    /// Fold one event in. Non-React events are ignored. LWW per
    /// (target, key, author): only the strictly-newest HLC wins. The
    /// grouping key is the unicode `emoji` string for unicode reactions
    /// and a CID-derived sentinel key for customs (so the two can't
    /// collide). For a custom reaction, the emoji descriptor is retained
    /// on the key the first time it is seen — identical across reactors
    /// of the same key by CID identity.
    pub fn apply(&mut self, event: &SignedChannelEvent) {
        let SignedChannelEvent::React {
            target,
            author,
            at,
            emoji,
            emoji_attachment,
            add,
            ..
        } = event
        else {
            return;
        };
        let key = match emoji_attachment {
            Some(att) => custom_emoji_key(&att.cid),
            None => emoji.clone(),
        };
        let state = self
            .by_target
            .entry(*target)
            .or_default()
            .entry(key)
            .or_default();
        // Retain the custom-emoji descriptor on first sight. It is identical
        // across all reactors of this key (the key is derived from the CID),
        // so a later reactor neither needs nor should change it.
        if state.descriptor.is_none() {
            if let Some(att) = emoji_attachment {
                state.descriptor = Some(att.clone());
            }
        }
        match state.authors.get(author) {
            Some((prev_hlc, _)) if !at.is_strictly_newer_than(prev_hlc) => { /* stale — ignore */
            }
            _ => {
                state.authors.insert(*author, (at.clone(), *add));
            }
        }
    }

    /// Materialize the reaction summary for a message. Keys with zero
    /// present reactors are omitted. Deterministic order (BTreeMap).
    /// Custom emoji surface `emoji_cid`/`emoji_size` from the retained
    /// descriptor with an empty unicode `emoji`; unicode surface the
    /// `emoji` string with both CID fields `None`.
    pub fn reactions_for(&self, target: &MessageId, me: &OwnerAddr) -> Vec<ReactionDto> {
        let Some(by_key) = self.by_target.get(target) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (key, state) in by_key {
            let present: Vec<&OwnerAddr> = state
                .authors
                .iter()
                .filter(|(_, (_, add))| *add)
                .map(|(a, _)| a)
                .collect();
            if present.is_empty() {
                continue;
            }
            let (emoji, emoji_cid, emoji_size, encrypted) = match &state.descriptor {
                Some(att) => (
                    String::new(),
                    Some(hex::encode(att.cid)),
                    Some(att.size),
                    Some(
                        harmony_content::cid::ContentId::from_bytes(att.cid)
                            .flags()
                            .encrypted,
                    ),
                ),
                None => (key.clone(), None, None, None),
            };
            out.push(ReactionDto {
                emoji,
                count: present.len() as u32,
                mine: present.contains(&me),
                reactors: present.iter().map(|a| hex::encode(a.0)).collect(),
                emoji_cid,
                emoji_size,
                encrypted,
            });
        }
        out
    }
}

/// Snapshot of community state at a particular HLC, exposing just
/// what `verify_channel_event` needs. Phase 3's engine produces this
/// by materializing the community-state CRDT to `event.at`.
///
/// **Async by design.** The production adapter
/// (`CommunityStateAtHlcAdapter`) wraps an `Arc<tokio::sync::Mutex<CommunityState>>`
/// — the same Mutex Phase 1's `CommunitySyncEngine` uses for its CRDT.
/// Locking that Mutex requires `.await`, so the trait methods must be
/// async. Mocks (`MockState` / `AlwaysJoinedState`) implement the
/// async signature trivially since their data is in-memory.
///
/// **Single-method contract.** The trait exposes ONE method
/// (`snapshot_at`) that returns BOTH the channel-config and the
/// author's power level in a single materialized snapshot. The previous
/// shape (two methods, called sequentially with `.await` between them)
/// allowed a torn read: the production adapter re-acquires the live
/// `Arc<Mutex<CommunityState>>` lock on each call and re-materializes,
/// so a CRDT update landing between the two awaits — membership change,
/// channel deletion, write_power change — would let `verify_channel_event`
/// decide based on data that NEVER coexisted at one HLC. Returning
/// both values from one call under one lock acquisition closes that
/// authorization-bug hole.
#[async_trait::async_trait]
pub trait CommunityStateAtHlc {
    /// Materialize the community state at `at` and return BOTH the
    /// channel info and the author's effective power in a single
    /// atomic snapshot. The production adapter takes the
    /// `Arc<Mutex<CommunityState>>` lock once and projects both
    /// values from one materialization, guaranteeing they reflect
    /// the same state. Failed lookups are surfaced as `None` on the
    /// individual snapshot fields (channel didn't exist at `at` /
    /// author wasn't Joined at `at`).
    async fn snapshot_at(
        &self,
        channel_id: &ChannelId,
        author: &OwnerAddr,
        at: &Hlc,
    ) -> CommunityStateSnapshot;
}

/// Atomic snapshot returned by `CommunityStateAtHlc::snapshot_at`.
/// Both fields reflect a single materialized state at the requested
/// HLC, so verify-time authorization decisions are coherent — see the
/// trait doc-comment for the torn-read failure mode this prevents.
#[derive(Clone, Debug)]
pub struct CommunityStateSnapshot {
    /// Channel-config slice at the requested HLC. `None` if the
    /// channel didn't exist at `at`.
    pub channel: Option<ChannelInfo>,
    /// Author's effective power level at the requested HLC. `None`
    /// if the author was not Joined (Left / Banned / never-present);
    /// `Some(0)` for a Joined member with no explicit power-level
    /// entry (`power_levels` defaults to 0).
    pub author_power: Option<u8>,
    /// The author's materialized enrolled device verifying keys
    /// (ed25519, 32-byte) as of the requested HLC — the SAME
    /// `MemberState.enrolled_device_keys` set that root-publish auth
    /// (`community_state_sync::verify_publisher_sig`) trusts. ZEB-399:
    /// `verify_channel_event` checks the post signature against these.
    /// A channel post is signed by the author's enrolled device key #2,
    /// NOT the owner identity key, so authorship is proven by community
    /// membership — not by a DM-layer owner→identity cache that isn't
    /// populated on a community-invite first contact (the ZEB-399 bug).
    /// Empty for a non-member, or for a member with no materialized
    /// enrolled key (an anomaly).
    pub author_enrolled_keys: Vec<[u8; 32]>,
}

/// Run the §7 chain steps 3-7 on a pre-decrypted SignedChannelEvent.
/// On Ok, the event is wire-valid + identity-valid + signature-valid +
/// not-replayed + author-authorized at event.at. The replay tracker
/// is advanced as a side effect on Ok.
///
/// Step 1 (AEAD decrypt) and Step 2 (CBOR decode) are run by
/// `decrypt_channel_packet` before this function. Step 8 (append to
/// log + notify subscribers) is the caller's responsibility (Phase 3
/// engine).
///
/// The chain order matches the spec — cheapest checks first to drop
/// garbage early without expensive identity/membership lookups.
pub async fn verify_channel_event<S>(
    event: &SignedChannelEvent,
    expected_community_id: &SpaceId,
    expected_channel_id: &ChannelId,
    state: &S,
    replay_tracker: &mut ChannelLogReplayTracker,
) -> Result<(), ChannelEventError>
where
    S: CommunityStateAtHlc + Sync + ?Sized,
{
    let community_id = event.community_id();
    let channel_id = event.channel_id();
    let author = event.author();
    let at = event.at();
    let sig = event.sig();
    // `mentions`/`attachments` are Post-only; a React event carries neither,
    // so the ZEB-534/535 inbound caps below are no-ops for reactions.
    let (mentions, attachments) = match event {
        SignedChannelEvent::Post {
            mentions,
            attachments,
            ..
        } => (mentions.as_ref(), attachments.as_ref()),
        SignedChannelEvent::React { .. } => (None, None),
    };

    // Step 2.5 (ZEB-534): inbound mentions cap. MAX_MENTIONS is enforced
    // on the local mint path (publish) and at the IPC boundary; enforce it
    // here too so a remote peer cannot bypass the cap with a signed event
    // carrying an oversized `mn` array (which would otherwise be accepted,
    // appended, projected to a DTO, and re-broadcast). Cheap structural
    // check — runs before the async state materialization.
    if let Some(m) = mentions {
        if m.len() > MAX_MENTIONS {
            return Err(ChannelEventError::TooManyMentions {
                count: m.len(),
                max: MAX_MENTIONS,
            });
        }
    }

    // Step 2.6 (ZEB-535): inbound attachments cap + per-field length cap.
    // Same rationale as the mentions cap above — a remote peer can sign an
    // oversized `pa` array or over-long name/mime; reject before the event is
    // appended, projected to a DTO, and re-broadcast. Cheap structural check.
    if let Some(a) = attachments {
        if a.len() > MAX_ATTACHMENTS {
            return Err(ChannelEventError::TooManyAttachments {
                count: a.len(),
                max: MAX_ATTACHMENTS,
            });
        }
        for att in a {
            if att.name.len() > MAX_ATTACHMENT_FIELD_BYTES
                || att.mime.len() > MAX_ATTACHMENT_FIELD_BYTES
            {
                return Err(ChannelEventError::AttachmentFieldTooLong {
                    max: MAX_ATTACHMENT_FIELD_BYTES,
                });
            }
            // ZEB-539: reject an attachment whose signed size exceeds the
            // artifact cap — it could never be downloaded (the download path
            // rejects size > cap), so a peer must not be able to commit one.
            if att.size > MAX_ATTACHMENT_SIZE {
                return Err(ChannelEventError::AttachmentTooLarge {
                    size: att.size,
                    max: MAX_ATTACHMENT_SIZE,
                });
            }
        }
    }

    // Step 3: misroute defense.
    if community_id != expected_community_id || channel_id != expected_channel_id {
        return Err(ChannelEventError::Misroute {
            expected_community: *expected_community_id,
            expected_channel: *expected_channel_id,
            got_community: *community_id,
            got_channel: *channel_id,
        });
    }

    // ZEB-536: bound reaction emoji size (cheap, pre-auth). Use a dedicated
    // error variant (mirrors TooManyMentions/TooManyAttachments) so an inbound
    // emoji-cap rejection is distinguishable from a membership/authorization
    // failure without string-parsing — logging, metrics, and error-mapping can
    // tell the two apart (Greptile PR #314).
    if let SignedChannelEvent::React {
        emoji,
        emoji_attachment,
        ..
    } = event
    {
        if emoji.len() > MAX_REACTION_EMOJI_BYTES {
            return Err(ChannelEventError::EmojiTooLarge {
                len: emoji.len(),
                max: MAX_REACTION_EMOJI_BYTES,
            });
        }
        // The reaction index keys custom emoji under a NUL-prefixed sentinel
        // (`\0cid:<hex>`) to keep them disjoint from unicode emoji strings. A
        // 64-hex custom key can't fit under MAX_REACTION_EMOJI_BYTES today, so a
        // unicode emoji can't collide — but reject any NUL in the unicode emoji
        // to keep the two key-spaces provably disjoint by construction (and
        // future-proof against a larger cap). No legitimate emoji contains NUL.
        if emoji.as_bytes().contains(&0) {
            return Err(ChannelEventError::EmojiContainsNul);
        }
        // ZEB-541: a React may carry at most ONE custom-emoji CAS descriptor
        // (structurally guaranteed by the `Option` type). When present, apply
        // the same cheap pre-auth caps the artifact path uses: a signed blob
        // larger than the serve cap could never be previewed, and a non-image
        // mime can't render in the emoji chip — reject both before the async
        // membership/signature gate. The signature already covers
        // `emoji_attachment` (it's in the signed set), so a peer cannot rebind
        // a reaction to a different emoji CID without invalidating `sg`.
        if let Some(att) = emoji_attachment {
            // Protocol invariant (CodeRabbit PR #320): a custom react is EITHER
            // unicode OR a CAS descriptor, never both — a peer sending both
            // produces an ambiguous reaction-index key. The mint boundary
            // rejects this too; verify binds remote peers.
            if !emoji.is_empty() {
                return Err(ChannelEventError::CustomEmojiWithUnicode);
            }
            // Per-field length cap, parity with the Post-attachment path: a
            // remote peer can sign a React with an over-long `name`/`mime`
            // (our own mint forces `name=""`, but verify must bound a hostile
            // descriptor) that would bloat the log and every projected DTO.
            if att.name.len() > MAX_ATTACHMENT_FIELD_BYTES
                || att.mime.len() > MAX_ATTACHMENT_FIELD_BYTES
            {
                return Err(ChannelEventError::AttachmentFieldTooLong {
                    max: MAX_ATTACHMENT_FIELD_BYTES,
                });
            }
            if att.size > crate::MAX_CUSTOM_EMOJI_BYTES {
                return Err(ChannelEventError::CustomEmojiTooLarge {
                    size: att.size,
                    max: crate::MAX_CUSTOM_EMOJI_BYTES,
                });
            }
            if !att.mime.starts_with("image/") {
                return Err(ChannelEventError::CustomEmojiNotImage {
                    mime: att.mime.clone(),
                });
            }
        }
    }

    // Step 3b (precondition — moved earlier per cheapest-first ordering):
    // cheap replay-rejection check. Read-only — does NOT advance the
    // tracker. Authorization could still reject below; we only `record`
    // after the full chain succeeds. Mirrors `community_state_sync::
    // CommunityRootHlcTracker`'s would_accept/record split — failed-auth
    // events MUST NOT bump `last_seen` or they permanently block valid
    // future events on the same (channel, author, device) lane.
    //
    // Order trade-off: a message that's both a replay AND fails author
    // authorization now returns Replay. This is fine — replay-rejection
    // is a stronger signal, and the cheap O(log N) BTreeMap lookup must
    // run before the async state materialization (`snapshot_at`).
    replay_tracker.would_accept(event)?;

    // Step 4: materialize the community state at `at` ONCE. The snapshot
    // carries the channel-config, the author's effective power, AND the
    // author's enrolled device keys — all from a single materialization so
    // verify-time decisions are coherent (see the `CommunityStateAtHlc`
    // trait doc for the torn-read failure mode this prevents).
    let snapshot = state.snapshot_at(channel_id, author, at).await;

    // Step 5: membership-at-HLC gate. Both `write_power` and the
    // tombstone (`deleted_at`) are evaluated AS OF event.at, not as
    // of "now" — channel-config events between post-time and verify-
    // time may have raised/lowered the threshold or deleted the channel.
    let channel_info = snapshot.channel.ok_or_else(|| {
        ChannelEventError::NotAuthorized(format!(
            "channel {:?} did not exist at {:?}",
            channel_id, at
        ))
    })?;
    if let Some(deleted_at) = &channel_info.deleted_at {
        // Strictly-newer-than: post happened AFTER deletion is rejected.
        // Posts at exactly deleted_at or earlier are still valid.
        if at.is_strictly_newer_than(deleted_at) {
            return Err(ChannelEventError::NotAuthorized(format!(
                "channel deleted at {:?}, post at {:?}",
                deleted_at, at
            )));
        }
    }
    let author_power = snapshot.author_power.ok_or_else(|| {
        ChannelEventError::NotAuthorized(format!("author {:?} not Joined at {:?}", author, at))
    })?;
    if author_power < channel_info.write_power {
        return Err(ChannelEventError::NotAuthorized(format!(
            "author power {} < channel write_power {}",
            author_power, channel_info.write_power
        )));
    }

    // Step 6: signature verify against the author's MATERIALIZED enrolled
    // device keys (ZEB-399). A channel post is signed by the author's
    // enrolled device key #2 — the SAME trust root root-publish auth uses
    // (`community_state_sync::verify_publisher_sig`). Authorship is proven
    // by community membership, NOT by a DM-layer owner→identity cache
    // (which isn't populated on a community-invite first contact — the
    // ZEB-399 bug). The author was just confirmed Joined-at-`at` by the
    // power gate; an empty enrolled set for a Joined member is an anomaly
    // (a member always carries ≥1 enrolled key from their EnrollmentCert-
    // bearing Join) — surface it as UnknownAuthor for a clear diagnostic.
    if snapshot.author_enrolled_keys.is_empty() {
        return Err(ChannelEventError::UnknownAuthor(*author));
    }
    // verify_strict: rejects non-canonical S + small-order R, matching
    // community_membership::verify_signature and verify_publisher_sig.
    let canon = signed_set_canonical_cbor(event)?;
    let parsed_sig = ed25519_dalek::Signature::from_bytes(sig);
    let sig_ok = snapshot.author_enrolled_keys.iter().any(|key_bytes| {
        ed25519_dalek::VerifyingKey::from_bytes(key_bytes)
            .map(|vk| vk.verify_strict(&canon, &parsed_sig).is_ok())
            .unwrap_or(false)
    });
    if !sig_ok {
        return Err(ChannelEventError::BadSignature);
    }

    // Step 8: now that authorization succeeded, commit the tracker
    // advance. Failed-auth events do NOT mutate replay state — this
    // prevents an attacker who can produce a syntactically-valid but
    // unauthorized event from poisoning the (channel, author, device)
    // lane against the legitimate device.
    replay_tracker.record(event);

    Ok(())
}

/// Configuration for `ChannelLog::new`. Production passes
/// `DEFAULT_SEAL_THRESHOLD_EVENTS`; tests pass a smaller value to
/// exercise seal/reload paths in reasonable time.
#[derive(Clone, Debug)]
pub struct ChannelLogConfig {
    /// Number of events in `tail` that triggers a seal. After seal,
    /// tail is reset to empty and a new SegmentDescriptor is appended
    /// to the manifest.
    pub seal_threshold_events: usize,
}

/// Per spec §8 — production seal threshold. Tests should override
/// to a small value (e.g., 8) via `ChannelLogConfig`.
pub const DEFAULT_SEAL_THRESHOLD_EVENTS: usize = 1024;

/// Schema version byte prefixed to `manifest.cbor`. v3 will widen
/// to handle CasBook segment handles, additional manifest fields,
/// etc. — this byte lets reload dispatch on format version.
const CHANNEL_LOG_MANIFEST_V1: u8 = 1;
/// Schema version byte prefixed to `tail.cbor`. v3 may add per-tail
/// metadata (e.g., last_sealed_through HLC for crash-safety).
const CHANNEL_LOG_TAIL_V1: u8 = 1;
/// Schema version byte prefixed to each `segments/{N:08x}.cbor`.
/// v3 may add per-segment metadata or compression.
const CHANNEL_LOG_SEGMENT_V1: u8 = 1;
/// Schema version byte prefixed to the `backfill_state.cbor` sidecar
/// (ZEB-599). Bump when the sidecar gains fields.
const CHANNEL_BACKFILL_STATE_V1: u8 = 1;

/// Per-channel anti-entropy sidecar (ZEB-599), persisted at
/// `root/backfill_state.cbor` next to the manifest.
///
/// Holds the wall-clock ms of the last full (`since = None`) periodic
/// reconcile. The ZEB-425/584 resync floor arms its FIRST fire this
/// session at `last_full_reconcile_ms + interval` instead of
/// `spawn + interval`, so the ~1h floor survives process restarts
/// rather than resetting its clock on every respawn (a node restarting
/// more often than hourly otherwise never crosses the floor, and the
/// only backstop for a sub-max-HLC gap never fires).
///
/// Written independently of segment-sealing (via [`Self::save`]) so it
/// lands even for a quiet channel that never seals a segment — exactly
/// the case where a peer's offline-window backlog matters most. It is a
/// pure optimization hint: an absent, unreadable, or unknown-version
/// sidecar degrades to [`load`](Self::load) returning `None` (⇒ legacy
/// interval-from-spawn), never an error that could block spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelBackfillState {
    /// Wall-clock ms (UNIX epoch) of the last full periodic reconcile.
    pub last_full_reconcile_ms: u64,
}

impl ChannelBackfillState {
    /// Sidecar path under a channel-log `root` directory.
    fn path(root: &std::path::Path) -> PathBuf {
        root.join("backfill_state.cbor")
    }

    /// Decode sidecar bytes. `None` on empty, unknown-version, or corrupt
    /// content — all mean "never reconciled," so the caller falls back to
    /// interval-from-spawn. Deliberately non-erroring: a corrupt hint must
    /// never block a channel from spawning.
    fn parse(bytes: &[u8]) -> Option<ChannelBackfillState> {
        match bytes.split_first() {
            Some((&CHANNEL_BACKFILL_STATE_V1, rest)) => ciborium::from_reader(rest).ok(),
            _ => None,
        }
    }

    /// Read the sidecar synchronously (tests / non-async callers). `None`
    /// on file absent / unreadable / [`parse`](Self::parse) failure.
    pub fn load(root: &std::path::Path) -> Option<ChannelBackfillState> {
        Self::parse(&std::fs::read(Self::path(root)).ok()?)
    }

    /// Async sibling of [`load`](Self::load) for the channel-log spawn
    /// path, which must not park a tokio worker on filesystem I/O
    /// (ZEB-467 — mirrors the `tokio::fs::create_dir_all` used there;
    /// Qodo #380). Same `None`-on-any-error contract.
    pub async fn load_async(root: &std::path::Path) -> Option<ChannelBackfillState> {
        Self::parse(&tokio::fs::read(Self::path(root)).await.ok()?)
    }

    /// Atomically persist the sidecar with `last_full_reconcile_ms`.
    /// Errors surface to the caller, which logs-and-continues: a missed
    /// write only forfeits restart-awareness for that one cycle (the
    /// floor falls back to interval-from-spawn next boot).
    pub fn save(
        root: &std::path::Path,
        last_full_reconcile_ms: u64,
    ) -> Result<(), crate::owner_state_persist::PersistError> {
        let mut bytes = Vec::with_capacity(16);
        bytes.push(CHANNEL_BACKFILL_STATE_V1);
        ciborium::into_writer(
            &Self {
                last_full_reconcile_ms,
            },
            &mut bytes,
        )
        .map_err(|e| crate::owner_state_persist::PersistError::Io(std::io::Error::other(e)))?;
        crate::owner_state_persist::save_atomically(&Self::path(root), &bytes)
    }
}

impl Default for ChannelLogConfig {
    fn default() -> Self {
        Self {
            seal_threshold_events: DEFAULT_SEAL_THRESHOLD_EVENTS,
        }
    }
}

/// Per-channel segmented append-only log. In-memory `tail` plus
/// sealed segments on disk referenced by a manifest.
///
/// Per spec §8.
#[derive(Debug)]
pub struct ChannelLog {
    pub manifest: ChannelLogManifest,
    pub tail: Vec<SignedChannelEvent>,
    config: ChannelLogConfig,
    /// Root directory: `<identity_dir>/communities/{cid_hex}/channels/{ch_id_hex}/`.
    /// Manifest at `root/manifest.cbor`, tail at `root/tail.cbor`,
    /// sealed segments at `root/segments/{N:08x}.cbor`.
    root: PathBuf,
    /// ZEB-536: derived LWW reaction view. Maintained in `append`;
    /// rebuilt from the persisted log in `reload`.
    reaction_index: ReactionIndex,
    /// ZEB-585: derived per-author (per authoring-device) catch-up
    /// watermark — `device_id -> max (wall_ms, logical)`. Maintained in
    /// `append`; rebuilt from the persisted log in `reload` (mirrors
    /// `reaction_index`). Keeps `watermark_vector()` O(devices), not an
    /// O(history) rescan per catch-up query.
    device_watermarks: WatermarkVector,
    /// ZEB-592: in-memory canonical-order `(ReconcileKey, element_hash)` mirror
    /// of the whole log + a content-defined-chunk fingerprint index over it.
    /// Built in `reload`, maintained in `append`. Lets RBSR range fingerprints
    /// be served from memory — the per-query O(history) segment rescan the
    /// watermark-vector path pays (ZEB-585 §A.6) is gone. (A memory-frugal
    /// chunk-summary-only variant that reads boundary events from disk is a
    /// documented Slice 2b follow-up.)
    ///
    /// Invariant: `entry.1 == entry.0.3` — the second field always equals the
    /// `ReconcileKey`'s element-hash (its fourth tuple field). The duplication
    /// is intentional: [`ChunkIndex`] folds whatever hash it is handed and never
    /// assumes `hash == key.3`, which keeps that module hash-agnostic and its
    /// fingerprint algebra testable in isolation. Dropping the in-memory hash
    /// entirely belongs to the Slice 2b chunk-summary-only rework, not here.
    reconcile_entries: Vec<(ReconcileKey, [u8; 32])>,
    chunk_index: ChunkIndex,
}

/// On-disk index of sealed segments + the path to the active tail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelLogManifest {
    pub community_id: SpaceId,
    pub channel_id: ChannelId,
    /// Ordered ascending by `range.0` (first-event HLC) for fast
    /// backfill walk in Phase 3.
    pub segments: Vec<SegmentDescriptor>,
}

/// Manifest entry for one sealed segment. Stores the HLC range covered
/// (for backfill filtering), the event count, and the storage handle
/// that locates the segment data (currently only on-disk files;
/// `SegmentHandle::CasBook` is reserved for v3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentDescriptor {
    /// `(first_event.at, last_event.at)` inclusive. Used by Phase 3
    /// backfill to filter which segments overlap a `since` query.
    pub range: (Hlc, Hlc),
    pub count: u32,
    pub handle: SegmentHandle,
}

/// Storage location of a sealed segment. v2 ships only `LocalFile`;
/// `CasBook { cid }` is reserved for v3 to allow content-addressable
/// dedupe of segments across replicas. The tagged-enum encoding keeps
/// old `LocalFile` segments and new `CasBook` segments coexistent in
/// a single manifest indefinitely.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SegmentHandle {
    /// v2: local-disk segment, path relative to the channel's root dir.
    #[serde(rename = "f")]
    LocalFile { rel_path: String },
    // v3 reserved (additive — no v2 wire-format break):
    // #[serde(rename = "c")] CasBook { cid: ContentId },
}

/// Errors produced by `ChannelLog`'s persistence layer. Distinct from
/// `ChannelEventError` (which covers the wire/verify chain) so callers
/// can reason about disk failures vs cryptographic failures separately.
#[derive(thiserror::Error, Debug)]
pub enum ChannelLogPersistError {
    #[error("io: {0}")]
    Io(String),
    #[error("cbor encode: {0}")]
    CborEncode(String),
    #[error("cbor decode: {0}")]
    CborDecode(String),
    #[error("manifest mismatch: expected {expected:?}, got {got:?}")]
    Manifest { expected: String, got: String },
}

impl From<std::io::Error> for ChannelLogPersistError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl ChannelLog {
    /// Build a fresh empty log. Doesn't touch disk — `flush_tail` and
    /// `seal_and_persist` are explicit. The Phase 3 engine will call
    /// `reload` on startup if the directory already exists.
    pub fn new(
        community_id: SpaceId,
        channel_id: ChannelId,
        root: PathBuf,
        config: ChannelLogConfig,
    ) -> Self {
        Self {
            manifest: ChannelLogManifest {
                community_id,
                channel_id,
                segments: Vec::new(),
            },
            tail: Vec::new(),
            config,
            root,
            reaction_index: ReactionIndex::default(),
            device_watermarks: WatermarkVector::new(),
            reconcile_entries: Vec::new(),
            chunk_index: ChunkIndex::new(),
        }
    }

    /// Borrow the config this log was built with. Phase 3's flush
    /// loop reads `seal_threshold_events` to drive the seal-on-tail-
    /// length policy from outside the per-append `Ok(seal_ready)`
    /// signal — necessary because the flush loop also handles tails
    /// that were appended directly by tests bypassing the engine's
    /// publish path.
    pub fn config(&self) -> &ChannelLogConfig {
        &self.config
    }

    /// Push a verified event onto the in-memory tail. Validates that
    /// the event is bound to this log's `(community_id, channel_id)`
    /// — a caller bug or hostile feed that mixes events from
    /// different channels would otherwise silently corrupt the log
    /// (the per-stored-event binding isn't re-checked on reload —
    /// only the manifest is). Returns `Ok(true)` if the seal
    /// threshold has now been reached (caller should call
    /// `seal_and_persist`); `Ok(false)` otherwise.
    pub fn append(&mut self, event: SignedChannelEvent) -> Result<bool, ChannelLogPersistError> {
        let community_id = event.community_id();
        let channel_id = event.channel_id();
        if *community_id != self.manifest.community_id || *channel_id != self.manifest.channel_id {
            return Err(ChannelLogPersistError::Manifest {
                expected: format!(
                    "{:?}/{:?}",
                    self.manifest.community_id, self.manifest.channel_id
                ),
                got: format!("{:?}/{:?}", community_id, channel_id),
            });
        }
        // ZEB-536: maintain the derived reaction view at the single
        // append choke point (covers local react, inbound, backfill).
        if matches!(&event, SignedChannelEvent::React { .. }) {
            self.reaction_index.apply(&event);
        }
        // ZEB-585: advance the per-(author, device) catch-up watermark
        // before the event is moved into the tail.
        raise_watermark(&mut self.device_watermarks, event.author(), event.at());
        // ZEB-592: maintain the RBSR reconcile index before the event moves.
        self.maintain_reconcile_index(&event);
        self.tail.push(event);
        Ok(self.tail.len() >= self.config.seal_threshold_events)
    }

    /// Persist the active tail to `root/tail.cbor`. Atomic-rename via
    /// `tempfile`. Idempotent; safe to call repeatedly.
    ///
    /// Also persists `root/manifest.cbor` if it doesn't already exist
    /// on disk. This preserves the invariant that any persisted tail is
    /// recoverable on reload (which requires a manifest to validate
    /// community/channel binding before loading the tail). The stub
    /// manifest written here has `segments` empty; `seal_and_persist`
    /// unconditionally rewrites `manifest.cbor` with the real segment
    /// list, so the stub is genuinely transient and never observably
    /// escapes its first-flush window.
    pub fn flush_tail(&self) -> Result<(), ChannelLogPersistError> {
        std::fs::create_dir_all(&self.root)?;
        let manifest_path = self.root.join("manifest.cbor");
        if !manifest_path.exists() {
            let mut man_bytes = Vec::with_capacity(256);
            man_bytes.push(CHANNEL_LOG_MANIFEST_V1);
            ciborium::into_writer(&self.manifest, &mut man_bytes)
                .map_err(|e| ChannelLogPersistError::CborEncode(e.to_string()))?;
            crate::owner_state_persist::save_atomically(&manifest_path, &man_bytes)
                .map_err(|e| ChannelLogPersistError::Io(e.to_string()))?;
        }
        let mut bytes = Vec::with_capacity(1024);
        bytes.push(CHANNEL_LOG_TAIL_V1);
        ciborium::into_writer(&self.tail, &mut bytes)
            .map_err(|e| ChannelLogPersistError::CborEncode(e.to_string()))?;
        let tail_path = self.root.join("tail.cbor");
        crate::owner_state_persist::save_atomically(&tail_path, &bytes)
            .map_err(|e| ChannelLogPersistError::Io(e.to_string()))?;
        Ok(())
    }

    /// Seal the current tail to a new segment file and append a
    /// SegmentDescriptor to the manifest. Resets the in-memory tail
    /// to empty and re-persists both manifest and (now-empty) tail.
    /// Atomic per-file via `save_atomically`.
    ///
    /// Crash semantics. The on-disk write order is:
    /// 1. Write segment file (atomic).
    /// 2. Persist empty `tail.cbor` (atomic).
    /// 3. Write manifest with the new descriptor appended (atomic).
    /// 4. Commit in-memory state (clear tail + adopt new manifest).
    ///
    /// In-memory state is mutated ONLY after all three disk writes
    /// succeed — failure at any step preserves the in-memory tail and
    /// manifest so the caller can retry. The previous shape cleared
    /// the in-memory tail before the empty-tail flush, which dropped
    /// the only in-memory copy on a flush I/O error (recovery
    /// impossible — events were neither in memory nor in any
    /// reload-discoverable on-disk location).
    ///
    /// Crash points:
    /// - After (1): orphan segment file, manifest unchanged, tail
    ///   unchanged on disk. Reload sees the old N-1 segments + the
    ///   original tail; orphan segment is ignored (not in manifest)
    ///   and is overwritten by the next seal at the same index.
    /// - After (2): orphan segment file, `tail.cbor` empty on disk,
    ///   manifest unchanged. In-memory tail still holds the events.
    ///   On clean retry of `seal_and_persist`, the segment is
    ///   re-written at the same index (overwrite-safe) and the
    ///   manifest write completes. On crash + reload before retry,
    ///   the at-most-one-segment-worth of events that were in the
    ///   tail are lost — they exist only in the orphan segment which
    ///   reload doesn't discover. Acceptable per spec §8 (better
    ///   than re-emitting them as duplicates against the now-sealed
    ///   segment).
    /// - After (3): orphan segment file, tail empty on disk, manifest
    ///   updated. In-memory state still holds the (now stale) tail
    ///   contents and the (now stale) old manifest. On clean retry,
    ///   step 4 commits in-memory state. On crash, reload picks up
    ///   the new manifest + empty tail — same outcome as the
    ///   successful path. No data loss.
    /// - After (4): clean state. In-memory and on-disk both reflect
    ///   the new manifest with N+1 segments and empty tail.
    pub fn seal_and_persist(&mut self) -> Result<(), ChannelLogPersistError> {
        if self.tail.is_empty() {
            // Nothing to seal. No-op.
            return Ok(());
        }
        std::fs::create_dir_all(self.root.join("segments"))?;
        let next_index = self.manifest.segments.len() as u32;
        let rel_path = format!("segments/{:08x}.cbor", next_index);
        let abs_path = self.root.join(&rel_path);

        // Step 1: write segment file (segment-level atomic via save_atomically).
        let mut seg_bytes = Vec::with_capacity(64 * self.tail.len());
        seg_bytes.push(CHANNEL_LOG_SEGMENT_V1);
        ciborium::into_writer(&self.tail, &mut seg_bytes)
            .map_err(|e| ChannelLogPersistError::CborEncode(e.to_string()))?;
        crate::owner_state_persist::save_atomically(&abs_path, &seg_bytes)
            .map_err(|e| ChannelLogPersistError::Io(e.to_string()))?;

        // Compute true min/max HLC across the segment. Events aren't
        // globally HLC-monotonic across authors/devices (only per-lane
        // via ChannelLogReplayTracker), so first()/last() in append
        // order can give wrong bounds. Phase 3 backfill uses
        // SegmentDescriptor.range to filter which segments overlap a
        // `since` query — wrong bounds would silently skip segments
        // containing matching events. Hlc has no `Ord` derive (the
        // device_id String tuple position would force allocation in
        // any Ord impl), so use is_strictly_newer_than directly via
        // min_by/max_by.
        let first_at = self
            .tail
            .iter()
            .map(|e| e.at())
            .min_by(|a, b| {
                if a.is_strictly_newer_than(b) {
                    std::cmp::Ordering::Greater
                } else if b.is_strictly_newer_than(a) {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .expect("tail non-empty checked above")
            .clone();
        let last_at = self
            .tail
            .iter()
            .map(|e| e.at())
            .max_by(|a, b| {
                if a.is_strictly_newer_than(b) {
                    std::cmp::Ordering::Greater
                } else if b.is_strictly_newer_than(a) {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .expect("tail non-empty checked above")
            .clone();
        let descriptor = SegmentDescriptor {
            range: (first_at, last_at),
            count: self.tail.len() as u32,
            handle: SegmentHandle::LocalFile { rel_path },
        };

        // Step 2: persist empty tail to disk (writes a fresh tail.cbor
        // containing the empty-vec marker). Do this BEFORE mutating
        // self.tail in memory — if save_atomically fails, the in-memory
        // tail is preserved and the error propagates. Worst case on
        // success: orphan segment file is on disk; reload ignores
        // segments not in the manifest. We bypass `flush_tail()` here
        // because that helper would also write a stub manifest if the
        // manifest doesn't yet exist on disk — at this point we're
        // about to write the real manifest in step 3, so the stub
        // would just be overwritten.
        let empty_tail: Vec<SignedChannelEvent> = Vec::new();
        let mut empty_tail_bytes = Vec::with_capacity(8);
        empty_tail_bytes.push(CHANNEL_LOG_TAIL_V1);
        ciborium::into_writer(&empty_tail, &mut empty_tail_bytes)
            .map_err(|e| ChannelLogPersistError::CborEncode(e.to_string()))?;
        crate::owner_state_persist::save_atomically(
            &self.root.join("tail.cbor"),
            &empty_tail_bytes,
        )
        .map_err(|e| ChannelLogPersistError::Io(e.to_string()))?;

        // Step 3: build and persist the new manifest (with the
        // descriptor appended). Build on a CLONED + extended segment
        // list — if save_atomically fails, neither in-memory manifest
        // nor in-memory tail is mutated, so we can safely retry.
        let mut new_segments = self.manifest.segments.clone();
        new_segments.push(descriptor);
        // Maintain the documented "ascending by range.0" invariant.
        // verify_channel_event only guarantees per-(channel, author,
        // device) HLC monotonicity — a late event from a new lane can
        // produce a seal whose range.0 predates existing segments.
        // Phase 3 backfill walks manifest.segments and depends on
        // this ordering for "since N" filtering.
        new_segments.sort_by(|a, b| {
            if a.range.0.is_strictly_newer_than(&b.range.0) {
                std::cmp::Ordering::Greater
            } else if b.range.0.is_strictly_newer_than(&a.range.0) {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        });
        let new_manifest = ChannelLogManifest {
            community_id: self.manifest.community_id,
            channel_id: self.manifest.channel_id,
            segments: new_segments,
        };
        let mut man_bytes = Vec::with_capacity(256);
        man_bytes.push(CHANNEL_LOG_MANIFEST_V1);
        ciborium::into_writer(&new_manifest, &mut man_bytes)
            .map_err(|e| ChannelLogPersistError::CborEncode(e.to_string()))?;
        crate::owner_state_persist::save_atomically(&self.root.join("manifest.cbor"), &man_bytes)
            .map_err(|e| ChannelLogPersistError::Io(e.to_string()))?;

        // Step 4: ALL persistence succeeded — now safe to commit
        // in-memory state. After this point the in-memory and on-disk
        // states are consistent.
        self.manifest = new_manifest;
        self.tail.clear();
        Ok(())
    }

    /// Reload from disk. Reads manifest.cbor + tail.cbor, replays
    /// every sealed segment in manifest order, then loads the tail.
    /// Returns the count of events recovered (sum across segments + tail).
    ///
    /// If `root` doesn't exist, returns a fresh empty log.
    pub fn reload(
        community_id: SpaceId,
        channel_id: ChannelId,
        root: PathBuf,
        config: ChannelLogConfig,
    ) -> Result<(Self, usize), ChannelLogPersistError> {
        let manifest_path = root.join("manifest.cbor");
        if !manifest_path.exists() {
            return Ok((Self::new(community_id, channel_id, root, config), 0));
        }
        let manifest_bytes = std::fs::read(&manifest_path)?;
        let mut manifest: ChannelLogManifest = match manifest_bytes.split_first() {
            Some((&CHANNEL_LOG_MANIFEST_V1, rest)) => ciborium::from_reader(rest)
                .map_err(|e| ChannelLogPersistError::CborDecode(e.to_string()))?,
            Some((v, _)) => {
                return Err(ChannelLogPersistError::CborDecode(format!(
                    "manifest schema version {} not supported (expected {})",
                    v, CHANNEL_LOG_MANIFEST_V1
                )));
            }
            None => {
                return Err(ChannelLogPersistError::CborDecode(
                    "manifest file is empty".into(),
                ));
            }
        };
        if manifest.community_id != community_id {
            return Err(ChannelLogPersistError::Manifest {
                expected: format!("{:?}", community_id),
                got: format!("{:?}", manifest.community_id),
            });
        }
        if manifest.channel_id != channel_id {
            return Err(ChannelLogPersistError::Manifest {
                expected: format!("{:?}", channel_id),
                got: format!("{:?}", manifest.channel_id),
            });
        }
        // Restore the "ascending by range.0" invariant defensively.
        // seal_and_persist sorts before writing, but a corrupted/
        // hand-edited manifest or one written before the late-seal-
        // sort fix could still be unsorted. Phase 3 backfill depends
        // on this ordering for correct "since N" filtering.
        manifest.segments.sort_by(|a, b| {
            if a.range.0.is_strictly_newer_than(&b.range.0) {
                std::cmp::Ordering::Greater
            } else if b.range.0.is_strictly_newer_than(&a.range.0) {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        });
        // Count segment events. Segments themselves are read on demand
        // by the Phase 3 backfill code; reload doesn't materialize
        // them all into memory (could be megabytes per segment).
        let segment_count: usize = manifest.segments.iter().map(|s| s.count as usize).sum();
        let tail_path = root.join("tail.cbor");
        let tail: Vec<SignedChannelEvent> = if tail_path.exists() {
            let bytes = std::fs::read(&tail_path)?;
            match bytes.split_first() {
                Some((&CHANNEL_LOG_TAIL_V1, rest)) => ciborium::from_reader(rest)
                    .map_err(|e| ChannelLogPersistError::CborDecode(e.to_string()))?,
                Some((v, _)) => {
                    return Err(ChannelLogPersistError::CborDecode(format!(
                        "tail schema version {} not supported (expected {})",
                        v, CHANNEL_LOG_TAIL_V1
                    )));
                }
                // Empty tail.cbor is corruption (a normal sealed log
                // writes at least the schema-version byte + an empty
                // CBOR array). Symmetric with the empty-manifest.cbor
                // arm above — both surface as CborDecode rather than
                // silently returning Vec::new() and losing events.
                None => {
                    return Err(ChannelLogPersistError::CborDecode(
                        "tail file is empty".into(),
                    ));
                }
            }
        } else {
            // Distinct from "file exists but is zero bytes": a missing
            // tail.cbor is a legitimate fresh-log state.
            Vec::new()
        };
        let total = segment_count + tail.len();
        let mut log = Self {
            manifest,
            tail,
            config,
            root,
            reaction_index: ReactionIndex::default(),
            device_watermarks: WatermarkVector::new(),
            reconcile_entries: Vec::new(),
            chunk_index: ChunkIndex::new(),
        };
        log.rebuild_reaction_index();
        log.rebuild_device_watermarks();
        log.rebuild_reconcile_index();
        Ok((log, total))
    }

    /// Return the highest locally-persisted event HLC for this channel.
    ///
    /// This is the **backfill watermark** used by ZEB-418 P3a: when a peer
    /// joins or reconnects, it passes `max_hlc()` as the `since` argument
    /// of the history-backfill query so only events newer than the local
    /// watermark are fetched.
    ///
    /// **Why a max over everything:** HLCs are per-author/device
    /// monotonic only; arrival order and seal ranges can interleave, so
    /// the watermark is the max across all segments' upper bounds and
    /// all tail events. Neither "the last tail event" nor "the last
    /// segment's `range.1`" is guaranteed to carry the channel-wide
    /// max: the tail appends in ARRIVAL order (a cross-lane straggler
    /// can land after the newest event), and the manifest sorts by
    /// `range.0` — a late seal from a lagging lane can put a segment
    /// whose `range.1` understates an earlier segment's bound last
    /// (e.g. ranges (100,300),(200,200) sort with 200 last; true max
    /// is 300). Returns `None` for an empty log (fresh joiner requests
    /// full history).
    ///
    /// A stale-LOW watermark is merely wasteful (the replay tracker
    /// dedupes re-served events) but interacts badly with the paging
    /// loop, which reads a non-advancing watermark on a full page as
    /// no-progress and backs off.
    pub fn max_hlc(&self) -> Option<Hlc> {
        // Hlc has no `Ord` impl (the device_id String tuple position
        // would force allocation in any Ord impl), so use
        // is_strictly_newer_than via max_by — same pattern as
        // seal_and_persist's range computation.
        self.manifest
            .segments
            .iter()
            .map(|seg| &seg.range.1)
            .chain(self.tail.iter().map(|e| e.at()))
            .max_by(|a, b| {
                if a.is_strictly_newer_than(b) {
                    std::cmp::Ordering::Greater
                } else if b.is_strictly_newer_than(a) {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .cloned()
    }

    /// The channel log's root directory. Exposed so off-lock readers
    /// (e.g. `ChannelLogEngine::find_attachment`, which snapshots segment
    /// descriptors under the async mutex and then reads files via
    /// `read_segment_at` in `spawn_blocking`) can locate segment files
    /// without holding the lock across `std::fs` I/O.
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// Read all events from a sealed segment. Used by Phase 3 backfill.
    /// Phase 2 ships this for tests (verify seal/reload byte-equality).
    pub fn read_segment(
        &self,
        descriptor: &SegmentDescriptor,
    ) -> Result<Vec<SignedChannelEvent>, ChannelLogPersistError> {
        read_segment_at(&self.root, descriptor)
    }

    /// Materialized reactions for a message (ZEB-536).
    pub fn reactions_for(&self, target: &MessageId, me: &OwnerAddr) -> Vec<ReactionDto> {
        self.reaction_index.reactions_for(target, me)
    }

    /// Rebuild the reaction index from the persisted log. Reads each
    /// sealed segment once (transiently — peak extra memory is one
    /// segment), then folds the in-memory tail. One-time boot cost;
    /// acceptable for v1 (reactions are sparse, segments small). A
    /// persisted/summary index is a future optimization (out of scope).
    ///
    /// Segment read errors are non-fatal (ZEB-536): reactions are a
    /// derived, non-critical view. A missing or corrupt old segment
    /// emits a tracing::warn and is skipped — the channel still loads.
    fn rebuild_reaction_index(&mut self) {
        let mut idx = ReactionIndex::default();
        for seg in &self.manifest.segments {
            match self.read_segment(seg) {
                Ok(events) => {
                    for ev in events {
                        idx.apply(&ev);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        zeb = "ZEB-536",
                        segment = ?seg.handle,
                        error = %e,
                        "rebuild_reaction_index: skipping unreadable segment"
                    );
                }
            }
        }
        for ev in &self.tail {
            idx.apply(ev);
        }
        self.reaction_index = idx;
    }

    /// ZEB-585: snapshot the per-device catch-up watermark.
    pub fn watermark_vector(&self) -> WatermarkVector {
        self.device_watermarks.clone()
    }

    /// Rebuild the per-device watermark index from the persisted log.
    /// Same one-time boot cost + non-fatal segment-read handling as
    /// `rebuild_reaction_index` (an unreadable old segment is skipped with
    /// a warn; the channel still loads — a stale-low watermark only costs
    /// a wasteful re-fetch the periodic floor heals).
    fn rebuild_device_watermarks(&mut self) {
        let mut idx: WatermarkVector = WatermarkVector::new();
        for seg in &self.manifest.segments {
            match self.read_segment(seg) {
                Ok(events) => {
                    for ev in &events {
                        raise_watermark(&mut idx, ev.author(), ev.at());
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        zeb = "ZEB-585",
                        segment = ?seg.handle,
                        error = %e,
                        "rebuild_device_watermarks: skipping unreadable segment"
                    );
                }
            }
        }
        for ev in &self.tail {
            raise_watermark(&mut idx, ev.author(), ev.at());
        }
        self.device_watermarks = idx;
    }

    /// ZEB-592: rebuild the in-memory reconcile index (sorted entries + chunk
    /// index) from the persisted log. Same one-time boot cost + non-fatal
    /// segment-read handling as `rebuild_device_watermarks`.
    fn rebuild_reconcile_index(&mut self) {
        let mut entries: Vec<(ReconcileKey, [u8; 32])> = Vec::new();
        for seg in &self.manifest.segments {
            match self.read_segment(seg) {
                Ok(events) => {
                    for ev in &events {
                        let k = reconcile_key(ev);
                        let h = k.3;
                        entries.push((k, h));
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        zeb = "ZEB-592",
                        segment = ?seg.handle,
                        error = %e,
                        "rebuild_reconcile_index: skipping unreadable segment"
                    );
                }
            }
        }
        for ev in &self.tail {
            let k = reconcile_key(ev);
            let h = k.3;
            entries.push((k, h));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.dedup_by(|a, b| a.0 == b.0);
        self.chunk_index = ChunkIndex::build_from_sorted(&entries);
        self.reconcile_entries = entries;
    }

    /// ZEB-592: fold one event into the reconcile index at the `append` choke
    /// point. `mem::take` lets the chunk index read the (pre-insert) entries
    /// without a self-field borrow split.
    fn maintain_reconcile_index(&mut self, event: &SignedChannelEvent) {
        let key = reconcile_key(event);
        let hash = key.3;
        let pos = self.reconcile_entries.partition_point(|x| x.0 < key);
        if self.reconcile_entries.get(pos).is_some_and(|x| x.0 == key) {
            return; // idempotent re-append
        }
        let mut idx = std::mem::take(&mut self.chunk_index);
        idx.insert(key.clone(), hash, |lo, hi| {
            let s = self.reconcile_entries.partition_point(|x| &x.0 < lo);
            let e = self.reconcile_entries.partition_point(|x| &x.0 <= hi);
            self.reconcile_entries[s..e].to_vec()
        });
        self.chunk_index = idx;
        self.reconcile_entries.insert(pos, (key, hash));
    }

    /// ZEB-592: resolve a set of RBSR `ReconcileKey`s back to their full events
    /// for inline `Have` transfer. Reads only the segments whose `wall_ms` range
    /// overlaps the requested keys' span (plus the tail), so a small leaf range
    /// touches at most a couple of segments.
    // ZEB-593: reaches production via `rbsr_respond` ← the `rbsr/**` queryable.
    pub(crate) fn events_for_keys(
        &self,
        keys: &[ReconcileKey],
    ) -> Result<Vec<SignedChannelEvent>, ChannelLogPersistError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: std::collections::HashSet<ReconcileKey> = keys.iter().cloned().collect();
        // Track DISTINCT resolved keys so a duplicate body (same key seen twice)
        // can't mask another advertised-but-missing key when `rbsr_respond`
        // count-checks `events.len() == have_keys.len()`.
        let mut found: std::collections::HashSet<ReconcileKey> = std::collections::HashSet::new();
        let lo = keys.iter().map(|k| k.0).min().unwrap();
        let hi = keys.iter().map(|k| k.0).max().unwrap();
        let mut out = Vec::new();
        for seg in &self.manifest.segments {
            // Skip segments whose wall_ms span is entirely outside [lo, hi].
            if seg.range.1.wall_ms < lo || seg.range.0.wall_ms > hi {
                continue;
            }
            // Propagate read errors: silently skipping a segment would let the
            // sealed reply advertise `Have` keys whose event bodies are missing,
            // and the requester treats `Have` as resolved — losing those events.
            let events = self.read_segment(seg)?;
            for ev in events {
                let key = reconcile_key(&ev);
                if wanted.contains(&key) && found.insert(key) {
                    out.push(ev);
                }
            }
        }
        for ev in &self.tail {
            let key = reconcile_key(ev);
            if wanted.contains(&key) && found.insert(key) {
                out.push(ev.clone());
            }
        }
        Ok(out)
    }
}

/// ZEB-592: canonical RBSR set-element key for an event —
/// `(wall_ms, logical, device_id, element_hash)`.
fn reconcile_key(e: &SignedChannelEvent) -> ReconcileKey {
    let at = e.at();
    (
        at.wall_ms,
        at.logical,
        at.device_id.clone(),
        event_element_hash(e),
    )
}

impl RangeReconcileSource for ChannelLog {
    fn range_fingerprint(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> RangeFingerprint {
        self.chunk_index
            .range_fingerprint(lo, hi, &mut |first, last| {
                let s = self.reconcile_entries.partition_point(|x| &x.0 < first);
                let e = self.reconcile_entries.partition_point(|x| &x.0 <= last);
                self.reconcile_entries[s..e.max(s)].to_vec()
            })
    }

    fn range_count(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> u64 {
        let s = self.reconcile_entries.partition_point(|x| &x.0 < lo);
        let e = self.reconcile_entries.partition_point(|x| &x.0 < hi);
        e.saturating_sub(s) as u64
    }

    fn keys_in_range(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> Vec<ReconcileKey> {
        let s = self.reconcile_entries.partition_point(|x| &x.0 < lo);
        let e = self.reconcile_entries.partition_point(|x| &x.0 < hi);
        self.reconcile_entries[s..e.max(s)]
            .iter()
            .map(|x| x.0.clone())
            .collect()
    }

    fn split_key(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> Option<ReconcileKey> {
        let s = self.reconcile_entries.partition_point(|x| &x.0 < lo);
        let e = self.reconcile_entries.partition_point(|x| &x.0 < hi);
        if e.saturating_sub(s) < 2 {
            None
        } else {
            Some(self.reconcile_entries[s + (e - s) / 2].0.clone())
        }
    }
}

/// Read all events from a sealed segment given an explicit root dir.
///
/// Factored out of `ChannelLog::read_segment` so callers that have snapshotted
/// the root + descriptors under a lock can read off the async executor (via
/// `tokio::task::spawn_blocking`) WITHOUT holding the lock across the
/// synchronous `std::fs::read`. Pure / sync / no shared state.
pub fn read_segment_at(
    root: &std::path::Path,
    descriptor: &SegmentDescriptor,
) -> Result<Vec<SignedChannelEvent>, ChannelLogPersistError> {
    let SegmentHandle::LocalFile { rel_path } = &descriptor.handle;
    // Validate before joining: rel_path comes from deserialized
    // manifest.cbor, which a Phase 3 backfill peer could ship.
    // Reject absolute paths, parent-directory escapes, and
    // current-directory tricks; require the path to start with
    // the segments/ prefix where seal_and_persist writes them.
    let rel_path_p = std::path::Path::new(rel_path);
    let valid = !rel_path_p.is_absolute()
        && rel_path_p
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        && rel_path_p.starts_with("segments");
    if !valid {
        return Err(ChannelLogPersistError::Io(format!(
            "invalid segment path {:?} (must be a normalized relative path under segments/)",
            rel_path
        )));
    }
    let abs_path = root.join(rel_path_p);
    let bytes = std::fs::read(&abs_path)?;
    match bytes.split_first() {
        Some((&CHANNEL_LOG_SEGMENT_V1, rest)) => ciborium::from_reader(rest)
            .map_err(|e| ChannelLogPersistError::CborDecode(e.to_string())),
        Some((v, _)) => Err(ChannelLogPersistError::CborDecode(format!(
            "segment schema version {} not supported (expected {})",
            v, CHANNEL_LOG_SEGMENT_V1
        ))),
        None => Err(ChannelLogPersistError::CborDecode(
            "segment file is empty".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_mk() -> EpochKey {
        EpochKey::new([0xaa; 32])
    }

    fn fixture_community(id: u8) -> SpaceId {
        SpaceId([id; 16])
    }

    fn fixture_channel(id: u8) -> ChannelId {
        ChannelId([id; 16])
    }

    // ── ZEB-599: backfill_state sidecar ─────────────────────────────
    #[test]
    fn backfill_state_round_trips() {
        let tmp = tempfile::tempdir().expect("tmp");
        ChannelBackfillState::save(tmp.path(), 1_700_000_123_456).expect("save");
        assert_eq!(
            ChannelBackfillState::load(tmp.path()),
            Some(ChannelBackfillState {
                last_full_reconcile_ms: 1_700_000_123_456
            }),
        );
    }

    #[test]
    fn backfill_state_absent_is_none() {
        let tmp = tempfile::tempdir().expect("tmp");
        // No sidecar written → "never reconciled" → None (legacy path).
        assert_eq!(ChannelBackfillState::load(tmp.path()), None);
    }

    #[test]
    fn backfill_state_unknown_version_is_none() {
        let tmp = tempfile::tempdir().expect("tmp");
        // A future/corrupt schema byte must degrade to None, never error.
        let path = tmp.path().join("backfill_state.cbor");
        std::fs::write(&path, [0xFF, 0x01, 0x02, 0x03]).expect("write");
        assert_eq!(ChannelBackfillState::load(tmp.path()), None);
    }

    #[test]
    fn backfill_state_save_overwrites() {
        let tmp = tempfile::tempdir().expect("tmp");
        ChannelBackfillState::save(tmp.path(), 100).expect("save 1");
        ChannelBackfillState::save(tmp.path(), 999).expect("save 2");
        assert_eq!(
            ChannelBackfillState::load(tmp.path()).map(|s| s.last_full_reconcile_ms),
            Some(999),
        );
    }

    #[test]
    fn derive_channel_key_is_deterministic() {
        let mk = fixture_mk();
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let k1 = derive_channel_key(&mk, &cid, &chid);
        let k2 = derive_channel_key(&mk, &cid, &chid);
        assert_eq!(k1.as_bytes(), k2.as_bytes());
    }

    #[test]
    fn derive_presence_key_is_deterministic_and_distinct() {
        let mk = EpochKey::new([0x55; 32]);
        let c = SpaceId([0xc0; 16]);
        let p1 = derive_presence_key(&mk, &c);
        let p2 = derive_presence_key(&mk, &c);
        assert_eq!(p1.as_bytes(), p2.as_bytes(), "deterministic");
        let ch = derive_channel_key(&mk, &c, &ChannelId([0xc1; 16]));
        assert_ne!(p1.as_bytes(), ch.as_bytes(), "presence key != channel key");
        let other = derive_presence_key(&mk, &SpaceId([0xc2; 16]));
        assert_ne!(p1.as_bytes(), other.as_bytes(), "per-community");
    }

    #[test]
    fn derive_channel_key_distinct_by_channel_id() {
        let mk = fixture_mk();
        let cid = fixture_community(0xc0);
        let k_a = derive_channel_key(&mk, &cid, &fixture_channel(0x01));
        let k_b = derive_channel_key(&mk, &cid, &fixture_channel(0x02));
        assert_ne!(
            k_a.as_bytes(),
            k_b.as_bytes(),
            "different channel_id under same community must yield distinct keys"
        );
    }

    #[test]
    fn derive_channel_key_distinct_by_community_id() {
        let mk = fixture_mk();
        let chid = fixture_channel(0x01);
        let k_a = derive_channel_key(&mk, &fixture_community(0xc0), &chid);
        let k_b = derive_channel_key(&mk, &fixture_community(0xc1), &chid);
        assert_ne!(
            k_a.as_bytes(),
            k_b.as_bytes(),
            "same channel_id under different communities must yield distinct keys"
        );
    }

    #[test]
    fn derive_channel_key_distinct_by_membership_key() {
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let k_a = derive_channel_key(&EpochKey::new([0xaa; 32]), &cid, &chid);
        let k_b = derive_channel_key(&EpochKey::new([0xbb; 32]), &cid, &chid);
        assert_ne!(
            k_a.as_bytes(),
            k_b.as_bytes(),
            "different membership keys must yield distinct channel keys"
        );
    }

    #[test]
    fn channel_key_zeroize_on_drop() {
        // Use ZeroizeOnDrop's invariant: dropping the wrapper zeros the
        // underlying [u8; 32]. We can't easily observe the freed memory,
        // but we can verify the trait is implemented by constraining a
        // generic function.
        fn assert_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>() {}
        assert_zeroize_on_drop::<ChannelKey>();
    }

    #[test]
    fn watermark_vector_seal_open_round_trips() {
        let key = derive_channel_key(
            &fixture_mk(),
            &fixture_community(0xc0),
            &fixture_channel(0x01),
        );
        let mut v: WatermarkVector = BTreeMap::new();
        v.insert((OwnerAddr([0xa1; 16]), "dev-a".to_string()), (100, 3));
        v.insert((OwnerAddr([0xa1; 16]), "dev-b".to_string()), (250, 0));
        let sealed = seal_watermark_vector(&key, &v).expect("seal");
        let opened = open_watermark_vector(&key, &sealed).expect("open");
        assert_eq!(opened, v);
    }

    #[test]
    fn watermark_vector_open_rejects_oversize_before_decode() {
        let key = derive_channel_key(
            &fixture_mk(),
            &fixture_community(0xc0),
            &fixture_channel(0x01),
        );
        let too_big = vec![0u8; MAX_WATERMARK_VECTOR_BYTES + 1];
        let err = open_watermark_vector(&key, &too_big).expect_err("must reject oversize");
        assert!(
            matches!(err, ChannelEventError::MalformedPacket(n) if n == MAX_WATERMARK_VECTOR_BYTES + 1),
            "oversize must be rejected pre-decode as MalformedPacket, got {err:?}"
        );
    }

    #[test]
    fn watermark_vector_open_rejects_tampered() {
        let key = derive_channel_key(
            &fixture_mk(),
            &fixture_community(0xc0),
            &fixture_channel(0x01),
        );
        let mut v: WatermarkVector = BTreeMap::new();
        v.insert((OwnerAddr([0xa1; 16]), "dev-a".to_string()), (100, 3));
        let mut sealed = seal_watermark_vector(&key, &v).expect("seal");
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff; // flip a Poly1305 tag byte
        assert!(matches!(
            open_watermark_vector(&key, &sealed),
            Err(ChannelEventError::AeadDecrypt(_))
        ));
    }

    #[test]
    fn watermark_vector_open_rejects_wrong_key() {
        let key = derive_channel_key(
            &fixture_mk(),
            &fixture_community(0xc0),
            &fixture_channel(0x01),
        );
        let other = derive_channel_key(
            &EpochKey::new([0x44; 32]),
            &fixture_community(0xc0),
            &fixture_channel(0x01),
        );
        let mut v: WatermarkVector = BTreeMap::new();
        v.insert((OwnerAddr([0xa1; 16]), "dev-a".to_string()), (100, 3));
        let sealed = seal_watermark_vector(&key, &v).expect("seal");
        assert!(matches!(
            open_watermark_vector(&other, &sealed),
            Err(ChannelEventError::AeadDecrypt(_))
        ));
    }

    #[test]
    fn watermark_vector_wmv_aad_domain_separated_from_packet_aad() {
        // A sealed vector must NOT open as a reply packet and vice-versa —
        // distinct AAD makes the AEAD reject cross-use even under one key.
        assert_ne!(WATERMARK_VECTOR_AAD, CHANNEL_PACKET_AAD);
    }

    #[test]
    fn watermark_vector_tracks_per_device_max_and_survives_reload() {
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let mut log = ChannelLog::new(
            cid,
            chid,
            root.clone(),
            ChannelLogConfig {
                seal_threshold_events: 2,
            },
        );
        // Seal a 2-event segment from dev-a: (100,0) then (150,2).
        log.append(fixture_signed_event(100, 0, "dev-a"))
            .expect("append");
        log.append(fixture_signed_event(150, 2, "dev-a"))
            .expect("append");
        log.seal_and_persist().expect("seal");
        // Tail: a sub-max dev-b event + a newer dev-a event.
        log.append(fixture_signed_event(120, 0, "dev-b"))
            .expect("append");
        log.append(fixture_signed_event(200, 0, "dev-a"))
            .expect("append");

        let v = log.watermark_vector();
        // fixture_signed_event authors every event as fixture_identity(0xa1).
        let (_, author, _) = fixture_identity(0xa1);
        assert_eq!(
            v.get(&(author, "dev-a".to_string())),
            Some(&(200, 0)),
            "dev-a lane max spans segment + tail"
        );
        assert_eq!(v.get(&(author, "dev-b".to_string())), Some(&(120, 0)));
        assert_eq!(v.len(), 2);

        // Reload rebuilds the index identically from segment + tail.
        log.flush_tail().expect("flush");
        let (reloaded, _total) = ChannelLog::reload(
            cid,
            chid,
            root,
            ChannelLogConfig {
                seal_threshold_events: 2,
            },
        )
        .expect("reload");
        assert_eq!(reloaded.watermark_vector(), v);
    }

    fn fixture_owner_addr(byte: u8) -> OwnerAddr {
        OwnerAddr([byte; 16])
    }

    fn fixture_signing_key(seed: u8) -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
    }

    /// Build a (signing_key, owner_addr, identity_pub_64) triple from
    /// a seed where address_hash binds correctly. Use this in any test
    /// that hits `verify_channel_event`'s signature/binding chain
    /// (Phase 2 binding check requires the resolver-returned 64-byte
    /// composite's `address_hash` to equal the event's `OwnerAddr`).
    ///
    /// Replay-tracker-only tests can keep using the simpler
    /// `fixture_owner_addr` + `fixture_signing_key` helpers since
    /// `check_and_advance` doesn't touch the binding chain.
    fn fixture_identity(seed: u8) -> (ed25519_dalek::SigningKey, OwnerAddr, [u8; 64]) {
        let priv_id = harmony_identity::PrivateIdentity::from_seed(&[seed; 32]);
        let owner = OwnerAddr(priv_id.identity.address_hash);
        let pub_64 = priv_id.identity.to_public_bytes();
        // PrivateIdentity::signing_key is private; round-trip through
        // to_private_bytes (X25519_secret(32) || Ed25519_secret(32))
        // to recover a SigningKey we can pass to sign_channel_event.
        // Same pattern as tests/community_open_flow_integration.rs.
        let private_bytes = priv_id.to_private_bytes();
        let mut ed_secret = [0u8; 32];
        ed_secret.copy_from_slice(&private_bytes[32..64]);
        let signing = ed25519_dalek::SigningKey::from_bytes(&ed_secret);
        (signing, owner, pub_64)
    }

    fn fixture_hlc(wall_ms: u64, dev: &str) -> Hlc {
        Hlc {
            wall_ms,
            logical: 0,
            device_id: dev.to_string(),
        }
    }

    fn fixture_payload(
        body: &'static str,
    ) -> (ChannelPostPayload<'static>, ed25519_dalek::SigningKey) {
        let key = fixture_signing_key(0xa1);
        let payload = ChannelPostPayload {
            id: MessageId([0x11; 16]),
            community_id: fixture_community(0xc0),
            channel_id: fixture_channel(0x01),
            author: fixture_owner_addr(0xa1),
            at: fixture_hlc(100_000, "a-dev"),
            content_kind: 0,
            body,
            reply_to: None,
            mentions: None,
            attachments: None,
        };
        (payload, key)
    }

    #[test]
    fn sign_channel_event_round_trip() {
        let (payload, key) = fixture_payload("hello, world!");
        let signed = sign_channel_event(&payload, &key).expect("sign");
        let (
            id,
            community_id,
            channel_id,
            author,
            at,
            content_kind,
            body,
            mentions,
            attachments,
            reply_to,
            sig,
        ) = match signed {
            SignedChannelEvent::Post {
                id,
                community_id,
                channel_id,
                author,
                at,
                content_kind,
                body,
                mentions,
                attachments,
                reply_to,
                sig,
            } => (
                id,
                community_id,
                channel_id,
                author,
                at,
                content_kind,
                body,
                mentions,
                attachments,
                reply_to,
                sig,
            ),
            _ => panic!("expected Post"),
        };
        assert_eq!(id, payload.id);
        assert_eq!(community_id, payload.community_id);
        assert_eq!(channel_id, payload.channel_id);
        assert_eq!(author, payload.author);
        assert_eq!(at, payload.at);
        assert_eq!(content_kind, payload.content_kind);
        assert_eq!(body, payload.body);
        assert_eq!(mentions, payload.mentions);
        assert_eq!(reply_to, payload.reply_to);
        assert_eq!(attachments, payload.attachments);
        assert_eq!(sig.len(), 64);
    }

    #[test]
    fn sign_channel_event_carries_mentions() {
        let key = fixture_signing_key(0xa1);
        let m = vec![fixture_owner_addr(0xb2), fixture_owner_addr(0xc3)];
        let payload = ChannelPostPayload {
            id: MessageId([0x11; 16]),
            community_id: fixture_community(0xc0),
            channel_id: fixture_channel(0x01),
            author: fixture_owner_addr(0xa1),
            at: fixture_hlc(100_000, "a-dev"),
            content_kind: 0,
            body: "ping",
            reply_to: None,
            mentions: Some(m.clone()),
            attachments: None,
        };
        let signed = sign_channel_event(&payload, &key).expect("sign");
        let SignedChannelEvent::Post { mentions, .. } = signed else {
            panic!("expected Post");
        };
        assert_eq!(mentions, Some(m));
    }

    fn fixture_attachment(tag: u8) -> ChannelAttachment {
        ChannelAttachment {
            cid: [tag; 32],
            mime: "text/plain".to_string(),
            name: format!("log-{tag}.txt"),
            size: 1234,
        }
    }

    #[test]
    fn sign_channel_event_carries_attachments() {
        let key = fixture_signing_key(0xa1);
        let atts = vec![fixture_attachment(0xb2), fixture_attachment(0xc3)];
        let payload = ChannelPostPayload {
            id: MessageId([0x11; 16]),
            community_id: fixture_community(0xc0),
            channel_id: fixture_channel(0x01),
            author: fixture_owner_addr(0xa1),
            at: fixture_hlc(100_000, "a-dev"),
            content_kind: 0,
            body: "see log",
            reply_to: None,
            mentions: None,
            attachments: Some(atts.clone()),
        };
        let signed = sign_channel_event(&payload, &key).expect("sign");
        let SignedChannelEvent::Post { attachments, .. } = signed else {
            panic!("expected Post");
        };
        assert_eq!(attachments, Some(atts));
    }

    #[test]
    fn attachments_none_omits_pa_key_some_includes_it() {
        // CBOR text key "pa" encodes as 62 70 61 (text-str len-2 + 'p','a').
        const PA_KEY_HEX: &str = "627061";
        let key = fixture_signing_key(0xa1);

        let (none_payload, _k) = fixture_payload("no attachments");
        let none_event = sign_channel_event(&none_payload, &key).expect("sign");
        let mut none_bytes = Vec::new();
        ciborium::into_writer(&none_event, &mut none_bytes).expect("encode");
        assert!(
            !hex::encode(&none_bytes).contains(PA_KEY_HEX),
            "attachments:None must omit the pa key"
        );

        let some_payload = ChannelPostPayload {
            attachments: Some(vec![fixture_attachment(0xb2)]),
            ..none_payload
        };
        let some_event = sign_channel_event(&some_payload, &key).expect("sign");
        let mut some_bytes = Vec::new();
        ciborium::into_writer(&some_event, &mut some_bytes).expect("encode");
        assert!(
            hex::encode(&some_bytes).contains(PA_KEY_HEX),
            "attachments:Some must include the pa key"
        );
    }

    #[test]
    fn mentions_none_omits_mn_key_some_includes_it() {
        // CBOR text key "mn" encodes as 62 6d 6e (text-string len-2 + 'm','n').
        const MN_KEY_HEX: &str = "626d6e";
        let key = fixture_signing_key(0xa1);

        // mentions: None -> mn key absent (wire-identical to pre-feature).
        let (none_payload, _k) = fixture_payload("no mentions");
        let none_event = sign_channel_event(&none_payload, &key).expect("sign");
        let mut none_bytes = Vec::new();
        ciborium::into_writer(&none_event, &mut none_bytes).expect("encode");
        assert!(
            !hex::encode(&none_bytes).contains(MN_KEY_HEX),
            "mentions:None must omit the mn key"
        );

        // mentions: Some -> mn key present.
        let some_payload = ChannelPostPayload {
            id: MessageId([0x11; 16]),
            community_id: fixture_community(0xc0),
            channel_id: fixture_channel(0x01),
            author: fixture_owner_addr(0xa1),
            at: fixture_hlc(100_000, "a-dev"),
            content_kind: 0,
            body: "x",
            reply_to: None,
            mentions: Some(vec![fixture_owner_addr(0xb2)]),
            attachments: None,
        };
        let some_event = sign_channel_event(&some_payload, &key).expect("sign");
        let mut some_bytes = Vec::new();
        ciborium::into_writer(&some_event, &mut some_bytes).expect("encode");
        assert!(
            hex::encode(&some_bytes).contains(MN_KEY_HEX),
            "mentions:Some must include the mn key"
        );
    }

    #[tokio::test]
    async fn verify_channel_event_accepts_post_with_mentions() {
        // Mirrors verify_channel_event_happy_path but with mentions
        // populated: proves the signature (which now covers mn) verifies
        // end-to-end.
        let state = fixture_state_with_alice_joined();
        let mut tracker = ChannelLogReplayTracker::new();
        let (key, author, _pub64) = fixture_identity(0xa1);
        let payload = ChannelPostPayload {
            id: MessageId([0x11; 16]),
            community_id: fixture_community(0xc0),
            channel_id: fixture_channel(0x01),
            author,
            at: fixture_hlc(100_000, "a-dev"),
            content_kind: 0,
            body: "hi @bob",
            reply_to: None,
            mentions: Some(vec![fixture_owner_addr(0xb2)]),
            attachments: None,
        };
        let event = sign_channel_event(&payload, &key).expect("sign");
        verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &mut tracker,
        )
        .await
        .expect("verify accepts post with mentions");
    }

    #[tokio::test]
    async fn verify_channel_event_rejects_post_over_mention_cap() {
        // ZEB-534: a validly-signed inbound event with > MAX_MENTIONS must
        // be rejected at verify time, so a remote peer can't bypass the cap
        // the local publish path enforces.
        let state = fixture_state_with_alice_joined();
        let mut tracker = ChannelLogReplayTracker::new();
        let (key, author, _pub64) = fixture_identity(0xa1);
        let too_many: Vec<OwnerAddr> = (0..=MAX_MENTIONS)
            .map(|i| fixture_owner_addr(i as u8))
            .collect();
        assert_eq!(too_many.len(), MAX_MENTIONS + 1);
        let payload = ChannelPostPayload {
            id: MessageId([0x11; 16]),
            community_id: fixture_community(0xc0),
            channel_id: fixture_channel(0x01),
            author,
            at: fixture_hlc(100_000, "a-dev"),
            content_kind: 0,
            body: "spam",
            reply_to: None,
            mentions: Some(too_many),
            attachments: None,
        };
        let event = sign_channel_event(&payload, &key).expect("sign");
        let err = verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &mut tracker,
        )
        .await
        .expect_err("over-cap inbound mentions must reject");
        assert!(
            matches!(err, ChannelEventError::TooManyMentions { count, max }
                if count == MAX_MENTIONS + 1 && max == MAX_MENTIONS),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn verify_channel_event_rejects_post_over_attachment_cap() {
        // ZEB-535: a validly-signed inbound event with > MAX_ATTACHMENTS must
        // be rejected at verify time, so a remote peer can't bypass the cap
        // the local publish path enforces.
        let state = fixture_state_with_alice_joined();
        let mut tracker = ChannelLogReplayTracker::new();
        let (key, author, _pub64) = fixture_identity(0xa1);
        let too_many: Vec<ChannelAttachment> = (0..=MAX_ATTACHMENTS)
            .map(|i| fixture_attachment(i as u8))
            .collect();
        assert_eq!(too_many.len(), MAX_ATTACHMENTS + 1);
        let payload = ChannelPostPayload {
            id: MessageId([0x11; 16]),
            community_id: fixture_community(0xc0),
            channel_id: fixture_channel(0x01),
            author,
            at: fixture_hlc(100_000, "a-dev"),
            content_kind: 0,
            body: "x",
            reply_to: None,
            mentions: None,
            attachments: Some(too_many),
        };
        let event = sign_channel_event(&payload, &key).expect("sign");
        let err = verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &mut tracker,
        )
        .await
        .expect_err("over-cap attachments must be rejected");
        assert!(
            matches!(err, ChannelEventError::TooManyAttachments { count, max }
                if count == MAX_ATTACHMENTS + 1 && max == MAX_ATTACHMENTS),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn verify_channel_event_rejects_post_over_attachment_field_len() {
        // ZEB-535: a validly-signed inbound event whose attachment name/mime
        // exceeds MAX_ATTACHMENT_FIELD_BYTES must be rejected at verify time,
        // so a remote peer can't ship unbounded metadata strings.
        let state = fixture_state_with_alice_joined();
        let mut tracker = ChannelLogReplayTracker::new();
        let (key, author, _pub64) = fixture_identity(0xa1);
        let mut oversized = fixture_attachment(0xb2);
        oversized.name = "x".repeat(MAX_ATTACHMENT_FIELD_BYTES + 1);
        let payload = ChannelPostPayload {
            id: MessageId([0x11; 16]),
            community_id: fixture_community(0xc0),
            channel_id: fixture_channel(0x01),
            author,
            at: fixture_hlc(100_000, "a-dev"),
            content_kind: 0,
            body: "x",
            reply_to: None,
            mentions: None,
            attachments: Some(vec![oversized]),
        };
        let event = sign_channel_event(&payload, &key).expect("sign");
        let err = verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &mut tracker,
        )
        .await
        .expect_err("over-length attachment field must be rejected");
        assert!(
            matches!(err, ChannelEventError::AttachmentFieldTooLong { max }
                if max == MAX_ATTACHMENT_FIELD_BYTES),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn verify_channel_event_rejects_post_over_attachment_size() {
        // ZEB-539: a validly-signed inbound event whose attachment `size`
        // exceeds MAX_ATTACHMENT_SIZE must be rejected at verify time — such an
        // attachment could never be downloaded (the download path rejects
        // size > cap), so a peer must not be able to commit one.
        let state = fixture_state_with_alice_joined();
        let mut tracker = ChannelLogReplayTracker::new();
        let (key, author, _pub64) = fixture_identity(0xa1);
        let mut oversized = fixture_attachment(0xb2);
        oversized.size = MAX_ATTACHMENT_SIZE + 1;
        let payload = ChannelPostPayload {
            id: MessageId([0x11; 16]),
            community_id: fixture_community(0xc0),
            channel_id: fixture_channel(0x01),
            author,
            at: fixture_hlc(100_000, "a-dev"),
            content_kind: 0,
            body: "x",
            reply_to: None,
            mentions: None,
            attachments: Some(vec![oversized]),
        };
        let event = sign_channel_event(&payload, &key).expect("sign");
        let err = verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &mut tracker,
        )
        .await
        .expect_err("over-size attachment must be rejected");
        assert!(
            matches!(err, ChannelEventError::AttachmentTooLarge { size, max }
                if size == MAX_ATTACHMENT_SIZE + 1 && max == MAX_ATTACHMENT_SIZE),
            "got: {err:?}"
        );
    }

    #[test]
    fn sign_channel_event_signature_verifies_against_canonical_cbor() {
        use ed25519_dalek::Verifier;
        let (payload, key) = fixture_payload("verify me");
        let signed = sign_channel_event(&payload, &key).expect("sign");
        let canon = signed_set_canonical_cbor(&signed).expect("canon");
        let sig = match &signed {
            SignedChannelEvent::Post { sig, .. } => sig,
            _ => panic!("expected Post"),
        };
        let pubkey = key.verifying_key();
        // Note: in production the author addr would be derived from
        // the identity pubkey via the resolver; here we just verify
        // the signature against the explicit pubkey directly.
        pubkey
            .verify(&canon, &ed25519_dalek::Signature::from_bytes(sig))
            .expect("ed25519 verify");
    }

    #[test]
    fn signed_set_canonical_cbor_is_stable() {
        // Re-encoding the same event must produce byte-identical canonical
        // CBOR (deterministic for replay-protection + signature-stability).
        let (payload, key) = fixture_payload("stable");
        let signed = sign_channel_event(&payload, &key).expect("sign");
        let canon_a = signed_set_canonical_cbor(&signed).expect("canon a");
        let canon_b = signed_set_canonical_cbor(&signed).expect("canon b");
        assert_eq!(canon_a, canon_b);
    }

    #[test]
    fn element_hash_matches_sha256_of_canonical_cbor_and_is_deterministic() {
        use sha2::Digest;
        let (payload, key) = fixture_payload("hello");
        let ev = sign_channel_event(&payload, &key).expect("sign");
        // deterministic: re-hashing the same event yields the same bytes
        assert_eq!(event_element_hash(&ev), event_element_hash(&ev));
        // equals SHA-256 of the canonical signed-set CBOR
        let expect: [u8; 32] =
            sha2::Sha256::digest(signed_set_canonical_cbor(&ev).expect("canon")).into();
        assert_eq!(event_element_hash(&ev), expect);
        // a different event hashes differently
        let (payload2, key2) = fixture_payload("world");
        let ev2 = sign_channel_event(&payload2, &key2).expect("sign");
        assert_ne!(event_element_hash(&ev), event_element_hash(&ev2));
    }

    #[test]
    fn rbsr_seal_round_trips_and_rejects_tamper_wrongkey_oversize() {
        use crate::channel_rbsr::{
            max_key, RbsrMessage, RbsrMode, RbsrRange, RBSR_PROTOCOL_VERSION,
        };
        let mk = fixture_mk();
        let key = derive_channel_key(&mk, &fixture_community(0xc0), &fixture_channel(0x01));
        let other = derive_channel_key(&mk, &fixture_community(0xc1), &fixture_channel(0x01));
        let msg = RbsrMessage {
            version: RBSR_PROTOCOL_VERSION,
            ranges: vec![RbsrRange {
                upper: max_key(),
                mode: RbsrMode::Fingerprint([3u8; 16]),
            }],
        };
        let sealed = seal_rbsr_message(&key, &msg).unwrap();
        assert_eq!(open_rbsr_message(&key, &sealed).unwrap(), msg);
        assert!(
            open_rbsr_message(&other, &sealed).is_err(),
            "wrong key rejected"
        );
        let mut t = sealed.clone();
        *t.last_mut().unwrap() ^= 0x01;
        assert!(open_rbsr_message(&key, &t).is_err(), "tamper rejected");
        let big = vec![0u8; MAX_RBSR_MESSAGE_BYTES + 1];
        assert!(
            matches!(open_rbsr_message(&key, &big), Err(ChannelEventError::MalformedPacket(n)) if n == MAX_RBSR_MESSAGE_BYTES + 1),
            "oversize rejected before decrypt"
        );
    }

    #[test]
    fn seal_rbsr_message_rejects_oversize_before_encrypt() {
        use crate::channel_rbsr::{
            max_key, RbsrMessage, RbsrMode, RbsrRange, RBSR_PROTOCOL_VERSION,
        };
        let mk = fixture_mk();
        let key = derive_channel_key(&mk, &fixture_community(0xc0), &fixture_channel(0x01));
        // A Have list whose CBOR plaintext exceeds the 64 KiB cap — exercises the
        // cap-before-alloc guard on the seal path (only `open` was covered before).
        let keys: Vec<_> = (0..4000u64)
            .map(|i| (i, 0u32, "d".to_string(), [0u8; 32]))
            .collect();
        let msg = RbsrMessage {
            version: RBSR_PROTOCOL_VERSION,
            ranges: vec![RbsrRange {
                upper: max_key(),
                mode: RbsrMode::Have(keys),
            }],
        };
        let err = seal_rbsr_message(&key, &msg).expect_err("oversize seal must be rejected");
        assert!(
            matches!(err, ChannelEventError::MalformedPacket(n) if n > MAX_RBSR_MESSAGE_BYTES),
            "oversize seal must reject as MalformedPacket(> cap) before encrypt, got {err:?}"
        );
    }

    #[test]
    fn rbsr_aad_is_domain_separated_from_wmv() {
        let mk = fixture_mk();
        let key = derive_channel_key(&mk, &fixture_community(0xc0), &fixture_channel(0x01));
        let wmv = WatermarkVector::new();
        let wmv_sealed = seal_watermark_vector(&key, &wmv).unwrap();
        assert!(
            open_rbsr_message(&key, &wmv_sealed).is_err(),
            "a wmv-AAD payload must not open as an RBSR message"
        );
        assert_ne!(RBSR_AAD, WATERMARK_VECTOR_AAD);
    }

    #[test]
    fn rbsr_open_rejects_malformed_partition() {
        use crate::channel_rbsr::{RbsrMessage, RbsrMode, RbsrRange, RBSR_PROTOCOL_VERSION};
        let mk = fixture_mk();
        let key = derive_channel_key(&mk, &fixture_community(0xc0), &fixture_channel(0x01));
        // Structurally valid CBOR, but the ranges do not cover [min_key, max_key)
        // — a peer holding the channel key could seal this; `open` must reject it
        // at the trust boundary rather than feed it to the state machine.
        let bad = RbsrMessage {
            version: RBSR_PROTOCOL_VERSION,
            ranges: vec![RbsrRange {
                upper: (10, 0, "d".into(), [1u8; 32]),
                mode: RbsrMode::Skip,
            }],
        };
        let sealed = seal_rbsr_message(&key, &bad).expect("seal");
        assert!(
            open_rbsr_message(&key, &sealed).is_err(),
            "an invalid partition must be rejected on open"
        );
    }

    #[test]
    fn rbsr_log_source_fingerprint_matches_naive_and_survives_reload() {
        use crate::channel_rbsr::{max_key, min_key, RangeReconcileSource, SliceSource};
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let mut log = ChannelLog::new(
            cid,
            chid,
            root.clone(),
            ChannelLogConfig {
                seal_threshold_events: 3,
            },
        );
        // Across devices, with sub-max stragglers (90 after 150, 130 after 220)
        // landing out of HLC order → exercises the canonical sort + segments.
        let specs = [
            (100u64, 0u32, "dev-a"),
            (150, 0, "dev-a"),
            (120, 0, "dev-b"),
            (200, 0, "dev-a"),
            (90, 0, "dev-c"),
            (210, 0, "dev-b"),
            (220, 0, "dev-a"),
            (130, 0, "dev-c"),
        ];
        let events: Vec<_> = specs
            .iter()
            .map(|&(w, l, d)| fixture_signed_event(w, l, d))
            .collect();
        for e in &events {
            if log.append(e.clone()).expect("append") {
                log.seal_and_persist().expect("seal");
            }
        }
        let all: Vec<_> = events
            .iter()
            .map(|e| {
                let k = reconcile_key(e);
                let h = k.3;
                (k, h)
            })
            .collect();
        let naive = SliceSource::from_unsorted(all);
        let want = naive.range_fingerprint(&min_key(), &max_key()).finalize();

        assert_eq!(
            log.range_fingerprint(&min_key(), &max_key()).finalize(),
            want,
            "log-backed source must match naive over the same events"
        );

        log.flush_tail().expect("flush");
        let (reloaded, _total) = ChannelLog::reload(
            cid,
            chid,
            root,
            ChannelLogConfig {
                seal_threshold_events: 3,
            },
        )
        .expect("reload");
        assert_eq!(
            reloaded
                .range_fingerprint(&min_key(), &max_key())
                .finalize(),
            want,
            "reload rebuilds the reconcile index identically"
        );
    }

    #[test]
    fn rbsr_recovers_within_device_out_of_order_hole_over_real_logs() {
        use crate::channel_rbsr::{
            initial_request, max_key, min_key, process_reply, respond, RangeReconcileSource,
            MAX_RBSR_ROUNDS,
        };
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let mk = || {
            let tmp = tempfile::tempdir().expect("tmp");
            let log = ChannelLog::new(
                cid,
                chid,
                tmp.path().to_path_buf(),
                ChannelLogConfig {
                    seal_threshold_events: 4,
                },
            );
            (log, tmp)
        };
        let (mut a, _ta) = mk();
        let (mut b, _tb) = mk();

        // Common backlog held by BOTH (across seal threshold → real segments).
        let common: Vec<_> = (0..10u64)
            .map(|i| fixture_signed_event(1000 + i * 100, 0, "dev-a"))
            .collect();
        for e in &common {
            if a.append(e.clone()).expect("a append") {
                a.seal_and_persist().expect("seal a");
            }
            if b.append(e.clone()).expect("b append") {
                b.seal_and_persist().expect("seal b");
            }
        }
        // Both hold dev-x's HIGH event (the per-device max).
        let x_high = fixture_signed_event(2500, 0, "dev-x");
        if a.append(x_high.clone()).expect("a") {
            a.seal_and_persist().expect("seal a");
        }
        if b.append(x_high.clone()).expect("b") {
            b.seal_and_persist().expect("seal b");
        }
        // Only A holds dev-x's LOW event — the within-one-device out-of-order
        // hole (B's per-device watermark is 2500, so a scalar/vector catch-up
        // filters this 1500 event out forever; only RBSR's range fingerprint
        // detects the mismatch).
        let x_low = fixture_signed_event(1500, 0, "dev-x");
        if a.append(x_low.clone()).expect("a") {
            a.seal_and_persist().expect("seal a");
        }

        let b_before = b.range_count(&min_key(), &max_key());

        // Drive RBSR rounds: A is the responder (holds the gap), B the
        // requester. Ingest the events B is missing each round.
        let mut request = initial_request(&b);
        let mut transferred = 0usize;
        let mut rounds = 0u32;
        loop {
            rounds += 1;
            assert!(
                rounds <= MAX_RBSR_ROUNDS,
                "must converge within the round cap"
            );
            let reply = respond(&request, &a);
            let (missing, next) = process_reply(&reply, &b);
            let events = a.events_for_keys(&missing).expect("read events for keys");
            transferred += events.len();
            for e in events {
                let _ = b.append(e).expect("ingest");
            }
            match next {
                None => break,
                Some(n) => request = n,
            }
        }

        let b_after = b.range_count(&min_key(), &max_key());
        assert_eq!(
            b_after,
            b_before + 1,
            "B recovered exactly the missing hole event"
        );
        assert!(
            transferred <= 4,
            "O(gap) transfer, not full history: {transferred}"
        );
        assert_eq!(
            a.range_fingerprint(&min_key(), &max_key()).finalize(),
            b.range_fingerprint(&min_key(), &max_key()).finalize(),
            "logs converged after RBSR"
        );
    }

    #[test]
    fn aead_round_trip() {
        let mk = fixture_mk();
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let key = derive_channel_key(&mk, &cid, &chid);
        let (payload, signing_key) = fixture_payload("encrypted hello");
        let event = sign_channel_event(&payload, &signing_key).expect("sign");
        let packet = encrypt_channel_packet(&key, &event).expect("encrypt");
        // Wire packet is at least 12 (nonce) + 16 (Poly1305 tag) + body bytes.
        assert!(
            packet.len() > 12 + 16,
            "packet must include nonce + tag + body"
        );
        let decrypted = decrypt_channel_packet(&key, &packet).expect("decrypt");
        assert_eq!(decrypted, event);
    }

    #[test]
    fn aead_decrypt_rejects_wrong_key() {
        let mk = fixture_mk();
        let key_a = derive_channel_key(&mk, &fixture_community(0xc0), &fixture_channel(0x01));
        let key_b = derive_channel_key(&mk, &fixture_community(0xc0), &fixture_channel(0x02));
        let (payload, signing_key) = fixture_payload("body");
        let event = sign_channel_event(&payload, &signing_key).expect("sign");
        let packet = encrypt_channel_packet(&key_a, &event).expect("encrypt");
        let err = decrypt_channel_packet(&key_b, &packet).expect_err("must fail under wrong key");
        assert!(matches!(err, ChannelEventError::AeadDecrypt(_)));
    }

    #[test]
    fn aead_decrypt_rejects_tampered_ciphertext() {
        let mk = fixture_mk();
        let key = derive_channel_key(&mk, &fixture_community(0xc0), &fixture_channel(0x01));
        let (payload, signing_key) = fixture_payload("body");
        let event = sign_channel_event(&payload, &signing_key).expect("sign");
        let mut packet = encrypt_channel_packet(&key, &event).expect("encrypt");
        // Flip a bit deep in the ciphertext (past the nonce).
        let last = packet.len() - 1;
        packet[last] ^= 0x01;
        let err = decrypt_channel_packet(&key, &packet).expect_err("tampered must fail");
        assert!(matches!(err, ChannelEventError::AeadDecrypt(_)));
    }

    #[test]
    fn aead_decrypt_rejects_short_packet() {
        let mk = fixture_mk();
        let key = derive_channel_key(&mk, &fixture_community(0xc0), &fixture_channel(0x01));
        // Anything shorter than NONCE_LEN + TAG_LEN (28) cannot
        // structurally contain both — reject before invoking AEAD.
        for len in [0usize, 5, 11, 12, 27] {
            let buf = vec![0u8; len];
            let err = decrypt_channel_packet(&key, &buf)
                .expect_err(&format!("len {len} must be rejected as MalformedPacket"));
            assert!(
                matches!(err, ChannelEventError::MalformedPacket(actual) if actual == len),
                "len {len} should produce MalformedPacket({len}), got {err:?}"
            );
        }
    }

    fn fixture_signed_event(at_wall: u64, at_logical: u32, device: &str) -> SignedChannelEvent {
        // Use fixture_identity (PrivateIdentity::from_seed) so the
        // event's author OwnerAddr matches the address_hash the
        // resolver-returned 64-byte identity_pub will derive.
        // verify_channel_event's binding check requires this.
        let (key, author, _pub64) = fixture_identity(0xa1);
        let payload = ChannelPostPayload {
            id: MessageId([0x11; 16]),
            community_id: fixture_community(0xc0),
            channel_id: fixture_channel(0x01),
            author,
            at: Hlc {
                wall_ms: at_wall,
                logical: at_logical,
                device_id: device.to_string(),
            },
            content_kind: 0,
            body: "test",
            reply_to: None,
            mentions: None,
            attachments: None,
        };
        sign_channel_event(&payload, &key).expect("sign")
    }

    #[test]
    fn replay_tracker_accepts_strictly_monotone() {
        let mut t = ChannelLogReplayTracker::new();
        let e1 = fixture_signed_event(100, 0, "a-dev");
        let e2 = fixture_signed_event(200, 0, "a-dev");
        t.check_and_advance(&e1).expect("first event");
        t.check_and_advance(&e2)
            .expect("strictly monotone follow-up");
    }

    #[test]
    fn replay_tracker_accepts_logical_bump_on_same_wall() {
        let mut t = ChannelLogReplayTracker::new();
        let e1 = fixture_signed_event(100, 0, "a-dev");
        let e2 = fixture_signed_event(100, 1, "a-dev");
        t.check_and_advance(&e1).expect("first");
        t.check_and_advance(&e2).expect("logical bump");
    }

    #[test]
    fn replay_tracker_rejects_duplicate() {
        let mut t = ChannelLogReplayTracker::new();
        let e1 = fixture_signed_event(100, 0, "a-dev");
        t.check_and_advance(&e1).expect("first");
        let err = t
            .check_and_advance(&e1)
            .expect_err("identical event must replay-reject");
        assert!(matches!(err, ChannelEventError::Replay { .. }));
    }

    #[test]
    fn replay_tracker_rejects_stale() {
        let mut t = ChannelLogReplayTracker::new();
        let recent = fixture_signed_event(200, 0, "a-dev");
        let stale = fixture_signed_event(100, 0, "a-dev");
        t.check_and_advance(&recent).expect("recent");
        let err = t
            .check_and_advance(&stale)
            .expect_err("stale event must replay-reject");
        assert!(matches!(err, ChannelEventError::Replay { .. }));
    }

    #[test]
    fn replay_tracker_independent_lanes_per_device() {
        let mut t = ChannelLogReplayTracker::new();
        let e_a = fixture_signed_event(200, 0, "a-dev");
        let e_b = fixture_signed_event(100, 0, "b-dev");
        t.check_and_advance(&e_a).expect("a-dev recent");
        t.check_and_advance(&e_b)
            .expect("b-dev's earlier wall time is fine — distinct device lane");
    }

    #[test]
    fn replay_tracker_independent_lanes_per_channel() {
        // Same author + same device, but two different channels.
        // The tracker key includes channel_id, so each channel has
        // its own monotone lane; an earlier-walled event on channel B
        // is fine even after a later-walled event on channel A.
        let mut t = ChannelLogReplayTracker::new();
        let key = fixture_signing_key(0xa1);
        let payload_a = ChannelPostPayload {
            id: MessageId([0x11; 16]),
            community_id: fixture_community(0xc0),
            channel_id: fixture_channel(0x01),
            author: fixture_owner_addr(0xa1),
            at: Hlc {
                wall_ms: 200,
                logical: 0,
                device_id: "a-dev".into(),
            },
            content_kind: 0,
            body: "x",
            reply_to: None,
            mentions: None,
            attachments: None,
        };
        let payload_b = ChannelPostPayload {
            id: MessageId([0x22; 16]),
            community_id: fixture_community(0xc0),
            channel_id: fixture_channel(0x02),
            author: fixture_owner_addr(0xa1),
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "a-dev".into(),
            },
            content_kind: 0,
            body: "y",
            reply_to: None,
            mentions: None,
            attachments: None,
        };
        let event_a = sign_channel_event(&payload_a, &key).expect("sign a");
        let event_b = sign_channel_event(&payload_b, &key).expect("sign b");
        t.check_and_advance(&event_a).expect("ch=01 wall=200");
        t.check_and_advance(&event_b)
            .expect("ch=02 wall=100 must accept — distinct channel lane");
    }

    #[test]
    fn replay_tracker_independent_lanes_per_author() {
        // Same channel + same device, but two different authors.
        // The tracker key includes author, so each author has its
        // own monotone lane within a channel.
        let mut t = ChannelLogReplayTracker::new();
        let key_a = fixture_signing_key(0xa1);
        let key_b = fixture_signing_key(0xb2);
        let payload_a = ChannelPostPayload {
            id: MessageId([0x11; 16]),
            community_id: fixture_community(0xc0),
            channel_id: fixture_channel(0x01),
            author: fixture_owner_addr(0xa1),
            at: Hlc {
                wall_ms: 200,
                logical: 0,
                device_id: "shared-dev".into(),
            },
            content_kind: 0,
            body: "x",
            reply_to: None,
            mentions: None,
            attachments: None,
        };
        let payload_b = ChannelPostPayload {
            id: MessageId([0x22; 16]),
            community_id: fixture_community(0xc0),
            channel_id: fixture_channel(0x01),
            author: fixture_owner_addr(0xb2),
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "shared-dev".into(),
            },
            content_kind: 0,
            body: "y",
            reply_to: None,
            mentions: None,
            attachments: None,
        };
        let event_a = sign_channel_event(&payload_a, &key_a).expect("sign a");
        let event_b = sign_channel_event(&payload_b, &key_b).expect("sign b");
        t.check_and_advance(&event_a)
            .expect("author=alice wall=200");
        t.check_and_advance(&event_b)
            .expect("author=bob wall=100 must accept — distinct author lane");
    }

    // ── verify_channel_event chain tests (spec §7 steps 3-7) ──

    use std::collections::HashMap;

    /// Mock community state. Channels and members are stored as
    /// `(at_hlc, value)` history vectors so the mock can answer
    /// "what was X at HLC Y?" — matching the per-event materialization
    /// behaviour the real engine will provide in Phase 3.
    struct MockState {
        channels: HashMap<ChannelId, Vec<(Hlc, ChannelInfo)>>,
        members: HashMap<OwnerAddr, Vec<(Hlc, u8)>>, // (joined_at, power)
        // For "left" semantics, store the leave HLC per author. None = still joined.
        left_at: HashMap<OwnerAddr, Hlc>,
        // ZEB-399: author's enrolled device verifying keys (ed25519), each
        // paired with the HLC it was enrolled at so `snapshot_at` resolves
        // them AS-OF the requested time — mirroring production, where enrolled
        // keys come from membership materialized at `at` (a key enrolled after
        // `at` must not authorize an earlier post). `verify_channel_event`
        // checks the post sig against the resolved-at-`at` set.
        enrolled: HashMap<OwnerAddr, Vec<(Hlc, [u8; 32])>>,
    }

    #[async_trait::async_trait]
    impl CommunityStateAtHlc for MockState {
        async fn snapshot_at(
            &self,
            channel_id: &ChannelId,
            author: &OwnerAddr,
            at: &Hlc,
        ) -> CommunityStateSnapshot {
            // Channel-config snapshot most recent at `at`. Walk
            // back-to-front via DoubleEndedIterator + find — first
            // hit is the most recent at-or-before `at`. (Avoids
            // `Iterator::last` on a DoubleEndedIterator, per clippy.)
            let channel = self.channels.get(channel_id).and_then(|history| {
                history
                    .iter()
                    .rev()
                    .find(|(hlc, _)| {
                        (hlc.wall_ms, hlc.logical, &hlc.device_id)
                            <= (at.wall_ms, at.logical, &at.device_id)
                    })
                    .map(|(_, info)| info.clone())
            });

            // Most recent power level at-or-before `at`. None if author
            // had Left before `at` or was never Joined.
            let author_power = if let Some(left_hlc) = self.left_at.get(author) {
                if (left_hlc.wall_ms, left_hlc.logical, &left_hlc.device_id)
                    <= (at.wall_ms, at.logical, &at.device_id)
                {
                    None
                } else {
                    self.members.get(author).and_then(|history| {
                        history
                            .iter()
                            .rev()
                            .find(|(hlc, _)| {
                                (hlc.wall_ms, hlc.logical, &hlc.device_id)
                                    <= (at.wall_ms, at.logical, &at.device_id)
                            })
                            .map(|(_, p)| *p)
                    })
                }
            } else {
                self.members.get(author).and_then(|history| {
                    history
                        .iter()
                        .rev()
                        .find(|(hlc, _)| {
                            (hlc.wall_ms, hlc.logical, &hlc.device_id)
                                <= (at.wall_ms, at.logical, &at.device_id)
                        })
                        .map(|(_, p)| *p)
                })
            };

            CommunityStateSnapshot {
                channel,
                author_power,
                author_enrolled_keys: self
                    .enrolled
                    .get(author)
                    .map(|history| {
                        // Resolve enrolled keys as-of `at`: only keys enrolled
                        // at-or-before the event HLC count (matches production's
                        // materialize-at-`at`).
                        history
                            .iter()
                            .filter(|(hlc, _)| {
                                (hlc.wall_ms, hlc.logical, &hlc.device_id)
                                    <= (at.wall_ms, at.logical, &at.device_id)
                            })
                            .map(|(_, key)| *key)
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        }
    }

    /// ZEB-399: the author's enrolled device verifying key is the ed25519
    /// half of the 64-byte identity composite (`fixture_signed_event` signs
    /// with `fixture_identity(seed).0`, whose verifying key == pub64[32..]).
    fn enrolled_key_from_pub64(pub64: &[u8; 64]) -> [u8; 32] {
        let mut k = [0u8; 32];
        k.copy_from_slice(&pub64[32..64]);
        k
    }

    fn fixture_state_with_alice_joined() -> MockState {
        // Use fixture_identity so the author OwnerAddr in the members map
        // matches the event author, and the enrolled key matches the
        // signing key fixture_signed_event uses.
        let (_signing, alice, alice_pub64) = fixture_identity(0xa1);
        let mut channels = HashMap::new();
        let creator_hlc = Hlc {
            wall_ms: 50_000,
            logical: 0,
            device_id: "creator".into(),
        };
        channels.insert(
            fixture_channel(0x01),
            vec![(
                creator_hlc.clone(),
                ChannelInfo {
                    name: "general".into(),
                    write_power: 0,
                    kind: crate::community_membership::ChannelKind::Text,
                    created_at: creator_hlc,
                    deleted_at: None,
                },
            )],
        );
        let mut members = HashMap::new();
        members.insert(alice, vec![(fixture_hlc(60_000, "a-dev"), 100)]);
        let mut enrolled = HashMap::new();
        enrolled.insert(
            alice,
            vec![(
                fixture_hlc(60_000, "a-dev"),
                enrolled_key_from_pub64(&alice_pub64),
            )],
        );
        MockState {
            channels,
            members,
            left_at: HashMap::new(),
            enrolled,
        }
    }

    #[tokio::test]
    async fn verify_channel_event_happy_path() {
        let state = fixture_state_with_alice_joined();
        let mut tracker = ChannelLogReplayTracker::new();
        let event = fixture_signed_event(100_000, 0, "a-dev");
        verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &mut tracker,
        )
        .await
        .expect("happy path verifies");
    }

    #[tokio::test]
    async fn verify_channel_event_accepts_post_signed_by_enrolled_device_key() {
        // ZEB-399 regression anchor. A channel post is signed by the
        // author's enrolled DEVICE key (device #2), which is DISTINCT from
        // the owner identity key. The author field is the OwnerAddr, but
        // the signing key is the device key — exactly the production shape
        // the old owner-identity resolver could NOT validate (it bound the
        // resolved 64-byte identity's address_hash to the author and
        // verified against the owner identity key). Verify must accept
        // because the device key is in the materialized enrolled_device_keys.
        let (_owner_signing, alice, _alice_pub64) = fixture_identity(0xa1);
        // A distinct device key (≠ alice's identity key) — stands in for
        // the enrolled device key #2.
        let device_key = ed25519_dalek::SigningKey::from_bytes(&[0x2d; 32]);
        let device_vk = device_key.verifying_key().to_bytes();

        let mut channels = HashMap::new();
        let creator_hlc = Hlc {
            wall_ms: 50_000,
            logical: 0,
            device_id: "creator".into(),
        };
        channels.insert(
            fixture_channel(0x01),
            vec![(
                creator_hlc.clone(),
                ChannelInfo {
                    name: "general".into(),
                    write_power: 0,
                    kind: crate::community_membership::ChannelKind::Text,
                    created_at: creator_hlc,
                    deleted_at: None,
                },
            )],
        );
        let mut members = HashMap::new();
        members.insert(alice, vec![(fixture_hlc(60_000, "a-dev"), 100)]);
        let mut enrolled = HashMap::new();
        enrolled.insert(alice, vec![(fixture_hlc(60_000, "a-dev"), device_vk)]);
        let state = MockState {
            channels,
            members,
            left_at: HashMap::new(),
            enrolled,
        };

        // Post authored by alice (owner addr) but signed by the device key.
        let payload = ChannelPostPayload {
            id: MessageId([0x22; 16]),
            community_id: fixture_community(0xc0),
            channel_id: fixture_channel(0x01),
            author: alice,
            at: Hlc {
                wall_ms: 100_000,
                logical: 0,
                device_id: "a-dev".into(),
            },
            content_kind: 0,
            body: "hello from device key",
            reply_to: None,
            mentions: None,
            attachments: None,
        };
        let event = sign_channel_event(&payload, &device_key).expect("sign");

        let mut tracker = ChannelLogReplayTracker::new();
        verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &mut tracker,
        )
        .await
        .expect("post signed by enrolled device key must verify");
    }

    #[tokio::test]
    async fn verify_channel_event_rejects_post_signed_by_non_enrolled_key() {
        // ZEB-399: a member's enrolled set is the authority. A post signed
        // by a key NOT in enrolled_device_keys must reject as BadSignature.
        let (_owner_signing, alice, alice_pub64) = fixture_identity(0xa1);
        let mut channels = HashMap::new();
        let creator_hlc = Hlc {
            wall_ms: 50_000,
            logical: 0,
            device_id: "creator".into(),
        };
        channels.insert(
            fixture_channel(0x01),
            vec![(
                creator_hlc.clone(),
                ChannelInfo {
                    name: "general".into(),
                    write_power: 0,
                    kind: crate::community_membership::ChannelKind::Text,
                    created_at: creator_hlc,
                    deleted_at: None,
                },
            )],
        );
        let mut members = HashMap::new();
        members.insert(alice, vec![(fixture_hlc(60_000, "a-dev"), 100)]);
        let mut enrolled = HashMap::new();
        enrolled.insert(
            alice,
            vec![(
                fixture_hlc(60_000, "a-dev"),
                enrolled_key_from_pub64(&alice_pub64),
            )],
        );
        let state = MockState {
            channels,
            members,
            left_at: HashMap::new(),
            enrolled,
        };

        // Sign with an imposter key NOT in alice's enrolled set.
        let imposter = ed25519_dalek::SigningKey::from_bytes(&[0x99; 32]);
        let payload = ChannelPostPayload {
            id: MessageId([0x33; 16]),
            community_id: fixture_community(0xc0),
            channel_id: fixture_channel(0x01),
            author: alice,
            at: Hlc {
                wall_ms: 100_000,
                logical: 0,
                device_id: "a-dev".into(),
            },
            content_kind: 0,
            body: "imposter",
            reply_to: None,
            mentions: None,
            attachments: None,
        };
        let event = sign_channel_event(&payload, &imposter).expect("sign");

        let mut tracker = ChannelLogReplayTracker::new();
        let err = verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &mut tracker,
        )
        .await
        .expect_err("post signed by a non-enrolled key must reject");
        assert!(matches!(err, ChannelEventError::BadSignature));
    }

    #[tokio::test]
    async fn verify_channel_event_rejects_misroute_community() {
        let state = fixture_state_with_alice_joined();
        let mut tracker = ChannelLogReplayTracker::new();
        let event = fixture_signed_event(100_000, 0, "a-dev");
        let err = verify_channel_event(
            &event,
            &fixture_community(0xff),
            &fixture_channel(0x01),
            &state,
            &mut tracker,
        )
        .await
        .expect_err("wrong community must reject");
        assert!(matches!(err, ChannelEventError::Misroute { .. }));
    }

    #[tokio::test]
    async fn verify_channel_event_rejects_misroute_channel() {
        let state = fixture_state_with_alice_joined();
        let mut tracker = ChannelLogReplayTracker::new();
        let event = fixture_signed_event(100_000, 0, "a-dev");
        let err = verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0xff),
            &state,
            &mut tracker,
        )
        .await
        .expect_err("wrong channel must reject");
        assert!(matches!(err, ChannelEventError::Misroute { .. }));
    }

    #[tokio::test]
    async fn verify_channel_event_rejects_unknown_author() {
        // ZEB-399: author is Joined (passes the power gate) but has no
        // materialized enrolled device key — verify can't authenticate
        // the post and surfaces UnknownAuthor.
        let mut state = fixture_state_with_alice_joined();
        state.enrolled.clear();
        let mut tracker = ChannelLogReplayTracker::new();
        let event = fixture_signed_event(100_000, 0, "a-dev");
        let err = verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &mut tracker,
        )
        .await
        .expect_err("author with no enrolled key must reject");
        assert!(matches!(err, ChannelEventError::UnknownAuthor(_)));
    }

    #[tokio::test]
    async fn verify_channel_event_rejects_key_enrolled_after_event_time() {
        // ZEB-399 time-correctness: enrolled keys are resolved AS-OF the
        // event's HLC (production materializes membership at `at`). A device
        // key enrolled AFTER the post's timestamp must NOT authorize that post
        // — otherwise a key added later could retroactively validate earlier
        // history. The author is Joined-at-`at` (power gate passes), but the
        // enrolled set resolved at `at` is empty, so verify surfaces
        // UnknownAuthor.
        let (_owner_signing, alice, _alice_pub64) = fixture_identity(0xa1);
        let device_key = ed25519_dalek::SigningKey::from_bytes(&[0x2d; 32]);
        let device_vk = device_key.verifying_key().to_bytes();

        let mut channels = HashMap::new();
        let creator_hlc = Hlc {
            wall_ms: 50_000,
            logical: 0,
            device_id: "creator".into(),
        };
        channels.insert(
            fixture_channel(0x01),
            vec![(
                creator_hlc.clone(),
                ChannelInfo {
                    name: "general".into(),
                    write_power: 0,
                    kind: crate::community_membership::ChannelKind::Text,
                    created_at: creator_hlc,
                    deleted_at: None,
                },
            )],
        );
        let mut members = HashMap::new();
        members.insert(alice, vec![(fixture_hlc(60_000, "a-dev"), 100)]);
        // Key enrolled at 200_000 — AFTER the post at 100_000.
        let mut enrolled = HashMap::new();
        enrolled.insert(alice, vec![(fixture_hlc(200_000, "a-dev"), device_vk)]);
        let state = MockState {
            channels,
            members,
            left_at: HashMap::new(),
            enrolled,
        };

        let payload = ChannelPostPayload {
            id: MessageId([0x44; 16]),
            community_id: fixture_community(0xc0),
            channel_id: fixture_channel(0x01),
            author: alice,
            at: Hlc {
                wall_ms: 100_000,
                logical: 0,
                device_id: "a-dev".into(),
            },
            content_kind: 0,
            body: "signed by a key enrolled later",
            reply_to: None,
            mentions: None,
            attachments: None,
        };
        let event = sign_channel_event(&payload, &device_key).expect("sign");

        let mut tracker = ChannelLogReplayTracker::new();
        let err = verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &mut tracker,
        )
        .await
        .expect_err("a key enrolled after the post time must not authorize it");
        assert!(
            matches!(err, ChannelEventError::UnknownAuthor(_)),
            "expected UnknownAuthor for a not-yet-enrolled key, got {err:?}"
        );
    }

    #[tokio::test]
    async fn verify_channel_event_rejects_bad_signature() {
        let state = fixture_state_with_alice_joined();
        let mut tracker = ChannelLogReplayTracker::new();
        let mut event = fixture_signed_event(100_000, 0, "a-dev");
        // Flip a byte in the signature.
        if let SignedChannelEvent::Post { sig, .. } = &mut event {
            sig[0] ^= 0xff;
        }
        let err = verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &mut tracker,
        )
        .await
        .expect_err("bad sig must reject");
        assert!(matches!(err, ChannelEventError::BadSignature));
    }

    #[tokio::test]
    async fn verify_channel_event_rejects_replay() {
        let state = fixture_state_with_alice_joined();
        let mut tracker = ChannelLogReplayTracker::new();
        let event = fixture_signed_event(100_000, 0, "a-dev");
        verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &mut tracker,
        )
        .await
        .expect("first verify");
        let err = verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &mut tracker,
        )
        .await
        .expect_err("replay must reject");
        assert!(matches!(err, ChannelEventError::Replay { .. }));
    }

    #[tokio::test]
    async fn verify_channel_event_rejects_below_write_power() {
        // Build a state where the channel requires write_power=50 but
        // alice is power=0 (not promoted).
        let (_signing, alice, alice_pub64) = fixture_identity(0xa1);
        let mut channels = HashMap::new();
        let creator_hlc = Hlc {
            wall_ms: 50_000,
            logical: 0,
            device_id: "creator".into(),
        };
        channels.insert(
            fixture_channel(0x01),
            vec![(
                creator_hlc.clone(),
                ChannelInfo {
                    name: "ops".into(),
                    write_power: 50,
                    kind: crate::community_membership::ChannelKind::Text,
                    created_at: creator_hlc,
                    deleted_at: None,
                },
            )],
        );
        let mut members = HashMap::new();
        members.insert(alice, vec![(fixture_hlc(60_000, "a-dev"), 0)]);
        let mut enrolled = HashMap::new();
        enrolled.insert(
            alice,
            vec![(
                fixture_hlc(60_000, "a-dev"),
                enrolled_key_from_pub64(&alice_pub64),
            )],
        );
        let state = MockState {
            channels,
            members,
            left_at: HashMap::new(),
            enrolled,
        };
        let mut tracker = ChannelLogReplayTracker::new();
        let event = fixture_signed_event(100_000, 0, "a-dev");
        let err = verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &mut tracker,
        )
        .await
        .expect_err("below threshold must reject");
        assert!(matches!(err, ChannelEventError::NotAuthorized(_)));
        // After the failed verify, the replay tracker MUST NOT have
        // advanced — otherwise a future legitimate event on the same
        // lane would be wrongly rejected as a replay. (Regression
        // guard for the advance-before-auth bug.)
        let key = (
            *event.channel_id(),
            *event.author(),
            event.at().device_id.clone(),
        );
        assert!(
            !tracker.last_seen().contains_key(&key),
            "tracker must NOT advance on failed authorization (was advance-before-auth bug)"
        );
    }

    #[tokio::test]
    async fn verify_channel_event_rejects_post_after_delete() {
        let (_signing, alice, alice_pub64) = fixture_identity(0xa1);
        let mut channels = HashMap::new();
        let creator_hlc = Hlc {
            wall_ms: 50_000,
            logical: 0,
            device_id: "creator".into(),
        };
        channels.insert(
            fixture_channel(0x01),
            vec![(
                creator_hlc.clone(),
                ChannelInfo {
                    name: "deleted".into(),
                    write_power: 0,
                    kind: crate::community_membership::ChannelKind::Text,
                    created_at: creator_hlc,
                    // Channel deleted at wall=80_000.
                    deleted_at: Some(Hlc {
                        wall_ms: 80_000,
                        logical: 0,
                        device_id: "mod".into(),
                    }),
                },
            )],
        );
        let mut members = HashMap::new();
        members.insert(alice, vec![(fixture_hlc(60_000, "a-dev"), 100)]);
        let mut enrolled = HashMap::new();
        enrolled.insert(
            alice,
            vec![(
                fixture_hlc(60_000, "a-dev"),
                enrolled_key_from_pub64(&alice_pub64),
            )],
        );
        let state = MockState {
            channels,
            members,
            left_at: HashMap::new(),
            enrolled,
        };
        let mut tracker = ChannelLogReplayTracker::new();
        // Post at wall=100_000 — after delete (80_000).
        let event = fixture_signed_event(100_000, 0, "a-dev");
        let err = verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &mut tracker,
        )
        .await
        .expect_err("post-delete must reject");
        assert!(matches!(err, ChannelEventError::NotAuthorized(_)));
        // After the failed verify, the replay tracker MUST NOT have
        // advanced — otherwise a future legitimate event on the same
        // lane would be wrongly rejected as a replay. (Regression
        // guard for the advance-before-auth bug.)
        let key = (
            *event.channel_id(),
            *event.author(),
            event.at().device_id.clone(),
        );
        assert!(
            !tracker.last_seen().contains_key(&key),
            "tracker must NOT advance on failed authorization (was advance-before-auth bug)"
        );
    }

    #[tokio::test]
    async fn verify_channel_event_chain_returns_earliest_failure() {
        // Construct a request that fails multiple checks at once:
        //   - Step 3  (misroute) — wrong community_id passed to verify
        //   - Step 3b (replay)   — pre-bumped tracker
        // The chain runs cheapest-first; expect Misroute (step 3) to win.
        let state = fixture_state_with_alice_joined();
        let mut tracker = ChannelLogReplayTracker::new();
        let event = fixture_signed_event(100_000, 0, "a-dev");
        // Pre-bump the tracker so step 3b would fail too.
        tracker.check_and_advance(&event).expect("seed");
        let err = verify_channel_event(
            &event,
            &fixture_community(0xff), // wrong — triggers step 3
            &fixture_channel(0x01),
            &state,
            &mut tracker,
        )
        .await
        .expect_err("must reject");
        assert!(
            matches!(err, ChannelEventError::Misroute { .. }),
            "earliest failure (step 3 misroute) must win, got {err:?}"
        );
    }

    #[tokio::test]
    async fn verify_channel_event_chain_replay_check_runs_before_identity_resolve() {
        // would_accept (cheap sync) must run BEFORE the async state
        // materialization (snapshot_at) to honor cheapest-first ordering.
        // A pre-bumped tracker makes step 3b fire first regardless of the
        // downstream membership/signature checks.
        let state = fixture_state_with_alice_joined();
        let mut tracker = ChannelLogReplayTracker::new();
        let event = fixture_signed_event(100_000, 0, "a-dev");
        // Pre-bump the tracker so step 3b would_accept fails.
        tracker.check_and_advance(&event).expect("seed");
        let err = verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &mut tracker,
        )
        .await
        .expect_err("must reject");
        assert!(
            matches!(err, ChannelEventError::Replay { .. }),
            "Replay (cheap sync step 3b) must win over the async snapshot checks; got {err:?}"
        );
    }

    #[test]
    fn channel_log_append_below_threshold_no_seal_signal() {
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let mut log = ChannelLog::new(
            cid,
            chid,
            tmp.path().to_path_buf(),
            ChannelLogConfig {
                seal_threshold_events: 8,
            },
        );
        for i in 0..7 {
            let event = fixture_signed_event(100_000 + i, 0, "a-dev");
            assert!(
                !log.append(event).expect("append"),
                "below threshold must not signal seal"
            );
        }
        assert_eq!(log.tail.len(), 7);
        assert!(log.manifest.segments.is_empty());
    }

    #[test]
    fn channel_log_append_at_threshold_signals_seal() {
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let mut log = ChannelLog::new(
            cid,
            chid,
            tmp.path().to_path_buf(),
            ChannelLogConfig {
                seal_threshold_events: 4,
            },
        );
        for i in 0..3 {
            assert!(!log
                .append(fixture_signed_event(100_000 + i, 0, "a-dev"))
                .expect("append"));
        }
        assert!(
            log.append(fixture_signed_event(103_000, 0, "a-dev"))
                .expect("append"),
            "fourth event must signal seal at threshold=4"
        );
    }

    #[test]
    fn channel_log_seal_and_persist_round_trip() {
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let mut log = ChannelLog::new(
            cid,
            chid,
            root.clone(),
            ChannelLogConfig {
                seal_threshold_events: 4,
            },
        );
        // Fill exactly threshold worth of events.
        let originals: Vec<SignedChannelEvent> = (0..4)
            .map(|i| fixture_signed_event(100_000 + (i as u64) * 1000, 0, "a-dev"))
            .collect();
        for ev in &originals {
            log.append(ev.clone()).expect("append");
        }
        log.seal_and_persist().expect("seal");
        // After seal: tail empty, manifest grew by one, segment file exists.
        assert!(log.tail.is_empty());
        assert_eq!(log.manifest.segments.len(), 1);
        assert!(root.join("segments/00000000.cbor").exists());
        assert!(root.join("manifest.cbor").exists());
        assert!(root.join("tail.cbor").exists());
        // Assert the manifest descriptor fields are correctly populated —
        // Phase 3's backfill walks segments by these range bounds so any
        // regression here would silently break backfill filtering.
        let descriptor = &log.manifest.segments[0];
        assert_eq!(
            descriptor.count, 4,
            "descriptor count must equal seal batch"
        );
        let first_at = originals[0].at();
        let last_at = originals[3].at();
        assert_eq!(
            &descriptor.range.0, first_at,
            "range.0 must equal first event HLC"
        );
        assert_eq!(
            &descriptor.range.1, last_at,
            "range.1 must equal last event HLC"
        );
        // Reload: byte-identical events recovered.
        let (reloaded, total) = ChannelLog::reload(
            cid,
            chid,
            root,
            ChannelLogConfig {
                seal_threshold_events: 4,
            },
        )
        .expect("reload");
        assert_eq!(total, 4);
        assert_eq!(reloaded.manifest.segments.len(), 1);
        assert!(reloaded.tail.is_empty());
        let segment_events = reloaded
            .read_segment(&reloaded.manifest.segments[0])
            .expect("read segment");
        assert_eq!(segment_events, originals);
    }

    #[test]
    fn channel_log_reload_recovers_tail_only() {
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let mut log = ChannelLog::new(
            cid,
            chid,
            root.clone(),
            ChannelLogConfig {
                seal_threshold_events: 8,
            },
        );
        let originals: Vec<SignedChannelEvent> = (0..3)
            .map(|i| fixture_signed_event(100_000 + (i as u64) * 1000, 0, "a-dev"))
            .collect();
        for ev in &originals {
            log.append(ev.clone()).expect("append");
        }
        log.flush_tail().expect("flush");
        let (reloaded, total) = ChannelLog::reload(
            cid,
            chid,
            root,
            ChannelLogConfig {
                seal_threshold_events: 8,
            },
        )
        .expect("reload");
        assert_eq!(total, 3);
        assert!(reloaded.manifest.segments.is_empty());
        assert_eq!(reloaded.tail, originals);
    }

    #[test]
    fn channel_log_reload_fresh_dir_returns_empty() {
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let (log, total) = ChannelLog::reload(
            cid,
            chid,
            tmp.path().to_path_buf(),
            ChannelLogConfig::default(),
        )
        .expect("reload empty dir");
        assert_eq!(total, 0);
        assert!(log.tail.is_empty());
        assert!(log.manifest.segments.is_empty());
    }

    #[test]
    fn channel_log_reload_rejects_wrong_community() {
        let cid = fixture_community(0xc0);
        let other = fixture_community(0xff);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let mut log = ChannelLog::new(cid, chid, root.clone(), ChannelLogConfig::default());
        log.append(fixture_signed_event(100_000, 0, "a-dev"))
            .expect("append");
        log.flush_tail().expect("flush");
        log.seal_and_persist().expect("seal");
        let err = ChannelLog::reload(other, chid, root, ChannelLogConfig::default())
            .expect_err("manifest community mismatch must reject");
        assert!(matches!(err, ChannelLogPersistError::Manifest { .. }));
    }

    #[test]
    fn channel_log_seal_idempotent_on_empty_tail() {
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let mut log = ChannelLog::new(
            cid,
            chid,
            tmp.path().to_path_buf(),
            ChannelLogConfig::default(),
        );
        log.seal_and_persist().expect("seal empty");
        assert!(log.manifest.segments.is_empty());
        log.seal_and_persist().expect("seal empty again");
        assert!(log.manifest.segments.is_empty());
    }

    #[test]
    fn channel_log_seal_range_uses_min_max_not_first_last() {
        // Regression for the Fix 2 bug: SegmentDescriptor.range used
        // tail.first()/last() in append order, but events aren't
        // globally HLC-monotonic across authors/devices (only per-lane).
        // Phase 3 backfill filters segments by range — wrong bounds
        // silently skip segments containing matching events.
        //
        // Construct a tail in non-monotonic append order:
        // [wall=200, wall=100, wall=300]. Old code would record
        // range=(200, 300); correct min/max scan records (100, 300).
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let mut log = ChannelLog::new(
            cid,
            chid,
            root.clone(),
            ChannelLogConfig {
                seal_threshold_events: 16,
            },
        );
        // Use distinct device_ids so the non-monotonic order doesn't
        // violate the per-lane invariant — the producer side is
        // append-only ChannelLog::append (no replay-tracker check),
        // but tests should still construct events that could
        // legitimately arrive in this order from distinct devices.
        log.append(fixture_signed_event(200, 0, "dev-a"))
            .expect("append");
        log.append(fixture_signed_event(100, 0, "dev-b"))
            .expect("append");
        log.append(fixture_signed_event(300, 0, "dev-c"))
            .expect("append");
        log.seal_and_persist().expect("seal");
        let descriptor = &log.manifest.segments[0];
        assert_eq!(descriptor.count, 3);
        assert_eq!(
            descriptor.range.0.wall_ms, 100,
            "range.0 must be true min HLC across the segment, not first()"
        );
        assert_eq!(
            descriptor.range.1.wall_ms, 300,
            "range.1 must be true max HLC across the segment, not last()"
        );
    }

    #[test]
    fn channel_log_multiple_seals_grow_manifest() {
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let mut log = ChannelLog::new(
            cid,
            chid,
            root.clone(),
            ChannelLogConfig {
                seal_threshold_events: 2,
            },
        );
        // Two seals × 2 events each = 4 total.
        for i in 0..4u64 {
            log.append(fixture_signed_event(100_000 + i * 1000, 0, "a-dev"))
                .expect("append");
            if log.tail.len() >= 2 {
                log.seal_and_persist().expect("seal");
            }
        }
        assert_eq!(log.manifest.segments.len(), 2);
        assert!(root.join("segments/00000000.cbor").exists());
        assert!(root.join("segments/00000001.cbor").exists());
        let (reloaded, total) = ChannelLog::reload(
            cid,
            chid,
            root,
            ChannelLogConfig {
                seal_threshold_events: 2,
            },
        )
        .expect("reload");
        assert_eq!(total, 4);
        assert_eq!(reloaded.manifest.segments.len(), 2);
    }

    #[test]
    fn channel_log_manifest_segments_sorted_by_range_start_after_late_seal() {
        // Per-(channel, author, device) HLC monotonicity does NOT
        // imply global HLC monotonicity across all events. A late
        // seal containing events from a new lane can have a range.0
        // that predates existing segments. Phase 3 backfill walks
        // manifest.segments depending on the documented ascending-
        // by-start invariant, so the manifest must be sorted after
        // every seal regardless of seal arrival order.
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let mut log = ChannelLog::new(
            cid,
            chid,
            tmp.path().to_path_buf(),
            ChannelLogConfig {
                seal_threshold_events: 2,
            },
        );
        // First seal: lane "a-dev" at wall=200.
        log.append(fixture_signed_event(200, 0, "a-dev"))
            .expect("append");
        log.append(fixture_signed_event(200, 1, "a-dev"))
            .expect("append");
        log.seal_and_persist().expect("seal 1");
        assert_eq!(log.manifest.segments.len(), 1);
        // Second seal: lane "b-dev" at wall=100 (EARLIER than first
        // seal — legitimate per per-lane monotonicity).
        log.append(fixture_signed_event(100, 0, "b-dev"))
            .expect("append");
        log.append(fixture_signed_event(100, 1, "b-dev"))
            .expect("append");
        log.seal_and_persist().expect("seal 2");
        assert_eq!(log.manifest.segments.len(), 2);
        // Manifest must be sorted ascending by range.0 — the b-dev
        // segment (range.0 wall=100) sorts BEFORE the a-dev segment
        // (range.0 wall=200), even though b-dev was sealed second.
        assert_eq!(
            log.manifest.segments[0].range.0.wall_ms, 100,
            "earliest segment by range.0 must come first"
        );
        assert_eq!(
            log.manifest.segments[1].range.0.wall_ms, 200,
            "later segment by range.0 must come second"
        );
    }

    #[test]
    fn channel_log_read_segment_rejects_path_traversal() {
        // descriptor.handle.rel_path comes from deserialized
        // manifest.cbor, which a Phase 3 backfill peer could ship
        // hostile. read_segment must reject anything that's not a
        // normalized relative path under segments/.
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let log = ChannelLog::new(
            cid,
            chid,
            tmp.path().to_path_buf(),
            ChannelLogConfig::default(),
        );
        // Construct a hostile descriptor with a parent-dir escape.
        let hostile_paths = [
            "../etc/passwd",
            "../../secrets",
            "/etc/passwd",
            "segments/../../../etc/passwd",
            "./segments/00000000.cbor", // leading "./" is also non-Normal
            "other_dir/00000000.cbor",  // doesn't start with segments/
        ];
        for hostile in &hostile_paths {
            let descriptor = SegmentDescriptor {
                range: (
                    Hlc {
                        wall_ms: 100,
                        logical: 0,
                        device_id: "x".into(),
                    },
                    Hlc {
                        wall_ms: 100,
                        logical: 0,
                        device_id: "x".into(),
                    },
                ),
                count: 1,
                handle: SegmentHandle::LocalFile {
                    rel_path: hostile.to_string(),
                },
            };
            let err = log
                .read_segment(&descriptor)
                .expect_err(&format!("hostile rel_path {hostile:?} must reject"));
            assert!(
                matches!(err, ChannelLogPersistError::Io(_)),
                "hostile rel_path {hostile:?} must produce Io error, got {err:?}"
            );
        }
    }

    #[test]
    fn channel_log_append_rejects_event_bound_to_different_community() {
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let mut log = ChannelLog::new(
            cid,
            chid,
            tmp.path().to_path_buf(),
            ChannelLogConfig::default(),
        );
        // Build an event bound to a DIFFERENT community.
        let key = fixture_signing_key(0xa1);
        let payload = ChannelPostPayload {
            id: MessageId([0xff; 16]),
            community_id: fixture_community(0xff), // wrong community!
            channel_id: chid,
            author: fixture_owner_addr(0xa1),
            at: fixture_hlc(100, "a-dev"),
            content_kind: 0,
            body: "wrong community",
            reply_to: None,
            mentions: None,
            attachments: None,
        };
        let foreign_event = sign_channel_event(&payload, &key).expect("sign");
        let err = log
            .append(foreign_event)
            .expect_err("foreign-community event must reject");
        assert!(
            matches!(err, ChannelLogPersistError::Manifest { .. }),
            "expected Manifest error, got {err:?}"
        );
    }

    #[test]
    fn channel_log_append_rejects_event_bound_to_different_channel() {
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let mut log = ChannelLog::new(
            cid,
            chid,
            tmp.path().to_path_buf(),
            ChannelLogConfig::default(),
        );
        // Build an event bound to a DIFFERENT channel in the same community.
        let key = fixture_signing_key(0xa1);
        let payload = ChannelPostPayload {
            id: MessageId([0xff; 16]),
            community_id: cid,
            channel_id: fixture_channel(0xff), // wrong channel!
            author: fixture_owner_addr(0xa1),
            at: fixture_hlc(100, "a-dev"),
            content_kind: 0,
            body: "wrong channel",
            reply_to: None,
            mentions: None,
            attachments: None,
        };
        let foreign_event = sign_channel_event(&payload, &key).expect("sign");
        let err = log
            .append(foreign_event)
            .expect_err("foreign-channel event must reject");
        assert!(
            matches!(err, ChannelLogPersistError::Manifest { .. }),
            "expected Manifest error, got {err:?}"
        );
    }

    #[test]
    fn channel_log_reload_sorts_unsorted_manifest_defensively() {
        // A corrupted, hand-edited, or pre-fix manifest may have
        // segments in seal-arrival order rather than ascending
        // range.0 order. reload must restore the invariant
        // defensively so Phase 3 backfill's "since N" filtering
        // doesn't silently skip segments.
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("segments")).expect("mkdir segments");
        // Hand-craft a manifest with segments in WRONG order
        // (range.0 wall=200 first, range.0 wall=100 second).
        let manifest = ChannelLogManifest {
            community_id: cid,
            channel_id: chid,
            segments: vec![
                SegmentDescriptor {
                    range: (
                        Hlc {
                            wall_ms: 200,
                            logical: 0,
                            device_id: "a-dev".into(),
                        },
                        Hlc {
                            wall_ms: 200,
                            logical: 1,
                            device_id: "a-dev".into(),
                        },
                    ),
                    count: 2,
                    handle: SegmentHandle::LocalFile {
                        rel_path: "segments/00000000.cbor".into(),
                    },
                },
                SegmentDescriptor {
                    range: (
                        Hlc {
                            wall_ms: 100,
                            logical: 0,
                            device_id: "b-dev".into(),
                        },
                        Hlc {
                            wall_ms: 100,
                            logical: 1,
                            device_id: "b-dev".into(),
                        },
                    ),
                    count: 2,
                    handle: SegmentHandle::LocalFile {
                        rel_path: "segments/00000001.cbor".into(),
                    },
                },
            ],
        };
        // Persist with the schema version byte prefix.
        let mut bytes = vec![CHANNEL_LOG_MANIFEST_V1];
        ciborium::into_writer(&manifest, &mut bytes).expect("encode");
        crate::owner_state_persist::save_atomically(&root.join("manifest.cbor"), &bytes)
            .expect("save");
        // Also need a stub tail.cbor for reload (or none — reload
        // tolerates missing tail.cbor by returning empty).
        let (reloaded, _) =
            ChannelLog::reload(cid, chid, root, ChannelLogConfig::default()).expect("reload");
        assert_eq!(reloaded.manifest.segments.len(), 2);
        // After reload's defensive sort, b-dev (wall=100) must come
        // before a-dev (wall=200) regardless of on-disk order.
        assert_eq!(
            reloaded.manifest.segments[0].range.0.wall_ms, 100,
            "reload must sort segments ascending by range.0"
        );
        assert_eq!(
            reloaded.manifest.segments[1].range.0.wall_ms, 200,
            "reload must sort segments ascending by range.0"
        );
    }

    #[test]
    fn dm_voice_key_is_deterministic_and_call_scoped() {
        let dm = crate::owner_state_types::DmContentKey::new([7u8; 32]);
        let call_a = [1u8; 16];
        let call_b = [2u8; 16];
        let k_a1 = derive_dm_voice_key(&dm, &call_a);
        let k_a2 = derive_dm_voice_key(&dm, &call_a);
        let k_b = derive_dm_voice_key(&dm, &call_b);
        assert_eq!(k_a1.as_bytes(), k_a2.as_bytes());
        assert_ne!(k_a1.as_bytes(), k_b.as_bytes());
    }

    #[test]
    fn groupdm_presence_key_is_stable_and_domain_separated() {
        let ck = crate::owner_state_types::DmContentKey::new([0x11; 32]);
        // Stable across calls (no per-call salt): same content_key -> same key.
        let a = derive_groupdm_presence_key(&ck);
        let b = derive_groupdm_presence_key(&ck);
        assert_eq!(
            a.as_bytes(),
            b.as_bytes(),
            "presence key must be call-independent"
        );
        // Domain-separated from the media key for any call_id.
        let media = derive_dm_voice_key(&ck, &[0x22; 16]);
        assert_ne!(
            a.as_bytes(),
            media.as_bytes(),
            "presence key must differ from media key"
        );
        // Different content_key -> different presence key.
        let other =
            derive_groupdm_presence_key(&crate::owner_state_types::DmContentKey::new([0x99; 32]));
        assert_ne!(a.as_bytes(), other.as_bytes());
    }

    // ── ChannelLog::max_hlc tests (ZEB-418 P3a watermark) ──

    #[test]
    fn max_hlc_none_on_empty_log() {
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let log = ChannelLog::new(
            cid,
            chid,
            tmp.path().to_path_buf(),
            ChannelLogConfig::default(),
        );
        assert!(
            log.max_hlc().is_none(),
            "fresh empty log must return None — caller requests full history"
        );
    }

    #[test]
    fn max_hlc_reads_tail_when_present() {
        // Two tail events; max_hlc must return the max (newest) HLC.
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let mut log = ChannelLog::new(
            cid,
            chid,
            tmp.path().to_path_buf(),
            ChannelLogConfig {
                seal_threshold_events: 8,
            },
        );
        let e1 = fixture_signed_event(100_000, 0, "a-dev");
        let e2 = fixture_signed_event(200_000, 0, "a-dev");
        log.append(e1).expect("append e1");
        log.append(e2).expect("append e2");
        let watermark = log.max_hlc().expect("must be Some with 2 tail events");
        assert_eq!(
            watermark.wall_ms, 200_000,
            "max_hlc must return the max tail HLC (wall=200_000)"
        );
    }

    #[test]
    fn max_hlc_reads_last_segment_bound_when_tail_empty() {
        // One sealed segment ending at a known HLC, empty tail.
        // max_hlc must return the segment's upper range bound.
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let mut log = ChannelLog::new(
            cid,
            chid,
            tmp.path().to_path_buf(),
            ChannelLogConfig {
                seal_threshold_events: 4,
            },
        );
        for i in 0..4u64 {
            log.append(fixture_signed_event(100_000 + i * 1_000, 0, "a-dev"))
                .expect("append");
        }
        log.seal_and_persist().expect("seal");
        assert!(log.tail.is_empty(), "tail must be empty after seal");
        // The sealed segment's range.1 is the last event's HLC: wall=103_000.
        let watermark = log.max_hlc().expect("must be Some with one segment");
        assert_eq!(
            watermark.wall_ms, 103_000,
            "max_hlc must return the last segment's range.1 HLC when tail is empty"
        );
    }

    #[test]
    fn max_hlc_prefers_tail_over_segments() {
        // Sealed segment ends at wall=300_000; tail has an event at wall=400_000.
        // max_hlc must return 400_000 — here the tail carries the channel-wide
        // max (max_hlc takes the max over ALL segment bounds and tail events;
        // there is no "tail strictly newer than segments" invariant — HLCs are
        // only per-author/device monotonic).
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let mut log = ChannelLog::new(
            cid,
            chid,
            tmp.path().to_path_buf(),
            ChannelLogConfig {
                seal_threshold_events: 4,
            },
        );
        // Build and seal a segment with max HLC at wall=300_000.
        for i in 0..4u64 {
            log.append(fixture_signed_event(200_000 + i * 33_000, 0, "a-dev"))
                .expect("append");
        }
        log.seal_and_persist().expect("seal segment");
        assert_eq!(log.manifest.segments.len(), 1);
        assert!(log.tail.is_empty());
        // Add a tail event at wall=400_000 — newer than the segment's range.1.
        log.append(fixture_signed_event(400_000, 0, "a-dev"))
            .expect("append tail event");
        let watermark = log.max_hlc().expect("must be Some");
        assert_eq!(
            watermark.wall_ms, 400_000,
            "max_hlc must take the tail event when it carries the channel max"
        );
    }

    #[test]
    fn max_hlc_takes_max_across_overlapping_segments() {
        // Two sealed segments with ranges (100_000, 300_000) and
        // (200_000, 200_000): the manifest sorts ascending by range.0,
        // so the LAST segment's range.1 (200_000) understates the true
        // max (300_000). HLCs are only per-author/device monotonic —
        // seal ranges can interleave like this when a lagging lane
        // seals late. max_hlc must take the max across ALL segments'
        // upper bounds, not the last one's.
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let mut log = ChannelLog::new(
            cid,
            chid,
            tmp.path().to_path_buf(),
            ChannelLogConfig {
                seal_threshold_events: 4,
            },
        );
        // Segment 1: range (100_000, 300_000).
        for wall in [100_000u64, 150_000, 250_000, 300_000] {
            log.append(fixture_signed_event(wall, 0, "a-dev"))
                .expect("append");
        }
        log.seal_and_persist().expect("seal segment 1");
        // Segment 2: four events at wall=200_000 (logical 0..=3) →
        // range (200_000, 200_000), sorted AFTER segment 1 by range.0.
        for logical in 0..4u32 {
            log.append(fixture_signed_event(200_000, logical, "b-dev"))
                .expect("append");
        }
        log.seal_and_persist().expect("seal segment 2");
        assert_eq!(log.manifest.segments.len(), 2);
        assert!(log.tail.is_empty());
        assert_eq!(
            log.manifest
                .segments
                .last()
                .expect("two segments")
                .range
                .1
                .wall_ms,
            200_000,
            "fixture precondition: last-sorted segment's bound understates the max"
        );
        let watermark = log.max_hlc().expect("must be Some with two segments");
        assert_eq!(
            watermark.wall_ms, 300_000,
            "max_hlc must take the max across ALL segments' range.1, \
             not the last-sorted segment's"
        );
    }

    #[test]
    fn max_hlc_takes_max_within_unordered_tail() {
        // The tail appends in ARRIVAL order: a cross-lane straggler can
        // land after the newest event (HLCs are only per-author/device
        // monotonic). Tail = [wall=300_000 ("a-dev"), wall=200_000
        // ("b-dev")] in arrival order — max_hlc must return 300_000,
        // not the last-appended event's 200_000.
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let tmp = tempfile::tempdir().expect("tmp");
        let mut log = ChannelLog::new(
            cid,
            chid,
            tmp.path().to_path_buf(),
            ChannelLogConfig {
                seal_threshold_events: 8,
            },
        );
        log.append(fixture_signed_event(300_000, 0, "a-dev"))
            .expect("append newest first");
        log.append(fixture_signed_event(200_000, 0, "b-dev"))
            .expect("append straggler last");
        let watermark = log.max_hlc().expect("must be Some with 2 tail events");
        assert_eq!(
            watermark.wall_ms, 300_000,
            "max_hlc must take the max across tail events, not the last-appended"
        );
    }

    #[tokio::test]
    async fn verify_react_rejects_tampered_emoji() {
        let state = fixture_state_with_alice_joined();
        let (signing_key, author, _pub64) = fixture_identity(0xa1);
        let community_id = fixture_community(0xc0);
        let channel_id = fixture_channel(0x01);
        let at = Hlc {
            wall_ms: 100_000,
            logical: 0,
            device_id: "a-dev".into(),
        };
        let payload = ChannelReactPayload {
            target: MessageId([7u8; 16]),
            community_id,
            channel_id,
            author,
            at,
            emoji_attachment: None,
            emoji: "👍".to_string(),
            add: true,
        };
        let mut event = sign_channel_react(&payload, &signing_key).expect("sign react");
        if let SignedChannelEvent::React { emoji, .. } = &mut event {
            *emoji = "👎".into();
        }
        let mut tracker = ChannelLogReplayTracker::new();
        let err = verify_channel_event(&event, &community_id, &channel_id, &state, &mut tracker)
            .await
            .expect_err("tampered emoji must fail verify");
        assert!(
            matches!(err, ChannelEventError::BadSignature),
            "expected BadSignature, got {err:?}"
        );
    }

    #[tokio::test]
    async fn verify_react_rejects_non_member() {
        let community_id = fixture_community(0xc0);
        let channel_id = fixture_channel(0x01);
        let (signing_key, author, _pub64) = fixture_identity(0xa1);
        let creator_hlc = Hlc {
            wall_ms: 50_000,
            logical: 0,
            device_id: "creator".into(),
        };
        let mut channels = HashMap::new();
        channels.insert(
            channel_id,
            vec![(
                creator_hlc.clone(),
                ChannelInfo {
                    name: "general".into(),
                    write_power: 0,
                    kind: crate::community_membership::ChannelKind::Text,
                    created_at: creator_hlc,
                    deleted_at: None,
                },
            )],
        );
        let state = MockState {
            channels,
            members: HashMap::new(),
            left_at: HashMap::new(),
            enrolled: HashMap::new(),
        };
        let at = Hlc {
            wall_ms: 100_000,
            logical: 0,
            device_id: "a-dev".into(),
        };
        let payload = ChannelReactPayload {
            target: MessageId([7u8; 16]),
            community_id,
            channel_id,
            author,
            at,
            emoji_attachment: None,
            emoji: "👍".to_string(),
            add: true,
        };
        let event = sign_channel_react(&payload, &signing_key).expect("sign react");
        let mut tracker = ChannelLogReplayTracker::new();
        let err = verify_channel_event(&event, &community_id, &channel_id, &state, &mut tracker)
            .await
            .expect_err("non-member react must fail");
        assert!(
            matches!(err, ChannelEventError::NotAuthorized(_)),
            "expected NotAuthorized, got {err:?}"
        );
    }

    #[tokio::test]
    async fn verify_react_rejects_oversized_emoji() {
        let state = fixture_state_with_alice_joined();
        let (signing_key, author, _pub64) = fixture_identity(0xa1);
        let community_id = fixture_community(0xc0);
        let channel_id = fixture_channel(0x01);
        let at = Hlc {
            wall_ms: 100_000,
            logical: 0,
            device_id: "a-dev".into(),
        };
        let oversized = "x".repeat(MAX_REACTION_EMOJI_BYTES + 1);
        let payload = ChannelReactPayload {
            target: MessageId([7u8; 16]),
            community_id,
            channel_id,
            author,
            at,
            emoji_attachment: None,
            emoji: oversized,
            add: true,
        };
        let event = sign_channel_react(&payload, &signing_key).expect("sign react");
        let mut tracker = ChannelLogReplayTracker::new();
        let err = verify_channel_event(&event, &community_id, &channel_id, &state, &mut tracker)
            .await
            .expect_err("oversized emoji must fail verify");
        assert!(
            matches!(
                err,
                ChannelEventError::EmojiTooLarge { len, max }
                    if len == MAX_REACTION_EMOJI_BYTES + 1 && max == MAX_REACTION_EMOJI_BYTES
            ),
            "expected EmojiTooLarge {{ len, max }} for oversized emoji, got {err:?}"
        );
    }

    #[tokio::test]
    async fn verify_react_accepts_unknown_target() {
        let state = fixture_state_with_alice_joined();
        let (signing_key, author, _pub64) = fixture_identity(0xa1);
        let community_id = fixture_community(0xc0);
        let channel_id = fixture_channel(0x01);
        let at = Hlc {
            wall_ms: 100_000,
            logical: 0,
            device_id: "a-dev".into(),
        };
        let payload = ChannelReactPayload {
            target: MessageId([0xde; 16]),
            community_id,
            channel_id,
            author,
            at,
            emoji_attachment: None,
            emoji: "✅".to_string(),
            add: true,
        };
        let event = sign_channel_react(&payload, &signing_key).expect("sign react");
        let mut tracker = ChannelLogReplayTracker::new();
        verify_channel_event(&event, &community_id, &channel_id, &state, &mut tracker)
            .await
            .expect("react with unknown target must pass verify (orphan tolerance)");
    }

    #[tokio::test]
    async fn sign_and_verify_react_round_trips() {
        let state = fixture_state_with_alice_joined();
        let (signing_key, author, _pub64) = fixture_identity(0xa1);
        let community_id = fixture_community(0xc0);
        let channel_id = fixture_channel(0x01);
        let at = Hlc {
            wall_ms: 100_000,
            logical: 0,
            device_id: "a-dev".into(),
        };
        let payload = ChannelReactPayload {
            target: MessageId([7u8; 16]),
            community_id,
            channel_id,
            author,
            at: at.clone(),
            emoji_attachment: None,
            emoji: "👍".to_string(),
            add: true,
        };
        let event = sign_channel_react(&payload, &signing_key).expect("sign react");
        let key = derive_channel_key(&fixture_mk(), &community_id, &channel_id);
        let packet = encrypt_channel_packet(&key, &event).expect("encrypt");
        let decoded = decrypt_channel_packet(&key, &packet).expect("decrypt");
        assert_eq!(decoded, event);
        let mut tracker = ChannelLogReplayTracker::new();
        verify_channel_event(&event, &community_id, &channel_id, &state, &mut tracker)
            .await
            .expect("verify react");
    }

    // ── ZEB-541: custom-emoji React verify caps ─────────────────────────────

    /// Helper: build a custom-emoji React payload with a given emoji descriptor.
    fn custom_emoji_react_payload(
        community_id: SpaceId,
        channel_id: ChannelId,
        author: OwnerAddr,
        emoji_attachment: Option<ChannelAttachment>,
    ) -> ChannelReactPayload {
        ChannelReactPayload {
            target: MessageId([7u8; 16]),
            community_id,
            channel_id,
            author,
            at: Hlc {
                wall_ms: 100_000,
                logical: 0,
                device_id: "a-dev".into(),
            },
            emoji_attachment,
            // customs carry an empty unicode grouping key
            emoji: String::new(),
            add: true,
        }
    }

    #[tokio::test]
    async fn verify_react_rejects_nul_in_unicode_emoji() {
        let state = fixture_state_with_alice_joined();
        let (signing_key, author, _pub64) = fixture_identity(0xa1);
        let community_id = fixture_community(0xc0);
        let channel_id = fixture_channel(0x01);
        // A unicode react (no attachment) whose emoji embeds the NUL sentinel
        // must be rejected so it can never land in the custom-emoji key-space.
        let mut payload = custom_emoji_react_payload(community_id, channel_id, author, None);
        payload.emoji = "\u{0}cid:deadbeef".to_string();
        let event = sign_channel_react(&payload, &signing_key).expect("sign react");
        let mut tracker = ChannelLogReplayTracker::new();
        let err = verify_channel_event(&event, &community_id, &channel_id, &state, &mut tracker)
            .await
            .expect_err("NUL in emoji must fail verify");
        assert!(
            matches!(err, ChannelEventError::EmojiContainsNul),
            "expected EmojiContainsNul, got {err:?}"
        );
    }

    #[tokio::test]
    async fn verify_react_rejects_oversized_custom_emoji() {
        let state = fixture_state_with_alice_joined();
        let (signing_key, author, _pub64) = fixture_identity(0xa1);
        let community_id = fixture_community(0xc0);
        let channel_id = fixture_channel(0x01);
        let att = ChannelAttachment {
            cid: [0xB2; 32],
            mime: "image/png".to_string(),
            name: String::new(),
            size: crate::MAX_CUSTOM_EMOJI_BYTES + 1,
        };
        let payload = custom_emoji_react_payload(community_id, channel_id, author, Some(att));
        let event = sign_channel_react(&payload, &signing_key).expect("sign react");
        let mut tracker = ChannelLogReplayTracker::new();
        let err = verify_channel_event(&event, &community_id, &channel_id, &state, &mut tracker)
            .await
            .expect_err("oversized custom emoji must fail verify");
        assert!(
            matches!(
                err,
                ChannelEventError::CustomEmojiTooLarge { size, max }
                    if size == crate::MAX_CUSTOM_EMOJI_BYTES + 1 && max == crate::MAX_CUSTOM_EMOJI_BYTES
            ),
            "expected CustomEmojiTooLarge, got {err:?}"
        );
    }

    #[tokio::test]
    async fn verify_react_rejects_non_image_custom_emoji() {
        let state = fixture_state_with_alice_joined();
        let (signing_key, author, _pub64) = fixture_identity(0xa1);
        let community_id = fixture_community(0xc0);
        let channel_id = fixture_channel(0x01);
        let att = ChannelAttachment {
            cid: [0xB2; 32],
            mime: "application/zip".to_string(),
            name: String::new(),
            size: 1024,
        };
        let payload = custom_emoji_react_payload(community_id, channel_id, author, Some(att));
        let event = sign_channel_react(&payload, &signing_key).expect("sign react");
        let mut tracker = ChannelLogReplayTracker::new();
        let err = verify_channel_event(&event, &community_id, &channel_id, &state, &mut tracker)
            .await
            .expect_err("non-image custom emoji must fail verify");
        assert!(
            matches!(
                err,
                ChannelEventError::CustomEmojiNotImage { ref mime } if mime == "application/zip"
            ),
            "expected CustomEmojiNotImage, got {err:?}"
        );
    }

    #[tokio::test]
    async fn verify_react_rejects_overlong_custom_emoji_field() {
        let state = fixture_state_with_alice_joined();
        let (signing_key, author, _pub64) = fixture_identity(0xa1);
        let community_id = fixture_community(0xc0);
        let channel_id = fixture_channel(0x01);
        // An over-long mime that still starts with "image/": the field-length
        // cap must fire BEFORE the image-mime check, parity with the Post path.
        let att = ChannelAttachment {
            cid: [0xB2; 32],
            mime: format!("image/{}", "x".repeat(MAX_ATTACHMENT_FIELD_BYTES)),
            name: String::new(),
            size: 1024,
        };
        let payload = custom_emoji_react_payload(community_id, channel_id, author, Some(att));
        let event = sign_channel_react(&payload, &signing_key).expect("sign react");
        let mut tracker = ChannelLogReplayTracker::new();
        let err = verify_channel_event(&event, &community_id, &channel_id, &state, &mut tracker)
            .await
            .expect_err("over-long custom emoji field must fail verify");
        assert!(
            matches!(
                err,
                ChannelEventError::AttachmentFieldTooLong { max } if max == MAX_ATTACHMENT_FIELD_BYTES
            ),
            "expected AttachmentFieldTooLong, got {err:?}"
        );
    }

    #[tokio::test]
    async fn verify_react_accepts_valid_custom_emoji() {
        let state = fixture_state_with_alice_joined();
        let (signing_key, author, _pub64) = fixture_identity(0xa1);
        let community_id = fixture_community(0xc0);
        let channel_id = fixture_channel(0x01);
        // `[0xB2; 32]` decodes to mode nibble 0xB (encrypted bit set) — an
        // ENCRYPTED custom emoji. Both encrypted and public custom emoji verify;
        // this case keeps the encrypted path covered.
        let att = ChannelAttachment {
            cid: [0xB2; 32],
            mime: "image/png".to_string(),
            name: String::new(),
            size: 1024,
        };
        let payload = custom_emoji_react_payload(community_id, channel_id, author, Some(att));
        let event = sign_channel_react(&payload, &signing_key).expect("sign react");
        let mut tracker = ChannelLogReplayTracker::new();
        verify_channel_event(&event, &community_id, &channel_id, &state, &mut tracker)
            .await
            .expect("a valid custom-emoji react must verify");
    }

    #[tokio::test]
    async fn verify_react_rejects_custom_emoji_with_unicode() {
        // CodeRabbit PR #320: a custom react carrying BOTH a CAS descriptor and
        // a unicode emoji is an ambiguous-key protocol violation — verify must
        // reject it so a hostile peer can't inject one.
        let state = fixture_state_with_alice_joined();
        let (signing_key, author, _pub64) = fixture_identity(0xa1);
        let community_id = fixture_community(0xc0);
        let channel_id = fixture_channel(0x01);
        let att = ChannelAttachment {
            cid: [0xB2; 32],
            mime: "image/png".to_string(),
            name: String::new(),
            size: 1024,
        };
        let mut payload = custom_emoji_react_payload(community_id, channel_id, author, Some(att));
        payload.emoji = "\u{1F44D}".to_string(); // also carry a unicode 👍
        let event = sign_channel_react(&payload, &signing_key).expect("sign react");
        let mut tracker = ChannelLogReplayTracker::new();
        let err = verify_channel_event(&event, &community_id, &channel_id, &state, &mut tracker)
            .await
            .expect_err("custom emoji + unicode must fail verify");
        assert!(
            matches!(err, ChannelEventError::CustomEmojiWithUnicode),
            "expected CustomEmojiWithUnicode, got {err:?}"
        );
    }

    #[tokio::test]
    async fn verify_react_accepts_public_custom_emoji_cid() {
        // Public custom emoji (foundation): `[0x42; 32]` decodes to mode nibble
        // 0x4 (encrypted bit CLEAR) → a PUBLIC CID. Custom emoji default to
        // public (deduplicated, freely served), so verify must ACCEPT it; the
        // descriptor passes the field/size/image checks.
        let state = fixture_state_with_alice_joined();
        let (signing_key, author, _pub64) = fixture_identity(0xa1);
        let community_id = fixture_community(0xc0);
        let channel_id = fixture_channel(0x01);
        let att = ChannelAttachment {
            cid: [0x42; 32],
            mime: "image/png".to_string(),
            name: String::new(),
            size: 1024,
        };
        let payload = custom_emoji_react_payload(community_id, channel_id, author, Some(att));
        let event = sign_channel_react(&payload, &signing_key).expect("sign react");
        let mut tracker = ChannelLogReplayTracker::new();
        verify_channel_event(&event, &community_id, &channel_id, &state, &mut tracker)
            .await
            .expect("a public custom-emoji react must verify");
    }

    #[tokio::test]
    async fn verify_react_rejects_tampered_custom_emoji_cid() {
        // The signature covers `emoji_attachment` (it's in the signed set), so
        // rebinding a reaction to a different emoji CID after signing must
        // invalidate `sg`.
        let state = fixture_state_with_alice_joined();
        let (signing_key, author, _pub64) = fixture_identity(0xa1);
        let community_id = fixture_community(0xc0);
        let channel_id = fixture_channel(0x01);
        let att = ChannelAttachment {
            cid: [0xB2; 32],
            mime: "image/png".to_string(),
            name: String::new(),
            size: 1024,
        };
        let payload = custom_emoji_react_payload(community_id, channel_id, author, Some(att));
        let mut event = sign_channel_react(&payload, &signing_key).expect("sign react");
        if let SignedChannelEvent::React {
            emoji_attachment: Some(att),
            ..
        } = &mut event
        {
            // flip the CID after signing — the descriptor is still a valid
            // image under cap, so only the signature check can catch this.
            att.cid = [0xC3; 32];
        } else {
            panic!("expected a custom-emoji React");
        }
        let mut tracker = ChannelLogReplayTracker::new();
        let err = verify_channel_event(&event, &community_id, &channel_id, &state, &mut tracker)
            .await
            .expect_err("tampered emoji CID must fail verify");
        assert!(
            matches!(err, ChannelEventError::BadSignature),
            "expected BadSignature, got {err:?}"
        );
    }

    // ── ReactionIndex + ReactionDto tests (Task 2) ──────────────────────────

    /// Build an unsigned React event directly — apply() only reads fields,
    /// no signing needed.
    fn react_event(
        target: MessageId,
        author: OwnerAddr,
        emoji: &str,
        add: bool,
        wall: u64,
    ) -> SignedChannelEvent {
        SignedChannelEvent::React {
            add,
            author,
            target,
            emoji_attachment: None,
            emoji: emoji.to_string(),
            community_id: SpaceId([0u8; 16]),
            channel_id: ChannelId([0u8; 16]),
            at: Hlc {
                wall_ms: wall,
                logical: 0,
                device_id: format!("dev-{}", hex::encode(author.0)),
            },
            sig: [0u8; 64],
        }
    }

    #[test]
    fn reaction_index_lww_toggle_and_counts() {
        let target = MessageId([1u8; 16]);
        let a = OwnerAddr([0xAA; 16]);
        let b = OwnerAddr([0xBB; 16]);
        let mut idx = ReactionIndex::default();
        let mk = |author, emoji: &str, add, wall| react_event(target, author, emoji, add, wall);
        idx.apply(&mk(a, "👍", true, 10));
        idx.apply(&mk(b, "👍", true, 11));
        idx.apply(&mk(a, "🎉", true, 12));
        // out-of-order + LWW: a's older un-react (wall 9) must NOT override the wall-10 react
        idx.apply(&mk(a, "👍", false, 9));
        let r = idx.reactions_for(&target, &a);
        // 👍 -> {a,b} present; 🎉 -> {a}
        let thumbs = r.iter().find(|d| d.emoji == "👍").unwrap();
        assert_eq!(thumbs.count, 2);
        assert!(thumbs.mine);
        assert_eq!(thumbs.reactors.len(), 2);
        // now a un-reacts 👍 with a NEWER hlc → count drops to 1, mine=false
        idx.apply(&mk(a, "👍", false, 20));
        let r2 = idx.reactions_for(&target, &a);
        let thumbs2 = r2.iter().find(|d| d.emoji == "👍").unwrap();
        assert_eq!(thumbs2.count, 1);
        assert!(!thumbs2.mine);
    }

    #[test]
    fn reaction_index_apply_is_idempotent() {
        let target = MessageId([2u8; 16]);
        let a = OwnerAddr([0xAA; 16]);
        let mut idx = ReactionIndex::default();
        let ev = react_event(target, a, "👍", true, 10);
        // Apply the same event twice — only one reactor should appear
        idx.apply(&ev);
        idx.apply(&ev);
        let r = idx.reactions_for(&target, &a);
        let thumbs = r.iter().find(|d| d.emoji == "👍").unwrap();
        assert_eq!(thumbs.count, 1, "idempotent apply must not double-count");
        assert!(thumbs.mine);
        assert_eq!(thumbs.reactors.len(), 1);
    }

    #[test]
    fn reaction_index_empty_for_unknown_target() {
        let idx = ReactionIndex::default();
        let unknown = MessageId([0xFF; 16]);
        let me = OwnerAddr([0xAA; 16]);
        let r = idx.reactions_for(&unknown, &me);
        assert!(r.is_empty(), "unknown target must return empty vec");
    }

    #[test]
    fn reaction_index_ignores_non_react_events() {
        let mut idx = ReactionIndex::default();
        // Build a Post event and apply it — should be silently ignored
        let key = fixture_signing_key(0xa1);
        let (payload, _) = fixture_payload("hello");
        let post_event = sign_channel_event(&payload, &key).expect("sign");
        idx.apply(&post_event);
        // The target message id is payload.id
        let me = OwnerAddr([0xAA; 16]);
        let r = idx.reactions_for(&payload.id, &me);
        assert!(r.is_empty(), "Post events must be ignored by ReactionIndex");
    }

    // ── ZEB-541: custom-emoji materialization (Task 2) ──────────────────────

    /// Build an unsigned custom-emoji React event directly — `apply()` only
    /// reads fields, no signing needed. The grouping key derives from `cid`.
    fn custom_react_event(
        target: MessageId,
        author: OwnerAddr,
        cid: [u8; 32],
        size: u64,
        add: bool,
        wall: u64,
    ) -> SignedChannelEvent {
        SignedChannelEvent::React {
            add,
            author,
            target,
            emoji_attachment: Some(ChannelAttachment {
                cid,
                mime: "image/png".to_string(),
                name: String::new(),
                size,
            }),
            // customs carry an empty unicode grouping key
            emoji: String::new(),
            community_id: SpaceId([0u8; 16]),
            channel_id: ChannelId([0u8; 16]),
            at: Hlc {
                wall_ms: wall,
                logical: 0,
                device_id: format!("dev-{}", hex::encode(author.0)),
            },
            sig: [0u8; 64],
        }
    }

    #[test]
    fn reaction_index_groups_same_custom_emoji_by_cid() {
        // Two different reactors with the SAME custom emoji (same cid) → ONE
        // DTO, count==2, with emoji_cid/emoji_size set and empty unicode emoji.
        let target = MessageId([1u8; 16]);
        let a = OwnerAddr([0xAA; 16]);
        let b = OwnerAddr([0xBB; 16]);
        let cid = [0xB2; 32];
        let size = 1024u64;
        let mut idx = ReactionIndex::default();
        idx.apply(&custom_react_event(target, a, cid, size, true, 10));
        idx.apply(&custom_react_event(target, b, cid, size, true, 11));
        let r = idx.reactions_for(&target, &a);
        assert_eq!(r.len(), 1, "same cid must group into a single DTO");
        let d = &r[0];
        assert_eq!(d.count, 2);
        assert!(d.mine);
        assert_eq!(d.reactors.len(), 2);
        assert_eq!(d.emoji, "", "custom emoji uses an empty unicode field");
        assert_eq!(d.emoji_cid.as_deref(), Some(hex::encode(cid).as_str()));
        assert_eq!(d.emoji_size, Some(size));
    }

    #[test]
    fn reaction_index_unicode_and_custom_are_distinct() {
        // A unicode reaction and a custom reaction on the same message →
        // two distinct DTOs (one emoji_cid None, one Some).
        let target = MessageId([2u8; 16]);
        let a = OwnerAddr([0xAA; 16]);
        let cid = [0xC3; 32];
        let mut idx = ReactionIndex::default();
        idx.apply(&react_event(target, a, "👍", true, 10));
        idx.apply(&custom_react_event(target, a, cid, 512, true, 11));
        let r = idx.reactions_for(&target, &a);
        assert_eq!(r.len(), 2, "unicode and custom must not collide");
        let unicode = r
            .iter()
            .find(|d| d.emoji_cid.is_none())
            .expect("a unicode DTO");
        assert_eq!(unicode.emoji, "👍");
        assert_eq!(unicode.emoji_size, None);
        let custom = r
            .iter()
            .find(|d| d.emoji_cid.is_some())
            .expect("a custom DTO");
        assert_eq!(custom.emoji, "");
        assert_eq!(custom.emoji_cid.as_deref(), Some(hex::encode(cid).as_str()));
        assert_eq!(custom.emoji_size, Some(512));
    }

    #[test]
    fn reaction_index_remove_custom_emoji_drops_chip() {
        // Removing a custom reaction (add=false) with a newer HLC removes it;
        // the chip disappears when the present count hits 0.
        let target = MessageId([3u8; 16]);
        let a = OwnerAddr([0xAA; 16]);
        let cid = [0xD4; 32];
        let size = 2048u64;
        let mut idx = ReactionIndex::default();
        idx.apply(&custom_react_event(target, a, cid, size, true, 10));
        let r = idx.reactions_for(&target, &a);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].count, 1);
        assert_eq!(r[0].emoji_size, Some(size));
        // un-react with a strictly-newer HLC → present count 0 → chip gone
        idx.apply(&custom_react_event(target, a, cid, size, false, 20));
        let r2 = idx.reactions_for(&target, &a);
        assert!(
            r2.is_empty(),
            "a removed custom reaction must drop the chip, got {r2:?}"
        );
    }

    #[test]
    fn reaction_index_different_custom_emoji_are_distinct() {
        // Two reactors with DIFFERENT custom emoji (different cids) → two DTOs.
        let target = MessageId([4u8; 16]);
        let a = OwnerAddr([0xAA; 16]);
        let b = OwnerAddr([0xBB; 16]);
        let cid_a = [0x01; 32];
        let cid_b = [0x02; 32];
        let mut idx = ReactionIndex::default();
        idx.apply(&custom_react_event(target, a, cid_a, 100, true, 10));
        idx.apply(&custom_react_event(target, b, cid_b, 200, true, 11));
        let r = idx.reactions_for(&target, &a);
        assert_eq!(r.len(), 2, "distinct cids must yield distinct DTOs");
        let da = r
            .iter()
            .find(|d| d.emoji_cid.as_deref() == Some(hex::encode(cid_a).as_str()))
            .expect("cid_a DTO");
        assert_eq!(da.count, 1);
        assert_eq!(da.emoji_size, Some(100));
        assert!(da.mine);
        let db = r
            .iter()
            .find(|d| d.emoji_cid.as_deref() == Some(hex::encode(cid_b).as_str()))
            .expect("cid_b DTO");
        assert_eq!(db.count, 1);
        assert_eq!(db.emoji_size, Some(200));
        assert!(!db.mine);
    }

    #[test]
    fn reactions_for_surfaces_encrypted_flag_for_custom_emoji() {
        use harmony_content::cid::ContentId;
        // Two custom emoji CIDs: one with the encrypted flag unset, one set.
        let public_cid = [0x42u8; 32]; // encrypted flag unset
        let encrypted_cid = [0xB2u8; 32]; // encrypted flag set
        assert!(!ContentId::from_bytes(public_cid).flags().encrypted);
        assert!(ContentId::from_bytes(encrypted_cid).flags().encrypted);

        // Record one public-custom and one encrypted-custom reaction by the same
        // author on the same target. Distinct CIDs → distinct keys → two DTOs,
        // mirroring `reaction_index_different_custom_emoji_are_distinct`.
        let target = MessageId([7u8; 16]);
        let me = OwnerAddr([0xAA; 16]);
        let mut idx = ReactionIndex::default();
        idx.apply(&custom_react_event(target, me, public_cid, 200, true, 10));
        idx.apply(&custom_react_event(
            target,
            me,
            encrypted_cid,
            200,
            true,
            11,
        ));

        let dtos = idx.reactions_for(&target, &me);
        let pub_dto = dtos
            .iter()
            .find(|d| d.emoji_cid.as_deref() == Some(hex::encode(public_cid).as_str()))
            .expect("public reaction present");
        let enc_dto = dtos
            .iter()
            .find(|d| d.emoji_cid.as_deref() == Some(hex::encode(encrypted_cid).as_str()))
            .expect("encrypted reaction present");
        assert_eq!(pub_dto.encrypted, Some(false));
        assert_eq!(enc_dto.encrypted, Some(true));
    }

    // ── Task 3: ChannelLog reaction index (append-maintained + boot rebuild) ──

    /// Build an unsigned Post event bound to the given (cid, chid) so
    /// `ChannelLog::append`'s misroute check passes.
    fn post_event(
        id: MessageId,
        author: OwnerAddr,
        cid: SpaceId,
        chid: ChannelId,
        wall: u64,
    ) -> SignedChannelEvent {
        SignedChannelEvent::Post {
            id,
            community_id: cid,
            channel_id: chid,
            author,
            at: Hlc {
                wall_ms: wall,
                logical: 0,
                device_id: "test-dev".to_string(),
            },
            content_kind: 0,
            body: "test body".to_string(),
            mentions: None,
            attachments: None,
            reply_to: None,
            sig: [0u8; 64],
        }
    }

    /// Build an unsigned React event bound to the given (cid, chid) so
    /// `ChannelLog::append`'s misroute check passes.
    fn react_event_for(
        target: MessageId,
        author: OwnerAddr,
        cid: SpaceId,
        chid: ChannelId,
        emoji: &str,
        add: bool,
        wall: u64,
    ) -> SignedChannelEvent {
        SignedChannelEvent::React {
            target,
            author,
            community_id: cid,
            channel_id: chid,
            emoji_attachment: None,
            emoji: emoji.to_string(),
            add,
            at: Hlc {
                wall_ms: wall,
                logical: 0,
                device_id: "test-dev".to_string(),
            },
            sig: [0u8; 64],
        }
    }

    #[test]
    fn channel_log_reactions_survive_seal_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let (cid, chid) = (SpaceId([3; 16]), ChannelId([4; 16]));
        let cfg = ChannelLogConfig {
            seal_threshold_events: 4,
        };
        let mut log = ChannelLog::new(cid, chid, dir.path().to_path_buf(), cfg.clone());
        // append a Post, then a React to it, enough to force a seal, then more
        let target = MessageId([9; 16]);
        let me = OwnerAddr([0xAA; 16]);
        log.append(post_event(target, me, cid, chid, 10)).unwrap();
        log.append(react_event_for(target, me, cid, chid, "👍", true, 11))
            .unwrap();
        // drive a seal
        log.append(post_event(MessageId([8; 16]), me, cid, chid, 12))
            .unwrap();
        if log
            .append(post_event(MessageId([7; 16]), me, cid, chid, 13))
            .unwrap()
        {
            log.seal_and_persist().unwrap();
        }
        log.flush_tail().unwrap();
        // reload from disk — index must be rebuilt from the sealed segment
        let (reloaded, _n) = ChannelLog::reload(cid, chid, dir.path().to_path_buf(), cfg).unwrap();
        let r = reloaded.reactions_for(&target, &me);
        assert_eq!(r.iter().find(|d| d.emoji == "👍").unwrap().count, 1);
    }

    /// ZEB-536 robustness regression: reload must succeed even when a sealed
    /// segment file is missing from disk. Reactions are a non-critical derived
    /// view; a hard-fail on an unreadable segment would break channel load
    /// for channels with any missing/corrupt historical segment file.
    #[test]
    fn channel_log_reload_tolerates_unreadable_segment() {
        let dir = tempfile::tempdir().unwrap();
        let (cid, chid) = (SpaceId([5; 16]), ChannelId([6; 16]));
        // Small threshold so we can force a seal with few events.
        let cfg = ChannelLogConfig {
            seal_threshold_events: 2,
        };
        let mut log = ChannelLog::new(cid, chid, dir.path().to_path_buf(), cfg.clone());
        let me = OwnerAddr([0xCC; 16]);
        let target = MessageId([0xDD; 16]);

        // Append enough events to hit the seal threshold.
        log.append(post_event(target, me, cid, chid, 1_000))
            .unwrap();
        let needs_seal = log
            .append(post_event(MessageId([0xEE; 16]), me, cid, chid, 2_000))
            .unwrap();
        assert!(needs_seal, "threshold=2 must signal seal after 2nd append");
        // Seal: writes segments/00000000.cbor and updates the manifest.
        log.seal_and_persist().unwrap();
        // Append a tail React (post-seal) so we can assert it survives reload.
        log.append(react_event_for(target, me, cid, chid, "✅", true, 3_000))
            .unwrap();
        log.flush_tail().unwrap();

        // Delete the sealed segment file — simulates a missing/corrupt segment.
        let seg_path = dir.path().join("segments").join("00000000.cbor");
        std::fs::remove_file(&seg_path).expect("segment file must exist before we delete it");

        // reload MUST succeed despite the missing segment (non-critical reactions).
        let result = ChannelLog::reload(cid, chid, dir.path().to_path_buf(), cfg);
        assert!(
            result.is_ok(),
            "reload must tolerate a missing sealed segment; got: {:?}",
            result.err()
        );

        // Tail reactions (post-seal) must still be present after reload.
        let (reloaded, _) = result.unwrap();
        let reactions = reloaded.reactions_for(&target, &me);
        assert_eq!(
            reactions.iter().find(|d| d.emoji == "✅").map(|d| d.count),
            Some(1),
            "tail React must survive reload even when a sealed segment is missing"
        );
    }
}
