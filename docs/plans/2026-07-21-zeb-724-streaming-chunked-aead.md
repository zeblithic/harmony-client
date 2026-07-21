# ZEB-724 Streaming (chunked-AEAD) Encryption Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make encrypted personal-file ingest stream chunk-by-chunk (bounded memory), lift the 256 MiB cap, and update decrypt-on-read to the new chunked-AEAD layout.

**Architecture:** A new in-tree module `file_stream_crypto.rs` provides a pure, unit-tested STREAM (RustCrypto `aead::stream`, `EncryptorBE32`/`DecryptorBE32` over ChaCha20-Poly1305) frame sealer/opener + a versioned 16-byte header. Ingest wraps the plaintext file in a producer task that seals frames into a `tokio::io::duplex` pipe, whose read half feeds the **unchanged** FastCDC `streaming_ingest_with_options`. Read decrypts the reassembled ciphertext frame-by-frame (writing plaintext incrementally on export). Clean v1 break: the decryptor validates its header magic and fails loud on any non-v2 blob.

**Tech Stack:** Rust (`src-tauri`), `chacha20poly1305` 0.10 (`stream` feature), `sha2`, `tokio` (`io-util`), FastCDC chunker (`harmony-content`, unchanged).

## Global Constraints

Every task's requirements implicitly include these:

- **Single-repo only.** All changes in `harmony-client/src-tauri`. Do **not** modify `harmony-content` or `harmony-crypto` (consumed unchanged).
- **v2 wire format (exact):** header = `magic b"HSF2"` (4) ‖ `version 0x02` (1) ‖ `frame_size:u32 BE` (4) ‖ `nonce_prefix` (7) = **16 bytes**. `DEFAULT_FRAME_SIZE = 65536` (64 KiB plaintext/frame). Per-frame tag = 16 bytes. `nonce_prefix = SHA-256(b"harmony-file-stream-v2-nonce" ‖ dek.as_bytes())[..7]`.
- **AEAD:** `chacha20poly1305::aead::stream::{EncryptorBE32, DecryptorBE32}` (7-byte nonce prefix); final frame via `encrypt_last`/`decrypt_last`. Never hand-roll nonce/tag/counter framing.
- **Clean v1 break, fail-loud:** decrypt validates magic+version; a non-v2 blob → `FileStreamError::UnsupportedLegacyFormat` (never garbage, never panic).
- **Do NOT touch the channel-artifact overhead path** (`BLOB_ENCRYPTION_OVERHEAD` at `lib.rs:29994`, `community_state_sync::{encrypt_blob,decrypt_blob}`). Those remain whole-blob for community artifacts and are out of scope.
- **Keychain isolation in tests:** derive keys via `KeyTree::derive(&[u8;32])`; **never** `KeychainStore::new()`. Tests are wall-clock-free.
- **Gates:** `cargo fmt --all -- --check`, `cargo clippy --all-targets` (0 warnings), tests green. Preserve `notify_dirty()` after every `crdt_state` mutation.
- **TDD:** write the failing test first, watch it fail, implement minimally, watch it pass, commit.

---

### Task 1: Pure v2 frame crypto core (`file_stream_crypto.rs`)

**Files:**
- Create: `src-tauri/src/file_stream_crypto.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod file_stream_crypto;` near the other `pub mod` declarations)
- Modify: `src-tauri/Cargo.toml:136` (enable `chacha20poly1305` `stream` feature)

**Interfaces:**
- Consumes: `crate::owner_state_types::EpochKey` (`as_bytes() -> &[u8;32]`, `as_chacha_key() -> &chacha20poly1305::Key`).
- Produces (used by Tasks 2 & 3):
  - `pub const DEFAULT_FRAME_SIZE: u32` (= 65536), `pub const V2_HEADER_LEN: usize` (= 16)
  - `pub struct FrameSealer` with `fn new(dek: &EpochKey, frame_size: u32) -> Self`, `fn header(&self) -> [u8; V2_HEADER_LEN]`, `fn seal_next(&mut self, frame: &[u8]) -> Result<Vec<u8>, FileStreamError>`, `fn seal_last(&mut self, frame: &[u8]) -> Result<Vec<u8>, FileStreamError>`
  - `pub fn decrypt_stream(dek: &EpochKey, ciphertext: &[u8]) -> Result<Vec<u8>, FileStreamError>`
  - `pub fn decrypt_stream_to_writer<W: std::io::Write>(dek: &EpochKey, ciphertext: &[u8], out: &mut W) -> Result<(), FileStreamError>`
  - `pub fn v2_ciphertext_len(plaintext_len: u64, frame_size: u32) -> u64`
  - `pub enum FileStreamError` (`UnsupportedLegacyFormat`, `Truncated`, `BadFrameSize`, `Aead`, `Io(String)`)

- [ ] **Step 1: Enable the `stream` feature in Cargo.toml**

Change `src-tauri/Cargo.toml:136` from:
```toml
chacha20poly1305 = "0.10"
```
to:
```toml
chacha20poly1305 = { version = "0.10", features = ["stream"] }
```

Run: `cd src-tauri && cargo build --lib 2>&1 | tail -5`
Expected: builds (feature resolves; `aead 0.5.2` gains its `stream` module).

- [ ] **Step 2: Write the module with the failing unit tests**

Create `src-tauri/src/file_stream_crypto.rs`:

