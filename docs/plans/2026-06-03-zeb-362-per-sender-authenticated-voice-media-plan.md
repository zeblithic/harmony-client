# ZEB-362 Per-Sender Authenticated Voice Media — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bind every community-voice media frame to its sender's device key (encrypt-then-sign Ed25519 v2 envelope) so a receiver authenticates the true owner from the verified presence roster instead of trusting the sender-controlled Zenoh topic suffix — closing the ZEB-358 moderation-drop evasion.

**Architecture:** Rust-only, community-voice path. `voice_crypto.rs` gains three **additive** v2 helpers (`seal_and_sign_voice_packet`, `verify_voice_frame_sig`, `open_voice_packet`); the generic `encrypt_voice_packet`/`decrypt_voice_packet` stay (presence + moderation still use them). The two community-media call sites in `event_loop.rs` switch to v2: the publish arm signs with the device key already stashed in `voice_identity`; the subscribe arm runs an always-verify, fail-closed sequence (claimed-device → verified-owner → verify-sig → moderation-drop → open → attribution-check). Zero frontend changes.

**Tech Stack:** Rust, `ed25519-dalek` 2.x (sign/verify), `chacha20poly1305` (existing AEAD), `cargo-nextest`, the `test-fixtures` feature for deterministic wire pins.

**Settled (do not reopen):** community-voice only; always sign + always verify; clean v2 break (no active users, no dual-version interop); fail-closed. DM/group media path untouched.

**Spec:** `docs/specs/2026-06-03-zeb-362-per-sender-authenticated-voice-media-design.md` (commit `62c1278`).

---

## Files

| File | Responsibility | Change |
|---|---|---|
| `src-tauri/src/voice_crypto.rs` | AEAD seam for voice packets | **Add** v2 media helpers + consts + `SigFailed` error + v2 unit tests. Generic functions untouched. |
| `src-tauri/tests/voice_media_auth_integration.rs` | End-to-end sender-auth security properties | **Create** — honest accept, spoofed-suffix drop, attribution-mismatch drop, tamper drop. |
| `src-tauri/tests/wire_format_voice_fixtures.rs` | On-wire byte-identity pins | **Replace** the community `voice_packet_wire_bytes_pinned` with a v2 pin; fix imports. |
| `src-tauri/src/event_loop.rs` | Voice media publish + subscribe arms | Publish: sign via `voice_identity` signing key. Subscribe: always-verify fail-closed sequence. |

**Per-task gates (harmony-app relink cost — do NOT use `--all-targets` until the final task):**
```bash
cd src-tauri && cargo fmt --all
cd src-tauri && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```
Integration-test tasks additionally run only their file, e.g.:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test voice_media_auth_integration
cd src-tauri && cargo nextest run --locked --features test-fixtures --test wire_format_voice_fixtures
```

**Implementer discipline (every task):** commit BEFORE running any long gate; enforce a 10-minute wall-clock kill switch on any single cargo command (if it exceeds 10 min, kill it, report, and return `DONE_WITH_CONCERNS`); use the `DONE_WITH_CONCERNS` escape rather than silently stalling. The 6 known iroh/zenoh loopback flakes are non-blocking.

---

## Task 0: Pre-flight baseline

**Files:** none (verification only).

- [ ] **Step 1: Confirm the branch + a green starting point**

Run:
```bash
git -C /Users/zeblith/work/zeblithic/harmony-client branch --show-current
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(voice_crypto)'
```
Expected: branch is `zeb-362-per-sender-authenticated-voice-media`; the existing `voice_crypto` tests pass (round_trip_voice_packet, wrong_key_drops, etc.). If any fail on a clean checkout, STOP and report — that's pre-existing drift to fix first (test drift is ours).

- [ ] **Step 2: Confirm fmt clean**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo fmt --all -- --check`
Expected: no output, exit 0.

---

## Task 1: voice_crypto v2 helpers + unit tests

**Files:**
- Modify: `src-tauri/src/voice_crypto.rs`

- [ ] **Step 1: Add the `SigFailed` error variant**

In the `VoiceCryptoError` enum (after the `OpenFailed` variant, ~line 42), add:
```rust
    #[error("voice packet signature verification failed")]
    SigFailed,
```

- [ ] **Step 2: Add the v2 constants**

