# Friends Phase 1b (ZEB-371) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give friends durable cross-WAN rendezvous: establish + store a per-friendship secret at link time, publish/resolve each friend's live reachability under a private Case-D pkarr slot on the existing cadence, and add Path A (mutual-key) with hybrid consent.

**Architecture:** Ephemeral X25519 ECDH inside the `harmony/friend/v1` handshake → 32-byte `friendship_secret`, KeyTree-sealed in `FriendEntry`, synced across the owner's devices. The secret keys a direction-specific `PkarrCase::Friend` DHT slot (`HKDF(secret, epoch ‖ publisher_owner_id)`); the resolver reverses it. Path A reuses the same handshake without a token, gated by an auto-accept-known / prompt-new consent tree.

**Tech Stack:** Rust (tauri `src-tauri/`), `x25519-dalek 2`, `chacha20poly1305`, `hkdf`, `harmony-pkarr` (harmony-core, git dep), Svelte 5 frontend.

**Spec:** `docs/specs/2026-06-04-zeb-371-friends-phase-1b-design.md`.

**Gates (run from `src-tauri/`):**
- Test: `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- Lint: `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- Fmt: `cargo fmt --all -- --check`
- Frontend (repo root): `npx tsc --noEmit` + `npx vitest run`
- Single test: `cargo nextest run --locked --features test-fixtures -E 'test(NAME)'`

> Known flake: UDP-port-4242 contention (`os error 10048`) in iroh/zenoh integration tests under parallelism — NOT a regression; rerun the affected `*_integration` test serially with `-j 1` to confirm.

---

## File structure

**harmony-core (`C:/zeblith/work/zeblithic/harmony`):**
- Modify `crates/harmony-pkarr/src/derive.rs` — add `PkarrCase::Friend` + salt + reference vector.

**harmony-client (`src-tauri/`):**
- Modify `Cargo.toml` — re-pin `harmony-pkarr` rev (both dep lines).
- Create `src/friend_rendezvous.rs` — ephemeral-ECDH secret derivation + Case-D key/payload helpers.
- Modify `src/owner_state_crypto.rs` — `KeyTree.friend_aead` sub-key + `encrypt_friend_secret`/`decrypt_friend_secret`.
- Modify `src/friend_graph.rs` — `FriendEntry.sealed_secret` field.
- Modify `src/iroh_friend_acceptor.rs` — wire types (`token_sig: Option`, `eph_x25519_pub`), preimages, `process_friend_request` (derive+seal+store), Path A decision tree + pending store.
- Modify `src/lib.rs` — redeem path secret threading; KeyTree injection; Path A IPCs (`add_friend_by_key`, `accept`/`decline`/`list_pending_friend_requests`); `friend_auto_accept_known` setting.
- Create `src/pkarr_friend_publisher.rs` — Case-D publisher + resolver.
- Modify `src/reachability_publisher.rs` wiring (in `lib.rs` event loop) — register/refresh/drop Case-D handles.
- Modify `src/friend-service.ts`, `src/lib/components/FriendsPanel.svelte`, add a requests UI — frontend.
- Modify `tests/wire_format_zeb370_fixtures.rs` — re-pin changed wire shapes; add Case-D key vector.

---

## Phase 0 — harmony-core: `PkarrCase::Friend`

### Task 1: Add `PkarrCase::Friend` to harmony-pkarr

**Files:**
- Modify: `C:/zeblith/work/zeblithic/harmony/crates/harmony-pkarr/src/derive.rs`

> **Cross-repo:** this is in the `harmony` repo. The variant must be **committed and pushed** so harmony-client's Cargo git-rev dep can fetch it. Work on a branch `zeb-371-pkarr-case-friend`.

- [ ] **Step 1: Add the failing reference-vector + distinctness tests.** In `derive.rs` `mod tests`, add:

```rust
#[test]
fn reference_vector_case_friend() {
    // ikm = 32 zero bytes (placeholder friendship_secret)
    // info = epoch_id 12345 BE ‖ 16-byte owner_id of 0x11
    let ikm = [0u8; 32];
    let mut info = Vec::new();
    info.extend_from_slice(&12345u64.to_be_bytes());
    info.extend_from_slice(&[0x11u8; 16]);
    let key = derive_ephemeral_key(PkarrCase::Friend, &ikm, &info);
    let vk_hex = hex::encode(key.verifying_key().to_bytes());
    // Pin: compute once (run the test, paste the actual), then lock it.
    let expected = "PLACEHOLDER_RUN_ONCE";
    assert_eq!(vk_hex, expected, "case-friend v1 keying must not drift");
}
```

Also extend `different_cases_produce_different_keys` to derive `k4 = derive_ephemeral_key(PkarrCase::Friend, &ikm, &info)` and assert `k4.verifying_key()` differs from `k1`/`k2`/`k3`.

