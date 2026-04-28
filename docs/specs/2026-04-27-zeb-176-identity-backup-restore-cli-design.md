# ZEB-176 — Identity Backup/Restore CLI Design

## Goal

Wire the [ZEB-175](https://linear.app/zeblith/issue/ZEB-175) recovery primitives shipped in `harmony-owner` (PR [#262](https://github.com/zeblithic/harmony/pull/262)) into `harmony-app` as user-facing CLI subcommands. Mirrors the ZEB-174 pattern: a small, testable, headless-friendly slice that converts an internal primitive into actual user value before the GUI half lands.

## Why CLI first (not GUI)

Same rationale as ZEB-174's `rotate-passphrase`: headless installs (server, Docker, CI) need recovery flows too, the CLI surface is small enough to land cleanly in one PR, and the GUI flow can be designed in parallel knowing the underlying machinery already works end-to-end.

## Hard prerequisite: ZEB-177

This ticket cannot ship until [ZEB-177](https://linear.app/zeblith/issue/ZEB-177) lands. ZEB-177 adds `PrivateIdentity::from_seed(&[u8; 32])` and `PqPrivateIdentity::from_seed(&[u8; 32])` to `harmony-identity` upstream. Without seeded keygen, "restore from a 32-byte master seed" has nothing to derive — the current `generate(rng)` API produces fresh randomness each call. The decision to take this path (over backing up the 161-byte serialized blob and giving up the BIP39-24 mnemonic option) was made explicitly during brainstorming on the basis that we have no continuity-with-existing-identities constraint to preserve.

## Architecture

### Identity-at-rest model change

Today (`identity.rs:934-937`): first launch generates two independent keypairs via `PqPrivateIdentity::generate(OsRng) + PrivateIdentity::generate(OsRng)`, serializes them as a 161-byte blob, encrypts the blob in the `HRMI` envelope at `~/.harmony/identity.enc`.

After ZEB-176: first launch generates a single 32-byte seed via `OsRng.fill_bytes()`, encrypts the seed in the `HRMI` envelope at `~/.harmony/identity.enc`, and derives the keypairs on every load via `NodeIdentity::from_seed(&seed)` (which calls the ZEB-177 constructors). Wire-format payload shrinks from 161 → 32 bytes; total file size drops from 230 → 101 bytes. The `HRMI` magic and Argon2id+XChaCha20-Poly1305 envelope are unchanged.

```text
First launch                Subsequent launch
────────────                ─────────────────
generate 32B seed (OsRng)   load 32B seed from identity.enc
  │                           │
  ├─ from_seed → ed25519 ─┐   ├─ from_seed → ed25519 ─┐
  ├─ from_seed → ml-kem ──┼─→ NodeIdentity            ┼─→ NodeIdentity
  ├─ from_seed → ml-dsa ──┘   └─ from_seed → ml-dsa ──┘
  │
  └─ store seed in identity.enc  (HRMI envelope, 32B payload)
```

**No migration:** existing on-disk identities are placeholder; the pre-ZEB-176 code path is replaced wholesale. A user upgrading past this commit who had a previous identity gets a hard failure on launch ("identity store payload length is unexpected"). Acceptable per scope ("just me using this right now").

### Two distinct AEAD envelopes

Same crypto primitives, different lifetimes, different magic bytes:

| Magic | Where | Passphrase source | Purpose |
|---|---|---|---|
| `HRMI` | `~/.harmony/identity.enc` (local, per-machine) | `HARMONY_PASSPHRASE` / `HARMONY_PASSPHRASE_FILE` | Identity at rest. Decrypted on every launch. |
| `HRMR` | Backup file at any operator-chosen path | `HARMONY_RECOVERY_PASSPHRASE` / `HARMONY_RECOVERY_PASSPHRASE_FILE` | Portable backup. Cross-machine restore. |

`HRMR` is shipped by `harmony_owner::recovery` (ZEB-175); `HRMI` is the existing `harmony-client` envelope from ZEB-174. Both use Argon2id (m=64 MiB, t=3, p=1) + XChaCha20-Poly1305, share KDF parameters byte-for-byte, but the envelopes are distinct because they protect distinct concerns and may diverge in the future.

### Backup / restore data flow

```text
Backup:    identity.enc → read seed → harmony_owner::recovery encode → mnemonic | recovery-file
Restore:   mnemonic | recovery-file → harmony_owner::recovery decode → write seed → identity.enc
```

Both directions use the existing `identity.rs` keychain → encrypted-file resolution chain to read or write the seed. No bypass; the CLI commands are thin shims over `harmony_owner::recovery` + `identity.rs`.

## CLI surface

Five top-level subcommands on `harmony-app`:

```text
harmony-app rotate-passphrase --new-passphrase-file PATH        (existing, unchanged)
harmony-app export <FORMAT>                                     (new)
harmony-app restore <FORMAT> [--force]                          (new)
harmony-app help / --help / --version                           (clap default)
```

Where `<FORMAT>` is `mnemonic` or `recovery-file`.

```sh
harmony-app export mnemonic
harmony-app export recovery-file --out PATH [--comment STRING]

harmony-app restore mnemonic --mnemonic-file PATH [--force]
harmony-app restore recovery-file --in PATH [--force]
```

### Per-command I/O contract

| Command | Reads | Writes | Stdout | Stderr |
|---|---|---|---|---|
| `export mnemonic` | `identity.enc` (via `HARMONY_PASSPHRASE`/`_FILE`) | nothing | bare 24 words on a single line | warning preamble + `identity-hash: <hex32>` |
| `export recovery-file --out PATH [--comment S]` | `identity.enc` (at-rest passphrase) + `HARMONY_RECOVERY_PASSPHRASE`/`_FILE` | `PATH` (encrypted recovery file) | nothing | `wrote <PATH> (<NN> bytes)\nidentity-hash: <hex32>` |
| `restore mnemonic --mnemonic-file PATH [--force]` | `PATH` (24 ASCII words, permissive whitespace) + `HARMONY_PASSPHRASE`/`_FILE` (for re-encrypt) | `identity.enc` (at-rest passphrase) | nothing | `restored identity-hash: <hex32>` |
| `restore recovery-file --in PATH [--force]` | `PATH` (encrypted recovery file) + `HARMONY_RECOVERY_PASSPHRASE`/`_FILE` (for decrypt) + `HARMONY_PASSPHRASE`/`_FILE` (for re-encrypt) | `identity.enc` (at-rest passphrase) | nothing | `restored identity-hash: <hex32>` |

### Mnemonic stdout shape (export)

Stdout: bare single line of 24 ASCII words separated by single spaces, terminated with a single `\n`. Example:

```text
abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art
```

Stderr (when interactive — the operator sees this; redirection to a file does not capture stderr by default):

```text
*** Identity recovery mnemonic ***
Write these 24 words on paper. Anyone with these
words can impersonate you. Storing in a digital
file is dangerous.

identity-hash: 1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d
```

Stdout/stderr separation is the load-bearing UX: `harmony-app export mnemonic > backup.txt` writes only the words to the file (no warning text leakage); running interactively shows the warning + fingerprint on the terminal. The fingerprint is the operator's confirmation that the backup is for the identity they expect.

### Mnemonic file format (restore)

`restore mnemonic --mnemonic-file PATH` accepts any whitespace-separated 24 ASCII words. The harmony-owner mnemonic decoder is already permissive (whitespace-tolerant, case-insensitive, ASCII-only — non-ASCII rejected outright). Multi-line, mixed case, leading/trailing whitespace, single-line, indented — all valid. Matches what `export mnemonic > backup.txt` writes AND what someone typing from paper would naturally produce.

### Restore policy when an identity already exists

Both restore subcommands check whether `~/.harmony/identity.enc` exists at the start. If it does and `--force` is NOT passed, the command fails with:

```text
Error: identity already exists at <path>; pass --force to overwrite (this is destructive)
```

Process exits 1. With `--force`, the file is overwritten in place via the existing atomic `create_new` tmp-then-rename pattern in `identity.rs:save_with_fallback`.

This is the standard CLI convention (`cp -f` etc.) and avoids reintroducing the `.bak` cleanup tangle that ZEB-174 explicitly removed.

### Identity-hash display

Full 32-character hex of the master `identity_hash()` (a `[u8; 16]` per `harmony_owner::pubkey_bundle`), computed by `RecoveryArtifact::from_seed(seed).master_pubkey_bundle().identity_hash()`. Format: `identity-hash: <32-hex-chars>`. Easy to copy-paste, diff, grep; no extra deps; matches `git`-style hash display. Operator's eyeball-comparison fingerprint for round-trip verification.

This deliberately uses `harmony_owner`'s owner-identity-hash function rather than computing a hash directly from the harmony-client `NodeIdentity` Ed25519 / ML-KEM public-key bytes. Reasoning: the seed is a deterministic function of the artifact, the owner-identity-hash is a deterministic function of the seed, so identical seeds always produce identical fingerprints — which is exactly the eyeball-comparison invariant we want for round-trip verification. Using a per-device hash would conflate seed-equivalence with device-identity, which doesn't add value in a model where seeded keygen makes the keys themselves a deterministic function of the seed.

### Error reporting

`RecoveryError` variants surface verbatim via `Display`. Examples:

```text
Error: expected 24 BIP39 words, got 23
Error: unknown word at position 7: "harmonny"
Error: mnemonic checksum mismatch — likely a typo somewhere in the 24 words
Error: wrong passphrase or corrupted recovery file (AEAD tag rejected)
Error: recovery file is too small (50 bytes; minimum 69)
```

`WrongPassphraseOrCorrupt` is already deliberately ambiguous inside the library (the AEAD does not — and should not — distinguish the two cases). All other variants are operator-actionable diagnostics; pass-through is the only sensible UX for a tool the operator runs themselves.

Process exits 1 on any error.

## Components & code organization

Four files change in `harmony-client/src-tauri/src/`:

### `Cargo.toml` — add the harmony-owner dependency

```toml
harmony-owner = { git = "https://github.com/zeblithic/harmony.git", branch = "main", features = ["recovery"] }
```

The `recovery` feature is default-on in `harmony-owner` post ZEB-175. Listing it explicitly matches the existing `harmony-runtime` / `harmony-identity` / etc. dep declarations and pins the surface we need.

### `src/identity.rs` — adapt to seed-based storage

Five targeted changes, scoped tightly:

1. Replace 161-byte `identity_to_blob` / `blob_to_identity` with 32-byte `seed_to_blob` / `blob_to_seed`. Update constants: `BLOB_LEN: usize = 32`, `ENC_FILE_LEN: usize = 101`.
2. Replace fresh-generate path (`identity.rs:934-937`) with: generate 32B seed via `OsRng.fill_bytes()` → call `NodeIdentity::from_seed(&seed)` → save the seed.
3. Add `NodeIdentity::from_seed(seed: &[u8; 32]) -> Self` — thin wrapper over the new ZEB-177 upstream constructors.
4. Add `pub(crate) fn read_seed_from_disk(plaintext_path: &Path) -> Result<Zeroizing<[u8; 32]>, String>`. Re-uses the existing keychain → encrypted-file → fail resolution chain — but stops at "got the seed bytes" instead of continuing to derive `NodeIdentity`. The existing `load_or_generate` becomes a thin wrapper over `read_seed_from_disk` + `NodeIdentity::from_seed`.
5. Add `pub(crate) fn write_seed_to_disk(plaintext_path: &Path, seed: &[u8; 32], force: bool) -> Result<(), String>`. Refuses with `RestoreRefusedExistingIdentity` if the destination exists and `force == false`. With `force = true`, overwrites in place via the existing atomic `create_new` tmp-then-rename pattern.

### `src/recovery_cli.rs` (new file) — CLI subcommand entry points

```rust
pub fn export_mnemonic_cli(plaintext_path: &Path) -> Result<(), String>;
pub fn export_recovery_file_cli(
    plaintext_path: &Path,
    out: &Path,
    comment: Option<&str>,
) -> Result<(), String>;
pub fn restore_mnemonic_cli(
    plaintext_path: &Path,
    mnemonic_file: &Path,
    force: bool,
) -> Result<(), String>;
pub fn restore_recovery_file_cli(
    plaintext_path: &Path,
    in_path: &Path,
    force: bool,
) -> Result<(), String>;
```

Each one composes `identity::read_seed_from_disk` / `identity::write_seed_to_disk` with the relevant `harmony_owner::recovery` API. Also contains the recovery-passphrase resolver (`HARMONY_RECOVERY_PASSPHRASE` / `_FILE`) — structurally identical to the existing at-rest resolver in `identity.rs` but kept separate intentionally so neither env var falls back to the other.

Tests pass an explicit `plaintext_path` rather than reading `HOME` directly — `main.rs` resolves the real path via the existing `identity::resolve_path` before calling. This makes `recovery_cli` unit-testable against `tempdir()`-rooted paths.

### `src/main.rs` — wire the clap subcommands

Extend the existing `Command` enum with two new variants:

```rust
#[derive(Subcommand, Debug)]
enum Command {
    RotatePassphrase { ... },                        // existing
    Export {
        #[command(subcommand)]
        format: ExportFormat,
    },
    Restore {
        #[command(subcommand)]
        format: RestoreFormat,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ExportFormat {
    Mnemonic,
    RecoveryFile {
        #[arg(long)] out: PathBuf,
        #[arg(long)] comment: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum RestoreFormat {
    Mnemonic {
        #[arg(long)] mnemonic_file: PathBuf,
    },
    RecoveryFile {
        #[arg(long, name = "in")] in_path: PathBuf,
    },
}
```

Dispatch into `recovery_cli` in the `Some(Command::Export { ... })` / `Some(Command::Restore { ... })` arms. The existing `RotatePassphrase` arm and the GUI fallthrough are unchanged. ~40 lines added total.

### `src/lib.rs` — module declaration

Add `pub mod recovery_cli;`.

## Documentation updates

### `docs/headless-install.md` — Backup and recovery section

Replace the current "Backup and recovery" section (which currently points at ZEB-175 as a future ticket) with the actual command reference:

- A worked example for both export commands (mnemonic to stdout, recovery-file to disk).
- A worked example for both restore commands.
- Document the new `HARMONY_RECOVERY_PASSPHRASE` / `HARMONY_RECOVERY_PASSPHRASE_FILE` env vars.
- The `--force` flag and what it overwrites.
- Note that mnemonic export + paper backup is the recommended primary recovery path; recovery-file is the secondary path for "I want to copy this to a USB stick."

The existing "treat identity.enc and your passphrase as two halves of a recovery key" warning stays; the recovery commands give an additional layer.

## Testing strategy

### Unit tests in `recovery_cli.rs`

Cover the CLI plumbing without touching real `~/.harmony/`. Each helper accepts `&Path` rather than reading `HOME` directly; tests pass a `tempdir()`-rooted path.

- `export_recovery_file_with_metadata` — encode → file exists → file decodes back to same seed.
- `restore_mnemonic_idempotent` — generate seed, export mnemonic, write to tmp file, restore into a fresh path, assert seed identity.
- `restore_refuses_when_identity_exists_without_force` — touch the destination, attempt restore, assert the refusal error.
- `restore_with_force_overwrites_existing` — same setup but `force = true`, assert success and that the new seed is loaded.
- `recovery_passphrase_env_var_resolution` — set `HARMONY_RECOVERY_PASSPHRASE_FILE` to a tmp file, attempt export-recovery-file, assert success.
- `recovery_passphrase_neither_set_fails_with_pointer_to_docs` — neither env var → hard error pointing at the docs.

### Integration tests in `tests/recovery_cli_integration.rs`

Exercise the full pipeline against a real tempdir-rooted identity store.

- `mnemonic_round_trip_preserves_identity_hash` — generate → export mnemonic → wipe identity store → restore from mnemonic → recompute `RecoveryArtifact::from_seed(restored_seed).master_pubkey_bundle().identity_hash()` → assert it matches the pre-export hash.
- `recovery_file_round_trip_preserves_identity_hash` — same shape, recovery-file path: generate → export recovery-file → wipe identity store → restore from recovery-file → recompute `RecoveryArtifact::from_seed(restored_seed).master_pubkey_bundle().identity_hash()` → assert it matches the pre-export hash.
- `cross_encoding_equivalence_via_cli` — export both ways, restore via mnemonic, verify hash; restore via recovery file, verify hash; both equal to original. Mirrors the equivalence test that already exists in `harmony_owner::recovery::equivalence_tests`.

### Wire-format regression

None needed in `harmony-client`. ZEB-175 already pins the recovery-file wire format byte-for-byte upstream (`recovery_v1.bin`, `recovery_v1_no_metadata.bin`); the harmony-client side inherits that guarantee transitively.

The local `HRMI` envelope wire format gets a payload-length change (161 → 32 bytes). The existing `identity.rs` tests should be updated to reflect the new `BLOB_LEN`; no new fixture file is needed because `HRMI` is not a portable artifact (no cross-machine interop concern).

### `identity.rs` test adjustments

- Existing `BLOB_LEN`-using tests get the value updated 161 → 32.
- New: `seed_round_trip_via_blob` — write seed → load seed → assert byte-identical.
- New: `from_seed_yields_same_NodeIdentity_across_launches` — calls `NodeIdentity::from_seed` twice with the same seed, asserts both produce identical `to_private_bytes()` output. Belt-and-suspenders alongside the upstream ZEB-177 determinism test.

### Out of scope for this ticket

- GUI flow tests (no GUI surface added).
- Multi-machine cross-platform fixture (the recovery-file fixture upstream already covers byte-level wire stability; cross-machine portability is implied).
- Verifying behavior on existing pre-ZEB-176 identities — explicit migration is out of scope; users with prior identities re-mint.

## Definition of done

1. All four subcommands work end-to-end against `~/.harmony/identity.enc`.
2. Round-trip: export mnemonic → wipe → restore mnemonic → derived `identity_hash` matches the original.
3. Round-trip: export recovery-file → wipe → restore recovery-file → derived `identity_hash` matches the original.
4. Cross-encoding equivalence: export both ways, restore either, identity hash equal in both directions.
5. Headless mode (`HARMONY_PASSPHRASE` / `_FILE` for at-rest, `HARMONY_RECOVERY_PASSPHRASE` / `_FILE` for backup file) works for both export and restore.
6. `--force` flag is required to overwrite an existing identity.
7. `docs/headless-install.md` Backup and recovery section updated with the new commands and worked examples.
8. CLI surface is documented in `harmony-app help` output.
9. ZEB-177 has landed (hard prerequisite — this ticket cannot ship before).

## Open questions resolved during brainstorming

For posterity, the decisions captured here resolve:

1. **Foundational scope** — Path B: add seeded keygen upstream, switch client to seeded model. (Path A: backup the 161B blob, was rejected because it gives up the BIP39-24 mnemonic option since 1288 bits doesn't fit in a 264-bit BIP39 budget.)
2. **Slicing** — A: file ZEB-177 as a hard prerequisite, brainstorm the client slice as ZEB-176.
3. **Storage shape** — A: store only the 32-byte seed, derive on every load. (Bitcoin-wallet model.)
4. **Command structure** — B: action-grouped (`export <FORMAT>` / `restore <FORMAT>`).
5. **Recovery-file passphrase source** — B: new env vars `HARMONY_RECOVERY_PASSPHRASE` / `HARMONY_RECOVERY_PASSPHRASE_FILE`.
6. **Restore policy** — B: refuse + `--force` to overwrite.
7. **Mnemonic stdout shape** — C: bare to stdout, warning + identity-hash to stderr.
8. **Mnemonic file format** — A: permissive (use harmony-owner's existing decoder).
9. **Identity-hash display** — A: full 32-char hex.
10. **Error message granularity** — A: pass-through `RecoveryError` `Display` strings verbatim.
