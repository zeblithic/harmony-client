# ZEB-159 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire `fetch_rx`'s spawned task to fire-and-forget-admit each fetched CID's bytes to the local StorageTier cache via `CasOp::PutLocal { reply: None }`, so the ZEB-155 fetch-completion replay hook's `collect_descendants` walk sees the full bundle tree.

**Architecture:** Extract `wrap_fetch_one_with_admission` helper near `fetch_recursive` in `event_loop.rs`. The helper takes a per-CID `fetch_one` closure plus a clone of `cas_op_tx` and returns a new closure that calls `fetch_one`, then on `Ok(bytes)` fires `cas_op_tx.try_send(CasOp::PutLocal { cid, blob: bytes.clone(), reply: None })`. Wire the wrapper into the fetch_rx arm. Update doc comments on fetch_completion_rx + the existing ZEB-155 integration test.

**Tech Stack:** Rust 1.x stable, Tokio mpsc, harmony-content types.

**Spec:** `docs/specs/2026-05-15-zeb-159-fetch-rx-cache-admission-design.md` (commit `cf90d86`)

**Branch:** `zeb-159-fetch-rx-cache-admission` (already cut from `2505a47` on `origin/main`)

---

## Task 0: Pre-flight + green-baseline confirm

**No commit.** Verifies the spec-only commit didn't regress any gate.

- [ ] **Step 0.1: Confirm clean tree on the right branch.**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git status -s   # expect empty
git rev-parse --abbrev-ref HEAD   # expect: zeb-159-fetch-rx-cache-admission
git log --oneline -2
# Expect:
#   cf90d86 docs(zeb-159): spec for fetch_rx cache admission
#   2505a47 ZEB-221: tighten start_node generation race window (#124)
```

- [ ] **Step 0.2: Run all 5 gates green.**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd ..
npx tsc --noEmit
npx vitest run
```

All 5 gates must pass. If any fails, STOP and report — likely unrelated drift to file as a follow-up per the `feedback_unrelated_test_failures` memory rule.

---

## Task 1: Add `wrap_fetch_one_with_admission` helper + TDD tests

**Files:**
- Modify: `src-tauri/src/event_loop.rs`

This task introduces the wrapper helper with the per-CID admission side effect. TDD-shaped: tests first, then implementation.

### Step 1.1: Write the failing test for per-CID admission

- [ ] **Locate the existing `mod fetch_recursive_tests`** at `src-tauri/src/event_loop.rs:2622`. Add a sibling `mod fetch_one_wrapper_tests` immediately after the closing `}` of `fetch_recursive_tests` at line ~2696.

- [ ] **Add the first test:**

