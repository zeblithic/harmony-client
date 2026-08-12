# ZEB-922 Serve-Allowlist Lease Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give `CommunityServeAllowlist` lease semantics — renewed by authoritative re-affirmation and serve demand, collapsed by a TTL sweep — and fix the latent post-restart reused-segment serve stall.

**Architecture:** Swap the allowlist's `HashSet<ContentId>` for `HashMap<ContentId, u64 /*last_affirmed_ms*/>`; add a default-body `ContentStore::affirm_serveable` hook that `encode_root_packet` calls for every manifest segment (publish + every peer root GET); touch leases on successful serves in the content queryable; sweep hourly from a standalone `lib.rs` task mirroring the relay-hold GC.

**Tech Stack:** Rust, tokio, existing `std::sync::RwLock` allowlist idiom (no guard across await).

**Spec:** `docs/superpowers/specs/2026-08-12-zeb922-serve-allowlist-lease-design.md`

## Global Constraints

- All cargo commands from `src-tauri/`, always `--locked --features test-fixtures`.
- Clippy gate: `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`.
- `contains` stays read-pure and fail-closed on lock poison; `allow` keeps its existing signature; no `RwLock` guard ever held across an `.await`.
- Sweep boundary is strict: an entry whose `last_affirmed + ttl == now` SURVIVES (mirrors `RelayHoldDoc::gc`).
- Wall clock everywhere stamps are minted (`crate::wall_clock_ms()`); every new method takes `now_ms` so tests never sleep.
- Commit trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D`.

---

### Task 1: Lease core in `CommunityServeAllowlist`

**Files:**
- Modify: `src-tauri/src/content_store.rs:31-53` (type + methods), imports at top (`HashSet` → `HashMap`), constants near the type.
- Test: same file, `mod tests` (from `:491`).

**Interfaces:**
- Produces: `SERVE_ALLOWLIST_TTL_MS: u64` (30 days), `SERVE_ALLOWLIST_SWEEP_INTERVAL_MS: u64` (1 h), and methods `allow(&self, ContentId)` (unchanged signature), `allow_at(&self, ContentId, u64)`, `contains(&self, &ContentId) -> bool` (unchanged), `touch(&self, &ContentId)`, `touch_at(&self, &ContentId, u64) -> bool`, `sweep_expired(&self, u64, u64) -> usize`, `last_affirmed_ms(&self, &ContentId) -> Option<u64>`, `len(&self) -> usize`, `is_empty(&self) -> bool`.

- [ ] **Step 1: Write the failing tests** (append to the existing `mod tests`; reuse the module's existing CID-construction helpers):

```rust
#[test]
fn lease_allow_at_inserts_and_refreshes() {
    let a = CommunityServeAllowlist::new();
    let cid = test_cid(b"lease-a"); // reuse/mirror the module's existing helper
    assert_eq!(a.last_affirmed_ms(&cid), None);
    a.allow_at(cid, 100);
    assert_eq!(a.last_affirmed_ms(&cid), Some(100));
    a.allow_at(cid, 250);
    assert_eq!(a.last_affirmed_ms(&cid), Some(250), "allow_at refreshes");
}

#[test]
fn lease_touch_at_refreshes_only_existing_entries() {
    let a = CommunityServeAllowlist::new();
    let present = test_cid(b"lease-b1");
    let absent = test_cid(b"lease-b2");
    a.allow_at(present, 100);
    assert!(a.touch_at(&present, 500));
    assert_eq!(a.last_affirmed_ms(&present), Some(500));
    assert!(!a.touch_at(&absent, 500), "touch never inserts");
    assert_eq!(a.last_affirmed_ms(&absent), None);
    assert!(!a.contains(&absent));
}

#[test]
fn lease_sweep_expired_boundary_is_strict() {
    let a = CommunityServeAllowlist::new();
    let at_boundary = test_cid(b"lease-c1");
    let expired = test_cid(b"lease-c2");
    let fresh = test_cid(b"lease-c3");
    a.allow_at(at_boundary, 1_000);
    a.allow_at(expired, 999);
    a.allow_at(fresh, 5_000);
    // now = stamp + ttl exactly → survives; one ms older → collapses.
    let removed = a.sweep_expired(11_000, 10_000);
    assert_eq!(removed, 1);
    assert!(a.contains(&at_boundary), "stamp+ttl == now must survive");
    assert!(!a.contains(&expired));
    assert!(a.contains(&fresh));
}

