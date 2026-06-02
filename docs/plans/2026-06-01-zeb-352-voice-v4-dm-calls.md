# Voice V4 — 1:1 DM Calls Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship ring/answer 1:1 voice calls on DMs — a signaling state machine (invite/accept/decline/cancel/end/timeout/busy), a non-blocking incoming-call ring toast, a persistent in-call bar, and a "Call" button in the DM header — reusing the V3 media engine for a 2-party room.

**Architecture:** DM calls live inside an existing DM space. The backend is a **dumb relay**: it seals/opens signaling on `harmony/voice-signal/{calleeOwner}` (device-#2-signed, sealed via the existing `dm_signing::seal_to_owner` sealed-box — "the DM key" per spec) and relays media on `harmony/voice/dm/{callId}/{senderDevice}` (symmetric `K_voice = HKDF(DmContentKey, callId)`, sealed exactly like channel voice but DM-scoped AAD). The signaling **state machine + timers** live in a frontend `call-session.ts` that reuses the V3 `VoiceSender`/`VoiceReceiver`/`VoiceMixer` for the 2-party room. No presence beacons (2-party is implicit). No per-call rekey (D3 / non-goals).

**Tech Stack:** Rust (Tauri, Zenoh, ChaCha20-Poly1305, X25519 sealed-box, HKDF-SHA256, ed25519), Svelte 5 + TypeScript (vitest, @testing-library/svelte).

**Linear:** ZEB-352 (parent epic ZEB-348). Spec: `docs/specs/2026-05-31-voice-comms-design.md` §V4. Branch: `zeb-352-voice-v4-dm-calls` (off `873a10c`; carries cherry-picked `4173d7e` — the dropped ZEB-351 `setPttMode` rollback fix — which the PR body must call out so reviewers understand a V3 fix is present).

---

## Crypto / key model (load-bearing — reviewers sanity-check here)

Resolved from the spec + **non-goals** ("no per-call ephemeral rekey / per-call FS beyond existing EpochKey rotation"; D3 "reuse the DM key, no per-call rekey"):

1. **A DM call requires an existing DM space** with the peer. The Call button lives in the DM header, so the frontend always has the `spaceId`; the backend resolves the peer `OwnerAddr` (the non-self entry in `Space.members`), the peer X25519 pubkey (`OwnerDeviceCache` → `OwnerDeviceEntry.device_identity_pubs[i][0..32]`), and the shared `Space.content_key: DmContentKey` from that space. **The frontend never handles owner-id ↔ Reticulum-address bridging.**
2. **Signaling** (`invite`/`accept`/`decline`/`cancel`/`end`): a canonical-CBOR body, **device-#2-signed** via `dm_signing::sign_dm_packet`, then **sealed to the callee** via `dm_signing::seal_to_owner(callee_x25519_pub, signed_bytes)`. Sealed-box gives metadata privacy (no cleartext caller on the wire) + ephemeral FS, and is exactly the mechanism that already delivers `DmContentKey`. Published to `harmony/voice-signal/{calleeOwnerHex}`. The callee opens with `open_from_owner(self_x25519_priv, sealed)`, then verifies the device-#2 signature against the caller's cached identity pub.
3. **Media** (the call audio): symmetric `K_voice = derive_dm_voice_key(DmContentKey, callId)` (HKDF-SHA256, mirror of `derive_channel_key`). Sealed with `encrypt_dm_voice_packet(K_voice, callId, VOICE_DM_PACKET_AAD, frame)` — identical AEAD to channel voice, but the AAD binds `callId` instead of `(community, channel)`. **No per-call rekey:** `K_voice` is *derived*, not minted, so it rotates only when the DM `EpochKey`/`DmContentKey` rotates.
4. **callId** = 16 random bytes (caller-generated), carried as lowercase hex (32 chars, no `/`) in topics + IPC.

## File structure

**Rust (create/modify under `src-tauri/`):**
- `src/community_channel_log.rs` — **modify:** add `derive_dm_voice_key` (next to `derive_channel_key`).
- `src/voice_crypto.rs` — **modify:** add `VOICE_DM_PACKET_AAD`, `scope_aad_dm`, `encrypt_dm_voice_packet`/`decrypt_dm_voice_packet` (+ deterministic-nonce test variant) + in-crate DM tests.
- `src/voice_signal.rs` — **create:** signal types (`VoiceSignal`, `VoiceSignalKind`), `build_sealed_signal` / `open_sealed_signal`, `VoiceSignalRequest` (IPC→event-loop), `VoiceSignalOutKind`.
- `src/voice.rs` — **modify:** `VoiceOutbound` → enum `{ Channel, Dm }`; add DM arms to `VoiceChannelRequest`; add DM IPC payload structs.
- `src/event_loop.rs` — **modify:** DM media arms (outbound/join/leave/mute), DM voice state maps, always-on signal subscription, inbound signal decode→emit, outbound `VoiceSignalRequest` arm.
- `src/lib.rs` — **modify:** signaling IPCs (`place_call`/`accept_call`/`decline_call`/`cancel_call`/`end_call`), DM media IPCs (`join_dm_call`/`leave_dm_call`/`send_dm_voice_frame`/`set_dm_call_muted`), register all; add `voice_signal_tx` to `NodeState`.
- `tests/voice_dm_two_engine_integration.rs` — **create:** DM media round-trip over Zenoh loopback (no presence).
- `tests/wire_format_voice_fixtures.rs` — **modify:** pin a DM voice packet + a sealed signal.

**Frontend (create/modify under `src/`):**
- `src/lib/voice/talk-gate.ts` — **create:** pure VAD/mute/PTT → `{send, ptt}` gate, extracted from `voice-session.ts`; shared by both controllers.
- `src/lib/voice/voice-sender.ts` — **modify:** add optional `publishFrame?` override (additive; default unchanged).
- `src/lib/voice/voice-receiver.ts` — **modify:** add optional `frameEvent?` + `frameFilter?` (additive; defaults unchanged).
- `src/lib/voice-session.ts` — **modify:** use shared `talk-gate.ts` (refactor; all V3 tests stay green).
- `src/lib/call-session.ts` — **create:** `CallSession` controller (signaling state machine + 2-party media).
- `src/lib/components/IncomingCallToast.svelte` — **create:** ring toast.
- `src/lib/components/CallInProgressBar.svelte` — **create:** persistent in-call bar.
- `src/lib/components/TextFeed.svelte` (DM header) — **modify:** add "Call" button for DM scope.
- `src/App.svelte` — **modify:** build `CallSession`, mount toast + in-call bar globally, one-active-session coordinator, `incoming-call` listener.
- Test files alongside each (`*.test.ts` / `__tests__/*.test.ts`).

---

## Task 0: Pre-flight baseline

**Files:** none (verification only).

- [ ] **Step 1: Confirm branch + cherry-picked fix.**

Run: `git -C /Users/zeblith/work/zeblithic/harmony-client log --oneline -2`
Expected: HEAD `4173d7e fix(zeb-351): roll back pttMode if coupled setMuted fails` on `873a10c`.

- [ ] **Step 2: Backend baseline green.**

Run (from `src-tauri/`): `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(voice)'`
Expected: PASS (existing voice unit/integration tests). The 6 known iroh/zenoh loopback flakes are NOT in this filter.

- [ ] **Step 3: Frontend baseline green.**

Run (repo root): `npx tsc --noEmit && npx vitest run src/lib/voice-session.test.ts src/lib/components/__tests__/VoiceChannelView.test.ts`
Expected: tsc clean; voice-session 14/14, VoiceChannelView 13/13 (incl. the cherry-picked `setPttMode rolls back` test).

- [ ] **Step 4: Commit nothing.** Baseline only.

---

## Task 1: DM voice AEAD seam + key derivation

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs` (add `derive_dm_voice_key` after `derive_channel_key`, ~line 80)
- Modify: `src-tauri/src/voice_crypto.rs` (add DM AAD + scope + encrypt/decrypt + det. variant + tests)

- [ ] **Step 1: Write the failing key-derivation test** in `community_channel_log.rs` `#[cfg(test)]`:

```rust
#[test]
fn dm_voice_key_is_deterministic_and_call_scoped() {
    let dm = DmContentKey::from_bytes([7u8; 32]);
    let call_a = [1u8; 16];
    let call_b = [2u8; 16];
    let k_a1 = derive_dm_voice_key(&dm, &call_a);
    let k_a2 = derive_dm_voice_key(&dm, &call_a);
    let k_b = derive_dm_voice_key(&dm, &call_b);
    assert_eq!(k_a1.as_bytes(), k_a2.as_bytes(), "same (key, callId) → same subkey");
    assert_ne!(k_a1.as_bytes(), k_b.as_bytes(), "distinct callId → distinct subkey");
}
```

> If `DmContentKey` lacks a test ctor, use the existing one (grep `DmContentKey::` in this module's tests — `dm_content_key_round_trip` constructs one). Match that idiom rather than adding `from_bytes` if it does not already exist.

- [ ] **Step 2: Run it; expect FAIL** (`derive_dm_voice_key` not defined).

Run: `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(dm_voice_key_is_deterministic)'`

- [ ] **Step 3: Implement `derive_dm_voice_key`** after `derive_channel_key` (add `use crate::owner_state_types::DmContentKey;` to the imports):

```rust
/// HKDF-SHA256 derivation of a per-call DM voice key from the DM space's
/// `DmContentKey`. Mirrors `derive_channel_key`: any party holding the DM
/// content key derives the same per-call subkey from the (caller-generated)
/// `call_id`, with no out-of-band coordination and no per-call rekey (D3 /
/// V4 non-goals). Salt = `call_id` (per-call scope); Info = `b"voice-dm:"`.
pub fn derive_dm_voice_key(dm_key: &DmContentKey, call_id: &[u8; 16]) -> ChannelKey {
    let mut out = zeroize::Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(&call_id[..]), dm_key.as_bytes())
        .expand(b"voice-dm:", out.as_mut())
        .expect("32 ≤ 8160");
    ChannelKey(*out)
}
```

- [ ] **Step 4: Run it; expect PASS.**

- [ ] **Step 5: Write failing DM-AEAD tests** in `voice_crypto.rs` `#[cfg(test)] mod dm_tests` (mirror the existing channel tests):

```rust
#[cfg(test)]
mod dm_tests {
    use super::*;
    use crate::community_channel_log::derive_dm_voice_key;
    use crate::owner_state_types::DmContentKey;

    fn key(call: &[u8; 16]) -> ChannelKey {
        derive_dm_voice_key(&DmContentKey::from_bytes([9u8; 32]), call)
    }

    #[test]
    fn dm_round_trip() {
        let call = [3u8; 16];
        let k = key(&call);
        let frame = (0u8..30).collect::<Vec<_>>();
        let sealed = encrypt_dm_voice_packet(&k, &call, VOICE_DM_PACKET_AAD, &frame).unwrap();
        let opened = decrypt_dm_voice_packet(&k, &call, VOICE_DM_PACKET_AAD, &sealed).unwrap();
        assert_eq!(opened, frame);
    }

    #[test]
    fn dm_wrong_call_id_drops() {
        let k = key(&[3u8; 16]);
        let frame = (0u8..30).collect::<Vec<_>>();
        let sealed = encrypt_dm_voice_packet(&k, &[3u8; 16], VOICE_DM_PACKET_AAD, &frame).unwrap();
        // Same key bytes but a different call_id in the AAD must fail to open.
        assert!(decrypt_dm_voice_packet(&k, &[4u8; 16], VOICE_DM_PACKET_AAD, &sealed).is_err());
    }

    #[test]
    fn dm_wrong_domain_drops() {
        let call = [3u8; 16];
        let k = key(&call);
        let frame = (0u8..30).collect::<Vec<_>>();
        let sealed = encrypt_dm_voice_packet(&k, &call, VOICE_DM_PACKET_AAD, &frame).unwrap();
        assert!(decrypt_dm_voice_packet(&k, &call, VOICE_PACKET_AAD, &sealed).is_err());
    }
}
```

- [ ] **Step 6: Run; expect FAIL** (`encrypt_dm_voice_packet` undefined).

- [ ] **Step 7: Implement the DM AEAD** in `voice_crypto.rs`. Add the constant near the existing domain separators:

```rust
/// Domain separator for sealed DM-call voice media packets.
pub const VOICE_DM_PACKET_AAD: &[u8] = b"harmony-voice-dm-pkt-v1";
```

Add the DM scope-AAD next to `scope_aad`:

```rust
/// AAD = domain ‖ call_id (16B). Binds every sealed DM-call packet to its
/// domain and `call_id`, so a packet from one call can't open in another.
fn scope_aad_dm(domain: &[u8], call_id: &[u8; 16]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(domain.len() + 16);
    aad.extend_from_slice(domain);
    aad.extend_from_slice(&call_id[..]);
    aad
}
```

Add the seal/open + deterministic test variant (mirror `encrypt_voice_packet`/`seal_inner`/`decrypt_voice_packet`):

```rust
/// Seal `plaintext` under `key` for `call_id` with a random nonce.
/// Output: `[12B nonce][ChaCha20-Poly1305 ciphertext+tag]`.
pub fn encrypt_dm_voice_packet(
    key: &ChannelKey,
    call_id: &[u8; 16],
    domain: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, VoiceCryptoError> {
    use chacha20poly1305::aead::OsRng;
    use chacha20poly1305::AeadCore;
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let nonce_bytes: [u8; NONCE_LEN] = nonce.into();
    seal_inner_dm(key, call_id, domain, plaintext, nonce_bytes)
}

fn seal_inner_dm(
    key: &ChannelKey,
    call_id: &[u8; 16],
    domain: &[u8],
    plaintext: &[u8],
    nonce_bytes: [u8; NONCE_LEN],
) -> Result<Vec<u8>, VoiceCryptoError> {
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let aad = scope_aad_dm(domain, call_id);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), Payload { msg: plaintext, aad: &aad })
        .map_err(|_| VoiceCryptoError::SealFailed)?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a packet sealed by [`encrypt_dm_voice_packet`]. Any failure → caller drops.
pub fn decrypt_dm_voice_packet(
    key: &ChannelKey,
    call_id: &[u8; 16],
    domain: &[u8],
    packet: &[u8],
) -> Result<Vec<u8>, VoiceCryptoError> {
    if packet.len() < MIN_PACKET_LEN {
        return Err(VoiceCryptoError::TooShort(packet.len()));
    }
    let (nonce_bytes, ct) = packet.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let aad = scope_aad_dm(domain, call_id);
    cipher
        .decrypt(Nonce::from_slice(nonce_bytes), Payload { msg: ct, aad: &aad })
        .map_err(|_| VoiceCryptoError::OpenFailed)
}

#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn encrypt_dm_voice_packet_with_nonce(
    key: &ChannelKey,
    call_id: &[u8; 16],
    domain: &[u8],
    plaintext: &[u8],
    nonce: [u8; NONCE_LEN],
) -> Result<Vec<u8>, VoiceCryptoError> {
    seal_inner_dm(key, call_id, domain, plaintext, nonce)
}
```

> `ChannelKey::as_bytes()` is `pub(crate)`; `voice_crypto` is in-crate, so this compiles. If `voice_crypto` currently imports `ChannelKey`, no new import is needed.

- [ ] **Step 8: Run all of Task 1's tests; expect PASS.**

Run: `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(dm_round_trip) + test(dm_wrong) + test(dm_voice_key)'`

- [ ] **Step 9: fmt + clippy, then commit.**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
git add -A && git commit -m "feat(zeb-352): DM voice AEAD seam + derive_dm_voice_key

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `voice_signal.rs` — signal types, sign+seal, open+verify

**Files:**
- Create: `src-tauri/src/voice_signal.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod voice_signal;` near the other `mod voice*;` declarations)

- [ ] **Step 1: Declare the module.** In `lib.rs`, next to `mod voice_crypto;` / `mod voice_presence;`, add `mod voice_signal;`.

- [ ] **Step 2: Write the signal types + builder/opener** in `voice_signal.rs`:

```rust
//! ZEB-352 Voice V4: 1:1 DM-call signaling.
//!
//! A signaling packet is a canonical-CBOR `VoiceSignal`, device-#2-signed
//! (`dm_signing::sign_dm_packet`) and sealed to the callee via
//! `dm_signing::seal_to_owner` (the "DM key" sealed-box). The wire payload on
//! `harmony/voice-signal/{calleeOwner}` is just the sealed bytes — no cleartext
//! caller. The state machine + timers live in the frontend; this module is the
//! sign/seal/open/verify seam only.

use crate::dm_signing::{self, DmReceiveError, DmSignError};
use crate::owner_state_types::OwnerAddr;
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};

/// The signaling verbs (spec §V4). `reason` is only meaningful for `Decline`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceSignalKind {
    Invite,
    Accept,
    Decline,
    Cancel,
    End,
}

/// Why a call was declined (carried on `Decline`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclineReason {
    User,
    Busy,
    Timeout,
}

/// The signed inner body. `caller` lets the callee resolve which DM space /
/// identity to verify against once the sealed envelope is opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceSignal {
    #[serde(rename = "k")]
    pub kind: VoiceSignalKind,
    #[serde(rename = "ci", serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr", deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr")]
    pub call_id: [u8; 16],
    /// Caller owner address (so the callee picks the right DM space to verify).
    #[serde(rename = "cl")]
    pub caller: OwnerAddr,
    #[serde(rename = "dr", default, skip_serializing_if = "Option::is_none")]
    pub decline_reason: Option<DeclineReason>,
}

/// Signed wrapper: canonical-CBOR(VoiceSignal) + a device-#2 Ed25519 signature
/// over those exact bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedVoiceSignal {
    #[serde(rename = "bd")]
    pub body: VoiceSignal,
    #[serde(rename = "sg", serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr", deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr")]
    pub sig: [u8; 64],
}

/// Sign `signal` with `signing_key` (device #2), then seal to the callee's
/// X25519 pubkey. Returns the wire bytes for `harmony/voice-signal/{callee}`.
pub fn build_sealed_signal(
    signal: &VoiceSignal,
    signing_key: &SigningKey,
    callee_x25519_pub: &[u8; 32],
) -> Result<Vec<u8>, DmSignError> {
    let body_bytes = canonical_cbor(signal);
    let sig = dm_signing::sign_dm_packet(&body_bytes, signing_key);
    let signed = SignedVoiceSignal { body: signal.clone(), sig };
    let signed_bytes = canonical_cbor(&signed);
    dm_signing::seal_to_owner(callee_x25519_pub, &signed_bytes)
}

/// Open + verify an inbound sealed signal. Steps: (1) `open_from_owner` with our
/// X25519 priv; (2) decode `SignedVoiceSignal`; (3) verify the device-#2 sig
/// against the caller's cached identity pub (defeats key substitution). Returns
/// the verified `VoiceSignal` on success.
pub fn open_sealed_signal(
    self_x25519_priv: &[u8; 32],
    sealed: &[u8],
    caller_identity_pub: &[u8; 64],
    caller_signing_device_hash: dm_signing::DeviceIdentityHash,
) -> Result<VoiceSignal, DmReceiveError> {
    let signed_bytes = dm_signing::open_from_owner(self_x25519_priv, sealed)
        .map_err(|_| DmReceiveError::DecryptionFailed)?;
    let signed: SignedVoiceSignal =
        decode_cbor(&signed_bytes).map_err(|_| DmReceiveError::MalformedPacket)?;
    let body_bytes = canonical_cbor(&signed.body);
    dm_signing::verify_dm_packet_signature(
        &body_bytes,
        &signed.sig,
        caller_identity_pub,
        caller_signing_device_hash,
    )?;
    Ok(signed.body)
}

fn canonical_cbor<T: Serialize>(v: &T) -> Vec<u8> {
    let mut buf = Vec::new();
    ciborium::into_writer(v, &mut buf).expect("CBOR serialize");
    buf
}

fn decode_cbor<T: for<'de> Deserialize<'de>>(b: &[u8]) -> Result<T, ()> {
    ciborium::from_reader(b).map_err(|_| ())
}
```

> Verify the exact names of `DmReceiveError` variants (`MalformedPacket`, `DecryptionFailed`) and the bstr serde helper names by grepping `dm_signing.rs` / `owner_state_types.rs`; adjust to the real identifiers. The bstr helpers are the same ones `community_membership`/`voice_presence` use.

- [ ] **Step 3: Write tests** (`#[cfg(test)]` in `voice_signal.rs`). Build a deterministic identity via the existing test helper (`owner_state_types` tests use `(device_hash, identity_pub)` from a `PrivateIdentity` — mirror `owner_device_entry_*` test setup):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    // Use the same identity-construction helper the owner_state_types tests use
    // to get (signing_key, identity_pub[64], device_hash) + an X25519 keypair.

    #[test]
    fn sign_seal_open_round_trip() { /* build_sealed_signal → open_sealed_signal == original */ }

    #[test]
    fn wrong_recipient_cannot_open() { /* open with a different X25519 priv → Err */ }

    #[test]
    fn tampered_body_fails_signature() { /* flip a byte in the sealed bytes → Err */ }

    #[test]
    fn wrong_caller_identity_fails_verify() { /* verify against a different identity_pub → Err */ }
}
```

Fill each test with concrete construction mirroring `dm_signing.rs`'s own round-trip tests (grep `seal_to_owner` test usage). Do not leave them as comments — the spec-reviewer will reject stubs.

- [ ] **Step 4: Run; expect PASS.** `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(voice_signal)'`

