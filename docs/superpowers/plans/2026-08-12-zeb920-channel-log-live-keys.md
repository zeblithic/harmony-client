# ZEB-920: Channel-Log Live Epoch Keys Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Channel-log wire crypto (posts, reacts, watermark vectors, RBSR, backfill replies) and the voice-join key grab select epoch keys at operation time — encrypt under the live current key, decrypt under `[current, previous]` candidates — instead of the engine's spawn-pinned key.

**Architecture:** Per-op live re-derive (spec §2): the engine keeps its pinned `Arc<ChannelKey>` as the degrade fallback and gains an optional `ChannelKeyLiveSource { membership_key, crdt_state }`. `encrypt_channel_key()` / `decrypt_channel_keys()` wrap ZEB-919's `community_publish_epoch_key_typed` / `epoch_key_candidates` + `derive_channel_key`. No re-key events, no wire-format change.

**Tech Stack:** Rust / tokio; existing helpers in `community_state_sync.rs` (`community_publish_epoch_key_typed`, `epoch_key_candidates`, `test_community_space`) and `community_channel_log.rs` (`derive_channel_key`, AEAD fns).

**Spec:** `docs/superpowers/specs/2026-08-12-zeb920-channel-log-live-keys-design.md`

## Global Constraints

- All cargo commands run from `src-tauri/` with `--locked --features test-fixtures`.
- Gates per task: `cargo fmt --all -- --check`, `cargo clippy --all-targets --no-deps --locked --features test-fixtures -- -D warnings`, scoped tests via `cargo nextest run --locked --features test-fixtures -E '<filter>'`.
- `live_key_source: None` MUST be byte-identical to today's behavior (spec §3.1) — no existing test assertion changes.
- No wire-format, AAD, or packet-shape changes (spec §6).
- Decrypt candidates never reach more than one epoch back (ZEB-918 invariant).
- Commit messages end with the standard `Co-Authored-By` + `Claude-Session` trailers (session URL uses `claude.ai`).
- Line refs are at main `3b9a2c87`; re-anchor by symbol, not line number (memory: scope by compiler).

---

### Task 1: Candidate-open helpers in `community_channel_log.rs`

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs` (helpers next to `decrypt_channel_packet` ~`:768`, `open_watermark_vector` ~`:878`, `open_rbsr_message` ~`:974`; tests in the existing `mod tests`)

**Interfaces:**
- Consumes: existing `decrypt_channel_packet`, `open_watermark_vector`, `open_rbsr_message`, `ChannelEventError`.
- Produces (Task 3 depends on these exact signatures):
  - `pub(crate) fn decrypt_channel_packet_with_any(keys: &[std::sync::Arc<ChannelKey>], packet: &[u8]) -> Result<SignedChannelEvent, ChannelEventError>`
  - `pub(crate) fn open_watermark_vector_with_any(keys: &[std::sync::Arc<ChannelKey>], packet: &[u8]) -> Result<WatermarkVector, ChannelEventError>`
  - `pub(crate) fn open_rbsr_message_with_any(keys: &[std::sync::Arc<ChannelKey>], frame: &[u8]) -> Result<crate::channel_rbsr::RbsrMessage, ChannelEventError>`

- [ ] **Step 1: Write the failing tests** (in `community_channel_log.rs` `mod tests`, after `rbsr_seal_round_trips_and_rejects_tamper_wrongkey_oversize`)

```rust
/// ZEB-920: the previous-epoch candidate opens an OLD-sealed artifact; an
/// unrelated key does not. One test per artifact kind, same shape as the
/// presence/addrbook `*_with_any` pins (ZEB-919).
#[test]
fn with_any_previous_candidate_opens_old_sealed_packet() {
    use std::sync::Arc;
    let c = fixture_community(0xc0);
    let ch = fixture_channel(0x01);
    let key_old = derive_channel_key(&EpochKey::new([0x11; 32]), &c, &ch);
    let key_new = derive_channel_key(&EpochKey::new([0x22; 32]), &c, &ch);
    let key_bad = derive_channel_key(&EpochKey::new([0x33; 32]), &c, &ch);

    let (payload, sk) = fixture_payload("hello");
    let ev = sign_channel_event(&payload, &sk).expect("sign");
    let sealed = encrypt_channel_packet(&key_old, &ev).expect("encrypt");

    let candidates = vec![Arc::new(key_new.clone()), Arc::new(key_old.clone())];
    assert_eq!(
        decrypt_channel_packet_with_any(&candidates, &sealed).expect("previous rung opens"),
        ev
    );
    assert!(
        decrypt_channel_packet_with_any(&[Arc::new(key_bad.clone())], &sealed).is_err(),
        "unrelated key must not open"
    );
}

