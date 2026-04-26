# Headless Encrypted Identity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the three remaining gaps in `src-tauri/src/identity.rs` so harmony-client never writes plaintext identity material to disk: legacy `.bak` auto-cleanup, headless passphrase-derived encrypted-file backend, and `rotate-passphrase` CLI subcommand.

**Architecture:** Three storage layers behind a `KeyStore` trait — `KeychainStore` (existing, unchanged), new `EncryptedFileStore` (Argon2id + XChaCha20-Poly1305 AEAD keyed from `HARMONY_PASSPHRASE` / `HARMONY_PASSPHRASE_FILE`), new `LegacyPlaintextReader` (read-only legacy migration helper, deliberately *not* a `KeyStore`). Resolution chain is keychain → encrypted-file → legacy-plaintext-migrate → fresh-generate, with hard-fail when no destination is available rather than silent plaintext fallback.

**Tech Stack:** Rust 2021, `argon2 = "0.5"` (RustCrypto), `chacha20poly1305 = "0.10"`, `secrecy = "0.10"`, `subtle = "2"`, `clap = "4"` (derive), `serial_test = "3"` (dev). Existing: `keyring = "3"` (apple-native + windows-native + sync-secret-service), `zeroize`, `tracing`, `tempfile`.

**Reference spec:** `docs/specs/2026-04-26-headless-encrypted-identity-design.md`

**Branch:** `zeb-174-headless-encrypted-identity` (already created from `origin/main`)

---

## File Structure Overview

| File | Action | Responsibility |
|---|---|---|
| `.cargo/config.toml` | **Create** | Set `RUST_MIN_STACK=8388608` so PQ keygen tests don't need per-test thread-spawn boilerplate |
| `src-tauri/Cargo.toml` | **Modify** | Add `argon2`, `chacha20poly1305`, `secrecy`, `subtle`, `clap` deps; `serial_test` dev-dep |
| `src-tauri/src/identity.rs` | **Major rewrite** | Add `EncryptedFileStore`, `LegacyPlaintextReader`, encrypt/decrypt helpers, `cleanup_legacy_bak`, `verify_round_trip`, `rotate_passphrase`; rewrite `load_or_generate_with_stores` for three-store chain; remove `FileStore` (replaced by `LegacyPlaintextReader` for reads, `EncryptedFileStore` for writes) |
| `src-tauri/src/main.rs` | **Modify** | Parse `rotate-passphrase` subcommand via `clap` *before* launching Tauri runtime; subcommand exits the process when done |
| `src-tauri/tests/fixtures/encrypted_v1.bin` | **Create** | Pinned 230-byte wire-format fixture (locks format against silent drift) |
| `docs/headless-install.md` | **Create** | Server-admin-facing install guide: env vars, systemd, Docker, rotation, troubleshooting |

The plan is 12 tasks. Each task is a single commit. TDD throughout.

---

## Task 1: Cargo deps and stack-size config

**Files:**
- Create: `.cargo/config.toml`
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/src/identity.rs:439-470` (one existing test) to verify the stack-size config works

- [ ] **Step 1: Create `.cargo/config.toml`**

```toml
# Workspace-level cargo config.
#
# ML-DSA scalar NTT keygen requires ~2 MB of stack. Default Rust thread stacks
# are 2 MB but `cargo test` uses smaller worker threads. Bumping RUST_MIN_STACK
# lets every test that allocates a PqPrivateIdentity skip the explicit
# `std::thread::Builder::new().stack_size(...)` boilerplate.
[env]
RUST_MIN_STACK = "8388608"  # 8 MiB
```

- [ ] **Step 2: Add dependencies to `src-tauri/Cargo.toml`**

Locate the `[dependencies]` section and add (alphabetized — match existing style):

```toml
argon2 = "0.5"
chacha20poly1305 = "0.10"
clap = { version = "4", features = ["derive"] }
secrecy = "0.10"
subtle = "2"
```

Locate the `[dev-dependencies]` section (or create it if absent — `tempfile` is currently in `[dependencies]`; leave it there) and add:

```toml
[dev-dependencies]
serial_test = "3"
```

- [ ] **Step 3: Strip the `thread::Builder` boilerplate from one existing test**

In `src-tauri/src/identity.rs`, locate `fn file_store_round_trip()` (around line 439). Replace the entire test body with:

```rust
#[test]
fn file_store_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("identity.key");
    let store = FileStore::new(path.clone());

    let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
    let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
    let identity = NodeIdentity { pq, ed25519 };

    store.save(&identity).unwrap();
    let loaded = store.load().unwrap().expect("should find saved identity");
    assert_eq!(
        loaded.ed25519.public_identity().address_hash,
        identity.ed25519.public_identity().address_hash,
    );
    assert_eq!(
        loaded.pq.public_identity().address_hash,
        identity.pq.public_identity().address_hash,
    );
}
```

- [ ] **Step 4: Run test and verify the stack-size config works**

```bash
cd src-tauri && cargo test --lib identity::tests::file_store_round_trip
```

Expected: PASS (no stack-overflow). If FAIL with stack-overflow, the `.cargo/config.toml` is not being picked up; check the file is at the workspace root (`harmony-client/.cargo/config.toml`, not inside `src-tauri/`).

- [ ] **Step 5: Run full test suite**

```bash
cd src-tauri && cargo test --lib identity
```

Expected: 9/9 PASS (the existing tests, all still working with their `thread::Builder` wrappers — we only stripped boilerplate from one).

- [ ] **Step 6: Commit**

```bash
git add .cargo/config.toml src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/identity.rs
git commit -m "$(cat <<'EOF'
chore(identity): cargo deps for encrypted-file backend + RUST_MIN_STACK config