```rust
#[cfg(test)]
mod fetch_one_wrapper_tests {
    use super::{fetch_recursive, wrap_fetch_one_with_admission};
    use crate::content_store::CasOp;
    use harmony_content::bundle::BundleBuilder;
    use harmony_content::cid::{ContentFlags, ContentId};
    use std::collections::HashMap;
    use tokio::sync::mpsc;

    /// Drain the cas_op receiver into a Vec of (ContentId, Vec<u8>) for
    /// assertions. Each iteration matches `CasOp::PutLocal { reply: None }`
    /// — `GetOrFetch` should never appear in these test scenarios.
    fn drain_admits(rx: &mut mpsc::Receiver<CasOp>) -> Vec<(ContentId, Vec<u8>)> {
        let mut out = Vec::new();
        while let Ok(op) = rx.try_recv() {
            match op {
                CasOp::PutLocal { cid, blob, reply } => {
                    assert!(reply.is_none(), "wrapper must use fire-and-forget reply: None");
                    out.push((cid, blob));
                }
                CasOp::GetOrFetch { .. } => {
                    panic!("wrapper must not send GetOrFetch");
                }
            }
        }
        out
    }

    #[tokio::test]
    async fn admits_each_fetched_cid_for_a_bundle_tree() {
        // Bundle tree: root → [a, b, c]
        let a_bytes = b"aaa".to_vec();
        let b_bytes = b"bbbb".to_vec();
        let c_bytes = b"ccccc".to_vec();
        let a = ContentId::for_book(&a_bytes, ContentFlags::default()).unwrap();
        let b = ContentId::for_book(&b_bytes, ContentFlags::default()).unwrap();
        let c = ContentId::for_book(&c_bytes, ContentFlags::default()).unwrap();

        let mut builder = BundleBuilder::new();
        builder.add(a).add(b).add(c);
        let (payload, root) = builder.build_with_flags(ContentFlags::default()).unwrap();

        let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
        store.insert(a, a_bytes.clone());
        store.insert(b, b_bytes.clone());
        store.insert(c, c_bytes.clone());
        store.insert(root, payload.clone());

        let fetcher = move |cid: ContentId| {
            let bytes = store.get(&cid).cloned();
            std::future::ready(bytes.ok_or_else(|| format!("missing cid: {cid:?}")))
        };

        let (cas_op_tx, mut cas_op_rx) = mpsc::channel::<CasOp>(16);
        let wrapped = wrap_fetch_one_with_admission(fetcher, cas_op_tx);

        // Drive through fetch_recursive — every per-CID call goes through
        // the wrapper, so every successful fetch must produce a PutLocal.
        let got = fetch_recursive(wrapped, root).await.unwrap();

        // fetch_recursive's output is the concatenated leaves (existing
        // contract; we don't break it).
        let mut expected_concat = a_bytes.clone();
        expected_concat.extend_from_slice(&b_bytes);
        expected_concat.extend_from_slice(&c_bytes);
        assert_eq!(got, expected_concat);

        // Admission: every CID encountered (root bundle + 3 leaves).
        let admits = drain_admits(&mut cas_op_rx);
        assert_eq!(admits.len(), 4, "expected 4 admissions, got {:?}", admits);

        // Each admission carries the correct bytes for its CID.
        let admit_map: HashMap<ContentId, Vec<u8>> = admits.into_iter().collect();
        assert_eq!(admit_map.get(&root), Some(&payload));
        assert_eq!(admit_map.get(&a), Some(&a_bytes));
        assert_eq!(admit_map.get(&b), Some(&b_bytes));
        assert_eq!(admit_map.get(&c), Some(&c_bytes));
    }
}
```

