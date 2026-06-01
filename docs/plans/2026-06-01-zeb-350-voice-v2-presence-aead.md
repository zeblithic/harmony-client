# Voice V2 — Presence + AEAD Seam Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Seal community voice packets and presence beacons end-to-end under the existing channel `ChannelKey`, move sender routing into the Zenoh topic, and add an ephemeral signed+sealed presence-beacon system (4 s heartbeat / 12 s eviction) that maintains a live per-channel roster — proving "live roster + sealed relay" with no mic capture in comms yet.

**Architecture:** The three voice IPCs and `voice.rs` types gain `(community, channel)` scope. The `join_voice_channel` IPC resolves the channel's `Arc<ChannelKey>` (via the channel-log registry), the device-#2 signing key (via `DmOutbox`), and the node's own owner/device id, and threads them into an enriched `VoiceChannelRequest::Join` — so `event_loop::run`'s 40-parameter signature is left untouched. The event-loop voice arm seals outbound frames with a new `encrypt_voice_packet` wrapper and publishes to `harmony/voice/{community}/{channel}/{ownDevice}`; the subscriber opens inbound and drops on AEAD failure. A new `voice_presence` module owns the beacon type (signed by device #2, sealed under `ChannelKey` with a distinct AAD), the in-memory roster map with heartbeat/eviction, and the publisher/subscriber spawn helpers. Beacon receivers verify the signer against materialized community membership (the ZEB-339 norm).

