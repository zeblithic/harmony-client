# ZEB-248 Phase 2 — ChannelLog data plane (in-process) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the per-channel signed-event data plane (`SignedChannelEvent`, `ChannelKey` HKDF, AEAD wrap, `ChannelLog` segmented persistence, `ChannelLogReplayTracker`, `verify_channel_event`) as a self-contained module with no transport wiring. Phase 3 will graft a `ChannelLogEngine` + Zenoh + IPC surface on top.

**Architecture:** Single new module `src-tauri/src/community_channel_log.rs` exporting the full data plane. ChannelKey is HKDF-SHA256(IKM=MembershipKey, salt=community_id, info="channel:" || channel_id). Wire packet is `[12B random nonce][ChaCha20-Poly1305(ChannelKey, plaintext = canonical_cbor(SignedChannelEvent), AAD = b"harmony-channel-msg-v1")]`. ChannelLog is a segmented append-only log with an in-memory tail and sealed-on-disk segments referenced by a manifest; seal threshold is configurable per-instance (production: 1024 events; tests: 8). Replay tracker is `BTreeMap<(ChannelId, OwnerAddr, String /* device_id */), Hlc>` storing the highest-HLC seen per (channel, author, device). `verify_channel_event` runs §7 chain steps 3-7 against a pre-decrypted `SignedChannelEvent`; AEAD decrypt is a separate `decrypt_channel_packet` so the signature verifies the canonical-CBOR payload, not the wire bytes.

**Tech Stack:** Rust 1.x. `chacha20poly1305 = "0.10"` (already in deps), `hkdf = "0.12"` (already in deps), `sha2 = "0.10"` (already in deps), `ed25519-dalek` (already in use), `ciborium` (canonical CBOR — same as Phase 1), `tempfile` (atomic save). Platform-agnostic; no Tauri/IPC surface in this phase.

**Parent spec:** `docs/specs/2026-05-09-zeb-248-channels-within-communities-design.md` commit `5145484`. Sections §5.2 (SignedChannelEvent wire format), §6 (ChannelKey HKDF), §7 (verify_channel_event chain), §8 (ChannelLog persistence), §13.1 (Phase 2 unit tests).

**Plan-time decisions baked in (see §15 of parent spec):**
1. **Tail flush is engine-side.** Phase 2 ships pure async I/O functions (`flush_tail`, `seal_and_persist`); the debounce timer that drives periodic flushing lives in `ChannelLogEngine` (Phase 3). Keeps the data plane free of scheduler concerns.
2. **AEAD layer is split.** `verify_channel_event` operates on `SignedChannelEvent` (post-decryption); AEAD is a separate `decrypt_channel_packet` that runs first. Mirrors Phase 1's `verify_event` shape (which takes `SignedMembershipEvent`, not raw bytes).
3. **Seal threshold as constructor field.** `ChannelLogConfig { seal_threshold_events: usize }`. Production passes 1024; tests pass 8. No `cfg(test)` coupling.
4. **Replay tracker keyed on (channel, author, device_id).** Per spec §7 step 6 — captures per-device monotonicity inside each channel, mirrors `CommunityRootHlcTracker`'s shape.

---

## File Structure

**Create:**
- `src-tauri/src/community_channel_log.rs` — the Phase 2 module (everything below).

**Modify:**
- `src-tauri/src/lib.rs` — add `pub mod community_channel_log;` next to existing module declarations.

**No other files changed.** No IPC handlers added. No frontend changes. No `lib.rs` IPC surface.

---

## Task 0: Pre-flight + green baseline

**Files:** none modified. Verifies the branch is in a clean, green state before implementation begins.

- [ ] **Step 1: Confirm branch + base + clean tree**

```bash
git -C /Users/zeblith/work/zeblithic/harmony-client status
git -C /Users/zeblith/work/zeblithic/harmony-client log --oneline -3
git -C /Users/zeblith/work/zeblithic/harmony-client merge-base HEAD origin/main
```
Expected: branch `zeb-248-phase2-channel-log`, working tree clean, HEAD on `a58aff9` (the PR #94 merge), `merge-base` returns `a58aff9` (HEAD itself, since branch is freshly cut from main).

- [ ] **Step 2: Confirm cargo gates green on baseline (no edits yet)**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail && cargo fmt --all -- --check 2>&1 | tail -5 ; echo "FMT_EXIT=$?"
set -o pipefail && cargo clippy --all-targets -- -D warnings 2>&1 | tail -10 ; echo "CLIPPY_EXIT=$?"
set -o pipefail && cargo test --workspace --no-fail-fast 2>&1 | grep -E "^test result:" | awk '{p+=$4; f+=$6; i+=$8} END {print "TOTAL passed="p" failed="f" ignored="i}'
```
Expected:
- `FMT_EXIT=0`
- `CLIPPY_EXIT=0`
- `TOTAL passed=856 failed=0 ignored=2` (the post-PR-#94 baseline)

- [ ] **Step 3: Confirm dependency versions present in Cargo.toml**

```bash
grep -E "^(chacha20poly1305|hkdf|sha2|ed25519-dalek|ciborium|tempfile)" /Users/zeblith/work/zeblithic/harmony-client/src-tauri/Cargo.toml
```
Expected: all six lines present. No new dependencies need to be added.

- [ ] **Step 4 (NO COMMIT):** Task 0 has no commit. It's a baseline check only. Proceed to Task 1.

---

## Task 1: ChannelKey + derive_channel_key + HKDF tests

**Files:**
- Create: `src-tauri/src/community_channel_log.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod community_channel_log;`)

- [ ] **Step 1: Add module declaration to lib.rs**

Find the cluster of `pub mod` declarations near the top of `src-tauri/src/lib.rs` (somewhere in the first ~120 lines). Add `pub mod community_channel_log;` in alphabetical order (after `pub mod community_channel_config_persist;` if present, before `pub mod community_membership;`).

```bash
grep -n "^pub mod community_" /Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/lib.rs
```
Use the result to find the right insertion point. Insert one line:
```rust
pub mod community_channel_log;
```

- [ ] **Step 2: Create the module skeleton with ChannelKey + derive_channel_key**

Create `src-tauri/src/community_channel_log.rs` with this content (this is the FULL initial file; subsequent tasks append to it):

```rust
//! ZEB-248 Phase 2: per-channel data plane.
//!
//! Ships:
//! - `SignedChannelEvent` (Post variant; v3-reserved variants commented).
//! - `ChannelKey` + `derive_channel_key` (HKDF-SHA256 over MembershipKey).
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
use crate::owner_state_types::MembershipKey;
use crate::owner_state_types::SpaceId;
use hkdf::Hkdf;
use sha2::Sha256;

/// Symmetric key for one channel's wire encryption. Derived
/// deterministically from `(MembershipKey, community_id, channel_id)`
/// via HKDF-SHA256, so any Joined member can derive every channel's
/// key without out-of-band coordination. v3 will use this seam to
/// add private channels (distribute the ChannelKey to a subset of
/// members) without a wire-format break.
#[derive(Clone)]
pub struct ChannelKey([u8; 32]);

impl ChannelKey {
    /// Borrow the raw 32 bytes for AEAD initialization. Not `pub` —
    /// callers go through `encrypt_channel_packet` / `decrypt_channel_packet`.
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for ChannelKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ChannelKey(<32 bytes redacted>)")
    }
}

