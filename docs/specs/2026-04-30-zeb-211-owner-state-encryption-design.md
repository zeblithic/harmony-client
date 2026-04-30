# ZEB-211: Owner-state Zenoh topic encryption design

**Date:** 2026-04-30
**Linear:** [ZEB-211](https://linear.app/zeblith/issue/ZEB-211)
**Origin:** Surfaced during ZEB-206 brainstorm (2026-04-30) as a hard prerequisite for ZEB-215 Sub-A. The owner-state Prolly Tree CRDT replicates across an owner's bound devices via Zenoh; the topic is observable by anyone who knows the owner address. This spec defines the encryption scheme that prevents observers from reading the user's nav tree, DM outbox, or read markers.

## Goal

Define a concrete encryption scheme for the owner-state CRDT that:

1. Hides Space-entry contents and tree structure from any party without the owner master seed.
2. Lives entirely at the harmony-client application layer — no modifications to harmony-core (harmony-runtime, harmony-content, harmony-identity).
3. Preserves Prolly Tree CRDT semantics (per-entry granularity, structural sharing on DAG-sync).
4. Is implementable using the crypto primitives `chacha20poly1305`, `hkdf`, `sha2`, `blake3` (already in `src-tauri/Cargo.toml`) plus one new dependency: `hmac = "0.12"` (compatible with the existing `sha2 = "0.10"`).

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

```text
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

Four keys, all deterministically derived from the master seed (already exists per ZEB-173 + shared across bound devices per ZEB-197).

```text
master_seed: [u8; 32]
   │
   └── HKDF-SHA256-Extract(IKM=master_seed,
                            salt=b"harmony-owner-state-v1-epoch-0")
        │
        └── PRK
             ├── HKDF-Expand(PRK, info=b"entry-aead-key", L=32) → owner_state_entry_aead_key
             ├── HKDF-Expand(PRK, info=b"root-aead-key",  L=32) → owner_state_root_aead_key
             ├── HKDF-Expand(PRK, info=b"tree-lookup",    L=32) → owner_state_lookup_key
             └── HKDF-Expand(PRK, info=b"nonce-deriv",    L=32) → owner_state_nonce_key
```

### Key separation: why two AEAD keys

Per-entry encryption uses deterministic nonces (BLAKE3-derived from cleartext + space_lookup_key). State-root publishes use random nonces (CSPRNG per publish). These two nonce strategies share a 12-byte nonce space. If they shared a single AEAD key, a deterministic per-entry nonce could (with vanishingly low but non-zero probability) collide with a random root nonce, producing nonce reuse under the same key — which catastrophically breaks ChaCha20-Poly1305 confidentiality (see "Why `space_lookup_key` MUST be in the nonce input" below for the underlying mechanic).

AAD does NOT provide effective domain separation here because Poly1305's `(r, s)` authenticator key derives from `(aead_key, nonce, counter=0)` only — AAD is not part of the keystream derivation. The clean fix is **separate AEAD keys per cryptographic context**: `owner_state_entry_aead_key` for per-entry encryption, `owner_state_root_aead_key` for state-root publishes. The two contexts now have entirely independent nonce spaces, eliminating the collision concern at design time.

### Salt versioning + epoch reservation

The salt embeds two version dimensions:

- `v1` is the wire-format version. Bump if the encryption scheme itself changes (e.g., switch primitives).
- `epoch-0` is reserved for future key rotation. v1 hard-codes `epoch-0`; bumping requires an offline re-encryption migration. **No active rotation in v1**, but the salt structure is forward-compatible.

When ZEB-197 grows a "wipe master from device" action, a separate follow-up will bump the epoch on every revocation. See Future Work.

### Implementation note

Use the `hkdf` crate (already in Cargo.toml):

```rust
use hkdf::Hkdf;
use sha2::Sha256;

let hk = Hkdf::<Sha256>::new(
    Some(b"harmony-owner-state-v1-epoch-0"),
    master_seed.as_ref(),
);
let mut entry_aead_key = [0u8; 32];
hk.expand(b"entry-aead-key", &mut entry_aead_key)?;
let mut root_aead_key = [0u8; 32];
hk.expand(b"root-aead-key", &mut root_aead_key)?;
let mut lookup_key = [0u8; 32];
hk.expand(b"tree-lookup", &mut lookup_key)?;
let mut nonce_key = [0u8; 32];
hk.expand(b"nonce-deriv", &mut nonce_key)?;
```

All four keys MUST be wrapped in `Zeroizing<[u8; 32]>` (the `zeroize` crate is already a dep) so they don't linger in freed memory.

## Canonical CBOR encoding (required)

All `serialize_cbor(...)` calls in this scheme MUST use **deterministic CBOR encoding** per RFC 8949 §4.2 ("Core Deterministic Encoding Requirements"):

- Bytewise lexicographic ordering of map keys (sort keys before encoding)
- Shortest-form integer encoding (no leading zeros)
- Definite-length collections only (no indefinite-length encoding)
- No CBOR tags on owner-state types
- Floats are not used in owner-state types (avoid float canonicalization edge cases)

This is **load-bearing** for the deterministic-encryption property below: the same logical Space entry MUST produce the same `cleartext_cbor` byte sequence on any bound device, otherwise nonces and cipher-CIDs diverge and CRDT convergence breaks. Use `ciborium` (or equivalent) with deterministic mode explicitly enabled. Verify via the cross-encoder gate in [Verification gates](#verification-gates).

## Per-entry encryption scheme

For each Space-entry write:

```text
space_lookup_key = HMAC-SHA256(key=owner_state_lookup_key, message=space_id_bytes)  // 32 bytes; see "Tree-lookup-key scheme" below
cleartext_cbor   = serialize_cbor(space_entry)                                       // canonical CBOR; see above
nonce_12         = BLAKE3-keyed-MAC(
                     key=owner_state_nonce_key,
                     message=b"owner-state-entry-v1" || space_lookup_key || cleartext_cbor
                   )[..12]
ciphertext       = ChaCha20Poly1305-encrypt(
                     key=owner_state_entry_aead_key,
                     nonce=nonce_12,
                     aad=space_lookup_key,
                     plaintext=cleartext_cbor)
storage_blob     = nonce_12 || ciphertext_with_tag
cipher_cid       = BLAKE3(storage_blob)
```

### Why deterministic nonce

ChaCha20-Poly1305 normally uses random nonces, but for owner-state we need **deterministic encryption**: the same cleartext + same key MUST produce the same ciphertext, so that the cipher-CID is stable across bound devices. Without that, two devices encrypting the same Space entry would produce different CIDs and the CRDT would treat them as conflicting writes.

The nonce is derived from `b"owner-state-entry-v1" || space_lookup_key || cleartext_cbor` via a keyed BLAKE3 MAC truncated to 12 bytes. This is safe and sufficient because:

- **Different (space, cleartext) pairs produce different nonces.** Cross-space nonce reuse is prevented because `space_lookup_key` (which is per-space, per the Tree-lookup-key scheme) is mixed into the nonce derivation.
- **Same (space, cleartext) pair produces the same nonce → same ciphertext** (intentional, deterministic across all bound devices).
- **An attacker cannot derive valid nonces** without `owner_state_nonce_key` (a keyed MAC, not a public hash).
- **Domain-separation prefix** `b"owner-state-entry-v1"` versions the construction and prevents collisions with any future MAC computation that happens to take the same inputs.

### Why `space_lookup_key` MUST be in the nonce input (not just AAD)

ChaCha20-Poly1305's Poly1305 authenticator key `(r, s)` is derived from `(aead_key, nonce, counter=0)` only — **AAD is not part of the (r, s) derivation**. If two different Space entries end up with the same `cleartext_cbor` (a pathological but possible application bug), and the nonce were derived only from the cleartext, both encryptions would use identical `(owner_state_entry_aead_key, nonce)` pairs. ChaCha20 would generate the same keystream, and XOR'ing the two ciphertexts would reveal the XOR of plaintexts — full confidentiality break, regardless of AAD differences.

Mixing `space_lookup_key` into the nonce derivation makes nonces a function of `(space, cleartext)`, so cross-space cleartext collisions can never collide nonces. Determinism for the same `(space, cleartext)` pair is preserved (CRDT convergence still works).

### Why AAD = `space_lookup_key`

Even with the nonce binding above, AAD is still useful: binding ciphertext to its tree position via AAD prevents a **relocation attack**, where an adversary who obtains a valid Space-A ciphertext cannot move it into Space-B's tree slot. The AEAD verification fails because the AAD bound at encryption (Space-A's lookup key) won't match the AAD presented at decryption (Space-B's lookup key). This is defense-in-depth on top of the nonce binding.

### Implementation note

```rust
use chacha20poly1305::{ChaCha20Poly1305, Nonce, KeyInit, AeadInPlace};
use blake3;

let cipher = ChaCha20Poly1305::new_from_slice(&entry_aead_key)?;

// Derive nonce — bind to space_lookup_key to prevent cross-space nonce reuse
let mut nonce_bytes = [0u8; 12];
let mut hasher = blake3::Hasher::new_keyed(&nonce_key);
hasher.update(b"owner-state-entry-v1");
hasher.update(&space_lookup_key);
hasher.update(&cleartext_cbor);
hasher.finalize_xof().fill(&mut nonce_bytes);
let nonce = Nonce::from_slice(&nonce_bytes);

// Encrypt in place (space_lookup_key derived per the "Tree-lookup-key scheme" section)
let mut buffer = cleartext_cbor;
cipher.encrypt_in_place(nonce, &space_lookup_key, &mut buffer)?;
let ciphertext_with_tag = buffer;  // includes appended Poly1305 tag

// Storage blob: nonce(12) || ciphertext_with_tag
let mut storage_blob = Vec::with_capacity(12 + ciphertext_with_tag.len());
storage_blob.extend_from_slice(&nonce_bytes);
storage_blob.extend_from_slice(&ciphertext_with_tag);
let cipher_cid = blake3::hash(&storage_blob);
```

## Tree-lookup-key scheme

Prolly Tree keys (the keys used to find a Space in the tree) are MAC'd:

```text
space_lookup_key = HMAC-SHA256(key=owner_state_lookup_key, message=space_id_bytes)
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
- Encrypted root-CID pointers as opaque AEAD blobs (28 bytes nonce+tag overhead — 12 nonce + 16 Poly1305 tag — + canonical-CBOR plaintext bytes)
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
- Cannot decrypt without `owner_state_entry_aead_key` (per-entry CAS blobs) or `owner_state_root_aead_key` (state-root publishes)

### Bound device with master seed

Full read/write access. Same threat surface as ZEB-173 backup file unlocked: full owner crypto sovereignty.

## Wire format

### Zenoh topic

- Name: `harmony/owner/{addr_hex}/state-root-v1`
- Payload byte layout (the entire Zenoh payload, no outer framing):

```text
state_root_publish_bytes = nonce(12) || ChaCha20-Poly1305-ciphertext-with-tag
```

Where the **plaintext** is the canonical-CBOR encoding of:

```text
state_root_payload = {
  "root_cid": bstr,              // 32 bytes; BLAKE3 of the owner-state Prolly Tree root block
  "at":       HLC,               // publishing device's HLC at the moment of this publish
}
```

(`HLC` is a CBOR map with sorted keys: `{"wall_ms": uint, "logical": uint, "device_id": tstr}`. Canonical-CBOR rules apply.)

**Nonce generation:** cryptographically-random 12 bytes from the OS CSPRNG, fresh per publish. Determinism is **not** required for root publishes — they are pub/sub events, not content-addressed (different from per-Space-entry encryption above). Random nonces eliminate any chance of cross-publish nonce reuse.

**Receiver parsing:** read the first 12 bytes as the nonce; the remainder is the AEAD ciphertext-with-tag. Decrypt with `owner_state_root_aead_key` and `aad = b"state-root-pointer"`. On AEAD failure, drop the publish (could be unauthenticated injection).

**AEAD parameters:**

- Key: `owner_state_root_aead_key` (separate from `owner_state_entry_aead_key` — see "Key separation: why two AEAD keys" above)
- AAD: `b"state-root-pointer"`
- Nonce: random per publish (above)

### Storage blob format

Stored in harmony-content CAS at CID `BLAKE3(blob)`:

```text
storage_blob = nonce(12 bytes) || ChaCha20-Poly1305-ciphertext-with-tag
```

Total overhead per encrypted Space entry: 12 (nonce) + 16 (Poly1305 tag) = 28 bytes.

### Tree key format

Prolly Tree keys are 32 bytes (SHA-256 output size). harmony-content's existing tree machinery handles 32-byte keys natively.

## Performance estimate

Per Space-entry write (typical 200-byte CBOR):

- HKDF-Extract + 4× HKDF-Expand: ~12 μs (one-time per session)
- BLAKE3-keyed-MAC (200 bytes): ~1 μs
- ChaCha20-Poly1305 encrypt (200 bytes): ~2 μs
- BLAKE3-hash (228 bytes for cipher-CID): ~1 μs
- HMAC-SHA256 (Space ID, ~16 bytes): ~1 μs

Total: ~5 μs per Space-entry write, after the one-time ~12 μs key derivation. Negligible vs. CRDT bookkeeping.

## Migration path

### v1 → v2 (future)

If we later need to change primitives or rotate keys:

1. Bump salt version: `harmony-owner-state-v1-epoch-0` → `harmony-owner-state-v1-epoch-1` (rotation) or `harmony-owner-state-v2-...` (scheme change).
2. Re-encrypt all owner-state CBOR under new keys → new CIDs.
3. Publish new state-root pointer to a new topic version: `state-root-v2`.
4. Old subscribers continue reading old topic (eventually they migrate too); old CAS blobs are GC'd after a grace period.

This is **offline migration** — runs on bound-device boot, not in real time. Fine for v1 since rotation isn't actively triggered.

## Future work (follow-up tickets)

- **Forward secrecy via key chaining post-master-wipe.** When ZEB-197 grows a "wipe master from device" action, file a follow-up ticket to bump epoch on revocation events. The salt's `epoch-N` reservation is the schema hook.
- **Topic-name privacy.** Today the Zenoh topic name embeds the owner address, leaking "owner X is active" to any subscriber who knows the address. A follow-up could explore deriving a topic name from `HMAC(some_topic_key, addr)` so observers without `topic_key` can't even subscribe. Cost: breaks public discovery of "is this owner reachable?" if we later need it.
- **Side-channel hardening.** Currently update timing leaks user activity patterns. This could be mitigated by batching publishes on a fixed cadence plus dummy traffic; the cost is increased real-time convergence latency.

## Verification gates

Before ZEB-211 implementation lands (note: this spec is design-only; implementation rides on ZEB-215 Sub-A):

- All key-derivation, encryption, and decryption paths covered by Rust unit tests
- Round-trip test: encrypt 100 random Space entries, decrypt, verify cleartext match
- Stability test: encrypt the same cleartext on two simulated bound devices (same master seed), verify identical cipher-CIDs
- **Canonical-CBOR cross-encoder test:** serialize the same logical `space_entry` value with two independent encoder configurations (or two separate processes); assert byte-identical CBOR output. Load-bearing for deterministic encryption.
- **Cross-space nonce-binding test:** craft two Space entries A and B with identical `cleartext_cbor` (artificial — bypass HLC variance for the test) but different `space_id`s; assert their derived `nonce_12` values differ. Verifies the nonce-binding fix to the cross-space nonce-reuse vulnerability.
- Negative test: ciphertext from Space-A cannot be decrypted as Space-B (AAD binding works)
- Negative test: Space ID enumeration via tree-lookup-key brute force is computationally infeasible (assert HMAC, not plain hash, used for lookup keys)
- Round-trip test for state-root publish: encrypt random `(root_cid, HLC)` payloads with random nonces and `aad=b"state-root-pointer"`, decrypt, verify cleartext match
- `cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings` clean
- `cargo fmt --all -- --check` clean

## Acceptance criteria for this spec

- All five questions in the original ZEB-211 ticket description are answered with concrete primitive choices
- Threat model explicitly distinguishes in-scope vs out-of-scope adversaries
- All cryptographic operations specified to a level of detail an implementer can translate directly into Rust
- Performance estimate exists and is negligible vs. CRDT overhead
- Migration path defined for future scheme changes
- Followup tickets identified and listed for deferred concerns