After the existing domain constants (after `VOICE_MODERATION_AAD`, ~line 23), add:
```rust
/// ZEB-362 v2 AAD domain for community voice MEDIA packets. Bumped from
/// `VOICE_PACKET_AAD` (v1) so a stray unsigned v1 frame cleanly fails to open
/// rather than mis-parsing. v2 packets also carry a detached sender signature.
pub const VOICE_PACKET_AAD_V2: &[u8] = b"harmony-voice-pkt-v2";
/// Domain separator for the per-frame sender-signature transcript. Distinct
/// from every AAD domain so a media signature can never be confused with any
/// other signed/sealed voice artifact.
pub const VOICE_PACKET_SIG_DOMAIN_V2: &[u8] = b"harmony-voice-pkt-sig-v2";
/// Detached Ed25519 signature length appended to a v2 community voice packet.
pub const SIG_LEN: usize = 64;
/// Minimum v2 packet length: nonce(12) + tag(16) + sig(64), empty plaintext.
pub const MIN_PACKET_LEN_V2: usize = NONCE_LEN + TAG_LEN + SIG_LEN;
```

- [ ] **Step 3: Add the v2 AAD + transcript helpers**

After `scope_aad` (~line 53), add:
```rust
/// v2 AAD = domain ‖ community_id(16) ‖ channel_id(16) ‖ sender_device_vk(32).
/// Binds a v2 media packet to its (community, channel) scope AND the claimed
/// sender device (defense-in-depth; the detached signature is what actually
/// authenticates the sender, since the shared key alone can't).
fn scope_aad_v2(
    domain: &[u8],
    community: &SpaceId,
    channel: &ChannelId,
    device_vk: &[u8; 32],
) -> Vec<u8> {
    let mut aad = Vec::with_capacity(domain.len() + 16 + 16 + 32);
    aad.extend_from_slice(domain);
    aad.extend_from_slice(&community.0);
    aad.extend_from_slice(&channel.0);
    aad.extend_from_slice(device_vk);
    aad
}

/// Transcript the sender's device key signs, over the CIPHERTEXT
/// (encrypt-then-sign): sig_domain ‖ community(16) ‖ channel(16) ‖ nonce(12) ‖
/// ciphertext+tag. Signing the ciphertext lets a receiver reject a forgery
/// before spending an AEAD-open and binds the exact transmitted bytes.
fn voice_sig_transcript(
    community: &SpaceId,
    channel: &ChannelId,
    nonce: &[u8; NONCE_LEN],
    ct: &[u8],
) -> Vec<u8> {
    let mut t =
        Vec::with_capacity(VOICE_PACKET_SIG_DOMAIN_V2.len() + 16 + 16 + NONCE_LEN + ct.len());
    t.extend_from_slice(VOICE_PACKET_SIG_DOMAIN_V2);
    t.extend_from_slice(&community.0);
    t.extend_from_slice(&channel.0);
    t.extend_from_slice(nonce);
    t.extend_from_slice(ct);
    t
}
```

- [ ] **Step 4: Add `seal_and_sign_voice_packet` (+ inner)**

After `decrypt_voice_packet` (~line 127), add:
```rust
/// ZEB-362: seal `plaintext` for `(community, channel)` and append a detached
/// Ed25519 signature by `device_sk` over the ciphertext. A receiver verifies
/// the signature against the device VK it trusts from the presence roster,
/// binding the frame to its true sender. Output:
/// `[12B nonce][ChaCha20-Poly1305 ct+tag][64B Ed25519 sig]`.
pub fn seal_and_sign_voice_packet(
    key: &ChannelKey,
    device_sk: &ed25519_dalek::SigningKey,
    community: &SpaceId,
    channel: &ChannelId,
    plaintext: &[u8],
) -> Result<Vec<u8>, VoiceCryptoError> {
    use chacha20poly1305::aead::OsRng;
    use chacha20poly1305::AeadCore;
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let nonce_bytes: [u8; NONCE_LEN] = nonce.into();
    seal_and_sign_inner(key, device_sk, community, channel, plaintext, nonce_bytes)
}

fn seal_and_sign_inner(
    key: &ChannelKey,
    device_sk: &ed25519_dalek::SigningKey,
    community: &SpaceId,
    channel: &ChannelId,
    plaintext: &[u8],
    nonce_bytes: [u8; NONCE_LEN],
) -> Result<Vec<u8>, VoiceCryptoError> {
    use ed25519_dalek::Signer;
    let device_vk = device_sk.verifying_key().to_bytes();
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let aad = scope_aad_v2(VOICE_PACKET_AAD_V2, community, channel, &device_vk);
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| VoiceCryptoError::SealFailed)?;
    let transcript = voice_sig_transcript(community, channel, &nonce_bytes, &ct);
    let sig = device_sk.sign(&transcript).to_bytes();
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len() + SIG_LEN);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    out.extend_from_slice(&sig);
    Ok(out)
}
```

- [ ] **Step 5: Add `verify_voice_frame_sig` and `open_voice_packet`**