```rust
//! ZEB-724: streaming chunked-AEAD (RustCrypto STREAM) encryption for large
//! personal files.
//!
//! Layout (v2): a fixed 16-byte header, then a sequence of ChaCha20-Poly1305
//! STREAM frames. Each full frame seals `frame_size` plaintext bytes (+16-byte
//! tag); the final frame uses `encrypt_last` so truncation/reordering fail to
//! decrypt. The per-file DEK is unique, so the STREAM nonce prefix (derived
//! from the DEK) never repeats across files.
//!
//! Header: magic(4)=b"HSF2" ‖ version(1)=0x02 ‖ frame_size(4, u32 BE)
//!         ‖ nonce_prefix(7). = 16 bytes.
//!
//! v1 (ZEB-674 whole-blob `encrypt_blob`) files are NOT readable here: any
//! blob whose magic/version does not match is rejected as
//! `UnsupportedLegacyFormat` (clean break, fail-loud — never garbage).

use crate::owner_state_types::EpochKey;
use chacha20poly1305::aead::generic_array::GenericArray;
use chacha20poly1305::aead::stream::{DecryptorBE32, EncryptorBE32};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit};
use sha2::{Digest, Sha256};

const V2_MAGIC: [u8; 4] = *b"HSF2";
const V2_VERSION: u8 = 0x02;
/// Plaintext bytes sealed per full frame. 64 KiB matches `age`'s STREAM chunk.
pub const DEFAULT_FRAME_SIZE: u32 = 64 * 1024;
/// magic(4) + version(1) + frame_size(4) + nonce_prefix(7).
pub const V2_HEADER_LEN: usize = 16;
const STREAM_TAG_LEN: usize = 16;
const NONCE_PREFIX_LEN: usize = 7; // ChaCha20Poly1305 nonce(12) − BE32 overhead(5)
const NONCE_DERIVE_INFO: &[u8] = b"harmony-file-stream-v2-nonce";

#[derive(Debug, thiserror::Error)]
pub enum FileStreamError {
    #[error("legacy or unsupported encrypted format; re-ingest this file")]
    UnsupportedLegacyFormat,
    #[error("truncated ciphertext")]
    Truncated,
    #[error("invalid frame size")]
    BadFrameSize,
    #[error("aead authentication failed")]
    Aead,
    #[error("io: {0}")]
    Io(String),
}

fn derive_nonce_prefix(dek: &EpochKey) -> [u8; NONCE_PREFIX_LEN] {
    let mut h = Sha256::new();
    h.update(NONCE_DERIVE_INFO);
    h.update(dek.as_bytes());
    let digest = h.finalize();
    let mut out = [0u8; NONCE_PREFIX_LEN];
    out.copy_from_slice(&digest[..NONCE_PREFIX_LEN]);
    out
}

fn v2_header(frame_size: u32, nonce_prefix: &[u8; NONCE_PREFIX_LEN]) -> [u8; V2_HEADER_LEN] {
    let mut hdr = [0u8; V2_HEADER_LEN];
    hdr[0..4].copy_from_slice(&V2_MAGIC);
    hdr[4] = V2_VERSION;
    hdr[5..9].copy_from_slice(&frame_size.to_be_bytes());
    hdr[9..16].copy_from_slice(nonce_prefix);
    hdr
}

/// Exact ciphertext length for a plaintext of `plaintext_len` bytes at
/// `frame_size`. An empty plaintext still emits one (empty) final frame.
pub fn v2_ciphertext_len(plaintext_len: u64, frame_size: u32) -> u64 {
    let fs = frame_size as u64;
    let frames = if plaintext_len == 0 {
        1
    } else {
        (plaintext_len + fs - 1) / fs
    };
    V2_HEADER_LEN as u64 + plaintext_len + frames * STREAM_TAG_LEN as u64
}

/// Seals plaintext frames into v2 STREAM ciphertext. The caller emits
/// `header()` once, then `seal_next` for every non-final frame and `seal_last`
/// for the final frame (which may be empty). `seal_last` consumes the inner
/// STREAM encryptor; further calls error.
pub struct FrameSealer {
    enc: Option<EncryptorBE32<ChaCha20Poly1305>>,
    frame_size: u32,
    nonce_prefix: [u8; NONCE_PREFIX_LEN],
}

impl FrameSealer {
    pub fn new(dek: &EpochKey, frame_size: u32) -> Self {
        let nonce_prefix = derive_nonce_prefix(dek);
        let cipher = ChaCha20Poly1305::new(dek.as_chacha_key());
        let enc = EncryptorBE32::from_aead(cipher, GenericArray::from_slice(&nonce_prefix));
        Self {
            enc: Some(enc),
            frame_size,
            nonce_prefix,
        }
    }

    pub fn header(&self) -> [u8; V2_HEADER_LEN] {
        v2_header(self.frame_size, &self.nonce_prefix)
    }

    pub fn seal_next(&mut self, frame: &[u8]) -> Result<Vec<u8>, FileStreamError> {
        self.enc
            .as_mut()
            .ok_or(FileStreamError::Aead)?
            .encrypt_next(frame)
            .map_err(|_| FileStreamError::Aead)
    }

    pub fn seal_last(&mut self, frame: &[u8]) -> Result<Vec<u8>, FileStreamError> {
        self.enc
            .take()
            .ok_or(FileStreamError::Aead)?
            .encrypt_last(frame)
            .map_err(|_| FileStreamError::Aead)
    }
}

/// Decrypt a whole v2 ciphertext into a plaintext `Vec`.
pub fn decrypt_stream(dek: &EpochKey, ciphertext: &[u8]) -> Result<Vec<u8>, FileStreamError> {
    let mut out = Vec::new();
    decrypt_stream_to_writer(dek, ciphertext, &mut out)?;
    Ok(out)
}

/// Decrypt a whole v2 ciphertext, writing plaintext frame-by-frame to `out`
/// (so the whole plaintext need not be resident). Validates the v2 header and
/// rejects non-v2 blobs as `UnsupportedLegacyFormat`.
pub fn decrypt_stream_to_writer<W: std::io::Write>(
    dek: &EpochKey,
    ciphertext: &[u8],
    out: &mut W,
) -> Result<(), FileStreamError> {
    if ciphertext.len() < V2_HEADER_LEN {
        return Err(FileStreamError::UnsupportedLegacyFormat);
    }
    let (hdr, body) = ciphertext.split_at(V2_HEADER_LEN);
    if hdr[0..4] != V2_MAGIC || hdr[4] != V2_VERSION {
        return Err(FileStreamError::UnsupportedLegacyFormat);
    }
    let frame_size = u32::from_be_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]);
    if frame_size == 0 {
        return Err(FileStreamError::BadFrameSize);
    }
    let mut nonce_prefix = [0u8; NONCE_PREFIX_LEN];
    nonce_prefix.copy_from_slice(&hdr[9..16]);

    let cipher = ChaCha20Poly1305::new(dek.as_chacha_key());
    let mut dec = Some(DecryptorBE32::from_aead(
        cipher,
        GenericArray::from_slice(&nonce_prefix),
    ));

    let ct_frame_len = frame_size as usize + STREAM_TAG_LEN;
    if body.len() < STREAM_TAG_LEN {
        return Err(FileStreamError::Truncated); // need at least one final tag
    }
    let mut pos = 0usize;
    loop {
        let remaining = body.len() - pos;
        if remaining <= ct_frame_len {
            if remaining < STREAM_TAG_LEN {
                return Err(FileStreamError::Truncated);
            }
            let d = dec.take().ok_or(FileStreamError::Aead)?;
            let pt = d
                .decrypt_last(&body[pos..])
                .map_err(|_| FileStreamError::Aead)?;
            out.write_all(&pt).map_err(|e| FileStreamError::Io(e.to_string()))?;
            break;
        }
        let d = dec.as_mut().ok_or(FileStreamError::Aead)?;
        let pt = d
            .decrypt_next(&body[pos..pos + ct_frame_len])
            .map_err(|_| FileStreamError::Aead)?;
        out.write_all(&pt).map_err(|e| FileStreamError::Io(e.to_string()))?;
        pos += ct_frame_len;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Small test frame size so multi-frame cases don't need big inputs.
    const FS: u32 = 32;

    fn dek() -> EpochKey {
        EpochKey::new([0x42u8; 32])
    }

    /// Seal a whole plaintext with the given frame size (test helper mirroring
    /// the producer's lookahead: exactly ceil(len/FS) frames, min 1).
    fn seal_all(dek: &EpochKey, pt: &[u8], frame_size: u32) -> Vec<u8> {
        let mut sealer = FrameSealer::new(dek, frame_size);
        let mut wire = sealer.header().to_vec();
        let fs = frame_size as usize;
        if pt.is_empty() {
            wire.extend_from_slice(&sealer.seal_last(&[]).unwrap());
            return wire;
        }
        let mut chunks = pt.chunks(fs).peekable();
        while let Some(c) = chunks.next() {
            if chunks.peek().is_none() {
                wire.extend_from_slice(&sealer.seal_last(c).unwrap());
            } else {
                wire.extend_from_slice(&sealer.seal_next(c).unwrap());
            }
        }
        wire
    }

    #[test]
    fn round_trip_multi_frame() {
        let d = dek();
        let pt: Vec<u8> = (0..200u32).map(|i| i as u8).collect(); // > 6 frames at FS=32
        let ct = seal_all(&d, &pt, FS);
        assert_eq!(decrypt_stream(&d, &ct).unwrap(), pt);
    }

    #[test]
    fn round_trip_empty() {
        let d = dek();
        let ct = seal_all(&d, &[], FS);
        assert_eq!(ct.len(), V2_HEADER_LEN + STREAM_TAG_LEN); // 16 + 16 = 32
        assert_eq!(decrypt_stream(&d, &ct).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn round_trip_exact_frame_boundary() {
        let d = dek();
        let pt = vec![7u8; (FS as usize) * 3]; // exactly 3 full frames
        let ct = seal_all(&d, &pt, FS);
        // ceil(96/32)=3 frames, no empty trailing frame.
        assert_eq!(ct.len() as u64, v2_ciphertext_len(pt.len() as u64, FS));
        assert_eq!(decrypt_stream(&d, &ct).unwrap(), pt);
    }

    #[test]
    fn round_trip_sub_frame() {
        let d = dek();
        let pt = vec![9u8; (FS as usize) - 1];
        let ct = seal_all(&d, &pt, FS);
        assert_eq!(decrypt_stream(&d, &ct).unwrap(), pt);
    }

    #[test]
    fn truncation_fails() {
        let d = dek();
        let pt = vec![3u8; (FS as usize) * 3];
        let mut ct = seal_all(&d, &pt, FS);
        ct.truncate(ct.len() - (FS as usize + STREAM_TAG_LEN)); // drop the last frame
        assert!(decrypt_stream(&d, &ct).is_err());
    }

    #[test]
    fn reorder_fails() {
        let d = dek();
        let pt = vec![5u8; (FS as usize) * 3];
        let ct = seal_all(&d, &pt, FS);
        let f = FS as usize + STREAM_TAG_LEN;
        // swap frame 0 and frame 1 within the body.
        let mut bad = ct.clone();
        let b = V2_HEADER_LEN;
        bad[b..b + f].copy_from_slice(&ct[b + f..b + 2 * f]);
        bad[b + f..b + 2 * f].copy_from_slice(&ct[b..b + f]);
        assert!(decrypt_stream(&d, &bad).is_err());
    }

    #[test]
    fn single_bit_tamper_fails() {
        let d = dek();
        let pt = vec![1u8; 100];
        let mut ct = seal_all(&d, &pt, FS);
        let i = V2_HEADER_LEN + 1;
        ct[i] ^= 0x01;
        assert!(decrypt_stream(&d, &ct).is_err());
    }

    #[test]
    fn non_v2_blob_rejected() {
        let d = dek();
        // A v1-style whole blob (no HSF2 magic).
        let v1 = crate::community_state_sync::encrypt_blob(&d, b"hello world").unwrap();
        match decrypt_stream(&d, &v1) {
            Err(FileStreamError::UnsupportedLegacyFormat) => {}
            other => panic!("expected UnsupportedLegacyFormat, got {other:?}"),
        }
        // Too-short input also rejected as legacy/unsupported.
        assert!(matches!(
            decrypt_stream(&d, &[0u8; 4]),
            Err(FileStreamError::UnsupportedLegacyFormat)
        ));
    }

    #[test]
    fn wrong_dek_fails() {
        let d = dek();
        let other = EpochKey::new([0x99u8; 32]);
        let ct = seal_all(&d, b"secret payload here", FS);
        assert!(decrypt_stream(&other, &ct).is_err());
    }

    #[test]
    fn ciphertext_len_matches_emission() {
        let d = dek();
        for &l in &[0usize, 1, FS as usize - 1, FS as usize, FS as usize + 1, FS as usize * 4] {
            let pt = vec![0u8; l];
            let ct = seal_all(&d, &pt, FS);
            assert_eq!(ct.len() as u64, v2_ciphertext_len(l as u64, FS), "len {l}");
        }
    }

    #[test]
    fn decrypt_to_writer_streams_plaintext() {
        let d = dek();
        let pt = vec![0xABu8; 250];
        let ct = seal_all(&d, &pt, FS);
        let mut sink = Vec::new();
        decrypt_stream_to_writer(&d, &ct, &mut sink).unwrap();
        assert_eq!(sink, pt);
    }
}
```

