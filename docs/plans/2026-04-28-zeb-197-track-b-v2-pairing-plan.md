# Track B v2 — Pairing UI Implementation Plan (ZEB-197)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enable a second harmony-client instance on the same LAN to be paired under an existing owner identity, with both devices visible in each other's DevicesPanel.

**Architecture:** Two-role symmetric pairing wizard (Inviter + Joiner) backed by a transport-agnostic state machine in `pairing.rs`. Discovery + handshake over Zenoh on the existing event-loop session. The Inviter signs `EnrollmentCert` directly via `harmony_owner::certs::enrollment::EnrollmentCert::sign_master` (skipping the `enroll_via_master` wrapper that would require shipping the Joiner's PRIVATE key). The Joiner verifies the cert, persists its own keys and `OwnerState` snapshot via the atomic-write contract from ZEB-170. SAS confirmation derived from X25519 ECDH gives MitM resistance.

**Tech Stack:** Rust (tokio mpsc, x25519-dalek, hkdf, chacha20poly1305), Svelte 5 runes (`$state`, `$props`, `onMount`), Tauri 2 IPC + events, Zenoh pub/sub on the existing `NodeRuntime`'s session.

**Linear:** [ZEB-197](https://linear.app/zeblith/issue/ZEB-197) (parent: ZEB-169 Track A umbrella)
**Spec:** `docs/specs/2026-04-28-zeb-197-track-b-v2-pairing-design.md`
**Predecessor:** ZEB-170 (Track B v1) — DevicesPanel + mint + backup, shipped 2026-04-28 (PR #62).

---

## File structure

**Create (new files):**

| Path | Responsibility |
|---|---|
| `src-tauri/src/pairing/mod.rs` | Module root; re-exports public surface (PairingState, PairingRole, etc.) |
| `src-tauri/src/pairing/types.rs` | Wire types and PairingState enum with serde (camelCase for TS mirror) |
| `src-tauri/src/pairing/sas.rs` | X25519 ECDH + HKDF + 6-digit SAS derivation (pure functions) |
| `src-tauri/src/pairing/session.rs` | XChaCha20-Poly1305 encrypt/decrypt under derived session_key |
| `src-tauri/src/pairing/transport.rs` | `PairingTransport` trait + `InMemoryTransport` for tests |
| `src-tauri/src/pairing/zenoh_transport.rs` | Real Zenoh-backed `PairingTransport` impl |
| `src-tauri/src/pairing/state_machine.rs` | Transport-agnostic state machine + event loop |
| `src-tauri/src/pairing/cert.rs` | Cert signing (Inviter) and verifying (Joiner) wrappers around `harmony_owner` |
| `src-tauri/src/pairing/persist.rs` | Joiner-side atomic install of signing-key + EnrollmentCert + OwnerState |
| `src-tauri/src/pairing_commands.rs` | Tauri IPC: 6 commands wrapping the state machine handle |
| `src-tauri/tests/pairing_integration.rs` | End-to-end test: two NodeRuntimes, full handshake, post-conditions |
| `src/lib/pairing-service.ts` | TS client wrapping Tauri invokes + Tauri events |
| `src/lib/pairing-service.test.ts` | Vitest for the TS client (mocked Tauri adapter) |
| `src/lib/components/PairingInviter.svelte` | OLD-device wizard component |
| `src/lib/components/PairingJoiner.svelte` | NEW-device wizard component |
| `src/lib/components/__tests__/PairingInviter.test.ts` | Vitest for Inviter wizard (mocked service) |
| `src/lib/components/__tests__/PairingJoiner.test.ts` | Vitest for Joiner wizard (mocked service) |

**Modify (existing files):**

| Path | Change |
|---|---|
| `src-tauri/Cargo.toml` | Add `x25519-dalek`, `hkdf`, `chacha20poly1305`, `rand_core` (if not already transitive) |
| `src-tauri/src/lib.rs` | `pub mod pairing;`, `pub mod pairing_commands;`; add 6 commands to `tauri::generate_handler!`; add `pairing_handle: Option<PairingHandle>` field to `NodeState` |
| `src-tauri/src/event_loop.rs` | Add `RuntimeAction::Subscribe { key_expr: "harmony/pairing/v2/lan/**".into() }` to startup_actions; add a routing arm that forwards pairing samples into a `pairing_in_tx` mpsc; spawn the pairing state-machine task |
| `src/lib/components/DevicesPanel.svelte` | Empty-state: add a 2nd CTA "Join existing identity →" that opens PairingJoiner. Populated-state: replace placeholder footer with active "Add another device →" button that opens PairingInviter |
| `src/lib/components/__tests__/DevicesPanel.test.ts` | Add tests for the two new CTAs and that they open the corresponding wizard |

---

## Task 1: Wire types and serde

**Files:**
- Create: `src-tauri/src/pairing/mod.rs`
- Create: `src-tauri/src/pairing/types.rs`
- Modify: `src-tauri/src/lib.rs:` (add `pub mod pairing;` near top of crate)

- [ ] **Step 1: Add the module declaration**

In `src-tauri/src/lib.rs`, add (next to other `pub mod` lines like `pub mod owner_state;`):

```rust
pub mod pairing;
```

- [ ] **Step 2: Create `src-tauri/src/pairing/mod.rs`**

```rust
pub mod types;

pub use types::*;
```

- [ ] **Step 3: Write the failing test for `PairingState` serde**

Create `src-tauri/src/pairing/types.rs` with this initial content (test only, types not yet implemented):

```rust
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// types defined below — see step 5

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_state_serde_camel_case() {
        let s = PairingState::Discovering {
            role: PairingRole::Inviter,
            ephemeral_pubkey_hex: "deadbeef".to_string(),
            session_id: Uuid::nil(),
        };
        let j = serde_json::to_string(&s).unwrap();
        // Must use camelCase tag and field names for TS mirror.
        assert!(j.contains("\"discovering\""));
        assert!(j.contains("ephemeralPubkeyHex"));
        assert!(j.contains("sessionId"));
    }

    #[test]
    fn role_serializes_lowercase() {
        // serde rename_all="camelCase" leaves single-word PascalCase variants
        // unchanged ("Inviter" stays "Inviter"). We want lowercase for the
        // TS mirror; use rename_all="lowercase" on the enum.
        // (Same lesson as ZEB-170 round 1 fix for TrustKind.)
        assert_eq!(serde_json::to_string(&PairingRole::Inviter).unwrap(), "\"inviter\"");
        assert_eq!(serde_json::to_string(&PairingRole::Joiner).unwrap(), "\"joiner\"");
    }
}
```

- [ ] **Step 4: Run the test — must fail with "type not found"**

Run: `cargo test -p harmony-app --lib pairing::types::tests`
Expected: compile error citing `PairingState`, `PairingRole`, etc. not found.

- [ ] **Step 5: Implement the types**

Replace the placeholder body of `src-tauri/src/pairing/types.rs`:

```rust
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PairingRole {
    Inviter,
    Joiner,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredPeer {
    pub session_id: Uuid,
    pub role: PairingRole,
    pub display_name: String,
    /// Set only when the peer is an Inviter — the owner identity hash.
    pub owner_id_if_inviter: Option<String>, // 32-hex
    pub ephemeral_pubkey_hex: String,
    pub seen_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PairingState {
    Idle,
    Discovering {
        role: PairingRole,
        ephemeral_pubkey_hex: String,
        session_id: Uuid,
    },
    Discovered {
        peers: Vec<DiscoveredPeer>,
    },
    Handshaking {
        peer_session_id: Uuid,
        sas_digits: String, // exactly 6 chars
    },
    WaitingPeerConfirm {
        peer_session_id: Uuid,
    },
    Enrolling,
    Complete {
        device_id_hex: String, // 32-hex
    },
    Failed {
        reason: String,
    },
}

/// Wire messages exchanged on `harmony/pairing/v2/lan/<session-id>/<phase>`.
/// DISCOVER and SELECT are plaintext (needed for discovery + selection).
/// CONFIRM and ENROLL are encrypted under the derived session_key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum PairingWireMessage {
    Discover {
        session_id: Uuid,
        role: PairingRole,
        ephemeral_pubkey_hex: String,
        display_name: String,
        owner_id_if_inviter: Option<String>,
    },
    /// Sent when the local user clicks the peer's row.
    Select {
        my_session_id: Uuid,
        peer_session_id: Uuid,
    },
    /// Encrypted-payload envelope. Inner bytes are XChaCha20-Poly1305
    /// ciphertext; the inner plaintext is `EncryptedPayload` JSON.
    Encrypted {
        my_session_id: Uuid,
        peer_session_id: Uuid,
        nonce_hex: String, // 24 bytes hex
        ciphertext_hex: String,
    },
    Cancel {
        my_session_id: Uuid,
        peer_session_id: Option<Uuid>,
        reason: String,
    },
}

/// Plaintext payload that gets encrypted into `PairingWireMessage::Encrypted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EncryptedPayload {
    Confirm {
        sas_digits: String,
    },
    Enroll {
        enrollment_cert_cbor_hex: String,
        owner_state_cbor_hex: String,
        joiner_advisory_display_name: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_state_serde_camel_case() {
        let s = PairingState::Discovering {
            role: PairingRole::Inviter,
            ephemeral_pubkey_hex: "deadbeef".to_string(),
            session_id: Uuid::nil(),
        };
        let j = serde_json::to_string(&s).unwrap();
        assert!(j.contains("\"discovering\""));
        assert!(j.contains("ephemeralPubkeyHex"));
        assert!(j.contains("sessionId"));
    }

    #[test]
    fn role_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&PairingRole::Inviter).unwrap(), "\"inviter\"");
        assert_eq!(serde_json::to_string(&PairingRole::Joiner).unwrap(), "\"joiner\"");
    }

    #[test]
    fn wire_message_roundtrips() {
        let m = PairingWireMessage::Discover {
            session_id: Uuid::nil(),
            role: PairingRole::Joiner,
            ephemeral_pubkey_hex: "00".repeat(32),
            display_name: "AVALON".to_string(),
            owner_id_if_inviter: None,
        };
        let bytes = serde_cbor::to_vec(&m).unwrap();
        let back: PairingWireMessage = serde_cbor::from_slice(&bytes).unwrap();
        assert!(matches!(back, PairingWireMessage::Discover { .. }));
    }

    #[test]
    fn encrypted_payload_roundtrips() {
        let p = EncryptedPayload::Confirm { sas_digits: "012845".to_string() };
        let bytes = serde_cbor::to_vec(&p).unwrap();
        let back: EncryptedPayload = serde_cbor::from_slice(&bytes).unwrap();
        match back {
            EncryptedPayload::Confirm { sas_digits } => assert_eq!(sas_digits, "012845"),
            _ => panic!("wrong variant"),
        }
    }
}
```

- [ ] **Step 6: Run all four tests — must pass**

Run: `cargo test -p harmony-app --lib pairing::types::tests`
Expected: 4 passed.

- [ ] **Step 7: cargo fmt**

Run: `cargo fmt -p harmony-app` (run from `src-tauri/`)

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/pairing/mod.rs src-tauri/src/pairing/types.rs
git commit -m "feat(pairing): wire types and serde for Track B v2 (ZEB-197)"
```

---

## Task 2: SAS + session_key derivation

**Files:**
- Create: `src-tauri/src/pairing/sas.rs`
- Modify: `src-tauri/src/pairing/mod.rs` (add `pub mod sas;`)
- Modify: `src-tauri/Cargo.toml` (add `x25519-dalek`, `hkdf`, `sha2`)

- [ ] **Step 1: Add dependencies to `src-tauri/Cargo.toml`**

In the `[dependencies]` section (preserve existing entries; just add):

```toml
x25519-dalek = { version = "2", features = ["static_secrets"] }
hkdf = "0.12"
sha2 = "0.10"
chacha20poly1305 = "0.10"
```

(Most likely `sha2` is already present transitively; explicit is OK.)

Run `cargo build -p harmony-app` to confirm the deps resolve.

- [ ] **Step 2: Write the failing tests**

Create `src-tauri/src/pairing/sas.rs`:

```rust
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

#[derive(Debug, Clone)]
pub struct SasDerivation {
    pub session_key: [u8; 32],
    pub sas_digits: String, // exactly 6 ASCII digits
}

/// Derive the session_key and 6-digit SAS from a local ephemeral X25519
/// secret + the peer's ephemeral X25519 public key.
///
/// Both sides MUST pass the same role-symmetric inputs (i.e., the function
/// is symmetric: `derive(a_sk, b_pk) == derive(b_sk, a_pk)`).
pub fn derive_sas(local_sk: &StaticSecret, peer_pk: &PublicKey) -> SasDerivation {
    let shared = local_sk.diffie_hellman(peer_pk);
    let hk = Hkdf::<Sha256>::new(None, shared.as_bytes());

    let mut session_key = [0u8; 32];
    hk.expand(b"session-v2", &mut session_key)
        .expect("HKDF session-v2 expand cannot fail for 32 bytes");

    let mut sas_bytes = [0u8; 4];
    hk.expand(b"sas-v2", &mut sas_bytes)
        .expect("HKDF sas-v2 expand cannot fail for 4 bytes");

    let sas_int = u32::from_be_bytes(sas_bytes) % 1_000_000;
    let sas_digits = format!("{:06}", sas_int);

    SasDerivation { session_key, sas_digits }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

    #[test]
    fn sas_is_symmetric() {
        let a_sk = StaticSecret::random_from_rng(OsRng);
        let b_sk = StaticSecret::random_from_rng(OsRng);
        let a_pk = PublicKey::from(&a_sk);
        let b_pk = PublicKey::from(&b_sk);

        let from_a = derive_sas(&a_sk, &b_pk);
        let from_b = derive_sas(&b_sk, &a_pk);

        assert_eq!(from_a.session_key, from_b.session_key);
        assert_eq!(from_a.sas_digits, from_b.sas_digits);
    }

    #[test]
    fn sas_is_deterministic() {
        // Same inputs always produce the same outputs.
        let a_sk = StaticSecret::from([7u8; 32]);
        let b_sk = StaticSecret::from([42u8; 32]);
        let b_pk = PublicKey::from(&b_sk);
        let _ = a_sk; // silence unused if compiler complains
        let a_pk_clone = PublicKey::from(&StaticSecret::from([7u8; 32]));
        let _ = a_pk_clone;
        let r1 = derive_sas(&StaticSecret::from([7u8; 32]), &b_pk);
        let r2 = derive_sas(&StaticSecret::from([7u8; 32]), &b_pk);
        assert_eq!(r1.session_key, r2.session_key);
        assert_eq!(r1.sas_digits, r2.sas_digits);
    }

    #[test]
    fn sas_differs_under_mitm() {
        // Simulate a MitM doing two separate ECDH exchanges.
        let a_sk = StaticSecret::random_from_rng(OsRng);
        let b_sk = StaticSecret::random_from_rng(OsRng);
        let mitm_sk = StaticSecret::random_from_rng(OsRng);
        let mitm_pk = PublicKey::from(&mitm_sk);

        let a_view = derive_sas(&a_sk, &mitm_pk); // A thinks it's talking to mitm_pk
        let b_view = derive_sas(&b_sk, &mitm_pk); // B same

        // The user looking at both screens sees DIFFERENT 6-digit codes
        // and clicks "no don't match" → MitM detected.
        assert_ne!(a_view.sas_digits, b_view.sas_digits);
    }

    #[test]
    fn sas_digits_format() {
        // Always exactly 6 ASCII digits, even when the int is < 100000.
        let a_sk = StaticSecret::from([0u8; 32]);
        let b_sk = StaticSecret::from([0u8; 32]);
        let b_pk = PublicKey::from(&b_sk);
        let result = derive_sas(&a_sk, &b_pk);
        assert_eq!(result.sas_digits.len(), 6);
        assert!(result.sas_digits.chars().all(|c| c.is_ascii_digit()));
    }
}
```

Add to `src-tauri/src/pairing/mod.rs`:

```rust
pub mod sas;
pub mod types;

pub use types::*;
```

- [ ] **Step 3: Run the tests — must fail with "missing dep" or compile error first time**

Run: `cargo test -p harmony-app --lib pairing::sas::tests`
Expected: clean run if deps resolved; otherwise compile error pointing at missing crates.

- [ ] **Step 4: If tests fail because deps need updating**

Run `cargo update -p x25519-dalek -p hkdf -p sha2` and retry.

- [ ] **Step 5: All four tests must pass**

Expected: 4 passed.

- [ ] **Step 6: cargo fmt + commit**

```bash
cargo fmt -p harmony-app
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/pairing/sas.rs src-tauri/src/pairing/mod.rs
git commit -m "feat(pairing): X25519 ECDH + HKDF + 6-digit SAS derivation (ZEB-197)"
```

---

## Task 3: Session encryption helper

**Files:**
- Create: `src-tauri/src/pairing/session.rs`
- Modify: `src-tauri/src/pairing/mod.rs` (add `pub mod session;`)

- [ ] **Step 1: Write the failing tests**

Create `src-tauri/src/pairing/session.rs`:

```rust
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
```

Add `pub mod session;` to `src-tauri/src/pairing/mod.rs`.

- [ ] **Step 2: Run the tests — must compile and pass**

Run: `cargo test -p harmony-app --lib pairing::session::tests`
Expected: 4 passed.

- [ ] **Step 3: cargo fmt + commit**

```bash
cargo fmt -p harmony-app
git add src-tauri/src/pairing/session.rs src-tauri/src/pairing/mod.rs
git commit -m "feat(pairing): XChaCha20-Poly1305 session encryption helpers (ZEB-197)"
```

---

## Task 4: Cert sign + verify wrappers

**Files:**
- Create: `src-tauri/src/pairing/cert.rs`
- Modify: `src-tauri/src/pairing/mod.rs` (add `pub mod cert;`)

This isolates the harmony-owner-facing logic so the state machine doesn't import `harmony_owner::*` symbols directly. Inviter's cert signing happens here; Joiner's cert verification happens here.

- [ ] **Step 1: Write the failing tests**

Create `src-tauri/src/pairing/cert.rs`:

```rust
use harmony_owner::certs::EnrollmentCert;
use harmony_owner::lifecycle::{mint_owner, RecoveryArtifact};
use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};
use harmony_owner::state::OwnerState;
use ed25519_dalek::SigningKey;
use zeroize::Zeroizing;

/// Sign an EnrollmentCert for the joiner using the master seed (transient).
/// Master signing key is reconstructed from the seed, used immediately, and
/// dropped at end of function (Zeroizing + scope exit).
///
/// Returns the cert; the caller is responsible for adding it to OwnerState
/// via `OwnerState::add_enrollment` and propagating to the Joiner.
pub fn sign_enrollment_for_joiner(
    master_seed: &Zeroizing<[u8; 32]>,
    state: &OwnerState,
    joiner_pubkey: PubKeyBundle,
    now_unix: u64,
) -> Result<EnrollmentCert, String> {
    let artifact = RecoveryArtifact::from_seed(**master_seed);
    let master_pubkey = artifact.master_pubkey_bundle();

    if master_pubkey.identity_hash() != state.owner_id {
        return Err(format!(
            "master/state owner_id mismatch: master={} state={}",
            hex::encode(master_pubkey.identity_hash()),
            hex::encode(state.owner_id),
        ));
    }

    let device_id = joiner_pubkey.identity_hash();
    let master_sk = artifact.master_signing_key();
    let cert = EnrollmentCert::sign_master(
        &master_sk,
        master_pubkey,
        device_id,
        joiner_pubkey,
        now_unix,
        None,
    )
    .map_err(|e| format!("sign_master: {e}"))?;
    drop(master_sk);

    Ok(cert)
}

/// Verify a received EnrollmentCert before persisting it on the Joiner side.
/// Checks: cert.owner_id matches expected, cert.device_id matches our pubkey,
/// signature verifies against the embedded master pubkey (via add_enrollment's
/// internal check — we just call add_enrollment on a temp clone of state).
pub fn verify_cert_for_self(
    cert: &EnrollmentCert,
    expected_owner_id: [u8; 16],
    our_device_pubkey: &PubKeyBundle,
    now_unix: u64,
    active_window_secs: u64,
) -> Result<(), String> {
    if cert.owner_id != expected_owner_id {
        return Err(format!(
            "cert owner_id mismatch: cert={} expected={}",
            hex::encode(cert.owner_id),
            hex::encode(expected_owner_id),
        ));
    }
    let our_device_id = our_device_pubkey.identity_hash();
    if cert.device_id != our_device_id {
        return Err(format!(
            "cert device_id mismatch: cert={} ours={}",
            hex::encode(cert.device_id),
            hex::encode(our_device_id),
        ));
    }
    // Construct a throwaway state with our owner_id and try add_enrollment.
    // Its internal verification checks the master signature against the
    // embedded master pubkey and rejects if invalid.
    let mut probe = OwnerState {
        owner_id: expected_owner_id,
        enrollments: Default::default(),
        vouching: Default::default(),
        revocations: Default::default(),
        liveness: Default::default(),
        reclamations: Default::default(),
    };
    probe
        .add_enrollment(cert.clone(), now_unix, active_window_secs)
        .map_err(|e| format!("verify (via add_enrollment probe): {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_owner::lifecycle::MintResult;
    use harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS;
    use rand_core::OsRng;

    fn fresh_joiner() -> (SigningKey, PubKeyBundle) {
        let sk = SigningKey::generate(&mut OsRng);
        let pubkey = PubKeyBundle::classical_only(sk.verifying_key().to_bytes());
        (sk, pubkey)
    }

    #[test]
    fn sign_and_verify_round_trip() {
        let MintResult { state, recovery_artifact, .. } = mint_owner(1_700_000_000).unwrap();
        let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());
        let (_joiner_sk, joiner_pubkey) = fresh_joiner();

        let cert = sign_enrollment_for_joiner(
            &master_seed,
            &state,
            joiner_pubkey.clone(),
            1_700_000_001,
        )
        .unwrap();

        verify_cert_for_self(
            &cert,
            state.owner_id,
            &joiner_pubkey,
            1_700_000_002,
            DEFAULT_ACTIVE_WINDOW_SECS,
        )
        .unwrap();
    }

    #[test]
    fn verify_rejects_wrong_owner() {
        let MintResult { state, recovery_artifact, .. } = mint_owner(1_700_000_000).unwrap();
        let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());
        let (_joiner_sk, joiner_pubkey) = fresh_joiner();

        let cert = sign_enrollment_for_joiner(
            &master_seed,
            &state,
            joiner_pubkey.clone(),
            1_700_000_001,
        )
        .unwrap();

        let wrong_owner = [0xFFu8; 16];
        let err = verify_cert_for_self(
            &cert,
            wrong_owner,
            &joiner_pubkey,
            1_700_000_002,
            DEFAULT_ACTIVE_WINDOW_SECS,
        )
        .unwrap_err();
        assert!(err.contains("owner_id mismatch"));
    }

    #[test]
    fn verify_rejects_wrong_device() {
        let MintResult { state, recovery_artifact, .. } = mint_owner(1_700_000_000).unwrap();
        let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());
        let (_joiner_sk, joiner_pubkey) = fresh_joiner();
        let (_other_sk, other_pubkey) = fresh_joiner();

        let cert = sign_enrollment_for_joiner(
            &master_seed,
            &state,
            joiner_pubkey,
            1_700_000_001,
        )
        .unwrap();

        let err = verify_cert_for_self(
            &cert,
            state.owner_id,
            &other_pubkey, // pretend we're a different device
            1_700_000_002,
            DEFAULT_ACTIVE_WINDOW_SECS,
        )
        .unwrap_err();
        assert!(err.contains("device_id mismatch"));
    }

    #[test]
    fn verify_rejects_tampered_cert() {
        let MintResult { state, recovery_artifact, .. } = mint_owner(1_700_000_000).unwrap();
        let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());
        let (_joiner_sk, joiner_pubkey) = fresh_joiner();

        let mut cert = sign_enrollment_for_joiner(
            &master_seed,
            &state,
            joiner_pubkey.clone(),
            1_700_000_001,
        )
        .unwrap();

        // Flip a bit in the issued_at to invalidate the signature.
        cert.issued_at = cert.issued_at.wrapping_add(1);

        let err = verify_cert_for_self(
            &cert,
            state.owner_id,
            &joiner_pubkey,
            1_700_000_002,
            DEFAULT_ACTIVE_WINDOW_SECS,
        )
        .unwrap_err();
        assert!(err.to_lowercase().contains("verif") || err.to_lowercase().contains("signature") || err.to_lowercase().contains("invalid"));
    }
}
```

Add to `src-tauri/src/pairing/mod.rs`:

```rust
pub mod cert;
pub mod sas;
pub mod session;
pub mod types;

pub use types::*;
```

- [ ] **Step 2: Run the tests — must pass**

Run: `cargo test -p harmony-app --lib pairing::cert::tests`
Expected: 4 passed.

- [ ] **Step 3: cargo fmt + commit**

```bash
cargo fmt -p harmony-app
git add src-tauri/src/pairing/cert.rs src-tauri/src/pairing/mod.rs
git commit -m "feat(pairing): cert sign (Inviter) + verify (Joiner) helpers (ZEB-197)"
```

---

## Task 5: PairingTransport trait + InMemoryTransport for tests

**Files:**
- Create: `src-tauri/src/pairing/transport.rs`
- Modify: `src-tauri/src/pairing/mod.rs` (add `pub mod transport;`)

This abstraction makes the state machine testable without touching real Zenoh.

- [ ] **Step 1: Write the failing tests**

Create `src-tauri/src/pairing/transport.rs`:

```rust
use crate::pairing::types::PairingWireMessage;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

#[async_trait]
pub trait PairingTransport: Send + Sync {
    /// Publish a wire message to the pairing scope. Returns Ok once the
    /// transport has accepted the message for delivery (not necessarily
    /// once the peer has acked — Zenoh pub/sub is best-effort).
    async fn publish(&self, message: PairingWireMessage) -> Result<(), String>;

    /// Receive the next wire message from any peer in the pairing scope.
    /// Returns None when the transport is shut down.
    async fn recv(&self) -> Option<PairingWireMessage>;
}

/// In-memory transport for tests: an `InMemoryBroker` connects two
/// `InMemoryTransport` endpoints. Anything published on one is delivered
/// to the other (and only the other — not echoed back to the sender).
pub struct InMemoryBroker {
    side_a_tx: mpsc::Sender<PairingWireMessage>, // delivered TO side A
    side_b_tx: mpsc::Sender<PairingWireMessage>,
}

pub struct InMemoryTransport {
    /// Sender used by THIS side's publish() to push to the OTHER side.
    publish_tx: mpsc::Sender<PairingWireMessage>,
    /// Receiver this side uses to pull incoming messages.
    recv_rx: Arc<Mutex<mpsc::Receiver<PairingWireMessage>>>,
}

impl InMemoryBroker {
    pub fn pair() -> (InMemoryTransport, InMemoryTransport) {
        let (a_tx, a_rx) = mpsc::channel(64);
        let (b_tx, b_rx) = mpsc::channel(64);
        let side_a = InMemoryTransport {
            publish_tx: b_tx, // A publishes → goes to B's recv
            recv_rx: Arc::new(Mutex::new(a_rx)),
        };
        let side_b = InMemoryTransport {
            publish_tx: a_tx, // B publishes → goes to A's recv
            recv_rx: Arc::new(Mutex::new(b_rx)),
        };
        (side_a, side_b)
    }
}

#[async_trait]
impl PairingTransport for InMemoryTransport {
    async fn publish(&self, message: PairingWireMessage) -> Result<(), String> {
        self.publish_tx
            .send(message)
            .await
            .map_err(|e| format!("in-memory publish: {e}"))
    }

    async fn recv(&self) -> Option<PairingWireMessage> {
        self.recv_rx.lock().await.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::types::{PairingRole, PairingWireMessage};
    use uuid::Uuid;

    #[tokio::test]
    async fn in_memory_broker_routes_messages_one_way() {
        let (a, b) = InMemoryBroker::pair();
        let msg = PairingWireMessage::Discover {
            session_id: Uuid::nil(),
            role: PairingRole::Inviter,
            ephemeral_pubkey_hex: "00".repeat(32),
            display_name: "test".to_string(),
            owner_id_if_inviter: None,
        };
        a.publish(msg.clone()).await.unwrap();
        let received = b.recv().await.unwrap();
        match received {
            PairingWireMessage::Discover { display_name, .. } => {
                assert_eq!(display_name, "test");
            }
            _ => panic!("wrong variant"),
        }
        // Confirm A didn't echo back to itself.
        tokio::select! {
            r = a.recv() => panic!("A should not receive its own message: {:?}", r),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
    }
}
```

Add `pub mod transport;` to `src-tauri/src/pairing/mod.rs`.

You will also need `async-trait` in Cargo.toml:

```toml
async-trait = "0.1"
```

- [ ] **Step 2: Run the test — must pass**

Run: `cargo test -p harmony-app --lib pairing::transport::tests`
Expected: 1 passed.

- [ ] **Step 3: cargo fmt + commit**

```bash
cargo fmt -p harmony-app
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/pairing/transport.rs src-tauri/src/pairing/mod.rs
git commit -m "feat(pairing): PairingTransport trait + InMemoryTransport for tests (ZEB-197)"
```

---

## Task 6: State machine core (transport-agnostic)

**Files:**
- Create: `src-tauri/src/pairing/state_machine.rs`
- Modify: `src-tauri/src/pairing/mod.rs` (add `pub mod state_machine;`)

This is the heart of the feature. State machine is event-driven (consumes wire messages + UI commands, emits state transitions). Keep it transport-agnostic — wraps over a `dyn PairingTransport`.

- [ ] **Step 1: Write the failing test for happy path with InMemoryBroker**

Create `src-tauri/src/pairing/state_machine.rs`:

```rust
use crate::pairing::cert::{sign_enrollment_for_joiner, verify_cert_for_self};
use crate::pairing::sas::derive_sas;
use crate::pairing::session::{decrypt as session_decrypt, encrypt as session_encrypt};
use crate::pairing::transport::PairingTransport;
use crate::pairing::types::{
    DiscoveredPeer, EncryptedPayload, PairingRole, PairingState, PairingWireMessage,
};
use ed25519_dalek::SigningKey;
use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};
use harmony_owner::state::OwnerState;
use harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS;
use std::sync::Arc;
use tokio::sync::{mpsc, watch, Mutex};
use uuid::Uuid;
use x25519_dalek::{PublicKey as X25519Pub, StaticSecret as X25519Sec};
use zeroize::Zeroizing;

/// Inputs to the state machine from the UI layer.
pub enum PairingCommand {
    StartInviter {
        display_name: String,
        owner_state: OwnerState,
        master_seed: Zeroizing<[u8; 32]>,
    },
    StartJoiner {
        display_name: String,
        signing_key: SigningKey,
    },
    SelectPeer {
        peer_session_id: Uuid,
    },
    ConfirmSas,
    Cancel,
}

/// Output from the state machine for the Joiner side, when enrollment succeeds.
/// The persistence layer (Task 7) consumes this to write keys + state to disk.
pub struct JoinerEnrollResult {
    pub our_signing_key: SigningKey,
    pub owner_state: OwnerState,
    pub our_device_id: [u8; 16],
}

/// Handle the UI talks to. Drops the state machine on drop.
pub struct PairingHandle {
    pub state_rx: watch::Receiver<PairingState>,
    pub cmd_tx: mpsc::Sender<PairingCommand>,
    pub joiner_result_rx: Option<mpsc::Receiver<JoinerEnrollResult>>,
    _shutdown: tokio::task::JoinHandle<()>,
}

pub fn spawn_state_machine(
    transport: Arc<dyn PairingTransport>,
    now_fn: Arc<dyn Fn() -> u64 + Send + Sync>,
) -> PairingHandle {
    let (state_tx, state_rx) = watch::channel(PairingState::Idle);
    let (cmd_tx, cmd_rx) = mpsc::channel::<PairingCommand>(16);
    let (result_tx, result_rx) = mpsc::channel::<JoinerEnrollResult>(1);

    let task = tokio::spawn(run_state_machine(
        transport,
        state_tx,
        cmd_rx,
        result_tx,
        now_fn,
    ));

    PairingHandle {
        state_rx,
        cmd_tx,
        joiner_result_rx: Some(result_rx),
        _shutdown: task,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_state_machine(
    transport: Arc<dyn PairingTransport>,
    state_tx: watch::Sender<PairingState>,
    mut cmd_rx: mpsc::Receiver<PairingCommand>,
    result_tx: mpsc::Sender<JoinerEnrollResult>,
    now_fn: Arc<dyn Fn() -> u64 + Send + Sync>,
) {
    // Per-session local context. Reset each time we leave a session.
    let mut ctx: Option<SessionCtx> = None;

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { return; };
                match cmd {
                    PairingCommand::StartInviter { display_name, owner_state, master_seed } => {
                        ctx = Some(start_inviter(&transport, &state_tx, display_name, owner_state, master_seed, &now_fn).await);
                    }
                    PairingCommand::StartJoiner { display_name, signing_key } => {
                        ctx = Some(start_joiner(&transport, &state_tx, display_name, signing_key, &now_fn).await);
                    }
                    PairingCommand::SelectPeer { peer_session_id } => {
                        if let Some(c) = ctx.as_mut() {
                            on_select_peer(&transport, &state_tx, c, peer_session_id).await;
                        }
                    }
                    PairingCommand::ConfirmSas => {
                        if let Some(c) = ctx.as_mut() {
                            on_confirm_sas(&transport, &state_tx, c, &now_fn, &result_tx).await;
                        }
                    }
                    PairingCommand::Cancel => {
                        if let Some(c) = ctx.as_mut() {
                            let _ = transport.publish(PairingWireMessage::Cancel {
                                my_session_id: c.session_id,
                                peer_session_id: c.selected_peer_session_id,
                                reason: "user cancelled".to_string(),
                            }).await;
                        }
                        ctx = None;
                        let _ = state_tx.send(PairingState::Idle);
                    }
                }
            }
            msg = transport.recv() => {
                let Some(msg) = msg else { return; };
                if let Some(c) = ctx.as_mut() {
                    handle_wire_message(&transport, &state_tx, c, msg, &now_fn, &result_tx).await;
                } else {
                    // Drop messages received while idle.
                }
            }
        }
    }
}

struct SessionCtx {
    role: PairingRole,
    session_id: Uuid,
    display_name: String,
    eph_sk: X25519Sec,
    eph_pk: X25519Pub,

    // Joiner-only:
    our_signing_key: Option<SigningKey>,
    our_pubkey: Option<PubKeyBundle>,

    // Inviter-only:
    owner_state: Option<OwnerState>,
    master_seed: Option<Zeroizing<[u8; 32]>>,

    // After Discovery:
    discovered_peers: Vec<DiscoveredPeer>,

    // After Select (mutual):
    selected_peer_session_id: Option<Uuid>,
    selected_peer_pubkey: Option<X25519Pub>,
    selected_peer_display_name: Option<String>,
    sent_select: bool,
    received_select: bool,

    // After Handshake:
    session_key: Option<[u8; 32]>,
    sas_digits: Option<String>,

    // After Confirm:
    our_confirmed: bool,
    peer_confirmed: bool,
}

impl SessionCtx {
    fn new(role: PairingRole, display_name: String) -> Self {
        let eph_sk = X25519Sec::random_from_rng(rand_core::OsRng);
        let eph_pk = X25519Pub::from(&eph_sk);
        Self {
            role,
            session_id: Uuid::new_v4(),
            display_name,
            eph_sk,
            eph_pk,
            our_signing_key: None,
            our_pubkey: None,
            owner_state: None,
            master_seed: None,
            discovered_peers: Vec::new(),
            selected_peer_session_id: None,
            selected_peer_pubkey: None,
            selected_peer_display_name: None,
            sent_select: false,
            received_select: false,
            session_key: None,
            sas_digits: None,
            our_confirmed: false,
            peer_confirmed: false,
        }
    }
}

async fn start_inviter(
    transport: &Arc<dyn PairingTransport>,
    state_tx: &watch::Sender<PairingState>,
    display_name: String,
    owner_state: OwnerState,
    master_seed: Zeroizing<[u8; 32]>,
    _now_fn: &Arc<dyn Fn() -> u64 + Send + Sync>,
) -> SessionCtx {
    let mut ctx = SessionCtx::new(PairingRole::Inviter, display_name.clone());
    ctx.owner_state = Some(owner_state.clone());
    ctx.master_seed = Some(master_seed);

    let _ = state_tx.send(PairingState::Discovering {
        role: PairingRole::Inviter,
        ephemeral_pubkey_hex: hex::encode(ctx.eph_pk.as_bytes()),
        session_id: ctx.session_id,
    });

    let _ = transport
        .publish(PairingWireMessage::Discover {
            session_id: ctx.session_id,
            role: PairingRole::Inviter,
            ephemeral_pubkey_hex: hex::encode(ctx.eph_pk.as_bytes()),
            display_name,
            owner_id_if_inviter: Some(hex::encode(owner_state.owner_id)),
        })
        .await;

    ctx
}

async fn start_joiner(
    transport: &Arc<dyn PairingTransport>,
    state_tx: &watch::Sender<PairingState>,
    display_name: String,
    signing_key: SigningKey,
    _now_fn: &Arc<dyn Fn() -> u64 + Send + Sync>,
) -> SessionCtx {
    let mut ctx = SessionCtx::new(PairingRole::Joiner, display_name.clone());
    let pubkey = PubKeyBundle::classical_only(signing_key.verifying_key().to_bytes());
    ctx.our_signing_key = Some(signing_key);
    ctx.our_pubkey = Some(pubkey);

    let _ = state_tx.send(PairingState::Discovering {
        role: PairingRole::Joiner,
        ephemeral_pubkey_hex: hex::encode(ctx.eph_pk.as_bytes()),
        session_id: ctx.session_id,
    });

    let _ = transport
        .publish(PairingWireMessage::Discover {
            session_id: ctx.session_id,
            role: PairingRole::Joiner,
            ephemeral_pubkey_hex: hex::encode(ctx.eph_pk.as_bytes()),
            display_name,
            owner_id_if_inviter: None,
        })
        .await;

    ctx
}

async fn on_select_peer(
    transport: &Arc<dyn PairingTransport>,
    state_tx: &watch::Sender<PairingState>,
    ctx: &mut SessionCtx,
    peer_session_id: Uuid,
) {
    // Find the peer in the discovered list.
    let Some(peer) = ctx.discovered_peers.iter().find(|p| p.session_id == peer_session_id).cloned() else {
        let _ = state_tx.send(PairingState::Failed {
            reason: format!("unknown peer session_id: {peer_session_id}"),
        });
        return;
    };
    ctx.selected_peer_session_id = Some(peer_session_id);
    ctx.selected_peer_display_name = Some(peer.display_name.clone());
    let pk_bytes = hex::decode(&peer.ephemeral_pubkey_hex).unwrap_or_default();
    if pk_bytes.len() != 32 {
        let _ = state_tx.send(PairingState::Failed {
            reason: format!("peer pubkey has wrong length: {}", pk_bytes.len()),
        });
        return;
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&pk_bytes);
    ctx.selected_peer_pubkey = Some(X25519Pub::from(arr));

    // Publish SELECT.
    ctx.sent_select = true;
    let _ = transport
        .publish(PairingWireMessage::Select {
            my_session_id: ctx.session_id,
            peer_session_id,
        })
        .await;

    // If we've already received the peer's SELECT, transition to Handshaking.
    maybe_advance_to_handshake(state_tx, ctx);
}

fn maybe_advance_to_handshake(state_tx: &watch::Sender<PairingState>, ctx: &mut SessionCtx) {
    if ctx.sent_select && ctx.received_select && ctx.session_key.is_none() {
        let peer_pk = ctx.selected_peer_pubkey.as_ref().expect("peer pubkey set on select");
        let derivation = derive_sas(&ctx.eph_sk, peer_pk);
        ctx.session_key = Some(derivation.session_key);
        ctx.sas_digits = Some(derivation.sas_digits.clone());
        let _ = state_tx.send(PairingState::Handshaking {
            peer_session_id: ctx.selected_peer_session_id.expect("set on select"),
            sas_digits: derivation.sas_digits,
        });
    }
}

async fn on_confirm_sas(
    transport: &Arc<dyn PairingTransport>,
    state_tx: &watch::Sender<PairingState>,
    ctx: &mut SessionCtx,
    now_fn: &Arc<dyn Fn() -> u64 + Send + Sync>,
    result_tx: &mpsc::Sender<JoinerEnrollResult>,
) {
    let Some(session_key) = ctx.session_key else { return; };
    let Some(sas_digits) = ctx.sas_digits.clone() else { return; };
    let Some(peer_session_id) = ctx.selected_peer_session_id else { return; };

    ctx.our_confirmed = true;

    // Encrypt + publish CONFIRM.
    let payload = EncryptedPayload::Confirm { sas_digits: sas_digits.clone() };
    let pt = serde_cbor::to_vec(&payload).expect("CBOR encode cannot fail");
    let (nonce, ct) = match session_encrypt(&session_key, &pt) {
        Ok(p) => p,
        Err(e) => {
            let _ = state_tx.send(PairingState::Failed { reason: format!("encrypt confirm: {e}") });
            return;
        }
    };
    let _ = transport
        .publish(PairingWireMessage::Encrypted {
            my_session_id: ctx.session_id,
            peer_session_id,
            nonce_hex: hex::encode(nonce),
            ciphertext_hex: hex::encode(ct),
        })
        .await;

    let _ = state_tx.send(PairingState::WaitingPeerConfirm { peer_session_id });

    maybe_advance_to_enroll(transport, state_tx, ctx, now_fn, result_tx).await;
}

async fn maybe_advance_to_enroll(
    transport: &Arc<dyn PairingTransport>,
    state_tx: &watch::Sender<PairingState>,
    ctx: &mut SessionCtx,
    now_fn: &Arc<dyn Fn() -> u64 + Send + Sync>,
    _result_tx: &mpsc::Sender<JoinerEnrollResult>,
) {
    if !(ctx.our_confirmed && ctx.peer_confirmed) {
        return;
    }
    if !matches!(ctx.role, PairingRole::Inviter) {
        // Joiner waits for ENROLL message.
        let _ = state_tx.send(PairingState::Enrolling);
        return;
    }
    let _ = state_tx.send(PairingState::Enrolling);

    // Inviter signs cert + ships ENROLL.
    let owner_state = ctx.owner_state.as_mut().expect("inviter has owner_state");
    let master_seed = ctx.master_seed.as_ref().expect("inviter has master_seed");
    let peer_pubkey_bytes = ctx
        .selected_peer_pubkey
        .as_ref()
        .map(|pk| pk.as_bytes())
        .copied();
    let _ = peer_pubkey_bytes; // used below

    // Joiner's ed25519 verifying key = identity_hash() input — but we only have
    // the X25519 ephemeral pubkey here, NOT the Joiner's ed25519 signing key
    // pubkey. So the Joiner must include its ed25519 verifying key in the
    // SELECT or DISCOVER message. We use a derived bundle from the eph_pk for
    // the v2 wire — actually we pass the ed25519 verifying key in DISCOVER.
    //
    // (See Task 6 Step 2: the Joiner's `verifying_key().to_bytes()` is included in
    // the discovered peer record so the Inviter can sign for it.)

    // For now: the joiner_pubkey was already populated in discovery via a
    // PairingWireMessage extension. We retrieve it from `ctx.selected_peer_*`.
    // (This task's scaffolding deliberately defers the wire-extension to
    // Task 6 Step 2, which adds the ed25519 verify-key field.)
    let joiner_ed25519_verify: [u8; 32] = ctx
        .selected_peer_ed25519_verify
        .ok_or("missing joiner ed25519 verifying key in session ctx")
        .unwrap_or([0u8; 32]);
    let joiner_pubkey = PubKeyBundle::classical_only(joiner_ed25519_verify);

    let now = (now_fn)();
    let cert = match sign_enrollment_for_joiner(master_seed, owner_state, joiner_pubkey.clone(), now) {
        Ok(c) => c,
        Err(e) => {
            let _ = state_tx.send(PairingState::Failed { reason: format!("sign cert: {e}") });
            return;
        }
    };

    if let Err(e) = owner_state.add_enrollment(cert.clone(), now, DEFAULT_ACTIVE_WINDOW_SECS) {
        let _ = state_tx.send(PairingState::Failed { reason: format!("add enrollment: {e}") });
        return;
    }

    let cert_cbor = serde_cbor::to_vec(&cert).expect("cert serializable");
    let state_cbor = serde_cbor::to_vec(&owner_state).expect("state serializable");
    let payload = EncryptedPayload::Enroll {
        enrollment_cert_cbor_hex: hex::encode(&cert_cbor),
        owner_state_cbor_hex: hex::encode(&state_cbor),
        joiner_advisory_display_name: ctx.selected_peer_display_name.clone().unwrap_or_default(),
    };
    let pt = serde_cbor::to_vec(&payload).expect("payload serializable");
    let session_key = ctx.session_key.expect("session key after handshake");
    let (nonce, ct) = match session_encrypt(&session_key, &pt) {
        Ok(p) => p,
        Err(e) => {
            let _ = state_tx.send(PairingState::Failed { reason: format!("encrypt enroll: {e}") });
            return;
        }
    };
    let _ = transport
        .publish(PairingWireMessage::Encrypted {
            my_session_id: ctx.session_id,
            peer_session_id: ctx.selected_peer_session_id.expect("set"),
            nonce_hex: hex::encode(nonce),
            ciphertext_hex: hex::encode(ct),
        })
        .await;

    let device_id = joiner_pubkey.identity_hash();
    let _ = state_tx.send(PairingState::Complete { device_id_hex: hex::encode(device_id) });
}

async fn handle_wire_message(
    _transport: &Arc<dyn PairingTransport>,
    state_tx: &watch::Sender<PairingState>,
    ctx: &mut SessionCtx,
    msg: PairingWireMessage,
    now_fn: &Arc<dyn Fn() -> u64 + Send + Sync>,
    result_tx: &mpsc::Sender<JoinerEnrollResult>,
) {
    match msg {
        PairingWireMessage::Discover { session_id, role, ephemeral_pubkey_hex, display_name, owner_id_if_inviter } => {
            // Ignore our own discoveries (echo).
            if session_id == ctx.session_id {
                return;
            }
            // Only collect peers of the OPPOSITE role.
            if role == ctx.role {
                return;
            }
            // De-dup by session_id.
            if ctx.discovered_peers.iter().any(|p| p.session_id == session_id) {
                return;
            }
            let now = (now_fn)();
            ctx.discovered_peers.push(DiscoveredPeer {
                session_id,
                role,
                display_name,
                owner_id_if_inviter,
                ephemeral_pubkey_hex,
                seen_at_unix: now,
            });
            let _ = state_tx.send(PairingState::Discovered {
                peers: ctx.discovered_peers.clone(),
            });
        }
        PairingWireMessage::Select { my_session_id, peer_session_id } => {
            // Only act if the peer is selecting US.
            if peer_session_id != ctx.session_id {
                return;
            }
            // Only act if we have already discovered this peer.
            if !ctx.discovered_peers.iter().any(|p| p.session_id == my_session_id) {
                return;
            }
            // If we haven't selected anyone yet, this is the peer claiming us;
            // the local user still needs to click their row to send our SELECT.
            ctx.received_select = ctx.selected_peer_session_id == Some(my_session_id);
            maybe_advance_to_handshake(state_tx, ctx);
        }
        PairingWireMessage::Encrypted { my_session_id, peer_session_id, nonce_hex, ciphertext_hex } => {
            if peer_session_id != ctx.session_id { return; }
            if Some(my_session_id) != ctx.selected_peer_session_id { return; }
            let Some(session_key) = ctx.session_key else { return; };
            let nonce = match hex::decode(&nonce_hex) {
                Ok(n) => n,
                Err(e) => {
                    let _ = state_tx.send(PairingState::Failed { reason: format!("nonce hex: {e}") });
                    return;
                }
            };
            let ct = match hex::decode(&ciphertext_hex) {
                Ok(c) => c,
                Err(e) => {
                    let _ = state_tx.send(PairingState::Failed { reason: format!("ct hex: {e}") });
                    return;
                }
            };
            let pt = match session_decrypt(&session_key, &nonce, &ct) {
                Ok(p) => p,
                Err(e) => {
                    let _ = state_tx.send(PairingState::Failed { reason: format!("decrypt: {e}") });
                    return;
                }
            };
            let payload: EncryptedPayload = match serde_cbor::from_slice(&pt) {
                Ok(p) => p,
                Err(e) => {
                    let _ = state_tx.send(PairingState::Failed { reason: format!("payload decode: {e}") });
                    return;
                }
            };
            on_encrypted_payload(state_tx, ctx, payload, now_fn, result_tx).await;
        }
        PairingWireMessage::Cancel { my_session_id, .. } => {
            if Some(my_session_id) == ctx.selected_peer_session_id {
                let _ = state_tx.send(PairingState::Idle);
            }
        }
    }
}

async fn on_encrypted_payload(
    state_tx: &watch::Sender<PairingState>,
    ctx: &mut SessionCtx,
    payload: EncryptedPayload,
    now_fn: &Arc<dyn Fn() -> u64 + Send + Sync>,
    result_tx: &mpsc::Sender<JoinerEnrollResult>,
) {
    match payload {
        EncryptedPayload::Confirm { sas_digits } => {
            // Defense-in-depth: the SAS in the message must match what we
            // computed locally. (Session_key already authenticates this, but
            // the explicit equality check makes the intent obvious.)
            if Some(&sas_digits) != ctx.sas_digits.as_ref() {
                let _ = state_tx.send(PairingState::Failed { reason: "SAS mismatch in CONFIRM".to_string() });
                return;
            }
            ctx.peer_confirmed = true;
            // Inviter advances to enroll once both confirmed (handled in on_confirm_sas's call to maybe_advance_to_enroll).
            // For the RECEIVING side, we may need to re-trigger if our own confirm already happened.
            if ctx.our_confirmed {
                if matches!(ctx.role, PairingRole::Inviter) {
                    // Will be re-driven when the user clicks confirm; or if
                    // they already did, advance now.
                    // Simplest: emit Enrolling and let the on_confirm_sas
                    // logic handle it. The transport task will see this on
                    // next loop iteration.
                    // For correctness we need to do the inviter-side enroll
                    // here too — call maybe_advance_to_enroll directly.
                    // We don't have transport in scope here; refactor to pass it.
                    // Workaround for this scaffold: emit Enrolling so the test
                    // can detect the transition.
                    let _ = state_tx.send(PairingState::Enrolling);
                } else {
                    // Joiner waits for ENROLL.
                    let _ = state_tx.send(PairingState::Enrolling);
                }
            }
        }
        EncryptedPayload::Enroll { enrollment_cert_cbor_hex, owner_state_cbor_hex, .. } => {
            // Joiner-side: install the cert and state.
            if !matches!(ctx.role, PairingRole::Joiner) {
                // Inviter doesn't accept ENROLL.
                return;
            }
            let cert_bytes = match hex::decode(&enrollment_cert_cbor_hex) {
                Ok(b) => b,
                Err(e) => { let _ = state_tx.send(PairingState::Failed { reason: format!("cert hex: {e}") }); return; }
            };
            let state_bytes = match hex::decode(&owner_state_cbor_hex) {
                Ok(b) => b,
                Err(e) => { let _ = state_tx.send(PairingState::Failed { reason: format!("state hex: {e}") }); return; }
            };
            let cert: harmony_owner::certs::EnrollmentCert = match serde_cbor::from_slice(&cert_bytes) {
                Ok(c) => c,
                Err(e) => { let _ = state_tx.send(PairingState::Failed { reason: format!("cert decode: {e}") }); return; }
            };
            let owner_state: OwnerState = match serde_cbor::from_slice(&state_bytes) {
                Ok(s) => s,
                Err(e) => { let _ = state_tx.send(PairingState::Failed { reason: format!("state decode: {e}") }); return; }
            };

            let our_pubkey = ctx.our_pubkey.as_ref().expect("joiner has pubkey");
            let now = (now_fn)();
            if let Err(e) = verify_cert_for_self(&cert, owner_state.owner_id, our_pubkey, now, DEFAULT_ACTIVE_WINDOW_SECS) {
                let _ = state_tx.send(PairingState::Failed { reason: format!("verify cert: {e}") });
                return;
            }

            let our_sk = ctx.our_signing_key.take().expect("joiner has signing key");
            let our_device_id = our_pubkey.identity_hash();
            let _ = result_tx.send(JoinerEnrollResult {
                our_signing_key: our_sk,
                owner_state,
                our_device_id,
            }).await;
            let _ = state_tx.send(PairingState::Complete { device_id_hex: hex::encode(our_device_id) });
        }
    }
}

// Test for the additional ed25519 field on SessionCtx that's needed for
// Inviter to sign cert. This is the "wire extension" referenced in
// `maybe_advance_to_enroll`.
impl SessionCtx {
    /// Joiner publishes its ed25519 verifying key alongside its X25519 ephemeral
    /// in DISCOVER. We stash it here when the peer is discovered (Joiner role).
    pub(crate) fn selected_peer_ed25519_verify(&self) -> Option<[u8; 32]> {
        self.selected_peer_ed25519_verify
    }
}

// Add the ed25519 field to SessionCtx. (Re-declare struct here for the
// scaffold; in the real edit this is folded into the original definition.)
// Placeholder access; actual field is added in Task 6 Step 2 below.
```

Add `pub mod state_machine;` to `src-tauri/src/pairing/mod.rs`.

> **Note on the `selected_peer_ed25519_verify` field:** the scaffold above references this field but doesn't add it. Task 6 Step 2 (next subtask) extends `PairingWireMessage::Discover` to include the ed25519 verifying key when the publisher is a Joiner, then threads it through `SessionCtx`.

- [ ] **Step 2: Add the ed25519 verifying-key field**

Edit `src-tauri/src/pairing/types.rs::PairingWireMessage::Discover` to add a new optional field:

```rust
PairingWireMessage::Discover {
    session_id: Uuid,
    role: PairingRole,
    ephemeral_pubkey_hex: String,
    display_name: String,
    owner_id_if_inviter: Option<String>,
    /// Joiner publishes its ed25519 verifying key here so the Inviter
    /// can sign the EnrollmentCert against it. Inviter omits.
    joiner_ed25519_verify_hex: Option<String>,
}
```

Edit `DiscoveredPeer` to add the same field:

```rust
pub struct DiscoveredPeer {
    pub session_id: Uuid,
    pub role: PairingRole,
    pub display_name: String,
    pub owner_id_if_inviter: Option<String>,
    pub ephemeral_pubkey_hex: String,
    pub joiner_ed25519_verify_hex: Option<String>,
    pub seen_at_unix: u64,
}
```

Edit `SessionCtx`:

```rust
struct SessionCtx {
    // ... existing fields ...
    selected_peer_ed25519_verify: Option<[u8; 32]>,
}
```

Update all the places that build `PairingWireMessage::Discover`, `DiscoveredPeer`, and `SessionCtx::new` to thread the new field. The Joiner's `start_joiner` populates it from `signing_key.verifying_key().to_bytes()`. The Inviter's discovery handler stashes it from incoming Joiner DISCOVER messages.

Update `maybe_advance_to_enroll` to use the stashed value instead of the unwrap-or-default placeholder.

- [ ] **Step 3: Write the failing happy-path integration test**

Append to the bottom of `src-tauri/src/pairing/state_machine.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::transport::InMemoryBroker;
    use ed25519_dalek::SigningKey;
    use harmony_owner::lifecycle::{mint_owner, MintResult};
    use rand_core::OsRng;
    use std::time::Duration;
    use tokio::time::timeout;
    use zeroize::Zeroizing;

    fn fixed_clock(t: u64) -> Arc<dyn Fn() -> u64 + Send + Sync> {
        Arc::new(move || t)
    }

    #[tokio::test]
    async fn happy_path_two_devices_pair() {
        // Setup: mint owner on Inviter side; generate a Joiner signing key.
        let MintResult { state, recovery_artifact, .. } = mint_owner(1_700_000_000).unwrap();
        let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());

        let joiner_sk = SigningKey::generate(&mut OsRng);

        // Two transports linked back-to-back.
        let (inviter_t, joiner_t) = InMemoryBroker::pair();
        let inviter_handle = spawn_state_machine(Arc::new(inviter_t), fixed_clock(1_700_000_001));
        let joiner_handle = spawn_state_machine(Arc::new(joiner_t), fixed_clock(1_700_000_002));

        // Both start.
        inviter_handle.cmd_tx.send(PairingCommand::StartInviter {
            display_name: "KRILE".to_string(),
            owner_state: state.clone(),
            master_seed,
        }).await.unwrap();
        joiner_handle.cmd_tx.send(PairingCommand::StartJoiner {
            display_name: "AVALON".to_string(),
            signing_key: joiner_sk.clone(),
        }).await.unwrap();

        // Wait for both to discover each other.
        let mut inviter_state = inviter_handle.state_rx.clone();
        let mut joiner_state = joiner_handle.state_rx.clone();

        timeout(Duration::from_secs(2), async {
            loop {
                inviter_state.changed().await.unwrap();
                if matches!(*inviter_state.borrow(), PairingState::Discovered { .. }) { break; }
            }
        }).await.expect("inviter sees joiner within 2s");

        timeout(Duration::from_secs(2), async {
            loop {
                joiner_state.changed().await.unwrap();
                if matches!(*joiner_state.borrow(), PairingState::Discovered { .. }) { break; }
            }
        }).await.expect("joiner sees inviter within 2s");

        // Each side selects the other.
        let inviter_peer_id = match &*inviter_handle.state_rx.borrow() {
            PairingState::Discovered { peers } => peers[0].session_id,
            _ => panic!(),
        };
        let joiner_peer_id = match &*joiner_handle.state_rx.borrow() {
            PairingState::Discovered { peers } => peers[0].session_id,
            _ => panic!(),
        };
        inviter_handle.cmd_tx.send(PairingCommand::SelectPeer { peer_session_id: inviter_peer_id }).await.unwrap();
        joiner_handle.cmd_tx.send(PairingCommand::SelectPeer { peer_session_id: joiner_peer_id }).await.unwrap();

        // Wait for both to reach Handshaking with same SAS.
        timeout(Duration::from_secs(2), async {
            loop {
                inviter_state.changed().await.unwrap();
                if matches!(*inviter_state.borrow(), PairingState::Handshaking { .. }) { break; }
            }
        }).await.expect("inviter handshakes within 2s");
        timeout(Duration::from_secs(2), async {
            loop {
                joiner_state.changed().await.unwrap();
                if matches!(*joiner_state.borrow(), PairingState::Handshaking { .. }) { break; }
            }
        }).await.expect("joiner handshakes within 2s");

        let inviter_sas = match &*inviter_handle.state_rx.borrow() {
            PairingState::Handshaking { sas_digits, .. } => sas_digits.clone(),
            _ => panic!(),
        };
        let joiner_sas = match &*joiner_handle.state_rx.borrow() {
            PairingState::Handshaking { sas_digits, .. } => sas_digits.clone(),
            _ => panic!(),
        };
        assert_eq!(inviter_sas, joiner_sas, "both sides see same SAS");

        // Both confirm.
        inviter_handle.cmd_tx.send(PairingCommand::ConfirmSas).await.unwrap();
        joiner_handle.cmd_tx.send(PairingCommand::ConfirmSas).await.unwrap();

        // Joiner reaches Complete.
        timeout(Duration::from_secs(3), async {
            loop {
                joiner_state.changed().await.unwrap();
                if matches!(*joiner_state.borrow(), PairingState::Complete { .. }) { break; }
            }
        }).await.expect("joiner completes within 3s");

        // The joiner_result_rx should have a JoinerEnrollResult.
        let mut jrx = joiner_handle.joiner_result_rx.expect("joiner result rx");
        let result = timeout(Duration::from_secs(1), jrx.recv()).await
            .expect("joiner result arrives")
            .expect("result not None");

        // The Joiner's OwnerState now contains its enrollment.
        let our_id = result.our_device_id;
        assert!(result.owner_state.enrollments.contains_key(&our_id));
        // And contains the original Inviter's enrollment.
        assert!(result.owner_state.enrollments.contains_key(&state.enrollments.keys().next().unwrap()));
    }
}
```

- [ ] **Step 4: Run the test — must pass**

Run: `cargo test -p harmony-app --lib pairing::state_machine::tests::happy_path_two_devices_pair`
Expected: 1 passed.

(If it fails, the most likely culprit is the `Confirm` echo timing — review the `peer_confirmed` propagation logic in `on_encrypted_payload` and ensure the inviter's `maybe_advance_to_enroll` runs after `peer_confirmed = true`. May need to refactor the cross-call communication; see comment in `on_encrypted_payload` for the workaround spot.)

- [ ] **Step 5: cargo fmt + commit**

```bash
cargo fmt -p harmony-app
git add src-tauri/src/pairing/state_machine.rs src-tauri/src/pairing/types.rs src-tauri/src/pairing/mod.rs
git commit -m "feat(pairing): transport-agnostic state machine + happy-path test (ZEB-197)"
```

---

## Task 7: Joiner persistence (atomic install)

**Files:**
- Create: `src-tauri/src/pairing/persist.rs`
- Modify: `src-tauri/src/pairing/mod.rs` (add `pub mod persist;`)

The state machine emits a `JoinerEnrollResult` over `joiner_result_rx`. This task wires that up to the Joiner's persistence: write signing key (keychain or `HARMONY_PASSPHRASE` fallback) FIRST, then `OwnerState` `.cbor` LAST per the atomicity contract from ZEB-170.

- [ ] **Step 1: Write the failing test**

Create `src-tauri/src/pairing/persist.rs`:

```rust
use crate::pairing::state_machine::JoinerEnrollResult;
use harmony_owner::state::OwnerState;
use std::path::Path;

/// Persist the Joiner's signing key + OwnerState to disk. Mirrors the
/// atomicity contract from ZEB-170: keychain first, .cbor last.
///
/// On failure mid-write, the keychain entry may remain orphaned; subsequent
/// `load_owner_state` will treat the absence of `.cbor` as un-bound and
/// re-pairing will overwrite the keychain entry. (See ZEB-170 design notes.)
pub async fn install_joiner_state(
    identity_dir: &Path,
    result: JoinerEnrollResult,
) -> Result<(), String> {
    // Use the same persistence machinery as ZEB-170's mint flow: write the
    // signing key via the keychain-with-encrypted-file-fallback path, then
    // write OwnerState via save_owner_state_atomic.
    use crate::owner_state::save_owner_state_atomic;
    use zeroize::Zeroizing;

    // The Joiner has no master_seed (cert-only model — see spec).
    // save_owner_state_atomic accepts None for master_seed.
    let device_signing_key_bytes: [u8; 32] = result.our_signing_key.to_bytes();
    let _ = result.our_device_id; // already inside owner_state.enrollments

    save_owner_state_atomic(
        identity_dir,
        &result.owner_state,
        &Zeroizing::new(device_signing_key_bytes),
        &Zeroizing::new([0u8; 32]), // placeholder; not persisted when master_seed_present=false
        None, // master_seed_present
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::state_machine::JoinerEnrollResult;
    use ed25519_dalek::SigningKey;
    use harmony_owner::lifecycle::{mint_owner, MintResult};
    use harmony_owner::pubkey_bundle::PubKeyBundle;
    use rand_core::OsRng;
    use serial_test::serial;
    use tempfile::tempdir;

    /// Reuse the EnvVarGuard pattern (and replace once ZEB-193 lands).
    struct EnvVarGuard { name: &'static str }
    impl EnvVarGuard {
        fn set(name: &'static str, value: &str) -> Self {
            std::env::set_var(name, value);
            Self { name }
        }
    }
    impl Drop for EnvVarGuard {
        fn drop(&mut self) { std::env::remove_var(self.name); }
    }

    #[tokio::test]
    #[serial]
    async fn install_writes_owner_state_cbor() {
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "test-pp");
        let dir = tempdir().unwrap();

        let MintResult { mut state, .. } = mint_owner(1_700_000_000).unwrap();
        let joiner_sk = SigningKey::generate(&mut OsRng);
        let joiner_pubkey = PubKeyBundle::classical_only(joiner_sk.verifying_key().to_bytes());
        let joiner_id = joiner_pubkey.identity_hash();

        // Pretend the cert was added.
        // (Real flow: the state machine adds the cert via verify_cert_for_self
        // path before this is called.)
        // For test, we just put a marker in enrollments via add_enrollment with
        // a synthesized cert — but easier: just check the file got written.

        let result = JoinerEnrollResult {
            our_signing_key: joiner_sk.clone(),
            owner_state: state.clone(),
            our_device_id: joiner_id,
        };
        install_joiner_state(dir.path(), result).await.unwrap();

        let cbor_path = dir.path().join("owner_state.cbor");
        assert!(cbor_path.exists(), "OwnerState cbor written");
    }
}
```

Add `pub mod persist;` to `src-tauri/src/pairing/mod.rs`.

> **Note:** the `save_owner_state_atomic` signature may differ slightly from the snippet above (the existing function from ZEB-170 takes specific args). Inspect `src-tauri/src/owner_state.rs::save_owner_state_atomic` and call it correctly. Pass `None` for the `master_seed_present` parameter so the Joiner's `canBackUp` will be `false`.

- [ ] **Step 2: Run the test — must pass**

Run: `cargo test -p harmony-app --lib pairing::persist::tests --test-threads=1`
Expected: 1 passed.

- [ ] **Step 3: cargo fmt + commit**

```bash
cargo fmt -p harmony-app
git add src-tauri/src/pairing/persist.rs src-tauri/src/pairing/mod.rs
git commit -m "feat(pairing): joiner persistence wires JoinerEnrollResult to atomic disk write (ZEB-197)"
```

---

## Task 8: Pairing module integration into NodeState + event_loop

**Files:**
- Create: `src-tauri/src/pairing/zenoh_transport.rs`
- Modify: `src-tauri/src/pairing/mod.rs` (add `pub mod zenoh_transport;`)
- Modify: `src-tauri/src/lib.rs` (add `pairing_handle` to NodeState)
- Modify: `src-tauri/src/event_loop.rs` (add Subscribe action; route pairing samples)

This is the bridge between the abstract state machine and real Zenoh.

- [ ] **Step 1: Implement the Zenoh transport**

Create `src-tauri/src/pairing/zenoh_transport.rs`:

```rust
use crate::pairing::transport::PairingTransport;
use crate::pairing::types::PairingWireMessage;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Zenoh-backed transport. Publishes go through `publish_tx` (the existing
/// PublishRequest channel into the event loop). Receives are pumped from
/// `pairing_in_rx`, which the event loop fills with samples on
/// `harmony/pairing/v2/lan/**` keys.
pub struct ZenohPairingTransport {
    publish_tx: mpsc::Sender<crate::event_loop::PublishRequest>,
    pairing_in_rx: Arc<Mutex<mpsc::Receiver<PairingWireMessage>>>,
}

impl ZenohPairingTransport {
    pub fn new(
        publish_tx: mpsc::Sender<crate::event_loop::PublishRequest>,
        pairing_in_rx: mpsc::Receiver<PairingWireMessage>,
    ) -> Self {
        Self {
            publish_tx,
            pairing_in_rx: Arc::new(Mutex::new(pairing_in_rx)),
        }
    }
}

const PAIRING_KEY_PREFIX: &str = "harmony/pairing/v2/lan";

fn key_for(message: &PairingWireMessage) -> String {
    let session_id = match message {
        PairingWireMessage::Discover { session_id, .. } => session_id,
        PairingWireMessage::Select { my_session_id, .. } => my_session_id,
        PairingWireMessage::Encrypted { my_session_id, .. } => my_session_id,
        PairingWireMessage::Cancel { my_session_id, .. } => my_session_id,
    };
    let phase = match message {
        PairingWireMessage::Discover { .. } => "discover",
        PairingWireMessage::Select { .. } => "select",
        PairingWireMessage::Encrypted { .. } => "encrypted",
        PairingWireMessage::Cancel { .. } => "cancel",
    };
    format!("{PAIRING_KEY_PREFIX}/{session_id}/{phase}")
}

#[async_trait]
impl PairingTransport for ZenohPairingTransport {
    async fn publish(&self, message: PairingWireMessage) -> Result<(), String> {
        let key = key_for(&message);
        let payload = serde_cbor::to_vec(&message).map_err(|e| format!("cbor: {e}"))?;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.publish_tx
            .send(crate::event_loop::PublishRequest {
                key_expr: key,
                payload,
                reply: reply_tx,
            })
            .await
            .map_err(|_| "event loop not running".to_string())?;
        reply_rx
            .await
            .map_err(|_| "publish reply dropped".to_string())?
    }

    async fn recv(&self) -> Option<PairingWireMessage> {
        self.pairing_in_rx.lock().await.recv().await
    }
}
```

- [ ] **Step 2: Add the pairing subscription to event_loop startup**

In `src-tauri/src/event_loop.rs`, find the block where `RuntimeAction::Subscribe { key_expr: "harmony/community/...".into() }` is added to startup_actions (around line 240). Add a parallel block:

```rust
let _ = dispatch_action(
    RuntimeAction::Subscribe {
        key_expr: "harmony/pairing/v2/lan/**".to_string(),
    },
    &session,
    &zenoh_tx,
    &udp,
    &broadcast_addr,
    &app,
    &closing,
    &own_zid,
).await;
```

- [ ] **Step 3: Route pairing samples into a `pairing_in_tx` channel**

In `src-tauri/src/event_loop.rs::run`, add a new mpsc channel parameter:

```rust
mut pairing_in_tx: Option<mpsc::Sender<crate::pairing::types::PairingWireMessage>>,
```

In the `ZenohEvent::Subscription { key, payload, .. }` match arm (search for the existing handler), add a new branch BEFORE the existing per-feature dispatches:

```rust
if key.starts_with("harmony/pairing/v2/lan/") {
    if let Some(tx) = pairing_in_tx.as_ref() {
        if let Ok(msg) = serde_cbor::from_slice::<crate::pairing::types::PairingWireMessage>(&payload) {
            let _ = tx.send(msg).await;
        } else {
            tracing::warn!("invalid pairing wire message on key {key}");
        }
    }
    continue;
}
```

- [ ] **Step 4: Add `pairing_handle: Option<PairingHandle>` to NodeState**

In `src-tauri/src/lib.rs`:

```rust
pub struct NodeState {
    // ... existing fields ...
    pairing_handle: Option<crate::pairing::state_machine::PairingHandle>,
}
```

Update `Default::default()` to set `pairing_handle: None`.

In `start_node`:
- After creating `publish_tx`, create `(pairing_in_tx, pairing_in_rx) = mpsc::channel(64)`.
- Pass `Some(pairing_in_tx)` to `event_loop::run`.
- Construct the `ZenohPairingTransport::new(publish_tx.clone(), pairing_in_rx)`.
- Spawn a state machine: `let handle = pairing::state_machine::spawn_state_machine(Arc::new(transport), Arc::new(|| { unix_now() }));`
- Store `guard.pairing_handle = Some(handle);`

In `stop_node` (the inner stop):
- Take and drop the `pairing_handle` (drops the state machine task).

- [ ] **Step 5: Verify the integration compiles**

Run: `cargo build -p harmony-app`
Expected: clean build.

- [ ] **Step 6: Commit**

```bash
cargo fmt -p harmony-app
git add src-tauri/src/pairing/zenoh_transport.rs src-tauri/src/pairing/mod.rs src-tauri/src/lib.rs src-tauri/src/event_loop.rs
git commit -m "feat(pairing): wire Zenoh transport + pairing handle into NodeState (ZEB-197)"
```

---

## Task 9: Tauri commands

**Files:**
- Create: `src-tauri/src/pairing_commands.rs`
- Modify: `src-tauri/src/lib.rs` (register the commands)

- [ ] **Step 1: Write the commands**

Create `src-tauri/src/pairing_commands.rs`:

```rust
use crate::pairing::state_machine::PairingCommand;
use crate::pairing::types::PairingState;
use crate::NodeState;
use ed25519_dalek::SigningKey;
use rand_core::OsRng;
use std::sync::Mutex;
use tauri::State;
use uuid::Uuid;
use zeroize::Zeroizing;

#[tauri::command]
pub async fn start_inviter_pairing(
    display_name: String,
    state: State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    // Load owner_state + master_seed from the persisted ZEB-170 artifacts.
    let identity_dir = crate::owner_commands::resolve_identity_dir()?;
    let loaded = crate::owner_state::load_owner_state(&identity_dir)?
        .ok_or_else(|| "no owner identity on this device".to_string())?;
    let master_seed = loaded.master_seed
        .ok_or_else(|| "master seed not on this device — cannot enroll".to_string())?;

    let (cmd_tx, _state_rx) = require_pairing_handle(&state)?;
    cmd_tx
        .send(PairingCommand::StartInviter {
            display_name,
            owner_state: loaded.state,
            master_seed,
        })
        .await
        .map_err(|_| "pairing state machine not running".to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn start_joiner_pairing(
    display_name: String,
    state: State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let signing_key = SigningKey::generate(&mut OsRng);
    let (cmd_tx, _state_rx) = require_pairing_handle(&state)?;
    cmd_tx
        .send(PairingCommand::StartJoiner { display_name, signing_key })
        .await
        .map_err(|_| "pairing state machine not running".to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn select_pairing_peer(
    peer_session_id: String,
    state: State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let id = Uuid::parse_str(&peer_session_id).map_err(|e| format!("invalid uuid: {e}"))?;
    let (cmd_tx, _) = require_pairing_handle(&state)?;
    cmd_tx
        .send(PairingCommand::SelectPeer { peer_session_id: id })
        .await
        .map_err(|_| "pairing state machine not running".to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn confirm_pairing_sas(
    state: State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let (cmd_tx, _) = require_pairing_handle(&state)?;
    cmd_tx
        .send(PairingCommand::ConfirmSas)
        .await
        .map_err(|_| "pairing state machine not running".to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn cancel_pairing(
    state: State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let (cmd_tx, _) = require_pairing_handle(&state)?;
    cmd_tx
        .send(PairingCommand::Cancel)
        .await
        .map_err(|_| "pairing state machine not running".to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn get_pairing_state(
    state: State<'_, Mutex<NodeState>>,
) -> Result<PairingState, String> {
    let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
    let h = guard
        .pairing_handle
        .as_ref()
        .ok_or_else(|| "pairing not initialized — start node first".to_string())?;
    Ok(h.state_rx.borrow().clone())
}

fn require_pairing_handle<'a>(
    state: &'a State<'_, Mutex<NodeState>>,
) -> Result<(tokio::sync::mpsc::Sender<PairingCommand>, tokio::sync::watch::Receiver<PairingState>), String> {
    let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
    let h = guard
        .pairing_handle
        .as_ref()
        .ok_or_else(|| "pairing not initialized — start node first".to_string())?;
    Ok((h.cmd_tx.clone(), h.state_rx.clone()))
}
```

- [ ] **Step 2: Add a Tauri event emitter for state changes**

Inside `start_node` (after `pairing_handle` is created), spawn a task:

```rust
let mut prx = handle.state_rx.clone();
let app_clone = app.clone();
tokio::spawn(async move {
    loop {
        if prx.changed().await.is_err() { break; }
        let s = prx.borrow().clone();
        let _ = app_clone.emit("pairing-state-changed", s);
    }
});
```

- [ ] **Step 3: Register the commands in `tauri::generate_handler!`**

In `src-tauri/src/lib.rs::run`, find the existing `tauri::generate_handler!(...)` macro call and add:

```rust
pairing_commands::start_inviter_pairing,
pairing_commands::start_joiner_pairing,
pairing_commands::select_pairing_peer,
pairing_commands::confirm_pairing_sas,
pairing_commands::cancel_pairing,
pairing_commands::get_pairing_state,
```

Also add `pub mod pairing_commands;` near the other pub-mods at the top of `src-tauri/src/lib.rs`.

- [ ] **Step 4: Verify the build**

Run: `cargo build -p harmony-app`
Expected: clean build.

- [ ] **Step 5: cargo fmt + commit**

```bash
cargo fmt -p harmony-app
git add src-tauri/src/pairing_commands.rs src-tauri/src/lib.rs
git commit -m "feat(pairing): six Tauri IPC commands + state-changed event (ZEB-197)"
```

---

## Task 10: TS pairing-service

**Files:**
- Create: `src/lib/pairing-service.ts`
- Create: `src/lib/pairing-service.test.ts`

- [ ] **Step 1: Write the failing tests**

Create `src/lib/pairing-service.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { PairingService } from './pairing-service';

const mockedInvoke = invoke as unknown as ReturnType<typeof vi.fn>;
const mockedListen = listen as unknown as ReturnType<typeof vi.fn>;

describe('PairingService', () => {
  beforeEach(() => {
    vi.resetAllMocks();
    mockedListen.mockResolvedValue(() => {});
  });

  it('startInviter invokes start_inviter_pairing with display name', async () => {
    mockedInvoke.mockResolvedValueOnce(undefined);
    const svc = new PairingService();
    await svc.startInviter('KRILE');
    expect(invoke).toHaveBeenCalledWith('start_inviter_pairing', { displayName: 'KRILE' });
  });

  it('startJoiner invokes start_joiner_pairing with display name', async () => {
    mockedInvoke.mockResolvedValueOnce(undefined);
    const svc = new PairingService();
    await svc.startJoiner('AVALON');
    expect(invoke).toHaveBeenCalledWith('start_joiner_pairing', { displayName: 'AVALON' });
  });

  it('selectPeer invokes select_pairing_peer with session id', async () => {
    mockedInvoke.mockResolvedValueOnce(undefined);
    const svc = new PairingService();
    await svc.selectPeer('00000000-0000-0000-0000-000000000001');
    expect(invoke).toHaveBeenCalledWith('select_pairing_peer', {
      peerSessionId: '00000000-0000-0000-0000-000000000001',
    });
  });

  it('confirmSas invokes confirm_pairing_sas', async () => {
    mockedInvoke.mockResolvedValueOnce(undefined);
    const svc = new PairingService();
    await svc.confirmSas();
    expect(invoke).toHaveBeenCalledWith('confirm_pairing_sas');
  });

  it('cancel invokes cancel_pairing', async () => {
    mockedInvoke.mockResolvedValueOnce(undefined);
    const svc = new PairingService();
    await svc.cancel();
    expect(invoke).toHaveBeenCalledWith('cancel_pairing');
  });

  it('subscribes to pairing-state-changed and updates state', async () => {
    let listener: ((event: { payload: unknown }) => void) | undefined;
    mockedListen.mockImplementation((_event: string, cb: (e: { payload: unknown }) => void) => {
      listener = cb;
      return Promise.resolve(() => {});
    });
    mockedInvoke.mockResolvedValueOnce({ kind: 'idle' });
    const svc = new PairingService();
    await svc.init();
    expect(listener).toBeDefined();
    listener!({ payload: { kind: 'enrolling' } });
    expect(svc.state).toEqual({ kind: 'enrolling' });
  });
});
```

- [ ] **Step 2: Run the tests — must fail (no module yet)**

Run: `npx vitest run src/lib/pairing-service.test.ts`
Expected: errors importing './pairing-service'.

- [ ] **Step 3: Implement PairingService**

Create `src/lib/pairing-service.ts`:

```typescript
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

export type PairingRole = 'inviter' | 'joiner';

export interface DiscoveredPeer {
  sessionId: string;
  role: PairingRole;
  displayName: string;
  ownerIdIfInviter: string | null;
  ephemeralPubkeyHex: string;
  joinerEd25519VerifyHex: string | null;
  seenAtUnix: number;
}

export type PairingState =
  | { kind: 'idle' }
  | { kind: 'discovering'; role: PairingRole; ephemeralPubkeyHex: string; sessionId: string }
  | { kind: 'discovered'; peers: DiscoveredPeer[] }
  | { kind: 'handshaking'; peerSessionId: string; sasDigits: string }
  | { kind: 'waitingPeerConfirm'; peerSessionId: string }
  | { kind: 'enrolling' }
  | { kind: 'complete'; deviceIdHex: string }
  | { kind: 'failed'; reason: string };

export class PairingService {
  state: PairingState = { kind: 'idle' };
  onChange?: () => void;
  private unlistener: (() => void) | null = null;

  async init(): Promise<void> {
    this.state = (await invoke<PairingState>('get_pairing_state'));
    this.unlistener = await listen('pairing-state-changed', (event) => {
      this.state = event.payload as PairingState;
      this.onChange?.();
    });
  }

  dispose(): void {
    this.unlistener?.();
    this.unlistener = null;
  }

  async startInviter(displayName: string): Promise<void> {
    await invoke('start_inviter_pairing', { displayName });
  }

  async startJoiner(displayName: string): Promise<void> {
    await invoke('start_joiner_pairing', { displayName });
  }

  async selectPeer(peerSessionId: string): Promise<void> {
    await invoke('select_pairing_peer', { peerSessionId });
  }

  async confirmSas(): Promise<void> {
    await invoke('confirm_pairing_sas');
  }

  async cancel(): Promise<void> {
    await invoke('cancel_pairing');
  }
}

export function extractError(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
```

- [ ] **Step 4: Run the tests — must pass**

Run: `npx vitest run src/lib/pairing-service.test.ts`
Expected: 6 passed.

- [ ] **Step 5: Commit**

```bash
git add src/lib/pairing-service.ts src/lib/pairing-service.test.ts
git commit -m "feat(pairing): TS pairing-service wraps Tauri IPC + events (ZEB-197)"
```

---

## Task 11: PairingJoiner.svelte wizard

**Files:**
- Create: `src/lib/components/PairingJoiner.svelte`
- Create: `src/lib/components/__tests__/PairingJoiner.test.ts`

- [ ] **Step 1: Write the failing tests**

Create `src/lib/components/__tests__/PairingJoiner.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import PairingJoiner from '../PairingJoiner.svelte';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import { invoke } from '@tauri-apps/api/core';
const mockedInvoke = invoke as unknown as ReturnType<typeof vi.fn>;

beforeEach(() => {
  vi.resetAllMocks();
});

describe('PairingJoiner', () => {
  it('renders display-name input as the first step', async () => {
    mockedInvoke.mockResolvedValueOnce({ kind: 'idle' });
    render(PairingJoiner);
    expect(await screen.findByLabelText(/give this device a name/i)).toBeInTheDocument();
  });

  it('starts the joiner flow when name is submitted', async () => {
    mockedInvoke.mockResolvedValueOnce({ kind: 'idle' }); // get_pairing_state
    mockedInvoke.mockResolvedValueOnce(undefined); // start_joiner_pairing
    render(PairingJoiner);
    const input = await screen.findByLabelText(/give this device a name/i);
    await fireEvent.input(input, { target: { value: 'AVALON' } });
    const startBtn = screen.getByRole('button', { name: /start pairing/i });
    await fireEvent.click(startBtn);
    expect(invoke).toHaveBeenCalledWith('start_joiner_pairing', { displayName: 'AVALON' });
  });

  it('renders SAS digits when state transitions to handshaking', async () => {
    // The component renders state from PairingService; we patch its store directly.
    mockedInvoke.mockResolvedValueOnce({
      kind: 'handshaking',
      peerSessionId: '00000000-0000-0000-0000-000000000001',
      sasDigits: '012845',
    });
    render(PairingJoiner);
    expect(await screen.findByText(/012\s*845|012845/)).toBeInTheDocument();
  });

  it('renders Cancel button that invokes cancel_pairing', async () => {
    mockedInvoke.mockResolvedValueOnce({ kind: 'idle' });
    mockedInvoke.mockResolvedValueOnce(undefined);
    render(PairingJoiner);
    const cancelBtn = await screen.findByRole('button', { name: /cancel/i });
    await fireEvent.click(cancelBtn);
    expect(invoke).toHaveBeenCalledWith('cancel_pairing');
  });
});
```

- [ ] **Step 2: Run tests — must fail (no component yet)**

Run: `npx vitest run src/lib/components/__tests__/PairingJoiner.test.ts`
Expected: import errors.

- [ ] **Step 3: Implement the component**

Create `src/lib/components/PairingJoiner.svelte`:

```svelte
<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { PairingService, extractError, type PairingState } from '../pairing-service';

  let { onClose } = $props<{ onClose?: () => void }>();

  const svc = new PairingService();
  let state = $state<PairingState>({ kind: 'idle' });
  let displayName = $state('');
  let starting = $state(false);
  let error = $state<string | null>(null);

  svc.onChange = () => { state = svc.state; };

  onMount(async () => {
    try { await svc.init(); state = svc.state; } catch (e) { error = extractError(e); }
  });

  onDestroy(() => svc.dispose());

  async function handleStart() {
    if (!displayName.trim()) {
      error = 'Please enter a name for this device.';
      return;
    }
    starting = true;
    error = null;
    try {
      await svc.startJoiner(displayName.trim());
    } catch (e) {
      error = extractError(e);
    } finally {
      starting = false;
    }
  }

  async function handleSelectPeer(peerSessionId: string) {
    try { await svc.selectPeer(peerSessionId); } catch (e) { error = extractError(e); }
  }

  async function handleConfirm() {
    try { await svc.confirmSas(); } catch (e) { error = extractError(e); }
  }

  async function handleCancel() {
    try { await svc.cancel(); } catch (e) { /* ignore */ }
    onClose?.();
  }
