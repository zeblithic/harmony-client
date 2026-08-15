# ZEB-932 — Voting-log RBSR (replace the periodic full-dump with range-based reconciliation) — Design

**Ticket:** ZEB-932 (child of ZEB-909; carve-out of the ZEB-916 R6b study)
**Status:** design, pending review
**Author:** Koya (Claude)
**Date:** 2026-08-15

## 1. Goal & motivation

The voting plane is the last sync path paying an **unbounded full-dump cost**. `read_backfill_frames` (`community_voting_log_engine.rs:543`) re-ships **O(all live events)** per community every `VOTING_BACKFILL_INTERVAL` (300 s, `lib.rs:58284`), one CBOR frame per event, over a Zenoh queryable answered with `ConsolidationMode::None` — so the requester pays **O(peers × events)** fan-in every round. The receiver dedups correctly against an order-independent `HashSet` of coordinates, but the *wire* cost is the entire voting log every interval, growing monotonically with poll history.

The ZEB-933 fleet measurement (2026-08-14) settled the go/no-go: voting full-dump volume is **perfectly linear at ~304 B/event** (R² ≈ 1.0 across a 375× range, 2 → 750 events), re-shipped every 300 s per requester × responder → **STRONG GO** for RBSR.

**Goal:** bring the proven channel-log RBSR machinery (ZEB-592/593) to the voting log so catch-up cost tracks the *diff* between two peers, not the total log size, while preserving every correctness invariant of the current admission path.

## 2. Approach (decision recorded)

**Approach A — Live-set RBSR + full-dump backstop.** RBSR becomes the primary incremental catch-up path; the existing full-dump is retained as the fallback (no RBSR responder / RBSR failure / round-cap) and as a periodic anti-entropy backstop at a raised interval (~1 h + jitter, down from every 300 s). **No change to the 90-day retention / archival path.** This mirrors the channel-log design exactly (RBSR-first, watermark-vector fallback, ~1 h `since=None` periodic floor).

Rejected alternative — *Replicated archive cut* (make the archive predicate a replicated fact so RBSR converges hard over the full universe and the full-dump can be retired). Cleaner end state but changes the correctness-adjacent retention semantics and adds test surface; over-engineering while voting volume is sparse. Left as a future follow-up if fleet data later shows frequent archive-window fallbacks.

**Out of scope (deferred, unchanged from ticket):** the bidirectional *reverse-delta* half of R6b (this is the pull/forward half only); community-state RBSR; any archival-semantics change.

## 3. Why a blind port fails: the archive boundary

`archive_finalized_polls` (`community_voting_log.rs:1348`) physically **drops** a finalized poll's per-ballot events (`retain`) ~90 days after finalize, keyed on each node's **local wall clock** (`now_wall_ms - fin_at > NINETY_DAYS_MS`, where `fin_at` is the *replicated* `PollResult.hlc.wall_ms`). Key facts that shape the design:

- There is **no replicated archive cut** in the data model. The only archived-vs-live signal is `PollMeta.lifecycle == Archived`, which lives in materialized state, not on the wire.
- Archival is decided **per-poll on finalize-time**, so archived and live events are **interleaved** in `canonical_key` order — the live set is *not* a contiguous `[lo, hi)` range.
- While a poll sits in its **mid-archival window** (peer A has pruned it, peer B has not yet crossed its local horizon), the two peers hold genuinely unequal event sets over that key range. A naive full-universe RBSR fingerprint there **never matches** → the protocol bisects to the round cap → falls back to a full pull.

**No data loss results** — the receiver's `seen_coords` set is never pruned on archive (`community_voting_log_engine.rs:192-194`), so a re-advertised archived event is dropped, never resurrected. The cost is *wasted rounds*, bounded per-poll by the clock-skew + archival-cadence gap between the two peers.

**Approach A handles this by construction:** the reconcile set is built from `log.events`, which has *already* had archived ballots pruned out — so archived events are simply out of RBSR scope. Residual divergence (a poll mid-window) costs at most one round-cap → full-dump fallback for that round, which the retained periodic backstop already covers. The design does **not** attempt hard convergence across the boundary.

## 4. Architecture: reuse map

The channel-log RBSR implementation (ZEB-592/593) is deliberately layered so the protocol core is log-agnostic. This design reuses that core and adds voting-specific adapters.

