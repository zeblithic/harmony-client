# ZEB-592 — Channel-log catch-up: range-based set reconciliation (Negentropy-style RBSR)

**Status:** design approved (2026-06-28, Jake — all four flagged calls approved). Drives a single PR covering **Slice 1 (RBSR core)** + **Slice 2 (CDC fingerprint cache)**. Slice 3 (retire the Part A watermark vector) is an explicit follow-up.

**Ticket:** ZEB-592 — Part B successor to ZEB-585 (Part A, the interim per-author watermark vector, merged as PR #364). Same arc as ZEB-584 (periodic full-reconcile floor, #358).

**Branch:** `channel-log-range-reconciliation`.

---

## Goal

Make channel-log reconnect catch-up transfer **only the symmetric difference** between two peers' event sets — independent of clock skew, arrival order, or device count — closing every gap a scalar or per-lane watermark leaves open, while preserving every shipped wire byte and the existing periodic full-reconcile backstop.

## Problem — what Part A (ZEB-585) still leaves open

Part A replaced the scalar `since: Option<Hlc>` with a per-author `(author, device_id)` **watermark vector**. That closed the observed cross-author offline-window gap and made the common reconnect O(diff) on the **wire**. Three limits remain:

1. **Version-vector metadata cost.** A per-author watermark vector is a version vector: its wire/metadata size scales with the number of author-devices, not with the actual difference (ZEB-585 spec §A.3). At scale (many members × devices × key rotations) the metadata can rival payload.
2. **Per-lane scalar blind spot.** A within-one-device non-contiguous hole — the member holds `X@5` but is missing `X@3` (out-of-order delivery within one device's stream) — is still filtered, because the per-device entry is `5` and `X@3 < 5`. Only the ~1h periodic full-reconcile floor heals it (ZEB-585 spec §A.2).
3. **O(history) responder disk.** The vector path cannot use the `SegmentDescriptor.range` segment skip (a never-seen lane may sit in any segment), so the responder reads **all** segments per page (ZEB-585 spec §A.6; the lock-hold refinement is ZEB-591). Wire is O(diff); disk is O(history).

Range-Based Set Reconciliation (RBSR) fixes all three: wire and steady-state disk both scale with the **difference**, and there is no per-author-monotonicity assumption — an out-of-order or skewed event simply changes the fingerprint of the range it falls in, which the next round detects and surgically retrieves.

## Decision — approved calls (2026-06-28)

1. **Coexist, don't replace.** RBSR is a new, additive Zenoh query family. The Part A vector path stays as the fallback; retiring it is a separate follow-up.
2. **Hand-roll the engine.** No `negentropy` crate — its footguns (load-everything-into-RAM, tombstone+insert mutation model) fight our segmented append log; the protocol core is small and we AEAD-seal it like everything else. Study the crate as reference only.
3. **In-memory CDC chunk-fingerprint index**, following the established `reaction_index` / `device_watermarks` pattern — not a persistent merkle/CAS store (that is a future optimization where the reserved `SegmentHandle::CasBook` would land).
4. **Inline block transfer** — missing events ride back in the RBSR reply stream as `encrypt_channel_packet`-sealed packets (the exact path today's catch-up uses); no separate CAS/DAG fetch.

The periodic full-reconcile floor (ZEB-584) stays **as-is** as the ultimate backstop under all paths.

---

# Slice 1 — RBSR core

## 1.1 Coexistence & capability negotiation

A new query family `harmony/channels/{cid_hex}/{ch_id_hex}/rbsr/{round}` runs **alongside** the existing `…/since/**` catch-up. The dedicated `rbsr/**` queryable is registered separately from the `since/**` queryable (the `since=None` branch of the existing queryable deliberately strips the GET payload — `event_loop.rs:7893` — so RBSR must not ride under that key family).

**Negotiation:** the requester attempts RBSR first. A GET on `rbsr/0` against a peer that predates this change matches no queryable → the reply stream closes empty. The driver detects "no RBSR responder" (zero replies on round 0) and **falls back** to the existing watermark-vector GET on `…/since/**`. New↔new peers reconcile via RBSR; new↔old peers via the vector path; the periodic floor backstops both. This PR removes **zero** existing wire paths.

## 1.2 Primitives

### Element hash (new)

Each `SignedChannelEvent` becomes an RBSR set element identified by

```
element_hash(event) = SHA-256(signed_set_canonical_cbor(event))   // [u8; 32]
```

`signed_set_canonical_cbor` already exists (`community_channel_log.rs:594`, currently private `fn`) and produces deterministic RFC-8949 canonical CBOR of the signed-set (all fields except `sig`). It is the same byte sequence the Ed25519 signature covers, so it is stable, content-derived, and identical across devices for identical content. **`MessageId` is NOT usable** — it is a random 16-byte value (`community_channel_log_engine.rs:1021`), not content-derived. Work: expose a `pub(crate) fn event_element_hash(&SignedChannelEvent) -> [u8; 32]` wrapper (one ciborium encode + one SHA-256 per event — pure CPU, no I/O).

### Canonical order (new, materialized at query time)

The set is totally ordered by

```
key(event) = (wall_ms, logical, device_id, element_hash)
```

i.e. the full `Hlc` lexicographic order (`Hlc::is_strictly_newer_than`, `owner_state_types.rs:331`) with `element_hash` as a deterministic tiebreaker for the (possible) cross-lane case of two devices sharing `(wall_ms, logical)`. The log is **not** stored in this order on disk (tail and segments are arrival-ordered; the manifest is sorted only by segment `range.0`), so the canonical order is materialized as a **view** at query time (Slice 1) and cached in the chunk index (Slice 2).

## 1.3 Range fingerprint

Over a canonical-order range, the fingerprint is the count-folded modular hash sum (Negentropy V1-adapted):

```
raw_sum = ( Σ over events in range:  LE_u256(element_hash) ) mod 2^256     // [u8; 32]
count   = number of events in range                                        // u64
fingerprint = SHA-256( raw_sum_le_32 || varint(count) )[..16]              // [u8; 16]
```

- **Associativity (load-bearing for Slice 2):** for disjoint adjacent sub-ranges, `raw_sum` adds mod 2²⁵⁶ and `count` adds. A wide range's fingerprint is computed by summing its sub-ranges' `(raw_sum, count)` and hashing **once** at the end — this is what lets cached chunk summaries aggregate in O(log n).
- **Cancellation-attack resistance:** folding `count` into the SHA-256 means an injected synthetic event whose hash is crafted to cancel a target's (forging a `raw_sum` match) still increments `count` and breaks the fingerprint. (Pure homomorphic-XOR/sum set hashes lack this — see §1.8.)

## 1.4 Bisection protocol (pull-only, stateless per GET)

RBSR is normally symmetric, but catch-up only needs the **requester** to pull what it lacks — the responder catches up independently via its own backfill driver and live gossip. So this is **one-directional, requester-pull** reconciliation: each round narrows the ranges where the responder holds events the requester is missing, and the responder ships those events inline.

**Message: a `version byte` + ordered list of ranges.** Each range:

```
{ upper_bound: BoundKey,        // (wall_ms, logical, id_prefix) — delta-encoded vs previous bound
  mode: Skip | Fingerprint | Have }
```

- `Fingerprint(payload: [u8;16])` — request→responder: "my view of this range hashes to X."
- `Skip` — responder→requester: this range agrees; nothing to do.
- `Have(events: Vec<EncryptedChannelPacket>)` — responder→requester: the responder's events in this (small, resolved-to-leaf) range, shipped inline (§1.6).

**State machine (requester drives; responder is stateless per GET):**

- **Round 0 (requester):** one `Fingerprint` covering the whole universe `[0, MAX]`, computed over the requester's own set.
- **Responder, per incoming `Fingerprint` range:** compute its own fingerprint over the same range.
  - Match → reply `Skip`.
  - Mismatch and `responder_count_in_range > LEAF_THRESHOLD` → **bisect** into sub-ranges (split point chosen by the responder's local event density; optionally a randomized non-50/50 ratio, §1.8) and reply one `Fingerprint` (of the responder's own view) per sub-range.
  - Mismatch and `responder_count_in_range ≤ LEAF_THRESHOLD` → reply `Have` with **all** the responder's events in that range. The range is small (≤ threshold), so shipping it wholesale is cheap; the requester's existing inbound path (signature + replay-tracker + known-`MessageId` check) **dedups** any it already holds, so no `IdList` negotiation round-trip is needed. (An `IdList` mode to trim the few redundant sends is a possible future optimization, not built here.)
- **Requester, per reply:**
  - `Skip` → range resolved.
  - `Have(events)` → ingest through the normal inbound path (dedup is free); range resolved.
  - `Fingerprint` sub-ranges → recompute own fingerprints over each; carry only the still-mismatching sub-ranges into the next round's request message.
- **Termination:** loop until a round's request message has **no mismatching ranges left** (every range came back `Skip` or was drained by a `Have`). Hard cap `MAX_RBSR_ROUNDS = 32`; on cap-exceeded the driver **falls back to a full reconcile** (`since=None` on the legacy path) as the safety net. Convergence is O(log n) rounds in the size of the symmetric difference.

Each GET is independent and the queryable is **stateless per-GET** (`event_loop.rs:7851`): round N's full input is the request message in the GET payload; nothing is retained server-side between rounds. This matches the queryable's existing contract exactly.

## 1.5 Wire protocol & transport

- **Key family:** `harmony/channels/{cid_hex}/{ch_id_hex}/rbsr/{round}`. The `{round}` segment is a routing/trace hint only; all round parameters live in the payload.
- **Request:** Zenoh GET, `ConsolidationMode::None` (load-bearing — the only mode that streams individual reply frames; `event_loop.rs:7985`), with the requester's RBSR message as the **GET payload**, and an **explicit per-round `.timeout()`** mirroring the root-fetch driver (`event_loop.rs:7342`). The current `since/**` backfill GET has **no** timeout — tolerable for one-shot, but fatal for multi-round if a peer completes round 0 and hangs on round 1; an explicit timeout is required here.
- **Reply:** the responder streams its RBSR message — `Fingerprint`/`Skip` ranges and any `Have` event packets — over the reply stream (one or more frames under `ConsolidationMode::None`). Stream close signals round done (`recv_async() → Err`, the existing completion signal); the `outcome_tx` oneshot fires and the driver starts the next round.
- **Sealing:** the entire RBSR message (request payload and reply frames) is AEAD-sealed with the per-channel key — `derive_channel_key(EpochKey, community_id, channel_id)` → `ChannelKey`, ChaCha20-Poly1305, 12-byte random nonce, wire `[nonce || ct || tag]` — with a **domain-separated** AAD `b"harmony-channel-rbsr-v1"` (distinct from Part A's `b"harmony-channel-wmv-v1"` and the reply-packet `b"harmony-channel-msg-v1"`). Sealing happens **engine-side** (the engine holds the channel key; the adapter driver holds only hex IDs — same constraint and seam Part A established): the engine seals each round's request and opens each reply; the responder's `read_for_query` closure (also on an engine, also holding the key) opens the request and seals the reply.
- **Caps (cap-before-alloc, on the responder, on the bytes view before decrypt — mirrors `MAX_PAIRING_WIRE_BYTES` at `event_loop.rs:5626`):** `MAX_RBSR_MESSAGE_BYTES` (reuse the 64 KiB ceiling), `MAX_RBSR_RANGES_PER_MESSAGE`, and the `MAX_RBSR_ROUNDS = 32` round cap. Over any cap → drop to the legacy/fallback path. AEAD authenticity gives malformed/tampered-message fallback for free (open failure → treat as no RBSR → fall back).

## 1.6 Block transfer (inline)

Events the responder ships in `Have` ranges are sealed with the existing `encrypt_channel_packet` and delivered through the **same reply path and engine inbound dispatch** the live subscriber and `since/**` catch-up already use (`event_loop.rs:7913`). The requester ingests them exactly as today (verify signature, replay-tracker check, append). No separate CAS/DAG fetch: channel events are small, already have a sealed reply-packet path, and attachments already travel through CAS independently. (CAS/DAG block-exchange — `harmony-content::dag` + `harmony-zenoh` CID routing — is noted as a future option only if event payloads ever grow large enough to benefit from dedup.)

## 1.7 Trust model (unchanged)

RBSR does not widen trust. A peer can only reconcile a channel whose epoch/channel key it holds (the AEAD seal on every RBSR message gates participation), and the responder serves only events it would already serve on the `since/**` path (same membership/authorization). The improvement is purely in the diff math.

## 1.8 Security

- **Count-folding** (§1.3) defeats hash-cancellation forgery of fingerprint matches.
- **Randomized bisection split-ratio** (optional hardening): when the responder bisects, choose the split point with a small randomized offset rather than always at the median, so an attacker cannot reliably keep a synthetic canceller co-located with its target across rounds. Randomness is per-response and need not be reproducible (vary by responder-local entropy; not part of the canonical-CBOR pin).
- **Caps + AEAD** (§1.5) bound resource use and authenticate every message.

---

# Slice 2 — CDC fingerprint cache (bounds responder disk to O(diff·log n))

## 2.1 Why segments can't be reused

Per-segment fingerprints are **not** cross-peer comparable: segment boundaries are arrival-dependent (the tail seals at `seal_threshold_events` in arrival order — `community_channel_log.rs:1629` — irrespective of canonical order), so two peers holding the identical event set partition it into different segments. RBSR range summaries must be keyed to boundaries that are a **function of content**, identical between any two peers with the same events.

## 2.2 Content-defined chunks over the canonical order

Run a rolling hash (Gear-style) over each event's `element_hash` **in canonical `key` order**; declare a chunk boundary when the low bits hit a target mask (tunable average chunk size, e.g. ~256–1024 events; hard min/max chunk-size caps to bound worst case). Because the boundary is decided by the content hash, two peers derive **identical chunks** for the same event set regardless of arrival order — so their cached chunk summaries are directly comparable. (Reference for the rolling-hash boundary logic: `harmony-content`'s FastCDC chunker / `harmony-db::prolly::chunker`; we adapt the boundary predicate, not the byte-chunking.)

## 2.3 In-memory chunk-fingerprint index

A new in-memory index on `ChannelLog`, following the `reaction_index` (ZEB-536) / `device_watermarks` (ZEB-585) pattern — built in the **same reload scan** that already rebuilds those indexes (`community_channel_log.rs::reload`; folded into the existing single pass over segments+tail — **zero new O(history) disk passes**), and maintained incrementally on `append`.

Per chunk, the index stores a summary (not the events):

```
ChunkSummary { first_key: Key, last_key: Key, count: u64, raw_sum: [u8; 32] }
```

- **Range-fingerprint query (O(log n)):** for an RBSR range `[lo, hi]`, binary-search the chunk list; sum `(raw_sum, count)` of fully-contained chunks (associative, §1.3) and hash only the partial boundary chunks' events on the fly. No per-query, per-round disk after the one-time reload build.
- **Incremental append:** a new event inserts at its canonical position (binary search by `key`). `raw_sum += LE_u256(hash)` and `count += 1` on its chunk are **O(1)**. If the event's hash creates a new content-defined boundary, the chunk **splits**: the two halves' summaries are recomputed by re-reading only that one chunk's events (bounded by the max-chunk-size cap, not history). Splits are ~1/chunk_size of appends, so amortized append cost stays well bounded.

Result: per-query and per-round responder disk → ~0 after reload; per-query CPU → O(diff·log n). This is the honest "bounds the disk" guarantee the user asked for with Slice 1+2.

## 2.4 Future: persistent / CAS-backed chunks

If the in-memory index proves heavy at very large per-channel scale, chunk summaries can be persisted (the reserved `SegmentHandle::CasBook { cid }` variant, `community_channel_log.rs:1532`, is where a CAS-backed chunk store would land) and shared via `harmony-content` CAS. Out of scope here; the in-memory index is the Slice-2 deliverable.

---

# Components & files

- **`community_channel_log.rs`**
  - Expose `event_element_hash(&SignedChannelEvent) -> [u8; 32]` (wraps the now-`pub(crate)` `signed_set_canonical_cbor`).
  - `seal_rbsr_message` / `open_rbsr_message` AEAD helpers (ChaCha20-Poly1305, AAD `b"harmony-channel-rbsr-v1"`, wire `[nonce||ct||tag]`); `MAX_RBSR_MESSAGE_BYTES`; cap-before-alloc on the bytes view.
  - RBSR message/range types (`RbsrMessage`, `RbsrRange`, `RbsrMode`, `BoundKey`) with canonical-CBOR encoding.
  - Slice 2: `ChunkSummary` type + an in-memory `chunk_index` on `ChannelLog`; built in `reload` (extend the existing rebuild pass), maintained in `append`; `fn range_fingerprint(lo, hi) -> (raw_sum, count)` accessor.
- **`community_channel_log_engine.rs`**
  - `rbsr_respond(request_msg) -> reply_msg` — the responder half: open the sealed request, for each range compute fingerprint (via the chunk index), bisect or `Have`-fill at the leaf, seal the reply.
  - `rbsr_request_round(ranges) -> BackfillQueryRequest`-equivalent — the requester half: build + seal this round's message, drive the GET, ingest `Have` events and the responder's range replies, return the narrowed range set for the next round.
  - Engine-side sealing of request/reply (engine holds `channel_key_ref()`).
- **`channel_backfill.rs`**
  - The driver gains an **RBSR mode**: round-0 GET on `rbsr/0`; if zero replies → fall back to the existing vector path; else loop rounds until all-`Skip` or `MAX_RBSR_ROUNDS` (→ full-reconcile fallback). The existing watermark paging loop and periodic floor remain for the fallback path. The `BackfillLatch` serial-GET shape is reused (RBSR rounds are inherently serial).
- **`event_loop.rs`**
  - New dedicated `rbsr/**` queryable (separate from `since/**`): parse key, cap-before-alloc on `query.payload()`, call the engine's `rbsr_respond`, reply with the sealed reply message (+ inline `Have` packets) under `ConsolidationMode::None`.
  - GET-request driver: forward the engine's sealed RBSR request as the GET payload **with an explicit `.timeout()`**; drain replies.
  - `spawn_channel_log_zenoh_adapter` + closure types extended for the RBSR query path.
- **Tests** — unit tests (fingerprint determinism + associativity, bisection convergence, leaf-threshold wholesale `Have`, CDC boundary determinism across arrival orders, count-fold cancellation resistance, seal/open + cap), an integration acceptance test (below), and canonical-CBOR wire pins for the RBSR message types.

# Acceptance test (the ticket's strengthened bar)

Two-engine integration test (extends `channel_backfill_integration.rs`):

1. Members **A** and **B** converge on a backlog.
2. B goes offline.
3. Device **X** posts an event whose HLC sorts **below** B's per-device max for X — i.e. the **within-one-device out-of-order hole** Part A's per-lane scalar filters and leaves to the floor (hold `X@5`, missing `X@3`). (Also include the cross-author sub-max case for parity with ZEB-585's test.)
4. B reconnects and reconciles via RBSR.

**Assert:** B recovers the missing event(s) — including the within-one-device hole the Part A vector path misses — and the responder shipped **~O(gap)** events (measured by reply/`Have` count), not B's whole history. Plus a backward-compat assertion: an old-style requester (no `rbsr/**`, vector GET only) still gets today's behavior (fallback intact).

# Test plan / gates

- Unit + integration + canonical-CBOR pins as above.
- `cargo fmt --all -- --check`
- `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- MSRV (`cargo check --locked --all-targets --features test-fixtures`) and frontend gates (no frontend change expected).

# Out of scope (this PR)

- **Slice 3:** retiring the Part A watermark-vector wire path — separate follow-up, after RBSR is proven cross-WAN.
- Persistent / CAS-backed chunk summaries (`SegmentHandle::CasBook`) — future optimization (§2.4).
- Symmetric (bidirectional push) reconciliation — unnecessary; the responder backfills independently (§1.4).
- Changes to the periodic full-reconcile floor, the `since/**` key family, or `SegmentDescriptor` on-disk format.

# References

- ZEB-585 spec Part B (`docs/superpowers/specs/2026-06-28-zeb-585-channel-log-diff-reconciliation-design.md` §§B.1–B.5) — the deep-research source for this design.
- `negentropy` (hoytech; rust-nostr fork) — reference RBSR impl (study only; not a dependency).
- `prollytree` — reference for CDC rolling-hash boundary logic.
- Reused in-tree: `harmony-content` (FastCDC chunker, CAS/DAG, Zenoh CID routing), `derive_channel_key` / `encrypt_channel_packet` (existing channel crypto).
- Prior art: `harmony-db::prolly` (ZEB-98/106/109) — surveyed, not reused (per-insert rebuild, CAS-coupled, wrong shape for an append log).

---

# As-built reconciliation (2026-06-29)

The implementation refined the design in four ways. This section is authoritative where it differs from the sections above.

## AB.1 Delivery split into two PRs

The RBSR **core** (this PR) ships everything except the live Zenoh wiring: the pure protocol (`channel_rbsr.rs`), the content-defined chunk index (`channel_chunk_index.rs`), the AEAD seal/open, `ChannelLogEngine::rbsr_respond`, `ChannelLog: RangeReconcileSource`, the backfill reconcile-mode helpers, the in-process acceptance test, and the wire pins — fully tested and CI-green. The **live Zenoh transport** (`rbsr/**` queryable + GET driver + backfill-driver RBSR path + Zenoh integration test) is split to **ZEB-593** because it is one atomic ~300–500-line change across the generic `spawn_channel_log_zenoh_adapter` signature (4 `read_for_query` call sites + the adapter request type + registry construction) with no incrementally-CI-green intermediate state. Until ZEB-593 lands, `rbsr_respond` / `events_for_keys` carry `#[allow(dead_code)]` (present in the binary for the transport to call). ZEB-592 closes when ZEB-593 merges.

## AB.2 Convergence: full ordered partition + Skip-coalescing

§1.4's state machine is realized with both sides emitting a **full ordered partition** of the universe each round (resolved/matching spans echoed as `Skip`, not dropped) so the receiver's positional lower-bound chain (`lo` advancing by each range's `upper`) stays aligned — dropping resolved ranges desyncs the chain and prevents convergence. Adjacent `Skip` ranges are **coalesced** so the resolved prefix/suffix collapse, keeping a message O(diff·log n), not O(total leaves explored). `process_reply` returns `None` (converged) only when no range mismatches.

## AB.3 Bisection via `split_key` (disk-bounded)

The responder picks its bisection split via a `split_key(lo, hi) -> Option<ReconcileKey>` trait method rather than materializing `keys_in_range` — so a wide early range never scans the whole history to find a median. `keys_in_range` is used only at the `≤ LEAF_THRESHOLD` leaf (`Have` wholesale).

## AB.4 Slice 2 is a local accelerator; CDC determinism is unnecessary

§2.1–2.2's emphasis on **content-defined chunk boundaries being identical across peers** turned out to be unneeded: chunk boundaries are never sent on the wire (only `range_fingerprint` *results* over bisection-chosen ranges are), so the chunk index is a purely **local** acceleration structure. Dropping the cross-peer-determinism requirement let the boundaries stay content-defined-but-local and **insert-stable** (no min/max caps), making incremental `append` maintenance a bounded single-chunk window rebuild.

As-built, `ChannelLog` holds an in-memory **`reconcile_entries`** (sorted `(ReconcileKey, element_hash)` mirror of the whole log) **plus** a `ChunkIndex` built over it. `range_fingerprint` is served from the chunk summaries (boundary chunks folded from the in-memory entries) — **disk-free**, eliminating the per-query O(history) segment rescan the watermark-vector path paid (§A.6). `range_count` / `keys_in_range` / `split_key` read the in-memory entries directly. This costs **O(n) memory** per channel (the entries mirror); the memory-frugal variant — chunk summaries only, with boundary events read from disk on demand (no full entries mirror) — is deferred as **Slice 2b**. The headline disk goal is met (queries are disk-free); the memory-frugality is the follow-up.

## AB.5 Acceptance proven in-process; Zenoh-level test deferred

The ticket's bar — a within-one-device out-of-order hole (hold `X@5`, missing `X@3`) recovered with ~O(gap) transfer — is proven by `rbsr_recovers_within_device_out_of_order_hole_over_real_logs`, which reconciles two real `ChannelLog`s through the full protocol + engine path (transferred = gap only). The end-to-end Zenoh-transport integration test is deferred to ZEB-593 (it needs the transport).
