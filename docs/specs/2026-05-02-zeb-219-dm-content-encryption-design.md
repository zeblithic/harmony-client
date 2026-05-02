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

## Design constraints (architectural — read before implementation or v2 design)

- **Fixed 32-byte key shape.** `Space.content_key: Option<Zeroizing<[u8; 32]>>` and `Space.prior_content_keys: Vec<Zeroizing<[u8; 32]>>` hardcode 256-bit symmetric keys (the `Zeroizing` wrapper is the in-memory zeroization invariant — see "Implementation note" below; the wire format is identical to a bare `[u8; 32]`). v1 (ChaCha20-Poly1305) and a hypothetical v2 (XChaCha20-Poly1305) both use 256-bit keys, so the version-byte prefix carries the v1↔v2 migration cleanly without changing the `Space` schema. **A future v3 primitive that requires a different key length (e.g., a 384-bit AEAD) would require a Space-schema migration**, not just a version-byte bump — both `content_key` and `prior_content_keys` would need restructuring (e.g., to `Option<Zeroizing<Vec<u8>>>` with length-prefixed entries, plus a per-key version tag in `prior_content_keys` to disambiguate which primitive expects which key length). Design the v3 transition as a Space-schema migration, not a wire-format-only bump.
- **Symmetric AEAD only.** The scheme assumes every member can both encrypt and decrypt with the same `content_key`. Asymmetric primitives (e.g., per-recipient public-key encryption) would shift the data model away from a per-Space shared key. Out of scope for v1; tied to a hypothetical "DM with read-only members" feature that doesn't exist yet.
- **Reticulum-link as the authenticator.** The receive-time sender-binding check (see "Sender-binding check" below) relies on Reticulum's authenticated `Link` primitive to provide the ground-truth origin owner. A future non-Reticulum delivery transport would either need to provide an equivalent authenticated origin OR require per-message Ed25519 signing (Future Work).

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
│   adds: content_key: Option<Zeroizing<[u8; 32]>>            │
│         prior_content_keys: Vec<Zeroizing<[u8; 32]>>        │
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
│   stores storage_blob = version || nonce_12 || ct || tag    │
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
use zeroize::Zeroizing;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmInvite {
    #[serde(rename = "si")] pub space_id: SpaceId,
    #[serde(rename = "kn")] pub kind: SpaceKind,                 // 'dm' or 'group-dm'
    #[serde(rename = "me")] pub members: Vec<OwnerAddr>,         // canonical member set including inviter
    #[serde(rename = "ck")] pub content_key: Zeroizing<[u8; 32]>, // zeros on drop, same as Space.content_key
    #[serde(rename = "ca")] pub created_at: Hlc,                 // for the new Space entry's HLC
}
impl_canonical!(DmInvite);
```

`content_key` here is wrapped in `Zeroizing` for the same reason as `Space.content_key`: an in-flight invite is key material in memory and must not leak via freed allocations. Wire format is unchanged (`Zeroizing<T>` delegates serde to `T`).

Short `serde(rename)` keys mirror the existing `Space` / `OutboxEntry` patterns; canonical CBOR rules from ZEB-220 apply via `impl_canonical!`.

The invite itself is sent over Reticulum's `Link` primitive — a Curve25519-ECDH-derived authenticated channel between the inviter's bound device and the recipient's bound device, with forward secrecy at the link-establishment level. **We do NOT add a second application-layer encryption around the invite payload**: that would duplicate Reticulum's existing E2E story and introduce a separate key-distribution concern for the invite-encryption key, which is a regress.

### Storage at rest

Per bound device, each member's owner-state CRDT contains the `Space` entry with the `content_key` field. The CRDT block is encrypted at rest under that owner's `owner_state_entry_aead_key` (ZEB-211). So `content_key` is never on disk in cleartext — it's bytes inside an AEAD-sealed CRDT block, and ZEB-211's per-entry deterministic encryption is what protects it.

### Lifetime

Same as the parent Space — created at `add_dm_space`, persists until manual delete. **No rotation in v1** (see "Future work" for v2 considerations).

## Per-message encryption scheme

For each outgoing DM message:

```text
content_key       = space.content_key                             // 32 bytes (the active key)
plaintext_cbor    = canonical_cbor_encode(MessagePayload { .. }) // length = N
nonce_12          = OsRng.fill_bytes(12)                          // fresh CSPRNG per message
aad               = canonical_cbor_encode(space.dedupe_key())     // see "Why AAD = ..." below
version_byte      = 0x01                                          // v1; reserved for v1→v2 migration
ciphertext        = ChaCha20Poly1305::encrypt(
                      key       = content_key,
                      nonce     = nonce_12,
                      aad       = aad,
                      plaintext = plaintext_cbor)                 // length = N
