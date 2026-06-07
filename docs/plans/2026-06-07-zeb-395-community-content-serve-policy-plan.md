# ZEB-395 — Community content serve policy (v1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let community members fetch each other's encrypted community state-root blob over CAS (unblocking cross-machine community sync) without re-opening serving of private encrypted content (DMs, private profiles).

**Architecture:** A process-wide `CommunityServeAllowlist` (an `Arc<RwLock<HashSet<ContentId>>>`) records the community-root CIDs this node has published. The production `RuntimeContentStore` registers a root CID in the allowlist when `publish_root_now` calls a new `put_serveable` trait method; the content-serve queryable consults the same (Arc-shared) allowlist and serves an encrypted CID only if it is allowlisted. Private encrypted blobs use plain `put` and stay refused. No community config structs and no `CasOp` variants change — the handle is shared by `Arc` clone between the two production sites (`RuntimeContentStore` and `event_loop::run`).

**Tech Stack:** Rust, `async_trait`, `zenoh` (queryable serve/GET), `tokio`, `harmony_content::cid::{ContentId, ContentFlags}`.

**Spec:** `docs/specs/2026-06-07-zeb-395-community-content-serve-policy-design.md` (commit `86fccf9b`).
**Branch:** `zeb-395-community-content-serve` (already created off main; spec commits `68971fdf`, `86fccf9b`).

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `src-tauri/src/content_store.rs` | CAS trait + stores | NEW `CommunityServeAllowlist` type; NEW `put_serveable` trait method (default == `put`); `RuntimeContentStore` gains optional allowlist field + `with_serve_allowlist` builder + `put_serveable` override. Unit tests. |
| `src-tauri/src/event_loop.rs` | content-serve queryable + run() | `spawn_content_serve_queryable` gains `serve_allowlist` param; gate consults it. `run()` gains a trailing `serve_allowlist` param and forwards it to the queryable. Serve-gate unit test. |
| `src-tauri/src/lib.rs` | `start_node` wiring | Create the allowlist once; `.with_serve_allowlist(...)` on the production `RuntimeContentStore`; pass the clone as the trailing arg to `event_loop::run`. |
| `src-tauri/src/community_state_sync.rs` | community state-root publish | `publish_root_now`: swap the single `content_store.put(root_cid, …)` for `put_serveable`. |
| `src-tauri/tests/community_serve_allowlist_integration.rs` | NEW regression test | Two-node cross-store serve: allowlisted encrypted CID is served; non-allowlisted encrypted CID is refused; public control proves liveness. |
| `src-tauri/tests/cas_serve_two_node_integration.rs`<br>`src-tauri/tests/profile_card_avatar_cross_peer_integration.rs`<br>`src-tauri/tests/profile_page_cross_peer_integration.rs` | existing queryable callers | Pass an empty `CommunityServeAllowlist::new()` to the new param (behavior unchanged). |

**Per-task discipline (applies to EVERY implementer task):**
- Work on branch `zeb-395-community-content-serve`. Do NOT touch any other branch. Never create a worktree.
- **Commit BEFORE running the verification gate** (so a hung gate never loses work).
- **10-minute wall-clock kill switch** on every `cargo` command (`timeout`-style discipline; if a command exceeds ~10 min, stop, report `DONE_WITH_CONCERNS` with what ran, and hand back).
- Cargo commands run from `src-tauri/`. Use `--locked` and `--features test-fixtures` where integration tests are involved.
- If blocked or the plan is wrong, report `BLOCKED` with specifics rather than guessing.

---

## Task 1: `CommunityServeAllowlist` type

**Files:**
- Modify: `src-tauri/src/content_store.rs` (add type near the top, after the `use` block / before `ContentStoreError`, around line 17)
- Test: `src-tauri/src/content_store.rs` (`#[cfg(test)] mod tests`, ~line 176)

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `content_store.rs` (after the existing `cid` helper):

```rust
#[test]
fn allowlist_allow_then_contains() {
    let a = CommunityServeAllowlist::new();
    let c = cid(7);
    assert!(!a.contains(&c), "fresh allowlist contains nothing");
    a.allow(c);
    assert!(a.contains(&c), "allowed CID is contained");
    assert!(!a.contains(&cid(8)), "un-added CID is not contained");
}

#[test]
fn allowlist_clone_shares_state() {
    // Arc-backed: a clone observes inserts made via the original.
    let a = CommunityServeAllowlist::new();
    let b = a.clone();
    let c = cid(42);
    a.allow(c);
    assert!(b.contains(&c), "clone shares the underlying set");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib -E 'test(allowlist_)'`
Expected: FAIL to compile — `CommunityServeAllowlist` not found.