- [ ] **Step 5: fmt + clippy + commit** (`feat(zeb-352): voice signaling sign/seal/open seam`).

---

## Task 3: `voice.rs` — DM scope on outbound/request types + IPC payloads

**Files:**
- Modify: `src-tauri/src/voice.rs`
- Modify: `src-tauri/src/event_loop.rs` (the outbound voice arm's `VoiceOutbound` construction — update to the new `Channel` variant)
- Modify: `src-tauri/src/lib.rs` (`send_voice_frame` IPC — construct `VoiceOutbound::Channel`)

- [ ] **Step 1: Convert `VoiceOutbound` to an enum** (was a struct):

```rust
/// An outbound voice frame from the frontend, ready to seal + publish.
#[derive(Debug)]
pub enum VoiceOutbound {
    Channel {
        community_id: SpaceId,
        channel_id: ChannelId,
        frame: Vec<u8>,
    },
    /// ZEB-352 Voice V4: a frame for a 1:1 DM call, sealed under the
    /// per-call `K_voice` and published to `harmony/voice/dm/{callId}/{own}`.
    Dm {
        call_id: [u8; 16],
        frame: Vec<u8>,
    },
}
```

- [ ] **Step 2: Add DM arms to `VoiceChannelRequest`:**

```rust
    /// ZEB-352: join a 1:1 DM call's 2-party room. `caps.channel_key` carries
    /// the derived `K_voice`; presence is implicit (no beacons).
    JoinDmCall {
        call_id: [u8; 16],
        caps: VoiceJoinCaps,
    },
    LeaveDmCall {
        call_id: [u8; 16],
    },
    SetDmCallMuted {
        call_id: [u8; 16],
        muted: bool,
    },
```

- [ ] **Step 3: Add DM IPC payload structs** (camelCase boundary):

```rust
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendDmVoiceFramePayload {
    pub call_id: String,
    pub frame_bytes: Vec<u8>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetDmCallMutedPayload {
    pub call_id: String,
    pub muted: bool,
}
```

- [ ] **Step 4: Update the two existing `VoiceOutbound` construction sites** to `VoiceOutbound::Channel { .. }`:
  - `lib.rs` `send_voice_frame` (~11591): construct `VoiceOutbound::Channel { community_id, channel_id, frame: payload.frame_bytes }`.
  - `event_loop.rs` outbound arm (~2410): change `Some(voice) = voice_rx.recv()` to `match voice { VoiceOutbound::Channel { community_id, channel_id, frame } => { <existing body, referencing the destructured locals> } VoiceOutbound::Dm { .. } => { /* added in Task 4 */ } }`.

- [ ] **Step 5: Build the crate** (expect it to compile; this task is a type change with no new behavior path yet — the `Dm` arm is a temporary `{}`/`todo!`-free no-op that simply drops, to be filled in Task 4):

Run: `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(voice)'`
Expected: PASS (existing channel-voice behavior unchanged).

- [ ] **Step 6: fmt + clippy + commit** (`refactor(zeb-352): VoiceOutbound enum + DM request/payload types`).

---

## Task 4: event_loop.rs — DM media arms (outbound / join / leave / mute)

**Files:**
- Modify: `src-tauri/src/event_loop.rs`

- [ ] **Step 1: Add DM voice state maps** next to the channel maps (~1697):

```rust
let mut dm_voice_subs: std::collections::HashMap<[u8; 16], JoinHandle<()>> = HashMap::new();
let mut dm_voice_keys: std::collections::HashMap<[u8; 16], Arc<ChannelKey>> = HashMap::new();
let mut dm_voice_mute_flags: std::collections::HashMap<[u8; 16], Arc<AtomicBool>> = HashMap::new();
```

(DM calls need **no** presence subs/pubs/identity maps — 2-party is implicit.)

- [ ] **Step 2: Fill the outbound `VoiceOutbound::Dm` arm** (from Task 3 Step 4):

```rust
VoiceOutbound::Dm { call_id, frame } => {
    if let Some(key) = dm_voice_keys.get(&call_id) {
        let own = voice_own_device.as_deref().unwrap_or("self");
        match crate::voice_crypto::encrypt_dm_voice_packet(
            key,
            &call_id,
            crate::voice_crypto::VOICE_DM_PACKET_AAD,
            &frame,
        ) {
            Ok(sealed) => {
                let key_expr =
                    format!("harmony/voice/dm/{}/{}", hex::encode(call_id), own);
                if let Err(e) = session.put(&key_expr, sealed).await {
                    tracing::warn!(%key_expr, err = %e, "dm voice publish failed");
                }
            }
            Err(e) => tracing::warn!(err = %e, "dm voice seal failed; dropping frame"),
        }
    }
    // else: not joined to that call — drop.
}
```

- [ ] **Step 3: Add the `JoinDmCall` arm** to the `VoiceChannelRequest` match (mirror the channel `Join` subscriber spawn, but emit `dm-voice-frame-received` with `callId`, and **no** presence pub/sub):

```rust
crate::voice::VoiceChannelRequest::JoinDmCall { call_id, caps } => {
    let sub_key = format!("harmony/voice/dm/{}/*", hex::encode(call_id));
    let key_for_sub = std::sync::Arc::clone(&caps.channel_key);
    let app_sub = app.clone();
    let closing_sub = closing.clone();
    let call_hex = hex::encode(call_id);
    let call_id_aad = call_id;
    if voice_own_device.is_none() {
        voice_own_device = Some(hex::encode(caps.self_device));
    }
    match session.declare_subscriber(&sub_key).await {
        Ok(sub) => {
            let handle = tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    if sample.payload().len() > crate::voice_crypto::MAX_VOICE_PACKET_BYTES {
                        continue;
                    }
                    let sealed = sample.payload().to_bytes().to_vec();
                    match crate::voice_crypto::decrypt_dm_voice_packet(
                        &key_for_sub,
                        &call_id_aad,
                        crate::voice_crypto::VOICE_DM_PACKET_AAD,
                        &sealed,
                    ) {
                        Ok(frame) => {
                            let _ = app_sub.emit(
                                "dm-voice-frame-received",
                                serde_json::json!({ "callId": call_hex, "frameBytes": frame }),
                            );
                        }
                        Err(_) => { /* wrong call / tamper → drop */ }
                    }
                }
                if !closing_sub.load(std::sync::atomic::Ordering::SeqCst) {
                    tracing::warn!("dm voice subscriber closed unexpectedly");
                }
            });
            // State only after the subscriber is live (no split-brain).
            dm_voice_keys.insert(call_id, caps.channel_key);
            dm_voice_mute_flags.insert(call_id, Arc::new(AtomicBool::new(true)));
            dm_voice_subs.insert(call_id, handle);
        }
        Err(e) => tracing::warn!(err = %e, "dm voice subscribe failed"),
    }
}
```

- [ ] **Step 4: Add `LeaveDmCall` + `SetDmCallMuted` arms:**

```rust
crate::voice::VoiceChannelRequest::LeaveDmCall { call_id } => {
    dm_voice_mute_flags.remove(&call_id);
    dm_voice_keys.remove(&call_id);
    if let Some(handle) = dm_voice_subs.remove(&call_id) {
        handle.abort();
    }
}
crate::voice::VoiceChannelRequest::SetDmCallMuted { call_id, muted } => {
    if let Some(flag) = dm_voice_mute_flags.get(&call_id) {
        flag.store(muted, std::sync::atomic::Ordering::SeqCst);
    }
}
```

> The DM `mute_flags` map is currently write-only (no presence publisher reads it). It exists so the symmetry with channels holds and a future DM-presence/visual-mute hook has a home; clippy won't complain (it's read via `.get` in `SetDmCallMuted`). If clippy flags it as unused, gate behind the actual read or drop it and track mute purely client-side — decide at implementation time, do not leave dead code.