- [ ] **Step 2: Run — expect a compile error** (`PkarrCase::Friend` doesn't exist): `cd C:/zeblith/work/zeblithic/harmony && cargo test -p harmony-pkarr friend`.

- [ ] **Step 3: Add the variant.** In `derive.rs`:

```rust
pub enum PkarrCase {
    Invite,
    Identity,
    Community,
    /// Case D: friend-scoped rendezvous. `ikm` = the 32-byte per-friendship
    /// secret (ZEB-371); `info` = `epoch_be ‖ publisher_owner_id`. Derived
    /// from a SHARED SECRET (not a public key) — the slot is findable only by
    /// the friend who holds the secret.
    Friend,
}
```

In `impl PkarrCase::salt`: `Self::Friend => b"harmony.pkarr.v1.friend",`.

- [ ] **Step 4: Run to get the real vector, paste it into `expected`, re-run green.** `cargo test -p harmony-pkarr friend` (read the assert-left value, replace `PLACEHOLDER_RUN_ONCE`, re-run). Then the full crate: `cargo test -p harmony-pkarr`.

- [ ] **Step 5: Commit + push the harmony branch.**

```bash
cd C:/zeblith/work/zeblithic/harmony
git checkout -b zeb-371-pkarr-case-friend
git add crates/harmony-pkarr/src/derive.rs
git commit -m "feat(pkarr): add PkarrCase::Friend (ZEB-371 Case-D rendezvous)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
git push -u origin zeb-371-pkarr-case-friend
git rev-parse HEAD   # capture SHA for Task 2
```

> Report the pushed SHA back to the controller — Task 2 pins to it. (A separate harmony PR reviews/merges this; re-pin to the merged SHA before the harmony-client PR merges.)

### Task 2: Re-pin `harmony-pkarr` in harmony-client

**Files:**
- Modify: `src-tauri/Cargo.toml:87` and `:149` (both `harmony-pkarr` lines)

- [ ] **Step 1: Update both rev pins** to the SHA from Task 1 (base dep line + the `test-fixtures` line — they MUST match):

```toml
harmony-pkarr = { git = "https://github.com/zeblithic/harmony", rev = "<SHA_FROM_TASK_1>" }
# ...and the dev/test line:
harmony-pkarr = { git = "https://github.com/zeblithic/harmony", rev = "<SHA_FROM_TASK_1>", features = ["test-fixtures"] }
```

- [ ] **Step 2: Update the lockfile + verify the variant is visible.**

```bash
cd src-tauri && cargo update -p harmony-pkarr --precise <SHA_FROM_TASK_1>
cargo check --locked --all-targets --features test-fixtures
```

- [ ] **Step 3: Commit.**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "build(zeb-371): re-pin harmony-pkarr for PkarrCase::Friend"
```

---

## Phase 1 — Secret establishment

### Task 3: `friendship_secret` derivation (ephemeral ECDH + HKDF)

**Files:**
- Create: `src-tauri/src/friend_rendezvous.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod friend_rendezvous;`)

- [ ] **Step 1: Write the failing tests** in `friend_rendezvous.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::OwnerAddr;

    #[test]
    fn both_sides_derive_identical_secret() {
        let (a_sk, a_pub) = generate_ephemeral();
        let (b_sk, b_pub) = generate_ephemeral();
        let owner_a = OwnerAddr([0x11; 16]);
        let owner_b = OwnerAddr([0x22; 16]);
        // A derives with its sk + B's pub; B with its sk + A's pub. Owners may
        // be passed in either order — sorted internally.
        let s_a = derive_friendship_secret(a_sk, &b_pub, owner_a, owner_b);
        let s_b = derive_friendship_secret(b_sk, &a_pub, owner_b, owner_a);
        assert_eq!(s_a.as_ref(), s_b.as_ref());
    }

    #[test]
    fn owner_order_does_not_change_secret() {
        let (a_sk, _a_pub) = generate_ephemeral();
        let (_b_sk, b_pub) = generate_ephemeral();
        let s1 = derive_friendship_secret(a_sk, &b_pub, OwnerAddr([1; 16]), OwnerAddr([2; 16]));
        let (a_sk2, _) = ephemeral_from_seed(&[7u8; 32]); // deterministic re-derive helper for the test
        let s2 = derive_friendship_secret(a_sk2, &b_pub, OwnerAddr([2; 16]), OwnerAddr([1; 16]));
        // Can't reuse a_sk (consumed); assert order-independence via fixed inputs instead:
        let _ = (s1, s2); // replaced by the deterministic check below
    }

    #[test]
    fn distinct_ephemerals_distinct_secret() {
        let (a_sk, _) = generate_ephemeral();
        let (_, b_pub) = generate_ephemeral();
        let (_, c_pub) = generate_ephemeral();
        let owner_a = OwnerAddr([1; 16]);
        let owner_b = OwnerAddr([2; 16]);
        let s_ab = derive_friendship_secret(a_sk, &b_pub, owner_a, owner_b);
        let (a_sk2, _) = generate_ephemeral();
        let s_ac = derive_friendship_secret(a_sk2, &c_pub, owner_a, owner_b);
        assert_ne!(s_ab.as_ref(), s_ac.as_ref());
    }
}
```

> Note for the implementer: drop the awkward `owner_order_does_not_change_secret` body above and instead prove order-independence directly on the `info` builder — extract a `fn rendezvous_info(a: OwnerAddr, b: OwnerAddr) -> [u8;32]` and assert `rendezvous_info(x,y) == rendezvous_info(y,x)`. Keep `both_sides_derive_identical_secret` and `distinct_ephemerals_distinct_secret` as the ECDH round-trip checks.

- [ ] **Step 2: Run — expect fail** (module/functions absent): `cargo nextest run --features test-fixtures -E 'test(friend_rendezvous)'`.

- [ ] **Step 3: Implement** the secret-derivation half of `friend_rendezvous.rs`:

```rust
//! ZEB-371 Phase 1b: per-friendship rendezvous secret (ephemeral X25519 ECDH)
//! and Case-D pkarr key/payload derivation.

use crate::owner_state_types::OwnerAddr;
use hkdf::Hkdf;
use rand::rngs::OsRng;
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey};
use zeroize::Zeroizing;

const FRIENDSHIP_SECRET_SALT: &[u8] = b"harmony.friend.v1.rendezvous";

/// Generate a single-use ephemeral X25519 keypair for one handshake.
/// The secret is consumed by [`derive_friendship_secret`]; only the 32-byte
/// public half is sent on the wire.
pub fn generate_ephemeral() -> (EphemeralSecret, [u8; 32]) {
    let sk = EphemeralSecret::random_from_rng(OsRng);
    let pk = PublicKey::from(&sk);
    (sk, pk.to_bytes())
}

/// `info` for the HKDF: the two owner_ids sorted so both parties compute the
/// same value regardless of who is requester/accepter.
fn rendezvous_info(a: OwnerAddr, b: OwnerAddr) -> [u8; 32] {
    let (lo, hi) = if a.0 <= b.0 { (a.0, b.0) } else { (b.0, a.0) };
    let mut info = [0u8; 32];
    info[..16].copy_from_slice(&lo);
    info[16..].copy_from_slice(&hi);
    info
}

/// Derive the shared 32-byte friendship secret. `my_eph` is consumed (one-shot).
/// Binds to the two authenticated owner identities via the HKDF `info`.
pub fn derive_friendship_secret(
    my_eph: EphemeralSecret,
    their_eph_pub: &[u8; 32],
    owner_a: OwnerAddr,
    owner_b: OwnerAddr,
) -> Zeroizing<[u8; 32]> {
    let shared = my_eph.diffie_hellman(&PublicKey::from(*their_eph_pub));
    let hk = Hkdf::<Sha256>::new(Some(FRIENDSHIP_SECRET_SALT), shared.as_bytes());
    let mut out = Zeroizing::new([0u8; 32]);
    hk.expand(&rendezvous_info(owner_a, owner_b), out.as_mut())
        .expect("HKDF-SHA256 always produces 32 bytes");
    out
}
```

- [ ] **Step 4: Run green** (`-E 'test(friend_rendezvous)'`), then `cargo clippy --features test-fixtures`.

- [ ] **Step 5: Commit.** `feat(zeb-371): per-friendship rendezvous secret via ephemeral X25519 ECDH`

### Task 4: KeyTree friend-secret seal/open

**Files:**
- Modify: `src-tauri/src/owner_state_crypto.rs`

- [ ] **Step 1: Write failing tests** in the `owner_state_crypto.rs` `mod tests`:

```rust
#[test]
fn friend_secret_seal_round_trip() {
    let kt = KeyTree::derive(&TEST_SEED).expect("derive");
    let friend = [0x33u8; 16];
    let secret = [0xABu8; 32];
    let blob = encrypt_friend_secret(&kt, &friend, &secret).expect("seal");
    assert_ne!(&blob[12..44], &secret[..], "must be ciphertext, not plaintext");
    let back = decrypt_friend_secret(&kt, &friend, &blob).expect("open");
    assert_eq!(back.as_ref(), &secret);
}