- [ ] **Step 3: Add the type**

Insert into `content_store.rs` immediately after the `use std::sync::Mutex;` line (~line 16), before `ContentStoreError`:

```rust
use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use crate::owner_state_types::ContentId;

/// Set of community state-root CIDs this node is willing to serve over CAS even
/// though they carry the `encrypted` flag (ZEB-395). Community roots are
/// epoch-key ciphertext shared among members; serving them by CID is safe (see
/// `docs/specs/2026-06-07-zeb-395-community-content-serve-policy-design.md` §3).
/// Private encrypted blobs (DMs, private profiles) are never inserted, so the
/// content-serve queryable keeps refusing them.
///
/// `std::sync::RwLock` (not tokio) is intentional: `allow`/`contains` lock,
/// mutate/read, and drop the guard synchronously — no guard is ever held across
/// an `.await`. The handle is `Clone` (Arc bump) and shared between the
/// production `RuntimeContentStore` (registration) and the content-serve
/// queryable (lookup).
#[derive(Clone, Default)]
pub struct CommunityServeAllowlist(Arc<RwLock<HashSet<ContentId>>>);

impl CommunityServeAllowlist {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a community-root CID serveable. Idempotent. A poisoned lock is
    /// treated as "could not record" (the publish already succeeded; the next
    /// publish re-registers), never a panic.
    pub fn allow(&self, cid: ContentId) {
        if let Ok(mut g) = self.0.write() {
            g.insert(cid);
        }
    }

    /// True if `cid` is an allowlisted community-root CID. A poisoned lock reads
    /// as "not allowlisted" (fail closed — never serve on a poisoned guard).
    pub fn contains(&self, cid: &ContentId) -> bool {
        self.0.read().map(|g| g.contains(cid)).unwrap_or(false)
    }
}
```

