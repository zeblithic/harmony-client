# ZEB-726 streaming-encryption hardening & cleanup — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rev the streaming file-encryption format to a fully-authenticated v3 (header bound as AEAD associated data, nonce re-derived from the DEK) and land the deferred `pub`-API hardening + test/perf cleanup from ZEB-724.

**Architecture:** Clean v2→v3 wire break in `file_stream_crypto.rs`: 9-byte header (`magic ‖ version ‖ frame_size`, no stored nonce) bound as `aad` on the first STREAM frame; `nonce_prefix` re-derived from the DEK on both seal and decrypt. Consumers in `lib.rs` adapt to the fallible `FrameSealer::new` and the renamed `V3_HEADER_LEN` / `v3_ciphertext_len`. Triplicated integration-test helpers move into `tests/common/`.

**Tech Stack:** Rust, `chacha20poly1305::aead::stream` (`EncryptorBE32`/`DecryptorBE32`) + `aead::Payload` for AAD, `sha2`, `tempfile`, `cargo-nextest`.

## Global Constraints

- **Single repo / single PR.** No changes to `harmony-content` / `harmony-crypto`. The per-file DEK stays fresh-random (`generate_file_dek`).
- **Clean break, fail-loud.** Any blob whose magic/version ≠ `(b"HSF3", 0x03)` → `FileStreamError::UnsupportedLegacyFormat`. This now includes v2 `HSF2` blobs. Never emit garbage plaintext.
- **v3 header = exactly 9 bytes:** `magic(4)=b"HSF3" ‖ version(1)=0x03 ‖ frame_size(4, u32 BE)`. The 7-byte nonce prefix is NOT on the wire; it is re-derived as `SHA-256(b"harmony-file-stream-v3-nonce" ‖ dek)[..7]` on both seal and decrypt.
- **AAD binding:** the 9-byte header is passed as `aead::Payload{ msg, aad: &header }` on the FIRST frame only (seal and decrypt); later frames use empty aad. Frame 0 always exists (empty plaintext still emits one final frame).
- **Determinism / hygiene:** keychain-free, wall-clock-free tests. Never construct `KeychainStore::new()`. Deterministic-nonce helpers stay behind `#[cfg(any(test, feature = "test-fixtures"))]`.
- **Final gates (CI-exact, run from `src-tauri/`):**
  - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
  - `cargo fmt --all -- --check`
  - Iterative dev may use `scripts/test-select --context task` — paste its printed `round=… bucket=…` summary line into the task report so the selection is auditable (Qodo rule 1601747); the final pre-PR sweep is the full `--workspace --all-targets` commands above.

---

### Task 1: v3 crypto core + `pub`-API hardening (`file_stream_crypto.rs`)

**Files:**
- Modify: `src-tauri/src/file_stream_crypto.rs` (whole module: constants, `FrameSealer`, `decrypt_stream_to_writer`, `v3_ciphertext_len`, `decrypt_stream_to_path`, all `#[cfg(test)]` tests)
- Modify: `src-tauri/src/lib.rs` — **only** the two compile-critical call sites that the fallible `FrameSealer::new` + `V3_HEADER_LEN` rename break (`produce_ciphertext` ~L20099 → `.map_err(|e| e.to_string())?`; ingest cap ~L20199 → `V3_HEADER_LEN`). These keep the crate compiling so T1 is an independently-testable deliverable. Everything else in lib.rs (buffer reuse, doc wording) stays in Task 2.

