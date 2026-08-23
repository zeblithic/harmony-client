# ZEB-981: At-rest encryption for fleet-synced datasets — design

**Ticket:** [ZEB-981](https://linear.app/zeblith/issue/ZEB-981) · **Status:** approved design · **Date:** 2026-08-23

## Problem

Every fleet-synced personal dataset persists as **plaintext CBOR in the identity
dir** (`~/.harmony` or `~/.harmony/profiles/<name>`): `notes.cbor`,
`contacts.cbor`, `dm_inbox.cbor`, `relay_hold.cbor`, and the rest of the
mirror family, plus their replay trackers and tombstone sidecars. ZEB-977
raised the stakes: contact notes are private observations about *other people*,
and petnames reveal your private naming of identities. Anyone with filesystem
**read** access — backup tooling, cloud-sync folders, a stolen disk image —
reads them all.

Wire-side is already encrypted (fleet sync publishes sealed frames); this is
purely at-rest.

## Threat model

**Defended:**

- **Confidentiality against filesystem readers.** Backup tooling, sync
  folders, forensic images of an unlocked-at-rest disk: dataset content is
  ciphertext to them.
- **Integrity within encrypted (v2) files.** AEAD tags reject bit-rot and
  tampering; the AAD binds the file label, so ciphertext copied between
  dataset files (e.g. `contacts.cbor` content planted as `notes.cbor`) fails
  its tag.

**Deliberately not defended:**

- **An attacker with filesystem write access.** They can delete files
  outright, or plant a plaintext v1 file (v1 must remain parseable for
  migration and for restores from pre-981 backups, so a v1 downgrade is
  accepted). Closing this requires a keychain-latched "datasets are sealed"
  marker — a future hardening ticket, only if ever warranted.
- **Residual plaintext sectors.** Atomic-rename replacement leaves the old
  plaintext bytes on disk until the filesystem reuses them; secure erase is
  out of scope. Pre-existing `.corrupt-*` quarantine files and
  `friend_nicknames.json.migrated` also remain plaintext; they are not
  touched.

## Decisions (settled with Jake, 2026-08-23)

1. **Key source: pinned epoch-0 KeyTree, `friend_aead` sub-key + domain AAD**
   — the ZEB-674 `encrypt_file_dek` precedent. No `FleetKeyMaterial` wire or
   vault format change (a sixth sub-key would change the ZEB-492 pairing
   payload and be underivable for master-less quorum-bumped trees), no new
   stored secret and no new failure mode (a separate keychain-wrapped DEK
   would fail on unattended default-profile macOS boots where the login
   keychain is locked). The key inherits exactly the availability the
   datasets already require to sync at all. Epoch-independent like friend
   secrets → no multi-epoch decrypt candidates, and a tag failure is never
   "rotated epoch".
2. **Scope: all 10 mirror-family datasets in this PR**, via a new shared
   module (see table below). `owner_state.cbor` / `owner_state_crdt.cbor`
   stay out — they run in keyless local-only mode (ZEB-905) and are written
   by non-fleet paths; follow-up ticket.
3. **AEAD tag failure on load → quarantine + self-heal**, same policy as
   `CborDecode`. Bytes preserved in `.corrupt-<ms>`; fleet sync re-populates
   the content from peer devices, so self-heal recovers *data*, not just
   bootability. Transient I/O keeps propagating untouched (ZEB-460).

## Why this is safe on the boot path

All 23 dataset files (10 docs, 9 replay trackers, 4 sidecars) load
synchronously inside
`start_node_inner`'s fleet band (`lib.rs:6160`,
`if let Some((kt, keys)) = fleet_crypto`):

- **Key availability is guaranteed by existing ordering.** The band only
  opens when the KeyTree exists, and every target engine is constructed
  inside it. Local-only boots (no KeyTree) construct none of these engines
  and write none of these files — no plaintext fallback is needed.
- **Headless/unattended boot is the same code path** (`serve_cli` →
  `start_node_inner`); the KeyTree already resolves through the
  keychain-preferred → `HARMONY_PASSPHRASE` `.enc`-fallback chain that
  unattended deployments configure today.
- **No per-file KDF.** The key is HKDF-derived once at KeyTree construction;
  per-file work is one ChaCha20-Poly1305 pass (~29 bytes overhead). A
  per-file Argon2id would be 13 × 64 MiB serially on the critical path —
  ruled out.

## Crypto surface

Two free functions in `owner_state_crypto.rs`, mirroring
`encrypt_file_dek`/`decrypt_file_dek` (ZEB-674):

