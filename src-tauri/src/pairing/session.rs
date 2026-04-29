use chacha20poly1305::{
    aead::{Aead, KeyInit, OsRng},
    AeadCore, XChaCha20Poly1305, XNonce,
};

/// Encrypt a payload under the session_key. Returns (nonce, ciphertext).
/// Nonce is a fresh 24-byte XChaCha20 nonce per call.
pub fn encrypt(session_key: &[u8; 32], plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
    let cipher = XChaCha20Poly1305::new(session_key.into());
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| format!("encrypt: {e}"))?;
    Ok((nonce.to_vec(), ciphertext))
}

/// Decrypt a payload under the session_key. Returns the plaintext.
pub fn decrypt(session_key: &[u8; 32], nonce: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    if nonce.len() != 24 {
        return Err(format!("nonce must be 24 bytes, got {}", nonce.len()));
    }
    let cipher = XChaCha20Poly1305::new(session_key.into());
    let nonce = XNonce::from_slice(nonce);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| format!("decrypt: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let key = [0x42u8; 32];
        let pt = b"hello world".to_vec();
        let (nonce, ct) = encrypt(&key, &pt).unwrap();
        let back = decrypt(&key, &nonce, &ct).unwrap();
        assert_eq!(back, pt);
    }

    #[test]
    fn wrong_key_fails() {
        let key = [0x42u8; 32];
        let pt = b"hello world".to_vec();
        let (nonce, ct) = encrypt(&key, &pt).unwrap();
        let bad_key = [0x43u8; 32];
        assert!(decrypt(&bad_key, &nonce, &ct).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = [0x42u8; 32];
        let pt = b"hello world".to_vec();
        let (nonce, mut ct) = encrypt(&key, &pt).unwrap();
        ct[0] ^= 0x01; // flip a bit
        assert!(decrypt(&key, &nonce, &ct).is_err());
    }

    #[test]
    fn wrong_nonce_length_fails() {
        let key = [0x42u8; 32];
        let bad = vec![0u8; 12]; // ChaCha20 nonce, not XChaCha20
        let ct = vec![0u8; 16];
        assert!(decrypt(&key, &bad, &ct).is_err());
    }
}
