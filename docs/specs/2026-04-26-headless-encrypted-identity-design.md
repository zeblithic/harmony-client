# Headless Encrypted Identity At Rest — Design

**Linear:** ZEB-174 — *harmony-client: OS keychain integration for identity keys at rest*
**Builds on:** [`2026-04-10-identity-keychain-design.md`](./2026-04-10-identity-keychain-design.md) (the ZEB-34 keychain integration this extends)

## Goal

Close the three gaps that remain in `src-tauri/src/identity.rs` after the
ZEB-34 keychain landing:

1. **Plaintext `.bak` files persist after migration.** When the existing
   resolution chain migrates a plaintext identity into the keychain, it
   renames the source file to `identity.key.bak` and leaves it on disk
   indefinitely. The keys we just "encrypted" are still recoverable from
   `~/.harmony/identity.key.bak`.
2. **Headless installs still write plaintext.** When no OS keychain is
   reachable (Linux server with no Secret Service, Docker container,
   CI environment), the file fallback writes the 161-byte identity blob
   in the clear at `~/.harmony/identity.key`.
3. **No documentation for the headless install path.** DoD item 3 from
   ZEB-174 is unmet.

This design adds a passphrase-derived encrypted-file backend, replaces
the post-migration `.bak` retention with verify-and-delete, automates
cleanup of legacy `.bak` files from the prior code, and adds a
`rotate-passphrase` CLI subcommand.

## Non-goals

- Owner→device binding integration (ZEB-173 — separate ticket, separate
  crate; this spec only protects what `harmony-client` already writes
  to disk today)
- Backup/restore UX, mnemonic export, recovery artifacts (ZEB-175)
- Hardware tokens (YubiKey/TPM) — possible future, out of scope here
- OpenWRT and other memory-constrained embedded targets (will be picked
  up by `harmony-openwrt` with tuned KDF parameters)
- Cross-store divergence detection (when keychain and `.enc` both contain
  identities and they differ — verifying both stores at every boot would
  force an unconditional Argon2id derivation)
- Multiple identity profiles per user

## Architecture

### Three storage layers

```text
┌─────────────────┐
│ KeychainStore   │ ── implements KeyStore  (load + save)
└─────────────────┘     unchanged from ZEB-34
                        primary on macOS / Windows / Linux-with-Secret-Service

┌──────────────────────┐
│ EncryptedFileStore   │ ── implements KeyStore  (load + save)   *** NEW ***
└──────────────────────┘    Argon2id + XChaCha20-Poly1305 AEAD
                            keyed from `HARMONY_PASSPHRASE` /
                            `HARMONY_PASSPHRASE_FILE`
                            file at `~/.harmony/identity.enc`

┌─────────────────────────┐
│ LegacyPlaintextReader   │ ── *not* a KeyStore impl              *** NEW ***
└─────────────────────────┘    read-only legacy plaintext reader
                               only used during one-shot migration of
                               `~/.harmony/identity.key`
```

`FileStore` is removed. The plaintext file format is read-only via
`LegacyPlaintextReader`; we never write a fresh plaintext file again.
The trait stays at two methods — `load`, `save` — and only the two
modern stores implement it.

### Resolution chain

`load_or_generate_with_stores` becomes:

```text
1. match keychain.load() {
       Ok(Some(id)) → cleanup_legacy_bak(plaintext_path, &id, &keychain); return id
       Ok(None)     → fall through  (no entry yet)
       Err(_)       → log warn, set keychain_healthy = false, fall through
                       (transient OS-keychain error — try next store; do NOT hard fail)
   }

2. encrypted = EncryptedFileStore::from_env(enc_path)?
   // ? propagates env-parsing errors (empty passphrase, unreadable file) as hard fails.
   // Returns Some(store) when env var is set and well-formed; Ok(None) when no env var.
   if let Some(enc) = &encrypted {
       match enc.load() {
           Ok(Some(id)) → cleanup_legacy_bak(plaintext_path, &id, enc); return id
           Ok(None)     → fall through  (no .enc file yet — fresh-with-passphrase install)
           Err(e)       → return Err(e)
                          (AEAD failure, format mismatch, length mismatch — HARD FAIL.
                           Do NOT fall through to step 4; that would silently regenerate
                           identity on a passphrase typo.)
       }
   }

3. legacy = LegacyPlaintextReader::new(plaintext_path)
   if legacy.read()? → Some(id):
       dest: &dyn KeyStore = if keychain_healthy { &keychain }
                              else if let Some(enc) = &encrypted { enc }
                              else { return HARD FAIL with docs/headless-install.md pointer }
       dest.save(&id)?
       verify_round_trip(dest, &id)?  // failure here aborts; plaintext NOT unlinked
       fs::remove_file(plaintext_path).log_warn_on_err()
       return id

4. // Fresh generate.
   dest: &dyn KeyStore = same precedence as step 3, same hard-fail rule
   id = NodeIdentity::generate()
   dest.save(&id)?
   verify_round_trip(dest, &id)?
   return id
```