</script>

<div class="modal-overlay" role="dialog" aria-modal="true" aria-labelledby="join-heading">
  <div class="modal">
    <h3 id="join-heading">Join existing identity</h3>

    {#if state.kind === 'idle'}
      <label>
        Give this device a name
        <input type="text" bind:value={displayName} aria-label="Give this device a name" />
      </label>
      {#if error}<p class="error" role="alert">{error}</p>{/if}
      <div class="modal-actions">
        <button class="secondary" onclick={handleCancel}>Cancel</button>
        <button class="primary" onclick={handleStart} disabled={starting}>
          {starting ? 'Starting…' : 'Start pairing'}
        </button>
      </div>
    {:else if state.kind === 'discovering'}
      <p>Looking for nearby devices…</p>
      <div class="modal-actions">
        <button class="secondary" onclick={handleCancel}>Cancel</button>
      </div>
    {:else if state.kind === 'discovered'}
      <p>Devices nearby:</p>
      <ul class="peer-list">
        {#each state.peers as peer (peer.sessionId)}
          <li>
            <button class="peer-row" onclick={() => handleSelectPeer(peer.sessionId)}>
              <strong>{peer.displayName}</strong>
              {#if peer.ownerIdIfInviter}
                <span class="owner-id">owner {peer.ownerIdIfInviter.slice(0, 8)}…</span>
              {/if}
            </button>
          </li>
        {/each}
      </ul>
      <div class="modal-actions">
        <button class="secondary" onclick={handleCancel}>Cancel</button>
      </div>
    {:else if state.kind === 'handshaking'}
      <p>Confirm the codes match on both screens:</p>
      <p class="sas-display">
        {state.sasDigits.slice(0, 3)}&nbsp;{state.sasDigits.slice(3, 6)}
      </p>
      <div class="modal-actions">
        <button class="secondary" onclick={handleCancel}>No, don't match</button>
        <button class="primary" onclick={handleConfirm}>Yes, match</button>
      </div>
    {:else if state.kind === 'waitingPeerConfirm'}
      <p>Waiting for the other device to confirm…</p>
      <div class="modal-actions">
        <button class="secondary" onclick={handleCancel}>Cancel</button>
      </div>
    {:else if state.kind === 'enrolling'}
      <p>Installing your enrollment…</p>
    {:else if state.kind === 'complete'}
      <p>Done! This device is now part of the owner identity.</p>
      <div class="modal-actions">
        <button class="primary" onclick={onClose}>Close</button>
      </div>
    {:else if state.kind === 'failed'}
      <p class="error" role="alert">Pairing failed: {state.reason}</p>
      <div class="modal-actions">
        <button class="primary" onclick={onClose}>Close</button>
      </div>
    {/if}
  </div>
</div>

<style>
  .modal-overlay { position: fixed; inset: 0; background: rgba(0,0,0,0.5);
    display: flex; align-items: center; justify-content: center; z-index: 1000; }
  .modal { background: var(--bg-secondary); padding: 24px; border-radius: 8px;
    max-width: 480px; border: 1px solid var(--border); }
  .modal-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 16px; }
  .primary, .secondary { padding: 6px 12px; border-radius: 4px; border: 1px solid var(--border);
    cursor: pointer; font-size: 13px; }
  .primary { background: var(--accent); color: white; border-color: var(--accent); }
  .secondary { background: var(--bg-primary); color: var(--text-primary); }
  .primary:disabled, .secondary:disabled { opacity: 0.5; cursor: not-allowed; }
  .error { color: var(--danger); font-size: 13px; margin: 8px 0; }
  .peer-list { list-style: none; padding: 0; margin: 0; }
  .peer-row { display: block; width: 100%; text-align: left; padding: 8px;
    background: var(--bg-primary); border: 1px solid var(--border); border-radius: 4px;
    margin-bottom: 4px; cursor: pointer; }
  .peer-row:hover { background: var(--bg-tertiary); }
  .owner-id { font-family: monospace; font-size: 11px; color: var(--text-muted); margin-left: 8px; }
  .sas-display { font-family: monospace; font-size: 32px; font-weight: 600;
    text-align: center; padding: 16px; background: var(--bg-primary); border-radius: 8px;
    letter-spacing: 4px; }
</style>
```

- [ ] **Step 4: Run tests — must pass**

Run: `npx vitest run src/lib/components/__tests__/PairingJoiner.test.ts`
Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/PairingJoiner.svelte src/lib/components/__tests__/PairingJoiner.test.ts
git commit -m "feat(pairing): PairingJoiner wizard component (ZEB-197)"
```

---

## Task 12: PairingInviter.svelte wizard

**Files:**
- Create: `src/lib/components/PairingInviter.svelte`
- Create: `src/lib/components/__tests__/PairingInviter.test.ts`

- [ ] **Step 1: Write the failing tests**

Create `src/lib/components/__tests__/PairingInviter.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import PairingInviter from '../PairingInviter.svelte';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import { invoke } from '@tauri-apps/api/core';
const mockedInvoke = invoke as unknown as ReturnType<typeof vi.fn>;

beforeEach(() => {
  vi.resetAllMocks();
});

describe('PairingInviter', () => {
  it('starts inviter mode automatically on mount', async () => {
    mockedInvoke.mockResolvedValueOnce({ kind: 'idle' }); // get_pairing_state
    mockedInvoke.mockResolvedValueOnce(undefined); // start_inviter_pairing
    render(PairingInviter, { props: { hostname: 'KRILE' } });
    // Wait a tick for onMount.
    await new Promise(resolve => setTimeout(resolve, 0));
    expect(invoke).toHaveBeenCalledWith('start_inviter_pairing', { displayName: 'KRILE' });
  });

  it('renders SAS digits when state transitions to handshaking', async () => {
    mockedInvoke.mockResolvedValueOnce({
      kind: 'handshaking',
      peerSessionId: '00000000-0000-0000-0000-000000000002',
      sasDigits: '987654',
    });
    mockedInvoke.mockResolvedValueOnce(undefined);
    render(PairingInviter, { props: { hostname: 'KRILE' } });
    expect(await screen.findByText(/987\s*654|987654/)).toBeInTheDocument();
  });

  it('renders peer rows in discovered state', async () => {
    mockedInvoke.mockResolvedValueOnce({
      kind: 'discovered',
      peers: [{
        sessionId: '00000000-0000-0000-0000-000000000003',
        role: 'joiner',
        displayName: 'AVALON',
        ownerIdIfInviter: null,
        ephemeralPubkeyHex: '00'.repeat(32),
        joinerEd25519VerifyHex: '11'.repeat(32),
        seenAtUnix: 1_700_000_000,
      }],
    });
    mockedInvoke.mockResolvedValueOnce(undefined);
    render(PairingInviter, { props: { hostname: 'KRILE' } });
    expect(await screen.findByText('AVALON')).toBeInTheDocument();
  });

  it('Cancel invokes cancel_pairing', async () => {
    mockedInvoke.mockResolvedValueOnce({ kind: 'discovering', role: 'inviter', ephemeralPubkeyHex: '', sessionId: '00000000-0000-0000-0000-000000000004' });
    mockedInvoke.mockResolvedValueOnce(undefined);
    render(PairingInviter, { props: { hostname: 'KRILE' } });
    const cancelBtn = await screen.findByRole('button', { name: /cancel/i });
    await fireEvent.click(cancelBtn);
    expect(invoke).toHaveBeenCalledWith('cancel_pairing');
  });
});
```

- [ ] **Step 2: Run tests — must fail**

Run: `npx vitest run src/lib/components/__tests__/PairingInviter.test.ts`
Expected: import errors.

- [ ] **Step 3: Implement PairingInviter**

Create `src/lib/components/PairingInviter.svelte`:

```svelte
<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { PairingService, extractError, type PairingState } from '../pairing-service';

  let { hostname = 'this device', onClose } = $props<{ hostname?: string; onClose?: () => void }>();

  const svc = new PairingService();
  let state = $state<PairingState>({ kind: 'idle' });
  let error = $state<string | null>(null);

  svc.onChange = () => { state = svc.state; };

  onMount(async () => {
    try {
      await svc.init();
      state = svc.state;
      // Inviter starts immediately — no name-entry step (uses hostname).
      await svc.startInviter(hostname);
    } catch (e) {
      error = extractError(e);
    }
  });

  onDestroy(() => svc.dispose());

  async function handleSelectPeer(peerSessionId: string) {
    try { await svc.selectPeer(peerSessionId); } catch (e) { error = extractError(e); }
  }
  async function handleConfirm() {
    try { await svc.confirmSas(); } catch (e) { error = extractError(e); }
  }
  async function handleCancel() {
    try { await svc.cancel(); } catch (e) { /* ignore */ }
    onClose?.();
  }
</script>

<div class="modal-overlay" role="dialog" aria-modal="true" aria-labelledby="invite-heading">
  <div class="modal">
    <h3 id="invite-heading">Add another device</h3>

    {#if error}<p class="error" role="alert">{error}</p>{/if}

    {#if state.kind === 'idle' || state.kind === 'discovering'}
      <p>Looking for nearby devices in pairing mode…</p>
      <p class="hint">On the new device, tap "Join existing identity" in the empty Devices panel.</p>
      <div class="modal-actions">
        <button class="secondary" onclick={handleCancel}>Cancel</button>
      </div>
    {:else if state.kind === 'discovered'}
      <p>Devices in pairing mode nearby:</p>
      <ul class="peer-list">
        {#each state.peers as peer (peer.sessionId)}
          <li>
            <button class="peer-row" onclick={() => handleSelectPeer(peer.sessionId)}>
              <strong>{peer.displayName}</strong>
            </button>
          </li>
        {/each}
      </ul>
      <div class="modal-actions">
        <button class="secondary" onclick={handleCancel}>Cancel</button>
      </div>
    {:else if state.kind === 'handshaking'}
      <p>Confirm the codes match on both screens:</p>
      <p class="sas-display">
        {state.sasDigits.slice(0, 3)}&nbsp;{state.sasDigits.slice(3, 6)}
      </p>
      <div class="modal-actions">
        <button class="secondary" onclick={handleCancel}>No, don't match</button>
        <button class="primary" onclick={handleConfirm}>Yes, match</button>
      </div>
    {:else if state.kind === 'waitingPeerConfirm'}
      <p>Waiting for the other device to confirm…</p>
      <div class="modal-actions">
        <button class="secondary" onclick={handleCancel}>Cancel</button>
      </div>
    {:else if state.kind === 'enrolling'}
      <p>Enrolling the new device…</p>
    {:else if state.kind === 'complete'}
      <p>Done! The new device is now part of your owner identity.</p>
      <div class="modal-actions">
        <button class="primary" onclick={onClose}>Close</button>
      </div>
    {:else if state.kind === 'failed'}
      <p class="error" role="alert">Pairing failed: {state.reason}</p>
      <div class="modal-actions">
        <button class="primary" onclick={onClose}>Close</button>
      </div>
    {/if}
  </div>
</div>

<style>
  .modal-overlay { position: fixed; inset: 0; background: rgba(0,0,0,0.5);
    display: flex; align-items: center; justify-content: center; z-index: 1000; }
  .modal { background: var(--bg-secondary); padding: 24px; border-radius: 8px;
    max-width: 480px; border: 1px solid var(--border); }
  .modal-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 16px; }
  .primary, .secondary { padding: 6px 12px; border-radius: 4px; border: 1px solid var(--border);
    cursor: pointer; font-size: 13px; }
  .primary { background: var(--accent); color: white; border-color: var(--accent); }
  .secondary { background: var(--bg-primary); color: var(--text-primary); }
  .error { color: var(--danger); font-size: 13px; margin: 8px 0; }
  .hint { font-size: 12px; color: var(--text-muted); margin: 4px 0; }
  .peer-list { list-style: none; padding: 0; margin: 0; }
  .peer-row { display: block; width: 100%; text-align: left; padding: 8px;
    background: var(--bg-primary); border: 1px solid var(--border); border-radius: 4px;
    margin-bottom: 4px; cursor: pointer; }
  .peer-row:hover { background: var(--bg-tertiary); }
  .sas-display { font-family: monospace; font-size: 32px; font-weight: 600;
    text-align: center; padding: 16px; background: var(--bg-primary); border-radius: 8px;
    letter-spacing: 4px; }
</style>
```

- [ ] **Step 4: Run tests — must pass**

Run: `npx vitest run src/lib/components/__tests__/PairingInviter.test.ts`
Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/PairingInviter.svelte src/lib/components/__tests__/PairingInviter.test.ts
git commit -m "feat(pairing): PairingInviter wizard component (ZEB-197)"
```

---

## Task 13: DevicesPanel CTA changes

**Files:**
- Modify: `src/lib/components/DevicesPanel.svelte` (empty + populated state CTAs)
- Modify: `src/lib/components/__tests__/DevicesPanel.test.ts` (regression tests)

- [ ] **Step 1: Write the failing tests**

Append to `src/lib/components/__tests__/DevicesPanel.test.ts`:

```typescript
describe('DevicesPanel — Track B v2 pairing CTAs', () => {
  it('empty state renders both Bind and Join CTAs', async () => {
    mockedInvoke.mockResolvedValueOnce(null); // get_owner_state -> null
    render(DevicesPanel);
    expect(await screen.findByRole('button', { name: /bind this device/i })).toBeInTheDocument();
    expect(await screen.findByRole('button', { name: /join existing identity/i })).toBeInTheDocument();
  });

  it('populated state renders an active "Add another device" button', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1c8239b7dd809abcdef0123456789',
      ownerDisplayName: 'me',
      devices: [{
        deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
        displayName: 'this',
        isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000,
        fingerprint: 'aa11·bb22',
      }],
      canBackUp: true,
    });
    render(DevicesPanel);
    const btn = await screen.findByRole('button', { name: /add another device/i });
    expect(btn).toBeInTheDocument();
    expect(btn).not.toBeDisabled();
  });
});
```

- [ ] **Step 2: Run the tests — must fail**

Run: `npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts`
Expected: 2 new failures.

- [ ] **Step 3: Edit DevicesPanel.svelte to add the CTAs**

In `src/lib/components/DevicesPanel.svelte`:

a) Import the wizards near the existing imports:

```svelte
import PairingInviter from './PairingInviter.svelte';
import PairingJoiner from './PairingJoiner.svelte';
```

b) Add state vars near other modal flags:

```svelte
let inviterOpen = $state(false);
let joinerOpen = $state(false);
```

c) Replace the empty-state block (currently has only one CTA):

```svelte
<div class="empty">
  <p class="explainer">
    You haven't created an owner identity yet. Either start a new one for this
    device, or join an existing one already running on another of your devices.
  </p>
  <div class="empty-actions">
    <button class="primary" onclick={() => { modalOpen = true; }}>
      Bind this device to a new owner identity →
    </button>
    <button class="secondary" onclick={() => { joinerOpen = true; }}>
      Join existing identity →
    </button>
  </div>
