# Community Presence (ZEB-537) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development to implement task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Surface which members of the *active* community are online/reachable, via signed+sealed liveness beacons over a dedicated zenoh topic with a TTL staleness window, surfaced as a `presence-updated` event and rendered against the member list.

**Architecture:** Generalize the voice-presence beacon pattern (`voice_presence.rs`, ZEB-350) from per-call to per-community scope. New `community_presence.rs` module (beacon types, crypto helpers, roster map, pub/sub/sweeper spawns). Lifecycle driven by a `subscribe_community_presence` IPC mirroring the `subscribe_member_card` → `ProfileCardRequest` → event-loop-spawns-subscriber pattern. Active-community-only (one subscription at a time, but keyed by community so N is fine).

**Tech stack:** Rust (tauri, tokio, zenoh, chacha20poly1305, ed25519-dalek, ciborium), TypeScript/Svelte frontend.

**Spec:** `docs/specs/2026-06-21-zeb-537-community-presence-design.md`.

**Build/gate (from `src-tauri/`):**
- fmt: `cargo fmt --all -- --check`
- clippy: `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- test: `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- frontend (repo root): `npx tsc --noEmit` && `npx vitest run`
- Per-task fast loop: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(<name>)'` + `cargo clippy -p harmony-app --lib --features test-fixtures`.