Add the module declaration in `src-tauri/src/lib.rs` next to the sibling module declarations (search for `pub mod file_sharing;` and add directly below it):
```rust
pub mod file_stream_crypto;
```

- [ ] **Step 3: Run the tests to verify they compile and pass**

Run: `cd src-tauri && cargo test --lib file_stream_crypto 2>&1 | tail -25`
Expected: all `file_stream_crypto::tests::*` pass. If `EncryptorBE32::from_aead` / `GenericArray::from_slice` paths mismatch the resolved crate, fix the import (`chacha20poly1305::aead::stream` and `chacha20poly1305::aead::generic_array` are the confirmed paths at `chacha20poly1305 0.10.1` / `aead 0.5.2`).

- [ ] **Step 4: Gate check**

Run: `cd src-tauri && cargo fmt --all && cargo clippy --lib --all-targets 2>&1 | tail -15`
Expected: fmt clean, 0 clippy warnings in the new module. (Clippy may suggest `div_ceil`; the manual ceil is intentional for MSRV safety — if clippy flags it, add `#[allow(clippy::manual_div_ceil)]` on `v2_ciphertext_len` with a one-line comment.)

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/file_stream_crypto.rs src-tauri/src/lib.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat(zeb-724): v2 streaming chunked-AEAD frame core"
```

---

### Task 2: Stream encrypted ingest into the chunker; lift the cap

**Files:**
- Modify: `src-tauri/src/lib.rs` — `ingest_content_encrypted_inner` (`:20018`), `ingest_content_encrypted` wrapper (`:20149`); delete `MAX_ENCRYPTED_INGEST_BYTES` (`:20083`), `read_file_capped` (`:20090`) + its test mod (`:20112`); add a private `produce_ciphertext` + `read_up_to` helper near the inner.
- Modify: `src-tauri/Cargo.toml:69` — add `io-util` to the main `tokio` features (for `tokio::io::duplex`).
- Modify test: `src-tauri/tests/file_sharing_dek.rs` — rework the round-trip that assumes "single chunk ⇒ leaf bytes are the whole ciphertext".
- Create test: `src-tauri/tests/file_sharing_streaming.rs` — multi-frame/multi-chunk + frame-boundary + large (ignored) ingest.

**Interfaces:**
- Consumes: Task 1's `FrameSealer`, `DEFAULT_FRAME_SIZE`, `V2_HEADER_LEN`, `decrypt_stream`. `streaming_ingest_with_options<R: AsyncRead + Unpin>(reader, ingest_tx, ChunkerConfig, cancel, IngestOptions) -> Result<(ContentId, u64), IngestError>`. `EpochKey: Clone`.
- Produces: `ingest_content_encrypted_inner(ingest_tx, content_index, crdt_state, keytree, sync_engine, plaintext_reader: tokio::fs::File, file_name) -> Result<IngestResult, String>` (**signature change**: `plaintext: Vec<u8>` → `plaintext_reader: tokio::fs::File`).

- [ ] **Step 1: Add `io-util` to tokio features**

Change `src-tauri/Cargo.toml:69` main tokio line to include `"io-util"`:
```toml
tokio = { version = "1", features = ["rt", "rt-multi-thread", "net", "time", "sync", "macros", "signal", "io-util"] }
```
(Idempotent if already transitively enabled — `streaming_ingest`'s `AsyncReadExt` needs it; making it explicit guarantees `tokio::io::duplex`.)

- [ ] **Step 2: Write the failing streaming round-trip test**

Create `src-tauri/tests/file_sharing_streaming.rs`:

```rust
//! ZEB-724: streaming chunked-AEAD ingest round-trips across MANY frames and
//! MANY FastCDC chunks (unlike the ZEB-674 single-chunk case). Drives the real
//! `ingest_content_encrypted_inner` with a recording store, then reassembles
//! the stored leaves in ingest order and decrypts via the v2 stream decryptor.
//!
//! Keychain-free (ZEB-428): the KeyTree comes from `KeyTree::derive`.