Immediately after the seal function, add:
```rust
/// ZEB-362: verify the detached sender signature on a v2 community voice packet
/// against `device_vk` (the claimed sender from the topic suffix, confirmed
/// present in the verified roster by the caller). Public-key only — no channel
/// key, no decryption. `Ok(())` iff the holder of `device_vk`'s private key
/// produced this exact ciphertext.
pub fn verify_voice_frame_sig(
    device_vk: &[u8; 32],
    community: &SpaceId,
    channel: &ChannelId,
    packet: &[u8],
) -> Result<(), VoiceCryptoError> {
    if packet.len() < MIN_PACKET_LEN_V2 {
        return Err(VoiceCryptoError::TooShort(packet.len()));
    }
    let sig_start = packet.len() - SIG_LEN;
    let (nonce_and_ct, sig_bytes) = packet.split_at(sig_start);
    let (nonce_bytes, ct) = nonce_and_ct.split_at(NONCE_LEN);
    let vk = ed25519_dalek::VerifyingKey::from_bytes(device_vk)
        .map_err(|_| VoiceCryptoError::SigFailed)?;
    let sig_arr: [u8; SIG_LEN] = sig_bytes
        .try_into()
        .map_err(|_| VoiceCryptoError::SigFailed)?;
    let sig = ed25519_dalek::Signature::from_bytes(&sig_arr);
    let nonce_arr: [u8; NONCE_LEN] = nonce_bytes
        .try_into()
        .map_err(|_| VoiceCryptoError::SigFailed)?;
    let transcript = voice_sig_transcript(community, channel, &nonce_arr, ct);
    vk.verify_strict(&transcript, &sig)
        .map_err(|_| VoiceCryptoError::SigFailed)
}

/// ZEB-362: open a v2 community voice packet sealed by
/// [`seal_and_sign_voice_packet`]. Verify the signature first via
/// [`verify_voice_frame_sig`]; this strips the trailing signature and AEAD-opens
/// `[nonce][ct]` with the v2 AAD (which binds `device_vk`). Any failure returns
/// an error — callers drop.
pub fn open_voice_packet(
    key: &ChannelKey,
    device_vk: &[u8; 32],
    community: &SpaceId,
    channel: &ChannelId,
    packet: &[u8],
) -> Result<Vec<u8>, VoiceCryptoError> {
    if packet.len() < MIN_PACKET_LEN_V2 {
        return Err(VoiceCryptoError::TooShort(packet.len()));
    }
    let sig_start = packet.len() - SIG_LEN;
    let nonce_and_ct = &packet[..sig_start];
    let (nonce_bytes, ct) = nonce_and_ct.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let aad = scope_aad_v2(VOICE_PACKET_AAD_V2, community, channel, device_vk);
    cipher
        .decrypt(
            Nonce::from_slice(nonce_bytes),
            Payload { msg: ct, aad: &aad },
        )
        .map_err(|_| VoiceCryptoError::OpenFailed)
}
```

- [ ] **Step 6: Add the deterministic fixture variant**

Next to the existing `encrypt_voice_packet_with_nonce` (~line 302), add:
```rust
/// Deterministic-nonce v2 variant for wire-format fixtures. NEVER call from
/// production — a fixed nonce with a reused key is catastrophic nonce reuse.
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn seal_and_sign_voice_packet_with_nonce(
    key: &ChannelKey,
    device_sk: &ed25519_dalek::SigningKey,
    community: &SpaceId,
    channel: &ChannelId,
    plaintext: &[u8],
    nonce: [u8; NONCE_LEN],
) -> Result<Vec<u8>, VoiceCryptoError> {
    seal_and_sign_inner(key, device_sk, community, channel, plaintext, nonce)
}
```

- [ ] **Step 7: Write the v2 unit tests**

