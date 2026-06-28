# ZEB-585 — Channel-log catch-up: per-author watermark vector (interim) → range-based set reconciliation (follow-up)

**Status:** design approved (2026-06-28). Phased; this spec drives **Part A** (the interim per-author watermark vector). **Part B** (Negentropy-style RBSR) is documented future direction for a separate ticket.

**Ticket:** ZEB-585 (scale-grade follow-up to ZEB-584, which shipped the periodic full-reconcile stopgap — merged as #358).

## Goal

Make channel-log reconnect catch-up fetch a member's missing offline-window entries **without re-transferring the full history**, closing the cross-author offline-window gap that a single scalar HLC high-water mark leaves open — while preserving every shipped wire byte and the existing periodic full-reconcile backstop.

## Problem

Catch-up sends one `since: Option<Hlc>`; the responder serves events where `ev.at().is_strictly_newer_than(since)` (`community_channel_log_engine.rs::collect_events`). A returning member seeds `since = log_max_hlc()` — a single **global** ceiling.

A scalar max is **not a completeness certificate**: `max_hlc = M` only asserts "the newest event I hold is M," not "I hold every event ≤ M." A peer's offline-window message whose HLC sorts **below** M (skewed-low wall-clock, never received) is filtered out forever. Live-discovered cross-WAN during the ZEB-526 B2 relaunch (ZEB-584).

ZEB-584's fix — the periodic anti-entropy floor re-arms with `since = None` (full reconcile, ~1 h + 0–10 min jitter, `channel_backfill::PERIODIC_RESYNC_FLOOR_MS`) — **heals** the gap but at **O(full-history) per resync interval** wire cost. Fine at alpha scale; poor at real scale (busy channel × many channels × periodic full re-pull).

## Decision — phased

Selected 2026-06-28 (Jake): **interim per-author watermark vector now; principled range-based set reconciliation as a deep-research-informed follow-up.**

- **Part A (this PR):** replace the scalar `since` with a per-author (per authoring-device) watermark vector. Closes the **cross-author** gap (the actual observed bug) and turns the common reconnect catch-up into O(diff). Small, additive wire change, RC-safe.
- **Part B (follow-up ticket):** Negentropy-style range-based set reconciliation — the complete, O(diff·log n), skew-and-arrival-order-proof fix. Out of scope here; documented below so the research is not lost.

The periodic full-reconcile floor stays **as-is** (Jake's call) as the within-author completeness backstop under both Part A and Part B.

---

# Part A — Per-author watermark vector (this PR)

## A.1 Core change

`since` becomes a **watermark vector** keyed by the **`(author, device_id)` lane**: `{ (author, device_id) → (wall_ms, logical) }` — the member's max HLC per authoring lane. The lane key matches the replay tracker's lane identity (`replay_tracker_independent_lanes_per_author`): two authors may legitimately share a `device_id`, so keying by device alone would collapse their lanes and let one author's watermark suppress the other's events.

The responder serves an event when, for its `at.device_id`, the requester's vector either:
- has **no entry** (requester has nothing from that device → serve all of it), or
- has an entry the event exceeds: `(ev.wall_ms, ev.logical) > (entry.wall_ms, entry.logical)`.

**Why `(wall_ms, logical)`, not full `Hlc`:** within one device's stream `device_id` is constant, so `Hlc`'s lexicographic order `(wall_ms, logical, device_id)` collapses to `(wall_ms, logical)`. An HLC guarantees a single device's own successive events strictly increase, so a per-device scalar **is** a completeness certificate *within that device's append-only stream*. Storing `(wall_ms, logical)` avoids the redundant device_id in the value (it is the map key).

This closes the observed bug: the returning member had **no entry** for the offline-window peer's device, so it now pulls that device's events in full regardless of how their HLC sorts against the member's global max.

## A.2 Residual gap (documented, backstopped — not fixed here)

A **within-one-device** non-contiguous hole — the member holds X@5 but is missing X@3 (out-of-order delivery within one device's stream) — is still filtered, because the per-device entry is 5 and X@3 < 5. This is rare (a device appends in HLC order; gossip usually delivers in order) and is exactly what the **periodic full-reconcile floor** backstops within ~1 h. Part B (RBSR) is the durable fix. This spec does **not** attempt to close it.

## A.3 Limitation — version-vector-class metadata (accepted, bounded)

A per-author watermark vector **is** a version vector: its size scales linearly with the number of distinct author-devices in the channel. At large scale (many members × devices × key rotations) this becomes a metadata tax that can rival payload size. Accepted as an **interim** cost because:
- It is a strict improvement over O(full-history)-per-reconnect.
- It is **bounded** (A.5): over the byte cap, the requester drops the payload and degrades to today's scalar path + the floor backstop.
- Part B (RBSR) supersedes it — RBSR's wire cost scales with the *difference*, not the device count — at which point the vector wire path is removed.

## A.4 Wire protocol — additive, encrypted, zero break

The catch-up query is a Zenoh GET keyed `harmony/channels/{cid}/{ch}/since/{hlc_hex}/{limit}` (`event_loop.rs:7950`), served by a **dedicated** queryable (`event_loop.rs:7836`) that parses `since`/`limit` from the **key-expr**.

**Change:** keep that key-expr **exactly as-is** (scalar `since = log_max_hlc()` still in the key), and add the watermark vector as an **optional encrypted CBOR payload** on the GET (`session.get(key).payload(ciphertext)`).

- **New responder** sees a payload → decrypts, decodes, applies the per-device filter (efficient, complete cross-author).
- **Old responder** ignores the payload → uses the key's scalar `since` → **today's exact behavior**. No regression.
- **New responder, old requester** (no payload) → `query.payload()` is `None` → scalar path. No regression.
- **Periodic full-reconcile** (`since = None`, no payload) is untouched — the floor for within-author gaps and any mixed-version pair.

Verified viable against the in-tree Zenoh **1.9.0**: the queryable read path `query.payload().map(|p| p.to_bytes().to_vec())` is already used at `event_loop.rs:5576`; `.attachment()` on a builder at `:5557`. The GET-with-payload mechanism is proven in this exact version.

**Encryption (Jake's call):** AEAD-seal the vector with the **same per-channel key the reply packets use** — `derive_channel_key(EpochKey, community_id, channel_id)` (HKDF-SHA256) → `ChannelKey`, `ChaCha20Poly1305`, 12-byte random nonce, wire `[nonce || ct || tag]`, with a **distinct** AAD `b"harmony-channel-wmv-v1"` (domain-separated from the reply-packet AAD `b"harmony-channel-msg-v1"`; mirrors `encrypt_channel_packet`).

**Where the sealing happens (planning finding):** the requester-side adapter driver that builds the GET (`event_loop.rs:7913`) does **not** hold the channel key — it has only the hex IDs (`community_id_hex_qr`, `channel_id_hex_qr`). The key lives on the engine (`channel_key_ref()`, reachable on the responder via the `read_for_query` closure). So the **engine seals the vector before the request leaves it**: `request_backfill_with_outcome` computes + seals the vector and stores the opaque ciphertext in `BackfillQueryRequest.watermark_sealed: Option<Vec<u8>>`; the GET driver forwards those bytes verbatim as the GET payload; the responder's `read_for_query` closure (also on an engine, also holding the key) opens it. Jake's intent — AEAD-encrypted with the channel key — is preserved; only the seal *site* moves to the engine.

**When to attach:** seal a vector **iff `since.is_some()`** (a normal catch-up). The periodic floor and a fresh joiner both pass `since = None` → **no vector** → responder serves the full reconcile, exactly as today. This keeps the floor's completeness semantics untouched.

Benefits unchanged: (1) no cleartext-metadata widening beyond the status-quo key-expr (`cid`/`ch`/scalar-`since`); (2) AEAD authenticity gives the **malformed/tampered-vector fallback for free** — open failure → treat as no payload → scalar path.

## A.5 Cap-before-alloc (security)

Mirror the established precedent at `event_loop.rs:5626` (the `MAX_PAIRING_WIRE_BYTES` check that "MUST run BEFORE the heap allocation," CodeRabbit PR #63): introduce `MAX_WATERMARK_VECTOR_BYTES`, checked **on the responder** on the payload **bytes view before decrypt/decode**. Over cap → ignore the payload, serve via the key scalar. On the **engine seal side** (requester): if the sealed vector would exceed the cap, the engine attaches **no** `watermark_sealed` (degrade to scalar + floor) — simplest correct degradation; top-N-most-recent-device subsetting is a possible future refinement (YAGNI for this PR). Choose the cap generously (e.g. ≥ 64 KiB ≈ 1000+ devices) so real early-scale communities never hit it; it is a safety valve against pathological/malicious vectors.

## A.6 Paging & convergence

`collect_events` caps each page at `effective_limit` and the driver (`channel_backfill::run_backfill_driver`) re-arms and re-queries. For the vector path the driver's watermark re-reader produces the **vector** (via `log_watermark_vector()`) instead of the scalar. After a page, the requester ingests the served events, raising its per-device maxes, so the next query's vector advances and the responder serves the next batch. The responder iterates in stored order (segments ascending by `range.0`, then tail); each page serves the oldest-missing events first. Progress is guaranteed because ingestion strictly advances at least one per-device watermark until drained; an event is never re-served once its device watermark passes it.

**Disk-scan cost (Part A, accepted):** unlike the scalar path (which skips whole segments older than `since` via `SegmentDescriptor.range`), the vector path cannot skip by global range — a never-seen device's events may sit in any segment — so the responder reads **all** segments per vector page. The scarce resource over WAN is **wire** bytes, and that stays O(diff); the O(history) **disk** read is a local cost accepted for the interim. Part B's per-segment fingerprint summaries (content-defined-chunked) are what bound the disk side; out of scope here.

## A.7 Components & files

- **`community_channel_log.rs`** —
  - `WatermarkVector` type (`BTreeMap<(OwnerAddr, String), (u64, u32)>`, canonical CBOR — keyed by the `(author, device_id)` lane).
  - an **in-memory `device_watermarks` index** on `ChannelLog` (`BTreeMap<(OwnerAddr, String),(u64,u32)>`), maintained in `append` (raise the entry for the `(ev.author(), ev.at().device_id)` lane) and **rebuilt in `reload`** by scanning — mirrors the existing `reaction_index` (ZEB-536), so `ChannelLog::watermark_vector()` is O(lanes), not an O(history) rescan per query.
  - `seal_watermark_vector(&ChannelKey, &WatermarkVector) -> Result<Vec<u8>>` / `open_watermark_vector(&ChannelKey, &[u8]) -> Result<WatermarkVector>` AEAD helpers (ChaCha20-Poly1305, AAD `b"harmony-channel-wmv-v1"`, wire `[nonce||ct||tag]`), plus `MAX_WATERMARK_VECTOR_BYTES`; `open_*` runs the cap-before-alloc check on the bytes view first.
- **`community_channel_log_engine.rs`** —
  - `log_watermark_vector()` delegating to `ChannelLog::watermark_vector()`.
  - `collect_events_vector(vector, limit, keep)` parallel to `collect_events` — same segment-then-tail walk, but the per-device filter (no global-range segment skip; a never-seen device may sit in any segment) plus `list_messages_vector` / `list_post_events_vector` wrappers.
  - `BackfillQueryRequest` grows `watermark_sealed: Option<Vec<u8>>` (additive); `request_backfill_with_outcome` seals the vector **iff `since.is_some()`** and under cap (else `None`).
  - the `read_for_query` closure gains a `watermark_sealed: Option<Vec<u8>>` param: on `Some`, `open_watermark_vector(channel_key_ref(), ..)` then `list_messages_vector`; on `None` (or open failure), the existing `list_messages` scalar path. Reply-packet encryption unchanged.
- **`channel_backfill.rs`** — **unchanged.** The driver's watermark closure still returns `Option<Hlc>` for the key-expr; the vector is sealed inside the engine's request method, so `run_backfill_driver`'s generic signature does not change. Periodic floor unchanged.
- **`event_loop.rs`** — the query-request driver (`:7913`) forwards `req.watermark_sealed` as the GET payload when present (`session_qr.get(&key).payload(bytes)`); the dedicated queryable (`:7836`) reads `query.payload()`, runs the `MAX_WATERMARK_VECTOR_BYTES` cap-before-alloc check, and threads the (still-sealed) bytes into the now-3-arg `read_for_query` closure. `spawn_channel_log_zenoh_adapter` + the `read_for_query` boxed-closure type updated for the new param.
- **Tests** — `tests/channel_backfill_integration.rs` gains A.8; a canonical-CBOR pin for `WatermarkVector`; unit tests for the index, `collect_events_vector`, and the seal/open + cap helpers.

## A.8 Acceptance test (the ticket's bar)

Two-registry integration test (extends `channel_backfill_integration.rs`):
1. Member **B** builds a backlog; its global max HLC is from device B (high `wall_ms`).
2. B "goes offline."
3. Device **X**, which B has **never seen**, posts an event whose HLC sorts **below** B's global max (skewed-low `wall_ms`).
4. B reconnects and runs catch-up with the watermark vector.

**Assert:** B receives X's event (gap closed — the scalar path misses it), **and** the responder served **~O(gap)** — only X's events, not B's whole history (measured by reply count). Proves both correctness and the wire-volume goal.

A second assertion pins backward-compat: a requester sending **no payload** still gets today's scalar behavior (no regression).

## A.9 Test plan / gates

- New unit tests: `watermark_vector()` correctness (per-device max across segments+tail); `collect_events_vector` filter (no-entry → serve all; entry → serve newer-only); cap-before-alloc rejects over-cap payload before decode; AEAD decrypt-failure → scalar fallback.
- A.8 integration test (cross-author sub-max-HLC gap, O(gap) wire volume) + backward-compat assertion.
- Canonical-CBOR pin for `WatermarkVector`.
- `cargo fmt --all -- --check`
- `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- `cargo nextest run --locked --workspace --all-targets --features test-fixtures`

## A.10 Out of scope (this PR)

No prolly-tree / RBSR; no CAS-for-segments (`SegmentHandle::CasBook` stays reserved); no change to the periodic floor, the key-expr format, or `SegmentDescriptor`. The vector wire path is additive and removable when Part B lands.

---

# Part B — Range-based set reconciliation (follow-up ticket; documented future direction)

> Not built in this PR. Captured here from the 2026-06-28 deep-research report so the follow-up ticket can reference it. The follow-up gets its own spec → plan → PR.

## B.1 Recommendation: Negentropy-style RBSR (not MST/prolly-tree)

For an HLC-ordered, content-hashed, append-mostly log over untrusted P2P links with mobile endpoints, **Range-Based Set Reconciliation (Negentropy variant)** is preferred over Merkle Search Trees / prolly-trees because it **decouples reconciliation from physical storage layout** — it runs over the existing sorted log computing range fingerprints on the fly, with no rigid probabilistically-balanced tree to maintain/rebalance (better mobile-memory story). It resolves in logarithmic round-trips, has no capacity ceiling (unlike IBLT/PinSketch sketches, which fail or go superlinear past their preallocated capacity), and its wire cost scales with the symmetric **difference**, not the device count (unlike the Part A version vector).

An out-of-order event with an artificially low HLC alters the fingerprint of its historical range; the next pass detects the mismatch, bisects, and surgically retrieves it — no full-log transmission.

## B.2 Wire-protocol sketch (Negentropy V1-adapted)

- **Canonical order:** events sorted by `(HLC, content-hash)` — HLC for chronological locality, hash as deterministic tie-breaker. Universe `[0, u64::MAX]`.
- **Range fingerprint:** treat each 32-byte event hash in the range as a little-endian integer; sum mod 2²⁵⁶; append the range's **event count** as a varint; SHA-256 the concatenation; take the first 16 bytes.
- **Messages:** version byte + ordered list of ranges, each `{ delta-encoded upper-bound timestamp, id-prefix (boundary disambiguation), mode, payload }`, mode ∈ `Skip(0) | Fingerprint(1) | IdList(2)`.
- **State machine:** initiator sends one Fingerprint over the whole universe. On a mismatch the responder **bisects** into sub-ranges (split by local count for density), recomputing fingerprints; below a small count threshold it switches to **IdList** (literal 32-byte hashes). Each side, on an IdList, marks remote-missing IDs for block transfer and pushes its own locally-missing IDs. Loop until a message is all-`Skip` (difference resolved).

## B.3 CAS-segment reuse via content-defined chunking (optimization, not prerequisite)

Base RBSR needs no CAS changes. To *cache* segment fingerprints and reuse them as range summaries, both peers must derive **identical** segment boundaries — which append-order sealing does **not** give (confirmed: `SegmentDescriptor.range` partitions differ by arrival order). Fix: **content-defined chunking** at seal time — pass each ordered event hash through a rolling hash (Gear/Rabin); declare a boundary when the low bits hit a target (e.g. 12-bit mask ≈ 4096 events/chunk), with a hard max-chunk cap to bound mobile memory. Then cache each chunk's Negentropy fingerprint in its descriptor; a broad-range fingerprint aggregates fully-contained chunk fingerprints via the associative modular sum in log time, hashing only boundary remainders on the fly. This is where `SegmentHandle::CasBook { cid }` (currently reserved) would land.

## B.4 Security (untrusted peers)

Pure homomorphic-XOR set hashes are vulnerable to **cancellation attacks** (craft synthetic events whose hashes negate a target's, forging a fingerprint match to censor the target). Negentropy mitigates by folding the **event count** into the SHA-256 fingerprint: an injected synthetic event increments the count and breaks the hash. Optional hardening: **randomized bisection split-ratio** (not always 50/50) so an attacker cannot reliably keep synthetic cancellers in the same sub-range as the target.

## B.5 Rust crates to study

- `negentropy` — https://github.com/hoytech/negentropy (+ rust-nostr fork https://github.com/rust-nostr/negentropy). The reference RBSR impl. **Footguns:** do not load the whole dataset into RAM (use memory-mapped/paginated adapters); model mutations as tombstone + insert (no in-place update).
- `prollytree` — https://github.com/zhangfengcdt/prollytree — reference for the CDC rolling-hash boundary logic (B.3). Enforce hard max-chunk caps.
- `merkle-search-tree` — https://crates.io/crates/merkle-search-tree — history-independent MST reference (rejected as core for us: rigid hash-prefix layout, incompatible with reusing existing non-MST CAS chunking).
- `iroh-willow` / Willow — https://willowprotocol.org — multi-dimensional RBSR; powerful but heavy (subspace/namespace parameterization); useful for the "bisect where data exists, not empty numeric space" idea.
