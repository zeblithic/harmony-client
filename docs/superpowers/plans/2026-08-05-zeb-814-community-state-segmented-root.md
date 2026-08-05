# ZEB-814 Segmented Community-State Root — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task (inline execution chosen). Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the monolithic ≤1 MiB community-state root blob with a small **manifest** referencing immutable, content-addressed **segments**, so publish/serve/bootstrap are per-publisher O(delta) and no fixed-size cliff bounds community growth.

**Architecture:** A new pure module `community_state_segments.rs` holds the segment/manifest types, the cascade-free partition+seal logic, and the encode/decrypt helpers. `community_state_sync.rs` wires it into the publish/serve encoder and the receive/bootstrap decoder behind a signed `"mf"` format discriminator (dual-read of legacy monolithic roots). `community_state_persist.rs` gains a `segments.cbor` sidecar giving per-publisher segment-CID stability. Local `crdt.cbor` is unchanged (the 1 MiB cliff is CAS-only).

**Tech Stack:** Rust, ciborium canonical CBOR, ChaCha20-Poly1305 (`encrypt_blob`/`decrypt_blob`), `harmony_content::cid::ContentId`, tokio, cargo-nextest.

## Global Constraints

- Spec of record: `docs/superpowers/specs/2026-08-05-zeb-814-community-state-segmented-root-design.md`. Every task implicitly includes it.
- **Scope tier: MVP — per-publisher O(delta).** Non-goals (do NOT build): strict cross-peer deterministic dedup, RBSR tail-reconcile, manifest chunking, local-storage segmentation.
- Seal thresholds (approved): `SEGMENT_SEAL_EVENTS = 512`, `SEGMENT_SEAL_BYTES = 256 * 1024`.
- Migration is **flag-day + dual-read** (approved): receivers decode both legacy monolithic roots (`"mf"` absent) and manifests (`"mf" = Some(1)`); publishers emit manifests only after this lands. No data migration.
- **Crypto reuse verbatim:** manifest encrypts under the current epoch key via `encrypt_blob`; segments encrypt under a random per-segment `EpochKey` (`K_s`). `K_s` lives in plaintext *inside* the epoch-encrypted manifest — no new key-wrap primitive.
- **Segmentation is a transport partition only.** Replay always re-sorts every event by the full `event_sort_key` (`community_membership.rs:2260`), so a mis-assigned event is harmless to correctness. Segment boundaries are pinned `(wall_ms, logical, device_id, EventId)` cut points (sig omitted) → a backdated event re-seals exactly one segment, never a cascade.
- CBOR field keys on new maps are uniform 2-char codes (codebase convention; keeps canonical ordering unambiguous).
- Gates (run from `src-tauri/`): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; scoped tests via `cargo nextest run --locked --features test-fixtures -E 'test(<name>)'`; final full sweep `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.

---

### Task 1: Pure segment/manifest core (`community_state_segments.rs`)

**Files:**
- Create: `src-tauri/src/community_state_segments.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod community_state_segments;` near the other `community_state_*` module declarations)

**Interfaces — Produces (later tasks rely on these exact names/types):**

```rust
// Tunables (Global Constraints).
pub const SEGMENT_SEAL_EVENTS: usize = 512;
pub const SEGMENT_SEAL_BYTES: usize = 256 * 1024;
pub const MANIFEST_FORMAT_V1: u8 = 1;   // the "mf" discriminator value