In the `#[cfg(test)] mod tests` block (after the existing `deterministic_nonce_variant_is_stable` test, ~line 390), add a fixed signing key helper and the v2 tests:
```rust
    fn dev_sk(b: u8) -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[b; 32])
    }

    #[test]
    fn v2_round_trip_seal_verify_open() {
        let k = key();
        let sk = dev_sk(1);
        let vk = sk.verifying_key().to_bytes();
        let plain = b"opus-frame-bytes-1234567890".to_vec();
        let sealed = seal_and_sign_voice_packet(&k, &sk, &C, &CH, &plain).unwrap();
        assert!(sealed.len() >= MIN_PACKET_LEN_V2);
        verify_voice_frame_sig(&vk, &C, &CH, &sealed).unwrap();
        assert_eq!(open_voice_packet(&k, &vk, &C, &CH, &sealed).unwrap(), plain);
    }

    #[test]
    fn v2_tampered_ciphertext_fails_open() {
        let k = key();
        let sk = dev_sk(1);
        let vk = sk.verifying_key().to_bytes();
        let mut sealed = seal_and_sign_voice_packet(&k, &sk, &C, &CH, b"hello").unwrap();
        // Flip a ciphertext byte (after the 12B nonce). The sig is over the
        // ciphertext, so verify fails first; assert open fails too on its own.
        sealed[NONCE_LEN + 1] ^= 0xff;
        assert_eq!(
            verify_voice_frame_sig(&vk, &C, &CH, &sealed),
            Err(VoiceCryptoError::SigFailed)
        );
        // Re-seal clean, then tamper only what open sees by re-signing is not
        // possible without the key; instead prove open rejects a flipped tag.
        let mut s2 = seal_and_sign_voice_packet(&k, &sk, &C, &CH, b"hello").unwrap();
        let tag_byte = s2.len() - SIG_LEN - 1; // last ciphertext/tag byte
        s2[tag_byte] ^= 0xff;
        assert_eq!(
            open_voice_packet(&k, &vk, &C, &CH, &s2),
            Err(VoiceCryptoError::OpenFailed)
        );
    }

    #[test]
    fn v2_tampered_signature_fails_verify() {
        let k = key();
        let sk = dev_sk(1);
        let vk = sk.verifying_key().to_bytes();
        let mut sealed = seal_and_sign_voice_packet(&k, &sk, &C, &CH, b"hello").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        assert_eq!(
            verify_voice_frame_sig(&vk, &C, &CH, &sealed),
            Err(VoiceCryptoError::SigFailed)
        );
    }

    #[test]
    fn v2_wrong_device_vk_fails_verify() {
        let k = key();
        let sk = dev_sk(1);
        let other_vk = dev_sk(2).verifying_key().to_bytes();
        let sealed = seal_and_sign_voice_packet(&k, &sk, &C, &CH, b"hello").unwrap();
        assert_eq!(
            verify_voice_frame_sig(&other_vk, &C, &CH, &sealed),
            Err(VoiceCryptoError::SigFailed)
        );
    }

    #[test]
    fn v2_cross_channel_fails_verify_and_open() {
        let k = key();
        let sk = dev_sk(1);
        let vk = sk.verifying_key().to_bytes();
        let other_ch = ChannelId([0xc2; 16]);
        let sealed = seal_and_sign_voice_packet(&k, &sk, &C, &CH, b"hello").unwrap();
        // transcript includes the channel id → verifying for another channel fails
        assert_eq!(
            verify_voice_frame_sig(&vk, &C, &other_ch, &sealed),
            Err(VoiceCryptoError::SigFailed)
        );
        // AAD also includes the channel id → opening for another channel fails
        assert_eq!(
            open_voice_packet(&k, &vk, &C, &other_ch, &sealed),
            Err(VoiceCryptoError::OpenFailed)
        );
    }

    #[test]
    fn v2_rejects_v1_shaped_unsigned_frame() {
        // A v1 frame is [nonce][ct] with no 64B sig. Most are shorter than the
        // v2 minimum; even a long one fails the signature check.
        let k = key();
        let v1 = encrypt_voice_packet(&k, &C, &CH, VOICE_PACKET_AAD, b"hello").unwrap();
        let vk = dev_sk(1).verifying_key().to_bytes();
        assert!(verify_voice_frame_sig(&vk, &C, &CH, &v1).is_err());
    }

    #[test]
    fn v2_too_short_is_rejected() {
        let vk = dev_sk(1).verifying_key().to_bytes();
        assert_eq!(
            verify_voice_frame_sig(&vk, &C, &CH, b"short"),
            Err(VoiceCryptoError::TooShort(5))
        );
    }

    #[test]
    fn v2_deterministic_variant_is_stable() {
        let k = key();
        let sk = dev_sk(7);
        let a =
            seal_and_sign_voice_packet_with_nonce(&k, &sk, &C, &CH, b"hi", [0u8; 12]).unwrap();
        let b =
            seal_and_sign_voice_packet_with_nonce(&k, &sk, &C, &CH, b"hi", [0u8; 12]).unwrap();
        assert_eq!(a, b);
        assert_eq!(&a[..NONCE_LEN], &[0u8; 12]);
    }
```

- [ ] **Step 8: Gate + commit**

Run (commit first, then gate — kill any cargo step exceeding 10 min):
```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo fmt --all
git -C /Users/zeblith/work/zeblithic/harmony-client add src-tauri/src/voice_crypto.rs
git -C /Users/zeblith/work/zeblithic/harmony-client commit -m "feat(zeb-362): v2 seal-and-sign voice media helpers + unit tests

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(voice_crypto)'
```
Expected: clippy clean; all `voice_crypto` tests pass (the eight new `v2_*` tests + the existing ones).

---

## Task 2: Receiver-decision security integration test

**Files:**
- Create: `src-tauri/tests/voice_media_auth_integration.rs`

This test drives the exact receiver decision the `event_loop` subscribe arm will run (Task 4), proving the headline security properties deterministically — no Zenoh transport, no flakes. The live-transport relay path stays covered by `tests/voice_presence_two_engine_integration.rs`.

- [ ] **Step 1: Write the failing test file**