### Reuse verbatim (generic, no change)
- **`channel_rbsr.rs`** — the `RangeReconcileSource` trait, `RangeFingerprint`, `RbsrMode`/`RbsrRange`/`RbsrMessage`, `validate_message`, and the pure state machine (`initial_request` / `respond` / `process_reply`). `ReconcileKey = (u64, u32, String, [u8;32])` is exactly the shape voting's `canonical_key` already returns.
- **`channel_chunk_index.rs`** — `ChunkIndex` disk-bounded fingerprint accelerator; keyed only on `ReconcileKey` + hash, zero channel coupling.
- **`event_loop.rs` transport scaffolding** — `RbsrAdapterHooks`, `RbsrStep`, `drive_rbsr_rounds`, `rbsr_get_frames`, `format_rbsr_key`/`parse_rbsr_key`, the `MAX_RBSR_ROUND_BYTES`/`RBSR_FRAME_OVERHEAD` caps, and the `rbsr/**` queryable task block. Only the topic prefix is channel-specific.
- **`channel_backfill.rs` scheduling logic** — `ReconcileMode`, `reconcile_mode_after_round0` / `reconcile_mode_after_round`, the periodic-floor + jitter constants. Reused as the shape of the voting requester's RBSR-first strategy.

> **Refactor note:** where these modules currently hard-code the `harmony/channels/...` topic prefix or a `channel`-named symbol, extract the prefix to a parameter rather than copy-pasting. Prefer parametrizing the existing generic code over forking it; only fork when a genuine behavioral difference forces it. Any such extraction is a mechanical, separately-reviewable step.

### New, voting-specific (this ticket)
1. **`impl RangeReconcileSource for VotingLog`** + a maintained reconcile index (§5.1).
2. **`voting_reconcile_key(event)`** — thin wrapper over the existing `canonical_key` (already `(wall_ms, logical, device_id, event_hash)`); the content hash already exists via `sha256_of_signing_bytes` / `event_hash_of`. **No new hashing primitive.**
3. **Voting RBSR seal/open** + a fresh domain-separated `VOTING_RBSR_AAD` and a voting message cap (§5.2).
4. **Voting engine halves** — `rbsr_respond` / `rbsr_build_initial` / `rbsr_ingest_and_next` (§5.3), feeding fetched bodies into the existing `apply_backfilled_event`.
5. **Transport wiring** — a voting `rbsr/**` queryable + RBSR-first requester driver (§5.4).
6. **Backstop retune** — raise the full-dump interval 300 s → ~1 h + jitter (§5.5).

## 5. Components

### 5.1 `RangeReconcileSource for VotingLog` + reconcile index

Mirror `community_channel_log.rs:2573-2607`. Maintain a **sorted** `reconcile_index: Vec<(ReconcileKey, [u8;32])>` (key + element hash) plus a `ChunkIndex`, and answer the four trait methods (`range_fingerprint`, `range_count`, `keys_in_range`, `split_key`) over half-open `[lo, hi)` ranges from that index (the fingerprint delegating to the chunk index).

Source events: `VotingLog.events` (the flat `Vec<SignedVotingEvent>`), sorted by `canonical_key`. Because `log.events` arrival order is explicitly *not* correctness-bearing, the index MUST sort by `canonical_key`, never rely on `events` order.

**Index-maintenance seams (three, not two):**
- **build-on-load** — after `poll_restore` / snapshot load, rebuild the index (mirror `rebuild_reconcile_index`).
- **insert-on-append** — at the single append choke point, incrementally `ChunkIndex::insert` the new key.
- **drop-on-archive** — *voting-specific, absent in the channel log:* `archive_finalized_polls` mutates `log.events` (drops pruned ballots), so it MUST also remove those keys from the reconcile index (and rebuild the affected chunks). **This is load-bearing:** if the index advertises a key whose body was archived away, the responder's `events.len() == have_keys.len()` check fails and it returns `None` (a needless fallback), or worse advertises an unbackable key. A test asserts the index and `log.events` stay in lockstep across an archive.

**Have-key → body resolution (`events_for_keys` analogue):** resolve advertised keys to full `SignedVotingEvent` bodies with the same discipline as `community_channel_log.rs:2520`: dedup by distinct key, and **require the resolved count to equal the requested count** — never advertise a key you cannot back with a body. Resolution binary-searches the sorted index for the key, then fetches the body (keep a key→body accessor that is robust to `log.events` index shifts from archival; do not store raw `Vec` indices across an archive).

### 5.2 Seal/open + domain separation

Mirror `seal_rbsr_message` / `open_rbsr_message` / `open_rbsr_message_with_any` (`community_channel_log.rs:1002-1045`): wire = `[12B nonce][ChaCha20-Poly1305(key, cbor(msg), AAD)]`, cap-checked before both encrypt and decrypt.