Note: `content_store.rs` currently imports `ContentId` via `use crate::owner_state_types::ContentId;` (line 13) — the `use` line added above is a duplicate import path **only if** line 13 is absent. Line 13 already exists, so do NOT re-add `use crate::owner_state_types::ContentId;`; keep only the `HashSet`/`Arc`/`RwLock` imports. (The existing file uses `std::sync::Mutex` already; add `Arc, RwLock, HashSet` without disturbing it.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib -E 'test(allowlist_)'`
Expected: PASS (2 tests).

- [ ] **Step 5: Lint + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked -p harmony-app --lib --no-deps -- -D warnings
git add src-tauri/src/content_store.rs
git commit -m "feat(zeb-395): CommunityServeAllowlist type (Arc-shared serve set)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```
Expected: clippy clean; commit succeeds.

---

## Task 2: `put_serveable` trait method + `RuntimeContentStore` registration

**Files:**
- Modify: `src-tauri/src/content_store.rs`
  - `ContentStore` trait (~line 24-28): add `put_serveable` default method.
  - `RuntimeContentStore` struct (~line 115-118): add `serve_allowlist` field.
  - `RuntimeContentStore::new` (~line 120-130): initialize field to `None`.
  - `impl RuntimeContentStore` (add `with_serve_allowlist` builder).
  - `impl ContentStore for RuntimeContentStore` (~line 132-163): override `put_serveable`.
- Test: `src-tauri/src/content_store.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to `#[cfg(test)] mod tests`:

```rust
#[tokio::test]
async fn put_serveable_registers_cid_in_allowlist() {
    // RuntimeContentStore.with_serve_allowlist: put_serveable admits AND records
    // the CID; plain put admits but does NOT record.
    let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
    let allowlist = CommunityServeAllowlist::new();
    let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500))
        .with_serve_allowlist(allowlist.clone());

    // Stub receiver: ack every PutLocal reply so put()/put_serveable() return Ok.
    let stub = tokio::spawn(async move {
        while let Some(op) = cas_op_rx.recv().await {
            if let CasOp::PutLocal {
                reply: Some(reply), ..
            } = op
            {
                let _ = reply.send(Ok(()));
            }
        }
    });

    let served = ContentId::from_bytes([0x11; 32]);
    let private = ContentId::from_bytes([0x22; 32]);
    store.put_serveable(served, vec![1, 2, 3]).await.unwrap();
    store.put(private, vec![4, 5, 6]).await.unwrap();

    assert!(allowlist.contains(&served), "put_serveable registers the CID");
    assert!(
        !allowlist.contains(&private),
        "plain put does NOT register the CID"
    );
    drop(store);
    stub.await.unwrap();
}

#[tokio::test]
async fn put_serveable_default_impl_routes_to_put() {
    // The default trait impl (InMemoryStub) routes put_serveable to put with no
    // allowlist concept and no panic.
    let store = InMemoryStub::default();
    store
        .put_serveable(ContentId::from_bytes([9; 32]), vec![7, 8])
        .await
        .unwrap();
    let got = store
        .get(&ContentId::from_bytes([9; 32]))
        .await
        .unwrap()
        .expect("blob present");
    assert_eq!(got, vec![7, 8]);
}

#[tokio::test]
async fn put_serveable_without_allowlist_is_just_put() {
    // RuntimeContentStore with no allowlist set: put_serveable behaves like put.
    let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
    let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500));
    let stub = tokio::spawn(async move {
        if let Some(CasOp::PutLocal {
            reply: Some(reply), ..
        }) = cas_op_rx.recv().await
        {
            let _ = reply.send(Ok(()));
        }
    });
    store
        .put_serveable(ContentId::from_bytes([3; 32]), vec![1])
        .await
        .unwrap();
    stub.await.unwrap();
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib -E 'test(put_serveable)'`
Expected: FAIL to compile — `put_serveable` / `with_serve_allowlist` not found.

- [ ] **Step 3: Add the trait default method**

In `pub trait ContentStore` (after the `get` method, before the closing `}` ~line 27):

```rust
    /// Like `put`, but also marks `cid` serveable to peers over CAS even though
    /// it carries the `encrypted` flag (ZEB-395 community-root sharing). The
    /// default impl is identical to `put`; only `RuntimeContentStore` registers
    /// the CID in its shared `CommunityServeAllowlist`. Callers use this ONLY
    /// for content that is safe to serve to any requester who can name the CID
    /// (community state-root ciphertext) — never for private blobs.
    async fn put_serveable(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError> {
        self.put(cid, blob).await
    }
```

- [ ] **Step 4: Add the field + builder + override**

Change the `RuntimeContentStore` struct (~line 115):

```rust
pub struct RuntimeContentStore {
    cas_op_tx: tokio::sync::mpsc::Sender<CasOp>,
    fetch_timeout: std::time::Duration,
    /// ZEB-395: when set, `put_serveable` records the put CID here so the
    /// content-serve queryable will serve it despite the `encrypted` flag.
    /// `None` for the legacy/test constructions that don't serve community
    /// roots. Shared (Arc clone) with `event_loop::run`'s serve queryable.
    serve_allowlist: Option<CommunityServeAllowlist>,
}
```

Change `RuntimeContentStore::new` to initialize the field (~line 120):

```rust
    pub fn new(
        cas_op_tx: tokio::sync::mpsc::Sender<CasOp>,
        fetch_timeout: std::time::Duration,
    ) -> Self {
        Self {
            cas_op_tx,
            fetch_timeout,
            serve_allowlist: None,
        }
    }

    /// ZEB-395: attach the shared serve-allowlist so `put_serveable` registers
    /// community-root CIDs. Chained builder so the ~10 existing
    /// `RuntimeContentStore::new(...)` call sites stay untouched.
    pub fn with_serve_allowlist(mut self, allowlist: CommunityServeAllowlist) -> Self {
        self.serve_allowlist = Some(allowlist);
        self
    }
```

In `impl ContentStore for RuntimeContentStore` (after the `get` method, ~line 162), add the override:

```rust
    async fn put_serveable(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError> {
        // Admit first; only record as serveable after a successful put.
        self.put(cid, blob).await?;
        if let Some(allowlist) = &self.serve_allowlist {
            allowlist.allow(cid);
        }
        Ok(())
    }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib -E 'test(put_serveable)'`
Expected: PASS (3 tests).

- [ ] **Step 6: Lint + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked -p harmony-app --lib --no-deps -- -D warnings
git add src-tauri/src/content_store.rs
git commit -m "feat(zeb-395): put_serveable trait method + RuntimeContentStore registration

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```
Expected: clippy clean; commit succeeds.

---

## Task 3: serve-queryable gate consults the allowlist

**Files:**
- Modify: `src-tauri/src/event_loop.rs`
  - `spawn_content_serve_queryable` signature (~line 6713) + doc comment (~line 6704-6711) + gate (~line 6756).
  - The production caller at ~line 1880 — pass a temporary empty allowlist (Task 4 wires the real one). Mark with a `// ZEB-395 Task 4` comment.
  - Serve-gate unit test in a `#[cfg(test)]` block.
- Modify (pass empty allowlist to the new param):
  - `src-tauri/tests/cas_serve_two_node_integration.rs:44, 115`
  - `src-tauri/tests/profile_card_avatar_cross_peer_integration.rs:71`
  - `src-tauri/tests/profile_page_cross_peer_integration.rs:91, 175`
- Create: `src-tauri/tests/community_serve_allowlist_integration.rs`

- [ ] **Step 1: Write the failing regression test**

Create `src-tauri/tests/community_serve_allowlist_integration.rs`:

```rust
//! ZEB-395 regression: the content-serve queryable serves an ENCRYPTED CID iff
//! it is in the CommunityServeAllowlist. This is the case the existing
//! community-sync test (shared CAS) cannot exercise: separate stores reachable
//! only over the serve queryable. Models on cas_serve_two_node_integration.rs.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use harmony_app::content_store::CommunityServeAllowlist;
use harmony_app::event_loop::spawn_content_serve_queryable;
use harmony_content::cid::{ContentFlags, ContentId};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serves_allowlisted_encrypted_cid_but_not_others() {
    tokio::time::timeout(Duration::from_secs(30), inner())
        .await
        .expect("allowlist serve test must complete within 30s");
}

async fn inner() {
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B"));

    // Public control CID (liveness proof) + an allowlisted encrypted CID
    // (must serve) + a non-allowlisted encrypted CID (must NOT serve).
    let pub_blob = b"public-control".to_vec();
    let pub_cid = ContentId::for_book(&pub_blob, ContentFlags::default()).expect("public cid");

    let enc_flags = ContentFlags {
        encrypted: true,
        ..ContentFlags::default()
    };
    let allowed_blob = b"community-root-ciphertext".to_vec();
    let allowed_cid = ContentId::for_book(&allowed_blob, enc_flags).expect("allowed enc cid");
    let denied_blob = b"private-dm-ciphertext".to_vec();
    let denied_cid = ContentId::for_book(&denied_blob, enc_flags).expect("denied enc cid");

    let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
    store.insert(pub_cid, pub_blob.clone());
    store.insert(allowed_cid, allowed_blob.clone());
    store.insert(denied_cid, denied_blob.clone());
    let store = Arc::new(store);

    let lookup = {
        let store = Arc::clone(&store);
        Arc::new(move |cid: ContentId| {
            let store = Arc::clone(&store);
            Box::pin(async move { store.get(&cid).cloned() })
                as std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send>>
        })
    };

    let allowlist = CommunityServeAllowlist::new();
    allowlist.allow(allowed_cid); // only this encrypted CID is serveable

    let closing = Arc::new(AtomicBool::new(false));
    let _serve = spawn_content_serve_queryable(
        Arc::clone(&session_a),
        lookup,
        Arc::clone(&closing),
        allowlist,
    )
    .await
    .expect("declare content-serve queryable");

    let key_for = |c: &ContentId| {
        let hex = hex::encode(c.to_bytes());
        format!("harmony/content/{}/{}", &hex[1..2], hex)
    };

    // --- Step 1: liveness via public control CID ---
    let pub_key = key_for(&pub_cid);
    let mut pub_got: Option<Vec<u8>> = None;
    for _ in 0..60 {
        let replies = session_b.get(&pub_key).await.expect("get public");
        while let Ok(reply) = replies.recv_async().await {
            if let Ok(sample) = reply.result() {
                pub_got = Some(sample.payload().to_bytes().to_vec());
                break;
            }
        }
        if pub_got.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(
        pub_got.as_deref(),
        Some(pub_blob.as_slice()),
        "public control CID must serve (liveness)"
    );

    // --- Step 2: the allowlisted encrypted CID MUST serve ---
    let allowed_key = key_for(&allowed_cid);
    let mut allowed_got: Option<Vec<u8>> = None;
    for _ in 0..60 {
        let replies = session_b.get(&allowed_key).await.expect("get allowed enc");
        while let Ok(reply) = replies.recv_async().await {
            if let Ok(sample) = reply.result() {
                allowed_got = Some(sample.payload().to_bytes().to_vec());
                break;
            }
        }
        if allowed_got.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(
        allowed_got.as_deref(),
        Some(allowed_blob.as_slice()),
        "allowlisted encrypted CID must be served"
    );

    // --- Step 3: the non-allowlisted encrypted CID MUST NOT serve ---
    let denied_key = key_for(&denied_cid);
    let replies = session_b.get(&denied_key).await.expect("get denied enc");
    let served_flag = Arc::new(AtomicBool::new(false));
    let served_flag2 = Arc::clone(&served_flag);
    let _ = tokio::time::timeout(Duration::from_secs(3), async move {
        while let Ok(reply) = replies.recv_async().await {
            if reply.result().is_ok() {
                served_flag2.store(true, Ordering::SeqCst);
            }
        }
    })
    .await;
    assert!(
        !served_flag.load(Ordering::SeqCst),
        "non-allowlisted encrypted CID must NOT be served"
    );

    closing.store(true, Ordering::SeqCst);
}
```

- [ ] **Step 2: Run to verify it fails (compile)**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures --test community_serve_allowlist_integration`
Expected: FAIL to compile — `spawn_content_serve_queryable` takes 3 args, got 4.

- [ ] **Step 3: Add the param + gate change**

In `event_loop.rs`, update the doc comment on `spawn_content_serve_queryable` (~lines 6704-6706) — replace the "Serve gate" paragraph with:

```rust
/// Serve gate (ZEB-395): a CID is servable iff it is unencrypted OR it is an
/// allowlisted community-root CID (`serve_allowlist.contains`). Private
/// encrypted blobs (DMs, private profiles) are never allowlisted, so they keep
/// getting no reply. The encrypted flag is intrinsic to the CID header; the
/// allowlist is the publisher's explicit opt-in via `ContentStore::put_serveable`.
```

Change the signature (~line 6713) to add the trailing param:

```rust
pub async fn spawn_content_serve_queryable<F>(
    session: Arc<zenoh::Session>,
    lookup: Arc<F>,
    closing: Arc<AtomicBool>,
    serve_allowlist: crate::content_store::CommunityServeAllowlist,
) -> Result<tokio::task::JoinHandle<()>, String>
```

`serve_allowlist` is `Clone` and is moved into the spawned task with the other captures (it appears inside the `async move` block via the gate; no extra clone needed since `lookup`/`closing` are already moved — add `serve_allowlist` to the move by referencing it in the gate).

Change the gate (~line 6756) from:

```rust
                    if cid.flags().encrypted {
                        continue;
                    }
```
to:

```rust
                    if cid.flags().encrypted && !serve_allowlist.contains(&cid) {
                        continue; // private encrypted content stays unservable
                    }
```

- [ ] **Step 4: Update the production caller (temporary empty) + all test callers**

In `event_loop.rs` ~line 1880, change the production call to pass an empty allowlist for now:

```rust
        let _serve_handle = match spawn_content_serve_queryable(
            std::sync::Arc::clone(&session_arc),
            serve_lookup,
            std::sync::Arc::clone(&closing),
            // ZEB-395 Task 4 replaces this with the run()-level shared allowlist.
            crate::content_store::CommunityServeAllowlist::new(),
        )
```

In each test file, add the import and the 4th argument:

`tests/cas_serve_two_node_integration.rs` — after line 12 add:
```rust
use harmony_app::content_store::CommunityServeAllowlist;
```
and at lines 44 and 115 change the call to end with `, CommunityServeAllowlist::new())`:
```rust
        spawn_content_serve_queryable(
            Arc::clone(&session_a),
            lookup,
            Arc::clone(&closing),
            CommunityServeAllowlist::new(),
        )