/// HKDF-SHA256 derivation of a per-channel symmetric key.
///
/// - IKM: `MembershipKey` raw bytes (32 B).
/// - Salt: `community_id` raw bytes (16 B). Community-scoped so the same
///   channel-id collision across two communities yields different keys.
/// - Info: `b"channel:" || channel_id` (8 + 16 = 24 B). Channel-scoped so
///   distinct channels in the same community yield different keys.
/// - Output: 32 B → ChannelKey.
///
/// Per spec §6.
pub fn derive_channel_key(
    mk: &MembershipKey,
    community_id: &SpaceId,
    channel_id: &ChannelId,
) -> ChannelKey {
    let salt = community_id.0;
    let mut info = Vec::with_capacity(8 + 16);
    info.extend_from_slice(b"channel:");
    info.extend_from_slice(&channel_id.0[..]);
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&salt), mk.as_bytes())
        .expand(&info, &mut out)
        .expect("32 ≤ 8160");
    ChannelKey(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_mk() -> MembershipKey {
        MembershipKey::new([0xaa; 32])
    }

    fn fixture_community(id: u8) -> SpaceId {
        SpaceId([id; 16])
    }

    fn fixture_channel(id: u8) -> ChannelId {
        ChannelId([id; 16])
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
        let k_a = derive_channel_key(&MembershipKey::new([0xaa; 32]), &cid, &chid);
        let k_b = derive_channel_key(&MembershipKey::new([0xbb; 32]), &cid, &chid);
        assert_ne!(
            k_a.as_bytes(),
            k_b.as_bytes(),
            "different membership keys must yield distinct channel keys"
        );
    }
}
```

- [ ] **Step 3: Verify gates pass for Task 1 surface**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail && cargo fmt --all -- --check 2>&1 | tail -5 ; echo "FMT_EXIT=$?"
set -o pipefail && cargo clippy --all-targets -- -D warnings 2>&1 | tail -10 ; echo "CLIPPY_EXIT=$?"
set -o pipefail && cargo test -p harmony-app community_channel_log:: 2>&1 | tail -10
```
Expected: FMT 0, CLIPPY 0, four `tests::derive_channel_key_*` tests pass.

- [ ] **Step 4: Commit**

```bash
git -C /Users/zeblith/work/zeblithic/harmony-client add src-tauri/src/community_channel_log.rs src-tauri/src/lib.rs
git -C /Users/zeblith/work/zeblithic/harmony-client commit -m "feat(zeb-269): ChannelKey + HKDF derivation for per-channel encryption"
```

---

## Task 2: SignedChannelEvent + sign_channel_event + canonical-CBOR fixture

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs` (append below the ChannelKey section, before the `#[cfg(test)]` block)
- Create: `src-tauri/tests/wire_format_channel_log_fixtures.rs` (canonical-CBOR pin test)

- [ ] **Step 1: Append SignedChannelEvent + helpers to community_channel_log.rs**

After the `derive_channel_key` function (and before `#[cfg(test)] mod tests`), insert:

```rust
use crate::community_membership::EventId;
use crate::owner_state_types::{Hlc, OwnerAddr};
use serde::{Deserialize, Serialize};

/// 16-byte ULID identifying a single message within a channel.
/// Generated client-side at post time. Stable identity for v3
/// references (Edit/Delete/React variants will target this id).
pub type MessageId = [u8; 16];

/// Static AAD bytes for ChaCha20-Poly1305 wrapping of channel events.
/// v3 may extend with per-event AAD; for now this is a constant across
/// every packet on every channel.
pub const CHANNEL_PACKET_AAD: &[u8] = b"harmony-channel-msg-v1";

/// One signed channel event. Phase 2 ships only the `Post` variant.
/// Wire format: 2-key adjacently-tagged outer (`tg` + `vl`); inner
/// fields all 2-char keys to satisfy the same-length-keys invariant.
///
/// `sg` covers canonical CBOR of `(id, community_id, channel_id, author,
/// at, content_kind, body, reply_to)` — every field minus the signature
/// itself. v3 Edit/Delete/React variants will sign their own typed
/// payloads with no field reuse across variants.
///
/// Per spec §5.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tg", content = "vl")]
pub enum SignedChannelEvent {
    #[serde(rename = "p")]
    Post {
        #[serde(rename = "id")]
        id: MessageId,
        #[serde(rename = "ci")]
        community_id: SpaceId,
        #[serde(rename = "ch")]
        channel_id: ChannelId,
        #[serde(rename = "au")]
        author: OwnerAddr,
        #[serde(rename = "at")]
        at: Hlc,
        #[serde(rename = "kd")]
        content_kind: u8,
        #[serde(rename = "bd")]
        body: String,
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
    // React { id, ci, ch, au, at, em, sg }
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
}

/// The signed-set tuple. Canonical CBOR of this is what `sg` covers
/// AND what the SHA-256 (event_id derivation) hashes.
#[derive(Serialize)]
struct ChannelPostSignedSet<'a> {
    id: &'a MessageId,
    community_id: &'a SpaceId,
    channel_id: &'a ChannelId,
    author: &'a OwnerAddr,
    at: &'a Hlc,
    content_kind: u8,
    body: &'a str,
    reply_to: &'a Option<MessageId>,
}

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
    #[error("malformed packet (length {0} bytes — need at least 12 for nonce)")]
    MalformedPacket(usize),
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
    let signed_set = ChannelPostSignedSet {
        id: &payload.id,
        community_id: &payload.community_id,
        channel_id: &payload.channel_id,
        author: &payload.author,
        at: &payload.at,
        content_kind: payload.content_kind,
        body: payload.body,
        reply_to: &payload.reply_to,
    };
    let mut canon = Vec::with_capacity(256);
    ciborium::into_writer(&signed_set, &mut canon)
        .map_err(|e| ChannelEventError::CborEncode(e.to_string()))?;
    let sig = signing_key.sign(&canon).to_bytes();
    Ok(SignedChannelEvent::Post {
        id: payload.id,
        community_id: payload.community_id,
        channel_id: payload.channel_id,
        author: payload.author,
        at: payload.at.clone(),
        content_kind: payload.content_kind,
        body: payload.body.to_string(),
        reply_to: payload.reply_to,
        sig,
    })
}