- **New constant `VOTING_RBSR_AAD = b"harmony-voting-rbsr-v1"`** — MUST be distinct from every existing AAD (`RBSR_AAD`, the channel packet/watermark AADs, and the voting live/backfill AADs). **This separation is load-bearing for the frame classifier** (§5.3): an inline `Have` event packet (voting-packet AAD) must never open as an RBSR message, which is how the requester tells a reply frame from an event frame.
- **`MAX_VOTING_RBSR_MESSAGE_BYTES`** — a voting analogue of `MAX_RBSR_MESSAGE_BYTES` (64 KiB is a fine start).
- **Epoch key (mirror ZEB-920):** the voting backfill already re-encrypts each frame under the *current* tier-3 community epoch at serve time. The RBSR reply **and** its `Have` event packets MUST be sealed under **one** consistent epoch key; a rotation between two fetches would split epochs and silently lose events. Seal reply + packets together under a single `encrypt`-epoch snapshot; open under `[current, previous]` candidates.

### 5.3 Engine halves

Mirror `community_channel_log_engine.rs:1998-2135`:

- **`rbsr_build_initial() -> Vec<u8>`** — round 0: seal a whole-universe `Fingerprint` message.
- **`rbsr_respond(sealed_request) -> Option<(sealed_reply, Vec<packet>)>`** — open under epoch candidates; run `channel_rbsr::respond` against the `VotingLog` source; resolve `Have` keys → bodies (§5.1), enforcing `bodies.len() == have_keys.len()` (else return `None` → requester falls back); seal reply + each body-as-voting-packet under one epoch key. Return `None` on any open failure or resolution shortfall — never a partial answer.
- **`rbsr_ingest_and_next(frames) -> RbsrStep`** — classify each frame by attempting to open it as a sealed `RbsrMessage` (that is the reply; anything that fails is an inline `Have` voting packet). Route `Have` packets through the **existing `apply_backfilled_event`** path (decode → skew guard → length guard → `seen_coord` dedup → verify@hlc → eligibility → apply → record) — RBSR is a different *delivery* path, not a different *trust* path. Guard: if a **second** sealed reply appears (`saw_extra_reply`), bail to `RbsrStep::Failed` (multiple holders with divergent logs could falsely converge → fall back to the dedup-tolerant full-dump). Then `channel_rbsr::process_reply` computes the next partition; `None` ⇒ `Converged`.

### 5.4 Transport wiring

Add to the voting Zenoh adapter (currently `event_loop.rs:10522-10813`):
- **Responder:** declare a `harmony/community/{id_hex}/voting/rbsr/**` queryable alongside the existing live + backfill topics; on a well-formed request (guard `parse_rbsr_key`, cap before alloc, payload-less GET → reply nothing), call `rbsr_respond` and stream `[sealed_reply, have_packet_1, …]` via `query.reply`.
- **Requester:** on each backfill trigger, `drive_rbsr_rounds` **first** (round 0 = `initial`, then loop to `MAX_RBSR_ROUNDS`), issuing GETs with `ConsolidationMode::None`, **`Locality::Remote`** (so the node's own self-reply doesn't force premature convergence and "0 frames on round 0" cleanly means "no remote RBSR responder"), 10 s timeout, `MAX_RBSR_ROUND_BYTES` drain cap.
- **Fallback wiring** (reuse `ReconcileMode` shape): 0 replies on round 0 → `VectorFallback` → run the existing full-dump GET. Converged → done. Round-cap or `Failed` → full-dump. This keeps old peers (no `rbsr/**` queryable) and archive-window rounds correct via the unchanged full-dump path.

### 5.5 Backstop retune

Split the current single 300 s pull into two roles:
- **RBSR-first catch-up** on the existing triggers (spawn/join, reconnect, periodic).
- **Periodic full-dump backstop** raised from 300 s to a ~1 h floor + jitter (mirror `PERIODIC_RESYNC_FLOOR_MS = 3_600_000` / `PERIODIC_RESYNC_JITTER_MS = 600_000`). This is the safety net for anything RBSR misses (archive-window divergence, an old peer, a wedged round). Net: the common case becomes a cheap RBSR diff; the expensive full-dump drops ~12× in frequency and only fires as a floor.

## 6. Determinism rules (bake into the design)

- Fingerprint from **per-event canonical bytes** (`event_hash` via `signing_bytes()`), never from whole-snapshot CBOR. `PersistedVotingLog.poll_restore` is a `HashMap` (`community_voting_persist.rs:70-111`) → `voting.cbor` is not byte-reproducible across runs; per-event hashing sidesteps this entirely, so **no `HashMap → BTreeMap` change is required**.
- RBSR ordering comes from **sorting by `canonical_key`**, never `log.events` arrival order or any map iteration.
- `event_hash` excludes the `sig` field (matches channel-log semantics). Ed25519 is deterministic, so a given (content, signer) yields one signature; two *distinct* signers of byte-identical content would collide on element hash — acceptable, same as the channel log, noted for the fingerprint's collision reasoning.

## 7. Invariants to preserve (learned-in-blood, from channel-log RBSR)