```

`tests/profile_card_avatar_cross_peer_integration.rs` — after line 13 add `use harmony_app::content_store::CommunityServeAllowlist;`; at line 71 add the same 4th arg.

`tests/profile_page_cross_peer_integration.rs` — after line 23 add `use harmony_app::content_store::CommunityServeAllowlist;`; at lines 91 and 175 add the same 4th arg.

- [ ] **Step 5: Add the serve-gate unit test**

Append a focused unit test inside `event_loop.rs`. Put it in the existing `#[cfg(test)] mod content_serve_parse_tests` (~line 6805) or a new `#[cfg(test)] mod content_serve_gate_tests`. It tests the gate predicate directly (no zenoh), mirroring the production condition:

```rust
#[cfg(test)]
mod content_serve_gate_tests {
    use crate::content_store::CommunityServeAllowlist;
    use harmony_content::cid::{ContentFlags, ContentId};

    /// The exact predicate used in the serve loop: serve iff unencrypted OR
    /// allowlisted. Kept in lockstep with spawn_content_serve_queryable's gate.
    fn servable(cid: &ContentId, allowlist: &CommunityServeAllowlist) -> bool {
        !(cid.flags().encrypted && !allowlist.contains(cid))
    }

    #[test]
    fn gate_serves_unencrypted_always() {
        let cid = ContentId::for_book(b"pub", ContentFlags::default()).unwrap();
        let allow = CommunityServeAllowlist::new();
        assert!(servable(&cid, &allow));
    }

    #[test]
    fn gate_refuses_encrypted_unless_allowlisted() {
        let enc = ContentFlags {
            encrypted: true,
            ..ContentFlags::default()
        };
        let cid = ContentId::for_book(b"sec", enc).unwrap();
        let allow = CommunityServeAllowlist::new();
        assert!(!servable(&cid, &allow), "encrypted + not allowlisted => refuse");
        allow.allow(cid);
        assert!(servable(&cid, &allow), "encrypted + allowlisted => serve");
    }
}
```