#[test]
fn lease_refreshed_entry_survives_the_sweep_that_kills_its_cohort() {
    let a = CommunityServeAllowlist::new();
    let stale = test_cid(b"lease-d1");
    let renewed = test_cid(b"lease-d2");
    a.allow_at(stale, 100);
    a.allow_at(renewed, 100);
    assert!(a.touch_at(&renewed, 9_000));
    let removed = a.sweep_expired(15_000, 10_000);
    assert_eq!(removed, 1);
    assert!(!a.contains(&stale));
    assert!(a.contains(&renewed));
}
```

If the tests module has no reusable CID helper, add one:

```rust
fn test_cid(seed: &[u8]) -> ContentId {
    ContentId::for_book(
        seed,
        harmony_content::cid::ContentFlags { encrypted: true, ..Default::default() },
    )
    .expect("test cid")
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(lease_)' --lib`
Expected: compile errors (`allow_at`/`touch_at`/`sweep_expired`/`last_affirmed_ms` not found).

- [ ] **Step 3: Implement** — replace the struct + impl at `content_store.rs:30-53` (keep and adapt the existing doc comments: the RwLock rationale stays; “HashSet” wording becomes the lease map; `allow`'s and `contains`'s poison-semantics comments stay accurate):

```rust
/// Lease TTL for allowlist entries (30 days, matching the
/// `RELAY_HOLD_TTL_MS` precedent): serving intent nothing has re-affirmed
/// or served within this window collapses by default (ZEB-922 / Freenet
/// R5 lease discipline). Renewal is push-based — producer re-publish,
/// `affirm_serveable`, and successful serves — so expiry of a
/// still-referenced community segment is self-healing: the next peer root
/// GET re-affirms every current segment before the peer fetches them.
pub const SERVE_ALLOWLIST_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1_000;

/// Sweep cadence for the lease sweep task in `lib.rs`. Precision is
/// irrelevant against a 30-day TTL; hourly keeps the task negligible.
pub const SERVE_ALLOWLIST_SWEEP_INTERVAL_MS: u64 = 60 * 60 * 1_000;

#[derive(Clone, Default)]
pub struct CommunityServeAllowlist(Arc<RwLock<HashMap<ContentId, u64>>>);

impl CommunityServeAllowlist {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a community-root CID serveable, stamping the lease with the
    /// shared wall clock. Idempotent; re-calling refreshes the lease.
    pub fn allow(&self, cid: ContentId) {
        self.allow_at(cid, crate::wall_clock_ms());
    }

    /// Insert-or-refresh with a caller-supplied clock (test seam).
    pub fn allow_at(&self, cid: ContentId, now_ms: u64) {
        if let Ok(mut g) = self.0.write() {
            g.insert(cid, now_ms);
        }
    }

    pub fn contains(&self, cid: &ContentId) -> bool {
        self.0.read().map(|g| g.contains_key(cid)).unwrap_or(false)
    }

    /// Refresh the lease iff the CID is already allowlisted — a successful
    /// serve is demand, but demand alone must never CREATE serving intent,
    /// so this never inserts.
    pub fn touch(&self, cid: &ContentId) {
        self.touch_at(cid, crate::wall_clock_ms());
    }

    pub fn touch_at(&self, cid: &ContentId, now_ms: u64) -> bool {
        if let Ok(mut g) = self.0.write() {
            if let Some(stamp) = g.get_mut(cid) {
                *stamp = now_ms;
                return true;
            }
        }
        false
    }

    /// Remove entries whose lease expired: `last_affirmed + ttl < now`
    /// (strict — an entry at exactly `now - ttl` survives, mirroring
    /// `RelayHoldDoc::gc`). Returns the removed count.
    pub fn sweep_expired(&self, now_ms: u64, ttl_ms: u64) -> usize {
        if let Ok(mut g) = self.0.write() {
            let before = g.len();
            g.retain(|_, stamp| stamp.saturating_add(ttl_ms) >= now_ms);
            before - g.len()
        } else {
            0
        }
    }

    pub fn last_affirmed_ms(&self, cid: &ContentId) -> Option<u64> {
        self.0.read().ok().and_then(|g| g.get(cid).copied())
    }

    pub fn len(&self) -> usize {
        self.0.read().map(|g| g.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
```

Swap the `HashSet` import for `HashMap` at the top of the file (keep `HashSet` only if still used elsewhere in the file).

- [ ] **Step 4: Run the module tests**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(lease_) or test(allowlist)' --lib`
Expected: PASS, including the pre-existing `allowlist_allow_then_contains` / `allowlist_clone_shares_state`.

- [ ] **Step 5: Commit** — `feat(zeb-922): lease-stamped serve allowlist (allow_at/touch/sweep_expired)`

---

### Task 2: `affirm_serveable` trait hook

**Files:**
- Modify: `src-tauri/src/content_store.rs` — `ContentStore` trait (default-body methods live at `:61-113`), `RuntimeContentStore` impl (near its `put_serveable` at `:435-456`).
- Test: same file's `mod tests`.

**Interfaces:**
- Consumes: Task 1's `allow`.
- Produces: `fn affirm_serveable(&self, cid: ContentId)` on `ContentStore` (default no-op); `RuntimeContentStore` override = insert-or-refresh via its allowlist handle (no-op when built without one).

- [ ] **Step 1: Write the failing tests** (model on the existing `put_serveable_default_impl_routes_to_put` / `put_serveable_without_allowlist_is_just_put` fixtures in this module — they already build stub stores and a `RuntimeContentStore` with/without allowlist):

```rust
#[tokio::test]
async fn affirm_serveable_default_impl_is_a_noop() {
    // Reuse the stub ContentStore the put_serveable default-impl test uses;
    // calling affirm_serveable on it must compile and do nothing observable.
    // (Assert via the stub's recorded ops staying unchanged.)
}

#[tokio::test]
async fn runtime_affirm_serveable_inserts_and_refreshes_lease() {
    // RuntimeContentStore built WITH an allowlist (same harness as
    // put_serveable_registers_cid_in_allowlist):
    //   affirm_serveable(cid) on an absent cid → contains() true
    //   allow_at(cid, 1); affirm_serveable(cid) → last_affirmed_ms > 1
}

#[tokio::test]
async fn runtime_affirm_serveable_without_allowlist_is_noop() {
    // RuntimeContentStore built WITHOUT an allowlist: affirm_serveable
    // must not panic (same shape as put_serveable_without_allowlist_is_just_put).
}
```

Write these as real tests against the module's existing fixtures (the comments above describe the assertions; the fixtures at `content_store.rs:793-931` show the exact construction).

- [ ] **Step 2: Run to verify they fail** — `cargo nextest run --locked --features test-fixtures -E 'test(affirm_serveable)' --lib` → compile error (method not found).

- [ ] **Step 3: Implement.** Trait (beside `put_serveable`'s declaration):

```rust
/// Re-affirm an existing serve-intent lease (or create one) WITHOUT
/// re-writing bytes. Producers call this for content that is still
/// referenced by authoritative state but whose bytes were stored by an
/// earlier publish — e.g. reused community state segments, which a
/// republish deliberately does not re-`put` (O(delta)). Default is a
/// no-op; only `RuntimeContentStore` registers the CID in its shared
/// `CommunityServeAllowlist` (ZEB-922).
fn affirm_serveable(&self, cid: ContentId) {
    let _ = cid;
}
```

`RuntimeContentStore` impl (beside its `put_serveable` override; use the same allowlist field that override reads):

```rust
fn affirm_serveable(&self, cid: ContentId) {
    if let Some(allowlist) = &self.serve_allowlist {
        allowlist.allow(cid);
    }
}
```

(If the field name differs, mirror whatever `put_serveable` at `:435-456` uses.)

- [ ] **Step 4: Run** — same filter → PASS.
- [ ] **Step 5: Commit** — `feat(zeb-922): ContentStore::affirm_serveable hook (default no-op; runtime = lease affirm)`

---

### Task 3: Demand renewal + serve observability in the content queryable

**Files:**
- Modify: `src-tauri/src/event_loop.rs:11516-11518` (the reply site in `spawn_content_serve_queryable`).
- Test: `src-tauri/tests/community_misc/community_serve_allowlist_integration.rs`.

**Interfaces:**
- Consumes: Task 1's `touch` / `last_affirmed_ms` / `allow_at`.

- [ ] **Step 1: Write the failing test.** In the integration test, clone the allowlist before it moves into the queryable, seed the allowed CID at stamp 0, and after the successful step-2 fetch poll for the stamp to advance:

```rust
// (top of inner(), replacing the existing allow call)
let allowlist = CommunityServeAllowlist::new();
allowlist.allow_at(allowed_cid, 0); // stamp 0 so ANY renewal is visible
let allowlist_probe = allowlist.clone();
```

After the step-2 assertion:

```rust
// --- Step 2b (ZEB-922): a successful serve must refresh the lease ---
let mut renewed = false;
for _ in 0..40 {
    if allowlist_probe.last_affirmed_ms(&allowed_cid) > Some(0) {
        renewed = true;
        break;
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
}
assert!(renewed, "successful serve must touch the lease stamp");
```

After the step-3 assertion:

```rust
// Refused + never-allowlisted CIDs must not gain entries from requests.
assert_eq!(allowlist_probe.last_affirmed_ms(&denied_cid), None);
assert_eq!(allowlist_probe.last_affirmed_ms(&pub_cid), None);
```

- [ ] **Step 2: Run to verify it fails** — `cargo nextest run --locked --features test-fixtures --test community_misc_tests -E 'test(serves_allowlisted_encrypted_cid)'` → the Step-2b assertion fails (stamp stays 0).

- [ ] **Step 3: Implement.** In the queryable loop, replace the reply tail (`event_loop.rs:11516-11518`) with:

```rust
match query.reply(query.key_expr(), bytes).await {
    Ok(()) => {
        // ZEB-922: a successful serve is demonstrated demand — refresh
        // the lease (no-op for unencrypted CIDs, which are never in the
        // map). Also the first-ever success-path observability here.
        serve_allowlist.touch(&cid);
        tracing::debug!(%qkey, "content-serve: served");
    }
    Err(e) => {
        tracing::warn!(%qkey, error = %e, "content-serve reply failed");
    }
}
```

- [ ] **Step 4: Run** — same filter → PASS.
- [ ] **Step 5: Commit** — `feat(zeb-922): content-serve success touches the lease + debug observability`

---

### Task 4: Segment re-affirmation on every root encode (restart-stall fix)

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs:3663-3679` (after the manifest `put_serveable`).
- Test: `src-tauri/src/community_state_sync.rs` `mod tests`.

**Interfaces:**
- Consumes: Task 2's `affirm_serveable`.

- [ ] **Step 1: Write the failing test.** Fixture: a store that RECORDS. Model the store on `MissingUntilStore` (`community_state_sync.rs:9977-10008`) but backed by a working in-memory map, recording `put_serveable` and `affirm_serveable` CIDs separately:

```rust
/// ZEB-922: in-memory working CAS that records which CIDs were
/// put_serveable'd vs affirm_serveable'd.
struct RecordingAffirmStore {
    blobs: std::sync::Mutex<std::collections::HashMap<ContentId, Vec<u8>>>,
    puts_serveable: std::sync::Mutex<Vec<ContentId>>,
    affirmed: std::sync::Mutex<Vec<ContentId>>,
}

#[async_trait::async_trait]
impl ContentStore for RecordingAffirmStore {
    async fn put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError> {
        self.blobs.lock().unwrap().insert(cid, blob);
        Ok(())
    }
    async fn get(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError> {
        Ok(self.blobs.lock().unwrap().get(cid).cloned())
    }
    async fn put_serveable(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError> {
        self.puts_serveable.lock().unwrap().push(cid);
        self.put(cid, blob).await
    }
    fn affirm_serveable(&self, cid: ContentId) {
        self.affirmed.lock().unwrap().push(cid);
    }
}
```

Test flow (model the engine spawn on `flush_persists_crdt_even_when_publish_fails_zeb462` at `:8726-8756`, but with a WORKING store and live channels so the publish SUCCEEDS, and with `CatchUpChannels { root_serve_rx: Some(rx), fetch_request_tx: None, transport_epoch_rx: None }` instead of `::none()`):

```rust
#[tokio::test]
async fn zeb922_root_serve_reaffirms_every_manifest_segment() {
    // 1. Fixture with a RecordingAffirmStore-backed registry (add a
    //    build_test_fixture_with_store(...) variant that threads the store
    //    into the registry config the same way build_test_fixture builds
    //    its RuntimeContentStore at :8064-8067).
    // 2. Spawn the engine with root_serve_rx wired (keep pub/sub + adapter
    //    receivers ALIVE so the publish lands).
    // 3. Seed a membership event; flush_now() → publish seals ≥1 segment.
    // 4. Read the persisted segment index (the segments.cbor sidecar the
    //    publish wrote — community_state_persist::load_segment_index) to
    //    get the authoritative current segment CIDs.
    // 5. Clear the store's `affirmed` log (this models the restart: bytes
    //    persist, allowlist state gone).
    // 6. Send a RootServeRequest oneshot down root_serve_tx; await Ok packet.
    // 7. Assert: every segment CID from the index is in `affirmed` — the
    //    reused segments were re-affirmed by the SERVE, not a re-seal.
}
```

Write it fully (the comments give the flow; steps 1-2 mirror the cited fixtures line-for-line).

- [ ] **Step 2: Run to verify it fails** — `cargo nextest run --locked --features test-fixtures -E 'test(zeb922_root_serve_reaffirms)' --lib` → assertion failure at step 7 (`affirmed` empty).

- [ ] **Step 3: Implement.** In `encode_root_packet`, immediately after the manifest `put_serveable` succeeds (`:3667-3669`):

```rust
// ZEB-922: re-affirm every reused segment's serve lease. Newly-sealed
// segments were just put_serveable'd; REUSED segments were put by an
// earlier publish — possibly a previous process, whose allowlist died
// with it — and are deliberately not re-put (O(delta)). Without this, a
// restarted publisher serves the manifest but silently refuses every
// reused segment (the ZEB-706/398 stall class), and long-idle segment
// leases would decay while still referenced. This runs on every publish
// AND every peer root GET, so current segments are re-affirmed exactly
// before a requester fetches them.
for seg_ref in &manifest.segments {
    ctx.content_store.affirm_serveable(seg_ref.segment_cid);
}
```

- [ ] **Step 4: Run** — same filter, then the wider engine suite: `cargo nextest run --locked --features test-fixtures -E 'test(community_state_sync) or test(zeb922)' --lib` → PASS.
- [ ] **Step 5: Commit** — `fix(zeb-922): root encode re-affirms reused segment leases (post-restart joiner stall)`

---

### Task 5: Hourly lease sweep task in `lib.rs`

**Files:**
- Modify: `src-tauri/src/lib.rs` — spawn after the relay-hold GC block (`:12770-12841`); abort-handle field beside `community_relay_gc_handle` (declared near `:1992`, aborted near `:14750`); handle-option binding beside `community_relay_gc_handle_opt`.

**Interfaces:**
- Consumes: Task 1's `sweep_expired` / `len` + both constants.

- [ ] **Step 1: Implement** (wiring-only task; the sweep logic is Task-1-tested and the task body is deliberately too thin to unit-test, matching the relay-hold GC precedent). After the relay GC block:

```rust
// ZEB-922: serve-allowlist lease sweep — serving intent nothing has
// re-affirmed or served within SERVE_ALLOWLIST_TTL_MS collapses by
// default. Renewal is entirely push-based (producer re-publish,
// affirm_serveable on root encodes, touch on successful serves), so this
// task needs only the allowlist handle. Same shape as the relay-hold GC
// above; stop_inner aborts it via serve_allowlist_sweep_handle.
{
    let sweep_allowlist = serve_allowlist.clone();
    serve_allowlist_sweep_handle_opt = Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(
            crate::content_store::SERVE_ALLOWLIST_SWEEP_INTERVAL_MS,
        ));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Consume the immediate first tick — nothing can be expired at boot.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let removed = sweep_allowlist.sweep_expired(
                crate::wall_clock_ms(),
                crate::content_store::SERVE_ALLOWLIST_TTL_MS,
            );
            if removed > 0 {
                tracing::debug!(
                    removed,
                    remaining = sweep_allowlist.len(),
                    "ZEB-922: serve-allowlist lease sweep collapsed expired entries"
                );
            }
        }
    }));
}
```

Declare `serve_allowlist_sweep_handle_opt` beside `community_relay_gc_handle_opt`, store it into the node guard beside `community_relay_gc_handle`, and abort it on the stop path beside the relay GC abort — copy each of the three relay-GC touchpoints exactly (declaration, guard store, abort).

- [ ] **Step 2: Compile + targeted checks**

Run: `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` (from `src-tauri/`; confirm exit 0 via `${pipestatus[1]}` if piped)
Expected: clean.

- [ ] **Step 3: Commit** — `feat(zeb-922): hourly serve-allowlist lease sweep task (relay-GC shape)`

---

### Task 6: Full gates, docs, PR

- [ ] **Step 1:** `cargo fmt --all` then `cargo fmt --all -- --check`.
- [ ] **Step 2:** Full sweep: `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (full, NOT test-select — this is the pre-PR gate). Expected: all green, including `revoke_read_is_lazy_and_keeps_allowlist`, `content_serve_gate_tests`, `allow_serve_subtree_tests`, fleet ZEB-706/707 regressions.
- [ ] **Step 3:** `git status --short` must be clean after the final commit (local gates run the working tree).
- [ ] **Step 4:** Push branch, open PR titled `ZEB-922: serve-allowlist lease discipline (renew by reference + demand, collapse by default)` with the spec link, the §2 restart-stall finding called out, and the standard footer. Fire `@coderabbitai review` exactly once at open; never again.
