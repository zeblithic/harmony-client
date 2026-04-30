//! Owner-state encryption primitives per ZEB-211.
//!
//! See `docs/specs/2026-04-30-zeb-211-owner-state-encryption-design.md`.
//!
//! This module is pure crypto — no I/O, no Space/OutboxEntry knowledge.
//! Phase 2 of ZEB-215 wires it into the CRDT layer.

use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, KeyInit, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::Zeroizing;
use rand::rngs::OsRng;
use rand::RngCore;
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("HKDF expand failed: {0}")]
    Hkdf(String),
    #[error("AEAD encryption failed")]
    AeadEncrypt,
    #[error("AEAD decryption failed (forged or wrong key)")]
    AeadDecrypt,
    #[error("CBOR encode failed: {0}")]
    CborEncode(String),
    #[error("CBOR decode failed: {0}")]
    CborDecode(String),
    #[error("Replay rejected: at HLC not strictly newer than last accepted from device {0}")]
    ReplayRejected(String),
}

#[cfg(test)]
mod tests {
    // Tests added in subsequent tasks.
}