- [ ] **Step 6: Run tests to verify they pass**

```bash
cd src-tauri && cargo nextest run --locked -p harmony-app --lib -E 'test(content_serve_gate)'
cargo nextest run --locked --features test-fixtures --test community_serve_allowlist_integration
cargo nextest run --locked --features test-fixtures --test cas_serve_two_node_integration
```
Expected: all PASS (gate unit tests; new regression: 1; existing two-node serve + encrypted-gate: 2).

- [ ] **Step 7: Lint + commit**

```bash
cd src-tauri && cargo fmt --all
cargo clippy --locked -p harmony-app --lib --no-deps -- -D warnings
cargo clippy --locked --features test-fixtures --test community_serve_allowlist_integration --test cas_serve_two_node_integration --test profile_card_avatar_cross_peer_integration --test profile_page_cross_peer_integration --no-deps -- -D warnings
git add src-tauri/src/event_loop.rs src-tauri/tests/community_serve_allowlist_integration.rs src-tauri/tests/cas_serve_two_node_integration.rs src-tauri/tests/profile_card_avatar_cross_peer_integration.rs src-tauri/tests/profile_page_cross_peer_integration.rs
git commit -m "feat(zeb-395): content-serve queryable consults serve-allowlist + regression test

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```
Expected: clippy clean; commit succeeds.

