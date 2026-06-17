# ZEB-492 — Distribute the fleet KeyTree at pairing — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give a cert-only enrolled device the owner's fleet `KeyTree` (the 5 derived AEAD/HMAC keys), sealed during SAS pairing, so it can build the fleet engines and act as a butler — without ever receiving the master seed.

**Architecture:** A new `FleetKeyMaterial` type is the single serialization surface for KeyTree key bytes. The inviter (always a seed-holder) derives the KeyTree and seals its material into the existing SAS-encrypted ENROLL payload. The joiner persists it (variable-length slot in the consolidated keychain vault, or a `fleet_keytree.enc` file under `HARMONY_PASSPHRASE`). On boot, the `start_node` engine gate obtains the KeyTree from the seed (minting device) *or* the persisted material (cert-only device), then builds the identical engine set.

**Tech stack:** Rust, Tauri, `ciborium` (CBOR), `zeroize`, XChaCha20-Poly1305 / Argon2id (existing identity-vault crypto), `cargo nextest`.

**Spec:** `docs/specs/2026-06-17-zeb-492-distribute-fleet-keytree-design.md`

**Gates (run from `src-tauri/`):**
- `cargo fmt --all -- --check`
- `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- `cargo nextest run --locked -p harmony-app --features test-fixtures`

**Conventions that bite (from CLAUDE.md):** always `--locked` + `--all-targets` + `--features test-fixtures`. Tests touching identity persistence MUST inject `keychain: None` via `*_inner` seams and set `HARMONY_PASSPHRASE` for the encrypted-file fallback — never construct `KeychainStore::new()` in test-reachable code (ZEB-428). `KeychainStore::new()` already refuses in test builds.

---

## File Structure

- `src-tauri/src/owner_state_crypto.rs` — **Task 1.** Add `FleetKeyMaterial` + `KeyTree::to_fleet_material`/`from_fleet_material` + unit tests. Pure crypto, no I/O.
- `src-tauri/src/identity.rs` — **Task 2.** Add `SecretVault.fleet_keytree` field + accessors + `vault_{load,save,clear}_fleet_keytree` (variable-length, reuse `encrypt_vault`/`decrypt_v2_plaintext`).
- `src-tauri/src/owner_state.rs` — **Task 2.** Add `save_fleet_keytree`/`load_fleet_keytree` orchestration; add `LoadedOwnerState.fleet_keytree` field; load it in `load_owner_state`.
- `src-tauri/src/lib.rs` — **Task 3.** Surgical boot-gate refactor at ~3726: compute `fleet_kt` before the gate, swap the inner `if let Some(seed)` condition to `if let Some(kt)`.
- `src-tauri/src/pairing/types.rs` — **Task 4.** Add `fleet_keytree_cbor_hex: Option<String>` to `EncryptedPayload::Enroll`.
- `src-tauri/src/pairing/state_machine.rs` — **Task 4.** `JoinerEnrollResult.fleet_keytree` field; inviter derives + populates; joiner extracts.
- `src-tauri/src/pairing/persist.rs` — **Task 4.** `install_joiner_state_inner` persists the material; integration test.
- `e2e-harness/` — **Task 5.** Upgrade `s7_butler_deposit_recover` from characterize-at-boundary-0b to a full HELD→RECV→CLEARED assert.

---

## Task 1: `FleetKeyMaterial` + KeyTree export/import

**Files:**
- Modify: `src-tauri/src/owner_state_crypto.rs` (add type + 2 methods near the `KeyTree` impl, ~line 170; tests in the existing `#[cfg(test)] mod tests`)

Context: `KeyTree` (`owner_state_crypto.rs:120`) has 5 private `Zeroizing<[u8; 32]>` fields — `entry_aead`, `root_aead`, `lookup`, `nonce`, `friend_aead` — derived by `KeyTree::derive(&[u8;32])` via HKDF-SHA256. All consumers go through `space_lookup_key`, `encrypt_entry`, `decrypt_entry`, `encrypt_root_publish`, `decrypt_root_publish`, `encrypt_friend_secret`, `decrypt_friend_secret`. We add a serializable export mirroring the `SecretVault` pattern (plain `[u8;32]` serde fields + struct-level `ZeroizeOnDrop`, no `Debug`).

- [ ] **Step 1: Write the failing test** — append to `mod tests` in `owner_state_crypto.rs`:

```rust
#[test]
fn fleet_material_round_trips_to_identical_keytree() {
    let seed = [0x42u8; 32];
    let kt = KeyTree::derive(&seed).unwrap();
    let material = kt.to_fleet_material();
    assert_eq!(material.epoch, 0);
    let kt2 = KeyTree::from_fleet_material(&material).unwrap();

    // Same lookup key for a given space tag.
    let lk1 = space_lookup_key(&kt, b"notes-v1");
    let lk2 = space_lookup_key(&kt2, b"notes-v1");
    assert_eq!(lk1.as_slice(), lk2.as_slice());

    // An entry encrypted under kt decrypts under kt2 (and vice versa).
    let lk = space_lookup_key(&kt, b"dm-inbox-v1");
    let ct = encrypt_entry(&kt, &lk, b"hello fleet").unwrap();
    let pt = decrypt_entry(&kt2, &lk, &ct).unwrap();
    assert_eq!(pt, b"hello fleet");

    // friend_aead also carries: a friend secret sealed under kt opens under kt2.
    let fid = [9u8; 16];
    let secret = [3u8; 32];
    let sealed = encrypt_friend_secret(&kt, &fid, &secret).unwrap();
    let opened = decrypt_friend_secret(&kt2, &fid, &sealed).unwrap();
    assert_eq!(opened.as_slice(), &secret);
}

#[test]
fn fleet_material_cbor_round_trips() {
    let kt = KeyTree::derive(&[7u8; 32]).unwrap();
    let m = kt.to_fleet_material();
    let mut buf = Vec::new();
    ciborium::into_writer(&m, &mut buf).unwrap();
    let back: FleetKeyMaterial = ciborium::from_reader(buf.as_slice()).unwrap();
    let kt_back = KeyTree::from_fleet_material(&back).unwrap();
    let lk = space_lookup_key(&kt, b"x");
    let ct = encrypt_entry(&kt, &lk, b"z").unwrap();
    assert_eq!(decrypt_entry(&kt_back, &lk, &ct).unwrap(), b"z");
}

#[test]
fn fleet_material_from_different_seed_cannot_decrypt() {
    let kt_a = KeyTree::derive(&[1u8; 32]).unwrap();
    let kt_b = KeyTree::derive(&[2u8; 32]).unwrap();
    let lk = space_lookup_key(&kt_a, b"notes-v1");
    let ct = encrypt_entry(&kt_a, &lk, b"secret").unwrap();
    let kt_b2 = KeyTree::from_fleet_material(&kt_b.to_fleet_material()).unwrap();
    let lk_b = space_lookup_key(&kt_b2, b"notes-v1");
    assert!(decrypt_entry(&kt_b2, &lk_b, &ct).is_err());
}

#[test]
fn fleet_material_unsupported_epoch_rejected() {
    // Only epoch 0 is supported today (KeyTree rotation is out of scope). A
    // future / corrupt epoch must be REJECTED at import, not silently rebuilt.
    // `to_fleet_material` only ever stamps epoch 0, so forge a non-zero epoch by
    // overriding the field on exported material.
    let kt = KeyTree::derive(&[0x42u8; 32]).unwrap();
    let mut material = kt.to_fleet_material();
    material.epoch = 1;
    match KeyTree::from_fleet_material(&material) {
        Err(CryptoError::UnsupportedEpoch(1)) => {}
        Err(other) => panic!("expected UnsupportedEpoch(1), got: {other}"),
        Ok(_) => panic!("epoch 1 material must be rejected, not reconstructed"),
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(fleet_material)'`
Expected: compile error — `FleetKeyMaterial` / `to_fleet_material` / `from_fleet_material` not found.

- [ ] **Step 3: Implement** — add after the `impl KeyTree { ... }` block (after `derive`, ~line 170). Note: `ciborium`, `serde`, `zeroize` are already crate deps.

```rust
/// Serializable export of a `KeyTree`'s raw key material, for sealed
/// distribution to cert-only enrolled devices (ZEB-492). Carries an explicit
/// `epoch` so a future KeyTree rotation is non-breaking (rotation itself is
/// out of scope — see the ZEB-492 spec).
///
/// SECURITY: this is the ONLY place `KeyTree` key bytes leave the type. No
/// `Debug` (would print key material). `ZeroizeOnDrop` wipes the key fields on
/// drop. Only ever moved through the SAS-sealed pairing channel and the
/// encrypted vault. Mirrors the `SecretVault` pattern (plain `[u8;32]` serde
/// fields + struct-level zeroize).
#[derive(serde::Serialize, serde::Deserialize, zeroize::ZeroizeOnDrop)]
pub struct FleetKeyMaterial {
    #[zeroize(skip)]
    pub epoch: u32,
    entry_aead: [u8; 32],
    root_aead: [u8; 32],
    lookup: [u8; 32],
    nonce: [u8; 32],
    friend_aead: [u8; 32],
}

impl KeyTree {
    /// Export this KeyTree's key material for sealed distribution to an
    /// enrolled device. Only the seed-holding (inviter) device calls this.
    /// Takes no `epoch` param: it stamps `epoch: 0` internally (the only epoch
    /// the current derivation produces, and the only one `from_fleet_material`
    /// accepts). A future KeyTree rotation will revisit this.
    pub fn to_fleet_material(&self) -> FleetKeyMaterial {
        FleetKeyMaterial {
            epoch: 0,
            entry_aead: *self.entry_aead,
            root_aead: *self.root_aead,
            lookup: *self.lookup,
            nonce: *self.nonce,
            friend_aead: *self.friend_aead,
        }
    }

    /// Reconstruct a KeyTree from distributed material (cert-only device, which
    /// has no master seed to re-derive from). Produces a KeyTree byte-identical
    /// to the originating seed-holder's.
    ///
    /// Fallible: rejects an unsupported `epoch` (anything `!= 0` today) with
    /// `CryptoError::UnsupportedEpoch` so corrupt/future-version material can't
    /// be silently accepted at the cert-only boot boundary.
    pub fn from_fleet_material(m: &FleetKeyMaterial) -> Result<Self, CryptoError> {
        if m.epoch != 0 {
            return Err(CryptoError::UnsupportedEpoch(m.epoch));
        }
        Ok(Self {
            entry_aead: Zeroizing::new(m.entry_aead),
            root_aead: Zeroizing::new(m.root_aead),
            lookup: Zeroizing::new(m.lookup),
            nonce: Zeroizing::new(m.nonce),
            friend_aead: Zeroizing::new(m.friend_aead),
        })
    }
}
```