// Pinned cut point in event_sort_key order (sig omitted — see Global Constraints).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EventBoundary {
    #[serde(rename = "wm")] pub wall_ms: u64,
    #[serde(rename = "lg")] pub logical: u32,
    #[serde(rename = "dv")] pub device_id: String,
    #[serde(rename = "id")] pub id: crate::community_membership::EventId,
}
impl EventBoundary {
    pub fn of(e: &crate::community_membership::SignedMembershipEvent) -> Self;
    // Total order matching event_sort_key minus sig.
    pub fn key(&self) -> (u64, u32, &str, &crate::community_membership::EventId);
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SegmentRef {
    #[serde(rename = "sc")] pub segment_cid: harmony_content::cid::ContentId,
    #[serde(rename = "lo")] pub lo: EventBoundary,   // first event
    #[serde(rename = "hi")] pub hi: EventBoundary,   // last event
    #[serde(rename = "nn")] pub count: u32,
    #[serde(rename = "ks", with = "crate::community_state_sync::bstr32")] pub k_s: [u8; 32],
}

// Persisted-in-manifest cleartext (encrypted under the current epoch key).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManifestCleartext {
    #[serde(rename = "vn")] pub version: u16,             // = 1
    #[serde(rename = "ci")] pub community_id: crate::owner_state_types::SpaceId,
    #[serde(rename = "sg")] pub segments: Vec<SegmentRef>,
    #[serde(rename = "tl")] pub tail: Vec<crate::community_membership::SignedMembershipEvent>,
}

// Per-segment cleartext (encrypted under its K_s).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SegmentCleartext {
    #[serde(rename = "vn")] pub version: u16,             // = 1
    #[serde(rename = "ci")] pub community_id: crate::owner_state_types::SpaceId,
    #[serde(rename = "ev")] pub events: Vec<crate::community_membership::SignedMembershipEvent>,
}

// Sidecar entry (persisted locally; see Task 2).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SealedEntry {
    #[serde(rename = "lo")] pub lo: EventBoundary,
    #[serde(rename = "hi")] pub hi: EventBoundary,
    #[serde(rename = "nn")] pub count: u32,
    #[serde(rename = "ks", with = "crate::community_state_sync::bstr32")] pub k_s: [u8; 32],
    #[serde(rename = "sc")] pub segment_cid: harmony_content::cid::ContentId,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SegmentIndex {
    #[serde(rename = "vn")] pub version: u16,             // = 1 when non-empty
    #[serde(rename = "sg")] pub sealed: Vec<SealedEntry>, // ascending by lo
}

/// Output of a publish-time re-derivation over the current sorted log.
pub struct SegmentPlan {
    pub refs: Vec<SegmentRef>,                 // for the manifest, ascending
    pub tail: Vec<crate::community_membership::SignedMembershipEvent>,
    pub index: SegmentIndex,                   // to persist (Task 2)
    pub newly_sealed: Vec<(SegmentRef, Vec<u8>)>, // (ref, segment_ciphertext) to put_serveable
}

/// Deterministically (re)partition `sorted_events` (ascending event_sort_key,
/// deduped) against a prior `index`, sealing new/dirty segments. `new_k_s`
/// supplies fresh 32-byte keys for segments sealed this call. Pure + sync.
pub fn plan_segments(
    community_id: crate::owner_state_types::SpaceId,
    sorted_events: &[crate::community_membership::SignedMembershipEvent],
    prior: &SegmentIndex,
    new_k_s: &mut dyn FnMut() -> [u8; 32],
) -> Result<SegmentPlan, SegmentError>;

/// Encrypt+CID a segment's events under `k_s`. Returns (cid, ciphertext).
pub fn seal_segment(
    community_id: crate::owner_state_types::SpaceId,
    events: &[crate::community_membership::SignedMembershipEvent],
    k_s: &[u8; 32],
) -> Result<(harmony_content::cid::ContentId, Vec<u8>), SegmentError>;

/// Encrypt+CID the manifest cleartext under the current epoch key.
pub fn seal_manifest(
    epoch_key: &crate::owner_state_crypto::EpochKey,
    manifest: &ManifestCleartext,
) -> Result<(harmony_content::cid::ContentId, Vec<u8>), SegmentError>;

/// Decrypt a manifest blob (current epoch key). Verifies community_id.
pub fn open_manifest(
    epoch_key: &crate::owner_state_crypto::EpochKey,
    expected_community: crate::owner_state_types::SpaceId,
    ciphertext: &[u8],
) -> Result<ManifestCleartext, SegmentError>;