use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_crypto::KeyTree;
use harmony_app::content_index::{self, ContentIndex};
use harmony_app::event_loop::IngestRequest;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

type Store = Arc<Mutex<Vec<(String, Vec<u8>)>>>; // (cid_hex, data) in ingest order

fn spawn_recording_store() -> (tokio::sync::mpsc::Sender<IngestRequest>, Store) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<IngestRequest>(256);
    let store: Store = Arc::new(Mutex::new(Vec::new()));
    let store_c = store.clone();
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            store_c.lock().unwrap().push((req.cid_hex.clone(), req.data));
            let _ = req.reply.send(Ok(()));
        }
    });
    (tx, store)
}

fn fresh_content_index() -> Arc<Mutex<ContentIndex>> {
    let dir = tempfile::tempdir().expect("tempdir");
    let idx = ContentIndex::load(dir.path());
    std::mem::forget(dir);
    Arc::new(Mutex::new(idx))
}

async fn write_temp(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("input.bin");
    tokio::fs::write(&path, bytes).await.unwrap();
    (dir, path)
}

/// Reassemble the DAG the recording store captured. For a bundle tree the leaf
/// order in the store is ingest order (leaves first, then bundles). We fetch
/// the sidecar-declared root and walk it. Simplest robust check: the plaintext
/// is recovered by decrypting the concatenation of the stored LEAF chunks in
/// ingest order — but to be layout-correct we instead reassemble via the same
/// helper the app uses. Here we assert the round trip end-to-end by decrypting
/// the reassembled ciphertext obtained from `harmony_content::dag::reassemble`.
async fn round_trip(plaintext: Vec<u8>) {
    let keytree = KeyTree::derive(&[0x42u8; 32]).expect("keytree");
    let crdt_state = Arc::new(tokio::sync::Mutex::new(OwnerState::default()));
    let content_index = fresh_content_index();
    let (ingest_tx, store) = spawn_recording_store();
    let (_dir, path) = write_temp(&plaintext).await;
    let reader = tokio::fs::File::open(&path).await.unwrap();

    let result = harmony_app::ingest_content_encrypted_inner(
        &ingest_tx,
        &content_index,
        &crdt_state,
        &keytree,
        None,
        reader,
        "big.bin".to_string(),
    )
    .await
    .expect("encrypted streaming ingest succeeds");

    // Recover the DEK the ingest stored, keyed by the root CID.
    let root_bytes = harmony_app::parse_cid_hex(&result.cid).expect("cid hex");
    let sealed = {
        let st = crdt_state.lock().await;
        st.file_deks.get(&root_bytes).cloned().expect("file_deks[root]")
    };
    let dek = harmony_app::file_sharing::open_dek_at_rest(&keytree, &sealed).expect("unseal dek");

    // Reassemble the ciphertext from the recorded chunks via the content DAG,
    // then decrypt with the v2 stream decryptor.
    let ciphertext = reassemble_from_store(&store, &root_bytes);
    let recovered = harmony_app::file_stream_crypto::decrypt_stream(&dek, &ciphertext)
        .expect("v2 decrypt");
    assert_eq!(recovered, plaintext, "streamed round-trip must recover plaintext");
}