/// Recompute the signed-set canonical CBOR for a SignedChannelEvent::Post.
/// Used by both `sign_channel_event` (above, via the borrowed payload
/// path) and `verify_channel_event` (Task 5, via this borrowed path on
/// the deserialized event).
fn signed_set_canonical_cbor(event: &SignedChannelEvent) -> Result<Vec<u8>, ChannelEventError> {
    let SignedChannelEvent::Post {
        id,
        community_id,
        channel_id,
        author,
        at,
        content_kind,
        body,
        reply_to,
        ..
    } = event;
    let signed_set = ChannelPostSignedSet {
        id,
        community_id,
        channel_id,
        author,
        at,
        content_kind: *content_kind,
        body,
        reply_to,
    };
    let mut canon = Vec::with_capacity(256);
    ciborium::into_writer(&signed_set, &mut canon)
        .map_err(|e| ChannelEventError::CborEncode(e.to_string()))?;
    Ok(canon)
}
```

**Reuses the existing `serialize_bytes_as_bstr` / `deserialize_bytes_from_bstr` helpers** declared `pub(crate)` in `src-tauri/src/owner_state_types.rs:18` and used by Phase 1's `SignedMembershipEvent::sig`. ciborium emits these as `bstr(N)` (65 bytes for a 64-byte signature) — vs the 67-byte CBOR `array(64)` form that serde would otherwise pick. No new dependency needed.

- [ ] **Step 2: Add unit tests inside the existing `mod tests` block**

Append to `mod tests`:
```rust
    fn fixture_owner_addr(byte: u8) -> OwnerAddr {
        OwnerAddr([byte; 32])
    }

    fn fixture_signing_key(seed: u8) -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
    }

    fn fixture_hlc(wall_ms: u64, dev: &str) -> Hlc {
        Hlc {
            wall_ms,
            logical: 0,
            device_id: dev.to_string(),
        }
    }

    fn fixture_payload(body: &'static str) -> (ChannelPostPayload<'static>, ed25519_dalek::SigningKey)
    {
        let key = fixture_signing_key(0xa1);
        let payload = ChannelPostPayload {
            id: [0x11; 16],
            community_id: fixture_community(0xc0),
            channel_id: fixture_channel(0x01),
            author: fixture_owner_addr(0xa1),
            at: fixture_hlc(100_000, "a-dev"),
            content_kind: 0,
            body,
            reply_to: None,
        };
        (payload, key)
    }

    #[test]
    fn sign_channel_event_round_trip() {
        let (payload, key) = fixture_payload("hello, world!");
        let signed = sign_channel_event(&payload, &key).expect("sign");
        let SignedChannelEvent::Post {
            id,
            community_id,
            channel_id,
            author,
            at,
            content_kind,
            body,
            reply_to,
            sig,
        } = signed;
        assert_eq!(id, payload.id);
        assert_eq!(community_id, payload.community_id);
        assert_eq!(channel_id, payload.channel_id);
        assert_eq!(author, payload.author);
        assert_eq!(at, payload.at);
        assert_eq!(content_kind, payload.content_kind);
        assert_eq!(body, payload.body);
        assert_eq!(reply_to, payload.reply_to);
        assert_eq!(sig.len(), 64);
    }

    #[test]
    fn sign_channel_event_signature_verifies_against_canonical_cbor() {
        use ed25519_dalek::Verifier;
        let (payload, key) = fixture_payload("verify me");
        let signed = sign_channel_event(&payload, &key).expect("sign");
        let canon = signed_set_canonical_cbor(&signed).expect("canon");
        let SignedChannelEvent::Post { sig, author, .. } = &signed;
        let pubkey = key.verifying_key();
        // Author addr should be derivable from pubkey in production; here
        // we just verify the signature against the explicit pubkey.
        let _ = author;
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
```

- [ ] **Step 3: Create canonical-CBOR pin fixture in tests/wire_format_channel_log_fixtures.rs**

This file pins one `SignedChannelEvent::Post` to a hex string so any wire-format drift (e.g., field-order changes, key renames, encoding changes) becomes a loud test failure. Mirrors the pattern in `tests/wire_format_community_fixtures.rs` and `tests/wire_format_community_sync_fixtures.rs`.

```rust
//! ZEB-269: canonical-CBOR pin tests for SignedChannelEvent.
//!
//! Any field-order change, key rename, or encoding shift in
//! SignedChannelEvent::Post will deliberately break this pin. If the
//! wire format genuinely needs to change, regenerate the hex via:
//!
//!   #[test]
//!   fn print_pin() {
//!       let bytes = ciborium::ser::into_writer_canonical(...);
//!       eprintln!("{}", hex::encode(bytes));
//!   }

use harmony_app::community_channel_log::{sign_channel_event, ChannelPostPayload, SignedChannelEvent};
use harmony_app::community_membership::ChannelId;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

fn fixture() -> SignedChannelEvent {
    let key = ed25519_dalek::SigningKey::from_bytes(&[0xa1; 32]);
    let payload = ChannelPostPayload {
        id: [0x11; 16],
        community_id: SpaceId([0xc0; 16]),
        channel_id: ChannelId([0x01; 16]),
        author: OwnerAddr([0xa1; 32]),
        at: Hlc {
            wall_ms: 100_000,
            logical: 0,
            device_id: "a-dev".to_string(),
        },
        content_kind: 0,
        body: "hello",
        reply_to: None,
    };
    sign_channel_event(&payload, &key).expect("sign")
}

#[test]
fn signed_channel_event_post_wire_bytes_pinned() {
    let event = fixture();
    let mut bytes = Vec::new();
    ciborium::into_writer(&event, &mut bytes).expect("encode");
    // Pin the byte sequence. If this fails after intentional schema
    // change, regenerate via temporary `eprintln!("{}", hex::encode(...))`.
    let hex = hex::encode(&bytes);
    insta::assert_snapshot!(hex);
}
```

If `insta` isn't already a workspace dev-dependency, the test should instead inline the expected hex. Check first:
```bash
grep "^insta" /Users/zeblith/work/zeblithic/harmony-client/src-tauri/Cargo.toml
```
- If `insta` is present → use the snapshot form above (and on first run, accept the snapshot via `cargo insta accept`).
- If not present → replace the `insta::assert_snapshot!(hex)` line with `assert_eq!(hex, "...");` where `...` is the literal hex string. Generate this hex by running the test once with a temporary `eprintln!`, copy the output, paste into the assertion. Drop the eprintln.

The implementer will need to run the test once to capture the hex (in either form) and pin it. This is normal for new wire-format pins.

- [ ] **Step 4: Verify gates pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail && cargo fmt --all -- --check 2>&1 | tail -5 ; echo "FMT_EXIT=$?"
set -o pipefail && cargo clippy --all-targets -- -D warnings 2>&1 | tail -10 ; echo "CLIPPY_EXIT=$?"
set -o pipefail && cargo test -p harmony-app community_channel_log:: 2>&1 | tail -10
set -o pipefail && cargo test --test wire_format_channel_log_fixtures 2>&1 | tail -10
```
Expected: FMT 0, CLIPPY 0, all `community_channel_log::tests::*` pass, the wire-format pin passes (or accepts a fresh snapshot on first run).

- [ ] **Step 5: Commit**

```bash
git -C /Users/zeblith/work/zeblithic/harmony-client add src-tauri/src/community_channel_log.rs src-tauri/tests/wire_format_channel_log_fixtures.rs
# If insta snapshot was created:
# git -C /Users/zeblith/work/zeblithic/harmony-client add src-tauri/tests/snapshots/
git -C /Users/zeblith/work/zeblithic/harmony-client commit -m "feat(zeb-269): SignedChannelEvent + sign_channel_event + wire-format pin"
```

---

## Task 3: AEAD encrypt/decrypt + AAD-tamper rejection tests

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs`

- [ ] **Step 1: Add AEAD encrypt/decrypt helpers**

Append after the `signed_set_canonical_cbor` function (and before `mod tests`):

```rust
use chacha20poly1305::aead::{Aead, OsRng, Payload};
use chacha20poly1305::{AeadCore, ChaCha20Poly1305, KeyInit};

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
    let mut plaintext = Vec::with_capacity(256);
    ciborium::into_writer(event, &mut plaintext)
        .map_err(|e| ChannelEventError::CborEncode(e.to_string()))?;
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: &plaintext,
                aad: CHANNEL_PACKET_AAD,
            },
        )
        .map_err(|e| ChannelEventError::AeadEncrypt(e.to_string()))?;
    let mut packet = Vec::with_capacity(12 + ciphertext.len());
    packet.extend_from_slice(nonce.as_slice());
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
    if packet.len() < 12 {
        return Err(ChannelEventError::MalformedPacket(packet.len()));
    }
    let (nonce_bytes, ciphertext) = packet.split_at(12);
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
```

- [ ] **Step 2: Add AEAD round-trip + tamper tests inside `mod tests`**

```rust
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
        assert!(packet.len() > 12 + 16, "packet must include nonce + tag + body");
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
        let err = decrypt_channel_packet(&key, &[0u8; 5]).expect_err("must reject short packet");
        assert!(matches!(err, ChannelEventError::MalformedPacket(5)));
    }