/// Decrypt a segment blob under its K_s. Verifies community_id.
pub fn open_segment(
    k_s: &[u8; 32],
    expected_community: crate::owner_state_types::SpaceId,
    ciphertext: &[u8],
) -> Result<Vec<crate::community_membership::SignedMembershipEvent>, SegmentError>;

#[derive(thiserror::Error, Debug)]
pub enum SegmentError {
    #[error("CBOR encode: {0}")] CborEncode(String),
    #[error("CBOR decode: {0}")] CborDecode(String),
    #[error("crypto: {0}")] Crypto(#[from] crate::community_state_crdt::CommunityCryptoError),
    #[error("ContentId: {0}")] ContentId(String),
    #[error("misrouted segment/manifest: expected {expected:?} found {found:?}")]
    Misrouted { expected: crate::owner_state_types::SpaceId, found: crate::owner_state_types::SpaceId },
}
```

> Note on the `bstr32` helper: `[u8;32]` needs the same `serialize_bytes_as_bstr`/`deserialize_bytes_from_bstr` treatment the payload's `[u8;64]` sig uses (`community_state_sync.rs:246`). Add a `pub(crate) mod bstr32` in `community_state_sync.rs` mirroring that pair for 32-byte arrays, or a local serde-with in this module. Confirm `EpochKey` exposes a byte constructor (it's loaded from `old_epoch_keys` on disk, so a `from_bytes`/`TryFrom<[u8;32]>` path exists) for wrapping `K_s`.

**`plan_segments` algorithm (the load-bearing pure logic):**
1. Walk `prior.sealed` in order. For each sealed entry, collect from `sorted_events` the contiguous run whose `EventBoundary::of` key is in `[entry.lo.key(), entry.hi.key()]` inclusive.
   - If that run's events are byte-identical to what the entry represents (same count AND same first/last boundary AND no new event fell inside the `[lo,hi]` interval), **reuse** `entry.k_s`/`entry.segment_cid` unchanged → contributes to `refs`, NOT to `newly_sealed`.
   - If a new event fell inside `[lo,hi]` (count differs), **re-seal that interval's region** through the shared bounded-chunking helper (`seal_chunks`, `seal_remainder = true`): the first replacement chunk reuses `entry.k_s`, and if the interval grew past `SEGMENT_SEAL_EVENTS`/`SEGMENT_SEAL_BYTES` it is **split into bounded replacement segments** (extra chunks get fresh `new_k_s()`) so no single segment can exceed the threshold. Each replacement contributes to both `refs` and `newly_sealed`.
2. The remaining suffix of `sorted_events` (keys strictly greater than the last sealed `hi`) is the **tail candidate**. Run the same `seal_chunks` helper (`seal_remainder = false`, fresh `new_k_s()` per chunk): it cuts leading threshold-bounded segments while the remaining run is `≥ SEGMENT_SEAL_EVENTS` events OR `≥ SEGMENT_SEAL_BYTES` cleartext bytes, maintaining the running byte total incrementally (O(run), not a per-iteration suffix sum), and leaves the sub-threshold remainder.
3. Whatever remains is `tail`. Assemble `SegmentPlan { refs, tail, index (rebuilt from refs+keys), newly_sealed }`.

Caller contract: `plan_segments` requires `sorted_events` ascending by `event_sort_key` and deduplicated — callers sort (it does NOT sort internally).

Invariant to preserve: sealed `[lo,hi]` intervals are contiguous and disjoint over the total order; the tail is strictly `> last hi`; **every** sealed segment (first-seal or backdated re-seal) respects the seal thresholds. A backdated event with a key inside a sealed interval routes to exactly that interval (step 1 re-seal, splitting if oversized), never shifting later intervals (their `lo`/`hi` are pinned).

- [ ] **Step 1: Write failing tests** — create `#[cfg(test)] mod tests` in `community_state_segments.rs`. Use a helper `ev(wall_ms, logical, dev, id_byte) -> SignedMembershipEvent` building a minimal signed Leave event (deterministic, no enrollment cert), and a fixed `k_s = [7u8;32]`. Tests:
  - `roundtrip_segment` — `seal_segment` then `open_segment` returns the same events; wrong community_id → `Misrouted`.
  - `roundtrip_manifest` — `seal_manifest`/`open_manifest` roundtrip under a fixed `EpochKey`; misroute rejected.
  - `plan_empty_log_has_no_segments_only_tail` — empty prior + a handful of events under threshold → `refs` empty, `tail` == all events, `newly_sealed` empty.
  - `plan_seals_at_event_threshold` — feed `SEGMENT_SEAL_EVENTS + 5` ascending events, empty prior → exactly one `SegmentRef` of `SEGMENT_SEAL_EVENTS`, tail of 5, one `newly_sealed`.
  - `plan_reuses_prior_segments_cid_stable` — run `plan_segments`, persist its `index`, append 3 new tail events, run again → the prior segment's `segment_cid` is byte-identical and NOT in `newly_sealed` (per-publisher O(delta)).
  - `plan_backdated_event_reseals_affected_interval_no_cascade` — with ≥2 sealed segments, inject an event whose key falls inside segment 0's `[lo,hi]` → segment 0's region re-seals (its old cid is gone); a LATER segment keeps its cid and is NOT in `newly_sealed` (cascade-free, asserted by identity since a re-seal may split); every segment ≤ `SEGMENT_SEAL_EVENTS`.
  - `plan_dirty_reseal_stays_under_seal_threshold` — bulk-backdate ~2 segments' worth of events into one sealed interval → it splits into multiple bounded segments (none over threshold) and reconstruction recovers every event.
  - `plan_caller_side_resort_is_deterministic` — sorting a differently-ordered copy of the same events back into `event_sort_key` order yields byte-identical `refs`/`tail` (pins the caller-side sort contract; the planner does NOT sort internally).

