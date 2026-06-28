# Channel-log RBSR (range-based set reconciliation) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace channel-log catch-up's per-author watermark vector with range-based set reconciliation that transfers only the symmetric difference between two peers, independent of clock skew, arrival order, or device count.

**Architecture:** A new pure-logic module `channel_rbsr.rs` holds the protocol (canonical ordering, count-folded range fingerprints, pull-only bisection state machine) behind a `RangeReconcileSource` trait. A new `channel_chunk_index.rs` holds an in-memory content-defined-chunk fingerprint index that makes range-fingerprint queries O(log n). `ChannelLog` owns the index, feeds it in `append`/`reload`, and implements `RangeReconcileSource`. The engine seals/opens RBSR messages with the channel key and drives rounds; `event_loop.rs` adds a dedicated `rbsr/**` Zenoh queryable + GET driver; `channel_backfill.rs` adds an RBSR driver mode with vector-path fallback. Coexists with the Part A vector path; retiring it is a follow-up.

**Tech Stack:** Rust, Tauri, Zenoh 1.9.0 (GET-with-payload, `ConsolidationMode::None`), ChaCha20-Poly1305 AEAD, SHA-256 (sha2), ciborium (canonical CBOR), tokio.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-06-28-zeb-592-channel-log-rbsr-design.md` (all decisions verbatim there).
- Preserve every shipped wire byte: the `…/since/**` key family, the Part A watermark-vector payload, the periodic full-reconcile floor, and `SegmentDescriptor` on-disk format are **unchanged**. RBSR is purely additive.
- AEAD domain separation: RBSR AAD is `b"harmony-channel-rbsr-v1"` — distinct from `b"harmony-channel-wmv-v1"` (Part A) and `b"harmony-channel-msg-v1"` (reply packets).
- Cap-before-alloc on the responder: check the byte length **before** decrypt/decode (mirror `MAX_PAIRING_WIRE_BYTES` at `event_loop.rs:5626` and `MAX_WATERMARK_VECTOR_BYTES`). `MAX_RBSR_MESSAGE_BYTES = 64 * 1024`.
- Deterministic-nonce crypto variants stay behind `#[cfg(any(test, feature = "test-fixtures"))]` — production must never call them.
- Never construct `KeychainStore::new()` in test-reachable code; never set `HARMONY_DISABLE_KEYCHAIN` on a networked launch.
- Constants: `MAX_RBSR_ROUNDS = 32`, `LEAF_THRESHOLD = 16` (events; below this a mismatching range ships wholesale via `Have`), CDC target ≈ 256 events/chunk (`CHUNK_MASK_BITS = 8`), `CHUNK_MIN = 64`, `CHUNK_MAX = 1024`. All tunable; pin them as named consts.
- CI gates (run from `src-tauri/`): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. During iteration scope to `-p harmony-app --lib` (lib change relinks ~97 integ binaries under `--all-targets`; run the full `--all-targets` clippy/nextest only as the final per-PR gate).

## File Structure

- **Create `src-tauri/src/channel_rbsr.rs`** — pure protocol: `ReconcileKey`, `RangeFingerprint`, `RangeReconcileSource` trait, `SliceSource` (test impl), `RbsrMode`/`RbsrRange`/`RbsrMessage`/`BoundKey` + canonical CBOR, `respond()`, `initial_request()`, `process_reply()`, constants. No I/O, no crypto, no Zenoh — fully unit-testable.
- **Create `src-tauri/src/channel_chunk_index.rs`** — `ChunkSummary`, `ChunkIndex` (CDC boundary, `build_from_sorted`, `insert`, `range_fingerprint`). No I/O — takes events/lookups as inputs.
- **Modify `src-tauri/src/community_channel_log.rs`** — expose `signed_set_canonical_cbor`; add `event_element_hash`; `seal_rbsr_message`/`open_rbsr_message` + `MAX_RBSR_MESSAGE_BYTES`; `ChannelLog` owns a `ChunkIndex`, fed in `append`/`reload`; impl `RangeReconcileSource` for the log.
- **Modify `src-tauri/src/community_channel_log_engine.rs`** — `rbsr_respond(sealed) -> sealed`; `rbsr_request_round(...)`; engine-side seal/open via `channel_key_ref()`.
- **Modify `src-tauri/src/channel_backfill.rs`** — RBSR driver mode + vector-path fallback + round cap → full-reconcile.
- **Modify `src-tauri/src/event_loop.rs`** — dedicated `rbsr/**` queryable; RBSR GET driver with explicit `.timeout()`; adapter wiring.
- **Modify `src-tauri/src/lib.rs`** — `mod channel_rbsr;` and `mod channel_chunk_index;`.
- **Modify `src-tauri/tests/channel_backfill_integration.rs`** — acceptance test.
- **Create/modify a wire-format pin** (`src-tauri/tests/wire_format/channel_log_fixtures.rs`) — canonical-CBOR pins for RBSR message types + a deterministic-nonce sealed pin.

## Interfaces (shared signatures, defined once)

```rust
// channel_rbsr.rs
pub type ReconcileKey = (u64 /*wall_ms*/, u32 /*logical*/, String /*device_id*/, [u8; 32] /*element_hash*/);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeFingerprint { pub raw_sum: [u8; 32], pub count: u64 }
impl RangeFingerprint {
    pub fn zero() -> Self;
    pub fn fold(&mut self, element_hash: &[u8; 32]); // raw_sum += hash mod 2^256; count += 1
    pub fn combine(&self, other: &Self) -> Self;     // associative: raw_sums add mod 2^256, counts add
    pub fn finalize(&self) -> [u8; 16];              // SHA-256(raw_sum || leb128(count))[..16]
}

