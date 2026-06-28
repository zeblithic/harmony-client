# ZEB-585 Part A — Per-author watermark-vector catch-up — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace channel-log catch-up's single scalar HLC high-water mark with a per-author (per authoring-device) watermark vector, AEAD-sealed onto the existing Zenoh-GET catch-up query, so a returning member fetches a peer's missing offline-window messages at O(diff) wire cost instead of relying on the O(full-history) periodic reconcile.

**Architecture:** The engine computes a `{device_id → (wall_ms, logical)}` vector from an incrementally-maintained index, AEAD-seals it with the per-channel `ChannelKey` (the requester-side GET driver has no key), and stows the ciphertext in `BackfillQueryRequest.watermark_sealed`. The GET driver forwards those bytes as the GET payload. The responder's queryable reads `query.payload()`, caps + opens it, and serves per-device deltas. Additive + backward-compatible: no payload (or open failure / over cap) → today's scalar `since` path. The periodic full-reconcile floor is untouched as the within-author backstop.

**Tech Stack:** Rust, Tauri, Zenoh 1.9.0 (GET request payload), ChaCha20-Poly1305 + HKDF-SHA256 (existing `community_channel_log` crypto), ciborium (canonical CBOR), tokio, nextest.

## Global Constraints

- **Spec:** `docs/superpowers/specs/2026-06-28-zeb-585-channel-log-diff-reconciliation-design.md` (Part A).
- **Wire compatibility:** the Zenoh key-expr `harmony/channels/{cid}/{ch}/since/{hlc_hex}/{limit}` is UNCHANGED. The watermark vector is an **additive optional GET payload only**. Old peers ignore it (`query.payload()` → `None`) and use the key scalar — zero regression.
- **Encryption:** ChaCha20-Poly1305 with `derive_channel_key(EpochKey, community_id, channel_id)`; 12-byte (`NONCE_LEN`) random nonce; wire `[nonce || ct || tag]`; AAD `b"harmony-channel-wmv-v1"` (domain-separated from the reply-packet AAD `b"harmony-channel-msg-v1"`).
- **Cap-before-alloc:** `MAX_WATERMARK_VECTOR_BYTES` checked on the raw bytes view BEFORE decrypt/decode (mirrors `MAX_PAIRING_WIRE_BYTES` at `event_loop.rs:5626`).
- **Attach iff `since.is_some()`:** `since = None` (periodic floor / fresh join) carries NO vector → full reconcile, exactly as today.
- **No key plumbed to the requester driver:** the engine seals the vector before it leaves the engine. Never expose `ChannelKey`/`EpochKey` to the `event_loop` GET driver.
- **Gates (CI parity):** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. For fast iteration use `-p harmony-app --lib`; run `--all-targets` as the final gate.
- **Branch:** `channel-log-diff-reconciliation` (already on origin/main lineage). Keep ZEB IDs out of commit titles.

---

### Task 1: `WatermarkVector` type + AEAD seal/open + cap (pure)

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs` (after the `ChannelKey` block ~line 55; constants near `CHANNEL_PACKET_AAD` ~line 147)
- Modify: `src-tauri/tests/wire_format/channel_log_fixtures.rs` (mirror `backfill_reply_packet_wire_bytes_pinned`)

**Interfaces:**
- Produces: `pub type WatermarkVector = std::collections::BTreeMap<String, (u64, u32)>;`
- Produces: `pub fn seal_watermark_vector(key: &ChannelKey, v: &WatermarkVector) -> Result<Vec<u8>, ChannelEventError>`; `pub fn open_watermark_vector(key: &ChannelKey, packet: &[u8]) -> Result<WatermarkVector, ChannelEventError>`; `pub const MAX_WATERMARK_VECTOR_BYTES: usize = 64 * 1024;`
- A `#[cfg(any(test, feature = "test-fixtures"))] pub fn seal_watermark_vector_with_nonce(...)` for deterministic pinning (mirror `encrypt_channel_packet_with_nonce`).
- Consumes: existing `ChannelKey`, `ChannelEventError` (variants `CborEncode`/`AeadEncrypt`/`AeadDecrypt`/`CborDecode`/`MalformedPacket`), `NONCE_LEN` (12), `MIN_PACKET_LEN` (28), `ChaCha20Poly1305`, `OsRng`, `Payload` (all already imported in this file).

