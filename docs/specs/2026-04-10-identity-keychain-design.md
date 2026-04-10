# Identity & Keychain: OS-Native Key Management

## Goal

Store harmony-client's private identity keys in the OS-native keychain
(macOS Keychain, Linux Secret Service, Windows Credential Manager) instead
of an unencrypted file on disk. Fall back to file-based storage when no
keychain is available.

## Scope

**In scope:**
- Keychain storage via the `keyring` crate (macOS/Linux/Windows)
- File-based fallback for environments without a keychain
- Auto-migration of existing `~/.harmony/identity.key` into keychain
- Backup of migrated file as `identity.key.bak`

**Out of scope:**
- Key backup/restore UX
- Multiple identity profiles
- Hardware security module (HSM/TPM) integration
- Frontend changes (no UI for keychain status or selection)

## Architecture

### Storage Backends

Two backends behind a common interface:

```
KeyStore (trait)
  load() -> Option<(PrivateIdentity, PqPrivateIdentity)>
  save(ed25519_bytes, pq_bytes) -> Result<()>

KeychainStore  -- delegates to keyring::Entry
FileStore      -- current ~/.harmony/identity.key logic, extracted
```

### Keychain Entry

- **Service name:** `"harmony"`
- **Account name:** `"identity"`
- **Data format:** Same 161-byte binary blob as the file format
  (1 byte version + 96 bytes PQ private key + 64 bytes Ed25519 private key)
- Appears as "harmony - identity" in macOS Keychain Access

### Resolution Chain

`load_or_generate()` at startup:

1. Try `KeychainStore::load()`
   - Found: return keys, done
2. Check `~/.harmony/identity.key` via `FileStore::load()`
   - Found: migrate to keychain (see Migration below), return keys
3. Neither exists: generate fresh keys
   - Try `KeychainStore::save()` first
   - If keychain write fails, fall back to `FileStore::save()`

### Migration

When an existing `identity.key` file is found and no keychain entry exists:

1. Load keys from file
2. Write 161-byte blob to keychain via `KeychainStore::save()`
3. If keychain write succeeds: rename file to `identity.key.bak`
4. If keychain write fails: leave file in place, use file store (migration aborted)

The `.bak` file is a safety net. No code reads it — it exists only for
manual recovery if a keychain interaction goes wrong.

### File Changes

**Modified:**
- `src-tauri/src/identity.rs` — Extract current I/O into `FileStore`,
  add `KeychainStore`, replace `load_or_generate()` with the resolution
  chain. The public API shape stays the same so `lib.rs` callers don't change.
- `src-tauri/Cargo.toml` — Add `keyring` dependency

**Unchanged:**
- `src-tauri/src/lib.rs` — Still calls `identity::load_or_generate()`,
  gets back the same types
- `src-tauri/src/event_loop.rs` — No identity changes
- All frontend code — `get_node_addr()` works identically

## Error Handling & Fallback

The keychain can fail: no D-Bus session (headless Linux), locked keychain
with user cancel (macOS), permission denied (managed Windows).

| Scenario | Behavior |
|----------|----------|
| Keychain read fails | Fall back to file store, log warning |
| Keychain write fails (generation) | Write to file store instead, log warning |
| Keychain write fails (migration) | Keep using file store, don't rename to `.bak` |
| File store also fails | Hard error, cannot start node (same as today) |

No retry logic, no user prompts. The fallback is transparent — if the
keychain isn't available, file storage works exactly as it does today.

## Testing Strategy

### Unit Tests

Using `keyring`'s `mock` feature for tests that don't require a real keychain:

- **FileStore round-trip:** Write keys, read back, verify match
- **KeychainStore round-trip:** Same, using mock credential store
- **Migration:** File exists + no keychain entry -> keys end up in keychain,
  file renamed to `.bak`
- **Migration abort:** File exists + keychain write fails -> keys loaded
  from file, no `.bak` created
- **Fallback:** Keychain unavailable -> file store used, no error
- **Fresh generation:** Nothing exists -> keys generated, stored in keychain
  (or file if keychain fails)

### Manual Integration Test

1. Delete `~/.harmony/identity.key` and keychain entry, launch app ->
   new identity generated in keychain
2. Verify entry visible in macOS Keychain Access under "harmony"
3. Kill app, relaunch -> same identity loaded from keychain
4. Delete keychain entry -> falls back to file store on next launch

## Known Limitations

1. **No encryption of backup file** — `identity.key.bak` retains the same
   unencrypted format. It's a recovery artifact, not a security improvement.
   Users who want the backup gone can delete it manually.

2. **Keychain access may prompt** — macOS may show a system dialog asking
   the user to allow harmony-client to access the keychain on first use.
   This is expected OS behavior, not something we control.

3. **Linux requires D-Bus** — Linux Secret Service requires an active D-Bus
   session with a secret service provider (GNOME Keyring, KWallet). Headless
   servers and minimal installs fall back to file storage.