- [ ] **Step 2: Run tests to verify they fail** — `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_state_segments)'` → FAIL (module/functions absent).

- [ ] **Step 3: Implement the module** — types above; `EventBoundary::of`/`key`; `seal_segment`/`seal_manifest` (canonical_cbor_encode → `encrypt_blob` → `ContentId::for_book(ct, ContentFlags{encrypted:true,..})`); `open_segment`/`open_manifest` (`decrypt_blob` → `canonical_cbor_decode` → community_id check); `plan_segments` per the algorithm. Add the module to `lib.rs`.

- [ ] **Step 4: Run tests to verify they pass** — same nextest filter → PASS.

- [ ] **Step 5: Gate + commit**
```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/src/community_state_segments.rs src-tauri/src/community_state_sync.rs src-tauri/src/lib.rs
git commit -m "feat(ZEB-814): segment/manifest core — types, cascade-free plan_segments, seal/open"
```

---

### Task 2: `segments.cbor` sidecar persistence (`community_state_persist.rs`)

**Files:**
- Modify: `src-tauri/src/community_state_persist.rs` (add `save_segment_index`/`load_segment_index` mirroring `save_crdt`/`load_crdt`, reusing `write_atomic` + `quarantine_corrupted`)

**Interfaces — Consumes:** `SegmentIndex` (Task 1). **Produces:**
```rust
pub fn save_segment_index(path: &Path, index: &SegmentIndex) -> Result<(), PersistError>;
/// Missing file → SegmentIndex::default(); decode error → quarantine + default
/// (self-heal: a lost sidecar only costs one O(total) re-upload, receivers still decode).
pub fn load_segment_index(path: &Path) -> Result<SegmentIndex, PersistError>;
```