**Tech Stack:** Rust (Tauri backend), `ChaCha20-Poly1305` + HKDF-SHA256 (existing channel AEAD), `ed25519-dalek` (device-#2 signatures), `ciborium` canonical CBOR, Zenoh pub/sub, `tokio`, Svelte 5 (minimal frontend IPC-shape updates), `cargo-nextest` + `vitest`.

---

## Background context (read before starting)

These facts were verified against the current tree (branch `zeb-350-voice-presence-aead` off `origin/main` `81ecdc7`). Line numbers drift as you edit — re-grep before trusting an offset.

**The three voice IPCs** live in `src-tauri/src/lib.rs`:
- `send_voice_frame` (≈ lines 11594–11614) — takes `payload: voice::SendVoiceFramePayload`, sends `voice::VoiceOutbound { channel_id, frame }` over `NodeState.voice_tx`.
- `join_voice_channel` (≈ 11616–11632) — takes `channel_id: String`, sends `voice::VoiceChannelRequest::Join { channel_id }` over `NodeState.voice_channel_tx`.
- `leave_voice_channel` (≈ 11634–11650) — symmetric, sends `Leave { channel_id }`.
- `validate_voice_channel_id` (≈ 11580–11592) rejects empty / `/ * ? # $`.
- Registered in `tauri::generate_handler!` (≈ line 32223): `send_voice_frame, join_voice_channel, leave_voice_channel`. **Names are unchanged by V2** — only signatures change, so the handler list is untouched.

**`src-tauri/src/voice.rs`** (entire file, ~30 lines):
```rust
#[derive(Debug)]
pub struct VoiceOutbound {
    pub channel_id: String,
    pub frame: Vec<u8>,
}

#[derive(Debug)]
pub enum VoiceChannelRequest {
    Join { channel_id: String },
    Leave { channel_id: String },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendVoiceFramePayload {
    pub channel_id: String,
    pub frame_bytes: Vec<u8>,
}
```

**Event-loop voice arms** in `src-tauri/src/event_loop.rs`:
- Outbound relay (≈ 2313–2324):
  ```rust
  Some(voice) = voice_rx.recv() => {
      if voice.frame.len() >= 23 {
          let node_addr = hex::encode(&voice.frame[7..23]);
          let key_expr = format!("harmony/voice/{}/{}", voice.channel_id, node_addr);
          if let Err(e) = session.put(&key_expr, voice.frame).await {
              tracing::warn!(%key_expr, err = %e, "voice publish failed");
          }
      }
  }
  ```
- Join/Leave subscriber arm (≈ 2326–2361): on `Join`, `declare_subscriber("harmony/voice/{channel_id}/*")`, spawn a task that loops `sub.recv_async()` → `sample.payload().to_bytes().to_vec()` → `app.emit("voice-frame-received", json!({ "frameBytes": payload }))`; stores the `JoinHandle` in `voice_subs: HashMap<String, JoinHandle<()>>`. On `Leave`, `voice_subs.remove(&channel_id).abort()`. On shutdown, `voice_subs.drain()` aborts all.
- `voice_subs` is declared ≈ line 1697; `closing: Arc<AtomicBool>` is the shutdown flag used by subscriber tasks.

**Channel AEAD** in `src-tauri/src/community_channel_log.rs`:
- `pub struct ChannelKey([u8; 32])` (≈ 39–54), `ZeroizeOnDrop`, redacting `Debug`. `pub(crate) fn as_bytes(&self) -> &[u8; 32]`.
- `pub const CHANNEL_PACKET_AAD: &[u8] = b"harmony-channel-msg-v1";` (≈ 103).
- `NONCE_LEN = 12`, `TAG_LEN = 16`. Wire layout `[12B nonce][ChaCha20-Poly1305(plaintext, AAD)]`.
- `encrypt_channel_packet(&ChannelKey, &SignedChannelEvent)` / `decrypt_channel_packet(&ChannelKey, &[u8])` operate on `SignedChannelEvent` (CBOR), **not raw bytes** — V2 needs new raw-byte wrappers.
- `pub fn derive_channel_key(mk: &EpochKey, community_id: &SpaceId, channel_id: &ChannelId) -> ChannelKey` (≈ 66–80).

**`ChannelKey` resolution:** `ChannelLogRegistry::engine(&SpaceId, &ChannelId).await -> Option<Arc<ChannelLogEngine<R>>>` (`community_channel_log_engine.rs` ≈ 1726). The engine holds `channel_key: Arc<ChannelKey>` (≈ 299); the only accessor today is `pub(crate) fn channel_key_ref(&self) -> &ChannelKey` (≈ 1007). `reconcile_from_state` (≈ 1782) spawns an engine for **every** non-deleted channel regardless of `ChannelKind`, so voice channels have a registry engine + key. The registry is on `NodeState.channel_log_registry: Option<Arc<ChannelLogRegistry<tauri::Wry>>>` (≈ 532).

**Device-#2 signing key:** built in `start_node` as `community_signing_key_arc: Arc<ed25519_dalek::SigningKey>` (lib.rs ≈ 2619) and stored on `DmOutbox.community_signing_key`. IPC handlers read it via `outbox_g.community_signing_key` (the `create_channel`/`modify_channel`/`delete_channel` IPCs do this at ≈ 13287/13524/13750). `NodeState.dm_device_id: Option<String>` (hex of the ed25519 verifying key, ≈ 458) and `NodeState.dm_self_owner: Option<OwnerAddr>` (≈ 459) hold the node's own device id and owner address.

**Membership for beacon verification:** `event_loop::run` already receives `community_registry: Option<Arc<crate::community_state_sync::CommunitySyncRegistry>>`. `CommunitySyncRegistry::engine_arc(&SpaceId).await -> Option<Arc<CommunitySyncEngine>>` (community_state_sync.rs ≈ 4400). `CommunitySyncEngine::admin_addr() -> OwnerAddr` (≈ 1129) and `CommunitySyncEngine::state() -> Arc<Mutex<CommunityState>>` (≈ 1107). `CommunityState::materialized(admin_addr)` returns a cached `MaterializedMembership` (caching keyed by `cached_admin_addr`, community_state_crdt.rs ≈ 122). `MaterializedMembership.members: BTreeMap<OwnerAddr, MemberState>` where `MemberState.enrolled_device_keys: BTreeSet<[u8; 32]>` (community_membership.rs ≈ 1388) and `MemberState.status: MemberStatus` (the `Joined` variant gates active members). **Confirm `materialized`'s exact signature (`&self` vs `&mut self`, owned vs borrowed return) in Task 7 before wiring.**

**HLC:** `Hlc { wall_ms, logical, device_id }` (owner_state_types.rs ≈ 235), **no `Ord`** — order by `is_strictly_newer_than`. `reserve_next_hlc_for_device(&Arc<Mutex<BTreeMap<String, Hlc>>>, device_id, wall_now_ms).await -> Hlc` (dm_outbox.rs ≈ 2152). `wall_now_ms` from `SystemTime::now().duration_since(UNIX_EPOCH)`. **V2 orders beacons by a monotonic `seq: u64` per publisher, not by HLC** — `joined_hlc` is carried as identifying metadata only.

**Zenoh idioms (must match exactly):** publish `session.put(&key_expr, bytes).await`; subscribe `session.declare_subscriber(&key_expr).await` then in a spawned task `while let Ok(sample) = sub.recv_async().await { let bytes = sample.payload().to_bytes().to_vec(); … }` (use `.to_bytes().to_vec()`, **never** `.contiguous()`); wildcard `*` = one segment, `**` = multi. Store the `JoinHandle`, `.abort()` on leave/shutdown.

**Two-engine integration template:** `src-tauri/tests/community_channel_messages_integration.rs` — `fixture_identity(seed) -> (SigningKey, OwnerAddr, [u8;64])`, two `zenoh::open(Config::default())` sessions on the same default router, `tauri::test::mock_app()` per side, an mpsc "adapter bridge drainer" task, `derive_channel_key(&membership_key, &community_id, &channel_id)`, and a `wait_until` / `wait_for_stable_count` poller. Mirror this for the presence two-engine test.

**Frontend:** `src/lib/voice/voice-sender.ts` calls `invoke('send_voice_frame', { payload: { channelId, frameBytes } })`; `src/lib/voice/voice-receiver.ts` listens on `voice-frame-received` and reads `{ frameBytes }`, decoding the 23-byte header (sender hash at bytes 7..23) for per-sender demux. **The 23-byte packet header is unchanged by V2** — the whole frame (header + payload) is sealed as opaque bytes; the receiver still sees the decrypted header. `join_voice_channel`/`leave_voice_channel` have no FE callers yet; `voice-presence-changed` does not exist yet. V2's frontend work is limited to threading `communityId` into the `send_voice_frame` payload shape + types (the roster UI is V3).

---

## File structure

**New files:**
- `src-tauri/src/voice_crypto.rs` — the AEAD seam: `VOICE_PACKET_AAD`, `VOICE_PRESENCE_AAD`, `encrypt_voice_packet` / `decrypt_voice_packet` (+ test-fixtures deterministic-nonce variants), `VoiceCryptoError`. One responsibility: raw-byte seal/open under a `ChannelKey` with a domain+scope-bound AAD.
- `src-tauri/src/voice_presence.rs` — the presence layer: `VoicePresenceBeacon`, `SignedVoicePresenceBeacon`, `sign_presence_beacon` / `verify_presence_beacon_sig`, `VoicePresenceMap` (roster + heartbeat/eviction), `PresenceEntry` / `RosterEntry`, and the `spawn_voice_presence_publisher` / `spawn_voice_presence_subscriber` helpers + a membership `BeaconVerifier`.
- `src-tauri/tests/wire_format_voice_fixtures.rs` — pins the sealed voice-packet bytes and the signed+sealed beacon bytes (deterministic nonce, test-fixtures gated).
- `src-tauri/tests/voice_presence_two_engine_integration.rs` — two-engine presence-exchange + sealed-relay integration test.

**Modified files:**
- `src-tauri/src/voice.rs` — `community_id` + carried capabilities on the IPC channel types.
- `src-tauri/src/lib.rs` — register the two new modules; rework the three voice IPCs to `(community, channel)` + capability resolution; `validate_voice_community_id`.
- `src-tauri/src/community_channel_log_engine.rs` — add `pub(crate) fn channel_key_arc(&self) -> Arc<ChannelKey>`.
- `src-tauri/src/event_loop.rs` — rework the voice outbound/Join/Leave arms (seal/open, topic, presence publisher+subscriber, sweep), key/own-device cache.
- `src/lib/voice/voice-sender.ts` + its test — thread `communityId` into the `send_voice_frame` payload.

---

## Task 0: Pre-flight baseline

**Files:** none (verification only).

- [ ] **Step 1: Confirm branch + clean tree**

Run: `git -C /Users/zeblith/work/zeblithic/harmony-client status -sb | head -3 && git -C /Users/zeblith/work/zeblithic/harmony-client log --oneline -1 origin/main`
Expected: on `zeb-350-voice-presence-aead`, tree clean, `origin/main` at `81ecdc7` (ZEB-349 #174).

- [ ] **Step 2: Backend baseline**

Run (commit any local change first so a 10-min wall-clock kill leaves a clean tree):
```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && \
cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -40
```
Expected: PASS except the known 6 iroh/zenoh transport orphan-flakes (loopback 45 s deadline; they pass on CI real-network runners). Record their exact names. **Do not "fix" them in this PR** (per the unrelated-test-failures rule). Use `${PIPESTATUS[0]}` / `set -o pipefail` — never trust `| tail` exit codes.

- [ ] **Step 3: Frontend baseline**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit && npx vitest run 2>&1 | tail -20`
Expected: tsc clean; vitest all green.

---

## Task 1: Voice AEAD seam (`voice_crypto.rs`)

**Files:**
- Create: `src-tauri/src/voice_crypto.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod voice_crypto;` next to the other `mod` declarations)

The seal is a raw-byte AEAD over `ChannelKey` with AAD = `domain ‖ community_id(16B) ‖ channel_id(16B)`, layout `[12B nonce][ChaCha20-Poly1305 ciphertext+tag]` — identical framing to `encrypt_channel_packet`, but operating on `&[u8]` instead of a `SignedChannelEvent`, and with a distinct domain so a channel-text packet can never open as voice (or vice-versa), and a voice packet from channel X can never open under channel Y.

- [ ] **Step 1: Write the failing tests**

Create `src-tauri/src/voice_crypto.rs` with the test module first:
```rust
//! ZEB-350 Voice V2: raw-byte AEAD seam for voice packets and presence
//! beacons. Thin wrappers over ChaCha20-Poly1305 keyed by the channel
//! `ChannelKey` (the same key that seals channel text — voice inherits the
//! existing E2E channel encryption). A distinct per-domain, per-scope AAD
//! prevents cross-domain replay (a text packet replayed as voice) and
//! cross-channel replay (channel X's packet opened under channel Y).

use crate::community_channel_log::ChannelKey;
use crate::owner_state_types::{ChannelId, SpaceId};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};

/// Domain separator for sealed voice media packets.
pub const VOICE_PACKET_AAD: &[u8] = b"harmony-voice-pkt-v1";
/// Domain separator for sealed presence beacons.
pub const VOICE_PRESENCE_AAD: &[u8] = b"harmony-voice-presence-v1";

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const MIN_PACKET_LEN: usize = NONCE_LEN + TAG_LEN; // empty plaintext still carries a tag

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VoiceCryptoError {
    #[error("voice packet too short ({0} bytes)")]
    TooShort(usize),
    #[error("voice AEAD seal failed")]
    SealFailed,
    #[error("voice AEAD open failed (wrong key / wrong scope / tampered)")]
    OpenFailed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_channel_log::derive_channel_key;
    use crate::owner_state_types::EpochKey;

    fn key() -> ChannelKey {
        derive_channel_key(&EpochKey::new([0x11; 32]), &SpaceId([0xc0; 16]), &ChannelId([0xc1; 16]))
    }
    const C: SpaceId = SpaceId([0xc0; 16]);
    const CH: ChannelId = ChannelId([0xc1; 16]);

    #[test]
    fn round_trip_voice_packet() {
        let k = key();
        let plain = b"opus-frame-bytes-1234567890".to_vec();
        let sealed = encrypt_voice_packet(&k, &C, &CH, VOICE_PACKET_AAD, &plain).unwrap();
        assert_ne!(sealed, plain);
        assert!(sealed.len() >= MIN_PACKET_LEN);
        let opened = decrypt_voice_packet(&k, &C, &CH, VOICE_PACKET_AAD, &sealed).unwrap();
        assert_eq!(opened, plain);
    }

    #[test]
    fn wrong_key_drops() {
        let sealed = encrypt_voice_packet(&key(), &C, &CH, VOICE_PACKET_AAD, b"x").unwrap();
        let other = derive_channel_key(&EpochKey::new([0x22; 32]), &C, &CH);
        assert_eq!(
            decrypt_voice_packet(&other, &C, &CH, VOICE_PACKET_AAD, &sealed),
            Err(VoiceCryptoError::OpenFailed)
        );
    }

    #[test]
    fn wrong_scope_drops() {
        let k = key();
        let sealed = encrypt_voice_packet(&k, &C, &CH, VOICE_PACKET_AAD, b"x").unwrap();
        // same key, different channel id in the AAD → must not open
        let other_ch = ChannelId([0xc2; 16]);
        assert_eq!(
            decrypt_voice_packet(&k, &C, &other_ch, VOICE_PACKET_AAD, &sealed),
            Err(VoiceCryptoError::OpenFailed)
        );
    }

    #[test]
    fn wrong_domain_drops() {
        let k = key();
        let sealed = encrypt_voice_packet(&k, &C, &CH, VOICE_PACKET_AAD, b"x").unwrap();
        // a media packet must not open as a presence beacon
        assert_eq!(
            decrypt_voice_packet(&k, &C, &CH, VOICE_PRESENCE_AAD, &sealed),
            Err(VoiceCryptoError::OpenFailed)
        );
    }

    #[test]
    fn truncated_drops() {
        assert_eq!(decrypt_voice_packet(&key(), &C, &CH, VOICE_PACKET_AAD, b"short"),
                   Err(VoiceCryptoError::TooShort(5)));
    }

    #[test]
    fn deterministic_nonce_variant_is_stable() {
        let k = key();
        let a = encrypt_voice_packet_with_nonce(&k, &C, &CH, VOICE_PACKET_AAD, b"hello", [7u8; 12]).unwrap();
        let b = encrypt_voice_packet_with_nonce(&k, &C, &CH, VOICE_PACKET_AAD, b"hello", [7u8; 12]).unwrap();
        assert_eq!(a, b);
        assert_eq!(&a[..NONCE_LEN], &[7u8; 12]);
    }
}
```

- [ ] **Step 2: Run tests, confirm they fail to compile (functions missing)**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(voice_crypto)'`
Expected: compile error — `encrypt_voice_packet` / `decrypt_voice_packet` not found.

- [ ] **Step 3: Implement the seam**

Add to `voice_crypto.rs` (above the test module). Note `ChannelKey::as_bytes()` is `pub(crate)`, reachable from this sibling module:
```rust
/// AAD = domain ‖ community_id (16B) ‖ channel_id (16B). Binds every sealed
/// packet to its domain and (community, channel) scope.
fn scope_aad(domain: &[u8], community: &SpaceId, channel: &ChannelId) -> Vec<u8> {
    let mut aad = Vec::with_capacity(domain.len() + 32);
    aad.extend_from_slice(domain);
    aad.extend_from_slice(&community.0);
    aad.extend_from_slice(&channel.0);
    aad
}

/// Seal `plaintext` under `key` for `(community, channel)` with a random nonce.
/// Output: `[12B nonce][ChaCha20-Poly1305 ciphertext+tag]`.
pub fn encrypt_voice_packet(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    domain: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, VoiceCryptoError> {
    use chacha20poly1305::aead::OsRng;
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let nonce_bytes: [u8; NONCE_LEN] = nonce.into();
    encrypt_voice_packet_with_nonce(key, community, channel, domain, plaintext, nonce_bytes)
}

fn seal_inner(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    domain: &[u8],
    plaintext: &[u8],
    nonce_bytes: [u8; NONCE_LEN],
) -> Result<Vec<u8>, VoiceCryptoError> {
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let aad = scope_aad(domain, community, channel);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), Payload { msg: plaintext, aad: &aad })
        .map_err(|_| VoiceCryptoError::SealFailed)?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a packet sealed by [`encrypt_voice_packet`]. Any failure (wrong key,
/// wrong scope, wrong domain, tamper, truncation) returns an error — callers
/// drop silently.
pub fn decrypt_voice_packet(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    domain: &[u8],
    packet: &[u8],
) -> Result<Vec<u8>, VoiceCryptoError> {
    if packet.len() < MIN_PACKET_LEN {
        return Err(VoiceCryptoError::TooShort(packet.len()));
    }
    let (nonce_bytes, ct) = packet.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let aad = scope_aad(domain, community, channel);
    cipher
        .decrypt(Nonce::from_slice(nonce_bytes), Payload { msg: ct, aad: &aad })
        .map_err(|_| VoiceCryptoError::OpenFailed)
}

/// Deterministic-nonce variant for wire-format fixtures. NEVER call from
/// production — a fixed nonce with a reused key is catastrophic nonce reuse.
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn encrypt_voice_packet_with_nonce(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    domain: &[u8],
    plaintext: &[u8],
    nonce: [u8; NONCE_LEN],
) -> Result<Vec<u8>, VoiceCryptoError> {
    seal_inner(key, community, channel, domain, plaintext, nonce)
}
```
Then route the production `encrypt_voice_packet` through `seal_inner` (it already does via `encrypt_voice_packet_with_nonce` under the gate — but production must not depend on a test-gated fn). Restructure so production calls `seal_inner` directly:
- In `encrypt_voice_packet`, replace the call to `encrypt_voice_packet_with_nonce` with `seal_inner(key, community, channel, domain, plaintext, nonce_bytes)`.
- Keep `encrypt_voice_packet_with_nonce` as the gated thin shim over `seal_inner` for fixtures.

Confirm the `chacha20poly1305` and `thiserror` imports/deps already exist (they do — `community_channel_log.rs` uses both). Add `mod voice_crypto;` to `lib.rs` beside the existing module declarations (search for `mod voice;` and add after it).

- [ ] **Step 4: Run tests, confirm pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(voice_crypto)'`
Expected: 6 tests pass.

- [ ] **Step 5: fmt + clippy + commit**

Run: `cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -15`
Then:
```bash
git add src-tauri/src/voice_crypto.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-350): voice AEAD seam — encrypt/decrypt_voice_packet over ChannelKey

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Voice-packet wire-format fixture

**Files:**
- Create: `src-tauri/tests/wire_format_voice_fixtures.rs`

Pin the sealed voice-packet bytes so a future change to the AEAD framing or AAD construction is caught. Uses the deterministic-nonce variant (test-fixtures gated).

- [ ] **Step 1: Write the pinning test (self-bootstrapping)**

Create `src-tauri/tests/wire_format_voice_fixtures.rs`:
```rust
//! ZEB-350 Voice V2 wire-format pins. Locks the sealed voice-packet framing
//! (and, in Task 5, the signed+sealed presence beacon). A drift here means
//! the on-the-wire format changed — bump the version domain and re-pin
//! deliberately, never silently.
#![cfg(feature = "test-fixtures")]

use harmony_app::owner_state_types::{ChannelId, EpochKey, SpaceId};
use harmony_app::community_channel_log::derive_channel_key;
use harmony_app::voice_crypto::{encrypt_voice_packet_with_nonce, VOICE_PACKET_AAD};

#[test]
fn voice_packet_wire_bytes_pinned() {
    let key = derive_channel_key(&EpochKey::new([0x11; 32]), &SpaceId([0xc0; 16]), &ChannelId([0xc1; 16]));
    // 23-byte header (flags|seq|ts|senderHash) + a short opus payload, all zeros
    // except markers — the relay seals the whole frame opaquely.
    let frame: Vec<u8> = (0u8..30).collect();
    let sealed = encrypt_voice_packet_with_nonce(
        &key, &SpaceId([0xc0; 16]), &ChannelId([0xc1; 16]), VOICE_PACKET_AAD, &frame, [0u8; 12],
    )
    .expect("seal");
    let actual = hex::encode(&sealed);
    if std::env::var("UPDATE_VOICE_FIXTURE").is_ok() {
        eprintln!("UPDATE_VOICE_FIXTURE voice_packet: {actual}");
    }
    let expected = "PASTE_FROM_BOOTSTRAP_RUN";
    assert_eq!(actual, expected, "sealed voice-packet wire format drifted");
}
```

- [ ] **Step 2: Bootstrap the expected hex**

Run: `cd src-tauri && UPDATE_VOICE_FIXTURE=1 cargo nextest run --locked --features test-fixtures -E 'test(voice_packet_wire_bytes_pinned)' --no-capture 2>&1 | grep UPDATE_VOICE_FIXTURE`
Copy the printed hex into `expected` (replace `PASTE_FROM_BOOTSTRAP_RUN`). **Remove the `eprintln!`/`UPDATE_VOICE_FIXTURE` block after pinning** (leftover debug prints are a known review nit).

- [ ] **Step 3: Confirm the pin holds + commit**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(voice_packet_wire_bytes_pinned)'`
Expected: PASS. Then `cargo fmt --all` and:
```bash
git add src-tauri/tests/wire_format_voice_fixtures.rs
git commit -m "test(zeb-350): pin sealed voice-packet wire bytes

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `voice.rs` types + IPC rework to `(community, channel)`

**Files:**
- Modify: `src-tauri/src/voice.rs`
- Modify: `src-tauri/src/community_channel_log_engine.rs` (add `channel_key_arc`)
- Modify: `src-tauri/src/lib.rs` (three IPCs + `validate_voice_community_id`)
- Test: inline `#[cfg(test)]` in `lib.rs` for validation; IPC capability resolution is covered by Task 8 integration.

The `Join` request is **enriched** with the resolved capabilities so the event loop needs no new `run` parameters. Media-relay frames still flow through `VoiceOutbound` (now scoped); the key is resolved once on Join and cached in the loop.

- [ ] **Step 1: Add the `channel_key_arc` accessor**

In `src-tauri/src/community_channel_log_engine.rs`, beside `channel_key_ref` (≈ 1007):
```rust
/// ZEB-350: clone the `Arc<ChannelKey>` so the voice relay can hold the key
/// for the lifetime of a join without borrowing the engine.
pub(crate) fn channel_key_arc(&self) -> std::sync::Arc<ChannelKey> {
    std::sync::Arc::clone(&self.channel_key)
}
```

- [ ] **Step 2: Rework `voice.rs` types**

Replace the three types in `src-tauri/src/voice.rs`:
```rust
use serde::Deserialize;
use std::sync::Arc;
use crate::community_channel_log::ChannelKey;
use crate::owner_state_types::{ChannelId, OwnerAddr, SpaceId};

#[derive(Debug)]
pub struct VoiceOutbound {
    pub community_id: SpaceId,
    pub channel_id: ChannelId,
    pub frame: Vec<u8>,
}

/// Capabilities resolved at the IPC boundary (which holds `NodeState`) and
/// carried into the event loop on join — so `event_loop::run` needs no new
/// parameters. The signing key + own identity drive the presence publisher;
/// the channel key seals/open both media and beacons.
#[derive(Debug)]
pub struct VoiceJoinCaps {
    pub channel_key: Arc<ChannelKey>,
    pub signing_key: Arc<ed25519_dalek::SigningKey>,
    pub self_owner: OwnerAddr,
    /// 32-byte ed25519 verifying key of this device (device #2).
    pub self_device: [u8; 32],
}

#[derive(Debug)]
pub enum VoiceChannelRequest {
    Join {
        community_id: SpaceId,
        channel_id: ChannelId,
        caps: VoiceJoinCaps,
    },
    Leave {
        community_id: SpaceId,
        channel_id: ChannelId,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendVoiceFramePayload {
    pub community_id: String,
    pub channel_id: String,
    pub frame_bytes: Vec<u8>,
}
```
(`VoiceJoinCaps` derives `Debug`; `SigningKey` and `ChannelKey` both have non-leaking `Debug`.)

- [ ] **Step 3: Add `validate_voice_community_id` + parsing helper in `lib.rs`**

Beside `validate_voice_channel_id` (≈ 11580):
```rust
/// Parse a hex community/channel id (32 hex chars → 16 bytes). Rejects bad
/// length or non-hex. Reused by all three voice IPCs.
fn parse_voice_id_16(label: &str, s: &str) -> Result<[u8; 16], String> {
    let bytes = hex::decode(s).map_err(|_| format!("{label} not hex"))?;
    <[u8; 16]>::try_from(bytes.as_slice()).map_err(|_| format!("{label} must be 16 bytes"))
}
```

- [ ] **Step 4: Rework `send_voice_frame`**

Replace the body (≈ 11594):
```rust
#[tauri::command]
async fn send_voice_frame(
    payload: voice::SendVoiceFramePayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let community_id = crate::owner_state_types::SpaceId(parse_voice_id_16("communityId", &payload.community_id)?);
    let channel_id = crate::owner_state_types::ChannelId(parse_voice_id_16("channelId", &payload.channel_id)?);
    let voice_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.voice_tx.clone().ok_or_else(|| "not connected".to_string())?
    };
    voice_tx
        .send(voice::VoiceOutbound { community_id, channel_id, frame: payload.frame_bytes })
        .await
        .map_err(|_| "event loop not running".to_string())
}
```

- [ ] **Step 5: Rework `join_voice_channel` (capability resolution)**

Replace the body (≈ 11616). This resolves the channel key (registry), signing key (`DmOutbox`), and own identity (`NodeState`):
```rust
#[tauri::command]
async fn join_voice_channel(
    community_id: String,
    channel_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let community = crate::owner_state_types::SpaceId(parse_voice_id_16("communityId", &community_id)?);
    let channel = crate::owner_state_types::ChannelId(parse_voice_id_16("channelId", &channel_id)?);

    // Snapshot the handles we need out of NodeState without holding the lock
    // across awaits (the guard is !Send).
    let (tx, registry, dm_outbox, self_owner, self_device) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        let tx = guard.voice_channel_tx.clone().ok_or_else(|| "not connected".to_string())?;
        let registry = guard.channel_log_registry.clone().ok_or_else(|| "no channel registry".to_string())?;
        let dm_outbox = guard.dm_outbox.clone().ok_or_else(|| "no dm outbox".to_string())?;
        let self_owner = guard.dm_self_owner.ok_or_else(|| "no owner identity".to_string())?;
        let device_hex = guard.dm_device_id.clone().ok_or_else(|| "no device id".to_string())?;
        let self_device = <[u8; 32]>::try_from(
            hex::decode(&device_hex).map_err(|_| "device id not hex".to_string())?.as_slice(),
        ).map_err(|_| "device id must be 32 bytes".to_string())?;
        (tx, registry, dm_outbox, self_owner, self_device)
    };

    let engine = registry.engine(&community, &channel).await
        .ok_or_else(|| "voice channel not ready (no channel engine)".to_string())?;
    let channel_key = engine.channel_key_arc();
    let signing_key = { dm_outbox.lock().await.community_signing_key.clone() };

    tx.send(voice::VoiceChannelRequest::Join {
        community_id: community,
        channel_id: channel,
        caps: voice::VoiceJoinCaps { channel_key, signing_key, self_owner, self_device },
    })
    .await
    .map_err(|_| "event loop not running".to_string())
}
```
**Confirm** the exact types: `NodeState.dm_outbox` field name (grep `dm_outbox` in the `struct NodeState` block) and that `DmOutbox.community_signing_key` is `Arc<ed25519_dalek::SigningKey>` (used at lib.rs ≈ 13287). If `dm_outbox` is not a `NodeState` field, locate the field that holds the `Arc<Mutex<DmOutbox>>` and use it. Adjust the `.clone()` / `.lock().await` accordingly.

- [ ] **Step 6: Rework `leave_voice_channel`**

Replace the body (≈ 11634):
```rust
#[tauri::command]
async fn leave_voice_channel(
    community_id: String,
    channel_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let community = crate::owner_state_types::SpaceId(parse_voice_id_16("communityId", &community_id)?);
    let channel = crate::owner_state_types::ChannelId(parse_voice_id_16("channelId", &channel_id)?);
    let tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.voice_channel_tx.clone().ok_or_else(|| "not connected".to_string())?
    };
    tx.send(voice::VoiceChannelRequest::Leave { community_id: community, channel_id: channel })
        .await
        .map_err(|_| "event loop not running".to_string())
}
```

- [ ] **Step 7: Validation unit tests**

Add an inline test near the helper (or in the existing `lib.rs` test module):
```rust
#[test]
fn parse_voice_id_16_accepts_32_hex_and_rejects_else() {
    assert!(parse_voice_id_16("x", &"ab".repeat(16)).is_ok());
    assert!(parse_voice_id_16("x", "zz").is_err());          // non-hex
    assert!(parse_voice_id_16("x", &"ab".repeat(8)).is_err()); // wrong length
}
```

- [ ] **Step 8: Compile, fix downstream, commit**

`event_loop.rs` will not yet compile against the new `VoiceOutbound`/`VoiceChannelRequest` shapes — that is Task 4. To keep this task's commit green, **do Tasks 3 and 4 as one compile unit**: implement Task 4's event-loop changes before running the full build. Run `cd src-tauri && cargo check --locked --all-targets --features test-fixtures` only after Task 4. (If you prefer a green commit here, stub the event-loop arms minimally to match the new types, then complete them in Task 4.)

Commit after Task 4 compiles (see Task 4 Step 6).

---

## Task 4: Event-loop voice relay rework (seal/open + topic-routing)

**Files:**
- Modify: `src-tauri/src/event_loop.rs`

Outbound seals under the cached key and publishes to a fully-scoped, own-device-named topic. Inbound opens and drops on failure. The key is cached per `(community, channel)` on Join; the node's own device id is cached once.

- [ ] **Step 1: Add caches near `voice_subs`**

Near the `voice_subs` declaration (≈ 1697), add:
```rust
// ZEB-350: per-join channel key (seals/open media + beacons) and the node's
// own device id (names the outbound topic segment). Keyed identically to
// voice_subs so Join/Leave keep them in lockstep.
let mut voice_keys: std::collections::HashMap<
    (crate::owner_state_types::SpaceId, crate::owner_state_types::ChannelId),
    std::sync::Arc<crate::community_channel_log::ChannelKey>,
> = std::collections::HashMap::new();
let mut voice_own_device: Option<String> = None; // hex of self ed25519 vk
```
Change `voice_subs` key type from `String` to `(SpaceId, ChannelId)` (re-declare its `HashMap` type). The presence subscriber/publisher/sweep handles (Task 7) will be stored in parallel maps declared there too.

- [ ] **Step 2: Rework the outbound relay arm**

Replace the `Some(voice) = voice_rx.recv()` arm (≈ 2313):
```rust
Some(voice) = voice_rx.recv() => {
    if let Some(key) = voice_keys.get(&(voice.community_id, voice.channel_id)) {
        let own = voice_own_device.as_deref().unwrap_or("self");
        match crate::voice_crypto::encrypt_voice_packet(
            key, &voice.community_id, &voice.channel_id,
            crate::voice_crypto::VOICE_PACKET_AAD, &voice.frame,
        ) {
            Ok(sealed) => {
                let key_expr = format!(
                    "harmony/voice/{}/{}/{}",
                    hex::encode(voice.community_id.0),
                    hex::encode(voice.channel_id.0),
                    own,
                );
                if let Err(e) = session.put(&key_expr, sealed).await {
                    tracing::warn!(%key_expr, err = %e, "voice publish failed");
                }
            }
            Err(e) => tracing::warn!(err = %e, "voice seal failed; dropping frame"),
        }
    }
    // else: not joined to that (community, channel) — drop.
}
```

- [ ] **Step 3: Rework the Join arm (cache key + own device, seal-aware subscriber)**

In the `VoiceChannelRequest::Join { community_id, channel_id, caps }` arm, before declaring the subscriber:
```rust
voice_keys.insert((community_id, channel_id), std::sync::Arc::clone(&caps.channel_key));
if voice_own_device.is_none() {
    voice_own_device = Some(hex::encode(caps.self_device));
}
let sub_key = format!(
    "harmony/voice/{}/{}/*",
    hex::encode(community_id.0), hex::encode(channel_id.0),
);
```
Then the spawned subscriber task opens before emitting (capture an `Arc<ChannelKey>` clone + the ids):
```rust
let key_for_sub = std::sync::Arc::clone(&caps.channel_key);
let (c_sub, ch_sub) = (community_id, channel_id);
let app_sub = app.clone();
let closing_sub = closing.clone();
match session.declare_subscriber(&sub_key).await {
    Ok(sub) => {
        let handle = tokio::spawn(async move {
            while let Ok(sample) = sub.recv_async().await {
                let sealed = sample.payload().to_bytes().to_vec();
                match crate::voice_crypto::decrypt_voice_packet(
                    &key_for_sub, &c_sub, &ch_sub,
                    crate::voice_crypto::VOICE_PACKET_AAD, &sealed,
                ) {
                    Ok(frame) => {
                        let _ = app_sub.emit("voice-frame-received", serde_json::json!({ "frameBytes": frame }));
                    }
                    Err(_) => { /* non-member / stale epoch / tamper → drop silently */ }
                }
            }
            if !closing_sub.load(std::sync::atomic::Ordering::SeqCst) {
                tracing::warn!("voice subscriber closed unexpectedly");
            }
        });
        if let Some(old) = voice_subs.insert((community_id, channel_id), handle) {
            old.abort();
        }
    }
    Err(e) => tracing::error!(%sub_key, err = %e, "voice subscribe failed"),
}
```
(Task 7 adds the presence publisher + subscriber spawn calls inside this same Join arm, after the media subscriber.)

- [ ] **Step 4: Rework the Leave arm**

```rust
crate::voice::VoiceChannelRequest::Leave { community_id, channel_id } => {
    voice_keys.remove(&(community_id, channel_id));
    if let Some(handle) = voice_subs.remove(&(community_id, channel_id)) {
        handle.abort();
    }
    // Task 7: send presence tombstone + stop presence publisher/subscriber/sweep here.
}
```

- [ ] **Step 5: Update the shutdown drain**

Wherever `voice_subs.drain()` aborts on shutdown, it still works with the new key type — confirm it compiles (the `for (_, handle) in voice_subs.drain()` pattern is key-type agnostic).

- [ ] **Step 6: Build the Task 3+4 compile unit, fmt/clippy, commit**

Run:
```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && \
cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -20 && \
cargo fmt --all && \
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -15
```
Expected: clean. Then:
```bash
git add src-tauri/src/voice.rs src-tauri/src/lib.rs src-tauri/src/community_channel_log_engine.rs src-tauri/src/event_loop.rs
git commit -m "feat(zeb-350): scope voice IPCs+relay to (community, channel); seal media under ChannelKey

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Presence beacon type + sign/seal (`voice_presence.rs`)

**Files:**
- Create: `src-tauri/src/voice_presence.rs` (beacon types + sign/verify; map + spawn helpers added in Task 6/7)
- Modify: `src-tauri/src/lib.rs` (`mod voice_presence;`)
- Modify: `src-tauri/tests/wire_format_voice_fixtures.rs` (pin the signed+sealed beacon)

Beacon: canonical CBOR, 2-char keys, device-#2 ed25519 signature over the canonical bytes of the unsigned beacon. The signed wrapper is then sealed under `ChannelKey` with `VOICE_PRESENCE_AAD`.

- [ ] **Step 1: Write failing tests**

Create `src-tauri/src/voice_presence.rs`:
```rust
//! ZEB-350 Voice V2 presence: ephemeral signed+sealed beacons + the live
//! roster. Beacons ride a dedicated Zenoh topic (never the CRDT); the seal
//! under `ChannelKey` gates non-members, and the device-#2 signature +
//! materialized-membership check (Task 7) prevents intra-member spoofing.

use serde::{Deserialize, Serialize};
use crate::owner_state_types::{ChannelId, Hlc, OwnerAddr, SpaceId};

/// The unsigned presence claim. Canonical CBOR, 2-char keys (same-length
/// invariant for deterministic encoding).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoicePresenceBeacon {
    #[serde(rename = "ow", serialize_with = "crate::reachability_record::serialize_bytes_as_bstr",
            deserialize_with = "crate::reachability_record::deserialize_bytes_from_bstr")]
    pub owner: [u8; 16],
    #[serde(rename = "dv", serialize_with = "crate::reachability_record::serialize_bytes_as_bstr",
            deserialize_with = "crate::reachability_record::deserialize_bytes_from_bstr")]
    pub device: [u8; 32],
    #[serde(rename = "mu")]
    pub muted: bool,
    #[serde(rename = "jh")]
    pub joined_hlc: Hlc,
    #[serde(rename = "sq")]
    pub seq: u64,
    #[serde(rename = "lf", default, skip_serializing_if = "is_false")]
    pub left: bool,
}