Note: `*self.entry_aead` derefs `Zeroizing<[u8;32]>` to `[u8;32]` and copies. If the deref-copy trips a clippy lint, use `**` / `<[u8;32]>::from(...)` as needed — the intent is a plain-array copy of each sub-key.

- [ ] **Step 4: Run to verify pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(fleet_material)'`
Expected: 3 tests pass.

- [ ] **Step 5: Lint + format + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
git add src/owner_state_crypto.rs
git commit -m "feat(zeb-492): FleetKeyMaterial + KeyTree export/import

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: Persistence — vault field + variable-length accessors + owner_state orchestration + LoadedOwnerState

**Files:**
- Modify: `src-tauri/src/identity.rs` (`SecretVault` struct ~423, accessors ~457-472, vault fns ~1549-1660)
- Modify: `src-tauri/src/owner_state.rs` (`LoadedOwnerState` ~369, `load_owner_state` ~386, new `save_fleet_keytree`/`load_fleet_keytree` near `load_secret`/`save_secret` ~764)

Context: secrets persist two ways. **With keychain:** one consolidated `SecretVault` (CBOR, variable-length `v0x02` envelope) via `vault_load_slot`/`vault_save_slot`. **Without keychain (`HARMONY_PASSPHRASE`):** per-secret 32-byte v0x01 files via `EncryptedFileStore`. The existing slot API is `[u8;32]`-only; the fleet KeyTree material (~161-byte CBOR) needs variable-length siblings. `encrypt_vault(passphrase, &[u8]) -> Vec<u8>` (`identity.rs:1136`) and `decrypt_v2_plaintext(passphrase, &[u8]) -> Result<Zeroizing<Vec<u8>>>` (`identity.rs:1221`) are already generic variable-length envelope helpers — reuse them for the file fallback.

### 2a. `SecretVault.fleet_keytree` field + accessors (`identity.rs`)

- [ ] **Step 1: Write the failing test** — extend `vault_cbor_round_trips` (or add a sibling) in `mod vault_tests` (~498):

```rust
#[test]
fn vault_carries_fleet_keytree() {
    let mut v = SecretVault::from_seed([7u8; BLOB_LEN]);
    assert!(v.fleet_keytree().is_none());
    v.set_fleet_keytree(Some(vec![1, 2, 3, 4, 5]));
    let cbor = v.to_cbor().expect("encode");
    let back = SecretVault::from_cbor(&cbor).expect("decode");
    assert_eq!(back.fleet_keytree(), Some(&[1, 2, 3, 4, 5][..]));
}

#[test]
fn vault_without_fleet_keytree_decodes_to_none() {
    // A vault CBOR-encoded before the field existed must decode with None
    // (serde default), not error.
    let v = SecretVault::from_seed([1u8; BLOB_LEN]);
    let cbor = v.to_cbor().unwrap();
    let back = SecretVault::from_cbor(&cbor).unwrap();
    assert!(back.fleet_keytree().is_none());
}
```

- [ ] **Step 2: Run to verify fail**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(fleet_keytree)'`
Expected: compile error — `fleet_keytree`/`set_fleet_keytree` not found.

- [ ] **Step 3: Implement** — in `SecretVault` (`identity.rs:423`) add the field after `owner_master_seed`:

```rust
    /// Distributed fleet KeyTree material (CBOR of `FleetKeyMaterial`) for a
    /// cert-only enrolled device (ZEB-492). `None` on the minting device (it
    /// re-derives the KeyTree from `owner_master_seed`) and on devices paired
    /// before ZEB-492. Variable-length, so it is NOT a 32-byte `VaultSlot`.
    #[serde(default)]
    fleet_keytree: Option<Vec<u8>>,
```

Add `fleet_keytree: None` to the `from_seed` constructor (`identity.rs:443`). Add accessors in `impl SecretVault`:

```rust
    fn fleet_keytree(&self) -> Option<&[u8]> {
        self.fleet_keytree.as_deref()
    }

    fn set_fleet_keytree(&mut self, material: Option<Vec<u8>>) {
        self.fleet_keytree = material;
    }
```

Do NOT bump `VAULT_VERSION` — `#[serde(default)]` makes absence decode to `None`, so old vaults stay readable. Update any other `SecretVault { .. }` literal in the file (search `SecretVault {`) to include `fleet_keytree: None` — e.g. the `vault_cbor_round_trips` test literal at ~503.

- [ ] **Step 4: Run to verify pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(fleet_keytree) + test(vault_cbor)'`
Expected: pass.

### 2b. Variable-length vault accessors (`identity.rs`)

Context: read `vault_load_slot` (~1549), `vault_save_slot` (~1601), `vault_clear_slot` (~1628) and their `_with_store` internals. They resolve the keychain-backed `SecretVault`, read/modify/write it, and return. Mirror them for the variable-length field — but the fleet KeyTree has **no legacy keychain item**, so omit the `legacy: &keyring::Entry` migration parameter and any legacy-fold logic.

- [ ] **Step 1: Implement** — add next to the slot fns:

```rust
/// Load the distributed fleet KeyTree material from the consolidated keychain
/// vault. `Ok(None)` when the vault has no such item. Mirrors
/// `vault_load_slot` but variable-length and with no legacy-item migration
/// (the fleet KeyTree slot is new in ZEB-492).
pub fn vault_load_fleet_keytree() -> Result<Option<Zeroizing<Vec<u8>>>, String> {
    // Use the same store resolution as vault_load_slot_with_store, then:
    //   Ok(vault.fleet_keytree().map(|b| Zeroizing::new(b.to_vec())))
    // (see implementation note below)
}

/// Persist the fleet KeyTree material into the consolidated keychain vault.
/// `Ok(true)` when written to the vault, `Ok(false)` when there is no vault
/// item (keychain-less — caller falls back to the encrypted file). Mirrors
/// `vault_save_slot` (read-modify-write, preserving other slots).
pub fn vault_save_fleet_keytree(material: &[u8]) -> Result<bool, String> { /* ... */ }

/// Clear the fleet KeyTree material from the vault (best-effort; mirrors
/// `vault_clear_slot`). Used when a joiner is re-bound without fleet material.
pub fn vault_clear_fleet_keytree() -> Result<(), String> { /* ... */ }
```

Implementation note: factor exactly like the `_with_store` trio. The load reads the vault and returns `.fleet_keytree()`; save does read-modify-write calling `set_fleet_keytree(Some(material.to_vec()))` then re-encrypts via the SAME path `vault_save_slot_with_store` uses; clear calls `set_fleet_keytree(None)`. If the existing slot fns share a private read-vault/write-vault helper, reuse it; otherwise replicate its body. Keep the `Ok(false) = no vault item` contract identical to `vault_save_slot`.

- [ ] **Step 2: Unit test** — add to `mod vault_tests` using the same in-memory/mock store harness the existing `vault_save_slot`/`vault_load_slot` tests use (see `save_slot_writes_to_vault_or_reports_no_item` ~1035 and `load_slot_returns_vault_value_and_migrates_legacy` ~997 for the exact store-injection pattern):

```rust
#[test]
fn fleet_keytree_vault_round_trips_via_store() {
    // Mirror the store setup from save_slot_writes_to_vault_or_reports_no_item.
    // 1. save material -> Ok(true)
    // 2. load -> Some(material)
    // 3. clear -> load returns None
}
```

(Use the same `*_with_store` test seam as the neighbouring slot tests; do not call the keychain.)

- [ ] **Step 3: Run + verify pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(fleet_keytree)'`

### 2c. `owner_state` orchestration + `LoadedOwnerState` (`owner_state.rs`)

Context: `load_secret`/`save_secret` (`owner_state.rs:764`/`820`) orchestrate keychain-vault-preferred + encrypted-file fallback for 32-byte secrets. Mirror them for variable-length material. `EncryptedFileStore` is 32-byte-only, so the file fallback uses `encrypt_vault`/`decrypt_v2_plaintext` directly with the `HARMONY_PASSPHRASE`. Reuse the existing passphrase resolver that `EncryptedFileStore::from_env` uses (find it — e.g. a `read_passphrase_env()` helper; if `from_env` resolves the passphrase internally, extract/reuse that resolution so behavior matches: `HARMONY_PASSPHRASE` or `HARMONY_PASSPHRASE_FILE`).

- [ ] **Step 1: Write the failing test** — add to `owner_state.rs` `#[cfg(test)] mod tests`. Follow the existing `home_override`/`HARMONY_PASSPHRASE` pattern used by `save_owner_state_atomic` tests (keychain `None`):

```rust
#[test]
fn fleet_keytree_save_load_round_trips_via_encrypted_file() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("HARMONY_PASSPHRASE", "test-pass-zeb492");
    let material = vec![0xABu8; 161]; // ~ CBOR of FleetKeyMaterial size
    save_fleet_keytree(&None, dir.path(), &material).expect("save");
    let loaded = load_fleet_keytree(&None, dir.path()).expect("load");
    assert_eq!(loaded.as_deref(), Some(&material[..]));
    std::env::remove_var("HARMONY_PASSPHRASE");
}

#[test]
fn fleet_keytree_absent_loads_none() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("HARMONY_PASSPHRASE", "test-pass-zeb492b");
    let loaded = load_fleet_keytree(&None, dir.path()).expect("load");
    assert!(loaded.is_none());
    std::env::remove_var("HARMONY_PASSPHRASE");
}
```