```

- [ ] **Step 3: Verify gates pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail && cargo fmt --all -- --check 2>&1 | tail -5 ; echo "FMT_EXIT=$?"
set -o pipefail && cargo clippy --all-targets -- -D warnings 2>&1 | tail -10 ; echo "CLIPPY_EXIT=$?"
set -o pipefail && cargo test -p harmony-app community_channel_log::tests::aead 2>&1 | tail -10
```
Expected: FMT 0, CLIPPY 0, four `tests::aead_*` tests pass.

- [ ] **Step 4: Commit**

```bash
git -C /Users/zeblith/work/zeblithic/harmony-client add src-tauri/src/community_channel_log.rs
git -C /Users/zeblith/work/zeblithic/harmony-client commit -m "feat(zeb-269): ChaCha20-Poly1305 AEAD wire packet encrypt/decrypt"
```

---

## Task 4: ChannelLogReplayTracker

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs`

- [ ] **Step 1: Append the replay tracker**

After the AEAD section, before `mod tests`:

```rust
use std::collections::BTreeMap;

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

    /// Check + advance the tracker for an incoming event. Returns Ok
    /// if the event is strictly newer than the last seen for this
    /// (channel, author, device) triple, or never-seen. Returns
    /// `Err(Replay)` otherwise.
    ///
    /// On Ok, the tracker is bumped to this event's HLC. Concurrent
    /// callers must serialize externally — the tracker holds
    /// `&mut self` and is not internally locked.
    pub fn check_and_advance(
        &mut self,
        event: &SignedChannelEvent,
    ) -> Result<(), ChannelEventError> {
        let SignedChannelEvent::Post {
            channel_id,
            author,
            at,
            id,
            ..
        } = event;
        let key = (*channel_id, *author, at.device_id.clone());
        if let Some(prev) = self.last_seen.get(&key) {
            // Strict monotonicity by sort-key: (wall_ms, logical, device_id).
            // device_id is constant within this key, so really just
            // (wall_ms, logical).
            if (at.wall_ms, at.logical) <= (prev.wall_ms, prev.logical) {
                return Err(ChannelEventError::Replay {
                    event_id: *id,
                    author: *author,
                    device_id: at.device_id.clone(),
                    at: at.clone(),
                });
            }
        }
        self.last_seen.insert(key, at.clone());
        Ok(())
    }

    /// Snapshot of the current tracker state. Useful for tests + Phase 3
    /// engine startup (rebuild from persisted segments + tail).
    pub fn last_seen(&self) -> &BTreeMap<(ChannelId, OwnerAddr, String), Hlc> {
        &self.last_seen
    }
}
```

- [ ] **Step 2: Add replay-tracker tests inside `mod tests`**

```rust
    fn fixture_signed_event(at_wall: u64, at_logical: u32, device: &str) -> SignedChannelEvent {
        let key = fixture_signing_key(0xa1);
        let payload = ChannelPostPayload {
            id: [0x11; 16],
            community_id: fixture_community(0xc0),
            channel_id: fixture_channel(0x01),
            author: fixture_owner_addr(0xa1),
            at: Hlc {
                wall_ms: at_wall,
                logical: at_logical,
                device_id: device.to_string(),
            },
            content_kind: 0,
            body: "test",
            reply_to: None,
        };
        sign_channel_event(&payload, &key).expect("sign")
    }

    #[test]
    fn replay_tracker_accepts_strictly_monotone() {
        let mut t = ChannelLogReplayTracker::new();
        let e1 = fixture_signed_event(100, 0, "a-dev");
        let e2 = fixture_signed_event(200, 0, "a-dev");
        t.check_and_advance(&e1).expect("first event");
        t.check_and_advance(&e2).expect("strictly monotone follow-up");
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
        t.check_and_advance(&e_b).expect(
            "b-dev's earlier wall time is fine — distinct device lane",
        );
    }
```

- [ ] **Step 3: Verify gates pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail && cargo fmt --all -- --check 2>&1 | tail -5 ; echo "FMT_EXIT=$?"
set -o pipefail && cargo clippy --all-targets -- -D warnings 2>&1 | tail -10 ; echo "CLIPPY_EXIT=$?"
set -o pipefail && cargo test -p harmony-app community_channel_log::tests::replay 2>&1 | tail -10
```
Expected: FMT 0, CLIPPY 0, five `tests::replay_tracker_*` tests pass.

- [ ] **Step 4: Commit**

```bash
git -C /Users/zeblith/work/zeblithic/harmony-client add src-tauri/src/community_channel_log.rs
git -C /Users/zeblith/work/zeblithic/harmony-client commit -m "feat(zeb-269): ChannelLogReplayTracker for per-(channel, author, device) HLC monotonicity"
```

---

## Task 5: verify_channel_event chain (steps 3-7)

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs`

This task implements the §7 chain on a pre-decrypted `SignedChannelEvent`. AEAD decrypt is the caller's responsibility (`decrypt_channel_packet` from Task 3 runs first).

- [ ] **Step 1: Sketch the chain function and supporting types**

Append before `mod tests`:

```rust
use crate::community_membership::ChannelInfo;

/// Snapshot of community state at a particular HLC, exposing just
/// what `verify_channel_event` needs. Phase 3's engine will produce
/// this by materializing the community-state CRDT to `event.at`;
/// Phase 2 keeps the trait small so unit tests can pass mock state
/// without dragging in the full CommunityState materialization.
pub trait CommunityStateAtHlc {
    /// Lookup the channel-config snapshot at `at`. Returns None if
    /// the channel didn't exist at that HLC.
    fn channel_at(&self, channel_id: &ChannelId, at: &Hlc) -> Option<ChannelInfo>;

    /// Author's effective power level at `at`. Returns None if the
    /// author was not Joined (or never present) at `at`.
    fn author_power_at(&self, author: &OwnerAddr, at: &Hlc) -> Option<u8>;
}

