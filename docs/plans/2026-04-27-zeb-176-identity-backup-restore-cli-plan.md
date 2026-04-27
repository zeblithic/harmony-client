# ZEB-176 — Identity Backup/Restore CLI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire `harmony-owner::recovery` (mnemonic + encrypted-file backup primitives shipped in ZEB-175) into `harmony-app` as four CLI subcommands (`export mnemonic`, `export recovery-file`, `restore mnemonic`, `restore recovery-file`). Switches identity-at-rest from a 161-byte keypair blob to a 32-byte master seed; keypairs are re-derived on every load via the ZEB-177 seeded-keygen API.

**Architecture:** Three-layer change.

1. **Storage layer (`identity.rs`):** the on-disk wire format under the `HRMI` envelope shrinks from 161 bytes (versioned identity blob) to 32 bytes (seed). The `KeyStore` trait, all three impls (`FileStore`, `KeychainStore`, `EncryptedFileStore`), and the `encrypt_with_params` / `decrypt` helpers are retyped to operate on `[u8; 32]` seeds. The legacy plaintext migration path (`LegacyPlaintextReader`, the `.bak` cleanup) is removed wholesale; existing pre-ZEB-176 identities hard-fail on launch ("identity store payload length is unexpected") — acceptable per scope.
2. **Identity-construction layer (`NodeIdentity::from_seed`):** thin shim over `harmony_identity::PrivateIdentity::from_seed` and `PqPrivateIdentity::from_seed` (ZEB-177). Two seed-shaped public helpers (`read_seed_from_disk`, `write_seed_to_disk`) sit above the `KeyStore` trait and below `load_or_generate`, so the recovery CLI can read/write seeds without touching `NodeIdentity` directly.
3. **CLI layer (`recovery_cli.rs` + `main.rs`):** four entry points compose `read_seed_from_disk` / `write_seed_to_disk` with the `harmony_owner::recovery` API. A separate recovery-passphrase resolver reads `HARMONY_RECOVERY_PASSPHRASE` / `HARMONY_RECOVERY_PASSPHRASE_FILE` (deliberately disjoint from the at-rest passphrase). `main.rs` wires the clap subcommands; per-command stdout/stderr separation is the load-bearing UX (mnemonic export pipes to a file cleanly because the warning preamble + identity-hash go to stderr).

**Tech Stack:** Rust 2021, `harmony-owner` (git, branch=main, default `recovery` feature), `harmony-identity` (already a dep), `argon2` 0.5, `chacha20poly1305` 0.10, `clap` 4 (derive), `secrecy` 0.10, `zeroize` 1, `tempfile` 3 (dev), `serial_test` 3 (dev).

**Hard prerequisites already met:** ZEB-177 has landed on `origin/main` (commit `ad96840`); `cargo update -p harmony-identity` will pull `PrivateIdentity::from_seed` / `PqPrivateIdentity::from_seed`.

---

## Task 1: Add `harmony-owner` dependency

**Files:**
- Modify: `src-tauri/Cargo.toml` — add `harmony-owner` git dep
- Test: `cargo check -p harmony-app` confirms the dep resolves and pulls in the `recovery` feature.

- [ ] **Step 1: Add the dep**

In `src-tauri/Cargo.toml`, immediately after the existing `harmony-mailbox` line in the `[dependencies]` block, add:

```toml
harmony-owner = { git = "https://github.com/zeblithic/harmony.git", branch = "main", features = ["recovery"] }
```

The `recovery` feature is default-on in `harmony-owner` post ZEB-175; listing it explicitly matches the existing dep style and pins the surface we need.

- [ ] **Step 2: Refresh the Cargo.lock**

Run: `cd src-tauri && cargo update -p harmony-identity -p harmony-runtime -p harmony-content -p harmony-compute -p harmony-telemetry -p harmony-mailbox`
Expected: lockfile pulls the latest `main` commits for the existing harmony deps so `harmony-owner` shares the same workspace SHA. No errors.

- [ ] **Step 3: Verify the dep compiles**

Run: `cd src-tauri && cargo check -p harmony-app`
Expected: clean compile. The new dep introduces no callers yet — this just proves the version resolution works against the post-ZEB-177 harmony repo.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "deps(harmony-app): add harmony-owner with recovery feature (ZEB-176)"
```

---

## Task 2: `NodeIdentity::from_seed` shim

**Files:**
- Modify: `src-tauri/src/identity.rs` (add `impl NodeIdentity { pub fn from_seed(...) }`, add tests)

- [ ] **Step 1: Confirm the upstream API surface**

Run: `cargo doc -p harmony-identity --no-deps --open` (or `grep -n 'pub fn from_seed' ../../harmony/crates/harmony-identity/src/`)
Expected: `harmony_identity::PrivateIdentity::from_seed(seed: &[u8; 32]) -> Self` and `harmony_identity::PqPrivateIdentity::from_seed(seed: &[u8; 32]) -> Self`. Both infallible.

- [ ] **Step 2: Write the failing determinism test**

In `src-tauri/src/identity.rs`, find the `#[cfg(test)] mod tests {` block (around line 1370) and add the following test inside it:

```rust
#[test]
fn from_seed_yields_same_node_identity_across_launches() {
    let seed = [0x42u8; 32];
    let id_a = NodeIdentity::from_seed(&seed);
    let id_b = NodeIdentity::from_seed(&seed);
    assert_eq!(
        id_a.ed25519.to_private_bytes().as_slice(),
        id_b.ed25519.to_private_bytes().as_slice(),
        "Ed25519 sub-key must be deterministic across calls"
    );
    assert_eq!(
        id_a.pq.to_private_bytes().as_slice(),
        id_b.pq.to_private_bytes().as_slice(),
        "PQ sub-key must be deterministic across calls"
    );
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cd src-tauri && cargo test -p harmony-app --lib identity::tests::from_seed_yields_same_node_identity_across_launches`
Expected: compile error — `no function or associated item named 'from_seed' found for struct 'NodeIdentity'`.

- [ ] **Step 4: Add the shim**

In `src-tauri/src/identity.rs`, immediately after the `impl std::fmt::Debug for NodeIdentity` block (around line 67), insert:

```rust
impl NodeIdentity {
    /// Derive a `NodeIdentity` from a 32-byte master seed.
    ///
    /// Thin shim over `PrivateIdentity::from_seed` and
    /// `PqPrivateIdentity::from_seed` (ZEB-177). Deterministic: the same
    /// seed produces byte-identical sub-keys on every call. This is the
    /// load-bearing invariant for the seed-on-disk storage model — every
    /// launch reads the seed and re-derives the keypairs from scratch.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            pq: PqPrivateIdentity::from_seed(seed),
            ed25519: PrivateIdentity::from_seed(seed),
        }
    }
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cd src-tauri && cargo test -p harmony-app --lib identity::tests::from_seed_yields_same_node_identity_across_launches`
Expected: `test result: ok. 1 passed; 0 failed`.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/identity.rs
git commit -m "feat(identity): NodeIdentity::from_seed shim (ZEB-176)"
```

---

## Task 3: Switch identity-at-rest to 32-byte seed

**Files:**
- Modify: `src-tauri/src/identity.rs`
  - Constants: `BLOB_LEN: 32`, drop `PQ_KEY_LEN` / `ED25519_KEY_LEN` / `VERSION` (the seed is unversioned — it has no internal structure to version, and the envelope already carries `ENC_FORMAT_VERSION`).
  - Replace `identity_to_blob` / `blob_to_identity` with `seed_to_blob` / `blob_to_seed`.
  - Retype `KeyStore::load`/`save` to take `&[u8; 32]` / return `Zeroizing<[u8; 32]>` instead of `NodeIdentity`.
  - Update all three `KeyStore` impls (`FileStore` test-only, `KeychainStore`, `EncryptedFileStore`).
  - Retype `encrypt_with_params` and `decrypt` to operate on `[u8; 32]`.
  - Drop `LegacyPlaintextReader` and the legacy `.key` / `.bak` migration logic in `load_or_generate_with_stores`.
  - Replace fresh-generate path: random 32-byte seed via `OsRng.fill_bytes` → save seed → derive `NodeIdentity::from_seed`.
  - `verify_round_trip` operates on seed bytes (compare via `subtle::ConstantTimeEq`).
  - `cleanup_legacy_bak` is removed (legacy plaintext is gone).
- Modify: existing tests that reference `BLOB_LEN`, `ENC_FILE_LEN`, `identity_to_blob`, `blob_to_identity`, `LegacyPlaintextReader`, or the 230-byte fixture.

This task is the structural pivot of the PR. It must produce a clean compile + green test suite at commit time.

- [ ] **Step 1: Read the existing wire-format constants and helpers to plan the swap**

Run: `grep -n 'BLOB_LEN\|ENC_FILE_LEN\|identity_to_blob\|blob_to_identity\|LegacyPlaintextReader\|cleanup_legacy_bak' src-tauri/src/identity.rs`
Expected: ~50 hits across the file. Note especially the call sites in `load_or_generate_with_stores`, `load_or_generate_with_stores_post_probe`, `rotate_passphrase`, and the test fixtures (`pinned_v1_wire_fixture` is the most important pin to update).

- [ ] **Step 2: Write the failing seed round-trip test**

In `src-tauri/src/identity.rs`, inside the `#[cfg(test)] mod tests {` block, add:

```rust
#[test]
fn seed_round_trip_via_blob() {
    let seed = [0xABu8; 32];
    let blob = seed_to_blob(&seed);
    let recovered = blob_to_seed(blob.as_slice()).unwrap();
    assert_eq!(seed, *recovered, "seed must round-trip byte-for-byte through blob serialization");
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cd src-tauri && cargo test -p harmony-app --lib identity::tests::seed_round_trip_via_blob`
Expected: compile error — `cannot find function 'seed_to_blob' in this scope`.

- [ ] **Step 4: Replace the wire-format constants**

In `src-tauri/src/identity.rs`, replace the block at lines 28-31:

```rust
const VERSION: u8 = 0x01;
const PQ_KEY_LEN: usize = 96;
const ED25519_KEY_LEN: usize = 64;
const BLOB_LEN: usize = 1 + PQ_KEY_LEN + ED25519_KEY_LEN; // 161
```

with:

```rust
/// Plaintext payload protected by the `HRMI` envelope: the master 32-byte
/// seed. Sub-key derivation is deterministic via `NodeIdentity::from_seed`
/// — the seed is the only secret on disk.
const BLOB_LEN: usize = 32;
```

The `ENC_FILE_LEN: usize = HEADER_LEN + SALT_LEN + NONCE_LEN + BLOB_LEN + TAG_LEN` line at line 50 stays as-is — it auto-shifts to 101 once `BLOB_LEN` shrinks.

- [ ] **Step 5: Replace `identity_to_blob` / `blob_to_identity` with `seed_to_blob` / `blob_to_seed`**

Replace the block at lines 69-102:

```rust
// ── Serialization helpers (shared by both backends) ─────────────────────

/// Serialize a `NodeIdentity` into the 161-byte binary format.
fn identity_to_blob(identity: &NodeIdentity) -> Zeroizing<Vec<u8>> {
    let pq_bytes = Zeroizing::new(identity.pq.to_private_bytes());
    let ed_bytes = Zeroizing::new(identity.ed25519.to_private_bytes());
    let mut buf = Zeroizing::new(Vec::with_capacity(BLOB_LEN));
    buf.push(VERSION);
    buf.extend_from_slice(&pq_bytes);
    buf.extend_from_slice(ed_bytes.as_slice());
    debug_assert_eq!(buf.len(), BLOB_LEN, "identity blob length mismatch");
    buf
}

/// Deserialize a `NodeIdentity` from a 161-byte binary blob.
fn blob_to_identity(buf: &[u8]) -> Result<NodeIdentity, String> {
    if buf.len() != BLOB_LEN {
        return Err(format!(
            "Corrupt identity blob: expected {BLOB_LEN} bytes, got {}",
            buf.len()
        ));
    }
    if buf[0] != VERSION {
        return Err(format!(
            "Unsupported identity blob version: {:#04x}",
            buf[0]
        ));
    }
    let pq = PqPrivateIdentity::from_private_bytes(&buf[1..1 + PQ_KEY_LEN])
        .map_err(|e| format!("Corrupt PQ identity: {e}"))?;
    let ed25519 = PrivateIdentity::from_private_bytes(&buf[1 + PQ_KEY_LEN..])
        .map_err(|e| format!("Corrupt Ed25519 identity: {e}"))?;
    Ok(NodeIdentity { pq, ed25519 })
}
```

with:

```rust
// ── Serialization helpers (shared by both backends) ─────────────────────

/// Serialize a 32-byte seed into the on-disk binary format. Identity at this
/// layer is *just* the seed — the sub-keys are derived deterministically via
/// `NodeIdentity::from_seed` on every load.
fn seed_to_blob(seed: &[u8; BLOB_LEN]) -> Zeroizing<Vec<u8>> {
    let mut buf = Zeroizing::new(Vec::with_capacity(BLOB_LEN));
    buf.extend_from_slice(seed);
    debug_assert_eq!(buf.len(), BLOB_LEN, "seed blob length mismatch");
    buf
}

/// Deserialize a 32-byte seed from a binary blob.
fn blob_to_seed(buf: &[u8]) -> Result<Zeroizing<[u8; BLOB_LEN]>, String> {
    if buf.len() != BLOB_LEN {
        return Err(format!(
            "identity store payload length is unexpected: expected {BLOB_LEN} bytes, got {}",
            buf.len()
        ));
    }
    let mut out: Zeroizing<[u8; BLOB_LEN]> = Zeroizing::new([0u8; BLOB_LEN]);
    out.copy_from_slice(buf);
    Ok(out)
}
```