poly1305_tag      = (16-byte Poly1305 authenticator)              // returned alongside ciphertext
ciphertext_with_tag = ciphertext || poly1305_tag                  // length = N + 16
storage_blob      = version_byte || nonce_12 || ciphertext_with_tag
                  //  1 byte      || 12 bytes || N + 16 bytes
                  // total length = N + 29 bytes
message_cid       = ContentId::for_book(
                      &storage_blob,
                      ContentFlags { encrypted: true, ..Default::default() })?  // fallible
```

Decrypt is the inverse with **multi-key fallback in deterministic order** (see "DM dedupe + content_key collisions" below):

1. Read the 1-byte `version` prefix from `storage_blob[0]`. If it's not a version this build recognizes, drop the blob.
2. **Select the per-version wire-format layout and the matching primitive** (this is what makes mixed-version dedupe collisions safe — see "DM dedupe + content_key collisions" and "Migration path"):
   - `version == 0x01` (v1, ChaCha20-Poly1305): `nonce_len = 12`. Slice `nonce = storage_blob[1..13]` and `ciphertext_with_tag = storage_blob[13..]`. Use `ChaCha20Poly1305::decrypt`.
   - `version == 0x02` (v2, XChaCha20-Poly1305) — *reserved for future use; not implemented in v1 builds:* `nonce_len = 24`. Slice `nonce = storage_blob[1..25]` and `ciphertext_with_tag = storage_blob[25..]`. Use `XChaCha20Poly1305::decrypt`.
   - In all versions, `poly1305_tag_len = 16` and the plaintext length is implicit: `N = storage_blob.len() - 1 - nonce_len - 16`.
3. Recompute `aad = canonical_cbor_encode(space.dedupe_key())` using the merged Space's current dedupe key (which is dedupe-stable — see "Why AAD ..." below). AAD is independent of version.
4. Try `decrypt(key=space.content_key, nonce, aad, ciphertext_with_tag)` first — the active key, with the version-selected primitive. On AEAD success, return plaintext.
5. On AEAD failure, iterate `space.prior_content_keys` **in stored order** (which is lexicographically sorted and deduplicated — see invariants below), retrying decrypt with each candidate using the same version-selected primitive. Return plaintext on first success.
6. If all keys fail, drop the blob (could be relocation attack, corrupted ciphertext, or genuinely wrong-key payload from a non-member).

Stored order matters for test determinism — the "DM dedupe collision preserves decryptability" verification gate depends on a fixed iteration sequence to avoid flake.

Note: the same 32-byte `content_key` is usable by both v1 and v2 primitives (both happen to use 256-bit keys), which is why the per-Space key set is scheme-agnostic. A future v3 with a different key length would need a Space-schema migration — see "Design constraints" above.

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
    // `Option<Zeroizing<[u8; 32]>>::as_deref()` returns `Option<&[u8; 32]>`
    // via Zeroizing's Deref<Target = T>; the inner key bytes never leave
    // the Zeroizing wrapper, so they zero on drop.
    let content_key: &[u8; 32] = space
        .content_key
        .as_deref()
        .ok_or(DmEncryptError::MissingKey)?;

    let plaintext_cbor = canonical_cbor_encode(payload)?;
    let aad = canonical_cbor_encode(&space.dedupe_key())?;  // dedupe-stable binding

    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let cipher = ChaCha20Poly1305::new_from_slice(content_key)
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
        Vec::with_capacity(1 + nonce_bytes.len() + ciphertext_with_tag.len());
    storage_blob.push(DM_ENCRYPTION_VERSION_V1);  // 0x01 — see "Version-byte prefix" below
    storage_blob.extend_from_slice(&nonce_bytes);
    storage_blob.extend_from_slice(&ciphertext_with_tag);

    let message_cid = ContentId::for_book(
        &storage_blob,
        ContentFlags { encrypted: true, ..Default::default() },
    )
    .map_err(DmEncryptError::Cid)?;
    Ok((storage_blob, message_cid))
}

const DM_ENCRYPTION_VERSION_V1: u8 = 0x01;
```