#[test]
fn friend_secret_seal_binds_to_friend_owner_id() {
    let kt = KeyTree::derive(&TEST_SEED).expect("derive");
    let blob = encrypt_friend_secret(&kt, &[1u8; 16], &[7u8; 32]).expect("seal");
    // Opening under a different friend owner_id (AAD) must fail.
    assert!(matches!(
        decrypt_friend_secret(&kt, &[2u8; 16], &blob),
        Err(CryptoError::AeadDecrypt)
    ));
}

#[test]
fn friend_aead_key_distinct_from_other_subkeys() {
    let kt = KeyTree::derive(&TEST_SEED).expect("derive");
    assert_ne!(kt.friend_aead.as_ref(), kt.entry_aead.as_ref());
    assert_ne!(kt.friend_aead.as_ref(), kt.root_aead.as_ref());
    assert_ne!(kt.friend_aead.as_ref(), kt.lookup.as_ref());
    assert_ne!(kt.friend_aead.as_ref(), kt.nonce.as_ref());
}
```

- [ ] **Step 2: Run — expect fail** (`friend_aead`/`encrypt_friend_secret` absent): `cargo nextest run --features test-fixtures -E 'test(friend_secret) + test(friend_aead)'`.

- [ ] **Step 3: Implement.** In `owner_state_crypto.rs`:
  - Add `const INFO_FRIEND_AEAD: &[u8] = b"friend-secret-aead-key";`
  - Add field `friend_aead: Zeroizing<[u8; 32]>` to `KeyTree`; in `derive()` expand it (mirror the other four).
  - Add the seal/open pair (random nonce, AAD = domain ‖ friend owner_id, exact-length blob check):

```rust
const AAD_FRIEND_SECRET: &[u8] = b"friend-rendezvous-secret";

/// Seal a 32-byte friendship secret for storage in a `FriendEntry`. AAD binds
/// the ciphertext to the specific friendship so a sealed secret cannot be
/// relocated into another friend's entry. Random nonce; layout
/// `nonce(12) ‖ ct(32+16)`.
pub fn encrypt_friend_secret(
    keys: &KeyTree,
    friend_owner_id: &[u8; 16],
    secret: &[u8; 32],
) -> Result<Vec<u8>, CryptoError> {
    let mut nonce_bytes = [0u8; 12];
    OsRng.try_fill_bytes(&mut nonce_bytes).map_err(|e| CryptoError::Rng(e.to_string()))?;
    let cipher = ChaCha20Poly1305::new_from_slice(keys.friend_aead.as_ref())
        .expect("ChaCha20-Poly1305 accepts a 32-byte key");
    let mut aad = AAD_FRIEND_SECRET.to_vec();
    aad.extend_from_slice(friend_owner_id);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes),
            chacha20poly1305::aead::Payload { msg: secret, aad: &aad })
        .map_err(|_| CryptoError::AeadEncrypt)?;
    let mut blob = Vec::with_capacity(12 + ct.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ct);
    Ok(blob)
}

/// Open a blob produced by [`encrypt_friend_secret`].
pub fn decrypt_friend_secret(
    keys: &KeyTree,
    friend_owner_id: &[u8; 16],
    blob: &[u8],
) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    if blob.len() != 12 + 32 + 16 {
        return Err(CryptoError::AeadDecrypt);
    }
    let (nonce_bytes, ct) = blob.split_at(12);
    let cipher = ChaCha20Poly1305::new_from_slice(keys.friend_aead.as_ref())
        .expect("ChaCha20-Poly1305 accepts a 32-byte key");
    let mut aad = AAD_FRIEND_SECRET.to_vec();
    aad.extend_from_slice(friend_owner_id);
    let pt = cipher
        .decrypt(Nonce::from_slice(nonce_bytes),
            chacha20poly1305::aead::Payload { msg: ct, aad: &aad })
        .map_err(|_| CryptoError::AeadDecrypt)?;
    let mut out = Zeroizing::new([0u8; 32]);
    out.copy_from_slice(&pt);
    Ok(out)
}
```

- [ ] **Step 4: Run green** + update the existing `key_tree_derives_four_distinct_keys_deterministically` test to also assert `kt1.friend_aead == kt2.friend_aead` (deterministic) so the new key is covered there too. Re-run `-E 'test(owner_state_crypto)'`.

- [ ] **Step 5: Commit.** `feat(zeb-371): KeyTree friend-secret seal/open (at-rest protection)`

### Task 5: `FriendEntry.sealed_secret` field + clear-on-revoke

**Files:**
- Modify: `src-tauri/src/friend_graph.rs`
- Modify: `src-tauri/src/lib.rs` (`unfriend_inner` tombstone builder ~`:33227`)

- [ ] **Step 1: Write failing tests** in `friend_graph.rs` `mod tests`:

```rust
#[test]
fn friend_entry_sealed_secret_round_trips() {
    let mut e = sample_entry();
    e.sealed_secret = Some(vec![0xCD; 12 + 32 + 16]); // fixed opaque blob
    let bytes = canonical_cbor_encode(&e).expect("encode");
    let back: FriendEntry = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(e, back);
}

#[test]
fn friend_entry_absent_sealed_secret_decodes_as_none() {
    // Back-compat: a Phase-1 entry (no "k" key) decodes with sealed_secret None.
    let e = sample_entry(); // sample_entry sets sealed_secret: None
    let bytes = canonical_cbor_encode(&e).expect("encode");
    let back: FriendEntry = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(back.sealed_secret, None);
}
```

Update `sample_entry()` to set `sealed_secret: None`.

- [ ] **Step 2: Run — expect fail** (`sealed_secret` field absent): `-E 'test(friend_entry_sealed) + test(friend_entry_absent)'`.

- [ ] **Step 3: Implement.** Add to `FriendEntry` (after `learned_at`), wire key `"k"`, opaque bytes (NOT the bstr-newtype helpers — it's variable-length):

```rust
    /// ZEB-371: the per-friendship rendezvous secret, KeyTree-sealed
    /// (`owner_state_crypto::encrypt_friend_secret`). `None` for legacy
    /// Phase-1 entries and `Pending`/`Revoked` entries. Opaque bytes; never
    /// stored or logged in the clear.
    #[serde(rename = "k", skip_serializing_if = "Option::is_none", default)]
    pub sealed_secret: Option<Vec<u8>>,
