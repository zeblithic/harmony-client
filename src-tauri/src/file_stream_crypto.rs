//! ZEB-724 / ZEB-726: streaming chunked-AEAD (RustCrypto STREAM) encryption for
//! large personal files.
//!
//! Layout (v3): a fixed 9-byte header, then a sequence of ChaCha20-Poly1305
//! STREAM frames. Each full frame seals `frame_size` plaintext bytes (+16-byte
//! tag); the final frame uses `encrypt_last` so truncation/reordering fail to
//! decrypt. The per-file DEK is unique, so the STREAM nonce prefix (derived
//! from the DEK) never repeats across files.
//!
//! Header: magic(4)=b"HSF3" ‖ version(1)=0x03 ‖ frame_size(4, u32 BE). = 9 bytes.
//!
//! v3 hardening (ZEB-726): the 9-byte header is bound as AEAD associated data on
//! the FIRST STREAM frame (whether that frame is `encrypt_next` or the sole
//! `encrypt_last`), so any tamper with the magic/version/frame_size fields is
//! detected — a v2 attacker could rewrite the header's framing/nonce fields
//! undetected. The nonce prefix is NO LONGER carried in the header: it is
//! re-derived deterministically from the DEK on both the seal and decrypt sides
//! (info string bumped to v3), removing an attacker-malleable header field
//! entirely.
//!
//! v2 (`HSF2`) and v1 (ZEB-674 whole-blob `encrypt_blob`) files are NOT readable
//! here: any blob whose magic/version does not match v3 is rejected as
//! `UnsupportedLegacyFormat` (clean break, fail-loud — never garbage).

use crate::owner_state_types::EpochKey;
use chacha20poly1305::aead::generic_array::GenericArray;
use chacha20poly1305::aead::stream::{DecryptorBE32, EncryptorBE32};
use chacha20poly1305::aead::Payload;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit};
use sha2::{Digest, Sha256};
use std::io::Write as _;

const V3_MAGIC: [u8; 4] = *b"HSF3";
const V3_VERSION: u8 = 0x03;
/// Plaintext bytes sealed per full frame. 64 KiB matches `age`'s STREAM chunk.
pub const DEFAULT_FRAME_SIZE: u32 = 64 * 1024;
/// magic(4) + version(1) + frame_size(4). No nonce prefix (re-derived from DEK).
pub const V3_HEADER_LEN: usize = 9;
const STREAM_TAG_LEN: usize = 16;
const NONCE_PREFIX_LEN: usize = 7; // ChaCha20Poly1305 nonce(12) − BE32 overhead(5)
const NONCE_DERIVE_INFO: &[u8] = b"harmony-file-stream-v3-nonce";

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

fn v3_header(frame_size: u32) -> [u8; V3_HEADER_LEN] {
    let mut hdr = [0u8; V3_HEADER_LEN];
    hdr[0..4].copy_from_slice(&V3_MAGIC);
    hdr[4] = V3_VERSION;
    hdr[5..9].copy_from_slice(&frame_size.to_be_bytes());
    hdr
}

/// Exact ciphertext length for a plaintext of `plaintext_len` bytes at
/// `frame_size`. An empty plaintext still emits one (empty) final frame.
/// Fallible: rejects `frame_size == 0` (which cannot frame any data) as
/// `BadFrameSize`.
#[allow(clippy::manual_div_ceil)] // MSRV-safe manual ceil
pub fn v3_ciphertext_len(plaintext_len: u64, frame_size: u32) -> Result<u64, FileStreamError> {
    if frame_size == 0 {
        return Err(FileStreamError::BadFrameSize);
    }
    let fs = frame_size as u64;
    let frames = if plaintext_len == 0 {
        1
    } else {
        (plaintext_len + fs - 1) / fs
    };
    Ok(V3_HEADER_LEN as u64 + plaintext_len + frames * STREAM_TAG_LEN as u64)
}