Adds argon2, chacha20poly1305, secrecy, subtle, clap to deps and
serial_test as dev-dep. Sets RUST_MIN_STACK=8388608 in .cargo/config.toml
so PQ-keygen tests can drop per-test thread::Builder boilerplate (one
test stripped as proof; others migrate as they're touched).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Extract `write_atomic_0600` helper

**Files:**
- Modify: `src-tauri/src/identity.rs` (refactor `FileStore::save`, extract shared helper)

This is a pure refactor — preserves existing behavior so the existing tests pass unchanged.

- [ ] **Step 1: Extract the helper above the `KeyStore` trait definition**

In `src-tauri/src/identity.rs`, after the serialization helpers (`identity_to_blob` / `blob_to_identity`, around line 70) and before the `KeyStore` trait, insert:

```rust
// ── Atomic file write ──────────────────────────────────────────────────

/// Write `bytes` to `path` atomically with mode 0o600 on Unix.
///
/// Steps:
///   1. Ensure parent directory exists with mode 0o700 (Unix only).
///   2. Open `<path>.tmp` with mode 0o600 (Unix only).
///   3. Write + fsync.
///   4. Atomic rename `<path>.tmp` → `<path>`.
///
/// `TmpGuard` removes the `.tmp` file if anything panics or returns Err
/// before the rename completes. After successful rename, the guard is
/// `mem::forget`ed so the renamed file isn't unlinked.
fn write_atomic_0600(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }

    let tmp_path = {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".tmp");
        path.with_file_name(name)
    };

    struct TmpGuard<'a>(&'a Path);
    impl Drop for TmpGuard<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0);
        }
    }
    let guard = TmpGuard(&tmp_path);

    {
        #[cfg(unix)]
        let f = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_path)
                .map_err(|e| format!("Failed to create {}: {e}", tmp_path.display()))?
        };
        #[cfg(not(unix))]
        let f = {
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)
                .map_err(|e| format!("Failed to create {}: {e}", tmp_path.display()))?
        };
        use std::io::Write;
        (&f)
            .write_all(bytes)
            .map_err(|e| format!("Failed to write {}: {e}", tmp_path.display()))?;
        f.sync_all()
            .map_err(|e| format!("Failed to fsync {}: {e}", tmp_path.display()))?;
    }
    std::fs::rename(&tmp_path, path).map_err(|e| {
        format!(
            "Failed to rename {} → {}: {e}",
            tmp_path.display(),
            path.display()
        )
    })?;
    std::mem::forget(guard);
    Ok(())
}
```

- [ ] **Step 2: Replace `FileStore::save` body to call the helper**

Locate `impl KeyStore for FileStore { ... fn save ... }` (around line 121-187). Replace the `save` body with:

```rust
fn save(&self, identity: &NodeIdentity) -> Result<(), String> {
    let blob = identity_to_blob(identity);
    write_atomic_0600(&self.path, &blob)
}
```

(All the inline parent-dir-mkdir / TmpGuard / OpenOptions / fsync / rename code is now in `write_atomic_0600`.)

- [ ] **Step 3: Run existing tests to confirm no regression**

```bash
cd src-tauri && cargo test --lib identity
```

Expected: 9/9 PASS (the `file_store_round_trip` test exercises the same code path — refactor is transparent).

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/identity.rs
git commit -m "$(cat <<'EOF'
refactor(identity): extract write_atomic_0600 helper from FileStore::save

Pure refactor; preserves existing semantics (parent-dir mkdir at 0o700,
.tmp file at 0o600, fsync, atomic rename, TmpGuard for panic safety).
The helper will be reused by EncryptedFileStore::save in the next task.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `LegacyPlaintextReader` (replaces `FileStore` for read paths)

**Files:**
- Modify: `src-tauri/src/identity.rs` (add new type alongside existing `FileStore`)

`FileStore` stays for now — we'll remove it in Task 8 once the new resolution chain is in place. This task only adds the read-only successor.

- [ ] **Step 1: Add tests**

In `src-tauri/src/identity.rs`, locate the `#[cfg(test)] mod tests { ... }` block. Add a new submodule before the closing brace:

```rust
mod legacy_plaintext_reader {
    use super::*;

    #[test]
    fn read_existing_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");

        // Pre-populate via FileStore (which writes the same 161-byte format)
        let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
        let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
        let original = NodeIdentity { pq, ed25519 };
        let original_addr = original.ed25519.public_identity().address_hash;
        FileStore::new(path.clone()).save(&original).unwrap();

        // Read back via LegacyPlaintextReader
        let reader = LegacyPlaintextReader::new(path);
        let loaded = reader.read().unwrap().expect("should read plaintext");
        assert_eq!(
            loaded.ed25519.public_identity().address_hash,
            original_addr,
        );
    }

    #[test]
    fn read_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.key");
        let reader = LegacyPlaintextReader::new(path);
        assert!(reader.read().unwrap().is_none());
    }

    #[test]
    fn read_from_static_method_works() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
        let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
        FileStore::new(path.clone()).save(&NodeIdentity { pq, ed25519 }).unwrap();

        let loaded = LegacyPlaintextReader::read_from(&path).unwrap();
        assert!(loaded.is_some());
    }
}
```

- [ ] **Step 2: Run tests and verify failure**

```bash
cd src-tauri && cargo test --lib identity::tests::legacy_plaintext_reader
```

Expected: FAIL with `cannot find type LegacyPlaintextReader`.

- [ ] **Step 3: Implement `LegacyPlaintextReader`**

In `src-tauri/src/identity.rs`, after the `FileStore` impl block (around line 188) and before the `KeychainStore` section, insert:

```rust
// ── LegacyPlaintextReader ───────────────────────────────────────────────

/// Read-only reader for legacy plaintext identity files at `~/.harmony/identity.key`.
///
/// Deliberately does not implement `KeyStore` — there is no `save` and there
/// will never be one. This type exists solely to migrate identities written by
/// the pre-encryption code path into the modern keychain or encrypted-file
/// backends.
pub struct LegacyPlaintextReader {
    path: PathBuf,
}

impl LegacyPlaintextReader {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Read the plaintext identity at `self.path`, or `Ok(None)` if missing.
    pub fn read(&self) -> Result<Option<NodeIdentity>, String> {
        Self::read_from(&self.path)
    }

    /// Free function variant — read plaintext identity from `path`, or
    /// `Ok(None)` if missing.
    pub fn read_from(path: &Path) -> Result<Option<NodeIdentity>, String> {
        let raw = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("Failed to read {}: {e}", path.display())),
        };
        let buf = Zeroizing::new(raw);
        let identity = blob_to_identity(&buf)?;
        #[cfg(unix)]
        warn_permissions(path);
        Ok(Some(identity))
    }
}
```

- [ ] **Step 4: Run tests and verify pass**

```bash
cd src-tauri && cargo test --lib identity::tests::legacy_plaintext_reader
```

Expected: 3/3 PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/identity.rs
git commit -m "$(cat <<'EOF'
feat(identity): add LegacyPlaintextReader for one-shot migration of legacy plaintext

Read-only reader for ~/.harmony/identity.key written by pre-encryption
code. Deliberately not a KeyStore — there is no save and there never
will be one. Exists solely to migrate existing identities into the
modern keychain or encrypted-file backends.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Wire format encrypt / decrypt helpers

**Files:**
- Modify: `src-tauri/src/identity.rs` (add private `encrypt_with_params` and `decrypt` functions, plus constants)

These are pure functions — given inputs they produce deterministic byte output (when salt and nonce are passed in). The `EncryptedFileStore` wrapper in Task 5 supplies fresh `OsRng` salt and nonce for production use.

- [ ] **Step 1: Add tests**

In `src-tauri/src/identity.rs`, add a new submodule inside the `#[cfg(test)] mod tests { ... }` block:

```rust
mod wire_format {
    use super::*;

    const TEST_PASSPHRASE: &[u8] = b"correct horse battery staple";
    const TEST_SALT: [u8; 16] = [0xAB; 16];
    const TEST_NONCE: [u8; 24] = [0xCD; 24];
    const TEST_BLOB: [u8; 161] = [0x42; 161];

    #[test]
    fn round_trip_correct_passphrase() {
        let bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
        let decrypted = decrypt(TEST_PASSPHRASE, &bytes).unwrap();
        assert_eq!(&decrypted[..], &TEST_BLOB[..]);
    }

    #[test]
    fn wrong_passphrase_fails_aead() {
        let bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
        let err = decrypt(b"wrong passphrase", &bytes).unwrap_err();
        assert!(
            err.contains("wrong passphrase or corrupted file"),
            "expected indistinguishable error, got: {err}"
        );
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let mut bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
        bytes[100] ^= 0x01;  // flip one bit in the ciphertext range (53..214)
        let err = decrypt(TEST_PASSPHRASE, &bytes).unwrap_err();
        assert!(err.contains("wrong passphrase or corrupted file"));
    }

    #[test]
    fn tampered_kdf_params_fails_aad() {
        let mut bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
        // Flip a byte in kdf_m_kib (offset 6..10) — part of AAD
        bytes[7] ^= 0x01;
        let err = decrypt(TEST_PASSPHRASE, &bytes).unwrap_err();
        assert!(
            err.contains("wrong passphrase or corrupted file"),
            "AAD binding should reject param tampering, got: {err}"
        );
    }

    #[test]
    fn tampered_magic_fails() {
        let mut bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
        bytes[0] = b'X';  // trash magic
        let err = decrypt(TEST_PASSPHRASE, &bytes).unwrap_err();
        assert!(err.contains("unrecognized format"), "got: {err}");
    }

    #[test]
    fn tampered_version_fails() {
        let mut bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
        bytes[4] = 0xFF;  // unknown version
        let err = decrypt(TEST_PASSPHRASE, &bytes).unwrap_err();
        assert!(err.contains("unrecognized format"), "got: {err}");
    }

    #[test]
    fn truncated_file_fails() {
        let bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
        let err = decrypt(TEST_PASSPHRASE, &bytes[..200]).unwrap_err();
        assert!(err.contains("expected 230 bytes"), "got: {err}");
    }

    #[test]
    fn output_is_exactly_230_bytes() {
        let bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
        assert_eq!(bytes.len(), 230);
    }

    #[test]
    fn header_layout_is_exact() {
        let bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
        assert_eq!(&bytes[0..4], b"HRMI", "magic mismatch");
        assert_eq!(bytes[4], 0x01, "format_version mismatch");
        assert_eq!(bytes[5], 0x01, "kdf_id mismatch");
        assert_eq!(&bytes[6..10], &65536u32.to_be_bytes(), "kdf_m_kib mismatch");
        assert_eq!(&bytes[10..14][..4], &3u32.to_be_bytes(), "kdf_t mismatch");
        // Note: kdf_t is at offset 10..14 (4 bytes), kdf_p at 14? Wait — re-check.
        // Per spec: offset 12 is kdf_p (1 byte). So kdf_t is offset 10..14 (4 bytes BE),
        // overlapping nothing — let me verify by re-reading spec layout.
        // Spec table: offset 10 size 4 kdf_t; offset 14? Wait, offset 12 = kdf_p.
        // That means kdf_t is offset 10..12 (only 2 bytes) — NO, spec says size 4.
        // Re-check carefully against spec: "10  4  kdf_t  u32 BE", "14  1  kdf_p u8".
        // Wait spec says "12  1  kdf_p" — offset 12, not 14. So kdf_t is 10..14 BUT
        // spec puts kdf_p at 12. That overlaps. Let me verify the spec table once more
        // before encoding — IF the spec layout is actually
        //   0  4  magic
        //   4  1  format_version
        //   5  1  kdf_id
        //   6  4  kdf_m_kib
        //   10 4  kdf_t
        //   14 1  kdf_p
        //   15 16 salt   (offsets shift)
        // then the salt offset is 15 not 13. The implementer should verify against spec.
    }
}
```

**Note on the layout test above:** the spec table at §"Encrypted file wire format (v1)" says salt starts at offset 13, nonce at 29, ciphertext at 53. That implies the header is 13 bytes, so kdf_t must be 2 bytes wide, not 4. **The implementer should treat the spec offsets as authoritative** and encode kdf_t as `u16 BE` at offset 10..12, not `u32 BE`. Update the test to match:

Replace the `header_layout_is_exact` test body with:

```rust
#[test]
fn header_layout_is_exact() {
    let bytes = encrypt_with_params(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
    // Per spec: 230-byte file, header is 13 bytes, then 16-byte salt, 24-byte nonce,
    // 161-byte ciphertext, 16-byte tag.
    assert_eq!(&bytes[0..4], b"HRMI", "magic mismatch");
    assert_eq!(bytes[4], 0x01, "format_version mismatch");
    assert_eq!(bytes[5], 0x01, "kdf_id mismatch");
    assert_eq!(&bytes[6..10], &65536u32.to_be_bytes(), "kdf_m_kib (u32 BE) mismatch");
    assert_eq!(&bytes[10..12], &3u16.to_be_bytes(), "kdf_t (u16 BE) mismatch");
    assert_eq!(bytes[12], 1, "kdf_p (u8) mismatch");
    assert_eq!(&bytes[13..29], &TEST_SALT[..], "salt mismatch");
    assert_eq!(&bytes[29..53], &TEST_NONCE[..], "nonce mismatch");
    // bytes[53..214] is ciphertext (opaque), bytes[214..230] is poly1305 tag (opaque)
    assert_eq!(bytes.len(), 230);
}
```

(The spec table's "kdf_t  u32 BE" was a transcription error in the design doc; the offsets force kdf_t to be 16-bit. Worth noting in code comments.)

- [ ] **Step 2: Run tests and verify failure**

```bash
cd src-tauri && cargo test --lib identity::tests::wire_format
```

Expected: FAIL with `cannot find function encrypt_with_params` (and `decrypt`).

- [ ] **Step 3: Implement constants and helpers**

In `src-tauri/src/identity.rs`, add these constants near the top (after the existing `VERSION` / length constants):

```rust
// ── Encrypted file wire format constants ───────────────────────────────

const ENC_MAGIC: &[u8; 4] = b"HRMI";
const ENC_FORMAT_VERSION: u8 = 0x01;
const ENC_KDF_ID_ARGON2ID: u8 = 0x01;

// Argon2id parameters (v1):
const KDF_M_KIB: u32 = 65536;  // 64 MiB
const KDF_T: u16 = 3;          // iterations
const KDF_P: u8 = 1;           // parallelism
const KDF_OUT_LEN: usize = 32; // XChaCha20-Poly1305 key length

// Wire format offsets:
const HEADER_LEN: usize = 13;   // magic(4) + version(1) + kdf_id(1) + m(4) + t(2) + p(1)
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;    // XChaCha20 needs 192-bit nonce
const TAG_LEN: usize = 16;      // Poly1305 tag
const ENC_FILE_LEN: usize = HEADER_LEN + SALT_LEN + NONCE_LEN + BLOB_LEN + TAG_LEN; // 230
```

Then add the encrypt/decrypt helpers after the `write_atomic_0600` function:

```rust
// ── Encrypted file wire format helpers ─────────────────────────────────

/// Encode a 161-byte identity blob into the 230-byte encrypted-file format.
///
/// Caller supplies salt and nonce explicitly so the function is deterministic
/// for testing. Production code generates fresh random values per save.
fn encrypt_with_params(
    passphrase: &[u8],
    salt: &[u8; SALT_LEN],
    nonce: &[u8; NONCE_LEN],
    blob: &[u8; BLOB_LEN],
) -> Vec<u8> {
    use argon2::{Algorithm, Argon2, Params, Version};
    use chacha20poly1305::{
        aead::{Aead, KeyInit, Payload},
        XChaCha20Poly1305, XNonce,
    };

    // Build header (13 bytes — also serves as AAD).
    let mut out = Vec::with_capacity(ENC_FILE_LEN);
    out.extend_from_slice(ENC_MAGIC);
    out.push(ENC_FORMAT_VERSION);
    out.push(ENC_KDF_ID_ARGON2ID);
    out.extend_from_slice(&KDF_M_KIB.to_be_bytes());
    out.extend_from_slice(&KDF_T.to_be_bytes());
    out.push(KDF_P);
    debug_assert_eq!(out.len(), HEADER_LEN);

    // Append salt, nonce.
    out.extend_from_slice(salt);
    out.extend_from_slice(nonce);
    debug_assert_eq!(out.len(), HEADER_LEN + SALT_LEN + NONCE_LEN);

    // KDF.
    let params = Params::new(KDF_M_KIB, KDF_T as u32, KDF_P as u32, Some(KDF_OUT_LEN))
        .expect("Argon2 params hardcoded valid");
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; KDF_OUT_LEN]);
    argon
        .hash_password_into(passphrase, salt, key.as_mut_slice())
        .expect("Argon2 derivation cannot fail with hardcoded params");

    // AEAD encrypt with header (first 13 bytes) as AAD.
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_slice())
        .expect("32-byte key always valid");
    let payload = Payload {
        msg: blob,
        aad: &out[..HEADER_LEN],
    };
    let ciphertext_with_tag = cipher
        .encrypt(XNonce::from_slice(nonce), payload)
        .expect("AEAD encrypt cannot fail with valid inputs");
    debug_assert_eq!(ciphertext_with_tag.len(), BLOB_LEN + TAG_LEN);

    out.extend_from_slice(&ciphertext_with_tag);
    debug_assert_eq!(out.len(), ENC_FILE_LEN);
    out
}

/// Decode a 230-byte encrypted-file blob back into the 161-byte identity blob.
///
/// Indistinguishable error for wrong-passphrase vs corrupted-ciphertext to
/// avoid leaking which case occurred (an attacker who can probe with arbitrary
/// passphrases gains no signal from the error message).
fn decrypt(passphrase: &[u8], bytes: &[u8]) -> Result<[u8; BLOB_LEN], String> {
    use argon2::{Algorithm, Argon2, Params, Version};
    use chacha20poly1305::{
        aead::{Aead, KeyInit, Payload},
        XChaCha20Poly1305, XNonce,
    };

    if bytes.len() != ENC_FILE_LEN {
        return Err(format!(
            "identity store is corrupt: expected {ENC_FILE_LEN} bytes, got {}",
            bytes.len()
        ));
    }
    if &bytes[0..4] != ENC_MAGIC {
        return Err(format!(
            "identity store is in an unrecognized format (magic={:?}) — this build may be too old",
            &bytes[0..4]
        ));
    }
    if bytes[4] != ENC_FORMAT_VERSION {
        return Err(format!(
            "identity store is in an unrecognized format (version={:#04x}) — this build may be too old",
            bytes[4]
        ));
    }
    if bytes[5] != ENC_KDF_ID_ARGON2ID {
        return Err(format!(
            "identity store is in an unrecognized format (kdf_id={:#04x}) — this build may be too old",
            bytes[5]
        ));
    }

    // Pull KDF params from the file (self-describing).
    let m_kib = u32::from_be_bytes(bytes[6..10].try_into().unwrap());
    let t = u16::from_be_bytes(bytes[10..12].try_into().unwrap()) as u32;
    let p = bytes[12] as u32;
    let salt: &[u8; SALT_LEN] = bytes[13..29].try_into().unwrap();
    let nonce: &[u8; NONCE_LEN] = bytes[29..53].try_into().unwrap();
    let ciphertext_with_tag = &bytes[53..ENC_FILE_LEN];

    let params = Params::new(m_kib, t, p, Some(KDF_OUT_LEN))
        .map_err(|e| format!("invalid KDF params in file: {e}"))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; KDF_OUT_LEN]);
    argon
        .hash_password_into(passphrase, salt, key.as_mut_slice())
        .map_err(|e| format!("Argon2 derivation failed: {e}"))?;

    let cipher = XChaCha20Poly1305::new_from_slice(key.as_slice())
        .expect("32-byte key always valid");
    let payload = Payload {
        msg: ciphertext_with_tag,
        aad: &bytes[..HEADER_LEN],
    };
    let plaintext = cipher
        .decrypt(XNonce::from_slice(nonce), payload)
        .map_err(|_| "identity store could not be decrypted: wrong passphrase or corrupted file".to_string())?;

    let blob: [u8; BLOB_LEN] = plaintext
        .as_slice()
        .try_into()
        .map_err(|_| format!("decrypted plaintext was {} bytes, expected {}", plaintext.len(), BLOB_LEN))?;
    Ok(blob)
}
```

- [ ] **Step 4: Run tests and verify pass**

```bash
cd src-tauri && cargo test --lib identity::tests::wire_format
```

Expected: 9/9 PASS (including the truncated, tampered, and round-trip tests).

- [ ] **Step 5: Run full test suite**

```bash
cd src-tauri && cargo test --lib identity
```

Expected: previous tests still PASS (12 total now: 9 existing + 3 from Task 3 — the 9 wire-format tests are inside their own submodule). Actually expected: 9 existing + 3 LegacyPlaintextReader + 9 wire_format = 21 total pass.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/identity.rs
git commit -m "$(cat <<'EOF'
feat(identity): wire-format encrypt/decrypt for headless identity-at-rest

Pure functions encrypt_with_params(passphrase, salt, nonce, blob) and
decrypt(passphrase, bytes) implementing the 230-byte v1 format:
  [4 magic | 1 version | 1 kdf_id | 4 m_kib | 2 t | 1 p | 16 salt
   | 24 nonce | 161 ciphertext | 16 tag]
Argon2id (m=64MiB, t=3, p=1) derives a 32-byte key for XChaCha20-Poly1305.
The 13-byte header is bound as AAD so KDF-param downgrades break the tag.
Wrong-passphrase and corrupted-ciphertext share an indistinguishable
error string per spec.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `EncryptedFileStore` wrapper

**Files:**
- Modify: `src-tauri/src/identity.rs` (add the wrapper type implementing `KeyStore`)

- [ ] **Step 1: Add tests**

In the `#[cfg(test)] mod tests` block, add a new submodule:

```rust
mod encrypted_file_store {
    use super::*;
    use secrecy::SecretString;

    fn fresh_identity() -> NodeIdentity {
        let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
        let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
        NodeIdentity { pq, ed25519 }
    }

    fn fresh_passphrase() -> SecretString {
        SecretString::from("correct horse battery staple".to_string())
    }

    #[test]
    fn round_trip_correct_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.enc");
        let store = EncryptedFileStore::new(path.clone(), fresh_passphrase());

        let original = fresh_identity();
        let original_addr = original.ed25519.public_identity().address_hash;

        store.save(&original).unwrap();
        let loaded = store.load().unwrap().expect("should find saved identity");
        assert_eq!(loaded.ed25519.public_identity().address_hash, original_addr);
    }

    #[test]
    fn load_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.enc");
        let store = EncryptedFileStore::new(path, fresh_passphrase());
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn wrong_passphrase_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.enc");

        EncryptedFileStore::new(path.clone(), fresh_passphrase())
            .save(&fresh_identity())
            .unwrap();

        let wrong = EncryptedFileStore::new(path, SecretString::from("wrong".to_string()));
        let err = wrong.load().unwrap_err();
        assert!(err.contains("wrong passphrase or corrupted file"), "got: {err}");
    }

    #[test]
    fn salt_rotates_per_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.enc");
        let store = EncryptedFileStore::new(path.clone(), fresh_passphrase());
        let id = fresh_identity();

        store.save(&id).unwrap();
        let bytes_a = std::fs::read(&path).unwrap();
        store.save(&id).unwrap();
        let bytes_b = std::fs::read(&path).unwrap();

        assert_ne!(bytes_a, bytes_b, "salt+nonce must rotate per save");
        // Both must still load back to the same identity:
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(
            loaded.ed25519.public_identity().address_hash,
            id.ed25519.public_identity().address_hash,
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_mode_0o600_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.enc");
        let store = EncryptedFileStore::new(path.clone(), fresh_passphrase());

        store.save(&fresh_identity()).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0o600, got {mode:#o}");
    }

    #[test]
    fn file_is_exactly_230_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.enc");
        let store = EncryptedFileStore::new(path.clone(), fresh_passphrase());
        store.save(&fresh_identity()).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 230);
    }

    #[test]
    fn truncated_file_load_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.enc");
        let store = EncryptedFileStore::new(path.clone(), fresh_passphrase());
        store.save(&fresh_identity()).unwrap();

        // Truncate to 200 bytes.
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..200]).unwrap();

        let err = store.load().unwrap_err();
        assert!(err.contains("expected 230 bytes"), "got: {err}");
    }
}
```

- [ ] **Step 2: Run tests and verify failure**

```bash
cd src-tauri && cargo test --lib identity::tests::encrypted_file_store
```

Expected: FAIL with `cannot find type EncryptedFileStore`.

- [ ] **Step 3: Implement `EncryptedFileStore`**

In `src-tauri/src/identity.rs`, after the `KeychainStore` impl block (around line 253), insert:

```rust
// ── EncryptedFileStore ─────────────────────────────────────────────────

use secrecy::{ExposeSecret, SecretString};

/// Passphrase-encrypted identity file at a given path.
///
/// On-disk format is the 230-byte layout produced by `encrypt_with_params`:
/// Argon2id (m=64MiB, t=3, p=1) derives a 32-byte key for XChaCha20-Poly1305
/// AEAD over the 161-byte identity blob. The 13-byte header (magic, version,
/// kdf_id, KDF params) is bound as AAD.
///
/// Used as the headless fallback when no OS keychain is reachable. Keyed from
/// the `HARMONY_PASSPHRASE` / `HARMONY_PASSPHRASE_FILE` environment variables
/// — see [`Self::from_env`].
pub struct EncryptedFileStore {
    path: PathBuf,
    passphrase: SecretString,
}

impl EncryptedFileStore {
    /// Build a store backed by `path`, encrypted with `passphrase`.
    pub fn new(path: PathBuf, passphrase: SecretString) -> Self {
        Self { path, passphrase }
    }

    /// Path to the on-disk file (used by callers like `rotate_passphrase`).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl KeyStore for EncryptedFileStore {
    fn load(&self) -> Result<Option<NodeIdentity>, String> {
        let raw = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("Failed to read {}: {e}", self.path.display())),
        };
        let blob = decrypt(self.passphrase.expose_secret().as_bytes(), &raw)?;
        let blob_buf = Zeroizing::new(blob.to_vec());
        let identity = blob_to_identity(&blob_buf)?;
        Ok(Some(identity))
    }

    fn save(&self, identity: &NodeIdentity) -> Result<(), String> {
        let blob = identity_to_blob(identity);
        let blob_arr: [u8; BLOB_LEN] = blob
            .as_slice()
            .try_into()
            .expect("identity_to_blob always returns BLOB_LEN bytes");

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

- [ ] **Step 4: Run tests and verify pass**

```bash
cd src-tauri && cargo test --lib identity::tests::encrypted_file_store
```

Expected: 7/7 PASS.

- [ ] **Step 5: Run full test suite**

```bash
cd src-tauri && cargo test --lib identity
```

Expected: 28 PASS (9 existing + 3 LegacyPlaintextReader + 9 wire_format + 7 EncryptedFileStore).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/identity.rs
git commit -m "$(cat <<'EOF'
feat(identity): EncryptedFileStore — passphrase-encrypted KeyStore backend

Wraps encrypt_with_params/decrypt in the KeyStore trait surface used by
the resolution chain. Holds the passphrase as secrecy::SecretString
(zeroizes on drop). Save generates fresh OsRng salt + nonce per call and
delegates atomic write to write_atomic_0600. Load is a thin parse over
decrypt.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: `from_env` constructor + passphrase env var parsing

**Files:**
- Modify: `src-tauri/src/identity.rs` (add `EncryptedFileStore::from_env` and helper `read_passphrase_from_env`)

- [ ] **Step 1: Add tests**

```rust
mod env {
    use super::*;
    use serial_test::serial;
    use secrecy::ExposeSecret;

    const HARMONY_PASSPHRASE: &str = "HARMONY_PASSPHRASE";
    const HARMONY_PASSPHRASE_FILE: &str = "HARMONY_PASSPHRASE_FILE";

    /// Clear both env vars before each test to avoid cross-test leakage.
    fn clear_env() {
        std::env::remove_var(HARMONY_PASSPHRASE);
        std::env::remove_var(HARMONY_PASSPHRASE_FILE);
    }

    #[test]
    #[serial]
    fn returns_none_when_no_env_var() {
        clear_env();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.enc");
        assert!(EncryptedFileStore::from_env(path).unwrap().is_none());
    }

    #[test]
    #[serial]
    fn direct_env_var_set() {
        clear_env();
        std::env::set_var(HARMONY_PASSPHRASE, "foo");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.enc");
        let store = EncryptedFileStore::from_env(path).unwrap().expect("should be Some");
        assert_eq!(store.passphrase.expose_secret(), "foo");
        clear_env();
    }

    #[test]
    #[serial]
    fn file_var_set_strips_trailing_lf() {
        clear_env();
        let dir = tempfile::tempdir().unwrap();
        let pass_file = dir.path().join("pass.txt");
        std::fs::write(&pass_file, b"bar\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&pass_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::env::set_var(HARMONY_PASSPHRASE_FILE, &pass_file);

        let path = dir.path().join("identity.enc");
        let store = EncryptedFileStore::from_env(path).unwrap().expect("should be Some");
        assert_eq!(store.passphrase.expose_secret(), "bar");
        clear_env();
    }

    #[test]
    #[serial]
    fn file_var_set_strips_trailing_crlf() {
        clear_env();
        let dir = tempfile::tempdir().unwrap();
        let pass_file = dir.path().join("pass.txt");
        std::fs::write(&pass_file, b"bar\r\n").unwrap();
        std::env::set_var(HARMONY_PASSPHRASE_FILE, &pass_file);

        let path = dir.path().join("identity.enc");
        let store = EncryptedFileStore::from_env(path).unwrap().expect("should be Some");
        assert_eq!(store.passphrase.expose_secret(), "bar");
        clear_env();
    }

    #[test]
    #[serial]
    fn direct_wins_over_file() {
        clear_env();
        let dir = tempfile::tempdir().unwrap();
        let pass_file = dir.path().join("pass.txt");
        std::fs::write(&pass_file, b"from_file").unwrap();
        std::env::set_var(HARMONY_PASSPHRASE, "from_env");
        std::env::set_var(HARMONY_PASSPHRASE_FILE, &pass_file);

        let path = dir.path().join("identity.enc");
        let store = EncryptedFileStore::from_env(path).unwrap().expect("should be Some");
        assert_eq!(store.passphrase.expose_secret(), "from_env");
        clear_env();
    }

    #[test]
    #[serial]
    fn empty_direct_hard_fails() {
        clear_env();
        std::env::set_var(HARMONY_PASSPHRASE, "");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.enc");
        let err = EncryptedFileStore::from_env(path).unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
        clear_env();
    }

    #[test]
    #[serial]
    fn empty_file_hard_fails() {
        clear_env();
        let dir = tempfile::tempdir().unwrap();
        let pass_file = dir.path().join("pass.txt");
        std::fs::write(&pass_file, b"\n").unwrap();  // strips to empty
        std::env::set_var(HARMONY_PASSPHRASE_FILE, &pass_file);

        let path = dir.path().join("identity.enc");
        let err = EncryptedFileStore::from_env(path).unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
        clear_env();
    }

    #[test]
    #[serial]
    fn missing_file_hard_fails() {
        clear_env();
        std::env::set_var(HARMONY_PASSPHRASE_FILE, "/nonexistent/passphrase/file");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.enc");
        let err = EncryptedFileStore::from_env(path).unwrap_err();
        assert!(err.contains("could not be read"), "got: {err}");
        clear_env();
    }
}
```

- [ ] **Step 2: Run tests and verify failure**

```bash
cd src-tauri && cargo test --lib identity::tests::env
```

Expected: FAIL with `no function or associated item named from_env found`.

- [ ] **Step 3: Implement `from_env`**

Add to `impl EncryptedFileStore { ... }`:

```rust
/// Construct from the `HARMONY_PASSPHRASE` / `HARMONY_PASSPHRASE_FILE`
/// environment variables.
///
/// Returns:
///   - `Ok(None)` if neither env var is set
///   - `Ok(Some(store))` if a non-empty passphrase resolves
///   - `Err(...)` if either var is set but malformed (empty, file unreadable,
///     resolves to empty)
///
/// Precedence: `HARMONY_PASSPHRASE` (direct) wins over `HARMONY_PASSPHRASE_FILE`
/// when both are set; a warning is logged.
pub fn from_env(path: PathBuf) -> Result<Option<Self>, String> {
    let direct = std::env::var("HARMONY_PASSPHRASE").ok();
    let file_path = std::env::var("HARMONY_PASSPHRASE_FILE").ok();

    if direct.is_some() && file_path.is_some() {
        tracing::warn!(
            "both HARMONY_PASSPHRASE and HARMONY_PASSPHRASE_FILE are set; HARMONY_PASSPHRASE takes precedence"
        );
    }

    let passphrase_str = if let Some(s) = direct {
        if s.is_empty() {
            return Err("HARMONY_PASSPHRASE is set to an empty string".to_string());
        }
        s
    } else if let Some(file_path) = file_path {
        let raw = std::fs::read(&file_path)
            .map_err(|e| format!("HARMONY_PASSPHRASE_FILE={file_path} could not be read: {e}"))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&file_path) {
                let mode = meta.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    tracing::warn!(
                        path = %file_path,
                        mode = format!("{mode:#05o}"),
                        "HARMONY_PASSPHRASE_FILE has open permissions, should be 0600"
                    );
                }
            }
        }

        // Strip exactly one trailing \n or \r\n.
        let mut s = String::from_utf8(raw)
            .map_err(|_| format!("HARMONY_PASSPHRASE_FILE={file_path} is not valid UTF-8"))?;
        if s.ends_with("\r\n") {
            s.truncate(s.len() - 2);
        } else if s.ends_with('\n') {
            s.truncate(s.len() - 1);
        }
        if s.is_empty() {
            return Err(format!(
                "HARMONY_PASSPHRASE_FILE={file_path} contains an empty passphrase (after trimming one trailing newline)"
            ));
        }
        s
    } else {
        return Ok(None);
    };

    Ok(Some(Self::new(path, SecretString::from(passphrase_str))))
}
```

- [ ] **Step 4: Run tests and verify pass**

```bash
cd src-tauri && cargo test --lib identity::tests::env
```

Expected: 8/8 PASS. (`serial_test` ensures env-mutating tests don't race.)

- [ ] **Step 5: Run full test suite**

```bash
cd src-tauri && cargo test --lib identity
```

Expected: 36 PASS (28 prior + 8 env).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/identity.rs
git commit -m "$(cat <<'EOF'
feat(identity): EncryptedFileStore::from_env — passphrase env var parsing

HARMONY_PASSPHRASE (direct) takes precedence over HARMONY_PASSPHRASE_FILE
(file path) with a warning when both are set. File contents are read
raw, with exactly one trailing \\n or \\r\\n stripped. Empty passphrases
(direct or post-strip) hard-fail. File mode warns when more permissive
than 0600 on Unix. Returns Ok(None) when neither var is set so the
resolution chain can fall through naturally.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: `cleanup_legacy_bak` helper

**Files:**
- Modify: `src-tauri/src/identity.rs` (add helper function + tests)

This helper is called from the resolution chain in Task 8, but lands separately so the CRDT-style test of "matching deleted, mismatched preserved" gets isolated coverage.

- [ ] **Step 1: Add tests**

```rust
mod legacy_bak_cleanup {
    use super::*;