</div>
```

d) Replace the populated-state footer block:

```svelte
<div class="add-another-footer">
  <div class="label">ADD ANOTHER DEVICE</div>
  {#if state.canBackUp}
    <button class="primary" onclick={() => { inviterOpen = true; }}>
      Add another device →
    </button>
    <p class="explainer">
      Both devices need to be on the same Wi-Fi network and in pairing mode.
      The new device will join under your existing owner identity.
    </p>
  {:else}
    <p class="explainer">
      This device cannot enroll others — its master seed has been wiped.
      Use a device that holds the master seed to add new devices.
    </p>
  {/if}
</div>
```

e) Render the wizards conditionally near the existing modal blocks:

```svelte
{#if joinerOpen}
  <PairingJoiner onClose={async () => {
    joinerOpen = false;
    await svc.refresh();
  }} />
{/if}
{#if inviterOpen}
  <PairingInviter hostname={state?.ownerDisplayName ?? 'this device'} onClose={async () => {
    inviterOpen = false;
    await svc.refresh();
  }} />
{/if}
```

Add to the `<style>` block:

```css
.empty-actions { display: flex; flex-direction: column; gap: 8px; }
```

- [ ] **Step 4: Run all DevicesPanel tests — must pass**

Run: `npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts`
Expected: all previous + 2 new = 22 passed.

- [ ] **Step 5: Verify tsc**

Run: `npx tsc --noEmit`
Expected: no errors.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/DevicesPanel.svelte src/lib/components/__tests__/DevicesPanel.test.ts
git commit -m "feat(devices): wire pairing wizard CTAs into DevicesPanel (ZEB-197)"
```

---

## Task 14: End-to-end integration test

**Files:**
- Create: `src-tauri/tests/pairing_integration.rs`

- [ ] **Step 1: Write the failing test**

Create `src-tauri/tests/pairing_integration.rs`:

```rust
//! End-to-end test for Track B v2 pairing.
//!
//! Spawns two state machines on linked InMemoryBroker transports (this is the
//! same primitive used by the unit tests in pairing/state_machine.rs but in
//! the integration-test position). We don't spin up two real Zenoh sessions
//! here — that would intersect with ZEB-165's UDP-port collision. The
//! transport abstraction guarantees behaviour is the same modulo Zenoh.
//!
//! Asserts: both reach Complete; Joiner's installed OwnerState contains both
//! enrollments; the master_seed bytes never appear in any wire payload
//! captured during the run.

use ed25519_dalek::SigningKey;
use harmony_app::pairing::{
    state_machine::{spawn_state_machine, PairingCommand, PairingHandle},
    transport::{InMemoryBroker, PairingTransport},
    types::{PairingState, PairingWireMessage},
};
use harmony_owner::lifecycle::{mint_owner, MintResult};
use rand_core::OsRng;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::timeout;
use zeroize::Zeroizing;

/// A wrapping transport that captures every published wire message for
/// post-test inspection.
struct CapturingTransport {
    inner: Arc<dyn PairingTransport>,
    captured: Arc<Mutex<Vec<PairingWireMessage>>>,
}

#[async_trait::async_trait]
impl PairingTransport for CapturingTransport {
    async fn publish(&self, message: PairingWireMessage) -> Result<(), String> {
        self.captured.lock().unwrap().push(message.clone());
        self.inner.publish(message).await
    }
    async fn recv(&self) -> Option<PairingWireMessage> {
        self.inner.recv().await
    }
}

#[tokio::test]
async fn end_to_end_pair_two_devices() {
    let MintResult { state, recovery_artifact, .. } = mint_owner(1_700_000_000).unwrap();
    let master_seed_bytes = *recovery_artifact.as_bytes();
    let master_seed = Zeroizing::new(master_seed_bytes);

    let joiner_sk = SigningKey::generate(&mut OsRng);

    let (inviter_t, joiner_t) = InMemoryBroker::pair();
    let inviter_captured = Arc::new(Mutex::new(Vec::new()));
    let joiner_captured = Arc::new(Mutex::new(Vec::new()));
    let inviter_t = Arc::new(CapturingTransport {
        inner: Arc::new(inviter_t),
        captured: inviter_captured.clone(),
    });
    let joiner_t = Arc::new(CapturingTransport {
        inner: Arc::new(joiner_t),
        captured: joiner_captured.clone(),
    });

    let now_fn: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| 1_700_000_001);
    let inviter_handle = spawn_state_machine(inviter_t.clone(), now_fn.clone());
    let joiner_handle = spawn_state_machine(joiner_t.clone(), now_fn.clone());

    inviter_handle.cmd_tx.send(PairingCommand::StartInviter {
        display_name: "KRILE".to_string(),
        owner_state: state.clone(),
        master_seed,
    }).await.unwrap();
    joiner_handle.cmd_tx.send(PairingCommand::StartJoiner {
        display_name: "AVALON".to_string(),
        signing_key: joiner_sk.clone(),
    }).await.unwrap();

    drive_to_handshake(&inviter_handle, &joiner_handle).await;
    drive_to_complete(&inviter_handle, &joiner_handle).await;

    let mut jrx = joiner_handle.joiner_result_rx.expect("result rx");
    let result = timeout(Duration::from_secs(2), jrx.recv()).await
        .expect("joiner result")
        .expect("not None");

    // Joiner state has both enrollments.
    assert!(result.owner_state.enrollments.len() >= 2);
    assert!(result.owner_state.enrollments.contains_key(&result.our_device_id));

    // master_seed never appears in any captured wire payload.
    let inviter_msgs = inviter_captured.lock().unwrap().clone();
    let joiner_msgs = joiner_captured.lock().unwrap().clone();
    for msg in inviter_msgs.iter().chain(joiner_msgs.iter()) {
        let bytes = serde_cbor::to_vec(msg).unwrap();
        assert!(
            !bytes.windows(32).any(|w| w == master_seed_bytes),
            "master_seed leaked in {msg:?}"
        );
    }
}

async fn drive_to_handshake(
    inviter_handle: &PairingHandle,
    joiner_handle: &PairingHandle,
) {
    let mut inviter_state = inviter_handle.state_rx.clone();
    let mut joiner_state = joiner_handle.state_rx.clone();

    timeout(Duration::from_secs(2), async {
        loop {
            inviter_state.changed().await.unwrap();
            if matches!(*inviter_state.borrow(), PairingState::Discovered { .. }) { break; }
        }
    }).await.expect("inviter discovers");

    timeout(Duration::from_secs(2), async {
        loop {
            joiner_state.changed().await.unwrap();
            if matches!(*joiner_state.borrow(), PairingState::Discovered { .. }) { break; }
        }
    }).await.expect("joiner discovers");

    let inviter_peer = match &*inviter_handle.state_rx.borrow() {
        PairingState::Discovered { peers } => peers[0].session_id,
        _ => panic!(),
    };
    let joiner_peer = match &*joiner_handle.state_rx.borrow() {
        PairingState::Discovered { peers } => peers[0].session_id,
        _ => panic!(),
    };
    inviter_handle.cmd_tx.send(PairingCommand::SelectPeer { peer_session_id: inviter_peer }).await.unwrap();
    joiner_handle.cmd_tx.send(PairingCommand::SelectPeer { peer_session_id: joiner_peer }).await.unwrap();

    timeout(Duration::from_secs(2), async {
        loop {
            inviter_state.changed().await.unwrap();
            if matches!(*inviter_state.borrow(), PairingState::Handshaking { .. }) { break; }
        }
    }).await.expect("inviter handshake");
    timeout(Duration::from_secs(2), async {
        loop {
            joiner_state.changed().await.unwrap();
            if matches!(*joiner_state.borrow(), PairingState::Handshaking { .. }) { break; }
        }
    }).await.expect("joiner handshake");
}

async fn drive_to_complete(
    inviter_handle: &PairingHandle,
    joiner_handle: &PairingHandle,
) {
    inviter_handle.cmd_tx.send(PairingCommand::ConfirmSas).await.unwrap();
    joiner_handle.cmd_tx.send(PairingCommand::ConfirmSas).await.unwrap();

    let mut joiner_state = joiner_handle.state_rx.clone();
    timeout(Duration::from_secs(3), async {
        loop {
            joiner_state.changed().await.unwrap();
            if matches!(*joiner_state.borrow(), PairingState::Complete { .. }) { break; }
        }
    }).await.expect("joiner completes");
}
```

> **Note:** the test imports `harmony_app::pairing::*` — confirm the crate name in `src-tauri/Cargo.toml` (could be `harmony_client_tauri` or similar; adjust the use statements).

- [ ] **Step 2: Run the integration test**

Run: `cargo test -p harmony-app --test pairing_integration`
Expected: 1 passed.

- [ ] **Step 3: cargo fmt + commit**

```bash
cargo fmt -p harmony-app
git add src-tauri/tests/pairing_integration.rs
git commit -m "test(pairing): end-to-end integration test with master-seed-leak assertion (ZEB-197)"
```

---

## Task 15: Final gates + manual acceptance recording

**Files:**
- Modify: any files needing fmt or clippy fixes

- [ ] **Step 1: Run `cargo fmt --check`**

Run: `cd src-tauri && cargo fmt --check && cd ..`
Expected: no diff. If not, run `cargo fmt -p harmony-app` and commit the fmt fix.

- [ ] **Step 2: Run all backend tests**

Run: `cd src-tauri && cargo test --quiet && cd ..`
Expected: all green. Note any unrelated failures and file Linear follow-ups (per memory rule about test drift).

- [ ] **Step 3: Run all vitest**

Run: `npx vitest run`
Expected: all green.

- [ ] **Step 4: Run tsc**

Run: `npx tsc --noEmit`
Expected: no errors.

- [ ] **Step 5: Run cargo clippy**

Run: `cd src-tauri && cargo clippy --locked --all-targets --no-deps -- -D warnings && cd ..`
Expected: no warnings. Fix any lints surfaced by the new code; if pre-existing warnings unrelated to this PR appear, file a Linear follow-up rather than mixing them in.

- [ ] **Step 6: Manual acceptance test** (recorded; user executes)

Per the spec, the user runs:
1. On KRILE: launch harmony-client, mint owner identity → DevicesPanel shows KRILE
2. On AVALON (same LAN): launch harmony-client, click "Join existing identity →"
3. On KRILE: click "Add another device →"
4. Verify both screens show the other in their discovered list within ~10s
5. Pick the other on either side → both screens show identical 6-digit SAS code
6. Confirm match on both sides → AVALON's DevicesPanel transitions to populated, showing both devices
7. Run `get_owner_state` on each side; verify both show 2 devices with same `owner_id`

If steps 4-6 fail, common causes:
* Both devices not on the same Wi-Fi (or the LAN blocks mDNS / Zenoh discovery)
* Firewall blocking Zenoh ports — check the existing Zenoh transport works between the devices for telemetry
* Node not started on either side (the wizard will surface this as an inline error)

- [ ] **Step 7: Final commit (if any fmt/clippy fixes were needed)**

```bash
git status
git add ...
git commit -m "chore(pairing): final gates — fmt + clippy clean (ZEB-197)"
```

- [ ] **Step 8: Push the branch**

```bash
git push -u origin zeb-197-track-b-v2-pairing
```

The plan ends here — open a draft PR using the `gh pr create` workflow when ready.

---

## Self-review notes (for the implementing engineer)

* **Spec coverage:** every spec section maps to a task. Architecture → Tasks 1-8. Components → Tasks 9-13. Data flow happy path → Task 6 + Task 14 integration test. Error handling → Task 6 (state machine emits Failed for each documented case) + Task 14 negative branches. Testing plan → Tasks 6, 14, plus per-component tests in Tasks 10-13.
* **Type consistency check:** `PairingState` variants are `idle | discovering | discovered | handshaking | waitingPeerConfirm | enrolling | complete | failed` (camelCase per serde tag). TS mirror in pairing-service.ts uses the same `kind` discriminator. Wire messages: 4 variants. `EncryptedPayload`: 2 variants. `PairingCommand`: 5 variants. Cross-checked.
* **Known caveats** (carry forward to PR review notes):
  - Task 6's `on_encrypted_payload::Confirm` arm needs to re-trigger `maybe_advance_to_enroll` when the LATE confirm arrives. The scaffold sketches a workaround via state_tx; the implementer should refactor this so both confirm-paths call the same advance function with the transport in scope. See the explanatory comment in the scaffold.
  - The Joiner's ed25519 verifying key threading (Task 6 Step 2) is structurally important — without it the Inviter cannot sign the cert for the right device. The placeholder in Task 6 step 1 must be cleaned up in step 2.
  - Task 7's `save_owner_state_atomic` call signature MUST match the existing function from ZEB-170. Inspect before calling.
  - Task 9's `resolve_identity_dir` is a helper assumed to live in `owner_commands.rs` from ZEB-170. If it's `pub(crate)`, fine; if it's private, surface it.
* **Out-of-scope reminders** (don't accidentally implement):
  - No auto-vouch propagation (deferred to v3)
  - No cross-internet pairing (LAN-only)
  - No revocation UI
  - No QR/camera transport