```

Update every in-crate `FriendEntry { .. }` literal to add `sealed_secret: <None|Some(..)>` (the compiler lists them: `iroh_friend_acceptor.rs` `process_friend_request`, `lib.rs` `unfriend_inner` tombstone + test helpers, `friend_token`/integration tests). For the `unfriend_inner` tombstone (`lib.rs:33227`), set **`sealed_secret: None`** (clear the secret on revoke) — add a one-line comment citing spec §3.3.

- [ ] **Step 4: Run green:** `-E 'test(friend_graph) + test(unfriend)'`, then full `friend` scope. (`process_friend_request` callers still pass `None` until Task 7 — green.)

- [ ] **Step 5: Commit.** `feat(zeb-371): FriendEntry.sealed_secret field + clear-on-revoke`

### Task 6: Handshake wire types — ephemeral key + optional token + preimage

**Files:**
- Modify: `src-tauri/src/iroh_friend_acceptor.rs` (wire types, preimages, encode/decode)
- Modify: `src-tauri/src/lib.rs` (redeem path call sites — compile-only update here)
- Modify: `src-tauri/tests/wire_format_zeb370_fixtures.rs` (re-pin shapes)

> One wire change carries BOTH `token_sig: Option` and `eph_x25519_pub` so fixtures are re-pinned once. Path A's `None` token isn't *produced* until Task 9, but the shape lands now.

- [ ] **Step 1: Update the round-trip + preimage tests** in `iroh_friend_acceptor.rs` `mod tests` to construct the new fields. Add an explicit:

```rust
#[test]
fn request_preimage_binds_ephemeral_key() {
    let eph = [0x42u8; 32];
    let p1 = friend_request_sig_preimage(OwnerAddr([1; 16]), Some(&[9u8; 64]), &eph);
    let mut eph2 = eph; eph2[0] ^= 1;
    let p2 = friend_request_sig_preimage(OwnerAddr([1; 16]), Some(&[9u8; 64]), &eph2);
    assert_ne!(p1, p2, "a different ephemeral key must change the signed preimage");
    // None vs Some(token) must also differ.
    let p3 = friend_request_sig_preimage(OwnerAddr([1; 16]), None, &eph);
    assert_ne!(p1, p3);
}
```

Update `signed_request(..)` helper to generate an ephemeral (`crate::friend_rendezvous::generate_ephemeral`), put its pub in the request, and sign `friend_request_sig_preimage(owner, Some(&token_sig), &eph_pub)`.

- [ ] **Step 2: Run — expect fail** (signatures changed): `-E 'test(friend_request) + test(friend_accepted) + test(request_preimage)'`.

- [ ] **Step 3: Implement the wire/preimage changes:**
  - `FriendLinkRequest`: change `token_sig: [u8;64]` → `token_sig: Option<[u8; 64]>` (serde: keep bstr-on-`Some` via a helper, or store as `Option<serde_bytes::ByteArray<64>>` — simplest: a small module that (de)serializes `Option<[u8;64]>` as an optional bstr; `skip_serializing_if = Option::is_none`, `default`). Add `eph_x25519_pub: [u8; 32]` (bstr via the existing `serialize_bytes_as_bstr`/`deserialize_bytes_from_bstr`).
  - `FriendLinkAccepted`: add `eph_x25519_pub: [u8; 32]` (bstr).
  - Preimage fns → `(from_addr, token_sig: Option<&[u8;64]>, eph_x25519_pub: &[u8;32])`. Update the `Preimage` CBOR struct: `token_sig: Option<&[u8]>` (serde_bytes) + `eph: &[u8]`. Keep `"hfr1"`/`"hfa1"` domain tags.
  - Update `encode/decode` unchanged (struct-driven). Strict-decode/trailing-bytes/cap behavior unchanged.

- [ ] **Step 4: Update the redeem-path call site** in `lib.rs` (`connectivity_link_friend_iroh_inner`, ~`:32711`) so it COMPILES (full secret threading is Task 7): generate an ephemeral, send its pub, wrap `token_sig` as `Some(payload.token.sig)`, and pass `&eph_pub` to the preimage. Hold the `EphemeralSecret` in a local for Task 7. The accept it reads now has an `eph_x25519_pub` field (ignored until Task 7).

- [ ] **Step 5: Re-pin wire fixtures.** In `tests/wire_format_zeb370_fixtures.rs`, the `FriendLinkRequest`/`FriendLinkAccepted` (and `FriendEntry`) pinned-hex fixtures now use a FIXED `eph_x25519_pub = [0x55; 32]` and `token_sig = Some([..])`; for `FriendEntry` use `sealed_secret: Some(vec![0xAB; 60])` and/or a `None` variant. Run the fixture test, read the new hex, paste it in, re-run green. Document in a comment that these pin the *wire shape*, not live crypto (ephemerals/seals are random at runtime).

- [ ] **Step 6: Run** `-E 'test(friend) + test(wire_format_zeb370)'` green, clippy, fmt.

- [ ] **Step 7: Commit.** `feat(zeb-371): handshake carries ephemeral X25519 key + optional token; preimage binds both`

### Task 7: Establish + store the secret on both sides

**Files:**
- Modify: `src-tauri/src/iroh_friend_acceptor.rs` (`process_friend_request` + acceptor struct: inject `KeyTree`)
- Modify: `src-tauri/src/lib.rs` (redeem path: derive+seal+store A-side; wire `KeyTree` into the acceptor + redeem)

- [ ] **Step 1: Write failing tests.** In `iroh_friend_acceptor.rs`, extend `process_friend_request_adds_active_token_friend_and_returns_verifiable_accept` to assert the written entry now has `entry.sealed_secret.is_some()`, and add:

```rust
#[test]
fn process_friend_request_derives_secret_matching_requester() {
    // The acceptor's stored secret must equal what the requester derives from
    // the accept's ephemeral key. Drive both halves of the ECDH here.
    use crate::friend_rendezvous::{generate_ephemeral, derive_friendship_secret};
    use crate::owner_state_crypto::{KeyTree, decrypt_friend_secret};

    let me = mint_test_owner(0x60);
    let requester = mint_test_owner(0x61);
    let token_sig = [0x5a; 64];
    let (req_eph_sk, req_eph_pub) = generate_ephemeral();
    let preimage = friend_request_sig_preimage(requester.owner, Some(&token_sig), &req_eph_pub);
    let sig = requester.device_key.sign(&preimage).to_bytes();
    let req = FriendLinkRequest {
        from_addr: requester.owner, display: None, token_sig: Some(token_sig),
        eph_x25519_pub: req_eph_pub, enrollment: requester.cert, sig,
    };

    let kt = KeyTree::derive(&[9u8; 32]).expect("kt");
    let mut state = OwnerState::default();
    let accepted = process_friend_request(
        &mut state, test_hlc(1000), &req, me.owner, Some("me".into()),
        &me.cert, &me.device_key, &kt,
    ).expect("processed");

    // Requester derives the secret from the accept's ephemeral.
    let requester_secret =
        derive_friendship_secret(req_eph_sk, &accepted.eph_x25519_pub, requester.owner, me.owner);
    // Acceptor stored the sealed secret; open it and compare.
    let entry = state.friend_graph.friends.get(&requester.owner).expect("friend");
    let sealed = entry.sealed_secret.as_ref().expect("secret stored");
    let opened = decrypt_friend_secret(&kt, &requester.owner.0, sealed).expect("open");
    assert_eq!(opened.as_ref(), requester_secret.as_ref());
}
```

- [ ] **Step 2: Run — expect fail** (`process_friend_request` arity + no secret stored).

- [ ] **Step 3: Implement.**
  - `process_friend_request(..)` gains a `keytree: &KeyTree` param. After authenticating: `let (self_eph_sk, self_eph_pub) = generate_ephemeral();` then `let secret = derive_friendship_secret(self_eph_sk, &req.eph_x25519_pub, self_owner, req.from_addr);` then `let sealed = owner_state_crypto::encrypt_friend_secret(keytree, &req.from_addr.0, &secret).map_err(|_| FriendHandshakeError::ApplyRejected("seal".into()))?;` Set `sealed_secret: Some(sealed)` on the `FriendEntry`. Put `self_eph_pub` in the returned `FriendLinkAccepted`. The accept preimage now includes `self_eph_pub` (and the same `req.token_sig`).
  - `IrohFriendHandshakeAcceptor`: add a `keytree: Arc<KeyTree>` field + constructor arg (or a `with_keytree` fluent setter to avoid breaking test call sites — prefer the setter, default a test-only `KeyTree::derive(&[0;32])`? No — make it a required field; update the handful of constructor call sites). `handle_friend_handshake_inbound` passes `&self.keytree` to `process_friend_request`.
  - Redeem path (`lib.rs`): after reading the accept, `let secret = derive_friendship_secret(self_eph_sk /* held from Task 6 */, &accepted.eph_x25519_pub, self_owner, payload.inviter_addr);` seal with the injected `keytree`, and set `sealed_secret: Some(..)` on the A-side `FriendEntry`. Add `keytree: Arc<KeyTree>` param to `connectivity_link_friend_iroh_inner`; the IPC wrapper passes `NodeState`'s KeyTree (derive/look up where lib.rs already builds it, ~`:2629`).

- [ ] **Step 4: Run green** (`-E 'test(process_friend_request) + test(friend_rendezvous)'`), clippy, fmt. Update other `process_friend_request`/acceptor-constructor call sites + the friend integration test (`tests/friend_token_roundtrip_integration.rs`) to pass a `KeyTree` and assert both sides end with a matching opened secret.

- [ ] **Step 5: Commit.** `feat(zeb-371): establish + KeyTree-seal the friendship secret on both handshake sides`

---

## Phase 2 — Case-D crypto + publisher/resolver

### Task 8: Case-D key derivation + payload seal/open

**Files:**
- Modify: `src-tauri/src/friend_rendezvous.rs`

- [ ] **Step 1: Write failing tests:**

```rust
#[test]
fn case_d_reference_vector() {
    // Pin the Case-D slot keying. secret = 0x00*32, epoch 12345, owner 0x11*16.
    let secret = [0u8; 32];
    let key = case_d_publish_key(&secret, 12345, &[0x11; 16]);
    let vk_hex = hex::encode(key.verifying_key().to_bytes());
    assert_eq!(vk_hex, "PLACEHOLDER_RUN_ONCE", "case-d v1 keying must not drift");
}