- [ ] **Step 1: Write the failing tests** (append to the existing `#[cfg(test)] mod tests` in `community_channel_log.rs`)

```rust
#[test]
fn watermark_vector_seal_open_round_trips() {
    let mk = EpochKey::new([0x33; 32]);
    let key = derive_channel_key(&mk, &SpaceId([0xc0; 16]), &ChannelId([0x01; 16]));
    let mut v: WatermarkVector = BTreeMap::new();
    v.insert("dev-a".to_string(), (100, 3));
    v.insert("dev-b".to_string(), (250, 0));
    let sealed = seal_watermark_vector(&key, &v).expect("seal");
    let opened = open_watermark_vector(&key, &sealed).expect("open");
    assert_eq!(opened, v);
}

#[test]
fn watermark_vector_open_rejects_oversize_before_decode() {
    let mk = EpochKey::new([0x33; 32]);
    let key = derive_channel_key(&mk, &SpaceId([0xc0; 16]), &ChannelId([0x01; 16]));
    let too_big = vec![0u8; MAX_WATERMARK_VECTOR_BYTES + 1];
    let err = open_watermark_vector(&key, &too_big).expect_err("must reject oversize");
    assert!(matches!(err, ChannelEventError::MalformedPacket(n) if n == MAX_WATERMARK_VECTOR_BYTES + 1));
}

#[test]
fn watermark_vector_open_rejects_tampered() {
    let mk = EpochKey::new([0x33; 32]);
    let key = derive_channel_key(&mk, &SpaceId([0xc0; 16]), &ChannelId([0x01; 16]));
    let mut v: WatermarkVector = BTreeMap::new();
    v.insert("dev-a".to_string(), (100, 3));
    let mut sealed = seal_watermark_vector(&key, &v).expect("seal");
    let last = sealed.len() - 1;
    sealed[last] ^= 0xff; // flip a tag byte
    assert!(matches!(open_watermark_vector(&key, &sealed), Err(ChannelEventError::AeadDecrypt(_))));
}

#[test]
fn watermark_vector_open_rejects_wrong_key() {
    let mk = EpochKey::new([0x33; 32]);
    let key = derive_channel_key(&mk, &SpaceId([0xc0; 16]), &ChannelId([0x01; 16]));
    let other = derive_channel_key(&EpochKey::new([0x44; 32]), &SpaceId([0xc0; 16]), &ChannelId([0x01; 16]));
    let mut v: WatermarkVector = BTreeMap::new();
    v.insert("dev-a".to_string(), (100, 3));
    let sealed = seal_watermark_vector(&key, &v).expect("seal");
    assert!(matches!(open_watermark_vector(&other, &sealed), Err(ChannelEventError::AeadDecrypt(_))));
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p harmony-app --lib community_channel_log::tests::watermark_vector` → FAIL (functions/type undefined).

- [ ] **Step 3: Implement the type + helpers** (in `community_channel_log.rs`)

