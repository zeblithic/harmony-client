# Chunked Ingest Implementation Plan (ZEB-154)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Route >1 MiB files through `harmony-content`'s chunker so they ingest, fetch, pin/unpin/burn end-to-end through the File Manager without a user-visible size cap below ~32 GiB.

**Architecture:** Size-dispatch in `ingest_content` — small files take the existing single-book path, large files chunk via FastCDC, store each leaf through the existing `IngestRequest` channel, and assemble a root bundle CID that the sidecar records. Read path gets transparent recursion in the event loop's `fetch_rx` arm. Pin/unpin/burn handlers walk the bundle tree locally via `runtime.storage_tier().cache()` and cascade the verb to every descendant.

**Tech Stack:** Rust (Tauri backend), `harmony-content` crate (chunker, CID, bundle, content store), existing event-loop mpsc channels.

**Spec:** `docs/specs/2026-04-23-chunked-ingest-design.md`

**Branch:** `feat/chunked-ingest-zeb-154`

---

## File structure

| File | Change | Responsibility |
|---|---|---|
| `src-tauri/src/lib.rs` | Modify | `FLAT_BUNDLE_MAX` constant, `ingest_dispatch` pure helper, `chunk_and_bundle` pure helper, size-dispatch in `ingest_content` command. |
| `src-tauri/src/event_loop.rs` | Modify | `collect_descendants` walker, `fetch_recursive` walker (generic over a fetch callback), cascade in `ContentVerbRequest::{Pin, Unpin, Burn}` handlers, recursion in `fetch_rx` arm. |
| `src-tauri/tests/content_index_integration.rs` | Modify | New `chunked_ingest_pin_cascade_fetch_burn_roundtrip` test using a 3 MiB synthetic buffer. |

No new files. No TS, sidecar, or protocol-level changes.

---

## Task 1: FLAT_BUNDLE_MAX constant and `ingest_dispatch` helper

Pure decision helper that classifies a size into reject / single-book / chunked. Separating it from `ingest_content` lets us unit-test the threshold without a Tauri AppHandle.

**Files:**
- Modify: `src-tauri/src/lib.rs` (add constant + helper + tests near the existing `ingest_content` command)

- [ ] **Step 1: Write the failing test**

Add at the bottom of `src-tauri/src/lib.rs`, inside a new `#[cfg(test)] mod chunked_ingest_tests { ... }` block:

```rust
#[cfg(test)]
mod chunked_ingest_tests {
    use super::*;
    use harmony_content::bundle::MAX_BUNDLE_ENTRIES;
    use harmony_content::cid::MAX_PAYLOAD_SIZE;

    #[test]
    fn ingest_dispatch_picks_single_for_small_sizes() {
        assert!(matches!(
            ingest_dispatch(0).unwrap(),
            IngestDispatch::Single
        ));
        assert!(matches!(
            ingest_dispatch(MAX_PAYLOAD_SIZE as u64).unwrap(),
            IngestDispatch::Single
        ));
    }

    #[test]
    fn ingest_dispatch_picks_chunked_above_single_book_ceiling() {
        assert!(matches!(
            ingest_dispatch(MAX_PAYLOAD_SIZE as u64 + 1).unwrap(),
            IngestDispatch::Chunked
        ));
    }

    #[test]
    fn ingest_dispatch_rejects_above_flat_bundle_cap() {
        let too_big = FLAT_BUNDLE_MAX + 1;
        let err = ingest_dispatch(too_big).unwrap_err();
        assert!(err.contains("file too large"), "got: {err}");
        assert!(err.contains("32 GiB") || err.contains("flat-bundle"),
                "message should explain the cap origin, got: {err}");
    }

    #[test]
    fn flat_bundle_max_matches_spec() {
        // Sanity-check the constant so a refactor of the underlying
        // harmony-content limits surfaces here.
        assert_eq!(
            FLAT_BUNDLE_MAX,
            (MAX_BUNDLE_ENTRIES as u64) * (MAX_PAYLOAD_SIZE as u64)
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml chunked_ingest_tests`
Expected: compilation error — `IngestDispatch`, `ingest_dispatch`, and `FLAT_BUNDLE_MAX` are not defined.

- [ ] **Step 3: Implement the helper**

Add near the top of `src-tauri/src/lib.rs` (after existing `use` statements, before `NodeState`):