#[test]
fn case_d_publish_matches_friends_resolve() {
    // A publishes under info=A_owner; B resolves A under info=A_owner. Same slot.
    let secret = [7u8; 32];
    let a = [0xAA; 16];
    let pub_key = case_d_publish_key(&secret, 100, &a);
    let resolve_key = case_d_resolve_key(&secret, 100, &a);
    assert_eq!(pub_key.verifying_key(), resolve_key.verifying_key());
    // Different direction (B's own slot) differs.
    let b = [0xBB; 16];
    assert_ne!(case_d_publish_key(&secret, 100, &a).verifying_key(),
               case_d_publish_key(&secret, 100, &b).verifying_key());
}

#[test]
fn case_d_payload_seal_round_trip() {
    let secret = [3u8; 32];
    let plaintext = b"iroh-reachability-blob";
    let sealed = seal_case_d_payload(&secret, 100, plaintext).expect("seal");
    assert_ne!(&sealed[..], &plaintext[..]);
    let opened = open_case_d_payload(&secret, 100, &sealed).expect("open");
    assert_eq!(opened, plaintext);
    // Wrong epoch (AAD) fails.
    assert!(open_case_d_payload(&secret, 101, &sealed).is_err());
}
```

- [ ] **Step 2: Run — expect fail.**

- [ ] **Step 3: Implement** in `friend_rendezvous.rs`:

```rust
use harmony_pkarr::{derive_ephemeral_key, PkarrCase};
use ed25519_dalek::SigningKey;
use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, KeyInit, Nonce};

fn case_d_info(epoch: u64, owner_id: &[u8; 16]) -> Vec<u8> {
    let mut info = Vec::with_capacity(8 + 16);
    info.extend_from_slice(&epoch.to_be_bytes());
    info.extend_from_slice(owner_id);
    info
}

/// Slot key I PUBLISH under so `self_owner`'s friends can find me.
pub fn case_d_publish_key(secret: &[u8; 32], epoch: u64, self_owner: &[u8; 16]) -> SigningKey {
    derive_ephemeral_key(PkarrCase::Friend, secret, &case_d_info(epoch, self_owner))
}

/// Slot key I RESOLVE to find `friend_owner` (their publish slot).
pub fn case_d_resolve_key(secret: &[u8; 32], epoch: u64, friend_owner: &[u8; 16]) -> SigningKey {
    derive_ephemeral_key(PkarrCase::Friend, secret, &case_d_info(epoch, friend_owner))
}