**Interfaces:**
- Consumes: `EpochKey` (`as_bytes`, `as_chacha_key`, `new`), `chacha20poly1305::aead::{stream::{EncryptorBE32, DecryptorBE32}, Payload, generic_array::GenericArray}`, `ChaCha20Poly1305`, `sha2::Sha256`, `tempfile::NamedTempFile`.
- Produces (relied on by Task 2):
  - `pub const V3_HEADER_LEN: usize = 9;`
  - `pub const DEFAULT_FRAME_SIZE: u32 = 65536;` (unchanged)
  - `pub struct FrameSealer;` with `pub fn new(dek: &EpochKey, frame_size: u32) -> Result<Self, FileStreamError>`, `pub fn header(&self) -> [u8; V3_HEADER_LEN]`, `pub fn seal_next(&mut self, frame: &[u8]) -> Result<Vec<u8>, FileStreamError>`, `pub fn seal_last(&mut self, frame: &[u8]) -> Result<Vec<u8>, FileStreamError>`
  - `pub fn decrypt_stream(dek: &EpochKey, ciphertext: &[u8]) -> Result<Vec<u8>, FileStreamError>` (signature unchanged)
  - `pub fn decrypt_stream_to_writer<W: std::io::Write>(dek, ciphertext, out) -> Result<(), FileStreamError>` (signature unchanged)
  - `pub fn decrypt_stream_to_path(dek, ciphertext, final_path: &Path) -> Result<(), FileStreamError>` (signature unchanged)
  - `pub fn v3_ciphertext_len(plaintext_len: u64, frame_size: u32) -> Result<u64, FileStreamError>` (RENAMED from `v2_ciphertext_len`; now fallible)
  - `pub enum FileStreamError { UnsupportedLegacyFormat, Truncated, BadFrameSize, Aead, Io(String) }` (unchanged)

**Design contract (must hold):**
1. Constants: `V3_MAGIC = *b"HSF3"`, `V3_VERSION: u8 = 0x03`, `V3_HEADER_LEN = 9`, `NONCE_DERIVE_INFO = b"harmony-file-stream-v3-nonce"`. Header layout: `hdr[0..4]=magic`, `hdr[4]=version`, `hdr[5..9]=frame_size.to_be_bytes()`. No nonce bytes in the header.
2. `derive_nonce_prefix(dek)` unchanged except the info string bumps to v3. Called in `FrameSealer::new` AND in `decrypt_stream_to_writer` (the header no longer carries the nonce).
3. `FrameSealer::new` rejects `frame_size == 0` → `Err(BadFrameSize)`. Stores the header bytes + a `header_bound: bool` (init `false`).
4. First `seal_next`/`seal_last` call passes `Payload{ msg: frame, aad: &self.header_bytes }` and sets `header_bound = true`; subsequent calls pass the bare `frame` (empty aad). `seal_next` requires `frame.len() == frame_size` else `Err(BadFrameSize)`; `seal_last` requires `frame.len() <= frame_size` else `Err(BadFrameSize)`.
5. `decrypt_stream_to_writer`: reject `ciphertext.len() < V3_HEADER_LEN` and any `magic/version` mismatch → `UnsupportedLegacyFormat`. Reject `frame_size == 0` → `BadFrameSize`. `ct_frame_len = (frame_size as usize).checked_add(STREAM_TAG_LEN).ok_or(BadFrameSize)?`. First frame (whether `decrypt_next` or `decrypt_last`) passes `Payload{ msg, aad: &hdr }`; later frames bare. Truncation/reorder/tamper still `Err`.
6. `v3_ciphertext_len` rejects `frame_size == 0` → `Err(BadFrameSize)`; else `Ok(V3_HEADER_LEN as u64 + plaintext_len + frames * STREAM_TAG_LEN as u64)` with `frames = if plaintext_len == 0 { 1 } else { plaintext_len.div_ceil(fs) }` (keep the existing MSRV-safe manual ceil + `#[allow(clippy::manual_div_ceil)]`).
7. `decrypt_stream_to_path`: unchanged temp-file + `sync_all` + `persist` logic, PLUS after `persist` succeeds, best-effort fsync the parent directory (`std::fs::File::open(dir).and_then(|d| d.sync_all())` — ignore the error; export is user-re-triggerable). Keep all existing data-loss protections (never touch `final_path` until decrypt fully succeeds).

**TDD steps:**

- [ ] **Step 1 — Update the module doc header + constants.** Rewrite the top doc block for v3 (magic `HSF3`, 9-byte header `magic ‖ version ‖ frame_size`, nonce re-derived from DEK not stored, header bound as AAD, v2/v1 rejected). Introduce `V3_MAGIC`/`V3_VERSION`/`V3_HEADER_LEN`/v3 `NONCE_DERIVE_INFO`. Add `use chacha20poly1305::aead::Payload;`.

