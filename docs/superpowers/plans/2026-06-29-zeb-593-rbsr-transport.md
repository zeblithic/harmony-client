# ZEB-593 — Wire the live RBSR Zenoh transport (Part C) Implementation Plan

> **For agentic workers:** the RBSR *core* landed dormant in PR #365 (`channel_rbsr.rs`, `channel_chunk_index.rs`, engine `rbsr_respond`, `ChannelLog: RangeReconcileSource`, `ReconcileMode` helpers, AEAD seal/open, wire pins). This plan wires the *live* path and removes the `#[allow(dead_code)]` gates. On merge, closes ZEB-592.

**Goal:** Reconnect catch-up uses RBSR end-to-end over Zenoh (multi-round, requester-pull), negotiated by absence with a clean fallback to the Part A watermark-vector path.

**Architecture:** The Zenoh `session` lives in the adapter (`event_loop.rs`); the channel key + `RangeReconcileSource` live in the engine (`community_channel_log_engine.rs`). So: crypto + round logic are engine-side (closures), the network GET is adapter-side — the exact seam Part A's `read_for_query` established. A second `rbsr/**` queryable answers; a multi-round requester driver in the adapter calls two engine closures + one GET primitive.

**Tech stack:** Rust, Zenoh (`ConsolidationMode::None`, GET with explicit `.timeout()`), ChaCha20-Poly1305 (`seal_rbsr_message`/`open_rbsr_message`), tokio.

## Global Constraints (verbatim)

- CI gates: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. All run from `src-tauri/`.
- `-D warnings` means **every** `#[allow(dead_code)]` removal must land with its caller in the same change — no incrementally-green intermediate.
- RBSR is purely additive: remove **zero** existing wire paths (`since/**`, watermark vector, periodic floor, on-disk `SegmentDescriptor`).
- Have events MUST ingest via the existing `process_inbound_packet` (same signature/replay/epoch verification + flush as live gossip).
- Reuse `MAX_RBSR_MESSAGE_BYTES` (64 KiB) cap; cap-before-alloc on the responder, on the bytes view before decrypt.
- Branch `channel-log-rbsr-transport`; no `ZEB-NNN` in branch/commit titles; PR body closes ZEB-593 **and** ZEB-592.

---

## Current-state seam map (post-#365, verified)

- `spawn_channel_log_zenoh_adapter` — `event_loop.rs:7679`; `since/**` queryable prefix built at `:7710`; handler invokes `read_for_query_qbl` at `:7912`; `since=None` forces `watermark_sealed=None` at `:7908`.
- `ChannelLogAdapterRequest` — `event_loop.rs:187`; constructed `community_channel_log_engine.rs:2457`.
- Query-request GET driver — `event_loop.rs:7942`; key built `:7964`; `session.get` `:7986`.
- `run_backfill_driver` — `channel_backfill.rs:457`; `BackfillAction::Request` arm `:558`. `ReconcileMode` `:61`; `reconcile_mode_after_round0` `:75`; `reconcile_mode_after_round` `:85`.
- `process_inbound_packet` — `community_channel_log_engine.rs:1487`.
- Dormant: `rbsr_respond` `community_channel_log_engine.rs:1762`; `events_for_keys` `community_channel_log.rs:2280`.
- Pure protocol: `initial_request` `channel_rbsr.rs:323`; `respond` `:340`; `process_reply` `:401`.
- Crypto: `seal_rbsr_message` `community_channel_log.rs:945`; `open_rbsr_message` `:956`; `encrypt_channel_packet` `:679`.
- Key parse: `parse_channel_backfill_key` `event_loop.rs:8363`; `format_hlc_hex` `:8403`.
- Registry closures: `read_for_query` `engine.rs:2326`; request send `:2457`.

---

## Task 1: `parse_rbsr_key` + key construction (pure, TDD)

**Files:** Modify `event_loop.rs` (near `parse_channel_backfill_key:8363`); test inline `#[cfg(test)]`.

**Interfaces — Produces:** `fn parse_rbsr_key(key: &str) -> Option<u32>` (returns `{round}`; `None` on non-rbsr / malformed; cid/ch scoping is left to Zenoh routing, mirroring `parse_channel_backfill_key`); `fn format_rbsr_key(cid_hex, ch_hex, round: u32) -> String` → `harmony/channels/{cid}/{ch}/rbsr/{round}`.