```rust
/// Domain-separated so a sealed dataset file can never be opened as a friend
/// secret or a file DEK, and vice versa. The label (the dataset's filename
/// constant, e.g. "notes.cbor") is bound into the AAD so ciphertext moved
/// between dataset files fails authentication.
const AAD_DATASET_AT_REST: &[u8] = b"zeb-981-dataset-at-rest:v2:";

/// Seal `plaintext` for at-rest storage. Layout: nonce(12) ‖ ct‖tag(+16),
/// ChaCha20-Poly1305 under the pinned epoch-0 tree's friend_aead sub-key,
/// AAD = AAD_DATASET_AT_REST ‖ label. Random nonce per seal (files are
/// wholesale-rewritten each persist; collision bound unreachable).
pub fn seal_dataset_file(keys: &KeyTree, label: &str, plaintext: &[u8]) -> Vec<u8>;

/// Open a sealed dataset file. Returns the inner bytes zeroized-on-drop.
/// Tag failure (corruption, cross-file swap, foreign-identity file) is a
/// CryptoError; callers map it to quarantine-and-self-heal.
pub fn open_dataset_file(keys: &KeyTree, label: &str, sealed: &[u8])
    -> Result<Zeroizing<Vec<u8>>, CryptoError>;
```

Primitive: `harmony_crypto::aead::{encrypt, decrypt, generate_nonce}`
(ChaCha20-Poly1305, 12-byte nonce — the house layout used by every other
KeyTree surface). Implementation reuses the `friend_aead` accessor pattern
internal to `owner_state_crypto.rs`; key bytes never leave the module.

## On-disk format

- **v1 (legacy, read-only):** `[0x01] ‖ plaintext CBOR` — today's format for
  every dataset, still parsed on load, never written again.
- **v2 (written):** `[0x02] ‖ nonce(12) ‖ AEAD(inner) ‖ tag(16)` where
  **inner = the complete former v1 file bytes** (`[0x01] ‖ CBOR`).

Nesting the whole v1 image as the envelope plaintext keeps encryption and
content-schema evolution orthogonal: a future content bump changes the
*inner* version byte and never touches the envelope; migration is "seal the
bytes you already have"; and the inner parse path (including the
trailing-bytes rejection) is byte-identical pre- and post-encryption.

The outer version byte `0x02` is uniform across all datasets. A pre-981
binary reading a v2 file hits its existing unknown-version path →
quarantine + default + fleet re-sync — rollback degrades to the existing
self-heal, never a brick.

## Shared module: `src/fleet_dataset_file.rs`

The persistence code is currently copy-pasted across ten modules
(`atomic_write` / `load` / `save` / `quarantine` / `load_*_or_recover` /
replay variants, byte-for-byte parallel). The new module absorbs that
duplication **once**, generic over the file struct:

```rust
/// Sealing context shared by every dataset persist: the pinned epoch-0
/// KeyTree. Cheap to clone (Arc).
pub struct DatasetCipher { keys: Arc<KeyTree> }

/// Load a dataset file: NotFound → default; v1 → parse plaintext, then
/// eagerly re-save as v2 (see Migration); v2 → open envelope, parse inner.
/// CborDecode/tag-failure/unknown-version/empty → quarantine + default.
/// Transient I/O → propagate untouched (ZEB-460).
pub fn load_or_recover<T>(cipher: &DatasetCipher, path: &Path, label: &'static str)
    -> Result<T, SyncError>
where T: DeserializeOwned + Default;

/// Serialize + seal + atomic-write (tempfile, fsync, rename, parent-dir
/// fsync via owner_state_persist::save_atomically).
pub fn save<T: Serialize>(cipher: &DatasetCipher, path: &Path, label: &'static str,
    value: &T) -> Result<(), SyncError>;
```

(Exact generic shape may adjust to fit the per-dataset `FileV1` wrapper
structs during planning; the contract above is fixed.)

Per-dataset modules shrink to: filename/label constants, their `FileV1`
struct(s), thin wrappers over the shared module, and their `FleetPersist`
impl. **The tombstones-before-doc write ordering in `dm_inbox_persist` and
`relay_hold_persist` stays verbatim in those impls** — the shared module
changes how a file is written, never the order files are written in.

## Migration & recovery semantics

On load (per file, first boot after upgrade):

1. `NotFound` → default (unchanged).
2. **v1 parses → eagerly re-save as v2 immediately**, then return the doc.
   Files upgrade on first boot even if never mutated. If the migration write
   fails transiently: log a warning, return the doc anyway — the plaintext v1
   stays in place (atomic rename = no torn state) and the next persist or
   next boot retries.
3. v2 → `open_dataset_file` → parse inner (existing v1 parse incl.
   trailing-bytes rejection).

Failure taxonomy (the ZEB-460 contract, extended):

| Failure on load | Action |
|---|---|
| Transient I/O (read error ≠ NotFound) | Propagate untouched; never quarantine |
| AEAD tag failure (v2) | Quarantine + default (**new category**; decision 3) |
| CBOR decode failure (v1 payload or v2 inner) | Quarantine + default (unchanged) |
| Unknown outer version byte, empty file | Quarantine + default (unchanged) |
| Trailing bytes after inner CBOR value | CborDecode → quarantine + default (unchanged) |

