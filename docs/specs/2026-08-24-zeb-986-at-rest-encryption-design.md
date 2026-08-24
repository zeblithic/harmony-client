# ZEB-986 PR-3 — At-rest encryption of the remaining plaintext app-data stores — design

**Ticket:** ZEB-986 (Local-persistence hardening sweep), PR 3 of a 3-PR split.
**Scope:** seal the six plaintext app-data / storage stores that ZEB-981/982/983
did not cover, under the existing device-sealed envelope. PR-1 (recovery contracts)
is merged (#730); PR-2 (standalone write-atomicity) was **dropped** — sealing delivers
atomicity + fsync + 0600 for free through `save_atomically`, so a separate plaintext
atomicity fold would be redundant work every one of these files' sealing then re-touches.
**Date:** 2026-08-24
**Depends on:** ZEB-982 `device_dataset_file` (`DeviceCipher`, `read_image`/`write_image`/
`reseal_if_legacy`), ZEB-986 PR-1 `recoverable_load`.

## Problem

Six app-data stores still hold sensitive data as plaintext on disk:

| Store | File | On-disk today | Sensitive contents |
|---|---|---|---|
| `follows.rs` `FollowManager` | `follows.json` | plaintext JSON, fixed-`.tmp` no-fsync | outbound follow graph (who I follow) |
| `content_index.rs` `ContentIndex` | `content-index.json` | plaintext JSON, fixed-`.tmp` no-fsync | filenames, sensitivity labels, provenance |
| `vine_feed_cache.rs` `VineFeedCache` | `vine_feed.json` | plaintext JSON, fixed-`.tmp` no-fsync | other owners' full follow lists |
| `vine_pull_driver.rs` | `vine_pull.cbor` | plaintext CBOR, fixed-`.tmp` no-fsync | followed-creator graph + relay dialing hints |
| `storage_records.rs` `StorageRecordStore` | `storage_records.json` | plaintext JSON, `write_atomic_0600` | **`signer_pins` TOFU anchors** + pledge/backup topology |
| `storage_ledger.rs` `StorageLedger` | `storage_ledger.json` | plaintext JSON, `write_atomic_0600` | per-buddy hosted CIDs + sizes |

The first four use PR-1's `recoverable_load` recovery contract but are plaintext. The two
storage stores are plaintext **and** carry a latent PR-1-class bug: their load silently
starts empty on *any* error including a transient read failure (`storage_records.rs:257`,
`storage_ledger.rs:78`), so one EIO + one mutation wipes the store. `storage_records.json`
additionally holds the `signer_pins` trust anchors, which the ticket flags as a tamper
target ("re-verify signatures on load").

## The re-verify-on-load ask is subsumed by sealing (key finding)

The ticket asks to "re-verify signatures on load" for the storage anchors. Investigation
(agent map, 2026-08-24) shows **there are no signatures on disk to re-verify**:
`storage_records.rs:12` — signatures are verify-*once*-at-ingest and never persisted. The
`signer_pins` are stored as bare `(owner_id, device_ed25519)` bytes: no self-signature, no
chain. Today, editing the file to swap a pinned device key survives reload undetected (only
hex/length is checked, `storage_records.rs:334`).

The two asks — *encrypt at rest* and *tamper-evidence for the anchors* — therefore collapse
into one under device-sealing. The `DeviceCipher` AEAD seal is keyed to a device-local key
derived from the identity seed (keychain / encrypted `identity.enc`), which a filesystem
attacker does not have. Any edit to a sealed file fails the AEAD tag on load → detected. The
seal gives **both** confidentiality **and** tamper-evidence — strictly stronger than replaying
persisted owner-signatures (which give neither confidentiality nor protection against a
device-seed holder, who has already achieved total device compromise).

**Decision (maintainer-confirmed): seal only.** Do not persist enrollment/binding/device
signature material and do not replay `verify_v2_common` on load. Sealing is the integrity
boundary. This avoids putting cert material at rest and pulling the enrollment-verify path
into boot.

## Global constraints

- **One envelope, already shipped.** Reuse `device_dataset_file` unchanged. Sealed form is
  `[0x03] ‖ nonce(12) ‖ AEAD(inner) ‖ tag(16)`; AAD binds the canonical filename. No new
  crypto, no schema-version bump to the envelope.
- **Lazy plaintext → sealed migration.** First load reads the legacy plaintext image
  (`was_legacy = true`), parses it, and `reseal_if_legacy` rewrites it sealed — byte-lossless,
  exactly as ZEB-981/982/983. No migration script; no separate migration boot.
- **One-way ratchet, no rollback.** Once sealed, saves are always sealed. Sentinel `0x03` is
  reserved forever and collides with no JSON (`0x7B`) or CBOR (`0x80+`/`0xA0+`) first byte, so
  legacy detection is exact. An older build cannot read a sealed file — accepted policy since
  ZEB-981.
- **Atomicity + 0600 come free.** `write_image` routes through `owner_state_persist::
  save_atomically` (atomic temp + fsync + dir-fsync) and inherits `tempfile`'s 0600 mode. This
  closes the PR-2 durability/permission gap for all six files as a side effect — pin it with a
  mode-0600 assertion test.
- **Determinism / testability.** Tests use `device_dataset_file::test_cipher()` (fixed
  `[7u8;32]` seed) and fixed `now_ms`. No wall-clock read on a test-exercised path.
- **Never panic on load.** Every load path returns a value (real or default); no `unwrap`/
  `expect` on file contents or decryption.

## Cipher threading

Each store gains a `cipher: DeviceCipher` field (cheap clone; key `Arc`-shared, zeroized on
final drop). The production boot derives one cipher early via
`device_dataset_file::get_or_derive(&identity_dir)` (memoized; the seed is in hand right after
identity load) and threads it into every store's `load`. `save` reads the held field. Test and
placeholder (`Path::new("")`) call sites pass `test_cipher()` — the bare-path branch
short-circuits before any read, so its cipher is unused.

## Two recovery contracts

Sealing composes with PR-1's recovery discipline, but the two store classes need **different
handling of a sealed decrypt-failure** — the reason PR-1's `load_or_recover` cannot be reused
verbatim.

### App-data stores (follows / content_index / vine_feed / vine_pull): `load_sealed_or_recover`

A new sibling to `load_or_recover` in `recoverable_load.rs`:

```rust
pub fn load_sealed_or_recover<T: Default>(
    cipher: &DeviceCipher,
    path: &Path,
    filename: &str,
    now_ms: u64,
    parse: impl FnOnce(&[u8]) -> Result<T, String>,
) -> Recovered<T>;
```

Classification (via `device_dataset_file::read_image`'s typed error):

- `Ok(None)` (missing) → `(default, frozen=false)` — first run, silent.
- `Err(ImageError::Io)` (transient read) → `(default, frozen=true)` + warn — preserve maybe-good bytes.
- `Err(ImageError::Crypto)` (**sealed** file, bad tag / truncated / wrong-or-rotated key) →
  `(default, frozen=true)` + warn. **Freeze, never quarantine.** A sealed file that will not
  decrypt must not be wiped: it may be a key rotation the memo has not yet caught, and these
  stores fully re-derive from the network, so freezing loses nothing permanent. This is why
  the shared helper cannot route through the old quarantine-on-any-parse-error path — a wrong
  key would otherwise silently quarantine (wipe) every store before boot could notice, and the
  boot ordering (`follows` loads at `lib.rs:4295`, before the owner-state CRDT decrypt at
  `:5991`) gives no earlier key-correctness gate.
- `Ok(Some(image))`, `parse(image.bytes)`:
  - `Ok(v)` → `reseal_if_legacy(cipher, path, filename, &image)` then `(v, frozen=false)`.
  - `Err(reason)` → this is a **legacy-plaintext** content corruption (a sealed body would have
    failed at `read_image` as `Crypto`, above) → quarantine aside `.corrupt-<now_ms>` + heal on
    next write → `(default, frozen=false)`; a failed quarantine-rename → `(default, frozen=true)`.
    Reuses PR-1's collision-safe, exhaustion-freezing `quarantine`.

Version handling stays at the **store layer**, unchanged from PR-1: a parseable-but-unsupported
`version` freezes in place (the store sets `frozen=true` after the parse). A forward-version
*legacy* file gets resealed-then-frozen inside the helper — byte-lossless, data preserved, now
encrypted; acceptable since `FILE_VERSION` has only ever been 1.

Store integration is a near-one-liner per store: swap `load_or_recover(path, now_ms, parse)` →
`load_sealed_or_recover(cipher, path, filename, now_ms, parse)` (parse closure unchanged), swap
the manual tmp+rename in `save()` → `write_image(cipher, &self.path, filename, &bytes)`, keep
the existing `disk_write_frozen` guard.

### Storage anchors (storage_records / storage_ledger): fail-closed, bespoke mapping

The TOFU anchors must **never silently reset the anti-rebind ratchet**. They do not use the
shared helper; each maps `read_image` outcomes directly:

- `Ok(None)` (missing) → empty store, **accept ingest** — legitimate first-run TOFU.
- `Ok(Some(image))`, parse `Ok`, version OK → `reseal_if_legacy`; loaded, accept ingest.
- `Ok(Some(image))`, parse `Ok`, version mismatch → freeze in place + **sealed-fault**; preserve file.
- `Ok(Some(image))`, parse `Err` (corrupt) → freeze + **sealed-fault**; **preserve in place, do
  not quarantine** (a trust anchor is not self-healing — start-empty-and-re-TOFU is exactly the
  downgrade to avoid).
- `Err(ImageError::Io | Crypto)` → freeze + **sealed-fault**; preserve file.

**sealed-fault** is a new `bool` on each store. When set: `save()` is a no-op (file preserved
for forensics) and `StorageRecordStore::v2_admission` (and the ledger's mutators) **reject all
storage-record ingest for the session** — the storage subsystem fails closed rather than
re-TOFU to whatever is presented next. Loud one-line warn on entry to the fault state. Boot
otherwise proceeds normally (this is a bounded-severity storage-hosting subsystem, not
identity/messages — no boot-fatal).

This also fixes the latent transient-IO-wipe bug: an `Io` error now freezes instead of silently
starting empty and overwriting on the next save.

## Per-file integration summary

| File | Load change | Save change | Struct field(s) added |
|---|---|---|---|
| `follows.rs` | `load_sealed_or_recover`, filename `follows.json` | `write_image` | `cipher` |
| `content_index.rs` | `load_sealed_or_recover`, filename `content-index.json` | `write_image` | `cipher` |
| `vine_feed_cache.rs` | `load_sealed_or_recover`, filename `vine_feed.json` | `write_image` | `cipher` |
| `vine_pull_driver.rs` | `load_sealed_or_recover`, filename `vine_pull.cbor` | `write_image` | `cipher` (on driver) |
| `storage_records.rs` | bespoke `read_image` map (fail-closed) | `write_image`, no-op if sealed-fault | `cipher`, `sealed_fault` |
| `storage_ledger.rs` | bespoke `read_image` map (fail-closed) | `write_image`, no-op if sealed-fault | `cipher`, `sealed_fault` |

Boot wiring (`lib.rs`): derive the device cipher once before the store loads and thread it into
`FollowManager::load` (:4295), `VineFeedCache::load` (:4334), `StorageRecordStore::new` /
`StorageLedger::new` (:4305/:4313), `ContentIndex::load` (:13720), `VinePullDriver::new`
(:12747). `content_index::load`'s many call sites (mostly tests + bare-path placeholders) take
the cipher param; tests pass `test_cipher()`.

## Testing

Per-store, in each file's `#[cfg(test)] mod tests`, using `test_cipher()`:

- **round-trip sealed:** load empty → mutate → save → assert on-disk first byte is `0x03`
  (sealed) and a reload recovers the value.
- **plaintext → sealed migration:** write a legacy plaintext file → load (recovers the data) →
  assert the file is now sealed (`0x03`) and byte-lossless through a second reload.
- **mode 0600:** (unix) after a save, assert the file mode is `0o600` — pins the PR-2
  permission property that sealing delivers for free.
- **sealed AEAD-failure → freeze (app-data):** seal under `test_cipher`, load under a *foreign*
  cipher → assert empty + frozen, mutate + save → assert the sealed file is **not** overwritten
  and **not** quarantined.
- **legacy-plaintext corruption → quarantine + heal (app-data):** write garbage JSON/CBOR →
  load → assert empty, a `.corrupt-<ms>` sidecar holds the bytes, and the next save heals.
- **transient Io → freeze (app-data):** (unix) unreadable file → load → frozen; save → not
  overwritten.
- **storage fail-closed on seal-failure:** seal → load under a foreign cipher → assert empty +
  `sealed_fault` set, an ingest sample is **rejected**, and `save()` leaves the sealed file
  byte-identical (no quarantine, no wipe).
- **storage corrupt → fail-closed (not quarantined):** garbage plaintext `storage_records.json`
  → load → `sealed_fault` set, file preserved in place, ingest rejected.
- **storage first-run TOFU still works:** missing file → load → not faulted → a first ingest
  pins normally.
- **storage transient-IO no longer wipes:** (unix) unreadable file → load → `sealed_fault`
  set + frozen; a subsequent save does not overwrite the still-good file.

Full gates before PR: `cargo fmt`, `cargo clippy --all-targets -D warnings`, scoped `--lib`
during iteration, full `--workspace --all-targets` sweep before push; frontend untouched (run
`tsc`/`vitest` to confirm zero drift).

## Out of scope (later tickets)

- **ZEB-984** — mail bodies/index at rest (its own ticket; sealing there also handles
  CID-verify-on-read + the mail index recovery contract).
- **ZEB-985** — `mint/ledger.db` SQLCipher (whole-file sealing is a non-starter for SQLite;
  needs its own design pass).
- Persisting + replaying the storage v2 signature chain on load — explicitly rejected above;
  sealing subsumes it.
- content-index rebuild-from-directory-scan — the PR-1 follow-up idea; the `.corrupt-<ms>`
  sidecar still preserves bytes for it.