#[test]
fn with_any_previous_candidate_opens_old_sealed_watermark() {
    use std::sync::Arc;
    let c = fixture_community(0xc0);
    let ch = fixture_channel(0x01);
    let key_old = derive_channel_key(&EpochKey::new([0x11; 32]), &c, &ch);
    let key_new = derive_channel_key(&EpochKey::new([0x22; 32]), &c, &ch);
    let key_bad = derive_channel_key(&EpochKey::new([0x33; 32]), &c, &ch);

    let mut wmv = WatermarkVector::new();
    wmv.observe("dev-a", 7, 3);
    let sealed = seal_watermark_vector(&key_old, &wmv).expect("seal");

    let candidates = vec![Arc::new(key_new), Arc::new(key_old)];
    assert_eq!(
        open_watermark_vector_with_any(&candidates, &sealed).expect("previous rung opens"),
        wmv
    );
    assert!(open_watermark_vector_with_any(&[Arc::new(key_bad)], &sealed).is_err());
}

#[test]
fn with_any_previous_candidate_opens_old_sealed_rbsr() {
    use crate::channel_rbsr::{max_key, RbsrMessage, RbsrMode, RbsrRange, RBSR_PROTOCOL_VERSION};
    use std::sync::Arc;
    let c = fixture_community(0xc0);
    let ch = fixture_channel(0x01);
    let key_old = derive_channel_key(&EpochKey::new([0x11; 32]), &c, &ch);
    let key_new = derive_channel_key(&EpochKey::new([0x22; 32]), &c, &ch);
    let key_bad = derive_channel_key(&EpochKey::new([0x33; 32]), &c, &ch);

    let msg = RbsrMessage {
        version: RBSR_PROTOCOL_VERSION,
        ranges: vec![RbsrRange {
            upper: max_key(),
            mode: RbsrMode::Fingerprint([3u8; 16]),
        }],
    };
    let sealed = seal_rbsr_message(&key_old, &msg).expect("seal");

    let candidates = vec![Arc::new(key_new), Arc::new(key_old)];
    assert_eq!(
        open_rbsr_message_with_any(&candidates, &sealed).expect("previous rung opens"),
        msg
    );
    assert!(open_rbsr_message_with_any(&[Arc::new(key_bad)], &sealed).is_err());
}
```

Note: `fixture_payload`, `fixture_community`, `fixture_channel` already exist in this test module. Check `WatermarkVector`'s constructor/observe method names against the existing `rbsr_aad_is_domain_separated_from_wmv` test (it uses `WatermarkVector::new()`) and the watermark tests in the same module — adjust the `observe` line to whatever the existing watermark round-trip test uses to add an entry (or use the empty vector as that test does).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(with_any_previous_candidate)' --no-fail-fast`
Expected: compile FAIL — `decrypt_channel_packet_with_any` not found.

- [ ] **Step 3: Implement the three helpers** (directly below each single-key sibling)

```rust
/// ZEB-920: try `decrypt_channel_packet` under each candidate key in order
/// (live `[current, previous]`, or the degraded `[pinned]`), returning the
/// first success. All-fail returns the LAST error so the caller's
/// garbage-drop warn carries a real cause. Mirrors ZEB-919's
/// `open_presence_with_any` / `open_records_with_any`.
pub(crate) fn decrypt_channel_packet_with_any(
    keys: &[std::sync::Arc<ChannelKey>],
    packet: &[u8],
) -> Result<SignedChannelEvent, ChannelEventError> {
    let mut last = Err(ChannelEventError::AeadDecrypt("no candidate keys".to_string()));
    for key in keys {
        match decrypt_channel_packet(key, packet) {
            Ok(ev) => return Ok(ev),
            Err(e) => last = Err(e),
        }
    }
    last
}
```