/// Reassemble ciphertext from the recording store using the content DAG. The
/// store holds every leaf + bundle keyed by cid_hex; walk the bundle tree from
/// the root exactly as the fetch path's `dag::reassemble` does.
fn reassemble_from_store(store: &Store, root: &[u8; 32]) -> Vec<u8> {
    let map: HashMap<String, Vec<u8>> = store.lock().unwrap().iter().cloned().collect();
    let root_cid = harmony_content::cid::ContentId::from_bytes(*root);
    let getter = |cid: &harmony_content::cid::ContentId| -> Option<Vec<u8>> {
        map.get(&hex::encode(cid.to_bytes())).cloned()
    };
    harmony_content::dag::reassemble(root_cid, &getter).expect("reassemble")
}

#[tokio::test]
async fn streaming_round_trip_multi_frame_multi_chunk() {
    // ~1.5 MiB: crosses many 64 KiB frames AND multiple 256 KiB+ FastCDC chunks.
    let pt: Vec<u8> = (0..(1_500_000u32)).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    round_trip(pt).await;
}

#[tokio::test]
async fn streaming_round_trip_empty_and_boundary() {
    round_trip(Vec::new()).await;
    round_trip(vec![0u8; 64 * 1024]).await; // exactly one frame
    round_trip(vec![7u8; 64 * 1024 + 1]).await; // one frame + 1
}

