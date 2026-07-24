# ZEB-746 PR 2 (client): converge HRMI + HRSS onto `harmony_crypto::password_envelope`

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use `- [ ]` checkboxes.

**Goal:** Rewire the client's three hand-rolled Argon2id+XChaCha20-Poly1305 at-rest envelopes (`identity.rs` HRMI v0x01 + v0x02, `state_snapshot.rs` HRSS) onto the shared `harmony_crypto::password_envelope::{seal, open, Argon2idParams}` primitive landed in PR 1 (core `31ec347`), with **zero on-disk byte change**.

**Architecture:** The primitive is format-agnostic — the caller supplies the already-serialized header (HRMI) or the constant AAD string (HRSS) as opaque `aad`, plus salt/nonce/params. Every existing byte-layout, DoS guard, magic/version/kdf check, oracle-free error collapse, and salt/nonce RNG stays caller-side. Byte-identity is proven by three pins staying green: the v0x01 golden `encrypted_v1.bin`, a **new** v0x02 golden `encrypted_v2.bin` (captured from the current inline code *before* the rewire), and the existing HRSS inline-hex pin in `zeb213_fixtures.rs`.

**Tech Stack:** Rust, `harmony-crypto` (feature `password-envelope`), `chacha20poly1305 0.10` (retained — used by 9 other modules), `argon2 0.5` (dropped — sole users are the two rewired files).

## Global Constraints