- [ ] **Step 5: Build + run voice tests; expect PASS** (no integration coverage of the DM path yet — that's Task 7). `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(voice)'`

- [ ] **Step 6: fmt + clippy + commit** (`feat(zeb-352): DM-call media relay arms in event loop`).

---

## Task 5: event_loop.rs — signaling subscription + relay

**Files:**
- Modify: `src-tauri/src/event_loop.rs`
- Modify: `src-tauri/src/lib.rs` (`NodeState.voice_signal_tx` + `start_node` channel wiring; thread `voice_signal_rx` into `event_loop::run`)

- [ ] **Step 1: Add the request channel.** In `lib.rs` `NodeState`, next to `voice_channel_tx`, add:

```rust
voice_signal_tx: Option<tokio::sync::mpsc::Sender<voice_signal::VoiceSignalRequest>>,
```

Initialize `None` in `NodeState::new`. In `start_node` (where `voice_channel_tx`/`voice_tx` channels are created), create `let (voice_signal_tx, voice_signal_rx) = tokio::sync::mpsc::channel(64);`, store `voice_signal_tx` on the state, and pass `voice_signal_rx` into `event_loop::run` (add the param — follow how `voice_channel_rx` is threaded).

- [ ] **Step 2: Define the outbound request type** in `voice_signal.rs`:

```rust
/// IPC → event-loop: a fully-resolved outbound signal ready to publish.
/// The IPC boundary resolves the callee owner + X25519 pubkey + signing key
/// + sealed bytes (so `event_loop::run` needs no new state handles).
#[derive(Debug)]
pub struct VoiceSignalRequest {
    /// Hex of the callee owner addr — names the `harmony/voice-signal/{}` topic.
    pub callee_owner_hex: String,
    /// The sealed wire bytes from `build_sealed_signal`.
    pub sealed: Vec<u8>,
}
```

- [ ] **Step 3: Add the outbound relay arm** to the `select!` (next to the voice-channel arm):

```rust
Some(req) = voice_signal_rx.recv() => {
    let key_expr = format!("harmony/voice-signal/{}", req.callee_owner_hex);
    if let Err(e) = session.put(&key_expr, req.sealed).await {
        tracing::warn!(%key_expr, err = %e, "voice signal publish failed");
    }
}
```

- [ ] **Step 4: Add the always-on inbound subscription.** Near the mail/own-topic subscriptions (~1601, gated on owner identity loaded), subscribe to our own signaling topic and decode inbound signals. Resolve our X25519 priv (from the same identity handle the presence/DM paths use — grep how `dm_outbox` exposes the X25519 private / how `ed25519_priv_to_x25519` is obtained) and, per inbound sample, resolve the caller's cached identity pub from `OwnerDeviceCache`:

```rust
// ZEB-352: subscribe our own voice-signal topic so we can be rung while online.
if let Some(self_owner_hex) = voice_signal_self_owner.as_deref() {
    let sig_topic = format!("harmony/voice-signal/{self_owner_hex}");
    let app_sig = app.clone();
    let x25519_priv = voice_signal_self_x25519_priv.clone(); // Zeroizing<[u8;32]>
    let dev_cache = std::sync::Arc::clone(&owner_device_cache); // for caller identity lookup
    match session.declare_subscriber(&sig_topic).await {
        Ok(sub) => {
            let h = tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let sealed = sample.payload().to_bytes().to_vec();
                    // Open with our X25519 priv; the opened body carries `caller`,
                    // which we use to look up the caller's identity pub + device
                    // hash for signature verification.
                    if let Some(signal) = try_open_voice_signal(&x25519_priv, &sealed, &dev_cache).await {
                        emit_voice_signal_event(&app_sig, &signal);
                    }
                    // open/verify failure → drop silently
                }
            });
            // keep `h` in a Vec with the other always-on subscriber handles
        }
        Err(e) => tracing::warn!(err = %e, "voice-signal subscribe failed"),
    }
}
```

Implement two small helpers in `event_loop.rs` (or `voice_signal.rs`):
  - `try_open_voice_signal`: first `open_from_owner` to get the signed bytes, decode the `SignedVoiceSignal` to read `body.caller`, look up that owner in the device cache to get `identity_pub[64]` + `DeviceIdentityHash`, then call `voice_signal::open_sealed_signal` for the full verified result. (Two-pass: peek caller from the decoded-but-unverified body to pick the verifying key, then verify. This is safe — verification still binds the signature to that identity.)
  - `emit_voice_signal_event`: map `VoiceSignalKind` → a frontend event name + payload:
    - `Invite` → `"incoming-call"` `{ callId, callerOwner }`
    - `Accept` → `"call-accepted"` `{ callId }`
    - `Decline` → `"call-declined"` `{ callId, reason }`
    - `Cancel` → `"call-canceled"` `{ callId }`
    - `End` → `"call-ended"` `{ callId }`

> `voice_signal_self_owner`, `voice_signal_self_x25519_priv`, and `owner_device_cache` are snapshotted at the top of `run` from the same handles the DM/presence paths already use. Grep `ed25519_priv_to_x25519` and the presence path's identity snapshot (`voice_own_device`/`self_device`) to source them; do NOT invent new state.

- [ ] **Step 5: Build + run; expect PASS** (signaling decode is covered by Task 2 unit tests + Task 7 integration). `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(voice)'`