`open_watermark_vector_with_any` and `open_rbsr_message_with_any` are the same loop over `open_watermark_vector` / `open_rbsr_message` with return types `Result<WatermarkVector, ChannelEventError>` / `Result<crate::channel_rbsr::RbsrMessage, ChannelEventError>` and the same doc comment pointing at ZEB-920.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(with_any_previous_candidate)'`
Expected: 3 PASS.

- [ ] **Step 5: Commit**

```bash
git add src/community_channel_log.rs
git commit -m "ZEB-920: candidate-open helpers for channel packets, watermarks, RBSR"
```

---

### Task 2: `ChannelKeyLiveSource` + engine key-selection methods + spawn threading

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs` — `DeferredSpawn` (~`:111`), `ChannelLogEngineParams` (~`:440`), engine struct (~`:471`) + `new()` field copy (~`:568`), `Registry::spawn` (~`:2459`), `spawn_inner_now` params build (~`:2554`), `reconcile_from_state` (~`:3057`), key-selection methods next to `channel_key_ref` (~`:1900`), test fixtures + every test params/spawn construction.

**Interfaces:**
- Consumes: `community_publish_epoch_key_typed(SpaceId, Option<&Arc<Mutex<OwnerState>>>, &EpochKey) -> EpochKey` and `epoch_key_candidates(SpaceId, Option<&Arc<Mutex<OwnerState>>>, &EpochKey) -> Vec<EpochKey>` from `community_state_sync.rs`; `derive_channel_key(&EpochKey, &SpaceId, &ChannelId) -> ChannelKey`.
- Produces (Tasks 3-4 depend on):
  - `pub(crate) struct ChannelKeyLiveSource { pub(crate) membership_key: EpochKey, pub(crate) crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>> }`
  - `ChannelLogEngineParams.live_key_source: Option<ChannelKeyLiveSource>` (new field)
  - `Registry::spawn(&self, community_id, channel_id, channel_key: ChannelKey, live_key_source: Option<ChannelKeyLiveSource>, state_at_hlc, hlc_tracker)` (param added after `channel_key`)
  - `reconcile_from_state(&self, community_id, materialized, membership_key: &EpochKey, crdt_state: Option<std::sync::Arc<tokio::sync::Mutex<OwnerState>>>, state_at_hlc, hlc_tracker)` (param added after `membership_key`)
  - `pub(crate) async fn ChannelLogEngine::encrypt_channel_key(&self) -> Arc<ChannelKey>`
  - `pub(crate) async fn ChannelLogEngine::decrypt_channel_keys(&self) -> Vec<Arc<ChannelKey>>`

- [ ] **Step 1: Write the failing tests** (engine test module, near `build_engine_fixture`)

Add a fixture variant that threads a live source (old fixture delegates):

```rust
/// ZEB-920: fixture with a live key source over `crdt_state`. The spawn
/// membership key stays `[0x55; 32]` (what `channel_key` derives from), so
/// tests can pit the pinned key against a rotated live state.
async fn build_engine_fixture_with_live_source(
    crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
) -> EngineFixture {
    build_engine_fixture_inner(8, 250, 1000, Some(crdt_state)).await
}
```

Refactor `build_engine_fixture(seal, debounce, dirty)` → delegate to `build_engine_fixture_inner(seal, debounce, dirty, live_crdt: Option<Arc<Mutex<OwnerState>>>)`; the inner builds `live_key_source: live_crdt.map(|cs| ChannelKeyLiveSource { membership_key: membership_key.clone(), crdt_state: cs })` in the params (membership_key is the existing `EpochKey::new([0x55; 32])` local).

Tests:

```rust
/// ZEB-920 §3.2: with no live source the selection methods return exactly
/// the pinned spawn key — the documented degraded/test mode.
#[tokio::test]
async fn key_selection_degraded_none_is_pinned() {
    let fix = build_engine_fixture(8, 250, 1000).await;
    let enc = fix.engine.encrypt_channel_key().await;
    assert_eq!(enc.as_bytes(), fix.channel_key.as_bytes());
    let dec = fix.engine.decrypt_channel_keys().await;
    assert_eq!(dec.len(), 1);
    assert_eq!(dec[0].as_bytes(), fix.channel_key.as_bytes());
}

