# ZEB-724 — Streaming (chunked-AEAD) encryption for large-file ingest

**Status:** design approved 2026-07-21 · single-repo (`harmony-client`), single-PR
**Follows:** ZEB-674 (per-viewer encrypted file sharing, PR #512), converge finding G
**Ticket:** [ZEB-724](https://linear.app/zeblith/issue/ZEB-724)

## Problem

ZEB-674's encrypted ingest (`ingest_content_encrypted_inner`, `lib.rs:20018`) does
**whole-blob** DEK encryption: it reads the entire plaintext into RAM
(`read_file_capped`), calls `encrypt_blob(&dek, &plaintext)` once to produce a single
monolithic ChaCha20-Poly1305 blob (`nonce12 ‖ ct ‖ tag16`), then feeds that ciphertext
through the FastCDC chunker. Both the full plaintext **and** the full ciphertext are
resident simultaneously. To avoid OOM, round 1 added a hard **256 MiB** cap
(`MAX_ENCRYPTED_INGEST_BYTES`, `lib.rs:20083`) that simply refuses larger encrypted
uploads. The non-encrypted path has no such cap — it streams through the chunker with
bounded memory (ZEB-161).

This ticket lifts the cap by making encrypted ingest **stream**: plaintext is encrypted
frame-by-frame and fed to the chunker incrementally, so neither the whole plaintext nor
the whole ciphertext is ever resident.

## Current-state facts (verified in code)

1. **`encrypt_blob`/`decrypt_blob` are in-tree** (`community_state_sync.rs:163/189`),
   ChaCha20-Poly1305 (12-byte nonce, RustCrypto `chacha20poly1305` crate), single-shot
   `&[u8] -> Vec<u8>`, with a **deterministic** nonce = `SHA-256(prefix ‖ key ‖ pt)[..12]`
   prepended. **No version/algorithm header.** Freely editable client-side.
2. **The DEK is an `EpochKey([u8;32])`** (`owner_state_types.rs:479`), fresh-random per
   file (`file_sharing::generate_file_dek`). Because the DEK is fresh per file, encrypted
   CIDs are **already** non-deterministic across ingests — a random or DEK-derived nonce
   prefix costs nothing.
3. **The "encrypted" signal is a bit in the CID header** (`ContentFlags.encrypted`, top
   mode bit `0x80`, `cid.rs`), **not** a manifest. Read decides to decrypt purely on
   `cid.flags().encrypted` (`lib.rs:19804`). The DEK is keyed by root-CID bytes in
   `OwnerState.file_deks` (own) / `received_file_grants` (shared). The layout change is
   therefore **entirely inside the content bytes** — no CID-scheme or manifest change.
4. **The FastCDC path (`streaming_ingest_with_options`, `lib.rs:540`) streams any reader**
   in a 1 MiB window (`READ_WINDOW_SIZE`), emitting Book leaves + a Bundle tree with
   bounded memory. It is the model to match; it is untouched by this change.
5. **The read path also reassembles the whole ciphertext** (`dag::reassemble` →
   one `Vec<u8>`) before decrypt — identical to how public/non-encrypted files read today.
6. **No byte-golden fixture pins the ZEB-674 encrypted layout.** Only round-trip tests
   (`tests/file_sharing_dek.rs`, `_grantee.rs`, `_grants.rs`); `file_sharing_dek.rs`
   assumes "single-chunk plaintext ⇒ leaf bytes ARE the whole ciphertext" and must be
   reworked. The only `tests/wire_format` fixture pins the **identity keystore** envelope,
   unrelated to file content.
7. **Prior art:** `harmony-tunnel/frame.rs` already does per-frame counter-nonce AEAD
   (`counter_to_nonce`, `encrypt_frame`) over ChaCha20-Poly1305 — the transport analogue
   of what we build for content.

## Product decisions (approved)

- **Read scope — stream ingest, read at parity.** Ingest streams with bounded memory and
  the 256 MiB cap is removed (the ticket's goal). Decrypt-on-read parses the new chunked
  layout; `export_content` writes plaintext **incrementally** to the output file (no 2×
  plaintext+ciphertext residency). The ciphertext is still reassembled whole by the
  existing fetch pipeline — **identical to public-file reads today, so no regression.**
  True incremental *read* (rebuilding `dag::reassemble` / the fetch channel, which also
  governs public files) is deferred to a separate cross-cutting ticket (filed alongside
  this design).
- **AEAD construction — RustCrypto STREAM.** Use `chacha20poly1305`'s vetted `aead::stream`
  (Rogaway's STREAM / "Online AE"): 7-byte nonce prefix + BE32 per-chunk counter + a
  last-block flag that turns truncation **or** reordering into a hard decrypt failure.
  We do **not** hand-roll nonce/tag/final-chunk framing.
- **v1 compatibility — clean break, fail-loud.** New writes use the v2 chunked layout.
  Pre-existing whole-blob (v1) encrypted files are **not** readable and must be
  re-ingested (acceptable pre-alpha). To avoid silent corruption, the v2 decryptor
  **validates its header magic** and returns a clear "legacy/unsupported encrypted
  format — re-ingest" error on any non-v2 blob, rather than emitting garbage.

## Architecture (client-only)

Everything lands in `harmony-client/src-tauri`. `harmony-content` (chunker/CID/bundle/dag)
and `harmony-crypto` are consumed **unchanged**. New crypto is in-tree over the existing
`chacha20poly1305` dependency (enable its `stream` feature).

### New module: `src-tauri/src/file_stream_crypto.rs`

```rust
/// v2 header, prepended once to the ciphertext stream.
/// magic(4) ‖ version(1) ‖ frame_size:u32 BE(4) ‖ nonce_prefix(7) = 16 bytes.
const V2_MAGIC: [u8; 4] = *b"HSF2";           // Harmony Stream File, v2
const V2_VERSION: u8 = 0x02;
const DEFAULT_FRAME_SIZE: u32 = 64 * 1024;    // 64 KiB plaintext/frame (age's choice)
pub const V2_HEADER_LEN: usize = 16;
const STREAM_TAG_LEN: usize = 16;             // Poly1305 tag per frame

/// nonce_prefix = SHA-256(b"harmony-file-stream-v2-nonce" || dek)[..7].
/// Deterministic (no RNG); safe because the DEK is unique per file.
fn derive_nonce_prefix(dek: &EpochKey) -> [u8; 7];

/// A `Read` adapter that seals `inner` (plaintext) into v2 frames on the fly.
/// Yields: header, then STREAM frames; the final frame uses `encrypt_last`.
/// Bounded memory: one plaintext frame + one ciphertext frame buffered.
pub struct StreamingEncryptReader<R: Read> { /* dek-keyed STREAM encryptor + framing state */ }
impl<R: Read> StreamingEncryptReader<R> {
    pub fn new(inner: R, dek: &EpochKey, frame_size: u32) -> Self;
}
impl<R: Read> Read for StreamingEncryptReader<R> { /* ... */ }

/// Whole-ciphertext → whole-plaintext (used by fetch_content; small in-app reads).
/// Validates header magic/version; errors on non-v2 (clean break).
pub fn decrypt_stream(dek: &EpochKey, ciphertext: &[u8]) -> Result<Vec<u8>, FileStreamError>;

/// Whole-ciphertext → plaintext streamed to `out` frame-by-frame (used by export_content;
/// peak ≈ ciphertext size, no 2×). Same header validation.
pub fn decrypt_stream_to_writer<W: Write>(
    dek: &EpochKey, ciphertext: &[u8], out: &mut W,
) -> Result<(), FileStreamError>;

/// Exact ciphertext length for a given plaintext length (for file_size / max_bytes).
/// = V2_HEADER_LEN + L + frames*STREAM_TAG_LEN, frames = max(1, ceil(L/frame_size)).
pub fn v2_ciphertext_len(plaintext_len: u64, frame_size: u32) -> u64;
```

The `Read`/`Write` traits here are whichever the ingest streamer consumes (sync `std::io`
vs `tokio::io`); Task 1 pins that against `streaming_ingest_with_options`'s reader bound and
the adapter implements the matching trait.

### Data flow (ingest)

Pick file → open a **streaming reader** over the path (no `read_file_capped`) →
`generate_file_dek()` → wrap the reader in `StreamingEncryptReader` →
`streaming_ingest_with_options(reader, …, IngestOptions{ flags.encrypted=true, serveable })`
chunks the **ciphertext stream** into encrypted-flagged Book/Bundle CIDs (bounded memory) →
`seal_dek_at_rest` → `file_deks[root] = sealed` → `notify_dirty()` → sidecar row.

### Data flow (read)

`fetch_content`/`export_content` request root CID → `dag::reassemble` → whole ciphertext
`Vec` → `decrypt_personal_file_if_held` looks up the sealed DEK (own `file_deks` else
`received_file_grants`), unseals, and:
- `fetch_content` → `decrypt_stream` → whole plaintext `Vec` returned to FE.
- `export_content` → `decrypt_stream_to_writer` → plaintext streamed to the save path.

Non-v2 bytes (magic mismatch) → explicit `FileStreamError::UnsupportedLegacyFormat`.

## Seams changed (all `src-tauri`)

| # | Location | Change |
|---|----------|--------|
| S1 | `file_stream_crypto.rs` (new) | STREAM encryptor/decryptor, header, overhead fn. |
| S2 | `ingest_content_encrypted_inner` `lib.rs:20018` + IPC wrapper `:20150` | Take a streaming plaintext source; wrap in `StreamingEncryptReader`; drop `encrypt_blob`. |
| S3 | `MAX_ENCRYPTED_INGEST_BYTES` / `read_file_capped` `lib.rs:20083/20090` + unit test | Delete (cap lifted; sole caller is S2). |
| S4 | `decrypt_personal_file_if_held` `lib.rs:19798` | `decrypt_blob` → `decrypt_stream`; validate magic → clean error on non-v2. DEK lookup unchanged. |
| S5 | `export_content` `lib.rs:19864` | `decrypt_stream_to_writer` → incremental plaintext write. `fetch_content` `:24099` uses `decrypt_stream`. |
| S6 | `FileGrantInner`/`ReceivedFileGrant` `file_size` + `max_bytes` guards | Store **plaintext** size; compute ciphertext bound via `v2_ciphertext_len`. Update the `+28` overhead sites. |
| S7 | `Cargo.toml` | Enable `chacha20poly1305` `stream` feature (verify the RustCrypto re-export). |
| S8 | `tests/file_sharing_dek.rs` (+ `_grantee`/`_grants` as needed) | Drop the "single-chunk ⇒ whole ciphertext" assumption; drive the new layout. |

## Security model

- **Confidentiality** rests entirely on DEK secrecy (unchanged from ZEB-674; serve is
  open-once-allowlisted, so the CID is not a secret).
- **STREAM integrity:** each frame is an independent ChaCha20-Poly1305 seal under a nonce =
  `prefix ‖ BE32(counter) ‖ last_block_flag`. A truncated stream (missing final frame),
  a reordered frame, a duplicated frame, or a single-bit tamper all fail decrypt — the
  last-block flag specifically defeats truncation (dropping the tail no longer verifies).
- **Nonce uniqueness:** the prefix is derived from the DEK, which is unique per file, so
  the (key, nonce) space is never reused across files; the BE32 counter is unique within
  a file. 32-bit counter ⇒ ≤ 2^32−1 frames ⇒ ≤ 256 TiB per file at 64 KiB frames.
- **Domain separation:** v2 magic + version in the header; the nonce-prefix derivation uses
  a v2-specific info string, so a v2 seal can never be confused with the v1 blob nonce or
  any other seal.

## Honest limits (stated plainly)

- **Read memory is not yet bounded.** After this change, *ingest* streams, but *reading* a
  very large encrypted file still reassembles the whole ciphertext in RAM (then streams
  plaintext to disk on export). This is exactly at parity with public-file reads today and
  is not a regression. Incremental read is a **separate deferred ticket** (rebuild
  `dag::reassemble` / fetch pipeline; affects public files too).
- **v1 encrypted files are unreadable** (clean break); they fail loud and must be
  re-ingested.
- **Folder-ingest encryption** is now unblocked (each descendant leaf can be streamed +
  encrypted) but remains a **separate follow-up**, out of scope here.

## Testing (TDD, `--features test-fixtures`)

Rust unit (`file_stream_crypto.rs`) + integration (`tests/file_sharing_*.rs`):

1. **Round-trip, multi-frame:** plaintext > several frames → ingest → read → bytes equal.
2. **Empty file:** L = 0 → 32-byte ciphertext (header + one empty final tag) → round-trips.
3. **Exact frame boundary:** L = k·frame_size (final frame is a full frame) → round-trips.
4. **Sub-frame:** L < frame_size (single final frame) → round-trips.
5. **Truncation fails:** drop the last ciphertext frame → `decrypt_stream` errors.
6. **Reorder/duplicate fails:** swap two frames → errors.
7. **Tamper fails:** flip one ciphertext bit → errors.
8. **Non-v2 rejected:** feed a v1 `encrypt_blob` blob → `UnsupportedLegacyFormat`.
9. **Bounded-memory large ingest:** stream a > 256 MiB source (temp/sparse or repeated
   reader) → ingest + read succeed with the cap gone; assert peak residency stays bounded
   (e.g. a counting reader proves the whole file is never buffered at once).
10. **Overhead fn:** `v2_ciphertext_len` matches actual emitted length across L ∈ {0, 1,
    frame_size−1, frame_size, frame_size+1, k·frame_size}.
11. **Grantee path:** `_grantee.rs` round-trips a shared multi-frame encrypted file
    (grant → receive → decrypt).

Keychain-isolated (no `KeychainStore::new()`); wall-clock-free.

## Out of scope / follow-ups (to file)

- Incremental **streaming read** (bounded read memory) — separate ticket.
- **Folder-ingest** encryption — separate ZEB-674 deferral.
- v1→v2 migration/re-ingest tooling (clean break means none for MVP).