fn is_false(b: &bool) -> bool { !*b }

/// Beacon + detached device-#2 signature over `canonical_cbor_encode(beacon)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedVoicePresenceBeacon {
    #[serde(rename = "bc")]
    pub beacon: VoicePresenceBeacon,
    #[serde(rename = "sg", serialize_with = "crate::reachability_record::serialize_bytes_as_bstr",
            deserialize_with = "crate::reachability_record::deserialize_bytes_from_bstr")]
    pub sig: [u8; 64],
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BeaconError {
    #[error("beacon CBOR encode failed")]
    Encode,
    #[error("beacon signature invalid")]
    BadSig,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn beacon(seq: u64) -> VoicePresenceBeacon {
        VoicePresenceBeacon {
            owner: [0xa1; 16],
            device: [0u8; 32], // overwritten by sign helper's caller in real use
            muted: true,
            joined_hlc: Hlc { wall_ms: 1000, logical: 0, device_id: "aa".repeat(32) },
            seq,
            left: false,
        }
    }

    #[test]
    fn sign_then_verify_round_trips() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(1);
        b.device = sk.verifying_key().to_bytes();
        let signed = sign_presence_beacon(b.clone(), &sk).unwrap();
        assert_eq!(signed.beacon, b);
        verify_presence_beacon_sig(&signed).expect("valid sig");
    }

    #[test]
    fn tampered_beacon_fails_verify() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(1);
        b.device = sk.verifying_key().to_bytes();
        let mut signed = sign_presence_beacon(b, &sk).unwrap();
        signed.beacon.muted = false; // tamper after signing
        assert_eq!(verify_presence_beacon_sig(&signed), Err(BeaconError::BadSig));
    }

    #[test]
    fn signature_must_match_embedded_device_key() {
        let signer = SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(1);
        b.device = SigningKey::from_bytes(&[9u8; 32]).verifying_key().to_bytes(); // different device
        let signed = sign_presence_beacon(b, &signer).unwrap();
        assert_eq!(verify_presence_beacon_sig(&signed), Err(BeaconError::BadSig));
    }
}
```

- [ ] **Step 2: Confirm fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(voice_presence)'`
Expected: compile error — `sign_presence_beacon` / `verify_presence_beacon_sig` missing.

