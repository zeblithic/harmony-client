# ZEB-211: Owner-state Zenoh topic encryption design

**Date:** 2026-04-30
**Linear:** [ZEB-211](https://linear.app/zeblith/issue/ZEB-211)
**Origin:** Surfaced during ZEB-206 brainstorm (2026-04-30) as a hard prerequisite for ZEB-215 Sub-A. The owner-state Prolly Tree CRDT replicates across an owner's bound devices via Zenoh; the topic is observable by anyone who knows the owner address. This spec defines the encryption scheme that prevents observers from reading the user's nav tree, DM outbox, or read markers.

## Goal

Define a concrete encryption scheme for the owner-state CRDT that:

1. Hides Space-entry contents and tree structure from any party without the owner master seed.
2. Lives entirely at the harmony-client application layer — no modifications to harmony-core (harmony-runtime, harmony-content, harmony-identity).
3. Preserves Prolly Tree CRDT semantics (per-entry granularity, structural sharing on DAG-sync).
4. Is implementable using the crypto primitives already in `src-tauri/Cargo.toml`: `chacha20poly1305`, `hkdf`, `sha2`, `blake3`.

## Threat model

**In scope:**

- Passive Zenoh-topic observer (knows the owner address, subscribes to `harmony/owner/{addr}/state-root-v1`)
- Active Zenoh-querier (issues DAG-sync queries against the owner's published CIDs)
- Compromised gateway / relay forwarding the topic
- Adversary who later obtains a leaked CAS blob without the master seed

**Out of scope:**

- Adversary with the master seed (this is full owner compromise — handled by ZEB-173 fresh-identity-on-total-loss; orthogonal to encryption)
- Adversary who has full read access to one of the owner's bound devices (also handled by ZEB-173)
- Forward secrecy under bound-device compromise (deferred — see Future Work; depends on "wipe master from device" landing in ZEB-197 follow-on)
- Side-channel attacks (timing, traffic analysis) on the topic — fundamentally unavoidable; observers see *that* the owner is publishing, never *what*

## Non-goals

- Forward secrecy in v1.
- Per-device key isolation (every bound device holds the master seed by design — per-device keys would not improve security; cf. ZEB-211 brainstorm Q2 alternatives).
- Encryption of DMs (Reticulum unicast handles DM privacy via transport-layer unlinkability — separate concern, see ZEB-216).
- Encryption of community-membership CRDTs (separate ticket scope; community state is multi-party shared and needs a different design).
- Hiding owner-address activity (impossible on Zenoh — the topic name leaks the address; mitigation via topic-name encryption is a future-work consideration).

## Architecture

Encryption is purely at the harmony-client application layer. Cleartext Space CBOR → encrypted bytes → handed to harmony-content for CAS storage. harmony-content is unmodified; it sees ciphertext blobs and computes BLAKE3 CIDs over them.

```
┌─────────────────────────────────────────────────────────────┐
│ harmony-client::owner_state_crdt                            │
│   ├─ encrypt_value()      ← ChaCha20-Poly1305 AEAD          │
│   ├─ derive_lookup_key()  ← HMAC-SHA256                      │
│   └─ derive_nonce()       ← BLAKE3-keyed-MAC                 │
├─────────────────────────────────────────────────────────────┤
│ harmony-content (unmodified, upstream harmony repo)         │
│   stores ciphertext blobs at BLAKE3(ciphertext) CIDs        │
├─────────────────────────────────────────────────────────────┤
│ Zenoh transport (vanilla)                                    │
│   topic: harmony/owner/{addr}/state-root-v1                 │
│   payload: AEAD(root-CID-pointer)                           │
└─────────────────────────────────────────────────────────────┘
```

## Key derivation tree

Three keys, all deterministically derived from the master seed (already exists per ZEB-173 + shared across bound devices per ZEB-197).

```
master_seed: [u8; 32]
   │
   └── HKDF-SHA256-Extract(IKM=master_seed,
                            salt=b"harmony-owner-state-v1-epoch-0")
        │
        └── PRK
             ├── HKDF-Expand(PRK, info=b"aead-key",     L=32) → owner_state_aead_key
             ├── HKDF-Expand(PRK, info=b"tree-lookup",  L=32) → owner_state_lookup_key
             └── HKDF-Expand(PRK, info=b"nonce-deriv",  L=32) → owner_state_nonce_key
```

### Salt versioning + epoch reservation

The salt embeds two version dimensions:

- `v1` is the wire-format version. Bump if the encryption scheme itself changes (e.g., switch primitives).
- `epoch-0` is reserved for future key rotation. v1 hard-codes `epoch-0`; bumping requires an offline re-encryption migration. **No active rotation in v1**, but the salt structure is forward-compatible.

When ZEB-197 grows a "wipe master from device" action, a separate followup will bump the epoch on every revocation. See Future Work.

### Implementation note

Use the `hkdf` crate (already in Cargo.toml):

```rust
use hkdf::Hkdf;
use sha2::Sha256;

let hk = Hkdf::<Sha256>::new(
    Some(b"harmony-owner-state-v1-epoch-0"),
    master_seed.as_ref(),
);
let mut aead_key = [0u8; 32];
hk.expand(b"aead-key", &mut aead_key)?;
let mut lookup_key = [0u8; 32];
hk.expand(b"tree-lookup", &mut lookup_key)?;
let mut nonce_key = [0u8; 32];
hk.expand(b"nonce-deriv", &mut nonce_key)?;
```

All three keys MUST be wrapped in `Zeroizing<[u8; 32]>` (the `zeroize` crate is already a dep) so they don't linger in freed memory.

## Per-entry encryption scheme

For each Space-entry write:

```
space_lookup_key = HMAC-SHA256(key=owner_state_lookup_key, message=space_id_bytes)  // 32 bytes; see "Tree-lookup-key scheme" below
cleartext_cbor   = serialize_cbor(space_entry)
nonce_12         = BLAKE3-keyed-MAC(key=owner_state_nonce_key, message=cleartext_cbor)[..12]
ciphertext       = ChaCha20Poly1305-encrypt(
                     key=owner_state_aead_key,
                     nonce=nonce_12,
                     aad=space_lookup_key,
                     plaintext=cleartext_cbor)
storage_blob     = nonce_12 || ciphertext
cipher_cid       = BLAKE3(storage_blob)
```

### Why deterministic nonce

ChaCha20-Poly1305 normally uses random nonces, but for owner-state we need **deterministic encryption**: the same cleartext + same key MUST produce the same ciphertext, so that the cipher-CID is stable across bound devices. Without that, two devices encrypting the same Space entry would produce different CIDs and the CRDT would treat them as conflicting writes.

The nonce is derived from the cleartext via `BLAKE3-keyed-MAC(nonce_key, cleartext)` — a 12-byte MAC truncation. This is safe because:

- Different cleartexts produce different nonces (cryptographic MAC, no observable collisions in practice).
- Same cleartext produces the same nonce → same ciphertext (intentional).
- An attacker cannot derive valid nonces without `owner_state_nonce_key`.

The "leak" of deterministic encryption — that observers can detect "two entries with identical ciphertext = identical cleartext" — is acceptable because Space-entry cleartexts naturally vary on every write (HLC `updated_at` advances). True repeats would only occur if the application wrote the same exact CBOR twice, which is a no-op the application should already deduplicate.

### Why AAD = space_lookup_key

Binding the ciphertext to its tree position via the Additional Authenticated Data field prevents a **relocation attack**: an adversary who somehow obtains a valid Space-A ciphertext cannot move it into Space-B's tree slot. The AEAD verification will fail because the AAD bound at encryption (Space-A's lookup key) won't match the AAD presented at decryption (Space-B's lookup key).