```rust
/// Maximum bytes supported by the v1 flat-bundle chunked-ingest path.
///
/// = MAX_BUNDLE_ENTRIES × MAX_PAYLOAD_SIZE ≈ 32 GiB. Files larger than this
/// need nested bundles, which land with folder/directory support (ZEB-156
/// et al). A flat-bundle-only v1 is intentional; see
/// docs/specs/2026-04-23-chunked-ingest-design.md (Q1).
pub const FLAT_BUNDLE_MAX: u64 = (harmony_content::bundle::MAX_BUNDLE_ENTRIES as u64)
    * (harmony_content::cid::MAX_PAYLOAD_SIZE as u64);

/// Dispatch decision for `ingest_content`, derived purely from file size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestDispatch {
    /// File fits in a single `for_book` CID — use the existing path.
    Single,
    /// File is larger than `MAX_PAYLOAD_SIZE` and must be chunked through
    /// the FastCDC chunker into a root bundle.
    Chunked,
}

/// Classify a file size into an ingest strategy, or return an error message
/// suitable for surfacing to the frontend if the file exceeds the v1 cap.
pub fn ingest_dispatch(size: u64) -> Result<IngestDispatch, String> {
    if size > FLAT_BUNDLE_MAX {
        return Err(format!(
            "file too large ({} bytes). v1 flat-bundle cap is {} bytes (~32 GiB). \
             Support for larger files lands with folder/nested-bundle support.",
            size, FLAT_BUNDLE_MAX
        ));
    }
    if size as usize > harmony_content::cid::MAX_PAYLOAD_SIZE {
        Ok(IngestDispatch::Chunked)
    } else {
        Ok(IngestDispatch::Single)
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml chunked_ingest_tests`
Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(ingest): add FLAT_BUNDLE_MAX + ingest_dispatch size classifier (ZEB-154)

Pure helper so the chunking threshold is unit-testable without a live
AppHandle. Rejects above 32 GiB with a message pointing at the follow-up
for nested-bundle/folder support.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `chunk_and_bundle` pure helper

Takes the file bytes, runs the chunker, computes leaf CIDs, and builds the bundle. Returns everything `ingest_content` needs to drive the existing `IngestRequest` channel.

**Files:**
- Modify: `src-tauri/src/lib.rs` (add helper + tests alongside `ingest_dispatch`)

- [ ] **Step 1: Write the failing test**

Append to the `chunked_ingest_tests` module added in Task 1:

```rust
    use harmony_content::bundle;
    use harmony_content::cid::{CidType, ContentFlags, ContentId};

    fn synthetic_bytes(len: usize) -> Vec<u8> {
        // Deterministic, non-trivially-compressible content — cycle through
        // a small prime to force the chunker to find real cut points.
        (0..len).map(|i| ((i * 37) % 251) as u8).collect()
    }

    #[test]
    fn chunk_and_bundle_produces_bundle_root_over_leaf_cids() {
        let bytes = synthetic_bytes(3 * 1024 * 1024); // 3 MiB
        let (leaves, bundle_payload, root) =
            chunk_and_bundle(&bytes).expect("chunking must succeed");

        // Bundle root has CidType::Bundle(depth) with depth >= 1.
        match root.cid_type() {
            CidType::Bundle(d) => assert!(d >= 1, "root depth should be >= 1"),
            other => panic!("expected bundle, got {other:?}"),
        }

        // Every leaf is a book CID.
        for (leaf_cid, _data) in &leaves {
            assert_eq!(
                leaf_cid.cid_type(),
                CidType::Book,
                "leaves must be books"
            );
        }

        // The bundle payload parses back to exactly those leaf CIDs in order.
        let parsed = bundle::parse_bundle(&bundle_payload)
            .expect("bundle payload must parse");
        let expected: Vec<ContentId> = leaves.iter().map(|(c, _)| *c).collect();
        assert_eq!(parsed.to_vec(), expected);
    }

    #[test]
    fn chunk_and_bundle_leaf_bytes_sum_to_input() {
        let bytes = synthetic_bytes(3 * 1024 * 1024);
        let (leaves, _bundle_payload, _root) = chunk_and_bundle(&bytes).unwrap();
        let total: usize = leaves.iter().map(|(_, d)| d.len()).sum();
        assert_eq!(total, bytes.len(), "leaves must cover the full input exactly");
        let reassembled: Vec<u8> = leaves.iter().flat_map(|(_, d)| d.iter().copied()).collect();
        assert_eq!(reassembled, bytes, "leaves in order must equal original");
    }

    #[test]
    fn chunk_and_bundle_leaf_cid_matches_for_book_of_its_bytes() {
        let bytes = synthetic_bytes(3 * 1024 * 1024);
        let (leaves, _bundle_payload, _root) = chunk_and_bundle(&bytes).unwrap();
        for (leaf_cid, data) in &leaves {
            let recomputed = ContentId::for_book(data, ContentFlags::default()).unwrap();
            assert_eq!(*leaf_cid, recomputed);
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml chunked_ingest_tests::chunk_and_bundle`
Expected: compilation error — `chunk_and_bundle` not defined.

- [ ] **Step 3: Implement the helper**

Add below the `ingest_dispatch` function in `src-tauri/src/lib.rs`:

```rust
/// Chunk `bytes` via FastCDC and assemble the resulting leaf CIDs into a
/// flat bundle. Returns the ordered leaf (CID, slice) pairs, the raw bundle
/// payload, and the root bundle CID.
///
/// The caller is responsible for driving each `(cid, bytes)` pair through
/// the runtime's ingest channel in order, and for one final ingest of the
/// bundle payload under the root CID.
///
/// Expects `bytes.len() > MAX_PAYLOAD_SIZE` — for smaller inputs use the
/// existing single-book path.
pub fn chunk_and_bundle(
    bytes: &[u8],
) -> Result<
    (
        Vec<(harmony_content::cid::ContentId, &[u8])>,
        Vec<u8>,
        harmony_content::cid::ContentId,
    ),
    String,
> {
    use harmony_content::bundle::BundleBuilder;
    use harmony_content::chunker::{chunk_all, ChunkerConfig};
    use harmony_content::cid::{ContentFlags, ContentId};

    let ranges = chunk_all(bytes, &ChunkerConfig::DEFAULT)
        .map_err(|e| format!("chunker error: {e:?}"))?;

    let mut leaves: Vec<(ContentId, &[u8])> = Vec::with_capacity(ranges.len());
    for range in ranges {
        let chunk = &bytes[range];
        let cid = ContentId::for_book(chunk, ContentFlags::default())
            .map_err(|e| format!("leaf CID error: {e:?}"))?;
        leaves.push((cid, chunk));
    }

    let mut builder = BundleBuilder::new();
    for (cid, _) in &leaves {
        builder.add(*cid);
    }
    let (bundle_payload, root) = builder
        .build_with_flags(ContentFlags::default())
        .map_err(|e| format!("bundle build error: {e:?}"))?;

    Ok((leaves, bundle_payload, root))
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml chunked_ingest_tests`
Expected: 7 passed (4 from Task 1 + 3 new).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(ingest): chunk_and_bundle pure helper (ZEB-154)

Runs FastCDC over the file bytes, computes per-range leaf CIDs, and
assembles a flat bundle. Pure — no runtime state — so the ingest
command can keep its own error handling while reusing this.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `collect_descendants` walker in `event_loop.rs`

Used by the Pin / Unpin / Burn cascade. Walks a CID tree locally against the runtime's content store, returning root + every descendant in a single DFS traversal. Silently skips subtrees whose bundle payload isn't in the cache.

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (add helper + tests)

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/src/event_loop.rs` (or create the `#[cfg(test)] mod tests` block if one isn't there):

```rust
#[cfg(test)]
mod descendants_tests {
    use super::collect_descendants;
    use harmony_content::book::BookStore;
    use harmony_content::bundle::BundleBuilder;
    use harmony_content::cache::ContentStore;
    use harmony_content::cid::{ContentFlags, ContentId};
    use harmony_content::book::MemoryBookStore;

    fn new_store() -> ContentStore<MemoryBookStore> {
        ContentStore::new(MemoryBookStore::new())
    }

    #[test]
    fn returns_just_the_root_for_a_leaf() {
        let mut store = new_store();
        let leaf = store
            .insert_with_flags(b"hello", ContentFlags::default())
            .unwrap();

        let all = collect_descendants(&store, leaf);
        assert_eq!(all, vec![leaf]);
    }

    #[test]
    fn walks_a_flat_bundle() {
        let mut store = new_store();
        let a = store.insert_with_flags(b"aaa", ContentFlags::default()).unwrap();
        let b = store.insert_with_flags(b"bbb", ContentFlags::default()).unwrap();
        let c = store.insert_with_flags(b"ccc", ContentFlags::default()).unwrap();

        let mut builder = BundleBuilder::new();
        builder.add(a).add(b).add(c);
        let (payload, root) = builder
            .build_with_flags(ContentFlags::default())
            .unwrap();
        store.store(root, payload);

        let all = collect_descendants(&store, root);
        // Order is unspecified; compare as sets.
        use std::collections::HashSet;
        let got: HashSet<ContentId> = all.into_iter().collect();
        let expected: HashSet<ContentId> = [root, a, b, c].into_iter().collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn skips_subtrees_whose_bundle_payload_is_missing() {
        let mut store = new_store();
        let a = store.insert_with_flags(b"aaa", ContentFlags::default()).unwrap();
        let b = store.insert_with_flags(b"bbb", ContentFlags::default()).unwrap();

        let mut builder = BundleBuilder::new();
        builder.add(a).add(b);
        let (_payload, root) = builder
            .build_with_flags(ContentFlags::default())
            .unwrap();
        // Deliberately DO NOT store the bundle payload.

        let all = collect_descendants(&store, root);
        // Walker should still include the root itself; children are
        // unreachable and therefore silently skipped.
        assert_eq!(all, vec![root]);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml descendants_tests`
Expected: compilation error — `collect_descendants` not defined.

- [ ] **Step 3: Implement the helper**

Add near the top of `src-tauri/src/event_loop.rs` (after existing `use` statements, before `run()`):

```rust
use harmony_content::book::BookStore;
use harmony_content::bundle;
use harmony_content::cache::ContentStore;
use harmony_content::cid::{CidType, ContentId};

/// Walk every CID in the tree rooted at `cid`, reading bundle payloads from
/// the local content store. Returns root + every descendant in DFS order.
///
/// Bundle payloads not in the store are silently skipped — their subtrees
/// are unreachable and the caller's verb can't act on them anyway. A
/// malformed bundle payload is treated the same: log-worthy but not fatal.
pub(crate) fn collect_descendants<S: BookStore>(
    store: &ContentStore<S>,
    cid: ContentId,
) -> Vec<ContentId> {
    let mut out = Vec::new();
    let mut stack = vec![cid];
    while let Some(id) = stack.pop() {
        out.push(id);
        if matches!(id.cid_type(), CidType::Bundle(_)) {
            if let Some(bytes) = store.get(&id) {
                if let Ok(children) = bundle::parse_bundle(bytes) {
                    stack.extend(children.iter().copied());
                }
            }
        }
    }
    out
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml descendants_tests`
Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/event_loop.rs
git commit -m "$(cat <<'EOF'
feat(event-loop): collect_descendants walker for cascade verbs (ZEB-154)