// Payload sealing: a sub-key + deterministic nonce from epoch keep publish idempotent.
fn case_d_payload_key(secret: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    let hk = Hkdf::<Sha256>::new(Some(b"harmony.friend.v1.case-d-payload"), secret);
    let mut k = Zeroizing::new([0u8; 32]);
    hk.expand(b"", k.as_mut()).expect("32 bytes");
    k
}

pub fn seal_case_d_payload(secret: &[u8; 32], epoch: u64, plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let key = case_d_payload_key(secret);
    let cipher = ChaCha20Poly1305::new_from_slice(key.as_ref()).expect("32-byte key");
    let mut nonce = [0u8; 12];
    nonce[..8].copy_from_slice(&epoch.to_be_bytes()); // epoch-deterministic nonce (one msg/epoch/key)
    cipher.encrypt(Nonce::from_slice(&nonce),
        chacha20poly1305::aead::Payload { msg: plaintext, aad: &epoch.to_be_bytes() })
        .map_err(|_| "case-d seal failed".to_string())
}

pub fn open_case_d_payload(secret: &[u8; 32], epoch: u64, sealed: &[u8]) -> Result<Vec<u8>, String> {
    let key = case_d_payload_key(secret);
    let cipher = ChaCha20Poly1305::new_from_slice(key.as_ref()).expect("32-byte key");
    let mut nonce = [0u8; 12];
    nonce[..8].copy_from_slice(&epoch.to_be_bytes());
    cipher.decrypt(Nonce::from_slice(&nonce),
        chacha20poly1305::aead::Payload { msg: sealed, aad: &epoch.to_be_bytes() })
        .map_err(|_| "case-d open failed".to_string())
}
```

- [ ] **Step 4: Run to fill `PLACEHOLDER_RUN_ONCE`, re-run green.** clippy, fmt.

- [ ] **Step 5: Commit.** `feat(zeb-371): Case-D slot key derivation + sealed reachability payload`

### Task 9: `PkarrFriendPublisher` (Case-D publish)

**Files:**
- Create: `src-tauri/src/pkarr_friend_publisher.rs`
- Modify: `src-tauri/src/lib.rs` (`mod pkarr_friend_publisher;`)

- [ ] **Step 1: Write failing test** (mirror `pkarr_identity_publisher`'s enable/disable):

```rust
#[tokio::test]
async fn register_then_unregister_friend_slot() {
    let relay = MockPkarrRelay::start().await;
    let pool = RelayPool::new(vec![relay.base_url.clone()]);
    let client = Arc::new(RelayClient::new(pool));
    let publisher = Arc::new(PkarrPublisher::new(client));
    let _ph = Arc::clone(&publisher).spawn();

    let secret = [5u8; 32];
    let self_owner = [0xAA; 16];
    let friend = [0xBB; 16];
    let fp = PkarrFriendPublisher::new(Arc::clone(&publisher), self_owner,
        Arc::new(|| b"routing".to_vec()));
    fp.register_friend(friend, secret).await;
    assert!(publisher.active_handles().await.iter().any(|h| h.starts_with("friend:")));
    fp.unregister_friend(&friend).await;
    assert!(!publisher.active_handles().await.iter().any(|h| h.starts_with("friend:")));
}
```

- [ ] **Step 2: Run — expect fail.**

- [ ] **Step 3: Implement.** Mirror `PkarrIdentityPublisher`, but per-friend handles and Case-D keying. The `harmony_identity_pub` embeds the Case-D verifying key in `[32..64]` and the record is inner-signed by the SAME Case-D key (so `verify_inner_sig` passes on resolve; `verify_identity_match` is skipped). The `routing_blob` is `seal_case_d_payload(secret, epoch, &raw_blob)`.

```rust
use crate::friend_rendezvous::{case_d_publish_key, seal_case_d_payload};
use harmony_pkarr::{current_epoch_id, EphemeralKeyBuilder, PkarrPublisher,
    PkarrRoutingRecord, RecordBuilder};
use std::sync::Arc;

pub struct PkarrFriendPublisher {
    publisher: Arc<PkarrPublisher>,
    self_owner: [u8; 16],
    routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
}

fn friend_handle(friend_owner: &[u8; 16]) -> String { format!("friend:{}", hex::encode(friend_owner)) }

impl PkarrFriendPublisher {
    pub fn new(publisher: Arc<PkarrPublisher>, self_owner: [u8; 16],
        routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>) -> Self {
        Self { publisher, self_owner, routing_blob_builder }
    }

    /// Begin (or refresh) Case-D publication for one active friend.
    pub async fn register_friend(&self, friend_owner: [u8; 16], secret: [u8; 32]) {
        let self_owner = self.self_owner;
        let key_builder: EphemeralKeyBuilder = Arc::new(move |at_ms| {
            case_d_publish_key(&secret, current_epoch_id(at_ms), &self_owner)
        });
        let blob_builder = Arc::clone(&self.routing_blob_builder);
        let builder: RecordBuilder = Arc::new(move |at_ms| {
            let epoch = current_epoch_id(at_ms);
            let cd_key = case_d_publish_key(&secret, epoch, &self_owner);
            let mut id_pub = [0u8; 64];
            id_pub[32..].copy_from_slice(&cd_key.verifying_key().to_bytes());
            let sealed = seal_case_d_payload(&secret, epoch, &blob_builder())
                .expect("case-d seal");
            PkarrRoutingRecord::sign_new(sealed, id_pub, at_ms, &cd_key)
                .expect("sign — derived key matches embedded id_pub")
        });
        self.publisher.register(friend_handle(&friend_owner), key_builder, builder).await;
    }