**Key template anchors (read these; mirror, don't reinvent):**
- `src-tauri/src/voice_presence.rs` — beacon types (13-61), sign/verify (82-102), seal/open (106-149), `VoicePresenceMap` (272-470), publisher (667-721), subscriber (531-597), membership gate `beacon_signer_is_member` (501-521).
- `src-tauri/src/voice_crypto.rs` — `encrypt_voice_packet`/`decrypt_voice_packet` (119-180), `scope_aad` (60-68), `VOICE_PRESENCE_AAD` (17), `NONCE_LEN`/`MIN_PACKET_LEN`/`MAX_VOICE_PACKET_BYTES`.
- `src-tauri/src/community_channel_log.rs` — `ChannelKey` (40), `derive_channel_key` (67-81).
- `src-tauri/src/community_state_sync.rs` — `CommunitySyncEngine.membership_key()` (1201-1203), `engine_arc` (4734).
- `src-tauri/src/event_loop.rs` — `ProfileCardRequest` (355-363), subscribe handler spawn (2077-2228), unsubscribe (2230-2235); voice pub/sub spawn sites (3987-4046).
- `src-tauri/src/lib.rs` — member-card IPC + impls (27687-27796); `generate_handler!` (~46675-46810).
- `src-tauri/src/api/rpc.rs` — `rpc!` macro (51-76), an Args struct (145-152), a registration (403-417), curated surface test (903-979).
- `src-tauri/tests/wire_format/voice_fixtures.rs` — presence-beacon fixture (75-122).
- `src/lib/member-card-service.ts`, `src/lib/channel-message-service.ts`, `src/lib/voice-session.ts:480` (listen pattern).

---

## Task 1: Beacon types + crypto primitives

**Files:**
- Create: `src-tauri/src/community_presence.rs`
- Modify: `src-tauri/src/voice_crypto.rs` (add `COMMUNITY_PRESENCE_AAD`), `src-tauri/src/community_channel_log.rs` (add `derive_presence_key`), `src-tauri/src/lib.rs` (add `mod community_presence;`)

- [ ] **Step 1: Add the AAD constant.** In `voice_crypto.rs`, next to `VOICE_PRESENCE_AAD` (line 17):
```rust
/// ZEB-537 community-presence beacon AEAD domain. Distinct from
/// `VOICE_PRESENCE_AAD` so a community-presence packet can never be opened in
/// (or confused with) a voice-presence or channel-log context.
pub const COMMUNITY_PRESENCE_AAD: &[u8] = b"harmony-community-presence-v1";
```

- [ ] **Step 2: Add `derive_presence_key`** in `community_channel_log.rs` next to `derive_channel_key` (67-81). Mirrors it exactly but binds only the community (no channel), with a distinct `info`:
```rust
/// ZEB-537: derive the per-community presence key from the community epoch
/// (membership) key. Mirrors `derive_channel_key` but binds only the community
/// — presence is community-scoped, not per-channel — with a distinct `info`
/// label so the presence key is independent of every channel key.
pub fn derive_presence_key(mk: &EpochKey, community_id: &SpaceId) -> ChannelKey {
    let salt = community_id.0;
    let info = b"presence:";
    let mut out = zeroize::Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(&salt), mk.as_bytes())
        .expand(info, out.as_mut())
        .expect("32 <= 8160");
    ChannelKey(*out)
}
```
(Confirm `ChannelKey`'s tuple field is constructible here — it is, same module. Reuse the existing `Hkdf`/`Sha256` imports.)

- [ ] **Step 3: Write the failing test** for `derive_presence_key` (in `community_channel_log.rs` tests, mirroring `derive_channel_key_is_deterministic` at 1508):
```rust
#[test]
fn derive_presence_key_is_deterministic_and_distinct() {
    let mk = EpochKey::new([0x55; 32]);
    let c = SpaceId([0xc0; 16]);
    let p1 = derive_presence_key(&mk, &c);
    let p2 = derive_presence_key(&mk, &c);
    assert_eq!(p1.as_bytes(), p2.as_bytes(), "deterministic");
    // Distinct from any channel key and from a different community's presence key.
    let ch = derive_channel_key(&mk, &c, &ChannelId([0xc1; 16]));
    assert_ne!(p1.as_bytes(), ch.as_bytes(), "presence key != channel key");
    let other = derive_presence_key(&mk, &SpaceId([0xc2; 16]));
    assert_ne!(p1.as_bytes(), other.as_bytes(), "per-community");
}
```
Run: `cargo nextest run -p harmony-app --lib --features test-fixtures -E 'test(derive_presence_key)'` → FAIL (not defined), then PASS after Step 2.

- [ ] **Step 4: Create `community_presence.rs` with beacon types.** Mirror `voice_presence.rs` 1-61, dropping `muted`/`left`, renaming `joined_hlc`→`started_hlc`:
```rust
//! ZEB-537 community presence: ephemeral signed+sealed liveness beacons + the
//! live roster. Beacons ride a dedicated zenoh topic per community (never the
//! CRDT); the seal under the per-community presence key (derived from the epoch
//! key) gates non-members, and the device signature + materialized-membership
//! check prevents intra-member spoofing. Generalizes voice_presence.rs from
//! per-call to per-community scope; relies on a TTL staleness window (no
//! explicit leave/gravestone).
use crate::community_channel_log::ChannelKey;
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceBeacon {
    #[serde(rename = "ow", serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr", deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr")]
    pub owner: [u8; 16],
    #[serde(rename = "dv", serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr", deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr")]
    pub device: [u8; 32],
    #[serde(rename = "sh")]
    pub started_hlc: Hlc,
    #[serde(rename = "sq")]
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedPresenceBeacon {
    #[serde(rename = "bc")]
    pub beacon: PresenceBeacon,
    #[serde(rename = "sg", serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr", deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr")]
    pub sig: [u8; 64],
}

impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for PresenceBeacon {}
impl crate::owner_state_crypto::CanonicalPayload for PresenceBeacon {}
impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for SignedPresenceBeacon {}
impl crate::owner_state_crypto::CanonicalPayload for SignedPresenceBeacon {}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BeaconError {
    #[error("beacon CBOR encode failed")] Encode,
    #[error("beacon signature invalid")] BadSig,
    #[error("beacon transport publish failed")] Publish,
}
```
Register the module in `lib.rs` (`mod community_presence;` next to `mod voice_presence;`).

- [ ] **Step 5: Add sign/verify/seal/open** in `community_presence.rs`, mirroring voice_presence.rs 82-149 but sealing via `COMMUNITY_PRESENCE_AAD` and a zero-channel sentinel (presence is community-scoped; the constant channel id keeps the audited `encrypt_voice_packet` signature satisfied while the per-community key + AAD provide separation):
```rust
use crate::community_channel_log::ChannelId; // re-exported; the sentinel below
use crate::owner_state_crypto::canonical_cbor_encode;
use crate::voice_crypto::{decrypt_voice_packet, encrypt_voice_packet, COMMUNITY_PRESENCE_AAD};

/// Community presence is not channel-scoped; bind a constant zero channel into
/// the AEAD AAD so the audited `encrypt_voice_packet` signature is satisfied.
/// Separation comes from the per-community presence key + the distinct AAD.
const PRESENCE_SENTINEL_CHANNEL: ChannelId = ChannelId([0u8; 16]);

pub fn sign_presence_beacon(beacon: PresenceBeacon, signing_key: &ed25519_dalek::SigningKey) -> Result<SignedPresenceBeacon, BeaconError> {
    use ed25519_dalek::Signer;
    let bytes = canonical_cbor_encode(&beacon).map_err(|_| BeaconError::Encode)?;
    let sig = signing_key.sign(&bytes).to_bytes();
    Ok(SignedPresenceBeacon { beacon, sig })
}

pub fn verify_presence_beacon_sig(signed: &SignedPresenceBeacon) -> Result<(), BeaconError> {
    let bytes = canonical_cbor_encode(&signed.beacon).map_err(|_| BeaconError::Encode)?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&signed.beacon.device).map_err(|_| BeaconError::BadSig)?;
    let sig = ed25519_dalek::Signature::from_bytes(&signed.sig);
    vk.verify_strict(&bytes, &sig).map_err(|_| BeaconError::BadSig)
}

pub fn seal_presence_beacon(key: &ChannelKey, community: &SpaceId, signed: &SignedPresenceBeacon) -> Result<Vec<u8>, BeaconError> {
    let plain = canonical_cbor_encode(signed).map_err(|_| BeaconError::Encode)?;
    encrypt_voice_packet(key, community, &PRESENCE_SENTINEL_CHANNEL, COMMUNITY_PRESENCE_AAD, &plain).map_err(|_| BeaconError::Encode)
}

pub fn open_presence_beacon(key: &ChannelKey, community: &SpaceId, packet: &[u8]) -> Option<SignedPresenceBeacon> {
    let plain = decrypt_voice_packet(key, community, &PRESENCE_SENTINEL_CHANNEL, COMMUNITY_PRESENCE_AAD, packet).ok()?;
    ciborium::from_reader(plain.as_slice()).ok()
}

#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn seal_presence_beacon_with_nonce(key: &ChannelKey, community: &SpaceId, signed: &SignedPresenceBeacon, nonce: [u8; 12]) -> Result<Vec<u8>, BeaconError> {
    let plain = canonical_cbor_encode(signed).map_err(|_| BeaconError::Encode)?;
    crate::voice_crypto::encrypt_voice_packet_with_nonce(key, community, &PRESENCE_SENTINEL_CHANNEL, COMMUNITY_PRESENCE_AAD, &plain, nonce).map_err(|_| BeaconError::Encode)
}
```
(If `ChannelId` isn't re-exported from `community_channel_log`, import from its real path — confirm via grep. Confirm `encrypt_voice_packet_with_nonce` exists under `test-fixtures`; the extraction confirmed the voice equivalent does.)

- [ ] **Step 6: Crypto unit tests** in `community_presence.rs` `#[cfg(test)] mod tests`:
```rust
fn fixture_keypair() -> ed25519_dalek::SigningKey { ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]) }
fn fixture_beacon(sk: &ed25519_dalek::SigningKey) -> PresenceBeacon {
    PresenceBeacon { owner: [0xa1; 16], device: sk.verifying_key().to_bytes(),
        started_hlc: Hlc { wall_ms: 1000, logical: 0, device_id: "aa".repeat(32) }, seq: 1 }
}
#[test] fn sign_then_verify_ok() { let sk = fixture_keypair(); let s = sign_presence_beacon(fixture_beacon(&sk), &sk).unwrap(); assert!(verify_presence_beacon_sig(&s).is_ok()); }
#[test] fn tampered_sig_rejected() { let sk = fixture_keypair(); let mut s = sign_presence_beacon(fixture_beacon(&sk), &sk).unwrap(); s.beacon.seq = 999; assert!(verify_presence_beacon_sig(&s).is_err()); }
#[test] fn seal_open_roundtrip_and_wrong_key_drops() {
    use crate::community_channel_log::derive_presence_key; use crate::owner_state_types::EpochKey;
    let sk = fixture_keypair(); let signed = sign_presence_beacon(fixture_beacon(&sk), &sk).unwrap();
    let c = SpaceId([0xc0; 16]);
    let key = derive_presence_key(&EpochKey::new([0x11; 32]), &c);
    let sealed = seal_presence_beacon(&key, &c, &signed).unwrap();
    assert_eq!(open_presence_beacon(&key, &c, &sealed), Some(signed));
    let wrong = derive_presence_key(&EpochKey::new([0x22; 32]), &c);
    assert!(open_presence_beacon(&wrong, &c, &sealed).is_none());
}
```
Verify: targeted nextest for `community_presence` + `derive_presence_key` PASS; `cargo clippy -p harmony-app --lib --features test-fixtures` clean.

- [ ] **Step 7: Commit** `feat(presence): beacon types + community-scoped seal/open + derive_presence_key`.

---

## Task 2: `CommunityPresenceMap` (roster, freshness, TTL)

**Files:** `src-tauri/src/community_presence.rs`

- [ ] **Step 1: Write failing tests first** (TDD) for the roster behaviors:
```rust
// in tests mod
use crate::owner_state_types::Hlc;
fn b(owner: u8, dev: u8, wall: u64, seq: u64) -> PresenceBeacon {
    PresenceBeacon { owner: [owner;16], device: [dev;32],
        started_hlc: Hlc { wall_ms: wall, logical: 0, device_id: "aa".repeat(32) }, seq }
}
#[test] fn new_device_marks_online_and_reports_change() {
    let mut m = CommunityPresenceMap::new(); let c = SpaceId([1;16]);
    assert!(m.apply(&c, &b(1,1,100,0), 1_000));
    let r = m.online_owners(&c); assert_eq!(r.len(), 1); assert_eq!(r[0].owner, [1;16]); assert_eq!(r[0].device_count, 1);
}
#[test] fn bare_refresh_does_not_report_change() {
    let mut m = CommunityPresenceMap::new(); let c = SpaceId([1;16]);
    assert!(m.apply(&c, &b(1,1,100,0), 1_000));
    assert!(!m.apply(&c, &b(1,1,100,1), 2_000)); // same session, newer seq, already online
}
#[test] fn reordered_old_beacon_rejected() {
    let mut m = CommunityPresenceMap::new(); let c = SpaceId([1;16]);
    assert!(m.apply(&c, &b(1,1,100,5), 1_000));
    assert!(!m.apply(&c, &b(1,1,100,3), 2_000)); // stale seq, no change
}
#[test] fn restart_new_session_supersedes() {
    let mut m = CommunityPresenceMap::new(); let c = SpaceId([1;16]);
    assert!(m.apply(&c, &b(1,1,100,9), 1_000));
    assert!(!m.apply(&c, &b(1,1,200,0), 2_000)); // newer started_hlc, seq reset 0 accepted; already-online owner → no visible change
    // last_seen advanced so sweep keeps it
}
#[test] fn sweep_evicts_stale_and_reports() {
    let mut m = CommunityPresenceMap::new(); let c = SpaceId([1;16]);
    m.apply(&c, &b(1,1,100,0), 1_000);
    let ev = m.sweep(1_000 + 30_001, 30_000);
    assert_eq!(ev.len(), 1); assert!(m.online_owners(&c).is_empty());
}
#[test] fn multi_device_aggregates_to_one_owner() {
    let mut m = CommunityPresenceMap::new(); let c = SpaceId([1;16]);
    m.apply(&c, &b(1,1,100,0), 1_000); m.apply(&c, &b(1,2,100,0), 1_000);
    let r = m.online_owners(&c); assert_eq!(r.len(), 1); assert_eq!(r[0].device_count, 2);
}
```

- [ ] **Step 2: Implement `CommunityPresenceMap`** mirroring `VoicePresenceMap` (272-470) but: keyed by `SpaceId` → device → `PresenceEntry { owner, started_hlc, seq, last_seen_ms }` (no `muted`, no `left`/gravestone). `apply` returns true only when a NEW device appears (owner-visible change); accept rule = `started_hlc strictly_newer` OR (`==` AND `seq > stored.seq`); always bump `last_seen_ms` on accept. Add `online_owners(&SpaceId) -> Vec<OwnerPresence { owner:[u8;16], device_count:u32, last_seen_ms:u64 }>` aggregating devices→owner (last_seen = max). `sweep(now, ttl) -> Vec<(SpaceId,[u8;16],[u8;32])>` evict stale, reclaim emptied sub-maps. `remove_community(&SpaceId)`. Use `Hlc::is_strictly_newer_than` (confirm method name in `owner_state_types`).

- [ ] **Step 3: Run tests** → all PASS. `clippy -p harmony-app --lib`.
- [ ] **Step 4: Commit** `feat(presence): CommunityPresenceMap roster with TTL + freshness`.

---

## Task 3: Wire-format fixture

**Files:** `src-tauri/tests/wire_format/presence_fixtures.rs` (or add a section to the existing `wire_format` module — confirm the module layout in `tests/wire_format/`), register in the `wire_format` test crate.

- [ ] **Step 1: Write the pinning test** mirroring `voice_fixtures.rs:75-122`: deterministic key `[7u8;32]`, fixed `started_hlc`, `seq=1`, `derive_presence_key(EpochKey::new([0x11;32]), SpaceId([0xc0;16]))`, `seal_presence_beacon_with_nonce(..., [0u8;12])`. Assert the hex is stable across runs and that the pinned bytes `open_presence_beacon` back to the original (back-compat decode). Leave the `expected = "..."` empty first.
- [ ] **Step 2: Run once** to capture the actual hex, paste it into `expected`, re-run → PASS. (Requires `--features test-fixtures`.)
- [ ] **Step 3: Commit** `test(presence): pin presence-beacon wire format`.

---

## Task 4: Publisher + subscriber spawn functions

**Files:** `src-tauri/src/community_presence.rs`

- [ ] **Step 1: `spawn_community_presence_publisher`** mirroring voice publisher (667-721) but: no mute/kick; community-scoped; re-derive the key per tick via the registry so rotation is followed. Signature:
```rust
pub fn spawn_community_presence_publisher(
    session: zenoh::Session,
    topic: String,
    registry: Arc<crate::community_state_sync::CommunitySyncRegistry>,
    community: SpaceId,
    signing_key: Arc<ed25519_dalek::SigningKey>,
    self_owner: OwnerAddr,
    self_device: [u8; 32],
    started_hlc: Hlc,
    seq_counter: Arc<AtomicU64>,
    interval: std::time::Duration,
    closing: Arc<AtomicBool>,
) -> JoinHandle<()>
```
Each tick: skip if `closing`; `seq = seq_counter.fetch_add(1)`; build `PresenceBeacon`; sign; fetch key = `registry.engine_arc(&community).await?.membership_key()` → `derive_presence_key(&mk, &community)`; seal; `session.put(&topic, sealed)`. (If the engine is gone, skip this tick.)

- [ ] **Step 2: `spawn_community_presence_subscriber`** mirroring voice subscriber (531-597): `declare_subscriber(topic)`; per sample: bound-check against `MAX_VOICE_PACKET_BYTES`; fetch key from registry; `open_presence_beacon`; `verify_presence_beacon_sig`; `beacon_signer_is_member(&registry, &community, &OwnerAddr(beacon.owner), &beacon.device)`; `map.apply`; on change, build `online_owners` roster and `emit_ser(app, "presence-updated", &PresenceUpdatedPayload{..})` (DTO defined in Task 6 — for now emit a `serde_json::json!` matching the DTO shape; switch to the typed struct in Task 6). Signature mirrors the voice subscriber (session, topic, registry, community, map Arc, app sink, closing, now_ms fn).

- [ ] **Step 3: Tests** — drive the publisher/subscriber via an in-process zenoh-free harness is heavy; instead unit-test the pure pieces already covered (Task 1/2). Add ONE focused async test using a `tokio::sync::mpsc`-backed fake? Not feasible without a session. Defer end-to-end pub/sub coverage to Task 10's two-engine integration test; here just ensure it compiles + clippy-clean. (Document this in the task so the reviewer doesn't expect unit coverage of the spawn fns.)

- [ ] **Step 4: Commit** `feat(presence): community presence publisher + subscriber spawns`.

---

## Task 5: Event-loop lifecycle + request channel + sweeper

**Files:** `src-tauri/src/event_loop.rs`, `src-tauri/src/lib.rs` (NodeState fields)

- [ ] **Step 1: Add `CommunityPresenceRequest`** in `event_loop.rs` near `ProfileCardRequest` (355):
```rust
pub enum CommunityPresenceRequest {
    Subscribe { community_id: [u8; 16] },
    Unsubscribe { community_id: [u8; 16] },
}
```

- [ ] **Step 2: NodeState plumbing** (`lib.rs`): add fields `community_presence_request_tx: Option<mpsc::Sender<CommunityPresenceRequest>>` and `community_presence_map: Option<Arc<tokio::sync::Mutex<crate::community_presence::CommunityPresenceMap>>>` (shared so `get_community_presence` can read it). Initialize them where the event loop + other request channels are wired at node start (mirror `profile_card_request_tx`).

- [ ] **Step 3: Event-loop handler** mirroring the `ProfileCardRequest` handler (2077-2235): keep a `handles: HashMap<[u8;16], (JoinHandle, JoinHandle)>` (pub+sub) keyed by community. On `Subscribe`: dedupe; build `topic = format!("harmony/presence/{}/beacons", hex::encode(community_id))`; spawn publisher + subscriber (using the registry, the shared map, the sink, self identity + signing key from the event-loop scope — locate the same source voice caps/state-root publishing use; `started_hlc` = a fresh Hlc at spawn; `seq_counter` = new `Arc::new(AtomicU64::new(0))`; `BEACON_INTERVAL_MS = 10_000`). Store handles. Emit an initial empty `presence-updated` for the community. On `Unsubscribe`: abort both handles, `map.lock().await.remove_community(&community)`, emit an empty roster.

- [ ] **Step 4: Global sweeper** — at event-loop startup (where other periodic tasks spawn), spawn a task: every `BEACON_INTERVAL_MS` call `map.sweep(now_ms, STALE_MS=30_000)`; for each affected community, re-emit its `online_owners` roster via `presence-updated`. Guard on `closing`.

- [ ] **Step 5: Constants** in `community_presence.rs`: `pub const BEACON_INTERVAL_MS: u64 = 10_000; pub const STALE_MS: u64 = 30_000;`.

- [ ] **Step 6:** Build + clippy. (Behavioral coverage via Task 10.) **Commit** `feat(presence): event-loop subscribe/unsubscribe lifecycle + sweeper`.

---

## Task 6: IPC commands + DTOs

**Files:** `src-tauri/src/lib.rs`

- [ ] **Step 1: DTOs** (camelCase, near other payloads ~1797):
```rust
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresenceMemberDto { pub owner_id_hex: String, pub online: bool, pub last_seen_ms: u64, pub device_count: u32 }
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresenceUpdatedPayload { pub community_id: String, pub members: Vec<PresenceMemberDto> }
```
Switch Task 4/5's `json!` emits to these typed structs.

- [ ] **Step 2: IPC commands + impls** mirroring member-card (27687-27796):
```rust
#[tauri::command]
async fn subscribe_community_presence(state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>, community_id: String) -> Result<(), String> { subscribe_community_presence_impl(state_lock.inner(), community_id).await }
#[tauri::command]
async fn unsubscribe_community_presence(state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>, community_id: String) -> Result<(), String> { unsubscribe_community_presence_impl(state_lock.inner(), community_id).await }
#[tauri::command]
async fn get_community_presence(state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>, community_id: String) -> Result<Vec<PresenceMemberDto>, String> { get_community_presence_impl(state_lock.inner(), community_id).await }
```
Impls: parse `community_id` hex → `[u8;16]` (reuse the community-id parse helper used elsewhere, e.g. the 16-byte/32-hex validation in `post_channel_message_impl`); subscribe/unsubscribe send the request via `community_presence_request_tx` (OWNER_NOT_LOADED_MSG if None); `get_community_presence_impl` reads the shared `community_presence_map` → `online_owners` → map to `PresenceMemberDto { online: true, .. }` (a returned row is by definition online).

- [ ] **Step 3: Register in `generate_handler!`** (~46675-46810): add the three commands.

- [ ] **Step 4: Impl-level tests** (community-id-too-short rejected; owner-not-loaded error path). Run targeted nextest + clippy.
- [ ] **Step 5: Commit** `feat(presence): subscribe/unsubscribe/get IPC + DTOs`.

---

## Task 7: RPC registry + curated surface

**Files:** `src-tauri/src/api/rpc.rs`

- [ ] **Step 1: Args structs** (camelCase, near 145):
```rust
#[derive(serde::Deserialize)] #[serde(rename_all = "camelCase")] struct SubscribeCommunityPresenceArgs { community_id: String }
#[derive(serde::Deserialize)] #[serde(rename_all = "camelCase")] struct UnsubscribeCommunityPresenceArgs { community_id: String }
#[derive(serde::Deserialize)] #[serde(rename_all = "camelCase")] struct GetCommunityPresenceArgs { community_id: String }
```
- [ ] **Step 2: Register** three `rpc!(...)` calls (mirror 403-417) delegating to the `*_impl` fns.
- [ ] **Step 3: Curated surface** — add `"subscribe_community_presence"`, `"unsubscribe_community_presence"`, `"get_community_presence"` to the `expected` list in `registry_has_exactly_the_curated_v1_surface` (903-979), under a `// community presence (ZEB-537)` comment.
- [ ] **Step 4:** Run `cargo nextest run -p harmony-app --lib -E 'test(registry_has_exactly_the_curated_v1_surface)'` → PASS. **Commit** `feat(presence): headless RPC registrations + curated surface`.

---

## Task 8: Frontend presence service

**Files:** Create `src/lib/presence-service.ts`, `src/lib/__tests__/presence-service.test.ts`

- [ ] **Step 1: Write failing vitest** mirroring `member-card-service.test.ts` / `channel-message-service.test.ts`: a fake adapter with `invoke` + `listen`; assert `subscribe(communityId, onUpdate)` invokes `subscribe_community_presence` (camelCase `{ communityId }`), seeds via `get_community_presence`, and an emitted `presence-updated` for that community calls `onUpdate` with the parsed members (and one for a DIFFERENT community is ignored); `unsubscribe` invokes `unsubscribe_community_presence` and drops the listener; rejection normalization (`e instanceof Error ? e.message : String(e)`).
- [ ] **Step 2: Implement `presence-service.ts`** — `ChannelMessageService`/`member-card-service` shape: `PresenceMemberDto` TS interface, `subscribe`, `unsubscribe`, `getPresence`, an internal map + `isOnline(ownerIdHex)`, `onUpdate` callback. Filter `presence-updated` by `communityId`.
- [ ] **Step 3: Run `npx vitest run src/lib/__tests__/presence-service.test.ts`** → PASS; `npx tsc --noEmit` clean.
- [ ] **Step 4: Commit** `feat(presence): frontend presence-service`.

---

## Task 9: Wire presence into the member-list view (minimal dot)

**Files:** the community member-list Svelte component (locate via grep: `list_community_members` callers / a "members" panel under `src/`).

- [ ] **Step 1:** On entering/viewing a community, call `presenceService.subscribe(communityId, ...)`; on leaving/unmount, `unsubscribe`. (This realizes "active community only".)
- [ ] **Step 2:** Render an online indicator (dot) next to each member row driven by `isOnline(ownerIdHex)`. Keep it minimal; match existing member-row styling.
- [ ] **Step 3:** `npx tsc --noEmit` + `npx vitest run` clean. Manual smoke not required for the gate. **Commit** `feat(presence): show online dots in the member list`.

---

## Task 10: Two-engine integration test + full gate

**Files:** an integration test (mirror the two-engine channel-log / e2e pattern — locate an existing two-engine test, e.g. under `tests/` or `e2e-harness/`).

- [ ] **Step 1:** Two in-process nodes sharing a community: node A `subscribe_community_presence` + beacons; node B `subscribe_community_presence`; poll until B's `get_community_presence` (or a captured `presence-updated`) shows A online — asserting on camelCase DTO keys (`communityId`, `ownerIdHex`). Then stop A's publisher / unsubscribe and advance past `STALE_MS`; assert A goes offline. Use a fast TTL override if the test harness allows injecting `STALE_MS`/`now_ms` (prefer logical time over a 30s sleep — see the wall-clock-budget rule). If a real 30s wait is unavoidable, gate it behind the slow-test profile or inject a short TTL.
- [ ] **Step 2: Full gate** from `src-tauri/`: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Repo root: `npx tsc --noEmit`; `npx vitest run`.
- [ ] **Step 3: Commit** `test(presence): two-engine online/offline integration`.

---

## Self-review checklist (run before opening the PR)
- Spec coverage: beacon+crypto (T1), roster/TTL (T2), wire pin (T3), pub/sub (T4), lifecycle/sweeper (T5), IPC/DTO (T6), RPC/curated (T7), frontend service (T8), GUI dot (T9), integration+gate (T10). ✓
- Type consistency: `PresenceBeacon`/`SignedPresenceBeacon`/`CommunityPresenceMap`/`PresenceUpdatedPayload`/`PresenceMemberDto`/`CommunityPresenceRequest` used identically across tasks; event name `presence-updated` and command names match between IPC, RPC, curated test, and frontend.
- No ZEB-NNN in branch/commit/PR-title/PR-body (code comments + this plan/spec may reference ZEB freely).
- Crypto: new surface is only a new AAD constant + a new HKDF label + a sentinel channel; the AEAD primitive is the audited `encrypt_voice_packet`. Never call `*_with_nonce` outside `test-fixtures`.