- [ ] **Run the test — confirm it fails to compile** (because `wrap_fetch_one_with_admission` doesn't exist yet):

```bash
cd src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(admits_each_fetched_cid_for_a_bundle_tree)' 2>&1 | tail -20
```

Expected: compilation error `cannot find function 'wrap_fetch_one_with_admission' in this scope`.

### Step 1.2: Implement `wrap_fetch_one_with_admission`

- [ ] **Add the helper** immediately after `fetch_recursive`'s closing `}` at `src-tauri/src/event_loop.rs:2548`:

```rust
/// ZEB-159: wraps a per-CID fetch closure so each successful fetch
/// fire-and-forget-admits the bytes to the local StorageTier cache via
/// `cas_op_tx`. Mirrors the GetOrFetch admit-hop pattern at
/// `event_loop.rs:1625` so fetched bundle trees populate the cache
/// before `fetch_completion_rx`'s pin cascade walks them.
///
/// Admission is fire-and-forget: cache rejection (W-TinyLFU policy)
/// or channel saturation does NOT fail the fetch — the caller still
/// gets the bytes; only the per-CID cache population is best-effort.
/// On `fetch_one` failure (Err), no admission is sent for that CID.
pub(crate) fn wrap_fetch_one_with_admission<F, Fut>(
    fetch_one: F,
    cas_op_tx: tokio::sync::mpsc::Sender<crate::content_store::CasOp>,
) -> impl Fn(
    ContentId,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>,
>
       + Clone
       + Send
       + 'static
where
    F: Fn(ContentId) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>> + Send + 'static,
{
    move |cid: ContentId| {
        let inner = fetch_one.clone();
        let cas_op_tx = cas_op_tx.clone();
        Box::pin(async move {
            let bytes = inner(cid).await?;
            // Fire-and-forget. `bytes.clone()` is load-bearing:
            // `CasOp::PutLocal.blob` consumes the bytes, but the caller
            // (and `fetch_recursive`'s bundle parser) needs them too.
            // `reply: None` signals fire-and-forget intent — the
            // PutLocal handler skips its reply.send when reply is None.
            let _ = cas_op_tx.try_send(crate::content_store::CasOp::PutLocal {
                cid,
                blob: bytes.clone(),
                reply: None,
            });
            Ok(bytes)
        })
    }
}
```

- [ ] **Re-run the first test — expect PASS:**

```bash
cd src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(admits_each_fetched_cid_for_a_bundle_tree)' 2>&1 | tail -10
```

Expected: `PASS` for `admits_each_fetched_cid_for_a_bundle_tree`.

### Step 1.3: Add the failure-path tests

- [ ] **Append two more tests** to `mod fetch_one_wrapper_tests`:

```rust
    #[tokio::test]
    async fn skips_admit_on_fetch_failure() {
        // fetch_one returns Err for the requested CID. Verify no
        // CasOp::PutLocal was sent.
        let cid = ContentId::for_book(b"missing", ContentFlags::default()).unwrap();
        let fetcher = |_cid: ContentId| {
            std::future::ready(Err::<Vec<u8>, String>("synthetic fetch failure".to_string()))
        };

        let (cas_op_tx, mut cas_op_rx) = mpsc::channel::<CasOp>(4);
        let wrapped = wrap_fetch_one_with_admission(fetcher, cas_op_tx);

        let result = wrapped(cid).await;
        assert!(result.is_err(), "expected Err propagation; got {:?}", result);
        assert!(result.unwrap_err().contains("synthetic fetch failure"));

        // No admission should have been sent.
        let admits = drain_admits(&mut cas_op_rx);
        assert!(
            admits.is_empty(),
            "wrapper must not admit on fetch failure; got {:?}",
            admits
        );
    }

    #[tokio::test]
    async fn admit_failure_does_not_fail_fetch() {
        // cas_op channel is closed (receiver dropped). The wrapper's
        // try_send returns Err but the wrapper must NOT propagate that
        // — the caller still gets the fetched bytes.
        let bytes = b"payload".to_vec();
        let cid = ContentId::for_book(&bytes, ContentFlags::default()).unwrap();
        let bytes_for_fetcher = bytes.clone();
        let fetcher = move |_cid: ContentId| {
            let b = bytes_for_fetcher.clone();
            std::future::ready(Ok::<Vec<u8>, String>(b))
        };

        let (cas_op_tx, cas_op_rx) = mpsc::channel::<CasOp>(1);
        drop(cas_op_rx); // close the receiver — every try_send will Err.
        let wrapped = wrap_fetch_one_with_admission(fetcher, cas_op_tx);

        let result = wrapped(cid).await;
        assert!(
            result.is_ok(),
            "admission failure must not propagate to fetch caller; got {:?}",
            result
        );
        assert_eq!(result.unwrap(), bytes);
    }
```

- [ ] **Run all three new tests — expect PASS:**

```bash
cd src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(fetch_one_wrapper_tests::)' 2>&1 | tail -15
```

Expected: all 3 tests pass (`admits_each_fetched_cid_for_a_bundle_tree`, `skips_admit_on_fetch_failure`, `admit_failure_does_not_fail_fetch`).

### Step 1.4: Run the full clippy + fmt gates locally before committing

- [ ] **Run clippy + fmt:**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
```

Both must pass. If clippy complains about the helper's signature (e.g. needless `'static` bound, redundant clone), address inline.

### Step 1.5: Commit

- [ ] **Commit Task 1:**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/event_loop.rs
git commit -m "$(cat <<'EOF'
feat(zeb-159): add wrap_fetch_one_with_admission helper

Wraps a per-CID fetch closure to fire-and-forget-admit fetched bytes
to the local StorageTier cache via cas_op_tx.try_send(CasOp::PutLocal
{ reply: None }). Mirrors the GetOrFetch admit-hop pattern at
event_loop.rs:1625. Three unit tests cover per-CID admission, no
admission on fetch failure, and silent admission-failure behavior.

The wrapper is not yet wired into fetch_rx — that comes in the next
commit (Task 2). This commit establishes the testable seam.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Wire the wrapper into fetch_rx + update doc comments

**Files:**
- Modify: `src-tauri/src/event_loop.rs:1448-1493` (fetch_rx arm)
- Modify: `src-tauri/src/event_loop.rs:1742-1749` (fetch_completion_rx arm doc comment)
- Modify: `src-tauri/tests/content_index_integration.rs:651-657` (ZEB-155 test doc comment)

### Step 2.1: Wire the wrapper into the fetch_rx arm

- [ ] **Locate the fetch_rx arm** at `src-tauri/src/event_loop.rs:1448`. The current spawned-task body builds `fetch_one` inline (lines 1469-1477) and calls `fetch_recursive(fetch_one, root)` at line 1479.

- [ ] **Modify the arm to wrap `fetch_one` with admission before passing it to fetch_recursive.** Replace lines 1448-1493 with:

```rust
            Some(req) = fetch_rx.recv() => {
                let session = session.clone();
                let cid_hex = req.cid_hex;
                // ZEB-155: clone the completion sender so the spawned
                // task can notify the main loop after a successful fetch.
                let completion_tx = fetch_completion_tx.clone();
                // ZEB-159: clone cas_op_tx so the wrapped fetch_one can
                // fire-and-forget-admit each fetched CID's bytes to the
                // StorageTier cache. Without this, the ZEB-155 fetch-
                // completion arm walks an empty cache for freshly-
                // fetched roots and pin_content is a no-op.
                let cas_op_tx_for_fetch = cas_op_tx.clone();
                tokio::spawn(async move {
                    // Parse hex → 32-byte CID. Reply with an error if malformed.
                    let cid_bytes = match hex::decode(&cid_hex)
                        .ok()
                        .and_then(|b| <[u8; 32]>::try_from(b).ok())
                    {
                        Some(b) => b,
                        None => {
                            let _ = req.reply.send(Err(format!("invalid CID hex: {cid_hex}")));
                            return;
                        }
                    };
                    let root = ContentId::from_bytes(cid_bytes);

                    // Closure that does one Zenoh GET for a single CID.
                    let fetch_one = move |cid: ContentId| {
                        let session = session.clone();
                        async move {
                            let cid_hex = hex::encode(cid.to_bytes());
                            let prefix = cid_hex.get(1..2).unwrap_or("");
                            let key = format!("harmony/content/{prefix}/{cid_hex}");
                            fetch_via_zenoh(&session, &key).await
                        }
                    };
                    // ZEB-159: wrap fetch_one so each successful fetch
                    // also admits the bytes to the local cache. The
                    // wrapper fire-and-forget-sends CasOp::PutLocal
                    // { reply: None } per CID — mirrors the GetOrFetch
                    // admit-hop pattern at event_loop.rs:1625.
                    let fetch_one_with_admit =
                        wrap_fetch_one_with_admission(fetch_one, cas_op_tx_for_fetch);

                    let result = fetch_recursive(fetch_one_with_admit, root).await;
                    // ZEB-155: reply to the fetch caller FIRST so a full
                    // completion channel never delays the fetch reply.
                    // Then best-effort-notify via try_send. If the
                    // completion channel is full (rare — main loop drain
                    // is O(1) per select pass), we lose this chance to
                    // auto-repin; the next user action or next start_node
                    // reconverges. try_send also returns Err on closed,
                    // which is fine (event loop shutting down).
                    let is_ok = result.is_ok();
                    let _ = req.reply.send(result);
                    if is_ok {
                        let _ = completion_tx.try_send(cid_bytes);
                    }
                });
            }
```

- [ ] **Run the full nextest suite — expect no regressions:**

```bash
cd src-tauri
cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -10
```

Expected: all tests pass. Specifically, the ZEB-155 test `fetch_complete_arm_pins_root_in_intent` should still pass (its synthetic injection pattern bypasses the now-wired fetch_rx arm).

### Step 2.2: Update doc comment on fetch_completion_rx arm

- [ ] **Locate the doc comment** at `src-tauri/src/event_loop.rs:1742-1749`. The current comment reads:

```rust
            // ZEB-155: fetch-completion replay hook. When the fetch_rx
            // spawned task above completes a recursive fetch successfully,
            // it fires a completion signal on this channel. If the root
            // CID is in pin_intent (loaded at start_node from the sidecar),
            // walk all descendants currently in the cache and pin them.
            // This re-engages runtime-side eviction protection that was
            // lost when the previous node stopped and its in-memory
            // pinned-set went with it.
```

The current comment lacks the ZEB-159 caveat. Per `feedback_no_pushover_when_active` we don't pre-announce, but the existing doc may have stale "today's fetch_rx path does NOT admit" language elsewhere in or near this block. **Search the surrounding 30-line block** for any "does NOT admit" / "cascade walks an empty cache" / "no-op" / "ZEB-159" mentions and remove them.

- [ ] **Use Grep to find any such caveats:**

```bash
cd src-tauri
grep -n "does NOT admit\|empty cache\|no-op\|ZEB-159" src/event_loop.rs
```

- [ ] **For each match found inside the fetch_completion_rx arm's doc block (~line 1735-1758),** remove the stale caveat sentence(s) and replace with a single-line note that ZEB-159 closed the gap:

```rust
            // ZEB-155 + ZEB-159: fetch-completion replay hook. When the
            // fetch_rx spawned task above completes a recursive fetch
            // successfully, it fires a completion signal on this channel.
            // The spawned task admits every fetched CID's bytes via a
            // CasOp::PutLocal { reply: None } fire-and-forget hop
            // (ZEB-159), so by the time this arm runs, the bundle tree
            // is in the cache. If the root CID is in pin_intent (loaded
            // at start_node from the sidecar), walk all descendants
            // currently in the cache and pin them. This re-engages
            // runtime-side eviction protection that was lost when the
            // previous node stopped and its in-memory pinned-set went
            // with it.
```

(Adjust the diff above to match what's actually in the file — the canonical Edit operation should match the current text exactly.)

### Step 2.3: Update doc comment on the ZEB-155 integration test

- [ ] **Locate** `tests/content_index_integration.rs:653-657`:

```rust
/// ZEB-155: when the fetch-completion arm receives a root CID that's in
/// pin_intent, the cascade pins the root (and any descendants) in the
/// runtime cache. Injected via a test-owned fetch_completion_tx clone so
/// we don't need a real peer to answer a fetch_rx request.
```

- [ ] **Append a ZEB-159 note:**

```rust
/// ZEB-155 + ZEB-159: when the fetch-completion arm receives a root CID
/// that's in pin_intent, the cascade pins the root (and any descendants)
/// in the runtime cache. Injected via a test-owned fetch_completion_tx
/// clone so we don't need a real peer to answer a fetch_rx request.
///
/// ZEB-159 made the real fetch_rx → cache-admission → completion path
/// work end-to-end (the spawned fetch task now admits each fetched CID
/// via CasOp::PutLocal { reply: None } before signaling completion).
/// This test continues to exercise the cascade arm directly by injecting
/// completion synthetically — the synthetic path remains valuable as a
/// unit-style assertion that does not require a live Zenoh peer.
```

### Step 2.4: Run all 5 gates locally

- [ ] **Five-gate sweep:**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd ..
npx tsc --noEmit
npx vitest run
```

All 5 must pass.

### Step 2.5: Commit

- [ ] **Commit Task 2:**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/event_loop.rs src-tauri/tests/content_index_integration.rs
git commit -m "$(cat <<'EOF'
feat(zeb-159): wire fetch_rx to admit fetched bytes to the cache

The spawned fetch task in event_loop.rs's fetch_rx arm now wraps its
per-CID fetch_one closure with wrap_fetch_one_with_admission, so each
fetched bundle node + leaf is fire-and-forget-admitted to the local
StorageTier cache via CasOp::PutLocal { reply: None } before the
ZEB-155 fetch_completion_rx hook signals.

Closes the production-side gap surfaced from the ZEB-155 cross-cutting
review: collect_descendants(cache, root) now sees the full bundle tree
for a freshly-fetched root, so the pin cascade actually re-engages
runtime-side eviction protection after re-fetch.

Doc comments on fetch_completion_rx and the ZEB-155 integration test
updated to reflect the closed gap.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Final 5-gate sweep + push + PR

- [ ] **Step 3.1: Re-verify all 5 gates from a clean state.**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd ..
npx tsc --noEmit
npx vitest run
```

- [ ] **Step 3.2: Push the branch.**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git push -u origin zeb-159-fetch-rx-cache-admission
```

- [ ] **Step 3.3: Create the PR.** Use markdown-linked refs per `feedback_linear_pr_auto_close` — only ZEB-159 auto-closes; cross-refs to ZEB-155 / ZEB-154 are read-only.

```bash
gh pr create --title "ZEB-159: fetch_rx admits fetched bytes into the storage cache" --body "$(cat <<'EOF'
## Summary

Closes [ZEB-159](https://linear.app/zeblith/issue/ZEB-159).

Wraps the per-CID `fetch_one` closure in `fetch_rx`'s spawned task with `wrap_fetch_one_with_admission`, so each fetched bundle node + leaf is fire-and-forget-admitted to the local `StorageTier` cache via `CasOp::PutLocal { reply: None }` before the [ZEB-155](https://linear.app/zeblith/issue/ZEB-155) fetch-completion replay hook signals.

The production-side gap surfaced from the [ZEB-155](https://linear.app/zeblith/issue/ZEB-155) cross-cutting review: `collect_descendants(runtime.storage_tier().cache(), root)` previously walked an empty cache for freshly-fetched roots (the spawned task returned bytes to the caller without admitting them), so `runtime.pin_content` was a no-op and runtime-side W-TinyLFU eviction protection never re-engaged after re-fetch. Pin badges survived restart (display-join via sidecar) but the underlying cache state didn't match user expectation.

### Design

Reuses the established `GetOrFetch` admit-hop pattern at `event_loop.rs:1625` (existing `CasOp::PutLocal { reply: None }` fire-and-forget admission from a spawned task back to the event-loop thread). The wrapper sits at the seam between `fetch_one` and `fetch_recursive`: every successful per-CID fetch admits its bytes as a side effect; failed fetches do NOT admit; admission failure (cache rejection / channel saturation) is silent and does NOT propagate to the caller.

Spec: [docs/specs/2026-05-15-zeb-159-fetch-rx-cache-admission-design.md](https://github.com/zeblithic/harmony-client/blob/zeb-159-fetch-rx-cache-admission/docs/specs/2026-05-15-zeb-159-fetch-rx-cache-admission-design.md) (commit \`cf90d86\`).
Plan: [docs/plans/2026-05-15-zeb-159-fetch-rx-cache-admission-plan.md](https://github.com/zeblithic/harmony-client/blob/zeb-159-fetch-rx-cache-admission/docs/plans/2026-05-15-zeb-159-fetch-rx-cache-admission-plan.md).

### Changes

- `src-tauri/src/event_loop.rs`: add `wrap_fetch_one_with_admission` helper near `fetch_recursive`; wire it into the `fetch_rx` arm; update doc comment on the `fetch_completion_rx` arm to reflect the closed gap.
- `src-tauri/tests/content_index_integration.rs`: update doc comment on the existing ZEB-155 test to note that ZEB-159 closed the cache-admission gap (the synthetic-injection test pattern is preserved as it remains useful for unit-style cascade-arm assertions without a live Zenoh peer).

### Test plan

- [x] 3 new unit tests in `mod fetch_one_wrapper_tests`:
  - `admits_each_fetched_cid_for_a_bundle_tree` (4 admissions for root + 3 leaves; bytes match per CID)
  - `skips_admit_on_fetch_failure` (no admission when `fetch_one` returns Err)
  - `admit_failure_does_not_fail_fetch` (closed cas_op channel doesn't propagate to caller)
- [x] `cargo fmt --all -- --check` clean
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` clean
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` — full suite green
- [x] `npx tsc --noEmit` clean (no frontend changes)
- [x] `npx vitest run` clean (no frontend changes)

### Out of scope

- Changes to `fetch_recursive`'s walk algorithm (still pure DFS).
- Two-node integration test for real `fetch_via_zenoh` round-trip (separate ticket if needed).
- Proactive refetch on startup for previously-pinned CIDs (separate architecture question; belongs with the disk-backed storage tier work).
- Disk-tier admission.

## Related

- [ZEB-155](https://linear.app/zeblith/issue/ZEB-155) — introduces the replay hook this PR makes effective.
- [ZEB-154](https://linear.app/zeblith/issue/ZEB-154) — `fetch_recursive` + `collect_descendants` whose interaction surfaced this gap.
- [ZEB-146](https://linear.app/zeblith/issue/ZEB-146) — file manager backend; the "real archive tier" follow-up also intersects with cache admission concerns.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3.4: Confirm PR is open.** Capture the PR number for the autonomous bot-review monitoring loop.

```bash
gh pr view --json url,number,mergeable,mergeStateStatus --jq '{url, number, mergeable, mergeStateStatus}'
```

Expected: PR open, mergeable + CLEAN (CI is disabled per the repo's memory rule).

---

## Self-review checklist (run inline before declaring plan complete)

- [x] Every task except Task 0 ends with a commit.
- [x] All file paths are absolute or repo-relative; line ranges are explicit.
- [x] TDD ordering: test first → run failing → implement → run passing → commit.
- [x] Test bodies are inline, not "similar to" or "TODO".
- [x] Doc-comment edits match exact existing text (will need Edit tool to operate on file as it exists at edit time).
- [x] Spec compliance: every spec section §1-§7 has a corresponding task step.
- [x] No invented Linear IDs in the PR body; only ZEB-159 / ZEB-155 / ZEB-154 / ZEB-146 (all verified to exist).
- [x] No unrelated changes folded in (no `Cargo.toml` bumps, no clippy auto-fixes outside touched files).