#[tokio::test]
#[ignore = "slow: >256 MiB; proves the cap is gone + bounded memory. Run with --ignored."]
async fn streaming_ingest_above_old_cap() {
    // 300 MiB > the removed 256 MiB cap. With streaming this must succeed.
    round_trip(vec![0x5Au8; 300 * 1024 * 1024]).await;
}
```

> NOTE for the implementer: `harmony_content::dag::reassemble`'s exact getter signature must be confirmed against the crate (the extraction cited `dag.rs:139`, `reassemble(root_cid, store)`); adapt the `getter`/argument shape to the real signature. The assertion (recovered == plaintext) is the invariant; the reassembly mechanics may differ. If `parse_cid_hex` / `file_sharing` / `file_stream_crypto` are not `pub` at crate root, add the minimal `pub use`/`pub` needed (they are already `pub` per the extraction).

- [ ] **Step 3: Run to verify it fails (inner still takes `Vec<u8>`)**

Run: `cd src-tauri && cargo test --test file_sharing_streaming 2>&1 | tail -20`
Expected: compile error — `ingest_content_encrypted_inner` expects `plaintext: Vec<u8>`, got `tokio::fs::File`. That is the signature we change next.

- [ ] **Step 4: Add the producer helpers + rewrite the inner to stream**

In `src-tauri/src/lib.rs`, add near `ingest_content_encrypted_inner` (above it):

```rust
/// Read up to `buf.len()` bytes, tolerating short reads; returns bytes filled
/// (< len only at EOF).
async fn read_up_to(
    file: &mut tokio::fs::File,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    use tokio::io::AsyncReadExt;
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = file.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// Seal `file`'s plaintext into v2 STREAM frames and write them to `writer`
/// (the write half of a duplex pipe). One-frame lookahead marks the true final
/// frame (so a frame-aligned file emits exactly ceil(len/frame_size) frames).
async fn produce_ciphertext(
    mut file: tokio::fs::File,
    dek: crate::owner_state_types::EpochKey,
    frame_size: u32,
    writer: &mut tokio::io::DuplexStream,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let mut sealer = crate::file_stream_crypto::FrameSealer::new(&dek, frame_size);
    writer
        .write_all(&sealer.header())
        .await
        .map_err(|e| format!("pipe write header: {e}"))?;

    let fs = frame_size as usize;
    let mut cur = vec![0u8; fs];
    let mut cur_len = read_up_to(&mut file, &mut cur)
        .await
        .map_err(|e| format!("read plaintext: {e}"))?;
    loop {
        if cur_len < fs {
            // File ended within this frame ⇒ it is the final frame (maybe empty).
            let ct = sealer.seal_last(&cur[..cur_len]).map_err(|e| e.to_string())?;
            writer.write_all(&ct).await.map_err(|e| format!("pipe write: {e}"))?;
            break;
        }
        // `cur` is a full frame; peek to learn whether more data follows.
        let mut nxt = vec![0u8; fs];
        let nxt_len = read_up_to(&mut file, &mut nxt)
            .await
            .map_err(|e| format!("read plaintext: {e}"))?;
        if nxt_len == 0 {
            let ct = sealer.seal_last(&cur[..cur_len]).map_err(|e| e.to_string())?;
            writer.write_all(&ct).await.map_err(|e| format!("pipe write: {e}"))?;
            break;
        }
        let ct = sealer.seal_next(&cur[..cur_len]).map_err(|e| e.to_string())?;
        writer.write_all(&ct).await.map_err(|e| format!("pipe write: {e}"))?;
        cur = nxt;
        cur_len = nxt_len;
    }
    writer
        .shutdown()
        .await
        .map_err(|e| format!("pipe shutdown: {e}"))?;
    Ok(())
}
```

Replace the body of `ingest_content_encrypted_inner` (lib.rs:20018-20073) with:

```rust
pub async fn ingest_content_encrypted_inner(
    ingest_tx: &tokio::sync::mpsc::Sender<event_loop::IngestRequest>,
    content_index: &std::sync::Arc<std::sync::Mutex<content_index::ContentIndex>>,
    crdt_state: &std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    keytree: &crate::owner_state_crypto::KeyTree,
    sync_engine: Option<&std::sync::Arc<crate::owner_state_sync::SyncEngine>>,
    plaintext_reader: tokio::fs::File,
    file_name: String,
) -> Result<IngestResult, String> {
    use harmony_content::chunker::ChunkerConfig;

    // 1. Fresh per-file DEK.
    let dek = crate::file_sharing::generate_file_dek();
    let frame_size = crate::file_stream_crypto::DEFAULT_FRAME_SIZE;

    // 2. Stream: producer seals plaintext → v2 frames → duplex pipe; the FastCDC
    //    chunker consumes the ciphertext byte-stream. Neither the whole plaintext
    //    nor the whole ciphertext is ever resident (bounded to a few frames).
    let opts = IngestOptions {
        flags: harmony_content::cid::ContentFlags {
            encrypted: true,
            ..Default::default()
        },
        serveable: true,
    };
    let cap = (frame_size as usize + 16) * 2 + crate::file_stream_crypto::V2_HEADER_LEN;
    let (mut pipe_w, pipe_r) = tokio::io::duplex(cap);
    let dek_for_producer = dek.clone();
    let producer = tokio::spawn(async move {
        produce_ciphertext(plaintext_reader, dek_for_producer, frame_size, &mut pipe_w).await
    });
    let ingest_res =
        streaming_ingest_with_options(pipe_r, ingest_tx, ChunkerConfig::DEFAULT, None, opts).await;
    // Join the producer and surface its error BEFORE any state commit — a
    // producer failure would otherwise look like a clean short stream.
    let produce_res = producer
        .await
        .map_err(|e| format!("encrypt task join: {e}"))?;
    produce_res?;
    let (root, size_bytes) = ingest_res.map_err(|e| e.to_string())?;

    // 3. Seal the DEK at rest and store it keyed by the root CID (ZEB-709).
    let sealed = crate::file_sharing::seal_dek_at_rest(keytree, &dek)
        .map_err(|e| format!("seal DEK at rest: {e:?}"))?;
    {
        let mut st = crdt_state.lock().await;
        st.file_deks.insert(root.to_bytes(), sealed);
    }
    if let Some(engine) = sync_engine {
        engine.notify_dirty();
    }

    // 4. Insert the sidecar row pointing at the streamed root CID.
    send_ingest_with_name(content_index, root.to_bytes(), file_name, size_bytes, None)
        .await
        .map_err(|e| e.to_string())
}
```

- [ ] **Step 5: Rewrite the IPC wrapper to open a streaming reader; delete the cap**

In `ingest_content_encrypted` (lib.rs:20149), replace the `read_file_capped` call + inner invocation (the `// 3. Read the whole file...` block through the `ingest_content_encrypted_inner(...)` call) with:

```rust
    // 3. Open a streaming reader; the inner encrypts frame-by-frame (no cap).
    let plaintext_reader = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("open failed: {e}"))?;

    ingest_content_encrypted_inner(
        &ingest_tx,
        &content_index,
        &crdt_state,
        &keytree,
        sync_engine.as_ref(),
        plaintext_reader,
        file_name,
    )
    .await
```

Delete `MAX_ENCRYPTED_INGEST_BYTES` (lib.rs:20083), the whole `read_file_capped` fn (lib.rs:20090-20110), and its `mod read_file_capped_tests` (lib.rs:20112-20142). Confirm no other caller:
Run: `cd src-tauri && grep -rn "read_file_capped\|MAX_ENCRYPTED_INGEST_BYTES" src/ tests/`
Expected: no matches after deletion.

- [ ] **Step 6: Rework the ZEB-674 single-chunk round-trip test**

In `src-tauri/tests/file_sharing_dek.rs`, the `encrypted_ingest_dek_round_trip` test (and the file header comment) assume single-chunk `decrypt_blob`. Update it to: (a) pass a `tokio::fs::File` (write plaintext to a tempfile, open it) instead of a `Vec`; (b) recover the DEK from `file_deks[root]`; (c) reassemble via `harmony_content::dag::reassemble` and decrypt with `harmony_app::file_stream_crypto::decrypt_stream` (not `decrypt_blob`). Reuse the reassembly helper from `file_sharing_streaming.rs` (copy it in, or move both tests to share a small `mod common`). Remove the "single-chunk ⇒ leaf bytes ARE the whole ciphertext" comment and the `assert_eq!(s.len(), 1, ...)` assertion. Keep `sealed_dek_at_rest_is_not_plaintext` (chunking-independent). Any other test in that file that drives `ingest_content_encrypted_inner` with a `Vec` (`owner_encrypted_file_decrypts_to_plaintext`, `received_grant_file_decrypts_to_plaintext`, `encrypted_but_no_personal_dek_passes_through_unchanged`, `tampered_ciphertext_surfaces_error`, `file_deks_persist_reload`) must switch to the tempfile+`File` reader form; those that decrypt should go through the fetch/`decrypt_personal_file_if_held` path reworked in Task 3 — where a test asserts decryption behavior that Task 3 owns, leave a `// reworked in Task 3` marker and keep only the ingest half green here.

- [ ] **Step 7: Build, run, gate**

```bash
cd src-tauri
cargo build --lib 2>&1 | tail -5
cargo test --test file_sharing_streaming 2>&1 | tail -20        # multi-frame/boundary pass (ignored one skipped)
cargo test --test file_sharing_dek 2>&1 | tail -20              # reworked ingest round-trip passes
cargo fmt --all && cargo clippy --lib --all-targets 2>&1 | tail -15
```
Expected: green; 0 clippy warnings. Optionally run the ignored large test locally: `cargo test --test file_sharing_streaming -- --ignored streaming_ingest_above_old_cap` (verifies the cap is truly gone under streaming).

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/Cargo.toml src-tauri/tests/file_sharing_streaming.rs src-tauri/tests/file_sharing_dek.rs
git commit -m "feat(zeb-724): stream encrypted ingest through the chunker; remove 256 MiB cap"
```

---

### Task 3: Decrypt-on-read for the v2 chunked layout

**Files:**
- Modify: `src-tauri/src/lib.rs` — `decrypt_personal_file_if_held` (`:19798`), `export_content` (`:19863`), `fetch_content` (`:24099` — unchanged call, verify), add `resolve_personal_file_dek` helper.
- Modify test: `src-tauri/tests/file_sharing_grantee.rs` — grantee decrypt of a multi-frame file.
- Modify test: `src-tauri/tests/file_sharing_dek.rs` — the decryption-asserting tests deferred from Task 2 Step 6.

**Interfaces:**
- Consumes: Task 1's `decrypt_stream`, `decrypt_stream_to_writer`, `FileStreamError`. Existing DEK lookup (`state.file_deks` / `state.received_file_grants[cid].sealed_dek` → `open_dek_at_rest`).
- Produces: `decrypt_personal_file_if_held` now v2-decrypts (fail-loud on non-v2). `export_content` streams plaintext to disk.

- [ ] **Step 1: Write the failing grantee multi-frame decrypt test**

In `src-tauri/tests/file_sharing_grantee.rs`, add a test that ingests a multi-frame encrypted file (tempfile + `File` reader, ~200 KiB so it crosses several 64 KiB frames), records the grant on a grantee `OwnerState.received_file_grants[cid]` with the sealed DEK, then calls `decrypt_personal_file_if_held(ciphertext, cid, &grantee_state, &keytree)` and asserts it returns the original plaintext. Use the reassembly helper (shared `mod common`). Model the grant/seal setup on the existing `grantee_ingest_then_decrypt` test.

Run: `cd src-tauri && cargo test --test file_sharing_grantee multi_frame 2>&1 | tail -20`
Expected: FAIL — `decrypt_personal_file_if_held` still calls `decrypt_blob`, which cannot parse the v2 layout.

- [ ] **Step 2: Point `decrypt_personal_file_if_held` at the v2 decryptor**

Replace the final two lines of `decrypt_personal_file_if_held` (lib.rs:19819-19821) — the `decrypt_blob` call — with:

```rust
    crate::file_stream_crypto::decrypt_stream(&dek, &bytes)
        .map_err(|e| format!("decrypt personal file: {e}"))
```

(The `FileStreamError::UnsupportedLegacyFormat` Display — "legacy or unsupported encrypted format; re-ingest this file" — surfaces cleanly for any v1 blob. The `if !cid.flags().encrypted` early-return and the DEK-not-held passthrough are unchanged.)

- [ ] **Step 3: Add `resolve_personal_file_dek` and stream export to disk**

Add a helper (near `maybe_decrypt_personal_file`, lib.rs:~19829) that factors out the DEK resolution so export can stream rather than materialize plaintext:

```rust
/// Resolve the unsealed per-file DEK for an encrypted personal CID, if this
/// node holds it (own `file_deks` or a received grant). `None` = not held.
async fn resolve_personal_file_dek(
    state: &Mutex<NodeState>,
    cid: &harmony_content::cid::ContentId,
) -> Result<Option<crate::owner_state_types::EpochKey>, String> {
    if !cid.flags().encrypted {
        return Ok(None);
    }
    let (crdt_state, keytree) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        match (guard.crdt_state.clone(), guard.owner_keytree.clone()) {
            (Some(c), Some(k)) => (c, k),
            _ => return Ok(None),
        }
    };
    let key = cid.to_bytes();
    let st = crdt_state.lock().await;
    let sealed = st
        .file_deks
        .get(&key)
        .or_else(|| st.received_file_grants.get(&key).map(|g| &g.sealed_dek));
    let Some(sealed) = sealed else {
        return Ok(None);
    };
    let dek = crate::file_sharing::open_dek_at_rest(&keytree, sealed)
        .map_err(|e| format!("unseal personal file DEK: {e:?}"))?;
    Ok(Some(dek))
}
```

In `export_content` (lib.rs:19863), keep the fetch as-is (yields whole ciphertext `bytes`), and — after the save dialog yields `path` — replace the current `maybe_decrypt_personal_file` + `tokio::fs::write(path, &bytes)` sequence with a branch that streams plaintext to disk for encrypted-and-held files:

```rust
    let path = file_path
        .as_path()
        .ok_or_else(|| "unsupported file path".to_string())?;

    let cid_parsed = parse_cid_hex(&cid)
        .ok()
        .map(harmony_content::cid::ContentId::from_bytes);
    if let Some(cid_obj) = cid_parsed {
        if let Some(dek) = resolve_personal_file_dek(state.inner(), &cid_obj).await? {
            // Stream-decrypt: ciphertext is already resident; write plaintext
            // frame-by-frame so the whole plaintext is never also resident.
            let out_path = path.to_path_buf();
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let mut f = std::fs::File::create(&out_path)
                    .map_err(|e| format!("create failed: {e}"))?;
                crate::file_stream_crypto::decrypt_stream_to_writer(&dek, &bytes, &mut f)
                    .map_err(|e| format!("decrypt personal file: {e}"))?;
                f.flush().map_err(|e| format!("flush failed: {e}"))?;
                Ok(())
            })
            .await
            .map_err(|e| format!("decrypt task: {e}"))??;
            return Ok(true);
        }
    }

    // Public (or encrypted-but-not-held) content: write bytes unchanged.
    tokio::fs::write(path, &bytes)
        .await
        .map_err(|e| format!("write failed: {e}"))?;

    Ok(true)
