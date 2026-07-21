# ZEB-726: streaming-encryption hardening & cleanup — design

**Status:** approved (design fork settled 2026-07-21)
**Follow-up to:** ZEB-724 (streaming chunked-AEAD encryption, PR #514)
**Scope:** `harmony-client` only, single PR. No changes to `harmony-content` / `harmony-crypto`.

## Problem

ZEB-724 shipped the v2 streaming file-encryption format (`file_stream_crypto.rs`,
magic `HSF2`). The whole-branch review + CodeRabbit surfaced a bundle of
non-blocking follow-ups, triaged as follow-up-grade (none blocked #514):

1. **Header is unauthenticated.** The v2 header (`magic ‖ version ‖ frame_size ‖
   nonce_prefix`) is read on decrypt without being covered by any AEAD tag.
   - `nonce_prefix` is *already* derived deterministically from the DEK
     (`derive_nonce_prefix`) but on decrypt it is read back from the header
     instead of re-derived — a needless trust of an unauthenticated field.
   - `frame_size` is read from the header and drives frame splitting. Tampering
     it today is **fail-closed** (wrong split → AEAD auth failure — DoS-only,
     never a confidentiality/integrity break), but it is still an
     unauthenticated field the decoder trusts.
2. **`pub` API panics / overflows on adversarial inputs** (defense-in-depth,
   unreachable from the sole production caller which always uses the
   `DEFAULT_FRAME_SIZE` const):
   - `v2_ciphertext_len(_, 0)` divides by zero → panic.
   - `FrameSealer::seal_next` / `seal_last` do not validate frame lengths.
   - `ct_frame_len = frame_size as usize + STREAM_TAG_LEN` can overflow `usize`
     on 32-bit targets.
3. **Test-helper triplication.** `spawn_recording_store` / `write_temp` /
   `reassemble_from_store` are copied verbatim into three integration-test
   binaries (`file_sharing_dek.rs`, `file_sharing_streaming.rs`,
   `file_sharing_grantee.rs`).
4. **Micro-perf / durability nits.** `produce_ciphertext` reallocates the
   look-ahead frame buffer every loop iteration; the export atomic-rename does
   not fsync the parent directory (crash-durability parity with
   `owner_state_persist::save_atomically`).

## Decision (settled fork)

Authenticate the header via a **clean v2→v3 wire break** that binds the header
as AEAD associated data. Rationale: it is the most-correct end state (no
unauthenticated fields, `frame_size` flexibility retained *and* authenticated),
it is cheapest **now** — v1 files are already unreadable (clean break shipped in
#514) and v2 files can only have been produced after #514 merged the same day,
so a fail-loud v3 break hits essentially no real data — and it matches the
established "clean break, fail-loud" precedent before folder-ingest or anything
else builds on the format.

## v3 wire format

```
Header (9 bytes, bound as AEAD associated data on the FIRST frame):
  magic(4)      = b"HSF3"
  version(1)    = 0x03
  frame_size(4) = u32 BE   (produced as DEFAULT_FRAME_SIZE = 65536)

Body: ChaCha20-Poly1305 STREAM (EncryptorBE32/DecryptorBE32) frames.
  - nonce prefix = SHA-256("harmony-file-stream-v3-nonce" ‖ dek)[..7]
    — re-derived from the DEK on BOTH seal and decrypt; NEVER stored in the wire.
  - first frame  : sealed/opened with the 9-byte header as `aad`.
  - later frames : empty `aad`.
  - final frame  : encrypt_last / decrypt_last (truncation/reorder fail-closed).
```

Changes from v2: magic `HSF2`→`HSF3`, version `0x02`→`0x03`, the 7-byte stored
`nonce_prefix` is dropped (header 16→9 bytes), and the header is now bound as
AAD. The nonce-derivation domain string bumps to `…-v3-nonce` (cosmetic; the DEK
is unique per file so nonce uniqueness never depended on it).

### Why binding the header on the first frame suffices

STREAM chains frames: the final frame authenticates the whole sequence, and
every frame's nonce embeds the BE32 counter, so a tampered first-frame tag fails
the whole decrypt. Binding the header as `aad` on frame 0 (which always exists —
an empty plaintext still emits one final frame) means any header tamper — a
flipped `frame_size`, `version`, or `magic` — flips frame 0's authentication and
the decrypt returns `Err`. `magic`/`version` are additionally validated as
constants (→ `UnsupportedLegacyFormat`), so the AAD binding is the belt to
`frame_size`'s missing suspenders.

## Threat model / what this closes

- **Before:** decode trusts `frame_size` + `nonce_prefix` from an unauthenticated
  header. Worst case is DoS (tamper → auth failure), never a confidentiality or
  integrity break, because the per-frame AEAD tags still gate every plaintext byte.
- **After:** decode trusts nothing from the wire except the AAD-bound,
  tag-covered header; `nonce_prefix` is re-derived from the secret DEK. No
  unauthenticated field influences decryption. This is defense-in-depth /
  smell-removal, not a live-vulnerability fix.

## API changes (`file_stream_crypto.rs`)

- Constants: `V3_MAGIC = *b"HSF3"`, `V3_VERSION = 0x03`, `V3_HEADER_LEN = 9`,
  `NONCE_DERIVE_INFO = b"harmony-file-stream-v3-nonce"`. `DEFAULT_FRAME_SIZE`,
  `STREAM_TAG_LEN`, `NONCE_PREFIX_LEN` unchanged.
- `FrameSealer::new(dek, frame_size) -> Result<Self, FileStreamError>` — now
  fallible; rejects `frame_size == 0` (`BadFrameSize`). Stores the 9-byte header
  and a `header_bound: bool` latch.
- `FrameSealer::seal_next(&mut self, frame) -> Result<..>` — validates
  `frame.len() == frame_size` (`BadFrameSize` otherwise); binds the header as
  `aad` on the first seal, then clears the latch.
- `FrameSealer::seal_last(&mut self, frame) -> Result<..>` — validates
  `frame.len() <= frame_size`; binds the header as `aad` if it is the first
  seal (covers the empty-plaintext single-final-frame case).
- `decrypt_stream_to_writer` — parses the 9-byte header; rejects any blob whose
  magic/version is not `(HSF3, 0x03)` as `UnsupportedLegacyFormat` (this now
  includes v2 `HSF2` blobs — the clean break); re-derives `nonce_prefix` from the
  DEK; computes `ct_frame_len` with a checked add (`BadFrameSize` on overflow);
  binds the header as `aad` on the first frame decrypt.
- `v2_ciphertext_len` → **`v3_ciphertext_len(plaintext_len, frame_size) ->
  Result<u64, FileStreamError>`** — rejects `frame_size == 0`; header term is
  now `V3_HEADER_LEN`.
- `decrypt_stream_to_path` — unchanged temp-file + atomic-rename logic, plus a
  best-effort **parent-directory fsync** after `persist()` (durability parity
  with `save_atomically`; a failed dir-fsync is non-fatal since the export is
  user-re-triggerable).

## Consumer changes

- `lib.rs::produce_ciphertext` — `FrameSealer::new(...)?` (now fallible);
  reuse one look-ahead buffer via `std::mem::swap(&mut cur, &mut nxt)` instead of
  `let mut nxt = vec![…]` every iteration; v2→v3 doc wording.
- `lib.rs::ingest_content_encrypted_inner` — pipe-buffer `cap` uses
  `V3_HEADER_LEN` (line ~20199); v2→v3 doc wording.
- `lib.rs::fetch_content` / `export_content` — call sites unchanged
  (`decrypt_stream` / `decrypt_stream_to_path` signatures are stable); v2→v3
  doc wording only.
- `file_sharing.rs` (×2) + `owner_state_types.rs` (×1) — doc comments referencing
  `v2_ciphertext_len` → `v3_ciphertext_len`; "v2" wording → "v3".

## Test-helper de-duplication

Move `spawn_recording_store` / `write_temp` / `reassemble_from_store` (and the
`Store` type alias they depend on) into a new
`tests/common/file_sharing_helpers.rs`, gated `#[cfg(feature = "test-fixtures")]`
and `#[allow(dead_code)]` (not every including binary uses every helper). Wire it
through the existing `tests/common/mod.rs` (`pub mod file_sharing_helpers;`).
`file_sharing_dek.rs`, `file_sharing_streaming.rs`, and `file_sharing_grantee.rs`
each replace their local copies with `#[path = "common/mod.rs"] mod common;` +
`use common::file_sharing_helpers::…`.

## Testing

**Unit (`file_stream_crypto.rs`):** update the existing 14 tests for the 9-byte
header (offsets, `round_trip_empty` length `9 + 16 = 25`, `FrameSealer::new(…)?`,
rename `non_v2_blob_rejected`). Add:
- **header-tamper-fails** — flip a header byte (e.g. `frame_size`) → decrypt `Err`
  (proves the AAD binding; RED without it).
- **v2-magic rejected** — an `HSF2`-prefixed blob → `UnsupportedLegacyFormat`
  (proves the clean break).
- **`v3_ciphertext_len(_, 0)` → `Err(BadFrameSize)`** (no panic).
- **`seal_next` wrong-length / `seal_last` over-length → `Err(BadFrameSize)`.**
- **nonce not on the wire** — assert the 9-byte header contains no
  `derive_nonce_prefix(dek)` bytes at the old offset (regression guard).

**Integration:** `file_sharing_{dek,streaming,grantee}.rs` exercise the real
ingest → `dag::reassemble` → `decrypt_stream` path, so they are format-agnostic
and must stay green after the de-dup (produce v3, decrypt v3). The existing
tamper test in `file_sharing_dek.rs` still asserts fail-closed decrypt.

Keychain-free, wall-clock-free. Final gate: CI-exact
`cargo nextest run --locked --workspace --all-targets --features test-fixtures`
+ `cargo clippy --locked --all-targets --features test-fixtures --no-deps --
-D warnings` + `cargo fmt --all -- --check`.

## Out of scope (remain on ZEB-726 / ZEB-674 deferral list)

- Incremental streaming **READ** (bounded read memory; rebuilds `dag::reassemble`
  — its own epic).
- **Folder-ingest** encryption (now unblocked by #514).