/// ZEB-920 §3.2: encrypt key follows the rotated live state while the
/// engine's pinned key stays on the spawn epoch.
#[tokio::test]
async fn encrypt_key_follows_rotated_live_state() {
    use std::sync::Arc;
    let community_id = SpaceId([0xc1; 16]);
    let channel_id = ChannelId([0x77; 16]);
    let live = EpochKey::new([0x22; 32]);

    let mut os = crate::owner_state_crdt::OwnerState::default();
    os.spaces.insert(
        community_id,
        crate::community_state_sync::test_community_space(community_id, 1, live.clone()),
    );
    let fix =
        build_engine_fixture_with_live_source(Arc::new(tokio::sync::Mutex::new(os))).await;

    let enc = fix.engine.encrypt_channel_key().await;
    let expect_live = derive_channel_key(&live, &community_id, &channel_id);
    assert_eq!(enc.as_bytes(), expect_live.as_bytes());
    assert_ne!(enc.as_bytes(), fix.channel_key.as_bytes(), "must leave the spawn pin");
}

/// ZEB-920 §3.2: decrypt candidates are [current, previous] — the previous
/// rung heals rotation skew; never more than one epoch back.
#[tokio::test]
async fn decrypt_keys_include_previous_epoch_rung() {
    use std::sync::Arc;
    let community_id = SpaceId([0xc1; 16]);
    let channel_id = ChannelId([0x77; 16]);
    let old = EpochKey::new([0x11; 32]);
    let new = EpochKey::new([0x22; 32]);

    let mut os = crate::owner_state_crdt::OwnerState::default();
    let mut space =
        crate::community_state_sync::test_community_space(community_id, 1, new.clone());
    space.old_epoch_keys.insert(0, old.clone());
    os.spaces.insert(community_id, space);
    let fix =
        build_engine_fixture_with_live_source(Arc::new(tokio::sync::Mutex::new(os))).await;

    let dec = fix.engine.decrypt_channel_keys().await;
    assert_eq!(dec.len(), 2);
    assert_eq!(dec[0].as_bytes(), derive_channel_key(&new, &community_id, &channel_id).as_bytes());
    assert_eq!(dec[1].as_bytes(), derive_channel_key(&old, &community_id, &channel_id).as_bytes());
}
```

(`SpaceId([0xc1; 16])` / `ChannelId([0x77; 16])` match the fixture's hardcoded ids; keep them in sync with `build_engine_fixture_inner`.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(key_selection_degraded) or test(encrypt_key_follows) or test(decrypt_keys_include)' --no-fail-fast`
Expected: compile FAIL — `ChannelKeyLiveSource` / `encrypt_channel_key` not found.

- [ ] **Step 3: Implement**

1. Struct (above `ChannelLogEngineParams`):

```rust
/// ZEB-920: live epoch-key source for per-op channel-key selection. Both
/// fields travel together — a live read always has its degrade fallback.
/// `None` on the engine = the documented degraded/test mode (pinned
/// spawn-time key both directions, byte-identical to pre-ZEB-920).
pub(crate) struct ChannelKeyLiveSource {
    /// Spawn-time membership key — what the pinned `channel_key` derives
    /// from; the fallback every live-read degrade lands on.
    pub(crate) membership_key: EpochKey,
    /// Live owner-state (`Space.current_epoch_key` / `old_epoch_keys`).
    pub(crate) crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
}
```

2. Thread `live_key_source: Option<ChannelKeyLiveSource>` through: `ChannelLogEngineParams` (new pub field after `channel_key`), engine struct (private field after `channel_key`), `ChannelLogEngine::new` (copy from params), `DeferredSpawn` (field after `channel_key`), `Registry::spawn` (param after `channel_key`; both the queue push and the fast-path `DeferredSpawn` literal), `spawn_inner_now` (params literal gains `live_key_source: ds.live_key_source`).
3. `reconcile_from_state` gains `crdt_state: Option<Arc<Mutex<OwnerState>>>` after `membership_key`; inside the per-channel loop:

```rust
let live_key_source = crdt_state.as_ref().map(|cs| ChannelKeyLiveSource {
    membership_key: membership_key.clone(),
    crdt_state: std::sync::Arc::clone(cs),
});
```

passed to `self.spawn(...)`.
4. Selection methods (next to `channel_key_ref`):