- [ ] **Step 6: fmt + clippy + commit** (`feat(zeb-352): always-on voice-signal subscription + relay`).

---

## Task 6: lib.rs — signaling + DM media IPCs

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Add a DM-space resolver helper** (private fn in `lib.rs`) that, given a `space_id` hex, returns `(peer_owner: OwnerAddr, peer_x25519_pub: [u8;32], dm_content_key: DmContentKey, self_signing_key: Arc<SigningKey>)`. It:
  - parses `space_id` → `SpaceId`;
  - finds the `Space` in `OwnerState.spaces`; asserts `content_key.is_some()` (DM/group-dm) else `Err("not a DM space")`;
  - picks the single non-self member from `space.members` (1:1) → `peer_owner`;
  - looks up `peer_owner` in `OwnerDeviceCache.devices` → first `Some(identity_pub[64])` in `device_identity_pubs` → `peer_x25519_pub = identity_pub[0..32]`; `Err("peer key unknown")` if absent;
  - `self_signing_key` = `dm_outbox.lock().await.community_signing_key.clone()` (device #2).

Write it concretely against the structs confirmed in exploration (`Space` @ owner_state_types.rs:1461; `OwnerDeviceEntry.device_identity_pubs` @ :494). Snapshot `NodeState` under the std `Mutex`, drop the lock before any `.await` (the `send_dm` pattern @ lib.rs:6006).

- [ ] **Step 2: `place_call` IPC** (returns the generated `callId` hex):

```rust
#[tauri::command]
async fn place_call(
    space_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<String, String> {
    let (voice_signal_tx, resolver_handles) = { /* snapshot voice_signal_tx + handles */ };
    let (peer_owner, peer_x25519, _dm_key, signing_key) =
        resolve_dm_call_peer(&resolver_handles, &space_id).await?;
    // callId = 16 random bytes (OsRng).
    let mut call_id = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut call_id);
    let self_owner = /* snapshot self OwnerAddr */;
    let signal = voice_signal::VoiceSignal {
        kind: voice_signal::VoiceSignalKind::Invite,
        call_id,
        caller: self_owner,
        decline_reason: None,
    };
    let sealed = voice_signal::build_sealed_signal(&signal, &signing_key, &peer_x25519)
        .map_err(|e| format!("seal: {e}"))?;
    voice_signal_tx
        .send(voice_signal::VoiceSignalRequest {
            callee_owner_hex: hex::encode(peer_owner.0),
            sealed,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    Ok(hex::encode(call_id))
}
```

- [ ] **Step 3: `accept_call` / `decline_call` / `cancel_call` / `end_call` IPCs.** Each takes `{ call_id: String, space_id: String }` (decline also `reason: String`), parses `call_id` hex → `[u8;16]`, re-resolves the peer via the helper, builds the matching `VoiceSignal` (`Accept`/`Decline{reason}`/`Cancel`/`End`), seals, and sends a `VoiceSignalRequest`. Mirror `place_call` exactly; for `decline_call` map `"busy"|"timeout"|_ → DeclineReason`.

- [ ] **Step 4: DM media IPCs:**

```rust
#[tauri::command]
async fn join_dm_call(
    call_id: String,
    space_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let call = parse_call_id(&call_id)?;                    // hex → [u8;16]
    let (tx, resolver_handles, self_owner, device_hex, self_device, hlc_tracker) =
        { /* snapshot voice_channel_tx + the join handles, like join_voice_channel */ };
    let (_peer, _x, dm_key, signing_key) =
        resolve_dm_call_peer(&resolver_handles, &space_id).await?;
    let k_voice = crate::community_channel_log::derive_dm_voice_key(&dm_key, &call);
    let wall_now_ms = /* SystemTime now ms */;
    let joined_hlc = reserve_next_hlc_for_device(&hlc_tracker, &device_hex, wall_now_ms).await;
    tx.send(voice::VoiceChannelRequest::JoinDmCall {
        call_id: call,
        caps: voice::VoiceJoinCaps {
            channel_key: std::sync::Arc::new(k_voice),
            signing_key,
            self_owner,
            self_device,
            joined_hlc,
        },
    })
    .await
    .map_err(|_| "event loop not running".to_string())
}
```

Plus `leave_dm_call(call_id)` → `VoiceChannelRequest::LeaveDmCall`, `send_dm_voice_frame(payload: SendDmVoiceFramePayload)` → `VoiceOutbound::Dm`, `set_dm_call_muted(payload: SetDmCallMutedPayload)` → `VoiceChannelRequest::SetDmCallMuted`. Mirror `leave_voice_channel`/`send_voice_frame`/`set_voice_muted` (lib.rs:11708/11591/11738). Add `fn parse_call_id(hex: &str) -> Result<[u8;16], String>` (16-byte hex, mirror `parse_voice_id_16`).

- [ ] **Step 5: Register all 9 commands** in the `generate_handler!` list (lib.rs:~32384, next to the existing voice commands): `place_call, accept_call, decline_call, cancel_call, end_call, join_dm_call, leave_dm_call, send_dm_voice_frame, set_dm_call_muted`. If `add_dm_ipc_handlers` (test harness, lib.rs:~32541) is used by an integration test that needs these, add them there too.

- [ ] **Step 6: Build + run; expect PASS.** `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(voice)'`

- [ ] **Step 7: fmt + clippy + commit** (`feat(zeb-352): DM-call signaling + media IPCs`).

---

## Task 7: Integration test + wire fixtures

**Files:**
- Create: `src-tauri/tests/voice_dm_two_engine_integration.rs`
- Modify: `src-tauri/tests/wire_format_voice_fixtures.rs`

- [ ] **Step 1: Two-engine DM media round-trip** (mirror `voice_presence_two_engine_integration.rs` but with NO presence/membership seeding). Two Zenoh sessions on loopback; A seals a frame under `K_voice` and `session.put("harmony/voice/dm/{callId}/{deviceA}")`; B subscribes `harmony/voice/dm/{callId}/*`, opens with the same `K_voice`, asserts the plaintext round-trips; assert a wrong-`callId` key fails to open. Use `wait_until(timeout, …)`. This belongs to the known transport-flake class (passes on CI; may flake on a loopback-restricted local box) — note that in a comment, do not gate CI on it.

- [ ] **Step 2: Signal sealed round-trip integration** (can be a plain unit-style test, no Zenoh): build a sealed `Invite` from A to B with A's signing key + B's X25519 pub, `open_sealed_signal` on B → assert kind/call_id/caller match; assert opening with C's key fails.

- [ ] **Step 3: Wire-format fixtures** in `wire_format_voice_fixtures.rs`:
  - `dm_voice_packet_wire_bytes_pinned`: `encrypt_dm_voice_packet_with_nonce(&K_voice, &call_id, VOICE_DM_PACKET_AAD, &frame, [0u8;12])` → assert hex equals a pinned constant (compute it once from a green run, paste it in).
  - `voice_signal_invite_wire_bytes_pinned`: pin the canonical-CBOR of a fixed `SignedVoiceSignal` body (deterministic, no sealing — sealing uses ephemeral randomness so it can't be byte-pinned; pin the **signed inner** bytes + assert the signature verifies).

- [ ] **Step 4: Run** `cargo nextest run --locked -p harmony-app --all-targets --features test-fixtures -E 'test(voice_dm) + test(dm_voice_packet_wire) + test(voice_signal)'`; expect PASS.

- [ ] **Step 5: commit** (`test(zeb-352): DM media two-engine + signal round-trip + wire fixtures`).

---

## Task 8: Frontend — shared talk-gate + `CallSession` controller

**Files:**
- Create: `src/lib/voice/talk-gate.ts` + `src/lib/voice/talk-gate.test.ts`
- Modify: `src/lib/voice-session.ts` (use the shared gate; tests stay green)
- Modify: `src/lib/voice/voice-sender.ts` (additive `publishFrame?`)
- Modify: `src/lib/voice/voice-receiver.ts` (additive `frameEvent?` + `frameFilter?`)
- Create: `src/lib/call-session.ts` + `src/lib/call-session.test.ts`

- [ ] **Step 1: Extract the pure gate.** Write `talk-gate.ts` exposing `makeTalkGate(opts)` returning `(pcm) => {send, ptt}` plus the VAD instance, lifting the exact VAD/mute/PTT/hangover logic currently inside `voice-session.ts`. Add `talk-gate.test.ts` covering: muted→no send; open-mic VAD energy + hangover→silence; PTT ignores VAD and follows hold. (Move the equivalent assertions out of `voice-session.test.ts` only if duplicated; keep voice-session's behavior tests intact.)

- [ ] **Step 2: Refactor `voice-session.ts`** to consume `makeTalkGate`. Run `npx vitest run src/lib/voice-session.test.ts` → expect 14/14 still green (no behavior change).

- [ ] **Step 3: Additive sender/receiver hooks.**
  - `voice-sender.ts`: add `publishFrame?: (frameBytes: number[]) => Promise<unknown>` to `VoiceSenderConfig`; in the send path, call `this.config.publishFrame ? this.config.publishFrame(frameBytes) : this.config.invoke('send_voice_frame', { communityId, channelId, frameBytes })`. Default behavior identical.
  - `voice-receiver.ts`: add `frameEvent?: string` (default `'voice-frame-received'`) and `frameFilter?: (payload: unknown) => number[] | null` (default extracts `payload.frameBytes`). `init()` listens `frameEvent`; per event, `const f = (this.config.frameFilter ?? defaultFilter)(payload); if (f) this.handleFrame(f)`.
  - Run existing voice tests → green.

- [ ] **Step 4: Write `call-session.test.ts` first** (mirror `voice-session.test.ts` deps/factory harness). Cover the state machine:

```ts
it('place: idle → ringingOut, invokes place_call, returns callId', async () => { … });
it('incoming invite → incoming state; accept invokes accept_call + join_dm_call, connects muted', async () => { … });
it('decline on incoming invokes decline_call(user) and returns to idle', async () => { … });
it('ring timeout (30s) auto-declines incoming with reason=timeout', async () => { /* fake timers */ });
it('caller cancel before answer invokes cancel_call and resets', async () => { … });
it('remote call-accepted (caller side) → connecting → active + join_dm_call', async () => { … });
it('remote call-ended tears down the media session and resets to idle', async () => { … });
it('busy: an incoming invite while active auto-declines with reason=busy', async () => { … });
it('mute/PTT/deafen delegate to the shared media core (start muted)', async () => { … });
```

- [ ] **Step 5: Implement `CallSession`.** A Svelte-store-backed controller with `CallSessionState { phase: 'idle'|'ringingOut'|'incoming'|'connecting'|'active'|'ended'; callId: string|null; peerOwnerHex: string|null; muted; pttMode; pttHeld; deafened; startedAt: number|null }`. Public API: `placeCall(spaceId)`, `onIncoming(callId, callerOwnerHex)`, `accept(spaceId)`, `decline(reason)`, `cancel()`, `end()`, plus `setMuted/setPttMode/setPttHeld/setDeafened`. Internally it builds a media core (VoiceSender with `publishFrame: (f) => invoke('send_dm_voice_frame', { callId, frameBytes: f })`, VoiceReceiver with `frameEvent: 'dm-voice-frame-received'` + `frameFilter: (p) => p.callId === this.callId ? p.frameBytes : null`, VoiceMixer) on connect, using `makeTalkGate`. It owns the 30s ring timer and the busy guard (an incoming invite while `phase !== 'idle'` → immediate `decline('busy')`). Connect = `invoke('join_dm_call', { callId, spaceId })` + start media **muted** (D10). Listens (via `deps.listen`, wired by App) are delivered through `onIncoming`/`onRemote*` methods the App calls — keep the class transport-injected like `VoiceSession`. Export `getCallSession(deps)` singleton.

- [ ] **Step 6: Run** `npx vitest run src/lib/call-session.test.ts src/lib/voice-session.test.ts` → all green. Commit (`feat(zeb-352): CallSession signaling state machine + shared talk-gate`).

---

## Task 9: `IncomingCallToast.svelte`

**Files:**
- Create: `src/lib/components/IncomingCallToast.svelte` + `src/lib/components/__tests__/IncomingCallToast.test.ts`

- [ ] **Step 1: Write the test first** (vitest + @testing-library/svelte): renders caller name + avatar; ✓ click calls `onAccept(callId)`; ✗ click calls `onDecline(callId)`; renders nothing when `incomingCall` is null.

- [ ] **Step 2: Implement** the component (mirror `Toast.svelte` styling + a fly transition; non-blocking, bottom-center; avatar + "Incoming call" + ✓/✗ buttons). Props: `{ incomingCall: { callId, callerName, callerAvatarUrl? } | null, onAccept, onDecline }`. Use `data-testid="incoming-call"` and accessible button labels (`aria-label="Accept call"` / `"Decline call"`).

- [ ] **Step 3: Run; green. Commit** (`feat(zeb-352): incoming-call ring toast`).

---

## Task 10: `CallInProgressBar.svelte`

**Files:**
- Create: `src/lib/components/CallInProgressBar.svelte` + `src/lib/components/__tests__/CallInProgressBar.test.ts`

- [ ] **Step 1: Test first:** shows peer label + a running timer when `phase==='active'|'connecting'`; hidden when `idle`; Mute toggle calls `session.setMuted`; PTT toggle calls `setPttMode`; the hold control drives `setPttHeld` (reuse the `VoiceChannelView` control-bar pattern incl. the `{#if pttMode}` hold button); Leave calls `onEnd`.

- [ ] **Step 2: Implement** — a fixed bottom bar (`z-index: 50`, below modals/toasts), reusing `VoiceChannelView`'s control-bar markup (Mute/PTT/hold/Leave) + a `mm:ss` timer derived from `startedAt`. Props `{ session: CallSession | null, onEnd }`. Auto-subscribe `session.state` via a non-rune alias.

- [ ] **Step 3: Run; green. Commit** (`feat(zeb-352): persistent in-call bar`).

---

## Task 11: "Call" button in the DM header

**Files:**
- Modify: `src/lib/components/TextFeed.svelte` (the DM conversation header) + its test (`src/lib/components/__tests__/TextFeed.test.ts` if present; else add focused coverage)

- [ ] **Step 1: Confirm the header host.** Grep `channelType === 'dm'` / `channelName` in `TextFeed.svelte` to find the header. (If the DM header is a different component, add the button there — follow the actual structure, do not assume.)

- [ ] **Step 2: Test first:** when `channelType === 'dm'`, a "Call" button (`aria-label="Start call"`) renders and clicking it calls the injected `onStartCall(spaceId)`; for non-DM channels it does not render.

- [ ] **Step 3: Implement** — add an `onStartCall?: (spaceId: string) => void` prop and render the button only for DM scope, passing the active DM `spaceId` (the channel/space id `TextFeed` already has).

- [ ] **Step 4: Run; green. Commit** (`feat(zeb-352): DM-header Call button`).

---

## Task 12: App.svelte wiring + one-active-session coordinator

**Files:**
- Modify: `src/App.svelte`

- [ ] **Step 1: Build the `CallSession`** alongside `buildVoiceSession` (reuse the same `get_self_voice_identity` result + adapter `invoke`/`listen`). Store `let callSession = $state<CallSession | null>(null)`.

- [ ] **Step 2: Wire signaling listeners** (in the Tauri-init block): `listen('incoming-call', …)` → if `callSession` is busy, it auto-declines (busy) internally; else set `incomingCall` state + hand `onIncoming(callId, callerOwner)` to the session; resolve the caller's card (`resolveCard`) for the toast name/avatar. `listen('call-accepted'|'call-declined'|'call-canceled'|'call-ended', …)` → forward to the corresponding `callSession` method. Clear `incomingCall` on accept/decline/timeout.

- [ ] **Step 3: Mount globally** after `<ToastHost />`: `{#if callSession}<IncomingCallToast incomingCall={incomingCall} onAccept={…} onDecline={…} />{/if}` and `{#if callSession}<CallInProgressBar session={callSession} onEnd={() => callSession?.end()} />{/if}`.

- [ ] **Step 4: One-active-session coordinator (D12).** Add a tiny guard: starting a channel voice join while a DM call is active first `await callSession.end()`, and `callSession.placeCall/accept` first `await voiceSession.leave()` if a channel session is connected. Implement by checking each store's `phase` before the other's join. Keep it minimal and centralized (a `leaveAnyActiveVoice()` helper called from both entry points).

- [ ] **Step 5: Wire the DM-header Call button** to `callSession.placeCall(spaceId)` (thread `onStartCall` down to `TextFeed`).

- [ ] **Step 6: Run** `npx tsc --noEmit && npx vitest run` (full FE suite) → green. Commit (`feat(zeb-352): app wiring — call session, ring toast, in-call bar, one-session coordinator`).

---

## Task 13: Final gate sweep + push + PR

**Files:** none (verification + delivery).

- [ ] **Step 1: Full backend gates.**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: fmt clean; clippy 0; nextest green except the 6 known iroh/zenoh loopback flakes (reachability_publisher::force_notify_triggers_publish, zeb_321_connectivity_ipc_tests::force_republish_wakes_publisher, zenoh_iroh_link::paired_stream_roundtrip_via_loopback, two zenoh_iroh_transport tests, community_reachability_two_engine_integration::two_engines_exchange_via_iroh_zenoh) + possibly `voice_dm_two_engine_integration` if loopback is restricted locally. Treat ONLY those as known; any other failure is real.

- [ ] **Step 2: MSRV gate.** `cargo check --locked --all-targets --features test-fixtures`.

- [ ] **Step 3: Frontend gates.** `npx tsc --noEmit && npx vitest run` → green.

- [ ] **Step 4: Push + open PR.**

```bash
git push -u origin zeb-352-voice-v4-dm-calls
gh pr create --repo zeblithic/harmony-client \
  --title "ZEB-352 Voice V4: 1:1 DM calls (signaling state machine + ring toast + in-call bar)" \
  --body "<see body below>"
```

PR body must include: spec §V4 reference; the crypto/key model summary (sealed-box signaling + HKDF-derived per-call media key, no per-call rekey); the new Zenoh topics; a test-plan checklist (fmt/clippy/nextest/MSRV/frontend + the two-engine DM media proof); **an explicit note that the branch also carries the dropped ZEB-351 `setPttMode` rollback fix (`4173d7e`)** so reviewers understand why a V3 voice-session fix appears in a V4 PR; and the known-flake list.

- [ ] **Step 5: Enter the autonomous bot-review loop** (CodeRabbit/Cursor/CodeAnt/Qodo + the 5 CI jobs). NEVER trigger Greptile. Do NOT merge (Jake's gate). Pushover at ready-to-merge.

---

## Self-review notes

- **Spec coverage:** signaling state machine (T2/T5/T6/T8) · invite/accept/decline/cancel/end/timeout/busy (T8 state machine + T6 IPCs) · sealed+signed signals (T2) · ring toast 30s (T8/T9) · in-call bar D9 (T10) · DM Call button (T11) · reuse V3 controller for 2-party (T8) · start muted D10 (T8 connect) · one active session D12 (T12) · tests at every layer (T1/T2/T7/T8–T11). All §V4 bullets map to a task.
- **Type consistency:** `call_id` is `[u8;16]` in Rust, lowercase hex `string` across the IPC boundary (`parse_call_id`/`hex::encode`). `K_voice` is a `ChannelKey` everywhere. Frontend event names: `incoming-call`, `call-accepted`, `call-declined`, `call-canceled`, `call-ended`, `dm-voice-frame-received` — used identically in T5 (emit) and T8/T12 (listen).
- **No-presence invariant:** DM arms (T4) touch only `dm_voice_*` maps; the presence sweep + beacon pub/sub stay channel-only. Reviewer gate: no presence call inside any `*DmCall*` arm.
- **Risk flags for implementers:** (a) verify real identifiers in `dm_signing.rs` (`DmReceiveError` variants, `seal_to_owner`/`open_from_owner` arities) and the bstr serde helper names before relying on the snippets; (b) source the event-loop's self-X25519-priv + `OwnerDeviceCache` snapshot from existing handles, never new state; (c) the `dm_voice_mute_flags` map must be genuinely read or dropped — no dead code; (d) sealed signals can't be byte-pinned (ephemeral nonce) — pin the signed *inner* bytes instead.
