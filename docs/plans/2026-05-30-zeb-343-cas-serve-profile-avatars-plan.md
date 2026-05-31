# ZEB-343: peer-to-peer CAS-serve primitive + profile avatars — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the missing peer-to-peer CAS *serve* half (a content-serve Zenoh queryable + verify-on-fetch), prove it with a two-node fetch-by-CID test, then ship profile avatars as its first consumer — all in one PR.

**Architecture:** A node already *stores* (`CasOp::PutLocal` → `StorageTier`) and *fetches* (`fetch_via_zenoh` GET) content, but no node *answers* content GETs, so every fetch times out. We add a single content-serve queryable on `harmony/content/*/**` that, per query, parses the CID, gates on the CID's own `encrypted` bit (`!cid.flags().encrypted` ⇒ public ⇒ servable; this is intrinsic + per-chunk, no registry), looks up the bytes in the local `StorageTier` cache via a new read-only `CasOp::GetLocal`, and replies inline — mirroring the existing channel-log queryable. Bytes returned by a fetch are verified `hash==CID` before use (the cache already verifies on admit, but the *returned* bytes were not checked). Avatars ride on the ZEB-341 `ProfileCardBroadcast` via a new optional `avatar_cid` field, ingested as default-flags (`PublicDurable`) CAS bytes and rendered through the existing `avatar-resolver.ts` / `Avatar.svelte`.

**Tech Stack:** Rust (Tauri backend, `harmony-content` CAS crate, Zenoh queryables), Svelte 5 runes frontend, canonical CBOR (`ciborium`), Ed25519 (`ed25519-dalek`).