Create `src-tauri/tests/voice_media_auth_integration.rs`:
```rust
//! ZEB-362: per-sender authenticated community voice media. Proves a receiver
//! binds each frame to its true owner device via the detached signature, so a
//! modified client cannot evade a mute/kick (or impersonate another speaker) by
//! lying about the Zenoh topic suffix. Drives the same decision the event_loop
//! subscribe arm runs (verify sig → open → attribution), minus the async
//! presence/moderation map lookups the event loop owns.
#![cfg(feature = "test-fixtures")]

use ed25519_dalek::SigningKey;
use harmony_app::community_channel_log::{derive_channel_key, ChannelKey};
use harmony_app::community_membership::ChannelId;
use harmony_app::owner_state_types::{EpochKey, SpaceId};
use harmony_app::voice_crypto::{
    open_voice_packet, seal_and_sign_voice_packet, verify_voice_frame_sig,
};

const C: SpaceId = SpaceId([0xc0; 16]);
const CH: ChannelId = ChannelId([0xc1; 16]);

fn channel_key() -> ChannelKey {
    derive_channel_key(&EpochKey::new([0x11; 32]), &C, &CH)
}

/// 23-byte voice header (flags|seq|ts|senderHash) carrying `sender_vk`'s 16-byte
/// prefix as the senderHash (bytes 7..23, mirroring the frontend layout) + a
/// short payload.
fn frame_with_header(sender_vk_prefix: &[u8; 32]) -> Vec<u8> {
    let mut f = vec![0u8; 23];
    f[0] = 0x10; // version nibble
    f[7..23].copy_from_slice(&sender_vk_prefix[..16]);
    f.extend_from_slice(b"opus-payload");
    f
}

/// Mirror of the event_loop subscribe decision (verify sig → open →
/// attribution). `claimed_dev` is what the receiver parses from the topic
/// suffix. Returns the opened frame, or a drop reason.
fn receive(key: &ChannelKey, claimed_dev: &[u8; 32], packet: &[u8]) -> Result<Vec<u8>, &'static str> {
    verify_voice_frame_sig(claimed_dev, &C, &CH, packet).map_err(|_| "sig")?;
    let frame = open_voice_packet(key, claimed_dev, &C, &CH, packet).map_err(|_| "open")?;
    if frame.len() < 23 || frame[7..23] != claimed_dev[..16] {
        return Err("attribution");
    }
    Ok(frame)
}

#[test]
fn honest_frame_is_accepted() {
    let key = channel_key();
    let a = SigningKey::from_bytes(&[1u8; 32]);
    let a_vk = a.verifying_key().to_bytes();
    let frame = frame_with_header(&a_vk);
    let packet = seal_and_sign_voice_packet(&key, &a, &C, &CH, &frame).unwrap();
    assert_eq!(receive(&key, &a_vk, &packet).unwrap(), frame);
}

#[test]
fn spoofed_suffix_without_senders_key_is_dropped() {
    // The muted/kicked attacker B seals with B's OWN key but publishes under
    // A's device suffix to evade a drop on B. Receiver parses A's suffix →
    // verifies against A's VK → B's signature fails → dropped. This IS the
    // muted-owner evasion attempt, now closed.
    let key = channel_key();
    let a_vk = SigningKey::from_bytes(&[1u8; 32]).verifying_key().to_bytes();
    let b = SigningKey::from_bytes(&[2u8; 32]);
    let b_vk = b.verifying_key().to_bytes();
    let frame = frame_with_header(&b_vk);
    let packet = seal_and_sign_voice_packet(&key, &b, &C, &CH, &frame).unwrap();
    assert_eq!(receive(&key, &a_vk, &packet), Err("sig"));
}

#[test]
fn attribution_mismatch_is_dropped() {
    // A signs a valid frame but stamps B's senderHash into the cleartext header
    // to mislabel the audio as B. Receiver verifies A's sig + opens, but the
    // header senderHash != A → dropped.
    let key = channel_key();
    let a = SigningKey::from_bytes(&[1u8; 32]);
    let a_vk = a.verifying_key().to_bytes();
    let b_vk = SigningKey::from_bytes(&[2u8; 32]).verifying_key().to_bytes();
    let frame = frame_with_header(&b_vk); // header lies: says B
    let packet = seal_and_sign_voice_packet(&key, &a, &C, &CH, &frame).unwrap();
    assert_eq!(receive(&key, &a_vk, &packet), Err("attribution"));
}

#[test]
fn tampered_ciphertext_is_dropped() {
    let key = channel_key();
    let a = SigningKey::from_bytes(&[1u8; 32]);
    let a_vk = a.verifying_key().to_bytes();
    let frame = frame_with_header(&a_vk);
    let mut packet = seal_and_sign_voice_packet(&key, &a, &C, &CH, &frame).unwrap();
    packet[12 + 1] ^= 0xff; // flip a ciphertext byte → sig verify fails
    assert_eq!(receive(&key, &a_vk, &packet), Err("sig"));
}
```

