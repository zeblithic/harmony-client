# ZEB-156 — Implementation plan

Spec: [`docs/specs/2026-05-19-zeb-156-root-pin-set-design.md`](../specs/2026-05-19-zeb-156-root-pin-set-design.md)
Branch: `zeb-156-root-pin-set`
Bundles: ZEB-160 (pin/unpin/burn serialization mutex)

Three sequential tasks. Each is small (~30-80 lines diff) so review iterations are quick.

```text
Task 1: event-loop keep-set cascade for Unpin (correctness fix)
      ↓
Task 2: event-loop keep-set cascade for Burn + cache.remove expunge
      ↓
Task 3: ZEB-160 — pin_serial_lock across all three Tauri commands
```

---

## Task 1 — Event-loop `Unpin` keep-set cascade

**Spec sections:** "API surface > Modified — event loop verb handlers" (the `Unpin` arm), "Test plan > Unit tests 1-3, 7"

**Files:**
- `src-tauri/src/event_loop.rs:1719-1728` — rewrite the `ContentVerbRequest::Unpin` arm to compute a keep set from remaining `pin_intent` and skip unpinning descendants in the keep set.
- `src-tauri/src/event_loop.rs` (inline test module if present, or a new `#[cfg(test)] mod pin_cascade_tests` block at the end of the file) — add unit tests 1-3 from the spec.
- `src-tauri/tests/content_index_integration.rs` — add integration test 7 (two-root sidecar fixture, unpin the folder, leaf stays pinned).

**Constraints:**
- DO NOT change the `Pin` verb handler (`event_loop.rs:1697-1718`). Per D4, the existing cascade is correct under root-set semantics.
- DO NOT touch the `Burn` verb in this task — Task 2 handles it.
- DO NOT touch the Tauri command layer (`lib.rs`) — Task 3 handles ZEB-160.
- Use `std::collections::HashSet<ContentId>` for the keep set. `ContentId` already implements `Hash` and `Eq` per `harmony_content::cid` (verify; otherwise hash by `[u8; 32]`).
- Capacity-hint the keep set with `doomed.len()` — typical case is mostly disjoint, so the keep set fits in a small constant factor of the doomed set's size.

**Test gates:**
- `cd src-tauri; cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(pin_cascade) or test(unpin) or test(content_index_integration)'` — green.
- `cd src-tauri; cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` — zero warnings.
- `cd src-tauri; cargo fmt --all -- --check`.

**Commit message:** `fix(pin): event-loop Unpin honors remaining pin_intent keep set (ZEB-156)`

---

## Task 2 — Event-loop `Burn` keep-set cascade + cache.remove expunge

**Spec sections:** "API surface > Modified — event loop verb handlers" (the `Burn` arm), "Test plan > Unit tests 4-6"

**Files:**
- `src-tauri/src/event_loop.rs:1729-1742` — rewrite the `ContentVerbRequest::Burn` arm: apply the same keep-set logic as `Unpin` (Task 1), AND for every CID that gets unpinned, also call `runtime.storage_tier_mut().cache_mut().remove(&id)` (verify the exact runtime API for getting a `&mut ContentStore<_>`; the spec assumes `storage_tier_mut().cache_mut()` but the runtime crate may expose this differently).
- `src-tauri/src/event_loop.rs` (inline test module) — add unit tests 4-6 from the spec.

**Constraints:**
- Burn's keep-set logic must match Unpin's (Task 1) — consider extracting a private helper `fn compute_keep_set(store: &ContentStore<_>, pin_intent: &HashSet<[u8; 32]>) -> HashSet<ContentId>` in `event_loop.rs` to avoid duplicating the keep-set computation across the two arms. Implementer to decide: extract if it improves readability, inline if it's clearer that way.
- The cache eviction MUST only happen for CIDs we just unpinned, NOT for CIDs in the keep set (those are still needed).
- `cache.remove()` returning `None` (CID not in cache) is fine — `let _ = ...` to discard.

**Test gates:** Task 1's plus:
- The new unit tests 4-6 pass.
- No regression in existing `chunked_ingest_pin_cascade_fetch_burn_roundtrip` integration test (that test exercises the burn path end-to-end; the keep-set fix should not change its behavior because that test has only one pinned root).

**Commit message:** `fix(pin): event-loop Burn honors keep set + evicts via cache.remove (ZEB-156)`

---

## Task 3 — ZEB-160: `pin_serial_lock` for Tauri commands

**Spec sections:** "API surface > New — NodeState::pin_serial_lock", "Test plan > Tests 8-9"

**Files:**
- `src-tauri/src/lib.rs` (`NodeState` struct definition) — add `pin_serial_lock: Arc<tokio::sync::Mutex<()>>` field. Initialize to `Arc::new(tokio::sync::Mutex::new(()))` in `Default::default()` (or wherever `NodeState` is constructed).
- `src-tauri/src/lib.rs:5673-` (`pin_content`) — acquire `pin_serial_lock` at the top of the function body (after `parse_sidecar_id`). Hold across the existing sidecar lock + `verb_tx.send().await` + reply wait. Drop naturally at function exit.
- `src-tauri/src/lib.rs:5766-` (`unpin_content`) — same restructure.
- `src-tauri/src/lib.rs:5840-` (`burn_content`) — same restructure.
- `src-tauri/tests/content_index_integration.rs` — add integration test 8 (rapid pin/unpin toggling 100×; assert sidecar and runtime cache agree).

**Constraints:**
- `pin_serial_lock` is `tokio::sync::Mutex`, NOT `std::sync::Mutex`, because the critical section spans an `.await` (the `verb_tx.send().await` and the reply `oneshot.await`). Holding a `std::sync::Mutex` across an `.await` is a clippy warning and a correctness risk (blocking the executor thread).
- The existing `state.lock()` (the `std::sync::Mutex<NodeState>`) acquisitions inside the Tauri commands remain — those are sync mutexes acquired transiently to read fields out of `NodeState`. The new lock is layered ON TOP of those.
- For pin: the lock spans (read state → mutate sidecar → dispatch Pin → await reply). For unpin: (read state → mutate sidecar + OR-join → dispatch Unpin if applicable → await reply). For burn: (read state → mutate sidecar + three-branch decision → dispatch verb if applicable → await reply).
- DO NOT introduce per-CID locks. A single coarse lock for all three IPCs is the design per the spec.

**Test gates:** Task 2's plus:
- Integration test 8 passes: 100 rapid alternating pin/unpin calls, final state consistent.
- `cargo clippy` doesn't complain about lock-across-await on the new lock.

**Commit message:** `fix(pin): serialize pin/unpin/burn IPCs to prevent sidecar/runtime drift (ZEB-160)`

---

## Post-task gates (before opening PR)

```powershell
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo check --locked --all-targets --features test-fixtures
cargo nextest run --locked --workspace --all-targets --features test-fixtures  # CI runs this; locally hits Windows DLL quirk
cd ..
npx tsc --noEmit
npx vitest run
```

All green except Windows-local nextest run (DLL quirk per CLAUDE.md — Linux CI authoritative).

## Out-of-scope follow-ups (mention in PR body)

- **Effective-set caching** (per spec D3): recompute is fine for v1; cache if profiling shows it dominates.
- **Per-CID lock granularity**: single coarse lock is sufficient; per-CID locks only matter at unrealistic concurrent-toggle scale.
- **ZEB-157 (partial-ingest rollback)**: orthogonal; ZEB-156's keep-set semantics actually make ZEB-157 easier.