### Implementation note

```rust
use chacha20poly1305::{ChaCha20Poly1305, Nonce, KeyInit, AeadInPlace};
use blake3;

let cipher = ChaCha20Poly1305::new_from_slice(&aead_key)?;

// Derive nonce
let mut nonce_bytes = [0u8; 12];
let mut hasher = blake3::Hasher::new_keyed(&nonce_key);
hasher.update(&cleartext_cbor);
hasher.finalize_xof().fill(&mut nonce_bytes);
let nonce = Nonce::from_slice(&nonce_bytes);

// Encrypt in place (space_lookup_key derived per the "Tree-lookup-key scheme" section)
let mut buffer = cleartext_cbor;
cipher.encrypt_in_place(nonce, &space_lookup_key, &mut buffer)?;
let ciphertext = buffer;

// Storage blob
let mut storage_blob = Vec::with_capacity(12 + ciphertext.len());
storage_blob.extend_from_slice(&nonce_bytes);
storage_blob.extend_from_slice(&ciphertext);
let cipher_cid = blake3::hash(&storage_blob);
```

## Tree-lookup-key scheme

Prolly Tree keys (the keys used to find a Space in the tree) are MAC'd:

```
lookup_key_bytes = HMAC-SHA256(key=owner_state_lookup_key, message=space_id_bytes)
                                                                                   // 32 bytes
```

A passive observer of the tree structure sees opaque 32-byte lookup keys with no recoverable Space-ID information. Only a holder of `owner_state_lookup_key` can compute the lookup key for a given Space ID.

### Why HMAC instead of plain hash

Plain `BLAKE3(space_id)` would be a deterministic public hash — anyone could compute it and check whether owner X has joined community Y by querying for `BLAKE3(known_community_id)` in the tree. HMAC with the owner's secret lookup key prevents this enumeration attack.

### Implementation note

```rust
use hmac::{Hmac, Mac};
use sha2::Sha256;

let mut mac = Hmac::<Sha256>::new_from_slice(&owner_state_lookup_key)?;
mac.update(space_id_bytes);
let space_lookup_key = mac.finalize().into_bytes();
// 32 bytes; use as Prolly Tree key AND as AAD for AEAD encryption of this space's value
```

The `hmac` crate is not yet in Cargo.toml — needs to be added: `hmac = "0.12"`. Compatible with the existing `sha2 = "0.10"`.