*(Every task's requirements implicitly include this section. Copy verbatim into each reviewer prompt.)*

- **Byte-identity is NON-NEGOTIABLE.** After every task these MUST stay green and their `.bin`/hex literals MUST NOT be regenerated: `wire_format_v1_pinned` (golden `tests/fixtures/encrypted_v1.bin`), `wire_format_v2_pinned` (golden `tests/fixtures/encrypted_v2.bin`, added in Task 2), `hrss_envelope_byte_pinned` (inline hex in `wire_format/zeb213_fixtures.rs`). `tests/fixtures/encrypted_v1.bin` is never touched.
- **AAD differs by envelope and must be preserved exactly:** all four HRMI sites bind `aad = &header[..HEADER_LEN]` (the 13-byte header); HRSS binds `aad = HRSS_AAD` (the constant `b"harmony-owner-state-snapshot-v1"`).
- **Crypto params are fixed:** Argon2id, `Version::V0x13`, m=65536 KiB, t=3, p=1, output=32 bytes; XChaCha20-Poly1305, 24-byte nonce, 16-byte tag. The primitive hard-codes output=32 (`KEY_LEN`); `Argon2idParams::new(m_kib, t_cost, p_cost)` takes the other three.
- **Route crypto through `harmony_crypto::password_envelope::{seal, open, Argon2idParams}`.** Keep caller-side: header serialization, the strict param-equality DoS guard (reject non-pinned m/t/p *before* deriving), magic/version/kdf_id checks, the indistinguishable `wrong passphrase or corrupted file` error collapse (no decryption oracle), and salt/nonce RNG.
- **Deterministic-nonce seams MUST be gated** `#[cfg(any(test, feature = "test-fixtures"))]` — production must never link a caller-supplied-nonce encrypt path (nonce reuse on XChaCha20 is catastrophic; CLAUDE.md hard rule + Qodo C4).
- **Lockstep pins:** the 13 harmony crates on the shared rev bump **together** to `31ec3472890db6f02919f072a982768c96cbf810`; the 2 `harmony-pkarr` lines (own rev `80f6d808…`) are **untouched**.
- **Gates (CI-parity, run from `src-tauri/`):** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Iterative dev may use `scripts/test-select --context task`; the final pre-PR sweep is the full commands.

## Primitive API (from core `31ec347`, `crates/harmony-crypto/src/password_envelope.rs`)

```rust
pub const KEY_LEN: usize = 32;   // Argon2 output; hard-coded inside
pub const NONCE_LEN: usize = 24;
pub const TAG_LEN: usize = 16;
pub struct Argon2idParams { /* private fields */ }
impl Argon2idParams { pub fn new(m_kib: u32, t_cost: u32, p_cost: u32) -> Result<Self, CryptoError>; }
pub fn seal(password: &[u8], params: &Argon2idParams, salt: &[u8], nonce: &[u8], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError>;      // returns ct‖tag
pub fn open(password: &[u8], params: &Argon2idParams, salt: &[u8], nonce: &[u8], aad: &[u8], ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>, CryptoError>;
```

`seal`/`open` check `nonce.len() == 24` internally; `open` also rejects `ciphertext < TAG_LEN`. `salt`/`nonce`/`aad` are `&[u8]`, so `&[u8; 16]`/`&[u8; 24]`/`&header[..13]` all coerce.

---

### Task 1: Bump lockstep pins to R1 + enable `password-envelope`

**Files:**
- Modify: `src-tauri/Cargo.toml` (the 13 harmony git pins + the harmony-crypto feature list)
- Modify: `src-tauri/Cargo.lock` (regenerated)

**Interfaces:**
- Produces: the `harmony_crypto::password_envelope` module becomes resolvable in the client build (unused until Task 3).

- [ ] **Step 1: Bump the 13 shared-rev pins.** In `src-tauri/Cargo.toml`, replace every `rev = "374574499d1873f3d069af610d5bc789c78c1c36"` with `rev = "31ec3472890db6f02919f072a982768c96cbf810"`. That is exactly the 13 lines for: `harmony-runtime`, `harmony-identity`, `harmony-content`, `harmony-compute`, `harmony-telemetry`, `harmony-mailbox`, `harmony-owner`, `harmony-crypto`, `harmony-crdt-sync`, `harmony-tunnel`, `harmony-iroh`, `harmony-tunnel-iroh`, `harmony-reachability`. **Do NOT touch** the two `harmony-pkarr` lines (rev `80f6d80858f283d4f4094d483d548e50b8c4e107`).

- [ ] **Step 2: Enable the feature on the direct harmony-crypto pin.** The `harmony-crypto` line currently reads:
  ```toml
  harmony-crypto = { git = "https://github.com/zeblithic/harmony.git", rev = "31ec3472890db6f02919f072a982768c96cbf810" }
  ```
  Add the feature:
  ```toml
  harmony-crypto = { git = "https://github.com/zeblithic/harmony.git", rev = "31ec3472890db6f02919f072a982768c96cbf810", features = ["password-envelope"] }
  ```
  (Harmony-owner's `recovery` feature also enables it transitively, but declaring it on the direct dependency is explicit and correct.)

- [ ] **Step 3: Regenerate the lock + build.** From `src-tauri/`:
  ```bash
  cargo build --workspace 2>&1 | tail -20
  ```
  Expected: resolves the new git rev and compiles clean (the primitive is present but unused). Then confirm the lock changed only for the harmony crates:
  ```bash
  git -C .. diff --stat src-tauri/Cargo.lock
  git -C .. diff src-tauri/Cargo.lock | grep -E '^[+-]source' | sort -u
  ```
  Expected: the `+`/`-` `source` lines reference only `harmony.git?rev=31ec347…` (added) and `…?rev=3745744…` (removed); the `harmony-pkarr` `?rev=80f6d808…` source is unchanged.

- [ ] **Step 4: Verify feature reaches harmony-crypto.**
  ```bash
  cargo tree -p harmony-crypto -f "{p} {f}" 2>/dev/null | grep -m1 harmony-crypto
  ```
  Expected: the feature list for `harmony-crypto` includes `password-envelope`.

- [ ] **Step 5: Commit.**
  ```bash
  git add src-tauri/Cargo.toml src-tauri/Cargo.lock
  git commit -m "deps: bump harmony pins to ZEB-746 core rev + enable password-envelope"
  ```

---

### Task 2: v0x02 deterministic seam + golden fixture (captured from CURRENT inline crypto)

**Why before the rewire:** v0x01 and HRSS already have byte-pins captured against today's inline crypto, so keeping them green after Task 3 proves byte-identity. v0x02 has **no pin and no deterministic seam** (`encrypt_vault` generates its own `OsRng` salt/nonce). This task adds the seam and captures the golden **while `encrypt_vault` still uses inline Argon2** — so the fixture pins the pre-rewire bytes. Capturing it after Task 3 would pin the new bytes to themselves and prove nothing.

**Files:**
- Modify: `src-tauri/src/identity.rs` (split `encrypt_vault`; add gated `encrypt_vault_with_params`; extend `test_only`)
- Modify: `src-tauri/tests/wire_format/fixture.rs` (add `wire_format_v2_pinned`)
- Create: `src-tauri/tests/fixtures/encrypted_v2.bin` (generated, committed)

**Interfaces:**
- Consumes: existing `encrypt_vault`, `decrypt_vault_bytes` (unchanged crypto).
- Produces: `identity::test_only::encrypt_vault_with_params_for_test(passphrase, plaintext, &salt, &nonce) -> Vec<u8>` (gated); golden `encrypted_v2.bin`.

- [ ] **Step 1: Byte-neutral split of `encrypt_vault`.** In `src-tauri/src/identity.rs`, replace the body of `fn encrypt_vault` (currently `identity.rs:1441`) so it only generates entropy and delegates, and move the framing+crypto verbatim into a private `encrypt_vault_inner`. The inner keeps the **exact current** inline Argon2/XChaCha (this task does NOT introduce the primitive):

  ```rust
  fn encrypt_vault(passphrase: &[u8], plaintext: &[u8]) -> Vec<u8> {
      use rand::RngCore;
      let mut salt = [0u8; SALT_LEN];
      let mut nonce = [0u8; NONCE_LEN];
      rand::rngs::OsRng.fill_bytes(&mut salt);
      rand::rngs::OsRng.fill_bytes(&mut nonce);
      encrypt_vault_inner(passphrase, plaintext, &salt, &nonce)
  }

  /// Deterministic variant for byte-pinning fixtures. Gated so production
  /// cannot link a caller-supplied-nonce path (nonce reuse on XChaCha20 is
  /// catastrophic — mirrors `encode_snapshot_with_params`, Qodo C4).
  #[cfg(any(test, feature = "test-fixtures"))]
  #[doc(hidden)]
  pub fn encrypt_vault_with_params(
      passphrase: &[u8],
      plaintext: &[u8],
      salt: &[u8; SALT_LEN],
      nonce: &[u8; NONCE_LEN],
  ) -> Vec<u8> {
      encrypt_vault_inner(passphrase, plaintext, salt, nonce)
  }

  fn encrypt_vault_inner(
      passphrase: &[u8],
      plaintext: &[u8],
      salt: &[u8; SALT_LEN],
      nonce: &[u8; NONCE_LEN],
  ) -> Vec<u8> {
      use argon2::{Algorithm, Argon2, Params, Version};
      use chacha20poly1305::{
          aead::{Aead, KeyInit, Payload},
          XChaCha20Poly1305, XNonce,
      };

      let mut out = Vec::with_capacity(HEADER_LEN + SALT_LEN + NONCE_LEN + plaintext.len() + TAG_LEN);
      out.extend_from_slice(ENC_MAGIC);
      out.push(ENC_FORMAT_VERSION_V2);
      out.push(ENC_KDF_ID_ARGON2ID);
      out.extend_from_slice(&KDF_M_KIB.to_be_bytes());
      out.extend_from_slice(&KDF_T.to_be_bytes());
      out.push(KDF_P);
      debug_assert_eq!(out.len(), HEADER_LEN);
      out.extend_from_slice(salt);
      out.extend_from_slice(nonce);

      let params = Params::new(KDF_M_KIB, KDF_T as u32, KDF_P as u32, Some(KDF_OUT_LEN))
          .expect("Argon2 params hardcoded valid");
      let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
      let mut key = Zeroizing::new([0u8; KDF_OUT_LEN]);
      argon
          .hash_password_into(passphrase, salt, key.as_mut_slice())
          .expect("Argon2 derivation cannot fail with hardcoded params");

      let cipher =
          XChaCha20Poly1305::new_from_slice(key.as_slice()).expect("32-byte key always valid");
      let payload = Payload {
          msg: plaintext,
          aad: &out[..HEADER_LEN],
      };
      let ciphertext_with_tag = cipher
          .encrypt(XNonce::from_slice(nonce), payload)
          .expect("AEAD encrypt cannot fail with valid inputs");
      out.extend_from_slice(&ciphertext_with_tag);
      out
  }
  ```

  This is a pure extract-method refactor: the production `encrypt_vault` path produces byte-identical output (same header, same `OsRng` salt/nonce, same crypto).

- [ ] **Step 2: Extend the `test_only` re-export.** In the `pub mod test_only` block (`identity.rs:3507`), add alongside the existing re-exports:
  ```rust
  #[cfg(any(test, feature = "test-fixtures"))]
  pub use super::encrypt_vault_with_params as encrypt_vault_with_params_for_test;
  ```
  (The whole `test_only` module is already test/fixtures-reachable; match the gating of its siblings — if the module is unconditionally `pub`, gate this individual re-export as shown.)

- [ ] **Step 3: Add the v0x02 pin test.** Append to `src-tauri/tests/wire_format/fixture.rs`:
  ```rust
  use harmony_app::identity::test_only::encrypt_vault_with_params_for_test;

  const V2_PASSPHRASE: &[u8] = b"correct horse battery staple";
  const V2_SALT: [u8; 16] = [0x1A; 16];
  const V2_NONCE: [u8; 24] = [0x2B; 24];
  // Fixed arbitrary plaintext — the v0x02 envelope protects opaque bytes, so
  // the pin is independent of SecretVault CBOR shape.
  const V2_PLAINTEXT: [u8; 48] = [0x5C; 48];

  fn fixture_v2_path() -> std::path::PathBuf {
      std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
          .join("tests")
          .join("fixtures")
          .join("encrypted_v2.bin")
  }

  #[test]
  fn wire_format_v2_pinned() {
      let bytes =
          encrypt_vault_with_params_for_test(V2_PASSPHRASE, &V2_PLAINTEXT, &V2_SALT, &V2_NONCE);
      // header(13) + salt(16) + nonce(24) + plaintext(48) + tag(16) = 117
      assert_eq!(bytes.len(), 117, "v0x02 envelope length");
      assert_eq!(&bytes[..4], b"HRMI", "magic");
      assert_eq!(bytes[4], 0x02, "v0x02 format version");

      let path = fixture_v2_path();
      if std::env::var("HARMONY_REGENERATE_WIRE_FIXTURE").is_ok() {
          std::fs::create_dir_all(path.parent().unwrap()).unwrap();
          std::fs::write(&path, &bytes).expect("write fixture");
          eprintln!("Regenerated v2 fixture at {}", path.display());
          return;
      }
      let expected = std::fs::read(&path).unwrap_or_else(|_| {
          panic!(
              "Fixture missing at {}.\nFirst-time setup: run with HARMONY_REGENERATE_WIRE_FIXTURE=1 to generate, then commit.",
              path.display()
          )
      });
      assert_eq!(
          bytes, expected,
          "v0x02 WIRE FORMAT CHANGED — this envelope must stay byte-identical across the password_envelope rewire"
      );

      // Round-trip through the live decoder confirms the pinned bytes decrypt.
      let back = harmony_app::identity::decrypt_vault_bytes(V2_PASSPHRASE, &bytes)
          .expect("decrypt pinned v2 envelope");
      assert_eq!(&back[..], &V2_PLAINTEXT[..], "round-trip plaintext");
  }
  ```
  Confirm `decrypt_vault_bytes` is reachable from integration tests (it is `pub(crate)` today — if not re-exported for tests, add a `test_only::decrypt_vault_bytes_for_test` re-export mirroring `decrypt_for_test`, and call that instead).

- [ ] **Step 4: Generate the golden.** From `src-tauri/`:
  ```bash
  HARMONY_REGENERATE_WIRE_FIXTURE=1 cargo nextest run --locked --features test-fixtures -E 'test(wire_format_v2_pinned)'
  ```
  Then re-run WITHOUT the env var to confirm the assertion passes:
  ```bash
  cargo nextest run --locked --features test-fixtures -E 'test(wire_format_v2_pinned)'
  ```
  Expected: PASS. Confirm `git status` shows the new `tests/fixtures/encrypted_v2.bin`.

- [ ] **Step 5: Scoped gate + commit.**
  ```bash
  cargo fmt --all
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
  cargo nextest run --locked --features test-fixtures -E 'test(wire_format) or test(vault) or test(identity)' 2>&1 | tail -15
  git -C .. add src-tauri/src/identity.rs src-tauri/tests/wire_format/fixture.rs src-tauri/tests/fixtures/encrypted_v2.bin
  git -C .. commit -m "identity: add deterministic v0x02 seam + byte-pin encrypted_v2.bin (pre-rewire capture)"
  ```

---

### Task 3: Rewire all HRMI + HRSS crypto onto `password_envelope`

**Files:**
- Modify: `src-tauri/src/identity.rs` (`encrypt_with_params`, `decrypt`, `encrypt_vault_inner`, `decrypt_v2_plaintext`)
- Modify: `src-tauri/src/state_snapshot.rs` (`encode_snapshot_inner`, `decode_snapshot`, and the inline-Argon2 test `hrss_unknown_version_rejected`)

**Interfaces:**
- Consumes: `harmony_crypto::password_envelope::{seal, open, Argon2idParams}` (Task 1 made it resolvable).
- Byte-identity gate: `wire_format_v1_pinned`, `wire_format_v2_pinned`, `hrss_envelope_byte_pinned` all stay green with **no fixture regeneration**.

- [ ] **Step 1: Add the import to `identity.rs`.** Near the top imports add:
  ```rust
  use harmony_crypto::password_envelope::{self, Argon2idParams};
  ```

- [ ] **Step 2: Rewire `encrypt_with_params` (v0x01, `identity.rs:239`).** Keep the header build + salt/nonce append verbatim (through the `debug_assert_eq!(out.len(), HEADER_LEN + SALT_LEN + NONCE_LEN)`), then replace the KDF + AEAD block (the `use argon2…`/`use chacha20poly1305…` lines and everything from `let params = Params::new(…)` down to the `ciphertext_with_tag` assignment) with:
  ```rust
      let params = Argon2idParams::new(KDF_M_KIB, KDF_T as u32, KDF_P as u32)
          .expect("Argon2 params hardcoded valid");
      let ciphertext_with_tag = password_envelope::seal(
          passphrase,
          &params,
          salt,
          nonce,
          &out[..HEADER_LEN],
          blob,
      )
      .expect("seal cannot fail with valid inputs");
      debug_assert_eq!(ciphertext_with_tag.len(), BLOB_LEN + TAG_LEN);
      out.extend_from_slice(&ciphertext_with_tag);
      debug_assert_eq!(out.len(), ENC_FILE_LEN);
      out
  ```
  Delete the now-unused `use argon2…`/`use chacha20poly1305…` at the top of this fn.

- [ ] **Step 3: Rewire `decrypt` (v0x01, `identity.rs:302`).** Keep everything through the strict DoS param-equality guard (`if m_kib != KDF_M_KIB … { return Err(…) }`). Replace the crypto block (from `let params = Params::new(…)` through the `let plaintext = Zeroizing::new(cipher.decrypt(…))` binding) with:
  ```rust
      let params = Argon2idParams::new(m_kib, t, p).map_err(|_| {
          "identity store could not be decrypted: wrong passphrase or corrupted file".to_string()
      })?;
      let plaintext = password_envelope::open(
          passphrase,
          &params,
          salt,
          nonce,
          &bytes[..HEADER_LEN],
          ciphertext_with_tag,
      )
      .map_err(|_| {
          "identity store could not be decrypted: wrong passphrase or corrupted file".to_string()
      })?;
  ```
  Keep the existing length-validate + copy-into-`Zeroizing<[u8; BLOB_LEN]>` tail unchanged (`plaintext` is `Zeroizing<Vec<u8>>`). Delete the fn's `use argon2…`/`use chacha20poly1305…`. **Note:** the previously-distinct `"Argon2 derivation failed: {e}"` message on the derive-failure path is now folded into the indistinguishable message — an intentional, strictly-more-conservative change (no oracle), and that path is unreachable for a fixed 16-byte salt + validated params. Flag it in the task report.

- [ ] **Step 4: Rewire `encrypt_vault_inner` (v0x02, from Task 2).** Same shape as Step 2, variable-length plaintext, version byte already `0x02`:
  ```rust
  fn encrypt_vault_inner(
      passphrase: &[u8],
      plaintext: &[u8],
      salt: &[u8; SALT_LEN],
      nonce: &[u8; NONCE_LEN],
  ) -> Vec<u8> {
      let mut out = Vec::with_capacity(HEADER_LEN + SALT_LEN + NONCE_LEN + plaintext.len() + TAG_LEN);
      out.extend_from_slice(ENC_MAGIC);
      out.push(ENC_FORMAT_VERSION_V2);
      out.push(ENC_KDF_ID_ARGON2ID);
      out.extend_from_slice(&KDF_M_KIB.to_be_bytes());
      out.extend_from_slice(&KDF_T.to_be_bytes());
      out.push(KDF_P);
      debug_assert_eq!(out.len(), HEADER_LEN);
      out.extend_from_slice(salt);
      out.extend_from_slice(nonce);
      let params = Argon2idParams::new(KDF_M_KIB, KDF_T as u32, KDF_P as u32)
          .expect("Argon2 params hardcoded valid");
      let ciphertext_with_tag =
          password_envelope::seal(passphrase, &params, salt, nonce, &out[..HEADER_LEN], plaintext)
              .expect("seal cannot fail with valid inputs");
      out.extend_from_slice(&ciphertext_with_tag);
      out
  }
  ```

- [ ] **Step 5: Rewire `decrypt_v2_plaintext` (v0x02, `identity.rs:1526`).** Keep the offset consts, the kdf_id check, the param reads, and the DoS param-equality guard. Replace the crypto block (from `let params = Params::new(…)` through the returned `plaintext`) with:
  ```rust
      let params = Argon2idParams::new(m_kib, t, p).map_err(|_| {
          "identity store could not be decrypted: wrong passphrase or corrupted file".to_string()
      })?;
      let plaintext = password_envelope::open(
          passphrase,
          &params,
          salt,
          nonce,
          &bytes[..HEADER_LEN],
          ciphertext_with_tag,
      )
      .map_err(|_| {
          "identity store could not be decrypted: wrong passphrase or corrupted file".to_string()
      })?;
      Ok(plaintext)
  ```
  `open` returns `Zeroizing<Vec<u8>>`, which is exactly this fn's return type. Delete the fn's `use argon2…`/`use chacha20poly1305…`.

- [ ] **Step 6: Rewire HRSS `encode_snapshot_inner` (`state_snapshot.rs:153`).** Add `use harmony_crypto::password_envelope::{self, Argon2idParams};` to the file's imports. Keep the CBOR encode + header build + salt/nonce append verbatim. Replace the KDF+AEAD block (from `let params = Params::new(…)` through the `ciphertext_with_tag` assignment and `out.extend_from_slice(&ciphertext_with_tag)`) with:
  ```rust
      let params = Argon2idParams::new(KDF_M_KIB, KDF_T as u32, KDF_P as u32)
          .map_err(|e| SnapshotError::Argon2Fail(e.to_string()))?;
      let ciphertext_with_tag =
          password_envelope::seal(passphrase, &params, salt, nonce, HRSS_AAD, cbor.as_slice())
              .map_err(|_| SnapshotError::WrongPassphraseOrCorrupt)?;
      out.extend_from_slice(&ciphertext_with_tag);
      Ok(out)
  ```
  Note the AAD is `HRSS_AAD` (the constant string), **not** the header. Delete the fn's `use argon2…`/`use chacha20poly1305…`.

- [ ] **Step 7: Rewire HRSS `decode_snapshot` (`state_snapshot.rs:220`).** Keep all header parsing + the DoS param-equality guard. Replace the KDF+AEAD block (from `let params = Params::new(…)` through the `let cleartext = Zeroizing::new(cleartext);`) with:
  ```rust
      let params = Argon2idParams::new(m_kib, t, p).map_err(|e| SnapshotError::Argon2Fail(e.to_string()))?;
      let cleartext = password_envelope::open(passphrase, &params, salt, nonce, HRSS_AAD, ciphertext_with_tag)
          .map_err(|_| SnapshotError::WrongPassphraseOrCorrupt)?;
  ```
  `cleartext` is now `Zeroizing<Vec<u8>>`; the subsequent `from_reader(cleartext.as_slice())` is unchanged. Delete the fn's `use argon2…`/`use chacha20poly1305…`.

- [ ] **Step 8: Rewire the inline-Argon2 test `hrss_unknown_version_rejected` (`state_snapshot.rs:406`).** This test hand-builds a v=99 envelope with inline `argon2`/`chacha20poly1305` to prove decode rejects an unknown inner version. Replace its manual KDF+AEAD (the `use argon2…`, `Params::new`, `Argon2::new`, `hash_password_into`, and `cipher.encrypt(...)` section) with the primitive so no direct `argon2` use remains:
  ```rust
      use harmony_app::… // not needed; already in module scope
      let salt = [0u8; SALT_LEN];
      let nonce = [0u8; NONCE_LEN];
      let mut out = Vec::new();
      out.extend_from_slice(HRSS_MAGIC);
      out.push(HRSS_FORMAT_VERSION);
      out.push(HRSS_KDF_ID_ARGON2ID);
      out.extend_from_slice(&KDF_M_KIB.to_be_bytes());
      out.extend_from_slice(&KDF_T.to_be_bytes());
      out.push(KDF_P);
      out.extend_from_slice(&salt);
      out.extend_from_slice(&nonce);
      let params = Argon2idParams::new(KDF_M_KIB, KDF_T as u32, KDF_P as u32).unwrap();
      let ciphertext_with_tag =
          password_envelope::seal(b"pp", &params, &salt, &nonce, HRSS_AAD, cbor.as_slice()).unwrap();
      out.extend_from_slice(&ciphertext_with_tag);
  ```
  (The `use super::*;` at the top of the `tests` module already brings `password_envelope`/`Argon2idParams` into scope via the file-level import from Step 6. If not, add `use harmony_crypto::password_envelope::{self, Argon2idParams};` inside the test.) Leave the rest of the test (the `decode_snapshot(...).expect_err(...)` assertion) unchanged.

- [ ] **Step 9: Byte-identity gate.** From `src-tauri/`, the three pins + round-trips MUST pass with no fixture regeneration:
  ```bash
  cargo nextest run --locked --features test-fixtures \
    -E 'test(wire_format_v1_pinned) or test(wire_format_v2_pinned) or test(hrss_envelope_byte_pinned) or test(hrss) or test(vault) or test(decrypt) or test(rotation)' 2>&1 | tail -25
  git -C .. status --short src-tauri/tests/fixtures/   # MUST be empty — no fixture changed
  ```
  Expected: all PASS; `tests/fixtures/` clean.

- [ ] **Step 10: Scoped gate + commit.**
  ```bash
  cargo fmt --all
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
  # Task 1 changed Cargo.toml's dependency graph this session, so the module-mapped
  # iterative selector is unreliable here (it will bail with that warning). Use an
  # explicit scoped selection instead:
  cargo nextest run --locked --all-targets --features test-fixtures \
    -E 'test(identity) or test(state_snapshot) or test(vault) or test(wire_format) or test(owner_state)' 2>&1 | tail -20
  git -C .. add src-tauri/src/identity.rs src-tauri/src/state_snapshot.rs
  git -C .. commit -m "identity+state_snapshot: converge HRMI v1/v2 + HRSS onto password_envelope (byte-identical)"
  ```

---

### Task 4: Drop the now-unused `argon2` dep + final CI-parity sweep

**Files:**
- Modify: `src-tauri/Cargo.toml` (remove `argon2 = "0.5"`)
- Modify: `src-tauri/Cargo.lock`

- [ ] **Step 1: Confirm `argon2` is fully unused.** From `src-tauri/`:
  ```bash
  grep -rn 'argon2' src/ tests/ benches/ 2>/dev/null
  ```
  Expected: **no matches** (Task 3 removed the last direct uses). If any remain, STOP and report — do not remove the dep.

- [ ] **Step 2: Remove the dependency.** Delete the `argon2 = "0.5"` line (`src-tauri/Cargo.toml:165`). Leave `chacha20poly1305` (used by community_state_sync, community_channel_log, owner_state_crypto, voice_crypto, friend_rendezvous, file_stream_crypto, owner_state_types, dm_crypto, pairing/session).

- [ ] **Step 3: Regenerate lock + build.**
  ```bash
  cargo build --workspace 2>&1 | tail -10
  ```
  Expected: compiles; `git diff src-tauri/Cargo.lock` drops the client's direct `argon2` edge (argon2 may remain in the tree only under `harmony-crypto`'s optional dep — that is correct and expected).

- [ ] **Step 4: Full CI-parity gates.** From `src-tauri/`:
  ```bash
  cargo fmt --all -- --check
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
  cargo nextest run --locked --workspace --all-targets --features test-fixtures
  ```
  Expected: all green. Re-confirm `git status --short src-tauri/tests/fixtures/` is empty.

- [ ] **Step 5: Commit.**
  ```bash
  git add src-tauri/Cargo.toml src-tauri/Cargo.lock
  git commit -m "deps: drop now-unused direct argon2 (crypto routed through harmony-crypto)"
  ```

---

## Self-Review (controller, before dispatch)

- **Spec coverage:** DoD #2 (client rewired: identity v1+v2 + HRSS on the primitive; v0x02 pin added; existing wire/rotation/vault tests green) → Tasks 2+3. DoD #3 (gates green; CI + bots) → Task 4 + PR phase. Pin bump → Task 1.
- **Premise correction vs the ticket:** the ticket says "new golden fixtures for v0x02 **and HRSS** (both currently have NO byte-pin)." HRSS **is** already byte-pinned (`hrss_envelope_byte_pinned`, inline hex) — so only v0x02 needs a new fixture. Noted in the PR body; not a scope change (converge-all-three still holds).
- **Type consistency:** `open` → `Zeroizing<Vec<u8>>` matches `decrypt_v2_plaintext`/`decode_snapshot` returns; v0x01 `decrypt` copies into `Zeroizing<[u8; BLOB_LEN]>` via the retained tail. `Argon2idParams::new` fallibility mapped per-site (encode → `expect`/`Argon2Fail`; decode → indistinguishable error).
- **No placeholders:** every crypto site has complete replacement code.
