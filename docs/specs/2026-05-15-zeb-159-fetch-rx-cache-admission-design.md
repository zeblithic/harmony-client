# ZEB-159: fetch_rx admits fetched bytes into the storage cache

**Branch:** `zeb-159-fetch-rx-cache-admission`
**Linear:** [ZEB-159](https://linear.app/zeblith/issue/ZEB-159)
**Related:** [ZEB-155](https://linear.app/zeblith/issue/ZEB-155) (introduces the replay hook this ticket makes effective), [ZEB-154](https://linear.app/zeblith/issue/ZEB-154) (`fetch_recursive` algorithm)

## §1 Problem

`fetch_rx`'s spawned task at `src-tauri/src/event_loop.rs:1448-1493` calls `fetch_recursive(fetch_one, root)` and replies to the Tauri caller with the concatenated leaf bytes, but it **never admits the fetched bytes to the StorageTier cache**.

Consequence: the ZEB-155 fetch-completion replay hook at `event_loop.rs:1742-1758` calls `collect_descendants(runtime.storage_tier().cache(), root)` against an empty cache for the freshly-fetched root, so the cascade walks only the root CID. `runtime.pin_content(root)` then returns `false` because the CID was never admitted. The hook fires, does zero work, and returns.

End-user observation: pin badges survive restart (display-join works through the sidecar `pinned` flag), but the runtime-side W-TinyLFU eviction protection does NOT re-engage after re-fetch. Pinned content can be evicted under cache pressure even though the user expects pinning to prevent that.

The existing ZEB-155 integration test `fetch_complete_arm_pins_root_in_intent` (`tests/content_index_integration.rs:657`) passes only because it pre-ingests the bytes manually before injecting a synthetic completion signal through `fetch_completion_tx_for_test`. It never exercises real `fetch_recursive` output, so the gap was invisible at task-review time.

## §2 Architecture

**Wrap the per-CID `fetch_one` closure so each successful fetch fires a fire-and-forget `CasOp::PutLocal` admission hop** through the existing `cas_op_tx` channel:

```text
fetch_rx → spawn(fetch_recursive(wrap_with_admit(fetch_one), root))
                                              │
                                              ├─ for each fetched CID:
                                              │     cas_op_tx.try_send(PutLocal { cid, blob, reply: None })
                                              │
                                              └─ returns Vec<u8> concatenated leaves
                                                 ↓
                                     fetch_completion_tx.try_send(cid_bytes)
                                                 ↓
                                     fetch_completion_rx arm:
                                       collect_descendants(cache, root)
                                       runtime.pin_content(id) for each
```

This admits **every fetched CID** (bundle nodes AND leaves) to the cache during the recursive walk. By the time the completion signal reaches the `fetch_completion_rx` arm, the full bundle tree is in the cache and `collect_descendants` finds the full descendant set.

### Why this design

**Reuses the GetOrFetch admit-hop pattern.** The `CasOp::GetOrFetch` handler at `event_loop.rs:1583-1645` already uses exactly this pattern: spawn a Zenoh GET, on success fire-and-forget admit via `cas_op_tx.try_send(CasOp::PutLocal { cid, blob, reply: None })`, then reply to caller. The doc comments at `content_store.rs:74-79` explicitly call out the `reply: None` variant as "fire-and-forget admit hops from the spawned-fetch task." This is established precedent, not new ground.

**`CasOp::PutLocal` routes through the event-loop thread.** The spawned fetch task does not have direct access to `&mut NodeRuntime` (the runtime is single-owned by the event-loop thread). The `cas_op_tx` mpsc hop is the established mechanism for cross-thread admission; the receiving `CasOp::PutLocal` arm at `event_loop.rs:1552-1568` pushes a `RuntimeEvent::SubscriptionMessage` with key `harmony/content/publish/{cid_hex}` and ticks the runtime.

**Per-CID admission inside `fetch_recursive`'s walk.** `fetch_recursive` already calls `fetch_one(cid).await?` once per CID encountered (`event_loop.rs:2536`). Wrapping `fetch_one` so it admits as a side effect after each successful fetch is the minimal-touch change. `fetch_recursive`'s signature stays unchanged.

**Why not alternative: change `fetch_recursive`'s signature** to take a separate `admit_one: A` closure? Considered and rejected. Adds a third generic parameter, requires updating all call sites + existing unit tests in `fetch_recursive_tests` (`event_loop.rs:2622-2696`), and the wrapper-helper approach achieves the same semantic without API churn. The wrapper helper IS the seam — keep it local.

**Why not alternative: send through `ingest_tx`** (the existing single-blob ingest channel)? `ingest_tx` is single-shot — one blob per request with a `reply` oneshot. Sending N times for a bundle tree adds N oneshot allocations and serial round-trips. `cas_op_tx.try_send` with `reply: None` is fire-and-forget and matches the existing GetOrFetch precedent. (Both routes ultimately push the same `RuntimeEvent::SubscriptionMessage`; `cas_op_tx` is just the more idiomatic channel for SyncEngine-side admits.)

### Failure modes (documented, not blocked)

**Admission rejection (W-TinyLFU policy).** The cache may silently reject an admission under pressure — same as `ingest_rx`'s contract (`event_loop.rs:1521-1538`) and the GetOrFetch admit hop. If the cache rejects an admission for some CID, the fetch_completion arm's `collect_descendants` will return a partial walk and `pin_content` will pin only what's present. Strictly better than today (today: nothing admitted, nothing pinned). Documented; no special handling.

**`cas_op_tx` full or closed.** `try_send` returns `Err` on full/closed channel; the spawned task discards the error and continues fetching. Same failure shape as the GetOrFetch admit hop. Strictly better than today.

**`fetch_one` returns Err.** No admission happens for that CID (the wrapper only admits on `Ok`). `fetch_recursive` propagates the error up; admission for any already-fetched CIDs in the tree may have completed (partial), but those admitted CIDs are valid bytes — no corruption. The pin cascade will simply pin a partial set when the completion arm doesn't fire (since `fetch_recursive` returns `Err`, `is_ok` is false at `event_loop.rs:1488` and no completion signal is sent).

### Double-announce risk

Local `RuntimeEvent::SubscriptionMessage` pushes (from `ingest_rx`, `CasOp::PutLocal`, and now wrapped `fetch_one`) do NOT trigger frontend event emission. The frontend `emit` path is gated on Zenoh-sourced subscriptions only (`event_loop.rs:1401-1419` — `emit_frontend_event` is called BEFORE pushing the SubscriptionMessage, only for events that arrive from the Zenoh subscriber). So admitting fetched bytes does not re-broadcast them on the network. Safe.

## §3 Code seams

| File | Lines | Change |
|---|---|---|
| `src-tauri/src/event_loop.rs` | new helper near 2517 | Add `fn wrap_fetch_one_with_admission<F, Fut>(fetch_one: F, cas_op_tx: mpsc::Sender<CasOp>) -> impl Fn(ContentId) -> ... + 'static` |
| `src-tauri/src/event_loop.rs` | 1448-1493 | Capture `cas_op_tx.clone()` in fetch_rx; pass through wrapper in spawned task |
| `src-tauri/src/event_loop.rs` | 1742-1749 | Update doc comment on fetch_completion_rx arm — note ZEB-159 closes the cache-admission gap |
| `tests/content_index_integration.rs` | 651-657 | Update doc comment on `fetch_complete_arm_pins_root_in_intent` to note the synthetic-injection pattern is no longer load-bearing |

## §4 Wrapper helper shape

```rust
/// ZEB-159: wraps a per-CID fetch closure so each successful fetch
/// also fire-and-forget-admits the bytes to the local StorageTier
/// cache via `cas_op_tx`. Mirrors the GetOrFetch admit-hop pattern at
/// `event_loop.rs:1625` so fetched bundle trees populate the cache
/// before `fetch_completion_rx`'s pin cascade walks them.
///
/// Admission is fire-and-forget: cache rejection (W-TinyLFU policy)
/// or channel saturation does NOT fail the fetch — the caller still
/// gets the bytes; only the per-CID cache population is best-effort.
/// On `fetch_one` failure (Err), no admission is sent for that CID.
fn wrap_fetch_one_with_admission<F, Fut>(
    fetch_one: F,
    cas_op_tx: tokio::sync::mpsc::Sender<crate::content_store::CasOp>,
) -> impl Fn(ContentId) -> impl std::future::Future<Output = Result<Vec<u8>, String>>
where
    F: Fn(ContentId) -> Fut + Clone,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    move |cid: ContentId| {
        let inner = fetch_one.clone();
        let cas_op_tx = cas_op_tx.clone();
        async move {
            let bytes = inner(cid).await?;
            // Fire-and-forget. `bytes.clone()` is load-bearing —
            // PutLocal.blob consumes the bytes but the caller (and
            // fetch_recursive's bundle parser) needs them too.
            let _ = cas_op_tx.try_send(crate::content_store::CasOp::PutLocal {
                cid,
                blob: bytes.clone(),
                reply: None,
            });
            Ok(bytes)
        }
    }
}
```

Note: Rust's `impl Trait` return position for closures returning futures has stabilization quirks across MSRV. The actual implementation may use a small wrapper struct + manual `Fn` impl, or a `Box::pin(async move {...})` future. Decision deferred to implementer; either compiles cleanly under the project's stable toolchain. The behavioral contract is the same.

## §5 Tests

### Unit tests (new in `mod fetch_recursive_tests` or sibling `mod fetch_one_wrapper_tests`)

1. **`wrap_fetch_one_admits_each_fetched_cid`** — construct a bundle tree {root → (a, b, c)} with synthetic bytes. Build a HashMap-backed mock `fetch_one`. Wrap with admission against a real `tokio::sync::mpsc::channel::<CasOp>`. Run `fetch_recursive(wrapped, root)`. Drain the cas_op receiver. Assert exactly 4 `CasOp::PutLocal` events received (root + 3 leaves), each with the correct `cid` and `blob` matching the fetched bytes.

2. **`wrap_fetch_one_skips_admit_on_fetch_failure`** — wrap a `fetch_one` that returns `Err("synthetic")` for a specific CID. Call the wrapped closure for that CID. Assert the result is `Err` AND no `CasOp::PutLocal` was sent (drain_with_timeout on the cas_op_rx returns empty).

3. **`wrap_fetch_one_admit_failure_does_not_fail_fetch`** — wrap a `fetch_one` against a `cas_op_tx` whose receiver has been dropped (channel closed). Call the wrapped closure. Assert it still returns `Ok(bytes)` — admission failure is silent.

### Integration test (existing, doc-updated only)

The existing `fetch_complete_arm_pins_root_in_intent` (`tests/content_index_integration.rs:657`) keeps its synthetic-injection pattern. Update its doc comment: "ZEB-159 makes the real fetch_rx → cache-admission → completion path work end-to-end; this test continues to exercise the cascade arm directly by injecting completion synthetically."

### Out of scope for tests

- A two-node integration test that exercises real `fetch_via_zenoh` over a live Zenoh session is out of scope (filed separately if needed); the unit tests above cover the new admit-hop contract.

## §6 Out of scope

- Changes to `fetch_recursive`'s walk algorithm (still pure DFS).
- Changes to `fetch_one` signature in `fetch_recursive` (still `Fn(ContentId) -> Future`).
- Proactive refetch on startup for previously-pinned CIDs (separate architecture question; belongs with disk-backed storage tier work).
- Changes to the cache admission policy (W-TinyLFU silent-drop behavior is preserved).
- Disk-tier admission (cache → disk migration is StorageTier's concern, not fetch_rx's).

## §7 Acceptance

- `fetch_rx` admits every successfully-fetched CID (bundle nodes and leaves) to the local StorageTier cache via `CasOp::PutLocal { reply: None }`.
- The ZEB-155 fetch-completion replay hook's `collect_descendants` walk sees the full bundle tree for a freshly-fetched root that's in `pin_intent`.
- All 5 CI gates green: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `npx tsc --noEmit`, `npx vitest run`.
- Doc comment on `fetch_completion_rx` arm no longer references "today's fetch_rx path does NOT admit..."