The asymmetry between step 1's `Err → fall through` and step 2's `Err → hard fail`
is deliberate: a transient OS-keychain error (locked, no D-Bus session, momentary
unavailability) is recoverable by trying the file backend, but an AEAD failure on
the encrypted file is *catastrophic* — it means either the passphrase is wrong or
the file is corrupt, both of which would lead to identity loss if we silently
generated fresh keys instead.

**Migration destination precedence: keychain > encrypted_file.** Keychain
is OS-protected (kernel + user-session unlock); encrypted-file is only
as strong as the passphrase. A `HARMONY_PASSPHRASE` env var on a system
with a healthy keychain does **not** force the encrypted-file path.

**Wrong passphrase ≠ regenerate.** A failed `EncryptedFileStore::load`
(AEAD tag rejection) bubbles up as an error. The chain does not advance
to step 4 to mint a fresh identity, because doing so would silently
overwrite the user's identity on a passphrase typo.

### Verify-and-delete (replaces `.bak` retention)

After every `KeyStore::save`, the chain immediately calls `verify_round_trip`:

```rust
fn verify_round_trip(store: &dyn KeyStore, expected: &NodeIdentity) -> Result<(), String> {
    let loaded = store.load()?
        .ok_or_else(|| format!("verify-after-write returned None"))?;
    let expected_blob = identity_to_blob(expected);
    let loaded_blob = identity_to_blob(&loaded);
    if !bool::from(subtle::ConstantTimeEq::ct_eq(
        expected_blob.as_slice(), loaded_blob.as_slice())) {
        return Err("identity store verify-after-write failed: store does not return what was written".into());
    }
    Ok(())
}
```

If `verify_round_trip` fails, the chain returns the error immediately
and does **not** delete the plaintext source file. The current `.bak`
rename is removed entirely — once the destination has the verified
identity, the source is `unlink`ed.

### Legacy `.bak` cleanup

Runs at the end of every successful `keychain.load()` or
`encrypted_file.load()` (i.e., when an existing store had the identity
already and no migration was needed):

```rust
fn cleanup_legacy_bak(plaintext_path: &Path, in_memory: &NodeIdentity, store: &dyn KeyStore) {
    let bak = plaintext_path.with_extension("key.bak");
    if !bak.exists() { return; }
    match LegacyPlaintextReader::read_from(&bak) {
        Ok(Some(bak_id)) if identity_blobs_eq(&bak_id, in_memory)
                         && verify_round_trip(store, in_memory).is_ok() => {
            if let Err(e) = fs::remove_file(&bak) {
                tracing::warn!(path = %bak.display(), error = %e,
                    "legacy .bak removal failed — manual cleanup needed");
            } else {
                tracing::info!(path = %bak.display(),
                    "removed legacy plaintext .bak after verifying live store has matching identity");
            }
        }
        Ok(Some(_)) => tracing::warn!(path = %bak.display(),
            "legacy .bak present but identity differs from current — leaving in place; manual review needed"),
        Ok(None) | Err(_) => tracing::warn!(path = %bak.display(),
            "legacy .bak unreadable — leaving in place"),
    }
}
```

The mismatch case is conservative: never delete a `.bak` whose contents
do not match what is currently in use. That covers identity rotation
across boots and accidentally pointing at someone else's data
directory.

### Atomic write helper

Extract the existing `FileStore::save` write-atomic-with-0o600 logic into
a shared `write_atomic_0600(path, bytes)` helper. Both
`EncryptedFileStore::save` and the (gone) `FileStore::save` would use it;
in the final design only `EncryptedFileStore::save` calls it. The helper
keeps the `.tmp → fsync → rename + TmpGuard` semantics that already work.