## What observers see vs. don't

### Topic-subscriber-without-master-seed

Sees:

- The owner is publishing state-root updates (timing observable)
- Encrypted root-CID pointers as opaque AEAD blobs (24 bytes nonce+tag overhead + actual CID bytes)
- Update frequency (potentially correlatable with user activity — fundamental to pub/sub)

Cannot see:

- Tree contents
- Tree structure (how many spaces, what kinds, hierarchy)
- Any Space ID
- Any DM membership
- Read markers

### Topic-subscriber + DAG-sync-querier

Additionally fetches CAS blobs.

Sees:

- Opaque ciphertext blobs at unguessable CIDs
- Cannot enumerate (BLAKE3 CIDs are 32-byte unguessable; tree-lookup keys are HMACs)
- Cannot decrypt without `owner_state_aead_key`

### Bound device with master seed

Full read/write access. Same threat surface as ZEB-173 backup file unlocked: full owner crypto sovereignty.

## Wire format

### Zenoh topic

- Name: `harmony/owner/{addr_hex}/state-root-v1`
- Payload format:

```
state_root_publish = {
  encrypted_root: bytes,         // AEAD-encrypted current root CID + HLC stamp
}
```

The encrypted-root payload uses the same `owner_state_aead_key` with `aad = b"state-root-pointer"` (domain-separated from per-Space-entry encryption via different AAD).

### Storage blob format

Stored in harmony-content CAS at CID `BLAKE3(blob)`:

```
storage_blob = nonce(12 bytes) || ChaCha20-Poly1305-ciphertext-with-tag
```

Total overhead per encrypted Space entry: 12 (nonce) + 16 (Poly1305 tag) = 28 bytes.

### Tree key format

Prolly Tree keys are 32 bytes (SHA-256 output size). harmony-content's existing tree machinery handles 32-byte keys natively.

## Performance estimate

Per Space-entry write (typical 200-byte CBOR):

- HKDF-Extract + 3× HKDF-Expand: ~10 μs (one-time per session)
- BLAKE3-keyed-MAC (200 bytes): ~1 μs
- ChaCha20-Poly1305 encrypt (200 bytes): ~2 μs
- BLAKE3-hash (228 bytes for cipher-CID): ~1 μs
- HMAC-SHA256 (Space ID, ~16 bytes): ~1 μs

Total: ~5 μs per Space-entry write, after one-time ~10 μs key derivation. Negligible vs. CRDT bookkeeping.

## Migration path

### v1 → v2 (future)

If we later need to change primitives or rotate keys:

1. Bump salt version: `harmony-owner-state-v1-epoch-0` → `harmony-owner-state-v1-epoch-1` (rotation) or `harmony-owner-state-v2-...` (scheme change).
2. Re-encrypt all owner-state CBOR under new keys → new CIDs.
3. Publish new state-root pointer to a new topic version: `state-root-v2`.
4. Old subscribers continue reading old topic (eventually they migrate too); old CAS blobs are GC'd after a grace period.

This is **offline migration** — runs on bound-device boot, not in real time. Fine for v1 since rotation isn't actively triggered.

## Future work (followup tickets)

- **Forward secrecy via key chaining post-master-wipe.** When ZEB-197 grows a "wipe master from device" action, file a followup ticket to bump epoch on revocation events. The salt's `epoch-N` reservation is the schema hook.
- **Topic-name privacy.** Today the Zenoh topic name embeds the owner address, leaking "owner X is active" to any subscriber who knows the address. A followup could explore deriving a topic name from `HMAC(some_topic_key, addr)` so observers without `topic_key` can't even subscribe. Cost: breaks public discovery of "is this owner reachable?" if we later need it.
- **Side-channel hardening.** Currently update timing leaks user activity patterns. Could be mitigated by batching publishes on a fixed cadence + dummy traffic; cost is real-time convergence latency.

## Verification gates

Before ZEB-211 implementation lands (note: this spec is design-only; implementation rides on ZEB-215 Sub-A):

- All key-derivation, encryption, and decryption paths covered by Rust unit tests
- Round-trip test: encrypt 100 random Space entries, decrypt, verify cleartext match
- Stability test: encrypt the same cleartext on two simulated bound devices (same master seed), verify identical cipher-CIDs
- Negative test: ciphertext from Space-A cannot be decrypted as Space-B (AAD binding works)
- Negative test: Space ID enumeration via tree-lookup-key brute force is computationally infeasible (assert HMAC, not plain hash, used for lookup keys)
- `cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings` clean
- `cargo fmt --all -- --check` clean

## Acceptance criteria for this spec

- All five questions in the original ZEB-211 ticket description are answered with concrete primitive choices
- Threat model explicitly distinguishes in-scope vs out-of-scope adversaries
- All cryptographic operations specified to a level of detail an implementer can translate directly into Rust
- Performance estimate exists and is negligible vs. CRDT overhead
- Migration path defined for future scheme changes
- Followup tickets identified and listed for deferred concerns