- [ ] **Step 3: Implement sign/verify + seal/open**

Add to `voice_presence.rs`:
```rust
use crate::owner_state_crypto::canonical_cbor_encode;
use crate::community_channel_log::ChannelKey;
use crate::voice_crypto::{decrypt_voice_packet, encrypt_voice_packet, VOICE_PRESENCE_AAD};

/// Sign a beacon with the device-#2 ed25519 key. The signature covers the
/// canonical CBOR of the unsigned beacon (sig field excluded by construction).
pub fn sign_presence_beacon(
    beacon: VoicePresenceBeacon,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<SignedVoicePresenceBeacon, BeaconError> {
    use ed25519_dalek::Signer;
    let bytes = canonical_cbor_encode(&beacon).map_err(|_| BeaconError::Encode)?;
    let sig = signing_key.sign(&bytes).to_bytes();
    Ok(SignedVoicePresenceBeacon { beacon, sig })
}

/// Verify the detached signature against the verifying key embedded in
/// `beacon.device`. This proves the holder of `device`'s private key signed
/// it; Task 7 additionally checks `device ∈ owner.enrolled_device_keys`.
pub fn verify_presence_beacon_sig(signed: &SignedVoicePresenceBeacon) -> Result<(), BeaconError> {
    let bytes = canonical_cbor_encode(&signed.beacon).map_err(|_| BeaconError::Encode)?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&signed.beacon.device).map_err(|_| BeaconError::BadSig)?;
    let sig = ed25519_dalek::Signature::from_bytes(&signed.sig);
    vk.verify_strict(&bytes, &sig).map_err(|_| BeaconError::BadSig)
}

/// Seal a signed beacon under the channel key for transport. Output framing
/// matches the voice media packet (`[12B nonce][ct+tag]`), distinct AAD.
pub fn seal_presence_beacon(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    signed: &SignedVoicePresenceBeacon,
) -> Result<Vec<u8>, BeaconError> {
    let plain = canonical_cbor_encode(signed).map_err(|_| BeaconError::Encode)?;
    encrypt_voice_packet(key, community, channel, VOICE_PRESENCE_AAD, &plain).map_err(|_| BeaconError::Encode)
}

/// Open + decode a sealed beacon. Returns `None` on any failure (drop).
pub fn open_presence_beacon(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    packet: &[u8],
) -> Option<SignedVoicePresenceBeacon> {
    let plain = decrypt_voice_packet(key, community, channel, VOICE_PRESENCE_AAD, packet).ok()?;
    ciborium::from_reader(plain.as_slice()).ok()
}
```
**Confirm** `serialize_bytes_as_bstr` / `deserialize_bytes_from_bstr` are `pub` in `reachability_record.rs` (they are used by `ReachabilityAnnouncePayload`); if they are `pub(crate)` and not re-exported, reference them by their crate path as written, or copy the 4-line helpers locally. Confirm `canonical_cbor_encode` is at `crate::owner_state_crypto` (it is — the fixtures import `harmony_app::owner_state_crypto::canonical_cbor_encode`).