## Encrypted file wire format (v1)

Fixed-size, network byte order, totally self-describing. 230 bytes total.

```text
offset  size  field
------  ----  -----
0       4     magic            = b"HRMI"
4       1     format_version   = 0x01
5       1     kdf_id           = 0x01 (Argon2id)
6       4     kdf_m_kib        u32 BE  (memory in KiB; 65536 = 64 MiB)
10      2     kdf_t            u16 BE  (time/iterations; 3)
12      1     kdf_p            u8       (parallelism; 1)
13      16    salt             OsRng
29      24    nonce            OsRng (XChaCha20 needs 192-bit nonce)
53      161   ciphertext       AEAD(plaintext_blob)
214     16    poly1305_tag     AEAD authentication tag
─────────────
total: 230 bytes
```

**Plaintext input** is the existing 161-byte identity blob produced by
`identity_to_blob()` (1 byte version + 96 PQ + 64 Ed25519). The encryption
layer is a pure envelope; the inner blob format is unchanged.

**KDF.** Argon2id with output length 32 bytes → key for XChaCha20-Poly1305.
Parameters are written into the file so it is self-describing. A future
parameter change is a `format_version` bump that adds a new branch in the
load path; the old branch keeps reading old files.

**AEAD additional data (AAD).** The first 13 bytes
(`magic | format_version | kdf_id | kdf_m_kib | kdf_t | kdf_p`) are passed
as AAD. This binds the KDF parameters to the ciphertext — an attacker
cannot downgrade the KDF (e.g., rewrite `kdf_m_kib` from 65536 to 8)
without breaking the Poly1305 tag.

**Salt and nonce rotate on every save.** Both are freshly generated from
`OsRng` per `save()` call. The KDF result is not cached across writes.

**Cipher choice rationale.** XChaCha20-Poly1305 is selected over AES-GCM
for its 192-bit random nonce (collision-safe with `OsRng` for the
lifetime of the universe; no nonce-management state) and constant-time
behavior on platforms without AES-NI.

**KDF choice rationale.** Argon2id is the OWASP recommendation for
password hashing — memory-hard against GPU and ASIC attackers. Scrypt is
acceptable but Argon2id is the modern default. PBKDF2 is too cheap.

**Passphrase normalization.** UTF-8 bytes used as-is. No NFKC
normalization — different normalization implementations yield different
keys, which is itself an attack surface.