- [ ] **Step 1: Write failing tests** — in the module's `#[cfg(test)] mod tests` (uses `tempfile`): `segment_index_roundtrip` (save then load equals input), `segment_index_missing_file_is_default` (load a nonexistent path → `SegmentIndex::default()`), `segment_index_corrupt_quarantines_and_defaults` (write garbage bytes, load → default + a `.corrupt.*` sibling exists).
- [ ] **Step 2: Run to verify fail** — `cargo nextest run --locked --features test-fixtures -E 'test(segment_index)'` → FAIL.
- [ ] **Step 3: Implement** — the two functions delegating to `canonical_cbor_encode`/`write_atomic` and `std::fs::read`/`canonical_cbor_decode`/`quarantine_corrupted`, exactly patterning `save_crdt`/`load_crdt` (no community_id-mismatch arm — the sidecar isn't community-tagged; it lives under the community's own directory).
- [ ] **Step 4: Run to verify pass** → PASS.
- [ ] **Step 5: Gate + commit**
```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/src/community_state_persist.rs
git commit -m "feat(ZEB-814): segments.cbor sidecar (save/load, atomic + quarantine self-heal)"
```

---

### Task 3: Publish/serve wiring + `"mf"` discriminator + watermarks (`community_state_sync.rs`)

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` — the encoder (`encode_root_packet` `:3106`, `publish_root_now` `:3320`, serve arm `:2869`), the payload structs (`:224`, `:270`), `classify_root_size`/watermark block (`:3185`, `:4715`), and `InternalCtx` (thread the sidecar path + a way to load/save the index).

**Interfaces — Consumes:** `plan_segments`, `seal_manifest`, `SegmentIndex`, `MANIFEST_FORMAT_V1` (Task 1); `save_segment_index`/`load_segment_index` (Task 2). **Produces:** every published root is now a manifest CID; `CommunityRootPublishPayload`/`CommunityRootSignedPayload` carry `"mf": Option<u8>`.

Concrete changes:
1. **Payload discriminator.** Add `#[serde(rename = "mf", skip_serializing_if = "Option::is_none", default)] pub manifest_format: Option<u8>` to **both** `CommunityRootPublishPayload` and `CommunityRootSignedPayload`. Thread it through `into_wire` (add a `manifest_format` param) and the signing site (`:3282-3307`) so the format is signed. `None` for any legacy-shaped construction; `Some(MANIFEST_FORMAT_V1)` for the new encoder. (Absent ⇒ byte-identical to pinned legacy fixtures — verified in Task 5.)
2. **Encoder rewrite.** After the existing TOCTOU epoch-recheck loop yields `(current_key, current_epoch, snapshot)` (`:3127-3173`), replace steps 1–3 (the whole-blob encode/encrypt/`for_book` at `:3175-3269`) with:
   - `let mut sorted = snapshot.events().cloned().collect::<Vec<_>>(); sorted.sort_by(|a,b| event_sort_key(a).cmp(&event_sort_key(b)));`
   - `let prior = load_segment_index(&ctx.segments_path)?;`
   - `let plan = community_state_segments::plan_segments(ctx.community_id, &sorted, &prior, &mut || random_32())?;`
   - `for (r, ct) in &plan.newly_sealed { ctx.content_store.put_serveable(&r.segment_cid, ct).await?; }`
   - `let manifest = ManifestCleartext { version: 1, community_id: ctx.community_id, segments: plan.refs, tail: plan.tail };`
   - `let (root_cid, manifest_ct) = seal_manifest(&current_key, &manifest)?;`
   - `ctx.content_store.put_serveable(&root_cid, &manifest_ct).await?;` (mirror the existing `put_serveable` call the old blob used at `:3271+`).
   - `save_segment_index(&ctx.segments_path, &plan.index)?;`
   - Build/sign the payload with `manifest_format: Some(MANIFEST_FORMAT_V1)` and `root_cid`.
3. **Watermarks.** Re-point the `:3185-3241` block: classify on `manifest.segments.len()` against the `SegmentRef` capacity (`MAX_PAYLOAD_SIZE / approx_ref_bytes`, a new `const MANIFEST_SEGMENT_CAP`), keeping the `RootSizeWatermark`/`report_degraded` transitions. Change the warn copy to reference segments-vs-manifest-cap. (`classify_root_size` can stay for a byte check on the sealed manifest ciphertext as defense-in-depth.)
4. **`InternalCtx`.** Add `segments_path: PathBuf` (derived beside `crdt.cbor`, the `{community_id_hex}/segments.cbor`); populate at engine spawn where `crdt`/`replay` paths are built.