```rust
/// ZEB-920: the key to SEAL under — derived from the live current epoch
/// key (publisher-degrades to the pinned spawn key; every degrade lands
/// on it, never worse than pre-ZEB-920).
pub(crate) async fn encrypt_channel_key(&self) -> std::sync::Arc<ChannelKey> {
    match &self.live_key_source {
        None => std::sync::Arc::clone(&self.channel_key),
        Some(src) => {
            let mk = crate::community_state_sync::community_publish_epoch_key_typed(
                self.community_id,
                Some(&src.crdt_state),
                &src.membership_key,
            )
            .await;
            std::sync::Arc::new(derive_channel_key(&mk, &self.community_id, &self.channel_id))
        }
    }
}

/// ZEB-920: ordered candidate keys to OPEN under — `[current, previous]`
/// epochs (ZEB-918: the previous rung heals rotation skew, never more
/// than one epoch back), degraded `[pinned]`.
pub(crate) async fn decrypt_channel_keys(&self) -> Vec<std::sync::Arc<ChannelKey>> {
    match &self.live_key_source {
        None => vec![std::sync::Arc::clone(&self.channel_key)],
        Some(src) => crate::community_state_sync::epoch_key_candidates(
            self.community_id,
            Some(&src.crdt_state),
            &src.membership_key,
        )
        .await
        .iter()
        .map(|mk| {
            std::sync::Arc::new(derive_channel_key(mk, &self.community_id, &self.channel_id))
        })
        .collect(),
    }
}
```

5. Compile-fix every existing params/spawn construction with `live_key_source: None` (test fixtures at ~`:3281`, `:5480`, `:7608`, `:7869`; `reconcile_from_state` test callers at `:6327/:6375/:6387/:6517` gain a `None` arg) and the production `register_channel_log_engine` / `reconcile_community_channel_logs` / boot-reconcile callers in `lib.rs` gain `None` **temporarily** (Task 4 wires the real sources; the compiler finds every site — do not eye-count).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(key_selection_degraded) or test(encrypt_key_follows) or test(decrypt_keys_include)'`
Expected: 3 PASS. Then the engine suite as regression: `cargo nextest run --locked --features test-fixtures -E 'binary_id(harmony-app) and test(channel_log)'` — all PASS (degraded mode unchanged).

- [ ] **Step 5: Commit**

```bash
git add src/community_channel_log_engine.rs src/lib.rs
git commit -m "ZEB-920: ChannelKeyLiveSource + per-op key selection on the channel-log engine"
```

---

### Task 3: Convert every engine wire-crypto consumer to per-op selection

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs` — `publish` (~`:1098`), `react` (~`:1300`), `send_backfill_request` watermark seal (~`:1227`), `rbsr_respond` (~`:1910`), `rbsr_build_initial` (~`:1950`), `rbsr_ingest_and_next` (~`:1985`), `process_inbound_packet` (~`:1622`), registry `read_for_query` serve task (~`:2597-2635`), `channel_key_ref` / `channel_key_arc` retirement.

**Interfaces:**
- Consumes: Task 1 helpers (`*_with_any`), Task 2 methods (`encrypt_channel_key`, `decrypt_channel_keys`).
- Produces: no new surface; `channel_key_ref` / `channel_key_arc` become `#[cfg(any(test, feature = "test-fixtures"))]` (or are deleted if no test uses them — grep first).

- [ ] **Step 1: Write the failing rotation-pin tests** (engine test module)