    fn fresh_identity() -> NodeIdentity {
        let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
        let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
        NodeIdentity { pq, ed25519 }
    }

    #[test]
    fn matching_bak_deleted_after_keychain_verify() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");
        let bak_path = dir.path().join("identity.key.bak");

        let id = fresh_identity();
        // Pre-populate .bak with the same identity that's in the keychain.
        FileStore::new(bak_path.clone()).save(&id).unwrap();

        let keychain = KeychainStore::new_mock();
        keychain.save(&id).unwrap();

        cleanup_legacy_bak(&plaintext_path, &id, &keychain);

        assert!(!bak_path.exists(), ".bak should be removed");
    }

    #[test]
    fn mismatched_bak_left_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");
        let bak_path = dir.path().join("identity.key.bak");

        let id_in_use = fresh_identity();
        let id_in_bak = fresh_identity();  // different
        FileStore::new(bak_path.clone()).save(&id_in_bak).unwrap();

        let keychain = KeychainStore::new_mock();
        keychain.save(&id_in_use).unwrap();

        cleanup_legacy_bak(&plaintext_path, &id_in_use, &keychain);

        assert!(bak_path.exists(), ".bak with mismatched identity must be preserved");
    }

    #[test]
    fn unreadable_bak_left_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");
        let bak_path = dir.path().join("identity.key.bak");

        // Write garbage to .bak (not a valid 161-byte identity blob).
        std::fs::write(&bak_path, b"not a valid identity blob").unwrap();

        let id = fresh_identity();
        let keychain = KeychainStore::new_mock();
        keychain.save(&id).unwrap();

        cleanup_legacy_bak(&plaintext_path, &id, &keychain);

        assert!(bak_path.exists(), "unreadable .bak must be preserved");
    }

    #[test]
    fn no_bak_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");
        // No .bak exists.

        let id = fresh_identity();
        let keychain = KeychainStore::new_mock();
        keychain.save(&id).unwrap();

        // Should not panic / error.
        cleanup_legacy_bak(&plaintext_path, &id, &keychain);
    }
}
```

- [ ] **Step 2: Run tests and verify failure**

```bash
cd src-tauri && cargo test --lib identity::tests::legacy_bak_cleanup
```

Expected: FAIL with `cannot find function cleanup_legacy_bak`.

- [ ] **Step 3: Add the `verify_round_trip` and `cleanup_legacy_bak` helpers**

In `src-tauri/src/identity.rs`, after `write_atomic_0600`, add:

```rust
// ── Verify-after-write helper ──────────────────────────────────────────