**KDF parameters (v1).** `m=65536 KiB`, `t=3`, `p=1`. Hardcoded as
constants. No runtime tuning, no env-var override (avoids the "user
accidentally sets t=1 in dev, ships to prod" footgun). If we need
OpenWRT-class smaller params or hardened-laptop bigger params later,
that is a `format_version` bump.

## Headless trigger and passphrase source

Passphrase comes from one of two environment variables. Set neither and
the encrypted-file backend is unavailable.

| Variable | Meaning |
|---|---|
| `HARMONY_PASSPHRASE` | UTF-8 passphrase as the variable's value (least preferred — exposed in process listings) |
| `HARMONY_PASSPHRASE_FILE` | Path to a file whose contents are the passphrase (preferred — keeps secrets out of process tables) |

**Precedence.** `HARMONY_PASSPHRASE` wins over `HARMONY_PASSPHRASE_FILE`
when both are set. A warning is logged on startup. Rationale: a direct
env var is the more explicit signal — if you set it deliberately at the
shell, you are saying "use this exact value, not the file path."

**File parsing.** Read raw bytes; strip exactly one trailing `\n` if
present (most editors append one). Trailing `\r\n` strips both. No other
trimming. Passphrases that genuinely contain trailing whitespace are not
supported and would surprise users; document the rule.

**Empty passphrase rejected.** `HARMONY_PASSPHRASE=""` or a file that
resolves to empty after newline-strip is a hard error.

**File mode warning.** If the passphrase file is group/world-readable on
Unix (mode `& 0o077 != 0`), log a warning at startup but continue —
same posture as the existing plaintext file warning.

## CLI subcommand: `rotate-passphrase`

`harmony-client` gains one subcommand handled in `src-tauri/src/main.rs`
*before* launching the Tauri runtime. The subcommand exits the process
when done; it never starts the GUI, mesh node, event loop, or anything
else.

```bash
HARMONY_PASSPHRASE_FILE=/etc/harmony/old.txt \
  harmony-client rotate-passphrase \
    --new-passphrase-file=/etc/harmony/new.txt
```

**Argument parsing.** `clap = "4"` with the `derive` feature. Already a
transitive dep across the workspace; one explicit declaration in
`harmony-client/src-tauri/Cargo.toml`. Hand-rolled argv matching is
possible but `clap` gives `--help` and typo suggestions for free.

**Old passphrase** is read from the existing `HARMONY_PASSPHRASE` /
`HARMONY_PASSPHRASE_FILE` chain — we are already required to know it
to load the file.

**New passphrase** is read from a required `--new-passphrase-file=<path>`
flag. File path only, never a raw value, same posture as the file env
var. This keeps secrets out of process listings and shell history.

**Refusal conditions** (pre-checks before touching the file):

```
1. KeychainStore::new().load() → Some(_):
       hard-fail "your identity is currently in the OS keychain;
                  passphrase rotation only applies to headless installs.
                  Re-encryption of keychain entries is handled by the OS
                  when you change your login password."
2. EncryptedFileStore::from_env returns Ok(None) — no env var set:
       hard-fail "HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set —
                  cannot rotate without the old passphrase"
   (Err from from_env — empty passphrase, unreadable file — propagates with
    its own message, same as load-time.)
3. --new-passphrase-file missing or unreadable:
       hard-fail with same posture as load-time errors
4. New passphrase resolves to empty (after newline strip):
       hard-fail
5. New passphrase byte-identical to old:
       log warning "new passphrase matches old — proceeding anyway", succeed
6. Otherwise: rotate, verify_round_trip, exit 0
```

**Atomic semantics.** Rotation writes the new `.enc` to `.tmp` first,
fsyncs, atomic-renames over the old one. If the rename succeeds, the old
passphrase no longer decrypts the file. If anything fails before the
rename, the old file is untouched (the `TmpGuard` from the shared
`write_atomic_0600` helper cleans up `.tmp`).

**Implementation surface in `identity.rs`:**

```rust
pub fn rotate_passphrase(
    old: &EncryptedFileStore,
    new_passphrase: SecretString,
) -> Result<(), String> {
    let identity = old.load()?
        .ok_or_else(|| "no encrypted identity to rotate at <path>".to_string())?;
    let new_store = EncryptedFileStore::new(old.path().clone(), new_passphrase);
    new_store.save(&identity)?;
    verify_round_trip(&new_store, &identity)?;
    Ok(())
}
```

## Error handling

Three principles guide the choices: never silently degrade to plaintext;
never regenerate identity on a recoverable error (catastrophic data loss);
never leak which-failed-which when the answer would help an attacker.

The error type stays `Result<_, String>` to match the existing
`identity.rs` surface (consumed by Tauri commands that already use
`String` errors). Messages must be diagnostic.

### Hard failures (refuse to start, surface error to caller)

| Condition | Error message | Why hard-fail |
|---|---|---|
| Plaintext exists + no keychain healthy + no `HARMONY_PASSPHRASE` / `HARMONY_PASSPHRASE_FILE` | `"plaintext identity at <path> needs a destination but no keychain available and HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set — see docs/headless-install.md"` | Deleting plaintext without a verified destination = identity loss |
| Fresh generate needed + no keychain healthy + no `HARMONY_PASSPHRASE` / `HARMONY_PASSPHRASE_FILE` | `"no identity store available: keychain unavailable and HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set — see docs/headless-install.md"` | Falling back to plaintext is the bug we are fixing |
| `EncryptedFileStore::load` AEAD tag fails | `"identity store at <path> could not be decrypted: wrong passphrase or corrupted file"` | Indistinguishable on purpose — do not leak which |
| `EncryptedFileStore::load` format/magic/version mismatch | `"identity store at <path> is in an unrecognized format (magic=<…>, version=<…>) — this build may be too old"` | Older binary on newer file → bail loudly, do not regenerate |
| `EncryptedFileStore::load` length mismatch | `"identity store at <path> is corrupt: expected 230 bytes, got <N>"` | File is truncated or wrong type — bail |
| Round-trip verify after `save` fails | `"identity store verify-after-write failed for <store>: store does not return what was written"` | Store is fundamentally broken; absolutely do not delete plaintext |
| `HARMONY_PASSPHRASE_FILE` unreadable | `"HARMONY_PASSPHRASE_FILE=<path> could not be read: <io error>"` | Configuration error — fail fast |
| `HARMONY_PASSPHRASE_FILE` resolves to empty passphrase | `"HARMONY_PASSPHRASE_FILE=<path> contains an empty passphrase (after trimming one trailing newline)"` | Empty passphrase is a footgun — refuse |
| `HARMONY_PASSPHRASE` set to empty string | `"HARMONY_PASSPHRASE is set to an empty string"` | Same |

### Warnings (continue, but log loudly)

| Condition | Action |
|---|---|
| Legacy `.bak` content differs from in-memory identity | `tracing::warn!` "leaving in place — manual review needed", do not delete |
| Legacy `.bak` unreadable | `tracing::warn!` "leaving in place — could not parse" |
| Legacy `.bak` removal `unlink` fails | `tracing::warn!` "manual cleanup needed at <path>" — startup proceeds |
| Both `HARMONY_PASSPHRASE` and `HARMONY_PASSPHRASE_FILE` set | `tracing::warn!` "both set; HARMONY_PASSPHRASE takes precedence" |
| `HARMONY_PASSPHRASE_FILE` mode is world/group-readable on Unix | `tracing::warn!` "<path> has open permissions <mode>, should be 0600" |
| Keychain transient load error (not `NoEntry`) | `tracing::warn!` "keychain load failed, trying next store" — existing behavior |
| Plaintext `unlink` after successful migration fails | `tracing::warn!` "identity migrated but plaintext file at <path> could not be removed: <err>" — eventual consistency next boot |

### Edge cases worth calling out

1. **Wrong passphrase ≠ regenerate.** AEAD failure returns the error to
   the caller; the chain does not advance to "step 4: generate fresh."
2. **Newline handling.** Read raw bytes; strip exactly one trailing `\n`
   (or `\r\n`) if present. No other trimming.
3. **Keychain healthy but `.enc` also present** (someone copied a backup
   over): keychain wins, `.enc` is silently ignored. Cross-store
   divergence detection is out of scope.
4. **Argon2id allocation failure** (m=64 MiB rejected on memory-pressed
   system): bubble up the underlying error verbatim.
5. **Concurrent identical-passphrase writes** from two processes: same
   hazard as the existing plaintext-file race; not made worse here, not
   fixed here.

## Wire format interop test

One pinned fixture, same posture as `harmony-owner`'s `interop_fixtures.rs`.
Uses a test-only `encrypt_with_params(passphrase, salt, nonce, blob)`
helper that takes salt and nonce explicitly so the byte output is
deterministic.

```rust
#[test]
fn wire_format_v1_pinned() {
    let passphrase = b"correct horse battery staple";
    let salt = [0xAB; 16];
    let nonce = [0xCD; 24];
    let identity_blob: [u8; 161] =
        include_bytes!("fixtures/identity_blob_v1.bin").try_into().unwrap();
    let bytes = encrypt_with_params(passphrase, &salt, &nonce, &identity_blob);
    let expected = include_bytes!("fixtures/encrypted_v1.bin");
    assert_eq!(bytes.as_slice(), expected,
        "WIRE FORMAT CHANGED — bump format_version and add a v2 fixture");
}
```

## Testing

Tests live inline in `identity.rs`, split into focused sub-modules:

```rust
#[cfg(test)]
mod tests {
    mod keychain_store;          // existing, unchanged
    mod encrypted_file_store;    // NEW
    mod legacy_plaintext_reader; // NEW
    mod env;                     // NEW — passphrase env var precedence
    mod resolution_chain;        // existing + new chain coverage
    mod legacy_bak_cleanup;      // NEW
    mod rotation;                // NEW
    mod wire_format_fixture;     // NEW
}
```

The stack-size workaround
(`std::thread::Builder::new().stack_size(8 * 1024 * 1024)` wrapping every
PQ-keygen test) moves to a workspace `.cargo/config.toml` via
`RUST_MIN_STACK=8388608`. Individual tests drop the boilerplate.

### `encrypted_file_store` — ~10 tests

| Test | Asserts |
|---|---|
| `round_trip_correct_passphrase` | save → load → identical NodeIdentity |
| `wrong_passphrase_fails_aead` | save with A, load with B → "wrong passphrase or corrupted file" |
| `tampered_ciphertext_fails` | flip 1 bit in ciphertext → AEAD tag rejection |
| `tampered_kdf_params_fails_aad` | flip `kdf_m_kib` bytes → AEAD AAD rejection (AAD-binding works) |
| `tampered_magic_fails` | overwrite first 4 bytes → magic check rejection |
| `tampered_version_fails` | overwrite version byte → unrecognized format error |
| `truncated_file_fails` | truncate to 200 bytes → length mismatch error |
| `salt_rotates_per_save` | save twice → on-disk bytes differ |
| `file_mode_0o600_unix` | save → `metadata().mode() & 0o777 == 0o600` |
| `decrypt_does_not_regenerate_on_failure` | wrong-passphrase error bubbles up, no fresh keys generated |

### `legacy_plaintext_reader` — ~3 tests

| Test | Asserts |
|---|---|
| `read_existing_plaintext` | reads a 161-byte legacy file correctly |
| `read_returns_none_when_missing` | `Ok(None)` not error |
| `does_not_implement_keystore` | compile-time assertion (optional, via `static_assertions`) |

### `env` — ~7 tests (use `serial_test = "3"` to avoid env races)

| Test | Asserts |
|---|---|
| `direct_env_var_set` | `HARMONY_PASSPHRASE=foo` → returns `Some("foo")` |
| `file_var_set` | `HARMONY_PASSPHRASE_FILE=/tmp/x` (containing `bar\n`) → `Some("bar")` |
| `crlf_stripped` | file contains `bar\r\n` → `Some("bar")` |
| `direct_wins_over_file` | both set → returns direct value, log captured contains warning |
| `empty_direct_hard_fails` | `HARMONY_PASSPHRASE=""` → error |
| `empty_file_hard_fails` | file is `\n` → error |
| `missing_file_hard_fails` | file path does not exist → error |

### `resolution_chain` — extends existing 9 to ~15 tests

| New test | Asserts |
|---|---|
| `headless_no_keychain_no_env_hard_fails_on_fresh` | step 4 with no destination → error |
| `headless_no_keychain_no_env_hard_fails_with_plaintext` | step 3 with no destination → error, plaintext NOT deleted |
| `wrong_passphrase_does_not_regenerate` | `.enc` exists, wrong env → error, `.enc` untouched |
| `migrate_plaintext_prefers_keychain_over_encrypted` | plaintext + keychain healthy + env set → migrated to keychain, `.enc` not created |
| `migrate_plaintext_to_encrypted_when_no_keychain` | plaintext + no keychain + env set → migrated to `.enc`, plaintext unlinked |
| `verify_round_trip_failure_aborts_migration` | mock store returns mutated bytes on load → migration error, plaintext NOT unlinked |

### `legacy_bak_cleanup` — ~4 tests

| Test | Asserts |
|---|---|
| `matching_bak_deleted_after_keychain_verify` | keychain has X, `.bak` has X → `.bak` removed |
| `mismatched_bak_left_in_place` | keychain has X, `.bak` has Y → warn logged, `.bak` exists |
| `unreadable_bak_left_in_place` | keychain has X, `.bak` is garbage → warn logged, `.bak` exists |
| `bak_cleanup_only_after_verify_round_trip_succeeds` | mock store with mismatched-load → `.bak` not deleted |

### `rotation` — ~5 tests

| Test | Asserts |
|---|---|
| `rotate_happy_path` | encrypt with A, rotate to B, decrypt with B succeeds, decrypt with A fails |
| `rotate_wrong_old_passphrase_fails` | wrong `HARMONY_PASSPHRASE` → load error, file untouched |
| `rotate_empty_new_passphrase_fails` | new file is empty → hard fail, file untouched |
| `rotate_to_same_passphrase_warns_but_succeeds` | A → A → log warning, file rewritten with new salt+nonce |
| `rotate_with_keychain_present_refuses` | keychain has identity → rotation hard-fails before touching the file |

### `wire_format_fixture` — 1 test

The pinned fixture above.

### Out of scope for tests

- Real-OS keychain integration (macOS Keychain, Windows Credential
  Manager, real Secret Service) — `keyring`'s mock backend covers the
  contract; real-OS testing requires CI runners with entitlements
- OpenWRT params tuning
- Cross-store divergence detection

**Final test tally:** 52 lib `identity` tests passing (4 baseline KeychainStore/FileStore + 3 LegacyPlaintextReader + 9 wire_format + 7 EncryptedFileStore + 8 env + 4 legacy_bak_cleanup + 12 resolution_chain + 4 rotation + 1 keychain-Err fall-through), plus 1 wire-format-fixture integration test. The 2 `rotate_passphrase_cli` integration tests are `#[ignore]`d by default (probe the real OS keychain — opt in via `--ignored` on environments with a known-clean keychain).

## Documentation: `docs/headless-install.md`

One new file. Server-admin-friendly, copy-pasteable, lives next to the
code so it stays in sync. Outline:

- Quickstart for Linux server / Docker
- systemd unit example
- Docker example
- Env var precedence (`HARMONY_PASSPHRASE` > `HARMONY_PASSPHRASE_FILE`)
- Passphrase format rules (UTF-8, no normalization, byte-stable)
- Migration from prior versions (with/without keychain, with/without
  env var)
- `rotate-passphrase` subcommand usage
- Backup and recovery (one short pointer to ZEB-175 — backup is
  explicitly out of scope here)
- Troubleshooting table mapping error messages to causes and fixes
- "Not yet supported" — OpenWRT, hardware tokens

## Crate / file layout

```text
harmony-client/
├── docs/
│   ├── specs/
│   │   └── 2026-04-26-headless-encrypted-identity-design.md  ← this doc
│   ├── plans/
│   │   └── 2026-04-26-headless-encrypted-identity-plan.md    ← from writing-plans
│   └── headless-install.md                                    ← NEW (user-facing)
├── src-tauri/
│   ├── Cargo.toml                                             ← + argon2, chacha20poly1305, secrecy, subtle, serial_test, clap
│   ├── src/
│   │   ├── identity.rs                                        ← rewritten resolution chain, EncryptedFileStore, LegacyPlaintextReader, rotate_passphrase, tests submodules
│   │   └── main.rs                                            ← + clap subcommand parsing for rotate-passphrase
│   └── tests/fixtures/
│       ├── identity_blob_v1.bin                               ← NEW (deterministic 161-byte input)
│       └── encrypted_v1.bin                                   ← NEW (pinned 230-byte output)
└── .cargo/
    └── config.toml                                            ← NEW: RUST_MIN_STACK=8388608
```

## Threat coverage

| Threat | Defense |
|---|---|
| Local attacker with read access to `~/.harmony/` | Identity is in OS keychain (desktop) or encrypted file (headless); plaintext file does not exist after migration |
| Backup of disk image leaks identity | Same — disk image contains only `.enc`, not raw keys |
| Process listing / `ps -ef` leaks passphrase | `HARMONY_PASSPHRASE_FILE` keeps the passphrase out of argv and process tables |
| Attacker downgrades KDF params on `.enc` to brute-force faster | KDF params are AAD — Poly1305 tag rejects on tamper |
| Attacker swaps `.enc` for a known-passphrase one | Identity changes; downstream peers detect the new identity hash and treat as a new node — same posture as identity rotation |
| User typos passphrase on boot | Hard error; identity is **not** silently regenerated |
| User upgrades binary across `format_version` | Hard error with version diagnostic; binary refuses to act on a format it doesn't understand |
| Plaintext `.bak` from prior version persists | Auto-cleaned on first boot after verifying live store has matching identity |
| Keychain corruption later | Same recovery path as ZEB-175 (mnemonic / recovery artifact) — `.bak` retention does not help here |

## Future work

- **OpenWRT KDF tuning** — bump `format_version`, write smaller params
  into the file. Old files keep reading. Picked up by `harmony-openwrt`.
- **Hardware token integration (YubiKey, TPM)** — separate `KeyStore`
  backend; can slot into the resolution chain.
- **Wired into `harmony-owner`'s mint output** — when `harmony-client`
  consumes the harmony-owner crate, the mint's master seed and signing
  keys flow through the same `EncryptedFileStore` / `KeychainStore`
  surface. No format change needed; the inner blob just gets longer
  and the existing `format_version` on the inner identity blob covers
  it.
- **Cross-store divergence detection** — only worth doing if cheap;
  current Argon2id cost makes it not.
- **Real-OS keychain CI** — separate ticket; needs runners with
  entitlements.