---

## Task 4: production wiring — share the allowlist and trigger registration

**Files:**
- Modify: `src-tauri/src/event_loop.rs`
  - `run()` signature (~line 403-460): add a trailing `serve_allowlist: crate::content_store::CommunityServeAllowlist` param.
  - The production caller at ~line 1880: replace the temporary `CommunityServeAllowlist::new()` with the `serve_allowlist` param.
- Modify: `src-tauri/src/lib.rs`
  - `start_node` (~line 2798): create the allowlist; `.with_serve_allowlist(...)` on the production `RuntimeContentStore`.
  - Add a `serve_allowlist_for_loop` clone alongside the other `*_for_loop`/`*_into_loop` bindings; pass it as the final argument to `event_loop::run` (~line 5174).
- Modify: `src-tauri/src/community_state_sync.rs`
  - `publish_root_now` (~line 2602): `put` → `put_serveable`.

- [ ] **Step 1: Thread the allowlist through `run()` and the production caller**

In `event_loop.rs`, add to the END of `run()`'s parameter list (after `dial_telemetry_into_loop` — confirm the last param name by reading the signature; add a trailing comma then the new param):

```rust
    // ZEB-395: shared serve-allowlist. The same handle is attached to the
    // production RuntimeContentStore (so publish_root_now's put_serveable
    // registers community-root CIDs) and consulted by the content-serve
    // queryable below. Empty for any caller that doesn't publish community roots.
    serve_allowlist: crate::content_store::CommunityServeAllowlist,
```

At ~line 1880, replace the temporary empty allowlist from Task 3 with the param:

```rust
        let _serve_handle = match spawn_content_serve_queryable(
            std::sync::Arc::clone(&session_arc),
            serve_lookup,
            std::sync::Arc::clone(&closing),
            serve_allowlist.clone(),
        )
```

- [ ] **Step 2: Wire lib.rs — create the allowlist, attach to the store, pass to run()**

In `lib.rs`, immediately before the `content_store` construction (~line 2798) insert:

```rust
                    // ZEB-395: one shared serve-allowlist for this node. Attached
                    // to the production content store (registration via
                    // put_serveable) and to event_loop::run's serve queryable
                    // (lookup). Same Arc-backed handle on both sides.
                    let serve_allowlist =
                        crate::content_store::CommunityServeAllowlist::new();
```

Change the `content_store` construction (~line 2798-2804) to attach it:

```rust
                    let content_store: std::sync::Arc<dyn crate::content_store::ContentStore> =
                        std::sync::Arc::new(
                            crate::content_store::RuntimeContentStore::new(
                                cas_op_tx.clone(),
                                std::time::Duration::from_millis(
                                    crate::content_store::DEFAULT_FETCH_TIMEOUT_MS,
                                ),
                            )
                            .with_serve_allowlist(serve_allowlist.clone()),
                        );
```

Find where the other `*_for_loop` / `*_into_loop` bindings are created (search for `let dial_telemetry_into_loop =`) and add adjacent to them:

```rust
                let serve_allowlist_for_loop = serve_allowlist.clone();
```

Add `serve_allowlist_for_loop,` as the FINAL argument of the `event_loop::run(...)` call (after `dial_telemetry_into_loop,` at ~line 5174):

```rust
                                dial_telemetry_into_loop,
                                serve_allowlist_for_loop,
                            )
```