```

Remove the now-unused `// 1b. decrypt to plaintext before writing` line (the earlier `maybe_decrypt_personal_file` call in `export_content`) so decryption happens only in the branch above. `fetch_content` (lib.rs:24099) keeps calling `maybe_decrypt_personal_file` and returns a whole plaintext `Vec` — verify it compiles unchanged (it now routes through the v2 `decrypt_stream` via Step 2).

> `use std::io::Write as _;` may be needed in `export_content`'s scope for `f.flush()` — add it locally if the compiler asks.

- [ ] **Step 4: Run the grantee + dek decryption tests**

Run:
```bash
cd src-tauri
cargo test --test file_sharing_grantee 2>&1 | tail -20
cargo test --test file_sharing_dek 2>&1 | tail -20   # the deferred decryption asserts now pass
```
Expected: green (multi-frame grantee decrypt recovers plaintext; owner/received decrypt tests pass through the v2 path; the tampered-ciphertext test still surfaces an error via `FileStreamError::Aead`).

- [ ] **Step 5: Gate**

Run: `cd src-tauri && cargo fmt --all && cargo clippy --lib --all-targets 2>&1 | tail -15`
Expected: fmt clean, 0 clippy warnings.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/file_sharing_grantee.rs src-tauri/tests/file_sharing_dek.rs
git commit -m "feat(zeb-724): decrypt-on-read for v2 chunked layout; stream export to disk"
```

---

### Task 4: Grant-metadata overhead accuracy (doc + accounting)

**Files:**
- Modify: `src-tauri/src/file_sharing.rs` (doc comments at `:76`, `:370`), `src-tauri/src/owner_state_types.rs` (doc comment at `:2586`).
- Verify-only grep across the grant/share path.

**Interfaces:**
- Consumes: Task 1's `v2_ciphertext_len`.

- [ ] **Step 1: Verify no personal-file code assumes the flat +28 overhead**

Run:
```bash
cd src-tauri
grep -rn "BLOB_ENCRYPTION_OVERHEAD" src/          # confirm only community_state_sync + the CHANNEL-artifact site (lib.rs:29994/30058)
grep -rn "28\b\|saturating_sub\|- 28\|+ 28" src/file_sharing.rs src/owner_state_types.rs
grep -rn "file_size" src/file_sharing.rs           # where FileGrantInner.file_size is POPULATED at share time
```
Confirm: (a) the only *code* use of the flat overhead is the channel-artifact `authorize_and_fetch_artifact` path (`lib.rs:29994`) — **out of scope, do not touch**; (b) `FileGrantInner.file_size` / `ReceivedFileGrant.file_size` are populated from the file's actual stored size (sidecar/CAS size), not computed as `plaintext + 28`. If a populate-site DOES compute `plaintext.len() + 28`, change it to `crate::file_stream_crypto::v2_ciphertext_len(plaintext_len, DEFAULT_FRAME_SIZE)`; if it reads the actual stored size, no code change (the value is naturally correct for v2).

- [ ] **Step 2: Fix the stale "plaintext + 28" doc comments**

Update the three doc comments (`file_sharing.rs:76`, `file_sharing.rs:370`, `owner_state_types.rs:2586`) from the v1 whole-blob wording to describe the v2 chunked overhead. Replace each "…plaintext length + 28" sentence with:

```
/// Stored (CAS) byte length of the file's content. For v2 streaming-encrypted
/// content this is the chunked-AEAD ciphertext length: a 16-byte header plus,
/// per 64 KiB frame, a 16-byte tag (see `file_stream_crypto::v2_ciphertext_len`),
/// so it exceeds the plaintext length by the header + per-frame tag overhead.
```

- [ ] **Step 3: Gate + confirm the grant tests still pass**

Run:
```bash
cd src-tauri
cargo test --test file_sharing_grants 2>&1 | tail -15
cargo fmt --all && cargo clippy --lib --all-targets 2>&1 | tail -15
```
Expected: green. If `file_sharing_grants.rs` hardcodes a `file_size` = plaintext+28 expectation anywhere, update it to `v2_ciphertext_len(...)`.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/file_sharing.rs src-tauri/src/owner_state_types.rs src-tauri/tests/file_sharing_grants.rs
git commit -m "docs(zeb-724): grant file_size overhead reflects v2 chunked layout"
```

---

## Final gates (whole-branch, before PR)

```bash
cd src-tauri
cargo fmt --all -- --check
# CI-exact (see CLAUDE.md): --locked + --features test-fixtures are load-bearing
# (integration targets only compile with the feature; --locked pins the dep graph).
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -30 # all green (ignored large test excluded)
```
Frontend is untouched (no `src/` changes), so `tsc`/`vitest` are not required for this branch — but run `npx tsc --noEmit` once to be safe if any `.ts` was touched (it should not be).

## Out of scope (follow-ups to file after merge)

- Incremental **streaming read** (bounded read memory; rebuild `dag::reassemble`/fetch pipeline; affects public files too).
- **Folder-ingest** encryption (now unblocked — each descendant leaf streamed + encrypted).
- v1→v2 re-ingest tooling (clean break ⇒ none for MVP).