**Spec:** `docs/specs/2026-05-31-cas-serve-primitive-and-profile-avatars-design.md` (commit `65d3484`, linked `44a445a`).
**Branch:** `zeb-343-cas-serve-profile-avatars` off `c82416a8` (merged main, includes ZEB-341 PR #171). Working tree must stay on this branch — **never a worktree** (use `git checkout -b` in the main repo only).
**Linear:** ZEB-343 (related to ZEB-341).

---

## Hard rules baked into EVERY implementer task

These mirror ZEB-338/339 discipline. Enforce on every dispatch:

- **5 backend gates** (run from `src-tauri/`):
  1. `cargo fmt --all -- --check`
  2. `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  3. `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (a scoped `-E` subset is OK per-task; the **full** sweep runs in T14)
  4. `HARMONY_LARGE_TESTS=1 cargo nextest run --locked --features test-fixtures -E 'test(folder_ingest_walker)'` (T14 only)
  5. MSRV: `cargo check --locked --all-targets --features test-fixtures` (T14 only)
- **2 frontend gates** (run from repo root, via `npx` NOT `pnpm`):
  1. `npx tsc --noEmit`
  2. `npx vitest run` (scope per-task; full in T14)
- **COMMIT BEFORE the long gate.** `timeout`/`gtimeout` are NOT on macOS — rely on the Bash tool's own 600000ms timeout. If a gate exceeds ~10 min wall-clock → report `DONE_WITH_CONCERNS`, do NOT silently stall.
- **Pipe exit codes lie** (zsh): never trust `cmd | tail`. Use `set -o pipefail`, or check `$pipestatus[1]` (zsh is 1-indexed; `${PIPESTATUS[0]}` returns empty in zsh), or capture `echo "EXIT=$?"` on its own line.
- **Backend clippy/test compiles the workspace (~10-11 min).** Commit first; expect the wait.
- **Canonical CBOR:** new serde field codes are 2 chars; `ciborium` emits struct fields in **declaration order** (NOT sorted) — placement is deterministic but does not need alphabetical sorting. `skip_serializing_if = "Option::is_none"` keeps the no-avatar wire byte-identical to ZEB-341.
- **TDD per task:** failing test first → red → minimal impl → green → commit. Each task ends with a commit.
- **NEVER trigger Greptile** (paid, manual-only; Jake triggers it). **NEVER merge** (Jake's gate).
- macOS XprotectService one-time setup must be active or cold cargo builds appear to hang (`spctl developer-mode enable-terminal` + Developer Tools toggle).

---

## File Structure

**Backend — create:**
- `src-tauri/tests/cas_serve_two_node_integration.rs` — the Phase 0 proof (two-session Zenoh fetch-by-CID) + encrypted-gate negative.
- `src-tauri/tests/wire_format_profile_card_avatar_fixtures.rs` — avatar / no-avatar wire-format pins (or extend the existing `wire_format_profile_card_fixtures.rs`).

**Backend — modify:**
- `src-tauri/src/content_store.rs` — add `CasOp::GetLocal`.
- `src-tauri/src/event_loop.rs` — add `spawn_content_serve_queryable`; handle `CasOp::GetLocal`; verify-on-fetch in `wrap_fetch_one_with_admission`; spawn the serve queryable in `run()`; add `avatarCid` to the `member-card-received` event.
- `src-tauri/src/profile_card_broadcast.rs` — `avatar_cid` field; `sign_card` param; `CachedCard` + `DiscoveredCardInfo` + `get_cached` + `insert_verified`; `publish_card_once`.
- `src-tauri/src/lib.rs` — `publish_owner_card` / `publish_profile` / `republish_owner_card` threading; new `ingest_avatar_bytes` IPC; invoke-handler registration.
- `src-tauri/tests/wire_format_profile_card_fixtures.rs` — update existing literals for the new field.

**Frontend — create:**
- `src/lib/avatar-normalize.ts` — canvas 256² PNG normalizer.
- `src/lib/__tests__/avatar-normalize.test.ts`, `src/lib/__tests__/member-card-service.avatar.test.ts`.

**Frontend — modify:**
- `src/lib/member-card-service.ts` — `avatarUrl`/`avatarCid` on cards; `AvatarResolver` wiring.
- `src/lib/components/{MemberRow,ChannelMessageFeed,ProfilePopover}.svelte` — render `<Avatar avatarUrl>`.
- `src/lib/components/ProfileEditor.svelte` — avatar upload control.
- `src/App.svelte` — `republishOwnerCard` avatarCid; share `AvatarResolver` with `memberCardService`; `seedSelf` avatarUrl.
- Type files (`src/lib/types.ts` / popover `OwnerCard` type) — `avatarUrl` on the owner-card shape.

---

## Validated facts (load-bearing; from code as of `44a445a`)

These were confirmed by reading the actual source. Implementers MAY re-read the cited files.

1. **CID encodes encryption in its leading bit.** `harmony-content/src/cid.rs`: `ContentId` = 4-byte header + 28-byte hash; `ContentFlags::from_bits` (`cid.rs:74`): `encrypted: byte & 0x80 != 0`. `ContentId::flags()` (`cid.rs:125`), `content_class()` (`cid.rs:129`). `ContentId::verify_hash(&self, data) -> bool` (`cid.rs:222`, **pub**), `verify_checksum()` (`cid.rs:202`, **pub**), `from_bytes([u8;32])` (`cid.rs:369`, **pub**), `to_bytes() -> [u8;32]` (`cid.rs:362`, **pub**). `ContentFlags::default()` = all-false = `PublicDurable` (unencrypted).
2. **StorageTier already verifies `hash==cid` on admit** (`storage_tier.rs:1074` `handle_publish` → `Self::verify_cid` → `cid.verify_hash` + `verify_checksum`). So the cache only holds verified bytes; serve-from-cache is inherently integrity-safe. The gap is the *returned* bytes (§ verify-on-fetch, T4).
3. **Content key patterns:** `harmony_content::zenoh_bridge::content_queryable_key_exprs()` → 16 shard patterns `harmony/content/{0..f}/**`. The GET key is `harmony/content/{2nd-hex-char-of-cid}/{cid_hex}` (`event_loop.rs:2030-2031`).
4. **`streaming_ingest`** (`lib.rs:179`) is generic over `R: tokio::io::AsyncRead + Unpin`, uses `ContentFlags::default()` everywhere, returns `(ContentId, u64)`, and admits all leaves+bundles to StorageTier itself. `ChunkerConfig::DEFAULT`. `send_ingest_with_name` (`lib.rs:8336`) is pure sidecar insertion — **skip it for avatars**.
5. **`Option<[u8;N]>` bstr serde helpers already exist:** `owner_state_types::serialize_optional_bytes_as_bstr` / `deserialize_optional_bytes_from_bstr` (used by `LibraryDirectoryEntry`, `library_directory.rs:81-101`). Reuse them — do **not** write a new module.
6. **`ProfilePayload` already carries `avatar_cid: Option<String>` (hex)** (`lib.rs:1038-1054`); frontend `Profile` already has `avatarCid`/`avatarUrl`. The CID is already plumbed for the DM/Reticulum path; ZEB-343 connects it to the owner *card*.
7. **`cas_op_tx` is event-loop-internal** (param `event_loop.rs:349`), not on `NodeState`. The serve queryable is spawned inside `run()` where `cas_op_tx` + `session_arc` are in scope (~`event_loop.rs:1271-1310`, alongside the card subscriber-pool spawn).
8. **Two-session Zenoh test harness:** `let cfg = zenoh::Config::default(); session_a = zenoh::open(cfg.clone()); session_b = zenoh::open(cfg);` — they discover each other via in-process gossip (`community_channel_messages_integration.rs:199-201`). Replies drained with a retry loop under an outer `tokio::time::timeout`.
9. **Channel-log queryable template** (`event_loop.rs:4214-4289` body; `spawn_channel_log_zenoh_adapter` at `4064`): declare queryable → `tokio::select!` loop on `qbl.recv_async()` → parse key → look up → `query.reply(query.key_expr(), bytes).await`, with a `closing` `AtomicBool` + 1s sleep arm.

---

## Task Sequence (15 tasks)

Phases map to spec §9: **T0** baseline · **T1–T3 = Phase 0** (serve + proof, HARD GATE) · **T4–T5 = Phase 1** (harden) · **T6–T8 = Phase 2** (card field) · **T9–T10, T13 = Phase 3** (ingest/upload) · **T11–T12 = Phase 4** (render) · **T14 = Phase 5** (e2e + gates + PR).

---

### Task 0: Pre-flight baseline

**Files:** none modified (verification only; no commit).

- [ ] **Step 1: Capture the orphan-failure baseline.** From `src-tauri/`:

```bash
cd src-tauri && set -o pipefail
cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -40
echo "NEXTEST_EXIT=$?"
```

Expected: green except the known orphan flake classes (transport/port: `zenoh_iroh_*`, `rename_content_integration` port-4242, iroh-bind timing). Record the exact set of pre-existing failures. **Any NEW card/serve/verify failure introduced by T1–T13 is blocking** (test drift is our fault).

- [ ] **Step 2: Frontend baseline.** From repo root:

```bash
npx tsc --noEmit; echo "TSC_EXIT=$?"
npx vitest run 2>&1 | tail -20; echo "VITEST_DONE=$?"
```

Expected: tsc clean; vitest green.

- [ ] **Step 3: Confirm the load-bearing APIs exist** (read-only; do not edit):

```bash
# CID API surface
grep -n "pub fn verify_hash\|pub fn flags\|pub fn from_bytes\|pub fn to_bytes" \
  ~/.cargo/git/checkouts/harmony-*/04449d6/crates/harmony-content/src/cid.rs
# 16 shard patterns helper
grep -rn "content_queryable_key_exprs\|all_shard_patterns" \
  ~/.cargo/git/checkouts/harmony-*/04449d6/crates/harmony-content/src/zenoh_bridge.rs
# Option bstr helpers
grep -n "serialize_optional_bytes_as_bstr\|deserialize_optional_bytes_from_bstr" src/owner_state_types.rs
# streaming_ingest generic signature
grep -n "pub async fn streaming_ingest" src/lib.rs
```

Expected: all present. If any is missing/renamed, report `DONE_WITH_CONCERNS` with the actual shape — do NOT guess.

- [ ] **Step 4: No commit.** Report the baseline failure set to the controller.

---

### Task 1: `CasOp::GetLocal` — read-only local cache lookup

**Files:**
- Modify: `src-tauri/src/content_store.rs:64-91` (the `CasOp` enum)
- Modify: `src-tauri/src/event_loop.rs:1982-2015` (the `cas_op_rx` match arm)
- Test: `src-tauri/src/event_loop.rs` (inline `#[cfg(test)]`)

- [ ] **Step 1: Write the failing test.** Add to the existing test module in `event_loop.rs` (or create one near the CAS tests). This test drives the GetLocal arm via a direct in-process channel + a minimal runtime that has one CID cached. Because constructing a full `NodeRuntime` in a unit test is heavy, assert the *enum shape + handler contract* with a focused integration-style test that seeds the StorageTier through `CasOp::PutLocal` then reads it back through `CasOp::GetLocal`. Place it in `cas_serve_two_node_integration.rs` if a runtime harness is easier there; otherwise inline. Minimal contract test (enum + variant compiles and round-trips a reply type):

```rust
#[tokio::test]
async fn cas_getlocal_returns_none_for_absent_then_bytes_after_putlocal() {
    // Build the same event-loop CAS channel the production run() uses, drive
    // one PutLocal then one GetLocal, asserting GetLocal reflects the cache.
    // (Harness: reuse the existing event-loop test rig that constructs a
    // NodeRuntime + cas_op channel. If none exists, this test lands in
    // cas_serve_two_node_integration.rs alongside Task 2's harness, which
    // already stands up a runtime-backed lookup.)
    // ASSERTION SHAPE:
    //   let (tx, rx) = mpsc::channel(8); spawn the run-loop CAS arm over `rx`.
    //   let cid = ContentId::for_book(b"hello", ContentFlags::default()).unwrap();
    //   // absent:
    //   let (r1tx, r1rx) = oneshot::channel();
    //   tx.send(CasOp::GetLocal { cid, reply: r1tx }).await.unwrap();
    //   assert_eq!(r1rx.await.unwrap(), None);
    //   // after admit:
    //   let (p_tx, p_rx) = oneshot::channel();
    //   tx.send(CasOp::PutLocal { cid, blob: b"hello".to_vec(), reply: Some(p_tx) }).await.unwrap();
    //   p_rx.await.unwrap().unwrap();
    //   let (r2tx, r2rx) = oneshot::channel();
    //   tx.send(CasOp::GetLocal { cid, reply: r2tx }).await.unwrap();
    //   assert_eq!(r2rx.await.unwrap(), Some(b"hello".to_vec()));
}
```

> NOTE TO IMPLEMENTER: the cleanest harness is the same one Task 2 builds (a tokio task owning a `NodeRuntime` + `StorageTier`, selecting over the `cas_op_rx`). If that harness doesn't yet exist as a reusable helper, **defer this exact round-trip assertion into Task 2's integration file** (where the runtime-backed lookup is constructed for the proof test) and in THIS task only add the enum variant + handler arm + a compile-level test that the variant matches. Report which path you took.

- [ ] **Step 2: Run it to verify it fails** (variant doesn't exist yet):

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(cas_getlocal)' 2>&1 | tail -20; echo "EXIT=$?"
```

Expected: FAIL to compile (`no variant GetLocal`).

- [ ] **Step 3: Add the `CasOp::GetLocal` variant.** In `content_store.rs`, after the `GetOrFetch` variant (around `content_store.rs:90`):

```rust
    /// Read-only local-cache lookup: return the bytes held under `cid` in the
    /// StorageTier cache, or `None` on a cache miss. Unlike `GetOrFetch`, this
    /// NEVER triggers a network fetch — it is the lookup path for the
    /// content-serve queryable, which must not recursively fetch while
    /// answering a peer's GET (that would invert the serve relationship and
    /// could deadlock the event loop). The cache only holds bytes that passed
    /// `hash==cid` verification at admit time (StorageTier::verify_cid), so a
    /// `Some(bytes)` reply is already integrity-checked.
    GetLocal {
        cid: ContentId,
        reply: tokio::sync::oneshot::Sender<Option<Vec<u8>>>,
    },
```

- [ ] **Step 4: Handle it in the CAS arm.** In `event_loop.rs`, inside `match op {` (after the `CasOp::GetOrFetch { .. } => { ... }` arm, before the closing `}` of the match, around `event_loop.rs:2110`):

```rust
                    CasOp::GetLocal { cid, reply } => {
                        // Read-only: pull from the in-memory StorageTier cache
                        // without any network fetch. Mirrors the fast-path
                        // cache check in GetOrFetch (event_loop.rs:2018) but
                        // never spawns a Zenoh GET on a miss.
                        let bytes = runtime
                            .storage_tier()
                            .cache()
                            .get(&cid)
                            .map(|b| b.to_vec());
                        let _ = reply.send(bytes);
                    }
```

- [ ] **Step 5: Run the test to verify it passes** (or, if deferred to Task 2, verify the variant compiles via clippy):

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(cas_getlocal)' 2>&1 | tail -20; echo "EXIT=$?"
# If deferred to Task 2:
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -15; echo "CLIPPY_EXIT=$?"
```

Expected: PASS (or clippy clean).

- [ ] **Step 6: Commit.**

```bash
git add src-tauri/src/content_store.rs src-tauri/src/event_loop.rs
git commit -m "feat(zeb-343): CasOp::GetLocal read-only cache lookup for serve path"
```

---

### Task 2: `spawn_content_serve_queryable` + two-node fetch-by-CID PROOF (HARD GATE)

**This is the prove-first gate. It must be green before any later task proceeds.** It answers "does Zenoh GET work peer-to-peer at all?" — the unknown that has blocked every prior CAS attempt.

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (new `pub fn spawn_content_serve_queryable`, near `spawn_channel_log_zenoh_adapter` ~`event_loop.rs:4064`)
- Create: `src-tauri/tests/cas_serve_two_node_integration.rs`

- [ ] **Step 1: Write the failing integration test (the PROOF).** Create `src-tauri/tests/cas_serve_two_node_integration.rs`:

```rust
//! ZEB-343 Phase 0: two-node fetch-by-CID proof. Node A declares the content
//! serve queryable backed by a stub store holding one blob; node B issues a
//! Zenoh GET on harmony/content/{prefix}/{cid_hex} and must receive the exact
//! bytes. This is the prove-first gate: it validates the Zenoh GET serve/fetch
//! round-trip end-to-end, the unknown that blocked every prior CAS attempt.
//!
//! Harness mirrors community_channel_messages_integration.rs:199-201 (two
//! in-process zenoh::open(Config::default()) sessions that discover each other
//! via gossip) under an outer 30s wall-clock timeout (standing convention).

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use harmony_app::event_loop::spawn_content_serve_queryable;
use harmony_content::cid::{ContentFlags, ContentId};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serves_public_cid_to_a_second_zenoh_node() {
    tokio::time::timeout(Duration::from_secs(30), serve_inner())
        .await
        .expect("cas-serve two-node proof must complete within 30s");
}

async fn serve_inner() {
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B"));

    // Node A's local store: one public blob.
    let blob = b"avatar-bytes-proof".to_vec();
    let cid = ContentId::for_book(&blob, ContentFlags::default()).expect("cid");
    let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
    store.insert(cid, blob.clone());
    let store = Arc::new(store);

    // Lookup closure backing the serve queryable (production wires this to
    // CasOp::GetLocal; the test wires a HashMap).
    let lookup = {
        let store = Arc::clone(&store);
        Arc::new(move |cid: ContentId| {
            let store = Arc::clone(&store);
            Box::pin(async move { store.get(&cid).cloned() })
                as std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send>>
        })
    };

    let closing = Arc::new(AtomicBool::new(false));
    let _serve = spawn_content_serve_queryable(Arc::clone(&session_a), lookup, Arc::clone(&closing));

    // Build the GET key the production fetch path uses.
    let cid_hex = hex::encode(cid.to_bytes());
    let prefix = &cid_hex[1..2];
    let key = format!("harmony/content/{prefix}/{cid_hex}");

    // Retry GET until gossip discovery lands a reply (or the 30s outer budget
    // trips). Each attempt drains all replies; a non-empty success short-circuits.
    let mut got: Option<Vec<u8>> = None;
    for _ in 0..60 {
        let replies = session_b.get(&key).await.expect("get");
        while let Ok(reply) = replies.recv_async().await {
            if let Ok(sample) = reply.result() {
                got = Some(sample.payload().to_bytes().to_vec());
                break;
            }
        }
        if got.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    assert_eq!(got.as_deref(), Some(blob.as_slice()), "node B must receive A's served bytes");

    closing.store(true, std::sync::atomic::Ordering::SeqCst);
}
```

- [ ] **Step 2: Run it to verify it fails** (function doesn't exist):

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test cas_serve_two_node_integration 2>&1 | tail -25; echo "EXIT=$?"
```

Expected: FAIL to compile (`spawn_content_serve_queryable` not found).

- [ ] **Step 3: Implement `spawn_content_serve_queryable`.** In `event_loop.rs`, near `spawn_channel_log_zenoh_adapter` (~`event_loop.rs:4064`). Add the import `use harmony_content::cid::ContentId;` if not already in scope (it is used elsewhere in the file).

```rust
/// ZEB-343: the peer-to-peer CAS serve primitive. Declares a single Zenoh
/// queryable on `harmony/content/*/**` and answers content GETs for PUBLIC
/// (unencrypted) CIDs held in the local store.
///
/// `lookup` is the local-store accessor (production wires it to a
/// `CasOp::GetLocal` round-trip; tests wire a HashMap) — passed in to avoid an
/// engine↔adapter circular dep, exactly like channel-log's `read_for_query`
/// (event_loop.rs:4073).
///
/// Serve gate: a CID is servable iff `!cid.flags().encrypted` (spec §5.2). This
/// is intrinsic to the CID (its header's leading bit) and holds per-chunk, so
/// no public-membership registry is needed. Encrypted CIDs get no reply.
///
/// Returned bytes are inherently integrity-safe: the local cache only admits
/// bytes that passed `hash==cid` (StorageTier::verify_cid), so anything `lookup`
/// returns already verifies. We still re-check `cid.verify_hash` before replying
/// as defense-in-depth (cheap; never serve corrupt bytes).
#[allow(clippy::type_complexity)]
pub fn spawn_content_serve_queryable<F>(
    session: Arc<zenoh::Session>,
    lookup: Arc<F>,
    closing: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()>
where
    F: Fn(
            ContentId,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send>>
        + Send
        + Sync
        + ?Sized
        + 'static,
{
    // One queryable covers all 16 content shards: `*` matches the single shard
    // segment, `**` the cid segment. Non-content children of harmony/content/
    // (publish/transit/stats) have multi-char word segments and are rejected by
    // the strict shard(1 hex)+cid(64 hex) parse below — and are never GET'd
    // anyway.
    let key_pattern = "harmony/content/*/**".to_string();

    tokio::spawn(async move {
        if closing.load(Ordering::SeqCst) {
            return;
        }
        let qbl = match session.declare_queryable(&key_pattern).await {
            Ok(q) => q,
            Err(e) => {
                if !closing.load(Ordering::SeqCst) {
                    tracing::error!(error = %e, "failed to declare content-serve queryable");
                }
                return;
            }
        };
        loop {
            tokio::select! {
                biased;
                res = qbl.recv_async() => {
                    let Ok(query) = res else { break; };
                    let qkey = query.key_expr().to_string();
                    let Some(cid) = parse_content_serve_cid(&qkey) else {
                        // Malformed / non-content key — skip silently (no reply).
                        continue;
                    };
                    // Public-tier gate: never serve encrypted content.
                    if cid.flags().encrypted {
                        continue;
                    }
                    let Some(bytes) = (lookup)(cid).await else {
                        continue; // not held locally — let other responders answer
                    };
                    // Defense-in-depth: never serve bytes that don't match the CID.
                    if !cid.verify_hash(&bytes) {
                        tracing::warn!(%qkey, "content-serve: local bytes failed hash==cid; not serving");
                        continue;
                    }
                    if let Err(e) = query.reply(query.key_expr(), bytes).await {
                        tracing::warn!(%qkey, error = %e, "content-serve reply failed");
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                    if closing.load(Ordering::SeqCst) { break; }
                }
            }
        }
    })
}

/// Parse a `harmony/content/{shard}/{cid_hex}` serve key into a ContentId.
/// Requires EXACTLY 4 slash-segments, a single-hex shard char, and a 64-hex
/// cid. Returns None for publish/transit/stats keys or any malformed selector.
fn parse_content_serve_cid(key: &str) -> Option<ContentId> {
    let segs: Vec<&str> = key.split('/').collect();
    if segs.len() != 4 || segs[0] != "harmony" || segs[1] != "content" {
        return None;
    }
    let shard = segs[2];
    if shard.len() != 1 || !shard.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let cid_hex = segs[3];
    if cid_hex.len() != 64 || !cid_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let raw = hex::decode(cid_hex).ok()?;
    let arr: [u8; 32] = raw.try_into().ok()?;
    Some(ContentId::from_bytes(arr))
}
```

> Implementer note: confirm `harmony_app::event_loop` re-exports `spawn_content_serve_queryable` as `pub` so the integration test (which uses `harmony_app::event_loop::spawn_content_serve_queryable`) links. `spawn_channel_log_zenoh_adapter` is already `pub fn` in this module, so a sibling `pub fn` is reachable the same way. Add a `#[cfg(test)]` unit test for `parse_content_serve_cid` covering: a valid key → Some; `harmony/content/publish/{64hex}` → None; 63-hex cid → None; 5 segments → None.

- [ ] **Step 4: Run the proof test to verify it passes:**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test cas_serve_two_node_integration 2>&1 | tail -25; echo "EXIT=$?"
```

Expected: PASS within 30s. **If it FAILS to receive bytes (times out): STOP. This is the prove-first gate.** Diagnose whether (a) gossip discovery needs more time / a different `Config` (try a brief `tokio::time::sleep` after open before the GET loop, or raise the retry count), or (b) the queryable/key is wrong. If after genuine effort the two sessions cannot exchange, this is an architectural blocker — pushover Jake (`~/work/pushover-notify.sh "ZEB-343 Phase-0 blocked" "two-node Zenoh GET proof not passing — <detail>"`) and surface it; do NOT proceed to later tasks on a broken primitive.

- [ ] **Step 5: clippy + fmt, then commit.**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -15; echo "CLIPPY=$?"
git add src-tauri/src/event_loop.rs src-tauri/tests/cas_serve_two_node_integration.rs
git commit -m "feat(zeb-343): content-serve queryable + two-node fetch-by-CID proof (Phase 0 gate)"
```

---

### Task 3: Wire the serve queryable into `event_loop::run` (production)

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (in `run()`, near the card subscriber-pool spawn ~`event_loop.rs:1271-1310`)

- [ ] **Step 1: Add the production wiring.** In `run()`, after the existing startup spawns (a good neighbor is right after the profile-card subscriber pool spawn around `event_loop.rs:1310`, where `session_arc`, `cas_op_tx`, and `closing` are all in scope). Insert:

```rust
        // ── ZEB-343: content-serve queryable ─────────────────────────
        // Answer peer content GETs from the local StorageTier cache. The
        // lookup closure routes through CasOp::GetLocal so the read happens
        // on the event-loop-owned runtime (read-only; no recursive fetch).
        {
            let cas_op_tx_serve = cas_op_tx.clone();
            let serve_lookup = std::sync::Arc::new(move |cid: ContentId| {
                let tx = cas_op_tx_serve.clone();
                Box::pin(async move {
                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                    if tx
                        .send(crate::content_store::CasOp::GetLocal { cid, reply: reply_tx })
                        .await
                        .is_err()
                    {
                        return None;
                    }
                    reply_rx.await.ok().flatten()
                })
                    as std::pin::Pin<
                        Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send>,
                    >
            });
            let _serve_handle = spawn_content_serve_queryable(
                std::sync::Arc::clone(&session_arc),
                serve_lookup,
                std::sync::Arc::clone(&closing),
            );
        }
```

> Implementer note: verify the exact in-scope identifiers at the chosen insertion line — `cas_op_tx` (the `run()` param at `event_loop.rs:349`), `session_arc` (`event_loop.rs:843`), and `closing` (the `Arc<AtomicBool>`). If `closing` is named differently at that point, match the local name. The `_serve_handle` is intentionally dropped (the task self-terminates on `closing`); if the codebase collects task handles for graceful shutdown elsewhere, push it there instead and report.

- [ ] **Step 2: Verify it compiles (no new test — covered e2e in T14):**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -15; echo "CLIPPY=$?"
```

Expected: clean.

- [ ] **Step 3: Quick scoped test sanity (serve test still green):**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test cas_serve_two_node_integration 2>&1 | tail -12; echo "EXIT=$?"
```

- [ ] **Step 4: Commit.**

```bash
git add src-tauri/src/event_loop.rs
git commit -m "feat(zeb-343): spawn content-serve queryable in event_loop::run (GetLocal-backed)"
```

---

### Task 4: Verify-on-fetch (`hash==CID`) on the fetch-return path

**Files:**
- Modify: `src-tauri/src/event_loop.rs:3105-3152` (`wrap_fetch_one_with_admission`)
- Test: inline `#[cfg(test)]` in `event_loop.rs` (the existing `descendants_tests` / wrap tests around `event_loop.rs:3316`)

- [ ] **Step 1: Write the failing test.** The existing wrap tests (`event_loop.rs:3316+`) already build a `wrap_fetch_one_with_admission(fetcher, cas_op_tx)` with a stub fetcher. Add a test where the fetcher returns bytes that do NOT match the requested CID, asserting the wrapped closure returns `Err` (tampered bytes are rejected, not returned):

```rust
    #[tokio::test]
    async fn wrap_rejects_bytes_that_fail_hash_eq_cid() {
        use harmony_content::cid::{ContentFlags, ContentId};
        // CID derived from "real" bytes; fetcher returns "tampered" bytes.
        let real = b"the real avatar bytes";
        let cid = ContentId::for_book(real, ContentFlags::default()).unwrap();
        let (cas_op_tx, _cas_op_rx) = tokio::sync::mpsc::channel(8);
        let fetcher = move |_cid: ContentId| async move { Ok(b"TAMPERED".to_vec()) };
        let wrapped = wrap_fetch_one_with_admission(fetcher, cas_op_tx);
        let result = wrapped(cid).await;
        assert!(result.is_err(), "tampered bytes must be rejected, not returned");
        assert!(
            result.unwrap_err().contains("hash"),
            "error should mention the hash==CID verification failure"
        );
    }
```

- [ ] **Step 2: Run it to verify it fails:**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(wrap_rejects_bytes_that_fail)' 2>&1 | tail -15; echo "EXIT=$?"
```

Expected: FAIL (currently returns `Ok(b"TAMPERED")`).

- [ ] **Step 3: Add the verify gate.** In `wrap_fetch_one_with_admission`, immediately after `let bytes = inner(cid).await?;` (`event_loop.rs:3109`):

```rust
            // ZEB-343 verify-on-fetch (spec §5.3): the StorageTier cache admit
            // already verifies hash==cid, but the bytes RETURNED here go
            // straight to the caller (e.g. the avatar resolver) regardless of
            // admit success. Reject a tampered reply before it is returned OR
            // admitted, so a malicious server can never get its bytes rendered.
            if !cid.verify_hash(&bytes) {
                return Err(format!(
                    "fetched bytes for {} failed hash==CID verification",
                    hex::encode(cid.to_bytes())
                ));
            }
```

- [ ] **Step 4: Run the test to verify it passes** + confirm the existing wrap tests still pass (they use real CIDs, so they verify fine):

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(wrap_)' 2>&1 | tail -20; echo "EXIT=$?"
```

Expected: PASS (new test + all pre-existing `wrap_*` tests).

- [ ] **Step 5: Commit.**

```bash
git add src-tauri/src/event_loop.rs
git commit -m "feat(zeb-343): verify hash==CID on fetch-return path (security keystone)"
```

---

### Task 5: Encrypted-CID serve gate — negative test

**Files:**
- Modify: `src-tauri/tests/cas_serve_two_node_integration.rs` (add a second test)

- [ ] **Step 1: Write the failing test** (it already passes if the gate works, but lock it in as a regression guard). Add to `cas_serve_two_node_integration.rs`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn does_not_serve_encrypted_cid() {
    tokio::time::timeout(Duration::from_secs(15), encrypted_inner())
        .await
        .expect("encrypted-gate test must complete within 15s");
}

async fn encrypted_inner() {
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B"));

    // An ENCRYPTED CID held locally — must NOT be served.
    let blob = b"secret".to_vec();
    let enc_flags = ContentFlags { encrypted: true, ..ContentFlags::default() };
    let cid = ContentId::for_book(&blob, enc_flags).expect("cid");
    let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
    store.insert(cid, blob.clone());
    let store = Arc::new(store);
    let lookup = {
        let store = Arc::clone(&store);
        Arc::new(move |cid: ContentId| {
            let store = Arc::clone(&store);
            Box::pin(async move { store.get(&cid).cloned() })
                as std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send>>
        })
    };
    let closing = Arc::new(AtomicBool::new(false));
    let _serve = spawn_content_serve_queryable(Arc::clone(&session_a), lookup, Arc::clone(&closing));

    let cid_hex = hex::encode(cid.to_bytes());
    let key = format!("harmony/content/{}/{}", &cid_hex[1..2], cid_hex);

    // Give discovery time, then GET: expect NO successful reply.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let replies = session_b.get(&key).await.expect("get");
    let mut served = false;
    // Drain with a short budget; an encrypted CID should yield no success reply.
    let drain = tokio::time::timeout(Duration::from_secs(3), async {
        while let Ok(reply) = replies.recv_async().await {
            if reply.result().is_ok() {
                served = true;
            }
        }
    })
    .await;
    let _ = drain; // timeout is the expected "no more replies" terminator
    assert!(!served, "encrypted CID must not be served");
    closing.store(true, std::sync::atomic::Ordering::SeqCst);
}
```

- [ ] **Step 2: Run it** (should pass given Task 2's gate):

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test cas_serve_two_node_integration 2>&1 | tail -20; echo "EXIT=$?"
```

Expected: BOTH tests PASS. If `does_not_serve_encrypted_cid` fails (something IS served), the encrypted gate in `spawn_content_serve_queryable` is wrong — fix `if cid.flags().encrypted { continue; }`.

- [ ] **Step 3: Commit.**

```bash
git add src-tauri/tests/cas_serve_two_node_integration.rs
git commit -m "test(zeb-343): encrypted CID is never served (public-tier gate guard)"
```

---

### Task 6: `avatar_cid` field on `ProfileCardBroadcast` + `sign_card`

**Files:**
- Modify: `src-tauri/src/profile_card_broadcast.rs:28-54` (struct), `:74-107` (`sign_card`)
- Test: in-module `#[cfg(test)]` in `profile_card_broadcast.rs`

- [ ] **Step 1: Write the failing tests.** Add to the in-module tests in `profile_card_broadcast.rs`:

```rust
    #[test]
    fn sign_verify_round_trips_with_avatar() {
        let owner = crate::community_membership::mint_test_owner(0x5A);
        let signer = signing_key_for(&owner); // existing test helper that yields the device #2 SigningKey
        let avatar = Some([0xABu8; 32]);
        let card = sign_card(
            &signer,
            owner.owner.0,
            "Ann".into(),
            "hi".into(),
            avatar,
            owner.cert.clone(),
            Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        )
        .expect("sign");
        assert_eq!(card.avatar_cid, avatar);
        assert_eq!(verify_card(&card).expect("verify"), owner.owner.0);
    }

    #[test]
    fn no_avatar_card_is_byte_identical_to_pre_field_encoding() {
        // With avatar_cid = None + skip_serializing_if, the encoding must stay a
        // 6-entry map (0xA6) with exactly the ZEB-341 key set — proving wire
        // backward-compat with cards already on the network.
        let owner = crate::community_membership::mint_test_owner(0x5B);
        let card = ProfileCardBroadcast {
            owner_id: owner.owner.0,
            display_name: "Bo".into(),
            status_text: "".into(),
            avatar_cid: None,
            enrollment: owner.cert,
            shared_at: Hlc { wall_ms: 9, logical: 1, device_id: "x".into() },
            signature: [0u8; 64],
        };
        let bytes = crate::owner_state_crypto::canonical_cbor_encode(&card).expect("encode");
        assert_eq!(bytes[0], 0xA6, "no-avatar card must stay a 6-entry map");
    }
```

> Implementer note: locate the existing test helper that produces the device-#2 `SigningKey` matching `owner.cert.device_pubkeys.classical.ed25519_verify` (the in-module `sign_card` tests already construct one — reuse it; `signing_key_for` above is a placeholder for whatever it is actually called). `mint_test_owner` is `harmony_app::community_membership::mint_test_owner(u8)`.

- [ ] **Step 2: Run to verify it fails** (field + param don't exist):

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(avatar)' -E 'test(byte_identical)' 2>&1 | tail -20; echo "EXIT=$?"
```

Expected: FAIL to compile.

- [ ] **Step 3: Add the field.** In `ProfileCardBroadcast`, insert AFTER `status_text` and BEFORE `enrollment` (declaration order is the wire order; grouping the visible-profile fields dn/st/av reads well):

```rust
    #[serde(
        rename = "av",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::owner_state_types::serialize_optional_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_optional_bytes_from_bstr"
    )]
    pub avatar_cid: Option<[u8; 32]>,
```

- [ ] **Step 4: Add the `sign_card` parameter.** Change `sign_card`'s signature to take `avatar_cid: Option<[u8; 32]>` (insert it after `status_text`) and set it in the struct literal:

```rust
pub fn sign_card(
    signer: &SigningKey,
    owner_id: [u8; 16],
    display_name: String,
    status_text: String,
    avatar_cid: Option<[u8; 32]>,
    enrollment: EnrollmentCert,
    shared_at: Hlc,
) -> Result<ProfileCardBroadcast, CardError> {
    // ... existing fail-fast checks unchanged ...
    let mut card = ProfileCardBroadcast {
        owner_id,
        display_name,
        status_text,
        avatar_cid,
        enrollment,
        shared_at,
        signature: [0u8; 64],
    };
    let bytes = canonical_cbor_encode(&card)?;
    card.signature = signer.sign(&bytes).to_bytes();
    Ok(card)
}
```

`verify_card` needs NO change (it clones + zeroes only `signature`, so `avatar_cid` is inside the verified bytes automatically).

- [ ] **Step 5: Fix the in-module call sites that break.** Every existing in-module test that calls `sign_card(...)` or builds a `ProfileCardBroadcast { .. }` literal now needs the new arg/field. Update them: pass `None` for `avatar_cid` in the existing cases (preserving their assertions). Also `publish_card_once` (`profile_card_broadcast.rs:264-286`) calls `sign_card` — add an `avatar_cid` param to `publish_card_once` and thread it (Task 7 wires the caller).

- [ ] **Step 6: Run to verify green:**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(profile_card)' -E 'test(avatar)' 2>&1 | tail -25; echo "EXIT=$?"
```

Expected: PASS.

- [ ] **Step 7: Commit.**

```bash
git add src-tauri/src/profile_card_broadcast.rs
git commit -m "feat(zeb-343): avatar_cid field on ProfileCardBroadcast + sign_card param"
```

---

### Task 7: Thread `avatar_cid` through cache/DTO/event + wire-format fixtures

**Files:**
- Modify: `src-tauri/src/profile_card_broadcast.rs` (`CachedCard` `:182`, `insert_verified` `:227`, `DiscoveredCardInfo` `:168`, `get_cached` `:249`, `publish_card_once` `:264`)
- Modify: `src-tauri/src/event_loop.rs:1392-1394` (the `member-card-received` push event)
- Modify: `src-tauri/tests/wire_format_profile_card_fixtures.rs` (update existing literals)
- Create: `src-tauri/tests/wire_format_profile_card_avatar_fixtures.rs` (avatar present case)

- [ ] **Step 1: Write the failing fixture test.** Create `src-tauri/tests/wire_format_profile_card_avatar_fixtures.rs`:

```rust
//! ZEB-343: pin the canonical CBOR wire format of ProfileCardBroadcast WITH an
//! avatar_cid set (7-entry map incl. "av"). The no-avatar case stays in
//! wire_format_profile_card_fixtures.rs (6-entry map, byte-identical to ZEB-341).
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::Hlc;
use harmony_app::profile_card_broadcast::ProfileCardBroadcast;

#[test]
fn profile_card_with_avatar_pins_seven_keys_incl_av() {
    let owner = harmony_app::community_membership::mint_test_owner(0x7E);
    let card = ProfileCardBroadcast {
        owner_id: owner.owner.0,
        display_name: "Ann".into(),
        status_text: "hi".into(),
        avatar_cid: Some([0x33u8; 32]),
        enrollment: owner.cert,
        shared_at: Hlc { wall_ms: 1234, logical: 0, device_id: "d".into() },
        signature: [0u8; 64],
    };
    let bytes = canonical_cbor_encode(&card).expect("encode");
    assert_eq!(bytes[0], 0xA7, "expected 7-entry CBOR map header with avatar set");
    let value: ciborium::value::Value = ciborium::de::from_reader(&bytes[..]).expect("decode");
    let map = match value {
        ciborium::value::Value::Map(m) => m,
        other => panic!("expected CBOR map, got {other:?}"),
    };
    let keys: std::collections::HashSet<String> = map
        .iter()
        .filter_map(|(k, _)| match k {
            ciborium::value::Value::Text(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        keys,
        std::collections::HashSet::from_iter(
            ["oi", "dn", "st", "av", "en", "sa", "sg"].map(str::to_string)
        )
    );
    // Round-trips with the avatar intact.
    let back: ProfileCardBroadcast = ciborium::de::from_reader(&bytes[..]).expect("decode struct");
    assert_eq!(back.avatar_cid, Some([0x33u8; 32]));
}
```

- [ ] **Step 2: Update the existing no-avatar fixtures.** In `src-tauri/tests/wire_format_profile_card_fixtures.rs`, both `ProfileCardBroadcast { .. }` literals must add `avatar_cid: None,` (after `status_text`). The existing assertions (`0xA6`, the 6-key set, round-trip) stay unchanged — proving backward-compat.

- [ ] **Step 3: Run to verify the avatar fixture fails to compile / the no-avatar updated ones compile:**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test wire_format_profile_card_avatar_fixtures --test wire_format_profile_card_fixtures 2>&1 | tail -20; echo "EXIT=$?"
```

Expected: the new avatar test FAILs only if field missing (it's added in T6, so it should pass once compiled); the no-avatar tests pass with the added `None`.

- [ ] **Step 4: Thread `avatar_cid` through the cache + DTO.** In `profile_card_broadcast.rs`:

`CachedCard` (`:182`) — add field:
```rust
struct CachedCard {
    owner_id: [u8; 16],
    display_name: String,
    status_text: String,
    avatar_cid: Option<[u8; 32]>,
    shared_at: Hlc,
}
```

`insert_verified` (`:227`) — copy `card.avatar_cid` into the `CachedCard` it builds.

`DiscoveredCardInfo` (`:168`) — add the hex CID field:
```rust
    #[serde(rename = "avatarCid", skip_serializing_if = "Option::is_none")]
    pub avatar_cid: Option<String>,
```

`get_cached` (`:249`) — populate it:
```rust
        Some(DiscoveredCardInfo {
            owner_id_hex: hex::encode(c.owner_id),
            display_name: c.display_name.clone(),
            status_text: c.status_text.clone(),
            avatar_cid: c.avatar_cid.map(hex::encode),
        })
```

- [ ] **Step 5: Thread through the `member-card-received` push event.** In `event_loop.rs:1392-1394`, the event payload struct gains `avatar_cid` (hex). Find the emit site (it serializes `owner_id_hex/display_name/status_text`); add `avatar_cid: cached.avatar_cid.map(hex::encode)` (or equivalent from the verified card). Keep the JS payload key `avatarCid`.

- [ ] **Step 6: Run the fixture + card tests green:**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(profile_card)' -E 'test(wire_format_profile_card)' 2>&1 | tail -25; echo "EXIT=$?"
```

Expected: PASS.

- [ ] **Step 7: Commit.**

```bash
git add src-tauri/src/profile_card_broadcast.rs src-tauri/src/event_loop.rs \
        src-tauri/tests/wire_format_profile_card_fixtures.rs \
        src-tauri/tests/wire_format_profile_card_avatar_fixtures.rs
git commit -m "feat(zeb-343): thread avatar_cid through card cache, DTO, and received event"
```

---

### Task 8: Thread `avatar_cid` through the publish IPCs (lib.rs)

**Files:**
- Modify: `src-tauri/src/lib.rs` — `publish_owner_card` (`:5635`), `publish_profile` (`:5545`/`:5610`), `republish_owner_card` (`:5713`)

- [ ] **Step 1: Add `avatar_cid` to `publish_owner_card`.** Add a param `avatar_cid: Option<[u8; 32]>` (after `status_text`) and pass it into the `sign_card(...)` call at `lib.rs:5680` (insert as the new 5th arg, matching T6's signature order: `..., status_text, avatar_cid, enrollment_cert, hlc`).

- [ ] **Step 2: Thread from `publish_profile`.** At the `publish_owner_card(...)` call (`lib.rs:5610-5618`), decode the hex CID already on `ProfilePayload`:

```rust
    // ProfilePayload.avatar_cid is Option<String> (hex) — decode to [u8;32].
    let avatar_cid_bytes: Option<[u8; 32]> = profile
        .avatar_cid
        .as_deref()
        .and_then(|h| hex::decode(h).ok())
        .and_then(|b| <[u8; 32]>::try_from(b).ok());
```

and pass `avatar_cid_bytes` into `publish_owner_card(...)`.

- [ ] **Step 3: Thread from `republish_owner_card`.** Change its signature to add `avatar_cid: Option<String>` (hex), decode the same way, and pass into `publish_owner_card`:

```rust
#[tauri::command]
async fn republish_owner_card(
    display_name: String,
    status_text: String,
    avatar_cid: Option<String>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let avatar_cid_bytes: Option<[u8; 32]> = avatar_cid
        .as_deref()
        .and_then(|h| hex::decode(h).ok())
        .and_then(|b| <[u8; 32]>::try_from(b).ok());
    // ... existing extraction, then publish_owner_card(..., status_text, avatar_cid_bytes, ...)
}
```

- [ ] **Step 4: Compile + scoped test.** No new Rust unit test (frontend tests cover the IPC contract; e2e in T14). Verify:

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -15; echo "CLIPPY=$?"
```

Expected: clean. (If any other caller of `publish_owner_card` exists — grep it — pass `None`.)

- [ ] **Step 5: Commit.**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-343): thread avatar_cid through publish_profile/republish_owner_card IPCs"
```

---

### Task 9: `ingest_avatar_bytes` IPC (in-memory bytes → PublicDurable CID)

**Files:**
- Modify: `src-tauri/src/lib.rs` (new `#[tauri::command]` near `ingest_content` `:8212`; register in `invoke_handler` ~`:31653`)
- Test: in-module `#[cfg(test)]` in `lib.rs` (mirror `ingest_file_at_path_*` tests `:8429`)

- [ ] **Step 1: Write the failing test.** Mirror the existing ingest tests' harness (they build an `ingest_tx` + drainer). Assert that ingesting bytes yields a `PublicDurable` (unencrypted) CID whose hash matches the input:

```rust
    #[tokio::test]
    async fn ingest_avatar_bytes_yields_public_durable_cid() {
        use harmony_content::cid::{ContentClass, ContentId};
        // Stand up the same ingest channel + drainer the other ingest tests use.
        let (ingest_tx, mut ingest_rx) = tokio::sync::mpsc::channel::<event_loop::IngestRequest>(16);
        let drainer = tokio::spawn(async move {
            while let Some(req) = ingest_rx.recv().await {
                let _ = req.reply.send(Ok(()));
            }
        });
        let bytes = vec![0x89, 0x50, 0x4E, 0x47, 1, 2, 3, 4]; // PNG-magic-ish payload
        let cid_hex = ingest_avatar_bytes_inner(&ingest_tx, bytes.clone())
            .await
            .expect("ingest");
        let raw = hex::decode(&cid_hex).unwrap();
        let cid = ContentId::from_bytes(<[u8; 32]>::try_from(raw).unwrap());
        assert_eq!(cid.content_class(), ContentClass::PublicDurable);
        assert!(cid.verify_hash(&bytes), "cid must hash the ingested bytes");
        drop(ingest_tx);
        let _ = drainer.await;
    }
```

> Implementer note: factor the core into a testable `pub(crate) async fn ingest_avatar_bytes_inner(ingest_tx: &Sender<IngestRequest>, bytes: Vec<u8>) -> Result<String, String>` (no Tauri `State`), and have the `#[tauri::command]` wrapper extract `ingest_tx` from `NodeState` and call it — mirroring how `ingest_content` (`:8212`) is the command and `ingest_file_at_path` (`:8287`) is the testable core.

- [ ] **Step 2: Run to verify it fails:**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(ingest_avatar_bytes)' 2>&1 | tail -15; echo "EXIT=$?"
```

Expected: FAIL to compile.

- [ ] **Step 3: Implement.** Add near `ingest_content` (`lib.rs:8212`):

```rust
/// ZEB-343: ingest an in-memory byte buffer (a normalized avatar PNG from the
/// frontend) into CAS, returning the root CID hex. Uses default ContentFlags
/// (PublicDurable / unencrypted) so the resulting CID is publicly servable.
/// Skips the sidecar insert (send_ingest_with_name) so avatars never appear in
/// the file listing.
pub(crate) async fn ingest_avatar_bytes_inner(
    ingest_tx: &tokio::sync::mpsc::Sender<event_loop::IngestRequest>,
    bytes: Vec<u8>,
) -> Result<String, String> {
    use harmony_content::chunker::ChunkerConfig;
    // Bound the input (frontend normalizes to ~tens of KB; cap defends the node).
    const MAX_AVATAR_BYTES: usize = 512 * 1024;
    if bytes.is_empty() {
        return Err("empty avatar bytes".to_string());
    }
    if bytes.len() > MAX_AVATAR_BYTES {
        return Err(format!("avatar too large: {} > {MAX_AVATAR_BYTES}", bytes.len()));
    }
    let reader = tokio::io::BufReader::new(std::io::Cursor::new(bytes));
    let (root, _size) = streaming_ingest(reader, ingest_tx, ChunkerConfig::DEFAULT, None)
        .await
        .map_err(|e| e.to_string())?;
    Ok(hex::encode(root.to_bytes()))
}

#[tauri::command]
async fn ingest_avatar_bytes(
    bytes: Vec<u8>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<String, String> {
    let ingest_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.ingest_tx.clone().ok_or_else(|| "not connected".to_string())?
    };
    ingest_avatar_bytes_inner(&ingest_tx, bytes).await
}
```

- [ ] **Step 4: Register the command** in the `invoke_handler!` list (`lib.rs` ~`:31653`, alphabetically near `ingest_content`): add `ingest_avatar_bytes,`.

- [ ] **Step 5: Run the test green:**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(ingest_avatar_bytes)' 2>&1 | tail -15; echo "EXIT=$?"
```

Expected: PASS.

- [ ] **Step 6: Commit.**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-343): ingest_avatar_bytes IPC (in-memory bytes -> PublicDurable CID)"
```

---

### Task 10: Frontend avatar normalizer (`avatar-normalize.ts`)

**Files:**
- Create: `src/lib/avatar-normalize.ts`
- Create: `src/lib/__tests__/avatar-normalize.test.ts`

- [ ] **Step 1: Write the failing test.** Create `src/lib/__tests__/avatar-normalize.test.ts`. jsdom lacks full canvas; test the pure guards (type/size validation) and mock the canvas path:

```typescript
import { describe, it, expect } from 'vitest';
import { validateAvatarInput, AVATAR_MAX_INPUT_BYTES } from '../avatar-normalize';

describe('avatar-normalize input guards', () => {
  it('rejects a non-image file', () => {
    const f = new File([new Uint8Array([1, 2, 3])], 'x.txt', { type: 'text/plain' });
    expect(() => validateAvatarInput(f)).toThrow(/image/i);
  });

  it('rejects an oversize file', () => {
    const big = new Uint8Array(AVATAR_MAX_INPUT_BYTES + 1);
    const f = new File([big], 'big.png', { type: 'image/png' });
    expect(() => validateAvatarInput(f)).toThrow(/too large/i);
  });

  it('accepts a small png', () => {
    const f = new File([new Uint8Array([0x89, 0x50])], 'ok.png', { type: 'image/png' });
    expect(() => validateAvatarInput(f)).not.toThrow();
  });
});
```

- [ ] **Step 2: Run to verify it fails:**

```bash
npx vitest run src/lib/__tests__/avatar-normalize.test.ts 2>&1 | tail -15; echo "DONE"
```

Expected: FAIL (module missing).

- [ ] **Step 3: Implement `src/lib/avatar-normalize.ts`:**

```typescript
/** Max accepted input file size before downscale (10 MB). */
export const AVATAR_MAX_INPUT_BYTES = 10 * 1024 * 1024;
/** Output square edge in px. */
export const AVATAR_EDGE = 256;

/** Throw if `file` is not an acceptable avatar input. */
export function validateAvatarInput(file: File): void {
  if (!file.type.startsWith('image/')) {
    throw new Error(`not an image: ${file.type || 'unknown type'}`);
  }
  if (file.size > AVATAR_MAX_INPUT_BYTES) {
    throw new Error(`image too large: ${file.size} > ${AVATAR_MAX_INPUT_BYTES}`);
  }
}

/**
 * Normalize an image File to a 256x256 PNG byte array, center-cropped (cover).
 * Frontend-side so there is no Rust image dependency and served bytes are
 * hard-bounded. Returns the PNG bytes ready for `ingest_avatar_bytes`.
 */
export async function normalizeAvatar(file: File): Promise<Uint8Array> {
  validateAvatarInput(file);
  const bitmap = await createImageBitmap(file);
  try {
    const canvas = document.createElement('canvas');
    canvas.width = AVATAR_EDGE;
    canvas.height = AVATAR_EDGE;
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('2d canvas context unavailable');
    // Cover: scale to fill, center-crop the overflow.
    const scale = Math.max(AVATAR_EDGE / bitmap.width, AVATAR_EDGE / bitmap.height);
    const dw = bitmap.width * scale;
    const dh = bitmap.height * scale;
    ctx.drawImage(bitmap, (AVATAR_EDGE - dw) / 2, (AVATAR_EDGE - dh) / 2, dw, dh);
    const blob: Blob = await new Promise((resolve, reject) =>
      canvas.toBlob(
        (b) => (b ? resolve(b) : reject(new Error('toBlob produced null'))),
        'image/png',
      ),
    );
    return new Uint8Array(await blob.arrayBuffer());
  } finally {
    bitmap.close();
  }
}
```

- [ ] **Step 4: Run the test green:**

```bash
npx vitest run src/lib/__tests__/avatar-normalize.test.ts 2>&1 | tail -15; echo "DONE"
npx tsc --noEmit; echo "TSC=$?"
```

Expected: PASS + tsc clean.

- [ ] **Step 5: Commit.**

```bash
git add src/lib/avatar-normalize.ts src/lib/__tests__/avatar-normalize.test.ts
git commit -m "feat(zeb-343): frontend avatar normalizer (256x256 PNG, input guards)"
```

---

### Task 11: `MemberCardService` avatar resolution

**Files:**
- Modify: `src/lib/member-card-service.ts`
- Create/Modify: `src/lib/__tests__/member-card-service.avatar.test.ts`

- [ ] **Step 1: Write the failing test.** Create `src/lib/__tests__/member-card-service.avatar.test.ts`. A fake `AvatarResolver` returns a URL for a known CID; assert `resolve(owner).avatarUrl` becomes that URL after `applyCard` with an `avatarCid`:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { MemberCardService } from '../member-card-service';

function fakeResolver(map: Record<string, string>) {
  return {
    resolve: (cid: string) => map[cid],
    connectAdapter: vi.fn(),
    destroy: vi.fn(),
    onChange: undefined as undefined | (() => void),
  };
}

describe('MemberCardService avatar resolution', () => {
  it('resolves avatarCid to avatarUrl via the AvatarResolver', () => {
    const svc = new MemberCardService();
    const resolver = fakeResolver({ deadbeef: 'blob:fake-url' });
    svc.setAvatarResolver(resolver as any);
    svc.applyCard('AA'.repeat(16), {
      displayName: 'Ann',
      statusText: 'hi',
      avatarCid: 'deadbeef',
    } as any);
    const card = svc.resolve('aa'.repeat(16));
    expect(card?.avatarUrl).toBe('blob:fake-url');
  });

  it('leaves avatarUrl undefined when no avatarCid', () => {
    const svc = new MemberCardService();
    svc.setAvatarResolver(fakeResolver({}) as any);
    svc.applyCard('BB'.repeat(16), { displayName: 'Bo', statusText: '' } as any);
    expect(svc.resolve('bb'.repeat(16))?.avatarUrl).toBeUndefined();
  });
});
```

- [ ] **Step 2: Run to verify it fails:**

```bash
npx vitest run src/lib/__tests__/member-card-service.avatar.test.ts 2>&1 | tail -15; echo "DONE"
```

Expected: FAIL (`setAvatarResolver` missing; `avatarUrl` undefined).

- [ ] **Step 3: Extend `member-card-service.ts`:**

`ResolvedCard` + `DiscoveredCardInfo`:
```typescript
export interface ResolvedCard {
  displayName: string;
  statusText: string;
  avatarUrl?: string;
}

export interface DiscoveredCardInfo {
  ownerIdHex: string;
  displayName: string;
  statusText: string;
  avatarCid?: string;
}
```

Add a resolver field + setter + a helper, and use it in `applyCard` and `pollOnce`:
```typescript
  private avatarResolver?: {
    resolve: (cid: string) => string | undefined;
    onChange?: () => void;
  };

  /** Attach an AvatarResolver so cards with an avatarCid resolve to a blob URL. */
  setAvatarResolver(resolver: { resolve: (cid: string) => string | undefined; onChange?: () => void }): void {
    this.avatarResolver = resolver;
    // When the resolver fetches a late blob URL, re-resolve known cards + notify.
    resolver.onChange = () => {
      this.refreshAvatars();
    };
  }

  private resolveAvatarUrl(avatarCid?: string): string | undefined {
    if (!avatarCid || !this.avatarResolver) return undefined;
    return this.avatarResolver.resolve(avatarCid);
  }

  /** Re-resolve avatarUrls for cards whose avatarCid newly resolved. */
  private refreshAvatars(): void {
    let changed = false;
    for (const [owner, card] of this.cards) {
      const cid = this.cardAvatarCids.get(owner);
      if (!cid) continue;
      const url = this.resolveAvatarUrl(cid);
      if (url && card.avatarUrl !== url) {
        this.cards.set(owner, { ...card, avatarUrl: url });
        changed = true;
      }
    }
    if (changed) this.onUpdate?.();
  }
```

Track the per-owner avatarCid (so `refreshAvatars` can re-resolve) and write `avatarUrl` into the card in BOTH `applyCard` and `pollOnce`. Add a field `private cardAvatarCids = new Map<string, string>();`. In `applyCard(ownerIdHex, card)` accept the optional `avatarCid` on the incoming object, store it, and set `avatarUrl`:
```typescript
  applyCard(ownerIdHex: string, card: ResolvedCard & { avatarCid?: string }): void {
    const key = ownerIdHex.toLowerCase();
    if (key === this.selfKey) return;
    if (card.avatarCid) this.cardAvatarCids.set(key, card.avatarCid);
    const avatarUrl = this.resolveAvatarUrl(card.avatarCid);
    const next: ResolvedCard = { displayName: card.displayName, statusText: card.statusText, avatarUrl };
    const prev = this.cards.get(key);
    if (
      prev &&
      prev.displayName === next.displayName &&
      prev.statusText === next.statusText &&
      prev.avatarUrl === next.avatarUrl
    ) {
      return;
    }
    this.cards.set(key, next);
    this.onUpdate?.();
  }
```
Apply the analogous change in `pollOnce` where it reads `info.avatarCid` and builds the cached card (store the cid, resolve the url, include it in the change-detection compare).

- [ ] **Step 4: Run the test green + the existing member-card tests (regression):**

```bash
npx vitest run src/lib/__tests__/member-card-service.test.ts src/lib/__tests__/member-card-service.avatar.test.ts 2>&1 | tail -20; echo "DONE"
npx tsc --noEmit; echo "TSC=$?"
```

Expected: PASS (new + existing). Update the existing member-card test if a strict object-shape assertion now sees `avatarUrl: undefined` (additive, should be fine).

- [ ] **Step 5: Commit.**

```bash
git add src/lib/member-card-service.ts src/lib/__tests__/member-card-service.avatar.test.ts
git commit -m "feat(zeb-343): MemberCardService resolves avatarCid -> avatarUrl"
```

---

### Task 12: Render avatars in MemberRow / ChannelMessageFeed / ProfilePopover

**Files:**
- Modify: `src/lib/components/MemberRow.svelte`, `src/lib/components/ChannelMessageFeed.svelte`, `src/lib/components/ProfilePopover.svelte`
- Modify: the `OwnerCard`/`OpenCardPayload` type (in `ProfilePopover.svelte` and/or `src/lib/types.ts`) — add `avatarUrl?`
- Test: extend an existing component test or add a focused render assertion

- [ ] **Step 1: Write a failing render test.** Add to the existing `MemberRow`/popover test (or create `src/lib/components/__tests__/avatar-render.test.ts`). Assert that when `resolveCard` returns a card with `avatarUrl`, MemberRow renders an `<img>` with that src:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render } from '@testing-library/svelte';
import MemberRow from '../MemberRow.svelte';

describe('MemberRow avatar', () => {
  it('renders the resolved avatar image when avatarUrl is present', () => {
    const member = { address: 'aa'.repeat(20), displayName: 'Ann', power: 0, status: 'joined' };
    const resolveCard = vi.fn(() => ({ displayName: 'Ann', statusText: 'hi', avatarUrl: 'blob:pic' }));
    const { container } = render(MemberRow, { props: { member, resolveCard } as any });
    const img = container.querySelector('img');
    expect(img?.getAttribute('src')).toBe('blob:pic');
  });
});
```

- [ ] **Step 2: Run to verify it fails:**

```bash
npx vitest run src/lib/components/__tests__/avatar-render.test.ts 2>&1 | tail -15; echo "DONE"
```

Expected: FAIL (MemberRow renders an initial-letter `<div>`, no `<img>`).

- [ ] **Step 3: Update MemberRow.** Replace the initial-letter avatar (`MemberRow.svelte:145-148`) with `<Avatar>` (import it: `import Avatar from './Avatar.svelte';`):

```svelte
  <Avatar
    address={member.address}
    displayName={displayName}
    avatarUrl={resolveCard?.(member.address)?.avatarUrl}
    size={28}
  />
```

(Keep the `.avatar` sizing via `Avatar`'s `size` prop; remove the now-unused `.avatar` initial-letter `<div>` + its CSS if nothing else uses it.)

- [ ] **Step 4: Update ChannelMessageFeed.** At `ChannelMessageFeed.svelte:375-377`, add `avatarUrl`:

```svelte
          <div class="avatar-col">
            <Avatar address={msg.author} avatarUrl={resolveCard?.(msg.author)?.avatarUrl} size={32} />
          </div>
```

(Drop the ignored `{trustService}` from this `<Avatar>` — it is silently discarded today.)

- [ ] **Step 5: Update ProfilePopover owner-card variant.** Add `avatarUrl?: string` to the `OwnerCard` type (`ProfilePopover.svelte:16-56`) and pass it (`ProfilePopover.svelte:252`):

```svelte
    <Avatar address={card.ownerIdHex} displayName={card.displayName} avatarUrl={card.avatarUrl} size={64} />
```

Thread `avatarUrl` into the `OwnerCard` where it's constructed — `App.svelte`'s `openMemberCard` builds the popover `card` from `resolveCard(ownerIdHex)`; include `avatarUrl: resolveCard(ownerIdHex)?.avatarUrl`. (And `OpenCardPayload` in `MemberRow.handleNameClick` / `ChannelMessageFeed.handleAuthorClick` may carry `avatarUrl` too if the popover reads from the payload rather than re-resolving — match whichever path `openMemberCard` uses.)

- [ ] **Step 6: Run the render test + tsc green:**

```bash
npx vitest run src/lib/components/__tests__/avatar-render.test.ts 2>&1 | tail -15; echo "DONE"
npx tsc --noEmit; echo "TSC=$?"
```

Expected: PASS + tsc clean.

- [ ] **Step 7: Commit.**

```bash
git add src/lib/components/MemberRow.svelte src/lib/components/ChannelMessageFeed.svelte \
        src/lib/components/ProfilePopover.svelte src/lib/components/__tests__/avatar-render.test.ts
git commit -m "feat(zeb-343): render resolved avatars in MemberRow/ChannelMessageFeed/ProfilePopover"
```

---

### Task 13: Upload UI (ProfileEditor) + App.svelte wiring + self-seed

**Files:**
- Modify: `src/lib/components/ProfileEditor.svelte` (avatar picker → normalize → ingest → set avatarCid)
- Modify: `src/App.svelte` (`republishOwnerCard` avatarCid; share `AvatarResolver` with `memberCardService`; `seedSelf` avatarUrl)
- Test: extend ProfileEditor test or add focused upload-flow test (mock invoke)

- [ ] **Step 1: Write the failing test.** Add a ProfileEditor test that, given a mocked `invoke('ingest_avatar_bytes')` returning a CID, sets `avatarCid` on the saved profile. Mock `normalizeAvatar` to return fixed bytes:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import ProfileEditor from '../ProfileEditor.svelte';

vi.mock('../../avatar-normalize', () => ({
  normalizeAvatar: vi.fn(async () => new Uint8Array([1, 2, 3])),
  validateAvatarInput: vi.fn(),
  AVATAR_MAX_INPUT_BYTES: 10485760,
  AVATAR_EDGE: 256,
}));

describe('ProfileEditor avatar upload', () => {
  it('ingests normalized bytes and stores the returned CID on save', async () => {
    const invoke = vi.fn(async (cmd: string) =>
      cmd === 'ingest_avatar_bytes' ? 'cidhex123' : undefined,
    );
    const onSave = vi.fn();
    const { getByLabelText } = render(ProfileEditor, {
      props: { profile: { address: 'x', displayName: 'A', statusText: '' }, onSave, adapter: { invoke } } as any,
    });
    const file = new File([new Uint8Array([9])], 'a.png', { type: 'image/png' });
    const input = getByLabelText(/avatar/i) as HTMLInputElement;
    await fireEvent.change(input, { target: { files: [file] } });
    // ... assert invoke('ingest_avatar_bytes', { bytes: [1,2,3] }) called; saved profile.avatarCid === 'cidhex123'
    expect(invoke).toHaveBeenCalledWith('ingest_avatar_bytes', { bytes: [1, 2, 3] });
  });
});
```

> Implementer note: match `ProfileEditor`'s actual prop/callback names + how it currently obtains the Tauri adapter (it may import `invoke` dynamically like `App.svelte:142` rather than take an `adapter` prop). Adapt the test + component to the real shape. The load-bearing assertion is: pick → `normalizeAvatar(file)` → `invoke('ingest_avatar_bytes', { bytes: Array.from(u8) })` → set `avatarCid` on the profile model.

- [ ] **Step 2: Run to verify it fails:**

```bash
npx vitest run src/lib/components/__tests__/ProfileEditor.test.ts 2>&1 | tail -15; echo "DONE"
```

Expected: FAIL (no avatar input).

- [ ] **Step 3: Add the upload control to ProfileEditor.** A labeled file input + handler:

```svelte
<script lang="ts">
  import { normalizeAvatar } from '../avatar-normalize';
  // ... existing ...
  let avatarBusy = $state(false);
  let avatarError = $state<string | null>(null);

  async function handleAvatarPick(e: Event) {
    const input = e.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    if (!file) return;
    avatarBusy = true; avatarError = null;
    try {
      const bytes = await normalizeAvatar(file);
      const { invoke } = await import('@tauri-apps/api/core');
      const cidHex = (await invoke('ingest_avatar_bytes', { bytes: Array.from(bytes) })) as string;
      profile = { ...profile, avatarCid: cidHex };
      // Self-seed an immediate local preview (no network round-trip).
      profile = { ...profile, avatarUrl: URL.createObjectURL(new Blob([bytes], { type: 'image/png' })) };
    } catch (err) {
      avatarError = err instanceof Error ? err.message : String(err);
    } finally {
      avatarBusy = false;
      input.value = '';
    }
  }
</script>

<label for="avatar-input">Avatar</label>
<input id="avatar-input" type="file" accept="image/*" onchange={handleAvatarPick} disabled={avatarBusy} />
{#if avatarError}<p class="error">{avatarError}</p>{/if}
```

(Match the component's actual `profile` binding + save flow. The save path must include `avatarCid` so `publishProfileToNetwork` / `republishOwnerCard` carry it.)

- [ ] **Step 4: Wire App.svelte.**

(a) `republishOwnerCard` (`App.svelte:160-171`) passes the CID:
```typescript
      await invoke('republish_owner_card', {
        displayName: profile.displayName,
        statusText: profile.statusText ?? '',
        avatarCid: profile.avatarCid ?? null,
      });
```

(b) `publishProfileToNetwork` (`App.svelte:140-156`) payload includes `avatarCid: profile.avatarCid`.

(c) Share the `AvatarResolver` with `memberCardService` (near `App.svelte:618-637` where `avatarResolver` is built and `navService.setAvatarResolver` is called):
```typescript
  memberCardService.setAvatarResolver(avatarResolver);
```
(`avatarResolver.onChange` currently calls `navService.refreshAvatars()`; chain both — call the member-card refresh too. `MemberCardService.setAvatarResolver` reassigns `resolver.onChange`; instead, App should set a combined `onChange` that calls both services' refreshes, OR give `MemberCardService` its own resolver instance. SIMPLEST + safe: construct combined onChange in App:)
```typescript
  avatarResolver.onChange = () => {
    navService.refreshAvatars();
    // MemberCardService exposes a public refresh hook for this:
    memberCardService.onAvatarsRefreshed();
  };
```
> Implementer note: to avoid `setAvatarResolver` clobbering `avatarResolver.onChange`, give `MemberCardService` a `setAvatarResolver` that does NOT reassign `onChange` (App owns the combined `onChange`), and expose a public `onAvatarsRefreshed()` that runs the private `refreshAvatars()`. Adjust Task 11's `setAvatarResolver` accordingly (drop the `resolver.onChange = ...` line; add `onAvatarsRefreshed()`). Update Task 11's avatar test if needed.

(d) `seedSelf` (`App.svelte:213,420,938,1081`) — when seeding self, include `avatarUrl` from `myProfile.avatarUrl` so the user's own row shows their picture immediately. `seedSelf(ownerIdHex, { displayName, statusText, avatarUrl })`.

- [ ] **Step 5: Run frontend gates:**

```bash
npx vitest run 2>&1 | tail -25; echo "DONE"
npx tsc --noEmit; echo "TSC=$?"
```

Expected: PASS + clean.

- [ ] **Step 6: Commit.**

```bash
git add src/lib/components/ProfileEditor.svelte src/App.svelte \
        src/lib/member-card-service.ts src/lib/components/__tests__/ProfileEditor.test.ts
git commit -m "feat(zeb-343): avatar upload UI + App wiring (republish CID, shared resolver, self-seed)"
```

---

### Task 14: Cross-peer e2e + full gate sweep + push + PR

**Files:**
- Create: `src-tauri/tests/profile_card_avatar_cross_peer_integration.rs` (optional if T2+T7 cover it; otherwise a two-owner card→resolve test)
- No source changes beyond test + any gate fixes.

- [ ] **Step 1: Cross-peer e2e test (avatar through the card).** Add a two-node test where owner A publishes a `ProfileCardBroadcast` with `avatar_cid` set (and seeds the bytes in A's serve store) and owner B subscribes, verifies the card, reads `avatar_cid`, GETs the bytes via the serve queryable, and verifies `hash==cid`. This composes Task 2's serve harness with a signed card. If the existing card subscriber-pool is too heavy to stand up in a test, assert the composition at the seam: sign a card with `avatar_cid` → `verify_card` returns owner → serve queryable serves that CID to a second session → bytes verify. Keep it under a 30s outer timeout.

- [ ] **Step 2: FULL backend sweep (commit first — already committed per task).** From `src-tauri/`:

```bash
cd src-tauri && set -o pipefail
cargo fmt --all -- --check; echo "FMT=$?"
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -15; echo "CLIPPY=$?"
cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -40; echo "NEXTEST_done"
```

Expected: fmt 0, clippy 0, nextest green except the T0 orphan baseline. **Any NEW failure tied to ZEB-343 is blocking — fix forward.**

- [ ] **Step 3: Large-tests + MSRV gates.**

```bash
cd src-tauri && set -o pipefail
HARMONY_LARGE_TESTS=1 cargo nextest run --locked --features test-fixtures -E 'test(folder_ingest_walker)' 2>&1 | tail -20; echo "LARGE_done"
cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -15; echo "MSRV=$?"
```

Expected: green.

- [ ] **Step 4: FULL frontend gates** (repo root):

```bash
npx tsc --noEmit; echo "TSC=$?"
npx vitest run 2>&1 | tail -25; echo "VITEST_done"
```

Expected: tsc clean; vitest green.

- [ ] **Step 5: Commit the e2e test, then push the branch.**

```bash
git add src-tauri/tests/profile_card_avatar_cross_peer_integration.rs
git commit -m "test(zeb-343): cross-peer avatar card publish -> resolve -> verify e2e"
git push -u origin zeb-343-cas-serve-profile-avatars
```

- [ ] **Step 6: Open the PR.** Body links ZEB-343 via a closing keyword; ZEB-341 stays plain-text lineage (Linear's GH integration closes EVERY ZEB-NNN written as a closing keyword — only ZEB-343 should auto-close).

```bash
gh pr create --repo zeblithic/harmony-client \
  --title "ZEB-343: peer-to-peer CAS-serve primitive + profile avatars over CAS" \
  --body "$(cat <<'EOF'
## Summary

Builds the missing peer-to-peer CAS **serve** half and ships profile avatars as its first consumer — the ZEB-341 follow-on the `ProfileCardBroadcast` wire format was designed for.

**The serve primitive (Phase 0–1):**
- New content-serve Zenoh queryable on `harmony/content/*/**` (`spawn_content_serve_queryable`), mirroring the channel-log queryable: parse CID → gate on `!cid.flags().encrypted` → read local StorageTier cache via new read-only `CasOp::GetLocal` → reply inline.
- Public-tier gate is the **CID's own leading `encrypted` bit** (intrinsic, per-chunk, no registry).
- **Verify-on-fetch** `hash==CID` on the fetch-return path (`wrap_fetch_one_with_admission`) — the cache already verified on admit, but returned bytes were not; a tampered reply can no longer be rendered.
- **Two-node fetch-by-CID integration proof** (`cas_serve_two_node_integration.rs`) — the prove-first gate that validates Zenoh GET p2p end-to-end (the unknown that blocked every prior CAS attempt). Encrypted-CID-not-served negative included.

**Avatars (Phase 2–5):**
- `avatar_cid: Option<[u8;32]>` (serde `"av"`) on the device-#2-signed `ProfileCardBroadcast`; no-avatar cards stay **byte-identical** to ZEB-341 (wire fixtures pin both 6-key and 7-key cases).
- `ingest_avatar_bytes` IPC: in-memory PNG → default-flags (`PublicDurable`) CAS bytes → CID.
- Frontend: `avatar-normalize.ts` (256² PNG), `MemberCardService` avatar resolution, render via `Avatar.svelte` in MemberRow / ChannelMessageFeed / ProfilePopover, identicon fallback, self-seed.

## Test plan
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- [ ] `HARMONY_LARGE_TESTS=1` folder_ingest_walker
- [ ] MSRV `cargo check`
- [ ] `npx tsc --noEmit` + `npx vitest run`
- [ ] Two-node CAS-serve proof passes (`cas_serve_two_node_integration`)

Spec: `docs/specs/2026-05-31-cas-serve-primitive-and-profile-avatars-design.md`.
Follow-on to ZEB-341 (#171).

Closes ZEB-343

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 7: Enter the autonomous PR-monitoring loop.** Per `feedback_autonomous_pr_monitoring_loop` + `feedback_human_in_loop_window` + `feedback_no_askuserquestion_for_pr_loop_mode`:
  - Watch the 5 CI jobs (fmt+clippy / nextest / large-tests / MSRV / frontend) AND bot reviewers (CodeRabbit / Cursor Bugbot / CodeAnt / Qodo).
  - Address each round as ONE bundled batch + a SINGLE push. Resolve threads via GraphQL.
  - `ScheduleWakeup(<<autonomous-loop-dynamic>>, ~1200s)` to self-pace; never Bash-sleep-poll; never `AskUserQuestion` mid-loop.
  - **NEVER trigger Greptile.** **NEVER merge** (Jake's gate).
  - Converge until no actionable bot/CI feedback, then **pushover Jake**: `~/work/pushover-notify.sh "ZEB-343 ready to merge" "CAS-serve + avatars PR converged — all CI green, bots clear"`.
  - Post-merge (Jake merges): verify ZEB-343 Done; if a Linear cascade closes any unintended parent, reopen it.

---

## Self-Review (run before dispatching)

**1. Spec coverage:**
- §5.1 serve queryable → T2/T3. §5.2 encrypted-bit gate → T2 (gate) + T5 (negative). §5.3 verify-on-fetch → T4. §5.4 Zenoh transport → existing path, T2/T3. §6.1 card field → T6/T7. §6.2 upload (normalize + bytes-ingest + self-seed) → T9/T10/T13. §6.3 render → T11/T12. §7 e2e → T14. §9 phases → T0–T14. §10 testing → each task's test step. §11 scope (one PR) → single branch/PR. ✓ No gap.

**2. Placeholder scan:** Test helper names flagged for the implementer to match the real codebase (`signing_key_for`, ProfileEditor prop shape) are explicitly called out as "match the actual code," not silent TODOs. Every code step shows real code. ✓

**3. Type consistency:** `sign_card` arg order (`..., status_text, avatar_cid, enrollment, shared_at`) is identical in T6 (def), T6 callers, T7 `publish_card_once`, T8 `publish_owner_card`. `avatar_cid: Option<[u8;32]>` (Rust) ↔ `Option<String>` hex at the IPC/DTO boundary ↔ `avatarCid?: string` (TS) ↔ `avatarUrl?: string` (resolved) is consistent across T6–T13. `CasOp::GetLocal { cid, reply: oneshot::Sender<Option<Vec<u8>>> }` identical in T1 (def + handler) and T2/T3 (caller). `spawn_content_serve_queryable<F>` signature identical in T2 (def + test) and T3 (prod caller). ✓

**4. Risk gates flagged:** T2 (prove-first; STOP + pushover on failure), T6 (struct/sign_card touches every in-module call site), T8 (IPC signature change — grep all `publish_owner_card` callers), T13 (the `setAvatarResolver`/`onChange` ownership subtlety). ✓
