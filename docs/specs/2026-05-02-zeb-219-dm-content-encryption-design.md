# ZEB-219: DM content encryption design

**Date:** 2026-05-02
**Linear:** [ZEB-219](https://linear.app/zeblith/issue/ZEB-219)
**Origin:** Surfaced during ZEB-206 PR #70 round-4 CodeRabbit review (2026-04-30). Companion ticket to ZEB-211 (which specs owner-state Zenoh topic encryption); this one specs DM payload encryption. The original ZEB-206 design said "Rust stores message blob in CAS → gets CID" without specifying encryption — but CAS is content-addressed and not access-controlled, so any CID-leak vector (timing analysis, log exposure, malicious bound-device exfiltration) would compromise DM contents stored as cleartext. Per the round-4 spec update, DM message blobs MUST be encrypted before CAS storage; this spec defines how.

## Goal

Define a concrete encryption scheme for DM and group-DM message payloads stored in CAS that:

1. Hides plaintext from any non-member who learns a `message_cid` (the primary motivating threat).
2. Lives entirely at the harmony-client application layer — no harmony-core changes (same constraint as ZEB-211).
3. Reuses primitives already in `src-tauri/Cargo.toml`: `chacha20poly1305`, `blake3`, `rand` (no new crypto deps).
4. Uniformly handles 2-person DM (`kind = 'dm'`, members = 2) and group-DM (`kind = 'group-dm'`, members = 3..=16) with a single scheme.
5. Composes cleanly with the existing data model — `OutboxEntry.message_cid` and `InboxEntry.message_cid` already point at "the encrypted blob in CAS"; this spec defines what those CIDs resolve to.

## Threat model

**In scope:**

- Adversary who learns one or more `message_cid`s (timing analysis, log exposure, accidental leak in error messages, malicious external relay forwarding the CID).
- Passive CAS observer who fetches arbitrary blobs at known CIDs.
- Future-revoked group member trying to read messages sent **before** their join — defended by "the per-Space `content_key` is only in current members' CRDTs, plus historical InboxEntry/OutboxEntry records that join with `message_cid` are also only in current members' CRDTs."
- Cross-Space relocation attack: adversary with two valid ciphertexts (one from Space-A, one from Space-B, same content_key by accident) trying to splice them.

**Out of scope:**

- Master-seed compromise of any current member (ZEB-173 fresh-identity-on-total-loss handles total identity loss; orthogonal to message encryption).
- Compromised bound device of a current member — the bound device has the master seed and the full owner-state CRDT; it can already read all DMs that owner participates in. Defending against this is "compromised bound device" model from ZEB-211, not this spec.
- Forward secrecy for messages sent **after** a member is removed from a group-DM. v1 doesn't propagate leaves (Sub-B explicit choice), so removed-Charlie keeps her copy of the `content_key` indefinitely. v2 with propagating leaves would need key rotation; deferred (see Future Work).
- **Per-message Ed25519 signing** as a primitive layered on top of AEAD. The cross-member impersonation case (a malicious bound-device co-member of a group-DM crafts ciphertext claiming `sender = SomeoneElse`) is in-scope and defended by the **receive-time sender-binding check** documented below — that check uses Reticulum's authenticated link origin as ground truth. Per-message signing would extend the same protection to delivery paths where Reticulum origin authentication is unavailable (e.g., relayed or stored-and-forwarded delivery via non-Reticulum transports); deferred — see Future Work.

## Non-goals

- Forward secrecy in v1.
- Per-member or per-device key isolation within a Space (every member of a Space holds the same `content_key` by design — required for symmetric AEAD on shared content).
- Encryption of the `Space` CRDT entry itself (it already inherits ZEB-211's at-rest encryption — `content_key` lives inside that wrapped block).
- Encryption of community-membership CRDTs (separate ticket scope; communities use signed-event CRDTs with public power-level state, not shared symmetric encryption).
- Hiding "Owner X is in some DM with Owner Y" metadata — Reticulum unicast addresses leak the destination identity-hash; that's a transport-layer concern under ZEB-16.

## Architecture

Encryption sits at the harmony-client application layer between the message-compose path and CAS storage. harmony-content (and harmony-core more broadly) is unmodified — it sees only opaque storage blobs and computes structured CIDs over them.

```text
┌─────────────────────────────────────────────────────────────┐
│ harmony-client::dm_content (NEW module)                     │
│   ├─ encrypt_dm_payload(&space, payload)                    │
│   │     → (storage_blob, message_cid)                       │
│   ├─ decrypt_dm_payload(&space, blob)  ← multi-key fallback │
│   │     → MessagePayload                                    │
│   ├─ verify_sender_binding(payload, reticulum_origin)       │
│   │     → Result — load-bearing receive-time check          │
│   └─ MessagePayload — bound envelope (canonical CBOR)       │
├─────────────────────────────────────────────────────────────┤
│ harmony-client::owner_state_types::Space                    │
│   adds field: content_key: Option<[u8; 32]>                 │
│   (None for folder/community; Some for dm/group-dm.         │
│   Inherits ZEB-211 at-rest encryption automatically.)       │
├─────────────────────────────────────────────────────────────┤
│ harmony-client::dm_outbox (Sub-B will introduce)            │
│   ├─ Reticulum invite: carries DmInvite { members,          │
│   │                                       content_key, ... } │
│   └─ Reticulum delivery: ships storage_blob bytes to the    │
│         recipient's bound device(s).                        │
├─────────────────────────────────────────────────────────────┤
│ harmony-content (unmodified, upstream)                      │
│   stores storage_blob = nonce_12 || ciphertext_with_tag     │
│   at message_cid = ContentId::for_book(blob, encrypted=true)│
└─────────────────────────────────────────────────────────────┘
```

## Key lifecycle

### Creation

When the user creates a new DM or group-DM Space (e.g. `add_dm_space` IPC), the originating client generates a fresh symmetric content_key from the OS CSPRNG and writes it as a field on the new `Space` CRDT entry:

```rust
use rand::{rngs::OsRng, RngCore};

let mut content_key = [0u8; 32];
OsRng.fill_bytes(&mut content_key);

// content_key is then written into Space.content_key on the originating device's
// owner-state CRDT, which inherits ZEB-211's at-rest encryption.
```

The key MUST come from a CSPRNG. `rand::rngs::OsRng` is the canonical choice in the existing codebase (used by ZEB-211 root-publish nonces and ZEB-194 capability tokens).

### Distribution

Per-DM-Space `content_key` is distributed to each prospective member via Reticulum unicast as part of the Space invite. The invite payload carries everything the recipient needs to write a matching `Space` entry into their own owner-state CRDT:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmInvite {
    #[serde(rename = "si")] pub space_id: SpaceId,
    #[serde(rename = "kn")] pub kind: SpaceKind,         // 'dm' or 'group-dm'
    #[serde(rename = "me")] pub members: Vec<OwnerAddr>, // canonical member set including inviter
    #[serde(rename = "ck")] pub content_key: [u8; 32],
    #[serde(rename = "ca")] pub created_at: Hlc,         // for the new Space entry's HLC
}
impl_canonical!(DmInvite);
```

Short `serde(rename)` keys mirror the existing `Space` / `OutboxEntry` patterns; canonical CBOR rules from ZEB-220 apply via `impl_canonical!`.

The invite itself is sent over Reticulum's `Link` primitive — a Curve25519-ECDH-derived authenticated channel between the inviter's bound device and the recipient's bound device, with forward secrecy at the link-establishment level. **We do NOT add a second application-layer encryption around the invite payload**: that would duplicate Reticulum's existing E2E story and introduce a separate key-distribution concern for the invite-encryption key, which is a regress.

### Storage at rest

Per bound device, each member's owner-state CRDT contains the `Space` entry with the `content_key` field. The CRDT block is encrypted at rest under that owner's `owner_state_entry_aead_key` (ZEB-211). So `content_key` is never on disk in cleartext — it's bytes inside an AEAD-sealed CRDT block, and ZEB-211's per-entry deterministic encryption is what protects it.

### Lifetime

Same as the parent Space — created at `add_dm_space`, persists until manual delete. **No rotation in v1** (see "Future work" for v2 considerations).

## Per-message encryption scheme

For each outgoing DM message:

```text
content_key      = Space.content_key                              // 32 bytes (the active key)
plaintext_cbor   = canonical_cbor_encode(MessagePayload { .. })
nonce_12         = OsRng.fill_bytes(12)                           // fresh CSPRNG per message
aad              = canonical_cbor_encode(space.dedupe_key())      // see "Why AAD = ..." below
ciphertext_tag   = ChaCha20Poly1305::encrypt(
                     key       = content_key,
                     nonce     = nonce_12,
                     aad       = aad,                              // relocation + dedupe-stable binding
                     plaintext = plaintext_cbor)                  // appends 16-byte Poly1305 tag
storage_blob     = nonce_12 || ciphertext_tag                     // 12 + N + 16 = N + 28 bytes
message_cid      = ContentId::for_book(
                     &storage_blob,
                     ContentFlags { encrypted: true, ..Default::default() })?  // fallible
```

Decrypt is the inverse with **multi-key fallback** (see "DM dedupe + content_key collisions" below):

1. Take `storage_blob[0..12]` as the nonce; the remainder is `ciphertext_with_tag`.
2. Recompute `aad = canonical_cbor_encode(space.dedupe_key())` using the merged Space's current dedupe key (which is dedupe-stable — see "Why AAD ..." below).
3. Try `ChaCha20Poly1305::decrypt(content_key, nonce, aad, ciphertext_with_tag)`. On AEAD success, return plaintext.
4. On AEAD failure, iterate `space.prior_content_keys` in any order, retrying decrypt with each. Return on first success.
5. If all keys fail, drop the blob (could be relocation attack, corrupted ciphertext, or genuinely wrong-key payload from a non-member).

### Why random nonce, not deterministic

ZEB-211 owner-state encryption uses *deterministic* nonces (BLAKE3-MAC of plaintext + space_lookup_key) because the same Space entry MUST encrypt to the same ciphertext on every bound device — otherwise CRDT convergence breaks. **DM encryption has no such requirement**: only the original sender encrypts, and `OutboxEntry.message_cid` is computed once at compose time and reused on retries (the OutboxEntry persists in the sender's CRDT; retries don't re-encrypt). So determinism buys nothing here, and using random nonces avoids a footgun: with deterministic nonces, two identical messages ("ok" twice in a row) would collapse to a single InboxEntry under upsert-by-(space_id, message_cid). Random nonces preserve message identity.

### Why AAD = `canonical_cbor_encode(space.dedupe_key())`, not `space_id.0`

A naive choice would be AAD = `space_id.0` (the 16-byte ULID). **This breaks decrypt after CRDT dedupe collapses two Spaces.** The harmony-client CRDT explicitly canonicalizes `SpaceId` when two Spaces share a dedupe key (`owner_state_crdt.rs::apply_space` lines 162-172): the lexicographically-smaller ULID wins, the loser is dropped, and `canonicalize_dependent_space_ids` rewrites every `OutboxEntry.space_id` and `InboxEntry.space_id` to point to the winner. For DM Spaces — which dedupe by `SortedMembers` (see `DedupeKey::SortedMembers` in `owner_state_types.rs`) — this canonicalization is the expected outcome of two bound devices independently creating the same DM offline and then syncing.

If AAD were `space_id.0`, the loser-side ciphertext would have been encrypted under the loser's old SpaceId; after merge, decrypt would compute AAD from the canonicalized winner SpaceId, AEAD would fail, and the message history under the loser ID would be silently unreadable.

The fix is to bind ciphertext to a value that is **stable across dedupe canonicalization**: the Space's dedupe key itself. For:

- `dm` (`DedupeKey::SortedMembers(Vec<OwnerAddr>)`) — the sorted member set is the canonical identity; immutable for the Space's lifetime.
- `group-dm` (`DedupeKey::Id(SpaceId)`) — group-DMs do NOT cross-dedupe (two devices that independently create different SpaceIds for the same group are simply two different group-DMs in v1). So the SpaceId is stable here too.

Encoding the `DedupeKey` enum via canonical CBOR (RFC 8949 §4.2; same rules used for Space CRDT entries via `impl_canonical!`) gives a single, well-defined byte string per Space across the system. The encoding is uniform — the same code path handles DM and group-DM — and stable through the entire Space lifetime.

A weaker alternative is to use `space_id.0` for group-DM only and the sorted-members CBOR for DM, branching on `kind`. Uniform-by-dedupe-key is cleaner, and the encoded-enum-tag overhead (1-2 bytes vs raw `SpaceId`) is irrelevant.

### Why no key separation between encrypt and any other AEAD context

ZEB-211 derives two AEAD keys from one master (one for per-entry encryption with deterministic nonces, one for state-root publishes with random nonces) because mixing nonce strategies under one key risks a deterministic-nonce-collides-with-random-nonce catastrophe. **DM encryption uses only random nonces under `content_key`**, so there's no nonce-strategy collision concern — one key is correct.

### Implementation note

```rust
use chacha20poly1305::{aead::{Aead, KeyInit, Payload}, ChaCha20Poly1305, Nonce};
use rand::{rngs::OsRng, RngCore};

fn encrypt_dm_payload(
    space: &Space,
    payload: &MessagePayload,
) -> Result<(Vec<u8>, ContentId), DmEncryptError> {
    // Active key is required by the Space invariant for kind=dm/group-dm.
    let content_key = space.content_key.as_ref().ok_or(DmEncryptError::MissingKey)?;

    let plaintext_cbor = canonical_cbor_encode(payload)?;
    let aad = canonical_cbor_encode(&space.dedupe_key())?;  // dedupe-stable binding

    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let cipher = ChaCha20Poly1305::new_from_slice(content_key.as_slice())
        .map_err(DmEncryptError::Key)?;

    let ciphertext_with_tag = cipher
        .encrypt(
            nonce,
            Payload {
                msg: &plaintext_cbor,
                aad: &aad,
            },
        )
        .map_err(DmEncryptError::Aead)?;

    let mut storage_blob =
        Vec::with_capacity(nonce_bytes.len() + ciphertext_with_tag.len());
    storage_blob.extend_from_slice(&nonce_bytes);
    storage_blob.extend_from_slice(&ciphertext_with_tag);

    let message_cid = ContentId::for_book(
        &storage_blob,
        ContentFlags { encrypted: true, ..Default::default() },
    )
    .map_err(DmEncryptError::Cid)?;
    Ok((storage_blob, message_cid))
}
```

`ContentId::for_book` is fallible (matches the existing call sites in `owner_state_sync.rs:415-422`, `folders.rs:78-82`); `DmEncryptError::Cid` wraps the upstream error. The function takes `&Space` rather than `(&[u8; 32], &SpaceId)` so the dedupe-key AAD is computed from the same source-of-truth that the encryption key comes from — this prevents a class of caller-side bugs where the wrong `SpaceId` is paired with a different Space's `content_key`.

The nonce buffer is on the stack (small, no zeroize concern). `Space.content_key` and `Space.prior_content_keys` are bytes inside an AEAD-sealed CRDT block on disk; in-memory long-lived references SHOULD be wrapped in `Zeroizing` (consistent with ZEB-211's handling of derived keys).

## Plaintext envelope: `MessagePayload`

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagePayload {
    #[serde(rename = "bd")] pub body: Vec<u8>,        // raw bytes — UTF-8 text or media payload
    #[serde(rename = "mt")] pub mime_type: String,    // e.g. "text/plain", "image/jpeg"
    #[serde(rename = "se")] pub sender: OwnerAddr,    // 16-byte; sender's owner address
    #[serde(rename = "sa")] pub sent_at: Hlc,         // sender's HLC at compose time
}
impl_canonical!(MessagePayload);
```

### Why bind `sender` and `sent_at` into the ciphertext

These could equally live only on `OutboxEntry`/`InboxEntry` CRDT metadata. Binding them inside the AEAD'd plaintext serves two purposes:

- **Single source of truth on display**: the receiver's UI renders directly from the decrypted plaintext. If an `InboxEntry`'s metadata disagrees with the binding (e.g. someone wrote an InboxEntry with metadata sender=Alice but the ciphertext binds sender=Bob), the receive-time check below detects and rejects.
- **Bind to encryption author**: the binding tells the receiver "whoever encrypted this blob claimed it was from `sender`". Combined with the receive-time check, this turns into a real authenticity guarantee.

**The bind alone is not sufficient.** Any co-member of a group-DM holds the `content_key` and can produce ciphertext with arbitrary `sender` field values. To prevent cross-member impersonation, the receive path MUST verify the bind against the authenticated message origin.

### Receive-time sender-binding check (normative)

When a DM ciphertext arrives via Reticulum unicast at a recipient's bound device, **before** writing an `InboxEntry`, the receiver MUST:

1. Identify the **authenticated link origin owner** — Reticulum's `Link` primitive authenticates the originating destination, and harmony-client maps Reticulum identity hashes back to `OwnerAddr` via the existing transport stack (ZEB-16 plane B).
2. Decrypt the storage blob (multi-key fallback per "Per-message encryption scheme").
3. Compare the decrypted `MessagePayload.sender` against the link origin owner.
4. If they match → write the `InboxEntry` with the bound sender; render normally.
5. If they mismatch → drop the ciphertext, do NOT write `InboxEntry`, surface a `dm-impersonation-rejected` telemetry event so the malicious-co-member case is observable in production.

This check is the load-bearing defense against cross-member sender impersonation. The plaintext bind is what makes the check possible (without the bind, the receiver would have no ground-truth sender to compare against the Reticulum origin); the check is what gives the bind its protective value.

For self-write paths (e.g., a sender's bound device writing its own `OutboxEntry`), the same check applies trivially — the bound sender must equal the local owner address.

This receive-time check is a v1 hard requirement. Per-message Ed25519 signing (Future Work) would extend the same protection to scenarios where Reticulum origin authentication is unavailable or weakened (e.g., relayed delivery via a non-Reticulum transport).

### Why determinism in canonical-CBOR encoding even with random nonces

The plaintext encoding uses RFC 8949 §4.2 deterministic CBOR (bytewise key sort, shortest-form integers, definite-length, no tags) via `impl_canonical!` (defined in ZEB-220). Determinism in plaintext encoding is *not* load-bearing for cross-device CID convergence here (only the original sender encrypts; nothing converges across senders). It IS useful for the cross-encoder verification gate (catches encoder regressions early) and forward-compatible if we later decide to switch to deterministic AEAD nonces. Cheap to do, hard to retrofit.

## Wire format

### Storage blob (in CAS at `message_cid`)

```text
storage_blob[0..12]     = nonce_12                                  (12 bytes random)
storage_blob[12..N+12]  = ChaCha20 keystream XOR plaintext_cbor     (N bytes)
storage_blob[N+12..]    = Poly1305 authentication tag               (16 bytes)
total overhead per msg  = 28 bytes (12 nonce + 16 tag)
```

### `Space.content_key` and `Space.prior_content_keys` fields

Added to the existing `Space` struct in `owner_state_types.rs`:

```rust
pub struct Space {
    // ... existing fields (id, kind, parent, community_id, name, transport, members, ...) ...

    /// Active key — used for ALL new encryption. `Some` for kind=dm/group-dm,
    /// `None` for everything else. Enforced by `Space::validate_invariants`
    /// (see "Required invariants" below).
    #[serde(rename = "ck", skip_serializing_if = "Option::is_none")]
    pub content_key: Option<[u8; 32]>,

    /// Historical keys retained for DECRYPTION only — never used for new
    /// encryption. Populated by the dedupe-merge rule (see "DM dedupe +
    /// content_key collisions" below). Empty for non-DM Spaces and for DM
    /// Spaces that have not undergone any dedupe collision. Stored sorted
    /// lexicographically (canonical CBOR contract).
    #[serde(rename = "pk", skip_serializing_if = "Vec::is_empty", default)]
    pub prior_content_keys: Vec<[u8; 32]>,
}
```

`skip_serializing_if` keeps the wire format unchanged for non-DM Spaces (no new bytes on folders, channels, or communities).

#### Required invariants (enforced by `Space::validate_invariants`)

Add the following kind-specific rules to the existing `validate_invariants` function in `owner_state_types.rs:566-662`:

| `kind` | `content_key` | `prior_content_keys` |
|---|---|---|
| `Folder` | MUST be `None` | MUST be empty |
| `Channel`, `PublicChannel` | MUST be `None` | MUST be empty |
| `Community` | MUST be `None` | MUST be empty |
| `Dm`, `GroupDm` | MUST be `Some` | MAY be empty or non-empty |

These are MUST-level invariants — `validate_invariants` MUST return `Err(InvariantError(...))` if any is violated. The function is called on every locally-produced write AND on every incoming merge, so a malformed Space cannot enter the CRDT through any code path. There is no fallback-to-cleartext path: every encrypt/decrypt site that operates on a `kind=Dm/GroupDm` Space MUST treat `content_key.is_none()` as a hard error.

### DM dedupe + `content_key` collisions

Two bound devices can independently create a Space with the same dedupe identity (e.g., both Alice-laptop and Alice-phone create a DM with Bob while offline). When the CRDTs sync, `apply_space_with_canonicalization` (`owner_state_crdt.rs:294-325`) runs — the lexicographically-smaller `SpaceId` wins, the loser is dropped, and dependent records (`OutboxEntry`, `InboxEntry`, `ReadMarker`) are rewritten to the winner.

The naive merge rule "winner's `content_key` survives, loser's is dropped" makes any messages encrypted under the loser's `content_key` permanently undecryptable. This spec REQUIRES the following merge rule instead:

**Merge rule (normative).** When two `Dm` or `GroupDm` Spaces collide on dedupe and merge:

1. The winner's `content_key` becomes the merged Space's `content_key` (active key for future encryption).
2. The loser's `content_key` (if `Some`) is appended to the merged Space's `prior_content_keys`.
3. Both sides' existing `prior_content_keys` are unioned into the merged `prior_content_keys` (deduplicated by byte-equality, sorted lexicographically).

Implementation note: this happens inside `lww_merge_space` (or its DM-specific successor) and applies symmetrically — the winner-side device runs the same merge logic the loser-side device does.

**Behavioral consequences:**
- Encrypt always uses the active `content_key`. New messages are decryptable by anyone holding the active key.
- Decrypt iterates `[content_key] + prior_content_keys` until one AEAD verification succeeds (per "Decrypt is the inverse" in "Per-message encryption scheme"). Old messages encrypted under any historically-active key remain decryptable as long as the bound device still holds the relevant key bytes.
- The set is monotonically growing. In v1 there is no GC of `prior_content_keys`. In practice the set typically holds 0 or 1 entries (dedupe collisions are uncommon outside the offline-creation case).
- New members invited AFTER a collision settles receive only the active `content_key` via `DmInvite` — they cannot decrypt messages encrypted under any prior key. This matches v1's "no rotation" + "no automatic share-history" stance and is consistent with `DmInvite`'s wire format below (active key only).

**Merge ordering on a single device.** A device may apply incoming Spaces in any order, but the merge rule is associative and commutative over the set of historically-active keys: regardless of which side a device sees first, the final `prior_content_keys` set is the union of all keys ever associated with the dedupe identity, with the LWW winner as the active key. CRDT convergence is preserved.

### `DmInvite` (Reticulum payload)

CBOR-encoded `DmInvite` per "Distribution" above. The encoded blob is sent as the application payload over a Reticulum `Link` between the inviter's and recipient's destinations — Reticulum supplies transport encryption + authentication; we add no application-layer wrapper.

## Performance estimate

Per encrypt or decrypt of a 200-byte text message:

- CSPRNG nonce (12 bytes via `OsRng`): ~50 ns
- ChaCha20-Poly1305 encrypt/decrypt (200 bytes): ~2 μs
- BLAKE3 hash via `ContentId::for_book` (228 bytes): ~1 μs
- Canonical-CBOR encode (~140 bytes for `MessagePayload`): ~1 μs

Total: ~5 μs per send or receive. Negligible vs Reticulum link establishment (typical ~ms) or owner-state CRDT bookkeeping. No blocking I/O on the encrypt/decrypt path.

## Migration path

### v1 → v2 (future, e.g. switching to XChaCha20-Poly1305)

**Per-Space, not global.** Existing v1 Spaces keep their existing `content_key` and v1 scheme; new Spaces created post-upgrade use v2. Flow:

1. Bump the encryption-scheme indicator on the `Space` CRDT type — e.g., add `encryption_version: u8` (defaulting to 1 for old entries, 2 for new ones). `MessagePayload` parsing branches on the parent Space's `encryption_version`.
2. Existing Spaces continue to use ChaCha20-Poly1305 with 12-byte nonce indefinitely. Spaces created after the upgrade use the v2 primitive.
3. No flag day, no global re-encryption migration, no first-byte-version-tag ambiguity in the storage blob.

Cost: mixed-version users have a heterogeneous Space set during the rollout. Acceptable for a v1→v2 transition since each Space's lifetime is independent.

## Future work (follow-up tickets — file as descriptive phrases, get IDs from Linear)

- **Per-message Ed25519 signing.** Defends against bound-device sender-impersonation among co-members of a group-DM (Q4 option C in brainstorm). Today the threat is constrained by the per-owner CRDT replication boundary, but a richer threat model (e.g., explicit message-forwarding flow) would require this.
- **Forward-secrecy / member-removal re-keying.** Tied to ZEB-216 v2 propagating leaves. Today, removing a member from a group-DM is owner-local — we cannot revoke their decrypt capability for new messages without rotating the per-Space `content_key`.
- **Group-DM ratcheting (MLS / Sender Keys).** If group-DM grows past 16 (currently capped) or cross-device synchronization of group state evolves, look at MLS for proper async group key agreement. Out-of-scope for v1.
- **Reticulum invite metadata privacy.** The Reticulum unicast destination identity-hash leaks "X is establishing a link with Y" to a network observer. Mitigation lives at the transport layer (ZEB-16), not in this spec.

## Verification gates

Before ZEB-219 implementation lands (note: this spec is design-only; implementation rides on ZEB-216 Sub-B):

- **Round-trip correctness:** encrypt 100 random `MessagePayload`s under random `Space`s (random `content_key` + random valid `dedupe_key`), decrypt, assert byte-identical plaintext.
- **Cross-Space relocation rejection:** encrypt against `Space_A` (one dedupe_key), attempt decrypt with AAD computed from `Space_B`'s dedupe_key, assert AEAD failure.
- **Wrong-key rejection:** encrypt under `content_key_X`, attempt decrypt under `content_key_Y` (with `prior_content_keys` empty), assert AEAD failure.
- **DM dedupe collision preserves decryptability:** create two `Dm` Spaces on simulated bound devices (same sorted-members, different SpaceIds, different `content_key`s); encrypt one message under each; merge the Spaces via `apply_space_with_canonicalization`; assert both ciphertexts decrypt successfully against the merged Space (one via active key, one via `prior_content_keys` fallback).
- **AAD stability across canonicalization:** encrypt a message with the loser's `SpaceId` in scope; trigger CRDT canonicalization (loser → winner SpaceId rewrite); recompute AAD = `canonical_cbor_encode(merged_space.dedupe_key())`; assert AEAD verification still succeeds. (This is the regression test for the original `space_id.0` AAD bug.)
- **Sender-binding mismatch rejection:** simulate Reticulum delivery from origin owner Charlie carrying a payload with `MessagePayload.sender = Bob`; assert the receive path rejects (no `InboxEntry` written) and emits the `dm-impersonation-rejected` telemetry event.
- **Invariant enforcement:** call `Space::validate_invariants` against malformed Spaces — `kind=Dm` with `content_key=None`, `kind=Folder` with `content_key=Some(...)`, `kind=Folder` with non-empty `prior_content_keys` — assert each returns `Err(InvariantError(...))`.
- **No-cleartext-write gate:** static / lint check (or manual code review item) confirming there is no code path in the DM send/receive flow that writes plaintext `MessagePayload` bytes to CAS or Reticulum without going through `encrypt_dm_payload`. Gates against accidental regressions.
- **Canonical-CBOR cross-encoder gate:** serialize the same `MessagePayload` via two encoder paths (or two separate processes), assert byte-identical output. Sanity check; not load-bearing for convergence here but cheap.
- **`DmInvite` round-trip:** serialize/deserialize, assert `members` set + `content_key` bytes intact.
- **Concurrent send safety:** encrypt 10 concurrent messages under one `content_key` from one sender (`tokio::spawn` × 10), assert all 10 produce distinct `nonce_12` values (CSPRNG quality / lack-of-collision check on a tiny scale).
- **Outbox idempotency:** call `encrypt_dm_payload` twice with the same `(content_key, space_id, payload)`, assert the resulting `message_cid`s **differ** (random-nonce property; verifies we did not accidentally re-introduce determinism).
- **`Space.content_key` zeroization on drop:** if `Space` is wrapped in `Zeroizing` at the call sites that hold it long-lived, verify a debug-build sanity test confirms drop clears the bytes.
- `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings` clean.
- `cargo fmt --all -- --check` clean.

## Acceptance criteria for this spec

- All five questions in the original ZEB-219 ticket description (key derivation/lifecycle, AEAD primitive + nonce policy, group-DM rotation, storage blob format, threat model) are answered with concrete primitive choices and rationale.
- Threat model explicitly distinguishes in-scope vs out-of-scope adversaries.
- All cryptographic operations specified to a level of detail an implementer can translate directly into Rust (an implementer should not have to make further crypto-design decisions; only translate types and call existing crate APIs).
- Performance estimate exists and is negligible vs CRDT bookkeeping / Reticulum link costs.
- Migration path defined for future scheme changes.
- Followup concerns documented with descriptive phrases (Linear IDs assigned at file-time, never invented).
- Does NOT modify harmony-core — same constraint as ZEB-211.