`ContentId::for_book` is fallible (matches the existing call sites in `owner_state_sync.rs:415-422`, `folders.rs:78-82`); `DmEncryptError::Cid` wraps the upstream error. The function takes `&Space` rather than `(&[u8; 32], &SpaceId)` so the dedupe-key AAD is computed from the same source-of-truth that the encryption key comes from — this prevents a class of caller-side bugs where the wrong `SpaceId` is paired with a different Space's `content_key`.

The nonce buffer is on the stack (small, no zeroize concern). `Space.content_key`, `Space.prior_content_keys`, and `DmInvite.content_key` MUST hold key material via `Zeroizing<[u8; 32]>` — the field types in this spec encode that requirement directly (see the struct definitions above). This is a security invariant: bare `[u8; 32]` would leave key material in freed allocations and defeat the at-rest protection that the rest of this scheme rests on. Wire format is unchanged because `Zeroizing<T>` delegates serde Serialize/Deserialize to `T` transparently.

Local-variable bindings that materialize key material (e.g., a derived key during a merge step, or a working copy lifted out of `DmInvite` before assignment to `Space`) MUST be wrapped in `Zeroizing` for the same reason. Use `as_deref()` (which returns `Option<&[u8; 32]>` via `Zeroizing`'s `Deref<Target = [u8; 32]>` impl) when you need a `&[u8; 32]` for an AEAD call — see the `encrypt_dm_payload` snippet above. This keeps the underlying bytes inside the wrapper for the entirety of their lifetime; nothing escapes into an unzeroed long-lived binding.

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

### Sender-binding check (normative — applies to ALL InboxEntry write paths)

The sender-binding check is required at **every code path that writes or updates an `InboxEntry` from decrypted payload bytes**. v1 has exactly one such path (live Reticulum receive), but the requirement is phrased to forward-protect any path that ever decrypts a `MessagePayload` and creates an `InboxEntry` from it.

#### Live Reticulum receive (the only v1 path)

When a DM ciphertext arrives via Reticulum unicast at a recipient's bound device, **before** writing an `InboxEntry`, the receiver MUST:

1. Identify the **authenticated link origin owner** — Reticulum's `Link` primitive authenticates the originating destination, and harmony-client maps Reticulum identity hashes back to `OwnerAddr` via the existing transport stack (ZEB-16 plane B).
2. Decrypt the storage blob (multi-key fallback per "Per-message encryption scheme").
3. Compare the decrypted `MessagePayload.sender` against the link origin owner.
4. If they match → write the `InboxEntry` with the bound sender; render normally.
5. If they mismatch → drop the ciphertext, do NOT write `InboxEntry`, surface a `dm-impersonation-rejected` telemetry event so the malicious-co-member case is observable in production.

The plaintext bind makes the check possible (without the bind, the receiver would have no ground-truth sender to compare against the Reticulum origin); the check is what gives the bind its protective value.

#### Why CRDT replication of an existing `InboxEntry` does NOT need a re-check

Owner-state CRDT sync replicates the `InboxEntry` *record* (a CRDT data structure) across the recipient's bound devices. It does NOT re-decrypt the underlying ciphertext. The `InboxEntry` was written exactly once on the originating device — the device that received the live Reticulum delivery and ran the check — and the resulting CRDT record (with its bound sender) is what propagates. The originating device's check is the only check that runs, and that's sufficient because the per-owner CRDT replication boundary means all of the recipient's bound devices share the same trust domain (same master seed, same owner identity); a downstream bound device trusts an upstream bound device of the same owner by design.

If a downstream bound device somehow received a malicious `InboxEntry` from an *upstream* malicious bound device of its own owner, that's the "compromised bound device" case which is out-of-scope per the threat model (full-master-seed compromise = full owner compromise).

#### Forward requirement for hypothetical future paths

Any future feature that creates or updates an `InboxEntry` from decrypted ciphertext bytes — for example, a "reimport from CAS after device migration" flow, a "merge in archived ciphertexts from external backup" flow, or a non-Reticulum transport — MUST satisfy at least ONE of:

- **(a)** the path has access to an authenticated message origin (e.g., a transport-layer authenticated peer identity or a per-message signature) and runs the equivalent of steps 1-5 above; OR
- **(b)** the path persists a verified bound-sender marker into the `InboxEntry` at the time of original receipt (e.g., a flag `sender_verified_via: ReticulumLink | Ed25519Sig | ...`), and the rehydration code asserts the marker is present before rendering.

Implementations MUST NOT create or update an `InboxEntry` from raw decrypted bytes via a path that lacks both (a) and (b). On mismatch, the path MUST drop the payload and emit `dm-impersonation-rejected`. This forward requirement is documented here so downstream features cannot accidentally regress the impersonation defense.

#### Self-write paths (sender-side)

For a sender's bound device writing its own `OutboxEntry`, the bound `sender` field MUST equal the local owner address. This is a trivially-passing check in normal operation but the spec mandates it explicitly to catch any bug that would write a misbound `OutboxEntry` (which would later replicate via owner-state sync to the sender's other bound devices and surface as "Did I send this?").