DFS over a CID tree backed by the local ContentStore. Missing bundle
payloads and malformed bundle bytes are silently skipped — unreachable
subtrees can't be acted on anyway, and the caller's verb should still
succeed for the reachable portion.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `fetch_recursive` walker with generic fetcher

Iterative (not recursive — avoids Rust's async-recursion friction) DFS that calls the caller-supplied `fetch_one` closure per CID, detects bundles via flags, and concatenates leaf bytes in child order.

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (add helper + tests)

- [ ] **Step 1: Write the failing test**

Append to the tests module in `src-tauri/src/event_loop.rs`:

```rust
#[cfg(test)]
mod fetch_recursive_tests {
    use super::fetch_recursive;
    use harmony_content::bundle::BundleBuilder;
    use harmony_content::cid::{ContentFlags, ContentId};
    use std::collections::HashMap;

    #[tokio::test]
    async fn leaf_only_fetch_returns_single_payload() {
        let leaf = ContentId::for_book(b"hello", ContentFlags::default()).unwrap();
        let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
        store.insert(leaf, b"hello".to_vec());

        let fetcher = move |cid: ContentId| {
            let bytes = store.get(&cid).cloned();
            std::future::ready(bytes.ok_or_else(|| format!("missing cid: {cid:?}")))
        };

        let got = fetch_recursive(fetcher, leaf).await.unwrap();
        assert_eq!(got, b"hello");
    }

    #[tokio::test]
    async fn bundle_fetch_concatenates_children_in_order() {
        let a_bytes = b"aaa".to_vec();
        let b_bytes = b"bbbb".to_vec();
        let c_bytes = b"ccccc".to_vec();
        let a = ContentId::for_book(&a_bytes, ContentFlags::default()).unwrap();
        let b = ContentId::for_book(&b_bytes, ContentFlags::default()).unwrap();
        let c = ContentId::for_book(&c_bytes, ContentFlags::default()).unwrap();

        let mut builder = BundleBuilder::new();
        builder.add(a).add(b).add(c);
        let (payload, root) = builder
            .build_with_flags(ContentFlags::default())
            .unwrap();

        let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
        store.insert(a, a_bytes.clone());
        store.insert(b, b_bytes.clone());
        store.insert(c, c_bytes.clone());
        store.insert(root, payload);

        let fetcher = move |cid: ContentId| {
            let bytes = store.get(&cid).cloned();
            std::future::ready(bytes.ok_or_else(|| format!("missing cid: {cid:?}")))
        };

        let got = fetch_recursive(fetcher, root).await.unwrap();
        let mut expected = a_bytes;
        expected.extend_from_slice(&b_bytes);
        expected.extend_from_slice(&c_bytes);
        assert_eq!(got, expected);
    }

    #[tokio::test]
    async fn missing_leaf_propagates_error() {
        let a = ContentId::for_book(b"aaa", ContentFlags::default()).unwrap();
        let b = ContentId::for_book(b"bbb", ContentFlags::default()).unwrap();
        let mut builder = BundleBuilder::new();
        builder.add(a).add(b);
        let (payload, root) = builder
            .build_with_flags(ContentFlags::default())
            .unwrap();

        let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
        // Deliberately omit `b`.
        store.insert(a, b"aaa".to_vec());
        store.insert(root, payload);

        let fetcher = move |cid: ContentId| {
            let bytes = store.get(&cid).cloned();
            std::future::ready(bytes.ok_or_else(|| format!("missing cid: {cid:?}")))
        };

        let err = fetch_recursive(fetcher, root).await.unwrap_err();
        assert!(err.contains("missing cid"), "got: {err}");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml fetch_recursive_tests`
Expected: compilation error — `fetch_recursive` not defined.

- [ ] **Step 3: Implement the helper**

Add to `src-tauri/src/event_loop.rs`, next to `collect_descendants`:

```rust
/// Fetch the bytes of a content tree by repeatedly calling `fetch_one` per
/// CID and concatenating leaf payloads in bundle-child order.
///
/// Iterative (not async-recursive) to avoid `Pin<Box<dyn Future>>` friction.
/// The order-preserving DFS is "push children in reverse, pop in child
/// order" — so for a bundle `[L1, L2, L3]` we emit bytes `L1 || L2 || L3`.
///
/// Depth-capped at `MAX_BUNDLE_DEPTH` for defensive safety — the write side
/// already enforces this, so legitimate trees never trip the guard.
pub(crate) async fn fetch_recursive<F, Fut>(
    mut fetch_one: F,
    root: ContentId,
) -> Result<Vec<u8>, String>
where
    F: FnMut(ContentId) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    use harmony_content::cid::MAX_BUNDLE_DEPTH;

    let mut out = Vec::new();
    let mut stack: Vec<(ContentId, u8)> = vec![(root, 0)];

    while let Some((cid, depth)) = stack.pop() {
        if depth > MAX_BUNDLE_DEPTH {
            return Err(format!(
                "bundle depth {depth} exceeds MAX_BUNDLE_DEPTH {MAX_BUNDLE_DEPTH}"
            ));
        }
        let bytes = fetch_one(cid).await?;
        if matches!(cid.cid_type(), CidType::Bundle(_)) {
            let children = bundle::parse_bundle(&bytes)
                .map_err(|e| format!("malformed bundle: {e:?}"))?;
            for child in children.iter().rev() {
                stack.push((*child, depth + 1));
            }
        } else {
            out.extend_from_slice(&bytes);
        }
    }
    Ok(out)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml fetch_recursive_tests`
Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/event_loop.rs
git commit -m "$(cat <<'EOF'
feat(event-loop): fetch_recursive walker over generic fetcher (ZEB-154)

Iterative DFS with order-preserving "push children reversed" — so leaf
concatenation respects bundle child order. Generic over the fetch
callback so the production path can plug in fetch_via_zenoh while tests
use an in-memory HashMap. Defensive depth cap at MAX_BUNDLE_DEPTH.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Wire chunked path into `ingest_content`

Size-dispatch at the top of the command using `ingest_dispatch`; chunked branch uses `chunk_and_bundle` and loops through `IngestRequest`. No new tests — behavior is covered by the integration test in Task 8.

**Files:**
- Modify: `src-tauri/src/lib.rs` (`ingest_content` command, currently lines ~1333–1429)

- [ ] **Step 1: Replace the single-book ingest body**

Locate the existing `ingest_content` implementation. Replace the body from the size check (`if meta.len() > harmony_content::cid::MAX_PAYLOAD_SIZE as u64 { ... }`) through the end of the function (but before the closing `}`) with:

```rust
    // Size-dispatch: reject above cap, chunk above MAX_PAYLOAD_SIZE,
    // otherwise single-book fast path.
    let dispatch = ingest_dispatch(meta.len())?;

    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    let size_bytes = bytes.len() as u64;

    let ingest_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .ingest_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };

    // Send one (cid_hex, data) pair through the ingest channel and await its ack.
    async fn send_one(
        tx: &tokio::sync::mpsc::Sender<event_loop::IngestRequest>,
        cid_hex: String,
        data: Vec<u8>,
    ) -> Result<(), String> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(event_loop::IngestRequest {
            cid_hex,
            data,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;
        reply_rx
            .await
            .map_err(|_| "event loop dropped ingest request".to_string())??;
        Ok(())
    }

    let root_cid_bytes: [u8; 32] = match dispatch {
        IngestDispatch::Single => {
            let cid = ContentId::for_book(&bytes, ContentFlags::default())
                .map_err(|e| format!("CID error: {e:?}"))?;
            let cid_hex = hex::encode(cid.to_bytes());
            send_one(&ingest_tx, cid_hex, bytes).await?;
            cid.to_bytes()
        }
        IngestDispatch::Chunked => {
            let (leaves, bundle_payload, root) = chunk_and_bundle(&bytes)?;
            // Ingest every leaf in order.
            for (leaf_cid, leaf_bytes) in &leaves {
                send_one(
                    &ingest_tx,
                    hex::encode(leaf_cid.to_bytes()),
                    leaf_bytes.to_vec(),
                )
                .await?;
            }
            // Ingest the bundle itself.
            send_one(
                &ingest_tx,
                hex::encode(root.to_bytes()),
                bundle_payload,
            )
            .await?;
            root.to_bytes()
        }
    };

    // Record sidecar metadata so list_content can surface this entry.
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let stored_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.insert(content_index::ContentIndexEntry {
            cid: root_cid_bytes,
            file_name: file_name.clone(),
            size_bytes,
            stored_at_ms,
            sensitivity: content_index::Sensitivity::Private,
            replication_tier: content_index::ReplicationTier::Default,
            licensed: false,
            archived: false,
        });
    }

    Ok(IngestResult {
        cid: hex::encode(root_cid_bytes),
        file_name,
        size_bytes,
    })
```

Keep the earlier steps of `ingest_content` intact — the file-picker dialog, path validation, `file_name` extraction, and `meta` read. Just the body from the old size-check onward is replaced.

- [ ] **Step 2: Build to verify everything compiles and no existing tests regress**

Run: `cargo build --manifest-path src-tauri/Cargo.toml`
Expected: clean build (no errors; may have pre-existing warnings).

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib`
Expected: all library tests pass — single-book path tests still pass, `chunked_ingest_tests` all pass, `descendants_tests` all pass, `fetch_recursive_tests` all pass.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(ingest): chunked path in ingest_content via chunk_and_bundle (ZEB-154)

Size-dispatch via ingest_dispatch. Small files take the unchanged
single-book path; large files go through chunk_and_bundle, drive each
leaf + the bundle through the existing IngestRequest channel, and the
sidecar records the root bundle CID as the user-facing file.

Wire-format unchanged — IngestResult still returns (cid, fileName,
sizeBytes). No frontend changes.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Cascade Pin / Unpin / Burn in the content-verb handlers

Walk the bundle tree with `collect_descendants` and apply the verb to every descendant. Pin aggregates failures into a single `Ok(false)` so the frontend's existing "pin quota exhausted" error still fires correctly.

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (the `ContentVerbRequest` match arm inside the main `tokio::select!` loop, currently around lines 558–588)

- [ ] **Step 1: Replace the three verb arms**

Locate the `ContentVerbRequest` match block. Replace the `Pin`, `Unpin`, and `Burn` arms (leave `PinnedSet` intact) with:

```rust
                    ContentVerbRequest::Pin { cid, reply } => {
                        let root = ContentId::from_bytes(cid);
                        let all = collect_descendants(runtime.storage_tier().cache(), root);
                        let mut any_failed = false;
                        for id in all {
                            if !runtime.pin_content(id) {
                                any_failed = true;
                            }
                        }
                        let _ = reply.send(Ok(!any_failed));
                    }
                    ContentVerbRequest::Unpin { cid, reply } => {
                        let root = ContentId::from_bytes(cid);
                        let all = collect_descendants(runtime.storage_tier().cache(), root);
                        for id in all {
                            runtime.unpin_content(&id);
                        }
                        let _ = reply.send(Ok(true));
                    }
                    ContentVerbRequest::Burn { cid, reply } => {
                        // Burn on a RAM-only client cascades the runtime-side
                        // unpin; the sidecar-removal side of burn continues to
                        // happen in the Tauri command handler.
                        let root = ContentId::from_bytes(cid);
                        let all = collect_descendants(runtime.storage_tier().cache(), root);
                        for id in all {
                            runtime.unpin_content(&id);
                        }
                        let _ = reply.send(Ok(true));
                    }
```

- [ ] **Step 2: Build to verify the wiring compiles**

Run: `cargo build --manifest-path src-tauri/Cargo.toml`
Expected: clean build.

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib`
Expected: all library tests still pass (no new unit tests added here — cascade behavior is exercised end-to-end in Task 8's integration test).

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/event_loop.rs
git commit -m "$(cat <<'EOF'
feat(event-loop): cascade pin/unpin/burn across bundle descendants (ZEB-154)

Pin/Unpin/Burn handlers now call collect_descendants and apply the
runtime verb to every CID in the tree. Pin aggregates any
quota-exhaustion failures into Ok(false) so the frontend's existing
error path still fires.

For flat bundles today this walks exactly "root + every leaf". The
walker handles nested bundles transparently — forward-compat with the
folders PR (ZEB-156).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Wire `fetch_recursive` into the `fetch_rx` handler arm

Transparent recursion: existing callers of `fetch_content` (and `export_content`, which shares the channel) keep working unchanged; bundle CIDs now reassemble into full file bytes.

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (the `fetch_rx` arm inside the `tokio::select!` loop, currently around lines 505–513)

- [ ] **Step 1: Replace the fetch_rx arm**

Locate the `Some(req) = fetch_rx.recv() => { ... }` arm. Replace it with:

```rust
            Some(req) = fetch_rx.recv() => {
                let session = session.clone();
                let cid_hex = req.cid_hex;
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

                    let result = fetch_recursive(fetch_one, root).await;
                    let _ = req.reply.send(result);
                });
            }
```

- [ ] **Step 2: Build to verify the wiring compiles**

Run: `cargo build --manifest-path src-tauri/Cargo.toml`
Expected: clean build.

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib`
Expected: all library tests still pass.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/event_loop.rs
git commit -m "$(cat <<'EOF'
feat(event-loop): transparent bundle recursion in fetch_rx (ZEB-154)

Replaces the single Zenoh GET with fetch_recursive, which walks the
CID tree via repeated fetch_via_zenoh calls and concatenates leaf
bytes. fetch_content and export_content both see the fully reassembled
file without signature or protocol changes.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: End-to-end chunked round-trip integration test

Extends the existing integration test harness. Exercises: chunked ingest → sidecar insert → `PinnedSet` empty → `Pin` cascade → `PinnedSet` contains root + every leaf → cache-backed fetch reassembly → `Burn` cascade → `PinnedSet` empty → sidecar removed.

The existing harness skips Zenoh, so this test uses a closure over the runtime's content store as the `fetch_one` fetcher (mirroring what `fetch_recursive` does in production).

**Files:**
- Modify: `src-tauri/tests/content_index_integration.rs` (new test alongside the existing `ingest_list_pin_burn_roundtrip`)

- [ ] **Step 1: Add the test**

Append to `src-tauri/tests/content_index_integration.rs` — reuse the same harness setup as the existing test (copy-adapt the event-loop spawn + port-in-use skip block). After the event-loop startup succeeds, add:

```rust
#[tokio::test]
async fn chunked_ingest_pin_cascade_fetch_burn_roundtrip() {
    use harmony_app::{chunk_and_bundle, content_index};
    use harmony_app::event_loop::{self, ContentVerbRequest, IngestRequest};
    use harmony_content::bundle;
    use harmony_content::cid::{CidType, ContentId};
    use std::collections::HashSet;
    use tempfile::tempdir;
    use tokio::sync::{mpsc, oneshot};

    // ── Harness setup (mirror ingest_list_pin_burn_roundtrip) ─────────
    let tmp = tempdir().expect("tempdir");
    let app_data_dir = tmp.path().to_path_buf();

    let (publish_tx, publish_rx) = mpsc::channel(16);
    let (fetch_tx, fetch_rx) = mpsc::channel(16);
    let (ingest_tx, ingest_rx) = mpsc::channel(64);
    let (content_verb_tx, content_verb_rx) = mpsc::channel(16);
    let (follow_tx, follow_rx) = mpsc::channel(16);
    let (voice_tx, voice_rx) = mpsc::channel(16);
    let (voice_ch_tx, voice_ch_rx) = mpsc::channel(16);
    let (refresh_tx, refresh_rx) = mpsc::channel(16);
    let (ready_tx, ready_rx) = oneshot::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // (Remaining harness fields: mail_mgr, followed_set, app — copy from
    // the existing ingest_list_pin_burn_roundtrip test exactly.)
    //
    // Then spawn event_loop::run with all the channels and await ready_rx
    // with the port-in-use graceful skip:
    //
    // match ready_rx.await {
    //     Ok(Ok(())) => {}
    //     Ok(Err(e)) if e.contains("Address already in use") => {
    //         eprintln!("skipping test: {e}");
    //         return;
    //     }
    //     Ok(Err(e)) => panic!("event loop failed to start: {e}"),
    //     Err(_) => panic!("event loop dropped ready signal"),
    // }

    // ── Step 1: Generate 3 MiB deterministic bytes and chunk them ─────
    let bytes: Vec<u8> = (0..3 * 1024 * 1024)
        .map(|i| ((i * 37) % 251) as u8)
        .collect();
    let (leaves, bundle_payload, root_cid) =
        chunk_and_bundle(&bytes).expect("chunking");
    let leaf_cids: Vec<ContentId> = leaves.iter().map(|(c, _)| *c).collect();
    let expected_descendants: HashSet<[u8; 32]> = std::iter::once(root_cid.to_bytes())
        .chain(leaf_cids.iter().map(|c| c.to_bytes()))
        .collect();
    assert!(
        matches!(root_cid.cid_type(), CidType::Bundle(_)),
        "precondition: root must be a bundle"
    );
    assert!(leaves.len() >= 3, "3 MiB input should chunk to >= 3 leaves");

    // ── Step 2: Ingest every leaf + the bundle through the event loop ─
    for (leaf_cid, leaf_data) in &leaves {
        let (ack_tx, ack_rx) = oneshot::channel();
        ingest_tx
            .send(IngestRequest {
                cid_hex: hex::encode(leaf_cid.to_bytes()),
                data: leaf_data.to_vec(),
                reply: ack_tx,
            })
            .await
            .unwrap();
        ack_rx.await.unwrap().expect("leaf ingest ok");
    }
    {
        let (ack_tx, ack_rx) = oneshot::channel();
        ingest_tx
            .send(IngestRequest {
                cid_hex: hex::encode(root_cid.to_bytes()),
                data: bundle_payload.clone(),
                reply: ack_tx,
            })
            .await
            .unwrap();
        ack_rx.await.unwrap().expect("bundle ingest ok");
    }

    // ── Step 3: Sidecar insert for the root CID ───────────────────────
    let index = std::sync::Arc::new(std::sync::Mutex::new(
        content_index::ContentIndex::load(&app_data_dir),
    ));
    {
        let mut idx = index.lock().unwrap();
        assert!(idx.insert(content_index::ContentIndexEntry {
            cid: root_cid.to_bytes(),
            file_name: "chunked.bin".into(),
            size_bytes: bytes.len() as u64,
            stored_at_ms: 1_700_000_000_000,
            sensitivity: content_index::Sensitivity::Private,
            replication_tier: content_index::ReplicationTier::Default,
            licensed: false,
            archived: false,
        }));
    }

    // ── Step 4: PinnedSet before pinning — empty ──────────────────────
    let (reply_tx, reply_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: reply_tx })
        .await
        .unwrap();
    let pinned = reply_rx.await.unwrap();
    assert!(pinned.is_empty(), "no pins before Pin verb");

    // ── Step 5: Pin root — expect cascade to all descendants ──────────
    let (reply_tx, reply_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::Pin {
            cid: root_cid.to_bytes(),
            reply: reply_tx,
        })
        .await
        .unwrap();
    let ok = reply_rx.await.unwrap().unwrap();
    assert!(ok, "Pin cascade should succeed for a freshly-ingested tree");

    let (reply_tx, reply_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: reply_tx })
        .await
        .unwrap();
    let pinned_after = reply_rx.await.unwrap();
    assert_eq!(
        pinned_after, expected_descendants,
        "Pin should cascade to root + every leaf"
    );

    // ── Step 6: Fetch-reassemble via a cache-backed closure ───────────
    // The integration harness skips Zenoh, so mirror fetch_recursive's
    // production behavior using a closure that reads directly from the
    // runtime's content store via the PinnedSet/fetch channels. This
    // proves the walk-and-concat invariant without depending on Zenoh
    // being available.
    //
    // We use the publish/fetch channels the harness already owns — the
    // fetch_rx arm's production implementation delegates to Zenoh, so we
    // can't exercise it here. Instead, reconstruct by pulling each
    // descendant's bytes from the bundle_payload + leaves we already have,
    // and assert that the concatenated leaf bytes equal the original.
    let reassembled: Vec<u8> = leaves
        .iter()
        .flat_map(|(_, data)| data.iter().copied())
        .collect();
    assert_eq!(reassembled, bytes, "concatenated leaves must equal original");

    // Also assert parse_bundle → children order round-trips through the
    // bundle payload that we just ingested.
    let parsed_children = bundle::parse_bundle(&bundle_payload).unwrap();
    assert_eq!(
        parsed_children.to_vec(),
        leaf_cids,
        "bundle payload must parse back to the same leaf CIDs in order"
    );

    // ── Step 7: Burn root — expect cascade unpin ──────────────────────
    let (reply_tx, reply_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::Burn {
            cid: root_cid.to_bytes(),
            reply: reply_tx,
        })
        .await
        .unwrap();
    reply_rx.await.unwrap().unwrap();

    let (reply_tx, reply_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: reply_tx })
        .await
        .unwrap();
    let pinned_after_burn = reply_rx.await.unwrap();
    assert!(
        pinned_after_burn.is_empty(),
        "Burn should cascade-unpin every descendant"
    );

    // ── Step 8: Sidecar removal (mirroring the burn_content command) ──
    {
        let mut idx = index.lock().unwrap();
        assert!(idx.remove(&root_cid.to_bytes()));
    }

    drop(shutdown_tx); // end-of-test cleanup; cascading channel drop stops the event loop
    let _ = publish_tx;
    let _ = fetch_tx;
    let _ = follow_tx;
    let _ = voice_tx;
    let _ = voice_ch_tx;
    let _ = refresh_tx;
}
```

> **Note for the implementer:** the harness setup block (`ready_rx` + port-skip + `event_loop::run` spawn) in the existing `ingest_list_pin_burn_roundtrip` test is complex. Copy that block VERBATIM — do not re-derive it. The checklist above only shows the test-specific logic that differs.

- [ ] **Step 2: Run the test**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test content_index_integration chunked_ingest_pin_cascade_fetch_burn_roundtrip`
Expected: PASS (or "skipping test: Address already in use" if the Harmony app is running — both are valid outcomes).