- [ ] **Step 2 — Write/adapt the failing tests first.** Update `seal_all` helper (`FrameSealer::new(dek, fs).unwrap()`; header now 9 bytes). Update offset/length asserts: `round_trip_empty` expects `ct.len() == V3_HEADER_LEN + STREAM_TAG_LEN` (= 25); `single_bit_tamper_fails`/`reorder_fails`/`truncation_fails` use `V3_HEADER_LEN`. Rename `non_v2_blob_rejected` → `non_v3_blob_rejected` and add a case: an `HSF2`-magic blob (e.g. `let mut v2 = seal_all(...); v2[3] = b'2'; v2[4] = 0x02;` or hand-built) → `UnsupportedLegacyFormat`. Add new tests:
  - `header_tamper_fails`: seal, flip a byte in `ct[5..9]` (frame_size) → `decrypt_stream` is `Err` (RED without AAD binding).
  - `v3_ciphertext_len_rejects_zero_frame_size`: `assert!(matches!(v3_ciphertext_len(10, 0), Err(FileStreamError::BadFrameSize)))`.
  - `seal_next_wrong_length_rejected` / `seal_last_over_length_rejected`: `BadFrameSize`.
  - Keep `ciphertext_len_matches_emission` but unwrap the now-`Result` `v3_ciphertext_len`.

- [ ] **Step 3 — Run tests to verify they fail** (compile errors / RED for the new assertions): `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(file_stream_crypto)'`. Expected: FAIL.

- [ ] **Step 4 — Implement v3.** `FrameSealer` (header bytes + `header_bound` latch, fallible `new`, AAD-on-first, length validation); `decrypt_stream_to_writer` (9-byte parse, re-derive nonce, `checked_add`, AAD-on-first); `v3_ciphertext_len` (Result); `v2_header` helper → `v3_header` (9 bytes); `decrypt_stream_to_path` dir-fsync.

- [ ] **Step 5 — Run tests to verify they pass:** same command. Expected: PASS (all unit tests incl. the new ones).

- [ ] **Step 6 — Commit** (`feat(zeb-726): v3 fully-authenticated streaming file-encryption format`).

**Scope note for reviewer:** this task owns the ENTIRE `file_stream_crypto.rs`. The dir-fsync (durability) and the `pub`-API guards (frame_size==0, length validation, overflow) live here by design — same file, reviewed together with the crypto. Task 1 ALSO makes exactly two compile-critical edits in `lib.rs` (the fallible-`new` `.map_err(...)?` and the `V3_HEADER_LEN` cap constant) so the crate compiles — these are authorized, not scope creep; anything more in lib.rs would be.

---

### Task 2: consumers, accounting & docs (`lib.rs`, `file_sharing.rs`, `owner_state_types.rs`)

**Files:**
- Modify: `src-tauri/src/lib.rs` (`produce_ciphertext` ~20092, `ingest_content_encrypted_inner` ~20199, and v2→v3 doc wording near 19815 / 20089 / 20154-20191 / 20244)
- Modify: `src-tauri/src/file_sharing.rs` (doc comments at lines ~76 and ~372)
- Modify: `src-tauri/src/owner_state_types.rs` (doc comment ~2586)

**Interfaces:**
- Consumes from Task 1: `FrameSealer::new -> Result`, `V3_HEADER_LEN`, `v3_ciphertext_len`.

**Already done in Task 1 (do NOT redo):** `produce_ciphertext`'s `FrameSealer::new(...).map_err(|e| e.to_string())?` and `ingest_content_encrypted_inner`'s `V3_HEADER_LEN` cap constant — Task 1 folded these two compile-critical edits in. Confirm they're present; do not duplicate.

**Changes (this task):**
- `produce_ciphertext`: reuse the look-ahead buffer: allocate `nxt` once before the loop and swap instead of reallocating — replace `let mut nxt = vec![0u8; fs];` inside the loop with a pre-loop `let mut nxt = vec![0u8; fs];` and, on the "advance" branch, `std::mem::swap(&mut cur, &mut nxt); cur_len = nxt_len;` (semantics identical: `cur` becomes the peeked frame; the old `cur` buffer is reused next iteration). Verify `read_up_to(&mut file, &mut nxt)` still fills from index 0 each iteration.
- Doc wording: "v2 chunked-AEAD" → "v3", "v2 STREAM frames" → "v3 STREAM frames" in the lib.rs comments listed above.
- `file_sharing.rs` ×2 + `owner_state_types.rs` ×1: `file_stream_crypto::v2_ciphertext_len` → `v3_ciphertext_len`; "v2" → "v3" wording. (These describe `file_size` accounting; the "16-byte tag" AEAD-tag figure is correct and stays.)