Add `mod voice_presence;` to `lib.rs`.

- [ ] **Step 4: Pass + add a seal/open round-trip test**

Append to the test module:
```rust
#[test]
fn seal_open_round_trips_and_wrong_key_drops() {
    use crate::community_channel_log::derive_channel_key;
    use crate::owner_state_types::EpochKey;
    let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
    let mut b = beacon(3);
    b.device = sk.verifying_key().to_bytes();
    let signed = sign_presence_beacon(b, &sk).unwrap();
    let (c, ch) = (SpaceId([0xc0; 16]), ChannelId([0xc1; 16]));
    let key = derive_channel_key(&EpochKey::new([0x11; 32]), &c, &ch);
    let sealed = seal_presence_beacon(&key, &c, &ch, &signed).unwrap();
    assert_eq!(open_presence_beacon(&key, &c, &ch, &sealed), Some(signed));
    let other = derive_channel_key(&EpochKey::new([0x22; 32]), &c, &ch);
    assert_eq!(open_presence_beacon(&other, &c, &ch, &sealed), None);
}
```
Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(voice_presence)'` → all pass.

- [ ] **Step 5: Pin the signed+sealed beacon wire bytes**

Append to `src-tauri/tests/wire_format_voice_fixtures.rs` a test that builds a fully-deterministic `SignedVoicePresenceBeacon` (fixed key `[7u8;32]`, fixed `joined_hlc`, `seq=1`) and pins `hex::encode(seal_presence_beacon_with_nonce(...))`. Because `seal_presence_beacon` uses a random nonce, add a test-fixtures-gated `seal_presence_beacon_with_nonce` that calls `encrypt_voice_packet_with_nonce` with `[0u8;12]`, mirroring Task 1's pattern. Bootstrap with `UPDATE_VOICE_FIXTURE=1`, paste the hex, remove the debug print. Also add a back-compat decode assertion: `open` of the pinned bytes yields the expected beacon (guards against silent struct drift).

- [ ] **Step 6: fmt/clippy/commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo fmt --all && \
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -15
git add src-tauri/src/voice_presence.rs src-tauri/src/lib.rs src-tauri/tests/wire_format_voice_fixtures.rs
git commit -m "feat(zeb-350): presence beacon type — device-#2 sign + ChannelKey seal + wire pin

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Roster map + heartbeat/eviction (`voice_presence.rs`)

**Files:**
- Modify: `src-tauri/src/voice_presence.rs`