/// Identity-resolution trait. Mirrors the existing
/// `CommunitySyncEngineConfig::identity_resolver` shape so the Phase 3
/// engine can pass through its existing IdentityResolver impl.
#[async_trait::async_trait]
pub trait ChannelIdentityResolver: Send + Sync {
    /// Resolve OwnerAddr → identity-public-key bytes (Ed25519 verifying key).
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 32]>;
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
pub async fn verify_channel_event<S, R>(
    event: &SignedChannelEvent,
    expected_community_id: &SpaceId,
    expected_channel_id: &ChannelId,
    state: &S,
    resolver: &R,
    replay_tracker: &mut ChannelLogReplayTracker,
) -> Result<(), ChannelEventError>
where
    S: CommunityStateAtHlc + Sync,
    R: ChannelIdentityResolver + ?Sized,
{
    let SignedChannelEvent::Post {
        community_id,
        channel_id,
        author,
        at,
        sig,
        ..
    } = event;

    // Step 3: misroute defense.
    if community_id != expected_community_id || channel_id != expected_channel_id {
        return Err(ChannelEventError::Misroute {
            expected_community: *expected_community_id,
            expected_channel: *expected_channel_id,
            got_community: *community_id,
            got_channel: *channel_id,
        });
    }

    // Step 4: identity resolution.
    let identity_pub = resolver
        .resolve(author)
        .await
        .ok_or(ChannelEventError::UnknownAuthor(*author))?;

    // Step 5: signature verify.
    let canon = signed_set_canonical_cbor(event)?;
    let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&identity_pub)
        .map_err(|_| ChannelEventError::BadSignature)?;
    use ed25519_dalek::Verifier;
    verifying_key
        .verify(&canon, &ed25519_dalek::Signature::from_bytes(sig))
        .map_err(|_| ChannelEventError::BadSignature)?;

    // Step 6: replay-tracker check + advance. Bumps tracker on Ok.
    replay_tracker.check_and_advance(event)?;

    // Step 7: membership-at-HLC gate. Both `write_power` and the
    // tombstone (`deleted_at`) are evaluated AS OF event.at, not as
    // of "now" — channel-config events between post-time and verify-
    // time may have raised/lowered the threshold or deleted the channel.
    let channel_info = state.channel_at(channel_id, at).ok_or_else(|| {
        ChannelEventError::NotAuthorized(format!(
            "channel {:?} did not exist at {:?}",
            channel_id, at
        ))
    })?;
    if let Some(deleted_at) = &channel_info.deleted_at {
        // `at > deleted_at` means the post happened after deletion.
        if (at.wall_ms, at.logical, &at.device_id)
            > (deleted_at.wall_ms, deleted_at.logical, &deleted_at.device_id)
        {
            return Err(ChannelEventError::NotAuthorized(format!(
                "channel deleted at {:?}, post at {:?}",
                deleted_at, at
            )));
        }
    }
    let author_power = state.author_power_at(author, at).ok_or_else(|| {
        ChannelEventError::NotAuthorized(format!(
            "author {:?} not Joined at {:?}",
            author, at
        ))
    })?;
    if author_power < channel_info.write_power {
        return Err(ChannelEventError::NotAuthorized(format!(
            "author power {} < channel write_power {}",
            author_power, channel_info.write_power
        )));
    }

    Ok(())
}
```

**Note on `ChannelInfo.deleted_at` field name.** Phase 1 introduced `ChannelInfo` in `community_membership.rs`. Verify the field name with:
```bash
grep -A 10 "^pub struct ChannelInfo" /Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/community_membership.rs
```
The plan assumes `deleted_at: Option<Hlc>` and `write_power: u8`. If Phase 1 named these differently (e.g., `tombstoned_at`, `min_write_power`), update the verify_channel_event body to match. **This is a known plan-time uncertainty: the implementer must check Phase 1's exact ChannelInfo field names and adjust.**

- [ ] **Step 2: Mock state + resolver helpers for tests inside `mod tests`**

```rust
    use std::collections::HashMap;

    struct MockState {
        channels: HashMap<ChannelId, Vec<(Hlc, ChannelInfo)>>,
        members: HashMap<OwnerAddr, Vec<(Hlc, u8)>>, // (joined_at, power)
        // For "left" semantics, store the leave HLC per author. None = still joined.
        left_at: HashMap<OwnerAddr, Hlc>,
    }

    impl CommunityStateAtHlc for MockState {
        fn channel_at(&self, channel_id: &ChannelId, at: &Hlc) -> Option<ChannelInfo> {
            // Return the channel-config snapshot most recent at `at`.
            let history = self.channels.get(channel_id)?;
            history
                .iter()
                .filter(|(hlc, _)| {
                    (hlc.wall_ms, hlc.logical, &hlc.device_id)
                        <= (at.wall_ms, at.logical, &at.device_id)
                })
                .last()
                .map(|(_, info)| info.clone())
        }

        fn author_power_at(&self, author: &OwnerAddr, at: &Hlc) -> Option<u8> {
            // Most recent power level at-or-before `at`. None if author
            // had Left before `at` or was never Joined.
            if let Some(left_hlc) = self.left_at.get(author) {
                if (left_hlc.wall_ms, left_hlc.logical, &left_hlc.device_id)
                    <= (at.wall_ms, at.logical, &at.device_id)
                {
                    return None;
                }
            }
            let history = self.members.get(author)?;
            history
                .iter()
                .filter(|(hlc, _)| {
                    (hlc.wall_ms, hlc.logical, &hlc.device_id)
                        <= (at.wall_ms, at.logical, &at.device_id)
                })
                .last()
                .map(|(_, p)| *p)
        }
    }

    struct MockResolver {
        entries: HashMap<OwnerAddr, [u8; 32]>,
    }

    #[async_trait::async_trait]
    impl ChannelIdentityResolver for MockResolver {
        async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 32]> {
            self.entries.get(addr).copied()
        }
    }

    fn fixture_state_with_alice_joined() -> (MockState, MockResolver) {
        let alice = fixture_owner_addr(0xa1);
        let alice_signing = fixture_signing_key(0xa1);
        let mut channels = HashMap::new();
        channels.insert(
            fixture_channel(0x01),
            vec![(
                Hlc {
                    wall_ms: 50_000,
                    logical: 0,
                    device_id: "creator".into(),
                },
                ChannelInfo {
                    name: "general".into(),
                    write_power: 0,
                    created_at: Hlc {
                        wall_ms: 50_000,
                        logical: 0,
                        device_id: "creator".into(),
                    },
                    deleted_at: None,
                    // Other fields per Phase 1 ChannelInfo — fill with defaults
                    // appropriate to whatever Phase 1 added. The implementer
                    // checks the actual ChannelInfo definition and pads.
                    ..Default::default()
                },
            )],
        );
        let mut members = HashMap::new();
        members.insert(alice, vec![(fixture_hlc(60_000, "a-dev"), 100)]);
        let state = MockState {
            channels,
            members,
            left_at: HashMap::new(),
        };
        let mut entries = HashMap::new();
        entries.insert(alice, alice_signing.verifying_key().to_bytes());
        let resolver = MockResolver { entries };
        (state, resolver)
    }