- [ ] **Step 1: Write failing test(s)** — a new integration-ish unit in `community_state_sync.rs` tests: `manifest_publish_roundtrips_through_receive` is deferred to Task 4; here add `encode_root_packet_produces_manifest_format` — drive the encoder against a small in-memory `InternalCtx` (reuse existing test scaffolding for the engine ctx; if none is unit-reachable, assert at the `plan_segments`+`seal_manifest` seam that a published payload carries `manifest_format == Some(1)` and `root_cid` decodes via `open_manifest` back to the snapshot's events sorted). Also `publish_twice_no_change_is_cid_stable` — encode twice with no new events → identical `root_cid`.
- [ ] **Step 2: Run to verify fail** — `cargo nextest run --locked --features test-fixtures -E 'test(manifest)' ` → FAIL.
- [ ] **Step 3: Implement** the four changes above.
- [ ] **Step 4: Run to verify pass** → PASS; then scoped regression `cargo nextest run --locked --features test-fixtures -E 'test(community_state_sync)'`.
- [ ] **Step 5: Gate + commit**
```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/src/community_state_sync.rs
git commit -m "feat(ZEB-814): publish/serve emit a manifest root (mf discriminator, segment put, watermark re-point)"
```

---

### Task 4: Receive/bootstrap dual-read (`community_state_sync.rs`)

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` — the receive path around the blob fetch/decrypt/decode (`:4086-4188`) inside `handle_incoming_publish`.

**Interfaces — Consumes:** `open_manifest`, `open_segment` (Task 1); `payload.manifest_format` (Task 3).

Concrete change: after `root_key_used` is known (the epoch key that opened the wire packet), branch on `payload.manifest_format`:
- **`Some(MANIFEST_FORMAT_V1)`** → fetch `payload.root_cid` (the manifest) via the existing `get_with_budget` (keep the FetchMiss/error retry arms verbatim); `open_manifest(root_key_used, ctx.community_id, &manifest_ct)?`; for each `SegmentRef`, `get_with_budget(&r.segment_cid, …)` (a segment miss → `FetchMiss(wire)`, same retry-safe pre-mutation semantics), `open_segment(&r.k_s, ctx.community_id, &seg_ct)?`; collect all sealed events + `manifest.tail` into `resolved`. Then continue **unchanged** from the existing `resolved.sort_by(event_sort_key)` (`:4185`) through bootstrap-admit, insert, and tracker commit.
- **`None`** → the existing legacy monolithic path verbatim (decrypt blob → decode `CommunityState` → `into_events()`).

Keep the misroute guard (manifest/segment `community_id` == `ctx.community_id`, mirroring `:4157`). All post-`resolved` logic is shared between both branches.

- [ ] **Step 1: Write failing test** — `manifest_bootstrap_parity_with_monolithic`: build a `CommunityState` with a mixed log (some Joins-with-cert, Leaves, SetPowers spanning >1 seal boundary); encode it via the Task-3 manifest encoder AND via a legacy monolithic encode; run each through `handle_incoming_publish` (or the shared decode seam) into a fresh engine; assert the two materialized `CommunityState`s are byte-identical (`canonical_cbor_encode` equal). Add `receive_rejects_misrouted_segment` (a segment with a foreign community_id → pre-mutation error, no state change) and `dual_read_decodes_legacy_and_manifest` (one of each `mf` value decodes).
- [ ] **Step 2: Run to verify fail** → FAIL.
- [ ] **Step 3: Implement** the branch.
- [ ] **Step 4: Run to verify pass** → PASS; scoped regression `cargo nextest run --locked --features test-fixtures -E 'test(community_state_sync)'`.
- [ ] **Step 5: Gate + commit**
```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/src/community_state_sync.rs
git commit -m "feat(ZEB-814): receive path dual-reads manifest roots (fetch segments + replay) and legacy blobs"
```

---

### Task 5: Wire-format fixtures + full sweep

**Files:**
- Modify/Create: `src-tauri/tests/wire_format/community_sync_fixtures.rs` (+ a new `community_segment_fixtures.rs` if cleaner) — byte-pins for `SegmentRef`, `ManifestCleartext`, `SegmentCleartext`, `SegmentIndex`, and the `"mf"`-present `CommunityRootPublishPayload`/`CommunityRootSignedPayload`.

**Interfaces — Consumes:** all Task-1 types + Task-3 payload fields.

- [ ] **Step 1: Write pinning tests** — for each new type, construct a fixed instance (deterministic fields), `canonical_cbor_encode`, and assert the hex against an inline constant (mirror the existing `community_sync_fixtures.rs:37-49` style). Add `legacy_publish_payload_bytes_unchanged`: a `CommunityRootPublishPayload` with `manifest_format: None` encodes to the **existing** pinned legacy hex (proves byte-compat). Add `manifest_publish_payload_pins`: with `manifest_format: Some(1)`, pin the new hex.
- [ ] **Step 2: Run to verify** — generate the expected hex by running once with a `dbg!`/failing assert, capture, then pin (standard fixture bootstrap). `cargo nextest run --locked --features test-fixtures -E 'test(fixture)'`.
- [ ] **Step 3: Confirm legacy fixtures untouched** — run the full existing wire-format suite: `cargo nextest run --locked --features test-fixtures -E 'test(wire_format)'` → all prior pins (crdt.cbor / CommunityState / zeb250 / zeb285) green, unchanged.
- [ ] **Step 4: Full CI-parity sweep**
```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: green. (Warm-cache lib change relinks integ binaries — allow ~50 min; supervise per the long-running-background rule.)
- [ ] **Step 5: Commit**
```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/tests/
git commit -m "test(ZEB-814): wire-format byte-pins for segment/manifest + mf-present envelope; legacy pins unchanged"
```

---

## Self-Review

**Spec coverage:** §4.1 formats → Task 1 types. §4.2 envelope crypto → Task 1 `seal_manifest`/`seal_segment` + Task 3 (manifest under epoch key, K_s inside). §4.3 seal policy → Task 1 `plan_segments` + its cascade test. §4.4 sidecar → Task 2. §4.5 publish/serve → Task 3; receive/bootstrap → Task 4. §5 `"mf"` under signature → Task 3 step 1. §6 dual-read → Task 4. §8 health surface → Task 3 step 3. §9 tests → distributed across Task 1/3/4/5 (per-publisher CID stability, O(delta), backdated single-segment re-seal, rotation-touches-manifest-only via `roundtrip_manifest`+cid-stability, bootstrap parity, dual-read, fixtures, sidecar recovery). §7 non-goals → excluded. **Gap check:** the "epoch rotation touches manifest only, segment CIDs unchanged" property is implied by Task-1 `seal_segment` (K_s-keyed, epoch-independent) but not directly asserted end-to-end — **added** to Task 4 step 1 as an explicit `rotation_reseals_manifest_not_segments` assertion (re-run the Task-3 encoder under a different `current_key`; every `segment_cid` unchanged, `root_cid` changed).

**Placeholder scan:** thresholds are concrete (512 / 256 KiB); all functions have real signatures; test assertions are concrete. `MANIFEST_SEGMENT_CAP` and `bstr32`/`EpochKey::from_bytes` are named as the two things to confirm-and-wire during Task 1/3, not left vague.

**Type consistency:** `SegmentRef`, `ManifestCleartext`, `SegmentCleartext`, `SegmentIndex`, `SealedEntry`, `EventBoundary`, `plan_segments`, `seal_segment`, `seal_manifest`, `open_manifest`, `open_segment`, `MANIFEST_FORMAT_V1`, `manifest_format` are used identically across Tasks 1→5.