The error string `identity store payload length is unexpected` is the spec-mandated message for users upgrading past pre-ZEB-176 identities (the 161-byte payload won't match the 32-byte expectation).

- [ ] **Step 6: Retype `encrypt_with_params` and `decrypt`**

Replace the `pub fn encrypt_with_params` signature at line 301:

```rust
pub fn encrypt_with_params(
    passphrase: &[u8],
    salt: &[u8; SALT_LEN],
    nonce: &[u8; NONCE_LEN],
    blob: &[u8; BLOB_LEN],
) -> Vec<u8> {
```

stays the same shape (the `BLOB_LEN` reference auto-shifts to 32). The body's `debug_assert_eq!(ciphertext_with_tag.len(), BLOB_LEN + TAG_LEN);` and `debug_assert_eq!(out.len(), ENC_FILE_LEN);` remain correct.

For `pub fn decrypt`, the return type already says `Zeroizing<[u8; BLOB_LEN]>` — that auto-shifts. The internal error string at line 372-375 is now fired for *both* a corrupted file *and* a pre-ZEB-176 161-byte payload; that's intentional (we want a single error path for "file payload size is wrong").

The `plaintext_slice: &[u8; BLOB_LEN] = plaintext.as_slice().try_into()` at line 453 also auto-shifts — no body changes needed in `decrypt`.

- [ ] **Step 7: Retype the `KeyStore` trait**

Replace the `KeyStore` trait at lines 462-470:

```rust
pub trait KeyStore {
    /// Load identity from this store. Returns `Ok(None)` if no entry exists.
    fn load(&self) -> Result<Option<NodeIdentity>, String>;
    /// Save identity to this store.
    fn save(&self, identity: &NodeIdentity) -> Result<(), String>;
}
```

with:

```rust
pub trait KeyStore {
    /// Load the master seed from this store. Returns `Ok(None)` if no entry exists.
    fn load(&self) -> Result<Option<Zeroizing<[u8; BLOB_LEN]>>, String>;
    /// Save the master seed to this store.
    fn save(&self, seed: &[u8; BLOB_LEN]) -> Result<(), String>;
}
```

- [ ] **Step 8: Update `FileStore` impl (test-only legacy plaintext writer)**

Replace the `#[cfg(test)] impl KeyStore for FileStore` block at lines 510-528:

```rust
#[cfg(test)]
impl KeyStore for FileStore {
    fn load(&self) -> Result<Option<NodeIdentity>, String> {
        let raw = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("Failed to read {}: {e}", self.path.display())),
        };
        let buf = Zeroizing::new(raw);
        let identity = blob_to_identity(&buf)?;
        #[cfg(unix)]
        warn_permissions(&self.path);
        Ok(Some(identity))
    }

    fn save(&self, identity: &NodeIdentity) -> Result<(), String> {
        let blob = identity_to_blob(identity);
        write_atomic_0600(&self.path, &blob)
    }
}
```

with:

```rust
#[cfg(test)]
impl KeyStore for FileStore {
    fn load(&self) -> Result<Option<Zeroizing<[u8; BLOB_LEN]>>, String> {
        let raw = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("Failed to read {}: {e}", self.path.display())),
        };
        let buf = Zeroizing::new(raw);
        let seed = blob_to_seed(&buf)?;
        #[cfg(unix)]
        warn_permissions(&self.path);
        Ok(Some(seed))
    }

    fn save(&self, seed: &[u8; BLOB_LEN]) -> Result<(), String> {
        let blob = seed_to_blob(seed);
        write_atomic_0600(&self.path, &blob)
    }
}
```

- [ ] **Step 9: Delete `LegacyPlaintextReader`**

Remove the entire block at lines 531-567 (`// ── LegacyPlaintextReader ───`, `pub(crate) struct LegacyPlaintextReader`, `impl LegacyPlaintextReader`). Per spec: pre-ZEB-176 plaintext identities are a placeholder format with no migration path.

- [ ] **Step 10: Update `KeychainStore` impl**

Replace the `impl KeyStore for KeychainStore` block at lines 616-635:

```rust
impl KeyStore for KeychainStore {
    fn load(&self) -> Result<Option<NodeIdentity>, String> {
        match self.entry.get_secret() {
            Ok(bytes) => {
                let buf = Zeroizing::new(bytes);
                let identity = blob_to_identity(&buf)?;
                Ok(Some(identity))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("keychain load failed: {e}")),
        }
    }

    fn save(&self, identity: &NodeIdentity) -> Result<(), String> {
        let blob = identity_to_blob(identity);
        self.entry
            .set_secret(&blob)
            .map_err(|e| format!("keychain save failed: {e}"))
    }
}
```

with:

```rust
impl KeyStore for KeychainStore {
    fn load(&self) -> Result<Option<Zeroizing<[u8; BLOB_LEN]>>, String> {
        match self.entry.get_secret() {
            Ok(bytes) => {
                let buf = Zeroizing::new(bytes);
                let seed = blob_to_seed(&buf)?;
                Ok(Some(seed))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("keychain load failed: {e}")),
        }
    }

    fn save(&self, seed: &[u8; BLOB_LEN]) -> Result<(), String> {
        let blob = seed_to_blob(seed);
        self.entry
            .set_secret(&blob)
            .map_err(|e| format!("keychain save failed: {e}"))
    }
}
```

- [ ] **Step 11: Update `EncryptedFileStore` impl**

Replace the `impl KeyStore for EncryptedFileStore` block at lines 773-810:

```rust
impl KeyStore for EncryptedFileStore {
    fn load(&self) -> Result<Option<NodeIdentity>, String> {
        let raw = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("Failed to read {}: {e}", self.path.display())),
        };
        // `decrypt` returns Zeroizing<[u8; BLOB_LEN]> — the stack array's bytes
        // are wiped on drop. blob_to_identity reads the slice without copying.
        let blob = decrypt(self.passphrase.expose_secret().as_bytes(), &raw)?;
        let identity = blob_to_identity(blob.as_slice())?;
        Ok(Some(identity))
    }

    fn save(&self, identity: &NodeIdentity) -> Result<(), String> {
        let blob = identity_to_blob(identity);
        // Wrap the fixed-size copy in Zeroizing so the second plaintext-key
        // buffer is wiped on drop. The original `blob: Zeroizing<Vec<u8>>` is
        // already protected; without this, dropping the owned `[u8; BLOB_LEN]`
        // at end of scope would leave key bytes on the stack.
        let mut blob_arr: Zeroizing<[u8; BLOB_LEN]> = Zeroizing::new([0u8; BLOB_LEN]);
        blob_arr.copy_from_slice(blob.as_slice());

        let mut salt = [0u8; SALT_LEN];
        let mut nonce = [0u8; NONCE_LEN];
        use rand::RngCore;
        rand::rngs::OsRng.fill_bytes(&mut salt);
        rand::rngs::OsRng.fill_bytes(&mut nonce);

        let bytes = encrypt_with_params(
            self.passphrase.expose_secret().as_bytes(),
            &salt,
            &nonce,
            &blob_arr,
        );
        write_atomic_0600(&self.path, &bytes)
    }
}
```

with:

```rust
impl KeyStore for EncryptedFileStore {
    fn load(&self) -> Result<Option<Zeroizing<[u8; BLOB_LEN]>>, String> {
        let raw = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("Failed to read {}: {e}", self.path.display())),
        };
        // `decrypt` returns Zeroizing<[u8; BLOB_LEN]> — the underlying [u8; 32]
        // is wiped on drop. The seed array can be returned directly without
        // a second deserialization step.
        let seed = decrypt(self.passphrase.expose_secret().as_bytes(), &raw)?;
        Ok(Some(seed))
    }

    fn save(&self, seed: &[u8; BLOB_LEN]) -> Result<(), String> {
        let mut salt = [0u8; SALT_LEN];
        let mut nonce = [0u8; NONCE_LEN];
        use rand::RngCore;
        rand::rngs::OsRng.fill_bytes(&mut salt);
        rand::rngs::OsRng.fill_bytes(&mut nonce);

        let bytes = encrypt_with_params(
            self.passphrase.expose_secret().as_bytes(),
            &salt,
            &nonce,
            seed,
        );
        write_atomic_0600(&self.path, &bytes)
    }
}
```

The `seed_to_blob` indirection is unnecessary here — the `[u8; BLOB_LEN]` is already the on-the-wire shape and `encrypt_with_params` takes a `&[u8; BLOB_LEN]` directly.

- [ ] **Step 12: Update `verify_round_trip`**

Find the existing `verify_round_trip` (search: `grep -n 'fn verify_round_trip' src-tauri/src/identity.rs` — around line 200). Replace its body to compare seed bytes constant-time-equal:

```rust
fn verify_round_trip(store: &dyn KeyStore, expected: &[u8; BLOB_LEN]) -> Result<(), String> {
    let loaded = store.load()
        .map_err(|e| format!("verify-after-write read-back failed: {e}"))?
        .ok_or_else(|| "verify-after-write read-back returned None".to_string())?;
    use subtle::ConstantTimeEq;
    if !bool::from(loaded.as_slice().ct_eq(expected.as_slice())) {
        return Err("verify-after-write returned a different seed than was just written".to_string());
    }
    Ok(())
}
```

The function previously took `&NodeIdentity`; updating to `&[u8; BLOB_LEN]` ripples through the two call sites in `save_with_fallback`. Update those next.

- [ ] **Step 13: Update `save_with_fallback` and the resolution chain**

In `save_with_fallback` (lines 961-1014), every `&id: &NodeIdentity` parameter and every `verify_round_trip(kc, id)` / `verify_round_trip(enc, id)` call needs the seed-shaped signature. Replace the function signature:

```rust
fn save_with_fallback(
    keychain_healthy: bool,
    keychain: Option<&KeychainStore>,
    encrypted: Option<&EncryptedFileStore>,
    id: &NodeIdentity,
    no_dest_err: impl FnOnce() -> String,
    keychain_failed_no_enc_err: impl FnOnce(&str) -> String,
) -> Result<(), String> {
```

with:

```rust
fn save_with_fallback(
    keychain_healthy: bool,
    keychain: Option<&KeychainStore>,
    encrypted: Option<&EncryptedFileStore>,
    seed: &[u8; BLOB_LEN],
    no_dest_err: impl FnOnce() -> String,
    keychain_failed_no_enc_err: impl FnOnce(&str) -> String,
) -> Result<(), String> {
```

In the body, replace every `kc.save(id)` with `kc.save(seed)`, every `enc.save(id)` with `enc.save(seed)`, every `verify_round_trip(kc, id)` with `verify_round_trip(kc, seed)`, every `verify_round_trip(enc, id)` with `verify_round_trip(enc, seed)`.

- [ ] **Step 14: Replace `load_or_generate_with_stores` with the seed-shaped version (drop legacy migration)**

Replace the entire `load_or_generate_with_stores` and `load_or_generate_with_stores_post_probe` block at lines 843-949 with:

```rust
/// Internal resolution chain — accepts injected stores for testability.
///
/// Resolution order:
///   1. keychain.load() — return on success; fall through on None or Err
///   2. encrypted.load() — return on success; HARD FAIL on Err (wrong
///      passphrase / corruption — never silently regenerate)
///   3. fresh generate → save 32B seed to keychain (preferred) or encrypted
///
/// Hard-fails when no destination is available (no keychain, no encrypted store).
/// Pre-ZEB-176 plaintext `~/.harmony/identity.key` files are no longer
/// auto-migrated — users with a placeholder pre-ZEB-176 identity hard-fail
/// and re-mint (acceptable per spec scope).
fn load_or_generate_with_stores(
    keychain: Option<&KeychainStore>,
    encrypted: Option<&EncryptedFileStore>,
) -> Result<Zeroizing<[u8; BLOB_LEN]>, String> {
    let mut keychain_healthy = false;
    if let Some(kc) = keychain {
        match kc.load() {
            Ok(Some(seed)) => return Ok(seed),
            Ok(None) => keychain_healthy = true,
            Err(e) => {
                tracing::warn!("keychain load failed, trying next store: {e}");
            }
        }
    }
    load_or_generate_with_stores_post_probe(keychain, keychain_healthy, encrypted)
}

fn load_or_generate_with_stores_post_probe(
    keychain: Option<&KeychainStore>,
    keychain_healthy: bool,
    encrypted: Option<&EncryptedFileStore>,
) -> Result<Zeroizing<[u8; BLOB_LEN]>, String> {
    if let Some(enc) = encrypted {
        match enc.load() {
            Ok(Some(seed)) => return Ok(seed),
            Ok(None) => { /* fall through to fresh-generate */ }
            Err(e) => return Err(e),
        }
    }

    let mut seed_buf: Zeroizing<[u8; BLOB_LEN]> = Zeroizing::new([0u8; BLOB_LEN]);
    use rand::RngCore;
    rand::rngs::OsRng.fill_bytes(seed_buf.as_mut());
    save_with_fallback(
        keychain_healthy,
        keychain,
        encrypted,
        &seed_buf,
        || "no identity store available: keychain unavailable and HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set — see docs/headless-install.md".to_string(),
        |e| format!(
            "keychain save failed and no encrypted fallback configured: {e} — see docs/headless-install.md"
        ),
    )?;
    Ok(seed_buf)
}
```

The `plaintext_path: &Path` parameter is gone — there's no legacy plaintext migration to point at. The `cleanup_legacy_bak` helper and its call sites can also be removed (search and delete).

- [ ] **Step 15: Update `load_or_generate` and `load_or_generate_with_keychain`**

Replace `load_or_generate` and `load_or_generate_with_keychain` (lines 1030-1094) with:

```rust
/// Public entry point — resolves env-derived encrypted store, attempts the
/// keychain, and runs the resolution chain. Returns the derived `NodeIdentity`.
pub fn load_or_generate(plaintext_path: &Path) -> Result<NodeIdentity, String> {
    let seed = read_seed_from_disk_with_keychain(plaintext_path, KeychainStore::new().ok())?;
    Ok(NodeIdentity::from_seed(&seed))
}

pub(crate) fn load_or_generate_with_keychain(
    plaintext_path: &Path,
    keychain: Option<KeychainStore>,
) -> Result<NodeIdentity, String> {
    let seed = read_seed_from_disk_with_keychain(plaintext_path, keychain)?;
    Ok(NodeIdentity::from_seed(&seed))
}
```

The `plaintext_path` argument is preserved only because `read_seed_from_disk_with_keychain` (added in Task 4) still needs it to resolve the sibling `identity.enc` path. The legacy `.key` file is no longer read.

- [ ] **Step 16: Update existing tests that reference the old wire format**

Run: `grep -n 'BLOB_LEN\|ENC_FILE_LEN\|identity_to_blob\|blob_to_identity\|LegacyPlaintextReader\|legacy.*plaintext\|cleanup_legacy_bak\|161\|230' src-tauri/src/identity.rs` to enumerate all affected tests.

For each hit:

- Tests that reference `BLOB_LEN` or `ENC_FILE_LEN` *as a name* keep working — the constants auto-resolve to the new values.
- Tests with literal `230` (ENC_FILE_LEN) bytes: replace with `101`. The most important is `pinned_v1_wire_fixture` (around line 1380) — the entire ciphertext blob needs regeneration. To do so, run the existing fixture in isolation, observe the printed bytes, and paste them in. Alternatively, switch the test to assert structural invariants (magic, version, kdf params, total length) without pinning the ciphertext bytes — the upstream `harmony-owner::recovery` already pins recovery-file bytes; the local `HRMI` envelope is not a portable artifact and a structural check is sufficient.
  - Recommended approach: structural-check rewrite. Replace the byte-pinning lines with assertions on `bytes.len() == ENC_FILE_LEN`, `&bytes[0..4] == b"HRMI"`, `bytes[4] == 0x01` (version), `bytes[5] == 0x01` (kdf id), and round-trip via `decrypt`.
- Tests that call `identity_to_blob` / `blob_to_identity`: rewrite using `seed_to_blob` / `blob_to_seed` and a stub seed (`[0xABu8; 32]`). Most of these are sanity checks of the serialization shape — the post-rewrite version is mechanically simpler.
- Tests that call `LegacyPlaintextReader`: delete the entire test (legacy migration is gone).
- Tests that exercise `cleanup_legacy_bak` / `.bak` rename: delete (likewise gone).
- Tests that build a `NodeIdentity` for save/load asserts: rewrite to operate on a 32-byte seed and assert seed equality on round-trip.

- [ ] **Step 17: Update the integration wire-format fixture**

The integration test at `src-tauri/tests/wire_format_fixture.rs` pins the v1 envelope as 230 bytes around a 161-byte payload. Update it to match the new shape.

In `src-tauri/tests/wire_format_fixture.rs`:

- Change `const TEST_BLOB: [u8; 161] = [0x42; 161];` to `const TEST_BLOB: [u8; 32] = [0x42; 32];`.
- Change `assert_eq!(bytes.len(), 230, ...)` to `assert_eq!(bytes.len(), 101, "v1 format must be exactly 101 bytes");`.
- Update the panic message after `Fixture missing at {}` if it references a byte count.
- Delete the existing `src-tauri/tests/fixtures/encrypted_v1.bin` (it's a 230-byte blob from the old format).

Regenerate the fixture once:

Run: `cd src-tauri && rm -f tests/fixtures/encrypted_v1.bin && HARMONY_REGENERATE_WIRE_FIXTURE=1 cargo test -p harmony-app --test wire_format_fixture`
Expected: prints `Regenerated fixture at <path>`, exits 0.

Re-run without the flag to confirm the pin holds:

Run: `cd src-tauri && cargo test -p harmony-app --test wire_format_fixture`
Expected: passes against the freshly committed fixture.

- [ ] **Step 18: Run the test suite to verify everything compiles and passes**

Run: `cd src-tauri && cargo test -p harmony-app --lib identity::`
Expected: all tests pass. If a test fails because the legacy migration assumption baked in, delete it (legacy migration is gone — no replacement test needed; the new `seed_round_trip_via_blob` plus the structural fixture check carry the wire-format assertions).

- [ ] **Step 19: Run clippy**

Run: `cd src-tauri && cargo clippy -p harmony-app --lib -- -D warnings`
Expected: clean. If clippy flags an unused `VERSION` / `PQ_KEY_LEN` / `ED25519_KEY_LEN` constant, those should already be deleted in Step 4.

- [ ] **Step 20: Commit**

```bash
git add src-tauri/src/identity.rs src-tauri/tests/wire_format_fixture.rs src-tauri/tests/fixtures/encrypted_v1.bin
git commit -m "refactor(identity): switch identity-at-rest to 32B master seed (ZEB-176)"
```

---

## Task 4: `read_seed_from_disk` and `write_seed_to_disk`

**Files:**
- Modify: `src-tauri/src/identity.rs` — add the two seed-shaped public helpers

These helpers sit above the `KeyStore` trait and below `load_or_generate`. The recovery CLI consumes them directly so it can read/write seeds without ever materializing a `NodeIdentity`.

- [ ] **Step 1: Write the failing `read_seed_from_disk` test**

In `src-tauri/src/identity.rs`, inside the `#[cfg(test)] mod tests {` block, add:

```rust
#[test]
#[serial]
fn read_seed_round_trips_via_encrypted_file() {
    use secrecy::SecretString;
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");
    let enc_path = dir.path().join("identity.enc");

    // Set up an encrypted store with a known passphrase, write a known seed.
    std::env::set_var("HARMONY_PASSPHRASE", "round-trip-test");
    let store = EncryptedFileStore::new(enc_path.clone(), SecretString::from("round-trip-test".to_string()));
    let written = [0xCDu8; 32];
    store.save(&written).expect("save");

    // Read it back through the public seed-shaped helper.
    let loaded = read_seed_from_disk_with_keychain(&plaintext_path, None).expect("read");
    assert_eq!(*loaded, written, "seed must round-trip through the encrypted store");

    std::env::remove_var("HARMONY_PASSPHRASE");
}
```

The `#[serial]` attribute (from `serial_test`) keeps env-var-touching tests from racing.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd src-tauri && cargo test -p harmony-app --lib identity::tests::read_seed_round_trips_via_encrypted_file`
Expected: compile error — `cannot find function 'read_seed_from_disk_with_keychain' in this scope`.

- [ ] **Step 3: Add `read_seed_from_disk_with_keychain` and the public `read_seed_from_disk`**

In `src-tauri/src/identity.rs`, immediately after `load_or_generate_with_keychain`, add:

```rust
/// Read the master seed from disk via the standard resolution chain
/// (keychain → encrypted file → fresh-generate). Returns the seed bytes
/// directly so the recovery CLI can encode them without first deriving a
/// `NodeIdentity`.
pub fn read_seed_from_disk(plaintext_path: &Path) -> Result<Zeroizing<[u8; BLOB_LEN]>, String> {
    read_seed_from_disk_with_keychain(plaintext_path, KeychainStore::new().ok())
}

/// Inner entry point. Integration tests (across the crate boundary) inject a
/// deterministic keychain. `pub` rather than `pub(crate)` so
/// `tests/recovery_cli_integration.rs` can reach it.
pub fn read_seed_from_disk_with_keychain(
    plaintext_path: &Path,
    keychain: Option<KeychainStore>,
) -> Result<Zeroizing<[u8; BLOB_LEN]>, String> {
    let mut keychain_probe_ok = false;
    if let Some(kc) = &keychain {
        match kc.load() {
            Ok(Some(seed)) => return Ok(seed),
            Ok(None) => keychain_probe_ok = true,
            Err(e) => {
                tracing::warn!(
                    "keychain probe failed in read_seed_from_disk ({e}); env-var \
                     configuration errors will stay fatal"
                );
            }
        }
    }

    let enc_path = plaintext_path.with_file_name("identity.enc");
    let encrypted = match EncryptedFileStore::from_env(enc_path) {
        Ok(opt) => opt,
        Err(e) if keychain_probe_ok => {
            tracing::warn!(
                "HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE configured but invalid \
                 ({e}); ignoring — keychain is available as fallback"
            );
            None
        }
        Err(e) => return Err(e),
    };

    load_or_generate_with_stores(keychain.as_ref(), encrypted.as_ref())
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cd src-tauri && cargo test -p harmony-app --lib identity::tests::read_seed_round_trips_via_encrypted_file`
Expected: `test result: ok. 1 passed`.

- [ ] **Step 5: Write the failing `write_seed_to_disk` refusal test**

Inside the same test module, add:

```rust
#[test]
#[serial]
fn write_seed_refuses_when_identity_exists_without_force() {
    use secrecy::SecretString;
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");
    let enc_path = dir.path().join("identity.enc");

    std::env::set_var("HARMONY_PASSPHRASE", "refuse-test");
    let existing_seed = [0x11u8; 32];
    let store = EncryptedFileStore::new(enc_path.clone(), SecretString::from("refuse-test".to_string()));
    store.save(&existing_seed).unwrap();

    let new_seed = [0x22u8; 32];
    let err = write_seed_to_disk(&plaintext_path, &new_seed, /*force=*/ false)
        .expect_err("must refuse when destination exists");
    assert!(err.contains("identity already exists"), "actual: {err}");
    assert!(err.contains("--force"), "actual: {err}");

    std::env::remove_var("HARMONY_PASSPHRASE");
}

#[test]
#[serial]
fn write_seed_with_force_overwrites_existing() {
    use secrecy::SecretString;
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");
    let enc_path = dir.path().join("identity.enc");

    std::env::set_var("HARMONY_PASSPHRASE", "force-test");
    let existing_seed = [0x33u8; 32];
    let store = EncryptedFileStore::new(enc_path.clone(), SecretString::from("force-test".to_string()));
    store.save(&existing_seed).unwrap();

    let new_seed = [0x44u8; 32];
    write_seed_to_disk(&plaintext_path, &new_seed, /*force=*/ true).expect("force must succeed");

    let reloaded = read_seed_from_disk(&plaintext_path).expect("reload");
    assert_eq!(*reloaded, new_seed, "after force-overwrite, the new seed must be present");

    std::env::remove_var("HARMONY_PASSPHRASE");
}
```

- [ ] **Step 6: Run the tests to verify they fail**

Run: `cd src-tauri && cargo test -p harmony-app --lib identity::tests::write_seed`
Expected: compile error — `cannot find function 'write_seed_to_disk'`.

- [ ] **Step 7: Add `write_seed_to_disk`**

After `read_seed_from_disk_with_keychain`, add:

```rust
/// Write the master seed to disk via the standard resolution chain
/// (keychain preferred, encrypted-file fallback). Refuses if a destination
/// already exists unless `force` is true; with `force = true`, overwrites
/// in place via the existing atomic `create_new` tmp-then-rename pattern in
/// `save_with_fallback`.
pub fn write_seed_to_disk(
    plaintext_path: &Path,
    seed: &[u8; BLOB_LEN],
    force: bool,
) -> Result<(), String> {
    write_seed_to_disk_with_keychain(plaintext_path, seed, force, KeychainStore::new().ok())
}

pub fn write_seed_to_disk_with_keychain(
    plaintext_path: &Path,
    seed: &[u8; BLOB_LEN],
    force: bool,
    keychain: Option<KeychainStore>,
) -> Result<(), String> {
    if !force {
        // Refuse if either destination has an existing identity.
        // Check the keychain first (cheap probe), then the encrypted file path.
        if let Some(kc) = &keychain {
            if matches!(kc.load(), Ok(Some(_))) {
                return Err(format!(
                    "identity already exists in OS keychain; pass --force to overwrite (this is destructive)"
                ));
            }
        }
        let enc_path = plaintext_path.with_file_name("identity.enc");
        if enc_path.exists() {
            return Err(format!(
                "identity already exists at {}; pass --force to overwrite (this is destructive)",
                enc_path.display()
            ));
        }
    }

    let mut keychain_healthy = false;
    if let Some(kc) = &keychain {
        // Use load() as a connectivity probe (Ok(_) means responsive).
        if kc.load().is_ok() {
            keychain_healthy = true;
        }
    }

    let enc_path = plaintext_path.with_file_name("identity.enc");
    let encrypted = EncryptedFileStore::from_env(enc_path)?;

    save_with_fallback(
        keychain_healthy,
        keychain.as_ref(),
        encrypted.as_ref(),
        seed,
        || "no identity store available: keychain unavailable and HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set — see docs/headless-install.md".to_string(),
        |e| format!(
            "keychain save failed and no encrypted fallback configured: {e} — see docs/headless-install.md"
        ),
    )
}
```

The keychain `load()` "Ok means responsive" probe matches the discriminator already used in `load_or_generate_with_keychain`.

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cd src-tauri && cargo test -p harmony-app --lib identity::tests::write_seed`
Expected: both tests pass.

- [ ] **Step 9: Run the full identity test module**

Run: `cd src-tauri && cargo test -p harmony-app --lib identity::`
Expected: all tests still green.

- [ ] **Step 10: Commit**

```bash
git add src-tauri/src/identity.rs
git commit -m "feat(identity): read_seed_from_disk + write_seed_to_disk helpers (ZEB-176)"
```

---

## Task 5: `recovery_cli.rs` — recovery passphrase resolver + four CLI entry points

**Files:**
- Create: `src-tauri/src/recovery_cli.rs`
- Modify: `src-tauri/src/lib.rs` — `pub mod recovery_cli;` declaration

This module is the thin shim layer between the harmony-owner recovery library and the at-rest seed store.

- [ ] **Step 1: Add the module declaration in `lib.rs`**

In `src-tauri/src/lib.rs`, find the existing module declarations (`mod identity;`, `mod follows;`, etc., near the top of the file) and add:

```rust
pub mod recovery_cli;
```

Use `pub mod` rather than `mod` so the integration tests in `tests/recovery_cli_integration.rs` can reach it.

- [ ] **Step 2: Create the file with the recovery-passphrase resolver and a single failing test**

Create `src-tauri/src/recovery_cli.rs` with:

```rust
//! CLI subcommand entry points for identity backup/restore.
//!
//! Each entry point composes [`crate::identity::read_seed_from_disk`] /
//! [`crate::identity::write_seed_to_disk`] with the appropriate
//! [`harmony_owner::recovery`] API. The recovery passphrase
//! (`HARMONY_RECOVERY_PASSPHRASE` / `HARMONY_RECOVERY_PASSPHRASE_FILE`) is
//! resolved separately from the at-rest passphrase
//! (`HARMONY_PASSPHRASE` / `HARMONY_PASSPHRASE_FILE`) — neither variable
//! falls back to the other.

use std::path::Path;

use harmony_owner::recovery::{RecoveryArtifact, RecoveryMetadata};
use secrecy::SecretString;
use zeroize::Zeroizing;

use crate::identity;

/// Resolve the recovery passphrase from `HARMONY_RECOVERY_PASSPHRASE` or
/// `HARMONY_RECOVERY_PASSPHRASE_FILE`. Hard-fails if neither is set, with
/// a pointer to docs/headless-install.md.
///
/// Mirrors `EncryptedFileStore::from_env` but for the disjoint recovery vars.
pub(crate) fn resolve_recovery_passphrase() -> Result<SecretString, String> {
    let direct = std::env::var("HARMONY_RECOVERY_PASSPHRASE").ok();
    let file_path = std::env::var("HARMONY_RECOVERY_PASSPHRASE_FILE").ok();

    if direct.is_some() && file_path.is_some() {
        tracing::warn!(
            "both HARMONY_RECOVERY_PASSPHRASE and HARMONY_RECOVERY_PASSPHRASE_FILE are set; \
             HARMONY_RECOVERY_PASSPHRASE takes precedence"
        );
    }

    let s = if let Some(s) = direct {
        if s.is_empty() {
            return Err("HARMONY_RECOVERY_PASSPHRASE is set to an empty string".to_string());
        }
        s
    } else if let Some(file_path) = file_path {
        identity::parse_passphrase_file(Path::new(&file_path))
            .map_err(|e| format!("HARMONY_RECOVERY_PASSPHRASE_FILE={file_path} {e}"))?
    } else {
        return Err(
            "neither HARMONY_RECOVERY_PASSPHRASE nor HARMONY_RECOVERY_PASSPHRASE_FILE is set — see docs/headless-install.md"
                .to_string(),
        );
    };

    Ok(SecretString::from(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn recovery_passphrase_neither_set_fails_with_pointer_to_docs() {
        // Ensure both env vars are unset before the assertion.
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE_FILE");

        let err = resolve_recovery_passphrase().expect_err("must hard-fail when neither is set");
        assert!(err.contains("HARMONY_RECOVERY_PASSPHRASE"), "actual: {err}");
        assert!(err.contains("docs/headless-install.md"), "actual: {err}");
    }
}
```

Note: `crate::identity::parse_passphrase_file` is `pub(crate)` per existing identity.rs source — it's already callable from `recovery_cli`.

- [ ] **Step 3: Run the test to verify it passes (function compiles + test asserts hold)**

Run: `cd src-tauri && cargo test -p harmony-app --lib recovery_cli::tests::recovery_passphrase_neither_set_fails_with_pointer_to_docs`
Expected: pass. (No iteration loop here — the function is small enough that test-fail-then-implement is overkill; the test pins the contract that "neither set" raises a docs-pointer error.)

- [ ] **Step 4: Add the `export_mnemonic_cli` function with a unit test**

Append to `src-tauri/src/recovery_cli.rs`:

```rust
/// Export the master seed as a 24-word BIP39 English mnemonic.
///
/// Side effects:
///   - Reads the seed via the standard resolution chain (keychain → encrypted file).
///   - Writes the bare 24 words on a single line to stdout, terminated by `\n`.
///   - Writes a warning preamble + `identity-hash: <hex32>` to stderr.
///
/// Stdout/stderr separation is the load-bearing UX: `harmony-app export
/// mnemonic > backup.txt` writes only the words; running interactively shows
/// the warning + fingerprint on the terminal.
pub fn export_mnemonic_cli(plaintext_path: &Path) -> Result<(), String> {
    let seed = identity::read_seed_from_disk(plaintext_path)?;
    let artifact = RecoveryArtifact::from_seed(*seed);
    let mnemonic = artifact.to_mnemonic();
    let id_hash = artifact.master_pubkey_bundle().identity_hash();

    eprintln!("*** Identity recovery mnemonic ***");
    eprintln!("Write these 24 words on paper. Anyone with these");
    eprintln!("words can impersonate you. Storing in a digital");
    eprintln!("file is dangerous.");
    eprintln!();
    eprintln!("identity-hash: {}", hex::encode(id_hash));

    println!("{}", mnemonic.as_str());
    Ok(())
}
```

The `*seed` deref turns `Zeroizing<[u8; 32]>` into a `[u8; 32]` (Copy) — `RecoveryArtifact::from_seed` takes ownership; the original `seed` continues to zeroize on drop.

- [ ] **Step 5: Add the `export_recovery_file_cli` function**

Append to `src-tauri/src/recovery_cli.rs`:

```rust
/// Export the master seed as a passphrase-encrypted recovery file at `out`.
///
/// Reads the seed via the standard resolution chain. The recovery passphrase
/// is read from `HARMONY_RECOVERY_PASSPHRASE` / `HARMONY_RECOVERY_PASSPHRASE_FILE`
/// (DISTINCT from the at-rest `HARMONY_PASSPHRASE`).
///
/// Stdout: nothing. Stderr: `wrote <PATH> (<NN> bytes)\nidentity-hash: <hex32>`.
pub fn export_recovery_file_cli(
    plaintext_path: &Path,
    out: &Path,
    comment: Option<&str>,
) -> Result<(), String> {
    let seed = identity::read_seed_from_disk(plaintext_path)?;
    let passphrase = resolve_recovery_passphrase()?;
    let artifact = RecoveryArtifact::from_seed(*seed);
    let metadata = RecoveryMetadata {
        mint_at: None,
        comment: comment.map(str::to_string),
    };
    let bytes = artifact
        .to_encrypted_file(&passphrase, &metadata)
        .map_err(|e| format!("Error: {e}"))?;
    let id_hash = artifact.master_pubkey_bundle().identity_hash();

    std::fs::write(out, &bytes)
        .map_err(|e| format!("Error: failed to write {}: {e}", out.display()))?;

    eprintln!("wrote {} ({} bytes)", out.display(), bytes.len());
    eprintln!("identity-hash: {}", hex::encode(id_hash));
    Ok(())
}
```

- [ ] **Step 6: Add the `restore_mnemonic_cli` function**

Append to `src-tauri/src/recovery_cli.rs`:

```rust
/// Restore the master seed from a 24-word mnemonic file.
///
/// Reads the mnemonic from `mnemonic_file` (whitespace-tolerant,
/// case-insensitive, ASCII-only — non-ASCII rejected). Writes the seed via
/// the standard resolution chain. Refuses if an identity already exists
/// unless `force` is true.
///
/// Stdout: nothing. Stderr: `restored identity-hash: <hex32>`.
pub fn restore_mnemonic_cli(
    plaintext_path: &Path,
    mnemonic_file: &Path,
    force: bool,
) -> Result<(), String> {
    // Read the mnemonic file. Wrap in Zeroizing so the contents do not linger.
    let raw = std::fs::read_to_string(mnemonic_file)
        .map_err(|e| format!("Error: failed to read {}: {e}", mnemonic_file.display()))?;
    let raw = Zeroizing::new(raw);

    let artifact = RecoveryArtifact::from_mnemonic(raw.as_str())
        .map_err(|e| format!("Error: {e}"))?;
    let seed_bytes: Zeroizing<[u8; 32]> = Zeroizing::new(*artifact.as_bytes());
    let id_hash = artifact.master_pubkey_bundle().identity_hash();

    identity::write_seed_to_disk(plaintext_path, &seed_bytes, force)?;
    eprintln!("restored identity-hash: {}", hex::encode(id_hash));
    Ok(())
}
```

- [ ] **Step 7: Add the `restore_recovery_file_cli` function**

Append to `src-tauri/src/recovery_cli.rs`:

```rust
/// Restore the master seed from a passphrase-encrypted recovery file.
///
/// Reads the encrypted file from `in_path`. Decrypts using the recovery
/// passphrase (`HARMONY_RECOVERY_PASSPHRASE` / `_FILE`). Writes the seed
/// via the standard resolution chain (using the at-rest
/// `HARMONY_PASSPHRASE` / `_FILE` for re-encryption). Refuses if an
/// identity already exists unless `force` is true.
///
/// Stdout: nothing. Stderr: `restored identity-hash: <hex32>`.
pub fn restore_recovery_file_cli(
    plaintext_path: &Path,
    in_path: &Path,
    force: bool,
) -> Result<(), String> {
    let bytes = std::fs::read(in_path)
        .map_err(|e| format!("Error: failed to read {}: {e}", in_path.display()))?;
    let passphrase = resolve_recovery_passphrase()?;
    let restored = RecoveryArtifact::from_encrypted_file(&bytes, &passphrase)
        .map_err(|e| format!("Error: {e}"))?;
    let artifact = restored.into_artifact();
    let seed_bytes: Zeroizing<[u8; 32]> = Zeroizing::new(*artifact.as_bytes());
    let id_hash = artifact.master_pubkey_bundle().identity_hash();

    identity::write_seed_to_disk(plaintext_path, &seed_bytes, force)?;
    eprintln!("restored identity-hash: {}", hex::encode(id_hash));
    Ok(())
}
```

- [ ] **Step 8: Add the recovery-passphrase env-var resolution test**

Append to the `#[cfg(test)] mod tests` block in `recovery_cli.rs`:

```rust
#[test]
#[serial]
fn recovery_passphrase_env_var_resolution() {
    use secrecy::ExposeSecret;

    std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
    std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "from-env-direct");
    let pp = resolve_recovery_passphrase().expect("env-direct resolves");
    assert_eq!(pp.expose_secret(), "from-env-direct");
    std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recovery.passphrase");
    std::fs::write(&path, "from-env-file\n").unwrap();
    std::env::set_var("HARMONY_RECOVERY_PASSPHRASE_FILE", &path);
    let pp = resolve_recovery_passphrase().expect("env-file resolves");
    assert_eq!(pp.expose_secret(), "from-env-file");
    std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE_FILE");
}
```

- [ ] **Step 9: Add the export-recovery-file unit test (round-trip via memory)**

Append:

```rust
#[test]
#[serial]
fn export_recovery_file_with_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");
    let recovery_out = dir.path().join("recovery.bin");

    // Plant a known seed via the at-rest passphrase env var. The
    // write_seed_to_disk_with_keychain helper resolves the encrypted-file
    // backend from HARMONY_PASSPHRASE internally.
    std::env::set_var("HARMONY_PASSPHRASE", "at-rest-pass");
    std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery-pass");
    identity::write_seed_to_disk_with_keychain(
        &plaintext_path,
        &[0xCAu8; 32],
        /*force=*/ true,
        None,
    )
    .unwrap();

    export_recovery_file_cli(&plaintext_path, &recovery_out, Some("test")).expect("export");
    assert!(recovery_out.exists(), "recovery file must be written");

    // Decode the file back; it should round-trip to the same seed.
    let bytes = std::fs::read(&recovery_out).unwrap();
    use secrecy::SecretString;
    let restored = RecoveryArtifact::from_encrypted_file(
        &bytes,
        &SecretString::from("recovery-pass".to_string()),
    )
    .unwrap()
    .into_artifact();
    assert_eq!(restored.as_bytes(), &[0xCAu8; 32]);

    std::env::remove_var("HARMONY_PASSPHRASE");
    std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
}
```

- [ ] **Step 10: Add the restore-mnemonic and restore-refusal tests**

Append:

```rust
#[test]
#[serial]
fn restore_mnemonic_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");
    let mnemonic_path = dir.path().join("mnemonic.txt");

    std::env::set_var("HARMONY_PASSPHRASE", "restore-test");
    let original = RecoveryArtifact::from_seed([0xEFu8; 32]);
    std::fs::write(&mnemonic_path, original.to_mnemonic().as_str()).unwrap();
    let original_id = original.master_pubkey_bundle().identity_hash();

    restore_mnemonic_cli(&plaintext_path, &mnemonic_path, /*force=*/ false).expect("restore");

    let reloaded_seed = identity::read_seed_from_disk(&plaintext_path).unwrap();
    let reloaded = RecoveryArtifact::from_seed(*reloaded_seed);
    assert_eq!(reloaded.master_pubkey_bundle().identity_hash(), original_id);

    std::env::remove_var("HARMONY_PASSPHRASE");
}

#[test]
#[serial]
fn restore_refuses_when_identity_exists_without_force() {
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");
    let mnemonic_path = dir.path().join("mnemonic.txt");

    std::env::set_var("HARMONY_PASSPHRASE", "refuse-test");
    let original = RecoveryArtifact::from_seed([0x12u8; 32]);
    std::fs::write(&mnemonic_path, original.to_mnemonic().as_str()).unwrap();
    // Plant an existing identity.
    identity::write_seed_to_disk_with_keychain(
        &plaintext_path,
        &[0x99u8; 32],
        /*force=*/ true,
        None,
    )
    .unwrap();

    let err = restore_mnemonic_cli(&plaintext_path, &mnemonic_path, /*force=*/ false)
        .expect_err("must refuse");
    assert!(err.contains("identity already exists"), "actual: {err}");

    std::env::remove_var("HARMONY_PASSPHRASE");
}

#[test]
#[serial]
fn restore_with_force_overwrites_existing() {
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");
    let mnemonic_path = dir.path().join("mnemonic.txt");

    std::env::set_var("HARMONY_PASSPHRASE", "force-test");
    let original = RecoveryArtifact::from_seed([0xDDu8; 32]);
    std::fs::write(&mnemonic_path, original.to_mnemonic().as_str()).unwrap();
    let original_id = original.master_pubkey_bundle().identity_hash();
    // Plant a different existing identity.
    identity::write_seed_to_disk_with_keychain(
        &plaintext_path,
        &[0x77u8; 32],
        /*force=*/ true,
        None,
    )
    .unwrap();

    restore_mnemonic_cli(&plaintext_path, &mnemonic_path, /*force=*/ true).expect("force succeeds");
    let reloaded_seed = identity::read_seed_from_disk(&plaintext_path).unwrap();
    let reloaded = RecoveryArtifact::from_seed(*reloaded_seed);
    assert_eq!(reloaded.master_pubkey_bundle().identity_hash(), original_id);

    std::env::remove_var("HARMONY_PASSPHRASE");
}
```

- [ ] **Step 11: Run the full recovery_cli test module**

Run: `cd src-tauri && cargo test -p harmony-app --lib recovery_cli::`
Expected: all six tests pass (`recovery_passphrase_neither_set_fails_with_pointer_to_docs`, `recovery_passphrase_env_var_resolution`, `export_recovery_file_with_metadata`, `restore_mnemonic_idempotent`, `restore_refuses_when_identity_exists_without_force`, `restore_with_force_overwrites_existing`).

- [ ] **Step 12: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/recovery_cli.rs
git commit -m "feat(recovery-cli): library entry points for export/restore (ZEB-176)"
```

---

## Task 6: Wire CLI into `main.rs`

**Files:**
- Modify: `src-tauri/src/main.rs` — extend the clap subcommand enum and dispatch table

- [ ] **Step 1: Extend the `Command` enum**

In `src-tauri/src/main.rs`, replace the `enum Command` block (lines 13-27):

```rust
#[derive(Subcommand, Debug)]
enum Command {
    /// Re-encrypt ~/.harmony/identity.enc with a new passphrase.
    ///
    /// The OLD passphrase is read from HARMONY_PASSPHRASE or
    /// HARMONY_PASSPHRASE_FILE (the same env vars used at startup). The NEW
    /// passphrase is read from --new-passphrase-file. Refuses to rotate if the
    /// identity is currently in the OS keychain; in that case the OS handles
    /// re-encryption when you change your login password.
    RotatePassphrase {
        /// Path to a file containing the new passphrase.
        #[arg(long, value_name = "PATH")]
        new_passphrase_file: PathBuf,
    },
}
```

with:

```rust
#[derive(Subcommand, Debug)]
enum Command {
    /// Re-encrypt ~/.harmony/identity.enc with a new passphrase.
    ///
    /// The OLD passphrase is read from HARMONY_PASSPHRASE or
    /// HARMONY_PASSPHRASE_FILE (the same env vars used at startup). The NEW
    /// passphrase is read from --new-passphrase-file. Refuses to rotate if the
    /// identity is currently in the OS keychain; in that case the OS handles
    /// re-encryption when you change your login password.
    RotatePassphrase {
        /// Path to a file containing the new passphrase.
        #[arg(long, value_name = "PATH")]
        new_passphrase_file: PathBuf,
    },

    /// Export the identity for backup.
    Export {
        #[command(subcommand)]
        format: ExportFormat,
    },

    /// Restore an identity from a backup.
    Restore {
        #[command(subcommand)]
        format: RestoreFormat,

        /// Overwrite an existing identity (destructive).
        #[arg(long, global = true)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ExportFormat {
    /// Print 24-word BIP39 mnemonic (bare to stdout, warning + identity-hash to stderr).
    Mnemonic,
    /// Write a passphrase-encrypted recovery file. Requires
    /// HARMONY_RECOVERY_PASSPHRASE / HARMONY_RECOVERY_PASSPHRASE_FILE.
    RecoveryFile {
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
        #[arg(long, value_name = "STRING")]
        comment: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum RestoreFormat {
    /// Read a 24-word mnemonic from a file (whitespace-tolerant, case-insensitive).
    Mnemonic {
        #[arg(long, value_name = "PATH")]
        mnemonic_file: PathBuf,
    },
    /// Read a passphrase-encrypted recovery file. Requires
    /// HARMONY_RECOVERY_PASSPHRASE / HARMONY_RECOVERY_PASSPHRASE_FILE.
    RecoveryFile {
        #[arg(long, name = "in", value_name = "PATH")]
        in_path: PathBuf,
    },
}
```

The `force` flag uses `global = true` so it applies to either subcommand under `restore`.

- [ ] **Step 2: Extend the dispatch table**

In `main()`, replace the existing dispatch arm:

```rust
Some(Command::RotatePassphrase { new_passphrase_file }) => { ... }
None => { harmony_app::run(); }
```

with:

```rust
Some(Command::RotatePassphrase { new_passphrase_file }) => {
    init_tracing();
    match harmony_app::rotate_passphrase_cli(&new_passphrase_file) {
        Ok(()) => {
            println!("Passphrase rotated. Update your systemd unit / Docker secret to point at the new file.");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("Rotation failed: {e}");
            std::process::exit(1);
        }
    }
}
Some(Command::Export { format }) => {
    init_tracing();
    let plaintext_path = match harmony_app::identity::resolve_path(None) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };
    let result = match format {
        ExportFormat::Mnemonic => {
            harmony_app::recovery_cli::export_mnemonic_cli(&plaintext_path)
        }
        ExportFormat::RecoveryFile { out, comment } => {
            harmony_app::recovery_cli::export_recovery_file_cli(
                &plaintext_path,
                &out,
                comment.as_deref(),
            )
        }
    };
    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
Some(Command::Restore { format, force }) => {
    init_tracing();
    let plaintext_path = match harmony_app::identity::resolve_path(None) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };
    let result = match format {
        RestoreFormat::Mnemonic { mnemonic_file } => {
            harmony_app::recovery_cli::restore_mnemonic_cli(
                &plaintext_path,
                &mnemonic_file,
                force,
            )
        }
        RestoreFormat::RecoveryFile { in_path } => {
            harmony_app::recovery_cli::restore_recovery_file_cli(
                &plaintext_path,
                &in_path,
                force,
            )
        }
    };
    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
None => {
    // Default path — launch the Tauri runtime.
    harmony_app::run();
}
```

- [ ] **Step 3: Extract the `init_tracing` helper**

Just below `main`, add:

```rust
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}
```

The existing `RotatePassphrase` arm has the same five-line tracing setup inlined; the dispatch refactor moves it into one helper to avoid triplicating the call.

- [ ] **Step 4: Make `identity::resolve_path` and the `recovery_cli` module reachable from `main.rs`**

`identity::resolve_path` is already `pub`. `recovery_cli` was added as `pub mod recovery_cli;` in Task 5. No additional changes needed.

- [ ] **Step 5: Verify the binary compiles and the CLI surface looks right**

Run: `cd src-tauri && cargo build --bin harmony-app`
Expected: clean build.

Run: `cd src-tauri && cargo run --bin harmony-app -- help`
Expected: clap usage block listing `rotate-passphrase`, `export`, `restore`. Subcommand help (`harmony-app help export`) shows `mnemonic` and `recovery-file` choices.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/main.rs
git commit -m "feat(cli): wire export/restore subcommands into harmony-app (ZEB-176)"
```

---

## Task 7: Integration tests

**Files:**
- Create: `src-tauri/tests/recovery_cli_integration.rs`

These tests exercise the full pipeline end-to-end against a tempdir-rooted identity store. They differ from the Task 5 unit tests in that they generate an identity, export it, wipe the store, restore from the export, and verify the master `identity_hash` round-trips.

- [ ] **Step 1: Create the integration test file**

Create `src-tauri/tests/recovery_cli_integration.rs`:

```rust
//! End-to-end recovery CLI tests.
//!
//! Each test:
//!   1. Plants a known seed in a tempdir-rooted identity store.
//!   2. Exports it via mnemonic or recovery file.
//!   3. Wipes the identity store.
//!   4. Restores from the export.
//!   5. Verifies the restored seed yields the same master `identity_hash`.

use harmony_app::{identity, recovery_cli};
use harmony_owner::recovery::RecoveryArtifact;
use serial_test::serial;

fn plant_seed(plaintext_path: &std::path::Path, seed: &[u8; 32]) {
    identity::write_seed_to_disk_with_keychain(
        plaintext_path,
        seed,
        /*force=*/ true,
        None,
    )
    .expect("plant");
}

fn wipe_identity_store(plaintext_path: &std::path::Path) {
    let enc_path = plaintext_path.with_file_name("identity.enc");
    let _ = std::fs::remove_file(&enc_path);
}

#[test]
#[serial]
fn mnemonic_round_trip_preserves_identity_hash() {
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");
    let mnemonic_path = dir.path().join("mnemonic.txt");

    std::env::set_var("HARMONY_PASSPHRASE", "mnemonic-rt");

    let original_seed = [0xA1u8; 32];
    plant_seed(&plaintext_path, &original_seed);
    let original_id = RecoveryArtifact::from_seed(original_seed)
        .master_pubkey_bundle()
        .identity_hash();

    // Export mnemonic. The unit CLI writes mnemonic to stdout; we replicate
    // that using the library API directly to capture it for restore.
    let seed = identity::read_seed_from_disk(&plaintext_path).unwrap();
    let mnemonic = RecoveryArtifact::from_seed(*seed).to_mnemonic();
    std::fs::write(&mnemonic_path, mnemonic.as_str()).unwrap();

    // Wipe and restore.
    wipe_identity_store(&plaintext_path);
    recovery_cli::restore_mnemonic_cli(&plaintext_path, &mnemonic_path, false)
        .expect("restore");

    let reloaded = identity::read_seed_from_disk(&plaintext_path).unwrap();
    let reloaded_id = RecoveryArtifact::from_seed(*reloaded)
        .master_pubkey_bundle()
        .identity_hash();
    assert_eq!(reloaded_id, original_id);

    std::env::remove_var("HARMONY_PASSPHRASE");
}

#[test]
#[serial]
fn recovery_file_round_trip_preserves_identity_hash() {
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");
    let recovery_path = dir.path().join("recovery.bin");

    std::env::set_var("HARMONY_PASSPHRASE", "recovery-rt");
    std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "rt-pass");

    let original_seed = [0xB2u8; 32];
    plant_seed(&plaintext_path, &original_seed);
    let original_id = RecoveryArtifact::from_seed(original_seed)
        .master_pubkey_bundle()
        .identity_hash();

    recovery_cli::export_recovery_file_cli(&plaintext_path, &recovery_path, Some("rt-test"))
        .expect("export");

    wipe_identity_store(&plaintext_path);

    recovery_cli::restore_recovery_file_cli(&plaintext_path, &recovery_path, false)
        .expect("restore");

    let reloaded = identity::read_seed_from_disk(&plaintext_path).unwrap();
    let reloaded_id = RecoveryArtifact::from_seed(*reloaded)
        .master_pubkey_bundle()
        .identity_hash();
    assert_eq!(reloaded_id, original_id);

    std::env::remove_var("HARMONY_PASSPHRASE");
    std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
}

#[test]
#[serial]
fn cross_encoding_equivalence_via_cli() {
    let dir = tempfile::tempdir().unwrap();
    let plaintext_path = dir.path().join("identity.key");
    let mnemonic_path = dir.path().join("mnemonic.txt");
    let recovery_path = dir.path().join("recovery.bin");

    std::env::set_var("HARMONY_PASSPHRASE", "cross-rt");
    std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "rt-cross");

    let original_seed = [0xC3u8; 32];
    plant_seed(&plaintext_path, &original_seed);
    let original_id = RecoveryArtifact::from_seed(original_seed)
        .master_pubkey_bundle()
        .identity_hash();

    // Export both ways.
    let seed = identity::read_seed_from_disk(&plaintext_path).unwrap();
    let mnemonic = RecoveryArtifact::from_seed(*seed).to_mnemonic();
    std::fs::write(&mnemonic_path, mnemonic.as_str()).unwrap();
    recovery_cli::export_recovery_file_cli(&plaintext_path, &recovery_path, None)
        .expect("export-recovery");

    // Wipe + restore from mnemonic.
    wipe_identity_store(&plaintext_path);
    recovery_cli::restore_mnemonic_cli(&plaintext_path, &mnemonic_path, false)
        .expect("restore-mnemonic");
    let id_via_m = RecoveryArtifact::from_seed(*identity::read_seed_from_disk(&plaintext_path).unwrap())
        .master_pubkey_bundle()
        .identity_hash();
    assert_eq!(id_via_m, original_id, "mnemonic restore preserves identity_hash");

    // Wipe + restore from recovery file.
    wipe_identity_store(&plaintext_path);
    recovery_cli::restore_recovery_file_cli(&plaintext_path, &recovery_path, false)
        .expect("restore-recovery");
    let id_via_f = RecoveryArtifact::from_seed(*identity::read_seed_from_disk(&plaintext_path).unwrap())
        .master_pubkey_bundle()
        .identity_hash();
    assert_eq!(id_via_f, original_id, "recovery-file restore preserves identity_hash");

    std::env::remove_var("HARMONY_PASSPHRASE");
    std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
}
```

- [ ] **Step 2: Run the integration tests**

Run: `cd src-tauri && cargo test -p harmony-app --test recovery_cli_integration`
Expected: all three tests pass.

- [ ] **Step 3: Run the full test suite to confirm no regressions**

Run: `cd src-tauri && cargo test -p harmony-app`
Expected: every test in identity, recovery_cli, and recovery_cli_integration passes; the existing rotate-passphrase suite is untouched.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/tests/recovery_cli_integration.rs
git commit -m "test(recovery-cli): end-to-end round-trip integration tests (ZEB-176)"
```

---

## Task 8: Documentation update — `docs/headless-install.md`

**Files:**
- Modify: `docs/headless-install.md` (Backup and recovery section)

- [ ] **Step 1: Replace the Backup and recovery section**

Open `docs/headless-install.md` and locate the section starting `## Backup and recovery` (around line 107). Replace its body (down to but not including the next `##` heading) with:

````markdown
## Backup and recovery

`harmony-app` ships two complementary backup formats. Both encode the same
master 32-byte seed; restoring from either produces a byte-identical identity.

### Mnemonic backup (recommended primary)

24 BIP39 English words. Defends against complete data loss — write the
words on paper.

```bash
# Export. The 24 words go to stdout; a warning preamble + identity-hash to stderr.
harmony-app export mnemonic > backup.txt
# Restore (refuses if an identity already exists; pass --force to overwrite).
harmony-app restore mnemonic --mnemonic-file backup.txt
```

The mnemonic file is whitespace-tolerant, case-insensitive, and
ASCII-only. Single line, multi-line, indented, mixed case — all valid.

### Encrypted recovery file (secondary, copy-to-USB friendly)

Argon2id + XChaCha20-Poly1305 envelope of the same seed. Defends against
file leak — without the recovery passphrase the file is useless.

```bash
# Export — requires HARMONY_RECOVERY_PASSPHRASE or HARMONY_RECOVERY_PASSPHRASE_FILE.
HARMONY_RECOVERY_PASSPHRASE_FILE=/run/secrets/harmony-recovery \
    harmony-app export recovery-file --out /mnt/usb/identity.harmony --comment "hostname=$(hostname) date=$(date -I)"

# Restore — same env vars; --force required to overwrite an existing identity.
HARMONY_RECOVERY_PASSPHRASE_FILE=/run/secrets/harmony-recovery \
    harmony-app restore recovery-file --in /mnt/usb/identity.harmony
```

### Recovery passphrase env vars

| Var | Format |
|---|---|
| `HARMONY_RECOVERY_PASSPHRASE` | Direct UTF-8 string |
| `HARMONY_RECOVERY_PASSPHRASE_FILE` | Path to a file containing the passphrase (UTF-8, one trailing newline allowed) |

These are **distinct** from the at-rest `HARMONY_PASSPHRASE` /
`HARMONY_PASSPHRASE_FILE`. Neither falls back to the other — restoring a
recovery file with the wrong env var set fails with a docs pointer.

### Identity hash

Every export and restore prints `identity-hash: <hex32>` to stderr. This
is the operator's eyeball-comparison fingerprint: if the hash on the
backup matches the hash you see when restoring on a new machine, the
seeds are byte-identical and the round-trip worked.

### `--force`

Both restore subcommands check whether `~/.harmony/identity.enc` already
exists. Without `--force`, they refuse and exit 1. With `--force`, the
existing file is overwritten in place via the same atomic
tmp-then-rename pattern used elsewhere. **This is destructive** —
verify the identity-hash before passing `--force` on a machine that has
a real identity you want to keep.

### Why two formats?

The mnemonic is the catastrophic-loss recovery path: words on paper
survive everything except the paper itself. The encrypted recovery file
is the routine-portability path: you can copy it across machines, mail
it to yourself, store it on a USB stick — and without the passphrase
it's useless. Together, both lost simultaneously is required to lose
the identity.

The existing "treat `~/.harmony/identity.enc` and your passphrase as
two halves of a recovery key" guidance still applies for at-rest
storage — the recovery commands give you an additional layer.
````

- [ ] **Step 2: Update the Troubleshooting table**

Locate the troubleshooting table at line ~120. The `plaintext identity at <path> needs a destination ...` row no longer applies (legacy plaintext migration was removed). Delete that row.

Add new rows for the recovery commands' common errors:

| Error | Meaning | Fix |
|---|---|---|
| `Error: expected 24 BIP39 words, got <N>` | Mnemonic file has wrong word count | Re-check the file; trim partial pastes |
| `Error: unknown word at position <N>: "<word>"` | Mnemonic typo | Re-check the indicated word against the BIP39 wordlist |
| `Error: mnemonic checksum mismatch — likely a typo somewhere in the 24 words` | One or more typos | Visually re-verify each word against the source |
| `Error: wrong passphrase or corrupted recovery file (AEAD tag rejected)` | Bad recovery passphrase OR the file was tampered with | Verify the recovery passphrase matches what was used to export |
| `identity already exists at <path>; pass --force to overwrite (this is destructive)` | Restore policy | If you really want to overwrite, re-run with `--force`; otherwise, this is the safety net |
| `neither HARMONY_RECOVERY_PASSPHRASE nor HARMONY_RECOVERY_PASSPHRASE_FILE is set` | Recovery passphrase missing | Set one; remember it's distinct from the at-rest passphrase |

- [ ] **Step 3: Verify the docs render correctly**

Run: `grep -n '^##' docs/headless-install.md` to confirm the section structure is intact and no headings collide.

- [ ] **Step 4: Commit**

```bash
git add docs/headless-install.md
git commit -m "docs(headless-install): backup/restore CLI command reference (ZEB-176)"
```

---

## Definition of Done

After all 8 tasks land, the following must hold:

1. [ ] `cargo test -p harmony-app` passes (lib + integration tests).
2. [ ] `cargo clippy -p harmony-app -- -D warnings` is clean.
3. [ ] `cargo build --bin harmony-app` succeeds.
4. [ ] `harmony-app help` lists `rotate-passphrase`, `export`, `restore`.
5. [ ] `harmony-app help export` shows `mnemonic` and `recovery-file` choices.
6. [ ] Round-trip via mnemonic preserves the master `identity_hash`.
7. [ ] Round-trip via recovery file preserves the master `identity_hash`.
8. [ ] Cross-encoding equivalence: mnemonic and recovery file restore to the same `identity_hash`.
9. [ ] `--force` is required to overwrite an existing identity; refusal error mentions both the path and the flag.
10. [ ] `docs/headless-install.md` Backup and recovery section reflects the new CLI surface.
11. [ ] No legacy plaintext migration code remains (search: `grep -rn 'LegacyPlaintextReader\|cleanup_legacy_bak\|identity.key.bak' src-tauri/src/` returns no hits).