```rust
/// ZEB-920 §5: publish seals under the LIVE key after rotation while the
/// engine's pinned key stays on the spawn epoch. Mirrors ZEB-919's
/// `publisher_seals_under_live_key_when_rotated`.
#[tokio::test]
async fn publish_seals_under_live_key_when_rotated() {
    use std::sync::Arc;
    let community_id = SpaceId([0xc1; 16]);
    let channel_id = ChannelId([0x77; 16]);
    let live = EpochKey::new([0x22; 32]);

    let mut os = crate::owner_state_crdt::OwnerState::default();
    os.spaces.insert(
        community_id,
        crate::community_state_sync::test_community_space(community_id, 1, live.clone()),
    );
    let mut fix =
        build_engine_fixture_with_live_source(Arc::new(tokio::sync::Mutex::new(os))).await;

    fix.engine
        .publish(b"rotated".to_vec(), None, None, None)
        .await
        .expect("publish");
    let packet = fix.publisher_rx.recv().await.expect("packet");

    let live_key = derive_channel_key(&live, &community_id, &channel_id);
    assert!(
        decrypt_channel_packet(&live_key, &packet).is_ok(),
        "post-rotation publish must seal under the live key"
    );
    assert!(
        decrypt_channel_packet(&fix.channel_key, &packet).is_err(),
        "the spawn-pinned key must NOT open a post-rotation packet"
    );
}

/// ZEB-920 §5: an OLD-sealed inbound packet is accepted via the previous
/// candidate rung after rotation (the healing direction).
#[tokio::test]
async fn inbound_old_sealed_packet_accepted_via_previous_rung() {
    use std::sync::Arc;
    let community_id = SpaceId([0xc1; 16]);
    let channel_id = ChannelId([0x77; 16]);
    let spawn_mk = EpochKey::new([0x55; 32]); // fixture's spawn membership key
    let new = EpochKey::new([0x22; 32]);

    let mut os = crate::owner_state_crdt::OwnerState::default();
    let mut space =
        crate::community_state_sync::test_community_space(community_id, 1, new.clone());
    // The spawn-epoch key is the archived previous epoch.
    space.old_epoch_keys.insert(0, spawn_mk.clone());
    os.spaces.insert(community_id, space);
    let mut fix =
        build_engine_fixture_with_live_source(Arc::new(tokio::sync::Mutex::new(os))).await;

    let hlc = Hlc {
        wall_ms: 1_000,
        logical: 0,
        device_id: "remote-device".to_string(),
    };
    let event = make_signed_event(
        fix.community_id,
        fix.channel_id,
        fix.self_owner,
        hlc,
        "old-sealed",
        &fix.signing_key,
    );
    // Sealed under the OLD (spawn-epoch) channel key — an un-rotated peer.
    let packet = encrypt_channel_packet(&fix.channel_key, &event).expect("encrypt");
    fix.subscriber_tx.send(packet).await.expect("send");

    let listed = wait_for(
        || async {
            let v = fix.engine.list_messages(None, 100).await.unwrap();
            if v.len() == 1 {
                Some(v)
            } else {
                None
            }
        },
        Duration::from_secs(2),
    )
    .await
    .expect("old-sealed packet accepted via previous rung");
    assert_eq!(extract_id(&listed[0]), extract_id(&event));
}

/// ZEB-920 §3.4: rbsr_build_initial seals under the live key post-rotation.
#[tokio::test]
async fn rbsr_initial_seals_under_live_key_when_rotated() {
    use std::sync::Arc;
    let community_id = SpaceId([0xc1; 16]);
    let channel_id = ChannelId([0x77; 16]);
    let live = EpochKey::new([0x22; 32]);

    let mut os = crate::owner_state_crdt::OwnerState::default();
    os.spaces.insert(
        community_id,
        crate::community_state_sync::test_community_space(community_id, 1, live.clone()),
    );
    let fix =
        build_engine_fixture_with_live_source(Arc::new(tokio::sync::Mutex::new(os))).await;

    let sealed = fix.engine.rbsr_build_initial().await;
    let live_key = derive_channel_key(&live, &community_id, &channel_id);
    assert!(
        crate::community_channel_log::open_rbsr_message(&live_key, &sealed).is_ok(),
        "initial RBSR request must seal under the live key"
    );
    assert!(
        crate::community_channel_log::open_rbsr_message(&fix.channel_key, &sealed).is_err(),
        "the spawn-pinned key must NOT open it"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(publish_seals_under_live) or test(inbound_old_sealed) or test(rbsr_initial_seals)'`