```

**Note:** the `..Default::default()` for `ChannelInfo` assumes Phase 1's `ChannelInfo` either derives `Default` or provides equivalent defaults. If it doesn't, the implementer must construct `ChannelInfo` explicitly with all fields. **This is a known plan-time uncertainty.**

- [ ] **Step 3: Add the chain test cases**

```rust
    #[tokio::test]
    async fn verify_channel_event_happy_path() {
        let (state, resolver) = fixture_state_with_alice_joined();
        let mut tracker = ChannelLogReplayTracker::new();
        let event = fixture_signed_event(100_000, 0, "a-dev");
        verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &resolver,
            &mut tracker,
        )
        .await
        .expect("happy path verifies");
    }

    #[tokio::test]
    async fn verify_channel_event_rejects_misroute_community() {
        let (state, resolver) = fixture_state_with_alice_joined();
        let mut tracker = ChannelLogReplayTracker::new();
        let event = fixture_signed_event(100_000, 0, "a-dev");
        let err = verify_channel_event(
            &event,
            &fixture_community(0xff),
            &fixture_channel(0x01),
            &state,
            &resolver,
            &mut tracker,
        )
        .await
        .expect_err("wrong community must reject");
        assert!(matches!(err, ChannelEventError::Misroute { .. }));
    }

    #[tokio::test]
    async fn verify_channel_event_rejects_misroute_channel() {
        let (state, resolver) = fixture_state_with_alice_joined();
        let mut tracker = ChannelLogReplayTracker::new();
        let event = fixture_signed_event(100_000, 0, "a-dev");
        let err = verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0xff),
            &state,
            &resolver,
            &mut tracker,
        )
        .await
        .expect_err("wrong channel must reject");
        assert!(matches!(err, ChannelEventError::Misroute { .. }));
    }

    #[tokio::test]
    async fn verify_channel_event_rejects_unknown_author() {
        let (state, _) = fixture_state_with_alice_joined();
        // Empty resolver — author won't resolve.
        let resolver = MockResolver {
            entries: HashMap::new(),
        };
        let mut tracker = ChannelLogReplayTracker::new();
        let event = fixture_signed_event(100_000, 0, "a-dev");
        let err = verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &resolver,
            &mut tracker,
        )
        .await
        .expect_err("unresolvable author must reject");
        assert!(matches!(err, ChannelEventError::UnknownAuthor(_)));
    }

    #[tokio::test]
    async fn verify_channel_event_rejects_bad_signature() {
        let (state, resolver) = fixture_state_with_alice_joined();
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
            &resolver,
            &mut tracker,
        )
        .await
        .expect_err("bad sig must reject");
        assert!(matches!(err, ChannelEventError::BadSignature));
    }

    #[tokio::test]
    async fn verify_channel_event_rejects_replay() {
        let (state, resolver) = fixture_state_with_alice_joined();
        let mut tracker = ChannelLogReplayTracker::new();
        let event = fixture_signed_event(100_000, 0, "a-dev");
        verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &resolver,
            &mut tracker,
        )
        .await
        .expect("first verify");
        let err = verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &resolver,
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
        let alice = fixture_owner_addr(0xa1);
        let mut channels = HashMap::new();
        channels.insert(
            fixture_channel(0x01),
            vec![(
                Hlc {
                    wall_ms: 50_000,
                    logical: 0,
                    device_id: "creator".into(),
                },
                ChannelInfo {
                    name: "ops".into(),
                    write_power: 50,
                    created_at: Hlc {
                        wall_ms: 50_000,
                        logical: 0,
                        device_id: "creator".into(),
                    },
                    deleted_at: None,
                    ..Default::default()
                },
            )],
        );
        let mut members = HashMap::new();
        members.insert(alice, vec![(fixture_hlc(60_000, "a-dev"), 0)]);
        let state = MockState {
            channels,
            members,
            left_at: HashMap::new(),
        };
        let mut entries = HashMap::new();
        entries.insert(alice, fixture_signing_key(0xa1).verifying_key().to_bytes());
        let resolver = MockResolver { entries };
        let mut tracker = ChannelLogReplayTracker::new();
        let event = fixture_signed_event(100_000, 0, "a-dev");
        let err = verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &resolver,
            &mut tracker,
        )
        .await
        .expect_err("below threshold must reject");
        assert!(matches!(err, ChannelEventError::NotAuthorized(_)));
    }

    #[tokio::test]
    async fn verify_channel_event_rejects_post_after_delete() {
        let alice = fixture_owner_addr(0xa1);
        let mut channels = HashMap::new();
        channels.insert(
            fixture_channel(0x01),
            vec![(
                Hlc {
                    wall_ms: 50_000,
                    logical: 0,
                    device_id: "creator".into(),
                },
                ChannelInfo {
                    name: "deleted".into(),
                    write_power: 0,
                    created_at: Hlc {
                        wall_ms: 50_000,
                        logical: 0,
                        device_id: "creator".into(),
                    },
                    // Channel deleted at wall=80_000.
                    deleted_at: Some(Hlc {
                        wall_ms: 80_000,
                        logical: 0,
                        device_id: "mod".into(),
                    }),
                    ..Default::default()
                },
            )],
        );
        let mut members = HashMap::new();
        members.insert(alice, vec![(fixture_hlc(60_000, "a-dev"), 100)]);
        let state = MockState {
            channels,
            members,
            left_at: HashMap::new(),
        };
        let mut entries = HashMap::new();
        entries.insert(alice, fixture_signing_key(0xa1).verifying_key().to_bytes());
        let resolver = MockResolver { entries };
        let mut tracker = ChannelLogReplayTracker::new();
        // Post at wall=100_000 — after delete (80_000).
        let event = fixture_signed_event(100_000, 0, "a-dev");
        let err = verify_channel_event(
            &event,
            &fixture_community(0xc0),
            &fixture_channel(0x01),
            &state,
            &resolver,
            &mut tracker,
        )
        .await
        .expect_err("post-delete must reject");
        assert!(matches!(err, ChannelEventError::NotAuthorized(_)));
    }
```

- [ ] **Step 4: Verify gates pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail && cargo fmt --all -- --check 2>&1 | tail -5 ; echo "FMT_EXIT=$?"
set -o pipefail && cargo clippy --all-targets -- -D warnings 2>&1 | tail -10 ; echo "CLIPPY_EXIT=$?"
set -o pipefail && cargo test -p harmony-app community_channel_log::tests::verify 2>&1 | tail -15
```
Expected: FMT 0, CLIPPY 0, eight `tests::verify_channel_event_*` tests pass.

- [ ] **Step 5: Commit**

```bash
git -C /Users/zeblith/work/zeblithic/harmony-client add src-tauri/src/community_channel_log.rs
git -C /Users/zeblith/work/zeblithic/harmony-client commit -m "feat(zeb-269): verify_channel_event chain (misroute, identity, signature, replay, membership-at-HLC)"
```

---

