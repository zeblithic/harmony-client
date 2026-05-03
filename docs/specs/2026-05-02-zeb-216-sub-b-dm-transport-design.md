# ZEB-216 Sub-B: harmony-client DM transport design

**Date:** 2026-05-02
**Linear:** [ZEB-216](https://linear.app/zeblith/issue/ZEB-216) (parent: [ZEB-206](https://linear.app/zeblith/issue/ZEB-206))
**Blocks:** ZEB-206 umbrella completion
**Blocked by:** None at design time. Implementation depends on ZEB-219 (DM content encryption design — Done) and ZEB-215 Sub-A (owner-state CRDT foundation — Done through Phase 3b).
**Companion specs:** [ZEB-219](2026-05-02-zeb-219-dm-content-encryption-design.md) defines the per-message encryption scheme; this spec consumes that contract and adds wire transport.

## Goal

Implement Reticulum unicast DM transport on top of the owner-state CRDT and ZEB-219 encryption primitives. A connected harmony-client should be able to send and receive direct messages and group-DMs (3-16 members) end-to-end, with bound-device store-and-forward semantics so that messages reach the recipient when *any* of their bound devices comes online.

## Scope

In scope:
- New Rust modules `dm_crypto.rs`, `dm_envelope.rs`, `dm_outbox.rs`
- Space struct extensions for per-DM-Space encryption keys (the in-code form of ZEB-219's data contract)
- Reticulum wire envelope (`DmPacket` with discriminant byte) for invite, message-CID-notify, and ack packets
- Outbox drain loop, exponential-backoff retry schedule, 30-day expiration
- Inbound demux, CAS-mediated blob fetch, sender-binding verification, InboxEntry write
- IPC `send_dm` command; `dm-received` and `dm-delivered` events
- NavService rendering of DM/group-DM Space kinds; at-17 conversion UX
- Companion harmony-runtime PR adding `RuntimeAction::SendUnicastToDevice` + `RuntimeEvent::UnicastReceived` (the only new transport primitive)

Out of scope (deferred):
- Voice/video in DMs — separate transport design
- Reactions on DMs — channel reactions ship via ZEB-32; DM parity deferred
- Read-receipts visible to peers — privacy default
- Group DM > 16 members — must convert to community
- Forward secrecy / content-key rotation under group-DM membership growth — ZEB-219 v1 acceptable to skip
- Per-device delivery lease in OutboxEntry — v1 tolerates duplicate sends across sender's devices

## Threat model

Same as ZEB-219 plus transport-layer concerns:
- **CAS observer**: encrypted blobs only; no plaintext leak even with full CID inventory
- **Reticulum link observer**: link-layer ECDH between device identities encrypts wire payloads; observer sees identity_hash → identity_hash + length, not contents
- **Sender impersonation across owners**: receive-time `verify_sender_binding` check blocks impersonation by comparing `MessagePayload.sender` against the link-origin OwnerAddr resolved from `from_identity_hash`
- **Sender impersonation across own devices**: per-owner CRDT replication boundary makes this structurally impossible — only the owner's own bound devices can write to their owner-state
- **OwnerDeviceCache poisoning via spoofed payload owner**: blocked by the link-origin binding rule — every cache mutation uses the owner resolved from `from_identity_hash`, never the payload-controlled owner field. Mismatched packets are dropped with telemetry.
- **Forged DmAck inflating `delivered_to`**: blocked by (a) link-origin binding (ack owner = link origin owner) AND (b) recipient-membership check (resolved owner must be in `OutboxEntry.recipient_owners`).
- **Stale device list**: piggyback refresh on every DM envelope keeps recipient's `OwnerDeviceCache` fresh; cache LWW on `learned_at` HLC
- **Bootstrap trust on first DmInvite**: receiver has no prior `OwnerDeviceCache` entry for the inviter, so cannot run link-origin binding. Trust comes from the out-of-band channel by which the inviter's `(owner_addr, device_identity_hash)` was shared (invite link, QR, library directory listing). UI MUST surface "Invite from <owner_addr>" with affirmative user acceptance before the cache is updated. Owner-key signature on DmInvite (UCAN-rooted via ZEB-173 device delegation) is stronger and a deferred future improvement.

### DmInvite rejection / decline semantics (v1)

When the user declines a `DmInvite`:
- **No persistent state is written.** Owner-state CRDT is untouched (no Space, no `OwnerDeviceCache` update), so no replication side-effects on the recipient's other bound devices.
- **No notification to the inviter.** This is privacy-preserving — the recipient does not reveal whether their device was online, whether they saw the invite, or whether they declined vs ignored. The inviter's `OutboxEntry` (if they sent a follow-up DmCidNotify) will simply expire at 30 days like any unack'd DM.
- **Repeat invites from the same OwnerAddr are re-prompted.** v1 has no per-OwnerAddr "ignored inviters" persistent set. If the user wants to durably block an OwnerAddr, that's a separate "block this peer" feature deferred to a follow-up ticket.
- **Implementation note.** The decline path is just "drop the DmInvite packet and emit no IPC event"; no special handling beyond not running the accept path.

If the receiver has already accepted a previous DmInvite from the same OwnerAddr (Space exists), a new DmInvite for the same `dedupe_key` is treated as a normal CRDT merge into the existing Space (per the dedupe-merge cap rule for `prior_content_keys`).

Out of scope:
- Master-seed compromise of any participant (ZEB-173 fresh-identity flow)
- Forward secrecy on past messages (ZEB-219 v1 trade-off)
- Forensic deniability (recipients can prove "Alice sent this" by retaining ciphertext + Alice's link-layer signature)

## Phase decomposition

Five-PR rollout. Each PR ships independently green; Phase 3a is a cross-repo companion in `~/work/zeblithic/harmony` that gates Phase 3b.

| Phase | Repo | Scope | Ships when done |
|---|---|---|---|
| 1 | harmony-client | DM encryption primitives in code: Space struct fields, validate_invariants, dedupe-merge for prior_content_keys, `dm_crypto.rs`, `dm_envelope.rs` types + canonical CBOR | ZEB-219 contract is implemented and tested; no transport, no IPC |
| 2 | harmony-client | `dm_outbox.rs` skeleton with stub transport; `send_dm` IPC writes encrypted blob to CAS + creates OutboxEntry; drain state machine with backoff + 30-day expiration | DM send works end-to-end against an in-memory transport mock |
| 3a | harmony-runtime | `RuntimeAction::SendUnicastToDevice { device_identity_hash, payload }` + `RuntimeEvent::UnicastReceived { from_identity_hash, payload }`. Resolves identity_hash → existing tunnel (or initiates) | Companion PR merged + tagged in upstream; harmony-client `[patch]` removed |
| 3b | harmony-client | Replace stub transport with real harmony-runtime addressed-pipe; wire `UnicastReceived` to inbound demux (CasOp::GetOrFetch + decrypt + sender-binding + InboxEntry); DmAck handling | DMs work end-to-end through real Reticulum transport (still mocked at the RuntimeAction-channel boundary in tests) |
| 4 | harmony-client + frontend | NavService DM/group-DM rendering; DmComposer/DmMessageList/DmCreateDialog Svelte components; `dm-received`/`dm-delivered` IPC event subscriptions; at-17 conversion UX | DMs work in the GUI; manual two-device LAN smoke deferred to follow-up Linear ticket |

## Architecture

```text
┌────────────────────────────────────────────────────────────────────┐
│ Frontend (Phase 4):                                                │
│   DmComposer, DmMessageList, DmCreateDialog, NavService            │
│   Listens for: dm-received, dm-delivered, nav-updated              │
├────────────────────────────────────────────────────────────────────┤
│ IPC commands (Phase 2 + 4):                                        │
│   send_dm(space_id, content, mime_type) -> MessageId               │
│   add_space(kind=dm|group-dm, members, ...) -> SpaceId             │
├────────────────────────────────────────────────────────────────────┤
│ harmony-client Rust (Phases 1 + 2 + 3b + 4):                       │
│                                                                    │
│   ┌────────────────┐   ┌────────────────┐   ┌──────────────────┐  │
│   │  dm_crypto.rs  │   │ dm_envelope.rs │   │   dm_outbox.rs   │  │
│   │  encrypt/      │   │  DmInvite,     │   │   drain loop,    │  │
│   │  decrypt,      │   │  DmCidNotify,  │   │   send_dm,       │  │
│   │  AAD,          │   │  DmAck,        │   │   handle_unicast,│  │
│   │  sender-bind   │   │  DmPacket      │   │   handle_ack,    │  │
│   └────────────────┘   └────────────────┘   │   30d expiration │  │
│            ▲                    ▲           └──────────────────┘  │
│            │                    │                    │            │
│   ┌────────┴────────────────────┴──────────────┐    │            │
│   │  owner_state_types.rs (Phase 1):           │    │            │
│   │    Space.content_key, prior_content_keys   │    │            │
│   │    OwnerDeviceCache                        │    │            │
│   │    MessagePayload (plaintext envelope)     │    │            │
│   └────────────────────────────────────────────┘    │            │
│                              ▲                       │            │
│                              │                       ▼            │
│   ┌──────────────────────────┴────────┐    ┌──────────────────┐  │
│   │ owner_state_crdt.rs (Phase 1):    │    │ event_loop.rs    │  │
│   │   apply_space + dedupe canonical- │    │   tick → drain   │  │
│   │   ization extended for prior_     │    │   UnicastReceived│  │
│   │   content_keys cap rule           │    │     → demux      │  │
│   │   apply_owner_device_update LWW   │    └──────────────────┘  │
│   └───────────────────────────────────┘             │            │
│                                                     │            │
│   ┌────────────────────────────────────────┐        │            │
│   │ Existing Phase 3b CAS infra:           │◄───────┘            │
│   │   CasOp::PutLocal (sender writes blob) │                     │
│   │   CasOp::GetOrFetch (recipient fetches)│                     │
│   └────────────────────────────────────────┘                     │
├────────────────────────────────────────────────────────────────────┤
│ harmony-runtime (Phase 3a — companion PR):                         │
│   RuntimeAction::SendUnicastToDevice{device_identity_hash,payload} │
│   RuntimeEvent::UnicastReceived{from_identity_hash, payload}       │
│   Resolves identity_hash → tunnel (existing iroh-net + Reticulum)  │
└────────────────────────────────────────────────────────────────────┘
```

## Data model

### Wire-format newtypes (Phase 1)

Per existing repo convention (see `owner_state_types.rs:17-23` and the bstr-helpers around `SpaceId` / `OwnerAddr` / `OutboxEntryId`), all fixed-size byte identifiers that go on the wire MUST be wrapped in newtypes carrying `serialize_bytes_as_bstr` / `deserialize_bytes_from_bstr` serde attributes. A bare `[u8; N]` would serialize as a CBOR array-of-u8 (major type 4) — roughly 2× the size of bstr (major type 2) for random bytes — which defeats the MTU-conscious wire-format goals of this spec.

Two new wire newtypes for Phase 1:

```rust
use zeroize::{Zeroize, ZeroizeOnDrop};

/// 32-byte symmetric content key for DM/group-DM ChaCha20-Poly1305 encryption.
/// Wire format: bstr(32). In-memory: zeroized on drop.
///
/// Custom Drop ensures key bytes are wiped from memory before the allocation
/// is freed (the same property `Zeroizing<[u8; 32]>` provides), and the bstr
/// serde helpers force compact wire encoding.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, ZeroizeOnDrop)]
pub struct DmContentKey(
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr",
    )]
    [u8; 32],
);

impl DmContentKey {
    pub fn new(key: [u8; 32]) -> Self { Self(key) }
    pub fn as_bytes(&self) -> &[u8; 32] { &self.0 }
    pub fn random() -> Self {
        let mut k = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut k);
        Self(k)
    }
}

// Manual Debug to avoid leaking key material to logs.
impl std::fmt::Debug for DmContentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DmContentKey(<32 bytes redacted>)")
    }
}

/// 16-byte Reticulum device-identity hash (matches Reticulum
/// `IDENTITY_TRUNCATED_HASH_LENGTH = 16`). Wire format: bstr(16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DeviceIdentityHash(
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr",
    )]
    pub [u8; 16],
);
```

Both types must round-trip through CBOR as bstr (regression tests required, mirroring the existing `space_id_serializes_as_bstr` patterns in `owner_state_types.rs`).

### Space struct additions (Phase 1)

```rust
pub struct Space {
    // ... existing fields ...

    /// Per-DM-Space symmetric content key (ChaCha20-Poly1305).
    /// MUST be Some for kind ∈ {dm, group-dm}; MUST be None otherwise.
    /// Wire format: bstr(32) inside the Space CBOR map under key "ck".
    /// In-memory: zeroized on drop via DmContentKey's ZeroizeOnDrop impl.
    #[serde(rename = "ck", skip_serializing_if = "Option::is_none", default)]
    pub content_key: Option<DmContentKey>,

    /// Historical content keys retained from past dedupe-collision merges.
    /// Used as fallback decryption for messages encrypted under a now-
    /// superseded key. Bounded by MAX_PRIOR_CONTENT_KEYS = 16.
    /// MUST NOT contain the current `content_key`.
    /// MUST be empty for non-DM kinds.
    /// Wire format: array of bstr(32) under key "pk".
    #[serde(rename = "pk", skip_serializing_if = "Vec::is_empty", default)]
    pub prior_content_keys: Vec<DmContentKey>,
}

pub const MAX_PRIOR_CONTENT_KEYS: usize = 16;
```

`serde(default)` on both fields ensures non-DM Spaces deserialize cleanly even when the map omits these keys.

### OwnerDeviceCache (Phase 1)

A new collection on `OwnerStateView` tracking each known peer OwnerAddr's bound-device list, replicated across the user's bound devices via Flow A:

```rust
pub struct OwnerDeviceCache {
    pub devices: BTreeMap<OwnerAddr, OwnerDeviceEntry>,
}

pub struct OwnerDeviceEntry {
    pub devices: Vec<DeviceIdentityHash>,  // sorted lex
    pub learned_at: Hlc,                   // LWW key
}
```

`DeviceIdentityHash` is the bstr(16) newtype defined above.

Apply rule (LWW on `learned_at`, with mandatory dedupe + cap to prevent cache-growth DoS):

```rust
/// Maximum number of device identities retained per OwnerAddr. Bounds the
/// memory cost of OwnerDeviceCache and the Reticulum-MTU cost of any
/// piggybacked sender_devices lists. Chosen to comfortably exceed
/// "one user with many bound devices" (~12-20 today) while staying
/// well under levels that would meaningfully bloat envelopes.
pub const MAX_DEVICES_PER_OWNER: usize = 32;

pub fn apply_owner_device_update(
    cache: &mut OwnerDeviceCache,
    addr: OwnerAddr,
    devices: Vec<DeviceIdentityHash>,
    learned_at: Hlc,
) -> ApplyOutcome {
    match cache.devices.get(&addr) {
        Some(existing) if existing.learned_at >= learned_at => ApplyOutcome::NoOp,
        _ => {
            let mut sanitized = devices;
            sanitized.sort();                            // ascending lex
            sanitized.dedup();                           // drop repeated hashes
            sanitized.truncate(MAX_DEVICES_PER_OWNER);   // bound size
            cache.devices.insert(
                addr,
                OwnerDeviceEntry { devices: sanitized, learned_at },
            );
            ApplyOutcome::Applied
        }
    }
}
```

Storage cost bound: 16 bytes (OwnerAddr) + 16 × min(K, 32) + ~24 bytes (HLC) per peer. At 1000 DM peers × 32 devices each ≈ 560KB. Still trivial.

### Plaintext envelope (Phase 1, recap from ZEB-219)

```rust
pub struct MessagePayload {
    #[serde(rename = "bd")] pub body: Vec<u8>,
    #[serde(rename = "mt")] pub mime_type: String,
    #[serde(rename = "se")] pub sender: OwnerAddr,
    #[serde(rename = "sa")] pub sent_at: Hlc,
}
```

This is what's ChaCha20-Poly1305-encrypted into the storage_blob written to CAS. AAD = `canonical_cbor_encode(space.dedupe_key())`.

`storage_blob = version_byte(1) || nonce_12(12) || ciphertext(N) || poly1305_tag(16)` = `N + 29` bytes. Length-gate before slicing: reject `storage_blob.len() < 29`.

## Wire format

Reticulum unicast carries one of three packet types. Wire layout: `[u8 discriminant][CBOR-encoded body]`. This mirrors ZEB-219's per-message version-byte pattern (consistent style across the same feature) and lets future packet types add new discriminants without bumping a top-level CBOR schema.

| Disc | Type | Direction | Purpose |
|---|---|---|---|
| `0x01` | `DmInvite` | inviter → invitee | Space creation: members, content_key, sender's bound-device list |
| `0x02` | `DmCidNotify` | sender → recipient devices | New message exists in CAS at this CID; piggybacks sender's current device list |
| `0x03` | `DmAck` | recipient → sender devices | Receipt confirmation for `(space_id, message_cid)`; piggybacks recipient's current device list |

```rust
pub struct DmInvite {
    #[serde(rename = "si")] pub space_id: SpaceId,
    #[serde(rename = "kn")] pub kind: SpaceKind,            // dm or group-dm
    /// Members of the Space — sorted ascending lex (matches Space::members
    /// invariant for canonical CBOR determinism). Includes the inviter.
    /// CANNOT be used to identify the inviter — `members[0]` is the
    /// lex-smallest OwnerAddr, not the sender.
    #[serde(rename = "me")] pub members: Vec<OwnerAddr>,
    /// Inviter's OwnerAddr — the sender of this DmInvite. Bootstrap-trusted
    /// at first contact (cache has no entry yet, so link-origin binding
    /// can't run). Receiver MUST verify `inviter ∈ members` and
    /// `sender_devices.contains(from_identity_hash)` as sanity gates,
    /// then prompt the user before applying any state mutations.
    #[serde(rename = "iv")] pub inviter: OwnerAddr,
    #[serde(rename = "ck")] pub content_key: DmContentKey,  // bstr(32) on wire, zeroized in memory
    #[serde(rename = "sd")] pub sender_devices: Vec<DeviceIdentityHash>,
    #[serde(rename = "ca")] pub created_at: Hlc,
}

pub struct DmCidNotify {
    #[serde(rename = "si")] pub space_id: SpaceId,
    #[serde(rename = "mc")] pub message_cid: ContentId,
    /// Diagnostic only — receiver MUST verify this matches the owner
    /// resolved from the inbound `from_identity_hash` via OwnerDeviceCache,
    /// and drop the packet on mismatch. Authoritative sender identity
    /// comes from the link origin, not this field.
    #[serde(rename = "so")] pub sender_owner_addr: OwnerAddr,
    #[serde(rename = "sd")] pub sender_devices: Vec<DeviceIdentityHash>,
}

pub struct DmAck {
    #[serde(rename = "si")] pub space_id: SpaceId,
    #[serde(rename = "mc")] pub message_cid: ContentId,
    /// Diagnostic only — receiver MUST verify this matches the owner
    /// resolved from the inbound `from_identity_hash`, AND that the
    /// resolved owner is listed in the OutboxEntry's `recipient_owners`.
    /// Drop the packet on either mismatch.
    #[serde(rename = "ao")] pub ack_from_owner_addr: OwnerAddr,
    #[serde(rename = "ad")] pub ack_from_devices: Vec<DeviceIdentityHash>,
}
```

### Link-origin binding rule (load-bearing, applies to DmCidNotify and DmAck)

Both DmCidNotify and DmAck carry payload-controlled owner fields (`sender_owner_addr` / `ack_from_owner_addr`). Without binding to the Reticulum link origin, an attacker on identity_hash *X* could forge a DmCidNotify claiming `sender_owner_addr = Alice` and overwrite Alice's `OwnerDeviceCache` entry with the attacker's devices, or forge a DmAck marking the attacker as a "delivered" recipient on someone else's OutboxEntry.

The receive-side rule (Phase 3b):

```rust
/// Resolve link-origin device → owner. MUST match exactly one OwnerAddr.
///
/// Returns Err on zero matches (UnknownLinkOrigin) or multiple matches
/// (AmbiguousLinkOrigin). Multi-match is reachable via corrupted state
/// or a malicious cache-poisoning DmInvite that claimed an existing
/// device hash for a different owner; either way the resolution is not
/// trustworthy — drop + telemetry.
fn resolve_link_origin_owner(
    cache: &OwnerDeviceCache,
    from_identity_hash: DeviceIdentityHash,
) -> Result<OwnerAddr, DmReceiveError> {
    let matches: Vec<OwnerAddr> = cache.devices.iter()
        .filter(|(_, entry)| entry.devices.binary_search(&from_identity_hash).is_ok())
        .map(|(addr, _)| *addr)
        .collect();
    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(DmReceiveError::UnknownLinkOrigin),
        _ => Err(DmReceiveError::AmbiguousLinkOrigin),
    }
}

// For DmCidNotify and DmAck:
let resolved_owner = resolve_link_origin_owner(cache, from_identity_hash)?;  // drop + telemetry on Err
if payload_owner_field != resolved_owner {
    return Err(DmReceiveError::OwnerFieldMismatch);  // drop + telemetry
}
// Additional check for DmAck:
if matches!(packet, DmPacket::Ack(_)) && !outbox_entry.recipient_owners.contains(&resolved_owner) {
    return Err(DmReceiveError::AckFromNonRecipient);  // drop + telemetry
}
// All authoritative state mutations (apply_owner_device_update, apply_outbox)
// use `resolved_owner`, never the payload field.
```

DmInvite is the bootstrap exception: at first contact the receiver doesn't yet have the inviter in `OwnerDeviceCache`, so `resolve_link_origin_owner` returns `Err(UnknownLinkOrigin)`. The Invite teaches the cache the `(invite.inviter, invite.sender_devices)` mapping after the user accepts. Sanity gates that MUST run before the user prompt:

```rust
// 1. inviter must be one of the Space members (otherwise the invite is
//    structurally inconsistent — a non-member can't invite themselves).
if !invite.members.contains(&invite.inviter) {
    return Err(DmReceiveError::InviterNotInMembers);  // drop + telemetry
}
// 2. sender_devices must include the device that actually sent the packet
//    (the inviter is claiming "these are my devices" — that list MUST
//    include the device transmitting the claim).
if invite.sender_devices.binary_search(&from_identity_hash).is_err() {
    return Err(DmReceiveError::SenderDeviceNotInSenderDevices);  // drop + telemetry
}
// 3. Receiver's own OwnerAddr must be in members (otherwise this invite
//    isn't for us — likely a misroute or a probe).
if !invite.members.contains(&self_owner_addr) {
    return Err(DmReceiveError::ReceiverNotInMembers);  // drop + telemetry
}
// 4. UI prompt: "Invite from {invite.inviter} to {invite.kind}, accept?"
//    On accept:
//      apply_owner_device_update(
//          invite.inviter,                  // NOT members[0]
//          invite.sender_devices.clone(),
//          invite.created_at,
//      );
//      apply_space(Space {
//          content_key: Some(invite.content_key.clone()),
//          ... fields from invite ...
//      });
```

Note that `invite.members` is sorted ascending (matching `Space::members` invariants for canonical CBOR determinism), so `invite.members[0]` is the lex-smallest OwnerAddr — **NOT** the inviter. Always use `invite.inviter` for owner-binding decisions. Bootstrap trust comes from the out-of-band channel (invite link / QR / library directory) — see Threat model. After Invite acceptance, all subsequent DmCidNotify / DmAck from that owner's devices are validated through link-origin binding.

Field renames keep CBOR small on Reticulum's MTU-constrained link (~500 bytes effective payload on LoRa interfaces). Decode pseudocode:

```rust
pub fn decode_packet(bytes: &[u8]) -> Result<DmPacket, DecodeError> {
    let (disc, body) = bytes.split_first().ok_or(DecodeError::Empty)?;
    match disc {
        0x01 => Ok(DmPacket::Invite(from_reader(body)?)),
        0x02 => Ok(DmPacket::CidNotify(from_reader(body)?)),
        0x03 => Ok(DmPacket::Ack(from_reader(body)?)),
        other => Err(DecodeError::UnknownDiscriminant(*other)),
    }
}
```

## Encryption helpers (Phase 1)

```rust
pub fn encrypt_dm_message(
    content_key: &DmContentKey,
    aad: &[u8],
    payload: &MessagePayload,
) -> Result<Vec<u8>, DmEncryptError> {
    let plaintext = canonical_cbor_encode(payload)?;
    let nonce: [u8; 12] = OsRng.gen();
    let cipher = ChaCha20Poly1305::new(content_key.as_bytes().into());
    let mut ciphertext_with_tag = cipher.encrypt(&nonce.into(), Payload { msg: &plaintext, aad })
        .map_err(|_| DmEncryptError::AeadFailure)?;
    let mut blob = Vec::with_capacity(1 + 12 + ciphertext_with_tag.len());
    blob.push(0x01);                       // version byte
    blob.extend_from_slice(&nonce);
    blob.append(&mut ciphertext_with_tag); // includes 16-byte poly1305 tag
    Ok(blob)
}

pub fn decrypt_dm_message(
    content_key: &DmContentKey,
    prior_content_keys: &[DmContentKey],
    aad: &[u8],
    storage_blob: &[u8],
) -> Result<MessagePayload, DmDecryptError> {
    if storage_blob.len() < 29 {
        return Err(DmDecryptError::TruncatedBlob);
    }
    let version = storage_blob[0];
    let (nonce_slice, ciphertext_slice) = match version {
        0x01 => (&storage_blob[1..13], &storage_blob[13..]),
        // 0x02 reserved (XChaCha20-Poly1305 with 24-byte nonce)
        other => return Err(DmDecryptError::UnknownVersion(other)),
    };
    let nonce: [u8; 12] = nonce_slice.try_into().unwrap();

    // Try current key first, then prior_content_keys in stored order.
    for key in std::iter::once(content_key).chain(prior_content_keys.iter()) {
        let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
        if let Ok(plaintext) = cipher.decrypt(
            &nonce.into(),
            Payload { msg: ciphertext_slice, aad },
        ) {
            return canonical_cbor_decode(&plaintext)
                .map_err(DmDecryptError::PayloadDecode);
        }
    }
    Err(DmDecryptError::AeadFailureAllKeys)
}

pub fn verify_sender_binding(
    payload: &MessagePayload,
    link_origin: OwnerAddr,
) -> Result<(), DmReceiveError> {
    if payload.sender != link_origin {
        return Err(DmReceiveError::SenderImpersonation);
    }
    Ok(())
}

pub fn compute_aad(space: &Space) -> Vec<u8> {
    canonical_cbor_encode(&space.dedupe_key())
        .expect("dedupe_key always serializes")
}
```

## Validate invariants extension (Phase 1)

```rust
impl Space {
    pub fn validate_invariants(&self) -> Result<(), InvariantError> {
        // ... existing checks ...

        match self.kind {
            SpaceKind::Dm | SpaceKind::GroupDm => {
                if self.content_key.is_none() {
                    return Err(InvariantError::DmRequiresContentKey);
                }
            }
            _ => {
                if self.content_key.is_some() {
                    return Err(InvariantError::NonDmHasContentKey);
                }
                if !self.prior_content_keys.is_empty() {
                    return Err(InvariantError::NonDmHasPriorContentKeys);
                }
            }
        }

        if self.prior_content_keys.len() > MAX_PRIOR_CONTENT_KEYS {
            return Err(InvariantError::PriorContentKeysCapExceeded);
        }

        if let Some(ck) = &self.content_key {
            if self.prior_content_keys.iter().any(|p| p.as_bytes() == ck.as_bytes()) {
                return Err(InvariantError::ContentKeyInPriorList);
            }
        }

        Ok(())
    }
}
```

## Dedupe-merge cap rule (Phase 1)

When two devices independently create the same DM (different ULIDs, different content_keys), `apply_space` deduplicates via `dedupe_key`. The merge:

1. Winner = lex-smaller ULID.
2. Loser's `content_key` is added to the merged `prior_content_keys` set, joined with both sides' existing `prior_content_keys`.
3. Filter the current winner `content_key` out of `prior_content_keys`.
4. Sort the filtered set lexicographically by raw 32-byte key value, dedup, take the first `MAX_PRIOR_CONTENT_KEYS` entries.

This rule is path-independent (multi-merge convergent). The 5-Space convergence test from ZEB-219 is the canonical regression case.

```rust
fn merge_prior_content_keys(
    winner_current: &DmContentKey,
    winner_prior: &[DmContentKey],
    loser_current: &DmContentKey,
    loser_prior: &[DmContentKey],
) -> Vec<DmContentKey> {
    let mut all: Vec<DmContentKey> = winner_prior.iter().cloned()
        .chain(std::iter::once(loser_current.clone()))
        .chain(loser_prior.iter().cloned())
        .filter(|k| k.as_bytes() != winner_current.as_bytes())
        .collect();
    all.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    all.dedup_by(|a, b| a.as_bytes() == b.as_bytes());
    all.truncate(MAX_PRIOR_CONTENT_KEYS);
    all
}
```

## Module structure

### New Rust modules (`src-tauri/src/`)

| Module | Phase | Responsibility |
|---|---|---|
| `dm_crypto.rs` | 1 | `encrypt_dm_message`, `decrypt_dm_message`, `verify_sender_binding`, `compute_aad`. Pure functions over key + AAD + bytes — no I/O, no state. Error types: `DmEncryptError`, `DmDecryptError`, `DmReceiveError`. |
| `dm_envelope.rs` | 1 | `MessagePayload` (plaintext envelope) + canonical CBOR. `DmInvite`, `DmCidNotify`, `DmAck`, `DmPacket` discriminated wire-format encode/decode (`encode_packet`, `decode_packet`). |
| `dm_outbox.rs` | 2 (skeleton) → 3b (real transport) | `DmTransport` trait (Phase 2 stub, Phase 3b real). `send_dm` orchestrator. `drain` (called from event-loop tick) state machine + backoff scheduler + 30-day expiration. `handle_unicast` for inbound DmPacket demux. `handle_ack` for OutboxEntry update. |

### Modified Rust files

| File | Phase | Change |
|---|---|---|
| `owner_state_types.rs` | 1 | Add `Space.content_key`, `Space.prior_content_keys` (with `serde(default)`). Add `MAX_PRIOR_CONTENT_KEYS = 16`. Add `OwnerDeviceCache`, `OwnerDeviceEntry`, `DeviceIdentityHash`. |
| `owner_state_crdt.rs` | 1 | Extend `Space::validate_invariants` for content_key MUST/MUST-NOT rules. Extend `apply_space` and the dedupe-canonicalization path to merge `prior_content_keys` per the cap rule. Add `apply_owner_device_update` (LWW on `learned_at`). |
| `owner_state_persist.rs` | 1 | Persistence round-trip for new Space fields and OwnerDeviceCache. (Should be free with serde — verify via round-trip tests.) |
| `event_loop.rs` | 2 (stub), 3b (real) | Tick arm calls `dm_outbox::drain(&mut state, &transport, now)`. Phase 3b: new select arm for `RuntimeEvent::UnicastReceived` → `dm_outbox::handle_unicast(...)`. RuntimeAction channel push for `SendUnicastToDevice`. |
| `lib.rs` | 2 (send_dm), 4 (DM kinds in add_space) | Phase 2: register `send_dm` IPC. Phase 4: extend `add_space` to handle DM/group-DM kinds with content_key generation + DmInvite distribution. Wire IPC events `dm-received`, `dm-delivered`. |

### File-size impact estimate

| File | Current | After Phase 1-4 |
|---|---|---|
| `owner_state_types.rs` | 1280 | ~1450 |
| `owner_state_crdt.rs` | already significant | +~120 |
| `event_loop.rs` | 1780 | +~80 |
| `lib.rs` | 4031 | +~150 |
| `dm_crypto.rs` | 0 | ~150 |
| `dm_envelope.rs` | 0 | ~250 |
| `dm_outbox.rs` | 0 | ~400 |

If `event_loop.rs` crosses ~2000 lines during this work, file a follow-up to extract domain-specific arms into separate modules. Not blocking.

## IPC surface

```rust
// Phase 2
//
// `MessageId` is a type alias for `OutboxEntryId` — the IPC surface uses the
// "MessageId" name for the frontend (matches ZEB-206 spec) but the underlying
// value is the OutboxEntryId that locates the just-created OutboxEntry.
pub type MessageId = OutboxEntryId;

#[tauri::command]
async fn send_dm(
    app: AppHandle,
    space_id: SpaceId,
    content: Vec<u8>,
    mime_type: String,
) -> Result<MessageId, String>;
// Validates Space exists and kind ∈ {dm, group-dm}.
// Encrypts content + envelope, writes blob to CAS via CasOp::PutLocal,
// creates OutboxEntry. Returns MessageId immediately.
// Drain loop handles delivery asynchronously.

// Phase 4 (extends existing add_space)
#[tauri::command]
async fn add_space(
    app: AppHandle,
    kind: SpaceKind,
    name: String,
    parent: Option<SpaceId>,
    members: Option<Vec<OwnerAddr>>,
    transport: Option<TransportBinding>,
) -> Result<SpaceId, String>;
// Phase 4 adds DM/group-DM handling: generates content_key,
// builds DmInvite, sends to each non-self member's known devices.
```

### IPC events (push to frontend)

| Event | Phase | Trigger | Payload |
|---|---|---|---|
| `dm-received` | 4 | After CAS-fetch + decrypt + sender-binding check + InboxEntry write succeed | `{ space_id, message_cid, from, sent_at, body, mime_type }` |
| `dm-delivered` | 4 | When OutboxEntry.delivered_to gains a recipient (per-recipient, deduped) | `{ space_id, message_cid, recipient_owner_addr }` |
| `nav-updated` | already shipped | Any Space CRDT change including new DM Spaces | (existing payload) |

## Frontend changes (Phase 4, high-level)

- `src/lib/nav-service.ts` — render DM/group-DM Spaces with member display
- New `DmComposer.svelte` — message input + send for DM Spaces
- New `DmMessageList.svelte` — renders InboxEntry ∪ OutboxEntry sorted by HLC for a given DM Space
- New `DmCreateDialog.svelte` — pick members from contacts; warns at-16 cap; blocks at-17 with conversion prompt
- Subscribe to `dm-received` / `dm-delivered`; refresh DmMessageList accordingly

## Flow walkthroughs

### Flow 1 — DM creation

```text
Alice device A1 (initiator)              Bob device B1 (recipient)
─────────────────────────                ──────────────────────────
1. UI: "Start DM with Bob"
2. IPC add_space(kind='dm',
     members=[Alice, Bob])
3. Generate content_key (32 random bytes,
   wrap in Zeroizing).
4. Create Space {id: ULID, kind: 'dm',
     members, content_key: Some(ck),
     prior_content_keys: vec![],
     transport: Reticulum, ...}
5. apply_space() locally → owner-state CRDT.
6. Build DmInvite {space_id, kind, members
     (sorted ascending lex), inviter:
     Alice_addr (explicit field — NOT
     members[0], which is the lex-smallest
     OwnerAddr and may not be the inviter),
     content_key: ck, sender_devices
     (Alice's bound devices, MUST include
     the device about to send), created_at:
     HLC now}
7. For each Bob device hash known
   (via OwnerDeviceCache or Reticulum
   path discovery on first contact):
   SendUnicastToDevice(bob_dev_hash,
     [0x01]||cbor(invite))
                                          8. UnicastReceived(from=A1,
                                             payload=[0x01]||cbor(...))
                                          9. parse disc 0x01 → DmInvite
                                          10. validate kind ∈ {dm, group-dm};
                                              members.len() matches kind
                                          10a. sanity gates (drop on any fail):
                                              - invite.inviter ∈ invite.members
                                                (InviterNotInMembers)
                                              - from_identity_hash ∈
                                                invite.sender_devices
                                                (SenderDeviceNotInSenderDevices)
                                              - self_owner_addr ∈
                                                invite.members
                                                (ReceiverNotInMembers)
                                          10b. UI prompt: "Invite from
                                              {invite.inviter} to start a
                                              {kind}, accept?" On decline:
                                              silent drop, no state mutation,
                                              no inviter notification.
                                          11. on accept: apply_space(Space {
                                                id: invite.space_id, kind,
                                                members,
                                                content_key: Some(
                                                  invite.content_key.clone()),
                                                ...}) → CRDT dedupe handles
                                                "Alice and I both created"
                                          12. apply_owner_device_update(
                                                invite.inviter (NOT
                                                members[0]),
                                                invite.sender_devices,
                                                invite.created_at)
                                          13. emit IPC nav-updated
                                          14. Owner-state Flow A replicates
                                              new Space + content_key to Bob's
                                              other devices (B2, B3, ...). Each
                                              of those devices learns the
                                              (inviter → devices) mapping via
                                              the same Flow A propagation of
                                              the OwnerDeviceCache update.
```

**Edge: dedupe collision.** If A1's invite races A2's independent creation, CRDT dedupe via `dedupe_key = sorted_members` merges them. Loser's content_key rolls into winner's `prior_content_keys` per the cap rule.

### Flow 2 — Send message (online recipient)

```text
Alice A1 (sender)                         Bob B1 (recipient)
─────────────────                         ─────────────────
1. UI: "Hello Bob" → send
2. IPC send_dm(space_id, "Hello Bob"
   bytes, "text/plain")
3. dm_outbox::send_dm():
   - lookup Space, assert kind ∈ DM kinds
   - build MessagePayload {body, mime_type,
     sender: Alice_addr, sent_at: HLC now}
   - encrypt: ck + AAD (= cbor(dedupe_key))
     + random nonce → storage_blob
   - CAS write: CasOp::PutLocal(storage_blob)
     → message_cid (BLAKE3 over storage_blob)
   - derive recipient_owners from
     Space.members:
       1. exclude sender's own OwnerAddr
       2. dedupe (set semantics)
       3. sort lex (deterministic order
          for equality checks across
          sender's bound devices)
     For 1:1 DM: recipient_owners = [Bob].
     For group-DM with members
     [Alice, Bob, Carol, Carol]: result is
     [Bob, Carol] (sorted, deduped).
   - create OutboxEntry {id: ULID, space_id,
     message_cid, recipient_owners (derived
     above), delivered_to: [],
     status: 'pending', created_at: HLC now}
   - apply_outbox() locally → CRDT
   - returns MessageId to UI immediately
4. Owner-state Flow A: A2, A3, ... see
   the new OutboxEntry.
5. Drain tick (every 250ms):
   - walks outbox where status ∈ {pending, partial}
   - first attempt for new entry: backoff(0)=0
     so attempts immediately
   - resolves Bob's device_hashes from
     OwnerDeviceCache → [B1, B2, B3]
   - for each unack'd device, build
     DmCidNotify{space_id, message_cid,
     sender_owner_addr=Alice, sender_devices}
   - SendUnicastToDevice(B1, [0x02]||cbor(...))
     SendUnicastToDevice(B2, [0x02]||cbor(...))
     SendUnicastToDevice(B3, [0x02]||cbor(...))
                                          6. (B1) UnicastReceived(from=A1,
                                             payload=[0x02]||cbor(...))
                                          7. parse → DmCidNotify
                                          7a. resolve_link_origin_owner(
                                              cache, from_identity_hash=A1)
                                              → Some(Alice). If None: drop
                                              packet (unknown identity).
                                          7b. verify notify.sender_owner_addr
                                              == Alice. If mismatch: drop
                                              + telemetry (cache-poisoning
                                              attempt).
                                          8. apply_owner_device_update(
                                              Alice (resolved, NOT payload
                                              field), notify.sender_devices,
                                              HLC now)
                                          9. CasOp::GetOrFetch(message_cid):
                                             - local CAS miss
                                             - DAG-sync fetch from Alice's CAS
                                               via Zenoh (Phase 3b infra)
                                             - 500ms timeout, fallback retry
                                          10. on fetch success: storage_blob bytes
                                          11. decrypt_dm_message(ck (+ prior), AAD,
                                              storage_blob) → MessagePayload
                                          12. verify_sender_binding(payload.sender
                                              == Alice_addr from link_origin lookup)
                                          13. if ok:
                                              - outcome = apply_inbox(InboxEntry
                                                {space_id, message_cid, from:
                                                Alice, received_at: HLC})
                                              - if outcome == ApplyOutcome::Applied:
                                                  emit IPC dm-received
                                                (NoOp = duplicate, no IPC emit)
                                              - For each device in
                                                notify.sender_devices (fan out
                                                ack to ALL sender devices, not
                                                just A1):
                                                  SendUnicastToDevice(device,
                                                    [0x03]||cbor(DmAck{...}))
                                                Cost: K_sender_devices unicast
                                                sends per ack (~12 × 120 bytes
                                                = 1.4KB). Liveness benefit:
                                                ack reaches sender even if A1
                                                went offline between notify
                                                and ack. Failed sends are
                                                silent (no retry on the ack
                                                itself; sender's drain will
                                                retry the original notify if
                                                no ack lands anywhere).
                                          14. Owner-state Flow A: B2, B3 see the
                                              new InboxEntry. They each
                                              CasOp::GetOrFetch(message_cid),
                                              decrypt locally, render. Their
                                              local apply_inbox returns NoOp
                                              (entry already present from
                                              Flow A merge or their own
                                              direct receive), so no duplicate
                                              dm-received. Each may also send
                                              its own DmAck fan-out (sender's
                                              drain treats duplicate acks as
                                              idempotent).
15. UnicastReceived(from=B1,
    payload=[0x03]||cbor(DmAck))
16. dm_outbox::handle_ack:
    - resolve_link_origin_owner(cache,
      from_identity_hash=B1) → Some(Bob).
      If None: drop (unknown identity).
    - verify ack.ack_from_owner_addr ==
      Bob. If mismatch: drop + telemetry
      (impersonated ack).
    - lookup OutboxEntry by (space_id,
      message_cid). Verify Bob ∈ entry
      .recipient_owners. If not: drop +
      telemetry (ack from non-recipient).
    - apply_owner_device_update(Bob
      (resolved), ack.ack_from_devices,
      HLC now)
    - apply_outbox: outbox[id].delivered_to
      .insert(Bob); recompute status →
      'complete' (only one recipient)
    - emit IPC dm-delivered
```

### Flow 3 — Send message (offline recipient)

```text
Alice A1                                  Bob (all devices offline)
────────                                  ─────────────────────────
1. send_dm() — same path as Flow 2 steps 1-3
   (encrypt, CAS write, OutboxEntry created).
2. Drain tick: SendUnicastToDevice fails for
   every Bob device (no Reticulum tunnel
   resolves). Returns error to drain.
3. Drain marks per-device retry timestamp,
   leaves OutboxEntry as 'pending'.
4. (Some of Alice's devices may also be
   online and run their own drain — same
   OutboxEntry is in their CRDT view too.
   Idempotency: per-device in-flight set
   prevents duplicate sends within a single
   drain tick. Cross-device duplicate sends
   are tolerated because recipient's
   apply_inbox is idempotent and sender's
   apply_outbox merges delivered_to via
   union.)
5. Time passes. Each drain tick re-evaluates
   per-entry exponential backoff (5s base,
   2× mult, 5min cap, ±20% jitter). Attempts
   continue until delivery or 30-day expiration.
                                          6. Bob's B1 comes online (eventually).
7. Reticulum path discovery / native announce
   resurfaces B1 as reachable (harmony-runtime
   native; harmony-client doesn't drive it).
8. Next drain tick: SendUnicastToDevice
   succeeds. Same path as Flow 2 from step 5.
   B1 receives, fetches, decrypts, acks. Owner-
   state Flow A propagates InboxEntry to B2, B3
   as they also come online.
```

### Flow 4 — 30-day expiration

```text
Alice A1
────────
N+1th drain tick (where N covers 30 days):
  for entry in outbox where status ∈ {pending, partial}:
    // Boundary: at-or-after 30 days (>=, not strictly >).
    // Picked >= so the 30-day mark is the LAST moment an entry can be
    // "still trying"; tests + UI assert "expired at 30 days" semantics.
    if (now - entry.created_at) >= 30.days()
       and entry.delivered_to.len() < entry.recipient_owners.len():
      entry.delivery_status = 'expired'
      apply_outbox(entry)
      // No retry from this point. UI shows "undeliverable" badge.
      // Recipients still in recipient_owners but not in delivered_to
      // are surfaced as "didn't deliver" per spec.
      // Entry is NOT GC'd — it's persistent chat history.
```

The **silent-leaver / wrong-addr / 30-day-offline** cases are deliberately indistinguishable in v1.

## Idempotency and drain semantics

- **Within-tick deduplication.** `dm_outbox` maintains an in-process `HashSet<(OutboxEntryId, DeviceIdentityHash)>` of in-flight sends; drain tick clears entries on transport result. Prevents duplicate sends within a single tick.
- **Cross-device drain duplication tolerated.** Two of sender's devices may both run drain on the same OutboxEntry. Cost: marginal extra Reticulum traffic. Recipient's `apply_inbox` is idempotent (composite key `(space_id, message_cid)`), and sender's `apply_outbox` merges `delivered_to` via union — no corruption. Future optimization: HLC-based delivery lease.
- **DmAck idempotent.** Same `(space_id, message_cid, ack_from_owner_addr)` arriving twice — `delivered_to.insert(addr)` is set-semantics; second ack is a no-op.
- **Inbound DmCidNotify idempotent (atomic-emit semantics).** If recipient receives the same notify twice (e.g., sender's two drain devices both sent), `apply_inbox` upserts on `(space_id, message_cid)` — single InboxEntry exists. `apply_inbox` returns `ApplyOutcome::Applied` for the first call (new entry written) and `ApplyOutcome::NoOp` for the duplicate. The `dm-received` IPC event MUST be emitted **only when `apply_inbox` returns `Applied`** — the inserted-vs-already-present discriminant is the atomic boundary, not a separate pre-write existence check (which would race between concurrent handlers on the same device).
- **Reticulum link encryption is not authentication of OwnerAddr.** The link-layer ECDH proves "this packet came from a device holding the private key for `from_identity_hash`," but `from_identity_hash` is a *device* identifier. The receive-time `verify_sender_binding` check is what binds the encrypted-payload `sender` field to the link-origin OwnerAddr. Sender impersonation across owners requires both subverting Reticulum link-layer crypto AND owning the encryption content_key — not separately defensible.

## 30-day expiration mechanism

- Drain tick evaluates each `pending`/`partial` OutboxEntry: if `(now - created_at) > 30 days` and not all recipients ack'd, transition to `expired`.
- Expiration is computed at drain time (no separate timer); on restart, drain just runs and naturally catches up.
- `expired` entries stay in CRDT as persistent chat history; no GC in v1 (consistent with ZEB-206 spec).
- UI surfaces "undeliverable" badge for the unack'd recipients; user can manually delete.

## Tests

### Phase 1 — encryption primitives

- `dm_crypto::encrypt_then_decrypt_roundtrip` — random key + AAD + payload → decrypt yields identical bytes
- `dm_crypto::aad_mismatch_rejects` — encrypt under AAD₁, decrypt under AAD₂ → AeadFailureAllKeys
- `dm_crypto::version_byte_unknown_rejects` — storage_blob[0] = 0xFF → UnknownVersion
- `dm_crypto::length_gate_short_blob_rejects` — storage_blob.len() < 29 → TruncatedBlob (no panic)
- `dm_crypto::tampered_ciphertext_rejects` — flip a bit → AeadFailureAllKeys
- `dm_crypto::prior_content_keys_fallback_succeeds` — encrypt under K₁; decrypt with content_key=K₂ + prior=[K₁] → success
- `dm_crypto::sender_binding_mismatch_rejects` — payload.sender ≠ link_origin → SenderImpersonation
- `dm_envelope::dm_packet_discriminant_round_trip` — encode each variant, decode, equal
- `dm_envelope::dm_packet_unknown_discriminant_rejects` — `[0xFF, ...]` → Err(UnknownDiscriminant)
- `owner_state_types::dm_content_key_serializes_as_bstr_32` — single key encodes as `0x58 0x20 || <32 bytes>` (34 bytes total), NOT as CBOR array-of-u8 (~63 bytes for random data)
- `owner_state_types::device_identity_hash_serializes_as_bstr_16` — single hash encodes as `0x50 || <16 bytes>` (17 bytes total), NOT as CBOR array-of-u8
- `owner_state_types::dm_content_key_zeroized_on_drop` — drop a key in a controlled allocation, verify backing memory is zero (use `ZeroizeOnDrop` derive's guarantees)
- `owner_state_types::dm_content_key_debug_redacts` — `format!("{:?}", key)` does not include any of the 32 byte values
- `owner_state_types::space_with_content_key_round_trip_cbor` — DM Space round-trips through CBOR persistence
- `owner_state_types::folder_space_no_content_key_round_trip_cbor` — Folder Space serializes without `ck`/`pk` keys
- `owner_state_crdt::validate_invariants_dm_requires_content_key` — DM without content_key → InvariantError
- `owner_state_crdt::validate_invariants_folder_rejects_content_key` — Folder with content_key → InvariantError
- `owner_state_crdt::validate_invariants_content_key_in_prior_rejects`
- `owner_state_crdt::validate_invariants_prior_cap_exceeded_rejects`
- `owner_state_crdt::dedupe_merge_prior_content_keys_cap_convergent` — the 5-Space scenario from ZEB-219: K₃<K₂<K₄<K₅<K₁ lex, cap=2, two distinct merge orders → both yield prior_content_keys Vec equal to `[K₃, K₂]` (smallest two in ascending lex order — `merge_prior_content_keys` sorts ascending then truncates, so the stored Vec preserves ascending order). The set-equivalence `{K₂, K₃}` referenced in ZEB-219 prose is identical; the test asserts the ordered Vec for byte-equality regression coverage.
- `owner_state_crdt::owner_device_cache_lww_apply` — newer learned_at replaces; older is no-op
- `owner_state_crdt::owner_device_cache_apply_dedupes_devices` — input `[d1, d2, d1]` results in stored `[d1, d2]` (sorted, deduped)
- `owner_state_crdt::owner_device_cache_apply_caps_at_max` — input of 100 devices results in stored Vec of length 32, comprising the lex-smallest 32 entries
- `dm_outbox::resolve_link_origin_owner_one_match_returns_owner`
- `dm_outbox::resolve_link_origin_owner_zero_matches_returns_unknown`
- `dm_outbox::resolve_link_origin_owner_multi_match_returns_ambiguous` — same DeviceIdentityHash present under two OwnerAddr entries → AmbiguousLinkOrigin (regression for cache-poisoning attack via duplicate-claim)
- `dm_outbox::send_dm_recipient_owners_excludes_sender_dedup_sort` — group-DM with members [Alice, Bob, Carol, Carol] produces OutboxEntry.recipient_owners = [Bob, Carol] (sorted, deduped, sender excluded)
- `owner_state_persist::full_roundtrip_with_dm_state` — DM Space + OwnerDeviceCache entries persist + reload identically

### Phase 2 — outbox skeleton + send_dm IPC, stub transport

- `dm_outbox::send_dm_creates_outbox_entry`
- `dm_outbox::send_dm_invalid_space_kind_rejects` (kind=Folder → Err(InvalidSpaceKind))
- `dm_outbox::send_dm_unknown_space_rejects`
- `dm_outbox::drain_advances_pending_to_complete_on_stub_success` (single-recipient DM)
- `dm_outbox::drain_partial_state_some_recipients_acked` (group-DM, 2 of 3 ack)
- `dm_outbox::drain_respects_backoff_skipping_recently_attempted`
- `dm_outbox::drain_expires_30day_old_entry` (sim clock 30d + 1s past)
- `dm_outbox::drain_complete_entry_is_no_op`
- `dm_outbox::drain_in_flight_set_prevents_duplicate_send_within_tick`
- `dm_outbox::handle_ack_updates_delivered_to`
- `dm_outbox::handle_ack_duplicate_is_idempotent`
- `tests/dm_send_integration.rs` — invoke `send_dm` via Tauri test harness; verify OutboxEntry written, MessageId returned

### Phase 3a — harmony-runtime companion PR

- `runtime::send_unicast_to_known_device_via_existing_tunnel`
- `runtime::send_unicast_initiates_tunnel_if_absent`
- `runtime::unicast_received_demuxed_from_tunnel_packet`

### Phase 3b — real Reticulum delivery + 30-day expiration

- All Phase 2 stub-transport tests re-run with real harmony-runtime transport (mocked at the RuntimeAction-channel boundary, not over the wire)
- `dm_outbox::handle_unicast_invite_creates_space` — inject UnicastReceived(disc=0x01, ...) → new Space + OwnerDeviceCache update keyed by `invite.inviter` (NOT members[0])
- `dm_outbox::handle_unicast_invite_binds_inviter_field_not_members_zero` — group-DM where `invite.inviter` is lex-LARGEST member → cache entry created under `invite.inviter`, not `invite.members[0]`. Subsequent DmCidNotify from inviter's identity_hash resolves correctly via link-origin binding (regression for the members[0]-vs-inviter bug)
- `dm_outbox::handle_unicast_invite_inviter_not_in_members_drops` — invite.inviter ∉ invite.members → InviterNotInMembers, drop, no state mutation
- `dm_outbox::handle_unicast_invite_sender_device_not_in_sender_devices_drops` — from_identity_hash ∉ invite.sender_devices → SenderDeviceNotInSenderDevices, drop
- `dm_outbox::handle_unicast_invite_receiver_not_in_members_drops` — self_owner_addr ∉ invite.members → ReceiverNotInMembers, drop (likely misroute or probe)
- `dm_outbox::handle_unicast_invite_decline_writes_no_state` — UI declines → no Space written, no cache update, no IPC emitted, no notification to inviter
- `dm_outbox::handle_unicast_cidnotify_triggers_cas_fetch_decrypt_inbox_write` — inject (disc=0x02, ...) + mock CasOp::GetOrFetch → InboxEntry written, dm-received emitted, DmAck fan-out queued to all sender_devices
- `dm_outbox::handle_unicast_cidnotify_duplicate_no_dm_received_emit` — second receive of same notify → apply_inbox returns NoOp → no second dm-received IPC event (atomic-emit regression)
- `dm_outbox::handle_unicast_cidnotify_sender_binding_mismatch_drops` — `MessagePayload.sender` ≠ link_origin → no InboxEntry, no ack, telemetry logged
- `dm_outbox::handle_unicast_cidnotify_owner_field_mismatch_drops_no_cache_update` — `notify.sender_owner_addr` ≠ resolved owner from `from_identity_hash` → no `apply_owner_device_update`, no InboxEntry, no ack, telemetry logged (cache-poisoning regression)
- `dm_outbox::handle_unicast_cidnotify_unknown_link_origin_drops` — `from_identity_hash` not in any OwnerDeviceCache entry → drop with `UnknownLinkOrigin` telemetry
- `dm_outbox::handle_unicast_cidnotify_decrypt_failure_uses_prior_keys` — primary content_key fails, prior_content_keys[0] succeeds → InboxEntry written
- `dm_outbox::handle_unicast_ack_updates_outbox_delivered_to` — inject (disc=0x03, ...) → OutboxEntry update, dm-delivered emitted
- `dm_outbox::handle_unicast_ack_owner_field_mismatch_drops` — `ack.ack_from_owner_addr` ≠ resolved owner → no delivered_to mutation, telemetry logged
- `dm_outbox::handle_unicast_ack_from_non_recipient_drops` — resolved owner not in `OutboxEntry.recipient_owners` → no delivered_to mutation, telemetry logged (forged-ack regression)
- `dm_outbox::handle_unicast_ack_ambiguous_link_origin_drops` — same DeviceIdentityHash claimed by two OwnerAddr entries → AmbiguousLinkOrigin → no mutation
- `dm_outbox::expiration_at_30day_boundary_marks_expired` — entry with `created_at = now - 30.days()` (exactly at the boundary) → status transitions to `'expired'` on this drain tick (verifies `>=` not `>` semantics)
- `dm_outbox::expiration_29day_old_entry_stays_pending` — entry with `created_at = now - 29.days()` → status remains `'pending'` (boundary regression)
- `dm_outbox::expiration_30day_real_transport_path`

### Phase 4 — NavService + IPC events + UI

- vitest `nav-service.test.ts` — receives `nav-updated` for new DM Space → renders in tree
- vitest `dm-received-handler.test.ts` — IPC event triggers DmMessageList refresh
- vitest `dm-create-dialog.test.ts` — picks members; blocks at-17 with conversion prompt; triggers add_space IPC
- Manual two-device LAN smoke deferred to follow-up Linear ticket

## Verification gates

Per user memory `cargo fmt + cargo clippy gates required at every task verification, not just clippy`:

- `cargo fmt --all -- --check` — clean
- `cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings` — clean
- `cargo test --manifest-path src-tauri/Cargo.toml` — all green
- `npx vitest run` — all green
- `npx tsc --noEmit` — clean

For local pipe-based verification commands: use `set -o pipefail` or `${PIPESTATUS[0]}` to avoid silent pipe-exit-code lies.

## Acceptance criteria

### Per-phase

**Phase 1**: All `dm_crypto`, `dm_envelope`, Space-CRDT-extension tests pass; existing Sub-A tests still pass; all gates green.

**Phase 2**: `send_dm` IPC works end-to-end against stub transport; drain state machine + backoff + expiration tests pass; all gates green.

**Phase 3a** (harmony-runtime): Unicast round-trip tested via existing tunnel infra; harmony-node test suite green; harmony-runtime semver bumped.

**Phase 3b**: Phase 3a merged + tagged upstream; harmony-client deps bumped; real-transport tests pass at the RuntimeAction-channel boundary; all gates green.

**Phase 4**: DMs work in the GUI on a single device (UI shows messages, send/receive, delivered indicators); at-17 conversion prompt blocks adds correctly; all gates green; manual two-device LAN test deferred to follow-up Linear ticket.

### Umbrella ZEB-216

- All phase PRs merged green on `main`
- A connected harmony-client can:
  - Create a DM with another OwnerAddr (single recipient or group of 3-16)
  - Send and receive DM messages with content encryption
  - See messages converge across the user's bound devices
  - Handle offline-recipient case (outbox retries until delivery or 30-day expiration)
  - Surface delivered/expired states in the UI
- Sender impersonation prevented (sender-binding check)
- Encryption-at-rest: every DM blob in CAS is ChaCha20-Poly1305 ciphertext, never cleartext
- ZEB-219 design implemented and verified (round-trip tests + multi-merge convergence test)
- Existing tests (Sub-A, Phase 3b CAS, identity, pairing) all still pass
- New cargo + vitest + tsc gates all green
- Manual LAN testing follow-up filed in Linear

## Manual testing — deferred to follow-up

Per the Phase 3b precedent (ZEB-224), file a follow-up Linear issue at PR-creation time for manual two-device LAN scenarios that can't be CI-tested:

- DM round-trip between two paired devices on the same LAN
- Group-DM (3-5 members) round-trip
- Sender online, recipient offline → recipient comes online → message delivers
- Recipient ack lost → sender retries → eventual ack
- Multi-device recipient: send from Phone, recipient B1 receives, B2/B3 see InboxEntry via Flow A
- 30-day expiration triggers correctly (use sim clock; full 30-day timer too long for human test)
- Dedupe collision: A1 and A2 both create DM with same person while disconnected → after pairing, prior_content_keys merges correctly + both old + new messages decrypt
- DmInvite with ≤16 valid members works; 17 is blocked at IPC layer

Use a descriptive phrase to file the issue (per memory rule "never invent Linear IDs"); use the assigned ID after Linear creates it.

## Out of scope / deferred follow-ups

- Forward secrecy / content-key rotation under group-DM membership growth (ZEB-219 deferral)
- Lex-grinding attack on prior_content_keys cap rule (ZEB-219 future work — DmInvite rate-limiting is the complementary mitigation)
- Per-device delivery lease in OutboxEntry to suppress cross-device duplicate sends (v1 tolerates them)
- HLC-monotonic per-OwnerAddr `device_list_version` to suppress redundant `sender_devices` piggyback on every message (v1 always includes)
- DmReactions, DmReadReceipts (deferred from ZEB-206 spec)
- Voice/video DM transport (separate ticket)

## References

- [ZEB-206 nav-tree umbrella spec](2026-04-30-zeb-206-nav-tree-design.md) — Sub-B section, Flow C (Sending a DM)
- [ZEB-219 DM content encryption design](2026-05-02-zeb-219-dm-content-encryption-design.md) — defines the encryption contract this spec implements
- [ZEB-211 owner-state encryption design](2026-04-30-zeb-211-owner-state-encryption-design.md) — parallel pattern for owner-state Zenoh topic
- [ZEB-215 Sub-A Phase 3b CAS spec](2026-05-01-zeb-215-sub-a-phase3b-content-cas-design.md) — `CasOp::PutLocal` and `CasOp::GetOrFetch` infrastructure consumed here
- ZEB-16 — Reticulum unicast plane B (the underlying transport)
- ZEB-173 — owner identity recovery (out-of-scope threat model anchor)