The map applies beacons (freshness by `seq` per device), evicts on a 12 s silence window (3 missed 4 s heartbeats), and removes instantly on a `left` tombstone. Time is injected (`now: Instant`-equivalent) so eviction is tested with logical time, not wall-clock sleeps (per the timing-test rule).

- [ ] **Step 1: Write failing tests**

Append to `voice_presence.rs`:
```rust
#[cfg(test)]
mod map_tests {
    use super::*;

    fn b(owner: u8, device: u8, seq: u64, muted: bool, left: bool) -> VoicePresenceBeacon {
        VoicePresenceBeacon {
            owner: [owner; 16], device: [device; 32], muted,
            joined_hlc: Hlc { wall_ms: 1, logical: 0, device_id: "x".into() },
            seq, left,
        }
    }
    const C: SpaceId = SpaceId([0xc0; 16]);
    const CH: ChannelId = ChannelId([0xc1; 16]);
    const TTL_MS: u64 = 12_000;

    #[test]
    fn apply_then_roster_lists_member() {
        let mut m = VoicePresenceMap::new();
        assert!(m.apply(&C, &CH, &b(1, 1, 0, true, false), 0));
        let r = m.roster(&C, &CH);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].owner, [1; 16]);
        assert!(r[0].muted);
    }

    #[test]
    fn stale_seq_ignored_newer_applied() {
        let mut m = VoicePresenceMap::new();
        m.apply(&C, &CH, &b(1, 1, 5, true, false), 0);
        assert!(!m.apply(&C, &CH, &b(1, 1, 3, false, false), 0), "older seq → no change");
        assert!(m.roster(&C, &CH)[0].muted, "still muted=true from seq 5");
        assert!(m.apply(&C, &CH, &b(1, 1, 6, false, false), 0), "newer seq → change");
        assert!(!m.roster(&C, &CH)[0].muted);
    }

    #[test]
    fn heartbeat_keeps_alive_silence_evicts() {
        let mut m = VoicePresenceMap::new();
        m.apply(&C, &CH, &b(1, 1, 0, true, false), 0);
        m.apply(&C, &CH, &b(1, 1, 1, true, false), 8_000); // heartbeat at 8s
        assert!(m.sweep(11_000, TTL_MS).is_empty(), "within TTL of last beacon");
        assert_eq!(m.sweep(21_000, TTL_MS), vec![([1u8;16], [1u8;32])], "12s after last → evict");
        assert!(m.roster(&C, &CH).is_empty());
    }

    #[test]
    fn tombstone_removes_instantly() {
        let mut m = VoicePresenceMap::new();
        m.apply(&C, &CH, &b(1, 1, 0, true, false), 0);
        assert!(m.apply(&C, &CH, &b(1, 1, 1, true, true), 100), "left=true → change (removal)");
        assert!(m.roster(&C, &CH).is_empty());
    }
}
```

- [ ] **Step 2: Implement the map**

Append:
```rust
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceEntry {
    pub owner: [u8; 16],
    pub muted: bool,
    pub seq: u64,
    pub joined_hlc: Hlc,
    /// Monotonic-ms timestamp of the last applied beacon (injected by caller).
    pub last_seen_ms: u64,
}

/// One roster row surfaced to the frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RosterEntry {
    #[serde(serialize_with = "crate::reachability_record::serialize_bytes_as_bstr")]
    pub owner: [u8; 16],
    #[serde(serialize_with = "crate::reachability_record::serialize_bytes_as_bstr")]
    pub device: [u8; 32],
    pub muted: bool,
}

#[derive(Debug, Default)]
pub struct VoicePresenceMap {
    // (community, channel) → device → entry
    inner: BTreeMap<(SpaceId, ChannelId), BTreeMap<[u8; 32], PresenceEntry>>,
}

impl VoicePresenceMap {
    pub fn new() -> Self { Self::default() }

    /// Apply a (verified, opened) beacon. `now_ms` is a monotonic clock the
    /// caller supplies. Returns true if the roster changed.
    pub fn apply(&mut self, c: &SpaceId, ch: &ChannelId, beacon: &VoicePresenceBeacon, now_ms: u64) -> bool {
        let chan = self.inner.entry((*c, *ch)).or_default();
        if beacon.left {
            return chan.remove(&beacon.device).is_some();
        }
        match chan.get_mut(&beacon.device) {
            Some(e) if beacon.seq <= e.seq => false, // stale or duplicate
            Some(e) => {
                let changed = e.muted != beacon.muted;
                e.muted = beacon.muted;
                e.seq = beacon.seq;
                e.last_seen_ms = now_ms;
                e.joined_hlc = beacon.joined_hlc.clone();
                changed || true // seq/last_seen advanced; treat as change for heartbeat liveness
            }
            None => {
                chan.insert(beacon.device, PresenceEntry {
                    owner: beacon.owner, muted: beacon.muted, seq: beacon.seq,
                    joined_hlc: beacon.joined_hlc.clone(), last_seen_ms: now_ms,
                });
                true
            }
        }
    }

    /// Evict entries whose last beacon is older than `ttl_ms`. Returns the
    /// `(owner, device)` of each evicted entry.
    pub fn sweep(&mut self, now_ms: u64, ttl_ms: u64) -> Vec<([u8; 16], [u8; 32])> {
        let mut evicted = Vec::new();
        for ((_, _), chan) in self.inner.iter_mut() {
            chan.retain(|device, e| {
                let alive = now_ms.saturating_sub(e.last_seen_ms) < ttl_ms;
                if !alive { evicted.push((e.owner, *device)); }
                alive
            });
        }
        evicted
    }

    pub fn roster(&self, c: &SpaceId, ch: &ChannelId) -> Vec<RosterEntry> {
        self.inner.get(&(*c, *ch)).map(|chan| {
            chan.iter().map(|(device, e)| RosterEntry { owner: e.owner, device: *device, muted: e.muted }).collect()
        }).unwrap_or_default()
    }
}
```
**Note on the `Some(e)` arm return:** it returns `true` whenever a beacon advances `seq` (even with no mute change) because liveness/heartbeat is itself a roster-relevant change for the emit cadence; the `changed` local is folded in for clarity. If the reviewer prefers emitting only on mute/membership change (not on every heartbeat), gate the emit at the call site (Task 7) instead — keep `apply` returning true on any advance so `last_seen` always updates. Document whichever you pick.

- [ ] **Step 3: Pass, fmt/clippy, commit**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(voice_presence)'` → all pass.
```bash
cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -15
git add src-tauri/src/voice_presence.rs
git commit -m "feat(zeb-350): voice presence roster map — seq freshness + 12s eviction + tombstone

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Membership-verified subscriber + heartbeat publisher, wired into the loop

**Files:**
- Modify: `src-tauri/src/voice_presence.rs` (verifier + spawn helpers)
- Modify: `src-tauri/src/event_loop.rs` (start on Join, stop+tombstone on Leave, sweep task)

- [ ] **Step 1: Membership beacon verifier**

In `voice_presence.rs`, add an async verifier that resolves the sender owner's enrolled device keys from materialized membership and confirms `device ∈ keys` and `status == Joined`:
```rust
use std::sync::Arc;
use crate::community_state_sync::CommunitySyncRegistry;

/// Resolve whether `(owner, device)` is an enrolled, joined member of
/// `community` per materialized membership. Cheap: `materialized()` is cached.
pub async fn beacon_signer_is_member(
    registry: &Arc<CommunitySyncRegistry>,
    community: &SpaceId,
    owner: &OwnerAddr,
    device: &[u8; 32],
) -> bool {
    let Some(engine) = registry.engine_arc(community).await else { return false };
    let admin = engine.admin_addr();
    let state = engine.state();
    let guard = state.lock().await;
    let materialized = guard.materialized(admin); // confirm signature (Step 1a)
    let Some(member) = materialized.members.get(owner) else { return false };
    member.status == crate::community_membership::MemberStatus::Joined
        && member.enrolled_device_keys.contains(device)
}
```
**Step 1a — confirm `materialized`:** grep `fn materialized` in `community_state_crdt.rs`. If it is `&mut self` (fills the cache), take `let mut guard` and call on `&mut *guard`. If it returns an owned `MaterializedMembership`, bind it; if it returns `&MaterializedMembership`, the borrow ends with the guard — clone the small `enrolled_device_keys`/status out before dropping the guard. Adjust this 6-line block to the real signature; do not invent one.

- [ ] **Step 2: Subscriber spawn helper**

```rust
use tokio::task::JoinHandle;