> Note on scope: `serve_allowlist` is created in the same `start_node` block that builds `content_store` (~2798) and later spawns the runtime thread that calls `event_loop::run` (~5128). If `serve_allowlist` is not in scope at the `let serve_allowlist_for_loop = …` site (e.g., it lives inside a narrower block), hoist the `let serve_allowlist = …` binding up to the same scope level as `content_store` / the other `*_for_loop` source bindings. Do NOT create a second independent allowlist — both sides MUST share one instance.

- [ ] **Step 3: Trigger registration in `publish_root_now`**

In `community_state_sync.rs` (~line 2601-2602), change:

```rust
    // 4. Put into ContentStore (routes through CasOp::PutLocal).
    ctx.content_store.put(root_cid, blob_ciphertext).await?;
```
to:

```rust
    // 4. Put into ContentStore AND mark this community-root CID serveable to
    //    peers (ZEB-395). put_serveable admits via CasOp::PutLocal exactly like
    //    put, then (production RuntimeContentStore only) records root_cid in the
    //    shared serve-allowlist so the content-serve queryable will serve it
    //    despite the encrypted flag. Registration completes before the state-
    //    root envelope announcing root_cid is published below, so no peer can
    //    request the CID before it is allowlisted.
    ctx.content_store
        .put_serveable(root_cid, blob_ciphertext)
        .await?;
```

- [ ] **Step 4: Verify it compiles and existing tests stay green**