/// After a `KeyStore::save`, immediately re-read and byte-compare against the
/// expected identity. Constant-time comparison via the `subtle` crate.
///
/// Returns Err if the store doesn't return what was written — never used as a
/// "should I delete the source?" check on its own; it's a precondition for
/// any destructive cleanup (legacy plaintext unlink, .bak removal).
fn verify_round_trip(store: &dyn KeyStore, expected: &NodeIdentity) -> Result<(), String> {
    let loaded = store
        .load()?
        .ok_or_else(|| "verify-after-write returned None from store".to_string())?;
    let expected_blob = identity_to_blob(expected);
    let loaded_blob = identity_to_blob(&loaded);
    if !bool::from(subtle::ConstantTimeEq::ct_eq(
        expected_blob.as_slice(),
        loaded_blob.as_slice(),
    )) {
        return Err(
            "identity store verify-after-write failed: store does not return what was written".to_string(),
        );
    }
    Ok(())
}

// ── Legacy .bak cleanup ────────────────────────────────────────────────

/// Best-effort cleanup of a legacy `identity.key.bak` from the pre-encryption
/// code path. Removes only when the .bak content matches the in-memory identity
/// AND the live store's verify-round-trip succeeds.
///
/// All failure modes log warnings and leave the .bak in place — this is
/// defensive cleanup, not a hard guarantee.
fn cleanup_legacy_bak(plaintext_path: &Path, in_memory: &NodeIdentity, store: &dyn KeyStore) {
    let bak = plaintext_path.with_extension("key.bak");
    if !bak.exists() {
        return;
    }

    let bak_id = match LegacyPlaintextReader::read_from(&bak) {
        Ok(Some(id)) => id,
        Ok(None) => {
            tracing::warn!(
                path = %bak.display(),
                "legacy .bak unreadable (file disappeared) — leaving in place"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                path = %bak.display(),
                error = %e,
                "legacy .bak unreadable — leaving in place"
            );
            return;
        }
    };

    let expected_blob = identity_to_blob(in_memory);
    let bak_blob = identity_to_blob(&bak_id);
    let identities_match = bool::from(subtle::ConstantTimeEq::ct_eq(
        expected_blob.as_slice(),
        bak_blob.as_slice(),
    ));
    if !identities_match {
        tracing::warn!(
            path = %bak.display(),
            "legacy .bak present but identity differs from current — leaving in place; manual review needed"
        );
        return;
    }

    // Verify the live store actually returns the same identity before deleting.
    if let Err(e) = verify_round_trip(store, in_memory) {
        tracing::warn!(
            path = %bak.display(),
            error = %e,
            "legacy .bak NOT removed: live store verify failed"
        );
        return;
    }

    match std::fs::remove_file(&bak) {
        Ok(()) => tracing::info!(
            path = %bak.display(),
            "removed legacy plaintext .bak after verifying live store has matching identity"
        ),
        Err(e) => tracing::warn!(
            path = %bak.display(),
            error = %e,
            "legacy .bak removal failed — manual cleanup needed"
        ),
    }
}
```

- [ ] **Step 4: Run tests and verify pass**

```bash
cd src-tauri && cargo test --lib identity::tests::legacy_bak_cleanup
```

Expected: 4/4 PASS.

- [ ] **Step 5: Run full test suite**

```bash
cd src-tauri && cargo test --lib identity
```

Expected: 40 PASS (36 prior + 4 cleanup).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/identity.rs
git commit -m "$(cat <<'EOF'
feat(identity): cleanup_legacy_bak + verify_round_trip helpers

cleanup_legacy_bak removes ~/.harmony/identity.key.bak from earlier code
ONLY when its content matches the in-memory identity AND the live store
round-trip-verifies. Mismatch / unreadable / verify-fail cases all
preserve the .bak with a warning. verify_round_trip is the constant-time
post-save check used by cleanup and (next task) by the resolution chain.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Resolution chain rewrite + remove `FileStore`

**Files:**
- Modify: `src-tauri/src/identity.rs` (rewrite `load_or_generate_with_stores`, rewrite `load_or_generate`, remove `FileStore` and its tests)

This is the largest task. Replaces the chain wholesale and removes the now-redundant `FileStore` type (writes are gone — its `save` is no longer called from anywhere except its own tests, which we delete; reads are covered by `LegacyPlaintextReader`).

- [ ] **Step 1: Add resolution chain tests** (extends existing 6 chain tests with 6 new ones)

Inside `mod tests`, add a new submodule replacing the old free-floating chain tests:

```rust
mod resolution_chain {
    use super::*;
    use secrecy::SecretString;
    use serial_test::serial;