1. **Count-fold in the fingerprint** — never drop `leb128(count)` from `finalize`, or hash-cancellation forgery reopens.
2. **Full-partition messages** closing at `max_key`, validated at the trust boundary (`validate_message`) before running the state machine.
3. **`bodies.len() == have_keys.len()` on the responder** — never advertise a `Have` key you can't back with a body (silent gap otherwise). For voting this couples to the **drop-on-archive index seam** (§5.1).
4. **Seal reply + `Have` packets under one epoch key** (ZEB-920); open under `[current, previous]`.
5. **Distinct AAD per message kind** — the frame classifier depends on `VOTING_RBSR_AAD` being un-openable as a voting event packet.
6. **`Locality::Remote` on the GET** — excludes the self-reply; "0 replies" means "no remote responder."
7. **Multi-holder `saw_extra_reply` → fall back**, don't converge on one reply.
8. **The periodic full-dump backstop remains** — RBSR is an optimization layered over it, never a replacement.
9. **RBSR feeds the existing `apply_backfilled_event`** — no second trust path; `seen_coords` dedup + verify@hlc still gate every applied event.

## 8. Testing (TDD)

All tests behavior-first, `--features test-fixtures`, in the voting module test scaffolding. Each written to fail first.

1. **Convergence, O(diff) transfer** — two `VotingLog`s differing by *k* events reconcile to equality; assert the transferred `Have`-key count equals the true diff (not the log size), across a spread of overlap ratios.
2. **Identical logs → Skip in one round** — no `Have` frames, immediate convergence.
3. **Archive-window fallback is clean** — peer A archived a poll B still holds; the round does not converge, hits the cap, and returns `VectorFallback`; **no archived event is resurrected** (assert `seen_coords` gate holds and A's log is unchanged).
4. **Index ↔ `log.events` lockstep across archive** — after `archive_finalized_polls`, every reconcile-index key resolves to a present body and vice-versa (guards invariant #3).
5. **Multi-holder guard** — a second sealed reply in the frame set ⇒ `Failed` ⇒ fallback.
6. **No-responder** — 0 frames on round 0 ⇒ `VectorFallback` ⇒ the existing full-dump path still catches up (old-peer compatibility).
7. **AAD domain separation** — a voting event packet never opens as an RBSR message and vice-versa.
8. **Epoch rotation** — reply + `Have` packets sealed across a simulated epoch boundary still open (single-epoch sealing holds).
9. **Determinism** — two independently-built indices over the same event set produce identical fingerprints regardless of insertion order.
10. **Count-fold anti-forgery** — a synthetic hash-cancelling event still breaks the range match (port the channel-log proof).

## 9. Scope, sequencing, risk

**Files (new/modified):**
- `community_voting_log.rs` — reconcile index + `RangeReconcileSource` impl + the drop-on-archive seam.
- `community_voting_tier3.rs` — thin `voting_reconcile_key` wrapper (canonical_key already exists).
- a voting RBSR seal/open module (new small file or a section alongside the voting engine) — `VOTING_RBSR_AAD`, seal/open, cap.
- `community_voting_log_engine.rs` — the three engine halves + hook bundle; RBSR bodies feed `apply_backfilled_event`.
- `event_loop.rs` — voting `rbsr/**` queryable + RBSR-first requester driver; parametrize the reused scaffolding by topic prefix.
- `lib.rs` — backstop interval retune + wiring.

**Estimated size:** ~300–500 LOC of voting-specific code + adapter wiring + tests, comparable to ZEB-592/593. Most of the protocol is reused.

**Sequencing (for the plan):** (1) reconcile index + `RangeReconcileSource` + archive seam (pure, fully unit-testable); (2) seal/open + AAD; (3) engine halves against an in-memory pair (no transport); (4) transport queryable + requester driver + fallback wiring; (5) backstop retune. Each stage independently testable.

**Risks:**
- *Archive-window fallback frequency* — if fleet data later shows it's common, revisit Approach B. Mitigated: bounded by clock-skew + archival cadence; caught by the backstop; sparse volume today.
- *Reused-code parametrization* — extracting the topic prefix from channel-scaffolding must not alter channel-log behavior; guarded by the existing channel-log RBSR tests staying green.
- *Epoch rotation mid-fetch* — mitigated by single-epoch sealing (invariant #4) + test #8.

## 10. Success criteria

- Two in-sync peers exchange ~0 event bytes per voting reconcile (fingerprints only); a peer *k* events behind transfers ≈ *k* events, not the whole log.
- The full-dump still fires as a ≤1 h backstop and as the fallback for old peers / archive-window rounds.
- No change to archival/retention semantics; no data loss; every existing voting admission invariant intact.
- Channel-log RBSR tests remain green (no regression from any shared-code parametrization).