Expected: the two seal tests FAIL (packets still seal under the pinned key); the inbound test FAILS (single-key decrypt drops the old-sealed packet — note the fixture now has a live source whose current key is NOT the spawn key, so `process_inbound_packet`'s pinned decrypt... still succeeds pre-conversion. If it passes pre-conversion, that is expected — it pins the POST-conversion contract; keep it and note in the commit message that it guards the candidate path).

Correction (verified reasoning): pre-conversion, `process_inbound_packet` decrypts with the PINNED key, and the packet is sealed under the pinned key, so the inbound test PASSES before the change. It is a regression pin for after the conversion (post-conversion the primary candidate is the NEW key and only the previous rung opens the packet). The two seal tests are the true red→green pair.

- [ ] **Step 3: Convert the consumers**

Each conversion fetches the key(s) once at the top of the operation:

1. `publish` (~`:1098`): before the `encrypt_channel_packet` call add `let seal_key = self.encrypt_channel_key().await;` and change to `encrypt_channel_packet(&seal_key, &event)`.
2. `react` (~`:1300`): same two-line change.
3. `send_backfill_request` (~`:1227`): replace `seal_watermark_vector(self.channel_key_ref(), &vector)` with

```rust
let seal_key = self.encrypt_channel_key().await;
// … inside the existing else branch:
match seal_watermark_vector(&seal_key, &vector) {
```

(fetch the key before the `if vector.len() > …` check so the borrow lives long enough; the extra fetch when the vector is oversize is harmless).
4. `rbsr_respond`: open under candidates, seal under live:

```rust
let open_keys = self.decrypt_channel_keys().await;
let request =
    crate::community_channel_log::open_rbsr_message_with_any(&open_keys, sealed_request).ok()?;
// … unchanged body …
let seal_key = self.encrypt_channel_key().await;
let sealed = crate::community_channel_log::seal_rbsr_message(&seal_key, &reply).ok()?;
```

5. `rbsr_build_initial`: `let seal_key = self.encrypt_channel_key().await;` then `seal_rbsr_message(&seal_key, &req)`.
6. `rbsr_ingest_and_next`: fetch both once at entry (`let open_keys = self.decrypt_channel_keys().await;` / seal key fetched only at the `Some(msg)` arm); classify frames with `open_rbsr_message_with_any(&open_keys, &frame)`; seal `next` with `seal_rbsr_message(&seal_key, &msg)`.
7. `process_inbound_packet` (~`:1622`):

```rust
let open_keys = self.decrypt_channel_keys().await;
let event = match crate::community_channel_log::decrypt_channel_packet_with_any(&open_keys, &packet) {
```

(warn arm unchanged — last-error semantics keep the cause).
8. Registry `read_for_query` serve task (~`:2597-2635`): at task start fetch `let open_keys = me.decrypt_channel_keys().await;` and `let seal_key = me.encrypt_channel_key().await;` once per request; replace `open_watermark_vector(me.channel_key_ref(), &bytes)` with `open_watermark_vector_with_any(&open_keys, &bytes)` (adjust the match: the helper returns `Result`, the old site may match `Ok/Err` already — keep shape) and both `encrypt_channel_packet(me.channel_key_ref(), ev)` with `encrypt_channel_packet(&seal_key, ev)`.
9. `channel_key_ref` / `channel_key_arc`: grep consumers. `channel_key_arc`'s only production consumer (voice join, `lib.rs:28516`) is converted in Task 4 — after that, gate both accessors `#[cfg(any(test, feature = "test-fixtures"))]` (tests at `:3443/:3512/...` still use `channel_key_ref`) or delete if genuinely unused. No ungated production accessor to the pinned key remains (ZEB-919 grep-clean invariant).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(publish_seals_under_live) or test(inbound_old_sealed) or test(rbsr_initial_seals)'`
Expected: 3 PASS. Regression: `cargo nextest run --locked --features test-fixtures -E 'binary_id(harmony-app) and (test(channel_log) or test(rbsr) or test(backfill))'` — all PASS.

- [ ] **Step 5: Commit**

```bash
git add src/community_channel_log_engine.rs
git commit -m "ZEB-920: channel-log wire crypto selects keys per-op (seal live, open candidates)"
```

---

### Task 4: Wire live sources at every spawn site + voice join (`lib.rs`)

**Files:**
- Modify: `src-tauri/src/lib.rs` — `register_channel_log_engine` (~`:32461`), `reconcile_community_channel_logs` (~`:32507`), boot reconcile call (~`:9044`), delta-consumer hook (~`:7947`), eager create spawn (~`:32786`), serve-API spawn (~`:37039`), open-join reconcile call (~`:42202`), voice join key grab (~`:28516`).

**Interfaces:**
- Consumes: Task 2's `ChannelKeyLiveSource`, `Registry::spawn` / `reconcile_from_state` new params, `encrypt_channel_key`.
- Produces: `register_channel_log_engine(..., crdt_state: Option<Arc<Mutex<OwnerState>>>)` and `reconcile_community_channel_logs(..., crdt_state: Option<Arc<Mutex<OwnerState>>>)` (param appended before `hlc_tracker`-style trailing args as fits each signature; compiler drives call-site fixes).

- [ ] **Step 1: Thread the params**

1. `register_channel_log_engine` gains `crdt_state: Option<std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>>`; body builds

```rust
let live_key_source = crdt_state.as_ref().map(|cs| {
    crate::community_channel_log_engine::ChannelKeyLiveSource {
        membership_key: membership_key.clone(),
        crdt_state: std::sync::Arc::clone(cs),
    }
});
```

and passes it to `registry.spawn(...)` (replacing Task 2's temporary `None`).
2. `reconcile_community_channel_logs` gains the same param, forwarded to `reconcile_from_state` (replacing the temporary `None`).
3. Call-site sources (the compiler enumerates; expected wiring):
   - Boot reconcile `:9044` + delta-consumer hook `:7947`: clone the owner-block `crdt_state` local (`let crdt_state_for_channel_logs = std::sync::Arc::clone(&crdt_state);` hoisted before the spawn/loop, ZEB-919 pattern) → `Some(...)`.
   - `create_channel_impl` `:32786` and the serve-API spawn `:37039`: read the state guard's `crdt_state` field (`lib.rs:968`) in the same scope that produced the registry/engine handles → pass the `Option` through directly (it is already an `Option` — no `.expect`).
   - Open-join reconcile `:42202`: same — the enclosing scope that fetched the community engine has the state handle; pass its `crdt_state` clone.
   - If any site turns out to have NO reachable owner-state handle, pass `None` with a one-line comment naming why (degraded = today's behavior) — do not plumb new params beyond these two functions.
4. Voice join `:28516`: `let channel_key = engine.channel_key_arc();` → `let channel_key = engine.encrypt_channel_key().await;` with a comment:

```rust
// ZEB-920: join-time live key — a join after rotation derives from the
// live epoch. A call already in progress keeps its key (mid-call re-key
// is out of scope; calls are minutes, epochs are long-lived).
```

5. Now gate/delete `channel_key_arc` per Task 3 step 3.9.

- [ ] **Step 2: Compile + targeted tests**

Run: `cargo clippy --all-targets --no-deps --locked --features test-fixtures -- -D warnings`
Expected: clean (all E0061 sites fixed).
Run: `cargo nextest run --locked --features test-fixtures -E 'test(channel) and (test(create) or test(reconcile) or test(open_join) or test(voice))'` — PASS.

- [ ] **Step 3: Grep-clean invariant**

```bash
grep -n "channel_key_arc\|channel_key_ref" src/lib.rs src/event_loop.rs src/community_channel_log_engine.rs | grep -v "cfg(any(test" 
```

Expected: no production consumer outside the engine's own pinned-fallback internals; every hit is test-gated or a doc comment. Also `grep -n "membership_key()" src/lib.rs | <channel-log sites>` shows only fallback-arg uses.

- [ ] **Step 4: Commit**

```bash
git add src/lib.rs src/community_channel_log_engine.rs
git commit -m "ZEB-920: thread live key sources through channel-log spawn sites + voice join"
```

---

### Task 5: Gates, sweep, PR

- [ ] **Step 1: Full local gates** (working tree must be git-status-clean first)

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --no-deps --locked --features test-fixtures -- -D warnings
cargo nextest run --locked --features test-fixtures --workspace --all-targets
```

Expected: fmt clean, clippy clean, full sweep green (~5,970+ tests).

- [ ] **Step 2: Push branch, open PR**

PR body: summary (pinned-consumer table → per-op selection), skew-semantics table (spec §4), testing (new pins + degraded regression), rollout (no wire change), `Closes ZEB-920`, standard footer. Fire `@coderabbitai review` ONCE at open; never again.

- [ ] **Step 3: Convergence loop** — scan all three comment buckets, bundle findings, fix, ONE push per round, reply + resolve threads, hold at ready.