```rust
/// ZEB-585: per-author (per authoring-device) catch-up watermark.
/// Maps each `Hlc.device_id` to that device's max `(wall_ms, logical)`
/// in the local log. Sealed onto a catch-up GET so the responder serves,
/// per device, only what the requester is missing — closing the
/// cross-author sub-max-HLC gap a scalar `since` leaves open. `(wall_ms,
/// logical)` (not full `Hlc`) suffices: within one device's stream
/// `device_id` is constant, so the HLC order collapses to that pair.
pub type WatermarkVector = std::collections::BTreeMap<String, (u64, u32)>;

/// AEAD AAD for sealed watermark vectors. Domain-separated from
/// `CHANNEL_PACKET_AAD` so a reply packet can never be opened as a
/// vector (or vice-versa).
pub const WATERMARK_VECTOR_AAD: &[u8] = b"harmony-channel-wmv-v1";

/// Hard cap on a sealed watermark-vector payload, checked on the bytes
/// view BEFORE decrypt/decode (cap-before-alloc; mirrors
/// `MAX_PAIRING_WIRE_BYTES`). 64 KiB ≈ 1000+ device entries — far above
/// any real early-scale community; a safety valve against a pathological
/// or malicious vector. Over cap → responder ignores the payload and
/// serves via the key-expr scalar `since`.
pub const MAX_WATERMARK_VECTOR_BYTES: usize = 64 * 1024;

#[cfg(any(test, feature = "test-fixtures"))]
pub fn seal_watermark_vector_with_nonce(
    key: &ChannelKey,
    vector: &WatermarkVector,
    nonce: [u8; NONCE_LEN],
) -> Result<Vec<u8>, ChannelEventError> {
    seal_watermark_vector_inner(key, vector, nonce)
}

fn seal_watermark_vector_inner(
    key: &ChannelKey,
    vector: &WatermarkVector,
    nonce: [u8; NONCE_LEN],
) -> Result<Vec<u8>, ChannelEventError> {
    let mut plaintext = Vec::with_capacity(64);
    ciborium::into_writer(vector, &mut plaintext)
        .map_err(|e| ChannelEventError::CborEncode(e.to_string()))?;
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let ciphertext = cipher
        .encrypt(
            (&nonce).into(),
            Payload { msg: &plaintext, aad: WATERMARK_VECTOR_AAD },
        )
        .map_err(|e| ChannelEventError::AeadEncrypt(e.to_string()))?;
    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// AEAD-seal a watermark vector with a random nonce (production path).
pub fn seal_watermark_vector(
    key: &ChannelKey,
    vector: &WatermarkVector,
) -> Result<Vec<u8>, ChannelEventError> {
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    seal_watermark_vector_inner(key, vector, nonce.into())
}

/// Open a sealed watermark vector. Rejects an oversize payload on the
/// bytes view BEFORE any AEAD work or allocation (cap-before-alloc).
pub fn open_watermark_vector(
    key: &ChannelKey,
    packet: &[u8],
) -> Result<WatermarkVector, ChannelEventError> {
    if packet.len() > MAX_WATERMARK_VECTOR_BYTES || packet.len() < MIN_PACKET_LEN {
        return Err(ChannelEventError::MalformedPacket(packet.len()));
    }
    let (nonce_bytes, ciphertext) = packet.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let plaintext = cipher
        .decrypt(
            nonce_bytes.into(),
            Payload { msg: ciphertext, aad: WATERMARK_VECTOR_AAD },
        )
        .map_err(|e| ChannelEventError::AeadDecrypt(e.to_string()))?;
    ciborium::from_reader(plaintext.as_slice())
        .map_err(|e| ChannelEventError::CborDecode(e.to_string()))
}
```

- [ ] **Step 4: Run to verify they pass** — `cargo test -p harmony-app --lib community_channel_log::tests::watermark_vector` → PASS.

- [ ] **Step 5: Add the canonical-CBOR pin** (in `tests/wire_format/channel_log_fixtures.rs`, mirroring `backfill_reply_packet_wire_bytes_pinned`; import `seal_watermark_vector_with_nonce`, `WatermarkVector`). Bootstrap the literal via `UPDATE_*_FIXTURE`-style print then freeze.

```rust
#[cfg(feature = "test-fixtures")]
#[test]
fn watermark_vector_sealed_wire_bytes_pinned() {
    let community_id = SpaceId([0xc0; 16]);
    let channel_id = ChannelId([0x01; 16]);
    let mk = EpochKey::new([0x77; 32]);
    let key = derive_channel_key(&mk, &community_id, &channel_id);
    let mut v: WatermarkVector = std::collections::BTreeMap::new();
    v.insert("a-dev".to_string(), (100_000, 0));
    v.insert("b-dev".to_string(), (250_000, 7));
    let sealed = seal_watermark_vector_with_nonce(&key, &v, [0u8; 12]).expect("seal");
    let actual_hex = hex::encode(&sealed);
    if std::env::var("UPDATE_BACKFILL_FIXTURE").is_ok() {
        eprintln!("UPDATE_WMV_FIXTURE: {actual_hex}");
    }
    let expected_hex = "<FILL FROM FIRST RUN>";
    assert_eq!(actual_hex, expected_hex, "watermark-vector wire format drifted");
}
```

- [ ] **Step 6: Bootstrap + freeze the pin** — run with `UPDATE_BACKFILL_FIXTURE=1 cargo test -p harmony-app --features test-fixtures wire_format ... -- --nocapture`, paste the hex into `expected_hex`, re-run to confirm.

- [ ] **Step 7: Commit** — `git add -A && git commit -m "feat(channel-log): WatermarkVector type + AEAD seal/open + cap"`

---

### Task 2: `device_watermarks` index on `ChannelLog` + `watermark_vector()`

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs` (the `ChannelLog` struct ~1366; `append`; `reload`; where `reaction_index` is declared/maintained/rebuilt — grep `reaction_index`)

**Interfaces:**
- Produces: `pub fn ChannelLog::watermark_vector(&self) -> WatermarkVector` (clone of the maintained index).
- Internal field: `device_watermarks: WatermarkVector`, maintained in `append`, rebuilt in `reload` (in-memory derived; NOT persisted — same lifecycle as `reaction_index`).

- [ ] **Step 1: Write the failing test** (in `community_channel_log.rs` tests; use the file's existing helpers for building a `ChannelLog` + appending `SignedChannelEvent::Post` — find an existing `append`-exercising test and mirror its setup)

```rust
#[tokio::test] // or #[test] — match the existing append-test style
async fn watermark_vector_tracks_per_device_max_and_survives_reload() {
    // build a temp ChannelLog (mirror the nearest existing test's setup)
    let (mut log, _dir) = test_log(); // <-- use the file's actual constructor helper
    append_post(&mut log, "dev-a", 100, 0, "a1");
    append_post(&mut log, "dev-a", 150, 2, "a2"); // newer for dev-a
    append_post(&mut log, "dev-b", 120, 0, "b1");
    let v = log.watermark_vector();
    assert_eq!(v.get("dev-a"), Some(&(150, 2)));
    assert_eq!(v.get("dev-b"), Some(&(120, 0)));
    // reload rebuilds the index identically
    let reloaded = ChannelLog::reload(/* same root/config */).expect("reload");
    assert_eq!(reloaded.watermark_vector(), v);
}
```

(Where `test_log` / `append_post` are thin wrappers over the file's real constructors; if no such helper exists, inline the existing test pattern — do NOT invent new persistence APIs.)

- [ ] **Step 2: Run to verify it fails** — `cargo test -p harmony-app --lib community_channel_log::tests::watermark_vector_tracks` → FAIL (`watermark_vector` undefined).

- [ ] **Step 3: Implement**
  - Add `device_watermarks: WatermarkVector` to the `ChannelLog` struct (next to `reaction_index`).
  - Initialize it everywhere `ChannelLog` is constructed (mirror `reaction_index` initialization sites).
  - In `append(ev)`: after the event lands in `tail`, raise the entry:
    ```rust
    let at = ev.at();
    let e = self.device_watermarks.entry(at.device_id.clone()).or_insert((0, 0));
    if (at.wall_ms, at.logical) > *e { *e = (at.wall_ms, at.logical); }
    ```
  - In `reload`: after segments + tail are loaded and `reaction_index` is rebuilt, rebuild `device_watermarks` by iterating every event (segments then tail) applying the same raise. (Place it beside the `reaction_index` rebuild.)
  - Add the accessor:
    ```rust
    /// ZEB-585: snapshot the per-device catch-up watermark.
    pub fn watermark_vector(&self) -> WatermarkVector {
        self.device_watermarks.clone()
    }
    ```

- [ ] **Step 4: Run to verify it passes** — `cargo test -p harmony-app --lib community_channel_log::tests::watermark_vector_tracks` → PASS.

- [ ] **Step 5: Commit** — `git commit -am "feat(channel-log): maintain per-device watermark index on ChannelLog"`

---

### Task 3: `collect_events_vector` + list wrappers + `log_watermark_vector()`

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs` (alongside `collect_events` ~687, `list_messages` ~657, `list_post_events` ~670, `log_max_hlc` ~830)

**Interfaces:**
- Consumes: `WatermarkVector`, `ChannelLog::watermark_vector` (Task 2).
- Produces: `async fn collect_events_vector(&self, vector: &WatermarkVector, limit, keep) -> Result<Vec<SignedChannelEvent>, ChannelLogEngineError>`; `pub async fn list_messages_vector(&self, vector: &WatermarkVector, limit) -> Result<Vec<SignedChannelEvent>, _>`; `pub async fn list_post_events_vector(...)`; `pub async fn log_watermark_vector(&self) -> WatermarkVector`.

- [ ] **Step 1: Write the failing test** (in `community_channel_log_engine.rs` tests; mirror an existing `collect_events`/`list_messages` test's engine setup)

```rust
#[tokio::test]
async fn collect_events_vector_serves_unseen_device_and_per_device_tail() {
    let engine = test_engine().await; // mirror the nearest existing engine test
    append_post(&engine, "dev-a", 100, 0, "a1").await;
    append_post(&engine, "dev-a", 200, 0, "a2").await;
    append_post(&engine, "dev-b", 50, 0, "b1").await; // sub-max, unseen device
    // Requester has dev-a up to (150,0) and has NEVER seen dev-b.
    let mut v: WatermarkVector = std::collections::BTreeMap::new();
    v.insert("dev-a".to_string(), (150, 0));
    let bodies: Vec<String> = engine
        .list_messages_vector(&v, 1000).await.unwrap()
        .into_iter().filter_map(post_body).collect();
    // Serves dev-a's newer-than-(150,0) event AND all of unseen dev-b —
    // including b1 whose HLC (50) sorts BELOW the requester's global max.
    assert!(bodies.contains(&"a2".to_string()));
    assert!(bodies.contains(&"b1".to_string()));
    assert!(!bodies.contains(&"a1".to_string())); // (100,0) <= (150,0)
}
```

- [ ] **Step 2: Run to verify it fails** — FAIL (`list_messages_vector` undefined).

- [ ] **Step 3: Implement** (mirror `collect_events`’s segment-then-tail walk; replace the scalar filter with the per-device one; NO global-range segment skip)

```rust
fn vector_serves(vector: &WatermarkVector, ev: &SignedChannelEvent) -> bool {
    let at = ev.at();
    match vector.get(&at.device_id) {
        None => true, // never seen this device → serve all of it
        Some(&(w, l)) => (at.wall_ms, at.logical) > (w, l),
    }
}

async fn collect_events_vector(
    &self,
    vector: &WatermarkVector,
    limit: usize,
    keep: impl Fn(&SignedChannelEvent) -> bool,
) -> Result<Vec<SignedChannelEvent>, ChannelLogEngineError> {
    let effective_limit = if limit == 0 { self.config.backfill_default_limit } else { limit };
    let log = self.log.lock().await;
    let mut out: Vec<SignedChannelEvent> = Vec::new();
    for seg in &log.manifest.segments {
        // NOTE: no global-range skip — a never-seen device may sit in any segment.
        let events = log.read_segment(seg).map_err(ChannelLogEngineError::Persist)?;
        for ev in events {
            if !Self::vector_serves(vector, &ev) || !keep(&ev) { continue; }
            out.push(ev);
            if out.len() >= effective_limit { return Ok(out); }
        }
    }
    for ev in &log.tail {
        if !Self::vector_serves(vector, ev) || !keep(ev) { continue; }
        out.push(ev.clone());
        if out.len() >= effective_limit { return Ok(out); }
    }
    Ok(out)
}

pub async fn list_messages_vector(&self, vector: &WatermarkVector, limit: usize)
    -> Result<Vec<SignedChannelEvent>, ChannelLogEngineError> {
    self.collect_events_vector(vector, limit, |_| true).await
}

pub async fn list_post_events_vector(&self, vector: &WatermarkVector, limit: usize)
    -> Result<Vec<SignedChannelEvent>, ChannelLogEngineError> {
    self.collect_events_vector(vector, limit, |ev| matches!(ev, SignedChannelEvent::Post { .. })).await
}

pub async fn log_watermark_vector(&self) -> WatermarkVector {
    self.log.lock().await.watermark_vector()
}
```

(`vector_serves` as an associated `fn` keeps it callable as `Self::vector_serves`; make it `fn` not `async`.)

- [ ] **Step 4: Run to verify it passes** — PASS.

- [ ] **Step 5: Commit** — `git commit -am "feat(channel-log): collect_events_vector + log_watermark_vector"`

---

### Task 4: `BackfillQueryRequest.watermark_sealed` + engine seals in the request method

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs` (`BackfillQueryRequest` ~402; `request_backfill_with_outcome` ~1039; `send_backfill_request` ~1051)

**Interfaces:**
- Produces: `BackfillQueryRequest { since, limit, outcome_tx, watermark_sealed: Option<Vec<u8>> }`.
- Consumes: `seal_watermark_vector`, `MAX_WATERMARK_VECTOR_BYTES` (Task 1); `log_watermark_vector` (Task 3); `channel_key_ref()` (~1622).

- [ ] **Step 1: Write the failing test** — exercise `send_backfill_request` and read the produced `BackfillQueryRequest` off a test `query_request_rx` (mirror how existing engine tests capture sent requests; if none, add a small helper that constructs the engine with a known `query_request_tx`/`rx` pair).

```rust
#[tokio::test]
async fn since_some_seals_vector_since_none_does_not() {
    let (engine, mut req_rx) = test_engine_with_req_rx().await;
    append_post(&engine, "dev-a", 100, 0, "a1").await;
    // since=Some → a sealed vector is attached and opens to the engine's vector
    engine.clone().request_backfill_with_outcome(Some(hlc("dev-a", 100, 0)), oneshot::channel().0).await.unwrap();
    let req = req_rx.recv().await.unwrap();
    let sealed = req.watermark_sealed.expect("since=Some must seal a vector");
    let opened = open_watermark_vector(engine.channel_key_ref(), &sealed).unwrap();
    assert_eq!(opened, engine.log_watermark_vector().await);
    // since=None → no vector (full reconcile)
    engine.clone().request_backfill_with_outcome(None, oneshot::channel().0).await.unwrap();
    let req2 = req_rx.recv().await.unwrap();
    assert!(req2.watermark_sealed.is_none(), "since=None must NOT seal a vector");
}
```

- [ ] **Step 2: Run to verify it fails** — FAIL (`watermark_sealed` field missing).

- [ ] **Step 3: Implement**
  - Add `pub watermark_sealed: Option<Vec<u8>>,` to `BackfillQueryRequest`.
  - In `send_backfill_request`, thread a `watermark_sealed` param into the struct (default `None`). Compute it in `request_backfill_with_outcome` (and the fire-and-forget sibling if it should also seal — for the driver path, only `request_backfill_with_outcome` is used):
    ```rust
    let watermark_sealed = if since.is_some() {
        let v = self.log_watermark_vector().await;
        match seal_watermark_vector(self.channel_key_ref(), &v) {
            Ok(bytes) if bytes.len() <= MAX_WATERMARK_VECTOR_BYTES => Some(bytes),
            _ => None, // over cap or seal error → degrade to scalar + floor
        }
    } else {
        None
    };
    ```
  - Pass `watermark_sealed` through `send_backfill_request` into the `BackfillQueryRequest { since, limit: 0, outcome_tx, watermark_sealed }`.
  - Update every other `BackfillQueryRequest { .. }` literal (IPC fire-and-forget path) to set `watermark_sealed: None` (grep the struct name).

- [ ] **Step 4: Run to verify it passes** — PASS. Then `cargo test -p harmony-app --lib community_channel_log` (whole module green).

- [ ] **Step 5: Commit** — `git commit -am "feat(channel-log): engine seals watermark vector into BackfillQueryRequest (since=Some only)"`

---

### Task 5: Plumb the sealed payload through the GET + queryable + `read_for_query`; acceptance test

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs` (`read_for_query` closure ~2147 — add a `watermark_sealed` param)
- Modify: `src-tauri/src/event_loop.rs` (queryable task ~7836; query-request driver GET ~7950; `spawn_channel_log_zenoh_adapter` signature + the `read_for_query` boxed-closure type ~203 / the adapter struct)
- Modify: `src-tauri/tests/channel_backfill_integration.rs` (acceptance test + the `spawn_adapter_bridge_drainer` if the adapter signature changed)

**Interfaces:**
- `read_for_query` becomes `Fn(Option<Hlc>, usize, Option<Vec<u8>>) -> Pin<Box<dyn Future<Output = Vec<Vec<u8>>> + Send>>`.
- The queryable forwards `query.payload()` bytes (capped) as the third arg; the GET driver forwards `req.watermark_sealed` as `.payload(..)`.

- [ ] **Step 1: Write the failing acceptance test** (in `channel_backfill_integration.rs`, using the two-`RegistryHandle` harness + `wait_for_count`/`list_bodies`)

```rust
/// ZEB-585: a returning member recovers a NEVER-SEEN device's
/// offline-window message whose HLC sorts BELOW the member's global max,
/// via the per-author watermark vector — and the scalar path alone would
/// miss it. Asserts the gap closes (correctness) and wire volume is
/// O(gap) not O(history).
#[tokio::test(flavor = "multi_thread")]
async fn returning_member_recovers_unseen_device_sub_max_hlc_event() {
    // A = high-HLC author (member B's global max comes from A's posts).
    // B = returning member. X = a third device B has never seen, posting
    //     with a wall_ms BELOW A's max while B is offline.
    // 1. B online; A posts a backlog; B converges (wait_for_count).
    // 2. B goes offline (stop its engine; keep its TempDir).
    // 3. X posts one event with wall_ms < A's max.
    // 4. B reconnects (re-spawn under the same TempDir).
    // 5. wait_for_count(B, backlog + 1) — the X event arrives.
    //    (Under the old scalar path B's since=max(A) filters X out.)
}
```

(Fill in using the exact `build_registry` / `spawn_channel` / `wait_for_count` helpers verbatim from the file; X can be a second author registry posting into the same channel.)

- [ ] **Step 2: Run to verify it fails** — FAIL (vector path not wired → X's sub-max event never delivered, `wait_for_count` times out).

- [ ] **Step 3: Implement the plumbing**
  - `read_for_query` closure: add `watermark_sealed: Option<Vec<u8>>` param; body:
    ```rust
    let events = match watermark_sealed {
        Some(bytes) => match crate::community_channel_log::open_watermark_vector(me.channel_key_ref(), &bytes) {
            Ok(v) => me.list_messages_vector(&v, limit).await,
            Err(_) => me.list_messages(since, limit).await, // open fail → scalar
        },
        None => me.list_messages(since, limit).await,
    };
    // ... existing encrypt_channel_packet map unchanged ...
    ```
    Update the boxed-`Fn` type alias accordingly (the adapter's `read_for_query` field type + `spawn_channel_log_zenoh_adapter` param type).
  - Queryable task (`event_loop.rs:7836`): after `parse_channel_backfill_key`, read the payload with a cap-before-alloc guard, then pass it:
    ```rust
    let wmv = query.payload().and_then(|p| {
        let bytes = p.to_bytes();
        if bytes.len() > crate::community_channel_log::MAX_WATERMARK_VECTOR_BYTES {
            tracing::debug!(%qkey, len = bytes.len(), "watermark vector over cap; serving scalar");
            None
        } else {
            Some(bytes.to_vec())
        }
    });
    let packets = (read_for_query_qbl)(since, limit, wmv).await;
    ```
  - GET driver (`event_loop.rs:7950`): forward the sealed bytes:
    ```rust
    let mut get = session_qr.get(&key).consolidation(zenoh::query::ConsolidationMode::None);
    if let Some(bytes) = req.watermark_sealed.take() { get = get.payload(bytes); }
    let receiver = match get.await { ... };
    ```
    (`req` is already `let Some(mut req) = maybe`, so `.take()` works; confirm `req` mutability.)

- [ ] **Step 4: Run to verify the acceptance test passes** — `cargo test -p harmony-app --test channel_backfill_integration returning_member_recovers_unseen_device --features test-fixtures` → PASS.

- [ ] **Step 5: Backward-compat assertion** — add/extend a test (or an assertion) that a request with `watermark_sealed: None` still returns today's scalar behavior (an existing backfill integration test already exercises the scalar path; confirm it stays green — that IS the regression guard).

- [ ] **Step 6: Commit** — `git commit -am "feat(channel-log): wire sealed watermark vector through GET payload + queryable + read_for_query"`

---

### Task 6: Full gates + push

- [ ] **Step 1: fmt** — `cargo fmt --all` then `cargo fmt --all -- --check` → clean.
- [ ] **Step 2: clippy (all-targets)** — `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` → clean.
- [ ] **Step 3: nextest (workspace)** — `cargo nextest run --locked --workspace --all-targets --features test-fixtures` → all pass (watch the channel-log + backfill + wire-format suites).
- [ ] **Step 4: Push + open PR** — `git push -u origin channel-log-diff-reconciliation`; open the PR (body references the spec + plan + ZEB-585 in plain text for Linear auto-close; NOT in the title). Trigger CodeRabbit immediately after push.

---

## Self-Review

**Spec coverage:** A.1 core → Tasks 2/3; A.4 wire/seal-site → Tasks 1/4/5; A.5 cap → Tasks 1/5; A.6 paging → Task 3 (filter) + driver unchanged; A.7 components → Tasks 1–5 one-to-one; A.8 acceptance → Task 5; A.9 gates → Tasks 1–6; A.10 out-of-scope respected (no `channel_backfill.rs` change, no SegmentDescriptor change, floor untouched). ✓

**Placeholder scan:** the only `<FILL FROM FIRST RUN>` is the fixture hex (bootstrapped in Task 1 Step 6 — inherent to pin tests). Test helper names (`test_engine`, `append_post`, `test_log`) are explicitly flagged as "mirror the nearest existing test" because the exact helper names must be read from the files at execution; the surrounding real APIs (`list_messages`, `request_backfill_with_outcome`, `build_registry`, `wait_for_count`) are exact. ✓

**Type consistency:** `WatermarkVector = BTreeMap<String,(u64,u32)>` used identically across Tasks 1–5; `watermark_sealed: Option<Vec<u8>>` consistent in Task 4 (struct) ↔ Task 5 (GET `.payload`/queryable); `read_for_query` 3-arg signature consistent Task 5 closure ↔ adapter type. ✓