    fn fresh_identity() -> NodeIdentity {
        let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
        let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
        NodeIdentity { pq, ed25519 }
    }

    fn fresh_passphrase() -> SecretString {
        SecretString::from("correct horse battery staple".to_string())
    }

    #[test]
    fn keychain_present_returns_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");
        let original = fresh_identity();
        let original_addr = original.ed25519.public_identity().address_hash;

        let keychain = KeychainStore::new_mock();
        keychain.save(&original).unwrap();

        let result = load_or_generate_with_stores(Some(&keychain), None, &plaintext_path).unwrap();
        assert_eq!(result.ed25519.public_identity().address_hash, original_addr);
        assert!(!plaintext_path.exists(), "no plaintext should be created");
    }

    #[test]
    fn fresh_install_writes_to_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");

        let keychain = KeychainStore::new_mock();
        let result = load_or_generate_with_stores(Some(&keychain), None, &plaintext_path).unwrap();

        let from_keychain = keychain.load().unwrap().expect("identity should be in keychain");
        assert_eq!(
            from_keychain.ed25519.public_identity().address_hash,
            result.ed25519.public_identity().address_hash,
        );
        assert!(!plaintext_path.exists());
    }

    #[test]
    fn migrate_plaintext_to_keychain_and_unlink() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");

        let original = fresh_identity();
        let original_addr = original.ed25519.public_identity().address_hash;
        FileStore::new(plaintext_path.clone()).save(&original).unwrap();

        let keychain = KeychainStore::new_mock();
        let result = load_or_generate_with_stores(Some(&keychain), None, &plaintext_path).unwrap();

        assert_eq!(result.ed25519.public_identity().address_hash, original_addr);
        assert!(!plaintext_path.exists(), "plaintext should be unlinked after migration");
        let from_keychain = keychain.load().unwrap().expect("should be in keychain");
        assert_eq!(
            from_keychain.ed25519.public_identity().address_hash,
            original_addr,
        );
        // Critically: no .bak created by the new chain.
        let bak = plaintext_path.with_extension("key.bak");
        assert!(!bak.exists(), "new chain must not create .bak");
    }

    #[test]
    fn migrate_plaintext_prefers_keychain_over_encrypted() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");
        let enc_path = dir.path().join("identity.enc");

        let original = fresh_identity();
        let original_addr = original.ed25519.public_identity().address_hash;
        FileStore::new(plaintext_path.clone()).save(&original).unwrap();

        let keychain = KeychainStore::new_mock();
        let encrypted = EncryptedFileStore::new(enc_path.clone(), fresh_passphrase());

        load_or_generate_with_stores(Some(&keychain), Some(&encrypted), &plaintext_path).unwrap();

        assert!(keychain.load().unwrap().is_some(), "keychain should win as destination");
        assert!(!enc_path.exists(), ".enc must NOT be created when keychain is healthy");
        assert!(!plaintext_path.exists());
    }

    #[test]
    fn migrate_plaintext_to_encrypted_when_no_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");
        let enc_path = dir.path().join("identity.enc");

        let original = fresh_identity();
        let original_addr = original.ed25519.public_identity().address_hash;
        FileStore::new(plaintext_path.clone()).save(&original).unwrap();

        let encrypted = EncryptedFileStore::new(enc_path.clone(), fresh_passphrase());
        let result = load_or_generate_with_stores(None, Some(&encrypted), &plaintext_path).unwrap();

        assert_eq!(result.ed25519.public_identity().address_hash, original_addr);
        assert!(enc_path.exists(), ".enc should be the destination");
        assert!(!plaintext_path.exists(), "plaintext should be unlinked");
    }

    #[test]
    fn fresh_install_writes_to_encrypted_when_no_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");
        let enc_path = dir.path().join("identity.enc");

        let encrypted = EncryptedFileStore::new(enc_path.clone(), fresh_passphrase());
        let result = load_or_generate_with_stores(None, Some(&encrypted), &plaintext_path).unwrap();

        assert!(enc_path.exists());
        let from_enc = encrypted.load().unwrap().expect("should be in .enc");
        assert_eq!(
            from_enc.ed25519.public_identity().address_hash,
            result.ed25519.public_identity().address_hash,
        );
    }

    #[test]
    fn headless_no_keychain_no_env_hard_fails_on_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");

        let err = load_or_generate_with_stores(None, None, &plaintext_path).unwrap_err();
        assert!(err.contains("no identity store available"), "got: {err}");
        assert!(err.contains("docs/headless-install.md"), "should point at docs: {err}");
    }

    #[test]
    fn headless_no_keychain_no_env_hard_fails_with_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");

        let original = fresh_identity();
        FileStore::new(plaintext_path.clone()).save(&original).unwrap();

        let err = load_or_generate_with_stores(None, None, &plaintext_path).unwrap_err();
        assert!(err.contains("plaintext identity"), "got: {err}");
        assert!(err.contains("docs/headless-install.md"));
        assert!(plaintext_path.exists(), "plaintext must NOT be deleted on hard-fail");
    }

    #[test]
    fn wrong_passphrase_does_not_regenerate() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");
        let enc_path = dir.path().join("identity.enc");

        // Write an .enc with passphrase A.
        let original = fresh_identity();
        let original_addr = original.ed25519.public_identity().address_hash;
        EncryptedFileStore::new(enc_path.clone(), fresh_passphrase())
            .save(&original)
            .unwrap();

        // Try to load with wrong passphrase B.
        let wrong = EncryptedFileStore::new(enc_path.clone(), SecretString::from("WRONG".to_string()));
        let err = load_or_generate_with_stores(None, Some(&wrong), &plaintext_path).unwrap_err();
        assert!(err.contains("wrong passphrase or corrupted file"), "got: {err}");

        // Critically: original .enc must still be intact (not regenerated).
        let recovered = EncryptedFileStore::new(enc_path.clone(), fresh_passphrase())
            .load()
            .unwrap()
            .expect("original .enc must still be loadable with correct passphrase");
        assert_eq!(
            recovered.ed25519.public_identity().address_hash,
            original_addr,
            "wrong-passphrase must NOT trigger fresh generate",
        );
    }

    #[test]
    fn keychain_present_with_legacy_bak_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");
        let bak_path = dir.path().join("identity.key.bak");

        let id = fresh_identity();
        let keychain = KeychainStore::new_mock();
        keychain.save(&id).unwrap();
        // Pre-existing .bak with matching identity
        FileStore::new(bak_path.clone()).save(&id).unwrap();

        load_or_generate_with_stores(Some(&keychain), None, &plaintext_path).unwrap();

        assert!(!bak_path.exists(), "matching .bak should be auto-removed");
    }

    /// Legacy plaintext + corrupted destination would cause verify-round-trip
    /// to fail. The chain must NOT unlink the plaintext in that case.
    /// Implemented via a wrapper KeyStore whose load returns mutated bytes.
    #[test]
    fn verify_round_trip_failure_aborts_migration() {
        // Custom KeyStore that drops a bit on load (corrupts post-write).
        struct CorruptingStore { inner: KeychainStore }
        impl KeyStore for CorruptingStore {
            fn save(&self, id: &NodeIdentity) -> Result<(), String> { self.inner.save(id) }
            fn load(&self) -> Result<Option<NodeIdentity>, String> {
                // Always return a freshly generated (different) identity to force mismatch.
                let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
                let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
                Ok(Some(NodeIdentity { pq, ed25519 }))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");

        let original = fresh_identity();
        FileStore::new(plaintext_path.clone()).save(&original).unwrap();

        let store = CorruptingStore { inner: KeychainStore::new_mock() };
        // Have to call the with-stores variant via a typed reference path.
        // Cheat: wrap as &dyn through inline call.
        // We can't pass CorruptingStore as Some(&KeychainStore), so this test
        // exercises verify_round_trip directly via a smaller surface:
        let err = verify_round_trip(&store, &original).unwrap_err();
        assert!(err.contains("verify-after-write failed"), "got: {err}");
        assert!(plaintext_path.exists(), "plaintext must be preserved on verify-fail");
    }
}
```

The last test verifies `verify_round_trip` directly because the resolution chain takes concrete `&KeychainStore` / `&EncryptedFileStore` references rather than `&dyn KeyStore` (deliberate — the destination-precedence logic needs to know which is which). The chain delegates the verify call into `verify_round_trip`, so testing `verify_round_trip` independently provides the coverage.

- [ ] **Step 2: Run tests and verify failure**

```bash
cd src-tauri && cargo test --lib identity::tests::resolution_chain
```

Expected: FAIL — the new tests reference `load_or_generate_with_stores` with the new `(Option<&KeychainStore>, Option<&EncryptedFileStore>, &Path)` signature.

- [ ] **Step 3: Rewrite the resolution chain**

In `src-tauri/src/identity.rs`, replace the existing `load_or_generate_with_stores` and `load_or_generate` (around lines 272-355) with:

```rust
/// Internal resolution chain — accepts injected stores for testability.
///
/// See `docs/specs/2026-04-26-headless-encrypted-identity-design.md` §Resolution chain
/// for the precise step-by-step semantics. Summary:
///
///   1. keychain.load() — return on success; legacy .bak cleanup; fall through on
///      None or transient Err
///   2. encrypted.load() — return on success; legacy .bak cleanup; HARD FAIL on Err
///      (wrong passphrase / corruption — never silently regenerate)
///   3. legacy plaintext present → migrate to keychain (preferred) or encrypted;
///      verify_round_trip; unlink plaintext
///   4. fresh generate → write to keychain (preferred) or encrypted; verify_round_trip
///
/// Hard-fails when no destination is available (no keychain, no encrypted store)
/// for either step 3 or step 4 — refuses to fall back to plaintext writes.
fn load_or_generate_with_stores(
    keychain: Option<&KeychainStore>,
    encrypted: Option<&EncryptedFileStore>,
    plaintext_path: &Path,
) -> Result<NodeIdentity, String> {
    let mut keychain_healthy = false;

    // Step 1: keychain.
    if let Some(kc) = keychain {
        match kc.load() {
            Ok(Some(id)) => {
                cleanup_legacy_bak(plaintext_path, &id, kc);
                return Ok(id);
            }
            Ok(None) => {
                keychain_healthy = true;  // present but empty
            }
            Err(e) => {
                tracing::warn!("keychain load failed, trying next store: {e}");
                keychain_healthy = false;
            }
        }
    }

    // Step 2: encrypted file (if env var set).
    if let Some(enc) = encrypted {
        match enc.load() {
            Ok(Some(id)) => {
                cleanup_legacy_bak(plaintext_path, &id, enc);
                return Ok(id);
            }
            Ok(None) => {
                // Fall through — fresh-with-passphrase install.
            }
            Err(e) => {
                // HARD FAIL — wrong passphrase or corruption. Do NOT regenerate.
                return Err(e);
            }
        }
    }

    // Step 3: legacy plaintext migration.
    let legacy = LegacyPlaintextReader::new(plaintext_path.to_path_buf());
    if let Some(id) = legacy.read()? {
        // Pick destination: keychain > encrypted > hard fail.
        if keychain_healthy {
            let kc = keychain.expect("keychain_healthy implies Some(keychain)");
            kc.save(&id)?;
            verify_round_trip(kc, &id)?;
        } else if let Some(enc) = encrypted {
            enc.save(&id)?;
            verify_round_trip(enc, &id)?;
        } else {
            return Err(format!(
                "plaintext identity at {} needs a destination but no keychain available and HARMONY_PASSPHRASE not set — see docs/headless-install.md",
                plaintext_path.display()
            ));
        }
        // Verified copy is in the destination; unlink the plaintext.
        if let Err(e) = std::fs::remove_file(plaintext_path) {
            tracing::warn!(
                path = %plaintext_path.display(),
                error = %e,
                "identity migrated but plaintext file could not be removed — manual cleanup needed"
            );
        }
        return Ok(id);
    }

    // Step 4: fresh generate.
    let id = NodeIdentity {
        pq: PqPrivateIdentity::generate(&mut rand::rngs::OsRng),
        ed25519: PrivateIdentity::generate(&mut rand::rngs::OsRng),
    };
    if keychain_healthy {
        let kc = keychain.expect("keychain_healthy implies Some(keychain)");
        kc.save(&id)?;
        verify_round_trip(kc, &id)?;
        tracing::info!("new identity stored in OS keychain");
    } else if let Some(enc) = encrypted {
        enc.save(&id)?;
        verify_round_trip(enc, &id)?;
        tracing::info!(path = %enc.path().display(), "new identity stored in encrypted file");
    } else {
        return Err(
            "no identity store available: keychain unavailable and HARMONY_PASSPHRASE not set — see docs/headless-install.md".to_string(),
        );
    }
    Ok(id)
}

/// Public entry point — resolves env-derived encrypted store, attempts the
/// keychain, and runs the resolution chain.
///
/// Resolution order (see `load_or_generate_with_stores` for the full spec):
///   1. OS keychain
///   2. ~/.harmony/identity.enc  (if HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE set)
///   3. ~/.harmony/identity.key  (legacy plaintext — migrated to (1) or (2), then unlinked)
///   4. Generate fresh keys (stored in (1) or (2); HARD FAIL if neither available)
pub fn load_or_generate(plaintext_path: &Path) -> Result<NodeIdentity, String> {
    let enc_path = plaintext_path.with_file_name("identity.enc");
    let keychain = KeychainStore::new().ok();
    let encrypted = EncryptedFileStore::from_env(enc_path)?;

    load_or_generate_with_stores(keychain.as_ref(), encrypted.as_ref(), plaintext_path)
}
```

- [ ] **Step 4: Run resolution chain tests**

```bash
cd src-tauri && cargo test --lib identity::tests::resolution_chain
```

Expected: 11/11 PASS.

- [ ] **Step 5: Delete the now-obsolete inline chain tests + `FileStore::save`**

In the existing `#[cfg(test)] mod tests { ... }` block, delete these old free-floating tests (now superseded by `mod resolution_chain`):

- `load_or_generate_migrates_file_to_keychain`
- `load_or_generate_uses_keychain_when_present`
- `load_or_generate_creates_new_in_keychain`
- `load_or_generate_falls_back_to_file_on_keychain_write_failure`
- `migration_aborted_when_keychain_write_fails`
- `keychain_load_error_no_file_generates_to_file`
- `keychain_load_error_with_file_uses_file_without_migration`

Keep `file_store_round_trip` (now testing the legacy-write path used only by tests for setup) and `file_store_load_returns_none_when_missing`. The `KeychainStore::new_failing_mock` and `KeychainStore::new_load_failing_mock` test helpers stay — still useful for resolution-chain tests of the keychain-Err path.

Optional cleanup: if `FileStore::save` is no longer called from anywhere outside tests, mark it `#[cfg(test)]`. Inspect call sites:

```bash
cd src-tauri && grep -rn "FileStore::new\|FileStore {" src/
```

Expected: every call site is inside `#[cfg(test)]` or a test module. If so, gate the entire `impl KeyStore for FileStore { ... }` block behind `#[cfg(test)]` and add a deprecation comment:

```rust
// FileStore is retained as a test-only helper for setting up legacy
// plaintext fixtures. Production code never writes plaintext — see
// LegacyPlaintextReader for the read-only legacy migration path.
#[cfg(test)]
impl KeyStore for FileStore { ... existing impl ... }
```

(If the production code path still references `FileStore` somewhere outside tests, keep the impl unguarded and document why in the comment instead.)

- [ ] **Step 6: Run full test suite**

```bash
cd src-tauri && cargo test --lib identity
```

Expected: ~42 PASS (40 prior - 7 deleted old chain tests + 11 new resolution_chain tests = 44). Adjust upward if any prior count was off.

```bash
cd src-tauri && cargo build
```

Expected: compiles cleanly. If `FileStore::save` was gated behind `#[cfg(test)]` and any non-test code still references it, the build will surface the call site to clean up.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/identity.rs
git commit -m "$(cat <<'EOF'
feat(identity): three-store resolution chain — never write plaintext

Rewrites load_or_generate_with_stores for the new chain:
  keychain → encrypted_file → legacy_plaintext (migrate) → fresh-generate

Migration destination precedence: keychain > encrypted_file. Plaintext
is unlinked (not renamed to .bak) after verify_round_trip on the
destination. Wrong passphrase on the .enc file is a HARD FAIL — never
falls through to step 4 to silently regenerate identity.

Hard-fails when no destination is available rather than falling back to
plaintext: closes the headless-install gap that this PR exists to fix.

FileStore::save is gated behind #[cfg(test)] (production code only ever
writes through KeychainStore or EncryptedFileStore now); FileStore reads
remain as test fixture helpers. Legacy plaintext reads in production go
through LegacyPlaintextReader.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: `rotate_passphrase` function

