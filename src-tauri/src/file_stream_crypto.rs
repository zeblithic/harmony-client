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
#[allow(clippy::manual_div_ceil)] // MSRV-safe manual ceil
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
            out.write_all(&pt)
                .map_err(|e| FileStreamError::Io(e.to_string()))?;
            break;
        }
        let d = dec.as_mut().ok_or(FileStreamError::Aead)?;
        let pt = d
            .decrypt_next(&body[pos..pos + ct_frame_len])
            .map_err(|_| FileStreamError::Aead)?;
        out.write_all(&pt)
            .map_err(|e| FileStreamError::Io(e.to_string()))?;
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