This receive-time check is a v1 hard requirement. Per-message Ed25519 signing (Future Work) would extend the same protection to scenarios where the authenticated link origin is unavailable or weakened.

### Why determinism in canonical-CBOR encoding even with random nonces

The plaintext encoding uses RFC 8949 §4.2 deterministic CBOR (bytewise key sort, shortest-form integers, definite-length, no tags) via `impl_canonical!` (defined in ZEB-220). Determinism in plaintext encoding is *not* load-bearing for cross-device CID convergence here (only the original sender encrypts; nothing converges across senders). It IS useful for the cross-encoder verification gate (catches encoder regressions early) and forward-compatible if we later decide to switch to deterministic AEAD nonces. Cheap to do, hard to retrofit.

## Wire format

### Storage blob (in CAS at `message_cid`)

Let `N := len(plaintext_cbor)` (the canonical-CBOR encoding of `MessagePayload`). Then:

```text
storage_blob[0]              = version_byte                    (1 byte; v1 = 0x01)
storage_blob[1..13]          = nonce_12                        (12 bytes random)
storage_blob[13..13+N]       = ciphertext                      (N bytes)
storage_blob[13+N..13+N+16]  = poly1305_tag                    (16 bytes)
total length                 = N + 29 bytes
```

Where:
- `ciphertext` = `ChaCha20(content_key, nonce_12) XOR plaintext_cbor`, of length `N`.
- `poly1305_tag` = 16-byte Poly1305 MAC over `aad || pad16(aad) || ciphertext || pad16(ciphertext) || len64_le(aad) || len64_le(ciphertext)`, where the Poly1305 one-time key is derived from `ChaCha20(content_key, nonce_12, counter=0)` per RFC 8439 §2.8. The nonce influences the MAC indirectly via the one-time-key derivation; it is not itself authenticated data.
- `ciphertext_with_tag` (used internally by RustCrypto's `Aead::encrypt` API) = `ciphertext || poly1305_tag`, of length `N + 16`.

`message_cid = ContentId::for_book(storage_blob, ContentFlags { encrypted: true, ..Default::default() })` is computed over the **exact bytes of `storage_blob`** (i.e., over `version_byte || nonce_12 || ciphertext || poly1305_tag`). Total per-message overhead vs. plaintext: **29 bytes** (1 version + 12 nonce + 16 tag).

### Version-byte prefix and v1→v2 migration

The leading version byte is reserved for forward-compatibility with future encryption-scheme migrations (see Migration path). v1 ciphertexts always start with `0x01`; v2 (when defined) would use `0x02`; etc. The byte is part of `storage_blob` and is therefore part of the input to `ContentId::for_book`, so v1 and v2 ciphertexts of the same plaintext + key + nonce produce different CIDs.

Decryption reads the version byte first and dispatches to the matching primitive — see the decrypt step list in "Per-message encryption scheme". This makes mixed-version dedupe collisions safe (see "DM dedupe + content_key collisions" below): a merged Space can hold ciphertexts produced under different schemes, and decrypt picks the right primitive per ciphertext rather than per Space. The active encryption scheme used for **new** writes is governed by the local build's compiled-in default; v1 implementations always write `0x01`.

Receivers MUST reject ciphertexts whose version byte they do not recognize. v1 receivers reject anything other than `0x01`; v2 receivers accept `0x01` (with v1 primitives) and `0x02` (with v2 primitives). This forward-compatibility scheme means a fleet running mixed v1/v2 builds during a rollout can still exchange messages with each other — v1 senders' messages decrypt on v2 receivers; v2 senders' messages are dropped on v1 receivers (which cannot decrypt them).

### `Space.content_key` and `Space.prior_content_keys` fields

Added to the existing `Space` struct in `owner_state_types.rs`:

```rust
use zeroize::Zeroizing;

pub struct Space {
    // ... existing fields (id, kind, parent, community_id, name, transport, members, ...) ...

    /// Active key — used for ALL new encryption. `Some` for kind=dm/group-dm,
    /// `None` for everything else. Enforced by `Space::validate_invariants`
    /// (see "Required invariants" below).
    ///
    /// Wrapped in `Zeroizing` so the bytes are deterministically cleared on
    /// drop — this is a security MUST, not a SHOULD. Wire format is identical
    /// to a bare `[u8; 32]` because `Zeroizing<T>` delegates serde
    /// Serialize/Deserialize to `T` transparently.
    #[serde(rename = "ck", skip_serializing_if = "Option::is_none")]
    pub content_key: Option<Zeroizing<[u8; 32]>>,

    /// Historical keys retained for DECRYPTION only — never used for new
    /// encryption. Populated by the dedupe-merge rule (see "DM dedupe +
    /// content_key collisions" below). Empty for non-DM Spaces and for DM
    /// Spaces that have not undergone any dedupe collision. Stored sorted
    /// lexicographically (canonical CBOR contract). Each entry is wrapped in
    /// `Zeroizing` for the same reason as `content_key`.
    #[serde(rename = "pk", skip_serializing_if = "Vec::is_empty", default)]
    pub prior_content_keys: Vec<Zeroizing<[u8; 32]>>,
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
| `Dm`, `GroupDm` | MUST be `Some` | MAY be empty or non-empty (subject to the rules below) |

In addition, for any `Space` (regardless of `kind`), `prior_content_keys` MUST satisfy:

- **Strictly sorted lexicographically:** for all adjacent pairs `(prior_content_keys[i], prior_content_keys[i+1])`, the bytewise comparison MUST yield `<` (strict; equality is forbidden — see uniqueness).
- **Unique:** no two entries are byte-equal. (Implied by strict sortedness, but stated explicitly because the merge rule produces sorted-deduplicated unions and a callsite that bypasses that path would not catch a duplicate without an explicit invariant.)
- **Disjoint from `content_key`:** for `kind=Dm/GroupDm`, the active `content_key` MUST NOT appear inside `prior_content_keys`. A two-step dedupe collision where one side already had the eventual winner's key as a historical entry would otherwise produce a Space with `content_key = K1` AND `K1 ∈ prior_content_keys`, causing every decrypt to iterate `K1` twice (active path then prior path) and producing a corner case the merge rule must filter out — see step 4 of the merge rule below.
- **Bounded length:** `prior_content_keys.len() <= MAX_PRIOR_CONTENT_KEYS` (= 16; defined in "DoS bound on `prior_content_keys`" below).

These are MUST-level invariants — `validate_invariants` MUST return `Err(InvariantError(...))` if any is violated. The function is called on every locally-produced write AND on every incoming merge, so a malformed Space cannot enter the CRDT through any code path. There is no fallback-to-cleartext path: every encrypt/decrypt site that operates on a `kind=Dm/GroupDm` Space MUST treat `content_key.is_none()` as a hard error.

### DM dedupe + `content_key` collisions

Two bound devices can independently create a Space with the same dedupe identity (e.g., both Alice-laptop and Alice-phone create a DM with Bob while offline). When the CRDTs sync, `apply_space_with_canonicalization` (`owner_state_crdt.rs:294-325`) runs — the lexicographically-smaller `SpaceId` wins, the loser is dropped, and dependent records (`OutboxEntry`, `InboxEntry`, `ReadMarker`) are rewritten to the winner.

The naive merge rule "winner's `content_key` survives, loser's is dropped" makes any messages encrypted under the loser's `content_key` permanently undecryptable. This spec REQUIRES the following merge rule instead:

**Merge rule (normative).** When two `Dm` or `GroupDm` Spaces collide on dedupe and merge:

1. The winner's `content_key` becomes the merged Space's `content_key` (active key for future encryption).
2. The loser's `content_key` (if `Some`) is appended to a working set.
3. Both sides' existing `prior_content_keys` are unioned into the working set (deduplicated by byte-equality, sorted lexicographically).
4. **Filter the winner's active `content_key` out of the working set.** If a prior merge had ever cycled the eventual winner's key through someone's historical pool, it would re-appear here; remove it so `content_key ∉ prior_content_keys` is preserved (per the invariant above). This step is required for correctness, not just optimization — without it, decrypt iterates the active key twice and the disjoint-from-`content_key` invariant fails.
5. Apply the cap (see "DoS bound on `prior_content_keys`" below).
6. The resulting set becomes the merged Space's `prior_content_keys` (sorted, deduplicated, disjoint from `content_key`, bounded).

Implementation note: this happens inside `lww_merge_space` (or its DM-specific successor) and applies symmetrically — the winner-side device runs the same merge logic the loser-side device does, both starting from the same input pair, and both arriving at the same merged state.

**Behavioral consequences:**
- Encrypt always uses the active `content_key`. New messages are decryptable by anyone holding the active key.
- Decrypt iterates `[content_key] + prior_content_keys` (in stored order) until one AEAD verification succeeds (per "Decrypt is the inverse" in "Per-message encryption scheme"). Old messages encrypted under any historically-active retained key remain decryptable as long as the bound device still holds the relevant key bytes.
- New members invited AFTER a collision settles receive only the active `content_key` via `DmInvite` — they cannot decrypt messages encrypted under any prior key. This matches v1's "no rotation" + "no automatic share-history" stance and is consistent with `DmInvite`'s wire format below (active key only).

**Merge ordering on a single device.** A device may apply incoming Spaces in any order, but the merge rule (after the cap below) is order-independent: the final `prior_content_keys` set on a converged device is a deterministic function of the set of historically-active keys it has observed for that dedupe identity, plus the cap policy. CRDT convergence is preserved.

#### DoS bound on `prior_content_keys`

A malicious co-member of a DM can repeatedly send `DmInvite`s for the same `(sorted_members)` identity, each with a fresh random `content_key`. Each invite, when applied on the victim's device, triggers a dedupe collision and (under the naive merge rule) appends to `prior_content_keys`. Decryption cost grows linearly with the set size; an attacker can cheaply force unbounded growth and pump per-message decrypt latency.

**Cap (normative).** `MAX_PRIOR_CONTENT_KEYS = 16`. After steps 1-4 of the merge rule (union, dedupe, sort, filter winner's active key), the merge MUST cap the size:

1. Let `merged = winner.prior_content_keys ∪ {loser.content_key} ∪ loser.prior_content_keys`, deduplicated, sorted lexicographically, with the winner's `content_key` removed (per merge step 4).
2. If `merged.len() <= 16`, accept the merge with `prior_content_keys = merged`.
3. If `merged.len() > 16`, **deterministically reject the loser-side contribution that would push past the cap.** Concretely: the merged set is `winner.prior_content_keys` (kept verbatim) plus as many keys from `{loser.content_key} ∪ loser.prior_content_keys` (in lexicographic order) as fit. Keys from the loser side that would push past 16 are discarded. The winner's active `content_key` is also kept verbatim. Apply the rest of the merge (Space metadata, members, etc.) normally.
4. Emit a `dm-prior-keys-cap-exceeded` telemetry event with `(space_id, current_size)` so operators can observe attacker activity. The user-facing UI MAY surface a warning prompting the user to delete the DM Space (treating it as adversarial).

Both peers compute the same final state because the input pair `(winner, loser)` is symmetric across devices (both observe the same two Spaces and both apply the same lexicographic SpaceId tie-break). The cap-rejection step therefore deterministically converges.

Cap policy choice rationale: 16 chosen to match the group-DM member cap (the "natural maximum participation" heuristic). Reject-overflow rather than drop-oldest because we have no HLC ordering on retained keys (they're just byte values), so "oldest" is undefined; reject-overflow is deterministic and safe. The trade-off is that legitimate offline-creation collisions remain decryptable (the realistic count is 0-1 entries), while adversarial pumping is bounded.

**Decrypt-after-cap behavior — winner side.** Messages encrypted under loser-side keys that were rejected by the cap are silently undecryptable on the winner-side device. This matches the "non-member can't decrypt" property — a spammer's keys are effectively non-members from the perspective of the legitimate DM. No silent fallback to cleartext.

**Decrypt-after-cap behavior — loser side (footgun for legitimate cases).** When the cap fires on a *loser-side device* — e.g., Alice-phone in an intra-owner offline-creation collision where Alice-phone's locally-active `content_key` is the "loser" because Alice-phone's SpaceId is larger than Alice-laptop's — the merge step that drops loser-side contributions ALSO drops Alice-phone's previously-active `content_key` from the merged state. As a result, **Alice-phone loses the ability to decrypt its own pre-merge messages on this device**. The messages remain decryptable on Alice-laptop (which has Alice-phone's old key in `prior_content_keys` if the cap hasn't fired there) and on any other bound device that synced before the cap fired — but the loser-originating device itself cannot decrypt its own history once the cap rejects its own key.

This is a footgun for legitimate dedupe collisions when the cap fires (rare in practice — `prior_content_keys` typically has 0-1 entries). For adversarial cases this is the desired property: the cap is meant to bound attacker damage, not preserve every-device-decrypts-everything semantics. Implementations SHOULD surface the `dm-prior-keys-cap-exceeded` telemetry event prominently in operator dashboards so the asymmetry is observable when it bites.

### `DmInvite` (Reticulum payload)

CBOR-encoded `DmInvite` per "Distribution" above. The encoded blob is sent as the application payload over a Reticulum `Link` between the inviter's and recipient's destinations — Reticulum supplies transport encryption + authentication; we add no application-layer wrapper.

## Performance estimate

Per encrypt or decrypt of a 200-byte text message:

- CSPRNG nonce (12 bytes via `OsRng`): ~50 ns
- ChaCha20-Poly1305 encrypt/decrypt (200 bytes): ~2 μs
- BLAKE3 hash via `ContentId::for_book` (229 bytes for storage_blob: 1 version + 12 nonce + 200 ct + 16 tag): ~1 μs
- Canonical-CBOR encode (~140 bytes for `MessagePayload`): ~1 μs

Total: ~5 μs per send or receive. Negligible vs Reticulum link establishment (typical ~ms) or owner-state CRDT bookkeeping. No blocking I/O on the encrypt/decrypt path.

## Migration path

### v1 → v2 (future, e.g. switching to XChaCha20-Poly1305)

**Per-message, not per-Space.** v1 ships a 1-byte version prefix on every `storage_blob` (see "Version-byte prefix and v1→v2 migration" in Wire format). v2 implementations switch to writing `0x02` for new ciphertexts; v1 ciphertexts in CAS continue to be readable because their version-byte tells the decryptor to use the v1 primitive.

Flow:

1. v2 builds add the v2 primitive (e.g., XChaCha20-Poly1305 with 24-byte nonce) and a new compiled-in `DM_ENCRYPTION_VERSION_V2 = 0x02`. New writes use `0x02`.
2. v2 decrypt branches on the version byte: `0x01` → v1 primitive (existing); `0x02` → v2 primitive. Both branches share the same key-fallback iteration over `[content_key] + prior_content_keys`.
3. **Mixed-version dedupe collisions are safe.** When a v1 device and a v2 device independently create a Space with the same `sorted_members` and later sync, the dedupe-merge proceeds normally. Some ciphertexts in the merged Space are tagged `0x01` and others `0x02`; decrypt picks the right primitive per ciphertext. The merged Space's `content_key` and `prior_content_keys` are scheme-agnostic (32-byte values usable by both primitives — both happen to use 256-bit keys; if a future v3 changed key length, the design would need extension).
4. v1 builds reject `0x02` ciphertexts (cannot decrypt). v2 senders' messages are silently dropped on v1 receivers — same fail-closed semantics as a wrong-key payload from a non-member. Operators rolling out v2 should target a coordinated fleet upgrade or accept the asymmetric-readability window.
5. No flag day, no global re-encryption migration. Mixed-version fleets converge as devices upgrade.

The 1-byte cost in v1 is the price of forward-compatibility; without it, v1→v2 migration would face the dedupe-collision-with-different-primitives problem (one merged Space, two ciphertexts under different primitives, no way to disambiguate without a per-message marker).

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
- **Invariant enforcement:** call `Space::validate_invariants` against malformed Spaces — `kind=Dm` with `content_key=None`, `kind=Folder` with `content_key=Some(...)`, `kind=Folder` with non-empty `prior_content_keys`, `kind=Dm` with unsorted `prior_content_keys`, `kind=Dm` with duplicate entries in `prior_content_keys`, `kind=Dm` with `prior_content_keys.len() > MAX_PRIOR_CONTENT_KEYS`, `kind=Dm` where `content_key` is also present in `prior_content_keys` (disjoint invariant) — assert each returns `Err(InvariantError(...))`.
- **Two-step dedupe collision (winner's key in both fields):** simulate the Greptile-flagged scenario — Device A has `{content_key: K1, prior: []}`, Device B has `{content_key: K2, prior: [K1]}` (B previously merged with someone whose key was K1). Trigger A↔B dedupe-merge with A's SpaceId smaller (so K1 wins as active). Assert the merged Space has `content_key = K1` and `K1 ∉ prior_content_keys` (i.e., the merge-rule step 4 filter ran and K1 was removed from the candidate prior set). Assert messages encrypted under K1 decrypt via the active path; messages encrypted under K2 decrypt via the prior-fallback path.
- **DoS-cap rejection:** simulate 17+ dedupe-collision merges against a single DM Space; assert `prior_content_keys.len() <= MAX_PRIOR_CONTENT_KEYS` after every merge; assert `dm-prior-keys-cap-exceeded` telemetry fires on the overflow attempt; assert ciphertexts encrypted under the rejected key are NOT decryptable on the post-cap Space.
- **Version-byte rejection:** craft a `storage_blob` with `version_byte = 0x99` (unknown); assert decrypt rejects without attempting any primitive.
- **Mixed-version dedupe simulation:** craft two ciphertexts under the same Space with `version_byte = 0x01` (v1 ChaCha20-Poly1305) and a stub `0x02` primitive (test-only fixture); merge two Spaces with one ciphertext each; assert decrypt branches correctly per byte and both ciphertexts decode. (This gate locks the forward-compat contract before v2 lands.)
- **Deterministic-iteration order test:** populate `prior_content_keys` with 3 keys in unsorted order; assert that after `validate_invariants` accepts (or after the canonicalizing constructor), the stored order is lexicographically sorted; encrypt one ciphertext under the lexicographically-largest key; assert decrypt iterates active → prior[0] → prior[1] → prior[2] (the last) in that exact order.
- **No-cleartext-write gate:** static / lint check (or manual code review item) confirming there is no code path in the DM send/receive flow that writes plaintext `MessagePayload` bytes to CAS or Reticulum without going through `encrypt_dm_payload`. Gates against accidental regressions.
- **Canonical-CBOR cross-encoder gate:** serialize the same `MessagePayload` via two encoder paths (or two separate processes), assert byte-identical output. Sanity check; not load-bearing for convergence here but cheap.
- **`DmInvite` round-trip:** serialize/deserialize, assert `members` set + `content_key` bytes intact.
- **Concurrent send safety:** encrypt 10 concurrent messages under one `content_key` from one sender (`tokio::spawn` × 10), assert all 10 produce distinct `nonce_12` values (CSPRNG quality / lack-of-collision check on a tiny scale).
- **Outbox idempotency:** call `encrypt_dm_payload` twice with the same `(space, payload)`. Assert the two generated `nonce_12` values differ (direct test of CSPRNG randomness — this is what the random-nonce contract actually requires). Asserting that the resulting `message_cid`s differ is also acceptable as a smoke test, with the understanding that CID equality under distinct nonces is astronomically improbable but theoretically possible (BLAKE3-224 collision space). The nonce-difference assertion is the load-bearing one.
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