## Task 6: ChannelLog persistence (manifest + tail + segments + seal/reload)

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs`

This task ships the data structure + atomic-rename I/O for the segmented log. Production threshold: 1024 events. Tests: 8.

- [ ] **Step 1: Append the persistence layer**

Before `mod tests`:

```rust
use std::path::{Path, PathBuf};

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
pub struct ChannelLog {
    pub manifest: ChannelLogManifest,
    pub tail: Vec<SignedChannelEvent>,
    config: ChannelLogConfig,
    /// Root directory: `<identity_dir>/communities/{cid_hex}/channels/{ch_id_hex}/`.
    /// Manifest at `root/manifest.cbor`, tail at `root/tail.cbor`,
    /// sealed segments at `root/segments/{N:08x}.cbor`.
    root: PathBuf,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentDescriptor {
    /// `(first_event.at, last_event.at)` inclusive. Used by Phase 3
    /// backfill to filter which segments overlap a `since` query.
    pub range: (Hlc, Hlc),
    pub count: u32,
    pub handle: SegmentHandle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SegmentHandle {
    /// v2: local-disk segment, path relative to the channel's root dir.
    #[serde(rename = "f")]
    LocalFile { rel_path: String },
    // v3 reserved (additive — no v2 wire-format break):
    // #[serde(rename = "c")] CasBook { cid: ContentId },
}

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
        }
    }

    /// Push an already-verified event onto the tail. Returns `Ok(true)`
    /// if the seal threshold was reached after this push (caller should
    /// then call `seal_and_persist` to flush). Returns `Ok(false)`
    /// otherwise. Pure / sync / no I/O.
    pub fn append(&mut self, event: SignedChannelEvent) -> bool {
        self.tail.push(event);
        self.tail.len() >= self.config.seal_threshold_events
    }

    /// Persist the active tail to `root/tail.cbor`. Atomic-rename via
    /// `tempfile`. Idempotent; safe to call repeatedly.
    pub fn flush_tail(&self) -> Result<(), ChannelLogPersistError> {
        std::fs::create_dir_all(&self.root)?;
        let mut bytes = Vec::with_capacity(1024);
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
    /// Idempotent at the manifest level — a crash between segment
    /// write and manifest update will leave an orphaned segment file
    /// that the next startup can rediscover (Phase 3 may add explicit
    /// orphan recovery; v2 reload tolerates extra files in segments/).
    pub fn seal_and_persist(&mut self) -> Result<(), ChannelLogPersistError> {
        if self.tail.is_empty() {
            // Nothing to seal. No-op.
            return Ok(());
        }
        std::fs::create_dir_all(self.root.join("segments"))?;
        let next_index = self.manifest.segments.len() as u32;
        let rel_path = format!("segments/{:08x}.cbor", next_index);
        let abs_path = self.root.join(&rel_path);
        let mut seg_bytes = Vec::with_capacity(64 * self.tail.len());
        ciborium::into_writer(&self.tail, &mut seg_bytes)
            .map_err(|e| ChannelLogPersistError::CborEncode(e.to_string()))?;
        crate::owner_state_persist::save_atomically(&abs_path, &seg_bytes)
            .map_err(|e| ChannelLogPersistError::Io(e.to_string()))?;
        let first = self.tail.first().expect("tail non-empty checked above");
        let last = self.tail.last().expect("tail non-empty checked above");
        let (first_at, last_at) = match (first, last) {
            (
                SignedChannelEvent::Post { at: a, .. },
                SignedChannelEvent::Post { at: b, .. },
            ) => (a.clone(), b.clone()),
        };
        self.manifest.segments.push(SegmentDescriptor {
            range: (first_at, last_at),
            count: self.tail.len() as u32,
            handle: SegmentHandle::LocalFile { rel_path },
        });
        // Persist manifest BEFORE clearing tail. If we crash after
        // segment + manifest writes, the cleared tail is recovered as
        // empty on reload — fine, the events are now in the segment.
        let mut man_bytes = Vec::with_capacity(256);
        ciborium::into_writer(&self.manifest, &mut man_bytes)
            .map_err(|e| ChannelLogPersistError::CborEncode(e.to_string()))?;
        crate::owner_state_persist::save_atomically(&self.root.join("manifest.cbor"), &man_bytes)
            .map_err(|e| ChannelLogPersistError::Io(e.to_string()))?;
        self.tail.clear();
        self.flush_tail()?;
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
        let manifest: ChannelLogManifest = ciborium::from_reader(manifest_bytes.as_slice())
            .map_err(|e| ChannelLogPersistError::CborDecode(e.to_string()))?;
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
        // Count segment events. Segments themselves are read on demand
        // by the Phase 3 backfill code; reload doesn't materialize
        // them all into memory (could be megabytes per segment).
        let segment_count: usize = manifest.segments.iter().map(|s| s.count as usize).sum();
        let tail_path = root.join("tail.cbor");
        let tail: Vec<SignedChannelEvent> = if tail_path.exists() {
            let bytes = std::fs::read(&tail_path)?;
            ciborium::from_reader(bytes.as_slice())
                .map_err(|e| ChannelLogPersistError::CborDecode(e.to_string()))?
        } else {
            Vec::new()
        };
        let total = segment_count + tail.len();
        Ok((
            Self {
                manifest,
                tail,
                config,
                root,
            },
            total,
        ))
    }

    /// Read all events from a sealed segment. Used by Phase 3 backfill.
    /// Phase 2 ships this for tests (verify seal/reload byte-equality).
    pub fn read_segment(
        &self,
        descriptor: &SegmentDescriptor,
    ) -> Result<Vec<SignedChannelEvent>, ChannelLogPersistError> {
        let SegmentHandle::LocalFile { rel_path } = &descriptor.handle;
        let abs_path = self.root.join(rel_path);
        let bytes = std::fs::read(&abs_path)?;
        ciborium::from_reader(bytes.as_slice())
            .map_err(|e| ChannelLogPersistError::CborDecode(e.to_string()))
    }
}
```

- [ ] **Step 2: Add persistence tests inside `mod tests`**

```rust
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
            assert!(!log.append(event), "below threshold must not signal seal");
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
            assert!(!log.append(fixture_signed_event(100_000 + i, 0, "a-dev")));
        }
        assert!(
            log.append(fixture_signed_event(103_000, 0, "a-dev")),
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
            log.append(ev.clone());
        }
        log.seal_and_persist().expect("seal");
        // After seal: tail empty, manifest grew by one, segment file exists.
        assert!(log.tail.is_empty());
        assert_eq!(log.manifest.segments.len(), 1);
        assert!(root.join("segments/00000000.cbor").exists());
        assert!(root.join("manifest.cbor").exists());
        assert!(root.join("tail.cbor").exists());
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
            log.append(ev.clone());
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
        let mut log = ChannelLog::new(
            cid,
            chid,
            root.clone(),
            ChannelLogConfig::default(),
        );
        log.append(fixture_signed_event(100_000, 0, "a-dev"));
        log.flush_tail().expect("flush");
        log.seal_and_persist().expect("seal");
        let err =
            ChannelLog::reload(other, chid, root, ChannelLogConfig::default()).expect_err(
                "manifest community mismatch must reject",
            );
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
            log.append(fixture_signed_event(100_000 + i * 1000, 0, "a-dev"));
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
```

- [ ] **Step 3: Verify gates pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail && cargo fmt --all -- --check 2>&1 | tail -5 ; echo "FMT_EXIT=$?"
set -o pipefail && cargo clippy --all-targets -- -D warnings 2>&1 | tail -10 ; echo "CLIPPY_EXIT=$?"
set -o pipefail && cargo test -p harmony-app community_channel_log::tests::channel_log 2>&1 | tail -15
```
Expected: FMT 0, CLIPPY 0, eight `tests::channel_log_*` tests pass.

- [ ] **Step 4: Commit**

```bash
git -C /Users/zeblith/work/zeblithic/harmony-client add src-tauri/src/community_channel_log.rs
git -C /Users/zeblith/work/zeblithic/harmony-client commit -m "feat(zeb-269): segmented ChannelLog persistence (manifest + tail + seal/reload)"
```

---

## Task 7: Final verification + push + PR

- [ ] **Step 1: Full workspace verification**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail && cargo fmt --all -- --check 2>&1 | tail -5 ; echo "FMT_EXIT=$?"
set -o pipefail && cargo clippy --all-targets -- -D warnings 2>&1 | tail -10 ; echo "CLIPPY_EXIT=$?"
set -o pipefail && cargo test --workspace --no-fail-fast 2>&1 | grep -E "^test result:" | awk '{p+=$4; f+=$6; i+=$8} END {print "TOTAL passed="p" failed="f" ignored="i}'
```
Expected:
- FMT 0
- CLIPPY 0
- TOTAL passed = baseline (856) + this PR's tests (4 HKDF + 3 sign + 4 AEAD + 5 replay + 8 verify + 8 channel_log + 1 wire-format pin = 33) = 889. Adjust expected number if some tests share a name pattern; the floor is 856.

- [ ] **Step 2: Push branch + create PR**

```bash
git -C /Users/zeblith/work/zeblithic/harmony-client push -u origin zeb-248-phase2-channel-log
```

Then create the PR:
```bash
gh pr create --repo zeblithic/harmony-client --title "ZEB-248 Phase 2: ChannelLog data plane (in-process)" --body "$(cat <<'EOF'
## Summary