Quarantine mechanics unchanged: rename to `<file>.corrupt-<ms>`, bytes
preserved verbatim, `tracing::error!`, return `T::default()` so the app
boots; fleet sync then re-populates content from peer devices.

## Scope: datasets migrating in this PR

All construction sites are in `start_node_inner`'s fleet band; each persist
struct gains `keys: Arc<KeyTree>` (the pinned epoch-0 `kt`).

| Persist module | Files (all + replay trackers) | Notes |
|---|---|---|
| `notes_persist.rs` | `notes.cbor`, `notes_replay.cbor` | fix stale `:18` path comment in passing |
| `contacts_persist.rs` | `contacts.cbor`, `contacts_replay.cbor` | |
| `dm_inbox_persist.rs` | `dm_inbox.cbor`, `dm_inbox_replay.cbor`, `dm_inbox_first_observed.cbor`, `dm_inbox_expired.cbor` | sidecar write ordering preserved |
| `dm_outhold_persist.rs` | `dm_outhold.cbor`, `dm_outhold_replay.cbor` | |
| `relay_hold_persist.rs` | `relay_hold.cbor`, `relay_hold_replay.cbor`, `relay_hold_first_observed.cbor`, `relay_hold_expired.cbor` | sidecar write ordering preserved |
| `relay_optin_persist.rs` | `relay_optin.cbor`, `relay_optin_replay.cbor` | |
| `fleet_net_persist.rs` | `fleet_net.cbor`, `fleet_net_replay.cbor` | |
| `community_device_intro_persist.rs` | `community_device_intro.cbor`, `community_device_intro_replay.cbor` | |
| `fleet_peer_seed_persist.rs` | `fleet_peer_seed.cbor` | no `FleetPersist` impl; see checkpoint 1 |
| `fleet_key_epoch.rs` | `fleet_keys.cbor`, `fleet_keys_replay.cbor` | see checkpoint 2 |

Replay trackers and tombstone sidecars encrypt uniformly (ticket: "cheapest
is to encrypt uniformly" — one format, one code path).

**Implementation checkpoints (verify during planning):**

1. **`fleet_peer_seed.cbor` is written from `pairing/persist.rs:36`**, not
   from the fleet band. Verify the joiner has its KeyTree installed before
   that write fires; if a write path exists where no KeyTree is present yet,
   that write must be deferred until after key installation (it already
   cannot be *used* before then).
2. **`fleet_keys.cbor` (epoch-carrier) is non-circular:** it loads at
   BOOT-PROBE 08-fleet-keys (`lib.rs:7609`), strictly after the pinned
   epoch-0 tree is constructed (`lib.rs:5797-5889`), and only installs
   *additional* epochs. Sealing the carrier under epoch-0 is therefore safe;
   confirm no other reader touches it earlier.

**Out of scope (follow-ups):**

- `owner_state.cbor`, `owner_state_crdt.cbor`, `state_root_replay.cbor` +
  the trust/quorum docs that share `owner_state.cbor` — keyless local-only
  mode writes these, and non-fleet paths write `owner_state.cbor` under
  `OWNER_STATE_WRITE_LOCK`. Ticket to be filed at PR time.
- `mint_sync_state.cbor`, per-community `community_state`/`voting` persist
  families, `outbound_friend_links.cbor`, `profile_cards.*.cbor` — different
  persistence families; assess in the follow-up.
- Downgrade-block latch (v1 rejection after first seal) — future hardening.

## Testing

**Shared module unit tests (the contract lives here):**

- round-trip v2 (doc + replay shapes)
- v1 file → load returns doc **and the on-disk bytes become v2**
- v1 migration write failure (read-only parent) → doc still returned,
  plaintext left in place
- tag tamper (flip one ct byte) → quarantine, bytes preserved verbatim,
  default returned
- **label cross-swap**: seal under `"contacts.cbor"`, load as `"notes.cbor"`
  → quarantine
- foreign-key file (sealed under a different KeyTree) → quarantine
- transient I/O (path is a directory) → `SyncError::Persist` propagated, no
  quarantine
- empty file / unknown outer version / trailing bytes after inner CBOR →
  quarantine (or error) per taxonomy table
- missing file → default, no quarantine artifacts

**Per-dataset:** existing suites (each `*_persist.rs` has 8-10 tests pinning
the ZEB-460 contract) updated to construct a test cipher from
`KeyTree::derive(&[0u8; 32])`; sidecar-ordering tests unchanged.

**Integration:** existing headless e2e flows exercise boot decrypt
implicitly (every boot now opens ~23 sealed files). No new e2e needed.

## PR shape

One PR, layered commits: (1) crypto functions + tests, (2) shared module +
contract tests, (3) notes + contacts migration, (4) remaining eight
datasets, (5) `lib.rs` wiring + boot verification. All-Rust diff → full
local gates (`fmt`, `clippy --all-targets`, nextest, `--features
test-fixtures`) before push.
