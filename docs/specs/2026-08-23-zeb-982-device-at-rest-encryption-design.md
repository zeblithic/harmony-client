# ZEB-982: Device-sealed at-rest encryption — owner-state family + keyless-boot peripherals

Ticket: ZEB-982. Follow-up to ZEB-981 (PR #727), which sealed the 8
mirror-family fleet datasets under the pinned epoch-0 fleet KeyTree via
`fleet_dataset_file.rs`. This design covers the files that ZEB-981 could
not: the owner-state family and the peripheral datasets that are written
on boots where **no fleet KeyTree exists**.

## Problem

The owner-state family persists as plaintext CBOR in the identity dir:
`owner_state.cbor` (trust graph, friends, device bindings),
`owner_state_crdt.cbor` + `state_root_replay.cbor` (spaces, inbox/outbox
CRDT), and the trust/quorum sidecars. Peripheral datasets in the same
exposure class are also plaintext: `outbound_friend_links.cbor` (who you
asked to befriend), `profile_cards.{owner}.cbor` (peer roster with names
and status text), `mint_sync_state.cbor` (finance-sync device topology).
Anyone with filesystem access (backup tooling, sync folders, a stolen
unlocked laptop image) reads them.

ZEB-981's KeyTree-bound cipher cannot cover these files, for structural
reasons verified against the code:

1. **Keyless boots write them.** The owner-state engine runs in
   local-only mode with `keys: None` — "persistence is key-free" by
   design (`owner_state_sync.rs:113-117`, ZEB-905). The community
   registry, friend-links store, and card store are all constructed
   outside the fleet band (`lib.rs:7969+`, `lib.rs:14632`,
   `lib.rs:13453`); only mint sync is inside it.
2. **The recovery CLI writes them with no node at all.**
   `recovery_cli.rs:825` writes `owner_state_crdt.cbor` from an HRSS
   snapshot in a process with no `NodeState` and no KeyTree.
3. **Schema-byte collision.** `CRDT_FILE_SCHEMA_V2 == 2 ==
   SEALED_SCHEMA_V2` (`owner_state_persist.rs:76`,
   `fleet_dataset_file.rs:29`). `fleet_dataset_file::load_impl` matches
   the sealed sentinel first, so pointing it at an existing plaintext
   CRDT file would AEAD-fail and — under `load_or_recover` — quarantine
   the user's spaces/inbox/outbox to `OwnerState::default()` on the
   first post-upgrade boot. Silent data loss.
4. **The quarantine contract does not transfer.** `load_or_recover`'s
   discard-and-default is justified by fleet re-sync from peers. A
   keyless local-only device has no peers; for it, discard is permanent
   loss. The owner family's boot-fatal-but-recoverable contract
   (`lib.rs:4804`, `lib.rs:5967`) must survive encryption unchanged.

## Threat model

**Defends against:** passive filesystem readers (backup tooling, cloud
sync, disk images, other local users) reading user content, the social
graph, and moderation/trust history; cross-file ciphertext swap
(filename-bound AAD); cross-owner card-store swap (the filename embeds
the owner id hex, and the AAD binds the filename).

**Deliberately not defended:** attackers who can *write* the filesystem
(they can delete or roll back files regardless); residual plaintext in
unallocated sectors after migration; readers who can also extract the
node identity master seed. The protection floor equals the seed's own
at-rest protection — OS keychain on desktop, `HARMONY_PASSPHRASE`
envelope headless, and obfuscation-only where the seed itself is a
plaintext identity file. That floor is identical to the one the
device's identity keys already live at, and no file sealed here can be
protected more strongly than the identity that owns it.

## Decisions (approved 2026-08-23)

1. **Key source: seed-derived device key.** One 32-byte
   ChaCha20-Poly1305 key derived from the node identity master seed via
   HKDF-SHA256 (the crate's established KDF — `KeyTree::derive_at_epoch`
   pattern) with a dedicated salt/info domain. The seed resolves via the
   existing chain (keychain → `identity.enc` → generate) at
   `identity::load_or_generate` / `read_seed_from_disk_with_keychain`,
   is available strictly before every reader/writer in scope (boot loads
   it ~100 lines before owner state; `recovery_cli` already reads it),
   and exists in every mode including keyless boots. No new stored
   secret, no vault schema change, no plaintext fallback mode, no
   KeyTree dependency. Files sealed under it are device-local artifacts,
   which matches what they are: every device materializes its own copies.
2. **Scope: owner family + small peripherals.** See the scope table.
   Community family, channel logs, mail, and `ledger.db` are follow-up
   tickets (each needs its own recovery-story design).
3. **Hot readers: process-cached key.** The keychain-free readers
   (`read_persisted_owner_id`, `read_enrolled_device_vk_hex` — the
   latter documented as safe on hot paths like `set_butler_pin`) decrypt
   via a derived-once cipher. Explicit threading is preferred
   everywhere a construction site exists; a memoized
   `get_or_derive(identity_dir)` helper covers free-function call
   sites. The memo is keyed by canonicalized identity-dir path, so
   multi-profile processes and multi-test binaries never cross keys.
4. **Fold two adjacent gaps in:** delete plaintext
   `friend_nicknames.json.migrated` once the sealed contacts file loads
   verified (ZEB-981 leftover, `contacts_commands.rs:293-294`); add
   `owner_trust_replay.cbor` to `OWNER_RESET_FILES`
   (`owner_commands.rs:1940-1949`), where it is missing today.
5. **Rollback stance (accepted):** an old binary reading a sealed
   `owner_state.cbor` fails boot with the existing "corrupt" error —
   loud, recoverable by re-upgrading or Reset-this-device. Unlike the
   mirror datasets there is no peer re-sync to soften this, and lazy
   sealing would only stagger the same outcome. Rollback across a
   storage-format change being unsupported is the normal contract; the
   spec documents it rather than engineering around it.

## Architecture

### New module `src-tauri/src/device_dataset_file.rs`

A sibling of `fleet_dataset_file`, one layer lower architecturally: it
owns **only the envelope**, never the load/recover pipeline. Families
keep their own parsing, quarantine, and recovery semantics operating on
the inner image — contracts are preserved by construction.

**Envelope (v3):**

```
[0x03] ‖ nonce(12) ‖ ChaCha20-Poly1305(inner image) ‖ tag(16)
```

- AAD = `b"zeb-982-device-at-rest:v3:" ‖ filename` — a new domain,
  separated from ZEB-981's `zeb-981-dataset-at-rest:v2:` and the
  file-DEK/friend-secret domains.
- Sentinel `0x03` is collision-proof against every legacy first byte in
  scope (verified: schema bytes `1`/`2`; bare-CBOR map headers `0xA0+`
  for `owner_state.cbor` and `mint_sync_state.cbor`; array headers
  `0x80+` for `outbound_friend_links.cbor`). It also marks the key
  domain: `0x02` = sealed under fleet KeyTree, `0x03` = sealed under
  device key. A file handed to the wrong module fails loudly at the
  version byte, not as a confusing AEAD failure. `0x03` is reserved
  forever; plaintext schema numbering for these files is frozen at ≤ 2
  (after sealing ships, saves are always sealed, so plaintext formats
  never evolve again).
- Random 12-byte nonce per write (`OsRng`), as in ZEB-981.

**Key derivation:**

```
HKDF-SHA256(salt = b"zeb-982-device-dataset-salt",
            ikm  = node identity master seed (32 bytes))
  .expand(b"device-dataset-aead", 32)
```

Golden-pin the derivation with a fixture test (seed of `[7u8; 32]` →
pinned key bytes) so it can never silently drift.

**Cipher type and acquisition:**

```rust
#[derive(Clone)]
pub struct DeviceCipher { key: Arc<Zeroizing<[u8; 32]>> }

impl DeviceCipher {
    /// Derive from a seed already in hand (boot, recovery CLI, tests).
    pub fn derive(seed: &[u8; 32]) -> Result<Self, CryptoError>;
}

/// Memoized derive for free-function call sites that cannot be threaded.
/// Memo keyed by canonicalized identity-dir path. Test builds hit the
/// same ZEB-428 gates as any seed read (KeychainStore::new() refuses;
/// HARMONY_PASSPHRASE / plaintext fallback applies).
pub fn get_or_derive(identity_dir: &Path) -> Result<DeviceCipher, String>;

/// Drop the memo entry for `identity_dir`. Wired into
/// `identity::write_seed_to_disk_with_keychain` (the seed-write choke
/// point): the memo is keyed by directory, not seed value, so a recovery
/// restore that rewrites the seed would otherwise keep sealing under the
/// pre-restore key and strand every sealed file.
pub fn invalidate(identity_dir: &Path);

#[cfg(any(test, feature = "test-fixtures"))]
pub fn test_cipher() -> DeviceCipher;   // DeviceCipher::derive(&[7u8; 32])

/// Pre-populate the memo with `test_cipher` for a fixture dir, so tests of
/// the free-function owner-state paths need no identity store. The memo is
/// per-directory — fixtures using subdirs install for each.
#[cfg(any(test, feature = "test-fixtures"))]
pub fn install_test_cipher(identity_dir: &Path);

/// The full sealed on-disk byte form (sentinel-prefixed), for families
/// that keep their own write primitive (owner_state.rs's 0600 writes).
pub fn seal_image(cipher, filename, inner) -> Result<Vec<u8>, String>;
```

In-app, the cipher is derived at boot just before `load_owner_state` and
threaded to every construction site; the derive also warms the process-wide
memo for the free-function readers. `recovery_cli` restore derives directly
from the just-written seed (never through the memo the seed write just
invalidated).

**Lazy acquisition (implementation refinement):** callers that read files
which may still be legacy plaintext (the `owner_state.rs` free functions)
acquire the cipher only when the file's first byte IS the sentinel. Legacy
files therefore parse with no identity store at all — pre-982 behavior for
store-less fixtures, and a structural guarantee that a pure read never
fresh-generates a node identity as a side effect. A sealed file implies the
device had a working store when it sealed it, and a derive failure there
surfaces as "sealed but the device key is unavailable", never "corrupt".

**Lock ordering (hard constraint, discovered as a live deadlock):** the
seed read behind `get_or_derive` can take `IDENTITY_FILE_WRITE_LOCK` (the
fresh-generate path via `with_identity_write_guards`), so the cipher MUST
be derived BEFORE that lock is acquired — `save_owner_state_atomic`
hoists the derive above its guard — and `get_or_derive` never holds its
own memo lock across the seed read. Documented at both acquisition sites.

**Envelope API (image-level, not value-level):**

```rust
pub enum ImageError {
    /// Read-side I/O (NotFound is NOT an error — see `read_image`).
    Io(std::io::Error),
    /// AEAD failure, truncated envelope, oversize file: content-corrupt.
    Crypto(String),
}

pub struct Image {
    pub bytes: Zeroizing<Vec<u8>>,  // the inner (legacy-format) image
    pub was_legacy: bool,           // true → file was plaintext on disk
}

/// None = file absent. Legacy (first byte ≠ 3) → whole file is the image.
/// Sealed → decrypt. Metadata size cap (256 MiB) checked before read.
pub fn read_image(cipher, path, filename) -> Result<Option<Image>, ImageError>;

/// Seal `inner` and write atomically (delegates to
/// `owner_state_persist::save_atomically`, as fleet_dataset_file does).
pub fn write_image(cipher, path, filename, inner: &[u8]) -> Result<(), std::io::Error>;

/// Best-effort eager migration: if `was_legacy`, re-seal the image
/// byte-for-byte; on write failure warn and leave plaintext in place.
pub fn reseal_if_legacy(cipher, path, filename, image: &Image);
```

The `Io`/`Crypto` split is deliberate: it maps directly onto
`persistent_card_store`'s freeze discrimination and onto each family's
existing corrupt-vs-transient branches, so every family can classify an
envelope failure exactly the way it classifies the equivalent plaintext
failure today.

Crypto primitives (`seal_device_file`/`open_device_file`) live in
`owner_state_crypto.rs` beside `seal_dataset_file`/`open_dataset_file`,
taking `&DeviceCipher` instead of `&KeyTree`, sharing the nonce/AAD
layout code where practical.

## Scope

| Family | Files (dir) | Contract preserved |
|---|---|---|
| Owner state doc | `owner_state.cbor` (identity) | Hard error; boot-fatal on corrupt (`lib.rs:4804`); missing = un-minted |
| Owner CRDT | `owner_state_crdt.cbor`, `state_root_replay.cbor` (identity) | Hard error; boot-fatal (`lib.rs:5967-5970`); V1→default warn path unchanged inside image |
| Trust sidecar | `owner_trust_replay.cbor` (identity) | Warn + default on I/O; quarantine-aside on corrupt |
| Quorum sidecars | `owner_quorum_req.cbor`, `owner_quorum_replay.cbor` (identity) | Quarantine-aside `.corrupt-{ms}` + default |
| Friend links | `outbound_friend_links.cbor` (identity) | Quarantine-aside; ephemeral path-less store if rename fails |
| Profile cards | `profile_cards.{owner}.cbor` (app-data) | Freeze-writes on read-I/O; self-heal on content corrupt; never blocks boot |
| Mint sync | `mint/mint_sync_state.cbor` (app-data) | Hard error → feature disarm (`break 'mint_init`); `SchemaTooNew` unchanged inside image |

All seven families use the **device cipher**, including mint sync (which
is inside the fleet band and could reach the fleet cipher): one key
story per module, and mint's files are device-local too.

**Writer inventory covered** (from the design exploration): both
`owner_state.rs` save functions and all seven production call sites of
`save_owner_state_cbor_only` (mint, liveness refresh, trust engine sink,
FileOnly trust mutation, pairing joiner/inviter, mnemonic restore); the
owner-state engine's `OwnerStatePersist` sink; `recovery_cli` export
(`:644`) and restore (`:825` — restore now writes sealed; the HRSS
sidecar's `snap.tree` remains the inner image, so existing `.hrmr`
backups stay valid); `backup_state::staleness_from_dir`;
`friend_requests::persist`; `persistent_card_store::persist`; the three
mint-sync writers.

## Per-family integration

Each `*_persist` load/save keeps its signature shape and gains a
`&DeviceCipher` parameter (mirroring ZEB-981's threading of
`&DatasetCipher`); internally the raw `fs::read`/`save_atomically` pair
is replaced by `read_image`/`write_image`, and the existing
schema-byte/CBOR handling runs against `image.bytes`:

- **`owner_state.rs`**: `save_owner_state_atomic` /
  `save_owner_state_cbor_only` seal the canonical CBOR; the vault-first,
  cbor-last write ordering and both locks are untouched. Loaders map
  `ImageError::Crypto` to the existing
  `"owner_state.cbor is corrupt: …"` string family so boot behavior and
  the ZEB-835/836 reset escape are byte-for-byte the same experience.
  The keychain-free readers gain `_with_cipher` variants (mirroring the
  `_with_keychain` seam pattern); their convenience wrappers use
  `get_or_derive`.
- **`owner_state_persist.rs`**: `load_crdt`/`load_replay` first byte `3`
  → envelope; `1`/`2` → existing legacy paths (V1 silent-discard warn
  preserved verbatim). `save_crdt`/`save_replay` seal.
- **`owner_trust_sync.rs` / `owner_quorum_sync.rs`**: envelope beneath
  the existing quarantine helpers. Quarantined bytes are now ciphertext
  — which also closes the "quarantine drops plaintext" leak for these
  files going forward (legacy plaintext quarantine only ever happens to
  files that were already plaintext on disk).
- **`friend_requests.rs`**: envelope inside `load_or_recover` before
  `decode`; the ephemeral-store-on-rename-failure subtlety untouched.
- **`persistent_card_store.rs`**: `ImageError::Io` → the existing
  `PersistError::Io` freeze arm; `ImageError::Crypto` → the content arm
  (no freeze, next flush self-heals). The freeze contract is the pin
  test for this family.
- **`mint_sync_persist.rs`**: envelope around the bare-CBOR image;
  `MintSyncError` variants unchanged.
- **`lib.rs`**: derive `DeviceCipher` once post-identity-load; thread to
  the owner-state engine sink (`OwnerStatePersist`), the trust and
  quorum sinks, the card store, mint init, the friend-links store, and
  the command paths that load/save owner state directly.

## Migration & rollback

- **Eager, byte-lossless.** On first load of a legacy file, the exact
  on-disk image is sealed and rewritten (`reseal_if_legacy`); reseal
  write failure warns and leaves the plaintext in place (next boot
  retries). No re-serialization anywhere — the ZEB-981 CR-2 lesson.
- **Pre-982 binary + sealed file:** owner family → loud boot failure
  (Decision 5); quarantine families → quarantine + rebuild;
  profile-cards/mint → warn-empty / disarm. No silent misreads anywhere:
  sentinel 3 never parses as CBOR or as schema 1/2.
- **`OWNER_RESET_FILES`** needs no change for sealing (filename-only,
  in-place format change) — the fold-in *adds* `owner_trust_replay.cbor`
  to it. Reset-backup dirs will contain sealed copies going forward,
  which strictly improves on today's plaintext copies.
- **`friend_nicknames.json.migrated` cleanup:** after a successful load of
  sealed `contacts.cbor` **with at least one contact entry**, delete the
  leftover file; absence is not an error. The non-empty guard matters:
  `load_doc_or_recover` quarantine-defaults on corruption, and deleting the
  plaintext backup after a quarantine reset would destroy the last copy of
  the nicknames. An empty legacy import leaves an empty leftover — nothing
  protected, nothing leaked.

## Out of scope (follow-up tickets, filed at PR time)

- **Community family** (`crdt.cbor`, `replay.cbor`, `segments.cbor`,
  `voting.cbor`, `addrbook.cbor`, backfill state) + **channel logs**
  (`tail.cbor`, `segments/*.cbor` — full chat bodies in the clear).
  Blocked on a recovery-story design: the channel log hard-errors and
  never spawns the engine on a bad file, which under encryption turns
  one undecryptable tail into a silently dead community. Registry needs
  cipher threading (`CommunityRegistryConfig` has no cipher field).
- **Mail** (`mail/blobs/*.bin`, `mail/index.json`) — plaintext bodies,
  plus a missing integrity check on blob read.
- **`mint/ledger.db`** — plain SQLite; WAL sidecars rule out whole-file
  sealing; needs SQLCipher-or-equivalent.
- **`pre_fork_snapshot.bin`** — includes message bodies.
- **JSON persistence hardening sweep** — `follows.json` silent
  self-destruction on corrupt, `vine_feed.json`/`vine_pull.cbor`/
  `content-index.json` warn-empty contracts, atomicity uniformity onto
  `save_atomically`, 0600 permissions, freeze-contract generalization.
- **Log hygiene** (`logs/harmony.log.*` carries peer ids and paths and
  is excluded from clean-slate wipe) and `_reset-backup-*` retention.
- **D-FROST Phase 4b** sealed key-package storage should adopt this
  module when it lands (`community_dfrost_log.rs:51-55`).
- Downgrade-block latch (v1 rejection after first seal) — carried
  forward from ZEB-981, still future hardening.

## Testing

**Module tests (the contract lives here):**

- golden-pinned key derivation (fixed seed → pinned key bytes)
- round-trip: `write_image` → `read_image`, `was_legacy == false`
- legacy file (schema-1, schema-2, and bare-CBOR-map first bytes) →
  image returned verbatim, `was_legacy == true`; `reseal_if_legacy`
  rewrites sealed and the sealed file opens to the identical image
- reseal write failure (read-only parent, root-probe skip as in ZEB-981)
  → image still returned, plaintext left in place
- filename-swap AAD rejection; foreign-seed rejection; tamper → `Crypto`
- truncated envelope (< 1+12+16 bytes) → `Crypto`, clean
- transient I/O (path is a directory) → `Io`, never `Crypto`
- 256 MiB metadata cap (sparse `set_len` file) → refused before read
- missing file → `Ok(None)`

**Per-family contract pins (the point of the layering):**

- owner CRDT: sealed-corrupt file → load error propagates (boot-fatal
  preserved), **no quarantine artifact created**
- collision regression: a legacy CRDT-V2 file (first byte `2`) routes to
  the legacy parser, never the envelope; a sentinel-`3` file routes to
  the envelope, never schema parsing
- quorum: sealed-corrupt → `.corrupt-{ms}` quarantine + default (aside
  file contains ciphertext)
- profile cards: envelope `Io` → writes frozen; envelope `Crypto` → not
  frozen, next persist rewrites sealed
- mint: sealed-corrupt → `MintSyncError`, engine disarm path unchanged
- friend links: quarantine + ephemeral-on-rename-failure preserved
- keyless-boot integration: seed present, no KeyTree → owner CRDT
  save/load round-trips sealed (`test_cipher` threading mirrors ZEB-981)
- recovery_cli: export from sealed CRDT + restore writes sealed;
  `.hrmr` fixture from a pre-982 export still restores
- migration: each family's `legacy_plaintext_*_migrates_to_sealed` test
  (as in ZEB-981), byte-losslessness asserted via re-open
- `read_persisted_owner_id`/`read_enrolled_device_vk_hex` `_with_cipher`
  variants read sealed files; ZEB-428 keychain gates untouched
  (`tests/keychain_isolation.rs` still pins the constructor)
- fold-ins: `.migrated` deleted after verified sealed-contacts load
  (and only then); `OWNER_RESET_FILES` includes `owner_trust_replay.cbor`

**Gates:** `cd src-tauri && cargo nextest run --locked --workspace
--all-targets --features test-fixtures`; clippy
`--all-targets --features test-fixtures -D warnings`; `cargo fmt --all
-- --check`; `npx tsc --noEmit` + `npx vitest run` (no frontend changes
expected; gates run regardless).