    pub async fn unregister_friend(&self, friend_owner: &[u8; 16]) {
        self.publisher.unregister(&friend_handle(friend_owner)).await;
    }
}
```

- [ ] **Step 4: Run green**, clippy, fmt.

- [ ] **Step 5: Commit.** `feat(zeb-371): PkarrFriendPublisher — per-friend Case-D publication`

### Task 10: Case-D resolver

**Files:**
- Modify: `src-tauri/src/pkarr_friend_publisher.rs` (add resolver fn) or a sibling `resolve_friend_case_d`.

- [ ] **Step 1: Write failing test** (publish then resolve via MockPkarrRelay; unseal + decode):

```rust
#[tokio::test]
async fn case_d_publish_then_resolve_round_trip() {
    let relay = MockPkarrRelay::start().await;
    let pool = RelayPool::new(vec![relay.base_url.clone()]);
    let client = Arc::new(RelayClient::new(pool));
    let publisher = Arc::new(PkarrPublisher::new(Arc::clone(&client)));
    let _ph = Arc::clone(&publisher).spawn();
    let resolver = Arc::new(PkarrResolver::new(Arc::clone(&client)));

    let secret = [9u8; 32];
    let a_owner = [0xAA; 16]; // publisher
    let raw = b"alice-iroh-routing".to_vec();
    let fp = PkarrFriendPublisher::new(Arc::clone(&publisher), a_owner,
        Arc::new(move || raw.clone()));
    fp.register_friend([0xBB; 16], secret).await; // A publishes its own slot (info=a_owner)

    // B resolves A under info=a_owner, unseals, gets the raw blob.
    let mut attempts = 0;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        attempts += 1; assert!(attempts < 60, "resolve timed out");
        if let Some(blob) = resolve_friend_case_d(&resolver, &secret, &a_owner).await.expect("resolve") {
            assert_eq!(blob, b"alice-iroh-routing");
            return;
        }
    }
}
```

- [ ] **Step 2: Run — expect fail.**

- [ ] **Step 3: Implement** `resolve_friend_case_d` — derive the resolve keys across the epoch window, `resolve_window`, then unseal `routing_blob` with the matching epoch:

```rust
use crate::friend_rendezvous::{case_d_resolve_key, open_case_d_payload};
use harmony_pkarr::{epoch_tolerance_window, current_epoch_id, PkarrResolver};

/// Resolve `friend_owner`'s current Case-D routing blob (unsealed) using the
/// shared `secret`. Returns the freshest valid record's payload, or `None`.
pub async fn resolve_friend_case_d(
    resolver: &PkarrResolver,
    secret: &[u8; 32],
    friend_owner: &[u8; 16],
) -> Result<Option<Vec<u8>>, String> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let window = epoch_tolerance_window(now_ms);
    let keys: Vec<_> = window.iter()
        .map(|&e| case_d_resolve_key(secret, e, friend_owner).verifying_key())
        .collect();
    let Some(rec) = resolver.resolve_window(&keys).await.map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    // Try each epoch in the window to unseal (the record could be from any).
    for &e in &window {
        if let Ok(blob) = open_case_d_payload(secret, e, &rec.routing_blob) {
            return Ok(Some(blob));
        }
    }
    Ok(None) // record present but unseal failed (wrong epoch/secret) — treat as miss
}
```

- [ ] **Step 4: Run green** (rerun serially if it flakes on the mock relay), clippy, fmt.

- [ ] **Step 5: Commit.** `feat(zeb-371): Case-D resolver (resolve + unseal a friend's reachability)`

---

## Phase 3 — Cadence wiring + reconnection

### Task 11: Wire Case-D into the reachability cadence + add/drop on Active/Revoke

**Files:**
- Modify: `src-tauri/src/lib.rs` (NodeState: hold a `PkarrFriendPublisher`; the reachability `PublishFn` also refreshes friend slots; register/unregister on friend Active/Revoke in the acceptor + redeem + unfriend paths).

> Integration-heavy; the unit-testable seam is "given the active-friend set + secrets, the publisher ends with exactly those handles." Drive that directly.

- [ ] **Step 1: Write a failing test** (`lib.rs` or a focused module) that builds a `PkarrFriendPublisher` over a `PkarrPublisher`, then calls a new helper `sync_case_d_handles(publisher, &owner_state, &keytree)` and asserts the active handles equal the set of `Active` friends *with a `sealed_secret`*:

```rust
#[tokio::test]
async fn sync_case_d_handles_tracks_active_friends_with_secrets() {
    // owner_state has: one Active+secret friend, one Active+no-secret, one Revoked.
    // After sync, exactly the Active+secret friend's handle is registered.
    // (construct via apply_friend_update + encrypt_friend_secret; see helper)
}
```

- [ ] **Step 2: Run — expect fail.**

- [ ] **Step 3: Implement** `sync_case_d_handles` (iterate `friend_graph.friends`, for each `Active` with `sealed_secret` → open with KeyTree → `register_friend`; for others → `unregister_friend`). Call it: (a) on startup after owner-state load; (b) inside the reachability `PublishFn` (so address changes refresh blobs — `register_friend` re-registers with the current blob builder); (c) after each friend write (acceptor accept, redeem, unfriend) — reuse the existing `notify_owner_state_dirty` seam to also trigger a `sync_case_d_handles`. Hold the `PkarrFriendPublisher` + `KeyTree` + `PkarrResolver` on `NodeState`.

- [ ] **Step 4: Reconnection (resolve-on-connect).** Add a helper `connectivity_resolve_friend(owner_id) -> Option<NodeAddr>` that loads the friend's `sealed_secret`, opens it, `resolve_friend_case_d`, decodes the `ReachabilityAnnouncePayload` → `EndpointAddr` (reuse the redeem path's decode block ~`lib.rs:32656`). This is what a future "message/connect a friend" path calls; for 1b, expose it as IPC `connectivity_resolve_friend` + an integration assertion that a re-published address is picked up.

- [ ] **Step 5: Run green** (`-E 'test(case_d) + test(sync_case_d)'`), clippy, fmt. Add the two-node integration test (link → change address → re-resolve restores reachability) in `tests/` (generous timeouts; serial-safe).

- [ ] **Step 6: Commit.** `feat(zeb-371): publish/refresh/drop Case-D slots on the reachability cadence + resolve-on-connect`

---

## Phase 4 — Path A (mutual-key) + consent

### Task 12: Inbound consent decision tree + pending-request store + policy

**Files:**
- Modify: `src-tauri/src/iroh_friend_acceptor.rs` (`FriendLinkResponse` enum; decision tree; pending store)
- Modify: `src-tauri/src/lib.rs` (`friend_auto_accept_known` setting; pending-inbound store on NodeState)

- [ ] **Step 1: Write failing truth-table tests** for a pure `decide_consent(...)` (no I/O):

```rust
// known+auto -> AcceptInline; known+auto-off -> Pending; unknown-new -> Pending;
// unknown-with-prior-accept -> AcceptInline; revoked -> Pending; Some(token) handled upstream.
#[test]
fn consent_decision_truth_table() { /* assert each arm of ConsentDecision */ }
```

Where `decide_consent(token_sig: Option<_>, requester: OwnerAddr, graph: &FriendGraph, community_member: bool, auto_known: bool, prior_accept: bool) -> ConsentDecision { AcceptInline | Pending | TokenPath }`.

- [ ] **Step 2: Run — expect fail.**