/// Seals plaintext frames into v3 STREAM ciphertext. The caller emits
/// `header()` once, then `seal_next` for every non-final frame and `seal_last`
/// for the final frame (which may be empty). The 9-byte header is bound as AEAD
/// associated data on the FIRST sealed frame. `seal_last` consumes the inner
/// STREAM encryptor; further calls error.
pub struct FrameSealer {
    enc: Option<EncryptorBE32<ChaCha20Poly1305>>,
    frame_size: u32,
    header_bytes: [u8; V3_HEADER_LEN],
    /// Latch: the header is bound as AEAD associated data on the FIRST sealed
    /// frame only. A `bool` (not a frame counter) so the empty-plaintext case —
    /// which emits exactly one final frame via `seal_last` — is handled.
    header_bound: bool,
}

impl FrameSealer {
    /// Build a sealer for `dek` at `frame_size`. Rejects `frame_size == 0` as
    /// `BadFrameSize` (a zero frame can never advance the stream).
    pub fn new(dek: &EpochKey, frame_size: u32) -> Result<Self, FileStreamError> {
        if frame_size == 0 {
            return Err(FileStreamError::BadFrameSize);
        }
        let nonce_prefix = derive_nonce_prefix(dek);
        let cipher = ChaCha20Poly1305::new(dek.as_chacha_key());
        let enc = EncryptorBE32::from_aead(cipher, GenericArray::from_slice(&nonce_prefix));
        Ok(Self {
            enc: Some(enc),
            frame_size,
            header_bytes: v3_header(frame_size),
            header_bound: false,
        })
    }

    pub fn header(&self) -> [u8; V3_HEADER_LEN] {
        self.header_bytes
    }

    pub fn seal_next(&mut self, frame: &[u8]) -> Result<Vec<u8>, FileStreamError> {
        if frame.len() != self.frame_size as usize {
            return Err(FileStreamError::BadFrameSize);
        }
        // `[u8; 9]` is `Copy`; take a local so the header AAD borrow doesn't
        // collide with the `&mut self.enc` borrow below.
        let hdr = self.header_bytes;
        let bind = !self.header_bound;
        let enc = self.enc.as_mut().ok_or(FileStreamError::Aead)?;
        let ct = if bind {
            enc.encrypt_next(Payload {
                msg: frame,
                aad: &hdr,
            })
        } else {
            enc.encrypt_next(frame)
        }
        .map_err(|_| FileStreamError::Aead)?;
        self.header_bound = true;
        Ok(ct)
    }

    pub fn seal_last(&mut self, frame: &[u8]) -> Result<Vec<u8>, FileStreamError> {
        if frame.len() > self.frame_size as usize {
            return Err(FileStreamError::BadFrameSize);
        }
        let hdr = self.header_bytes;
        let bind = !self.header_bound;
        let enc = self.enc.take().ok_or(FileStreamError::Aead)?;
        let ct = if bind {
            enc.encrypt_last(Payload {
                msg: frame,
                aad: &hdr,
            })
        } else {
            enc.encrypt_last(frame)
        }
        .map_err(|_| FileStreamError::Aead)?;
        self.header_bound = true;
        Ok(ct)
    }
}

/// Decrypt a whole v3 ciphertext into a plaintext `Vec`.
pub fn decrypt_stream(dek: &EpochKey, ciphertext: &[u8]) -> Result<Vec<u8>, FileStreamError> {
    let mut out = Vec::new();
    decrypt_stream_to_writer(dek, ciphertext, &mut out)?;
    Ok(out)
}