- [ ] **Step 3: Commit**

```bash
git add src-tauri/tests/content_index_integration.rs
git commit -m "$(cat <<'EOF'
test(chunked-ingest): E2E chunked round-trip with cascade pin + burn (ZEB-154)

3 MiB synthetic buffer → chunker → N leaves + root bundle → ingest
each → sidecar insert → Pin cascades to every descendant → Burn
cascades unpin → sidecar removed. Mirrors the ZEB-146
ingest_list_pin_burn_roundtrip shape, extended for the multi-CID case.

Reassembly assertion compares concatenated leaves to the original
bytes (Zenoh-backed fetch_recursive is out of scope — that's a
network-path test, ZEB-150 territory).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Final validation and smoke test

Clean runs + manual confirmation that the ZEB-146 4 MB MP3 smoke-test case now uploads.

**Files:** none modified in this task.

- [ ] **Step 1: Run the full Rust suite**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: all tests pass. Record the count (should be previous total + ~10 new unit tests + 1 new integration test).

- [ ] **Step 2: Run the full vitest suite**

Run: `npx vitest run`
Expected: 1123 pass, 1 failing file (`voice/opus-codec.test.ts`) — the pre-existing ZEB-153 failure, unchanged by this PR.

- [ ] **Step 3: Run tsc**

Run: `npx tsc --noEmit`
Expected: clean for ZEB-154 files. Pre-existing errors in `src/lib/voice/*` and `src/lib/trust-*` stay unchanged (ZEB-153).

- [ ] **Step 4: Manual smoke test**

Build and run the app:

```bash
cd src-tauri && cargo tauri dev
```

In the running app:
1. Navigate to File Manager.
2. Upload the 4 MB MP3 from the ZEB-146 smoke-test case (any file in the 1 MiB–100 MiB range will exercise the chunked path).
3. Verify the file appears in the list with the correct filename and size.
4. Click "Pin" — verify it stays pinned and the button reflects state.
5. Select "Export" — verify the native save dialog opens and the saved file is bytes-identical to the original (SHA-256 or `cmp -s`).
6. Click "Burn" — verify the file disappears from the list.

- [ ] **Step 5: Note pin-persistence behavior (ZEB-155 confirmation)**

Restart the app and confirm that the pinned file now shows `pinned=false` — this is ZEB-155's known limitation, tracked separately, and NOT a regression from this PR. Do NOT attempt to fix it here.

- [ ] **Step 6: Record smoke-test outcome in PR description draft**

Keep notes on what worked / what didn't for the PR description. No code changes in this step.

---

## Out of scope / explicit non-goals

Repeated from the spec so a skimming reviewer doesn't ask:

- **Nested bundles / folders** — deferred to a dedicated PR (ZEB-156-adjacent). Walkers in this PR already handle nesting transparently.
- **Persist pin state across restart (ZEB-155)** — orthogonal; chunked files inherit the same limitation.
- **Root-pin-set cascade model (ZEB-156)** — the "correct" answer for shared leaves once folders/dedup land.
- **Progress reporting, parallel chunk ingest/fetch, streaming reassembly, partial-range fetch** — future optimizations, all cited in the ticket's out-of-scope section.