/// Spawn a presence subscriber: open → verify sig → verify membership →
/// apply to the shared map → emit `voice-presence-changed`. Drops on any
/// failure. Mirrors the channel-log subscriber idiom.
#[allow(clippy::too_many_arguments)]
pub fn spawn_voice_presence_subscriber<R: tauri::Runtime>(
    session: Arc<zenoh::Session>,
    topic: String,
    channel_key: Arc<ChannelKey>,
    community: SpaceId,
    channel: ChannelId,
    registry: Arc<CommunitySyncRegistry>,
    map: Arc<tokio::sync::Mutex<VoicePresenceMap>>,
    app: tauri::AppHandle<R>,
    closing: Arc<std::sync::atomic::AtomicBool>,
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let sub = match session.declare_subscriber(&topic).await {
            Ok(s) => s,
            Err(e) => { tracing::error!(%topic, err = %e, "presence subscribe failed"); return; }
        };
        while let Ok(sample) = sub.recv_async().await {
            let bytes = sample.payload().to_bytes().to_vec();
            let Some(signed) = open_presence_beacon(&channel_key, &community, &channel, &bytes) else { continue };
            if verify_presence_beacon_sig(&signed).is_err() { continue; }
            let owner = OwnerAddr(signed.beacon.owner);
            if !beacon_signer_is_member(&registry, &community, &owner, &signed.beacon.device).await { continue; }
            let changed = {
                let mut g = map.lock().await;
                g.apply(&community, &channel, &signed.beacon, (now_ms)())
            };
            if changed {
                let roster = { map.lock().await.roster(&community, &channel) };
                let _ = app.emit("voice-presence-changed", serde_json::json!({
                    "community": hex::encode(community.0),
                    "channel": hex::encode(channel.0),
                    "roster": roster,
                }));
            }
        }
        if !closing.load(std::sync::atomic::Ordering::SeqCst) {
            tracing::warn!(%topic, "presence subscriber closed unexpectedly");
        }
    })
}
```
Confirm `app.emit` is in scope via `use tauri::Emitter;` (the voice arm already emits, so the import exists in `event_loop.rs`; for a helper in `voice_presence.rs` add `use tauri::Emitter;`).

- [ ] **Step 3: Publisher spawn helper**

```rust
/// Spawn a 4 s heartbeat publisher. Emits an immediate beacon, then every
/// `interval`. `muted_now` lets the session report live mute state (always
/// true in V2 — start muted, no capture). Returns the handle + a `seq` is
/// internal. Caller sends a `left` tombstone on stop (see Step 5).
#[allow(clippy::too_many_arguments)]
pub fn spawn_voice_presence_publisher(
    session: Arc<zenoh::Session>,
    topic: String,
    channel_key: Arc<ChannelKey>,
    community: SpaceId,
    channel: ChannelId,
    signing_key: Arc<ed25519_dalek::SigningKey>,
    self_owner: OwnerAddr,
    self_device: [u8; 32],
    joined_hlc: Hlc,
    interval: std::time::Duration,
    closing: Arc<std::sync::atomic::AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut seq: u64 = 0;
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            if closing.load(std::sync::atomic::Ordering::SeqCst) { break; }
            let beacon = VoicePresenceBeacon {
                owner: self_owner.0, device: self_device, muted: true,
                joined_hlc: joined_hlc.clone(), seq, left: false,
            };
            seq = seq.wrapping_add(1);
            let Ok(signed) = sign_presence_beacon(beacon, &signing_key) else { continue };
            let Ok(sealed) = seal_presence_beacon(&channel_key, &community, &channel, &signed) else { continue };
            if let Err(e) = session.put(&topic, sealed).await {
                tracing::warn!(%topic, err = %e, "presence publish failed");
            }
        }
    })
}

/// Build + sign + seal a single `left=true` tombstone for instant removal.
pub fn build_presence_tombstone(
    channel_key: &ChannelKey, community: &SpaceId, channel: &ChannelId,
    signing_key: &ed25519_dalek::SigningKey, self_owner: OwnerAddr, self_device: [u8; 32], joined_hlc: Hlc,
) -> Option<Vec<u8>> {
    let beacon = VoicePresenceBeacon { owner: self_owner.0, device: self_device, muted: true, joined_hlc, seq: u64::MAX, left: true };
    let signed = sign_presence_beacon(beacon, signing_key).ok()?;
    seal_presence_beacon(channel_key, community, channel, &signed).ok()
}
```

- [ ] **Step 4: Shared presence map + sweep task in the event loop**

Near the Task-4 caches in `event_loop.rs`, add a single shared map + maps for the publisher/subscriber handles, and spawn one sweep task for the loop's lifetime (only if `community_registry` is `Some`):
```rust
let voice_presence_map = std::sync::Arc::new(tokio::sync::Mutex::new(
    crate::voice_presence::VoicePresenceMap::new()));
let mut voice_presence_subs: std::collections::HashMap<(crate::owner_state_types::SpaceId, crate::owner_state_types::ChannelId), tokio::task::JoinHandle<()>> = std::collections::HashMap::new();
let mut voice_presence_pubs: std::collections::HashMap<(crate::owner_state_types::SpaceId, crate::owner_state_types::ChannelId), tokio::task::JoinHandle<()>> = std::collections::HashMap::new();
// Monotonic clock for apply/sweep (Instant-based ms since loop start).
let voice_clock_start = std::time::Instant::now();
let voice_now_ms: std::sync::Arc<dyn Fn() -> u64 + Send + Sync> = {
    let start = voice_clock_start;
    std::sync::Arc::new(move || start.elapsed().as_millis() as u64)
};
```
Add a sweep arm to the main `select!` (every 4 s) that evicts and emits on change:
```rust
_ = tokio::time::sleep(std::time::Duration::from_secs(4)) => {
    let now = (voice_now_ms)();
    let evicted = { voice_presence_map.lock().await.sweep(now, 12_000) };
    if !evicted.is_empty() {
        // Re-emit affected channels' rosters. Simplest: emit a generic
        // "swept" signal carrying the evicted (owner, device, community,
        // channel)s, or re-emit each touched channel's roster. Implementer:
        // track which (c,ch) each evicted entry belonged to (extend sweep to
        // return the key) and emit voice-presence-changed per touched channel.
    }
}
```
**Refine `sweep`** to return `Vec<((SpaceId, ChannelId), [u8;16], [u8;32])>` so the loop can emit the right channel rosters. Update Task 6's signature + tests accordingly (the test asserts `vec![((C,CH),[1;16],[1;32])]`). This is a small, deliberate change — make it now rather than leaving the emit a stub.

Beware: a bare `tokio::time::sleep` arm in a `select!` that also drives other receivers will reset every loop iteration. Prefer a dedicated `let mut sweep_tick = tokio::time::interval(Duration::from_secs(4));` declared with the caches and `_ = sweep_tick.tick() => { … }` as the arm, matching tokio idioms.

- [ ] **Step 5: Wire Join/Leave**

In the Join arm (after the media subscriber, Task 4 Step 3), only when `community_registry` is `Some`:
```rust
if let Some(registry) = community_registry.clone() {
    let pres_topic = format!("harmony/voice-presence/{}/{}", hex::encode(community_id.0), hex::encode(channel_id.0));
    // Reserve a joined HLC for this session (own device).
    let wall_now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let joined_hlc = crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &hex::encode(caps.self_device), wall_now_ms).await;
    let sub = crate::voice_presence::spawn_voice_presence_subscriber(
        session.clone(), pres_topic.clone(), std::sync::Arc::clone(&caps.channel_key),
        community_id, channel_id, registry, std::sync::Arc::clone(&voice_presence_map),
        app.clone(), closing.clone(), std::sync::Arc::clone(&voice_now_ms),
    );
    let pubh = crate::voice_presence::spawn_voice_presence_publisher(
        session.clone(), pres_topic, std::sync::Arc::clone(&caps.channel_key),
        community_id, channel_id, std::sync::Arc::clone(&caps.signing_key),
        caps.self_owner, caps.self_device, joined_hlc, std::time::Duration::from_secs(4), closing.clone(),
    );
    if let Some(h) = voice_presence_subs.insert((community_id, channel_id), sub) { h.abort(); }
    if let Some(h) = voice_presence_pubs.insert((community_id, channel_id), pubh) { h.abort(); }
}
```
**Confirm `hlc_tracker` is in scope** at the event-loop level. If it is not threaded into `run`, reserve the HLC at the IPC boundary instead (the IPC has access) and carry `joined_hlc` inside `VoiceJoinCaps`. Prefer the latter if `hlc_tracker` is not already a `run` local — it keeps `run`'s signature untouched: add `pub joined_hlc: Hlc` to `VoiceJoinCaps`, reserve it in `join_voice_channel` via `reserve_next_hlc_for_device` against the same tracker the DM/reachability paths use (grep the tracker handle on `NodeState`/`DmOutbox`).

In the Leave arm (Task 4 Step 4), before aborting:
```rust
if let Some(h) = voice_presence_pubs.remove(&(community_id, channel_id)) { h.abort(); }
if let Some(h) = voice_presence_subs.remove(&(community_id, channel_id)) { h.abort(); }
if let Some(key) = voice_keys.get(&(community_id, channel_id)) {
    // best-effort tombstone (need joined_hlc; reuse a zeroed Hlc is acceptable
    // since left=true ignores ordering — or stash the join's hlc in a map).
    if let (Some(dev), Some(owner)) = (voice_own_device.as_ref(), dm_self_owner_in_loop) {
        // see note below on owner/device availability in the loop
    }
}
// then remove voice_keys + abort media sub (Task 4)
```
**Tombstone identity note:** the Leave arm needs `self_owner` + `self_device` + a `joined_hlc`. Stash these from the Join `caps` into a small `voice_identity: HashMap<(SpaceId,ChannelId), (OwnerAddr,[u8;32],Hlc)>` on Join, and read+remove it on Leave to build the tombstone via `build_presence_tombstone`, then `session.put(pres_topic, tombstone).await`. Add that map beside the other caches.

- [ ] **Step 6: Build, fmt/clippy, commit**

Run the Task-4-style `cargo check` + `clippy` + `fmt`. Expected clean. Commit:
```bash
git add src-tauri/src/voice_presence.rs src-tauri/src/event_loop.rs
git commit -m "feat(zeb-350): presence publisher+subscriber wired into loop; membership-verified beacons

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Two-engine presence + relay integration test

**Files:**
- Create: `src-tauri/tests/voice_presence_two_engine_integration.rs`

Mirror `community_channel_messages_integration.rs`. Two Zenoh sessions, two identities, a shared `(community, channel)` and derived `ChannelKey`. Engine A runs a presence publisher; engine B runs a subscriber wired to a real `CommunitySyncRegistry` whose materialized membership lists A as a Joined, enrolled member. Assert B's roster converges to include A, then that silence evicts A after the TTL, then that a tombstone removes instantly. Add an AEAD media-relay leg: A seals a frame, B opens + emits.

- [ ] **Step 1: Stand up the harness**