- [ ] **Step 2: Run the test**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures --test voice_media_auth_integration`
Expected: 4 tests pass (they depend only on the Task 1 helpers). If `honest_frame_is_accepted` fails, the seal/open/attribution composition is wrong — fix before proceeding.

- [ ] **Step 3: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo fmt --all
git -C /Users/zeblith/work/zeblithic/harmony-client add src-tauri/tests/voice_media_auth_integration.rs
git -C /Users/zeblith/work/zeblithic/harmony-client commit -m "test(zeb-362): sender-auth security properties (spoof/attribution/tamper)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Wire-format v2 fixture pin

**Files:**
- Modify: `src-tauri/tests/wire_format_voice_fixtures.rs`

- [ ] **Step 1: Fix the imports**

In the `use harmony_app::voice_crypto::{...}` block (~line 17), the community-media v1 imports become unused once the pin is v2. Replace:
```rust
use harmony_app::voice_crypto::{
    encrypt_dm_voice_packet_with_nonce, encrypt_voice_packet_with_nonce, VOICE_DM_PACKET_AAD,
    VOICE_PACKET_AAD,
};
```
with:
```rust
use harmony_app::voice_crypto::{
    encrypt_dm_voice_packet_with_nonce, open_voice_packet, seal_and_sign_voice_packet_with_nonce,
    verify_voice_frame_sig, VOICE_DM_PACKET_AAD,
};
```
(`encrypt_dm_voice_packet_with_nonce` + `VOICE_DM_PACKET_AAD` are still used by `dm_voice_packet_wire_bytes_pinned`.)

- [ ] **Step 2: Replace the community media pin with a v2 pin**

Replace the entire `voice_packet_wire_bytes_pinned` test (~lines 30-52) with:
```rust
#[test]
fn voice_packet_v2_wire_bytes_pinned() {
    let key = derive_channel_key(
        &EpochKey::new([0x11; 32]),
        &SpaceId([0xc0; 16]),
        &ChannelId([0xc1; 16]),
    );
    // Fixed device-#2 signing key [7u8;32] (matches the presence-beacon fixture),
    // 23-byte header + short payload, zeroed nonce → fully deterministic envelope
    // [nonce(12)][ct+tag][sig(64)]. Ed25519 is deterministic for a fixed key+msg.
    let device_sk = SigningKey::from_bytes(&[7u8; 32]);
    let device_vk = device_sk.verifying_key().to_bytes();
    let frame: Vec<u8> = (0u8..30).collect();
    let sealed = seal_and_sign_voice_packet_with_nonce(
        &key,
        &device_sk,
        &SpaceId([0xc0; 16]),
        &ChannelId([0xc1; 16]),
        &frame,
        [0u8; 12],
    )
    .expect("seal+sign");
    // GENERATE-THEN-PIN: run this test once with the placeholder below; it will
    // fail and print the actual hex. Paste that value here, then re-run to green.
    let expected = "PASTE_ACTUAL_HEX_FROM_FIRST_RUN";
    assert_eq!(
        hex::encode(&sealed),
        expected,
        "sealed v2 voice-packet wire format drifted"
    );
    // Meaningful, not opaque: the pinned bytes must verify + open back to `frame`.
    verify_voice_frame_sig(
        &device_vk,
        &SpaceId([0xc0; 16]),
        &ChannelId([0xc1; 16]),
        &sealed,
    )
    .expect("pinned v2 packet must verify");
    assert_eq!(
        open_voice_packet(
            &key,
            &device_vk,
            &SpaceId([0xc0; 16]),
            &ChannelId([0xc1; 16]),
            &sealed,
        )
        .expect("pinned v2 packet must open"),
        frame,
        "pinned v2 packet opened to a different frame"
    );
}
```

- [ ] **Step 3: Generate the pinned hex**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures --test wire_format_voice_fixtures -E 'test(voice_packet_v2_wire_bytes_pinned)' --no-capture`
Expected: FAIL with a left/right hex mismatch. Copy the **left** (actual) hex string and paste it as `expected` in place of `PASTE_ACTUAL_HEX_FROM_FIRST_RUN`.