```bash
cd src-tauri && cargo nextest run --locked -p harmony-app --lib
cargo nextest run --locked --features test-fixtures --test community_sync_integration
cargo nextest run --locked --features test-fixtures --test community_serve_allowlist_integration
```
Expected: lib tests PASS; `community_sync_integration` PASS (InMemoryStub's `put_serveable` == `put`, so behavior is unchanged); the regression PASS. If `community_sync_integration` is heavy/slow, allow up to the 10-min budget; if it exceeds, commit and report `DONE_WITH_CONCERNS` noting which test was still running.

- [ ] **Step 5: Lint + commit**

```bash
cd src-tauri && cargo fmt --all
cargo clippy --locked -p harmony-app --lib --no-deps -- -D warnings
git add src-tauri/src/event_loop.rs src-tauri/src/lib.rs src-tauri/src/community_state_sync.rs
git commit -m "feat(zeb-395): wire shared serve-allowlist into start_node + publish_root_now

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```
Expected: clippy clean; commit succeeds.

---

## Task 5: final gate sweep + push + PR

**Files:** none (verification + publish only).

- [ ] **Step 1: Full workspace gates (the CI mirror)**

Run from `src-tauri/` (each with the 10-min wall-clock discipline; these are the slow ones — run sequentially, commit is already done so nothing is at risk):

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: fmt clean; clippy 0 warnings; nextest all green except known iroh/zenoh transport orphan-flakes (per project memory — those are environmental first-bind flakes, not introduced here; re-run the specific flaky test once to confirm it's a flake, and note it).

> If clippy/nextest `--all-targets` exceeds the time budget on this machine (lib changes relink ~97 integration binaries), it is acceptable to rely on the per-task scoped gates already run (lib + the touched integration tests) plus CI for the full sweep. In that case push and let CI run the full matrix; note in the PR that the local full sweep was time-bounded.

- [ ] **Step 2: Frontend (untouched — sanity only)**

```bash
cd .. && npx tsc --noEmit
```
Expected: PASS (no frontend files changed). Skip `vitest` (no FE changes).

- [ ] **Step 3: Strip any diagnostic instrumentation**

Confirm no leftover diagnostics from earlier debugging are committed:
```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git diff main --stat
```
The diff should be limited to: `content_store.rs`, `event_loop.rs`, `lib.rs`, `community_state_sync.rs`, the new test, the 3 updated tests, and the two `docs/` files. No `tracing` diagnostics, no `/tmp` patches applied. (The live re-test instrumentation in `/tmp/zeb-395-diagnostic-instrumentation.patch` is applied ONLY for the §8 manual re-test and is NEVER committed.)

- [ ] **Step 4: Push + open PR**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git push -u origin zeb-395-community-content-serve
gh pr create --title "ZEB-395: community content serve policy — serve encrypted community-root CIDs to members" --body "$(cat <<'EOF'
## Summary
Cross-machine community sync (Koya ↔ Ildwyn) failed because the ZEB-343 CAS content-serve gate refuses **all** encrypted CIDs, but community state-root blobs are epoch-key ciphertext shared among members. Neither side would serve the other's community root, so the CRDT never transferred (`ErrPreMutation(ContentStore(Io("no successful reply")))`, both directions). Root-caused with Koya-side instrumentation during a live redeem (community `5bbfe67d…`, node `39753b0b…`).

This adds a per-node **serve-allowlist** of community-root CIDs. A root CID is allowlisted when `publish_root_now` publishes it (via a new `ContentStore::put_serveable`), and the content-serve queryable serves an encrypted CID only if it is allowlisted. Private encrypted blobs (DMs, private profiles) use plain `put` and stay refused.

**Why it's safe (v1):** the blob is epoch-key ciphertext (useless to non-members), and the root CID is only learnable by decrypting a member-only state-root publish — so serving "by allowlisted CID" is implicitly member-gated. Membership-authenticated serve is the documented approach-2 hardening follow-up.

**Plumbing note:** the allowlist is an `Arc<RwLock<HashSet<ContentId>>>` shared by clone between the production `RuntimeContentStore` (registration) and `event_loop::run`'s serve queryable (lookup). This deliberately avoids adding fields to the community config structs / `CasOp` (which would have forced ~43 + ~25 mechanical test-site edits) — behavior-identical to the approved design, far smaller diff.

## Changes
- `content_store.rs`: `CommunityServeAllowlist` type; `put_serveable` trait method (default == `put`); `RuntimeContentStore` optional allowlist field + `with_serve_allowlist` builder + override.
- `event_loop.rs`: `spawn_content_serve_queryable` + `run()` gain a `serve_allowlist` param; gate consults it.
- `lib.rs`: create one allowlist in `start_node`, attach to the store, pass to `run()`.
- `community_state_sync.rs`: `publish_root_now` uses `put_serveable`.
- Tests: new cross-store serve regression (`community_serve_allowlist_integration.rs`) — the case the shared-CAS community-sync test cannot exercise; serve-gate + allowlist + `put_serveable` unit tests.

## Why tests missed the original bug
`community_sync_integration.rs` wires both engines to a **shared** CAS (`spawn_shared_cas`), so `content_store.get(root_cid)` resolves locally and never traverses the serve queryable's encrypted gate. The new regression uses **separate** stores routed through the queryable.

## Test plan
- [ ] CI: `rust-check` (fmt + clippy `-D warnings`), `rust-test` (nextest), `msrv`, `frontend` all green
- [ ] New `community_serve_allowlist_integration` passes (allowlisted encrypted CID served; non-allowlisted encrypted refused; public control liveness)
- [ ] Existing `cas_serve_two_node_integration` (incl. `does_not_serve_encrypted_cid`) still green
- [ ] Two-machine live re-test (Koya mints invite-only, Ildwyn redeems): Koya logs `content-serve HIT` for the root CID, outcome `Applied` (not `ErrPreMutation`); Ildwyn shows `channels=[#general]`, `members=2` → closes ZEB-366 / ZEB-330 DoD#3

Spec: `docs/specs/2026-06-07-zeb-395-community-content-serve-policy-design.md`
Plan: `docs/plans/2026-06-07-zeb-395-community-content-serve-policy-plan.md`
Blocks: ZEB-330 (cross-WAN first-contact) / ZEB-366.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 5: Report PR URL** and hand off to the autonomous bot-review loop.

---

## Self-Review (plan vs spec)

- **Spec §3 approach (serve-allowlist):** T1 (type) + T2 (`put_serveable` registration) + T3 (gate consults it) + T4 (publish triggers it). ✓
- **Spec §4.1 type in content_store.rs:** T1. ✓
- **Spec §4.2 registration via `put_serveable` (default == `put`, RuntimeContentStore overrides):** T2 + T4 (publish_root_now swap). ✓
- **Spec §4.3 Arc-shared, no config changes:** T2 builder + T4 lib.rs single-instance wiring; no `CommunityRegistryConfig`/`CommunitySyncEngineConfig`/`InternalCtx`/`CasOp` edits. ✓
- **Spec §4.4 gate change + new queryable param:** T3. ✓
- **Spec §6 testing:** §6.1 allowlist unit (T1), §6.2 `put_serveable` unit (T2), §6.3 serve-gate unit (T3), §6.4 cross-store regression (T3); §6.5 deferred to §8 live re-test (documented). ✓
- **Spec §8 re-test:** referenced in T5 PR test plan; the diagnostic patch is applied manually post-merge-prep, never committed (T5 Step 3). ✓
- **Type consistency:** `CommunityServeAllowlist` (`new`/`allow`/`contains`/`Clone`/`Default`), `put_serveable`, `with_serve_allowlist` used identically across T1-T4. `ContentId` is the single re-exported type (`owner_state_types` re-exports `harmony_content::cid::ContentId`), so `HashSet<ContentId>` accepts CIDs minted via either import path. ✓
- **No placeholders:** every code step shows complete code; every command shows expected output. ✓