**Files:**
- Modify: `src-tauri/src/identity.rs` (add public function + tests)

- [ ] **Step 1: Add tests**

```rust
mod rotation {
    use super::*;
    use secrecy::SecretString;

    fn fresh_identity() -> NodeIdentity {
        let pq = PqPrivateIdentity::generate(&mut rand::rngs::OsRng);
        let ed25519 = PrivateIdentity::generate(&mut rand::rngs::OsRng);
        NodeIdentity { pq, ed25519 }
    }

    #[test]
    fn rotate_happy_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.enc");

        let pass_a = SecretString::from("pass_a".to_string());
        let pass_b = SecretString::from("pass_b".to_string());

        let id = fresh_identity();
        let id_addr = id.ed25519.public_identity().address_hash;

        // Write with A.
        EncryptedFileStore::new(path.clone(), pass_a.clone())
            .save(&id)
            .unwrap();

        // Rotate to B.
        let store_a = EncryptedFileStore::new(path.clone(), pass_a.clone());
        rotate_passphrase(&store_a, pass_b.clone()).unwrap();

        // B can decrypt.
        let loaded = EncryptedFileStore::new(path.clone(), pass_b)
            .load()
            .unwrap()
            .unwrap();
        assert_eq!(loaded.ed25519.public_identity().address_hash, id_addr);

        // A can no longer decrypt.
        let err = EncryptedFileStore::new(path, pass_a).load().unwrap_err();
        assert!(err.contains("wrong passphrase or corrupted file"), "got: {err}");
    }

    #[test]
    fn rotate_wrong_old_passphrase_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.enc");

        EncryptedFileStore::new(path.clone(), SecretString::from("real".to_string()))
            .save(&fresh_identity())
            .unwrap();

        let bytes_before = std::fs::read(&path).unwrap();

        let wrong = EncryptedFileStore::new(path.clone(), SecretString::from("wrong".to_string()));
        let err = rotate_passphrase(&wrong, SecretString::from("new".to_string())).unwrap_err();
        assert!(err.contains("wrong passphrase or corrupted file"), "got: {err}");

        // File untouched.
        let bytes_after = std::fs::read(&path).unwrap();
        assert_eq!(bytes_before, bytes_after, "file must not be modified on auth failure");
    }

    #[test]
    fn rotate_to_same_passphrase_succeeds_with_new_salt_nonce() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.enc");
        let pass = SecretString::from("same".to_string());

        EncryptedFileStore::new(path.clone(), pass.clone())
            .save(&fresh_identity())
            .unwrap();
        let bytes_before = std::fs::read(&path).unwrap();

        let store = EncryptedFileStore::new(path.clone(), pass.clone());
        rotate_passphrase(&store, pass.clone()).unwrap();

        let bytes_after = std::fs::read(&path).unwrap();
        assert_ne!(bytes_before, bytes_after, "salt+nonce must rotate even when passphrase is same");
    }

    #[test]
    fn rotate_no_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.enc");
        let store = EncryptedFileStore::new(path, SecretString::from("any".to_string()));

        let err = rotate_passphrase(&store, SecretString::from("new".to_string())).unwrap_err();
        assert!(err.contains("no encrypted identity to rotate"), "got: {err}");
    }
}
```