- [ ] **Step 4: Re-run to green**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures --test wire_format_voice_fixtures`
Expected: all pins pass (the new v2 community pin + the untouched DM/presence/group/moderation pins).

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo fmt --all
git -C /Users/zeblith/work/zeblithic/harmony-client add src-tauri/tests/wire_format_voice_fixtures.rs
git -C /Users/zeblith/work/zeblithic/harmony-client commit -m "test(zeb-362): pin v2 signed community voice-packet wire format

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: event_loop media arms — sign on publish, always-verify on subscribe

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (publish arm ~2829-2858; subscribe arm ~2954-3016)

- [ ] **Step 1: Sign on publish**

Replace the community publish block (the `crate::voice::VoiceOutbound::Channel { .. } => { ... }` body, ~lines 2829-2858, specifically the `if let Some(key) = voice_keys.get(...)` block) with:
```rust
                    crate::voice::VoiceOutbound::Channel { community_id, channel_id, frame } => {
                        // ZEB-362: seal the frame AND sign it with this device's
                        // signing key (the same device-#2 key that signs presence
                        // beacons, stashed in `voice_identity` at Join), then
                        // publish to the own-device-named topic. A receiver now
                        // authenticates the sender from the verified presence map
                        // instead of trusting the (sender-controlled) topic suffix.
                        if let (Some(key), Some(identity)) = (
                            voice_keys.get(&(community_id, channel_id)),
                            voice_identity.get(&(community_id, channel_id)),
                        ) {
                            let own = voice_own_device.as_deref().unwrap_or("self");
                            match crate::voice_crypto::seal_and_sign_voice_packet(
                                key,
                                &identity.3,
                                &community_id,
                                &channel_id,
                                &frame,
                            ) {
                                Ok(sealed) => {
                                    let key_expr = format!(
                                        "harmony/voice/{}/{}/{}",
                                        hex::encode(community_id.0),
                                        hex::encode(channel_id.0),
                                        own,
                                    );
                                    if let Err(e) = session.put(&key_expr, sealed).await {
                                        tracing::warn!(%key_expr, err = %e, "voice publish failed");
                                    }
                                }
                                Err(e) => tracing::warn!(err = %e, "voice seal+sign failed; dropping frame"),
                            }
                        }
                        // else: not joined / no signing identity for that
                        // (community, channel) — drop.
                    }
```
(`identity.3` is the `Arc<SigningKey>`; `&identity.3` deref-coerces to `&SigningKey`, exactly as the existing subscribe code passes `&key_for_sub` (an `Arc<ChannelKey>`) where `&ChannelKey` is expected.)

- [ ] **Step 2: Always-verify, fail-closed on subscribe**

Replace the body of the inner receive loop — from the oversize-cap check through the `decrypt_voice_packet` match (~lines 2956-3015) — with:
```rust
                                            if sample.payload().len() > crate::voice_crypto::MAX_VOICE_PACKET_BYTES {
                                                tracing::warn!(
                                                    len = sample.payload().len(),
                                                    max = crate::voice_crypto::MAX_VOICE_PACKET_BYTES,
                                                    "oversized voice packet dropped"
                                                );
                                                continue;
                                            }
                                            // ZEB-362: authenticate the sender of EVERY frame
                                            // (always-verify, fail-closed). The topic suffix is
                                            // now an untrusted hint; the detached Ed25519
                                            // signature, checked against the device VK we trust
                                            // from the verified presence roster, binds the frame
                                            // to its owner.
                                            //
                                            // (1) parse the claimed device VK from the suffix.
                                            let dev = match sample
                                                .key_expr()
                                                .as_str()
                                                .rsplit('/')
                                                .next()
                                                .and_then(|h| hex::decode(h).ok())
                                                .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
                                            {
                                                Some(d) => d,
                                                None => continue, // not 32-byte hex → drop
                                            };
                                            // (2) device → verified owner from the signed presence
                                            //     roster. Unknown device → drop (fail-closed). The
                                            //     start-muted invariant (D10) means a transmitting
                                            //     device has already announced presence, so this
                                            //     costs no real audio.
                                            let owner = {
                                                let g = presence_map_media.lock().await;
                                                g.owner_for_device(&c_sub, &ch_sub, &dev)
                                            };
                                            let owner = match owner {
                                                Some(o) => o,
                                                None => continue,
                                            };
                                            let sealed = sample.payload().to_bytes().to_vec();
                                            // (3) verify the per-frame signature against the device.
                                            if crate::voice_crypto::verify_voice_frame_sig(
                                                &dev, &c_sub, &ch_sub, &sealed,
                                            )
                                            .is_err()
                                            {
                                                continue; // forged / spoofed-suffix / corrupt → drop
                                            }
                                            // (4) moderation drop on the now-AUTHENTICATED owner,
                                            //     gated for the hot path (no extra locks while no
                                            //     moderation is active — Qodo perf).
                                            if mod_active_media
                                                .load(std::sync::atomic::Ordering::Relaxed)
                                            {
                                                let now = (voice_now_ms_media)();
                                                let g = mod_map_media.lock().await;
                                                if g.is_muted(&c_sub, &ch_sub, &owner, now)
                                                    || g.is_kicked(&c_sub, &ch_sub, &owner, now)
                                                {
                                                    continue; // moderated sender — un-spoofable now
                                                }
                                            }
                                            // (5) open the packet (AAD binds the device VK).
                                            let frame = match crate::voice_crypto::open_voice_packet(
                                                &key_for_sub,
                                                &dev,
                                                &c_sub,
                                                &ch_sub,
                                                &sealed,
                                            ) {
                                                Ok(f) => f,
                                                Err(_) => continue, // wrong key / stale / tamper → drop
                                            };
                                            // (6) attribution integrity: the cleartext header's
                                            //     senderHash (VK[0..16], bytes 7..23) must match the
                                            //     authenticated device, so a member can't sign their
                                            //     own frame but mislabel the audio as someone else.
                                            if frame.len() < 23 || frame[7..23] != dev[..16] {
                                                continue;
                                            }
                                            let _ = app_sub.emit(
                                                "voice-frame-received",
                                                serde_json::json!({ "frameBytes": frame }),
                                            );