Use `fixture_identity`, two `zenoh::open(Config::default())` sessions, `derive_channel_key`, and `tauri::test::mock_app()`. For membership, build a real `CommunitySyncEngine`/registry seeded so that `materialized(admin).members[owner_a]` is `Joined` with `enrolled_device_keys = { device_a_vk }`. **Reuse the existing membership test helper** (`mint_test_owner` and the community-engine setup used by `community_membership` / `community_state_sync` tests — grep `mint_test_owner` and the helper that produces a seeded `CommunitySyncEngine`). If standing up a full seeded registry is heavy, factor `beacon_signer_is_member`'s core into a pure helper `device_is_enrolled(materialized: &MaterializedMembership, owner, device) -> bool` and unit-test that directly, while the two-engine test exercises seal/sign/open/apply with a stub verifier injected. Prefer the real registry if the existing helpers make it ≤ ~40 lines.

- [ ] **Step 2: Assertions (use logical polling, not fixed sleeps where possible)**

```rust
// roster convergence
wait_until(Duration::from_secs(10), || async {
    map_b.lock().await.roster(&community, &channel).iter().any(|r| r.owner == owner_a.0)
}).await;
// eviction: stop A's publisher, advance the injected clock past TTL, sweep
// tombstone: publish build_presence_tombstone(...) from A → B roster empties
```
For eviction, since the publisher uses real `tokio::time::interval`, either (a) drop A's publisher handle and `sweep` with a `now_ms` advanced by >12 000, or (b) use `tokio::time::pause()`/`advance` if the test is single-threaded compatible. Given the channel-log template runs multi-thread, prefer the injected-clock approach: call `map_b.sweep(now_after_ttl, 12_000)` directly and assert the roster empties.

- [ ] **Step 3: Run + commit**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(voice_presence_two_engine)' --no-capture 2>&1 | tail -30`
Expected: PASS (allow a generous outer `tokio::time::timeout(Duration::from_secs(30), …)` like the template; loopback Zenoh declaration settling needs ~1 s). If it deterministically times out only in the local sandbox the way the 6 known transport orphan-flakes do, note it and rely on CI — but first confirm it is the same class (real-network CI green), do not assume.
```bash
git add src-tauri/tests/voice_presence_two_engine_integration.rs
git commit -m "test(zeb-350): two-engine voice presence exchange + sealed relay integration

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Frontend IPC-shape updates

**Files:**
- Modify: `src/lib/voice/voice-sender.ts` + its test
- Modify: `src/lib/community-service.ts` (only if it exposes voice types)

V2 ships no comms mic capture, so this is limited to keeping the IPC boundary type-correct: `send_voice_frame` now requires `communityId`.

- [ ] **Step 1: Thread `communityId` through `VoiceSender`**

In `src/lib/voice/voice-sender.ts`, add `communityId: string` to `VoiceSenderConfig`, and change the invoke:
```ts
await this.adapter.invoke('send_voice_frame', {
  payload: { communityId: this.config.communityId, channelId: this.config.channelId, frameBytes: Array.from(frame) },
});
```
(Match the existing `frameBytes` serialization — keep whatever array/Uint8 form is already used.)

- [ ] **Step 2: Update the sender test**

In the voice-sender test, add `communityId` to the config and assert the invoke is called with the `communityId` field. Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/voice 2>&1 | tail -20`.

- [ ] **Step 3: tsc + commit**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit`
Expected: clean (no other caller passes a `VoiceSenderConfig` without `communityId`; if Spellbook's `FlashcardView`/`PttButton` construct one, thread a `communityId` there — grep `new VoiceSender(` and fix every call site).
```bash
git add src/lib/voice/voice-sender.ts src/lib/voice/__tests__ src/lib/community-service.ts
git commit -m "feat(zeb-350): thread communityId into send_voice_frame IPC shape

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Final gate sweep + push + PR

**Files:** none (verification + PR).

- [ ] **Step 1: Full backend gate**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && \
cargo fmt --all -- --check && \
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && \
set -o pipefail && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -40
```
Expected: fmt clean, clippy clean, nextest green except the known 6 transport orphan-flakes. Verify the new `voice_crypto` / `voice_presence` / `wire_format_voice` / `voice_presence_two_engine` tests all pass.

- [ ] **Step 2: MSRV + frontend**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo check --locked --all-targets --features test-fixtures
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit && npx vitest run 2>&1 | tail -20
```

- [ ] **Step 3: Push + open PR**

```bash
git push -u origin zeb-350-voice-presence-aead
gh pr create --title "ZEB-350 Voice V2: presence + AEAD seam (sealed relay + signed beacons)" --body "$(cat <<'EOF'
## Summary
Voice V2 (ZEB-350) — second slice of the voice-comms epic (parent **ZEB-348**). Proves a live roster + sealed relay; no comms mic capture yet (that's V3 / ZEB-351).

- **AEAD seam** (`voice_crypto.rs`): `encrypt/decrypt_voice_packet` — raw-byte ChaCha20-Poly1305 over the existing channel `ChannelKey`, AAD = domain ‖ community ‖ channel (distinct domains for media vs presence, so cross-domain/cross-channel replay can't open).
- **Relay rework** (`event_loop.rs`): outbound seals + publishes to `harmony/voice/{community}/{channel}/{ownDevice}` (routing moved into the topic so the whole frame is sealed); inbound opens, drops silently on AEAD failure.
- **IPC + types** (`voice.rs`, `lib.rs`): `send_voice_frame`/`join_voice_channel`/`leave_voice_channel` now carry `(community, channel)`; the join IPC resolves the channel key + device-#2 signing key + own identity and threads them in via `VoiceJoinCaps` (no change to `event_loop::run`'s signature).
- **Presence beacons** (`voice_presence.rs`): ephemeral signed (device #2) + sealed (`ChannelKey`) beacons on `harmony/voice-presence/{community}/{channel}`; 4 s heartbeat, 12 s eviction, instant `left` tombstone; `VoicePresenceMap` roster; `voice-presence-changed` event. Receivers verify the signer against **materialized membership** (`enrolled_device_keys`), per the ZEB-339 norm.

Reuses ZEB-248 channel crypto (`ChannelKey` / `derive_channel_key`) and ZEB-339 enrolled-device-key membership.

## Test plan
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (green except the 6 known iroh/zenoh transport orphan-flakes)
- [ ] AEAD round-trip + wrong-key/wrong-scope/wrong-domain drops
- [ ] Beacon sign/seal round-trip; seq-freshness + 12 s eviction + tombstone (logical time)
- [ ] Two-engine presence exchange + sealed relay integration
- [ ] Voice-packet + beacon wire-format pins
- [ ] `npx tsc --noEmit` + `npx vitest run`

Spec: `docs/specs/2026-05-31-voice-comms-design.md` §V2. Plan: `docs/plans/2026-06-01-zeb-350-voice-v2-presence-aead.md`.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 4: Attach the PR to ZEB-350** (use bare `ZEB-350` mention in the body — do NOT use Closes/Fixes keywords on ZEB-348/351-353, to avoid the Linear cascade closing the epic or sibling slices). After opening, link the PR on the Linear issue and set ZEB-350 → In Progress.

---

## Self-review notes

**Spec coverage (spec §V2 + ZEB-350 acceptance):**
- AEAD seam (`encrypt/decrypt_voice_packet`, distinct AAD) → Task 1; round-trip + wrong-key/AAD drops → Task 1 tests. ✓
- Relay rework (seal outbound / open inbound / routing-in-topic) → Task 4. ✓
- IPC + `voice.rs` rework to `(community, channel)` → Task 3. ✓
- Beacon topic/payload/sign/seal → Task 5; cadence (4 s) + eviction (12 s) + tombstone → Tasks 6–7. ✓
- `VoicePresence` map + `voice-presence-changed` emit → Tasks 6–7. ✓
- Beacon sign/seal round-trip + heartbeat/timeout (logical time) → Tasks 5–6 tests. ✓
- Two-engine presence-exchange integration → Task 8. ✓
- Wire fixtures (voice packet + beacon) → Tasks 2, 5. ✓
- All gates → Task 10. ✓

**Design deviation from spec (documented):** the spec said the presence machinery "mirrors `community_reachability`," but reachability is actually CRDT-borne with no heartbeat/eviction — so V2 builds the ephemeral beacon + heartbeat/evict pattern fresh (the design decision D5 "ephemeral Zenoh beacons" is unchanged). The two-engine test mirrors `community_channel_messages_integration.rs`, not the transport-layer reachability test.

**Type consistency:** `SpaceId`/`ChannelId` are `[u8;16]` newtypes; `OwnerAddr` wraps `[u8;16]`; device key is `[u8;32]` ed25519 vk; `ChannelKey` reused as-is with a new `channel_key_arc()` accessor. `VoiceOutbound`/`VoiceChannelRequest`/`VoiceJoinCaps` are consistent across Tasks 3, 4, 7. `sweep` returns `(key, owner, device)` after the Task-7 refinement — Task 6's test must be updated to match (called out in Task 7 Step 4).

**Open confirmations the implementer MUST resolve against real code (flagged inline, not placeholders):** `materialized()` signature (Task 7 Step 1a); `NodeState.dm_outbox` field name + `DmOutbox.community_signing_key` type (Task 3 Step 5); `hlc_tracker` availability at the loop vs. reserving `joined_hlc` at the IPC (Task 7 Step 5); `serialize_bytes_as_bstr` visibility (Task 5 Step 3); every `new VoiceSender(` call site (Task 9 Step 3).

**Risk notes:** Task 4 + Task 7 touch the large `event_loop.rs` `select!` — keep the sweep arm an `interval.tick()`, not a resetting `sleep`. Per-beacon membership materialization is cached, but if a reviewer flags cost at 64 members, the fix is a short-TTL enrolled-keys cache (follow-up, not V2-blocking). Start-muted is hardcoded `muted: true` in V2 (no capture); V3 makes it dynamic.