Phase 2 of ZEB-248 (Sub-C v2 — channels within communities). Ships the per-channel data plane (`SignedChannelEvent`, `ChannelKey` HKDF, ChaCha20-Poly1305 wire wrap, segmented `ChannelLog` persistence, `ChannelLogReplayTracker`, `verify_channel_event` chain) as a self-contained module at `src-tauri/src/community_channel_log.rs`. **In-process only** — no Zenoh wiring, no IPC surface, no engine. Phase 3 (next PR) will add `ChannelLogEngine` + transport + IPCs on top.

Closes ZEB-269.

## What's in this PR

- **`ChannelKey` + `derive_channel_key`** — HKDF-SHA256(IKM=MembershipKey, salt=community_id, info=`b"channel:" || channel_id`) → 32-byte key. Deterministic per (community, channel); rotates only on community-key rotation.
- **`SignedChannelEvent::Post`** + **`MessageId = [u8; 16]`** + **`sign_channel_event`** — wire format per spec §5.2. Same-length-keys invariant (`tg`/`vl` outer; all inner field keys 2-char). Signature covers canonical CBOR of (id, community_id, channel_id, author, at, content_kind, body, reply_to). v3 Edit/Delete/React variants reserved as comments.
- **`encrypt_channel_packet` / `decrypt_channel_packet`** — `[12B random nonce][ChaCha20-Poly1305(ChannelKey, plaintext = canonical_cbor(event), AAD = b"harmony-channel-msg-v1")]`. Per-packet random nonce; replay protection lives at the tracker layer.
- **`ChannelLogReplayTracker`** — `BTreeMap<(ChannelId, OwnerAddr, String /*device_id*/), Hlc>`. Strict-monotonicity check + bump per event. Independent lanes per device — Bob's later events on b-dev don't block Alice's later events on a-dev.
- **`verify_channel_event`** — §7 chain steps 3-7 against pre-decrypted event: misroute defense → identity resolution → signature verify → replay check → membership-at-HLC gate (write_power + tombstone evaluated AS OF event.at, not now). Returns typed `ChannelEventError` per step.
- **`ChannelLog`** — `manifest + tail + Vec<SignedChannelEvent>`, segmented persistence per §8. `append` (sync), `flush_tail` (atomic-rename via `save_atomically`), `seal_and_persist` (writes segment + grows manifest + clears tail), `reload` (replays manifest + tail; returns fresh empty log if no manifest). `ChannelLogConfig.seal_threshold_events` is constructor-injected (1024 production, 8 tests).
- **Canonical-CBOR pin** — `tests/wire_format_channel_log_fixtures.rs` pins one `SignedChannelEvent::Post` to a stable byte sequence. Wire-format drift fails this test loudly.
- **33 unit tests** + **1 wire-format pin** covering the full chain (HKDF determinism / distinctness, signature verify, AEAD round-trip + tamper rejection, replay tracker monotonicity + lane independence, verify chain happy path + 6 rejection types, persistence round-trip + reload + multi-seal manifest growth + community-mismatch rejection).

## What's NOT in this PR (deferred to Phase 3)

- `ChannelLogEngine` (drives Zenoh broadcast + queryable backfill).
- IPCs `post_channel_message` / `list_channel_messages` / `request_channel_backfill`.
- `channel-message-received` / `channel-backfill-progress` Tauri events.
- `ChannelLogRegistry` lifecycle (spawn-on-create, stop-on-delete).
- Debounced tail-flush task (engine-side scheduler).
- Frontend changes (channel sub-sidebar, message feed, compose box) — Phase 4.

## Plan-time decisions baked in (per spec §15)

1. **Tail flush is engine-side.** Phase 2 ships pure async I/O (`flush_tail`); the debounce timer that drives periodic flushing belongs to `ChannelLogEngine` (Phase 3). Keeps the data plane scheduler-free.
2. **AEAD layer split.** `verify_channel_event` operates on `SignedChannelEvent` (post-decryption); AEAD is a separate `decrypt_channel_packet` that runs first. Mirrors Phase 1's `verify_event` shape (which takes `SignedMembershipEvent`, not raw bytes). Means signatures verify the canonical-CBOR payload, not the wire bytes.
3. **Seal threshold as constructor field.** `ChannelLogConfig { seal_threshold_events: usize }`. Production passes 1024; tests pass 8. No `cfg(test)` coupling in production code.
4. **Replay tracker keyed on (channel, author, device_id).** Per-device lane isolation matches `CommunityRootHlcTracker`'s shape; per-author would conflate same-author multi-device posts as duplicates.

## Cross-references

- Parent spec: `docs/specs/2026-05-09-zeb-248-channels-within-communities-design.md` commit `5145484`
- Parent ticket: ZEB-248 (Sub-C v2 — channels within communities)
- Sub-ticket: ZEB-269 (this PR)
- Sibling Phase 1: ZEB-266, merged via PR #93 (`b67468f`)
- Sibling cross-cutting refactor: ZEB-267, merged via PR #94 (`a58aff9`)
- Predecessor: ZEB-217 (Sub-C v1 — communities substrate)

## Test plan

- [ ] `cargo fmt --all -- --check` — green
- [ ] `cargo clippy --all-targets -- -D warnings` — green
- [ ] `cargo test --workspace --no-fail-fast` — 856 baseline + ~33 new tests = 889
- [ ] CodeRabbit / Cursor Bugbot / Greptile (when invoked) review

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Capture the PR URL for reference. The PR will sit in pre-review state until all bots weigh in.

---

## Self-review notes

**Coverage check vs parent spec sections:**
- §5.2 SignedChannelEvent — Task 2 ✓
- §5.3 wire packet — Task 3 (AEAD with same nonce + AAD shape) ✓
- §6 ChannelKey HKDF — Task 1 ✓
- §7 verify_channel_event — Task 5 (steps 3-7; AEAD step 1 is Task 3, CBOR decode step 2 is part of decrypt_channel_packet) ✓
- §8 ChannelLog persistence — Task 6 ✓
- §13.1 Phase 2 unit tests — covered across Tasks 1-6 ✓

**Known plan-time uncertainties for the implementer:**
1. `insta` is NOT in dev-deps (verified during plan writing — `Cargo.toml` doesn't list it). Use the literal hex assertion fallback for the wire-format pin: capture the hex on first run via temporary `eprintln!`, then inline as `assert_eq!(hex, "...");`. Drop the eprintln before commit.
2. Phase 1 introduced `ChannelInfo` but the exact field names (`deleted_at` vs `tombstoned_at`, `write_power` vs `min_write_power`) are inferred from spec context — Task 5 Step 1 includes a `grep` to verify and adjusts the `verify_channel_event` body if needed. Task 5 Step 2 mock fixtures may need adjustment for the actual `ChannelInfo` constructor shape (`Default::default()` may not work — a manual literal with all fields may be required).

**Lock-order / async-discipline check:** None. This module is single-engine-state; no shared locks held across awaits. The `ChannelLogReplayTracker::check_and_advance` takes `&mut self` (not async) — caller must serialize externally; Phase 3's engine will own the tracker behind its own lock.

**No ZEB-267 violations:** This module doesn't reserve HLCs (events are minted with HLCs supplied externally); no tracker pattern that needs the ZEB-267 atomic-reservation discipline.