**TDD steps:**
- [ ] **Step 1 — Apply the code + doc edits above.** (No behavior change beyond the fallible `new` and buffer reuse; the format change is exercised end-to-end by the existing integration tests.)
- [ ] **Step 2 — Verify the encrypted-ingest integration path is green:** `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(file_sharing)'`. Expected: PASS (produce v3 → reassemble → decrypt v3 round-trips).
- [ ] **Step 3 — Scoped lib build/clippy:** `cargo clippy --locked --lib --features test-fixtures --no-deps -- -D warnings`. Expected: clean.
- [ ] **Step 4 — Commit** (`refactor(zeb-726): adapt encrypted-ingest consumers to v3 (fallible sealer, V3_HEADER_LEN, buffer reuse)`).

---

### Task 3: de-duplicate integration-test helpers (`tests/common/`)

**Files:**
- Create: `src-tauri/tests/common/file_sharing_helpers.rs`
- Modify: `src-tauri/tests/common/mod.rs` (register the new submodule)
- Modify: `src-tauri/tests/file_sharing_dek.rs`, `src-tauri/tests/file_sharing_streaming.rs`, `src-tauri/tests/file_sharing_grantee.rs` (drop local copies; include the shared module)

**Changes:**
- Move `spawn_recording_store`, `write_temp`, `reassemble_from_store`, and the `Store` type alias (currently duplicated in all three files) into `tests/common/file_sharing_helpers.rs`. Copy the canonical bodies verbatim from `file_sharing_dek.rs` (lines 39/75/87) — confirm the three copies are byte-identical first (`diff` the extracted fns); if any diverges, use the dek.rs version and note it in the report. Gate the module `#[cfg(feature = "test-fixtures")]` and top the file with `#![allow(dead_code)]` (not every binary uses every helper).
- `tests/common/mod.rs`: add `#[cfg(feature = "test-fixtures")] pub mod file_sharing_helpers;`.
- Each of the three test binaries: add `#[path = "common/mod.rs"] mod common;` (matching the existing `api_tests.rs` convention) near the top, delete the three local fn definitions + the local `Store` alias, and add `use common::file_sharing_helpers::{spawn_recording_store, write_temp, reassemble_from_store, Store};` (import only what that binary actually uses; prune unused imports to satisfy `-D warnings`). Bump the v2→v3 comment wording in these files (`file_sharing_grantee.rs:250` "v2 frames" → "v3", `file_sharing_dek.rs`/`streaming.rs` "v2 decrypt" → "v3").

**TDD steps:**
- [ ] **Step 1 — Extract the shared helpers** into `tests/common/file_sharing_helpers.rs` (verify the three copies are identical first).
- [ ] **Step 2 — Register in `common/mod.rs`** and rewire the three binaries to the shared module; delete the local copies.
- [ ] **Step 3 — Run the affected integration binaries:** `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(file_sharing)'`. Expected: PASS, same test count as before (no tests lost in the move).
- [ ] **Step 4 — Scoped clippy on the test targets:** `cargo clippy --locked --tests --features test-fixtures --no-deps -- -D warnings`. Expected: clean (no dead_code / unused-import warnings).
- [ ] **Step 5 — Commit** (`test(zeb-726): de-duplicate file-sharing integration-test helpers into tests/common`).

---

## Final whole-branch review + gates

After Task 3: dispatch the whole-branch code review (Opus) over `git merge-base main HEAD`..HEAD, focused on the crypto correctness of the v3 AAD binding + the clean-break rejection. Then run the full CI-exact gates (clippy + nextest `--workspace --all-targets --features test-fixtures` + fmt) and reconcile the spec/plan "as-built" if anything drifted. Open the PR, fire one `@coderabbitai review`, converge.