```

- [ ] **Step 3: Confirm the moderation-only captures are still all used**

`presence_map_media` is now read on every frame (not just under moderation); `mod_map_media`, `voice_now_ms_media`, `mod_active_media` are used in step (4). No capture becomes unused, so no `#[allow(unused)]` churn is needed. If clippy flags an unused capture, that means a step was dropped — re-check.

- [ ] **Step 4: Gate + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo fmt --all
git -C /Users/zeblith/work/zeblithic/harmony-client add src-tauri/src/event_loop.rs
git -C /Users/zeblith/work/zeblithic/harmony-client commit -m "feat(zeb-362): sign community voice on publish; always-verify fail-closed on subscribe

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(voice)'
```
Expected: clippy clean; voice lib tests pass. (Behavioral proof lives in Tasks 2 + 5.)

---

## Task 5: Final gate sweep + push + PR

**Files:** none (verification + delivery).

- [ ] **Step 1: Full-workspace gates (`--all-targets`)**

Run each (commit is already done; kill any single command exceeding 10 min and report):
```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo fmt --all -- --check
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: fmt clean; clippy clean; tests green **except** possibly the 6 known iroh/zenoh loopback flakes (reachability_publisher::force_notify_triggers_publish, zeb_321_connectivity_ipc_tests::force_republish_wakes_publisher, zenoh_iroh_link::paired_stream_roundtrip_via_loopback, two zenoh_iroh_transport tests, community_reachability_two_engine_integration) — those are non-blocking. Any **voice** test failure is real; fix it.

- [ ] **Step 2: Frontend guard (no FE change expected)**

Run:
```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run
```
Expected: both green, unchanged from baseline (this PR touches no frontend).

- [ ] **Step 3: MSRV check**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo check --locked --all-targets --features test-fixtures`
Expected: compiles clean.

- [ ] **Step 4: Push + open PR**

```bash
git -C /Users/zeblith/work/zeblithic/harmony-client push -u origin zeb-362-per-sender-authenticated-voice-media
```
Then open the PR with `gh pr create` — title `ZEB-362: per-sender authenticated voice media (sign community voice frames)`; body summarizing: the v2 encrypt-then-sign envelope, the always-verify fail-closed receiver sequence closing the ZEB-358 moderation-drop evasion, community-only scope, zero frontend change; link the spec + plan; include a test-plan checklist; end with the Claude Code generated-with line.

Expected: PR opens; CI starts. Hand off to the autonomous bot-review loop (do NOT self-merge — Jake's gate; pushover at ready-to-merge; never trigger Greptile).

---

## Self-review

**Spec coverage:** v2 envelope + AAD bump + sig transcript (T1) ✓; always-verify fail-closed receiver sequence incl. moderation-on-authenticated-owner + attribution (T4) ✓; sender signs from `voice_identity` device key (T4 publish) ✓; byte-identity fixture (T3) ✓; spoof/attribution/tamper negative tests (T2) ✓; generic functions kept for presence/moderation (T1 additive) ✓. No spec requirement left unmapped.

**Placeholder scan:** the only deferred value is the fixture hex in T3, which is a deterministic generate-then-pin step (explicit run-and-paste), matching the established `wire_format_voice_fixtures.rs` pattern — not a vague TODO.

**Type consistency:** `seal_and_sign_voice_packet(key:&ChannelKey, device_sk:&SigningKey, community:&SpaceId, channel:&ChannelId, plaintext:&[u8])`, `verify_voice_frame_sig(device_vk:&[u8;32], community:&SpaceId, channel:&ChannelId, packet:&[u8])`, `open_voice_packet(key:&ChannelKey, device_vk:&[u8;32], community:&SpaceId, channel:&ChannelId, packet:&[u8])` — names/signatures identical across T1, T2, T3, T4. `VoiceCryptoError::SigFailed` defined in T1, used in T1/T2. `identity.3` = `Arc<SigningKey>` matches the `voice_identity` map declared at `event_loop.rs:2085`.