- [ ] **Step 3: Implement.**
  - Add `enum FriendLinkResponse { Accepted(FriendLinkAccepted), Pending }` (length-prefixed/strict like the others). Token path + AcceptInline return `Accepted`; Pending returns `Pending`.
  - `decide_consent` pure fn (per the spec §7.1 tree). `known = graph has requester as Active|Pending || community_member`; `community_member` left `false` in 1b (spec open-Q1 — friend-graph-only first; leave a `// TODO Phase 2: community co-member` note).
  - Process-local pending-inbound store on `NodeState` (`Mutex<HashMap<OwnerAddr, PendingFriendRequest>>` + a decision set), mirroring the live-token map. On `Pending`, insert (idempotent) + emit `friend-request-received`. On a recorded Accept for the requester at a later dial → `AcceptInline`.
  - The acceptor's `handle_friend_handshake_inbound`: branch on `req.token_sig` → token gate (unchanged) for `Some`; else `decide_consent` → AcceptInline (run `process_friend_request` as `MutualKey`) or Pending (record + reply `Pending`, write nothing).
  - `friend_auto_accept_known: bool` setting (default `true`) persisted with the existing friend/pkarr settings; read into `decide_consent`.

- [ ] **Step 4: Run green** (`-E 'test(consent)'`), clippy, fmt.

- [ ] **Step 5: Commit.** `feat(zeb-371): Path A consent decision tree + pending-request store + auto-accept setting`

### Task 13: Path A IPCs + requester Pending/retry

**Files:**
- Modify: `src-tauri/src/lib.rs` (IPCs `add_friend_by_key`, `accept_friend_request`, `decline_friend_request`, `list_pending_friend_requests`, `set_friend_auto_accept`; requester Pending handling + bounded retry)

- [ ] **Step 1: Write failing inner-fn tests** (drive the pure inners, not the Tauri shells): e.g. `accept_friend_request_inner` records a decision that a subsequent `decide_consent` resolves to `AcceptInline`; `add_friend_by_key_inner` produces a `FriendLinkRequest{token_sig: None, ..}`; on `Pending` the requester writes a local `FriendEntry{Pending, MutualKey, sealed_secret: None}`.

- [ ] **Step 2: Run — expect fail.**

- [ ] **Step 3: Implement.**
  - `add_friend_by_key(owner_id_hex or owner_pub, reachability hint)`: resolve the target (Case-B if discoverable; else `Err("not reachable — ask them for a friend token")`); dial `harmony/friend/v1` with `token_sig: None` + ephemeral; on `Accepted` derive+seal+store (reuse Task 7 path); on `Pending` write local `Pending` entry + surface "request sent". Bounded retry: re-dial up to N times on a small backoff (decide cap in code; log when giving up — no silent truncation).
  - `accept_friend_request(owner_id)` / `decline_friend_request(owner_id)`: record/drop the decision; emit `friend-list-changed`.
  - `list_pending_friend_requests()` → DTOs (owner_id hex, display).
  - `set_friend_auto_accept(enabled)` persists the setting.
  - Register all in `generate_handler!`.

- [ ] **Step 4: Run green**, clippy, fmt.

- [ ] **Step 5: Commit.** `feat(zeb-371): Path A IPCs (add-by-key, accept/decline, pending list, policy) + retry`

---

## Phase 5 — Frontend + pins + full gate

### Task 14: Frontend — requests inbox, add-by-key, policy toggle

**Files:**
- Modify: `src/lib/friend-service.ts`, `src/lib/friend-service.test.ts`
- Modify: `src/lib/components/FriendsPanel.svelte`; add `FriendRequestsList` UI + an "Add by key" input + an auto-accept toggle.

- [ ] **Step 1: Write failing vitest** for `friend-service`: `listPendingRequests()`, `acceptRequest()/declineRequest()`, `addByKey()`, `setAutoAccept()` call the right IPC names with camelCase args; a `friend-request-received` event refreshes the pending list (mirror the existing `onFriendsChanged` multi-listener pattern).

- [ ] **Step 2: Run — expect fail:** `npx vitest run friend-service`.

- [ ] **Step 3: Implement** the service methods + event subscription + minimal Svelte UI (requests list with Accept/Decline, add-by-key field, auto-accept toggle). Follow the existing `FriendsPanel`/`FriendService` conventions.

- [ ] **Step 4: Run green:** `npx vitest run` + `npx tsc --noEmit`.

- [ ] **Step 5: Commit.** `feat(zeb-371): friend-requests inbox, add-by-key, auto-accept toggle (frontend)`

### Task 15: Wire-format pins refresh + full gate

**Files:**
- Modify: `src-tauri/tests/wire_format_zeb370_fixtures.rs` (add a `FriendLinkResponse` pin + a Case-D key vector mirror; confirm the Task-6 re-pins hold)
- Verify: full workspace gate.

- [ ] **Step 1:** Add a pinned wire fixture for `FriendLinkResponse::Pending` and `::Accepted` (fixed ephemeral), and a Rust-side mirror of the `case_d_reference_vector` (so a harmony-pkarr drift is caught here too). Run, paste hex, green.

- [ ] **Step 2: Full gate** from `src-tauri/`:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Then repo root: `npx tsc --noEmit && npx vitest run`. (Rerun any UDP-4242-flaky `*_integration` test serially with `-j 1` to confirm it's the known flake, not a regression.)

- [ ] **Step 3: Commit.** `test(zeb-371): wire-format pins for Path A response + Case-D vector; full gate green`

---

## Self-review notes (for the executor)

- **Each task leaves the tree green** — Task 5 adds the field defaulted `None` before Task 7 fills it; Task 6 threads the ephemeral through call sites (compile-only) before Task 7 uses it. Don't reorder 6→7.
- **Wire-format determinism:** runtime ephemerals/seals are random, so the pinned fixtures construct structs with FIXED `eph_x25519_pub`/opaque `sealed_secret` bytes — they pin *shape*, not live crypto.
- **`PLACEHOLDER_RUN_ONCE`** appears in three reference-vector tests (Tasks 1, 8, 15-mirror) — each is computed-once-then-pinned, NOT left as a literal.
- **Cross-repo ordering:** Tasks 1–2 require a pushed harmony commit. The harmony-pkarr change gets its own small PR; re-pin to its merged SHA before the harmony-client PR merges.
- **Type consistency:** `case_d_publish_key`/`case_d_resolve_key`/`seal_case_d_payload`/`open_case_d_payload`/`resolve_friend_case_d`/`PkarrFriendPublisher::{register,unregister}_friend`/`decide_consent`/`FriendLinkResponse` names are used identically across tasks.
- **TDD throughout:** every task is RED → GREEN → commit; reference vectors and the consent truth-table are the highest-value pins.