(If the suite runs tests in parallel and `set_var` races, gate these behind the same serialization the existing passphrase tests use — match the neighbouring test's approach, e.g. a shared `home_override`/env mutex.)

- [ ] **Step 2: Run to verify fail**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(fleet_keytree_save_load) + test(fleet_keytree_absent)'`
Expected: compile error — `save_fleet_keytree`/`load_fleet_keytree` not found.

- [ ] **Step 3: Implement** — add to `owner_state.rs` near `load_secret`/`save_secret`. The filename constant: `const FLEET_KEYTREE_FILENAME: &str = "fleet_keytree.enc";`

```rust
/// Persist the fleet KeyTree material (CBOR of `FleetKeyMaterial`): keychain
/// vault first, encrypted-file fallback (`HARMONY_PASSPHRASE`) otherwise.
/// Variable-length sibling of `save_secret` (ZEB-492).
fn save_fleet_keytree(
    keychain: &Option<KeychainStore>,
    identity_dir: &Path,
    material: &[u8],
) -> Result<(), String> {
    if keychain.is_some() {
        match crate::identity::vault_save_fleet_keytree(material) {
            Ok(true) => return Ok(()),
            Ok(false) => {} // no vault item → fall through to file
            Err(e) => {
                tracing::warn!("vault fleet-keytree write: {e}; falling through to file");
            }
        }
    }
    // Encrypted-file fallback: reuse the v0x02 variable-length envelope.
    let passphrase = crate::identity::resolve_passphrase_env()
        .map_err(|e| format!("fleet-keytree fallback: {e}"))?
        .ok_or_else(|| "HARMONY_PASSPHRASE not set; cannot encrypt fleet_keytree.enc".to_string())?;
    let blob = crate::identity::encrypt_vault_bytes(&passphrase, material);
    let path = identity_dir.join(FLEET_KEYTREE_FILENAME);
    write_atomic_0600(&path, &blob).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

/// Load the fleet KeyTree material. `Ok(None)` if nowhere present.
fn load_fleet_keytree(
    keychain: &Option<KeychainStore>,
    identity_dir: &Path,
) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
    if keychain.is_some() {
        match crate::identity::vault_load_fleet_keytree() {
            Ok(Some(v)) => return Ok(Some(v)),
            Ok(None) => {}
            Err(e) => tracing::warn!("vault fleet-keytree read: {e}; trying file fallback"),
        }
    }
    let path = identity_dir.join(FLEET_KEYTREE_FILENAME);
    if !path.exists() {
        return Ok(None);
    }
    let passphrase = crate::identity::resolve_passphrase_env()
        .map_err(|e| format!("fleet-keytree fallback: {e}"))?
        .ok_or_else(|| "fleet_keytree.enc present but HARMONY_PASSPHRASE not set".to_string())?;
    let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let plaintext = crate::identity::decrypt_vault_bytes(&passphrase, &bytes)
        .map_err(|e| format!("decrypt {}: {e}", path.display()))?;
    Ok(Some(plaintext))
}
```

This requires three small `pub(crate)` exposures in `identity.rs` (the helpers are currently private):
- `resolve_passphrase_env() -> Result<Option<Zeroizing<Vec<u8>>>, String>` — extract from / mirror whatever `EncryptedFileStore::from_env` uses to read `HARMONY_PASSPHRASE`/`HARMONY_PASSPHRASE_FILE`. If such a function already exists privately, just make it `pub(crate)`.
- `encrypt_vault_bytes(passphrase: &[u8], plaintext: &[u8]) -> Vec<u8>` — thin `pub(crate)` wrapper over the existing `encrypt_vault`.
- `decrypt_vault_bytes(passphrase: &[u8], bytes: &[u8]) -> Result<Zeroizing<Vec<u8>>, String>` — thin `pub(crate)` wrapper over the existing `decrypt_v2_plaintext`.

(Reuse `write_atomic_0600` — already used in `save_owner_state_atomic`. Confirm its path/visibility in `owner_state.rs`.)

- [ ] **Step 4: Add `LoadedOwnerState.fleet_keytree` + load it.** In `owner_state.rs:369` add to `LoadedOwnerState`:

```rust
    /// Distributed fleet KeyTree (ZEB-492). `Some` on a cert-only enrolled
    /// device that was given the KeyTree at pairing; `None` on the minting
    /// device (derives from `master_seed`) and pre-ZEB-492 paired devices.
    pub fleet_keytree: Option<crate::owner_state_crypto::FleetKeyMaterial>,
```

In `load_owner_state` (~386), after loading `master_seed`, load + decode the material:

```rust
    let fleet_keytree = match load_fleet_keytree(&keychain, identity_dir)? {
        Some(bytes) => Some(
            ciborium::from_reader::<crate::owner_state_crypto::FleetKeyMaterial, _>(bytes.as_slice())
                .map_err(|e| format!("decode fleet_keytree: {e}"))?,
        ),
        None => None,
    };
```

and add `fleet_keytree` to the returned `LoadedOwnerState { ... }`. Update any other `LoadedOwnerState { .. }` literal (search the file/tests) to include `fleet_keytree: None`.

- [ ] **Step 5: Run to verify pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(fleet_keytree)'`
Expected: pass.

- [ ] **Step 6: Lint + format + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
git add src/identity.rs src/owner_state.rs
git commit -m "feat(zeb-492): persist distributed fleet KeyTree (vault field + var-length file fallback)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Boot-gate refactor — obtain KeyTree from seed OR material

**Files:**
- Modify: `src-tauri/src/lib.rs` (the engine gate, ~3726-3734)

Context: the engine block is `let sync_engine_arc = if let Some(ref loaded) = owner_loaded { if let Some(seed) = loaded.master_seed.as_ref() { let kt = Arc::new(KeyTree::derive(seed)?); <~3900-line body uses kt + loaded> } else { None } } else { None };`. Verified facts: `seed` is used ONLY at the `KeyTree::derive` line; the rest of the body uses `kt` and `loaded`; no seed-only capability (minting w/ master key, cert signing, recovery) lives in this block. So the refactor is surgical: compute `kt` before the gate, swap the inner condition. **Do NOT restructure the body.**

- [ ] **Step 1: Read the exact gate.** Open `src-tauri/src/lib.rs` and locate the `let sync_engine_arc ... = if let Some(ref loaded) = owner_loaded {` (~3727) and the immediately-following `if let Some(seed) = loaded.master_seed.as_ref() {` (3730) and the `let kt = std::sync::Arc::new(crate::owner_state_crypto::KeyTree::derive(seed) ...)?;` (3731-3734). Confirm the body's first real statement after the `kt` derivation (currently the `let device_id = ...` at ~3735).

- [ ] **Step 2: Implement.** Immediately BEFORE `let sync_engine_arc ... = if let Some(ref loaded) = owner_loaded {`, insert:

```rust
        // ZEB-492: obtain the fleet KeyTree from EITHER the master seed (minting
        // device — authoritative) OR the distributed material persisted at
        // pairing (cert-only enrolled device). Neither → no fleet engines
        // (graceful fallback: pre-ZEB-492 paired devices, or material delivery
        // failed). No seed-only capability lives in the engine block, so a
        // cert-only device building engines here cannot mint/sign-certs/recover.
        let fleet_kt: Option<std::sync::Arc<crate::owner_state_crypto::KeyTree>> =
            match owner_loaded.as_ref() {
                Some(loaded) => {
                    if let Some(seed) = loaded.master_seed.as_ref() {
                        Some(std::sync::Arc::new(
                            crate::owner_state_crypto::KeyTree::derive(seed)
                                .map_err(|e| format!("KeyTree::derive: {e}"))?,
                        ))
                    } else if let Some(material) = loaded.fleet_keytree.as_ref() {
                        // `from_fleet_material` is fallible: a bad / unsupported-epoch
                        // blob degrades to `None` (warn + boot with no fleet engines)
                        // rather than hard-failing the whole boot.
                        match crate::owner_state_crypto::KeyTree::from_fleet_material(material) {
                            Ok(kt) => Some(std::sync::Arc::new(kt)),
                            Err(e) => {
                                tracing::warn!(
                                    "ignoring unusable fleet material ({e}); no fleet engines"
                                );
                                None
                            }
                        }
                    } else {
                        None
                    }
                }
                None => None,
            };
```

Then change the inner gate from:

```rust
                if let Some(seed) = loaded.master_seed.as_ref() {
                    let kt = std::sync::Arc::new(
                        crate::owner_state_crypto::KeyTree::derive(seed)
                            .map_err(|e| format!("KeyTree::derive: {e}"))?,
                    );
```

to:

```rust
                if let Some(kt) = fleet_kt.clone() {
```

i.e. **delete** the `let kt = Arc::new(KeyTree::derive(seed)?);` statement (now computed above) and replace the `if let Some(seed) = loaded.master_seed.as_ref()` condition with `if let Some(kt) = fleet_kt.clone()`. The body (the `let device_id = ...` onward) is unchanged — it already binds `kt` and `loaded`.

Watch-outs:
- The body references `loaded` (still bound by the outer `if let Some(ref loaded)`). Keep the outer gate intact.
- If `seed` is referenced anywhere else in the body, the compiler will flag it — per the verified facts it is not, but if it surfaces, STOP and report (it would contradict the design).
- `fleet_kt.clone()` clones the `Arc` (cheap). The pre-gate `fleet_kt` binding still owns one ref; that's fine.

- [ ] **Step 3: Verify it compiles + existing tests pass.**

Run: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Then: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures`
Expected: clean clippy; all existing tests pass (no behavior change for seed-holders — `fleet_kt` is `Some` exactly when the old gate was, plus the new cert-only branch which no existing test exercises).

- [ ] **Step 4: Commit**

```bash
cd src-tauri && cargo fmt --all
git add src/lib.rs
git commit -m "feat(zeb-492): boot gate obtains KeyTree from seed or distributed material

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: Pairing — seal the KeyTree to the joiner, persist on install

**Files:**
- Modify: `src-tauri/src/pairing/types.rs` (`EncryptedPayload::Enroll` ~115)
- Modify: `src-tauri/src/pairing/state_machine.rs` (`JoinerEnrollResult` ~54; inviter build ~784; joiner extract ~1134/1247)
- Modify: `src-tauri/src/pairing/persist.rs` (`install_joiner_state_inner` ~38; integration test)

Context: the inviter signs the cert (`state_machine.rs:747`, `master_seed` in scope), CBOR-encodes the cert + owner_state, and builds `EncryptedPayload::Enroll { enrollment_cert_cbor_hex, owner_state_cbor_hex, joiner_advisory_display_name }` (~784), then seals it under the SAS session key. The joiner decrypts in the `EncryptedPayload::Enroll { .. }` arm (~1134), hex+CBOR-decodes the cert and state, and builds `JoinerEnrollResult { our_signing_key, owner_state, our_device_id }` (~1247), sent to the persistence channel.

### 4a. Wire field through the payload + result types

- [ ] **Step 1: Add the wire field.** In `pairing/types.rs`, add to `EncryptedPayload::Enroll` (after `joiner_advisory_display_name`):

```rust
        #[serde(default)]
        fleet_keytree_cbor_hex: Option<String>,
```

(`#[serde(default)]` keeps it optional for forward/backward compat. The enum already has `rename_all_fields = "camelCase"`.)

- [ ] **Step 2: Add the result field.** In `state_machine.rs`, add to `JoinerEnrollResult` (~54):

```rust
    /// Distributed fleet KeyTree material (ZEB-492), present when the inviter
    /// sealed it into the ENROLL payload. Persisted by `install_joiner_state`.
    pub fleet_keytree: Option<crate::owner_state_crypto::FleetKeyMaterial>,
```

### 4b. Inviter populates

- [ ] **Step 3: Inviter derives + seals.** In `state_machine.rs`, in the function that builds the `EncryptedPayload::Enroll` (~784) where `master_seed` is in scope (it was used at `sign_enrollment_for_joiner` ~747): after `state_cbor` is built, derive + encode the material:

```rust
    // ZEB-492: seal the fleet KeyTree to the enrolled (cert-only) device so it
    // can build fleet engines + act as a butler. master_seed is in scope here
    // (cert signing above requires it). Rides the same SAS-sealed payload.
    let fleet_keytree_cbor_hex = match crate::owner_state_crypto::KeyTree::derive(master_seed) {
        Ok(kt) => {
            let mut buf = Vec::new();
            match ciborium::into_writer(&kt.to_fleet_material(), &mut buf) {
                Ok(()) => Some(hex::encode(&buf)),
                Err(e) => {
                    let _ = state_tx.send(PairingState::Failed {
                        reason: format!("encode fleet keytree: {e}"),
                    });
                    return;
                }
            }
        }
        Err(e) => {
            let _ = state_tx.send(PairingState::Failed {
                reason: format!("derive fleet keytree: {e}"),
            });
            return;
        }
    };
```

Then add `fleet_keytree_cbor_hex,` to the `EncryptedPayload::Enroll { ... }` literal (~784).

Note: `master_seed` here is whatever type the function holds (e.g. `&Zeroizing<[u8;32]>`). `KeyTree::derive` takes `&[u8;32]` — pass `master_seed` (deref as needed: `KeyTree::derive(master_seed)` if it's already `&[u8;32]`, else `KeyTree::derive(&**master_seed)`). Match the type at the call site.

### 4c. Joiner extracts + threads to result

- [ ] **Step 4: Joiner decodes.** In `state_machine.rs`, in the `EncryptedPayload::Enroll { .. }` arm (~1134), add `fleet_keytree_cbor_hex,` to the destructure. After `owner_state` is decoded and before building `JoinerEnrollResult` (~1247), decode the material (hard-fail on a malformed value — it was sealed under the authenticated SAS channel, so corruption is a protocol error, not a network glitch):

```rust
    let fleet_keytree = match fleet_keytree_cbor_hex {
        Some(hexs) => {
            let bytes = match hex::decode(&hexs) {
                Ok(b) => b,
                Err(e) => {
                    let _ = state_tx.send(PairingState::Failed {
                        reason: format!("fleet keytree hex: {e}"),
                    });
                    return;
                }
            };
            match ciborium::from_reader::<crate::owner_state_crypto::FleetKeyMaterial, _>(
                bytes.as_slice(),
            ) {
                Ok(m) => Some(m),
                Err(e) => {
                    let _ = state_tx.send(PairingState::Failed {
                        reason: format!("fleet keytree decode: {e}"),
                    });
                    return;
                }
            }
        }
        None => None,
    };
```

Then add `fleet_keytree,` to the `JoinerEnrollResult { ... }` literal (~1247).

Update any test/other `JoinerEnrollResult { .. }` and `EncryptedPayload::Enroll { .. }` literals in the codebase (search both) to include the new field (`fleet_keytree: None` / `fleet_keytree_cbor_hex: None`) so they still compile.

### 4d. Persist on install

- [ ] **Step 5: Persist.** In `pairing/persist.rs` `install_joiner_state_inner` (~38), after `save_owner_state_atomic(...)` succeeds, persist the material when present:

```rust
    if let Some(material) = result.fleet_keytree.as_ref() {
        let mut buf = Vec::new();
        ciborium::into_writer(material, &mut buf)
            .map_err(|e| format!("encode fleet keytree for persist: {e}"))?;
        crate::owner_state::save_fleet_keytree(&keychain, identity_dir, &buf)?;
    }
```

This requires `save_fleet_keytree` to be reachable from `persist.rs`. It is currently private in `owner_state.rs` — make it `pub(crate)` (and `load_fleet_keytree` too, for symmetry/tests). Add `use` as needed.

### 4e. Integration test

- [ ] **Step 6: Write the integration test.** Add to `pairing/persist.rs` `#[cfg(test)] mod tests` (or the existing pairing integration test file if that's the established home — match where `install_joiner_state_inner` is currently tested). The test proves a joiner persists the material and a subsequent load reconstructs a KeyTree that decrypts an inviter-written entry:

```rust
#[test]
fn install_joiner_persists_fleet_keytree_and_decrypts_inviter_entry() {
    use crate::owner_state_crypto::{decrypt_entry, encrypt_entry, space_lookup_key, KeyTree};
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("HARMONY_PASSPHRASE", "zeb492-persist-test");

    // Inviter side: derive KeyTree from a seed, write an entry, export material.
    let seed = [0x5Au8; 32];
    let kt = KeyTree::derive(&seed).unwrap();
    let lk = space_lookup_key(&kt, b"dm-inbox-v1");
    let ciphertext = encrypt_entry(&kt, &lk, b"butler payload").unwrap();
    let material = kt.to_fleet_material();

    // Build a JoinerEnrollResult carrying the material + persist it (keychain None).
    let result = /* construct JoinerEnrollResult with a throwaway signing key,
                    a minimal OwnerState, our_device_id, and Some(material).
                    Reuse whatever helper the existing joiner-persist test uses
                    to build a JoinerEnrollResult. */;
    crate::pairing::persist::install_joiner_state_inner(dir.path(), result, None).unwrap();

    // Reload the material and confirm it decrypts the inviter's entry.
    let loaded = crate::owner_state::load_fleet_keytree(&None, dir.path()).unwrap().unwrap();
    let m: crate::owner_state_crypto::FleetKeyMaterial =
        ciborium::from_reader(loaded.as_slice()).unwrap();
    let kt2 = KeyTree::from_fleet_material(&m).unwrap();
    let lk2 = space_lookup_key(&kt2, b"dm-inbox-v1");
    assert_eq!(decrypt_entry(&kt2, &lk2, &ciphertext).unwrap(), b"butler payload");

    std::env::remove_var("HARMONY_PASSPHRASE");
}
```

(Look at the existing `install_joiner_state_inner` test — e.g. `install_writes_owner_state_cbor` referenced in prior runs — for the exact `JoinerEnrollResult` construction helper + `OwnerState` fixture, and reuse it. Keep `keychain: None` per ZEB-428.)

- [ ] **Step 7: Run the pairing tests.**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(install_joiner) + test(pairing)'`
Then full crate + all-targets to catch the literal-update compile breaks (this is the CI-scope gate — Task 4 touches integration tests):

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: all pass; clippy clean. If `tests/pairing_integration.rs` has `EncryptedPayload::Enroll`/`JoinerEnrollResult` literals, they must compile with the new field.

- [ ] **Step 8: Commit**

```bash
cd src-tauri && cargo fmt --all
git add src/pairing/types.rs src/pairing/state_machine.rs src/pairing/persist.rs
git commit -m "feat(zeb-492): seal fleet KeyTree to joiner at pairing + persist on install

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: e2e — upgrade `s7_butler_deposit_recover` to a full assert

**Files:**
- Modify: `e2e-harness/` — the `s7_butler_deposit_recover` test (search `s7_butler_deposit_recover`)

Context: pre-ZEB-492, `s7` reaches "boundary 0b": the cert-only joiner B2 pairs (peers=1) but `get_butler_held` 500s "dm-inbox not running" because B2 built no fleet engines, so the test characterizes (logs the finding, passes via fallback) instead of asserting the deposit chain. With ZEB-492, B2 receives + persists the KeyTree at pairing and builds its dm-inbox/fleet engines on relaunch — so the full HELD→RECV→CLEARED chain should now run.

- [ ] **Step 1: Build the harness binary.**

```bash
cd src-tauri && cargo build --bin harmony-app
```

- [ ] **Step 2: Run s7 as-is to confirm it now gets past boundary 0b.**

```bash
cd e2e-harness && cargo nextest run --features e2e -E 'test(s7_butler_deposit_recover)' --no-capture
```

Expected: B2 now starts its dm-inbox engine (no "dm-inbox not running" 500). If it still 500s, STOP — the KeyTree isn't reaching the engines; debug Tasks 2-4 before changing the harness.

- [ ] **Step 3: Replace the characterize-at-0b fallback with a hard assert.** In the `s7_butler_deposit_recover` test, remove the "FINDING (ZEB-491): ... Skipping HELD/RECV/CLEARED" early-return/characterize branch and instead drive + assert the deposit chain: butler B2 holds the deposit (`get_butler_held` returns the held item), the recipient recovers it, and the item reaches CLEARED. Use the existing assertion helpers/poll utilities in the harness (match the s6/relay-rung scenarios' HELD→RECV→CLEARED assertions; reuse their `poll_until`/`*_contains` patterns). **Use the DTO's camelCase keys** in any `poll_until`/`*_contains` (`channelId`/`spaceId`/etc.) — a guessed `id` key silently always-times-out (the ZEB-462-A lesson).

- [ ] **Step 4: Run s7 ×3 for determinism.**

```bash
cd e2e-harness && for i in 1 2 3; do echo "### s7 run $i"; cargo nextest run --features e2e -E 'test(s7_butler_deposit_recover)' --no-capture || break; done
```

Expected: 3/3 pass with the full HELD→RECV→CLEARED assert.

- [ ] **Step 5: Commit**

```bash
git add e2e-harness/
git commit -m "test(zeb-492): s7 asserts full butler HELD->RECV->CLEARED (cert-only B2 now a butler)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Final verification (before PR)

- [ ] Full gates, CI scope:

```bash
cd src-tauri \
  && cargo fmt --all -- --check \
  && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings \
  && cargo nextest run --locked -p harmony-app --features test-fixtures
```

- [ ] e2e `s7` green ×3 (Task 5).
- [ ] Confirm no `seed`/`master_seed` leaked into logs (grep new code for `tracing::` lines that interpolate key material — there should be none).

## Self-review notes (plan author)

- **Spec coverage:** §Components 1 → Task 1; §Components 4 (persistence) → Task 2; §Components 2 (boot gate) → Task 3; §Components 3 (pairing payload) → Task 4; §Testing e2e → Task 5. §Security (no-Debug, Zeroize, friend_aead included, no seed path) is enforced in Task 1's type + Task 3's comment.
- **Type consistency:** `FleetKeyMaterial` (owner_state_crypto) is the single type threaded through `LoadedOwnerState.fleet_keytree`, `JoinerEnrollResult.fleet_keytree`, and CBOR-on-the-wire (`fleet_keytree_cbor_hex`) / CBOR-at-rest (`SecretVault.fleet_keytree: Option<Vec<u8>>`). `KeyTree::to_fleet_material`/`from_fleet_material` are the only converters.
- **Known soft spots for the implementer to resolve against real code (flagged, not hidden):** the exact `vault_*_slot` store-resolution boilerplate (2b); the `HARMONY_PASSPHRASE` resolver name in `identity.rs` (2c — `resolve_passphrase_env` is the intended name; reuse the existing resolver if differently named); the `JoinerEnrollResult` construction helper in the existing pairing test (4e); the s7 HELD→RECV→CLEARED helper names (5). Each step says which existing code to mirror.