- [ ] Write failing tests: round-trip `format`→`parse` for rounds `0`, `5`, `MAX_RBSR_ROUNDS`; reject a `since/**` key; reject non-numeric round; reject trailing junk (exactly 6 segments).
- [ ] Implement both helpers mirroring `parse_channel_backfill_key`'s selector split.
- [ ] `cargo nextest run -p harmony-app --lib -E 'test(rbsr_key)'` → PASS.
- [ ] Commit.

## Task 2: Requester round-driver state machine (pure-ish, TDD with fakes)

**Files:** New `fn drive_rbsr_rounds(...)` in `event_loop.rs` (adapter module) or a small helper module; tests with in-memory fakes.

**Interfaces — Consumes:** `rbsr_get: impl Fn(u32, Vec<u8>) -> Fut<Vec<Vec<u8>>>` (round, sealed_request → reply frames); `rbsr_initial: impl Fn() -> Fut<Vec<u8>>`; `rbsr_ingest_and_next: impl Fn(Vec<Vec<u8>>) -> Fut<RbsrStep>`. **Produces:** `enum RbsrStep { Converged { ingested: usize }, Continue { ingested: usize, next: Vec<u8> }, Failed }` (the success variants carry the round's ingested Have-packet count so the driver reports real progress, not the raw frame count) and `async fn drive_rbsr_rounds(...) -> (ReconcileMode, usize)` (the `usize` is the total transferred, for the backfill-progress tick).

Driver logic (uses committed `ReconcileMode` helpers):
```rust
let mut sealed = rbsr_initial().await;
for round in 0..MAX_RBSR_ROUNDS {
    let frames = rbsr_get(round, sealed).await;
    if round == 0 && matches!(reconcile_mode_after_round0(frames.len()), ReconcileMode::VectorFallback) {
        return ReconcileMode::VectorFallback;
    }
    match rbsr_ingest_and_next(frames).await {
        RbsrStep::Converged => return ReconcileMode::Done,
        RbsrStep::Failed => return ReconcileMode::VectorFallback,
        RbsrStep::Continue(next) => {
            if matches!(reconcile_mode_after_round(round, false), ReconcileMode::FullReconcile) {
                return ReconcileMode::FullReconcile;
            }
            sealed = next;
        }
    }
}
ReconcileMode::FullReconcile
```

- [ ] Write failing tests with fakes: (a) round-0 zero frames → `VectorFallback`; (b) converge in 2 rounds → `Done` and the fake `rbsr_get` saw exactly the expected sealed payloads; (c) never-converge → `FullReconcile` at the cap; (d) ingest failure → `VectorFallback`.
- [ ] Implement `drive_rbsr_rounds` + `RbsrStep`.
- [ ] `cargo nextest run -p harmony-app --lib -E 'test(drive_rbsr)'` → PASS.
- [ ] Commit.

## Task 3: Engine closures — responder + requester halves

**Files:** Modify `community_channel_log_engine.rs` (engine impl + registry `:2326`).

**Interfaces — Produces (built in the registry, capturing `Arc<ChannelLogEngine>`):**
- `rbsr_respond_query: Arc<dyn Fn(Vec<u8>) -> Pin<Box<Fut<Option<(Vec<u8>, Vec<Vec<u8>>)>>>>>` — calls `engine.rbsr_respond(&sealed)`; on `Some((sealed_reply, events))` encrypts each via `encrypt_channel_packet(key, ev)` → `Vec<Vec<u8>>`; returns `(sealed_reply, packets)`.
- `rbsr_initial: Arc<dyn Fn() -> Pin<Box<Fut<Vec<u8>>>>>` — locks log, `channel_rbsr::initial_request(&*log)`, `seal_rbsr_message(key, &msg)`.
- `rbsr_ingest_and_next: Arc<dyn Fn(Vec<Vec<u8>>) -> Pin<Box<Fut<RbsrStep>>>>` — frame[0] = `open_rbsr_message(key, &f0)` (err → `Failed`); for each frame[1..] call `self.process_inbound_packet(frame)`; then lock log, `channel_rbsr::process_reply(&reply, &*log)` → `(_, Option<next_msg>)`; `None` → `Converged`, `Some` → `seal_rbsr_message` → `Continue`.

- [ ] Add engine methods `async fn rbsr_build_initial(&self) -> Vec<u8>` and `async fn rbsr_ingest_and_next(self: &Arc<Self>, frames: Vec<Vec<u8>>) -> RbsrStep` (thin, reuse dormant pieces).
- [ ] Unit test `rbsr_ingest_and_next` over a real `ChannelLog`: a sealed reply carrying one Have packet ingests + a follow-up `process_reply` converges. (Reuses #365 test fixtures.)
- [ ] Build the three closures in the registry.
- [ ] `cargo nextest run -p harmony-app --lib -E 'test(rbsr_ingest)'` → PASS.
- [ ] Commit (compiles only after Task 4 threads them; commit together with Task 4 if needed for `-D warnings`).

## Task 4: Thread closures through the adapter + register `rbsr/**` queryable

**Files:** Modify `event_loop.rs` (`ChannelLogAdapterRequest:187`, `spawn_channel_log_zenoh_adapter:7679`, registry call `:5344`), `community_channel_log_engine.rs` (request send `:2457`).

- [ ] Add `rbsr_respond_query`, `rbsr_initial`, `rbsr_ingest_and_next` fields to `ChannelLogAdapterRequest` and the generic `spawn_channel_log_zenoh_adapter` params (mirror `read_for_query` trait-object/`Arc` shape; keep the `#[allow(clippy::type_complexity)]`).
- [ ] Register a second queryable on `format!("harmony/channels/{cid}/{ch}/rbsr/**")`. Handler: `parse_rbsr_key` (drop non-matching); cap-check `query.payload()` vs `MAX_RBSR_MESSAGE_BYTES` before `.to_bytes()`; `(rbsr_respond_query)(payload).await`; on `Some((sealed, packets))` `query.reply(key, sealed)` then one `query.reply` per packet; on `None` reply nothing.
- [ ] Wire the requester: in the adapter, build the `rbsr_get` closure (`session.get(format_rbsr_key(..)).payload(sealed).allowed_destination(Locality::Remote).consolidation(ConsolidationMode::None).timeout(Duration::from_secs(10))`, drain `recv_async` frames until stream close, enforcing a per-round buffer cap) and call `drive_rbsr_rounds(rbsr_get, rbsr_initial, rbsr_ingest_and_next)`. **`Locality::Remote` is load-bearing** — the requester also declares an `rbsr/**` queryable, so without it the GET draws the requester's own all-`Skip` self-reply and may converge prematurely.
- [ ] `cargo check -p harmony-app --lib` clean.
- [ ] Commit.

## Task 5: Wire `run_backfill_driver` RBSR-first + remove dead_code

**Files:** Modify `channel_backfill.rs:457` (driver), the adapter call site that constructs the driver, `community_channel_log_engine.rs:1762` + `community_channel_log.rs:2280` (drop `#[allow(dead_code)]`).

**Interfaces — Consumes:** an `rbsr_attempt: impl Fn() -> Fut<ReconcileMode>` preamble closure (wraps `drive_rbsr_rounds` with the live closures).

- [ ] Add `rbsr_attempt` to `run_backfill_driver`; at the start of a backfill cycle call it once: `Done` → cycle complete (no watermark GET); `VectorFallback` → existing watermark loop unchanged; `FullReconcile` → existing loop with first `since=None`.
- [ ] Remove `#[allow(dead_code)]` on `rbsr_respond` + `events_for_keys` (callers now exist).
- [ ] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` clean.
- [ ] Commit.

## Task 6: Two-engine Zenoh integration test

**Files:** Modify `tests/channel_backfill_integration.rs`.

- [ ] Test `rbsr_recovers_within_device_out_of_order_hole_over_zenoh`: two engines + real Zenoh session; open a within-one-device out-of-order hole on B; reconnect; assert B recovers the missing event(s) and the responder shipped **~O(gap)** Have packets (count), not B's whole history.
- [ ] Backward-compat assertion: an old-style requester (vector GET only, no `rbsr/**`) still gets today's behavior.
- [ ] `cargo nextest run --locked --workspace --all-targets --features test-fixtures -E 'test(channel_backfill)'` → PASS.
- [ ] Commit.

## Task 7: Final gates + PR

- [ ] `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; full `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- [ ] Push; open PR (body closes ZEB-593 **and** ZEB-592); trigger CodeRabbit.

## Self-review notes

- Type consistency: `RbsrStep` defined once (Task 2), consumed by Task 3's closure. `ReconcileMode` is the #365 enum — reuse, don't redefine.
- The requester driver never touches the channel key (opaque sealed bytes only); the responder closure never touches the session. Seam preserved.
- `events_for_keys` returns encounter order — fine; ingestion is per-event and order-independent.