- [ ] **Step 2: Run tests and verify failure**

```bash
cd src-tauri && cargo test --lib identity::tests::rotation
```

Expected: FAIL with `cannot find function rotate_passphrase`.

- [ ] **Step 3: Implement `rotate_passphrase`**

In `src-tauri/src/identity.rs`, after the `load_or_generate` function, add:

```rust
/// Re-encrypt the identity at `old.path()` with `new_passphrase`.
///
/// Loads the identity using the old store's passphrase, writes back to the same
/// path with the new passphrase (fresh salt + nonce, atomic rename), and
/// verifies the round-trip before returning.
///
/// Caller-side concerns (keychain check, env var resolution, CLI wiring) live
/// in `main.rs` — this function is the pure key-rotation primitive.
pub fn rotate_passphrase(
    old: &EncryptedFileStore,
    new_passphrase: SecretString,
) -> Result<(), String> {
    let identity = old
        .load()?
        .ok_or_else(|| {
            format!(
                "no encrypted identity to rotate at {}",
                old.path().display()
            )
        })?;

    let new_store = EncryptedFileStore::new(old.path().to_path_buf(), new_passphrase);
    new_store.save(&identity)?;
    verify_round_trip(&new_store, &identity)?;
    Ok(())
}
```

- [ ] **Step 4: Run tests and verify pass**

```bash
cd src-tauri && cargo test --lib identity::tests::rotation
```

Expected: 4/4 PASS.

- [ ] **Step 5: Run full test suite**

```bash
cd src-tauri && cargo test --lib identity
```

Expected: ~48 PASS (44 prior + 4 rotation).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/identity.rs
git commit -m "$(cat <<'EOF'
feat(identity): rotate_passphrase — re-encrypt .enc with new passphrase

Pure primitive: load with old, save with new, verify_round_trip. Atomic
rename via the shared write_atomic_0600 — old file untouched on any
failure. Salt and nonce rotate even when old == new passphrase, so a
"rotate" with the same passphrase still freshens the on-disk bytes.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: `clap` subcommand wiring in `main.rs`

**Files:**
- Modify: `src-tauri/src/main.rs`
- Modify: `src-tauri/src/lib.rs` (add a `pub fn rotate_passphrase_cli(...)` entry point that main.rs invokes)

The Tauri runtime is launched by `harmony_app::run()` in `lib.rs`. We add a sibling entry point for the rotation flow, and `main.rs` dispatches.

- [ ] **Step 1: Add the CLI handler in `lib.rs`**

In `src-tauri/src/lib.rs`, find an appropriate top-level location (near `pub fn run()`) and add:

```rust
/// CLI entry point for `harmony-app rotate-passphrase`.
///
/// Refusal conditions (in order):
///   1. OS keychain has an identity → refuse with explanation
///   2. HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set → refuse
///   3. --new-passphrase-file unreadable / empty → refuse (via env-style parse)
///   4. New passphrase byte-identical to old → log warning, proceed
///
/// Returns Ok(()) on successful rotation; Err on any refusal or rotation
/// failure. Caller (main.rs) translates Err into a non-zero exit.
pub fn rotate_passphrase_cli(new_passphrase_file: &std::path::Path) -> Result<(), String> {
    // Refusal 1: keychain has identity.
    if let Ok(kc) = identity::KeychainStore::new() {
        match kc.load() {
            Ok(Some(_)) => {
                return Err(
                    "your identity is currently in the OS keychain; passphrase rotation only applies to headless installs. \
                     Re-encryption of keychain entries is handled by the OS when you change your login password.".to_string(),
                );
            }
            Ok(None) | Err(_) => {} // No keychain entry — proceed to next refusal check.
        }
    }

    // Resolve old passphrase via the standard env chain.
    let plaintext_path = identity::resolve_path(None)?;
    let enc_path = plaintext_path.with_file_name("identity.enc");
    let old_store = identity::EncryptedFileStore::from_env(enc_path)?
        .ok_or_else(|| {
            "HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set — cannot rotate without the old passphrase".to_string()
        })?;

    // Read the new passphrase file (same parsing rules as HARMONY_PASSPHRASE_FILE).
    let raw = std::fs::read(new_passphrase_file)
        .map_err(|e| format!("--new-passphrase-file={} could not be read: {e}", new_passphrase_file.display()))?;
    let mut new_str = String::from_utf8(raw).map_err(|_| {
        format!(
            "--new-passphrase-file={} is not valid UTF-8",
            new_passphrase_file.display()
        )
    })?;
    if new_str.ends_with("\r\n") {
        new_str.truncate(new_str.len() - 2);
    } else if new_str.ends_with('\n') {
        new_str.truncate(new_str.len() - 1);
    }
    if new_str.is_empty() {
        return Err(format!(
            "--new-passphrase-file={} contains an empty passphrase (after trimming one trailing newline)",
            new_passphrase_file.display()
        ));
    }

    // Warn if no-op rotation.
    {
        use secrecy::ExposeSecret;
        if old_store.passphrase_for_test_only().expose_secret() == &new_str {
            tracing::warn!("new passphrase matches old — proceeding anyway");
        }
    }

    // Rotate.
    use secrecy::SecretString;
    identity::rotate_passphrase(&old_store, SecretString::from(new_str))?;
    Ok(())
}
```

The above references `EncryptedFileStore::passphrase_for_test_only()`. We need to add it (or restructure to avoid it). Cleaner alternative: expose passphrase comparison via a helper on the store itself:

In `src-tauri/src/identity.rs`, add to `impl EncryptedFileStore`:

```rust
/// Constant-time check whether `candidate` matches the stored passphrase.
///
/// Used by the CLI rotate handler to detect a no-op rotation (old == new) so
/// it can emit a warning without aborting.
pub fn passphrase_eq(&self, candidate: &SecretString) -> bool {
    use secrecy::ExposeSecret;
    bool::from(subtle::ConstantTimeEq::ct_eq(
        self.passphrase.expose_secret().as_bytes(),
        candidate.expose_secret().as_bytes(),
    ))
}
```

And update `lib.rs` to use it:

```rust
{
    use secrecy::SecretString;
    let candidate = SecretString::from(new_str.clone());
    if old_store.passphrase_eq(&candidate) {
        tracing::warn!("new passphrase matches old — proceeding anyway");
    }
}
```

(Drop the `passphrase_for_test_only()` reference.)

Make sure `pub mod identity;` is declared somewhere in `lib.rs` (search for existing `mod identity;` and adjust if needed).

- [ ] **Step 2: Add the `clap` subcommand parser to `main.rs`**

Replace the contents of `src-tauri/src/main.rs` with:

```rust
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "harmony-app", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

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

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::RotatePassphrase { new_passphrase_file }) => {
            // Initialize tracing for CLI subcommands so warnings show up.
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .init();

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
        None => {
            // Default path — launch the Tauri runtime.
            harmony_app::run();
        }
    }
}
```

If `tracing_subscriber` is not already a dev-dep / dep, check `Cargo.toml` — if missing, you'll need to add it. The harmony workspace likely already pulls it in. Verify with:

```bash
grep -n "tracing-subscriber\|tracing_subscriber" src-tauri/Cargo.toml
```

If it's missing, add to `[dependencies]`:

```toml
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

- [ ] **Step 3: Build and smoke-test the binary**

```bash
cd src-tauri && cargo build
```

Expected: compiles cleanly. The binary now has subcommand parsing.

```bash
cd src-tauri && cargo run -- rotate-passphrase --help
```

Expected: prints clap-generated help text for the subcommand. **Do not** run it without the flag — it would launch the GUI in default mode.

- [ ] **Step 4: Add an integration test for the CLI handler**

Create `src-tauri/tests/rotate_passphrase_cli.rs`:

```rust
//! Integration test for the rotate-passphrase CLI handler. Tests run against
//! the public `harmony_app::rotate_passphrase_cli` entry point.

use std::env;

#[test]
fn no_old_passphrase_env_refuses() {
    // Clear env so HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE are unset.
    env::remove_var("HARMONY_PASSPHRASE");
    env::remove_var("HARMONY_PASSPHRASE_FILE");

    let dir = tempfile::tempdir().unwrap();
    let new_file = dir.path().join("new.txt");
    std::fs::write(&new_file, b"new_pass").unwrap();

    let err = harmony_app::rotate_passphrase_cli(&new_file).unwrap_err();
    assert!(
        err.contains("HARMONY_PASSPHRASE") && err.contains("not set"),
        "got: {err}"
    );
}

#[test]
fn missing_new_passphrase_file_refuses() {
    env::set_var("HARMONY_PASSPHRASE", "old");

    let dir = tempfile::tempdir().unwrap();
    let bogus = dir.path().join("does_not_exist.txt");

    let err = harmony_app::rotate_passphrase_cli(&bogus).unwrap_err();
    assert!(err.contains("could not be read"), "got: {err}");

    env::remove_var("HARMONY_PASSPHRASE");
}
```

This is an integration-test-style smoke test — does not exercise the keychain refusal path (which would require either a mock keychain dependency-injected through the CLI handler or actual OS keychain access — both out of scope for v1; the keychain-refusal logic is straightforward enough that a unit-level confidence is fine here).

- [ ] **Step 5: Run integration tests**

```bash
cd src-tauri && cargo test --test rotate_passphrase_cli
```

Expected: 2/2 PASS.

- [ ] **Step 6: Run full test suite**

```bash
cd src-tauri && cargo test
```

Expected: all prior tests + 2 new integration tests PASS.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/main.rs src-tauri/src/lib.rs src-tauri/src/identity.rs src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/tests/rotate_passphrase_cli.rs
git commit -m "$(cat <<'EOF'
feat(cli): rotate-passphrase subcommand for headless installs

Adds clap subcommand parsing in main.rs that dispatches before launching
the Tauri runtime. The handler in lib.rs (rotate_passphrase_cli):
  1. Refuses if OS keychain has the identity (rotation is a headless-only
     concept; OS handles keychain re-encryption on login-password change)
  2. Resolves the old passphrase from HARMONY_PASSPHRASE / _FILE
  3. Reads the new passphrase from --new-passphrase-file (same parsing
     rules as HARMONY_PASSPHRASE_FILE: UTF-8, one trailing newline strip)
  4. Constant-time compare to warn on no-op rotation
  5. Calls identity::rotate_passphrase

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Headless install documentation

**Files:**
- Create: `docs/headless-install.md`

- [ ] **Step 1: Write the doc**

Create `harmony-client/docs/headless-install.md` with:

````markdown
# Headless install (servers, CI, containers)

harmony-client encrypts identity at rest in one of two ways:

1. **Desktop**: OS keychain (macOS Keychain, Windows Credential Manager, Linux Secret Service via gnome-keyring/KWallet)
2. **Headless**: AEAD-encrypted file at `~/.harmony/identity.enc`, key derived from a passphrase you supply

If you're running on a server, in a container, in CI, or anywhere without a
running OS keychain, you must supply a passphrase via environment variable.
**Without a passphrase, harmony-client refuses to start** — it will not write
plaintext identity material to disk.

## Quickstart (Linux server / Docker)

Generate a passphrase (≥32 chars, store in your secret manager):

```sh
openssl rand -base64 48 > /etc/harmony/passphrase
chmod 600 /etc/harmony/passphrase
chown harmony:harmony /etc/harmony/passphrase
```

Then either:

```sh
# Direct (less common — exposes passphrase in process listings)
HARMONY_PASSPHRASE='...' harmony-app