/// Decrypt a whole v3 ciphertext, writing plaintext frame-by-frame to `out`
/// (so the whole plaintext need not be resident). Validates the v3 header,
/// re-derives the nonce prefix from `dek`, binds the header as AEAD associated
/// data on the first frame, and rejects non-v3 blobs as
/// `UnsupportedLegacyFormat`.
pub fn decrypt_stream_to_writer<W: std::io::Write>(
    dek: &EpochKey,
    ciphertext: &[u8],
    out: &mut W,
) -> Result<(), FileStreamError> {
    if ciphertext.len() < V3_HEADER_LEN {
        return Err(FileStreamError::UnsupportedLegacyFormat);
    }
    let (hdr, body) = ciphertext.split_at(V3_HEADER_LEN);
    if hdr[0..4] != V3_MAGIC || hdr[4] != V3_VERSION {
        return Err(FileStreamError::UnsupportedLegacyFormat);
    }
    let frame_size = u32::from_be_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]);
    if frame_size == 0 {
        return Err(FileStreamError::BadFrameSize);
    }
    let nonce_prefix = derive_nonce_prefix(dek);

    let cipher = ChaCha20Poly1305::new(dek.as_chacha_key());
    let mut dec = Some(DecryptorBE32::from_aead(
        cipher,
        GenericArray::from_slice(&nonce_prefix),
    ));

    let ct_frame_len = (frame_size as usize)
        .checked_add(STREAM_TAG_LEN)
        .ok_or(FileStreamError::BadFrameSize)?;
    if body.len() < STREAM_TAG_LEN {
        return Err(FileStreamError::Truncated); // need at least one final tag
    }
    // The 9-byte header is bound as AEAD associated data on the FIRST frame
    // only — mirror the sealer exactly, or every decrypt fails.
    let mut header_bound = false;
    let mut pos = 0usize;
    loop {
        let remaining = body.len() - pos;
        if remaining <= ct_frame_len {
            if remaining < STREAM_TAG_LEN {
                return Err(FileStreamError::Truncated);
            }
            let d = dec.take().ok_or(FileStreamError::Aead)?;
            let pt = if !header_bound {
                d.decrypt_last(Payload {
                    msg: &body[pos..],
                    aad: hdr,
                })
            } else {
                d.decrypt_last(&body[pos..])
            }
            .map_err(|_| FileStreamError::Aead)?;
            out.write_all(&pt)
                .map_err(|e| FileStreamError::Io(e.to_string()))?;
            break;
        }
        let d = dec.as_mut().ok_or(FileStreamError::Aead)?;
        let pt = if !header_bound {
            d.decrypt_next(Payload {
                msg: &body[pos..pos + ct_frame_len],
                aad: hdr,
            })
        } else {
            d.decrypt_next(&body[pos..pos + ct_frame_len])
        }
        .map_err(|_| FileStreamError::Aead)?;
        header_bound = true;
        out.write_all(&pt)
            .map_err(|e| FileStreamError::Io(e.to_string()))?;
        pos += ct_frame_len;
    }
    Ok(())
}