pub trait RangeReconcileSource {
    /// Fingerprint over the half-open canonical range [lo, hi).
    fn range_fingerprint(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> RangeFingerprint;
    /// Count of events in [lo, hi).
    fn range_count(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> u64;
    /// The element keys in [lo, hi), ascending. Used to pick bisection split points and to enumerate a leaf.
    fn keys_in_range(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> Vec<ReconcileKey>;
}

pub const MIN_KEY: ReconcileKey; // (0,0,"",[0;32])
pub const MAX_KEY: ReconcileKey; // (u64::MAX,u32::MAX, "\u{10FFFF}"…, [0xFF;32]) — sentinel upper bound
pub const LEAF_THRESHOLD: u64 = 16;
pub const MAX_RBSR_ROUNDS: u32 = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RbsrMode {
    Skip,
    Fingerprint([u8; 16]),
    Have(Vec<ReconcileKey>), // responder→requester: the element keys it holds in this leaf range
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RbsrRange { pub upper: ReconcileKey, pub mode: RbsrMode }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RbsrMessage { pub version: u8, pub ranges: Vec<RbsrRange> } // ranges partition [prev_upper, upper)

pub fn initial_request(source: &impl RangeReconcileSource) -> RbsrMessage;
pub fn respond(request: &RbsrMessage, source: &impl RangeReconcileSource) -> RbsrMessage;
/// Returns (leaf_keys_the_requester_is_missing, next_request_or_none_if_converged).
pub fn process_reply(reply: &RbsrMessage, source: &impl RangeReconcileSource) -> (Vec<ReconcileKey>, Option<RbsrMessage>);
```

`Have` carries element **keys** (not events) at the protocol layer so `channel_rbsr.rs` stays I/O-free; the engine maps keys→event packets for the wire. Determinism note: `RbsrMessage` CBOR encodes `ReconcileKey` as a 4-tuple; the `device_id` String and `element_hash` bytes give a total order.

```rust
// channel_chunk_index.rs
#[derive(Clone, Debug)]
pub struct ChunkSummary { pub first: ReconcileKey, pub last: ReconcileKey, pub count: u64, pub raw_sum: [u8; 32] }
pub struct ChunkIndex { /* Vec<ChunkSummary> sorted by first */ }
impl ChunkIndex {
    pub fn new() -> Self;
    /// entries MUST be pre-sorted ascending by ReconcileKey.
    pub fn build_from_sorted(entries: &[(ReconcileKey, [u8; 32])]) -> Self;
    /// Insert one element at its canonical position; re-chunk locally.
    pub fn insert(&mut self, key: ReconcileKey, element_hash: [u8; 32]);
    /// Aggregate whole chunks fully inside [lo,hi); call `boundary_events` for the (≤2) partial boundary chunks.
    pub fn range_fingerprint(
        &self, lo: &ReconcileKey, hi: &ReconcileKey,
        boundary_events: &mut dyn FnMut(&ReconcileKey, &ReconcileKey) -> Vec<(ReconcileKey, [u8; 32])>,
    ) -> RangeFingerprint;
}
```

```rust
// community_channel_log.rs (new/changed signatures)
pub(crate) fn signed_set_canonical_cbor(event: &SignedChannelEvent) -> Vec<u8>; // was private `fn`
pub(crate) fn event_element_hash(event: &SignedChannelEvent) -> [u8; 32];
pub const MAX_RBSR_MESSAGE_BYTES: usize = 64 * 1024;
pub(crate) fn seal_rbsr_message(key: &ChannelKey, msg: &RbsrMessage) -> Result<Vec<u8>, ChannelLogError>;
pub(crate) fn open_rbsr_message(key: &ChannelKey, bytes: &[u8]) -> Result<RbsrMessage, ChannelLogError>;
// ChannelLog: impl RangeReconcileSource (range_fingerprint via chunk_index, keys/count via segment+tail scan)
// ChannelLog: pub(crate) fn events_for_keys(&self, keys: &[ReconcileKey]) -> Vec<SignedChannelEvent>;
```

```rust
// community_channel_log_engine.rs
pub(crate) async fn rbsr_respond(&self, sealed_request: &[u8]) -> Option<Vec<u8>>; // None = cap/decrypt fail → caller falls back
// requester side: a round helper used by the backfill driver (exact shape in Task 12/13).
```

---

### Task 1: Element hash primitive

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs` (`signed_set_canonical_cbor` near `:594`; add `event_element_hash` adjacent; unit tests in the file's `#[cfg(test)]` module)

**Interfaces:**
- Consumes: existing `SignedChannelEvent`, `signed_set_canonical_cbor`.
- Produces: `pub(crate) fn event_element_hash(event: &SignedChannelEvent) -> [u8; 32]`; `signed_set_canonical_cbor` becomes `pub(crate)`.

- [ ] **Step 1: Write the failing test**

In the `#[cfg(test)] mod tests` of `community_channel_log.rs`:

```rust
#[test]
fn element_hash_is_deterministic_and_content_derived() {
    // build two events with identical content (use existing test helpers in this module)
    let ev = test_post_event("alice", 100, 0, "hello");
    let ev_same = test_post_event("alice", 100, 0, "hello");
    let ev_diff = test_post_event("alice", 100, 0, "world");
    assert_eq!(event_element_hash(&ev), event_element_hash(&ev_same), "same content → same hash");
    assert_ne!(event_element_hash(&ev), event_element_hash(&ev_diff), "different body → different hash");
    // hash must equal SHA-256 of the canonical signed-set CBOR
    use sha2::{Digest, Sha256};
    let expect: [u8; 32] = Sha256::digest(&signed_set_canonical_cbor(&ev)).into();
    assert_eq!(event_element_hash(&ev), expect);
}
```

(If no `test_post_event` helper exists, reuse the construction already used by the existing `reaction_index`/`watermark` unit tests in this file — grep the test module for how `SignedChannelEvent`s are built.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(element_hash_is_deterministic)'`
Expected: FAIL — `event_element_hash` not found.

- [ ] **Step 3: Implement**

Change `fn signed_set_canonical_cbor` to `pub(crate) fn signed_set_canonical_cbor`. Add:

```rust
pub(crate) fn event_element_hash(event: &SignedChannelEvent) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(signed_set_canonical_cbor(event)).into()
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(element_hash_is_deterministic)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_channel_log.rs
git commit -m "feat(channel-log): event_element_hash over canonical signed-set CBOR"
```

---

### Task 2: Range fingerprint primitive

**Files:**
- Create: `src-tauri/src/channel_rbsr.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod channel_rbsr;`)

**Interfaces:**
- Produces: `RangeFingerprint { raw_sum:[u8;32], count:u64 }` with `zero`/`fold`/`combine`/`finalize` (signatures above).

- [ ] **Step 1: Write the failing test**

Create `channel_rbsr.rs` with the type stub plus this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    fn h(n: u8) -> [u8; 32] { let mut x = [0u8; 32]; x[0] = n; x }

    #[test]
    fn fingerprint_is_order_independent_and_associative() {
        let hashes = [h(1), h(2), h(3), h(4)];
        let mut all = RangeFingerprint::zero();
        for x in &hashes { all.fold(x); }

        // folding in reverse yields the same fingerprint (sum is commutative)
        let mut rev = RangeFingerprint::zero();
        for x in hashes.iter().rev() { rev.fold(x); }
        assert_eq!(all.finalize(), rev.finalize());

        // combine of two halves == fold all
        let mut a = RangeFingerprint::zero(); a.fold(&hashes[0]); a.fold(&hashes[1]);
        let mut b = RangeFingerprint::zero(); b.fold(&hashes[2]); b.fold(&hashes[3]);
        assert_eq!(a.combine(&b).finalize(), all.finalize());
        assert_eq!(a.combine(&b).count, 4);
    }

    #[test]
    fn count_fold_breaks_match_on_synthetic_cancellation() {
        // two different multisets that sum to the same raw_sum must still differ via count
        let mut one = RangeFingerprint::zero(); one.fold(&h(5));
        let mut two = RangeFingerprint::zero(); two.fold(&h(2)); two.fold(&h(3)); // 2+3 == 5 in first byte
        assert_eq!(one.raw_sum, two.raw_sum, "raw sums collide by construction");
        assert_ne!(one.finalize(), two.finalize(), "count fold must distinguish them");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(fingerprint_is_order_independent)'`
Expected: FAIL (compile error — methods unimplemented).

- [ ] **Step 3: Implement**

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeFingerprint { pub raw_sum: [u8; 32], pub count: u64 }

fn add_mod_256(acc: &mut [u8; 32], add: &[u8; 32]) {
    let mut carry = 0u16;
    for i in 0..32 {
        let s = acc[i] as u16 + add[i] as u16 + carry;
        acc[i] = (s & 0xff) as u8;
        carry = s >> 8;
    } // overflow out of 256 bits is discarded (mod 2^256)
}

fn leb128(mut n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop { let b = (n & 0x7f) as u8; n >>= 7;
        if n == 0 { out.push(b); break } else { out.push(b | 0x80) } }
    out
}

impl RangeFingerprint {
    pub fn zero() -> Self { Self { raw_sum: [0u8; 32], count: 0 } }
    pub fn fold(&mut self, element_hash: &[u8; 32]) { add_mod_256(&mut self.raw_sum, element_hash); self.count += 1; }
    pub fn combine(&self, other: &Self) -> Self {
        let mut raw = self.raw_sum; add_mod_256(&mut raw, &other.raw_sum);
        Self { raw_sum: raw, count: self.count + other.count }
    }
    pub fn finalize(&self) -> [u8; 16] {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new(); h.update(self.raw_sum); h.update(leb128(self.count));
        let d = h.finalize(); let mut out = [0u8; 16]; out.copy_from_slice(&d[..16]); out
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(fingerprint)'`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/channel_rbsr.rs src-tauri/src/lib.rs
git commit -m "feat(rbsr): count-folded modular range fingerprint primitive"
```

---

### Task 3: Canonical key, source trait, and naive `SliceSource`

**Files:**
- Modify: `src-tauri/src/channel_rbsr.rs`

**Interfaces:**
- Produces: `ReconcileKey`, `MIN_KEY`, `MAX_KEY`, `RangeReconcileSource` trait, `SliceSource` (test/double impl over a `Vec<(ReconcileKey,[u8;32])>`).

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod source_tests {
    use super::*;
    fn key(w: u64, hb: u8) -> ReconcileKey { (w, 0, "d".into(), { let mut h=[0u8;32]; h[0]=hb; h }) }

    fn sample() -> SliceSource {
        SliceSource::from_unsorted(vec![ key(10,1), key(20,2), key(30,3), key(40,4) ]
            .into_iter().map(|k| (k.clone(), k.3)).collect())
    }

    #[test]
    fn range_count_and_keys_are_half_open() {
        let s = sample();
        assert_eq!(s.range_count(&key(10,1), &key(40,4)), 3, "[10,40) excludes 40");
        assert_eq!(s.keys_in_range(&key(10,1), &key(30,3)), vec![key(10,1), key(20,2)]);
        assert_eq!(s.range_count(&MIN_KEY, &MAX_KEY), 4, "whole universe");
    }

    #[test]
    fn range_fingerprint_matches_manual_fold() {
        let s = sample();
        let mut expect = RangeFingerprint::zero();
        expect.fold(&key(20,2).3); expect.fold(&key(30,3).3);
        assert_eq!(s.range_fingerprint(&key(20,2), &key(40,4)).finalize(), expect.finalize());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(range_count_and_keys)'`
Expected: FAIL (compile — `SliceSource`/`MIN_KEY`/`MAX_KEY` missing).

- [ ] **Step 3: Implement**

```rust
pub type ReconcileKey = (u64, u32, String, [u8; 32]);
pub fn min_key() -> ReconcileKey { (0, 0, String::new(), [0u8; 32]) }
pub fn max_key() -> ReconcileKey { (u64::MAX, u32::MAX, "\u{10FFFF}".repeat(8), [0xFFu8; 32]) }
// expose as consts via once-style fns; tests use min_key()/max_key(). Provide MIN_KEY/MAX_KEY as fn aliases.

pub trait RangeReconcileSource {
    fn range_fingerprint(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> RangeFingerprint;
    fn range_count(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> u64;
    fn keys_in_range(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> Vec<ReconcileKey>;
}

pub struct SliceSource { entries: Vec<(ReconcileKey, [u8; 32])> } // sorted ascending by key
impl SliceSource {
    pub fn from_unsorted(mut e: Vec<(ReconcileKey, [u8; 32])>) -> Self {
        e.sort_by(|a, b| a.0.cmp(&b.0)); e.dedup_by(|a, b| a.0 == b.0); Self { entries: e }
    }
    fn slice(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> &[(ReconcileKey, [u8; 32])] {
        let s = self.entries.partition_point(|x| &x.0 < lo);
        let e = self.entries.partition_point(|x| &x.0 < hi);
        &self.entries[s..e]
    }
}
impl RangeReconcileSource for SliceSource {
    fn range_fingerprint(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> RangeFingerprint {
        let mut f = RangeFingerprint::zero();
        for (_, h) in self.slice(lo, hi) { f.fold(h); } f
    }
    fn range_count(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> u64 { self.slice(lo, hi).len() as u64 }
    fn keys_in_range(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> Vec<ReconcileKey> {
        self.slice(lo, hi).iter().map(|x| x.0.clone()).collect()
    }
}
```

Replace `MIN_KEY`/`MAX_KEY` references in tests with `min_key()`/`max_key()` (or add `pub fn MIN_KEY()`—prefer snake_case fns to satisfy clippy). Update the interface block usages accordingly in later tasks.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(range_count_and_keys)' -E 'test(range_fingerprint_matches_manual)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/channel_rbsr.rs
git commit -m "feat(rbsr): canonical ReconcileKey, source trait, naive SliceSource"
```

---

### Task 4: RBSR message types + canonical CBOR

**Files:**
- Modify: `src-tauri/src/channel_rbsr.rs`

**Interfaces:**
- Produces: `RbsrMode`, `RbsrRange`, `RbsrMessage`, plus `encode_message(&RbsrMessage)->Vec<u8>` / `decode_message(&[u8])->Result<RbsrMessage, RbsrError>` (canonical CBOR via ciborium, Serde-derived).

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod wire_tests {
    use super::*;
    fn k(w: u64) -> ReconcileKey { (w, 0, "d".into(), [w as u8; 32]) }

    #[test]
    fn message_cbor_round_trips() {
        let m = RbsrMessage { version: 1, ranges: vec![
            RbsrRange { upper: k(10), mode: RbsrMode::Skip },
            RbsrRange { upper: k(20), mode: RbsrMode::Fingerprint([7u8; 16]) },
            RbsrRange { upper: k(30), mode: RbsrMode::Have(vec![k(21), k(22)]) },
        ]};
        let bytes = encode_message(&m);
        assert_eq!(decode_message(&bytes).unwrap(), m);
    }

    #[test]
    fn decode_rejects_garbage() { assert!(decode_message(&[0xff, 0x00, 0x13]).is_err()); }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(message_cbor_round_trips)'`
Expected: FAIL (compile).

- [ ] **Step 3: Implement**

Add `serde::{Serialize, Deserialize}` derives. Use ciborium (already a dep — grep `ciborium` usage in `community_channel_log.rs`).

```rust
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RbsrMode {
    #[serde(rename = "s")] Skip,
    #[serde(rename = "f")] Fingerprint([u8; 16]),
    #[serde(rename = "h")] Have(Vec<ReconcileKey>),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RbsrRange { #[serde(rename = "u")] pub upper: ReconcileKey, #[serde(rename = "m")] pub mode: RbsrMode }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RbsrMessage { #[serde(rename = "v")] pub version: u8, #[serde(rename = "r")] pub ranges: Vec<RbsrRange> }

#[derive(Debug)]
pub enum RbsrError { Decode, TooLarge }

pub fn encode_message(m: &RbsrMessage) -> Vec<u8> {
    let mut v = Vec::new(); ciborium::into_writer(m, &mut v).expect("cbor encode"); v
}
pub fn decode_message(bytes: &[u8]) -> Result<RbsrMessage, RbsrError> {
    ciborium::from_reader(bytes).map_err(|_| RbsrError::Decode)
}
```

`RBSR_PROTOCOL_VERSION: u8 = 1`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(message_cbor)' -E 'test(decode_rejects_garbage)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/channel_rbsr.rs
git commit -m "feat(rbsr): message types with canonical CBOR encode/decode"
```

---

### Task 5: AEAD seal/open + cap (channel-keyed)

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs`

**Interfaces:**
- Consumes: `RbsrMessage`, `encode_message`/`decode_message` (Task 4); existing `ChannelKey`, the ChaCha20-Poly1305 helpers used by `seal_watermark_vector` (Part A).
- Produces: `MAX_RBSR_MESSAGE_BYTES`, `seal_rbsr_message`, `open_rbsr_message`, plus a `#[cfg(any(test, feature="test-fixtures"))] seal_rbsr_message_with_nonce`.

- [ ] **Step 1: Write the failing test**

In `community_channel_log.rs` tests (mirror the existing `seal_watermark_vector` tests — grep `seal_watermark_vector` in the test module and copy structure):

```rust
#[test]
fn rbsr_seal_round_trips_and_rejects_tamper_wrongkey_oversize() {
    let key = test_channel_key();          // reuse the helper the wmv tests use
    let other = test_channel_key_other();
    let msg = sample_rbsr_message();        // small RbsrMessage
    let sealed = seal_rbsr_message(&key, &msg).unwrap();
    assert_eq!(open_rbsr_message(&key, &sealed).unwrap(), msg);

    // wrong key fails
    assert!(open_rbsr_message(&other, &sealed).is_err());
    // tamper fails
    let mut t = sealed.clone(); *t.last_mut().unwrap() ^= 0x01;
    assert!(open_rbsr_message(&key, &t).is_err());
    // oversize rejected before decrypt
    let big = vec![0u8; MAX_RBSR_MESSAGE_BYTES + 1];
    assert!(matches!(open_rbsr_message(&key, &big), Err(ChannelLogError::WireTooLarge)));
}

#[test]
fn rbsr_aad_is_domain_separated_from_wmv() {
    // a message sealed under the wmv AAD must NOT open as an rbsr message
    let key = test_channel_key();
    let wmv_sealed = seal_watermark_vector(&key, &sample_watermark_vector()).unwrap();
    assert!(open_rbsr_message(&key, &wmv_sealed).is_err());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(rbsr_seal_round_trips)'`
Expected: FAIL (compile).

- [ ] **Step 3: Implement**

Copy the `seal_watermark_vector`/`open_watermark_vector` bodies verbatim, changing: the AAD constant to `RBSR_AAD = b"harmony-channel-rbsr-v1"`, the cap to `MAX_RBSR_MESSAGE_BYTES`, the payload encode/decode to `encode_message`/`decode_message`. Keep the same nonce generation, `[nonce||ct||tag]` framing, and the `_with_nonce`/`_inner` split behind `#[cfg(any(test, feature="test-fixtures"))]`. The cap check runs on `bytes.len()` **before** any allocation/decrypt (return `ChannelLogError::WireTooLarge`).

```rust
const RBSR_AAD: &[u8] = b"harmony-channel-rbsr-v1";
pub const MAX_RBSR_MESSAGE_BYTES: usize = 64 * 1024;

pub(crate) fn open_rbsr_message(key: &ChannelKey, bytes: &[u8]) -> Result<RbsrMessage, ChannelLogError> {
    if bytes.len() > MAX_RBSR_MESSAGE_BYTES { return Err(ChannelLogError::WireTooLarge); } // BEFORE alloc
    let plaintext = aead_open(key, bytes, RBSR_AAD)?;       // same helper wmv open uses
    decode_message(&plaintext).map_err(|_| ChannelLogError::CborDecode)
}
```

(Use whatever the existing `ChannelLogError` variant names are — grep the wmv open for `WireTooLarge`/`CborDecode` equivalents and match them exactly.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(rbsr_seal)' -E 'test(rbsr_aad_is_domain)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_channel_log.rs
git commit -m "feat(channel-log): AEAD seal/open for RBSR messages (rbsr-v1 AAD, 64KiB cap)"
```

---

### Task 6: Responder bisection logic

**Files:**
- Modify: `src-tauri/src/channel_rbsr.rs`

**Interfaces:**
- Consumes: `RangeReconcileSource`, `RbsrMessage`, `LEAF_THRESHOLD`.
- Produces: `pub fn respond(request: &RbsrMessage, source: &impl RangeReconcileSource) -> RbsrMessage`.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod respond_tests {
    use super::*;
    fn k(w: u64, hb: u8) -> ReconcileKey { (w,0,"d".into(),{let mut h=[0u8;32];h[0]=hb;h}) }
    fn src(ws: &[(u64,u8)]) -> SliceSource {
        SliceSource::from_unsorted(ws.iter().map(|&(w,b)| { let key=k(w,b); (key.clone(), key.3) }).collect())
    }

    #[test]
    fn matching_whole_range_returns_skip() {
        let s = src(&[(10,1),(20,2),(30,3)]);
        let req = RbsrMessage { version: RBSR_PROTOCOL_VERSION, ranges: vec![
            RbsrRange { upper: max_key(), mode: RbsrMode::Fingerprint(s.range_fingerprint(&min_key(), &max_key()).finalize()) }
        ]};
        let reply = respond(&req, &s);
        assert_eq!(reply.ranges.len(), 1);
        assert_eq!(reply.ranges[0].mode, RbsrMode::Skip);
    }

    #[test]
    fn small_mismatch_returns_have_wholesale() {
        let responder = src(&[(10,1),(20,2),(30,3)]); // requester lacks 20
        let requester = src(&[(10,1),(30,3)]);
        let req = RbsrMessage { version: RBSR_PROTOCOL_VERSION, ranges: vec![
            RbsrRange { upper: max_key(), mode: RbsrMode::Fingerprint(requester.range_fingerprint(&min_key(), &max_key()).finalize()) }
        ]};
        let reply = respond(&req, &responder);
        // count (3) <= LEAF_THRESHOLD → wholesale Have of responder's keys in the mismatching range
        let have: Vec<_> = reply.ranges.iter().filter_map(|r| if let RbsrMode::Have(ks)=&r.mode {Some(ks.clone())} else {None}).flatten().collect();
        assert!(have.contains(&k(20,2)));
    }

    #[test]
    fn large_mismatch_bisects_into_fingerprints() {
        // responder has 40 events, requester has 39 (missing one) → above LEAF_THRESHOLD → bisect, not Have
        let resp: Vec<_> = (0..40u64).map(|i| (i*10, i as u8)).collect();
        let responder = src(&resp);
        let mut req_set = resp.clone(); req_set.remove(20);
        let requester = src(&req_set);
        let req = RbsrMessage { version: RBSR_PROTOCOL_VERSION, ranges: vec![
            RbsrRange { upper: max_key(), mode: RbsrMode::Fingerprint(requester.range_fingerprint(&min_key(), &max_key()).finalize()) }
        ]};
        let reply = respond(&req, &responder);
        assert!(reply.ranges.len() >= 2, "bisected");
        assert!(reply.ranges.iter().all(|r| matches!(r.mode, RbsrMode::Fingerprint(_) | RbsrMode::Skip)));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(matching_whole_range_returns_skip)'`
Expected: FAIL (compile — `respond` missing).

- [ ] **Step 3: Implement**

`respond` walks the request's ranges (each `[prev_upper, upper)`), and for each `Fingerprint(fp)` compares to the responder's own fingerprint:

```rust
pub fn respond(request: &RbsrMessage, source: &impl RangeReconcileSource) -> RbsrMessage {
    let mut out = Vec::new();
    let mut lo = min_key();
    for range in &request.ranges {
        let hi = range.upper.clone();
        if let RbsrMode::Fingerprint(their_fp) = &range.mode {
            let mine = source.range_fingerprint(&lo, &hi).finalize();
            if &mine == their_fp {
                out.push(RbsrRange { upper: hi.clone(), mode: RbsrMode::Skip });
            } else {
                let count = source.range_count(&lo, &hi);
                if count <= LEAF_THRESHOLD {
                    out.push(RbsrRange { upper: hi.clone(), mode: RbsrMode::Have(source.keys_in_range(&lo, &hi)) });
                } else {
                    // bisect by local median key; each sub-range carries the responder's own fingerprint
                    let keys = source.keys_in_range(&lo, &hi);
                    let mid = keys[keys.len() / 2].clone(); // (randomized split optional; median is deterministic+fine)
                    out.push(RbsrRange { upper: mid.clone(), mode: RbsrMode::Fingerprint(source.range_fingerprint(&lo, &mid).finalize()) });
                    out.push(RbsrRange { upper: hi.clone(),  mode: RbsrMode::Fingerprint(source.range_fingerprint(&mid, &hi).finalize()) });
                }
            }
        } // Skip/Have in a request are inert (requester never sends them); ignore defensively
        lo = hi;
    }
    RbsrMessage { version: RBSR_PROTOCOL_VERSION, ranges: out }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(respond_tests)'`
Expected: PASS (all three).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/channel_rbsr.rs
git commit -m "feat(rbsr): responder bisection (skip/bisect/wholesale-Have at leaf)"
```

---

### Task 7: Requester round logic + convergence harness

**Files:**
- Modify: `src-tauri/src/channel_rbsr.rs`

**Interfaces:**
- Consumes: `respond` (for the test harness), `RangeReconcileSource`.
- Produces: `initial_request`, `process_reply` (signatures above), `RBSR_PROTOCOL_VERSION`.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod converge_tests {
    use super::*;
    fn k(w: u64, hb: u8) -> ReconcileKey { (w,0,"d".into(),{let mut h=[0u8;32];h[0]=hb;h}) }
    fn src(ws: &[(u64,u8)]) -> SliceSource {
        SliceSource::from_unsorted(ws.iter().map(|&(w,b)| { let key=k(w,b); (key.clone(), key.3) }).collect())
    }

    /// Drive requester(missing) ↔ responder(full) to convergence; return (rounds, total Have keys).
    fn reconcile(req: &SliceSource, resp: &SliceSource) -> (u32, usize) {
        let mut request = initial_request(req);
        let mut acquired: Vec<ReconcileKey> = Vec::new();
        let mut rounds = 0;
        loop {
            rounds += 1; assert!(rounds <= MAX_RBSR_ROUNDS, "must converge");
            let reply = respond(&request, resp);
            let (missing, next) = process_reply(&reply, req);
            acquired.extend(missing);
            match next { None => break, Some(n) => request = n }
        }
        (rounds, acquired.len())
    }

    #[test]
    fn converges_and_transfers_only_the_gap() {
        // responder has 100 events; requester missing exactly 3 (incl one sub-max out-of-order hole)
        let full: Vec<_> = (0..100u64).map(|i| (i, (i % 251) as u8)).collect();
        let resp = src(&full);
        let mut miss = full.clone();
        for idx in [70, 40, 5] { miss.remove(idx); } // drop 3
        let req = src(&miss);
        let (rounds, have) = reconcile(&req, &resp);
        assert!(rounds <= MAX_RBSR_ROUNDS);
        // O(gap): Have keys bounded well under full history (leaf wholesale may resend a few neighbours)
        assert!(have >= 3 && have < 40, "transferred ~gap, not history: {have}");
    }

    #[test]
    fn identical_sets_converge_in_one_round_with_no_transfer() {
        let s = src(&[(1,1),(2,2),(3,3)]);
        let (rounds, have) = reconcile(&s, &s);
        assert_eq!((rounds, have), (1, 0));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(converges_and_transfers_only_the_gap)'`
Expected: FAIL (compile — `initial_request`/`process_reply` missing).

- [ ] **Step 3: Implement**

```rust
pub const RBSR_PROTOCOL_VERSION: u8 = 1;

pub fn initial_request(source: &impl RangeReconcileSource) -> RbsrMessage {
    RbsrMessage { version: RBSR_PROTOCOL_VERSION, ranges: vec![
        RbsrRange { upper: max_key(), mode: RbsrMode::Fingerprint(source.range_fingerprint(&min_key(), &max_key()).finalize()) }
    ]}
}

/// Returns (keys the responder has that we lack, next request — None when no mismatch remains).
pub fn process_reply(reply: &RbsrMessage, source: &impl RangeReconcileSource) -> (Vec<ReconcileKey>, Option<RbsrMessage>) {
    let mut missing = Vec::new();
    let mut next_ranges = Vec::new();
    let mut lo = min_key();
    for range in &reply.ranges {
        let hi = range.upper.clone();
        match &range.mode {
            RbsrMode::Skip => {}
            RbsrMode::Have(keys) => {
                let have_local: std::collections::HashSet<_> = source.keys_in_range(&lo, &hi).into_iter().collect();
                for kk in keys { if !have_local.contains(kk) { missing.push(kk.clone()); } }
            }
            RbsrMode::Fingerprint(their_fp) => {
                let mine = source.range_fingerprint(&lo, &hi).finalize();
                if &mine != their_fp {
                    next_ranges.push(RbsrRange { upper: hi.clone(), mode: RbsrMode::Fingerprint(mine) });
                }
            }
        }
        lo = hi;
    }
    let next = if next_ranges.is_empty() { None }
               else { Some(RbsrMessage { version: RBSR_PROTOCOL_VERSION, ranges: next_ranges }) };
    (missing, next)
}
```

Note the convergence subtlety: when the requester re-sends a still-mismatching `Fingerprint` sub-range, the responder will bisect it further or `Have` it; the loop shrinks the mismatching span each round (O(log n) rounds). The `identical_sets` case: round 0 fingerprint matches → responder returns one `Skip` → `process_reply` produces `next=None` → 1 round, 0 transfer.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(converge_tests)'`
Expected: PASS (both).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/channel_rbsr.rs
git commit -m "feat(rbsr): requester round logic + convergence (pure protocol harness)"
```

---

### Task 8: CDC chunk index — build + range query

**Files:**
- Create: `src-tauri/src/channel_chunk_index.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod channel_chunk_index;`)

**Interfaces:**
- Consumes: `ReconcileKey`, `RangeFingerprint` (from `channel_rbsr`).
- Produces: `ChunkSummary`, `ChunkIndex::{new, build_from_sorted, range_fingerprint}`, CDC consts.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel_rbsr::{ReconcileKey, RangeFingerprint, SliceSource, RangeReconcileSource, min_key, max_key};
    fn entry(w: u64, hb: u8) -> (ReconcileKey, [u8;32]) { let mut h=[0u8;32]; h[0]=hb; h[1]=(w as u8); ((w,0,"d".into(),h), h) }

    fn sorted(n: u64) -> Vec<(ReconcileKey,[u8;32])> {
        let mut v: Vec<_> = (0..n).map(|i| entry(i, (i % 251) as u8)).collect();
        v.sort_by(|a,b| a.0.cmp(&b.0)); v
    }

    #[test]
    fn chunk_boundaries_are_content_defined_not_input_order() {
        let s = sorted(2000);
        let a = ChunkIndex::build_from_sorted(&s);
        let mut shuffled = s.clone(); shuffled.reverse(); shuffled.sort_by(|x,y| x.0.cmp(&y.0)); // same sorted set
        let b = ChunkIndex::build_from_sorted(&shuffled);
        assert_eq!(a.boundaries(), b.boundaries(), "same set → identical chunks regardless of pre-sort path");
    }

    #[test]
    fn range_fingerprint_matches_naive_over_same_set() {
        let s = sorted(2000);
        let idx = ChunkIndex::build_from_sorted(&s);
        let naive = SliceSource::from_unsorted(s.clone());
        let mut lookup = |lo: &ReconcileKey, hi: &ReconcileKey| -> Vec<(ReconcileKey,[u8;32])> {
            s.iter().filter(|(k,_)| k >= lo && k < hi).cloned().collect()
        };
        let lo = s[300].0.clone(); let hi = s[1700].0.clone();
        assert_eq!(idx.range_fingerprint(&lo, &hi, &mut lookup).finalize(),
                   naive.range_fingerprint(&lo, &hi).finalize());
        // whole-universe too
        assert_eq!(idx.range_fingerprint(&min_key(), &max_key(), &mut lookup).finalize(),
                   naive.range_fingerprint(&min_key(), &max_key()).finalize());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(chunk_boundaries_are_content_defined)'`
Expected: FAIL (compile).

- [ ] **Step 3: Implement**

```rust
use crate::channel_rbsr::{ReconcileKey, RangeFingerprint};

pub const CHUNK_MASK_BITS: u32 = 8;   // ~256 events/chunk
pub const CHUNK_MIN: usize = 64;
pub const CHUNK_MAX: usize = 1024;

fn is_boundary(element_hash: &[u8; 32], run_len: usize) -> bool {
    if run_len < CHUNK_MIN { return false; }
    if run_len >= CHUNK_MAX { return true; }
    let v = u64::from_le_bytes(element_hash[..8].try_into().unwrap());
    (v & ((1u64 << CHUNK_MASK_BITS) - 1)) == 0
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkSummary { pub first: ReconcileKey, pub last: ReconcileKey, pub count: u64, pub raw_sum: [u8; 32] }

pub struct ChunkIndex { chunks: Vec<ChunkSummary> } // sorted ascending by `first`

impl ChunkIndex {
    pub fn new() -> Self { Self { chunks: Vec::new() } }

    pub fn build_from_sorted(entries: &[(ReconcileKey, [u8; 32])]) -> Self {
        let mut chunks = Vec::new();
        let (mut cur, mut run): (Option<ChunkSummary>, usize) = (None, 0);
        for (key, hash) in entries {
            let c = cur.get_or_insert(ChunkSummary { first: key.clone(), last: key.clone(), count: 0, raw_sum: [0u8;32] });
            let mut f = RangeFingerprint { raw_sum: c.raw_sum, count: c.count }; f.fold(hash);
            c.raw_sum = f.raw_sum; c.count = f.count; c.last = key.clone(); run += 1;
            if is_boundary(hash, run) { chunks.push(cur.take().unwrap()); run = 0; }
        }
        if let Some(c) = cur.take() { chunks.push(c); }
        Self { chunks }
    }

    pub fn boundaries(&self) -> Vec<ReconcileKey> { self.chunks.iter().map(|c| c.last.clone()).collect() }

    pub fn range_fingerprint(
        &self, lo: &ReconcileKey, hi: &ReconcileKey,
        boundary_events: &mut dyn FnMut(&ReconcileKey, &ReconcileKey) -> Vec<(ReconcileKey, [u8; 32])>,
    ) -> RangeFingerprint {
        let mut agg = RangeFingerprint::zero();
        for c in &self.chunks {
            if &c.last < lo || &c.first >= hi { continue; }                 // chunk entirely outside [lo,hi)
            if &c.first >= lo && &c.last < hi {
                agg = agg.combine(&RangeFingerprint { raw_sum: c.raw_sum, count: c.count }); // whole chunk
            } else {
                // partial boundary chunk: fold only the events inside [lo,hi)
                for (k, h) in boundary_events(&c.first, &c.last) {
                    if &k >= lo && &k < hi { agg.fold(&h); }
                }
            }
        }
        agg
    }
}
```

Note: `boundary_events(first,last)` returns all events in the chunk's `[first, last]` inclusive span; the closure body filters to `[lo,hi)`. The naive lookup in the test passes the chunk span and the impl filters.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(channel_chunk_index)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/channel_chunk_index.rs src-tauri/src/lib.rs
git commit -m "feat(rbsr): content-defined-chunk index, O(log n) range fingerprint aggregation"
```

---

### Task 9: CDC chunk index — incremental insert

**Files:**
- Modify: `src-tauri/src/channel_chunk_index.rs`

**Interfaces:**
- Produces: `ChunkIndex::insert(key, element_hash)` — incremental maintenance that keeps the index byte-identical to a `build_from_sorted` over the same set.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn incremental_insert_equals_rebuild() {
    let mut s = sorted(1500);
    // pull out 200 scattered entries to insert incrementally (incl mid-order/low-key ones)
    let mut pulled = Vec::new();
    for idx in (0..s.len()).step_by(7).rev() { pulled.push(s.remove(idx)); }
    let mut idx = ChunkIndex::build_from_sorted(&s);
    for (k, h) in pulled.iter().cloned() { idx.insert(k, h); }

    let mut full = s.clone(); full.extend(pulled); full.sort_by(|a,b| a.0.cmp(&b.0));
    let rebuilt = ChunkIndex::build_from_sorted(&full);
    assert_eq!(idx.boundaries(), rebuilt.boundaries(), "insert path == rebuild");
    // and fingerprints agree over the whole universe
    let mut lk = |lo:&ReconcileKey,hi:&ReconcileKey| full.iter().filter(|(k,_)| k>=lo&&k<hi).cloned().collect::<Vec<_>>();
    assert_eq!(idx.range_fingerprint(&min_key(),&max_key(),&mut lk).finalize(),
               rebuilt.range_fingerprint(&min_key(),&max_key(),&mut lk).finalize());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(incremental_insert_equals_rebuild)'`
Expected: FAIL (`insert` missing).

- [ ] **Step 3: Implement**

To keep `insert` provably equal to `build_from_sorted`, recompute the affected chunk by re-chunking the local run. Simplest correct approach: locate the chunk whose span contains `key` (or the gap between chunks), reconstruct that chunk's `(key,hash)` list **plus** the new entry by replaying — but since the index doesn't store per-event hashes, store them: change `ChunkSummary` to also hold `entries: Vec<(ReconcileKey,[u8;32])>` **only while mutable**, OR re-derive from a held sorted entries vector. Decision (memory-frugal): keep a parallel `entries: Vec<(ReconcileKey,[u8;32])>` (sorted) on `ChunkIndex` as the source of truth; `chunks` is a derived summary. `insert` does a sorted insert into `entries`, then re-runs `build_from_sorted` **over only the window between the surrounding boundaries** and splices the resulting chunks back.

```rust
pub struct ChunkIndex { chunks: Vec<ChunkSummary>, entries: Vec<(ReconcileKey,[u8;32])> }
// build_from_sorted also stores entries; new() inits both empty.

pub fn insert(&mut self, key: ReconcileKey, element_hash: [u8; 32]) {
    let pos = self.entries.partition_point(|(k,_)| k < &key);
    if self.entries.get(pos).map(|(k,_)| k == &key).unwrap_or(false) { return; } // dedup
    self.entries.insert(pos, (key, element_hash));
    // Re-chunk a bounded window: from the previous boundary before `pos` to the next boundary after.
    // For correctness-first (optimize later), rebuild from entries (bounded by CHUNK_MAX rescan in the optimized form).
    let rebuilt = ChunkIndex::build_from_sorted(&self.entries);
    self.chunks = rebuilt.chunks;
}
```

Correctness-first rebuild keeps the test green; the bounded-window optimization (re-chunk only `[prev_boundary, next_boundary]` and splice) is a follow-up noted inline as a `// PERF:` comment. **Do not** ship the O(n) rebuild as the final form for very large channels — leave the `// PERF:` marker and a one-line note that the window splice is the intended production form (the integration test tolerates either; a micro-benchmark is out of scope for this PR).

> Reviewer note: if the window-splice optimization is in scope for this PR, implement it here with a test that asserts only O(CHUNK_MAX) entries are rescanned. Otherwise the rebuild-from-entries form is acceptable for correctness and the `// PERF:` marker tracks the debt.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(incremental_insert_equals_rebuild)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/channel_chunk_index.rs
git commit -m "feat(rbsr): incremental chunk-index insert (rebuild-window, parity with build)"
```

---

### Task 10: `ChannelLog` owns the index + implements `RangeReconcileSource`

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs`

**Interfaces:**
- Consumes: `ChunkIndex` (Task 8/9), `event_element_hash` (Task 1), `RangeReconcileSource` (Task 3).
- Produces: a `chunk_index` field on `ChannelLog` (built in `reload`, maintained in `append`); `impl RangeReconcileSource for ChannelLog`; `events_for_keys`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn log_source_fingerprint_matches_slice_source_and_survives_reload() {
    // build a ChannelLog with N events across segments+tail (reuse the seal-threshold test helper)
    let dir = tempdir();
    let mut log = build_log_with_events(&dir, &demo_events(900)); // crosses seal threshold → multiple segments
    let all: Vec<(ReconcileKey,[u8;32])> = log.all_events_sorted_for_test()
        .iter().map(|e| (reconcile_key(e), event_element_hash(e))).collect();
    let naive = SliceSource::from_unsorted(all);

    assert_eq!(log.range_fingerprint(&min_key(), &max_key()).finalize(),
               naive.range_fingerprint(&min_key(), &max_key()).finalize());

    // reload from disk and re-check (index rebuilt in reload)
    let reloaded = ChannelLog::reload(&dir.path(), test_config()).unwrap();
    assert_eq!(reloaded.range_fingerprint(&min_key(), &max_key()).finalize(),
               naive.range_fingerprint(&min_key(), &max_key()).finalize());
}
```

(`reconcile_key(e) = (e.at().wall_ms, e.at().logical, e.at().device_id.clone(), event_element_hash(e))`. Add a small `#[cfg(test)] all_events_sorted_for_test` helper if none exists.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(log_source_fingerprint_matches_slice_source)'`
Expected: FAIL (compile — `range_fingerprint` not on `ChannelLog`).

- [ ] **Step 3: Implement**

- Add `chunk_index: ChunkIndex` to `ChannelLog`.
- In `reload`, after the existing segment+tail scan that rebuilds `reaction_index`/`device_watermarks`, build a sorted `Vec<(ReconcileKey,[u8;32])>` over **all** events and `self.chunk_index = ChunkIndex::build_from_sorted(&sorted)` (fold into the same pass — collect `(key,hash)` while scanning, sort once at the end).
- In `append`, after pushing to `tail`, call `self.chunk_index.insert(reconcile_key(&event), event_element_hash(&event))`.
- Implement the trait. `range_fingerprint`/`range_count`/`keys_in_range` read the canonical-sorted view. Because the log isn't sorted on disk, the source materializes the needed events: for `range_fingerprint`, delegate to `chunk_index.range_fingerprint(lo,hi, &mut |cf, cl| self.events_in_chunk_span(cf, cl))` where `events_in_chunk_span` reads the (≤2) boundary chunks' events from segments+tail. `keys_in_range`/`range_count` scan segments overlapping `[lo,hi)` (skip by `SegmentDescriptor.range`) + tail, collect+sort+filter.

```rust
fn reconcile_key(e: &SignedChannelEvent) -> ReconcileKey {
    let at = e.at(); (at.wall_ms, at.logical, at.device_id.clone(), event_element_hash(e))
}

impl RangeReconcileSource for ChannelLog {
    fn range_fingerprint(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> RangeFingerprint {
        let mut lookup = |cf: &ReconcileKey, cl: &ReconcileKey| self.events_in_key_span(cf, cl)
            .into_iter().map(|e| (reconcile_key(&e), event_element_hash(&e))).collect();
        self.chunk_index.range_fingerprint(lo, hi, &mut lookup)
    }
    fn range_count(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> u64 { self.keys_in_range(lo, hi).len() as u64 }
    fn keys_in_range(&self, lo: &ReconcileKey, hi: &ReconcileKey) -> Vec<ReconcileKey> {
        let mut ks: Vec<ReconcileKey> = self.events_in_key_span(lo, hi).iter().map(reconcile_key).collect();
        ks.sort(); ks.retain(|k| k >= lo && k < hi); ks
    }
}
```

`events_in_key_span(lo, hi)`: iterate segments whose `range` overlaps `[lo.0..=hi.0]` (compare on `wall_ms`; over-include is fine — filtered by caller), read each, plus the tail; return the events whose `reconcile_key` ∈ `[lo, hi)`. Reuse `read_segment`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(log_source_fingerprint_matches_slice_source)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_channel_log.rs
git commit -m "feat(channel-log): ChannelLog owns chunk index + implements RangeReconcileSource"
```

---

### Task 11: Engine respond half (`rbsr_respond`)

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs`

**Interfaces:**
- Consumes: `open_rbsr_message`/`seal_rbsr_message` (Task 5), `respond` (Task 6), the log's `RangeReconcileSource` (Task 10), `channel_key_ref()`, `events_for_keys`.
- Produces: `pub(crate) async fn rbsr_respond(&self, sealed_request: &[u8]) -> Option<Vec<u8>>` and, in the reply, the `Have` keys mapped to encrypted event packets (the keys-vs-packets bridge).

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn engine_rbsr_respond_round_trips_and_serves_gap() {
    let full = engine_with_events(&demo_events(50)).await;   // responder
    let lacking = engine_with_events(&omit(demo_events(50), &[20])).await; // requester missing #20

    // requester seals round-0 request
    let req_msg = lacking.log_initial_rbsr_request().await;  // thin wrapper: initial_request over the log source
    let sealed_req = lacking.seal_rbsr(&req_msg).await.unwrap();

    let sealed_reply = full.rbsr_respond(&sealed_req).await.expect("responds");
    let reply = lacking.open_rbsr(&sealed_reply).await.unwrap();
    // reply must mention the gap somewhere (Have or a mismatching fingerprint sub-range)
    assert!(!reply.ranges.is_empty());

    // oversize/garbage → None (caller falls back)
    assert!(full.rbsr_respond(&vec![0u8; MAX_RBSR_MESSAGE_BYTES + 1]).await.is_none());
}
```

(Add thin test-only async wrappers `log_initial_rbsr_request`/`seal_rbsr`/`open_rbsr` if needed, gated `#[cfg(any(test, feature="test-fixtures"))]`, that lock the log and call the pure fns + the seal/open with `channel_key_ref()`.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(engine_rbsr_respond_round_trips)'`
Expected: FAIL (compile).

- [ ] **Step 3: Implement**

```rust
pub(crate) async fn rbsr_respond(&self, sealed_request: &[u8]) -> Option<Vec<u8>> {
    let key = self.channel_key_ref()?;                     // None if no key → caller falls back
    let request = open_rbsr_message(key, sealed_request).ok()?; // cap+AEAD inside; Err → None
    let log = self.log.lock().await;
    let mut reply = channel_rbsr::respond(&request, &*log); // &ChannelLog: RangeReconcileSource
    // Map Have(keys) → still keys on the wire; the requester fetches packets in the NEXT round via a
    // dedicated leaf request. (Simplest: in this PR, inline the events as encrypted packets alongside.)
    // -> For inline transfer, attach packets for Have keys here:
    let have_keys: Vec<ReconcileKey> = reply.ranges.iter().flat_map(|r| match &r.mode {
        RbsrMode::Have(ks) => ks.clone(), _ => vec![] }).collect();
    drop(log);
    // (packets travel as separate reply frames at the transport layer — see Task 12; the sealed RbsrMessage
    //  carries the keys, the transport co-sends encrypt_channel_packet(event) for each Have key.)
    let _ = have_keys; // used by Task 12 wiring
    seal_rbsr_message(key, &reply).ok()
}
```

Decision recorded: the sealed `RbsrMessage` carries `Have` **keys**; the **transport** (Task 12) co-sends the encrypted event packets as additional reply frames, reusing `encrypt_channel_packet` and the existing inbound path. Provide `pub(crate) async fn events_for_have_keys(&self, msg: &RbsrMessage) -> Vec<SignedChannelEvent>` that resolves Have keys → events via `log.events_for_keys`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(engine_rbsr_respond)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_channel_log_engine.rs
git commit -m "feat(channel-log-engine): rbsr_respond (open→respond→seal) + Have-key resolution"
```

---

### Task 12: Transport — `rbsr/**` queryable + GET driver

**Files:**
- Modify: `src-tauri/src/event_loop.rs`

**Interfaces:**
- Consumes: `rbsr_respond`, `events_for_have_keys` (Task 11), `encrypt_channel_packet`, the existing adapter scaffolding (`spawn_channel_log_zenoh_adapter` at `:7679`, queryable pattern at `:7714`/`:7854`, GET driver at `:7942`).
- Produces: a registered `…/rbsr/**` queryable and a GET path that carries the sealed request payload + drains sealed-reply + `Have` event frames, with an explicit `.timeout()`.

- [ ] **Step 1: Write the failing test**

Transport needs live Zenoh; cover it via the Task 14 integration test. For this task, add a **key-parse unit test** (the one purely-logical piece) next to `parse_channel_backfill_key` (`:8363`):

```rust
#[test]
fn parse_rbsr_key_extracts_round() {
    assert_eq!(parse_rbsr_key("harmony/channels/aa/bb/rbsr/3"), Some(("aa".into(), "bb".into(), 3)));
    assert_eq!(parse_rbsr_key("harmony/channels/aa/bb/since/0/256"), None);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(parse_rbsr_key_extracts_round)'`
Expected: FAIL.

- [ ] **Step 3: Implement**

1. `parse_rbsr_key(&str) -> Option<(String,String,u32)>` mirroring `parse_channel_backfill_key`.
2. Declare a **separate** queryable on `harmony/channels/{cid}/{ch}/rbsr/**` inside `spawn_channel_log_zenoh_adapter` (a fifth task alongside the four existing). On each query: cap-check `query.payload()` len vs `MAX_RBSR_MESSAGE_BYTES` **before** reading; call `engine.rbsr_respond(payload).await`; if `Some(sealed_reply)`, `query.reply(key, sealed_reply)`; then for each event from `events_for_have_keys`, `query.reply(key, encrypt_channel_packet(event))` (separate frames, `ConsolidationMode::None`). If `None`, reply nothing (requester sees empty → fallback/again).
3. RBSR GET driver: when the backfill driver requests an RBSR round (Task 13), build `session.get(rbsr_key).payload(sealed_request).timeout(Duration::from_secs(10))` (explicit timeout — footgun fix), drain replies in the existing `select!` loop. The **first** reply frame is the sealed `RbsrMessage`; subsequent frames are encrypted event packets routed to the engine inbound path (same `subscriber_tx_qr` channel as today). Distinguish by position (frame 0 = message) or by attempting `open_rbsr_message` first and falling back to packet ingest.

Follow the exact patterns at `:7854` (queryable) and `:7985` (GET build) — copy `ConsolidationMode::None`, add `.timeout(...)`.

- [ ] **Step 4: Run test + manual compile**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(parse_rbsr_key)'` → PASS.
Run: `cd src-tauri && cargo clippy --locked -p harmony-app --features test-fixtures -- -D warnings` → clean.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/event_loop.rs
git commit -m "feat(event-loop): rbsr/** queryable + GET driver with explicit per-round timeout"
```

---

### Task 13: Backfill driver RBSR mode + vector fallback

**Files:**
- Modify: `src-tauri/src/channel_backfill.rs`, `src-tauri/src/community_channel_log_engine.rs` (driver request plumbing)

**Interfaces:**
- Consumes: the RBSR GET path (Task 12), `process_reply`/`initial_request` (Task 7), the existing `BackfillLatch`/`run_backfill_driver`, the existing vector path (`request_backfill_with_outcome`).
- Produces: an RBSR-first reconcile loop that falls back to the watermark-vector GET when round 0 draws zero replies, and to full reconcile (`since=None`) when `MAX_RBSR_ROUNDS` is hit.

- [ ] **Step 1: Write the failing test**

Driver logic is integration-shaped; add a **unit test of the fallback decision** (pull the decision into a pure helper):

```rust
#[test]
fn rbsr_round0_zero_replies_selects_vector_fallback() {
    assert_eq!(reconcile_mode_after_round0(/*rbsr_replies=*/0), ReconcileMode::VectorFallback);
    assert_eq!(reconcile_mode_after_round0(/*rbsr_replies=*/1), ReconcileMode::RbsrContinue);
}
#[test]
fn rbsr_round_cap_falls_back_to_full_reconcile() {
    assert_eq!(reconcile_mode_after_round(MAX_RBSR_ROUNDS, /*converged=*/false), ReconcileMode::FullReconcile);
    assert_eq!(reconcile_mode_after_round(3, /*converged=*/true), ReconcileMode::Done);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(rbsr_round0_zero_replies)'`
Expected: FAIL.

- [ ] **Step 3: Implement**

Add `enum ReconcileMode { RbsrContinue, VectorFallback, FullReconcile, Done }` and the two pure `reconcile_mode_*` helpers. Wire them into `run_backfill_driver`: on an RBSR `Request`, issue round 0; `reconcile_mode_after_round0(replies)` → if `VectorFallback`, run the existing watermark-vector `request_page` path (unchanged); else loop RBSR rounds (`initial_request`→GET→`process_reply`→next GET), ingesting `Have` packets, until `Done` or `FullReconcile` (then issue a `since=None` page). The periodic floor and epoch re-arm paths are unchanged.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(rbsr_round)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/channel_backfill.rs src-tauri/src/community_channel_log_engine.rs
git commit -m "feat(backfill): RBSR-first driver with vector + full-reconcile fallbacks"
```

---

### Task 14: Integration acceptance test (the ticket's bar)

**Files:**
- Modify: `src-tauri/tests/channel_backfill_integration.rs`

**Interfaces:**
- Consumes: the whole stack (Tasks 1–13). Reuse the two-engine harness already in this file (grep `returning_member_recovers_unseen_device_sub_max_hlc_event` — the ZEB-585 acceptance test — and clone its setup).

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn rbsr_recovers_within_device_out_of_order_hole_with_gap_sized_transfer() {
    // A and B converge on a backlog; B goes offline.
    let (mut a, mut b, ctx) = two_engine_channel().await;
    seed_converged_backlog(&mut a, &mut b, 30).await; // both hold 30 events
    let pre = list_bodies(&b).await;

    take_offline(&mut b).await;
    // device X (which B HAS seen — same device, higher max) posts X@3 (a LOW logical that sorts BELOW B's X max)
    // i.e. a within-one-device out-of-order hole: B holds X@5 but never received X@3.
    post_out_of_order_hole(&mut a, "deviceX", /*wall*/1500, /*logical*/3).await;
    // plus a cross-author sub-max event for parity with ZEB-585
    post_sub_max_cross_author(&mut a, "deviceZ").await;

    reconnect(&mut b, &ctx).await;
    // B reconciles via RBSR
    let report = await_backfill_quiescent(&mut b).await;

    let post = list_bodies(&b).await;
    assert!(post.len() == pre.len() + 2, "recovered both gap events: {post:?}");
    assert!(report.events_transferred <= 8, "O(gap), not O(history=30): {}", report.events_transferred);
}

#[tokio::test(flavor = "multi_thread")]
async fn old_style_requester_still_uses_vector_path() {
    // a requester that issues only the since/** vector GET (no rbsr/**) still catches up — fallback intact.
    // Drive the vector path directly and assert recovery, exactly as the ZEB-585 test does.
}
```

(Use the file's existing helpers; add `events_transferred` to the backfill report if not already surfaced — count `Have` packets ingested during the reconcile.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(rbsr_recovers_within_device_out_of_order_hole)'`
Expected: FAIL (helpers/behavior missing) — iterate until the assertions hold.

- [ ] **Step 3: Implement / wire**

Fill in any missing harness helpers (`post_out_of_order_hole`, `await_backfill_quiescent` returning a report with `events_transferred`). No new production code should be needed if Tasks 1–13 are correct; if the test exposes a gap, fix it in the owning module and note it.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(rbsr_recovers)' -E 'test(old_style_requester_still_uses_vector)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/tests/channel_backfill_integration.rs
git commit -m "test(rbsr): acceptance — within-device hole recovered with gap-sized transfer + vector fallback"
```

---

### Task 15: Wire-format pins + final full-suite gate

**Files:**
- Modify: `src-tauri/tests/wire_format/channel_log_fixtures.rs` (or the existing channel-log fixtures file — grep `watermark_vector_sealed_wire_bytes_pinned`)

**Interfaces:**
- Consumes: `encode_message` (Task 4), `seal_rbsr_message_with_nonce` (Task 5, deterministic nonce).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn rbsr_message_canonical_cbor_pinned() {
    let m = fixed_sample_rbsr_message(); // deterministic content
    let hex = hex::encode(channel_rbsr::encode_message(&m));
    assert_eq!(hex, "<FILL AFTER FIRST RUN>");
}
#[test]
fn rbsr_sealed_wire_bytes_pinned() {
    let key = fixed_channel_key();
    let sealed = seal_rbsr_message_with_nonce(&key, &fixed_sample_rbsr_message(), [7u8; 12]).unwrap();
    assert_eq!(hex::encode(sealed), "<FILL AFTER FIRST RUN>");
}
```

- [ ] **Step 2: Run, capture the bytes, pin them**

Run the tests once; they fail printing the actual hex. Paste the actual values into the `assert_eq!`s (the standard pin workflow used by `watermark_vector_sealed_wire_bytes_pinned`). Re-run → PASS. These pins lock the wire format against accidental drift.

- [ ] **Step 3: Full-suite gates (per-PR final)**

```bash
cd src-tauri
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

All must be clean/green. (Expect the nextest `--all-targets` run to be the long pole ~20 min; supervise with a wall-clock net.)

- [ ] **Step 4: Commit**

```bash
git add src-tauri/tests/wire_format/channel_log_fixtures.rs
git commit -m "test(rbsr): pin canonical-CBOR + sealed wire bytes for RBSR messages"
```

---

## Self-Review

**1. Spec coverage:**
- §1.1 coexistence/negotiation → Task 13 (fallback) + Task 12 (separate queryable). ✓
- §1.2 element hash + canonical order → Task 1 + Task 3. ✓
- §1.3 count-folded fingerprint + associativity → Task 2. ✓
- §1.4 pull-only bisection (Skip/Fingerprint/Have, leaf wholesale, MAX_RBSR_ROUNDS) → Tasks 6, 7. ✓
- §1.5 wire/transport (rbsr/** key, payload, ConsolidationMode::None, explicit timeout, engine-side seal, caps) → Tasks 4, 5, 11, 12. ✓
- §1.6 inline block transfer (encrypt_channel_packet reuse) → Task 11 decision + Task 12. ✓
- §1.7 trust model (channel-key gate) → Task 5 (AEAD) + Task 11 (`channel_key_ref` None → fallback). ✓
- §1.8 security (count-fold, optional randomized split, caps+AEAD) → Task 2 (count-fold test), Task 6 (split; randomized noted optional), Task 5 (caps). ✓
- §2.1–2.3 CDC chunk index (content-defined boundaries, in-memory, built in reload, incremental append, O(log n) aggregation) → Tasks 8, 9, 10. ✓
- §2.4 persistent/CasBook → out of scope (noted). ✓
- Acceptance test (within-device hole, O(gap), fallback) → Task 14. ✓
- Wire pins + gates → Task 15. ✓

**2. Placeholder scan:** The only deferred specifics are tunable consts (pinned with values) and the chunk-index `insert` perf-optimization (explicitly marked `// PERF:` with a correctness-first fallback that passes the parity test — a tracked, acceptable debt, not a silent gap). Wire-pin hex values are `<FILL AFTER FIRST RUN>` by the standard pin workflow (intentional, not a placeholder failure).

**3. Type consistency:** `ReconcileKey`, `RangeFingerprint`, `RbsrMessage`/`RbsrRange`/`RbsrMode`, `RangeReconcileSource`, `ChunkSummary`/`ChunkIndex`, `MAX_RBSR_MESSAGE_BYTES`, `MAX_RBSR_ROUNDS`, `LEAF_THRESHOLD`, `RBSR_PROTOCOL_VERSION` are used consistently across tasks. `min_key()`/`max_key()` are functions (clippy-friendly) referenced uniformly. `Have` carries keys at the protocol layer; the engine/transport bridge keys→packets (Tasks 11/12) — consistent.

## Notes for the executor

- **Per-task scope:** during Tasks 1–11 iterate with `-p harmony-app --lib` (or `--features test-fixtures` for the integration-visible items). Run the full `--all-targets` clippy/nextest only at Task 15 (lib changes relink ~97 integ binaries — ~50 min under `--all-targets`).
- **Liveness:** the final `--all-targets` nextest is ~20 min; supervise with a wall-clock net, don't assume hung.
- **Order independence:** Tasks 1–9 are pure/unit and can be reviewed independently; Tasks 10–14 integrate; Task 15 is the gate. Keep commits per task.