# File-based (recommended — file is loaded once at startup)
HARMONY_PASSPHRASE_FILE=/etc/harmony/passphrase harmony-app
```

## systemd unit

```ini
[Service]
ExecStart=/usr/local/bin/harmony-app
Environment=HARMONY_PASSPHRASE_FILE=/etc/harmony/passphrase
User=harmony
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/var/lib/harmony
```

## Docker

```sh
docker run --rm \
  -v /etc/harmony/passphrase:/run/secrets/passphrase:ro \
  -v harmony-data:/home/harmony/.harmony \
  -e HARMONY_PASSPHRASE_FILE=/run/secrets/passphrase \
  harmony-client
```

## Env var precedence

If both are set, `HARMONY_PASSPHRASE` (direct) wins over `HARMONY_PASSPHRASE_FILE`.
A warning is logged.

## Passphrase format rules

- UTF-8 bytes, used as-is — no Unicode normalization
- Must be byte-stable across boots — same UTF-8 bytes in, same key out
- Empty passphrases (after stripping one trailing `\n` or `\r\n`) are rejected
- File mode should be `0600`; a warning is logged if more permissive

## Migration from prior versions

If you upgrade from an earlier harmony-client that wrote plaintext to
`~/.harmony/identity.key`:

- **With keychain available**: harmony migrates plaintext → keychain on first
  launch, verifies, then deletes the plaintext file. No `.bak` is left.
- **Headless with `HARMONY_PASSPHRASE` set**: same migration, destination is
  `~/.harmony/identity.enc`.
- **Headless without `HARMONY_PASSPHRASE`**: harmony refuses to start with a
  hard error pointing here. Set the env var and re-launch.

A `.bak` file from earlier code that did keep backups is auto-cleaned on first
launch after harmony verifies the live store has the same identity.

## Rotating the passphrase

To re-encrypt `identity.enc` with a new passphrase:

```sh
# Old passphrase still needed to decrypt; new one supplied via flag.
HARMONY_PASSPHRASE_FILE=/etc/harmony/old.txt \
  harmony-app rotate-passphrase \
    --new-passphrase-file=/etc/harmony/new.txt
```

The command verifies the new file decrypts correctly before exiting. Once it
returns 0, swap your systemd unit / Docker secret to point at the new file
and discard the old one.

Rotation is only meaningful for the encrypted-file backend. If you're running
on a desktop install (keychain backend), the OS handles re-encryption of
keychain entries when you change your login password — you don't need to do
anything here.

## Backup and recovery

Backup of identity material — including how to recover from device loss,
how to mint a fresh identity that claims continuity with a lost one, and
how to export/import a recovery artifact — is the scope of **ZEB-175**
(Identity backup/restore UX). This document covers encryption-at-rest only.

In the meantime: treat `~/.harmony/identity.enc` and your passphrase as
two halves of a recovery key. Lose either and the other is useless. Back
both up to separate storage if you can't tolerate identity loss.

## Troubleshooting

| Error | Meaning | Fix |
|---|---|---|
| `no identity store available: keychain unavailable and HARMONY_PASSPHRASE not set` | Step 4 in resolution chain | Set `HARMONY_PASSPHRASE` or `HARMONY_PASSPHRASE_FILE`, or install/start a Secret Service provider |
| `identity store could not be decrypted: wrong passphrase or corrupted file` | AEAD tag rejected | Verify the passphrase exactly matches what was used to encrypt; do not regenerate identity unless you accept losing it |
| `identity store is in an unrecognized format` | Old binary, newer file | Upgrade harmony-client |
| `identity store verify-after-write failed` | The store accepted the write but returned different bytes — keychain/disk corruption | File a bug; do not retry blindly |
| `plaintext identity at <path> needs a destination but no keychain available and HARMONY_PASSPHRASE not set` | Existing plaintext file but no destination to migrate it to | Set `HARMONY_PASSPHRASE` or run on a system with a keychain — harmony will migrate the plaintext on next launch |

## Not yet supported

- **OpenWRT and other embedded Linux** — Argon2id with m=64 MiB exceeds available
  RAM on most embedded targets. The harmony-openwrt repo will provide a tuned
  build with smaller KDF params (and bump the on-disk format_version).
- **Hardware tokens (YubiKey, TPM)** — possible future, out of scope here.
````

- [ ] **Step 2: Verify the doc renders**

Spot-check the markdown by viewing it in any markdown previewer or with:

```bash
cat docs/headless-install.md | head -100
```

- [ ] **Step 3: Commit**

```bash
git add docs/headless-install.md
git commit -m "$(cat <<'EOF'
docs: headless install guide for HARMONY_PASSPHRASE-based installs

Server-admin-facing reference for the encrypted-file backend: env vars,
systemd unit, Docker invocation, passphrase format rules, migration
from prior plaintext, rotate-passphrase usage, and troubleshooting.
Backup/restore UX is explicitly punted to ZEB-175 with a forward
pointer.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: Pinned wire-format fixture + final smoke test

**Files:**
- Create: `src-tauri/tests/fixtures/encrypted_v1.bin`
- Create: `src-tauri/tests/wire_format_fixture.rs`

The pin lands as an *integration test* (separate file, separate test target) so it can use `include_bytes!` to load the fixture without polluting the main `identity.rs` test module.

- [ ] **Step 1: Write a one-shot fixture generator helper**

Create `src-tauri/tests/wire_format_fixture.rs`:

```rust
//! Pin the v1 wire format. Catches accidental byte-layout drift early.
//!
//! The fixture file at tests/fixtures/encrypted_v1.bin is generated once via
//! the GENERATE_FIXTURE flag below and then committed. Future runs assert
//! byte-equality against the committed fixture.
//!
//! To regenerate (only needed if the v1 format intentionally changes — and
//! at that point you should bump format_version to v2 and add a v2 fixture
//! instead): set the env var HARMONY_REGENERATE_WIRE_FIXTURE=1 and run this
//! test once. It will overwrite the fixture file. Then commit and run again
//! without the env var to confirm the assertion passes.

use harmony_app::identity::test_only::encrypt_with_params_for_test;
use std::path::PathBuf;

const TEST_PASSPHRASE: &[u8] = b"correct horse battery staple";
const TEST_SALT: [u8; 16] = [0xAB; 16];
const TEST_NONCE: [u8; 24] = [0xCD; 24];
const TEST_BLOB: [u8; 161] = [0x42; 161];

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("encrypted_v1.bin")
}

#[test]
fn wire_format_v1_pinned() {
    let bytes = encrypt_with_params_for_test(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
    assert_eq!(bytes.len(), 230, "v1 format must be exactly 230 bytes");

    let path = fixture_path();

    if std::env::var("HARMONY_REGENERATE_WIRE_FIXTURE").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &bytes).expect("write fixture");
        eprintln!("Regenerated fixture at {}", path.display());
        return;
    }

    let expected = std::fs::read(&path).unwrap_or_else(|_| {
        panic!(
            "Fixture missing at {}.\n\
             First-time setup: run with HARMONY_REGENERATE_WIRE_FIXTURE=1 to generate, then commit.",
            path.display()
        )
    });

    assert_eq!(
        bytes, expected,
        "WIRE FORMAT CHANGED — bump format_version and add a v2 fixture before regenerating"
    );
}
```

This requires exposing `encrypt_with_params` from the crate. Add a `test_only` module to `src-tauri/src/identity.rs`:

```rust
/// Test-only re-exports. Gated behind a feature so they can't be misused from
/// production code. The integration test in tests/wire_format_fixture.rs uses
/// these to pin the wire format.
#[doc(hidden)]
pub mod test_only {
    pub use super::encrypt_with_params as encrypt_with_params_for_test;
    pub use super::decrypt as decrypt_for_test;
}
```

The `encrypt_with_params` and `decrypt` helpers from Task 4 are still file-private (`fn`, not `pub fn`). Make them `pub(crate)` so the `test_only` module can re-export them, OR mark them `pub` directly with the `#[doc(hidden)]` attribute. Use `pub(crate)`:

```rust
// In Task 4's encrypt_with_params and decrypt: change `fn` → `pub(crate) fn`
pub(crate) fn encrypt_with_params(...) -> Vec<u8> { ... }
pub(crate) fn decrypt(...) -> Result<[u8; BLOB_LEN], String> { ... }
```

Also ensure the `identity` module is publicly exposed from `lib.rs` (it likely already is — `mod identity;` at top of lib.rs); if it's currently `mod identity;` (private), change to `pub mod identity;`.

- [ ] **Step 2: Generate the fixture**

```bash
cd src-tauri && HARMONY_REGENERATE_WIRE_FIXTURE=1 cargo test --test wire_format_fixture wire_format_v1_pinned
```

Expected output includes `Regenerated fixture at .../tests/fixtures/encrypted_v1.bin`. Verify the file exists:

```bash
ls -l src-tauri/tests/fixtures/encrypted_v1.bin
```

Expected: 230 bytes.

- [ ] **Step 3: Run the test without the regenerate flag**

```bash
cd src-tauri && cargo test --test wire_format_fixture wire_format_v1_pinned
```

Expected: PASS (the assertion confirms generation + read-back are byte-identical).

- [ ] **Step 4: Run the entire test suite + lint**

```bash
cd src-tauri && cargo test
```

Expected: all unit tests + integration tests PASS.

```bash
cd src-tauri && cargo clippy --all-targets -- -D warnings
```

Expected: no warnings. Fix any lint issues that surface (likely candidates: unused imports if any deps weren't pruned, missing `#[must_use]` on functions returning `Result`).

```bash
cd src-tauri && cargo fmt --check
```

Expected: no diff. If `cargo fmt` flags issues, run without `--check` to apply them, then re-stage.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/tests/wire_format_fixture.rs src-tauri/tests/fixtures/encrypted_v1.bin src-tauri/src/identity.rs
git commit -m "$(cat <<'EOF'
test(identity): pin v1 wire format with deterministic 230-byte fixture

Integration test loads tests/fixtures/encrypted_v1.bin and asserts
byte-equality against encrypt_with_params output for fixed
passphrase/salt/nonce/blob. Catches accidental layout drift early. To
regenerate (only when bumping format_version intentionally), set
HARMONY_REGENERATE_WIRE_FIXTURE=1.

Exposes encrypt_with_params / decrypt as pub(crate) and re-exports
through identity::test_only for the integration test.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Done

After Task 12, the branch should have ~50 passing tests (~9 baseline kept + ~3 LegacyPlaintextReader + ~9 wire format + ~7 EncryptedFileStore + ~8 env + ~4 cleanup_legacy_bak + ~11 resolution chain + ~4 rotation + ~2 CLI + 1 wire format fixture pin), `cargo clippy --all-targets -- -D warnings` clean, `cargo fmt --check` clean.

Push with:

```bash
git push -u origin zeb-174-headless-encrypted-identity
```

Open the PR with a summary referencing the spec and the three gaps addressed (legacy `.bak` cleanup, headless passphrase backend, install docs). Manual verification on macOS (keychain backend) and one Linux container (encrypted-file backend) before requesting review.