/// Decrypt v3 ciphertext directly onto `final_path` without ever creating or
/// truncating the caller's chosen file until decryption has fully succeeded.
///
/// Decrypts into a [`tempfile::NamedTempFile`] created in `final_path`'s
/// parent directory (same filesystem, so the final rename is atomic),
/// flushes + fsyncs it, then persists (renames) it onto `final_path` — only
/// once decrypt has produced a complete, authenticated plaintext. After the
/// rename succeeds we best-effort fsync the parent directory so the rename
/// itself is durable across a crash (errors ignored — the export is
/// user-re-triggerable). On ANY error (bad header, truncated/tampered
/// ciphertext, or I/O failure) the temp file is removed (via `NamedTempFile`'s
/// drop-deletes-on-error behavior, including the file `tempfile::PersistError`
/// hands back) and `final_path` is left completely untouched — an existing
/// file there is neither truncated nor partially overwritten. See ZEB-724
/// whole-branch review (data-loss MUST-FIX): the previous `export_content`
/// path did `File::create(&out_path)` (truncating to 0 bytes) BEFORE
/// attempting decrypt, which clobbered the user's chosen file on every
/// legacy-v1 export (deterministic `UnsupportedLegacyFormat`) and on any
/// tampered ciphertext.
pub fn decrypt_stream_to_path(
    dek: &EpochKey,
    ciphertext: &[u8],
    final_path: &std::path::Path,
) -> Result<(), FileStreamError> {
    let dir = match final_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => std::path::Path::new("."),
    };
    let mut tmp =
        tempfile::NamedTempFile::new_in(dir).map_err(|e| FileStreamError::Io(e.to_string()))?;
    decrypt_stream_to_writer(dek, ciphertext, &mut tmp)?;
    tmp.flush()
        .map_err(|e| FileStreamError::Io(e.to_string()))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| FileStreamError::Io(e.to_string()))?;
    // `persist` renames the temp file onto `final_path`. On failure it hands
    // the still-live `NamedTempFile` back inside `PersistError`; we discard
    // it here, which drops (and thus removes) the temp file.
    tmp.persist(final_path)
        .map_err(|e| FileStreamError::Io(e.to_string()))?;
    // Best-effort: fsync the parent directory so the rename is persisted across
    // a crash. Ignore any error — the export is user-re-triggerable.
    let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
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
        let mut sealer = FrameSealer::new(dek, frame_size).unwrap();
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
        assert_eq!(ct.len(), V3_HEADER_LEN + STREAM_TAG_LEN); // 9 + 16 = 25
        assert_eq!(decrypt_stream(&d, &ct).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn round_trip_exact_frame_boundary() {
        let d = dek();
        let pt = vec![7u8; (FS as usize) * 3]; // exactly 3 full frames
        let ct = seal_all(&d, &pt, FS);
        // ceil(96/32)=3 frames, no empty trailing frame.
        assert_eq!(
            ct.len() as u64,
            v3_ciphertext_len(pt.len() as u64, FS).unwrap()
        );
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
        let b = V3_HEADER_LEN;
        bad[b..b + f].copy_from_slice(&ct[b + f..b + 2 * f]);
        bad[b + f..b + 2 * f].copy_from_slice(&ct[b..b + f]);
        assert!(decrypt_stream(&d, &bad).is_err());
    }

    #[test]
    fn single_bit_tamper_fails() {
        let d = dek();
        let pt = vec![1u8; 100];
        let mut ct = seal_all(&d, &pt, FS);
        let i = V3_HEADER_LEN + 1;
        ct[i] ^= 0x01;
        assert!(decrypt_stream(&d, &ct).is_err());
    }

    /// ZEB-726: the 9-byte header is bound as AEAD associated data on the first
    /// frame, so tampering with the `frame_size` field (hdr[5..9]) is detected.
    /// Uses a single-frame plaintext and ENLARGES frame_size so that, absent the
    /// AAD binding, decrypt would still frame the (unchanged) body as one final
    /// frame and succeed — i.e. the AAD binding is the ONLY thing that rejects
    /// this tamper (RED without it, GREEN with it).
    #[test]
    fn header_tamper_fails() {
        let d = dek();
        let pt = vec![1u8; 10]; // single frame at FS=32
        let mut ct = seal_all(&d, &pt, FS);
        // Flip the MSB of the frame_size field: 0x0000_0020 -> 0x0100_0020.
        ct[5] ^= 0x01;
        assert!(decrypt_stream(&d, &ct).is_err());
    }

    #[test]
    fn v3_ciphertext_len_rejects_zero_frame_size() {
        assert!(matches!(
            v3_ciphertext_len(10, 0),
            Err(FileStreamError::BadFrameSize)
        ));
    }

    #[test]
    fn seal_next_wrong_length_rejected() {
        let d = dek();
        let mut sealer = FrameSealer::new(&d, FS).unwrap();
        // seal_next requires EXACTLY frame_size bytes.
        assert!(matches!(
            sealer.seal_next(&[0u8; (FS as usize) - 1]),
            Err(FileStreamError::BadFrameSize)
        ));
    }

    #[test]
    fn seal_last_over_length_rejected() {
        let d = dek();
        let mut sealer = FrameSealer::new(&d, FS).unwrap();
        // seal_last accepts <= frame_size; over-length is rejected.
        assert!(matches!(
            sealer.seal_last(&[0u8; (FS as usize) + 1]),
            Err(FileStreamError::BadFrameSize)
        ));
    }

    #[test]
    fn new_rejects_zero_frame_size() {
        let d = dek();
        assert!(matches!(
            FrameSealer::new(&d, 0),
            Err(FileStreamError::BadFrameSize)
        ));
    }

    #[test]
    fn non_v3_blob_rejected() {
        let d = dek();
        // A v1-style whole blob (no HSF3 magic).
        let v1 = crate::community_state_sync::encrypt_blob(&d, b"hello world").unwrap();
        match decrypt_stream(&d, &v1) {
            Err(FileStreamError::UnsupportedLegacyFormat) => {}
            other => panic!("expected UnsupportedLegacyFormat, got {other:?}"),
        }
        // A v2-format blob (HSF2 magic / version 0x02) is a clean break: no v2
        // backward compatibility, rejected as legacy/unsupported.
        let mut v2 = seal_all(&d, b"payload here", FS);
        v2[3] = b'2'; // HSF3 -> HSF2
        v2[4] = 0x02; // version 0x03 -> 0x02
        assert!(matches!(
            decrypt_stream(&d, &v2),
            Err(FileStreamError::UnsupportedLegacyFormat)
        ));
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
        for &l in &[
            0usize,
            1,
            FS as usize - 1,
            FS as usize,
            FS as usize + 1,
            FS as usize * 4,
        ] {
            let pt = vec![0u8; l];
            let ct = seal_all(&d, &pt, FS);
            assert_eq!(
                ct.len() as u64,
                v3_ciphertext_len(l as u64, FS).unwrap(),
                "len {l}"
            );
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

    /// ZEB-724 whole-branch review (data-loss MUST-FIX): a successful
    /// `decrypt_stream_to_path` writes the full plaintext to `final_path` and
    /// leaves no temp file behind in the directory.
    #[test]
    fn decrypt_to_path_success_writes_plaintext() {
        let d = dek();
        let pt: Vec<u8> = (0..300u32).map(|i| i as u8).collect(); // multi-frame at FS=32
        let ct = seal_all(&d, &pt, FS);
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.bin");

        decrypt_stream_to_path(&d, &ct, &final_path).unwrap();

        assert_eq!(std::fs::read(&final_path).unwrap(), pt);
        // No stray temp file left in the directory.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("out.bin")]);
    }

    /// ZEB-724 whole-branch review (data-loss MUST-FIX): the previous
    /// `export_content` path did `File::create(&out_path)` — truncating the
    /// user's chosen file to 0 bytes — BEFORE attempting decrypt, so a
    /// deterministic legacy export or a tampered ciphertext clobbered the
    /// user's chosen file. `decrypt_stream_to_path` must never touch
    /// `final_path` until decrypt has fully succeeded: on failure, the
    /// existing file at `final_path` is left byte-for-byte untouched and no
    /// leftover temp file remains in the directory.
    #[test]
    fn decrypt_to_path_failure_preserves_existing_file() {
        let d = dek();
        let pt = vec![4u8; (FS as usize) * 3];
        let mut ct = seal_all(&d, &pt, FS);
        // Tamper a body byte so decrypt fails with an AEAD auth error.
        let i = V3_HEADER_LEN + 1;
        ct[i] ^= 0x01;

        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.bin");
        std::fs::write(&final_path, b"KEEP ME").unwrap();

        let err = decrypt_stream_to_path(&d, &ct, &final_path).unwrap_err();
        assert!(matches!(err, FileStreamError::Aead));

        // Final path is completely untouched.
        assert_eq!(std::fs::read(&final_path).unwrap(), b"KEEP ME");
        // No leftover temp file in the directory.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("out.bin")]);
    }

    /// Same as above but for the OTHER deterministic failure mode called out
    /// in ZEB-724: exporting a legacy v1 whole-blob file (which still has a
    /// live DEK, so decrypt is attempted and immediately rejected).
    #[test]
    fn decrypt_to_path_legacy_v1_preserves_existing_file() {
        let d = dek();
        let v1 = crate::community_state_sync::encrypt_blob(&d, b"hello world").unwrap();

        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.bin");
        std::fs::write(&final_path, b"KEEP ME").unwrap();

        let err = decrypt_stream_to_path(&d, &v1, &final_path).unwrap_err();
        assert!(matches!(err, FileStreamError::UnsupportedLegacyFormat));

        assert_eq!(std::fs::read(&final_path).unwrap(), b"KEEP ME");
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("out.bin")]);
    }
}
